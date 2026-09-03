use super::parse_utils::*;
use super::sqlite_ro::{open_sqlite_ro, strip_virtual_path, virtual_path, SqliteRo};
use super::{units_from_messages, AgentAdapter};
use crate::models::*;
use anyhow::{anyhow, Result};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Hermes Agent(Nous Research):`~/.hermes/state.db` 全明文 SQLite
/// (`HERMES_HOME` 可改道;`profiles/<name>/state.db` 是多档案,同扫)。
/// sessions(id,source,model,title,started_at,ended_at,token 各列)+
/// messages(role,content,tool_calls,tool_call_id,tool_name,reasoning,timestamp)。
/// 时间戳是 unix **秒**(REAL),tool_calls 是 JSON 数组——写端两种形状并存:
/// Hermes 自家精简版 `[{name,arguments}]`(arguments 多为 JSON 字符串)与
/// OpenAI 原样 `[{id,type,function:{name,arguments}}]`;工具结果是独立
/// `role="tool"` 行,按 tool_call_id 回填,精简形状没有 id 时按 tool_name
/// 顺位回填。`source` 是启动面(cli/telegram/discord/…),`tool` 是
/// session_search 工具内部起的检索会话,不列。会话行没有 cwd(Hermes 是
/// 通用助手,不按项目分桶),project 留空。
/// 无每会话文件,SessionFileRef 用虚拟路径 `<db>#<id>`;多库结构与 opencode 同款。
pub struct HermesAdapter {
    dbs: Vec<HermesDb>,
}

struct HermesDb {
    path: PathBuf,
    /// 聚合行按库(含 -wal)mtime 缓存一轮扫描内的重复调用
    rows_cache: MtimeCache<Vec<HermesRow>>,
    /// 各迁移版本才有的列,每库探一次(见 HermesSchema)
    schema: OnceLock<HermesSchema>,
}

impl HermesDb {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            rows_cache: MtimeCache::new(),
            schema: OnceLock::new(),
        }
    }
}

/// Hermes 库是按 schema_version 逐版 ALTER 上来的(hermes_state.py):title 列 v3、
/// cache_*/reasoning_tokens v5、messages.reasoning v6。老版本 Hermes 写出的库
/// 没升过就没有这些列,查询里硬引用会让 prepare 失败、整家会话消失——缺列
/// 一律用默认表达式顶上
#[derive(Clone, Copy)]
struct HermesSchema {
    title: bool,
    cache_read: bool,
    cache_write: bool,
    reasoning_tokens: bool,
    msg_reasoning: bool,
}

impl HermesSchema {
    fn probe(conn: &rusqlite::Connection) -> Self {
        let has = |table: &str, col: &str| {
            conn.prepare(&format!(
                "SELECT 1 FROM pragma_table_info('{table}') WHERE name = ?1"
            ))
            .and_then(|mut s| s.exists([col]))
            .unwrap_or(false)
        };
        Self {
            title: has("sessions", "title"),
            cache_read: has("sessions", "cache_read_tokens"),
            cache_write: has("sessions", "cache_write_tokens"),
            reasoning_tokens: has("sessions", "reasoning_tokens"),
            msg_reasoning: has("messages", "reasoning"),
        }
    }
}

/// 一个 Hermes home 下的全部库:主库 + `profiles/<name>/state.db`(构造时刻
/// 快照,不变量 8);默认实例与目录形态的自定义根同一枚举,档案不会静默缺失
fn dbs_under(root: &Path) -> Vec<PathBuf> {
    let mut paths = vec![root.join("state.db")];
    if let Ok(entries) = std::fs::read_dir(root.join("profiles")) {
        let mut extra: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path().join("state.db"))
            .filter(|p| p.is_file())
            .collect();
        extra.sort();
        paths.extend(extra);
    }
    paths
}

