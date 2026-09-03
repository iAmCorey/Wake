use crate::adapters::AgentAdapter;
use crate::db::Store;
use crate::models::*;
use anyhow::Result;
use std::sync::Arc;

#[derive(Debug, Clone, Default)]
pub struct ScanProgress {
    pub scanning: bool,
    pub done: usize,
    pub total: usize,
    pub error: Option<String>,
}

/// 扫描回调:进度更新 / 会话数据有变化(UI 应刷新)
pub trait ScanEvents: Send + Sync {
    fn on_progress(&self, p: &ScanProgress);
    fn on_sessions_changed(&self);
    /// 文件监听后端报告事件丢失(FSEvents 的 MustScanSubDirs / inotify 队列
    /// 溢出):丢失期间的改动没人知道,需要一轮增量扫描兜底。默认忽略——
    /// scan CLI 与测试不跑 watcher;GUI 接到后经现有扫描状态机排队
    fn on_rescan_needed(&self) {}
}

pub struct NullEvents;
impl ScanEvents for NullEvents {
    fn on_progress(&self, _: &ScanProgress) {}
    fn on_sessions_changed(&self) {}
}

/// 扫描收尾守卫:Drop 时必定发出一次 `scanning = false` 的终态进度事件。
///
/// UI 的模态刷新弹窗把三条关闭路径全关了(close_button / overlay_closable /
/// keyboard),只认这个事件收场——扫描中途离开而没发终态,界面就被永久锁死。
/// 把它挂在 Drop 上,提前 `?` 与 panic unwind 就都兜得住,"必发"由类型保证,
/// 不再依赖后来者记得在每条返回路径上补一句。
struct ScanFinale<'a> {
    events: &'a dyn ScanEvents,
    progress: ScanProgress,
    /// 正常收尾时置 true;panic unwind 会让它保持 false,Drop 借此区分两种收场
    graceful: bool,
}

impl Drop for ScanFinale<'_> {
    fn drop(&mut self) {
        self.progress.scanning = false;
        if !self.graceful {
            self.progress.error = Some("Scan stopped unexpectedly".into());
        }
        self.events.on_progress(&self.progress);
    }
}

/// 进程内同一时刻只跑一条扫描。GUI 里 Dock 重开主窗会新建 Workbench 并立刻
/// 起一轮启动扫描,而上一个 Workbench 的扫描线程是脱管的(watcher 在 Drop 里
/// join 了,扫描没有)——不排队就是两条扫描并发改写同一个库。门放在 run_scan
/// 入口而不是某个调用方:任何起扫描的入口都自动被管住。锁是进程级而非 Store
/// 级,要挡的正是两个 Store 实例开同一个库文件;扫描 panic 会毒化锁,
/// into_inner 照常放行。排在后面的照常出终态事件,UI 只是多等一会儿
static SCAN_GATE: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// 全量/增量扫描。quickMeta 先行秒出列表,然后按 mtime 降序逐文件解析。
/// 阻塞执行——调用方放后台线程。进度与终态一律经 `events` 上报,终态由
/// `ScanFinale` 保证送达;返回的 `Result` 只用于调用方自己记日志。
pub fn run_scan(
    adapters: &[Box<dyn AgentAdapter>],
    store: &Arc<Store>,
    events: &dyn ScanEvents,
    full: bool,
) -> Result<()> {
    let _gate = SCAN_GATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut fin = ScanFinale {
        events,
        progress: ScanProgress {
            scanning: true,
            ..Default::default()
        },
        graceful: false,
    };
    events.on_progress(&fin.progress);

    let result = run_scan_inner(adapters, store, events, full, &mut fin.progress);
    fin.progress.error = result.as_ref().err().map(|e| e.to_string());
    fin.graceful = true;
    result
}

