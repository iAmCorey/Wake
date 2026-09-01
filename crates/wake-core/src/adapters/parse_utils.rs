use crate::models::*;
use base64::engine::general_purpose::{STANDARD as BASE64, URL_SAFE as BASE64_URL_SAFE};
use base64::Engine as _;
use serde_json::Value;
use std::cell::Cell;

const MAX_TRANSCRIPT_IMAGE_BYTES: usize = 64 * 1024 * 1024;

thread_local! {
    static IMAGE_DECODE_REMAINING: Cell<Option<usize>> = const { Cell::new(None) };
}

/// 一次会话解析的图片压缩字节总预算。嵌套调用复用外层预算，离开解析作用域后恢复。
pub struct ImageDecodeBudgetGuard {
    previous: Option<usize>,
    changed: bool,
}

pub fn transcript_image_decode_budget(decode_images: bool) -> ImageDecodeBudgetGuard {
    image_decode_budget(decode_images.then_some(MAX_TRANSCRIPT_IMAGE_BYTES))
}

fn image_decode_budget(limit: Option<usize>) -> ImageDecodeBudgetGuard {
    IMAGE_DECODE_REMAINING.with(|remaining| {
        let previous = remaining.get();
        let changed = previous.is_none() && limit.is_some();
        if changed {
            remaining.set(limit);
        }
        ImageDecodeBudgetGuard { previous, changed }
    })
}

impl Drop for ImageDecodeBudgetGuard {
    fn drop(&mut self) {
        if self.changed {
            IMAGE_DECODE_REMAINING.with(|remaining| remaining.set(self.previous));
        }
    }
}

fn consume_image_decode_budget(bytes: usize) -> bool {
    IMAGE_DECODE_REMAINING.with(|remaining| match remaining.get() {
        Some(left) if bytes <= left => {
            remaining.set(Some(left - bytes));
            true
        }
        Some(_) => false,
        None => true,
    })
}

/// 文件 mtime → epoch ms(各家 adapter 与 watcher 共用)
pub fn mtime_ms(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// cwd → 项目显示名(路径末段;空/异常 = "Unknown project")
pub fn project_name_of(cwd: &str) -> String {
    if cwd.is_empty() {
        return "Unknown project".to_string();
    }
    std::path::Path::new(cwd)
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "Unknown project".to_string())
}

/// user 消息的 kind:注入内容归 Meta 折叠
pub fn user_kind(text: &str) -> MessageKind {
    if is_injected_user_content(text) {
        MessageKind::Meta
    } else {
        MessageKind::Text
    }
}

/// 纯文本消息构造(clip + user 注入判定),多家 adapter 共用
pub fn text_msg(role: Role, text: &str, ts: i64) -> TranscriptMessage {
    let (clipped, truncated) = clip(text.trim(), MAX_MSG_TEXT);
    let kind = if role == Role::User {
        user_kind(text)
    } else {
        MessageKind::Text
    };
    TranscriptMessage {
        seq: 0,
        role,
        kind,
        text: clipped,
        truncated,
        tool_calls: Vec::new(),
        thinking: None,
        timestamp: if ts > 0 { Some(ts) } else { None },
        model: None,
        images: Vec::new(),
    }
}

/// seq 回填——FTS seq 与详情页序号一致(跨文件不变量 1)的统一执行点,
/// 必须在消息序列定型后、入库/返回前调用
pub fn assign_seq(messages: &mut [TranscriptMessage]) {
    for (i, m) in messages.iter_mut().enumerate() {
        m.seq = i as i64;
    }
}

/// 标题推导:首条真实用户消息经清洗,各家共用的回退链
pub fn title_from_messages(messages: &[TranscriptMessage]) -> Option<String> {
    messages
        .iter()
        .filter(|m| m.role == Role::User && m.kind == MessageKind::Text)
        .map(|m| clean_title_candidate(&m.text))
        .find(|title| !title.is_empty())
}

