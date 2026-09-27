//! ZCode(Z.ai 给 GLM-5.3 做的官方 harness,桌面 app)。它的 agent 运行时
//! `~/.zcode/cli/` 是 OpenCode 的衍生物:`cli/db/db.sqlite` 的 session / message /
//! part 三表与 OpenCode v1 同名同列,part 的 JSON(text / reasoning /
//! tool{callID,tool,state} / step-start / step-finish)逐字段一致——正文解码直接
//! 走 opencode.rs 的 `parse_v1_messages`(排序子句 + 逐条消息的钩子是两家仅有
//! 的差异)。分家的是 session 表:没有 model / tokens 三列(OpenCode 的枚举 SQL
//! 对它 prepare 就失败,所以不能当孪生实例),多了 project_id / slug / task_type /
//! title_source / trace_id;model 逐消息记在 message.data(assistant 顶层
//! `modelId`,同一会话会换),token 记在 assistant 消息的 `tokens.total`,按调用累加。
//!
//! 桌面层另有 `v2/tasks-index.sqlite`(`tasks.task_id` = `session.id`),这里只借
//! 它两个过滤位:`deleted`(桌面端软删,cli 库里那行还在)与 `migration_source`
//! (首启向导从 Claude Code 导入的对话——原家已经索引过,再列一遍就是双份)。
//! 它不产生会话,不进 data_roots(不变量 8);读不出来 = 什么都不藏。
//!
//! 哪些会话算用户的:按 `session.task_type` **白名单** interactive / fork /
//! selection_side_chat(后两种带 parent_id——用户从某条会话分出去的对话,桌面端
//! 照样列在任务里,所以不能按 parent_id 一刀切);subagent_child / workflow_child
//! 这类运行时会话不列。未知类型不列(dsh 同款取舍:宁可少列也不放 Untitled 噪音)。
//! task_type 是 ALTER 追加的列,老库退回 `parent_id IS NULL`(OpenCode 语义)。
//!
//! Meta 判据两条并列:`semantics.transcriptVisibility == "hidden"`(源码里 fork
//! 通知、system reminder、compaction 摘要都这么标,任一角色)与用户消息的
//! `semantics.origin != "real_user"`(system / import / synthetic);
//! `kind == "compact_summary"` 另给 CompactSummary(与 OpenCode 的 compaction 同一
//! 渲染)。**字段缺席按真人放行**——库是 22 次 migration 攒出来的(0.15.2 →
//! 0.16.5),老库与向导导入的旧版会话未必有它,一刀切会把所有用户消息归 Meta、
//! 标题与 prompt 数一起归零。列级降级同理:title_source / task_type 与两张表的
//! sequence 是 ALTER 追加的列(parent_id / directory / time_archived 在初始
//! schema 里,不用探),每次开库现探两条 PRAGMA、缺列给默认值,老库不得整家消失
//! (Hermes 同款,但不用 OnceLock——ZCode 会原地 migration,钉死到重启才看见新列)。
//!
//! resume 不提供:PATH 里没有 CLI;app 内的 `Resources/glm/zcode.cjs` 是常驻
//! 运行时的一部分、持有 db.sqlite 的写锁,外面再起一个对着同一个库跑不安全;
//! `zcode://` 只有 workspace/open?path=、share/import、oauth/callback 三条深链,
//! 没有按会话打开的——详情页不画 Open In(WorkBuddy 同款)。
//! 2026-09-21 对本机 3.14.0(运行时 0.16.9)的真实会话首验:text / reasoning /
//! tool / step-start / step-finish 五种 part、中途换模型、429 失败的 turn 全吻合;
//! 同日 ZCode 开源(github.com/zai-org/zcode,Apache-2.0),task_type / semantics /
//! title_source / migration_source / deleted 的语义全部按源码核过。
//!
//! 记忆(2026-09-21 晚,记忆可见层):`cli/memories/projects/<slug>-<hash>/memory/*.md`,
//! `MEMORY.md` 索引 + 逐条主题文件,frontmatter(name / description / metadata.type)
//! 与 Claude auto-memory 同款——源码 context/sections/memory.ts 那段提示词就是照着
//! Claude 的写的,由它的 memory extraction 子代理在对话后写入,桌面端自己也有一个
//! 只认 `*.md` 的 Project Memory 列表。目录名的 hash 是 sha256(工作区路径) 的前
//! 16 位(memory/project-root.ts;win32 先小写),而库里每条会话都带 directory:
//! 逐条算一遍就精确对上项目,不用会话锚点、不用像 Claude 那样在磁盘上反推;
//! 对不上的落 Unknown project。只从 home 形态取(裸库拷贝没有 home,不摸父目录)。
//! 本机目录还是空的:目录名规则对真机核过(`default-0e81bcb7a4286b1b` 正是库里唯一
//! 工作区 `~/.zcode/workspace/default` 的 hash),文件格式按源码推断、**未经真机验证**
//! (OpenClaw / CodeBuddy 同款),fixture 合成。
use super::opencode::{parse_v1_messages, V1Order};
use super::parse_utils::*;
use super::sqlite_ro::{
    db_cache_stamp, open_sqlite_ro, strip_virtual_path, table_columns, virtual_path, SqliteRo,
};
use super::AgentAdapter;
use crate::models::*;
use anyhow::{anyhow, Result};
use rusqlite::Connection;
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

