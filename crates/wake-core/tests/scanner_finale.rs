//! 扫描终态契约(CLAUDE.md 不变量 6):run_scan 无论正常结束、Err 提前返回
//! 还是 adapter 出错,都必须发出一次 scanning=false 的终态进度事件——
//! UI 的模态刷新弹窗只认这个事件收场,收不到就永久锁死。

mod common;

use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{bail, Result};
use serde_json::json;
use wake_core::adapters::codex::CodexAdapter;
use wake_core::adapters::AgentAdapter;
use wake_core::db::Store;
use wake_core::mcp::tools::{self, TranscriptCache};
use wake_core::models::*;
use wake_core::scanner::{refresh_parent_links, run_scan, scan_files, ScanEvents, ScanProgress};

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

/// FTS 派生规则换代(db::FTS_FORMAT):旧库首开挂 fts_reindex 旗子,下一轮增量扫描
/// 把 mtime/size 都没变的行也重处理一遍,然后清旗子;新库不挂
#[test]
fn fts_reindex_flag_reprocesses_unchanged_rows_once() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("reindex.db");
    {
        let store = Store::open(&db_path).unwrap();
        assert!(!store.needs_fts_reindex(), "新库不该挂旗子");
    }
    // 模拟升级前的库:有 sessions 表,但没记过 fts_format
    rusqlite::Connection::open(&db_path)
        .unwrap()
        .execute("DELETE FROM schema_meta WHERE key = 'fts_format'", [])
        .unwrap();
    let store = Arc::new(Store::open(&db_path).unwrap());
    assert!(store.needs_fts_reindex(), "旧库首开要挂旗子");

    let path = "/tmp/reindex/p/s.jsonl";
    let mut before = seed(AgentId::ClaudeCode, "/tmp/reindex", path, "s", 42);
    before.meta.title = "before".into();
    // 库里已有同 mtime/size 的行,增量扫描本会跳过
    store.write_meta_only(&[(before.meta.clone(), 42)]).unwrap();
    let mut after = seed(AgentId::ClaudeCode, "/tmp/reindex", path, "s", 42);
    after.meta.title = "after".into();
    run_scan(
        &[Box::new(after) as Box<dyn AgentAdapter>],
        &store,
        &Recorder::new(),
        false,
    )
    .unwrap();
    let title = |store: &Store| store.get_session("claude-code:s").unwrap().unwrap().title;
    assert_eq!(title(&store), "after", "旗子在时未变的文件也要重处理");
    assert!(!store.needs_fts_reindex(), "跑完一轮就清");

    // 没旗子、文件没变:增量扫描照常跳过
    let mut third = seed(AgentId::ClaudeCode, "/tmp/reindex", path, "s", 42);
    third.meta.title = "third".into();
    run_scan(
        &[Box::new(third) as Box<dyn AgentAdapter>],
        &store,
        &Recorder::new(),
        false,
    )
    .unwrap();
    assert_eq!(title(&store), "after");
}