/// `AgentAdapter::file_ref` 的默认实现:非空 .jsonl,stem 即 native_id。
/// 覆写方可在此结果上做路径过滤或 native_id 改写。
pub fn default_file_ref(agent: AgentId, path: &std::path::Path) -> Option<SessionFileRef> {
    let name = path.file_name()?.to_string_lossy().to_string();
    if name.starts_with('.') {
        return None;
    }
    let stem = name.strip_suffix(".jsonl")?;
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() == 0 {
        return None;
    }
    Some(SessionFileRef {
        agent,
        native_id: stem.to_string(),
        file_path: path.to_string_lossy().to_string(),
        mtime_ms: mtime_ms(&meta),
        size: meta.len() as i64,
    })
}

/// 递归枚举目录下非空 .jsonl 为 SessionFileRef;`native_id` 从文件 stem 提取
/// 会话 id(多数家恒等,codex 需剥 rollout 前缀)
pub fn list_jsonl_refs(
    dir: &std::path::Path,
    agent: AgentId,
    native_id: impl Fn(&str) -> String,
) -> Vec<SessionFileRef> {
    let mut refs = Vec::new();
    if !dir.is_dir() {
        return refs;
    }
    for entry in walkdir::WalkDir::new(dir).into_iter().flatten() {
        if !entry.file_type().is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        let Some(stem) = name.strip_suffix(".jsonl") else {
            continue;
        };
        let Ok(meta) = entry.metadata() else { continue };
        if meta.len() == 0 {
            continue;
        }
        refs.push(SessionFileRef {
            agent,
            native_id: native_id(stem),
            file_path: entry.path().to_string_lossy().to_string(),
            mtime_ms: mtime_ms(&meta),
            size: meta.len() as i64,
        });
    }
    refs
}

/// content blocks → 文本和图片。支持各家目前出现的 Anthropic、OpenAI、
/// Gemini、Pi/Kiro 与 OpenCode 形状；未知、远程、本地路径或过大的图片退回占位符。
#[derive(Debug, Default)]
pub struct ParsedContent {
    pub text: String,
    pub images: Vec<ImageAttachment>,
}

impl ParsedContent {
    /// 追加一个正文块，并保持已经记录的图片偏移不变。
    pub fn push_text(&mut self, value: &str) {
        self.push_text_with_separator(value, "\n\n");
    }

    pub fn push_text_with_separator(&mut self, value: &str, separator: &str) {
        let value = value.trim();
        if value.is_empty() {
            return;
        }
        if !self.text.is_empty() {
            self.text.push_str(separator);
        }
        self.text.push_str(value);
    }

    /// 追加另一段已解析内容；图片偏移随正文基址一起平移。
    pub fn append(&mut self, other: Self) {
        self.append_with_separator(other, "\n\n");
    }

    pub fn append_with_separator(&mut self, mut other: Self, separator: &str) {
        let had_text = !self.text.is_empty();
        let has_other_text = !other.text.is_empty();
        let base = self.text.len();
        let separator_len = if had_text && has_other_text {
            separator.len()
        } else {
            0
        };
        for image in &mut other.images {
            image.text_offset += base
                + if image.text_offset > 0 {
                    separator_len
                } else {
                    0
                };
        }
        if has_other_text {
            if separator_len > 0 {
                self.text.push_str(separator);
            }
            self.text.push_str(&other.text);
        }
        self.images.extend(other.images);
    }

    fn push_image(&mut self, mut image: ImageAttachment) {
        image.text_offset = self.text.len();
        self.images.push(image);
    }
}

/// 把新内容并入已存在的消息，并同步平移图片的正文偏移。
pub fn append_content_to_message(
    message: &mut TranscriptMessage,
    content: ParsedContent,
    separator: &str,
) {
    let mut combined = ParsedContent {
        text: std::mem::take(&mut message.text),
        images: std::mem::take(&mut message.images),
    };
    combined.append_with_separator(content, separator);
    message.text = combined.text;
    message.images = combined.images;
}

