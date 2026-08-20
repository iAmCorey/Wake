//! 十二家 adapter 的解析契约测试:全部走公开 API(`AgentAdapter` trait),
//! fixture 为全合成数据(tests/fixtures/,SQLite 型在临时 HOME 里现建)。
//!
//! Copilot/OpenCode/Antigravity 的 parse 只认各自 HOME 下的库,Gemini 的 cwd
//! 反查读 `~/.gemini/projects.json`,Kimi 的 cwd 反查读
//! `~/.kimi-code/session_index.jsonl`,因此测试统一把 HOME 指到临时假家目录
//! (OnceLock 保证 set_var 先于一切 adapter 构造,且只发生一次)。文件型
//! agent 的 SessionFileRef 直接指向 fixture 路径,不依赖 HOME。

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use wake_core::adapters::antigravity::AntigravityAdapter;
use wake_core::adapters::claude::ClaudeAdapter;
use wake_core::adapters::codex::CodexAdapter;
use wake_core::adapters::copilot::CopilotAdapter;
use wake_core::adapters::cursor::CursorAdapter;
use wake_core::adapters::gemini::GeminiAdapter;
use wake_core::adapters::grok::GrokAdapter;
use wake_core::adapters::kimi::KimiAdapter;
use wake_core::adapters::kiro::KiroAdapter;
use wake_core::adapters::opencode::OpencodeAdapter;
use wake_core::adapters::pi::PiAdapter;
use wake_core::adapters::AgentAdapter;
use wake_core::models::*;

// ---------------------------------------------------------------- 测试环境

struct TestEnv {
    copilot_db: PathBuf,
    opencode_db: PathBuf,
    antigravity_db: PathBuf,
    /// 假 HOME 目录本体,持有 TempDir 保证整个测试进程期间不被清理
    _home: tempfile::TempDir,
}

static ENV: OnceLock<TestEnv> = OnceLock::new();

/// 所有测试的第一步:建假 HOME(含两只 SQLite fixture 库与 gemini 的
/// projects.json)并把 HOME 指过去。必须先于任何 Adapter::new()。
fn setup() -> &'static TestEnv {
    ENV.get_or_init(|| {
        let home = tempfile::Builder::new()
            .prefix("wake-adapter-contracts-")
            .tempdir()
            .expect("create fake home");

        let copilot_dir = home.path().join(".copilot");
        fs::create_dir_all(&copilot_dir).expect("mkdir .copilot");
        let copilot_db = copilot_dir.join("session-store.db");
        build_copilot_db(&copilot_db);

        let oc_dir = home.path().join(".local").join("share").join("opencode");
        fs::create_dir_all(&oc_dir).expect("mkdir opencode dir");
        let opencode_db = oc_dir.join("opencode.db");
        build_opencode_db(&opencode_db);

        let gem_dir = home.path().join(".gemini");
        fs::create_dir_all(&gem_dir).expect("mkdir .gemini");
        fs::write(
            gem_dir.join("projects.json"),
            r#"{"projects":{"/Users/tester/Github/wakefx":"wakefx-gem"}}"#,
        )
        .expect("write projects.json");

        let ag_dir = gem_dir.join("antigravity-cli");
        fs::create_dir_all(&ag_dir).expect("mkdir antigravity-cli");
        let antigravity_db = ag_dir.join("conversation_summaries.db");
        build_antigravity_db(&antigravity_db);

        let kimi_dir = home.path().join(".kimi-code");
        fs::create_dir_all(&kimi_dir).expect("mkdir .kimi-code");
        fs::write(
            kimi_dir.join("session_index.jsonl"),
            concat!(
                r#"{"sessionId":"session_88888888-aaaa-bbbb-cccc-000000000008","sessionDir":"/x","workDir":"/Users/tester/Github/wakefx"}"#,
                "\n",
                r#"{"sessionId":"session_99999999-aaaa-bbbb-cccc-000000000009","sessionDir":"/x","workDir":"/Users/tester/Github/wakefx"}"#,
                "\n",
            ),
        )
        .expect("write kimi session_index");

        std::env::set_var("WAKE_HOME", home.path());
        TestEnv {
            copilot_db,
            opencode_db,
            antigravity_db,
            _home: home,
        }
    })
}

/// Copilot `session-store.db` 最小同构库:sessions + turns。
/// cop-0002 的 summary 为空,验证标题回退首条用户消息。
fn build_copilot_db(path: &Path) {
    let conn = rusqlite::Connection::open(path).expect("create copilot fixture db");
    conn.execute_batch(
        r#"
        CREATE TABLE sessions (
            id TEXT PRIMARY KEY, cwd TEXT, branch TEXT, summary TEXT,
            created_at TEXT, updated_at TEXT
        );
        CREATE TABLE turns (
            id INTEGER PRIMARY KEY, session_id TEXT, turn_index INTEGER,
            user_message TEXT, assistant_response TEXT, timestamp TEXT
        );
        INSERT INTO sessions VALUES
            ('cop-0001','/Users/tester/Github/wakefx','main','Copilot QR fix','2026-08-05 09:00:00','2026-08-05 09:30:00'),
            ('cop-0002','/Users/tester/Github/wakefx','','','2026-08-05 10:00:00','2026-08-05 10:05:00');
        INSERT INTO turns VALUES
            (1,'cop-0001',0,'Copilot 看看二维码扫描为何闪退,是不是 useEffect() 的问题','是依赖数组问题,我给出了修复补丁。','2026-08-05 09:05:00'),
            (2,'cop-0001',1,'谢谢,合并了',NULL,'2026-08-05 09:30:00'),
            (3,'cop-0002',0,'空 summary 会话的兜底标题应取这句','好的。','2026-08-05 10:05:00');
        "#,
    )
    .expect("populate copilot fixture db");
}

