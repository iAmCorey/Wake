use crate::models::*;
use anyhow::{Context as _, Result};
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

pub type LocationOverrides = (
    Vec<(AgentId, std::path::PathBuf)>,
    Vec<AgentId>,
    Vec<(AgentId, std::path::PathBuf)>,
);

/// remote_hosts 表的一行(Settings → Remote hosts / roster 组装共用)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteHost {
    pub name: String,
    pub enabled: bool,
    /// epoch ms;None = 从未成功同步过
    pub last_sync_at: Option<i64>,
    pub last_sync_error: Option<String>,
}

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
  parent_key     TEXT NOT NULL DEFAULT '',
  host           TEXT NOT NULL DEFAULT ''
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
  key        TEXT,
  deleted_at INTEGER
);

CREATE TABLE IF NOT EXISTS custom_roots (
  agent    TEXT NOT NULL,
  path     TEXT NOT NULL,
  added_at INTEGER,
  PRIMARY KEY (agent, path)
);

CREATE TABLE IF NOT EXISTS removed_defaults (
  agent      TEXT PRIMARY KEY,
  removed_at INTEGER
);

CREATE TABLE IF NOT EXISTS removed_default_roots (
  agent      TEXT NOT NULL,
  path       TEXT NOT NULL,
  removed_at INTEGER,
  PRIMARY KEY (agent, path)
);

-- 应用级 UI 偏好。与 schema_meta 同形但语义不同:schema_meta 是索引
-- 自身的迁移状态,迁移代码可随意增删;prefs 是用户数据(user_data 同类,
-- 只是不挂在会话上),勿合并两表
CREATE TABLE IF NOT EXISTS prefs (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS disabled_locations (
  agent       TEXT NOT NULL,
  path        TEXT NOT NULL,
  disabled_at INTEGER,
  PRIMARY KEY (agent, path)
);

CREATE TABLE IF NOT EXISTS remote_hosts (
  name            TEXT PRIMARY KEY,
  enabled         INTEGER DEFAULT 1,
  added_at        INTEGER,
  last_sync_at    INTEGER,
  last_sync_error TEXT
);
"#;

fn open_conn(path: &Path) -> Result<Connection> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut conn = Connection::open(path)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.busy_timeout(std::time::Duration::from_millis(3000))?;
    let sessions_existed: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='sessions')",
        [],
        |row| row.get(0),
    )?;
    conn.execute_batch(DDL)
        .context("failed to initialize SQLite schema")?;
    // tombstones.key 迁移(2026-08-24 加列,老库无此列;重复加列报错即忽略):
    // 墓碑按逻辑会话(key)+物理路径双轨屏蔽,多 location 副本不得复活已删会话
    let _ = conn.execute("ALTER TABLE tombstones ADD COLUMN key TEXT", []);
    if !table_has_column(&conn, "sessions", "parent_key")? {
        let tx = conn.transaction()?;
        tx.execute(
            "ALTER TABLE sessions ADD COLUMN parent_key TEXT NOT NULL DEFAULT ''",
            [],
        )?;
        // 只有从旧 schema 升级时才需要强制重解析 Grok；新库第一次增量扫描
        // 本来就会解析全部文件，不应额外记一个永久升级状态。
        if sessions_existed {
            tx.execute(
                "INSERT OR REPLACE INTO schema_meta(key, value) VALUES ('grok_parent_backfill', '1')",
                [],
            )?;
        }
        tx.commit()?;
    }
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_sessions_parent ON sessions(parent_key)",
        [],
    )?;
    // host 迁移(2026-09-01 远程会话加列;空串 = 本地)。老库首扫时既有行
    // 全部落 '',与远程装饰器生产的非空 host 天然分域,无需回填
    if !table_has_column(&conn, "sessions", "host")? {
        conn.execute(
            "ALTER TABLE sessions ADD COLUMN host TEXT NOT NULL DEFAULT ''",
            [],
        )?;
    }
    // 部分索引须在 host 列就位后建(放 DDL 里对未迁移老库会 no such column):
    // host_counts 的 GROUP BY 从全表扫降为远小于全表的索引扫
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_sessions_host ON sessions(host) WHERE host != ''",
        [],
    )?;
    Ok(conn)
}

fn table_has_column(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let names = stmt.query_map([], |row| row.get::<_, String>(1))?;
    for name in names {
        if name? == column {
            return Ok(true);
        }
    }
    Ok(false)
}

/// 打开索引库;打不开就把它连同 WAL/SHM 一起挪到 `.corrupt` 旁路再建一个空的。
/// 索引本来就能从磁盘全量重扫恢复,重建的真实损失只有 user_data(收藏/置顶)
/// 与 location 配置——而它远好过 GUI 无提示秒退。
/// 返回的 `Some(_)` 是给用户看的说明文案。
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
            "Index was damaged and has been rebuilt — stars, pins and location \
             settings are gone. The old file is kept at {}",
            backup.display()
        )),
    ))
}

