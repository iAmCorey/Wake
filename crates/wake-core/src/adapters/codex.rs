use super::parse_utils::*;
use super::sqlite_ro::{open_sqlite_ro, virtual_path};
use super::AgentAdapter;
use crate::models::*;
use anyhow::Result;
use rusqlite::Connection;
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

pub struct CodexAdapter {
    sessions_dir: PathBuf,
    archived_dir: PathBuf,
    state_db: PathBuf,
    scan_sessions: bool,
    scan_archived: bool,
    links_cache: MtimeCache<Vec<(String, String)>>,
    /// CODEX_HOME(memories/ 与 memories_1.sqlite 的所在);自定义根选的是独立的
    /// sessions 目录或裸 rollout 拷贝时没有 home——侧档绝不越界摸父目录(不变量 8⑨)
    home: Option<PathBuf>,
    /// memories/*.md 按目录指纹缓存、memories_1.sqlite 的线程记忆按库戳缓存
    /// (每轮扫描都会列,没变不重读)
    memories: super::MemoryCache,
    thread_memories: MtimeCache<Vec<MemoryDoc>>,
}

impl CodexAdapter {
    pub fn new() -> Self {
        // codex 认 CODEX_HOME(kooky 的 CodexUsageMonitor 同样处理)。
        // 采信前探真会话目录而不是只看根目录在不在——后者会让 CODEX_HOME
        // 指向空目录的机器整家会话凭空消失(与 opencode 探库文件同一规则)
        let root = super::env_dir("CODEX_HOME")
            .filter(|p| p.join("sessions").is_dir() || p.join("archived_sessions").is_dir())
            .unwrap_or_else(|| super::home_dir().unwrap_or_default().join(".codex"));
        Self {
            sessions_dir: root.join("sessions"),
            archived_dir: root.join("archived_sessions"),
            state_db: root.join("state_5.sqlite"),
            scan_sessions: true,
            scan_archived: true,
            links_cache: MtimeCache::new(),
            home: Some(root),
            memories: super::MemoryCache::new(),
            thread_memories: MtimeCache::new(),
        }
    }
}

