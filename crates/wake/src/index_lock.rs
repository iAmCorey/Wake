//! 索引写锁在 GUI 里的生命期:跟着 Workbench 与它派出的写线程走,**不跟进程走**。macOS 关掉
//! 主窗、app 留在 Dock 时 Workbench 已释放、watcher 随之停了;进程若还持锁,定时跑的
//! `wake-cli refresh` 会一直退让、索引就此停摆(Codex review 2026-09-23)。Workbench 持一份
//! `Arc`,每条会写库的线程 / 后台任务各持一份克隆(扫描、远程同步、记忆同步、删除、清理),
//! 最后一份放掉锁才释放;watcher 在 Workbench 的字段里排在锁前面,drop 时先 join 再放锁。
//! Dock 重开新建 Workbench 时旧扫描线程可能还在跑、那份 Arc 还活着,就在这里 upgrade 共用——
//! 否则同进程第二次 try_lock 会撞上自己的 fd,把自己当成"另一个 Wake"劝退
use std::path::Path;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use wake_core::db::{IndexLock, Wait, LOCK_KIND_APP};

static SLOT: Mutex<Weak<IndexLock>> = Mutex::new(Weak::new());

/// 拿到就 Some。持有者是另一个 Wake、或等满 60s 仍拿不到就弹窗退出——两个 GUI 写同一个库
/// 正是 wake-core SCAN_GATE 那段注释想挡而挡不住的事;拿锁本身出错(不给建议锁的文件系统)
/// 不该把 app 拦在门外,记一行、None 照常起。阻塞调用方:只在开窗之前的 Workbench::new 里调
pub fn take(db: &Path) -> Option<Arc<IndexLock>> {
    let mut slot = SLOT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(live) = slot.upgrade() {
        return Some(live);
    }
    let me = format!("{LOCK_KIND_APP} {}", std::process::id());
    let deadline = Instant::now() + Duration::from_secs(60);
    let holder = loop {
        match IndexLock::acquire_or_wait(db, LOCK_KIND_APP, Duration::from_secs(60)) {
            Ok(Wait::Ours(lock)) => {
                let lock = Arc::new(lock);
                *slot = Arc::downgrade(&lock);
                return Some(lock);
            }
            // 上一份的最后一个持有者刚放手、fd 还没关上的那一瞬,撞见的是自己:等一下再拿
            Ok(Wait::HeldByApp(holder))
                if holder.to_string() == me && Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(10));
            }
            Ok(Wait::HeldByApp(holder) | Wait::TimedOut(holder)) => break holder,
            Err(e) => {
                eprintln!(
                    "wake: index lock unavailable at {}: {e}; continuing without it",
                    db.display()
                );
                return None;
            }
        }
    };
    // 点了 wake:// 链接时 Wake 恰好正在启动:两个进程都以为自己是第一个,后拿锁的这个把链接
    // 交给先拿到的、安静退出,别弹"另一个进程在用"(#82;持有者是 CLI 时 forward_to_running
    // 探不到 GUI 锁,照常报)
    if crate::links::forward_to_running(db) {
        std::process::exit(0);
    }
    wake_core::services::terminal::show_fatal_alert(&crate::tf!(
        "Wake couldn't open its index at {}: another process is using it ({}). Wait for it to finish or quit it, then open Wake again.",
        db.display(),
        holder
    ));
    std::process::exit(1);
}
