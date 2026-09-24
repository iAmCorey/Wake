//! wake-mcp 的四个只读工具:定义(JSON Schema)与执行。输出全是 Markdown 文本,
//! 给 LLM 直接读;结构化输出(structuredContent)不做。
//!
//! 错误分两类:参数形状不对是协议错误(`InvalidParams` → JSON-RPC -32602);
//! 执行失败(坏 key、文件解析失败)是给 LLM 看的结果(`Failed` → isError)。
//! "没匹配上项目""没有结果"都不是错误,照常返回带提示的文本。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use crate::adapters::{adapter_for, AgentAdapter};
use crate::db::Store;
use crate::models::*;
use crate::services::context::{parse_since, resolve_project_paths};
use crate::services::exporter::{fmt_time, render_compact, CompactOptions};
use crate::text::{one_line, plural};

struct ToolContext<'a> {
    store: &'a Store,
    adapters: &'a [Box<dyn AgentAdapter>],
    now_ms: i64,
    cache: &'a TranscriptCache,
}

/// 单槽转录缓存:分页读同一会话时不必每页整文件重解析(默认 20k 字符一页,
/// 大会话几十页)。源文件或索引中的项目归属变化即失效;server 进程随客户端
/// 会话长驻,连续翻页总是同一文件。项目归属可由边车更新,不必修改正文。
#[derive(Default)]
pub struct TranscriptCache(Mutex<Option<CachedTranscript>>);

struct CachedTranscript {
    path: String,
    mtime: i64,
    size: i64,
    indexed_project: String,
    transcript: Arc<ParsedTranscript>,
}

impl TranscriptCache {
    fn get_or_parse(
        &self,
        adapter: &dyn AgentAdapter,
        meta: &SessionMeta,
    ) -> anyhow::Result<Arc<ParsedTranscript>> {
        let r = SessionFileRef::from_meta(meta);
        // SQLite 型会话的 `<db>#<id>` 虚拟路径 stat 不到,from_meta 退回索引里的
        // 时间——源库变了、Wake 没重扫时键不会变。改用库文件与 -wal 的 mtime
        // 作戳(sqlite_ro 行缓存同款判据),源库一写就失效
        let stamp = if std::path::Path::new(&r.file_path).exists() {
            r.mtime_ms
        } else {
            let db = crate::adapters::sqlite_ro::strip_virtual_path(&r.file_path);
            crate::adapters::sqlite_ro::db_cache_stamp(std::path::Path::new(db))
        };
        if let Some(cached) = self.0.lock().unwrap().as_ref() {
            if cached.path == r.file_path
                && cached.mtime == stamp
                && cached.size == r.size
                && cached.indexed_project == meta.project_path
            {
                return Ok(cached.transcript.clone());
            }
        }
        let t = Arc::new(adapter.parse_transcript(&r)?);
        *self.0.lock().unwrap() = Some(CachedTranscript {
            path: r.file_path,
            mtime: stamp,
            size: r.size,
            indexed_project: meta.project_path.clone(),
            transcript: t.clone(),
        });
        Ok(t)
    }
}

#[derive(Debug)]
pub enum ToolError {
    InvalidParams(String),
    Failed(String),
    Internal(String),
}

impl From<anyhow::Error> for ToolError {
    fn from(e: anyhow::Error) -> Self {
        ToolError::Internal(format!("{e:#}"))
    }
}

type ToolResult = Result<String, ToolError>;

pub const SEARCH: &str = "wake_search";
pub const LIST_SESSIONS: &str = "wake_list_sessions";
pub const GET_SESSION: &str = "wake_get_session";
pub const LIST_PROJECTS: &str = "wake_list_projects";
pub const LIST_MEMORIES: &str = "wake_list_memories";
/// 工具名的清单:自指回声过滤与 wake_lookups 记账(`adapters::wake_lookup_kind`)与
/// definitions 的稳定性测试都读它,加一家改这里与 `definitions()` 两处即可
pub const NAMES: [&str; 5] = [
    SEARCH,
    LIST_SESSIONS,
    GET_SESSION,
    LIST_PROJECTS,
    LIST_MEMORIES,
];

const MAX_SEARCH_SESSIONS: i64 = 30;
const MAX_LIST_SESSIONS: i64 = 100;
const MAX_LIST_MEMORIES: i64 = 100;
/// wake_search 末尾附带的记忆命中数
const MEMORY_HITS_IN_SEARCH: i64 = 5;
const MAX_LIST_PROJECTS: i64 = 200;
const SNIPPETS_PER_SESSION: usize = 3;

fn read_only_annotations() -> Value {
    json!({
        "readOnlyHint": true,
        "destructiveHint": false,
        "idempotentHint": true,
        "openWorldHint": false,
    })
}

fn project_param() -> Value {
    json!({
        "type": "string",
        "description": "Scope to one project. Pass an absolute path — your current working directory is ideal: Wake matches the enclosing indexed project, or every project below a parent directory — or a project name. Omit to cover all projects.",
    })
}

fn agents_param() -> Value {
    json!({
        "type": "array",
        "items": { "type": "string", "enum": AgentId::ALL.iter().map(|a| a.as_str()).collect::<Vec<_>>() },
        "description": "Only sessions from these agents (ids as listed). Omit for all agents.",
    })
}

fn since_param() -> Value {
    json!({
        "type": "string",
        "description": "Only sessions updated at or after this time: relative (30m, 24h, 7d, 2w) or an ISO date/time (2026-09-01, 2026-09-01T09:30:00Z).",
    })
}