/// 工具结果不属于消息正文流；其图片统一放到现有正文末尾。
pub fn append_images_to_message_end(
    message: &mut TranscriptMessage,
    mut images: Vec<ImageAttachment>,
) {
    for image in &mut images {
        image.text_offset = message.text.len();
    }
    message.images.extend(images);
}

pub fn content_parts(v: &Value, decode_images: bool) -> ParsedContent {
    let mut parsed = ParsedContent::default();
    collect_content(v, decode_images, 0, &mut parsed);
    parsed
}

fn collect_content(v: &Value, decode_images: bool, depth: usize, parsed: &mut ParsedContent) {
    if depth > 4 {
        return;
    }
    match v {
        Value::String(value) => {
            parsed.push_text(value);
        }
        Value::Array(parts) => {
            for part in parts {
                collect_content(part, decode_images, depth + 1, parsed);
            }
        }
        Value::Object(_) => {
            if is_image_part(v) {
                if decode_images {
                    if let Some(image) = decode_image_part(v, depth) {
                        parsed.push_image(image);
                        return;
                    }
                }
                parsed.push_text(IMAGE_PLACEHOLDER);
                return;
            }

            let kind = part_kind(v);
            if matches!(kind.as_deref(), Some("text" | "input_text" | "output_text")) {
                if let Some(value) = v
                    .get("text")
                    .or_else(|| v.get("data"))
                    .and_then(Value::as_str)
                {
                    parsed.push_text(value);
                }
                return;
            }

            // Gemini 的普通 part 没有 type；Grok 的 content wrapper 则需要
            // 再下钻一层。reasoning/tool 块由各 adapter 自己分桶，不在这里混入正文。
            if kind.is_none() {
                if let Some(value) = v.get("text").and_then(Value::as_str) {
                    parsed.push_text(value);
                    return;
                }
                if let Some(inner) = v.get("content") {
                    collect_content(inner, decode_images, depth + 1, parsed);
                    return;
                }
            }
            if matches!(kind.as_deref(), Some("content" | "part")) {
                if let Some(inner) = v.get("content").or_else(|| v.get("data")) {
                    collect_content(inner, decode_images, depth + 1, parsed);
                }
            }
        }
        _ => {}
    }
}

/// 工具结果与普通消息块的区别是：未知对象在旧解析器里会以 JSON 原文显示。
/// 图片仍走统一解码，无法识别的非图片对象则保留这个兼容行为。
pub fn tool_result_parts(v: &Value, decode_images: bool) -> ParsedContent {
    let values = v
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_else(|| std::slice::from_ref(v));
    let mut result = ParsedContent::default();
    for value in values {
        let parsed = content_parts(value, decode_images);
        if !parsed.text.is_empty() {
            result.append_with_separator(parsed, "\n");
        } else if parsed.images.is_empty() && !value.is_null() {
            if let Ok(json) = serde_json::to_string(value) {
                result.push_text_with_separator(&json, "\n");
            }
        } else {
            result.append_with_separator(parsed, "\n");
        }
    }
    result
}

fn part_kind(v: &Value) -> Option<String> {
    v.get("type")
        .or_else(|| v.get("kind"))
        .and_then(Value::as_str)
        .map(|value| value.to_ascii_lowercase().replace('-', "_"))
}

fn image_media_type(v: &Value) -> Option<String> {
    ["media_type", "mediaType", "mime_type", "mimeType", "mime"]
        .iter()
        .find_map(|key| v.get(*key).and_then(Value::as_str))
        .filter(|value| value.starts_with("image/"))
        .map(|value| {
            value
                .split(';')
                .next()
                .unwrap_or(value)
                .to_ascii_lowercase()
        })
}

pub fn is_image_part(v: &Value) -> bool {
    part_kind(v).is_some_and(|kind| kind.contains("image"))
        || image_media_type(v).is_some()
        || v.get("inlineData").is_some()
        || v.get("inline_data").is_some()
        || v.get("image_url").is_some()
        || v.get("imageUrl").is_some()
        || v.get("source")
            .is_some_and(|source| image_media_type(source).is_some())
}

