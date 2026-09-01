use super::parse_utils::*;
use super::{units_from_messages, AgentAdapter};
use crate::models::*;
use anyhow::Result;
use serde_json::Value;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

/// Cursor CLI:`~/.cursor/projects/<slug>/agent-transcripts/<uuid>/<uuid>.jsonl` 明文。
/// 行结构 {role, message:{content:[{type:text|tool_use}]}} + {type:"turn_ended"}。
/// user 正文包在 <timestamp>/<user_query> 壳里;transcript 不含 cwd,
/// 从有损 slug 目录名 DFS 反推真实路径。IDE chats(store.db 加密)不做。
pub struct CursorAdapter {
    root: PathBuf,
}

impl CursorAdapter {
    pub fn new() -> Self {
        Self {
            root: super::home_dir()
                .unwrap_or_default()
                .join(".cursor")
                .join("projects"),
        }
    }
}

/// "Users-corey-Github-image-translate" → "/Users/corey/Github/image-translate"。
/// '-' 既可能是路径分隔也可能是目录名字符,按磁盘真实存在的目录 DFS(优先短段);
/// 项目目录已删时回退直译。
fn decode_slug(slug: &str) -> String {
    let parts: Vec<&str> = slug.split('-').collect();
    fn dfs(base: PathBuf, parts: &[&str]) -> Option<PathBuf> {
        if parts.is_empty() {
            return Some(base);
        }
        let mut seg = String::new();
        for i in 0..parts.len() {
            if i > 0 {
                seg.push('-');
            }
            seg.push_str(parts[i]);
            let cand = base.join(&seg);
            if cand.is_dir() {
                if let Some(hit) = dfs(cand, &parts[i + 1..]) {
                    return Some(hit);
                }
            }
        }
        None
    }
    dfs(PathBuf::from("/"), &parts)
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|| format!("/{}", slug.replace('-', "/")))
}

/// "Thursday, Jul 23, 2026, 4:00 PM (UTC+8)" → epoch ms,解析失败 = 0
fn cursor_ts_ms(s: &str) -> i64 {
    (|| -> Option<i64> {
        let (dt_part, tz_part) = s.rsplit_once(" (")?;
        let naive =
            chrono::NaiveDateTime::parse_from_str(dt_part.trim(), "%A, %b %d, %Y, %I:%M %p")
                .ok()?;
        let off = tz_part.trim_end_matches(')').strip_prefix("UTC")?;
        let (sign, rest) = match off.as_bytes().first()? {
            b'+' => (1i32, &off[1..]),
            b'-' => (-1i32, &off[1..]),
            _ => (1i32, off),
        };
        let secs = match rest.split_once(':') {
            Some((h, m)) => h.parse::<i32>().ok()? * 3600 + m.parse::<i32>().ok()? * 60,
            None => rest.parse::<i32>().ok()? * 3600,
        };
        let offset = chrono::FixedOffset::east_opt(sign * secs)?;
        use chrono::TimeZone;
        Some(
            offset
                .from_local_datetime(&naive)
                .single()?
                .timestamp_millis(),
        )
    })()
    .unwrap_or(0)
}

struct CursorParse {
    messages: Vec<TranscriptMessage>,
    created_at: i64,
    updated_at: i64,
    unknown_lines: u32,
}

#[derive(Default)]
struct PendingAssistant {
    content: ParsedContent,
    tool_calls: Vec<ToolCallView>,
    timestamp: Option<i64>,
}

fn flush_assistant(pending: &mut Option<PendingAssistant>, messages: &mut Vec<TranscriptMessage>) {
    let Some(p) = pending.take() else { return };
    let ParsedContent { text, images } = p.content;
    if text.is_empty() && p.tool_calls.is_empty() && images.is_empty() {
        return;
    }
    let (clipped, truncated) = clip(&text, MAX_MSG_TEXT);
    messages.push(TranscriptMessage {
        seq: 0,
        role: Role::Assistant,
        kind: MessageKind::Text,
        text: clipped,
        truncated,
        tool_calls: p.tool_calls,
        thinking: None,
        timestamp: p.timestamp,
        model: None,
        images,
    });
}