/// 读写分连接(WAL 单写多读);Connection 非 Sync,各自套 Mutex
pub struct Store {
    write: Mutex<Connection>,
    read: Mutex<Connection>,
    /// insights() 开临时连接用:统计要连扫 messages 几十毫秒,共用唯一
    /// 读连接会让 UI 线程的列表查询排队等它(2026-08-27 Codex review)
    path: std::path::PathBuf,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        Ok(Self {
            write: Mutex::new(open_conn(path)?),
            read: Mutex::new(open_conn(path)?),
            path: path.to_path_buf(),
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
        write_session_tx(&tx, meta, file_mtime, units)?;
        tx.commit()?;
        Ok(())
    }

    /// 增量写入的并发安全版:胜者比较与写入**同一事务**——先查后写分开时,
    /// 败方副本的事件能与全量扫描交错、后发落库,让 file_path 违背 mtime 裁决
    /// (2026-08-24 Codex review)。返回 false = 本次是败方副本,一字未写
    pub fn write_session_guarded(
        &self,
        meta: &SessionMeta,
        file_mtime: i64,
        units: &[IndexUnit],
    ) -> Result<bool> {
        let mut conn = self.write.lock().unwrap();
        let tx = conn.transaction()?;
        let cur: Option<(String, i64)> = tx
            .query_row(
                "SELECT file_path, file_mtime FROM sessions WHERE key = ?1",
                params![meta.key],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some((cur_path, cur_mtime)) = cur {
            let loses = cur_path != meta.file_path
                && (cur_mtime > file_mtime
                    || (cur_mtime == file_mtime && cur_path.as_str() < meta.file_path.as_str()));
            if loses {
                return Ok(false); // 事务未提交即弃
            }
        }
        write_session_tx(&tx, meta, file_mtime, units)?;
        tx.commit()?;
        Ok(true)
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

    /// 旧索引第一次升级到父子会话 schema 后，现有 Grok 行需要强制重解析，
    /// 才能把临时 worktree cwd 统一回主会话项目。
    pub fn needs_grok_parent_backfill(&self) -> bool {
        let conn = self.read.lock().unwrap();
        conn.query_row(
            "SELECT 1 FROM schema_meta WHERE key = 'grok_parent_backfill'",
            [],
            |_| Ok(()),
        )
        .optional()
        .map(|value| value.is_some())
        .unwrap_or(false)
    }

    pub fn finish_grok_parent_backfill(&self) -> Result<()> {
        let conn = self.write.lock().unwrap();
        conn.execute(
            "DELETE FROM schema_meta WHERE key = 'grok_parent_backfill'",
            [],
        )?;
        Ok(())
    }

    /// 当前胜出副本的 `(key, file_path)`，scanner 用 file_path 把同一 agent
    /// 的多 location 会话交回真正拥有它的 adapter 快照。
    pub fn session_sources_for_agent(&self, agent: AgentId) -> Result<Vec<(String, String)>> {
        let conn = self.read.lock().unwrap();
        let mut stmt = conn.prepare_cached(
            "SELECT key, file_path FROM sessions WHERE agent_id = ?1 ORDER BY key",
        )?;
        let rows = stmt.query_map(params![agent.as_str()], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// scanner 在替换全量关系前用它找出解除/换父的 child；这些会话必须先
    /// 从自己的源文件重解析，恢复被父项目覆盖前的 canonical project。
    pub fn parent_links_for_agent(&self, agent: AgentId) -> Result<HashMap<String, String>> {
        let conn = self.read.lock().unwrap();
        let mut stmt = conn.prepare_cached(
            "SELECT key, parent_key FROM sessions
             WHERE agent_id = ?1 AND parent_key != '' ORDER BY key",
        )?;
        let rows = stmt.query_map(params![agent.as_str()], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// 用某家 adapter 的全量快照原子替换父子关系。只接受库内存在、同 agent、
    /// 非自指的父子键；陈旧关系会被清空。返回是否真的改变了任何行。
    pub fn replace_parent_links(&self, agent: AgentId, links: &[(String, String)]) -> Result<bool> {
        let mut conn = self.write.lock().unwrap();
        let tx = conn.transaction()?;
        let before: Vec<(String, String)> = {
            let mut stmt = tx.prepare_cached(
                "SELECT key, parent_key FROM sessions
                 WHERE agent_id = ?1 AND parent_key != '' ORDER BY key",
            )?;
            let rows = stmt.query_map(params![agent.as_str()], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        tx.execute(
            "UPDATE sessions SET parent_key = '' WHERE agent_id = ?1 AND parent_key != ''",
            params![agent.as_str()],
        )?;
        let mut update = tx.prepare_cached(
            "UPDATE sessions AS child SET
               parent_key = ?1,
               project_path = COALESCE((
                 SELECT NULLIF(parent.project_path, '') FROM sessions parent WHERE parent.key = ?1
               ), child.project_path),
               project_name = COALESCE((
                 SELECT NULLIF(parent.project_name, '') FROM sessions parent WHERE parent.key = ?1
               ), child.project_name)
             WHERE child.key = ?2 AND child.agent_id = ?3 AND child.key != ?1
               AND EXISTS (
                 SELECT 1 FROM sessions parent
                 WHERE parent.key = ?1 AND parent.agent_id = child.agent_id
               )",
        )?;
        let mut unique = std::collections::HashSet::new();
        for (child, parent) in links {
            if unique.insert(child.as_str()) {
                update.execute(params![parent, child, agent.as_str()])?;
            }
        }
        drop(update);
        let after: Vec<(String, String)> = {
            let mut stmt = tx.prepare_cached(
                "SELECT key, parent_key FROM sessions
                 WHERE agent_id = ?1 AND parent_key != '' ORDER BY key",
            )?;
            let rows = stmt.query_map(params![agent.as_str()], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        tx.commit()?;
        Ok(before != after)
    }

    pub fn remove_session(&self, key: &str, tombstone: bool) -> Result<()> {
        self.remove_sessions(&[key.to_string()], tombstone)
    }

    /// 一棵会话树在索引侧原子删除。磁盘路径由调用方先整体移入废纸篓；若
    /// 任一索引步骤失败，所有 session/message/FTS/tombstone 改动一起回滚。
    pub fn remove_sessions(&self, keys: &[String], tombstone: bool) -> Result<()> {
        let mut conn = self.write.lock().unwrap();
        let tx = conn.transaction()?;
        for key in keys {
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
                        "INSERT OR REPLACE INTO tombstones(file_path, key, deleted_at) VALUES (?1, ?2, ?3)",
                        params![fp, key, now_ms()],
                    )?;
                }
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// 路径 → 现行 key(watcher 增量的易主清理用)
    pub fn key_for_path(&self, file_path: &str) -> Result<Option<String>> {
        let conn = self.read.lock().unwrap();
        Ok(conn
            .query_row(
                "SELECT key FROM sessions WHERE file_path = ?1",
                params![file_path],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// 按路径删行,返回被删会话的 key——watcher 用它触发幸存副本上位
    ///(同 key 的另一 location 副本接管,Codex review P2)
    pub fn remove_by_path(&self, file_path: &str) -> Result<Option<String>> {
        let key: Option<String> = {
            let conn = self.read.lock().unwrap();
            conn.query_row(
                "SELECT key FROM sessions WHERE file_path = ?1",
                params![file_path],
                |r| r.get(0),
            )
            .optional()?
        };
        if let Some(k) = &key {
            self.remove_session(k, false)?;
        }
        Ok(key)
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

    /// 应用级 KV 偏好(Open In 目标记忆等 UI 状态)。value 语义由调用方定
    /// (多为 json),不存在回 None
    pub fn pref_get(&self, key: &str) -> Option<String> {
        let conn = self.write.lock().unwrap();
        conn.query_row(
            "SELECT value FROM prefs WHERE key = ?1",
            params![key],
            |r| r.get(0),
        )
        .optional()
        .ok()
        .flatten()
    }

    pub fn pref_set(&self, key: &str, value: &str) -> Result<()> {
        let conn = self.write.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO prefs(key, value) VALUES (?1, ?2)",
            params![key, value],
        )?;
        Ok(())
    }

    // ---------- 自定义 location(Session locations 面板的 Add location) ----------

    /// 与收藏/置顶同层级的用户数据:索引重扫不动它,只有索引文件本体损坏
    /// 重建才丢(open_or_rebuild 的提示文案已列入)
    pub fn list_custom_roots(&self) -> Result<Vec<(String, String)>> {
        let conn = self.read.lock().unwrap();
        let mut stmt =
            conn.prepare_cached("SELECT agent, path FROM custom_roots ORDER BY added_at, path")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.flatten().collect())
    }

    pub fn add_custom_root(&self, agent: &str, path: &str) -> Result<()> {
        let conn = self.write.lock().unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO custom_roots(agent, path, added_at) VALUES (?1, ?2, ?3)",
            params![agent, path, now_ms()],
        )?;
        Ok(())
    }

    /// location 配置一次取齐(自定义根 + 被移除预设/预设路径),解析成模型层类型;
    /// 未识别的 agent 名(库被降级版本写过)静默跳过。GUI 与 scan CLI 共用
    pub fn location_overrides(&self) -> LocationOverrides {
        let customs = self
            .list_custom_roots()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|(a, p)| AgentId::from_str(&a).map(|a| (a, std::path::PathBuf::from(p))))
            .collect();
        let removed = self
            .list_removed_defaults()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|a| AgentId::from_str(&a))
            .collect();
        let removed_roots = self
            .list_removed_default_roots()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|(a, p)| AgentId::from_str(&a).map(|a| (a, std::path::PathBuf::from(p))))
            .collect();
        (customs, removed, removed_roots)
    }

    /// 被用户暂时停用的数据根。与 removed_defaults 不同：停用只控制扫描，
    /// location 配置本身仍在，因此管理面板可以原位重新开启。
    pub fn disabled_locations(&self) -> Vec<(AgentId, std::path::PathBuf)> {
        self.list_disabled_locations()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|(a, p)| AgentId::from_str(&a).map(|a| (a, std::path::PathBuf::from(p))))
            .collect()
    }

    pub fn list_disabled_locations(&self) -> Result<Vec<(String, String)>> {
        let conn = self.read.lock().unwrap();
        let mut stmt = conn.prepare_cached(
            "SELECT agent, path FROM disabled_locations ORDER BY disabled_at, agent, path",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.flatten().collect())
    }

    /// location 开关的唯一写入口。enabled=true 删除停用标记；false 幂等记入。
    pub fn set_location_enabled(&self, agent: &str, path: &str, enabled: bool) -> Result<()> {
        let conn = self.write.lock().unwrap();
        if enabled {
            conn.execute(
                "DELETE FROM disabled_locations WHERE agent = ?1 AND path = ?2",
                params![agent, path],
            )?;
        } else {
            conn.execute(
                "INSERT OR IGNORE INTO disabled_locations(agent, path, disabled_at)
                 VALUES (?1, ?2, ?3)",
                params![agent, path, now_ms()],
            )?;
        }
        Ok(())
    }

    /// 编辑 location 的全形态原子写入(2026-08-24 Codex review:分开自动提交
    /// 时第二步失败会把配置改成半生效)。旧单元:自定义 = 删记录,普通预设 =
    /// 压整家默认,多产品库预设 = 只压该 root;新单元一律记自定义——含换
    /// agent 的编辑,全在一个事务里
    pub fn replace_location(
        &self,
        old_agent: &str,
        old_custom_path: Option<&str>,
        old_default_root: Option<&str>,
        old_data_root: &str,
        new_agent: &str,
        new_path: &str,
    ) -> Result<()> {
        let mut conn = self.write.lock().unwrap();
        let tx = conn.transaction()?;
        // 编辑被停用的 location 后，新路径按启用状态开始；旧路径留下停用记录
        // 会在用户日后重新添加同一路径时意外继承，因此随旧配置一并清理。
        let disabled: Vec<String> = {
            let mut stmt = tx.prepare_cached(
                "SELECT path FROM disabled_locations WHERE agent = ?1 ORDER BY path",
            )?;
            let rows = stmt.query_map(params![old_agent], |r| r.get(0))?;
            rows.flatten().collect()
        };
        // 自定义配置可能派生多个数据根，按落库父路径整组清；内置配置则按
        // 用户正在编辑的真实数据根清。old_default_root 只管 Remove/替换语义，
        // 不能兼任这里的状态键（多数 adapter 在该参数中是 None）。
        let disabled_unit = old_custom_path.unwrap_or(old_data_root);
        for path in disabled
            .iter()
            .filter(|path| crate::adapters::path_owns(disabled_unit, path))
        {
            tx.execute(
                "DELETE FROM disabled_locations WHERE agent = ?1 AND path = ?2",
                params![old_agent, path],
            )?;
        }
        match (old_custom_path, old_default_root) {
            (Some(p), _) => {
                tx.execute(
                    "DELETE FROM custom_roots WHERE agent = ?1 AND path = ?2",
                    params![old_agent, p],
                )?;
            }
            (None, Some(root)) => {
                tx.execute(
                    "INSERT OR IGNORE INTO removed_default_roots(agent, path, removed_at) VALUES (?1, ?2, ?3)",
                    params![old_agent, root, now_ms()],
                )?;
            }
            (None, None) => {
                tx.execute(
                    "INSERT OR IGNORE INTO removed_defaults(agent, removed_at) VALUES (?1, ?2)",
                    params![old_agent, now_ms()],
                )?;
            }
        }
        tx.execute(
            "INSERT OR IGNORE INTO custom_roots(agent, path, added_at) VALUES (?1, ?2, ?3)",
            params![new_agent, new_path, now_ms()],
        )?;
        tx.commit()?;
        Ok(())
    }

    // ---------- 远程 host(Settings → Remote hosts,SSH 会话聚合) ----------

    /// 与 location 配置同层级的用户数据:重扫不动,索引重建才丢。
    /// name 即 ssh 目标(`~/.ssh/config` 的 Host 别名或 user@host),
    /// 字符集校验在 add_remote_host 那道门
    pub fn list_remote_hosts(&self) -> Result<Vec<RemoteHost>> {
        let conn = self.read.lock().unwrap();
        let mut stmt = conn.prepare_cached(
            "SELECT name, enabled, last_sync_at, last_sync_error
             FROM remote_hosts ORDER BY added_at, name",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(RemoteHost {
                name: r.get(0)?,
                enabled: r.get::<_, i64>(1)? != 0,
                last_sync_at: r.get(2)?,
                last_sync_error: r.get(3)?,
            })
        })?;
        Ok(rows.flatten().collect())
    }

    /// roster 组装(create_adapter_roster_for)与 UI 起同步共用——远程实例
    /// 集合与同步目标集合必须同源,别各自 filter 一遍。
    pub fn enabled_remote_host_names(&self) -> Vec<String> {
        self.list_remote_hosts()
            .unwrap_or_default()
            .into_iter()
            .filter(|h| h.enabled)
            .map(|h| h.name)
            .collect()
    }

    pub fn add_remote_host(&self, name: &str) -> Result<()> {
        // 校验在写库这道门(而非各入口自记):host 名进 session key 作中段、
        // 直接当 ssh/rsync 参数,任何未来入口(CLI/导入)都不得绕过
        if !crate::remote::valid_host_name(name) {
            return Err(anyhow::anyhow!("invalid host name: {name:?}"));
        }
        let conn = self.write.lock().unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO remote_hosts(name, enabled, added_at) VALUES (?1, 1, ?2)",
            params![name, now_ms()],
        )?;
        Ok(())
    }

    /// 移除 host 配置本身。缓存目录与已入库会话由调用方另行清理
    /// (Workbench 删缓存目录后补扫,run_scan 的删除检测出清库内行)
    pub fn remove_remote_host(&self, name: &str) -> Result<()> {
        let conn = self.write.lock().unwrap();
        conn.execute("DELETE FROM remote_hosts WHERE name = ?1", params![name])?;
        Ok(())
    }

    pub fn set_remote_host_enabled(&self, name: &str, enabled: bool) -> Result<()> {
        let conn = self.write.lock().unwrap();
        conn.execute(
            "UPDATE remote_hosts SET enabled = ?2 WHERE name = ?1",
            params![name, enabled as i64],
        )?;
        Ok(())
    }

    /// 同步收尾统一写状态:成功清 error,失败保留上次成功时间
    pub fn record_remote_sync(&self, name: &str, error: Option<&str>) -> Result<()> {
        let conn = self.write.lock().unwrap();
        match error {
            None => conn.execute(
                "UPDATE remote_hosts SET last_sync_at = ?2, last_sync_error = NULL WHERE name = ?1",
                params![name, now_ms()],
            )?,
            Some(e) => conn.execute(
                "UPDATE remote_hosts SET last_sync_error = ?2 WHERE name = ?1",
                params![name, e],
            )?,
        };
        Ok(())
    }

    /// 每个远程 host 的库内会话数(Remote hosts 面板;不滤 archived,
    /// 与 locations 面板的 counts_by_path_prefix 同口径)
    pub fn host_counts(&self) -> Result<HashMap<String, i64>> {
        let conn = self.read.lock().unwrap();
        let mut stmt = conn
            .prepare_cached("SELECT host, COUNT(*) FROM sessions WHERE host != '' GROUP BY host")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
        Ok(rows.flatten().collect())
    }

    /// 索引库文件所在目录(远程缓存 `remotes/<host>` 挂在它下面)。
    /// 打开哪个库就用哪个库旁边的缓存——GUI 与 scan CLI 的 --tmp 隔离库
    /// 由此天然分开,互不读写对方的缓存树
    pub fn db_dir(&self) -> Option<std::path::PathBuf> {
        self.path.parent().map(|p| p.to_path_buf())
    }

    /// 恢复初始:清空全部 location 偏离（自定义、被移除预设与停用状态）。
    pub fn clear_location_overrides(&self) -> Result<()> {
        let conn = self.write.lock().unwrap();
        conn.execute_batch(
            "DELETE FROM custom_roots;
             DELETE FROM removed_defaults;
             DELETE FROM removed_default_roots;
             DELETE FROM disabled_locations;",
        )?;
        Ok(())
    }

    /// 预设 location 的移除是"压制该家默认实例"而非删路径——默认根随
    /// env(CODEX_HOME 等)在构造时活解析,不能物化落库,故只记偏离
    pub fn list_removed_defaults(&self) -> Result<Vec<String>> {
        let conn = self.read.lock().unwrap();
        let mut stmt = conn.prepare_cached("SELECT agent FROM removed_defaults ORDER BY agent")?;
        let rows = stmt.query_map([], |r| r.get(0))?;
        Ok(rows.flatten().collect())
    }

    pub fn add_removed_default(&self, agent: &str) -> Result<()> {
        let conn = self.write.lock().unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO removed_defaults(agent, removed_at) VALUES (?1, ?2)",
            params![agent, now_ms()],
        )?;
        Ok(())
    }

    /// 多默认根 adapter 可只压制其中一条；路径保留为构造时解析出的绝对值，
    /// 不会因为移除 next 库而连带关掉同一 agent 的 stable 库。
    pub fn list_removed_default_roots(&self) -> Result<Vec<(String, String)>> {
        let conn = self.read.lock().unwrap();
        let mut stmt = conn.prepare_cached(
            "SELECT agent, path FROM removed_default_roots ORDER BY removed_at, path",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.flatten().collect())
    }

    pub fn add_removed_default_root(&self, agent: &str, path: &str) -> Result<()> {
        let conn = self.write.lock().unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO removed_default_roots(agent, path, removed_at) VALUES (?1, ?2, ?3)",
            params![agent, path, now_ms()],
        )?;
        Ok(())
    }

    pub fn remove_custom_root(&self, agent: &str, path: &str) -> Result<()> {
        let mut conn = self.write.lock().unwrap();
        let tx = conn.transaction()?;
        tx.execute(
            "DELETE FROM custom_roots WHERE agent = ?1 AND path = ?2",
            params![agent, path],
        )?;
        // 同一配置可能派生多个真实根（如 Codex sessions + archived）；真正
        // Remove 时一并清掉这些根的停用标记，今后重新添加应默认启用。
        let disabled: Vec<String> = {
            let mut stmt = tx.prepare_cached(
                "SELECT path FROM disabled_locations WHERE agent = ?1 ORDER BY path",
            )?;
            let rows = stmt.query_map(params![agent], |r| r.get(0))?;
            rows.flatten().collect()
        };
        for disabled_path in disabled
            .iter()
            .filter(|disabled_path| crate::adapters::path_owns(path, disabled_path))
        {
            tx.execute(
                "DELETE FROM disabled_locations WHERE agent = ?1 AND path = ?2",
                params![agent, disabled_path],
            )?;
        }
        tx.commit()?;
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

    /// 逻辑会话级墓碑:同 key 的任何副本(别的 location 里的拷贝)都不得
    /// 让已删会话复活(2026-08-24 Codex review P1,不变量 3 的多副本延伸)
    pub fn is_key_tombstoned(&self, key: &str) -> bool {
        let conn = self.read.lock().unwrap();
        conn.query_row(
            "SELECT 1 FROM tombstones WHERE key = ?1",
            params![key],
            |_| Ok(()),
        )
        .optional()
        .map(|o| o.is_some())
        .unwrap_or(false)
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
            wheres.push(if f.include_archived {
                ROOT_IGNORING_ARCHIVED.into()
            } else {
                ROOT_WHEN_HIDING_ARCHIVED.into()
            });
        }
        let where_sql = if wheres.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", wheres.join(" AND "))
        };
        let order_col = match (f.roots_only, f.sort, f.include_archived) {
            (true, SortKey::Updated, false) => ROOT_UPDATED_ACTIVE,
            (true, SortKey::Updated, true) => ROOT_UPDATED_ALL,
            (true, SortKey::Messages, false) => ROOT_MESSAGES_ACTIVE,
            (true, SortKey::Messages, true) => ROOT_MESSAGES_ALL,
            (_, SortKey::Updated, _) => "s.updated_at",
            (_, SortKey::Created, _) => "s.created_at",
            (_, SortKey::Messages, _) => "s.message_count",
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

        let selected_cols = if f.roots_only {
            if f.include_archived {
                ROOT_SESSION_COLS_ALL
            } else {
                ROOT_SESSION_COLS_ACTIVE
            }
        } else {
            SESSION_COLS
        };
        let sql = format!(
            "SELECT {selected_cols} FROM sessions s LEFT JOIN user_data u ON u.session_key = s.key
             {where_sql}
             ORDER BY COALESCE(u.pinned,0) DESC, {order_col} {order_dir}, s.key ASC LIMIT ? OFFSET ?"
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

    /// 当前筛选下各根会话可见的直接子会话数。Grok 关系在入库时已扁平到
    /// root，因此一次 GROUP BY 足够覆盖任意原始嵌套深度。
    pub fn child_counts(&self, f: &SessionFilter) -> Result<HashMap<String, i64>> {
        let (where_sql, args) = child_filter_sql(f, None);
        let conn = self.read.lock().unwrap();
        let sql = format!(
            "SELECT s.parent_key, COUNT(*)
             FROM sessions s LEFT JOIN user_data u ON u.session_key = s.key
             {where_sql} GROUP BY s.parent_key"
        );
        let mut stmt = conn.prepare_cached(&sql)?;
        let rows = stmt.query_map(
            rusqlite::params_from_iter(args.iter().map(|arg| arg.as_ref())),
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
        )?;
        let mut counts = HashMap::new();
        for row in rows {
            let (key, count) = row?;
            if !key.is_empty() {
                counts.insert(key, count);
            }
        }
        Ok(counts)
    }

    pub fn list_children(&self, parent_key: &str, f: &SessionFilter) -> Result<Vec<SessionMeta>> {
        let (where_sql, args) = child_filter_sql(f, Some(parent_key));
        let order_col = match f.sort {
            SortKey::Updated => "s.updated_at",
            SortKey::Created => "s.created_at",
            SortKey::Messages => "s.message_count",
        };
        let order_dir = if f.ascending { "ASC" } else { "DESC" };
        let conn = self.read.lock().unwrap();
        let sql = format!(
            "SELECT {SESSION_COLS}
             FROM sessions s LEFT JOIN user_data u ON u.session_key = s.key
             {where_sql}
             ORDER BY COALESCE(u.pinned,0) DESC, {order_col} {order_dir}, s.key ASC"
        );
        let mut stmt = conn.prepare_cached(&sql)?;
        let rows = stmt.query_map(
            rusqlite::params_from_iter(args.iter().map(|arg| arg.as_ref())),
            row_to_meta,
        )?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn parent_key_of(&self, key: &str) -> Result<Option<String>> {
        let conn = self.read.lock().unwrap();
        let value: Option<String> = conn
            .query_row(
                "SELECT NULLIF(parent_key, '') FROM sessions WHERE key = ?1",
                params![key],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        Ok(value)
    }

    /// 删除确认使用：不受当前列表筛选和 archived 状态影响，递归取完整子树。
    pub fn all_descendants(&self, key: &str) -> Result<Vec<SessionMeta>> {
        let conn = self.read.lock().unwrap();
        let sql = format!(
            "WITH RECURSIVE descendants(key) AS (
               SELECT key FROM sessions WHERE parent_key = ?1
               UNION
               SELECT child.key FROM sessions child
               JOIN descendants parent ON child.parent_key = parent.key
             )
             SELECT {SESSION_COLS}
             FROM sessions s
             JOIN descendants d ON d.key = s.key
             LEFT JOIN user_data u ON u.session_key = s.key
             ORDER BY s.created_at, s.key"
        );
        let mut stmt = conn.prepare_cached(&sql)?;
        let rows = stmt.query_map(params![key], row_to_meta)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn get_session(&self, key: &str) -> Result<Option<SessionMeta>> {
        let conn = self.read.lock().unwrap();
        let sql = format!(
            "SELECT {SESSION_COLS} FROM sessions s LEFT JOIN user_data u ON u.session_key = s.key WHERE s.key = ?1"
        );
        Ok(conn.query_row(&sql, params![key], row_to_meta).optional()?)
    }

    pub fn list_projects(&self) -> Result<Vec<ProjectInfo>> {
        let conn = self.read.lock().unwrap();
        let mut stmt = conn.prepare_cached(
            "SELECT s.project_path, s.project_name, COUNT(*),
                    MAX(MAX(s.updated_at, COALESCE((
                      SELECT MAX(c.updated_at) FROM sessions c
                      WHERE c.parent_key = s.key AND c.archived = 0
                    ), 0))) AS activity
             FROM sessions s
             WHERE s.archived = 0 AND (s.parent_key = '' OR NOT EXISTS (
               SELECT 1 FROM sessions p WHERE p.key = s.parent_key AND p.archived = 0
             ))
             GROUP BY s.project_path ORDER BY activity DESC",
        )?;
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
            // archived 过滤与 agent_counts/list_projects 同口径,徽标数 = 点开后可见数
            "SELECT COUNT(*) FROM user_data u JOIN sessions s ON s.key = u.session_key WHERE u.favorite = 1 AND s.archived = 0",
            [],
            |r| r.get(0),
        )?)
    }

    pub fn agent_counts(&self) -> Result<HashMap<String, i64>> {
        let conn = self.read.lock().unwrap();
        let mut stmt = conn.prepare_cached(
            "SELECT s.agent_id, COUNT(*) FROM sessions s
             WHERE s.archived = 0 AND (s.parent_key = '' OR NOT EXISTS (
               SELECT 1 FROM sessions p WHERE p.key = s.parent_key AND p.archived = 0
             ))
             GROUP BY s.agent_id",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
        let mut map = HashMap::new();
        for r in rows {
            let (k, v) = r?;
            map.insert(k, v);
        }
        Ok(map)
    }

    /// 各数据源目录下的会话数(Session locations 面板用):一次扫表按
    /// **(agent, 数据根)** 归属,免去每个目录一次往返。**不过滤 archived**
    /// ——归档目录本就该显示自己的量,那正是 agent_counts(WHERE archived = 0)
    /// 看不见的那部分。
    /// 必须连 agent 一起比,且边界走 adapters::path_owns:CODEX_HOME / XDG_DATA_HOME
    /// 允许把一家的数据根搬进另一家的树下,只认裸路径前缀会把整批会话静默
    /// 记到别家行上
    pub fn counts_by_path_prefix(&self, sources: &[(String, String)]) -> Result<Vec<i64>> {
        let conn = self.read.lock().unwrap();
        let mut stmt = conn.prepare_cached("SELECT agent_id, file_path FROM sessions")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        let mut counts = vec![0i64; sources.len()];
        for row in rows {
            let (agent, path) = row?;
            if let Some(i) = sources
                .iter()
                .position(|(a, root)| *a == agent && crate::adapters::path_owns(root, &path))
            {
                counts[i] += 1;
            }
        }
        Ok(counts)
    }

    /// Insights 页统计快照。`today` 由调用方传入(streak 相对它计算,
    /// 也让测试不依赖真实时钟);SQL 侧 `'localtime'` 与 chrono `Local`
    /// 同取系统时区,日界一致。messages 全表恰扫两遍(日×时分桶 + 榜单
    /// prompts),量级几十万行、几十毫秒——调用方走后台任务,别在 UI
    /// 线程等它。
    pub fn insights(&self, today: chrono::NaiveDate) -> Result<InsightsData> {
        use chrono::Datelike as _;
        // SQL 侧 date() 出来的本地日 → 可进分桶的日。时钟漂移的脏数据可能带
        // 未来日期:不进任何分桶(prompts 总数无日期语义仍计入)。热力图不画
        // 未来格,streak/活跃天数若认了就会与它互相矛盾(2026-08-27 Codex review)
        let day_of = |s: &str| {
            chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d")
                .ok()
                .filter(|d| *d <= today)
        };
        // "一条 prompt" 的行集(主线用户消息)——整页口径共用这一个片段,
        // 内联多份的话谓词一漂移,总数就会与分桶/榜单悄悄不一致
        const PROMPT_ROWS: &str = "FROM messages m JOIN sessions s ON s.key = m.session_key
             WHERE s.archived = 0 AND m.role = 'user' AND m.sidechain_id IS NULL";

        // 临时连接,不与 UI 的 read 连接抢锁:WAL 多读并发,几十毫秒的
        // 统计扫描不该让导航点击的列表查询排队(2026-08-27 Codex review)
        let conn = open_conn(&self.path)?;
        let mut data = InsightsData {
            as_of: today,
            ..Default::default()
        };

        (
            data.sessions,
            data.tokens,
            data.first_ts,
            data.project_count,
        ) = conn.query_row(
            "SELECT COUNT(*), COALESCE(SUM(tokens_used),0),
                    COALESCE(MIN(NULLIF(created_at,0)),0),
                    COUNT(DISTINCT NULLIF(project_path,''))
             FROM sessions WHERE archived = 0",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )?;

        // 第一遍:按(日,时)组合分桶(行数 = 活跃日×活跃时段,千级)。
        // 无 ts 的行落 NULL 桶只计入总数;weekday/monthly 由日期在 Rust 侧
        // 派生,省去再让 SQL 各扫一遍 + 每行多次 strftime 时区换算
        let mut stmt = conn.prepare_cached(&format!(
            "SELECT CASE WHEN m.ts > 0 THEN date(m.ts/1000,'unixepoch','localtime') END d,
                    CASE WHEN m.ts > 0 THEN CAST(strftime('%H', m.ts/1000,'unixepoch','localtime') AS INTEGER) END h,
                    COUNT(*)
             {PROMPT_ROWS}
             GROUP BY d, h ORDER BY d"
        ))?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, Option<String>>(0)?,
                r.get::<_, Option<i64>>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })?;
        for row in rows {
            let (d, h, n) = row?;
            data.prompts += n;
            let Some(day) = d.as_deref().and_then(day_of) else {
                continue;
            };
            if let Some(h) = h.filter(|h| (0..24).contains(h)) {
                data.hourly[h as usize] += n;
            }
            data.weekday[day.weekday().num_days_from_monday() as usize] += n;
            data.monthly[day.month0() as usize] += n;
            // ORDER BY d 保证同日相邻,尾项聚合即可
            match data.daily.last_mut() {
                Some((last, c)) if *last == day => *c += n,
                _ => data.daily.push((day, n)),
            }
        }
        drop(stmt);

        // 第二遍:榜单 prompts 按(agent,项目,模型,日)一次分组(行数 = 组合数×
        // 活跃日,千级),拆三张榜单 map + agent 周桶回填——榜单求和不看日期
        // (无 ts / 未来行也计),周桶只收落在趋势窗内的日。模型不出周桶:
        // s.model 是会话末态,按它切周会把整段历史归给最后用的模型
        let mut prompts_by_agent: HashMap<String, i64> = HashMap::new();
        let mut prompts_by_project: HashMap<String, i64> = HashMap::new();
        let mut prompts_by_model: HashMap<String, i64> = HashMap::new();
        let mut by_agent: HashMap<String, Vec<i64>> = HashMap::new();
        let mut stmt = conn.prepare_cached(&format!(
            "SELECT s.agent_id, s.project_path, COALESCE(s.model,''),
                    CASE WHEN m.ts > 0 THEN date(m.ts/1000,'unixepoch','localtime') END d,
                    COUNT(*)
             {PROMPT_ROWS}
             GROUP BY 1, 2, 3, 4"
        ))?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, i64>(4)?,
            ))
        })?;
        for row in rows {
            let (agent, project, model, d, n) = row?;
            *prompts_by_agent.entry(agent.clone()).or_default() += n;
            *prompts_by_project.entry(project).or_default() += n;
            if !model.is_empty() {
                *prompts_by_model.entry(model).or_default() += n;
            }
            let Some(ix) = d
                .as_deref()
                .and_then(day_of)
                .and_then(|day| trend_week_index(today, day))
            else {
                continue;
            };
            by_agent
                .entry(agent)
                .or_insert_with(|| vec![0; TREND_WEEKS])[ix] += n;
        }
        drop(stmt);
        data.trend_agents = by_agent
            .into_iter()
            .map(|(name, weekly)| TrendSeries { name, weekly })
            .collect();
        data.trend_agents
            .sort_by(|a, b| b.total().cmp(&a.total()).then_with(|| a.name.cmp(&b.name)));

        // 榜单主体只查 sessions 表(几百行),prompts 由上面的 map 回填。
        // display 与 group 分开传:项目按 path 分组、按名展示(同名异路径
        // 各占一行,合并会把两个真实目录混作一处)。全量返回不截断——
        // top-N 由 UI 按当前度量排序后取,否则换度量会漏项
        let usage = |display: &str,
                     group: &str,
                     filter: &str,
                     prompts: &HashMap<String, i64>|
         -> Result<Vec<UsageTally>> {
            let sql = format!(
                "SELECT {display}, {group}, COUNT(*), COALESCE(SUM(s.tokens_used),0)
                 FROM sessions s WHERE s.archived = 0{filter}
                 GROUP BY {group} ORDER BY COUNT(*) DESC, {display}"
            );
            let mut stmt = conn.prepare_cached(&sql)?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    UsageTally {
                        name: r.get(0)?,
                        sessions: r.get(2)?,
                        tokens: r.get(3)?,
                        prompts: 0,
                    },
                    r.get::<_, String>(1)?,
                ))
            })?;
            rows.map(|row| {
                let (mut tally, key) = row?;
                tally.prompts = prompts.get(&key).copied().unwrap_or(0);
                Ok(tally)
            })
            .collect()
        };
        data.agents = usage("s.agent_id", "s.agent_id", "", &prompts_by_agent)?;
        // 无 cwd 的会话 project_path 为空、name 是 "Unknown project" 占位:
        // 概览 project_count 按空 path 排除,榜单必须同谓词——按 name 滤会
        // 让页面一边数 0 一边列出 Unknown project(2026-08-27 Codex review)
        data.projects = usage(
            "s.project_name",
            "s.project_path",
            " AND s.project_path != ''",
            &prompts_by_project,
        )?;
        data.models = usage(
            "s.model",
            "s.model",
            " AND s.model IS NOT NULL AND s.model != ''",
            &prompts_by_model,
        )?;

        // 会话按创建日分桶(Last 7 days 的 sessions 对比用)。date() 对超出
        // 其范围的正数(微秒戳、脏值)返回 NULL——按 Option 读,坏行跳过,
        // 不能让一条脏 created_at 掀翻整张快照(2026-09-03 Codex review)
        let mut stmt = conn.prepare_cached(
            "SELECT date(created_at/1000,'unixepoch','localtime'), COUNT(*)
             FROM sessions WHERE archived = 0 AND created_at > 0
             GROUP BY 1 ORDER BY 1",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, Option<String>>(0)?, r.get::<_, i64>(1)?))
        })?;
        for row in rows {
            let (d, n) = row?;
            if let Some(day) = d.as_deref().and_then(day_of) {
                data.daily_sessions.push((day, n));
            }
        }
        drop(stmt);

        (data.current_streak, data.longest_streak) = compute_streaks(&data.daily, today);
        Ok(data)
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

