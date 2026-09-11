//! wake-mcp:把 Wake 的索引以 MCP(stdio 传输、JSON-RPC 2.0)**只读**暴露给别的
//! coding agent(Claude Code / Codex / Cursor …)。目标是"换一个 agent 不用重新
//! 解释自己做到哪了":对方按项目列最近会话、全文搜、读一段转录,全部来自本机
//! 索引与磁盘上的会话文件,零 LLM、零网络、不写库。
//!
//! 协议层手写而不引 rmcp:wake-core 全同步、没有 tokio,server 端真正要实现的
//! 只有 initialize / notifications/initialized / ping / tools/list / tools/call
//! 五个方法。消息按行分隔(stdio 传输规范:一行一条 JSON,不含裸换行),
//! stdout 只写协议,日志一律 stderr。需要 resources / prompts / sampling
//! 全家桶时再考虑换库。
//!
//! 索引库经 `Store::open_read_only` 打开——旁路进程绝不 `open_or_rebuild`,
//! 也不扫描;新鲜度靠 GUI 常驻的 watcher,每个工具返回都带"索引覆盖到的最新
//! 活动时间"。读会话是现场解析磁盘文件,不受索引新旧影响。

pub mod tools;

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{json, Value};

use crate::adapters::AgentAdapter;
use crate::db::Store;
use crate::models::AgentId;

/// 本方声明的协议版本(客户端请求的版本在 SUPPORTED 内就回显它,否则回这个)
pub const PROTOCOL_VERSION: &str = "2025-06-18";
/// 已知且工具子集完全兼容的版本
const SUPPORTED: [&str; 4] = ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];
pub const SERVER_NAME: &str = "wake";
pub const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// 随 initialize 下发给客户端的使用说明(LLM 会读)。重点是"什么时候该用":
/// 模型对陌生工具默认不碰,问"最近在做什么"会去翻 git log——要点明会话记录里
/// 有而 git 里没有的东西(讨论、决策、试过的路、停在哪)
const INSTRUCTIONS: &str = "Wake indexes every coding-agent session on this machine \
(Claude Code, Codex, Cursor, Gemini CLI, OpenCode and more) and exposes them read-only. \
Use these tools whenever the user refers to earlier conversations or sessions with any AI \
coding agent: what was discussed, decided or tried, why something was done a certain way, \
where previous work stopped, or whether an error was seen before. Git history and the \
working tree do not contain that; Wake does. Start with wake_list_sessions (pass the \
current working directory as `project`) or wake_search, then read the relevant transcript \
with wake_get_session. Nothing here can modify a session.";

const JSONRPC_PARSE_ERROR: i64 = -32700;
const JSONRPC_INVALID_REQUEST: i64 = -32600;
const JSONRPC_METHOD_NOT_FOUND: i64 = -32601;
const JSONRPC_INVALID_PARAMS: i64 = -32602;
const JSONRPC_INTERNAL_ERROR: i64 = -32603;

/// 一个 MCP server 实例:持有只读 Store 与按库配置构造的 roster
pub struct McpServer {
    store: Arc<Store>,
    adapters: Vec<Box<dyn AgentAdapter>>,
    transcripts: tools::TranscriptCache,
}

impl McpServer {
    pub fn new(store: Arc<Store>, adapters: Vec<Box<dyn AgentAdapter>>) -> Self {
        Self {
            store,
            adapters,
            transcripts: tools::TranscriptCache::default(),
        }
    }

