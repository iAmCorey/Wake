//! 十四家 adapter 的解析契约测试:全部走公开 API(`AgentAdapter` trait),
//! fixture 为全合成数据(tests/fixtures/,SQLite 型在临时 HOME 里现建,
//! dsh 的 zstd 日志由检入的明文 fixture 在临时 HOME 里压制)。
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
use wake_core::adapters::dsh::DshAdapter;
use wake_core::adapters::gemini::GeminiAdapter;
use wake_core::adapters::grok::GrokAdapter;
use wake_core::adapters::kimi::KimiAdapter;
use wake_core::adapters::kiro::KiroAdapter;
use wake_core::adapters::opencode::OpencodeAdapter;
use wake_core::adapters::pi::PiAdapter;
use wake_core::adapters::qoder::QoderAdapter;
use wake_core::adapters::AgentAdapter;
use wake_core::models::*;

// ---------------------------------------------------------------- 测试环境

struct TestEnv {
    copilot_db: PathBuf,
    opencode_db: PathBuf,
    opencode_next_db: PathBuf,
    antigravity_db: PathBuf,
    dsh_log: PathBuf,
    /// 假 HOME 目录本体,持有 TempDir 保证整个测试进程期间不被清理
    _home: tempfile::TempDir,
}

static ENV: OnceLock<TestEnv> = OnceLock::new();

/// 所有测试的第一步:建假 HOME(含 SQLite fixture 库与 gemini 的
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
        let opencode_next_db = oc_dir.join("opencode-next.db");
        build_opencode_next_db(&opencode_next_db);

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

        // dsh:检入的明文 fixture 压成真实写端布局的 zstd 多帧文件(首帧 header
        // 行、次帧事件批,帧直接连接),另放一个子代理会话验证 file_ref 过滤
        let dsh_project = home.path().join(".dsh").join("sessions").join("--Users-tester-Github-wakefx--");
        let dsh_sess = dsh_project.join("dsh-e2e4-0001");
        fs::create_dir_all(&dsh_sess).expect("mkdir dsh session dir");
        let plain = fs::read_to_string(fixture("dsh/session.jsonl")).expect("read dsh fixture");
        let (header, body) = plain.split_once('\n').expect("dsh fixture header line");
        let mut frames = zstd::encode_all(format!("{header}\n").as_bytes(), 3).expect("zstd header frame");
        frames.extend(zstd::encode_all(body.as_bytes(), 3).expect("zstd event frame"));
        let dsh_log = dsh_sess.join("session.jsonl.zstd");
        fs::write(&dsh_log, frames).expect("write dsh zstd log");
        let dsh_sub = dsh_project.join("dsh-sub-0002");
        fs::create_dir_all(&dsh_sub).expect("mkdir dsh subagent dir");
        fs::write(
            dsh_sub.join("session.jsonl"),
            concat!(
                r#"{"type":"session","version":0,"id":"dsh-sub-0002","createdAt":1786100000000,"cwd":"/Users/tester/Github/wakefx","origin":"subagent","delegationDepth":1}"#,
                "\n",
                r#"{"type":"user/message","seq":0,"time":1786100001000,"data":{"id":"m","role":"user","content":[{"type":"text","text":"child task"}],"source":{"kind":"user"}}}"#,
                "\n",
            ),
        )
        .expect("write dsh subagent log");

        // WAKE_HOME 是 adapter 侧的统一改道开关,三端一致;HOME 仍设一份供
        // 其他 POSIX 依赖使用(Windows 上 dirs 不看 HOME,单设它等于没设)
        std::env::set_var("WAKE_HOME", home.path());
        std::env::set_var("HOME", home.path());
        // 测试只受这个假 HOME 支配:opencode 认 XDG_DATA_HOME、codex 认
        // CODEX_HOME,开发者或 CI 机器上设了它们,adapter 就会绕过 fixture
        // 去读真实库(实测 opencode 的两个契约测试会因此挂掉)
        std::env::remove_var("XDG_DATA_HOME");
        std::env::remove_var("CODEX_HOME");
        std::env::remove_var("QODER_CONFIG_DIR");
        TestEnv {
            copilot_db,
            opencode_db,
            opencode_next_db,
            antigravity_db,
            dsh_log,
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

/// `opencode-ai@next`/binary `opencode2` 的真实同构布局(GitHub #2):数据库名
/// `opencode-next.db`,会话元数据仍在 session,新正文才在 session_message。
/// message/part 两张 v1 表仍随 migration 存在但本会话没有对应行——只检查
/// session_v2 或只对 part 求长度都会把它静默过滤。
fn build_opencode_next_db(path: &Path) {
    let conn = rusqlite::Connection::open(path).expect("create opencode next fixture db");
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
        CREATE TABLE session_message (
            id TEXT PRIMARY KEY, session_id TEXT, type TEXT, seq INTEGER,
            time_created INTEGER, time_updated INTEGER, data TEXT
        );
        INSERT INTO session VALUES
            ('ocnext-0001', NULL, '/Users/tester/Github/wakefx', 'OpenCode next real schema',
             1786200000000, 1786200300000,
             '{"id":"gpt-5.6","providerID":"openai","variant":"default"}',
             20, 8, 4, NULL, '0.0.0-next-202606270058');
        INSERT INTO session_message VALUES
            ('next-m0','ocnext-0001','user',0,1786200000000,1786200000000,
             '{"text":"OpenCode next 检查二维码组件","time":{"created":1786200000000},"files":[],"agents":[]}'),
            ('next-m1','ocnext-0001','synthetic',1,1786200001000,1786200001000,
             '{"sessionID":"ocnext-0001","text":"editor context: QrScanner.tsx","time":{"created":1786200001000}}'),
            ('next-m2','ocnext-0001','assistant',2,1786200002000,1786200002000,
             '{"agent":"build","model":{"id":"gpt-5.6","providerID":"openai","variant":"default"},"time":{"created":1786200002000},"content":[{"type":"reasoning","id":"rsn-1","text":"检查 effect 清理"},{"type":"tool","id":"tool-1","name":"grep","state":{"status":"completed","input":{"pattern":"useEffect"},"structured":{},"content":[{"type":"text","text":"src/QrScanner.tsx:42"}],"result":{"matches":1}},"time":{"created":1786200002100,"completed":1786200002200}},{"type":"text","id":"txt-1","text":"next schema 解析成功。"}]}'),
            ('next-m3','ocnext-0001','system',3,1786200003000,1786200003000,
             '{"text":"system notice","time":{"created":1786200003000}}'),
            ('next-m4','ocnext-0001','shell',4,1786200004000,1786200004000,
             '{"callID":"shell-1","command":"cargo test","output":"ok","time":{"created":1786200004000,"completed":1786200004100}}'),
            ('next-m5','ocnext-0001','compaction',5,1786200005000,1786200005000,
             '{"reason":"auto","summary":"保留二维码排查上下文","recent":"","time":{"created":1786200005000}}'),
            ('next-m6','ocnext-0001','agent-switched',6,1786200006000,1786200006000,
             '{"agent":"build","time":{"created":1786200006000}}'),
            ('next-m7','ocnext-0001','wibble-next',7,1786200007000,1786200007000,'{}');
        "#,
    )
    .expect("populate opencode next fixture db");
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
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(rel)
}

