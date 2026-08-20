use super::parse_utils::*;
use super::sqlite_ro::open_sqlite_ro;
use super::{units_from_messages, AgentAdapter};
use crate::models::*;
use anyhow::Result;
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Cursor CLI:`~/.cursor/projects/<slug>/agent-transcripts/<uuid>/<uuid>.jsonl` 明文。
/// 行结构 {role, message:{content:[{type:text|tool_use}]}} + {type:"turn_ended"}。
/// user 正文包在 <timestamp>/<user_query> 壳里;transcript 不含 cwd,
/// 从有损 slug 目录名 DFS 反推真实路径。IDE chats 正文在 store.db 加密,
/// 但改过的标题在 Application Support 的 conversation-search.db 明文。
pub struct CursorAdapter {
    root: PathBuf,
    titles_db: PathBuf,
    titles: Mutex<Option<(i64, HashMap<String, String>)>>,
}

impl CursorAdapter {
    pub fn new() -> Self {
        let home = dirs::home_dir().unwrap_or_default();
        Self {
            root: home.join(".cursor").join("projects"),
            titles_db: dirs::data_dir()
                .unwrap_or_else(|| home.join("Library").join("Application Support"))
                .join("Cursor")
                .join("User")
                .join("globalStorage")
                .join("conversation-search.db"),
            titles: Mutex::new(None),
        }
    }

    fn renamed_title(&self, id: &str) -> Option<String> {
        self.renamed_titles().get(id).cloned()
    }

    fn renamed_titles(&self) -> HashMap<String, String> {
        let mtime = fs::metadata(&self.titles_db)
            .map(|m| mtime_ms(&m))
            .unwrap_or(0);
        {
            let cache = self.titles.lock().unwrap();
            if let Some((t, map)) = cache.as_ref() {
                if *t == mtime {
                    return map.clone();
                }
            }
        }
        let map = read_conversation_titles(&self.titles_db);
        *self.titles.lock().unwrap() = Some((mtime, map.clone()));
        map
    }

    fn with_renamed_title(&self, mut meta: SessionMeta) -> SessionMeta {
        if let Some(t) = self.renamed_title(&meta.id) {
            meta.title = t;
        }
        meta
    }
}

/// Cursor IDE `/rename` 写在 conversation-search.db 的 conversations.title
fn read_conversation_titles(db: &Path) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let Some(ro) = open_sqlite_ro(db, "cursor-titles") else {
        return map;
    };
    let Ok(mut stmt) = ro.conn.prepare(
        "SELECT id, title FROM conversations WHERE title IS NOT NULL AND trim(title) != ''",
    ) else {
        return map;
    };
    let Ok(rows) = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
    else {
        return map;
    };
    for row in rows.flatten() {
        if !row.0.is_empty() && !row.1.trim().is_empty() {
            map.insert(row.0, row.1.trim().to_string());
        }
    }
    map
}

/// "Users-corey-Github-image-translate" → "/Users/corey/Github/image-translate"。
/// Cursor 把路径里的 `/` 和 `_` 都压成 `-`,所以 slug 的 `-` 可能是路径分隔、
/// 连字符目录名、或下划线。按磁盘真实存在的目录 DFS(优先短段);
/// 短段走不通再拼更长名字,项目目录已删时回退直译(`/`)。
fn decode_slug(slug: &str) -> String {
    decode_slug_at(Path::new("/"), slug)
}

fn decode_slug_at(root: &Path, slug: &str) -> String {
    let parts: Vec<&str> = slug.split('-').collect();
    fn dfs(base: PathBuf, parts: &[&str]) -> Option<PathBuf> {
        if parts.is_empty() {
            return Some(base);
        }
        for n in 1..=parts.len() {
            for name in dir_name_candidates(&parts[..n]) {
                let cand = base.join(&name);
                if cand.is_dir() {
                    if let Some(hit) = dfs(cand, &parts[n..]) {
                        return Some(hit);
                    }
                }
            }
        }
        None
    }
    dfs(root.to_path_buf(), &parts)
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|| {
            let rest = slug.replace('-', "/");
            if root == Path::new("/") {
                format!("/{rest}")
            } else {
                root.join(rest.trim_start_matches('/'))
                    .to_string_lossy()
                    .into_owned()
            }
        })
}

/// 一段目录名:单段原样;多段把 `-`/`_` 的组合都试一遍(mask=0 为全连字符,保持旧行为优先)。
fn dir_name_candidates(parts: &[&str]) -> Vec<String> {
    if parts.is_empty() {
        return Vec::new();
    }
    if parts.len() == 1 {
        return vec![parts[0].to_string()];
    }
    let slots = parts.len() - 1;
    if slots > 8 {
        return vec![parts.join("-"), parts.join("_")];
    }
    let mut out = Vec::with_capacity(1 << slots);
    for mask in 0..(1u32 << slots) {
        let mut s = String::from(parts[0]);
        for i in 0..slots {
            s.push(if mask & (1 << i) == 0 { '-' } else { '_' });
            s.push_str(parts[i + 1]);
        }
        out.push(s);
    }
    out
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
    text: Vec<String>,
    tool_calls: Vec<ToolCallView>,
    timestamp: Option<i64>,
}

