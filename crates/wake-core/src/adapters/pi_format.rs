//! pi-ai 消息形状的共用渲染核心:Pi / Oh My Pi 的 `<ts>_<uuid>.jsonl` 与 OpenClaw
//! 的转录条目都是同一族——`{role:user|assistant|toolResult, content:[…]}`,
//! assistant 块 text|thinking|toolCall,toolResult 独立 role 按 toolCallId 回填,
//! 连续 assistant(中间只隔 toolResult)合并成一条(合并与截断在
//! `parse_utils::merge_into_last_assistant`,各家共用)。两家的差异全部落在 `PiRenderOptions`。

use super::parse_utils::*;
use crate::models::*;
use serde_json::Value;
use std::collections::HashMap;

#[derive(Clone, Copy, Default)]
pub(crate) struct PiRenderOptions {
    /// assistant 的 `thinking` 块进 `thinking`(Pi 不落盘 thinking,OpenClaw 落)
    pub thinking_blocks: bool,
    /// user 行带 `runtimeContextCarrier:true` 是运行时注入的上下文,归 Meta
    pub meta_carrier: bool,
    /// toolResult 正文走 tool_result_parts(未知对象保留 JSON 原文)而非 content_parts
    pub raw_tool_results: bool,
}

/// 逐条喂 message 的累加器;`messages` 定型后由调用方 assign_seq
pub(crate) struct PiRender {
    pub messages: Vec<TranscriptMessage>,
    pub model: Option<String>,
    pub tokens_used: Option<i64>,
    opts: PiRenderOptions,
    decode_images: bool,
    /// toolCallId → (消息下标, tool_calls 下标)
    tool_index: HashMap<String, (usize, usize)>,
}

impl PiRender {
    pub fn new(opts: PiRenderOptions, decode_images: bool) -> Self {
        Self {
            messages: Vec::new(),
            model: None,
            tokens_used: None,
            opts,
            decode_images,
            tool_index: HashMap::new(),
        }
    }

    /// 一条 `{role,content,…}`;未知 role 返回 false(调用方计 unknown)
    pub fn push(&mut self, msg: &Value, ts: i64) -> bool {
        let content = msg.get("content").unwrap_or(&Value::Null);
        match msg.get("role").and_then(Value::as_str) {
            Some("user") => {
                let parsed = content_parts(content, self.decode_images);
                if parsed.text.is_empty() && parsed.images.is_empty() {
                    return true;
                }
                let mut message = text_msg(Role::User, &parsed.text, ts);
                message.images = parsed.images;
                if self.opts.meta_carrier
                    && msg.get("runtimeContextCarrier").and_then(Value::as_bool) == Some(true)
                {
                    message.kind = MessageKind::Meta;
                }
                self.messages.push(message);
            }
            Some("assistant") => self.push_assistant(msg, content, ts),
            Some("toolResult") => {
                let Some(call_id) = msg.get("toolCallId").and_then(Value::as_str) else {
                    return true;
                };
                let Some(&(mi, ti)) = self.tool_index.get(call_id) else {
                    return true;
                };
                let parsed = if self.opts.raw_tool_results {
                    tool_result_parts(content, self.decode_images)
                } else {
                    content_parts(content, self.decode_images)
                };
                let message = &mut self.messages[mi];
                let tc = &mut message.tool_calls[ti];
                if !parsed.text.is_empty() {
                    tc.output = Some(clip(&parsed.text, MAX_TOOL_IO).0);
                }
                if msg.get("isError").and_then(Value::as_bool) == Some(true) {
                    tc.is_error = true;
                }
                append_images_to_message_end(message, parsed.images);
            }
            _ => return false,
        }
        true
    }