const HOME_DIR_NAME: &str = ".zcode";
/// 会话与正文的唯一来源,相对 ZCode home
const DB_REL: &str = "cli/db/db.sqlite";
/// 桌面层的任务索引,相对 ZCode home
const TASKS_REL: &str = "v2/tasks-index.sqlite";
/// 按项目的记忆目录,相对 ZCode home:`<slug>-<hash>/memory/*.md`
const MEMORIES_REL: &str = "cli/memories/projects";

pub struct ZcodeAdapter {
    db: PathBuf,
    /// None = 孤立的库拷贝(自定义 location 直接选了 db.sqlite),没有桌面层,什么都不藏
    tasks_db: Option<PathBuf>,
    /// ZCode home(记忆目录的所在);孤立的库拷贝没有——侧档绝不越界摸父目录(不变量 8⑨)
    home: Option<PathBuf>,
    /// 枚举查询带全表相关子查询,按库 mtime 缓存一轮扫描内的重复调用
    rows_cache: MtimeCache<Vec<ZcRow>>,
    /// tasks-index 里要藏的会话 id,按它自己的 mtime 缓存
    hidden_cache: MtimeCache<HashSet<String>>,
    /// 记忆文档按目录指纹缓存(每轮扫描都会列,没变不重读)
    memories: super::MemoryCache,
}

#[derive(Clone)]
struct ZcRow {
    id: String,
    directory: String,
    title: String,
    /// title_source == "default":桌面端还没来得及生成标题的占位(源码里配的是空 title)
    placeholder_title: bool,
    created_ms: i64,
    updated_ms: i64,
    archived: bool,
    /// part.data 总长——空会话过滤与 SessionFileRef.size 的脏判据(单会话取行时不算)
    content_len: i64,
}

/// ALTER 追加的列各自在不在(初始 schema 的列不用探)
#[derive(Clone, Copy)]
struct ZcSchema {
    title_source: bool,
    task_type: bool,
    part_sequence: bool,
    message_sequence: bool,
}

impl ZcSchema {
    fn probe(conn: &Connection) -> Self {
        let session = table_columns(conn, "session");
        Self {
            title_source: session.contains("title_source"),
            task_type: session.contains("task_type"),
            part_sequence: table_columns(conn, "part").contains("sequence"),
            message_sequence: table_columns(conn, "message").contains("sequence"),
        }
    }

    /// 两张表都有 autofill 触发器给的 sequence 列;老库没有就退回 OpenCode 的序
    fn order(&self) -> V1Order {
        V1Order {
            part: if self.part_sequence {
                "message_id, sequence, id"
            } else {
                V1Order::OPENCODE.part
            },
            message: if self.message_sequence {
                "sequence, time_created, id"
            } else {
                V1Order::OPENCODE.message
            },
        }
    }
}

/// 一次会话解析的产物:正文之外还带枚举列里没有的 token 累计
struct Parsed {
    messages: Vec<TranscriptMessage>,
    unknown: u32,
    tokens: i64,
}