/// 会话所属的 Hermes profile 名(resume 用):`profiles/<name>/state.db` → name,
/// 其余(主库、远程缓存里的主库)→ "default"。Hermes 的 `--profile` 显式覆盖
/// sticky 的 active_profile,不带它时在别的档案激活期间会找不到会话
pub fn profile_of(file_path: &str) -> String {
    let db = Path::new(strip_virtual_path(file_path));
    db.parent()
        .filter(|dir| dir.parent().and_then(Path::file_name) == Some("profiles".as_ref()))
        .and_then(Path::file_name)
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "default".to_string())
}

impl HermesAdapter {
    pub fn new() -> Self {
        let home = super::home_dir().unwrap_or_default();
        // HERMES_HOME 只是候选:指向的目录里有 state.db 才采信,否则回落 ~/.hermes
        let root = super::env_dir("HERMES_HOME")
            .filter(|d| d.join("state.db").is_file())
            .unwrap_or_else(|| home.join(".hermes"));
        Self::with_paths(dbs_under(&root))
    }

    fn with_paths(paths: Vec<PathBuf>) -> Self {
        Self {
            dbs: paths.into_iter().map(HermesDb::new).collect(),
        }
    }

    fn rows(db: &HermesDb) -> Option<Vec<HermesRow>> {
        let stamp = super::sqlite_ro::db_cache_stamp(&db.path);
        db.rows_cache.get_or_try_build(stamp, || {
            let ro = open_sqlite_ro(&db.path, "hermes")?;
            query_rows(&ro, db)
        })
    }

    fn db_for_ref(&self, r: &SessionFileRef) -> Option<&HermesDb> {
        let db_path = Path::new(strip_virtual_path(&r.file_path));
        self.dbs.iter().find(|db| db.path == db_path)
    }

    fn build_meta(
        r: &SessionFileRef,
        row: &HermesRow,
        messages: &[TranscriptMessage],
    ) -> SessionMeta {
        let title = Some(clean_title_candidate(&row.title))
            .filter(|t| !t.is_empty())
            .or_else(|| title_from_messages(messages))
            .unwrap_or_else(|| UNTITLED.to_string());
        SessionMeta {
            key: format!("hermes:{}", row.id),
            host: String::new(),
            id: row.id.clone(),
            agent: AgentId::Hermes,
            title,
            project_path: String::new(),
            project_name: project_name_of(""),
            file_path: r.file_path.clone(),
            created_at: if row.started_ms > 0 {
                row.started_ms
            } else {
                r.mtime_ms
            },
            updated_at: if row.updated_ms > 0 {
                row.updated_ms
            } else {
                r.mtime_ms
            },
            message_count: messages
                .iter()
                .filter(|m| m.kind == MessageKind::Text)
                .count() as i64,
            size_bytes: r.size,
            git_branch: None,
            model: row.model.clone().filter(|m| !m.is_empty()),
            tokens_used: (row.tokens > 0).then_some(row.tokens),
            archived: false,
            // cli 是默认启动面,徽章只给非 cli 的(telegram/discord/…)
            source: row.source.clone().filter(|s| !s.is_empty() && s != "cli"),
            favorite: false,
            pinned: false,
        }
    }