#[test]
fn cursor_path_backfill_flag_reprocesses_unchanged_rows_once() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("cursor-path.db");
    {
        let store = Store::open(&db_path).unwrap();
        assert!(
            !store.needs_cursor_path_backfill(),
            "新库本来就会全量解析,不该挂旗子"
        );
    }
    // 模拟只从 slug 还原路径的旧版索引。
    rusqlite::Connection::open(&db_path)
        .unwrap()
        .execute(
            "UPDATE schema_meta SET value = '2' WHERE key = 'cursor_path_format'",
            [],
        )
        .unwrap();
    let store = Arc::new(Store::open(&db_path).unwrap());
    assert!(
        store.needs_cursor_path_backfill(),
        "旧库首开要挂 Cursor 路径回填旗子"
    );

    let path = "/tmp/cursor-path/p/s.jsonl";
    let mut stale = seed(AgentId::Cursor, "/tmp/cursor-path", path, "s", 42);
    stale.meta.project_path = "/Users/tester/My/Project".into();
    stale.meta.project_name = "Project".into();
    store.write_meta_only(&[(stale.meta.clone(), 42)]).unwrap();

    let mut repaired = seed(AgentId::Cursor, "/tmp/cursor-path", path, "s", 42);
    repaired.meta.project_path = "/Users/tester/My Project".into();
    repaired.meta.project_name = "My Project".into();
    run_scan(
        &[Box::new(repaired) as Box<dyn AgentAdapter>],
        &store,
        &Recorder::new(),
        false,
    )
    .unwrap();

    let project = |store: &Store| store.get_session("cursor:s").unwrap().unwrap().project_path;
    assert_eq!(project(&store), "/Users/tester/My Project");
    assert!(!store.needs_cursor_path_backfill(), "成功一轮后清旗子");

    // 没旗子、文件没变:后续增量扫描照常跳过。
    let mut third = seed(AgentId::Cursor, "/tmp/cursor-path", path, "s", 42);
    third.meta.project_path = "/should/not/replace".into();
    run_scan(
        &[Box::new(third) as Box<dyn AgentAdapter>],
        &store,
        &Recorder::new(),
        false,
    )
    .unwrap();
    assert_eq!(project(&store), "/Users/tester/My Project");
}

#[test]
fn cursor_path_backfill_does_not_repeat_successful_rows_after_parse_failure() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("cursor-path-failure.db");
    let store = Arc::new(Store::open(&db_path).unwrap());
    let good = seed(
        AgentId::Cursor,
        "/tmp/cursor-good",
        "/tmp/cursor-good/s.jsonl",
        "good",
        42,
    );
    let mut broken = seed(
        AgentId::Cursor,
        "/tmp/cursor-broken",
        "/tmp/cursor-broken/s.jsonl",
        "broken",
        42,
    );
    broken.fail_parse = true;
    store
        .write_meta_only(&[(good.meta.clone(), 42), (broken.meta.clone(), 42)])
        .unwrap();
    rusqlite::Connection::open(&db_path)
        .unwrap()
        .execute(
            "INSERT INTO schema_meta(key, value) VALUES ('cursor_path_backfill', '1')",
            [],
        )
        .unwrap();
    let adapters: Vec<Box<dyn AgentAdapter>> = vec![Box::new(good), Box::new(broken)];
    let first = Recorder::new();
    run_scan(&adapters, &store, &first, false).unwrap();
    assert_eq!(first.0.lock().unwrap().last().unwrap().total, 2);
    assert!(!store.needs_cursor_path_backfill());

    let second = Recorder::new();
    run_scan(&adapters, &store, &second, false).unwrap();
    assert_eq!(second.0.lock().unwrap().last().unwrap().total, 1);
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
    rank: u8,
}

impl AgentAdapter for SeedAdapter {
    fn agent(&self) -> AgentId {
        self.agent
    }
    fn dedup_rank(&self) -> u8 {
        self.rank
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
            rank: self.rank,
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
            host: String::new(),
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
        rank: 0,
    }
}

