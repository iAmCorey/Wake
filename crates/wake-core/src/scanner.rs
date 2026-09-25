use crate::adapters::AgentAdapter;
use crate::db::{IndexLock, LockHolder, Ownership, Store};
use crate::models::*;
use crate::text::plural;
use anyhow::Result;
use std::path::Path;
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
/// into_inner 照常放行。排在后面的照常出终态事件,UI 只是多等一会儿。进程**之间**
/// 由 `db::IndexLock` 管(GUI 整个生命周期持有、CLI 写命令干活时持有),两者不可
/// 互代:按扫描粒度拿文件锁会与 GUI 的常驻持有自锁(同进程第二个文件描述是另一把 flock)
static SCAN_GATE: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// `build_index` / `refresh_index` 的收场;措辞由 bin 翻。退让不是失败
pub enum Outcome {
    /// 干完了,库里现在有这些
    Done(Tally),
    /// 前置条件不满足、库一个字节没动
    Skipped(Skip),
    /// 别的进程正持有这份索引(GUI 在跑,或另一个 wake-cli)
    Busy(LockHolder),
}

/// `Outcome::Skipped` 的缘由;措辞与退出码由 bin 定(`Exists` 是正常收场,其余都是
/// `--db` 拿错了路径)
pub enum Skip {
    /// `index`:库已在
    Exists,
    /// `refresh`:没有库
    Missing,
    /// `refresh`:`--db` 指到的不是 Wake 的索引(别家 SQLite)
    NotAnIndex,
    /// `refresh`:有远程会话入库,镜像目录却不在这条路径旁——给的不是 GUI 开它的那条路径
    MirrorsElsewhere,
}

/// 索引里有多少:两个写命令收尾说的同一句
pub struct Tally {
    pub sessions: i64,
    pub agents: i64,
}

impl Tally {
    pub fn of(store: &Store) -> Result<Self> {
        let sessions = store
            .list_sessions(&SessionFilter {
                limit: 1,
                ..Default::default()
            })?
            .1;
        let agents = store.agent_counts()?.len() as i64;
        Ok(Self { sessions, agents })
    }
}

impl std::fmt::Display for Tally {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} session{} from {} agent{}",
            self.sessions,
            plural(self.sessions),
            self.agents,
            plural(self.agents)
        )
    }
}

/// 从零建一次索引:**只在索引文件不存在时**建,已经有库就原样留给 GUI(`Skipped`)。
/// 这是"重建权只归 GUI 的 `open_or_rebuild`"(db.rs)对"建"的唯一例外,场景是
/// "装了 Wake 但从没启动过";已有的库要跟上磁盘用 `refresh_index`。
///
/// 先拿 `IndexLock`(拿不到即 `Busy` 退让:GUI 正在启动、下一步就是自己建库),拿到
/// 再看一次库在不在。扫描仍落在旁边的 `<db>.build-<pid>`,扫完才用 `hard_link` 占位——
/// 锁管的是"同一时刻只有一个写者",这一层管的是"中途死掉不留半截":
/// ① 中途报错 / panic / 被 agent 超时杀掉,目标路径一个字节都没出现过,下次
///    还能重试(不然半截索引会被开头那道 exists 当成"已经有了"永久挡住,而
///    只读查询会把它当正常结果);
/// ② 不认这把锁的老版本 GUI 在这几秒里首次启动建了库,hard_link 对它照样是
///    AlreadyExists——我们丢掉自己那份退让,绝不往对方的库里写;
/// ③ 用 hard_link 不用 rename:rename 无条件覆盖,给不出"已存在即失败"这条
///    语义(POSIX 与 Windows 都保证)。
///
/// 占位成功后清孤儿边车:目标刚才还不存在,躺在那儿的 -wal/-shm 只可能是孤儿(主库
/// 被手删,或 open_or_rebuild 挪库与删边车之间崩过),留着任何一个,新库都会接着读旧
/// 日志——锁在手,这几十微秒里没人会开这个库。**别加 --force**:重建已有的库归 GUI
pub fn build_index(path: &Path, events: &dyn ScanEvents) -> Result<Outcome> {
    if path.exists() {
        return Ok(Outcome::Skipped(Skip::Exists));
    }
    let _lock = match IndexLock::try_acquire(path, "wake-cli index")? {
        Ownership::Ours(lock) => lock,
        Ownership::Held(holder) => return Ok(Outcome::Busy(holder)),
    };
    // 锁外那一眼可能过时:GUI 刚建完库又退出了
    if path.exists() {
        return Ok(Outcome::Skipped(Skip::Exists));
    }
    let staging = Staging::new(path);
    let tally = {
        let store = Arc::new(Store::open(&staging.0)?);
        scan_with(&store, events, true)?
    }; // 关掉最后一个连接即 checkpoint,-wal/-shm 消失,临时库自成一体
    match std::fs::hard_link(&staging.0, path) {
        Ok(()) => {}
        // 有人抢先了(不认锁的老版本 GUI 首扫)。他的库归他,退让
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            return Ok(Outcome::Skipped(Skip::Exists))
        }
        // 个别文件系统不给硬链接:退回 rename,但只在目标确实还空着时
        Err(_) if !path.exists() => std::fs::rename(&staging.0, path)?,
        Err(_) => return Ok(Outcome::Skipped(Skip::Exists)),
    }
    for suffix in ["-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
    }
    Ok(Outcome::Done(tally))
}

