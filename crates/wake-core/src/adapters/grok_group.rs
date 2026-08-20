//! Grok 会话归组:主会话 subagents 反查优先,无父时 worktree/remote 启发式兜底。
use super::parse_utils::project_name_of;
use super::sqlite_ro::open_sqlite_ro;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;
use std::sync::Arc;

/// 一轮扫描共享的归组快照
#[derive(Clone, Debug, Default)]
pub struct GroupCtx {
    pub worktrees: Vec<WorktreeRow>,
    pub parents: HashMap<String, ParentRef>,
}

#[derive(Clone, Debug)]
pub struct WorktreeRow {
    pub path: String,
    pub source_repo: String,
    pub repo_name: String,
}

/// 主会话登记的子代理:child_id → 父会话 id 与父 cwd
#[derive(Clone, Debug)]
pub struct ParentRef {
    pub parent_id: String,
    pub parent_cwd: String,
}

pub fn load_group_ctx(sessions_root: &Path, grok_home: &Path) -> Arc<GroupCtx> {
    Arc::new(GroupCtx {
        worktrees: load_worktree_rows(grok_home),
        parents: load_subagent_parents(sessions_root),
    })
}

/// (child_key, parent_key) 全量父子链,给 store 旁写
pub fn parent_links(ctx: &GroupCtx) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for child_id in ctx.parents.keys() {
        if !seen.insert(child_id.clone()) {
            continue;
        }
        if let Some(p) = root_parent(child_id, &ctx.parents) {
            out.push((format!("grok:{child_id}"), format!("grok:{}", p.parent_id)));
        }
    }
    out
}

