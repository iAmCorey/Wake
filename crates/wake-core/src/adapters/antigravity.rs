use super::parse_utils::*;
use super::sqlite_ro::{open_sqlite_ro, virtual_path};
use super::{units_from_messages, AgentAdapter};
use crate::models::*;
use anyhow::{anyhow, Result};
use std::collections::HashMap;
use std::path::PathBuf;

/// Antigravity CLI(Google,binary `agy`):会话正文是加密 .pb,唯一明文是
/// `~/.gemini/antigravity-cli/conversation_summaries.db`(WAL)——只能做
/// 元数据级会话卡片:标题在 preview 列(title 列基本为空)、时间、workspace。
/// 详情页由一条 System 消息承载 preview 与"正文加密"说明,FTS 只搜得到它。
/// 无每会话文件,SessionFileRef 用虚拟路径;打开一律走 sqlite_ro 三级梯度。
pub struct AntigravityAdapter {
    db: PathBuf,
    /// 全表很小(元数据行),按 db mtime 缓存一轮扫描内的重复调用
    rows_cache: MtimeCache<Vec<AgRow>>,
}

impl AntigravityAdapter {
    pub fn new() -> Self {
        Self {
            db: super::home_dir()
                .unwrap_or_default()
                .join(".gemini")
                .join("antigravity-cli")
                .join("conversation_summaries.db"),
            rows_cache: MtimeCache::new(),
        }
    }

    fn rows(&self) -> Option<Vec<AgRow>> {
        let mtime = super::sqlite_ro::db_cache_stamp(&self.db);
        self.rows_cache.get_or_try_build(mtime, || {
            let ro = open_sqlite_ro(&self.db, "antigravity")?;
            let mut stmt = ro
                .conn
                .prepare(
                    "SELECT conversation_id, title, preview, step_count, last_modified_time, workspace_uris
                     FROM conversation_summaries
                     WHERE parent_conversation_id = '' AND nesting_depth = 0",
                )
                .ok()?;
            let rows = stmt
                .query_map([], |r| {
                    Ok(AgRow {
                        id: r.get(0)?,
                        title: r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                        preview: r.get::<_, Option<String>>(2)?.unwrap_or_default(),
                        step_count: r.get::<_, Option<i64>>(3)?.unwrap_or(0),
                        modified_ms: sqlite_dt_ms(r.get::<_, Option<String>>(4)?.unwrap_or_default().trim()),
                        cwd: first_workspace(&r.get::<_, Option<String>>(5)?.unwrap_or_default()),
                    })
                })
                .ok()?
                .collect::<rusqlite::Result<Vec<_>>>()
                .ok()?;
            Some(rows)
        })
    }

