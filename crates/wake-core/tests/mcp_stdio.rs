//! wake-mcp 的端到端契约:合成 fixtures 建临时库 → 以真实 bin 起子进程 → 走
//! stdio JSON-RPC。卡住:工具清单稳定;search 命中的 seq 等于 parse_transcript
//! 的序号(seq 契约延伸到 MCP);get_session 分页的 next_seq 逐页衔接、覆盖全部
//! 非 Meta 消息;全程只读(库文件字节不变);坏 key 是 isError 而非崩溃;未知
//! 方法 -32601;库不存在时进程以清晰错误退出。
//!
//! WAKE_HOME 是进程级环境且子进程继承它,整个文件只有一个用例。

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::Arc;

use serde_json::{json, Value};
use wake_core::adapters::{adapter_for, create_adapter_roster_for};
use wake_core::db::Store;
use wake_core::models::*;
use wake_core::scanner::{run_scan, NullEvents};

mod common;

const CLAUDE_KEY: &str = "claude-code:11111111-aaaa-bbbb-cccc-000000000001";
const CLAUDE_PROJECT: &str = "/Users/tester/Github/wakefx";

struct Client {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    next_id: i64,
}

impl Client {
    fn spawn(db: &Path) -> Client {
        let mut child = Command::new(env!("CARGO_BIN_EXE_wake-mcp"))
            .arg("--db")
            .arg(db)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn wake-mcp");
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        Client {
            child,
            stdin: Some(stdin),
            stdout,
            next_id: 0,
        }
    }

    fn send_raw(&mut self, line: &str) {
        let stdin = self.stdin.as_mut().unwrap();
        stdin.write_all(line.as_bytes()).unwrap();
        stdin.write_all(b"\n").unwrap();
        stdin.flush().unwrap();
    }

    fn read_reply(&mut self) -> Value {
        let mut line = String::new();
        let n = self.stdout.read_line(&mut line).expect("read reply");
        assert!(n > 0, "wake-mcp closed stdout unexpectedly");
        serde_json::from_str(&line).unwrap_or_else(|e| panic!("reply is not JSON: {e}: {line}"))
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        self.send_raw(
            &json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }).to_string(),
        );
        let reply = self.read_reply();
        assert_eq!(reply["jsonrpc"], "2.0");
        assert_eq!(reply["id"], id, "reply id mismatch: {reply}");
        reply
    }

    fn notify(&mut self, method: &str) {
        self.send_raw(&json!({ "jsonrpc": "2.0", "method": method }).to_string());
    }

    /// tools/call → (text, isError);协议错误直接 panic
    fn call(&mut self, tool: &str, args: Value) -> (String, bool) {
        let reply = self.request("tools/call", json!({ "name": tool, "arguments": args }));
        assert!(
            reply.get("error").is_none(),
            "{tool} returned a protocol error: {}",
            reply["error"]
        );
        let result = &reply["result"];
        let text = result["content"][0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("{tool} result has no text content: {result}"))
            .to_string();
        (text, result["isError"].as_bool().unwrap_or(false))
    }

    fn finish(mut self) {
        drop(self.stdin.take());
        let status = self.child.wait().expect("wait wake-mcp");
        assert!(status.success(), "wake-mcp exited with {status}");
    }
}

/// `### [seq N]` 标题里的 seq,按出现顺序
fn seqs_in(text: &str) -> Vec<i64> {
    text.lines()
        .filter_map(|l| l.strip_prefix("### [seq "))
        .filter_map(|rest| rest.split(']').next()?.parse().ok())
        .collect()
}

fn next_seq_hint(text: &str) -> Option<i64> {
    text.lines()
        .find_map(|l| l.split("from_seq=").nth(1))
        .and_then(|rest| rest.trim_end_matches('.').trim().parse().ok())
}