/// 路径末段,`/` 与 `\` 都算分隔符(state DB 可能是 Windows 机器写的)
fn rollout_file_name(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

/// Codex 把它启动的**每一条**线程都写进同一棵 rollout 树:用户线程之外还有
/// guardian auto-review、`/review`、compaction、memory consolidation 与
/// `spawn_agent` 子代理。它自家的历史只列交互来源(cli/vscode/atlas/chatgpt),
/// Wake 则在 adapter 文件边界上把这棵树分成三支——UI、FTS、MCP 共用同一份
/// 索引,不必各猜一次(issue #30、#42)。
///
/// 判据只读首行 session_meta。`source` 是对象即结构化来源:`subagent` 的
/// review/compact/memory_consolidation/other 与 `internal` 的
/// guardian/memory_consolidation 全是噪音,**只有 `subagent.thread_spawn`
/// 例外**——那是用户自己在会话里派出去的活,进索引,再由 `parent_links`
/// 挂回父线程(issue #42)。字符串来源(cli/vscode/exec/mcp/unknown/自定义)
/// 是普通用户线程;`thread_source` 另认 subagent/guardian_review/
/// memory_consolidation 三个内部值(issue #30 里的过渡格式只带这个字段),
/// 它分不出 thread_spawn 与噪音,保守归内部。上游落盘时把 Internal(Guardian)
/// 改写成 SubAgent(Other("guardian")),两套写法都要认。`thread_source` 的其余
/// 值是 `codex exec --thread-source` 给自动化打的 Feature 标签,走 exec 这条
/// Wake 本就展示的路,照常保留。首行读不出、不是 JSON 或不是 session_meta
/// (2025 老格式)一律保守放行:半写入或未来格式不能让真实会话消失。review
/// 子线程的结果本就经 `<user_action>` 注入父会话渲染。
enum ThreadKind {
    /// 交互来源,或判不出用途的首行:照常作为顶层会话
    User,
    /// 整族不进索引
    Internal,
    /// 进索引,但挂在父线程下。**resume 照给**:`codex resume <SESSION_ID>`
    /// 收任何记录在案的线程 id,子线程在 Codex 自己的 state DB 里就是一条
    /// 记录(它家 picker 另有 `--all` 才列全),所以不像 OpenClaw/Kiro 那样
    /// 返回 None。kooky 认不认这条 id 没在真机验过,而 `open kooky://…` 一律
    /// 退 0(不变量 7),那条路上的失败看不出来
    Spawned(SpawnMeta),
}

/// `source.subagent.thread_spawn` 里给子线程起的名字。标题只能从这里来——
/// 派活的 payload 是加密的,子线程自己没有任何可用的用户消息。父子关系
/// **不从这里取**:同一个对象里的 `parent_thread_id` 只有读过这个文件才知道,
/// 拿它当关系来源会让 `parent_links` 变成"本进程迄今扫过谁"的函数;关系统一
/// 问 state DB 的登记表(见 `parent_links`)
struct SpawnMeta {
    agent_path: String,
    agent_nickname: String,
}

impl SpawnMeta {
    fn from_value(value: &Value) -> Self {
        let text = |key: &str| optional_string(value.get(key)).unwrap_or_default();
        Self {
            agent_path: text("agent_path"),
            agent_nickname: text("agent_nickname"),
        }
    }

    /// 标题取用户在 `spawn_agent(task_name=…)` 里起的名字:`agent_path` 是
    /// `/root/<task_name>` 的路径形态,末段即任务名。缺了退回上游随机起的
    /// 昵称(agent_nickname),两者都没有才交回 UNTITLED
    fn title(&self) -> Option<String> {
        let task = self.agent_path.rsplit('/').next().unwrap_or_default();
        let pick = if task.is_empty() {
            &self.agent_nickname
        } else {
            task
        };
        (!pick.is_empty()).then(|| pick.to_string())
    }
}

fn read_thread_kind(path: &Path) -> ThreadKind {
    let Ok(file) = fs::File::open(path) else {
        return ThreadKind::User;
    };
    // payload 还带 base_instructions:实测首行中位数 18 KB、最大 49 KB
    // (2026-09-15),预留容量免得 read_line 反复倍增
    let mut first_line = String::with_capacity(1 << 16);
    if BufReader::new(file).read_line(&mut first_line).unwrap_or(0) == 0 {
        return ThreadKind::User;
    }
    let Ok(head) = serde_json::from_str::<SessionMetaHead>(&first_line) else {
        return ThreadKind::User;
    };
    if head.kind != "session_meta" {
        return ThreadKind::User;
    }
    let Some(payload) = head.payload else {
        return ThreadKind::User;
    };
    thread_kind(payload.source.as_ref(), payload.thread_source.as_deref())
}

/// 首行的两个键 → 线程身份。`read_thread_kind`(窄结构反序列化)与 `parse_rollout`
/// (整行 Value)都走这里,判据不可能分家
fn thread_kind(source: Option<&Value>, thread_source: Option<&str>) -> ThreadKind {
    if let Some(source) = source.filter(|source| source.is_object()) {
        return match source.pointer("/subagent/thread_spawn") {
            Some(spawn) => ThreadKind::Spawned(SpawnMeta::from_value(spawn)),
            None => ThreadKind::Internal,
        };
    }
    if matches!(
        thread_source,
        Some("subagent" | "guardian_review" | "memory_consolidation")
    ) {
        return ThreadKind::Internal;
    }
    ThreadKind::User
}

/// `thread_kind` 只看的两个键。按窄结构反序列化,serde 跳过
/// base_instructions 那几十 KB 而不是整棵 Value 建起来再丢——别换回 Value
#[derive(Deserialize)]
struct SessionMetaHead {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    payload: Option<SessionMetaPayloadHead>,
}

#[derive(Deserialize)]
struct SessionMetaPayloadHead {
    /// 字符串 = 用户线程来源;对象 = subagent / internal 结构化来源(本身很小)
    #[serde(default)]
    source: Option<Value>,
    #[serde(default)]
    thread_source: Option<String>,
}

/// rollout 存储本体(用户选中的是数据目录而非 codex home):自身不含
/// sessions/archived 子目录,且顶层有 YYYY 日期目录(sessions 树)**或**
/// 平铺的 rollout-*.jsonl(archived_sessions 的真实布局,实测平铺)。
/// with_custom_root 与 normalize_custom_root 共用同一判据
fn is_rollout_store(dir: &Path) -> bool {
    !dir.join("sessions").is_dir()
        && !dir.join("archived_sessions").is_dir()
        && std::fs::read_dir(dir)
            .map(|rd| {
                rd.flatten().any(|e| {
                    let name = e.file_name();
                    let Some(n) = name.to_str() else { return false };
                    (e.path().is_dir() && n.len() == 4 && n.bytes().all(|b| b.is_ascii_digit()))
                        || (n.starts_with("rollout-") && n.ends_with(".jsonl"))
                })
            })
            .unwrap_or(false)
}

/// 各家归一化的 codex 实现(mod.rs 静态分派,**不依赖 roster 在场**):
/// 直选数据目录且父目录呈 home 形态 → 存父目录,state DB 与两个数据根全部
/// 找回。"数据目录"认两种证据:内容形态(rollout 存储),或目录名本身就是
/// sessions/archived_sessions——空目录没有内容证据,但表单允许空路径,真实
/// 的空 sessions 目录同样该上提(2026-08-24 Codex review 两轮)
pub(crate) fn normalize_custom_root(dir: PathBuf) -> PathBuf {
    let name = dir.file_name().and_then(|n| n.to_str());
    let looks_data_dir =
        is_rollout_store(&dir) || matches!(name, Some("sessions") | Some("archived_sessions"));
    if looks_data_dir {
        if let Some(parent) = dir.parent() {
            // home 证据必须独立于被选目录自身:目录名恰为 sessions 时,
            // parent/sessions 就是它自己,不能算证据
            let sibling = match name {
                Some("sessions") => parent.join("archived_sessions").is_dir(),
                Some("archived_sessions") => parent.join("sessions").is_dir(),
                _ => parent.join("sessions").is_dir() || parent.join("archived_sessions").is_dir(),
            };
            if parent.join("state_5.sqlite").is_file() || sibling {
                return parent.to_path_buf();
            }
        }
    }
    dir
}

#[derive(Debug)]
struct ThreadRow {
    id: String,
    rollout_path: String,
    cwd: String,
    title: String,
    name: Option<String>,
    tokens_used: Option<i64>,
    archived: bool,
    git_branch: Option<String>,
    model: Option<String>,
    source: Option<String>,
    created_at_ms: Option<i64>,
    updated_at_ms: Option<i64>,
}

/// 只读读取 Codex state DB(三级梯度统一走 sqlite_ro,绝不写、绝不 immutable=1)
fn read_threads(state_db: &Path) -> Option<Vec<ThreadRow>> {
    let query = |conn: &Connection| -> rusqlite::Result<Vec<ThreadRow>> {
        let mut stmt = conn.prepare(
            "SELECT id, rollout_path, cwd, title, name, tokens_used, archived,
                    git_branch, model, source, created_at_ms, updated_at_ms
             FROM threads",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(ThreadRow {
                id: r.get(0)?,
                rollout_path: r.get(1)?,
                cwd: r.get(2)?,
                title: r.get(3)?,
                name: r.get(4)?,
                tokens_used: r.get(5)?,
                archived: r.get::<_, i64>(6)? == 1,
                git_branch: r.get(7)?,
                model: r.get(8)?,
                source: r.get(9)?,
                created_at_ms: r.get(10)?,
                updated_at_ms: r.get(11)?,
            })
        })?;
        rows.collect()
    };
    let ro = open_sqlite_ro(state_db, "codex")?;
    query(&ro.conn).ok()
}

/// state DB 的 spawn 登记表(child → parent)。status 列("open"/…)不参与:
/// 任务跑没跑完不改变归属。
///
/// **`None` 与 `Some(空)` 是两件事**,调用方据此决定缓不缓存:没有 state DB、
/// 老库没有这张表 = 确定没有关系(`Some(空)`);库在但打不开、读到一半出错 =
/// 不知道(`None`)。混成一个值的后果是 `sync_parent_links` 把"读不出来"当成
/// "关系已解除",`replace_parent_links` 一句 `UPDATE … SET parent_key=''`
/// 把整家的父子关系清空,而错误答案还被按 mtime 戳缓存下来、不自愈
fn read_spawn_edges(state_db: &Path) -> Option<HashMap<String, String>> {
    if !state_db.is_file() {
        return Some(HashMap::new());
    }
    let ro = open_sqlite_ro(state_db, "codex")?;
    // 缺表时 prepare 就失败(no such table)——那是老库,不是读不出来
    let Ok(mut stmt) = ro
        .conn
        .prepare("SELECT child_thread_id, parent_thread_id FROM thread_spawn_edges")
    else {
        return Some(HashMap::new());
    };
    let rows = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        .ok()?;
    // 逐行 Err(列类型变了、读到一半 SQLITE_BUSY)不能 flatten 掉:少几条边
    // 与"这几条边被删了"在下游无法区分,同样会解挂那几条子会话
    rows.collect::<rusqlite::Result<HashMap<_, _>>>().ok()
}

