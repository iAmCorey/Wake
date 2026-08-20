use super::parse_utils::*;
use super::sqlite_ro::open_sqlite_ro;
use super::{units_from_messages, AgentAdapter};
use crate::models::*;
use anyhow::Result;
use rusqlite::Connection;
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

pub struct CodexAdapter {
    sessions_dir: PathBuf,
    archived_dir: PathBuf,
    state_db: PathBuf,
}

impl CodexAdapter {
    pub fn new() -> Self {
        let root = crate::home_dir().join(".codex");
        Self {
            sessions_dir: root.join("sessions"),
            archived_dir: root.join("archived_sessions"),
            state_db: root.join("state_5.sqlite"),
        }
    }
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

fn parse_rollout(path: &Path) -> Result<CodexParse> {
    let file = fs::File::open(path)?;
    let reader = BufReader::with_capacity(1 << 20, file);

    let mut messages: Vec<TranscriptMessage> = Vec::new();
    let mut event_fallback: Vec<TranscriptMessage> = Vec::new();
    let mut tool_index: HashMap<String, (usize, usize)> = HashMap::new();
    let mut cwd = String::new();
    let mut git_branch: Option<String> = None;
    let mut model: Option<String> = None;
    let mut source: Option<String> = None;
    let mut tokens_used: i64 = 0;
    let mut created_at: i64 = 0;
    let mut updated_at: i64 = 0;
    let mut unknown_lines: u32 = 0;
    let mut saw_session_meta = false;

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
                        let mut parts: Vec<String> = Vec::new();
                        match payload.get("content") {
                            Some(Value::Array(blocks)) => {
                                for b in blocks {
                                    if matches!(
                                        b.get("type").and_then(|v| v.as_str()),
                                        Some("input_text") | Some("output_text") | Some("text")
                                    ) {
                                        if let Some(t) = b.get("text").and_then(|v| v.as_str()) {
                                            parts.push(t.to_string());
                                        }
                                    }
                                }
                            }
                            Some(Value::String(s)) => parts.push(s.clone()),
                            _ => {}
                        }
                        let text = parts.join("\n\n").trim().to_string();
                        if text.is_empty() {
                            continue;
                        }
                        match role {
                            "user" => {
                                messages.push(mk_msg(Role::User, user_kind(&text), &text, ts))
                            }
                            "assistant" => {
                                messages.push(mk_msg(Role::Assistant, MessageKind::Text, &text, ts))
                            }
                            _ => messages.push(mk_msg(Role::System, MessageKind::Meta, &text, ts)),
                        }
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
                            match messages.last_mut() {
                                Some(last)
                                    if last.role == Role::Assistant
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
                        let need_host = !matches!(
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
                        let call_id = payload.get("call_id").and_then(|v| v.as_str()).unwrap_or("");
                        if let Some(&(mi, ti)) = tool_index.get(call_id) {
                            let out_text = match payload.get("output") {
                                Some(Value::String(s)) => s.clone(),
                                Some(o @ Value::Object(_)) => o
                                    .get("content")
                                    .and_then(|v| v.as_str())
                                    .map(String::from)
                                    .unwrap_or_else(|| serde_json::to_string(o).unwrap_or_default()),
                                _ => String::new(),
                            };
                            messages[mi].tool_calls[ti].output = Some(clip(&out_text, MAX_TOOL_IO).0);
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
                            if !m.trim().is_empty() {
                                event_fallback.push(mk_msg(Role::User, user_kind(m), m.trim(), ts));
                            }
                        }
                    }
                    "agent_message" => {
                        if let Some(m) = payload.get("message").and_then(|v| v.as_str()) {
                            if !m.trim().is_empty() {
                                event_fallback
                                    .push(mk_msg(Role::Assistant, MessageKind::Text, m.trim(), ts));
                            }
                        }
                    }
                    _ => {}
                }
            }
            "compacted" => {
                messages.push(mk_msg(Role::System, MessageKind::CompactSummary, "── Context compacted ──", ts));
            }
            "world_state" => {}
            _ => unknown_lines += 1,
        }
    }

    // response_item 完全缺席的会话退回 event_msg 流
    let has_real = messages.iter().any(|m| m.kind == MessageKind::Text && !m.text.is_empty());
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
    })
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
    let title = title_from_messages(&p.messages).unwrap_or_else(|| UNTITLED.to_string());
    let project_name = project_name_of(&p.cwd);
    SessionMeta {
        key: format!("codex:{}", r.native_id),
        id: r.native_id.clone(),
        agent: AgentId::Codex,
        title,
        project_path: p.cwd.clone(),
        project_name,
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
        archived: r.file_path.starts_with(&archived_dir.to_string_lossy().to_string()),
        source: p.source.clone(),
        favorite: false,
        pinned: false,
    }
}

impl AgentAdapter for CodexAdapter {
    fn agent(&self) -> AgentId {
        AgentId::Codex
    }

    fn detect(&self) -> bool {
        self.sessions_dir.is_dir() || self.archived_dir.is_dir()
    }

    fn list_session_files(&self) -> Result<Vec<SessionFileRef>> {
        let mut refs = list_jsonl_refs(&self.sessions_dir, AgentId::Codex, rollout_native_id);
        refs.extend(list_jsonl_refs(&self.archived_dir, AgentId::Codex, rollout_native_id));
        Ok(refs)
    }

    fn quick_meta(&self, refs: &[SessionFileRef]) -> Option<HashMap<String, SessionMeta>> {
        let rows = read_threads(&self.state_db)?;
        let by_path: HashMap<&str, &ThreadRow> =
            rows.iter().map(|r| (r.rollout_path.as_str(), r)).collect();
        let mut out = HashMap::new();
        for r in refs {
            let Some(row) = by_path.get(r.file_path.as_str()) else {
                continue;
            };
            let title = row
                .name
                .as_deref()
                .filter(|n| !n.trim().is_empty())
                .map(String::from)
                .or_else(|| {
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
        r.native_id = rollout_native_id(&r.native_id);
        Some(r)
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
        let parsed = parse_rollout(Path::new(&r.file_path))?;
        let meta = build_meta(r, &parsed, &self.archived_dir);
        let units = units_from_messages(&parsed.messages);
        Ok(ParsedSession {
            meta,
            units,
            unknown_line_count: parsed.unknown_lines,
        })
    }

    fn parse_transcript(&self, r: &SessionFileRef) -> Result<ParsedTranscript> {
        let parsed = parse_rollout(Path::new(&r.file_path))?;
        Ok(ParsedTranscript {
            meta: build_meta(r, &parsed, &self.archived_dir),
            mainline: parsed.messages,
            sidechains: Vec::new(),
            unknown_line_count: parsed.unknown_lines,
        })
    }

    fn watch_paths(&self) -> Vec<PathBuf> {
        let mut v = Vec::new();
        if self.sessions_dir.is_dir() {
            v.push(self.sessions_dir.clone());
        }
        if self.archived_dir.is_dir() {
            v.push(self.archived_dir.clone());
        }
        v
    }
}