pub fn has_image_content(v: &Value) -> bool {
    if is_image_part(v) {
        return true;
    }
    match v {
        Value::Array(values) => values.iter().any(has_image_content),
        Value::Object(_) => v
            .get("content")
            .or_else(|| v.get("data"))
            .is_some_and(has_image_content),
        _ => false,
    }
}

fn decode_image_part(v: &Value, depth: usize) -> Option<ImageAttachment> {
    if depth > 5 {
        return None;
    }
    for key in ["source", "inlineData", "inline_data"] {
        if let Some(inner) = v.get(key) {
            if let Some(image) = decode_image_part(inner, depth + 1) {
                return Some(image);
            }
        }
    }

    let media_type = image_media_type(v);
    for key in [
        "image_url",
        "imageUrl",
        "url",
        "uri",
        "fileUri",
        "file_uri",
        "path",
    ] {
        if let Some(source) = v.get(key) {
            if let Some(value) = source.as_str() {
                if let Some(image) = decode_image_source(value, media_type.as_deref()) {
                    return Some(image);
                }
            } else if let Some(image) = decode_image_part(source, depth + 1) {
                return Some(image);
            }
        }
    }

    if let Some(data) = v.get("data") {
        match data {
            Value::String(value) => {
                if value.starts_with("data:") {
                    return decode_data_uri(value);
                }
                if media_type.as_deref() == Some("image/svg+xml") && value.contains("<svg") {
                    return attachment_from_bytes(value.as_bytes().to_vec(), media_type.as_deref());
                }
                return decode_base64_image(value, media_type.as_deref());
            }
            Value::Array(values) if values.len() <= MAX_IMAGE_BYTES as usize => {
                let bytes = values
                    .iter()
                    .map(Value::as_u64)
                    .collect::<Option<Vec<_>>>()?
                    .into_iter()
                    .map(u8::try_from)
                    .collect::<Result<Vec<_>, _>>()
                    .ok()?;
                return attachment_from_bytes(bytes, media_type.as_deref());
            }
            Value::Object(_) => return decode_image_part(data, depth + 1),
            _ => {}
        }
    }
    None
}

fn decode_image_source(value: &str, _media_type: Option<&str>) -> Option<ImageAttachment> {
    if value.starts_with("data:") {
        return decode_data_uri(value);
    }
    // 会话文件是不可信输入，不能允许其中的 path/file:// 指向任意本地文件。
    // 各适配器只解析会话自身携带的 data URI/base64/byte-array 图片。
    None
}

fn decode_base64_image(data: &str, media_type: Option<&str>) -> Option<ImageAttachment> {
    if data.len() > MAX_IMAGE_B64 {
        return None;
    }
    let compact;
    let data = if data.bytes().any(|byte| byte.is_ascii_whitespace()) {
        compact = data
            .bytes()
            .filter(|byte| !byte.is_ascii_whitespace())
            .collect::<Vec<_>>();
        compact.as_slice()
    } else {
        data.as_bytes()
    };
    let bytes = BASE64
        .decode(data)
        .or_else(|_| BASE64_URL_SAFE.decode(data))
        .ok()?;
    attachment_from_bytes(bytes, media_type)
}

/// `data:image/png;base64,...` → 图片附件。只接受 image MIME 与 base64，
/// 不下载 http(s) URL。
pub fn decode_data_uri(uri: &str) -> Option<ImageAttachment> {
    let rest = uri.strip_prefix("data:")?;
    let (metadata, data) = rest.split_once(',')?;
    let mut fields = metadata.split(';');
    let media_type = fields.next()?;
    if !media_type.starts_with("image/") || !fields.any(|field| field == "base64") {
        return None;
    }
    decode_base64_image(data, Some(media_type))
}