struct CodexParse {
    messages: Vec<TranscriptMessage>,
    cwd: String,
    git_branch: Option<String>,
    model: Option<String>,
    /// rollout 首行 originator 的友好名。state DB 的 source 列会把
    /// Codex Desktop 与 IDE 扩展都归为 "vscode",originator 才分得开。
    source: Option<String>,
    tokens_used: i64,
    created_at: i64,
    updated_at: i64,
    unknown_lines: u32,
    /// 非 None = 这是 `spawn_agent` 起的子线程
    spawn: Option<SpawnMeta>,
}

fn friendly_source(originator: &str) -> Option<String> {
    Some(match originator {
        "codex_cli_rs" | "codex-tui" => "CLI".to_string(),
        "codex_exec" => "exec".to_string(),
        "codex_vscode" => "IDE extension".to_string(),
        "codex_work_desktop" => "Codex Desktop".to_string(),
        "" => return None,
        other => other.to_string(), // "Codex Desktop"、"Claude Code" 等原样
    })
}

/// Codex Desktop 把一次 review 同时写成注入式 `<user_action>` 与
/// `ExitedReviewMode.review_output`。前者只作结构化事件的文本兜底，不应以
/// “用户消息 + XML 外壳”的形态出现在详情里。
fn review_results_from_user_action(text: &str) -> Option<String> {
    let text = text.trim();
    if !text.starts_with("<user_action") || extract_tag(text, "action")?.trim() != "review" {
        return None;
    }
    let results = extract_tag(text, "results")?;
    (!results.trim().is_empty()).then(|| results.trim().to_string())
}

fn review_confidence(value: Option<&Value>) -> Option<String> {
    let value = value?.as_f64()?;
    let percent = if (0.0..=1.0).contains(&value) {
        value * 100.0
    } else {
        value
    };
    percent.is_finite().then(|| format!("{percent:.0}%"))
}

/// review 的机器 JSON 转成详情页 Markdown。严格要求 `findings` 数组，避免
/// 把用户恰好贴出的普通 JSON 误判成 review。
fn format_review_output(review: &Value) -> Option<String> {
    let findings = review.get("findings")?.as_array()?;
    let mut out = String::from("## Code review\n\n");

    let verdict = review
        .get("overall_correctness")
        .and_then(Value::as_str)
        .map(|value| match value {
            "patch is correct" => "Passed",
            "patch is incorrect" => "Changes requested",
            other => other,
        });
    let confidence = review_confidence(review.get("overall_confidence_score"));
    let mut facts = Vec::new();
    if let Some(verdict) = verdict {
        facts.push(format!("**Result:** {verdict}"));
    }
    if let Some(confidence) = confidence {
        facts.push(format!("**Confidence:** {confidence}"));
    }
    if !facts.is_empty() {
        out.push_str(&facts.join(" · "));
        out.push_str("\n\n");
    }

    if let Some(explanation) = review
        .get("overall_explanation")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
    {
        out.push_str(explanation);
        out.push_str("\n\n");
    }

    out.push_str("### Findings\n\n");
    if findings.is_empty() {
        out.push_str("No findings.");
        return Some(out);
    }

    for (index, finding) in findings.iter().enumerate() {
        let title = finding
            .get("title")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|title| !title.is_empty())
            .unwrap_or("Untitled finding")
            .replace(['\r', '\n'], " ");
        let priority = finding.get("priority").and_then(Value::as_i64);
        let heading = if title.starts_with("[P") {
            title
        } else if let Some(priority) = priority {
            format!("[P{priority}] {title}")
        } else {
            title
        };
        let _ = write!(out, "#### {}. {heading}\n\n", index + 1);

        if let Some(body) = finding
            .get("body")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|body| !body.is_empty())
        {
            out.push_str(body);
            out.push_str("\n\n");
        }

        if let Some(location) = finding.get("code_location") {
            if let Some(path) = location.get("absolute_file_path").and_then(Value::as_str) {
                let range = location.get("line_range");
                let start = range
                    .and_then(|range| range.get("start"))
                    .and_then(Value::as_i64);
                let end = range
                    .and_then(|range| range.get("end"))
                    .and_then(Value::as_i64);
                let suffix = match (start, end) {
                    (Some(start), Some(end)) if start != end => format!(":{start}–{end}"),
                    (Some(start), _) => format!(":{start}"),
                    _ => String::new(),
                };
                // 路径理论上不会含反引号；替换掉可避免异常输入打断 Markdown。
                let path = path.replace('`', "'");
                let _ = write!(out, "**Location:** `{path}{suffix}`\n\n");
            }
        }

        if let Some(confidence) = review_confidence(finding.get("confidence_score")) {
            let _ = write!(out, "**Confidence:** {confidence}\n\n");
        }
    }

    Some(out.trim_end().to_string())
}

fn format_review_json(text: &str) -> Option<String> {
    let text = text.trim();
    let review: Value = match serde_json::from_str(text) {
        Ok(review) => review,
        Err(_) => {
            // 某些 reviewer 把键名按 Markdown 写成 `confidence\_score`；这类
            // 输出不是合法 JSON，只在初次解析失败后做窄化兼容。
            let unescaped = text.replace("\\_", "_");
            serde_json::from_str(&unescaped).ok()?
        }
    };
    format_review_output(&review)
}

