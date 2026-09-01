//! Grok 会话归组：主会话登记的 subagent 关系优先，无父会话时再用
//! worktree registry、git remote 与临时 worktree 名称恢复真实项目。

use super::parse_utils::project_name_of;
use super::sqlite_ro::open_sqlite_ro;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;
use std::sync::Arc;

/// 一轮扫描内共享的归组快照，避免每解析一个 updates.jsonl 都重扫目录/SQLite。
#[derive(Clone, Debug, Default)]
pub struct GroupCtx {
    worktrees: Vec<WorktreeRow>,
    parents: HashMap<String, ParentRef>,
}

#[derive(Clone, Debug)]
struct WorktreeRow {
    path: String,
    source_repo: String,
    repo_name: String,
}

/// Grok 主会话目录登记的子代理：child id → 直接父 id 与父会话 cwd。
#[derive(Clone, Debug)]
struct ParentRef {
    parent_id: String,
    parent_cwd: String,
}

pub fn load_group_ctx(sessions_root: &Path, grok_home: &Path) -> Arc<GroupCtx> {
    Arc::new(GroupCtx {
        worktrees: load_worktree_rows(grok_home),
        parents: load_subagent_parents(sessions_root),
    })
}

/// 输出当前数据根能确认的直接关系；scanner 合并全部 Grok location 后再
/// 扁平到 root，否则局部快照提前压平会丢失跨 location 的后半段父链。
pub fn parent_links(ctx: &GroupCtx) -> Vec<(String, String)> {
    ctx.parents
        .iter()
        .filter_map(|(child_id, parent)| {
            if parent.parent_id.is_empty() || parent.parent_id == *child_id {
                None
            } else {
                Some((
                    format!("grok:{child_id}"),
                    format!("grok:{}", parent.parent_id),
                ))
            }
        })
        .collect()
}

pub fn canonical_project(
    cwd: &str,
    remotes: &[String],
    grok_home: &Path,
    child_id: &str,
    ctx: &GroupCtx,
) -> (String, String) {
    if let Some(parent) = root_parent(child_id, &ctx.parents) {
        return (
            parent.parent_cwd.clone(),
            project_name_of(&parent.parent_cwd),
        );
    }
    if cwd.is_empty() {
        return (String::new(), project_name_of(cwd));
    }

    let slug = grok_worktree_slug(cwd, grok_home);
    if let Some(pair) = lookup_source(cwd, slug.as_deref(), &ctx.worktrees) {
        return pair;
    }
    if let Some(slug) = slug {
        let path = grok_home
            .join("worktrees")
            .join(&slug)
            .to_string_lossy()
            .into_owned();
        let name = remotes
            .iter()
            .find_map(|remote| repo_name_from_remote(remote))
            .unwrap_or_else(|| slug.strip_prefix("github-").unwrap_or(&slug).to_string());
        return (path, name);
    }

    if is_ephemeral_worktree(cwd) {
        if let Some(remote) = remotes.iter().find(|remote| !remote.is_empty()) {
            if let Some(local) = local_checkout_from_remote(remote) {
                return (local.clone(), project_name_of(&local));
            }
            if let Some(name) = repo_name_from_remote(remote) {
                return (format!("git-remote:{name}"), name);
            }
        }
        if let Some(path) = ephemeral_plan_path(cwd) {
            return (path.clone(), project_name_of(&path));
        }
    }

    (cwd.to_string(), project_name_of(cwd))
}

fn normalize_path(path: &str) -> String {
    let path = path.trim_end_matches('/');
    if let Some(rest) = path.strip_prefix("/private/") {
        if rest.starts_with("var/") {
            return format!("/{rest}");
        }
    }
    path.to_string()
}

fn load_worktree_rows(grok_home: &Path) -> Vec<WorktreeRow> {
    let Some(ro) = open_sqlite_ro(&grok_home.join("worktrees.db"), "grok-worktrees") else {
        return Vec::new();
    };
    let mut stmt = match ro.conn.prepare(
        "SELECT path, source_repo, repo_name FROM worktrees \
         WHERE path != '' AND source_repo != ''",
    ) {
        Ok(stmt) => stmt,
        Err(_) => return Vec::new(),
    };
    let Ok(rows) = stmt.query_map([], |row| {
        Ok(WorktreeRow {
            path: row.get(0)?,
            source_repo: row.get(1)?,
            repo_name: row.get::<_, String>(2).unwrap_or_default(),
        })
    }) else {
        return Vec::new();
    };
    rows.flatten().collect()
}

