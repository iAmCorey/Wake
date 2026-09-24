//! 集成测试共用的 fixture 搭建:合成 fixtures → 一个像真机 home 的目录树。
//! adapter_contracts 用它建假 HOME,remote_sync 用它建"远端 home"——同一份
//! 侧档/SQLite 库/dsh zstd 压制,两边不会各自漂移。fixture 全合成,绝不放
//! 真实会话数据。
#![allow(dead_code)] // 每个测试二进制只用到其中一部分

use std::fs;
use std::path::{Path, PathBuf};

use wake_core::db::{IndexLock, Ownership};

pub fn fixture(rel: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(rel)
}

/// 一个最小的 Codex home:state DB + 一条用户线程 + 一条 `spawn_agent` 子线程。
/// 子线程的前半段是 `fork_turns` 复制进来的父线程历史,分界点由首行的
/// `subagent_history_start_ordinal` 指定(真机形态,实测 0.151 会写);那一段里
/// 还夹着一封发给**别的**子代理的派活信封,用来卡住"认收件人而不是认第一条
/// 信封"这条兜底判据。fork 段**以助手消息收尾**、子线程自己**从工具调用开头**
/// ——这正是折叠会把子线程自己的工具调用一起 splice 掉的形状。父子关系登记在
/// state DB 的 thread_spawn_edges 里。adapter_contracts 与 scanner_finale 共用
/// 一份,免得两边各写一套慢慢漂开。返回 (父线程 id, 子线程 id, 子线程文件路径)
pub fn stage_codex_spawn_pair(home: &Path) -> (String, String, PathBuf) {
    let parent_id = "11111111-aaaa-4bbb-8ccc-000000000001".to_string();
    let child_id = "22222222-aaaa-4bbb-8ccc-000000000002".to_string();
    let day = home.join("sessions/2026/09/16");
    fs::create_dir_all(&day).unwrap();

    // Codex 给每行编 ordinal(= 行号);子线程首行的
    // subagent_history_start_ordinal 就是按它指的
    let write_jsonl = |path: &Path, lines: &[serde_json::Value]| {
        let text: String = lines
            .iter()
            .enumerate()
            .map(|(ordinal, line)| {
                let mut line = line.clone();
                line["ordinal"] = serde_json::json!(ordinal);
                format!("{line}\n")
            })
            .collect();
        fs::write(path, text).unwrap();
    };
    let session_meta = |id: &str, extra: serde_json::Value| {
        let mut payload = serde_json::json!({
            "id": id,
            "timestamp": "2026-09-16T09:00:00.000Z",
            "cwd": "/work/wake",
            "originator": "codex_cli_rs"
        });
        payload
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        serde_json::json!({
            "timestamp": "2026-09-16T09:00:00.000Z",
            "type": "session_meta",
            "payload": payload
        })
    };
    let message = |role: &str, text: &str| {
        serde_json::json!({
            "timestamp": "2026-09-16T09:05:00.000Z",
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": role,
                "content": [{"type": "input_text", "text": text}]
            }
        })
    };
    // 派活信封:抬头明文、Payload 加密,与实测形态一致
    let dispatch = |to: &str| {
        serde_json::json!({
            "timestamp": "2026-09-16T09:30:00.000Z",
            "type": "response_item",
            "payload": {
                "type": "agent_message",
                "author": "/root",
                "recipient": to,
                "content": [
                    {"type": "input_text", "text": format!("Message Type: NEW_TASK\nTask name: {to}\nSender: /root\nPayload:\n")},
                    {"type": "encrypted_content", "encrypted_content": "gAAAAAB-opaque"}
                ]
            }
        })
    };

    let parent_head = session_meta(
        &parent_id,
        serde_json::json!({"source": "cli", "thread_source": "user"}),
    );
    write_jsonl(
        &day.join(format!("rollout-2026-09-16T09-00-00-{parent_id}.jsonl")),
        &[
            parent_head.clone(),
            message("user", "inherited parent turn about the qr login bug"),
            message("assistant", "inherited parent answer"),
        ],
    );

    let child_head = session_meta(
        &child_id,
        serde_json::json!({
            "source": {"subagent": {"thread_spawn": {
                "parent_thread_id": parent_id,
                "depth": 1,
                "agent_path": "/root/review_issue17",
                "agent_nickname": "Wegener",
                "agent_role": null
            }}},
            "thread_source": "subagent",
            "agent_path": "/root/review_issue17",
            "subagent_history_start_ordinal": 7
        }),
    );
    let child_path = day.join(format!("rollout-2026-09-16T09-30-00-{child_id}.jsonl"));
    write_jsonl(
        &child_path,
        &[
            child_head,
            // spawn 时原样复制进来的父线程首行,saw_session_meta 必须挡住它
            parent_head,
            message("user", "inherited parent turn about the qr login bug"),
            message("assistant", "inherited parent answer"),
            dispatch("/root/other_task"),
            message("user", "inherited follow-up turn"),
            message("assistant", "inherited parent tail"),
            dispatch("/root/review_issue17"),
            serde_json::json!({
                "timestamp": "2026-09-16T09:31:00.000Z",
                "type": "response_item",
                "payload": {
                    "type": "function_call",
                    "name": "shell",
                    "call_id": "call_child_1",
                    "arguments": "{\"command\":\"rg useEffect\"}"
                }
            }),
            serde_json::json!({
                "timestamp": "2026-09-16T09:31:01.000Z",
                "type": "response_item",
                "payload": {
                    "type": "function_call_output",
                    "call_id": "call_child_1",
                    "output": "CHILD_TOOL_OUTPUT"
                }
            }),
            message("assistant", "child found the missing dependency array"),
        ],
    );

    // state DB:threads 两行(子线程的 title/name 与真机一样是空的,标题只能
    // 由解析侧的任务名给)+ 父子关系登记表
    let conn = rusqlite::Connection::open(home.join("state_5.sqlite")).unwrap();
    conn.execute_batch(
        "CREATE TABLE threads (id TEXT, rollout_path TEXT, cwd TEXT, title TEXT, name TEXT,
         tokens_used INTEGER, archived INTEGER, git_branch TEXT, model TEXT, source TEXT,
         created_at_ms INTEGER, updated_at_ms INTEGER);
         CREATE TABLE thread_spawn_edges (parent_thread_id TEXT NOT NULL,
         child_thread_id TEXT NOT NULL PRIMARY KEY, status TEXT NOT NULL);",
    )
    .unwrap();
    for (id, file, source) in [
        (
            &parent_id,
            format!("rollout-2026-09-16T09-00-00-{parent_id}.jsonl"),
            "cli",
        ),
        (
            &child_id,
            format!("rollout-2026-09-16T09-30-00-{child_id}.jsonl"),
            // 真机把整个结构化来源原样存进 source 列
            "{\"subagent\":{\"thread_spawn\":{}}}",
        ),
    ] {
        conn.execute(
            "INSERT INTO threads VALUES (?1, ?2, '/work/wake', '', NULL, 0, 0,
             NULL, NULL, ?3, 1789000000000, 1789000000000)",
            rusqlite::params![id, day.join(file).to_string_lossy(), source],
        )
        .unwrap();
    }
    conn.execute(
        "INSERT INTO thread_spawn_edges VALUES (?1, ?2, 'open')",
        rusqlite::params![parent_id, child_id],
    )
    .unwrap();
    drop(conn);

    (parent_id, child_id, child_path)
}

