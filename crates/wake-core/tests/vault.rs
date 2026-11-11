//! 本机归档(vault)端到端:源文件在时列表里是源那一份,源文件被删之后会话不消失、
//! 由归档里逐字节相同的副本顶上,照旧读得出正文、搜得到。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use wake_core::adapters::claude::ClaudeAdapter;
use wake_core::adapters::remote::create_archive_adapters;
use wake_core::adapters::{adapter_for, create_adapters, AgentAdapter};
use wake_core::db::Store;
use wake_core::models::*;
use wake_core::scanner::{run_scan, NullEvents};
use wake_core::vault;
use wake_core::watcher::promote_survivors;

const ID: &str = "5e55104a-0000-4000-8000-00000000a001";

/// home 下放一条两轮的 Claude Code 会话,返回它的路径
fn stage_session(home: &Path) -> PathBuf {
    let dir = home.join(".claude/projects/-Users-tester-Github-vaultfx");
    std::fs::create_dir_all(&dir).unwrap();
    let row =
        |typ: &str, uuid: &str, parent: Option<&str>, ts: &str, message: serde_json::Value| {
            serde_json::json!({
                "parentUuid": parent,
                "isSidechain": false,
                "cwd": "/Users/tester/Github/vaultfx",
                "sessionId": ID,
                "type": typ,
                "message": message,
                "uuid": uuid,
                "timestamp": ts,
            })
        };
    let lines = [
        row(
            "user",
            "u1",
            None,
            "2026-10-01T08:00:00.000Z",
            serde_json::json!({ "role": "user", "content": "归档测试:请解释 zanzibar 鉴权模型" }),
        ),
        row(
            "assistant",
            "a1",
            Some("u1"),
            "2026-10-01T08:00:05.000Z",
            serde_json::json!({
                "id": "msg_1",
                "role": "assistant",
                "model": "claude-opus-5-5",
                "content": [{ "type": "text", "text": "zanzibar 用关系元组表达权限。" }],
                "usage": { "input_tokens": 10, "output_tokens": 12 },
            }),
        ),
    ];
    let body: String = lines.iter().map(|l| format!("{l}\n")).collect();
    let path = dir.join(format!("{ID}.jsonl"));
    std::fs::write(&path, body).unwrap();
    path
}

struct Fixture {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    vault: PathBuf,
    live: PathBuf,
    store: Arc<Store>,
    adapters: Vec<Box<dyn AgentAdapter>>,
}

fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    // macOS 的临时目录在 /var 这个别名后面,路径比较要真实路径
    let root = tmp.path().canonicalize().unwrap();
    let home = root.join("home");
    let live = stage_session(&home);
    let store = Arc::new(Store::open(&root.join("index/scan.db")).unwrap());
    let vault = vault::ensure(&store).unwrap();
    let mut adapters: Vec<Box<dyn AgentAdapter>> =
        vec![ClaudeAdapter::new().with_custom_root(home.join(".claude/projects"))];
    adapters.extend(create_archive_adapters(&create_adapters(), &vault));
    Fixture {
        _tmp: tmp,
        home,
        vault,
        live,
        store,
        adapters,
    }
}

fn key() -> String {
    format!("claude-code:{ID}")
}

fn assert_readable_from(f: &Fixture, expect_archived: bool) {
    let meta = f.store.get_session(&key()).unwrap().expect("会话在库里");
    let vault_root = vault::dir_for(&f.store).unwrap();
    assert_eq!(
        vault::is_archived(&vault_root, &meta.file_path),
        expect_archived,
        "{}",
        meta.file_path
    );
    let adapter = adapter_for(&f.adapters, meta.agent, &meta.file_path).unwrap();
    let transcript = adapter
        .parse_transcript(&SessionFileRef::from_meta(&meta))
        .unwrap();
    assert_eq!(transcript.mainline.len(), 2);
    assert!(transcript.mainline[1].text.contains("关系元组"));
    let (all, total) = f.store.list_sessions(&SessionFilter::default()).unwrap();
    assert_eq!(total, 1, "两份副本只算一条会话");
    assert_eq!(all[0].key, key());
}

