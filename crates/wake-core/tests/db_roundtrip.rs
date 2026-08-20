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

    // 重建索引(sessions/messages 清空重来)后,收藏/置顶必须还在
    store.rebuild_all().unwrap();
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
        title_query: None,
        sort: SortKey::Updated,
        ascending: false,
        limit: 10,
        offset: 0,
        roots_only: false,
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

fn grok_meta(key: &str, title: &str, updated: i64) -> SessionMeta {
    let mut m = meta(key, title);
    m.agent = AgentId::Grok;
    m.updated_at = updated;
    m.created_at = updated;
    m
}

#[test]
fn apply_parent_links_diffs_and_is_idempotent() {
    let (_dir, store) = temp_store();
    let p = grok_meta("grok:orch", "调研终端", 100);
    let c = grok_meta("grok:child", "PR-9", 200);
    store.write_session(&p, p.updated_at, &[]).unwrap();
    store.write_session(&c, c.updated_at, &[]).unwrap();

    let changed = store
        .apply_parent_links(AgentId::Grok, &[("grok:child".into(), "grok:orch".into())])
        .unwrap();
    assert!(changed);
    assert_eq!(
        store.parent_key_of("grok:child").unwrap().as_deref(),
        Some("grok:orch")
    );
    let changed = store
        .apply_parent_links(AgentId::Grok, &[("grok:child".into(), "grok:orch".into())])
        .unwrap();
    assert!(!changed, "same links must not write");

    let changed = store.apply_parent_links(AgentId::Grok, &[]).unwrap();
    assert!(changed);
    assert_eq!(store.parent_key_of("grok:child").unwrap(), None);
}

#[test]
fn roots_only_hides_children_unless_parent_missing() {
    let (_dir, store) = temp_store();
    let p = grok_meta("grok:orch", "调研终端", 50);
    let c = grok_meta("grok:child", "PR-9", 200);
    let orphan = grok_meta("grok:orphan", "无父孩子", 80);
    store.write_session(&p, p.updated_at, &[]).unwrap();
    store.write_session(&c, c.updated_at, &[]).unwrap();
    store
        .write_session(&orphan, orphan.updated_at, &[])
        .unwrap();
    store
        .apply_parent_links(
            AgentId::Grok,
            &[
                ("grok:child".into(), "grok:orch".into()),
                ("grok:orphan".into(), "grok:gone".into()),
            ],
        )
        .unwrap();

    let filter = SessionFilter {
        roots_only: true,
        sort: SortKey::Updated,
        ascending: false,
        limit: 10,
        ..Default::default()
    };
    let (sessions, total) = store.list_sessions(&filter).unwrap();
    let keys: Vec<_> = sessions.iter().map(|s| s.key.as_str()).collect();
    assert!(
        !keys.contains(&"grok:child"),
        "linked child must not be a root"
    );
    assert!(keys.contains(&"grok:orch"));
    assert!(keys.contains(&"grok:orphan"), "orphan stays visible");
    assert_eq!(sessions[0].key, "grok:orch", "child activity lifts parent");
    assert_eq!(total, 2);

    let newer = grok_meta("grok:newer", "PR-15", 300);
    store.write_session(&newer, newer.updated_at, &[]).unwrap();
    store
        .apply_parent_links(
            AgentId::Grok,
            &[
                ("grok:child".into(), "grok:orch".into()),
                ("grok:newer".into(), "grok:orch".into()),
                ("grok:orphan".into(), "grok:gone".into()),
            ],
        )
        .unwrap();
    let kids = store
        .list_children("grok:orch", SortKey::Updated, false)
        .unwrap();
    assert_eq!(
        kids.iter().map(|s| s.key.as_str()).collect::<Vec<_>>(),
        vec!["grok:newer", "grok:child"]
    );
    let counts = store.child_counts().unwrap();
    assert_eq!(counts.get("grok:orch").copied(), Some(2));

    // 侧栏 Agents/Projects 只数顶层,不把 2 个子会话叠进 grok 总数
    let agents = store.agent_counts().unwrap();
    assert_eq!(
        agents.get("grok").copied(),
        Some(2),
        "orch + orphan, not children"
    );
    let projects = store.list_projects().unwrap();
    assert_eq!(projects.len(), 1);
    assert_eq!(projects[0].session_count, 2);
    assert_eq!(
        projects[0].last_active, 300,
        "child activity still lifts project"
    );
}

#[test]
fn upsert_does_not_clobber_parent_key() {
    let (_dir, store) = temp_store();
    let p = grok_meta("grok:orch", "调研终端", 100);
    let mut c = grok_meta("grok:child", "PR-9", 200);
    store.write_session(&p, p.updated_at, &[]).unwrap();
    store.write_session(&c, c.updated_at, &[]).unwrap();
    store
        .apply_parent_links(AgentId::Grok, &[("grok:child".into(), "grok:orch".into())])
        .unwrap();

    c.title = "PR-9 改标题".into();
    c.updated_at = 400;
    store.write_session(&c, c.updated_at, &[]).unwrap();
    assert_eq!(
        store.parent_key_of("grok:child").unwrap().as_deref(),
        Some("grok:orch"),
        "watcher 重解析不得冲掉旁写的父子链"
    );
    let got = store.get_session("grok:child").unwrap().unwrap();
    assert_eq!(got.title, "PR-9 改标题");
}

#[test]
fn all_children_includes_archived() {
    let (_dir, store) = temp_store();
    let p = grok_meta("grok:orch", "调研终端", 100);
    let c1 = grok_meta("grok:c1", "A", 200);
    let mut c2 = grok_meta("grok:c2", "B", 300);
    c2.archived = true;
    store.write_session(&p, p.updated_at, &[]).unwrap();
    store.write_session(&c1, c1.updated_at, &[]).unwrap();
    store.write_session(&c2, c2.updated_at, &[]).unwrap();
    store
        .apply_parent_links(
            AgentId::Grok,
            &[
                ("grok:c1".into(), "grok:orch".into()),
                ("grok:c2".into(), "grok:orch".into()),
            ],
        )
        .unwrap();

    let kids = store.all_children("grok:orch").unwrap();
    let mut keys: Vec<_> = kids.iter().map(|s| s.key.as_str()).collect();
    keys.sort();
    assert_eq!(keys, vec!["grok:c1", "grok:c2"]);
    assert_eq!(
        store
            .list_children("grok:orch", SortKey::Updated, false)
            .unwrap()
            .len(),
        1
    );

    store.remove_session("grok:orch", true).unwrap();
    store.remove_session("grok:c1", true).unwrap();
    store.remove_session("grok:c2", true).unwrap();
    assert!(store.get_session("grok:orch").unwrap().is_none());
    assert!(store.get_session("grok:c1").unwrap().is_none());
    assert!(store.is_tombstoned(&c1.file_path));
}