/// OpenCode `opencode.db` 最小同构库,v1 与 v2 两代表并存(v2 迁移后形态):
/// v1:session + message + part,msg-a 只有 synthetic part(应归 Meta),
/// msg-c 带 reasoning/tool/unknown part;oc-0001 只存在于 v1 表(模拟迁移后
/// 又用 v1 CLI 跑的会话),必须被 UNION 回捞。
/// v2:session_v2 + session_message,ocv2-0001 是 opencode2 beta 会话。
fn build_opencode_db(path: &Path) {
    let conn = rusqlite::Connection::open(path).expect("create opencode fixture db");
    conn.execute_batch(
        r#"
        CREATE TABLE session (
            id TEXT PRIMARY KEY, parent_id TEXT, directory TEXT, title TEXT,
            time_created INTEGER, time_updated INTEGER, model TEXT,
            tokens_input INTEGER, tokens_output INTEGER, tokens_reasoning INTEGER,
            time_archived INTEGER, version TEXT
        );
        CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT, data TEXT, time_created INTEGER);
        CREATE TABLE part (id TEXT PRIMARY KEY, session_id TEXT, message_id TEXT, data TEXT);
        INSERT INTO session VALUES
            ('oc-0001', NULL, '/Users/tester/Github/wakefx', 'OpenCode 二维码排查',
             1786000000000, 1786000600000, '{"providerID":"anthropic","id":"claude-sonnet-4-5"}',
             100, 50, 25, NULL, '1.14.50');
        INSERT INTO message VALUES
            ('msg-a','oc-0001','{"id":"msg-a","role":"user","time":{"created":1786000050000}}',1786000050000),
            ('msg-b','oc-0001','{"id":"msg-b","role":"user","time":{"created":1786000100000}}',1786000100000),
            ('msg-c','oc-0001','{"id":"msg-c","role":"assistant","time":{"created":1786000200000}}',1786000200000);
        INSERT INTO part VALUES
            ('prt-a-01','oc-0001','msg-a','{"type":"text","text":"editor context: QrScanner.tsx is open","synthetic":true}'),
            ('prt-b-01','oc-0001','msg-b','{"type":"text","text":"OpenCode 查一下二维码组件的 useEffect() 泄漏"}'),
            ('prt-c-01','oc-0001','msg-c','{"type":"step-start"}'),
            ('prt-c-02','oc-0001','msg-c','{"type":"reasoning","text":"先查 effect 依赖和清理函数"}'),
            ('prt-c-03','oc-0001','msg-c','{"type":"tool","callID":"oc_call_1","tool":"grep","state":{"status":"completed","input":{"pattern":"useEffect"},"output":"src/QrScanner.tsx: useEffect(() => watch())"}}'),
            ('prt-c-04','oc-0001','msg-c','{"type":"text","text":"找到泄漏点,已在清理回调里停止扫描。"}'),
            ('prt-c-05','oc-0001','msg-c','{"type":"wibble-part"}');
        CREATE TABLE session_v2 (
            id TEXT PRIMARY KEY, parent_id TEXT, directory TEXT, title TEXT,
            time_created INTEGER, time_updated INTEGER, model TEXT,
            tokens_input INTEGER, tokens_output INTEGER, tokens_reasoning INTEGER,
            time_archived INTEGER, version TEXT
        );
        CREATE TABLE session_message (
            id TEXT PRIMARY KEY, session_id TEXT, type TEXT, seq INTEGER,
            time_created INTEGER, time_updated INTEGER, data TEXT
        );
        INSERT INTO session_v2 VALUES
            ('ocv2-0001', NULL, '/Users/tester/Github/wakefx', 'OpenCode v2 greeting',
             1786100000000, 1786100300000, '{"id":"nemotron-3.5-lightning-free","providerID":"opencode"}',
             10, 5, 2, NULL, '0.0.0-beta-17639');
        INSERT INTO session_message VALUES
            ('m2-0','ocv2-0001','user',0,1786100000000,1786100000000,
             '{"text":"OpenCode v2 看看二维码组件","time":{"created":1786100000000},"files":[],"agents":[]}'),
            ('m2-1','ocv2-0001','synthetic',1,1786100001000,1786100001000,
             '{"text":"<system-reminder>Note: the user opened QrScanner.tsx</system-reminder>","time":{"created":1786100001000}}'),
            ('m2-2','ocv2-0001','assistant',2,1786100002000,1786100002000,
             '{"agent":"build","model":{"id":"nemotron-3.5-lightning-free","providerID":"opencode","variant":"default"},"time":{"created":1786100002000},"content":[{"type":"reasoning","text":"用户要看扫描组件"},{"type":"text","text":"看完了,组件没有泄漏。"},{"type":"wibble-block"}]}'),
            ('m2-3','ocv2-0001','wibble-row',3,1786100003000,1786100003000,'{}');
        "#,
    )
    .expect("populate opencode fixture db");
}

/// Antigravity `conversation_summaries.db` 最小同构库:标题在 preview 列
/// (title 列常空);ag-0002 是子会话(parent 非空),必须被过滤。
fn build_antigravity_db(path: &Path) {
    let conn = rusqlite::Connection::open(path).expect("create antigravity fixture db");
    conn.execute_batch(
        r#"
        CREATE TABLE conversation_summaries (
            conversation_id text, title text NOT NULL DEFAULT "",
            preview text NOT NULL DEFAULT "", step_count integer NOT NULL DEFAULT 0,
            last_modified_time datetime NOT NULL, workspace_uris text NOT NULL,
            parent_conversation_id text NOT NULL DEFAULT "",
            nesting_depth integer NOT NULL DEFAULT 0,
            last_user_input_time datetime NOT NULL,
            PRIMARY KEY (conversation_id)
        );
        INSERT INTO conversation_summaries
            (conversation_id, title, preview, step_count, last_modified_time,
             workspace_uris, parent_conversation_id, nesting_depth, last_user_input_time)
        VALUES
            ('ag-0001', '', 'QR overlay polish', 12, '2026-08-06 13:00:00.000000+00:00',
             '["file:///Users/tester/Github/wakefx"]', '', 0, '2026-08-06 13:00:00.000000+00:00'),
            ('ag-0002', '', 'Child convo', 3, '2026-08-06 13:05:00.000000+00:00',
             '["file:///Users/tester/Github/wakefx"]', 'ag-0001', 1, '0001-01-01 00:00:00+00:00');
        "#,
    )
    .expect("populate antigravity fixture db");
}

// ---------------------------------------------------------------- 小工具

fn fixture(rel: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("fixtures").join(rel)
}

/// 文件型 agent 的 SessionFileRef(直接指 fixture,不走 list_session_files)
fn fs_ref(agent: AgentId, path: &Path, native_id: &str) -> SessionFileRef {
    let meta = fs::metadata(path)
        .unwrap_or_else(|e| panic!("fixture missing {}: {e}", path.display()));
    SessionFileRef {
        agent,
        native_id: native_id.to_string(),
        file_path: path.to_string_lossy().to_string(),
        mtime_ms: meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0),
        size: meta.len() as i64,
    }
}

