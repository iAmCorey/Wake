use super::parse_utils::*;
use super::sqlite_ro::{open_sqlite_ro, strip_virtual_path, virtual_path, SqliteRo};
use super::AgentAdapter;
use crate::models::*;
use anyhow::{anyhow, Result};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// OpenCode stable:`~/.local/share/opencode/opencode.db`;OpenCode 2 next 渠道
/// 另用同目录的 `opencode-next.db`。两库可同时存在,必须并行扫描而不是二选一。
///
/// v1:session 表 + 正文在 part 表({type:text|reasoning|tool},synthetic=注入),
/// message 表只有角色与时间。OpenCode 2(binary `opencode2`)的真实 next schema
/// 仍用 session 表,正文改放 session_message(type 列
/// user|synthetic|assistant,data JSON)。早期 preview 曾用 session_v2 表,这里也
/// 保持兼容。parent_id 非空 = 子代理,不进列表。
///
/// Kilo Code 是同一个引擎上的另一个产品(见 `Flavor`),`OpencodeAdapter::kilo()`
/// 读它的 `~/.local/share/kilo/kilo.db`——WorkBuddy 之于 CodeBuddy 的同一种孪生
pub struct OpencodeAdapter {
    flavor: &'static Flavor,
    dbs: Vec<OcDb>,
}

/// 跑在 OpenCode 引擎上的产品。Kilo Code 2026-04 起整个换成 OpenCode 的分支(VS Code
/// 扩展在后台起 `kilo serve`,CLI 与 JetBrains 插件同一个引擎),三端共用一个库,表与
/// OpenCode 同形;差别只有下面这几项
struct Flavor {
    agent: AgentId,
    /// `$XDG_DATA_HOME`(缺省 `~/.local/share`)下的目录名。Windows 上同样是
    /// `%USERPROFILE%\.local\share`(两家都用 xdg-basedir,它在 Windows 上也这么算)
    dir: &'static str,
    /// 常驻 roster 的固定候选库名:没装也在,装上后普通刷新就能发现
    db_names: &'static [&'static str],
    /// 指定库的环境变量:绝对路径,或相对数据目录的文件名(两家同一规则)
    db_env: &'static str,
    /// 有没有 OpenCode 2 的 preview / next 渠道:那边的会话打 opencode2 徽章,resume 据此
    /// 换 opencode2 二进制(`OcRow::source`)。Kilo 没有这条渠道
    marks_preview: bool,
}

/// OpenCode 2 next 渠道另起的库名(preview 会话的最强信号,见 `OcRow::source`)
const NEXT_DB: &str = "opencode-next.db";

const OPENCODE: Flavor = Flavor {
    agent: AgentId::Opencode,
    dir: "opencode",
    db_names: &["opencode.db", NEXT_DB],
    db_env: "OPENCODE_DB",
    marks_preview: true,
};

/// 渠道库(`kilo-<channel>.db`)只有 Kilo 自己的开发构建会写,扩展还强制
/// `KILO_DISABLE_CHANNEL_DB=true`,所以只认 `kilo.db`
const KILO: Flavor = Flavor {
    agent: AgentId::Kilo,
    dir: "kilo",
    db_names: &["kilo.db"],
    db_env: "KILO_DB",
    marks_preview: false,
};

struct OcDb {
    path: PathBuf,
    /// 元数据查询带全表相关子查询,按各库 mtime 分别缓存一轮扫描内的重复调用
    rows_cache: MtimeCache<Vec<OcRow>>,
}

/// 两代 session 表的公共列(别名 s;content_len 子查询两代不同,单独拼)
const ROW_COLS: &str = "s.id, s.directory, s.title, s.time_created, s.time_updated,
        s.model, s.tokens_input + s.tokens_output + s.tokens_reasoning,
        s.time_archived, s.version";

fn select_from(table: &str, content_len: &str, v2_messages: &str) -> String {
    format!(
        "SELECT {ROW_COLS}, {content_len} AS content_len,
                {v2_messages} AS v2_messages
         FROM {table} s"
    )
}