    /// 阻塞服务到 stdin EOF。每行一条消息;写失败(客户端已退出)即返回
    pub fn serve(&self, input: impl BufRead, mut output: impl Write) -> std::io::Result<()> {
        for line in input.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            if let Some(reply) = self.handle_line(&line) {
                output.write_all(reply.as_bytes())?;
                output.write_all(b"\n")?;
                output.flush()?;
            }
        }
        Ok(())
    }

    /// 一行进、至多一行出(通知无回包)。批量数组逐条处理、按数组回;
    /// 测试与 `wake-mcp call` 直接用它,不经 stdio
    pub fn handle_line(&self, line: &str) -> Option<String> {
        let msg: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                return Some(
                    error_response(
                        Value::Null,
                        JSONRPC_PARSE_ERROR,
                        &format!("Parse error: {e}"),
                    )
                    .to_string(),
                )
            }
        };
        match msg {
            Value::Array(items) => {
                if items.is_empty() {
                    return Some(
                        error_response(Value::Null, JSONRPC_INVALID_REQUEST, "Invalid Request")
                            .to_string(),
                    );
                }
                let replies: Vec<Value> = items
                    .iter()
                    .filter_map(|m| self.handle_message(m))
                    .collect();
                (!replies.is_empty()).then(|| Value::Array(replies).to_string())
            }
            other => self.handle_message(&other).map(|v| v.to_string()),
        }
    }

    fn handle_message(&self, msg: &Value) -> Option<Value> {
        let obj = match msg.as_object() {
            Some(o) => o,
            None => {
                return Some(error_response(
                    Value::Null,
                    JSONRPC_INVALID_REQUEST,
                    "Invalid Request",
                ))
            }
        };
        let id = obj.get("id").cloned();
        let method = match obj.get("method").and_then(Value::as_str) {
            Some(m) => m,
            // 没有 method 的是"响应":本 server 从不向客户端发请求,静默丢弃
            None => {
                return id.filter(|id| !id.is_null()).and_then(|id| {
                    (!obj.contains_key("result") && !obj.contains_key("error"))
                        .then(|| error_response(id, JSONRPC_INVALID_REQUEST, "Invalid Request"))
                })
            }
        };
        let params = obj.get("params").cloned().unwrap_or(Value::Null);
        // 通知(无 id)不回包,不管认不认识
        let id = match id {
            Some(id) if !id.is_null() => id,
            _ => {
                if method == "notifications/initialized" {
                    eprintln!("wake-mcp: client initialized");
                }
                return None;
            }
        };
        let result = match method {
            "initialize" => Ok(self.initialize(&params)),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({ "tools": tools::definitions() })),
            "tools/call" => self.call_tool(&params),
            _ => Err((
                JSONRPC_METHOD_NOT_FOUND,
                format!("Method not found: {method}"),
            )),
        };
        Some(match result {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err((code, message)) => error_response(id, code, &message),
        })
    }

    fn initialize(&self, params: &Value) -> Value {
        let requested = params
            .get("protocolVersion")
            .and_then(Value::as_str)
            .unwrap_or("");
        let version = if SUPPORTED.contains(&requested) {
            requested
        } else {
            PROTOCOL_VERSION
        };
        json!({
            "protocolVersion": version,
            "capabilities": { "tools": { "listChanged": false } },
            "serverInfo": { "name": SERVER_NAME, "title": "Wake", "version": SERVER_VERSION },
            "instructions": INSTRUCTIONS,
        })
    }

    fn call_tool(&self, params: &Value) -> Result<Value, (i64, String)> {
        let name = params.get("name").and_then(Value::as_str).ok_or((
            JSONRPC_INVALID_PARAMS,
            "tools/call needs a string `name`".to_string(),
        ))?;
        let empty = json!({});
        let args = params.get("arguments").unwrap_or(&empty);
        if !args.is_object() && !args.is_null() {
            return Err((
                JSONRPC_INVALID_PARAMS,
                "`arguments` must be an object".to_string(),
            ));
        }
        match tools::invoke(&self.store, &self.adapters, &self.transcripts, name, args) {
            Ok(text) => Ok(tool_result(text, false)),
            // 参数形状错(含未知工具名)是协议错误;执行失败(坏 key、解析失败)
            // 是给 LLM 看的结果
            Err(tools::ToolError::InvalidParams(m)) => Err((JSONRPC_INVALID_PARAMS, m)),
            Err(tools::ToolError::Failed(m)) => Ok(tool_result(m, true)),
            Err(tools::ToolError::Internal(m)) => Err((JSONRPC_INTERNAL_ERROR, m)),
        }
    }
}

fn tool_result(text: String, is_error: bool) -> Value {
    json!({
        "content": [{ "type": "text", "text": text }],
        "isError": is_error,
    })
}