impl ZcodeAdapter {
    pub fn new() -> Self {
        // ZCODE_STORAGE_DIR 是 zcode.cjs 自己的开关(beta 渠道用它切到
        // zcode-beta 目录)。与其他 env override 一致:候选里真有库文件才采信,
        // 存在但空的目录不能遮掉默认根(Dock 启动与 shell 启动的环境常不同)
        let home = super::env_dir("ZCODE_STORAGE_DIR")
            .filter(|dir| dir.join(DB_REL).is_file())
            .unwrap_or_else(|| super::home_dir().unwrap_or_default().join(HOME_DIR_NAME));
        Self::from_home(home)
    }

    /// home 形态:库、任务索引、记忆目录全相对派生
    fn from_home(home: PathBuf) -> Self {
        Self {
            db: home.join(DB_REL),
            tasks_db: Some(home.join(TASKS_REL)),
            home: Some(home),
            rows_cache: MtimeCache::new(),
            hidden_cache: MtimeCache::new(),
            memories: super::MemoryCache::new(),
        }
    }

    /// 孤立的库拷贝:只有会话,没有桌面层、没有记忆
    fn from_db(db: PathBuf) -> Self {
        Self {
            db,
            tasks_db: None,
            home: None,
            rows_cache: MtimeCache::new(),
            hidden_cache: MtimeCache::new(),
            memories: super::MemoryCache::new(),
        }
    }

    fn open(&self) -> Option<SqliteRo> {
        open_sqlite_ro(&self.db, "zcode")
    }

    /// `cli/memories/projects/<slug>-<hash>/memory/*.md` 展开成读取单元。目录名的 hash
    /// 对应库里会话的 directory(见 memory_dir_hash);库读不出就都对不上,记忆照列、
    /// 落 Unknown project。没这个目录 = 确定没有记忆;别的读取失败报 Err,scanner 只
    /// 冻结这一来源
    fn tree_units(&self, source: &MemorySource) -> Result<Vec<super::MemoryUnit>> {
        let workspaces: HashMap<String, String> = self
            .rows()
            .unwrap_or_default()
            .into_iter()
            .map(|row| (memory_dir_hash(&row.directory), row.directory))
            .collect();
        super::project_tree_units(source, |entry| {
            let project_path = entry
                .file_name()
                .to_str()
                .and_then(|name| name.rsplit_once('-'))
                .and_then(|(_, hash)| workspaces.get(hash))
                .cloned()
                .unwrap_or_default();
            (String::new(), project_path)
        })
    }

    /// 行清单。库这一刻读不出(ZCode 原地 migration、copy 梯度失败)交回**上一次读到
    /// 的**:枚举接口表达不了"不知道",而空清单会让 scanner 把整家会话当"磁盘已删"
    /// 清掉、下一轮再全部重解析回来;从没读成功过才是 None(2026-09-21 review)
    fn rows(&self) -> Option<Vec<ZcRow>> {
        let stamp = db_cache_stamp(&self.db);
        self.rows_cache.get_or_stale(stamp, || {
            let ro = self.open()?;
            query_rows(&ro.conn, ZcSchema::probe(&ro.conn), None).ok()
        })
    }

    /// 桌面层要藏起来的会话 id。库不在 = 确定没有要藏的(只同步了 cli 库的远程缓存、
    /// 没装桌面端);库在但读不出 = 不知道,用上一次读到的名单(`get_or_stale`,戳不
    /// 更新、下次重试)——折成空名单会把用户在桌面端删掉的、向导从 Claude 导入的
    /// 会话整批放进索引(2026-09-21 review);本进程从没读成功过的第一轮仍是空,
    /// 那一轮多列的行在下一轮读成功后随枚举消失
    fn hidden(&self) -> HashSet<String> {
        let Some(tasks_db) = &self.tasks_db else {
            return HashSet::new();
        };
        let stamp = db_cache_stamp(tasks_db);
        self.hidden_cache
            .get_or_stale(stamp, || read_hidden(tasks_db))
            .unwrap_or_default()
    }

