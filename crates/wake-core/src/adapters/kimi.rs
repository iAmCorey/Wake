use super::parse_utils::*;
use super::AgentAdapter;
use crate::models::*;
use anyhow::Result;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, BufReader};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Kimi Code(Moonshot AI):`<home>/sessions/wd_<名>_<hash>/<会话>/`,`<home>` 是
/// `~/.kimi-code`(或 `KIMI_CODE_HOME`)。一目录一会话,主文件 `agents/main/wire.jsonl`
/// 是事件溯源的记录流,按 Kimi 自己重建转录的方式(agent-core 的 `reduceWireRecords`)读:
/// - 用户输入是 `turn.prompt` / `turn.steer`,按 origin 分用户自己给的与系统产生的;同一份
///   输入随后以 `context.append_message`(role user)进上下文,那是回声、跳过——没有 turn
///   记录的(从旧版 Python kimi-cli 迁移来的会话)才由它出用户消息;
/// - 助手:CLI 按步写在 `context.append_loop_event`(step.begin / content.part 的 text 与
///   think / tool.call / tool.result / step.end),桌面端 0.4x 写在 `agent.message.appended`
///   (包装层 `{message, meta}`);同一轮的各步并成一条回复,工具结果回填到调用上;
/// - `context.apply_compaction` 是上下文压缩的摘要,`forked` 之前是从父会话复制来的。
///
/// `state.json` 边车给标题 / 时间 / 归档 / 分叉来源("New Session" 是占位标题),cwd 靠
/// home 下 `session_index.jsonl` 的 sessionId→workDir(目录名 hash 不可反推)。
/// `agents/<非main>/` 是子代理,不进列表。
pub struct KimiAdapter {
    /// 会话根:`<home>/sessions`(下面两级 `wd_*/<会话>`),或单个工作目录桶 `wd_*`(会话就在
    /// 它下面一级)
    root: PathBuf,
    /// home 顶层的 `session_index.jsonl`(sessions 的上一级),由根推出来
    index_path: PathBuf,
    /// sessionId → workDir,按 index mtime 缓存(全量刷新时逐会话调用)
    cwd_cache: MtimeCache<Arc<HashMap<String, String>>>,
    states: StateCache,
}

const INDEX_FILE: &str = "session_index.jsonl";

impl KimiAdapter {
    pub fn new() -> Self {
        Self::from_root(home_from(super::env_dir("KIMI_CODE_HOME")).join("sessions"))
    }

    fn from_root(root: PathBuf) -> Self {
        let levels = if is_workdir_bucket(&root) { 2 } else { 1 };
        let index_path = root
            .ancestors()
            .nth(levels)
            .unwrap_or(&root)
            .join(INDEX_FILE);
        Self {
            root,
            index_path,
            cwd_cache: MtimeCache::new(),
            states: StateCache::default(),
        }
    }

    /// 全部会话目录(`<桶>/<会话>`;根就是一个桶时只看它),列表与父子关系共用;读不出的目录跳过
    fn session_dirs(&self) -> Vec<PathBuf> {
        let children = |dir: &Path| -> Vec<PathBuf> {
            fs::read_dir(dir)
                .map(|entries| entries.flatten().map(|e| e.path()).collect())
                .unwrap_or_default()
        };
        let buckets = if is_workdir_bucket(&self.root) {
            vec![self.root.clone()]
        } else {
            children(&self.root)
        };
        buckets
            .iter()
            .flat_map(|bucket| children(bucket))
            .filter(|dir| is_session_dir(dir))
            .collect()
    }

    fn cwd_map(&self) -> Arc<HashMap<String, String>> {
        let mtime = fs::metadata(&self.index_path)
            .map(|m| mtime_ms(&m))
            .unwrap_or(0);
        self.cwd_cache
            .get_or_try_build(mtime, || {
                let mut out = HashMap::new();
                if let Ok(raw) = fs::read_to_string(&self.index_path) {
                    for line in raw.lines() {
                        let Ok(v) = serde_json::from_str::<Value>(line) else {
                            continue;
                        };
                        if let (Some(id), Some(wd)) = (
                            v.get("sessionId").and_then(|x| x.as_str()),
                            v.get("workDir").and_then(|x| x.as_str()),
                        ) {
                            out.insert(id.to_string(), wd.to_string());
                        }
                    }
                }
                Some(Arc::new(out))
            })
            .unwrap_or_default()
    }

    fn cwd_for(&self, native_id: &str) -> String {
        self.cwd_map().get(native_id).cloned().unwrap_or_default()
    }

