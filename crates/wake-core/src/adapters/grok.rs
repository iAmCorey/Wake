use super::parse_utils::*;
use super::{units_from_messages, AgentAdapter};
use crate::models::*;
use anyhow::Result;
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

/// Grok Build:`$GROK_HOME/sessions` 或 `~/.grok/sessions`。
/// 布局:`<urlencode(cwd)>/<session-id>/{summary.json,updates.jsonl}`。
/// `updates.jsonl` 是 ACP session/update 流(对话权威源);`summary.json` 给
/// 标题/cwd/模型/分支。凭证(`auth.json`)不读。
pub struct GrokAdapter {
    sessions_dir: PathBuf,
}

impl GrokAdapter {
    pub fn new() -> Self {
        Self {
            sessions_dir: grok_home().join("sessions"),
        }
    }
}

fn grok_home() -> PathBuf {
    std::env::var("GROK_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".grok"))
}

/// ACP 行里已知的非对话事件,静默跳过(不计 unknown)
const KNOWN_SKIP: &[&str] = &[
    "hook_execution",
    "task_backgrounded",
    "task_completed",
    "plan",
    "session_recap",
    "retry_state",
    "rewind_marker",
    "image_compressed",
    "auto_compact_started",
    "compaction_checkpoint",
];

#[derive(Default)]
struct Sidecar {
    cwd: String,
    title: String,
    git_branch: Option<String>,
    model: Option<String>,
    created_ms: i64,
    updated_ms: i64,
}

struct GrokParse {
    messages: Vec<TranscriptMessage>,
    title: String,
    cwd: String,
    git_branch: Option<String>,
    model: Option<String>,
    tokens_used: i64,
    created_at: i64,
    updated_at: i64,
    unknown_lines: u32,
}

impl GrokParse {
    /// 只有边车信息的空解析——`quick_meta` 的完整结果,也是 `parse_updates` 的起点。
    /// 两条路径共用它 + `build_meta`,快慢路径的 meta 不可能算出不同答案。
    fn from_sidecar(side: Sidecar) -> Self {
        Self {
            messages: Vec::new(),
            title: side.title,
            cwd: side.cwd,
            git_branch: side.git_branch,
            model: side.model,
            tokens_used: 0,
            created_at: side.created_ms,
            updated_at: side.updated_ms,
            unknown_lines: 0,
        }
    }
}

#[derive(Default)]
struct PendingAssistant {
    /// 流式增量直接追加(ACP 每个 delta 一行),不留 Vec 中间态
    text: String,
    thinking: String,
    tool_calls: Vec<ToolCallView>,
    timestamp: Option<i64>,
    model: Option<String>,
}

fn flush_assistant(
    pending: &mut Option<PendingAssistant>,
    messages: &mut Vec<TranscriptMessage>,
    tool_index: &mut HashMap<String, (usize, usize)>,
) {
    let Some(p) = pending.take() else { return };
    if p.text.is_empty() && p.thinking.is_empty() && p.tool_calls.is_empty() {
        return;
    }
    let (clipped, truncated) = clip(&p.text, MAX_MSG_TEXT);
    let thinking = if p.thinking.is_empty() {
        None
    } else {
        Some(clip(&p.thinking, MAX_TOOL_IO).0)
    };
    // tool_call_update 可能在气泡 flush 之后才到,登记 id → 消息内的真实位置
    let msg_idx = messages.len();
    for (ti, tc) in p.tool_calls.iter().enumerate() {
        if !tc.id.is_empty() {
            tool_index.insert(tc.id.clone(), (msg_idx, ti));
        }
    }
    messages.push(TranscriptMessage {
        seq: 0,
        role: Role::Assistant,
        kind: MessageKind::Text,
        text: clipped,
        truncated,
        tool_calls: p.tool_calls,
        thinking,
        timestamp: p.timestamp,
        model: p.model,
    });
}

fn ensure_pending<'a>(
    pending: &'a mut Option<PendingAssistant>,
    ts: i64,
    model: &Option<String>,
) -> &'a mut PendingAssistant {
    if pending.is_none() {
        *pending = Some(PendingAssistant {
            timestamp: ts_opt(ts),
            model: model.clone(),
            ..Default::default()
        });
    }
    pending.as_mut().unwrap()
}

