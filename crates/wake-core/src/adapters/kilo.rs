//! Kilo Code 一家两个数据源:
//!
//! - **新版**(2026-04 起 VS Code 扩展、CLI 与 JetBrains 插件共用的 OpenCode 分支):
//!   `~/.local/share/kilo/kilo.db`,由 `opencode::OpencodeAdapter::kilo()` 读——表与
//!   OpenCode 同形。
//! - **旧版扩展**(v4.x / v5.x,Roo Code / Cline 一系):每个任务一个目录
//!   `<编辑器用户数据>/User/globalStorage/kilocode.kilo-code/tasks/<taskId>/`,正文是
//!   `api_conversation_history.json`(Anthropic 消息数组),本文件的 `KiloLegacyAdapter`。
//!   扩展可以装在任何 VS Code 系编辑器里,所以默认根按编辑器各一条。
//!
//! 同一段对话在两边的身份:新版扩展把旧任务导进 kilo.db 时,会话 id 定成
//! `ses_migrated_` + sha1(任务 id) 的前 26 位(kilo-vscode 的
//! legacy-migration/sessions/lib/ids.ts)。旧任务在 Wake 里直接用这个 id——导过的那份
//! 与 kilo.db 里的同 key,交 scanner 的副本裁决(本源 `dedup_rank` 1,kilo.db 胜出,
//! 本源留作解析失败的回退);没导过的照常列出,日后导了 key 也不变,收藏与置顶跟着走。
//!
//! 正文的读法照 Kilo 自己的导入器(legacy-migration/sessions/lib/parts):`<task>` 里才是
//! 用户的任务、`<environment_details>` 是每轮附带的编辑器快照(丢掉)、`attempt_completion`
//! 的 result 是最终答复;此外把 tool_result 回挂到发起它的工具调用上(导入器挂在用户消息
//! 里),工具结果里夹带的用户回复(`<feedback>` / `<answer>`)单独成一条用户消息——工具
//! 输出不进全文索引,不拆出来这些话就搜不到。

use super::parse_utils::*;
use super::sqlite_ro::open_sqlite_ro;
use super::AgentAdapter;
use crate::models::*;
use anyhow::{anyhow, Context as _, Result};
use rusqlite::OptionalExtension;
use serde_json::Value;
use std::borrow::Cow;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// 扩展 id:globalStorage 下的目录名,也是 state.vscdb 里 globalState 的 key
const EXTENSION_ID: &str = "kilocode.kilo-code";
const API_FILE: &str = "api_conversation_history.json";
/// 较新的旧版逐任务写的元数据(标题 / 工作区 / token / 父任务)
const ITEM_FILE: &str = "history_item.json";
/// Roo 后来把 taskHistory 从 globalState 挪到了这里(Kilo 的 Roo 导入器同样两处都认)
const INDEX_FILE: &str = "_index.json";

/// 可能装过旧版扩展的 VS Code 系编辑器:(用户数据目录名, 显示名)。用户数据根三平台
/// 各异(`vscode_user_data`),目录名相同。显示名作 via 徽章
const EDITORS: &[(&str, &str)] = &[
    ("Code", "VS Code"),
    ("Code - Insiders", "VS Code Insiders"),
    ("VSCodium", "VSCodium"),
    ("Cursor", "Cursor"),
    ("Windsurf", "Windsurf"),
    ("Trae", "Trae"),
    ("Trae CN", "Trae CN"),
    ("Kiro", "Kiro"),
    ("Antigravity", "Antigravity"),
    ("Void", "Void"),
    ("Positron", "Positron"),
    ("CodeBuddy", "CodeBuddy"),
    ("Qoder", "Qoder"),
];

/// Remote-SSH 一类的服务端:用户数据在 `<home>/<目录>/data`(扩展装在远端时写在这里;
/// Wake 跑在那台机器上时它就是本地数据)。远程同步只镜像得到 VS Code 那个(见
/// remote::REMOTE_LAYOUTS,每家一个挂载点)
const SERVERS: &[(&str, &str)] = &[
    (".vscode-server", "VS Code"),
    (".cursor-server", "Cursor"),
    (".windsurf-server", "Windsurf"),
];

/// 新版扩展导入旧任务时给的会话 id(见文件头)。Wake 里旧任务的 native id 就是它
pub fn migrated_session_id(task_id: &str) -> String {
    let digest = sha1_smol::Sha1::from(task_id.as_bytes())
        .digest()
        .to_string();
    format!("ses_migrated_{}", &digest[..26])
}

/// 旧任务在 Wake 里的会话 key
pub fn legacy_task_key(task_id: &str) -> String {
    session_key(AgentId::Kilo, "", &migrated_session_id(task_id))
}

/// 会话的胜出副本是不是旧版任务文件:旧任务没有能接着聊的 CLI(新版 `kilo --session`
/// 只认 kilo.db 里的会话),Open In 据此不画
pub fn is_legacy_task(file_path: &str) -> bool {
    Path::new(file_path)
        .file_name()
        .is_some_and(|name| name == API_FILE)
}

/// 旧版扩展的任务目录:选中的就是它(`tasks`)、扩展目录、globalStorage、编辑器的 User
/// 目录或用户数据根都认。目录名就是 tasks / 扩展 id 时只看名字——远程挂载点在同步落盘
/// 之前就要定形;再往上几层看存在性
fn legacy_tasks_dir(dir: &Path) -> Option<PathBuf> {
    let name = dir.file_name()?.to_string_lossy();
    if name.eq_ignore_ascii_case(EXTENSION_ID) {
        return Some(dir.join("tasks"));
    }
    if name == "tasks" {
        return Some(dir.to_path_buf());
    }
    [
        PathBuf::from(EXTENSION_ID),
        Path::new("globalStorage").join(EXTENSION_ID),
        Path::new("User").join("globalStorage").join(EXTENSION_ID),
    ]
    .into_iter()
    .map(|sub| dir.join(sub))
    .find(|ext| ext.is_dir())
    .map(|ext| ext.join("tasks"))
}

