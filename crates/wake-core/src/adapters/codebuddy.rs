use super::parse_utils::*;
use super::AgentAdapter;
use crate::models::*;
use anyhow::Result;
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::sync::Arc;
#[cfg(test)]
use std::sync::Mutex;

/// CodeBuddy Code(腾讯 CodeBuddy 的 CLI,issue #27):`~/.codebuddy/projects/
/// <cwd 转义>/<sessionId>.jsonl`。文件布局借自 Claude Code——同名目录
/// `<sessionId>/subagents/agent-*.jsonl` 是子代理转录(作为子会话挂在派它的会话
/// 下面)、`<sessionId>.meta.json` 是边车;**行格式却是 OpenAI Responses
/// 形**:`message`(role 挂在信封上,content 块是 input_text / output_text)、
/// `reasoning`(rawContent[].reasoning_text)、`function_call`(callId / name /
/// arguments 是 JSON 字符串)、`function_call_result`(callId / output.text /
/// status)。一次模型调用 = reasoning + message + function_call 若干行;行上
/// 没有按次调用的 id(providerData.conversationRequestId 整个用户回合共用),
/// 所以按位置分条:工具结果回来之后再出现的 assistant 侧行视为下一次调用,
/// 另起一条助手消息,粒度与 Claude 按 API 响应分条相当,工具簇折叠靠它。
///
/// 标题行三种,优先级 `custom-title`(用户 `/rename`)> `ai-title` > `topic`,
/// 各自 last-wins;后两种会写占位符("(No content)"、"/compact"、
/// `<image_local_path>…`),CodeBuddy 自己的 getEffectiveSessionTitle 也跳过
/// 它们。模型取 providerData.requestModelName(人类可读)回退 model(id);
/// token 取 providerData.rawUsage(OpenAI completions 形)按调用累加。
/// 已知的元数据行见 `KNOWN_SKIP`,表外计 unknown。`CODEBUDDY_CONFIG_DIR` 覆盖数据根(与 Qoder 的 QODER_CONFIG_DIR
/// 同款:探到会话才采信)。
///
/// CBC 格式来自官方 CLI 文档及实现交叉核对;4.x 导入和真人回答另有本机
/// 只读样本与合成 fixture 覆盖。catalog 仅作为元数据来源,不写入 CodeBuddy。
///
/// WorkBuddy(腾讯的桌面 agent,与 CodeBuddy 同一内核)是本适配器的孪生实例
/// (与 pi / omp 同款):`~/.workbuddy/projects` 同构树、`WORKBUDDY_CONFIG_DIR`
/// 覆盖(官方桌面端设置的就是这个变量),解析核心完全共用,差异只有 agent 身份
/// 与根;它没有 CLI,resume 不提供。同构这一点只有 cc-switch 一家的说法,
/// 比 CodeBuddy 的三家佐证弱。
pub struct CodebuddyAdapter {
    agent: AgentId,
    root: PathBuf,
    /// 配置目录(`~/.codebuddy` / `*_CONFIG_DIR`);直接选中 projects 目录的自定义根没有
    home: Option<PathBuf>,
    catalog: Option<SourceCatalog>,
}

impl Default for CodebuddyAdapter {
    fn default() -> Self {
        Self::new()
    }
}

struct SourceCatalog {
    path: PathBuf,
    legacy_data: PathBuf,
    host: Option<String>,
    cache: super::codebuddy_legacy::CatalogCache,
    protect_missing: bool,
}

impl SourceCatalog {
    fn new(path: PathBuf, legacy_data: PathBuf, host: Option<String>) -> Self {
        Self {
            path,
            legacy_data,
            host,
            cache: super::codebuddy_legacy::CatalogCache::default(),
            protect_missing: true,
        }
    }

    fn rows(&self) -> Result<Arc<super::codebuddy_legacy::CatalogSnapshot>> {
        self.cache.read(
            &self.path,
            self.protect_missing,
            self.host.is_some() && self.legacy_data.is_dir(),
        )
    }

