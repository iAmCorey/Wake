use super::parse_utils::*;
use super::sqlite_ro::{open_sqlite_ro, virtual_path};
use super::{units_from_messages, AgentAdapter};
use crate::models::*;
use anyhow::{anyhow, Result};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Cursor IDE 内的 Chat/Composer 历史:`~/Library/Application Support/Cursor/
/// User/globalStorage/state.vscdb`(Linux/Windows 换用户数据根,见 default_db_path)。
/// **与 cursor.rs 是同一家的两个数据源**:CLI(`cursor-agent`)把完整对话写
/// `~/.cursor/.../agent-transcripts/*.jsonl`,IDE 面板只把回合标记
/// (`{"type":"turn_ended"}`)写进那里、正文全留在本库——所以 IDE 会话在只读
/// JSONL 的旧实现里是空壳。两源同 native_id 的重叠交 scanner 的副本裁决,
/// 本源以 `dedup_rank` 排在 CLI 源之后:转录带正文时 CLI 那份胜出,
/// 并可从本库补充项目路径;本源那份正文留作解析失败的回退;
/// 只有转录缺失或只剩空壳的会话才由本源胜出。不按 mtime 定胜负——两边写盘
/// 先后不固定,同一会话会在项目之间跳(2026-09-15 实测)。
///
/// 库结构(VS Code 的 KV 表):
/// - `cursorDiskKV`:`composerData:<composerId>` 是会话元数据 + 气泡顺序
///   (`fullConversationHeadersOnly[]` 的 `{bubbleId,type}`,type 1=user 2=assistant),
///   `bubbleId:<composerId>:<bubbleId>` 是单条消息正文(`text`/`thinking`/
///   `toolFormerData`)。正文**不内联**在 composerData 里,必须按气泡逐条取。
/// - `composerHeaders`:较新版本才有的索引表,只覆盖一部分会话,
///   `isSubagent`/`subagentInfo.parentComposerId` 给出子代理归属。老会话没有
///   这张表的行,所以枚举**以 composerData 为准**、headers 只作父子关系补充。
///
/// **两条已知限制**(2026-09-14 review 记录,都不是本文件能单独解决的):
/// 1. 不进远程同步。`remote::REMOTE_LAYOUTS` 每个 AgentId 一行,而
///    `adapters::remote::create_remote_adapters` 按 `find(agent == …)` 取模板
///    ——同 AgentId 的第二个源拿不到自己的行。更硬的一道是
///    `sync_paths` 的字符集契约(`remote.rs` 的 `sync_paths_are_shell_safe`:
///    路径不加引号直接进 `sh -c` 与 rsync 参数),而本库的路径含空格
///    (`Application Support`)。补齐远程支持要先给同步管线加引用机制,
///    属独立改动;在那之前远程主机只同步得到 Cursor 的 CLI 会话。
/// 2. 不进文件监听。`data_roots` 给的是库文件,默认 `watch_paths` 只留目录
///    (`adapters/mod.rs`),故 IDE 新会话要等下一轮扫描才出现——
///    与 copilot/antigravity/hermes 等 SQLite 型源同一行为。
pub struct CursorIdeAdapter {
    db: PathBuf,
    /// 枚举要对全部 composerData blob 跑 json_extract(2GB 级库约 5s),
    /// 一轮扫描里 list_session_files 与 quick_meta 各调一次——按库 mtime 缓存
    rows_cache: MtimeCache<Vec<IdeRow>>,
    /// 子代理归属快照(composerHeaders 有行的那部分),同样按库 mtime 失效
    links_cache: MtimeCache<Vec<(String, String)>>,
}

