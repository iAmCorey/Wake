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

pub struct ToolContext<'a> {
    pub store: &'a Store,
    pub adapters: &'a [Box<dyn AgentAdapter>],
    pub now_ms: i64,
    pub cache: &'a TranscriptCache,
}

/// 单槽转录缓存:分页读同一会话时不必每页整文件重解析(默认 20k 字符一页,
/// 大会话几十页)。键是 scanner 同款的脏判据 (file_path, mtime, size),文件一
/// 变即失效;server 进程随客户端会话长驻,连续翻页总是同一文件
#[derive(Default)]
pub struct TranscriptCache(Mutex<Option<(String, i64, i64, Arc<ParsedTranscript>)>>);

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
        if let Some((path, mtime, size, t)) = self.0.lock().unwrap().as_ref() {
            if *path == r.file_path && *mtime == stamp && *size == r.size {
                return Ok(t.clone());
            }
        }
        let t = Arc::new(adapter.parse_transcript(&r)?);
        *self.0.lock().unwrap() = Some((r.file_path.clone(), stamp, r.size, t.clone()));
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

const MAX_SEARCH_SESSIONS: i64 = 30;
const MAX_LIST_SESSIONS: i64 = 100;
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
            "description": "Full-text search across every indexed coding-agent session on this machine (user prompts, assistant replies, tool names and inputs). Use it when the user asks whether something was discussed, tried or solved before, or wants the conversation about a topic, an error message, a file or a decision — git history does not hold that. Terms are ANDed; CJK text and code substrings like `useEffect(` work. Returns matching sessions with up to three snippets each, plus a `wake://session/<key>#<seq>` reference per snippet that you can read with wake_get_session.",
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
            "description": "Read one session's transcript, parsed live from the agent's own files, as compact Markdown: user and assistant messages with `[seq N]` markers, tool calls folded to one line each, injected context omitted. Use it after wake_search or wake_list_sessions to see what actually happened — the reasoning, the decisions and the exact steps of an earlier session. Paginated: when the reply ends with a `from_seq` hint, call again with it to continue. Accepts a session key or a `wake://session/<key>#<seq>` reference (the seq becomes the starting point).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "key": { "type": "string", "description": "Session key such as `claude-code:1b2c…`, or a wake://session/… reference." },
                    "from_seq": { "type": "integer", "minimum": 0, "description": "Start at this message seq (inclusive). Default: the beginning." },
                    "max_messages": { "type": "integer", "minimum": 1, "maximum": 200, "default": 60, "description": "Messages per page." },
                    "max_chars": { "type": "integer", "minimum": 200, "maximum": 100000, "default": 20000, "description": "Character budget per page." },
                    "max_message_chars": { "type": "integer", "minimum": 100, "maximum": 50000, "default": 4000, "description": "Longer messages are truncated to this many characters." },
                    "include_tools": { "type": "boolean", "default": false, "description": "Include tool-call inputs and outputs (verbose)." },
                    "include_thinking": { "type": "boolean", "default": false, "description": "Include the assistant's thinking/reasoning text where the agent recorded it." },
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
    ]
}

