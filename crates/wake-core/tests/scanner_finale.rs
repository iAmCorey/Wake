//! 扫描终态契约(CLAUDE.md 不变量 6):run_scan 无论正常结束、Err 提前返回
//! 还是 adapter 出错,都必须发出一次 scanning=false 的终态进度事件——
//! UI 的模态刷新弹窗只认这个事件收场,收不到就永久锁死。

use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{bail, Result};
use wake_core::adapters::AgentAdapter;
use wake_core::db::Store;
use wake_core::models::*;
use wake_core::scanner::{refresh_parent_links, run_scan, ScanEvents, ScanProgress};

/// 收集全部进度事件与变更通知计数,供断言终态/刷新契约
struct Recorder(Mutex<Vec<ScanProgress>>, Mutex<usize>);

impl Recorder {
    fn new() -> Self {
        Recorder(Mutex::new(Vec::new()), Mutex::new(0))
    }
    fn changed(&self) -> usize {
        *self.1.lock().unwrap()
    }
}

impl ScanEvents for Recorder {
    fn on_progress(&self, p: &ScanProgress) {
        self.0.lock().unwrap().push(p.clone());
    }
    fn on_sessions_changed(&self) {
        *self.1.lock().unwrap() += 1;
    }
}

/// 枚举文件即失败的 adapter,模拟数据源不可读
struct FailingAdapter;

impl AgentAdapter for FailingAdapter {
    fn agent(&self) -> AgentId {
        AgentId::ClaudeCode
    }
    fn list_session_files(&self) -> Result<Vec<SessionFileRef>> {
        bail!("simulated data source failure")
    }
    fn parse_session(&self, _: &SessionFileRef) -> Result<ParsedSession> {
        bail!("unreachable")
    }
    fn parse_transcript(&self, _: &SessionFileRef) -> Result<ParsedTranscript> {
        bail!("unreachable")
    }
    fn data_roots(&self) -> Vec<std::path::PathBuf> {
        vec![std::path::PathBuf::from("/nonexistent/test-adapter")]
    }
    fn with_custom_root(&self, _: std::path::PathBuf) -> Box<dyn AgentAdapter> {
        Box::new(FailingAdapter)
    }
}

fn assert_terminal_event(events: &[ScanProgress], ctx: &str) {
    let last = events
        .last()
        .unwrap_or_else(|| panic!("{ctx}: 无任何进度事件"));
    assert!(
        !last.scanning,
        "{ctx}: 最后一个事件 scanning 仍为 true,UI 刷新弹窗将永久锁死"
    );
}

fn temp_store(dir: &Path) -> Arc<Store> {
    Arc::new(Store::open(&dir.join("scan.db")).expect("open store"))
}

#[test]
fn finale_fires_on_empty_scan() {
    let dir = tempfile::tempdir().unwrap();
    let store = temp_store(dir.path());
    let adapters: Vec<Box<dyn AgentAdapter>> = Vec::new();
    let rec = Recorder::new();

    let _ = run_scan(&adapters, &store, &rec, false);
    assert_terminal_event(&rec.0.lock().unwrap(), "空 adapter 列表");
}

#[test]
fn finale_fires_when_adapter_fails() {
    let dir = tempfile::tempdir().unwrap();
    let store = temp_store(dir.path());
    let adapters: Vec<Box<dyn AgentAdapter>> = vec![Box::new(FailingAdapter)];
    let rec = Recorder::new();

    // list_session_files 报错的路径:无论 run_scan 返回 Ok/Err,终态事件必须送达
    let _ = run_scan(&adapters, &store, &rec, false);
    assert_terminal_event(&rec.0.lock().unwrap(), "adapter 枚举失败");
}

