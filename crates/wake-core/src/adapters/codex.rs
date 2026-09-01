use super::parse_utils::*;
use super::sqlite_ro::open_sqlite_ro;
use super::{units_from_messages, AgentAdapter};
use crate::models::*;
use anyhow::Result;
use rusqlite::Connection;
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
        }
    }
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

fn parse_rollout(path: &Path) -> Result<CodexParse> {
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
                                if let Some(results) = review_results_from_user_action(&text) {
                                    messages.push(mk_msg(
                                        Role::Assistant,
                                        MessageKind::Text,
                                        &results,
                                        ts,
                                    ));
                                    pending_review_message = Some(messages.len() - 1);
                                } else {
                                    messages.push(mk_msg(Role::User, user_kind(&text), &text, ts));
                                }
                            }
                            "assistant" => {
                                let text = format_review_json(&text).unwrap_or(text);
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
                        let call_id = payload
                            .get("call_id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        if let Some(&(mi, ti)) = tool_index.get(call_id) {
                            let out_text = match payload.get("output") {
                                Some(Value::String(s)) => s.clone(),
                                Some(o @ Value::Object(_)) => o
                                    .get("content")
                                    .and_then(|v| v.as_str())
                                    .map(String::from)
                                    .unwrap_or_else(|| {
                                        serde_json::to_string(o).unwrap_or_default()
                                    }),
                                _ => String::new(),
                            };
                            messages[mi].tool_calls[ti].output =
                                Some(clip(&out_text, MAX_TOOL_IO).0);
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
                                if let Some(results) = review_results_from_user_action(m) {
                                    event_fallback.push(mk_msg(
                                        Role::Assistant,
                                        MessageKind::Text,
                                        &results,
                                        ts,
                                    ));
                                } else {
                                    event_fallback.push(mk_msg(
                                        Role::User,
                                        user_kind(m),
                                        m.trim(),
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
            "world_state" => {}
            _ => unknown_lines += 1,
        }
    }

    // response_item 完全缺席的会话退回 event_msg 流
    let has_real = messages
        .iter()
        .any(|m| m.kind == MessageKind::Text && !m.text.is_empty());
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

impl AgentAdapter for CodexAdapter {
    fn agent(&self) -> AgentId {
        AgentId::Codex
    }

    fn list_session_files(&self) -> Result<Vec<SessionFileRef>> {
        let mut refs = if self.scan_sessions {
            list_jsonl_refs(&self.sessions_dir, AgentId::Codex, rollout_native_id)
        } else {
            Vec::new()
        };
        if self.scan_archived {
            refs.extend(list_jsonl_refs(
                &self.archived_dir,
                AgentId::Codex,
                rollout_native_id,
            ));
        }
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

    fn with_custom_root(&self, dir: PathBuf) -> Box<dyn AgentAdapter> {
        // dir 视作 CODEX_HOME 形态;归一化未上提的孤立数据目录按**目录名**
        // 保留角色:空的独立 sessions 目录日后落盘的 rollout 要能被发现,
        // 独立 archived 拷贝的会话必须保住 archived 标记(build_meta 按
        // archived_dir 前缀判);无名可依的裸 rollout 拷贝当活跃 sessions。
        // 侧档一并相对 dir 派生,绝不越界摸父目录——"父目录有 home 证据"的
        // 场景由 normalize_custom_root 在入库前上提(2026-08-24 Codex review)
        let name = dir.file_name().and_then(|n| n.to_str());
        let (sessions_dir, archived_dir) = match name {
            Some("archived_sessions") => (dir.join("sessions"), dir.clone()),
            Some("sessions") => (dir.clone(), dir.join("archived_sessions")),
            _ if is_rollout_store(&dir) => (dir.clone(), dir.join("archived_sessions")),
            _ => (dir.join("sessions"), dir.join("archived_sessions")),
        };
        Box::new(Self {
            sessions_dir,
            archived_dir,
            state_db: dir.join("state_5.sqlite"),
            scan_sessions: true,
            scan_archived: true,
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
