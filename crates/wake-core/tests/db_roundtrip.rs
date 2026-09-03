//! Store 写入/搜索/删除语义的往返测试(临时库,不碰真实索引)。
//! 覆盖 CLAUDE.md 不变量 3:tombstone 防复活、user_data 独立表重建不丢。

use wake_core::db::Store;
use wake_core::models::*;

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
        project_path: None,
        favorite_only: false,
        include_archived: false,
        roots_only: false,
        title_query: None,
        sort: SortKey::Updated,
        ascending: false,
        limit: 10,
        offset: 0,
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
            .list_projects()
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
        !store.write_session_guarded(&loser, 5, &[]).unwrap(),
        "败方不该写入"
    );
    assert_eq!(
        store.get_session("codex:g").unwrap().unwrap().file_path,
        "/live/g.jsonl"
    );

    assert!(
        store.write_session_guarded(&loser, 12, &[]).unwrap(),
        "反超应接管"
    );
    assert_eq!(
        store.get_session("codex:g").unwrap().unwrap().file_path,
        "/backup/g.jsonl"
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
    assert_eq!(d.daily_sessions, vec![(created_day, 2, 500)]);
    // 趋势:as_of=1/13(周二)→ 本周从 1/12 起是末列 52;1/7、1/10、1/11 落在
    // 1/5 那周 = 51。未来行与无 ts 行不进周桶
    let claude = &d.trend_agents[0];
    assert_eq!((claude.name.as_str(), claude.total), ("claude-code", 4));
    assert_eq!((claude.weekly[51], claude.weekly[52]), (3, 1));
    assert_eq!(claude.weekly.len(), TREND_WEEKS);
    let codex = &d.trend_agents[1];
    assert_eq!((codex.name.as_str(), codex.total), ("codex", 2));
    assert_eq!(codex.weekly[51], 2);
    assert_eq!(
        d.trend_models
            .iter()
            .map(|s| (s.name.as_str(), s.total))
            .collect::<Vec<_>>(),
        vec![("claude-opus", 4), ("gpt-5-codex", 2)]
    );
    // Last 7 days(1/7–1/13)对前 7 天(12/31–1/6):prompts 6 / 0,活跃日 4 / 0;
    // 会话创建在 2023,两窗都是 0
    let (cur, prev) = d.last_week_pair();
    assert_eq!(
        cur,
        WindowStats {
            sessions: 0,
            prompts: 6,
            tokens: 0,
            active_days: 4
        }
    );
    assert_eq!(prev, WindowStats::default());

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