    fn parse(
        &self,
        r: &SessionFileRef,
        decode_images: bool,
    ) -> Result<(SessionMeta, Vec<TranscriptMessage>, u32)> {
        let path = Path::new(&r.file_path);
        // state.json 在却读不出(又没读到过)就让这次解析失败,先读它、省得白解析整份转录:
        // 按默认值入库,标题、归档都是错的,而引用里记着它此刻的 mtime / size,文件不再变就
        // 一直错着
        let state = match session_dir_of(path) {
            Some(dir) => self.states.get(dir)?,
            None => KimiState::default(),
        };
        let (messages, unknown) = parse_kimi_wire(path, decode_images)?;
        let meta = build_meta(r, &state, &self.cwd_for(&r.native_id), &messages);
        Ok((meta, messages, unknown))
    }
}

/// Kimi 的数据目录:`KIMI_CODE_HOME`(Kimi 自己的开关)探得到会话目录才采信,否则 `~/.kimi-code`
fn home_from(env: Option<PathBuf>) -> PathBuf {
    env.filter(|dir| dir.join("sessions").is_dir())
        .unwrap_or_else(|| super::home_dir().unwrap_or_default().join(".kimi-code"))
}

/// 主代理转录相对会话目录的位置。列表、file_ref 与会话目录反推共用这一处,按平台分隔符拼
/// (写死 `/` 的字符串比较在 Windows 上一条都对不上,PR #61)
const MAIN_WIRE: [&str; 3] = ["agents", "main", "wire.jsonl"];

fn main_wire_rel() -> PathBuf {
    MAIN_WIRE.iter().collect()
}

/// 会话目录名的前缀:Kimi Code 建的是 `session_<uuid>`,从旧版 Python kimi-cli 迁移来的是
/// `ses_<旧 uuid>`
const SESSION_DIR_PREFIXES: [&str; 2] = ["session_", "ses_"];

/// 会话目录:名字带会话前缀
fn is_session_dir(dir: &Path) -> bool {
    dir.file_name()
        .map(|name| name.to_string_lossy())
        .is_some_and(|name| SESSION_DIR_PREFIXES.iter().any(|p| name.starts_with(p)))
}

/// 工作目录桶:`wd_<名>_<hash>`
fn is_workdir_bucket(dir: &Path) -> bool {
    dir.file_name()
        .is_some_and(|name| name.to_string_lossy().starts_with("wd_"))
}

/// `…/<会话>/agents/main/wire.jsonl` → 会话目录;子代理(`agents/<非main>/`)与别的形状给
/// None。state.json、file_ref 与 session_paths 共用,漂移会 trash 错目录
fn session_dir_of(wire_path: &Path) -> Option<&Path> {
    if !wire_path.ends_with(main_wire_rel()) {
        return None;
    }
    wire_path
        .ancestors()
        .nth(MAIN_WIRE.len())
        .filter(|dir| is_session_dir(dir))
}

/// 会话目录下的边车:标题 / 时间 / 归档 / 分叉来源
const STATE_FILE: &str = "state.json";

#[derive(Default, Clone)]
struct KimiState {
    title: String,
    created_ms: i64,
    updated_ms: i64,
    /// 在 Kimi 里归档了(顶层 `archived: true`)
    archived: bool,
    /// 从哪个会话分叉来的(`forkedFrom`)
    forked_from: Option<String>,
}

/// 读到的 state.json → 字段;不是完整的 JSON 对象给 None。标题照 Kimi 的 `titleFromState`:
/// 新的写法是 `title` 配一个 `isCustomTitle`,老的把改过的名字放在 `customTitle`
fn parse_state(raw: &[u8]) -> Option<KimiState> {
    let v = serde_json::from_slice::<Value>(raw)
        .ok()
        .filter(Value::is_object)?;
    let string = |key: &str| v.get(key).filter(|value| value.is_string());
    let title = string("title")
        .filter(|_| v.get("isCustomTitle").is_some_and(Value::is_boolean))
        .or_else(|| string("customTitle"))
        .or_else(|| string("title"));
    let time = |key: &str| v.get(key).and_then(Value::as_str).map(iso_ms).unwrap_or(0);
    Some(KimiState {
        // "New Session" 是 Kimi 的占位标题,不当真实标题
        title: optional_string(title)
            .filter(|t| t != "New Session")
            .unwrap_or_default(),
        created_ms: time("createdAt"),
        updated_ms: time("updatedAt"),
        archived: v.get("archived").and_then(Value::as_bool) == Some(true),
        forked_from: optional_string(v.get("forkedFrom")),
    })
}

/// 会话目录 → 它的 state.json,按 (mtime, size) 缓存:父子关系的快照每批监听事件都要把全部
/// 会话过一遍,文件没变就不再打开。另一个用处是兜底——Kimi 用普通 writeFile 写它(先截断再
/// 写,不是原子替换),这一刻可能读到空的或半截的,那就沿用上次读到的
#[derive(Default)]
struct StateCache(Mutex<HashMap<PathBuf, ((i64, i64), KimiState)>>);

