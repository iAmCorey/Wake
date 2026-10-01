use super::parse_utils::*;
use super::AgentAdapter;
use crate::models::*;
use anyhow::Result;
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

/// Antigravity IDE(Google 编辑器,与 CLI、桌面端是三件工具):
/// 扫描 `~/.gemini/antigravity-ide/brain/<uuid>/.system_generated/logs/transcript.jsonl`,
/// 解析完整的用户请求、模型思考过程、工具调用与输出、以及检查点摘要。
/// 用户的贴图另存在同会话的 `.user_uploaded/media_<ms>.<ext>` 目录里,
/// 正文中不含图片数据。
///
/// 与 `antigravity`(读 `~/.gemini/antigravity-cli` 的 summaries,只有元数据卡片)
/// 是两个独立 adapter、各用各的 `AgentId`(CLI = `Antigravity`,IDE = `AntigravityIde`)。
/// 刻意不共用一个笼统的 "Antigravity" 名:那会让人以为桌面端(agy 本体)也支持了。
/// `~/.gemini/antigravity/` 是桌面端的数据,不归本 adapter。
pub struct AntigravityIdeAdapter {
    brain_root: Option<PathBuf>,
    projects_json: PathBuf,
    projects_cache: std::sync::Mutex<Option<(i64, HashMap<String, String>)>>,
}

impl AntigravityIdeAdapter {
    pub fn new() -> Self {
        let gemini = super::home_dir().unwrap_or_default().join(".gemini");
        Self {
            // 只认 IDE 自己的数据根;`~/.gemini/antigravity/` 是桌面端的数据,
            // 两者不是一回事,不要互相回退
            brain_root: Some(gemini.join("antigravity-ide").join("brain")),
            projects_json: gemini.join("projects.json"),
            projects_cache: std::sync::Mutex::new(None),
        }
    }

    fn projects_map(&self) -> HashMap<String, String> {
        let mtime = std::fs::metadata(&self.projects_json)
            .map(|m| mtime_ms(&m))
            .unwrap_or(0);
        {
            let cache = self.projects_cache.lock().unwrap();
            if let Some((t, map)) = cache.as_ref() {
                if *t == mtime {
                    return map.clone();
                }
            }
        }
        let mut out = HashMap::new();
        if let Ok(raw) = std::fs::read_to_string(&self.projects_json) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) {
                if let Some(serde_json::Value::Object(map)) = v.get("projects") {
                    for (path, name) in map {
                        if let Some(n) = name.as_str() {
                            out.insert(path.clone(), n.to_string());
                        }
                    }
                }
            }
        }
        *self.projects_cache.lock().unwrap() = Some((mtime, out.clone()));
        out
    }

    fn lookup_project_path(&self, candidate: &str) -> Option<String> {
        if candidate.is_empty() {
            return None;
        }
        let map = self.projects_map();
        let cand_norm = candidate.to_lowercase().replace('/', "\\");
        let mut best: Option<(&String, usize)> = None;
        for (proj_path, _) in &map {
            let p_norm = proj_path.to_lowercase().replace('/', "\\");
            if cand_norm.starts_with(&p_norm)
                && best.as_ref().map_or(true, |(_, len)| p_norm.len() > *len)
            {
                best = Some((proj_path, p_norm.len()));
            }
        }
        best.map(|(p, _)| p.clone())
    }

    fn build_meta(&self, r: &SessionFileRef, parsed: &AntigravityIdeParse) -> SessionMeta {
        let title = parsed
            .title
            .as_deref()
            .filter(|t| !t.is_empty())
            .map(clean_title_candidate)
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| UNTITLED.to_string());

        let mut project_path = parsed.project_path.clone().unwrap_or_default();
        if !project_path.is_empty() {
            let p = Path::new(&project_path);
            if p.is_file() || p.extension().is_some() {
                if let Some(parent) = p.parent() {
                    project_path = parent.to_string_lossy().to_string();
                }
            }
        }
        if let Some(mapped) = self.lookup_project_path(&project_path) {
            project_path = mapped;
        }

        let created_at = if parsed.created_at > 0 {
            parsed.created_at
        } else {
            r.mtime_ms
        };
        let updated_at = if parsed.updated_at > 0 {
            parsed.updated_at
        } else {
            r.mtime_ms
        };

        SessionMeta {
            key: format!("antigravity:{}", r.native_id),
            host: String::new(),
            id: r.native_id.clone(),
            agent: AgentId::AntigravityIde,
            title,
            project_name: project_name_of(&project_path),
            project_path,
            file_path: r.file_path.clone(),
            created_at,
            updated_at,
            message_count: parsed.messages.len() as i64,
            size_bytes: r.size,
            git_branch: None,
            model: parsed.model.clone(),
            tokens_used: None,
            archived: false,
            source: None,
            favorite: false,
            pinned: false,
        }
    }

    fn parse(
        &self,
        r: &SessionFileRef,
        decode_images: bool,
    ) -> Result<(SessionMeta, Vec<TranscriptMessage>, u32)> {
        let parsed = parse_antigravity_jsonl(Path::new(&r.file_path), decode_images)?;
        let meta = self.build_meta(r, &parsed);
        Ok((meta, parsed.messages, parsed.unknown_lines))
    }
}