/// 增量刷新一份**已有**的索引(`wake-cli refresh`):给"app 不开、靠 MCP / CLI 用 Wake"
/// 的人用 launch agent / systemd timer 定时跑(issue #43)。做的正是 GUI 启动与 ⌘R
/// 那一轮——增量 `run_scan`,收尾同步记忆,升级后的 FTS 回填旗子照常生效——只少远程
/// rsync(要 ssh,留给 GUI 的同步线程)。
///
/// **只在 Wake 没运行时干活**:`IndexLock` 拿不到就 `Busy` 退让。GUI 开着时它的 watcher
/// 本来就在保鲜,再跑一轮不只是多余——两个进程各建一份 roster,看到的 env 未必相同,
/// 删除检测会互删对方收进来的会话(理由在 `IndexLock` 的注释),锁把这类并发整个排除。
/// `Store::open` 会顺手迁移 schema,与 GUI 首开同一条路:刷新的目的就是让库跟上二进制;
/// 反过来(库比二进制新)认不出来,docs 让定时任务指向 app 包内那份 wake-cli
pub fn refresh_index(path: &Path, events: &dyn ScanEvents) -> Result<Outcome> {
    if !path.is_file() {
        return Ok(Outcome::Skipped(Skip::Missing));
    }
    // 开写之前先只读认一眼:`--db` 指错到别家 SQLite 时 `Store::open` 会改它的 journal_mode、
    // 建 Wake 的表,而别家数据只读是铁律(Codex review 2026-09-23)
    if !crate::db::is_wake_index(path) {
        return Ok(Outcome::Skipped(Skip::NotAnIndex));
    }
    let _lock = match IndexLock::try_acquire(path, "wake-cli refresh")? {
        Ownership::Ours(lock) => lock,
        Ownership::Held(holder) => return Ok(Outcome::Busy(holder)),
    };
    let store = Arc::new(Store::open(path)?);
    if mirrors_elsewhere(&store)? {
        return Ok(Outcome::Skipped(Skip::MirrorsElsewhere));
    }
    Ok(Outcome::Done(scan_with(&store, events, false)?))
}