const SESSION_COLS: &str =
    "s.key, s.agent_id, s.native_id, s.title, s.project_path, s.project_name,
    s.git_branch, s.created_at, s.updated_at, s.message_count, s.tokens_used, s.model, s.source,
    s.archived, s.file_path, s.file_size, COALESCE(u.favorite,0), COALESCE(u.pinned,0), s.host";

const ROOT_WHEN_HIDING_ARCHIVED: &str = "(s.parent_key = '' OR NOT EXISTS (
       SELECT 1 FROM sessions p WHERE p.key = s.parent_key AND p.archived = 0
     ))";
const ROOT_IGNORING_ARCHIVED: &str =
    "(s.parent_key = '' OR NOT EXISTS (SELECT 1 FROM sessions p WHERE p.key = s.parent_key))";
const ROOT_UPDATED_ACTIVE: &str =
    "MAX(s.updated_at, COALESCE((SELECT MAX(c.updated_at) FROM sessions c
      WHERE c.parent_key = s.key AND c.archived = 0), 0))";
const ROOT_UPDATED_ALL: &str =
    "MAX(s.updated_at, COALESCE((SELECT MAX(c.updated_at) FROM sessions c
      WHERE c.parent_key = s.key), 0))";
const ROOT_MESSAGES_ACTIVE: &str =
    "s.message_count + COALESCE((SELECT SUM(c.message_count) FROM sessions c
      WHERE c.parent_key = s.key AND c.archived = 0), 0)";