fn parse_updates(path: &Path, side: Sidecar) -> Result<GrokParse> {
    let file = fs::File::open(path)?;
    let reader = BufReader::with_capacity(1 << 20, file);

    let mut out = GrokParse::from_sidecar(side);
    let mut tool_index: HashMap<String, (usize, usize)> = HashMap::new();
    let mut pending: Option<PendingAssistant> = None;

    for line in reader.lines() {
        let Ok(line) = line else {
            out.unknown_lines += 1;
            continue;
        };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(row) = serde_json::from_str::<Value>(&line) else {
            out.unknown_lines += 1;
            continue;
        };
        let method = row.get("method").and_then(|v| v.as_str()).unwrap_or("");
        if method != "session/update" && method != "_x.ai/session/update" {
            out.unknown_lines += 1;
            continue;
        }
        let Some(params) = row.get("params") else {
            out.unknown_lines += 1;
            continue;
        };
        let Some(update) = params.get("update") else {
            out.unknown_lines += 1;
            continue;
        };
        let su = update.get("sessionUpdate").and_then(|v| v.as_str()).unwrap_or("");
        let ts = event_ts(&row, params);
        // modelId 挂在每个 chunk 上,整场通常恒定——变了才付一次 alloc
        if let Some(m) = chunk_model(update) {
            if out.model.as_deref() != Some(m) {
                out.model = Some(m.to_string());
            }
        }

        // touch_ts 只在真正的对话事件上调用:KNOWN_SKIP 与未知行不得推进
        // updated_at(否则一条尾随 hook_execution 就会盖掉真实的最后活动时间)
        match su {
            "user_message_chunk" => {
                touch_ts(ts, &mut out.created_at, &mut out.updated_at);
                flush_assistant(&mut pending, &mut out.messages, &mut tool_index);
                let text = chunk_text(update);
                if text.trim().is_empty() {
                    continue;
                }
                out.messages.push(text_msg(Role::User, &text, ts));
            }
            "agent_thought_chunk" => {
                touch_ts(ts, &mut out.created_at, &mut out.updated_at);
                let text = chunk_text(update);
                if text.trim().is_empty() {
                    continue;
                }
                // 新的 thought 且当前气泡已有正文/工具 → 另开一条,贴近逐步输出
                if pending
                    .as_ref()
                    .is_some_and(|p| !p.text.is_empty() || !p.tool_calls.is_empty())
                {
                    flush_assistant(&mut pending, &mut out.messages, &mut tool_index);
                }
                ensure_pending(&mut pending, ts, &out.model).thinking.push_str(&text);
            }
            "agent_message_chunk" => {
                touch_ts(ts, &mut out.created_at, &mut out.updated_at);
                let text = chunk_text(update);
                if text.is_empty() {
                    continue;
                }
                let p = ensure_pending(&mut pending, ts, &out.model);
                p.text.push_str(&text);
                if p.model.is_none() {
                    p.model = out.model.clone();
                }
            }
            "tool_call" => {
                touch_ts(ts, &mut out.created_at, &mut out.updated_at);
                let id = update
                    .get("toolCallId")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let name = tool_name(update);
                let input = update.get("rawInput").cloned().unwrap_or(Value::Null);
                let p = ensure_pending(&mut pending, ts, &out.model);
                p.tool_calls.push(tool_call_view(id, &name, &input, None, false));
            }
            "tool_call_update" => {
                touch_ts(ts, &mut out.created_at, &mut out.updated_at);
                let id = update.get("toolCallId").and_then(|v| v.as_str()).unwrap_or("");
                if let Some(tc) = locate_tool(&mut pending, &mut out.messages, &tool_index, id) {
                    if tc.input.is_none() {
                        if let Some(raw) = update.get("rawInput").filter(|r| !r.is_null()) {
                            // 复用 tool_call_view 的 preview/pretty-print 规则,
                            // 免得同一个 ToolCallView 有两套渲染口径
                            let filled = tool_call_view(String::new(), &tc.name, raw, None, false);
                            tc.input_preview = filled.input_preview;
                            tc.input = filled.input;
                        }
                    }
                    if let Some(o) = update.get("rawOutput").and_then(|v| v.as_str()) {
                        tc.output = Some(clip(o, MAX_TOOL_IO).0);
                    } else if tc.output.is_none() {
                        let fallback = extract_text(update.get("content"));
                        if !fallback.is_empty() {
                            tc.output = Some(clip(&fallback, MAX_TOOL_IO).0);
                        }
                    }
                    if is_error_status(update.get("status")) {
                        tc.is_error = true;
                    }
                }
            }
            "turn_completed" => {
                touch_ts(ts, &mut out.created_at, &mut out.updated_at);
                if let Some(usage) = update.get("usage") {
                    let total = usage.get("totalTokens").and_then(|v| v.as_i64()).unwrap_or(0);
                    if total > out.tokens_used {
                        out.tokens_used = total;
                    }
                }
                flush_assistant(&mut pending, &mut out.messages, &mut tool_index);
            }
            "auto_compact_completed" => {
                touch_ts(ts, &mut out.created_at, &mut out.updated_at);
                flush_assistant(&mut pending, &mut out.messages, &mut tool_index);
                out.messages
                    .push(mk_msg(Role::System, MessageKind::CompactSummary, COMPACT_DIVIDER, ts));
            }
            _ if KNOWN_SKIP.contains(&su) => {}
            _ => out.unknown_lines += 1,
        }
    }
    flush_assistant(&mut pending, &mut out.messages, &mut tool_index);
    assign_seq(&mut out.messages);

    Ok(out)
}