fn parse_rollout(path: &Path, decode_images: bool) -> Result<CodexParse> {
    let _image_budget = transcript_image_decode_budget(decode_images);
    let file = fs::File::open(path)?;
    let reader = BufReader::with_capacity(1 << 20, file);

    let mut messages: Vec<TranscriptMessage> = Vec::new();
    let mut event_fallback: Vec<TranscriptMessage> = Vec::new();
    let mut tool_index: HashMap<String, (usize, usize)> = HashMap::new();
    let mut pending_review_message: Option<usize> = None;
    let mut cwd = String::new();
    let mut git_branch: Option<String> = None;
    let mut model: Option<String> = None;
    let mut source: Option<String> = None;
    let mut tokens_used: i64 = 0;
    let mut created_at: i64 = 0;
    let mut updated_at: i64 = 0;
    let mut unknown_lines: u32 = 0;
    let mut saw_session_meta = false;
    let mut spawn: Option<SpawnMeta> = None;
    // 子线程自己的历史从哪个 ordinal 开始(首行 session_meta 里的权威值)
    let mut history_start: Option<i64> = None;
    // 子线程里"父线程历史到此为止"的下标(messages / event_fallback 各一)
    let mut inherited: Option<(usize, usize)> = None;

    for line in reader.lines() {
        let Ok(line) = line else {
            unknown_lines += 1;
            continue;
        };
        if line.trim().is_empty() {
            continue;
        }
        let row: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => {
                unknown_lines += 1;
                continue;
            }
        };
        let ts = row.get("timestamp").map(to_epoch_ms).unwrap_or(0);
        if ts > 0 {
            if created_at == 0 {
                created_at = ts;
            }
            if ts > updated_at {
                updated_at = ts;
            }
        }
        // fork 段的终点:Codex 在子线程首行写了 `subagent_history_start_ordinal`,
        // 这一行的 ordinal 追平它,后面才是子线程自己的活。权威值优先,信封
        // 判据(下面的 agent_message arm)只是它缺席时的兜底
        if let (Some(start), None, Some(_)) = (history_start, inherited, spawn.as_ref()) {
            if row
                .get("ordinal")
                .and_then(Value::as_i64)
                .is_some_and(|ordinal| ordinal >= start)
            {
                inherited = Some((messages.len(), event_fallback.len()));
            }
        }
        let typ = row.get("type").and_then(|v| v.as_str()).unwrap_or("");
        let Some(payload) = row.get("payload") else {
            if typ != "compacted" && typ != "world_state" {
                unknown_lines += 1;
            }
            continue;
        };

        match typ {
            "session_meta" => {
                if !saw_session_meta {
                    saw_session_meta = true;
                    // fork 进来的父线程历史第二行还有一条 session_meta(父自己的),
                    // saw_session_meta 已经把它挡住了
                    if let ThreadKind::Spawned(meta) = thread_kind(
                        payload.get("source"),
                        payload.get("thread_source").and_then(Value::as_str),
                    ) {
                        spawn = Some(meta);
                        history_start = payload
                            .get("subagent_history_start_ordinal")
                            .and_then(Value::as_i64);
                    }
                    if let Some(c) = payload.get("cwd").and_then(|v| v.as_str()) {
                        cwd = c.to_string();
                    }
                    if let Some(o) = payload.get("originator").and_then(|v| v.as_str()) {
                        source = friendly_source(o);
                    }
                    if let Some(b) = payload
                        .get("git")
                        .and_then(|g| g.get("branch"))
                        .and_then(|v| v.as_str())
                    {
                        git_branch = Some(b.to_string());
                    }
                }
            }
            "turn_context" => {
                if cwd.is_empty() {
                    if let Some(c) = payload.get("cwd").and_then(|v| v.as_str()) {
                        cwd = c.to_string();
                    }
                }
                if let Some(m) = payload.get("model").and_then(|v| v.as_str()) {
                    model = Some(m.to_string());
                }
            }
            "response_item" => {
                let pt = payload.get("type").and_then(|v| v.as_str()).unwrap_or("");
                match pt {
                    "message" => {
                        let role = payload.get("role").and_then(|v| v.as_str()).unwrap_or("");
                        let content = payload.get("content").unwrap_or(&Value::Null);
                        let blocks = content
                            .as_array()
                            .map(Vec::as_slice)
                            .unwrap_or_else(|| std::slice::from_ref(content));
                        let mut parsed_message = ParsedContent::default();
                        for block in blocks {
                            let mut parsed = content_parts(block, decode_images);
                            let text = if role == "user" {
                                clean_user_text(&parsed.text)
                            } else {
                                parsed.text.trim().to_string()
                            };
                            if text != parsed.text {
                                for image in &mut parsed.images {
                                    image.text_offset = image.text_offset.min(text.len());
                                }
                            }
                            parsed.text = text;
                            parsed_message.append(parsed);
                        }
                        let ParsedContent { text, images } = parsed_message;
                        if text.is_empty() && images.is_empty() {
                            continue;
                        }
                        let mut message = match role {
                            "user" => {
                                if let Some(results) = review_results_from_user_action(&text) {
                                    pending_review_message = Some(messages.len());
                                    mk_msg(Role::Assistant, MessageKind::Text, &results, ts)
                                } else {
                                    mk_msg(Role::User, user_kind(&text), &text, ts)
                                }
                            }
                            "assistant" => {
                                let text = format_review_json(&text).unwrap_or(text);
                                mk_msg(Role::Assistant, MessageKind::Text, &text, ts)
                            }
                            _ => mk_msg(Role::System, MessageKind::Meta, &text, ts),
                        };
                        message.images = images;
                        messages.push(message);
                    }
                    "reasoning" => {
                        // encrypted_content 丢弃,只取明文 summary
                        let mut parts: Vec<String> = Vec::new();
                        if let Some(Value::Array(summary)) = payload.get("summary") {
                            for s in summary {
                                if let Some(t) = s.get("text").and_then(|v| v.as_str()) {
                                    parts.push(t.to_string());
                                }
                            }
                        }
                        if !parts.is_empty() {
                            let thinking = clip(&parts.join("\n\n"), MAX_TOOL_IO).0;
                            // 同 need_host:fork 段里的宿主待会儿会被 splice 掉
                            let last_is_inherited =
                                inherited.is_some_and(|(cut, _)| messages.len() <= cut);
                            match messages.last_mut() {
                                Some(last)
                                    if !last_is_inherited
                                        && last.role == Role::Assistant
                                        && last.text.is_empty()
                                        && last.thinking.is_none() =>
                                {
                                    last.thinking = Some(thinking);
                                }
                                _ => {
                                    let mut m = mk_msg(Role::Assistant, MessageKind::Text, "", ts);
                                    m.thinking = Some(thinking);
                                    messages.push(m);
                                }
                            }
                        }
                    }
                    "function_call" | "custom_tool_call" | "local_shell_call" => {
                        let call_id = payload
                            .get("call_id")
                            .or_else(|| payload.get("id"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let name = payload
                            .get("name")
                            .and_then(|v| v.as_str())
                            .unwrap_or("exec")
                            .to_string();
                        let raw_input = payload
                            .get("arguments")
                            .and_then(|v| v.as_str())
                            .or_else(|| payload.get("input").and_then(|v| v.as_str()))
                            .map(String::from)
                            .unwrap_or_else(|| {
                                payload
                                    .get("action")
                                    .map(|a| serde_json::to_string(a).unwrap_or_default())
                                    .unwrap_or_default()
                            });
                        let preview_source: Value = serde_json::from_str(&raw_input)
                            .unwrap_or(Value::String(raw_input.clone()));
                        let call = ToolCallView {
                            id: call_id.clone(),
                            name,
                            input_preview: make_preview(&preview_source),
                            input: if raw_input.is_empty() {
                                None
                            } else {
                                Some(clip(&raw_input, MAX_TOOL_IO).0)
                            },
                            output: None,
                            is_error: false,
                            sidechain_ref: None,
                        };
                        // 末条消息落在 fork 段里(下标 < cut)时必须另起宿主:
                        // collapse_inherited 会把那段整段 splice 掉,挂上去的
                        // 工具调用与其输出会跟着消失
                        let last_is_inherited =
                            inherited.is_some_and(|(cut, _)| messages.len() <= cut);
                        let need_host = last_is_inherited
                            || !matches!(
                                messages.last(),
                                Some(m) if m.role == Role::Assistant && m.kind == MessageKind::Text
                            );
                        if need_host {
                            messages.push(mk_msg(Role::Assistant, MessageKind::Text, "", ts));
                        }
                        let mi = messages.len() - 1;
                        let host = &mut messages[mi];
                        host.tool_calls.push(call);
                        if !call_id.is_empty() {
                            tool_index.insert(call_id, (mi, host.tool_calls.len() - 1));
                        }
                    }
                    "function_call_output" | "custom_tool_call_output" => {
                        let call_id = payload
                            .get("call_id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        if let Some(&(mi, ti)) = tool_index.get(call_id) {
                            let output = payload.get("output").unwrap_or(&Value::Null);
                            let content = output.get("content").unwrap_or(output);
                            let parsed = tool_result_parts(content, decode_images);
                            let message = &mut messages[mi];
                            message.tool_calls[ti].output = Some(clip(&parsed.text, MAX_TOOL_IO).0);
                            append_images_to_message_end(message, parsed.images);
                        }
                    }
                    // 与下面 event_msg 块里的同名 arm 无关:那边是助手正文
                    "agent_message" => {
                        // 父线程派活/追加指令的信封(Payload 加密,只有抬头是
                        // 明文)。首行没给 `subagent_history_start_ordinal` 时的
                        // 兜底判据:第一条发给**本子线程**的信封之前的一切,是
                        // spawn 时按 fork_turns 复制进来的父线程历史。认收件人
                        // 而不是"第一条 agent_message":父线程在 fork 点之前派过
                        // 别的子代理时,那些信封也在复制进来的这段里。
                        // **agent_path 认不出来就一条都不折**——退回"第一条信封"
                        // 正是这句要防的事,而多折一段会让父线程的话进两次 FTS
                        if let (Some(meta), None) = (spawn.as_ref(), inherited) {
                            let mine = meta.agent_path.as_str();
                            let to = payload.get("recipient").and_then(Value::as_str);
                            if !mine.is_empty() && to == Some(mine) {
                                inherited = Some((messages.len(), event_fallback.len()));
                            }
                        }
                    }
                    _ => {}
                }
            }
            "event_msg" => {
                let pt = payload.get("type").and_then(|v| v.as_str()).unwrap_or("");
                match pt {
                    "token_count" => {
                        if let Some(total) = payload
                            .get("info")
                            .and_then(|i| i.get("total_token_usage"))
                            .and_then(|u| u.get("total_tokens"))
                            .and_then(|v| v.as_i64())
                        {
                            tokens_used = total;
                        }
                    }
                    "user_message" => {
                        if let Some(m) = payload.get("message").and_then(|v| v.as_str()) {
                            let cleaned = clean_user_text(m);
                            if !cleaned.trim().is_empty() {
                                if let Some(results) = review_results_from_user_action(&cleaned) {
                                    event_fallback.push(mk_msg(
                                        Role::Assistant,
                                        MessageKind::Text,
                                        &results,
                                        ts,
                                    ));
                                } else {
                                    event_fallback.push(mk_msg(
                                        Role::User,
                                        user_kind(&cleaned),
                                        cleaned.trim(),
                                        ts,
                                    ));
                                }
                            }
                        }
                    }
                    "agent_message" => {
                        if let Some(m) = payload.get("message").and_then(|v| v.as_str()) {
                            if !m.trim().is_empty() {
                                let text = format_review_json(m).unwrap_or_else(|| m.trim().into());
                                event_fallback.push(mk_msg(
                                    Role::Assistant,
                                    MessageKind::Text,
                                    &text,
                                    ts,
                                ));
                            }
                        }
                    }
                    "item_completed" => {
                        let item = payload.get("item");
                        let is_review = item
                            .and_then(|item| item.get("type"))
                            .and_then(Value::as_str)
                            == Some("ExitedReviewMode");
                        if is_review {
                            if let Some(markdown) = item
                                .and_then(|item| item.get("review_output"))
                                .and_then(format_review_output)
                            {
                                let review_message =
                                    mk_msg(Role::Assistant, MessageKind::Text, &markdown, ts);
                                if let Some(index) = pending_review_message
                                    .take()
                                    .filter(|index| *index + 1 == messages.len())
                                {
                                    messages[index] = review_message;
                                } else if !messages.last().is_some_and(|message| {
                                    message.role == Role::Assistant && message.text == markdown
                                }) {
                                    messages.push(review_message);
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            "compacted" => {
                messages.push(mk_msg(
                    Role::System,
                    MessageKind::CompactSummary,
                    "── Context compacted ──",
                    ts,
                ));
            }
            // 子线程每收一次派活就有一行,内容只有 {"trigger_turn":true}
            "world_state" | "inter_agent_communication_metadata" => {}
            _ => unknown_lines += 1,
        }
    }

    // fork 段先折叠再判 has_real:折完只剩一条 Meta 的子线程要走得到
    // event_msg 回退,与普通会话同一条路。回退流只截断、**不留标记**——
    // 它一非空就会在 has_real 为假时被选中,一条标记足以把子线程自己的
    // response_item 整条流挤掉(只跑了工具、还没出正文的子代理正是这形态)
    if let Some((cut, cut_fallback)) = inherited {
        // fork_turns 复制进来的父线程历史(实测复制的是真实 message 行,不是打包成
        // 一条的 "Codex agent history" 块,`is_injected_user_content` 拦不住)
        collapse_inherited(&mut messages, cut, "thread");
        event_fallback.drain(..cut_fallback.min(event_fallback.len()));
    }

    // response_item 完全缺席的会话退回 event_msg 流
    let has_real = messages
        .iter()
        .any(|m| m.kind == MessageKind::Text && (!m.text.is_empty() || !m.images.is_empty()));
    let mut final_messages = if has_real {
        messages
    } else if !event_fallback.is_empty() {
        event_fallback
    } else {
        messages
    };
    assign_seq(&mut final_messages);

    Ok(CodexParse {
        messages: final_messages,
        cwd,
        git_branch,
        model,
        source,
        tokens_used,
        created_at,
        updated_at,
        unknown_lines,
        spawn,
    })
}

/// Codex Desktop 会把文件清单包在真实提问外面，并在正文写入指向临时文件
/// 的 `<image ...>` 标签；图片本体由独立内容块承载。
fn clean_user_text(text: &str) -> String {
    let unwrapped = unwrap_file_preamble(text);
    strip_image_tags(unwrapped.as_deref().unwrap_or(text))
        .trim()
        .to_string()
}

fn mk_msg(role: Role, kind: MessageKind, text: &str, ts: i64) -> TranscriptMessage {
    let (clipped, truncated) = clip(text, MAX_MSG_TEXT);
    TranscriptMessage {
        seq: 0,
        role,
        kind,
        text: clipped,
        truncated,
        tool_calls: Vec::new(),
        thinking: None,
        timestamp: if ts > 0 { Some(ts) } else { None },
        model: None,
        images: Vec::new(),
    }
}

/// rollout-2026-08-14T11-47-18-<uuid>.jsonl → uuid
pub(crate) fn rollout_native_id(stem: &str) -> String {
    if let Some(rest) = stem.strip_prefix("rollout-") {
        // 跳过 "YYYY-MM-DDTHH-MM-SS-" 前缀(19 字符 + 尾随 '-')
        if rest.len() > 20 && rest.as_bytes()[10] == b'T' {
            return rest[20..].to_string();
        }
    }
    stem.to_string()
}

fn build_meta(r: &SessionFileRef, p: &CodexParse, archived_dir: &Path) -> SessionMeta {
    // 子线程优先用任务名:它自己没有用户消息(派活加密),而 fork 段万一没折成
    // (整个文件里找不到派活信封)推导出来的会是父线程的首条消息。用户在 Codex
    // 里手动命名过的话,merge_quick_meta 仍会用 state DB 的 name 盖过这里
    let title = p
        .spawn
        .as_ref()
        .and_then(SpawnMeta::title)
        .or_else(|| title_from_messages(&p.messages))
        .unwrap_or_else(|| UNTITLED.to_string());
    let project_name = project_name_of(&p.cwd);
    SessionMeta {
        key: format!("codex:{}", r.native_id),
        host: String::new(),
        id: r.native_id.clone(),
        agent: AgentId::Codex,
        title,
        project_path: p.cwd.clone(),
        project_name,
        file_path: r.file_path.clone(),
        created_at: if p.created_at > 0 {
            p.created_at
        } else {
            r.mtime_ms
        },
        updated_at: if p.updated_at > 0 {
            p.updated_at
        } else {
            r.mtime_ms
        },
        message_count: p
            .messages
            .iter()
            .filter(|m| m.kind == MessageKind::Text)
            .count() as i64,
        size_bytes: r.size,
        git_branch: p.git_branch.clone(),
        model: p.model.clone(),
        tokens_used: if p.tokens_used > 0 {
            Some(p.tokens_used)
        } else {
            None
        },
        archived: r
            .file_path
            .starts_with(&archived_dir.to_string_lossy().to_string()),
        source: p.source.clone(),
        favorite: false,
        pinned: false,
    }
}

/// memories_1.sqlite 的 stage1_outputs:Codex 记忆整合的第一阶段,按线程一行——
/// rollout_summary 是对整段会话的摘要,raw_memory 是从中抽出的记忆条目;挂到
/// `codex:<thread_id>` 会话上,项目读库时按该会话解析。按库 + WAL 的 mtime 戳缓存
/// (别家 SQLite 型 adapter 同款)。**`None` 与 `Some(空)` 是两件事**:库不存在、
/// 表不存在(老版本)= 确定没有;库在但打不开 / 读出错 = 不知道,交回 None 让缓存
/// 不记——把一次瞬时失败缓存成"没有"会让库里的线程记忆整组被删,而戳不动就再也
/// 不读(Codex 不在跑时文件 mtime 是静止的)。schema 按 2026-09 的库推断,本机零
/// 行、未经真数据验证
fn thread_memories(
    source: &MemorySource,
    cache: &MtimeCache<Vec<MemoryDoc>>,
) -> Option<Vec<MemoryDoc>> {
    cache.get_or_try_build(super::sqlite_ro::db_cache_stamp(&source.path), || {
        read_thread_memories(&source.path, &source.id())
    })
}

/// `source` 是来源 id(memories.source 列),由调用方从 MemorySource 取——别在这里按路径
/// 重新拼一遍(id 规则一变这里就静默对不上,2026-09-22 /simplify)
fn read_thread_memories(db: &Path, source: &str) -> Option<Vec<MemoryDoc>> {
    if !db.is_file() {
        return Some(Vec::new());
    }
    let ro = open_sqlite_ro(db, "codex-memories")?;
    let has_table: bool = ro
        .conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'stage1_outputs')",
            [],
            |r| r.get(0),
        )
        .ok()?;
    if !has_table {
        return Some(Vec::new());
    }
    let mut stmt = ro
        .conn
        .prepare(
            "SELECT thread_id, rollout_slug, rollout_summary, raw_memory, generated_at
             FROM stage1_outputs",
        )
        .ok()?;
    // 列的可空性与类型是推断的(本机零行):正文两列按可空读,generated_at 认数字
    // 与 ISO 文本;某一行的类型对不上只跳过那一行——原先一行坏就让整个函数 None、
    // 连 memories/*.md 一起从索引里消失(2026-09-21 review);读到一半的 I/O 错误
    // 仍是"不知道"
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, rusqlite::types::Value>(4)?,
            ))
        })
        .ok()?;
    let mut out = Vec::new();
    for row in rows {
        let (thread_id, slug, summary, memory, generated) = match row {
            Ok(row) => row,
            Err(rusqlite::Error::InvalidColumnType(..)) => continue,
            Err(_) => return None,
        };
        let summary = summary.unwrap_or_default();
        let memory = memory.unwrap_or_default();
        let generated = match generated {
            rusqlite::types::Value::Real(f) => f,
            rusqlite::types::Value::Integer(i) => i as f64,
            rusqlite::types::Value::Text(t) => iso_ms(&t) as f64 / 1000.0,
            _ => 0.0,
        };
        if summary.trim().is_empty() && memory.trim().is_empty() {
            continue;
        }
        let path = virtual_path(db, &thread_id);
        let body = format!(
            "## Summary\n\n{}\n\n## Memory\n\n{}\n",
            summary.trim(),
            memory.trim()
        );
        out.push(MemoryDoc {
            key: session_key(AgentId::Codex, "", &path),
            agent: AgentId::Codex,
            host: String::new(),
            scope: MemoryScope::Thread,
            project_path: String::new(),
            project_name: String::new(),
            session_key: session_key(AgentId::Codex, "", &thread_id),
            title: slug
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| "Session memory".to_string()),
            // 秒还是毫秒未验证,交 epoch_ms 统一裁定
            updated_at: epoch_ms(generated),
            size_bytes: body.len() as i64,
            source: source.to_string(),
            path,
            body,
        });
    }
    Some(out)
}

