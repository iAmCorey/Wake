//! wake-mcp:Wake 索引的只读 MCP server(stdio)。
//!
//!   wake-mcp [--db PATH]          以 MCP server 身份服务 stdin/stdout(客户端这样起它)
//!   wake-mcp setup [--db PATH]    打印接入 Claude Code / Codex / Cursor 的配置片段
//!   wake-mcp call TOOL [JSON]     跑一次工具调用、打印文本(调试用)
//!   wake-mcp --version | --help
//!
//! 索引库默认取 GUI 同一份(`db::default_db_path`),只读打开;库不存在或太老
//! 直接以清晰的 stderr 错误退出(非零),让客户端的日志说明白"先启动一次 Wake"。
use std::io::{self, Write as _};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use wake_core::adapters::create_adapters_for;
use wake_core::db::Store;
use wake_core::mcp::{self, McpServer};

const USAGE: &str = "wake-mcp — read-only MCP server for your Wake session index

USAGE:
  wake-mcp [--db PATH]            serve MCP over stdio (what MCP clients run)
  wake-mcp setup [--db PATH]      print config snippets for Claude Code / Codex / Cursor
  wake-mcp call TOOL [JSON]       run one tool call and print its text (debugging)
  wake-mcp --version | --help

OPTIONS:
  --db PATH   index database (default: Wake's own, e.g. ~/Library/Application Support/wake/wake.db)";

struct Args {
    db: Option<PathBuf>,
    command: Vec<String>,
}

fn parse_args() -> Result<Args, String> {
    let mut db = None;
    let mut command = Vec::new();
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--db" => {
                db = Some(PathBuf::from(
                    it.next().ok_or("--db needs a path".to_string())?,
                ))
            }
            "--help" | "-h" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            "--version" | "-V" => {
                println!("wake-mcp {}", mcp::SERVER_VERSION);
                std::process::exit(0);
            }
            s if s.starts_with("--db=") => db = Some(PathBuf::from(&s[5..])),
            s if s.starts_with('-') => return Err(format!("unknown option {s}\n\n{USAGE}")),
            _ => command.push(a),
        }
    }
    Ok(Args { db, command })
}

fn db_path(db: &Option<PathBuf>) -> PathBuf {
    db.clone().unwrap_or_else(wake_core::db::default_db_path)
}

fn open(db: &Option<PathBuf>) -> Result<McpServer, String> {
    let store = Store::open_read_only(&db_path(db)).map_err(|e| format!("{e:#}"))?;
    let store = Arc::new(store);
    // 必须带上库里的 location / remote host 配置(不变量 8⑥),否则自定义根
    // 与远程缓存里的会话找不到能读它的 adapter
    let adapters = create_adapters_for(&store);
    Ok(McpServer::new(store, adapters))
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("wake-mcp: {e}");
            return ExitCode::from(2);
        }
    };
    match args.command.first().map(String::as_str) {
        None => {
            let server = match open(&args.db) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("wake-mcp: {e}");
                    return ExitCode::from(2);
                }
            };
            let stdin = io::stdin();
            let stdout = io::stdout();
            match server.serve(stdin.lock(), stdout.lock()) {
                Ok(()) => ExitCode::SUCCESS,
                // 客户端先走一步、管道断开是正常收场
                Err(e) if e.kind() == io::ErrorKind::BrokenPipe => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("wake-mcp: {e}");
                    ExitCode::from(1)
                }
            }
        }
        Some("setup") => {
            let bin = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("wake-mcp"));
            let mut out = io::stdout().lock();
            let _ = writeln!(out, "wake-mcp binary: {}\n", bin.display());
            for s in mcp::setup_snippets(&bin) {
                let _ = writeln!(out, "## {} — {}\n\n{}\n", s.client, s.hint, s.text);
            }
            // 只探库,不必为一行提示把十六家 roster 建起来
            if let Err(e) = Store::open_read_only(&db_path(&args.db)) {
                let _ = writeln!(out, "Note: {e:#}");
            }
            ExitCode::SUCCESS
        }
        Some("call") => {
            let Some(tool) = args.command.get(1) else {
                eprintln!("wake-mcp: call needs a tool name\n\n{USAGE}");
                return ExitCode::from(2);
            };
            let json_args = args.command.get(2).map(String::as_str).unwrap_or("{}");
            let params: serde_json::Value = match serde_json::from_str(json_args) {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("wake-mcp: arguments are not valid JSON: {e}");
                    return ExitCode::from(2);
                }
            };
            let server = match open(&args.db) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("wake-mcp: {e}");
                    return ExitCode::from(2);
                }
            };
            let request = serde_json::json!({
                "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                "params": { "name": tool, "arguments": params },
            });
            let reply = server.handle_line(&request.to_string()).unwrap_or_default();
            let reply: serde_json::Value = serde_json::from_str(&reply).unwrap_or_default();
            if let Some(err) = reply.get("error") {
                eprintln!("wake-mcp: {}", err["message"].as_str().unwrap_or("error"));
                return ExitCode::from(1);
            }
            let text = reply["result"]["content"][0]["text"]
                .as_str()
                .unwrap_or_default();
            println!("{text}");
            if reply["result"]["isError"].as_bool().unwrap_or(false) {
                return ExitCode::from(1);
            }
            ExitCode::SUCCESS
        }
        Some(other) => {
            eprintln!("wake-mcp: unknown command {other}\n\n{USAGE}");
            ExitCode::from(2)
        }
    }
}