    fn row<'a>(
        &self,
        path: &Path,
        runtime: &str,
        rows: &'a [super::codebuddy_legacy::CatalogRow],
    ) -> Option<&'a super::codebuddy_legacy::CatalogRow> {
        let parent = subagent_parent(path);
        let runtime = parent.as_deref().unwrap_or(runtime);
        rows.iter().find(|row| {
            super::codebuddy_legacy::authority_matches(&row.authority, self.host.as_deref())
                && row.provider == "cbc"
                && (row.runtime_id == runtime || (row.runtime_id.is_empty() && row.id == runtime))
        })
    }

    fn legacy_index(&self, row: &super::codebuddy_legacy::CatalogRow) -> Option<PathBuf> {
        Some(
            self.legacy_data
                .join(super::codebuddy_legacy::legacy_relative_dir(row)?)
                .join("index.json"),
        )
    }

    fn converted(&self, row: &super::codebuddy_legacy::CatalogRow, file: &Path) -> bool {
        if self
            .legacy_index(row)
            .is_some_and(|p| p.parent().is_some_and(Path::is_dir))
        {
            return true;
        }
        let Some(parent) = file.parent() else {
            return false;
        };
        let marker = parent
            .join(&row.runtime_id)
            .join(".codebuddy-ide-4-conversion.json");
        fs::read(marker)
            .ok()
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            .and_then(|v| {
                v.get("conversationId")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .is_some_and(|id| id == row.id)
    }

    /// Attach catalog/legacy changes to the reference so a manual refresh
    /// revisits even an unchanged CBC transcript.
    fn reference(
        &self,
        mut r: SessionFileRef,
        rows: &[super::codebuddy_legacy::CatalogRow],
    ) -> Option<SessionFileRef> {
        let file = Path::new(&r.file_path);
        let row = self.row(file, &r.native_id, rows);
        if row.is_none() && self.host.is_none() {
            return Some(r);
        }
        if row.is_some_and(|row| row.deleted) {
            return None;
        }
        let converted =
            row.filter(|row| !is_subagent_transcript(&r.file_path) && self.converted(row, file));
        if let Some(row) = converted {
            r.native_id = row.id.clone();
        }
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        // Bump when catalog-derived metadata changes so existing CBC entries
        // are re-read without forcing every other agent to reindex.
        1_u8.hash(&mut hash);
        r.mtime_ms.hash(&mut hash);
        row.hash(&mut hash);
        if let Ok(metadata) = fs::metadata(file) {
            super::codebuddy_legacy::hash_file_identity(&metadata, &mut hash);
        }
        if let Some(row) = converted {
            if let Some(index) = self.legacy_index(row) {
                match index
                    .parent()
                    .and_then(|dir| super::codebuddy_legacy::read_session_file(dir, &index).ok())
                {
                    Some(bytes) => bytes.hash(&mut hash),
                    None => 0_u8.hash(&mut hash),
                }
            }
        }
        r.mtime_ms = (hash.finish() & i64::MAX as u64) as i64;
        Some(r)
    }
}

fn has_conversion_marker(path: &Path) -> bool {
    let Some(parent) = path.parent() else {
        return false;
    };
    let Some(id) = path.file_stem() else {
        return false;
    };
    parent
        .join(id)
        .join(".codebuddy-ide-4-conversion.json")
        .is_file()
}

impl CodebuddyAdapter {
    pub fn new() -> Self {
        Self::twin(AgentId::Codebuddy, ".codebuddy", "CODEBUDDY_CONFIG_DIR")
    }

    /// WorkBuddy 变体:`~/.workbuddy/projects`,解析核心完全共用
    pub fn workbuddy() -> Self {
        Self::twin(AgentId::Workbuddy, ".workbuddy", "WORKBUDDY_CONFIG_DIR")
    }

    fn twin(agent: AgentId, home_dir_name: &str, env_key: &str) -> Self {
        let default = super::home_dir().unwrap_or_default().join(home_dir_name);
        let home = super::env_dir(env_key)
            // 与其他 env override 一致:存在但没有任何会话的候选不能遮掉默认根
            // (Dock 启动与 shell 启动看到的环境经常不同)
            .filter(|dir| project_tree_has_session(&dir.join("projects"), agent))
            .unwrap_or(default);
        Self {
            agent,
            root: home.join("projects"),
            catalog: (agent == AgentId::Codebuddy).then(|| {
                SourceCatalog::new(
                    home.join("codebuddy.db"),
                    super::codebuddy_legacy::default_data_root(),
                    None,
                )
            }),
            home: Some(home),
        }
    }

    pub(crate) fn remote(root: PathBuf, host_cache: &Path, host: &str) -> Self {
        Self {
            agent: AgentId::Codebuddy,
            root,
            home: None,
            catalog: Some(SourceCatalog::new(
                super::codebuddy_legacy::catalog_path(),
                host_cache.join(super::codebuddy_legacy::REMOTE_DATA_ROOT),
                Some(host.to_string()),
            )),
        }
    }
}

/// 已知的非消息行:静默跳过、不计 unknown。表外类型计数(漂移金丝雀)。照 CodeBuddy 2.167
/// `isCustomInputItem` 的清单(标题行在解析循环里另处理)加它追加进转录的 `session-meta`
/// 快照。`model-usage` 是用量没落到消息行上的那次调用的账,是否与消息行上的 rawUsage 互斥
/// 从打包后的代码里判不准,先不计 token
const KNOWN_SKIP: &[&str] = &[
    "file-history-snapshot",
    "summary",
    "turn-metrics",
    "goal-result",
    "goal-progress",
    "resend-fork-notice",
    "acp-terminal-state",
    "model-usage",
    "credit-usage",
    "session-meta",
];

/// CodeBuddy 的 isGeneratedPlaceholderTitle:自动标题没东西可总结时写的占位,
/// 显示它比回退到首条用户消息更糟
fn is_placeholder_title(text: &str) -> bool {
    text == "(No content)"
        || text == "/compact"
        || (text.starts_with("<image_local_path>") && text.ends_with("</image_local_path>"))
}

/// providerData 里的模型名:优先人类可读的 requestModelName,回退 model id;
/// 两者都只在非空时采信,空串不遮掉有效值
fn row_model(row: &Value) -> Option<String> {
    let pd = row.get("providerData")?;
    ["requestModelName", "model"]
        .into_iter()
        .find_map(|key| optional_string(pd.get(key)))
}

/// 一行的 token 用量:providerData.rawUsage 是写端的原始 completions 账
/// (prompt_tokens 已含缓存前缀),message.usage 是同一份的规整形
fn row_usage(row: &Value) -> i64 {
    row.pointer("/providerData/rawUsage")
        .or_else(|| row.pointer("/message/usage"))
        .map(usage_tokens)
        .unwrap_or(0)
}

fn content_of(row: &Value, decode_images: bool) -> ParsedContent {
    content_parts(row.get("content").unwrap_or(&Value::Null), decode_images)
}

/// CodeBuddy's 4.x converter renders an AskUserQuestion answer as one line
/// per question: ` · question → answer`. Only the answer was typed by the
/// user. Ordinary tool output must remain outside the full-text index.
fn ask_user_answers(output: &Value, input: Option<&str>) -> Vec<String> {
    let mut keys = std::collections::HashSet::new();
    if let Some(questions) = input
        .and_then(|s| serde_json::from_str::<Value>(s).ok())
        .and_then(|v| v.get("questions").and_then(Value::as_array).cloned())
    {
        for (ix, question) in questions.iter().enumerate() {
            keys.insert(format!("q_{ix}"));
            for key in ["id", "question"] {
                if let Some(value) = question.get(key).and_then(Value::as_str) {
                    keys.insert(value.to_string());
                }
            }
        }
    }
    if let Some(answers) = structured_answers(output, &keys, 0) {
        return answers;
    }
    fn output_text(value: &Value, depth: usize) -> String {
        if depth > 5 {
            return String::new();
        }
        match value {
            Value::String(text) => text.clone(),
            Value::Array(items) => items
                .iter()
                .map(|v| output_text(v, depth + 1))
                .collect::<Vec<_>>()
                .join("\n"),
            Value::Object(_) => value
                .get("text")
                .or_else(|| value.get("content"))
                .map(|v| output_text(v, depth + 1))
                .unwrap_or_default(),
            _ => String::new(),
        }
    }
    let text = output_text(output, 0);
    let mut answers: Vec<String> = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        let marked = trimmed.starts_with(['·', '•']);
        let candidate = trimmed
            .trim_start_matches(|c: char| c.is_whitespace() || matches!(c, '·' | '•' | '-' | '*'));
        // The converter prefixes each new question. Unmarked continuation
        // lines may themselves contain arrows, bullets, or indentation.
        let pair = candidate
            .split_once('→')
            .or_else(|| candidate.split_once("->"));
        if let Some((_, answer)) = pair.filter(|(q, _)| {
            !q.trim().is_empty()
                && (answers.is_empty() || keys.contains(q.trim()) || (keys.is_empty() && marked))
        }) {
            answers.push(answer.trim().to_string());
        } else if let Some(answer) = answers.last_mut() {
            answer.push('\n');
            answer.push_str(line);
        }
    }
    answers
        .into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn structured_answers(
    value: &Value,
    keys: &std::collections::HashSet<String>,
    depth: usize,
) -> Option<Vec<String>> {
    if depth > 5 {
        return None;
    }
    match value {
        Value::String(s) if s.trim_start().starts_with(['{', '[']) => {
            serde_json::from_str::<Value>(s)
                .ok()
                .and_then(|v| structured_answers(&v, keys, depth + 1))
        }
        Value::Array(items) => {
            let out: Vec<String> = items
                .iter()
                .filter_map(|v| structured_answers(v, keys, depth + 1))
                .flatten()
                .collect();
            (!out.is_empty()).then_some(out)
        }
        Value::Object(map) => {
            for wrapper in ["content", "text"] {
                if let Some(result) = map
                    .get(wrapper)
                    .and_then(|v| structured_answers(v, keys, depth + 1))
                {
                    return Some(result);
                }
            }
            let answers = map.get("answers").and_then(Value::as_object).unwrap_or(map);
            let explicit_answers = map.get("answers").and_then(Value::as_object).is_some();
            let mut out = Vec::new();
            for (key, value) in answers {
                if !explicit_answers && !keys.contains(key) && !key.starts_with("q_") {
                    continue;
                }
                match value {
                    Value::String(s) if !s.trim().is_empty() => out.push(s.trim().to_string()),
                    Value::Array(items) => out.extend(
                        items
                            .iter()
                            .filter_map(Value::as_str)
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .map(str::to_string),
                    ),
                    _ => {}
                }
            }
            (!out.is_empty()).then_some(out)
        }
        _ => None,
    }
}

#[derive(Default)]
struct PendingAssistant {
    content: ParsedContent,
    thinking: Vec<String>,
    tool_calls: Vec<ToolCallView>,
    timestamp: Option<i64>,
    model: Option<String>,
    /// 已有工具结果回填过:再来的 assistant 侧行属于下一次模型调用,先 flush
    results_seen: bool,
}

struct ParseResult {
    messages: Vec<TranscriptMessage>,
    custom_title: String,
    ai_title: String,
    topic: String,
    cwd: String,
    model: Option<String>,
    tokens_used: i64,
    usage_by_request: HashMap<String, i64>,
    created_at: i64,
    updated_at: i64,
    unknown_lines: u32,
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

/// assistant 侧的行(reasoning / message / function_call)落到哪条助手消息:
/// 上一条已经收过工具结果就另起,否则续写当前这条。会话级的模型与 token
/// 累加也在这里做——三种行都带 providerData,只认一次
fn assistant_slot<'a>(
    out: &mut ParseResult,
    pending: &'a mut Option<PendingAssistant>,
    tool_index: &mut HashMap<String, (usize, usize)>,
    ts: i64,
    row: &Value,
) -> &'a mut PendingAssistant {
    let model = row_model(row);
    if model.is_some() {
        out.model = model.clone();
    }
    let usage = row_usage(row);
    out.tokens_used += usage;
    if usage > 0 {
        if let Some(id) = row
            .pointer("/providerData/conversationRequestId")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            *out.usage_by_request.entry(id.to_string()).or_default() += usage;
        }
    }
    if pending.as_ref().is_some_and(|p| p.results_seen) {
        flush_assistant(pending, &mut out.messages, tool_index);
    }
    let p = pending.get_or_insert_with(PendingAssistant::default);
    if p.timestamp.is_none() && ts > 0 {
        p.timestamp = Some(ts);
    }
    if model.is_some() {
        p.model = model;
    }
    p
}