#[test]
fn parent_links_from_multiple_locations_are_merged_by_winning_file() {
    let dir = tempfile::tempdir().unwrap();
    let store = temp_store(dir.path());
    let mut child = seed(
        AgentId::Grok,
        "/tmp/grok-child-location",
        "/tmp/grok-child-location/group/child/updates.jsonl",
        "child",
        20,
    );
    child.meta.project_path = "/tmp/wt-plan-pr-1".into();
    child.meta.project_name = "wt-plan-pr-1".into();
    child.manages_links = true;

    let mut parent = seed(
        AgentId::Grok,
        "/tmp/grok-parent-location",
        "/tmp/grok-parent-location/group/parent/updates.jsonl",
        "parent",
        10,
    );
    parent.meta.project_path = "/Users/tester/Github/source".into();
    parent.meta.project_name = "source".into();
    // Grok 的 subagents/meta.json 在 parent 会话目录，所以关系快照属于
    // parent location；child 的胜出 updates.jsonl 则来自另一 location。
    parent.manages_links = true;
    parent.parent_links = vec![("grok:child".into(), "grok:parent".into())];

    let adapters: Vec<Box<dyn AgentAdapter>> = vec![Box::new(child), Box::new(parent)];
    run_scan(&adapters, &store, &Recorder::new(), true).unwrap();

    assert_eq!(
        store.parent_key_of("grok:child").unwrap(),
        Some("grok:parent".into())
    );
    assert_eq!(
        store
            .get_session("grok:child")
            .unwrap()
            .unwrap()
            .project_path,
        "/Users/tester/Github/source"
    );
}

#[test]
fn nested_parent_chain_flattens_across_three_locations() {
    let dir = tempfile::tempdir().unwrap();
    let store = temp_store(dir.path());
    let mut leaf = seed(
        AgentId::Grok,
        "/tmp/grok-leaf-location",
        "/tmp/grok-leaf-location/group/leaf/updates.jsonl",
        "leaf",
        30,
    );
    leaf.manages_links = true;
    let mut middle = seed(
        AgentId::Grok,
        "/tmp/grok-middle-location",
        "/tmp/grok-middle-location/group/middle/updates.jsonl",
        "middle",
        20,
    );
    middle.manages_links = true;
    middle.parent_links = vec![("grok:leaf".into(), "grok:middle".into())];
    let mut root = seed(
        AgentId::Grok,
        "/tmp/grok-root-location",
        "/tmp/grok-root-location/group/root/updates.jsonl",
        "root",
        10,
    );
    root.meta.project_path = "/Users/tester/Github/source".into();
    root.meta.project_name = "source".into();
    root.manages_links = true;
    root.parent_links = vec![("grok:middle".into(), "grok:root".into())];

    let adapters: Vec<Box<dyn AgentAdapter>> =
        vec![Box::new(leaf), Box::new(middle), Box::new(root)];
    run_scan(&adapters, &store, &Recorder::new(), true).unwrap();

    assert_eq!(
        store.parent_key_of("grok:middle").unwrap(),
        Some("grok:root".into())
    );
    assert_eq!(
        store.parent_key_of("grok:leaf").unwrap(),
        Some("grok:root".into())
    );
    assert_eq!(
        store
            .get_session("grok:leaf")
            .unwrap()
            .unwrap()
            .project_path,
        "/Users/tester/Github/source"
    );
}