/// SQLite 型 agent 的虚拟路径引用(`<db>#<id>`,与 sqlite_ro::virtual_path 同构)
fn db_ref(agent: AgentId, db: &Path, id: &str) -> SessionFileRef {
    SessionFileRef {
        agent,
        native_id: id.to_string(),
        file_path: format!("{}#{id}", db.display()),
        mtime_ms: 1,
        size: 1,
    }
}

fn ms(rfc3339: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(rfc3339).expect("test timestamp").timestamp_millis()
}

fn roles_kinds(mainline: &[TranscriptMessage]) -> Vec<(Role, MessageKind)> {
    mainline.iter().map(|m| (m.role, m.kind)).collect()
}

// 各 fixture 的固定引用
fn claude_ref() -> SessionFileRef {
    fs_ref(
        AgentId::ClaudeCode,
        &fixture("claude/projects/-Users-tester-Github-wakefx/11111111-aaaa-bbbb-cccc-000000000001.jsonl"),
        "11111111-aaaa-bbbb-cccc-000000000001",
    )
}
fn codex_ref() -> SessionFileRef {
    fs_ref(
        AgentId::Codex,
        &fixture("codex/sessions/2026/08/02/rollout-2026-08-02T09-15-00-22222222-aaaa-bbbb-cccc-000000000002.jsonl"),
        "22222222-aaaa-bbbb-cccc-000000000002",
    )
}
fn cursor_ref() -> SessionFileRef {
    fs_ref(
        AgentId::Cursor,
        &fixture("cursor/projects/wakefx-cursor-proj/agent-transcripts/33333333-aaaa-bbbb-cccc-000000000003/33333333-aaaa-bbbb-cccc-000000000003.jsonl"),
        "33333333-aaaa-bbbb-cccc-000000000003",
    )
}
fn kiro_ref() -> SessionFileRef {
    fs_ref(
        AgentId::Kiro,
        &fixture("kiro/sessions/cli/44444444-aaaa-bbbb-cccc-000000000004.jsonl"),
        "44444444-aaaa-bbbb-cccc-000000000004",
    )
}
fn gemini_ref() -> SessionFileRef {
    fs_ref(
        AgentId::Gemini,
        &fixture("gemini/tmp/wakefx-gem/chats/session-2026-08-04T12-00-00.jsonl"),
        "session-2026-08-04T12-00-00",
    )
}
/// pi 与 omp 同构,同一份 fixture 两个 agent 复用(仅 root/AgentId 不同)
fn pi_ref(agent: AgentId) -> SessionFileRef {
    fs_ref(
        agent,
        &fixture("pi/agent/sessions/--Users-tester-Github-wakefx--/2026-08-06T10-00-00-000Z_66666666-aaaa-bbbb-cccc-000000000006.jsonl"),
        "66666666-aaaa-bbbb-cccc-000000000006",
    )
}
fn grok_ref() -> SessionFileRef {
    fs_ref(
        AgentId::Grok,
        &fixture("grok/sessions/%2FUsers%2Ftester%2FGithub%2Fwakefx/77777777-aaaa-bbbb-cccc-000000000007/updates.jsonl"),
        "77777777-aaaa-bbbb-cccc-000000000007",
    )
}
fn kimi_ref() -> SessionFileRef {
    fs_ref(
        AgentId::Kimi,
        &fixture("kimi/sessions/wd_wakefx_abc123/session_88888888-aaaa-bbbb-cccc-000000000008/agents/main/wire.jsonl"),
        "session_88888888-aaaa-bbbb-cccc-000000000008",
    )
}

// ---------------------------------------------------------------- 每家解析

#[test]
fn claude_parse_contract() {
    setup();
    let adapter = ClaudeAdapter::new();
    let r = claude_ref();
    let s = adapter.parse_session(&r).expect("claude parse_session");
    let t = adapter.parse_transcript(&r).expect("claude parse_transcript");

    // 标题=最后一条 custom-title,压过首条用户消息推导
    assert_eq!(s.meta.title, "QR login revamp");
    assert_eq!(s.meta.project_path, "/Users/tester/Github/wakefx");
    assert_eq!(s.meta.project_name, "wakefx");
    assert_eq!(s.meta.git_branch.as_deref(), Some("main"));
    assert_eq!(s.meta.model.as_deref(), Some("claude-opus-4-1"));
    assert_eq!(s.meta.tokens_used, Some(160));
    assert_eq!(s.meta.message_count, 3);
    assert_eq!(s.meta.created_at, ms("2026-08-01T09:59:00Z"));
    assert_eq!(s.meta.updated_at, ms("2026-08-01T10:02:00Z"));
    // "wibble-experimental" 计 unknown;"summary" 在已知跳过表内不计
    assert_eq!(s.unknown_line_count, 1);

    assert_eq!(
        roles_kinds(&t.mainline),
        vec![
            (Role::User, MessageKind::Meta),             // isMeta caveat
            (Role::User, MessageKind::Text),             // 真实提问
            (Role::Assistant, MessageKind::Text),        // msg_01 两行合并
            (Role::Assistant, MessageKind::Text),        // msg_02
            (Role::System, MessageKind::CompactSummary), // compact_boundary
        ]
    );
    assert_eq!(t.mainline[1].timestamp, Some(ms("2026-08-01T10:00:00Z")));

    // 同 message.id 的逐块行合并成一条:text + thinking + tool_use 同在 seq 2
    let a = &t.mainline[2];
    assert_eq!(a.text, "好的,我先查看现有代码。");
    assert!(a.thinking.as_deref().unwrap_or_default().contains("二维码"));
    assert_eq!(a.tool_calls.len(), 1);
    assert_eq!(a.tool_calls[0].name, "Read");
    // tool_result 在后续 user 行,应回填到 tool_use
    assert!(a.tool_calls[0].output.as_deref().unwrap_or_default().contains("useEffect"));
    assert!(!a.tool_calls[0].is_error);

    // units 只含 Text 消息,tool 名与 input 摘要并入正文
    assert_eq!(s.units.iter().map(|u| u.seq).collect::<Vec<_>>(), vec![1, 2, 3]);
    assert!(s.units[1].text.contains("Login.tsx"));

    // 无真实用户消息的变体 → UNTITLED
    let untitled = fs_ref(
        AgentId::ClaudeCode,
        &fixture("claude/projects/-Users-tester-Github-wakefx/aaaaaaaa-0000-4000-8000-00000000000a.jsonl"),
        "aaaaaaaa-0000-4000-8000-00000000000a",
    );
    let s2 = adapter.parse_session(&untitled).expect("claude untitled parse");
    assert_eq!(s2.meta.title, UNTITLED);
    assert_eq!(s2.meta.message_count, 1);
}