const ROOT_MESSAGES_ALL: &str =
    "s.message_count + COALESCE((SELECT SUM(c.message_count) FROM sessions c
      WHERE c.parent_key = s.key), 0)";
const ROOT_SESSION_COLS_ACTIVE: &str =
    "s.key, s.agent_id, s.native_id, s.title, s.project_path, s.project_name,
     s.git_branch, s.created_at,
     MAX(s.updated_at, COALESCE((SELECT MAX(c.updated_at) FROM sessions c
       WHERE c.parent_key = s.key AND c.archived = 0), 0)),
     s.message_count + COALESCE((SELECT SUM(c.message_count) FROM sessions c
       WHERE c.parent_key = s.key AND c.archived = 0), 0),
     s.tokens_used, s.model, s.source, s.archived, s.file_path, s.file_size,
     COALESCE(u.favorite,0), COALESCE(u.pinned,0), s.host";
const ROOT_SESSION_COLS_ALL: &str =
    "s.key, s.agent_id, s.native_id, s.title, s.project_path, s.project_name,
     s.git_branch, s.created_at,
     MAX(s.updated_at, COALESCE((SELECT MAX(c.updated_at) FROM sessions c
       WHERE c.parent_key = s.key), 0)),
     s.message_count + COALESCE((SELECT SUM(c.message_count) FROM sessions c
       WHERE c.parent_key = s.key), 0),
     s.tokens_used, s.model, s.source, s.archived, s.file_path, s.file_size,
     COALESCE(u.favorite,0), COALESCE(u.pinned,0), s.host";

