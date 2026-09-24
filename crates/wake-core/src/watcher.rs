use crate::adapters::AgentAdapter;
use crate::db::Store;
use crate::models::*;
use crate::scanner::{refresh_claims, refresh_parent_links, scan_files, ScanEvents};
use notify::{RecursiveMode, Watcher};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

/// 监听 adapter 数据目录,800ms 去抖后做单文件增量。
/// drop 即停:先撤事件源(通道关闭、线程退出循环),再 **join 等线程真正
/// 退出**——roster 换代时旧线程可能正持旧 roster/Store 在写库,不等它收尾,
/// 已移除根的会话会在新 roster 补扫之后被写回、复活到下次手动刷新
/// (2026-08-24 Codex review P1)。在批处理粒度上等待,通常毫秒级
pub struct SessionWatcher {
    watcher: Option<notify::RecommendedWatcher>,
    thread: Option<std::thread::JoinHandle<()>>,
    watched: std::collections::BTreeSet<PathBuf>,
}

impl SessionWatcher {
    /// 实际 watch 成功的根(缺目录等失败被静默跳过的不在内)——"挂了哪些"
    /// 的唯一真话。远程同步收工后据此判断缓存树是否长出了监听外的新根。
    pub fn watched_roots(&self) -> &std::collections::BTreeSet<PathBuf> {
        &self.watched
    }
}