fn parse_cursor_jsonl(path: &Path, decode_images: bool) -> Result<CursorParse> {
    let _image_budget = transcript_image_decode_budget(decode_images);
    let file = fs::File::open(path)?;
    let reader = BufReader::with_capacity(1 << 20, file);

    let mut messages: Vec<TranscriptMessage> = Vec::new();
    let mut pending: Option<PendingAssistant> = None;
    let mut created_at = 0i64;
    let mut updated_at = 0i64;
    let mut unknown_lines = 0u32;

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
        let Some(role) = row.get("role").and_then(|v| v.as_str()) else {
            match row.get("type").and_then(|v| v.as_str()) {
                Some("turn_ended") => flush_assistant(&mut pending, &mut messages),
                _ => unknown_lines += 1,
            }
            continue;
        };
        let blocks = row
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_array())
            .cloned()
            .unwrap_or_default();

        match role {
            "user" => {
                flush_assistant(&mut pending, &mut messages);
                let mut parsed_message = ParsedContent::default();
                let mut ts = 0i64;
                for b in &blocks {
                    match b.get("type").and_then(|v| v.as_str()) {
                        Some("text") => {
                            let Some(raw) = b.get("text").and_then(|v| v.as_str()) else {
                                continue;
                            };
                            if let Some(t) = extract_tag(raw, "timestamp") {
                                let parsed = cursor_ts_ms(&t);
                                if parsed > 0 {
                                    ts = parsed;
                                }
                            }
                            // 真实输入在 <user_query> 壳内;没有壳的行原样保留
                            let body =
                                extract_tag(raw, "user_query").unwrap_or_else(|| raw.to_string());
                            if !body.trim().is_empty() {
                                parsed_message.push_text(&body);
                            }
                        }
                        _ if is_image_part(b) => {
                            let parsed = content_parts(b, decode_images);
                            parsed_message.append(parsed);
                        }
                        _ => {}
                    }
                }
                let ParsedContent { text, images } = parsed_message;
                if text.trim().is_empty() && images.is_empty() {
                    continue;
                }
                if ts > 0 {
                    if created_at == 0 {
                        created_at = ts;
                    }
                    updated_at = updated_at.max(ts);
                }
                let mut message = text_msg(Role::User, &text, ts);
                message.images = images;
                messages.push(message);
            }
            "assistant" => {
                // Cursor 逐块落行且无消息 id,连续 assistant 行并成一条,turn_ended/user 处 flush
                let p = pending.get_or_insert_with(PendingAssistant::default);
                for b in &blocks {
                    match b.get("type").and_then(|v| v.as_str()) {
                        Some("text") => {
                            if let Some(t) = b.get("text").and_then(|v| v.as_str()) {
                                if !t.trim().is_empty() {
                                    p.content.push_text(t);
                                }
                            }
                        }
                        Some("tool_use") => {
                            let input = b.get("input").cloned().unwrap_or(Value::Null);
                            let name = b.get("name").and_then(|v| v.as_str()).unwrap_or("tool");
                            // transcript 不落盘工具结果,output 恒 None
                            p.tool_calls.push(tool_call_view(
                                String::new(),
                                name,
                                &input,
                                None,
                                false,
                            ));
                        }
                        _ if is_image_part(b) => {
                            let parsed = content_parts(b, decode_images);
                            p.content.append(parsed);
                        }
                        _ => {}
                    }
                }
            }
            _ => unknown_lines += 1,
        }
    }
    flush_assistant(&mut pending, &mut messages);
    assign_seq(&mut messages);
    Ok(CursorParse {
        messages,
        created_at,
        updated_at,
        unknown_lines,
    })
}

fn subagents_dir(r: &SessionFileRef) -> PathBuf {
    Path::new(&r.file_path)
        .parent()
        .unwrap_or(Path::new("."))
        .join("subagents")
}

