use super::parse_utils::*;
use super::AgentAdapter;
use crate::models::*;
use anyhow::{Context as _, Result};
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::io::{self, BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Craft Agents(craft-ai-agents/craft-agents-oss,Craft Docs 出的开源桌面 app;
/// 2026-09-24 按 v0.13.5 源码落地、真机两条会话核对,issue #44)。它自己不带 agent
/// 引擎:Anthropic 连接跑 Claude Agent SDK(无头 Claude Code,用的是用户真实的
/// `~/.claude`),其余连接(ChatGPT / Codex 订阅、Copilot、Google、OpenAI key…)跑
/// Pi SDK;Craft 在引擎之上另记一份给用户看的会话,Wake 读的就是这一份。
///
/// 布局:`<工作区>/sessions/<id>/session.jsonl`,默认工作区在
/// `~/.craft-agent/workspaces/<slug>/`。工作区可以建在磁盘任意位置(Obsidian 式,
/// 文件夹即工作区),登记在全局 `~/.craft-agent/config.json`——那个文件存着 server
/// token 与远程工作区 token,**不读**;别处的工作区由用户在 Session locations 里加
/// (根可以选工作区集合、单个工作区或 `.craft-agent` 本身)。
///
/// 首行 SessionHeader(name 是 AI 起的或用户改的标题,preview 是首条提问,
/// workingDirectory 是用户设的项目目录,sdkSessionId 是引擎那份转录的 id…),其余每行
/// 一条 StoredMessage:`type` ∈ user / assistant / tool / plan / info / error /
/// warning / auth-request;工具调用一行自带 toolInput 与 toolResult;不落 thinking。
/// 会话目录里的路径在盘上写成 `{{SESSION_PATH}}` 占位,读时展开。整文件重写
/// (写 .tmp → **先 unlink 正本** → rename)。会话 id(`YYMMDD-形容词-名词`)只在工作区
/// 内唯一,native id 因此带工作区命名空间:`<工作区 config.json 的 id>/<会话目录>`
/// (见 workspace_ns)。
///
/// 引擎的原料不读:Pi 的写在会话目录里的 `.pi-sessions/`(Wake 的 Pi adapter 不扫那里);
/// Claude 的那份落在 `~/.claude/projects/<sdkCwd>/<sdkSessionId>.jsonl`、被 Claude
/// adapter 收成一条 Claude Code 会话——由 `claimed_sessions` 认领掉,同一段对话只列一次
pub struct CraftAdapter {
    /// 工作区集合(默认 `~/.craft-agent/workspaces`)或单个工作区
    root: PathBuf,
    heads: HeadCache,
    workspaces: WorkspaceIds,
}

impl CraftAdapter {
    pub fn new() -> Self {
        Self::at(
            super::home_dir()
                .unwrap_or_default()
                .join(".craft-agent")
                .join("workspaces"),
        )
    }

    fn at(root: PathBuf) -> Self {
        Self {
            root,
            heads: HeadCache::default(),
            workspaces: WorkspaceIds::default(),
        }
    }

    /// 根下每个会话目录的 session.jsonl 路径(含隐藏会话——列表过滤在 file_ref,认领要看
    /// 全部;文件在不在交给调用方那一次 stat),顺带把首行缓存收成只剩这些路径。根是工作区
    /// 集合时会话在 `<根>/<工作区>/sessions/*`,是单个工作区时在 `<根>/sessions/*`,两种
    /// 形态都走一遍:错位的那一层没有 sessions/,NotFound 自然跳过。第二项为 false = 有目录
    /// 在却读不出来:枚举照用读到的那些,父子关系与认领的快照则不能当完整的交出去
    fn session_files(&self) -> (Vec<PathBuf>, bool) {
        let mut complete = true;
        let mut subdirs = |dir: &Path| -> Vec<PathBuf> {
            match fs::read_dir(dir) {
                Ok(entries) => entries
                    .flatten()
                    .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                    .map(|e| e.path())
                    .collect(),
                Err(e) => {
                    complete &= matches!(
                        e.kind(),
                        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                    );
                    Vec::new()
                }
            }
        };
        let mut workspaces = vec![self.root.clone()];
        workspaces.extend(subdirs(&self.root));
        let mut files: Vec<PathBuf> = workspaces
            .iter()
            .flat_map(|workspace| subdirs(&workspace.join("sessions")))
            .map(|session| session.join(SESSION_FILE))
            .collect();
        files.sort();
        self.heads.retain_only(&files);
        (files, complete)
    }

    /// 全部会话的首行摘要(父子关系与认领用)。**`None` = 这一刻有东西读不出来、又没有
    /// 上次读到的可顶**:拿缺一块的快照去对账,那块的认领会被撤掉、父子关系会被当成解除。
    /// 首行本身写坏了(craft 自己也列不出这种会话)才算没有
    fn all_heads(&self) -> Option<Vec<(PathBuf, Head)>> {
        let (files, complete) = self.session_files();
        if !complete {
            return None;
        }
        let mut heads = Vec::with_capacity(files.len());
        for path in files {
            let head = fs::metadata(&path)
                .and_then(|meta| self.heads.get(&path, mtime_ms(&meta), meta.len() as i64));
            match head {
                Ok(Some(head)) => heads.push((path, head)),
                Ok(None) => {}
                // 读不出来(含 craft 原子写"先删正本、再把 .tmp 改名过来"的那个空档)沿用上次
                // 读到的;没读过的,文件不在就当它还没有,别的错误才是真不知道
                Err(e) => match self.heads.last(&path) {
                    Some(head) => heads.push((path, head)),
                    None if e.kind() == io::ErrorKind::NotFound => {}
                    None => return None,
                },
            }
        }
        Some(heads)
    }

    /// 工作区在会话 key 里的命名空间:它自己 config.json 里的 `id`(craft 建工作区时随机
    /// 生成,改名、挪位置都不变;整个工作区备份一份,备份里的会话仍算同一条)。不用目录名:
    /// 会话 id 只在工作区内唯一,两个同名文件夹的工作区同一天生成同一个 id 就撞 key,被当成
    /// 副本吞掉一条。没有 config.json(craft 自己都不认它是工作区)或里面没有 id,退回目录名;
    /// config.json 在却读不出来,沿用上次读到的,没读过给 None——别拿目录名顶,key 一换就是
    /// 另一条会话(craft 写 config.json 是 tmp + rename,正常不会读到半截)
    fn workspace_ns(&self, workspace_dir: &Path) -> Option<String> {
        let dir_name = || Some(workspace_dir.file_name()?.to_string_lossy().to_string());
        let config = workspace_dir.join("config.json");
        let mut cache = self.workspaces.0.lock().unwrap();
        let last = cache.get(workspace_dir).cloned();
        let stamp = match fs::metadata(&config) {
            Ok(meta) => (mtime_ms(&meta), meta.len() as i64),
            Err(e) if e.kind() == io::ErrorKind::NotFound => return dir_name(),
            Err(_) => return last.map(|(_, ns)| ns),
        };
        if let Some((_, ns)) = last.as_ref().filter(|(cached, _)| *cached == stamp) {
            return Some(ns.clone());
        }
        let Some(config) = fs::read_to_string(&config)
            .ok()
            .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        else {
            return last.map(|(_, ns)| ns);
        };
        let ns = str_at(&config, "id")
            .map(str::to_string)
            .or_else(dir_name)?;
        cache.insert(workspace_dir.to_path_buf(), (stamp, ns.clone()));
        Some(ns)
    }

    /// 会话文件 → native id;不是 `<工作区>/sessions/<会话>/session.jsonl` 形状、或工作区 id
    /// 这一刻读不出来,给 None
    fn native_id_of(&self, session_file: &Path) -> Option<String> {
        let session_dir = session_file.parent()?;
        let sessions_dir = session_dir.parent()?;
        if sessions_dir.file_name()? != "sessions" {
            return None;
        }
        let ns = self.workspace_ns(sessions_dir.parent()?)?;
        Some(native_id(&ns, &session_dir.file_name()?.to_string_lossy()))
    }
}

/// 入库前的自定义根归一化(`adapters::normalize_custom_root` 按 agent 分派到这里):
/// 目录选择器里选中的是某个工作区的 `sessions/` 就上提到工作区——那一层下面的
/// `<会话>/session.jsonl` 两种枚举形态都够不着。自己底下还有 `sessions/` 的,说明
/// 它本身就是个叫 sessions 的工作区,不动
pub fn normalize_custom_root(dir: PathBuf) -> PathBuf {
    let is_sessions_dir =
        dir.file_name().is_some_and(|n| n == "sessions") && !dir.join("sessions").is_dir();
    match dir.parent() {
        Some(workspace) if is_sessions_dir => workspace.to_path_buf(),
        _ => dir,
    }
}

const SESSION_FILE: &str = "session.jsonl";
/// 盘上写成占位的会话目录(craft `makeSessionPathPortable`),读时换回真实路径
const SESSION_PATH_TOKEN: &str = "{{SESSION_PATH}}";
/// Claude 后端的回合锚点边车:Craft 的助手消息 id → 它所在的引擎会话
const CLAUDE_ANCHORS: &str = "meta/claude-turn-anchors.json";

/// native id = `<工作区命名空间>/<会话目录>`(见 workspace_ns)。`:` 是会话 key 的分段符、
/// `/` 分开两段,出现在各段里就换掉。会话自己与 parent_links 里的父会话都经这里拼——两端
/// 差一个字符父子关系就静默对不上
fn native_id(ns: &str, session: &str) -> String {
    format!(
        "{}/{}",
        ns.replace([':', '/'], "_"),
        session.replace([':', '/'], "_")
    )
}

fn workspace_dir_of(session_file: &Path) -> Option<&Path> {
    session_file.parent()?.parent()?.parent()
}

fn session_key_of(native: &str) -> String {
    session_key(AgentId::CraftAgents, "", native)
}

/// 首行里枚举、父子关系与认领要用的几项(缓存单位)
#[derive(Clone)]
struct Head {
    /// 不进列表:mini 编辑会话(hidden)与看板上还没领养的任务草稿(taskDraft)
    hidden: bool,
    /// 同工作区的父会话目录名:spawn_session 派出的子任务(parentSessionId),
    /// 或分支的来源(branchFromSessionPath 的末段)
    parent: Option<String>,
    /// 这条会话在 Claude 引擎那边的转录 id(Pi 后端的会话没有)
    claude_ids: Vec<String>,
}

fn str_at<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn true_at(v: &Value, key: &str) -> bool {
    v.get(key).and_then(Value::as_bool) == Some(true)
}

/// 首行 header。消息行都带 `type`,header 没有——拿它区分。`Ok(None)` = 首行不是 header
/// (写坏了,craft 自己也列不出这种会话);打不开、读出错是 `Err`——那是"这一刻不知道"
fn read_header(path: &Path) -> io::Result<Option<Value>> {
    let mut line = String::new();
    BufReader::with_capacity(16 << 10, fs::File::open(path)?).read_line(&mut line)?;
    Ok(parse_header(&line))
}

fn parse_header(line: &str) -> Option<Value> {
    let v: Value = serde_json::from_str(line.trim_end()).ok()?;
    (v.is_object() && v.get("type").is_none()).then_some(v)
}

fn is_pi_model(header: &Value) -> bool {
    str_at(header, "model").is_some_and(|m| m.starts_with("pi/"))
}

fn head_of(path: &Path, header: &Value) -> io::Result<Head> {
    let parent = str_at(header, "parentSessionId")
        .map(str::to_string)
        .or_else(|| {
            str_at(header, "branchFromSessionPath").and_then(|p| {
                // 绝对路径(craft 不把它转成 ~ 形态);两种分隔符都认
                p.rsplit(['/', '\\'])
                    .find(|s| !s.is_empty())
                    .map(str::to_string)
            })
        });
    let mut claude_ids = Vec::new();
    // Pi 后端的 sdkSessionId 是 Pi 自己的会话 id,认领它只会往表里塞永远对不上的 key
    if !is_pi_model(header) {
        claude_ids.extend(str_at(header, "sdkSessionId").map(str::to_string));
        // 回合锚点记着每条助手消息实际落在哪份引擎转录里:续跑或恢复换过 SDK 会话时,
        // 旧的那份照样装着这条会话的对话。没有边车、边车写坏了就只认 header 里当前那份;
        // 读出错往上报(不知道)。与 header 重复的 id 在 claimed_sessions 里去重
        let anchors = match path
            .parent()
            .map(|dir| fs::read_to_string(dir.join(CLAUDE_ANCHORS)))
        {
            Some(Ok(raw)) => serde_json::from_str::<Value>(&raw).ok(),
            Some(Err(e)) if e.kind() != io::ErrorKind::NotFound => return Err(e),
            _ => None,
        };
        claude_ids.extend(
            anchors
                .iter()
                .filter_map(|a| a.get("anchors")?.as_object())
                .flat_map(|m| m.values())
                .filter_map(|record| str_at(record, "sdkSessionId"))
                .map(str::to_string),
        );
    }
    Ok(Head {
        hidden: true_at(header, "hidden") || true_at(header, "taskDraft"),
        parent,
        claude_ids,
    })
}

/// 首行摘要按 (session.jsonl 的 mtime、size, 锚点边车的 mtime) 缓存:每轮扫描、每批
/// watcher 事件都要把全部会话的首行过一遍(列表过滤、父子关系、认领),文件没变
/// 就不再打开
#[derive(Default)]
struct HeadCache(Mutex<HashMap<PathBuf, ((i64, i64, i64), Head)>>);

impl HeadCache {
    /// `mtime` / `size` 是调用方已经 stat 过的 session.jsonl。`Ok(None)` = 首行不是 header
    fn get(&self, path: &Path, mtime: i64, size: i64) -> io::Result<Option<Head>> {
        let anchors_mtime = path
            .parent()
            .and_then(|dir| fs::metadata(dir.join(CLAUDE_ANCHORS)).ok())
            .map(|m| mtime_ms(&m))
            .unwrap_or(0);
        let stamp = (mtime, size, anchors_mtime);
        if let Some((cached, head)) = self.0.lock().unwrap().get(path) {
            if *cached == stamp {
                return Ok(Some(head.clone()));
            }
        }
        let Some(header) = read_header(path)? else {
            return Ok(None);
        };
        let head = head_of(path, &header)?;
        self.0
            .lock()
            .unwrap()
            .insert(path.to_path_buf(), (stamp, head.clone()));
        Ok(Some(head))
    }

    /// 上次读到的(不管文件后来变没变):这一刻读不出来时顶上
    fn last(&self, path: &Path) -> Option<Head> {
        self.0
            .lock()
            .unwrap()
            .get(path)
            .map(|(_, head)| head.clone())
    }

    /// 缓存只留这次枚举到的会话文件(删掉的会话不再占着)
    fn retain_only(&self, files: &[PathBuf]) {
        let live: std::collections::HashSet<&Path> = files.iter().map(PathBuf::as_path).collect();
        self.0
            .lock()
            .unwrap()
            .retain(|p, _| live.contains(p.as_path()));
    }
}

/// 工作区命名空间(workspace_ns)按工作区目录缓存,戳是它 config.json 的 (mtime, size):
/// 每个会话的 key 都要问一次,工作区只有几个
#[derive(Default)]
struct WorkspaceIds(Mutex<HashMap<PathBuf, ((i64, i64), String)>>);

/// JSON 值里所有字符串中的 `{{SESSION_PATH}}` 换回会话目录。解析之后再换:
/// Windows 路径里的反斜杠直接塞进 JSON 文本会破坏转义
fn expand_session_path(v: &mut Value, dir: &str) {
    match v {
        Value::String(s) if s.contains(SESSION_PATH_TOKEN) => {
            *s = s.replace(SESSION_PATH_TOKEN, dir);
        }
        Value::Array(items) => items.iter_mut().for_each(|i| expand_session_path(i, dir)),
        Value::Object(map) => map.values_mut().for_each(|i| expand_session_path(i, dir)),
        _ => {}
    }
}

struct CraftParse {
    header: Value,
    messages: Vec<TranscriptMessage>,
    last_ts: i64,
    unknown_lines: u32,
}

/// 用户消息正文:原话 + 附件名(与 craft 递给模型的 `[Attached file: …]` 同一写法)
fn user_text(row: &Value) -> String {
    let mut text = str_at(row, "content").unwrap_or_default().to_string();
    for name in row
        .get("attachments")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|a| str_at(a, "name"))
    {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&format!("[Attached file: {name}]"));
    }
    text
}

