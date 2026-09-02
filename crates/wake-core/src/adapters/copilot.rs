use super::parse_utils::*;
use super::sqlite_ro::{open_sqlite_ro, virtual_path};
use super::{units_from_messages, AgentAdapter};
use crate::models::*;
use anyhow::{anyhow, Result};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

/// Copilot CLI:`~/.copilot/session-store.db` 全明文 SQLite。
/// sessions(id,cwd,branch,summary) + turns(user_message,assistant_response)。
/// 无每会话文件,SessionFileRef 用虚拟路径,mtime 取行内 updated_at、
/// size 取 turns 正文总长,二者共同构成增量扫描的 dirty 判断。
pub struct CopilotAdapter {
    db: PathBuf,
    /// rows() 的全表聚合较贵,按 db mtime 缓存一轮扫描内的重复调用
    /// (list_session_files 与 quick_meta 各调一次)
    rows_cache: Mutex<Option<(i64, Vec<CopilotRow>)>>,
}

impl CopilotAdapter {
    pub fn new() -> Self {
        Self {
            db: super::home_dir()
                .unwrap_or_default()
                .join(".copilot")
                .join("session-store.db"),
            rows_cache: Mutex::new(None),
        }
    }

    fn db_mtime(&self) -> i64 {
        super::sqlite_ro::db_cache_stamp(&self.db)
    }

    fn rows(&self) -> Option<Vec<CopilotRow>> {
        let mtime = self.db_mtime();
        {
            let cache = self.rows_cache.lock().unwrap();
            if let Some((t, rows)) = cache.as_ref() {
                if *t == mtime {
                    return Some(rows.clone());
                }
            }
        }
        let ro = open_sqlite_ro(&self.db, "copilot")?;
        let mut stmt = ro
            .conn
            .prepare(
                "SELECT s.id, s.cwd, s.branch, s.summary, s.created_at, s.updated_at,
                        COALESCE(SUM(LENGTH(COALESCE(t.user_message,'')) + LENGTH(COALESCE(t.assistant_response,''))), 0),
                        COUNT(t.id)
                 FROM sessions s LEFT JOIN turns t ON t.session_id = s.id
                 GROUP BY s.id",
            )
            .ok()?;
        let rows = stmt
            .query_map([], |r| {
                Ok(CopilotRow {
                    id: r.get(0)?,
                    cwd: r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                    branch: r.get(2)?,
                    summary: r.get::<_, Option<String>>(3)?.unwrap_or_default(),
                    created_ms: sqlite_dt_ms(&r.get::<_, Option<String>>(4)?.unwrap_or_default()),
                    updated_ms: sqlite_dt_ms(&r.get::<_, Option<String>>(5)?.unwrap_or_default()),
                    content_len: r.get(6)?,
                    turn_count: r.get(7)?,
                })
            })
            .ok()?
            .collect::<rusqlite::Result<Vec<_>>>()
            .ok()?;
        *self.rows_cache.lock().unwrap() = Some((mtime, rows.clone()));
        Some(rows)
    }

    fn build_meta(&self, r: &SessionFileRef, row: &CopilotRow, message_count: i64) -> SessionMeta {
        let title = clean_title_candidate(&row.summary);
        SessionMeta {
            key: format!("copilot:{}", row.id),
            host: String::new(),
            id: row.id.clone(),
            agent: AgentId::Copilot,
            title: if title.is_empty() {
                UNTITLED.to_string()
            } else {
                title
            },
            project_path: row.cwd.clone(),
            project_name: project_name_of(&row.cwd),
            file_path: r.file_path.clone(),
            created_at: if row.created_ms > 0 {
                row.created_ms
            } else {
                r.mtime_ms
            },
            updated_at: if row.updated_ms > 0 {
                row.updated_ms
            } else {
                r.mtime_ms
            },
            message_count,
            size_bytes: r.size,
            git_branch: row.branch.clone().filter(|b| !b.is_empty()),
            model: None,
            tokens_used: None,
            archived: false,
            source: None,
            favorite: false,
            pinned: false,
        }
    }