impl AgentAdapter for CodexAdapter {
    fn agent(&self) -> AgentId {
        AgentId::Codex
    }

    fn list_session_files(&self) -> Result<Vec<SessionFileRef>> {
        // 全量枚举与 watcher 同走 file_ref 这一个漏斗(dsh 同款):native id 剥离
        // 与内部线程判定只存在一处,两条入口不可能分家;缺根时 jsonl_entries 给空
        Ok(self
            .data_roots()
            .iter()
            .flat_map(|root| jsonl_entries(root))
            .filter_map(|entry| self.file_ref(entry.path()))
            .collect())
    }

    fn quick_meta(&self, refs: &[SessionFileRef]) -> Option<HashMap<String, SessionMeta>> {
        let rows = read_threads(&self.state_db)?;
        // state DB 的 rollout_path 是**写库那台机器**上的绝对路径——远程缓存、
        // 自定义根下与本实例枚举出的 file_path 对不上;rollout 文件名(时间戳 +
        // uuid)全局唯一,按文件名匹配即可,与根/host/平台分隔符都无关
        let by_name: HashMap<&str, &ThreadRow> = rows
            .iter()
            .map(|r| (rollout_file_name(&r.rollout_path), r))
            .collect();
        let mut out = HashMap::new();
        for r in refs {
            let Some(row) = by_name.get(rollout_file_name(&r.file_path)) else {
                continue;
            };
            // 结构化 source(对象)= 子代理线程。它的 `title` 是 Codex 按首条
            // 消息自动生成的,而子线程开头是 fork 进来的父线程对话——用它会让
            // 子会话又变成父会话标题的副本(0.6.6 修掉的那个症状)。这类行只
            // 认用户手工命名的 `name`,其余交回解析侧合成的任务名
            let auto_title_is_parents = row
                .source
                .as_deref()
                .is_some_and(|source| source.trim_start().starts_with('{'));
            let title = row
                .name
                .as_deref()
                .filter(|n| !n.trim().is_empty())
                .map(String::from)
                .or_else(|| {
                    if auto_title_is_parents {
                        return None;
                    }
                    // Codex 自家 state DB 会把注入文本存进 title,同样要过滤
                    if is_injected_user_content(&row.title) {
                        return None;
                    }
                    let t = clean_title_candidate(&row.title);
                    if t.is_empty() {
                        None
                    } else {
                        Some(t)
                    }
                })
                .unwrap_or_else(|| UNTITLED.to_string());
            let project_name = project_name_of(&row.cwd);
            out.insert(
                r.file_path.clone(),
                SessionMeta {
                    key: format!("codex:{}", row.id),
                    host: String::new(),
                    id: row.id.clone(),
                    agent: AgentId::Codex,
                    title,
                    project_path: row.cwd.clone(),
                    project_name,
                    file_path: r.file_path.clone(),
                    created_at: row.created_at_ms.unwrap_or(r.mtime_ms),
                    updated_at: row.updated_at_ms.unwrap_or(r.mtime_ms),
                    message_count: 0,
                    size_bytes: r.size,
                    git_branch: row.git_branch.clone(),
                    model: row.model.clone(),
                    tokens_used: row.tokens_used,
                    archived: row.archived,
                    source: row.source.clone(),
                    favorite: false,
                    pinned: false,
                },
            );
        }
        Some(out)
    }

