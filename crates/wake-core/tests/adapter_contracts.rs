//! 各家 adapter 的解析契约测试:全部走公开 API(`AgentAdapter` trait),
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
use wake_core::adapters::codebuddy::CodebuddyAdapter;
use wake_core::adapters::codex::CodexAdapter;
use wake_core::adapters::copilot::CopilotAdapter;
use wake_core::adapters::craft::CraftAdapter;
use wake_core::adapters::cursor::CursorAdapter;
use wake_core::adapters::cursor_ide::CursorIdeAdapter;
use wake_core::adapters::devin::DevinAdapter;
use wake_core::adapters::dsh::DshAdapter;
use wake_core::adapters::gemini::GeminiAdapter;
use wake_core::adapters::grok::GrokAdapter;
use wake_core::adapters::hermes::HermesAdapter;
use wake_core::adapters::kimi::KimiAdapter;
use wake_core::adapters::kiro::KiroAdapter;
use wake_core::adapters::openclaw::OpenclawAdapter;
use wake_core::adapters::opencode::OpencodeAdapter;
use wake_core::adapters::pi::PiAdapter;
use wake_core::adapters::qoder::QoderAdapter;
use wake_core::adapters::zcode::ZcodeAdapter;
use wake_core::adapters::AgentAdapter;
use wake_core::models::*;

mod common;
use common::fixture;

// ---------------------------------------------------------------- 测试环境

struct TestEnv {
    copilot_db: PathBuf,
    opencode_db: PathBuf,
    opencode_next_db: PathBuf,
    antigravity_db: PathBuf,
    dsh_log: PathBuf,
    hermes_db: PathBuf,
    openclaw_db: PathBuf,
    cursor_ide_db: PathBuf,
    zcode_db: PathBuf,
    devin_db: PathBuf,
    /// 假 HOME 目录本体,持有 TempDir 保证整个测试进程期间不被清理
    _home: tempfile::TempDir,
}

static ENV: OnceLock<TestEnv> = OnceLock::new();

/// 把文件 mtime 往前拨 `secs` 秒:按 mtime 戳判脏的缓存在同一毫秒内的两次写之间分不出新旧
fn touch_forward(path: &Path, secs: u64) {
    let file = fs::File::options().write(true).open(path).unwrap();
    file.set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(secs))
        .unwrap();
}

/// 所有测试的第一步:建假 HOME(含 SQLite fixture 库与 gemini 的
/// projects.json)并把 HOME 指过去。必须先于任何 Adapter::new()。
/// 读一个 adapter 的全部默认来源(项目模式除外——那要项目根);契约测试里的
/// "list_memories()" 就是它
fn all_memories(adapter: &Box<dyn AgentAdapter>) -> anyhow::Result<Vec<MemoryDoc>> {
    adapter.list_memories(&adapter.memory_sources(), &[])
}

fn setup() -> &'static TestEnv {
    ENV.get_or_init(|| {
        let home = tempfile::Builder::new()
            .prefix("wake-adapter-contracts-")
            .tempdir()
            .expect("create fake home");

        // 侧档与 SQLite fixture 库的搭建在 tests/common(remote_sync 的假远端
        // home 复用同一份,两边不会各自漂移)
        let sc = common::stage_sidecars(home.path());

        common::isolate_home(home.path());
        TestEnv {
            copilot_db: sc.copilot_db,
            opencode_db: sc.opencode_db,
            opencode_next_db: sc.opencode_next_db,
            antigravity_db: sc.antigravity_db,
            dsh_log: sc.dsh_log,
            hermes_db: sc.hermes_db,
            openclaw_db: sc.openclaw_db,
            cursor_ide_db: sc.cursor_ide_db,
            zcode_db: sc.zcode_db,
            devin_db: sc.devin_db,
            _home: home,
        }
    })
}

// ---------------------------------------------------------------- 小工具

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

/// 默认 roster 的实例数。**不等于 `AgentId::ALL.len()`**:Cursor 一家有两个
/// 数据源(CLI 的 agent-transcripts 与 IDE 的 state.vscdb),各占一个实例。
/// 新增 agent 或给某家再加数据源时,这个数跟着加一——契约要卡的是"漏了实例
/// 就爆",而不是"每家恰好一个"
const DEFAULT_INSTANCES: usize = AgentId::ALL.len() + 1;

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
fn claude_images_ref() -> SessionFileRef {
    fs_ref(
        AgentId::ClaudeCode,
        &fixture("claude/projects/-Users-tester-Github-wakefx/44444444-aaaa-bbbb-cccc-000000000004.jsonl"),
        "44444444-aaaa-bbbb-cccc-000000000004",
    )
}
/// CodeBuddy 与 WorkBuddy 同构,同一份 fixture 两个 agent 复用(仅 root/AgentId 不同)
fn codebuddy_ref(agent: AgentId) -> SessionFileRef {
    fs_ref(
        agent,
        &fixture("codebuddy/projects/Users-fixture-src-wakefx/cb000001-aaaa-bbbb-cccc-000000000001.jsonl"),
        "cb000001-aaaa-bbbb-cccc-000000000001",
    )
}

fn codebuddy_topic_ref() -> SessionFileRef {
    fs_ref(
        AgentId::Codebuddy,
        &fixture("codebuddy/projects/Users-fixture-src-wakefx/cb000002-aaaa-bbbb-cccc-000000000002.jsonl"),
        "cb000002-aaaa-bbbb-cccc-000000000002",
    )
}

fn codex_images_ref() -> SessionFileRef {
    fs_ref(
        AgentId::Codex,
        &fixture("codex/sessions/2026/08/08/rollout-2026-08-08T09-00-00-55555555-aaaa-bbbb-cccc-000000000005.jsonl"),
        "55555555-aaaa-bbbb-cccc-000000000005",
    )
}
fn codex_ref() -> SessionFileRef {
    fs_ref(
        AgentId::Codex,
        &fixture("codex/sessions/2026/08/02/rollout-2026-08-02T09-15-00-22222222-aaaa-bbbb-cccc-000000000002.jsonl"),
        "22222222-aaaa-bbbb-cccc-000000000002",
    )
}

#[test]
fn claude_inline_images_only_decode_for_transcripts() {
    setup();
    let adapter = ClaudeAdapter::new();
    let r = claude_images_ref();
    let transcript = adapter.parse_transcript(&r).expect("claude images");
    let users: Vec<_> = transcript
        .mainline
        .iter()
        .filter(|message| message.role == Role::User)
        .collect();

    assert_eq!(users.len(), 4);
    assert_eq!(users[0].text, "这个按钮的颜色对不上");
    assert_eq!(users[0].images.len(), 1);
    assert_eq!(users[0].images[0].text_offset, 0);
    assert_eq!(users[0].images[0].media_type, "image/png");
    assert!(users[0].images[0].bytes.starts_with(b"\x89PNG"));
    assert!(users[1].text.is_empty());
    assert_eq!(users[1].images.len(), 1, "纯图片消息不能被丢掉");
    assert_eq!(users[2].images[0].media_type, "image/heic");
    assert!(users[3].images.is_empty());
    assert_eq!(users[3].text, "[image]");
    assert_eq!(transcript.meta.title, "这个按钮的颜色对不上");

    let session = adapter.parse_session(&r).expect("claude image index");
    let user_units: Vec<_> = session
        .units
        .iter()
        .filter(|unit| unit.role == Role::User)
        .map(|unit| unit.text.as_str())
        .collect();
    assert_eq!(
        user_units,
        vec![
            "[image]\n\n这个按钮的颜色对不上",
            "[image]",
            "[image]",
            "[image]"
        ]
    );
}

