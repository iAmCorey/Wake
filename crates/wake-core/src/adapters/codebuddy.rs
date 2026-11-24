use super::parse_utils::*;
use super::AgentAdapter;
use crate::models::*;
use anyhow::Result;
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::io::BufReader;
use std::path::{Path, PathBuf};

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
/// 用户行不全是人打的字:IDE 上下文垫在提问前面(`row_content` 切开)、压缩摘要与各种内部
/// 回灌也写成用户行(`flagged_kind` / `prefixed_kind`);贴进来的图片按内容寻址存在 `<home>/blobs`(`blob_file`),
/// 没有 home 的根(只选了 projects 目录、远程镜像)照旧占位;只有元数据的文件不列
/// (`has_conversation`)。
///
/// 格式来源:官方 CLI 文档 + 三家独立实现(cc-switch / codex-host / codeg)
/// 交叉核对,本机零会话,**未经真机验证**;fixture 合成。上面那段用户行的规则来自
/// PR #88(@XYZliang)在真实会话上的发现,由我们重写,同样没在本机核对过。
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
            home: Some(home),
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
    "context-usage",
    "request-recovery",
];

/// 不是对话内容的行:`KNOWN_SKIP` 加三种标题行
fn is_metadata_row(typ: &str) -> bool {
    KNOWN_SKIP.contains(&typ) || matches!(typ, "custom-title" | "ai-title" | "topic")
}

/// 判断有没有对话只看每行的 `type`,不建整棵 Value
#[derive(serde::Deserialize)]
struct RowType {
    #[serde(rename = "type", default)]
    typ: String,
}

/// 文件里有没有对话:读到第一条不是元数据的行就停。整份读完都只有元数据(开了会话没说话就
/// 关了)才算没有;读不出来、有解不开的行(正在写的半行)都按有算——宁可多列一条空会话,
/// 也不把正在写的会话藏起来
fn has_conversation(path: &Path) -> bool {
    let Ok(file) = fs::File::open(path) else {
        return true;
    };
    for row in jsonl_rows::<RowType, _>(BufReader::new(file)) {
        let Ok(Some(row)) = row else {
            return true;
        };
        if !is_metadata_row(&row.typ) {
            return true;
        }
    }
    false
}

/// 图片旁边那块 `<image_local_path>…</image_local_path>`:写下它那台机器上的临时路径。只认
/// 整块恰好是一个标签的;标签之间夹着话的不算,整块丢掉会连话一起丢
fn is_image_label(text: &str) -> bool {
    text.trim()
        .strip_prefix("<image_local_path>")
        .and_then(|rest| rest.strip_suffix("</image_local_path>"))
        .is_some_and(|path| !path.contains('<'))
}

/// CodeBuddy 的 isGeneratedPlaceholderTitle:自动标题没东西可总结时写的占位,
/// 显示它比回退到首条用户消息更糟
fn is_placeholder_title(text: &str) -> bool {
    text == "(No content)" || text == "/compact" || is_image_label(text)
}

fn is_blob(block: &Value) -> bool {
    block.get("type").and_then(Value::as_str) == Some("image_blob_ref")
}

/// `image_blob_ref` 在 blob 目录里的相对路径 `<id 前两位>/<id>.<扩展名>`。id 是内容的 sha256
/// (64 位十六进制),扩展名取 blob_path 的文件名;blob_path 本身是写下它那台机器上的绝对路径,
/// 不解引用。是不是图片由 `image_from_session_file` 按字节认
fn blob_file(block: &Value) -> Option<PathBuf> {
    let id = block.get("blob_id")?.as_str()?;
    if id.len() != 64 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let name = path_tail(block.get("blob_path")?.as_str()?);
    let ext = name.strip_prefix(id)?.strip_prefix('.')?;
    (!ext.is_empty()).then(|| Path::new(&id[..2]).join(name))
}