    fn parse(
        &self,
        r: &SessionFileRef,
        decode_images: bool,
    ) -> Result<(SessionMeta, Vec<TranscriptMessage>, u32)> {
        let _image_budget = transcript_image_decode_budget(decode_images);
        if Path::new(strip_virtual_path(&r.file_path)) != self.db.as_path() {
            return Err(anyhow!("zcode database is outside adapter roots"));
        }
        let ro = self.open().ok_or_else(|| anyhow!("cannot open zcode db"))?;
        let schema = ZcSchema::probe(&ro.conn);
        let row = query_rows(&ro.conn, schema, Some(&r.native_id))?
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("zcode session {} not in db", r.native_id))?;
        let parsed = parse_messages(&ro, &r.native_id, decode_images, &schema.order())?;
        let meta = build_meta(r, &row, &parsed);
        Ok((meta, parsed.messages, parsed.unknown))
    }
}

fn build_meta(r: &SessionFileRef, row: &ZcRow, parsed: &Parsed) -> SessionMeta {
    // 占位标题退回首条真人消息;其余三级(first_input / generated / custom)
    // 库里的就是最终值
    let title = (!row.placeholder_title)
        .then_some(row.title.as_str())
        .map(clean_title_candidate)
        .filter(|t| !t.is_empty())
        .or_else(|| title_from_messages(&parsed.messages))
        .unwrap_or_else(|| UNTITLED.to_string());
    SessionMeta {
        key: format!("zcode:{}", row.id),
        host: String::new(),
        id: row.id.clone(),
        agent: AgentId::Zcode,
        title,
        project_path: row.directory.clone(),
        project_name: project_name_of(&row.directory),
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
        message_count: parsed
            .messages
            .iter()
            .filter(|m| m.kind == MessageKind::Text)
            .count() as i64,
        size_bytes: r.size,
        git_branch: None,
        // 同一会话会换模型,取最后一条 assistant 用的
        model: parsed.messages.iter().rev().find_map(|m| m.model.clone()),
        tokens_used: (parsed.tokens > 0).then_some(parsed.tokens),
        archived: row.archived,
        // task_type 只有 interactive / fork / selection_side_chat 会列,没有可打的 via 徽章
        source: None,
        favorite: false,
        pinned: false,
        custom_title: None,
    }
}

/// 入库前的归一化(mod.rs 静态分派):用户在目录选择器里选中 `cli/db/db.sqlite`、
/// `cli/db`、`cli` 这几层时上提到 ZCode home,tasks-index 随之找回。判据是纯
/// 路径形状——选中的路径以这几层收尾就按层数上提——不摸文件系统;孤立的库
/// 拷贝(父链不长这个样)原样保留。构造器自己不越界摸父目录(不变量 8⑨)
pub fn normalize_custom_root(dir: PathBuf) -> PathBuf {
    for rel in [DB_REL, "cli/db", "cli"] {
        if dir.ends_with(rel) {
            let depth = Path::new(rel).components().count();
            return dir
                .ancestors()
                .nth(depth)
                .map_or_else(|| dir.clone(), Path::to_path_buf);
        }
    }
    dir
}

/// 枚举 / 单会话取行。缺列给默认值:老库不得整家消失。单会话取行时不算 part
/// 总长——它只给枚举做空会话过滤与脏判据,parse 拿的是 r.size,免得每次 parse
/// 把该会话的 part 载荷多读一遍
fn query_rows(
    conn: &Connection,
    schema: ZcSchema,
    id: Option<&str>,
) -> rusqlite::Result<Vec<ZcRow>> {
    let title_source = if schema.title_source {
        "s.title_source"
    } else {
        "'first_input'"
    };
    // 用户自己的会话才列(白名单,见模块注释);老库没有 task_type 退回
    // parent_id IS NULL(OpenCode 语义)
    let listed = if schema.task_type {
        "s.task_type IN ('interactive', 'fork', 'selection_side_chat')"
    } else {
        "s.parent_id IS NULL"
    };
    let content_len = if id.is_some() {
        "0"
    } else {
        "(SELECT COALESCE(SUM(LENGTH(p.data)), 0) FROM part p WHERE p.session_id = s.id)"
    };
    let sql = format!(
        "SELECT s.id, s.directory, s.title, {title_source}, s.time_created, s.time_updated,
                s.time_archived, {content_len}
         FROM session s WHERE {listed} AND (?1 IS NULL OR s.id = ?1)"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([id], |r| {
        Ok(ZcRow {
            id: r.get(0)?,
            directory: r.get::<_, Option<String>>(1)?.unwrap_or_default(),
            title: r.get::<_, Option<String>>(2)?.unwrap_or_default(),
            placeholder_title: r.get::<_, Option<String>>(3)?.as_deref() == Some("default"),
            created_ms: r.get::<_, Option<i64>>(4)?.unwrap_or(0),
            updated_ms: r.get::<_, Option<i64>>(5)?.unwrap_or(0),
            archived: r.get::<_, Option<i64>>(6)?.is_some(),
            content_len: r.get::<_, Option<i64>>(7)?.unwrap_or(0),
        })
    })?;
    rows.collect()
}

