//! Store 写入/搜索/删除语义的往返测试(临时库,不碰真实索引)。
//! 覆盖 CLAUDE.md 不变量 3:tombstone 防复活、user_data 独立表重建不丢。

use std::time::{Duration, Instant};

use wake_core::db::{self, IndexLock, Ownership, Store, Wait};
use wake_core::models::*;

mod common;

fn temp_store() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::open(&dir.path().join("test.db")).expect("open store");
    (dir, store)
}

fn meta(key: &str, title: &str) -> SessionMeta {
    SessionMeta {
        host: String::new(),
        key: key.to_string(),
        id: key.split(':').nth(1).unwrap_or(key).to_string(),
        agent: AgentId::ClaudeCode,
        title: title.to_string(),
        project_path: "/tmp/proj".into(),
        project_name: "proj".into(),
        file_path: format!("/tmp/fixtures/{key}.jsonl"),
        created_at: 1_700_000_000_000,
        updated_at: 1_700_000_100_000,
        message_count: 2,
        size_bytes: 128,
        git_branch: None,
        model: None,
        tokens_used: None,
        archived: false,
        source: None,
        favorite: false,
        pinned: false,
    }
}

fn unit(seq: i64, role: Role, text: &str) -> IndexUnit {
    IndexUnit {
        seq,
        sidechain_id: None,
        role,
        timestamp: Some(1_700_000_000_000 + seq),
        text: text.to_string(),
    }
}

#[test]
fn search_roundtrip_hits_correct_seq() {
    let (_dir, store) = temp_store();
    let m = meta("claude-code:s1", "测试会话");
    let units = vec![
        unit(0, Role::User, "请帮我实现二维码扫描"),
        unit(3, Role::Assistant, "好的,用 useEffect( 挂载扫描器"),
    ];
    store.write_session(&m, m.updated_at, &units).unwrap();

    // 中文 trigram
    let (hits, degraded) = store.search("二维码", &[], None, 10).unwrap();
    assert!(!degraded, "3 码点应走 FTS 不降级");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].session.key, "claude-code:s1");
    assert_eq!(hits[0].seq, 0, "命中 seq 必须等于写入时的消息 seq");

    // 代码子串
    let (hits, _) = store.search("useEffect(", &[], None, 10).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].seq, 3);

    // <3 码点降级 LIKE
    let (hits, degraded) = store.search("好的", &[], None, 10).unwrap();
    assert!(degraded, "2 码点应降级");
    assert_eq!(hits.len(), 1);
}

#[test]
fn tombstone_primitives() {
    let (_dir, store) = temp_store();
    let m = meta("codex:s2", "会被删除的会话");
    store.write_session(&m, m.updated_at, &[]).unwrap();
    assert!(store.get_session("codex:s2").unwrap().is_some());

    // remove_session(tombstone=true) 后按 file_path 记墓碑。
    // 注意分层:write_meta_only 是纯写入原语、不查墓碑——防复活由
    // scanner 两条路径先过 is_tombstoned 保证(端到端见 scanner_finale.rs)
    store.remove_session("codex:s2", true).unwrap();
    assert!(store.get_session("codex:s2").unwrap().is_none());
    assert!(store.is_tombstoned(&m.file_path));
    assert!(!store.is_tombstoned("/tmp/other.jsonl"));
}

#[test]
fn user_data_survives_rebuild() {
    let (_dir, store) = temp_store();
    let m = meta("claude-code:s3", "收藏的会话");
    store.write_session(&m, m.updated_at, &[]).unwrap();
    store
        .set_user_data("claude-code:s3", Some(true), Some(true))
        .unwrap();

    let removed = meta("claude-code:removed", "已删除的会话");
    store
        .write_session(&removed, removed.updated_at, &[])
        .unwrap();
    store.remove_session("claude-code:removed", true).unwrap();
    store.add_custom_root("codex", "/tmp/codex-copy").unwrap();
    store.add_removed_default("gemini").unwrap();
    store
        .set_location_enabled("codex", "/tmp/codex-copy/sessions", false)
        .unwrap();

    // 重建索引只动可派生表；用户选择与防复活墓碑都必须保留。
    store.rebuild_all().unwrap();
    assert!(store.is_key_tombstoned("claude-code:removed"));
    assert_eq!(
        store.list_custom_roots().unwrap(),
        vec![("codex".to_string(), "/tmp/codex-copy".to_string())]
    );
    assert_eq!(
        store.list_removed_defaults().unwrap(),
        vec!["gemini".to_string()]
    );
    assert_eq!(
        store.list_disabled_locations().unwrap(),
        vec![("codex".to_string(), "/tmp/codex-copy/sessions".to_string())]
    );

    // session 被扫描器重新写回后，独立 user_data 重新合并进结果。
    store.write_session(&m, m.updated_at, &[]).unwrap();
    let got = store.get_session("claude-code:s3").unwrap().unwrap();
    assert!(got.favorite, "重建后收藏丢失 = user_data 未独立");
    assert!(got.pinned, "重建后置顶丢失 = user_data 未独立");
}

#[test]
fn list_sessions_filters_and_counts() {
    let (_dir, store) = temp_store();
    let mut a = meta("claude-code:s4", "A 会话");
    let mut b = meta("codex:s5", "B 会话");
    b.agent = AgentId::Codex;
    a.updated_at = 2_000;
    b.updated_at = 1_000;
    store.write_session(&a, a.updated_at, &[]).unwrap();
    store.write_session(&b, b.updated_at, &[]).unwrap();

    let all = SessionFilter {
        agents: vec![],
        favorite_only: false,
        include_archived: false,
        roots_only: false,
        title_query: None,
        sort: SortKey::Updated,
        ascending: false,
        limit: 10,
        offset: 0,
        updated_since: None,
        project_paths: Vec::new(),
        ignore_pins: false,
    };
    let (sessions, total) = store.list_sessions(&all).unwrap();
    assert_eq!(total, 2);
    assert_eq!(sessions[0].key, "claude-code:s4", "默认按 updated 降序");

    let only_codex = SessionFilter {
        agents: vec![AgentId::Codex],
        ..all
    };
    let (sessions, total) = store.list_sessions(&only_codex).unwrap();
    assert_eq!(total, 1);
    assert_eq!(sessions[0].key, "codex:s5");
}

#[test]
fn list_sessions_pages_have_stable_tie_order() {
    let (_dir, store) = temp_store();
    for suffix in ["d", "a", "e", "b", "c"] {
        let session = meta(&format!("claude-code:{suffix}"), suffix);
        store
            .write_session(&session, session.updated_at, &[])
            .unwrap();
    }

    let mut filter = SessionFilter {
        sort: SortKey::Updated,
        ascending: false,
        limit: 2,
        ..Default::default()
    };
    let mut keys = Vec::new();
    for offset in [0, 2, 4] {
        filter.offset = offset;
        let (page, total) = store.list_sessions(&filter).unwrap();
        assert_eq!(total, 5);
        keys.extend(page.into_iter().map(|session| session.key));
    }

    assert_eq!(
        keys,
        [
            "claude-code:a",
            "claude-code:b",
            "claude-code:c",
            "claude-code:d",
            "claude-code:e",
        ]
    );
}

#[test]
fn nested_sessions_are_aggregated_but_starred_stays_flat() {
    let (_dir, store) = temp_store();
    let mut parent = meta("grok:parent", "parent");
    parent.agent = AgentId::Grok;
    parent.project_path = "/work/source".into();
    parent.project_name = "source".into();
    parent.file_path = "/tmp/grok/a/parent/updates.jsonl".into();
    parent.updated_at = 100;
    parent.message_count = 2;
    let mut child = meta("grok:child", "child");
    child.agent = AgentId::Grok;
    child.project_path = "/tmp/wt-plan-pr-1".into();
    child.project_name = "wt-plan-pr-1".into();
    child.file_path = "/tmp/grok/b/child/updates.jsonl".into();
    child.updated_at = 500;
    child.message_count = 7;
    let mut other = meta("grok:other", "other");
    other.agent = AgentId::Grok;
    other.file_path = "/tmp/grok/a/other/updates.jsonl".into();
    other.updated_at = 400;
    other.message_count = 3;
    store
        .write_meta_only(&[
            (parent.clone(), parent.updated_at),
            (child.clone(), child.updated_at),
            (other, 400),
        ])
        .unwrap();
    store
        .replace_parent_links(AgentId::Grok, &[(child.key.clone(), parent.key.clone())])
        .unwrap();

    let filter = SessionFilter {
        roots_only: true,
        limit: 20,
        ..Default::default()
    };
    let (roots, total) = store.list_sessions(&filter).unwrap();
    assert_eq!(total, 2);
    assert_eq!(
        roots[0].key, parent.key,
        "child activity should sort its root"
    );
    assert_eq!(roots[0].updated_at, 500);
    assert_eq!(roots[0].message_count, 9);
    assert_eq!(store.child_counts(&filter).unwrap()[&parent.key], 1);
    let children = store.list_children(&parent.key, &filter).unwrap();
    assert_eq!(children.len(), 1);
    assert_eq!(children[0].key, child.key);
    assert_eq!(children[0].project_path, parent.project_path);
    assert_eq!(
        store.parent_key_of(&child.key).unwrap(),
        Some(parent.key.clone())
    );
    assert_eq!(store.agent_counts().unwrap()["grok"], 2);
    assert_eq!(
        store
            .list_projects(false)
            .unwrap()
            .iter()
            .find(|project| project.path == parent.project_path)
            .unwrap()
            .session_count,
        1
    );

    store.set_user_data(&child.key, Some(true), None).unwrap();
    let starred = SessionFilter {
        favorite_only: true,
        roots_only: false,
        limit: 20,
        ..Default::default()
    };
    let (rows, total) = store.list_sessions(&starred).unwrap();
    assert_eq!(total, 1);
    assert_eq!(
        rows[0].key, child.key,
        "Starred must keep child sessions flat"
    );
}

