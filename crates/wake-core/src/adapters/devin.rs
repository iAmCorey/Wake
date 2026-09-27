//! Devin(Cognition 的 agent,CLI 与 Devin Desktop 共用本地存储):
//! `<数据根>/cli/sessions.db` 全明文 SQLite。数据根服从 XDG——
//! `$XDG_DATA_HOME/devin`,缺省 `~/.local/share/devin`(macOS 同样走 XDG
//! 形态,实测);`~/Library/Application Support/devin` 是桌面 Electron 壳的
//! profile 目录,作为历史候选一并探;Windows 的 Devin Desktop 把库放在
//! `%APPDATA%\devin`(从 `home_dir()` 派生,与 cursor_ide 同一约定:不走
//! `dirs::config_dir()`,否则 `WAKE_HOME` 改道对它无效),同样探。候选按序取
//! 第一个真有库文件的,都没有时回落 XDG 语义下的默认位置(env 候选里真有库
//! 才采信,存在但空的目录不能遮掉默认根——与其他 adapter 同一约定)。
//!
//! `sessions(id,title,working_directory,model,agent_mode,created_at,
//! last_activity_at,hidden,main_chain_id,…)` + `message_nodes(row_id,
//! session_id,node_id,parent_node_id,chat_message,created_at)`——每个会话
//! 一座森林:重试/编辑从中间节点开出侧枝,可见转录是从
//! `sessions.main_chain_id` 这片叶子沿 `parent_node_id` 走回根的那一条链;
//! 叶子缺列(老库)、悬空或走不出根(成环)时退回 `created_at,row_id` 全列。
//! 时间戳全是 unix **秒**(INTEGER)。`hidden = 1` 是 Devin 自己不列的会话
//! (compaction 辅助会话、已删会话),不列。空会话(零正文)不列。
//!
//! `chat_message` 是 JSON:`role` ∈ user/assistant/system/tool,`content`
//! 恒为字符串;assistant 的 `tool_calls` 是 `[{id,name,arguments(对象)}]`,
//! `role="tool"` 行按 `tool_call_id` 回填 output;`thinking` 是对象
//! `{thinking,signature,signature_type}`——signature 是 sealed 二进制签名,
//! 只取 thinking 文本;`metadata` 携带 `is_user_input`(真人输入= true)、
//! `telemetry.source`(`cache_keepalive` 等内部来源)、`generation_model`
//! (逐请求的实发模型,比 sessions.model 权威)与 `metrics`
//! (`input_tokens`/`output_tokens`/`cache_read_tokens`/
//! `cache_creation_tokens`,按主链累计进 tokens_used)。
//!
//! 内部消息归 Meta:`system` 角色(注入上下文)、`telemetry.source` 非
//! `user` 的用户消息(cache_keepalive 心跳)、`is_user_input == false`、
//! 以及 compaction 请求正文("Conversation to summarize:" /
//! "Now summarize the conversation above");assistant 的 `<summary>` 应答
//! 折成 CompactSummary。字段缺席按真人放行(老写端未必有 is_user_input)。
//!
//! resume:`devin --resume <id>`(会话按 cwd 分桶,`devin list` 只列当前
//! 目录,故 resume 在项目目录里跑)。无每会话文件,SessionFileRef 用虚拟
//! 路径 `<db>#<id>`。
use super::parse_utils::*;
use super::sqlite_ro::{
    db_cache_stamp, open_sqlite_ro, strip_virtual_path, table_columns, virtual_path, SqliteRo,
};
use super::AgentAdapter;
use crate::models::*;
use anyhow::{anyhow, Result};
use rusqlite::Connection;
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// 会话与正文的唯一来源,相对数据根
const DB_REL: &str = "cli/sessions.db";

/// 数据根下的库文件,按段拼:`join(DB_REL)` 在 Windows 上会留下正斜杠,虚拟路径
/// 与别处按段拼出的同一个库字符串对不上
fn db_in(root: &Path) -> PathBuf {
    root.join(DB_REL.split('/').collect::<PathBuf>())
}

pub struct DevinAdapter {
    db: PathBuf,
    /// 枚举查询带全表相关子查询,按库 mtime 缓存一轮扫描内的重复调用
    rows_cache: MtimeCache<Vec<DevinRow>>,
}

