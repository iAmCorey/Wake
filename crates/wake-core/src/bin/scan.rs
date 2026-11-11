//! 数据层黄金对比 CLI:扫描全部会话入库并打印统计,
//! 与 Electron 版基准(claude 97 / codex 168 / messages 31871±)对照。
use anyhow::Result;
use std::sync::Arc;
use wake_core::adapters::create_adapters_for;
use wake_core::db::{IndexLock, Ownership, Store};
use wake_core::models::SessionFilter;
use wake_core::scanner::{run_scan, NullEvents, ScanEvents, ScanProgress};

struct StderrEvents;
impl ScanEvents for StderrEvents {
    fn on_progress(&self, p: &ScanProgress) {
        if p.total > 0 {
            eprint!("\rIndexing {}/{}   ", p.done, p.total);
        }
    }
    fn on_sessions_changed(&self) {}
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let full = args.iter().any(|a| a == "--full");
    let db_path = if args.iter().any(|a| a == "--tmp") {
        std::env::temp_dir().join("wake-scan-test.db")
    } else {
        wake_core::db::default_db_path()
    };
    eprintln!("DB: {}", db_path.display());
    // 写真实索引的进程都得持 IndexLock(`Store::open` 的约定):GUI 在跑就别扫
    let _lock = match IndexLock::try_acquire(&db_path, "scan")? {
        Ownership::Ours(lock) => lock,
        Ownership::Held(holder) => anyhow::bail!(
            "index at {} is held by {holder}; quit Wake first, or use --tmp",
            db_path.display()
        ),
    };

    let store = Arc::new(Store::open(&db_path)?);
    // 必须带上库里的 location 配置:默认 roster 会把自定义根会话当已删清掉
    // 归档先镜像、再组 roster(roster 按 vault 目录在不在决定挂不挂归档实例)。
    // --tmp 的隔离库不建归档:它只是拿真实数据试扫一遍
    if !args.iter().any(|a| a == "--tmp") {
        wake_core::vault::ensure(&store);
        if let Some(stats) = wake_core::vault::sync_for(&store) {
            eprintln!("vault: {stats:?}");
        }
    }
    let adapters = create_adapters_for(&store);
    eprintln!(
        "adapters: {:?}",
        adapters
            .iter()
            .map(|a| a.agent().as_str())
            .collect::<Vec<_>>()
    );

    let t0 = std::time::Instant::now();
    let events: Box<dyn ScanEvents> = if args.iter().any(|a| a == "--quiet") {
        Box::new(NullEvents)
    } else {
        Box::new(StderrEvents)
    };
    run_scan(&adapters, &store, events.as_ref(), full)?;
    eprintln!();
    println!("Scan took {:.1}s", t0.elapsed().as_secs_f64());

    for (agent, count) in store.agent_counts()? {
        println!("{agent}: {count} sessions");
    }
    let (sessions, total) = store.list_sessions(&SessionFilter {
        limit: 5,
        include_archived: true,
        ..Default::default()
    })?;
    println!("Total sessions (incl. archived): {total}");
    println!("Latest 5:");
    for s in sessions {
        println!(
            "  [{}] {} — {} ({} msgs)",
            s.agent.as_str(),
            s.title.chars().take(30).collect::<String>(),
            s.project_name,
            s.message_count
        );
    }

    // 搜索冒烟
    if let Some(q) = args
        .iter()
        .position(|a| a == "--search")
        .map(|i| args.get(i + 1))
        .flatten()
    {
        let t1 = std::time::Instant::now();
        let (hits, degraded) = store.search(q, &[], None, 10)?;
        println!(
            "搜索 \"{q}\": {} hits, degraded={degraded}, {:.0}ms",
            hits.len(),
            t1.elapsed().as_secs_f64() * 1000.0
        );
        for h in hits.iter().take(3) {
            println!(
                "  {} #{}: {}",
                h.session.title.chars().take(24).collect::<String>(),
                h.seq,
                h.snippet
                    .chars()
                    .take(60)
                    .collect::<String>()
                    .replace('\n', " ")
            );
        }
    }
    Ok(())
}
