use super::parse_utils::*;
use super::{units_from_messages, AgentAdapter};
use crate::models::*;
use anyhow::Result;
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

/// Gemini CLI:`~/.gemini/tmp/<slug>/chats/session-*.jsonl`。
/// 首行 header({sessionId,startTime,lastUpdated}),后续 {$set:{messages:[…]}}
/// **覆盖式**快照——重放到最后一条 $set 为准,不能按行追加。
/// cwd 靠 `~/.gemini/projects.json` 的 路径→slug 映射反查;无原生标题。
pub struct GeminiAdapter {
    root: PathBuf,
    projects_json: PathBuf,
    /// slug → 项目路径映射,按 projects.json mtime 缓存(全量刷新时逐会话调用)
    slug_cache: std::sync::Mutex<Option<(i64, HashMap<String, String>)>>,
}

impl GeminiAdapter {
    pub fn new() -> Self {
        let home = super::home_dir().unwrap_or_default().join(".gemini");
        Self {
            root: home.join("tmp"),
            projects_json: home.join("projects.json"),
            slug_cache: std::sync::Mutex::new(None),
        }
    }

    /// slug → 项目真实路径
    fn slug_map(&self) -> HashMap<String, String> {
        let mtime = fs::metadata(&self.projects_json)
            .map(|m| mtime_ms(&m))
            .unwrap_or(0);
        {
            let cache = self.slug_cache.lock().unwrap();
            if let Some((t, map)) = cache.as_ref() {
                if *t == mtime {
                    return map.clone();
                }
            }
        }
        let mut out = HashMap::new();
        if let Ok(raw) = fs::read_to_string(&self.projects_json) {
            if let Ok(v) = serde_json::from_str::<Value>(&raw) {
                if let Some(Value::Object(map)) = v.get("projects") {
                    for (path, slug) in map {
                        if let Some(s) = slug.as_str() {
                            out.insert(s.to_string(), path.clone());
                        }
                    }
                }
            }
        }
        *self.slug_cache.lock().unwrap() = Some((mtime, out.clone()));
        out
    }
}

struct GeminiParse {
    session_id: Option<String>,
    messages: Vec<TranscriptMessage>,
    created_at: i64,
    updated_at: i64,
    unknown_lines: u32,
}

fn parse_gemini_jsonl(path: &Path, decode_images: bool) -> Result<GeminiParse> {
    let _image_budget = transcript_image_decode_budget(decode_images);
    let file = fs::File::open(path)?;
    let reader = BufReader::with_capacity(1 << 20, file);

    let mut session_id: Option<String> = None;
    let mut created_at = 0i64;
    let mut updated_at = 0i64;
    let mut unknown = 0u32;
    let mut last_set: Option<Value> = None;

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
        if let Some(set) = row.get("$set") {
            if set.get("messages").and_then(|m| m.as_array()).is_some() {
                last_set = Some(set.clone());
            }
            continue;
        }
        if let Some(id) = row.get("sessionId").and_then(|v| v.as_str()) {
            session_id = Some(id.to_string());
            if let Some(t) = row.get("startTime").and_then(|v| v.as_str()) {
                created_at = iso_ms(t);
            }
            if let Some(t) = row.get("lastUpdated").and_then(|v| v.as_str()) {
                updated_at = iso_ms(t);
            }
            continue;
        }
        unknown += 1;
    }

    let mut messages: Vec<TranscriptMessage> = Vec::new();
    if let Some(set) = last_set {
        if let Some(arr) = set.get("messages").and_then(|m| m.as_array()) {
            for m in arr {
                let role = match m.get("type").and_then(|v| v.as_str()) {
                    Some("user") => Role::User,
                    Some(_) => Role::Assistant,
                    None => {
                        unknown += 1;
                        continue;
                    }
                };
                let parsed = content_parts(m.get("content").unwrap_or(&Value::Null), decode_images);
                if parsed.text.is_empty() && parsed.images.is_empty() {
                    continue;
                }
                let ts = m
                    .get("timestamp")
                    .and_then(|v| v.as_str())
                    .map(iso_ms)
                    .unwrap_or(0);
                if ts > 0 {
                    if created_at == 0 {
                        created_at = ts;
                    }
                    updated_at = updated_at.max(ts);
                }
                let mut message = text_msg(role, &parsed.text, ts);
                message.images = parsed.images;
                messages.push(message);
            }
        }
    }
    assign_seq(&mut messages);
    Ok(GeminiParse {
        session_id,
        messages,
        created_at,
        updated_at,
        unknown_lines: unknown,
    })
}