fn has_table(conn: &rusqlite::Connection, name: &str) -> bool {
    conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1",
        [name],
        |_| Ok(()),
    )
    .is_ok()
}

/// 根据库内真实表组合生成一次枚举 SQL。新版 next 与 v1 共用 session 表,
/// 所以必须逐会话看 session_message 是否有正文;仅检查 session_v2 会把真实
/// next 会话误走 part 路径并以 content_len=0 全部过滤(GitHub #2)。
///
/// 两代表都有行时,session_message 里有用户 / 助手消息才算 v2 转录;只有 shell /
/// compaction 这类旁路记录的(实验开关 `*_EXPERIMENTAL_EVENT_SYSTEM` 下 v1 运行时的
/// 零星双写,Kilo 实测)读 message + part,否则整段对话会被换成那几条
fn rows_sql(conn: &rusqlite::Connection) -> Option<String> {
    let has_session = has_table(conn, "session");
    let has_session_v2 = has_table(conn, "session_v2");
    let has_parts = has_table(conn, "part");
    let has_messages_v2 = has_table(conn, "session_message");
    if !has_session && !has_session_v2 {
        return None;
    }

    let part_len = "(SELECT COALESCE(SUM(LENGTH(p.data)), 0) FROM part p \
                    WHERE p.session_id = s.id)";
    let message_len = "(SELECT COALESCE(SUM(LENGTH(m.data)), 0) FROM session_message m \
                       WHERE m.session_id = s.id)";
    let v2_transcript = "(EXISTS(SELECT 1 FROM session_message m WHERE m.session_id = s.id \
                         AND m.type IN ('user', 'assistant')) \
                         OR NOT EXISTS(SELECT 1 FROM message mm WHERE mm.session_id = s.id))";
    let mut selects = Vec::new();

    // 早期 preview schema:session_v2 是全集,session 仅作 v1 回捞。
    if has_session_v2 {
        let len = if has_messages_v2 { message_len } else { "0" };
        selects.push(format!(
            "{} WHERE s.parent_id IS NULL",
            select_from("session_v2", len, "1")
        ));
    }

    if has_session {
        let (len, v2) = match (has_parts, has_messages_v2) {
            (true, true) => (
                format!("CASE WHEN {v2_transcript} THEN {message_len} ELSE {part_len} END"),
                v2_transcript.to_string(),
            ),
            (true, false) => (part_len.to_string(), "0".to_string()),
            (false, true) => (message_len.to_string(), "1".to_string()),
            (false, false) => ("0".to_string(), "0".to_string()),
        };
        let mut sql = format!(
            "{} WHERE s.parent_id IS NULL",
            select_from("session", &len, &v2)
        );
        if has_session_v2 {
            sql.push_str(" AND s.id NOT IN (SELECT id FROM session_v2)");
        }
        selects.push(sql);
    }
    Some(selects.join(" UNION ALL "))
}

fn query_rows(conn: &rusqlite::Connection, id: Option<&str>) -> Result<Vec<OcRow>> {
    let sql = rows_sql(conn).ok_or_else(|| anyhow!("opencode database has no session table"))?;
    let sql = match id {
        Some(_) => format!("SELECT * FROM ({sql}) WHERE id = ?1"),
        None => sql,
    };
    let mut stmt = conn.prepare(&sql)?;
    let rows = match id {
        Some(id) => stmt
            .query_map([id], row_from)?
            .collect::<rusqlite::Result<Vec<_>>>()?,
        None => stmt
            .query_map([], row_from)?
            .collect::<rusqlite::Result<Vec<_>>>()?,
    };
    Ok(rows)
}