/// 远程镜像按 `Store::db_dir()` 找(与 GUI 同一约定:挂在打开的那个路径旁)。给的是真库的
/// 别名、或被链接到别处的真库本身,而 GUI 开的是另一条路径时,这里的 `remotes/<host>` 不在,
/// 枚举为空,删除检测会把远程会话整批当"磁盘已删"清掉、还报成功——有远程会话入库、镜像
/// 根却不在,就是拿错了路径(Codex review 2026-09-23)。"有东西入库"要连记忆一起看:
/// 转录清掉了、Codex 记忆还在的 host 没有会话行,`sync_memories` 照样会把它的记忆当消失
/// 的来源整组删掉(同轮 review)
fn mirrors_elsewhere(store: &Store) -> Result<bool> {
    let Some(db_dir) = store.db_dir() else {
        return Ok(false);
    };
    let sessions = store.host_counts()?;
    let memories: std::collections::HashSet<String> = store
        .memory_groups()?
        .into_iter()
        .map(|(_, host)| host)
        .filter(|host| !host.is_empty())
        .collect();
    Ok(store.enabled_remote_host_names().iter().any(|host| {
        (sessions.get(host).is_some_and(|n| *n > 0) || memories.contains(host))
            && !crate::remote::host_cache_dir(&db_dir, host).is_dir()
    }))
}

/// 按库里的 location / remote host 配置建 roster(不变量 8⑥)、扫一轮、数一数。读写
/// `Store` 由调用方持有并在放锁前关掉(最后一个连接关掉即 checkpoint,-wal/-shm 消失)
fn scan_with(store: &Arc<Store>, events: &dyn ScanEvents, full: bool) -> Result<Tally> {
    let adapters = crate::adapters::create_adapters_for(store);
    run_scan(&adapters, store, events, full)?;
    Tally::of(store)
}

/// 临时库的清场:正常收尾、`?` 提前返回、panic unwind 三条路都要把它连同边车
/// 删干净,不然一次失败就在用户的索引目录里留下几百 MB。占位成功后这一删只是
/// 去掉临时那个名字,inode 已经挂在目标名下
struct Staging(std::path::PathBuf);

impl Staging {
    fn new(target: &Path) -> Self {
        let me = Staging(std::path::PathBuf::from(format!(
            "{}.build-{}",
            target.display(),
            std::process::id()
        )));
        me.clear(); // pid 会重用:上一次被 SIGKILL 留下的同名残骸先扫掉
        me
    }

    fn clear(&self) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", self.0.display()));
        }
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        self.clear();
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

/// 只同步记忆、不碰会话:Memory 页的 Refresh 与 Settings → Memory locations 变更的
/// 收尾(用户 2026-09-22 定"memory 的只刷 memory、session 的只刷 session,不然设置里
/// 分开的 locations 就没意义")。读的是当下的 Memory locations 配置(停用 / 自定义 /
/// 项目模式),与扫描收尾那一步是同一个函数;过同一把 `SCAN_GATE` 与进行中的扫描
/// 串行——它收尾也对 memories 对账,两边别同时写同一组。阻塞执行,调用方放后台;
/// 返回库里有没有改动
pub fn run_memory_sync(adapters: &[Box<dyn AgentAdapter>], store: &Arc<Store>) -> bool {
    let _gate = SCAN_GATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    sync_memories(adapters, store)
}

