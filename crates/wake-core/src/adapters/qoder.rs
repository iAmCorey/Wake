use super::parse_utils::*;
use super::{units_from_messages, AgentAdapter};
use crate::models::*;
use anyhow::Result;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

/// Qoder CLI:`~/.qoder/projects/<project-key>/<session-id>.jsonl`。
///
/// user/assistant/system 行以 `uuid` + `parentUuid` 组成可回退的消息树，尾部
/// `active-leaf` 指向当前分支；标题、工作目录和运行配置则以独立元数据行追加。
/// 会话同名目录还保存 state/subagents 等边车，主列表只枚举 project-key 目录
/// 直属的 JSONL，不能把 `subagents/agent-*.jsonl` 当成顶层会话。
pub struct QoderAdapter {
    root: PathBuf,
}

impl QoderAdapter {
    pub fn new() -> Self {
        let default = super::home_dir()
            .unwrap_or_default()
            .join(".qoder")
            .join("projects");
        let root = super::env_dir("QODER_CONFIG_DIR")
            .map(|dir| dir.join("projects"))
            // 与其他 env override 一致：存在但没有任何会话的候选不能遮掉
            // 默认根（Dock 启动与 shell 启动看到的环境经常不同）。
            .filter(|dir| contains_session_file(dir))
            .unwrap_or(default);
        Self { root }
    }
}

fn direct_jsonl_refs(dir: &Path) -> Vec<SessionFileRef> {
    let mut refs = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return refs;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(mut r) = default_file_ref(AgentId::Qoder, &path) else {
            continue;
        };
        r.agent = AgentId::Qoder;
        refs.push(r);
    }
    refs
}

/// 默认根下是 `<project-key>/<session>.jsonl`；自定义 location 也允许直接
/// 选中某个 project-key 目录，因此根直属与下一层两种形态都探。
fn list_refs(root: &Path) -> Vec<SessionFileRef> {
    let mut refs = direct_jsonl_refs(root);
    let Ok(entries) = fs::read_dir(root) else {
        return refs;
    };
    for entry in entries.flatten() {
        if entry.file_type().is_ok_and(|t| t.is_dir()) {
            refs.extend(direct_jsonl_refs(&entry.path()));
        }
    }
    refs.sort_by(|a, b| a.file_path.cmp(&b.file_path));
    refs
}

fn contains_session_file(root: &Path) -> bool {
    !list_refs(root).is_empty()
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
];

fn optional_string(v: Option<&Value>) -> Option<String> {
    v.and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn usage_tokens(usage: &Value) -> i64 {
    let total = usage
        .get("total_tokens")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    if total > 0 {
        return total;
    }
    let message_tokens: i64 = [
        "input_tokens",
        "output_tokens",
        "cache_creation_input_tokens",
        "cache_read_input_tokens",
    ]
    .iter()
    .map(|key| usage.get(*key).and_then(Value::as_i64).unwrap_or(0))
    .sum();
    if message_tokens > 0 {
        return message_tokens;
    }
    ["prompt_tokens", "completion_tokens"]
        .iter()
        .map(|key| usage.get(*key).and_then(Value::as_i64).unwrap_or(0))
        .sum()
}

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

fn parse_qoder_jsonl(path: &Path, decode_images: bool) -> Result<QoderParse> {
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

    for line in reader.lines() {
        let line = match line {
            Ok(line) => line,
            Err(_) => {
                unknown += 1;
                continue;
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        let row: Value = match serde_json::from_str(&line) {
            Ok(row) => row,
            Err(_) => {
                unknown += 1;
                continue;
            }
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
                if row.get("isSidechain").and_then(Value::as_bool) == Some(true) {
                    continue;
                }
                let Some(uuid) = optional_string(row.get("uuid")) else {
                    unknown += 1;
                    continue;
                };
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

    let chain = active_chain(&nodes, &order, &active_leaf, &mut unknown);
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
            match message.get("content") {
                Some(Value::String(text)) => parsed_message.push_text(text),
                Some(Value::Array(blocks)) => {
                    for block in blocks {
                        match block.get("type").and_then(Value::as_str) {
                            Some("text") => {
                                if let Some(text) = block.get("text").and_then(Value::as_str) {
                                    parsed_message.push_text(text);
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
            let ParsedContent { text, images } = parsed_message;
            if text.is_empty() && images.is_empty() {
                continue;
            }
            let compact = row.get("isCompactSummary").and_then(Value::as_bool) == Some(true);
            let meta = row.get("isMeta").and_then(Value::as_bool) == Some(true)
                || row
                    .get("isVisibleInTranscriptOnly")
                    .and_then(Value::as_bool)
                    == Some(true)
                || row
                    .pointer("/origin/kind")
                    .and_then(Value::as_str)
                    .is_some_and(|kind| kind != "human")
                || is_injected_user_content(&text);
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

fn build_meta(r: &SessionFileRef, parsed: &QoderParse) -> SessionMeta {
    let title = [
        parsed.custom_title.as_str(),
        parsed.ai_title.as_str(),
        parsed.last_prompt.as_str(),
    ]
    .into_iter()
    .map(clean_title_candidate)
    .find(|title| !title.is_empty())
    .or_else(|| title_from_messages(&parsed.messages))
    .unwrap_or_else(|| UNTITLED.to_string());
    SessionMeta {
        key: format!("qoder:{}", r.native_id),
        id: r.native_id.clone(),
        agent: AgentId::Qoder,
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
        favorite: false,
        pinned: false,
    }
}

impl AgentAdapter for QoderAdapter {
    fn agent(&self) -> AgentId {
        AgentId::Qoder
    }

    fn list_session_files(&self) -> Result<Vec<SessionFileRef>> {
        Ok(list_refs(&self.root))
    }

    fn file_ref(&self, path: &Path) -> Option<SessionFileRef> {
        let relative = path.strip_prefix(&self.root).ok()?;
        // root/<session>.jsonl 或 root/<project-key>/<session>.jsonl；更深的
        // state/subagents/memory 均是边车，不得成为顶层会话。
        if relative.components().count() > 2 {
            return None;
        }
        default_file_ref(self.agent(), path)
    }

    fn parse_session(&self, r: &SessionFileRef) -> Result<ParsedSession> {
        let parsed = parse_qoder_jsonl(Path::new(&r.file_path), false)?;
        let meta = build_meta(r, &parsed);
        let units = units_from_messages(&parsed.messages);
        Ok(ParsedSession {
            meta,
            units,
            unknown_line_count: parsed.unknown_lines,
        })
    }

    fn parse_transcript(&self, r: &SessionFileRef) -> Result<ParsedTranscript> {
        let parsed = parse_qoder_jsonl(Path::new(&r.file_path), true)?;
        Ok(ParsedTranscript {
            meta: build_meta(r, &parsed),
            mainline: parsed.messages,
            sidechains: Vec::new(),
            unknown_line_count: parsed.unknown_lines,
        })
    }

    fn session_paths(&self, meta: &SessionMeta) -> Vec<String> {
        let mut paths = vec![meta.file_path.clone()];
        if let Some(parent) = Path::new(&meta.file_path).parent() {
            let sidecar = parent.join(&meta.id);
            if sidecar.is_dir() {
                paths.push(sidecar.to_string_lossy().to_string());
            }
        }
        paths
    }

    fn with_custom_root(&self, dir: PathBuf) -> Box<dyn AgentAdapter> {
        let root = if dir.join("projects").is_dir() {
            dir.join("projects")
        } else {
            dir
        };
        Box::new(Self { root })
    }

    fn data_roots(&self) -> Vec<PathBuf> {
        vec![self.root.clone()]
    }
}