#[derive(Clone)]
struct DevinRow {
    id: String,
    title: String,
    cwd: String,
    model: String,
    created_ms: i64,
    updated_ms: i64,
    /// 主链叶子;老库没有 main_chain_id 列(见模块注释的兜底)
    main_chain_id: Option<i64>,
    /// chat_message 总长——空会话过滤与 SessionFileRef.size 的脏判据(单会话取行时不算)
    content_len: i64,
}

/// ALTER 追加的列各自在不在(初始 schema 的列不用探)
#[derive(Clone, Copy)]
struct DevinSchema {
    hidden: bool,
    main_chain_id: bool,
}

impl DevinSchema {
    fn probe(conn: &Connection) -> Self {
        let sessions = table_columns(conn, "sessions");
        Self {
            hidden: sessions.contains("hidden"),
            main_chain_id: sessions.contains("main_chain_id"),
        }
    }
}

struct DevinNode {
    node_id: i64,
    parent_node_id: Option<i64>,
    chat_message: String,
    created_ms: i64,
}

impl DevinAdapter {
    pub fn new() -> Self {
        let home = super::home_dir().unwrap_or_default();
        let local = home.join(".local").join("share").join("devin");
        let xdg = super::env_dir("XDG_DATA_HOME").map(|d| d.join("devin"));
        let app_support = home
            .join("Library")
            .join("Application Support")
            .join("devin");
        // Windows 的 Devin Desktop: %APPDATA%\devin。Mac/Linux 上该目录不会
        // 存在,无条件作候选无害(与 app_support 同理),也让三平台的契约测试
        // 都能覆盖这条解析
        let appdata = home.join("AppData").join("Roaming").join("devin");
        let hit = xdg
            .iter()
            .chain([&local, &app_support, &appdata])
            .find(|dir| db_in(dir).is_file())
            .cloned();
        let root = hit.unwrap_or_else(|| xdg.unwrap_or(local));
        Self::with_db(db_in(&root))
    }

    fn with_db(db: PathBuf) -> Self {
        Self {
            db,
            rows_cache: MtimeCache::new(),
        }
    }

    fn open(&self) -> Option<SqliteRo> {
        open_sqlite_ro(&self.db, "devin")
    }

    /// 行清单,按库(含 -wal)mtime 缓存;库这一刻读不出交回**上一次读到的**
    /// (空清单会让 scanner 把整家会话当"磁盘已删"清掉),从没读成功过才是 None
    fn rows(&self) -> Option<Vec<DevinRow>> {
        let stamp = db_cache_stamp(&self.db);
        self.rows_cache.get_or_stale(stamp, || {
            let ro = self.open()?;
            query_rows(&ro.conn, DevinSchema::probe(&ro.conn), None).ok()
        })
    }

    fn parse(&self, r: &SessionFileRef) -> Result<(SessionMeta, Decoded)> {
        if Path::new(strip_virtual_path(&r.file_path)) != self.db.as_path() {
            return Err(anyhow!("devin database is outside adapter roots"));
        }
        let ro = self.open().ok_or_else(|| anyhow!("cannot open devin db"))?;
        let schema = DevinSchema::probe(&ro.conn);
        let row = query_rows(&ro.conn, schema, Some(&r.native_id))?
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("devin session {} not in db", r.native_id))?;
        let nodes = query_nodes(&ro.conn, &r.native_id)?;
        let chain = main_chain(nodes, row.main_chain_id);
        let decoded = decode_nodes(chain, &row.model);
        let meta = build_meta(r, &row, &decoded.messages, decoded.tokens);
        Ok((meta, decoded))
    }
}

