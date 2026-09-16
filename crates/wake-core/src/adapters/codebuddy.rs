use super::parse_utils::*;
use super::{units_from_messages, AgentAdapter};
use crate::models::*;
use anyhow::Result;
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

/// CodeBuddy Code(腾讯 CodeBuddy 的 CLI,issue #27):`~/.codebuddy/projects/
/// <cwd 转义>/<sessionId>.jsonl`。文件布局借自 Claude Code——同名目录
/// `<sessionId>/subagents/agent-*.jsonl` 是子代理转录、`<sessionId>.meta.json`
/// 是边车,主列表只枚举 slug 目录直属的 JSONL;**行格式却是 OpenAI Responses
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
/// `file-history-snapshot` / `summary` / `turn-metrics` 是已知元数据行,表外
/// 计 unknown。`CODEBUDDY_CONFIG_DIR` 覆盖数据根(与 Qoder 的 QODER_CONFIG_DIR
/// 同款:探到会话才采信)。
///
/// 格式来源:官方 CLI 文档 + 三家独立实现(cc-switch / codex-host / codeg)
/// 交叉核对,本机零会话,**未经真机验证**;fixture 合成。
///
/// WorkBuddy(腾讯的桌面 agent,与 CodeBuddy 同一内核)是本适配器的孪生实例
/// (与 pi / omp 同款):`~/.workbuddy/projects` 同构树、`WORKBUDDY_CONFIG_DIR`
/// 覆盖(官方桌面端设置的就是这个变量),解析核心完全共用,差异只有 agent 身份
/// 与根;它没有 CLI,resume 不提供。同构这一点只有 cc-switch 一家的说法,
/// 比 CodeBuddy 的三家佐证弱。
pub struct CodebuddyAdapter {
    agent: AgentId,
    root: PathBuf,
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
        let default = super::home_dir()
            .unwrap_or_default()
            .join(home_dir_name)
            .join("projects");
        let root = super::env_dir(env_key)
            .map(|dir| dir.join("projects"))
            // 与其他 env override 一致:存在但没有任何会话的候选不能遮掉默认根
            // (Dock 启动与 shell 启动看到的环境经常不同)
            .filter(|dir| project_tree_has_session(dir, agent))
            .unwrap_or(default);
        Self { agent, root }
    }
}

/// 已知的非消息行:静默跳过、不计 unknown。表外类型计数(漂移金丝雀)
const KNOWN_SKIP: &[&str] = &["file-history-snapshot", "summary", "turn-metrics"];

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
        created_at: 0,
        updated_at: 0,
        unknown_lines: 0,
    };
    let mut tool_index: HashMap<String, (usize, usize)> = HashMap::new();
    let mut pending: Option<PendingAssistant> = None;

    for line in reader.lines() {
        let Ok(line) = line else {
            out.unknown_lines += 1;
            continue;
        };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(row) = serde_json::from_str::<Value>(&line) else {
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
                        out.messages.push(msg);
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
                let parsed_args;
                let arguments: &Value = match row.get("arguments") {
                    Some(Value::String(s)) => {
                        parsed_args = serde_json::from_str::<Value>(s)
                            .unwrap_or_else(|_| Value::String(s.clone()));
                        &parsed_args
                    }
                    Some(v) => v,
                    None => &Value::Null,
                };
                let p = assistant_slot(&mut out, &mut pending, &mut tool_index, ts, &row);
                p.tool_calls
                    .push(tool_call_view(call_id, name, arguments, None, false));
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
        favorite: false,
        pinned: false,
    }
}

impl AgentAdapter for CodebuddyAdapter {
    fn agent(&self) -> AgentId {
        self.agent
    }

    fn list_session_files(&self) -> Result<Vec<SessionFileRef>> {
        Ok(list_project_tree_refs(&self.root, self.agent))
    }

    fn file_ref(&self, path: &Path) -> Option<SessionFileRef> {
        let relative = path.strip_prefix(&self.root).ok()?;
        // root/<session>.jsonl 或 root/<slug>/<session>.jsonl;更深的
        // <session>/subagents/agent-*.jsonl 是子代理转录,不得成为顶层会话
        if relative.components().count() > 2 {
            return None;
        }
        default_file_ref(self.agent(), path)
    }

    fn parse_session(&self, r: &SessionFileRef) -> Result<ParsedSession> {
        let parsed = parse_codebuddy_jsonl(Path::new(&r.file_path), false)?;
        let meta = build_meta(self.agent, r, &parsed);
        let units = units_from_messages(&parsed.messages);
        Ok(ParsedSession {
            meta,
            units,
            unknown_line_count: parsed.unknown_lines,
        })
    }

    fn parse_transcript(&self, r: &SessionFileRef) -> Result<ParsedTranscript> {
        let parsed = parse_codebuddy_jsonl(Path::new(&r.file_path), true)?;
        Ok(ParsedTranscript {
            meta: build_meta(self.agent, r, &parsed),
            mainline: parsed.messages,
            sidechains: Vec::new(),
            unknown_line_count: parsed.unknown_lines,
        })
    }

    fn session_paths(&self, meta: &SessionMeta) -> Vec<String> {
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
        let root = if dir.join("projects").is_dir() {
            dir.join("projects")
        } else {
            dir
        };
        Box::new(Self {
            agent: self.agent,
            root,
        })
    }

    fn data_roots(&self) -> Vec<PathBuf> {
        vec![self.root.clone()]
    }
}