/// 文件型 agent 的 SessionFileRef(直接指 fixture,不走 list_session_files)
fn fs_ref(agent: AgentId, path: &Path, native_id: &str) -> SessionFileRef {
    let meta =
        fs::metadata(path).unwrap_or_else(|e| panic!("fixture missing {}: {e}", path.display()));
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
    chrono::DateTime::parse_from_rfc3339(rfc3339)
        .expect("test timestamp")
        .timestamp_millis()
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
fn codex_subagent_ref() -> SessionFileRef {
    fs_ref(
        AgentId::Codex,
        &fixture("codex/sessions/2026/08/07/rollout-2026-08-07T12-44-01-33333333-aaaa-bbbb-cccc-000000000003.jsonl"),
        "33333333-aaaa-bbbb-cccc-000000000003",
    )
}
fn qoder_ref() -> SessionFileRef {
    fs_ref(
        AgentId::Qoder,
        &fixture(
            "qoder/projects/-Users-tester-Github-wakefx/abababab-aaaa-bbbb-cccc-000000000014.jsonl",
        ),
        "abababab-aaaa-bbbb-cccc-000000000014",
    )
}
fn qoder_null_ref() -> SessionFileRef {
    fs_ref(
        AgentId::Qoder,
        &fixture(
            "qoder/projects/-Users-tester-Github-wakefx/cdcdcdcd-aaaa-bbbb-cccc-000000000015.jsonl",
        ),
        "cdcdcdcd-aaaa-bbbb-cccc-000000000015",
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
    let t = adapter
        .parse_transcript(&r)
        .expect("claude parse_transcript");

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
    assert!(a.tool_calls[0]
        .output
        .as_deref()
        .unwrap_or_default()
        .contains("useEffect"));
    assert!(!a.tool_calls[0].is_error);

    // units 只含 Text 消息,tool 名与 input 摘要并入正文
    assert_eq!(
        s.units.iter().map(|u| u.seq).collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
    assert!(s.units[1].text.contains("Login.tsx"));

    // 无真实用户消息的变体 → UNTITLED
    let untitled = fs_ref(
        AgentId::ClaudeCode,
        &fixture("claude/projects/-Users-tester-Github-wakefx/aaaaaaaa-0000-4000-8000-00000000000a.jsonl"),
        "aaaaaaaa-0000-4000-8000-00000000000a",
    );
    let s2 = adapter
        .parse_session(&untitled)
        .expect("claude untitled parse");
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
    let t = adapter
        .parse_transcript(&r)
        .expect("codex parse_transcript");

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
    assert!(host.tool_calls[0]
        .output
        .as_deref()
        .unwrap_or_default()
        .contains("QrScanner"));

    // 空文本的 reasoning 宿主凭 tool call 进入 units
    assert_eq!(
        s.units.iter().map(|u| u.seq).collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
}

#[test]
fn qoder_parse_contract() {
    setup();
    let adapter = QoderAdapter::new();
    let r = qoder_ref();
    let s = adapter.parse_session(&r).expect("qoder parse_session");
    let t = adapter
        .parse_transcript(&r)
        .expect("qoder parse_transcript");

    assert_eq!(s.meta.key, "qoder:abababab-aaaa-bbbb-cccc-000000000014");
    assert_eq!(s.meta.title, "Qoder active branch title");
    assert_eq!(s.meta.project_path, "/Users/tester/Github/wakefx-relocated");
    assert_eq!(s.meta.project_name, "wakefx-relocated");
    assert_eq!(s.meta.git_branch.as_deref(), Some("feature/qoder"));
    assert_eq!(s.meta.model.as_deref(), Some("qoder-performance"));
    assert_eq!(s.meta.tokens_used, Some(160));
    assert_eq!(s.meta.message_count, 4);
    assert_eq!(s.unknown_line_count, 1);
    assert_eq!(
        roles_kinds(&t.mainline),
        vec![
            (Role::User, MessageKind::Meta),
            (Role::User, MessageKind::Text),
            (Role::Assistant, MessageKind::Text),
            (Role::User, MessageKind::Text),
            (Role::Assistant, MessageKind::Text),
        ]
    );
    assert!(t
        .mainline
        .iter()
        .all(|message| !message.text.contains("废弃")));
    let tool_host = &t.mainline[2];
    assert!(tool_host.text.contains("我先定位相关 effect"));
    assert_eq!(tool_host.tool_calls.len(), 1);
    assert_eq!(tool_host.tool_calls[0].name, "Grep");
    assert!(tool_host.tool_calls[0]
        .output
        .as_deref()
        .unwrap_or_default()
        .contains("QrScanner.tsx:42"));
    assert!(tool_host
        .thinking
        .as_deref()
        .unwrap_or_default()
        .contains("清理回调"));
    assert_eq!(
        s.units.iter().map(|unit| unit.seq).collect::<Vec<_>>(),
        vec![1, 2, 3, 4]
    );

    // 默认 projects 根只枚举 project-key 直属会话，不能吸入会话边车。
    let rooted = QoderAdapter::new().with_custom_root(fixture("qoder/projects"));
    let refs = rooted.list_session_files().expect("qoder list sessions");
    assert_eq!(refs.len(), 2);
    assert!(refs.iter().any(|session| session.native_id == r.native_id));
    assert_eq!(rooted.session_paths(&s.meta).len(), 2);
    let sideagent = fixture(
        "qoder/projects/-Users-tester-Github-wakefx/abababab-aaaa-bbbb-cccc-000000000014/subagents/agent-child.jsonl",
    );
    assert!(rooted.file_ref(&sideagent).is_none());
}

#[test]
fn qoder_explicit_null_active_leaf_is_empty() {
    setup();
    let adapter = QoderAdapter::new();
    let r = qoder_null_ref();
    let s = adapter
        .parse_session(&r)
        .expect("qoder empty parse_session");
    let t = adapter
        .parse_transcript(&r)
        .expect("qoder empty parse_transcript");

    assert_eq!(s.meta.title, "Qoder empty rewind");
    assert_eq!(s.meta.project_path, "/Users/tester/Github/wakefx-null");
    assert_eq!(s.meta.message_count, 0);
    assert_eq!(s.meta.tokens_used, None);
    assert!(s.units.is_empty());
    assert!(t.mainline.is_empty());
    assert_eq!(s.unknown_line_count, 0);
}

#[test]
fn copilot_parse_contract() {
    let env = setup();
    let adapter = CopilotAdapter::new();
    let r = db_ref(AgentId::Copilot, &env.copilot_db, "cop-0001");
    let s = adapter.parse_session(&r).expect("copilot parse_session");
    let t = adapter
        .parse_transcript(&r)
        .expect("copilot parse_transcript");

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
    assert_eq!(
        s.units.iter().map(|u| u.seq).collect::<Vec<_>>(),
        vec![0, 1, 2]
    );

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
    let t = adapter
        .parse_transcript(&r)
        .expect("cursor parse_transcript");

    // 标题取 <user_query> 壳内正文;无壳的注入行(<workspace>)归 Meta 被跳过
    assert_eq!(
        s.meta.title,
        "把二维码扫描组件抽出来,注意 useEffect() 的清理"
    );
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
    assert_eq!(
        t.mainline[1].timestamp,
        Some(ms("2026-08-01T09:30:00+08:00"))
    );
    let a = &t.mainline[2];
    assert_eq!(a.tool_calls.len(), 1);
    assert_eq!(a.tool_calls[0].name, "read_file");
    assert_eq!(a.tool_calls[0].output, None); // transcript 不落盘工具结果
    assert_eq!(
        s.units.iter().map(|u| u.seq).collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
}

#[test]
fn opencode_parse_contract() {
    let env = setup();
    let adapter = OpencodeAdapter::new();
    let r = db_ref(AgentId::Opencode, &env.opencode_db, "oc-0001");
    let s = adapter.parse_session(&r).expect("opencode parse_session");
    let t = adapter
        .parse_transcript(&r)
        .expect("opencode parse_transcript");

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
    assert!(a
        .thinking
        .as_deref()
        .unwrap_or_default()
        .contains("先查 effect 依赖"));
    assert_eq!(a.tool_calls.len(), 1);
    assert_eq!(a.tool_calls[0].name, "grep");
    assert!(a.tool_calls[0]
        .output
        .as_deref()
        .unwrap_or_default()
        .contains("QrScanner"));
    assert!(!a.tool_calls[0].is_error);
    assert_eq!(
        s.units.iter().map(|u| u.seq).collect::<Vec<_>>(),
        vec![1, 2]
    );
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
        vec![
            (Role::User, MessageKind::Text),
            (Role::Assistant, MessageKind::Text)
        ]
    );
    // jsonl 的 timestamp 是 unix 秒,应换算成 ms
    assert_eq!(t.mainline[0].timestamp, Some(1785744300000));
    assert_eq!(t.mainline[1].timestamp, Some(1785744360000));
    assert_eq!(
        s.units.iter().map(|u| u.seq).collect::<Vec<_>>(),
        vec![0, 1]
    );
}

#[test]
fn gemini_parse_contract() {
    setup();
    let adapter = GeminiAdapter::new();
    let r = gemini_ref();
    let s = adapter.parse_session(&r).expect("gemini parse_session");
    let t = adapter
        .parse_transcript(&r)
        .expect("gemini parse_transcript");

    // $set 是覆盖式快照:只认最后一条,旧快照文本不得出现
    assert_eq!(t.mainline.len(), 2);
    assert!(t.mainline.iter().all(|m| !m.text.contains("旧快照")));
    assert_eq!(
        roles_kinds(&t.mainline),
        vec![
            (Role::User, MessageKind::Text),
            (Role::Assistant, MessageKind::Text)
        ]
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
    assert_eq!(
        s.units.iter().map(|u| u.seq).collect::<Vec<_>>(),
        vec![0, 1]
    );
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
    let s = adapter
        .parse_session(&r)
        .expect("opencode v2 parse_session");
    let t = adapter
        .parse_transcript(&r)
        .expect("opencode v2 parse_transcript");

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
    assert!(a
        .thinking
        .as_deref()
        .unwrap_or_default()
        .contains("扫描组件"));
    assert_eq!(a.model.as_deref(), Some("nemotron-3.5-lightning-free"));
    assert_eq!(
        s.units.iter().map(|u| u.seq).collect::<Vec<_>>(),
        vec![0, 2]
    );

    // 仅存于 v1 表的会话不带 v2 标记(resume 走 v1 二进制)
    let s1 = adapter
        .parse_session(&db_ref(AgentId::Opencode, &env.opencode_db, "oc-0001"))
        .expect("opencode v1 parse_session");
    assert_eq!(s1.meta.source, None);
}

#[test]
fn opencode_next_real_schema_contract() {
    let env = setup();
    let adapter = OpencodeAdapter::new();
    let roots = adapter.data_roots();
    assert!(
        roots.contains(&env.opencode_db),
        "stable 数据库路径必须保留"
    );
    assert!(
        roots.contains(&env.opencode_next_db),
        "next 数据库路径必须新增扫描"
    );

    let refs = adapter
        .list_session_files()
        .expect("opencode stable + next list");
    let r = refs
        .iter()
        .find(|r| r.native_id == "ocnext-0001")
        .expect("真实 session + session_message 会话应被列出");
    assert!(r
        .file_path
        .starts_with(env.opencode_next_db.to_string_lossy().as_ref()));
    let quick = adapter.quick_meta(&refs).expect("opencode next quick meta");
    assert_eq!(
        quick.get(&r.file_path).and_then(|m| m.source.as_deref()),
        Some("opencode2"),
        "列表快路径也必须带 preview 标记"
    );

    let s = adapter
        .parse_session(r)
        .expect("opencode next parse_session");
    let t = adapter
        .parse_transcript(r)
        .expect("opencode next parse_transcript");
    assert_eq!(s.meta.title, "OpenCode next real schema");
    assert_eq!(s.meta.model.as_deref(), Some("gpt-5.6"));
    assert_eq!(s.meta.tokens_used, Some(32));
    assert_eq!(s.meta.source.as_deref(), Some("opencode2"));
    assert_eq!(s.meta.message_count, 3); // user + assistant + shell tool-only message
    assert_eq!(s.unknown_line_count, 1); // only wibble-next;状态切换是已知元数据

    assert_eq!(
        roles_kinds(&t.mainline),
        vec![
            (Role::User, MessageKind::Text),
            (Role::User, MessageKind::Meta),
            (Role::Assistant, MessageKind::Text),
            (Role::System, MessageKind::Meta),
            (Role::Assistant, MessageKind::Text),
            (Role::System, MessageKind::CompactSummary),
        ]
    );
    let assistant = &t.mainline[2];
    assert_eq!(assistant.text, "next schema 解析成功。");
    assert!(assistant
        .thinking
        .as_deref()
        .unwrap_or_default()
        .contains("effect 清理"));
    assert_eq!(assistant.tool_calls.len(), 1);
    assert_eq!(assistant.tool_calls[0].id, "tool-1");
    assert_eq!(assistant.tool_calls[0].name, "grep");
    assert_eq!(
        assistant.tool_calls[0].output.as_deref(),
        Some("src/QrScanner.tsx:42")
    );
    assert_eq!(t.mainline[4].tool_calls[0].name, "shell");
    assert_eq!(t.mainline[5].kind, MessageKind::CompactSummary);
    assert_eq!(
        s.units.iter().map(|u| u.seq).collect::<Vec<_>>(),
        vec![0, 2, 4]
    );
}

#[test]
fn opencode_default_databases_can_be_removed_individually() {
    let env = setup();
    let dir = tempfile::tempdir().unwrap();
    let store = wake_core::db::Store::open(&dir.path().join("wake.db")).unwrap();

    store
        .add_removed_default_root("opencode", env.opencode_next_db.to_str().unwrap())
        .unwrap();
    let roster = wake_core::adapters::create_adapters_for(&store);
    let adapter = roster
        .iter()
        .find(|a| a.agent() == AgentId::Opencode)
        .expect("移除 next 根后 stable adapter 仍应保留");
    assert!(adapter.supports_individual_root_removal());
    assert!(adapter.data_roots().contains(&env.opencode_db));
    assert!(!adapter.data_roots().contains(&env.opencode_next_db));
    assert!(adapter
        .list_session_files()
        .unwrap()
        .iter()
        .any(|r| r.native_id == "oc-0001"));

    store
        .add_removed_default_root("opencode", env.opencode_db.to_str().unwrap())
        .unwrap();
    let roster = wake_core::adapters::create_adapters_for(&store);
    assert!(
        roster.iter().all(|a| a.agent() != AgentId::Opencode),
        "两条默认库都被移除后不应留下空 adapter"
    );
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
        vec![
            (Role::User, MessageKind::Text),
            (Role::Assistant, MessageKind::Text)
        ]
    );
    let a = &t.mainline[1];
    assert_eq!(a.text, "找到泄漏点,已补清理回调。");
    assert_eq!(a.tool_calls.len(), 1);
    assert_eq!(a.tool_calls[0].name, "bash");
    // toolResult 是独立 role 行,按 toolCallId 回填
    assert!(a.tool_calls[0]
        .output
        .as_deref()
        .unwrap_or_default()
        .contains("QrScanner"));
    assert!(!a.tool_calls[0].is_error);
    assert_eq!(
        s.units.iter().map(|u| u.seq).collect::<Vec<_>>(),
        vec![0, 1]
    );

    // omp 是 pi 的 fork,同一解析核心,只有 key 前缀不同
    let omp = PiAdapter::omp();
    let s2 = omp
        .parse_session(&pi_ref(AgentId::Omp))
        .expect("omp parse_session");
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
    assert!(adapter
        .file_ref(&path.with_file_name("chat_history.jsonl"))
        .is_none());

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
        vec![
            (Role::User, MessageKind::Text),
            (Role::Assistant, MessageKind::Text)
        ]
    );
    assert_eq!(
        t.mainline[0].text,
        "Grok 看看二维码扫描,重点 useEffect() 清理"
    );
    assert_eq!(t.mainline[0].timestamp, Some(1786014300000));
    let a = &t.mainline[1];
    assert_eq!(a.text, "已定位泄漏,补了清理回调。");
    assert!(a
        .thinking
        .as_deref()
        .unwrap_or_default()
        .contains("effect 泄漏"));
    assert_eq!(a.tool_calls.len(), 1);
    assert_eq!(a.tool_calls[0].name, "Grep");
    // tool_call_update 的 content 文本回填 output;字节数组 rawOutput 不碰
    assert!(a.tool_calls[0]
        .output
        .as_deref()
        .unwrap_or_default()
        .contains("found 2 matches"));
    assert!(!a.tool_calls[0].is_error);
    assert_eq!(
        s.units.iter().map(|u| u.seq).collect::<Vec<_>>(),
        vec![0, 1]
    );
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
        vec![
            (Role::User, MessageKind::Text),
            (Role::Assistant, MessageKind::Text)
        ]
    );
    assert_eq!(
        t.mainline[0].text,
        "Kimi 修一下二维码组件的 useEffect() 内存泄漏"
    );
    assert!(t.mainline[1].text.contains("QrScanner"));
    assert_eq!(
        s.units.iter().map(|u| u.seq).collect::<Vec<_>>(),
        vec![0, 1]
    );

    // "New Session" 是占位标题,必须回退首条用户消息
    let placeholder = fs_ref(
        AgentId::Kimi,
        &fixture("kimi/sessions/wd_wakefx_abc123/session_99999999-aaaa-bbbb-cccc-000000000009/agents/main/wire.jsonl"),
        "session_99999999-aaaa-bbbb-cccc-000000000009",
    );
    let s2 = adapter
        .parse_session(&placeholder)
        .expect("kimi placeholder parse");
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
    let s = adapter
        .parse_session(&r)
        .expect("antigravity parse_session");
    let t = adapter
        .parse_transcript(&r)
        .expect("antigravity parse_transcript");

    assert_eq!(s.meta.title, "QR overlay polish"); // 标题在 preview 列
    assert_eq!(s.meta.project_path, "/Users/tester/Github/wakefx"); // file:// URI 解码
    assert_eq!(s.meta.project_name, "wakefx");
    // 带 +00:00 偏移的 datetime 必须解析成功(不能落到 0/mtime 兜底)
    assert_eq!(s.meta.created_at, ms("2026-08-06T13:00:00Z"));
    assert_eq!(s.meta.message_count, 12); // step_count
    assert!(s.meta.file_path.ends_with("#ag-0001")); // 虚拟路径

    // 正文加密:唯一一条 System 消息承载 preview 与说明,FTS 搜得到 preview
    assert_eq!(
        roles_kinds(&t.mainline),
        vec![(Role::System, MessageKind::Text)]
    );
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
    let s = adapter
        .parse_session(r)
        .unwrap_or_else(|e| panic!("[{tag}] parse_session: {e}"));
    let t = adapter
        .parse_transcript(r)
        .unwrap_or_else(|e| panic!("[{tag}] parse_transcript: {e}"));

    assert!(!t.mainline.is_empty(), "[{tag}] mainline 不应为空");
    for (i, m) in t.mainline.iter().enumerate() {
        assert_eq!(m.seq, i as i64, "[{tag}] mainline seq 必须从 0 严格递增");
    }

    assert!(
        !s.units.is_empty(),
        "[{tag}] fixture 应产出至少一个 FTS 单元"
    );
    let seqs: HashSet<i64> = t.mainline.iter().map(|m| m.seq).collect();
    for u in &s.units {
        assert!(
            seqs.contains(&u.seq),
            "[{tag}] unit seq {} 在 mainline 中不存在",
            u.seq
        );
        let m = &t.mainline[u.seq as usize];
        assert_eq!(m.role, u.role, "[{tag}] seq {} 两侧角色不一致", u.seq);
    }

    // 两条解析路径共用核心解析器,meta 关键字段必须一致
    assert_eq!(s.meta.key, t.meta.key, "[{tag}] key 两侧不一致");
    assert_eq!(s.meta.title, t.meta.title, "[{tag}] title 两侧不一致");
    assert_eq!(
        s.meta.message_count, t.meta.message_count,
        "[{tag}] message_count 两侧不一致"
    );
}

#[test]
fn watch_paths_derive_from_data_roots() {
    // watch_paths 没有各家实现,统一由 data_roots 的"现存目录"子集派生——
    // 直接断言派生关系本体,外加 SQLite 型必须无监听目录这条语义守卫
    // (watcher 只认 .jsonl,库变更靠启动/手动刷新;有人把库文件的父目录
    // 塞进 data_roots 就会破)
    let _env = setup();
    for a in wake_core::adapters::create_adapters() {
        let tag = a.agent().as_str();
        let watched = a.watch_paths();
        let expect: Vec<std::path::PathBuf> =
            a.data_roots().into_iter().filter(|p| p.is_dir()).collect();
        assert_eq!(
            watched, expect,
            "[{tag}] watch_paths 必须等于 data_roots 的现存目录子集"
        );
        if matches!(
            a.agent(),
            AgentId::Copilot | AgentId::Opencode | AgentId::Antigravity
        ) {
            assert!(watched.is_empty(), "[{tag}] SQLite 型不该有监听目录");
        }
    }
}

#[test]
fn overlapping_watch_roots_dispatch_to_deepest() {
    // env 自定义根可以落在别家数据树内(CODEX_HOME=~/.claude/projects/codex
    // 这类):事件分派必须取最长匹配根,首个命中会把 codex 的 rollout 交给
    // claude 的 file_ref(对 .jsonl 宽松)以错误 agent 入库。
    // 兄弟目录同名前缀(projects vs projects-old)靠 Path 组件语义天然免疫
    use std::path::{Path, PathBuf};
    let roots = vec![
        (PathBuf::from("/h/.claude/projects"), AgentId::ClaudeCode),
        (
            PathBuf::from("/h/.claude/projects/codex/sessions"),
            AgentId::Codex,
        ),
    ];
    let deep = Path::new("/h/.claude/projects/codex/sessions/2026/08/rollout-x.jsonl");
    assert_eq!(
        wake_core::watcher::resolve_watch_agent(&roots, deep),
        Some(AgentId::Codex),
        "嵌套根必须归最深那家"
    );
    let shallow = Path::new("/h/.claude/projects/p1/sess.jsonl");
    assert_eq!(
        wake_core::watcher::resolve_watch_agent(&roots, shallow),
        Some(AgentId::ClaudeCode)
    );
    let sibling = Path::new("/h/.claude/projects-old/p1/sess.jsonl");
    assert_eq!(
        wake_core::watcher::resolve_watch_agent(&roots, sibling),
        None,
        "同名前缀兄弟目录不该匹配"
    );
}

#[test]
fn data_roots_contract() {
    // roster 单实例契约:create_adapters 返回全量十四家(不按 detect 过滤,
    // scanner 对缺根家靠各自 list_session_files 降级为空);每家必须给出
    // 绝对路径的数据根——"Session locations" 面板、watch_paths 派生、按
    // (agent, 根) 计数全都建立在它上面
    let _env = setup();
    let adapters = wake_core::adapters::create_adapters();
    assert_eq!(adapters.len(), 14, "全量 roster 必须十四家,含本机没装的");
    for a in &adapters {
        let tag = a.agent().as_str();
        let roots = a.data_roots();
        assert!(!roots.is_empty(), "[{tag}] data_roots 不能为空");
        for r in &roots {
            assert!(r.is_absolute(), "[{tag}] 路径须为绝对路径: {r:?}");
        }
    }
    // 假 HOME 里造过数据的四家必须被 detect(默认实现 = data_roots 任一存在)
    // 认出;其中三家是 SQLite 型,它们 watch_paths 恒空,这正是 data_roots
    // 独立存在的理由
    let detected: Vec<&str> = adapters
        .iter()
        .filter(|a| a.detect())
        .map(|a| a.agent().as_str())
        .collect();
    for expect in ["copilot", "opencode", "antigravity", "dsh"] {
        assert!(
            detected.contains(&expect),
            "{expect} 应被检出,实际 {detected:?}"
        );
    }
}

#[test]
fn dsh_torn_final_frame_terminates() {
    // 半写的末帧:写端每次 append 一帧,扫描与 dsh 天然并发,必然读到。
    // zstd decoder 对断尾**反复**返回 UnexpectedEof 而非 EOF——解析器不就地
    // 收尾就是死循环:扫描线程打满 CPU、ScanFinale 永不 Drop、刷新弹窗按
    // 不变量 6 永久锁死。独立 tempdir,不进 list 以免扰动 dsh_parse_contract
    let env = setup();
    let full = fs::read(&env.dsh_log).expect("read dsh zstd log");
    let dir = tempfile::tempdir().expect("tempdir");
    let torn = dir.path().join("session.jsonl.zstd");
    fs::write(&torn, &full[..full.len() - 12]).expect("write torn log");

    let adapter = DshAdapter::new();
    // header 在首帧、完整,所以断尾会话照常进列表(只是内容截止到断点)
    let r = adapter.file_ref(&torn).expect("torn file_ref");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        tx.send(DshAdapter::new().parse_transcript(&r).is_ok()).ok();
    });
    let ok = rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("断尾帧把解析器卡住了(死循环)");
    assert!(ok, "断尾应优雅收尾,不是把整个会话判失败");
}