struct AntigravityIdeParse {
    messages: Vec<TranscriptMessage>,
    title: Option<String>,
    project_path: Option<String>,
    model: Option<String>,
    created_at: i64,
    updated_at: i64,
    unknown_lines: u32,
}

#[derive(Default)]
struct PendingAssistant {
    content: String,
    thinking: Option<String>,
    tool_calls: Vec<ToolCallView>,
    timestamp: Option<i64>,
    model: Option<String>,
}

fn flush_pending_assistant(pending: &mut PendingAssistant, messages: &mut Vec<TranscriptMessage>) {
    if pending.content.is_empty() && pending.thinking.is_none() && pending.tool_calls.is_empty() {
        return;
    }
    let (clipped, truncated) = clip(&pending.content, MAX_MSG_TEXT);
    let thinking = pending.thinking.take().map(|t| clip(&t, MAX_TOOL_IO).0);
    messages.push(TranscriptMessage {
        seq: 0,
        role: Role::Assistant,
        kind: MessageKind::Text,
        text: clipped,
        truncated,
        tool_calls: std::mem::take(&mut pending.tool_calls),
        thinking,
        timestamp: pending.timestamp,
        model: pending.model.take(),
        images: Vec::new(),
    });
    pending.content.clear();
    pending.timestamp = None;
}

fn extract_user_request(raw: &str) -> (String, Option<String>, Option<String>) {
    let user_req = if let Some(start) = raw.find("<USER_REQUEST>") {
        let after = &raw[start + "<USER_REQUEST>".len()..];
        if let Some(end) = after.find("</USER_REQUEST>") {
            after[..end].trim().to_string()
        } else {
            after.trim().to_string()
        }
    } else {
        raw.trim().to_string()
    };

    let mut doc_path = None;
    if let Some(pos) = raw.find("Active Document:") {
        let after = &raw[pos + "Active Document:".len()..];
        let line = after.lines().next().unwrap_or("").trim();
        let path_part = if let Some((p, _)) = line.split_once(" (") {
            p.trim()
        } else {
            line
        };
        if !path_part.is_empty() {
            doc_path = Some(path_part.to_string());
        }
    }

    let mut model = None;
    if let Some(pos) = raw.find("Model Selection` from") {
        let after = &raw[pos..];
        if let Some(to_pos) = after.find(" to ") {
            let after_to = &after[to_pos + 4..];
            let end_idx = after_to
                .find(". No need")
                .or_else(|| after_to.find(".\r\n"))
                .or_else(|| after_to.find(".\n"))
                .or_else(|| after_to.find('\n'))
                .unwrap_or(after_to.len());
            let m = after_to[..end_idx].trim().trim_end_matches('.');
            if !m.is_empty() {
                model = Some(m.to_string());
            }
        }
    }

    (user_req, doc_path, model)
}

