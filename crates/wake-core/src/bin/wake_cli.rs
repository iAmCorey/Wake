//! wake-cli:从终端查 Wake 的会话索引(只读)。
//!
//!   wake-cli search QUERY [OPTIONS]   全文搜索
//!   wake-cli sessions [OPTIONS]       最近的会话
//!   wake-cli show KEY [OPTIONS]       读一份转录
//!   wake-cli projects [OPTIONS]       有会话历史的项目
//!   wake-cli setup                    路径、进 PATH、给 agent 的说明
//!   wake-cli --version | --help
//!
//! 输出就是 MCP 工具那份文本、一字不改(tests/cli.rs 逐字节卡),所以 docs 与
//! 将来的 Skill 只需描述一份格式。解析在 `wake_core::cli`,这里只做 I/O 与退
//! 出码。索引库只读打开,绝不 open_or_rebuild、不扫描、不写。
//!
//! **结果不许走 println!**:Rust 忽略 SIGPIPE,`wake-cli show K | head` 会让
//! println! panic 成 101。所有输出统一经 cli::emit,BrokenPipe 由 write 收场。
use std::io::{self, Write as _};
use std::path::PathBuf;
use std::process::ExitCode;

use wake_core::adapters::create_adapters_for;
use wake_core::cli::{self, Action, CliError, Report, Stream};
use wake_core::db::{self, Store};
use wake_core::mcp::{self, tools};
use wake_core::models::SessionFilter;
use wake_core::scanner::{run_scan, NullEvents};
use wake_core::text::plural;

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
        Action::Index => write(index(&inv.db)),
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
        Err(e) => {
            return write(Report {
                stream: Stream::Stderr,
                text: format!("wake-cli: {e}"),
                code: 2,
            })
        }
    };
    let cache = tools::TranscriptCache::default();
    write(cli::report(tools::invoke(
        &store, &adapters, &cache, tool, args,
    )))
}

/// 从零建一次索引。**只在索引文件不存在时**建:已存在的库归 GUI 管。
/// "旁路进程绝不 open_or_rebuild" 是为了不和 GUI 正在写的库打架,而
/// `SCAN_GATE` 只是**进程级**互斥、跨进程不管用——库根本不存在的那一刻
/// 没有任何写入方可冲突,所以这道口子只开这么宽,**别加 --force**。
/// 场景是"装了 Wake 但从没启动过":skill 让 agent 跑这一条就能自救
fn index(db: &Option<PathBuf>) -> Report {
    let path = db_path(db);
    if path.exists() {
        return ok(format!(
            "An index already exists at {}. Launch Wake to update or rebuild it.",
            path.display()
        ));
    }
    if let Some(dir) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(dir) {
            return fail(format!("could not create {}: {e}", dir.display()));
        }
    }
    let store = match Store::open(&path) {
        Ok(s) => std::sync::Arc::new(s),
        Err(e) => return fail(format!("{e:#}")),
    };
    // 不变量 8⑥:按库里的 location 配置建 roster(新库即内置十六家)
    let adapters = create_adapters_for(&store);
    if let Err(e) = run_scan(&adapters, &store, &NullEvents, true) {
        return fail(format!("{e:#}"));
    }
    let sessions = store
        .list_sessions(&SessionFilter {
            limit: 1,
            ..Default::default()
        })
        .map(|(_, total)| total)
        .unwrap_or(0);
    let agents = store.agent_counts().map(|c| c.len() as i64).unwrap_or(0);
    ok(format!(
        "Indexed {sessions} session{} from {agents} agent{} into {}.\n\
         Launch Wake to keep it current — it watches the agents' files while it runs.",
        plural(sessions),
        plural(agents),
        path.display()
    ))
}

/// 跑起来了然后失败 → 退 1(与 `cli::report` 对 Failed/Internal 同档)
fn fail(message: String) -> Report {
    Report {
        stream: Stream::Stderr,
        text: format!("wake-cli: {message}"),
        code: 1,
    }
}

fn setup(db: &Option<PathBuf>) -> String {
    let cli_bin = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("wake-cli"));
    // sibling_named 取的就是 current_exe 同级目录,从 wake-cli 调恰好对
    let mcp_bin = mcp::sibling_named("wake-mcp").filter(|p| p.exists());
    let path = db_path(db);
    // 只探库,不必为一行提示把十六家 roster 建起来
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