/// 一行的内容块。blob 里的图片按 id 在 `blobs` 里解(解不出来、不解码时 `content_parts` 照常
/// 占位,两种解析的消息条数一样),有 blob 时去掉旁边的路径标签
fn row_content(row: &Value, decode_images: bool, blobs: Option<&Path>) -> ParsedContent {
    let blocks = match row.get("content").unwrap_or(&Value::Null) {
        Value::Array(blocks) => blocks.as_slice(),
        other => std::slice::from_ref(other),
    };
    let blobs = blobs.filter(|_| decode_images);
    let has_blob = blocks.iter().any(is_blob);
    let mut out = ParsedContent::default();
    for block in blocks {
        let decoded = match blobs {
            Some(root) if is_blob(block) => {
                blob_file(block).and_then(|file| image_from_session_file(root, &root.join(file)))
            }
            _ => None,
        };
        if let Some(image) = decoded {
            out.push_image(image);
            continue;
        }
        let part = content_parts(block, decode_images);
        if !(has_blob && part.images.is_empty() && is_image_label(&part.text)) {
            out.append(part);
        }
    }
    out
}

/// 记成用户行、却不是人打的字,providerData 上有标记的:压缩摘要归 CompactSummary,队友(别的
/// agent)的汇报、hook 的回灌、后台任务通知归 Meta。有标记的整行照它判,开头的上下文不切
fn flagged_kind(row: &Value) -> Option<MessageKind> {
    let flag = |key: &str| {
        row.get("providerData")
            .and_then(|pd| pd.get(key))
            .is_some_and(|v| !v.is_null() && *v != Value::Bool(false))
    };
    if flag("isSummary") {
        Some(MessageKind::CompactSummary)
    } else if [
        "isMeta",
        "isCompactInternal",
        "backgroundTaskNotification",
        "teammateMessage",
    ]
    .into_iter()
    .any(flag)
    {
        Some(MessageKind::Meta)
    } else {
        None
    }
}

/// 没有标记时看剥掉 IDE 上下文之后的开头:压缩摘要,队友汇报、本地命令输出、Stop hook 的反馈
fn prefixed_kind(text: &str) -> Option<MessageKind> {
    let starts = |prefixes: &[&str]| prefixes.iter().any(|p| text.starts_with(p));
    if starts(&["<cb_summary>", "<conversation_history_summary>"]) {
        Some(MessageKind::CompactSummary)
    } else if starts(&[
        "<teammate-message",
        "<local-command-stdout>",
        "A session-scoped Stop hook",
        "Stop hook feedback:",
    ]) {
        Some(MessageKind::Meta)
    } else {
        None
    }
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
    out.tokens_used += row_usage(row);
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

/// `blobs`:贴进来的图片所在的 blob 目录(`<home>/blobs`),没有 home 的根给 None
fn parse_codebuddy_jsonl(
    path: &Path,
    decode_images: bool,
    blobs: Option<&Path>,
) -> Result<ParseResult> {
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
        created_at: 0,
        updated_at: 0,
        unknown_lines: 0,
    };
    let mut tool_index: HashMap<String, (usize, usize)> = HashMap::new();
    let mut pending: Option<PendingAssistant> = None;

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
                        let content = row_content(&row, decode_images, blobs);
                        let flagged = flagged_kind(&row);
                        let before = out.messages.len();
                        // 垫在前面的 IDE 上下文:单独一条 Meta,不当提问、不进搜索,内容照留
                        let ParsedContent { text, images } = match flagged {
                            Some(_) => content,
                            None => push_leading_context(content, ts, &mut out.messages),
                        };
                        if !text.is_empty() || !images.is_empty() {
                            let lead = after_image_placeholders(&text);
                            let mut msg = text_msg(Role::User, &text, ts);
                            // 摘要优先:带 isCompactInternal 标记的 `<cb_summary>` 是压缩摘要
                            let prefixed = prefixed_kind(lead);
                            msg.kind = if [flagged, prefixed]
                                .contains(&Some(MessageKind::CompactSummary))
                            {
                                MessageKind::CompactSummary
                            } else {
                                flagged.or(prefixed).unwrap_or_else(|| user_kind(lead))
                            };
                            msg.images = images;
                            out.messages.push(msg);
                        } else if out.messages.len() == before {
                            continue;
                        }
                    }
                    "assistant" => {
                        let p = assistant_slot(&mut out, &mut pending, &mut tool_index, ts, &row);
                        p.content.append(row_content(&row, decode_images, blobs));
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
                // 与 grok / opencode 同款正向判错:只认已知的失败值,别的状态
                // (缺省、in_progress……)都不算错
                tc.is_error = matches!(
                    row.get("status").and_then(Value::as_str),
                    Some("failed" | "error")
                ) || row.get("isError").and_then(Value::as_bool) == Some(true);
                // 工具结果里的图片只占位不解码(CodeBuddy 工具结果暂无图片形态佐证)
                let output = tool_result_parts(row.get("output").unwrap_or(&Value::Null), false);
                tc.output = (!output.text.is_empty()).then(|| clip(&output.text, MAX_TOOL_IO).0);
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
    fn blobs(&self) -> Option<PathBuf> {
        self.home.as_ref().map(|home| home.join("blobs"))
    }

    /// 会话或子代理(子代理是 agent 派出去的,标题取边车记的任务说明)
    fn parse(&self, r: &SessionFileRef, decode_images: bool) -> Result<(SessionMeta, ParseResult)> {
        let path = Path::new(&r.file_path);
        let parsed = parse_codebuddy_jsonl(path, decode_images, self.blobs().as_deref())?;
        let mut meta = build_meta(self.agent, r, &parsed);
        mark_subagent(&mut meta, path);
        Ok((meta, parsed))
    }
}

