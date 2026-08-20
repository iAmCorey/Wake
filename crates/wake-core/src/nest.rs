//! 把已按 SQL 取好的顶层会话与孩子拼成可展开列表。
use crate::models::*;
use std::collections::{HashMap, HashSet};

pub fn nest_session_rows(
    roots: Vec<SessionMeta>,
    child_counts: &HashMap<String, i64>,
    children: &HashMap<String, Vec<SessionMeta>>,
    expanded: &HashSet<String>,
) -> Vec<SessionRow> {
    let mut out = Vec::new();
    for root in roots {
        let child_count = child_counts.get(&root.key).copied().unwrap_or(0) as usize;
        let is_expanded = child_count > 0 && expanded.contains(&root.key);
        let key = root.key.clone();
        out.push(SessionRow {
            meta: root,
            depth: 0,
            child_count,
            expanded: is_expanded,
        });
        if is_expanded {
            if let Some(kids) = children.get(&key) {
                for kid in kids {
                    out.push(SessionRow {
                        meta: kid.clone(),
                        depth: 1,
                        child_count: 0,
                        expanded: false,
                    });
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(key: &str, title: &str) -> SessionMeta {
        SessionMeta {
            key: key.into(),
            id: key.split(':').nth(1).unwrap_or(key).into(),
            agent: AgentId::Grok,
            title: title.into(),
            project_path: "/p".into(),
            project_name: "Grok".into(),
            file_path: format!("/{key}"),
            created_at: 1,
            updated_at: 1,
            message_count: 1,
            size_bytes: 1,
            git_branch: None,
            model: None,
            tokens_used: None,
            archived: false,
            source: None,
            favorite: false,
            pinned: false,
        }
    }

    #[test]
    fn collapsed_hides_children() {
        let roots = vec![s("grok:orch", "调研终端"), s("grok:other", "别的")];
        let mut counts = HashMap::new();
        counts.insert("grok:orch".into(), 2);
        let mut children = HashMap::new();
        children.insert(
            "grok:orch".into(),
            vec![s("grok:c1", "A"), s("grok:c2", "B")],
        );
        let rows = nest_session_rows(roots, &counts, &children, &HashSet::new());
        let titles: Vec<_> = rows.iter().map(|r| r.meta.title.as_str()).collect();
        assert_eq!(titles, vec!["调研终端", "别的"]);
        assert_eq!(rows[0].child_count, 2);
        assert!(!rows[0].expanded);
    }

    #[test]
    fn expanded_inserts_children_below_parent() {
        let roots = vec![s("grok:orch", "调研终端")];
        let mut counts = HashMap::new();
        counts.insert("grok:orch".into(), 2);
        let mut children = HashMap::new();
        children.insert(
            "grok:orch".into(),
            vec![s("grok:c1", "A"), s("grok:c2", "B")],
        );
        let mut open = HashSet::new();
        open.insert("grok:orch".into());
        let rows = nest_session_rows(roots, &counts, &children, &open);
        let titles: Vec<_> = rows
            .iter()
            .map(|r| (r.meta.title.as_str(), r.depth))
            .collect();
        assert_eq!(titles, vec![("调研终端", 0), ("A", 1), ("B", 1)]);
        assert!(rows[0].expanded);
    }

    #[test]
    fn flat_mode_no_chevrons_when_counts_empty() {
        let roots = vec![s("grok:c", "orphan"), s("grok:orch", "调研终端")];
        let rows = nest_session_rows(roots, &HashMap::new(), &HashMap::new(), &HashSet::new());
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r.depth == 0 && r.child_count == 0));
    }
}