#[test]
fn since_on_root_lists_uses_aggregated_child_activity() {
    let (_dir, store) = temp_store();
    let mut parent = meta("grok:since-parent", "parent");
    parent.agent = AgentId::Grok;
    parent.file_path = "/tmp/grok/s/parent/updates.jsonl".into();
    parent.updated_at = 100;
    let mut child = meta("grok:since-child", "child");
    child.agent = AgentId::Grok;
    child.file_path = "/tmp/grok/s/child/updates.jsonl".into();
    child.updated_at = 500;
    store
        .write_meta_only(&[
            (parent.clone(), parent.updated_at),
            (child.clone(), child.updated_at),
        ])
        .unwrap();
    store
        .replace_parent_links(AgentId::Grok, &[(child.key.clone(), parent.key.clone())])
        .unwrap();

    // 父旧子新:根列表按聚合活动时间过滤,父会话必须还在(否则父子都消失)
    let roots = SessionFilter {
        roots_only: true,
        updated_since: Some(300),
        limit: 20,
        ..Default::default()
    };
    let (rows, total) = store.list_sessions(&roots).unwrap();
    assert_eq!(total, 1);
    assert_eq!(rows[0].key, parent.key);
    assert_eq!(rows[0].updated_at, 500, "返回的时间同样是聚合值");
    // 平铺列表仍按自身时间过滤
    let flat = SessionFilter {
        updated_since: Some(300),
        limit: 20,
        ..Default::default()
    };
    let (rows, _) = store.list_sessions(&flat).unwrap();
    assert_eq!(
        rows.iter().map(|s| s.key.as_str()).collect::<Vec<_>>(),
        [child.key.as_str()]
    );
}

#[test]
fn ignore_pins_gives_true_recency_order() {
    let (_dir, store) = temp_store();
    let mut old = meta("claude-code:old-pinned", "old");
    old.updated_at = 100;
    let mut new = meta("claude-code:newest", "new");
    new.updated_at = 900;
    store
        .write_meta_only(&[(old.clone(), 100), (new.clone(), 900)])
        .unwrap();
    store.set_user_data(&old.key, None, Some(true)).unwrap();

    let gui = SessionFilter {
        limit: 1,
        ..Default::default()
    };
    let (rows, _) = store.list_sessions(&gui).unwrap();
    assert_eq!(rows[0].key, old.key, "GUI 列表置顶优先");
    let mcp = SessionFilter {
        limit: 1,
        ignore_pins: true,
        ..Default::default()
    };
    let (rows, _) = store.list_sessions(&mcp).unwrap();
    assert_eq!(rows[0].key, new.key, "ignore_pins 后 limit 内是真最近");
}

#[test]
fn archived_only_projects_are_listed_only_when_asked() {
    let (_dir, store) = temp_store();
    let mut archived = meta("codex:archived-only", "old work");
    archived.agent = AgentId::Codex;
    archived.archived = true;
    archived.project_path = "/work/retired".into();
    archived.project_name = "retired".into();
    archived.file_path = "/tmp/codex/archived_sessions/r.jsonl".into();
    store
        .write_meta_only(&[(archived.clone(), archived.updated_at)])
        .unwrap();
    let paths = |include: bool| {
        store
            .list_projects(include)
            .unwrap()
            .into_iter()
            .map(|p| p.path)
            .collect::<Vec<_>>()
    };
    assert!(paths(false).is_empty(), "GUI 项目列表不含归档");
    assert_eq!(paths(true), ["/work/retired".to_string()]);
}

#[test]
fn removing_a_session_tree_writes_every_tombstone_atomically() {
    let (_dir, store) = temp_store();
    let mut parent = meta("grok:delete-parent", "parent");
    parent.agent = AgentId::Grok;
    let mut child = meta("grok:delete-child", "child");
    child.agent = AgentId::Grok;
    store
        .write_meta_only(&[
            (parent.clone(), parent.updated_at),
            (child.clone(), child.updated_at),
        ])
        .unwrap();
    store
        .replace_parent_links(AgentId::Grok, &[(child.key.clone(), parent.key.clone())])
        .unwrap();
    assert_eq!(store.all_descendants(&parent.key).unwrap().len(), 1);

    store
        .remove_sessions(&[parent.key.clone(), child.key.clone()], true)
        .unwrap();
    assert!(store.get_session(&parent.key).unwrap().is_none());
    assert!(store.get_session(&child.key).unwrap().is_none());
    assert!(store.is_key_tombstoned(&parent.key));
    assert!(store.is_key_tombstoned(&child.key));
    assert!(store.is_tombstoned(&parent.file_path));
    assert!(store.is_tombstoned(&child.file_path));
}

/// 最老的 sessions schema(无 parent_key、无 host):迁移类测试共用,
/// 再加列时别再抄第三份 DDL
fn create_legacy_db(path: &std::path::Path) -> rusqlite::Connection {
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.execute_batch(
        "CREATE TABLE sessions (
           key TEXT PRIMARY KEY, agent_id TEXT NOT NULL, native_id TEXT NOT NULL,
           title TEXT NOT NULL DEFAULT '', project_path TEXT NOT NULL DEFAULT '',
           project_name TEXT NOT NULL DEFAULT '', git_branch TEXT, created_at INTEGER DEFAULT 0,
           updated_at INTEGER DEFAULT 0, message_count INTEGER DEFAULT 0, tokens_used INTEGER,
           model TEXT, source TEXT, archived INTEGER DEFAULT 0, file_path TEXT NOT NULL UNIQUE,
           file_size INTEGER DEFAULT 0, file_mtime INTEGER DEFAULT 0, unknown_lines INTEGER DEFAULT 0
         );",
    )
    .unwrap();
    conn
}

#[test]
fn old_database_marks_grok_backfill_until_scan_finishes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("old.db");
    drop(create_legacy_db(&path));

    let store = Store::open(&path).unwrap();
    assert!(store.needs_grok_parent_backfill());
    store.finish_grok_parent_backfill().unwrap();
    assert!(!store.needs_grok_parent_backfill());

    let conn = rusqlite::Connection::open(path).unwrap();
    let has_parent: bool = conn
        .prepare("PRAGMA table_info(sessions)")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(1))
        .unwrap()
        .flatten()
        .any(|name| name == "parent_key");
    assert!(has_parent);
}

#[test]
fn path_counts_respect_agent_and_boundary() {
    // Session locations 面板的计数按数据根归属。两条真实风险:
    // ① 自定义 CODEX_HOME/XDG_DATA_HOME 可以落在别家根之下,只比路径前缀
    //    不看 agent,会把这家的会话整批记到别家行上;
    // ② 裸 starts_with 没有边界,`…/sessions` 会连 `…/sessions-old` 一起吞。
    let (_d, store) = temp_store();
    let mut claude = meta("claude-code:a", "claude one");
    claude.file_path = "/home/u/.claude/projects/a.jsonl".into();
    // codex 的根被搬进了 claude 的树下(CODEX_HOME 允许这么设)
    let mut codex = meta("codex:b", "codex one");
    codex.agent = AgentId::Codex;
    codex.file_path = "/home/u/.claude/projects/codex/sessions/b.jsonl".into();
    // 同名前缀的兄弟目录:不该算进 `…/sessions`
    let mut sibling = meta("codex:c", "codex sibling");
    sibling.agent = AgentId::Codex;
    sibling.file_path = "/home/u/.claude/projects/codex/sessions-old/c.jsonl".into();
    store
        .write_meta_only(&[(claude, 0), (codex, 0), (sibling, 0)])
        .expect("seed sessions");

    let counts = store
        .counts_by_path_prefix(&[
            ("claude-code".into(), "/home/u/.claude/projects".into()),
            (
                "codex".into(),
                "/home/u/.claude/projects/codex/sessions".into(),
            ),
        ])
        .expect("counts");
    assert_eq!(counts[0], 1, "codex 的会话不该被记到 claude 行");
    assert_eq!(counts[1], 1, "sessions-old 不该算进 sessions");
}