fn parse_tool_args(
    _tool_name: &str,
    args_val: Option<&serde_json::Value>,
) -> (String, Option<String>, Option<String>) {
    let Some(args_val) = args_val else {
        return (String::new(), None, None);
    };

    let obj: Option<serde_json::Value> = match args_val {
        serde_json::Value::Object(_) => Some(args_val.clone()),
        serde_json::Value::String(s) => serde_json::from_str(s).ok(),
        _ => None,
    };

    let mut preview = String::new();
    let mut cwd = None;

    if let Some(serde_json::Value::Object(map)) = obj.as_ref() {
        for key in &["Cwd", "DirectoryPath", "SearchPath"] {
            if let Some(val) = map.get(*key).and_then(|v| v.as_str()) {
                let v = val.trim_matches(|c| c == '"' || c == '\'').trim();
                if !v.is_empty() {
                    cwd = Some(v.to_string());
                    break;
                }
            }
        }

        for key in &[
            "toolSummary",
            "toolAction",
            "CommandLine",
            "TargetFile",
            "AbsolutePath",
            "Query",
            "DirectoryPath",
        ] {
            if let Some(val) = map.get(*key).and_then(|v| v.as_str()) {
                let v = val.trim_matches(|c| c == '"' || c == '\'').trim();
                if !v.is_empty() {
                    preview = v.to_string();
                    break;
                }
            }
        }
    }

    if preview.is_empty() {
        preview = match args_val {
            serde_json::Value::String(s) => s.trim().to_string(),
            _ => args_val.to_string(),
        };
    }
    let preview = clip(&preview, 120).0;

    let input_str = match args_val {
        serde_json::Value::String(s) => Some(s.clone()),
        _ => Some(args_val.to_string()),
    };

    (preview, input_str, cwd)
}

/// 单张用户贴图的上限,防某个异常文件把内存吃穿(真机实测都在 300KB 内)
const MAX_UPLOAD_IMAGE_BYTES: u64 = 16 * 1024 * 1024;

struct UploadedImage {
    ms: i64,
    path: PathBuf,
}

/// `.user_uploaded/media_<ms>.<ext>`:文件名里的毫秒戳是贴图上传时刻,和它所属
/// 那条用户消息的 `created_at` 只差几秒(实测 36 张:28 张 ≤60s、35 张 ≤5min)
fn uploaded_images(session_dir: &Path) -> Vec<UploadedImage> {
    let Ok(entries) = std::fs::read_dir(session_dir.join(".user_uploaded")) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let Some(rest) = name.strip_prefix("media_") else {
            continue;
        };
        let Some((stem, _)) = rest.split_once('.') else {
            continue;
        };
        let Ok(ms) = stem.parse::<i64>() else {
            continue;
        };
        out.push(UploadedImage {
            ms,
            path: entry.path(),
        });
    }
    out.sort_by_key(|u| u.ms);
    out
}

fn media_type_for(path: &Path) -> Option<&'static str> {
    match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        "bmp" => Some("image/bmp"),
        _ => None,
    }
}

/// 按时间戳把贴图回挂到最近的用户消息(尾部插入,`text_offset` = 正文长度)。
/// 超过半小时对不上的宁可不挂,也不硬塞到别的轮次。
fn attach_uploaded_images(messages: &mut [TranscriptMessage], session_dir: &Path) {
    const MAX_AGE_MS: i64 = 30 * 60 * 1000;
    let uploads = uploaded_images(session_dir);
    if uploads.is_empty() {
        return;
    }
    let users: Vec<(usize, i64)> = messages
        .iter()
        .enumerate()
        .filter(|(_, m)| m.role == Role::User)
        .filter_map(|(i, m)| m.timestamp.map(|t| (i, t)))
        .collect();
    if users.is_empty() {
        return;
    }
    for upload in uploads {
        let Some((idx, ts)) = users
            .iter()
            .min_by_key(|(_, t)| (t - upload.ms).abs())
            .copied()
        else {
            break;
        };
        if (ts - upload.ms).abs() > MAX_AGE_MS {
            continue;
        }
        let Some(media_type) = media_type_for(&upload.path) else {
            continue;
        };
        let Ok(meta) = std::fs::metadata(&upload.path) else {
            continue;
        };
        if meta.len() == 0 || meta.len() > MAX_UPLOAD_IMAGE_BYTES {
            continue;
        }
        let Ok(bytes) = std::fs::read(&upload.path) else {
            continue;
        };
        let text_offset = messages[idx].text.len();
        messages[idx].images.push(ImageAttachment {
            media_type: media_type.to_string(),
            bytes,
            text_offset,
        });
    }
}