fn attachment_from_bytes(bytes: Vec<u8>, media_type: Option<&str>) -> Option<ImageAttachment> {
    if bytes.is_empty() || bytes.len() as u64 > MAX_IMAGE_BYTES {
        return None;
    }
    let media_type = media_type
        .filter(|value| value.starts_with("image/"))
        .map(str::to_string)
        .or_else(|| sniff_image_media_type(&bytes).map(str::to_string))?;
    if !consume_image_decode_budget(bytes.len()) {
        return None;
    }
    Some(ImageAttachment {
        media_type,
        bytes,
        text_offset: 0,
    })
}

fn sniff_image_media_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else if bytes.starts_with(b"BM") {
        Some("image/bmp")
    } else if bytes.starts_with(b"II*\0") || bytes.starts_with(b"MM\0*") {
        Some("image/tiff")
    } else if std::str::from_utf8(&bytes[..bytes.len().min(512)])
        .ok()
        .is_some_and(|head| head.contains("<svg"))
    {
        Some("image/svg+xml")
    } else {
        None
    }
}

pub const IMAGE_PLACEHOLDER: &str = "[image]";

/// 拆掉 Codex Desktop 文件清单包装，但保留 `My request` 后的真实提问。
pub fn unwrap_file_preamble(text: &str) -> Option<String> {
    const HEADS: &[&str] = &[
        "# Files mentioned by the user",
        "# Files pasted by the user",
    ];
    const MARKERS: &[&str] = &["## My request for Codex:", "## My request:"];
    let text = text.trim_start();
    if !HEADS.iter().any(|head| text.starts_with(head)) {
        return None;
    }
    Some(
        MARKERS
            .iter()
            .find_map(|marker| text.find(marker).map(|index| (marker, index)))
            .map(|(marker, index)| text[index + marker.len()..].trim().to_string())
            .unwrap_or_default(),
    )
}

