use super::parse_utils::*;
use super::sqlite_ro::{db_cache_stamp, open_sqlite_ro, table_columns, virtual_path};
use super::AgentAdapter;
use crate::models::*;
use anyhow::{anyhow, Result};
use serde_json::Value;
use std::borrow::Cow;
use std::collections::HashMap;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Antigravity(Google,binary `agy`):CLI、桌面端与 IDE 共用一个会话 id 空间,本地有两种数据。
/// - `~/.gemini/antigravity-cli/conversation_summaries.db`:跨端的会话索引(WAL)。只有元数据
///   ——标题在 preview 列(title 列常空)、时间、workspace、父会话、会话所在的 app 数据目录
///   (`app_data_dir`,老库没有这一列);正文另存为加密 .pb,读不到。
/// - `<app 数据目录>/brain/<id>/.system_generated/logs/transcript.jsonl`:新版本写下的明文转录
///   (`~/.gemini/antigravity/brain` 与 `~/.gemini/antigravity-ide/brain`),用户贴图在同会话的
///   `.user_uploaded/media_<毫秒>.<ext>`。格式来自 PR #52(@iroha3)对真实数据的整理;本机的
///   brain 目录还没有转录,这一形态未经真机验证。
///
/// 两种都如实列出,同一会话转录与卡片的胜负交给 scanner 的副本裁决(`dedup_rank`:转录恒胜,
/// 转录读不出时卡片接得住);转录的标题、工作区与来自哪一端仍以索引为准。
pub struct AntigravityAdapter {
    /// 会话索引;选中的自定义 location 不含它时为 None
    db: Option<PathBuf>,
    /// 转录根(各 app 数据目录下的 `brain`)
    brains: Vec<PathBuf>,
    /// 索引全表很小(元数据行),按库戳缓存;读失败交回上一次读到的
    index_cache: MtimeCache<Arc<Index>>,
}

const DB_FILE: &str = "conversation_summaries.db";
/// 写 `brain` 的 app 数据目录(`~/.gemini` 下):桌面端 / CLI 与 IDE
const APP_DIRS: [&str; 2] = ["antigravity", "antigravity-ide"];
/// 会话目录里转录的相对路径
const TRANSCRIPT: [&str; 3] = [".system_generated", "logs", "transcript.jsonl"];
const ENCRYPTED_NOTE: &str =
    "Antigravity stores this conversation encrypted — only its summary is available in Wake.";

/// 会话 id → 索引里那一行
type Index = HashMap<String, AgRow>;

struct AgRow {
    title: String,
    preview: String,
    step_count: i64,
    modified_ms: i64,
    cwd: String,
    /// 子会话(派生的、并行比较的):卡片与转录都不列
    child: bool,
    /// 会话所在的 app 数据目录名;老库没有这一列,空
    app: String,
}

impl AgRow {
    /// 这一行展示出来的东西的指纹(dirty 判断用):卡片的 size 就是它,转录的 size 加上它
    fn fingerprint(&self) -> i64 {
        (self.title.len() + self.preview.len() + self.cwd.len() + self.app.len()) as i64
    }
}

impl AntigravityAdapter {
    pub fn new() -> Self {
        Self::at(&super::home_dir().unwrap_or_default().join(".gemini"))
    }

    /// `~/.gemini` 形态:索引 + 两个 app 数据目录下的 brain
    fn at(gemini: &Path) -> Self {
        Self::from_parts(
            Some(gemini.join("antigravity-cli").join(DB_FILE)),
            APP_DIRS
                .iter()
                .map(|app| gemini.join(app).join("brain"))
                .collect(),
        )
    }

    fn from_parts(db: Option<PathBuf>, brains: Vec<PathBuf>) -> Self {
        Self {
            db,
            brains,
            index_cache: MtimeCache::new(),
        }
    }

