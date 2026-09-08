use crate::adapters::AgentAdapter;
use crate::models::*;
use chrono::{Local, TimeZone};

/// 本地时区 `YYYY-MM-DD HH:MM:SS`;0/None 为空串(导出与 wake-mcp 共用)
pub fn fmt_time(ts: Option<i64>) -> String {
    match ts {
        Some(t) if t > 0 => Local
            .timestamp_millis_opt(t)
            .single()
            .map(|d| d.format("%Y-%m-%d %H:%M:%S").to_string())
            .unwrap_or_default(),
        _ => String::new(),
    }
}

pub fn fmt_tokens(n: Option<i64>) -> String {
    match n {
        None | Some(0) => "-".to_string(),
        Some(n) if n >= 1_000_000_000 => format!("{:.1}B", n as f64 / 1e9),
        Some(n) if n >= 1_000_000 => format!("{:.1}M", n as f64 / 1e6),
        Some(n) if n >= 1_000 => format!("{:.1}K", n as f64 / 1e3),
        Some(n) => n.to_string(),
    }
}

fn role_label(role: Role) -> String {
    let emoji = match role {
        Role::User => "👤",
        Role::Assistant => "🤖",
        Role::System => "⚙️",
    };
    format!("{emoji} {}", role.title())
}

fn render_message(m: &TranscriptMessage, out: &mut String) {
    out.push_str(&format!(
        "### {}{}\n\n",
        role_label(m.role),
        m.timestamp
            .map(|t| format!(" · {}", fmt_time(Some(t))))
            .unwrap_or_default()
    ));
    if m.kind == MessageKind::CompactSummary {
        out.push_str(&format!("> {}\n\n", m.text));
        return;
    }
    if let Some(th) = &m.thinking {
        out.push_str(&format!(
            "<details><summary>🧠 Thinking</summary>\n\n{th}\n\n</details>\n\n"
        ));
    }
    let mut cursor = 0usize;
    for image in &m.images {
        let mut offset = image.text_offset.min(m.text.len());
        while offset > cursor && !m.text.is_char_boundary(offset) {
            offset -= 1;
        }
        offset = offset.max(cursor);
        if offset > cursor {
            out.push_str(&m.text[cursor..offset]);
            out.push_str("\n\n");
        }
        out.push_str(&format!(
            "> 🖼 Image attachment · {} · {} bytes\n\n",
            image.media_type,
            image.bytes.len()
        ));
        cursor = offset;
    }
    if cursor < m.text.len() {
        out.push_str(&m.text[cursor..]);
        out.push_str("\n\n");
    }
    for tc in &m.tool_calls {
        out.push_str(&format!(
            "<details><summary>🔧 {} — {}</summary>\n\n",
            tc.name,
            tc.input_preview.replace('<', "&lt;")
        ));
        if let Some(input) = &tc.input {
            out.push_str(&format!("Input:\n\n```\n{input}\n```\n\n"));
        }
        if let Some(output) = &tc.output {
            out.push_str(&format!("Output:\n\n```\n{output}\n```\n\n"));
        }
        out.push_str("</details>\n\n");
    }
}

pub fn to_markdown(
    meta: &SessionMeta,
    messages: &[TranscriptMessage],
    sidechains: &[(SidechainInfo, Vec<TranscriptMessage>)],
) -> String {
    let mut out = String::new();
    out.push_str(&format!("# {}\n\n", meta.title));
    out.push_str(&format!(
        "> **Agent**: {} · **Project**: {}{} · **Time**: {} – {} · **Messages**: {} · **Tokens**: {}\n\n---\n\n",
        meta.agent.display_name(),
        if meta.project_path.is_empty() { "(unknown)" } else { &meta.project_path },
        meta.git_branch.as_deref().map(|b| format!("({b})")).unwrap_or_default(),
        fmt_time(Some(meta.created_at)),
        fmt_time(Some(meta.updated_at)),
        meta.message_count,
        fmt_tokens(meta.tokens_used),
    ));
    for m in messages {
        if m.kind == MessageKind::Meta {
            continue;
        }
        render_message(m, &mut out);
    }
    for (sc, msgs) in sidechains {
        if msgs.is_empty() {
            continue;
        }
        let label = [sc.agent_type.as_deref(), sc.description.as_deref()]
            .iter()
            .flatten()
            .copied()
            .collect::<Vec<_>>()
            .join(":");
        out.push_str(&format!(
            "---\n\n## ⑂ Subagent: {}\n\n",
            if label.is_empty() { &sc.id } else { &label }
        ));
        for m in msgs {
            if m.kind == MessageKind::Meta {
                continue;
            }
            render_message(m, &mut out);
        }
    }
    out
}