#[test]
fn codex_parse_contract() {
    setup();
    let adapter = CodexAdapter::new();
    // file_ref 是公开 API:rollout-<ts>-<uuid>.jsonl 应剥出 uuid 作 native_id
    let path = fixture("codex/sessions/2026/08/02/rollout-2026-08-02T09-15-00-22222222-aaaa-bbbb-cccc-000000000002.jsonl");
    let r = adapter.file_ref(&path).expect("codex file_ref");
    assert_eq!(r.native_id, "22222222-aaaa-bbbb-cccc-000000000002");

    let s = adapter.parse_session(&r).expect("codex parse_session");
    let t = adapter.parse_transcript(&r).expect("codex parse_transcript");

    // 标题取首条真实用户消息(environment_context 注入行归 Meta 被跳过)
    assert_eq!(s.meta.title, "扫码登录报错,帮我查一下 useEffect() 依赖数组");
    assert_eq!(s.meta.key, "codex:22222222-aaaa-bbbb-cccc-000000000002");
    assert_eq!(s.meta.project_path, "/Users/tester/Github/wakefx");
    assert_eq!(s.meta.source.as_deref(), Some("CLI")); // originator codex_cli_rs
    assert_eq!(s.meta.model.as_deref(), Some("gpt-5.2-codex"));
    assert_eq!(s.meta.git_branch.as_deref(), Some("feat/qr"));
    assert_eq!(s.meta.tokens_used, Some(4321));
    assert_eq!(s.meta.message_count, 3);
    assert!(!s.meta.archived);
    assert_eq!(s.meta.created_at, ms("2026-08-02T09:15:00Z"));
    assert_eq!(s.meta.updated_at, ms("2026-08-02T09:15:21Z"));
    assert_eq!(s.unknown_line_count, 1);

    assert_eq!(
        roles_kinds(&t.mainline),
        vec![
            (Role::User, MessageKind::Meta), // <environment_context> 注入
            (Role::User, MessageKind::Text),
            (Role::Assistant, MessageKind::Text), // reasoning 宿主 + tool call
            (Role::Assistant, MessageKind::Text),
            (Role::System, MessageKind::CompactSummary),
        ]
    );

    // reasoning 只留明文 summary,encrypted_content 必须丢弃
    let host = &t.mainline[2];
    let thinking = host.thinking.as_deref().expect("reasoning summary");
    assert!(thinking.contains("先全局搜 useEffect"));
    assert!(!thinking.contains("OPAQUE-CIPHERTEXT"));
    assert_eq!(host.tool_calls.len(), 1);
    assert_eq!(host.tool_calls[0].name, "shell");
    assert!(host.tool_calls[0].output.as_deref().unwrap_or_default().contains("QrScanner"));

    // 空文本的 reasoning 宿主凭 tool call 进入 units
    assert_eq!(s.units.iter().map(|u| u.seq).collect::<Vec<_>>(), vec![1, 2, 3]);
}

#[test]
fn copilot_parse_contract() {
    let env = setup();
    let adapter = CopilotAdapter::new();
    let r = db_ref(AgentId::Copilot, &env.copilot_db, "cop-0001");
    let s = adapter.parse_session(&r).expect("copilot parse_session");
    let t = adapter.parse_transcript(&r).expect("copilot parse_transcript");

    assert_eq!(s.meta.title, "Copilot QR fix"); // summary 优先
    assert_eq!(s.meta.git_branch.as_deref(), Some("main"));
    assert_eq!(s.meta.project_name, "wakefx");
    assert_eq!(s.meta.created_at, ms("2026-08-05T09:00:00Z"));
    assert_eq!(s.meta.updated_at, ms("2026-08-05T09:30:00Z"));
    assert_eq!(s.meta.message_count, 3);
    assert!(s.meta.file_path.ends_with("#cop-0001")); // 虚拟路径原样保留
    assert_eq!(s.unknown_line_count, 0);

    // turn = user+assistant 两条;第二轮 assistant 为 NULL 只出 user
    assert_eq!(
        roles_kinds(&t.mainline),
        vec![
            (Role::User, MessageKind::Text),
            (Role::Assistant, MessageKind::Text),
            (Role::User, MessageKind::Text),
        ]
    );
    assert_eq!(t.mainline[0].timestamp, Some(ms("2026-08-05T09:05:00Z")));
    assert_eq!(s.units.iter().map(|u| u.seq).collect::<Vec<_>>(), vec![0, 1, 2]);

    // summary 为空的会话回退首条用户消息作标题
    let r2 = db_ref(AgentId::Copilot, &env.copilot_db, "cop-0002");
    let s2 = adapter.parse_session(&r2).expect("copilot fallback parse");
    assert_eq!(s2.meta.title, "空 summary 会话的兜底标题应取这句");
    assert_eq!(s2.meta.git_branch, None);
}

#[test]
fn cursor_parse_contract() {
    setup();
    let adapter = CursorAdapter::new();
    let r = cursor_ref();
    let s = adapter.parse_session(&r).expect("cursor parse_session");
    let t = adapter.parse_transcript(&r).expect("cursor parse_transcript");

    // 标题取 <user_query> 壳内正文;无壳的注入行(<workspace>)归 Meta 被跳过
    assert_eq!(s.meta.title, "把二维码扫描组件抽出来,注意 useEffect() 的清理");
    // slug 目录 "wakefx-cursor-proj" 磁盘上无对应真实路径 → 直译回退
    assert_eq!(s.meta.project_path, "/wakefx/cursor/proj");
    assert_eq!(s.meta.project_name, "proj");
    assert_eq!(s.meta.created_at, ms("2026-08-01T09:30:00+08:00"));
    assert_eq!(s.meta.updated_at, ms("2026-08-01T09:40:00+08:00"));
    assert_eq!(s.meta.message_count, 3);
    assert_eq!(s.unknown_line_count, 1); // session_started;turn_ended 不计

    assert_eq!(
        roles_kinds(&t.mainline),
        vec![
            (Role::User, MessageKind::Meta), // <workspace> 注入
            (Role::User, MessageKind::Text),
            (Role::Assistant, MessageKind::Text), // 连续 assistant 行合并
            (Role::User, MessageKind::Text),
        ]
    );
    assert_eq!(t.mainline[1].timestamp, Some(ms("2026-08-01T09:30:00+08:00")));
    let a = &t.mainline[2];
    assert_eq!(a.tool_calls.len(), 1);
    assert_eq!(a.tool_calls[0].name, "read_file");
    assert_eq!(a.tool_calls[0].output, None); // transcript 不落盘工具结果
    assert_eq!(s.units.iter().map(|u| u.seq).collect::<Vec<_>>(), vec![1, 2, 3]);
}

