use crate::models::*;
use anyhow::{Context as _, Result};
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::HashMap;
use std::fs::{File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

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

-- 标题的全文索引。独立一张表而不挂在 messages 上:标题不是转录消息、没有 seq
-- 可对,塞成假消息会破坏 seq 契约。key 不索引、只用来回查/删除
CREATE VIRTUAL TABLE IF NOT EXISTS titles_fts USING fts5(
  key UNINDEXED,
  title,
  tokenize="trigram case_sensitive 0"
);

-- agent 自己写下的记忆(Claude Code auto-memory、Codex memories)的只读镜像:
-- 扫描收尾按 (agent, host) 整组替换(2026-09-17)。正文一并入库(小 Markdown),
-- 搜索走 memories_fts(rowid = memories.id,删改按 rowid 定位——FTS 表里
-- UNINDEXED 列的 WHERE 是整表扫描,正文列在里面扫不起);阅读时文件型先读
-- 磁盘、读不到再用这份。project_path 多半为空,读时按 session_key 连 sessions 解析
CREATE TABLE IF NOT EXISTS memories (
  id           INTEGER PRIMARY KEY,
  key          TEXT NOT NULL UNIQUE,
  agent_id     TEXT NOT NULL,
  host         TEXT NOT NULL DEFAULT '',
  scope        TEXT NOT NULL,
  project_path TEXT NOT NULL DEFAULT '',
  project_name TEXT NOT NULL DEFAULT '',
  session_key  TEXT NOT NULL DEFAULT '',
  path         TEXT NOT NULL,
  title        TEXT NOT NULL DEFAULT '',
  updated_at   INTEGER DEFAULT 0,
  size_bytes   INTEGER DEFAULT 0,
  source       TEXT NOT NULL DEFAULT '',
  body         TEXT NOT NULL DEFAULT ''
);
CREATE INDEX IF NOT EXISTS idx_memories_group ON memories(agent_id, host);
CREATE TABLE IF NOT EXISTS memory_sources_custom (
  agent    TEXT NOT NULL,
  path     TEXT NOT NULL,
  added_at INTEGER NOT NULL,
  PRIMARY KEY (agent, path)
);
CREATE TABLE IF NOT EXISTS memory_sources_disabled (
  agent       TEXT NOT NULL,
  source      TEXT NOT NULL,
  disabled_at INTEGER NOT NULL,
  PRIMARY KEY (agent, source)
);
CREATE VIRTUAL TABLE IF NOT EXISTS memories_fts USING fts5(
  title,
  body,
  tokenize="trigram case_sensitive 0"
);

-- agent 查 Wake 的每一次调用(解析时从工具调用里认出,2026-09-17)。与 messages
-- 同一事务写、同 key 删;channel = mcp | cli,tool = MCP 工具名或命令行二进制名,
-- ts = 所在消息的时间(缺则 NULL,归窗时退回会话 updated_at)。老库首开由
-- FTS_FORMAT 换代的全量重解析回填
CREATE TABLE IF NOT EXISTS wake_lookups (
  session_key TEXT NOT NULL,
  seq         INTEGER NOT NULL,
  ts          INTEGER,
  channel     TEXT NOT NULL,
  tool        TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_wake_lookups_session ON wake_lookups(session_key);

-- 外壳产品认领的别家会话(2026-09-24,Craft Agents:它的 Claude 后端跑的是 Claude Agent
-- SDK,引擎在 ~/.claude/projects 里另落一份转录,同一段对话会以 Claude Code 身份再列一次)。
-- key 是替身的会话 key,claimant 是认领方 agent。被认领的 key 不入库(写入闸门在
-- write_session_guarded / write_meta_only 的事务里),认领落地时已入库的一并删掉;
-- 每轮扫描按认领方整组替换,认领方的会话没了,替身下一轮就回来。只有 GUI 与写库的
-- CLI 碰它,只读读者不查(NEWEST_* 不动)
CREATE TABLE IF NOT EXISTS claimed_sessions (
  key      TEXT PRIMARY KEY,
  claimant TEXT NOT NULL
);
"#;

/// 会话元数据和 FTS 单元的派生规则版本(`adapters::units_from_messages` 及其上游解析)。改了派生
/// 规则就换个值:旧库首开时挂 fts_reindex 旗子,下一轮扫描强制重解析全部文件。
/// "1" = 2026-09-14 前(工具段不过滤 Wake 自指),"2" = 过滤自指回声,
/// "3" = Pi / omp / OpenClaw 累计每次 assistant 调用的 token,
/// "4" = Cursor 项目路径优先读取工作区元数据,并恢复 slug 中的空格,
/// "5" / "6" = 未发布分支各自用过的中间态(#42 用过 5;meter-handoff 用过 6:同一派生里
///       记下 agent 查 Wake 的每次调用,落 wake_lookups 表,老库靠这轮重解析回填),
/// "7" = 三条分支合到 main 时对齐(2026-09-21/22):Codex spawn_agent 子线程折叠、
///       wake_lookups、记忆层一起回填——开发库可能戳着 5 或 6,换代判据是精确不等,
///       两个都跳过;
/// "8" = Cursor 会话补上模型与 token(IDE 库的 modelInfo / modelConfig / usageData /
///       tokenCount;转录胜出的会话向同一个 composer 借),老行靠这轮重解析回填。
pub const FTS_FORMAT: &str = "8";

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
    // FTS 派生规则换代:库里记的版本与 FTS_FORMAT 不一致且已有会话 → 挂 fts_reindex
    // 旗子,下一轮扫描把全部文件重解析一遍(增量按 mtime/size 跳过的也重来),否则
    // 旧行会按旧规则一直留着,直到用户手动全量刷新(Codex review 2026-09-14)
    let stored_format: Option<String> = conn
        .query_row(
            "SELECT value FROM schema_meta WHERE key = 'fts_format'",
            [],
            |r| r.get(0),
        )
        .optional()?;
    if stored_format.as_deref() != Some(FTS_FORMAT) {
        if sessions_existed {
            conn.execute(
                "INSERT OR REPLACE INTO schema_meta(key, value) VALUES ('fts_reindex', '1')",
                [],
            )?;
        }
        conn.execute(
            "INSERT OR REPLACE INTO schema_meta(key, value) VALUES ('fts_format', ?1)",
            params![FTS_FORMAT],
        )?;
    }
    // host 迁移(2026-09-01 远程会话加列;空串 = 本地)。老库首扫时既有行
    // 全部落 '',与远程装饰器生产的非空 host 天然分域,无需回填
    if !table_has_column(&conn, "sessions", "host")? {
        conn.execute(
            "ALTER TABLE sessions ADD COLUMN host TEXT NOT NULL DEFAULT ''",
            [],
        )?;
    }
    // 记忆来源列(记忆可见层二期,2026-09-21):source = 所属来源的 id。memories 表还
    // 没发布过,但开发机上的库已按旧 DDL 建过;老行 source 为空,下一轮扫描按来源
    // 指纹重写(开发机上短暂存在过的 origin 列留着不碍事,DDL 与读写都不再提它)
    // 守卫写死列名、不引用 NEWEST_COLUMN:那个常量下次迁移就会改指别的列,这里跟着改指
    // 就会对已迁移的库再 ALTER 一次,duplicate column 让 open_or_rebuild 把好端端的索引
    // 当坏库重建(2026-09-22 review)
    if !table_has_column(&conn, "memories", "source")? {
        conn.execute(
            "ALTER TABLE memories ADD COLUMN source TEXT NOT NULL DEFAULT ''",
            [],
        )?;
    }
    // 部分索引须在 host 列就位后建(放 DDL 里对未迁移老库会 no such column):
    // host_counts 的 GROUP BY 从全表扫降为远小于全表的索引扫
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_sessions_host ON sessions(host) WHERE host != ''",
        [],
    )?;
    // titles_fts 回填(2026-09-14 加表):老库首次打开时表是空的而 sessions 不空,
    // 把既有标题一次性灌进去;之后由 upsert_session 逐行维护。判据是"空表且有
    // 会话",不记 schema_meta 旗子——全删过的库再开也只是白查一次 count
    let needs_titles: bool = conn.query_row(
        "SELECT (SELECT count(*) FROM titles_fts) = 0 AND EXISTS (SELECT 1 FROM sessions)",
        [],
        |r| r.get(0),
    )?;
    if needs_titles {
        conn.execute(
            "INSERT INTO titles_fts(key, title) SELECT key, title FROM sessions WHERE title != ''",
            [],
        )?;
    }
    Ok(conn)
}

/// 最近一次迁移加的、**只读读者(wake-mcp / wake-cli)会查的**列——`open_read_only`
/// 用它判断库够不够新。加了只读路径要读的新列就改这里,否则只读入口会放行老库、
/// 深处查询才报 no such column;只有 GUI 读的表/列(如 wake_lookups)不算,否则
/// 一次 GUI 侧的加表就让旁路二进制对所有老库拒开
const NEWEST_COLUMN: (&str, &str) = ("memories", "source");
/// 最近一次迁移加的、只读读者会查的表(wake_list_memories 读 memories),与
/// NEWEST_COLUMN 同一用途、同一维护规矩
const NEWEST_TABLE: &str = "memories";

fn table_exists(conn: &Connection, table: &str) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type IN ('table','view') AND name = ?1)",
        params![table],
        |r| r.get(0),
    )?)
}

/// 只读连接:不建表、不迁移、不改 journal_mode(旁路进程用;`open_read_only`
/// 与只读 Store 的 insights 临时连接共用,别的入口不要直开)
fn open_conn_ro(path: &Path) -> Result<Connection> {
    let conn = Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(std::time::Duration::from_millis(3000))?;
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
    // 挪走的是**真实文件**:默认路径是符号链接时链接留着、新库仍沿链接建在目标处,GUI 手里
    // `IndexLock` 锁的正是目标旁那把;挪链接本身会让锁留在旧目标旁、新库无人看管
    // (Codex review 2026-09-23)。重开仍走原路径,`Store.path` 与远程镜像目录不变。
    // 三件套一起挪:留下 WAL 或 SHM 任何一个,新库都会接着读旧日志
    let real = canonical(path);
    let backup = std::path::PathBuf::from(format!("{}.corrupt", real.display()));
    let _ = std::fs::remove_file(&backup);
    let _ = std::fs::rename(&real, &backup);
    for suffix in ["-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", real.display()));
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