fn editor_tasks_dir(user_data: &Path) -> PathBuf {
    user_data
        .join("User")
        .join("globalStorage")
        .join(EXTENSION_ID)
        .join("tasks")
}

/// 任务目录在哪个编辑器的用户数据里(`editor_tasks_dir` 的逆):默认根、自定义 location
/// 与远程镜像都按它认出编辑器——远程 `~/.vscode-server` 下的任务照样标 VS Code。
/// 认不出 = None
fn editor_of(tasks: &Path) -> Option<&'static str> {
    let user_data = tasks.ancestors().nth(4)?;
    if tasks != editor_tasks_dir(user_data) {
        return None;
    }
    let name = user_data.file_name()?.to_string_lossy();
    if let Some((_, editor)) = EDITORS.iter().find(|(dir, _)| *dir == name) {
        return Some(editor);
    }
    // 服务端的用户数据是 `<home>/.vscode-server/data`:名字在上一级
    let server = user_data.parent()?.file_name()?.to_string_lossy();
    SERVERS
        .iter()
        .find(|(dir, _)| name == "data" && *dir == server)
        .map(|(_, editor)| *editor)
}

/// 旧版扩展的任务目录(见文件头)
pub struct KiloLegacyAdapter {
    roots: Vec<LegacyRoot>,
}

struct LegacyRoot {
    /// `…/globalStorage/kilocode.kilo-code/tasks`
    tasks: PathBuf,
    /// 哪个编辑器的(via 徽章,`editor_of`);认不出(任意自定义目录)是 None
    editor: Option<&'static str>,
    /// taskHistory 索引(`_index.json` 与编辑器 state.vscdb 里的 globalState 合在一起),
    /// 按两个文件的戳缓存。只给缺 history_item.json 的老任务补标题 / 工作区 / 父任务
    index: MtimeCache<Arc<HashMap<String, Value>>>,
}

impl LegacyRoot {
    fn new(tasks: PathBuf) -> Self {
        Self {
            editor: editor_of(&tasks),
            tasks,
            index: MtimeCache::new(),
        }
    }

    /// globalState 所在的库:`tasks` 的上两级是 globalStorage。自定义 location 不是这个
    /// 形状(或是远程缓存里没同步这个库)就没有
    fn state_db(&self) -> Option<PathBuf> {
        let ext = self.tasks.parent()?;
        ext.file_name()?
            .to_string_lossy()
            .eq_ignore_ascii_case(EXTENSION_ID)
            .then(|| ext.parent().map(|g| g.join("state.vscdb")))
            .flatten()
    }

    /// 两处索引合在一起。**None 是"不知道"**:库在却从没读成功过(读失败时交上一次读好的,
    /// 都不缓存、下轮再试)——父子关系的快照据此整份报 None,别把读失败当成关系解除
    fn index(&self) -> Option<Arc<HashMap<String, Value>>> {
        let index_file = self.tasks.join(INDEX_FILE);
        let state_db = self.state_db();
        let file_stamp = fs::metadata(&index_file).map(|m| mtime_ms(&m)).unwrap_or(0);
        let db_stamp = state_db
            .as_deref()
            .map(super::sqlite_ro::db_cache_stamp)
            .unwrap_or(0);
        let stamp = file_stamp.wrapping_mul(1_000_003) ^ db_stamp;
        let build = || {
            // globalState 先进、_index.json 后进覆盖:Roo 把索引挪出 globalState 之后,
            // 那边留下的是停在挪走那一刻的旧表
            let mut map = match state_db.as_deref() {
                Some(db) => read_global_state_history(db)?,
                None => HashMap::new(),
            };
            map.extend(read_index_file(&index_file));
            Some(Arc::new(map))
        };
        self.index.get_or_stale(stamp, build)
    }

    /// 任务的元数据:history_item.json(逐任务、最新的写端)优先,其次两处索引;索引不知道
    /// 时展示退到只看 _index.json
    fn history_item(&self, task_dir: &Path, task_id: &str) -> Option<Value> {
        if let Some(item) = read_item_file(task_dir).ok().flatten() {
            return Some(item);
        }
        match self.index() {
            Some(index) => index.get(task_id).cloned(),
            None => read_index_file(&self.tasks.join(INDEX_FILE)).remove(task_id),
        }
    }

    /// 任务的会话引用。缺 history_item.json 的老任务,标题 / 工作区 / token 来自索引:把
    /// 索引里那一条也算进 mtime / size,它变了(或索引从读不出变成读得出)才会重解析——
    /// 只看两个索引文件的戳不行,任何编辑器写 state.vscdb 都会让所有老任务重来一遍
    fn session_ref(
        &self,
        task_dir: &Path,
        index: Option<&HashMap<String, Value>>,
    ) -> Option<SessionFileRef> {
        let (mut r, has_item) = task_ref(task_dir)?;
        let task_id = task_dir.file_name()?.to_string_lossy();
        if let Some(entry) = index
            .filter(|_| !has_item)
            .and_then(|i| i.get(task_id.as_ref()))
        {
            r.mtime_ms = r.mtime_ms.max(item_i64(Some(entry), "ts"));
            r.size += entry.to_string().len() as i64;
        }
        Some(r)
    }
}

