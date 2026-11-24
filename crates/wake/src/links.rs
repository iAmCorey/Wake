//! `wake://` 链接(#82)在 GUI 这一侧:收链接、转给运行中的 Wake。解析、收件箱与 Windows 的
//! 协议登记在 `wake_core::links`。
//!
//! - macOS:系统按 Info.plist 的 CFBundleURLTypes 把链接交给 app(冷启动也是),gpui 的
//!   `on_open_urls` 收——这一版 gpui 只在 macOS 上调它。
//! - Windows / Linux:链接是命令行参数。已经有一个 Wake 窗口用着同一个库(它持着索引的 GUI
//!   锁)时,新进程把链接写进收件箱、自己不起;运行中的那个监听收件箱
use std::path::Path;
use std::sync::OnceLock;

use futures::channel::mpsc::UnboundedSender;

/// 启动参数里的 wake:// 链接(Windows / Linux 由系统这样交进来)
pub fn startup_links() -> &'static [String] {
    static LINKS: OnceLock<Vec<String>> = OnceLock::new();
    LINKS.get_or_init(|| wake_core::links::links_in_args(std::env::args_os().skip(1)))
}

/// 已经有一个 Wake 窗口用着这个库:把启动参数里的链接交给它,返回 true(调用方随即退出、
/// 不开窗)。macOS 上不会有为了链接而起的新进程(系统直接交给运行中的 app),恒为 false
pub fn forward_to_running(db: &Path) -> bool {
    !cfg!(target_os = "macos") && wake_core::links::forward_to_running(db, startup_links())
}

/// 运行中的这个 Wake 监听收件箱,别的进程转来的链接交给 `tx`(Windows / Linux)。先挂上监听
/// 再取一遍:抢锁那一瞬转过来的、我们起来之前放进去的都不漏
#[cfg_attr(target_os = "macos", allow(dead_code))]
pub fn listen(db: &Path, tx: UnboundedSender<String>) {
    let inbox = wake_core::links::inbox_dir(db);
    let watched = inbox.clone();
    let sender = tx.clone();
    if let Err(e) = wake_core::links::watch(&inbox, move || deliver(&watched, &sender)) {
        eprintln!("wake: can't watch {} for links: {e}", inbox.display());
    }
    deliver(&inbox, &tx);
}

fn deliver(inbox: &Path, tx: &UnboundedSender<String>) {
    for url in wake_core::links::take(inbox) {
        let _ = tx.unbounded_send(url);
    }
}
