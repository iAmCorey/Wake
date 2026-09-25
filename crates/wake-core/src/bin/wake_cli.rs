//! wake-cli:从终端查 Wake 的会话索引(查询只读;`index` / `refresh` 写它)。
//!
//!   wake-cli search QUERY [OPTIONS]   全文搜索
//!   wake-cli sessions [OPTIONS]       最近的会话
//!   wake-cli show KEY [OPTIONS]       读一份转录
//!   wake-cli projects [OPTIONS]       有会话历史的项目
//!   wake-cli setup                    路径、进 PATH、给 agent 的说明
//!   wake-cli index                    库不存在时建一次(见 scanner::build_index)
//!   wake-cli refresh                  库已在、Wake 没开时增量刷一轮(见 scanner::refresh_index)
//!   wake-cli --version | --help
//!
//! 输出就是 MCP 工具那份文本、一字不改(tests/cli.rs 逐字节卡),所以 docs 与
//! 将来的 Skill 只需描述一份格式。解析在 `wake_core::cli`,这里只做 I/O 与退
//! 出码。查询一律只读打开索引(`mcp::open_index`),**绝不 open_or_rebuild**;
//! 写库的只有 `index` 与 `refresh` 两个子命令,都先拿 `db::IndexLock`——规矩与
//! 理由在 `scanner::build_index` / `refresh_index`,别在这里复述或放宽。
//!
//! **结果不许走 println!**:Rust 忽略 SIGPIPE,`wake-cli show K | head` 会让
//! println! panic 成 101。所有输出统一经 cli::emit,BrokenPipe 由 write 收场。
use std::io::{self, IsTerminal as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use wake_core::cli::{self, Action, CliError, Report, Stream};
use wake_core::db::{self, LockHolder, Store};
use wake_core::mcp::{self, tools};
use wake_core::scanner::{self, NullEvents, Outcome, ScanEvents, ScanProgress, Skip};

fn main() -> ExitCode {
    // args()(而非 args_os)对非 UTF-8 参数是 panic,退出码 101 —— Linux 上
    // 目录名可以不是 UTF-8,而 `--project "$PWD"` 正是主用法,不能崩
    let argv: Vec<String> = match std::env::args_os()
        .skip(1)
        .map(|a| a.into_string())
        .collect()
    {
        Ok(v) => v,
        Err(bad) => {
            return write(
                CliError {
                    message: format!("argument is not valid UTF-8: {}", bad.to_string_lossy()),
                    with_usage: false,
                }
                .report(),
            )
        }
    };
    let inv = match cli::parse(&argv) {
        Ok(i) => i,
        Err(e) => return write(e.report()),
    };
    match inv.action {
        Action::Help => write(ok(cli::help())),
        Action::Version => write(ok(format!("wake-cli {}", mcp::SERVER_VERSION))),
        Action::Setup => write(ok(setup(&inv.db))),
        Action::Index => write(index(&db_path(&inv.db))),
        Action::Refresh => write(refresh(&db_path(&inv.db))),
        Action::Tool { tool, args } => run(&inv.db, tool, &args),
    }
}

fn ok(text: String) -> Report {
    Report {
        stream: Stream::Stdout,
        text,
        code: 0,
    }
}

/// db 路径只在真要用库时求值:`path_or_default` 落到 default_db_path 时有副
/// 作用(首次会把旧 vibex 库拷过来),--help / --version 不该碰用户的文件系统
fn db_path(db: &Option<PathBuf>) -> PathBuf {
    db::path_or_default(db.as_deref())
}

fn run(db: &Option<PathBuf>, tool: &str, args: &serde_json::Value) -> ExitCode {
    // 只读打开 + 按库配置建 roster,与 wake-mcp 同一条(不变量 8⑥);库不存在
    // 或太老由 open_read_only 给出 "launch Wake once"
    let (store, adapters) = match mcp::open_index(&db_path(db)) {
        Ok(x) => x,
        Err(e) => return write(refuse(e)),
    };
    let cache = tools::TranscriptCache::default();
    write(cli::report(tools::invoke(
        &store, &adapters, &cache, tool, args,
    )))
}

/// 建一次索引,然后说一句人话。建不建、凭什么敢建全在 `scanner::build_index`,
/// 这里只剩措辞。场景是"装了 Wake 但从没启动过":skill 让 agent 跑这一条自救
fn index(path: &Path) -> Report {
    match scanner::build_index(path, progress_events()) {
        Ok(Outcome::Done(tally)) => cli::report(Ok(format!(
            "Indexed {tally} into {}.\n\
             Launch Wake to keep it current — it watches the agents' files while it runs. \
             With the app closed, `wake-cli refresh` brings the index up to date.",
            path.display()
        ))),
        Ok(Outcome::Skipped(skip)) => skipped(path, skip),
        Ok(Outcome::Busy(holder)) => cli::report(Ok(busy_text(&holder))),
        Err(e) => cli::report(Err(e.into())),
    }
}

/// 增量刷一轮已有的索引,然后说一句人话。刷不刷、凭什么敢刷全在 `scanner::refresh_index`
fn refresh(path: &Path) -> Report {
    match scanner::refresh_index(path, progress_events()) {
        Ok(Outcome::Done(tally)) => cli::report(Ok(format!(
            "Refreshed the index at {}: {tally}.",
            path.display()
        ))),
        Ok(Outcome::Skipped(skip)) => skipped(path, skip),
        Ok(Outcome::Busy(holder)) => cli::report(Ok(busy_text(&holder))),
        Err(e) => cli::report(Err(e.into())),
    }
}

/// 没干活的缘由翻成人话:库已在是正常收场(退 0);其余三种都是 `--db` 拿错了路径——与
/// 查询命令的缺库同款,退 2。"跑起来了然后失败"的那类(退 1)全在 `cli::report`
fn skipped(path: &Path, skip: Skip) -> Report {
    match skip {
        Skip::Exists => cli::report(Ok(format!(
            "An index already exists at {}. Launch Wake to update or rebuild it, \
             or run `wake-cli refresh` while Wake is closed.",
            path.display()
        ))),
        Skip::Missing => refuse(format!(
            "{}, or run `wake-cli index`",
            db::missing_index(path)
        )),
        Skip::NotAnIndex => refuse(format!(
            "{} is not a Wake index — pass Wake's own database (`wake-cli setup` prints its path)",
            path.display()
        )),
        Skip::MirrorsElsewhere => refuse(format!(
            "{} is not the path Wake opens this index at — its remote-host mirrors are not next \
             to it; run refresh on the path `wake-cli setup` prints",
            path.display()
        )),
    }
}

/// 拿错了库的诊断:stderr、`wake-cli: ` 前缀、退 2——与 argv 错同一条出口
/// (`CliError::report`),别在这里手拼 Report;`cli::report` 那张表里的 2 是"值不对",不走它
fn refuse(message: String) -> Report {
    CliError {
        message,
        with_usage: false,
    }
    .report()
}

/// 索引被别的进程持有时说的话:GUI 不会中途让出,直说它在跑;另一个 wake-cli
/// 几秒就完,点名让人看得出是谁
fn busy_text(holder: &LockHolder) -> String {
    if holder.app {
        "Wake is running and keeps the index current itself; nothing to do.".to_string()
    } else {
        format!("Another process is writing the index right now ({holder}); nothing to do.")
    }
}

/// 进度只给终端看:扫描要几秒到几十秒,没有反馈像卡死;管道与 agent 调用时
/// stderr 保持安静(它是诊断通道,tests/cli.rs 也这么断言)
fn progress_events() -> &'static dyn ScanEvents {
    if io::stderr().is_terminal() {
        &TtyProgress
    } else {
        &NullEvents
    }
}

