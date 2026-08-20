use super::parse_utils::*;
use super::sqlite_ro::{open_sqlite_ro, virtual_path, SqliteRo};
use super::{units_from_messages, AgentAdapter};
use crate::models::*;
use anyhow::{anyhow, Result};
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;

/// OpenCode:`~/.local/share/opencode/opencode.db`,v1 与 v2 beta 共库不同表。
/// v1:session 表 + 正文在 part 表({type:text|reasoning|tool},synthetic=注入),
/// message 表只有角色与时间。v2(binary `opencode2`):session_v2 表 + 正文在
/// session_message 表(type 列 user|synthetic|assistant,data JSON——user 的
/// text 在顶层,assistant 的 content 是块数组)。v2 首次启动会把 v1 会话迁移进
/// session_v2(version 列保留原 CLI 版本),因此 v2 表是全集;仅存于 v1 表的
/// 会话(迁移后又用 v1 CLI 跑的)由 UNION 回捞。parent_id 非空 = 子代理,不进列表。
pub struct OpencodeAdapter {
    db: PathBuf,
    /// rows() 带按会话相关子查询(全 part/message 表求和),按 db mtime 缓存
    /// 一轮扫描内的重复调用
    rows_cache: MtimeCache<Vec<OcRow>>,
}

/// 两代 session 表的公共列(别名 s;content_len 子查询两代不同,单独拼)
const ROW_COLS: &str = "s.id, s.directory, s.title, s.time_created, s.time_updated,
        s.model, s.tokens_input + s.tokens_output + s.tokens_reasoning,
        s.time_archived, s.version";

fn v1_select() -> String {
    format!(
        "SELECT {ROW_COLS},
            (SELECT COALESCE(SUM(LENGTH(p.data)), 0) FROM part p WHERE p.session_id = s.id)
         FROM session s"
    )
}

fn v2_select() -> String {
    format!(
        "SELECT {ROW_COLS},
            (SELECT COALESCE(SUM(LENGTH(m.data)), 0) FROM session_message m WHERE m.session_id = s.id)
         FROM session_v2 s"
    )
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
    })
}

fn has_v2(conn: &rusqlite::Connection) -> bool {
    conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type='table' AND name='session_v2'",
        [],
        |_| Ok(()),
    )
    .is_ok()
}

impl OpencodeAdapter {
    pub fn new() -> Self {
        let home = crate::home_dir();
        let legacy = home
            .join(".local")
            .join("share")
            .join("opencode")
            .join("opencode.db");
        // OpenCode follows XDG on Unix and the per-user application-data
        // directory on Windows. Keep the legacy path as a fallback because
        // existing Windows installs may have been configured with XDG_DATA_HOME.
        let platform = dirs::data_dir()
            .unwrap_or_else(|| home.clone())
            .join("opencode")
            .join("opencode.db");
        Self {
            db: if cfg!(windows) && !legacy.exists() {
                platform
            } else {
                legacy
            },
            rows_cache: MtimeCache::new(),
        }
    }

    fn open(&self) -> Option<SqliteRo> {
        open_sqlite_ro(&self.db, "opencode")
    }

    fn rows(&self) -> Option<Vec<OcRow>> {
        let mtime = std::fs::metadata(&self.db).map(|m| mtime_ms(&m)).unwrap_or(0);
        self.rows_cache.get_or_try_build(mtime, || {
            let ro = self.open()?;
            let sql = if has_v2(&ro.conn) {
                format!(
                    "{} WHERE s.parent_id IS NULL \
                     UNION ALL \
                     {} WHERE s.parent_id IS NULL AND s.id NOT IN (SELECT id FROM session_v2)",
                    v2_select(),
                    v1_select()
                )
            } else {
                format!("{} WHERE s.parent_id IS NULL", v1_select())
            };
            let mut stmt = ro.conn.prepare(&sql).ok()?;
            let rows = stmt
                .query_map([], row_from)
                .ok()?
                .collect::<rusqlite::Result<Vec<_>>>()
                .ok()?;
            Some(rows)
        })
    }