    fn file_ref(&self, path: &Path) -> Option<SessionFileRef> {
        let mut r = default_file_ref(self.agent(), path)?;
        if matches!(read_thread_kind(path), ThreadKind::Internal) {
            return None;
        }
        r.native_id = rollout_native_id(&r.native_id);
        Some(r)
    }

    fn parent_links_global(&self) -> bool {
        // thread_spawn_edges 是 home 里的一张总表,parent 的胜出文件可能归同家另一个 location
        true
    }

    fn manages_parent_links(&self) -> bool {
        true
    }

    /// 只认 state DB 的登记表,按库+WAL 的 mtime 戳缓存(cursor_ide 同款)。
    /// trait 说这是"当前数据根内的全量快照",所以只能问磁盘——边扫边记会让它
    /// 变成"本进程扫到哪了"的函数,半截快照会被 `sync_parent_links` 当成
    /// "关系已解除",把子会话连同继承来的项目一起打回原形,下一轮全量再挂回去,
    /// 来回抖;而按戳缓存是照实答、只是省掉重复的库打开:`sync_parent_links`
    /// 问的是**所有**管关系的 adapter,别家 watcher 事件也会走到这里。
    /// 代价:没有 state DB 的根(自定义 location 只选了数据目录的裸 rollout
    /// 拷贝)认不出父子,子线程以任务名作为顶层会话列出——不是噪音,只是没嵌套
    fn parent_links(&self) -> Option<Vec<(String, String)>> {
        let stamp = super::sqlite_ro::db_cache_stamp(&self.state_db);
        // 读不出来时 `?` 交回 None:MtimeCache 不缓存、下次重试,scanner 对这一家整段
        // 跳过——原先这里 unwrap_or_default 成空,一次瞬时失败就让 sync_parent_links
        // 当成"关系全解除",整家清空所有子会话的挂靠(2026-09-21 review)
        self.links_cache.get_or_try_build(stamp, || {
            let mut links: Vec<(String, String)> = read_spawn_edges(&self.state_db)?
                .into_iter()
                .filter(|(child, parent)| !parent.is_empty() && child != parent)
                .map(|(child, parent)| (format!("codex:{child}"), format!("codex:{parent}")))
                .collect();
            // HashMap 的迭代序是每进程随机的,别让它漏进返回值
            links.sort();
            Some(links)
        })
    }