fn row_from(r: &rusqlite::Row) -> rusqlite::Result<OcRow> {
    Ok(OcRow {
        id: r.get(0)?,
        directory: r.get::<_, Option<String>>(1)?.unwrap_or_default(),
        title: r.get::<_, Option<String>>(2)?.unwrap_or_default(),
        created_ms: r.get::<_, Option<i64>>(3)?.unwrap_or(0),
        updated_ms: r.get::<_, Option<i64>>(4)?.unwrap_or(0),
        model_json: r.get::<_, Option<String>>(5)?.unwrap_or_default(),
        tokens: r.get::<_, Option<i64>>(6)?.unwrap_or(0),
        archived: r.get::<_, Option<i64>>(7)?.is_some(),
        version: r.get::<_, Option<String>>(8)?.unwrap_or_default(),
        content_len: r.get(9)?,
        v2_messages: r.get::<_, i64>(10)? != 0,
    })
}

impl OcDb {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            rows_cache: MtimeCache::new(),
        }
    }
}

fn push_unique(paths: &mut Vec<PathBuf>, path: PathBuf) {
    if !paths.contains(&path) {
        paths.push(path);
    }
}

fn known_db_paths(flavor: &Flavor, dir: &Path) -> Vec<PathBuf> {
    flavor.db_names.iter().map(|name| dir.join(name)).collect()
}

/// OpenCode 的数据目录服从 XDG;next 渠道另命名数据库而非替换 stable 库。
/// 两个固定候选都常驻 roster,这样 Wake 启动后才安装任一 CLI,普通刷新也能发现。
/// OPENCODE_DB 若被 GUI 进程继承则作为额外候选,但绝不压掉两个标准位置。
/// Kilo 同一套规则(`KILO_DB`,固定候选只有 `kilo.db`)
fn default_db_paths(flavor: &Flavor) -> Vec<PathBuf> {
    let default_dir = super::home_dir()
        .unwrap_or_default()
        .join(".local")
        .join("share")
        .join(flavor.dir);
    let xdg_dir = super::env_dir("XDG_DATA_HOME").map(|x| x.join(flavor.dir));
    let active_dir = xdg_dir
        .as_ref()
        .filter(|dir| known_db_paths(flavor, dir).iter().any(|p| p.is_file()))
        .unwrap_or(&default_dir);

    let mut paths = Vec::new();
    if let Some(value) = std::env::var_os(flavor.db_env).filter(|v| !v.is_empty()) {
        let configured = PathBuf::from(value);
        if configured != Path::new(":memory:") {
            let configured = if configured.is_absolute() {
                configured
            } else {
                active_dir.join(configured)
            };
            if configured.is_file() {
                push_unique(&mut paths, configured);
            }
        }
    }
    for path in known_db_paths(flavor, active_dir) {
        push_unique(&mut paths, path);
    }
    // XDG 位置被采信时,默认目录里已有的库仍是只读候选,避免稳定版历史消失。
    if active_dir != &default_dir {
        for path in known_db_paths(flavor, &default_dir)
            .into_iter()
            .filter(|p| p.is_file())
        {
            push_unique(&mut paths, path);
        }
    }
    paths
}

fn custom_db_paths(flavor: &Flavor, dir: PathBuf) -> Vec<PathBuf> {
    if dir.is_file() {
        return vec![dir];
    }
    let nested = dir.join(flavor.dir);
    let db_dir = if nested.is_dir() || known_db_paths(flavor, &nested).iter().any(|p| p.is_file()) {
        nested
    } else {
        dir
    };
    known_db_paths(flavor, &db_dir)
}

impl OpencodeAdapter {
    pub fn new() -> Self {
        Self::with_dbs(&OPENCODE, default_db_paths(&OPENCODE))
    }

    /// Kilo Code 的新版(2026-04 起)数据源:`~/.local/share/kilo/kilo.db`。旧版扩展的
    /// 任务目录是同一家的第二个实例(adapters::kilo)
    pub fn kilo() -> Self {
        Self::with_dbs(&KILO, default_db_paths(&KILO))
    }

    fn with_dbs(flavor: &'static Flavor, paths: Vec<PathBuf>) -> Self {
        Self {
            flavor,
            dbs: paths.into_iter().map(OcDb::new).collect(),
        }
    }

    fn open(&self, db: &Path) -> Option<SqliteRo> {
        open_sqlite_ro(db, self.flavor.dir)
    }