#[test]
fn opencode_parse_contract() {
    let env = setup();
    let adapter = OpencodeAdapter::new();
    let r = db_ref(AgentId::Opencode, &env.opencode_db, "oc-0001");
    let s = adapter.parse_session(&r).expect("opencode parse_session");
    let t = adapter.parse_transcript(&r).expect("opencode parse_transcript");

    assert_eq!(s.meta.title, "OpenCode 二维码排查"); // session 表自带标题
    assert_eq!(s.meta.project_name, "wakefx");
    assert_eq!(s.meta.model.as_deref(), Some("claude-sonnet-4-5")); // model json 的 id
    assert_eq!(s.meta.tokens_used, Some(175)); // input+output+reasoning
    assert!(!s.meta.archived); // time_archived NULL
    assert_eq!(s.meta.created_at, 1786000000000);
    assert_eq!(s.meta.updated_at, 1786000600000);
    assert_eq!(s.meta.message_count, 2);
    assert_eq!(s.unknown_line_count, 1); // wibble-part

    assert_eq!(
        roles_kinds(&t.mainline),
        vec![
            (Role::User, MessageKind::Meta), // 只有 synthetic part
            (Role::User, MessageKind::Text),
            (Role::Assistant, MessageKind::Text),
        ]
    );
    let a = &t.mainline[2];
    assert!(a.thinking.as_deref().unwrap_or_default().contains("先查 effect 依赖"));
    assert_eq!(a.tool_calls.len(), 1);
    assert_eq!(a.tool_calls[0].name, "grep");
    assert!(a.tool_calls[0].output.as_deref().unwrap_or_default().contains("QrScanner"));
    assert!(!a.tool_calls[0].is_error);
    assert_eq!(s.units.iter().map(|u| u.seq).collect::<Vec<_>>(), vec![1, 2]);
}

#[test]
fn kiro_parse_contract() {
    setup();
    let adapter = KiroAdapter::new();
    let r = kiro_ref();
    let s = adapter.parse_session(&r).expect("kiro parse_session");
    let t = adapter.parse_transcript(&r).expect("kiro parse_transcript");

    // .json 边车给标题、cwd 与模型(session_state.rts_model_state.model_info)
    assert_eq!(s.meta.title, "Kiro QR session");
    assert_eq!(s.meta.project_path, "/Users/tester/Github/wakefx");
    assert_eq!(s.meta.model.as_deref(), Some("claude-sonnet-4"));
    assert_eq!(s.meta.created_at, ms("2026-08-03T08:00:00Z"));
    assert_eq!(s.meta.updated_at, ms("2026-08-03T08:30:00Z")); // 边车晚于消息时间
    assert_eq!(s.meta.message_count, 2);
    assert_eq!(s.unknown_line_count, 1); // ToolLog

    assert_eq!(
        roles_kinds(&t.mainline),
        vec![(Role::User, MessageKind::Text), (Role::Assistant, MessageKind::Text)]
    );
    // jsonl 的 timestamp 是 unix 秒,应换算成 ms
    assert_eq!(t.mainline[0].timestamp, Some(1785744300000));
    assert_eq!(t.mainline[1].timestamp, Some(1785744360000));
    assert_eq!(s.units.iter().map(|u| u.seq).collect::<Vec<_>>(), vec![0, 1]);
}

#[test]
fn gemini_parse_contract() {
    setup();
    let adapter = GeminiAdapter::new();
    let r = gemini_ref();
    let s = adapter.parse_session(&r).expect("gemini parse_session");
    let t = adapter.parse_transcript(&r).expect("gemini parse_transcript");

    // $set 是覆盖式快照:只认最后一条,旧快照文本不得出现
    assert_eq!(t.mainline.len(), 2);
    assert!(t.mainline.iter().all(|m| !m.text.contains("旧快照")));
    assert_eq!(
        roles_kinds(&t.mainline),
        vec![(Role::User, MessageKind::Text), (Role::Assistant, MessageKind::Text)]
    );

    // id 取 header 的 sessionId(非文件名 stem)
    assert_eq!(s.meta.id, "55555555-aaaa-bbbb-cccc-000000000005");
    assert_eq!(s.meta.key, "gemini:55555555-aaaa-bbbb-cccc-000000000005");
    assert_eq!(s.meta.title, "Gemini 帮我调试二维码解码,顺带看 useEffect()");
    // cwd 经假 HOME 的 projects.json 路径→slug 反查
    assert_eq!(s.meta.project_path, "/Users/tester/Github/wakefx");
    assert_eq!(s.meta.created_at, ms("2026-08-04T12:00:00Z"));
    assert_eq!(s.meta.updated_at, ms("2026-08-04T12:20:00Z"));
    assert_eq!(s.meta.message_count, 2);
    assert_eq!(s.unknown_line_count, 1);
    assert_eq!(s.units.iter().map(|u| u.seq).collect::<Vec<_>>(), vec![0, 1]);
}