/// 写事务内副本裁决(`Store::write_session_guarded`)用的位次查询:按 file_path
/// 找拥有它的实例、问它这份副本的 `dedup_rank`;无实例认领的路径落到该家首个
/// 实例——与枚举时的候选排序同一把尺子,全量与增量两条写库路径才给出同一个胜者
fn rank_of<'a>(
    adapters: &'a [Box<dyn AgentAdapter>],
    agent: crate::models::AgentId,
) -> impl Fn(&str) -> u8 + 'a {
    move |path| {
        crate::adapters::adapter_for(adapters, agent, path).map_or(0, |a| a.dedup_rank(path))
    }
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
    // 认领先对账、再枚举:新认领的替身在这一步就出库,枚举时直接跳过、不白解析;
    // 撤销的认领(认领方会话没了)这一轮就放替身回来
    refresh_claims(adapters, store, events);
    let claimed = store.claimed_keys().unwrap_or_default();
    let force_grok_backfill = store.needs_grok_parent_backfill();
    // FTS 派生规则换代(db::FTS_FORMAT):这一轮把 mtime/size 没变的也全部重解析。
    // 跑完就清旗子,不按"全部成功"重试——解析失败的文件下次也不会自己好,它变了
    // 自然走增量重解析;留着旗子只会让每次启动都全量一遍
    let force_reindex = store.needs_fts_reindex();
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
            // 被外壳产品认领的替身(Craft 的 Claude 后端在 ~/.claude 落的那份)不索引:
            // 不在 seen_paths 里,库里若还有它的行,下面的删除检测一并清掉
            .filter(|r| !claimed.contains(&key_of(ix, r)))
            // 无任何根认领的引用(越界枚举或合成测试)保守放行给枚举者;
            // 过滤只裁决"确有更深的根拥有它"的情形
            .filter(|r| owner_of(&r.file_path).is_none_or(|o| o == ix))
            .collect();
        per_adapter.push(refs);
    }
    // 同家同 ID 去重:同一会话在默认根与自定义根各有一份副本时,两个文件会
    // 每轮轮流改写同一行(key 相同,file_path 摇摆)。候选按 (副本 dedup_rank
    // 小者, mtime 新者, 平局路径字典序小者) 排序,首位入队,其余留作**解析
    // 失败的回退顺位**——rank 让一家固定偏好某一份(Cursor 的转录源永远压过
    // IDE 库副本,dsh 的新一代日志永远压过旧代),mtime 只在同级副本之间裁决
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
        v.sort_by(|(ia, a), (ib, b)| {
            adapters[*ia]
                .dedup_rank(&a.file_path)
                .cmp(&adapters[*ib].dedup_rank(&b.file_path))
                .then_with(|| b.mtime_ms.cmp(&a.mtime_ms))
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
        let force_adapter =
            force_reindex || (force_grok_backfill && adapter.agent() == AgentId::Grok);
        for r in &refs {
            seen_paths.insert(r.file_path.clone());
        }

        // Sidecar metadata can arrive after the transcript's final write.
        // Refresh it independently of the mtime/size gate and notify the UI.
        if store.update_sidecar_meta(&refs, &adapter.sidecar_updates(&refs))? {
            events.on_sessions_changed();
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
                match store.write_session_guarded(
                    &meta,
                    item.r.mtime_ms,
                    &parsed.units,
                    &parsed.wake_lookups,
                    &rank_of(adapters, meta.agent),
                    None,
                ) {
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
                            // 库里这条 key 的行若正是刚解析失败的胜者(quick 阶段
                            // 的占位,或早先入库、后来损坏的转录),位次会让它挡住
                            // 回退副本——把它作为 supersedes 交给写事务:它的正文
                            // 已经读不出来,留着只会让详情页永远打不开;让位的判定
                            // 与写入同一事务,不在这里先删(Codex review 第二、三轮)
                            match store.write_session_guarded(
                                &meta,
                                fb.mtime_ms,
                                &parsed.units,
                                &parsed.wake_lookups,
                                &rank_of(adapters, meta.agent),
                                Some((item.r.file_path.as_str(), item.r.mtime_ms)),
                            ) {
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

    if sync_parent_links(adapters, store, None)? {
        events.on_sessions_changed();
    }
    if sync_memories(adapters, store) {
        events.on_sessions_changed();
    }
    if force_grok_backfill && grok_backfill_succeeded {
        store.finish_grok_parent_backfill()?;
    }
    if force_reindex {
        store.finish_fts_reindex()?;
    }

    Ok(())
}

/// 记忆文档随每轮扫描整组刷新:同 (agent, host) 的各实例(默认根 + 自定义根;远程
/// 镜像按 host 自成组)合并后交给 store 按组替换,消失的文件随之出库。项目归属
/// 不在这里算(读库时按 session_key 连 sessions)。某家读失败只 eprintln 并**跳过
/// 该组**(None,不替换,免得一次瞬时失败把库里那组清空),不截断整轮——与会话
/// 枚举同规矩。库里有、roster 里已经没有的分组(删掉的远程 host、停用的 agent)
/// 整组清掉——否则它们的记忆在会话与缓存都没了之后还能被列出、搜到(Codex review
/// 2026-09-17)
fn sync_memories(adapters: &[Box<dyn AgentAdapter>], store: &Arc<Store>) -> bool {
    /// 一组的收集结果:读成功的实例给文档,读失败的实例给它的来源 id(那些来源的行冻结)
    #[derive(Default)]
    struct Group {
        docs: Vec<MemoryDoc>,
        frozen: Vec<String>,
        succeeded: usize,
        failed: usize,
    }
    // 三样配置都从库里读:读不出就整轮不动——这一步的本质是"按缺席删",把读失败折成
    // "什么都没配置"会删光项目指令文件、把停用的来源重新索引进来(2026-09-22 review)
    let projects = match store.local_project_roots() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[scanner] memory sync skipped: project roots unreadable: {e}");
            return false;
        }
    };
    let (customs, disabled) = match store.memory_source_overrides() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[scanner] memory sync skipped: memory locations unreadable: {e}");
            return false;
        }
    };
    let stored: std::collections::HashSet<(AgentId, String)> = match store.memory_groups() {
        Ok(v) => v.into_iter().collect(),
        Err(e) => {
            eprintln!("[scanner] memory sync skipped: memory groups unreadable: {e}");
            return false;
        }
    };
    // 项目模式(`<project>/CLAUDE.md`)在已索引的本地项目根上展开;停用与自定义来源来自
    // Settings → Memory locations,按实例裁决只在 `memory_source_plan`(Settings 面板与
    // 表单查重看的是同一份)
    let plan = crate::adapters::memory_source_plan(adapters, &customs, &disabled);
    let mut groups: std::collections::BTreeMap<(AgentId, String), Group> =
        std::collections::BTreeMap::new();
    for key in &stored {
        groups.entry(key.clone()).or_default();
    }
    for planned in plan {
        let group = groups
            .entry((planned.agent, planned.host.clone()))
            .or_default();
        let enabled: Vec<MemorySource> = planned
            .sources
            .into_iter()
            .filter(|p| p.enabled)
            .map(|p| p.source)
            .collect();
        if enabled.is_empty() {
            // 没有来源(或全停用)的实例什么都不贡献,但这一组照常对账——停用的来源
            // 的行要随之出库
            group.succeeded += 1;
            continue;
        }
        // 逐来源读:一个来源读失败只冻结它自己的行——挂空的符号链接、TCC 挡住的项目根
        // 不该让整家的记忆停更(2026-09-22 review);缓存按来源分槽,逐个调不比整批贵。
        // 远程实例不展开项目模式(项目根是本机路径):RemoteAdapter 自己给 inner 传空
        for source in enabled {
            let one = std::slice::from_ref(&source);
            let read = match planned.adapter {
                Some(ix) => adapters[ix].list_memories(one, &projects),
                // 该家没有本地实例(会话 location 全停用):项目指令文件与自定义路径照读;
                // 没有实例就没处放缓存,这种配置少见、每轮重读
                None => crate::adapters::generic_memory_docs(
                    &crate::adapters::MemoryCache::new(),
                    planned.agent,
                    one,
                    &projects,
                ),
            };
            match read {
                Ok(docs) => {
                    group.docs.extend(docs);
                    group.succeeded += 1;
                }
                Err(e) => {
                    eprintln!(
                        "[scanner] memories of {} ({}) failed: {e}",
                        planned.agent.as_str(),
                        source.id()
                    );
                    group.frozen.push(source.id());
                    group.failed += 1;
                }
            }
        }
    }
    let mut changed = false;
    for ((agent, host), group) in groups {
        // 这一组的实例全失败:整组原样保留(roster 里没有实例的组是 0/0,照常清空)
        if group.succeeded == 0 && group.failed > 0 {
            continue;
        }
        // 没读到文档、没有冻结、库里也没有这一组:没有账可对,别为二十个实例各开一个
        // 写事务(2026-09-22 /simplify)
        if group.docs.is_empty()
            && group.frozen.is_empty()
            && !stored.contains(&(agent, host.clone()))
        {
            continue;
        }
        match store.replace_memories(agent, &host, &group.docs, &group.frozen) {
            Ok(c) => changed |= c,
            Err(e) => eprintln!(
                "[scanner] memories write failed for {}: {e}",
                agent.as_str()
            ),
        }
    }
    changed
}