/// tools/list 的内容。名字、参数名是对外契约,改了别人的配置就失效
pub fn definitions() -> Vec<Value> {
    vec![
        json!({
            "name": SEARCH,
            "title": "Search session history",
            "description": "Full-text search across every indexed coding-agent session on this machine (session titles, user prompts, assistant replies, tool names and inputs). Use it when the user asks whether something was discussed, tried or solved before, or wants the conversation about a topic, an error message, a file or a decision — git history does not hold that. Terms are ANDed; CJK text and code substrings like `useEffect(` work. Returns matching sessions with up to three snippets each, plus a `wake://session/<key>#<seq>` reference per snippet that you can read with wake_get_session.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Search terms. Terms shorter than 3 characters fall back to a slower substring scan." },
                    "project": project_param(),
                    "agents": agents_param(),
                    "since": since_param(),
                    "limit": { "type": "integer", "minimum": 1, "maximum": MAX_SEARCH_SESSIONS, "default": 10, "description": "Maximum sessions to return." },
                },
                "required": ["query"],
            },
            "annotations": read_only_annotations(),
        }),
        json!({
            "name": LIST_SESSIONS,
            "title": "List recent sessions",
            "description": "Most recently updated coding-agent sessions, optionally scoped to a project, to some agents, to a time window, or to starred sessions. Use it when the user asks what they or an agent worked on recently, wants to resume or continue earlier work, or refers to \"yesterday's session\", \"last time\", \"what Codex did here\" — pass the current working directory as `project`. It returns the session keys wake_get_session needs. Subagent sessions are folded into their parents; archived sessions are excluded.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "project": project_param(),
                    "agents": agents_param(),
                    "since": since_param(),
                    "starred": { "type": "boolean", "default": false, "description": "Only sessions the user starred in Wake." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": MAX_LIST_SESSIONS, "default": 20 },
                },
            },
            "annotations": read_only_annotations(),
        }),
        json!({
            "name": GET_SESSION,
            "title": "Read a session transcript",
            "description": "Read one session's transcript, parsed live from the agent's own files, as compact Markdown: user and assistant messages with `[seq N]` markers, tool calls folded to one line each, injected context omitted. Use it after wake_search or wake_list_sessions to see what actually happened — the reasoning, the decisions and the exact steps of an earlier session. Paginated: when the reply ends with a `from_seq` hint, call again with it to continue. Accepts a session key or a `wake://session/<key>#<seq>` reference (the seq becomes the starting point). Subagent transcripts (Claude Code sidechains, Cursor subagents) are listed at the end of the main transcript; pass one's id as `subagent` to read it. Also reads memory files: pass a `wake://memory/<key>` reference from wake_list_memories or wake_search to get that file.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "key": { "type": "string", "description": "Session key such as `claude-code:1b2c…`, a wake://session/… reference, or a wake://memory/… reference to read a memory file." },
                    "from_seq": { "type": "integer", "minimum": 0, "description": "Start at this message seq (inclusive). Default: the beginning." },
                    "max_messages": { "type": "integer", "minimum": 1, "maximum": 200, "default": 60, "description": "Messages per page." },
                    "max_chars": { "type": "integer", "minimum": 200, "maximum": 100000, "default": 20000, "description": "Character budget per page." },
                    "max_message_chars": { "type": "integer", "minimum": 100, "maximum": 50000, "default": 4000, "description": "Longer messages are truncated to this many characters." },
                    "include_tools": { "type": "boolean", "default": false, "description": "Include tool-call inputs and outputs (verbose)." },
                    "include_thinking": { "type": "boolean", "default": false, "description": "Include the assistant's thinking/reasoning text where the agent recorded it." },
                    "subagent": { "type": "string", "description": "Read this subagent transcript instead of the session's main transcript. The main transcript lists the ids at its end; paging works the same way." },
                },
                "required": ["key"],
            },
            "annotations": read_only_annotations(),
        }),
        json!({
            "name": LIST_PROJECTS,
            "title": "List indexed projects",
            "description": "Projects (working directories) that have coding-agent session history, most recently active first, with session counts. Use it when the user asks broadly what they have been working on (\"which projects did I touch this month?\") or to find the right `project` value for the other tools.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "since": since_param(),
                    "limit": { "type": "integer", "minimum": 1, "maximum": MAX_LIST_PROJECTS, "default": 50 },
                },
            },
            "annotations": read_only_annotations(),
        }),
        json!({
            "name": LIST_MEMORIES,
            "title": "List agent memory files",
            "description": "The memory files coding agents keep for themselves on this machine — Claude Code's and ZCode's per-project auto-memory (MEMORY.md and its topic files), Codex's memories — plus the instruction files the user keeps for them (CLAUDE.md, AGENTS.md, GEMINI.md, .cursor/rules, .kiro/steering, copilot-instructions.md), read-only, grouped by project, user memory (the notes that apply to every project) last. Use it when the user asks what an agent already knows or remembers about a project, or to reuse another agent's notes: decisions, conventions, gotchas. Each entry ends with a `wake://memory/<key>` reference; read one with wake_get_session.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "project": project_param(),
                    "agents": agents_param(),
                    "limit": { "type": "integer", "minimum": 1, "maximum": MAX_LIST_MEMORIES, "default": 50 },
                },
            },
            "annotations": read_only_annotations(),
        }),
    ]
}

fn call(ctx: &ToolContext, name: &str, args: &Value) -> ToolResult {
    match name {
        SEARCH => search(ctx, args),
        LIST_SESSIONS => list_sessions(ctx, args),
        GET_SESSION => get_session(ctx, args),
        LIST_PROJECTS => list_projects(ctx, args),
        LIST_MEMORIES => list_memories(ctx, args),
        other => Err(ToolError::InvalidParams(format!("Unknown tool: {other}"))),
    }
}

/// 用当前时刻构造 ToolContext 并跑一次工具。**MCP server 与 wake-cli 共用这
/// 一条**——now_ms 是两条路唯一可能各算各的东西(它只喂 parse_since),只留一
/// 个构造点就没有漂移的余地
pub fn invoke(
    store: &Store,
    adapters: &[Box<dyn AgentAdapter>],
    cache: &TranscriptCache,
    name: &str,
    args: &Value,
) -> ToolResult {
    let ctx = ToolContext {
        store,
        adapters,
        now_ms: crate::db::now_ms(),
        cache,
    };
    call(&ctx, name, args)
}

// ---------------------------------------------------------------- 参数读取

fn str_arg<'a>(args: &'a Value, name: &str) -> Result<Option<&'a str>, ToolError> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.as_str())),
        Some(_) => Err(ToolError::InvalidParams(format!(
            "`{name}` must be a string"
        ))),
    }
}

/// 去掉首尾空白、空串当没传的字符串参数(key / query / project / subagent 共用)
fn text_arg<'a>(args: &'a Value, name: &str) -> Result<Option<&'a str>, ToolError> {
    Ok(str_arg(args, name)?
        .map(str::trim)
        .filter(|s| !s.is_empty()))
}

fn int_arg(args: &Value, name: &str, default: i64, min: i64, max: i64) -> Result<i64, ToolError> {
    let v = match args.get(name) {
        None | Some(Value::Null) => return Ok(default),
        Some(Value::Number(n)) => n
            .as_i64()
            .or_else(|| n.as_f64().filter(|f| f.fract() == 0.0).map(|f| f as i64))
            .ok_or_else(|| ToolError::InvalidParams(format!("`{name}` must be an integer")))?,
        // 有的客户端把数字当字符串传
        Some(Value::String(s)) => s
            .trim()
            .parse::<i64>()
            .map_err(|_| ToolError::InvalidParams(format!("`{name}` must be an integer")))?,
        Some(_) => {
            return Err(ToolError::InvalidParams(format!(
                "`{name}` must be an integer"
            )))
        }
    };
    Ok(v.clamp(min, max))
}

fn bool_arg(args: &Value, name: &str, default: bool) -> Result<bool, ToolError> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(default),
        Some(Value::Bool(b)) => Ok(*b),
        Some(Value::String(s)) if s == "true" || s == "false" => Ok(s == "true"),
        Some(_) => Err(ToolError::InvalidParams(format!(
            "`{name}` must be a boolean"
        ))),
    }
}