impl AgentAdapter for CodebuddyAdapter {
    fn agent(&self) -> AgentId {
        self.agent
    }

    fn list_session_files(&self) -> Result<Vec<SessionFileRef>> {
        Ok(list_project_tree_refs(&self.root, self.agent)
            .into_iter()
            .filter(|r| has_conversation(Path::new(&r.file_path)))
            .collect())
    }

    fn file_ref(&self, path: &Path) -> Option<SessionFileRef> {
        // 判的是引用指向的转录:子代理边车(`.meta.json`)的事件也换成它的转录
        project_tree_file_ref(&self.root, self.agent, path)
            .filter(|r| has_conversation(Path::new(&r.file_path)))
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
        let blobs = self.blobs();
        load_subagent(r, sidechain_id, |file| {
            Ok(parse_codebuddy_jsonl(file, true, blobs.as_deref())?.messages)
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
        })
    }

    fn data_roots(&self) -> Vec<PathBuf> {
        vec![self.root.clone()]
    }

    fn homes(&self) -> Vec<PathBuf> {
        self.home.iter().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn image_labels_are_whole_blocks_only() {
        assert!(is_image_label(
            " <image_local_path>/tmp/a.png</image_local_path>\n"
        ));
        for text in [
            "<image_local_path>a.png</image_local_path>\n请比较两张图片\n<image_local_path>b.png</image_local_path>",
            "<image_local_path>a.png</image_local_path> 这张图",
            "看 <image_local_path>a.png</image_local_path>",
        ] {
            assert!(!is_image_label(text), "{text}");
        }
    }

    #[test]
    fn blob_paths_come_from_the_id_not_the_recorded_path() {
        const ID: &str = "aa0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcd";
        let expected = |ext: &str| Some(Path::new("aa").join(format!("{ID}.{ext}")));
        for (id, path, want) in [
            (
                ID,
                format!("/remote/home/.codebuddy/blobs/aa/{ID}.PNG"),
                expected("PNG"),
            ),
            (
                ID,
                format!(r"C:\Users\x\.codebuddy\blobs\aa\{ID}.jpg"),
                expected("jpg"),
            ),
            // id 不是 64 位十六进制、文件名不是 <id>.<扩展名>,一律不认
            ("../../outside", "outside.png".into(), None),
            (&*ID.replace('a', "g"), "x.png".into(), None),
            (ID, "/b/other.png".into(), None),
            (ID, format!("/b/{ID}"), None),
            (ID, format!("/b/{ID}."), None),
        ] {
            let block = json!({"type": "image_blob_ref", "blob_id": id, "blob_path": path});
            assert_eq!(blob_file(&block), want, "{id} {path}");
        }
        assert_eq!(
            blob_file(&json!({"type": "image_blob_ref", "blob_id": ID})),
            None
        );
    }
}
