//! End-to-end selective remote mirroring with a synthetic catalog and SSH host.
#![cfg(unix)]

use md5::{Digest, Md5};
use serde_json::json;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;
use wake_core::adapters::{adapter_for, create_adapters_for};
use wake_core::db::Store;
use wake_core::models::{AgentId, SessionFileRef};
use wake_core::scanner::{run_scan, NullEvents};

fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

#[test]
fn selected_sessions_and_assets_sync_atomically_and_keep_cache_offline() {
    let tmp = tempfile::tempdir_in("/tmp").unwrap();
    let home = tmp.path().join("local");
    let remote = tmp.path().join("remote");
    let bin = tmp.path().join("bin");
    fs::create_dir_all(&bin).unwrap();
    fs::create_dir_all(home.join(".codebuddy")).unwrap();
    fs::create_dir_all(&remote).unwrap();
    let ssh = bin.join("ssh");
    let offline = tmp.path().join("offline");
    let interrupted = tmp.path().join("interrupted");
    write(&ssh, &format!(
        "#!/bin/sh\nwhile [ $# -gt 0 ]; do case \"$1\" in -*) shift;; *) break;; esac; done\nhost=$1; shift\nif [ -f '{offline}' ]; then exit 255; fi\nif [ -f '{interrupted}' ]; then case \"$*\" in *'rsync --server'*) exit 255;; esac; fi\ncd '{remote}' || exit 1\nexec sh -c \"$*\"\n",
        offline = offline.display(), interrupted = interrupted.display(), remote = remote.display()
    ));
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o755)).unwrap();
    std::env::set_var(
        "PATH",
        format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        ),
    );
    std::env::set_var("WAKE_HOME", &home);
    for name in [
        "XDG_DATA_HOME",
        "XDG_CONFIG_HOME",
        "CODEX_HOME",
        "QODER_CONFIG_DIR",
        "HERMES_HOME",
        "OPENCLAW_STATE_DIR",
        "CODEBUDDY_CONFIG_DIR",
        "WORKBUDDY_CONFIG_DIR",
        "ZCODE_STORAGE_DIR",
        "KIMI_CODE_HOME",
        "MIMOCODE_HOME",
        "OPENCODE_DB",
        "KILO_DB",
    ] {
        std::env::remove_var(name);
    }

    let db = rusqlite::Connection::open(home.join(".codebuddy/codebuddy.db")).unwrap();
    db.execute_batch(
        "CREATE TABLE conversations (
        id TEXT PRIMARY KEY, user_id TEXT, workspace TEXT, workspace_authority TEXT,
        title TEXT, custom_title TEXT, created_at INTEGER, last_message_at INTEGER,
        deleted_at INTEGER, archived_at INTEGER, runtime_provider TEXT,
        runtime_session_id TEXT, revision INTEGER);
        INSERT INTO conversations VALUES ('remote-1','account','/opt/project',
        'codebuddy-remote-ssh+_x43__x56__x4d_','Remote fixture',NULL,1700000000000,1700000000000,
        NULL,NULL,'codebuddy-ide-4','remote-1',1);
        INSERT INTO conversations VALUES ('deleted','account','/opt/project',
        'codebuddy-remote-ssh+_x43__x56__x4d_','Deleted',NULL,1700000000000,1700000000000,
        1,NULL,'codebuddy-ide-4','deleted',1);
        INSERT INTO conversations VALUES ('converted','account','/opt/project',
        'codebuddy-remote-ssh+_x43__x56__x4d_','Converted',NULL,1700000000000,1700000000000,
        NULL,NULL,'cbc','runtime-5',1);",
    )
    .unwrap();
    let hash = format!("{:x}", Md5::digest(b"/opt/project"));
    let root =
        format!(".local/share/CodeBuddyExtension/Data/account/CodeBuddyIDE/account/history/{hash}");
    let session = format!("{root}/remote-1");
    let index = json!({"messages":[{"id":"m1"},{"id":"missing"}],"requests":[
        {"id":"r1","usage":{"inputTokens":2,"outputTokens":1,"totalTokens":3}}
    ]});
    write(
        &remote.join(&session).join("index.json"),
        &index.to_string(),
    );
    let message = json!({"role":"user","message":json!({"role":"user","content":[
        {"type":"text","text":"remote searchable phrase"},
        {"type":"image","image":"codebuddy-asset://assets/picture.png"}
    ]}).to_string(),"extra":"{}"});
    write(
        &remote.join(&session).join("messages/m1.json"),
        &message.to_string(),
    );
    fs::create_dir_all(remote.join(&session).join("assets")).unwrap();
    fs::write(
        remote.join(&session).join("assets/picture.png"),
        b"\x89PNG\r\n\x1a\n",
    )
    .unwrap();
    write(
        &remote.join(&session).join("assets/unreferenced.txt"),
        "DO NOT COPY",
    );
    let converted = format!("{root}/converted");
    write(
        &remote.join(&converted).join("index.json"),
        &json!({"messages":[{"id":"m1"}],"requests":[
            {"id":"r1","usage":{"inputTokens":6,"outputTokens":4,"totalTokens":10}}
        ]})
        .to_string(),
    );
    write(
        &remote.join(&converted).join("messages/m1.json"),
        &message.to_string(),
    );
    let blob_id = "b".repeat(64);
    let blob_path = format!(".codebuddy/blobs/bb/{blob_id}.png");
    fs::create_dir_all(remote.join(".codebuddy/blobs/bb")).unwrap();
    fs::write(remote.join(&blob_path), b"\x89PNG\r\n\x1a\n").unwrap();
    fs::write(
        remote.join(".codebuddy/blobs/bb/unreferenced.png"),
        "DO NOT COPY",
    )
    .unwrap();
    let cbc = [
        json!({"type":"message","role":"user","content":[{"type":"input_text","text":"remote converted prompt"},
            {"type":"image_blob_ref","blob_id":blob_id,"blob_path":format!("/home/remote/{blob_path}"),"mime":"image/png"}]}),
        json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":"remote converted answer"}],
            "providerData":{"conversationRequestId":"r2","rawUsage":{"total_tokens":7}}}),
    ].into_iter().map(|v| v.to_string() + "\n").collect::<String>();
    write(
        &remote.join(".codebuddy/projects/demo/runtime-5.jsonl"),
        &cbc,
    );
    let remote_child =
        remote.join(".codebuddy/projects/demo/runtime-5/subagents/agent-child.jsonl");
    write(
        &remote_child,
        &(json!({"type":"message", "role":"assistant",
        "content":[{"type":"output_text","text":"remote child result"}]})
        .to_string()
            + "\n"),
    );
    write(&remote.join(&root).join("orphan/index.json"), "DO NOT COPY");
    write(
        &remote.join(&root).join("deleted/index.json"),
        "DO NOT COPY",
    );
    write(&remote.join(".codebuddy/auth.json"), "NOT A REAL SECRET");

    let store = Arc::new(Store::open(&tmp.path().join("wake.db")).unwrap());
    store.add_remote_host("CVM").unwrap();
    let hosts = vec!["CVM".to_string()];
    wake_core::remote::sync_hosts(&store, &hosts);
    let host = store
        .list_remote_hosts()
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    assert_eq!(host.last_sync_error, None);
    let cache = wake_core::remote::host_cache_dir(&store.db_dir().unwrap(), "CVM");
    assert!(cache.join(&session).join("index.json").is_file());
    assert!(cache.join(&session).join("messages/m1.json").is_file());
    assert!(cache.join(&session).join("assets/picture.png").is_file());
    assert!(!cache
        .join(&session)
        .join("assets/unreferenced.txt")
        .exists());
    assert!(!cache.join(&root).join("orphan").exists());
    assert!(!cache.join(&root).join("deleted").exists());
    assert!(!cache.join(".codebuddy/auth.json").exists());
    assert!(cache.join(&blob_path).is_file());
    assert!(!cache.join(".codebuddy/blobs/bb/unreferenced.png").exists());
    fs::remove_file(remote.join(&blob_path)).unwrap();

    // Some writers replace a file with the same byte length and timestamp.
    // The mirror must still notice that its content changed.
    let source_message = remote.join(&session).join("messages/m1.json");
    let original = fs::read_to_string(&source_message).unwrap();
    let modified = original.replace("remote searchable phrase", "remote searchable PHRASE");
    assert_ne!(original, modified);
    assert_eq!(original.len(), modified.len());
    let mtime = fs::metadata(&source_message).unwrap().modified().unwrap();
    fs::write(&source_message, &modified).unwrap();
    fs::File::open(&source_message)
        .unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(mtime))
        .unwrap();
    wake_core::remote::sync_hosts(&store, &hosts);
    assert_eq!(store.list_remote_hosts().unwrap()[0].last_sync_error, None);
    assert_eq!(
        fs::read_to_string(cache.join(&session).join("messages/m1.json")).unwrap(),
        modified
    );

    let adapters = create_adapters_for(&store);
    run_scan(&adapters, &store, &NullEvents, false).unwrap();
    let meta = store
        .get_session("codebuddy:CVM:remote-1")
        .unwrap()
        .unwrap();
    assert_eq!(meta.tokens_used, Some(3));
    let adapter = adapter_for(&adapters, AgentId::Codebuddy, &meta.file_path).unwrap();
    let transcript = adapter
        .parse_transcript(&SessionFileRef::from_meta(&meta))
        .unwrap();
    assert_eq!(transcript.mainline[0].images.len(), 1);
    assert!(transcript.mainline[1].text.contains("missing"));
    let (hits, _) = store.search("searchable phrase", &[], None, 10).unwrap();
    let hit = hits
        .iter()
        .find(|hit| hit.session.key == "codebuddy:CVM:remote-1")
        .unwrap();
    assert_eq!(hit.seq, transcript.mainline[0].seq);

    // Failure to traverse the remote CBC root must not be treated as the
    // entire source having been removed.
    let cbc_root = remote.join(".codebuddy");
    let cached_cbc = cache.join(".codebuddy/projects/demo/runtime-5.jsonl");
    let cbc_before = fs::read(&cached_cbc).unwrap();
    fs::set_permissions(&cbc_root, fs::Permissions::from_mode(0o000)).unwrap();
    let inaccessible = fs::read_dir(&cbc_root).is_err();
    let mut sync_error = false;
    if inaccessible {
        wake_core::remote::sync_hosts(&store, &hosts);
        sync_error = store.list_remote_hosts().unwrap()[0]
            .last_sync_error
            .is_some();
    }
    fs::set_permissions(&cbc_root, fs::Permissions::from_mode(0o755)).unwrap();
    if inaccessible {
        assert!(sync_error, "an inaccessible CBC root must fail the sync");
        assert_eq!(fs::read(&cached_cbc).unwrap(), cbc_before);
    }

    // A temporarily unavailable symlink target is also not a deletion.
    let cbc_saved = remote.join(".codebuddy-saved");
    fs::rename(&cbc_root, &cbc_saved).unwrap();
    std::os::unix::fs::symlink("missing-target", &cbc_root).unwrap();
    wake_core::remote::sync_hosts(&store, &hosts);
    let symlink_error = store.list_remote_hosts().unwrap()[0]
        .last_sync_error
        .is_some();
    fs::remove_file(&cbc_root).unwrap();
    fs::rename(&cbc_saved, &cbc_root).unwrap();
    assert!(
        symlink_error,
        "an unavailable source symlink must fail the sync"
    );
    assert_eq!(fs::read(&cached_cbc).unwrap(), cbc_before);

    // A same-size, same-mtime rewrite must also invalidate the existing Wake
    // search index after the refreshed bytes have reached the cache.
    let before = fs::read_to_string(&source_message).unwrap();
    let after = before.replace("searchable", "altered___");
    assert_ne!(before, after);
    assert_eq!(before.len(), after.len());
    let mtime = fs::metadata(&source_message).unwrap().modified().unwrap();
    fs::write(&source_message, &after).unwrap();
    fs::File::open(&source_message)
        .unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(mtime))
        .unwrap();
    wake_core::remote::sync_hosts(&store, &hosts);
    assert_eq!(store.list_remote_hosts().unwrap()[0].last_sync_error, None);
    run_scan(&adapters, &store, &NullEvents, false).unwrap();
    let (hits, _) = store.search("altered___", &[], None, 10).unwrap();
    assert!(
        hits.iter()
            .any(|hit| hit.session.key == "codebuddy:CVM:remote-1"),
        "the cache changed but the search index stayed stale"
    );
    let stamp = store.file_stamp(&meta.file_path).unwrap().unwrap();
    wake_core::remote::sync_hosts(&store, &hosts);
    assert_eq!(store.list_remote_hosts().unwrap()[0].last_sync_error, None);
    run_scan(&adapters, &store, &NullEvents, false).unwrap();
    assert_eq!(
        store.file_stamp(&meta.file_path).unwrap().unwrap(),
        stamp,
        "unchanged hard-linked files must not trigger a full reindex"
    );
    // A permission error is not evidence that a referenced message was
    // deleted. Keep the previous generation until it can be read again.
    let source_dir = remote.join(&session).join("messages");
    let cached_before = fs::read(cache.join(&session).join("messages/m1.json")).unwrap();
    fs::set_permissions(&source_dir, fs::Permissions::from_mode(0o000)).unwrap();
    let inaccessible = fs::read_dir(&source_dir).is_err();
    let mut sync_error = false;
    if inaccessible {
        wake_core::remote::sync_hosts(&store, &hosts);
        sync_error = store.list_remote_hosts().unwrap()[0]
            .last_sync_error
            .is_some();
    }
    fs::set_permissions(&source_dir, fs::Permissions::from_mode(0o755)).unwrap();
    if inaccessible {
        assert!(
            sync_error,
            "an inaccessible message directory must fail the sync"
        );
        assert_eq!(
            fs::read(cache.join(&session).join("messages/m1.json")).unwrap(),
            cached_before
        );
    }
    let converted_meta = store
        .get_session("codebuddy:CVM:converted")
        .unwrap()
        .unwrap();
    assert_eq!(converted_meta.id, "runtime-5");
    assert_eq!(converted_meta.tokens_used, Some(17));
    assert_eq!(converted_meta.source.as_deref(), Some("CodeBuddy IDE 4.x"));
    let converted_adapter =
        adapter_for(&adapters, AgentId::Codebuddy, &converted_meta.file_path).unwrap();
    assert!(
        cache.join(&blob_path).is_file(),
        "missing cached blob before decode"
    );
    assert_eq!(
        fs::read(cache.join(&blob_path)).unwrap(),
        b"\x89PNG\r\n\x1a\n"
    );
    let detail = converted_adapter
        .parse_transcript(&SessionFileRef::from_meta(&converted_meta))
        .unwrap();
    assert_eq!(
        detail.mainline[0].images.len(),
        1,
        "a missing remote blob keeps the last cached image"
    );
    assert!(store
        .get_session("codebuddy:CVM:runtime-5")
        .unwrap()
        .is_none());
    // Converted CBC files are mirrored by the regular remote sync. They need
    // the same content-aware invalidation as the legacy message files.
    let source_cbc = remote.join(".codebuddy/projects/demo/runtime-5.jsonl");
    let before_cbc = fs::read_to_string(&source_cbc).unwrap();
    let after_cbc = before_cbc.replace("remote converted answer", "remote modified_ answer");
    assert_ne!(before_cbc, after_cbc);
    assert_eq!(before_cbc.len(), after_cbc.len());
    let mtime_cbc = fs::metadata(&source_cbc).unwrap().modified().unwrap();
    fs::write(&source_cbc, &after_cbc).unwrap();
    fs::File::open(&source_cbc)
        .unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(mtime_cbc))
        .unwrap();
    wake_core::remote::sync_hosts(&store, &hosts);
    assert_eq!(store.list_remote_hosts().unwrap()[0].last_sync_error, None);
    assert_eq!(
        fs::read_to_string(cache.join(".codebuddy/projects/demo/runtime-5.jsonl")).unwrap(),
        after_cbc
    );
    run_scan(&adapters, &store, &NullEvents, false).unwrap();
    let (updated_hits, _) = store.search("modified_ answer", &[], None, 10).unwrap();
    assert!(updated_hits
        .iter()
        .any(|hit| hit.session.key == "codebuddy:CVM:converted"));
    assert_eq!(
        store
            .parent_key_of("codebuddy:CVM:agent-child")
            .unwrap()
            .as_deref(),
        Some("codebuddy:CVM:converted")
    );
    let plan = wake_core::cleanup::delete_plan(&store, &adapters, &converted_meta).unwrap();
    assert_eq!(plan.sessions.len(), 2);
    assert!(plan.targets.is_empty());
    assert!(wake_core::cleanup::trash_sessions(&store, &plan.sessions, &plan.targets).unwrap());
    run_scan(&adapters, &store, &NullEvents, false).unwrap();
    assert!(store
        .get_session("codebuddy:CVM:converted")
        .unwrap()
        .is_none());
    let hidden = wake_core::cleanup::hidden_sessions(&store, &adapters).unwrap();
    assert_eq!(hidden.len(), 1);
    wake_core::cleanup::unhide(&store, &adapters, hidden.into_iter().next().unwrap()).unwrap();
    assert!(store
        .get_session("codebuddy:CVM:converted")
        .unwrap()
        .is_some());
    assert!(remote
        .join(".codebuddy/projects/demo/runtime-5.jsonl")
        .is_file());
    assert!(remote_child.is_file());

    db.execute(
        "INSERT INTO conversations VALUES ('unavailable','account','/opt/project',
        'codebuddy-remote-ssh+_x43__x56__x4d_','Unavailable',NULL,1700000000000,1700000000000,
        NULL,NULL,'codebuddy-ide-4','unavailable',1)",
        [],
    )
    .unwrap();
    wake_core::remote::sync_hosts(&store, &hosts);
    assert!(store.list_remote_hosts().unwrap()[0]
        .last_sync_error
        .is_some());
    assert!(cache.join(&session).join("index.json").is_file());
    db.execute("DELETE FROM conversations WHERE id='unavailable'", [])
        .unwrap();

    let old = fs::read(cache.join(&session).join("messages/m1.json")).unwrap();
    let updated = json!({"role":"user","message":json!({"content":[
        {"type":"text","text":"new remote text after a retry"}
    ]}).to_string(),"extra":"{}"})
    .to_string();
    write(&remote.join(&session).join("messages/m1.json"), &updated);
    write(&interrupted, "fail during rsync");
    wake_core::remote::sync_hosts(&store, &hosts);
    assert!(store.list_remote_hosts().unwrap()[0]
        .last_sync_error
        .is_some());
    assert_eq!(
        fs::read(cache.join(&session).join("messages/m1.json")).unwrap(),
        old
    );
    fs::remove_file(&interrupted).unwrap();
    wake_core::remote::sync_hosts(&store, &hosts);
    assert_eq!(store.list_remote_hosts().unwrap()[0].last_sync_error, None);
    assert_eq!(
        fs::read_to_string(cache.join(&session).join("messages/m1.json")).unwrap(),
        updated
    );
    let old = fs::read(cache.join(&session).join("messages/m1.json")).unwrap();
    write(&offline, "offline");
    write(
        &remote.join(&session).join("messages/m1.json"),
        "changed while offline",
    );
    wake_core::remote::sync_hosts(&store, &hosts);
    assert!(store.list_remote_hosts().unwrap()[0]
        .last_sync_error
        .is_some());
    assert_eq!(
        fs::read(cache.join(&session).join("messages/m1.json")).unwrap(),
        old
    );
    run_scan(&adapters, &store, &NullEvents, false).unwrap();
    assert!(store
        .get_session("codebuddy:CVM:remote-1")
        .unwrap()
        .is_some());
    assert!(remote.join(&session).join("index.json").is_file());
}