#[test]
fn removing_parent_link_restores_child_project_on_sidecar_event() {
    let dir = tempfile::tempdir().unwrap();
    let store = temp_store(dir.path());
    let mut child = seed(
        AgentId::Grok,
        "/tmp/grok-child-location",
        "/tmp/grok-child-location/group/child/updates.jsonl",
        "child",
        20,
    );
    child.meta.project_path = "/tmp/wt-plan-pr-1".into();
    child.meta.project_name = "wt-plan-pr-1".into();
    child.manages_links = true;
    let mut parent = seed(
        AgentId::Grok,
        "/tmp/grok-parent-location",
        "/tmp/grok-parent-location/group/parent/updates.jsonl",
        "parent",
        10,
    );
    parent.meta.project_path = "/Users/tester/Github/source".into();
    parent.meta.project_name = "source".into();
    parent.manages_links = true;
    parent.parent_links = vec![("grok:child".into(), "grok:parent".into())];

    let adapters: Vec<Box<dyn AgentAdapter>> = vec![Box::new(child), Box::new(parent)];
    run_scan(&adapters, &store, &Recorder::new(), true).unwrap();
    assert_eq!(
        store
            .get_session("grok:child")
            .unwrap()
            .unwrap()
            .project_path,
        "/Users/tester/Github/source"
    );

    let mut child = seed(
        AgentId::Grok,
        "/tmp/grok-child-location",
        "/tmp/grok-child-location/group/child/updates.jsonl",
        "child",
        20,
    );
    child.meta.project_path = "/tmp/wt-plan-pr-1".into();
    child.meta.project_name = "wt-plan-pr-1".into();
    child.manages_links = true;
    let mut parent = seed(
        AgentId::Grok,
        "/tmp/grok-parent-location",
        "/tmp/grok-parent-location/group/parent/updates.jsonl",
        "parent",
        10,
    );
    parent.meta.project_path = "/Users/tester/Github/source".into();
    parent.meta.project_name = "source".into();
    parent.manages_links = true;
    let adapters: Vec<Box<dyn AgentAdapter>> = vec![Box::new(child), Box::new(parent)];

    refresh_parent_links(&adapters, &store, &Recorder::new(), &[AgentId::Grok]);

    let child = store.get_session("grok:child").unwrap().unwrap();
    assert_eq!(store.parent_key_of(&child.key).unwrap(), None);
    assert_eq!(child.project_path, "/tmp/wt-plan-pr-1");
    assert_eq!(child.project_name, "wt-plan-pr-1");
}

#[test]
fn migration_backfill_reparses_unchanged_grok_rows_once() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("scan.db");
    let store = Arc::new(Store::open(&db_path).unwrap());
    let mut adapter = seed(
        AgentId::Grok,
        "/tmp/grok-backfill",
        "/tmp/grok-backfill/group/session/updates.jsonl",
        "session",
        42,
    );
    adapter.meta.project_path = "/Users/tester/Github/source".into();
    adapter.meta.project_name = "source".into();
    let mut stale = adapter.meta.clone();
    stale.project_path = "/tmp/wt-plan-pr-1".into();
    stale.project_name = "wt-plan-pr-1".into();
    store.write_meta_only(&[(stale, 42)]).unwrap();
    rusqlite::Connection::open(&db_path)
        .unwrap()
        .execute(
            "INSERT OR REPLACE INTO schema_meta(key, value) VALUES ('grok_parent_backfill', '1')",
            [],
        )
        .unwrap();

    run_scan(
        &[Box::new(adapter) as Box<dyn AgentAdapter>],
        &store,
        &Recorder::new(),
        false,
    )
    .unwrap();

    assert_eq!(
        store
            .get_session("grok:session")
            .unwrap()
            .unwrap()
            .project_path,
        "/Users/tester/Github/source"
    );
    assert!(!store.needs_grok_parent_backfill());
}

#[test]
fn migration_backfill_retries_after_parse_failure() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("retry.db");
    let store = Arc::new(Store::open(&db_path).unwrap());
    let mut broken = seed(
        AgentId::Grok,
        "/tmp/grok-backfill-retry",
        "/tmp/grok-backfill-retry/group/session/updates.jsonl",
        "session",
        42,
    );
    broken.meta.project_path = "/tmp/wt-plan-pr-1".into();
    broken.meta.project_name = "wt-plan-pr-1".into();
    broken.fail_parse = true;
    store
        .write_meta_only(&[(broken.meta.clone(), broken.r.mtime_ms)])
        .unwrap();
    rusqlite::Connection::open(&db_path)
        .unwrap()
        .execute(
            "INSERT OR REPLACE INTO schema_meta(key, value) VALUES ('grok_parent_backfill', '1')",
            [],
        )
        .unwrap();

    run_scan(
        &[Box::new(broken) as Box<dyn AgentAdapter>],
        &store,
        &Recorder::new(),
        false,
    )
    .unwrap();
    assert!(
        store.needs_grok_parent_backfill(),
        "failed forced rows must remain retryable"
    );

    let mut repaired = seed(
        AgentId::Grok,
        "/tmp/grok-backfill-retry",
        "/tmp/grok-backfill-retry/group/session/updates.jsonl",
        "session",
        42,
    );
    repaired.meta.project_path = "/Users/tester/Github/source".into();
    repaired.meta.project_name = "source".into();
    run_scan(
        &[Box::new(repaired) as Box<dyn AgentAdapter>],
        &store,
        &Recorder::new(),
        false,
    )
    .unwrap();

    assert_eq!(
        store
            .get_session("grok:session")
            .unwrap()
            .unwrap()
            .project_path,
        "/Users/tester/Github/source"
    );
    assert!(!store.needs_grok_parent_backfill());
}