/// 自定义 location 的持久化:与收藏/置顶同层级的用户数据,重复添加幂等
#[test]
fn custom_roots_roundtrip() {
    let (_dir, store) = temp_store();
    store.add_custom_root("codex", "/tmp/a").unwrap();
    store.add_custom_root("codex", "/tmp/a").unwrap(); // 幂等
    store.add_custom_root("claude-code", "/tmp/b").unwrap();

    let mut roots = store.list_custom_roots().unwrap();
    roots.sort();
    assert_eq!(
        roots,
        vec![
            ("claude-code".to_string(), "/tmp/b".to_string()),
            ("codex".to_string(), "/tmp/a".to_string()),
        ]
    );

    store.remove_custom_root("codex", "/tmp/a").unwrap();
    assert_eq!(
        store.list_custom_roots().unwrap(),
        vec![("claude-code".to_string(), "/tmp/b".to_string())]
    );

    // 预设移除是按 agent 压制,幂等
    store.add_removed_default("codex").unwrap();
    store.add_removed_default("codex").unwrap();
    assert_eq!(
        store.list_removed_defaults().unwrap(),
        vec!["codex".to_string()]
    );
    store
        .add_removed_default_root("opencode", "/tmp/opencode-next.db")
        .unwrap();
    store
        .add_removed_default_root("opencode", "/tmp/opencode-next.db")
        .unwrap();
    assert_eq!(
        store.list_removed_default_roots().unwrap(),
        vec![("opencode".to_string(), "/tmp/opencode-next.db".to_string())]
    );

    // 编辑的原子替换:自定义换路径 / 预设改自定义 / 换 agent,全走单事务
    store.add_custom_root("grok", "/tmp/g1").unwrap();
    store
        .replace_location("grok", Some("/tmp/g1"), None, "/tmp/g1", "grok", "/tmp/g2")
        .unwrap();
    assert_eq!(
        store.list_custom_roots().unwrap(),
        vec![
            ("claude-code".to_string(), "/tmp/b".to_string()),
            ("grok".to_string(), "/tmp/g2".to_string()),
        ]
    );
    store
        .replace_location("kiro", None, None, "/tmp/kiro-default", "kiro", "/tmp/k")
        .unwrap();
    assert!(store
        .list_removed_defaults()
        .unwrap()
        .contains(&"kiro".to_string()));
    store
        .replace_location(
            "opencode",
            None,
            Some("/tmp/opencode.db"),
            "/tmp/opencode.db",
            "opencode",
            "/tmp/opencode-copy",
        )
        .unwrap();
    assert!(store
        .list_removed_default_roots()
        .unwrap()
        .contains(&("opencode".to_string(), "/tmp/opencode.db".to_string())));
    store
        .replace_location(
            "grok",
            Some("/tmp/g2"),
            None,
            "/tmp/g2",
            "cursor",
            "/tmp/cur",
        )
        .unwrap();
    let roots = store.list_custom_roots().unwrap();
    assert!(roots.iter().any(|(a, p)| a == "cursor" && p == "/tmp/cur"));
    assert!(!roots.iter().any(|(a, _)| a == "grok"));

    // Restore defaults 语义:自定义与预设移除一把双清
    store.add_custom_root("grok", "/tmp/c").unwrap();
    store.clear_location_overrides().unwrap();
    assert!(store.list_custom_roots().unwrap().is_empty());
    assert!(store.list_removed_defaults().unwrap().is_empty());
    assert!(store.list_removed_default_roots().unwrap().is_empty());
}

/// location 开关只持久化停用状态，不删除配置；真正 Remove 自定义根或
/// Restore defaults 时清理相关状态，今后重新添加默认启用。
#[test]
fn disabled_locations_roundtrip() {
    let (_dir, store) = temp_store();
    store.add_custom_root("codex", "/tmp/codex-copy").unwrap();
    store
        .set_location_enabled("codex", "/tmp/codex-copy/sessions", false)
        .unwrap();
    store
        .set_location_enabled("codex", "/tmp/codex-copy/sessions", false)
        .unwrap(); // 幂等
    store
        .set_location_enabled("claude-code", "/tmp/claude", false)
        .unwrap();

    let mut disabled = store.list_disabled_locations().unwrap();
    disabled.sort();
    assert_eq!(
        disabled,
        vec![
            ("claude-code".to_string(), "/tmp/claude".to_string()),
            ("codex".to_string(), "/tmp/codex-copy/sessions".to_string(),),
        ]
    );
    assert_eq!(store.disabled_locations().len(), 2);

    store
        .set_location_enabled("claude-code", "/tmp/claude", true)
        .unwrap();
    assert_eq!(store.list_disabled_locations().unwrap().len(), 1);

    store
        .remove_custom_root("codex", "/tmp/codex-copy")
        .unwrap();
    assert!(store.list_disabled_locations().unwrap().is_empty());

    store
        .set_location_enabled("gemini", "/tmp/gemini-default", false)
        .unwrap();
    store
        .replace_location(
            "gemini",
            None,
            None,
            "/tmp/gemini-default",
            "gemini",
            "/tmp/gemini-copy",
        )
        .unwrap();
    assert!(
        store.list_disabled_locations().unwrap().is_empty(),
        "编辑停用的内置 location 后遗留了旧状态"
    );

    store
        .set_location_enabled("cursor", "/tmp/cursor", false)
        .unwrap();
    store.clear_location_overrides().unwrap();
    assert!(store.list_disabled_locations().unwrap().is_empty());
}

/// wake_lookups(agent 查 Wake 的每一次调用):随会话同事务写入、重解析即整体替换、
/// 删会话时一并删;quick 路径(write_meta_only)不碰这张表。Insights 的 Agents asking
/// Wake 按调用时刻归到最近 7 天、按渠道分列,缺时间戳的退回会话 updated_at
#[test]
fn wake_lookups_follow_the_session_and_reach_insights() {
    let (_dir, store) = temp_store();
    let today = chrono::Local::now().date_naive();
    let now = chrono::Local::now().timestamp_millis();
    let hour = 3_600_000;
    let lookup =
        |seq: i64, ts: Option<i64>, channel: LookupChannel, tool: &'static str| WakeLookup {
            seq,
            timestamp: ts,
            channel,
            tool,
        };
    let write = |m: &SessionMeta, mtime: i64, lookups: &[WakeLookup]| {
        assert!(store
            .write_session_guarded(m, mtime, &[], lookups, &|_| 0, None)
            .unwrap());
    };
    let tally = |name: &str, mcp: i64, cli: i64| LookupTally {
        name: name.to_string(),
        mcp,
        cli,
    };
    let tallies = || store.insights(today).unwrap().wake_lookups_7d;

    let mut a = meta("claude-code:a", "查过 Wake");
    a.updated_at = now - hour;
    write(
        &a,
        1,
        &[
            lookup(3, Some(now - 2 * hour), LookupChannel::Mcp, "wake_search"),
            lookup(7, Some(now - hour), LookupChannel::Mcp, "wake_get_session"),
            // 缺时间戳:退回会话 updated_at,在窗内
            lookup(9, None, LookupChannel::Cli, "wake-cli"),
            // 30 天前的那次不算
            lookup(
                11,
                Some(now - 30 * 24 * hour),
                LookupChannel::Cli,
                "wake-cli",
            ),
        ],
    );
    store.write_meta_only(&[(a.clone(), 1)]).unwrap();
    let mut b = meta("codex:b", "也查过");
    b.agent = AgentId::Codex;
    b.updated_at = now - hour;
    write(
        &b,
        1,
        &[lookup(1, Some(now - hour), LookupChannel::Cli, "wake-mcp")],
    );
    let mut none = meta("gemini:n", "没查过");
    none.agent = AgentId::Gemini;
    write(&none, 1, &[]);

    assert_eq!(
        tallies(),
        vec![tally("claude-code", 2, 1), tally("codex", 0, 1)],
        "quick 写入不动这张表;窗外那次与零调用的家不出现"
    );
    write(
        &a,
        2,
        &[lookup(3, Some(now - hour), LookupChannel::Cli, "wake-cli")],
    );
    assert_eq!(
        tallies()[0],
        tally("claude-code", 0, 1),
        "重解析整体替换旧记录"
    );
    store
        .remove_sessions(&["claude-code:a".to_string()], false)
        .unwrap();
    assert_eq!(tallies(), vec![tally("codex", 0, 1)], "删会话一并删记录");
}

/// 增量写入的胜者裁决在写事务内:败方副本(旧 mtime、异路径)一字不写,
/// 反超后按规则接管(2026-08-24 Codex review)
#[test]
fn guarded_write_respects_winner() {
    let (_dir, store) = temp_store();
    let mut winner = meta("codex:g", "胜者");
    winner.file_path = "/live/g.jsonl".into();
    store.write_session(&winner, 9, &[]).unwrap();

    let mut loser = meta("codex:g", "败方");
    loser.file_path = "/backup/g.jsonl".into();
    assert!(
        !store
            .write_session_guarded(&loser, 5, &[], &[], &|_| 0, None)
            .unwrap(),
        "败方不该写入"
    );
    assert_eq!(
        store.get_session("codex:g").unwrap().unwrap().file_path,
        "/live/g.jsonl"
    );

    assert!(
        store
            .write_session_guarded(&loser, 12, &[], &[], &|_| 0, None)
            .unwrap(),
        "反超应接管"
    );
    assert_eq!(
        store.get_session("codex:g").unwrap().unwrap().file_path,
        "/backup/g.jsonl"
    );

    // 位次压过 mtime(与 scanner 枚举时的候选排序同一把尺子):库里是 rank
    // 靠后的较新副本(Cursor IDE 库),rank 靠前的较旧副本(转录)到来仍接管;
    // 反向的较新 IDE 副本不得反超(2026-09-15 Codex review)
    let rank_of = |path: &str| if path.contains('#') { 1 } else { 0 };
    let mut ide = meta("cursor:h", "IDE 副本");
    ide.file_path = "/store/state.vscdb#h".into();
    store.write_session(&ide, 9, &[]).unwrap();
    let mut cli = meta("cursor:h", "转录");
    cli.file_path = "/cli/h.jsonl".into();
    assert!(
        store
            .write_session_guarded(&cli, 5, &[], &[], &rank_of, None)
            .unwrap(),
        "rank 靠前的较旧副本应接管"
    );
    assert_eq!(
        store.get_session("cursor:h").unwrap().unwrap().file_path,
        "/cli/h.jsonl"
    );
    assert!(
        !store
            .write_session_guarded(&ide, 12, &[], &[], &rank_of, None)
            .unwrap(),
        "rank 靠后的较新副本不得反超"
    );

    // supersedes:库里的行正是刚解析失败的那份副本(同路径、同枚举 mtime)时
    // 它让位,回退副本直接接管(判定与写入同一事务);同路径但库里版本更新
    //(mtime 更大)= watcher 已把修好的文件成功入库,不让位;库里若已被并发写成
    // 另一份副本,照常裁决、不误删(2026-09-15 Codex review 三、四轮)
    assert!(
        !store
            .write_session_guarded(&ide, 12, &[], &[], &rank_of, Some(("/cli/h.jsonl", 4)))
            .unwrap(),
        "同路径但库里版本更新(mtime 更大)不让位"
    );
    assert!(
        store
            .write_session_guarded(&ide, 12, &[], &[], &rank_of, Some(("/cli/h.jsonl", 5)))
            .unwrap(),
        "失效的胜者行让位给回退副本"
    );
    assert_eq!(
        store.get_session("cursor:h").unwrap().unwrap().file_path,
        "/store/state.vscdb#h"
    );
    // 同级(都是 IDE 库副本)且更旧的第三份:库里已不是让位的那份,照常裁决
    let mut third = meta("cursor:h", "第三份副本");
    third.file_path = "/store2/state.vscdb#h".into();
    assert!(
        !store
            .write_session_guarded(&third, 3, &[], &[], &rank_of, Some(("/cli/h.jsonl", 5)))
            .unwrap(),
        "库里已是别的副本时 supersedes 不生效,按位次与 mtime 照常裁决"
    );
    assert_eq!(
        store.get_session("cursor:h").unwrap().unwrap().file_path,
        "/store/state.vscdb#h"
    );

    // quick 阶段为失败的胜者写的占位行(file_mtime=0)同样让位
    let mut placeholder = meta("cursor:p", "占位");
    placeholder.file_path = "/cli/p.jsonl".into();
    store.write_meta_only(&[(placeholder, 0)]).unwrap();
    let mut p_ide = meta("cursor:p", "IDE 副本");
    p_ide.file_path = "/store/state.vscdb#p".into();
    assert!(
        store
            .write_session_guarded(&p_ide, 12, &[], &[], &rank_of, Some(("/cli/p.jsonl", 5)))
            .unwrap(),
        "占位行让位给回退副本"
    );
    assert_eq!(
        store.get_session("cursor:p").unwrap().unwrap().file_path,
        "/store/state.vscdb#p"
    );
}

