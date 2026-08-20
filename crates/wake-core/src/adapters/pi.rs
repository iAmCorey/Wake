use super::parse_utils::*;
use super::{units_from_messages, AgentAdapter};
use crate::models::*;
use anyhow::Result;
use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

/// Pi / Oh My Pi(omp 是 pi 的 fork,会话格式同构,只有数据根不同):
/// `~/.pi/agent/sessions/<有损编码目录>/<timestamp>_<uuid>.jsonl`。
/// 首行 {type:session,version,id,timestamp,cwd}——cwd 直接在首行,无需反推
/// 目录名;后续 {type:message,message:{role:user|assistant|toolResult,
/// content:[{type:text|toolCall,…}]}},toolResult 是独立 role,按 toolCallId
/// 回填;model_change/thinking_level_change 等已知行静默跳过。
pub struct PiAdapter {
    agent: AgentId,
    root: PathBuf,
}

impl PiAdapter {
    pub fn new() -> Self {
        Self {
            agent: AgentId::Pi,
            root: dirs::home_dir()
                .unwrap_or_default()
                .join(".pi")
                .join("agent")
                .join("sessions"),
        }
    }

    /// Oh My Pi 变体:`~/.omp/agent/sessions`,解析核心完全共用
    pub fn omp() -> Self {
        Self {
            agent: AgentId::Omp,
            root: dirs::home_dir()
                .unwrap_or_default()
                .join(".omp")
                .join("agent")
                .join("sessions"),
        }
    }
}

/// 文件名 stem `<timestamp>_<uuid>` → uuid;无 `_` 时整个 stem 兜底
fn native_id_of(stem: &str) -> String {
    stem.rsplit('_').next().unwrap_or(stem).to_string()
}

struct PiParse {
    session_id: Option<String>,
    title: String,
    cwd: String,
    created_at: i64,
    /// 最后一行消息的时间(合并后的消息保留回合首行时间,updated 单独追踪)
    last_ts: i64,
    messages: Vec<TranscriptMessage>,
    model: Option<String>,
    tokens_used: Option<i64>,
    unknown_lines: u32,
}

fn parse_pi_jsonl(path: &Path) -> Result<PiParse> {
    let file = fs::File::open(path)?;
    let reader = BufReader::with_capacity(1 << 20, file);

    let mut p = PiParse {
        session_id: None,
        title: String::new(),
        cwd: String::new(),
        created_at: 0,
        last_ts: 0,
        messages: Vec::new(),
        model: None,
        tokens_used: None,
        unknown_lines: 0,
    };
    // toolCallId → (消息下标, tool_calls 下标),toolResult 行回填用
    let mut tool_index: HashMap<String, (usize, usize)> = HashMap::new();

    for line in reader.lines() {
        let Ok(line) = line else {
            p.unknown_lines += 1;
            continue;
        };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(row) = serde_json::from_str::<serde_json::Value>(&line) else {
            p.unknown_lines += 1;
            continue;
        };
        match row.get("type").and_then(|v| v.as_str()) {
            Some("session") => {
                if let Some(id) = row.get("id").and_then(|v| v.as_str()) {
                    p.session_id = Some(id.to_string());
                }
                if let Some(c) = row.get("cwd").and_then(|v| v.as_str()) {
                    p.cwd = c.to_string();
                }
                if let Some(t) = row
                    .get("title")
                    .or_else(|| row.get("name"))
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    p.title = t.to_string();
                }
                if let Some(t) = row.get("timestamp").and_then(|v| v.as_str()) {
                    p.created_at = iso_ms(t);
                }
            }
            Some("message") => {
                let Some(msg) = row.get("message") else {
                    p.unknown_lines += 1;
                    continue;
                };
                let ts = row
                    .get("timestamp")
                    .and_then(|v| v.as_str())
                    .map(iso_ms)
                    .unwrap_or(0);
                p.last_ts = p.last_ts.max(ts);
                let content = msg.get("content").unwrap_or(&serde_json::Value::Null);
                match msg.get("role").and_then(|v| v.as_str()) {
                    Some("user") => {
                        let text = blocks_text(content);
                        if !text.is_empty() {
                            p.messages.push(text_msg(Role::User, &text, ts));
                        }
                    }
                    Some("assistant") => {
                        let text = blocks_text(content);
                        let mut tools: Vec<ToolCallView> = Vec::new();
                        for b in content.as_array().into_iter().flatten() {
                            if b.get("type").and_then(|v| v.as_str()) == Some("toolCall") {
                                let id = b.get("id").and_then(|v| v.as_str()).unwrap_or_default();
                                let name =
                                    b.get("name").and_then(|v| v.as_str()).unwrap_or_default();
                                let input = b
                                    .get("arguments")
                                    .cloned()
                                    .unwrap_or(serde_json::Value::Null);
                                tools.push(tool_call_view(
                                    id.to_string(),
                                    name,
                                    &input,
                                    None,
                                    false,
                                ));
                            }
                        }
                        if text.is_empty() && tools.is_empty() {
                            continue;
                        }
                        let model = msg.get("model").and_then(|v| v.as_str()).map(String::from);
                        if model.is_some() {
                            p.model = model.clone();
                        }
                        if let Some(t) = msg
                            .get("usage")
                            .and_then(|u| u.get("totalTokens"))
                            .and_then(|v| v.as_i64())
                        {
                            p.tokens_used = Some(t);
                        }
                        // 连续 assistant 行(中间只隔 toolResult)合并成一条,
                        // 详情页每个回合一条助手消息
                        if !matches!(p.messages.last(), Some(m) if m.role == Role::Assistant) {
                            p.messages.push(text_msg(Role::Assistant, "", ts));
                        }
                        let base = p.messages.len() - 1;
                        let last = &mut p.messages[base];
                        // 合并后统一压 MAX_MSG_TEXT 上限(整个 agentic 回合并成
                        // 一条消息,不能靠单行的 text_msg clip)
                        if !text.is_empty() && last.text.len() < MAX_MSG_TEXT {
                            if !last.text.is_empty() {
                                last.text.push_str("\n\n");
                            }
                            last.text.push_str(&text);
                            if last.text.len() > MAX_MSG_TEXT {
                                let (t, _) = clip(&last.text, MAX_MSG_TEXT);
                                last.text = t;
                                last.truncated = true;
                            }
                        }
                        if model.is_some() {
                            last.model = model;
                        }
                        for tc in tools {
                            tool_index.insert(tc.id.clone(), (base, last.tool_calls.len()));
                            last.tool_calls.push(tc);
                        }
                    }
                    Some("toolResult") => {
                        let Some(call_id) = msg.get("toolCallId").and_then(|v| v.as_str()) else {
                            continue;
                        };
                        if let Some(&(mi, ti)) = tool_index.get(call_id) {
                            let tc = &mut p.messages[mi].tool_calls[ti];
                            let text = blocks_text(content);
                            if !text.is_empty() {
                                tc.output = Some(clip(&text, MAX_TOOL_IO).0);
                            }
                            if msg.get("isError").and_then(|v| v.as_bool()) == Some(true) {
                                tc.is_error = true;
                            }
                        }
                    }
                    _ => {
                        p.unknown_lines += 1;
                    }
                }
            }
            // 已知的非内容行:模型/思考档位切换等,静默跳过
            Some("model_change") | Some("thinking_level_change") => {}
            _ => {
                p.unknown_lines += 1;
            }
        }
    }
    assign_seq(&mut p.messages);
    Ok(p)
}

