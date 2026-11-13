//! CodeBuddy 5.x memory files. Never inspect a workspace's legacy .codebuddy/.
use super::{codebuddy_legacy, frontmatter_field, parse_utils::*};
use crate::models::*;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

pub(crate) const REMOTE_MIRROR: &str = ".codebuddy-memory";
pub(crate) const MANIFEST: &str = "manifest.json";
pub(crate) const IDE_LINUX: &str = ".local/share/CodeBuddyExtension/Data/Public/.memories";
pub(crate) const IDE_MAC: &str =
    "Library/Application Support/CodeBuddyExtension/Data/Public/.memories";
const MAX_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct MemorySettings {
    pub cli: bool,
    pub project: bool,
    pub global: bool,
}

#[derive(Clone)]
pub(crate) struct Reader {
    pub projects: PathBuf,
    pub home: Option<PathBuf>,
    pub ide: Vec<PathBuf>,
    pub transcripts: PathBuf,
    pub catalog: Option<PathBuf>,
    pub host: Option<String>,
    pub mirror: Option<PathBuf>,
}

impl Reader {
    pub fn sources(&self) -> Vec<MemorySource> {
        let mut out = vec![MemorySource {
            agent: AgentId::Codebuddy,
            kind: MemorySourceKind::ProjectTree,
            path: self.projects.clone(),
        }];
        if let Some(home) = &self.home {
            out.push(MemorySource {
                agent: AgentId::Codebuddy,
                kind: MemorySourceKind::ProjectTree,
                path: home.join("memories"),
            });
            out.push(MemorySource {
                agent: AgentId::Codebuddy,
                kind: MemorySourceKind::Dir { ext: "md" },
                path: home.join("memories/global"),
            });
        }
        out.extend(self.ide.iter().map(|path| MemorySource {
            agent: AgentId::Codebuddy,
            kind: MemorySourceKind::Dir { ext: "mdc" },
            path: path.clone(),
        }));
        out
    }

    pub fn is_event(&self, path: &Path) -> bool {
        // Includes directory removals and first creation of a centralized root.
        if self.projects.starts_with(path) {
            return true;
        }
        if self
            .mirror
            .as_ref()
            .is_some_and(|m| path.starts_with(m) || m.starts_with(path))
        {
            return true;
        }
        if self
            .ide
            .iter()
            .any(|p| path.starts_with(p) || p.starts_with(path))
        {
            return true;
        }
        if self.home.as_ref().is_some_and(|h| {
            path.starts_with(h.join("memories"))
                || path == h.join("settings.json")
                || path == h.join("ide-memory-settings.json")
        }) {
            return true;
        }
        if let Ok(rel) = path.strip_prefix(&self.projects) {
            let parts: Vec<_> = rel.components().collect();
            return parts.len() <= 1 || parts.get(1).is_some_and(|p| p.as_os_str() == "memory");
        }
        self.catalog.as_ref().is_some_and(|p| {
            path == p || path.as_os_str() == format!("{}-wal", p.display()).as_str()
        })
    }

    pub fn watch_paths(&self) -> Vec<PathBuf> {
        if let Some(mirror) = &self.mirror {
            // The parent observes generation swaps, including the first sync.
            return mirror.parent().map(Path::to_path_buf).into_iter().collect();
        }
        let mut paths = Vec::new();
        for source in self.sources() {
            let mut p = source.path;
            while !p.is_dir() {
                let Some(parent) = p.parent() else {
                    break;
                };
                p = parent.to_path_buf();
            }
            if p.is_dir() {
                paths.push(p);
            }
        }
        paths.sort();
        paths.dedup();
        paths
    }

    pub fn watch_recursive(&self, path: &Path) -> bool {
        self.mirror.as_ref().is_some_and(|p| p == path)
            || self.sources().iter().any(|s| s.path == path)
    }

    pub fn list(&self, sources: &[MemorySource], projects: &[PathBuf]) -> Result<Vec<MemoryDoc>> {
        let _guard = self
            .host
            .as_ref()
            .map(|_| codebuddy_legacy::REMOTE_CACHE_SWITCH.read().unwrap());
        let defaults = self.sources();
        let (own, custom): (Vec<_>, Vec<_>) =
            sources.iter().cloned().partition(|s| defaults.contains(s));
        let mut docs = super::generic_memory_docs(
            &super::SHARED_MEMORY_CACHE,
            AgentId::Codebuddy,
            &custom,
            projects,
        )?;
        for source in own {
            docs.extend(self.read_source(&source)?);
        }
        Ok(docs)
    }