/// Insights 统计口径:archived=0、prompts 只数主线 user、daily/hourly 按
/// 本地时区分桶(SQL 'localtime' 与 chrono Local 必须同界)、streak 允许
/// 今天尚无活动时从昨天起算。日期全固定,不依赖真实时钟。
#[test]
fn insights_snapshot_and_streaks() {
    use chrono::TimeZone;
    let (_dir, store) = temp_store();
    let ts = |d: u32, h: u32| {
        chrono::Local
            .with_ymd_and_hms(2026, 1, d, h, 30, 0)
            .single()
            .expect("unambiguous local time")
            .timestamp_millis()
    };
    let at = |seq: i64, role: Role, t: Option<i64>| IndexUnit {
        seq,
        sidechain_id: None,
        role,
        timestamp: t,
        text: format!("msg {seq}"),
    };

    // s1(claude-code):10/11/12 三连活跃日;含 assistant、sidechain、无 ts 行
    let mut m1 = meta("claude-code:i1", "insights 甲");
    m1.model = Some("claude-opus".into());
    let mut units = vec![
        at(0, Role::User, Some(ts(10, 9))),
        at(1, Role::Assistant, Some(ts(10, 9))),
        at(2, Role::User, Some(ts(11, 14))),
        at(3, Role::User, Some(ts(11, 14))),
        at(4, Role::User, Some(ts(12, 22))),
        at(5, Role::User, None), // 无 ts:计入 prompts,不进 daily/hourly
    ];
    units.push(IndexUnit {
        seq: 6,
        sidechain_id: Some("side".into()),
        role: Role::User,
        timestamp: Some(ts(12, 22)),
        text: "子代理里的 user 不算 prompt".into(),
    });
    store.write_session(&m1, m1.updated_at, &units).unwrap();

    // s2(codex):1/7 孤立活跃日 + 1/11;带 tokens。另有一条 2/5 的
    // "未来"消息(相对 today=1/13,模拟时钟漂移脏数据):prompts 总数计入,
    // 分桶/streak/活跃天数全不认——与热力图不画未来格同口径
    let mut m2 = meta("codex:i2", "insights 乙");
    m2.agent = AgentId::Codex;
    m2.file_path = "/tmp/fixtures/i2.jsonl".into();
    m2.model = Some("gpt-5-codex".into());
    m2.tokens_used = Some(500);
    let future = chrono::Local
        .with_ymd_and_hms(2026, 2, 5, 9, 30, 0)
        .single()
        .expect("unambiguous local time")
        .timestamp_millis();
    store
        .write_session(
            &m2,
            m2.updated_at,
            &[
                at(0, Role::User, Some(ts(7, 9))),
                at(1, Role::User, Some(ts(11, 14))),
                at(2, Role::User, Some(future)),
            ],
        )
        .unwrap();

    // s3:archived,任何统计都不该出现
    let mut m3 = meta("codex:i3", "已归档");
    m3.agent = AgentId::Codex;
    m3.file_path = "/tmp/fixtures/i3.jsonl".into();
    m3.archived = true;
    store
        .write_session(&m3, m3.updated_at, &[at(0, Role::User, Some(ts(1, 9)))])
        .unwrap();

    let today = chrono::NaiveDate::from_ymd_opt(2026, 1, 13).unwrap();
    let d = store.insights(today).unwrap();

    assert_eq!(d.as_of, today);
    assert_eq!(d.sessions, 2);
    assert_eq!(d.prompts, 8, "主线 user:s1 五条(含无 ts)+ s2 三条(含未来)");
    assert_eq!(d.tokens, 500);
    assert_eq!(d.project_count, 1);
    assert_eq!(d.active_days(), 4, "未来日不算活跃天");
    assert_eq!(
        d.busiest_day(),
        Some((chrono::NaiveDate::from_ymd_opt(2026, 1, 11).unwrap(), 3))
    );
    assert_eq!((d.current_streak, d.longest_streak), (3, 3));
    assert_eq!(d.hourly[9], 2);
    assert_eq!(d.hourly[14], 3);
    assert_eq!(d.hourly[22], 1);
    assert_eq!(d.hourly.iter().sum::<i64>(), 6, "无 ts 与未来行不进 hourly");
    // 2026-01-07 周三、01-10 周六、01-11 周日×3、01-12 周一(周一起始序)
    assert_eq!(d.weekday, [1, 0, 1, 0, 0, 1, 3]);
    assert_eq!(d.monthly[0], 6, "全部落在一月");
    assert_eq!(d.monthly.iter().sum::<i64>(), 6);
    assert_eq!(
        d.agents,
        vec![
            UsageTally {
                name: "claude-code".into(),
                sessions: 1,
                prompts: 5, // 四条有 ts + 一条无 ts;sidechain 不算
                tokens: 0,
            },
            UsageTally {
                name: "codex".into(),
                sessions: 1,
                prompts: 3, // 榜单 prompts 无日期语义,未来行也计
                tokens: 500,
            },
        ]
    );
    assert_eq!(
        d.projects,
        vec![UsageTally {
            name: "proj".into(),
            sessions: 2,
            prompts: 8, // 两家主线 user 之和(含无 ts 与未来行)
            tokens: 500,
        }]
    );
    assert_eq!(
        d.models,
        vec![
            UsageTally {
                name: "claude-opus".into(),
                sessions: 1,
                prompts: 5,
                tokens: 0,
            },
            UsageTally {
                name: "gpt-5-codex".into(),
                sessions: 1,
                prompts: 3,
                tokens: 500,
            },
        ]
    );

    // 会话按创建日分桶(fixture meta 的 created_at 同一毫秒,本地日由时区定),归档不计
    let created_day = chrono::Local
        .timestamp_millis_opt(m1.created_at)
        .single()
        .expect("unambiguous local time")
        .date_naive();
    assert_eq!(d.daily_sessions, vec![(created_day, 2)]);
    // 趋势:as_of=1/13(周二)→ 本周从 1/12 起是末列 52;1/7、1/10、1/11 落在
    // 1/5 那周 = 51。未来行与无 ts 行不进周桶
    let claude = &d.trend_agents[0];
    assert_eq!((claude.name.as_str(), claude.total()), ("claude-code", 4));
    assert_eq!((claude.weekly[51], claude.weekly[52]), (3, 1));
    assert_eq!(claude.weekly.len(), TREND_WEEKS);
    let codex = &d.trend_agents[1];
    assert_eq!((codex.name.as_str(), codex.total()), ("codex", 2));
    assert_eq!(codex.weekly[51], 2);
    // Last 7 days(1/7–1/13)对前 7 天(12/31–1/6):prompts 6 / 0,活跃日 4 / 0;
    // 会话创建在 2023,两窗都是 0
    let (cur, prev) = d.last_week_pair();
    assert_eq!(
        cur,
        WindowStats {
            sessions: 0,
            prompts: 6,
            active_days: 4
        }
    );
    assert_eq!(prev, WindowStats::default());

    // 超出 SQLite date() 范围的正 created_at(微秒戳/脏值):date() 回 NULL,
    // 该行跳过,快照不得整体失败(2026-09-03 Codex review)
    let mut bad = meta("codex:i-bad", "脏 created_at");
    bad.created_at = 9_000_000_000_000_000;
    store
        .write_session(&bad, bad.updated_at, &[at(0, Role::User, Some(ts(12, 8)))])
        .unwrap();
    let d2 = store
        .insights(today)
        .expect("bad created_at must not abort insights");
    assert_eq!(d2.sessions, d.sessions + 1);
    assert_eq!(d2.daily_sessions, vec![(created_day, 2)]);

    // 断档超过一天 → current 归零,longest 保留
    let far = chrono::NaiveDate::from_ymd_opt(2026, 2, 1).unwrap();
    let d = store.insights(far).unwrap();
    assert_eq!((d.current_streak, d.longest_streak), (0, 3));
}

