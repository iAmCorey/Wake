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
/// 下一次全量扫描兜底。每个实例只枚举一次(纯 stat):删掉一个会话目录时,它派出去的子代理
/// 各是一个被删的 key,逐个重列整家会把同一份清单列上百遍
pub fn promote_survivors(
    adapters: &[Box<dyn AgentAdapter>],
    store: &Arc<Store>,
    events: &dyn ScanEvents,
    removed_keys: &[String],
) {
    let agents: std::collections::HashSet<AgentId> = removed_keys
        .iter()
        .filter_map(|key| key.split(':').next().and_then(AgentId::from_str))
        .collect();
    let listings: Vec<Vec<SessionFileRef>> = adapters
        .iter()
        .map(|a| {
            if agents.contains(&a.agent()) {
                a.list_session_files().unwrap_or_default()
            } else {
                Vec::new()
            }
        })
        .collect();
    let mut seen = std::collections::HashSet::new();
    let mut survivors: Vec<SessionFileRef> = Vec::new();
    for key in removed_keys {
        if !seen.insert(key.as_str()) {
            continue;
        }
        let Some(agent) = key.split(':').next().and_then(AgentId::from_str) else {
            continue;
        };
        // host 段对不上的实例整个跳过(本地实例对远程 key、A 机对 B 机):
        // 名字与解析比对都不可能命中——实例出口的 key 恒带自己的 host
        let candidates: Vec<(&str, &[SessionFileRef])> = adapters
            .iter()
            .zip(&listings)
            .filter(|(a, _)| a.agent() == agent)
            .filter_map(|(a, refs)| Some((strip_key_prefix(key, agent, a.host())?, &refs[..])))
            .collect();
        let by_name: Vec<SessionFileRef> = candidates
            .iter()
            .flat_map(|(native_id, refs)| refs.iter().filter(move |r| r.native_id == *native_id))
            .cloned()
            .collect();
        if !by_name.is_empty() {
            survivors.extend(by_name);
            continue;
        }
        // key 后缀是**内容 id** 的家(gemini 的 sessionId、pi/dsh 的首行 id)
        // 文件名对不上——退而解析比对,规模封顶防重罚(超限交下次全量扫描)
        let agent_refs = candidates.iter().flat_map(|(_, refs)| refs.iter());
        if agent_refs.clone().count() <= 32 {
            for r in agent_refs {
                let Some(owner) = crate::adapters::adapter_for(adapters, r.agent, &r.file_path)
                else {
                    continue;
                };
                if owner
                    .parse_session(r)
                    .is_ok_and(|p| p.meta.key.as_str() == key.as_str())
                {
                    survivors.push(r.clone());
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

    // 归档跟着源走:这一批里还在的文件当场镜像进 vault——等下一次全量扫描的话,
    // 写完就被清掉的会话(或者它最后那几轮)就进不了归档
    if let Some((home, vault)) = crate::vault::active(store) {
        for path in present.keys() {
            crate::vault::mirror_path(&home, &vault, path);
        }
    }

    // 不是已入库会话文件的路径(新出现的会话文件,或者从不入库的共享边车——Grok Bot 的侧栏列表)
    // 与消失的路径,可能改变同实例别的文件的引用(`ref_dependents`:Claude 的分支数着同目录更早的
    // 同根文件,Grok Bot 的副本并进了列表行的指纹)。那些文件没动、没有事件,这里按引用与索引
    // 重比,变了的跟本批一起收编——Wake 里删原件、从废纸篓放回、Claude 自己清理旧会话,
    // 留下的分支都当场展开 / 折叠,不用等下一次全量扫描
    let vanished: Vec<(&PathBuf, usize)> = gone
        .iter()
        .filter_map(|path| resolve_watch_agent(roots, path).map(|ix| (path, ix)))
        .collect();
    let appeared = present
        .iter()
        .filter(|(path, _)| matches!(store.file_stamp(&path.to_string_lossy()), Ok(None)))
        .map(|(path, ix)| (path, *ix));
    let mut dependents: Vec<SessionFileRef> = appeared
        .chain(vanished.iter().copied())
        .flat_map(|(path, ix)| {
            let adapter = &adapters[ix];
            adapter
                .ref_dependents(path)
                .into_iter()
                .filter_map(|dependent| adapter.file_ref(&dependent))
        })
        .filter(|r| {
            store
                .file_stamp(&r.file_path)
                .is_ok_and(|stamp| stamp.is_some_and(|stamp| stamp != (r.mtime_ms, r.size)))
        })
        .collect();

    // 装着好几个会话的文件(`sessions_in`,Grok Bot 的侧栏列表):库里挂在它名下的虚拟路径按它
    // 此刻的内容对账——新出现的收编,没了的出库,引用变了的重解析
    let mut dropped: Vec<String> = Vec::new();
    let every_path = present.iter().map(|(path, ix)| (path, *ix));
    for (path, ix) in every_path.chain(vanished.iter().copied()) {
        let Some(listed) = adapters[ix].sessions_in(path) else {
            continue;
        };
        let (from, to) = crate::adapters::sqlite_ro::virtual_path_bounds(path);
        let Ok(indexed) = store.file_stamps_in(&from, &to) else {
            continue;
        };
        let still_there: HashSet<&str> = listed.iter().map(|r| r.file_path.as_str()).collect();
        dropped.extend(
            indexed
                .keys()
                .filter(|indexed| !still_there.contains(indexed.as_str()))
                .cloned(),
        );
        // 没入库的也交出去:同一会话已由更优的副本代表着(Grok Bot 有副本的对话)时 scan_files
        // 解析之前就跳过
        dependents.extend(
            listed
                .into_iter()
                .filter(|r| indexed.get(&r.file_path) != Some(&(r.mtime_ms, r.size))),
        );
    }

    let removed_keys: Vec<String> = gone
        .iter()
        .map(|path| path.to_string_lossy().into_owned())
        .chain(dropped)
        .filter_map(|path| store.remove_by_path(&path).ok().flatten())
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
    // 该根的实例**(下标即 roots 表登记的归属)。file_ref 可以把边车换成它所属的会话
    // 文件(见 trait 文档),边车与正文同批变了只解析一次
    let mut seen = HashSet::new();
    let refs: Vec<SessionFileRef> = present
        .into_iter()
        .filter_map(|(path, ix)| adapters.get(ix).and_then(|a| a.file_ref(&path)))
        .chain(dependents)
        .filter(|r| seen.insert(r.file_path.clone()))
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
    use crate::adapters::claude_desktop::write_test_record;
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

    /// 在 Claude 桌面端归档、取消归档、删掉桌面会话:只有桌面端那份记录变了,转录没动。
    /// watcher 听着记录所在的目录,经 `ref_dependents` 把名下的转录补进同一批,库里那行当场跟上
    #[test]
    fn claude_desktop_archive_reaches_the_index_live() {
        use crate::adapters::claude::ClaudeAdapter;
        let tmp = tempfile::tempdir().unwrap();
        let projects = tmp.path().join("projects");
        let dir = projects.join("-w");
        std::fs::create_dir_all(&dir).unwrap();
        let id = "c1a0de00-0000-4000-8000-0000000000c3";
        let row = serde_json::json!({
            "type": "user", "uuid": "u1", "isSidechain": false, "cwd": "/w", "sessionId": id,
            "timestamp": "2026-09-01T08:00:01.000Z",
            "message": { "role": "user", "content": "Why does login time out?" },
        });
        std::fs::write(dir.join(format!("{id}.jsonl")), row.to_string() + "\n").unwrap();

        let desktop = tmp.path().join("desktop");
        let write_record = |archived: bool| {
            let fields = serde_json::json!({ "cliSessionId": id, "isArchived": archived });
            write_test_record(&desktop, "d", fields)
        };
        let record = write_record(false);

        let store = Arc::new(Store::open(&tmp.path().join("wake.db")).unwrap());
        let adapters: Vec<Box<dyn AgentAdapter>> = vec![Box::new(ClaudeAdapter::with_desktop(
            projects,
            desktop.clone(),
        ))];
        let roots: Vec<(PathBuf, usize)> = adapters[0]
            .watch_paths()
            .into_iter()
            .map(|p| (p, 0))
            .collect();
        crate::scanner::run_scan(&adapters, &store, &NullEvents, true).unwrap();
        let key = format!("claude-code:{id}");
        let archived = || store.get_session(&key).unwrap().unwrap().archived;
        let event = |kind: EventKind| {
            let batch = vec![Ok(notify::Event::new(kind).add_path(record.clone()))];
            process_batch(&adapters, &store, &NullEvents, &roots, batch);
        };
        assert!(!archived());

        write_record(true);
        event(EventKind::Modify(ModifyKind::Any));
        assert!(archived(), "在桌面端归档");

        write_record(false);
        event(EventKind::Modify(ModifyKind::Any));
        assert!(!archived(), "取消归档");

        write_record(true);
        event(EventKind::Modify(ModifyKind::Any));
        std::fs::remove_file(&record).unwrap();
        event(EventKind::Remove(RemoveKind::File));
        assert!(!archived(), "桌面会话删掉了,转录还在");
    }

    /// Claude 桌面端从一个会话派出去的会话(侧栏里嵌在它下面):挂到派它的会话下面、跟着它
    /// 的项目;记录里的 spawnedFrom 没了(或派它的会话没了),当场解挂、回到自己的项目
    #[test]
    fn claude_desktop_side_sessions_nest_under_the_session_that_started_them() {
        use crate::adapters::claude::ClaudeAdapter;
        let tmp = tempfile::tempdir().unwrap();
        let projects = tmp.path().join("projects");
        let (parent, child) = (
            "c1a0de00-0000-4000-8000-0000000000d4",
            "c1a0de00-0000-4000-8000-0000000000e5",
        );
        for (id, dir, cwd) in [(parent, "-w", "/w"), (child, "-w-wt", "/w/wt")] {
            let dir = projects.join(dir);
            std::fs::create_dir_all(&dir).unwrap();
            let row = serde_json::json!({
                "type": "user", "uuid": format!("u-{id}"), "isSidechain": false, "cwd": cwd,
                "sessionId": id, "timestamp": "2026-09-01T08:00:01.000Z",
                "message": { "role": "user", "content": "Why does login time out?" },
            });
            std::fs::write(dir.join(format!("{id}.jsonl")), row.to_string() + "\n").unwrap();
        }

        let desktop = tmp.path().join("desktop");
        write_test_record(&desktop, "p", serde_json::json!({ "cliSessionId": parent }));
        let write_child = |spawned: bool| {
            let mut fields = serde_json::json!({ "cliSessionId": child });
            if spawned {
                fields["spawnedFrom"] =
                    serde_json::json!({ "sessionId": "local_p", "taskId": "t" });
            }
            write_test_record(&desktop, "c", fields)
        };
        let record = write_child(true);

        let store = Arc::new(Store::open(&tmp.path().join("wake.db")).unwrap());
        let adapters: Vec<Box<dyn AgentAdapter>> = vec![Box::new(ClaudeAdapter::with_desktop(
            projects,
            desktop.clone(),
        ))];
        let roots: Vec<(PathBuf, usize)> = adapters[0]
            .watch_paths()
            .into_iter()
            .map(|p| (p, 0))
            .collect();
        crate::scanner::run_scan(&adapters, &store, &NullEvents, true).unwrap();
        let (parent_key, child_key) = (
            format!("claude-code:{parent}"),
            format!("claude-code:{child}"),
        );
        let state = || {
            let project = store.get_session(&child_key).unwrap().unwrap().project_path;
            (store.parent_key_of(&child_key).unwrap(), project)
        };
        assert_eq!(state(), (Some(parent_key.clone()), "/w".to_string()));

        write_child(false);
        let batch = vec![Ok(
            notify::Event::new(EventKind::Modify(ModifyKind::Any)).add_path(record.clone())
        )];
        process_batch(&adapters, &store, &NullEvents, &roots, batch);
        assert_eq!(state(), (None, "/w/wt".to_string()));
    }

    /// Claude Code 的分支 B 开头原样复制了原件 A 的第一轮。A 在 Wake 之外消失(Claude 按
    /// cleanupPeriodDays 清掉、在 Finder 里删掉)或者被放回时,B 的文件没动、没有它的事件——
    /// watcher 经 `ref_dependents` 把它补进同一批:A 没了,B 展开完整历史、变回顶层;
    /// A 回来,B 重新折叠、挂回 A 下面
    #[test]
    fn claude_branches_follow_their_original_coming_and_going() {
        use crate::adapters::claude::ClaudeAdapter;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("projects");
        let dir = root.join("-w");
        std::fs::create_dir_all(&dir).unwrap();
        let (id_a, id_b) = (
            "c1a0de00-0000-4000-8000-0000000000a1",
            "c1a0de00-0000-4000-8000-0000000000b2",
        );
        let row = |session: &str, uuid: &str, ts: &str, message: serde_json::Value| {
            serde_json::json!({
                "parentUuid": null, "isSidechain": false, "cwd": "/w", "sessionId": session,
                "type": message["role"], "message": message, "uuid": uuid, "timestamp": ts,
            })
            .to_string()
        };
        let user = |text: &str| serde_json::json!({ "role": "user", "content": text });
        let file = |session: &str, opened: &str, own: &str| {
            let queued = serde_json::json!({
                "type": "queue-operation", "operation": "enqueue",
                "timestamp": opened, "sessionId": session,
            });
            let reply = serde_json::json!({
                "id": "msg_shared", "role": "assistant", "model": "claude-opus-5-5",
                "content": [{ "type": "text", "text": "The connection pool is exhausted." }],
            });
            [
                queued.to_string(),
                row(
                    session,
                    "u1",
                    "2026-09-01T08:00:01.000Z",
                    user("Why does login time out?"),
                ),
                row(session, "u2", "2026-09-01T08:00:05.000Z", reply),
                row(session, own, "2026-09-02T09:00:10.000Z", user(own)),
            ]
            .join("\n")
        };
        let a = dir.join(format!("{id_a}.jsonl"));
        let b = dir.join(format!("{id_b}.jsonl"));
        let original = file(id_a, "2026-09-01T08:00:00.000Z", "add exponential backoff");
        std::fs::write(&a, &original).unwrap();
        std::fs::write(
            &b,
            file(id_b, "2026-09-02T09:00:00.000Z", "move login to a queue"),
        )
        .unwrap();

        let store = Arc::new(Store::open(&tmp.path().join("wake.db")).unwrap());
        let adapters: Vec<Box<dyn AgentAdapter>> =
            vec![ClaudeAdapter::new().with_custom_root(root.clone())];
        let roots = vec![(root, 0)];
        crate::scanner::run_scan(&adapters, &store, &NullEvents, true).unwrap();
        let (key_a, key_b) = (format!("claude-code:{id_a}"), format!("claude-code:{id_b}"));
        let shared_turn = || -> Vec<String> {
            let (hits, _) = store.search("connection pool", &[], None, 10).unwrap();
            hits.into_iter().map(|h| h.session.key).collect()
        };
        let parent_of_b = || store.parent_key_of(&key_b).unwrap();
        let event = |kind: EventKind| {
            let batch = vec![Ok(notify::Event::new(kind).add_path(a.clone()))];
            process_batch(&adapters, &store, &NullEvents, &roots, batch);
        };
        assert_eq!(shared_turn(), vec![key_a.clone()]);
        assert_eq!(parent_of_b(), Some(key_a.clone()));

        std::fs::remove_file(&a).unwrap();
        event(EventKind::Remove(RemoveKind::File));
        assert_eq!(
            shared_turn(),
            vec![key_b.clone()],
            "共有的那一轮回到分支名下"
        );
        assert_eq!(parent_of_b(), None);

        std::fs::write(&a, &original).unwrap();
        event(EventKind::Create(CreateKind::File));
        assert_eq!(shared_turn(), vec![key_a.clone()]);
        assert_eq!(parent_of_b(), Some(key_a));
    }

    /// Kimi 在自己那边归档会话只写 state.json:子代理跟着归档。它们的转录没动、没有事件——
    /// state.json 的事件经 `ref_dependents` 把它们补进同一批,不用等下一次全量扫描
    #[test]
    fn kimi_subagents_follow_their_session_into_the_archive() {
        use crate::adapters::kimi::KimiAdapter;
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("kimi");
        let session = home.join("sessions").join("wd_x_1").join("session_w1");
        let agents = session.join("agents");
        let prompt = |text: &str, origin: serde_json::Value| {
            serde_json::json!({ "type": "turn.prompt", "time": 1791417601000i64,
                "input": [{ "type": "text", "text": text }], "origin": origin })
            .to_string()
                + "\n"
        };
        for (agent, line) in [
            (
                "main",
                prompt("Review the parser", serde_json::json!({ "kind": "user" })),
            ),
            (
                "agent-1",
                prompt(
                    "Look at src/parser.rs",
                    serde_json::json!({ "kind": "system_trigger", "name": "subagent" }),
                ),
            ),
        ] {
            std::fs::create_dir_all(agents.join(agent)).unwrap();
            std::fs::write(agents.join(agent).join("wire.jsonl"), line).unwrap();
        }
        let state = session.join("state.json");
        std::fs::write(&state, r#"{"title":"Review the parser"}"#).unwrap();

        let store = Arc::new(Store::open(&tmp.path().join("wake.db")).unwrap());
        let adapters: Vec<Box<dyn AgentAdapter>> = vec![KimiAdapter::new().with_custom_root(home)];
        let roots: Vec<(PathBuf, usize)> = adapters[0]
            .watch_paths()
            .into_iter()
            .map(|path| (path, 0))
            .collect();
        crate::scanner::run_scan(&adapters, &store, &NullEvents, true).unwrap();
        let archived = |key: &str| store.get_session(key).unwrap().unwrap().archived;
        assert!(!archived("kimi:session_w1/agent-1"));

        std::fs::write(&state, r#"{"title":"Review the parser","archived":true}"#).unwrap();
        let batch = vec![Ok(
            notify::Event::new(EventKind::Modify(ModifyKind::Any)).add_path(state.clone())
        )];
        process_batch(&adapters, &store, &NullEvents, &roots, batch);
        assert!(archived("kimi:session_w1"));
        assert!(archived("kimi:session_w1/agent-1"), "子代理没跟着会话归档");
    }

    #[test]
    fn dropping_watcher_joins_worker_and_releases_its_state() {
        let fx = Fixture::new();
        let adapters = Arc::new(fx.adapters);
        let watcher = start_watcher(adapters.clone(), fx.store.clone(), Arc::new(NullEvents))
            .expect("fixture must create a watcher");
        assert_eq!(
            watcher.watched_roots(),
            &std::collections::BTreeSet::from([fx.root])
        );

        // 在另一条线程 drop,让真正的死锁变成有界失败,而不是永远到不了耗时断言。
        let (tx, rx) = mpsc::channel();
        let dropper = std::thread::spawn(move || {
            drop(watcher);
            tx.send(()).unwrap();
        });
        rx.recv_timeout(Duration::from_secs(5))
            .expect("watcher drop must stop and join its worker");
        dropper.join().unwrap();
        assert_eq!(Arc::strong_count(&adapters), 1);
        assert_eq!(Arc::strong_count(&fx.store), 1);
    }

    /// 只装着 Grok Bot 一家的存储目录(一个或几个 location)与索引;文件都在账号 github|1 名下,
    /// 不点名 location 的都写在第一个里
    struct GrokBotFixture {
        _tmp: tempfile::TempDir,
        locations: Vec<PathBuf>,
        store: Arc<Store>,
        adapters: Vec<Box<dyn AgentAdapter>>,
        roots: Vec<(PathBuf, usize)>,
    }

    impl GrokBotFixture {
        fn new() -> Self {
            Self::with_locations(1)
        }

        fn with_locations(count: usize) -> Self {
            use crate::adapters::grok_bot::GrokBotAdapter;
            let tmp = tempfile::tempdir().unwrap();
            let locations: Vec<PathBuf> = (0..count)
                .map(|ix| {
                    let root = tmp.path().join(format!("location-{ix}"));
                    let root = root.join("sand-client-persistence");
                    std::fs::create_dir_all(&root).unwrap();
                    root
                })
                .collect();
            Self {
                store: Arc::new(Store::open(&tmp.path().join("wake.db")).unwrap()),
                adapters: locations
                    .iter()
                    .map(|root| GrokBotAdapter::new().with_custom_root(root.clone()))
                    .collect(),
                roots: locations.iter().cloned().zip(0..).collect(),
                locations,
                _tmp: tmp,
            }
        }

        fn file_in(&self, location: usize, slice: &str) -> PathBuf {
            let key = format!("sand.client.slice.account.github%7C1.{slice}");
            self.locations[location].join(crate::adapters::grok_bot::store_file_name(&key).unwrap())
        }

        fn roster_in(&self, location: usize) -> PathBuf {
            self.file_in(location, "roster.last-roster")
        }

        fn roster(&self) -> PathBuf {
            self.roster_in(0)
        }

        fn write_roster_in(&self, location: usize, rows: &[String]) {
            std::fs::write(self.roster_in(location), roster_json(rows)).unwrap();
        }

        fn write_roster(&self, rows: &[String]) {
            self.write_roster_in(0, rows);
        }

        /// 一条副本,条目原样给
        fn write_replica(&self, id: &str, entries: &str) -> PathBuf {
            let path = self.file_in(0, &format!("transcript.replicas.{id}"));
            let json = format!(
                r#"{{"schemaVersion":1,"value":{{"entries":[{entries}],"persistedAt":1786000000000}}}}"#
            );
            std::fs::write(&path, json).unwrap();
            path
        }

        fn event(&self, paths: &[&Path]) {
            let event = paths.iter().fold(
                notify::Event::new(EventKind::Modify(ModifyKind::Any)),
                |event, path| event.add_path(path.to_path_buf()),
            );
            process_batch(
                &self.adapters,
                &self.store,
                &NullEvents,
                &self.roots,
                vec![Ok(event)],
            );
        }

        fn session(&self, id: &str) -> Option<SessionMeta> {
            self.store.get_session(&format!("grok-bot:{id}")).unwrap()
        }

        fn title(&self, id: &str) -> Option<String> {
            self.session(id).map(|session| session.title)
        }

        fn finds(&self, word: &str) -> bool {
            let (hits, _) = self.store.search(word, &[], None, 10).unwrap();
            !hits.is_empty()
        }
    }

    fn roster_json(rows: &[String]) -> String {
        let rows = rows.join(",");
        format!(r#"{{"schemaVersion":4,"value":{{"rows":[{rows}]}}}}"#)
    }

    fn grok_bot_row_at(id: &str, name: &str, description: &str, activity_ms: i64) -> String {
        format!(
            r#"{{"id":"{id}","name":"{name}","description":"{description}","createdAt":1786000000000,"lastActivityAt":{activity_ms}}}"#
        )
    }

    fn grok_bot_row(id: &str, name: &str, description: &str) -> String {
        grok_bot_row_at(id, name, description, 1786000060000)
    }

    const HELLO: &str = r#"{"kind":"message","id":"m1","seq":1,"timestampMs":1786000000000,"role":"user","content":"hello"}"#;

    /// Grok Bot 在侧栏里改名:只有列表文件有事件,副本文件没动。列表文件的事件经 `ref_dependents`
    /// 把同账号的副本带进同一批重比,标题当场跟上,不用等下一次全量扫描
    #[test]
    fn grok_bot_roster_changes_reach_untouched_replicas() {
        let fx = GrokBotFixture::new();
        fx.write_roster(&[grok_bot_row("c1", "Ada", "")]);
        let replica = fx.write_replica("c1", HELLO);
        fx.event(&[&replica]);
        assert_eq!(fx.title("c1").as_deref(), Some("Ada"));
        fx.write_roster(&[grok_bot_row("c1", "Ada Lovelace", "")]);
        fx.event(&[&fx.roster()]);
        assert_eq!(fx.title("c1").as_deref(), Some("Ada Lovelace"));
    }

    /// 列表里新建的 bot、删掉的 bot,卡片当场跟上(`sessions_in`);列表文件整个没了,卡片一张不剩,
    /// 有副本的对话照列、标题退回第一句话
    #[test]
    fn grok_bot_cards_follow_the_roster() {
        let fx = GrokBotFixture::new();
        let replica = fx.write_replica("c1", HELLO);
        fx.write_roster(&[
            grok_bot_row("c1", "Ada", ""),
            grok_bot_row("c2", "Scout", ""),
        ]);
        fx.event(&[&fx.roster(), &replica]);
        let replica_path = replica.to_string_lossy().into_owned();
        assert_eq!(fx.session("c1").unwrap().file_path, replica_path);
        assert_eq!(fx.title("c2").as_deref(), Some("Scout"));

        fx.write_roster(&[
            grok_bot_row("c1", "Ada", ""),
            grok_bot_row("c3", "Scribe", ""),
        ]);
        fx.event(&[&fx.roster()]);
        assert!(fx.session("c2").is_none(), "列表里删掉的 bot 不该还挂着");
        assert_eq!(fx.title("c3").as_deref(), Some("Scribe"));
        assert_eq!(fx.session("c1").unwrap().file_path, replica_path);

        std::fs::remove_file(fx.roster()).unwrap();
        fx.event(&[&fx.roster()]);
        assert!(fx.session("c3").is_none());
        assert_eq!(fx.title("c1").as_deref(), Some("hello"));
    }

    /// 副本只剩事件、退成卡片挂在副本自己的路径上:卡片才用的描述变了,副本没动也要重解析
    #[test]
    fn grok_bot_fallback_card_follows_the_roster_row() {
        let fx = GrokBotFixture::new();
        let replica = fx.write_replica(
            "c1",
            r#"{"kind":"event","id":"e1","seq":1,"timestampMs":1786000000000,"event":{"type":"name-changed","from":"a","to":"b"}}"#,
        );
        fx.write_roster(&[grok_bot_row("c1", "Ada", "Writes limericks")]);
        fx.event(&[&fx.roster(), &replica]);
        assert_eq!(
            fx.session("c1").unwrap().file_path,
            replica.to_string_lossy()
        );
        assert!(fx.finds("limericks"));
        fx.write_roster(&[grok_bot_row("c1", "Ada", "Writes sonnets")]);
        fx.event(&[&fx.roster()]);
        assert!(fx.finds("sonnets"));
        assert!(!fx.finds("limericks"));
    }

    /// 账号太长、键编码后超过 240 字符的列表存成摘要名的 `.kblob`,键在首行:文件被删之后读不到
    /// 首行,也得认出它是哪个账号的列表、把卡片撤掉
    #[test]
    fn grok_bot_framed_roster_deletion_drops_its_cards() {
        let fx = GrokBotFixture::new();
        let account = "x".repeat(150);
        let key = format!("sand.client.slice.account.{account}.roster.last-roster");
        assert!(crate::adapters::grok_bot::store_file_name(&key).is_none());
        let roster = fx.locations[0].join("0123456789abcdef.kblob");
        let framed = format!(
            "{}\n{}",
            serde_json::to_string(&key).unwrap(),
            roster_json(&[grok_bot_row("c9", "Scout", "")])
        );
        std::fs::write(&roster, framed).unwrap();
        fx.event(&[&roster]);
        assert_eq!(fx.title("c9").as_deref(), Some("Scout"));
        std::fs::remove_file(&roster).unwrap();
        fx.event(&[&roster]);
        assert!(fx.session("c9").is_none(), "列表没了,卡片不该还挂着");
    }

    /// 同一对话在两个 location 里都只有卡片:位次相同,按 mtime(最后活动时间)定胜负。落选那份
    /// 后来更新了,要当场接替,不用等全量扫描
    #[test]
    fn grok_bot_newer_card_from_another_location_takes_over() {
        let fx = GrokBotFixture::with_locations(2);
        fx.write_roster_in(0, &[grok_bot_row_at("c1", "Ada", "", 1786000060000)]);
        fx.write_roster_in(1, &[grok_bot_row_at("c1", "Ada", "", 1786000090000)]);
        fx.event(&[&fx.roster_in(0), &fx.roster_in(1)]);
        let winner = fx.session("c1").unwrap().file_path;
        assert!(winner.starts_with(&*fx.locations[1].to_string_lossy()));

        fx.write_roster_in(
            0,
            &[grok_bot_row_at("c1", "Ada Lovelace", "", 1786000120000)],
        );
        fx.event(&[&fx.roster_in(0)]);
        let session = fx.session("c1").unwrap();
        assert_eq!(session.title, "Ada Lovelace");
        assert!(session
            .file_path
            .starts_with(&*fx.locations[0].to_string_lossy()));
    }
}