    fn settings(&self, origin: MemoryOrigin, ide: Option<&Path>) -> Result<MemorySettings> {
        if let Some(mirror) = &self.mirror {
            let path = mirror.join(MANIFEST);
            if !exists(&path)? {
                return Ok(MemorySettings {
                    cli: true,
                    project: true,
                    global: true,
                });
            }
            let value: serde_json::Value = serde_json::from_str(&safe_text(mirror, &path)?)?;
            return Ok(serde_json::from_value(value["settings"].clone())?);
        }
        let mut status = MemorySettings {
            cli: true,
            project: true,
            global: true,
        };
        if origin != MemoryOrigin::Ide {
            if let Some(home) = &self.home {
                let cli = read_json(home, &home.join("settings.json"))?;
                status.cli = bool_field(&cli["memory"], "autoMemoryEnabled", true)?;
                status.project = if origin == MemoryOrigin::Shared {
                    bool_field(
                        &read_json(home, &home.join("ide-memory-settings.json"))?,
                        "projectEnabled",
                        status.cli,
                    )?
                } else {
                    true
                };
            }
        }
        if let Some(dir) = ide {
            status.global = bool_field(
                &read_json(dir, &dir.join(".memory-global-config.json"))?,
                "enabled",
                true,
            )?;
        }
        Ok(status)
    }