fn hex_digit(c: u8) -> Option<u8> {
    Some(match c {
        b'0'..=b'9' => c - b'0',
        b'a'..=b'f' => c - b'a' + 10,
        b'A'..=b'F' => c - b'A' + 10,
        _ => return None,
    })
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut ix = 0;
    while ix < bytes.len() {
        if bytes[ix] == b'%' && ix + 2 < bytes.len() {
            if let (Some(high), Some(low)) = (hex_digit(bytes[ix + 1]), hex_digit(bytes[ix + 2])) {
                out.push((high << 4) | low);
                ix += 3;
                continue;
            }
        }
        out.push(bytes[ix]);
        ix += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn group_cwd(cwd_dir: &Path) -> String {
    if let Ok(raw) = fs::read_to_string(cwd_dir.join(".cwd")) {
        let cwd = raw.trim();
        if !cwd.is_empty() {
            return cwd.to_string();
        }
    }
    percent_decode(&cwd_dir.file_name().unwrap_or_default().to_string_lossy())
}

fn load_subagent_parents(sessions_root: &Path) -> HashMap<String, ParentRef> {
    let mut parents = HashMap::new();
    let Ok(groups) = fs::read_dir(sessions_root) else {
        return parents;
    };
    for group in groups.flatten() {
        let group_path = group.path();
        if !group_path.is_dir() {
            continue;
        }
        let parent_cwd = group_cwd(&group_path);
        if parent_cwd.is_empty() {
            continue;
        }
        let Ok(sessions) = fs::read_dir(&group_path) else {
            continue;
        };
        for session in sessions.flatten() {
            let session_path = session.path();
            let subagents = session_path.join("subagents");
            if !subagents.is_dir() {
                continue;
            }
            let folder_id = session_path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            let Ok(children) = fs::read_dir(subagents) else {
                continue;
            };
            for child in children.flatten() {
                let Ok(raw) = fs::read_to_string(child.path().join("meta.json")) else {
                    continue;
                };
                let Ok(value) = serde_json::from_str::<Value>(&raw) else {
                    continue;
                };
                let parent_id = value
                    .get("parent_session_id")
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty())
                    .unwrap_or(&folder_id)
                    .to_string();
                let parent = ParentRef {
                    parent_id,
                    parent_cwd: parent_cwd.clone(),
                };
                for field in ["child_session_id", "subagent_id"] {
                    if let Some(id) = value
                        .get(field)
                        .and_then(Value::as_str)
                        .filter(|id| !id.is_empty())
                    {
                        parents
                            .entry(id.to_string())
                            .or_insert_with(|| parent.clone());
                    }
                }
            }
        }
    }
    parents
}

fn root_parent(child_id: &str, parents: &HashMap<String, ParentRef>) -> Option<ParentRef> {
    if child_id.is_empty() {
        return None;
    }
    let mut current = child_id.to_string();
    let mut seen = HashSet::new();
    let mut last = None;
    while seen.insert(current.clone()) {
        let Some(parent) = parents.get(&current) else {
            break;
        };
        last = Some(parent.clone());
        if parents.contains_key(&parent.parent_id) {
            current = parent.parent_id.clone();
        } else {
            break;
        }
    }
    last.filter(|parent| !parent.parent_id.is_empty() && !parent.parent_cwd.is_empty())
}

fn grok_worktree_slug(cwd: &str, grok_home: &Path) -> Option<String> {
    let relative = Path::new(cwd)
        .strip_prefix(grok_home.join("worktrees"))
        .ok()?;
    let slug = relative.components().next()?.as_os_str().to_str()?;
    (!slug.is_empty()).then(|| slug.to_string())
}

fn is_ephemeral_worktree(cwd: &str) -> bool {
    let name = Path::new(cwd)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    name.starts_with("wt-")
        && (cwd.contains("/T/grok-") || cwd.contains("/var/folders/") || cwd.contains("/tmp/"))
}

fn local_checkout_from_remote(remote: &str) -> Option<String> {
    if !remote.starts_with('/') {
        return None;
    }
    for suffix in [".origin.git", ".git"] {
        if let Some(stem) = remote.strip_suffix(suffix) {
            if !stem.is_empty() {
                return Some(stem.to_string());
            }
        }
    }
    Some(remote.trim_end_matches('/').to_string())
}

fn repo_name_from_remote(remote: &str) -> Option<String> {
    if let Some(local) = local_checkout_from_remote(remote) {
        return Path::new(&local)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned());
    }
    let remote = remote.trim().trim_end_matches('/').trim_end_matches(".git");
    let name = remote.rsplit(['/', ':']).next()?;
    (!name.is_empty() && !name.contains('@')).then(|| name.to_string())
}