    /// 索引全表(含子会话,列表与转录都要按它滤)。None = 没有索引库,或从没读成功过。
    /// 库不在了是确定没有,不交回上一次读到的(删掉的会话会一直列着);在却读不出才用旧的
    fn index(&self) -> Option<Arc<Index>> {
        let db = self.db.as_ref().filter(|db| db.is_file())?;
        self.index_cache.get_or_stale(db_cache_stamp(db), || {
            let ro = open_sqlite_ro(db, "antigravity")?;
            let app = if table_columns(&ro.conn, "conversation_summaries").contains("app_data_dir")
            {
                "app_data_dir"
            } else {
                "''"
            };
            let sql = format!(
                "SELECT conversation_id, title, preview, step_count, last_modified_time,
                        workspace_uris, parent_conversation_id, nesting_depth, {app}
                 FROM conversation_summaries"
            );
            let mut stmt = ro.conn.prepare(&sql).ok()?;
            let index = stmt
                .query_map([], |r| {
                    let text =
                        |i: usize| r.get::<_, Option<String>>(i).map(Option::unwrap_or_default);
                    let row = AgRow {
                        title: text(1)?,
                        preview: text(2)?,
                        step_count: r.get::<_, Option<i64>>(3)?.unwrap_or(0),
                        modified_ms: sqlite_dt_ms(text(4)?.trim()),
                        cwd: canonical_project_path(&first_workspace(&text(5)?)),
                        child: !text(6)?.is_empty() || r.get::<_, Option<i64>>(7)?.unwrap_or(0) > 0,
                        app: text(8)?,
                    };
                    Ok((r.get::<_, String>(0)?, row))
                })
                .ok()?
                .collect::<rusqlite::Result<Index>>()
                .ok()?;
            Some(Arc::new(index))
        })
    }

    fn parse(
        &self,
        r: &SessionFileRef,
        decode_images: bool,
    ) -> Result<(SessionMeta, Vec<TranscriptMessage>, u32)> {
        let index = self.index();
        let row = index.as_deref().and_then(|index| index.get(&r.native_id));
        if is_transcript(&r.file_path) {
            return transcript(r, row, decode_images);
        }
        let row = row
            .ok_or_else(|| anyhow!("antigravity conversation {} not in the index", r.native_id))?;
        Ok(card(r, row))
    }
}

