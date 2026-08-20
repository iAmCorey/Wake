use super::parse_utils::*;
use super::{units_from_messages, AgentAdapter};
use crate::models::*;
use anyhow::Result;
use serde_json::Value;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

/// Kiro CLI:`~/.kiro/sessions/cli/<uuid>.{jsonl,json,history}` 三件套。
/// .json 边车含 cwd/title/时间;.jsonl 行 {kind:Prompt|AssistantMessage,
/// data:{content:[{kind:text,data}],meta:{timestamp:unix秒}}};.history 忽略。
pub struct KiroAdapter {
    root: PathBuf,
}

impl KiroAdapter {
    pub fn new() -> Self {
        Self {
            root: crate::home_dir()
                .join(".kiro")
                .join("sessions")
                .join("cli"),
        }
    }
}

struct Sidecar {
    cwd: String,
    title: String,
    created_ms: i64,
    updated_ms: i64,
    model: Option<String>,
}

fn read_sidecar(jsonl_path: &Path) -> Sidecar {
    let mut side = Sidecar {
        cwd: String::new(),
        title: String::new(),
        created_ms: 0,
        updated_ms: 0,
        model: None,
    };
    let json_path = jsonl_path.with_extension("json");
    if let Ok(raw) = fs::read_to_string(&json_path) {
        if let Ok(v) = serde_json::from_str::<Value>(&raw) {
            if let Some(c) = v.get("cwd").and_then(|x| x.as_str()) {
                side.cwd = c.to_string();
            }
            if let Some(t) = v.get("title").and_then(|x| x.as_str()) {
                side.title = t.to_string();
            }
            if let Some(t) = v.get("created_at").and_then(|x| x.as_str()) {
                side.created_ms = iso_ms(t);
            }
            if let Some(t) = v.get("updated_at").and_then(|x| x.as_str()) {
                side.updated_ms = iso_ms(t);
            }
            side.model = v
                .pointer("/session_state/rts_model_state/model_info/model_id")
                .or_else(|| v.pointer("/session_state/rts_model_state/model_info/model_name"))
                .and_then(|x| x.as_str())
                .filter(|m| !m.is_empty())
                .map(String::from);
        }
    }
    side
}

fn parse_kiro_jsonl(path: &Path) -> Result<(Vec<TranscriptMessage>, u32)> {
    let file = fs::File::open(path)?;
    let reader = BufReader::with_capacity(1 << 20, file);
    let mut messages: Vec<TranscriptMessage> = Vec::new();
    let mut unknown = 0u32;

    for line in reader.lines() {
        let Ok(line) = line else {
            unknown += 1;
            continue;
        };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(row) = serde_json::from_str::<Value>(&line) else {
            unknown += 1;
            continue;
        };
        let role = match row.get("kind").and_then(|v| v.as_str()) {
            Some("Prompt") => Role::User,
            Some("AssistantMessage") => Role::Assistant,
            _ => {
                unknown += 1;
                continue;
            }
        };
        let Some(data) = row.get("data") else {
            unknown += 1;
            continue;
        };
        let mut parts: Vec<String> = Vec::new();
        if let Some(Value::Array(blocks)) = data.get("content") {
            for b in blocks {
                if b.get("kind").and_then(|v| v.as_str()) == Some("text") {
                    if let Some(t) = b.get("data").and_then(|v| v.as_str()) {
                        if !t.trim().is_empty() {
                            parts.push(t.trim().to_string());
                        }
                    }
                }
            }
        }
        let text = parts.join("\n\n");
        if text.is_empty() {
            continue;
        }
        let ts_sec = data
            .get("meta")
            .and_then(|m| m.get("timestamp"))
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        messages.push(text_msg(role, &text, ts_sec * 1000));
    }
    assign_seq(&mut messages);
    Ok((messages, unknown))
}

fn build_meta(r: &SessionFileRef, side: &Sidecar, messages: &[TranscriptMessage]) -> SessionMeta {
    let title = Some(clean_title_candidate(&side.title))
        .filter(|t| !t.is_empty())
        .or_else(|| title_from_messages(messages))
        .unwrap_or_else(|| UNTITLED.to_string());
    let msg_ts_max = messages.iter().filter_map(|m| m.timestamp).max().unwrap_or(0);
    SessionMeta {
        key: format!("kiro:{}", r.native_id),
        id: r.native_id.clone(),
        agent: AgentId::Kiro,
        title,
        project_path: side.cwd.clone(),
        project_name: project_name_of(&side.cwd),
        file_path: r.file_path.clone(),
        created_at: if side.created_ms > 0 { side.created_ms } else { r.mtime_ms },
        updated_at: match side.updated_ms.max(msg_ts_max) {
            t if t > 0 => t,
            _ => r.mtime_ms,
        },
        message_count: messages.iter().filter(|m| m.kind == MessageKind::Text).count() as i64,
        size_bytes: r.size,
        git_branch: None,
        model: side.model.clone(),
        tokens_used: None,
        archived: false,
        source: None,
        favorite: false,
        pinned: false,
    }
}

impl AgentAdapter for KiroAdapter {
    fn agent(&self) -> AgentId {
        AgentId::Kiro
    }

    fn detect(&self) -> bool {
        self.root.is_dir()
    }

    fn list_session_files(&self) -> Result<Vec<SessionFileRef>> {
        Ok(list_jsonl_refs(&self.root, AgentId::Kiro, str::to_string))
    }

    fn parse_session(&self, r: &SessionFileRef) -> Result<ParsedSession> {
        let (messages, unknown) = parse_kiro_jsonl(Path::new(&r.file_path))?;
        let side = read_sidecar(Path::new(&r.file_path));
        let meta = build_meta(r, &side, &messages);
        let units = units_from_messages(&messages);
        Ok(ParsedSession {
            meta,
            units,
            unknown_line_count: unknown,
        })
    }

    fn parse_transcript(&self, r: &SessionFileRef) -> Result<ParsedTranscript> {
        let (messages, unknown) = parse_kiro_jsonl(Path::new(&r.file_path))?;
        let side = read_sidecar(Path::new(&r.file_path));
        Ok(ParsedTranscript {
            meta: build_meta(r, &side, &messages),
            mainline: messages,
            sidechains: Vec::new(),
            unknown_line_count: unknown,
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