/// `globalStorage/state.vscdb` 的默认路径。VS Code 系三平台的用户数据根不同:
/// macOS 在 `~/Library/Application Support`,Windows 在 `%APPDATA%`,
/// Linux 在 `$XDG_CONFIG_HOME`(缺省 `~/.config`)。这里只做路径推导、
/// 不探测存在性——缺根由 list_session_files 降级为空(roster 契约)。
/// 三平台都从 `home_dir()` 派生而非 `dirs::config_dir()`:后者在 Windows 上
/// 走 SHGetKnownFolderPath,`WAKE_HOME` 改道对它无效(见 mod.rs 的 home_dir)
pub(super) fn default_db_path() -> PathBuf {
    let home = super::home_dir().unwrap_or_default();
    let base = if cfg!(target_os = "macos") {
        home.join("Library").join("Application Support")
    } else if cfg!(target_os = "windows") {
        home.join("AppData").join("Roaming")
    } else {
        super::env_dir("XDG_CONFIG_HOME").unwrap_or_else(|| home.join(".config"))
    };
    base.join("Cursor")
        .join("User")
        .join("globalStorage")
        .join(DB_NAME)
}

fn project_path_from_data(data: &Value) -> Option<&str> {
    [
        "/workspaceIdentifier/uri/fsPath",
        "/trackedGitRepos/0/repoPath",
    ]
    .iter()
    .find_map(|path| data.pointer(path)?.as_str().filter(|s| !s.is_empty()))
}

pub(super) fn project_path(db: &Path, id: &str) -> Option<String> {
    let ro = open_sqlite_ro(db, "cursor-project")?;
    let raw: String = ro
        .conn
        .query_row(
            "SELECT CAST(value AS TEXT) FROM cursorDiskKV WHERE key = ?1",
            [format!("{COMPOSER_PREFIX}{id}")],
            |r| r.get(0),
        )
        .ok()?;
    let data: Value = serde_json::from_str(&raw).ok()?;
    project_path_from_data(&data).map(str::to_string)
}

impl CursorIdeAdapter {
    pub fn new() -> Self {
        Self::at(default_db_path())
    }

    fn at(db: PathBuf) -> Self {
        Self {
            db,
            rows_cache: MtimeCache::new(),
            links_cache: MtimeCache::new(),
        }
    }

    /// 全部有正文的会话(`fullConversationHeadersOnly` 非空)。零消息的草稿
    /// composer 不进列表——与 copilot 的 `turn_count > 0` 同一口径。
    ///
    /// 时间戳三段回退:`lastUpdatedAt` 只有较新的记录才写,缺失时取末条气泡的
    /// ISO `createdAt`,再缺退 `createdAt`(该字段实测全覆盖)。
    /// 项目路径优先 `workspaceIdentifier.uri.fsPath`,回退首个 git 仓库路径。
    fn rows(&self) -> Option<Vec<IdeRow>> {
        let mtime = super::sqlite_ro::db_cache_stamp(&self.db);
        self.rows_cache.get_or_try_build(mtime, || {
            let ro = open_sqlite_ro(&self.db, "cursor-ide")?;
            let mut stmt = ro
                .conn
                .prepare(&format!(
                    "SELECT substr(k.key, {cut}),
                            COALESCE(json_extract(CAST(k.value AS TEXT), '$.name'), ''),
                            COALESCE(json_extract(CAST(k.value AS TEXT), '$.createdAt'), 0),
                            COALESCE(json_extract(CAST(k.value AS TEXT), '$.lastUpdatedAt'), 0),
                            COALESCE(json_extract(CAST(k.value AS TEXT), '$.fullConversationHeadersOnly[#-1].createdAt'), ''),
                            json_array_length(CAST(k.value AS TEXT), '$.fullConversationHeadersOnly'),
                            COALESCE(json_extract(CAST(k.value AS TEXT), '$.workspaceIdentifier.uri.fsPath'), ''),
                            COALESCE(json_extract(CAST(k.value AS TEXT), '$.trackedGitRepos[0].repoPath'), '')
                     FROM cursorDiskKV k
                     WHERE k.key LIKE '{COMPOSER_PREFIX}%'
                       AND json_array_length(CAST(k.value AS TEXT), '$.fullConversationHeadersOnly') > 0",
                    // SQLite substr 是 1-indexed:剥掉前缀 = 从其后一位起。
                    // 两处都由常量推导,前缀改了不会留下静默错位的魔数
                    cut = COMPOSER_PREFIX.len() + 1,
                ))
                .ok()?;
            let rows = stmt
                .query_map([], |r| {
                    let created: i64 = r.get(2)?;
                    let updated: i64 = r.get(3)?;
                    let last_bubble_ms = iso_ms(&r.get::<_, String>(4)?);
                    let fs_path: String = r.get(6)?;
                    let repo_path: String = r.get(7)?;
                    Ok(IdeRow {
                        id: r.get(0)?,
                        name: r.get(1)?,
                        created_ms: created,
                        updated_ms: pick_updated(updated, last_bubble_ms, created),
                        bubble_count: r.get(5)?,
                        cwd: if fs_path.is_empty() { repo_path } else { fs_path },
                    })
                })
                .ok()?
                .collect::<rusqlite::Result<Vec<_>>>()
                .ok()?;
            Some(rows)
        })
    }