fn run_scan_inner(
    adapters: &[Box<dyn AgentAdapter>],
    store: &Arc<Store>,
    events: &dyn ScanEvents,
    full: bool,
    progress: &mut ScanProgress,
) -> Result<()> {
    for adapter in adapters {
        adapter.begin_scan();
    }
    let force_grok_backfill = store.needs_grok_parent_backfill();
    let known = store.known_files()?;
    let mut seen_paths: std::collections::HashSet<String> = std::collections::HashSet::new();
    struct WorkItem<'a> {
        adapter: &'a dyn AgentAdapter,
        r: SessionFileRef,
        quick: Option<SessionMeta>,
        /// 同 key 的落选副本(裁决顺位),胜者解析失败时依次回退
        fallbacks: Vec<(usize, SessionFileRef)>,
    }
    let mut queue: Vec<WorkItem> = Vec::new();

    // 归属表:(根, 实例下标)。跨家/跨实例的重叠根下(自定义 location 可以
    // 落进别家树,pi/kiro 的递归枚举会吞下界内一切 .jsonl),一个文件只属于
    // **最长根**的那个实例——别家枚举到也丢弃,与 watcher 的最长根分派同一
    // 语义。不做这道过滤,两家会对同一 file_path 轮流写库(UNIQUE 冲突 +
    // 错误归属,2026-08-24 Codex review)
    let mut roots: Vec<(String, usize)> = Vec::new();
    for (ix, a) in adapters.iter().enumerate() {
        for r in a.data_roots() {
            roots.push((r.to_string_lossy().to_string(), ix));
        }
    }
    let owner_of = |path: &str| -> Option<usize> {
        roots
            .iter()
            .filter(|(root, _)| crate::adapters::path_owns(root, path))
            .max_by_key(|(root, _)| root.len())
            .map(|(_, ix)| *ix)
    };
    // 墓碑查询与去重域共用的会话 key:按枚举实例的 host 构造(远程三段),
    // 下面四处同一口径,别各自 format
    let key_of =
        |ix: usize, r: &SessionFileRef| session_key(r.agent, adapters[ix].host(), &r.native_id);
    // 第一遍:全量枚举 + 归属过滤
    let mut per_adapter: Vec<Vec<SessionFileRef>> = Vec::with_capacity(adapters.len());
    for (ix, adapter) in adapters.iter().enumerate() {
        let refs: Vec<SessionFileRef> = adapter
            .list_session_files()?
            .into_iter()
            // 墓碑双轨:物理路径之外还按逻辑会话(key)屏蔽——多 location 下
            // 删除只 trash 了胜者文件,别的 location 里的副本不得复活它
            //(2026-08-24 Codex review P1)。key 按实例 host 构造(远程三段)
            // ——阶段 1 远程禁删故无远程墓碑,但格式先写对,放开时不欠债
            .filter(|r| {
                !store.is_tombstoned(&r.file_path) && !store.is_key_tombstoned(&key_of(ix, r))
            })
            // 无任何根认领的引用(越界枚举或合成测试)保守放行给枚举者;
            // 过滤只裁决"确有更深的根拥有它"的情形
            .filter(|r| owner_of(&r.file_path).is_none_or(|o| o == ix))
            .collect();
        per_adapter.push(refs);
    }
    // 同家同 ID 去重:同一会话在默认根与自定义根各有一份副本时,两个文件会
    // 每轮轮流改写同一行(key 相同,file_path 摇摆)。候选按 (mtime 新者,
    // 平局路径字典序小者) 排序,首位入队,其余留作**解析失败的回退顺位**
    // ——胜者副本截断/损坏时,不能让整个会话从索引消失(Codex review P2)。
    // 去重域即 session_key(agent, 实例 host, native_id)——直接以它为键,
    // "去重域与最终 key 的分段一致"就结构性成立:两台机器各自续跑过的
    // 同 UUID 会话是两条独立会话,跨 host 按 mtime 互吞会让一台的凭空消失
    let mut candidates: std::collections::HashMap<String, Vec<(usize, SessionFileRef)>> =
        std::collections::HashMap::new();
    for (ix, refs) in per_adapter.iter().enumerate() {
        for r in refs {
            candidates
                .entry(key_of(ix, r))
                .or_default()
                .push((ix, r.clone()));
        }
    }
    for v in candidates.values_mut() {
        v.sort_by(|(_, a), (_, b)| {
            b.mtime_ms
                .cmp(&a.mtime_ms)
                .then_with(|| a.file_path.cmp(&b.file_path))
        });
    }
    for (ix, refs) in per_adapter.iter_mut().enumerate() {
        refs.retain(|r| {
            candidates
                .get(&key_of(ix, r))
                .is_none_or(|v| v[0].1.file_path == r.file_path)
        });
    }

    // 路径易主清理(location 编辑改了 agent / 更深的他家根接管既有文件):
    // 旧 agent 的行不先删,新 key 写入会撞 file_path UNIQUE;且文件 mtime/size
    // 未变会跳过解析——旧行先删、该路径强制入队(2026-08-24 Codex review P1)
    let mut owner_changed: std::collections::HashSet<String> = std::collections::HashSet::new();
    for refs in &per_adapter {
        for r in refs {
            if let Some((_, _, key)) = known.get(&r.file_path) {
                if key.split(':').next() != Some(r.agent.as_str()) {
                    let _ = store.remove_session(key, false);
                    owner_changed.insert(r.file_path.clone());
                }
            }
        }
    }

    for (ix, (adapter, refs)) in adapters.iter().zip(per_adapter).enumerate() {
        let force_adapter = force_grok_backfill && adapter.agent() == AgentId::Grok;
        for r in &refs {
            seen_paths.insert(r.file_path.clone());
        }

        // 快路径:新/变化的先写 meta 让列表立即可见
        let quick_map = adapter.quick_meta(&refs);
        if let Some(map) = &quick_map {
            let fresh: Vec<(SessionMeta, i64)> = refs
                .iter()
                .filter_map(|r| {
                    let meta = map.get(&r.file_path)?;
                    let changed = owner_changed.contains(&r.file_path)
                        || match known.get(&r.file_path) {
                            None => true,
                            Some((mtime, size, _)) => *mtime != r.mtime_ms || *size != r.size,
                        };
                    // quick 的 key 可能已被合并策略改写(codex thread-id),
                    // 墓碑要按**这个最终 key** 再卡——write_meta_only 是第三条
                    // 写库路径,漏了它,已删会话会以空正文卡片复活
                    //(2026-08-24 Codex review P1)
                    if (changed || full || force_adapter) && !store.is_key_tombstoned(&meta.key) {
                        // fileMtime=0 → 后续全量解析仍会执行
                        Some((meta.clone(), 0))
                    } else {
                        None
                    }
                })
                .collect();
            if !fresh.is_empty() {
                store.write_meta_only(&fresh)?;
                events.on_sessions_changed();
            }
        }

        for r in refs {
            let changed = owner_changed.contains(&r.file_path)
                || match known.get(&r.file_path) {
                    None => true,
                    Some((mtime, size, _)) => *mtime != r.mtime_ms || *size != r.size,
                };
            if full || force_adapter || changed {
                let quick = quick_map
                    .as_ref()
                    .and_then(|m| m.get(&r.file_path).cloned());
                let fallbacks = candidates
                    .get(&key_of(ix, &r))
                    .filter(|v| v.len() > 1)
                    .map(|v| v[1..].to_vec())
                    .unwrap_or_default();
                queue.push(WorkItem {
                    adapter: adapter.as_ref(),
                    r,
                    quick,
                    fallbacks,
                });
            }
        }
    }

    // 删除检测:库里有但磁盘没了
    let mut pruned = false;
    for (path, (_, _, key)) in &known {
        if !seen_paths.contains(path) {
            let _ = store.remove_session(key, false);
            pruned = true;
        }
    }
    // 纯删除轮(location 移除后的补扫常是)也要让 UI 刷新:解析队列为空时
    // 下方循环一次不跑,不发这个事件,列表会一直挂着已删会话(Codex review P1)
    if pruned {
        events.on_sessions_changed();
    }

    // 最近的会话优先
    queue.sort_by(|a, b| b.r.mtime_ms.cmp(&a.r.mtime_ms));
    progress.total = queue.len();
    events.on_progress(progress);

    let mut last_notify = std::time::Instant::now();
    let mut grok_backfill_succeeded = true;
    for item in &queue {
        let forced_grok = force_grok_backfill && item.adapter.agent() == AgentId::Grok;
        let mut item_written = false;
        match item.adapter.parse_session(&item.r) {
            Ok(parsed) => {
                // quick/parsed 合并策略属各 adapter(默认 parsed 为准 quick 补缺,
                // Codex 覆写 title/key 优先级),scanner 不再内嵌任何 agent 特例
                let meta = match &item.quick {
                    Some(q) => item.adapter.merge_quick_meta(parsed.meta, q),
                    None => parsed.meta,
                };
                // 合并可能改写 key(codex 的 state thread-id):改名后的 key
                // 也要受墓碑约束,否则改名副本绕过上面的枚举过滤
                if store.is_key_tombstoned(&meta.key) {
                    progress.done += 1;
                    continue;
                }
                // 全量写入也走事务内副本裁决:扫描快照里的旧副本不得覆盖
                // watcher 并发间隙写入的更新副本(启动扫描与手动刷新期间
                // watcher 都活着,2026-08-24 Codex review P1)
                match store.write_session_guarded(&meta, item.r.mtime_ms, &parsed.units) {
                    Ok(written) => item_written = written,
                    Err(e) => eprintln!("[scanner] write failed {}: {e}", item.r.file_path),
                }
            }
            Err(e) => {
                eprintln!("[scanner] parse failed {}: {e}", item.r.file_path);
                // 胜者副本坏了不等于会话消失:按裁决顺位回退到下一份有效副本。
                // 回退实例自己的 quick 合并不能省——codex 的 state key(thread id)
                // 可与文件 native id 不同,绕过 merge 会丢手工标题、还可能留下
                // 双 key 两行(2026-08-24 Codex review)
                for (fb_ix, fb) in &item.fallbacks {
                    let fb_adapter = &adapters[*fb_ix];
                    match fb_adapter.parse_session(fb) {
                        Ok(parsed) => {
                            let quick = fb_adapter.quick_meta(std::slice::from_ref(fb));
                            let meta = match quick.as_ref().and_then(|m| m.get(&fb.file_path)) {
                                Some(q) => fb_adapter.merge_quick_meta(parsed.meta, q),
                                None => parsed.meta,
                            };
                            if store.is_key_tombstoned(&meta.key) {
                                item_written = true;
                                break;
                            }
                            match store.write_session_guarded(&meta, fb.mtime_ms, &parsed.units) {
                                Ok(written) => item_written = written,
                                Err(e) => eprintln!(
                                    "[scanner] fallback write failed {}: {e}",
                                    fb.file_path
                                ),
                            }
                            break;
                        }
                        Err(e2) => {
                            eprintln!("[scanner] fallback parse failed {}: {e2}", fb.file_path)
                        }
                    }
                }
            }
        }
        if forced_grok && !item_written {
            grok_backfill_succeeded = false;
        }
        progress.done += 1;
        if last_notify.elapsed().as_millis() > 800 || progress.done == progress.total {
            last_notify = std::time::Instant::now();
            events.on_progress(progress);
            events.on_sessions_changed();
        }
    }

    if sync_parent_links(adapters, store)? {
        events.on_sessions_changed();
    }
    if force_grok_backfill && grok_backfill_succeeded {
        store.finish_grok_parent_backfill()?;
    }

    Ok(())
}

