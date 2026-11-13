use super::*;
use crate::db::Store;
use crate::scanner::{run_scan, scan_files, NullEvents};
use serde_json::json;

fn write(path: &Path, rows: &[Value]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        path,
        rows.iter().map(|r| format!("{r}\n")).collect::<String>(),
    )
    .unwrap();
}
fn adapter(root: &Path) -> CodebuddyAdapter {
    CodebuddyAdapter {
        agent: AgentId::Codebuddy,
        root: root.join("projects"),
        home: Some(root.into()),
        catalog: None,
    }
}
fn user(text: &str, ts: i64) -> Value {
    json!({"type":"message","role":"user","timestamp":ts,"content":[{"type":"input_text","text":text}]})
}

#[test]
fn mixed_input_survives_and_framework_events_never_become_prompts() {
    let tmp = tempfile::tempdir().unwrap();
    let a = adapter(tmp.path());
    let path = a.root.join("demo/chat.jsonl");
    let mut first = user("<system-reminder data-role=\"user-context\">private context</system-reminder>\nfirst real question\n  second line", 1700000000000);
    first["providerData"] = json!({"startsNewUserRequest":true});
    let mut teammate = user(
        "<teammate-message>agent report</teammate-message>",
        1700000000001,
    );
    teammate["providerData"] = json!({"teammateMessage":{"teammateId":"worker"}});
    let mut hook = user("an automatically injected followup", 1700000000002);
    hook["providerData"] = json!({"isMeta":true});
    let mut compact = user("<cb_summary>generated summary</cb_summary>", 1700000000003);
    compact["providerData"] = json!({"isCompactInternal":true});
    write(
        &path,
        &[
            first,
            teammate,
            hook,
            compact,
            user(
                "<local-command-stdout>Goal set: original request</local-command-stdout>",
                1700000000004,
            ),
            json!({"type":"message","role":"user","content":[
            {"type":"input_text","text":"<system-reminder>another context</system-reminder>"},
            {"type":"input_text","text":"<command-message>goal</command-message><command-name>/goal</command-name><command-args>real goal</command-args>"}]}),
            json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":"actual reply"}]}),
        ],
    );
    let r = a.file_ref(&path).unwrap();
    let parsed = a.parse_session(&r).unwrap();
    let transcript = a.parse_transcript(&r).unwrap();
    assert_eq!(
        parsed
            .units
            .iter()
            .map(|u| u.text.as_str())
            .collect::<Vec<_>>(),
        [
            "first real question\n  second line",
            "/goal real goal",
            "actual reply"
        ]
    );
    assert_eq!(parsed.meta.message_count, 3);
    for unit in &parsed.units {
        assert_eq!(transcript.mainline[unit.seq as usize].text, unit.text);
    }
    assert_eq!(transcript.mainline[3].kind, MessageKind::CompactSummary);
    let store = Store::open(&tmp.path().join("wake.db")).unwrap();
    store
        .write_session(&parsed.meta, r.mtime_ms, &parsed.units)
        .unwrap();
    assert_eq!(
        store
            .search("first real question", &[], None, 10)
            .unwrap()
            .0[0]
            .seq,
        0
    );
    assert!(store
        .search("agent report", &[], None, 10)
        .unwrap()
        .0
        .is_empty());
}