#[test]
fn opencode_v2_parse_contract() {
    let env = setup();
    let adapter = OpencodeAdapter::new();

    // 两代表 UNION:v2 会话 + 仅存于 v1 表的会话都在列表里,不重不漏
    let ids: HashSet<String> = adapter
        .list_session_files()
        .expect("opencode list")
        .into_iter()
        .map(|r| r.native_id)
        .collect();
    assert!(ids.contains("ocv2-0001"), "v2 会话应在列表");
    assert!(ids.contains("oc-0001"), "仅存于 v1 表的会话应被 UNION 回捞");

    let r = db_ref(AgentId::Opencode, &env.opencode_db, "ocv2-0001");
    let s = adapter.parse_session(&r).expect("opencode v2 parse_session");
    let t = adapter.parse_transcript(&r).expect("opencode v2 parse_transcript");

    assert_eq!(s.meta.title, "OpenCode v2 greeting");
    assert_eq!(s.meta.model.as_deref(), Some("nemotron-3.5-lightning-free"));
    assert_eq!(s.meta.source.as_deref(), Some("opencode2")); // beta 版本号 → 徽章/resume 换 bin
    assert_eq!(s.meta.tokens_used, Some(17));
    assert_eq!(s.meta.created_at, 1786100000000);
    // wibble-row(未知消息 type)+ wibble-block(未知内容块)各计一次
    assert_eq!(s.unknown_line_count, 2);

    // user 的 text 在 data 顶层;synthetic 行归 Meta;assistant 的 content 块数组
    assert_eq!(
        roles_kinds(&t.mainline),
        vec![
            (Role::User, MessageKind::Text),
            (Role::User, MessageKind::Meta),
            (Role::Assistant, MessageKind::Text),
        ]
    );
    assert_eq!(t.mainline[0].text, "OpenCode v2 看看二维码组件");
    let a = &t.mainline[2];
    assert_eq!(a.text, "看完了,组件没有泄漏。");
    assert!(a.thinking.as_deref().unwrap_or_default().contains("扫描组件"));
    assert_eq!(a.model.as_deref(), Some("nemotron-3.5-lightning-free"));
    assert_eq!(s.units.iter().map(|u| u.seq).collect::<Vec<_>>(), vec![0, 2]);

    // 仅存于 v1 表的会话不带 v2 标记(resume 走 v1 二进制)
    let s1 = adapter
        .parse_session(&db_ref(AgentId::Opencode, &env.opencode_db, "oc-0001"))
        .expect("opencode v1 parse_session");
    assert_eq!(s1.meta.source, None);
}

#[test]
fn pi_parse_contract() {
    setup();
    let adapter = PiAdapter::new();
    // file_ref 是公开 API:<timestamp>_<uuid>.jsonl 应剥出 uuid 作 native_id
    let path = fixture("pi/agent/sessions/--Users-tester-Github-wakefx--/2026-08-06T10-00-00-000Z_66666666-aaaa-bbbb-cccc-000000000006.jsonl");
    let r = adapter.file_ref(&path).expect("pi file_ref");
    assert_eq!(r.native_id, "66666666-aaaa-bbbb-cccc-000000000006");

    let s = adapter.parse_session(&r).expect("pi parse_session");
    let t = adapter.parse_transcript(&r).expect("pi parse_transcript");

    assert_eq!(s.meta.title, "Pi 查一下二维码组件的 useEffect() 清理");
    assert_eq!(s.meta.key, "pi:66666666-aaaa-bbbb-cccc-000000000006");
    // cwd 来自 session 首行,不反推有损编码目录名
    assert_eq!(s.meta.project_path, "/Users/tester/Github/wakefx");
    assert_eq!(s.meta.model.as_deref(), Some("gpt-5.5"));
    assert_eq!(s.meta.tokens_used, Some(4300)); // 最后一条 assistant 的 totalTokens
    assert_eq!(s.meta.message_count, 2);
    assert_eq!(s.meta.created_at, ms("2026-08-06T10:00:00Z"));
    assert_eq!(s.meta.updated_at, ms("2026-08-06T10:00:12Z"));
    // wibble-line 计 unknown;model_change/thinking_level_change 不计
    assert_eq!(s.unknown_line_count, 1);

    // 连续 assistant 行(中间只隔 toolResult)合并成一条
    assert_eq!(
        roles_kinds(&t.mainline),
        vec![(Role::User, MessageKind::Text), (Role::Assistant, MessageKind::Text)]
    );
    let a = &t.mainline[1];
    assert_eq!(a.text, "找到泄漏点,已补清理回调。");
    assert_eq!(a.tool_calls.len(), 1);
    assert_eq!(a.tool_calls[0].name, "bash");
    // toolResult 是独立 role 行,按 toolCallId 回填
    assert!(a.tool_calls[0].output.as_deref().unwrap_or_default().contains("QrScanner"));
    assert!(!a.tool_calls[0].is_error);
    assert_eq!(s.units.iter().map(|u| u.seq).collect::<Vec<_>>(), vec![0, 1]);

    // omp 是 pi 的 fork,同一解析核心,只有 key 前缀不同
    let omp = PiAdapter::omp();
    let s2 = omp.parse_session(&pi_ref(AgentId::Omp)).expect("omp parse_session");
    assert_eq!(s2.meta.agent, AgentId::Omp);
    assert_eq!(s2.meta.key, "omp:66666666-aaaa-bbbb-cccc-000000000006");
    assert_eq!(s2.meta.title, "Pi 查一下二维码组件的 useEffect() 清理");
}

#[test]
fn grok_parse_contract() {
    setup();
    let adapter = GrokAdapter::new();
    // file_ref 是公开 API:只认 updates.jsonl,native_id 取会话目录名
    let path = fixture("grok/sessions/%2FUsers%2Ftester%2FGithub%2Fwakefx/77777777-aaaa-bbbb-cccc-000000000007/updates.jsonl");
    let r = adapter.file_ref(&path).expect("grok file_ref");
    assert_eq!(r.native_id, "77777777-aaaa-bbbb-cccc-000000000007");
    assert!(adapter.file_ref(&path.with_file_name("chat_history.jsonl")).is_none());

    let s = adapter.parse_session(&r).expect("grok parse_session");
    let t = adapter.parse_transcript(&r).expect("grok parse_transcript");

    assert_eq!(s.meta.title, "Grok QR scan cleanup"); // summary.json 标题优先
    assert_eq!(s.meta.project_path, "/Users/tester/Github/wakefx"); // info.cwd
    assert_eq!(s.meta.git_branch.as_deref(), Some("feat/qr"));
    assert_eq!(s.meta.model.as_deref(), Some("grok-composer-2.5-fast"));
    assert_eq!(s.meta.created_at, ms("2026-08-06T11:00:00Z"));
    assert_eq!(s.meta.updated_at, ms("2026-08-06T11:20:00Z"));
    assert_eq!(s.meta.message_count, 2);
    assert_eq!(s.unknown_line_count, 1); // wibble_update;auto_compact_started 不计

    // chunk 流按角色段合并:两条 user chunk 拼成一条,thought/message/tool 全并入一条 assistant
    assert_eq!(
        roles_kinds(&t.mainline),
        vec![(Role::User, MessageKind::Text), (Role::Assistant, MessageKind::Text)]
    );
    assert_eq!(t.mainline[0].text, "Grok 看看二维码扫描,重点 useEffect() 清理");
    assert_eq!(t.mainline[0].timestamp, Some(1786014300000));
    let a = &t.mainline[1];
    assert_eq!(a.text, "已定位泄漏,补了清理回调。");
    assert!(a.thinking.as_deref().unwrap_or_default().contains("effect 泄漏"));
    assert_eq!(a.tool_calls.len(), 1);
    assert_eq!(a.tool_calls[0].name, "Grep");
    // tool_call_update 的 content 文本回填 output;字节数组 rawOutput 不碰
    assert!(a.tool_calls[0].output.as_deref().unwrap_or_default().contains("found 2 matches"));
    assert!(!a.tool_calls[0].is_error);
    assert_eq!(s.units.iter().map(|u| u.seq).collect::<Vec<_>>(), vec![0, 1]);
}