    fn build_meta(&self, r: &SessionFileRef, row: &OcRow, message_count: i64) -> SessionMeta {
        let title = clean_title_candidate(&row.title);
        let model = serde_json::from_str::<Value>(&row.model_json)
            .ok()
            .and_then(|m| m.get("id").and_then(|v| v.as_str()).map(String::from));
        SessionMeta {
            key: format!("opencode:{}", row.id),
            id: row.id.clone(),
            agent: AgentId::Opencode,
            title: if title.is_empty() { UNTITLED.to_string() } else { title },
            project_path: row.directory.clone(),
            project_name: project_name_of(&row.directory),
            file_path: r.file_path.clone(),
            created_at: if row.created_ms > 0 { row.created_ms } else { r.mtime_ms },
            updated_at: if row.updated_ms > 0 { row.updated_ms } else { r.mtime_ms },
            message_count,
            size_bytes: r.size,
            git_branch: None,
            model,
            tokens_used: if row.tokens > 0 { Some(row.tokens) } else { None },
            archived: row.archived,
            source: row.source(),
            favorite: false,
            pinned: false,
        }
    }

    /// 单会话解析:一次连接;v2 行命中走 session_message,否则回落 v1 的
    /// message + part 两表路径
    fn parse(&self, r: &SessionFileRef) -> Result<(SessionMeta, Vec<TranscriptMessage>, u32)> {
        let ro = self.open().ok_or_else(|| anyhow!("cannot open opencode db"))?;
        let v2_row = if has_v2(&ro.conn) {
            ro.conn
                .query_row(&format!("{} WHERE s.id = ?1", v2_select()), [&r.native_id], row_from)
                .ok()
        } else {
            None
        };
        let (row, messages, unknown) = match v2_row {
            Some(row) => {
                let (messages, unknown) = parse_v2_messages(&ro, &r.native_id)?;
                (row, messages, unknown)
            }
            None => {
                let row = ro
                    .conn
                    .query_row(&format!("{} WHERE s.id = ?1", v1_select()), [&r.native_id], row_from)
                    .map_err(|_| anyhow!("opencode session {} not in db", r.native_id))?;
                let (messages, unknown) = parse_v1_messages(&ro, &r.native_id)?;
                (row, messages, unknown)
            }
        };
        let count = messages.iter().filter(|m| m.kind == MessageKind::Text).count() as i64;
        let meta = self.build_meta(r, &row, count);
        Ok((meta, messages, unknown))
    }
}

/// v1 正文:message 表(角色/时间)+ part 表(内容块)按 message_id 分组
fn parse_v1_messages(ro: &SqliteRo, sid: &str) -> Result<(Vec<TranscriptMessage>, u32)> {
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
    let mut stmt = ro.conn.prepare(
        "SELECT id, data FROM message WHERE session_id = ?1 ORDER BY time_created, id",
    )?;
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
                        acc.text.push(t.trim().to_string());
                    }
                }
                Some("reasoning") => acc.push_reasoning(&p),
                Some("tool") => acc.push_tool(&p),
                Some("step-start") | Some("step-finish") | Some("snapshot")
                | Some("patch") | Some("file") => {}
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
fn parse_v2_messages(ro: &SqliteRo, sid: &str) -> Result<(Vec<TranscriptMessage>, u32)> {
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
                if !text.is_empty() {
                    messages.push(text_msg(Role::User, text, ts));
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
            "assistant" => {
                let model = md
                    .pointer("/model/id")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                let mut acc = BlockAcc::default();
                for b in md.get("content").and_then(|c| c.as_array()).into_iter().flatten() {
                    match b.get("type").and_then(|v| v.as_str()) {
                        Some("text") => {
                            if let Some(t) = b.get("text").and_then(|v| v.as_str()) {
                                if !t.trim().is_empty() {
                                    acc.text.push(t.trim().to_string());
                                }
                            }
                        }
                        Some("reasoning") => acc.push_reasoning(b),
                        Some("tool") => acc.push_tool(b),
                        Some("step-start") | Some("step-finish") | Some("snapshot")
                        | Some("patch") | Some("file") => {}
                        _ => unknown += 1,
                    }
                }
                if let Some(m) = acc.into_message(Role::Assistant, ts, model) {
                    messages.push(m);
                }
            }
            _ => unknown += 1,
        }
    }
    assign_seq(&mut messages);
    Ok((messages, unknown))
}