fn child_filter_sql(
    filter: &SessionFilter,
    parent_key: Option<&str>,
) -> (String, Vec<Box<dyn rusqlite::ToSql>>) {
    let mut wheres = Vec::new();
    let mut args: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    match parent_key {
        Some(parent) => {
            wheres.push("s.parent_key = ?".to_string());
            args.push(Box::new(parent.to_string()));
        }
        None => wheres.push("s.parent_key != ''".to_string()),
    }
    if !filter.agents.is_empty() {
        let placeholders = filter
            .agents
            .iter()
            .map(|_| "?")
            .collect::<Vec<_>>()
            .join(",");
        wheres.push(format!("s.agent_id IN ({placeholders})"));
        for agent in &filter.agents {
            args.push(Box::new(agent.as_str().to_string()));
        }
    }
    if let Some(project) = &filter.project_path {
        wheres.push("s.project_path = ?".to_string());
        args.push(Box::new(project.clone()));
    }
    if filter.favorite_only {
        wheres.push("COALESCE(u.favorite, 0) = 1".to_string());
    }
    if !filter.include_archived {
        wheres.push("s.archived = 0".to_string());
    }
    if let Some(query) = filter
        .title_query
        .as_deref()
        .map(str::trim)
        .filter(|query| !query.is_empty())
    {
        wheres.push("(s.title LIKE ? ESCAPE '\\' OR s.project_name LIKE ? ESCAPE '\\')".into());
        let like = format!("%{}%", escape_like(query));
        args.push(Box::new(like.clone()));
        args.push(Box::new(like));
    }
    (format!("WHERE {}", wheres.join(" AND ")), args)
}

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
        host: r.get(18)?,
    })
}