fn parse_antigravity_jsonl(path: &Path, decode_images: bool) -> Result<AntigravityIdeParse> {
    let file = File::open(path)?;
    let reader = BufReader::with_capacity(1 << 20, file);

    let mut messages: Vec<TranscriptMessage> = Vec::new();
    let mut unknown_lines = 0u32;
    let mut first_ts = 0i64;
    let mut last_ts = 0i64;
    let mut title_candidate = None;
    let mut candidate_doc_path = None;
    let mut candidate_tool_cwd = None;
    let mut current_model = None;

    let mut pending_assistant = PendingAssistant::default();

    for line in reader.lines() {
        let Ok(line) = line else {
            unknown_lines += 1;
            continue;
        };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(row) = serde_json::from_str::<serde_json::Value>(&line) else {
            unknown_lines += 1;
            continue;
        };

        let step_type = row.get("type").and_then(|v| v.as_str()).unwrap_or("");
        let status = row.get("status").and_then(|v| v.as_str()).unwrap_or("");
        let ts_str = row.get("created_at").and_then(|v| v.as_str()).unwrap_or("");
        let ts = iso_ms(ts_str);
        if ts > 0 {
            if first_ts == 0 {
                first_ts = ts;
            }
            last_ts = ts;
        }

        match step_type {
            "USER_INPUT" => {
                flush_pending_assistant(&mut pending_assistant, &mut messages);
                let content = row.get("content").and_then(|v| v.as_str()).unwrap_or("");
                let (user_req, doc_path, model_opt) = extract_user_request(content);
                if title_candidate.is_none() && !user_req.is_empty() {
                    let cleaned = clean_title_candidate(&user_req);
                    if !cleaned.is_empty() {
                        title_candidate = Some(cleaned);
                    }
                }
                if candidate_doc_path.is_none() && doc_path.is_some() {
                    candidate_doc_path = doc_path;
                }
                if let Some(m) = model_opt {
                    current_model = Some(m);
                }

                let (clipped, truncated) = clip(&user_req, MAX_MSG_TEXT);
                messages.push(TranscriptMessage {
                    seq: 0,
                    role: Role::User,
                    kind: user_kind(&user_req),
                    text: clipped,
                    truncated,
                    tool_calls: Vec::new(),
                    thinking: None,
                    timestamp: if ts > 0 { Some(ts) } else { None },
                    model: current_model.clone(),
                    images: Vec::new(),
                });
            }
            "PLANNER_RESPONSE" => {
                if pending_assistant.timestamp.is_none() && ts > 0 {
                    pending_assistant.timestamp = Some(ts);
                }
                if current_model.is_some() && pending_assistant.model.is_none() {
                    pending_assistant.model = current_model.clone();
                }
                if let Some(th) = row.get("thinking").and_then(|v| v.as_str()) {
                    if !th.trim().is_empty() {
                        if let Some(ref mut existing) = pending_assistant.thinking {
                            existing.push_str("\n\n");
                            existing.push_str(th.trim());
                        } else {
                            pending_assistant.thinking = Some(th.trim().to_string());
                        }
                    }
                }
                if let Some(c) = row.get("content").and_then(|v| v.as_str()) {
                    if !c.trim().is_empty() {
                        if !pending_assistant.content.is_empty() {
                            pending_assistant.content.push_str("\n\n");
                        }
                        pending_assistant.content.push_str(c.trim());
                    }
                }
                if let Some(calls) = row.get("tool_calls").and_then(|v| v.as_array()) {
                    let step_idx = row.get("step_index").and_then(|v| v.as_i64()).unwrap_or(0);
                    for (ci, tc) in calls.iter().enumerate() {
                        let name = tc.get("name").and_then(|v| v.as_str()).unwrap_or("tool");
                        let args = tc.get("args");
                        let (preview, input_str, cwd_cand) = parse_tool_args(name, args);
                        if candidate_tool_cwd.is_none() && cwd_cand.is_some() {
                            candidate_tool_cwd = cwd_cand;
                        }
                        pending_assistant.tool_calls.push(ToolCallView {
                            id: format!("tc-{}-{}", step_idx, ci),
                            name: name.to_string(),
                            input_preview: preview,
                            input: input_str,
                            output: None,
                            is_error: false,
                            sidechain_ref: None,
                        });
                    }
                }
            }
            "VIEW_FILE" | "RUN_COMMAND" | "GREP_SEARCH" | "LIST_DIRECTORY" | "CODE_ACTION"
            | "BROWSER_SUBAGENT" | "SEARCH_WEB" | "READ_URL_CONTENT" | "ASK_QUESTION"
            | "GENERIC" | "ERROR_MESSAGE" => {
                let content = row.get("content").and_then(|v| v.as_str()).unwrap_or("");
                let is_err = status == "ERROR" || step_type == "ERROR_MESSAGE";
                let mut matched = false;
                for tc in pending_assistant.tool_calls.iter_mut().rev() {
                    if tc.output.is_none() {
                        tc.output = Some(clip(content.trim(), MAX_TOOL_IO).0);
                        tc.is_error = is_err;
                        matched = true;
                        break;
                    }
                }
                if !matched && is_err && !content.trim().is_empty() {
                    let (clipped, truncated) = clip(content.trim(), MAX_MSG_TEXT);
                    messages.push(TranscriptMessage {
                        seq: 0,
                        role: Role::System,
                        kind: MessageKind::Text,
                        text: clipped,
                        truncated,
                        tool_calls: Vec::new(),
                        thinking: None,
                        timestamp: if ts > 0 { Some(ts) } else { None },
                        model: None,
                        images: Vec::new(),
                    });
                }
            }
            "CHECKPOINT" => {
                flush_pending_assistant(&mut pending_assistant, &mut messages);
                let content = row.get("content").and_then(|v| v.as_str()).unwrap_or("");
                if !content.trim().is_empty() {
                    let (clipped, truncated) = clip(content.trim(), MAX_MSG_TEXT);
                    messages.push(TranscriptMessage {
                        seq: 0,
                        role: Role::System,
                        kind: MessageKind::CompactSummary,
                        text: clipped,
                        truncated,
                        tool_calls: Vec::new(),
                        thinking: None,
                        timestamp: if ts > 0 { Some(ts) } else { None },
                        model: None,
                        images: Vec::new(),
                    });
                }
            }
            "CONVERSATION_HISTORY" | "KNOWLEDGE_ARTIFACTS" | "SYSTEM_MESSAGE" => {}
            _ => {
                unknown_lines += 1;
            }
        }
    }

    flush_pending_assistant(&mut pending_assistant, &mut messages);

    if decode_images {
        if let Some(session_dir) = path
            .parent()
            .and_then(|p| p.parent())
            .and_then(|p| p.parent())
        {
            attach_uploaded_images(&mut messages, session_dir);
        }
    }

    assign_seq(&mut messages);

    Ok(AntigravityIdeParse {
        messages,
        title: title_candidate,
        project_path: candidate_tool_cwd.or(candidate_doc_path),
        model: current_model,
        created_at: first_ts,
        updated_at: last_ts,
        unknown_lines,
    })
}