#[test]
fn dsh_parse_contract() {
    let env = setup();
    let adapter = DshAdapter::new();
    // file_ref 是公开 API:只认 session.jsonl[.zstd],native_id 取 header 的
    // 权威 id(目录名是转义过的 id);子代理会话(origin=subagent)在此过滤
    let r = adapter.file_ref(&env.dsh_log).expect("dsh file_ref");
    assert_eq!(r.native_id, "dsh-e2e4-0001");
    let sub = env
        .dsh_log
        .parent()
        .and_then(|d| d.parent())
        .expect("dsh project dir")
        .join("dsh-sub-0002")
        .join("session.jsonl");
    assert!(adapter.file_ref(&sub).is_none(), "子代理会话不进列表");

    // list 走 <project>/<session>/session.jsonl[.zstd] 两层布局,子代理被滤掉
    let listed = adapter
        .list_session_files()
        .expect("dsh list_session_files");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].native_id, "dsh-e2e4-0001");

    // 压缩配置换挡会两后缀并存:陈旧的一份在 file_ref 就让位(裁决单点,
    // watcher 入口同样受保护,不会把旧文件当主文件解析)
    let stale = env.dsh_log.with_file_name("session.jsonl");
    fs::write(&stale, "{\"type\":\"session\",\"version\":0,\"id\":\"dsh-e2e4-0001\",\"createdAt\":1786000000000,\"cwd\":\"/Users/tester/Github/wakefx\",\"delegationDepth\":0}\n")
        .expect("write stale sibling");
    let hour_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
    fs::OpenOptions::new()
        .write(true)
        .open(&stale)
        .and_then(|f| f.set_modified(hour_ago))
        .expect("age stale sibling");
    assert!(adapter.file_ref(&stale).is_none(), "陈旧 sibling 应让位");
    assert!(
        adapter.file_ref(&env.dsh_log).is_some(),
        "较新主文件不受影响"
    );
    fs::remove_file(&stale).expect("remove stale sibling");

    let s = adapter.parse_session(&r).expect("dsh parse_session");
    let t = adapter.parse_transcript(&r).expect("dsh parse_transcript");

    assert_eq!(s.meta.title, "QR scan dependency fix"); // session/title 事件 last-wins
    assert_eq!(s.meta.key, "dsh:dsh-e2e4-0001");
    assert_eq!(s.meta.project_path, "/Users/tester/Github/wakefx"); // header cwd,不反推目录名
    assert_eq!(s.meta.model.as_deref(), Some("deepseek-chat-v4")); // assistant source.model
                                                                   // usage 是"one model call"的账,按调用累加(1480 + 1560 + 空载体 400)。
                                                                   // surfaceOp={op:replace} 那条是 compaction 的缩短版:整条跳过,它挂的
                                                                   // 99999 不计——它不是新的模型调用,原始节点也不该被它遮蔽
    assert_eq!(s.meta.tokens_used, Some(3440));
    assert_eq!(s.meta.created_at, 1786100000000); // header createdAt(epoch ms)
    assert_eq!(s.meta.updated_at, 1786100007000); // 最后事件 time
    assert_eq!(s.meta.message_count, 2); // 注入上下文归 Meta 不计;replace/空载体都不产生气泡
    assert_eq!(s.unknown_line_count, 1); // mystery-row;*-chunks 打包行与 turn/step 边界不计

    // 连续 assistant step(中间只隔 tool/result)合并一条;source.kind 非 "user"
    // 的注入上下文(plugin / agent-instructions,后者不带 system-reminder 壳)归 Meta
    assert_eq!(
        roles_kinds(&t.mainline),
        vec![
            (Role::User, MessageKind::Text),
            (Role::Assistant, MessageKind::Text),
            (Role::User, MessageKind::Meta),
            (Role::User, MessageKind::Meta),
        ]
    );
    let a = &t.mainline[1];
    assert!(a.text.starts_with("我先查一下扫码组件"));
    assert!(a.text.contains("依赖数组漏了 device"));
    // reasoning 块分离进 thinking,不混入正文
    assert!(a
        .thinking
        .as_deref()
        .unwrap_or_default()
        .contains("依赖数组遗漏"));
    assert!(!a.text.contains("crash on unmount"));
    assert_eq!(a.tool_calls.len(), 1);
    assert_eq!(a.tool_calls[0].name, "read_file");
    // tool/result 事件按 toolCallId 回填输出
    assert!(a.tool_calls[0]
        .output
        .as_deref()
        .unwrap_or_default()
        .contains("useEffect"));
    assert!(!a.tool_calls[0].is_error);
    assert_eq!(
        s.units.iter().map(|u| u.seq).collect::<Vec<_>>(),
        vec![0, 1]
    );
}