impl StateCache {
    fn get(&self, session_dir: &Path) -> io::Result<KimiState> {
        let path = session_dir.join(STATE_FILE);
        let last = self.0.lock().unwrap().get(session_dir).cloned();
        let read = fs::metadata(&path).and_then(|meta| {
            let stamp = (mtime_ms(&meta), meta.len() as i64);
            match &last {
                Some((cached, state)) if *cached == stamp => Ok(Some(state.clone())),
                _ => fs::read(&path).map(|raw| {
                    let state = parse_state(&raw)?;
                    self.0
                        .lock()
                        .unwrap()
                        .insert(session_dir.to_path_buf(), (stamp, state.clone()));
                    Some(state)
                }),
            }
        });
        let last = last.map(|(_, state)| state);
        match read {
            Ok(Some(state)) => Ok(state),
            // 没有这个文件:空的默认值
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(KimiState::default()),
            // 写坏了:沿用上次读到的,没读到过按没有算(Kimi 自己也这样)
            Ok(None) => Ok(last.unwrap_or_default()),
            // 读不出:沿用上次读到的,没读到过就是不知道
            Err(e) => last.ok_or(e),
        }
    }

    /// 只留这次枚举到的会话(删掉的会话不再占着)
    fn retain_only(&self, session_dirs: &[PathBuf]) {
        let live: HashSet<&Path> = session_dirs.iter().map(PathBuf::as_path).collect();
        self.0
            .lock()
            .unwrap()
            .retain(|dir, _| live.contains(dir.as_path()));
    }
}