    /// 单会话解析:会话行取缓存的聚合表,只有消息按会话查
    fn parse(&self, r: &SessionFileRef) -> Result<(SessionMeta, Vec<TranscriptMessage>)> {
        let db = self
            .db_for_ref(r)
            .ok_or_else(|| anyhow!("hermes store for {} not in this instance", r.file_path))?;
        let row = Self::rows(db)
            .and_then(|rows| rows.into_iter().find(|row| row.id == r.native_id))
            .ok_or_else(|| anyhow!("hermes session {} not in store", r.native_id))?;
        let ro = open_sqlite_ro(&db.path, "hermes")
            .ok_or_else(|| anyhow!("cannot open hermes store"))?;
        let schema = *db.schema.get_or_init(|| HermesSchema::probe(&ro.conn));
        let reasoning_col = if schema.msg_reasoning {
            "reasoning"
        } else {
            "NULL"
        };
        let mut stmt = ro.conn.prepare(&format!(
            "SELECT id, role, content, tool_call_id, tool_calls, tool_name, {reasoning_col}, timestamp
             FROM messages WHERE session_id = ?1 ORDER BY timestamp, id"
        ))?;
        let msgs = stmt.query_map([&r.native_id], |m| {
            Ok(RawMsg {
                id: m.get(0)?,
                role: m.get::<_, Option<String>>(1)?.unwrap_or_default(),
                content: m.get(2)?,
                tool_call_id: m.get(3)?,
                tool_calls: m.get(4)?,
                tool_name: m.get(5)?,
                reasoning: m.get(6)?,
                ts_ms: secs_to_ms(m.get::<_, Option<f64>>(7)?.unwrap_or(0.0)),
            })
        })?;

        let mut messages: Vec<TranscriptMessage> = Vec::new();
        // tool_call_id → (消息下标, tool_calls 下标)
        let mut by_id: HashMap<String, (usize, usize)> = HashMap::new();
        // 精简形状没有 id:尚未回填的调用按出现顺序排队,tool 行按 tool_name 认领
        let mut pending: Vec<(usize, usize)> = Vec::new();
        for m in msgs.flatten() {
            match m.role.as_str() {
                "user" => {
                    let text = content_text(m.content.as_deref());
                    if !text.is_empty() {
                        messages.push(text_msg(Role::User, &text, m.ts_ms));
                    }
                }
                "assistant" => {
                    let text = content_text(m.content.as_deref());
                    let calls = tool_calls_of(m.tool_calls.as_deref(), m.id);
                    let thinking = m
                        .reasoning
                        .as_deref()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(String::from);
                    if text.is_empty() && calls.is_empty() && thinking.is_none() {
                        continue;
                    }
                    // 连续 assistant 行(中间只隔 tool 行)合并成一条,与 pi 同款
                    if !matches!(messages.last(), Some(l) if l.role == Role::Assistant) {
                        messages.push(text_msg(Role::Assistant, "", m.ts_ms));
                    }
                    let base = messages.len() - 1;
                    let last = &mut messages[base];
                    if !text.is_empty() {
                        let mut part = ParsedContent::default();
                        part.push_text(&text);
                        append_content_to_message(last, part, "\n\n");
                        if last.text.len() > MAX_MSG_TEXT {
                            let (t, _) = clip(&last.text, MAX_MSG_TEXT);
                            last.text = t;
                            last.truncated = true;
                        }
                    }
                    if let Some(t) = thinking {
                        let merged = match last.thinking.take() {
                            Some(existing) => format!("{existing}\n\n{t}"),
                            None => t,
                        };
                        last.thinking = Some(clip(&merged, MAX_TOOL_IO).0);
                    }
                    for (has_id, tc) in calls {
                        let slot = (base, last.tool_calls.len());
                        if has_id {
                            by_id.insert(tc.id.clone(), slot);
                        }
                        pending.push(slot);
                        last.tool_calls.push(tc);
                    }
                }
                "tool" => {
                    let output = content_text(m.content.as_deref());
                    let name = m.tool_name.as_deref().unwrap_or_default();
                    let slot = m
                        .tool_call_id
                        .as_deref()
                        .and_then(|id| by_id.get(id).copied())
                        .or_else(|| {
                            // 无 id:同名的最早待回填项,再退最早的待回填项
                            let unfilled = |&(mi, ti): &(usize, usize)| {
                                messages[mi].tool_calls[ti].output.is_none()
                            };
                            pending
                                .iter()
                                .copied()
                                .find(|s| {
                                    unfilled(s)
                                        && (name.is_empty()
                                            || messages[s.0].tool_calls[s.1].name == name)
                                })
                                .or_else(|| pending.iter().copied().find(unfilled))
                        });
                    if let Some((mi, ti)) = slot {
                        messages[mi].tool_calls[ti].output = Some(clip(&output, MAX_TOOL_IO).0);
                        pending.retain(|s| *s != (mi, ti));
                    }
                }
                "system" => {
                    let text = content_text(m.content.as_deref());
                    if !text.is_empty() {
                        let mut msg = text_msg(Role::System, &text, m.ts_ms);
                        msg.kind = MessageKind::Meta;
                        messages.push(msg);
                    }
                }
                _ => {}
            }
        }
        assign_seq(&mut messages);
        let meta = Self::build_meta(r, &row, &messages);
        Ok((meta, messages))
    }
}