fn lookup_source(cwd: &str, slug: Option<&str>, rows: &[WorktreeRow]) -> Option<(String, String)> {
    let cwd = normalize_path(cwd);
    for row in rows {
        let worktree = normalize_path(&row.path);
        if cwd == worktree || cwd.starts_with(&format!("{worktree}/")) {
            let name = if row.repo_name.is_empty() {
                project_name_of(&row.source_repo)
            } else {
                row.repo_name.clone()
            };
            return Some((row.source_repo.clone(), name));
        }
    }

    let slug = slug?;
    let needle = format!("/worktrees/{slug}");
    for row in rows {
        let worktree = normalize_path(&row.path);
        if worktree.contains(&format!("{needle}/"))
            || worktree.ends_with(&needle)
            || row.repo_name == slug
        {
            let name = if row.repo_name.is_empty() {
                project_name_of(&row.source_repo)
            } else {
                row.repo_name.clone()
            };
            return Some((row.source_repo.clone(), name));
        }
    }
    None
}

fn ephemeral_plan_path(cwd: &str) -> Option<String> {
    let path = Path::new(cwd);
    let name = path.file_name()?.to_str()?;
    let plan = name
        .rsplit_once("-pr-")
        .map(|(head, _)| head)
        .unwrap_or(name);
    Some(path.parent()?.join(plan).to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn grok_home() -> PathBuf {
        PathBuf::from("/Users/tester/.grok")
    }

    #[test]
    fn ordinary_cwd_is_unchanged() {
        let ctx = GroupCtx::default();
        assert_eq!(
            canonical_project("/Users/tester/Github/wakefx", &[], &grok_home(), "", &ctx,),
            ("/Users/tester/Github/wakefx".into(), "wakefx".into())
        );
    }

    #[test]
    fn ephemeral_worktree_follows_local_origin_remote() {
        let ctx = GroupCtx::default();
        assert_eq!(
            canonical_project(
                "/var/folders/xx/T/grok-501/wt-abcd1234-pr-9",
                &["/Users/tester/Github/wakefx.origin.git".into()],
                &grok_home(),
                "",
                &ctx,
            ),
            ("/Users/tester/Github/wakefx".into(), "wakefx".into())
        );
    }

    #[test]
    fn ephemeral_worktrees_for_one_plan_group_together() {
        let ctx = GroupCtx::default();
        let first = canonical_project(
            "/var/folders/xx/T/grok-501/wt-abcd1234-pr-9",
            &[],
            &grok_home(),
            "",
            &ctx,
        );
        let second = canonical_project(
            "/var/folders/xx/T/grok-501/wt-abcd1234-pr-15",
            &[],
            &grok_home(),
            "",
            &ctx,
        );
        assert_eq!(first, second);
        assert_eq!(first.0, "/var/folders/xx/T/grok-501/wt-abcd1234");
    }

    #[test]
    fn registry_maps_named_worktree_to_source_repo() {
        let ctx = GroupCtx {
            worktrees: vec![WorktreeRow {
                path: "/Users/tester/.grok/worktrees/works-app/branch".into(),
                source_repo: "/Users/tester/Works/app".into(),
                repo_name: "app".into(),
            }],
            parents: HashMap::new(),
        };
        assert_eq!(
            canonical_project(
                "/Users/tester/.grok/worktrees/works-app/branch/subagent",
                &[],
                &grok_home(),
                "",
                &ctx,
            ),
            ("/Users/tester/Works/app".into(), "app".into())
        );
    }

    #[test]
    fn nested_subagent_flattens_to_root_parent() {
        let mut parents = HashMap::new();
        parents.insert(
            "child-2".into(),
            ParentRef {
                parent_id: "child-1".into(),
                parent_cwd: "/var/folders/xx/wt".into(),
            },
        );
        parents.insert(
            "child-1".into(),
            ParentRef {
                parent_id: "orchestrator".into(),
                parent_cwd: "/Users/tester/Desktop/AI/Grok".into(),
            },
        );
        let ctx = GroupCtx {
            worktrees: Vec::new(),
            parents,
        };
        assert_eq!(
            canonical_project("/var/folders/xx/wt", &[], &grok_home(), "child-2", &ctx,),
            ("/Users/tester/Desktop/AI/Grok".into(), "Grok".into())
        );
        assert!(parent_links(&ctx).contains(&("grok:child-2".into(), "grok:child-1".into())));
        assert!(parent_links(&ctx).contains(&("grok:child-1".into(), "grok:orchestrator".into())));
    }

    #[test]
    fn loads_parent_relation_from_session_tree() {
        let temp = tempfile::tempdir().unwrap();
        let meta_dir = temp
            .path()
            .join("%2FUsers%2Ftester%2FDesktop%2FAI%2FGrok")
            .join("orchestrator")
            .join("subagents")
            .join("child");
        fs::create_dir_all(&meta_dir).unwrap();
        fs::write(
            meta_dir.join("meta.json"),
            r#"{"parent_session_id":"orchestrator","child_session_id":"child"}"#,
        )
        .unwrap();

        let ctx = load_group_ctx(temp.path(), temp.path());
        assert!(parent_links(&ctx).contains(&("grok:child".into(), "grok:orchestrator".into())));
        assert_eq!(percent_decode("%2FUsers%2Ftester"), "/Users/tester");
    }
}