/// 多 location 下关系元数据跟着 parent 会话，而 child 的胜出文件可能在另一根。
/// 因此先接受“关系目标 parent 的胜出文件也属于该快照”的直接边，再跨快照把
/// 嵌套链扁平到 root。解除/换父前重解析 child，恢复被旧父项目覆盖的自身归属。
fn sync_parent_links(adapters: &[Box<dyn AgentAdapter>], store: &Arc<Store>) -> Result<bool> {
    let mut managed_agents = std::collections::HashSet::new();
    let mut links_by_adapter: Vec<std::collections::HashMap<String, String>> =
        Vec::with_capacity(adapters.len());
    for adapter in adapters {
        if adapter.manages_parent_links() {
            managed_agents.insert(adapter.agent());
            links_by_adapter.push(adapter.parent_links().into_iter().collect());
        } else {
            links_by_adapter.push(std::collections::HashMap::new());
        }
    }

    let mut changed = false;
    for agent in managed_agents {
        let sources = store.session_sources_for_agent(agent)?;
        let mut source_by_key = std::collections::HashMap::new();
        let mut owner_by_key = std::collections::HashMap::new();
        for (key, file_path) in &sources {
            if let Some(adapter_ix) = crate::adapters::adapter_ix_for(adapters, agent, file_path) {
                source_by_key.insert(key.clone(), file_path.clone());
                owner_by_key.insert(key.clone(), adapter_ix);
            }
        }

        // meta.json 位于 parent 的 location。只采纳由当前胜出 parent 所属快照
        // 提供的边，避免另一份陈旧备份把已解除的关系重新挂回去。
        let mut direct = std::collections::HashMap::new();
        for (adapter_ix, links) in links_by_adapter.iter().enumerate() {
            if adapters[adapter_ix].agent() != agent {
                continue;
            }
            for (child, parent) in links {
                if source_by_key.contains_key(child)
                    && owner_by_key.get(parent).copied() == Some(adapter_ix)
                {
                    direct.insert(child.clone(), parent.clone());
                }
            }
        }

        // 各 location 只能看见自己的直接/局部链；合并后再走到全局 root。
        let mut desired_map = std::collections::HashMap::new();
        for child in direct.keys() {
            if let Some(parent) = flattened_parent(child, &direct) {
                desired_map.insert(child.clone(), parent);
            }
        }

        // replace_parent_links 会把关系内 child 的 project 覆写成父项目。关系
        // 解除或换父时，先从 child 自己的胜出文件重解析，恢复 fallback project；
        // 解析失败则保留仍有效的旧关系，下一次 Grok 事件继续重试。
        let current = store.parent_links_for_agent(agent)?;
        for (child, old_parent) in current {
            if desired_map.get(&child) == Some(&old_parent) {
                continue;
            }
            let restored = source_by_key.get(&child).is_some_and(|file_path| {
                reparse_for_parent_change(adapters, store, agent, &child, file_path)
            });
            if !restored && source_by_key.contains_key(&old_parent) {
                desired_map.insert(child, old_parent);
            }
        }

        let mut desired: Vec<(String, String)> = desired_map.into_iter().collect();
        desired.sort();
        changed |= store.replace_parent_links(agent, &desired)?;
    }
    Ok(changed)
}