pub fn canonical_project(
    cwd: &str,
    remotes: &[String],
    grok_home: &Path,
    child_id: &str,
    ctx: &GroupCtx,
) -> (String, String) {
    if let Some(pair) = parent_project(child_id, &ctx.parents) {
        return pair;
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
            .find_map(|r| repo_name_from_remote(r))
            .unwrap_or_else(|| slug.strip_prefix("github-").unwrap_or(&slug).to_string());
        return (path, name);
    }
    if is_ephemeral_worktree(cwd) {
        if let Some(remote) = remotes.iter().find(|r| !r.is_empty()) {
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

fn normalize_path(s: &str) -> String {
    let s = s.trim_end_matches('/');
    if let Some(rest) = s.strip_prefix("/private/") {
        if rest.starts_with("var/") {
            return format!("/{rest}");
        }
    }
    s.to_string()
}

fn load_worktree_rows(grok_home: &Path) -> Vec<WorktreeRow> {
    let db = grok_home.join("worktrees.db");
    let Some(ro) = open_sqlite_ro(&db, "grok-worktrees") else {
        return Vec::new();
    };
    let mut stmt = match ro.conn.prepare(
        "SELECT path, source_repo, repo_name FROM worktrees WHERE path != '' AND source_repo != ''",
    ) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let rows = stmt.query_map([], |r| {
        Ok(WorktreeRow {
            path: r.get(0)?,
            source_repo: r.get(1)?,
            repo_name: r.get::<_, String>(2).unwrap_or_default(),
        })
    });
    let Ok(rows) = rows else {
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
    let b = input.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(h), Some(l)) = (hex_digit(b[i + 1]), hex_digit(b[i + 2])) {
                out.push((h << 4) | l);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn group_cwd(cwd_dir: &Path) -> String {
    let marker = cwd_dir.join(".cwd");
    if let Ok(s) = fs::read_to_string(marker) {
        let t = s.trim();
        if !t.is_empty() {
            return t.to_string();
        }
    }
    percent_decode(&cwd_dir.file_name().unwrap_or_default().to_string_lossy())
}

fn load_subagent_parents(sessions_root: &Path) -> HashMap<String, ParentRef> {
    let mut map = HashMap::new();
    let Ok(groups) = fs::read_dir(sessions_root) else {
        return map;
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
        for sess in sessions.flatten() {
            let sess_path = sess.path();
            let sub = sess_path.join("subagents");
            if !sub.is_dir() {
                continue;
            }
            let folder_id = sess_path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            let Ok(kids) = fs::read_dir(&sub) else {
                continue;
            };
            for kid in kids.flatten() {
                let meta_path = kid.path().join("meta.json");
                let Ok(raw) = fs::read_to_string(meta_path) else {
                    continue;
                };
                let Ok(v) = serde_json::from_str::<Value>(&raw) else {
                    continue;
                };
                let parent_id = v
                    .get("parent_session_id")
                    .and_then(|x| x.as_str())
                    .filter(|s| !s.is_empty())
                    .unwrap_or(&folder_id)
                    .to_string();
                let info = ParentRef {
                    parent_id,
                    parent_cwd: parent_cwd.clone(),
                };
                for key in ["child_session_id", "subagent_id"] {
                    if let Some(id) = v
                        .get(key)
                        .and_then(|x| x.as_str())
                        .filter(|s| !s.is_empty())
                    {
                        map.entry(id.to_string()).or_insert_with(|| info.clone());
                    }
                }
            }
        }
    }
    map
}

fn root_parent(child_id: &str, parents: &HashMap<String, ParentRef>) -> Option<ParentRef> {
    if child_id.is_empty() {
        return None;
    }
    let mut id = child_id.to_string();
    let mut seen = HashSet::new();
    let mut last = None;
    while seen.insert(id.clone()) {
        let Some(p) = parents.get(&id) else {
            break;
        };
        last = Some(p.clone());
        if parents.contains_key(&p.parent_id) {
            id = p.parent_id.clone();
        } else {
            break;
        }
    }
    last.filter(|p| !p.parent_cwd.is_empty() && !p.parent_id.is_empty())
}

fn parent_project(
    child_id: &str,
    parents: &HashMap<String, ParentRef>,
) -> Option<(String, String)> {
    root_parent(child_id, parents).map(|p| (p.parent_cwd.clone(), project_name_of(&p.parent_cwd)))
}

fn grok_worktree_slug(cwd: &str, grok_home: &Path) -> Option<String> {
    let rel = Path::new(cwd)
        .strip_prefix(grok_home.join("worktrees"))
        .ok()?;
    let slug = rel.components().next()?.as_os_str().to_str()?;
    if slug.is_empty() {
        None
    } else {
        Some(slug.to_string())
    }
}

fn is_ephemeral_worktree(cwd: &str) -> bool {
    let name = Path::new(cwd)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    if !name.starts_with("wt-") {
        return false;
    }
    cwd.contains("/T/grok-") || cwd.contains("/var/folders/") || cwd.contains("/tmp/")
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
            .map(|s| s.to_string_lossy().into_owned());
    }
    let s = remote.trim().trim_end_matches('/').trim_end_matches(".git");
    let name = s.rsplit(['/', ':']).next()?;
    if name.is_empty() || name.contains('@') {
        None
    } else {
        Some(name.to_string())
    }
}

fn lookup_source(cwd: &str, slug: Option<&str>, rows: &[WorktreeRow]) -> Option<(String, String)> {
    let cwd_n = normalize_path(cwd);
    for r in rows {
        let p = normalize_path(&r.path);
        if cwd_n == p || cwd_n.starts_with(&format!("{p}/")) {
            let name = if r.repo_name.is_empty() {
                project_name_of(&r.source_repo)
            } else {
                r.repo_name.clone()
            };
            return Some((r.source_repo.clone(), name));
        }
    }
    let slug = slug?;
    let needle = format!("/worktrees/{slug}");
    for r in rows {
        let p = normalize_path(&r.path);
        if p.contains(&format!("{needle}/")) || p.ends_with(&needle) || r.repo_name == slug {
            let name = if r.repo_name.is_empty() {
                project_name_of(&r.source_repo)
            } else {
                r.repo_name.clone()
            };
            return Some((r.source_repo.clone(), name));
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
    let parent = path.parent()?;
    Some(parent.join(plan).to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn grok_home() -> PathBuf {
        PathBuf::from("/Users/tester/.grok")
    }

    fn ctx_with(parents: HashMap<String, ParentRef>, worktrees: Vec<WorktreeRow>) -> GroupCtx {
        GroupCtx { worktrees, parents }
    }

    fn resolve(cwd: &str, remotes: &[&str], worktrees: Vec<WorktreeRow>) -> (String, String) {
        let remotes: Vec<String> = remotes.iter().map(|s| (*s).to_string()).collect();
        canonical_project(
            cwd,
            &remotes,
            &grok_home(),
            "",
            &ctx_with(HashMap::new(), worktrees),
        )
    }

    #[test]
    fn ordinary_cwd_unchanged() {
        let (path, name) = resolve("/Users/tester/Github/wakefx", &[], Vec::new());
        assert_eq!(path, "/Users/tester/Github/wakefx");
        assert_eq!(name, "wakefx");
    }

    #[test]
    fn ephemeral_worktree_follows_local_origin_remote() {
        let (path, name) = resolve(
            "/var/folders/xx/T/grok-501/wt-abcd1234-pr-9",
            &["/Users/tester/Github/wakefx.origin.git"],
            Vec::new(),
        );
        assert_eq!(path, "/Users/tester/Github/wakefx");
        assert_eq!(name, "wakefx");
    }

    #[test]
    fn ephemeral_worktree_without_remote_groups_by_plan() {
        let (pa, na) = resolve(
            "/var/folders/xx/T/grok-501/wt-abcd1234-pr-9",
            &[],
            Vec::new(),
        );
        let (pb, nb) = resolve(
            "/var/folders/xx/T/grok-501/wt-abcd1234-pr-15",
            &[],
            Vec::new(),
        );
        assert_eq!(pa, pb);
        assert_eq!(pa, "/var/folders/xx/T/grok-501/wt-abcd1234");
        assert_eq!(na, "wt-abcd1234");
        assert_eq!(nb, "wt-abcd1234");
    }

    #[test]
    fn named_worktree_uses_registry_source_repo() {
        let rows = vec![WorktreeRow {
            path: "/Users/tester/.grok/worktrees/works-app-av4/2026-08-15-label".into(),
            source_repo: "/Users/tester/Works/app_av4".into(),
            repo_name: "app_av4".into(),
        }];
        let (path, name) = resolve(
            "/Users/tester/.grok/worktrees/works-app-av4/subagent-abc",
            &[],
            rows,
        );
        assert_eq!(path, "/Users/tester/Works/app_av4");
        assert_eq!(name, "app_av4");
    }

    #[test]
    fn named_worktree_without_registry_groups_by_slug() {
        let (path, name) = resolve(
            "/Users/tester/.grok/worktrees/github-react-native-syan-image-picker/subagent-1",
            &["https://github.com/syanbo/react-native-syan-image-picker.git"],
            Vec::new(),
        );
        assert_eq!(
            path,
            "/Users/tester/.grok/worktrees/github-react-native-syan-image-picker"
        );
        assert_eq!(name, "react-native-syan-image-picker");
    }

    #[test]
    fn subagent_uses_orchestrator_cwd_over_worktree_and_remote() {
        let mut parents = HashMap::new();
        parents.insert(
            "child-1".into(),
            ParentRef {
                parent_id: "orch".into(),
                parent_cwd: "/Users/tester/Desktop/AI/Grok".into(),
            },
        );
        let remotes = ["/Users/tester/Github/wakefx.origin.git".to_string()];
        let (path, name) = canonical_project(
            "/var/folders/xx/T/grok-501/wt-abcd1234-pr-9",
            &remotes,
            &grok_home(),
            "child-1",
            &ctx_with(parents, Vec::new()),
        );
        assert_eq!(path, "/Users/tester/Desktop/AI/Grok");
        assert_eq!(name, "Grok");
    }

    #[test]
    fn nested_subagent_walks_to_root_parent() {
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
                parent_id: "orch".into(),
                parent_cwd: "/Users/tester/Desktop/AI/Grok".into(),
            },
        );
        let (path, name) = canonical_project(
            "/var/folders/xx/wt",
            &[],
            &grok_home(),
            "child-2",
            &ctx_with(parents, Vec::new()),
        );
        assert_eq!(path, "/Users/tester/Desktop/AI/Grok");
        assert_eq!(name, "Grok");
    }

    #[test]
    fn percent_decode_cwd_group() {
        assert_eq!(
            percent_decode("%2FUsers%2Ftester%2FDesktop%2FAI%2FGrok"),
            "/Users/tester/Desktop/AI/Grok"
        );
    }

    #[test]
    fn load_subagent_parents_from_session_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let meta_dir = tmp
            .path()
            .join("%2FUsers%2Ftester%2FDesktop%2FAI%2FGrok")
            .join("orch-id")
            .join("subagents")
            .join("child-id");
        fs::create_dir_all(&meta_dir).unwrap();
        fs::write(
            meta_dir.join("meta.json"),
            r#"{"parent_session_id":"orch-id","child_session_id":"child-id","subagent_id":"child-id"}"#,
        )
        .unwrap();
        let map = load_subagent_parents(tmp.path());
        let p = map.get("child-id").expect("child mapped");
        assert_eq!(p.parent_id, "orch-id");
        assert_eq!(p.parent_cwd, "/Users/tester/Desktop/AI/Grok");
        let ctx = GroupCtx {
            worktrees: Vec::new(),
            parents: map,
        };
        let links = parent_links(&ctx);
        assert!(links.contains(&("grok:child-id".into(), "grok:orch-id".into())));
    }
}