#[test]
fn stdio_contract_end_to_end() {
    let tmp = tempfile::Builder::new()
        .prefix("wake-mcp-e2e-")
        .tempdir()
        .unwrap();
    let home = tmp.path().join("home");
    common::stage_dir_fixtures(&home);
    common::stage_sidecars(&home);
    // 子进程继承这些:它的 roster 也要指向 fixture home,别读到开发机的真实数据
    common::isolate_home(&home);

    // 1. 建库 + 全量扫描,并从本进程算出 seq 契约的参照
    let db = tmp.path().join("wake.db");
    let expected_seqs: Vec<i64>;
    let qr_seq: i64;
    let total_sessions: i64;
    {
        let store = Arc::new(Store::open(&db).unwrap());
        let roster = create_adapter_roster_for(&store);
        run_scan(&roster.active, &store, &NullEvents, true).expect("scan ok");
        let (_, total) = store
            .list_sessions(&SessionFilter {
                limit: 1,
                ..Default::default()
            })
            .unwrap();
        total_sessions = total;
        let meta = store
            .get_session(CLAUDE_KEY)
            .unwrap()
            .expect("claude fixture indexed");
        assert_eq!(meta.project_path, CLAUDE_PROJECT);
        let adapter = adapter_for(&roster.active, meta.agent, &meta.file_path).unwrap();
        let t = adapter
            .parse_transcript(&SessionFileRef::from_meta(&meta))
            .unwrap();
        expected_seqs = t
            .mainline
            .iter()
            .filter(|m| m.kind != MessageKind::Meta)
            .map(|m| m.seq)
            .collect();
        qr_seq = t
            .mainline
            .iter()
            .find(|m| m.text.contains("二维码"))
            .map(|m| m.seq)
            .expect("fixture has the 二维码 prompt");
        // Store 在这里关闭:最后一个连接落盘 checkpoint 之后再拍库文件快照
    }
    assert!(
        total_sessions > 10,
        "fixture home should index many sessions"
    );
    let before = std::fs::read(&db).unwrap();

    // 2. 握手与协议面
    let mut c = Client::spawn(&db);
    let init = c.request(
        "initialize",
        json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": { "name": "wake-test", "version": "0" }
        }),
    );
    assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(init["result"]["serverInfo"]["name"], "wake");
    assert!(init["result"]["capabilities"]["tools"].is_object());
    assert!(init["result"]["instructions"]
        .as_str()
        .unwrap()
        .contains("wake_search"));
    c.notify("notifications/initialized");
    assert_eq!(c.request("ping", json!({}))["result"], json!({}));

    let tools = c.request("tools/list", json!({}));
    let names: Vec<&str> = tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "wake_search",
            "wake_list_sessions",
            "wake_get_session",
            "wake_list_projects"
        ]
    );

    let unknown = c.request("resources/list", json!({}));
    assert_eq!(unknown["error"]["code"], -32601);
    let bad_tool = c.request(
        "tools/call",
        json!({ "name": "wake_delete_everything", "arguments": {} }),
    );
    assert_eq!(bad_tool["error"]["code"], -32602);
    let bad_since = c.request(
        "tools/call",
        json!({ "name": "wake_list_sessions", "arguments": { "since": "yesterday" } }),
    );
    assert_eq!(bad_since["error"]["code"], -32602, "参数形状错是协议错误");
    // 非 ASCII 收尾的 since 曾让 split_at 在字符中间 panic、整个 server 退出
    let cjk_since = c.request(
        "tools/call",
        json!({ "name": "wake_list_sessions", "arguments": { "since": "7天" } }),
    );
    assert_eq!(cjk_since["error"]["code"], -32602, "{cjk_since}");
    c.send_raw("{not json");
    let parse_err = c.read_reply();
    assert_eq!(parse_err["error"]["code"], -32700);
    assert!(parse_err["id"].is_null());

    // 3. search 命中的 seq 与 parse_transcript 的序号一致(seq 契约)。fixture
    // home 里十几家都有 "二维码" 提示词,按 agent 收窄让断言不依赖 bm25 排位
    let (everywhere, is_err) = c.call("wake_search", json!({ "query": "二维码", "limit": 30 }));
    assert!(!is_err, "{everywhere}");
    assert!(
        everywhere.contains("sessions match `二维码`"),
        "{everywhere}"
    );
    let (text, is_err) = c.call(
        "wake_search",
        json!({ "query": "二维码", "agents": ["claude"], "project": CLAUDE_PROJECT }),
    );
    assert!(!is_err, "{text}");
    assert!(text.contains(CLAUDE_KEY), "{text}");
    let expected_ref = format!("ref: wake://session/{CLAUDE_KEY}#{qr_seq}");
    assert!(
        text.contains(&expected_ref),
        "search hit must cite seq {qr_seq}:\n{text}"
    );
    assert!(text.contains("Index covers activity up to"));

    // 4. get_session:引用形态起点、分页衔接、覆盖全部非 Meta 消息
    let (page, is_err) = c.call(
        "wake_get_session",
        json!({ "key": format!("wake://session/{CLAUDE_KEY}#{qr_seq}"), "max_messages": 1 }),
    );
    assert!(!is_err, "{page}");
    assert_eq!(seqs_in(&page), vec![qr_seq]);
    assert!(page.contains("二维码"));
    assert!(page.contains("key: `claude-code:"));

    let mut visited = Vec::new();
    let mut from = 0;
    for _ in 0..50 {
        let (page, is_err) = c.call(
            "wake_get_session",
            json!({ "key": CLAUDE_KEY, "from_seq": from, "max_messages": 2 }),
        );
        assert!(!is_err, "{page}");
        let seqs = seqs_in(&page);
        assert!(
            !seqs.is_empty(),
            "page from {from} rendered nothing:\n{page}"
        );
        assert_eq!(seqs[0], *seqs.iter().min().unwrap());
        visited.extend(seqs);
        match next_seq_hint(&page) {
            Some(next) => {
                assert!(next > from, "next_seq must advance: {from} → {next}");
                from = next;
            }
            None => {
                assert!(page.contains("End of transcript."), "{page}");
                break;
            }
        }
    }
    assert_eq!(
        visited, expected_seqs,
        "paging must cover every non-Meta message exactly once"
    );

    let (tools_page, _) = c.call(
        "wake_get_session",
        json!({ "key": CLAUDE_KEY, "include_tools": true }),
    );
    assert!(
        tools_page.contains("output:"),
        "include_tools should add tool output:\n{tools_page}"
    );
    let (plain_page, _) = c.call("wake_get_session", json!({ "key": CLAUDE_KEY }));
    assert!(!plain_page.contains("output:"));
    assert!(
        plain_page.contains("🔧"),
        "tool calls fold to one line by default"
    );

    // 5. 项目匹配:cwd 在项目子目录里、没匹配上、项目清单
    let (listed, is_err) = c.call(
        "wake_list_sessions",
        json!({ "project": format!("{CLAUDE_PROJECT}/src/deep"), "agents": ["claude"] }),
    );
    assert!(!is_err);
    assert!(listed.contains(CLAUDE_KEY), "{listed}");
    let (nowhere, is_err) = c.call(
        "wake_list_sessions",
        json!({ "project": "/nowhere/at/all" }),
    );
    assert!(!is_err, "没匹配上项目不是错误");
    assert!(nowhere.contains("No indexed project matches"), "{nowhere}");
    assert!(
        nowhere.contains(CLAUDE_PROJECT),
        "should list known projects:\n{nowhere}"
    );
    let (projects, _) = c.call("wake_list_projects", json!({}));
    assert!(projects.contains(CLAUDE_PROJECT));
    // 用不可能到达的绝对日期,不用 "1m":fixture 的 updated_at 有的取自文件 mtime,
    // Linux 的 fs::copy 不保留 mtime(macOS 走 APFS clone 会保留),staged 的
    // fixture 在 Linux 上就是"刚刚"——2026-09-08 CI 只红 ubuntu 那路
    let (since_none, _) = c.call("wake_list_sessions", json!({ "since": "2999-01-01" }));
    assert!(since_none.starts_with("No sessions"), "{since_none}");

    // 6. 坏 key 是给 LLM 看的失败结果,不是协议错误、更不是崩溃
    let (missing, is_err) = c.call("wake_get_session", json!({ "key": "claude-code:nope" }));
    assert!(is_err);
    assert!(missing.contains("No session with key"), "{missing}");

    c.finish();

    // 7. 全程只读
    let after = std::fs::read(&db).unwrap();
    assert!(before == after, "wake-mcp must never write the index");
}

#[test]
fn missing_index_exits_with_a_clear_error() {
    let tmp = tempfile::tempdir().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_wake-mcp"))
        .arg("--db")
        .arg(tmp.path().join("nope.db"))
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("launch Wake once"), "{stderr}");
}