/// 剥掉 Codex Desktop 写入正文的 `<image ...>` 标签。真正的图片通常同时
/// 存在于 input_image 内容块；临时文件路径失效时不再污染正文。
pub fn strip_image_tags(text: &str) -> String {
    const INJECTED_PREFIX: &str = "<image name=[Image #";
    if !text.contains(INJECTED_PREFIX) {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(INJECTED_PREFIX) {
        out.push_str(&rest[..start]);
        let after = &rest[start..];
        let Some(open_end) = after.find('>').map(|index| index + 1) else {
            out.push_str(after);
            return out;
        };
        let open_tag = &after[..open_end];
        if !open_tag.contains("] path=") {
            out.push_str(&after[..INJECTED_PREFIX.len()]);
            rest = &after[INJECTED_PREFIX.len()..];
            continue;
        }
        let end = after[open_end..]
            .find("</image>")
            .map(|index| open_end + index + "</image>".len())
            .unwrap_or(open_end);
        rest = &after[end..];
    }
    out.push_str(rest);
    out.trim().to_string()
}

/// 按 mtime 失效的单值缓存(边车映射/SQLite 全表在一轮扫描内的重复调用)。
/// build 返回 None 时不缓存失败结果,mtime 不变也会在下次重试。
pub struct MtimeCache<T: Clone>(std::sync::Mutex<Option<(i64, T)>>);

impl<T: Clone> MtimeCache<T> {
    pub fn new() -> Self {
        Self(std::sync::Mutex::new(None))
    }

    pub fn get_or_try_build(&self, mtime: i64, build: impl FnOnce() -> Option<T>) -> Option<T> {
        {
            let cache = self.0.lock().unwrap();
            if let Some((t, v)) = cache.as_ref() {
                if *t == mtime {
                    return Some(v.clone());
                }
            }
        }
        let v = build()?;
        *self.0.lock().unwrap() = Some((mtime, v.clone()));
        Some(v)
    }
}

/// tool_use 块 → ToolCallView(preview + pretty-print input 三件套统一)
pub fn tool_call_view(
    id: String,
    name: &str,
    input: &Value,
    output: Option<String>,
    is_error: bool,
) -> ToolCallView {
    let input_json = if input.is_null() {
        String::new()
    } else {
        serde_json::to_string_pretty(input).unwrap_or_default()
    };
    ToolCallView {
        id,
        name: if name.is_empty() {
            "tool".to_string()
        } else {
            name.to_string()
        },
        input_preview: make_preview(input),
        input: if input_json.is_empty() {
            None
        } else {
            Some(clip(&input_json, MAX_TOOL_IO).0)
        },
        output: output.map(|o| clip(&o, MAX_TOOL_IO).0),
        is_error,
        sidechain_ref: None,
    }
}

/// 截断到 max 字符(按 char 边界),返回 (text, truncated)
pub fn clip(s: &str, max: usize) -> (String, bool) {
    if s.len() <= max {
        return (s.to_string(), false);
    }
    // 找不超过 max 的最大 char 边界
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    (format!("{}\n… (truncated)", &s[..end]), true)
}

/// RFC3339 字符串 → epoch ms,解析失败 = 0
pub fn iso_ms(s: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(s)
        .map(|d| d.timestamp_millis())
        .unwrap_or(0)
}

/// SQLite `datetime('now')` 风格("2026-05-15 06:23:37" 或带 T/毫秒/Z)→ epoch ms(按 UTC)
pub fn sqlite_dt_ms(s: &str) -> i64 {
    let ms = iso_ms(s);
    if ms > 0 {
        return ms;
    }
    let t = s.replace(' ', "T");
    let with_z = if t.ends_with('Z') { t } else { format!("{t}Z") };
    iso_ms(&with_z)
}

/// ISO8601 字符串或 unix 秒/毫秒 → epoch ms
pub fn to_epoch_ms(v: &Value) -> i64 {
    match v {
        Value::Number(n) => {
            let f = n.as_f64().unwrap_or(0.0);
            if f > 1e12 {
                f as i64
            } else if f > 0.0 {
                (f * 1000.0) as i64
            } else {
                0
            }
        }
        Value::String(s) => iso_ms(s),
        _ => 0,
    }
}

/// 清洗标题候选:剥 slash-command 壳与 system-reminder 等标签,压单行截断。空串=不可用
pub fn clean_title_candidate(raw: &str) -> String {
    let mut s = strip_tag_block(raw, "system-reminder");
    s = strip_tag_block(&s, "local-command-caveat");
    s = strip_tag_block(&s, "local-command-stdout");

    let args = extract_tag(&s, "command-args");
    let name = extract_tag(&s, "command-name");
    if args.is_some() || name.is_some() {
        s = args
            .filter(|a| !a.trim().is_empty())
            .or(name)
            .unwrap_or_default();
    }
    // 去掉残余短标签
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '<' {
            let mut tag = String::new();
            let mut closed = false;
            for c2 in chars.by_ref() {
                if c2 == '>' {
                    closed = true;
                    break;
                }
                if c2 == '\n' || tag.len() > 60 {
                    break;
                }
                tag.push(c2);
            }
            if !closed {
                out.push('<');
                out.push_str(&tag);
            } else {
                out.push(' ');
            }
        } else {
            out.push(c);
        }
    }
    let compact = out
        .replace(IMAGE_PLACEHOLDER, " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if compact.is_empty() {
        return String::new();
    }
    let chars: Vec<char> = compact.chars().collect();
    if chars.len() > MAX_TITLE {
        let mut t: String = chars[..MAX_TITLE].iter().collect();
        t.push('…');
        t
    } else {
        compact
    }
}

fn strip_tag_block(s: &str, tag: &str) -> String {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    loop {
        match rest.find(&open) {
            None => {
                out.push_str(rest);
                return out;
            }
            Some(i) => {
                out.push_str(&rest[..i]);
                out.push(' ');
                match rest[i..].find(&close) {
                    None => return out,
                    Some(j) => rest = &rest[i + j + close.len()..],
                }
            }
        }
    }
}

pub(crate) fn extract_tag(s: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let i = s.find(&open)?;
    let j = s[i + open.len()..].find(&close)?;
    Some(s[i + open.len()..i + open.len() + j].to_string())
}