/// 索引库的**跨进程**写者锁。`scanner::SCAN_GATE` 只管一个进程里的两条扫描串行,
/// 这把锁管进程之间:GUI 与 `wake-cli refresh` / `wake-cli index` / scan bin 都是写者,
/// 同一时刻只能有一个——两个进程各建一份 roster,launchd 与 Dock 起的 GUI 看到的 env
/// 未必相同(CODEX_HOME / XDG_DATA_HOME / WAKE_HOME 各解各的根),删除检测会把对方收进来
/// 的会话当"磁盘已删"清掉、下一轮再由对方加回,每十分钟互删一次;靠 busy_timeout 排队
/// 解决不了这个,只能不并发。
///
/// 形态:`<db>.lock` 旁路文件上的文件锁(unix flock / Windows LockFileEx,std 的
/// `File::try_lock`),路径按库的**真实路径**派生——`--db` 给到符号链接别名也得撞上同一把。
/// **GUI 另持一把 `<db>.lock.app`**:别人拿不到主锁时探这把锁,拿不到就是有个活着的 GUI——
/// 锁随进程生死,不会残留,"持有者是不是 GUI"这条决定 GUI 等不等、CLI 怎么措辞的判据
/// 只认它。持有者自述(`"wake-cli refresh 4242"`)另写在 `<db>.lock.holder`,**只用来点名**:
/// 它写在拿到锁之后,拿到与写完之间读到的是上一任的残留,拿它做判断就会把新起的 GUI 误
/// 劝退;也不写进锁文件——Windows 的文件锁是强制锁,别的句柄读不到被锁的内容(三条都是
/// 2026-09-23 Codex review)。
/// **谁写库谁持有,且持有期必须盖住读写 `Store` 的整个生命期**:GUI 从启动持到退出
/// (watcher 与扫描随时会写),CLI 写命令只在干活那几秒持有;只读旁路(wake-mcp、
/// wake-cli 的查询)不拿。锁不长在 `Store::open` 里,因为同一进程会同时开多个 Store
/// (scanner_finale 的用例、Dock 重开)而 `open_or_rebuild` 还会把库挪走重开,锁的生命期
/// 得独立于单个 Store——写者名单是封闭的,见 `Store::open` 的约定。锁跟着文件描述符走,
/// 进程退出(含 SIGKILL)即释放,不留残骸;锁文件本身不删——删了别人接着 open 出来的
/// 是另一个 inode,两把锁互不相见
pub struct IndexLock {
    _file: File,
    /// GUI 才有的第二把(`<db>.lock.app`)
    _app: Option<File>,
}

/// 拿不到锁时的持有者:`app` 来自 `.lock.app` 那把锁(活着的 GUI 才持有),文字来自
/// 旁路自述(可能残留,只用来点名)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockHolder {
    pub app: bool,
    description: String,
}

impl LockHolder {
    fn new(app: bool, note: &str) -> Self {
        let description = match note.trim() {
            "" => "another process",
            s => s,
        };
        Self {
            app,
            description: description.to_string(),
        }
    }
}

impl std::fmt::Display for LockHolder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.description)
    }
}

/// `IndexLock::try_acquire` 的结果
pub enum Ownership {
    Ours(IndexLock),
    Held(LockHolder),
}

/// `IndexLock::acquire_or_wait` 的结果
pub enum Wait {
    Ours(IndexLock),
    /// GUI 持有:没等,它不会让
    HeldByApp(LockHolder),
    /// 等满了还是别人的
    TimedOut(LockHolder),
}

/// GUI 拿锁时报的种类名;CLI 写命令写 "wake-cli index" / "wake-cli refresh"。种类是
/// GUI 才多持 `.lock.app` 那把锁
pub const LOCK_KIND_APP: &str = "Wake";

fn open_lock_file(path: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
}

impl IndexLock {
    /// `<db>.lock`,按库的真实路径派生(见 `canonical`)
    pub fn path_for(db: &Path) -> PathBuf {
        PathBuf::from(format!("{}.lock", canonical(db).display()))
    }

    /// 持有者自述的旁路文件 `<db>.lock.holder`
    fn holder_path(lock: &Path) -> PathBuf {
        PathBuf::from(format!("{}.holder", lock.display()))
    }

    /// GUI 的第二把锁 `<db>.lock.app`
    fn app_path(lock: &Path) -> PathBuf {
        PathBuf::from(format!("{}.app", lock.display()))
    }

    /// 非阻塞。`kind` 是自述的前半段,pid 由这里补
    pub fn try_acquire(db: &Path, kind: &str) -> std::io::Result<Ownership> {
        let path = Self::path_for(db);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let file = open_lock_file(&path)?;
        match file.try_lock() {
            Ok(()) => {
                // 自述只用来点名,写不进去(权限一类)不能把刚到手的主锁放掉——放掉后 GUI 的
                // 对策是无锁启动,那就成了并发写库;尽力写,写不成别人只看到 "another process"
                // (Codex review 2026-09-23)
                let _ = std::fs::write(
                    Self::holder_path(&path),
                    format!("{kind} {}", std::process::id()),
                );
                // 主锁到手就算拿到了;`.lock.app` 拿不到(权限一类)只让别人认不出这是 GUI:
                // CLI 会当成别的写者退让,另一个 GUI 会等满再报——都不会并发写库
                let app = if kind == LOCK_KIND_APP {
                    Self::take_app_lock(&path).ok()
                } else {
                    None
                };
                Ok(Ownership::Ours(IndexLock {
                    _file: file,
                    _app: app,
                }))
            }
            Err(TryLockError::WouldBlock) => {
                // 主锁被占是已经确认的事实,探 `.lock.app` 失败不能把它翻成 I/O 错——GUI 对
                // 拿锁出错的对策是无锁启动,那就成了并发写库;保守按"被占、不是 GUI"报,
                // GUI 会等、CLI 会退让(Codex review 2026-09-23)
                let app = Self::app_lock_is_held(&path).unwrap_or(false);
                let note = std::fs::read_to_string(Self::holder_path(&path)).unwrap_or_default();
                Ok(Ownership::Held(LockHolder::new(app, &note)))
            }
            Err(TryLockError::Error(e)) => Err(e),
        }
    }

    /// 主锁在手时拿 `.lock.app`(独占)。活着的 GUI 只可能是我们自己,所以它本该是空的;
    /// 别人的共享锁探测会瞬时占住几微秒(拿到即放),撞上就隔一下再拿,别当失败
    fn take_app_lock(lock: &Path) -> std::io::Result<File> {
        let path = Self::app_path(lock);
        let mut tries = 0;
        loop {
            let file = open_lock_file(&path)?;
            match file.try_lock() {
                Ok(()) => return Ok(file),
                Err(TryLockError::WouldBlock) if tries < 50 => {
                    tries += 1;
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(TryLockError::WouldBlock) => {
                    return Err(std::io::Error::other(
                        "another Wake holds the app lock while the index lock was free",
                    ))
                }
                Err(TryLockError::Error(e)) => return Err(e),
            }
        }
    }

    /// 探 `.lock.app`:以**共享锁**探,拿到即放,拿不到就是有个活着的 GUI 持着独占锁。
    /// 共享锁探测者之间不互斥——两个 CLI 或 CLI 与正在启动的 GUI 同时探,用独占锁探会把
    /// 对方的瞬时占用当成 GUI(Codex review 2026-09-23)
    fn app_lock_is_held(lock: &Path) -> std::io::Result<bool> {
        let probe = open_lock_file(&Self::app_path(lock))?;
        match probe.try_lock_shared() {
            Ok(()) => Ok(false),
            Err(TryLockError::WouldBlock) => Ok(true),
            Err(TryLockError::Error(e)) => Err(e),
        }
    }

    /// 等着拿(GUI 启动用,阻塞调用方,放在开窗之前):持有者是 CLI 写命令就隔 100ms
    /// 再试直到 `timeout`——它几秒到几十秒就完;是另一个 GUI 立刻放弃,它不会让。
    /// 用轮询不用阻塞的 `File::lock`:阻塞版认不出持有者中途从 CLI 换成了 GUI,
    /// 会无窗地挂死
    pub fn acquire_or_wait(db: &Path, kind: &str, timeout: Duration) -> std::io::Result<Wait> {
        let deadline = Instant::now() + timeout;
        loop {
            match Self::try_acquire(db, kind)? {
                Ownership::Ours(lock) => return Ok(Wait::Ours(lock)),
                Ownership::Held(holder) if holder.app => return Ok(Wait::HeldByApp(holder)),
                Ownership::Held(holder) if Instant::now() >= deadline => {
                    return Ok(Wait::TimedOut(holder))
                }
                Ownership::Held(_) => std::thread::sleep(Duration::from_millis(100)),
            }
        }
    }
}

/// `Store.path` 的形态:相对路径按 cwd 补成绝对——`--db wake.db` 这种相对目录不然会以相对
/// 形态拼进远程镜像的 file_path,换个 cwd 就读不到;**不解析符号链接**:远程镜像
/// `remotes/<host>` 挂在"你打开的那个路径"旁边,把库文件链到别的盘、链接留在默认位置的
/// 用户,镜像一直在默认目录里,解析了就找不到、下一轮扫描会把远程会话全判成已删(两条都是
/// 2026-09-23 Codex review)。锁文件另按 `canonical` 落在真实文件旁
fn anchored(db: &Path) -> PathBuf {
    std::path::absolute(db).unwrap_or_else(|_| db.to_path_buf())
}

/// 库的真实绝对路径:文件在就解析它本身;还不在(`index` 建库前、清过索引再启动)就
/// 顺着符号链接走到目标、解析目标的父目录再接文件名——`Store::open` 沿链接建库建在目标
/// 处,锁得提前落在那里;都解析不了原样用。锁文件落在哪、`open_or_rebuild` 挪哪个文件按它(`Store.path` 不按,见 `anchored`)
fn canonical(db: &Path) -> PathBuf {
    if let Ok(real) = db.canonicalize() {
        return strip_verbatim(real);
    }
    let mut target = db.to_path_buf();
    for _ in 0..8 {
        let Ok(link) = std::fs::read_link(&target) else {
            break;
        };
        target = match target.parent() {
            Some(dir) => dir.join(link),
            None => link,
        };
    }
    match (target.parent(), target.file_name()) {
        (Some(dir), Some(name)) => dir
            .canonicalize()
            .map(|dir| strip_verbatim(dir).join(name))
            .unwrap_or_else(|_| target.clone()),
        _ => target,
    }
}

/// Windows 的 `canonicalize` 给的是 `\\?\C:\…` 的 verbatim 形态;它存进 `Store.path`、
/// 派生成 `remotes/<host>` 根再与库里既有的 `C:\…` 形态的 file_path 按字符串前缀比,就
/// 全对不上(Codex review 2026-09-23)。盘符与 UNC 两种各剥回普通写法;其他平台原样
fn strip_verbatim(path: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        let text = path.to_string_lossy();
        if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
            return PathBuf::from(format!(r"\\{rest}"));
        }
        if let Some(rest) = text.strip_prefix(r"\\?\") {
            return PathBuf::from(rest);
        }
    }
    path
}

/// 这文件是不是 Wake 自己的索引:`wake-cli refresh --db` 指错到别家 SQLite(Cursor 的
/// state.vscdb、Hermes 的 state.db)时,`Store::open` 会对它改 journal_mode、建 Wake 的表——
/// 别家数据只读是铁律(Codex review 2026-09-23)。只读开、只看三张表在不在,老版本的 Wake
/// 索引照样认(refresh 要能升级它);打不开、不是 SQLite 都算不是
pub fn is_wake_index(path: &Path) -> bool {
    let Ok(conn) = open_conn_ro(path) else {
        return false;
    };
    ["schema_meta", "sessions", "messages_fts"]
        .iter()
        .all(|table| table_exists(&conn, table).unwrap_or(false))
}

/// "没有索引"这一句的唯一出处:只读旁路(wake-mcp / wake-cli 查询)与 `wake-cli refresh`
/// 说的是同一句,docs/cli.md 的排障条目照抄它
pub fn missing_index(path: &Path) -> String {
    format!(
        "no Wake index at {} — launch Wake once to build it",
        path.display()
    )
}

