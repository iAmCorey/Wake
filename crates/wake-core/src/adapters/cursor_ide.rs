use super::parse_utils::*;
use super::sqlite_ro::{open_sqlite_ro, virtual_path};
use super::AgentAdapter;
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
/// 并可从本库补充项目路径与模型 / token;本源那份正文留作解析失败的回退;
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

pub(super) struct ProjectMetadata {
    pub workspace: Option<String>,
    pub repositories: Vec<String>,
}

impl ProjectMetadata {
    /// composer 记的工作区与 Git 仓库;两样都没有给 None
    fn from_data(data: &Value) -> Option<Self> {
        let workspace = data
            .pointer("/workspaceIdentifier/uri/fsPath")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let repositories = data
            .get("trackedGitRepos")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|repo| repo.get("repoPath")?.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect::<Vec<_>>();
        (workspace.is_some() || !repositories.is_empty()).then_some(Self {
            workspace,
            repositories,
        })
    }
}

/// 转录那一路向本库借的东西(转录不记 cwd、模型与 token)。**只读 composer 那一行**:气泡动辄
/// 几十上百 MB(本机最大一条 15k 条气泡、98 MB),而转录胜出的都是新版 Cursor 的会话,气泡里
/// 本来就没有逐条模型与 token——逐条那一级只在 `CursorIdeAdapter::parse` 里用,那边气泡已经读进来了
#[derive(Default)]
pub(super) struct ComposerFacts {
    /// Keep workspace and repository paths distinct: a repo may contain the
    /// actual workspace, or be one of several repos inside it.
    pub project: Option<ProjectMetadata>,
    pub model: Option<String>,
}

/// 批量读,一次连接、一条缓存的语句(转录解析读一个,每轮扫描的侧档刷新读全部)。库打不开
/// 给 None;库里没有的 id(纯 CLI 会话)不出现在结果里
pub(super) fn composer_facts<'a>(
    db: &Path,
    ids: impl Iterator<Item = &'a str>,
) -> Option<HashMap<String, ComposerFacts>> {
    let ro = open_sqlite_ro(db, "cursor-project")?;
    Some(
        ids.filter_map(|id| {
            let data = read_composer(&ro.conn, id)?;
            let facts = ComposerFacts {
                project: ProjectMetadata::from_data(&data),
                model: composer_model(&data),
            };
            Some((id.to_string(), facts))
        })
        .collect(),
    )
}

/// IDE 会话用过的模型与 token——Cursor 在本地记下的那部分
#[derive(Default)]
struct Usage {
    model: Option<String>,
    tokens: Option<i64>,
}

/// 模型:对话里最后一条带 `modelInfo.modelName` 的气泡(逐次请求实际用的模型,2025-10 ~
/// 2026-01 的版本写在用户气泡上)> `composer_model`。token:各气泡 `tokenCount` 的输入加输出
/// 按调用累加——只有 2025-09 ~ 2026-01 的版本记过,之后的 Cursor 本地不记用量,给 None
fn usage_from(data: &Value, bubbles: &HashMap<String, Value>) -> Usage {
    let mut model = None;
    let mut tokens = 0i64;
    for (_, bubble) in in_order(data, bubbles) {
        if let Some(name) =
            optional_string(bubble.pointer("/modelInfo/modelName")).filter(|m| is_model_name(m))
        {
            model = Some(name);
        }
        let count = |pointer: &str| bubble.pointer(pointer).and_then(Value::as_i64).unwrap_or(0);
        tokens += count("/tokenCount/inputTokens") + count("/tokenCount/outputTokens");
    }
    Usage {
        model: model.or_else(|| composer_model(data)),
        tokens: (tokens > 0).then_some(tokens),
    }
}

/// 会话级的模型:`modelConfig.modelName`(当前选中的)> `usageData` 里请求数最多的模型
/// (`{模型: {costInCents, amount}}`,实际计过费的)
fn composer_model(data: &Value) -> Option<String> {
    optional_string(data.pointer("/modelConfig/modelName"))
        .filter(|m| is_model_name(m))
        .or_else(|| {
            data.get("usageData")?
                .as_object()?
                .iter()
                .filter(|(name, _)| is_model_name(name))
                .max_by_key(|(_, used)| used.get("amount").and_then(Value::as_i64).unwrap_or(0))
                .map(|(name, _)| name.clone())
        })
}

/// "default" 是 Auto 档的占位,不是模型名
fn is_model_name(name: &str) -> bool {
    !name.trim().is_empty() && name != "default"
}

/// 一个 composer 的元数据行(`composerData:<id>`);语句按连接缓存,批量读时只编译一次
fn read_composer(conn: &rusqlite::Connection, id: &str) -> Option<Value> {
    let raw: String = conn
        .prepare_cached("SELECT CAST(value AS TEXT) FROM cursorDiskKV WHERE key = ?1")
        .ok()?
        .query_row([format!("{COMPOSER_PREFIX}{id}")], |x| x.get(0))
        .ok()?;
    serde_json::from_str(&raw).ok()
}