impl AgentAdapter for AntigravityIdeAdapter {
    fn agent(&self) -> AgentId {
        AgentId::AntigravityIde
    }

    fn list_session_files(&self) -> Result<Vec<SessionFileRef>> {
        let mut refs = Vec::new();
        if let Some(ref brain) = self.brain_root {
            if let Ok(entries) = std::fs::read_dir(brain) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if !path.is_dir() {
                        continue;
                    }
                    let id = entry.file_name().to_string_lossy().to_string();
                    if id.starts_with('.') || id == "tempmediaStorage" {
                        continue;
                    }
                    let transcript = path
                        .join(".system_generated")
                        .join("logs")
                        .join("transcript.jsonl");
                    if let Ok(meta) = std::fs::metadata(&transcript) {
                        if meta.is_file() && meta.len() > 0 {
                            refs.push(SessionFileRef {
                                agent: AgentId::AntigravityIde,
                                native_id: id,
                                file_path: transcript.to_string_lossy().to_string(),
                                mtime_ms: mtime_ms(&meta),
                                size: meta.len() as i64,
                            });
                        }
                    }
                }
            }
        }
        Ok(refs)
    }

    fn file_ref(&self, path: &Path) -> Option<SessionFileRef> {
        let name = path.file_name()?.to_str()?;
        if name != "transcript.jsonl" {
            return None;
        }
        let logs = path.parent()?;
        if logs.file_name()?.to_str()? != "logs" {
            return None;
        }
        let sys = logs.parent()?;
        if sys.file_name()?.to_str()? != ".system_generated" {
            return None;
        }
        let session_dir = sys.parent()?;
        let native_id = session_dir.file_name()?.to_str()?.to_string();
        if native_id.starts_with('.') || native_id == "tempmediaStorage" {
            return None;
        }
        let meta = std::fs::metadata(path).ok()?;
        if !meta.is_file() || meta.len() == 0 {
            return None;
        }
        Some(SessionFileRef {
            agent: AgentId::AntigravityIde,
            native_id,
            file_path: path.to_string_lossy().to_string(),
            mtime_ms: mtime_ms(&meta),
            size: meta.len() as i64,
        })
    }

    fn parse_session(&self, r: &SessionFileRef) -> Result<ParsedSession> {
        let (meta, messages, unknown_line_count) = self.parse(r, false)?;
        Ok(ParsedSession::derive(meta, &messages, unknown_line_count))
    }

    fn parse_transcript(&self, r: &SessionFileRef) -> Result<ParsedTranscript> {
        let (meta, messages, unknown_line_count) = self.parse(r, true)?;
        Ok(ParsedTranscript {
            meta,
            mainline: messages,
            sidechains: Vec::new(),
            unknown_line_count,
        })
    }

    fn session_paths(&self, meta: &SessionMeta) -> Vec<String> {
        let p = Path::new(&meta.file_path);
        if meta.file_path.ends_with("transcript.jsonl") {
            if let Some(session_dir) = p.parent().and_then(|p| p.parent()).and_then(|p| p.parent())
            {
                if session_dir.is_dir() {
                    return vec![session_dir.to_string_lossy().to_string()];
                }
            }
        }
        vec![meta.file_path.clone()]
    }

    fn with_custom_root(&self, dir: PathBuf) -> Box<dyn AgentAdapter> {
        let brain_root = if dir.file_name().and_then(|s| s.to_str()) == Some("brain") {
            Some(dir.clone())
        } else if dir.join("brain").is_dir() {
            Some(dir.join("brain"))
        } else if dir.join("antigravity-ide").join("brain").is_dir() {
            Some(dir.join("antigravity-ide").join("brain"))
        } else {
            Some(dir.clone())
        };
        let projects_json = {
            let p = dir.join("projects.json");
            if p.is_file() {
                p
            } else if dir
                .parent()
                .and_then(|p| p.file_name())
                .and_then(|s| s.to_str())
                == Some("antigravity-ide")
            {
                // 根是 `…/antigravity-ide/brain` 形态(远程缓存挂载点):
                // projects.json 在再上一层的 `.gemini` 顶层。缓存里此刻可能
                // 还没同步,不做 is_file 判据
                dir.parent()
                    .and_then(|p| p.parent())
                    .map(|g| g.join("projects.json"))
                    .unwrap_or_else(|| dir.join("projects.json"))
            } else {
                super::home_dir()
                    .unwrap_or_default()
                    .join(".gemini")
                    .join("projects.json")
            }
        };
        Box::new(Self {
            brain_root,
            projects_json,
            projects_cache: std::sync::Mutex::new(None),
        })
    }

    fn data_roots(&self) -> Vec<PathBuf> {
        self.brain_root.iter().cloned().collect()
    }
}