struct RawMsg {
    id: i64,
    role: String,
    content: Option<String>,
    tool_call_id: Option<String>,
    tool_calls: Option<String>,
    tool_name: Option<String>,
    reasoning: Option<String>,
    ts_ms: i64,
}

#[derive(Clone)]
struct HermesRow {
    id: String,
    source: Option<String>,
    model: Option<String>,
    title: String,
    started_ms: i64,
    updated_ms: i64,
    tokens: i64,
    message_count: i64,
    content_len: i64,
    /// `/branch`、上下文压缩续段、子代理委派都会设它;这些仍是用户可见的
    /// 会话,照常列出,但按父子关系挂到父会话下(parent_links)
    parent_id: Option<String>,
}

/// Hermes 全部时间戳恒为 unix 秒(写端 time.time()),不做秒/毫秒猜测
fn secs_to_ms(secs: f64) -> i64 {
    ((secs * 1000.0).round() as i64).max(0)
}

/// 全表聚合行(list/quick/parse 共用,经 MtimeCache 一轮只跑一次)
fn query_rows(ro: &SqliteRo, db: &HermesDb) -> Option<Vec<HermesRow>> {
    let schema = *db.schema.get_or_init(|| HermesSchema::probe(&ro.conn));
    let col = |present: bool, name: &str| {
        if present {
            format!("COALESCE(s.{name},0)")
        } else {
            "0".to_string()
        }
    };
    let title_col = if schema.title {
        "COALESCE(s.title, '')"
    } else {
        "''"
    };
    let sql = format!(
        "SELECT s.id, s.source, s.model, {title_col}, s.started_at, s.ended_at,
                COALESCE(s.input_tokens,0) + COALESCE(s.output_tokens,0) + {} + {} + {},
                COUNT(m.id), COALESCE(MAX(m.timestamp), 0),
                COALESCE(SUM(LENGTH(COALESCE(m.content,'')) + LENGTH(COALESCE(m.tool_calls,''))), 0),
                s.parent_session_id
         FROM sessions s LEFT JOIN messages m ON m.session_id = s.id
         GROUP BY s.id",
        col(schema.cache_read, "cache_read_tokens"),
        col(schema.cache_write, "cache_write_tokens"),
        col(schema.reasoning_tokens, "reasoning_tokens"),
    );
    let mut stmt = ro.conn.prepare(&sql).ok()?;
    let rows = stmt
        .query_map([], |r| {
            let started = r.get::<_, Option<f64>>(4)?.unwrap_or(0.0);
            let ended = r.get::<_, Option<f64>>(5)?.unwrap_or(0.0);
            let last_msg = r.get::<_, Option<f64>>(8)?.unwrap_or(0.0);
            Ok(HermesRow {
                id: r.get(0)?,
                source: r.get(1)?,
                model: r.get(2)?,
                title: r.get::<_, Option<String>>(3)?.unwrap_or_default(),
                started_ms: secs_to_ms(started),
                updated_ms: secs_to_ms(ended.max(last_msg).max(started)),
                tokens: r.get::<_, Option<i64>>(6)?.unwrap_or(0),
                message_count: r.get(7)?,
                content_len: r.get(9)?,
                parent_id: r.get::<_, Option<String>>(10)?.filter(|p| !p.is_empty()),
            })
        })
        .ok()?
        .collect::<rusqlite::Result<Vec<_>>>()
        .ok();
    rows
}

/// content 列多为纯文本;多模态时写端存 JSON 块数组字符串,识别后走
/// content_parts(图片退占位符,Hermes 库里存的是路径不是数据)
fn content_text(content: Option<&str>) -> String {
    let trimmed = content.unwrap_or_default().trim();
    if let Some(parsed) = trimmed
        .starts_with(['[', '{'])
        .then(|| serde_json::from_str::<Value>(trimmed).ok())
        .flatten()
        .map(|v| content_parts(&v, false))
        .filter(|p| !p.text.is_empty())
    {
        return parsed.text;
    }
    trimmed.to_string()
}