/// 一个 composer 的全部气泡正文(`bubbleId:<id>:<气泡>`),按气泡 id 索引。一次范围扫描全取
/// 回来(逐条点查是 N 次往返);上界用同前缀接 U+FFFF:UUID 只含 [0-9a-f-],不会越界到别的
/// 会话。Cursor 清理过的气泡会留下 value 为 NULL 的行(本机 2.5 GB 库里 983 行、波及 107 个
/// 会话),按"已被清理"跳过——当成错误会让整个会话解析失败
fn read_bubbles(conn: &rusqlite::Connection, id: &str) -> rusqlite::Result<HashMap<String, Value>> {
    let prefix = format!("{BUBBLE_PREFIX}{id}:");
    let mut stmt = conn.prepare(
        "SELECT substr(key, ?2), CAST(value AS TEXT) FROM cursorDiskKV
         WHERE key >= ?1 AND key < ?3",
    )?;
    let upper = format!("{prefix}\u{FFFF}");
    let cut = prefix.len() as i64 + 1;
    let mut bubbles = HashMap::new();
    let mut found = stmt.query(rusqlite::params![&prefix, cut, &upper])?;
    while let Some(row) = found.next()? {
        let bubble_id: String = row.get(0)?;
        let Some(body) = row.get::<_, Option<String>>(1)? else {
            continue;
        };
        if let Ok(v) = serde_json::from_str::<Value>(&body) {
            bubbles.insert(bubble_id, v);
        }
    }
    Ok(bubbles)
}

/// 按 `fullConversationHeadersOnly` 的顺序配出(顺序表项, 气泡正文)。**顺序只认这张表**——
/// KV 表按 key 字典序,而 bubbleId 是随机 UUID,照 key 序读会把对话打乱。顺序表里有、KV 里
/// 没有的(气泡行被 Cursor 清理过,老会话常见)跳过
fn in_order<'a>(
    data: &'a Value,
    bubbles: &'a HashMap<String, Value>,
) -> impl Iterator<Item = (&'a Value, &'a Value)> {
    data.get("fullConversationHeadersOnly")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|head| Some((head, bubbles.get(head.get("bubbleId")?.as_str()?)?)))
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

    fn build_meta(
        &self,
        r: &SessionFileRef,
        row: &IdeRow,
        message_count: i64,
        usage: Usage,
    ) -> SessionMeta {
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
            model: usage.model,
            tokens_used: usage.tokens,
            archived: false,
            source: None,
            favorite: false,
            pinned: false,
        }
    }

    /// 单会话解析:一次连接,先读 composerData 拿气泡顺序,再按
    /// `bubbleId:<cid>:` 前缀范围扫出正文,按 `in_order` 排好
    fn parse(&self, r: &SessionFileRef) -> Result<(SessionMeta, Vec<TranscriptMessage>)> {
        let ro = open_sqlite_ro(&self.db, "cursor-ide")
            .ok_or_else(|| anyhow!("cannot open cursor IDE store"))?;
        let data = read_composer(&ro.conn, &r.native_id)
            .ok_or_else(|| anyhow!("cursor composer {} not in store", r.native_id))?;
        let bubbles = read_bubbles(&ro.conn, &r.native_id)?;

        let mut messages: Vec<TranscriptMessage> = Vec::new();
        for (head, bubble) in in_order(&data, &bubbles) {
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
        // 行信息从已经读到的 composer 现算(与 `rows` 的 SQL 同一算法,列表与详情对得上),
        // 不去翻枚举快照:Cursor 正在写库时快照随库戳失效,每条会话解析都会触发一次整库重列
        let row = IdeRow::from_data(&r.native_id, &data);
        let mut meta = self.build_meta(r, &row, count, usage_from(&data, &bubbles));
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
    // 没装 Cursor(库不在)、老版本没有 composerHeaders 表 = 确定没有关系;库在、表在但读
    // 不出才是"不知道"——原先三种都给 None,没装 Cursor 的机器每轮都把 Cursor 的父子
    // 关系冻住、子代理会话永远进不了根列表(2026-09-22 review)
    if !db.is_file() {
        return Some(Vec::new());
    }
    let ro = open_sqlite_ro(db, "cursor-ide")?;
    let has_table: bool = ro
        .conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'composerHeaders')",
            [],
            |r| r.get(0),
        )
        .ok()?;
    if !has_table {
        return Some(Vec::new());
    }
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
    /// 从 composer 的 JSON 现算,与 `rows` 的 SQL 逐列同一算法:改了一边另一边照改,
    /// 列表(快照)与详情(现算)才对得上
    fn from_data(id: &str, data: &Value) -> Self {
        let int = |key: &str| data.get(key).and_then(Value::as_i64).unwrap_or(0);
        let heads = data
            .get("fullConversationHeadersOnly")
            .and_then(Value::as_array);
        let last_bubble = heads
            .and_then(|h| h.last())
            .and_then(|h| h.get("createdAt"))
            .and_then(Value::as_str)
            .map(iso_ms)
            .unwrap_or(0);
        Self {
            id: id.to_string(),
            name: data
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            created_ms: int("createdAt"),
            updated_ms: pick_updated(int("lastUpdatedAt"), last_bubble, int("createdAt")),
            bubble_count: heads.map_or(0, |h| h.len() as i64),
            cwd: project_path_from_data(data).unwrap_or_default().to_string(),
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
                    self.build_meta(r, row, row.bubble_count, Usage::default()),
                );
            }
        }
        Some(out)
    }

    fn parse_session(&self, r: &SessionFileRef) -> Result<ParsedSession> {
        let (meta, messages) = self.parse(r)?;
        Ok(ParsedSession::derive(meta, &messages, 0))
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

    fn parent_links(&self) -> Option<Vec<(String, String)>> {
        // 库读不出 = None(scanner 保留库里的关系),不折成空
        let mtime = super::sqlite_ro::db_cache_stamp(&self.db);
        self.links_cache
            .get_or_try_build(mtime, || parent_links_from(&self.db))
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