/// 两种数据共有的那一半:身份、文件与大小;其余字段由调用方填
fn base_meta(r: &SessionFileRef) -> SessionMeta {
    SessionMeta {
        key: session_key(AgentId::Antigravity, "", &r.native_id),
        host: String::new(),
        id: r.native_id.clone(),
        agent: AgentId::Antigravity,
        title: String::new(),
        project_name: String::new(),
        project_path: String::new(),
        file_path: r.file_path.clone(),
        created_at: 0,
        updated_at: 0,
        message_count: 0,
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

fn card_meta(r: &SessionFileRef, row: &AgRow) -> SessionMeta {
    // 库里只有 last_modified 一个时间,created/updated 同源
    let ts = if row.modified_ms > 0 {
        row.modified_ms
    } else {
        r.mtime_ms
    };
    SessionMeta {
        title: first_title(&[&row.title, &row.preview]),
        project_name: project_name_of(&row.cwd),
        project_path: row.cwd.clone(),
        created_at: ts,
        updated_at: ts,
        message_count: row.step_count,
        source: surface(&row.app),
        ..base_meta(r)
    }
}

/// 只有元数据的卡片:正文加密不可读,一条 System 消息承载 preview,详情页与 FTS 都有着落
fn card(r: &SessionFileRef, row: &AgRow) -> (SessionMeta, Vec<TranscriptMessage>, u32) {
    let mut text = ParsedContent::default();
    text.push_text(&row.preview);
    text.push_text(ENCRYPTED_NOTE);
    let mut messages = vec![text_msg(Role::System, &text.text, row.modified_ms)];
    assign_seq(&mut messages);
    (card_meta(r, row), messages, 0)
}

fn transcript(
    r: &SessionFileRef,
    row: Option<&AgRow>,
    decode_images: bool,
) -> Result<(SessionMeta, Vec<TranscriptMessage>, u32)> {
    let path = Path::new(&r.file_path);
    let t = read_transcript(path, decode_images)?;
    // 一条消息都读不出(截断、全是不认识的行)算解析失败:scanner 退回同一会话的卡片,
    // 不让坏转录把读得出的摘要顶掉(cursor.rs 对零消息的坏转录同样报 Err)
    if t.messages.is_empty() {
        return Err(anyhow!(
            "antigravity transcript has no readable messages: {}",
            r.file_path
        ));
    }
    // 标题、工作区与来自哪一端以索引为准;没进索引的会话退回转录里的线索
    let (indexed_title, preview) =
        row.map_or(("", ""), |row| (row.title.as_str(), row.preview.as_str()));
    let request = title_from_messages(&t.messages).unwrap_or_default();
    let project = row
        .map(|row| row.cwd.clone())
        .filter(|cwd| !cwd.is_empty())
        .or_else(|| t.workspace.as_deref().map(canonical_project_path))
        .unwrap_or_default();
    let app = row
        .map(|row| row.app.as_str())
        .filter(|app| !app.is_empty())
        .or_else(|| app_dir_of(path))
        .unwrap_or_default();
    let fallback = row
        .map(|row| row.modified_ms)
        .filter(|ms| *ms > 0)
        .unwrap_or(r.mtime_ms);
    let meta = SessionMeta {
        title: first_title(&[indexed_title, &request, preview]),
        project_name: project_name_of(&project),
        project_path: project,
        created_at: if t.first_ts > 0 { t.first_ts } else { fallback },
        updated_at: if t.last_ts > 0 { t.last_ts } else { fallback },
        message_count: t.messages.len() as i64,
        model: t.model,
        source: surface(app),
        ..base_meta(r)
    };
    Ok((meta, t.messages, t.unknown_lines))
}

/// 依次取第一个洗得出东西的候选作标题
fn first_title(candidates: &[&str]) -> String {
    candidates
        .iter()
        .map(|raw| clean_title_candidate(raw))
        .find(|title| !title.is_empty())
        .unwrap_or_else(|| UNTITLED.to_string())
}

/// 会话来自哪一端:IDE 挂 via 徽章;桌面端 / CLI 是默认形态,不挂
fn surface(app_dir: &str) -> Option<String> {
    (app_dir == "antigravity-ide").then(|| "IDE".to_string())
}

fn is_transcript(path: &str) -> bool {
    Path::new(path).file_name().and_then(|n| n.to_str()) == Some(TRANSCRIPT[2])
}

/// 会话目录里转录的相对路径,按平台分隔符拼(写死 `/` 的话 Windows 上与 watcher 报来的
/// 路径对不上)
fn transcript_rel() -> PathBuf {
    TRANSCRIPT.iter().collect()
}

/// `<brain>/<id>/.system_generated/logs/transcript.jsonl` → `<brain>/<id>`(形状不对给 None)
fn session_dir(transcript: &Path) -> Option<&Path> {
    transcript
        .ends_with(transcript_rel())
        .then(|| transcript.ancestors().nth(TRANSCRIPT.len()))
        .flatten()
}

/// 转录所在的 app 数据目录名(`<app>/brain/<id>/…` 的 `<app>`)
fn app_dir_of(transcript: &Path) -> Option<&str> {
    session_dir(transcript)?
        .parent()?
        .parent()?
        .file_name()?
        .to_str()
}

/// 转录的会话引用,列表与 watcher 同一个判据。索引里那一行也算进 mtime / size:它变了而
/// 转录没动(在 app 里改了名、转录先于索引写出来)也会重解析
fn transcript_ref(index: Option<&Index>, path: &Path) -> Option<SessionFileRef> {
    let id = session_dir(path)?.file_name()?.to_str()?;
    let row = index.and_then(|index| index.get(id));
    if row.is_some_and(|row| row.child) {
        return None;
    }
    let mut r = default_file_ref(AgentId::Antigravity, path)?;
    r.native_id = id.to_string();
    if let Some(row) = row {
        r.mtime_ms = r.mtime_ms.max(row.modified_ms);
        r.size += row.fingerprint();
    }
    Some(r)
}

fn card_ref(db: &Path, id: &str, row: &AgRow) -> SessionFileRef {
    SessionFileRef {
        agent: AgentId::Antigravity,
        native_id: id.to_string(),
        file_path: virtual_path(db, id),
        mtime_ms: row.modified_ms,
        // 正文不可读,这一行的指纹即内容指纹
        size: row.fingerprint(),
    }
}

/// workspace_uris JSON 数组("[\"file:///Users/…\"]")首项 → 本地路径
fn first_workspace(raw: &str) -> String {
    let Ok(v) = serde_json::from_str::<Value>(raw) else {
        return String::new();
    };
    let Some(uri) = v.as_array().and_then(|a| a.first()).and_then(Value::as_str) else {
        return String::new();
    };
    percent_decode(uri.strip_prefix("file://").unwrap_or(uri))
}

// ---------------------------------------------------------------- 转录

/// 一行一个步骤 `{step_index, source, type, status, created_at, content?, thinking?, tool_calls?}`。
/// `USER_INPUT` 是用户的一轮(正文在 `<USER_REQUEST>` 里,其余是 IDE 注入的元数据);
/// `PLANNER_RESPONSE` 是模型的一步(正文 / thinking / tool_calls),一轮里的多步并成一条助手
/// 消息;下面这些是工具执行的结果,按顺序回填到还没有输出的调用上;`CHECKPOINT` 是上下文压缩
const TOOL_RESULTS: &[&str] = &[
    "VIEW_FILE",
    "RUN_COMMAND",
    "GREP_SEARCH",
    "LIST_DIRECTORY",
    "CODE_ACTION",
    "BROWSER_SUBAGENT",
    "SEARCH_WEB",
    "READ_URL_CONTENT",
    "ASK_QUESTION",
    "GENERIC",
    "ERROR_MESSAGE",
];
/// 给模型看的上下文,不是对话
const KNOWN_SKIP: &[&str] = &[
    "CONVERSATION_HISTORY",
    "KNOWLEDGE_ARTIFACTS",
    "SYSTEM_MESSAGE",
];
/// 工具输入里能当一行摘要的字段,依次取第一个(键是 PascalCase,`make_preview` 认的那几个
/// snake_case 键对不上)
const PREVIEW_KEYS: &[&str] = &[
    "toolSummary",
    "toolAction",
    "CommandLine",
    "TargetFile",
    "AbsolutePath",
    "Query",
    "DirectoryPath",
];
/// 工具输入里的工作目录(没进索引的会话拿它推工作区)
const CWD_KEYS: &[&str] = &["Cwd", "DirectoryPath", "SearchPath"];

#[derive(Default)]
struct Transcript {
    messages: Vec<TranscriptMessage>,
    /// 工具的工作目录 > 用户当时打开的文件所在目录
    workspace: Option<String>,
    model: Option<String>,
    first_ts: i64,
    last_ts: i64,
    unknown_lines: u32,
}

#[derive(Default)]
struct Turn {
    content: ParsedContent,
    thinking: Vec<String>,
    tool_calls: Vec<ToolCallView>,
    timestamp: Option<i64>,
    model: Option<String>,
}

impl Turn {
    fn flush(&mut self, messages: &mut Vec<TranscriptMessage>) {
        let turn = std::mem::take(self);
        if turn.content.text.is_empty() && turn.thinking.is_empty() && turn.tool_calls.is_empty() {
            return;
        }
        let (text, truncated) = clip(&turn.content.text, MAX_MSG_TEXT);
        messages.push(TranscriptMessage {
            seq: 0,
            role: Role::Assistant,
            kind: MessageKind::Text,
            text,
            truncated,
            tool_calls: turn.tool_calls,
            thinking: (!turn.thinking.is_empty())
                .then(|| clip(&turn.thinking.join("\n\n"), MAX_TOOL_IO).0),
            timestamp: turn.timestamp,
            model: turn.model,
            images: Vec::new(),
        });
    }
}

fn read_transcript(path: &Path, decode_images: bool) -> Result<Transcript> {
    let _budget = transcript_image_decode_budget(decode_images);
    let reader = BufReader::new(std::fs::File::open(path)?);
    let mut t = Transcript::default();
    let mut turn = Turn::default();
    let mut document = None;

    for row in jsonl_values(reader) {
        let Some(row) = row? else {
            t.unknown_lines += 1;
            continue;
        };
        let str_of = |key: &str| row.get(key).and_then(Value::as_str).unwrap_or("");
        let ts = iso_ms(str_of("created_at"));
        if ts > 0 {
            if t.first_ts == 0 {
                t.first_ts = ts;
            }
            t.last_ts = ts;
        }
        let step = str_of("type");
        match step {
            "USER_INPUT" => {
                turn.flush(&mut t.messages);
                let content = str_of("content");
                let text = extract_tag(content, "USER_REQUEST")
                    .map(|inner| inner.trim().to_string())
                    .unwrap_or_else(|| content.trim().to_string());
                if let Some(selected) = model_selection(content) {
                    t.model = Some(selected);
                }
                if document.is_none() {
                    document = active_document(content);
                }
                let mut message = text_msg(Role::User, &text, ts);
                message.model = t.model.clone();
                t.messages.push(message);
            }
            "PLANNER_RESPONSE" => {
                if turn.timestamp.is_none() && ts > 0 {
                    turn.timestamp = Some(ts);
                }
                if turn.model.is_none() {
                    turn.model = t.model.clone();
                }
                turn.content.push_text(str_of("content"));
                let thinking = str_of("thinking").trim();
                if !thinking.is_empty() {
                    turn.thinking.push(thinking.to_string());
                }
                let step_ix = row.get("step_index").and_then(Value::as_i64).unwrap_or(0);
                let calls = row.get("tool_calls").and_then(Value::as_array);
                for (n, call) in calls.into_iter().flatten().enumerate() {
                    let args = tool_args(call.get("args"));
                    if t.workspace.is_none() {
                        t.workspace = first_str(&args, CWD_KEYS);
                    }
                    let name = call.get("name").and_then(Value::as_str).unwrap_or_default();
                    let mut view =
                        tool_call_view(format!("step-{step_ix}-{n}"), name, &args, None, false);
                    if let Some(preview) = first_str(&args, PREVIEW_KEYS) {
                        view.input_preview = make_preview(&Value::String(preview));
                    }
                    turn.tool_calls.push(view);
                }
            }
            _ if TOOL_RESULTS.contains(&step) => {
                let content = str_of("content").trim();
                let is_error = str_of("status") == "ERROR" || step == "ERROR_MESSAGE";
                // 结果按调用的先后到达:回填给第一个还没有输出的调用
                if let Some(call) = turn.tool_calls.iter_mut().find(|c| c.output.is_none()) {
                    call.output = Some(clip(content, MAX_TOOL_IO).0);
                    call.is_error = is_error;
                } else if is_error && !content.is_empty() {
                    // 没有待回填的调用:先把这一轮已有的回复落下,错误排在它后面
                    turn.flush(&mut t.messages);
                    t.messages.push(text_msg(Role::System, content, ts));
                }
            }
            "CHECKPOINT" => {
                turn.flush(&mut t.messages);
                let content = str_of("content").trim();
                if !content.is_empty() {
                    let mut summary = text_msg(Role::System, content, ts);
                    summary.kind = MessageKind::CompactSummary;
                    t.messages.push(summary);
                }
            }
            _ if KNOWN_SKIP.contains(&step) => {}
            _ => t.unknown_lines += 1,
        }
    }
    turn.flush(&mut t.messages);
    if t.workspace.is_none() {
        t.workspace = document.and_then(|doc| {
            Path::new(&doc)
                .parent()
                .map(|dir| dir.to_string_lossy().into_owned())
        });
    }
    if decode_images {
        if let Some(dir) = session_dir(path) {
            attach_uploads(&mut t.messages, dir);
        }
    }
    assign_seq(&mut t.messages);
    Ok(t)
}

/// 工具参数:对象,或者装着对象的 JSON 字符串(解不开就按原样)
fn tool_args(args: Option<&Value>) -> Cow<'_, Value> {
    let Some(args) = args else {
        return Cow::Owned(Value::Null);
    };
    match args.as_str().and_then(|s| serde_json::from_str(s).ok()) {
        Some(parsed) => Cow::Owned(parsed),
        None => Cow::Borrowed(args),
    }
}

fn first_str(args: &Value, keys: &[&str]) -> Option<String> {
    let map = args.as_object()?;
    keys.iter().find_map(|key| {
        let value = map
            .get(*key)?
            .as_str()?
            .trim()
            .trim_matches(['"', '\''])
            .trim();
        (!value.is_empty()).then(|| value.to_string())
    })
}

/// `<USER_SETTINGS_CHANGE>` 里的 "changed setting `Model Selection` from X to Y."
fn model_selection(content: &str) -> Option<String> {
    let at = content.find("`Model Selection` from")?;
    let after = &content[at..];
    let to = after.find(" to ")? + " to ".len();
    let value = after[to..].lines().next()?.trim();
    let value = value
        .split(". ")
        .next()
        .unwrap_or(value)
        .trim_end_matches('.')
        .trim();
    (!value.is_empty()).then(|| value.to_string())
}

/// `<ADDITIONAL_METADATA>` 里的 "Active Document: /path/file.rs (LANGUAGE_RUST)"
fn active_document(content: &str) -> Option<String> {
    let at = content.find("Active Document:")?;
    let line = content[at + "Active Document:".len()..]
        .lines()
        .next()?
        .trim();
    let path = line.split_once(" (").map_or(line, |(path, _)| path).trim();
    (!path.is_empty()).then(|| path.to_string())
}

/// 用户贴图:`<会话>/.user_uploaded/media_<毫秒>.<ext>`,文件名里的毫秒是上传时刻,与所属那条
/// 用户消息的时间只差几秒(PR #52 实测 36 张:35 张在 5 分钟内)。按时间回挂到最近的一条用户
/// 消息、插在正文末尾;差出半小时的宁可不挂,也不塞给别的轮次
fn attach_uploads(messages: &mut [TranscriptMessage], session: &Path) {
    const MAX_GAP_MS: i64 = 30 * 60 * 1000;
    let Ok(entries) = std::fs::read_dir(session.join(".user_uploaded")) else {
        return;
    };
    let mut uploads: Vec<(i64, PathBuf)> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().to_string();
            let ms = name
                .strip_prefix("media_")?
                .split_once('.')?
                .0
                .parse()
                .ok()?;
            Some((ms, entry.path()))
        })
        .collect();
    uploads.sort();
    let users: Vec<(usize, i64)> = messages
        .iter()
        .enumerate()
        .filter(|(_, m)| m.role == Role::User)
        .filter_map(|(ix, m)| m.timestamp.map(|ts| (ix, ts)))
        .collect();
    for (ms, path) in uploads {
        let Some(&(ix, ts)) = users.iter().min_by_key(|(_, ts)| (ts - ms).abs()) else {
            return;
        };
        if (ts - ms).abs() > MAX_GAP_MS {
            continue;
        }
        if let Some(image) = image_from_session_file(session, &path) {
            append_images_to_message_end(&mut messages[ix], vec![image]);
        }
    }
}

