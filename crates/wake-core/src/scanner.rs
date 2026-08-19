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

/// 全量/增量扫描。quickMeta 先行秒出列表,然后按 mtime 降序逐文件解析。
/// 阻塞执行——调用方放后台线程。进度与终态一律经 `events` 上报,终态由
/// `ScanFinale` 保证送达;返回的 `Result` 只用于调用方自己记日志。
pub fn run_scan(
    adapters: &[Box<dyn AgentAdapter>],
    store: &Arc<Store>,
    events: &dyn ScanEvents,
    full: bool,
) -> Result<()> {
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
    let known = store.known_files()?;
    let mut seen_paths: std::collections::HashSet<String> = std::collections::HashSet::new();
    struct WorkItem<'a> {
        adapter: &'a dyn AgentAdapter,
        r: SessionFileRef,
        quick: Option<SessionMeta>,
    }
    let mut queue: Vec<WorkItem> = Vec::new();

    for adapter in adapters {
        let refs: Vec<SessionFileRef> = adapter
            .list_session_files()?
            .into_iter()
            .filter(|r| !store.is_tombstoned(&r.file_path))
            .collect();
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
                    let changed = match known.get(&r.file_path) {
                        None => true,
                        Some((mtime, size, _)) => *mtime != r.mtime_ms || *size != r.size,
                    };
                    if changed || full {
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
            let changed = match known.get(&r.file_path) {
                None => true,
                Some((mtime, size, _)) => *mtime != r.mtime_ms || *size != r.size,
            };
            if full || changed {
                let quick = quick_map.as_ref().and_then(|m| m.get(&r.file_path).cloned());
                queue.push(WorkItem {
                    adapter: adapter.as_ref(),
                    r,
                    quick,
                });
            }
        }
    }

    // 删除检测:库里有但磁盘没了
    for (path, (_, _, key)) in &known {
        if !seen_paths.contains(path) {
            let _ = store.remove_session(key, false);
        }
    }

    // 最近的会话优先
    queue.sort_by(|a, b| b.r.mtime_ms.cmp(&a.r.mtime_ms));
    progress.total = queue.len();
    events.on_progress(progress);

    let mut last_notify = std::time::Instant::now();
    for item in &queue {
        match item.adapter.parse_session(&item.r) {
            Ok(parsed) => {
                // quick/parsed 合并策略属各 adapter(默认 parsed 为准 quick 补缺,
                // Codex 覆写 title/key 优先级),scanner 不再内嵌任何 agent 特例
                let meta = match &item.quick {
                    Some(q) => item.adapter.merge_quick_meta(parsed.meta, q),
                    None => parsed.meta,
                };
                if let Err(e) = store.write_session(&meta, item.r.mtime_ms, &parsed.units) {
                    eprintln!("[scanner] write failed {}: {e}", item.r.file_path);
                }
            }
            Err(e) => eprintln!("[scanner] parse failed {}: {e}", item.r.file_path),
        }
        progress.done += 1;
        if last_notify.elapsed().as_millis() > 800 || progress.done == progress.total {
            last_notify = std::time::Instant::now();
            events.on_progress(progress);
            events.on_sessions_changed();
        }
    }

    Ok(())
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
    // 按 agent 分组:quick_meta 是整库查询(Codex 要开 state DB 读整张 threads),
    // 每组只查一次,不能逐文件调
    // 同一会话可能由多个被监听文件触发(Grok 的 updates.jsonl + summary.json
    // 都映射到同一个 ref),watcher 只按路径去重,这里必须按 file_path 再去一次,
    // 否则一次 turn 会把整段 FTS 删掉重建两遍
    let mut by_agent: std::collections::HashMap<AgentId, Vec<SessionFileRef>> =
        std::collections::HashMap::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for r in refs {
        if store.is_tombstoned(&r.file_path) {
            continue;
        }
        if !seen.insert(r.file_path.clone()) {
            continue;
        }
        by_agent.entry(r.agent).or_default().push(r);
    }

    let mut changed = false;
    for (agent, group) in by_agent {
        let Some(adapter) = adapters.iter().find(|a| a.agent() == agent) else {
            continue;
        };
        let quick = adapter.quick_meta(&group);
        for r in &group {
            match adapter.parse_session(r) {
                Ok(parsed) => {
                    let meta = match quick.as_ref().and_then(|m| m.get(&r.file_path)) {
                        Some(q) => adapter.merge_quick_meta(parsed.meta, q),
                        None => parsed.meta,
                    };
                    if store.write_session(&meta, r.mtime_ms, &parsed.units).is_ok() {
                        changed = true;
                    }
                }
                Err(e) => eprintln!("[scanner] incremental parse failed {}: {e}", r.file_path),
            }
        }
    }
    if changed {
        events.on_sessions_changed();
    }
}