    fn rows(&self, db: &OcDb) -> Option<Vec<OcRow>> {
        let mtime = super::sqlite_ro::db_cache_stamp(&db.path);
        db.rows_cache.get_or_try_build(mtime, || {
            let ro = self.open(&db.path)?;
            query_rows(&ro.conn, None).ok()
        })
    }

    fn db_for_ref(&self, r: &SessionFileRef) -> Option<&OcDb> {
        let db_path = Path::new(strip_virtual_path(&r.file_path));
        self.dbs.iter().find(|db| db.path == db_path)
    }

    fn build_meta(
        &self,
        r: &SessionFileRef,
        row: &OcRow,
        db: &Path,
        message_count: i64,
    ) -> SessionMeta {
        let title = clean_title_candidate(&row.title);
        let model = serde_json::from_str::<Value>(&row.model_json)
            .ok()
            .and_then(|m| m.get("id").and_then(|v| v.as_str()).map(String::from));
        let project_path = canonical_project_path(&row.directory);
        SessionMeta {
            key: format!("{}:{}", self.flavor.agent.as_str(), row.id),
            host: String::new(),
            id: row.id.clone(),
            agent: self.flavor.agent,
            title: if title.is_empty() {
                UNTITLED.to_string()
            } else {
                title
            },
            project_name: project_name_of(&project_path),
            project_path,
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
            git_branch: None,
            model,
            tokens_used: if row.tokens > 0 {
                Some(row.tokens)
            } else {
                None
            },
            archived: row.archived,
            source: self.flavor.marks_preview.then(|| row.source(db)).flatten(),
            favorite: false,
            pinned: false,
        }
    }

    /// 单会话解析:一次连接;v2 行命中走 session_message,否则回落 v1 的
    /// message + part 两表路径
    fn parse(
        &self,
        r: &SessionFileRef,
        decode_images: bool,
    ) -> Result<(SessionMeta, Vec<TranscriptMessage>, u32)> {
        let _image_budget = transcript_image_decode_budget(decode_images);
        let db = self
            .db_for_ref(r)
            .ok_or_else(|| anyhow!("opencode database is outside adapter roots"))?;
        let ro = self
            .open(&db.path)
            .ok_or_else(|| anyhow!("cannot open {} db", self.flavor.dir))?;
        let row = query_rows(&ro.conn, Some(&r.native_id))?
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("opencode session {} not in db", r.native_id))?;
        let (messages, unknown) = match row.v2_messages {
            true => parse_v2_messages(&ro, &r.native_id, decode_images)?,
            false => parse_v1_messages(
                &ro,
                &r.native_id,
                decode_images,
                &V1Order::OPENCODE,
                |_, _, _| {},
            )?,
        };
        let count = messages
            .iter()
            .filter(|m| m.kind == MessageKind::Text)
            .count() as i64;
        let meta = self.build_meta(r, &row, &db.path, count);
        Ok((meta, messages, unknown))
    }
}

/// v1 两表路径的排序子句。OpenCode 按 id(前缀时间有序);ZCode 的两张表另有
/// autofill 的 sequence 列,优先按它
pub(crate) struct V1Order {
    pub(crate) part: &'static str,
    pub(crate) message: &'static str,
}

impl V1Order {
    pub(crate) const OPENCODE: V1Order = V1Order {
        part: "message_id, id",
        message: "time_created, id",
    };
}

/// JSON 列直接从 SQLite 的缓冲区解析,不先拷成 String:part 载荷(工具输出)
/// 动辄几十 KB、一条会话几百个。非文本 / 坏 JSON 给 None,调用方计 unknown
fn json_at(r: &rusqlite::Row<'_>, ix: usize) -> rusqlite::Result<Option<Value>> {
    Ok(r.get_ref(ix)?
        .as_str()
        .ok()
        .and_then(|s| serde_json::from_str(s).ok()))
}

