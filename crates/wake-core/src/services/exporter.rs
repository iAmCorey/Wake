use crate::adapters::AgentAdapter;
use crate::models::*;
use chrono::{Local, TimeZone};

fn fmt_time(ts: Option<i64>) -> String {
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

fn role_label(role: Role) -> &'static str {
    match role {
        Role::User => "👤 User",
        Role::Assistant => "🤖 Assistant",
        Role::System => "⚙️ System",
    }
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