#[test]
fn codex_inline_images_decode_and_strip_desktop_wrappers() {
    setup();
    let adapter = CodexAdapter::new();
    let r = codex_images_ref();
    let transcript = adapter.parse_transcript(&r).expect("codex images");
    let users: Vec<_> = transcript
        .mainline
        .iter()
        .filter(|message| message.role == Role::User)
        .collect();

    assert_eq!(users.len(), 5);
    assert_eq!(users[0].text, "这个按钮颜色不对");
    assert_eq!(users[0].images.len(), 1);
    assert_eq!(users[0].images[0].text_offset, users[0].text.len());
    assert!(users[0].images[0].bytes.starts_with(b"\x89PNG"));
    assert!(users[1].text.is_empty());
    assert_eq!(users[1].images.len(), 1, "纯图片消息不能被丢掉");
    assert!(users[2].images.is_empty());
    assert_eq!(users[2].text, "[image]");
    assert_eq!(users[3].text, "参考这张图，人物面部不要有任何变化");
    assert_eq!(users[3].kind, MessageKind::Text);
    assert!(users[4].text.is_empty());
    assert_eq!(users[4].images.len(), 1);

    let session = adapter.parse_session(&r).expect("codex image index");
    let user_units: Vec<_> = session
        .units
        .iter()
        .filter(|unit| unit.role == Role::User)
        .map(|unit| unit.text.as_str())
        .collect();
    assert_eq!(
        user_units,
        vec![
            "这个按钮颜色不对\n\n[image]",
            "[image]",
            "[image]",
            "参考这张图，人物面部不要有任何变化\n\n[image]",
            "[image]"
        ]
    );
    assert_eq!(session.meta.title, "这个按钮颜色不对");
}
fn codex_branch_ref() -> SessionFileRef {
    fs_ref(
        AgentId::Codex,
        &fixture("codex/sessions/2026/08/07/rollout-2026-08-07T12-44-01-33333333-aaaa-bbbb-cccc-000000000003.jsonl"),
        "33333333-aaaa-bbbb-cccc-000000000003",
    )
}
fn codex_review_ref() -> SessionFileRef {
    fs_ref(
        AgentId::Codex,
        &fixture("codex/sessions/2026/08/09/rollout-2026-08-09T10-30-00-44444444-aaaa-bbbb-cccc-000000000004.jsonl"),
        "44444444-aaaa-bbbb-cccc-000000000004",
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
fn claude_bridge_metadata_can_pass_cleanup_review() {
    use std::sync::atomic::AtomicBool;
    use wake_core::{cleanup, db::Store};

    setup(); // All paths below belong to the shared synthetic home.
    let adapter = ClaudeAdapter::new();
    let root = adapter.data_roots().remove(0);
    fs::create_dir_all(&root).unwrap();
    // macOS temp paths may start with the /var alias; cleanup requires real paths.
    let root = root.canonicalize().unwrap();
    let adapter = adapter.with_custom_root(root.clone());
    let project = tempfile::tempdir_in(&root).unwrap();
    let path = project.path().join("bridge-cleanup-fixture.jsonl");
    let original = fs::read_to_string(&claude_ref().file_path).unwrap();
    let valid = original
        .lines()
        .filter(|line| !line.contains("wibble-experimental"))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    fs::write(&path, &valid).unwrap();
    let reference = fs_ref(AgentId::ClaudeCode, &path, "bridge-cleanup-fixture");
    let parsed = adapter.parse_session(&reference).unwrap();
    assert_eq!(parsed.unknown_line_count, 0);
    assert_eq!(parsed.meta.message_count, 3);
    let transcript = adapter.parse_transcript(&reference).unwrap();
    assert_eq!(transcript.unknown_line_count, 0);
    assert_eq!(transcript.mainline.len(), 5);

    let db_dir = tempfile::tempdir().unwrap();
    let store = Store::open(&db_dir.path().join("test.db")).unwrap();
    store
        .write_session(&parsed.meta, reference.mtime_ms, &parsed.units)
        .unwrap();
    let adapters = vec![adapter];
    let inventory = cleanup::inventory(&store, &adapters).unwrap();
    assert_eq!(inventory.candidates.len(), 1);
    let review = cleanup::review(
        &store,
        &adapters,
        inventory.candidates,
        &AtomicBool::new(false),
        |_| {},
    )
    .unwrap();
    assert_eq!(review.ready.len(), 1);
    assert!(review.skipped.is_empty());
    cleanup::revalidate(&store, &adapters, &review.ready[0]).unwrap();

    // Supporting known bookkeeping must not allow malformed or unknown records.
    for invalid in [r#"{"type":"unknown-fixture-record"}"#, "{truncated"] {
        fs::write(&path, format!("{valid}{invalid}\n")).unwrap();
        let inventory = cleanup::inventory(&store, &adapters).unwrap();
        let review = cleanup::review(
            &store,
            &adapters,
            inventory.candidates,
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap();
        assert!(review.ready.is_empty());
        assert_eq!(review.skipped.len(), 1);
        assert_eq!(review.skipped[0].reason, "Session has unrecognized content");
        assert!(path.exists());
    }
}

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
    // "wibble-experimental" 计 unknown;summary / atis-latch / bridge-session 不计
    assert_eq!(s.unknown_line_count, 1);
    assert_eq!(t.unknown_line_count, 1);

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

/// Codex 把非用户线程与用户会话写进同一棵树;文件边界按首行 session_meta 把
/// 整族挡掉。全量枚举走 file_ref 漏斗,这里两道都断言,守住"不可能分家"。
/// 判不出用途的首行——老写端无 thread_source、exec 的 Feature 标签、2025 老格式、
/// 截断的 JSON——一律保守可见(issue #30)
#[test]
fn codex_internal_threads_are_excluded_at_the_file_boundary() {
    setup();
    let home = tempfile::tempdir().unwrap();
    let day = home.path().join("sessions/2026/09/14");
    fs::create_dir_all(&day).unwrap();
    let adapter = CodexAdapter::new().with_custom_root(home.path().to_path_buf());

    fn id(n: u32) -> String {
        format!("00000000-0000-4000-8000-{n:012}")
    }
    // 一条 session_meta 首行;extra 是要试的 source / thread_source 字段
    let meta = |n: u32, extra: serde_json::Value| {
        let mut payload = serde_json::json!({
            "id": id(n),
            "timestamp": "2026-09-14T00:00:00.000Z",
            "cwd": "/work/wake",
            "originator": "codex_work_desktop"
        });
        payload
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        let line = serde_json::json!({
            "timestamp": "2026-09-14T00:00:00.000Z",
            "type": "session_meta",
            "payload": payload
        });
        (id(n), line.to_string())
    };
    let spawn = serde_json::json!({
        "parent_thread_id": id(99), "depth": 1, "agent_path": "/root/research"
    });
    // 对象来源默认即噪音,唯一的例外是 `subagent.thread_spawn`(issue #42)。
    // 列全上游各种落盘形态,是为了谁把默认那一侧从"除 thread_spawn 外全挡"
    // 收窄成白名单(只认 review/compact 几个词)时立刻红
    let cases = [
        // guardian auto-review:早期写端 thread_source 仍是 subagent;只带对象;
        // issue #30 的过渡格式只带 thread_source;字符串来源 + thread_source=subagent
        (meta(1, serde_json::json!({"source": {"subagent": {"other": "guardian"}}, "thread_source": "subagent"})), false),
        (meta(2, serde_json::json!({"source": {"subagent": {"other": "guardian"}}})), false),
        (meta(3, serde_json::json!({"source": "cli", "thread_source": "guardian_review"})), false),
        (meta(4, serde_json::json!({"source": "exec", "thread_source": "subagent"})), false),
        // `/review`、compaction
        (meta(5, serde_json::json!({"source": {"subagent": "review"}, "thread_source": "subagent"})), false),
        (meta(6, serde_json::json!({"source": {"subagent": "compact"}, "thread_source": "subagent"})), false),
        // memory consolidation 的 subagent / internal 两种写法;上游今天落盘前会把
        // Internal(Guardian) 改写成 subagent/other,哪天不改写了也要认
        (meta(8, serde_json::json!({"source": {"subagent": "memory_consolidation"}})), false),
        (meta(9, serde_json::json!({"source": {"internal": "memory_consolidation"}, "thread_source": "memory_consolidation"})), false),
        (meta(10, serde_json::json!({"source": {"internal": "guardian"}, "thread_source": "guardian_review"})), false),
        // `spawn_agent` 子代理:同样是 subagent/对象来源,但它是用户自己派的活
        (meta(7, serde_json::json!({"source": {"subagent": {"thread_spawn": spawn}}, "thread_source": "subagent"})), true),
        // 用户线程:字符串来源的各种写法,含 `codex exec --thread-source` 的 Feature 标签
        (meta(11, serde_json::json!({"source": "vscode", "thread_source": "user"})), true),
        (meta(12, serde_json::json!({"source": "exec"})), true),
        (meta(13, serde_json::json!({"source": "exec", "thread_source": "nightly_automation"})), true),
        (meta(14, serde_json::json!({"source": "unknown"})), true),
        // 首行不是 session_meta(2025 老格式)或 JSON 截断:判不出用途就保守放行
        ((id(15), format!(r#"{{"id":"{}","timestamp":"2025-09-15T07:46:43.457Z","instructions":null}}"#, id(15))), true),
        ((id(16), r#"{"timestamp":"2026-09-14T00:20:00.000Z","type":"session_meta","payload":{"id":""#.to_string()), true),
    ];

    let mut expected = HashSet::new();
    for (index, ((id, line), visible)) in cases.into_iter().enumerate() {
        let path = day.join(format!("rollout-2026-09-14T00-{index:02}-00-{id}.jsonl"));
        fs::write(&path, format!("{line}\n")).unwrap();
        assert_eq!(
            adapter.file_ref(&path).is_some(),
            visible,
            "watcher file_ref visibility for {id}"
        );
        if visible {
            expected.insert(id);
        }
    }

    // archived_sessions(平铺布局)走同一漏斗:内部线程照样挡、用户线程照样进
    let archived = home.path().join("archived_sessions");
    fs::create_dir_all(&archived).unwrap();
    for (n, extra, visible) in [
        (
            17,
            serde_json::json!({"source": "cli", "thread_source": "user"}),
            true,
        ),
        (
            18,
            serde_json::json!({"source": {"subagent": "review"}, "thread_source": "subagent"}),
            false,
        ),
    ] {
        let (id, line) = meta(n, extra);
        let path = archived.join(format!("rollout-2026-09-14T01-00-00-{id}.jsonl"));
        fs::write(&path, format!("{line}\n")).unwrap();
        assert_eq!(
            adapter.file_ref(&path).is_some(),
            visible,
            "archived file_ref visibility for {id}"
        );
        if visible {
            expected.insert(id);
        }
    }

    let actual: HashSet<String> = adapter
        .list_session_files()
        .unwrap()
        .into_iter()
        .map(|r| r.native_id)
        .collect();
    assert_eq!(actual, expected, "full scan must use the same boundary");
}

/// `spawn_agent` 子代理进索引(issue #42),但 `fork_turns` 复制进来的父线程
/// 历史必须整段折掉:原样入库就是子线程偷走父线程的标题、父线程每一轮在
/// FTS 里出现两次、Insights 的 prompt 数翻倍。分界点认首行的
/// `subagent_history_start_ordinal`,子线程自己的工具调用必须活下来——折叠是
/// 按下标 splice 的,而工具调用默认挂在"最后一条助手消息"上,fork 段以助手
/// 消息收尾时那一条正在待删区里
#[test]
fn codex_spawned_subagent_keeps_only_its_own_turns() {
    setup();
    let home = tempfile::tempdir().unwrap();
    let (parent_id, child_id, _) = common::stage_codex_spawn_pair(home.path());
    let adapter = CodexAdapter::new().with_custom_root(home.path().to_path_buf());

    let refs = adapter.list_session_files().unwrap();
    let ids: HashSet<String> = refs.iter().map(|r| r.native_id.clone()).collect();
    assert_eq!(
        ids,
        HashSet::from([parent_id.clone(), child_id.clone()]),
        "子代理必须与父线程一起进索引"
    );

    let child = refs.iter().find(|r| r.native_id == child_id).unwrap();
    let parsed = adapter.parse_session(child).unwrap();
    assert_eq!(
        parsed.meta.title, "review_issue17",
        "标题取 spawn_agent 的任务名:子线程自己没有用户消息"
    );

    let transcript = adapter.parse_transcript(child).unwrap();
    let visible: Vec<&TranscriptMessage> = transcript
        .mainline
        .iter()
        .filter(|m| m.kind != MessageKind::Meta)
        .collect();
    let text: String = visible
        .iter()
        .map(|m| m.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !text.contains("inherited"),
        "fork 段整段折成一条 Meta:{text}"
    );
    assert!(text.contains("child found the missing dependency array"));
    let tools: Vec<&ToolCallView> = visible.iter().flat_map(|m| &m.tool_calls).collect();
    assert_eq!(
        tools.len(),
        1,
        "子线程自己的工具调用不得随 fork 段一起被删掉"
    );
    assert_eq!(tools[0].output.as_deref(), Some("CHILD_TOOL_OUTPUT"));

    let indexed: String = parsed
        .units
        .iter()
        .map(|u| u.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !indexed.contains("inherited"),
        "父线程的话不得在子线程里再进一次 FTS:{indexed}"
    );
    // seq 契约:两侧解析器同源,FTS 单元的 seq 必须等于详情页的消息序号
    assert_eq!(parsed.units[0].seq, visible[0].seq);

    assert_eq!(
        adapter.parent_links().unwrap(),
        vec![(format!("codex:{child_id}"), format!("codex:{parent_id}"))]
    );
}

/// 首行没写 `subagent_history_start_ordinal`(老写端)时退回派活信封,并且
/// **认收件人**:fork 段里那封发给 `/root/other_task` 的信不算数
#[test]
fn codex_spawn_cut_falls_back_to_the_dispatch_envelope() {
    setup();
    let home = tempfile::tempdir().unwrap();
    let (_, child_id, child_path) = common::stage_codex_spawn_pair(home.path());
    strip_history_ordinal(&child_path);

    let adapter = CodexAdapter::new().with_custom_root(home.path().to_path_buf());
    let refs = adapter.list_session_files().unwrap();
    let child = refs.iter().find(|r| r.native_id == child_id).unwrap();
    let parsed = adapter.parse_session(child).unwrap();
    assert_eq!(parsed.meta.title, "review_issue17");
    let indexed: String = parsed
        .units
        .iter()
        .map(|u| u.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!indexed.contains("inherited"), "{indexed}");
    assert!(indexed.contains("child found the missing dependency array"));
}

/// 既没有权威 ordinal、又认不出发给自己的信封(半写入、未来格式、agent_path
/// 缺失)就**一条都不折**:内容多一份好过凭空消失,与"首行判不出用途一律保守
/// 放行"同一取舍。退回"第一条信封"才是要防的事——那会在 fork 段中间切一刀
#[test]
fn codex_spawn_without_a_usable_cut_keeps_everything() {
    setup();
    for blank_agent_path in [false, true] {
        let home = tempfile::tempdir().unwrap();
        let (_, child_id, child_path) = common::stage_codex_spawn_pair(home.path());
        let mut head = strip_history_ordinal(&child_path);
        if blank_agent_path {
            // agent_path 认不出来:不得退化成"第一条 agent_message 就是分界"
            head["payload"]["source"]["subagent"]["thread_spawn"]
                .as_object_mut()
                .unwrap()
                .remove("agent_path");
            head["payload"]
                .as_object_mut()
                .unwrap()
                .remove("agent_path");
        } else {
            // 信封整行不见了,但子线程首行还在,它仍然是子代理
            let rest = fs::read_to_string(&child_path).unwrap();
            let kept: String = rest
                .lines()
                .filter(|line| !line.contains("\"recipient\":\"/root/review_issue17\""))
                .map(|line| format!("{line}\n"))
                .collect();
            fs::write(&child_path, kept).unwrap();
        }
        if blank_agent_path {
            rewrite_head(&child_path, &head);
        }

        let adapter = CodexAdapter::new().with_custom_root(home.path().to_path_buf());
        let refs = adapter.list_session_files().unwrap();
        let child = refs.iter().find(|r| r.native_id == child_id).unwrap();
        let parsed = adapter.parse_session(child).unwrap();
        let indexed: String = parsed
            .units
            .iter()
            .map(|u| u.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            indexed.contains("inherited parent turn") && indexed.contains("missing dependency"),
            "blank_agent_path={blank_agent_path}: {indexed}"
        );
        // 仍然认得出是子代理,所以标题还是任务名(agent_path 抹掉那次退昵称)
        let expected = if blank_agent_path {
            "Wegener"
        } else {
            "review_issue17"
        };
        assert_eq!(
            parsed.meta.title, expected,
            "blank_agent_path={blank_agent_path}"
        );
    }
}

/// 只跑了工具、还没出正文的子代理:`has_real` 为假,折叠若在 event_fallback
/// 里留下标记,那条标记会把子线程自己的 response_item 整条流挤掉
#[test]
fn codex_running_subagent_keeps_its_tool_calls_over_the_event_stream() {
    setup();
    let home = tempfile::tempdir().unwrap();
    let (_, child_id, child_path) = common::stage_codex_spawn_pair(home.path());
    // 权威 ordinal 去掉,改由信封定界(下面要往 fork 段里插行,插了 ordinal 就
    // 对不上了);fork 段补一条 event_msg 让 event_fallback 非空,子线程自己
    // 那段只留工具调用(删掉它的助手正文)
    strip_history_ordinal(&child_path);
    let event = serde_json::json!({
        "timestamp": "2026-09-16T09:05:00.000Z",
        "type": "event_msg",
        "payload": {"type": "user_message", "message": "inherited parent turn about the qr login bug"}
    });
    let mut kept: Vec<String> = Vec::new();
    for line in fs::read_to_string(&child_path).unwrap().lines() {
        if line.contains("child found the missing dependency array") {
            continue;
        }
        if line.contains("\"recipient\":\"/root/review_issue17\"") {
            kept.push(event.to_string());
        }
        kept.push(line.to_string());
    }
    fs::write(
        &child_path,
        kept.iter().map(|l| format!("{l}\n")).collect::<String>(),
    )
    .unwrap();

    let adapter = CodexAdapter::new().with_custom_root(home.path().to_path_buf());
    let refs = adapter.list_session_files().unwrap();
    let child = refs.iter().find(|r| r.native_id == child_id).unwrap();
    let transcript = adapter.parse_transcript(child).unwrap();
    let tools: Vec<&ToolCallView> = transcript
        .mainline
        .iter()
        .flat_map(|m| &m.tool_calls)
        .collect();
    assert_eq!(tools.len(), 1, "{:#?}", transcript.mainline);
    assert_eq!(tools[0].output.as_deref(), Some("CHILD_TOOL_OUTPUT"));
}

/// 父子关系只认 state DB 的 thread_spawn_edges——它与 quick_meta 的 key 同一
/// id 空间,而且每次都是现问磁盘,不受"本进程扫到哪了"影响。代价写在这里:
/// 没有 state DB 的根认不出关系,子线程降级成带任务名的顶层会话,不是 Untitled
#[test]
fn codex_spawn_links_need_the_state_db_registry() {
    setup();
    let home = tempfile::tempdir().unwrap();
    let (_, child_id, _) = common::stage_codex_spawn_pair(home.path());
    fs::remove_file(home.path().join("state_5.sqlite")).unwrap();

    let adapter = CodexAdapter::new().with_custom_root(home.path().to_path_buf());
    let refs = adapter.list_session_files().unwrap();
    assert!(adapter.parent_links().unwrap().is_empty());
    let child = refs.iter().find(|r| r.native_id == child_id).unwrap();
    assert_eq!(
        adapter.parse_session(child).unwrap().meta.title,
        "review_issue17"
    );
}

/// 子代理线程的 state 行没有手工命名时,Codex 自动生成的 title 是按 fork 进来
/// 的父线程对话编的,不能拿它盖掉任务名(0.6.6 修掉的"标题是父会话的副本")
#[test]
fn codex_spawn_title_survives_codex_auto_title() {
    setup();
    let home = tempfile::tempdir().unwrap();
    let (_, child_id, _) = common::stage_codex_spawn_pair(home.path());
    let conn = rusqlite::Connection::open(home.path().join("state_5.sqlite")).unwrap();
    conn.execute(
        "UPDATE threads SET title = 'inherited parent turn about the qr login bug' WHERE id = ?1",
        rusqlite::params![child_id],
    )
    .unwrap();
    drop(conn);

    let adapter = CodexAdapter::new().with_custom_root(home.path().to_path_buf());
    let refs = adapter.list_session_files().unwrap();
    let child = refs.iter().find(|r| r.native_id == child_id).unwrap();
    let quick = adapter.quick_meta(&refs).expect("state db readable");
    let parsed = adapter.parse_session(child).unwrap();
    let merged = adapter.merge_quick_meta(parsed.meta, quick.get(&child.file_path).unwrap());
    assert_eq!(merged.title, "review_issue17");

    // 用户手工命名(name 列)仍然说了算
    let conn = rusqlite::Connection::open(home.path().join("state_5.sqlite")).unwrap();
    conn.execute(
        "UPDATE threads SET name = 'Renamed by user' WHERE id = ?1",
        rusqlite::params![child_id],
    )
    .unwrap();
    drop(conn);
    let quick = adapter.quick_meta(&refs).unwrap();
    let parsed = adapter.parse_session(child).unwrap();
    let merged = adapter.merge_quick_meta(parsed.meta, quick.get(&child.file_path).unwrap());
    assert_eq!(merged.title, "Renamed by user");
}

/// 子线程首行:去掉权威 ordinal,返回改过的首行 JSON 供调用方继续改
fn strip_history_ordinal(child_path: &Path) -> serde_json::Value {
    let text = fs::read_to_string(child_path).unwrap();
    let (head, rest) = text.split_once('\n').unwrap();
    let mut head: serde_json::Value = serde_json::from_str(head).unwrap();
    head["payload"]
        .as_object_mut()
        .unwrap()
        .remove("subagent_history_start_ordinal");
    fs::write(child_path, format!("{head}\n{rest}")).unwrap();
    head
}

fn rewrite_head(child_path: &Path, head: &serde_json::Value) {
    let text = fs::read_to_string(child_path).unwrap();
    let (_, rest) = text.split_once('\n').unwrap();
    fs::write(child_path, format!("{head}\n{rest}")).unwrap();
}

#[test]
fn codex_review_output_is_readable_and_not_duplicated() {
    setup();
    let adapter = CodexAdapter::new();
    let r = codex_review_ref();
    let transcript = adapter
        .parse_transcript(&r)
        .expect("codex review transcript");

    assert_eq!(transcript.mainline.len(), 1);
    let review = &transcript.mainline[0];
    assert_eq!(review.role, Role::Assistant);
    assert_eq!(review.kind, MessageKind::Text);
    assert!(review.text.contains("## Code review"));
    assert!(review.text.contains("Changes requested"));
    assert!(review.text.contains("[P2] Keep the selected row"));
    assert!(review.text.contains("workbench.rs:42–44"));
    assert!(review.text.contains("98%"));
    assert!(!review.text.contains("Fallback review text"));
    assert!(!review.text.contains("\"findings\""));
    assert!(!review.text.contains("<user_action>"));
    assert_eq!(transcript.unknown_line_count, 0);

    let session = adapter.parse_session(&r).expect("codex review session");
    assert_eq!(session.meta.message_count, 1);
    assert_eq!(session.meta.title, UNTITLED);
    assert_eq!(session.units.len(), 1);
    assert!(session.units[0].text.contains("Keep the selected row"));
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
fn qoder_credits_are_not_reported_as_tokens() {
    setup();
    let adapter = QoderAdapter::new();
    let original = qoder_ref();
    let source = fs::read_to_string(&original.file_path).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("credits-only.jsonl");

    for (has_real_tokens, expected) in [(false, None), (true, Some(40))] {
        let rows: Vec<String> = source
            .lines()
            .map(|line| {
                let mut row: serde_json::Value = serde_json::from_str(line).unwrap();
                if row["type"] == "assistant" {
                    let reported = has_real_tokens && row["uuid"] == "q-assistant-2";
                    row["message"]["usage"] = serde_json::json!({
                        "input_tokens": if reported { 30 } else { 0 },
                        "output_tokens": if reported { 10 } else { 0 },
                        "cache_creation_input_tokens": 0,
                        "cache_read_input_tokens": 0,
                        "credits": 1.5362908,
                        "context_usage_ratio": 0.1379833
                    });
                }
                serde_json::to_string(&row).unwrap()
            })
            .collect();
        fs::write(&path, rows.join("\n")).unwrap();
        let r = fs_ref(AgentId::Qoder, &path, &original.native_id);
        let parsed = adapter.parse_session(&r).unwrap();
        assert_eq!(parsed.meta.tokens_used, expected);
        assert_eq!(parsed.meta.message_count, 4);
        assert_eq!(
            adapter.parse_transcript(&r).unwrap().meta.tokens_used,
            expected
        );
    }
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

/// Cursor IDE(state.vscdb):正文按 `fullConversationHeadersOnly` 的气泡顺序
/// 组装。fixture 里 KV 的 key 字典序(aa/bb/cc/kk/mm/zz)与对话顺序
/// (zz→aa→mm→bb→cc→kk)**故意相反**——照 key 序读就会把对话打乱,这是
/// 本测试要防的主要回归。
/// Cursor 一家两源:移除其中一条 location 不得连带关掉另一条。
/// 两个实例都必须开 `supports_individual_root_removal`——否则面板的 Remove
/// 会退回 `removed_defaults` 的按 agent 整家压制,用户移除 CLI 转录目录时
/// IDE 会话会一起消失(反之亦然)。
#[test]
fn cursor_two_sources_remove_independently() {
    let env = setup();
    let roster = wake_core::adapters::create_adapters();
    let cursors: Vec<&Box<dyn wake_core::adapters::AgentAdapter>> = roster
        .iter()
        .filter(|a| a.agent() == AgentId::Cursor)
        .collect();
    assert_eq!(cursors.len(), 2, "Cursor 应有 CLI 与 IDE 两个数据源");
    // 同一会话两源各有一份时,scanner 的副本裁决按 dedup_rank 固定让 CLI 源
    // 胜出、IDE 副本作回退(端到端见 scanner_finale::cursor_transcript_outranks_ide_copy)
    let (cli, ide) = (cursors[0], cursors[1]);
    assert_eq!(
        ide.data_roots(),
        vec![env.cursor_ide_db.clone()],
        "roster 顺序:CLI 在前、IDE 库在后"
    );
    assert!(
        cli.dedup_rank() < ide.dedup_rank(),
        "CLI 源必须排在 IDE 库副本之前"
    );
    for a in &cursors {
        assert!(
            a.supports_individual_root_removal(),
            "两源都要能单独移除,只开一边等于没开"
        );
    }

    // 排除 A 的根:A 交出空根实例(roster 组装处据此丢弃),B 原样保留
    let (a, b) = (cursors[0], cursors[1]);
    let a_root = a.data_roots();
    assert_eq!(a_root.len(), 1, "两源各自单根");
    let trimmed = a
        .excluding_data_roots(&a_root)
        .expect("移除自己的根应给出替代实例");
    assert!(
        trimmed.data_roots().is_empty(),
        "自己的根被移除后不应再声明数据根"
    );
    assert!(
        b.excluding_data_roots(&a_root).is_none(),
        "另一源的根与我无关,应原样保留"
    );
}

/// 同 AgentId 两实例下的路由:会话必须落到**拥有其 file_path 的那个源**。
/// 这条不成立时后果不对称——IDE 会话的虚拟路径 `<db>#<id>` 若被路由到 CLI
/// 实例,`cursor.rs::session_paths` 会取其父目录(= 整个 globalStorage/)
/// 交给删除流程,一次删除就会端掉 Cursor 的全部 IDE 数据。
#[test]
fn cursor_two_sources_route_by_path() {
    let env = setup();
    let roster = wake_core::adapters::create_adapters();

    // IDE 的虚拟路径 → IDE 实例,且它不把库文件的父目录当会话目录
    let ide_path = format!("{}#cide-0001", env.cursor_ide_db.display());
    let ide = wake_core::adapters::adapter_for(&roster, AgentId::Cursor, &ide_path)
        .expect("IDE 虚拟路径必须有实例认领");
    assert_eq!(ide.data_roots(), vec![env.cursor_ide_db.clone()]);
    let ide_meta = ide
        .parse_session(&db_ref(AgentId::Cursor, &env.cursor_ide_db, "cide-0001"))
        .expect("ide parse")
        .meta;
    let targets = ide.session_paths(&ide_meta);
    assert_eq!(
        targets,
        vec![ide_path.clone()],
        "SQLite 型会话的删除目标是虚拟路径本身(磁盘上不存在,trash 会跳过),\
         绝不能是库文件所在目录"
    );
    assert!(
        !targets.iter().any(|t| t.ends_with("globalStorage")),
        "删除目标落到 globalStorage 目录就会端掉整个 Cursor IDE 数据"
    );
    assert!(ide.cleanup_paths(&ide_meta).is_none());
    let cleanup_db = tempfile::tempdir().unwrap();
    let store = wake_core::db::Store::open(&cleanup_db.path().join("cleanup.db")).unwrap();
    store
        .write_session(&ide_meta, ide_meta.updated_at, &[])
        .unwrap();
    let inventory = wake_core::cleanup::inventory(&store, &roster).unwrap();
    assert!(
        inventory.candidates.is_empty(),
        "IDE 数据库会话不能进入文件清理候选"
    );
    assert_eq!(inventory.unavailable.len(), 1);
    assert_eq!(inventory.unavailable[0].session.key, ide_meta.key);
    assert_eq!(
        inventory.unavailable[0].reason,
        "This source does not support independent file cleanup"
    );
    assert!(env.cursor_ide_db.exists());

    // CLI 的真实文件路径 → CLI 实例
    let cli_path = fixture(
        "cursor/projects/wakefx-cursor-proj/agent-transcripts/33333333-aaaa-bbbb-cccc-000000000003/33333333-aaaa-bbbb-cccc-000000000003.jsonl",
    );
    let cli =
        wake_core::adapters::adapter_for(&roster, AgentId::Cursor, &cli_path.to_string_lossy())
            .expect("CLI 路径必须有实例认领");
    assert_ne!(
        cli.data_roots(),
        vec![env.cursor_ide_db.clone()],
        "CLI 转录不该落到 IDE 实例"
    );
}

#[test]
fn cursor_ide_parse_contract() {
    let env = setup();
    let adapter = CursorIdeAdapter::new();

    // 枚举:零气泡的草稿不进列表,有正文的都在——含与 CLI 转录同 id 的
    // 3333…03(谁胜出由 scanner 按 dedup_rank 定,本源只管如实枚举)
    let mut ids: Vec<String> = adapter
        .list_session_files()
        .expect("cursor ide list")
        .into_iter()
        .map(|r| r.native_id)
        .collect();
    ids.sort();
    assert_eq!(
        ids,
        vec![
            "33333333-aaaa-bbbb-cccc-000000000003",
            "44444444-aaaa-bbbb-cccc-000000000004",
            "55555555-aaaa-bbbb-cccc-000000000005",
            "cide-0001",
            "cide-0002",
            "cide-0004"
        ]
    );

    let r = db_ref(AgentId::Cursor, &env.cursor_ide_db, "cide-0001");
    let s = adapter.parse_session(&r).expect("cursor ide parse_session");
    let t = adapter
        .parse_transcript(&r)
        .expect("cursor ide parse_transcript");

    // 与 cursor.rs(CLI 源)共用 AgentId 与 key 前缀:同一 composer 在两源
    // 各有一份时,scanner 才能按同一 key 去重裁决
    assert_eq!(s.meta.key, "cursor:cide-0001");
    assert_eq!(s.meta.agent, AgentId::Cursor);
    assert_eq!(s.meta.title, "Cursor IDE QR fix");
    assert_eq!(s.meta.project_path, "/Users/tester/Github/wakefx");
    assert_eq!(s.meta.project_name, "wakefx");
    assert_eq!(s.meta.created_at, 1786300000000);
    assert_eq!(s.meta.updated_at, 1786300600000);
    assert!(s.meta.file_path.ends_with("#cide-0001"));
    assert_eq!(s.unknown_line_count, 0);

    // 空壳流式气泡(bb)、已被清理的气泡(cc)与 value 为 NULL 的气泡行(dd)
    // 都不产出消息,也不让整段解析失败
    assert_eq!(
        roles_kinds(&t.mainline),
        vec![
            (Role::User, MessageKind::Text),
            (Role::Assistant, MessageKind::Text),
            (Role::Assistant, MessageKind::Text),
            (Role::Assistant, MessageKind::Text),
            (Role::Assistant, MessageKind::Text),
        ]
    );
    assert!(t.mainline[0].text.contains("二维码扫描为何闪退"));
    assert_eq!(
        t.mainline[1].thinking.as_deref(),
        Some("先查 effect 依赖和清理函数")
    );
    assert_eq!(t.mainline[2].tool_calls.len(), 1);
    assert_eq!(t.mainline[2].tool_calls[0].name, "grep");
    assert_eq!(t.mainline[2].tool_calls[0].id, "call_ide_1");
    assert!(t.mainline[2].tool_calls[0]
        .output
        .as_deref()
        .is_some_and(|o| o.contains("QrScanner.tsx")));
    assert!(!t.mainline[2].tool_calls[0].is_error);

    // rawArgs 为空、参数只在 params 的形态(真实库里占 11%,且全是
    // 终端命令/文件编辑这类最该被搜到的调用)
    let term = &t.mainline[3].tool_calls[0];
    assert_eq!(term.name, "run_terminal_command_v2");
    assert!(
        term.input
            .as_deref()
            .is_some_and(|i| i.contains("cargo test -p wakefx")),
        "params 里的命令行必须进 input,否则终端调用在索引里是空的"
    );
    assert!(
        term.input_preview.contains("cargo test"),
        "FTS 收的是 preview,命令行必须出现在这里"
    );
    assert_eq!(
        term.output.as_deref(),
        Some("test result: ok. 3 passed"),
        "终端结果包在 {{\"output\":…}} 对象里,应展平成人读文本"
    );
    assert!(t.mainline[4].text.contains("已在清理回调里停止扫描"));
    assert_eq!(
        t.mainline[0].timestamp,
        Some(ms("2026-08-09T10:00:05.000Z"))
    );
    assert_eq!(
        t.mainline[4].timestamp,
        Some(ms("2026-08-09T10:00:12.000Z"))
    );
    // FTS 单元只收 text 与工具名/输入摘要(units_from_messages 的全局口径),
    // seq 1 是纯 thinking 消息、正文为空,故不进索引——详情页仍然有它
    assert_eq!(
        s.units.iter().map(|u| u.seq).collect::<Vec<_>>(),
        vec![0, 2, 3, 4]
    );
    assert!(
        s.units.iter().any(|u| u.text.contains("cargo test")),
        "终端命令必须能被搜到"
    );

    // name 为空 + 无 lastUpdatedAt:标题回退首条用户消息,
    // updated_at 回退末条气泡的 ISO createdAt
    let r2 = db_ref(AgentId::Cursor, &env.cursor_ide_db, "cide-0002");
    let s2 = adapter
        .parse_session(&r2)
        .expect("cursor ide fallback parse");
    assert_eq!(s2.meta.title, "空 name 会话的兜底标题应取这句");
    assert_eq!(s2.meta.created_at, 1786310000000);
    assert_eq!(s2.meta.updated_at, ms("2026-08-09T11:00:20.000Z"));

    // 子代理归属来自 composerHeaders.subagentInfo
    assert!(adapter.manages_parent_links());
    assert_eq!(
        adapter.parent_links().unwrap(),
        vec![(
            "cursor:cide-0004".to_string(),
            "cursor:cide-0001".to_string()
        )]
    );
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
    assert_eq!(s.meta.tokens_used, Some(4242 + 4300)); // 两次调用,含合并前的工具调用
    assert_eq!(t.meta.tokens_used, s.meta.tokens_used);
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
    assert_eq!(s2.meta.tokens_used, Some(4242 + 4300));
    assert_eq!(
        omp.parse_transcript(&pi_ref(AgentId::Omp))
            .unwrap()
            .meta
            .tokens_used,
        s2.meta.tokens_used
    );
}

#[test]
fn grok_parse_contract() {
    setup();
    let adapter = GrokAdapter::new().with_custom_root(fixture("grok"));
    adapter.begin_scan();
    assert!(adapter.parent_links().unwrap().contains(&(
        "grok:aaaaaaaa-aaaa-bbbb-cccc-0000000000aa".into(),
        "grok:77777777-aaaa-bbbb-cccc-000000000007".into(),
    )));
    let parent_meta = fixture("grok/sessions/%2FUsers%2Ftester%2FGithub%2Fwakefx/77777777-aaaa-bbbb-cccc-000000000007/subagents/child-fixture/meta.json");
    assert!(adapter.is_snapshot_event(&parent_meta));
    // file_ref 是公开 API:只认 updates.jsonl,native_id 取会话目录名
    let path = fixture("grok/sessions/%2FUsers%2Ftester%2FGithub%2Fwakefx/77777777-aaaa-bbbb-cccc-000000000007/updates.jsonl");
    assert!(!adapter.is_snapshot_event(&path));
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
    // roster 覆盖契约:create_adapters 返回全量各家(不按 detect 过滤,
    // scanner 对缺根家靠各自 list_session_files 降级为空);每个实例必须给出
    // 绝对路径的数据根——"Session locations" 面板、watch_paths 派生、按
    // (agent, 根) 计数全都建立在它上面
    let _env = setup();
    let adapters = wake_core::adapters::create_adapters();
    assert_eq!(
        adapters.len(),
        DEFAULT_INSTANCES,
        "全量 roster 必须含每家(本机没装的也在)与每个数据源"
    );
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
        (
            Box::new(HermesAdapter::new()),
            db_ref(AgentId::Hermes, &env.hermes_db, "hs-0001"),
        ),
        (
            Box::new(OpenclawAdapter::new()),
            db_ref(AgentId::Openclaw, &env.openclaw_db, "claw-0001"),
        ),
        (Box::new(OpenclawAdapter::new()), openclaw_legacy_ref(env)),
        (
            Box::new(CodebuddyAdapter::new()),
            codebuddy_ref(AgentId::Codebuddy),
        ),
        (
            Box::new(CodebuddyAdapter::workbuddy()),
            codebuddy_ref(AgentId::Workbuddy),
        ),
        (
            Box::new(ZcodeAdapter::new()),
            db_ref(AgentId::Zcode, &env.zcode_db, "zc-0001"),
        ),
        (
            CraftAdapter::new().with_custom_root(fixture("craft-agents")),
            craft_ref("260801-brave-otter"),
        ),
        (
            CraftAdapter::new().with_custom_root(fixture("craft-agents")),
            craft_ref("260803-bold-pine"),
        ),
        (
            Box::new(DevinAdapter::new()),
            db_ref(AgentId::Devin, &env.devin_db, "dv-0001"),
        ),
        (
            Box::new(DevinAdapter::new()),
            db_ref(AgentId::Devin, &env.devin_db, "dv-0002"),
        ),
    ];
    for (adapter, r) in &checks {
        assert_seq_contract(adapter.as_ref(), r);
    }
}

fn craft_session_dir(session: &str) -> PathBuf {
    fixture("craft-agents/workspaces/wakefx-ws/sessions").join(session)
}

fn craft_ref(session: &str) -> SessionFileRef {
    fs_ref(
        AgentId::CraftAgents,
        &craft_session_dir(session).join("session.jsonl"),
        &format!("ws_f1x7e5a0/{session}"),
    )
}

#[test]
fn craft_agents_parse_contract() {
    setup();
    let adapter = CraftAdapter::new().with_custom_root(fixture("craft-agents"));
    // 选中 `.craft-agent` 本身、工作区集合、单个工作区,枚举出同一批会话。隐藏的 mini
    // 编辑会话(260804)不进列表;原子写的 .tmp、Pi 引擎的 .pi-sessions/ 都不是会话
    let listed = adapter.list_session_files().expect("craft list");
    let ids: Vec<String> = listed.iter().map(|r| r.native_id.clone()).collect();
    assert_eq!(
        ids,
        [
            "ws_f1x7e5a0/260801-brave-otter",
            "ws_f1x7e5a0/260802-quiet-lake",
            "ws_f1x7e5a0/260803-bold-pine",
            "ws_f1x7e5a0/260805-calm-reed",
        ]
    );
    for root in [
        fixture("craft-agents/workspaces"),
        fixture("craft-agents/workspaces/wakefx-ws"),
    ] {
        let again: Vec<String> = CraftAdapter::new()
            .with_custom_root(root)
            .list_session_files()
            .unwrap()
            .into_iter()
            .map(|r| r.native_id)
            .collect();
        assert_eq!(again, ids);
    }
    let otter = craft_session_dir("260801-brave-otter");
    assert!(adapter.file_ref(&otter.join("session.jsonl.tmp")).is_none());
    assert!(adapter
        .file_ref(&craft_session_dir("260802-quiet-lake").join(
            ".pi-sessions/2026-08-09T09-00-00-000Z_01a0d3b2-0000-7000-8000-000000000002.jsonl"
        ))
        .is_none());
    assert!(
        adapter
            .file_ref(&craft_session_dir("260804-tiny-fern").join("session.jsonl"))
            .is_none(),
        "隐藏的 mini 会话不进列表"
    );

    // Claude 后端的主会话:标题是 craft 的 name,项目是用户设的工作目录,活动时间取
    // lastMessageAt(lastUsedAt 连"点开看一眼"都会刷新)
    let r = craft_ref("260801-brave-otter");
    let s = adapter.parse_session(&r).expect("craft parse_session");
    let t = adapter
        .parse_transcript(&r)
        .expect("craft parse_transcript");
    assert_eq!(s.meta.key, "craft-agents:ws_f1x7e5a0/260801-brave-otter");
    assert_eq!(s.meta.title, "Fix QR scanner crash");
    assert_eq!(s.meta.project_path, "/Users/tester/Github/wakefx");
    assert_eq!(s.meta.model.as_deref(), Some("claude-opus-5-5"));
    assert_eq!(s.meta.tokens_used, Some(1500));
    assert_eq!(s.meta.created_at, 1786200000000);
    assert_eq!(s.meta.updated_at, 1786200060000);
    assert_eq!(s.meta.message_count, 3);
    assert_eq!(s.unknown_line_count, 1, "词汇表外的行计数(漂移金丝雀)");
    assert_eq!(
        roles_kinds(&t.mainline),
        vec![
            (Role::User, MessageKind::Text),
            (Role::Assistant, MessageKind::Text),
            (Role::Assistant, MessageKind::Text),
            (Role::System, MessageKind::Meta), // 压缩完成
            (Role::User, MessageKind::Meta),   // 只给模型看的推动消息
        ]
    );
    assert!(t.mainline[0].text.contains("crashes on unmount"));
    assert!(t.mainline[0].text.contains("[Attached file: crash.png]"));
    // 工具挂在发起它的那段助手话上;子代理(Task)内部的工具与话不进主线
    let tools = &t.mainline[1].tool_calls;
    assert_eq!(
        tools.iter().map(|tc| tc.name.as_str()).collect::<Vec<_>>(),
        ["Read", "Bash"]
    );
    assert!(tools[0]
        .output
        .as_deref()
        .unwrap_or_default()
        .contains("useEffect"));
    assert!(!tools[0].is_error);
    assert!(tools[1].is_error);
    let bash_input = tools[1].input.as_deref().unwrap_or_default();
    assert!(!bash_input.contains("{{SESSION_PATH}}"), "{bash_input}");
    assert!(bash_input.contains("260801-brave-otter/plans/notes.md"));
    assert!(!t.mainline.iter().any(|m| m.text.contains("Subagent notes")));

    // Pi 后端(连 ChatGPT 的那种):模型剥掉 pi/ 前缀;没设工作目录就归到工作区本身——
    // 取首行记的工作区路径(~ 形态按家目录展开),不是文件此刻所在的位置;归档与
    // "自动化建的"如实带上
    let lake = adapter.parse_session(&listed[1]).unwrap().meta;
    assert_eq!(lake.model.as_deref(), Some("gpt-6-astra"));
    assert_eq!(
        lake.tokens_used,
        Some(1000),
        "totalTokens 为 0 时退回 input + output"
    );
    assert!(lake.archived);
    assert_eq!(lake.source.as_deref(), Some("automation"));
    assert_eq!(
        lake.project_path,
        wake_core::adapters::expand_tilde("~/.craft-agent/workspaces/wakefx-ws")
    );
    assert_eq!(lake.project_name, "wakefx-ws");

    // 分支:从父会话复制来的那段折成一条标记;标题取分支自己的第一句,不拿父会话的
    // 预览顶替;紧跟分叉点的工具起一条新的承载,不挂到被折掉的父会话消息上
    let pine = adapter.parse_transcript(&listed[2]).unwrap();
    assert_eq!(
        roles_kinds(&pine.mainline),
        vec![
            (Role::System, MessageKind::Meta),
            (Role::Assistant, MessageKind::Text),
            (Role::User, MessageKind::Text),
            (Role::Assistant, MessageKind::Text),
        ]
    );
    assert!(pine.mainline[0].text.contains("2 messages inherited"));
    assert_eq!(pine.mainline[1].tool_calls.len(), 1);
    assert_eq!(pine.meta.title, "Try the camera module instead");

    // 子任务:计划(SubmitPlan 交上来的 Markdown)是助手正文,授权请求与报错是系统事件
    let reed = adapter.parse_transcript(&listed[3]).unwrap();
    assert_eq!(
        roles_kinds(&reed.mainline),
        vec![
            (Role::User, MessageKind::Text),
            (Role::Assistant, MessageKind::Text),
            (Role::System, MessageKind::Meta),
            (Role::System, MessageKind::Meta),
        ]
    );
    assert!(reed.mainline[1].text.contains("Regression test plan"));
    // lastMessageAt 只跟到用户那句;之后的计划 / 报错也是活动
    assert_eq!(reed.meta.updated_at, 1786700004000);

    // 子任务与分支挂回原会话
    assert!(adapter.manages_parent_links());
    let mut links = adapter.parent_links().unwrap();
    links.sort();
    let otter_key = "craft-agents:ws_f1x7e5a0/260801-brave-otter".to_string();
    assert_eq!(
        links,
        vec![
            (
                "craft-agents:ws_f1x7e5a0/260803-bold-pine".to_string(),
                otter_key.clone()
            ),
            (
                "craft-agents:ws_f1x7e5a0/260805-calm-reed".to_string(),
                otter_key
            ),
        ]
    );

    // 认领 Claude 引擎的转录:首行的 sdkSessionId、锚点边车里更早的那份、隐藏会话的;
    // Pi 后端的 sdkSessionId 是 Pi 自己的 id,不认
    assert!(adapter.manages_claims());
    let claims = adapter.claimed_sessions().unwrap();
    assert!(claims
        .iter()
        .all(|(agent, _)| *agent == AgentId::ClaudeCode));
    assert_eq!(
        claims.iter().map(|(_, id)| id.as_str()).collect::<Vec<_>>(),
        [
            "c1a0de00-aaaa-4bbb-8ccc-000000000001",
            "c1a0de00-aaaa-4bbb-8ccc-000000000003",
            "c1a0de00-aaaa-4bbb-8ccc-000000000004",
            "c1a0de00-aaaa-4bbb-8ccc-000000000005",
            "c1a0de00-aaaa-4bbb-8ccc-00000000000a",
        ],
        "排好序、去过重(锚点边车里又出现了一次首行那份)"
    );
    // 快照事件:任何会话的首行(含隐藏会话)与回合锚点边车,别的文件不算
    assert!(adapter.is_snapshot_event(&craft_session_dir("260804-tiny-fern").join("session.jsonl")));
    assert!(adapter.is_snapshot_event(&otter.join("meta/claude-turn-anchors.json")));
    assert!(!adapter.is_snapshot_event(&otter.join("attachments/crash.png")));
}

/// 合成一条 craft 会话:首行 + 一句用户消息
fn write_craft_session(workspace: &Path, session: &str, header: serde_json::Value) {
    let dir = workspace.join("sessions").join(session);
    fs::create_dir_all(&dir).unwrap();
    let mut header = header;
    header["id"] = serde_json::json!(session);
    let line = serde_json::json!({
        "id": "m1", "type": "user", "content": "hello", "timestamp": 1786200000000i64
    });
    fs::write(dir.join("session.jsonl"), format!("{header}\n{line}\n")).unwrap();
}

#[test]
fn craft_keys_are_namespaced_by_the_workspace_id() {
    // 会话 id 只在工作区内唯一:两个同名文件夹的工作区同一天各生成一条同名会话,key 不能撞
    // (撞了会被 scanner 当成副本吞掉一条);命名空间取工作区 config.json 的 id,没有
    // config.json 或 id 缺席才退回目录名
    let tmp = tempfile::tempdir().unwrap();
    let a = tmp.path().join("a/notes");
    let b = tmp.path().join("b/notes");
    let bare = tmp.path().join("c/bare");
    let idless = tmp.path().join("d/idless");
    for ws in [&a, &b, &bare, &idless] {
        write_craft_session(ws, "260901-same-name", serde_json::json!({}));
    }
    fs::write(
        a.join("config.json"),
        r#"{"id":"ws_aaaa1111","slug":"notes"}"#,
    )
    .unwrap();
    fs::write(
        b.join("config.json"),
        r#"{"id":"ws_bbbb2222","slug":"notes"}"#,
    )
    .unwrap();
    fs::write(idless.join("config.json"), r#"{"slug":"idless"}"#).unwrap();
    let native_of = |ws: &Path| -> Vec<String> {
        CraftAdapter::new()
            .with_custom_root(ws.to_path_buf())
            .list_session_files()
            .unwrap()
            .into_iter()
            .map(|r| r.native_id)
            .collect()
    };
    assert_eq!(native_of(&a), ["ws_aaaa1111/260901-same-name"]);
    assert_eq!(native_of(&b), ["ws_bbbb2222/260901-same-name"]);
    assert_eq!(native_of(&bare), ["bare/260901-same-name"]);
    assert_eq!(native_of(&idless), ["idless/260901-same-name"]);

    // 父子关系两端都在同一个命名空间里拼
    write_craft_session(
        &a,
        "260902-child",
        serde_json::json!({"parentSessionId": "260901-same-name"}),
    );
    let adapter = CraftAdapter::new().with_custom_root(a.clone());
    assert_eq!(
        adapter.parent_links().unwrap(),
        [(
            "craft-agents:ws_aaaa1111/260902-child".to_string(),
            "craft-agents:ws_aaaa1111/260901-same-name".to_string()
        )]
    );
    // config.json 写坏了:沿用上次读到的 id,不退回目录名换 key
    fs::write(a.join("config.json"), "{ not json").unwrap();
    assert_eq!(
        adapter.list_session_files().unwrap()[0].native_id,
        "ws_aaaa1111/260901-same-name"
    );
    // 没读到过就不知道:这一刻不列,别拿目录名顶
    assert!(CraftAdapter::new()
        .with_custom_root(a)
        .list_session_files()
        .unwrap()
        .is_empty());
}

#[test]
fn craft_snapshots_tell_unreadable_from_absent() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    let claude = |n: u8| format!("c1a0de00-0000-4000-8000-0000000000{n:02}");
    write_craft_session(
        &ws,
        "260901-one",
        serde_json::json!({"sdkSessionId": claude(1)}),
    );
    write_craft_session(
        &ws,
        "260902-two",
        serde_json::json!({"sdkSessionId": claude(2)}),
    );
    let ids = |adapter: &dyn AgentAdapter| -> Option<Vec<String>> {
        Some(
            adapter
                .claimed_sessions()?
                .into_iter()
                .map(|(_, id)| id)
                .collect(),
        )
    };
    let adapter = CraftAdapter::new().with_custom_root(ws.clone());
    assert_eq!(ids(adapter.as_ref()).unwrap(), [claude(1), claude(2)]);

    // 首行写坏了(不是 JSON):craft 自己也列不出,当它没有——快照照样完整
    let two = ws.join("sessions/260902-two/session.jsonl");
    fs::write(&two, "{ torn\n").unwrap();
    assert_eq!(ids(adapter.as_ref()).unwrap(), [claude(1)]);
    // 原子写的空档(正本已删、.tmp 还没改名过来):沿用上次读到的,认领不撤
    let one = ws.join("sessions/260901-one/session.jsonl");
    let parked = one.with_extension("jsonl.tmp");
    fs::rename(&one, &parked).unwrap();
    assert_eq!(ids(adapter.as_ref()).unwrap(), [claude(1)]);
    fs::rename(&parked, &one).unwrap();

    // 读不出来(权限)又没读到过:整份快照交回 None,scanner 保留库里的认领与父子关系;
    // 读到过的沿用上次那份
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&one, fs::Permissions::from_mode(0o000)).unwrap();
        // root 跑测试时权限拦不住,这一段没有意义
        if fs::File::open(&one).is_err() {
            assert_eq!(ids(adapter.as_ref()).unwrap(), [claude(1)]);
            let fresh = CraftAdapter::new().with_custom_root(ws.clone());
            assert!(fresh.claimed_sessions().is_none());
            assert!(fresh.parent_links().is_none());
            assert!(
                fresh.list_session_files().unwrap().is_empty(),
                "没读到过首行的会话不列:分不清它是不是隐藏会话"
            );
        }
        fs::set_permissions(&one, fs::Permissions::from_mode(0o644)).unwrap();
    }
}

#[test]
fn craft_custom_root_lifts_a_chosen_sessions_dir_to_its_workspace() {
    let tmp = tempfile::tempdir().unwrap();
    let workspace = tmp.path().join("Notes");
    fs::create_dir_all(workspace.join("sessions")).unwrap();
    let normalize =
        |dir: PathBuf| wake_core::adapters::normalize_custom_root(AgentId::CraftAgents, dir);
    assert_eq!(normalize(workspace.join("sessions")), workspace);
    assert_eq!(normalize(workspace.clone()), workspace);
    // 一个恰好叫 sessions 的工作区(底下还有自己的 sessions/)不上提
    let odd = tmp.path().join("sessions");
    fs::create_dir_all(odd.join("sessions")).unwrap();
    assert_eq!(normalize(odd.clone()), odd);
}

/// 旧版转录取假 HOME 里 stage_sidecars 拷入的那份(file_ref 只认自己根下的路径)
fn openclaw_legacy_ref(env: &TestEnv) -> SessionFileRef {
    let sessions = env
        .openclaw_db
        .parent()
        .and_then(Path::parent)
        .expect("openclaw agent dir")
        .join("sessions");
    fs_ref(
        AgentId::Openclaw,
        &sessions.join("cccccccc-aaaa-bbbb-cccc-000000000017.jsonl"),
        "cccccccc-aaaa-bbbb-cccc-000000000017",
    )
}

/// ZCode:cli/db/db.sqlite 是 OpenCode 形状的转录源,v2/tasks-index.sqlite 只借
/// deleted / migration_source 两个过滤位;parent_id 非空不列;semantics.origin
/// 白名单归 Meta、缺席放行;title_source=default 是占位;model 逐消息、token 按调用累加
#[test]
fn zcode_parse_contract() {
    let env = setup();
    let adapter = ZcodeAdapter::new();
    let mut ids: Vec<String> = adapter
        .list_session_files()
        .expect("zcode list")
        .into_iter()
        .map(|r| r.native_id)
        .collect();
    ids.sort();
    // zc-0003 桌面端软删、zc-0004 是向导从 Claude Code 导入的、zc-0005 是
    // subagent_child;zc-0008 是 fork——带 parent_id 但是用户自己的对话,要列
    assert_eq!(
        ids,
        vec!["zc-0001", "zc-0002", "zc-0006", "zc-0007", "zc-0008"]
    );

    let r = db_ref(AgentId::Zcode, &env.zcode_db, "zc-0001");
    let s = adapter.parse_session(&r).expect("zcode parse_session");
    let t = adapter
        .parse_transcript(&r)
        .expect("zcode parse_transcript");
    assert_eq!(s.meta.key, "zcode:zc-0001");
    assert_eq!(s.meta.title, "ZCode QR fix");
    assert_eq!(s.meta.project_path, "/Users/tester/Github/wakefx");
    // 中途换过模型,取最后用的;token 按调用累加(120 + 30)
    assert_eq!(s.meta.model.as_deref(), Some("GLM-5.3-Flash"));
    assert_eq!(s.meta.tokens_used, Some(150));
    assert_eq!(s.meta.created_at, 1789000000000);
    assert_eq!(s.meta.updated_at, 1789000060000);
    assert_eq!(s.meta.message_count, 4);
    assert!(!s.meta.archived);
    assert_eq!(
        roles_kinds(&t.mainline),
        vec![
            (Role::User, MessageKind::Text),
            (Role::Assistant, MessageKind::Text),
            (Role::User, MessageKind::Text),
            (Role::Assistant, MessageKind::Text),
            // compaction 摘要:user 角色、hidden、kind=compact_summary → 折成一条
            (Role::User, MessageKind::CompactSummary),
        ]
    );
    let a = &t.mainline[1];
    assert_eq!(a.text, "是依赖数组问题,我给出了修复补丁。");
    assert_eq!(a.thinking.as_deref(), Some("先读一下组件源码"));
    assert_eq!(a.model.as_deref(), Some("GLM-5.3"));
    assert_eq!(a.timestamp, Some(1789000005000));
    assert_eq!(a.tool_calls.len(), 1);
    assert_eq!(a.tool_calls[0].name, "Bash");
    assert!(a.tool_calls[0].input_preview.contains("QrScanner"));
    assert_eq!(
        a.tool_calls[0].output.as_deref(),
        Some("src/QrScanner.tsx:12: useEffect(() => {")
    );
    assert!(!a.tool_calls[0].is_error);
    assert_eq!(t.mainline[3].model.as_deref(), Some("GLM-5.3-Flash"));
    // seq 契约:FTS 单元的 seq 等于详情页序号(Meta / CompactSummary 不进 FTS)
    assert_eq!(
        s.units.iter().map(|u| u.seq).collect::<Vec<_>>(),
        t.mainline
            .iter()
            .filter(|m| m.kind == MessageKind::Text)
            .map(|m| m.seq)
            .collect::<Vec<_>>()
    );

    // 占位标题退回首条真人消息;注入上下文归 Meta、不计数
    let r2 = db_ref(AgentId::Zcode, &env.zcode_db, "zc-0002");
    let s2 = adapter.parse_session(&r2).unwrap();
    let t2 = adapter.parse_transcript(&r2).unwrap();
    assert_eq!(s2.meta.title, "空标题会话取这句");
    assert_eq!(s2.meta.message_count, 2);
    assert_eq!(
        roles_kinds(&t2.mainline),
        vec![
            (Role::User, MessageKind::Meta),
            (Role::User, MessageKind::Text),
            (Role::Assistant, MessageKind::Text),
        ]
    );

    // 老写端没有 semantics:按真人放行,不能整条会话归 Meta
    let r6 = db_ref(AgentId::Zcode, &env.zcode_db, "zc-0006");
    let s6 = adapter.parse_session(&r6).unwrap();
    assert_eq!(s6.meta.title, "老写端没有 semantics");
    assert_eq!(s6.meta.message_count, 2);

    let r7 = db_ref(AgentId::Zcode, &env.zcode_db, "zc-0007");
    assert!(adapter.parse_session(&r7).unwrap().meta.archived);
}

/// 老库缺列(title_source / task_type / sequence 是 ALTER 追加的;parent_id 在
/// 初始 schema 里,老库退回 parent_id IS NULL)与没装桌面端(没有 tasks-index)
/// 都不能让整家消失
#[test]
fn zcode_degrades_on_old_schema_and_missing_task_index() {
    setup();
    let home = tempfile::tempdir().unwrap();
    let db_dir = home.path().join(".zcode/cli/db");
    fs::create_dir_all(&db_dir).unwrap();
    let conn = rusqlite::Connection::open(db_dir.join("db.sqlite")).unwrap();
    conn.execute_batch(
        r#"
        CREATE TABLE session (id TEXT PRIMARY KEY, project_id TEXT, parent_id TEXT, directory TEXT,
                              title TEXT, version TEXT, time_created INTEGER, time_updated INTEGER,
                              time_archived INTEGER);
        CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER,
                              time_updated INTEGER, data TEXT);
        CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT, session_id TEXT,
                           time_created INTEGER, time_updated INTEGER, data TEXT);
        INSERT INTO session VALUES ('old-1','p',NULL,'/work/old','old schema','0.15.2',1789000000000,1789000001000,NULL);
        INSERT INTO session VALUES ('old-2','p','old-1','/work/old','old child','0.15.2',1789000002000,1789000003000,NULL);
        INSERT INTO message VALUES ('m1','old-1',1789000000000,1789000000000,'{"role":"user","time":{"created":1789000000000}}');
        INSERT INTO part VALUES ('p1','m1','old-1',1789000000000,1789000000000,'{"type":"text","text":"老库也要能读"}');
        "#,
    )
    .unwrap();
    drop(conn);

    let adapter = ZcodeAdapter::new().with_custom_root(home.path().join(".zcode"));
    let refs = adapter.list_session_files().unwrap();
    assert_eq!(
        refs.len(),
        1,
        "老 schema 不得整家消失;没有 task_type 时子会话按 parent_id 不列"
    );
    let s = adapter.parse_session(&refs[0]).unwrap();
    assert_eq!(s.meta.title, "old schema");
    assert_eq!(s.meta.project_path, "/work/old");
    assert_eq!(s.meta.message_count, 1);
    assert!(!s.meta.archived);
}

/// 自定义 location:构造器只认 home 与孤立的库文件两种形状,且只按路径整形不看
/// 存在性(远程 mount 契约);中间层 cli/、cli/db/ 由 normalize_custom_root 在入库
/// 前按纯路径形状上提到 home——父链不长那个样的库拷贝原样保留、tasks-index 也
/// 不去别处找
#[test]
fn zcode_custom_root_lifts_to_home_before_storing() {
    setup();
    let home = Path::new("/nowhere/.zcode");
    let expect = home.join("cli/db/db.sqlite");
    for dir in [
        home.to_path_buf(),
        home.join("cli"),
        home.join("cli/db"),
        expect.clone(),
    ] {
        let stored = wake_core::adapters::normalize_custom_root(AgentId::Zcode, dir.clone());
        assert_eq!(stored, home, "{}", dir.display());
    }
    let adapter = ZcodeAdapter::new().with_custom_root(home.to_path_buf());
    assert_eq!(adapter.data_roots(), vec![expect]);
    let lone = PathBuf::from("/backup/db.sqlite");
    assert_eq!(
        wake_core::adapters::normalize_custom_root(AgentId::Zcode, lone.clone()),
        lone
    );
    let adapter = ZcodeAdapter::new().with_custom_root(lone.clone());
    assert_eq!(adapter.data_roots(), vec![lone]);
}

/// Devin:`cli/sessions.db` 单库两表——sessions + message_nodes 森林。hidden=1
/// 与零正文会话不列;可见转录是从 main_chain_id 叶子沿 parent_node_id 走回
/// 根的链,重试侧枝不进转录、其 metrics 不进 token 累计;消息级
/// generation_model 比 sessions.model 权威;system 注入、心跳与 compaction
/// 请求归 Meta,<summary> 应答折 CompactSummary
#[test]
fn devin_parse_contract() {
    let env = setup();
    let adapter = DevinAdapter::new();
    let mut ids: Vec<String> = adapter
        .list_session_files()
        .expect("devin list")
        .into_iter()
        .map(|r| r.native_id)
        .collect();
    ids.sort();
    // dv-0003 是 hidden 会话、dv-0004 零正文,都不列
    assert_eq!(ids, vec!["dv-0001", "dv-0002"]);
    // 虚拟路径 <db>#<id>
    assert_eq!(
        adapter.list_session_files().unwrap()[0].file_path,
        format!("{}#dv-0001", env.devin_db.display())
    );

    let r = db_ref(AgentId::Devin, &env.devin_db, "dv-0001");
    let s = adapter.parse_session(&r).expect("devin parse_session");
    let t = adapter
        .parse_transcript(&r)
        .expect("devin parse_transcript");
    assert_eq!(s.meta.key, "devin:dv-0001");
    assert_eq!(s.meta.title, "Devin QR fix");
    assert_eq!(s.meta.project_path, "/Users/tester/Github/wakefx");
    assert_eq!(s.meta.project_name, "wakefx");
    // 逐消息 generation_model,取最后一条 assistant 的实发模型
    assert_eq!(s.meta.model.as_deref(), Some("swe-2-max"));
    // 主链 (200+20+800) + (300+40);重试侧枝 n5 的 550 不进账
    assert_eq!(s.meta.tokens_used, Some(1360));
    assert_eq!(s.meta.created_at, 1789000000000);
    assert_eq!(s.meta.updated_at, 1789000060000);
    assert_eq!(s.meta.message_count, 3);
    assert_eq!(
        roles_kinds(&t.mainline),
        vec![
            (Role::User, MessageKind::Text),
            (Role::Assistant, MessageKind::Text),
            (Role::Assistant, MessageKind::Text),
        ]
    );
    assert!(!t.mainline.iter().any(|m| m.text.contains("重试侧枝")));
    let a = &t.mainline[1];
    assert_eq!(a.thinking.as_deref(), Some("先读一下组件源码"));
    assert_eq!(a.model.as_deref(), Some("swe-2-high"));
    assert_eq!(a.timestamp, Some(1789000008000));
    assert_eq!(a.tool_calls.len(), 1);
    assert_eq!(a.tool_calls[0].name, "exec");
    assert!(a.tool_calls[0].input_preview.contains("rg useEffect"));
    assert_eq!(
        a.tool_calls[0].output.as_deref(),
        Some("src/QrScanner.tsx:12: useEffect(() => watch())")
    );
    // seq 契约:FTS 单元的 seq 等于详情页序号(Meta / CompactSummary 不进 FTS)
    assert_eq!(
        s.units.iter().map(|u| u.seq).collect::<Vec<_>>(),
        t.mainline
            .iter()
            .filter(|m| m.kind == MessageKind::Text)
            .map(|m| m.seq)
            .collect::<Vec<_>>()
    );

    // 空标题回退首条真人消息;system 注入、心跳、compaction 请求归 Meta,
    // <summary> 应答折 CompactSummary
    let r2 = db_ref(AgentId::Devin, &env.devin_db, "dv-0002");
    let s2 = adapter.parse_session(&r2).unwrap();
    let t2 = adapter.parse_transcript(&r2).unwrap();
    assert_eq!(s2.meta.title, "空标题会话取这句");
    assert_eq!(s2.meta.message_count, 2);
    assert_eq!(
        roles_kinds(&t2.mainline),
        vec![
            (Role::System, MessageKind::Meta),
            (Role::User, MessageKind::Meta),
            (Role::User, MessageKind::Meta),
            (Role::Assistant, MessageKind::CompactSummary),
            (Role::User, MessageKind::Text),
            (Role::Assistant, MessageKind::Text),
        ]
    );
}

/// 老库没有 hidden / main_chain_id 两列:照常列(无 hidden 不过滤),链退回
/// 全部节点(无 main_chain_id 就按 created_at,row_id 全列)
#[test]
fn devin_degrades_on_old_schema() {
    setup();
    let home = tempfile::tempdir().unwrap();
    let db_dir = home.path().join("cli");
    fs::create_dir_all(&db_dir).unwrap();
    let conn = rusqlite::Connection::open(db_dir.join("sessions.db")).unwrap();
    conn.execute_batch(
        r#"
        CREATE TABLE sessions (id TEXT PRIMARY KEY, working_directory TEXT NOT NULL,
                               backend_type TEXT NOT NULL, model TEXT NOT NULL,
                               agent_mode TEXT NOT NULL, created_at INTEGER NOT NULL,
                               last_activity_at INTEGER NOT NULL, title TEXT);
        CREATE TABLE message_nodes (row_id INTEGER PRIMARY KEY, session_id TEXT NOT NULL,
                                    node_id INTEGER NOT NULL, parent_node_id INTEGER,
                                    chat_message TEXT NOT NULL, created_at INTEGER NOT NULL);
        INSERT INTO sessions VALUES ('old-1','/work/old','windsurf','swe-1','auto',1789000000,1789000001,'old schema');
        INSERT INTO message_nodes (session_id, node_id, parent_node_id, chat_message, created_at) VALUES
            ('old-1',1,NULL,'{"role":"user","content":"老库也要能读"}',1789000000),
            ('old-1',2,1,'{"role":"assistant","content":"在。","metadata":{"generation_model":"swe-1","metrics":{"input_tokens":10,"output_tokens":5}}}',1789000001);
        "#,
    )
    .unwrap();
    drop(conn);

    let adapter = DevinAdapter::new().with_custom_root(home.path().to_path_buf());
    let refs = adapter.list_session_files().unwrap();
    assert_eq!(refs.len(), 1, "老 schema 不得整家消失");
    let s = adapter.parse_session(&refs[0]).unwrap();
    assert_eq!(s.meta.title, "old schema");
    assert_eq!(s.meta.project_path, "/work/old");
    assert_eq!(s.meta.message_count, 2);
    assert_eq!(s.meta.tokens_used, Some(15));
}

/// 主链走不到根(叶子的父节点不在库里)就退回全部节点,不给半截链;解不开的
/// chat_message 与词汇表外的 role 计进 unknown(格式漂移的金丝雀)
#[test]
fn devin_broken_chain_falls_back_and_counts_unknown_nodes() {
    setup();
    let home = tempfile::tempdir().unwrap();
    let db_dir = home.path().join("cli");
    fs::create_dir_all(&db_dir).unwrap();
    let conn = rusqlite::Connection::open(db_dir.join("sessions.db")).unwrap();
    conn.execute_batch(
        r#"
        CREATE TABLE sessions (id TEXT PRIMARY KEY, working_directory TEXT NOT NULL,
                               model TEXT NOT NULL, created_at INTEGER NOT NULL,
                               last_activity_at INTEGER NOT NULL, title TEXT,
                               main_chain_id INTEGER, hidden INTEGER NOT NULL DEFAULT 0);
        CREATE TABLE message_nodes (row_id INTEGER PRIMARY KEY, session_id TEXT NOT NULL,
                                    node_id INTEGER NOT NULL, parent_node_id INTEGER,
                                    chat_message TEXT NOT NULL, created_at INTEGER NOT NULL);
        INSERT INTO sessions VALUES ('gap-1','/work/gap','swe-2',1789000000,1789000005,'gap',5,0);
        INSERT INTO message_nodes (session_id, node_id, parent_node_id, chat_message, created_at) VALUES
            ('gap-1',1,NULL,'{"role":"user","content":"第一句"}',1789000001),
            ('gap-1',2,1,'{"role":"assistant","content":"第一句的回答"}',1789000002),
            ('gap-1',3,2,'{"role":"developer","content":"没见过的角色"}',1789000003),
            ('gap-1',4,3,'{ torn',1789000004),
            ('gap-1',5,99,'{"role":"assistant","content":"父节点 99 不在库里"}',1789000005);
        "#,
    )
    .unwrap();
    drop(conn);

    let adapter = DevinAdapter::new().with_custom_root(home.path().to_path_buf());
    let refs = adapter.list_session_files().unwrap();
    let t = adapter.parse_transcript(&refs[0]).unwrap();
    assert_eq!(
        t.mainline
            .iter()
            .map(|m| m.text.as_str())
            .collect::<Vec<_>>(),
        ["第一句", "第一句的回答", "父节点 99 不在库里"],
        "只剩叶子那一句就把前面的对话丢了"
    );
    assert_eq!(t.unknown_line_count, 2, "没见过的 role + 写坏的 JSON");
    assert_eq!(
        adapter.parse_session(&refs[0]).unwrap().unknown_line_count,
        2
    );
}

/// 自定义 location:`cli/sessions.db` 与 `cli` 层在入库前上提到数据根
/// (normalize_custom_root),孤立库拷贝原样进构造器
#[test]
fn devin_custom_root_normalization() {
    setup();
    let root = Path::new("/nowhere/devin-data");
    for (picked, stored) in [
        (root.join("cli/sessions.db"), root.to_path_buf()),
        (root.join("cli"), root.to_path_buf()),
        (root.to_path_buf(), root.to_path_buf()),
    ] {
        assert_eq!(
            wake_core::adapters::normalize_custom_root(AgentId::Devin, picked),
            stored
        );
    }
    let adapter = DevinAdapter::new().with_custom_root(root.to_path_buf());
    assert_eq!(adapter.data_roots(), vec![root.join("cli/sessions.db")]);
    let lone = PathBuf::from("/backup/sessions.db");
    let adapter = DevinAdapter::new().with_custom_root(lone.clone());
    assert_eq!(adapter.data_roots(), vec![lone]);
}

#[test]
fn hermes_parse_contract() {
    let env = setup();
    let adapter = HermesAdapter::new();
    // 枚举:tool 内部会话与零消息会话不列
    let mut ids: Vec<String> = adapter
        .list_session_files()
        .expect("hermes list")
        .into_iter()
        .map(|r| r.native_id)
        .collect();
    ids.sort();
    assert_eq!(ids, vec!["hs-0001", "hs-0002", "hs-0005"]);
    // /branch 分支挂到父会话下(parent_session_id),不当顶层
    assert!(adapter.manages_parent_links());
    assert_eq!(
        adapter.parent_links().unwrap(),
        vec![("hermes:hs-0005".to_string(), "hermes:hs-0001".to_string())]
    );

    let r = db_ref(AgentId::Hermes, &env.hermes_db, "hs-0001");
    let s = adapter.parse_session(&r).expect("hermes parse_session");
    let t = adapter
        .parse_transcript(&r)
        .expect("hermes parse_transcript");
    assert_eq!(s.meta.key, "hermes:hs-0001");
    assert_eq!(s.meta.title, "Hermes QR fix"); // 库内标题优先
    assert_eq!(s.meta.model.as_deref(), Some("gpt-5.4"));
    assert_eq!(s.meta.tokens_used, Some(450));
    assert_eq!(s.meta.source, None); // cli 不打徽章
    assert_eq!(s.meta.project_path, ""); // 库里没有 cwd
    assert_eq!(s.meta.created_at, ms("2026-08-08T07:00:00Z"));
    assert_eq!(s.meta.updated_at, ms("2026-08-08T07:02:00Z"));
    assert_eq!(s.meta.message_count, 3);
    assert!(s.meta.file_path.ends_with("#hs-0001"));
    // assistant(tool_calls)→ tool → assistant(text) 合并成一条
    assert_eq!(
        roles_kinds(&t.mainline),
        vec![
            (Role::User, MessageKind::Text),
            (Role::Assistant, MessageKind::Text),
            (Role::User, MessageKind::Text),
        ]
    );
    let a = &t.mainline[1];
    assert_eq!(a.text, "是依赖数组问题,我给出了修复补丁。");
    assert_eq!(a.thinking.as_deref(), Some("先读一下组件源码"));
    assert_eq!(a.timestamp, Some(ms("2026-08-08T07:00:08Z")));
    assert_eq!(a.tool_calls.len(), 1);
    assert_eq!(a.tool_calls[0].name, "read_file");
    // arguments 是 JSON 字符串,解开后预览才有路径
    assert!(a.tool_calls[0].input_preview.contains("QrScanner"));
    assert_eq!(
        a.tool_calls[0].output.as_deref(),
        Some("useEffect(() => watch())")
    );
    assert_eq!(
        s.units.iter().map(|u| u.seq).collect::<Vec<_>>(),
        vec![0, 1, 2]
    );

    // 无标题 → 首条用户消息;telegram 启动面成徽章;精简形状 tool_calls 无 id
    // 按 tool_name 顺位回填;多模态 content 数组只取文本(图片退占位)
    let r2 = db_ref(AgentId::Hermes, &env.hermes_db, "hs-0002");
    let s2 = adapter.parse_session(&r2).expect("hermes fallback parse");
    let t2 = adapter
        .parse_transcript(&r2)
        .expect("hermes fallback transcript");
    assert_eq!(s2.meta.title, "无标题会话的兜底标题应取这句");
    assert_eq!(s2.meta.source.as_deref(), Some("telegram"));
    assert_eq!(s2.meta.tokens_used, None);
    assert_eq!(s2.meta.updated_at, ms("2026-08-08T08:00:08Z")); // 无 ended_at 取末条消息
    let a2 = &t2.mainline[1];
    assert_eq!(a2.tool_calls.len(), 1);
    assert_eq!(a2.tool_calls[0].name, "terminal");
    assert_eq!(a2.tool_calls[0].output.as_deref(), Some("README.md"));
    assert_eq!(a2.text, "好的。");

    // 自定义 location:选 Hermes home(连同 profiles/*)或库文件本身都认
    let home = env.hermes_db.parent().unwrap().to_path_buf();
    let profile_db = home.join("profiles").join("coder").join("state.db");
    fs::create_dir_all(profile_db.parent().unwrap()).unwrap();
    common::build_hermes_db(&profile_db);
    let rooted = HermesAdapter::new().with_custom_root(home.clone());
    assert_eq!(
        rooted.data_roots(),
        vec![env.hermes_db.clone(), profile_db.clone()]
    );
    let direct = HermesAdapter::new().with_custom_root(env.hermes_db.clone());
    assert_eq!(direct.data_roots(), vec![env.hermes_db.clone()]);
    // resume 必须带所属档案:主库 = default,profiles/<name> = name(虚拟路径同样认)
    use wake_core::adapters::hermes::profile_of;
    assert_eq!(
        profile_of(&format!("{}#hs-0001", env.hermes_db.display())),
        "default"
    );
    assert_eq!(
        profile_of(&format!("{}#hs-0001", profile_db.display())),
        "coder"
    );
    fs::remove_dir_all(home.join("profiles")).unwrap();
}

/// 没被新版 Hermes 迁移过的库(schema v2:无 title、无 cache/reasoning 列、
/// messages 无 reasoning)必须照常枚举与解析,不能整家消失
#[test]
fn hermes_legacy_schema_still_parses() {
    setup();
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("state.db");
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute_batch(
        r#"
        CREATE TABLE sessions (
            id TEXT PRIMARY KEY, source TEXT NOT NULL, model TEXT, parent_session_id TEXT,
            started_at REAL NOT NULL, ended_at REAL, message_count INTEGER DEFAULT 0,
            input_tokens INTEGER DEFAULT 0, output_tokens INTEGER DEFAULT 0
        );
        CREATE TABLE messages (
            id INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT NOT NULL, role TEXT NOT NULL,
            content TEXT, tool_call_id TEXT, tool_calls TEXT, tool_name TEXT,
            timestamp REAL NOT NULL, finish_reason TEXT
        );
        INSERT INTO sessions VALUES ('old-1', 'cli', 'gpt-5.4', NULL, 1786172400.0, NULL, 2, 10, 5);
        INSERT INTO messages (session_id, role, content, timestamp) VALUES
            ('old-1', 'user', '老库里的会话', 1786172401.0),
            ('old-1', 'assistant', '仍要能解析', 1786172402.0);
        "#,
    )
    .unwrap();
    drop(conn);
    let adapter = HermesAdapter::new().with_custom_root(db.clone());
    let refs = adapter.list_session_files().expect("legacy list");
    assert_eq!(refs.len(), 1);
    let s = adapter.parse_session(&refs[0]).expect("legacy parse");
    assert_eq!(s.meta.title, "老库里的会话");
    assert_eq!(s.meta.tokens_used, Some(15));
    assert_eq!(s.meta.message_count, 2);
}

#[test]
fn openclaw_parse_contract() {
    let env = setup();
    let adapter = OpenclawAdapter::new();
    // 枚举:库里的活跃窗口 + reset 前旧窗口 + 旧版 jsonl;子代理(spawned_by /
    // sessions.json 的 spawnedBy)与 checkpoint 快照都不列
    let mut ids: Vec<String> = adapter
        .list_session_files()
        .expect("openclaw list")
        .into_iter()
        .map(|r| r.native_id)
        .collect();
    ids.sort();
    assert_eq!(
        ids,
        vec![
            "cccccccc-aaaa-bbbb-cccc-000000000017",
            "claw-0001",
            "claw-0003"
        ]
    );

    // —— 现版 SQLite:active_events 给出可见分支,死分支 a2-dead 不出现
    let r = db_ref(AgentId::Openclaw, &env.openclaw_db, "claw-0001");
    let s = adapter.parse_session(&r).expect("openclaw parse_session");
    let t = adapter
        .parse_transcript(&r)
        .expect("openclaw parse_transcript");
    assert_eq!(s.meta.key, "openclaw:claw-0001");
    assert_eq!(s.meta.title, "Node label"); // 无 session_info 时取 node label
    assert_eq!(s.meta.project_path, "/Users/tester/Github/wakefx"); // header cwd
    assert_eq!(s.meta.model.as_deref(), Some("claude-opus-5"));
    assert_eq!(s.meta.tokens_used, Some(7100 + 7300)); // 活跃分支两次调用之和
    assert_eq!(t.meta.tokens_used, s.meta.tokens_used);
    assert_eq!(s.meta.source.as_deref(), Some("telegram"));
    assert_eq!(s.meta.created_at, ms("2026-08-08T09:00:00Z"));
    assert_eq!(s.meta.updated_at, ms("2026-08-08T09:00:30Z"));
    assert_eq!(s.meta.message_count, 3);
    assert_eq!(s.unknown_line_count, 0);
    assert_eq!(
        roles_kinds(&t.mainline),
        vec![
            (Role::User, MessageKind::Text),
            (Role::Assistant, MessageKind::Text),
            (Role::User, MessageKind::Text),
        ]
    );
    let a = &t.mainline[1];
    assert_eq!(a.text, "找到泄漏点,已补清理回调。");
    assert_eq!(a.tool_calls.len(), 1);
    assert_eq!(a.tool_calls[0].name, "exec");
    assert!(a.tool_calls[0].is_error);
    assert!(a.tool_calls[0]
        .output
        .as_deref()
        .unwrap_or_default()
        .contains("QrScanner"));
    assert_eq!(
        s.units.iter().map(|u| u.seq).collect::<Vec<_>>(),
        vec![0, 1, 2]
    );

    // 旧窗口:无 active_events 退回树回溯;node 的 totalTokens 不归它
    let r3 = db_ref(AgentId::Openclaw, &env.openclaw_db, "claw-0003");
    let s3 = adapter.parse_session(&r3).expect("openclaw old window");
    assert_eq!(s3.meta.title, "Node label");
    assert_eq!(s3.meta.model.as_deref(), Some("gpt-5.5"));
    assert_eq!(s3.meta.tokens_used, None);
    assert_eq!(s3.meta.source, None); // cli 不打徽章
    assert_eq!(s3.meta.message_count, 2);

    // —— 旧版 jsonl:file_ref 只认 agents/<id>/sessions/ 下的活转录
    let legacy = openclaw_legacy_ref(env);
    let path = Path::new(&legacy.file_path);
    let by_ref = adapter.file_ref(path).expect("openclaw file_ref");
    assert_eq!(by_ref.native_id, "cccccccc-aaaa-bbbb-cccc-000000000017");
    let checkpoint = path.with_file_name("cccccccc-aaaa-bbbb-cccc-000000000017.checkpoint.1.jsonl");
    assert!(adapter.file_ref(&checkpoint).is_none());
    assert!(adapter
        .file_ref(&path.parent().unwrap().join("sessions.json"))
        .is_none());

    let s = adapter
        .parse_session(&legacy)
        .expect("openclaw legacy parse_session");
    let t = adapter
        .parse_transcript(&legacy)
        .expect("openclaw legacy parse_transcript");
    assert_eq!(s.meta.title, "OpenClaw QR cleanup"); // session_info 压过 sessions.json 的 label
    assert_eq!(s.meta.project_path, "/Users/tester/Github/wakefx");
    assert_eq!(s.meta.model.as_deref(), Some("claude-opus-5"));
    assert_eq!(s.meta.tokens_used, Some(5100 + 5200));
    assert_eq!(t.meta.tokens_used, s.meta.tokens_used);
    assert_eq!(s.meta.created_at, ms("2026-08-07T09:00:00Z"));
    assert_eq!(s.meta.updated_at, ms("2026-08-07T09:00:30Z"));
    // wibble-entry 是叶:回溯经 u3→c1→a2→r1→a1→u2→u1→i1→m1,死分支 a2-dead 不在链上;
    // 未知类型计 1
    assert_eq!(s.unknown_line_count, 1);
    assert_eq!(
        roles_kinds(&t.mainline),
        vec![
            (Role::User, MessageKind::Meta), // runtimeContextCarrier
            (Role::User, MessageKind::Text),
            (Role::Assistant, MessageKind::Text),
            (Role::System, MessageKind::CompactSummary),
            (Role::User, MessageKind::Text),
        ]
    );
    assert_eq!(s.meta.message_count, 3);
    let a = &t.mainline[2];
    assert_eq!(a.text, "找到泄漏点,已补清理回调。");
    assert_eq!(a.thinking.as_deref(), Some("先搜一下 useEffect"));
    assert_eq!(a.tool_calls.len(), 1);
    assert!(!a.tool_calls[0].is_error);
    assert!(t.mainline[3].text.contains("useEffect 泄漏"));

    // OpenClaw 没有 resume 形制:详情页不该给任何 Open In 目标
    assert!(wake_core::services::terminal::resume_targets(&s.meta).is_empty());

    // 自定义 location:状态目录、agents 目录都整形到 agents 层
    let state = env
        .openclaw_db
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    let agents = state.join("agents");
    assert_eq!(
        OpenclawAdapter::new()
            .with_custom_root(state.to_path_buf())
            .data_roots(),
        vec![agents.clone()]
    );
    assert_eq!(
        OpenclawAdapter::new()
            .with_custom_root(agents.clone())
            .data_roots(),
        vec![agents]
    );
}

// ---------------------------------------------------------------- quickMeta 合并

fn mk_meta(agent: AgentId, key: &str, id: &str, title: &str, source: Option<&str>) -> SessionMeta {
    SessionMeta {
        host: String::new(),
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
    // 一家可以有多个数据源(Cursor 的 CLI + IDE),集合比较前先去重——
    // 这里要卡的是"某家整个漏出 roster 或漏出 ALL",不是实例个数
    roster.dedup();
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
    assert_eq!(roster.len(), DEFAULT_INSTANCES - 1);
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

/// 注入模板出现在 role=user 消息里时归 Meta。fixture 3 是从父会话分出来的
/// **用户**线程(forked_from_id、字符串 source),走得到文件边界——guardian
/// 形态已在 codex_internal_threads_are_excluded_at_the_file_boundary 里覆盖,
/// 这里不能再用一个枚举不到的文件当被测对象
#[test]
fn codex_branch_transcript_injection_is_meta() {
    setup();
    let adapter = CodexAdapter::new();
    let r = codex_branch_ref();
    assert!(
        adapter.file_ref(Path::new(&r.file_path)).is_some(),
        "用户分支线程在文件边界可见"
    );
    let t = adapter
        .parse_transcript(&r)
        .expect("codex branch parse_transcript");

    // 父会话的整段 transcript 被打包成一条 role=user 的消息喂进来,里面含
    // 父会话的 assistant 输出。不识别的话,父会话里 AI 说的话会显示成这个
    // 会话里用户发的
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
        .expect("codex branch parse_session");
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
    let defaults = DEFAULT_INSTANCES;
    assert_eq!(roster.len(), defaults + 1, "全量默认 + 1 自定义");
    assert_eq!(roster[defaults].agent(), AgentId::ClaudeCode);

    let under = format!("{}/projects/p/x.jsonl", custom.display());
    assert_eq!(
        wake_core::adapters::adapter_ix_for(&roster, AgentId::ClaudeCode, &under),
        Some(defaults),
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

// ---------------------------------------------------------------- 远程装饰器

/// 远程实例 = 各家 with_custom_root(缓存内挂载点) 包 RemoteAdapter。
/// 断言两件事:①挂载点整形在**目录不存在时**也不越界——十四家的数据根
/// 必须全部落在缓存树内,落回真实 home 就会把本地会话错标成远程;
/// ②key/host 在解析出口统一改写,native id 保持纯净(resume 用)。
#[test]
fn remote_adapters_stay_inside_cache_and_rewrite_keys() {
    let env = setup();

    // ① 空缓存目录(尚未同步)——整形判据全走"不存在"分支。
    // 模板由调用方传入(roster 唯一构造点的契约,生产侧同式)
    let templates = wake_core::adapters::create_adapters();
    let empty = tempfile::tempdir().unwrap();
    let adapters =
        wake_core::adapters::remote::create_remote_adapters(&templates, "devbox", empty.path());
    assert_eq!(adapters.len(), AgentId::ALL.len(), "每家一个远程实例");
    let cache_prefix = empty.path().to_string_lossy().to_string();
    for adapter in &adapters {
        assert_eq!(adapter.host(), "devbox");
        for root in adapter.data_roots() {
            let root = root.to_string_lossy();
            assert!(
                root.starts_with(&cache_prefix),
                "{:?} 的远程数据根越界: {root}",
                adapter.agent()
            );
        }
        // 缺根必须降级为空枚举,不能 Err 截断整轮扫描(契约同默认实例)
        assert!(adapter
            .list_session_files()
            .expect("degrade to empty")
            .is_empty());
    }

    // ② 假 HOME 本身就是"远程 home 镜像"的形状——直接当缓存用,
    // SQLite 型/侧档型(copilot/opencode/antigravity/kimi/dsh)都有数据
    let home = env._home.path();
    let adapters = wake_core::adapters::remote::create_remote_adapters(&templates, "devbox", home);
    let by_agent = |a: AgentId| {
        adapters
            .iter()
            .find(|x| x.agent() == a)
            .expect("remote instance")
    };

    let copilot = by_agent(AgentId::Copilot);
    let refs = copilot.list_session_files().expect("copilot remote list");
    assert_eq!(refs.len(), 2);
    let r = refs.iter().find(|r| r.native_id == "cop-0001").unwrap();
    let parsed = copilot.parse_session(r).expect("copilot remote parse");
    assert_eq!(parsed.meta.key, "copilot:devbox:cop-0001");
    assert_eq!(parsed.meta.host, "devbox");
    assert_eq!(parsed.meta.id, "cop-0001", "native id 不得带 host 段");

    // dsh 的 file_ref 要读首行(zstd 首帧)——装饰器转发后行为不变,
    // 解析出口再打 host 标
    let dsh = by_agent(AgentId::Dsh);
    let r = dsh
        .file_ref(&env.dsh_log)
        .expect("dsh remote file_ref reads header");
    let parsed = dsh.parse_session(&r).expect("dsh remote parse");
    assert_eq!(parsed.meta.key, "dsh:devbox:dsh-e2e4-0001");
    assert_eq!(parsed.meta.host, "devbox");

    // transcript 出口同样改写(详情页/导出消费)
    let t = dsh.parse_transcript(&r).expect("dsh remote transcript");
    assert_eq!(t.meta.key, "dsh:devbox:dsh-e2e4-0001");

    // opencode 双库经装饰器仍然齐全
    let oc = by_agent(AgentId::Opencode);
    let refs = oc.list_session_files().expect("opencode remote list");
    assert!(refs.iter().any(|r| r.native_id == "oc-0001"));
    assert!(refs.iter().any(|r| r.native_id == "ocnext-0001"));

    // quick_meta 出口(整 map)也要改写——write_meta_only 是第三条写库路径,
    // 漏改写会让远程行先以本地 key 落库、随后被全量解析改名成双行
    if let Some(map) = oc.quick_meta(&refs) {
        for meta in map.values() {
            assert_eq!(meta.host, "devbox");
            assert!(
                meta.key.starts_with("opencode:devbox:"),
                "quick 出口未改写: {}",
                meta.key
            );
        }
    }
}

/// merge_quick_meta 在两个已改写的 meta 间搬运字段:codex 的 key 覆写
/// (state thread-id)经装饰器转发后仍保持远程格式。
#[test]
fn remote_codex_merge_keeps_host_key() {
    setup();
    let cache = tempfile::tempdir().unwrap();
    let templates = wake_core::adapters::create_adapters();
    let adapters =
        wake_core::adapters::remote::create_remote_adapters(&templates, "devbox", cache.path());
    let codex = adapters
        .iter()
        .find(|a| a.agent() == AgentId::Codex)
        .unwrap();

    // 复用 quickMeta 节的 mk_meta,只补远程差异字段
    let mk = |key: &str, id: &str, title: &str| {
        let mut meta = mk_meta(AgentId::Codex, key, id, title, None);
        meta.host = "devbox".to_string();
        meta
    };
    // parse 侧 key 是文件 native id,quick 侧是 state thread-id(可以不同);
    // 两者都已在各自出口带上 host 段
    let parsed = mk("codex:devbox:file-uuid", "file-uuid", "derived title");
    let quick = mk("codex:devbox:thread-1", "thread-1", "user renamed");
    let merged = codex.merge_quick_meta(parsed, &quick);
    assert_eq!(
        merged.key, "codex:devbox:thread-1",
        "state key 优先且保持远程格式"
    );
    assert_eq!(merged.id, "thread-1");
    assert_eq!(merged.title, "user renamed");
    assert_eq!(merged.host, "devbox");
}

/// state DB 的 rollout_path 是**写库那台机器**上的绝对路径(rsync 原样
/// 镜像过来)——远程缓存下必须按 rollout 文件名(时间戳 + uuid,全局唯一)匹配,
/// 否则远程 Codex 的手工标题与 thread-key 全部静默丢失(2026-09-02 review)
#[test]
fn remote_codex_state_matches_despite_foreign_rollout_path() {
    setup();
    let cache = tempfile::tempdir().unwrap();
    let codex_home = cache.path().join(".codex");
    let day = codex_home.join("sessions/2026/09/01");
    fs::create_dir_all(&day).unwrap();
    let uuid = "0199535a-40b3-7ac2-921b-d5de1a3a8e15";
    let file = format!("rollout-2026-09-01T10-00-00-{uuid}.jsonl");
    fs::write(
        day.join(&file),
        b"{\"timestamp\":\"t\",\"type\":\"x\",\"payload\":{}}\n",
    )
    .unwrap();

    let conn = rusqlite::Connection::open(codex_home.join("state_5.sqlite")).unwrap();
    conn.execute_batch(
        "CREATE TABLE threads (id TEXT, rollout_path TEXT, cwd TEXT, title TEXT, name TEXT,
         tokens_used INTEGER, archived INTEGER, git_branch TEXT, model TEXT, source TEXT,
         created_at_ms INTEGER, updated_at_ms INTEGER);",
    )
    .unwrap();
    conn.execute(
        "INSERT INTO threads VALUES (?1, ?2, '/work/proj', 'raw', 'Renamed by user',
         5, 0, NULL, NULL, NULL, 1, 2)",
        rusqlite::params![
            uuid,
            format!("/home/alice/.codex/sessions/2026/09/01/{file}")
        ],
    )
    .unwrap();
    drop(conn);

    let templates = wake_core::adapters::create_adapters();
    let adapters =
        wake_core::adapters::remote::create_remote_adapters(&templates, "devbox", cache.path());
    let codex = adapters
        .iter()
        .find(|a| a.agent() == AgentId::Codex)
        .unwrap();
    let refs = codex.list_session_files().unwrap();
    assert_eq!(refs.len(), 1);
    let quick = codex.quick_meta(&refs).expect("state db readable");
    let meta = quick
        .get(&refs[0].file_path)
        .expect("state row must match via remapped rollout_path");
    assert_eq!(meta.title, "Renamed by user", "手工标题不得因路径失配丢失");
    assert_eq!(meta.key, format!("codex:devbox:{uuid}"));
    assert_eq!(meta.host, "devbox");
}

/// roster 组装的真实路径(GUI 与 scan CLI 都走 create_adapter_roster_for):
/// 启用的 host 每家追加一个远程实例到 active 尾部,禁用即整组回落;
/// 远程实例不进 locations 面板快照。
#[test]
fn roster_appends_remote_instances_for_enabled_hosts() {
    setup();
    let dir = tempfile::tempdir().unwrap();
    let store = wake_core::db::Store::open(&dir.path().join("roster.db")).unwrap();

    let baseline = wake_core::adapters::create_adapter_roster_for(&store);
    let local_n = baseline.active.len();
    let locations_n = baseline.locations.len();

    store.add_remote_host("devbox").unwrap();
    let roster = wake_core::adapters::create_adapter_roster_for(&store);
    assert_eq!(
        roster.active.len(),
        local_n + AgentId::ALL.len(),
        "每家一个远程实例"
    );
    assert_eq!(
        roster.locations.len(),
        locations_n,
        "远程实例不进 Session locations 面板"
    );
    let remote_count = roster
        .active
        .iter()
        .filter(|a| a.host() == "devbox")
        .count();
    assert_eq!(remote_count, AgentId::ALL.len());
    // 顺序契约:默认实例在前,"按 agent 找第一个"的兜底不受远程影响
    assert!(roster.active[..local_n].iter().all(|a| a.host().is_empty()));

    store.set_remote_host_enabled("devbox", false).unwrap();
    let roster = wake_core::adapters::create_adapter_roster_for(&store);
    assert_eq!(roster.active.len(), local_n, "禁用的 host 整组移出 roster");
}

/// REMOTE_LAYOUTS 的 mount 契约:整形在"缓存尚未同步"(目录不存在)与
/// "缓存已落盘"两种状态下必须给出**同一组数据根**——远程实例是构造时刻
/// 快照,roster 不随同步重建;若某家 mount 选在会随目录出现而改判的层级,
/// 首次同步前后就会各指一棵树,先构造的实例读不到后落盘的数据。
#[test]
fn remote_mount_shaping_is_stable_across_sync_states() {
    setup();
    let templates = wake_core::adapters::create_adapters();

    let empty = tempfile::tempdir().unwrap();
    let synced = tempfile::tempdir().unwrap();
    // 按白名单把"已同步"缓存的目录结构造出来(目录源建树,文件源 touch)
    for layout in wake_core::remote::REMOTE_LAYOUTS {
        for path in layout.sync_paths {
            let dest = synced.path().join(path);
            if std::path::Path::new(path).extension().is_some() {
                fs::create_dir_all(dest.parent().unwrap()).unwrap();
                fs::write(dest, b"").unwrap();
            } else {
                fs::create_dir_all(dest).unwrap();
            }
        }
    }

    let before = wake_core::adapters::remote::create_remote_adapters(&templates, "h", empty.path());
    let after = wake_core::adapters::remote::create_remote_adapters(&templates, "h", synced.path());
    for (a, b) in before.iter().zip(&after) {
        assert_eq!(a.agent(), b.agent());
        let rel = |adapter: &Box<dyn AgentAdapter>, root: &std::path::Path| -> Vec<PathBuf> {
            adapter
                .data_roots()
                .iter()
                .map(|p| {
                    p.strip_prefix(root)
                        .expect("root inside cache")
                        .to_path_buf()
                })
                .collect()
        };
        assert_eq!(
            rel(a, empty.path()),
            rel(b, synced.path()),
            "{:?} 的 mount 整形随缓存落盘而漂移",
            a.agent()
        );
    }
}

// ---------------------------------------------------------------- CodeBuddy

/// CodeBuddy 的 Responses 形 JSONL 归桶:custom-title 压过前后的 ai-title、
/// 占位 ai-title 不采信、模型取 requestModelName 且 last-wins、token 按调用的
/// rawUsage 累加、reasoning 落 thinking、工具结果按 callId 回填并按 status 判错、
/// 工具结果之后的 assistant 行另起一条(与 Claude 按 API 响应分条同粒度)、
/// `<system-reminder>` 用户行归 Meta;summary / turn-metrics /
/// file-history-snapshot 是已知元数据,只有 wibble-row 计 unknown
#[test]
fn codebuddy_parse_contract() {
    let _env = setup();
    let adapter = CodebuddyAdapter::new();
    let r = codebuddy_ref(AgentId::Codebuddy);
    let s = adapter.parse_session(&r).unwrap();
    let t = adapter.parse_transcript(&r).unwrap();

    assert_eq!(
        s.meta.title, "QR effect cleanup",
        "custom-title 压过 ai-title"
    );
    assert_eq!(
        s.meta.model.as_deref(),
        Some("Hy3-Pro"),
        "requestModelName last-wins"
    );
    assert_eq!(s.meta.tokens_used, Some(1280 + 1440 + 1560));
    assert_eq!(s.meta.project_path, "/Users/fixture/src/wakefx");
    assert_eq!(s.meta.project_name, "wakefx");
    assert_eq!(s.meta.created_at, 1781000000000);
    assert_eq!(s.meta.updated_at, 1781000002400);
    assert_eq!(s.meta.key, "codebuddy:cb000001-aaaa-bbbb-cccc-000000000001");
    assert_eq!(s.unknown_line_count, 1); // wibble-row;summary/turn-metrics/file-history-snapshot 不计

    let roles: Vec<Role> = t.mainline.iter().map(|m| m.role).collect();
    assert_eq!(
        roles,
        vec![
            Role::User,
            Role::Assistant,
            Role::Assistant,
            Role::Assistant,
            Role::User,
            Role::Assistant,
            Role::User,
        ],
        "工具结果之后的 assistant 行另起一条"
    );
    assert_eq!(s.meta.message_count, 6, "Meta 的 system-reminder 不计");
    assert_eq!(t.mainline[6].kind, MessageKind::Meta);

    let first = &t.mainline[1];
    assert!(first
        .thinking
        .as_deref()
        .is_some_and(|x| x.contains("without a cleanup return")));
    assert_eq!(first.text, "先看一下组件源码。");
    assert_eq!(first.model.as_deref(), Some("Hy3"));
    assert_eq!(first.tool_calls.len(), 1);
    assert_eq!(first.tool_calls[0].name, "Read");
    assert!(
        first.tool_calls[0]
            .input
            .as_deref()
            .is_some_and(|i| i.contains("QrCode.tsx")),
        "arguments JSON 字符串已解开"
    );
    assert!(first.tool_calls[0]
        .output
        .as_deref()
        .is_some_and(|o| o.contains("setInterval")));
    assert!(!first.tool_calls[0].is_error);

    let second = &t.mainline[2];
    assert_eq!(second.tool_calls.len(), 1);
    assert_eq!(second.tool_calls[0].name, "Bash");
    assert!(second.tool_calls[0].is_error, "status=failed 判错");
    assert!(second.text.is_empty());

    assert_eq!(
        t.mainline[3].text,
        "定时器没有在 cleanup 里 clearInterval,我来补上。"
    );
    assert_eq!(t.mainline[5].model.as_deref(), Some("Hy3-Pro"));

    let text: String = s
        .units
        .iter()
        .map(|u| u.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("二维码组件"), "搜索应索引到用户提问");
    assert!(text.contains("clearInterval"), "搜索应索引到助手回复");
}

/// 没有 custom/ai 标题时退到 topic;reasoning 的 content.text 形也收进 thinking;
/// 只有 model id 时就显示 id;rawUsage 缺 total_tokens 时按 prompt+completion 计
#[test]
fn codebuddy_topic_title_and_model_id_fallback() {
    let _env = setup();
    let adapter = CodebuddyAdapter::new();
    let r = codebuddy_topic_ref();
    let s = adapter.parse_session(&r).unwrap();
    let t = adapter.parse_transcript(&r).unwrap();
    assert_eq!(s.meta.title, "Weekly report draft");
    assert_eq!(s.meta.model.as_deref(), Some("hy3"));
    assert_eq!(s.meta.tokens_used, Some(900 + 120));
    assert_eq!(s.unknown_line_count, 0);
    assert_eq!(t.mainline.len(), 2);
    assert!(t.mainline[1]
        .thinking
        .as_deref()
        .is_some_and(|x| x.contains("merged PRs")));
}

/// 枚举只认 slug 目录直属的转录:`<session>/subagents/agent-*.jsonl` 既不进列表、
/// watcher 事件也被 file_ref 拒掉;自定义 location 选 `~/.codebuddy` 或 projects
/// 目录都认;删除把 `<id>.meta.json` 与 `<id>/` 边车一并带走
#[test]
fn codebuddy_lists_only_top_level_transcripts() {
    let _env = setup();
    // 契约测试的假 home 不摆目录型 fixture(那是 cli / mcp / remote_sync 的活),
    // 直接把检入的 fixture 树当自定义 location
    let adapter = CodebuddyAdapter::new().with_custom_root(fixture("codebuddy"));
    let root = adapter.data_roots()[0].clone();
    let refs = adapter.list_session_files().unwrap();
    assert_eq!(refs.len(), 2, "两条顶层会话");
    assert!(refs.iter().all(|r| !r.file_path.contains("subagents")));

    let main = root.join("Users-fixture-src-wakefx/cb000001-aaaa-bbbb-cccc-000000000001.jsonl");
    let sub = root
        .join("Users-fixture-src-wakefx/cb000001-aaaa-bbbb-cccc-000000000001/subagents/agent-deadbeef.jsonl");
    assert!(sub.is_file(), "fixture 应带子代理转录");
    assert!(adapter.file_ref(&main).is_some());
    assert!(adapter.file_ref(&sub).is_none(), "子代理转录不是顶层会话");

    let meta = adapter.parse_session(&refs[0]).unwrap().meta;
    let paths = adapter.session_paths(&meta);
    assert!(
        paths.iter().any(|p| p.ends_with(".meta.json")),
        "meta.json 边车随会话删"
    );
    assert!(
        paths
            .iter()
            .any(|p| p.ends_with("cb000001-aaaa-bbbb-cccc-000000000001")),
        "边车目录随会话删"
    );

    for dir in ["codebuddy", "codebuddy/projects"] {
        let rooted = CodebuddyAdapter::new().with_custom_root(fixture(dir));
        assert_eq!(rooted.list_session_files().unwrap().len(), 2, "{dir}");
    }
}

/// WorkBuddy 是 CodeBuddy 的孪生实例(pi / omp 同款):同一解析核心,只有 agent
/// 身份、key 前缀与数据根不同;没有 CLI,所以不给任何 resume 目标
#[test]
fn workbuddy_is_a_codebuddy_twin() {
    let _env = setup();
    let twin = CodebuddyAdapter::workbuddy();
    assert_eq!(twin.agent(), AgentId::Workbuddy);
    assert!(twin.data_roots()[0].ends_with(".workbuddy/projects"));

    let rooted = twin.with_custom_root(fixture("codebuddy"));
    let refs = rooted.list_session_files().unwrap();
    assert_eq!(refs.len(), 2);
    assert!(refs.iter().all(|r| r.agent == AgentId::Workbuddy));
    let meta = rooted.parse_session(&refs[0]).unwrap().meta;
    assert_eq!(meta.agent, AgentId::Workbuddy);
    assert_eq!(meta.key, "workbuddy:cb000001-aaaa-bbbb-cccc-000000000001");
    assert_eq!(meta.title, "QR effect cleanup");
    assert!(
        wake_core::services::terminal::resume_targets(&meta).is_empty(),
        "没有 CLI 的 agent 不画 Open In"
    );
}

// ---------------------------------------------------------------- 记忆(只读镜像)

/// Claude auto-memory:projects/<dir>/memory/*.md,挂到同目录里的一条会话上(项目
/// 路径读库时按它解析,adapter 不填),标题取 frontmatter 的 description、没有就用
/// 文件名,正文原样;指纹没变第二次列出的是缓存的同一份
#[test]
fn claude_lists_project_memories() {
    setup();
    let adapter = ClaudeAdapter::new().with_custom_root(fixture("claude/projects"));
    let docs = all_memories(&adapter).unwrap();
    let titles: Vec<&str> = docs.iter().map(|d| d.title.as_str()).collect();
    assert_eq!(
        titles,
        ["MEMORY.md", "Wake testing conventions for this repo"]
    );
    let session_keys: Vec<String> = adapter
        .list_session_files()
        .unwrap()
        .iter()
        .map(|r| format!("claude-code:{}", r.native_id))
        .collect();
    for d in &docs {
        assert_eq!(d.agent, AgentId::ClaudeCode);
        assert_eq!(d.scope, MemoryScope::Project);
        assert!(
            d.project_path.is_empty() && d.project_name.is_empty(),
            "项目在读库时按锚点解析,adapter 不填"
        );
        assert!(
            session_keys.contains(&d.session_key),
            "锚点是同目录里的一条会话: {} ∉ {session_keys:?}",
            d.session_key
        );
        assert!(
            d.key.starts_with("claude-code:") && d.key.ends_with(".md"),
            "{}",
            d.key
        );
        assert!(d.host.is_empty());
        assert!(d.size_bytes > 0 && d.updated_at > 0);
    }
    assert_eq!(
        docs[1].body.lines().next(),
        Some("---"),
        "正文原样含 frontmatter"
    );
    assert!(docs[1].body.contains("name: wake-testing"));
    assert_eq!(all_memories(&adapter).unwrap(), docs, "指纹没变走缓存");
}

/// ZCode:`cli/memories/projects/<slug>-<hash>/memory/*.md`(Claude auto-memory 同款
/// 格式);目录名的 hash 是 sha256(工作区路径) 前 16 位,按库里会话的 directory 精确
/// 对上项目、不用锚点;对不上的落 Unknown project;没有 memory/ 子目录的工作区目录
/// 不算;裸库拷贝(没有 home)一份都不列
#[test]
fn zcode_lists_project_memories() {
    setup();
    let home = tempfile::tempdir().unwrap();
    let root = home.path().join(".zcode");
    let db = root.join("cli/db/db.sqlite");
    fs::create_dir_all(db.parent().unwrap()).unwrap();
    common::build_zcode_db(&db);
    // 3b44ec0d2ccabf78 = sha256("/Users/tester/Github/wakefx")[..16],fixture 会话的
    // directory。写死而不是调 memory_dir_hash 算——算法漂了这里才会红
    let wakefx = root.join("cli/memories/projects/wakefx-3b44ec0d2ccabf78/memory");
    fs::create_dir_all(&wakefx).unwrap();
    fs::write(
        wakefx.join("MEMORY.md"),
        "- [Prefers pnpm](prefers-pnpm.md) — tooling\n",
    )
    .unwrap();
    fs::write(
        wakefx.join("prefers-pnpm.md"),
        "---\nname: prefers-pnpm\ndescription: Prefers pnpm over npm\nmetadata:\n  type: feedback\n---\n\nUse pnpm.\n",
    )
    .unwrap();
    // 库里没有会话的工作区:hash 对不上,落 Unknown project
    let orphan = root.join("cli/memories/projects/other-0000000000000000/memory");
    fs::create_dir_all(&orphan).unwrap();
    fs::write(orphan.join("note.md"), "# orphan\n").unwrap();
    fs::create_dir_all(root.join("cli/memories/projects/empty-1111111111111111")).unwrap();

    let adapter = ZcodeAdapter::new().with_custom_root(root.clone());
    let docs = all_memories(&adapter).unwrap();
    let summary: Vec<(&str, &str, &str)> = docs
        .iter()
        .map(|d| {
            (
                d.title.as_str(),
                d.project_path.as_str(),
                d.project_name.as_str(),
            )
        })
        .collect();
    assert_eq!(
        summary,
        [
            ("note.md", "", ""),
            ("MEMORY.md", "/Users/tester/Github/wakefx", "wakefx"),
            (
                "Prefers pnpm over npm",
                "/Users/tester/Github/wakefx",
                "wakefx"
            ),
        ]
    );
    for d in &docs {
        assert_eq!(d.agent, AgentId::Zcode);
        assert_eq!(d.scope, MemoryScope::Project);
        assert!(d.session_key.is_empty(), "项目由目录名直接对上,不用锚点");
        assert!(
            d.key.starts_with("zcode:") && d.key.ends_with(".md"),
            "{}",
            d.key
        );
        assert!(d.host.is_empty());
        assert!(d.size_bytes > 0 && d.updated_at > 0);
    }
    assert!(
        docs[2].body.contains("name: prefers-pnpm"),
        "正文原样含 frontmatter"
    );
    assert_eq!(all_memories(&adapter).unwrap(), docs, "指纹没变走缓存");
    assert_eq!(
        wake_core::adapters::zcode::memory_dir_hash("/Users/tester/Github/wakefx/"),
        "3b44ec0d2ccabf78",
        "收尾分隔符不影响(源码先 resolve 再 hash)"
    );

    // 直接选中 db.sqlite 的裸库拷贝没有 home:不摸父目录,一份都不列
    let bare = ZcodeAdapter::new().with_custom_root(db.clone());
    assert!(all_memories(&bare).unwrap().is_empty());
    // 没有记忆目录的 home 也是空,不是错
    let other = tempfile::tempdir().unwrap();
    let fresh = ZcodeAdapter::new().with_custom_root(other.path().join(".zcode"));
    assert!(all_memories(&fresh).unwrap().is_empty());
}

/// 记忆可见层二期:用户写给 agent 的指令文件也进记忆层——各家 home 里的全局文件
/// (`~/.claude/CLAUDE.md`、`~/.codex/AGENTS.md` + `rules/*.rules`、`~/.gemini/GEMINI.md`)
/// 由各家 `memory_sources` 报,项目根下的(CLAUDE.md / AGENTS.md / GEMINI.md /
/// `.cursor/rules/*.mdc` / `.cursorrules` / `.kiro/steering/*.md` /
/// `.github/copilot-instructions.md`)由 `project_instruction_sources` 按 agent 给、
/// 按已索引的项目根展开。标题是文件名(mdc 有 frontmatter 的取 description),
/// 项目级的 project_path 直接填项目根,来源 id 带 `<project>/` 前缀
#[test]
fn instruction_files_join_the_memory_layer() {
    setup();
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let write_under = |root: &std::path::Path, rel: &str, body: &str| {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    };
    let w = |rel: &str, body: &str| write_under(home.path(), rel, body);
    let pw = |rel: &str, body: &str| write_under(project.path(), rel, body);
    w(".claude/CLAUDE.md", "# global claude\n");
    fs::create_dir_all(home.path().join(".claude/projects")).unwrap();
    w(".codex/AGENTS.md", "# global agents\n");
    w(
        ".codex/rules/safety.rules",
        "prefix_rule(prefix=[\"rm\"], decision=\"forbidden\")\n",
    );
    fs::create_dir_all(home.path().join(".codex/sessions")).unwrap();
    w(
        ".gemini/GEMINI.md",
        "## Gemini Added Memories\n- uses supastarter\n",
    );
    fs::create_dir_all(home.path().join(".gemini/tmp")).unwrap();
    pw("CLAUDE.md", "# project claude\n");
    pw("AGENTS.md", "# project agents\n");
    pw("GEMINI.md", "# project gemini\n");
    pw(
        ".cursor/rules/style.mdc",
        "---\ndescription: Use tabs, never spaces\nglobs: *.ts\nalwaysApply: false\n---\n\nTabs.\n",
    );
    pw(".cursorrules", "legacy rules\n");
    pw(".kiro/steering/product.md", "# product\n");
    pw(".github/copilot-instructions.md", "# copilot\n");
    let projects = vec![project.path().to_path_buf()];
    let project_str = project.path().to_string_lossy().to_string();

    // (adapter, 期望的 (标题, scope, 来源 id) 列表——项目级的来源 id 带 <project>/ 前缀)
    let claude = ClaudeAdapter::new().with_custom_root(home.path().join(".claude"));
    let codex = CodexAdapter::new().with_custom_root(home.path().join(".codex"));
    let gemini = GeminiAdapter::new().with_custom_root(home.path().join(".gemini"));
    let cursor = CursorAdapter::new().with_custom_root(project.path().join("nowhere"));
    let kiro = KiroAdapter::new().with_custom_root(project.path().join("nowhere"));
    let copilot = CopilotAdapter::new().with_custom_root(project.path().join("nowhere"));
    let cases: Vec<(&Box<dyn AgentAdapter>, Vec<(&str, MemoryScope, String)>)> = vec![
        (
            &claude,
            vec![
                (
                    "CLAUDE.md",
                    MemoryScope::User,
                    home.path()
                        .join(".claude")
                        .join("CLAUDE.md")
                        .to_string_lossy()
                        .to_string(),
                ),
                (
                    "CLAUDE.md",
                    MemoryScope::Project,
                    "<project>/CLAUDE.md".into(),
                ),
            ],
        ),
        (
            &codex,
            vec![
                (
                    "AGENTS.md",
                    MemoryScope::User,
                    home.path()
                        .join(".codex")
                        .join("AGENTS.md")
                        .to_string_lossy()
                        .to_string(),
                ),
                (
                    "safety.rules",
                    MemoryScope::User,
                    home.path()
                        .join(".codex")
                        .join("rules")
                        .to_string_lossy()
                        .to_string(),
                ),
                (
                    "AGENTS.md",
                    MemoryScope::Project,
                    "<project>/AGENTS.md".into(),
                ),
            ],
        ),
        (
            &gemini,
            vec![
                (
                    "GEMINI.md",
                    MemoryScope::User,
                    home.path()
                        .join(".gemini")
                        .join("GEMINI.md")
                        .to_string_lossy()
                        .to_string(),
                ),
                (
                    "GEMINI.md",
                    MemoryScope::Project,
                    "<project>/GEMINI.md".into(),
                ),
            ],
        ),
        (
            &cursor,
            vec![
                (
                    "Use tabs, never spaces",
                    MemoryScope::Project,
                    "<project>/.cursor/rules".into(),
                ),
                (
                    ".cursorrules",
                    MemoryScope::Project,
                    "<project>/.cursorrules".into(),
                ),
            ],
        ),
        (
            &kiro,
            vec![(
                "product.md",
                MemoryScope::Project,
                "<project>/.kiro/steering".into(),
            )],
        ),
        (
            &copilot,
            vec![(
                "copilot-instructions.md",
                MemoryScope::Project,
                "<project>/.github/copilot-instructions.md".into(),
            )],
        ),
    ];
    for (adapter, expected) in cases {
        let agent = adapter.agent();
        let mut sources = adapter.memory_sources();
        sources.extend(wake_core::adapters::project_instruction_sources(agent));
        // 这些临时 home 里没有 agent 自己记的记忆,列出来的全是指令文件
        let docs: Vec<MemoryDoc> = adapter.list_memories(&sources, &projects).unwrap();
        let got: Vec<(&str, MemoryScope, String)> = docs
            .iter()
            .map(|d| (d.title.as_str(), d.scope, d.source.clone()))
            .collect();
        assert_eq!(got, expected, "{}", agent.as_str());
        for d in &docs {
            assert_eq!(d.agent, agent);
            assert!(
                d.key.starts_with(&format!("{}:", agent.as_str())),
                "{}",
                d.key
            );
            assert!(d.session_key.is_empty(), "指令文件不用锚点");
            if d.scope == MemoryScope::Project {
                assert_eq!(d.project_path, project_str, "项目根直接填进归属");
                assert!(!d.project_name.is_empty());
            } else {
                assert!(d.project_path.is_empty());
            }
        }
    }
    // 项目根就是某家的 home(用户在 ~/.codex 里跑过 codex):`<project>/AGENTS.md` 展开成
    // 全局那份同一个文件,不能把用户级顶成 ".codex" 项目的——具体来源优先,模式跳过
    let mut sources = codex.memory_sources();
    sources.extend(wake_core::adapters::project_instruction_sources(
        AgentId::Codex,
    ));
    let agents_md = home.path().join(".codex").join("AGENTS.md");
    let docs = codex
        .list_memories(
            &sources,
            &[home.path().join(".codex"), project.path().to_path_buf()],
        )
        .unwrap();
    let same_file: Vec<&MemoryDoc> = docs
        .iter()
        .filter(|d| d.path == agents_md.to_string_lossy())
        .collect();
    assert_eq!(same_file.len(), 1, "{same_file:?}");
    assert_eq!(same_file[0].scope, MemoryScope::User);
    assert_eq!(same_file[0].source, agents_md.to_string_lossy());

    // 项目根传空(远程实例、自定义根)时项目模式什么都不展开,全局的照列
    let mut sources = claude.memory_sources();
    sources.extend(wake_core::adapters::project_instruction_sources(
        AgentId::ClaudeCode,
    ));
    let global_only = claude.list_memories(&sources, &[]).unwrap();
    assert_eq!(global_only.len(), 1);
    // 裸 projects 目录当根:没有 home,全局 CLAUDE.md 不摸父目录
    let bare = ClaudeAdapter::new().with_custom_root(home.path().join(".claude/projects"));
    assert!(bare
        .memory_sources()
        .iter()
        .all(|s| s.kind == MemorySourceKind::ProjectTree));
}

/// 标题来自 frontmatter 的 description:块标量(`description: >` 换行缩进写)折成一行,
/// 不是一个字面的 ">";超长的按字符封顶(本机有 565 字符的,列表与 MCP 一行放不下)
#[test]
fn memory_titles_fold_block_scalars_and_clip() {
    setup();
    let root = tempfile::tempdir().unwrap();
    let project = root.path().join("-Users-tester-Github-wakefx");
    fs::create_dir_all(project.join("memory")).unwrap();
    fs::write(
        project.join("11111111-aaaa-bbbb-cccc-000000000001.jsonl"),
        "{\"type\":\"user\",\"cwd\":\"/Users/tester/Github/wakefx\"}\n",
    )
    .unwrap();
    fs::write(
        project.join("memory").join("folded.md"),
        "---\nname: folded\ndescription: >\n  Prefers pnpm over npm,\n  and never runs db:push\nmetadata:\n  type: feedback\n---\n\nbody\n",
    )
    .unwrap();
    let long = "x".repeat(300);
    fs::write(
        project.join("memory").join("long.md"),
        format!("---\nname: long\ndescription: {long}\n---\n\nbody\n"),
    )
    .unwrap();
    let adapter = ClaudeAdapter::new().with_custom_root(root.path().to_path_buf());
    let docs = all_memories(&adapter).unwrap();
    let titles: Vec<&str> = docs.iter().map(|d| d.title.as_str()).collect();
    assert_eq!(titles[0], "Prefers pnpm over npm, and never runs db:push");
    assert!(
        titles[1].chars().count() <= 121 && titles[1].ends_with('…'),
        "{}",
        titles[1]
    );
}

/// Claude 记忆的锚点是项目目录里最新的**非空**会话:零字节的 jsonl(刚起的会话)
/// 进不了库,拿它当锚点整组记忆就落 Unknown project
#[test]
fn claude_memory_anchor_skips_empty_sessions() {
    setup();
    let root = tempfile::tempdir().unwrap();
    let project = root.path().join("-Users-tester-Github-wakefx");
    fs::create_dir_all(project.join("memory")).unwrap();
    fs::write(project.join("memory").join("MEMORY.md"), "# notes\n").unwrap();
    fs::write(
        project.join("11111111-aaaa-bbbb-cccc-000000000001.jsonl"),
        "{\"type\":\"user\",\"cwd\":\"/Users/tester/Github/wakefx\"}\n",
    )
    .unwrap();
    fs::write(
        project.join("ffffffff-aaaa-bbbb-cccc-00000000000f.jsonl"),
        "",
    )
    .unwrap();
    let adapter = ClaudeAdapter::new().with_custom_root(root.path().to_path_buf());
    let docs = all_memories(&adapter).unwrap();
    assert_eq!(docs.len(), 1);
    assert_eq!(
        docs[0].session_key,
        "claude-code:11111111-aaaa-bbbb-cccc-000000000001"
    );
}

/// Codex:memories/*.md 是用户级;memories_1.sqlite 的 stage1_outputs 逐线程一行,
/// 挂到 codex:<thread_id>;空摘要的线程不列;没有 home 的裸目录为空
#[test]
fn codex_lists_user_and_thread_memories() {
    setup();
    let home = tempfile::tempdir().unwrap();
    fs::create_dir_all(home.path().join("sessions")).unwrap();
    fs::create_dir_all(home.path().join("memories")).unwrap();
    fs::write(
        home.path().join("memories").join("user-preferences.md"),
        "# User preferences\n\nConcise replies.\n",
    )
    .unwrap();
    let db = rusqlite::Connection::open(home.path().join("memories_1.sqlite")).unwrap();
    db.execute_batch(
        "CREATE TABLE stage1_outputs (
            thread_id TEXT PRIMARY KEY, source_updated_at INTEGER NOT NULL,
            raw_memory TEXT, rollout_summary TEXT, rollout_slug TEXT,
            generated_at INTEGER NOT NULL, usage_count INTEGER, last_usage INTEGER,
            selected_for_phase2 INTEGER NOT NULL DEFAULT 0,
            selected_for_phase2_source_updated_at INTEGER);
         INSERT INTO stage1_outputs VALUES ('t-0001', 1, 'Prefers rustfmt before commits',
            'Fixed the scanner finale contract', 'scanner-finale', 1786100000, NULL, NULL, 0, NULL);
         INSERT INTO stage1_outputs VALUES ('t-0002', 1, '', '', NULL, 1786100001, NULL, NULL, 0, NULL);
         INSERT INTO stage1_outputs VALUES ('t-0003', 1, NULL, 'Only a summary, memory column NULL',
            'nullable-row', '2026-09-21T10:00:00Z', NULL, NULL, 0, NULL);",
    )
    .unwrap();
    drop(db);

    let adapter = CodexAdapter::new().with_custom_root(home.path().to_path_buf());
    let docs = all_memories(&adapter).unwrap();
    assert_eq!(docs.len(), 3, "空摘要空记忆的线程不列: {docs:?}");
    // 列的可空性与时间格式是推断的:NULL 正文列与 ISO 文本时间的行照样列,别的行不受影响
    let nullable = &docs[2];
    assert_eq!(
        (nullable.session_key.as_str(), nullable.title.as_str()),
        ("codex:t-0003", "nullable-row")
    );
    assert!(nullable.body.contains("Only a summary") && nullable.updated_at > 1_786_100_000_000);
    let user = &docs[0];
    assert_eq!(
        (user.scope, user.title.as_str()),
        (MemoryScope::User, "user-preferences.md")
    );
    assert!(user.project_path.is_empty() && user.session_key.is_empty());
    let thread = &docs[1];
    assert_eq!(thread.scope, MemoryScope::Thread);
    assert_eq!(thread.session_key, "codex:t-0001");
    assert_eq!(thread.title, "scanner-finale");
    assert!(
        thread.body.contains("Fixed the scanner finale contract")
            && thread.body.contains("rustfmt")
    );
    assert_eq!(thread.updated_at, 1_786_100_000_000, "秒换算成毫秒");
    // 非 UTF-8 的文件不是记忆:跳过它,别的照列(指纹变了才重读,新文件即新指纹)
    fs::write(
        home.path().join("memories").join("blob.md"),
        [0xff, 0xfe, 0x00],
    )
    .unwrap();
    let docs_again = all_memories(&adapter).unwrap();
    assert_eq!(docs_again.len(), 3, "{docs_again:?}");
    // 自定义根只选了 sessions 目录:没有 home,不摸父目录里的 memories
    let sessions_only = CodexAdapter::new().with_custom_root(home.path().join("sessions"));
    assert!(
        all_memories(&sessions_only).unwrap().is_empty(),
        "sessions 目录当根时不得越界读父目录的记忆"
    );
    // 库在但读不出(不是 SQLite 文件)= 不知道:整家报 Err、不缓存,scanner 跳过该组;
    // 缓存成"没有"会把库里的线程记忆整组删掉。库换回好的(戳变了)立刻恢复
    let db_path = home.path().join("memories_1.sqlite");
    let good = fs::read(&db_path).unwrap();
    fs::write(&db_path, b"not a sqlite database").unwrap();
    // 缓存戳是毫秒级 mtime:上一次读到覆写之间不到一毫秒时戳没变、拿到的是缓存的好结果,
    // CI 与本机都红过。把 mtime 明确往前拨,不赌时钟
    touch_forward(&db_path, 2);
    assert!(
        all_memories(&adapter).is_err(),
        "memories_1.sqlite 读不出必须报 Err 而不是当成空"
    );
    fs::write(&db_path, good).unwrap();
    touch_forward(&db_path, 4);
    assert_eq!(all_memories(&adapter).unwrap().len(), 3);
    assert!(
        thread.path.ends_with("memories_1.sqlite#t-0001"),
        "{}",
        thread.path
    );
    assert_eq!(thread.key, format!("codex:{}", thread.path));

    // 没有 home 证据的目录:memories 自然为空,不报错
    let bare = CodexAdapter::new().with_custom_root(home.path().join("elsewhere"));
    assert!(all_memories(&bare).unwrap().is_empty());
}

/// 没装 Hermes / Cursor(库不在、表不在)= **确定没有**父子关系(Some 空);只有库在但读不出
/// 才是"不知道"(None)。原先一律 None,没装这两家的机器每轮都把它们(本地真实的 /branch、
/// 子代理)的关系冻住、永不自愈(2026-09-22 review)
#[test]
fn missing_parent_link_stores_mean_no_links_not_unknown() {
    let dir = tempfile::tempdir().unwrap();
    let hermes = HermesAdapter::new().with_custom_root(dir.path().join("nope"));
    assert_eq!(hermes.parent_links(), Some(Vec::new()));
    let cursor = CursorIdeAdapter::new().with_custom_root(dir.path().join("cursor-nope"));
    assert_eq!(cursor.parent_links(), Some(Vec::new()));
}