/// watcher 收到关系边车事件、但没有会话主文件可交给 `scan_files` 时使用。
/// 同 agent 的全部 location 必须一起刷新，否则跨 location 的父链会被局部快照截断。
pub fn refresh_parent_links(
    adapters: &[Box<dyn AgentAdapter>],
    store: &Arc<Store>,
    events: &dyn ScanEvents,
    affected_agents: &[AgentId],
) {
    let mut refreshable = false;
    for adapter in adapters {
        if affected_agents.contains(&adapter.agent()) && adapter.manages_parent_links() {
            adapter.begin_scan();
            refreshable = true;
        }
    }
    if !refreshable {
        return;
    }
    match sync_parent_links(adapters, store) {
        Ok(true) => events.on_sessions_changed(),
        Ok(false) => {}
        Err(error) => eprintln!("[scanner] parent-link sidecar refresh failed: {error}"),
    }
}

fn flattened_parent(
    child: &str,
    direct: &std::collections::HashMap<String, String>,
) -> Option<String> {
    let mut current = child;
    let mut seen = std::collections::HashSet::from([child]);
    loop {
        let parent = direct.get(current)?;
        if !seen.insert(parent.as_str()) {
            return None;
        }
        if direct.contains_key(parent) {
            current = parent;
        } else {
            return Some(parent.clone());
        }
    }
}