/// 固定提供一个会话(含 quickMeta 快路径)的 adapter。agent/root 可参数化:
/// 防复活、跨根去重、重叠根归属三组测试共用
struct SeedAdapter {
    agent: AgentId,
    root: std::path::PathBuf,
    r: SessionFileRef,
    meta: SessionMeta,
    /// 模拟截断/损坏副本:解析一律报错(副本回退测试用)
    fail_parse: bool,
    /// 模拟 codex 的 state 改名:quick 给出的 key 与文件 native key 不同,
    /// merge 时 quick key 压过 parsed(codex 同款优先级)
    quick_key: Option<String>,
    manages_links: bool,
    parent_links: Vec<(String, String)>,
}

impl AgentAdapter for SeedAdapter {
    fn agent(&self) -> AgentId {
        self.agent
    }
    fn list_session_files(&self) -> Result<Vec<SessionFileRef>> {
        Ok(vec![self.r.clone()])
    }
    fn file_ref(&self, path: &Path) -> Option<SessionFileRef> {
        (path.to_string_lossy() == self.r.file_path).then(|| self.r.clone())
    }
    fn quick_meta(
        &self,
        _refs: &[SessionFileRef],
    ) -> Option<std::collections::HashMap<String, SessionMeta>> {
        let mut meta = self.meta.clone();
        if let Some(k) = &self.quick_key {
            meta.key = k.clone();
        }
        let mut m = std::collections::HashMap::new();
        m.insert(self.r.file_path.clone(), meta);
        Some(m)
    }
    fn merge_quick_meta(&self, mut parsed: SessionMeta, quick: &SessionMeta) -> SessionMeta {
        if self.quick_key.is_some() {
            parsed.key = quick.key.clone();
        }
        parsed
    }
    fn manages_parent_links(&self) -> bool {
        self.manages_links
    }
    fn parent_links(&self) -> Vec<(String, String)> {
        self.parent_links.clone()
    }
    fn parse_session(&self, _: &SessionFileRef) -> Result<ParsedSession> {
        if self.fail_parse {
            bail!("simulated corrupt copy")
        }
        Ok(ParsedSession {
            meta: self.meta.clone(),
            units: Vec::new(),
            unknown_line_count: 0,
        })
    }
    fn parse_transcript(&self, _: &SessionFileRef) -> Result<ParsedTranscript> {
        bail!("transcript not needed in scan")
    }
    fn data_roots(&self) -> Vec<std::path::PathBuf> {
        vec![self.root.clone()]
    }
    fn with_custom_root(&self, _: std::path::PathBuf) -> Box<dyn AgentAdapter> {
        Box::new(SeedAdapter {
            agent: self.agent,
            root: self.root.clone(),
            r: self.r.clone(),
            meta: self.meta.clone(),
            fail_parse: self.fail_parse,
            quick_key: self.quick_key.clone(),
            manages_links: self.manages_links,
            parent_links: self.parent_links.clone(),
        })
    }
}