fn build_meta(agent: AgentId, r: &SessionFileRef, p: &PiParse) -> SessionMeta {
    let native = p.session_id.clone().unwrap_or_else(|| r.native_id.clone());
    let title = Some(clean_title_candidate(&p.title))
        .filter(|t| !t.is_empty())
        .or_else(|| title_from_messages(&p.messages))
        .unwrap_or_else(|| UNTITLED.to_string());
    SessionMeta {
        key: format!("{}:{native}", agent.as_str()),
        id: native,
        agent,
        title,
        project_path: p.cwd.clone(),
        project_name: project_name_of(&p.cwd),
        file_path: r.file_path.clone(),
        created_at: if p.created_at > 0 {
            p.created_at
        } else {
            r.mtime_ms
        },
        updated_at: if p.last_ts > 0 { p.last_ts } else { r.mtime_ms },
        message_count: p
            .messages
            .iter()
            .filter(|m| m.kind == MessageKind::Text)
            .count() as i64,
        size_bytes: r.size,
        git_branch: None,
        model: p.model.clone(),
        tokens_used: p.tokens_used,
        archived: false,
        source: None,
        favorite: false,
        pinned: false,
    }
}

impl AgentAdapter for PiAdapter {
    fn agent(&self) -> AgentId {
        self.agent
    }

    fn detect(&self) -> bool {
        self.root.is_dir()
    }

    fn list_session_files(&self) -> Result<Vec<SessionFileRef>> {
        Ok(list_jsonl_refs(&self.root, self.agent, native_id_of))
    }

    fn file_ref(&self, path: &Path) -> Option<SessionFileRef> {
        let mut r = default_file_ref(self.agent, path)?;
        r.native_id = native_id_of(&r.native_id);
        Some(r)
    }

    fn parse_session(&self, r: &SessionFileRef) -> Result<ParsedSession> {
        let parsed = parse_pi_jsonl(Path::new(&r.file_path))?;
        let meta = build_meta(self.agent, r, &parsed);
        let units = units_from_messages(&parsed.messages);
        Ok(ParsedSession {
            meta,
            units,
            unknown_line_count: parsed.unknown_lines,
        })
    }

    fn parse_transcript(&self, r: &SessionFileRef) -> Result<ParsedTranscript> {
        let parsed = parse_pi_jsonl(Path::new(&r.file_path))?;
        Ok(ParsedTranscript {
            meta: build_meta(self.agent, r, &parsed),
            mainline: parsed.messages,
            sidechains: Vec::new(),
            unknown_line_count: parsed.unknown_lines,
        })
    }

    fn watch_paths(&self) -> Vec<PathBuf> {
        if self.detect() {
            vec![self.root.clone()]
        } else {
            Vec::new()
        }
    }
}