/// v1 正文:message 表(角色/时间)+ part 表(内容块)按 message_id 分组。
/// `on_message` 对每一行 message 都调一次——带上已判好的角色,第三个参数是这一行拼出的消息,
/// 全空(如只有 error 的失败 turn)时为 None;ZCode 用它拿 model / token /
/// semantics,OpenCode 传空闭包。两家的差异只在这里和排序子句
pub(crate) fn parse_v1_messages(
    ro: &SqliteRo,
    sid: &str,
    decode_images: bool,
    order: &V1Order,
    mut on_message: impl FnMut(&Value, Role, Option<&mut TranscriptMessage>),
) -> Result<(Vec<TranscriptMessage>, u32)> {
    let mut parts_by_msg: HashMap<String, Vec<Value>> = HashMap::new();
    {
        let mut stmt = ro.conn.prepare(&format!(
            "SELECT message_id, data FROM part WHERE session_id = ?1 ORDER BY {}",
            order.part
        ))?;
        let rows = stmt.query_map([sid], |p| Ok((p.get::<_, String>(0)?, json_at(p, 1)?)))?;
        for (mid, data) in rows.flatten() {
            if let Some(v) = data {
                parts_by_msg.entry(mid).or_default().push(v);
            }
        }
    }

    let mut messages: Vec<TranscriptMessage> = Vec::new();
    let mut unknown = 0u32;
    let mut stmt = ro.conn.prepare(&format!(
        "SELECT id, data FROM message WHERE session_id = ?1 ORDER BY {}",
        order.message
    ))?;
    let msg_rows = stmt.query_map([sid], |m| Ok((m.get::<_, String>(0)?, json_at(m, 1)?)))?;
    for (mid, data) in msg_rows.flatten() {
        let Some(md) = data else {
            unknown += 1;
            continue;
        };
        let role = match md.get("role").and_then(|v| v.as_str()) {
            Some("user") => Role::User,
            Some("assistant") => Role::Assistant,
            _ => Role::System,
        };
        let ts = md
            .get("time")
            .and_then(|t| t.get("created"))
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        let mut acc = BlockAcc::default();
        for p in parts_by_msg.remove(&mid).unwrap_or_default() {
            if !acc.push_part(&p, decode_images) {
                unknown += 1;
            }
        }
        let mut built = acc.into_message(role, ts, None);
        on_message(&md, role, built.as_mut());
        if let Some(m) = built {
            messages.push(m);
        }
    }
    assign_seq(&mut messages);
    Ok((messages, unknown))
}