/// 老库(无 host 列)打开即迁移;既有行落 ''(本地域),写入远程行后
/// host_counts 只统计非空 host。remote_hosts 配置与同步状态可往返。
#[test]
fn host_column_migration_and_remote_host_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("old.db");
    let conn = create_legacy_db(&path);
    conn.execute(
        "INSERT INTO sessions (key, agent_id, native_id, file_path)
         VALUES ('claude-code:legacy', 'claude-code', 'legacy', '/tmp/legacy.jsonl')",
        [],
    )
    .unwrap();
    drop(conn);

    let store = Store::open(&path).unwrap();
    // 老行读出来 host 为空(本地)
    let legacy = store.get_session("claude-code:legacy").unwrap().unwrap();
    assert!(legacy.host.is_empty());

    // 远程行写入/读出 host 往返
    let mut remote = meta("claude-code:devbox:r1", "远程会话");
    remote.key = "claude-code:devbox:r1".into();
    remote.id = "r1".into();
    remote.host = "devbox".into();
    remote.file_path = "/cache/devbox/.claude/projects/p/r1.jsonl".into();
    store
        .write_session(&remote, remote.updated_at, &[])
        .unwrap();
    let row = store.get_session("claude-code:devbox:r1").unwrap().unwrap();
    assert_eq!(row.host, "devbox");
    assert_eq!(row.id, "r1");

    let counts = store.host_counts().unwrap();
    assert_eq!(counts.get("devbox"), Some(&1));
    assert!(!counts.contains_key(""), "本地行不进 host 榜");

    // remote_hosts 配置往返:add → enabled 开关 → 同步状态 → remove
    store.add_remote_host("devbox").unwrap();
    store.add_remote_host("devbox").unwrap(); // 幂等
    let hosts = store.list_remote_hosts().unwrap();
    assert_eq!(hosts.len(), 1);
    assert!(hosts[0].enabled && hosts[0].last_sync_at.is_none());

    store.set_remote_host_enabled("devbox", false).unwrap();
    assert!(!store.list_remote_hosts().unwrap()[0].enabled);

    store
        .record_remote_sync("devbox", Some("ssh: permission denied"))
        .unwrap();
    let host = &store.list_remote_hosts().unwrap()[0];
    assert_eq!(
        host.last_sync_error.as_deref(),
        Some("ssh: permission denied")
    );
    assert!(host.last_sync_at.is_none(), "失败不得伪造成功时间");

    store.record_remote_sync("devbox", None).unwrap();
    let host = &store.list_remote_hosts().unwrap()[0];
    assert!(host.last_sync_error.is_none(), "成功清掉上次错误");
    assert!(host.last_sync_at.is_some());

    store.remove_remote_host("devbox").unwrap();
    assert!(store.list_remote_hosts().unwrap().is_empty());
}

#[test]
fn titles_are_searchable_and_tracked_with_the_session() {
    let (_dir, store) = temp_store();
    let m = meta("claude-code:t1", "部署脚本重构 deploy");
    store
        .write_session(
            &m,
            m.updated_at,
            &[
                unit(0, Role::User, "hello world"),
                unit(2, Role::Assistant, "deploy pipeline ok"),
            ],
        )
        .unwrap();

    // 标题命中:role=title、seq=0、高亮哨兵包住命中词
    let (hits, degraded) = store.search("部署脚本", &[], None, 10).unwrap();
    assert!(!degraded);
    assert_eq!(hits.len(), 1, "正文里没有这个词,只有标题命中");
    assert_eq!(hits[0].role, "title");
    assert_eq!(hits[0].seq, 0);
    assert_eq!(hits[0].session.key, "claude-code:t1");
    assert!(hits[0]
        .snippet
        .contains(&format!("{HL_OPEN}部署脚本{HL_CLOSE}")));

    // 短词走 LIKE 降级,标题同样命中
    let (hits, degraded) = store.search("部署", &[], None, 10).unwrap();
    assert!(degraded);
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].role, "title");

    // 标题与正文都命中时,标题排在前
    let (hits, _) = store.search("deploy", &[], None, 10).unwrap();
    assert_eq!(
        hits.iter().map(|h| h.role.as_str()).collect::<Vec<_>>(),
        ["title", "assistant"]
    );
    assert_eq!(hits[1].seq, 2, "正文命中的 seq 契约不受标题影响");

    // 会话侧筛选对标题命中同样生效
    let (hits, _) = store.search("deploy", &[AgentId::Codex], None, 10).unwrap();
    assert!(hits.is_empty());

    // quick 路径(write_meta_only)改标题:旧标题不再命中,新标题命中
    let mut renamed = m.clone();
    renamed.title = "数据库迁移".into();
    store.write_meta_only(&[(renamed, m.updated_at)]).unwrap();
    assert!(store
        .search("部署脚本", &[], None, 10)
        .unwrap()
        .0
        .is_empty());
    assert_eq!(
        store.search("数据库迁移", &[], None, 10).unwrap().0.len(),
        1
    );

    // 删除与重建都带走标题索引
    store.remove_session(&m.key, false).unwrap();
    assert!(store
        .search("数据库迁移", &[], None, 10)
        .unwrap()
        .0
        .is_empty());
    let again = meta("claude-code:t2", "第二个会话标题");
    store.write_meta_only(&[(again, 1)]).unwrap();
    assert_eq!(
        store.search("第二个会话", &[], None, 10).unwrap().0.len(),
        1
    );
    store.rebuild_all().unwrap();
    assert!(store
        .search("第二个会话", &[], None, 10)
        .unwrap()
        .0
        .is_empty());
}

/// 老库(titles_fts 刚由本版建出、还是空的)首次打开时把既有标题灌进去
#[test]
fn title_index_is_backfilled_for_older_databases() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.db");
    {
        let store = Store::open(&path).unwrap();
        let a = meta("claude-code:old1", "旧库里的标题一");
        let b = meta("codex:old2", "Legacy title two");
        store.write_meta_only(&[(a, 1), (b, 2)]).unwrap();
    }
    // 模拟"表刚建出来":清空标题索引,sessions 照旧
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute("DELETE FROM titles_fts", []).unwrap();
    }
    let store = Store::open(&path).unwrap();
    assert_eq!(
        store.search("旧库里的标题", &[], None, 10).unwrap().0.len(),
        1
    );
    assert_eq!(store.search("Legacy", &[], None, 10).unwrap().0.len(), 1);
}

/// 同一会话在多台 host 上各有镜像(两个远程 host 其实是同一台机器,或本地也有
/// 一份)时 Insights 只算一次——按 (agent, native id, created_at) 认同一会话,
/// 取更新最晚的那份;同 id 不同 created_at 是两个会话(Hermes 式小整数 id)
#[test]
fn insights_counts_a_session_mirrored_on_several_hosts_once() {
    let (_dir, store) = temp_store();
    let prompt = |seq: i64| unit(seq, Role::User, "prompt");
    let mirror = |key: &str, host: &str, tokens: i64| {
        let mut m = meta(key, "mirrored");
        m.id = "m1".into();
        m.host = host.into();
        m.tokens_used = Some(tokens);
        m
    };
    let local = mirror("claude-code:m1", "", 100);
    let a = mirror("claude-code:hosta:m1", "hosta", 100);
    let mut b = mirror("claude-code:hostb:m1", "hostb", 300);
    b.updated_at += 5_000; // 最新、最长的一份是统计口径
    store
        .write_session(&local, local.updated_at, &[prompt(0), prompt(2)])
        .unwrap();
    store
        .write_session(&a, a.updated_at, &[prompt(0), prompt(2)])
        .unwrap();
    store
        .write_session(&b, b.updated_at, &[prompt(0), prompt(2), prompt(4)])
        .unwrap();
    let mut seven = meta("hermes:7", "seven");
    seven.agent = AgentId::Hermes;
    let mut seven_remote = meta("hermes:hostb:7", "seven");
    seven_remote.agent = AgentId::Hermes;
    seven_remote.id = "7".into();
    seven_remote.host = "hostb".into();
    seven_remote.created_at += 1; // 不同起点:另一台机器自己的 7 号会话
    store
        .write_session(&seven, seven.updated_at, &[prompt(0)])
        .unwrap();
    store
        .write_session(&seven_remote, seven_remote.updated_at, &[prompt(0)])
        .unwrap();

    let d = store
        .insights(chrono::NaiveDate::from_ymd_opt(2026, 1, 13).unwrap())
        .unwrap();
    assert_eq!(d.sessions, 3, "三份镜像算一个会话,两个 hermes:7 是两个");
    assert_eq!(d.prompts, 3 + 1 + 1, "镜像组只计最新那份的三条 prompt");
    assert_eq!(d.tokens, 300, "tokens 同样只算最新那份");
    let claude = d
        .agents
        .iter()
        .find(|t| t.name == "claude-code")
        .expect("claude-code 榜单行");
    assert_eq!(
        (claude.sessions, claude.prompts, claude.tokens),
        (1, 3, 300)
    );
}