#[test]
fn session_outlives_its_source_across_full_scans() {
    let f = fixture();
    let original = std::fs::read(&f.live).unwrap();
    assert_eq!(vault::sync(&f.home, &f.vault).copied, 1);
    run_scan(&f.adapters, &f.store, &NullEvents, true).unwrap();
    assert_readable_from(&f, false);

    // 源被清掉(Claude Code 自己的定期清理、用户手动删)
    std::fs::remove_file(&f.live).unwrap();
    vault::sync(&f.home, &f.vault);
    run_scan(&f.adapters, &f.store, &NullEvents, false).unwrap();
    assert_readable_from(&f, true);

    let meta = f.store.get_session(&key()).unwrap().unwrap();
    assert_eq!(
        std::fs::read(&meta.file_path).unwrap(),
        original,
        "归档是原始文件逐字节的副本"
    );

    // 再来一轮全量重扫(相当于重建索引)也还在
    run_scan(&f.adapters, &f.store, &NullEvents, true).unwrap();
    assert_readable_from(&f, true);
}

/// watcher 的路径:源消失的事件把那一行出库,`promote_survivors` 当场让归档副本顶上,
/// 不用等下一次全量扫描
#[test]
fn archive_copy_takes_over_when_the_watcher_sees_the_source_vanish() {
    let f = fixture();
    vault::sync(&f.home, &f.vault);
    run_scan(&f.adapters, &f.store, &NullEvents, true).unwrap();

    std::fs::remove_file(&f.live).unwrap();
    let removed = f
        .store
        .remove_by_path(&f.live.to_string_lossy())
        .unwrap()
        .expect("源那一行出库");
    promote_survivors(&f.adapters, &f.store, &NullEvents, &[removed]);
    assert_readable_from(&f, true);
}

/// 源回来了(从废纸篓放回、从备份恢复):位次让源那份重新胜出
#[test]
fn live_copy_wins_again_once_the_source_is_back() {
    let f = fixture();
    let original = std::fs::read(&f.live).unwrap();
    vault::sync(&f.home, &f.vault);
    std::fs::remove_file(&f.live).unwrap();
    run_scan(&f.adapters, &f.store, &NullEvents, true).unwrap();
    assert_readable_from(&f, true);

    std::fs::write(&f.live, original).unwrap();
    run_scan(&f.adapters, &f.store, &NullEvents, false).unwrap();
    assert_readable_from(&f, false);
}

/// 换归档位置:旧归档整份并进新位置、旧目录清掉;源已删除的会话重组 roster 补扫后还在,
/// 读的是新位置那一份
#[test]
fn relocating_the_archive_keeps_sessions_whose_source_is_gone() {
    let mut f = fixture();
    let original = std::fs::read(&f.live).unwrap();
    vault::sync(&f.home, &f.vault);
    std::fs::remove_file(&f.live).unwrap();
    run_scan(&f.adapters, &f.store, &NullEvents, true).unwrap();
    assert_readable_from(&f, true);

    let chosen = f.home.parent().unwrap().join("cloud drive");
    let moved = vault::relocate(&f.store, Some(&chosen)).unwrap();
    assert_eq!(moved, chosen.join(vault::CUSTOM_SUBDIR));
    assert_eq!(vault::dir_for(&f.store).unwrap(), moved);
    assert!(vault::is_custom(&f.store));
    assert!(!f.vault.exists(), "旧归档目录已清掉");

    f.adapters = vec![ClaudeAdapter::new().with_custom_root(f.home.join(".claude/projects"))];
    f.adapters
        .extend(create_archive_adapters(&create_adapters(), &moved));
    run_scan(&f.adapters, &f.store, &NullEvents, false).unwrap();
    assert_readable_from(&f, true);
    let meta = f.store.get_session(&key()).unwrap().unwrap();
    assert!(meta.file_path.starts_with(&*moved.to_string_lossy()));
    assert_eq!(std::fs::read(&meta.file_path).unwrap(), original);

    // 搬回默认位置
    let back = vault::relocate(&f.store, None).unwrap();
    assert_eq!(back, f.vault);
    assert!(!vault::is_custom(&f.store));
    assert!(!moved.exists());
    assert_eq!(
        std::fs::read(
            back.join(
                meta.file_path
                    .strip_prefix(&*moved.to_string_lossy())
                    .unwrap()
                    .trim_start_matches('/')
            )
        )
        .unwrap(),
        original
    );
}

#[test]
fn relocation_refuses_nested_targets() {
    let f = fixture();
    assert!(vault::relocate(&f.store, Some(&f.vault.join("inner"))).is_err());
    assert!(vault::relocate(&f.store, Some(Path::new("relative/dir"))).is_err());
    assert_eq!(vault::dir_for(&f.store).unwrap(), f.vault);
}