/// 回填 tool_call_update:未 flush 的气泡按 id 线性找,flush 后走 tool_index
fn locate_tool<'a>(
    pending: &'a mut Option<PendingAssistant>,
    messages: &'a mut [TranscriptMessage],
    tool_index: &HashMap<String, (usize, usize)>,
    id: &str,
) -> Option<&'a mut ToolCallView> {
    if let Some(p) = pending.as_mut() {
        if let Some(tc) = p.tool_calls.iter_mut().find(|t| t.id == id) {
            return Some(tc);
        }
    }
    let &(mi, ti) = tool_index.get(id)?;
    messages.get_mut(mi)?.tool_calls.get_mut(ti)
}

fn touch_ts(ts: i64, created_at: &mut i64, updated_at: &mut i64) {
    if ts <= 0 {
        return;
    }
    if *created_at == 0 {
        *created_at = ts;
    }
    if ts > *updated_at {
        *updated_at = ts;
    }
}

fn event_ts(row: &Value, params: &Value) -> i64 {
    let meta_ms = params.pointer("/_meta/agentTimestampMs").map_or(0, to_epoch_ms);
    if meta_ms > 0 {
        return meta_ms;
    }
    row.get("timestamp").map_or(0, to_epoch_ms)
}

fn chunk_text(update: &Value) -> String {
    extract_text(update.get("content"))
}