#[test]
fn blobs_resolve_inside_the_source_home_and_preserve_image_only_input() {
    let tmp = tempfile::tempdir().unwrap();
    let a = adapter(tmp.path());
    let path = a.root.join("demo/chat.jsonl");
    let id = "a".repeat(64);
    let blob = json!({"type":"image_blob_ref","blob_id":id,"mime":"image/png",
        "blob_path":format!("/remote/home/.codebuddy/blobs/aa/{id}.png")});
    let local = tmp.path().join(format!("blobs/aa/{id}.png"));
    fs::create_dir_all(local.parent().unwrap()).unwrap();
    fs::write(&local, b"\x89PNG\r\n\x1a\n").unwrap();
    write(
        &path,
        &[json!({"type":"message","role":"user","content":[
        {"type":"input_text","text":"<system-reminder>context</system-reminder>"},blob,
        {"type":"input_text","text":"<image_local_path>/remote/temp/image.png</image_local_path>"}]})],
    );
    let r = a.file_ref(&path).unwrap();
    let p = a.parse_transcript(&r).unwrap();
    assert_eq!(p.mainline[0].kind, MessageKind::Text);
    assert!(p.mainline[0].text.is_empty());
    assert_eq!(p.mainline[0].images.len(), 1);
    assert_eq!(a.parse_session(&r).unwrap().units[0].seq, p.mainline[0].seq);
    fs::remove_file(&local).unwrap();
    assert!(a.parse_transcript(&r).unwrap().mainline[0]
        .text
        .contains(IMAGE_PLACEHOLDER));
    #[cfg(unix)]
    {
        let outside = tmp.path().join("outside.png");
        fs::write(&outside, b"\x89PNG\r\n\x1a\n").unwrap();
        std::os::unix::fs::symlink(outside, &local).unwrap();
        assert!(a.parse_transcript(&r).unwrap().mainline[0]
            .images
            .is_empty());
    }
    assert!(super::super::codebuddy_content::blob_relative(
        &json!({"type":"image_blob_ref","blob_id":"../../outside","blob_path":"outside.png"})
    )
    .is_none());
}

#[test]
fn metadata_stubs_leave_the_index_and_reappear_when_input_arrives() {
    let tmp = tempfile::tempdir().unwrap();
    let a = adapter(tmp.path());
    let path = a.root.join("demo/empty.jsonl");
    write(&path, &[json!({"type":"session-meta","meta":{}})]);
    let r = SessionFileRef {
        agent: AgentId::Codebuddy,
        native_id: "empty".into(),
        file_path: path.to_string_lossy().into(),
        mtime_ms: 0,
        size: 1,
    };
    let old = a.parse_session(&r).unwrap();
    let store = Arc::new(Store::open(&tmp.path().join("wake.db")).unwrap());
    store.write_session(&old.meta, 0, &old.units).unwrap();
    let adapters: Vec<Box<dyn AgentAdapter>> = vec![Box::new(a)];
    run_scan(&adapters, &store, &NullEvents, false).unwrap();
    assert!(path.is_file());
    assert!(store.get_session("codebuddy:empty").unwrap().is_none());
    assert!(adapters[0].file_ref(&path).is_none());
    write(&path, &[user("new conversation", 1700000000000)]);
    scan_files(
        &adapters,
        &store,
        &NullEvents,
        vec![adapters[0].file_ref(&path).unwrap()],
    );
    assert!(store.get_session("codebuddy:empty").unwrap().is_some());
    fs::write(&path, "{unfinished").unwrap();
    assert!(
        adapters[0].file_ref(&path).is_some(),
        "a partial write is not an empty session"
    );
}