pub fn call(ctx: &ToolContext, name: &str, args: &Value) -> ToolResult {
    match name {
        SEARCH => search(ctx, args),
        LIST_SESSIONS => list_sessions(ctx, args),
        GET_SESSION => get_session(ctx, args),
        LIST_PROJECTS => list_projects(ctx, args),
        other => Err(ToolError::InvalidParams(format!("Unknown tool: {other}"))),
    }
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
    let Some(arg) = str_arg(args, "project")?
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
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

/// 两个列表工具共用的 project 解包:没匹配上时直接把提示文本当结果返回
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

// ---------------------------------------------------------------- 工具实现

fn search(ctx: &ToolContext, args: &Value) -> ToolResult {
    let query = str_arg(args, "query")?
        .map(str::trim)
        .filter(|q| !q.is_empty())
        .ok_or_else(|| ToolError::InvalidParams("`query` is required".into()))?;
    let agents = agents_arg(args)?;
    let since = since_arg(ctx, args)?;
    let limit = int_arg(args, "limit", 10, 1, MAX_SEARCH_SESSIONS)?;
    let project = project_scope!(ctx, args);
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
    let mut out = String::new();
    if order.is_empty() {
        out.push_str(&format!("No matches for `{query}`{scope}.\n"));
        if degraded {
            out.push_str("Note: terms shorter than 3 characters use a substring scan; try a longer or more specific term.\n");
        }
        out.push_str("Try fewer or different terms, drop the project/agent/since filters, or list sessions with wake_list_sessions.\n\n");
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
    out.push_str(&index_note(ctx.store));
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

/// `key` 也接受 `wake://session/<key>#<seq>` 引用(seq 成为默认起点)
fn parse_key_arg(raw: &str) -> (String, Option<i64>) {
    let s = raw.trim();
    let s = s.strip_prefix("wake://session/").unwrap_or(s);
    match s.rsplit_once('#') {
        Some((key, seq)) if seq.parse::<i64>().is_ok() => (key.to_string(), seq.parse().ok()),
        _ => (s.to_string(), None),
    }
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

fn get_session(ctx: &ToolContext, args: &Value) -> ToolResult {
    let raw_key = str_arg(args, "key")?
        .map(str::trim)
        .filter(|k| !k.is_empty())
        .ok_or_else(|| ToolError::InvalidParams("`key` is required".into()))?;
    let (key, ref_seq) = parse_key_arg(raw_key);
    let from_seq = int_arg(args, "from_seq", ref_seq.unwrap_or(0), 0, i64::MAX)?;
    let opts = CompactOptions {
        from_seq,
        max_messages: int_arg(args, "max_messages", 60, 1, 200)? as usize,
        max_chars: int_arg(args, "max_chars", 20_000, 200, 100_000)? as usize,
        max_message_chars: int_arg(args, "max_message_chars", 4_000, 100, 50_000)? as usize,
        include_tools: bool_arg(args, "include_tools", false)?,
        include_thinking: bool_arg(args, "include_thinking", false)?,
    };
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
    let total_visible = transcript
        .mainline
        .iter()
        .filter(|m| m.kind != MessageKind::Meta)
        .count();
    let last_seq = transcript.mainline.last().map(|m| m.seq);
    let page = render_compact(&transcript.mainline, &opts);

    let mut out = String::new();
    let title = if live.title.is_empty() {
        UNTITLED
    } else {
        &live.title
    };
    out.push_str(&format!("# {title}\n"));
    let mut facts = vec![
        format!("key: `{}`", meta.key),
        format!("agent: {}", meta.agent.display_name()),
    ];
    if !meta.host.is_empty() {
        facts.push(format!("host: @{}", meta.host));
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
    facts.push(format!(
        "{} – {}",
        fmt_time(Some(live.created_at)),
        fmt_time(Some(live.updated_at))
    ));
    facts.push(format!(
        "{total_visible} message{}",
        plural(total_visible as i64)
    ));
    out.push_str(&facts.join(" · "));
    out.push_str("\n\n");

    if page.rendered == 0 {
        out.push_str(&match last_seq {
            Some(last) => format!(
                "No messages at or after seq {from_seq} (the transcript ends at seq {last}).\n"
            ),
            None => "This transcript has no messages.\n".to_string(),
        });
        return Ok(out);
    }
    out.push_str(&page.text);
    out.push_str("—\n");
    let (first, last) = page.seq_range.unwrap_or((from_seq, from_seq));
    let mut summary = format!(
        "Showing seq {first}–{last}: {} message{}",
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
    if !transcript.sidechains.is_empty() {
        summary.push_str(&format!(
            "; {} subagent transcript{} not included",
            transcript.sidechains.len(),
            plural(transcript.sidechains.len() as i64)
        ));
    }
    out.push_str(&summary);
    out.push_str(".\n");
    match page.next_seq {
        Some(next) => out.push_str(&format!(
            "More follows — call {GET_SESSION} again with from_seq={next}.\n"
        )),
        None => out.push_str("End of transcript.\n"),
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
        assert_eq!(
            parse_key_arg("wake://session/claude-code:abc#12"),
            ("claude-code:abc".to_string(), Some(12))
        );
        assert_eq!(
            parse_key_arg("codex:devbox:0195-xyz"),
            ("codex:devbox:0195-xyz".to_string(), None)
        );
        assert_eq!(
            parse_key_arg(" claude-code:abc "),
            ("claude-code:abc".to_string(), None)
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
        assert_eq!(names, [SEARCH, LIST_SESSIONS, GET_SESSION, LIST_PROJECTS]);
        for d in &defs {
            assert_eq!(d["inputSchema"]["type"], "object");
            assert_eq!(d["annotations"]["readOnlyHint"], true);
        }
    }
}