/// SeedAdapter 三件套:根/路径/mtime 齐配的最小会话
fn seed(agent: AgentId, root: &str, path: &str, native_id: &str, mtime: i64) -> SeedAdapter {
    SeedAdapter {
        agent,
        root: std::path::PathBuf::from(root),
        r: SessionFileRef {
            agent,
            native_id: native_id.into(),
            file_path: path.into(),
            mtime_ms: mtime,
            size: 1,
        },
        meta: SessionMeta {
            key: format!("{}:{native_id}", agent.as_str()),
            id: native_id.into(),
            agent,
            title: "seed".into(),
            project_path: "/tmp/p".into(),
            project_name: "p".into(),
            file_path: path.into(),
            created_at: 1,
            updated_at: mtime,
            message_count: 0,
            size_bytes: 1,
            git_branch: None,
            model: None,
            tokens_used: None,
            archived: false,
            source: None,
            favorite: false,
            pinned: false,
        },
        fail_parse: false,
        quick_key: None,
        manages_links: false,
        parent_links: Vec::new(),
    }
}

/// 不变量 3 端到端:删除(trash+tombstone)后,数据源仍枚举同一文件的
/// 下一次全量扫描不得让会话复活
#[test]
fn tombstoned_session_does_not_resurrect_on_rescan() {
    let dir = tempfile::tempdir().unwrap();
    let store = temp_store(dir.path());
    let meta = SessionMeta {
        key: "codex:ghost".into(),
        id: "ghost".into(),
        agent: AgentId::Codex,
        title: "残留会话".into(),
        project_path: "/tmp/p".into(),
        project_name: "p".into(),
        file_path: "/tmp/fixtures/ghost.jsonl".into(),
        created_at: 1,
        updated_at: 2,
        message_count: 0,
        size_bytes: 1,
        git_branch: None,
        model: None,
        tokens_used: None,
        archived: false,
        source: None,
        favorite: false,
        pinned: false,
    };
    let r = SessionFileRef {
        agent: AgentId::Codex,
        native_id: "ghost".into(),
        file_path: meta.file_path.clone(),
        mtime_ms: 2,
        size: 1,
    };
    let adapters: Vec<Box<dyn AgentAdapter>> = vec![Box::new(SeedAdapter {
        agent: AgentId::Codex,
        root: std::path::PathBuf::from("/tmp/fixtures"),
        r,
        meta: meta.clone(),
        fail_parse: false,
        quick_key: None,
        manages_links: false,
        parent_links: Vec::new(),
    })];
    let rec = Recorder::new();

    run_scan(&adapters, &store, &rec, true).unwrap();
    assert!(
        store.get_session("codex:ghost").unwrap().is_some(),
        "首扫应写入"
    );

    store.remove_session("codex:ghost", true).unwrap();
    run_scan(&adapters, &store, &rec, true).unwrap();
    assert!(
        store.get_session("codex:ghost").unwrap().is_none(),
        "tombstoned 会话重扫后复活 = 不变量 3 破坏"
    );
}

/// 同 agent 双根下的同 ID 会话:mtime 新者胜且跨轮稳定。旧行为是"后写者胜",
/// 两个副本每轮轮流改写同一行、file_path 随扫描摇摆(2026-08-24 Codex review)
/// ——故意把旧副本排在 roster 后面,旧代码在此必败
#[test]
fn duplicate_session_across_roots_resolves_to_newest() {
    let dir = tempfile::tempdir().unwrap();
    let store = temp_store(dir.path());
    let adapters: Vec<Box<dyn AgentAdapter>> = vec![
        Box::new(seed(AgentId::Codex, "/live", "/live/dup.jsonl", "dup", 9)),
        Box::new(seed(
            AgentId::Codex,
            "/backup",
            "/backup/dup.jsonl",
            "dup",
            5,
        )),
    ];
    let rec = Recorder::new();
    for round in 0..2 {
        run_scan(&adapters, &store, &rec, true).unwrap();
        let s = store.get_session("codex:dup").unwrap().expect("会话应在库");
        assert_eq!(
            s.file_path, "/live/dup.jsonl",
            "第 {round} 轮后 file_path 未稳定在 mtime 新者上"
        );
    }
}

