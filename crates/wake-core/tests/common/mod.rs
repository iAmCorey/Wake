//! 集成测试共用的 fixture 搭建:合成 fixtures → 一个像真机 home 的目录树。
//! adapter_contracts 用它建假 HOME,remote_sync 用它建"远端 home"——同一份
//! 侧档/SQLite 库/dsh zstd 压制,两边不会各自漂移。fixture 全合成,绝不放
//! 真实会话数据。
#![allow(dead_code)] // 每个测试二进制只用到其中一部分

use std::fs;
use std::path::{Path, PathBuf};

pub fn fixture(rel: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(rel)
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

    Sidecars {
        copilot_db,
        opencode_db,
        opencode_next_db,
        antigravity_db,
        dsh_log,
    }
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