    fn build_meta(&self, r: &SessionFileRef, row: &IdeRow, message_count: i64) -> SessionMeta {
        let title = clean_title_candidate(&row.name);
        SessionMeta {
            key: format!("cursor:{}", row.id),
            host: String::new(),
            id: row.id.clone(),
            agent: AgentId::Cursor,
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
            git_branch: None,
            model: None,
            tokens_used: None,
            archived: false,
            source: None,
            favorite: false,
            pinned: false,
        }
    }

    /// 单会话解析:一次连接,先读 composerData 拿气泡顺序,再按
    /// `bubbleId:<cid>:` 前缀范围扫出正文。**顺序只认
    /// `fullConversationHeadersOnly`**——KV 表按 key 字典序,而 bubbleId 是
    /// 随机 UUID,照 key 序读会把对话打乱。
    fn parse(&self, r: &SessionFileRef) -> Result<(SessionMeta, Vec<TranscriptMessage>)> {
        let ro = open_sqlite_ro(&self.db, "cursor-ide")
            .ok_or_else(|| anyhow!("cannot open cursor IDE store"))?;
        let raw: String = ro
            .conn
            .query_row(
                "SELECT CAST(value AS TEXT) FROM cursorDiskKV WHERE key = ?1",
                [format!("{COMPOSER_PREFIX}{}", r.native_id)],
                |x| x.get(0),
            )
            .map_err(|_| anyhow!("cursor composer {} not in store", r.native_id))?;
        let data: Value = serde_json::from_str(&raw)?;

        // 气泡正文单独存 KV,一次范围扫描全取回来(逐条点查是 N 次往返)。
        // 上界用同前缀接 U+FFFF:UUID 只含 [0-9a-f-],不会越界到别的会话
        let prefix = format!("{BUBBLE_PREFIX}{}:", r.native_id);
        let mut bubbles: HashMap<String, Value> = HashMap::new();
        {
            let mut stmt = ro.conn.prepare(
                "SELECT substr(key, ?2), CAST(value AS TEXT) FROM cursorDiskKV
                 WHERE key >= ?1 AND key < ?3",
            )?;
            let upper = format!("{prefix}\u{FFFF}");
            let cut = prefix.len() as i64 + 1;
            let mut found = stmt.query(rusqlite::params![&prefix, cut, &upper])?;
            while let Some(row) = found.next()? {
                let id: String = row.get(0)?;
                // Cursor 清理过的气泡会留下 value 为 NULL 的行(本机 2.5 GB 库里
                // 983 行、波及 107 个会话),按"已被清理"跳过——当成错误会让
                // 整个会话解析失败
                let Some(body) = row.get::<_, Option<String>>(1)? else {
                    continue;
                };
                if let Ok(v) = serde_json::from_str::<Value>(&body) {
                    bubbles.insert(id, v);
                }
            }
        }

        let order = data
            .get("fullConversationHeadersOnly")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let mut messages: Vec<TranscriptMessage> = Vec::new();
        for head in &order {
            let Some(bid) = head.get("bubbleId").and_then(|v| v.as_str()) else {
                continue;
            };
            // 顺序表里有、KV 里没有:气泡行已被 Cursor 清理(老会话常见)
            let Some(bubble) = bubbles.get(bid) else {
                continue;
            };
            // 角色以顺序表为准、气泡自带的 type 兜底(两处写同一枚举)
            let kind = head
                .get("type")
                .and_then(|v| v.as_i64())
                .or_else(|| bubble.get("type").and_then(|v| v.as_i64()))
                .unwrap_or(BUBBLE_ASSISTANT);
            if let Some(msg) = bubble_message(bubble, kind) {
                messages.push(msg);
            }
        }
        assign_seq(&mut messages);

        let count = messages
            .iter()
            .filter(|m| m.kind == MessageKind::Text)
            .count() as i64;
        // 枚举快照优先(与列表页同源);库在扫描与打开详情之间变过时就地重建
        let row = self
            .rows()
            .and_then(|rows| rows.into_iter().find(|x| x.id == r.native_id))
            .unwrap_or_else(|| IdeRow::from_data(&r.native_id, &data, order.len() as i64));
        let mut meta = self.build_meta(r, &row, count);
        // composer 没起名时(新建会话、或 Cursor 还没生成摘要)回退首条用户消息
        if meta.title == UNTITLED {
            if let Some(t) = title_from_messages(&messages) {
                meta.title = t;
            }
        }
        Ok((meta, messages))
    }
}