fn agents_arg(args: &Value) -> Result<Vec<AgentId>, ToolError> {
    let raw: Vec<String> = match args.get("agents") {
        None | Some(Value::Null) => return Ok(Vec::new()),
        Some(Value::String(s)) => s.split(',').map(|s| s.to_string()).collect(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|v| {
                v.as_str().map(str::to_string).ok_or_else(|| {
                    ToolError::InvalidParams("`agents` must be an array of agent ids".into())
                })
            })
            .collect::<Result<_, _>>()?,
        Some(_) => {
            return Err(ToolError::InvalidParams(
                "`agents` must be an array of agent ids".into(),
            ))
        }
    };
    let mut out = Vec::new();
    for s in raw {
        let s = s.trim();
        if s.is_empty() {
            continue;
        }
        let agent = parse_agent(s).ok_or_else(|| {
            ToolError::InvalidParams(format!(
                "unknown agent `{s}`; valid ids: {}",
                AgentId::ALL
                    .iter()
                    .map(|a| a.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        })?;
        if !out.contains(&agent) {
            out.push(agent);
        }
    }
    Ok(out)
}

/// agent 名宽松解析:正式 id 优先,其次展示名(大小写/空格/连字符不敏感),
/// 再加几个常见简称
fn parse_agent(s: &str) -> Option<AgentId> {
    if let Some(a) = AgentId::from_str(s) {
        return Some(a);
    }
    let norm: String = s
        .to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric())
        .collect();
    AgentId::ALL
        .iter()
        .copied()
        .find(|a| {
            let id: String = a.as_str().chars().filter(|c| c.is_alphanumeric()).collect();
            let name: String = a
                .display_name()
                .to_lowercase()
                .chars()
                .filter(|c| c.is_alphanumeric())
                .collect();
            id == norm || name == norm
        })
        .or(match norm.as_str() {
            "claude" => Some(AgentId::ClaudeCode),
            "deepseek" => Some(AgentId::Dsh),
            "opencode2" | "opencodenext" => Some(AgentId::Opencode),
            "craft" => Some(AgentId::CraftAgents),
            _ => None,
        })
}

fn since_arg(ctx: &ToolContext, args: &Value) -> Result<Option<i64>, ToolError> {
    match str_arg(args, "since")? {
        None => Ok(None),
        Some(s) if s.trim().is_empty() => Ok(None),
        Some(s) => parse_since(s, ctx.now_ms).map(Some).ok_or_else(|| {
            ToolError::InvalidParams(format!(
                "`since` not understood: `{s}` (use 30m / 24h / 7d / 2w or an ISO date/time)"
            ))
        }),
    }
}

/// `project` 参数的三种结局。`NoMatch` 带的是给 LLM 的完整回复(含已知项目
/// 清单)——没匹配上不是错误,照常返回文本
enum ProjectScope {
    All,
    Paths(Vec<String>),
    NoMatch(String),
}

fn project_arg(ctx: &ToolContext, args: &Value) -> Result<ProjectScope, ToolError> {
    let Some(arg) = text_arg(args, "project")? else {
        return Ok(ProjectScope::All);
    };
    // 含归档:搜索覆盖归档会话,只剩归档会话的项目不能在这一步被挡掉
    let projects = ctx.store.list_projects(true)?;
    let paths = resolve_project_paths(arg, &projects);
    if !paths.is_empty() {
        return Ok(ProjectScope::Paths(paths));
    }
    let mut text = format!("No indexed project matches `{arg}`.\n\n");
    if projects.is_empty() {
        text.push_str("The index has no projects yet.");
    } else {
        text.push_str("Known projects (most recently active first):\n");
        for p in projects.iter().filter(|p| !p.path.is_empty()).take(15) {
            text.push_str(&format!(
                "- {} — {} · {} session{}\n",
                p.path,
                p.name,
                p.session_count,
                plural(p.session_count)
            ));
        }
        text.push_str("\nPass one of these paths (or the project name), or omit `project` to cover everything.");
    }
    Ok(ProjectScope::NoMatch(text))
}

/// 记忆面的 project 解包:没匹配上索引里的项目**不早退**——用户级记忆对每个项目都
/// 成立,筛选串按原样传下去(项目级本来就一份都不会匹配、用户级照列),提示文本
/// 另外带回给调用方决定怎么说
fn memory_project_scope(
    ctx: &ToolContext,
    args: &Value,
) -> Result<(Option<Vec<String>>, Option<String>), ToolError> {
    Ok(match project_arg(ctx, args)? {
        ProjectScope::All => (None, None),
        ProjectScope::Paths(p) => (Some(p), None),
        ProjectScope::NoMatch(text) => {
            let arg = text_arg(args, "project")?.unwrap_or_default().to_string();
            (Some(vec![arg]), Some(text))
        }
    })
}

/// 会话列表工具的 project 解包:没匹配上时直接把提示文本当结果返回
macro_rules! project_scope {
    ($ctx:expr, $args:expr) => {
        match project_arg($ctx, $args)? {
            ProjectScope::All => None,
            ProjectScope::Paths(p) => Some(p),
            ProjectScope::NoMatch(text) => return Ok(text),
        }
    };
}

// ---------------------------------------------------------------- 输出小工具

fn index_note(store: &Store) -> String {
    match store.latest_activity() {
        Ok(Some(t)) => format!(
            "Index covers activity up to {} (local time); Wake keeps it fresh while it is running.",
            fmt_time(Some(t))
        ),
        Ok(None) => "The index is empty — launch Wake to build it.".to_string(),
        Err(e) => format!("Index freshness unknown: {e:#}"),
    }
}

fn scope_note(
    project: &Option<Vec<String>>,
    agents: &[AgentId],
    since: Option<i64>,
    starred: bool,
) -> String {
    let mut parts = Vec::new();
    if let Some(paths) = project {
        parts.push(match paths.as_slice() {
            [one] => format!("project {one}"),
            many => format!("{} projects", many.len()),
        });
    }
    if !agents.is_empty() {
        parts.push(format!(
            "agents {}",
            agents
                .iter()
                .map(|a| a.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if let Some(t) = since {
        parts.push(format!("since {}", fmt_time(Some(t))));
    }
    if starred {
        parts.push("starred".to_string());
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!(" ({})", parts.join(", "))
    }
}

fn session_line(s: &SessionMeta) -> String {
    let mut tags = Vec::new();
    if !s.host.is_empty() {
        tags.push(format!("@{}", s.host));
    }
    if let Some(src) = s.source.as_deref().filter(|v| !v.is_empty()) {
        tags.push(format!("via {src}"));
    }
    if s.favorite {
        tags.push("★ starred".to_string());
    }
    if s.archived {
        tags.push("archived".to_string());
    }
    let tags = if tags.is_empty() {
        String::new()
    } else {
        format!(" · {}", tags.join(" · "))
    };
    let project = if s.project_path.is_empty() {
        "(unknown project)".to_string()
    } else {
        s.project_path.clone()
    };
    let model = s
        .model
        .as_deref()
        .filter(|m| !m.is_empty())
        .map(|m| format!(" · {m}"))
        .unwrap_or_default();
    format!(
        "`{}` · {} · \"{}\"\n  {} · updated {} · {} message{}{}{}\n",
        s.key,
        s.agent.display_name(),
        one_line(&s.title, 120),
        project,
        fmt_time(Some(s.updated_at)),
        s.message_count,
        plural(s.message_count),
        model,
        tags
    )
}

fn clean_snippet(s: &str) -> String {
    let marked = s
        .replace(HL_OPEN, "**")
        .replace(HL_CLOSE, "**")
        .replace("****", "");
    one_line(&marked, 240)
}

fn session_ref(key: &str, seq: i64) -> String {
    format!("wake://session/{key}#{seq}")
}

fn memory_ref(key: &str) -> String {
    format!("wake://memory/{key}")
}

/// `wake_get_session` 的 `key` 参数认的三种形态:裸会话 key、`wake://session/<key>#<seq>`
/// (seq 成为默认起点)、`wake://memory/<key>`(记忆文件,另一条读法)。写引用的
/// `session_ref` / `memory_ref` 与读引用的 `parse` 放在一起,两边一起改。记忆 key
/// 自身可含 `#`(SQLite 型虚拟路径 `<db>#<id>`),所以先认 scheme 再拆 seq
#[derive(Debug, PartialEq, Eq)]
enum WakeRef {
    Session { key: String, seq: Option<i64> },
    Memory { key: String },
}

impl WakeRef {
    fn parse(raw: &str) -> Self {
        let s = raw.trim();
        if let Some(key) = s.strip_prefix("wake://memory/") {
            return Self::Memory {
                key: key.trim().to_string(),
            };
        }
        let s = s.strip_prefix("wake://session/").unwrap_or(s);
        match s.rsplit_once('#') {
            Some((key, seq)) if seq.parse::<i64>().is_ok() => Self::Session {
                key: key.to_string(),
                seq: seq.parse().ok(),
            },
            _ => Self::Session {
                key: s.to_string(),
                seq: None,
            },
        }
    }
}

// ---------------------------------------------------------------- 工具实现

fn search(ctx: &ToolContext, args: &Value) -> ToolResult {
    let query = text_arg(args, "query")?
        .ok_or_else(|| ToolError::InvalidParams("`query` is required".into()))?;
    let agents = agents_arg(args)?;
    let since = since_arg(ctx, args)?;
    let limit = int_arg(args, "limit", 10, 1, MAX_SEARCH_SESSIONS)?;
    let (project, unmatched) = memory_project_scope(ctx, args)?;
    if let Some(text) = unmatched {
        // 会话没有这个项目,但用户级记忆对每个项目都成立——提示之余照样查一遍
        let memory = memory_hits_section(ctx, query, &agents, &project, since)?;
        let mut out = text;
        if !memory.is_empty() {
            // 提示文本不带收尾换行,记忆段自带前导空行;没命中就一字不加
            out.push('\n');
            out.push_str(memory.trim_end());
        }
        return Ok(out);
    }
    // 命中是消息级、按 bm25 排;一个会话能占掉前几十行,多取一些再按会话归组
    let fetch = (limit * 8).clamp(60, 400);
    let (hits, degraded) = ctx.store.search_with(
        query,
        &SearchFilter {
            agents: agents.clone(),
            project_paths: project.clone().unwrap_or_default(),
            updated_since: since,
            limit: fetch,
        },
    )?;
    let mut order: Vec<String> = Vec::new();
    let mut groups: HashMap<String, (SessionMeta, Vec<&SearchHit>)> = HashMap::new();
    for h in &hits {
        let entry = groups.entry(h.session.key.clone()).or_insert_with(|| {
            order.push(h.session.key.clone());
            (h.session.clone(), Vec::new())
        });
        if entry.1.len() < SNIPPETS_PER_SESSION {
            entry.1.push(h);
        }
    }
    let scope = scope_note(&project, &agents, since, false);
    // 记忆命中是另一类数据,不与会话命中混排,作为尾巴附在两种结果后面
    let memory = memory_hits_section(ctx, query, &agents, &project, since)?;
    let mut out = String::new();
    if order.is_empty() {
        out.push_str(&format!("No session matches `{query}`{scope}.\n"));
        if degraded {
            out.push_str("Note: terms shorter than 3 characters use a substring scan; try a longer or more specific term.\n");
        }
        if memory.is_empty() {
            out.push_str("Try fewer or different terms, drop the project/agent/since filters, or list sessions with wake_list_sessions.\n\n");
        }
        out.push_str(&memory);
        out.push_str(&index_note(ctx.store));
        return Ok(out);
    }
    let shown = order.len().min(limit as usize);
    out.push_str(&format!(
        "{} session{} match `{query}`{scope}{} — showing {shown}, best matches first.\n",
        order.len(),
        plural(order.len() as i64),
        if hits.len() as i64 >= fetch {
            " (more may exist)"
        } else {
            ""
        }
    ));
    if degraded {
        out.push_str("Note: terms shorter than 3 characters use a slower substring scan.\n");
    }
    out.push('\n');
    for (ix, key) in order.iter().take(shown).enumerate() {
        let (meta, snippets) = &groups[key];
        out.push_str(&format!("{}. {}", ix + 1, session_line(meta)));
        for h in snippets {
            // 标题命中没有对应消息:只报"标题里有",引用落到会话开头
            if h.role == "title" {
                out.push_str(&format!(
                    "  - title: {}\n    ref: {}\n",
                    clean_snippet(&h.snippet),
                    session_ref(&meta.key, 0)
                ));
                continue;
            }
            let when = h
                .timestamp
                .filter(|t| *t > 0)
                .map(|t| format!(", {}", fmt_time(Some(t))))
                .unwrap_or_default();
            out.push_str(&format!(
                "  - seq {} ({}{}): {}\n    ref: {}\n",
                h.seq,
                h.role,
                when,
                clean_snippet(&h.snippet),
                session_ref(&meta.key, h.seq)
            ));
        }
        out.push('\n');
    }
    out.push_str(
        "Read a session with wake_get_session (pass the key, or a ref to start at that message).\n",
    );
    out.push_str(&memory);
    out.push_str(&index_note(ctx.store));
    Ok(out)
}

/// wake_search 末尾附带的记忆命中:agent 自己写的记忆里提到这个词的几份,带
/// `wake://memory/…` 引用。整段前后各一个空行,调用方直接拼、不用管分隔;没有就是空串
fn memory_hits_section(
    ctx: &ToolContext,
    query: &str,
    agents: &[AgentId],
    project: &Option<Vec<String>>,
    since: Option<i64>,
) -> Result<String, ToolError> {
    // since 同样约束记忆命中:说了 "in the last 24 hours" 就不能跟着几个月前的笔记
    let hits = ctx.store.search_memories(
        query,
        &MemoryFilter {
            agents: agents.to_vec(),
            project_paths: project.clone().unwrap_or_default(),
            unattributed: false,
            user: UserMemories::Alongside,
            updated_since: since,
            limit: MEMORY_HITS_IN_SEARCH,
        },
    )?;
    if hits.is_empty() {
        return Ok(String::new());
    }
    let mut out = format!("\nMemory files that mention `{query}`:\n");
    for h in &hits {
        out.push_str(&format!(
            "- {}{} · {}: {}\n  ref: {}\n",
            memory_label(&h.doc),
            memory_session_note(&h.doc),
            memory_group(&h.doc),
            clean_snippet(&h.snippet),
            memory_ref(&h.doc.key)
        ));
    }
    out.push('\n');
    Ok(out)
}

/// `<标题> · <agent> · <层级>[ · @host]`
fn memory_label(d: &MemoryDoc) -> String {
    // 标题入库时已按 MEMORY_TITLE_MAX 折行封顶;老库里的行要等文件变了才重读,这里再兜一道
    let mut s = format!(
        "{} · {} · {} memory",
        one_line(&d.title, crate::adapters::MEMORY_TITLE_MAX),
        d.agent.display_name(),
        d.scope.label()
    );
    if !d.host.is_empty() {
        s.push_str(&format!(" · @{}", d.host));
    }
    s
}

/// 记忆归属哪一组(列表的组头、搜索命中的归属都用它);分组判据在 `MemoryDoc::group`
fn memory_group(d: &MemoryDoc) -> String {
    match d.group() {
        MemoryGroup::User => "User memory (applies to every project)".to_string(),
        MemoryGroup::Unknown => "Unknown project".to_string(),
        MemoryGroup::Project { path, name } => format!("{path} — {name}"),
    }
}

/// 线程级记忆才标它所属的会话;项目级的 session_key 只是解析项目用的锚点,不露出
fn memory_session_note(d: &MemoryDoc) -> String {
    if d.scope == MemoryScope::Thread && !d.session_key.is_empty() {
        format!(" · session `{}`", d.session_key)
    } else {
        String::new()
    }
}

fn list_memories(ctx: &ToolContext, args: &Value) -> ToolResult {
    let agents = agents_arg(args)?;
    let limit = int_arg(args, "limit", 50, 1, MAX_LIST_MEMORIES)?;
    let (project, unmatched) = memory_project_scope(ctx, args)?;
    let docs = ctx.store.list_memories(&MemoryFilter {
        agents: agents.clone(),
        project_paths: project.clone().unwrap_or_default(),
        unattributed: false,
        user: UserMemories::Alongside,
        updated_since: None,
        limit,
    })?;
    let scope = scope_note(&project, &agents, None, false);
    let mut out = String::new();
    if let Some(text) = unmatched {
        let first_line = text.lines().next().unwrap_or_default();
        out.push_str(&format!(
            "{first_line} Only user memory, which applies everywhere, is listed below.\n\n"
        ));
    }
    if docs.is_empty() {
        out.push_str(&format!(
            "No memory files{scope}. Claude Code writes them under ~/.claude/projects/<project>/memory/ once it has saved something about a project; Codex keeps its own under ~/.codex/memories/; ZCode under ~/.zcode/cli/memories/projects/; instruction files such as CLAUDE.md or AGENTS.md are listed from the project roots Wake has sessions for. Wake only lists what is there.\n\n"
        ));
        out.push_str(&index_note(ctx.store));
        return Ok(out);
    }
    out.push_str(&format!(
        "{} memory file{}{scope}, grouped by project (user memory last; the limit applies to project memory, user memory is always included).\n",
        docs.len(),
        plural(docs.len() as i64)
    ));
    // store 已按 (用户级最后, 项目路径, 新到旧) 排好,这里只在组变化时打组头
    let mut current: Option<String> = None;
    for d in &docs {
        let group = memory_group(d);
        if current.as_deref() != Some(group.as_str()) {
            out.push_str(&format!("\n## {group}\n"));
            current = Some(group);
        }
        out.push_str(&format!(
            "- {}{} · updated {}\n  ref: {}\n",
            memory_label(d),
            memory_session_note(d),
            fmt_time(Some(d.updated_at)),
            memory_ref(&d.key)
        ));
    }
    out.push_str("\nRead one with wake_get_session using its wake://memory/… reference.\n");
    out.push_str(&index_note(ctx.store));
    Ok(out)
}

/// `wake_get_session` 收到 `wake://memory/<key>` 引用时的读法:标题行 + 归属 + 来源
/// 路径,然后是正文。文件型现场读磁盘(agent 刚改过也能看到),读不到退库里那份;
/// SQLite 型只有库里那份。同一个 `max_chars` 预算,超了截断并说明
/// `wake_get_session` 读记忆时一页的字符上限(与会话页同数);`int_arg` 静默封顶
const MAX_GET_CHARS: usize = 100_000;

fn get_memory(ctx: &ToolContext, key: &str, args: &Value) -> ToolResult {
    let max_chars = int_arg(args, "max_chars", 20_000, 200, MAX_GET_CHARS as i64)? as usize;
    let Some(doc) = ctx.store.get_memory(key)? else {
        return Err(ToolError::Failed(format!(
            "No memory file with key `{key}`; get a wake://memory/… reference from wake_list_memories or wake_search."
        )));
    };
    let body = crate::adapters::memory_body(&doc);
    let mut out = format!(
        "# {}\nkey: `{}` · {} · {} memory",
        doc.title,
        doc.key,
        doc.agent.display_name(),
        doc.scope.label()
    );
    if !doc.project_path.is_empty() {
        out.push_str(&format!(" · project {}", doc.project_path));
    }
    out.push_str(&memory_session_note(&doc));
    if !doc.host.is_empty() {
        out.push_str(&format!(" · @{}", doc.host));
    }
    out.push_str(&format!(
        " · updated {}\nsource: {}\n\n",
        fmt_time(Some(doc.updated_at)),
        doc.path
    ));
    let (text, truncated) = crate::adapters::parse_utils::clip_chars(&body, max_chars);
    out.push_str(&text);
    if !text.ends_with('\n') {
        out.push('\n');
    }
    if truncated {
        // 说实话:max_chars 静默封顶在 MAX_GET_CHARS,超过它没有翻页可言,只能指向
        // 文件本身——原先一律劝"传更大的 max_chars",顶格的调用方会原地打转
        if max_chars < MAX_GET_CHARS {
            out.push_str(&format!(
                "\n[truncated at {max_chars} of {} characters — pass a larger max_chars (up to {MAX_GET_CHARS}) for the rest]\n",
                body.chars().count()
            ));
        } else {
            out.push_str(&format!(
                "\n[truncated at {max_chars} characters; the file is {} characters and that is the largest page this tool reads — open {} for the rest]\n",
                body.chars().count(),
                doc.path
            ));
        }
    }
    Ok(out)
}

fn list_sessions(ctx: &ToolContext, args: &Value) -> ToolResult {
    let agents = agents_arg(args)?;
    let since = since_arg(ctx, args)?;
    let starred = bool_arg(args, "starred", false)?;
    let limit = int_arg(args, "limit", 20, 1, MAX_LIST_SESSIONS)?;
    let project = project_scope!(ctx, args);
    // 字段全列、不带 ..Default:新增筛选字段时这里必须表态(与 workbench
    // current_filter 同一约定)
    let (sessions, total) = ctx.store.list_sessions(&SessionFilter {
        agents: agents.clone(),
        favorite_only: starred,
        include_archived: false,
        roots_only: true,
        title_query: None,
        sort: SortKey::Updated,
        ascending: false,
        limit,
        offset: 0,
        updated_since: since,
        project_paths: project.clone().unwrap_or_default(),
        // "最近"就是最近:GUI 的置顶优先在这里会让 limit 先被旧置顶会话占掉
        ignore_pins: true,
    })?;
    let scope = scope_note(&project, &agents, since, starred);
    let mut out = String::new();
    if sessions.is_empty() {
        out.push_str(&format!("No sessions{scope}.\n\n"));
        out.push_str(&index_note(ctx.store));
        return Ok(out);
    }
    out.push_str(&format!(
        "{total} session{}{scope} — showing the {} most recently updated.\n\n",
        plural(total),
        sessions.len()
    ));
    for s in &sessions {
        out.push_str("- ");
        out.push_str(&session_line(s));
    }
    out.push_str("\nRead one with wake_get_session using its key.\n");
    out.push_str(&index_note(ctx.store));
    Ok(out)
}

fn list_projects(ctx: &ToolContext, args: &Value) -> ToolResult {
    let since = since_arg(ctx, args)?;
    let limit = int_arg(args, "limit", 50, 1, MAX_LIST_PROJECTS)? as usize;
    let projects: Vec<ProjectInfo> = ctx
        .store
        .list_projects(false)?
        .into_iter()
        .filter(|p| since.is_none_or(|t| p.last_active >= t))
        .collect();
    let window = since
        .map(|t| format!(" active since {}", fmt_time(Some(t))))
        .unwrap_or_default();
    let mut out = String::new();
    if projects.is_empty() {
        out.push_str(&format!("No projects{window}.\n\n"));
        out.push_str(&index_note(ctx.store));
        return Ok(out);
    }
    out.push_str(&format!(
        "{} project{}{window} — most recently active first{}.\n\n",
        projects.len(),
        plural(projects.len() as i64),
        if projects.len() > limit {
            format!(", showing {limit}")
        } else {
            String::new()
        }
    ));
    for p in projects.iter().take(limit) {
        let path = if p.path.is_empty() {
            "(unknown project)".to_string()
        } else {
            p.path.clone()
        };
        out.push_str(&format!(
            "- {} — {} · {} session{} · last active {}\n",
            path,
            if p.name.is_empty() { "?" } else { &p.name },
            p.session_count,
            plural(p.session_count),
            fmt_time(Some(p.last_active))
        ));
    }
    out.push_str("\nUse a path as `project` in wake_list_sessions or wake_search.\n");
    out.push_str(&index_note(ctx.store));
    Ok(out)
}

fn find_session(store: &Store, key: &str) -> Result<Result<SessionMeta, String>, ToolError> {
    if let Some(meta) = store.get_session(key)? {
        return Ok(Ok(meta));
    }
    // 兜底:对方只拿到原生 id(resume 用的那个),按列反查;同 UUID 跨 host 会多于一条
    let candidates = store.find_by_native_id(key)?;
    Ok(match candidates.as_slice() {
        [one] => Ok(one.clone()),
        [] => Err(format!(
            "No session with key `{key}`. Keys look like `claude-code:<id>` (or `<agent>:<host>:<id>` for remote hosts); get one from wake_search or wake_list_sessions."
        )),
        many => Err(format!(
            "`{key}` is ambiguous — {} sessions share that id:\n{}",
            many.len(),
            many.iter()
                .map(|s| format!("- `{}` ({}{})", s.key, s.agent.display_name(), if s.host.is_empty() { String::new() } else { format!(" @{}", s.host) }))
                .collect::<Vec<_>>()
                .join("\n")
        )),
    })
}

/// 主线页脚最多列多少个子代理转录;再多就 "… and N more"(Task 开得多的会话有上百个)
const MAX_SUBAGENTS_LISTED: usize = 30;
/// 子会话页脚的同款预算(与上面那个各自独立:一条会话可以既有子会话又有
/// 侧边转录,两张清单不共享额度)
const MAX_CHILDREN_LISTED: usize = 30;

/// 挂在这条会话下面的子会话。它们是完整会话(各有 key、各自可分页读),
/// 但 `wake_list_sessions` 只给根(roots_only),所以父会话是唯一的发现
/// 入口——Codex 的 `spawn_agent` 子代理、Grok 的子会话都走这里。
/// 与侧边的 subagent 转录不是一回事:那些没有自己的 key,按 id 读
fn child_sessions(ctx: &ToolContext, parent_key: &str) -> Vec<SessionMeta> {
    // 字段全列、不带 ..Default:新增筛选字段时这里必须表态(与 workbench
    // current_filter 同一约定)。list_children 不在 SQL 里限量,上限在下面截
    ctx.store
        .list_children(
            parent_key,
            &SessionFilter {
                agents: Vec::new(),
                favorite_only: false,
                include_archived: false,
                roots_only: false,
                title_query: None,
                sort: SortKey::Updated,
                ascending: false,
                limit: 0,
                offset: 0,
                updated_since: None,
                project_paths: Vec::new(),
                ignore_pins: true,
            },
        )
        .unwrap_or_default()
}

fn child_lines(children: &[SessionMeta]) -> String {
    let mut out = String::new();
    for child in children.iter().take(MAX_CHILDREN_LISTED) {
        out.push_str(&format!(
            "- `{}` — \"{}\" · {} message{}\n",
            child.key,
            one_line(&child.title, 120),
            child.message_count,
            plural(child.message_count)
        ));
    }
    if children.len() > MAX_CHILDREN_LISTED {
        out.push_str(&format!(
            "- … and {} more\n",
            children.len() - MAX_CHILDREN_LISTED
        ));
    }
    out
}

/// `agent-a1b2 — Explore: find the watcher code`;没有边车信息时只有 id
fn subagent_label(sc: &SidechainInfo) -> String {
    let desc = one_line(&sc.label(), 80);
    if desc.is_empty() {
        sc.id.clone()
    } else {
        format!("{} — {desc}", sc.id)
    }
}

fn subagent_lines(sidechains: &[SidechainInfo], cap: Option<usize>) -> String {
    let shown = cap.unwrap_or(sidechains.len());
    let mut out = String::new();
    for sc in sidechains.iter().take(shown) {
        out.push_str(&format!("- {}\n", subagent_label(sc)));
    }
    if sidechains.len() > shown {
        out.push_str(&format!(
            "- … and {} more — pass subagent=\"*\" for the full list\n",
            sidechains.len() - shown
        ));
    }
    out
}

/// `subagent="*"`:只列清单、不读转录、不设上限——主线页脚的清单封顶 30 条,Task 开
/// 得多的会话得有地方拿到其余的 id(Codex review 2026-09-14)
fn subagent_listing(title: &str, meta: &SessionMeta, sidechains: &[SidechainInfo]) -> String {
    let mut out = format!(
        "# {title}\nkey: `{}` · agent: {}\n\n",
        meta.key,
        meta.agent.display_name()
    );
    if sidechains.is_empty() {
        out.push_str("This session has no subagent transcripts.\n");
    } else {
        out.push_str(&format!(
            "{} subagent transcript{} (pass an id as `subagent` to read one):\n{}",
            sidechains.len(),
            plural(sidechains.len() as i64),
            subagent_lines(sidechains, None)
        ));
    }
    out
}

fn get_session(ctx: &ToolContext, args: &Value) -> ToolResult {
    let raw_key = text_arg(args, "key")?
        .ok_or_else(|| ToolError::InvalidParams("`key` is required".into()))?;
    // 记忆引用走另一条读法;不另开工具,agent 拿到什么引用都交给同一个入口
    let (key, ref_seq) = match WakeRef::parse(raw_key) {
        WakeRef::Memory { key } => return get_memory(ctx, &key, args),
        WakeRef::Session { key, seq } => (key, seq),
    };
    let from_seq = int_arg(args, "from_seq", ref_seq.unwrap_or(0), 0, i64::MAX)?;
    let opts = CompactOptions {
        from_seq,
        max_messages: int_arg(args, "max_messages", 60, 1, 200)? as usize,
        max_chars: int_arg(args, "max_chars", 20_000, 200, 100_000)? as usize,
        max_message_chars: int_arg(args, "max_message_chars", 4_000, 100, 50_000)? as usize,
        include_tools: bool_arg(args, "include_tools", false)?,
        include_thinking: bool_arg(args, "include_thinking", false)?,
    };
    let subagent = text_arg(args, "subagent")?;
    let meta = match find_session(ctx.store, &key)? {
        Ok(m) => m,
        Err(text) => return Err(ToolError::Failed(text)),
    };
    let adapter = adapter_for(ctx.adapters, meta.agent, &meta.file_path).ok_or_else(|| {
        ToolError::Failed(format!(
            "No adapter can read `{}` ({}) — its data location may be disabled in Wake's settings.",
            meta.key,
            meta.agent.display_name()
        ))
    })?;
    let transcript = ctx.cache.get_or_parse(adapter, &meta).map_err(|e| {
        ToolError::Failed(format!(
            "Could not read the transcript of `{}` from {}: {e:#}",
            meta.key, meta.file_path
        ))
    })?;
    let live = &transcript.meta;
    let title = if live.title.is_empty() {
        UNTITLED
    } else {
        &live.title
    };
    // 子代理转录按 id 现场解析,不进 TranscriptCache:子代理还在跑时它的文件独立于
    // 主文件增长,拿主文件的戳当键会一直吐旧内容;文件通常远小于主线,一页一解析。
    // 先读文件、再查缓存里的清单:清单来自缓存的主转录,主文件没变时看不见刚出现的
    // 子代理(Codex review 2026-09-14),读得到就算存在,边车信息缺就只报 id
    let sidechain = match subagent {
        None => None,
        Some("*") => return Ok(subagent_listing(title, &meta, &transcript.sidechains)),
        Some(id) => {
            if id.contains(std::path::is_separator) || id == "." || id == ".." {
                return Err(ToolError::InvalidParams(format!(
                    "`subagent` must be a bare id as listed by the main transcript, not a path: `{id}`"
                )));
            }
            let messages = adapter
                .load_sidechain(&SessionFileRef::from_meta(&meta), id)
                .map_err(|e| {
                    ToolError::Failed(format!(
                        "Could not read subagent transcript `{id}` of `{}`: {e:#}",
                        meta.key
                    ))
                })?;
            if messages.is_empty() {
                return Err(ToolError::Failed(if transcript.sidechains.is_empty() {
                    format!("`{}` has no subagent transcripts.", meta.key)
                } else {
                    format!(
                        "`{}` has no subagent transcript `{id}`. It has:\n{}",
                        meta.key,
                        subagent_lines(&transcript.sidechains, None)
                    )
                }));
            }
            let info = transcript
                .sidechains
                .iter()
                .find(|sc| sc.id == id)
                .cloned()
                .unwrap_or_else(|| SidechainInfo {
                    id: id.to_string(),
                    agent_type: None,
                    description: None,
                    tool_use_id: None,
                });
            Some((info, messages))
        }
    };
    let sub_id = sidechain.as_ref().map(|(info, _)| info.id.as_str());
    let messages: &[TranscriptMessage] = match &sidechain {
        Some((_, messages)) => messages,
        None => &transcript.mainline,
    };
    let total_visible = messages
        .iter()
        .filter(|m| m.kind != MessageKind::Meta)
        .count();
    let last_seq = messages.last().map(|m| m.seq);
    let page = render_compact(messages, &opts);

    let mut out = String::new();
    out.push_str(&format!("# {title}\n"));
    let mut facts = vec![
        format!("key: `{}`", meta.key),
        format!("agent: {}", meta.agent.display_name()),
    ];
    if !meta.host.is_empty() {
        facts.push(format!("host: @{}", meta.host));
    }
    // 子会话给出回父会话的路:它自己不在 wake_list_sessions 的结果里,
    // 读者多半是顺着搜索命中落进来的,得知道上下文挂在哪
    if let Ok(Some(parent)) = ctx.store.parent_key_of(&meta.key) {
        facts.push(format!("parent: `{parent}`"));
    }
    if let Some((info, _)) = &sidechain {
        facts.push(format!("subagent: {}", subagent_label(info)));
    }
    if !live.project_path.is_empty() {
        facts.push(format!(
            "project: {}{}",
            live.project_path,
            live.git_branch
                .as_deref()
                .filter(|b| !b.is_empty())
                .map(|b| format!(" ({b})"))
                .unwrap_or_default()
        ));
    }
    if let Some(m) = live.model.as_deref().filter(|m| !m.is_empty()) {
        facts.push(format!("model: {m}"));
    }
    if let Some(src) = meta.source.as_deref().filter(|v| !v.is_empty()) {
        facts.push(format!("via {src}"));
    }
    // 时间范围:主线用会话的起止,子代理用它自己消息的首末时间(没有就不写)
    let range = match &sidechain {
        Some((_, messages)) => {
            let ts = messages.iter().filter_map(|m| m.timestamp);
            ts.clone().min().zip(ts.max())
        }
        None => Some((live.created_at, live.updated_at)),
    };
    if let Some((start, end)) = range {
        facts.push(format!(
            "{} – {}",
            fmt_time(Some(start)),
            fmt_time(Some(end))
        ));
    }
    facts.push(format!(
        "{total_visible} message{}",
        plural(total_visible as i64)
    ));
    out.push_str(&facts.join(" · "));
    out.push_str("\n\n");

    // 子会话页脚在空页上也得给:`wake_list_sessions` 只回根会话,父会话是
    // 它们唯一的发现入口,而"这一页什么都没渲染"(整条都是 Meta、from_seq
    // 翻过了尾)与"没有子会话"是两回事
    let children = if sub_id.is_none() {
        child_sessions(ctx, &meta.key)
    } else {
        Vec::new()
    };
    let children_footer = |out: &mut String| {
        if children.is_empty() {
            return;
        }
        out.push_str(&format!(
            "{} child session{} (read one with {GET_SESSION} using its key):\n{}",
            children.len(),
            plural(children.len() as i64),
            child_lines(&children)
        ));
    };

    if page.rendered == 0 {
        out.push_str(&match last_seq {
            Some(last) => format!(
                "No messages at or after seq {from_seq} (the transcript ends at seq {last}).\n"
            ),
            None => "This transcript has no messages.\n".to_string(),
        });
        children_footer(&mut out);
        return Ok(out);
    }
    out.push_str(&page.text);
    out.push_str("—\n");
    let (first, last) = page.seq_range.unwrap_or((from_seq, from_seq));
    let mut summary = format!(
        "Showing seq {first}–{last}{}: {} message{}",
        sub_id
            .map(|id| format!(" of subagent `{id}`"))
            .unwrap_or_default(),
        page.rendered,
        plural(page.rendered as i64)
    );
    if page.skipped_meta > 0 {
        summary.push_str(&format!(
            "; {} injected-context message{} omitted",
            page.skipped_meta,
            plural(page.skipped_meta as i64)
        ));
    }
    out.push_str(&summary);
    out.push_str(".\n");
    // 主线页脚两张清单:挂在这条会话下面的子会话(有自己的 key,按 key 读),
    // 与侧边的子代理转录(不并进主线、各有自己的 seq,按 id 读)
    children_footer(&mut out);
    if sub_id.is_none() {
        if !transcript.sidechains.is_empty() {
            out.push_str(&format!(
                "{} subagent transcript{} (pass an id as `subagent` to read one):\n{}",
                transcript.sidechains.len(),
                plural(transcript.sidechains.len() as i64),
                subagent_lines(&transcript.sidechains, Some(MAX_SUBAGENTS_LISTED))
            ));
        }
    }
    let with_sub = sub_id
        .map(|id| format!("subagent=\"{id}\" and "))
        .unwrap_or_default();
    let noun = if sub_id.is_some() {
        "subagent transcript"
    } else {
        "transcript"
    };
    match page.next_seq {
        Some(next) => out.push_str(&format!(
            "More follows — call {GET_SESSION} again with {with_sub}from_seq={next}.\n"
        )),
        None => out.push_str(&format!("End of {noun}.\n")),
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_names_parse_leniently() {
        assert_eq!(parse_agent("claude-code"), Some(AgentId::ClaudeCode));
        assert_eq!(parse_agent("Claude Code"), Some(AgentId::ClaudeCode));
        assert_eq!(parse_agent("claude"), Some(AgentId::ClaudeCode));
        assert_eq!(parse_agent("Gemini CLI"), Some(AgentId::Gemini));
        assert_eq!(parse_agent("oh-my-pi"), Some(AgentId::Omp));
        assert_eq!(parse_agent("DeepSeek"), Some(AgentId::Dsh));
        assert_eq!(parse_agent("opencode2"), Some(AgentId::Opencode));
        assert_eq!(parse_agent("chatgpt"), None);
    }

    #[test]
    fn key_arg_accepts_wake_refs() {
        let session = |key: &str, seq: Option<i64>| WakeRef::Session {
            key: key.to_string(),
            seq,
        };
        assert_eq!(
            WakeRef::parse(&session_ref("claude-code:abc", 12)),
            session("claude-code:abc", Some(12))
        );
        assert_eq!(
            WakeRef::parse("codex:devbox:0195-xyz"),
            session("codex:devbox:0195-xyz", None)
        );
        assert_eq!(
            WakeRef::parse(" claude-code:abc "),
            session("claude-code:abc", None)
        );
        // 记忆 key 自身带 `#`(SQLite 型虚拟路径)也不能被当成 seq 拆掉
        let memory_key = "codex:/home/me/.codex/memories_1.sqlite#t-0001";
        assert_eq!(
            WakeRef::parse(&memory_ref(memory_key)),
            WakeRef::Memory {
                key: memory_key.to_string()
            }
        );
    }

    #[test]
    fn int_args_clamp_and_accept_numeric_strings() {
        let args = json!({ "limit": 500, "n": "7", "f": 3.0, "bad": "x" });
        assert_eq!(int_arg(&args, "limit", 10, 1, 30).unwrap(), 30);
        assert_eq!(int_arg(&args, "n", 10, 1, 30).unwrap(), 7);
        assert_eq!(int_arg(&args, "f", 10, 1, 30).unwrap(), 3);
        assert_eq!(int_arg(&args, "missing", 10, 1, 30).unwrap(), 10);
        assert!(matches!(
            int_arg(&args, "bad", 10, 1, 30),
            Err(ToolError::InvalidParams(_))
        ));
    }

    #[test]
    fn snippets_drop_highlight_sentinels() {
        let raw = format!("前文 {HL_OPEN}二维码{HL_CLOSE} 后文\n换行");
        assert_eq!(clean_snippet(&raw), "前文 **二维码** 后文 换行");
    }

    #[test]
    fn definitions_are_stable() {
        let defs = definitions();
        let names: Vec<&str> = defs.iter().map(|d| d["name"].as_str().unwrap()).collect();
        assert_eq!(names, NAMES);
        for d in &defs {
            assert_eq!(d["inputSchema"]["type"], "object");
            assert_eq!(d["annotations"]["readOnlyHint"], true);
        }
    }
}