    fn build_meta(&self, r: &SessionFileRef, row: &AgRow) -> SessionMeta {
        let title = Some(clean_title_candidate(&row.title))
            .filter(|t| !t.is_empty())
            .or_else(|| Some(clean_title_candidate(&row.preview)).filter(|t| !t.is_empty()))
            .unwrap_or_else(|| UNTITLED.to_string());
        // 库里只有 last_modified 一个时间,created/updated 同源
        let ts = if row.modified_ms > 0 {
            row.modified_ms
        } else {
            r.mtime_ms
        };
        SessionMeta {
            key: format!("antigravity:{}", row.id),
            host: String::new(),
            id: row.id.clone(),
            agent: AgentId::Antigravity,
            title,
            project_path: row.cwd.clone(),
            project_name: project_name_of(&row.cwd),
            file_path: r.file_path.clone(),
            created_at: ts,
            updated_at: ts,
            message_count: row.step_count,
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

    fn parse(&self, r: &SessionFileRef) -> Result<(SessionMeta, Vec<TranscriptMessage>)> {
        let rows = self
            .rows()
            .ok_or_else(|| anyhow!("cannot open antigravity summaries store"))?;
        let row = rows
            .iter()
            .find(|x| x.id == r.native_id)
            .ok_or_else(|| anyhow!("antigravity conversation {} not in store", r.native_id))?;

        // 正文加密不可读:一条 System 消息承载 preview,详情页与 FTS 都有着落
        let mut text = String::new();
        if !row.preview.trim().is_empty() {
            text.push_str(row.preview.trim());
            text.push_str("\n\n");
        }
        text.push_str("Antigravity stores conversation content encrypted — only this summary is available in Wake.");
        let mut messages = vec![text_msg(Role::System, &text, row.modified_ms)];
        assign_seq(&mut messages);
        Ok((self.build_meta(r, row), messages))
    }
}

#[derive(Clone)]
struct AgRow {
    id: String,
    title: String,
    preview: String,
    step_count: i64,
    modified_ms: i64,
    cwd: String,
}

/// workspace_uris JSON 数组("[\"file:///Users/…\"]")首项 → 本地路径
fn first_workspace(raw: &str) -> String {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(raw) else {
        return String::new();
    };
    let Some(uri) = v
        .as_array()
        .and_then(|a| a.first())
        .and_then(|x| x.as_str())
    else {
        return String::new();
    };
    let path = uri.strip_prefix("file://").unwrap_or(uri);
    percent_decode(path)
}

/// file:// URI 的最小 percent-decode(路径含空格/中文时是 %XX 编码)
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

impl AgentAdapter for AntigravityAdapter {
    fn agent(&self) -> AgentId {
        AgentId::Antigravity
    }

    fn list_session_files(&self) -> Result<Vec<SessionFileRef>> {
        let Some(rows) = self.rows() else {
            return Ok(Vec::new());
        };
        Ok(rows
            .into_iter()
            .map(|row| SessionFileRef {
                agent: AgentId::Antigravity,
                native_id: row.id.clone(),
                file_path: virtual_path(&self.db, &row.id),
                mtime_ms: row.modified_ms,
                // 正文不可读,标题/preview 长度即内容指纹(dirty 判断用)
                size: (row.title.len() + row.preview.len()) as i64,
            })
            .collect())
    }

    fn quick_meta(&self, refs: &[SessionFileRef]) -> Option<HashMap<String, SessionMeta>> {
        let rows = self.rows()?;
        let by_id: HashMap<&str, &AgRow> = rows.iter().map(|r| (r.id.as_str(), r)).collect();
        let mut out = HashMap::new();
        for r in refs {
            if let Some(row) = by_id.get(r.native_id.as_str()) {
                out.insert(r.file_path.clone(), self.build_meta(r, row));
            }
        }
        Some(out)
    }

    fn parse_session(&self, r: &SessionFileRef) -> Result<ParsedSession> {
        let (meta, messages) = self.parse(r)?;
        let units = units_from_messages(&messages);
        Ok(ParsedSession {
            meta,
            units,
            unknown_line_count: 0,
        })
    }

    fn parse_transcript(&self, r: &SessionFileRef) -> Result<ParsedTranscript> {
        let (meta, messages) = self.parse(r)?;
        Ok(ParsedTranscript {
            meta,
            mainline: messages,
            sidechains: Vec::new(),
            unknown_line_count: 0,
        })
    }

    fn with_custom_root(&self, dir: PathBuf) -> Box<dyn AgentAdapter> {
        // 选中 `~/.gemini` 形态(含 antigravity-cli/)、库所在目录,或直接
        // 给到库文件路径都认(Codex review)
        let nested = dir
            .join("antigravity-cli")
            .join("conversation_summaries.db");
        let db = if dir.is_file() {
            dir
        } else if nested.is_file() {
            nested
        } else {
            dir.join("conversation_summaries.db")
        };
        Box::new(Self {
            db,
            rows_cache: MtimeCache::new(),
        })
    }

    fn data_roots(&self) -> Vec<PathBuf> {
        vec![self.db.clone()]
    }
}