#[test]
fn kimi_parse_contract() {
    setup();
    let adapter = KimiAdapter::new();
    // file_ref 是公开 API:只认 agents/main/wire.jsonl,native_id 取会话目录名
    let path = fixture("kimi/sessions/wd_wakefx_abc123/session_88888888-aaaa-bbbb-cccc-000000000008/agents/main/wire.jsonl");
    let r = adapter.file_ref(&path).expect("kimi file_ref");
    assert_eq!(r.native_id, "session_88888888-aaaa-bbbb-cccc-000000000008");

    let s = adapter.parse_session(&r).expect("kimi parse_session");
    let t = adapter.parse_transcript(&r).expect("kimi parse_transcript");

    assert_eq!(s.meta.title, "Kimi QR fix"); // state.json 标题优先
    // cwd 靠假 HOME 的 session_index.jsonl 反查(目录名 hash 不可反推)
    assert_eq!(s.meta.project_path, "/Users/tester/Github/wakefx");
    assert_eq!(s.meta.created_at, ms("2026-08-06T12:00:00Z"));
    assert_eq!(s.meta.updated_at, ms("2026-08-06T12:30:00Z"));
    assert_eq!(s.meta.message_count, 2);
    // wibble.record 计 unknown;metadata/config/tools/turn.*/append_loop_event 不计
    assert_eq!(s.unknown_line_count, 1);

    assert_eq!(
        roles_kinds(&t.mainline),
        vec![(Role::User, MessageKind::Text), (Role::Assistant, MessageKind::Text)]
    );
    assert_eq!(t.mainline[0].text, "Kimi 修一下二维码组件的 useEffect() 内存泄漏");
    assert!(t.mainline[1].text.contains("QrScanner"));
    assert_eq!(s.units.iter().map(|u| u.seq).collect::<Vec<_>>(), vec![0, 1]);

    // "New Session" 是占位标题,必须回退首条用户消息
    let placeholder = fs_ref(
        AgentId::Kimi,
        &fixture("kimi/sessions/wd_wakefx_abc123/session_99999999-aaaa-bbbb-cccc-000000000009/agents/main/wire.jsonl"),
        "session_99999999-aaaa-bbbb-cccc-000000000009",
    );
    let s2 = adapter.parse_session(&placeholder).expect("kimi placeholder parse");
    assert_eq!(s2.meta.title, "占位标题会话应回退到这句");
}

#[test]
fn antigravity_parse_contract() {
    let env = setup();
    let adapter = AntigravityAdapter::new();

    // 子会话(parent_conversation_id 非空)不进列表
    let refs = adapter.list_session_files().expect("antigravity list");
    assert_eq!(refs.len(), 1);
    assert_eq!(refs[0].native_id, "ag-0001");

    let r = db_ref(AgentId::Antigravity, &env.antigravity_db, "ag-0001");
    let s = adapter.parse_session(&r).expect("antigravity parse_session");
    let t = adapter.parse_transcript(&r).expect("antigravity parse_transcript");

    assert_eq!(s.meta.title, "QR overlay polish"); // 标题在 preview 列
    assert_eq!(s.meta.project_path, "/Users/tester/Github/wakefx"); // file:// URI 解码
    assert_eq!(s.meta.project_name, "wakefx");
    // 带 +00:00 偏移的 datetime 必须解析成功(不能落到 0/mtime 兜底)
    assert_eq!(s.meta.created_at, ms("2026-08-06T13:00:00Z"));
    assert_eq!(s.meta.message_count, 12); // step_count
    assert!(s.meta.file_path.ends_with("#ag-0001")); // 虚拟路径

    // 正文加密:唯一一条 System 消息承载 preview 与说明,FTS 搜得到 preview
    assert_eq!(roles_kinds(&t.mainline), vec![(Role::System, MessageKind::Text)]);
    assert!(t.mainline[0].text.contains("QR overlay polish"));
    assert!(t.mainline[0].text.contains("encrypted"));
    assert_eq!(s.units.len(), 1);
    assert!(s.units[0].text.contains("QR overlay polish"));
}

// ---------------------------------------------------------------- seq 契约

/// 跨文件不变量 1:FTS 单元的 seq 必须能在详情页 mainline 中找到同号消息,
/// 且 mainline seq 从 0 严格递增(搜索跳转按 seq 定位依赖此契约)。
fn assert_seq_contract(adapter: &dyn AgentAdapter, r: &SessionFileRef) {
    let tag = r.agent.as_str();
    let s = adapter.parse_session(r).unwrap_or_else(|e| panic!("[{tag}] parse_session: {e}"));
    let t = adapter.parse_transcript(r).unwrap_or_else(|e| panic!("[{tag}] parse_transcript: {e}"));

    assert!(!t.mainline.is_empty(), "[{tag}] mainline 不应为空");
    for (i, m) in t.mainline.iter().enumerate() {
        assert_eq!(m.seq, i as i64, "[{tag}] mainline seq 必须从 0 严格递增");
    }

    assert!(!s.units.is_empty(), "[{tag}] fixture 应产出至少一个 FTS 单元");
    let seqs: HashSet<i64> = t.mainline.iter().map(|m| m.seq).collect();
    for u in &s.units {
        assert!(seqs.contains(&u.seq), "[{tag}] unit seq {} 在 mainline 中不存在", u.seq);
        let m = &t.mainline[u.seq as usize];
        assert_eq!(m.role, u.role, "[{tag}] seq {} 两侧角色不一致", u.seq);
    }

    // 两条解析路径共用核心解析器,meta 关键字段必须一致
    assert_eq!(s.meta.key, t.meta.key, "[{tag}] key 两侧不一致");
    assert_eq!(s.meta.title, t.meta.title, "[{tag}] title 两侧不一致");
    assert_eq!(s.meta.message_count, t.meta.message_count, "[{tag}] message_count 两侧不一致");
}