/// 跨 agent 重叠根:文件只归**最长根**的实例(与 watcher 分派同一语义)。
/// 旧行为两家轮流认领——先写的一家留下错误归属,后写的撞 file_path UNIQUE
#[test]
fn overlapping_roots_assign_file_to_deepest_instance() {
    let dir = tempfile::tempdir().unwrap();
    let store = temp_store(dir.path());
    let adapters: Vec<Box<dyn AgentAdapter>> = vec![
        Box::new(seed(AgentId::ClaudeCode, "/x", "/x/inner/f.jsonl", "f", 7)),
        Box::new(seed(AgentId::Codex, "/x/inner", "/x/inner/f.jsonl", "f", 7)),
    ];
    let rec = Recorder::new();
    run_scan(&adapters, &store, &rec, true).unwrap();
    assert!(
        store.get_session("codex:f").unwrap().is_some(),
        "最长根(/x/inner)的实例应拥有该文件"
    );
    assert!(
        store.get_session("claude-code:f").unwrap().is_none(),
        "外层根实例不得认领别家树内的文件"
    );
}

/// watcher 增量同样受副本裁决约束:败方副本(旧 mtime)的文件事件不得覆盖
/// 已选中的胜者行——rsync 刷新备份目录会带旧 mtime 触发一串事件
/// (2026-08-24 Codex review P1)
#[test]
fn incremental_write_respects_duplicate_winner() {
    let dir = tempfile::tempdir().unwrap();
    let store = temp_store(dir.path());
    let live = seed(AgentId::Codex, "/live", "/live/dup.jsonl", "dup", 9);
    let backup = seed(AgentId::Codex, "/backup", "/backup/dup.jsonl", "dup", 5);
    let backup_ref = backup.r.clone();
    let newer_ref = SessionFileRef {
        mtime_ms: 12,
        ..backup.r.clone()
    };
    let mut newer_backup = seed(AgentId::Codex, "/backup", "/backup/dup.jsonl", "dup", 12);
    newer_backup.r = newer_ref.clone();
    let adapters: Vec<Box<dyn AgentAdapter>> = vec![Box::new(live), Box::new(backup)];
    let rec = Recorder::new();
    run_scan(&adapters, &store, &rec, true).unwrap();
    assert_eq!(
        store.get_session("codex:dup").unwrap().unwrap().file_path,
        "/live/dup.jsonl"
    );

    // 旧副本来了个文件事件:必须被裁决挡下
    wake_core::scanner::scan_files(&adapters, &store, &rec, vec![backup_ref]);
    assert_eq!(
        store.get_session("codex:dup").unwrap().unwrap().file_path,
        "/live/dup.jsonl",
        "败方副本的增量事件覆盖了胜者行"
    );

    // 备份真被更新(mtime 反超):按同款规则易主
    let adapters2: Vec<Box<dyn AgentAdapter>> = vec![Box::new(newer_backup)];
    wake_core::scanner::scan_files(&adapters2, &store, &rec, vec![newer_ref]);
    assert_eq!(
        store.get_session("codex:dup").unwrap().unwrap().file_path,
        "/backup/dup.jsonl",
        "mtime 反超的副本应按规则接管"
    );
}

/// 纯删除轮也要发变更通知:location 移除后的补扫常常解析队列为空,只有
/// 删除检测在干活——不通知,列表会一直挂着已删会话(2026-08-24 Codex review P1)
#[test]
fn pure_deletion_scan_notifies_sessions_changed() {
    let dir = tempfile::tempdir().unwrap();
    let store = temp_store(dir.path());
    let adapters: Vec<Box<dyn AgentAdapter>> = vec![Box::new(seed(
        AgentId::Codex,
        "/live",
        "/live/one.jsonl",
        "one",
        5,
    ))];
    run_scan(&adapters, &store, &Recorder::new(), true).unwrap();
    assert!(store.get_session("codex:one").unwrap().is_some());

    // roster 里这家没了(location 被移除)→ 补扫是纯删除轮
    let rec = Recorder::new();
    let empty: Vec<Box<dyn AgentAdapter>> = Vec::new();
    run_scan(&empty, &store, &rec, false).unwrap();
    assert!(
        store.get_session("codex:one").unwrap().is_none(),
        "行应被出清"
    );
    assert!(
        rec.changed() > 0,
        "纯删除轮未发 on_sessions_changed,UI 不会刷新"
    );
}