#[test]
fn seq_contract_holds_for_all_agents() {
    let env = setup();
    let checks: Vec<(Box<dyn AgentAdapter>, SessionFileRef)> = vec![
        (Box::new(ClaudeAdapter::new()), claude_ref()),
        (Box::new(CodexAdapter::new()), codex_ref()),
        (Box::new(QoderAdapter::new()), qoder_ref()),
        (
            Box::new(CopilotAdapter::new()),
            db_ref(AgentId::Copilot, &env.copilot_db, "cop-0001"),
        ),
        (Box::new(CursorAdapter::new()), cursor_ref()),
        (
            Box::new(OpencodeAdapter::new()),
            db_ref(AgentId::Opencode, &env.opencode_db, "oc-0001"),
        ),
        (Box::new(KiroAdapter::new()), kiro_ref()),
        (Box::new(GeminiAdapter::new()), gemini_ref()),
        (Box::new(PiAdapter::new()), pi_ref(AgentId::Pi)),
        (Box::new(PiAdapter::omp()), pi_ref(AgentId::Omp)),
        (Box::new(GrokAdapter::new()), grok_ref()),
        (Box::new(KimiAdapter::new()), kimi_ref()),
        (
            Box::new(AntigravityAdapter::new()),
            db_ref(AgentId::Antigravity, &env.antigravity_db, "ag-0001"),
        ),
        (
            Box::new(DshAdapter::new()),
            fs_ref(AgentId::Dsh, &env.dsh_log, "dsh-e2e4-0001"),
        ),
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
    let parsed = mk_meta(
        AgentId::ClaudeCode,
        "claude-code:p",
        "p",
        "解析出来的标题",
        None,
    );
    let mut quick = mk_meta(
        AgentId::ClaudeCode,
        "claude-code:q",
        "q",
        "手动改名",
        Some("state"),
    );
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
    let parsed = mk_meta(
        AgentId::Codex,
        "codex:file-uuid",
        "file-uuid",
        "首条消息推导标题",
        Some("IDE extension"),
    );
    let mut quick = mk_meta(
        AgentId::Codex,
        "codex:thread-1",
        "thread-1",
        "用户手动命名",
        Some("vscode"),
    );
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
    let parsed = mk_meta(
        AgentId::Codex,
        "codex:file-uuid",
        "file-uuid",
        "首条消息推导标题",
        None,
    );
    let quick = mk_meta(
        AgentId::Codex,
        "codex:thread-1",
        "thread-1",
        UNTITLED,
        Some("vscode"),
    );
    let merged = codex.merge_quick_meta(parsed, &quick);
    assert_eq!(merged.title, "首条消息推导标题");
    assert_eq!(merged.id, "thread-1");
    assert_eq!(merged.source.as_deref(), Some("vscode")); // parsed 无 source 时兜底

    // 空标题同样不覆盖
    let parsed = mk_meta(
        AgentId::Codex,
        "codex:file-uuid",
        "file-uuid",
        "首条消息推导标题",
        None,
    );
    let quick = mk_meta(AgentId::Codex, "codex:thread-1", "thread-1", "", None);
    let merged = codex.merge_quick_meta(parsed, &quick);
    assert_eq!(merged.title, "首条消息推导标题");
}

// ---------------------------------------------------------------- 自定义 location

/// with_custom_root 契约(不变量 8 配套):agent 不变、数据根全部落在自定义
/// 目录之下(侧档也必须相对它派生,但侧档不在 data_roots,无法在此直接断言)、
/// 缺根照旧降级为 Ok(空)
#[test]
fn with_custom_root_contract() {
    setup();
    let dir = tempfile::tempdir().unwrap();
    let custom = dir.path().join("somewhere-else");
    for base in wake_core::adapters::create_adapters() {
        let inst = base.with_custom_root(custom.clone());
        assert_eq!(
            inst.agent(),
            base.agent(),
            "{:?}: 自定义实例换了 agent",
            base.agent()
        );
        let roots = inst.data_roots();
        assert!(!roots.is_empty(), "{:?}: 自定义实例无数据根", base.agent());
        for r in &roots {
            assert!(
                r.starts_with(&custom),
                "{:?}: 数据根 {} 溢出自定义目录 {}",
                base.agent(),
                r.display(),
                custom.display()
            );
        }
        let refs = inst
            .list_session_files()
            .unwrap_or_else(|e| panic!("{:?}: 缺根必须 Ok(空) 降级,却 Err: {e}", base.agent()));
        assert!(refs.is_empty(), "{:?}: 空目录读出了会话", base.agent());
    }

    // codex 的"直接选中 rollout 日期树"分支:顶层有 YYYY 目录时,dir 本身
    // 即 sessions 根(用户常会选中 sessions 目录本体)
    let tree = dir.path().join("codex-sessions-copy");
    fs::create_dir_all(tree.join("2026")).unwrap();
    let inst = CodexAdapter::new().with_custom_root(tree.clone());
    assert!(
        inst.data_roots().contains(&tree),
        "codex 未把 rollout 树本体当 sessions 根: {:?}",
        inst.data_roots()
    );
}

/// AgentId::ALL 是侧栏/面板/表单下拉共用的顺序事实源:必须与枚举声明序
/// (= Ord,用户 2026-08-20 钉的展示序)严格一致,且与 roster 的 agent 集合
/// 等同——第十五家漏进任何一份名单,在这里爆而不是静默从下拉里消失
#[test]
fn agent_id_all_matches_ord_and_roster() {
    setup();
    assert!(
        AgentId::ALL.windows(2).all(|w| w[0] < w[1]),
        "ALL 未按声明序(Ord)排列"
    );
    let mut roster: Vec<AgentId> = wake_core::adapters::create_adapters()
        .iter()
        .map(|a| a.agent())
        .collect();
    roster.sort();
    let mut all = AgentId::ALL.to_vec();
    all.sort();
    assert_eq!(all, roster, "ALL 与 roster 的 agent 集合不一致");
}

/// 预设 location 的移除 = 压制该家默认实例;该家的自定义实例仍从默认模板
/// 构造、照常在场(编辑预设 = 压默认 + 记自定义,正是这个组合)
#[test]
fn removed_defaults_suppress_instances() {
    setup();
    let roster = wake_core::adapters::create_adapters_with(&[], &[AgentId::ClaudeCode]);
    assert_eq!(roster.len(), 13);
    assert!(roster.iter().all(|a| a.agent() != AgentId::ClaudeCode));

    let dir = tempfile::tempdir().unwrap();
    let roster = wake_core::adapters::create_adapters_with(
        &[(AgentId::ClaudeCode, dir.path().to_path_buf())],
        &[AgentId::ClaudeCode],
    );
    let claude: Vec<_> = roster
        .iter()
        .filter(|a| a.agent() == AgentId::ClaudeCode)
        .collect();
    assert_eq!(claude.len(), 1, "默认被压制后应只剩自定义实例");
    assert!(claude[0]
        .data_roots()
        .iter()
        .all(|r| r.starts_with(dir.path())));
}

#[test]
fn codex_subagent_transcript_injection_is_meta() {
    setup();
    let adapter = CodexAdapter::new();
    let r = codex_subagent_ref();
    let t = adapter
        .parse_transcript(&r)
        .expect("codex subagent parse_transcript");

    // 分支 / subagent 线程把**父会话的整段 transcript** 打包成一条
    // role=user 的消息喂进来,里面含父会话的 assistant 输出。不识别的话,
    // 父会话里 AI 说的话会显示成这个会话里用户发的
    let injected = t
        .mainline
        .iter()
        .find(|m| {
            m.text
                .starts_with("The following is the Codex agent history")
        })
        .expect("注入的 transcript 应当仍在 mainline 里(只是归 Meta)");
    assert_eq!(injected.kind, MessageKind::Meta);
    assert!(
        injected.text.contains("[2] assistant:"),
        "父会话的 assistant 输出确实躺在这条 role=user 消息里"
    );

    // AGENTS.md 注入同理
    let agents_md = t
        .mainline
        .iter()
        .find(|m| m.text.starts_with("# AGENTS.md instructions"))
        .expect("AGENTS.md 注入");
    assert_eq!(agents_md.kind, MessageKind::Meta);

    // 真实用户输入不受影响
    let real = t
        .mainline
        .iter()
        .find(|m| m.text == "继续")
        .expect("真实用户消息");
    assert_eq!(real.role, Role::User);
    assert_eq!(real.kind, MessageKind::Text);

    // Meta 不进 FTS:注入进来的父会话内容不该被搜索命中
    let s = adapter
        .parse_session(&r)
        .expect("codex subagent parse_session");
    assert!(
        !s.units.iter().any(|u| u.text.contains("[2] assistant:")),
        "注入的父会话 transcript 不得进入检索单元"
    );
    // 标题也不能取注入内容
    assert_eq!(s.meta.title, "继续");
}

/// normalize_custom_root(静态分派,不依赖 roster——该家默认被移除时也要
/// 生效):codex 直选 sessions 树或**平铺 archived** 且父目录呈 home 形态时
/// 上提一层(侧档/归档找回,2026-08-24 Codex review);裸拷贝与其他家恒等
#[test]
fn codex_normalize_lifts_sessions_dir_to_home() {
    use wake_core::adapters::normalize_custom_root;
    setup();
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("codex-home");
    fs::create_dir_all(home.join("sessions").join("2026")).unwrap();
    fs::create_dir_all(home.join("archived_sessions")).unwrap();
    fs::write(
        home.join("archived_sessions").join("rollout-x.jsonl"),
        b"{}",
    )
    .unwrap();
    fs::write(home.join("state_5.sqlite"), b"x").unwrap();
    assert_eq!(
        normalize_custom_root(AgentId::Codex, home.join("sessions")),
        home
    );
    assert_eq!(
        normalize_custom_root(AgentId::Codex, home.join("archived_sessions")),
        home,
        "平铺 archived 目录也应上提到 home"
    );

    let bare = tmp.path().join("codex-copy");
    fs::create_dir_all(bare.join("2026")).unwrap();
    assert_eq!(
        normalize_custom_root(AgentId::Codex, bare.clone()),
        bare,
        "裸树不上提"
    );

    // 空的真实 sessions 目录(表单允许空路径):凭目录名 + 父级独立证据上提
    let home2 = tmp.path().join("codex-home-2");
    fs::create_dir_all(home2.join("sessions")).unwrap();
    fs::write(home2.join("state_5.sqlite"), b"x").unwrap();
    assert_eq!(
        normalize_custom_root(AgentId::Codex, home2.join("sessions")),
        home2,
        "空 sessions 目录也应上提"
    );
    // 孤立的空 sessions 目录(父级无任何 home 证据)保持原样
    let lone = tmp.path().join("lone");
    fs::create_dir_all(lone.join("sessions")).unwrap();
    assert_eq!(
        normalize_custom_root(AgentId::Codex, lone.join("sessions")),
        lone.join("sessions")
    );

    let d = tmp.path().join("whatever");
    assert_eq!(
        normalize_custom_root(AgentId::ClaudeCode, d.clone()),
        d,
        "无覆写的家恒等"
    );
}

/// create_adapters_for:roster 必须吃索引库里的 location 配置——scan CLI 曾用
/// 默认 roster 对配置过的库跑扫描,把自定义根会话当"已删"整批清掉
/// (2026-08-24 Codex review)
#[test]
fn create_adapters_for_honors_store_config() {
    setup();
    let dir = tempfile::tempdir().unwrap();
    let store = wake_core::db::Store::open(&dir.path().join("t.db")).unwrap();
    store
        .add_custom_root("claude-code", "/tmp/claude-backup")
        .unwrap();
    store.add_removed_default("codex").unwrap();
    let roster = wake_core::adapters::create_adapters_for(&store);
    assert!(
        roster.iter().all(|a| a.agent() != AgentId::Codex),
        "被移除的预设仍在 roster"
    );
    assert_eq!(
        roster
            .iter()
            .filter(|a| a.agent() == AgentId::ClaudeCode)
            .count(),
        2,
        "自定义 location 未生效"
    );
}

/// location 开关按真实数据根过滤 active roster，但管理快照必须保留停用行。
/// Codex 的 sessions/archived 来自同一 adapter，验证两行可以独立控制。
#[test]
fn disabled_location_stays_configured_but_leaves_active_roster() {
    setup();
    let dir = tempfile::tempdir().unwrap();
    let store = wake_core::db::Store::open(&dir.path().join("t.db")).unwrap();
    let codex_home = dir.path().join("codex-copy");
    let sessions = codex_home.join("sessions");
    let archived = codex_home.join("archived_sessions");
    fs::create_dir_all(&sessions).unwrap();
    fs::create_dir_all(&archived).unwrap();
    store
        .add_custom_root("codex", codex_home.to_str().unwrap())
        .unwrap();
    store
        .set_location_enabled("codex", sessions.to_str().unwrap(), false)
        .unwrap();

    let roster = wake_core::adapters::create_adapter_roster_for(&store);
    assert!(roster.locations.iter().any(|location| {
        location.agent == AgentId::Codex && location.path == sessions && !location.enabled
    }));
    assert!(roster.locations.iter().any(|location| {
        location.agent == AgentId::Codex && location.path == archived && location.enabled
    }));
    let active_roots: Vec<_> = roster
        .active
        .iter()
        .filter(|adapter| adapter.agent() == AgentId::Codex)
        .flat_map(|adapter| adapter.data_roots())
        .collect();
    assert!(!active_roots.contains(&sessions), "停用根仍进入扫描 roster");
    assert!(
        active_roots.contains(&archived),
        "同一 location 的另一根被误关"
    );
}

/// path_owns 的边界字典:分隔符边界(sessions-old 不属 sessions)、SQLite
/// 虚拟路径的 '#'、文件系统根 "/"(strip_prefix 剥掉的正是分隔符,通用分支
/// 会全判界外——2026-08-24 Codex review)
#[test]
fn path_owns_boundaries() {
    use wake_core::adapters::path_owns;
    assert!(path_owns("/a/sessions", "/a/sessions"));
    assert!(path_owns("/a/sessions", "/a/sessions/x.jsonl"));
    assert!(!path_owns("/a/sessions", "/a/sessions-old/x.jsonl"));
    assert!(path_owns("/a/store.db", "/a/store.db#42"));
    assert!(path_owns("/", "/anything/below"));
    assert!(!path_owns("/b", "/a/x"));
    // 空根不拥有任何东西:通用分支的 strip_prefix("") 会原样返回整条路径
    assert!(!path_owns("", "/a/x"));
}

/// path_owns 的 Windows 形态。**只能在 Windows 上跑**:`is_separator('\\')`
/// 在 Unix 上是 false(反斜杠在那里是合法文件名字符),同一组断言在 Linux
/// 上恒不成立——这也正是下面这个 bug 只在 Windows 上现形的原因。
/// 卡住的是"根判据必须是自身以分隔符收尾":一度用过 `parent().is_none()`,
/// 而 UNC 共享根在 Windows 上恰好 parent 为 None 且**不**以分隔符收尾,
/// 于是退化成裸前缀匹配、把 agents-old 吞进 agents(2026-08-25 review)
#[test]
#[cfg(target_os = "windows")]
fn path_owns_windows_shapes() {
    use wake_core::adapters::path_owns;
    // UNC 共享根:兄弟共享必须判在界外
    assert!(path_owns(r"\\nas\agents", r"\\nas\agents\x.jsonl"));
    assert!(!path_owns(r"\\nas\agents", r"\\nas\agents-old\x.jsonl"));
    // 盘符根以分隔符收尾,一切后代在界内
    assert!(path_owns(r"C:\", r"C:\Users\me\x.jsonl"));
    // 反斜杠边界与 POSIX 同规
    assert!(path_owns(
        r"C:\Users\me\.claude",
        r"C:\Users\me\.claude\p\x.jsonl"
    ));
    assert!(!path_owns(
        r"C:\Users\me\.claude",
        r"C:\Users\me\.claude-old\x.jsonl"
    ));
    // SQLite 虚拟路径
    assert!(path_owns(
        r"C:\Users\me\store.db",
        r"C:\Users\me\store.db#42"
    ));
}

/// 裸 Codex 数据目录按目录名保角色(2026-08-24 Codex review):独立 archived
/// 拷贝的会话保住 archived 标记;空的独立 sessions 目录以自身为数据根,
/// 日后落盘的 rollout 能被发现
#[test]
fn codex_bare_data_dir_keeps_role() {
    setup();
    let tmp = tempfile::tempdir().unwrap();
    let arch = tmp.path().join("archived_sessions");
    fs::create_dir_all(&arch).unwrap();
    fs::copy(
        fixture("codex/sessions/2026/08/02/rollout-2026-08-02T09-15-00-22222222-aaaa-bbbb-cccc-000000000002.jsonl"),
        arch.join("rollout-2026-08-02T09-15-00-22222222-aaaa-bbbb-cccc-000000000002.jsonl"),
    )
    .unwrap();
    let inst = CodexAdapter::new().with_custom_root(arch.clone());
    let refs = inst.list_session_files().unwrap();
    assert_eq!(refs.len(), 1, "独立 archived 目录应以自身为数据根");
    let parsed = inst.parse_session(&refs[0]).unwrap();
    assert!(parsed.meta.archived, "archived 角色丢失,归档会话被标成活跃");

    let empty_sessions = tmp.path().join("sessions");
    fs::create_dir_all(&empty_sessions).unwrap();
    let inst = CodexAdapter::new().with_custom_root(empty_sessions.clone());
    assert!(
        inst.data_roots().contains(&empty_sessions),
        "空的独立 sessions 目录应以自身为数据根"
    );
}

/// SQLite 型构造器直接给到库文件路径也认(预设行编辑值即库文件,当目录拼
/// 会得到 <db>/<db> 死路径——2026-08-24 Codex review)
#[test]
fn sqlite_custom_root_accepts_db_file() {
    let env = setup();
    let inst = CopilotAdapter::new().with_custom_root(env.copilot_db.clone());
    assert_eq!(inst.data_roots(), vec![env.copilot_db.clone()]);
    let inst = OpencodeAdapter::new().with_custom_root(env.opencode_db.clone());
    assert_eq!(inst.data_roots(), vec![env.opencode_db.clone()]);
    let inst = AntigravityAdapter::new().with_custom_root(env.antigravity_db.clone());
    assert_eq!(inst.data_roots(), vec![env.antigravity_db.clone()]);
}

/// 实例路由(不变量 8 配套):同 agent 多实例时,文件按"拥有其根的实例"
/// 分派(最长前缀 + 分隔符边界),匹配不到根回退默认实例
#[test]
fn adapter_ix_for_routes_to_owning_instance() {
    setup();
    let dir = tempfile::tempdir().unwrap();
    let custom = dir.path().to_path_buf();
    let roster =
        wake_core::adapters::create_adapters_with(&[(AgentId::ClaudeCode, custom.clone())], &[]);
    assert_eq!(roster.len(), 15, "14 默认 + 1 自定义");
    assert_eq!(roster[14].agent(), AgentId::ClaudeCode);

    let under = format!("{}/projects/p/x.jsonl", custom.display());
    assert_eq!(
        wake_core::adapters::adapter_ix_for(&roster, AgentId::ClaudeCode, &under),
        Some(14),
        "自定义根下的文件应路由到自定义实例"
    );
    // 兄弟目录(裸前缀)不得吸入
    let sibling = format!("{}-old/x.jsonl", custom.display());
    assert_eq!(
        wake_core::adapters::adapter_ix_for(&roster, AgentId::ClaudeCode, &sibling),
        Some(0),
        "边界外路径应回退默认实例"
    );
    assert_eq!(
        wake_core::adapters::adapter_ix_for(&roster, AgentId::Codex, "/nowhere/x.jsonl"),
        Some(1),
        "无根命中回退该 agent 首个实例"
    );
}
