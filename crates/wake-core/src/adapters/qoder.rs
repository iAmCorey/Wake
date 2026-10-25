use super::parse_utils::*;
use super::AgentAdapter;
use crate::models::*;
use anyhow::Result;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};

/// Qoder CLI:`~/.qoder/projects/<project-key>/<session-id>.jsonl`。
///
/// user/assistant/system 行以 `uuid` + `parentUuid` 组成可回退的消息树，尾部
/// `active-leaf` 指向当前分支；标题、工作目录和运行配置则以独立元数据行追加。
/// 会话同名目录还保存 state/subagents 等边车：`subagents/agent-*.jsonl` 是派出去的
/// 子代理(整份都是 `isSidechain: true` 的行),作为子会话挂在派它的会话下面。
///
/// Qoder IDE / Qoder CN 的任务(Quest)写在同一棵树里:`<project-key>/transcript/
/// task-<id>.session.execution.jsonl`,行格式是同一套 user/assistant,只是没有
/// parentUuid 与 active-leaf——按文件顺序就是对话本身。
///
/// 千问办公(QwenWorkCN.app)里跑的是同一个引擎,转录写在 `~/.qwenworkcn/projects`,
/// 行格式与 Qoder CLI 一字不差,是本适配器的孪生实例(WorkBuddy 之于 CodeBuddy 同款)。
pub struct QoderAdapter {
    agent: AgentId,
    root: PathBuf,
    /// 配置目录(`~/.qoder` / QODER_CONFIG_DIR);直接选中 projects 目录的自定义根没有
    home: Option<PathBuf>,
    /// 同家两个实例都有同一会话时谁胜出(`dedup_rank`,小者胜)
    rank: u8,
}

impl QoderAdapter {
    pub fn new() -> Self {
        Self::twin(AgentId::Qoder, ".qoder", Some("QODER_CONFIG_DIR"), 0)
    }

    /// 千问办公:`~/.qwenworkcn/projects`。app 没有公开改数据根的环境变量,不猜
    pub fn qwenwork() -> Self {
        Self::twin(AgentId::QwenWork, ".qwenworkcn", None, 0)
    }

    /// 千问办公的前身 QoderWork 国内版留下的 `~/.qoderworkcn/projects`。千问办公把它的会话
    /// 迁进了自己的目录(迁过来的项目目录名还是 `-…-qoderworkcn-workspace-…`),没迁的只在
    /// 这里;两边都有的同一会话由 scanner 的同家同 ID 去重让新目录那份胜出
    pub fn qwenwork_legacy() -> Self {
        Self::twin(AgentId::QwenWork, ".qoderworkcn", None, 1)
    }

    fn twin(agent: AgentId, home_dir_name: &str, env_key: Option<&str>, rank: u8) -> Self {
        let default = super::home_dir().unwrap_or_default().join(home_dir_name);
        let home = env_key
            .and_then(super::env_dir)
            // 与其他 env override 一致：存在但没有任何会话的候选不能遮掉
            // 默认根（Dock 启动与 shell 启动看到的环境经常不同）。
            .filter(|dir| project_tree_has_session(&dir.join("projects"), agent))
            .unwrap_or(default);
        Self {
            agent,
            root: home.join("projects"),
            home: Some(home),
            rank,
        }
    }
}

/// 任务转录所在的目录名(`<project-key>/transcript/`)
const TASK_TRANSCRIPT_DIR: &str = "transcript";

/// 根下每个 project-key 目录里的任务转录。根直接选中某个 project-key 目录时,
/// `transcript/*.jsonl` 已被 `list_project_tree_refs` 当作下一层会话收过,这里不重复收
fn task_transcript_refs(root: &Path, agent: AgentId) -> Vec<SessionFileRef> {
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|entry| fs::read_dir(entry.path().join(TASK_TRANSCRIPT_DIR)).ok())
        .flat_map(|inner| inner.flatten())
        .filter_map(|entry| default_file_ref(agent, &entry.path()))
        .collect()
}