fn parse_codebuddy_jsonl(path: &Path, decode_images: bool) -> Result<ParseResult> {
    let _budget = transcript_image_decode_budget(decode_images);
    let file = fs::File::open(path)?;
    let reader = BufReader::with_capacity(1 << 20, file);

    let mut out = ParseResult {
        messages: Vec::new(),
        custom_title: String::new(),
        ai_title: String::new(),
        topic: String::new(),
        cwd: String::new(),
        model: None,
        tokens_used: 0,
        usage_by_request: HashMap::new(),
        created_at: 0,
        updated_at: 0,
        unknown_lines: 0,
    };
    let mut tool_index: HashMap<String, (usize, usize)> = HashMap::new();
    let mut pending: Option<PendingAssistant> = None;
    let mut answered_calls = std::collections::HashSet::new();
    let mut last_synthetic_answer: Option<usize> = None;

    for row in jsonl_values(reader) {
        let Some(row) = row? else {
            out.unknown_lines += 1;
            continue;
        };
        let typ = row.get("type").and_then(Value::as_str).unwrap_or("");

        // 标题与元数据行不参与时间跨度(只有内容行定义会话首尾)
        match typ {
            "custom-title" => {
                if let Some(t) = optional_string(row.get("customTitle")) {
                    out.custom_title = t;
                }
                continue;
            }
            "ai-title" | "topic" => {
                let field = if typ == "topic" { "topic" } else { "aiTitle" };
                if let Some(t) =
                    optional_string(row.get(field)).filter(|t| !is_placeholder_title(t))
                {
                    if typ == "topic" {
                        out.topic = t;
                    } else {
                        out.ai_title = t;
                    }
                }
                continue;
            }
            _ if KNOWN_SKIP.contains(&typ) => continue,
            _ => {}
        }

        if out.cwd.is_empty() {
            if let Some(c) = row.get("cwd").and_then(Value::as_str) {
                if !c.is_empty() {
                    out.cwd = c.to_string();
                }
            }
        }
        let ts = row.get("timestamp").map(to_epoch_ms).unwrap_or(0);

        match typ {
            "message" => {
                match row.get("role").and_then(Value::as_str).unwrap_or("") {
                    "user" => {
                        flush_assistant(&mut pending, &mut out.messages, &mut tool_index);
                        let ParsedContent { text, images } = content_of(&row, decode_images);
                        if text.is_empty() && images.is_empty() {
                            continue;
                        }
                        let mut msg = text_msg(Role::User, &text, ts);
                        msg.images = images;
                        if last_synthetic_answer.take().is_some_and(|ix| {
                            ix + 1 == out.messages.len()
                                && out.messages[ix].text == msg.text
                                && msg.images.is_empty()
                        }) {
                            *out.messages.last_mut().unwrap() = msg;
                        } else {
                            out.messages.push(msg);
                        }
                    }
                    "assistant" => {
                        let p = assistant_slot(&mut out, &mut pending, &mut tool_index, ts, &row);
                        p.content.append(content_of(&row, decode_images));
                    }
                    "system" => {
                        // 系统行折叠为 Meta,不参与计数
                        flush_assistant(&mut pending, &mut out.messages, &mut tool_index);
                        let text = content_of(&row, false).text;
                        if text.is_empty() {
                            continue;
                        }
                        let mut msg = text_msg(Role::System, &text, ts);
                        msg.kind = MessageKind::Meta;
                        out.messages.push(msg);
                    }
                    _ => {
                        out.unknown_lines += 1;
                        continue;
                    }
                }
            }
            "reasoning" => {
                let p = assistant_slot(&mut out, &mut pending, &mut tool_index, ts, &row);
                // rawContent 是 reasoning_text 块;content 通常为空数组,个别写端
                // 把思考放在 content 的 text 块里,两处都收
                for key in ["rawContent", "content"] {
                    let Some(parts) = row.get(key).and_then(Value::as_array) else {
                        continue;
                    };
                    p.thinking.extend(
                        parts
                            .iter()
                            .filter_map(|part| optional_string(part.get("text"))),
                    );
                }
            }
            "function_call" => {
                let call_id = row
                    .get("callId")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let name = row.get("name").and_then(Value::as_str).unwrap_or("");
                // arguments 是 JSON 字符串;解不开就原样当字符串给预览
                let arguments = decoded_arguments(row.get("arguments").unwrap_or(&Value::Null));
                let p = assistant_slot(&mut out, &mut pending, &mut tool_index, ts, &row);
                p.tool_calls
                    .push(tool_call_view(call_id, name, &arguments, None, false));
            }
            "function_call_result" => {
                let call_id = row.get("callId").and_then(Value::as_str).unwrap_or("");
                // 先找目的地(未 flush 的当前助手消息优先,否则查已落盘的索引),
                // 找不到的孤儿结果连正文都不解
                let in_pending = pending.as_mut().and_then(|p| {
                    let ix = p.tool_calls.iter().position(|tc| tc.id == call_id)?;
                    p.results_seen = true;
                    Some(&mut p.tool_calls[ix])
                });
                let target = match in_pending {
                    Some(tc) => Some(tc),
                    None => tool_index
                        .get(call_id)
                        .map(|&(mi, ti)| &mut out.messages[mi].tool_calls[ti]),
                };
                let Some(tc) = target else { continue };
                let is_question = tc.name == "AskUserQuestion"
                    || row.get("name").and_then(Value::as_str) == Some("AskUserQuestion");
                // 与 grok / opencode 同款正向判错:只认已知的失败值,别的状态
                // (缺省、in_progress……)都不算错
                tc.is_error = matches!(
                    row.get("status").and_then(Value::as_str),
                    Some("failed" | "error")
                ) || row.get("isError").and_then(Value::as_bool) == Some(true);
                // 工具结果里的图片只占位不解码(CodeBuddy 工具结果暂无图片形态佐证)
                let output = tool_result_parts(row.get("output").unwrap_or(&Value::Null), false);
                tc.output = (!output.text.is_empty()).then(|| clip(&output.text, MAX_TOOL_IO).0);
                let answers = if is_question && !tc.is_error && !answered_calls.contains(call_id) {
                    ask_user_answers(
                        row.get("output").unwrap_or(&Value::Null),
                        tc.input.as_deref(),
                    )
                } else {
                    Vec::new()
                };
                if !answers.is_empty() {
                    answered_calls.insert(call_id.to_string());
                    // Put the synthetic user message at the result position in
                    // both index and detail parsing, so search jumps to it.
                    flush_assistant(&mut pending, &mut out.messages, &mut tool_index);
                    out.messages
                        .push(text_msg(Role::User, &answers.join("\n"), ts));
                    last_synthetic_answer = Some(out.messages.len() - 1);
                }
            }
            _ => {
                out.unknown_lines += 1;
                continue;
            }
        }

        if ts > 0 {
            if out.created_at == 0 || ts < out.created_at {
                out.created_at = ts;
            }
            if ts > out.updated_at {
                out.updated_at = ts;
            }
        }
    }
    flush_assistant(&mut pending, &mut out.messages, &mut tool_index);
    assign_seq(&mut out.messages);
    Ok(out)
}

fn build_meta(agent: AgentId, r: &SessionFileRef, parsed: &ParseResult) -> SessionMeta {
    let title = [
        parsed.custom_title.as_str(),
        parsed.ai_title.as_str(),
        parsed.topic.as_str(),
    ]
    .into_iter()
    .map(clean_title_candidate)
    .find(|title| !title.is_empty())
    .or_else(|| title_from_messages(&parsed.messages))
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
        updated_at: if parsed.updated_at > 0 {
            parsed.updated_at
        } else {
            r.mtime_ms
        },
        message_count: parsed
            .messages
            .iter()
            .filter(|m| m.kind == MessageKind::Text)
            .count() as i64,
        size_bytes: r.size,
        git_branch: None,
        model: parsed.model.clone(),
        tokens_used: (parsed.tokens_used > 0).then_some(parsed.tokens_used),
        archived: false,
        source: None,
        spawned: false,
        favorite: false,
        pinned: false,
    }
}