/// v2 正文:session_message 单表按 seq 有序,type 列分 user/synthetic/assistant,
/// data JSON——user 的 text 在顶层,assistant 的 content 是块数组
fn parse_v2_messages(
    ro: &SqliteRo,
    sid: &str,
    decode_images: bool,
) -> Result<(Vec<TranscriptMessage>, u32)> {
    let mut messages: Vec<TranscriptMessage> = Vec::new();
    let mut unknown = 0u32;
    let mut stmt = ro
        .conn
        .prepare("SELECT type, data FROM session_message WHERE session_id = ?1 ORDER BY seq")?;
    let rows = stmt.query_map([sid], |m| {
        Ok((m.get::<_, String>(0)?, m.get::<_, String>(1)?))
    })?;
    for (mtype, data) in rows.flatten() {
        let Ok(md) = serde_json::from_str::<Value>(&data) else {
            unknown += 1;
            continue;
        };
        let ts = md
            .get("time")
            .and_then(|t| t.get("created"))
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        match mtype.as_str() {
            "user" => {
                let text = md.get("text").and_then(|v| v.as_str()).unwrap_or("").trim();
                let mut parsed = content_parts(
                    md.get("content")
                        .or_else(|| md.get("attachments"))
                        .unwrap_or(&Value::Null),
                    decode_images,
                );
                if text == parsed.text.trim() {
                    parsed.text = text.to_string();
                } else if !text.is_empty() {
                    let mut combined = ParsedContent::default();
                    combined.push_text(text);
                    combined.append(parsed);
                    parsed = combined;
                }
                if !parsed.text.is_empty() || !parsed.images.is_empty() {
                    let mut message = text_msg(Role::User, &parsed.text, ts);
                    message.images = parsed.images;
                    messages.push(message);
                }
            }
            "synthetic" => {
                // 注入内容(编辑器上下文等),归 Meta 折叠
                let text = md.get("text").and_then(|v| v.as_str()).unwrap_or("").trim();
                if !text.is_empty() {
                    let mut m = text_msg(Role::User, text, ts);
                    m.kind = MessageKind::Meta;
                    messages.push(m);
                }
            }
            "system" => {
                let text = md.get("text").and_then(|v| v.as_str()).unwrap_or("").trim();
                if !text.is_empty() {
                    let mut m = text_msg(Role::System, text, ts);
                    m.kind = MessageKind::Meta;
                    messages.push(m);
                }
            }
            "shell" => {
                let command = md.get("command").and_then(|v| v.as_str()).unwrap_or("");
                let input = serde_json::json!({ "command": command });
                let output = md
                    .get("output")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .map(String::from);
                let mut acc = BlockAcc::default();
                acc.tools.push(tool_call_view(
                    md.get("callID")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    "shell",
                    &input,
                    output,
                    false,
                ));
                if let Some(m) = acc.into_message(Role::Assistant, ts, None) {
                    messages.push(m);
                }
            }
            "assistant" => {
                let model = md
                    .pointer("/model/id")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                let mut acc = BlockAcc::default();
                for b in md
                    .get("content")
                    .and_then(|c| c.as_array())
                    .into_iter()
                    .flatten()
                {
                    // v2 的助手块从不带 synthetic,分派与 v1 同一份
                    if !acc.push_part(b, decode_images) {
                        unknown += 1;
                    }
                }
                if let Some(m) = acc.into_message(Role::Assistant, ts, model) {
                    messages.push(m);
                }
            }
            "compaction" => {
                let summary = md
                    .get("summary")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .trim();
                if !summary.is_empty() {
                    let mut m = text_msg(Role::System, summary, ts);
                    m.kind = MessageKind::CompactSummary;
                    messages.push(m);
                }
            }
            // 纯状态切换不形成对话消息,但它们是已知 schema,不计未知行。
            "agent-switched" | "model-switched" => {}
            _ => unknown += 1,
        }
    }
    assign_seq(&mut messages);
    Ok((messages, unknown))
}

/// 一条消息的内容块累加器(text/synthetic/reasoning/tool),两代路径共用
#[derive(Default)]
struct BlockAcc {
    content: ParsedContent,
    synthetic: Vec<String>,
    thinking: Vec<String>,
    tools: Vec<ToolCallView>,
}

impl BlockAcc {
    /// 一个 part 块 → 累加器;返回 false = 表外类型(调用方计 unknown)。
    /// ZCode 的 part 与这里同形(它的运行时是 OpenCode 衍生物),两家共用这一个分派
    fn push_part(&mut self, p: &Value, decode_images: bool) -> bool {
        match p.get("type").and_then(|v| v.as_str()) {
            Some("text") => {
                let t = p.get("text").and_then(|v| v.as_str()).unwrap_or("");
                if t.trim().is_empty() {
                    return true;
                }
                if p.get("synthetic").and_then(|v| v.as_bool()) == Some(true) {
                    self.synthetic.push(t.trim().to_string());
                } else {
                    self.content.push_text(t);
                }
            }
            Some("reasoning") => self.push_reasoning(p),
            Some("tool") => self.push_tool(p, decode_images),
            Some("image") => self.push_image(p, decode_images),
            // 用户敲的命令要派子代理时,这一条用户消息里只有它(没有 text 块):照用户
            // 敲的写回 `/命令`,命令名缺席才退到描述与派给子代理的提示词
            Some("subtask") => {
                let text = match optional_string(p.get("command")) {
                    Some(command) => format!("/{command}"),
                    None => ["description", "prompt"]
                        .iter()
                        .find_map(|k| optional_string(p.get(*k)))
                        .unwrap_or_default(),
                };
                self.content.push_text(&text);
            }
            // agent = 正文里 @ 了哪个代理(文字本身在 text 块里);retry = 请求重试的记录;
            // compaction = 触发压缩的标记(摘要本身是后面那条助手消息)
            Some("step-start") | Some("step-finish") | Some("snapshot") | Some("patch")
            | Some("agent") | Some("retry") | Some("compaction") => {}
            Some("file") if is_image_part(p) => self.push_image(p, decode_images),
            Some("file") => {}
            _ => return false,
        }
        true
    }

