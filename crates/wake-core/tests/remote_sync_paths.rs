//! Real rsync on all three platforms, including cwRsync on Windows (#56).
//! This single test owns its process environment; all data and child working
//! directories are synthetic. CI explicitly runs it after installing rsync and
//! a C compiler (Cygwin GCC on Windows, to match cwRsync's transport runtime).
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use wake_core::db::Store;
use wake_core::remote::{host_cache_dir, sync_hosts};

struct RestoreCwd(PathBuf);

impl Drop for RestoreCwd {
    fn drop(&mut self) {
        std::env::set_current_dir(&self.0).unwrap();
    }
}

#[test]
#[ignore = "requires rsync and a C compiler; installed and run explicitly in CI"]
fn remote_sync_handles_native_cache_paths() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = tmp.path().join("bin");
    fs::create_dir_all(&bin).unwrap();

    // Cygwin rsync's transport uses Cygwin descriptors, so a native Windows
    // fixture cannot forward them. Compile for the same runtime instead.
    #[cfg(windows)]
    let mut compiler = {
        let root = PathBuf::from(
            std::env::var_os("WAKE_TEST_CYGWIN_ROOT").expect("Cygwin installation for GCC"),
        );
        let mut compiler = Command::new(root.join("bin/env.exe"));
        compiler.arg("/usr/bin/gcc");
        let mut paths = vec![root.join("bin")];
        paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
        compiler.env("PATH", std::env::join_paths(paths).unwrap());

        compiler
    };
    #[cfg(not(windows))]
    let mut compiler = Command::new("cc");
    fs::copy(
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/remote_sync_ssh.c"
        ),
        bin.join("remote_sync_ssh.c"),
    )
    .unwrap();
    let compiled = compiler
        .current_dir(&bin)
        .args([
            "-std=c11",
            "-Wall",
            "-Wextra",
            "-Werror",
            "remote_sync_ssh.c",
        ])
        .arg("-o")
        .arg(format!("ssh{}", std::env::consts::EXE_SUFFIX))
        .output()
        .unwrap();
    assert!(
        compiled.status.success(),
        "compile fake SSH: {}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    #[cfg(windows)]
    {
        // Keep the receiver, sender and fake SSH on cwRsync's bundled runtime,
        // without invoking Chocolatey's native rsync shim from Cygwin. Stage it
        // after compilation so GCC's subprocesses cannot load this older DLL
        // from their working directory instead of their own toolchain runtime.
        let cwrsync = PathBuf::from(
            std::env::var_os("WAKE_TEST_CWRSYNC_BIN").expect("cwRsync bin directory"),
        );
        fs::copy(cwrsync.join("rsync.exe"), bin.join("rsync.exe")).unwrap();
        for entry in fs::read_dir(cwrsync).unwrap() {
            let entry = entry.unwrap();
            if entry.path().extension().is_some_and(|ext| ext == "dll") {
                fs::copy(entry.path(), bin.join(entry.file_name())).unwrap();
            }
        }
    }
    let mut paths = vec![bin];
    paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
    std::env::set_var("PATH", std::env::join_paths(paths).unwrap());
    let version = Command::new("rsync").arg("--version").output().unwrap();
    assert!(version.status.success(), "rsync must be installed");
    let remote = tmp.path().join("remote");
    std::env::set_var("WAKE_TEST_REMOTE_HOME", &remote);
    let local_home = tmp.path().join("empty-home");
    fs::create_dir_all(&local_home).unwrap();
    for key in ["HOME", "USERPROFILE", "WAKE_HOME"] {
        std::env::set_var(key, &local_home);
    }

    // Even a broken relative destination must only write inside our fixture.
    let _restore_cwd = RestoreCwd(std::env::current_dir().unwrap());
    std::env::set_current_dir(&local_home).unwrap();
    let parent_cwd = std::env::current_dir().unwrap();
    let session = ".claude/projects/demo/session.jsonl";
    let deleted = ".claude/projects/demo/deleted.jsonl";
    let hosts = ["devbox".to_string(), "otherbox".to_string()];
    for host in &hosts {
        let source = remote.join(host).join(session);
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        fs::write(source, format!("synthetic session from {host}\n")).unwrap();
        fs::write(remote.join(host).join(deleted), "synthetic old session\n").unwrap();
    }

    // On Windows this is an absolute drive-letter path, with spaces and Unicode.
    let db_dir = tmp.path().join("local cache 用户 with spaces");
    fs::create_dir_all(&db_dir).unwrap();
    let store = Store::open(&db_dir.join("wake.db")).unwrap();
    for host in &hosts {
        store.add_remote_host(host).unwrap();
    }
    sync_hosts(&store, &hosts);
    assert_eq!(std::env::current_dir().unwrap(), parent_cwd);
    for host in store.list_remote_hosts().unwrap() {
        assert_eq!(host.last_sync_error, None, "host {}", host.name);
        assert!(host.last_sync_at.is_some());
        let cache = host_cache_dir(&db_dir, &host.name);
        assert_eq!(
            fs::read(cache.join(session)).unwrap(),
            fs::read(remote.join(&host.name).join(session)).unwrap(),
            "parallel hosts must keep separate cache trees"
        );
        assert!(cache.join(deleted).is_file());
    }
    assert!(!local_home.join(".claude").exists());

    fs::write(remote.join("devbox").join(session), "updated session\n").unwrap();
    fs::remove_file(remote.join("devbox").join(deleted)).unwrap();
    sync_hosts(&store, &hosts);
    assert_eq!(std::env::current_dir().unwrap(), parent_cwd);
    for host in store.list_remote_hosts().unwrap() {
        assert_eq!(host.last_sync_error, None, "host {}", host.name);
        assert_eq!(
            fs::read(host_cache_dir(&db_dir, &host.name).join(session)).unwrap(),
            fs::read(remote.join(&host.name).join(session)).unwrap()
        );
    }
    assert!(!host_cache_dir(&db_dir, "devbox").join(deleted).exists());
    assert!(host_cache_dir(&db_dir, "otherbox").join(deleted).is_file());
    assert!(remote.join("otherbox").join(deleted).is_file());
}