fn chunk_model(update: &Value) -> Option<&str> {
    update
        .get("_meta")
        .and_then(|m| m.get("modelId"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
}

fn extract_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Object(o)) => match o.get("type").and_then(|v| v.as_str()) {
            Some("text") => o.get("text").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            Some("image") => "[image]".to_string(),
            Some("content") => extract_text(o.get("content")),
            _ => o.get("text").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        },
        Some(Value::Array(arr)) => arr
            .iter()
            .map(|c| extract_text(Some(c)))
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn tool_name(update: &Value) -> String {
    update
        .get("_meta")
        .and_then(|m| m.get("x.ai/tool"))
        .and_then(|t| t.get("name"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .or_else(|| update.get("title").and_then(|v| v.as_str()).filter(|s| !s.is_empty()))
        .unwrap_or("tool")
        .to_string()
}

fn is_error_status(status: Option<&Value>) -> bool {
    match status {
        Some(Value::String(s)) => s == "failed" || s == "error",
        Some(Value::Object(o)) => matches!(
            o.get("status").and_then(|v| v.as_str()),
            Some("failed" | "error")
        ),
        _ => false,
    }
}

fn read_sidecar(updates_path: &Path) -> Sidecar {
    let mut side = Sidecar::default();
    let raw = updates_path
        .parent()
        .and_then(|d| fs::read_to_string(d.join("summary.json")).ok());
    if let Some(v) = raw.and_then(|r| serde_json::from_str::<Value>(&r).ok()) {
        let s = |k: &str| v.get(k).and_then(|x| x.as_str()).filter(|t| !t.is_empty());
        side.cwd = v
            .pointer("/info/cwd")
            .and_then(|x| x.as_str())
            .unwrap_or_default()
            .to_string();
        // 手动 /rename 落在 generated_title;为空才退回自动摘要
        side.title = s("generated_title")
            .filter(|t| !t.trim().is_empty())
            .or_else(|| s("session_summary"))
            .unwrap_or_default()
            .trim()
            .to_string();
        side.git_branch = s("head_branch").map(String::from);
        side.model = s("current_model_id").map(String::from);
        side.created_ms = s("created_at").map_or(0, iso_ms);
        side.updated_ms = s("updated_at").or_else(|| s("last_active_at")).map_or(0, iso_ms);
    }
    if side.cwd.is_empty() {
        side.cwd = cwd_from_session_dir(updates_path);
    }
    side
}

/// `sessions/<encoded-cwd>/<id>/updates.jsonl` → 解码 cwd;超长目录走 `.cwd`
fn cwd_from_session_dir(updates_path: &Path) -> String {
    let Some(session_dir) = updates_path.parent() else {
        return String::new();
    };
    let Some(group) = session_dir.parent() else {
        return String::new();
    };
    if let Ok(raw) = fs::read_to_string(group.join(".cwd")) {
        let t = raw.trim();
        if !t.is_empty() {
            return t.to_string();
        }
    }
    percent_decode(&group.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default())
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (from_hex(bytes[i + 1]), from_hex(bytes[i + 2])) {
                out.push((h << 4) | l);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn from_hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn build_meta(r: &SessionFileRef, p: &GrokParse) -> SessionMeta {
    let title = Some(clean_title_candidate(&p.title))
        .filter(|t| !t.is_empty())
        .or_else(|| title_from_messages(&p.messages))
        .unwrap_or_else(|| UNTITLED.to_string());
    SessionMeta {
        key: format!("grok-build:{}", r.native_id),
        id: r.native_id.clone(),
        agent: AgentId::GrokBuild,
        title,
        project_path: p.cwd.clone(),
        project_name: project_name_of(&p.cwd),
        file_path: r.file_path.clone(),
        created_at: if p.created_at > 0 { p.created_at } else { r.mtime_ms },
        updated_at: if p.updated_at > 0 { p.updated_at } else { r.mtime_ms },
        message_count: p
            .messages
            .iter()
            .filter(|m| m.kind == MessageKind::Text)
            .count() as i64,
        size_bytes: r.size,
        git_branch: p.git_branch.clone(),
        model: p.model.clone(),
        tokens_used: if p.tokens_used > 0 { Some(p.tokens_used) } else { None },
        archived: false,
        source: None,
        favorite: false,
        pinned: false,
    }
}

fn is_hidden_session_path(path: &Path) -> bool {
    let p = path.to_string_lossy();
    p.contains("/terminal/")
        || p.contains("/compaction_checkpoints/")
        || p.contains("/subagents/")
}

/// 会话目录 → SessionFileRef(主文件 updates.jsonl;mtime 取 updates/summary 较新者,
/// 只改标题的 summary.json 重写也要能触发重扫)
fn session_ref(dir: &Path) -> Option<SessionFileRef> {
    if is_hidden_session_path(dir) {
        return None;
    }
    let updates = dir.join("updates.jsonl");
    let meta = fs::metadata(&updates).ok()?;
    if !meta.is_file() || meta.len() == 0 {
        return None;
    }
    let mut mtime = mtime_ms(&meta);
    if let Ok(sm) = fs::metadata(dir.join("summary.json")) {
        mtime = mtime.max(mtime_ms(&sm));
    }
    Some(SessionFileRef {
        agent: AgentId::GrokBuild,
        native_id: dir.file_name()?.to_string_lossy().to_string(),
        file_path: updates.to_string_lossy().to_string(),
        mtime_ms: mtime,
        size: meta.len() as i64,
    })
}

fn parse_grok(r: &SessionFileRef) -> Result<GrokParse> {
    let path = Path::new(&r.file_path);
    parse_updates(path, read_sidecar(path))
}

impl AgentAdapter for GrokAdapter {
    fn agent(&self) -> AgentId {
        AgentId::GrokBuild
    }

    fn detect(&self) -> bool {
        self.sessions_dir.is_dir()
    }

    fn list_session_files(&self) -> Result<Vec<SessionFileRef>> {
        let mut refs = Vec::new();
        let Ok(groups) = fs::read_dir(&self.sessions_dir) else {
            return Ok(refs);
        };
        for group in groups.flatten() {
            if !group.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let Ok(sessions) = fs::read_dir(group.path()) else {
                continue;
            };
            for session in sessions.flatten() {
                if !session.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    continue;
                }
                if let Some(r) = session_ref(&session.path()) {
                    refs.push(r);
                }
            }
        }
        Ok(refs)
    }

    fn file_ref(&self, path: &Path) -> Option<SessionFileRef> {
        if is_hidden_session_path(path) {
            return None;
        }
        let name = path.file_name()?.to_string_lossy();
        if name != "updates.jsonl" && name != "summary.json" {
            return None;
        }
        session_ref(path.parent()?)
    }

    fn session_paths(&self, meta: &SessionMeta) -> Vec<String> {
        // 会话是 <id>/ 整个目录(updates + summary),删除时整目录进废纸篓
        Path::new(&meta.file_path)
            .parent()
            .map(|d| vec![d.to_string_lossy().to_string()])
            .unwrap_or_else(|| vec![meta.file_path.clone()])
    }

    fn quick_meta(&self, refs: &[SessionFileRef]) -> Option<HashMap<String, SessionMeta>> {
        Some(
            refs.iter()
                .map(|r| {
                    let side = read_sidecar(Path::new(&r.file_path));
                    (r.file_path.clone(), build_meta(r, &GrokParse::from_sidecar(side)))
                })
                .collect(),
        )
    }

    fn parse_session(&self, r: &SessionFileRef) -> Result<ParsedSession> {
        let p = parse_grok(r)?;
        Ok(ParsedSession {
            meta: build_meta(r, &p),
            units: units_from_messages(&p.messages),
            unknown_line_count: p.unknown_lines,
        })
    }

    fn parse_transcript(&self, r: &SessionFileRef) -> Result<ParsedTranscript> {
        let p = parse_grok(r)?;
        Ok(ParsedTranscript {
            meta: build_meta(r, &p),
            mainline: p.messages,
            sidechains: Vec::new(),
            unknown_line_count: p.unknown_lines,
        })
    }

    fn watch_paths(&self) -> Vec<PathBuf> {
        if self.detect() {
            vec![self.sessions_dir.clone()]
        } else {
            Vec::new()
        }
    }
}