/// `history_item.json`:不存在 / 写坏了都是 None;读不出(权限、I/O)才是 Err——父子关系
/// 的快照要分清"没有"与"不知道"
fn read_item_file(task_dir: &Path) -> std::io::Result<Option<Value>> {
    match fs::read(task_dir.join(ITEM_FILE)) {
        Ok(bytes) => Ok(serde_json::from_slice::<Value>(&bytes)
            .ok()
            .filter(Value::is_object)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// 索引里的条目按任务 id 收成表(整棵 JSON 交进来、条目搬出去,不逐条复制)
fn index_entries(mut json: Value, key: &str) -> HashMap<String, Value> {
    let Some(Value::Array(items)) = json.get_mut(key).map(Value::take) else {
        return HashMap::new();
    };
    items
        .into_iter()
        .filter_map(|item| {
            let id = item.get("id")?.as_str()?.to_string();
            Some((id, item))
        })
        .collect()
}

fn read_index_file(path: &Path) -> HashMap<String, Value> {
    fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .map(|json| index_entries(json, "entries"))
        .unwrap_or_default()
}

/// 编辑器 `state.vscdb` 的 ItemTable 里,扩展的 globalState 整份 JSON 存在扩展 id 这个
/// key 下,taskHistory 是其中一项。表或行不在 = 没有(Some 空);库打不开 / 读出错 = None
fn read_global_state_history(db: &Path) -> Option<HashMap<String, Value>> {
    if !db.is_file() {
        return Some(HashMap::new());
    }
    let ro = open_sqlite_ro(db, "kilo-legacy-state")?;
    let has_table: bool = ro
        .conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='ItemTable')",
            [],
            |r| r.get(0),
        )
        .ok()?;
    if !has_table {
        return Some(HashMap::new());
    }
    // key 是扩展声明的 id 原样(全小写);精确比较才走得上 key 的唯一索引——Cursor 的这个库
    // 有几个 GB,按 NOCASE 比就是整表扫
    let state: Option<Value> = ro
        .conn
        .query_row(
            "SELECT value FROM ItemTable WHERE key = ?1",
            [EXTENSION_ID],
            |r| {
                Ok(r.get_ref(0)?
                    .as_bytes()
                    .ok()
                    .and_then(|v| serde_json::from_slice(v).ok()))
            },
        )
        .optional()
        .ok()?
        .flatten();
    Some(
        state
            .map(|state| index_entries(state, "taskHistory"))
            .unwrap_or_default(),
    )
}

/// 任务目录 → (会话引用, 有没有 history_item.json)。没有正文文件、空数组(建了任务却
/// 没发出去)的不算会话。mtime / size 把 history_item.json 也算进去:标题与 token 在那里变
fn task_ref(task_dir: &Path) -> Option<(SessionFileRef, bool)> {
    let task_id = task_dir.file_name()?.to_str()?;
    if task_id.starts_with('.') {
        return None;
    }
    let api = task_dir.join(API_FILE);
    let meta = fs::metadata(&api).ok()?;
    if !meta.is_file() || meta.len() <= 2 {
        return None;
    }
    let item = fs::metadata(task_dir.join(ITEM_FILE))
        .ok()
        .filter(|m| m.is_file());
    let r = SessionFileRef {
        agent: AgentId::Kilo,
        native_id: migrated_session_id(task_id),
        file_path: api.to_string_lossy().to_string(),
        mtime_ms: item.as_ref().map_or(0, mtime_ms).max(mtime_ms(&meta)),
        size: (meta.len() + item.as_ref().map_or(0, |m| m.len())) as i64,
    };
    Some((r, item.is_some()))
}

impl KiloLegacyAdapter {
    /// 默认根:VS Code 恒在(roster 契约:每个实例至少一条数据根;没装过旧版的人也只多
    /// 这一行),其余编辑器与远程开发服务端构造时有扩展目录才收——旧版已停更,不会
    /// 在 Wake 启动后才冒出来
    pub fn new() -> Self {
        let home = super::home_dir().unwrap_or_default();
        let editors = EDITORS.iter().map(|(dir, _)| super::vscode_user_data(dir));
        let servers = SERVERS.iter().map(|(dir, _)| home.join(dir).join("data"));
        let roots = editors
            .chain(servers)
            .map(|user_data| editor_tasks_dir(&user_data))
            .enumerate()
            .filter(|(i, tasks)| *i == 0 || tasks.parent().is_some_and(Path::is_dir))
            .map(|(_, tasks)| LegacyRoot::new(tasks))
            .collect();
        Self { roots }
    }

    /// 自定义 location 与远程挂载点(只有一条根)
    fn at(tasks: PathBuf) -> Self {
        Self {
            roots: vec![LegacyRoot::new(tasks)],
        }
    }

    fn root_for(&self, task_dir: &Path) -> Option<&LegacyRoot> {
        let tasks = task_dir.parent()?;
        self.roots.iter().find(|root| root.tasks == tasks)
    }

    fn parse(
        &self,
        r: &SessionFileRef,
        decode_images: bool,
    ) -> Result<(SessionMeta, Vec<TranscriptMessage>, u32)> {
        let _image_budget = transcript_image_decode_budget(decode_images);
        let api = Path::new(&r.file_path);
        let task_dir = api
            .parent()
            .ok_or_else(|| anyhow!("kilo task file has no task directory"))?;
        let task_id = task_dir
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let root = self.root_for(task_dir);
        let item = match root {
            Some(root) => root.history_item(task_dir, &task_id),
            None => read_item_file(task_dir).ok().flatten(),
        };
        let parsed = parse_api_history(api, decode_images)?;
        let meta = build_meta(
            r,
            &task_id,
            item.as_ref(),
            &parsed,
            root.and_then(|root| root.editor),
        );
        Ok((meta, parsed.messages, parsed.unknown))
    }
}

fn item_i64(item: Option<&Value>, key: &str) -> i64 {
    item.and_then(|v| v.get(key))
        .and_then(|v| v.as_i64().or_else(|| v.as_f64().map(|f| f as i64)))
        .unwrap_or(0)
}

/// 任务 id 里带的创建时刻:Cline 一系早年用 `Date.now()` 当 id,后来是 UUID v7
/// (前 48 位就是毫秒时间戳)
fn id_timestamp(task_id: &str) -> i64 {
    if let Ok(n) = task_id.parse::<i64>() {
        return if n > 1_000_000_000_000 { n } else { 0 };
    }
    let hex: String = task_id.chars().filter(|c| *c != '-').collect();
    if hex.len() == 32 && hex.as_bytes()[12] == b'7' {
        return i64::from_str_radix(&hex[..12], 16).unwrap_or(0);
    }
    0
}

fn build_meta(
    r: &SessionFileRef,
    task_id: &str,
    item: Option<&Value>,
    parsed: &LegacyParse,
    editor: Option<&'static str>,
) -> SessionMeta {
    let field = |key: &str| optional_string(item.and_then(|item| item.get(key)));
    // VS Code 的 fsPath 在 Windows 上写小写盘符,统一成别家的写法
    let project_path = field("workspace")
        .or_else(|| parsed.env_cwd.clone())
        .map(|p| canonical_project_path(&p))
        .unwrap_or_default();
    let title = field("task")
        .map(|t| clean_title_candidate(&clean_task_text(&t)))
        .filter(|t| !t.is_empty())
        .or_else(|| title_from_messages(&parsed.messages))
        .unwrap_or_else(|| UNTITLED.to_string());
    let first = [id_timestamp(task_id), parsed.first_ts]
        .into_iter()
        .filter(|t| *t > 0)
        .min()
        .unwrap_or(0);
    let last = parsed.last_ts.max(item_i64(item, "ts"));
    // Claude 的口径:输入 + 输出 + 缓存写入(缓存读取不算新用量)
    let tokens =
        item_i64(item, "tokensIn") + item_i64(item, "tokensOut") + item_i64(item, "cacheWrites");
    let message_count = parsed
        .messages
        .iter()
        .filter(|m| m.kind == MessageKind::Text)
        .count() as i64;
    SessionMeta {
        key: session_key(AgentId::Kilo, "", &r.native_id),
        host: String::new(),
        id: r.native_id.clone(),
        agent: AgentId::Kilo,
        title,
        project_name: project_name_of(&project_path),
        project_path,
        file_path: r.file_path.clone(),
        created_at: if first > 0 { first } else { r.mtime_ms },
        updated_at: if last > 0 { last } else { r.mtime_ms },
        message_count,
        size_bytes: r.size,
        git_branch: None,
        model: None,
        tokens_used: (tokens > 0).then_some(tokens),
        archived: false,
        source: editor.map(String::from),
        favorite: false,
        pinned: false,
    }
}

// ---------------------------------------------------------------- 正文

/// `<tag>…</tag>` 的内文(去首尾空白);没有或为空 = None
fn tag_inner(text: &str, tag: &str) -> Option<String> {
    extract_tag(text, tag)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// 截掉每轮附带的 `<environment_details>` 及其后的内容(它总在消息末尾)
fn strip_env_details(text: &str) -> &str {
    text.find("<environment_details>")
        .map_or(text, |i| &text[..i])
        .trim()
}

/// 历史标题与首条任务同一种清洗:`<task>` 包着就取里面,纯编辑器快照算空
fn clean_task_text(text: &str) -> String {
    tag_inner(text, "task").unwrap_or_else(|| strip_env_details(text).to_string())
}

/// 编辑器快照里的工作区:Roo 写 `# Current Workspace Directory (/p) Files`,Cline 写
/// `# Current Working Directory (/p) Files`。路径里可能有括号,按 `) Files` 收尾
fn env_details_cwd(text: &str) -> Option<String> {
    let env = extract_tag(text, "environment_details")?;
    for marker in [
        "# Current Workspace Directory (",
        "# Current Working Directory (",
    ] {
        let Some(start) = env.find(marker).map(|i| i + marker.len()) else {
            continue;
        };
        let rest = &env[start..];
        let end = rest
            .find(") Files")
            .or_else(|| rest.find(")\n"))
            .or_else(|| rest.find(')'))?;
        let path = rest[..end].trim();
        if !path.is_empty() {
            return Some(path.to_string());
        }
    }
    None
}

enum UserPiece {
    /// 人说的话
    Typed(String),
    /// 给模型看的说明(报错提示、续跑提示),折叠
    Meta(String),
}

/// 不在工具结果里的一段用户文本(已去掉编辑器快照)归哪边:包在标签里的(首条任务的
/// `<task>`、续跑提示里的 `<user_message>` 等)取标签里的话,扩展塞的报错 / 续跑提示
/// 折叠,其余就是人打的字。工具结果里的文本另由 `tool_result_reply` 认
fn classify_user_text(body: &str) -> UserPiece {
    for tag in ["task", "feedback", "answer", "user_message"] {
        if let Some(inner) = tag_inner(body, tag) {
            return UserPiece::Typed(inner);
        }
    }
    let scaffolding = body.starts_with("[ERROR]")
        || body.starts_with("[TASK RESUMPTION]")
        || body.starts_with("<explicit_instructions");
    if scaffolding {
        UserPiece::Meta(body.to_string())
    } else {
        UserPiece::Typed(body.to_string())
    }
}

/// XML 工具协议时代(原生工具调用之前)的工具结果抬头 `[read_file for 'a.ts'] Result:`:
/// 返回工具名
fn xml_tool_result_header(text: &str) -> Option<&str> {
    let rest = text.strip_prefix('[')?;
    let (head, tail) = rest.split_once(']')?;
    if head.contains('\n') || !tail.starts_with(" Result:") {
        return None;
    }
    head.split(' ').next()
}

/// 工具结果里夹带的、人说的话:反馈信封(扩展的原话都以 `The user …` 开头,话在
/// `<feedback>` / `<user_message>` 里——批准或拒绝时附的、完成后给的反馈)与追问工具的
/// 回答(`<answer>`)。读到的文件里恰好有这些标签不算,所以只认信封与工具
fn tool_result_reply(text: &str, tool: Option<&str>) -> Option<String> {
    // XML 时代抬头与正文可能在同一块
    let body = text
        .find("] Result:")
        .filter(|_| xml_tool_result_header(text).is_some())
        .map_or(text, |i| &text[i + "] Result:".len()..])
        .trim_start();
    if tool == Some("ask_followup_question") {
        if let Some(answer) = tag_inner(body, "answer") {
            return Some(answer);
        }
    }
    if !body.starts_with("The user ") {
        return None;
    }
    ["feedback", "user_message"]
        .iter()
        .find_map(|tag| tag_inner(body, tag))
}

/// 助手正文里的两种 XML 痕迹:`<thinking>` 挪进 thinking,`<attempt_completion>` 的
/// `<result>` 换成它本身(最终答复)。其余 XML 工具调用原样留着——那就是模型当时写的
fn split_assistant_text(text: &str, thinking: &mut Vec<String>) -> String {
    let mut out = text.to_string();
    while let Some(inner) = extract_tag(&out, "thinking") {
        if !inner.trim().is_empty() {
            thinking.push(inner.trim().to_string());
        }
        out = out.replacen(&format!("<thinking>{inner}</thinking>"), "", 1);
    }
    if let Some(block) = extract_tag(&out, "attempt_completion") {
        if let Some(result) = tag_inner(&block, "result") {
            out = out.replacen(
                &format!("<attempt_completion>{block}</attempt_completion>"),
                &result,
                1,
            );
        }
    }
    out.trim().to_string()
}

/// 模型的推理:Responses 式的独立 reasoning 条目(text 或 summary[])、个别 provider 放在
/// 消息外的 reasoning_content / reasoning_details
fn entry_reasoning(entry: &Value) -> Option<String> {
    if entry.get("type").and_then(Value::as_str) == Some("reasoning") {
        if let Some(text) = optional_string(entry.get("text")) {
            return Some(text);
        }
        let summary = joined_texts(entry.get("summary"), &["text"]);
        if !summary.is_empty() {
            return Some(summary);
        }
    }
    if let Some(text) = optional_string(entry.get("reasoning_content")) {
        return Some(text);
    }
    let details = joined_texts(entry.get("reasoning_details"), &["text", "reasoning"]);
    (!details.is_empty()).then_some(details)
}

fn joined_texts(items: Option<&Value>, keys: &[&str]) -> String {
    items
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| keys.iter().find_map(|k| item.get(*k)?.as_str()))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// 一条 API 消息的 content:字符串或块数组,统一成块(数组原地借用——内嵌图片的
/// base64 动辄几 MB,别整块复制)
fn content_blocks(entry: &Value) -> Cow<'_, [Value]> {
    match entry.get("content") {
        Some(Value::String(s)) => {
            Cow::Owned(vec![serde_json::json!({ "type": "text", "text": s })])
        }
        Some(Value::Array(blocks)) => Cow::Borrowed(blocks.as_slice()),
        _ => Cow::Owned(Vec::new()),
    }
}

/// Responses 式的独立推理条目:只有推理、没有正文,先于它所属的回答落盘。Kilo 的类型声明
/// 里它挂在 MessageParam 上,但 role 不保证有——不按 role 认
fn is_reasoning_item(entry: &Value) -> bool {
    entry.get("type").and_then(Value::as_str) == Some("reasoning") && entry.get("content").is_none()
}

/// `api_conversation_history.json` 的解析结果;逐条喂 `entry`,`finish` 收尾
#[derive(Default)]
struct LegacyParse {
    messages: Vec<TranscriptMessage>,
    unknown: u32,
    first_ts: i64,
    last_ts: i64,
    /// 首条带编辑器快照的用户消息里写着的工作区(缺 history_item 时的项目路径)
    env_cwd: Option<String>,
    /// tool_use id → (消息下标, 工具下标);结果在下一条用户消息里
    tool_index: HashMap<String, (usize, usize)>,
    /// 独立的 reasoning 条目先于它所属的回答落盘,攒着并进下一条助手消息
    pending_thinking: Vec<String>,
}

impl LegacyParse {
    fn entry(&mut self, entry: &Value, decode_images: bool) {
        if !entry.is_object() {
            self.unknown += 1;
            return;
        }
        let ts = entry.get("ts").and_then(Value::as_i64).unwrap_or(0);
        if ts > 0 {
            self.first_ts = if self.first_ts == 0 {
                ts
            } else {
                self.first_ts.min(ts)
            };
            self.last_ts = self.last_ts.max(ts);
        }
        let flag = |key: &str| entry.get(key).and_then(Value::as_bool) == Some(true);
        // 上下文压缩(condense)写下的摘要与截断标记;被摘要取代的原始消息还在
        // (condenseParent),照常展示
        if flag("isSummary") || flag("isTruncationMarker") {
            let text = content_parts(entry.get("content").unwrap_or(&Value::Null), false).text;
            if !text.is_empty() {
                let mut m = meta_msg(&text, ts);
                if flag("isSummary") {
                    m.kind = MessageKind::CompactSummary;
                }
                self.messages.push(m);
            }
            return;
        }
        if is_reasoning_item(entry) {
            self.pending_thinking.extend(entry_reasoning(entry));
            return;
        }
        match entry.get("role").and_then(Value::as_str) {
            Some("assistant") => self.assistant(entry, ts, decode_images),
            Some("user") => self.user(entry, ts, decode_images),
            _ => self.unknown += 1,
        }
    }

    fn assistant(&mut self, entry: &Value, ts: i64, decode_images: bool) {
        let mut content = ParsedContent::default();
        let mut thinking = std::mem::take(&mut self.pending_thinking);
        thinking.extend(entry_reasoning(entry));
        let mut tools = Vec::new();
        for block in content_blocks(entry).iter() {
            match block.get("type").and_then(Value::as_str) {
                Some("text") => {
                    let text = block.get("text").and_then(Value::as_str).unwrap_or("");
                    content.push_text(&split_assistant_text(text, &mut thinking));
                }
                Some("thinking") | Some("reasoning") => {
                    thinking.extend(optional_string(
                        block.get("thinking").or_else(|| block.get("text")),
                    ));
                }
                Some("redacted_thinking") => {}
                Some("tool_use") => {
                    let name = block.get("name").and_then(Value::as_str).unwrap_or("tool");
                    let input = block.get("input").cloned().unwrap_or(Value::Null);
                    // 最终答复以工具调用的形式交出来;Kilo 的导入器也把它当正文
                    let result = (name == "attempt_completion")
                        .then(|| optional_string(input.get("result")))
                        .flatten();
                    match result {
                        Some(result) => content.push_text(&result),
                        None => tools.push(tool_call_view(
                            optional_string(block.get("id")).unwrap_or_default(),
                            name,
                            &input,
                            None,
                            false,
                        )),
                    }
                }
                _ if is_image_part(block) => content.append(content_parts(block, decode_images)),
                _ => self.unknown += 1,
            }
        }
        self.push_assistant(content, thinking, tools, ts);
    }

    fn user(&mut self, entry: &Value, ts: i64, decode_images: bool) {
        self.flush_thinking(ts);
        let mut real = ParsedContent::default();
        let mut meta: Vec<String> = Vec::new();
        // XML 工具协议时代,一次工具结果拆成几块:先是 `[read_file for 'x'] Result:` 抬头,
        // 内容在同一块或后面的块里——抬头之后都是结果本体(记下是哪个工具)
        let mut tool: Option<String> = None;
        for block in content_blocks(entry).iter() {
            match block.get("type").and_then(Value::as_str) {
                Some("text") => {
                    let text = block.get("text").and_then(Value::as_str).unwrap_or("");
                    if self.env_cwd.is_none() {
                        self.env_cwd = env_details_cwd(text);
                    }
                    let body = strip_env_details(text);
                    if body.is_empty() {
                        continue;
                    }
                    if let Some(name) = xml_tool_result_header(body) {
                        tool = Some(name.to_string());
                    }
                    if tool.is_some() {
                        match tool_result_reply(body, tool.as_deref()) {
                            Some(reply) => real.push_text(&reply),
                            None => meta.push(body.to_string()),
                        }
                        continue;
                    }
                    match classify_user_text(body) {
                        UserPiece::Typed(t) => real.push_text(&t),
                        UserPiece::Meta(t) => meta.push(t),
                    }
                }
                Some("tool_result") => self.tool_result(block, decode_images, &mut real),
                _ if is_image_part(block) => real.append(content_parts(block, decode_images)),
                _ => self.unknown += 1,
            }
        }
        if !meta.is_empty() {
            self.messages.push(meta_msg(&meta.join("\n\n"), ts));
        }
        if !real.text.is_empty() || !real.images.is_empty() {
            let mut m = text_msg(Role::User, &real.text, ts);
            m.images = real.images;
            self.messages.push(m);
        }
    }

    /// tool_result 回挂到发起它的工具调用上;结果里夹着的用户回复收进 `real`
    fn tool_result(&mut self, block: &Value, decode_images: bool, real: &mut ParsedContent) {
        let parsed = tool_result_parts(block.get("content").unwrap_or(&Value::Null), decode_images);
        let target = block
            .get("tool_use_id")
            .and_then(Value::as_str)
            .and_then(|id| self.tool_index.get(id))
            .copied();
        let tool = target.map(|(mi, ti)| self.messages[mi].tool_calls[ti].name.clone());
        if let Some(reply) = tool_result_reply(&parsed.text, tool.as_deref()) {
            real.push_text(&reply);
        }
        let Some((mi, ti)) = target else {
            // 发起它的调用已经转成了正文(attempt_completion):用户随反馈附的图归用户消息。
            // 按块取而不用 parsed.images——不解码时图片是 [image] 占位文字,两种解析都得
            // 留下这条消息,seq 才对得上
            for image in block
                .get("content")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter(|v| is_image_part(v))
            {
                real.append(content_parts(image, decode_images));
            }
            return;
        };
        let message = &mut self.messages[mi];
        message.tool_calls[ti].output = Some(clip(strip_env_details(&parsed.text), MAX_TOOL_IO).0);
        if block.get("is_error").and_then(Value::as_bool) == Some(true) {
            message.tool_calls[ti].is_error = true;
        }
        append_images_to_message_end(message, parsed.images);
    }

    fn push_assistant(
        &mut self,
        content: ParsedContent,
        thinking: Vec<String>,
        tools: Vec<ToolCallView>,
        ts: i64,
    ) {
        if content.text.is_empty()
            && content.images.is_empty()
            && thinking.is_empty()
            && tools.is_empty()
        {
            return;
        }
        let ParsedContent { text, images } = content;
        let (text, truncated) = clip(&text, MAX_MSG_TEXT);
        let ix = self.messages.len();
        for (ti, tool) in tools.iter().enumerate() {
            if !tool.id.is_empty() {
                self.tool_index.insert(tool.id.clone(), (ix, ti));
            }
        }
        self.messages.push(TranscriptMessage {
            seq: 0,
            role: Role::Assistant,
            kind: MessageKind::Text,
            text,
            truncated,
            tool_calls: tools,
            thinking: (!thinking.is_empty()).then(|| clip(&thinking.join("\n\n"), MAX_TOOL_IO).0),
            timestamp: (ts > 0).then_some(ts),
            model: None,
            images,
        });
    }

    /// 攒着的推理后面没有回答了(下一条是用户消息,或到了末尾)就自成一条
    fn flush_thinking(&mut self, ts: i64) {
        if !self.pending_thinking.is_empty() {
            let thinking = std::mem::take(&mut self.pending_thinking);
            self.push_assistant(ParsedContent::default(), thinking, Vec::new(), ts);
        }
    }

    fn finish(mut self) -> Self {
        self.flush_thinking(self.last_ts);
        assign_seq(&mut self.messages);
        self
    }
}

fn parse_api_history(path: &Path, decode_images: bool) -> Result<LegacyParse> {
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let json: Value =
        serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))?;
    let entries = json
        .as_array()
        .ok_or_else(|| anyhow!("kilo conversation history must be a JSON array"))?;
    let mut parsed = LegacyParse::default();
    for entry in entries {
        parsed.entry(entry, decode_images);
    }
    Ok(parsed.finish())
}