    /// 单会话解析:一次连接,会话行与 turns 都只查本会话(不做全表聚合)
    fn parse(&self, r: &SessionFileRef) -> Result<(SessionMeta, Vec<TranscriptMessage>)> {
        let ro = open_sqlite_ro(&self.db, "copilot")
            .ok_or_else(|| anyhow!("cannot open copilot store"))?;
        let row = ro
            .conn
            .query_row(
                "SELECT s.id, s.cwd, s.branch, s.summary, s.created_at, s.updated_at,
                        COALESCE(SUM(LENGTH(COALESCE(t.user_message,'')) + LENGTH(COALESCE(t.assistant_response,''))), 0),
                        COUNT(t.id)
                 FROM sessions s LEFT JOIN turns t ON t.session_id = s.id
                 WHERE s.id = ?1 GROUP BY s.id",
                [&r.native_id],
                |x| {
                    Ok(CopilotRow {
                        id: x.get(0)?,
                        cwd: x.get::<_, Option<String>>(1)?.unwrap_or_default(),
                        branch: x.get(2)?,
                        summary: x.get::<_, Option<String>>(3)?.unwrap_or_default(),
                        created_ms: sqlite_dt_ms(&x.get::<_, Option<String>>(4)?.unwrap_or_default()),
                        updated_ms: sqlite_dt_ms(&x.get::<_, Option<String>>(5)?.unwrap_or_default()),
                        content_len: x.get(6)?,
                        turn_count: x.get(7)?,
                    })
                },
            )
            .map_err(|_| anyhow!("copilot session {} not in store", r.native_id))?;

        let mut stmt = ro.conn.prepare(
            "SELECT user_message, assistant_response, timestamp
             FROM turns WHERE session_id = ?1 ORDER BY turn_index",
        )?;
        let turns = stmt.query_map([&r.native_id], |t| {
            Ok((
                t.get::<_, Option<String>>(0)?,
                t.get::<_, Option<String>>(1)?,
                t.get::<_, Option<String>>(2)?,
            ))
        })?;

        let mut messages: Vec<TranscriptMessage> = Vec::new();
        for turn in turns.flatten() {
            let ts = sqlite_dt_ms(&turn.2.unwrap_or_default());
            if let Some(u) = turn.0.filter(|s| !s.trim().is_empty()) {
                messages.push(text_msg(Role::User, &u, ts));
            }
            if let Some(a) = turn.1.filter(|s| !s.trim().is_empty()) {
                messages.push(text_msg(Role::Assistant, &a, ts));
            }
        }
        assign_seq(&mut messages);
        let count = messages
            .iter()
            .filter(|m| m.kind == MessageKind::Text)
            .count() as i64;
        let mut meta = self.build_meta(r, &row, count);
        // summary 缺失时回退首条用户消息
        if meta.title == UNTITLED {
            if let Some(t) = title_from_messages(&messages) {
                meta.title = t;
            }
        }
        Ok((meta, messages))
    }
}

#[derive(Clone)]
struct CopilotRow {
    id: String,
    cwd: String,
    branch: Option<String>,
    summary: String,
    created_ms: i64,
    updated_ms: i64,
    content_len: i64,
    turn_count: i64,
}

impl AgentAdapter for CopilotAdapter {
    fn agent(&self) -> AgentId {
        AgentId::Copilot
    }

    fn list_session_files(&self) -> Result<Vec<SessionFileRef>> {
        let Some(rows) = self.rows() else {
            return Ok(Vec::new());
        };
        Ok(rows
            .into_iter()
            .filter(|row| row.turn_count > 0)
            .map(|row| SessionFileRef {
                agent: AgentId::Copilot,
                native_id: row.id.clone(),
                file_path: virtual_path(&self.db, &row.id),
                mtime_ms: row.updated_ms,
                size: row.content_len,
            })
            .collect())
    }

    fn quick_meta(&self, refs: &[SessionFileRef]) -> Option<HashMap<String, SessionMeta>> {
        let rows = self.rows()?;
        let by_id: HashMap<&str, &CopilotRow> = rows.iter().map(|r| (r.id.as_str(), r)).collect();
        let mut out = HashMap::new();
        for r in refs {
            if let Some(row) = by_id.get(r.native_id.as_str()) {
                out.insert(r.file_path.clone(), self.build_meta(r, row, 0));
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
        // 手输/预填的就是库文件路径时直接认,不再往下拼(Codex review)
        let db = if dir.is_file() {
            dir
        } else {
            dir.join("session-store.db")
        };
        Box::new(Self {
            db,
            rows_cache: Mutex::new(None),
        })
    }

    fn data_roots(&self) -> Vec<PathBuf> {
        vec![self.db.clone()]
    }
}
