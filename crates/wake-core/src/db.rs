use crate::models::*;
use anyhow::{Context as _, Result};
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

const DDL: &str = r#"
CREATE TABLE IF NOT EXISTS schema_meta (key TEXT PRIMARY KEY, value TEXT);

CREATE TABLE IF NOT EXISTS sessions (
  key            TEXT PRIMARY KEY,
  agent_id       TEXT NOT NULL,
  native_id      TEXT NOT NULL,
  title          TEXT NOT NULL DEFAULT '',
  project_path   TEXT NOT NULL DEFAULT '',
  project_name   TEXT NOT NULL DEFAULT '',
  git_branch     TEXT,
  created_at     INTEGER DEFAULT 0,
  updated_at     INTEGER DEFAULT 0,
  message_count  INTEGER DEFAULT 0,
  tokens_used    INTEGER,
  model          TEXT,
  source         TEXT,
  archived       INTEGER DEFAULT 0,
  file_path      TEXT NOT NULL UNIQUE,
  file_size      INTEGER DEFAULT 0,
  file_mtime     INTEGER DEFAULT 0,
  unknown_lines  INTEGER DEFAULT 0,
  parent_key     TEXT NOT NULL DEFAULT ''
);
CREATE INDEX IF NOT EXISTS idx_sessions_updated ON sessions(updated_at DESC);
CREATE INDEX IF NOT EXISTS idx_sessions_agent   ON sessions(agent_id, updated_at DESC);
CREATE INDEX IF NOT EXISTS idx_sessions_project ON sessions(project_path, updated_at DESC);

CREATE TABLE IF NOT EXISTS messages (
  id           INTEGER PRIMARY KEY,
  session_key  TEXT NOT NULL,
  sidechain_id TEXT,
  seq          INTEGER NOT NULL,
  role         TEXT, ts INTEGER,
  text         TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_messages_session ON messages(session_key);

CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(
  text,
  content='messages', content_rowid='id',
  tokenize="trigram case_sensitive 0"
);

CREATE TABLE IF NOT EXISTS user_data (
  session_key TEXT PRIMARY KEY,
  favorite    INTEGER DEFAULT 0,
  pinned      INTEGER DEFAULT 0,
  updated_at  INTEGER
);

CREATE TABLE IF NOT EXISTS tombstones (
  file_path  TEXT PRIMARY KEY,
  deleted_at INTEGER
);
"#;

fn open_conn(path: &Path) -> Result<Connection> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let conn = Connection::open(path)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.busy_timeout(std::time::Duration::from_millis(3000))?;
    conn.execute_batch(DDL)
        .context("failed to initialize SQLite schema")?;
    migrate_sessions(&conn)?;
    Ok(conn)
}

/// 旧库 CREATE TABLE IF NOT EXISTS 不会加新列,这里补 parent_key
fn migrate_sessions(conn: &Connection) -> Result<()> {
    let mut has_parent = false;
    let mut stmt = conn.prepare("PRAGMA table_info(sessions)")?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(1))?;
    for name in rows.flatten() {
        if name == "parent_key" {
            has_parent = true;
            break;
        }
    }
    if !has_parent {
        conn.execute(
            "ALTER TABLE sessions ADD COLUMN parent_key TEXT NOT NULL DEFAULT ''",
            [],
        )?;
    }
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_sessions_parent ON sessions(parent_key)",
        [],
    )?;
    Ok(())
}

/// 打开索引库;打不开就把它连同 WAL/SHM 一起挪到 `.corrupt` 旁路再建一个空的。
/// 索引本来就能从磁盘全量重扫恢复,重建的真实损失只有 user_data(收藏/置顶)——
/// 而它远好过 GUI 无提示秒退。返回的 `Some(_)` 是给用户看的说明文案。
pub fn open_or_rebuild(path: &Path) -> Result<(Store, Option<String>)> {
    let first = match Store::open(path) {
        Ok(store) => return Ok((store, None)),
        Err(e) => e,
    };
    // 三件套一起挪:留下 WAL 或 SHM 任何一个,新库都会接着读旧日志
    let backup = std::path::PathBuf::from(format!("{}.corrupt", path.display()));
    let _ = std::fs::remove_file(&backup);
    let _ = std::fs::rename(path, &backup);
    for suffix in ["-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
    }
    let store = Store::open(path).with_context(|| format!("rebuild failed after: {first}"))?;
    Ok((
        store,
        Some(format!(
            "Index was damaged and has been rebuilt — stars and pins are gone. \
             The old file is kept at {}",
            backup.display()
        )),
    ))
}