    fn read_source(&self, source: &MemorySource) -> Result<Vec<MemoryDoc>> {
        let origin = if self.ide.contains(&source.path) {
            MemoryOrigin::Ide
        } else if source.kind == MemorySourceKind::ProjectTree {
            MemoryOrigin::Shared
        } else {
            MemoryOrigin::Cli
        };
        let status = self.settings(
            origin,
            (origin == MemoryOrigin::Ide).then_some(source.path.as_path()),
        )?;
        let mut files: Vec<(PathBuf, String)> = Vec::new();
        if origin == MemoryOrigin::Shared {
            let attribution = self.attribution()?;
            let fallback = source.path != self.projects;
            for entry in directories(&source.path)? {
                let id = entry.file_name().unwrap().to_string_lossy().to_string();
                if id.starts_with('.') || (fallback && id == "global") {
                    continue;
                }
                let primary = self.projects.join(&id).join("memory");
                if fallback && exists(&primary)? {
                    // A replacement that cannot be read is not proof that the
                    // previously indexed fallback can be discarded.
                    for path in memory_files(&primary, true, false)? {
                        safe_text(&primary, &path)?;
                    }
                    continue;
                }
                let root = if fallback { entry } else { primary };
                let project = attribution
                    .get(&id)
                    .filter(|v| v.len() == 1)
                    .and_then(|v| v.first())
                    .cloned()
                    .unwrap_or_default();
                for file in memory_files(&root, true, false)? {
                    files.push((file, project.clone()));
                }
            }
        } else {
            files.extend(
                memory_files(
                    &source.path,
                    origin == MemoryOrigin::Cli,
                    origin == MemoryOrigin::Ide,
                )?
                .into_iter()
                .map(|p| (p, String::new())),
            );
        }
        files.sort();
        let mut stamp = std::collections::hash_map::DefaultHasher::new();
        serde_json::to_string(&status)?.hash(&mut stamp);
        for (path, project) in &files {
            path.hash(&mut stamp);
            project.hash(&mut stamp);
            let m = fs::symlink_metadata(path)?;
            codebuddy_legacy::hash_file_identity(&m, &mut stamp);
            m.len().hash(&mut stamp);
            m.modified()?.hash(&mut stamp);
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                m.ctime().hash(&mut stamp);
                m.ctime_nsec().hash(&mut stamp);
            }
            #[cfg(not(unix))]
            {
                safe_text(&source.path, path)?.hash(&mut stamp);
            }
        }
        let key = format!(
            "codebuddy-memory:{}:{}",
            self.host.as_deref().unwrap_or_default(),
            source.id()
        );
        super::SHARED_MEMORY_CACHE.get_or_build(&key, stamp.finish() as i64, || {
            files
                .iter()
                .map(|(path, project)| {
                    let raw = safe_text(&source.path, path)?;
                    let meta = fs::metadata(path)?;
                    let title = ["title", "name", "description"]
                        .into_iter()
                        .find_map(|k| frontmatter_field(&raw, k).filter(|v| !v.trim().is_empty()))
                        .unwrap_or_else(|| path.file_name().unwrap().to_string_lossy().to_string());
                    let updated = frontmatter_field(&raw, "updatedAt")
                        .and_then(|s| chrono::DateTime::parse_from_rfc3339(&s).ok())
                        .map(|d| d.timestamp_millis())
                        .unwrap_or_else(|| mtime_ms(&meta));
                    let enabled = match super::frontmatter_ident(&raw, "enabled")
                        .map(|s| s.to_ascii_lowercase())
                    {
                        None => true,
                        Some(s) if s == "true" => true,
                        Some(s) if s == "false" => false,
                        _ => bail!("invalid memory enabled state: {}", path.display()),
                    };
                    let path_str = path.to_string_lossy().to_string();
                    let revision = {
                        use sha2::{Digest, Sha256};
                        format!("{:x}", Sha256::digest(raw.as_bytes()))
                    };
                    Ok(MemoryDoc {
                        key: session_key(AgentId::Codebuddy, "", &path_str),
                        agent: AgentId::Codebuddy,
                        host: String::new(),
                        scope: if origin == MemoryOrigin::Shared {
                            MemoryScope::Project
                        } else {
                            MemoryScope::User
                        },
                        project_path: project.clone(),
                        project_name: if project.is_empty() {
                            String::new()
                        } else {
                            project_name_of(project)
                        },
                        session_key: String::new(),
                        path: path_str,
                        title: crate::text::one_line(&title, super::MEMORY_TITLE_MAX),
                        updated_at: updated,
                        size_bytes: meta.len() as i64,
                        source: source.id(),
                        body: raw,
                        details: MemoryDetails {
                            revision,
                            origin: Some(origin),
                            is_index: path.file_name().is_some_and(|p| p == "MEMORY.md"),
                            enabled: Some(enabled),
                            ide_enabled: match origin {
                                MemoryOrigin::Ide => Some(status.global),
                                MemoryOrigin::Shared => self.home.as_ref().map(|_| status.project),
                                _ => None,
                            },
                            cli_enabled: (origin != MemoryOrigin::Ide && self.home.is_some())
                                .then_some(status.cli),
                        },
                    })
                })
                .collect()
        })
    }

    fn attribution(&self) -> Result<BTreeMap<String, BTreeSet<String>>> {
        let mut known: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        if let Some(mirror) = &self.mirror {
            let path = mirror.join(MANIFEST);
            if exists(&path)? {
                let v: serde_json::Value = serde_json::from_str(&safe_text(mirror, &path)?)?;
                known = serde_json::from_value(v["projects"].clone())?;
            }
        }
        if let Some(catalog) = &self.catalog {
            for row in codebuddy_legacy::read_catalog(catalog)? {
                if !codebuddy_legacy::authority_matches(&row.authority, self.host.as_deref())
                    || row.workspace.is_empty()
                {
                    continue;
                }
                // 5.x uses realpath, unlike the 4.x history hash. Never resolve remote paths locally.
                let workspace = workspace_path(&row.workspace, self.host.is_some());
                known
                    .entry(project_id(&workspace))
                    .or_default()
                    .insert(workspace);
            }
        }
        // Only inspect directories that actually own memory. No need to scan every transcript.
        let mut ids = BTreeSet::new();
        for dir in directories(&self.projects)? {
            if exists(&dir.join("memory"))? {
                ids.insert(dir.file_name().unwrap().to_string_lossy().to_string());
            }
        }
        if let Some(home) = &self.home {
            for dir in directories(&home.join("memories"))? {
                if dir.file_name().is_some_and(|n| n != "global") {
                    ids.insert(dir.file_name().unwrap().to_string_lossy().to_string());
                }
            }
        }
        for id in ids {
            let dir = self.transcripts.join(&id);
            if !exists(&dir)? {
                continue;
            }
            for e in fs::read_dir(&dir)? {
                let path = e?.path();
                if path.extension().is_none_or(|e| e != "jsonl") {
                    continue;
                }
                if let Some(cwd) = transcript_cwd(&path)? {
                    // Confirm the path by the recorded directory ID, never reverse a lossy slug.
                    let canonical = workspace_path(&cwd, self.host.is_some());
                    if project_id(&canonical) == id {
                        known.entry(id.clone()).or_default().insert(canonical);
                    } else if project_id(&cwd) == id {
                        known.entry(id.clone()).or_default().insert(cwd);
                    }
                }
            }
        }
        Ok(known)
    }
}