pub fn copy_tree(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).unwrap();
    for entry in fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let to = dst.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &to);
        } else {
            fs::copy(entry.path(), &to).unwrap();
        }
    }
}

/// 目录型 fixtures 摆进 home:fixture 目录名即各家 home 下的尾段(pi 的树
/// 同时复制成 omp——omp fork 自 pi,布局同构)
pub fn stage_dir_fixtures(home: &Path) {
    for (src, dst) in [
        ("claude", ".claude"),
        ("codex", ".codex"),
        ("cursor", ".cursor"),
        ("gemini", ".gemini"),
        ("grok", ".grok"),
        ("kimi", ".kimi-code"),
        ("kiro", ".kiro"),
        ("pi", ".pi"),
        ("pi", ".omp"),
        ("qoder", ".qoder"),
        ("codebuddy", ".codebuddy"),
        ("codebuddy", ".workbuddy"),
        ("craft-agents", ".craft-agent"),
    ] {
        copy_tree(&fixture(src), &home.join(dst));
    }
}

/// stage_sidecars 建出的库/日志路径(契约测试按路径直开)
pub struct Sidecars {
    pub copilot_db: PathBuf,
    pub opencode_db: PathBuf,
    pub opencode_next_db: PathBuf,
    pub antigravity_db: PathBuf,
    pub dsh_log: PathBuf,
    pub hermes_db: PathBuf,
    pub openclaw_db: PathBuf,
    pub cursor_ide_db: PathBuf,
    pub zcode_db: PathBuf,
    pub devin_db: PathBuf,
}

/// 侧档与 SQLite 型 fixture 库:copilot/opencode(两代)/antigravity 现建库,
/// gemini 的 projects.json、kimi 的 session_index.jsonl,dsh 的 zstd 日志与
/// 一个子代理会话(验证 file_ref 过滤)。
pub fn stage_sidecars(home: &Path) -> Sidecars {
    let copilot_dir = home.join(".copilot");
    fs::create_dir_all(&copilot_dir).expect("mkdir .copilot");
    let copilot_db = copilot_dir.join("session-store.db");
    build_copilot_db(&copilot_db);

    let oc_dir = home.join(".local").join("share").join("opencode");
    fs::create_dir_all(&oc_dir).expect("mkdir opencode dir");
    let opencode_db = oc_dir.join("opencode.db");
    build_opencode_db(&opencode_db);
    let opencode_next_db = oc_dir.join("opencode-next.db");
    build_opencode_next_db(&opencode_next_db);

    let gem_dir = home.join(".gemini");
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

    let kimi_dir = home.join(".kimi-code");
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
    let dsh_project = home
        .join(".dsh")
        .join("sessions")
        .join("--Users-tester-Github-wakefx--");
    let dsh_sess = dsh_project.join("dsh-e2e4-0001");
    fs::create_dir_all(&dsh_sess).expect("mkdir dsh session dir");
    let plain = fs::read_to_string(fixture("dsh/session.jsonl")).expect("read dsh fixture");
    let (header, body) = plain.split_once('\n').expect("dsh fixture header line");
    let mut frames =
        zstd::encode_all(format!("{header}\n").as_bytes(), 3).expect("zstd header frame");
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

    let hermes_dir = home.join(".hermes");
    fs::create_dir_all(&hermes_dir).expect("mkdir .hermes");
    let hermes_db = hermes_dir.join("state.db");
    build_hermes_db(&hermes_db);

    // openclaw:现版库与旧版 jsonl(检入 fixture)同在一个 agent 目录——两代
    // 存储并存是真机形态(doctor --fix 迁库后旧转录仍留在 sessions/)
    let claw_agent = home.join(".openclaw").join("agents").join("main");
    copy_tree(
        &fixture("openclaw/agents/main/sessions"),
        &claw_agent.join("sessions"),
    );
    let claw_dir = claw_agent.join("agent");
    fs::create_dir_all(&claw_dir).expect("mkdir openclaw agent dir");
    let openclaw_db = claw_dir.join("openclaw-agent.sqlite");
    build_openclaw_db(&openclaw_db);

    // Cursor IDE:VS Code 系的用户数据根三平台不同,与 adapter 的 storage_dir
    // 同一推导(WAKE_HOME 下派生,不走 dirs::config_dir)
    let cursor_ide_dir = cursor_ide_storage_dir(home);
    fs::create_dir_all(&cursor_ide_dir).expect("mkdir cursor globalStorage");
    let cursor_ide_db = cursor_ide_dir.join("state.vscdb");
    build_cursor_ide_db(&cursor_ide_db);

    let zcode_db = home.join(".zcode/cli/db/db.sqlite");
    fs::create_dir_all(zcode_db.parent().unwrap()).expect("mkdir .zcode/cli/db");
    build_zcode_db(&zcode_db);
    let zcode_tasks_db = home.join(".zcode/v2/tasks-index.sqlite");
    fs::create_dir_all(zcode_tasks_db.parent().unwrap()).expect("mkdir .zcode/v2");
    build_zcode_tasks_db(&zcode_tasks_db);

    let devin_db = home.join(".local/share/devin/cli/sessions.db");
    fs::create_dir_all(devin_db.parent().unwrap()).expect("mkdir devin cli dir");
    build_devin_db(&devin_db);

    Sidecars {
        copilot_db,
        opencode_db,
        opencode_next_db,
        antigravity_db,
        dsh_log,
        hermes_db,
        openclaw_db,
        cursor_ide_db,
        zcode_db,
        devin_db,
    }
}

/// `<home>/…/Cursor/User/globalStorage`,与 adapters::cursor_ide::storage_dir
/// 的平台分支逐条对应(改一处必须改另一处,契约测试会因路径不符而空列)
pub fn cursor_ide_storage_dir(home: &Path) -> PathBuf {
    let base = if cfg!(target_os = "macos") {
        home.join("Library").join("Application Support")
    } else if cfg!(target_os = "windows") {
        home.join("AppData").join("Roaming")
    } else {
        home.join(".config")
    };
    base.join("Cursor").join("User").join("globalStorage")
}

