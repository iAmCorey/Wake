use super::parse_utils::*;
use super::pi_format::{PiRender, PiRenderOptions};
use super::{units_from_messages, AgentAdapter};
use crate::models::*;
use anyhow::Result;
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
            root: super::home_dir()
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
            root: super::home_dir()
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
    cwd: String,
    created_at: i64,
    /// 最后一行消息的时间(合并后的消息保留回合首行时间,updated 单独追踪)
    last_ts: i64,
    messages: Vec<TranscriptMessage>,
    model: Option<String>,
    tokens_used: Option<i64>,
    unknown_lines: u32,
}

fn parse_pi_jsonl(path: &Path, decode_images: bool) -> Result<PiParse> {
    let _image_budget = transcript_image_decode_budget(decode_images);
    let file = fs::File::open(path)?;
    let reader = BufReader::with_capacity(1 << 20, file);

    let mut p = PiParse {
        session_id: None,
        cwd: String::new(),
        created_at: 0,
        last_ts: 0,
        messages: Vec::new(),
        model: None,
        tokens_used: None,
        unknown_lines: 0,
    };
    // 消息渲染核心与 OpenClaw 共用(pi_format);Pi 是它的基础配置
    let mut render = PiRender::new(PiRenderOptions::default(), decode_images);

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
                if !render.push(msg, ts) {
                    p.unknown_lines += 1;
                }
            }
            // 已知的非内容行:模型/思考档位切换等,静默跳过
            Some("model_change") | Some("thinking_level_change") => {}
            _ => {
                p.unknown_lines += 1;
            }
        }
    }
    p.messages = render.messages;
    p.model = render.model;
    p.tokens_used = render.tokens_used;
    assign_seq(&mut p.messages);
    Ok(p)
}

fn build_meta(agent: AgentId, r: &SessionFileRef, p: &PiParse) -> SessionMeta {
    let native = p.session_id.clone().unwrap_or_else(|| r.native_id.clone());
    let title = title_from_messages(&p.messages).unwrap_or_else(|| UNTITLED.to_string());
    SessionMeta {
        key: format!("{}:{native}", agent.as_str()),
        host: String::new(),
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

    fn list_session_files(&self) -> Result<Vec<SessionFileRef>> {
        Ok(list_jsonl_refs(&self.root, self.agent, native_id_of))
    }

    fn file_ref(&self, path: &Path) -> Option<SessionFileRef> {
        let mut r = default_file_ref(self.agent, path)?;
        r.native_id = native_id_of(&r.native_id);
        Some(r)
    }

    fn parse_session(&self, r: &SessionFileRef) -> Result<ParsedSession> {
        let parsed = parse_pi_jsonl(Path::new(&r.file_path), false)?;
        let meta = build_meta(self.agent, r, &parsed);
        let units = units_from_messages(&parsed.messages);
        Ok(ParsedSession {
            meta,
            units,
            unknown_line_count: parsed.unknown_lines,
        })
    }

    fn parse_transcript(&self, r: &SessionFileRef) -> Result<ParsedTranscript> {
        let parsed = parse_pi_jsonl(Path::new(&r.file_path), true)?;
        Ok(ParsedTranscript {
            meta: build_meta(self.agent, r, &parsed),
            mainline: parsed.messages,
            sidechains: Vec::new(),
            unknown_line_count: parsed.unknown_lines,
        })
    }

    fn with_custom_root(&self, dir: PathBuf) -> Box<dyn AgentAdapter> {
        // 选中 `~/.pi`/`~/.omp` 家目录形态或 sessions 目录本身都认。
        // pi 与 omp 布局全同:实例保持自己的 agent,探测时两家都会命中同一目录,
        // 取舍交 UI(用户知道自己装的是哪个 CLI)
        let nested = dir.join("agent").join("sessions");
        let root = if nested.is_dir() { nested } else { dir };
        Box::new(Self {
            agent: self.agent,
            root,
        })
    }

    fn data_roots(&self) -> Vec<PathBuf> {
        vec![self.root.clone()]
    }
}