fn build_meta(
    r: &SessionFileRef,
    row: &DevinRow,
    messages: &[TranscriptMessage],
    tokens: i64,
) -> SessionMeta {
    let title = Some(row.title.as_str())
        .map(clean_title_candidate)
        .filter(|t| !t.is_empty())
        .or_else(|| title_from_messages(messages))
        .unwrap_or_else(|| UNTITLED.to_string());
    SessionMeta {
        key: format!("devin:{}", row.id),
        host: String::new(),
        id: row.id.clone(),
        agent: AgentId::Devin,
        title,
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
        message_count: messages
            .iter()
            .filter(|m| m.kind == MessageKind::Text)
            .count() as i64,
        size_bytes: r.size,
        git_branch: None,
        // 同一会话会换模型,取最后一条 assistant 的实发模型;没有消息级记录用会话行
        model: messages
            .iter()
            .rev()
            .find_map(|m| m.model.clone())
            .or_else(|| Some(row.model.clone()).filter(|m| !m.is_empty())),
        tokens_used: (tokens > 0).then_some(tokens),
        archived: false,
        source: None,
        favorite: false,
        pinned: false,
    }
}

/// unix 秒 → epoch ms;非正数与溢出都给 0(调用方按"没有时间"处理)
fn secs_to_ms(secs: i64) -> i64 {
    secs.checked_mul(1000).unwrap_or(0).max(0)
}

/// 枚举 / 单会话取行。缺列给默认值:老库不得整家消失。单会话取行时不算
/// chat_message 总长——它只给枚举做空会话过滤与脏判据
fn query_rows(
    conn: &Connection,
    schema: DevinSchema,
    id: Option<&str>,
) -> rusqlite::Result<Vec<DevinRow>> {
    let visible = if schema.hidden {
        "COALESCE(s.hidden, 0) <> 1"
    } else {
        "1"
    };
    let leaf = if schema.main_chain_id {
        "s.main_chain_id"
    } else {
        "NULL"
    };
    let content_len = if id.is_some() {
        "0"
    } else {
        "(SELECT COALESCE(SUM(LENGTH(m.chat_message)), 0) FROM message_nodes m
          WHERE m.session_id = s.id)"
    };
    let sql = format!(
        "SELECT s.id, COALESCE(s.title, ''), COALESCE(s.working_directory, ''),
                COALESCE(s.model, ''), COALESCE(s.created_at, 0),
                COALESCE(s.last_activity_at, s.created_at, 0), {leaf}, {content_len}
         FROM sessions s WHERE {visible} AND (?1 IS NULL OR s.id = ?1)"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([id], |r| {
        Ok(DevinRow {
            id: r.get(0)?,
            title: r.get(1)?,
            cwd: r.get(2)?,
            model: r.get(3)?,
            created_ms: secs_to_ms(r.get::<_, i64>(4)?),
            updated_ms: secs_to_ms(r.get::<_, i64>(5)?),
            main_chain_id: r.get(6)?,
            content_len: r.get::<_, Option<i64>>(7)?.unwrap_or(0),
        })
    })?;
    rows.collect()
}

fn query_nodes(conn: &Connection, session_id: &str) -> rusqlite::Result<Vec<DevinNode>> {
    let mut stmt = conn.prepare(
        "SELECT node_id, parent_node_id, chat_message, created_at
         FROM message_nodes WHERE session_id = ?1 ORDER BY created_at, row_id",
    )?;
    let rows = stmt.query_map([session_id], |r| {
        Ok(DevinNode {
            node_id: r.get(0)?,
            parent_node_id: r.get(1)?,
            chat_message: r.get(2)?,
            created_ms: secs_to_ms(r.get::<_, i64>(3)?),
        })
    })?;
    rows.collect()
}

/// 可见链:从 main_chain_id 叶子沿 parent_node_id 走回根再反转。走不完
/// (叶子悬空、父节点缺失、成环)一律退回原始顺序全列——宁可多列侧枝
/// 也不让会话正文消失
fn main_chain(rows: Vec<DevinNode>, leaf: Option<i64>) -> Vec<DevinNode> {
    let Some(leaf) = leaf else { return rows };
    let by_id: HashMap<i64, usize> = rows
        .iter()
        .enumerate()
        .map(|(i, n)| (n.node_id, i))
        .collect();
    let Some(&start) = by_id.get(&leaf) else {
        return rows;
    };
    // 先按下标走完整条链,确认走得到根再搬节点
    let mut chain = vec![start];
    let mut cur = start;
    while let Some(parent) = rows[cur].parent_node_id {
        match by_id.get(&parent) {
            // 链不可能比全部节点还长,再长就是成环
            Some(&ix) if chain.len() < rows.len() => {
                chain.push(ix);
                cur = ix;
            }
            _ => return rows,
        }
    }
    let mut slots: Vec<Option<DevinNode>> = rows.into_iter().map(Some).collect();
    chain
        .iter()
        .rev()
        .filter_map(|&ix| slots[ix].take())
        .collect()
}