fn error_response(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

// ---------------------------------------------------------------- 安装辅助

/// 只读打开索引,并按库里的 location / remote host 配置建 roster(不变量
/// 8⑥:否则自定义根与远程缓存里的会话找不到能读它的 adapter)。旁路进程
/// (wake-mcp / wake-cli)共用这一条——**绝不 open_or_rebuild**,那会把 GUI
/// 正在写的库挪走重建
pub fn open_index(db: &Path) -> Result<(Arc<Store>, Vec<Box<dyn AgentAdapter>>), String> {
    let store = Arc::new(Store::open_read_only(db).map_err(|e| format!("{e:#}"))?);
    let adapters = crate::adapters::create_adapters_for(&store);
    Ok((store, adapters))
}

/// 与本进程同目录的某个辅助可执行文件(Windows 补 .exe)。打包时辅助 bin 与
/// Wake 主程序并排:macOS 在 Contents/MacOS,Linux/Windows 同目录。
/// current_exe 是系统调用,**别在 render 里问**——GUI 一律开窗时算一次
pub fn sibling_named(stem: &str) -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let name = if cfg!(target_os = "windows") {
        format!("{stem}.exe")
    } else {
        stem.to_string()
    };
    Some(exe.parent()?.join(name))
}

/// 一段可复制的接入配置
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupSnippet {
    /// 客户端名("Claude Code")
    pub client: &'static str,
    /// 对应的 agent(UI 取品牌图标用)
    pub agent: AgentId,
    /// 怎么用这段("Run in a terminal" / "Add to ~/.codex/config.toml")
    pub hint: &'static str,
    /// 复制按钮文案("Copy command" / "Copy config")——UI 只给按钮不展示片段
    pub copy_label: &'static str,
    pub text: String,
}

/// 三家客户端的接入片段(UI 的 Connect 页与 `wake-mcp setup` 同源)。路径一律
/// 按 JSON 字符串转义——TOML basic string 与 JSON 的转义子集兼容,Windows 的
/// 反斜杠与含空格路径都安全
pub fn setup_snippets(bin: &Path) -> Vec<SetupSnippet> {
    let quoted = serde_json::to_string(&bin.to_string_lossy()).unwrap_or_default();
    vec![
        SetupSnippet {
            client: "Claude Code",
            agent: AgentId::ClaudeCode,
            hint: "Run in a terminal",
            copy_label: "Copy command",
            text: format!("claude mcp add --scope user wake -- {quoted}"),
        },
        SetupSnippet {
            client: "Codex",
            agent: AgentId::Codex,
            hint: "Add to ~/.codex/config.toml",
            copy_label: "Copy config",
            text: format!("[mcp_servers.wake]\ncommand = {quoted}"),
        },
        SetupSnippet {
            client: "Cursor",
            agent: AgentId::Cursor,
            hint: "Merge into ~/.cursor/mcp.json",
            copy_label: "Copy config",
            text: format!(
                "{{\n  \"mcpServers\": {{\n    \"wake\": {{ \"command\": {quoted} }}\n  }}\n}}"
            ),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snippets_quote_paths_for_every_client() {
        let snippets = setup_snippets(Path::new(
            "/Applications/My Apps/Wake.app/Contents/MacOS/wake-mcp",
        ));
        assert_eq!(snippets.len(), 3);
        assert_eq!(
            snippets[0].text,
            "claude mcp add --scope user wake -- \"/Applications/My Apps/Wake.app/Contents/MacOS/wake-mcp\""
        );
        assert!(snippets[1]
            .text
            .starts_with("[mcp_servers.wake]\ncommand = \"/Applications/My Apps/"));
        let cursor: Value =
            serde_json::from_str(&snippets[2].text).expect("cursor snippet is valid JSON");
        assert_eq!(
            cursor["mcpServers"]["wake"]["command"],
            "/Applications/My Apps/Wake.app/Contents/MacOS/wake-mcp"
        );
        let win = setup_snippets(Path::new(r"C:\Program Files\Wake\wake-mcp.exe"));
        assert!(win[1]
            .text
            .contains(r#"command = "C:\\Program Files\\Wake\\wake-mcp.exe""#));
    }
}
