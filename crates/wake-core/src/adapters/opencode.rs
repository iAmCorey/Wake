use super::parse_utils::*;
use super::sqlite_ro::{open_sqlite_ro, strip_virtual_path, virtual_path, SqliteRo};
use super::{units_from_messages, AgentAdapter};
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
pub struct OpencodeAdapter {
    dbs: Vec<OcDb>,
}

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
    let message_exists = "EXISTS(SELECT 1 FROM session_message m WHERE m.session_id = s.id)";
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
                format!("CASE WHEN {message_exists} THEN {message_len} ELSE {part_len} END"),
                format!("CASE WHEN {message_exists} THEN 1 ELSE 0 END"),
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

fn known_db_paths(dir: &Path) -> Vec<PathBuf> {
    vec![dir.join("opencode.db"), dir.join("opencode-next.db")]
}

/// OpenCode 的数据目录服从 XDG;next 渠道另命名数据库而非替换 stable 库。
/// 两个固定候选都常驻 roster,这样 Wake 启动后才安装任一 CLI,普通刷新也能发现。
/// OPENCODE_DB 若被 GUI 进程继承则作为额外候选,但绝不压掉两个标准位置。
fn default_db_paths() -> Vec<PathBuf> {
    let default_dir = super::home_dir()
        .unwrap_or_default()
        .join(".local")
        .join("share")
        .join("opencode");
    let xdg_dir = super::env_dir("XDG_DATA_HOME").map(|x| x.join("opencode"));
    let active_dir = xdg_dir
        .as_ref()
        .filter(|dir| known_db_paths(dir).iter().any(|p| p.is_file()))
        .unwrap_or(&default_dir);

    let mut paths = Vec::new();
    if let Some(value) = std::env::var_os("OPENCODE_DB").filter(|v| !v.is_empty()) {
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
    for path in known_db_paths(active_dir) {
        push_unique(&mut paths, path);
    }
    // XDG 位置被采信时,默认目录里已有的库仍是只读候选,避免稳定版历史消失。
    if active_dir != &default_dir {
        for path in known_db_paths(&default_dir)
            .into_iter()
            .filter(|p| p.is_file())
        {
            push_unique(&mut paths, path);
        }
    }
    paths
}

fn custom_db_paths(dir: PathBuf) -> Vec<PathBuf> {
    if dir.is_file() {
        return vec![dir];
    }
    let nested = dir.join("opencode");
    let db_dir = if nested.is_dir() || known_db_paths(&nested).iter().any(|p| p.is_file()) {
        nested
    } else {
        dir
    };
    known_db_paths(&db_dir)
}

impl OpencodeAdapter {
    pub fn new() -> Self {
        Self {
            dbs: default_db_paths().into_iter().map(OcDb::new).collect(),
        }
    }

    fn open(db: &Path) -> Option<SqliteRo> {
        open_sqlite_ro(db, "opencode")
    }

    fn rows(db: &OcDb) -> Option<Vec<OcRow>> {
        let mtime = std::fs::metadata(&db.path)
            .map(|m| mtime_ms(&m))
            .unwrap_or(0);
        db.rows_cache.get_or_try_build(mtime, || {
            let ro = Self::open(&db.path)?;
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
        SessionMeta {
            key: format!("opencode:{}", row.id),
            id: row.id.clone(),
            agent: AgentId::Opencode,
            title: if title.is_empty() {
                UNTITLED.to_string()
            } else {
                title
            },
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
            source: row.source(db),
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
        let ro = Self::open(&db.path).ok_or_else(|| anyhow!("cannot open opencode db"))?;
        let row = query_rows(&ro.conn, Some(&r.native_id))?
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("opencode session {} not in db", r.native_id))?;
        let (messages, unknown) = match row.v2_messages {
            true => parse_v2_messages(&ro, &r.native_id, decode_images)?,
            false => parse_v1_messages(&ro, &r.native_id, decode_images)?,
        };
        let count = messages
            .iter()
            .filter(|m| m.kind == MessageKind::Text)
            .count() as i64;
        let meta = self.build_meta(r, &row, &db.path, count);
        Ok((meta, messages, unknown))
    }
}

/// v1 正文:message 表(角色/时间)+ part 表(内容块)按 message_id 分组
fn parse_v1_messages(
    ro: &SqliteRo,
    sid: &str,
    decode_images: bool,
) -> Result<(Vec<TranscriptMessage>, u32)> {
    // part 按 (message_id, id) 排序分组;id 前缀时间有序
    let mut parts_by_msg: HashMap<String, Vec<Value>> = HashMap::new();
    {
        let mut stmt = ro.conn.prepare(
            "SELECT message_id, data FROM part WHERE session_id = ?1 ORDER BY message_id, id",
        )?;
        let rows = stmt.query_map([sid], |p| {
            Ok((p.get::<_, String>(0)?, p.get::<_, String>(1)?))
        })?;
        for (mid, data) in rows.flatten() {
            if let Ok(v) = serde_json::from_str::<Value>(&data) {
                parts_by_msg.entry(mid).or_default().push(v);
            }
        }
    }

    let mut messages: Vec<TranscriptMessage> = Vec::new();
    let mut unknown = 0u32;
    let mut stmt = ro
        .conn
        .prepare("SELECT id, data FROM message WHERE session_id = ?1 ORDER BY time_created, id")?;
    let msg_rows = stmt.query_map([sid], |m| {
        Ok((m.get::<_, String>(0)?, m.get::<_, String>(1)?))
    })?;
    for (mid, data) in msg_rows.flatten() {
        let Ok(md) = serde_json::from_str::<Value>(&data) else {
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
            match p.get("type").and_then(|v| v.as_str()) {
                Some("text") => {
                    let t = p.get("text").and_then(|v| v.as_str()).unwrap_or("");
                    if t.trim().is_empty() {
                        continue;
                    }
                    if p.get("synthetic").and_then(|v| v.as_bool()) == Some(true) {
                        acc.synthetic.push(t.trim().to_string());
                    } else {
                        acc.content.push_text(t);
                    }
                }
                Some("reasoning") => acc.push_reasoning(&p),
                Some("tool") => acc.push_tool(&p, decode_images),
                Some("image") => acc.push_image(&p, decode_images),
                Some("step-start") | Some("step-finish") | Some("snapshot") | Some("patch") => {}
                Some("file") if is_image_part(&p) => acc.push_image(&p, decode_images),
                Some("file") => {}
                _ => unknown += 1,
            }
        }
        if let Some(m) = acc.into_message(role, ts, None) {
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
                    match b.get("type").and_then(|v| v.as_str()) {
                        Some("text") => {
                            if let Some(t) = b.get("text").and_then(|v| v.as_str()) {
                                if !t.trim().is_empty() {
                                    acc.content.push_text(t);
                                }
                            }
                        }
                        Some("reasoning") => acc.push_reasoning(b),
                        Some("tool") => acc.push_tool(b, decode_images),
                        Some("image") => acc.push_image(b, decode_images),
                        Some("step-start") | Some("step-finish") | Some("snapshot")
                        | Some("patch") => {}
                        Some("file") if is_image_part(b) => acc.push_image(b, decode_images),
                        Some("file") => {}
                        _ => unknown += 1,
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
        let next_db = db.file_name().and_then(|n| n.to_str()) == Some("opencode-next.db");
        let v2 = next_db
            || self.version.starts_with('2')
            || self.version.contains("beta")
            || self.version.contains("next");
        v2.then(|| "opencode2".to_string())
    }
}

impl AgentAdapter for OpencodeAdapter {
    fn agent(&self) -> AgentId {
        AgentId::Opencode
    }

    fn list_session_files(&self) -> Result<Vec<SessionFileRef>> {
        let mut out = Vec::new();
        for db in &self.dbs {
            let Some(rows) = Self::rows(db) else { continue };
            out.extend(
                rows.into_iter()
                    .filter(|row| row.content_len > 0)
                    .map(|row| SessionFileRef {
                        agent: AgentId::Opencode,
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
            let Some(rows) = Self::rows(db) else { continue };
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
        let units = units_from_messages(&messages);
        Ok(ParsedSession {
            meta,
            units,
            unknown_line_count: unknown,
        })
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
        // 选中 XDG data 根或 opencode 目录本身都可归一化。
        Box::new(Self {
            dbs: custom_db_paths(dir).into_iter().map(OcDb::new).collect(),
        })
    }

    fn data_roots(&self) -> Vec<PathBuf> {
        self.dbs.iter().map(|db| db.path.clone()).collect()
    }

    fn supports_individual_root_removal(&self) -> bool {
        true
    }

    fn excluding_data_roots(&self, roots: &[PathBuf]) -> Option<Box<dyn AgentAdapter>> {
        Some(Box::new(Self {
            dbs: self
                .dbs
                .iter()
                .filter(|db| !roots.contains(&db.path))
                .map(|db| OcDb::new(db.path.clone()))
                .collect(),
        }))
    }
}