/// 近因加权:同等文本相关性下最近活跃的会话排前面(bm25 同分时原本按写入顺序,
/// 先写的老会话在前),标题命中与正文命中同一规则
#[test]
fn search_ranks_recently_active_sessions_higher() {
    let (_dir, store) = temp_store();
    let now = wake_core::db::now_ms();
    let day = 86_400_000;
    let mut old = meta("claude-code:old", "缓存策略讨论");
    old.updated_at = now - 400 * day;
    let mut fresh = meta("claude-code:fresh", "缓存策略讨论");
    fresh.updated_at = now - day;
    for m in [&old, &fresh] {
        store
            .write_session(
                m,
                m.updated_at,
                &[unit(0, Role::User, "缓存失效要不要加 jitter")],
            )
            .unwrap();
    }
    let (hits, _) = store.search("缓存失效", &[], None, 10).unwrap();
    assert_eq!(
        hits.iter()
            .map(|h| h.session.key.as_str())
            .collect::<Vec<_>>(),
        ["claude-code:fresh", "claude-code:old"]
    );
    let (hits, _) = store.search("缓存策略", &[], None, 10).unwrap();
    assert!(hits.iter().all(|h| h.role == "title"));
    assert_eq!(
        hits.iter()
            .map(|h| h.session.key.as_str())
            .collect::<Vec<_>>(),
        ["claude-code:fresh", "claude-code:old"]
    );
}