fn tool_view(row: &Value) -> ToolCallView {
    let id = str_at(row, "toolUseId")
        .or_else(|| str_at(row, "id"))
        .unwrap_or_default()
        .to_string();
    let input = row.get("toolInput").cloned().unwrap_or(Value::Null);
    let output = str_at(row, "toolResult").map(str::to_string);
    // craft 对没显式标错、但以 [ERROR] / Error: 开头的结果也记成 error 状态
    let is_error = true_at(row, "isError") || str_at(row, "toolStatus") == Some("error");
    tool_call_view(
        id,
        str_at(row, "toolName").unwrap_or_default(),
        &input,
        output,
        is_error,
    )
}

fn parse_file(path: &Path) -> Result<CraftParse> {
    let raw = fs::read_to_string(path)
        .with_context(|| format!("read craft session {}", path.display()))?;
    let dir = path
        .parent()
        .map(|d| d.to_string_lossy().to_string())
        .unwrap_or_default();
    let mut lines = raw.lines();
    let mut header = lines
        .next()
        .and_then(parse_header)
        .context("craft session.jsonl has no session header")?;
    expand_session_path(&mut header, &dir);
    let branch_point = str_at(&header, "branchFromMessageId").map(str::to_string);

    let mut messages: Vec<TranscriptMessage> = Vec::new();
    // 分支会话自己的消息从这个下标开始;之前的是从父会话复制来的(折叠用)
    let mut own_from: Option<usize> = None;
    let mut last_ts = 0i64;
    let mut unknown_lines = 0u32;
    for line in lines {
        if line.trim().is_empty() {
            continue;
        }
        // 崩溃时写断的行 craft 自己也是跳过(parseMessagesResilient)
        let Ok(mut row) = serde_json::from_str::<Value>(line) else {
            unknown_lines += 1;
            continue;
        };
        expand_session_path(&mut row, &dir);
        let ts = row.get("timestamp").map(to_epoch_ms).unwrap_or(0);
        last_ts = last_ts.max(ts);
        // 子代理(Task 工具)内部的话与工具调用挂着 parentToolUseId:不进主线,
        // 子代理的结论在 Task 工具自己的结果里
        let nested = str_at(&row, "parentToolUseId").is_some();
        match row.get("type").and_then(Value::as_str).unwrap_or_default() {
            "user" => {
                let text = user_text(&row);
                if !text.trim().is_empty() {
                    let mut m = text_msg(Role::User, &text, ts);
                    // 系统生成、只给模型看的推动消息(后台任务完成的提醒一类)
                    if true_at(&row, "hidden") {
                        m.kind = MessageKind::Meta;
                    }
                    messages.push(m);
                }
            }
            "assistant" if !nested => {
                if let Some(text) = str_at(&row, "content") {
                    messages.push(text_msg(Role::Assistant, text, ts));
                }
            }
            // 计划(Explore 模式下 SubmitPlan 交上来的整份 Markdown)是助手的产出
            "plan" => {
                if let Some(text) = str_at(&row, "content") {
                    messages.push(text_msg(Role::Assistant, text, ts));
                }
            }
            "tool" if !nested => {
                // 挂到本段最近的助手消息上(craft 先落助手的过渡话、再落它发起的工具);
                // 前面没有可挂的(工具先行,或上一条是分叉点之前复制来的)就起一条空的承载
                let host = messages.len().checked_sub(1).filter(|&last| {
                    messages[last].role == Role::Assistant
                        && messages[last].kind == MessageKind::Text
                        && own_from.is_none_or(|from| last >= from)
                });
                let ix = match host {
                    Some(ix) => ix,
                    None => {
                        messages.push(text_msg(Role::Assistant, "", ts));
                        messages.len() - 1
                    }
                };
                messages[ix].tool_calls.push(tool_view(&row));
            }
            "assistant" | "tool" => {}
            // 压缩完成、授权请求、报错与警告:会话里的系统事件
            "info" | "error" | "warning" | "auth-request" => {
                let text = str_at(&row, "content").or_else(|| str_at(&row, "errorTitle"));
                if let Some(text) = text {
                    messages.push(meta_msg(text, ts));
                }
            }
            // 进度状态 craft 本就不落盘(persist 前滤掉),见到也不算漂移
            "status" => {}
            _ => unknown_lines += 1,
        }
        if own_from.is_none()
            && branch_point.is_some()
            && str_at(&row, "id") == branch_point.as_deref()
        {
            own_from = Some(messages.len());
        }
    }

    // 分支 = 父会话到分叉点为止的消息原样复制进来 + 自己的续写(craft createSession
    // 的 branch copy),复制段折成一条标记
    if let Some(cut) = own_from {
        collapse_inherited(&mut messages, cut, "session");
    }
    assign_seq(&mut messages);
    Ok(CraftParse {
        header,
        messages,
        last_ts,
        unknown_lines,
    })
}