/// Hermes `state.db` 最小同构库:sessions + messages(时间戳 unix 秒 REAL)。
/// hs-0001 有用户标题、OpenAI 形状的 tool_calls 与 reasoning;hs-0002 无标题
///(回退首条用户消息)、telegram 启动面、精简形状 tool_calls(无 id,按顺位回填)、
/// 多模态 content 是 JSON 块数组;hs-0003 是 session_search 工具内部会话
///(source=tool,不列);hs-0004 零消息(不列);hs-0005 是 hs-0001 的 /branch 分支
///(parent_session_id,照常列出、挂父子关系)。
pub fn build_hermes_db(path: &Path) {
    let conn = rusqlite::Connection::open(path).expect("create hermes fixture db");
    conn.execute_batch(
        r#"
        CREATE TABLE sessions (
            id TEXT PRIMARY KEY, source TEXT NOT NULL, user_id TEXT, model TEXT,
            model_config TEXT, system_prompt TEXT, parent_session_id TEXT,
            started_at REAL NOT NULL, ended_at REAL, end_reason TEXT,
            message_count INTEGER DEFAULT 0, tool_call_count INTEGER DEFAULT 0,
            input_tokens INTEGER DEFAULT 0, output_tokens INTEGER DEFAULT 0,
            cache_read_tokens INTEGER DEFAULT 0, cache_write_tokens INTEGER DEFAULT 0,
            reasoning_tokens INTEGER DEFAULT 0, title TEXT
        );
        CREATE TABLE messages (
            id INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT NOT NULL,
            role TEXT NOT NULL, content TEXT, tool_call_id TEXT, tool_calls TEXT,
            tool_name TEXT, timestamp REAL NOT NULL, token_count INTEGER,
            finish_reason TEXT, reasoning TEXT, reasoning_details TEXT,
            codex_reasoning_items TEXT
        );
        INSERT INTO sessions (id, source, model, started_at, ended_at, message_count,
                              input_tokens, output_tokens, cache_read_tokens, title) VALUES
            ('hs-0001', 'cli', 'gpt-5.4', 1786172400.0, 1786172520.0, 4, 300, 50, 100, 'Hermes QR fix'),
            ('hs-0002', 'telegram', 'claude-opus-5', 1786176000.0, NULL, 3, 0, 0, 0, NULL),
            ('hs-0003', 'tool', 'gpt-5.4', 1786176100.0, 1786176101.0, 2, 0, 0, 0, NULL),
            ('hs-0004', 'cli', 'gpt-5.4', 1786176200.0, 1786176201.0, 0, 0, 0, 0, NULL);
        INSERT INTO sessions (id, source, model, started_at, ended_at, message_count, parent_session_id, title) VALUES
            ('hs-0005', 'cli', 'gpt-5.4', 1786176300.0, 1786176301.0, 1, 'hs-0001', 'Hermes QR fix #2');
        INSERT INTO messages (session_id, role, content, tool_call_id, tool_calls, tool_name, timestamp, reasoning) VALUES
            ('hs-0001', 'user', 'Hermes 看看二维码扫描为何闪退,是不是 useEffect() 的问题', NULL, NULL, NULL, 1786172405.0, NULL),
            ('hs-0001', 'assistant', NULL, NULL,
             '[{"id":"call_h1","type":"function","function":{"name":"read_file","arguments":"{\"path\":\"src/QrScanner.tsx\"}"}}]',
             NULL, 1786172408.0, '先读一下组件源码'),
            ('hs-0001', 'tool', 'useEffect(() => watch())', 'call_h1', NULL, 'read_file', 1786172409.0, NULL),
            ('hs-0001', 'assistant', '是依赖数组问题,我给出了修复补丁。', NULL, NULL, NULL, 1786172412.0, NULL),
            ('hs-0001', 'user', '谢谢,合并了', NULL, NULL, NULL, 1786172520.0, NULL),
            ('hs-0002', 'user', '[{"type":"text","text":"无标题会话的兜底标题应取这句"},{"type":"image_url","image_url":{"url":"file:///tmp/a.png"}}]', NULL, NULL, NULL, 1786176005.0, NULL),
            ('hs-0002', 'assistant', NULL, NULL, '[{"name":"terminal","arguments":"{\"command\":\"ls\"}"}]', NULL, 1786176006.0, NULL),
            ('hs-0002', 'tool', 'README.md', NULL, NULL, 'terminal', 1786176007.0, NULL),
            ('hs-0002', 'assistant', '好的。', NULL, NULL, NULL, 1786176008.0, NULL),
            ('hs-0003', 'user', 'internal search', NULL, NULL, NULL, 1786176100.5, NULL),
            ('hs-0005', 'user', 'branched from hs-0001', NULL, NULL, NULL, 1786176300.5, NULL),
            ('hs-0003', 'assistant', 'nothing', NULL, NULL, NULL, 1786176101.0, NULL);
        "#,
    )
    .expect("populate hermes fixture db");
}