/// 读写分连接(WAL 单写多读);Connection 非 Sync,各自套 Mutex
pub struct Store {
    write: Mutex<Connection>,
    read: Mutex<Connection>,
    /// insights() 开临时连接用:统计要连扫 messages 几十毫秒,共用唯一
    /// 读连接会让 UI 线程的列表查询排队等它(2026-08-27 Codex review)
    path: std::path::PathBuf,
    /// `open_read_only` 建的实例:所有连接(含 insights 的临时连接)都只读,
    /// 写方法在运行期报 SQLITE_READONLY
    read_only: bool,
}

impl Store {
    /// 读写打开(建表、迁移)。**写真实索引的进程必须先持 `IndexLock`**,名单是封闭的:
    /// GUI(main.rs `hold_index_lock`)、`scanner::build_index` / `refresh_index`、
    /// scan bin——新写者照做。锁为什么不在这里拿,见 `IndexLock` 的注释
    pub fn open(path: &Path) -> Result<Self> {
        Ok(Self {
            write: Mutex::new(open_conn(path)?),
            read: Mutex::new(open_conn(path)?),
            path: anchored(path),
            read_only: false,
        })
    }

    /// 只读打开既有索引库(wake-mcp 这类旁路读者用):不建表、不迁移、不改
    /// journal_mode;库不存在或 schema 太老直接报错——重建权只归 GUI 的
    /// `open_or_rebuild`,旁路进程绝不能把正在被 GUI 写的库挪走重建
    /// (例外只有两个,都先拿 `IndexLock`:库不存在时 `scanner::build_index` 建一个,
    /// Wake 没开时 `scanner::refresh_index` 增量刷一轮)。
    /// WAL 下读者不阻塞 GUI 写入;只读连接要能建 -shm,同用户下目录可写即可
    pub fn open_read_only(path: &Path) -> Result<Self> {
        if !path.is_file() {
            anyhow::bail!("{}", missing_index(path));
        }
        let read = open_conn_ro(path)?;
        if !table_has_column(&read, NEWEST_COLUMN.0, NEWEST_COLUMN.1)?
            || !table_exists(&read, NEWEST_TABLE)?
        {
            anyhow::bail!(
                "Wake index at {} is empty or from an older version — launch Wake once to upgrade it",
                path.display()
            );
        }
        Ok(Self {
            // Store 的形状要求有 write 连接;这里给的同样是只读句柄
            write: Mutex::new(open_conn_ro(path)?),
            read: Mutex::new(read),
            path: anchored(path),
            read_only: true,
        })
    }

    /// 索引覆盖到的最新会话活动时间(epoch ms;空库 None)。wake-mcp 把它随
    /// 每次工具返回带给 agent,让对方知道索引有多新——GUI 没跑时 watcher 不在,
    /// 搜索/列表只反映到这个时刻(读会话是现场解析,不受影响)
    pub fn latest_activity(&self) -> Result<Option<i64>> {
        let conn = self.read.lock().unwrap();
        let latest: Option<i64> =
            conn.query_row("SELECT MAX(updated_at) FROM sessions", [], |r| r.get(0))?;
        Ok(latest.filter(|t| *t > 0))
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
        // 不经裁决的直写只有测试在用(生产路径全走 write_session_guarded),
        // 不带 Wake 调用记录
        write_session_tx(&tx, meta, file_mtime, units, &[])?;
        tx.commit()?;
        Ok(())
    }