/// 记忆表:按 (agent, host) 整组替换——没变的不动、变了的换、多出的删;列表项目内
/// 新到旧、用户级最后且不受项目筛选影响;搜索命中带 snippet 并随删除消失;rebuild 清空
/// `limit` 只封项目级:用户级(对每个项目都成立)不限量接在后面——否则默认 50 条
/// 一刀切砍掉的正是它们;侧栏计数与列表同口径:项目行的计数含用户级,点开看到几行
/// 徽章就是几;LIKE 降级的 snippet 遇到小写不保长的字符(Ω)与全角标点不得 panic
/// (2026-09-21 review)
#[test]
fn memory_limit_keeps_user_level_and_counts_and_snippets_agree() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("t.db")).unwrap();
    let doc = |key: &str, scope: MemoryScope, project: &str, body: &str, updated: i64| MemoryDoc {
        key: key.to_string(),
        agent: AgentId::ClaudeCode,
        host: String::new(),
        scope,
        project_path: project.to_string(),
        project_name: if project.is_empty() {
            String::new()
        } else {
            "app".to_string()
        },
        session_key: String::new(),
        path: key.trim_start_matches("claude-code:").to_string(),
        title: key.to_string(),
        updated_at: updated,
        size_bytes: body.len() as i64,
        source: String::new(),
        body: body.to_string(),
    };
    let docs = vec![
        doc(
            "claude-code:/m/p1.md",
            MemoryScope::Project,
            "/work/app",
            "Ωmega，二维码在这里",
            3,
        ),
        doc(
            "claude-code:/m/p2.md",
            MemoryScope::Project,
            "/work/app",
            "second",
            2,
        ),
        doc(
            "claude-code:/m/p3.md",
            MemoryScope::Project,
            "/work/app",
            "third",
            1,
        ),
        doc(
            "claude-code:/m/u.md",
            MemoryScope::User,
            "",
            "global prefs",
            9,
        ),
    ];
    store
        .replace_memories(AgentId::ClaudeCode, "", &docs, &[])
        .unwrap();
    let keys = |v: &[MemoryDoc]| v.iter().map(|d| d.key.clone()).collect::<Vec<_>>();

    let limited = store
        .list_memories(&MemoryFilter {
            project_paths: vec!["/work/app".into()],
            limit: 2,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(
        keys(&limited),
        [
            "claude-code:/m/p1.md",
            "claude-code:/m/p2.md",
            "claude-code:/m/u.md"
        ],
        "项目级封顶 2,用户级照列"
    );
    let limited_all = store
        .list_memories(&MemoryFilter {
            limit: 1,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(
        keys(&limited_all),
        ["claude-code:/m/p1.md", "claude-code:/m/u.md"]
    );

    let counts = store.memory_counts().unwrap();
    assert_eq!(counts.total, 4);
    assert_eq!(counts.user, 1);
    assert_eq!(
        counts
            .projects
            .iter()
            .map(|p| (p.path.as_str(), p.count))
            .collect::<Vec<_>>(),
        [("/work/app", 3)],
        "项目行只数项目记忆,用户记忆有自己的一行"
    );
    // GUI 的项目行:用户记忆不混进来,徽章 == 点开的行数
    let opened = store
        .list_memories(&MemoryFilter {
            project_paths: vec!["/work/app".into()],
            user: UserMemories::Excluded,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(
        opened.len() as i64,
        counts.projects[0].count,
        "徽章 == 点开的行数"
    );
    // GUI 的 User memory 行:只列用户记忆,徽章同样 == 行数
    let user_only = store
        .list_memories(&MemoryFilter {
            user: UserMemories::Only,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(keys(&user_only), ["claude-code:/m/u.md"]);
    assert_eq!(user_only.len() as i64, counts.user);

    // 2 个码点走 LIKE 降级;Ω 小写成 ω 后字节数变了,原先拿小写串的字节偏移切原文会 panic
    let hits = store
        .search_memories("二维", &MemoryFilter::default())
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert!(hits[0].snippet.contains("二维"), "{}", hits[0].snippet);

    // since 也约束记忆命中(wake_search 说了 "in the last …" 就不能跟着旧笔记):p2 的
    // updated_at 是 2
    let since = |t: i64| MemoryFilter {
        updated_since: Some(t),
        ..Default::default()
    };
    assert!(store
        .search_memories("second", &since(3))
        .unwrap()
        .is_empty());
    assert_eq!(store.search_memories("second", &since(2)).unwrap().len(), 1);
}

/// 记忆来源配置(Settings → Memory locations)往返:自定义增删改、停用开关、
/// Restore defaults;`local_project_roots` 只给本地会话的项目根
#[test]
fn memory_source_overrides_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("t.db")).unwrap();
    store.add_memory_source("claude-code", "/notes/a").unwrap();
    store.add_memory_source("claude-code", "/notes/a").unwrap();
    store.add_memory_source("codex", "/notes/b.md").unwrap();
    store
        .set_memory_source_enabled("claude-code", "<project>/CLAUDE.md", false)
        .unwrap();
    store
        .set_memory_source_enabled("claude-code", "<project>/CLAUDE.md", false)
        .unwrap();
    let (customs, disabled) = store.memory_source_overrides().unwrap();
    assert_eq!(
        customs,
        [
            (AgentId::ClaudeCode, std::path::PathBuf::from("/notes/a")),
            (AgentId::Codex, std::path::PathBuf::from("/notes/b.md"))
        ]
    );
    assert_eq!(
        disabled,
        std::collections::HashSet::from([(AgentId::ClaudeCode, "<project>/CLAUDE.md".to_string())])
    );
    store
        .replace_memory_source("claude-code", "/notes/a", "gemini", "/notes/c")
        .unwrap();
    store.remove_memory_source("codex", "/notes/b.md").unwrap();
    store
        .set_memory_source_enabled("claude-code", "<project>/CLAUDE.md", true)
        .unwrap();
    let (customs, disabled) = store.memory_source_overrides().unwrap();
    assert_eq!(
        customs,
        [(AgentId::Gemini, std::path::PathBuf::from("/notes/c"))]
    );
    assert!(disabled.is_empty());
    store.clear_memory_source_overrides().unwrap();
    assert!(store.memory_source_overrides().unwrap().0.is_empty());

    // 本地项目根:远程会话与没有项目的会话不算
    let mut local = meta("claude-code:local-1", "local");
    local.project_path = "/work/app".into();
    let mut remote = meta("codex:devbox:r-1", "remote");
    remote.agent = AgentId::Codex;
    remote.host = "devbox".into();
    remote.project_path = "/home/me/app".into();
    let mut orphan = meta("codex:o-1", "orphan");
    orphan.agent = AgentId::Codex;
    orphan.project_path = String::new();
    for s in [&local, &remote, &orphan] {
        store
            .write_session_guarded(s, 1, &[], &[], &|_| 0, None)
            .unwrap();
    }
    assert_eq!(
        store.local_project_roots().unwrap(),
        [std::path::PathBuf::from("/work/app")]
    );
}

#[test]
fn memories_replace_list_and_search() {
    let (_dir, store) = temp_store();
    let doc =
        |key: &str, scope: MemoryScope, project: &str, title: &str, body: &str, updated: i64| {
            MemoryDoc {
                key: key.to_string(),
                agent: AgentId::ClaudeCode,
                host: String::new(),
                scope,
                project_path: project.to_string(),
                project_name: project.rsplit('/').next().unwrap_or("").to_string(),
                session_key: String::new(),
                path: key.trim_start_matches("claude-code:").to_string(),
                title: title.to_string(),
                updated_at: updated,
                size_bytes: body.len() as i64,
                source: String::new(),
                body: body.to_string(),
            }
        };
    let a = doc(
        "claude-code:/m/a.md",
        MemoryScope::Project,
        "/work/app",
        "Scanner notes",
        "The scanner finale guard must always fire.",
        100,
    );
    let b = doc(
        "claude-code:/m/b.md",
        MemoryScope::Project,
        "/work/app",
        "Style",
        "Four-space indent, no tabs.",
        200,
    );
    let u = doc(
        "claude-code:/m/user.md",
        MemoryScope::User,
        "",
        "Preferences",
        "Concise replies.",
        50,
    );
    let all_three = [a.clone(), b.clone(), u.clone()];
    assert!(store
        .replace_memories(AgentId::ClaudeCode, "", &all_three, &[])
        .unwrap());
    assert!(
        !store
            .replace_memories(AgentId::ClaudeCode, "", &all_three, &[])
            .unwrap(),
        "没变不算改动"
    );

    let keys = |docs: &[MemoryDoc]| docs.iter().map(|d| d.key.clone()).collect::<Vec<_>>();
    assert_eq!(
        keys(&store.list_memories(&MemoryFilter::default()).unwrap()),
        [
            "claude-code:/m/b.md",
            "claude-code:/m/a.md",
            "claude-code:/m/user.md"
        ],
        "项目内新到旧,用户级最后"
    );
    let scoped = store
        .list_memories(&MemoryFilter {
            project_paths: vec!["/other".into()],
            ..Default::default()
        })
        .unwrap();
    assert_eq!(
        keys(&scoped),
        ["claude-code:/m/user.md"],
        "项目筛选下用户级照列"
    );
    assert_eq!(
        store
            .get_memory("claude-code:/m/a.md")
            .unwrap()
            .unwrap()
            .body,
        a.body
    );

    let hits = store
        .search_memories("finale guard", &MemoryFilter::default())
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].doc.key, a.key);
    assert!(hits[0].snippet.contains("finale"), "{}", hits[0].snippet);
    let degraded = store
        .search_memories("no", &MemoryFilter::default())
        .unwrap();
    assert!(
        degraded.iter().any(|h| h.doc.key == b.key),
        "<3 码点走 LIKE: {degraded:?}"
    );

    // b 变了、u 消失:替换后 b 是新正文,u 从列表与搜索里都不见
    let mut b2 = b.clone();
    b2.body = "Tabs are fine now.".into();
    b2.updated_at = 300;
    b2.size_bytes = b2.body.len() as i64;
    assert!(store
        .replace_memories(AgentId::ClaudeCode, "", &[a.clone(), b2.clone()], &[])
        .unwrap());
    assert_eq!(
        store.list_memories(&MemoryFilter::default()).unwrap().len(),
        2
    );
    assert_eq!(store.get_memory(&b.key).unwrap().unwrap().body, b2.body);
    assert!(
        store
            .search_memories("Concise", &MemoryFilter::default())
            .unwrap()
            .is_empty(),
        "删掉的记忆不再命中"
    );

    store.rebuild_all().unwrap();
    assert!(store
        .list_memories(&MemoryFilter::default())
        .unwrap()
        .is_empty());
}

/// 项目归属在读库时按 session_key 连 sessions 解析:会话晚于记忆入库、会话换了项目,
/// 记忆都自动跟上;没有锚点也没有项目的落 Unknown,项目筛选捞不到它
#[test]
fn memories_resolve_their_project_through_the_anchor_session() {
    let (_dir, store) = temp_store();
    let anchored = MemoryDoc {
        key: "claude-code:/m/anchored.md".to_string(),
        agent: AgentId::ClaudeCode,
        host: String::new(),
        scope: MemoryScope::Project,
        project_path: String::new(),
        project_name: String::new(),
        session_key: "claude-code:s1".to_string(),
        path: "/m/anchored.md".to_string(),
        title: "Anchored".to_string(),
        updated_at: 10,
        size_bytes: 4,
        source: String::new(),
        body: "body".to_string(),
    };
    let orphan = MemoryDoc {
        key: "claude-code:/m/orphan.md".to_string(),
        session_key: String::new(),
        path: "/m/orphan.md".to_string(),
        title: "Orphan".to_string(),
        ..anchored.clone()
    };
    store
        .replace_memories(AgentId::ClaudeCode, "", &[anchored.clone(), orphan], &[])
        .unwrap();
    let project_of = |key: &str| store.get_memory(key).unwrap().unwrap().project_path;
    assert_eq!(project_of(&anchored.key), "", "会话还没入库:暂时没有项目");

    // 会话入库(晚于记忆)——记忆立刻有了项目,写入时没有定格
    let mut s1 = meta("claude-code:s1", "Session one");
    s1.project_path = "/work/app".into();
    s1.project_name = "app".into();
    store
        .write_session_guarded(&s1, 5, &[], &[], &|_| 0, None)
        .unwrap();
    assert_eq!(project_of(&anchored.key), "/work/app");
    assert_eq!(
        store
            .list_memories(&MemoryFilter::default())
            .unwrap()
            .iter()
            .map(|d| d.key.as_str())
            .collect::<Vec<_>>(),
        [anchored.key.as_str(), "claude-code:/m/orphan.md"],
        "没归属的一组排在项目之后"
    );
    let scoped = store
        .list_memories(&MemoryFilter {
            project_paths: vec!["/work/app".into()],
            ..Default::default()
        })
        .unwrap();
    assert_eq!(
        scoped.iter().map(|d| d.key.as_str()).collect::<Vec<_>>(),
        [anchored.key.as_str()],
        "项目筛选按解析出的项目;没归属的不算任何项目"
    );
    assert_eq!(scoped[0].project_name, "app");
    let unknown = store
        .list_memories(&MemoryFilter {
            unattributed: true,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(
        unknown.iter().map(|d| d.key.as_str()).collect::<Vec<_>>(),
        ["claude-code:/m/orphan.md"],
        "空串筛的是没归属的那一组(侧栏 Unknown project 行)"
    );
    // 侧栏计数与列表同一口径:总数、按 agent、按解析出的项目(没归属的垫底)
    let counts = store.memory_counts().unwrap();
    assert_eq!(counts.total, 2);
    assert_eq!(counts.user, 0);
    assert_eq!(counts.agents, vec![(AgentId::ClaudeCode, 2)]);
    assert_eq!(
        counts
            .projects
            .iter()
            .map(|p| (p.path.as_str(), p.name.as_str(), p.count))
            .collect::<Vec<_>>(),
        [("/work/app", "app", 1), ("", "", 1)]
    );

    // 会话换了项目,记忆跟着走
    s1.project_path = "/work/app-renamed".into();
    s1.project_name = "app-renamed".into();
    store
        .write_session_guarded(&s1, 6, &[], &[], &|_| 0, None)
        .unwrap();
    assert_eq!(project_of(&anchored.key), "/work/app-renamed");

    // 锚点换了(目录里来了更新的会话)是改动,同 key 重写;正文与时间没变也要写。
    // 同 key 重复(同家两个实例的根重叠、自定义目录盖住默认文件)**前者胜**——docs 按
    // 来源计划的顺序读入,先到的是默认那份;后面那份与库里相同也不算数
    let mut moved = anchored.clone();
    moved.session_key = "claude-code:s2".to_string();
    assert!(store
        .replace_memories(
            AgentId::ClaudeCode,
            "",
            &[moved.clone(), anchored.clone()],
            &[]
        )
        .unwrap());
    assert_eq!(
        store.get_memory(&moved.key).unwrap().unwrap().session_key,
        "claude-code:s2"
    );
    assert!(
        store
            .search_memories("body", &MemoryFilter::default())
            .unwrap()
            .iter()
            .all(|h| h.doc.key == moved.key),
        "重写后 FTS 里只剩这一份、没有旧行残留"
    );
}

/// 跨进程写锁的契约:同一路径第二把拿不到、拿不到时点得出持有者(GUI 与否)、放了
/// 就能拿;等待策略只等 CLI 持有者,遇 GUI 立刻放弃。flock 与 LockFileEx 都按打开的
/// 文件描述计,进程内第二次 open 就能演
#[test]
fn index_lock_is_exclusive_names_its_holder_and_waits_only_for_the_cli() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("test.db");
    let app = common::lock_as(&db, db::LOCK_KIND_APP);
    match IndexLock::try_acquire(&db, "wake-cli refresh").unwrap() {
        Ownership::Ours(_) => panic!("the second acquisition must fail while the first is held"),
        Ownership::Held(who) => {
            assert!(who.app, "{who}");
            assert_eq!(who.to_string(), format!("Wake {}", std::process::id()));
        }
    }
    // GUI 持有:不等,立刻报
    let t0 = Instant::now();
    assert!(matches!(
        IndexLock::acquire_or_wait(&db, "wake-cli refresh", Duration::from_secs(5)).unwrap(),
        Wait::HeldByApp(_)
    ));
    assert!(t0.elapsed() < Duration::from_secs(1), "遇 GUI 不该等");
    drop(app);

    // CLI 持有:等它放
    let cli = common::lock_as(&db, "wake-cli refresh");
    match IndexLock::try_acquire(&db, db::LOCK_KIND_APP).unwrap() {
        Ownership::Held(who) => assert!(!who.app, "{who}"),
        Ownership::Ours(_) => panic!("the CLI holder should block the app"),
    }
    let releaser = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        drop(cli);
    });
    match IndexLock::acquire_or_wait(&db, db::LOCK_KIND_APP, Duration::from_secs(5)).unwrap() {
        Wait::Ours(_) => {}
        Wait::HeldByApp(who) | Wait::TimedOut(who) => panic!("should have waited out {who}"),
    }
    releaser.join().unwrap();

    // 等满了还是别人的
    let _cli = common::lock_as(&db, "wake-cli index");
    assert!(matches!(
        IndexLock::acquire_or_wait(&db, db::LOCK_KIND_APP, Duration::from_millis(250)).unwrap(),
        Wait::TimedOut(_)
    ));
}

/// `--db` 给符号链接别名也得撞上同一把锁:锁文件按库的真实路径派生(Codex review
/// 2026-09-23)。Windows 建符号链接要特权,只在 unix 演
#[cfg(unix)]
#[test]
fn index_lock_follows_the_database_through_symlinks() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("wake.db");
    std::fs::write(&db, b"").unwrap();
    let alias = dir.path().join("alias.db");
    std::os::unix::fs::symlink(&db, &alias).unwrap();
    assert_eq!(IndexLock::path_for(&alias), IndexLock::path_for(&db));
    let _app = common::lock_as(&db, db::LOCK_KIND_APP);
    match IndexLock::try_acquire(&alias, "wake-cli refresh").unwrap() {
        Ownership::Held(who) => assert!(who.app, "{who}"),
        Ownership::Ours(_) => panic!("the alias must share the app's lock"),
    }
    // 库还不存在时按父目录解析:目录的别名同样撞上
    let dir_alias = dir.path().join("dir-alias");
    std::os::unix::fs::symlink(dir.path(), &dir_alias).unwrap();
    assert_eq!(
        IndexLock::path_for(&dir_alias.join("none.db")),
        IndexLock::path_for(&dir.path().join("none.db"))
    );
}

/// `db_dir()` 是"打开的那个路径"所在目录,**不解析符号链接**——远程镜像 `remotes/<host>`
/// 挂在它下面,把库链到别的盘、链接留在默认位置的用户不能因升级换镜像目录;锁文件才按真实
/// 文件落(Codex review 2026-09-23)。相对 `--db` 补成绝对那一支没法在并行测试里演
/// (cwd 是进程级状态)
#[test]
fn store_keeps_the_directory_it_was_opened_at() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("x.db")).unwrap();
    assert_eq!(store.db_dir().unwrap(), dir.path());
    drop(store);
    let ro = Store::open_read_only(&dir.path().join("x.db")).unwrap();
    assert_eq!(ro.db_dir().unwrap(), dir.path());
    #[cfg(unix)]
    {
        let alias = dir.path().join("alias");
        std::os::unix::fs::symlink(dir.path(), &alias).unwrap();
        let via_alias = Store::open(&alias.join("x.db")).unwrap();
        assert_eq!(
            via_alias.db_dir().unwrap(),
            alias,
            "镜像目录跟着打开的路径走"
        );
        assert_eq!(
            IndexLock::path_for(&alias.join("x.db")),
            IndexLock::path_for(&dir.path().join("x.db")),
            "锁按真实文件落"
        );
    }
}