    fn push_assistant(&mut self, msg: &Value, content: &Value, ts: i64) {
        // Each assistant entry reports one API call, including cache tokens.
        // Account before display filtering: empty/thinking-only calls can still
        // consume tokens even when they produce no visible transcript message.
        if let Some(tokens) = msg
            .get("usage")
            .and_then(|u| u.get("totalTokens"))
            .and_then(Value::as_i64)
            .filter(|tokens| *tokens >= 0)
        {
            self.tokens_used = Some(self.tokens_used.unwrap_or(0).saturating_add(tokens));
        }
        let parsed = content_parts(content, self.decode_images);
        let mut tools: Vec<ToolCallView> = Vec::new();
        let mut thinking = String::new();
        for b in content.as_array().into_iter().flatten() {
            match b.get("type").and_then(Value::as_str) {
                Some("toolCall") => {
                    let id = b.get("id").and_then(Value::as_str).unwrap_or_default();
                    let name = b.get("name").and_then(Value::as_str).unwrap_or_default();
                    let input = b.get("arguments").cloned().unwrap_or(Value::Null);
                    tools.push(tool_call_view(id.to_string(), name, &input, None, false));
                }
                Some("thinking") if self.opts.thinking_blocks => {
                    if let Some(t) = b.get("thinking").and_then(Value::as_str) {
                        let t = t.trim();
                        if !t.is_empty() {
                            if !thinking.is_empty() {
                                thinking.push_str("\n\n");
                            }
                            thinking.push_str(t);
                        }
                    }
                }
                _ => {}
            }
        }
        if parsed.text.is_empty()
            && tools.is_empty()
            && parsed.images.is_empty()
            && thinking.is_empty()
        {
            return;
        }
        let model = msg.get("model").and_then(Value::as_str).map(String::from);
        if model.is_some() {
            self.model = model.clone();
        }
        // 连续 assistant(中间只隔 toolResult)合并成一条,详情页每个回合一条助手消息
        let base = merge_into_last_assistant(&mut self.messages, ts, parsed, &thinking);
        let last = &mut self.messages[base];
        if model.is_some() {
            last.model = model;
        }
        for tc in tools {
            self.tool_index
                .insert(tc.id.clone(), (base, last.tool_calls.len()));
            last.tool_calls.push(tc);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tokens_include_hidden_calls_without_adding_transcript_rows() {
        let mut render = PiRender::new(PiRenderOptions::default(), false);
        for (content, tokens) in [
            (json!([{"type": "text", "text": "First reply"}]), 100),
            (json!([]), 20),
            (json!([{"type": "thinking", "thinking": "Hidden"}]), 30),
            (json!([{"type": "text", "text": "Final reply"}]), 200),
        ] {
            render.push(
                &json!({"role": "assistant", "content": content,
                    "usage": {"totalTokens": tokens}}),
                1,
            );
        }
        assert_eq!(render.tokens_used, Some(350));
        assert_eq!(render.messages.len(), 1);
        assert_eq!(render.messages[0].text, "First reply\n\nFinal reply");
    }

    #[test]
    fn missing_invalid_and_zero_usage_preserve_reported_totals() {
        let mut render = PiRender::new(PiRenderOptions::default(), false);
        for usage in [json!(null), json!({}), json!({"totalTokens": -1})] {
            render.push(&json!({"role": "assistant", "usage": usage}), 1);
        }
        assert_eq!(render.tokens_used, None);

        render.push(
            &json!({"role": "assistant", "usage": {"totalTokens": 0}}),
            1,
        );
        assert_eq!(render.tokens_used, Some(0));
        for usage in [
            json!({"totalTokens": 42}),
            json!(null),
            json!({"totalTokens": -1}),
            json!({"totalTokens": 0}),
        ] {
            render.push(&json!({"role": "assistant", "usage": usage}), 1);
        }
        assert_eq!(render.tokens_used, Some(42));
    }

    #[test]
    fn token_sum_saturates_instead_of_overflowing_on_malformed_usage() {
        let mut render = PiRender::new(PiRenderOptions::default(), false);
        for tokens in [i64::MAX, 1] {
            render.push(
                &json!({"role": "assistant", "usage": {"totalTokens": tokens}}),
                1,
            );
        }
        assert_eq!(render.tokens_used, Some(i64::MAX));
    }
}