/// write_session / write_session_guarded 共用的事务内核
fn write_session_tx(
    tx: &rusqlite::Transaction<'_>,
    meta: &SessionMeta,
    file_mtime: i64,
    units: &[IndexUnit],
) -> Result<()> {
    // FTS external content 需要显式 delete 旧行
    let mut sel = tx.prepare_cached("SELECT id, text FROM messages WHERE session_key = ?1")?;
    let rows: Vec<(i64, String)> = sel
        .query_map(params![meta.key], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    drop(sel);
    let mut fts_del = tx.prepare_cached(
        "INSERT INTO messages_fts(messages_fts, rowid, text) VALUES ('delete', ?1, ?2)",
    )?;
    for (id, text) in rows {
        fts_del.execute(params![id, text])?;
    }
    drop(fts_del);
    tx.execute(
        "DELETE FROM messages WHERE session_key = ?1",
        params![meta.key],
    )?;

    upsert_session(tx, meta, file_mtime)?;

    let mut ins_msg = tx.prepare_cached(
        "INSERT INTO messages(session_key, sidechain_id, seq, role, ts, text) VALUES (?1,?2,?3,?4,?5,?6)",
    )?;
    let mut ins_fts = tx.prepare_cached("INSERT INTO messages_fts(rowid, text) VALUES (?1, ?2)")?;
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
    Ok(())
}

fn upsert_session(tx: &rusqlite::Transaction<'_>, m: &SessionMeta, file_mtime: i64) -> Result<()> {
    tx.execute(
        "INSERT INTO sessions(key, agent_id, native_id, title, project_path, project_name,
           git_branch, created_at, updated_at, message_count, tokens_used, model, source,
           archived, file_path, file_size, file_mtime, unknown_lines, host)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,0,?18)
         ON CONFLICT(key) DO UPDATE SET
           title=excluded.title, project_path=excluded.project_path,
           project_name=excluded.project_name, git_branch=excluded.git_branch,
           created_at=excluded.created_at, updated_at=excluded.updated_at,
           message_count=excluded.message_count, tokens_used=excluded.tokens_used,
           model=excluded.model, source=excluded.source, archived=excluded.archived,
           file_path=excluded.file_path, file_size=excluded.file_size,
           file_mtime=excluded.file_mtime, host=excluded.host",
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
            m.host,
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

/// (current, longest) 连续活跃天数。`daily` 须按日期升序(insights 的
/// SQL 保证)。current 从 today 往回数;今天尚无活动时允许从昨天起算
/// (GitHub 惯例:一天没结束不清零),更早断档即为 0。
fn compute_streaks(daily: &[(chrono::NaiveDate, i64)], today: chrono::NaiveDate) -> (i64, i64) {
    let mut longest = 0i64;
    let mut run = 0i64;
    let mut prev: Option<chrono::NaiveDate> = None;
    for &(d, _) in daily {
        run = match prev {
            Some(p) if (d - p).num_days() == 1 => run + 1,
            _ => 1,
        };
        longest = longest.max(run);
        prev = Some(d);
    }
    let current = match prev {
        Some(last) if (today - last).num_days() <= 1 => run,
        _ => 0,
    };
    (current, longest)
}

pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// 索引库路径:macOS 为 ~/Library/Application Support/wake,Linux 为
/// ~/.local/share/wake,Windows 为 %LOCALAPPDATA%\wake
/// (从旧 vibex 路径一次性迁移,保留收藏等 user_data)。
///
/// Windows 取 data_local_dir 而非 data_dir:后者是漫游 %APPDATA%,而本库
/// 开 WAL——WAL 要 -shm 共享内存映射,重定向到网络盘的漫游目录上根本打不开
/// (域环境 Folder Redirection 是标配),Wake 会在启动即致命退出;何况这是
/// 可随时重建的索引,几百 MB 跟着登录/注销来回同步纯属浪费(2026-08-25 review)
pub fn default_db_path() -> std::path::PathBuf {
    #[cfg(target_os = "windows")]
    let data = dirs::data_local_dir();
    #[cfg(not(target_os = "windows"))]
    let data = dirs::data_dir();
    let data = data.unwrap_or_else(|| std::path::PathBuf::from("."));
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
