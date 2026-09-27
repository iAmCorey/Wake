//! Devin 默认数据根的解析契约。`DevinAdapter::new()` 的候选链
//! (XDG → `~/.local/share` → macOS Application Support → Windows
//! `%APPDATA%`)依赖进程级 `WAKE_HOME`/`XDG_DATA_HOME`,因此整个文件只有
//! 一个用例、串行覆盖两种布局(与 mcp_stdio/remote_sync 同一约定):
//! 1. 只有 `%APPDATA%\devin\cli\sessions.db`(真实 Windows 机器的形态)时,
//!    默认解析必须落到它——此前 Windows 上没有任何候选,整家会话空列;
//! 2. XDG 形态(`~/.local/share/devin`)同时存在时仍按序优先(跨平台回归)。

use std::fs;
use wake_core::adapters::devin::DevinAdapter;
use wake_core::adapters::AgentAdapter;

mod common;

#[test]
fn devin_default_root_resolves_windows_appdata() {
    // 独立假家:进程级 env 之外不受 adapter_contracts 的共享 fixture 影响
    let home = tempfile::Builder::new()
        .prefix("wake-devin-default-root-")
        .tempdir()
        .expect("create fake home");
    let appdata_dir = home
        .path()
        .join("AppData")
        .join("Roaming")
        .join("devin")
        .join("cli");
    fs::create_dir_all(&appdata_dir).expect("mkdir AppData devin cli");
    let appdata_db = appdata_dir.join("sessions.db");
    common::build_devin_db(&appdata_db);

    // 老家目录清理 + 改道,先只给 AppData 一份(Windows 真机形态)
    std::env::set_var("WAKE_HOME", home.path());
    std::env::set_var("HOME", home.path());
    std::env::remove_var("XDG_DATA_HOME");

    let adapter = DevinAdapter::new();
    assert_eq!(
        adapter.data_roots(),
        vec![appdata_db.clone()],
        "只有 %APPDATA%\\devin 一份库时,默认解析必须命中它"
    );
    let ids: Vec<String> = adapter
        .list_session_files()
        .expect("devin list")
        .into_iter()
        .map(|r| r.native_id)
        .collect();
    assert_eq!(ids, vec!["dv-0001", "dv-0002"], "AppData 布局整家可列");

    // XDG 形态出现后仍按候选序优先:老平台用户的会话不能换根消失
    let xdg_dir = home
        .path()
        .join(".local")
        .join("share")
        .join("devin")
        .join("cli");
    fs::create_dir_all(&xdg_dir).expect("mkdir XDG devin cli");
    let xdg_db = xdg_dir.join("sessions.db");
    common::build_devin_db(&xdg_db);
    let adapter = DevinAdapter::new();
    assert_eq!(
        adapter.data_roots(),
        vec![xdg_db],
        "XDG 与 AppData 并存时按候选序取 XDG 形态"
    );
}