/// 不变量 3 端到端:删除(trash+tombstone)后,数据源仍枚举同一文件的
/// 下一次全量扫描不得让会话复活
#[test]
fn tombstoned_session_does_not_resurrect_on_rescan() {
    let dir = tempfile::tempdir().unwrap();
    let store = temp_store(dir.path());
    let meta = SessionMeta {
        host: String::new(),
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
        rank: 0,
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

/// 副本裁决先看实例的 dedup_rank、同级才比 mtime:一家的多个数据源可以固定
/// 偏好某一源,而不随两边写盘先后翻转;败方仍是解析失败的回退顺位
#[test]
fn lower_dedup_rank_beats_newer_mtime_and_still_falls_back() {
    let dir = tempfile::tempdir().unwrap();
    let store = temp_store(dir.path());
    let mut secondary = seed(AgentId::Cursor, "/ide", "/ide/db#dup", "dup", 9);
    secondary.rank = 1;
    let primary = seed(AgentId::Cursor, "/cli", "/cli/dup.jsonl", "dup", 5);
    let adapters: Vec<Box<dyn AgentAdapter>> = vec![Box::new(secondary), Box::new(primary)];
    run_scan(&adapters, &store, &Recorder::new(), true).unwrap();
    let s = store
        .get_session("cursor:dup")
        .unwrap()
        .expect("会话应在库");
    assert_eq!(
        s.file_path, "/cli/dup.jsonl",
        "rank 小者胜出,即使 mtime 更旧、roster 里排在后面"
    );

    let dir = tempfile::tempdir().unwrap();
    let store = temp_store(dir.path());
    let mut secondary = seed(AgentId::Cursor, "/ide", "/ide/db#dup", "dup", 9);
    secondary.rank = 1;
    let mut broken_primary = seed(AgentId::Cursor, "/cli", "/cli/dup.jsonl", "dup", 5);
    broken_primary.fail_parse = true;
    let adapters: Vec<Box<dyn AgentAdapter>> = vec![Box::new(broken_primary), Box::new(secondary)];
    run_scan(&adapters, &store, &Recorder::new(), true).unwrap();
    let s = store
        .get_session("cursor:dup")
        .unwrap()
        .expect("胜者损坏后会话不该消失");
    assert_eq!(s.file_path, "/ide/db#dup", "rank 靠后的副本仍是回退顺位");

    // 增量路径同一把尺子:库里已是 rank 靠后的较新副本(转录曾缺失或只剩空壳,
    // IDE 副本胜出过),rank 靠前的较旧副本经 scan_files(watcher 路径)到来
    // 仍要接管,不被 write_session_guarded 的 mtime 比较挡住(2026-09-15 Codex review)
    let dir = tempfile::tempdir().unwrap();
    let store = temp_store(dir.path());
    let mut ide_only = seed(AgentId::Cursor, "/ide", "/ide/db#dup", "dup", 9);
    ide_only.rank = 1;
    let only_ide: Vec<Box<dyn AgentAdapter>> = vec![Box::new(ide_only)];
    run_scan(&only_ide, &store, &Recorder::new(), true).unwrap();
    assert_eq!(
        store.get_session("cursor:dup").unwrap().unwrap().file_path,
        "/ide/db#dup"
    );
    let mut secondary = seed(AgentId::Cursor, "/ide", "/ide/db#dup", "dup", 9);
    secondary.rank = 1;
    let primary = seed(AgentId::Cursor, "/cli", "/cli/dup.jsonl", "dup", 5);
    let incoming = primary.r.clone();
    let both: Vec<Box<dyn AgentAdapter>> = vec![Box::new(secondary), Box::new(primary)];
    scan_files(&both, &store, &Recorder::new(), vec![incoming]);
    assert_eq!(
        store.get_session("cursor:dup").unwrap().unwrap().file_path,
        "/cli/dup.jsonl",
        "增量写入也按 rank 接管,不被库里较新的 IDE 副本挡住"
    );
}

/// Cursor 两源端到端:转录带正文的会话固定由 CLI 源胜出(它有 slug 可反推
/// 项目),哪怕 IDE 库里那份 lastUpdatedAt 更新、roster 里 IDE 实例排在前面;
/// 转录只剩 turn_ended 空壳的会话由 IDE 库副本胜出。按 mtime 裁决的话胜负随
/// 两边写盘先后翻转,IDE 副本没有工作区路径,一翻就从项目里掉进 Unknown project
/// (2026-09-15 实测,Cursor 3.18)
#[test]
fn cursor_transcript_outranks_ide_copy() {
    use wake_core::adapters::cursor::CursorAdapter;
    use wake_core::adapters::cursor_ide::CursorIdeAdapter;
    const WITH_BODY: &str = "cursor:33333333-aaaa-bbbb-cccc-000000000003";
    const STUB_ONLY: &str = "cursor:44444444-aaaa-bbbb-cccc-000000000004";
    const CORRUPT: &str = "cursor:55555555-aaaa-bbbb-cccc-000000000005";

    let dir = tempfile::tempdir().unwrap();
    let projects = dir.path().join("projects");
    common::copy_tree(&common::fixture("cursor/projects"), &projects);
    let ide_db = dir.path().join("state.vscdb");
    common::build_cursor_ide_db(&ide_db);
    // 转录的 mtime 压到 IDE 库里 lastUpdatedAt(1786340100000)之前,
    // 让 mtime 裁决与 rank 裁决给出相反答案
    let transcript = projects.join(
        "wakefx-cursor-proj/agent-transcripts/33333333-aaaa-bbbb-cccc-000000000003/33333333-aaaa-bbbb-cccc-000000000003.jsonl",
    );
    std::fs::File::options()
        .write(true)
        .open(&transcript)
        .unwrap()
        .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_millis(1786000000000))
        .unwrap();
    // 截断成 `{"role":` 的坏转录:过得了空壳判定、解不出一条消息——不能靠
    // rank 压过完整的 IDE 副本,要按解析失败回退过去(2026-09-15 Codex review)
    let corrupt_dir =
        projects.join("wakefx-cursor-proj/agent-transcripts/55555555-aaaa-bbbb-cccc-000000000005");
    std::fs::create_dir_all(&corrupt_dir).unwrap();
    std::fs::write(
        corrupt_dir.join("55555555-aaaa-bbbb-cccc-000000000005.jsonl"),
        "{\"role\":\n",
    )
    .unwrap();

    for ide_first in [true, false] {
        let cli: Box<dyn AgentAdapter> = CursorAdapter::new().with_custom_root(projects.clone());
        let ide: Box<dyn AgentAdapter> = CursorIdeAdapter::new().with_custom_root(ide_db.clone());
        let adapters: Vec<Box<dyn AgentAdapter>> = if ide_first {
            vec![ide, cli]
        } else {
            vec![cli, ide]
        };
        let store = temp_store(&dir.path().join(format!("store-{ide_first}")));
        run_scan(&adapters, &store, &Recorder::new(), true).unwrap();

        let body = store
            .get_session(WITH_BODY)
            .unwrap()
            .expect("带正文的会话在库");
        // 按 Path 比较:Windows 上 read_dir 给的是反斜杠,join 里的字面量是正斜杠,
        // 按字符串比在 windows-2022 job 上必红(2026-09-15 CI)
        assert_eq!(
            Path::new(&body.file_path),
            transcript.as_path(),
            "ide_first={ide_first}:转录带正文时 CLI 那份胜出,不看 mtime 与 roster 顺序"
        );
        assert_eq!(
            body.project_path, "/wakefx/cursor/proj",
            "项目来自转录的 slug"
        );
        let stub = store
            .get_session(STUB_ONLY)
            .unwrap()
            .expect("空壳转录的会话在库");
        assert!(
            stub.file_path.contains("state.vscdb#"),
            "ide_first={ide_first}:转录只剩空壳时由 IDE 库副本胜出"
        );
        let corrupt = store
            .get_session(CORRUPT)
            .unwrap()
            .expect("坏转录的会话不该消失");
        assert!(
            corrupt.file_path.contains("state.vscdb#"),
            "ide_first={ide_first}:坏转录按解析失败回退到 IDE 库副本"
        );
        assert_eq!(corrupt.title, "Corrupt twin");
    }

    // 早先正常入库的转录后来损坏:胜者行已在库里、mtime 也不是占位,位次不能
    // 让它挡住回退——回退分支先清掉失效行,IDE 副本接管(Codex review 第二轮)
    std::fs::write(&transcript, "{\"role\":\n").unwrap();
    let cli: Box<dyn AgentAdapter> = CursorAdapter::new().with_custom_root(projects.clone());
    let ide: Box<dyn AgentAdapter> = CursorIdeAdapter::new().with_custom_root(ide_db.clone());
    let store = temp_store(&dir.path().join("store-false"));
    run_scan(&vec![cli, ide], &store, &Recorder::new(), true).unwrap();
    let taken_over = store
        .get_session(WITH_BODY)
        .unwrap()
        .expect("转录损坏后会话不该消失");
    assert!(
        taken_over.file_path.contains("state.vscdb#"),
        "已入库的转录损坏后由 IDE 副本接管"
    );
    assert_eq!(taken_over.title, "CLI twin");
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

/// v0.6.4 已把 guardian reviewer 当根会话写进库的升级场景：原始 rollout 仍在
/// 磁盘，但新 adapter 不再枚举它，常规扫描应同时清掉 session 与 FTS；MCP 的
/// 搜索、列表、详情和项目计数都必须随同一份索引恢复，不能另加展示层补丁。
#[test]
fn codex_guardian_rescan_prunes_stale_index_and_all_mcp_views() {
    let dir = tempfile::tempdir().unwrap();
    let store = temp_store(dir.path());
    let codex_home = dir.path().join("codex-home");
    let day = codex_home.join("sessions/2026/09/14");
    std::fs::create_dir_all(&day).unwrap();

    let guardian_id = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
    let guardian_key = format!("codex:{guardian_id}");
    let guardian_path = day.join(format!("rollout-2026-09-14T01-00-00-{guardian_id}.jsonl"));
    std::fs::write(
        &guardian_path,
        format!(
            "{}\n{}\n",
            json!({
                "timestamp": "2026-09-14T01:00:00.000Z",
                "type": "session_meta",
                "payload": {
                    "id": guardian_id,
                    "parent_thread_id": "eeeeeeee-eeee-4eee-8eee-eeeeeeeeeeee",
                    "thread_source": "subagent",
                    "source": { "subagent": { "other": "guardian" } },
                    "cwd": "/work/wake",
                    "originator": "codex_work_desktop"
                }
            }),
            json!({
                "timestamp": "2026-09-14T01:00:01.000Z",
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "assistant",
                    "content": [{ "type": "output_text", "text": "guardianartifact7429" }]
                }
            })
        ),
    )
    .unwrap();

    let normal_id = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
    let normal_key = format!("codex:{normal_id}");
    let normal_path = day.join(format!("rollout-2026-09-14T02-00-00-{normal_id}.jsonl"));
    std::fs::write(
        &normal_path,
        format!(
            "{}\n{}\n",
            json!({
                "timestamp": "2026-09-14T02:00:00.000Z",
                "type": "session_meta",
                "payload": {
                    "id": normal_id,
                    "thread_source": "cli",
                    "source": "cli",
                    "cwd": "/work/wake",
                    "originator": "codex_cli_rs"
                }
            }),
            json!({
                "timestamp": "2026-09-14T02:00:01.000Z",
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "user",
                    "content": [{ "type": "input_text", "text": "keep the normal session" }]
                }
            })
        ),
    )
    .unwrap();

    let adapters: Vec<Box<dyn AgentAdapter>> =
        vec![CodexAdapter::new().with_custom_root(codex_home.clone())];

    // 绕过新枚举边界手工种一条旧版索引，模拟用户从 v0.6.4 升级。
    let stale_ref = SessionFileRef {
        agent: AgentId::Codex,
        native_id: guardian_id.to_string(),
        file_path: guardian_path.to_string_lossy().to_string(),
        mtime_ms: 1,
        size: std::fs::metadata(&guardian_path).unwrap().len() as i64,
    };
    let stale = adapters[0].parse_session(&stale_ref).unwrap();
    store
        .write_session(&stale.meta, stale_ref.mtime_ms, &stale.units)
        .unwrap();
    assert!(store.get_session(&guardian_key).unwrap().is_some());
    let before = tools::invoke(
        store.as_ref(),
        &adapters,
        &TranscriptCache::default(),
        tools::SEARCH,
        &json!({ "query": "guardianartifact7429", "agents": ["codex"] }),
    )
    .unwrap();
    assert!(
        before.contains(&guardian_key),
        "stale fixture was not indexed"
    );

    run_scan(&adapters, &store, &Recorder::new(), true).unwrap();

    assert!(guardian_path.exists(), "扫描不得删除 Codex 原始 rollout");
    assert!(store.get_session(&guardian_key).unwrap().is_none());
    assert!(store.get_session(&normal_key).unwrap().is_some());

    let cache = TranscriptCache::default();
    let searched = tools::invoke(
        store.as_ref(),
        &adapters,
        &cache,
        tools::SEARCH,
        &json!({ "query": "guardianartifact7429", "agents": ["codex"] }),
    )
    .unwrap();
    assert!(searched.contains("No matches"), "{searched}");

    let listed = tools::invoke(
        store.as_ref(),
        &adapters,
        &cache,
        tools::LIST_SESSIONS,
        &json!({ "project": "/work/wake", "agents": ["codex"] }),
    )
    .unwrap();
    assert!(listed.contains(&normal_key), "{listed}");
    assert!(!listed.contains(&guardian_key), "{listed}");
    assert!(
        tools::invoke(
            store.as_ref(),
            &adapters,
            &cache,
            tools::GET_SESSION,
            &json!({ "key": guardian_key })
        )
        .is_err(),
        "已清理的 reviewer key 不应继续可读"
    );

    let projects = tools::invoke(
        store.as_ref(),
        &adapters,
        &cache,
        tools::LIST_PROJECTS,
        &json!({}),
    )
    .unwrap();
    let wake = projects
        .lines()
        .find(|line| line.contains("/work/wake"))
        .unwrap_or_else(|| panic!("Wake project missing:\n{projects}"));
    assert!(wake.contains("· 1 session ·"), "{wake}");
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

/// 跨 host 去重分域(不变量 8⑦ 的远程扩展):同 native_id 的会话在本地与
/// 远程各有一份(rsync 过项目后各自续跑是真实场景),按 mtime 的全局去重
/// 会吞掉一台的——去重域必须含实例 host,两行(本地 key / host key)并存。
#[test]
fn local_and_remote_copies_of_same_native_id_both_survive() {
    let dir = tempfile::tempdir().unwrap();
    let store = temp_store(dir.path());

    // 同一份 Claude fixture 摆进"本地自定义根"与"远程缓存"两棵树
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/claude/projects/-Users-tester-Github-wakefx/11111111-aaaa-bbbb-cccc-000000000001.jsonl");
    let native = "11111111-aaaa-bbbb-cccc-000000000001";
    let local_root = dir.path().join("local-claude");
    let remote_cache = dir.path().join("remotes/devbox");
    for tree in [&local_root, &remote_cache] {
        let proj = if tree == &local_root {
            tree.join("projects/-Users-tester-Github-wakefx")
        } else {
            tree.join(".claude/projects/-Users-tester-Github-wakefx")
        };
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::copy(&fixture, proj.join(format!("{native}.jsonl"))).unwrap();
    }

    let base = wake_core::adapters::create_adapters();
    let claude_template = base
        .iter()
        .find(|a| a.agent() == AgentId::ClaudeCode)
        .unwrap();
    let local = claude_template.with_custom_root(local_root);
    let adapters: Vec<Box<dyn AgentAdapter>> = vec![local]
        .into_iter()
        .chain(wake_core::adapters::remote::create_remote_adapters(
            &base,
            "devbox",
            &remote_cache,
        ))
        .collect();

    let events = Recorder::new();
    run_scan(&adapters, &store, &events, true).expect("scan ok");

    let local_key = format!("claude-code:{native}");
    let remote_key = format!("claude-code:devbox:{native}");
    let local_row = store.get_session(&local_key).unwrap();
    let remote_row = store.get_session(&remote_key).unwrap();
    assert!(local_row.is_some(), "本地副本被跨 host 去重吞掉");
    assert!(remote_row.is_some(), "远程副本被跨 host 去重吞掉");
    let remote_row = remote_row.unwrap();
    assert_eq!(remote_row.host, "devbox");
    assert_eq!(remote_row.id, native, "native id 保持纯净");
    assert!(local_row.unwrap().host.is_empty());

    // 再跑一轮(模拟下一次刷新):两行都不摇摆、不互删
    let events = Recorder::new();
    run_scan(&adapters, &store, &events, false).expect("rescan ok");
    assert!(store.get_session(&local_key).unwrap().is_some());
    assert!(store.get_session(&remote_key).unwrap().is_some());
}