impl AgentAdapter for KiloLegacyAdapter {
    fn agent(&self) -> AgentId {
        AgentId::Kilo
    }

    /// 与 kilo.db 里导入过的那份同 key(见文件头):kilo.db 先、本源后
    fn dedup_rank(&self, _path: &str) -> u8 {
        1
    }

    fn list_session_files(&self) -> Result<Vec<SessionFileRef>> {
        let mut refs = Vec::new();
        for root in &self.roots {
            let Ok(entries) = fs::read_dir(&root.tasks) else {
                continue;
            };
            let index = root.index();
            // `_index.json` 一类的根级文件不是目录,task_ref 自然跳过
            refs.extend(
                entries
                    .flatten()
                    .filter_map(|e| root.session_ref(&e.path(), index.as_deref())),
            );
        }
        Ok(refs)
    }

    /// 只认正文文件(scanner 按库里的 file_path 重解析时走这里);不进文件监听,见 watch_paths
    fn file_ref(&self, path: &Path) -> Option<SessionFileRef> {
        if path.file_name()? != API_FILE {
            return None;
        }
        let task_dir = path.parent()?;
        match self.root_for(task_dir) {
            Some(root) => root.session_ref(task_dir, root.index().as_deref()),
            None => task_ref(task_dir).map(|(r, _)| r),
        }
    }