/// 胜者副本损坏时按裁决顺位回退:会话不得从索引消失(2026-08-24 Codex review P2)
#[test]
fn corrupt_winner_falls_back_to_valid_copy() {
    let dir = tempfile::tempdir().unwrap();
    let store = temp_store(dir.path());
    let mut broken_live = seed(AgentId::Codex, "/live", "/live/dup.jsonl", "dup", 9);
    broken_live.fail_parse = true;
    let adapters: Vec<Box<dyn AgentAdapter>> = vec![
        Box::new(broken_live),
        Box::new(seed(
            AgentId::Codex,
            "/backup",
            "/backup/dup.jsonl",
            "dup",
            5,
        )),
    ];
    run_scan(&adapters, &store, &Recorder::new(), true).unwrap();
    let s = store
        .get_session("codex:dup")
        .unwrap()
        .expect("胜者损坏后会话不该消失");
    assert_eq!(s.file_path, "/backup/dup.jsonl", "应回退到有效副本");
}

/// 胜者文件被删后,幸存副本经 promote_survivors 上位,不必等下一次全量扫描
/// (2026-08-24 Codex review P2)
#[test]
fn survivor_copy_promoted_after_winner_deleted() {
    let dir = tempfile::tempdir().unwrap();
    let store = temp_store(dir.path());
    let adapters: Vec<Box<dyn AgentAdapter>> = vec![
        Box::new(seed(AgentId::Codex, "/live", "/live/dup.jsonl", "dup", 9)),
        Box::new(seed(
            AgentId::Codex,
            "/backup",
            "/backup/dup.jsonl",
            "dup",
            5,
        )),
    ];
    let rec = Recorder::new();
    run_scan(&adapters, &store, &rec, true).unwrap();
    assert_eq!(
        store.get_session("codex:dup").unwrap().unwrap().file_path,
        "/live/dup.jsonl"
    );

    let key = store
        .remove_by_path("/live/dup.jsonl")
        .unwrap()
        .expect("应返回被删 key");
    assert!(store.get_session("codex:dup").unwrap().is_none());
    // 真实世界里被删文件不会再被枚举;SeedAdapter 是静态的,用"删除后"的
    // roster(只剩 backup 实例)喂给上位逻辑
    let after: Vec<Box<dyn AgentAdapter>> = vec![Box::new(seed(
        AgentId::Codex,
        "/backup",
        "/backup/dup.jsonl",
        "dup",
        5,
    ))];
    wake_core::watcher::promote_survivors(&after, &store, &rec, &[key]);
    assert_eq!(
        store
            .get_session("codex:dup")
            .unwrap()
            .expect("幸存副本应上位")
            .file_path,
        "/backup/dup.jsonl"
    );
}

/// location 易主(如把同一目录从 Pi 改成 Omp):文件 mtime/size 未变也要
/// 重新入库,旧 agent 行先删——否则旧行永留、新 key 撞 file_path UNIQUE,
/// 连全量刷新都救不回(2026-08-24 Codex review P1)
#[test]
fn owner_change_migrates_existing_rows() {
    let dir = tempfile::tempdir().unwrap();
    let store = temp_store(dir.path());
    let as_pi: Vec<Box<dyn AgentAdapter>> =
        vec![Box::new(seed(AgentId::Pi, "/d", "/d/s.jsonl", "s", 7))];
    run_scan(&as_pi, &store, &Recorder::new(), true).unwrap();
    assert!(store.get_session("pi:s").unwrap().is_some());

    // 同一文件、同 mtime/size,agent 换成 Omp;增量(full=false)也必须迁移
    let as_omp: Vec<Box<dyn AgentAdapter>> =
        vec![Box::new(seed(AgentId::Omp, "/d", "/d/s.jsonl", "s", 7))];
    run_scan(&as_omp, &store, &Recorder::new(), false).unwrap();
    assert!(
        store.get_session("pi:s").unwrap().is_none(),
        "旧 agent 行应被清理"
    );
    assert!(
        store.get_session("omp:s").unwrap().is_some(),
        "新 agent 行应入库"
    );
}