/// IDE/CLI 注入型"用户"消息:非用户手打,详情里应归 Meta 折叠,不参与标题推导。
pub fn is_injected_user_content(text: &str) -> bool {
    let t = text.trim_start();
    const PREFIXES: &[&str] = &[
        "<recommended_plugins",
        "<environment_context",
        "<user_action",
        "<user_instructions",
        "<permissions",
        "<workspace",
        "<system-",
        "<context ",
        "<session_context",
        "IMPORTANT: Do NOT read",
        "Caveat: The messages below",
        "# Files pasted by the user",
        "# Files mentioned by the user",
        "## Referenced ChatGPT conversation",
        // Codex 把 AGENTS.md 作为 user 消息注入
        "# AGENTS.md instructions",
        // Codex 的 subagent / 分支线程:父会话的整段 transcript 被打包成
        // 一条 role=user 消息喂进来,里面含父会话的 assistant 输出。不认它
        // 的话,父会话里 AI 说的话会显示成这个会话里用户发的
        "The following is the Codex agent history",
    ];
    if PREFIXES.iter().any(|p| t.starts_with(p)) {
        return true;
    }
    // Codex 的 skill 展开体:skill-name + SKILL.md 路径 + frontmatter(不带 $,
    // 用户手打的 "$skill" 引用是另一条独立消息,不受影响)。路径子串两种
    // 分隔符都认——Windows 上 Codex 写进日志的是反斜杠路径,只匹配 '/'
    // 会让展开体漏过滤、灌进正文与 FTS(2026-08-25 review)
    t.contains("/.codex/plugins/")
        || t.contains(r"\.codex\plugins\")
        || ((t.contains("/plugins/cache/") || t.contains(r"\plugins\cache\"))
            && t.contains("SKILL.md"))
}

/// tool 输入的单行摘要
pub fn make_preview(input: &Value) -> String {
    const MAX: usize = 200;
    let cand = if let Value::Object(obj) = input {
        [
            "command",
            "file_path",
            "path",
            "pattern",
            "query",
            "url",
            "description",
        ]
        .iter()
        .find_map(|k| {
            obj.get(*k)
                .and_then(|v| v.as_str())
                .filter(|s| !s.trim().is_empty())
        })
        .map(|s| s.to_string())
        .or_else(|| serde_json::to_string(input).ok())
    } else if let Value::String(s) = input {
        Some(s.clone())
    } else {
        serde_json::to_string(input).ok()
    };
    let one = cand
        .unwrap_or_default()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let chars: Vec<char> = one.chars().collect();
    if chars.len() > MAX {
        let mut t: String = chars[..MAX].iter().collect();
        t.push('…');
        t
    } else {
        one
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const IMAGE_SHAPES: &str = include_str!("../../tests/fixtures/image_content_shapes.json");

    #[test]
    fn readable_agent_image_shapes_decode_only_for_transcripts() {
        let fixtures: Vec<Value> = serde_json::from_str(IMAGE_SHAPES).expect("image fixtures");
        assert_eq!(fixtures.len(), 7);

        for fixture in fixtures {
            let name = fixture["name"].as_str().expect("fixture name");
            let content = &fixture["content"];

            let indexed = content_parts(content, false);
            assert_eq!(indexed.text, IMAGE_PLACEHOLDER, "index shape {name}");
            assert!(indexed.images.is_empty(), "index bytes {name}");

            let transcript = content_parts(content, true);
            assert!(transcript.text.is_empty(), "transcript text {name}");
            assert_eq!(transcript.images.len(), 1, "transcript image {name}");
            assert_eq!(transcript.images[0].media_type, "image/png", "mime {name}");
            assert!(
                transcript.images[0].bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
                "png bytes {name}"
            );
        }
    }

    #[test]
    fn image_parser_is_bounded_and_never_fetches_remote_urls() {
        let remote = serde_json::json!({
            "type": "image",
            "image_url": "https://example.com/private.png"
        });
        let parsed = content_parts(&remote, true);
        assert_eq!(parsed.text, IMAGE_PLACEHOLDER);
        assert!(parsed.images.is_empty());

        let non_image = serde_json::json!({
            "type": "document",
            "source": {"type": "base64", "media_type": "application/pdf", "data": "AA=="}
        });
        assert!(!is_image_part(&non_image));
        let parsed = content_parts(&non_image, true);
        assert!(parsed.text.is_empty());
        assert!(parsed.images.is_empty());

        let oversized = format!("data:image/png;base64,{}", "A".repeat(MAX_IMAGE_B64 + 1));
        assert!(decode_data_uri(&oversized).is_none());
    }

    #[test]
    fn transcript_image_budget_is_aggregate_and_restores_after_scope() {
        let png = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";
        let image = serde_json::json!({
            "type": "image",
            "media_type": "image/png",
            "data": png
        });
        {
            let _budget = image_decode_budget(Some(100));
            assert_eq!(content_parts(&image, true).images.len(), 1);
            let rejected = content_parts(&image, true);
            assert!(rejected.images.is_empty());
            assert_eq!(rejected.text, IMAGE_PLACEHOLDER);
        }
        assert_eq!(content_parts(&image, true).images.len(), 1);
    }

    #[test]
    fn image_parser_handles_wrappers_mimeless_data_and_tool_fallbacks() {
        let png = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";
        let wrapped = serde_json::json!({
            "content": {"type": "image", "data": png}
        });
        let parsed = content_parts(&wrapped, true);
        assert_eq!(parsed.images.len(), 1);
        assert_eq!(parsed.images[0].media_type, "image/png");

        let object = serde_json::json!({"exit_code": 0, "duration_ms": 12});
        let parsed = tool_result_parts(&object, true);
        assert!(parsed.images.is_empty());
        assert_eq!(parsed.text, r#"{"duration_ms":12,"exit_code":0}"#);
    }

    #[test]
    fn image_parser_preserves_text_image_order() {
        let png = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";
        let content = serde_json::json!([
            {"type": "text", "text": "前面的说明"},
            {"type": "image", "media_type": "image/png", "data": png},
            {"type": "text", "text": "后面的说明"}
        ]);
        let parsed = content_parts(&content, true);
        assert_eq!(parsed.text, "前面的说明\n\n后面的说明");
        assert_eq!(parsed.images.len(), 1);
        assert_eq!(parsed.images[0].text_offset, "前面的说明".len());
    }

    #[test]
    fn image_parser_never_dereferences_local_paths() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("secret.png");
        std::fs::write(&path, b"not really an image").expect("fixture");
        let content = serde_json::json!({
            "type": "image",
            "media_type": "image/png",
            "path": path
        });
        let parsed = content_parts(&content, true);
        assert_eq!(parsed.text, IMAGE_PLACEHOLDER);
        assert!(parsed.images.is_empty());
    }

    #[test]
    fn codex_wrappers_keep_only_the_user_request() {
        let wrapped = concat!(
            "# Files mentioned by the user:\n\n",
            "## Screenshot.png: /tmp/Screenshot.png\n\n",
            "## My request:\n",
            "看看这里 <image name=[Image #1] path=\"/tmp/missing.png\">ignored</image> 怎么了"
        );
        let request = unwrap_file_preamble(wrapped).expect("wrapped request");
        assert_eq!(strip_image_tags(&request), "看看这里  怎么了");
        let ordinary = "为什么 <image src=\"avatar.png\"></image> 没有显示？";
        assert_eq!(strip_image_tags(ordinary), ordinary);
        assert_eq!(clean_title_candidate("[image] 看看这里"), "看看这里");
    }

    #[test]
    fn title_skips_pure_image_messages() {
        let mut image = text_msg(Role::User, IMAGE_PLACEHOLDER, 1);
        image.images.push(ImageAttachment {
            media_type: "image/png".to_string(),
            bytes: vec![1],
            text_offset: 0,
        });
        let text = text_msg(Role::User, "后面的真实问题", 2);
        assert_eq!(
            title_from_messages(&[image, text]).as_deref(),
            Some("后面的真实问题")
        );
    }
}