/// `index` / `refresh` 在终端上的进度:格式在 `cli::progress_line`(有单测),这里只负责
/// "写到 stderr"。写失败一律忽略——进度条不是结果;stderr 无缓冲,不需要 flush
struct TtyProgress;

impl ScanEvents for TtyProgress {
    fn on_progress(&self, p: &ScanProgress) {
        let _ = cli::progress_line(p, &mut io::stderr().lock());
    }
    fn on_sessions_changed(&self) {}
}

fn setup(db: &Option<PathBuf>) -> String {
    let cli_bin = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("wake-cli"));
    // sibling_named 取的就是 current_exe 同级目录,从 wake-cli 调恰好对
    let mcp_bin = mcp::sibling_file("wake-mcp");
    let path = db_path(db);
    // 只探库,不必为一行提示把二十二家 roster 建起来
    let db_error = Store::open_read_only(&path).err().map(|e| format!("{e:#}"));
    cli::setup_text(&cli::SetupFacts {
        cli_bin: &cli_bin,
        mcp_bin: mcp_bin.as_deref(),
        db: &path,
        db_error,
    })
}

/// 唯一的输出出口。下游关掉管道(`wake-cli search x | head`)是正常收场,保持
/// 原本的退出码;stderr 也可能被关,同理别 panic
fn write(r: Report) -> ExitCode {
    let res = match r.stream {
        Stream::Stdout => cli::emit(&r.text, &mut io::stdout().lock()),
        Stream::Stderr => cli::emit(&r.text, &mut io::stderr().lock()),
    };
    match res {
        Ok(()) => ExitCode::from(r.code),
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => ExitCode::from(r.code),
        Err(e) => {
            let _ = writeln!(io::stderr(), "wake-cli: {e}");
            ExitCode::from(1)
        }
    }
}
