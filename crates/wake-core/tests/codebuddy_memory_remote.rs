//! Real rsync with a synthetic remote that exposes only sh and rsync.
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};
use std::path::Path;
use std::sync::Arc;
use wake_core::adapters::create_adapters_for;
use wake_core::db::Store;
use wake_core::models::{MemoryFilter, MemoryOrigin};
use wake_core::scanner::{run_scan, NullEvents};

fn put(root: &Path, relative: &str, body: &str) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, body).unwrap();
}

#[test]
fn memory_snapshots_work_without_remote_utilities_and_survive_changes_and_failures() {
    let tmp = tempfile::tempdir_in("/tmp").unwrap();
    let local = tmp.path().join("local");
    let remote = tmp.path().join("remote");
    let bin = tmp.path().join("bin");
    let remote_bin = tmp.path().join("remote-bin");
    for dir in [&local, &remote, &bin, &remote_bin] {
        fs::create_dir_all(dir).unwrap();
    }
    symlink("/bin/sh", remote_bin.join("sh")).unwrap();
    let rsync = std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|dir| dir.join("rsync"))
        .find(|p| p.is_file())
        .unwrap();
    symlink(rsync, remote_bin.join("rsync")).unwrap();
    let primary = ".codebuddy/projects/opt-project/memory/nested/中文 ; 'quoted' *.md";
    let counter = tmp.path().join("counter");
    let race = tmp.path().join("race");
    let offline = tmp.path().join("offline");
    let interrupted = tmp.path().join("interrupted");
    let ssh = bin.join("ssh");
    fs::write(
        &ssh,
        format!(
            "#!/bin/sh\n
while [ $# -gt 0 ]; do case \"$1\" in -*) shift;; *) break;; esac; done
shift
[ ! -f '{offline}' ] || exit 255
cd '{remote}' || exit 1
export HOME='{remote}' PATH='{remote_bin}'
if [ -f '{interrupted}' ]; then
  case \"$*\" in *rsync*--files-from*) printf 'truncated'; exit 23;; esac
fi
if [ -f '{race}' ]; then
  case \"$*\" in *'sh inventory')
    count=0; if [ -f '{counter}' ]; then IFS= read -r count < '{counter}'; fi
    count=$((count+1)); printf '%s\\n' \"$count\" > '{counter}'
    if [ \"$count\" = 2 ]; then printf '%s' delta > \"$HOME/{primary}\"; fi;;
  esac
fi
exec /bin/sh -c \"$*\"
",
            offline = offline.display(),
            interrupted = interrupted.display(),
            remote = remote.display(),
            remote_bin = remote_bin.display(),
            race = race.display(),
            counter = counter.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o755)).unwrap();
    std::env::set_var(
        "PATH",
        format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
    );
    std::env::set_var("WAKE_HOME", &local);
    std::env::remove_var("CODEBUDDY_CONFIG_DIR");
    put(
        &remote,
        ".codebuddy/projects/opt-project/s.jsonl",
        r#"{"type":"message","role":"user","cwd":"/opt/project","content":[{"type":"text","text":"synthetic prompt"}]}"#,
    );
    put(
        &remote,
        ".codebuddy/memories/opt-project/fallback.md",
        "fallback",
    );
    put(&remote, ".codebuddy/memories/global/cli.md", "cli-global");
    let linux = ".local/share/CodeBuddyExtension/Data/Public/.memories";
    let mac = "Library/Application Support/CodeBuddyExtension/Data/Public/.memories";
    for root in [linux, mac] {
        put(&remote, &format!("{root}/global.mdc"), "ide-global");
    }
    put(
        &remote,
        ".codebuddy/settings.json",
        r#"{"memory":{"autoMemoryEnabled":true},"token":"SECRET_NEVER_COPY"}"#,
    );
    let db_dir = tmp.path().join("local cache 用户 with spaces");
    fs::create_dir_all(&db_dir).unwrap();
    let store = Arc::new(Store::open(&db_dir.join("wake.db")).unwrap());
    store.add_remote_host("fixture").unwrap();
    let hosts = vec!["fixture".to_string()];
    let sync = || {
        wake_core::remote::sync_hosts(&store, &hosts);
        store.list_remote_hosts().unwrap()[0]
            .last_sync_error
            .clone()
    };
    assert_eq!(sync(), None);
    let cache = wake_core::remote::host_cache_dir(&store.db_dir().unwrap(), "fixture");
    let mirror = cache.join(".codebuddy-memory");
    assert!(!mirror.join(".codebuddy/settings.json").exists());
    assert!(!fs::read_to_string(mirror.join("manifest.json"))
        .unwrap()
        .contains("SECRET_NEVER_COPY"));
    let adapters = create_adapters_for(&store);
    let scan = || {
        run_scan(&adapters, &store, &NullEvents, false).unwrap();
        store.list_memories(&MemoryFilter::default()).unwrap()
    };
    let docs = scan();
    assert_eq!(docs.len(), 4);
    assert_eq!(
        docs.iter()
            .find(|d| d.title == "fallback.md")
            .unwrap()
            .project_path,
        "/opt/project"
    );
    assert!(docs.iter().all(|d| d.host == "fixture"));
    // An empty primary memory directory suppresses the old centralized fallback.
    fs::create_dir_all(remote.join(".codebuddy/projects/opt-project/memory")).unwrap();
    assert_eq!(sync(), None);
    assert_eq!(scan().len(), 3);
    put(&remote, primary, "alpha");
    put(
        &remote,
        ".codebuddy/projects/opt-project/memory/.ide-memory-backups/hidden.md",
        "SECRET_NEVER_COPY",
    );
    // A previous Wake version may already have copied the entire project
    // memory tree. The normal sync must remove that redundant copy while the
    // separately verified memory snapshot remains available.
    put(
        &cache,
        ".codebuddy/projects/opt-project/memory/.ide-memory-backups/stale.md",
        "old synthetic backup",
    );
    assert_eq!(sync(), None);
    assert_eq!(scan().len(), 4);
    assert!(!cache
        .join(".codebuddy/projects/opt-project/memory")
        .exists());
    assert!(!mirror
        .join(".codebuddy/projects/opt-project/memory/.ide-memory-backups")
        .exists());
    let primary_cache = mirror.join(primary);
    let inode = fs::metadata(&primary_cache).unwrap().ino();
    assert_eq!(sync(), None);
    assert_eq!(fs::metadata(&primary_cache).unwrap().ino(), inode);
    let source = remote.join(primary);
    let mtime = fs::metadata(&source).unwrap().modified().unwrap();
    fs::write(&source, "bravo").unwrap();
    fs::File::open(&source)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(mtime))
        .unwrap();
    assert_eq!(sync(), None);
    assert_eq!(fs::read_to_string(&primary_cache).unwrap(), "bravo");
    assert_eq!(
        store
            .search_memories("alpha", &Default::default())
            .unwrap()
            .len(),
        1
    );
    scan();
    assert!(store
        .search_memories("alpha", &Default::default())
        .unwrap()
        .is_empty());
    assert_eq!(
        store
            .search_memories("bravo", &Default::default())
            .unwrap()
            .len(),
        1
    );
    // Both IDE layouts have independent global switches; CLI/project default
    // inheritance and disabled files remain discoverable.
    put(
        &remote,
        &format!("{linux}/.memory-global-config.json"),
        r#"{"enabled":false}"#,
    );
    put(
        &remote,
        ".codebuddy/settings.json",
        r#"{"memory":{"autoMemoryEnabled":false},"token":"SECRET_NEVER_COPY"}"#,
    );
    assert_eq!(sync(), None);
    let docs = scan();
    let project = docs
        .iter()
        .find(|d| d.details.origin == Some(MemoryOrigin::Shared))
        .unwrap();
    assert_eq!(project.details.cli_enabled, Some(false));
    assert_eq!(project.details.ide_enabled, Some(false));
    assert_eq!(
        docs.iter()
            .find(|d| d.path.contains(linux))
            .unwrap()
            .details
            .ide_enabled,
        Some(false)
    );
    assert_eq!(
        docs.iter()
            .find(|d| d.path.contains(mac))
            .unwrap()
            .details
            .ide_enabled,
        Some(true)
    );
    put(&remote, ".codebuddy/settings.json", "{}");
    assert_eq!(sync(), None);
    scan();
    let protected = fs::read(mirror.join("manifest.json")).unwrap();
    for failure in [
        "json",
        "nul",
        "settings-utf8",
        "utf8",
        "oversize",
        "symlink",
        "permission",
        "offline",
        "interrupted",
        "race",
    ] {
        match failure {
            "json" => put(
                &remote,
                ".codebuddy/settings.json",
                r#"{"token":"SECRET_NEVER_COPY",}"#,
            ),
            "utf8" => fs::write(&source, [0xff, 0xfe]).unwrap(),
            "nul" => fs::write(
                remote.join(".codebuddy/settings.json"),
                b"{\"memory\":{\"autoMemoryEnabled\":fal\0se}}",
            )
            .unwrap(),
            "settings-utf8" => fs::write(
                remote.join(".codebuddy/settings.json"),
                b"{\"token\":\"\xff\"}",
            )
            .unwrap(),
            "oversize" => fs::write(&source, vec![b'x'; 4 * 1024 * 1024 + 1]).unwrap(),
            "symlink" => {
                fs::remove_file(&source).unwrap();
                symlink("missing", &source).unwrap();
            }
            "permission" => {
                fs::set_permissions(source.parent().unwrap(), fs::Permissions::from_mode(0o000))
                    .unwrap();
                if fs::read_dir(source.parent().unwrap()).is_ok() {
                    fs::set_permissions(
                        source.parent().unwrap(),
                        fs::Permissions::from_mode(0o755),
                    )
                    .unwrap();
                    continue;
                }
            }
            "offline" => fs::write(&offline, "offline").unwrap(),
            "interrupted" => fs::write(&interrupted, "truncated rsync stream").unwrap(),
            "race" => fs::write(&race, "change during second probe").unwrap(),
            _ => unreachable!(),
        }
        let error = sync().expect("failed memory source was reported as successful");
        assert!(!error.contains("SECRET_NEVER_COPY"));
        assert_eq!(fs::read(&primary_cache).unwrap(), b"bravo", "{failure}");
        assert_eq!(
            fs::read(mirror.join("manifest.json")).unwrap(),
            protected,
            "{failure}"
        );
        scan();
        assert_eq!(
            store
                .search_memories("bravo", &Default::default())
                .unwrap()
                .len(),
            1,
            "{failure}"
        );
        match failure {
            "json" | "nul" | "settings-utf8" => put(&remote, ".codebuddy/settings.json", "{}"),
            "symlink" => {
                fs::remove_file(&source).unwrap();
                fs::write(&source, "bravo").unwrap();
            }
            "permission" => {
                fs::set_permissions(source.parent().unwrap(), fs::Permissions::from_mode(0o755))
                    .unwrap()
            }
            "offline" => fs::remove_file(&offline).unwrap(),
            "interrupted" => fs::remove_file(&interrupted).unwrap(),
            "race" => {
                fs::remove_file(&race).unwrap();
                fs::write(&source, "bravo").unwrap();
            }
            _ => fs::write(&source, "bravo").unwrap(),
        }
    }
    assert_eq!(sync(), None);
    // The transcript names a symlink, while the memory ID names its physical
    // workspace. Exercise the actual resolve path instead of an unused fixture.
    let real_workspace = remote.join("workspace");
    fs::create_dir(&real_workspace).unwrap();
    let workspace_link = remote.join("workspace-link");
    symlink(&real_workspace, &workspace_link).unwrap();
    let physical = fs::canonicalize(&real_workspace).unwrap();
    let id = physical
        .to_str()
        .unwrap()
        .replace('/', "-")
        .trim_matches('-')
        .to_string();
    let relative = format!(".codebuddy/projects/{id}");
    put(&remote, &format!("{relative}/s.jsonl"), &serde_json::json!({"type":"message","role":"user","cwd":workspace_link,"content":"synthetic prompt"}).to_string());
    put(
        &remote,
        &format!("{relative}/memory/physical.md"),
        "symlink attribution",
    );
    assert_eq!(sync(), None);
    assert_eq!(
        scan()
            .iter()
            .find(|d| d.title == "physical.md")
            .unwrap()
            .project_path,
        physical.to_str().unwrap()
    );
    fs::remove_dir_all(remote.join(relative)).unwrap();
    assert_eq!(sync(), None);
    assert!(!scan().iter().any(|d| d.title == "physical.md"));
    assert!(fs::read_dir(&cache).unwrap().all(|e| !e
        .unwrap()
        .file_name()
        .to_string_lossy()
        .starts_with(".codebuddy-stage-")));
    // Recovery of a process that stopped halfway through a directory swap.
    let backup = cache.join(".codebuddy-memory.wake-previous");
    fs::rename(&mirror, &backup).unwrap();
    put(&remote, ".codebuddy/settings.json", "{");
    assert!(sync().is_some());
    assert!(mirror.is_dir());
    assert!(!backup.exists());
    assert_eq!(fs::read(&primary_cache).unwrap(), b"bravo");
    put(&remote, ".codebuddy/settings.json", "{}");
    // Successful remote deletion propagates only after a verified empty copy.
    fs::remove_dir_all(remote.join(".codebuddy/projects/opt-project/memory")).unwrap();
    fs::remove_dir_all(remote.join(".codebuddy/memories")).unwrap();
    for root in [linux, mac] {
        fs::remove_dir_all(remote.join(root)).unwrap();
    }
    assert_eq!(sync(), None);
    assert!(scan().is_empty());
    assert!(!primary_cache.exists());
}