impl CodebuddyAdapter {
    /// 会话或子代理(子代理是 agent 派出去的,标题取边车记的任务说明)
    fn parse(&self, r: &SessionFileRef, decode_images: bool) -> Result<(SessionMeta, ParseResult)> {
        let _guard = self
            .catalog
            .as_ref()
            .filter(|c| c.host.is_some())
            .map(|_| super::codebuddy_legacy::REMOTE_CACHE_SWITCH.read().unwrap());
        let path = Path::new(&r.file_path);
        if self
            .catalog
            .as_ref()
            .is_some_and(|c| c.protect_missing && !c.path.is_file())
            && has_conversion_marker(path)
        {
            anyhow::bail!("CodeBuddy conversation catalog is temporarily unavailable");
        }
        let parsed = parse_codebuddy_jsonl(path, decode_images)?;
        // Catalog-aware references use mtime_ms as an opaque fingerprint. It
        // must never be presented as a date when the transcript lacks times.
        let mut source_ref = r.clone();
        source_ref.mtime_ms = self.copy_mtime(r);
        let mut meta = build_meta(self.agent, &source_ref, &parsed);
        if let Some(catalog) = &self.catalog {
            let rows = catalog.rows()?;
            let runtime = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or(&r.native_id);
            if let Some(row) = catalog.row(path, runtime, &rows) {
                if row.deleted {
                    anyhow::bail!("CodeBuddy session was deleted in its catalog");
                }
                if !is_subagent_transcript(&r.file_path) {
                    meta.archived = row.archived;
                    if parsed.created_at == 0 && row.created_at > 0 {
                        meta.created_at = row.created_at;
                    }
                    if parsed.updated_at == 0 && row.updated_at > 0 {
                        meta.updated_at = row.updated_at;
                    }
                    if catalog.converted(row, path) {
                        // An empty/corrupt CBC file is not a successful winner:
                        // let scanner fall back to the original 4.x transcript.
                        if !parsed.messages.iter().any(|message| {
                            message.kind == MessageKind::Text
                                && matches!(message.role, Role::User | Role::Assistant)
                                && (!message.text.trim().is_empty()
                                    || !message.tool_calls.is_empty()
                                    || !message.images.is_empty())
                        }) {
                            anyhow::bail!(
                                "converted CodeBuddy transcript has no readable messages"
                            );
                        }
                        meta.key = session_key(self.agent, "", &row.id);
                        meta.source = Some(super::codebuddy_legacy::LEGACY_SOURCE.into());
                        // Resume needs the CBC runtime ID, which may differ from
                        // the logical catalog ID used for de-duplication.
                        meta.id = runtime.to_string();
                        if !row.custom_title.trim().is_empty() {
                            meta.title = clean_title_candidate(&row.custom_title);
                        }
                        let old = catalog
                            .legacy_index(row)
                            .and_then(|p| {
                                p.parent().and_then(|dir| {
                                    super::codebuddy_legacy::read_session_file(dir, &p).ok()
                                })
                            })
                            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
                            .and_then(|v| super::codebuddy_legacy::request_usage(&v));
                        meta.tokens_used = old
                            .map(|old| {
                                let total_old =
                                    old.values().fold(0_i64, |a, b| a.saturating_add(*b));
                                let duplicate = parsed
                                    .usage_by_request
                                    .iter()
                                    .filter(|(id, _)| old.contains_key(*id))
                                    .fold(0_i64, |a, (_, b)| a.saturating_add(*b));
                                total_old
                                    .saturating_add(parsed.tokens_used.saturating_sub(duplicate))
                            })
                            .or(meta.tokens_used);
                    }
                }
            }
        }
        mark_subagent(&mut meta, path);
        Ok((meta, parsed))
    }
}

impl AgentAdapter for CodebuddyAdapter {
    fn agent(&self) -> AgentId {
        self.agent
    }

    fn copy_mtime(&self, r: &SessionFileRef) -> i64 {
        if self.catalog.is_some() {
            fs::metadata(&r.file_path)
                .map(|m| mtime_ms(&m))
                .unwrap_or(0)
        } else {
            r.mtime_ms
        }
    }

    fn scan_priority(&self, r: &SessionFileRef) -> i64 {
        self.copy_mtime(r)
    }

    fn begin_scan(&self) {
        if let Some(catalog) = &self.catalog {
            catalog.cache.clear();
        }
    }

    fn native_id_aliases(&self, r: &SessionFileRef) -> Vec<String> {
        let Some(catalog) = &self.catalog else {
            return Vec::new();
        };
        let _guard = catalog
            .host
            .as_ref()
            .map(|_| super::codebuddy_legacy::REMOTE_CACHE_SWITCH.read().unwrap());
        let Ok(rows) = catalog.rows() else {
            return Vec::new();
        };
        let path = Path::new(&r.file_path);
        let Some(runtime) = path.file_stem().and_then(|s| s.to_str()) else {
            return Vec::new();
        };
        match catalog.row(path, runtime, &rows) {
            Some(row)
                if row.id != runtime && r.native_id == row.id && catalog.converted(row, path) =>
            {
                vec![runtime.to_string()]
            }
            _ => Vec::new(),
        }
    }

    fn list_session_files(&self) -> Result<Vec<SessionFileRef>> {
        let _guard = self
            .catalog
            .as_ref()
            .filter(|c| c.host.is_some())
            .map(|_| super::codebuddy_legacy::REMOTE_CACHE_SWITCH.read().unwrap());
        let refs = list_project_tree_refs(&self.root, self.agent);
        if self
            .catalog
            .as_ref()
            .is_some_and(|c| c.protect_missing && !c.path.is_file())
            && refs
                .iter()
                .any(|r| has_conversion_marker(Path::new(&r.file_path)))
        {
            anyhow::bail!("CodeBuddy conversation catalog is temporarily unavailable");
        }
        match &self.catalog {
            Some(catalog) => {
                let rows = catalog.rows()?;
                Ok(refs
                    .into_iter()
                    .filter_map(|r| catalog.reference(r, &rows))
                    .collect())
            }
            None => Ok(refs),
        }
    }

    fn file_ref(&self, path: &Path) -> Option<SessionFileRef> {
        let _guard = self
            .catalog
            .as_ref()
            .filter(|c| c.host.is_some())
            .map(|_| super::codebuddy_legacy::REMOTE_CACHE_SWITCH.read().unwrap());
        let r = project_tree_file_ref(&self.root, self.agent, path)?;
        if self
            .catalog
            .as_ref()
            .is_some_and(|c| c.protect_missing && !c.path.is_file())
            && has_conversion_marker(path)
        {
            return None;
        }
        match &self.catalog {
            Some(catalog) => catalog.reference(r, &catalog.rows().ok()?),
            None => Some(r),
        }
    }

    fn parse_session(&self, r: &SessionFileRef) -> Result<ParsedSession> {
        let (meta, parsed) = self.parse(r, false)?;
        Ok(ParsedSession::derive(
            meta,
            &parsed.messages,
            parsed.unknown_lines,
        ))
    }

    fn parse_transcript(&self, r: &SessionFileRef) -> Result<ParsedTranscript> {
        let (meta, parsed) = self.parse(r, true)?;
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
            Ok(parse_codebuddy_jsonl(file, true)?.messages)
        })
    }

    /// 子代理挂到派它的会话下面:所在会话目录的名字就是那个会话的 id
    fn parent_in_path(&self) -> Option<super::ParentInPath> {
        Some(subagent_parent)
    }

    fn parent_id_for_path(&self, path: &Path) -> Option<String> {
        let runtime = subagent_parent(path)?;
        let Some(catalog) = &self.catalog else {
            return Some(runtime);
        };
        let _guard = catalog
            .host
            .as_ref()
            .map(|_| super::codebuddy_legacy::REMOTE_CACHE_SWITCH.read().unwrap());
        let rows = catalog.rows().ok()?;
        let parent_file = subagent_session_dir(path)?
            .parent()?
            .join(format!("{runtime}.jsonl"));
        match catalog.row(path, &runtime, &rows) {
            Some(row) if catalog.converted(row, &parent_file) => Some(row.id.clone()),
            _ => Some(runtime),
        }
    }

    fn parent_links(&self) -> Option<Vec<(String, String)>> {
        match &self.catalog {
            Some(catalog) => catalog.rows().ok().map(|_| Vec::new()),
            None => Some(Vec::new()),
        }
    }

    fn session_paths(&self, meta: &SessionMeta) -> Vec<String> {
        if super::codebuddy_legacy::is_imported_legacy(meta) {
            return Vec::new();
        }
        if is_subagent_transcript(&meta.file_path) {
            return subagent_paths(&meta.file_path);
        }
        let mut paths = vec![meta.file_path.clone()];
        if let Some(parent) = Path::new(&meta.file_path).parent() {
            // <id>.meta.json 边车与 <id>/(subagents 等)随会话一并进废纸篓
            let meta_file = parent.join(format!("{}.meta.json", meta.id));
            if meta_file.is_file() {
                paths.push(meta_file.to_string_lossy().to_string());
            }
            let sidecar = parent.join(&meta.id);
            if sidecar.is_dir() {
                paths.push(sidecar.to_string_lossy().to_string());
            }
        }
        paths
    }

    fn cleanup_paths(&self, meta: &SessionMeta) -> Option<Vec<String>> {
        (!super::codebuddy_legacy::is_imported_legacy(meta)).then(|| self.session_paths(meta))
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
            catalog: if self.agent == AgentId::Codebuddy {
                home.as_ref().map(|home| {
                    let mut catalog = SourceCatalog::new(
                        home.join("codebuddy.db"),
                        super::codebuddy_legacy::default_data_root(),
                        None,
                    );
                    catalog.protect_missing = false;
                    catalog
                })
            } else {
                None
            },
            home,
        })
    }

    fn with_remote_root(
        &self,
        dir: PathBuf,
        host_cache: &Path,
        host: &str,
    ) -> Box<dyn AgentAdapter> {
        if self.agent == AgentId::Codebuddy {
            Box::new(Self::remote(dir, host_cache, host))
        } else {
            self.with_custom_root(dir)
        }
    }

    fn data_roots(&self) -> Vec<PathBuf> {
        vec![self.root.clone()]
    }

    fn homes(&self) -> Vec<PathBuf> {
        self.home.iter().cloned().collect()
    }
}