#[test]
fn seq_contract_holds_for_all_agents() {
    let env = setup();
    let checks: Vec<(Box<dyn AgentAdapter>, SessionFileRef)> = vec![
        (Box::new(ClaudeAdapter::new()), claude_ref()),
        (Box::new(CodexAdapter::new()), codex_ref()),
        (Box::new(CopilotAdapter::new()), db_ref(AgentId::Copilot, &env.copilot_db, "cop-0001")),
        (Box::new(CursorAdapter::new()), cursor_ref()),
        (Box::new(OpencodeAdapter::new()), db_ref(AgentId::Opencode, &env.opencode_db, "oc-0001")),
        (Box::new(KiroAdapter::new()), kiro_ref()),
        (Box::new(GeminiAdapter::new()), gemini_ref()),
        (Box::new(PiAdapter::new()), pi_ref(AgentId::Pi)),
        (Box::new(PiAdapter::omp()), pi_ref(AgentId::Omp)),
        (Box::new(GrokAdapter::new()), grok_ref()),
        (Box::new(KimiAdapter::new()), kimi_ref()),
        (Box::new(AntigravityAdapter::new()), db_ref(AgentId::Antigravity, &env.antigravity_db, "ag-0001")),
    ];
    for (adapter, r) in &checks {
        assert_seq_contract(adapter.as_ref(), r);
    }
}

// ---------------------------------------------------------------- quickMeta 合并

fn mk_meta(agent: AgentId, key: &str, id: &str, title: &str, source: Option<&str>) -> SessionMeta {
    SessionMeta {
        key: key.to_string(),
        id: id.to_string(),
        agent,
        title: title.to_string(),
        project_path: "/Users/tester/Github/wakefx".to_string(),
        project_name: "wakefx".to_string(),
        file_path: "/dev/null".to_string(),
        created_at: 1,
        updated_at: 2,
        message_count: 0,
        size_bytes: 0,
        git_branch: None,
        model: None,
        tokens_used: None,
        archived: false,
        source: source.map(String::from),
        favorite: false,
        pinned: false,
    }
}

#[test]
fn merge_quick_meta_default_vs_codex_override() {
    setup();

    // 默认实现(以 Claude 为代表):parsed 为准,quick 只补 source/model/tokens 缺口
    let claude = ClaudeAdapter::new();
    let parsed = mk_meta(AgentId::ClaudeCode, "claude-code:p", "p", "解析出来的标题", None);
    let mut quick = mk_meta(AgentId::ClaudeCode, "claude-code:q", "q", "手动改名", Some("state"));
    quick.model = Some("model-q".to_string());
    quick.tokens_used = Some(7);
    let merged = claude.merge_quick_meta(parsed, &quick);
    assert_eq!(merged.title, "解析出来的标题"); // 标题不被 quick 覆盖
    assert_eq!(merged.key, "claude-code:p"); // key/id 也不动
    assert_eq!(merged.id, "p");
    assert_eq!(merged.source.as_deref(), Some("state")); // None 才补
    assert_eq!(merged.model.as_deref(), Some("model-q"));
    assert_eq!(merged.tokens_used, Some(7));
    // parsed 已有 source 时 quick 不覆盖
    let parsed = mk_meta(AgentId::ClaudeCode, "claude-code:p", "p", "t", Some("CLI"));
    let merged = claude.merge_quick_meta(parsed, &quick);
    assert_eq!(merged.source.as_deref(), Some("CLI"));

    // Codex 覆写:state DB 的 title 是用户手动命名,压过解析标题;key/id 以
    // state 的线程 id 为准;source 相反(rollout originator 更精确,quick 只兜底)
    let codex = CodexAdapter::new();
    let parsed = mk_meta(AgentId::Codex, "codex:file-uuid", "file-uuid", "首条消息推导标题", Some("IDE extension"));
    let mut quick = mk_meta(AgentId::Codex, "codex:thread-1", "thread-1", "用户手动命名", Some("vscode"));
    quick.model = Some("gpt-5.2".to_string());
    quick.tokens_used = Some(999);
    let merged = codex.merge_quick_meta(parsed, &quick);
    assert_eq!(merged.title, "用户手动命名");
    assert_eq!(merged.key, "codex:thread-1");
    assert_eq!(merged.id, "thread-1");
    assert_eq!(merged.source.as_deref(), Some("IDE extension")); // parsed 优先
    assert_eq!(merged.model.as_deref(), Some("gpt-5.2")); // None 才补
    assert_eq!(merged.tokens_used, Some(999));

    // UNTITLED 守卫:quick 的占位标题不得覆盖解析标题,但 key/id 仍取 state
    let parsed = mk_meta(AgentId::Codex, "codex:file-uuid", "file-uuid", "首条消息推导标题", None);
    let quick = mk_meta(AgentId::Codex, "codex:thread-1", "thread-1", UNTITLED, Some("vscode"));
    let merged = codex.merge_quick_meta(parsed, &quick);
    assert_eq!(merged.title, "首条消息推导标题");
    assert_eq!(merged.id, "thread-1");
    assert_eq!(merged.source.as_deref(), Some("vscode")); // parsed 无 source 时兜底

    // 空标题同样不覆盖
    let parsed = mk_meta(AgentId::Codex, "codex:file-uuid", "file-uuid", "首条消息推导标题", None);
    let quick = mk_meta(AgentId::Codex, "codex:thread-1", "thread-1", "", None);
    let merged = codex.merge_quick_meta(parsed, &quick);
    assert_eq!(merged.title, "首条消息推导标题");
}