/// 读写分连接(WAL 单写多读);Connection 非 Sync,各自套 Mutex
pub struct Store {
    write: Mutex<Connection>,
    read: Mutex<Connection>,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        Ok(Self {
            write: Mutex::new(open_conn(path)?),
            read: Mutex::new(open_conn(path)?),
        })
    }

    // ---------- 写路径(扫描器/用户操作) ----------

    pub fn write_session(
        &self,
        meta: &SessionMeta,
        file_mtime: i64,
        units: &[IndexUnit],
    ) -> Result<()> {
        let mut conn = self.write.lock().unwrap();
        let tx = conn.transaction()?;
        {
            // FTS external content 需要显式 delete 旧行
            let mut sel =
                tx.prepare_cached("SELECT id, text FROM messages WHERE session_key = ?1")?;
            let rows: Vec<(i64, String)> = sel
                .query_map(params![meta.key], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?;
            let mut fts_del = tx.prepare_cached(
                "INSERT INTO messages_fts(messages_fts, rowid, text) VALUES ('delete', ?1, ?2)",
            )?;
            for (id, text) in rows {
                fts_del.execute(params![id, text])?;
            }
            tx.execute(
                "DELETE FROM messages WHERE session_key = ?1",
                params![meta.key],
            )?;

            upsert_session(&tx, meta, file_mtime)?;

            let mut ins_msg = tx.prepare_cached(
                "INSERT INTO messages(session_key, sidechain_id, seq, role, ts, text) VALUES (?1,?2,?3,?4,?5,?6)",
            )?;
            let mut ins_fts =
                tx.prepare_cached("INSERT INTO messages_fts(rowid, text) VALUES (?1, ?2)")?;
            for u in units {
                ins_msg.execute(params![
                    meta.key,
                    u.sidechain_id,
                    u.seq,
                    u.role.as_str(),
                    u.timestamp,
                    u.text
                ])?;
                let rowid = tx.last_insert_rowid();
                ins_fts.execute(params![rowid, u.text])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn write_meta_only(&self, metas: &[(SessionMeta, i64)]) -> Result<()> {
        let mut conn = self.write.lock().unwrap();
        let tx = conn.transaction()?;
        for (meta, mtime) in metas {
            upsert_session(&tx, meta, *mtime)?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn remove_session(&self, key: &str, tombstone: bool) -> Result<()> {
        let mut conn = self.write.lock().unwrap();
        let tx = conn.transaction()?;
        {
            let file_path: Option<String> = tx
                .query_row(
                    "SELECT file_path FROM sessions WHERE key = ?1",
                    params![key],
                    |r| r.get(0),
                )
                .optional()?;
            let mut sel =
                tx.prepare_cached("SELECT id, text FROM messages WHERE session_key = ?1")?;
            let rows: Vec<(i64, String)> = sel
                .query_map(params![key], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?;
            let mut fts_del = tx.prepare_cached(
                "INSERT INTO messages_fts(messages_fts, rowid, text) VALUES ('delete', ?1, ?2)",
            )?;
            for (id, text) in rows {
                fts_del.execute(params![id, text])?;
            }
            tx.execute("DELETE FROM messages WHERE session_key = ?1", params![key])?;
            tx.execute("DELETE FROM sessions WHERE key = ?1", params![key])?;
            if tombstone {
                if let Some(fp) = file_path {
                    tx.execute(
                        "INSERT OR REPLACE INTO tombstones(file_path, deleted_at) VALUES (?1, ?2)",
                        params![fp, now_ms()],
                    )?;
                }
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn remove_by_path(&self, file_path: &str) -> Result<()> {
        let key: Option<String> = {
            let conn = self.read.lock().unwrap();
            conn.query_row(
                "SELECT key FROM sessions WHERE file_path = ?1",
                params![file_path],
                |r| r.get(0),
            )
            .optional()?
        };
        if let Some(k) = key {
            self.remove_session(&k, false)?;
        }
        Ok(())
    }

    pub fn set_user_data(
        &self,
        key: &str,
        favorite: Option<bool>,
        pinned: Option<bool>,
    ) -> Result<()> {
        let conn = self.write.lock().unwrap();
        conn.execute(
            "INSERT INTO user_data(session_key, favorite, pinned, updated_at)
             VALUES (?1, COALESCE(?2, 0), COALESCE(?3, 0), ?4)
             ON CONFLICT(session_key) DO UPDATE SET
               favorite = COALESCE(?2, user_data.favorite),
               pinned   = COALESCE(?3, user_data.pinned),
               updated_at = excluded.updated_at",
            params![
                key,
                favorite.map(|v| v as i64),
                pinned.map(|v| v as i64),
                now_ms()
            ],
        )?;
        Ok(())
    }

    pub fn rebuild_all(&self) -> Result<()> {
        let conn = self.write.lock().unwrap();
        conn.execute_batch(
            "DELETE FROM messages; DELETE FROM messages_fts; DELETE FROM sessions;",
        )?;
        Ok(())
    }

    // ---------- 读路径(UI 查询) ----------

    pub fn known_files(&self) -> Result<HashMap<String, (i64, i64, String)>> {
        let conn = self.read.lock().unwrap();
        let mut stmt =
            conn.prepare_cached("SELECT file_path, file_mtime, file_size, key FROM sessions")?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, (r.get(1)?, r.get(2)?, r.get(3)?)))
        })?;
        let mut map = HashMap::new();
        for row in rows {
            let (path, v) = row?;
            map.insert(path, v);
        }
        Ok(map)
    }

    pub fn is_tombstoned(&self, file_path: &str) -> bool {
        let conn = self.read.lock().unwrap();
        conn.query_row(
            "SELECT 1 FROM tombstones WHERE file_path = ?1",
            params![file_path],
            |_| Ok(()),
        )
        .optional()
        .map(|o| o.is_some())
        .unwrap_or(false)
    }

    pub fn list_sessions(&self, f: &SessionFilter) -> Result<(Vec<SessionMeta>, i64)> {
        let mut wheres: Vec<String> = Vec::new();
        let mut args: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

        if !f.agents.is_empty() {
            let ph = f.agents.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            wheres.push(format!("s.agent_id IN ({ph})"));
            for a in &f.agents {
                args.push(Box::new(a.as_str().to_string()));
            }
        }
        if let Some(p) = &f.project_path {
            wheres.push("s.project_path = ?".into());
            args.push(Box::new(p.clone()));
        }
        if f.favorite_only {
            wheres.push("COALESCE(u.favorite, 0) = 1".into());
        }
        if !f.include_archived {
            wheres.push("s.archived = 0".into());
        }
        if let Some(q) = f.title_query.as_deref().filter(|q| !q.trim().is_empty()) {
            wheres.push("(s.title LIKE ? ESCAPE '\\' OR s.project_name LIKE ? ESCAPE '\\')".into());
            let like = format!("%{}%", escape_like(q.trim()));
            args.push(Box::new(like.clone()));
            args.push(Box::new(like));
        }
        if f.roots_only {
            if f.include_archived {
                wheres.push(ROOT_IGNORING_ARCHIVED.into());
            } else {
                wheres.push(ROOT_WHEN_HIDING_ARCHIVED.into());
            }
        }
        let where_sql = if wheres.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", wheres.join(" AND "))
        };
        let order_col = if f.roots_only {
            match f.sort {
                SortKey::Updated => {
                    "MAX(s.updated_at, COALESCE((SELECT MAX(c.updated_at) FROM sessions c WHERE c.parent_key = s.key), 0))"
                }
                SortKey::Created => "s.created_at",
                SortKey::Messages => {
                    "s.message_count + COALESCE((SELECT SUM(c.message_count) FROM sessions c WHERE c.parent_key = s.key), 0)"
                }
            }
        } else {
            match f.sort {
                SortKey::Updated => "s.updated_at",
                SortKey::Created => "s.created_at",
                SortKey::Messages => "s.message_count",
            }
        };
        let order_dir = if f.ascending { "ASC" } else { "DESC" };

        let conn = self.read.lock().unwrap();
        let total: i64 = conn.query_row(
            &format!(
                "SELECT COUNT(*) FROM sessions s LEFT JOIN user_data u ON u.session_key = s.key {where_sql}"
            ),
            rusqlite::params_from_iter(args.iter().map(|b| b.as_ref())),
            |r| r.get(0),
        )?;

        let sql = format!(
            "SELECT {SESSION_COLS} FROM sessions s LEFT JOIN user_data u ON u.session_key = s.key
             {where_sql}
             ORDER BY COALESCE(u.pinned,0) DESC, {order_col} {order_dir} LIMIT ? OFFSET ?"
        );
        let mut stmt = conn.prepare_cached(&sql)?;
        let limit = if f.limit > 0 { f.limit } else { 500 };
        args.push(Box::new(limit));
        args.push(Box::new(f.offset));
        let rows = stmt.query_map(
            rusqlite::params_from_iter(args.iter().map(|b| b.as_ref())),
            row_to_meta,
        )?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok((out, total))
    }

    pub fn get_session(&self, key: &str) -> Result<Option<SessionMeta>> {
        let conn = self.read.lock().unwrap();
        let sql = format!(
            "SELECT {SESSION_COLS} FROM sessions s LEFT JOIN user_data u ON u.session_key = s.key WHERE s.key = ?1"
        );
        Ok(conn.query_row(&sql, params![key], row_to_meta).optional()?)
    }

    /// 旁写父子链。只写差异,避免 watcher 每批空转 WAL。返回是否有变化。
    pub fn apply_parent_links(&self, agent: AgentId, links: &[(String, String)]) -> Result<bool> {
        let desired: HashMap<String, String> = links.iter().cloned().collect();
        let mut conn = self.write.lock().unwrap();
        let existing: HashMap<String, String> = {
            let mut stmt = conn.prepare(
                "SELECT key, parent_key FROM sessions WHERE agent_id = ?1 AND parent_key != ''",
            )?;
            let rows = stmt.query_map(params![agent.as_str()], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?;
            let mut map = HashMap::new();
            for row in rows {
                let (k, v) = row?;
                map.insert(k, v);
            }
            map
        };
        let mut changed = false;
        let tx = conn.transaction()?;
        for (key, pk) in &existing {
            if desired.get(key) != Some(pk) {
                tx.execute(
                    "UPDATE sessions SET parent_key = '' WHERE key = ?1",
                    params![key],
                )?;
                changed = true;
            }
        }
        for (key, pk) in &desired {
            if existing.get(key) != Some(pk) {
                let n = tx.execute(
                    "UPDATE sessions SET parent_key = ?1 WHERE key = ?2 AND agent_id = ?3",
                    params![pk, key, agent.as_str()],
                )?;
                if n > 0 {
                    changed = true;
                }
            }
        }
        tx.commit()?;
        Ok(changed)
    }

    pub fn list_children(
        &self,
        parent_key: &str,
        sort: SortKey,
        ascending: bool,
    ) -> Result<Vec<SessionMeta>> {
        let order_col = match sort {
            SortKey::Updated => "s.updated_at",
            SortKey::Created => "s.created_at",
            SortKey::Messages => "s.message_count",
        };
        let order_dir = if ascending { "ASC" } else { "DESC" };
        let conn = self.read.lock().unwrap();
        let sql = format!(
            "SELECT {SESSION_COLS} FROM sessions s LEFT JOIN user_data u ON u.session_key = s.key
             WHERE s.parent_key = ?1 AND s.archived = 0
             ORDER BY COALESCE(u.pinned,0) DESC, {order_col} {order_dir}"
        );
        let mut stmt = conn.prepare_cached(&sql)?;
        let rows = stmt.query_map(params![parent_key], row_to_meta)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// 某会话下全部子会话(含 archived),删除主会话时一并带走。
    pub fn all_children(&self, parent_key: &str) -> Result<Vec<SessionMeta>> {
        let conn = self.read.lock().unwrap();
        let sql = format!(
            "SELECT {SESSION_COLS} FROM sessions s LEFT JOIN user_data u ON u.session_key = s.key
             WHERE s.parent_key = ?1"
        );
        let mut stmt = conn.prepare_cached(&sql)?;
        let rows = stmt.query_map(params![parent_key], row_to_meta)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn child_counts(&self) -> Result<HashMap<String, i64>> {
        let conn = self.read.lock().unwrap();
        let mut stmt = conn.prepare_cached(
            "SELECT parent_key, COUNT(*) FROM sessions WHERE parent_key != '' AND archived = 0 GROUP BY parent_key",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
        let mut map = HashMap::new();
        for row in rows {
            let (k, n) = row?;
            map.insert(k, n);
        }
        Ok(map)
    }

    pub fn parent_key_of(&self, key: &str) -> Result<Option<String>> {
        let conn = self.read.lock().unwrap();
        let raw: Option<String> = conn
            .query_row(
                "SELECT parent_key FROM sessions WHERE key = ?1",
                params![key],
                |r| r.get(0),
            )
            .optional()?;
        Ok(raw.filter(|s| !s.is_empty()))
    }

    pub fn list_projects(&self) -> Result<Vec<ProjectInfo>> {
        let conn = self.read.lock().unwrap();
        // 个数只数顶层(与点开后列表一致);last_active 仍看整组,子会话活动会把项目顶上来
        let mut stmt = conn.prepare_cached(&format!(
            "SELECT s.project_path, s.project_name,
                    SUM(CASE WHEN {ROOT_WHEN_HIDING_ARCHIVED} THEN 1 ELSE 0 END),
                    MAX(s.updated_at)
             FROM sessions s WHERE s.archived = 0
             GROUP BY s.project_path ORDER BY MAX(s.updated_at) DESC",
        ))?;
        let rows = stmt.query_map([], |r| {
            Ok(ProjectInfo {
                path: r.get(0)?,
                name: r.get(1)?,
                session_count: r.get(2)?,
                last_active: r.get(3)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn starred_count(&self) -> Result<i64> {
        let conn = self.read.lock().unwrap();
        Ok(conn.query_row(
            // 收藏是扁平列表(含子会话),徽标数 = 点开后可见数
            "SELECT COUNT(*) FROM user_data u JOIN sessions s ON s.key = u.session_key WHERE u.favorite = 1 AND s.archived = 0",
            [],
            |r| r.get(0),
        )?)
    }

    pub fn agent_counts(&self) -> Result<HashMap<String, i64>> {
        let conn = self.read.lock().unwrap();
        let mut stmt = conn.prepare_cached(&format!(
            "SELECT s.agent_id, COUNT(*) FROM sessions s
             WHERE s.archived = 0 AND {ROOT_WHEN_HIDING_ARCHIVED}
             GROUP BY s.agent_id",
        ))?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
        let mut map = HashMap::new();
        for r in rows {
            let (k, v) = r?;
            map.insert(k, v);
        }
        Ok(map)
    }

    /// 全文搜索:trigram MATCH(每段 ≥3 码点)或 LIKE 降级。返回 (hits, degraded)
    pub fn search(
        &self,
        q: &str,
        agents: &[AgentId],
        project_path: Option<&str>,
        limit: i64,
    ) -> Result<(Vec<SearchHit>, bool)> {
        let segs: Vec<&str> = q.split_whitespace().filter(|s| !s.is_empty()).collect();
        if segs.is_empty() {
            return Ok((Vec::new(), false));
        }
        let degraded = segs.iter().any(|s| s.chars().count() < 3);

        let mut filter_sql = String::new();
        let mut filter_args: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        if !agents.is_empty() {
            let ph = agents.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            filter_sql.push_str(&format!(" AND s.agent_id IN ({ph})"));
            for a in agents {
                filter_args.push(Box::new(a.as_str().to_string()));
            }
        }
        if let Some(p) = project_path {
            filter_sql.push_str(" AND s.project_path = ?");
            filter_args.push(Box::new(p.to_string()));
        }

        let conn = self.read.lock().unwrap();
        let mut raw: Vec<(String, i64, Option<String>, String, Option<i64>, String)> = Vec::new();

        if !degraded {
            let match_expr = segs
                .iter()
                .map(|s| format!("\"{}\"", s.replace('"', "\"\"")))
                .collect::<Vec<_>>()
                .join(" AND ");
            let sql = format!(
                "SELECT m.session_key, m.seq, m.sidechain_id, m.role, m.ts,
                        snippet(messages_fts, 0, ?, ?, '…', 16)
                 FROM messages_fts
                 JOIN messages m ON m.id = messages_fts.rowid
                 JOIN sessions s ON s.key = m.session_key
                 WHERE messages_fts MATCH ?{filter_sql}
                 ORDER BY bm25(messages_fts) LIMIT ?"
            );
            let mut stmt = conn.prepare_cached(&sql)?;
            let mut all_args: Vec<Box<dyn rusqlite::ToSql>> = vec![
                Box::new(HL_OPEN.to_string()),
                Box::new(HL_CLOSE.to_string()),
                Box::new(match_expr),
            ];
            all_args.extend(filter_args);
            all_args.push(Box::new(limit));
            let rows = stmt.query_map(
                rusqlite::params_from_iter(all_args.iter().map(|b| b.as_ref())),
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                    ))
                },
            )?;
            for r in rows {
                raw.push(r?);
            }
        } else {
            let like_where = segs
                .iter()
                .map(|_| "m.text LIKE ? ESCAPE '\\'")
                .collect::<Vec<_>>()
                .join(" AND ");
            let sql = format!(
                "SELECT m.session_key, m.seq, m.sidechain_id, m.role, m.ts, m.text
                 FROM messages m JOIN sessions s ON s.key = m.session_key
                 WHERE {like_where}{filter_sql}
                 ORDER BY m.ts DESC LIMIT ?"
            );
            let mut stmt = conn.prepare_cached(&sql)?;
            let mut all_args: Vec<Box<dyn rusqlite::ToSql>> = segs
                .iter()
                .map(|s| Box::new(format!("%{}%", escape_like(s))) as Box<dyn rusqlite::ToSql>)
                .collect();
            all_args.extend(filter_args);
            all_args.push(Box::new(limit));
            let rows = stmt.query_map(
                rusqlite::params_from_iter(all_args.iter().map(|b| b.as_ref())),
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                    ))
                },
            )?;
            for r in rows {
                let (k, seq, sc, role, ts, text): (
                    String,
                    i64,
                    Option<String>,
                    String,
                    Option<i64>,
                    String,
                ) = r?;
                raw.push((k, seq, sc, role, ts, make_like_snippet(&text, segs[0])));
            }
        }

        // 补齐 session meta
        let mut hits = Vec::new();
        let sql = format!(
            "SELECT {SESSION_COLS} FROM sessions s LEFT JOIN user_data u ON u.session_key = s.key WHERE s.key = ?1"
        );
        for (key, seq, sidechain_id, role, ts, snippet) in raw {
            if let Some(session) = conn.query_row(&sql, params![key], row_to_meta).optional()? {
                hits.push(SearchHit {
                    session,
                    seq,
                    sidechain_id,
                    role,
                    snippet,
                    timestamp: ts,
                });
            }
        }
        Ok((hits, degraded))
    }
}

/// 顶层行:无父,或父不在未归档集合里(orphan 仍可见)。侧栏计数与 roots_only 列表同口径。
const ROOT_WHEN_HIDING_ARCHIVED: &str = "(s.parent_key = '' OR NOT EXISTS (SELECT 1 FROM sessions p WHERE p.key = s.parent_key AND p.archived = 0))";
const ROOT_IGNORING_ARCHIVED: &str =
    "(s.parent_key = '' OR NOT EXISTS (SELECT 1 FROM sessions p WHERE p.key = s.parent_key))";

const SESSION_COLS: &str =
    "s.key, s.agent_id, s.native_id, s.title, s.project_path, s.project_name,
    s.git_branch, s.created_at, s.updated_at, s.message_count, s.tokens_used, s.model, s.source,
    s.archived, s.file_path, s.file_size, COALESCE(u.favorite,0), COALESCE(u.pinned,0)";

fn row_to_meta(r: &rusqlite::Row<'_>) -> rusqlite::Result<SessionMeta> {
    let agent_str: String = r.get(1)?;
    Ok(SessionMeta {
        key: r.get(0)?,
        agent: AgentId::from_str(&agent_str).unwrap_or(AgentId::ClaudeCode),
        id: r.get(2)?,
        title: r.get(3)?,
        project_path: r.get(4)?,
        project_name: r.get(5)?,
        git_branch: r.get(6)?,
        created_at: r.get(7)?,
        updated_at: r.get(8)?,
        message_count: r.get(9)?,
        tokens_used: r.get(10)?,
        model: r.get(11)?,
        source: r.get(12)?,
        archived: r.get::<_, i64>(13)? == 1,
        file_path: r.get(14)?,
        size_bytes: r.get(15)?,
        favorite: r.get::<_, i64>(16)? == 1,
        pinned: r.get::<_, i64>(17)? == 1,
    })
}

fn upsert_session(tx: &rusqlite::Transaction<'_>, m: &SessionMeta, file_mtime: i64) -> Result<()> {
    tx.execute(
        "INSERT INTO sessions(key, agent_id, native_id, title, project_path, project_name,
           git_branch, created_at, updated_at, message_count, tokens_used, model, source,
           archived, file_path, file_size, file_mtime, unknown_lines)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,0)
         ON CONFLICT(key) DO UPDATE SET
           title=excluded.title, project_path=excluded.project_path,
           project_name=excluded.project_name, git_branch=excluded.git_branch,
           created_at=excluded.created_at, updated_at=excluded.updated_at,
           message_count=excluded.message_count, tokens_used=excluded.tokens_used,
           model=excluded.model, source=excluded.source, archived=excluded.archived,
           file_path=excluded.file_path, file_size=excluded.file_size,
           file_mtime=excluded.file_mtime",
        params![
            m.key,
            m.agent.as_str(),
            m.id,
            m.title,
            m.project_path,
            m.project_name,
            m.git_branch,
            m.created_at,
            m.updated_at,
            m.message_count,
            m.tokens_used,
            m.model,
            m.source,
            m.archived as i64,
            m.file_path,
            m.size_bytes,
            file_mtime,
        ],
    )?;
    Ok(())
}

fn escape_like(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

fn make_like_snippet(text: &str, first_seg: &str) -> String {
    let lower = text.to_lowercase();
    let seg_lower = first_seg.to_lowercase();
    let Some(byte_idx) = lower.find(&seg_lower) else {
        return text.chars().take(120).collect();
    };
    // 定位到字符边界安全的窗口
    let chars: Vec<char> = text.chars().collect();
    let char_idx = text[..byte_idx].chars().count();
    let seg_len = first_seg.chars().count();
    let start = char_idx.saturating_sub(40);
    let end = (char_idx + seg_len + 80).min(chars.len());
    let mut out = String::new();
    if start > 0 {
        out.push('…');
    }
    out.extend(&chars[start..char_idx]);
    out.push(HL_OPEN);
    out.extend(&chars[char_idx..(char_idx + seg_len).min(chars.len())]);
    out.push(HL_CLOSE);
    out.extend(&chars[(char_idx + seg_len).min(chars.len())..end]);
    if end < chars.len() {
        out.push('…');
    }
    out
}

pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// 索引库路径:~/Library/Application Support/wake/wake.db
/// (从旧 vibex 路径一次性迁移,保留收藏等 user_data)
pub fn default_db_path() -> std::path::PathBuf {
    let data = dirs::data_dir().unwrap_or_else(|| std::path::PathBuf::from("."));
    let dir = data.join("wake");
    let db = dir.join("wake.db");
    if !db.exists() {
        let old_db = data.join("vibex").join("vibex-rs.db");
        if old_db.exists() {
            let _ = std::fs::create_dir_all(&dir);
            for suffix in ["", "-wal", "-shm"] {
                let src = data.join("vibex").join(format!("vibex-rs.db{suffix}"));
                if src.exists() {
                    let _ = std::fs::copy(&src, dir.join(format!("wake.db{suffix}")));
                }
            }
        }
    }
    db
}
