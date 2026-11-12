//! IDE side questions live in the local catalog, not in CBC JSONL files.
use super::parse_utils::*;
use crate::models::*;
use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};

#[derive(Clone, Debug, Hash, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BtwEntry {
    id: String,
    #[serde(default)]
    conversation_id: String,
    question: String,
    #[serde(default)]
    answer: String,
    status: String,
    #[serde(default)]
    error: String,
    #[serde(default)]
    image_count: usize,
    created_at: i64,
    updated_at: i64,
}

pub(super) fn read(db: &rusqlite::Connection) -> Result<HashMap<String, Option<Vec<BtwEntry>>>> {
    if !super::sqlite_ro::has_table(db, "btw_entries")? {
        return Ok(HashMap::new());
    }
    let mut stmt = db.prepare("SELECT conversation_id, payload FROM btw_entries")?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
    let mut out = HashMap::new();
    for row in rows {
        let (id, payload) = row?;
        let entries = (payload.len() <= 16 * 1024 * 1024)
            .then(|| serde_json::from_str::<Vec<BtwEntry>>(&payload).ok())
            .flatten()
            .map(|mut entries| {
                entries.retain(|e| {
                    !e.id.is_empty() && (e.conversation_id.is_empty() || e.conversation_id == id)
                });
                entries.sort_by_key(|e| std::cmp::Reverse(e.updated_at));
                let mut seen = HashSet::new();
                entries.retain(|e| seen.insert(e.id.clone()));
                entries.sort_by_key(|e| e.created_at);
                entries
            });
        // None preserves this session's existing index on a malformed payload.
        out.insert(id, entries);
    }
    Ok(out)
}

pub(super) fn merge(
    messages: &mut Vec<TranscriptMessage>,
    entries: Option<&[BtwEntry]>,
) -> Result<()> {
    let entries = entries.context("CodeBuddy side questions are temporarily unreadable")?;
    if entries.is_empty() {
        return Ok(());
    }
    let mut groups = Vec::new();
    for e in entries {
        let mut group = Vec::new();
        for (role, text, ts) in [
            (Role::User, &e.question, e.created_at),
            (Role::Assistant, &e.answer, e.updated_at),
        ] {
            if text.trim().is_empty() {
                continue;
            }
            let mut message = text_msg(role, text, ts);
            message.kind = MessageKind::Aside;
            group.push(message);
            if role == Role::User && e.image_count > 0 {
                // The catalog records a count but no blob IDs or image paths.
                // Never guess which main-thread attachment belongs here.
                let mut notice = meta_msg(
                    &format!("[Missing side-question images: {}]", e.image_count),
                    ts,
                );
                notice.kind = MessageKind::SourceWarning;
                group.push(notice);
            }
        }
        if e.status != "done" {
            let label = if e.status == "error" {
                "Side question failed"
            } else {
                "Side question incomplete"
            };
            let mut notice = meta_msg(&format!("[{label}] {}", e.error), e.updated_at);
            notice.kind = MessageKind::SourceWarning;
            group.push(notice);
        }
        if !group.is_empty() {
            groups.push((e.created_at, group));
        }
    }
    // Insert whole Q/A pairs by question time, preserving the JSONL's order.
    // Sorting the entire transcript would reorder tool calls with delayed times.
    let mut pending = groups.into_iter().peekable();
    let mut merged = Vec::with_capacity(messages.len() + entries.len() * 2);
    for message in messages.drain(..) {
        if let Some(ts) = message.timestamp {
            while pending
                .peek()
                .is_some_and(|(created, _)| *created > 0 && *created <= ts)
            {
                merged.extend(pending.next().unwrap().1);
            }
        }
        merged.push(message);
    }
    for (_, group) in pending {
        merged.extend(group);
    }
    assign_seq(&mut merged);
    *messages = merged;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::{codebuddy::CodebuddyAdapter, codebuddy_legacy, AgentAdapter};

    #[test]
    #[ignore = "reads real local CodeBuddy side questions and transcripts"]
    fn real_side_questions_round_trip_into_search_units() {
        let catalog = codebuddy_legacy::read_catalog(&codebuddy_legacy::catalog_path()).unwrap();
        let adapter = CodebuddyAdapter::new();
        let refs = adapter.list_session_files().unwrap();
        let mut questions = 0;
        let mut answers = 0;
        let mut failures = 0;
        for row in catalog
            .iter()
            .filter(|r| !r.deleted && r.authority.is_empty())
        {
            let entries = row.btw.as_deref().expect("readable side questions");
            if entries.is_empty() {
                continue;
            }
            let runtime = if row.runtime_id.is_empty() {
                &row.id
            } else {
                &row.runtime_id
            };
            let reference = refs
                .iter()
                .find(|r| {
                    std::path::Path::new(&r.file_path)
                        .file_stem()
                        .is_some_and(|s| s == runtime.as_str())
                })
                .expect("CBC transcript exists");
            let parsed = adapter.parse_session(reference).unwrap();
            let transcript = adapter.parse_transcript(reference).unwrap();
            for entry in entries {
                for (role, text) in [
                    (Role::User, &entry.question),
                    (Role::Assistant, &entry.answer),
                ] {
                    if text.trim().is_empty() {
                        continue;
                    }
                    let message = transcript
                        .mainline
                        .iter()
                        .find(|m| {
                            m.kind == MessageKind::Aside && m.role == role && m.text == text.trim()
                        })
                        .expect("side content retained");
                    assert!(parsed
                        .units
                        .iter()
                        .any(|u| u.seq == message.seq && u.role == role && u.text == text.trim()));
                    if role == Role::User {
                        questions += 1;
                    } else {
                        answers += 1;
                    }
                }
                if entry.status == "error" {
                    failures += 1;
                }
            }
        }
        assert!(questions > 0 && answers > 0);
        println!("side question audit: questions={questions}, answers={answers}, source_errors={failures}");
    }
}