fn workspace_path(path: &str, remote: bool) -> String {
    if remote {
        return path.to_owned();
    }
    let canonical = fs::canonicalize(path)
        .ok()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_owned());
    if let Some(rest) = canonical.strip_prefix(r"\\?\UNC\") {
        return format!(r"\\{rest}");
    }
    canonical
        .strip_prefix(r"\\?\")
        .unwrap_or(&canonical)
        .to_owned()
}

pub(crate) fn project_id(path: &str) -> String {
    let mut base = String::new();
    for ch in path.chars() {
        let ch = if matches!(ch, '/' | '\\' | ':') {
            '-'
        } else {
            ch
        };
        if ch == '-' && (base.is_empty() || base.ends_with('-')) {
            continue;
        }
        base.push(ch);
    }
    let base = base.trim_end_matches('-');
    if base.len() <= 255 {
        return base.to_owned();
    }
    let hash = base
        .bytes()
        .fold(5381u32, |h, b| h.wrapping_mul(33) ^ u32::from(b));
    let mut n = hash;
    let mut digits = Vec::new();
    loop {
        digits.push(char::from_digit(n % 36, 36).unwrap());
        n /= 36;
        if n == 0 {
            break;
        }
    }
    let end = base
        .char_indices()
        .map(|(i, _)| i)
        .take_while(|i| *i <= 180)
        .last()
        .unwrap_or(0);
    format!(
        "{}-{}",
        &base[..end],
        digits.iter().rev().collect::<String>()
    )
}

fn transcript_cwd(path: &Path) -> Result<Option<String>> {
    if fs::symlink_metadata(path)?.file_type().is_symlink() {
        bail!("symlink transcript: {}", path.display());
    }
    let reader = BufReader::new(fs::File::open(path)?.take(1024 * 1024));
    for line in reader.lines().take(128) {
        let line = line?;
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if let Some(cwd) = v
            .get("cwd")
            .and_then(|v| v.as_str())
            .filter(|v| !v.is_empty())
        {
            return Ok(Some(cwd.to_owned()));
        }
    }
    Ok(None)
}

pub(crate) fn exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() => {
            bail!("memory symlink is unsupported: {}", path.display())
        }
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // An absent child under a dangling link is not proof of deletion.
            if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
                exists(parent)?;
            }
            Ok(false)
        }
        Err(e) => Err(e).with_context(|| path.display().to_string()),
    }
}

fn directories(root: &Path) -> Result<Vec<PathBuf>> {
    if !exists(root)? {
        return Ok(Vec::new());
    }
    let mut dirs = Vec::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            bail!("memory directory symlink: {}", entry.path().display());
        }
        if kind.is_dir() {
            dirs.push(entry.path());
        }
    }
    dirs.sort();
    Ok(dirs)
}

fn memory_files(root: &Path, recursive: bool, mdc: bool) -> Result<Vec<PathBuf>> {
    if !exists(root)? {
        return Ok(Vec::new());
    }
    if !fs::metadata(root)?.is_dir() {
        bail!("memory source is not a directory: {}", root.display());
    }
    let mut files = Vec::new();
    for entry in walkdir::WalkDir::new(root)
        .max_depth(if recursive { usize::MAX } else { 1 })
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| e.depth() == 0 || !e.file_name().to_string_lossy().starts_with('.'))
    {
        let entry = entry?;
        if entry.file_type().is_symlink() {
            bail!("memory symlink: {}", entry.path().display());
        }
        if entry.file_type().is_file()
            && entry
                .path()
                .extension()
                .is_some_and(|e| e == "md" || (mdc && e == "mdc"))
        {
            files.push(entry.into_path());
        }
    }
    // IDE resolves a same-ID .md before .mdc; do not index the inactive shadow.
    if mdc {
        files.retain(|p| {
            p.extension().is_none_or(|e| e != "mdc") || !p.with_extension("md").exists()
        });
    }
    files.sort();
    Ok(files)
}