fn epoch_at(v: &Value, key: &str) -> i64 {
    v.get(key).map(to_epoch_ms).unwrap_or(0)
}

fn build_meta(r: &SessionFileRef, p: &CraftParse) -> SessionMeta {
    let path = Path::new(&r.file_path);
    let h = &p.header;
    // 标题:craft 的 name(AI 起的或用户改的)> 首条真实提问 > craft 记的预览。分支的
    // 预览是父会话的首条提问,不拿它当自己的标题
    let title = str_at(h, "name")
        .map(clean_title_candidate)
        .filter(|t| !t.is_empty())
        .or_else(|| title_from_messages(&p.messages))
        .or_else(|| {
            str_at(h, "preview")
                .filter(|_| str_at(h, "branchFromMessageId").is_none())
                .map(clean_title_candidate)
                .filter(|t| !t.is_empty())
        })
        .unwrap_or_else(|| UNTITLED.to_string());
    // 项目:用户给会话设了工作目录就是它;没设(craft 的默认)就归到工作区本身——取首行记的
    // 工作区路径:远程镜像里文件所在的是本机缓存目录,拿它当项目,按远端路径就筛不到。首行
    // 的路径是 craft 的 ~ 形态(toPortablePath,workingDirectory 同样),按本机家目录展开——
    // 远程镜像的 ~ 本该是远端家目录,那边探测不到,与其他远程路径假设同一局限;没记才退回文件位置
    let project_path = str_at(h, "workingDirectory")
        .or_else(|| str_at(h, "workspaceRootPath"))
        .map(super::expand_tilde)
        .or_else(|| workspace_dir_of(path).map(|w| w.to_string_lossy().to_string()))
        .unwrap_or_default();
    let created = epoch_at(h, "createdAt");
    // lastUsedAt 连"点开看一眼"都会刷新,不是活动时间。lastMessageAt 实测只在用户发言时
    // 前移(助手几秒后的回复不算),与消息自己的时间戳取大者
    let updated = [epoch_at(h, "lastMessageAt").max(p.last_ts), created]
        .into_iter()
        .find(|t| *t > 0)
        .unwrap_or(r.mtime_ms);
    let usage = h.get("tokenUsage").unwrap_or(&Value::Null);
    let count = |key: &str| usage.get(key).and_then(Value::as_f64).unwrap_or(0.0) as i64;
    let tokens = match count("totalTokens") {
        0 => count("inputTokens") + count("outputTokens"),
        total => total,
    };
    SessionMeta {
        key: session_key_of(&r.native_id),
        id: r.native_id.clone(),
        host: String::new(),
        agent: AgentId::CraftAgents,
        title,
        project_name: project_name_of(&project_path),
        project_path,
        file_path: r.file_path.clone(),
        created_at: if created > 0 { created } else { r.mtime_ms },
        updated_at: updated,
        message_count: p
            .messages
            .iter()
            .filter(|m| m.kind == MessageKind::Text)
            .count() as i64,
        size_bytes: r.size,
        git_branch: None,
        // Pi 后端的模型写成 `pi/<id>`
        model: str_at(h, "model").map(|m| m.strip_prefix("pi/").unwrap_or(m).to_string()),
        tokens_used: (tokens > 0).then_some(tokens),
        archived: true_at(h, "isArchived"),
        // 自动化(定时 / 标签变化触发的 prompt)建出来的会话
        source: h
            .get("triggeredBy")
            .is_some_and(|t| t.is_object())
            .then(|| "automation".to_string()),
        favorite: false,
        pinned: false,
    }
}