/// 一条消息的内容块累加器(text/synthetic/reasoning/tool),两代路径共用
#[derive(Default)]
struct BlockAcc {
    text: Vec<String>,
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

    /// tool 块(两代同构):{callID,tool,state:{status,input,output}}
    fn push_tool(&mut self, b: &Value) {
        let state = b.get("state").cloned().unwrap_or(Value::Null);
        let input = state.get("input").cloned().unwrap_or(Value::Null);
        let output = match state.get("output") {
            Some(Value::String(s)) => Some(s.clone()),
            Some(v) if !v.is_null() => serde_json::to_string(v).ok(),
            _ => None,
        };
        self.tools.push(tool_call_view(
            b.get("callID").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            b.get("tool").and_then(|v| v.as_str()).unwrap_or("tool"),
            &input,
            output,
            state.get("status").and_then(|v| v.as_str()) == Some("error"),
        ));
    }

    /// 组装为消息;全空返回 None。只有注入内容的消息归 Meta 折叠。
    fn into_message(self, role: Role, ts: i64, model: Option<String>) -> Option<TranscriptMessage> {
        let (text, kind) = if self.text.is_empty() && !self.synthetic.is_empty() {
            (self.synthetic.join("\n\n"), MessageKind::Meta)
        } else {
            (self.text.join("\n\n"), MessageKind::Text)
        };
        if text.is_empty() && self.thinking.is_empty() && self.tools.is_empty() {
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
        })
    }
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
}

impl OcRow {
    /// v2 会话在 UI 标 "opencode2"(官方产品名 OpenCode 2,binary 同名),
    /// resume 据此换 opencode2 二进制;1.x/0.x 老版本不标
    fn source(&self) -> Option<String> {
        let v2 = self.version.starts_with('2') || self.version.contains("beta");
        v2.then(|| "opencode2".to_string())
    }
}

impl AgentAdapter for OpencodeAdapter {
    fn agent(&self) -> AgentId {
        AgentId::Opencode
    }

    fn detect(&self) -> bool {
        self.db.is_file()
    }

    fn list_session_files(&self) -> Result<Vec<SessionFileRef>> {
        let Some(rows) = self.rows() else {
            return Ok(Vec::new());
        };
        Ok(rows
            .into_iter()
            .filter(|row| row.content_len > 0)
            .map(|row| SessionFileRef {
                agent: AgentId::Opencode,
                native_id: row.id.clone(),
                file_path: virtual_path(&self.db, &row.id),
                mtime_ms: row.updated_ms,
                size: row.content_len,
            })
            .collect())
    }

    fn quick_meta(&self, refs: &[SessionFileRef]) -> Option<HashMap<String, SessionMeta>> {
        let rows = self.rows()?;
        let by_id: HashMap<&str, &OcRow> = rows.iter().map(|r| (r.id.as_str(), r)).collect();
        let mut out = HashMap::new();
        for r in refs {
            if let Some(row) = by_id.get(r.native_id.as_str()) {
                out.insert(r.file_path.clone(), self.build_meta(r, row, 0));
            }
        }
        Some(out)
    }

    fn parse_session(&self, r: &SessionFileRef) -> Result<ParsedSession> {
        let (meta, messages, unknown) = self.parse(r)?;
        let units = units_from_messages(&messages);
        Ok(ParsedSession {
            meta,
            units,
            unknown_line_count: unknown,
        })
    }

    fn parse_transcript(&self, r: &SessionFileRef) -> Result<ParsedTranscript> {
        let (meta, messages, unknown) = self.parse(r)?;
        Ok(ParsedTranscript {
            meta,
            mainline: messages,
            sidechains: Vec::new(),
            unknown_line_count: unknown,
        })
    }

    fn watch_paths(&self) -> Vec<PathBuf> {
        Vec::new()
    }
}