/// tasks-index 里要藏的会话 id。**`None` 与 `Some(空)` 是两件事**:库不在、没有
/// tasks 表、两个过滤列都没有 = 确定没有要藏的;库在但打不开、读到一半出错 =
/// 不知道,交回 None 让 `MtimeCache` 不缓存(它的契约:build 返回 None 时不缓存
/// 失败结果)——否则一次瞬时失败会把"已删除"的会话放出来并按 mtime 戳钉住。
/// 临时目录 tag 与主库不同:两个库在 copy 梯度上不能撞同一个目录
fn read_hidden(tasks_db: &Path) -> Option<HashSet<String>> {
    if !tasks_db.is_file() {
        return Some(HashSet::new());
    }
    let ro = open_sqlite_ro(tasks_db, "zcode-tasks")?;
    let cols = table_columns(&ro.conn, "tasks");
    if !cols.contains("task_id") {
        return Some(HashSet::new());
    }
    let mut predicates = Vec::new();
    if cols.contains("deleted") {
        predicates.push("deleted = 1");
    }
    if cols.contains("migration_source") {
        predicates.push("(migration_source IS NOT NULL AND migration_source != '')");
    }
    if predicates.is_empty() {
        return Some(HashSet::new());
    }
    let sql = format!(
        "SELECT task_id FROM tasks WHERE {}",
        predicates.join(" OR ")
    );
    let mut stmt = ro.conn.prepare(&sql).ok()?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0)).ok()?;
    rows.collect::<rusqlite::Result<HashSet<_>>>().ok()
}

/// OpenCode 的 v1 两表循环 + ZCode 自己的三处差异:assistant 顶层 modelId、
/// tokens.total 按调用累加(failed turn 没有 part、出不了消息,token 也照记)、
/// semantics 归 Meta / CompactSummary
fn parse_messages(
    ro: &SqliteRo,
    sid: &str,
    decode_images: bool,
    order: &V1Order,
) -> Result<Parsed> {
    let mut tokens = 0i64;
    let (messages, unknown) =
        parse_v1_messages(ro, sid, decode_images, order, |md, role, message| {
            if role == Role::Assistant {
                tokens += md
                    .pointer("/tokens/total")
                    .and_then(Value::as_i64)
                    .unwrap_or(0);
            }
            let Some(m) = message else { return };
            if role == Role::Assistant {
                m.model = optional_string(md.get("modelId"));
            }
            let semantics = md.get("semantics");
            let semantic = |key: &str| semantics.and_then(|s| s.get(key)).and_then(Value::as_str);
            let hidden = semantic("transcriptVisibility") == Some("hidden");
            let injected = role == Role::User
                && semantic("origin").is_some_and(|origin| origin != "real_user");
            if semantic("kind") == Some("compact_summary") {
                m.kind = MessageKind::CompactSummary;
            } else if hidden || injected {
                m.kind = MessageKind::Meta;
            }
        })?;
    Ok(Parsed {
        messages,
        unknown,
        tokens,
    })
}

impl AgentAdapter for ZcodeAdapter {
    fn agent(&self) -> AgentId {
        AgentId::Zcode
    }