impl AgentAdapter for CraftAdapter {
    fn agent(&self) -> AgentId {
        AgentId::CraftAgents
    }

    fn list_session_files(&self) -> Result<Vec<SessionFileRef>> {
        let (files, _) = self.session_files();
        Ok(files.iter().filter_map(|f| self.file_ref(f)).collect())
    }

    fn file_ref(&self, path: &Path) -> Option<SessionFileRef> {
        // 只认会话主文件:.tmp(原子写的中间态)、.pi-sessions/ 里 Pi 引擎的原料都不是
        if path.file_name()? != SESSION_FILE {
            return None;
        }
        let mut r = default_file_ref(AgentId::CraftAgents, path)?;
        r.native_id = self.native_id_of(path)?;
        let head = match self.heads.get(path, r.mtime_ms, r.size) {
            // None = 首行写坏了,craft 自己也列不出
            Ok(head) => head?,
            // 这一刻读不出来:按上次读到的算,没读过就先不列
            Err(_) => self.heads.last(path)?,
        };
        (!head.hidden).then_some(r)
    }

    fn parse_session(&self, r: &SessionFileRef) -> Result<ParsedSession> {
        let parsed = parse_file(Path::new(&r.file_path))?;
        Ok(ParsedSession::derive(
            build_meta(r, &parsed),
            &parsed.messages,
            parsed.unknown_lines,
        ))
    }