impl Drop for SessionWatcher {
    fn drop(&mut self) {
        self.watcher.take();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// 事件路径 → 归属 agent:取**最长匹配根**,不是第一个命中。env 自定义根
/// (CODEX_HOME/XDG_DATA_HOME)可以落在别家数据树内,此时事件路径同时匹配
/// 两个根,按 roster 顺序取首个会把事件分给外层那家——file_ref 又对 .jsonl
/// 很宽松,会以错误的 agent 入库(错 key + 同 file_path 还会触发 UNIQUE 冲突)。
/// 更深的根必然是更具体的归属;Path::starts_with 按组件比较,同名前缀兄弟
/// 目录不会误匹配
pub fn resolve_watch_agent<T: Copy>(roots: &[(PathBuf, T)], path: &Path) -> Option<T> {
    roots
        .iter()
        .filter(|(root, _)| path.starts_with(root))
        .max_by_key(|(root, _)| root.components().count())
        .map(|(_, tag)| *tag)
}

/// 胜者副本被删后的幸存者上位:按被删 key 反查同家实例枚举里的同 native_id
/// 引用,交常规增量收编(该 key 已无既有行,scan_files 的副本裁决自然放行)。
/// 期望 key 经 session_key 按实例 host 构造——远程三段 key 也走名字快路径。
/// codex 经 state DB 改写过的 key(thread-id ≠ 文件 native_id)反查不到,由
/// 下一次全量扫描兜底;删除事件罕见,逐家枚举(纯 stat)代价可忽略
pub fn promote_survivors(
    adapters: &[Box<dyn AgentAdapter>],
    store: &Arc<Store>,
    events: &dyn ScanEvents,
    removed_keys: &[String],
) {
    let mut seen = std::collections::HashSet::new();
    let mut survivors: Vec<SessionFileRef> = Vec::new();
    for key in removed_keys {
        if !seen.insert(key.as_str()) {
            continue;
        }
        let Some(agent) = key.split(':').next().and_then(AgentId::from_str) else {
            continue;
        };
        let mut agent_refs: Vec<SessionFileRef> = Vec::new();
        let mut by_name: Vec<SessionFileRef> = Vec::new();
        for a in adapters.iter().filter(|a| a.agent() == agent) {
            // host 段对不上的实例整个跳过(本地实例对远程 key、A 机对 B 机):
            // 名字与解析比对都不可能命中——实例出口的 key 恒带自己的 host
            let Some(native_id) = strip_key_prefix(key, agent, a.host()) else {
                continue;
            };
            if let Ok(refs) = a.list_session_files() {
                by_name.extend(refs.iter().filter(|r| r.native_id == native_id).cloned());
                agent_refs.extend(refs);
            }
        }
        if !by_name.is_empty() {
            survivors.extend(by_name);
            continue;
        }
        // key 后缀是**内容 id** 的家(gemini 的 sessionId、pi/dsh 的首行 id)
        // 文件名对不上——退而解析比对,规模封顶防重罚(超限交下次全量扫描)
        if agent_refs.len() <= 32 {
            for r in agent_refs {
                let Some(owner) = crate::adapters::adapter_for(adapters, r.agent, &r.file_path)
                else {
                    continue;
                };
                if owner
                    .parse_session(&r)
                    .is_ok_and(|p| p.meta.key.as_str() == key.as_str())
                {
                    survivors.push(r);
                    break;
                }
            }
        }
    }
    if !survivors.is_empty() {
        scan_files(adapters, store, events, survivors);
    }
}

/// 一批去抖后的事件落库。从监听线程里拆出来,测试可以不经文件系统通知、直接喂
/// 构造好的事件(通知的到达时序三平台各不相同,拿真通知测会很脆)。`roots` 是
/// (监听根, 实例下标),路径归属按最长匹配根(resolve_watch_agent)。
///
/// **事件种类不作数,按处理这一刻的现状裁决**:文件在就重解析,不在就出库。先 unlink
/// 再 rename 的原子替换(Craft Agents 为兼容 Windows 就这么存会话)会先报一条 Remove,
/// rename 的事件还可能落进下一批;改名走掉的旧路径报的又是改名而不是删除——按种类
/// 分派,前者让会话在列表里闪没、正在看的详情被踢掉,后者让库里的行一直指着不存在的
/// 路径,直到下次全量扫描
pub(crate) fn process_batch(
    adapters: &[Box<dyn AgentAdapter>],
    store: &Arc<Store>,
    events: &dyn ScanEvents,
    roots: &[(PathBuf, usize)],
    batch: Vec<notify::Result<notify::Event>>,
) {
    let mut present: HashMap<PathBuf, usize> = HashMap::new();
    let mut gone: HashSet<PathBuf> = HashSet::new();
    let mut snapshot_agents: HashSet<AgentId> = HashSet::new();
    let mut rescan = false;
    for ev in batch {
        let Ok(ev) = ev.inspect_err(|e| eprintln!("watcher: notify error: {e}")) else {
            continue;
        };
        // 后端丢过事件(FSEvents MustScanSubDirs / inotify Q_OVERFLOW):
        // 这条不指向具体会话文件——macOS 挂的是出事的目录、Linux 干脆
        // 无路径——按路径处理会被 file_ref 静默丢弃,改由整轮增量兜底
        if ev.need_rescan() {
            rescan = true;
            continue;
        }
        for path in ev.paths {
            let owner_ix = resolve_watch_agent(roots, &path);
            if let Some(adapter) = owner_ix.and_then(|ix| adapters.get(ix)) {
                if adapter.is_snapshot_event(&path) {
                    snapshot_agents.insert(adapter.agent());
                }
            }
            if path.exists() {
                gone.remove(&path);
                if let Some(ix) = owner_ix {
                    present.insert(path, ix);
                }
            } else {
                present.remove(&path);
                gone.insert(path);
            }
        }
    }

    let removed_keys: Vec<String> = gone
        .iter()
        .filter_map(|path| store.remove_by_path(&path.to_string_lossy()).ok().flatten())
        .collect();
    if !removed_keys.is_empty() {
        events.on_sessions_changed();
    }
    promote_survivors(adapters, store, events, &removed_keys);
    // 认领方的快照变了(会话存盘或删掉,隐藏会话与回合锚点也算):写库之前对账——同一批里
    // 引擎那份转录的写入才会被闸门挡住,原件没了的认领随之撤销
    if adapters
        .iter()
        .any(|a| a.manages_claims() && snapshot_agents.contains(&a.agent()))
    {
        refresh_claims(adapters, store, events);
    }

    // 路径是否本 agent 的会话文件、native_id 怎么取,统一问**拥有
    // 该根的实例**(下标即 roots 表登记的归属)
    let refs: Vec<SessionFileRef> = present
        .into_iter()
        .filter_map(|(path, ix)| adapters.get(ix).and_then(|a| a.file_ref(&path)))
        .collect();
    let scanned_agents: HashSet<AgentId> = refs.iter().map(|reference| reference.agent).collect();
    if !refs.is_empty() {
        scan_files(adapters, store, events, refs);
    }
    // scan_files 已经为它扫到的家刷过父子关系
    snapshot_agents.retain(|agent| !scanned_agents.contains(agent));
    if !snapshot_agents.is_empty() {
        let agents: Vec<AgentId> = snapshot_agents.into_iter().collect();
        refresh_parent_links(adapters, store, events, &agents);
    }
    if rescan {
        events.on_rescan_needed();
    }
}

pub fn start_watcher(
    adapters: Arc<Vec<Box<dyn AgentAdapter>>>,
    store: Arc<Store>,
    events: Arc<dyn ScanEvents>,
) -> Option<SessionWatcher> {
    // 根携带实例下标而非 AgentId:同 agent 多实例(自定义 location)时,
    // file_ref 也必须由拥有该根的实例执行——dsh 的 file_ref 要读文件首行,
    // 实例相对的判据按 agent 找第一个会拿默认实例跑偏
    let mut roots: Vec<(PathBuf, usize)> = Vec::new();
    for (ix, a) in adapters.iter().enumerate() {
        for p in a.watch_paths() {
            roots.push((p, ix));
        }
    }
    if roots.is_empty() {
        return None;
    }

    let (tx, rx) = mpsc::channel::<notify::Result<notify::Event>>();
    let mut watcher = notify::recommended_watcher(tx).ok()?;
    let mut watched = std::collections::BTreeSet::new();
    for (root, _) in &roots {
        // 失败(常见:目录还不存在,如未同步的远程缓存)跳过不算挂上
        if watcher.watch(root, RecursiveMode::Recursive).is_ok() {
            watched.insert(root.clone());
        }
    }

    let thread = std::thread::spawn(move || {
        loop {
            // 等首个事件(阻塞),然后 800ms 窗口收敛
            let first = match rx.recv() {
                Ok(e) => e,
                Err(_) => break, // watcher dropped
            };
            let mut batch = vec![first];
            let deadline = std::time::Instant::now() + Duration::from_millis(800);
            while let Ok(ev) =
                rx.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
            {
                batch.push(ev);
                if std::time::Instant::now() >= deadline {
                    break;
                }
            }
            process_batch(&adapters, &store, events.as_ref(), &roots, batch);
        }
    });

    Some(SessionWatcher {
        watcher: Some(watcher),
        thread: Some(thread),
        watched,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::craft::CraftAdapter;
    use crate::scanner::NullEvents;
    use notify::event::{CreateKind, EventKind, ModifyKind, RemoveKind, RenameMode};

    /// Craft 的会话首行(Claude 后端,引擎转录 id = `sdk`)+ 一句用户话
    fn header(name: &str, sdk: &str, hidden: bool) -> String {
        format!(
            "{{\"id\":\"s\",\"name\":\"{name}\",\"sdkSessionId\":\"{sdk}\",\"hidden\":{hidden},\"model\":\"claude-opus-5-5\",\"createdAt\":1786200000000}}\n\
             {{\"id\":\"m1\",\"type\":\"user\",\"content\":\"hello\",\"timestamp\":1786200001000}}\n"
        )
    }

    struct Fixture {
        _tmp: tempfile::TempDir,
        store: Arc<Store>,
        adapters: Vec<Box<dyn AgentAdapter>>,
        roots: Vec<(PathBuf, usize)>,
        root: PathBuf,
        file: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let root = tmp.path().join("workspaces");
            let fx = Self {
                store: Arc::new(Store::open(&tmp.path().join("wake.db")).unwrap()),
                adapters: vec![CraftAdapter::new().with_custom_root(root.clone())],
                roots: vec![(root.clone(), 0)],
                file: root.join("ws/sessions/260801-brave-otter/session.jsonl"),
                root,
                _tmp: tmp,
            };
            fx.write(&fx.file, &header("first", "sdk-1", false));
            fx
        }

        fn write(&self, path: &Path, content: &str) {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content).unwrap();
        }

        fn batch(&self, kind: EventKind, path: &Path) {
            let batch = vec![Ok(notify::Event::new(kind).add_path(path.to_path_buf()))];
            process_batch(&self.adapters, &self.store, &NullEvents, &self.roots, batch);
        }

        fn title(&self) -> Option<String> {
            self.store
                .get_session("craft-agents:ws/260801-brave-otter")
                .unwrap()
                .map(|s| s.title)
        }
    }

    /// Craft 存会话是"写 .tmp → 先 unlink 正本 → rename",rename 的事件可能落进下一批。
    /// 这一批只收到 Remove、而文件已经回来了:当修改处理(重解析),不删会话;
    /// 文件真没了才出库
    #[test]
    fn a_remove_for_a_file_that_is_back_is_a_modification() {
        let fx = Fixture::new();
        fx.batch(EventKind::Create(CreateKind::File), &fx.file);
        assert_eq!(fx.title().as_deref(), Some("first"));

        let tmp = fx.file.with_file_name("session.jsonl.tmp");
        fx.write(&tmp, &header("second", "sdk-1", false));
        std::fs::remove_file(&fx.file).unwrap();
        std::fs::rename(&tmp, &fx.file).unwrap();
        fx.batch(EventKind::Remove(RemoveKind::File), &fx.file);
        assert_eq!(
            fx.title().as_deref(),
            Some("second"),
            "一条过时的 Remove 把会话删了,或没按修改重解析"
        );

        std::fs::remove_file(&fx.file).unwrap();
        fx.batch(EventKind::Remove(RemoveKind::File), &fx.file);
        assert_eq!(fx.title(), None);
    }

    /// 改名走掉的旧路径报的是改名而不是删除:照样按"文件已不在"出库
    #[test]
    fn a_path_renamed_away_leaves_the_index() {
        let fx = Fixture::new();
        fx.batch(EventKind::Create(CreateKind::File), &fx.file);
        std::fs::rename(&fx.file, fx.file.with_file_name("session.jsonl.bak")).unwrap();
        fx.batch(
            EventKind::Modify(ModifyKind::Name(RenameMode::From)),
            &fx.file,
        );
        assert_eq!(fx.title(), None);
    }

    /// 认领方(Craft)的会话一存盘就认领它的引擎转录——隐藏的 mini 会话 file_ref 不收,
    /// 它的首行照样是快照事件;会话被删,认领随之撤销
    #[test]
    fn claimant_snapshot_events_keep_claims_current() {
        let fx = Fixture::new();
        fx.batch(EventKind::Create(CreateKind::File), &fx.file);
        let hidden = fx.root.join("ws/sessions/260802-mini-edit/session.jsonl");
        fx.write(&hidden, &header("mini", "sdk-2", true));
        fx.batch(EventKind::Create(CreateKind::File), &hidden);
        let claimed = fx.store.claimed_keys().unwrap();
        assert!(claimed.contains("claude-code:sdk-1"), "{claimed:?}");
        assert!(claimed.contains("claude-code:sdk-2"), "{claimed:?}");

        std::fs::remove_dir_all(fx.file.parent().unwrap()).unwrap();
        fx.batch(EventKind::Remove(RemoveKind::File), &fx.file);
        let claimed = fx.store.claimed_keys().unwrap();
        assert!(!claimed.contains("claude-code:sdk-1"), "{claimed:?}");
    }
}