/// 墓碑双轨(不变量 3 的多副本延伸):删除只 trash 了胜者文件,另一 location
/// 的副本不得让会话复活——全量与 watcher 增量都要挡(2026-08-24 Codex review P1)
#[test]
fn tombstone_blocks_all_copies() {
    let dir = tempfile::tempdir().unwrap();
    let store = temp_store(dir.path());
    let backup_ref = SessionFileRef {
        agent: AgentId::Codex,
        native_id: "dup".into(),
        file_path: "/backup/dup.jsonl".into(),
        mtime_ms: 5,
        size: 1,
    };
    let adapters: Vec<Box<dyn AgentAdapter>> = vec![
        Box::new(seed(AgentId::Codex, "/live", "/live/dup.jsonl", "dup", 9)),
        Box::new(seed(
            AgentId::Codex,
            "/backup",
            "/backup/dup.jsonl",
            "dup",
            5,
        )),
    ];
    let rec = Recorder::new();
    run_scan(&adapters, &store, &rec, true).unwrap();
    assert!(store.get_session("codex:dup").unwrap().is_some());

    // UI 删除:trash 胜者文件 + 墓碑(key 一并入墓)
    store.remove_session("codex:dup", true).unwrap();
    run_scan(&adapters, &store, &rec, true).unwrap();
    assert!(
        store.get_session("codex:dup").unwrap().is_none(),
        "备份副本在全量扫描中复活了已删会话"
    );
    wake_core::scanner::scan_files(&adapters, &store, &rec, vec![backup_ref]);
    assert!(
        store.get_session("codex:dup").unwrap().is_none(),
        "备份副本在增量事件中复活了已删会话"
    );
}

/// 第三条写库路径(quick 的 write_meta_only)同样受 key 墓碑约束:codex 式
/// 改名 key 已入墓时,别的副本不得经 quick 快路径以空正文卡片复活已删会话
/// (2026-08-24 Codex review P1)
#[test]
fn quick_meta_respects_key_tombstone() {
    let dir = tempfile::tempdir().unwrap();
    let store = temp_store(dir.path());
    let mut a = seed(AgentId::Codex, "/live", "/live/dup.jsonl", "dup", 9);
    a.quick_key = Some("codex:thread-1".to_string());
    let adapters: Vec<Box<dyn AgentAdapter>> = vec![Box::new(a)];
    let rec = Recorder::new();
    run_scan(&adapters, &store, &rec, true).unwrap();
    assert!(
        store.get_session("codex:thread-1").unwrap().is_some(),
        "quick 改名 key 应入库"
    );

    store.remove_session("codex:thread-1", true).unwrap();
    run_scan(&adapters, &store, &rec, true).unwrap();
    assert!(
        store.get_session("codex:thread-1").unwrap().is_none(),
        "quick 快路径复活了已删会话"
    );
    assert!(
        store.get_session("codex:dup").unwrap().is_none(),
        "native key 也不得复活"
    );
}

/// key 后缀是内容 id 的家(gemini/pi/dsh):上位反查文件名对不上时,
/// 解析比对回退仍能找到幸存副本(2026-08-24 Codex review P2)
#[test]
fn survivor_promotion_matches_content_keys() {
    let dir = tempfile::tempdir().unwrap();
    let store = temp_store(dir.path());
    // meta.key 的后缀(LOGICAL)与文件名 native_id(session-123)不同
    let mut backup = seed(
        AgentId::Gemini,
        "/backup",
        "/backup/session-123.jsonl",
        "session-123",
        5,
    );
    backup.meta.key = "gemini:LOGICAL".to_string();
    backup.meta.id = "LOGICAL".to_string();
    let after: Vec<Box<dyn AgentAdapter>> = vec![Box::new(backup)];
    let rec = Recorder::new();
    wake_core::watcher::promote_survivors(&after, &store, &rec, &["gemini:LOGICAL".to_string()]);
    assert!(
        store.get_session("gemini:LOGICAL").unwrap().is_some(),
        "内容 key 的幸存副本未被上位"
    );
}