fn build_meta(r: &SessionFileRef, p: &CursorParse) -> SessionMeta {
    // …/projects/<slug>/agent-transcripts/<uuid>/<uuid>.jsonl → slug
    let cwd = Path::new(&r.file_path)
        .ancestors()
        .nth(3)
        .and_then(|d| d.file_name())
        .map(|s| decode_slug(&s.to_string_lossy()))
        .unwrap_or_default();
    let title = title_from_messages(&p.messages).unwrap_or_else(|| UNTITLED.to_string());
    SessionMeta {
        key: format!("cursor:{}", r.native_id),
        id: r.native_id.clone(),
        agent: AgentId::Cursor,
        title,
        project_path: cwd.clone(),
        project_name: project_name_of(&cwd),
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

impl AgentAdapter for CursorAdapter {
    fn agent(&self) -> AgentId {
        AgentId::Cursor
    }

    fn list_session_files(&self) -> Result<Vec<SessionFileRef>> {
        let mut refs = Vec::new();
        let Ok(projects) = fs::read_dir(&self.root) else {
            return Ok(refs);
        };
        for project in projects.flatten() {
            let transcripts = project.path().join("agent-transcripts");
            let Ok(sessions) = fs::read_dir(&transcripts) else {
                continue;
            };
            for session in sessions.flatten() {
                let Ok(entries) = fs::read_dir(session.path()) else {
                    continue;
                };
                for entry in entries.flatten() {
                    let name = entry.file_name().to_string_lossy().to_string();
                    if !name.ends_with(".jsonl") {
                        continue;
                    }
                    let Ok(meta) = entry.metadata() else { continue };
                    if !meta.is_file() || meta.len() == 0 {
                        continue;
                    }
                    refs.push(SessionFileRef {
                        agent: AgentId::Cursor,
                        native_id: name.trim_end_matches(".jsonl").to_string(),
                        file_path: entry.path().to_string_lossy().to_string(),
                        mtime_ms: mtime_ms(&meta),
                        size: meta.len() as i64,
                    });
                }
            }
        }
        Ok(refs)
    }

    fn file_ref(&self, path: &Path) -> Option<SessionFileRef> {
        // 只认 transcript 主文件;subagents 转录不是独立会话
        let p = path.to_string_lossy();
        if !p.contains("/agent-transcripts/") || p.contains("/subagents/") {
            return None;
        }
        default_file_ref(self.agent(), path)
    }

    fn session_paths(&self, meta: &SessionMeta) -> Vec<String> {
        // 会话是 <uuid>/ 整个目录(含 subagents/),删除时整目录进废纸篓
        Path::new(&meta.file_path)
            .parent()
            .map(|d| vec![d.to_string_lossy().to_string()])
            .unwrap_or_else(|| vec![meta.file_path.clone()])
    }

    fn parse_session(&self, r: &SessionFileRef) -> Result<ParsedSession> {
        let parsed = parse_cursor_jsonl(Path::new(&r.file_path), false)?;
        let meta = build_meta(r, &parsed);
        let units = units_from_messages(&parsed.messages);
        Ok(ParsedSession {
            meta,
            units,
            unknown_line_count: parsed.unknown_lines,
        })
    }

    fn parse_transcript(&self, r: &SessionFileRef) -> Result<ParsedTranscript> {
        let parsed = parse_cursor_jsonl(Path::new(&r.file_path), true)?;
        // subagents/ 子目录与 Claude 同构,但无 meta 边车、主线 Task 调用无 id,
        // 挂不上具体 tool_use——仅列出供导出携带
        let mut sidechains = Vec::new();
        let dir = subagents_dir(r);
        if let Ok(entries) = fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if let Some(id) = name.strip_suffix(".jsonl") {
                    sidechains.push(SidechainInfo {
                        id: id.to_string(),
                        agent_type: None,
                        description: None,
                        tool_use_id: None,
                    });
                }
            }
        }
        Ok(ParsedTranscript {
            meta: build_meta(r, &parsed),
            mainline: parsed.messages,
            sidechains,
            unknown_line_count: parsed.unknown_lines,
        })
    }

    fn load_sidechain(
        &self,
        r: &SessionFileRef,
        sidechain_id: &str,
    ) -> Result<Vec<TranscriptMessage>> {
        let file = subagents_dir(r).join(format!("{sidechain_id}.jsonl"));
        if !file.is_file() {
            return Ok(Vec::new());
        }
        Ok(parse_cursor_jsonl(&file, true)?.messages)
    }

    fn with_custom_root(&self, dir: PathBuf) -> Box<dyn AgentAdapter> {
        // 选中 `~/.cursor` 形态(含 projects/)或直接选中 projects 目录都认
        let root = if dir.join("projects").is_dir() {
            dir.join("projects")
        } else {
            dir
        };
        Box::new(Self { root })
    }

    fn data_roots(&self) -> Vec<PathBuf> {
        vec![self.root.clone()]
    }
}