/// 思考块:`{"type":"think","think":"..."}`,content 数组或单个 part 都认,多段拼成一段
/// (截断在合并处统一做)
fn think_texts(content: &Value) -> String {
    let parts = match content {
        Value::Array(parts) => parts.as_slice(),
        part @ Value::Object(_) => std::slice::from_ref(part),
        _ => &[],
    };
    parts
        .iter()
        .filter(|p| p.get("type").and_then(Value::as_str) == Some("think"))
        .filter_map(|p| p.get("think").and_then(Value::as_str).map(str::trim))
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// 记录类型词汇表,照抄 Kimi Code agent-core(0.29)的 `restoreAgentRecord` 与 v2 的
/// `defineOp`。这些不产生对话内容:配置、权限、用量与请求快照、模式切换、目标、定时与后台
/// 任务、压缩的起止……`context.clear` / `context.undo` 也在这里——Kimi 的转录同样留着 clear
/// 之前的消息,undo 撤掉的轮次 Wake 照留。上游加了新类型就在这里补:表外的计未知行,那是格式
/// 漂移的预警,清理页也靠它判断会话读全了没有。`turn.*` 按前缀认(见解析处)
const KNOWN_RECORDS: &[&str] = &[
    "metadata",
    "config.update",
    "context.clear",
    "context.undo",
    "context.update_token_count",
    "context_size.measured",
    "cron.add",
    "cron.cursor",
    "cron.delete",
    "full_compaction.begin",
    "full_compaction.cancel",
    "full_compaction.complete",
    "goal.clear",
    "goal.create",
    "goal.update",
    "llm.request",
    "llm.tools_snapshot",
    "mcp.tools_discovered",
    "micro_compaction.apply",
    "permission.record_approval_result",
    "permission.rules.add",
    "permission.set_mode",
    "plan_mode.cancel",
    "plan_mode.enter",
    "plan_mode.exit",
    "profile.bind",
    "skill.activate",
    "swarm_mode.enter",
    "swarm_mode.exit",
    "task.started",
    "task.terminated",
    "tools.register_user_tool",
    "tools.reset_active_tools",
    "tools.set_active_tools",
    "tools.unregister_user_tool",
    "tools.update_store",
    "usage.record",
];

/// 这条用户输入是不是用户自己给的,照抄 Kimi 的 `isUserVisibleTurnInputRecord`:user;用户敲
/// 斜杠触发的 skill / plugin;shell 命令的输入。后台任务通知、定时任务、钩子结果、注入、重试、
/// 压缩摘要、系统触发(目标续跑)都不是。没有 origin 的(老记录、迁移来的会话)算用户的;
/// 不认识的 kind 给 None,调用方计未知行
fn user_visible(origin: Option<&Value>) -> Option<bool> {
    let Some(origin) = origin.filter(|o| !o.is_null()) else {
        return Some(true);
    };
    let field = |key: &str| origin.get(key).and_then(Value::as_str);
    Some(match field("kind")? {
        "user" => true,
        "skill_activation" | "plugin_command" => field("trigger") == Some("user-slash"),
        "shell_command" => field("phase") == Some("input"),
        "background_task" | "compaction_summary" | "cron_job" | "cron_missed" | "hook_result"
        | "injection" | "retry" | "system_trigger" => false,
        _ => return None,
    })
}

/// 一遍读完 wire.jsonl 的状态
#[derive(Default)]
struct WireReader {
    decode_images: bool,
    messages: Vec<TranscriptMessage>,
    unknown: u32,
    /// 这一轮的助手回复还开着(就是最后一条消息),同一轮的各步并进去;推别的消息之前都要
    /// 关上它。用户消息、压缩摘要与新的一轮(turn.* 里除了 turn.step.*)关上它——输入渲染
    /// 不出来的那一轮没有用户消息隔开,也不能并进上一轮
    reply_open: bool,
    /// 已显示、回声(context.append_message 里的同一份输入)还没到的用户输入数
    pending_echo: u32,
    /// toolCallId → (消息下标, 调用下标),工具结果回填用
    tool_slots: HashMap<String, (usize, usize)>,
    /// 最后一个 `forked` 标记之前的消息数:那一段是从父会话复制来的
    inherited: usize,
}

impl WireReader {
    fn record(&mut self, row: &Value) {
        let ts = row.get("time").map(to_epoch_ms).unwrap_or(0);
        let kind = row.get("type").and_then(Value::as_str).unwrap_or_default();
        match kind {
            "turn.prompt" | "turn.steer" => {
                let visible = self.visible(row.get("origin"));
                let input = row.get("input").unwrap_or(&Value::Null);
                // 显示出来的输入稍后还会以 context.append_message 回声一次;渲染不出来的
                // (只带视频)不记,让回声顶上——那一份里视频已换成 <video> 标记
                if self.push_user(input, ts, visible) && visible {
                    self.pending_echo += 1;
                }
            }
            "context.append_message" => match row.get("message") {
                Some(message) => self.context_message(message, ts),
                None => self.unknown += 1,
            },
            "context.append_loop_event" => match row.get("event") {
                Some(event) => self.loop_event(event, ts),
                None => self.unknown += 1,
            },
            // 桌面端 0.4x:包装层 `{message, meta}`。user 与 turn.prompt / turn.steer 重复,
            // notify 是界面通知;不认识的角色计未知行
            "agent.message.appended" => {
                let Some(message) = row.get("message").and_then(|w| w.get("message")) else {
                    self.unknown += 1;
                    return;
                };
                match message.get("role").and_then(Value::as_str) {
                    Some("assistant" | "tool") => self.context_message(message, ts),
                    Some("user" | "notify") => {}
                    _ => self.unknown += 1,
                }
            }
            "context.apply_compaction" => {
                self.reply_open = false;
                let summary = row.get("summary").and_then(Value::as_str).unwrap_or("");
                if !summary.trim().is_empty() {
                    let mut message = text_msg(Role::System, summary, ts);
                    message.kind = MessageKind::CompactSummary;
                    self.messages.push(message);
                }
            }
            // 分叉:Kimi 把父会话的记录原样复制过来,再补这一行(多次分叉以最后一个为准)
            "forked" => {
                self.inherited = self.messages.len();
                self.reply_open = false;
                self.pending_echo = 0;
                self.tool_slots.clear();
            }
            k if k.starts_with("turn.") => self.reply_open &= k.starts_with("turn.step."),
            k if KNOWN_RECORDS.contains(&k) => {}
            _ => self.unknown += 1,
        }
    }

    fn visible(&mut self, origin: Option<&Value>) -> bool {
        user_visible(origin).unwrap_or_else(|| {
            self.unknown += 1;
            false
        })
    }

    /// 一条用户消息,渲染不出东西就不出(返回 false)。不是用户自己给的(系统通知、注入……)
    /// 记成 Meta:留在转录里,不进搜索、不算消息数、不当标题
    fn push_user(&mut self, content: &Value, ts: i64, visible: bool) -> bool {
        self.reply_open = false;
        let parsed = content_parts(content, self.decode_images);
        if parsed.text.is_empty() && parsed.images.is_empty() {
            return false;
        }
        let mut message = text_msg(Role::User, &parsed.text, ts);
        message.images = parsed.images;
        if !visible {
            message.kind = MessageKind::Meta;
        }
        self.messages.push(message);
        true
    }

    /// 这一轮的回复(没开着就开一条),返回下标
    fn reply(&mut self, ts: i64) -> usize {
        if !self.reply_open {
            self.messages.push(text_msg(Role::Assistant, "", ts));
            self.reply_open = true;
        }
        self.messages.len() - 1
    }

    /// 一段助手内容(正文、图片、think 块)并进这一轮的回复
    fn assistant_content(&mut self, content: &Value, ts: i64) {
        let parsed = content_parts(content, self.decode_images);
        let thinking = think_texts(content);
        if parsed.text.is_empty() && parsed.images.is_empty() && thinking.is_empty() {
            return;
        }
        self.reply(ts);
        merge_into_last_assistant(&mut self.messages, ts, parsed, &thinking);
    }

    fn tool_call(&mut self, id: &str, name: &str, args: &Value, ts: i64) {
        let ix = self.reply(ts);
        let calls = &mut self.messages[ix].tool_calls;
        calls.push(tool_call_view(id.to_string(), name, args, None, false));
        if !id.is_empty() {
            self.tool_slots
                .insert(id.to_string(), (ix, calls.len() - 1));
        }
    }

    /// 工具结果回填到调用上(输出是字符串或 content part 数组,与别家同一个读法)
    fn tool_result(&mut self, id: &str, output: &Value, is_error: bool) {
        let Some((m, c)) = self.tool_slots.remove(id) else {
            return;
        };
        let call = &mut self.messages[m].tool_calls[c];
        call.output = Some(clip(&tool_result_parts(output, false).text, MAX_TOOL_IO).0);
        call.is_error = is_error;
    }

    /// `context.append_message`:进上下文的完整消息(桌面端包装层里的同形)
    fn context_message(&mut self, message: &Value, ts: i64) {
        let content = message.get("content").unwrap_or(&Value::Null);
        match message.get("role").and_then(Value::as_str) {
            Some("user") => {
                // 系统产生的(注入、提醒、目标续跑……)不显示;用户自己的输入多半是 turn.prompt /
                // turn.steer 的回声,已经显示过了
                if !self.visible(message.get("origin")) {
                    return;
                }
                if self.pending_echo > 0 {
                    self.pending_echo -= 1;
                    return;
                }
                self.push_user(content, ts, true);
            }
            // 钩子拦下提问时的说明、迁移来的会话里的回复、桌面端的回复
            Some("assistant") => {
                self.assistant_content(content, ts);
                let calls = message.get("toolCalls").and_then(Value::as_array);
                for call in calls.into_iter().flatten() {
                    // OpenAI 形,名字与参数在 function 里;Kimi 自己直接放在顶层
                    let func = call.get("function").unwrap_or(call);
                    let name = func.get("name").and_then(Value::as_str).unwrap_or_default();
                    let args = decoded_arguments(func.get("arguments").unwrap_or(&Value::Null));
                    let id = call.get("id").and_then(Value::as_str).unwrap_or_default();
                    self.tool_call(id, name, &args, ts);
                }
            }
            Some("tool") => {
                let id = message
                    .get("toolCallId")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let is_error = message.get("isError").and_then(Value::as_bool) == Some(true);
                self.tool_result(id, content, is_error);
            }
            Some("system") => {}
            _ => self.unknown += 1,
        }
    }

    /// `context.append_loop_event`:CLI 按步写下的模型产出
    fn loop_event(&mut self, event: &Value, ts: i64) {
        let text = |key: &str| event.get(key).and_then(Value::as_str).unwrap_or_default();
        match text("type") {
            "step.begin" | "step.end" => {}
            "content.part" => {
                if let Some(part) = event.get("part") {
                    self.assistant_content(part, ts);
                }
            }
            "tool.call" => self.tool_call(
                text("toolCallId"),
                text("name"),
                event.get("args").unwrap_or(&Value::Null),
                ts,
            ),
            "tool.result" => {
                let result = event.get("result");
                let output = result.and_then(|r| r.get("output")).unwrap_or(&Value::Null);
                let is_error = result
                    .and_then(|r| r.get("isError"))
                    .and_then(Value::as_bool)
                    == Some(true);
                self.tool_result(text("toolCallId"), output, is_error);
            }
            _ => self.unknown += 1,
        }
    }

    fn finish(mut self) -> (Vec<TranscriptMessage>, u32) {
        collapse_inherited(&mut self.messages, self.inherited, "session");
        assign_seq(&mut self.messages);
        (self.messages, self.unknown)
    }
}

fn parse_kimi_wire(path: &Path, decode_images: bool) -> Result<(Vec<TranscriptMessage>, u32)> {
    let _image_budget = transcript_image_decode_budget(decode_images);
    let reader = BufReader::with_capacity(1 << 20, fs::File::open(path)?);
    let mut wire = WireReader {
        decode_images,
        ..Default::default()
    };
    for row in jsonl_values(reader) {
        match row? {
            Some(row) => wire.record(&row),
            None => wire.unknown += 1,
        }
    }
    Ok(wire.finish())
}

fn session_key_of(native_id: &str) -> String {
    session_key(AgentId::Kimi, "", native_id)
}

fn build_meta(
    r: &SessionFileRef,
    state: &KimiState,
    cwd: &str,
    messages: &[TranscriptMessage],
) -> SessionMeta {
    let title = Some(clean_title_candidate(&state.title))
        .filter(|t| !t.is_empty())
        .or_else(|| title_from_messages(messages))
        .unwrap_or_else(|| UNTITLED.to_string());
    // Kimi 在 Windows 上把 workDir 记成 `C:/…`,别家记 `C:\…`:换成同一种写法才归到同一个项目
    let project = canonical_project_path(cwd);
    SessionMeta {
        key: session_key_of(&r.native_id),
        host: String::new(),
        id: r.native_id.clone(),
        agent: AgentId::Kimi,
        title,
        project_name: project_name_of(&project),
        project_path: project,
        file_path: r.file_path.clone(),
        created_at: if state.created_ms > 0 {
            state.created_ms
        } else {
            r.mtime_ms
        },
        updated_at: if state.updated_ms > 0 {
            state.updated_ms
        } else {
            r.mtime_ms
        },
        message_count: messages
            .iter()
            .filter(|m| m.kind == MessageKind::Text)
            .count() as i64,
        size_bytes: r.size,
        git_branch: None,
        model: None,
        tokens_used: None,
        archived: state.archived,
        source: None,
        favorite: false,
        pinned: false,
    }
}

/// 选中的目录里有没有 `wd_*` 工作目录桶(没有 `sessions/` 子目录时据此认"这就是 sessions")
fn has_workdir_buckets(dir: &Path) -> bool {
    fs::read_dir(dir).is_ok_and(|entries| entries.flatten().any(|e| is_workdir_bucket(&e.path())))
}

impl AgentAdapter for KimiAdapter {
    fn agent(&self) -> AgentId {
        AgentId::Kimi
    }

    fn list_session_files(&self) -> Result<Vec<SessionFileRef>> {
        // 主文件判定(存在、非空、native_id)统一走 file_ref
        let rel = main_wire_rel();
        Ok(self
            .session_dirs()
            .iter()
            .filter_map(|dir| self.file_ref(&dir.join(&rel)))
            .collect())
    }

    fn file_ref(&self, path: &Path) -> Option<SessionFileRef> {
        // 只认主代理的 wire.jsonl(形状判定在 session_dir_of;agents/<其他>/ 是子代理)。
        // state.json 单独改了(在 Kimi 里归档、改名)也要重新解析:监听到它就换成同会话的
        // 主转录,它的 mtime / size 并进引用——全量扫描按引用判脏
        let wire = if path.file_name()? == STATE_FILE {
            path.parent()?.join(main_wire_rel())
        } else {
            path.to_path_buf()
        };
        let dir = session_dir_of(&wire)?;
        let mut r = default_file_ref(self.agent(), &wire)?;
        // 会话目录名即 native_id(与 session_index.jsonl 的 sessionId 同形,resume 直接可用)
        r.native_id = dir.file_name()?.to_string_lossy().to_string();
        if let Ok(state) = fs::metadata(dir.join(STATE_FILE)) {
            r.mtime_ms = r.mtime_ms.max(mtime_ms(&state));
            r.size += state.len() as i64;
        }
        Some(r)
    }

    fn session_paths(&self, meta: &SessionMeta) -> Vec<String> {
        // 会话是整个会话目录(state.json + agents/),整目录进废纸篓
        session_dir_of(Path::new(&meta.file_path))
            .map(|d| vec![d.to_string_lossy().to_string()])
            .unwrap_or_else(|| vec![meta.file_path.clone()])
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

    fn cleanup_paths(&self, meta: &SessionMeta) -> Option<Vec<String>> {
        Some(self.session_paths(meta))
    }

    /// 分叉来的会话挂回父会话(state.json 的 `forkedFrom`;复制来的那段在解析时折成一条)
    fn manages_parent_links(&self) -> bool {
        true
    }

    fn parent_links(&self) -> Option<Vec<(String, String)>> {
        // 每批监听事件都会问:只扫目录、每个会话 stat 一次 state.json(没变就用缓存);转录在不在
        // 不用看,没入库的子会话 scanner 自己会丢掉
        let dirs = self.session_dirs();
        self.states.retain_only(&dirs);
        let mut links = Vec::new();
        for dir in &dirs {
            // 读不出又没读到过:不知道,库里的关系这一轮原样保留
            let state = self.states.get(dir).ok()?;
            let child = dir.file_name()?.to_string_lossy();
            if let Some(parent) = state.forked_from.as_deref().filter(|p| *p != child) {
                links.push((session_key_of(&child), session_key_of(parent)));
            }
        }
        Some(links)
    }

    /// 关系写在子会话自己的 state.json 里:多 location 下按子会话的胜出文件认边
    fn parent_links_in_child(&self) -> bool {
        true
    }

    fn with_custom_root(&self, dir: PathBuf) -> Box<dyn AgentAdapter> {
        // 按名字认形状(远程挂载点在第一次同步之前还不存在):sessions 目录本身、单个
        // `wd_*` 工作目录桶;其余看选中目录里面:有 sessions/ 是 home,直接放着 wd_* 是
        // sessions 的拷贝,都没有(home 里还没建 sessions/)按 home 算。索引相对根派生——
        // 落回默认家会拿错 cwd 映射
        let is_root = is_workdir_bucket(&dir)
            || dir.file_name().is_some_and(|name| name == "sessions")
            || (!dir.join("sessions").is_dir() && has_workdir_buckets(&dir));
        let root = if is_root { dir } else { dir.join("sessions") };
        Box::new(Self::from_root(root))
    }

    fn data_roots(&self) -> Vec<PathBuf> {
        vec![self.root.clone()]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// `<home>/sessions/wd_app_1/<id>/`:一句用户话的主转录 + 给定的 state.json
    fn session(home: &Path, id: &str, state: &str) -> PathBuf {
        let dir = home.join("sessions").join("wd_app_1").join(id);
        let wire = dir.join(main_wire_rel());
        fs::create_dir_all(wire.parent().unwrap()).unwrap();
        fs::write(
            &wire,
            "{\"type\":\"turn.prompt\",\"time\":1791417601000,\"input\":[{\"type\":\"text\",\"text\":\"hi\"}],\"origin\":{\"kind\":\"user\"}}\n",
        )
        .unwrap();
        fs::write(dir.join(STATE_FILE), state).unwrap();
        dir
    }

    #[test]
    fn half_written_state_keeps_what_was_read_before() {
        // Kimi 用普通 writeFile 写 state.json(先截断再写):半截的那一刻沿用上次读到的,
        // 没读到过的按没有算;读不出来(拿目录顶替文件)是不知道——父子关系整份交 None,
        // 解析失败、留着库里的旧行
        let tmp = tempfile::tempdir().unwrap();
        session(tmp.path(), "session_p", "{\"title\":\"Parent\"}");
        let child = session(
            tmp.path(),
            "session_c",
            "{\"title\":\"Child\",\"forkedFrom\":\"session_p\",\"archived\":true}",
        );
        let link = vec![("kimi:session_c".to_string(), "kimi:session_p".to_string())];
        let kimi = KimiAdapter::from_root(tmp.path().join("sessions"));
        assert_eq!(kimi.parent_links(), Some(link.clone()));

        fs::write(child.join(STATE_FILE), "{\"title\":\"Ch").unwrap();
        assert_eq!(kimi.parent_links(), Some(link.clone()));
        let r = kimi.file_ref(&child.join(main_wire_rel())).unwrap();
        let meta = kimi.parse_session(&r).unwrap().meta;
        assert_eq!((meta.title.as_str(), meta.archived), ("Child", true));
        let fresh = KimiAdapter::from_root(tmp.path().join("sessions"));
        assert_eq!(fresh.parent_links(), Some(Vec::new()));
        assert!(!fresh.parse_session(&r).unwrap().meta.archived);

        fs::remove_file(child.join(STATE_FILE)).unwrap();
        fs::create_dir(child.join(STATE_FILE)).unwrap();
        assert_eq!(kimi.parent_links(), Some(link));
        let fresh = KimiAdapter::from_root(tmp.path().join("sessions"));
        assert_eq!(fresh.parent_links(), None);
        assert!(fresh.parse_session(&r).is_err());
    }

    #[test]
    fn state_changes_reach_the_session_ref() {
        // 只改了 state.json(在 Kimi 里归档、改名):它的事件换成同会话的主转录,它的 mtime /
        // size 并进引用——不然全量扫描判不出脏,归档标记永远停在上次
        let tmp = tempfile::tempdir().unwrap();
        let dir = session(tmp.path(), "session_a", "{\"title\":\"A\"}");
        let kimi = KimiAdapter::from_root(tmp.path().join("sessions"));
        let before = kimi.file_ref(&dir.join(main_wire_rel())).unwrap();
        let by_state = kimi.file_ref(&dir.join(STATE_FILE)).unwrap();
        assert_eq!(by_state.file_path, before.file_path);
        assert_eq!(by_state.native_id, "session_a");
        assert_eq!(kimi.list_session_files().unwrap().len(), 1);

        fs::write(dir.join(STATE_FILE), "{\"title\":\"A\",\"archived\":true}").unwrap();
        let after = kimi.file_ref(&dir.join(main_wire_rel())).unwrap();
        assert_ne!(after.size, before.size);
        assert!(kimi.parse_session(&after).unwrap().meta.archived);
        // 别的目录里的 state.json 不是会话
        assert!(kimi.file_ref(&tmp.path().join(STATE_FILE)).is_none());
    }

    #[test]
    fn kimi_code_home_is_used_only_when_it_has_sessions() {
        let tmp = tempfile::tempdir().unwrap();
        let fallback = home_from(None);
        assert!(fallback.ends_with(".kimi-code"));
        // 变量指向一个还没有会话的目录:回落默认,不让整家会话凭空消失
        assert_eq!(home_from(Some(tmp.path().to_path_buf())), fallback);
        fs::create_dir_all(tmp.path().join("sessions")).unwrap();
        assert_eq!(home_from(Some(tmp.path().to_path_buf())), tmp.path());
    }

    #[test]
    fn origins_follow_kimis_own_visibility_rules() {
        assert_eq!(user_visible(None), Some(true));
        for (origin, want) in [
            (json!({"kind": "user"}), Some(true)),
            (
                json!({"kind": "skill_activation", "trigger": "user-slash"}),
                Some(true),
            ),
            (
                json!({"kind": "skill_activation", "trigger": "model"}),
                Some(false),
            ),
            (
                json!({"kind": "shell_command", "phase": "input"}),
                Some(true),
            ),
            (json!({"kind": "background_task"}), Some(false)),
            (
                json!({"kind": "system_trigger", "name": "goal_continuation"}),
                Some(false),
            ),
            (json!({"kind": "brand_new"}), None),
        ] {
            assert_eq!(user_visible(Some(&origin)), want, "{origin}");
        }
    }

    #[test]
    fn titles_follow_kimis_own_rules() {
        let title = |raw: &str| parse_state(raw.as_bytes()).unwrap().title;
        assert_eq!(
            title(r#"{"title":"Renamed","isCustomTitle":true,"customTitle":"Old"}"#),
            "Renamed"
        );
        // 老写法:改过的名字在 customTitle
        assert_eq!(
            title(r#"{"title":"Generated","customTitle":"Mine"}"#),
            "Mine"
        );
        assert_eq!(title(r#"{"title":"New Session"}"#), "");
    }

    /// Kimi 在自己那边归档、改名只改 state.json,转录不动:监听到它也要把会话重新解析一遍;
    /// 与转录同批到达只解析一次
    #[test]
    fn a_state_change_reparses_its_session() {
        use crate::{db::Store, scanner::NullEvents, watcher::process_batch};
        use notify::event::{EventKind, ModifyKind};
        let tmp = tempfile::tempdir().unwrap();
        let dir = session(tmp.path(), "session_a", "{\"title\":\"Before\"}");
        let (wire, state) = (dir.join(main_wire_rel()), dir.join(STATE_FILE));
        let store = Arc::new(Store::open(&tmp.path().join("wake.db")).unwrap());
        let root = tmp.path().join("sessions");
        let adapters: Vec<Box<dyn AgentAdapter>> =
            vec![Box::new(KimiAdapter::from_root(root.clone()))];
        let roots = vec![(root, 0)];
        let batch = |paths: &[&Path]| {
            let event = paths.iter().fold(
                notify::Event::new(EventKind::Modify(ModifyKind::Any)),
                |event, path| event.add_path(path.to_path_buf()),
            );
            process_batch(&adapters, &store, &NullEvents, &roots, vec![Ok(event)]);
        };
        let row = || store.get_session("kimi:session_a").unwrap().unwrap();

        batch(&[&wire, &state]);
        assert_eq!((row().title, row().archived), ("Before".to_string(), false));
        fs::write(&state, "{\"title\":\"After\",\"archived\":true}").unwrap();
        batch(&[&state]);
        assert_eq!((row().title, row().archived), ("After".to_string(), true));
        assert_eq!(row().file_path, wire.to_string_lossy());
    }

    #[test]
    fn custom_roots_take_their_shape_from_the_name() {
        let base = Path::new("/x/.kimi-code");
        let adapter = KimiAdapter::new();
        let roots = |dir: &Path| adapter.with_custom_root(dir.to_path_buf()).data_roots();
        assert_eq!(roots(&base.join("sessions")), vec![base.join("sessions")]);
        let bucket = base.join("sessions").join("wd_app_0123456789ab");
        assert_eq!(roots(&bucket), vec![bucket.clone()]);
        // home 里还没建 sessions/:照样按 home 算,建好之后列得出来
        assert_eq!(roots(base), vec![base.join("sessions")]);
        // 索引在 home 顶层:根是 sessions 时在上一级,是桶时在上两级
        for root in [base.join("sessions"), bucket] {
            assert_eq!(
                KimiAdapter::from_root(root).index_path,
                base.join(INDEX_FILE)
            );
        }
    }
}