/// compaction 请求的正文特征(Devin 把摘要写成隐藏辅助会话,也内联进主链)
fn is_compact_request(text: &str) -> bool {
    let t = text.trim_start();
    t.starts_with("Conversation to summarize:") || t.starts_with("Now summarize the conversation")
}

/// 一条 chat_message 是否内部来源:system 角色之外,`telemetry.source` 非
/// `user`(cache_keepalive 心跳)、`is_user_input` 显式 false、compaction
/// 请求正文都归 Meta;字段缺席按真人放行
fn is_internal_user(v: &Value, text: &str) -> bool {
    let meta = v.get("metadata");
    let source = meta
        .and_then(|m| m.pointer("/telemetry/source"))
        .and_then(Value::as_str);
    if source.is_some_and(|s| s != "user") {
        return true;
    }
    if meta
        .and_then(|m| m.get("is_user_input"))
        .and_then(Value::as_bool)
        == Some(false)
    {
        return true;
    }
    is_compact_request(text)
}

/// thinking 字段两种形态:对象 `{thinking,signature,signature_type}`(签名是
/// sealed 二进制,不进索引)或老写端的纯字符串
fn thinking_text(v: &Value) -> Option<String> {
    let t = v.get("thinking")?;
    t.get("thinking")
        .and_then(Value::as_str)
        .or_else(|| t.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
}

fn metric_i64(metrics: &Value, key: &str) -> i64 {
    metrics.get(key).and_then(Value::as_i64).unwrap_or(0)
}

struct Decoded {
    messages: Vec<TranscriptMessage>,
    /// 在消息取舍之外单独累计:content 为空但消耗了 token 的 assistant 收尾节点照样入账
    tokens: i64,
    /// 解不开的 chat_message 与词汇表外的 role:格式漂移的金丝雀
    unknown: u32,
}

/// 主链逐节点解码
fn decode_nodes(chain: Vec<DevinNode>, session_model: &str) -> Decoded {
    let mut messages: Vec<TranscriptMessage> = Vec::new();
    let mut tokens = 0i64;
    let mut unknown = 0u32;
    // tool_call_id → (消息下标, tool_calls 下标)
    let mut by_id: HashMap<String, (usize, usize)> = HashMap::new();
    for node in chain {
        let Ok(v) = serde_json::from_str::<Value>(&node.chat_message) else {
            unknown += 1;
            continue;
        };
        if let Some(metrics) = v.pointer("/metadata/metrics") {
            tokens += metric_i64(metrics, "input_tokens")
                + metric_i64(metrics, "output_tokens")
                + metric_i64(metrics, "cache_read_tokens")
                + metric_i64(metrics, "cache_creation_tokens");
        }
        match v.get("role").and_then(Value::as_str) {
            Some("user") => {
                let text = v.get("content").and_then(Value::as_str).unwrap_or_default();
                if text.trim().is_empty() {
                    continue;
                }
                let mut msg = text_msg(Role::User, text, node.created_ms);
                if is_internal_user(&v, text) {
                    msg.kind = MessageKind::Meta;
                }
                messages.push(msg);
            }
            Some("assistant") => {
                let text = v.get("content").and_then(Value::as_str).unwrap_or_default();
                let thinking = thinking_text(&v);
                let calls: Vec<ToolCallView> = v
                    .get("tool_calls")
                    .and_then(Value::as_array)
                    .map(|items| {
                        items
                            .iter()
                            .map(|item| {
                                let id = item
                                    .get("id")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default()
                                    .to_string();
                                let name =
                                    item.get("name").and_then(Value::as_str).unwrap_or_default();
                                let args = item.get("arguments").cloned().unwrap_or(Value::Null);
                                tool_call_view(id, name, &args, None, false)
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                if text.trim().is_empty() && calls.is_empty() && thinking.is_none() {
                    continue;
                }
                let mut msg = text_msg(Role::Assistant, text, node.created_ms);
                if text.trim_start().starts_with("<summary>") {
                    msg.kind = MessageKind::CompactSummary;
                }
                msg.thinking = thinking.map(|t| clip(&t, MAX_TOOL_IO).0);
                msg.model = v
                    .pointer("/metadata/generation_model")
                    .and_then(Value::as_str)
                    .map(String::from)
                    .or_else(|| Some(session_model.to_string()).filter(|m| !m.is_empty()));
                let base = messages.len();
                for tc in calls {
                    by_id.insert(tc.id.clone(), (base, msg.tool_calls.len()));
                    msg.tool_calls.push(tc);
                }
                messages.push(msg);
            }
            Some("tool") => {
                let output = v.get("content").and_then(Value::as_str).unwrap_or_default();
                let slot = v
                    .get("tool_call_id")
                    .and_then(Value::as_str)
                    .and_then(|id| by_id.get(id).copied());
                if let Some((mi, ti)) = slot {
                    messages[mi].tool_calls[ti].output = Some(clip(output, MAX_TOOL_IO).0);
                }
            }
            Some("system") => {
                let text = v.get("content").and_then(Value::as_str).unwrap_or_default();
                if text.trim().is_empty() {
                    continue;
                }
                let mut msg = text_msg(Role::System, text, node.created_ms);
                msg.kind = MessageKind::Meta;
                messages.push(msg);
            }
            _ => unknown += 1,
        }
    }
    assign_seq(&mut messages);
    Decoded {
        messages,
        tokens,
        unknown,
    }
}

impl AgentAdapter for DevinAdapter {
    fn agent(&self) -> AgentId {
        AgentId::Devin
    }

    fn list_session_files(&self) -> Result<Vec<SessionFileRef>> {
        // 库不在 = 没有会话(Ok 空);库在但从没读成功过 = 不知道,报 Err 让这轮
        // 扫描停下——折成空会让 seen_paths 清理把整家会话当"磁盘已删"删掉
        let rows = match self.rows() {
            Some(rows) => rows,
            None if self.db.is_file() => {
                anyhow::bail!("{} exists but could not be read", self.db.display())
            }
            None => Vec::new(),
        };
        Ok(rows
            .into_iter()
            .filter(|row| row.content_len > 0)
            .map(|row| SessionFileRef {
                agent: AgentId::Devin,
                file_path: virtual_path(&self.db, &row.id),
                native_id: row.id,
                mtime_ms: row.updated_ms,
                size: row.content_len,
            })
            .collect())
    }

    fn parse_session(&self, r: &SessionFileRef) -> Result<ParsedSession> {
        let (meta, decoded) = self.parse(r)?;
        Ok(ParsedSession::derive(
            meta,
            &decoded.messages,
            decoded.unknown,
        ))
    }

    fn parse_transcript(&self, r: &SessionFileRef) -> Result<ParsedTranscript> {
        let (meta, decoded) = self.parse(r)?;
        Ok(ParsedTranscript {
            meta,
            mainline: decoded.messages,
            sidechains: Vec::new(),
            unknown_line_count: decoded.unknown,
        })
    }

    fn data_roots(&self) -> Vec<PathBuf> {
        vec![self.db.clone()]
    }

    fn with_custom_root(&self, dir: PathBuf) -> Box<dyn AgentAdapter> {
        // 只按路径形状整形、不看存不存在:远程缓存首次同步前目录还没落盘,
        // 判据随目录出现而改判会让先构造的实例指错树。认两种:数据根
        // (<dir>/cli/sessions.db),或直接给到 sessions.db 的孤立库拷贝;
        // 中间层 cli/ 由 normalize_custom_root 在入库前上提到数据根
        let db = if dir.file_name().is_some_and(|n| n == "sessions.db") {
            dir
        } else {
            db_in(&dir)
        };
        Box::new(Self::with_db(db))
    }
}

/// 入库前的归一化(mod.rs 静态分派):用户在目录选择器里选中 `cli/sessions.db`
/// 或 `cli` 层时上提到数据根。判据是纯路径形状——选中的路径以这几层收尾就按
/// 层数上提——不摸文件系统;孤立的库拷贝(父链不长这个样)原样保留
pub fn normalize_custom_root(dir: PathBuf) -> PathBuf {
    for rel in [DB_REL, "cli"] {
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