/// 多 location 下关系元数据跟着 parent 会话，而 child 的胜出文件可能在另一根。
/// 因此先接受“关系目标 parent 的胜出文件也属于该快照”的直接边，再跨快照把
/// 嵌套链扁平到 root。解除/换父前重解析 child，恢复被旧父项目覆盖的自身归属。
/// `only` 把对账限在这些 agent(watcher 增量只动了它们的文件;关系只连同家会话,
/// 别家的快照不会因此变);全量扫描给 None 对账全部
fn sync_parent_links(
    adapters: &[Box<dyn AgentAdapter>],
    store: &Arc<Store>,
    only: Option<&std::collections::HashSet<AgentId>>,
) -> Result<bool> {
    let mut managed_agents = std::collections::HashSet::new();
    let mut unknown_agents = std::collections::HashSet::new();
    let mut links_by_adapter: Vec<std::collections::HashMap<String, String>> =
        Vec::with_capacity(adapters.len());
    for adapter in adapters {
        if adapter.manages_parent_links() && only.is_none_or(|only| only.contains(&adapter.agent()))
        {
            managed_agents.insert(adapter.agent());
            match adapter.parent_links() {
                Some(links) => links_by_adapter.push(links.into_iter().collect()),
                None => {
                    // 这一刻读不出关系(state DB 打不开):这一家整段跳过,库里的关系
                    // 原样保留——拿空快照对账就是整家清空(2026-09-21 review)
                    eprintln!(
                        "[scanner] parent links of {} unreadable this round; keeping the indexed ones",
                        adapter.agent().as_str()
                    );
                    unknown_agents.insert(adapter.agent());
                    links_by_adapter.push(std::collections::HashMap::new());
                }
            }
        } else {
            links_by_adapter.push(std::collections::HashMap::new());
        }
    }

    let mut changed = false;
    for agent in managed_agents {
        if unknown_agents.contains(&agent) {
            continue;
        }
        // 这一家眼下一条关系都没有、库里也没有:整段对账是纯浪费——
        // session_sources_for_agent 要把这家全部会话读一遍,replace_parent_links
        // 还要开写事务与扫描线程抢锁。全量扫描对每家管关系的都走一遍,而多数
        // agent 常年零关系(Codex 是 spawn_agent 用过才有)
        let current = store.parent_links_for_agent(agent)?;
        let has_links = links_by_adapter
            .iter()
            .enumerate()
            .any(|(ix, links)| adapters[ix].agent() == agent && !links.is_empty());
        if !has_links && current.is_empty() {
            continue;
        }

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
        // 提供的边，避免另一份陈旧备份把已解除的关系重新挂回去。关系写在 child
        // 自己文件里的家(`parent_links_in_child`,Craft)按同一道理换成 child 的
        // 胜出文件:那份首行才是现行的关系,parent 归哪个 location 无关
        let mut direct = std::collections::HashMap::new();
        for (adapter_ix, links) in links_by_adapter.iter().enumerate() {
            if adapters[adapter_ix].agent() != agent {
                continue;
            }
            let in_child = adapters[adapter_ix].parent_links_in_child();
            for (child, parent) in links {
                let holder = if in_child { child } else { parent };
                if source_by_key.contains_key(child)
                    && owner_by_key.get(holder).copied() == Some(adapter_ix)
                {
                    direct.insert(child.clone(), parent.clone());
                }
            }
        }
        // 上面那条"报边的实例必须拥有 parent"是照 Grok 写的:它的边车长在
        // parent 自己的 location 里,报边者天然就是拥有者。Codex 的
        // thread_spawn_edges 是 home 的一张总表,parent 的胜出文件完全可能
        // 归另一个同家实例(用户给某个备份目录加了 location)。所以补一轮:
        // 上一轮没认领的 child,只要 parent 确实在库里就接受——先来后到按
        // roster 顺序,权威那一轮的结论不会被这一轮盖掉
        for (adapter_ix, links) in links_by_adapter.iter().enumerate() {
            // 只对全局总表(Codex)放开:location 级边车(Grok)仍只认"报边者拥有 parent",
            // 否则备份目录里过期的边车会把用户已解除的关系每轮重新挂上(2026-09-22 review)
            if adapters[adapter_ix].agent() != agent || !adapters[adapter_ix].parent_links_global()
            {
                continue;
            }
            for (child, parent) in links {
                if source_by_key.contains_key(child)
                    && owner_by_key.contains_key(parent)
                    && !direct.contains_key(child)
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

        // 仍被 adapter 声称的关系,不算"已解除"。parent 行这一刻不在库里是
        // 常态:watcher 的增量不走 SCAN_GATE,与全量扫描并发,一批事件很可能
        // 在 parent 写进库之前就读了 sources 快照。少了这道判断,这一批会拿
        // replace_parent_links 的整家清空把全量刚建好的关系抹掉,而 Codex 没有
        // is_parent_link_event、state DB 也不在监听路径里,没有第二次机会补救
        // 关系写在 child 里的家只算 child 胜出文件那份的声称:落选的旧备份还写着的
        // 过期关系不该靠这道守卫留下来
        let owners = &owner_by_key;
        let asserted: std::collections::HashSet<(&str, &str)> = links_by_adapter
            .iter()
            .enumerate()
            .filter(|(adapter_ix, _)| adapters[*adapter_ix].agent() == agent)
            .flat_map(|(adapter_ix, links)| {
                let in_child = adapters[adapter_ix].parent_links_in_child();
                links
                    .iter()
                    .filter(move |(child, _)| {
                        !in_child || owners.get(*child).copied() == Some(adapter_ix)
                    })
                    .map(|(child, parent)| (child.as_str(), parent.as_str()))
            })
            .collect();

        // replace_parent_links 会把关系内 child 的 project 覆写成父项目。关系
        // 解除或换父时，先从 child 自己的胜出文件重解析，恢复 fallback project；
        // 解析失败则保留仍有效的旧关系，下一次 Grok 事件继续重试。
        for (child, old_parent) in current {
            if desired_map.get(&child) == Some(&old_parent) {
                continue;
            }
            // 保留不会留下悬空关系:replace_parent_links 的 UPDATE 带
            // EXISTS(parent),parent 真没了这条边写不进去。只在这一轮算不出这个
            // child 的父时才保留:算得出(parent 已入库、链已扁平到 root)就以算出的
            // 为准——否则先入库的直接边 child→mid 会一直压住 child→root,两层
            // spawn 链永远钉在中间那层(2026-09-21 review)
            if !desired_map.contains_key(&child)
                && asserted.contains(&(child.as_str(), old_parent.as_str()))
            {
                desired_map.insert(child, old_parent);
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

/// 外壳产品对别家替身的认领(`AgentAdapter::claimed_sessions`)按认领方 agent 整组对账:
/// 同家各实例(默认根、自定义根、远程镜像)的结果合并,key 按**报认领的实例**的 host 拼
/// (与枚举时的 key_of 同一个 session_key);任一实例这一刻读不出(None)这一家整段跳过、
/// 库里的认领原样保留(与 sync_parent_links 同一纪律)。库里有、roster 里已经没有的认领方
/// (location 全删或停用)整组撤销——它藏起来的替身得放回来。替身的删除在
/// `replace_claims` 的事务里做;返回库是否变了
fn sync_claims(adapters: &[Box<dyn AgentAdapter>], store: &Arc<Store>) -> Result<bool> {
    let mut claims: std::collections::BTreeMap<AgentId, Option<Vec<String>>> =
        std::collections::BTreeMap::new();
    for adapter in adapters.iter().filter(|a| a.manages_claims()) {
        let slot = claims
            .entry(adapter.agent())
            .or_insert_with(|| Some(Vec::new()));
        match adapter.claimed_sessions() {
            Some(claimed) => {
                if let Some(keys) = slot {
                    keys.extend(
                        claimed
                            .iter()
                            .map(|(agent, native)| session_key(*agent, adapter.host(), native)),
                    );
                }
            }
            None => {
                eprintln!(
                    "[scanner] claims of {} unreadable this round; keeping the indexed ones",
                    adapter.agent().as_str()
                );
                *slot = None;
            }
        }
    }
    for claimant in store.claimants()? {
        claims.entry(claimant).or_insert_with(|| Some(Vec::new()));
    }
    let mut changed = false;
    for (claimant, keys) in claims {
        if let Some(keys) = keys {
            changed |= store.replace_claims(claimant, &keys)?;
        }
    }
    Ok(changed)
}

/// 对账认领并通知列表;全量扫描开头与 watcher(认领方的快照事件,写库之前)共用。
/// 失败只记日志、不截断扫描——写入闸门仍按库里现有的认领把关。撤销的认领不会让替身
/// 在这一刻回来:它的行早删了、文件也没变,下一轮扫描枚举到它才重新入库
pub fn refresh_claims(
    adapters: &[Box<dyn AgentAdapter>],
    store: &Arc<Store>,
    events: &dyn ScanEvents,
) {
    match sync_claims(adapters, store) {
        Ok(true) => events.on_sessions_changed(),
        Ok(false) => {}
        Err(error) => eprintln!("[scanner] claim refresh failed: {error}"),
    }
}

/// watcher 收到快照事件(关系边车一类)、但没有会话主文件可交给 `scan_files` 时使用。
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
    let only: std::collections::HashSet<AgentId> = affected_agents.iter().copied().collect();
    match sync_parent_links(adapters, store, Some(&only)) {
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
    match store.write_session_guarded(
        &meta,
        reference.mtime_ms,
        &parsed.units,
        &parsed.wake_lookups,
        &rank_of(adapters, agent),
        None,
    ) {
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
    let mut changed = false;
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
        let key = session_key(r.agent, adapters[ix].host(), &r.native_id);
        // 认领的替身连解析都省了;合并改过 key 的情形由写入闸门兜住
        if store.is_key_tombstoned(&key) || store.is_key_claimed(&key) {
            continue;
        }
        by_adapter.entry(ix).or_default().push(r);
    }

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
                        .write_session_guarded(
                            &meta,
                            r.mtime_ms,
                            &parsed.units,
                            &parsed.wake_lookups,
                            &rank_of(adapters, meta.agent),
                            None,
                        )
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
        changed |=
            sync_parent_links(adapters, store, Some(&affected_agents)).unwrap_or_else(|error| {
                eprintln!("[scanner] parent-link refresh failed: {error}");
                false
            });
    }
    if changed {
        events.on_sessions_changed();
    }
}
