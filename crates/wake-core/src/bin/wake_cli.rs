//! wake-cli:从终端查 Wake 的会话索引(只读)。
//!
//!   wake-cli search QUERY [OPTIONS]   全文搜索
//!   wake-cli sessions [OPTIONS]       最近的会话
//!   wake-cli show KEY [OPTIONS]       读一份转录
//!   wake-cli projects [OPTIONS]       有会话历史的项目
//!   wake-cli setup                    路径、进 PATH、给 agent 的说明
//!   wake-cli index                    库不存在时建一次(见 scanner::build_index)
//!   wake-cli --version | --help
//!
//! 输出就是 MCP 工具那份文本、一字不改(tests/cli.rs 逐字节卡),所以 docs 与
//! 将来的 Skill 只需描述一份格式。解析在 `wake_core::cli`,这里只做 I/O 与退
//! 出码。索引库一律只读打开(`mcp::open_index`),**绝不 open_or_rebuild**;
//! 唯一的写是 `index` 子命令——库不存在时建一次,规矩与理由在
//! `scanner::build_index`,别在这里复述或放宽。
//!
//! **结果不许走 println!**:Rust 忽略 SIGPIPE,`wake-cli show K | head` 会让
//! println! panic 成 101。所有输出统一经 cli::emit,BrokenPipe 由 write 收场。
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use wake_core::cli::{self, Action, CliError, Report, Stream};
use wake_core::db::{self, Store};
use wake_core::mcp::{self, tools};
use wake_core::models::SessionFilter;
use wake_core::scanner::{self, NullEvents};
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
        Action::Index => write(cli::report(index(&db_path(&inv.db)))),
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

/// 建一次索引,然后说一句人话。建不建、凭什么敢建全在 `scanner::build_index`,
/// 这里只剩措辞。场景是"装了 Wake 但从没启动过":skill 让 agent 跑这一条自救。
/// 失败一律走 `?`(anyhow → `ToolError::Internal`)交 `cli::report` 收场——
/// "跑起来了然后失败"退 1,与工具调用失败同一条出口,不另写一份映射
fn index(path: &Path) -> Result<String, tools::ToolError> {
    let Some(store) = scanner::build_index(path, &NullEvents)? else {
        return Ok(format!(
            "An index already exists at {}. Launch Wake to update or rebuild it.",
            path.display()
        ));
    };
    let sessions = store
        .list_sessions(&SessionFilter {
            limit: 1,
            ..Default::default()
        })?
        .1;
    let agents = store.agent_counts()?.len() as i64;
    Ok(format!(
        "Indexed {sessions} session{} from {agents} agent{} into {}.\n\
         Launch Wake to keep it current — it watches the agents' files while it runs.",
        plural(sessions),
        plural(agents),
        path.display()
    ))
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