impl AgentAdapter for AntigravityAdapter {
    fn agent(&self) -> AgentId {
        AgentId::Antigravity
    }

    fn list_session_files(&self) -> Result<Vec<SessionFileRef>> {
        let index = self.index();
        // 索引在却从没读成功过:报错,scanner 把这一家这一轮冻结住(库里的卡片原样留着),
        // 别把它当成"没有会话"——那会把卡片整批删掉;库不在才是确定没有
        if index.is_none() && self.db.as_ref().is_some_and(|db| db.is_file()) {
            return Err(anyhow!("cannot read the Antigravity conversation index"));
        }
        let index = index.as_deref();
        let rel = transcript_rel();
        let mut refs = Vec::new();
        for brain in &self.brains {
            let Ok(entries) = std::fs::read_dir(brain) else {
                continue;
            };
            refs.extend(
                entries
                    .flatten()
                    .filter_map(|entry| transcript_ref(index, &entry.path().join(&rel))),
            );
        }
        // 有转录的会话也照列卡片:scanner 按 dedup_rank 取转录,转录读不出时退回卡片
        if let (Some(db), Some(index)) = (&self.db, index) {
            refs.extend(
                index
                    .iter()
                    .filter(|(_, row)| !row.child)
                    .map(|(id, row)| card_ref(db, id, row)),
            );
        }
        Ok(refs)
    }