    fn list_session_files(&self) -> Result<Vec<SessionFileRef>> {
        // 库不在 = 没有会话(Ok 空,不变量 8①);库在但从没读成功过(ZCode 常驻、正在原地
        // migration)= 不知道,报 Err 让这一轮扫描停下——折成空会让 seen_paths 清理把整家
        // 会话当"磁盘已删"删掉、下一轮再全部重解析回来(2026-09-22 review)
        let rows = match self.rows() {
            Some(rows) => rows,
            None if self.db.is_file() => {
                anyhow::bail!("{} exists but could not be read", self.db.display())
            }
            None => Vec::new(),
        };
        let hidden = self.hidden();
        Ok(rows
            .into_iter()
            .filter(|row| row.content_len > 0 && !hidden.contains(&row.id))
            .map(|row| SessionFileRef {
                agent: AgentId::Zcode,
                file_path: virtual_path(&self.db, &row.id),
                native_id: row.id,
                mtime_ms: row.updated_ms,
                size: row.content_len,
            })
            .collect())
    }

    fn parse_session(&self, r: &SessionFileRef) -> Result<ParsedSession> {
        let (meta, messages, unknown) = self.parse(r, false)?;
        Ok(ParsedSession::derive(meta, &messages, unknown))
    }

    fn parse_transcript(&self, r: &SessionFileRef) -> Result<ParsedTranscript> {
        let (meta, messages, unknown) = self.parse(r, true)?;
        Ok(ParsedTranscript {
            meta,
            mainline: messages,
            sidechains: Vec::new(),
            unknown_line_count: unknown,
        })
    }

    fn data_roots(&self) -> Vec<PathBuf> {
        vec![self.db.clone()]
    }

    fn memory_sources(&self) -> Vec<MemorySource> {
        self.home
            .as_ref()
            .map(|home| {
                vec![MemorySource {
                    agent: AgentId::Zcode,
                    kind: MemorySourceKind::ProjectTree,
                    path: home.join(MEMORIES_REL),
                }]
            })
            .unwrap_or_default()
    }

    fn list_memories(
        &self,
        sources: &[MemorySource],
        projects: &[PathBuf],
    ) -> Result<Vec<MemoryDoc>> {
        super::memory_docs_with_tree(&self.memories, AgentId::Zcode, sources, projects, |tree| {
            self.tree_units(tree)
        })
    }

    fn with_custom_root(&self, dir: PathBuf) -> Box<dyn AgentAdapter> {
        // 只按路径形状整形、不看存不存在:远程缓存首次同步前目录还没落盘,
        // 判据随目录出现而改判会让先构造的实例指错树(契约测试卡)。认两种:
        // ZCode home,或直接给到 db.sqlite——后者是孤立的库拷贝,没有桌面层;
        // 中间层(cli/、cli/db/)由 normalize_custom_root 在入库前上提到 home
        if dir.file_name().is_some_and(|n| n == "db.sqlite") {
            Box::new(Self::from_db(dir))
        } else {
            Box::new(Self::from_home(dir))
        }
    }
}

/// 记忆目录名 `<slug>-<hash>` 里的 hash:sha256(工作区路径) 的十六进制前 16 位
/// (源码 memory/project-root.ts:路径先 `resolve`——这里只剥收尾分隔符——win32 再
/// 小写)。库里 session.directory 就是那条工作区路径,算一遍就能对上。"win32 小写"按
/// **路径形态**判(盘符 / UNC 开头)而不按运行平台:远程镜像里 Linux 主机的 POSIX 路径
/// 在 Windows 上照样不能小写,反过来亦然(2026-09-22 Windows CI)
pub fn memory_dir_hash(workspace: &str) -> String {
    let trimmed = workspace.trim_end_matches(std::path::is_separator);
    let key = if trimmed.is_empty() {
        workspace
    } else {
        trimmed
    };
    let windows_shaped = key.starts_with("\\\\")
        || matches!(key.as_bytes(), [drive, b':', ..] if drive.is_ascii_alphabetic());
    let key = if windows_shaped {
        key.to_lowercase()
    } else {
        key.to_string()
    };
    Sha256::digest(key.as_bytes())
        .iter()
        .take(8)
        .map(|b| format!("{b:02x}"))
        .collect()
}