    fn parse_transcript(&self, r: &SessionFileRef) -> Result<ParsedTranscript> {
        let parsed = parse_file(Path::new(&r.file_path))?;
        Ok(ParsedTranscript {
            meta: build_meta(r, &parsed),
            mainline: parsed.messages,
            sidechains: Vec::new(),
            unknown_line_count: parsed.unknown_lines,
        })
    }

    fn session_paths(&self, meta: &SessionMeta) -> Vec<String> {
        // 一目录一会话:附件、计划、下载、引擎原料都在会话目录里,整个 trash
        // (craft 自己删会话也是删整个目录)
        match Path::new(&meta.file_path).parent() {
            Some(dir) => vec![dir.to_string_lossy().to_string()],
            None => vec![meta.file_path.clone()],
        }
    }

    fn cleanup_paths(&self, meta: &SessionMeta) -> Option<Vec<String>> {
        Some(self.session_paths(meta))
    }

    fn manages_parent_links(&self) -> bool {
        true
    }

    /// 子任务(spawn_session / 看板)与分支都挂到同工作区的父会话下。任一会话的首行或
    /// 工作区 id 这一刻读不出来,整份交回 None(不知道),别让缺的那几条被当成关系解除
    fn parent_links(&self) -> Option<Vec<(String, String)>> {
        let mut links = Vec::new();
        for (path, head) in self.all_heads()? {
            let Some(parent) = head.parent.filter(|_| !head.hidden) else {
                continue;
            };
            let ns = self.workspace_ns(workspace_dir_of(&path)?)?;
            let child = native_id(&ns, &path.parent()?.file_name()?.to_string_lossy());
            let parent = native_id(&ns, &parent);
            if parent != child {
                links.push((session_key_of(&child), session_key_of(&parent)));
            }
        }
        Some(links)
    }