pub fn to_json(
    meta: &SessionMeta,
    messages: &[TranscriptMessage],
    sidechains: &[(SidechainInfo, Vec<TranscriptMessage>)],
) -> String {
    let sc_json: Vec<serde_json::Value> = sidechains
        .iter()
        .map(|(sc, msgs)| {
            serde_json::json!({
                "id": sc.id, "agentType": sc.agent_type, "description": sc.description,
                "messages": msgs,
            })
        })
        .collect();
    serde_json::to_string_pretty(&serde_json::json!({
        "exportedAt": chrono::Utc::now().to_rfc3339(),
        "exportedBy": "wake",
        "session": meta,
        "messages": messages,
        "sidechains": sc_json,
    }))
    .unwrap_or_default()
}

/// 导出默认文件名:agent-标题-日期.ext
pub fn default_file_name(meta: &SessionMeta, ext: &str) -> String {
    let title: String = meta
        .title
        .chars()
        .filter(|c| !r#"/\:*?"<>|"#.contains(*c))
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(40)
        .collect();
    let date = Local
        .timestamp_millis_opt(if meta.updated_at > 0 {
            meta.updated_at
        } else {
            crate::db::now_ms()
        })
        .single()
        .map(|d| d.format("%Y%m%d").to_string())
        .unwrap_or_default();
    format!(
        "{}-{}-{}.{ext}",
        meta.agent.as_str(),
        if title.is_empty() { "session" } else { &title },
        date
    )
}

/// 一站式导出:按 meta 解析主线与全部子会话,渲染成 Markdown。错误原样上抛,UI 拼进通知
pub fn render_markdown(adapter: &dyn AgentAdapter, meta: &SessionMeta) -> anyhow::Result<String> {
    // from_meta 对虚拟路径(SQLite 型)自动回退,导出不依赖真实文件存在
    let r = SessionFileRef::from_meta(meta);
    let t = adapter.parse_transcript(&r)?;
    let sidechains: Vec<(SidechainInfo, Vec<TranscriptMessage>)> = t
        .sidechains
        .iter()
        .map(|sc| {
            let msgs = adapter.load_sidechain(&r, &sc.id).unwrap_or_default();
            (sc.clone(), msgs)
        })
        .collect();
    Ok(to_markdown(&t.meta, &t.mainline, &sidechains))
}

// ---------------------------------------------------------------- 精简渲染(wake-mcp)

/// `render_compact` 的参数。默认值按"喂给另一个 agent 当上下文"调:不带工具
/// 输出与 thinking,单条与整页都有字符硬顶,免得一个大会话吃光对方的窗口
#[derive(Debug, Clone)]
pub struct CompactOptions {
    /// 从该 seq 起(含)
    pub from_seq: i64,
    /// 本页最多输出多少条消息(Meta 不计)
    pub max_messages: usize,
    /// 本页正文字符上限(至少给出一条,超出即停并给 next_seq)
    pub max_chars: usize,
    /// 单条消息正文字符上限,超出截断并标注
    pub max_message_chars: usize,
    /// 带工具调用的输入/输出正文(默认只留一行名称加输入预览)
    pub include_tools: bool,
    /// 带 thinking
    pub include_thinking: bool,
}

impl Default for CompactOptions {
    fn default() -> Self {
        Self {
            from_seq: 0,
            max_messages: 60,
            max_chars: 20_000,
            max_message_chars: 4_000,
            include_tools: false,
            include_thinking: false,
        }
    }
}

/// 一页精简转录
#[derive(Debug, Clone)]
pub struct CompactPage {
    pub text: String,
    /// 下一页起点 seq;None = 已到主线末尾
    pub next_seq: Option<i64>,
    /// 本页输出的消息数
    pub rendered: usize,
    /// 本页范围内被跳过的注入上下文(Meta)条数
    pub skipped_meta: usize,
    /// 本页首/末条消息的 seq(rendered = 0 时 None)
    pub seq_range: Option<(i64, i64)>,
}

/// 一条消息最多列出多少条工具调用(pi 系把一整轮助手回复合并成一条,里面可能
/// 有上百次调用);超出的只报条数
const MAX_TOOL_LINES: usize = 40;

/// 省 token 的转录渲染:只出 user/assistant 正文,工具调用折成一行,Meta(注入
/// 上下文)跳过并计数,CompactSummary 保留为引用块,图片略去;按 seq 分页。
/// `max_message_chars` 是**整条消息**的预算:正文、thinking、工具行与工具
/// 输入输出共用一份——分页对本页首条消息不查总量(保证前进),单条消息自身
/// 必须有界,否则一轮上百次工具调用能撑爆对方上下文
/// 与 `to_markdown`(全量导出)并存,seq 编号同源(`TranscriptMessage::seq`),
/// 搜索命中的 `wake://session/<key>#<seq>` 引用直接对得上这里的 `[seq N]`
pub fn render_compact(messages: &[TranscriptMessage], o: &CompactOptions) -> CompactPage {
    let mut text = String::new();
    let mut rendered = 0usize;
    let mut skipped_meta = 0usize;
    let mut chars = 0usize;
    let mut next_seq = None;
    let mut first_seq = None;
    let mut last_seq = None;
    for m in messages.iter().filter(|m| m.seq >= o.from_seq) {
        if m.kind == MessageKind::Meta {
            skipped_meta += 1;
            continue;
        }
        let block = compact_block(m, o);
        let block_chars = block.chars().count();
        if rendered > 0 && (rendered >= o.max_messages || chars + block_chars > o.max_chars) {
            next_seq = Some(m.seq);
            break;
        }
        text.push_str(&block);
        chars += block_chars;
        rendered += 1;
        first_seq.get_or_insert(m.seq);
        last_seq = Some(m.seq);
    }
    CompactPage {
        text,
        next_seq,
        rendered,
        skipped_meta,
        seq_range: first_seq.zip(last_seq),
    }
}

fn compact_block(m: &TranscriptMessage, o: &CompactOptions) -> String {
    let role = m.role.title();
    let when = m
        .timestamp
        .map(|t| format!(" · {}", fmt_time(Some(t))))
        .unwrap_or_default();
    let mut out = format!("### [seq {}] {role}{when}\n", m.seq);
    let mut budget = o.max_message_chars;
    if m.kind == MessageKind::CompactSummary {
        out.push_str("> [compacted summary] ");
        out.push_str(&spend(&m.text, &mut budget).replace('\n', "\n> "));
        out.push_str("\n\n");
        return out;
    }
    // 正文优先于 thinking 拿预算,输出顺序仍是 thinking 在前
    let body = spend(m.text.trim(), &mut budget);
    let thinking = if o.include_thinking {
        m.thinking
            .as_deref()
            .filter(|t| !t.trim().is_empty())
            .map(|t| spend(t, &mut budget))
    } else {
        None
    };
    if let Some(th) = thinking {
        out.push_str("> [thinking] ");
        out.push_str(&th.replace('\n', "\n> "));
        out.push_str("\n\n");
    }
    if !body.is_empty() {
        out.push_str(&body);
        out.push_str("\n\n");
    } else if m.tool_calls.is_empty() {
        out.push_str("(empty)\n\n");
    }
    if !m.images.is_empty() {
        out.push_str(&format!(
            "({} image attachment{} omitted)\n\n",
            m.images.len(),
            if m.images.len() == 1 { "" } else { "s" }
        ));
    }
    let mut shown = 0usize;
    for tc in &m.tool_calls {
        // input_preview 由 parse_utils::make_preview 生产,已折行并封顶 200 字符
        let status = if tc.is_error { " ✗" } else { "" };
        let line = format!("- 🔧 {}{status}: {}\n", tc.name, tc.input_preview);
        let line_chars = line.chars().count();
        // 首条工具行无论如何给出(读者要知道这条消息动过工具),之后按预算
        if shown >= MAX_TOOL_LINES || (shown > 0 && line_chars > budget) {
            break;
        }
        out.push_str(&line);
        budget = budget.saturating_sub(line_chars);
        shown += 1;
        if o.include_tools && budget > 0 {
            if let Some(input) = tc.input.as_deref().filter(|s| !s.trim().is_empty()) {
                out.push_str(&format!(
                    "  input:\n  ```\n{}\n  ```\n",
                    indent(&spend(input, &mut budget))
                ));
            }
            if let Some(output) = tc.output.as_deref().filter(|s| !s.trim().is_empty()) {
                out.push_str(&format!(
                    "  output:\n  ```\n{}\n  ```\n",
                    indent(&spend(output, &mut budget))
                ));
            }
        }
    }
    let hidden = m.tool_calls.len() - shown;
    if hidden > 0 {
        out.push_str(&format!(
            "- … {hidden} more tool call{} not shown\n",
            if hidden == 1 { "" } else { "s" }
        ));
    }
    if !m.tool_calls.is_empty() {
        out.push('\n');
    }
    out
}

/// 从消息预算里扣着截:返回按剩余预算截断的文本,并把用掉的字符数记账
fn spend(s: &str, budget: &mut usize) -> String {
    let clipped = clip_chars(s, *budget);
    *budget = budget.saturating_sub(clipped.chars().count());
    clipped
}

/// 按字符截断并标注(字节切会切坏 CJK)。字节数 ≤ max 时字符数必 ≤ max,
/// 先按字节放行,免得每条消息都整段数一遍字符
fn clip_chars(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    match s.char_indices().nth(max) {
        Some((ix, _)) => format!("{}… [truncated]", &s[..ix]),
        None => s.to_string(),
    }
}

fn indent(s: &str) -> String {
    s.lines()
        .map(|l| format!("  {l}"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod compact_tests {
    use super::*;

    fn msg(seq: i64, role: Role, kind: MessageKind, text: &str) -> TranscriptMessage {
        TranscriptMessage {
            seq,
            role,
            kind,
            text: text.to_string(),
            truncated: false,
            tool_calls: Vec::new(),
            thinking: None,
            timestamp: Some(1_754_042_400_000 + seq * 1000),
            model: None,
            images: Vec::new(),
        }
    }

    fn transcript() -> Vec<TranscriptMessage> {
        let mut tool = msg(
            2,
            Role::Assistant,
            MessageKind::Text,
            "Let me edit the file.",
        );
        tool.tool_calls.push(ToolCallView {
            id: "t1".into(),
            name: "Edit".into(),
            input_preview: "src/db.rs".into(),
            input: Some("{\"path\":\"src/db.rs\"}".into()),
            output: Some("ok, 3 lines changed".into()),
            is_error: false,
            sidechain_ref: None,
        });
        tool.thinking = Some("私下想一想".into());
        vec![
            msg(0, Role::User, MessageKind::Text, "帮我实现二维码扫描登录"),
            msg(
                1,
                Role::User,
                MessageKind::Meta,
                "<system-reminder>injected</system-reminder>",
            ),
            tool,
            msg(
                3,
                Role::Assistant,
                MessageKind::Text,
                &"很长的回答。".repeat(50),
            ),
            msg(
                4,
                Role::System,
                MessageKind::CompactSummary,
                "Conversation compacted",
            ),
            msg(5, Role::User, MessageKind::Text, "继续"),
        ]
    }

    #[test]
    fn default_page_skips_meta_and_keeps_tool_names_only() {
        let page = render_compact(&transcript(), &CompactOptions::default());
        assert_eq!(page.rendered, 5);
        assert_eq!(page.skipped_meta, 1);
        assert_eq!(page.next_seq, None);
        assert_eq!(page.seq_range, Some((0, 5)));
        assert!(page.text.contains("### [seq 0] User"));
        assert!(!page.text.contains("injected"));
        assert!(page.text.contains("🔧 Edit: src/db.rs"));
        assert!(!page.text.contains("3 lines changed"), "工具输出默认不带");
        assert!(!page.text.contains("私下想一想"), "thinking 默认不带");
        assert!(page
            .text
            .contains("> [compacted summary] Conversation compacted"));
    }

    #[test]
    fn include_flags_add_tool_io_and_thinking() {
        let o = CompactOptions {
            include_tools: true,
            include_thinking: true,
            ..Default::default()
        };
        let page = render_compact(&transcript(), &o);
        assert!(page.text.contains("3 lines changed"));
        assert!(page.text.contains("[thinking] 私下想一想"));
    }

    #[test]
    fn pagination_by_messages_and_chars_hands_back_next_seq() {
        let o = CompactOptions {
            max_messages: 2,
            ..Default::default()
        };
        let page = render_compact(&transcript(), &o);
        assert_eq!(page.rendered, 2);
        assert_eq!(page.next_seq, Some(3), "第三条非 Meta 消息的 seq");
        let o = CompactOptions {
            from_seq: 3,
            max_chars: 1,
            ..Default::default()
        };
        let page = render_compact(&transcript(), &o);
        assert_eq!(page.rendered, 1, "字符上限再小也至少给一条");
        assert_eq!(page.seq_range, Some((3, 3)));
        assert_eq!(page.next_seq, Some(4));
        let o = CompactOptions {
            from_seq: 5,
            ..Default::default()
        };
        let page = render_compact(&transcript(), &o);
        assert_eq!(page.rendered, 1);
        assert_eq!(page.next_seq, None);
    }

    #[test]
    fn tool_calls_share_the_message_budget() {
        let mut m = msg(
            7,
            Role::Assistant,
            MessageKind::Text,
            "Running the whole suite.",
        );
        for i in 0..120 {
            m.tool_calls.push(ToolCallView {
                id: format!("t{i}"),
                name: "Bash".into(),
                input_preview: format!("cargo test --test case_{i:03}"),
                input: Some(format!("cargo test --test case_{i:03}")),
                output: Some("ok. 12 passed; 0 failed\n".repeat(40)),
                is_error: false,
                sidechain_ref: None,
            });
        }
        let o = CompactOptions {
            max_message_chars: 400,
            include_tools: true,
            ..Default::default()
        };
        let page = render_compact(std::slice::from_ref(&m), &o);
        let tool_lines = page.text.matches("🔧").count();
        assert!(
            tool_lines >= 1 && tool_lines < 120,
            "{tool_lines} tool lines"
        );
        assert!(
            page.text.contains("more tool calls not shown"),
            "{}",
            page.text
        );
        assert!(
            page.text.chars().count() < 400 + 300,
            "block must stay near the per-message budget, got {} chars",
            page.text.chars().count()
        );
        // 预算充足时也不超过 MAX_TOOL_LINES 条
        let roomy = CompactOptions {
            max_message_chars: 100_000,
            ..Default::default()
        };
        let page = render_compact(std::slice::from_ref(&m), &roomy);
        assert_eq!(page.text.matches("🔧").count(), MAX_TOOL_LINES);
        assert!(page.text.contains("80 more tool calls not shown"));
    }

    #[test]
    fn long_messages_are_clipped_on_char_boundaries() {
        let o = CompactOptions {
            max_message_chars: 10,
            ..Default::default()
        };
        let page = render_compact(&transcript(), &o);
        assert!(page.text.contains("很长的回答。很长的回… [truncated]"));
    }
}