const DB_NAME: &str = "state.vscdb";
/// `cursorDiskKV` 里会话元数据行的 key 前缀。**长度参与 SQL 的 substr 偏移**
/// (见 `rows`),所以只能有这一处字面量——写死的偏移量与前缀改动脱节时,
/// 剥出来的 id 会静默少几个字符
const COMPOSER_PREFIX: &str = "composerData:";
/// 单条消息正文行的 key 前缀,完整形态 `bubbleId:<composerId>:<bubbleId>`
const BUBBLE_PREFIX: &str = "bubbleId:";
/// `fullConversationHeadersOnly[].type` 与气泡自带 `type` 的取值
const BUBBLE_USER: i64 = 1;
const BUBBLE_ASSISTANT: i64 = 2;

/// `composerHeaders.isSubagent=1` 的行带 `subagentInfo.parentComposerId`。
/// 这张表只覆盖较新的会话,老会话查不到父子关系——不是错误,照常列为顶层。
fn parent_links_from(db: &std::path::Path) -> Option<Vec<(String, String)>> {
    let ro = open_sqlite_ro(db, "cursor-ide")?;
    let mut stmt = ro
        .conn
        .prepare(
            "SELECT composerId, json_extract(value, '$.subagentInfo.parentComposerId')
             FROM composerHeaders WHERE isSubagent = 1",
        )
        .ok()?;
    let rows = stmt
        .query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
        })
        .ok()?
        .collect::<rusqlite::Result<Vec<_>>>()
        .ok()?;
    Some(
        rows.into_iter()
            .filter_map(|(child, parent)| {
                let parent = parent?;
                // 自指会在关系图里成环,空父等于没有关系
                if parent.is_empty() || parent == child {
                    return None;
                }
                Some((format!("cursor:{child}"), format!("cursor:{parent}")))
            })
            .collect(),
    )
}

/// updated_at 的三段回退,见 `rows` 的说明
fn pick_updated(last_updated: i64, last_bubble: i64, created: i64) -> i64 {
    if last_updated > 0 {
        last_updated
    } else if last_bubble > 0 {
        last_bubble
    } else {
        created
    }
}