    fn file_ref(&self, path: &Path) -> Option<SessionFileRef> {
        transcript_ref(self.index().as_deref(), path)
    }

    fn quick_meta(&self, refs: &[SessionFileRef]) -> Option<HashMap<String, SessionMeta>> {
        let index = self.index()?;
        Some(
            refs.iter()
                // 卡片的元数据全在库里;转录要解析才有
                .filter(|r| !is_transcript(&r.file_path))
                .filter_map(|r| {
                    let row = index.get(&r.native_id)?;
                    Some((r.file_path.clone(), card_meta(r, row)))
                })
                .collect(),
        )
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

    /// 同一会话的转录永远压过库里的卡片——卡片只是转录还没写出来时的占位
    fn dedup_rank(&self, path: &str) -> u8 {
        u8::from(!is_transcript(path))
    }

    /// 转录是整个会话目录(贴图、生成的产物都在里面),整目录进废纸篓;卡片是库里的行,
    /// 只记墓碑
    fn session_paths(&self, meta: &SessionMeta) -> Vec<String> {
        match session_dir(Path::new(&meta.file_path)) {
            Some(dir) => vec![dir.to_string_lossy().to_string()],
            None => vec![meta.file_path.clone()],
        }
    }

    fn cleanup_paths(&self, meta: &SessionMeta) -> Option<Vec<String>> {
        is_transcript(&meta.file_path).then(|| self.session_paths(meta))
    }

    fn with_custom_root(&self, dir: PathBuf) -> Box<dyn AgentAdapter> {
        // 按名字认形状;只在选中的路径本身与它里面探索引库,不摸它外面的东西。远程挂载点
        // 在第一次同步之前还不存在,落到最后一支的 `.gemini` 形态
        let name = dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string();
        let adapter = if dir.is_file() {
            // 直接选了索引库文件
            Self::from_parts(Some(dir), Vec::new())
        } else if name == "brain" {
            Self::from_parts(None, vec![dir])
        } else if name == "antigravity-cli" || dir.join(DB_FILE).is_file() {
            // 索引库所在的目录(或它的拷贝)
            Self::from_parts(Some(dir.join(DB_FILE)), Vec::new())
        } else if APP_DIRS.contains(&name.as_str()) {
            Self::from_parts(None, vec![dir.join("brain")])
        } else {
            Self::at(&dir)
        };
        Box::new(adapter)
    }

    fn data_roots(&self) -> Vec<PathBuf> {
        self.db.iter().chain(&self.brains).cloned().collect()
    }

    /// 索引与各 brain 彼此独立,Session locations 里可以单独停掉其中一个
    fn supports_individual_root_removal(&self) -> bool {
        true
    }

    fn excluding_data_roots(&self, roots: &[PathBuf]) -> Option<Box<dyn AgentAdapter>> {
        Some(Box::new(Self::from_parts(
            self.db.clone().filter(|db| !roots.contains(db)),
            self.brains
                .iter()
                .filter(|brain| !roots.contains(brain))
                .cloned()
                .collect(),
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_input_metadata_is_read_out_of_the_injected_blocks() {
        let content = "<USER_REQUEST>\nFix it\n</USER_REQUEST>\n<ADDITIONAL_METADATA>\n\
                       Active Document: /Users/me/proj/src/ui.rs (LANGUAGE_RUST)\n\
                       </ADDITIONAL_METADATA>\n<USER_SETTINGS_CHANGE>\nThe user changed setting \
                       `Model Selection` from None to Gemini 3.7 Flash (High). No need to reply.\n\
                       </USER_SETTINGS_CHANGE>";
        assert_eq!(
            active_document(content).as_deref(),
            Some("/Users/me/proj/src/ui.rs")
        );
        assert_eq!(
            model_selection(content).as_deref(),
            Some("Gemini 3.7 Flash (High)")
        );
        assert_eq!(model_selection("<USER_REQUEST>hi</USER_REQUEST>"), None);
    }

    /// 没有待回填的调用时来的错误:排在这一轮已有的回复后面,不插到它前面
    #[test]
    fn a_standalone_error_follows_the_reply_before_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("transcript.jsonl");
        std::fs::write(
            &path,
            concat!(
                r#"{"type":"USER_INPUT","created_at":"2026-08-30T02:11:10Z","content":"<USER_REQUEST>go</USER_REQUEST>"}"#,
                "\n",
                r#"{"type":"PLANNER_RESPONSE","created_at":"2026-08-30T02:11:12Z","content":"Working on it."}"#,
                "\n",
                r#"{"type":"ERROR_MESSAGE","status":"ERROR","created_at":"2026-08-30T02:11:13Z","content":"quota exceeded"}"#,
                "\n",
            ),
        )
        .unwrap();
        let t = read_transcript(&path, false).unwrap();
        let roles: Vec<Role> = t.messages.iter().map(|m| m.role).collect();
        assert_eq!(roles, vec![Role::User, Role::Assistant, Role::System]);
        assert_eq!(t.messages[1].text, "Working on it.");
        assert_eq!(t.messages[2].text, "quota exceeded");
    }

    #[test]
    fn transcript_paths_have_one_shape() {
        let path: PathBuf = ["/h", ".gemini", "antigravity-ide", "brain", "abc"]
            .iter()
            .collect::<PathBuf>()
            .join(transcript_rel());
        let session: PathBuf = ["/h", ".gemini", "antigravity-ide", "brain", "abc"]
            .iter()
            .collect();
        assert_eq!(session_dir(&path), Some(session.as_path()));
        assert_eq!(app_dir_of(&path), Some("antigravity-ide"));
        assert_eq!(
            session_dir(Path::new("/h/brain/abc/logs/transcript.jsonl")),
            None
        );
        assert!(is_transcript(&path.to_string_lossy()));
        assert!(!is_transcript(
            "/h/.gemini/antigravity-cli/conversation_summaries.db#abc"
        ));
    }
}