/// OpenClaw `openclaw-agent.sqlite` 最小同构库:session_nodes + session_windows +
/// transcript_events + session_transcript_active_events(列集只取 Wake 读的)。
/// claw-0001 是活跃窗口:事件树里塞了一条被回滚的死分支(a2-dead),
/// active_events 只列可见分支;claw-0002 是被 spawn 的子代理窗口(不列);
/// claw-0003 是 reset 前的旧窗口,node 指向 claw-0001,无 active_events
///(退回树回溯),entry_json 的 totalTokens 不归它。
pub fn build_openclaw_db(path: &Path) {
    let conn = rusqlite::Connection::open(path).expect("create openclaw fixture db");
    conn.execute_batch(
        r#"
        CREATE TABLE session_nodes (
            session_key TEXT NOT NULL PRIMARY KEY, current_session_id TEXT NOT NULL,
            entry_json TEXT NOT NULL, updated_at INTEGER NOT NULL, label TEXT,
            display_name TEXT, spawned_by TEXT, parent_session_key TEXT, archived_at INTEGER
        );
        CREATE TABLE session_windows (
            session_id TEXT NOT NULL PRIMARY KEY, session_key TEXT NOT NULL,
            created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL,
            transcript_updated_at INTEGER, started_at INTEGER, model TEXT,
            model_provider TEXT, channel TEXT, chat_type TEXT, spawned_by TEXT,
            parent_session_key TEXT, display_name TEXT
        );
        CREATE TABLE transcript_events (
            session_id TEXT NOT NULL, seq INTEGER NOT NULL, event_json TEXT NOT NULL,
            created_at INTEGER NOT NULL, PRIMARY KEY (session_id, seq)
        );
        CREATE TABLE session_transcript_active_events (
            session_id TEXT NOT NULL, active_position INTEGER NOT NULL,
            event_seq INTEGER NOT NULL, message_position INTEGER,
            PRIMARY KEY (session_id, active_position)
        );
        INSERT INTO session_nodes VALUES
            ('agent:main:main', 'claw-0001',
             '{"sessionId":"claw-0001","updatedAt":1786266030000,"model":"claude-opus-5","totalTokens":7300,"label":"Node label"}',
             1786266030000, 'Node label', NULL, NULL, NULL, NULL),
            ('agent:main:subagent:sub1', 'claw-0002',
             '{"sessionId":"claw-0002","updatedAt":1786266100000,"spawnedBy":"agent:main:main"}',
             1786266100000, NULL, NULL, 'agent:main:main', 'agent:main:main', NULL);
        INSERT INTO session_windows VALUES
            ('claw-0001', 'agent:main:main', 1786266000000, 1786266030000, 1786266030000, 1786266000000,
             'claude-opus-5', 'anthropic', 'telegram', 'direct', NULL, NULL, NULL),
            ('claw-0002', 'agent:main:subagent:sub1', 1786266100000, 1786266100000, 1786266100000, 1786266100000,
             'claude-opus-5', 'anthropic', NULL, NULL, 'agent:main:main', 'agent:main:main', NULL),
            ('claw-0003', 'agent:main:main', 1786176000000, 1786176030000, 1786176030000, 1786176000000,
             'gpt-5.5', 'openai', 'cli', 'direct', NULL, NULL, NULL);
        INSERT INTO transcript_events VALUES
            ('claw-0001', 0, '{"type":"session","version":3,"id":"claw-0001","timestamp":"2026-08-08T09:00:00.000Z","cwd":"/Users/tester/Github/wakefx"}', 1786266000000),
            ('claw-0001', 1, '{"type":"message","id":"u1","parentId":null,"timestamp":"2026-08-08T09:00:05.000Z","message":{"role":"user","content":"OpenClaw 库里的会话:查二维码组件 useEffect() 清理","timestamp":1786266005000}}', 1786266005000),
            ('claw-0001', 2, '{"type":"message","id":"a1","parentId":"u1","timestamp":"2026-08-08T09:00:08.000Z","message":{"role":"assistant","content":[{"type":"toolCall","id":"call_db_1","name":"exec","arguments":{"command":"rg useEffect src/"}}],"api":"anthropic-messages","provider":"anthropic","model":"claude-opus-5","usage":{"input":100,"output":20,"cacheRead":0,"cacheWrite":0,"totalTokens":7100},"timestamp":1786266008000}}', 1786266008000),
            ('claw-0001', 3, '{"type":"message","id":"r1","parentId":"a1","timestamp":"2026-08-08T09:00:09.000Z","message":{"role":"toolResult","toolCallId":"call_db_1","toolName":"exec","content":[{"type":"text","text":"src/QrScanner.tsx: useEffect(() => watch())"}],"isError":true,"timestamp":1786266009000}}', 1786266009000),
            ('claw-0001', 4, '{"type":"message","id":"a2-dead","parentId":"r1","timestamp":"2026-08-08T09:00:11.000Z","message":{"role":"assistant","content":[{"type":"text","text":"死分支,不该出现"}],"api":"anthropic-messages","provider":"anthropic","model":"claude-opus-5","timestamp":1786266011000}}', 1786266011000),
            ('claw-0001', 5, '{"type":"message","id":"a2","parentId":"r1","timestamp":"2026-08-08T09:00:12.000Z","message":{"role":"assistant","content":[{"type":"text","text":"找到泄漏点,已补清理回调。"}],"api":"anthropic-messages","provider":"anthropic","model":"claude-opus-5","usage":{"input":200,"output":30,"cacheRead":0,"cacheWrite":0,"totalTokens":7300},"timestamp":1786266012000}}', 1786266012000),
            ('claw-0001', 6, '{"type":"message","id":"u2","parentId":"a2","timestamp":"2026-08-08T09:00:30.000Z","message":{"role":"user","content":"谢谢,合并了","timestamp":1786266030000}}', 1786266030000),
            ('claw-0002', 0, '{"type":"session","version":3,"id":"claw-0002","timestamp":"2026-08-08T09:01:40.000Z","cwd":"/Users/tester/Github/wakefx"}', 1786266100000),
            ('claw-0002', 1, '{"type":"message","id":"u1","parentId":null,"timestamp":"2026-08-08T09:01:41.000Z","message":{"role":"user","content":"child task","timestamp":1786266101000}}', 1786266101000),
            ('claw-0003', 0, '{"type":"session","version":3,"id":"claw-0003","timestamp":"2026-08-07T08:00:00.000Z","cwd":"/Users/tester/Github/wakefx"}', 1786176000000),
            ('claw-0003', 1, '{"type":"message","id":"u1","parentId":null,"timestamp":"2026-08-07T08:00:05.000Z","message":{"role":"user","content":"reset 之前的老窗口","timestamp":1786176005000}}', 1786176005000),
            ('claw-0003', 2, '{"type":"message","id":"a1","parentId":"u1","timestamp":"2026-08-07T08:00:30.000Z","message":{"role":"assistant","content":[{"type":"text","text":"收到。"}],"api":"openai-responses","provider":"openai","model":"gpt-5.5","timestamp":1786176030000}}', 1786176030000);
        INSERT INTO session_transcript_active_events VALUES
            ('claw-0001', 0, 1, 0), ('claw-0001', 1, 2, 1), ('claw-0001', 2, 3, 2),
            ('claw-0001', 3, 5, 3), ('claw-0001', 4, 6, 4);
        "#,
    )
    .expect("populate openclaw fixture db");
}