#[test]
fn btw_refreshes_from_wal_without_changing_jsonl_and_search_seq_matches_detail() {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("codebuddy.db");
    super::compat_tests::catalog(&db_path);
    let db = rusqlite::Connection::open(&db_path).unwrap();
    db.execute_batch("PRAGMA journal_mode=WAL; UPDATE conversations SET runtime_provider='cbc',runtime_session_id='runtime-5'; CREATE TABLE btw_entries(conversation_id TEXT PRIMARY KEY,payload TEXT,updated_at INTEGER);").unwrap();
    let mut a = adapter(tmp.path());
    a.catalog = Some(SourceCatalog::new(
        db_path.clone(),
        tmp.path().join("Data"),
        None,
    ));
    let path = a.root.join("demo/runtime-5.jsonl");
    write(
        &path,
        &[
            user("main question", 1700000000000),
            json!({"type":"message","role":"assistant","timestamp":1700000005000_i64,"content":[{"type":"output_text","text":"main reply"}]}),
        ],
    );
    let entries = json!([
        {"id":"aside","conversationId":"old-1","question":"side question","answer":"side answer searchable","status":"done","imageCount":1,"createdAt":1700000001000_i64,"updatedAt":1700000002000_i64},
        {"id":"failed","question":"another side question","answer":"","status":"error","error":"source unavailable","createdAt":1700000003000_i64,"updatedAt":1700000004000_i64}
    ]);
    let adapters: Vec<Box<dyn AgentAdapter>> = vec![Box::new(a)];
    let store = Arc::new(Store::open(&tmp.path().join("wake.db")).unwrap());
    run_scan(&adapters, &store, &NullEvents, false).unwrap();
    db.execute(
        "INSERT INTO btw_entries VALUES ('old-1',?1,1)",
        [entries.to_string()],
    )
    .unwrap();
    let roots = adapters[0]
        .watch_paths()
        .into_iter()
        .map(|p| (p, 0))
        .collect::<Vec<_>>();
    assert!(!adapters[0].watch_recursive(tmp.path()));
    crate::watcher::process_batch(
        &adapters,
        &store,
        &NullEvents,
        &roots,
        vec![Ok(notify::Event::new(notify::EventKind::Any).add_path(
            PathBuf::from(format!("{}-wal", db_path.display())),
        ))],
    );
    let r = adapters[0].file_ref(&path).unwrap();
    let p = adapters[0].parse_transcript(&r).unwrap();
    let hit = store
        .search("side answer searchable", &[], None, 10)
        .unwrap()
        .0
        .pop()
        .unwrap();
    assert_eq!(p.mainline[hit.seq as usize].text, "side answer searchable");
    assert_eq!(p.mainline[hit.seq as usize].kind, MessageKind::Aside);
    assert_eq!(p.meta.message_count, 5);
    assert!(p
        .mainline
        .iter()
        .any(|m| m.text == "[Missing side-question images: 1]"));
    assert!(p
        .mainline
        .iter()
        .any(|m| m.kind == MessageKind::SourceWarning && m.text.contains("source unavailable")));
    assert_eq!(p.mainline.last().unwrap().text, "main reply");
    assert!(crate::services::exporter::to_markdown(&p.meta, &p.mainline, &[]).contains("/btw"));
    db.execute("UPDATE btw_entries SET payload='{broken'", [])
        .unwrap();
    run_scan(&adapters, &store, &NullEvents, false).unwrap();
    assert_eq!(
        store
            .search("side answer searchable", &[], None, 10)
            .unwrap()
            .0
            .len(),
        1
    );
    db.execute("DELETE FROM btw_entries", []).unwrap();
    run_scan(&adapters, &store, &NullEvents, false).unwrap();
    assert!(store
        .search("side answer searchable", &[], None, 10)
        .unwrap()
        .0
        .is_empty());
}

#[test]
fn shared_catalog_events_refresh_remote_side_questions() {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("codebuddy.db");
    super::compat_tests::catalog(&db_path);
    let db = rusqlite::Connection::open(&db_path).unwrap();
    db.execute_batch("UPDATE conversations SET runtime_provider=NULL,runtime_session_id=NULL,workspace_authority='ssh-remote+CVM'; CREATE TABLE btw_entries(conversation_id TEXT,payload TEXT,updated_at INTEGER);").unwrap();
    let mut local = adapter(tmp.path());
    local.catalog = Some(SourceCatalog::new(
        db_path.clone(),
        tmp.path().join("Data"),
        None,
    ));
    let remote_root = tmp.path().join("cache/.codebuddy");
    let mut remote = adapter(&remote_root);
    remote.catalog = Some(SourceCatalog::new(
        db_path.clone(),
        tmp.path().join("RemoteData"),
        Some("CVM".into()),
    ));
    let path = remote.root.join("demo/old-1.jsonl");
    write(&path, &[user("remote main prompt", 1700000000000)]);
    let adapters: Vec<Box<dyn AgentAdapter>> = vec![
        Box::new(local),
        Box::new(crate::adapters::remote::RemoteAdapter::new(
            Box::new(remote),
            "CVM",
        )),
    ];
    let store = Arc::new(Store::open(&tmp.path().join("wake.db")).unwrap());
    run_scan(&adapters, &store, &NullEvents, false).unwrap();
    db.execute("INSERT INTO btw_entries VALUES ('old-1',?1,1)", [json!([{"id":"side","question":"remote side question","answer":"remote side answer","status":"done","createdAt":1700000001000_i64,"updatedAt":1700000002000_i64}]).to_string()]).unwrap();
    crate::watcher::process_batch(
        &adapters,
        &store,
        &NullEvents,
        &[(tmp.path().into(), 0)],
        vec![Ok(
            notify::Event::new(notify::EventKind::Any).add_path(db_path)
        )],
    );
    let hits = store.search("remote side answer", &[], None, 10).unwrap().0;
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].session.key, "codebuddy:CVM:old-1");
    assert_eq!(hits[0].session.updated_at, 1700000002000);
}
