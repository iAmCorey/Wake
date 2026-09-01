use crate::models::*;
use serde_json::Value;

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
        .find(|m| m.role == Role::User && m.kind == MessageKind::Text)
        .map(|m| clean_title_candidate(&m.text))
        .filter(|t| !t.is_empty())
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

/// content blocks 数组 → 纯文本(取各块 "text" 字段,trim 后 join)
pub fn blocks_text(v: &Value) -> String {
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
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
    let compact = out.split_whitespace().collect::<Vec<_>>().join(" ");
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