    /// 不进文件监听。旧版扩展已停更,任务目录基本不再变,启动 / 刷新时收就够;而每个任务
    /// 目录里都有 checkpoints/ 整份影子 git 仓库——Linux 的 inotify 递归监听逐目录挂 watch,
    /// 几百个任务就是几万个,启动时卡在 UI 线程上,旧内核的配额还会被用光
    fn watch_paths(&self) -> Vec<PathBuf> {
        Vec::new()
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

    fn session_paths(&self, meta: &SessionMeta) -> Vec<String> {
        // 任务是整个目录(界面回放、元数据、检查点),整目录进废纸篓
        Path::new(&meta.file_path)
            .parent()
            .map(|d| vec![d.to_string_lossy().to_string()])
            .unwrap_or_else(|| vec![meta.file_path.clone()])
    }

    fn cleanup_paths(&self, meta: &SessionMeta) -> Option<Vec<String>> {
        Some(self.session_paths(meta))
    }

    fn manages_parent_links(&self) -> bool {
        true
    }

    /// 子任务(new_task 派出去的)在自己的元数据里记着 parentTaskId
    fn parent_links_in_child(&self) -> bool {
        true
    }

    fn parent_links(&self) -> Option<Vec<(String, String)>> {
        let mut links = Vec::new();
        for root in &self.roots {
            let entries = match fs::read_dir(&root.tasks) {
                Ok(entries) => entries,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(_) => return None,
            };
            // 索引只在缺 history_item.json 时才查,一个根取一次;它不知道(读不出)整份交 None
            let mut index: Option<Option<Arc<HashMap<String, Value>>>> = None;
            for entry in entries.flatten() {
                let task_dir = entry.path();
                let Some((r, _)) = task_ref(&task_dir) else {
                    continue;
                };
                let task_id = entry.file_name().to_string_lossy().to_string();
                let parent = match read_item_file(&task_dir) {
                    Ok(Some(item)) => optional_string(item.get("parentTaskId")),
                    Ok(None) => {
                        let index = index.get_or_insert_with(|| root.index()).as_ref()?;
                        optional_string(
                            index
                                .get(&task_id)
                                .and_then(|item| item.get("parentTaskId")),
                        )
                    }
                    Err(_) => return None,
                };
                if let Some(parent) = parent.filter(|p| *p != task_id) {
                    links.push((
                        session_key(AgentId::Kilo, "", &r.native_id),
                        legacy_task_key(&parent),
                    ));
                }
            }
        }
        Some(links)
    }

    /// 自定义 location / 远程挂载点里扩展的任务目录归本实例,其余归 kilo.db 那个实例
    fn claims_custom_root(&self, dir: &Path) -> bool {
        legacy_tasks_dir(dir).is_some()
    }

    fn with_custom_root(&self, dir: PathBuf) -> Box<dyn AgentAdapter> {
        Box::new(Self::at(legacy_tasks_dir(&dir).unwrap_or(dir)))
    }

    fn data_roots(&self) -> Vec<PathBuf> {
        self.roots.iter().map(|r| r.tasks.clone()).collect()
    }

    /// 各编辑器的任务目录彼此独立,kilo.db 那个源也是;不开这个能力位,面板上的 Remove
    /// 会按 agent 整家压制
    fn supports_individual_root_removal(&self) -> bool {
        true
    }

    fn excluding_data_roots(&self, roots: &[PathBuf]) -> Option<Box<dyn AgentAdapter>> {
        Some(Box::new(Self {
            roots: self
                .roots
                .iter()
                .filter(|r| !roots.contains(&r.tasks))
                .map(|r| LegacyRoot::new(r.tasks.clone()))
                .collect(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrated_ids_match_the_kilo_importer() {
        // sha1("abc") 的前 26 位——与 Node 的 createHash("sha1").update(id).digest("hex") 同值
        assert_eq!(
            migrated_session_id("abc"),
            "ses_migrated_a9993e364706816aba3e257178"
        );
        assert_eq!(
            legacy_task_key("abc"),
            "kilo:ses_migrated_a9993e364706816aba3e257178"
        );
    }

    #[test]
    fn task_ids_carry_their_creation_time() {
        assert_eq!(id_timestamp("1736400000000"), 1_736_400_000_000);
        assert_eq!(id_timestamp("42"), 0);
        // UUID v7:前 48 位是毫秒
        assert_eq!(
            id_timestamp("0193f0a8-6c00-7abc-8def-0123456789ab"),
            0x0193_f0a8_6c00
        );
        // v4 没有时间
        assert_eq!(id_timestamp("0193f0a8-6c00-4abc-8def-0123456789ab"), 0);
    }

    #[test]
    fn user_text_keeps_only_what_a_person_typed() {
        let first = "<task>\nFix the QR scanner\n</task>\n\n<environment_details>\n# Current Workspace Directory (/w/app) Files\n</environment_details>";
        assert_eq!(env_details_cwd(first).as_deref(), Some("/w/app"));
        assert!(matches!(
            classify_user_text(strip_env_details(first)),
            UserPiece::Typed(t) if t == "Fix the QR scanner"
        ));
        assert_eq!(
            strip_env_details("<environment_details>\nstuff\n</environment_details>"),
            ""
        );
        assert!(matches!(
            classify_user_text("[ERROR] You did not use a tool in your previous response!"),
            UserPiece::Meta(_)
        ));
        assert!(matches!(
            classify_user_text("plain words"),
            UserPiece::Typed(t) if t == "plain words"
        ));
    }

    /// 工具结果里只有反馈信封与追问的回答是人说的;读到的文件里恰好有这些标签不算
    #[test]
    fn tool_results_only_yield_replies_from_envelopes() {
        assert_eq!(
            xml_tool_result_header("[read_file for 'a.ts'] Result:"),
            Some("read_file")
        );
        assert_eq!(
            xml_tool_result_header("[attempt_completion] Result:"),
            Some("attempt_completion")
        );
        assert_eq!(xml_tool_result_header("[ERROR] nope"), None);
        let feedback = "The user has provided feedback on the results.\n<feedback>\nadd tests too\n</feedback>";
        assert_eq!(
            tool_result_reply(feedback, Some("attempt_completion")).as_deref(),
            Some("add tests too")
        );
        // 抬头与信封在同一块
        let inline = format!("[attempt_completion] Result:\n\n{feedback}");
        assert_eq!(
            tool_result_reply(&inline, Some("attempt_completion")).as_deref(),
            Some("add tests too")
        );
        assert_eq!(
            tool_result_reply("<answer>\nvitest\n</answer>", Some("ask_followup_question"))
                .as_deref(),
            Some("vitest")
        );
        for file in [
            "<answer>not a reply</answer>",
            "# prompt\n<task>do X</task>\n<feedback>no</feedback>",
            "[read_file for 'p.md'] Result:\n<task>do X</task>",
        ] {
            assert_eq!(tool_result_reply(file, Some("read_file")), None, "{file}");
        }
    }

    #[test]
    fn assistant_xml_traces_are_unwrapped() {
        let mut thinking = Vec::new();
        let text = split_assistant_text(
            "<thinking>check the effect</thinking>\nDone.\n<attempt_completion>\n<result>\nFixed the leak.\n</result>\n</attempt_completion>",
            &mut thinking,
        );
        assert_eq!(thinking, vec!["check the effect"]);
        assert_eq!(text, "Done.\nFixed the leak.");
    }

    #[test]
    fn editors_are_recognised_from_the_task_path() {
        let support = Path::new("/h/Library/Application Support");
        assert_eq!(
            editor_of(&editor_tasks_dir(&support.join("Code"))),
            Some("VS Code")
        );
        assert_eq!(
            editor_of(&editor_tasks_dir(&support.join("Code - Insiders"))),
            Some("VS Code Insiders")
        );
        // 远程镜像:缓存树保持远端 home 的相对布局
        let mirror = Path::new("/w/remotes/devbox/.vscode-server/data");
        assert_eq!(editor_of(&editor_tasks_dir(mirror)), Some("VS Code"));
        assert_eq!(editor_of(Path::new("/backup/kilo/tasks")), None);
        assert_eq!(editor_of(&editor_tasks_dir(Path::new("/x/SomeFork"))), None);
    }
}