    fn merge_quick_meta(&self, mut parsed: SessionMeta, quick: &SessionMeta) -> SessionMeta {
        // state DB 的 title/name 是用户在 Codex 里手动命名,优先于首条消息推导;
        // UNTITLED 守卫防止占位符覆盖真实标题。key/id 以 state 的线程 id 为准。
        if !quick.title.is_empty() && quick.title != UNTITLED {
            parsed.title = quick.title.clone();
        }
        parsed.key = quick.key.clone();
        parsed.id = quick.id.clone();
        // source 相反:parsed 的 originator 比 state 的粗分类精确,quick 只兜底
        if parsed.source.is_none() {
            parsed.source = quick.source.clone();
        }
        if parsed.model.is_none() {
            parsed.model = quick.model.clone();
        }
        if parsed.tokens_used.is_none() {
            parsed.tokens_used = quick.tokens_used;
        }
        parsed
    }

    fn parse_session(&self, r: &SessionFileRef) -> Result<ParsedSession> {
        let parsed = parse_rollout(Path::new(&r.file_path), false)?;
        let meta = build_meta(r, &parsed, &self.archived_dir);
        Ok(ParsedSession::derive(
            meta,
            &parsed.messages,
            parsed.unknown_lines,
        ))
    }

    fn parse_transcript(&self, r: &SessionFileRef) -> Result<ParsedTranscript> {
        let parsed = parse_rollout(Path::new(&r.file_path), true)?;
        Ok(ParsedTranscript {
            meta: build_meta(r, &parsed, &self.archived_dir),
            mainline: parsed.messages,
            sidechains: Vec::new(),
            unknown_line_count: parsed.unknown_lines,
        })
    }

