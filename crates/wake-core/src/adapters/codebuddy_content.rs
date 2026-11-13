//! CBC stores IDE context and real input in the same user envelope. Classify
//! the envelope first, then remove only leading context blocks from real input.
use super::parse_utils::*;
use crate::models::MessageKind;
use serde_json::Value;
use std::path::{Path, PathBuf};

pub(super) fn user_kind(row: &Value) -> MessageKind {
    let pd = &row["providerData"];
    let text = content_parts(&row["content"], false).text;
    let text = text.trim_start();
    if pd["isSummary"] == true
        || text.starts_with("<cb_summary>")
        || text.starts_with("<conversation_history_summary>")
    {
        return MessageKind::CompactSummary;
    }
    if ["isMeta", "isCompactInternal", "backgroundTaskNotification"]
        .iter()
        .any(|key| pd[*key] == true)
        || pd
            .get("teammateMessage")
            .is_some_and(|v| !v.is_null() && v != &Value::Bool(false))
        || [
            "<teammate-message",
            "<task-notification",
            "<local-command-stdout>",
            "A session-scoped Stop hook",
            "Stop hook feedback:",
        ]
        .iter()
        .any(|p| text.starts_with(p))
    {
        MessageKind::Meta
    } else {
        MessageKind::Text
    }
}

fn input_text(text: &str) -> String {
    let mut rest = text.trim();
    // Attribute-bearing reminders are used by the IDE. A block may also have
    // genuine input after its closing tag; do not discard the whole string.
    loop {
        let Some(after) = rest.strip_prefix("<system-reminder") else {
            break;
        };
        if !after.starts_with('>') && !after.starts_with(char::is_whitespace) {
            break;
        }
        let Some(end) = rest.find("</system-reminder>") else {
            break;
        };
        rest = rest[end + "</system-reminder>".len()..].trim_start();
    }
    if rest.starts_with("<local-command-stdout>") || rest.starts_with("<local-command-caveat>") {
        return String::new();
    }
    if let Some(name) = extract_tag(rest, "command-name") {
        let args = extract_tag(rest, "command-args").unwrap_or_default();
        return format!("{} {}", name.trim(), args.trim())
            .trim()
            .to_string();
    }
    rest.to_string()
}

/// Derive a bounded, content-addressed path. Never dereference blob_path from
/// a transcript: remote absolute paths must resolve inside the host's cache.
pub(crate) fn blob_relative(block: &Value) -> Option<PathBuf> {
    if block["type"] != "image_blob_ref" {
        return None;
    }
    let id = block["blob_id"].as_str()?;
    if id.len() != 64 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let filename = block["blob_path"].as_str()?.rsplit(['/', '\\']).next()?;
    let (stem, ext) = filename.rsplit_once('.')?;
    if stem != id
        || !matches!(
            ext,
            "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "avif" | "heic" | "svg"
        )
    {
        return None;
    }
    Some(Path::new(&id[..2]).join(filename))
}

pub(super) fn content(
    row: &Value,
    decode: bool,
    blobs: Option<&Path>,
    clean_input: bool,
) -> ParsedContent {
    let value = &row["content"];
    let parts = value
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_else(|| std::slice::from_ref(value));
    let mut out = ParsedContent::default();
    let has_blob = parts.iter().any(|p| p["type"] == "image_blob_ref");
    for part in parts {
        let parsed = if part["type"] == "image_blob_ref" {
            let image = decode
                .then(|| {
                    let root = blobs?;
                    image_from_session_file(root, &root.join(blob_relative(part)?))
                })
                .flatten();
            match image {
                Some(image) => ParsedContent {
                    text: String::new(),
                    images: vec![image],
                },
                None => content_parts(part, false),
            }
        } else if clean_input
            && (part.is_string() || matches!(part["type"].as_str(), Some("text" | "input_text")))
        {
            let raw = part
                .as_str()
                .or_else(|| part["text"].as_str())
                .unwrap_or("");
            // The CLI also stores a temporary path label beside the blob.
            if has_blob
                && raw.trim().starts_with("<image_local_path>")
                && raw.trim().ends_with("</image_local_path>")
            {
                continue;
            }
            content_parts(&Value::String(input_text(raw)), false)
        } else {
            content_parts(part, decode)
        };
        out.append(parsed);
    }
    out
}