    /// 增量写入的并发安全版:胜者比较与写入**同一事务**——先查后写分开时,
    /// 败方副本的事件能与全量扫描交错、后发落库,让 file_path 违背 mtime 裁决
    /// (2026-08-24 Codex review)。裁决与 scanner 枚举时的候选排序是同一把尺子:
    /// 先比副本所属实例的 `dedup_rank`(`rank_of` 按 file_path 给出,小者胜),
    /// 同级再比 mtime 新者、平局路径字典序——两条路径尺子不一,会话就会在两份
    /// 副本之间摇摆(2026-09-15 Codex review)。`supersedes` 是刚解析失败的那份
    /// 副本(路径 + 枚举时看到的 mtime,scanner 回退分支传入):库里这条 key 的行
    /// 若正是它——同路径,且 file_mtime 不晚于枚举时看到的 mtime(quick 阶段占位
    /// 的 0、早先入库后来损坏的旧版本都算)——它已不是竞争者,本次写入直接接管;
    /// 期间 watcher 已把修好的同路径文件成功入库(mtime 更新)则不让位,照常裁决。判定与写入必须
    /// 同一事务,先查再删再写会让并发写入的第三份副本被误删;单凭 file_mtime=0
    /// 推断占位也不行,真实文件同样可能给 0(Codex review 二、三、四轮)。
    /// 返回 false = 本次是败方副本,一字未写
    pub fn write_session_guarded(
        &self,
        meta: &SessionMeta,
        file_mtime: i64,
        units: &[IndexUnit],
        lookups: &[WakeLookup],
        rank_of: &dyn Fn(&str) -> u8,
        supersedes: Option<(&str, i64)>,
    ) -> Result<bool> {
        let mut conn = self.write.lock().unwrap();
        let tx = conn.transaction()?;
        // 被外壳产品认领的替身不入库。与写入同一事务:认领在并发间隙落地也挡得住
        if is_claimed(&tx, &meta.key)? {
            return Ok(false);
        }
        let cur: Option<(String, i64)> = tx
            .query_row(
                "SELECT file_path, file_mtime FROM sessions WHERE key = ?1",
                params![meta.key],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some((cur_path, cur_mtime)) = cur {
            // 让位只认"库里还是我们解析失败的那个版本或更旧的":同路径且行上的
            // file_mtime ≤ 枚举时看到的 mtime(quick 占位的 0 自然包含;早先入库、
            // 后来损坏的转录是"更旧");期间 watcher 已把修好的同路径文件成功入库
            //(mtime 更新)就不让位,照常裁决(Codex review 第四轮)
            let yields = supersedes
                .is_some_and(|(failed, seen_mtime)| failed == cur_path && cur_mtime <= seen_mtime);
            if cur_path != meta.file_path && !yields {
                let (cur_rank, new_rank) = (rank_of(&cur_path), rank_of(&meta.file_path));
                let loses = cur_rank < new_rank
                    || (cur_rank == new_rank
                        && (cur_mtime > file_mtime
                            || (cur_mtime == file_mtime
                                && cur_path.as_str() < meta.file_path.as_str())));
                if loses {
                    return Ok(false); // 事务未提交即弃
                }
            }
        }
        write_session_tx(&tx, meta, file_mtime, units, lookups)?;
        tx.commit()?;
        Ok(true)
    }

    pub fn write_meta_only(&self, metas: &[(SessionMeta, i64)]) -> Result<()> {
        let mut conn = self.write.lock().unwrap();
        let tx = conn.transaction()?;
        for (meta, mtime) in metas {
            // quick 路径也是写库路径,认领的替身同样挡在门外
            if !is_claimed(&tx, &meta.key)? {
                upsert_session(&tx, meta, *mtime)?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Sidecar-only updates must not replace transcript contents or steal a
    /// session from another copy. Changed files go through normal parsing;
    /// children inherit their project through replace_parent_links instead.
    /// A field the sidecar does not know (`None` / empty) is left as it is.
    pub(crate) fn update_sidecar_meta(
        &self,
        refs: &[SessionFileRef],
        updates: &HashMap<String, SidecarMeta>,
    ) -> Result<bool> {
        if updates.is_empty() {
            return Ok(false);
        }
        let mut conn = self.write.lock().unwrap();
        let tx = conn.transaction()?;
        let mut changed = false;
        for r in refs {
            let Some(update) = updates.get(&r.file_path) else {
                continue;
            };
            if let Some(project) = update.project.as_deref().filter(|p| !p.is_empty()) {
                let name = crate::adapters::parse_utils::project_name_of(project);
                changed |= tx.execute(
                    "UPDATE sessions SET project_path = ?1, project_name = ?2
                     WHERE file_path = ?3 AND file_mtime = ?4 AND file_size = ?5
                       AND parent_key = '' AND (project_path <> ?1 OR project_name <> ?2)",
                    params![project, name, r.file_path, r.mtime_ms, r.size],
                )? > 0;
            }
            if let Some(model) = update.model.as_deref().filter(|m| !m.is_empty()) {
                changed |= tx.execute(
                    "UPDATE sessions SET model = ?1
                     WHERE file_path = ?2 AND file_mtime = ?3 AND file_size = ?4
                       AND model IS NOT ?1",
                    params![model, r.file_path, r.mtime_ms, r.size],
                )? > 0;
            }
        }
        tx.commit()?;
        Ok(changed)
    }

    /// schema_meta 里的一次性旗子(升级后要补做的事),做完由 scanner 清掉
    fn has_meta_flag(&self, key: &str) -> bool {
        let conn = self.read.lock().unwrap();
        conn.query_row(
            "SELECT 1 FROM schema_meta WHERE key = ?1",
            params![key],
            |_| Ok(()),
        )
        .optional()
        .map(|value| value.is_some())
        .unwrap_or(false)
    }

    fn clear_meta_flag(&self, key: &str) -> Result<()> {
        let conn = self.write.lock().unwrap();
        conn.execute("DELETE FROM schema_meta WHERE key = ?1", params![key])?;
        Ok(())
    }

    /// 旧索引第一次升级到父子会话 schema 后，现有 Grok 行需要强制重解析，
    /// 才能把临时 worktree cwd 统一回主会话项目。
    pub fn needs_grok_parent_backfill(&self) -> bool {
        self.has_meta_flag("grok_parent_backfill")
    }

    pub fn finish_grok_parent_backfill(&self) -> Result<()> {
        self.clear_meta_flag("grok_parent_backfill")
    }

    /// FTS 派生规则换代后(见 `FTS_FORMAT`)全部文件要重解析一遍;由 scanner 在
    /// 一轮没有解析失败的扫描后清掉,失败过就留着下次再来
    pub fn needs_fts_reindex(&self) -> bool {
        self.has_meta_flag("fts_reindex")
    }

    pub fn finish_fts_reindex(&self) -> Result<()> {
        self.clear_meta_flag("fts_reindex")
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
        self.remove_sessions_recorded(keys, tombstone, &[], now_ms())
    }

    /// Keep the reviewed paths even if the watcher already removed a missing
    /// source row. File moves and watcher delivery cannot share a transaction.
    pub fn complete_cleanup(&self, sessions: &[SessionMeta], stamp: i64) -> Result<()> {
        let keys = sessions.iter().map(|s| s.key.clone()).collect::<Vec<_>>();
        self.remove_sessions_recorded(&keys, true, sessions, stamp)
    }

    fn remove_sessions_recorded(
        &self,
        keys: &[String],
        tombstone: bool,
        recorded: &[SessionMeta],
        stamp: i64,
    ) -> Result<()> {
        let mut conn = self.write.lock().unwrap();
        let tx = conn.transaction()?;
        for key in keys {
            if let Some(expected) = recorded.iter().find(|s| s.key == *key) {
                let sql=format!("SELECT {SESSION_COLS} FROM sessions s LEFT JOIN user_data u ON u.session_key = s.key WHERE s.key = ?1");
                if let Some(current) = tx.query_row(&sql, params![key], row_to_meta).optional()? {
                    anyhow::ensure!(
                        current.file_path == expected.file_path
                            && current.updated_at == expected.updated_at
                            && current.created_at == expected.created_at
                            && current.favorite == expected.favorite
                            && current.pinned == expected.pinned,
                        "Session changed before index update: {}",
                        current.title
                    );
                }
            }
            let file_path: Option<String> = tx
                .query_row(
                    "SELECT file_path FROM sessions WHERE key = ?1",
                    params![key],
                    |r| r.get(0),
                )
                .optional()?;
            delete_session_row(&tx, key)?;
            if tombstone {
                if let Some(fp) = file_path.or_else(|| {
                    recorded
                        .iter()
                        .find(|s| s.key == *key)
                        .map(|s| s.file_path.clone())
                }) {
                    tx.execute(
                        "INSERT OR REPLACE INTO tombstones(file_path, key, deleted_at) VALUES (?1, ?2, ?3)",
                        params![fp, key, stamp],
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

    /// Cleanup journals survive ordinary index rebuilds, like other user prefs.
    pub fn cleanup_journals(&self) -> Result<Vec<String>> {
        let conn = self.read.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT value FROM prefs WHERE key LIKE 'cleanup.batch.%' ORDER BY key DESC",
        )?;
        let rows = stmt.query_map([], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Fresh ownership metadata for inventory and each deletion check. Deliberately
    /// independent of the message index, which can contain millions of rows.
    pub fn cleanup_index(&self) -> Result<Vec<crate::cleanup::IndexedSession>> {
        let conn = self.read.lock().unwrap();
        let sql = format!("SELECT {SESSION_COLS}, s.parent_key FROM sessions s LEFT JOIN user_data u ON u.session_key = s.key");
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map([], |r| {
            Ok(crate::cleanup::IndexedSession {
                meta: row_to_meta(r)?,
                parent: r.get(19)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Display-only counts, collected once per cleanup inventory, never per tree
    /// during review or execution. Sidechain prompts do not belong to the mainline.
    pub fn cleanup_prompt_counts(&self) -> Result<std::collections::HashMap<String, i64>> {
        let conn = self.read.lock().unwrap();
        let mut stmt = conn.prepare("SELECT session_key, COUNT(*) FROM messages WHERE role = 'user' AND sidechain_id IS NULL GROUP BY session_key")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Check before moving restored files, then check again in the transaction.
    pub fn validate_cleanup_restore(&self, sessions: &[SessionMeta], stamp: i64) -> Result<()> {
        let conn = self.read.lock().unwrap();
        for s in sessions {
            let conflict: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM tombstones WHERE (key = ?1 OR file_path = ?2) AND deleted_at != ?3)", params![s.key, s.file_path, stamp], |r| r.get(0))?;
            anyhow::ensure!(!conflict, "A newer deletion protects {}", s.file_path);
        }
        Ok(())
    }

    /// Only undo the tombstones written by this cleanup, never later deletions.
    pub fn restore_cleanup(&self, sessions: &[SessionMeta], stamp: i64) -> Result<()> {
        let mut conn = self.write.lock().unwrap();
        let tx = conn.transaction()?;
        for s in sessions {
            let conflict: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM tombstones WHERE (key = ?1 OR file_path = ?2) AND deleted_at != ?3)", params![s.key, s.file_path, stamp], |r| r.get(0))?;
            anyhow::ensure!(!conflict, "A newer deletion protects {}", s.file_path);
            tx.execute(
                "DELETE FROM tombstones WHERE key = ?1 AND file_path = ?2 AND deleted_at = ?3",
                params![s.key, s.file_path, stamp],
            )?;
        }
        tx.commit()?;
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
            "DELETE FROM messages; DELETE FROM messages_fts; DELETE FROM titles_fts;
             DELETE FROM memories; DELETE FROM memories_fts; DELETE FROM wake_lookups;
             DELETE FROM sessions;",
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

    /// 被外壳产品认领的全部会话 key(见 claimed_sessions 表)。scanner 在枚举时一次
    /// 取齐,跳过这些文件的解析;写入闸门另在事务里逐条查
    pub fn claimed_keys(&self) -> Result<std::collections::HashSet<String>> {
        let conn = self.read.lock().unwrap();
        let mut stmt = conn.prepare_cached("SELECT key FROM claimed_sessions")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// 单条查询版,watcher 的增量路由用(与 is_key_tombstoned 同位置)
    pub fn is_key_claimed(&self, key: &str) -> bool {
        is_claimed(&self.read.lock().unwrap(), key).unwrap_or(false)
    }

    /// 库里持有认领的 agent。roster 里已经没有的认领方(location 全删、停用)要整组
    /// 撤销,否则它藏起来的替身就一直藏着
    pub fn claimants(&self) -> Result<Vec<AgentId>> {
        let conn = self.read.lock().unwrap();
        let mut stmt = conn.prepare_cached("SELECT DISTINCT claimant FROM claimed_sessions")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        let names: Vec<String> = rows.collect::<rusqlite::Result<_>>()?;
        Ok(names.iter().filter_map(|n| AgentId::from_str(n)).collect())
    }

    /// 用某个认领方的全量快照原子替换它的认领,并在同一事务里删掉已经入库的替身
    /// (认领之前扫进来的那些——清法与 remove_sessions 相同,不留墓碑:认领撤销后
    /// 替身要能回来)。返回库是否变了
    pub fn replace_claims(&self, claimant: AgentId, keys: &[String]) -> Result<bool> {
        let wanted: std::collections::BTreeSet<&str> = keys.iter().map(String::as_str).collect();
        let unchanged = |conn: &Connection| -> Result<bool> {
            let mut stmt = conn.prepare_cached(
                "SELECT key FROM claimed_sessions WHERE claimant = ?1 ORDER BY key",
            )?;
            let held: Vec<String> = stmt
                .query_map(params![claimant.as_str()], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<_>>()?;
            Ok(held.iter().map(String::as_str).eq(wanted.iter().copied()))
        };
        // 认领方每存一次盘、每轮扫描都会走到这里,而认领集合几乎从不变:在读连接上比一眼
        // 就回,不碰写锁。集合没变时库里也不会冒出替身——写入闸门一直挡着
        if unchanged(&self.read.lock().unwrap())? {
            return Ok(false);
        }
        let mut conn = self.write.lock().unwrap();
        let tx = conn.transaction()?;
        // 并发的另一轮可能刚写完同一份
        if unchanged(&tx)? {
            return Ok(false);
        }
        tx.execute(
            "DELETE FROM claimed_sessions WHERE claimant = ?1",
            params![claimant.as_str()],
        )?;
        {
            let mut insert = tx.prepare_cached(
                "INSERT OR IGNORE INTO claimed_sessions(key, claimant) VALUES (?1, ?2)",
            )?;
            for key in &wanted {
                insert.execute(params![key, claimant.as_str()])?;
            }
        }
        let indexed: Vec<String> = tx
            .prepare_cached(
                "SELECT s.key FROM sessions s JOIN claimed_sessions c ON c.key = s.key
                 WHERE c.claimant = ?1",
            )?
            .query_map(params![claimant.as_str()], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<_>>()?;
        for key in &indexed {
            delete_session_row(&tx, key)?;
        }
        tx.commit()?;
        Ok(true)
    }

    pub fn list_sessions(&self, f: &SessionFilter) -> Result<(Vec<SessionMeta>, i64)> {
        let mut wheres: Vec<String> = Vec::new();
        let mut args: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

        // 根会话的"活动时间"是连子会话一起聚合的(排序、返回值都如此),since
        // 过滤必须用同一口径——只看父行会把"父旧子新"的整棵树滤掉
        let updated_col = match (f.roots_only, f.include_archived) {
            (true, false) => ROOT_UPDATED_ACTIVE,
            (true, true) => ROOT_UPDATED_ALL,
            _ => "s.updated_at",
        };
        push_session_filters(f, updated_col, &mut wheres, &mut args);
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
        let pin_order = pin_order(f);

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
             ORDER BY {pin_order}{order_col} {order_dir}, s.key ASC LIMIT ? OFFSET ?"
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
        let pin_order = pin_order(f);
        let conn = self.read.lock().unwrap();
        let sql = format!(
            "SELECT {SESSION_COLS}
             FROM sessions s LEFT JOIN user_data u ON u.session_key = s.key
             {where_sql}
             ORDER BY {pin_order}{order_col} {order_dir}, s.key ASC"
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
            .prepare_cached("SELECT NULLIF(parent_key, '') FROM sessions WHERE key = ?1")?
            .query_row(params![key], |row| row.get(0))
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

    /// 按原生 id 反查(wake-mcp 的 key 兜底:对方常只拿到 resume 用的那个 id)。
    /// 同 UUID 在多台 host 各续跑过会多于一行,由调用方裁决
    pub fn find_by_native_id(&self, native_id: &str) -> Result<Vec<SessionMeta>> {
        let conn = self.read.lock().unwrap();
        let sql = format!(
            "SELECT {SESSION_COLS} FROM sessions s LEFT JOIN user_data u ON u.session_key = s.key
             WHERE s.native_id = ?1 ORDER BY s.key"
        );
        let mut stmt = conn.prepare_cached(&sql)?;
        let rows = stmt.query_map(params![native_id], row_to_meta)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// 项目清单(根会话按 project_path 聚合)。GUI 侧栏与 wake_list_projects 传
    /// false 只算未归档;wake-mcp 解析 `project` 参数时传 true——搜索本身覆盖
    /// 归档会话,只剩归档会话的项目不能在解析这一步就被挡掉
    pub fn list_projects(&self, include_archived: bool) -> Result<Vec<ProjectInfo>> {
        let (root_updated, root_cond, archived) = if include_archived {
            (ROOT_UPDATED_ALL, ROOT_IGNORING_ARCHIVED, "")
        } else {
            (
                ROOT_UPDATED_ACTIVE,
                ROOT_WHEN_HIDING_ARCHIVED,
                "s.archived = 0 AND ",
            )
        };
        let conn = self.read.lock().unwrap();
        let mut stmt = conn.prepare_cached(&format!(
            "SELECT s.project_path, s.project_name, COUNT(*), MAX({root_updated}) AS activity
             FROM sessions s
             WHERE {archived}{root_cond}
             GROUP BY s.project_path ORDER BY activity DESC"
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
            // archived 过滤与 agent_counts/list_projects 同口径,徽标数 = 点开后可见数
            "SELECT COUNT(*) FROM user_data u JOIN sessions s ON s.key = u.session_key WHERE u.favorite = 1 AND s.archived = 0",
            [],
            |r| r.get(0),
        )?)
    }

    // ---------- 记忆(agent 写的 Markdown,只读镜像) ----------

    /// 一组 (agent, host) 的记忆整体对齐到 `docs`:多出的删、变了的换(按 updated_at、
    /// size 与归属锚点 / 项目判,正文不比)、没变的不动;返回有没有改动。同 key
    /// 重复后者胜。这是 memories / memories_fts 的**唯一写点**(rebuild_all 只清空)
    pub fn replace_memories(
        &self,
        agent: AgentId,
        host: &str,
        docs: &[MemoryDoc],
        frozen: &[String],
    ) -> Result<bool> {
        let mut conn = self.write.lock().unwrap();
        let tx = conn.transaction()?;
        // key → (id, 指纹);指纹 = (updated_at, size, session_key, project_path, source, title):
        // 锚点换了(目录里来了更新的会话)同样要重写,否则项目归属定格在首次入库那一刻;
        // 来源换了(同一文件从自定义变成默认)也重写;标题也在指纹里——标题是从文件派生
        // 的,派生规则换了(2026-09-21 改成文件名带扩展名)文件没变也要跟上,正文不比
        type Stamp = (i64, i64, String, String, String, String);
        let existing: HashMap<String, (i64, Stamp)> = {
            let mut stmt = tx.prepare_cached(
                "SELECT key, id, updated_at, size_bytes, session_key, project_path, source, title
                 FROM memories WHERE agent_id = ?1 AND host = ?2",
            )?;
            let rows = stmt.query_map(params![agent.as_str(), host], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    (
                        r.get::<_, i64>(1)?,
                        (
                            r.get(2)?,
                            r.get(3)?,
                            r.get(4)?,
                            r.get(5)?,
                            r.get(6)?,
                            r.get(7)?,
                        ),
                    ),
                ))
            })?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        let unchanged = |s: &Stamp, d: &MemoryDoc| {
            s.0 == d.updated_at
                && s.1 == d.size_bytes
                && s.2 == d.session_key
                && s.3 == d.project_path
                && s.4 == d.source
                && s.5 == d.title
        };
        // 同 key 重复**前者胜**:docs 按来源计划的顺序读入(默认来源 → 项目模式 → 自定义,
        // 默认实例 → 自定义实例),先到的是默认那份——自定义目录盖住默认文件(加 ~/.claude
        // 当自定义目录,里面的 CLAUDE.md 已是默认来源)时计数与开关才落在默认行上,而不是
        // 默认行显示 0 份、关掉也不生效(2026-09-22 review)。先按 key 收成一份再写,不然
        // 写了第一份、第二份与库里相同被跳过,胜负就看谁先到库里
        let mut latest: std::collections::BTreeMap<&str, &MemoryDoc> =
            std::collections::BTreeMap::new();
        for d in docs {
            latest.entry(d.key.as_str()).or_insert(d);
        }
        let mut changed = false;
        for d in latest.values() {
            if existing.get(&d.key).is_some_and(|(_, s)| unchanged(s, d)) {
                continue;
            }
            upsert_memory(&tx, d)?;
            changed = true;
        }
        // `frozen` 是这一轮读失败的实例的来源 id:那些来源的行不删(它们没出现在 docs
        // 里只是因为读不出,不是文件没了)
        let mut stmt_fts = tx.prepare_cached("DELETE FROM memories_fts WHERE rowid = ?1")?;
        let mut stmt_row = tx.prepare_cached("DELETE FROM memories WHERE key = ?1")?;
        for (key, (id, stamp)) in existing.iter() {
            if latest.contains_key(key.as_str()) || frozen.contains(&stamp.4) {
                continue;
            }
            stmt_fts.execute(params![id])?;
            stmt_row.execute(params![key])?;
            changed = true;
        }
        drop(stmt_fts);
        drop(stmt_row);
        tx.commit()?;
        Ok(changed)
    }

    /// Settings → Memory locations 的计数:每个 (agent, 来源 id) 有多少份
    pub fn memory_source_counts(&self) -> Result<HashMap<(String, String), i64>> {
        let conn = self.read.lock().unwrap();
        let mut stmt =
            conn.prepare_cached("SELECT agent_id, source, COUNT(*) FROM memories GROUP BY 1, 2")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                (r.get::<_, String>(0)?, r.get::<_, String>(1)?),
                r.get::<_, i64>(2)?,
            ))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// 已索引的本地项目根(host 为空、路径非空),给记忆的项目模式(`<project>/CLAUDE.md`
    /// 一类)展开;远程会话的项目是别的机器上的路径,不在其中
    pub fn local_project_roots(&self) -> Result<Vec<PathBuf>> {
        let conn = self.read.lock().unwrap();
        let mut stmt = conn.prepare_cached(
            "SELECT DISTINCT project_path FROM sessions
             WHERE host = '' AND project_path != '' ORDER BY project_path",
        )?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        Ok(rows
            .collect::<rusqlite::Result<Vec<_>>>()?
            .into_iter()
            .map(PathBuf::from)
            .collect())
    }

    // ---------- 记忆来源配置(Settings → Memory locations) ----------

    /// 与 Session locations 同一层级的用户数据:自定义来源(目录或文件)与被停用的
    /// 默认来源(按 `MemorySource::id`),重扫不动它。对外只有 `memory_source_overrides`
    /// 一次取齐
    fn list_memory_sources_custom(&self) -> Result<Vec<(String, String)>> {
        let conn = self.read.lock().unwrap();
        let mut stmt = conn.prepare_cached(
            "SELECT agent, path FROM memory_sources_custom ORDER BY added_at, path",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn add_memory_source(&self, agent: &str, path: &str) -> Result<()> {
        let conn = self.write.lock().unwrap();
        insert_memory_source(&conn, agent, path)
    }

    /// 删自定义来源,连同它的停用记录(日后重新添加同一路径要按启用开始)
    pub fn remove_memory_source(&self, agent: &str, path: &str) -> Result<()> {
        let mut conn = self.write.lock().unwrap();
        let tx = conn.transaction()?;
        delete_memory_source(&tx, agent, path)?;
        tx.commit()?;
        Ok(())
    }

    /// 编辑自定义来源:删旧记新,一个事务(半程失败不得半生效)
    pub fn replace_memory_source(
        &self,
        old_agent: &str,
        old_path: &str,
        new_agent: &str,
        new_path: &str,
    ) -> Result<()> {
        let mut conn = self.write.lock().unwrap();
        let tx = conn.transaction()?;
        delete_memory_source(&tx, old_agent, old_path)?;
        insert_memory_source(&tx, new_agent, new_path)?;
        tx.commit()?;
        Ok(())
    }

    fn list_memory_sources_disabled(&self) -> Result<Vec<(String, String)>> {
        let conn = self.read.lock().unwrap();
        let mut stmt = conn.prepare_cached(
            "SELECT agent, source FROM memory_sources_disabled ORDER BY disabled_at, source",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// 来源开关的唯一写入口。enabled=true 删停用标记;false 幂等记入
    pub fn set_memory_source_enabled(
        &self,
        agent: &str,
        source: &str,
        enabled: bool,
    ) -> Result<()> {
        let conn = self.write.lock().unwrap();
        if enabled {
            conn.execute(
                "DELETE FROM memory_sources_disabled WHERE agent = ?1 AND source = ?2",
                params![agent, source],
            )?;
        } else {
            conn.execute(
                "INSERT OR IGNORE INTO memory_sources_disabled(agent, source, disabled_at)
                 VALUES (?1, ?2, ?3)",
                params![agent, source, now_ms()],
            )?;
        }
        Ok(())
    }

    /// Restore defaults:清空自定义来源与停用记录
    pub fn clear_memory_source_overrides(&self) -> Result<()> {
        let conn = self.write.lock().unwrap();
        conn.execute_batch(
            "DELETE FROM memory_sources_custom;
             DELETE FROM memory_sources_disabled;",
        )?;
        Ok(())
    }

    /// 记忆来源配置一次取齐,解析成模型层类型;未识别的 agent 名静默跳过。停用表给
    /// 集合——调用方都是按 (agent, 来源 id) 查它。scanner 与 Settings 共用。读失败原样
    /// 报 Err:scanner 拿它做"按缺席删",折成"什么都没配置"会删光项目指令文件、把停用的
    /// 来源重新索引进来(2026-09-22 review);Settings 只是显示,自己 unwrap_or_default
    pub fn memory_source_overrides(
        &self,
    ) -> Result<(
        Vec<(AgentId, PathBuf)>,
        std::collections::HashSet<(AgentId, String)>,
    )> {
        let customs = self
            .list_memory_sources_custom()?
            .into_iter()
            .filter_map(|(a, p)| AgentId::from_str(&a).map(|a| (a, PathBuf::from(p))))
            .collect();
        let disabled = self
            .list_memory_sources_disabled()?
            .into_iter()
            .filter_map(|(a, s)| AgentId::from_str(&a).map(|a| (a, s)))
            .collect();
        Ok((customs, disabled))
    }

    /// 库里现有记忆的 (agent, host) 分组——scanner 拿它对照当前 roster,不再配置的来源
    /// (删掉的远程 host、停用的 agent)整组清掉
    pub fn memory_groups(&self) -> Result<Vec<(AgentId, String)>> {
        let conn = self.read.lock().unwrap();
        let mut stmt = conn.prepare_cached("SELECT DISTINCT agent_id, host FROM memories")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        let mut out = Vec::new();
        for r in rows {
            let (agent, host) = r?;
            if let Some(agent) = AgentId::from_str(&agent) {
                out.push((agent, host));
            }
        }
        Ok(out)
    }

    /// Memory 页侧栏的计数:agent 按 AgentId 声明序(与会话侧栏同序,计数平局不抖),
    /// 项目按最近更新降序、没归属的一项(path 为空)垫底。项目按读时解析的归属算,
    /// 与 list_memories 同一口径:**项目行只数项目记忆**(GUI 的项目行喂
    /// `UserMemories::Excluded`),用户级另给 `user`——曾把用户级加进每个项目行
    /// (2026-09-21 review),用户 2026-09-22 定用户记忆不进项目、自成一行;两种口径
    /// 下都是点开看到多少行、徽章就是多少
    pub fn memory_counts(&self) -> Result<MemoryCounts> {
        let conn = self.read.lock().unwrap();
        let mut stmt = conn.prepare_cached(&format!(
            "SELECT m.agent_id, {MEMORY_PROJECT},
                    COALESCE(NULLIF(m.project_name, ''), s.project_name, ''),
                    m.scope, COUNT(*), MAX(m.updated_at)
             FROM memories m {MEMORY_SESSION_JOIN}
             GROUP BY 1, 2, 3, 4"
        ))?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, i64>(5)?,
            ))
        })?;
        let mut counts = MemoryCounts::default();
        let mut agents: HashMap<AgentId, i64> = HashMap::new();
        let mut projects: HashMap<String, MemoryProject> = HashMap::new();
        let mut user_level = 0i64;
        for r in rows {
            let (agent, path, name, scope, n, last) = r?;
            let Some(agent) = AgentId::from_str(&agent) else {
                continue;
            };
            counts.total += n;
            *agents.entry(agent).or_default() += n;
            if scope == MemoryScope::User.as_str() {
                user_level += n;
                continue;
            }
            let p = projects
                .entry(path.clone())
                .or_insert_with(|| MemoryProject {
                    path,
                    name,
                    count: 0,
                    updated_at: 0,
                });
            p.count += n;
            p.updated_at = p.updated_at.max(last);
        }
        counts.user = user_level;
        counts.agents = agents.into_iter().collect();
        counts.agents.sort_by_key(|(agent, _)| *agent);
        counts.projects = projects.into_values().collect();
        counts.projects.sort_by(|a, b| {
            a.path
                .is_empty()
                .cmp(&b.path.is_empty())
                .then_with(|| b.updated_at.cmp(&a.updated_at))
                .then_with(|| a.path.cmp(&b.path))
        });
        Ok(counts)
    }

    /// 记忆列表:项目级按项目路径成组、组内新到旧,没归属的一组排在项目之后,
    /// 用户级(对每个项目都成立)最后;项目筛选下用户级照列(`f.user` 默认 Alongside,
    /// GUI 的项目行 Excluded、User memory 行 Only)。**`limit` 只封项目级**:
    /// 用户级排在最后,一刀切的 LIMIT 砍掉的正是筛选特意放进来的那几份(2026-09-21
    /// review),所以项目级带 LIMIT 查、用户级不限量另查再接上。**行里 body 是空串**
    /// (`MEMORY_LIST_COLS`),要正文走 `get_memory`
    pub fn list_memories(&self, f: &MemoryFilter) -> Result<Vec<MemoryDoc>> {
        let (filter_sql, args) = memory_filter(f);
        let user = MemoryScope::User.as_str();
        let conn = self.read.lock().unwrap();
        let order = format!(
            "ORDER BY m.scope = '{user}', {MEMORY_PROJECT} = '', {MEMORY_PROJECT},
                      m.updated_at DESC, m.key"
        );
        if f.limit <= 0 {
            let mut stmt = conn.prepare_cached(&format!(
                "SELECT {MEMORY_LIST_COLS} FROM memories m {MEMORY_SESSION_JOIN}
                 WHERE 1 = 1{filter_sql} {order}"
            ))?;
            let rows = stmt.query_map(
                rusqlite::params_from_iter(args.iter().map(|b| b.as_ref())),
                row_to_memory,
            )?;
            return Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?);
        }
        let mut out = {
            let mut stmt = conn.prepare_cached(&format!(
                "SELECT {MEMORY_LIST_COLS} FROM memories m {MEMORY_SESSION_JOIN}
                 WHERE m.scope != '{user}'{filter_sql} {order} LIMIT ?"
            ))?;
            let limit: Box<dyn rusqlite::ToSql> = Box::new(f.limit);
            let rows = stmt.query_map(
                rusqlite::params_from_iter(
                    args.iter()
                        .chain(std::iter::once(&limit))
                        .map(|b| b.as_ref()),
                ),
                row_to_memory,
            )?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        let mut stmt = conn.prepare_cached(&format!(
            "SELECT {MEMORY_LIST_COLS} FROM memories m {MEMORY_SESSION_JOIN}
             WHERE m.scope = '{user}'{filter_sql} {order}"
        ))?;
        let rows = stmt.query_map(
            rusqlite::params_from_iter(args.iter().map(|b| b.as_ref())),
            row_to_memory,
        )?;
        out.extend(rows.collect::<rusqlite::Result<Vec<_>>>()?);
        Ok(out)
    }

    pub fn get_memory(&self, key: &str) -> Result<Option<MemoryDoc>> {
        let conn = self.read.lock().unwrap();
        let mut stmt = conn.prepare_cached(&format!(
            "SELECT {MEMORY_COLS} FROM memories m {MEMORY_SESSION_JOIN} WHERE m.key = ?1"
        ))?;
        Ok(stmt.query_row(params![key], row_to_memory).optional()?)
    }

    /// 记忆全文搜索(标题 + 正文),与会话搜索同一套分词与 <3 码点降级规则;命中按
    /// bm25 排、snippet 取自正文。筛选同 list_memories
    pub fn search_memories(&self, q: &str, f: &MemoryFilter) -> Result<Vec<MemoryHit>> {
        let segs = fts_terms(q);
        if segs.is_empty() {
            return Ok(Vec::new());
        }
        let limit = if f.limit > 0 { f.limit } else { 20 };
        let (filter_sql, filter_args) = memory_filter(f);
        let conn = self.read.lock().unwrap();
        let mut out = Vec::new();
        if !needs_like_fallback(&segs) {
            let mut stmt = conn.prepare_cached(&format!(
                "SELECT {MEMORY_COLS}, snippet(memories_fts, 1, ?, ?, '…', 16)
                 FROM memories_fts JOIN memories m ON m.id = memories_fts.rowid
                 {MEMORY_SESSION_JOIN}
                 WHERE memories_fts MATCH ?{filter_sql}
                 ORDER BY bm25(memories_fts) LIMIT ?"
            ))?;
            let mut all_args: Vec<Box<dyn rusqlite::ToSql>> = vec![
                Box::new(HL_OPEN.to_string()),
                Box::new(HL_CLOSE.to_string()),
                Box::new(fts_match_expr(&segs)),
            ];
            all_args.extend(filter_args);
            all_args.push(Box::new(limit));
            let rows = stmt.query_map(
                rusqlite::params_from_iter(all_args.iter().map(|b| b.as_ref())),
                |r| {
                    Ok(MemoryHit {
                        doc: row_to_memory(r)?,
                        // snippet 是 SELECT 里跟在 memory_cols! 后面的最后一列,按列数取——写死下标加列时
                        // 顶错过一次(2026-09-21)
                        snippet: r.get(r.as_ref().column_count() - 1)?,
                    })
                },
            )?;
            for r in rows {
                out.push(r?);
            }
        } else {
            let like_where = like_where(&["m.title", "m.body"], segs.len());
            let mut stmt = conn.prepare_cached(&format!(
                "SELECT {MEMORY_COLS} FROM memories m {MEMORY_SESSION_JOIN}
                 WHERE {like_where}{filter_sql}
                 ORDER BY m.updated_at DESC LIMIT ?"
            ))?;
            let mut all_args = like_args(&segs, 2);
            all_args.extend(filter_args);
            all_args.push(Box::new(limit));
            let rows = stmt.query_map(
                rusqlite::params_from_iter(all_args.iter().map(|b| b.as_ref())),
                row_to_memory,
            )?;
            for r in rows {
                let doc = r?;
                let snippet = make_like_snippet(&doc.body, segs[0]);
                out.push(MemoryHit { doc, snippet });
            }
        }
        Ok(out)
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
             WHERE s.archived = 0 AND m.role = 'user' AND m.sidechain_id IS NULL
               AND s.key IN canonical_sessions";

        // 临时连接,不与 UI 的 read 连接抢锁:WAL 多读并发,几十毫秒的
        // 统计扫描不该让导航点击的列表查询排队(2026-08-27 Codex review)
        let conn = if self.read_only {
            open_conn_ro(&self.path)?
        } else {
            open_conn(&self.path)?
        };
        // 同一会话在多台 host 上各有一份镜像时(两个远程 host 指向同一台机器,
        // 或本地与远程都有)只算一次:按 (agent, native_id, created_at) 认同一会话
        // ——native_id 单独不够(Hermes 的 id 是小整数,两台机器的 1 号会话不是
        // 同一个),created_at 出自文件内容,同 id 同起点即同一会话;没有 created_at
        // 的不合并。取更新时间最新的那份(最完整),平局按 host 名(本地 '' 最小)。
        // scanner 层有意保留全部副本(不变量 8⑦:各机续跑会分叉),只是统计口径
        // 不能把一份对话数两遍(2026-09-14,0.4.0 遗留)。算一次放临时表——这条
        // 连接是本次调用私有的,只读连接也能建 temp 表;内联成子查询的话下面
        // 七条语句会各排一遍序
        conn.execute_batch(
            "CREATE TEMP TABLE canonical_sessions AS
             SELECT key FROM (
                 SELECT key, ROW_NUMBER() OVER (
                     PARTITION BY agent_id, native_id, created_at,
                                  CASE WHEN created_at > 0 THEN '' ELSE key END
                     ORDER BY updated_at DESC, host) AS rn
                 FROM sessions WHERE archived = 0) WHERE rn = 1",
        )?;
        let mut data = InsightsData {
            as_of: today,
            // Agents asking Wake:窗同 Last 7 days,按调用时刻归窗;与其余语句同一
            // 连接、同一张 canonical_sessions
            wake_lookups_7d: tally_wake_lookups(&conn, week_window_start_ms(today))?,
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
             FROM sessions WHERE archived = 0 AND key IN canonical_sessions",
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
                 FROM sessions s WHERE s.archived = 0 AND s.key IN canonical_sessions{filter}
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
               AND key IN canonical_sessions
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
        self.search_with(
            q,
            &SearchFilter {
                agents: agents.to_vec(),
                project_paths: project_path
                    .map(|p| vec![p.to_string()])
                    .unwrap_or_default(),
                updated_since: None,
                limit,
            },
        )
    }

    /// 全文搜索的完整筛选形态(多项目并集 + 时间下界);`search` 是它的便捷壳。
    /// 命中按 bm25 排序、消息级一行一条;archived 会话不排除(meta.archived 标出)
    pub fn search_with(&self, q: &str, f: &SearchFilter) -> Result<(Vec<SearchHit>, bool)> {
        let segs = fts_terms(q);
        if segs.is_empty() {
            return Ok((Vec::new(), false));
        }
        let degraded = needs_like_fallback(&segs);
        let limit = if f.limit > 0 { f.limit } else { 60 };
        // 近因加权:bm25 是负数、越小越相关,乘上 (1 + W / (1 + 距今天数 / H)) 让
        // 新会话的命中"更负"。W = 1、H = 30 天:今天的会话 ×2、一个月前 ×1.5、
        // 一年前 ×1.08——同等文本相关性下新会话在前,文本相关性差一倍以上的老命中
        // 仍然赢得过。按会话的 updated_at 而不是消息自己的 ts:排的是"该翻哪场
        // 对话",同一会话内各命中的相对顺序不受影响。只用四则运算与 max(),不依赖
        // SQLite 的数学扩展;`?` 是当前时间(毫秒)。降级的 LIKE 路径本来就按时间排
        const RECENCY_BOOST: &str = "(1.0 + 1.0 / (1.0 + max(0, ? - s.updated_at) / 2592000000.0))";
        let now = now_ms();

        // 会话侧筛选(agent / 项目并集 / 时间下界):SQL 片段拼一次,参数按需重建
        // ——正文与标题是两条查询,Box<dyn ToSql> 不能 clone
        let mut filter_sql = String::new();
        if !f.agents.is_empty() {
            filter_sql.push_str(&format!(
                " AND s.agent_id IN ({})",
                placeholders(f.agents.len())
            ));
        }
        if !f.project_paths.is_empty() {
            filter_sql.push_str(&format!(
                " AND s.project_path IN ({})",
                placeholders(f.project_paths.len())
            ));
        }
        if f.updated_since.is_some() {
            filter_sql.push_str(" AND s.updated_at >= ?");
        }
        let filter_args = |f: &SearchFilter| -> Vec<Box<dyn rusqlite::ToSql>> {
            let mut v: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
            for a in &f.agents {
                v.push(Box::new(a.as_str().to_string()));
            }
            for p in &f.project_paths {
                v.push(Box::new(p.clone()));
            }
            if let Some(t) = f.updated_since {
                v.push(Box::new(t));
            }
            v
        };
        // FTS 的 MATCH 表达式(正文与标题两张表共用)
        let match_expr = fts_match_expr(&segs);

        let conn = self.read.lock().unwrap();
        let mut raw: Vec<(String, i64, Option<String>, String, Option<i64>, String)> = Vec::new();

        if !degraded {
            let sql = format!(
                "SELECT m.session_key, m.seq, m.sidechain_id, m.role, m.ts,
                        snippet(messages_fts, 0, ?, ?, '…', 16)
                 FROM messages_fts
                 JOIN messages m ON m.id = messages_fts.rowid
                 JOIN sessions s ON s.key = m.session_key
                 WHERE messages_fts MATCH ?{filter_sql}
                 ORDER BY bm25(messages_fts) * {RECENCY_BOOST} LIMIT ?"
            );
            let mut stmt = conn.prepare_cached(&sql)?;
            let mut all_args: Vec<Box<dyn rusqlite::ToSql>> = vec![
                Box::new(HL_OPEN.to_string()),
                Box::new(HL_CLOSE.to_string()),
                Box::new(match_expr.clone()),
            ];
            all_args.extend(filter_args(f));
            all_args.push(Box::new(now));
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
            let like_where = like_where(&["m.text"], segs.len());
            let sql = format!(
                "SELECT m.session_key, m.seq, m.sidechain_id, m.role, m.ts, m.text
                 FROM messages m JOIN sessions s ON s.key = m.session_key
                 WHERE {like_where}{filter_sql}
                 ORDER BY m.ts DESC LIMIT ?"
            );
            let mut stmt = conn.prepare_cached(&sql)?;
            let mut all_args = like_args(&segs, 1);
            all_args.extend(filter_args(f));
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

        // 标题命中:独立的 titles_fts。排在正文命中之前——词出现在标题里是最强的
        // 相关性信号;role 记 "title"、seq 记 0,跳转落到会话开头(标题没有对应的
        // 转录行,不伪造 seq)
        let mut title_raw: Vec<(String, String)> = Vec::new();
        if !degraded {
            let sql = format!(
                "SELECT t.key, highlight(titles_fts, 1, ?, ?)
                 FROM titles_fts t JOIN sessions s ON s.key = t.key
                 WHERE titles_fts MATCH ?{filter_sql}
                 ORDER BY bm25(titles_fts) * {RECENCY_BOOST} LIMIT ?"
            );
            let mut stmt = conn.prepare_cached(&sql)?;
            let mut all_args: Vec<Box<dyn rusqlite::ToSql>> = vec![
                Box::new(HL_OPEN.to_string()),
                Box::new(HL_CLOSE.to_string()),
                Box::new(match_expr),
            ];
            all_args.extend(filter_args(f));
            all_args.push(Box::new(now));
            all_args.push(Box::new(limit));
            let rows = stmt.query_map(
                rusqlite::params_from_iter(all_args.iter().map(|b| b.as_ref())),
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )?;
            for r in rows {
                title_raw.push(r?);
            }
        } else {
            let like_where = like_where(&["s.title"], segs.len());
            let sql = format!(
                "SELECT s.key, s.title FROM sessions s
                 WHERE {like_where}{filter_sql}
                 ORDER BY s.updated_at DESC LIMIT ?"
            );
            let mut stmt = conn.prepare_cached(&sql)?;
            let mut all_args = like_args(&segs, 1);
            all_args.extend(filter_args(f));
            all_args.push(Box::new(limit));
            let rows = stmt.query_map(
                rusqlite::params_from_iter(all_args.iter().map(|b| b.as_ref())),
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )?;
            for r in rows {
                let (key, title) = r?;
                title_raw.push((key, make_like_snippet(&title, segs[0])));
            }
        }

        // 补齐 session meta:按会话只查一次(一个会话常占几十行命中),语句走
        // prepare_cached——Connection::query_row 每次都是裸 prepare
        let mut hits = Vec::new();
        let mut metas: HashMap<String, Option<SessionMeta>> = HashMap::new();
        let sql = format!(
            "SELECT {SESSION_COLS} FROM sessions s LEFT JOIN user_data u ON u.session_key = s.key WHERE s.key = ?1"
        );
        let mut stmt = conn.prepare_cached(&sql)?;
        let mut hydrate = |key: &str| -> Result<Option<SessionMeta>> {
            if let Some(cached) = metas.get(key) {
                return Ok(cached.clone());
            }
            let meta = stmt.query_row(params![key], row_to_meta).optional()?;
            metas.insert(key.to_string(), meta.clone());
            Ok(meta)
        };
        for (key, snippet) in title_raw {
            if let Some(session) = hydrate(&key)? {
                let timestamp = Some(session.updated_at);
                hits.push(SearchHit {
                    session,
                    seq: 0,
                    sidechain_id: None,
                    role: "title".to_string(),
                    snippet,
                    timestamp,
                });
            }
        }
        for (key, seq, sidechain_id, role, ts, snippet) in raw {
            if let Some(session) = hydrate(&key)? {
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
    push_session_filters(filter, "s.updated_at", &mut wheres, &mut args);
    (format!("WHERE {}", wheres.join(" AND ")), args)
}

/// ORDER BY 里置顶优先的那一段;`ignore_pins` 时为空,纯按排序键
fn pin_order(f: &SessionFilter) -> &'static str {
    if f.ignore_pins {
        ""
    } else {
        "COALESCE(u.pinned,0) DESC, "
    }
}

/// `?,?,?` 占位串
fn placeholders(n: usize) -> String {
    vec!["?"; n].join(",")
}

/// SessionFilter 里作用于会话行本身的谓词(agents / 项目并集 / 时间下界 /
/// 收藏 / 归档 / 标题词)。list_sessions 与 child_filter_sql 共用这一份,
/// roots_only 与 parent 谓词由各自追加——新增筛选字段只改这里(再教会
/// workbench 的两面镜子,见 models.rs SessionFilter 注释)。
/// `updated_col` 是时间下界作用的表达式:根会话列表传聚合后的活动时间
fn push_session_filters(
    f: &SessionFilter,
    updated_col: &str,
    wheres: &mut Vec<String>,
    args: &mut Vec<Box<dyn rusqlite::ToSql>>,
) {
    if !f.agents.is_empty() {
        wheres.push(format!("s.agent_id IN ({})", placeholders(f.agents.len())));
        for a in &f.agents {
            args.push(Box::new(a.as_str().to_string()));
        }
    }
    if !f.project_paths.is_empty() {
        wheres.push(format!(
            "s.project_path IN ({})",
            placeholders(f.project_paths.len())
        ));
        for p in &f.project_paths {
            args.push(Box::new(p.clone()));
        }
    }
    if let Some(t) = f.updated_since {
        wheres.push(format!("{updated_col} >= ?"));
        args.push(Box::new(t));
    }
    if f.favorite_only {
        wheres.push("COALESCE(u.favorite, 0) = 1".into());
    }
    if !f.include_archived {
        wheres.push("s.archived = 0".into());
    }
    if let Some(q) = f
        .title_query
        .as_deref()
        .map(str::trim)
        .filter(|q| !q.is_empty())
    {
        wheres.push("(s.title LIKE ? ESCAPE '\\' OR s.project_name LIKE ? ESCAPE '\\')".into());
        let like = format!("%{}%", escape_like(q));
        args.push(Box::new(like.clone()));
        args.push(Box::new(like));
    }
}

/// memories 连它的归属会话:项目路径在读时解析——adapter 填了就用 adapter 的,
/// 否则取 `session_key` 指向的会话的(线程级记忆是它所属的会话,Claude 项目级是
/// 同目录最新的会话)。归属随索引走,会话被重新归属、晚于记忆才入库都自动跟上
const MEMORY_SESSION_JOIN: &str = "LEFT JOIN sessions s ON s.key = m.session_key";
const MEMORY_PROJECT: &str = "COALESCE(NULLIF(m.project_path, ''), s.project_path, '')";

/// memories 的列(带 `m.` 前缀:搜索要与 memories_fts 连表,裸 key 会歧义);
/// 必须带上 `MEMORY_SESSION_JOIN` 选,项目两列就是 `MEMORY_PROJECT` 那个表达式。
/// 最后一列是正文的位置,`row_to_memory` 按位读
macro_rules! memory_cols {
    ($body:literal) => {
        concat!(
            "m.key, m.agent_id, m.host, m.scope,
    COALESCE(NULLIF(m.project_path, ''), s.project_path, ''),
    COALESCE(NULLIF(m.project_name, ''), s.project_name, ''),
    m.session_key, m.path, m.title, m.updated_at, m.size_bytes, m.source, ",
            $body
        )
    };
}
const MEMORY_COLS: &str = memory_cols!("m.body");
/// 列表用的同一组列,只是正文给空串:列表谁都不看正文(GUI 阅读时经 get_memory
/// 另取,MCP 的 wake_list_memories 只打标题),没必要每次刷新把几百 KB 文本拉出来
const MEMORY_LIST_COLS: &str = memory_cols!("''");

fn row_to_memory(r: &rusqlite::Row<'_>) -> rusqlite::Result<MemoryDoc> {
    let agent: String = r.get(1)?;
    let scope: String = r.get(3)?;
    Ok(MemoryDoc {
        key: r.get(0)?,
        agent: AgentId::from_str(&agent).unwrap_or(AgentId::ClaudeCode),
        host: r.get(2)?,
        scope: MemoryScope::parse(&scope).unwrap_or(MemoryScope::Project),
        project_path: r.get(4)?,
        project_name: r.get(5)?,
        session_key: r.get(6)?,
        path: r.get(7)?,
        title: r.get(8)?,
        updated_at: r.get(9)?,
        size_bytes: r.get(10)?,
        source: r.get(11)?,
        body: r.get(12)?,
    })
}

/// list_memories / search_memories 共用的筛选谓词(表别名 m):agent 并集;项目并集
/// 之外用户级记忆默认放行——它对每个项目都成立;`f.user` 可以把它剔掉(GUI 的
/// 项目行)或只留它(GUI 的 User memory 行)
fn memory_filter(f: &MemoryFilter) -> (String, Vec<Box<dyn rusqlite::ToSql>>) {
    let mut sql = String::new();
    let mut args: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    let user = MemoryScope::User.as_str();
    match f.user {
        UserMemories::Alongside => {}
        UserMemories::Excluded => sql.push_str(&format!(" AND m.scope != '{user}'")),
        UserMemories::Only => sql.push_str(&format!(" AND m.scope = '{user}'")),
    }
    if !f.agents.is_empty() {
        sql.push_str(&format!(
            " AND m.agent_id IN ({})",
            placeholders(f.agents.len())
        ));
        for a in &f.agents {
            args.push(Box::new(a.as_str().to_string()));
        }
    }
    if let Some(since) = f.updated_since {
        sql.push_str(" AND m.updated_at >= ?");
        args.push(Box::new(since));
    }
    if f.scopes_projects() {
        // 项目并集 ∪ 没归属的一组(显式的 unattributed,不拿空串当暗号:空串什么都
        // 不匹配)∪ 用户级(对每个项目都成立;只有 Alongside 放行——Excluded 上面已
        // 剔掉,Only 与项目筛选本就没有交集)
        let mut terms: Vec<String> = Vec::new();
        if !f.project_paths.is_empty() {
            terms.push(format!(
                "({MEMORY_PROJECT} != '' AND {MEMORY_PROJECT} IN ({}))",
                placeholders(f.project_paths.len())
            ));
            for p in &f.project_paths {
                args.push(Box::new(p.clone()));
            }
        }
        if f.unattributed {
            terms.push(format!("{MEMORY_PROJECT} = ''"));
        }
        if f.user == UserMemories::Alongside {
            terms.push(format!("m.scope = '{user}'"));
        }
        sql.push_str(&format!(" AND ({})", terms.join(" OR ")));
    }
    (sql, args)
}

/// 自定义记忆来源的两条写语句,add / remove / replace 共用(replace = 同一事务里先删后加)
fn insert_memory_source(conn: &rusqlite::Connection, agent: &str, path: &str) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO memory_sources_custom(agent, path, added_at) VALUES (?1, ?2, ?3)",
        params![agent, path, now_ms()],
    )?;
    Ok(())
}

/// 连同它的停用记录一起删(日后重新添加同一路径要按启用开始)
fn delete_memory_source(conn: &rusqlite::Connection, agent: &str, path: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM memory_sources_custom WHERE agent = ?1 AND path = ?2",
        params![agent, path],
    )?;
    conn.execute(
        "DELETE FROM memory_sources_disabled WHERE agent = ?1 AND source = ?2",
        params![agent, path],
    )?;
    Ok(())
}

/// 写一份记忆:memories 行按 key 就地更新(id 不变,memories_fts 的 rowid 就是它),
/// FTS 那份先按 rowid 删再插。语句都走 prepare_cached——一次冷建一百多份,逐条
/// 编译 SQL 白费
fn upsert_memory(tx: &rusqlite::Transaction<'_>, d: &MemoryDoc) -> Result<()> {
    let id: i64 = tx
        .prepare_cached(
            "INSERT INTO memories(key, agent_id, host, scope, project_path, project_name,
           session_key, path, title, updated_at, size_bytes, source, body)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)
         ON CONFLICT(key) DO UPDATE SET agent_id = excluded.agent_id, host = excluded.host,
           scope = excluded.scope, project_path = excluded.project_path,
           project_name = excluded.project_name, session_key = excluded.session_key,
           path = excluded.path, title = excluded.title, updated_at = excluded.updated_at,
           size_bytes = excluded.size_bytes, source = excluded.source, body = excluded.body
         RETURNING id",
        )?
        .query_row(
            params![
                d.key,
                d.agent.as_str(),
                d.host,
                d.scope.as_str(),
                d.project_path,
                d.project_name,
                d.session_key,
                d.path,
                d.title,
                d.updated_at,
                d.size_bytes,
                d.source,
                d.body,
            ],
            |r| r.get::<_, i64>(0),
        )?;
    tx.prepare_cached("DELETE FROM memories_fts WHERE rowid = ?1")?
        .execute(params![id])?;
    tx.prepare_cached("INSERT INTO memories_fts(rowid, title, body) VALUES (?1, ?2, ?3)")?
        .execute(params![id, d.title, d.body])?;
    Ok(())
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

/// Insights 的 "Agents asking Wake":各家 agent 在 `since_ms` 之后查 Wake 的次数,按接入
/// 渠道分列,总数降序、只含 >0 的家。按每次调用所在消息的时刻归窗,缺时间戳的退回
/// 会话 updated_at。跑在 insights 的私有连接上,`canonical_sessions` 是它建的 temp 表
/// (归档不计、多 host 镜像只算一份)。这是 mcp-roadmap 定的"阶段 A 有没有真实使用"的仪表
fn tally_wake_lookups(conn: &Connection, since_ms: i64) -> Result<Vec<LookupTally>> {
    let mut stmt = conn.prepare(
        "SELECT s.agent_id, SUM(l.channel = 'mcp'), SUM(l.channel = 'cli')
         FROM wake_lookups l JOIN sessions s ON s.key = l.session_key
         WHERE s.archived = 0 AND COALESCE(l.ts, s.updated_at) >= ?1
           AND s.key IN canonical_sessions
         GROUP BY s.agent_id ORDER BY COUNT(*) DESC, s.agent_id",
    )?;
    let rows = stmt
        .query_map(params![since_ms], |r| {
            Ok(LookupTally {
                name: r.get(0)?,
                mcp: r.get(1)?,
                cli: r.get(2)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// 清掉一条会话在库里的全部派生行:messages 及其 FTS 影子、wake_lookups。写入前
/// (write_session_tx)与删除时(remove_sessions_recorded)共用;titles_fts 不在这里,
/// 它随 upsert_session 走(quick 路径也要改标题)。再加一张派生表只改这里和写入侧
fn clear_session_rows(tx: &rusqlite::Transaction<'_>, key: &str) -> Result<()> {
    // FTS external content 需要显式 delete 旧行
    let rows: Vec<(i64, String)> = tx
        .prepare_cached("SELECT id, text FROM messages WHERE session_key = ?1")?
        .query_map(params![key], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let mut fts_del = tx.prepare_cached(
        "INSERT INTO messages_fts(messages_fts, rowid, text) VALUES ('delete', ?1, ?2)",
    )?;
    for (id, text) in rows {
        fts_del.execute(params![id, text])?;
    }
    drop(fts_del);
    tx.prepare_cached("DELETE FROM messages WHERE session_key = ?1")?
        .execute(params![key])?;
    tx.prepare_cached("DELETE FROM wake_lookups WHERE session_key = ?1")?
        .execute(params![key])?;
    Ok(())
}

/// 认领检查:写入闸门(write_session_guarded / write_meta_only,在写事务里)与
/// is_key_claimed(读连接)共用
fn is_claimed(conn: &Connection, key: &str) -> Result<bool> {
    Ok(conn
        .prepare_cached("SELECT 1 FROM claimed_sessions WHERE key = ?1")?
        .query_row(params![key], |_| Ok(()))
        .optional()?
        .is_some())
}

/// 从库里删掉一条会话:派生行、标题索引与会话行本身。删除(remove_sessions)与认领
/// (replace_claims)共用,墓碑由调用方决定
fn delete_session_row(tx: &rusqlite::Transaction<'_>, key: &str) -> Result<()> {
    clear_session_rows(tx, key)?;
    tx.execute("DELETE FROM titles_fts WHERE key = ?1", params![key])?;
    tx.execute("DELETE FROM sessions WHERE key = ?1", params![key])?;
    Ok(())
}

/// write_session / write_session_guarded 共用的事务内核
fn write_session_tx(
    tx: &rusqlite::Transaction<'_>,
    meta: &SessionMeta,
    file_mtime: i64,
    units: &[IndexUnit],
    lookups: &[WakeLookup],
) -> Result<()> {
    clear_session_rows(tx, &meta.key)?;
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
    // Wake 调用记录与 messages 同事务、同 key 删插;quick 路径(write_meta_only)
    // 不经这里,所以不会把记录冲掉。绝大多数会话一次都没有,别为它白取一次语句
    if !lookups.is_empty() {
        let mut ins_lookup = tx.prepare_cached(
            "INSERT INTO wake_lookups(session_key, seq, ts, channel, tool) VALUES (?1,?2,?3,?4,?5)",
        )?;
        for l in lookups {
            ins_lookup.execute(params![
                meta.key,
                l.seq,
                l.timestamp,
                l.channel.as_str(),
                l.tool
            ])?;
        }
    }
    Ok(())
}

fn upsert_session(tx: &rusqlite::Transaction<'_>, m: &SessionMeta, file_mtime: i64) -> Result<()> {
    // titles_fts 与 sessions.title 同步的**唯一写点**:全量、增量、quick(write_meta_only)
    // 三条路都经这里,标题一改索引即跟上。按 key 删再插(UNINDEXED 列的 DELETE 是
    // 整表扫,几千行也就微秒级),空标题不入索引
    tx.execute("DELETE FROM titles_fts WHERE key = ?1", params![m.key])?;
    if !m.title.is_empty() {
        tx.execute(
            "INSERT INTO titles_fts(key, title) VALUES (?1, ?2)",
            params![m.key, m.title],
        )?;
    }
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

/// 搜索串按空白切成词项(会话搜索与记忆搜索同一套分词)
fn fts_terms(q: &str) -> Vec<&str> {
    q.split_whitespace().filter(|s| !s.is_empty()).collect()
}

/// 有词项短于 3 码点就走 LIKE 子串扫描——trigram 分词对更短的词无能为力
fn needs_like_fallback(segs: &[&str]) -> bool {
    segs.iter().any(|s| s.chars().count() < 3)
}

/// FTS5 的 MATCH 表达式:每个词项加引号(内部引号翻倍)、AND 连接。三张 FTS 表共用,
/// 转义规则只此一处
fn fts_match_expr(segs: &[&str]) -> String {
    segs.iter()
        .map(|s| format!("\"{}\"", s.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" AND ")
}

/// LIKE 降级路径的 WHERE:每个词项对每一列各一个 `LIKE ? ESCAPE '\'`(列之间 OR、
/// 词项之间 AND),参数由 `like_args` 按同一顺序给。正文、标题、记忆三条降级路径共用
fn like_where(cols: &[&str], terms: usize) -> String {
    let per_term = cols
        .iter()
        .map(|c| format!("{c} LIKE ? ESCAPE '\\'"))
        .collect::<Vec<_>>()
        .join(" OR ");
    let per_term = if cols.len() > 1 {
        format!("({per_term})")
    } else {
        per_term
    };
    std::iter::repeat_n(per_term, terms)
        .collect::<Vec<_>>()
        .join(" AND ")
}

/// `like_where` 的参数:每个词项转义成 `%term%`,每列一份
fn like_args(segs: &[&str], cols: usize) -> Vec<Box<dyn rusqlite::ToSql>> {
    segs.iter()
        .flat_map(|s| {
            let pat = format!("%{}%", escape_like(s));
            std::iter::repeat_n(pat, cols)
        })
        .map(|pat| Box::new(pat) as Box<dyn rusqlite::ToSql>)
        .collect()
}

/// LIKE 降级路径的 snippet:不区分大小写找首个词项,前 40 / 后 80 字符开窗并高亮。
/// 全程按**字符**下标走:逐字符小写并记下每个小写字符来自原文第几个字符——小写
/// 不保长(Ω→ω、İ→i̇、K→k 字节数与字符数都会变),拿小写串里的字节偏移去切原文正是
/// 2026-09-21 review 复现的 panic(⌘K 逐字键入 CJK 查询必经此路,后台线程 panic
/// 直接 abort 整个 app)
fn make_like_snippet(text: &str, first_seg: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let needle: Vec<char> = first_seg.to_lowercase().chars().collect();
    let mut lower: Vec<char> = Vec::with_capacity(chars.len());
    let mut origin: Vec<usize> = Vec::with_capacity(chars.len());
    for (i, c) in chars.iter().enumerate() {
        for lc in c.to_lowercase() {
            lower.push(lc);
            origin.push(i);
        }
    }
    let hit = (!needle.is_empty())
        .then(|| {
            lower
                .windows(needle.len())
                .position(|w| w == needle.as_slice())
        })
        .flatten();
    let Some(pos) = hit else {
        return chars.iter().take(120).collect();
    };
    let char_idx = origin[pos];
    let match_end = origin[pos + needle.len() - 1] + 1;
    let start = char_idx.saturating_sub(40);
    let end = (match_end + 80).min(chars.len());
    let mut out = String::new();
    if start > 0 {
        out.push('…');
    }
    out.extend(&chars[start..char_idx]);
    out.push(HL_OPEN);
    out.extend(&chars[char_idx..match_end]);
    out.push(HL_CLOSE);
    out.extend(&chars[match_end..end]);
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

/// `--db` 给了就用它,否则取 GUI 那份。调用点一律**惰性**求值:
/// `default_db_path` 首次调用会把旧 vibex 库拷过来,`--help` 不该触发它
pub fn path_or_default(db: Option<&Path>) -> PathBuf {
    db.map(Path::to_path_buf).unwrap_or_else(default_db_path)
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