    fn cleanup_paths(&self, meta: &SessionMeta) -> Option<Vec<String>> {
        Some(self.session_paths(meta))
    }

    fn memory_sources(&self) -> Vec<MemorySource> {
        // memories/ 与 memories_1.sqlite(agent 记的)、AGENTS.md 与 rules/*.rules(用户
        // 写的指令)都是 CODEX_HOME 直属;只选了 sessions 目录或裸 rollout 拷贝的自定义
        // 根没有 home,那里就没有记忆(不摸父目录)。项目根下的 AGENTS.md 由
        // project_instruction_sources 给
        let Some(home) = &self.home else {
            return Vec::new();
        };
        let source = |kind, rel: &str| MemorySource {
            agent: AgentId::Codex,
            kind,
            path: home.join(rel),
        };
        vec![
            source(MemorySourceKind::Dir { ext: "md" }, "memories"),
            source(MemorySourceKind::ThreadDb, "memories_1.sqlite"),
            source(MemorySourceKind::File, "AGENTS.md"),
            source(MemorySourceKind::Dir { ext: "rules" }, "rules"),
        ]
    }

    fn list_memories(
        &self,
        sources: &[MemorySource],
        projects: &[PathBuf],
    ) -> Result<Vec<MemoryDoc>> {
        let mut out =
            super::generic_memory_docs(&self.memories, AgentId::Codex, sources, projects)?;
        if let Some(db) = sources
            .iter()
            .find(|s| s.kind == MemorySourceKind::ThreadDb)
        {
            let threads = thread_memories(db, &self.thread_memories).ok_or_else(|| {
                anyhow::anyhow!("{} exists but could not be read", db.path.display())
            })?;
            out.extend(threads);
        }
        Ok(out)
    }

    fn with_custom_root(&self, dir: PathBuf) -> Box<dyn AgentAdapter> {
        // dir 视作 CODEX_HOME 形态;归一化未上提的孤立数据目录按**目录名**
        // 保留角色:空的独立 sessions 目录日后落盘的 rollout 要能被发现,
        // 独立 archived 拷贝的会话必须保住 archived 标记(build_meta 按
        // archived_dir 前缀判);无名可依的裸 rollout 拷贝当活跃 sessions。
        // 侧档一并相对 dir 派生,绝不越界摸父目录——"父目录有 home 证据"的
        // 场景由 normalize_custom_root 在入库前上提(2026-08-24 Codex review)
        let name = dir.file_name().and_then(|n| n.to_str());
        let (sessions_dir, archived_dir, home) = match name {
            Some("archived_sessions") => (dir.join("sessions"), dir.clone(), None),
            Some("sessions") => (dir.clone(), dir.join("archived_sessions"), None),
            _ if is_rollout_store(&dir) => (dir.clone(), dir.join("archived_sessions"), None),
            _ => (
                dir.join("sessions"),
                dir.join("archived_sessions"),
                Some(dir.clone()),
            ),
        };
        Box::new(Self {
            sessions_dir,
            archived_dir,
            state_db: dir.join("state_5.sqlite"),
            scan_sessions: true,
            scan_archived: true,
            links_cache: MtimeCache::new(),
            home,
            memories: super::MemoryCache::new(),
            thread_memories: MtimeCache::new(),
        })
    }

    fn data_roots(&self) -> Vec<PathBuf> {
        let mut roots = Vec::with_capacity(2);
        if self.scan_sessions {
            roots.push(self.sessions_dir.clone());
        }
        if self.scan_archived {
            roots.push(self.archived_dir.clone());
        }
        roots
    }

    fn excluding_data_roots(&self, roots: &[PathBuf]) -> Option<Box<dyn AgentAdapter>> {
        Some(Box::new(Self {
            sessions_dir: self.sessions_dir.clone(),
            archived_dir: self.archived_dir.clone(),
            state_db: self.state_db.clone(),
            scan_sessions: self.scan_sessions && !roots.contains(&self.sessions_dir),
            scan_archived: self.scan_archived && !roots.contains(&self.archived_dir),
            links_cache: MtimeCache::new(),
            home: self.home.clone(),
            memories: super::MemoryCache::new(),
            thread_memories: MtimeCache::new(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_review_json_is_formatted_instead_of_exposed() {
        let raw = serde_json::json!({
            "findings": [{
                "title": "A finding",
                "body": "Human-readable detail.",
                "confidence_score": 0.91,
                "priority": 1,
                "code_location": {
                    "absolute_file_path": "/tmp/example.rs",
                    "line_range": { "start": 7, "end": 7 }
                }
            }],
            "overall_correctness": "patch is incorrect",
            "overall_explanation": "One issue remains.",
            "overall_confidence_score": 0.95
        })
        .to_string();

        let formatted = format_review_json(&raw).expect("review JSON");
        assert!(formatted.contains("## Code review"));
        assert!(formatted.contains("[P1] A finding"));
        assert!(formatted.contains("`/tmp/example.rs:7`"));
        assert!(!formatted.contains("\"findings\""));
        assert!(format_review_json(&raw.replace('_', "\\_")).is_some());
        assert!(format_review_json(r#"{"ordinary":"json"}"#).is_none());
    }
}