pub(crate) fn safe_text(root: &Path, path: &Path) -> Result<String> {
    let relative = path.strip_prefix(root)?;
    let mut p = root.to_path_buf();
    exists(&p)?;
    for component in relative.components() {
        p.push(component);
        exists(&p)?;
    }
    let file = fs::File::open(path)?;
    if !file.metadata()?.is_file() || file.metadata()?.len() > MAX_BYTES {
        bail!("invalid memory file: {}", path.display());
    }
    let mut bytes = Vec::new();
    file.take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_BYTES {
        bail!("memory file too large");
    }
    Ok(String::from_utf8(bytes)?)
}
fn read_json(root: &Path, path: &Path) -> Result<serde_json::Value> {
    if !exists(path)? {
        return Ok(serde_json::json!({}));
    }
    Ok(serde_json::from_str(&safe_text(root, path)?)?)
}
fn bool_field(v: &serde_json::Value, key: &str, default: bool) -> Result<bool> {
    match v.get(key) {
        None => Ok(default),
        Some(v) => v
            .as_bool()
            .with_context(|| format!("invalid memory setting: {key}")),
    }
}

pub(crate) fn body(doc: &MemoryDoc) -> String {
    // Keep frontmatter visible, just like other Wake memory documents. Search
    // can match a description there even when the entry uses a separate title.
    safe_text(Path::new(&doc.source), Path::new(&doc.path)).unwrap_or_else(|_| doc.body.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::AgentAdapter;
    use crate::db::Store;
    use crate::scanner::{run_memory_sync, NullEvents, ScanProgress};
    use std::sync::Arc;

    fn put(p: impl AsRef<Path>, content: &str) {
        let p = p.as_ref();
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, content).unwrap();
    }
    fn fixture(root: &Path) -> Reader {
        Reader {
            projects: root.join(".codebuddy/projects"),
            home: Some(root.join(".codebuddy")),
            ide: vec![root.join(IDE_LINUX)],
            transcripts: root.join(".codebuddy/projects"),
            catalog: None,
            host: None,
            mirror: None,
        }
    }
    struct Adapter(Reader);
    impl AgentAdapter for Adapter {
        fn agent(&self) -> AgentId {
            AgentId::Codebuddy
        }
        fn list_session_files(&self) -> Result<Vec<SessionFileRef>> {
            Ok(Vec::new())
        }
        fn parse_session(&self, _: &SessionFileRef) -> Result<ParsedSession> {
            bail!("not a session")
        }
        fn parse_transcript(&self, _: &SessionFileRef) -> Result<ParsedTranscript> {
            bail!("not a session")
        }
        fn with_custom_root(&self, dir: PathBuf) -> Box<dyn AgentAdapter> {
            Box::new(Adapter(fixture(&dir)))
        }
        fn data_roots(&self) -> Vec<PathBuf> {
            vec![self.0.projects.clone()]
        }
        fn memory_sources(&self) -> Vec<MemorySource> {
            self.0.sources()
        }
        fn list_memories(&self, s: &[MemorySource], p: &[PathBuf]) -> Result<Vec<MemoryDoc>> {
            self.0.list(s, p)
        }
        fn memory_watch_paths(&self) -> Vec<PathBuf> {
            self.0.watch_paths()
        }
        fn is_memory_event(&self, p: &Path) -> bool {
            self.0.is_event(p)
        }
    }

    #[test]
    fn centralized_sources_scopes_titles_statuses_and_precedence() {
        let tmp = tempfile::tempdir().unwrap();
        let r = fixture(tmp.path());
        let id = project_id("/project/with-hyphens");
        let mem = r.projects.join(&id).join("memory");
        put(mem.join("MEMORY.md"), "# Memory\n[topic](nested/topic.md)");
        put(mem.join("nested/topic.md"),"---\nname: A named memory\ndescription: Longer description\ntype: user\nenabled: false\nupdatedAt: 2026-11-13T00:00:00Z\n---\nActual project content");
        put(mem.join(".ide-memory-backups/old.md"), "backup");
        put(mem.join("topic.md.tmp"), "temporary");
        put(
            r.projects.join(&id).join("a.jsonl"),
            "{\"cwd\":\"/project/with-hyphens\"}\n",
        );
        let home = r.home.as_ref().unwrap();
        put(
            home.join("memories").join(&id).join("old.md"),
            "shadowed centralized fallback",
        );
        put(
            home.join("memories/another/topic.md"),
            "---\ndescription: Fallback title\n---\nfallback",
        );
        put(
            home.join("memories/global/topic.md"),
            "---\nname: CLI preference\n---\ncli global",
        );
        put(
            home.join("settings.json"),
            "{\"memory\":{\"autoMemoryEnabled\":false}}",
        );
        put(
            home.join("ide-memory-settings.json"),
            "{\"projectEnabled\":true}",
        );
        put(
            r.ide[0].join("uuid.mdc"),
            "---\ntitle: IDE preference\n---\nide global",
        );
        put(r.ide[0].join("duplicate.mdc"), "inactive shadow");
        put(
            r.ide[0].join("duplicate.md"),
            "---\ntitle: Active duplicate\n---\ncurrent",
        );
        put(
            r.ide[0].join(".memory-global-config.json"),
            "{\"enabled\":false}",
        );
        let docs = r.list(&r.sources(), &[]).unwrap();
        assert_eq!(docs.len(), 6);
        let topic = docs.iter().find(|d| d.title == "A named memory").unwrap();
        assert_eq!(topic.scope, MemoryScope::Project);
        assert_eq!(topic.project_path, "/project/with-hyphens");
        assert_eq!(topic.details.enabled, Some(false));
        assert_eq!(topic.details.ide_enabled, Some(true));
        assert_eq!(topic.details.cli_enabled, Some(false));
        assert_eq!(topic.updated_at, 1794528000000);
        assert!(body(topic).ends_with("Actual project content"));
        assert!(body(topic).contains("description: Longer description"));
        assert_eq!(docs.iter().filter(|d| d.details.is_index).count(), 1);
        let ide = docs.iter().find(|d| d.title == "IDE preference").unwrap();
        assert_eq!(ide.scope, MemoryScope::User);
        assert_eq!(ide.details.ide_enabled, Some(false));
        assert!(docs
            .iter()
            .any(|d| d.title == "Fallback title" && d.project_path.is_empty()));
        // Even an empty new directory takes precedence over the fallback.
        fs::create_dir_all(r.projects.join("another/memory")).unwrap();
        assert_eq!(r.list(&r.sources(), &[]).unwrap().len(), 5);
    }

    fn catalog(path: &Path, rows: &[(&str, &str)]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let db = rusqlite::Connection::open(path).unwrap();
        db.execute_batch("CREATE TABLE conversations(id TEXT,user_id TEXT,workspace TEXT,workspace_authority TEXT,title TEXT,custom_title TEXT,created_at INTEGER,last_message_at INTEGER,deleted_at INTEGER,archived_at INTEGER,runtime_provider TEXT,runtime_session_id TEXT,revision INTEGER);").unwrap();
        for (i, (workspace, host)) in rows.iter().enumerate() {
            db.execute("INSERT INTO conversations(id,user_id,workspace,workspace_authority,runtime_provider) VALUES(?1,'u',?2,?3,'codebuddy-ide-4')",rusqlite::params![i.to_string(),workspace,host]).unwrap();
        }
    }

    #[test]
    fn catalog_attributes_memory_without_cbc_and_keeps_hosts_separate() {
        let tmp = tempfile::tempdir().unwrap();
        let mut r = fixture(tmp.path());
        r.catalog = Some(tmp.path().join("catalog.db"));
        catalog(
            r.catalog.as_ref().unwrap(),
            &[
                ("/local/project", ""),
                ("/remote/project", "ssh-remote+CVM"),
                ("/collision-a/b", ""),
                ("/collision/a-b", ""),
            ],
        );
        for p in ["/local/project", "/remote/project", "/collision-a/b"] {
            put(r.projects.join(project_id(p)).join("memory/a.md"), "memory");
        }
        let docs = r.list(&r.sources(), &[]).unwrap();
        assert!(docs.iter().any(|d| d.project_path == "/local/project"));
        assert_eq!(docs.iter().filter(|d| d.project_path.is_empty()).count(), 2);
        r.host = Some("CVM".into());
        let docs = r.list(&r.sources(), &[]).unwrap();
        assert!(docs.iter().any(|d| d.project_path == "/remote/project"));
        assert!(!docs.iter().any(|d| d.project_path == "/local/project"));
    }

    #[test]
    fn watcher_updates_search_same_timestamp_metadata_and_deletion() {
        let tmp = tempfile::tempdir().unwrap();
        let r = fixture(tmp.path());
        let p = r.projects.join("demo/memory/topic.md");
        put(&p, "---\nname: Searchable name\n---\nalpha-content");
        let roots = vec![(r.projects.clone(), 0)];
        let adapters: Vec<Box<dyn AgentAdapter>> = vec![Box::new(Adapter(r.clone()))];
        let store = Arc::new(Store::open(&tmp.path().join("wake.db")).unwrap());
        assert!(run_memory_sync(&adapters, &store));
        let filter = MemoryFilter::default();
        assert_eq!(
            store
                .search_memories("alpha-content", &filter)
                .unwrap()
                .len(),
            1
        );
        let old = fs::metadata(&p).unwrap().modified().unwrap();
        put(&p, "---\nname: Searchable name\n---\nbravo-content");
        fs::File::options()
            .write(true)
            .open(&p)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(old))
            .unwrap();
        let event = |path: PathBuf| Ok(notify::Event::new(notify::EventKind::Any).add_path(path));
        crate::watcher::process_batch(
            &adapters,
            &store,
            &NullEvents,
            &roots,
            vec![event(p.clone())],
        );
        assert!(store
            .search_memories("alpha-content", &filter)
            .unwrap()
            .is_empty());
        assert_eq!(
            store
                .search_memories("bravo-content", &filter)
                .unwrap()
                .len(),
            1
        );
        let settings = r.home.as_ref().unwrap().join("ide-memory-settings.json");
        put(&settings, "{\"projectEnabled\":false}");
        crate::watcher::process_batch(
            &adapters,
            &store,
            &NullEvents,
            &roots,
            vec![event(settings.clone())],
        );
        let doc = store.list_memories(&filter).unwrap().pop().unwrap();
        assert_eq!(doc.details.ide_enabled, Some(false));
        assert!(doc.body.is_empty());
        // A read failure freezes this source rather than replacing it with an empty list.
        put(&settings, "{");
        assert!(!run_memory_sync(&adapters, &store));
        assert_eq!(store.memory_counts().unwrap().total, 1);
        put(&settings, "{}");
        fs::remove_file(&p).unwrap();
        crate::watcher::process_batch(&adapters, &store, &NullEvents, &roots, vec![event(p)]);
        assert_eq!(store.memory_counts().unwrap().total, 0);
        assert!(store
            .search_memories("bravo-content", &filter)
            .unwrap()
            .is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_or_unreadable_source_keeps_previous_index() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().unwrap();
        let r = fixture(tmp.path());
        let p = r.projects.join("demo/memory/a.md");
        put(&p, "searchable memory");
        let adapters: Vec<Box<dyn AgentAdapter>> = vec![Box::new(Adapter(r.clone()))];
        let store = Arc::new(Store::open(&tmp.path().join("wake.db")).unwrap());
        assert!(run_memory_sync(&adapters, &store));
        fs::remove_file(&p).unwrap();
        symlink(tmp.path().join("absent"), &p).unwrap();
        assert!(!run_memory_sync(&adapters, &store));
        assert_eq!(store.memory_counts().unwrap().total, 1);
        let doc = store
            .get_memory(&store.list_memories(&MemoryFilter::default()).unwrap()[0].key)
            .unwrap()
            .unwrap();
        assert_eq!(body(&doc), "searchable memory");
    }

    #[test]
    fn compression_handles_windows_unicode_and_long_paths() {
        assert_eq!(
            project_id("C:\\Users\\test-a\\project"),
            "C-Users-test-a-project"
        );
        assert_eq!(project_id("/a//b--c/"), "a-b-c");
        let p = format!("/{}", "记忆".repeat(70));
        let id = project_id(&p);
        assert!(id.len() <= 188);
        assert!(id.starts_with(&"记忆".repeat(30)));
    }

    #[test]
    fn unreadable_new_directory_keeps_the_indexed_centralized_fallback() {
        let tmp = tempfile::tempdir().unwrap();
        let r = fixture(tmp.path());
        put(
            r.home.as_ref().unwrap().join("memories/demo/old.md"),
            "fallback-body",
        );
        let adapters: Vec<Box<dyn AgentAdapter>> = vec![Box::new(Adapter(r.clone()))];
        let store = Arc::new(Store::open(&tmp.path().join("wake.db")).unwrap());
        run_memory_sync(&adapters, &store);
        let primary = r.projects.join("demo/memory/new.md");
        put(&primary, "placeholder");
        fs::write(&primary, [0xff, 0xfe]).unwrap();
        assert!(!run_memory_sync(&adapters, &store));
        assert_eq!(
            store
                .search_memories("fallback-body", &Default::default())
                .unwrap()
                .len(),
            1
        );
        put(&primary, "replacement-body");
        assert!(run_memory_sync(&adapters, &store));
        assert_eq!(store.memory_counts().unwrap().total, 1);
        assert!(store
            .search_memories("fallback-body", &Default::default())
            .unwrap()
            .is_empty());
        assert_eq!(
            store
                .search_memories("replacement-body", &Default::default())
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn shallow_parent_watches_notice_first_creation_and_empty_root_removal() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct Events(AtomicUsize);
        impl crate::scanner::ScanEvents for Events {
            fn on_progress(&self, _: &ScanProgress) {}
            fn on_sessions_changed(&self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        let tmp = tempfile::tempdir().unwrap();
        let r = fixture(tmp.path());
        assert!(r.watch_paths().contains(&tmp.path().to_path_buf()));
        assert!(!r.watch_recursive(tmp.path()));
        assert!(r.is_event(r.home.as_ref().unwrap()));
        fs::create_dir_all(&r.projects).unwrap();
        assert!(r.watch_paths().contains(&r.projects));
        let roots = vec![(r.projects.clone(), 0)];
        let adapters: Vec<Box<dyn AgentAdapter>> = vec![Box::new(Adapter(r.clone()))];
        let store = Arc::new(Store::open(&tmp.path().join("wake.db")).unwrap());
        fs::remove_dir(&r.projects).unwrap();
        let events = Events(AtomicUsize::new(0));
        crate::watcher::process_batch(
            &adapters,
            &store,
            &events,
            &roots,
            vec![Ok(
                notify::Event::new(notify::EventKind::Any).add_path(r.projects)
            )],
        );
        assert_eq!(
            events.0.load(Ordering::SeqCst),
            1,
            "rearm even when the removed source had no documents"
        );
    }

    #[test]
    fn old_memory_index_migrates_without_losing_documents() {
        let tmp = tempfile::tempdir().unwrap();
        let r = fixture(tmp.path());
        put(
            r.projects.join("demo/memory/a.md"),
            "---\nname: Migrated memory\n---\npreserved-body",
        );
        let path = tmp.path().join("wake.db");
        let adapters: Vec<Box<dyn AgentAdapter>> = vec![Box::new(Adapter(r))];
        {
            let store = Arc::new(Store::open(&path).unwrap());
            run_memory_sync(&adapters, &store);
            assert_eq!(store.memory_counts().unwrap().total, 1);
        }
        rusqlite::Connection::open(&path)
            .unwrap()
            .execute("ALTER TABLE memories DROP COLUMN details", [])
            .unwrap();
        assert!(Store::open_read_only(&path).is_err());
        let store = Arc::new(Store::open(&path).unwrap());
        assert_eq!(
            store
                .search_memories("preserved-body", &Default::default())
                .unwrap()
                .len(),
            1
        );
        assert!(run_memory_sync(&adapters, &store));
        assert_eq!(
            store.list_memories(&Default::default()).unwrap()[0]
                .details
                .origin,
            Some(MemoryOrigin::Shared)
        );
        assert!(Store::open_read_only(&path).is_ok());
    }

    #[test]
    #[ignore = "read-only real CodeBuddy memory audit into a temporary Wake index"]
    fn real_memories_are_attributed_searchable_and_readable() {
        let adapter = crate::adapters::codebuddy::CodebuddyAdapter::new();
        let sources = adapter.memory_sources();
        let docs = adapter.list_memories(&sources, &[]).unwrap();
        assert!(!docs.is_empty());
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(&tmp.path().join("audit.db")).unwrap();
        store
            .replace_memories(AgentId::Codebuddy, "", &docs, &[])
            .unwrap();
        for d in &docs {
            assert!(!d.title.is_empty());
            assert!(!body(d).is_empty());
            if d.scope == MemoryScope::Project {
                assert!(!d.project_path.is_empty(), "{}", d.path);
            }
            let terms: String = d.title.chars().take(8).collect();
            assert!(
                store
                    .search_memories(
                        &terms,
                        &MemoryFilter {
                            limit: 1000,
                            ..Default::default()
                        }
                    )
                    .unwrap()
                    .iter()
                    .any(|h| h.doc.key == d.key),
                "title search: {}",
                d.title
            );
        }
        eprintln!(
            "CodeBuddy memory audit: files={}, indexes={}, user={}, projects={}",
            docs.len(),
            docs.iter().filter(|d| d.details.is_index).count(),
            docs.iter().filter(|d| d.scope == MemoryScope::User).count(),
            docs.iter()
                .filter(|d| d.scope == MemoryScope::Project)
                .map(|d| &d.project_path)
                .collect::<BTreeSet<_>>()
                .len()
        );
    }
}