#[cfg(test)]
mod compat_tests {
    use super::*;
    use crate::adapters::codebuddy_legacy::{
        legacy_relative_dir, CatalogRow, CodebuddyLegacyAdapter,
    };
    use crate::db::Store;
    use crate::scanner::{run_scan, NullEvents, ScanEvents, ScanProgress};
    use serde_json::json;

    struct OrderRecorder {
        store: Arc<Store>,
        first: Mutex<Option<String>>,
        notifications: std::sync::atomic::AtomicUsize,
    }
    impl ScanEvents for OrderRecorder {
        fn on_progress(&self, _: &ScanProgress) {}
        fn on_sessions_changed(&self) {
            self.notifications
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let mut first = self.first.lock().unwrap();
            if first.is_none() {
                for key in ["codebuddy:old-1", "codebuddy:new-2"] {
                    if self.store.get_session(key).unwrap().is_some() {
                        *first = Some(key.into());
                        break;
                    }
                }
            }
        }
    }

    fn write(path: &Path, value: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, value).unwrap();
    }

    fn catalog(db: &Path) {
        let db = rusqlite::Connection::open(db).unwrap();
        db.execute_batch(
            "CREATE TABLE conversations (
            id TEXT PRIMARY KEY, user_id TEXT, workspace TEXT, workspace_authority TEXT,
            title TEXT, custom_title TEXT, created_at INTEGER, last_message_at INTEGER,
            deleted_at INTEGER, archived_at INTEGER, runtime_provider TEXT,
            runtime_session_id TEXT, revision INTEGER);
            INSERT INTO conversations VALUES ('old-1','account-1','/tmp/project',NULL,
            'Legacy title',NULL,1700000000000,1700000001000,NULL,NULL,
            'codebuddy-ide-4','old-1',1);",
        )
        .unwrap();
    }

    fn row() -> CatalogRow {
        CatalogRow {
            id: "old-1".into(),
            user_id: "account-1".into(),
            workspace: "/tmp/project".into(),
            authority: String::new(),
            title: "Legacy title".into(),
            custom_title: String::new(),
            created_at: 1700000000000,
            updated_at: 1700000001000,
            deleted: false,
            archived: false,
            provider: "codebuddy-ide-4".into(),
            runtime_id: "old-1".into(),
            revision: 1,
        }
    }

    fn cbc_text() -> String {
        [
            json!({"type":"message","role":"user","content":[{"type":"input_text","text":"converted prompt"}],"timestamp":1700000000000_i64}),
            json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":"converted reply"}],
                "providerData":{"conversationRequestId":"r1","rawUsage":{"total_tokens":10}}}),
            json!({"type":"function_call","callId":"q1","name":"AskUserQuestion","arguments":"{}"}),
            json!({"type":"function_call_result","callId":"q1","name":"AskUserQuestion","output":{"type":"text","text":" · Which? → unique answer"}}),
            json!({"type":"message","role":"user","content":[{"type":"input_text","text":"unique answer"}]}),
            json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":"final response"}],
                "providerData":{"conversationRequestId":"r3","rawUsage":{"total_tokens":7}}}),
        ].into_iter().map(|v| v.to_string() + "\n").collect()
    }

    #[test]
    fn question_answer_formats_exclude_tool_status() {
        assert_eq!(
            ask_user_answers(&json!({"text":"Choose? -> plain answer"}), None),
            vec!["plain answer"]
        );
        assert_eq!(
            ask_user_answers(
                &json!({"text":"{\"q_0\":[\"first\",\"second\"]}"}),
                Some(r#"{"questions":[{"id":"q_0"}]}"#)
            ),
            vec!["first", "second"]
        );
        assert!(ask_user_answers(&json!({"status":"completed","text":"done"}), None).is_empty());
    }

    #[test]
    fn multiline_question_answers_preserve_continuations() {
        assert_eq!(
            ask_user_answers(
                &json!({"text": " · First? → first line\nsecond line\n\n  indented -> continuation\n · Next? →\nsecond answer"}),
                None
            ),
            vec![
                "first line\nsecond line\n\n  indented -> continuation",
                "second answer"
            ]
        );
    }

    #[test]
    fn answer_lists_with_arrows_are_not_questions() {
        let input = r#"{"questions":[{"question":"Explain?"},{"question":"Next?"}]}"#;
        assert_eq!(
            ask_user_answers(
                &json!({"text":" · Explain? → steps\n- 输入 -> 输出\n · 中间 → 结果\n  * a -> b\n · Next? → done"}),
                Some(input)
            ),
            vec!["steps\n- 输入 -> 输出\n · 中间 → 结果\n  * a -> b", "done"]
        );
        assert_eq!(
            ask_user_answers(&json!({"text":" · Explain? → steps\n- 输入 -> 输出"}), None),
            vec!["steps\n- 输入 -> 输出"]
        );
    }

    #[test]
    fn converted_usage_survives_unavailable_legacy_usage() {
        let tmp = tempfile::tempdir().unwrap();
        let catalog_db = tmp.path().join("codebuddy.db");
        catalog(&catalog_db);
        rusqlite::Connection::open(&catalog_db)
            .unwrap()
            .execute(
                "UPDATE conversations SET runtime_provider='cbc',runtime_session_id='runtime-5'",
                [],
            )
            .unwrap();
        let data = tmp.path().join("Data");
        let index = data
            .join(legacy_relative_dir(&row()).unwrap())
            .join("index.json");
        fs::create_dir_all(index.parent().unwrap()).unwrap();
        let projects = tmp.path().join("projects");
        write(&projects.join("demo/runtime-5.jsonl"), &cbc_text());
        let adapter = CodebuddyAdapter {
            agent: AgentId::Codebuddy,
            root: projects,
            home: None,
            catalog: Some(SourceCatalog::new(catalog_db, data, None)),
        };
        for content in [None, Some("invalid json"), Some("{}")] {
            if let Some(content) = content {
                write(&index, content);
            }
            let reference = adapter.list_session_files().unwrap().pop().unwrap();
            assert_eq!(
                adapter.parse_session(&reference).unwrap().meta.tokens_used,
                Some(17)
            );
            assert_eq!(
                adapter
                    .parse_transcript(&reference)
                    .unwrap()
                    .meta
                    .tokens_used,
                Some(17)
            );
        }
    }

    #[test]
    fn cli_only_install_keeps_cbc_sessions_without_ide_catalog() {
        let tmp = tempfile::tempdir().unwrap();
        let catalog = tmp.path().join("codebuddy.db");
        let data = tmp.path().join("Data");
        fs::create_dir_all(&data).unwrap();
        let projects = tmp.path().join("projects");
        let cbc = projects.join("demo/runtime-1.jsonl");
        write(&cbc, &cbc_text());
        let adapter = CodebuddyAdapter {
            agent: AgentId::Codebuddy,
            root: projects,
            home: None,
            catalog: Some(SourceCatalog::new(catalog.clone(), data, None)),
        };
        assert_eq!(adapter.list_session_files().unwrap().len(), 1);
        rusqlite::Connection::open(&catalog)
            .unwrap()
            .execute("CREATE TABLE cli_data(id TEXT)", [])
            .unwrap();
        adapter.begin_scan();
        assert_eq!(adapter.list_session_files().unwrap().len(), 1);
        fs::remove_file(&catalog).unwrap();
        adapter.begin_scan();
        assert!(
            adapter.list_session_files().is_err(),
            "a previously available catalog disappearing must freeze the index"
        );
    }

    #[test]
    fn catalog_snapshot_refreshes_when_only_the_wal_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("codebuddy.db");
        catalog(&path);
        let writer = rusqlite::Connection::open(&path).unwrap();
        writer.execute_batch("PRAGMA journal_mode=WAL;").unwrap();
        let source = SourceCatalog::new(path, tmp.path().join("data"), None);
        assert_eq!(source.rows().unwrap()[0].title, "Legacy title");
        writer.execute("UPDATE conversations SET title='Revised title',revision=revision+1 WHERE id='old-1'", []).unwrap();
        assert_eq!(source.rows().unwrap()[0].title, "Revised title");
    }

    #[test]
    fn restart_without_catalog_keeps_legacy_index_and_refreshes_independent_cbc() {
        let tmp = tempfile::tempdir().unwrap();
        let catalog_db = tmp.path().join("codebuddy.db");
        catalog(&catalog_db);
        let data = tmp.path().join("Data");
        let legacy = data.join(legacy_relative_dir(&row()).unwrap());
        write(&legacy.join("index.json"), r#"{"messages":[{"id":"m1"}]}"#);
        write(
            &legacy.join("messages/m1.json"),
            &json!({"role":"user", "message":{"content":[
                {"type":"text","text":"retained legacy phrase"}
            ]}})
            .to_string(),
        );
        let projects = tmp.path().join("projects");
        let cli = projects.join("demo/cli-only.jsonl");
        write(&cli, &cbc_text());
        let adapters = || -> Vec<Box<dyn AgentAdapter>> {
            vec![
                Box::new(CodebuddyAdapter {
                    agent: AgentId::Codebuddy,
                    root: projects.clone(),
                    home: None,
                    catalog: Some(SourceCatalog::new(catalog_db.clone(), data.clone(), None)),
                }),
                Box::new(CodebuddyLegacyAdapter::at(
                    catalog_db.clone(),
                    data.clone(),
                    None,
                )),
            ]
        };
        let store = Arc::new(Store::open(&tmp.path().join("wake.db")).unwrap());
        run_scan(&adapters(), &store, &NullEvents, false).unwrap();
        assert!(store.get_session("codebuddy:old-1").unwrap().is_some());

        fs::rename(&catalog_db, tmp.path().join("catalog.hold")).unwrap();
        write(
            &cli,
            &cbc_text().replace("converted prompt", "updated independent prompt"),
        );
        // Fresh adapters model an app restart, without a remembered catalog.
        run_scan(&adapters(), &store, &NullEvents, false).unwrap();
        assert!(store.get_session("codebuddy:old-1").unwrap().is_some());
        assert_eq!(
            store
                .search("retained legacy phrase", &[], None, 10)
                .unwrap()
                .0
                .len(),
            1
        );
        assert!(!store
            .search("updated independent prompt", &[], None, 10)
            .unwrap()
            .0
            .is_empty());
    }

    #[test]
    fn converted_transcript_without_timestamps_uses_catalog_times() {
        let tmp = tempfile::tempdir().unwrap();
        let catalog_db = tmp.path().join("codebuddy.db");
        catalog(&catalog_db);
        rusqlite::Connection::open(&catalog_db).unwrap().execute(
            "UPDATE conversations SET runtime_provider='cbc',runtime_session_id='runtime-5' WHERE id='old-1'", [],
        ).unwrap();
        let data = tmp.path().join("Data");
        fs::create_dir_all(data.join(legacy_relative_dir(&row()).unwrap())).unwrap();
        let projects = tmp.path().join("projects");
        write(
            &projects.join("demo/runtime-5.jsonl"),
            &format!(
                "{}\n",
                json!({
                    "type":"message", "role":"user", "content":[{"type":"input_text","text":"no timestamp"}]
                })
            ),
        );
        let adapter = CodebuddyAdapter {
            agent: AgentId::Codebuddy,
            root: projects,
            home: None,
            catalog: Some(SourceCatalog::new(catalog_db, data, None)),
        };
        let reference = adapter.list_session_files().unwrap().pop().unwrap();
        let indexed = adapter.parse_session(&reference).unwrap().meta;
        assert_eq!(indexed.created_at, 1700000000000);
        assert_eq!(indexed.updated_at, 1700000001000);
        let detail = adapter
            .parse_transcript(&SessionFileRef::from_meta(&indexed))
            .unwrap();
        assert_eq!(detail.meta.created_at, indexed.created_at);
        assert_eq!(detail.meta.updated_at, indexed.updated_at);
    }

    #[test]
    fn alias_migration_keeps_cached_search_when_both_sources_are_unreadable() {
        let tmp = tempfile::tempdir().unwrap();
        let catalog_db = tmp.path().join("codebuddy.db");
        catalog(&catalog_db);
        let data = tmp.path().join("Data");
        let projects = tmp.path().join("projects");
        let cbc_path = projects.join("demo/runtime-5.jsonl");
        write(
            &cbc_path,
            &cbc_text().replace("converted prompt", "converted prompt 中文检索词"),
        );
        let child_path = projects.join("demo/runtime-5/subagents/agent-child.jsonl");
        write(
            &child_path,
            &(json!({"type":"message", "role":"assistant",
            "content":[{"type":"output_text","text":"child result"}]})
            .to_string()
                + "\n"),
        );
        let cbc = CodebuddyAdapter {
            agent: AgentId::Codebuddy,
            root: projects,
            home: None,
            catalog: Some(SourceCatalog::new(catalog_db.clone(), data.clone(), None)),
        };
        let reference = cbc.file_ref(&cbc_path).unwrap();
        let existing = cbc.parse_session(&reference).unwrap();
        let store = Arc::new(Store::open(&tmp.path().join("wake.db")).unwrap());
        store
            .write_session(&existing.meta, reference.mtime_ms, &existing.units)
            .unwrap();
        store
            .set_user_data("codebuddy:runtime-5", Some(true), Some(true))
            .unwrap();
        let child_ref = cbc.file_ref(&child_path).unwrap();
        let child = cbc.parse_session(&child_ref).unwrap();
        store
            .write_session(&child.meta, child_ref.mtime_ms, &child.units)
            .unwrap();
        store
            .replace_parent_links(
                AgentId::Codebuddy,
                &[(child.meta.key, existing.meta.key.clone())],
            )
            .unwrap();

        fs::create_dir_all(data.join(legacy_relative_dir(&row()).unwrap())).unwrap();
        rusqlite::Connection::open(&catalog_db).unwrap().execute(
            "UPDATE conversations SET runtime_provider='cbc',runtime_session_id='runtime-5' WHERE id='old-1'", [],
        ).unwrap();
        write(&cbc_path, "{bad json}\n");
        let adapters: Vec<Box<dyn AgentAdapter>> = vec![
            Box::new(cbc),
            Box::new(CodebuddyLegacyAdapter::at(catalog_db, data, None)),
        ];
        run_scan(&adapters, &store, &NullEvents, false).unwrap();
        let retained = store
            .get_session("codebuddy:old-1")
            .unwrap()
            .expect("cached session retained");
        assert!(retained.favorite && retained.pinned);
        let hits = store.search("unique answer", &[], None, 10).unwrap().0;
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].session.key, "codebuddy:old-1");
        let title_and_text = store.search("converted prompt", &[], None, 10).unwrap().0;
        assert!(!title_and_text.is_empty());
        assert!(title_and_text
            .iter()
            .all(|hit| hit.session.key == "codebuddy:old-1"));
        let cjk = store.search("检索", &[], None, 10).unwrap().0;
        assert!(!cjk.is_empty());
        assert!(cjk.iter().all(|hit| hit.session.key == "codebuddy:old-1"));
        assert_eq!(
            store
                .parent_key_of("codebuddy:agent-child")
                .unwrap()
                .as_deref(),
            Some("codebuddy:old-1")
        );
        assert!(store.get_session("codebuddy:runtime-5").unwrap().is_none());
    }

    #[test]
    fn alias_migration_keeps_a_previously_hidden_session_hidden() {
        let tmp = tempfile::tempdir().unwrap();
        let catalog_db = tmp.path().join("codebuddy.db");
        catalog(&catalog_db);
        let data = tmp.path().join("Data");
        let projects = tmp.path().join("projects");
        let cbc_path = projects.join("demo/runtime-5.jsonl");
        write(&cbc_path, &cbc_text());
        let cbc = CodebuddyAdapter {
            agent: AgentId::Codebuddy,
            root: projects,
            home: None,
            catalog: Some(SourceCatalog::new(catalog_db.clone(), data.clone(), None)),
        };
        let reference = cbc.list_session_files().unwrap().pop().unwrap();
        let indexed = cbc.parse_session(&reference).unwrap();
        let store = Arc::new(Store::open(&tmp.path().join("wake.db")).unwrap());
        store
            .write_session(&indexed.meta, reference.mtime_ms, &indexed.units)
            .unwrap();
        store
            .set_user_data("codebuddy:runtime-5", Some(true), Some(true))
            .unwrap();
        store
            .remove_trashed_sessions(&["codebuddy:runtime-5".into()], &HashMap::new())
            .unwrap();

        fs::create_dir_all(data.join(legacy_relative_dir(&row()).unwrap())).unwrap();
        rusqlite::Connection::open(&catalog_db).unwrap().execute(
            "UPDATE conversations SET runtime_provider='cbc',runtime_session_id='runtime-5' WHERE id='old-1'", [],
        ).unwrap();
        let adapters: Vec<Box<dyn AgentAdapter>> = vec![
            Box::new(cbc),
            Box::new(CodebuddyLegacyAdapter::at(catalog_db, data, None)),
        ];
        run_scan(&adapters, &store, &NullEvents, false).unwrap();
        assert!(store.get_session("codebuddy:old-1").unwrap().is_none());
        assert!(store.is_key_tombstoned("codebuddy:old-1"));
        let groups = crate::cleanup::hidden_sessions(&store, &adapters).unwrap();
        assert_eq!(groups.len(), 1);
        crate::cleanup::unhide(&store, &adapters, groups.into_iter().next().unwrap()).unwrap();
        let restored = store.get_session("codebuddy:old-1").unwrap().unwrap();
        assert!(restored.favorite && restored.pinned);
        assert!(cbc_path.is_file());
    }

    #[test]
    fn converted_child_uses_catalog_parent_and_hide_preserves_its_file() {
        let tmp = tempfile::tempdir().unwrap();
        let catalog_db = tmp.path().join("codebuddy.db");
        catalog(&catalog_db);
        rusqlite::Connection::open(&catalog_db).unwrap().execute(
            "UPDATE conversations SET runtime_provider='cbc',runtime_session_id='runtime-5' WHERE id='old-1'", [],
        ).unwrap();
        let data = tmp.path().join("Data");
        fs::create_dir_all(data.join(legacy_relative_dir(&row()).unwrap())).unwrap();
        let projects = tmp.path().join("projects");
        write(&projects.join("demo/runtime-5.jsonl"), &cbc_text());
        let child_path = projects.join("demo/runtime-5/subagents/agent-child.jsonl");
        write(
            &child_path,
            &(json!({"type":"message", "role":"assistant",
            "content":[{"type":"output_text","text":"child result"}]})
            .to_string()
                + "\n"),
        );
        let adapters: Vec<Box<dyn AgentAdapter>> = vec![
            Box::new(CodebuddyAdapter {
                agent: AgentId::Codebuddy,
                root: projects,
                home: None,
                catalog: Some(SourceCatalog::new(catalog_db.clone(), data.clone(), None)),
            }),
            Box::new(CodebuddyLegacyAdapter::at(catalog_db, data, None)),
        ];
        let store = Arc::new(Store::open(&tmp.path().join("wake.db")).unwrap());
        run_scan(&adapters, &store, &NullEvents, false).unwrap();
        let parent = store.get_session("codebuddy:old-1").unwrap().unwrap();
        assert_eq!(
            store
                .parent_key_of("codebuddy:agent-child")
                .unwrap()
                .as_deref(),
            Some(parent.key.as_str())
        );
        let plan = crate::cleanup::delete_plan(&store, &adapters, &parent).unwrap();
        assert_eq!(plan.sessions.len(), 2);
        assert!(
            plan.targets.is_empty(),
            "hiding an imported tree must not move any child transcript"
        );
        assert!(crate::cleanup::trash_sessions(&store, &plan.sessions, &plan.targets).unwrap());
        assert!(child_path.is_file());
        assert!(store
            .get_session("codebuddy:agent-child")
            .unwrap()
            .is_none());
    }

    #[test]
    fn converted_history_switches_sources_and_keeps_flags_and_usage() {
        let tmp = tempfile::tempdir().unwrap();
        let catalog_db = tmp.path().join("codebuddy.db");
        catalog(&catalog_db);
        let data = tmp.path().join("Data");
        let legacy = data.join(legacy_relative_dir(&row()).unwrap());
        let index = json!({"messages":[{"id":"m1"}],"requests":[
            {"id":"r1","usage":{"inputTokens":7,"outputTokens":3,"totalTokens":10}},
            {"id":"r2","usage":{"inputTokens":4,"outputTokens":1,"totalTokens":5}}
        ]});
        write(&legacy.join("index.json"), &index.to_string());
        write(
            &legacy.join("messages/m1.json"),
            &json!({"role":"user", "message":json!({
            "content":[{"type":"text","text":"legacy prompt"}]}).to_string(), "extra":"{}"})
            .to_string(),
        );
        let projects = tmp.path().join("projects");
        let cbc_path = projects.join("project/runtime-5.jsonl");
        let cbc = CodebuddyAdapter {
            agent: AgentId::Codebuddy,
            root: projects,
            home: None,
            catalog: Some(SourceCatalog::new(catalog_db.clone(), data.clone(), None)),
        };
        let old = CodebuddyLegacyAdapter::at(catalog_db.clone(), data.clone(), None);
        let adapters: Vec<Box<dyn AgentAdapter>> = vec![Box::new(cbc), Box::new(old)];
        let store = Arc::new(Store::open(&tmp.path().join("wake.db")).unwrap());
        run_scan(&adapters, &store, &NullEvents, false).unwrap();
        let key = "codebuddy:old-1";
        let first = store.get_session(key).unwrap().unwrap();
        assert_eq!(first.tokens_used, Some(15));
        write(
            &legacy.join("messages/m1.json"),
            &json!({"role":"user", "message":json!({
            "content":[{"type":"text","text":"revised legacy prompt"}]}).to_string(), "extra":"{}"})
            .to_string(),
        );
        run_scan(&adapters, &store, &NullEvents, false).unwrap();
        assert_eq!(
            store
                .search("revised legacy prompt", &[], None, 10)
                .unwrap()
                .0
                .len(),
            1
        );
        store.set_user_data(key, Some(true), Some(true)).unwrap();
        let plan = crate::cleanup::delete_plan(&store, &adapters, &first).unwrap();
        assert!(plan.targets.is_empty());

        let mut newer = row();
        newer.id = "new-2".into();
        newer.runtime_id = "new-2".into();
        rusqlite::Connection::open(&catalog_db)
            .unwrap()
            .execute(
                "INSERT INTO conversations VALUES ('new-2','account-1','/tmp/project',NULL,
             'New import',NULL,1700000000000,1700000002000,NULL,NULL,'codebuddy-ide-4','new-2',1)",
                [],
            )
            .unwrap();
        let newer_dir = data.join(legacy_relative_dir(&newer).unwrap());
        write(&newer_dir.join("index.json"), &index.to_string());
        write(
            &newer_dir.join("messages/m1.json"),
            &json!({"role":"user", "message":json!({
            "content":[{"type":"text","text":"newly imported"}]}).to_string(), "extra":"{}"})
            .to_string(),
        );
        let isolated = Arc::new(Store::open(&tmp.path().join("order.db")).unwrap());
        let order = OrderRecorder {
            store: isolated.clone(),
            first: Mutex::new(None),
            notifications: std::sync::atomic::AtomicUsize::new(0),
        };
        run_scan(&adapters, &isolated, &order, false).unwrap();
        assert_eq!(
            order.first.lock().unwrap().as_deref(),
            Some("codebuddy:new-2")
        );
        assert!(
            order
                .notifications
                .load(std::sync::atomic::Ordering::Relaxed)
                <= 2,
            "a batch should not send a separate UI refresh for every new session"
        );
        run_scan(&adapters, &store, &NullEvents, false).unwrap();

        rusqlite::Connection::open(&catalog_db).unwrap().execute(
            "UPDATE conversations SET runtime_provider='cbc',runtime_session_id='runtime-5',revision=2 WHERE id='old-1'", [],
        ).unwrap();
        write(&cbc_path, "{bad json}\n");
        let roots = vec![(adapters[0].data_roots()[0].clone(), 0)];
        let event = || {
            vec![Ok(notify::Event::new(notify::EventKind::Create(
                notify::event::CreateKind::File,
            ))
            .add_path(cbc_path.clone()))]
        };
        crate::watcher::process_batch(&adapters, &store, &NullEvents, &roots, event());
        assert_eq!(
            store.get_session(key).unwrap().unwrap().file_path,
            legacy.join("index.json").to_string_lossy()
        );
        write(&cbc_path, &cbc_text());
        crate::watcher::process_batch(&adapters, &store, &NullEvents, &roots, event());
        assert_eq!(
            store.get_session(key).unwrap().unwrap().file_path,
            cbc_path.to_string_lossy()
        );
        let live_row = json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":"watcher live update"}]}).to_string();
        write(&cbc_path, &(cbc_text() + &live_row + "\n"));
        let modified = vec![Ok(notify::Event::new(notify::EventKind::Modify(
            notify::event::ModifyKind::Any,
        ))
        .add_path(cbc_path.clone()))];
        crate::watcher::process_batch(&adapters, &store, &NullEvents, &roots, modified);
        assert_eq!(
            store
                .search("watcher live update", &[], None, 10)
                .unwrap()
                .0
                .len(),
            1
        );
        // Restore the old winner to model an index created before upgrade.
        let restored_old = adapters[1]
            .parse_session(&adapters[1].file_ref(&legacy.join("index.json")).unwrap())
            .unwrap();
        store
            .write_session(&restored_old.meta, 1, &restored_old.units)
            .unwrap();
        // An older Wake release keyed an already-indexed CBC copy by runtime
        // ID. Its flags must move to the catalog identity when IDs differ.
        let mut previously_indexed = first.clone();
        previously_indexed.key = "codebuddy:runtime-5".into();
        previously_indexed.id = "runtime-5".into();
        previously_indexed.file_path = cbc_path.to_string_lossy().into_owned();
        store.write_session(&previously_indexed, 1, &[]).unwrap();
        store
            .set_user_data("codebuddy:runtime-5", Some(true), Some(true))
            .unwrap();
        run_scan(&adapters, &store, &NullEvents, false).unwrap();
        let converted = store.get_session(key).unwrap().unwrap();
        assert!(store.get_session("codebuddy:runtime-5").unwrap().is_none());
        assert_eq!(converted.id, "runtime-5");
        assert!(converted.favorite && converted.pinned);
        assert_eq!(converted.tokens_used, Some(22));
        assert_eq!(converted.file_path, cbc_path.to_string_lossy());
        let (hits, _) = store.search("unique answer", &[], None, 10).unwrap();
        assert_eq!(hits.len(), 1);
        let r = SessionFileRef::from_meta(&converted);
        let transcript = adapters[0].parse_transcript(&r).unwrap();
        assert_eq!(
            transcript.mainline[hits[0].seq as usize].text,
            "unique answer"
        );
        let plan = crate::cleanup::delete_plan(&store, &adapters, &converted).unwrap();
        assert!(plan.targets.is_empty());
        assert!(crate::cleanup::trash_sessions(&store, &plan.sessions, &plan.targets).unwrap());
        run_scan(&adapters, &store, &NullEvents, false).unwrap();
        assert!(store.get_session(key).unwrap().is_none());
        let hidden = crate::cleanup::hidden_sessions(&store, &adapters).unwrap();
        assert_eq!(hidden.len(), 1);
        crate::cleanup::unhide(&store, &adapters, hidden.into_iter().next().unwrap()).unwrap();
        assert!(store.get_session(key).unwrap().is_some());
        assert!(cbc_path.is_file() && legacy.join("index.json").is_file());

        write(&cbc_path, "{bad json}\n");
        run_scan(&adapters, &store, &NullEvents, false).unwrap();
        let fallback = store.get_session(key).unwrap().unwrap();
        assert_eq!(
            fallback.file_path,
            legacy.join("index.json").to_string_lossy()
        );
        assert!(fallback.favorite && fallback.pinned);

        write(&cbc_path, &(json!({"type":"message","role":"system","content":[{"type":"input_text","text":"only metadata"}]}).to_string() + "\n"));
        run_scan(&adapters, &store, &NullEvents, false).unwrap();
        assert_eq!(
            store.get_session(key).unwrap().unwrap().file_path,
            legacy.join("index.json").to_string_lossy()
        );

        write(&cbc_path, &cbc_text());
        run_scan(&adapters, &store, &NullEvents, false).unwrap();
        rusqlite::Connection::open(&catalog_db)
            .unwrap()
            .execute(
                "UPDATE conversations SET deleted_at=123,revision=3 WHERE id='old-1'",
                [],
            )
            .unwrap();
        run_scan(&adapters, &store, &NullEvents, false).unwrap();
        assert!(store.get_session(key).unwrap().is_none());
        assert!(cbc_path.is_file() && legacy.join("index.json").is_file());
        rusqlite::Connection::open(&catalog_db).unwrap().execute(
            "UPDATE conversations SET deleted_at=NULL,archived_at=456,revision=4 WHERE id='old-1'", [],
        ).unwrap();
        run_scan(&adapters, &store, &NullEvents, false).unwrap();
        let restored = store.get_session(key).unwrap().unwrap();
        assert!(restored.archived && restored.favorite && restored.pinned);

        fs::rename(&catalog_db, tmp.path().join("catalog.hold")).unwrap();
        run_scan(&adapters, &store, &NullEvents, false).unwrap();
        let retained = store.get_session(key).unwrap().unwrap();
        assert!(retained.archived && retained.favorite && retained.pinned);
        fs::rename(tmp.path().join("catalog.hold"), &catalog_db).unwrap();

        fs::rename(legacy.join("index.json"), legacy.join("index.json.hold")).unwrap();
        run_scan(&adapters, &store, &NullEvents, false).unwrap();
        assert_eq!(
            store.get_session(key).unwrap().unwrap().tokens_used,
            Some(17)
        );
    }

    /// Check the converter's real output, including deleted catalog rows, by
    /// parsing files directly. Deleted rows are never added to a Wake index.
    #[test]
    #[ignore = "reads real CodeBuddy conversation files"]
    fn real_converted_answers_round_trip_without_indexing_deleted_sessions() {
        fn contains_text(value: &Value, needle: &str) -> bool {
            match value {
                Value::String(text) => text.contains(needle),
                Value::Array(items) => items.iter().any(|v| contains_text(v, needle)),
                Value::Object(map) => map.values().any(|v| contains_text(v, needle)),
                _ => false,
            }
        }
        let adapter = CodebuddyAdapter::new();
        let rows = super::super::codebuddy_legacy::read_catalog(
            &super::super::codebuddy_legacy::catalog_path(),
        )
        .unwrap();
        let mut checked = 0;
        let mut absent = 0;
        let mut repeated = 0;
        let mut deleted_answers = 0;
        for entry in walkdir::WalkDir::new(&adapter.root)
            .follow_links(false)
            .into_iter()
            .flatten()
        {
            if entry.file_name() != ".codebuddy-ide-4-conversion.json" {
                continue;
            }
            let Ok(marker) = fs::read(entry.path()) else {
                continue;
            };
            let Ok(marker) = serde_json::from_slice::<Value>(&marker) else {
                continue;
            };
            let Some(id) = marker.get("conversationId").and_then(Value::as_str) else {
                continue;
            };
            let Some(row) = rows
                .iter()
                .find(|row| row.id == id && row.authority.is_empty())
            else {
                continue;
            };
            let Some(relative) = legacy_relative_dir(row) else {
                continue;
            };
            let dir = super::super::codebuddy_legacy::default_data_root().join(relative);
            let Ok(index) =
                super::super::codebuddy_legacy::read_session_file(&dir, &dir.join("index.json"))
            else {
                continue;
            };
            let Ok(index) = serde_json::from_slice::<Value>(&index) else {
                continue;
            };
            let Some(project) = entry.path().parent().and_then(Path::parent) else {
                continue;
            };
            let cbc = project.join(format!("{}.jsonl", row.runtime_id));
            let Ok(raw) = fs::read_to_string(&cbc) else {
                continue;
            };
            let outputs: Vec<Value> = raw
                .lines()
                .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                .filter(|v| {
                    v.get("type").and_then(Value::as_str) == Some("function_call_result")
                        && v.get("name").and_then(Value::as_str) == Some("AskUserQuestion")
                })
                .filter_map(|v| v.get("output").cloned())
                .collect();
            if outputs.is_empty() {
                continue;
            }
            let parsed = parse_codebuddy_jsonl(&cbc, false).unwrap();
            let r = SessionFileRef {
                agent: AgentId::Codebuddy,
                native_id: row.id.clone(),
                file_path: cbc.to_string_lossy().into_owned(),
                mtime_ms: 0,
                size: 0,
            };
            let derived = ParsedSession::derive(
                build_meta(AgentId::Codebuddy, &r, &parsed),
                &parsed.messages,
                parsed.unknown_lines,
            );
            let mut answers = std::collections::HashSet::new();
            for item in index
                .get("messages")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let Some(mid) = item
                    .get("id")
                    .and_then(Value::as_str)
                    .filter(|id| super::super::codebuddy_legacy::safe_id(id))
                else {
                    continue;
                };
                let path = dir.join("messages").join(format!("{mid}.json"));
                let Ok(bytes) = super::super::codebuddy_legacy::read_session_file(&dir, &path)
                else {
                    continue;
                };
                let Ok(outer) = serde_json::from_slice::<Value>(&bytes) else {
                    continue;
                };
                let Some(extra) = outer
                    .get("extra")
                    .and_then(Value::as_str)
                    .and_then(|s| serde_json::from_str::<Value>(s).ok())
                else {
                    continue;
                };
                for question in extra
                    .pointer("/questionAnswer/questions")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    for answer in question
                        .get("answers")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                    {
                        if !answer.trim().is_empty()
                            && outputs.iter().any(|v| contains_text(v, answer))
                        {
                            answers.insert(answer.to_string());
                        }
                    }
                }
            }
            for answer in answers {
                let matches = derived
                    .units
                    .iter()
                    .filter(|u| u.role == Role::User && u.text.contains(&answer))
                    .count();
                checked += 1;
                absent += usize::from(matches == 0);
                repeated += usize::from(matches > 1);
                deleted_answers += usize::from(row.deleted);
            }
        }
        println!("converted answer audit: checked={checked}, deleted_source_answers={deleted_answers}, missing={absent}, repeated={repeated}");
        assert!(checked >= 3, "expected real converted answer samples");
        assert_eq!(absent, 0);
        assert_eq!(repeated, 0);
    }
}