/// tool_calls JSON → (是否自带 id, view)。两种写端形状都认;arguments 若是
/// JSON 字符串先解成对象,预览与详情才不是一坨转义
fn tool_calls_of(raw: Option<&str>, msg_id: i64) -> Vec<(bool, ToolCallView)> {
    let Some(Value::Array(items)) = raw.and_then(|r| serde_json::from_str::<Value>(r).ok()) else {
        return Vec::new();
    };
    items
        .iter()
        .enumerate()
        .map(|(i, item)| {
            let func = item.get("function").unwrap_or(item);
            let name = func.get("name").and_then(Value::as_str).unwrap_or_default();
            let args = func.get("arguments").cloned().unwrap_or(Value::Null);
            let input = args
                .as_str()
                .and_then(|s| serde_json::from_str::<Value>(s).ok())
                .unwrap_or(args);
            let (has_id, id) = match item
                .get("id")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                Some(s) => (true, s.to_string()),
                None => (false, format!("hermes-{msg_id}-{i}")),
            };
            (has_id, tool_call_view(id, name, &input, None, false))
        })
        .collect()
}

impl AgentAdapter for HermesAdapter {
    fn agent(&self) -> AgentId {
        AgentId::Hermes
    }

    fn list_session_files(&self) -> Result<Vec<SessionFileRef>> {
        let mut out = Vec::new();
        for db in &self.dbs {
            let Some(rows) = Self::rows(db) else { continue };
            out.extend(
                rows.into_iter()
                    // 空会话(刚起就退)与 session_search 工具内部的检索会话不列
                    .filter(|row| row.message_count > 0 && row.source.as_deref() != Some("tool"))
                    .map(|row| SessionFileRef {
                        agent: AgentId::Hermes,
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
        for db in &self.dbs {
            let Some(rows) = Self::rows(db) else { continue };
            let by_id: HashMap<&str, &HermesRow> =
                rows.iter().map(|r| (r.id.as_str(), r)).collect();
            for r in refs
                .iter()
                .filter(|r| Path::new(strip_virtual_path(&r.file_path)) == db.path)
            {
                if let Some(row) = by_id.get(r.native_id.as_str()) {
                    out.insert(r.file_path.clone(), Self::build_meta(r, row, &[]));
                }
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

    fn manages_parent_links(&self) -> bool {
        true
    }

    /// (child, parent) 全量快照:跨档案的父指针不成立(各库独立),只在库内配对
    fn parent_links(&self) -> Vec<(String, String)> {
        let mut links = Vec::new();
        for db in &self.dbs {
            let Some(rows) = Self::rows(db) else { continue };
            let ids: std::collections::HashSet<&str> = rows.iter().map(|r| r.id.as_str()).collect();
            links.extend(rows.iter().filter_map(|r| {
                let parent = r.parent_id.as_deref().filter(|p| ids.contains(p))?;
                Some((format!("hermes:{}", r.id), format!("hermes:{parent}")))
            }));
        }
        links
    }

    fn with_custom_root(&self, dir: PathBuf) -> Box<dyn AgentAdapter> {
        // 选中库文件本身、Hermes home(连同其 profiles/*)或单个档案目录都认;
        // 整形判据只看 dir 内结构,目录不存在时也落到 <dir>/state.db(远程挂载点契约)
        let paths = if dir.is_file() {
            vec![dir]
        } else {
            dbs_under(&dir)
        };
        Box::new(Self::with_paths(paths))
    }

    fn data_roots(&self) -> Vec<PathBuf> {
        self.dbs.iter().map(|db| db.path.clone()).collect()
    }

    fn supports_individual_root_removal(&self) -> bool {
        self.dbs.len() > 1
    }

    fn excluding_data_roots(&self, roots: &[PathBuf]) -> Option<Box<dyn AgentAdapter>> {
        let kept = self
            .dbs
            .iter()
            .filter(|db| !roots.contains(&db.path))
            .map(|db| db.path.clone())
            .collect();
        Some(Box::new(Self::with_paths(kept)))
    }
}