fn reparse_for_parent_change(
    adapters: &[Box<dyn AgentAdapter>],
    store: &Arc<Store>,
    agent: AgentId,
    key: &str,
    file_path: &str,
) -> bool {
    let Some(adapter_ix) = crate::adapters::adapter_ix_for(adapters, agent, file_path) else {
        return false;
    };
    let adapter = &adapters[adapter_ix];
    let Some(reference) = adapter.file_ref(std::path::Path::new(file_path)) else {
        eprintln!("[scanner] cannot restore detached session source: {file_path}");
        return false;
    };
    let parsed = match adapter.parse_session(&reference) {
        Ok(parsed) => parsed,
        Err(error) => {
            eprintln!("[scanner] detached session reparse failed {file_path}: {error}");
            return false;
        }
    };
    let quick = adapter.quick_meta(std::slice::from_ref(&reference));
    let meta = match quick.as_ref().and_then(|metas| metas.get(file_path)) {
        Some(quick) => adapter.merge_quick_meta(parsed.meta, quick),
        None => parsed.meta,
    };
    if meta.key != key || store.is_key_tombstoned(&meta.key) {
        return false;
    }
    match store.write_session_guarded(&meta, reference.mtime_ms, &parsed.units) {
        Ok(written) => written,
        Err(error) => {
            eprintln!("[scanner] detached session write failed {file_path}: {error}");
            false
        }
    }
}