/// 持有者自述是旁路文件、写在拿到锁之后,可能残留上一任 GUI 的 "Wake …";"是不是 GUI"
/// 只认 `.lock.app` 那把锁(随进程生死)。这里主锁被一个从不写自述、也不持 `.lock.app`
/// 的持有者拿着,残留的自述不能把新起的 GUI 劝退(Codex review 2026-09-23)
#[test]
fn a_stale_app_holder_note_does_not_turn_the_gui_away() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("test.db");
    let lock_path = IndexLock::path_for(&db);
    // 与 IndexLock 同一条命名规则:旁路文件 = 锁文件名 + ".holder"
    std::fs::write(format!("{}.holder", lock_path.display()), "Wake 1").unwrap();
    let raw = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .unwrap();
    raw.try_lock().unwrap();
    match IndexLock::try_acquire(&db, "wake-cli refresh").unwrap() {
        Ownership::Held(who) => {
            assert!(!who.app, "残留自述不算 GUI:{who}");
            assert_eq!(who.to_string(), "Wake 1", "点名仍用自述");
        }
        Ownership::Ours(_) => panic!("the raw holder should block"),
    }
    let releaser = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        drop(raw);
    });
    match IndexLock::acquire_or_wait(&db, db::LOCK_KIND_APP, Duration::from_secs(5)).unwrap() {
        Wait::Ours(_) => {}
        Wait::HeldByApp(who) => panic!("turned away by a stale note: {who}"),
        Wait::TimedOut(who) => panic!("should have waited out the holder: {who}"),
    }
    releaser.join().unwrap();
}

/// 默认路径是**悬空**符号链接(目标清掉后再启动):锁要落在目标旁,`Store::open` 沿链接
/// 建库正建在那里,之后按同一路径解析出的是同一把(Codex review 2026-09-23)
#[cfg(unix)]
#[test]
fn index_lock_follows_a_dangling_symlink_to_its_target() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("real").join("wake.db");
    std::fs::create_dir_all(target.parent().unwrap()).unwrap();
    let link = dir.path().join("wake.db");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let before = IndexLock::path_for(&link);
    assert_eq!(before, IndexLock::path_for(&target));
    let store = Store::open(&link).unwrap();
    assert!(target.is_file(), "沿链接建库应建在目标处");
    assert_eq!(store.db_dir().unwrap(), dir.path(), "镜像目录留在链接旁");
    assert_eq!(IndexLock::path_for(&link), before, "建库前后锁的位置不能变");
}

/// 探 `.lock.app` 用共享锁:多个探测者同时探(几个 CLI、或 CLI 撞上正在启动的 GUI)
/// 不能互相当成 GUI。主锁由一个不持 `.lock.app` 的持有者拿着,四个线程各探几十次,
/// 谁都不该看到 "GUI 在"(Codex review 2026-09-23)
#[test]
fn concurrent_probes_never_mistake_each_other_for_the_gui() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("test.db");
    let raw = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(IndexLock::path_for(&db))
        .unwrap();
    raw.try_lock().unwrap();
    let probers: Vec<_> = (0..4)
        .map(|_| {
            let db = db.clone();
            std::thread::spawn(move || {
                for _ in 0..40 {
                    match IndexLock::try_acquire(&db, "wake-cli refresh").unwrap() {
                        Ownership::Held(who) => {
                            assert!(!who.app, "a probe was taken for the GUI: {who}")
                        }
                        Ownership::Ours(_) => panic!("the raw holder should block"),
                    }
                }
            })
        })
        .collect();
    for p in probers {
        p.join().unwrap();
    }
    drop(raw);
}

/// 主锁被占是已经确认的事实,探 `.lock.app` 出错(这里让它是个目录,open 必败)不能把
/// "被占"翻成 I/O 错——GUI 对拿锁出错的对策是无锁启动。保守按"被占、不是 GUI"报
/// (Codex review 2026-09-23)
#[test]
fn a_failing_app_probe_still_reports_the_index_as_held() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("test.db");
    let lock_path = IndexLock::path_for(&db);
    std::fs::create_dir_all(format!("{}.app", lock_path.display())).unwrap();
    let raw = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .unwrap();
    raw.try_lock().unwrap();
    match IndexLock::try_acquire(&db, "wake-cli refresh").unwrap() {
        Ownership::Held(who) => assert!(!who.app, "{who}"),
        Ownership::Ours(_) => panic!("the raw holder should block"),
    }
    drop(raw);
    // 主锁到手、`.lock.app` 拿不到:GUI 照样算拿到,别人只是认不出它是 GUI
    match IndexLock::try_acquire(&db, db::LOCK_KIND_APP).unwrap() {
        Ownership::Ours(_) => {}
        Ownership::Held(who) => panic!("free lock reported held by {who}"),
    }
}

/// 自述文件写不进去(这里让它是个目录)不能把刚到手的主锁放掉:锁照拿,别人只是点不出名
/// (Codex review 2026-09-23)
#[test]
fn an_unwritable_holder_note_does_not_forfeit_the_lock() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("test.db");
    let lock_path = IndexLock::path_for(&db);
    std::fs::create_dir_all(format!("{}.holder", lock_path.display())).unwrap();
    let _held = match IndexLock::try_acquire(&db, db::LOCK_KIND_APP).unwrap() {
        Ownership::Ours(lock) => lock,
        Ownership::Held(who) => panic!("free lock reported held by {who}"),
    };
    match IndexLock::try_acquire(&db, "wake-cli refresh").unwrap() {
        Ownership::Held(who) => {
            assert!(who.app, "GUI 的第二把锁照拿:{who}");
            assert_eq!(
                who.to_string(),
                "another process",
                "点不出名就说 another process"
            );
        }
        Ownership::Ours(_) => panic!("the lock must still be held"),
    }
}

/// 外壳产品的认领(claimed_sessions 表):认领落地即删掉已入库的替身(连 FTS 一起)、
/// 两条写库路径都挡住它、认领撤销后能照常写回;认领方清单给"roster 里已经没有的
/// 认领方整组撤销"用
#[test]
fn claims_hide_the_copy_until_released() {
    let (_dir, store) = temp_store();
    let copy = meta("claude-code:engine-1", "引擎那份转录");
    store
        .write_session(
            &copy,
            copy.updated_at,
            &[unit(0, Role::User, "二维码扫描崩了")],
        )
        .unwrap();
    let never_ranks = |_: &str| 0u8;

    assert!(store
        .replace_claims(AgentId::CraftAgents, &[copy.key.clone()])
        .unwrap());
    assert!(
        store.get_session(&copy.key).unwrap().is_none(),
        "替身没出库"
    );
    assert!(
        store.search("二维码", &[], None, 10).unwrap().0.is_empty(),
        "替身的正文还在 FTS 里"
    );
    assert!(store.is_key_claimed(&copy.key));
    assert_eq!(store.claimants().unwrap(), vec![AgentId::CraftAgents]);
    // 全量 / 增量共用的写入闸门与 quick 路径都挡
    assert!(!store
        .write_session_guarded(&copy, copy.updated_at, &[], &[], &never_ranks, None)
        .unwrap());
    store.write_meta_only(&[(copy.clone(), 0)]).unwrap();
    assert!(store.get_session(&copy.key).unwrap().is_none());
    // 同一快照再对一遍:没有变化
    assert!(!store
        .replace_claims(AgentId::CraftAgents, &[copy.key.clone()])
        .unwrap());

    // 撤销(认领方会话没了):替身可以写回
    assert!(store.replace_claims(AgentId::CraftAgents, &[]).unwrap());
    assert!(!store.is_key_claimed(&copy.key));
    assert!(store.claimants().unwrap().is_empty());
    assert!(store
        .write_session_guarded(&copy, copy.updated_at, &[], &[], &never_ranks, None)
        .unwrap());
    assert!(store.get_session(&copy.key).unwrap().is_some());
}