/// Copilot `session-store.db` 最小同构库:sessions + turns。
/// cop-0002 的 summary 为空,验证标题回退首条用户消息。
pub fn build_copilot_db(path: &Path) {
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
pub fn build_opencode_db(path: &Path) {
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
pub fn build_opencode_next_db(path: &Path) {
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
pub fn build_antigravity_db(path: &Path) {
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

/// Cursor IDE `state.vscdb` 最小同构库:VS Code 的两张 KV 表 + 2026 新增的
/// composerHeaders 索引表。
/// - `cide-0001`:正常会话。气泡顺序只在 `fullConversationHeadersOnly` 里,
///   KV 的 key 序(随机 UUID)与它**故意不一致**——照 key 序读会打乱对话,
///   这条是防回归的主要断言。含 thinking 气泡、工具气泡(rawArgs 形态)、
///   **只有 params 没有 rawArgs 的终端气泡**(实测占工具调用的 11%,且
///   结果是 `{"output":…}` 对象而非字符串)、空壳流式气泡
///   (Cursor 每个分片都落一条,绝大多数没有 text),一条顺序表里有、
///   KV 里已被清理的气泡,以及一条 KV 里 value 为 NULL 的气泡行(Cursor 清理
///   过的会话常见,真实库里一条就曾让整个会话解析失败)。
/// - `cide-0002`:`name` 为空且无 lastUpdatedAt,验证标题回退首条用户消息、
///   updated_at 回退末条气泡的 ISO createdAt。
/// - `cide-0003`:零气泡的草稿 composer,不进列表。
/// - `cide-0004`:子代理会话,composerHeaders 给出 parentComposerId。
/// - `33333333-…-03`:与 CLI 转录 fixture 同 id 的 IDE 副本——转录带正文,
///   scanner 按 dedup_rank 让 CLI 那份胜出(scanner_finale 有端到端)。
/// - `44444444-…-04`:转录只剩 turn_ended 空壳(fixtures/cursor 下同名文件),
///   CLI 源丢弃、IDE 副本胜出。
/// - `55555555-…-05`:scanner_finale 在测试里给它写一份截断成 `{"role":` 的
///   坏转录——过得了空壳判定、解不出消息,必须按解析失败回退到这份 IDE 副本。
pub fn build_cursor_ide_db(path: &Path) {
    let conn = rusqlite::Connection::open(path).expect("create cursor ide fixture db");
    conn.execute_batch(
        r#"
        CREATE TABLE ItemTable (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB);
        CREATE TABLE cursorDiskKV (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB);
        CREATE TABLE composerHeaders (
            composerId TEXT PRIMARY KEY, workspaceId TEXT, createdAt INTEGER,
            lastUpdatedAt INTEGER, isArchived INTEGER, isSubagent INTEGER,
            recency INTEGER, checkpointAt INTEGER, value TEXT, subagentTypeName TEXT
        );

        INSERT INTO cursorDiskKV (key, value) VALUES
          ('composerData:cide-0001',
           '{"_v":18,"composerId":"cide-0001","name":"Cursor IDE QR fix",
             "createdAt":1786300000000,"lastUpdatedAt":1786300600000,
             "workspaceIdentifier":{"id":"ws1","uri":{"fsPath":"/Users/tester/Github/wakefx","path":"/Users/tester/Github/wakefx","scheme":"file"}},
             "trackedGitRepos":[],
             "fullConversationHeadersOnly":[
               {"bubbleId":"zz-user-1","type":1,"createdAt":"2026-08-09T10:00:05.000Z"},
               {"bubbleId":"aa-think-1","type":2,"createdAt":"2026-08-09T10:00:08.000Z"},
               {"bubbleId":"mm-tool-1","type":2,"createdAt":"2026-08-09T10:00:09.000Z"},
               {"bubbleId":"nn-term-1","type":2,"createdAt":"2026-08-09T10:00:09.500Z"},
               {"bubbleId":"bb-empty-1","type":2,"createdAt":"2026-08-09T10:00:10.000Z"},
               {"bubbleId":"cc-gone-1","type":2,"createdAt":"2026-08-09T10:00:11.000Z"},
               {"bubbleId":"dd-null-1","type":2,"createdAt":"2026-08-09T10:00:11.500Z"},
               {"bubbleId":"kk-final-1","type":2,"createdAt":"2026-08-09T10:00:12.000Z"}]}'),
          ('bubbleId:cide-0001:zz-user-1',
           '{"_v":3,"type":1,"bubbleId":"zz-user-1","createdAt":"2026-08-09T10:00:05.000Z",
             "text":"Cursor IDE 看看二维码扫描为何闪退,是不是 useEffect() 的问题"}'),
          ('bubbleId:cide-0001:aa-think-1',
           '{"_v":3,"type":2,"bubbleId":"aa-think-1","createdAt":"2026-08-09T10:00:08.000Z",
             "text":"","thinking":{"text":"先查 effect 依赖和清理函数"}}'),
          ('bubbleId:cide-0001:mm-tool-1',
           '{"_v":3,"type":2,"bubbleId":"mm-tool-1","createdAt":"2026-08-09T10:00:09.000Z","text":"",
             "toolFormerData":{"toolCallId":"call_ide_1","name":"grep","status":"completed",
               "rawArgs":"{\"pattern\":\"useEffect\"}",
               "result":"src/QrScanner.tsx: useEffect(() => watch())"}}'),
          ('bubbleId:cide-0001:nn-term-1',
           '{"_v":3,"type":2,"bubbleId":"nn-term-1","createdAt":"2026-08-09T10:00:09.500Z","text":"",
             "toolFormerData":{"toolCallId":"call_ide_2","name":"run_terminal_command_v2","status":"completed",
               "rawArgs":"",
               "params":{"command":"cargo test -p wakefx","cwd":""},
               "result":{"output":"test result: ok. 3 passed","rejected":false}}}'),
          ('bubbleId:cide-0001:bb-empty-1',
           '{"_v":3,"type":2,"bubbleId":"bb-empty-1","createdAt":"2026-08-09T10:00:10.000Z","text":""}'),
          ('bubbleId:cide-0001:dd-null-1', NULL),
          ('bubbleId:cide-0001:kk-final-1',
           '{"_v":3,"type":2,"bubbleId":"kk-final-1","createdAt":"2026-08-09T10:00:12.000Z",
             "text":"找到泄漏点,已在清理回调里停止扫描。"}'),

          ('composerData:cide-0002',
           '{"_v":13,"composerId":"cide-0002","name":"",
             "createdAt":1786310000000,
             "fullConversationHeadersOnly":[
               {"bubbleId":"q1","type":1},
               {"bubbleId":"q2","type":2,"createdAt":"2026-08-09T11:00:20.000Z"}]}'),
          ('bubbleId:cide-0002:q1',
           '{"_v":3,"type":1,"bubbleId":"q1","createdAt":"2026-08-09T11:00:10.000Z",
             "text":"空 name 会话的兜底标题应取这句"}'),
          ('bubbleId:cide-0002:q2',
           '{"_v":3,"type":2,"bubbleId":"q2","createdAt":"2026-08-09T11:00:20.000Z","text":"好的。"}'),

          ('composerData:cide-0003',
           '{"_v":18,"composerId":"cide-0003","name":"Draft never sent",
             "createdAt":1786320000000,"fullConversationHeadersOnly":[]}'),

          ('composerData:cide-0004',
           '{"_v":18,"composerId":"cide-0004","name":"Explore subagent",
             "createdAt":1786330000000,"lastUpdatedAt":1786330100000,
             "workspaceIdentifier":{"id":"ws1","uri":{"fsPath":"/Users/tester/Github/wakefx","path":"/Users/tester/Github/wakefx","scheme":"file"}},
             "fullConversationHeadersOnly":[{"bubbleId":"s1","type":1}]}'),
          ('bubbleId:cide-0004:s1',
           '{"_v":3,"type":1,"bubbleId":"s1","createdAt":"2026-08-09T12:00:00.000Z","text":"child task"}'),

          ('composerData:33333333-aaaa-bbbb-cccc-000000000003',
           '{"_v":18,"composerId":"33333333-aaaa-bbbb-cccc-000000000003","name":"CLI twin",
             "createdAt":1786340000000,"lastUpdatedAt":1786340100000,
             "fullConversationHeadersOnly":[{"bubbleId":"t1","type":1}]}'),
          ('bubbleId:33333333-aaaa-bbbb-cccc-000000000003:t1',
           '{"_v":3,"type":1,"bubbleId":"t1","createdAt":"2026-08-09T13:00:00.000Z","text":"IDE 库里的同一会话"}'),

          ('composerData:44444444-aaaa-bbbb-cccc-000000000004',
           '{"_v":18,"composerId":"44444444-aaaa-bbbb-cccc-000000000004","name":"Stub twin",
             "createdAt":1786350000000,"lastUpdatedAt":1786350100000,
             "fullConversationHeadersOnly":[{"bubbleId":"u1","type":1}]}'),
          ('bubbleId:44444444-aaaa-bbbb-cccc-000000000004:u1',
           '{"_v":3,"type":1,"bubbleId":"u1","createdAt":"2026-08-09T14:00:00.000Z","text":"只有空壳转录的会话"}'),

          ('composerData:55555555-aaaa-bbbb-cccc-000000000005',
           '{"_v":18,"composerId":"55555555-aaaa-bbbb-cccc-000000000005","name":"Corrupt twin",
             "createdAt":1786360000000,"lastUpdatedAt":1786360100000,
             "fullConversationHeadersOnly":[{"bubbleId":"v1","type":1}]}'),
          ('bubbleId:55555555-aaaa-bbbb-cccc-000000000005:v1',
           '{"_v":3,"type":1,"bubbleId":"v1","createdAt":"2026-08-09T15:00:00.000Z","text":"转录已损坏的会话"}');

        INSERT INTO composerHeaders
            (composerId, workspaceId, createdAt, lastUpdatedAt, isArchived, isSubagent, recency, checkpointAt, value, subagentTypeName)
        VALUES
          ('cide-0001','ws1',1786300000000,1786300600000,0,0,1786300600000,NULL,
           '{"type":"head","composerId":"cide-0001","name":"Cursor IDE QR fix"}',NULL),
          ('cide-0004','ws1',1786330000000,1786330100000,0,1,1786330100000,NULL,
           '{"type":"head","composerId":"cide-0004","name":"Explore subagent","subagentInfo":{"subagentType":3,"parentComposerId":"cide-0001","subagentTypeName":"explore"}}','explore');
        "#,
    )
    .expect("populate cursor ide fixture db");
}

/// 让本进程(及其子进程)的全部 adapter 只看这个假 HOME:WAKE_HOME 是 adapter
/// 侧的统一改道开关,三端一致;HOME 仍设一份供其他 POSIX 依赖(Windows 上 dirs
/// 不看 HOME,单设它等于没设)。再清掉各家的 env 根覆盖——开发机或 CI 上设了
/// CODEX_HOME / XDG_DATA_HOME 之类,adapter 就会绕过 fixture 去读真实库(实测
/// opencode 的两个契约测试会因此挂掉)。新增带 env 根的 agent 只改这里
pub fn isolate_home(home: &Path) {
    std::env::set_var("WAKE_HOME", home);
    std::env::set_var("HOME", home);
    clear_agent_env_overrides();
}

/// 只清 env 根覆盖、不动 HOME(live 远程用例要保留 ~/.ssh)
pub fn clear_agent_env_overrides() {
    for var in [
        "XDG_DATA_HOME",
        "XDG_CONFIG_HOME",
        "CODEX_HOME",
        "QODER_CONFIG_DIR",
        "HERMES_HOME",
        "OPENCLAW_STATE_DIR",
        "CODEBUDDY_CONFIG_DIR",
        "WORKBUDDY_CONFIG_DIR",
        "ZCODE_STORAGE_DIR",
    ] {
        std::env::remove_var(var);
    }
}

/// ZCode `~/.zcode` 最小同构:cli/db/db.sqlite(OpenCode 形状,session 表按真机
/// 3.14.0 / 运行时 0.16.9 的列)+ v2/tasks-index.sqlite(只有过滤要看的列)。
/// zc-0003 桌面端软删、zc-0004 是向导从 Claude Code 导入的、zc-0005 是
/// subagent_child,三条都不该列;zc-0008 是 fork(带 parent_id 但是用户自己的
/// 对话)要列;zc-0002 的标题是占位(title_source=default)且首条是注入上下文;
/// zc-0001 末尾有一条 compaction 摘要(user 角色、hidden);zc-0006 是没有
/// semantics 的老写端;zc-0007 已归档
pub fn build_zcode_db(path: &Path) {
    let conn = rusqlite::Connection::open(path).expect("create zcode fixture db");
    conn.execute_batch(
        r#"
        CREATE TABLE session (
            id TEXT PRIMARY KEY, project_id TEXT NOT NULL, workspace_id TEXT, parent_id TEXT,
            slug TEXT NOT NULL, directory TEXT NOT NULL, path TEXT, title TEXT NOT NULL,
            version TEXT NOT NULL, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL,
            time_archived INTEGER, task_type TEXT NOT NULL DEFAULT 'interactive',
            title_source TEXT NOT NULL DEFAULT 'first_input'
        );
        CREATE TABLE message (
            id TEXT PRIMARY KEY, session_id TEXT NOT NULL, time_created INTEGER NOT NULL,
            time_updated INTEGER NOT NULL, data TEXT NOT NULL, sequence INTEGER
        );
        CREATE TABLE part (
            id TEXT PRIMARY KEY, message_id TEXT NOT NULL, session_id TEXT NOT NULL,
            time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT NOT NULL,
            sequence INTEGER
        );
        INSERT INTO session (id, project_id, workspace_id, parent_id, slug, directory, path, title,
                             version, time_created, time_updated, time_archived, task_type, title_source) VALUES
            ('zc-0001','proj-wakefx',NULL,NULL,'zc-0001','/Users/tester/Github/wakefx','/Users/tester/Github/wakefx','ZCode QR fix','0.16.9',1789000000000,1789000060000,NULL,'interactive','generated'),
            ('zc-0002','proj-wakefx',NULL,NULL,'zc-0002','/Users/tester/Github/wakefx','/Users/tester/Github/wakefx','New task','0.16.9',1789000100000,1789000110000,NULL,'interactive','default'),
            ('zc-0003','proj-wakefx',NULL,NULL,'zc-0003','/Users/tester/Github/wakefx',NULL,'deleted in desktop','0.16.9',1789000200000,1789000210000,NULL,'interactive','first_input'),
            ('zc-0004','proj-wakefx',NULL,NULL,'zc-0004','/Users/tester/Github/wakefx',NULL,'imported from claude','0.16.9',1789000300000,1789000310000,NULL,'interactive','first_input'),
            ('zc-0005','proj-wakefx',NULL,'zc-0001','zc-0005','/Users/tester/Github/wakefx',NULL,'child task','0.16.9',1789000400000,1789000410000,NULL,'subagent_child','first_input'),
            ('zc-0008','proj-wakefx',NULL,'zc-0001','zc-0008','/Users/tester/Github/wakefx',NULL,'Fork of ZCode QR fix','0.16.9',1789000800000,1789000810000,NULL,'fork','generated'),
            ('zc-0006','proj-wakefx',NULL,NULL,'zc-0006','/Users/tester/Github/wakefx',NULL,'老写端没有 semantics','0.15.2',1789000500000,1789000510000,NULL,'interactive','first_input'),
            ('zc-0007','proj-wakefx',NULL,NULL,'zc-0007','/Users/tester/Github/wakefx',NULL,'archived one','0.16.9',1789000600000,1789000610000,1789000700000,'interactive','first_input');
        INSERT INTO message (id, session_id, time_created, time_updated, data, sequence) VALUES
            ('m-0001-0','zc-0001',1789000000000,1789000000000,'{"role":"user","time":{"created":1789000000000},"agent":"zcode-agent","modelSelection":{"providerId":"account:zai","modelId":"GLM-5.3"},"semantics":{"origin":"real_user","kind":"user_prompt","transcriptVisibility":"visible"}}',0),
            ('m-0001-1','zc-0001',1789000005000,1789000012000,'{"role":"assistant","time":{"created":1789000005000,"completed":1789000012000},"parentID":"m-0001-0","modelId":"GLM-5.3","providerId":"account:zai","tokens":{"total":120,"input":100,"output":20,"reasoning":0,"cache":{"read":0,"write":0}},"finish":"stop","semantics":{"origin":"agent_runtime","kind":"assistant_response"}}',1),
            ('m-0001-2','zc-0001',1789000050000,1789000050000,'{"role":"user","time":{"created":1789000050000},"modelSelection":{"providerId":"account:zai","modelId":"GLM-5.3-Flash"},"semantics":{"origin":"real_user","kind":"user_prompt"}}',2),
            ('m-0001-3','zc-0001',1789000055000,1789000060000,'{"role":"assistant","time":{"created":1789000055000,"completed":1789000060000},"modelId":"GLM-5.3-Flash","tokens":{"total":30,"input":25,"output":5,"reasoning":0,"cache":{"read":0,"write":0}},"finish":"stop","semantics":{"origin":"agent_runtime","kind":"assistant_response"}}',3),
            ('m-0001-4','zc-0001',1789000058000,1789000058000,'{"role":"user","time":{"created":1789000058000},"semantics":{"origin":"agent_runtime","kind":"compact_summary","uiVisibility":"hidden","transcriptVisibility":"hidden"}}',4),
            ('m-0008-0','zc-0008',1789000800000,1789000800000,'{"role":"user","time":{"created":1789000800000},"semantics":{"origin":"real_user","kind":"user_prompt"}}',0),
            ('m-0002-0','zc-0002',1789000100000,1789000100000,'{"role":"user","time":{"created":1789000100000},"semantics":{"origin":"system_injected","kind":"context","transcriptVisibility":"hidden"}}',0),
            ('m-0002-1','zc-0002',1789000101000,1789000101000,'{"role":"user","time":{"created":1789000101000},"semantics":{"origin":"real_user","kind":"user_prompt"}}',1),
            ('m-0002-2','zc-0002',1789000105000,1789000110000,'{"role":"assistant","time":{"created":1789000105000},"modelId":"GLM-5.3","tokens":{"total":10},"semantics":{"origin":"agent_runtime","kind":"assistant_response"}}',2),
            ('m-0003-0','zc-0003',1789000200000,1789000200000,'{"role":"user","time":{"created":1789000200000},"semantics":{"origin":"real_user"}}',0),
            ('m-0004-0','zc-0004',1789000300000,1789000300000,'{"role":"user","time":{"created":1789000300000},"semantics":{"origin":"real_user"}}',0),
            ('m-0005-0','zc-0005',1789000400000,1789000400000,'{"role":"user","time":{"created":1789000400000},"semantics":{"origin":"real_user"}}',0),
            ('m-0006-0','zc-0006',1789000500000,1789000500000,'{"role":"user","time":{"created":1789000500000}}',0),
            ('m-0006-1','zc-0006',1789000505000,1789000510000,'{"role":"assistant","time":{"created":1789000505000},"modelId":"GLM-5.3","tokens":{"total":5}}',1),
            ('m-0007-0','zc-0007',1789000600000,1789000600000,'{"role":"user","time":{"created":1789000600000},"semantics":{"origin":"real_user"}}',0);
        INSERT INTO part (id, message_id, session_id, time_created, time_updated, data, sequence) VALUES
            ('p-0001-0-0','m-0001-0','zc-0001',1789000000000,1789000000000,'{"type":"text","text":"ZCode 看看二维码扫描为何闪退,是不是 useEffect() 的问题","time":{"start":1789000000000,"end":1789000000000}}',0),
            ('p-0001-1-0','m-0001-1','zc-0001',1789000005000,1789000005000,'{"type":"step-start"}',0),
            ('p-0001-1-1','m-0001-1','zc-0001',1789000005500,1789000006000,'{"type":"reasoning","text":"先读一下组件源码","metadata":{"anthropic":{"signature":"abc"}},"time":{"start":1789000005500,"end":1789000006000}}',1),
            ('p-0001-1-2','m-0001-1','zc-0001',1789000006000,1789000007000,'{"type":"tool","callID":"call_0001","declarationIndex":0,"tool":"Bash","state":{"status":"completed","input":{"command":"rg useEffect src/QrScanner.tsx","description":"Find the hook"},"output":"src/QrScanner.tsx:12: useEffect(() => {","title":"Bash","time":{"start":1789000006000,"end":1789000007000}}}',2),
            ('p-0001-1-3','m-0001-1','zc-0001',1789000008000,1789000012000,'{"type":"text","text":"是依赖数组问题,我给出了修复补丁。","time":{"start":1789000008000,"end":1789000012000}}',3),
            ('p-0001-1-4','m-0001-1','zc-0001',1789000012000,1789000012000,'{"type":"step-finish","reason":"stop","cost":0,"tokens":{"total":120,"input":100,"output":20,"reasoning":0,"cache":{"read":0,"write":0}}}',4),
            ('p-0001-2-0','m-0001-2','zc-0001',1789000050000,1789000050000,'{"type":"text","text":"谢谢,合并了"}',0),
            ('p-0001-3-0','m-0001-3','zc-0001',1789000055000,1789000060000,'{"type":"text","text":"不客气。"}',0),
            ('p-0001-4-0','m-0001-4','zc-0001',1789000058000,1789000058000,'{"type":"text","text":"Summary of the conversation so far: fixed the qr login bug."}',0),
            ('p-0008-0-0','m-0008-0','zc-0008',1789000800000,1789000800000,'{"type":"text","text":"forked follow-up"}',0),
            ('p-0002-0-0','m-0002-0','zc-0002',1789000100000,1789000100000,'{"type":"text","text":"<workspace><root>/Users/tester/Github/wakefx</root></workspace>"}',0),
            ('p-0002-1-0','m-0002-1','zc-0002',1789000101000,1789000101000,'{"type":"text","text":"空标题会话取这句"}',0),
            ('p-0002-2-0','m-0002-2','zc-0002',1789000105000,1789000110000,'{"type":"text","text":"好的。"}',0),
            ('p-0003-0-0','m-0003-0','zc-0003',1789000200000,1789000200000,'{"type":"text","text":"deleted"}',0),
            ('p-0004-0-0','m-0004-0','zc-0004',1789000300000,1789000300000,'{"type":"text","text":"imported"}',0),
            ('p-0005-0-0','m-0005-0','zc-0005',1789000400000,1789000400000,'{"type":"text","text":"child"}',0),
            ('p-0006-0-0','m-0006-0','zc-0006',1789000500000,1789000500000,'{"type":"text","text":"老写端没有 semantics"}',0),
            ('p-0006-1-0','m-0006-1','zc-0006',1789000505000,1789000510000,'{"type":"text","text":"在。"}',0),
            ('p-0007-0-0','m-0007-0','zc-0007',1789000600000,1789000600000,'{"type":"text","text":"archived"}',0);
        "#,
    )
    .expect("populate zcode fixture db");
}

/// tasks-index:只有过滤要看的列
pub fn build_zcode_tasks_db(path: &Path) {
    let conn = rusqlite::Connection::open(path).expect("create zcode tasks fixture db");
    conn.execute_batch(
        r#"
        CREATE TABLE tasks (
            task_id TEXT PRIMARY KEY, migration_source TEXT,
            deleted INTEGER NOT NULL DEFAULT 0
        );
        INSERT INTO tasks (task_id, migration_source, deleted) VALUES
            ('zc-0001',NULL,0),
            ('zc-0003',NULL,1),
            ('zc-0004','claudeCode',0);
        "#,
    )
    .expect("populate zcode tasks fixture db");
}

/// Devin `<root>/cli/sessions.db` 最小同构库(列集按真机写端):
/// sessions + message_nodes(node_id/parent_node_id 森林,chat_message JSON,
/// unix 秒时间戳)。dv-0001 是正常会话:main_chain_id 指向 n4,n5 是挂在
/// n1 上的重试侧枝——既不该进转录,它的 metrics 也不该进 token 累计;
/// dv-0002 标题为空(回退首条真人消息),带 system 注入、cache_keepalive
/// 心跳、compaction 请求与 <summary> 应答;dv-0003 是 hidden 会话不列;
/// dv-0004 零正文不列
pub fn build_devin_db(path: &Path) {
    let conn = rusqlite::Connection::open(path).expect("create devin fixture db");
    conn.execute_batch(
        r#"
        CREATE TABLE sessions (
            id TEXT PRIMARY KEY, working_directory TEXT NOT NULL, backend_type TEXT NOT NULL,
            model TEXT NOT NULL, agent_mode TEXT NOT NULL, created_at INTEGER NOT NULL,
            last_activity_at INTEGER NOT NULL, title TEXT, main_chain_id INTEGER,
            hidden INTEGER NOT NULL DEFAULT 0, metadata TEXT
        );
        CREATE TABLE message_nodes (
            row_id INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT NOT NULL,
            node_id INTEGER NOT NULL, parent_node_id INTEGER,
            chat_message TEXT NOT NULL, created_at INTEGER NOT NULL, metadata TEXT,
            UNIQUE(session_id, node_id)
        );
        INSERT INTO sessions (id, working_directory, backend_type, model, agent_mode,
                              created_at, last_activity_at, title, main_chain_id, hidden) VALUES
            ('dv-0001','/Users/tester/Github/wakefx','windsurf','swe-2-max','auto',1789000000,1789000060,'Devin QR fix',4,0),
            ('dv-0002','/Users/tester/Github/wakefx','windsurf','swe-2-max','auto',1789000100,1789000110,'',6,0),
            ('dv-0003','/Users/tester/Github/wakefx','windsurf','swe-2-max','auto',1789000200,1789000210,'hidden helper',1,1),
            ('dv-0004','/Users/tester/Github/wakefx','windsurf','swe-2-max','auto',1789000300,1789000300,'empty',NULL,0);
        INSERT INTO message_nodes (session_id, node_id, parent_node_id, chat_message, created_at) VALUES
            ('dv-0001',1,NULL,'{"message_id":"u1","role":"user","content":"Devin 看看二维码扫描为何闪退,是不是 useEffect() 的问题","metadata":{"is_user_input":true,"telemetry":{"source":"user"}}}',1789000005),
            ('dv-0001',2,1,'{"message_id":"a1","role":"assistant","content":"","thinking":{"thinking":"先读一下组件源码","signature":"sealed.v1.xyz","signature_type":"sealed"},"tool_calls":[{"id":"exec_0","name":"exec","arguments":{"command":"rg useEffect src/QrScanner.tsx"},"index":0,"kind":"function"}],"metadata":{"generation_model":"swe-2-high","metrics":{"input_tokens":200,"output_tokens":20,"cache_read_tokens":800,"cache_creation_tokens":null}}}',1789000008),
            ('dv-0001',3,2,'{"message_id":"t1","role":"tool","content":"src/QrScanner.tsx:12: useEffect(() => watch())","tool_call_id":"exec_0","metadata":{}}',1789000009),
            ('dv-0001',4,3,'{"message_id":"a2","role":"assistant","content":"是依赖数组问题,我给出了修复补丁。","metadata":{"generation_model":"swe-2-max","metrics":{"input_tokens":300,"output_tokens":40}}}',1789000012),
            ('dv-0001',5,1,'{"message_id":"a3-retry","role":"assistant","content":"重试侧枝的另一种回答,不该进主链","metadata":{"generation_model":"swe-2-max","metrics":{"input_tokens":500,"output_tokens":50}}}',1789000020),
            ('dv-0002',1,NULL,'{"message_id":"s1","role":"system","content":"<system_info>\nworkspace context\n</system_info>","metadata":{"telemetry":{"source":"system"}}}',1789000101),
            ('dv-0002',2,1,'{"message_id":"u0","role":"user","content":"continue","metadata":{"telemetry":{"source":"cache_keepalive"}}}',1789000102),
            ('dv-0002',3,2,'{"message_id":"u1","role":"user","content":"Conversation to summarize:\n=== MESSAGE 0 - User ===\n二维码","metadata":{"is_user_input":null,"telemetry":{"source":"user"}}}',1789000103),
            ('dv-0002',4,3,'{"message_id":"a1","role":"assistant","content":"<summary>\n## Overview\n之前聊过二维码扫描的修复。\n</summary>","metadata":{"telemetry":{"source":"assistant"}}}',1789000104),
            ('dv-0002',5,4,'{"message_id":"u2","role":"user","content":"空标题会话取这句","metadata":{"is_user_input":true,"telemetry":{"source":"user"}}}',1789000105),
            ('dv-0002',6,5,'{"message_id":"a2","role":"assistant","content":"好的。","metadata":{"generation_model":"swe-2-max","metrics":{"input_tokens":50,"output_tokens":10}}}',1789000110),
            ('dv-0003',1,NULL,'{"message_id":"u1","role":"user","content":"Output a summary from the following messages","metadata":{"is_user_input":true,"telemetry":{"source":"user"}}}',1789000205);
        "#,
    )
    .expect("populate devin fixture db");
}

/// 以某种身份持住索引锁,拿不到就 panic 点名——各测试文件里"扮演 GUI / 另一个
/// wake-cli"的同一句
pub fn lock_as(db: &Path, kind: &str) -> IndexLock {
    // 并行的别条测试正在 fork 子进程的那几微秒里,本进程所有 fd(含刚放掉的锁)会被复制一份
    // 直到 exec 关掉——锁因此晚放一瞬。真实使用没有这形态(GUI 持锁到退出),测试里等一下
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        match IndexLock::try_acquire(db, kind).unwrap() {
            Ownership::Ours(lock) => return lock,
            Ownership::Held(_) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Ownership::Held(who) => panic!("a fresh lock should not be held by {who}"),
        }
    }
}