/// watcher 触发的单文件增量。与 run_scan 走同一道 quick/parsed 合并——
/// 跳过它,Codex 在 state DB 里被用户手动命名的标题就会被首条消息推导的
/// 标题静默覆盖(跨文件不变量 5:quickMeta 双路径)。
pub fn scan_files(
    adapters: &[Box<dyn AgentAdapter>],
    store: &Arc<Store>,
    events: &dyn ScanEvents,
    refs: Vec<SessionFileRef>,
) {
    let affected_agents: std::collections::HashSet<AgentId> =
        refs.iter().map(|reference| reference.agent).collect();
    for adapter in adapters {
        if affected_agents.contains(&adapter.agent()) {
            adapter.begin_scan();
        }
    }
    // 按**实例**分组,不是按 agent:自定义 location 让同 agent 有多实例,
    // 文件必须交给拥有其根的那个(gemini/kimi 的 cwd 反查、codex 的 state DB
    // 都是实例相对侧档);quick_meta 是整库查询,每组只查一次,不能逐文件调。
    // 先路由再查 key 墓碑——墓碑 key 按归属实例的 host 构造(与全量扫描同式)
    let mut by_adapter: std::collections::HashMap<usize, Vec<SessionFileRef>> =
        std::collections::HashMap::new();
    for r in refs {
        if store.is_tombstoned(&r.file_path) {
            continue;
        }
        let Some(ix) = crate::adapters::adapter_ix_for(adapters, r.agent, &r.file_path) else {
            continue;
        };
        if store.is_key_tombstoned(&session_key(r.agent, adapters[ix].host(), &r.native_id)) {
            continue;
        }
        by_adapter.entry(ix).or_default().push(r);
    }

    let mut changed = false;
    for (ix, group) in by_adapter {
        let adapter = &adapters[ix];
        let quick = adapter.quick_meta(&group);
        for r in &group {
            match adapter.parse_session(r) {
                Ok(parsed) => {
                    let meta = match quick.as_ref().and_then(|m| m.get(&r.file_path)) {
                        Some(q) => adapter.merge_quick_meta(parsed.meta, q),
                        None => parsed.meta,
                    };
                    // 合并后 key 改名(codex thread-id)也受墓碑约束
                    if store.is_key_tombstoned(&meta.key) {
                        continue;
                    }
                    // 路径易主(location 编辑改了 agent):旧 key 行不清,新 key
                    // 写入撞 file_path UNIQUE,会话永远停在旧家(Codex review P1)
                    if let Ok(Some(old_key)) = store.key_for_path(&r.file_path) {
                        if old_key.split(':').next() != Some(meta.agent.as_str()) {
                            let _ = store.remove_session(&old_key, false);
                        }
                    }
                    // 副本裁决在写事务内(write_session_guarded):先查后写与
                    // 全量扫描并发交错时,败方能后发落库违背 mtime 裁决
                    //(rsync 刷备份目录带旧 mtime 的事件串,2026-08-24 Codex review)
                    if store
                        .write_session_guarded(&meta, r.mtime_ms, &parsed.units)
                        .unwrap_or(false)
                    {
                        changed = true;
                    }
                }
                Err(e) => eprintln!("[scanner] incremental parse failed {}: {e}", r.file_path),
            }
        }
    }
    if adapters
        .iter()
        .any(|adapter| affected_agents.contains(&adapter.agent()) && adapter.manages_parent_links())
    {
        changed |= sync_parent_links(adapters, store).unwrap_or_else(|error| {
            eprintln!("[scanner] parent-link refresh failed: {error}");
            false
        });
    }
    if changed {
        events.on_sessions_changed();
    }
}