/// 监听事件路径是不是 `<project-key>/transcript/<id>.jsonl`(根下恰好三层)
fn is_task_transcript(root: &Path, path: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(root) else {
        return false;
    };
    let parts: Vec<_> = relative.components().collect();
    parts.len() == 3 && parts[1].as_os_str() == TASK_TRANSCRIPT_DIR
}

/// 引擎在后台跑的内部轮次也写成普通会话文件,首条用户消息是引擎拼好的任务说明:记忆整理
/// (`Target file this round: MEMORY.md` / `USER.md`)与 shell 命令的安全判定
/// (`# Input\nCommand: …`)。没有一句是人写的,app 自己的会话列表也不列它们。按行里
/// JSON 转义后的原样比对
const INTERNAL_RUN_PREFIXES: &[&[u8]] = &[br"Target file this round: ", br"# Input\nCommand: "];

/// 判据只读文件头一段:首条 user 行前面只有几行短元数据,前缀就在那一行开头附近。
/// 读不出、头里没有 user 行一律保守放行——半写入的真实会话不能消失
const INTERNAL_RUN_PROBE: u64 = 16 * 1024;

fn is_internal_run(path: &Path) -> bool {
    let mut head = Vec::new();
    let read =
        fs::File::open(path).and_then(|file| file.take(INTERNAL_RUN_PROBE).read_to_end(&mut head));
    if read.is_err() {
        return false;
    }
    // 不靠键序:块的 type 只有 text / tool_result 一类,`"type":"user"` 只会是行本身的
    let Some(line) = head
        .split(|byte| *byte == b'\n')
        .find(|line| find_bytes(line, br#""type":"user""#).is_some())
    else {
        return false;
    };
    // 首个文本:content 是字符串时紧跟 `"content":"`,是块数组时在首个 `"text":"` 之后
    let text_start = [&br#""content":""#[..], &br#""text":""#[..]]
        .iter()
        .filter_map(|key| find_bytes(line, key).map(|at| at + key.len()))
        .min();
    text_start.is_some_and(|at| {
        INTERNAL_RUN_PREFIXES
            .iter()
            .any(|prefix| line[at..].starts_with(prefix))
    })
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[derive(Clone)]
struct MessageNode {
    row: Value,
    parent: Option<String>,
}

enum ActiveLeaf {
    Missing,
    Empty,
    Uuid(String),
}

#[derive(Default)]
struct PendingAssistant {
    msg_id: Option<String>,
    content: ParsedContent,
    thinking: Vec<String>,
    tool_calls: Vec<ToolCallView>,
    timestamp: Option<i64>,
    model: Option<String>,
}

struct QoderParse {
    messages: Vec<TranscriptMessage>,
    custom_title: String,
    ai_title: String,
    last_prompt: String,
    cwd: String,
    git_branch: Option<String>,
    model: Option<String>,
    tokens_used: i64,
    created_at: i64,
    updated_at: i64,
    unknown_lines: u32,
}

const KNOWN_METADATA_TYPES: &[&str] = &[
    "summary",
    "custom-title",
    "ai-title",
    "last-prompt",
    "tag",
    "workspace-directories",
    "runtime-config",
    "mode",
    "content-replacement",
    "file-history-snapshot",
    "token-stats",
    "active-leaf",
    "relocated",
    "worktree-state",
    // 任务转录:SessionStart 等 hook 的进度、会话模式(plan / long_running)
    "progress",
    "session_meta",
];

fn flush_assistant(
    pending: &mut Option<PendingAssistant>,
    messages: &mut Vec<TranscriptMessage>,
    tool_index: &mut HashMap<String, (usize, usize)>,
) {
    let Some(p) = pending.take() else { return };
    let ParsedContent { text, images } = p.content;
    if text.trim().is_empty()
        && p.tool_calls.is_empty()
        && p.thinking.is_empty()
        && images.is_empty()
    {
        return;
    }
    let (text, truncated) = clip(text.trim(), MAX_MSG_TEXT);
    let thinking = if p.thinking.is_empty() {
        None
    } else {
        Some(clip(&p.thinking.join("\n\n"), MAX_TOOL_IO).0)
    };
    let msg_ix = messages.len();
    for (tool_ix, tool) in p.tool_calls.iter().enumerate() {
        if !tool.id.is_empty() {
            tool_index.insert(tool.id.clone(), (msg_ix, tool_ix));
        }
    }
    messages.push(TranscriptMessage {
        seq: 0,
        role: Role::Assistant,
        kind: MessageKind::Text,
        text,
        truncated,
        tool_calls: p.tool_calls,
        thinking,
        timestamp: p.timestamp,
        model: p.model,
        images,
    });
}

fn active_chain(
    nodes: &HashMap<String, MessageNode>,
    order: &[String],
    active_leaf: &ActiveLeaf,
    unknown: &mut u32,
) -> Vec<Value> {
    let mut cursor = match active_leaf {
        // Qoder 用显式 null 表示已回退到空会话，不能退回磁盘上的最后一条分支。
        ActiveLeaf::Empty => return Vec::new(),
        ActiveLeaf::Uuid(id) if nodes.contains_key(id) => id.clone(),
        // 老版本没有 active-leaf，或记录指向了已被压缩/损坏的节点时，
        // 保守选择文件中最后一个可用消息节点。
        ActiveLeaf::Missing | ActiveLeaf::Uuid(_) => match order.last() {
            Some(id) => id.clone(),
            None => return Vec::new(),
        },
    };
    let mut seen = HashSet::new();
    let mut chain_ids = Vec::new();
    loop {
        if !seen.insert(cursor.clone()) {
            *unknown += 1;
            break;
        }
        let Some(node) = nodes.get(&cursor) else {
            *unknown += 1;
            break;
        };
        chain_ids.push(cursor.clone());
        let Some(parent) = node.parent.as_ref() else {
            break;
        };
        cursor = parent.clone();
    }
    chain_ids.reverse();

    // 同一次 API 响应可能被写成多个共享 message.id 的 assistant 兄弟节点。
    // 单纯沿 parent 链只会拿到其中一个；按文件顺序把所有分片和它们的
    // tool_result 子节点收回，随后 PendingAssistant 会把分片合并为一条消息。
    let mut fragments_by_message: HashMap<String, Vec<String>> = HashMap::new();
    let mut message_by_fragment: HashMap<String, String> = HashMap::new();
    for id in order {
        let Some(node) = nodes.get(id) else { continue };
        if node.row.get("type").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let Some(message_id) = optional_string(node.row.pointer("/message/id")) else {
            continue;
        };
        fragments_by_message
            .entry(message_id.clone())
            .or_default()
            .push(id.clone());
        message_by_fragment.insert(id.clone(), message_id);
    }
    let mut results_by_message: HashMap<String, Vec<String>> = HashMap::new();
    for id in order {
        let Some(node) = nodes.get(id) else { continue };
        let is_tool_result = node.row.get("type").and_then(Value::as_str) == Some("user")
            && node
                .row
                .pointer("/message/content")
                .and_then(Value::as_array)
                .is_some_and(|blocks| {
                    blocks.iter().any(|block| {
                        block.get("type").and_then(Value::as_str) == Some("tool_result")
                    })
                });
        if !is_tool_result {
            continue;
        }
        let Some(message_id) = optional_string(node.row.get("parentUuid"))
            .and_then(|parent| message_by_fragment.get(&parent).cloned())
        else {
            continue;
        };
        results_by_message
            .entry(message_id)
            .or_default()
            .push(id.clone());
    }

    let mut out = Vec::new();
    let mut emitted = HashSet::new();
    for id in chain_ids {
        if emitted.contains(&id) {
            continue;
        }
        let Some(node) = nodes.get(&id) else {
            continue;
        };
        let Some(message_id) = message_by_fragment.get(&id) else {
            emitted.insert(id);
            out.push(node.row.clone());
            continue;
        };

        if let Some(fragment_ids) = fragments_by_message.get(message_id) {
            for fragment_id in fragment_ids {
                if emitted.insert(fragment_id.clone()) {
                    if let Some(fragment) = nodes.get(fragment_id) {
                        out.push(fragment.row.clone());
                    }
                }
            }
        }
        if let Some(result_ids) = results_by_message.get(message_id) {
            for result_id in result_ids {
                if emitted.insert(result_id.clone()) {
                    if let Some(result) = nodes.get(result_id) {
                        out.push(result.row.clone());
                    }
                }
            }
        }
    }
    if out.is_empty() {
        return Vec::new();
    }
    out
}

/// 会话自己的转录跳过 sidechain 行;子代理的转录(`subagents/agent-*.jsonl`)整份都是 sidechain 行
fn parse_qoder_jsonl(path: &Path, decode_images: bool) -> Result<QoderParse> {
    let sidechain = subagent_session_dir(path).is_some();
    let _image_budget = transcript_image_decode_budget(decode_images);
    let file = fs::File::open(path)?;
    let reader = BufReader::with_capacity(1 << 20, file);
    let mut nodes: HashMap<String, MessageNode> = HashMap::new();
    let mut order = Vec::new();
    let mut active_leaf = ActiveLeaf::Missing;
    let mut custom_title = String::new();
    let mut ai_title = String::new();
    let mut last_prompt = String::new();
    let mut relocated_cwd = String::new();
    let mut worktree_cwd = String::new();
    let mut worktree_branch: Option<String> = None;
    let mut runtime_model: Option<String> = None;
    let mut unknown = 0u32;
    // 消息行带 parentUuid 键(首条为 null)就是消息树;任务转录一个都不带
    let mut linked = false;

    for row in jsonl_values(reader) {
        let Some(row) = row? else {
            unknown += 1;
            continue;
        };
        let typ = row.get("type").and_then(Value::as_str).unwrap_or("");
        match typ {
            "custom-title" => {
                if let Some(title) = optional_string(row.get("customTitle")) {
                    custom_title = title;
                }
            }
            "ai-title" => {
                if let Some(title) = optional_string(row.get("aiTitle")) {
                    ai_title = title;
                }
            }
            "last-prompt" => {
                if let Some(prompt) = optional_string(row.get("lastPrompt")) {
                    last_prompt = prompt;
                }
            }
            "active-leaf" => match row.get("leafUuid") {
                Some(Value::Null) => active_leaf = ActiveLeaf::Empty,
                Some(Value::String(id)) => active_leaf = ActiveLeaf::Uuid(id.clone()),
                _ => unknown += 1,
            },
            "relocated" => {
                if let Some(cwd) = optional_string(row.get("relocatedCwd")) {
                    relocated_cwd = cwd;
                }
            }
            "worktree-state" => {
                match row.get("worktreeSession") {
                    Some(Value::Null) => {
                        worktree_cwd.clear();
                        worktree_branch = None;
                    }
                    Some(Value::Object(worktree)) => {
                        // 每条记录都是当前状态快照，缺字段也不能沿用上一条。
                        worktree_cwd =
                            optional_string(worktree.get("worktreePath")).unwrap_or_default();
                        worktree_branch = optional_string(worktree.get("worktreeBranch"));
                    }
                    _ => unknown += 1,
                }
            }
            "runtime-config" => {
                runtime_model = optional_string(row.get("model"))
                    .or_else(|| optional_string(row.pointer("/model/name")));
            }
            // attachment 本身不展示，但它有时会成为下一条消息的 parent，必须
            // 留在树里，否则 active-leaf 回溯会在这里提前断链。
            "user" | "assistant" | "system" | "attachment" => {
                if !sidechain && row.get("isSidechain").and_then(Value::as_bool) == Some(true) {
                    continue;
                }
                let Some(uuid) = optional_string(row.get("uuid")) else {
                    unknown += 1;
                    continue;
                };
                linked |= row.get("parentUuid").is_some() || row.get("logicalParentUuid").is_some();
                let parent = optional_string(row.get("logicalParentUuid"))
                    .or_else(|| optional_string(row.get("parentUuid")));
                if !nodes.contains_key(&uuid) {
                    order.push(uuid.clone());
                }
                nodes.insert(uuid, MessageNode { row, parent });
            }
            other if KNOWN_METADATA_TYPES.contains(&other) => {}
            _ => unknown += 1,
        }
    }

    let chain = if linked {
        active_chain(&nodes, &order, &active_leaf, &mut unknown)
    } else {
        order
            .iter()
            .filter_map(|id| nodes.get(id))
            .map(|node| node.row.clone())
            .collect()
    };
    let mut messages = Vec::new();
    let mut pending: Option<PendingAssistant> = None;
    let mut tool_index: HashMap<String, (usize, usize)> = HashMap::new();
    let mut usage_seen = HashSet::new();
    let mut cwd = String::new();
    let mut git_branch = None;
    let mut model = runtime_model;
    let mut tokens_used = 0i64;
    let mut created_at = 0i64;
    let mut updated_at = 0i64;

    for row in chain {
        let typ = row.get("type").and_then(Value::as_str).unwrap_or("");
        if let Some(value) = optional_string(row.get("cwd")) {
            cwd = value;
        }
        if let Some(value) = optional_string(row.get("gitBranch")) {
            git_branch = Some(value);
        }
        let ts = row.get("timestamp").map(to_epoch_ms).unwrap_or(0);
        if ts > 0 {
            if created_at == 0 {
                created_at = ts;
            }
            updated_at = updated_at.max(ts);
        }

        if typ == "system" {
            flush_assistant(&mut pending, &mut messages, &mut tool_index);
            if row.get("subtype").and_then(Value::as_str) == Some("compact_boundary") {
                messages.push(TranscriptMessage {
                    seq: 0,
                    role: Role::System,
                    kind: MessageKind::CompactSummary,
                    text: "── Context compacted ──".to_string(),
                    truncated: false,
                    tool_calls: Vec::new(),
                    thinking: None,
                    timestamp: (ts > 0).then_some(ts),
                    model: None,
                    images: Vec::new(),
                });
            }
            continue;
        }

        if typ == "attachment" {
            continue;
        }

        let Some(message) = row.get("message") else {
            unknown += 1;
            continue;
        };
        if typ == "user" {
            flush_assistant(&mut pending, &mut messages, &mut tool_index);
            let mut parsed_message = ParsedContent::default();
            // 引擎注入的文本块(`<system-reminder>` 的时区、可用工具)与人打的字是同一条消息里
            // 的两个块:分开收,有人打的字就只显示它,整条都是注入的才按注入内容收起来
            let mut injected = ParsedContent::default();
            match message.get("content") {
                Some(Value::String(text)) => parsed_message.push_text(text),
                Some(Value::Array(blocks)) => {
                    for block in blocks {
                        match block.get("type").and_then(Value::as_str) {
                            Some("text") => {
                                if let Some(text) = block.get("text").and_then(Value::as_str) {
                                    if is_injected_user_content(text) {
                                        injected.push_text(text);
                                    } else {
                                        parsed_message.push_text(text);
                                    }
                                }
                            }
                            _ if is_image_part(block) => {
                                let parsed = content_parts(block, decode_images);
                                parsed_message.append(parsed);
                            }
                            Some("tool_result") => {
                                let id = block
                                    .get("tool_use_id")
                                    .and_then(Value::as_str)
                                    .unwrap_or("");
                                if let Some(&(msg_ix, tool_ix)) = tool_index.get(id) {
                                    let parsed = tool_result_parts(
                                        block.get("content").unwrap_or(&Value::Null),
                                        decode_images,
                                    );
                                    let message = &mut messages[msg_ix];
                                    let tool = &mut message.tool_calls[tool_ix];
                                    tool.output = Some(clip(&parsed.text, MAX_TOOL_IO).0);
                                    tool.is_error = block
                                        .get("is_error")
                                        .and_then(Value::as_bool)
                                        .unwrap_or(false);
                                    append_images_to_message_end(message, parsed.images);
                                }
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
            if parsed_message.text.is_empty() {
                parsed_message.text = injected.text;
            }
            let ParsedContent { text, images } = parsed_message;
            // 千问办公把人打的那句原样另存在 humanInput.text,content 里则在它前面拼了一段
            // `<system-reminder>`(时区、可用工具)——整条按 content 判会被当成注入内容收起来
            let human_input = optional_string(row.pointer("/humanInput/text"));
            let typed = human_input.is_some();
            let text = human_input.unwrap_or(text);
            if text.is_empty() && images.is_empty() {
                continue;
            }
            let compact = row.get("isCompactSummary").and_then(Value::as_bool) == Some(true);
            let meta = !typed
                && (row.get("isMeta").and_then(Value::as_bool) == Some(true)
                    || row
                        .get("isVisibleInTranscriptOnly")
                        .and_then(Value::as_bool)
                        == Some(true)
                    || row
                        .pointer("/origin/kind")
                        .and_then(Value::as_str)
                        .is_some_and(|kind| kind != "human")
                    || is_injected_user_content(&text)
                    // 用户按停时引擎补写的那一行
                    || text.starts_with("[Request interrupted by user"));
            let kind = if compact {
                MessageKind::CompactSummary
            } else if meta {
                MessageKind::Meta
            } else {
                MessageKind::Text
            };
            let (text, truncated) = clip(&text, MAX_MSG_TEXT);
            messages.push(TranscriptMessage {
                seq: 0,
                role: Role::User,
                kind,
                text,
                truncated,
                tool_calls: Vec::new(),
                thinking: None,
                timestamp: (ts > 0).then_some(ts),
                model: None,
                images,
            });
            continue;
        }

        let msg_id = optional_string(message.get("id"));
        let need_new = match (&pending, &msg_id) {
            (None, _) => true,
            (Some(current), Some(id)) => current.msg_id.as_deref().is_some_and(|old| old != id),
            (Some(_), None) => false,
        };
        if need_new {
            flush_assistant(&mut pending, &mut messages, &mut tool_index);
            pending = Some(PendingAssistant {
                msg_id: msg_id.clone(),
                timestamp: (ts > 0).then_some(ts),
                ..Default::default()
            });
        }
        let current = pending.as_mut().expect("assistant pending initialized");
        if current.msg_id.is_none() {
            current.msg_id = msg_id.clone();
        }
        if let Some(value) = optional_string(message.get("model")) {
            if value != "<synthetic>" {
                current.model = Some(value.clone());
                model = Some(value);
            }
        }
        let usage_key = msg_id
            .clone()
            .or_else(|| optional_string(row.get("uuid")))
            .unwrap_or_default();
        if usage_seen.insert(usage_key) {
            if let Some(usage) = message.get("usage") {
                tokens_used += usage_tokens(usage);
            }
        }
        match message.get("content") {
            Some(Value::String(text)) if !text.trim().is_empty() => {
                current.content.push_text(text);
            }
            Some(Value::Array(blocks)) => {
                for block in blocks {
                    match block.get("type").and_then(Value::as_str) {
                        Some("text") => {
                            if let Some(text) = block.get("text").and_then(Value::as_str) {
                                if !text.trim().is_empty() {
                                    current.content.push_text(text);
                                }
                            }
                        }
                        Some("thinking") => {
                            if let Some(text) = block.get("thinking").and_then(Value::as_str) {
                                if !text.trim().is_empty() {
                                    current.thinking.push(text.to_string());
                                }
                            }
                        }
                        Some("tool_use") => {
                            let id = block
                                .get("id")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string();
                            let name = block.get("name").and_then(Value::as_str).unwrap_or("tool");
                            let input = block.get("input").cloned().unwrap_or(Value::Null);
                            current
                                .tool_calls
                                .push(tool_call_view(id, name, &input, None, false));
                        }
                        _ if is_image_part(block) => {
                            let parsed = content_parts(block, decode_images);
                            current.content.append(parsed);
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    flush_assistant(&mut pending, &mut messages, &mut tool_index);
    assign_seq(&mut messages);

    if !worktree_cwd.is_empty() {
        cwd = worktree_cwd;
    } else if !relocated_cwd.is_empty() {
        cwd = relocated_cwd;
    }
    if worktree_branch.is_some() {
        git_branch = worktree_branch;
    }
    Ok(QoderParse {
        messages,
        custom_title,
        ai_title,
        last_prompt,
        cwd,
        git_branch,
        model,
        tokens_used,
        created_at,
        updated_at,
        unknown_lines: unknown,
    })
}

fn build_meta(agent: AgentId, r: &SessionFileRef, parsed: &QoderParse) -> SessionMeta {
    let named = [parsed.custom_title.as_str(), parsed.ai_title.as_str()]
        .into_iter()
        .map(clean_title_candidate)
        .find(|title| !title.is_empty());
    let last_prompt = || Some(clean_title_candidate(&parsed.last_prompt)).filter(|t| !t.is_empty());
    // 千问办公的会话列表用 app 自己起的名字(不在转录里),last-prompt 只是最近一句
    // ("继续"、"好"),拿首条提问当标题更像那个名字;Qoder CLI 的 resume 列表显示的就是 last-prompt
    let title = if agent == AgentId::QwenWork {
        let asked = parsed
            .messages
            .iter()
            .filter(|m| m.role == Role::User && m.kind == MessageKind::Text)
            .map(|m| clean_title_candidate(&qwenwork_title_text(&m.text)))
            .find(|title| !title.is_empty());
        named.or(asked).or_else(last_prompt)
    } else {
        named
            .or_else(last_prompt)
            .or_else(|| title_from_messages(&parsed.messages))
    }
    .unwrap_or_else(|| UNTITLED.to_string());
    SessionMeta {
        key: format!("{}:{}", agent.as_str(), r.native_id),
        host: String::new(),
        id: r.native_id.clone(),
        agent,
        title,
        project_path: parsed.cwd.clone(),
        project_name: project_name_of(&parsed.cwd),
        file_path: r.file_path.clone(),
        created_at: if parsed.created_at > 0 {
            parsed.created_at
        } else {
            r.mtime_ms
        },
        updated_at: parsed.updated_at.max(r.mtime_ms),
        message_count: parsed
            .messages
            .iter()
            .filter(|message| message.kind == MessageKind::Text)
            .count() as i64,
        size_bytes: r.size,
        git_branch: parsed.git_branch.clone(),
        model: parsed.model.clone(),
        tokens_used: (parsed.tokens_used > 0).then_some(parsed.tokens_used),
        archived: false,
        source: None,
        spawned: false,
        favorite: false,
        pinned: false,
    }
}

/// 千问办公在人打的字前后拼的东西,标题里换掉:贴的图片只剩 `<user-pasted-image file=… />`
/// 占位,去掉;贴的长文本包在 `<user-pasted-text …>` 里,留里面的字;从技能面板发起的任务
/// 前面垫着一段固定的"必须用某个 skill 完成"的指令,只留技能名
fn qwenwork_title_text(raw: &str) -> String {
    const SKILL_LEAD: &str = "This request must be fulfilled using the \"";
    const SKILL_TAIL: &str = "Do NOT claim this capability is unavailable.";
    // 一次选了几个技能就垫几段
    let mut skills = Vec::new();
    let mut text = raw.trim_start();
    while let Some(rest) = text.strip_prefix(SKILL_LEAD) {
        let (Some(name_end), Some(tail)) = (rest.find('"'), rest.find(SKILL_TAIL)) else {
            break;
        };
        skills.push(&rest[..name_end]);
        text = rest[tail + SKILL_TAIL.len()..].trim_start();
    }
    let text = if skills.is_empty() {
        text.to_string()
    } else {
        format!("{} · {text}", skills.join(" + "))
    };
    let mut out = String::with_capacity(text.len());
    let mut rest = text.as_str();
    while let Some(start) = rest.find("<user-pasted-") {
        out.push_str(&rest[..start]);
        let Some(end) = rest[start..].find('>') else {
            rest = "";
            break;
        };
        rest = &rest[start + end + 1..];
    }
    out.push_str(rest);
    out.replace("</user-pasted-text>", " ")
}

/// 会话或子代理(子代理是 agent 派出去的,标题取边车记的任务说明)
fn parse(
    agent: AgentId,
    r: &SessionFileRef,
    decode_images: bool,
) -> Result<(SessionMeta, QoderParse)> {
    let path = Path::new(&r.file_path);
    let parsed = parse_qoder_jsonl(path, decode_images)?;
    let mut meta = build_meta(agent, r, &parsed);
    mark_subagent(&mut meta, path);
    Ok((meta, parsed))
}

impl AgentAdapter for QoderAdapter {
    fn agent(&self) -> AgentId {
        self.agent
    }

    fn list_session_files(&self) -> Result<Vec<SessionFileRef>> {
        let mut refs = list_project_tree_refs(&self.root, self.agent);
        refs.extend(task_transcript_refs(&self.root, self.agent));
        refs.retain(|r| !is_internal_run(Path::new(&r.file_path)));
        refs.sort_by(|a, b| a.file_path.cmp(&b.file_path));
        Ok(refs)
    }

    fn file_ref(&self, path: &Path) -> Option<SessionFileRef> {
        let r = if is_task_transcript(&self.root, path) {
            default_file_ref(self.agent, path)
        } else {
            project_tree_file_ref(&self.root, self.agent, path)
        }?;
        (!is_internal_run(path)).then_some(r)
    }

    fn dedup_rank(&self, _path: &str) -> u8 {
        self.rank
    }

    fn parse_session(&self, r: &SessionFileRef) -> Result<ParsedSession> {
        let (meta, parsed) = parse(self.agent, r, false)?;
        Ok(ParsedSession::derive(
            meta,
            &parsed.messages,
            parsed.unknown_lines,
        ))
    }

    fn parse_transcript(&self, r: &SessionFileRef) -> Result<ParsedTranscript> {
        let (meta, parsed) = parse(self.agent, r, true)?;
        Ok(transcript_with_subagents(
            r,
            meta,
            parsed.messages,
            parsed.unknown_lines,
        ))
    }

    fn load_sidechain(
        &self,
        r: &SessionFileRef,
        sidechain_id: &str,
    ) -> Result<Vec<TranscriptMessage>> {
        load_subagent(r, sidechain_id, |file| {
            Ok(parse_qoder_jsonl(file, true)?.messages)
        })
    }

    /// 子代理挂到派它的会话下面:所在会话目录的名字就是那个会话的 id
    fn parent_in_path(&self) -> Option<super::ParentInPath> {
        Some(subagent_parent)
    }

    fn session_paths(&self, meta: &SessionMeta) -> Vec<String> {
        if is_subagent_transcript(&meta.file_path) {
            return subagent_paths(&meta.file_path);
        }
        let mut paths = vec![meta.file_path.clone()];
        if let Some(mut parent) = Path::new(&meta.file_path).parent() {
            // 任务转录的边车(工具结果)不在 transcript/ 里,而在上一层的 project-key 目录
            if parent.file_name().is_some_and(|n| n == TASK_TRANSCRIPT_DIR) {
                parent = parent.parent().unwrap_or(parent);
            }
            let sidecar = parent.join(&meta.id);
            if sidecar.is_dir() {
                paths.push(sidecar.to_string_lossy().to_string());
            }
        }
        paths
    }

    fn cleanup_paths(&self, meta: &SessionMeta) -> Option<Vec<String>> {
        Some(self.session_paths(meta))
    }

    fn with_custom_root(&self, dir: PathBuf) -> Box<dyn AgentAdapter> {
        let (root, home) = if dir.join("projects").is_dir() {
            (dir.join("projects"), Some(dir))
        } else {
            (dir, None)
        };
        Box::new(Self {
            agent: self.agent,
            root,
            home,
            rank: 0,
        })
    }

    fn data_roots(&self) -> Vec<PathBuf> {
        vec![self.root.clone()]
    }

    fn homes(&self) -> Vec<PathBuf> {
        self.home.iter().cloned().collect()
    }
}