    /// 关系写在子会话自己的首行里(不是 Grok 那种长在父会话 location 里的边车):
    /// 多 location 下 scanner 按子会话的胜出文件认边
    fn parent_links_in_child(&self) -> bool {
        true
    }

    /// 首行(隐藏会话的也算——它们 file_ref 不收)与回合锚点边车决定父子关系与认领
    fn is_snapshot_event(&self, path: &Path) -> bool {
        path.file_name().is_some_and(|n| n == SESSION_FILE) || path.ends_with(CLAUDE_ANCHORS)
    }

    fn manages_claims(&self) -> bool {
        true
    }

    /// Claude 后端的会话在 `~/.claude/projects` 里各有一份引擎转录。隐藏的 mini 编辑
    /// 会话也认领:它们不进列表,引擎那份同样不该以 Claude Code 会话冒出来。有首行读不
    /// 出来就交回 None,scanner 保留库里的认领
    fn claimed_sessions(&self) -> Option<Vec<(AgentId, String)>> {
        let mut ids: Vec<String> = self
            .all_heads()?
            .into_iter()
            .flat_map(|(_, head)| head.claude_ids)
            .collect();
        ids.sort();
        ids.dedup();
        Some(
            ids.into_iter()
                .map(|id| (AgentId::ClaudeCode, id))
                .collect(),
        )
    }

    fn with_custom_root(&self, dir: PathBuf) -> Box<dyn AgentAdapter> {
        // 选中的是 `.craft-agent` 本身就下到它的 workspaces/;工作区集合与单个工作区
        // 两种形态 session_files 都认
        let root = if dir.join("workspaces").is_dir() {
            dir.join("workspaces")
        } else {
            dir
        };
        Box::new(Self::at(root))
    }

    fn data_roots(&self) -> Vec<PathBuf> {
        vec![self.root.clone()]
    }
}