fn build_meta(r: &SessionFileRef, p: &GeminiParse, cwd: &str) -> SessionMeta {
    let native = p.session_id.clone().unwrap_or_else(|| r.native_id.clone());
    let title = title_from_messages(&p.messages).unwrap_or_else(|| UNTITLED.to_string());
    SessionMeta {
        key: format!("gemini:{native}"),
        host: String::new(),
        id: native,
        agent: AgentId::Gemini,
        title,
        project_path: cwd.to_string(),
        project_name: project_name_of(cwd),
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
        git_branch: None,
        model: None,
        tokens_used: None,
        archived: false,
        source: None,
        favorite: false,
        pinned: false,
    }
}

/// …/tmp/<slug>/chats/session-*.jsonl → slug
fn slug_of(path: &Path) -> Option<String> {
    path.ancestors()
        .nth(2)
        .and_then(|d| d.file_name())
        .map(|s| s.to_string_lossy().to_string())
}

impl GeminiAdapter {
    fn cwd_for(&self, r: &SessionFileRef) -> String {
        slug_of(Path::new(&r.file_path))
            .and_then(|slug| self.slug_map().get(&slug).cloned())
            .unwrap_or_default()
    }
}

impl AgentAdapter for GeminiAdapter {
    fn agent(&self) -> AgentId {
        AgentId::Gemini
    }

    fn list_session_files(&self) -> Result<Vec<SessionFileRef>> {
        let mut refs = Vec::new();
        let Ok(slugs) = fs::read_dir(&self.root) else {
            return Ok(refs);
        };
        for slug in slugs.flatten() {
            let chats = slug.path().join("chats");
            let Ok(entries) = fs::read_dir(&chats) else {
                continue;
            };
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if !name.starts_with("session-") || !name.ends_with(".jsonl") {
                    continue;
                }
                let Ok(meta) = entry.metadata() else { continue };
                if !meta.is_file() || meta.len() == 0 {
                    continue;
                }
                refs.push(SessionFileRef {
                    agent: AgentId::Gemini,
                    native_id: name.trim_end_matches(".jsonl").to_string(),
                    file_path: entry.path().to_string_lossy().to_string(),
                    mtime_ms: mtime_ms(&meta),
                    size: meta.len() as i64,
                });
            }
        }
        Ok(refs)
    }

    fn file_ref(&self, path: &Path) -> Option<SessionFileRef> {
        // 只认 chats/session-*.jsonl(tmp 下其他 jsonl 不是会话)
        let name = path.file_name()?.to_string_lossy().to_string();
        if !name.starts_with("session-") || !path.to_string_lossy().contains("/chats/") {
            return None;
        }
        default_file_ref(self.agent(), path)
    }

    fn parse_session(&self, r: &SessionFileRef) -> Result<ParsedSession> {
        let parsed = parse_gemini_jsonl(Path::new(&r.file_path), false)?;
        let meta = build_meta(r, &parsed, &self.cwd_for(r));
        let units = units_from_messages(&parsed.messages);
        Ok(ParsedSession {
            meta,
            units,
            unknown_line_count: parsed.unknown_lines,
        })
    }

    fn parse_transcript(&self, r: &SessionFileRef) -> Result<ParsedTranscript> {
        let parsed = parse_gemini_jsonl(Path::new(&r.file_path), true)?;
        Ok(ParsedTranscript {
            meta: build_meta(r, &parsed, &self.cwd_for(r)),
            mainline: parsed.messages,
            sidechains: Vec::new(),
            unknown_line_count: parsed.unknown_lines,
        })
    }

    fn with_custom_root(&self, dir: PathBuf) -> Box<dyn AgentAdapter> {
        // `~/.gemini` 形态(含 tmp/)则 projects.json 在其顶层;直接选中 tmp
        // 形态则上一层找。侧档必须相对 dir 派生,落回默认家就会拿错 cwd 映射
        let (root, projects_json) = if dir.join("tmp").is_dir() {
            (dir.join("tmp"), dir.join("projects.json"))
        } else {
            let pj = dir
                .parent()
                .map(|p| p.join("projects.json"))
                .unwrap_or_else(|| dir.join("projects.json"));
            (dir, pj)
        };
        Box::new(Self {
            root,
            projects_json,
            slug_cache: std::sync::Mutex::new(None),
        })
    }

    fn data_roots(&self) -> Vec<PathBuf> {
        vec![self.root.clone()]
    }
}