fn flush_assistant(pending: &mut Option<PendingAssistant>, messages: &mut Vec<TranscriptMessage>) {
    let Some(p) = pending.take() else { return };
    let text = p.text.join("\n\n");
    if text.is_empty() && p.tool_calls.is_empty() {
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
    });
}

fn parse_cursor_jsonl(path: &Path) -> Result<CursorParse> {
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
                let mut parts: Vec<String> = Vec::new();
                let mut ts = 0i64;
                for b in &blocks {
                    if b.get("type").and_then(|v| v.as_str()) != Some("text") {
                        continue;
                    }
                    let Some(raw) = b.get("text").and_then(|v| v.as_str()) else {
                        continue;
                    };
                    if let Some(t) = extract_tag(raw, "timestamp") {
                        let parsed = cursor_ts_ms(&t);
                        if parsed > 0 {
                            ts = parsed;
                        }
                    }
                    // 真实输入在 <user_query> 壳内;没有壳的行(注入上下文等)原样保留
                    let body = extract_tag(raw, "user_query").unwrap_or_else(|| raw.to_string());
                    if !body.trim().is_empty() {
                        parts.push(body.trim().to_string());
                    }
                }
                let text = parts.join("\n\n");
                if text.trim().is_empty() {
                    continue;
                }
                if ts > 0 {
                    if created_at == 0 {
                        created_at = ts;
                    }
                    updated_at = updated_at.max(ts);
                }
                messages.push(text_msg(Role::User, &text, ts));
            }
            "assistant" => {
                // Cursor 逐块落行且无消息 id,连续 assistant 行并成一条,turn_ended/user 处 flush
                let p = pending.get_or_insert_with(PendingAssistant::default);
                for b in &blocks {
                    match b.get("type").and_then(|v| v.as_str()) {
                        Some("text") => {
                            if let Some(t) = b.get("text").and_then(|v| v.as_str()) {
                                if !t.trim().is_empty() {
                                    p.text.push(t.to_string());
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

    fn detect(&self) -> bool {
        self.root.is_dir()
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
        let parsed = parse_cursor_jsonl(Path::new(&r.file_path))?;
        let meta = self.with_renamed_title(build_meta(r, &parsed));
        let units = units_from_messages(&parsed.messages);
        Ok(ParsedSession {
            meta,
            units,
            unknown_line_count: parsed.unknown_lines,
        })
    }

    fn parse_transcript(&self, r: &SessionFileRef) -> Result<ParsedTranscript> {
        let parsed = parse_cursor_jsonl(Path::new(&r.file_path))?;
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
            meta: self.with_renamed_title(build_meta(r, &parsed)),
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
        Ok(parse_cursor_jsonl(&file)?.messages)
    }

    fn watch_paths(&self) -> Vec<PathBuf> {
        let mut v = Vec::new();
        if self.detect() {
            v.push(self.root.clone());
        }
        if let Some(dir) = self.titles_db.parent() {
            if dir.is_dir() {
                v.push(dir.to_path_buf());
            }
        }
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_slug_restores_underscores() {
        let tmp = tempfile::tempdir().unwrap();
        let proj = tmp.path().join("Works").join("app_av4");
        fs::create_dir_all(&proj).unwrap();
        let got = decode_slug_at(tmp.path(), "Works-app-av4");
        assert_eq!(Path::new(&got), proj.as_path());
    }

    #[test]
    fn decode_slug_hyphenated_dir_still_matches() {
        let tmp = tempfile::tempdir().unwrap();
        let proj = tmp.path().join("Github").join("image-translate");
        fs::create_dir_all(&proj).unwrap();
        let got = decode_slug_at(tmp.path(), "Github-image-translate");
        assert_eq!(Path::new(&got), proj.as_path());
    }

    #[test]
    fn decode_slug_missing_falls_back_to_slashes() {
        let got = decode_slug("wakefx-cursor-proj");
        assert_eq!(got, "/wakefx/cursor/proj");
    }

    #[test]
    fn decode_slug_backtracks_when_short_prefix_is_dead_end() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("Works").join("app")).unwrap();
        let proj = tmp.path().join("Works").join("app_av4");
        fs::create_dir_all(&proj).unwrap();
        let got = decode_slug_at(tmp.path(), "Works-app-av4");
        assert_eq!(Path::new(&got), proj.as_path());
    }
}