/// 一条气泡 → 一条消息。空壳气泡(无正文、无思考、无工具)返回 None:
/// Cursor 每个流式分片都落一条气泡,绝大多数是没有 text 的中间态。
fn bubble_message(bubble: &Value, kind: i64) -> Option<TranscriptMessage> {
    let role = if kind == BUBBLE_USER {
        Role::User
    } else {
        Role::Assistant
    };
    let ts = bubble
        .get("createdAt")
        .and_then(|v| v.as_str())
        .map(iso_ms)
        .unwrap_or(0);
    let text = bubble
        .get("text")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let thinking = bubble
        .get("thinking")
        .and_then(|t| t.get("text"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .map(|s| clip(s, MAX_MSG_TEXT).0);
    let tool = bubble.get("toolFormerData").and_then(tool_from);

    if text.trim().is_empty() && thinking.is_none() && tool.is_none() {
        return None;
    }
    let mut msg = text_msg(role, text, ts);
    msg.thinking = thinking;
    msg.tool_calls = tool.into_iter().collect();
    Some(msg)
}

/// 取一个"可能被双重编码"的字段:Cursor 对同一字段有时写 JSON 字符串
///(`"{\"path\":…}"`)、有时直接写对象。字符串能再解析就解析,解析不动
/// 就当纯文本保留(命令行、自由文本参数都是这一类)。空值一律 None。
fn json_field(data: &Value, key: &str) -> Option<Value> {
    match data.get(key)? {
        Value::String(s) if s.trim().is_empty() => None,
        Value::String(s) => {
            Some(serde_json::from_str(s).unwrap_or_else(|_| Value::String(s.clone())))
        }
        Value::Null => None,
        other => Some(other.clone()),
    }
}

/// `toolFormerData` → ToolCallView。
///
/// 参数有两个来源,**必须都读**:`rawArgs` 是模型原样吐出的 JSON 串,
/// `params` 是 Cursor 归一化后的形态。实测约 11% 的调用只有后者——
/// 且恰好是最该被搜到的那些(run_terminal_command_v2 的命令行、
/// edit_file_v2 的目标文件、task_v2 的子任务描述),只读 rawArgs 会让
/// 这些工具在索引里变成没有输入的空调用。
/// `result` 同理:多数工具写 JSON 串,终端类写 `{"output":…}` 对象。
fn tool_from(data: &Value) -> Option<ToolCallView> {
    let name = data.get("name").and_then(|v| v.as_str()).unwrap_or("tool");
    let id = data
        .get("toolCallId")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let input = json_field(data, "rawArgs")
        .or_else(|| json_field(data, "params"))
        .unwrap_or(Value::Null);
    let output = json_field(data, "result").map(|v| match v {
        // 终端类工具把正文包在 output 里,直接展平成人读的文本;
        // 其余形态保持 JSON(工具结果的结构本身就是信息)
        Value::Object(ref o) => o
            .get("output")
            .and_then(|x| x.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| v.to_string()),
        Value::String(s) => s,
        other => other.to_string(),
    });
    let is_error = data
        .get("status")
        .and_then(|v| v.as_str())
        .is_some_and(|s| s == "error");
    Some(tool_call_view(
        id.to_string(),
        name,
        &input,
        output,
        is_error,
    ))
}

#[derive(Clone)]
struct IdeRow {
    id: String,
    name: String,
    created_ms: i64,
    updated_ms: i64,
    bubble_count: i64,
    cwd: String,
}

impl IdeRow {
    /// 枚举快照里没有这条时的兜底(库在扫描与打开详情之间变过)
    fn from_data(id: &str, data: &Value, bubble_count: i64) -> Self {
        let created = data.get("createdAt").and_then(|v| v.as_i64()).unwrap_or(0);
        let updated = data
            .get("lastUpdatedAt")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        let cwd = project_path_from_data(data).unwrap_or_default();
        Self {
            id: id.to_string(),
            name: data
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
            created_ms: created,
            updated_ms: pick_updated(updated, 0, created),
            bubble_count,
            cwd: cwd.to_string(),
        }
    }
}

impl AgentAdapter for CursorIdeAdapter {
    fn agent(&self) -> AgentId {
        AgentId::Cursor
    }

    /// 与 cursor.rs 同 key 的副本:CLI 源先、本源后(见文件头)
    fn dedup_rank(&self) -> u8 {
        1
    }

    fn list_session_files(&self) -> Result<Vec<SessionFileRef>> {
        let Some(rows) = self.rows() else {
            return Ok(Vec::new());
        };
        Ok(rows
            .into_iter()
            .map(|row| SessionFileRef {
                agent: AgentId::Cursor,
                native_id: row.id.clone(),
                file_path: virtual_path(&self.db, &row.id),
                mtime_ms: row.updated_ms,
                // 正文在别的 KV 行里,枚举时不展开;气泡条数即内容指纹
                //(会话续跑必然追加气泡),与 mtime 共同构成 dirty 判断
                size: row.bubble_count,
            })
            .collect())
    }

    fn quick_meta(&self, refs: &[SessionFileRef]) -> Option<HashMap<String, SessionMeta>> {
        let rows = self.rows()?;
        let by_id: HashMap<&str, &IdeRow> = rows.iter().map(|r| (r.id.as_str(), r)).collect();
        let mut out = HashMap::new();
        for r in refs {
            if let Some(row) = by_id.get(r.native_id.as_str()) {
                out.insert(
                    r.file_path.clone(),
                    self.build_meta(r, row, row.bubble_count),
                );
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

    fn parent_links(&self) -> Vec<(String, String)> {
        let mtime = super::sqlite_ro::db_cache_stamp(&self.db);
        self.links_cache
            .get_or_try_build(mtime, || parent_links_from(&self.db))
            .unwrap_or_default()
    }

    fn with_custom_root(&self, dir: PathBuf) -> Box<dyn AgentAdapter> {
        // 直接给到库文件、给 globalStorage 目录、或给 Cursor 用户数据根
        //(含 User/globalStorage/)都认——与 copilot/antigravity 同一判序
        let db = if dir.is_file() {
            dir
        } else {
            let nested = dir.join("User").join("globalStorage").join(DB_NAME);
            if nested.is_file() {
                nested
            } else {
                dir.join(DB_NAME)
            }
        };
        Box::new(Self::at(db))
    }

    fn data_roots(&self) -> Vec<PathBuf> {
        // 空 db 路径 = 本条 location 已被用户移除(见 excluding_data_roots),
        // 此时如实报告"没有数据根",roster 组装处据此整体丢弃本实例
        if self.db.as_os_str().is_empty() {
            Vec::new()
        } else {
            vec![self.db.clone()]
        }
    }

    /// Cursor 一家两源(本实例的 state.vscdb 与 cursor.rs 的 agent-transcripts),
    /// 彼此独立。不开这个能力位的话,面板上的 Remove 会走 `removed_defaults`
    /// 按 agent 整家压制——移除其中一条 location,另一条跟着消失
    ///(同 OpenCode 的 stable/next 两库)
    fn supports_individual_root_removal(&self) -> bool {
        true
    }

    /// 单根实例:被排除的是自己的根就交出空根实例(roster 丢弃它);
    /// 排除列表按 agent 收集,里面可能是**另一个源**的根,那种情况返回
    /// None 表示"与我无关,原样保留"
    fn excluding_data_roots(&self, roots: &[PathBuf]) -> Option<Box<dyn AgentAdapter>> {
        if roots.contains(&self.db) {
            Some(Box::new(Self::at(PathBuf::new())))
        } else {
            None
        }
    }
}