    fn push_reasoning(&mut self, b: &Value) {
        if let Some(t) = b.get("text").and_then(|v| v.as_str()) {
            if !t.trim().is_empty() {
                self.thinking.push(t.trim().to_string());
            }
        }
    }

    /// tool 块兼容两种形态:
    /// preview 早期 `{callID,tool,state:{input,output}}` 与真实 next
    /// `{id,name,state:{input,content,result,error}}`。
    fn push_tool(&mut self, b: &Value, decode_images: bool) {
        let state = b.get("state").cloned().unwrap_or(Value::Null);
        let input = state.get("input").cloned().unwrap_or(Value::Null);
        let mut output = opencode_tool_output(&state);
        if let Some(content) = state.get("content") {
            let parsed = content_parts(content, decode_images);
            if !parsed.text.is_empty() {
                output = Some(parsed.text);
            }
            let mut images = parsed.images;
            for image in &mut images {
                image.text_offset = self.content.text.len();
            }
            self.content.images.extend(images);
        }
        self.tools.push(tool_call_view(
            b.get("callID")
                .or_else(|| b.get("id"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            b.get("tool")
                .or_else(|| b.get("name"))
                .and_then(|v| v.as_str())
                .unwrap_or("tool"),
            &input,
            output,
            state.get("status").and_then(|v| v.as_str()) == Some("error"),
        ));
    }

    fn push_image(&mut self, block: &Value, decode_images: bool) {
        let parsed = content_parts(block, decode_images);
        self.content.append(parsed);
    }

    /// 组装为消息;全空返回 None。只有注入内容的消息归 Meta 折叠。
    fn into_message(self, role: Role, ts: i64, model: Option<String>) -> Option<TranscriptMessage> {
        let ParsedContent { text, images } = self.content;
        let (text, kind) = if text.is_empty() && !self.synthetic.is_empty() {
            (self.synthetic.join("\n\n"), MessageKind::Meta)
        } else {
            (text, MessageKind::Text)
        };
        if text.is_empty() && self.thinking.is_empty() && self.tools.is_empty() && images.is_empty()
        {
            return None;
        }
        let (clipped, truncated) = clip(&text, MAX_MSG_TEXT);
        Some(TranscriptMessage {
            seq: 0,
            role,
            kind,
            text: clipped,
            truncated,
            tool_calls: self.tools,
            thinking: if self.thinking.is_empty() {
                None
            } else {
                Some(clip(&self.thinking.join("\n\n"), MAX_TOOL_IO).0)
            },
            timestamp: if ts > 0 { Some(ts) } else { None },
            model,
            images,
        })
    }
}

fn opencode_tool_output(state: &Value) -> Option<String> {
    if let Some(output) = state.get("output") {
        return match output {
            Value::String(s) if !s.is_empty() => Some(s.clone()),
            v if !v.is_null() => serde_json::to_string(v).ok(),
            _ => None,
        };
    }
    if let Some(content) = state.get("content").and_then(|v| v.as_array()) {
        let rendered = content
            .iter()
            .filter_map(|item| match item.get("type").and_then(|v| v.as_str()) {
                Some("text") => item.get("text").and_then(|v| v.as_str()).map(String::from),
                Some("file") => item
                    .get("name")
                    .or_else(|| item.get("uri"))
                    .and_then(|v| v.as_str())
                    .map(|s| format!("[file: {s}]")),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        if !rendered.is_empty() {
            return Some(rendered);
        }
    }
    if let Some(result) = state.get("result").filter(|v| !v.is_null()) {
        return match result {
            Value::String(s) => Some(s.clone()),
            v => serde_json::to_string(v).ok(),
        };
    }
    state
        .pointer("/error/message")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(String::from)
}

#[derive(Clone)]
struct OcRow {
    id: String,
    directory: String,
    title: String,
    created_ms: i64,
    updated_ms: i64,
    model_json: String,
    tokens: i64,
    archived: bool,
    /// 会话产生时的 CLI 版本("1.14.50" / "0.0.0-beta-17639"),v2 迁移保留原值
    version: String,
    content_len: i64,
    /// 这条会话的正文实际来自 session_message,而不是旧 message + part。
    v2_messages: bool,
}

impl OcRow {
    /// preview 会话在 UI 标 "opencode2",resume 据此换二进制。next 库名本身
    /// 是最强信号;共享库/早期 schema 则回看写入 session.version 的渠道标记。
    fn source(&self, db: &Path) -> Option<String> {
        let next_db = db.file_name().and_then(|n| n.to_str()) == Some(NEXT_DB);
        let v2 = next_db
            || self.version.starts_with('2')
            || self.version.contains("beta")
            || self.version.contains("next");
        v2.then(|| "opencode2".to_string())
    }
}

impl AgentAdapter for OpencodeAdapter {
    fn agent(&self) -> AgentId {
        self.flavor.agent
    }

    fn list_session_files(&self) -> Result<Vec<SessionFileRef>> {
        let mut out = Vec::new();
        for db in &self.dbs {
            let Some(rows) = self.rows(db) else { continue };
            out.extend(
                rows.into_iter()
                    .filter(|row| row.content_len > 0)
                    .map(|row| SessionFileRef {
                        agent: self.flavor.agent,
                        native_id: row.id.clone(),
                        file_path: virtual_path(&db.path, &row.id),
                        mtime_ms: row.updated_ms,
                        size: row.content_len,
                    }),
            );
        }
        Ok(out)
    }

    fn quick_meta(&self, refs: &[SessionFileRef]) -> Option<HashMap<String, SessionMeta>> {
        let mut out = HashMap::new();
        let mut opened = false;
        for db in &self.dbs {
            let Some(rows) = self.rows(db) else { continue };
            opened = true;
            let by_id: HashMap<&str, &OcRow> =
                rows.iter().map(|row| (row.id.as_str(), row)).collect();
            for r in refs
                .iter()
                .filter(|r| Path::new(strip_virtual_path(&r.file_path)) == db.path.as_path())
            {
                if let Some(row) = by_id.get(r.native_id.as_str()) {
                    out.insert(r.file_path.clone(), self.build_meta(r, row, &db.path, 0));
                }
            }
        }
        opened.then_some(out)
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

    fn with_custom_root(&self, dir: PathBuf) -> Box<dyn AgentAdapter> {
        // 目录 location 同时扫描 stable + next,直接给库文件则只认该文件。
        // 选中 XDG data 根或 opencode / kilo 目录本身都可归一化(Kilo 旧版扩展的任务
        // 目录由旧版实例认领,见 adapters::kilo)
        Box::new(Self::with_dbs(
            self.flavor,
            custom_db_paths(self.flavor, dir),
        ))
    }

    fn data_roots(&self) -> Vec<PathBuf> {
        self.dbs.iter().map(|db| db.path.clone()).collect()
    }

    fn supports_individual_root_removal(&self) -> bool {
        true
    }

    /// Kilo 一家两源(本实例的 kilo.db 与 adapters::kilo 的旧版任务目录):排除列表按
    /// agent 收集,里面可能全是另一个源的根——那时原样交回自己,与"与我无关"等价
    fn excluding_data_roots(&self, roots: &[PathBuf]) -> Option<Box<dyn AgentAdapter>> {
        Some(Box::new(Self::with_dbs(
            self.flavor,
            self.dbs
                .iter()
                .filter(|db| !roots.contains(&db.path))
                .map(|db| db.path.clone())
                .collect(),
        )))
    }
}
