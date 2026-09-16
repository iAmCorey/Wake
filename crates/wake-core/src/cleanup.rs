//! Local session cleanup: immutable review plans, conservative ownership checks,
//! per-target Trash results and a durable journal. No source database writes.
use crate::{
    adapters::{adapter_for, AgentAdapter},
    db::Store,
    models::*,
    services::terminal,
};
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeSet, HashMap, HashSet},
    io::Read,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    time::UNIX_EPOCH,
};

pub const DAY: i64 = 86_400_000;
pub const PREFS: &str = "cleanup.options.v1";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum CleanupSort {
    Created,
    Updated,
    #[default]
    Size,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum TimeField {
    Created,
    #[default]
    Updated,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(from = "SavedCleanupOptions")]
pub struct CleanupOptions {
    pub sort: CleanupSort,
    /// Old preferences used ascending dates and descending size, with no direction field.
    pub ascending: Option<bool>,
    pub created_days: i64,
    pub updated_days: i64,
    /// Empty source sets mean unrestricted; values within each set are ORed.
    pub agents: BTreeSet<AgentId>,
    pub projects: BTreeSet<String>,
    pub exclude_starred: bool,
    pub exclude_pinned: bool,
    pub only_cleanable: bool,
}
impl Default for CleanupOptions {
    fn default() -> Self {
        Self {
            sort: CleanupSort::Size,
            ascending: None,
            created_days: 0,
            updated_days: 30,
            agents: BTreeSet::new(),
            projects: BTreeSet::new(),
            exclude_starred: false,
            exclude_pinned: false,
            only_cleanable: false,
        }
    }
}

/// The original single-date filter is read only for migration. New preferences
/// always save both dates, including explicit zeroes for unrestricted dates.
#[derive(Default, Deserialize)]
#[serde(default)]
struct SavedCleanupOptions {
    sort: CleanupSort,
    ascending: Option<bool>,
    created_days: Option<i64>,
    updated_days: Option<i64>,
    time: TimeField,
    days: Option<i64>,
    agents: Option<BTreeSet<AgentId>>,
    projects: Option<BTreeSet<String>>,
    // Legacy single-source preferences, used only when the new sets are absent.
    agent: Option<AgentId>,
    project: Option<String>,
    exclude_starred: bool,
    exclude_pinned: bool,
    only_cleanable: bool,
}
impl From<SavedCleanupOptions> for CleanupOptions {
    fn from(saved: SavedCleanupOptions) -> Self {
        let age = |days, fallback| {
            if [0, 30, 90, 180, 365].contains(&days) {
                days
            } else {
                fallback
            }
        };
        let (created_days, updated_days) =
            if saved.created_days.is_some() || saved.updated_days.is_some() {
                (
                    age(saved.created_days.unwrap_or(0), 0),
                    age(saved.updated_days.unwrap_or(0), 30),
                )
            } else {
                let days = age(saved.days.unwrap_or(30), 30);
                match saved.time {
                    TimeField::Created => (days, 0),
                    TimeField::Updated => (0, days),
                }
            };
        Self {
            sort: saved.sort,
            ascending: saved.ascending,
            created_days,
            updated_days,
            agents: saved
                .agents
                .unwrap_or_else(|| saved.agent.into_iter().collect()),
            projects: saved
                .projects
                .unwrap_or_else(|| saved.project.into_iter().collect()),
            exclude_starred: saved.exclude_starred,
            exclude_pinned: saved.exclude_pinned,
            only_cleanable: saved.only_cleanable,
        }
    }
}
impl CleanupOptions {
    pub fn sort_ascending(&self) -> bool {
        self.ascending.unwrap_or(self.sort != CleanupSort::Size)
    }
    pub fn matches(&self, item: &CleanupCandidate, now: i64) -> bool {
        self.matches_metadata(&item.root, item.updated_at, &item.sessions, now)
    }
    fn matches_metadata(
        &self,
        root: &SessionMeta,
        updated_at: i64,
        sessions: &[SessionMeta],
        now: i64,
    ) -> bool {
        let older_than = |ts, days| days == 0 || (ts > 0 && ts <= now - days * DAY);
        older_than(root.created_at, self.created_days)
            && older_than(updated_at, self.updated_days)
            && (!self.exclude_starred || !sessions.iter().any(|s| s.favorite))
            && (!self.exclude_pinned || !sessions.iter().any(|s| s.pinned))
            && (self.agents.is_empty() || self.agents.contains(&root.agent))
            && (self.projects.is_empty() || self.projects.contains(&root.project_path))
    }
    pub fn matches_entry(&self, item: &CleanupEntry, now: i64) -> bool {
        match item {
            CleanupEntry::Available(candidate) => self.matches(candidate, now),
            CleanupEntry::Unavailable(item) => {
                !self.only_cleanable
                    && self.matches_metadata(&item.session, item.updated_at(), &item.sessions, now)
            }
        }
    }
    pub fn sort_entries(&self, items: &mut [CleanupEntry]) {
        let value = |entry: &CleanupEntry| match self.sort {
            CleanupSort::Created => {
                (entry.session().created_at > 0).then_some(i128::from(entry.session().created_at))
            }
            CleanupSort::Updated => {
                (entry.updated_at() > 0).then_some(i128::from(entry.updated_at()))
            }
            CleanupSort::Size => entry.candidate().map(|c| i128::from(c.bytes)),
        };
        items.sort_by(|a, b| {
            let order = match (value(a), value(b)) {
                (Some(a), Some(b)) if self.sort_ascending() => a.cmp(&b),
                (Some(a), Some(b)) => b.cmp(&a),
                // Unknown values remain last in either direction.
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => std::cmp::Ordering::Equal,
            };
            order.then_with(|| a.session().key.cmp(&b.session().key))
        });
    }
    pub fn sort(&self, items: &mut [CleanupCandidate]) {
        items.sort_by(|a, b| {
            let order = match self.sort {
                CleanupSort::Created => a.root.created_at.cmp(&b.root.created_at),
                CleanupSort::Updated => a.updated_at.cmp(&b.updated_at),
                CleanupSort::Size => a.bytes.cmp(&b.bytes),
            };
            (if self.sort_ascending() {
                order
            } else {
                order.reverse()
            })
            .then_with(|| a.root.key.cmp(&b.root.key))
        });
    }
}

pub struct IndexedSession {
    pub meta: SessionMeta,
    pub parent: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileStamp {
    pub path: PathBuf,
    pub bytes: u64,
    pub logical_bytes: u64,
    pub modified: u64,
    pub identity: (u64, u64),
    /// Upper half of Windows' 128-bit file ID (needed on ReFS).
    #[serde(default)]
    pub identity_high: u64,
    pub directory: bool,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CleanupTarget {
    pub path: PathBuf,
    pub files: Vec<FileStamp>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CleanupCandidate {
    pub root: SessionMeta,
    pub sessions: Vec<SessionMeta>,
    pub targets: Vec<CleanupTarget>,
    pub updated_at: i64,
    pub bytes: u64,
    pub prompts: i64,
    pub empty: bool,
}
impl CleanupCandidate {
    pub fn has_starred(&self) -> bool {
        self.sessions.iter().any(|s| s.favorite)
    }
    pub fn has_pinned(&self) -> bool {
        self.sessions.iter().any(|s| s.pinned)
    }
}
#[derive(Clone)]
pub struct UnavailableSession {
    pub session: SessionMeta,
    /// Known tree members, used only for filtering and display.
    pub sessions: Vec<SessionMeta>,
    pub reason: String,
}
impl UnavailableSession {
    pub fn updated_at(&self) -> i64 {
        self.sessions
            .iter()
            .map(|s| s.updated_at)
            .max()
            .unwrap_or(self.session.updated_at)
    }
}
#[derive(Clone)]
pub enum CleanupEntry {
    Available(CleanupCandidate),
    Unavailable(UnavailableSession),
}
impl CleanupEntry {
    pub fn session(&self) -> &SessionMeta {
        match self {
            Self::Available(c) => &c.root,
            Self::Unavailable(item) => &item.session,
        }
    }
    pub fn updated_at(&self) -> i64 {
        match self {
            Self::Available(c) => c.updated_at,
            Self::Unavailable(item) => item.updated_at(),
        }
    }
    pub fn candidate(&self) -> Option<&CleanupCandidate> {
        match self {
            Self::Available(c) => Some(c),
            Self::Unavailable(_) => None,
        }
    }
}
#[derive(Default)]
pub struct CleanupInventory {
    pub candidates: Vec<CleanupCandidate>,
    pub unavailable: Vec<UnavailableSession>,
}

#[derive(Default)]
pub struct CleanupReview {
    pub ready: Vec<CleanupCandidate>,
    pub skipped: Vec<UnavailableSession>,
}

/// Check every selected tree without moving files. A failed tree does not hide
/// the review of the remaining selection; the UI must present both groups.
/// Cancellation discards the entire review, so a partial check cannot be confirmed.
pub fn review(
    store: &Store,
    adapters: &[Box<dyn AgentAdapter>],
    chosen: Vec<CleanupCandidate>,
    cancel: &AtomicBool,
    mut progress: impl FnMut(usize),
) -> Option<CleanupReview> {
    let mut result = CleanupReview::default();
    for (i, candidate) in chosen.into_iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            return None;
        }
        match revalidate(store, adapters, &candidate) {
            Ok(()) => result.ready.push(candidate),
            Err(error) => result.skipped.push(UnavailableSession {
                session: candidate.root,
                sessions: candidate.sessions,
                reason: error.to_string(),
            }),
        }
        progress(i + 1);
    }
    (!cancel.load(Ordering::Relaxed)).then_some(result)
}

fn stamp(path: &Path) -> Result<FileStamp> {
    #[cfg(windows)]
    let crate::services::windows_fs::FileSnapshot {
        metadata: m,
        bytes,
        identity,
        identity_high,
    } = crate::services::windows_fs::snapshot(path)?;
    #[cfg(not(windows))]
    let m = std::fs::symlink_metadata(path)?;
    ensure!(
        !m.file_type().is_symlink(),
        "Symbolic links are not supported"
    );
    ensure!(m.is_file() || m.is_dir(), "Not a regular file or directory");
    let modified = u64::try_from(m.modified()?.duration_since(UNIX_EPOCH)?.as_nanos())?;
    #[cfg(unix)]
    let (bytes, identity) = {
        use std::os::unix::fs::MetadataExt;
        ensure!(
            !m.is_file() || m.nlink() == 1,
            "Shared hard links are not supported"
        );
        (
            if m.is_file() { m.blocks() * 512 } else { 0 },
            (m.dev(), m.ino()),
        )
    };
    Ok(FileStamp {
        path: path.to_path_buf(),
        bytes,
        logical_bytes: m.len(),
        modified,
        identity,
        #[cfg(windows)]
        identity_high,
        #[cfg(not(windows))]
        identity_high: 0,
        directory: m.is_dir(),
    })
}
fn target(path: &Path) -> Result<CleanupTarget> {
    // Canonical equality rejects links in ancestors as well as the target itself.
    ensure!(
        path.is_absolute() && same_canonical_path(path, &path.canonicalize()?),
        "Source path contains links or is not absolute"
    );
    let mut files = vec![];
    for entry in walkdir::WalkDir::new(path).follow_links(false) {
        files.push(stamp(entry?.path())?);
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(CleanupTarget {
        path: path.to_path_buf(),
        files,
    })
}

#[cfg(not(windows))]
fn same_canonical_path(path: &Path, canonical: &Path) -> bool {
    path == canonical
}

#[cfg(windows)]
fn same_canonical_path(path: &Path, canonical: &Path) -> bool {
    use std::path::{Component, Prefix};
    fn normalize(prefix: Prefix<'_>) -> Prefix<'_> {
        match prefix {
            Prefix::VerbatimDisk(drive) => Prefix::Disk(drive),
            Prefix::VerbatimUNC(server, share) => Prefix::UNC(server, share),
            other => other,
        }
    }
    // Windows canonicalize adds an extended-length prefix even without links.
    // Only normalize that prefix; a different ancestor or filename still fails.
    let mut original = path.components();
    let mut resolved = canonical.components();
    match (original.next(), resolved.next()) {
        (Some(Component::Prefix(a)), Some(Component::Prefix(b))) => {
            normalize(a.kind()) == normalize(b.kind()) && original.eq(resolved)
        }
        _ => false,
    }
}
fn supported_adapter<'a>(
    adapters: &'a [Box<dyn AgentAdapter>],
    meta: &SessionMeta,
) -> Result<&'a dyn AgentAdapter> {
    ensure!(meta.host.is_empty(), "Remote sessions are read-only");
    let a = adapter_for(adapters, meta.agent, &meta.file_path).context("Source is not enabled")?;
    ensure!(a.host().is_empty(), "Remote mirrors are read-only");
    ensure!(
        a.cleanup_paths(meta).is_some(),
        "This source does not support independent file cleanup"
    );
    let p = Path::new(&meta.file_path);
    ensure!(
        a.data_roots()
            .iter()
            .any(|r| r.is_dir() && p.starts_with(r) && p != r),
        "Source is outside the enabled location"
    );
    Ok(a)
}
/// File ownership and metadata needed for deletion, without display statistics.
struct CleanupPlan {
    sessions: Vec<SessionMeta>,
    targets: Vec<CleanupTarget>,
}

fn build_plan(
    root: &IndexedSession,
    index: &[IndexedSession],
    adapters: &[Box<dyn AgentAdapter>],
    ownership: &HashMap<String, Vec<PathBuf>>,
) -> Result<CleanupPlan> {
    let mut members = vec![root];
    let mut keys = HashSet::from([root.meta.key.as_str()]);
    let mut cursor = 0;
    while cursor < members.len() {
        let parent = members[cursor].meta.key.clone();
        for child in index.iter().filter(|s| s.parent == parent) {
            ensure!(
                keys.insert(child.meta.key.as_str()),
                "Session tree contains a cycle"
            );
            members.push(child);
        }
        cursor += 1;
    }
    let mut paths = vec![];
    for item in &members {
        let m = &item.meta;
        terminal::validate_trash_path(Path::new(&m.file_path))?;
        let a = supported_adapter(adapters, m)?;
        let owned = a
            .cleanup_paths(m)
            .context("This source does not support independent file cleanup")?;
        ensure!(!owned.is_empty(), "No independently owned files");
        for p in owned {
            let p = PathBuf::from(p);
            terminal::validate_trash_path(&p)?;
            ensure!(
                a.data_roots().iter().any(|r| p.starts_with(r) && p != *r),
                "Cleanup target is outside the source location"
            );
            ensure!(
                std::fs::symlink_metadata(&p).is_ok(),
                "Source file is missing"
            );
            paths.push(p);
        }
    }
    paths.sort();
    paths.dedup();
    let mut roots: Vec<PathBuf> = vec![];
    for path in paths {
        if !roots.iter().any(|p| path.starts_with(p)) {
            roots.push(path);
        }
    }
    for other in index.iter().filter(|s| !keys.contains(s.meta.key.as_str())) {
        ensure!(
            !roots
                .iter()
                .any(|p| Path::new(&other.meta.file_path).starts_with(p)),
            "Target also contains another session"
        );
        // Sessions outside the current filter still own all of their sidecars.
        if let Some(paths) = ownership.get(&other.meta.key) {
            ensure!(
                !paths.iter().any(|shared| roots
                    .iter()
                    .any(|p| shared.starts_with(p) || p.starts_with(shared))),
                "Shared cleanup target"
            );
        }
    }
    let targets = roots
        .iter()
        .map(|p| target(p))
        .collect::<Result<Vec<_>>>()?;
    Ok(CleanupPlan {
        sessions: members.iter().map(|s| s.meta.clone()).collect(),
        targets,
    })
}

fn inventory_candidate(
    root: &SessionMeta,
    plan: CleanupPlan,
    prompt_counts: &HashMap<String, i64>,
    adapters: &[Box<dyn AgentAdapter>],
) -> Result<CleanupCandidate> {
    let prompts = plan
        .sessions
        .iter()
        .map(|s| prompt_counts.get(&s.key).copied().unwrap_or(0))
        .sum();
    let updated_at = plan
        .sessions
        .iter()
        .map(|s| s.updated_at)
        .max()
        .unwrap_or(root.updated_at);
    let bytes = plan
        .targets
        .iter()
        .flat_map(|t| &t.files)
        .map(|f| f.bytes)
        .sum();
    let mut empty = plan.sessions.iter().all(|s| s.message_count == 0) && prompts == 0;
    // Empty is a positive parse result, never a missing/failed index entry.
    if empty {
        for s in &plan.sessions {
            let p =
                supported_adapter(adapters, s)?.parse_transcript(&SessionFileRef::from_meta(s))?;
            empty &= p.unknown_line_count == 0 && p.mainline.is_empty() && p.sidechains.is_empty();
        }
    }
    Ok(CleanupCandidate {
        root: root.clone(),
        sessions: plan.sessions,
        targets: plan.targets,
        updated_at,
        bytes,
        prompts,
        empty,
    })
}

pub fn inventory(store: &Store, adapters: &[Box<dyn AgentAdapter>]) -> Result<CleanupInventory> {
    let index = store.cleanup_index()?;
    let prompt_counts = store.cleanup_prompt_counts()?;
    let ownership = ownership(&index, adapters);
    let mut result = CleanupInventory::default();
    for root in index.iter().filter(|s| s.parent.is_empty()) {
        match build_plan(root, &index, adapters, &ownership)
            .and_then(|plan| inventory_candidate(&root.meta, plan, &prompt_counts, adapters))
        {
            Ok(c) => result.candidates.push(c),
            Err(e) => result.unavailable.push(UnavailableSession {
                session: root.meta.clone(),
                sessions: known_tree(root, &index),
                reason: e.to_string(),
            }),
        }
    }
    // Shared sidecars must not be counted twice or moved with an unselected tree.
    let mut paths: Vec<_> = result
        .candidates
        .iter()
        .enumerate()
        .flat_map(|(i, c)| c.targets.iter().map(move |t| (&t.path, i)))
        .collect();
    paths.sort();
    let mut shared = HashSet::new();
    for (i, (a, ai)) in paths.iter().enumerate() {
        for (b, bi) in paths.iter().skip(i + 1) {
            if !b.starts_with(a) {
                break;
            }
            if ai != bi {
                shared.insert(*ai);
                shared.insert(*bi);
            }
        }
    }
    result.candidates = result
        .candidates
        .into_iter()
        .enumerate()
        .filter_map(|(i, c)| {
            if shared.contains(&i) {
                result.unavailable.push(UnavailableSession {
                    session: c.root,
                    sessions: c.sessions,
                    reason: "Shared cleanup target".into(),
                });
                None
            } else {
                Some(c)
            }
        })
        .collect();
    for orphan in index
        .iter()
        .filter(|s| !s.parent.is_empty() && !index.iter().any(|p| p.meta.key == s.parent))
    {
        result.unavailable.push(UnavailableSession {
            session: orphan.meta.clone(),
            sessions: known_tree(orphan, &index),
            reason: "Parent session is unavailable".into(),
        });
    }
    Ok(result)
}

// Availability failures still need tree-aware dates and flags. This traversal
// tolerates malformed relationships without granting cleanup ownership.
fn known_tree(root: &IndexedSession, index: &[IndexedSession]) -> Vec<SessionMeta> {
    let mut members = vec![root.meta.clone()];
    let mut keys = HashSet::from([root.meta.key.as_str()]);
    let mut cursor = 0;
    while cursor < members.len() {
        let parent = members[cursor].key.clone();
        for child in index.iter().filter(|s| s.parent == parent) {
            if keys.insert(child.meta.key.as_str()) {
                members.push(child.meta.clone());
            }
        }
        cursor += 1;
    }
    members
}

fn ownership(
    index: &[IndexedSession],
    adapters: &[Box<dyn AgentAdapter>],
) -> HashMap<String, Vec<PathBuf>> {
    index
        .iter()
        .filter(|s| s.meta.host.is_empty())
        .filter_map(|s| {
            adapter_for(adapters, s.meta.agent, &s.meta.file_path).map(|a| {
                (
                    s.meta.key.clone(),
                    a.session_paths(&s.meta)
                        .into_iter()
                        .map(PathBuf::from)
                        .collect(),
                )
            })
        })
        .collect()
}

pub fn revalidate(
    store: &Store,
    adapters: &[Box<dyn AgentAdapter>],
    old: &CleanupCandidate,
) -> Result<()> {
    let index = store.cleanup_index()?;
    let root = index
        .iter()
        .find(|s| s.meta.key == old.root.key)
        .context("Session no longer exists")?;
    ensure!(root.parent.is_empty(), "Session tree changed");
    let new = build_plan(root, &index, adapters, &ownership(&index, adapters))?;
    let signature = |sessions: &[SessionMeta]| {
        let mut values = sessions
            .iter()
            .map(|m| {
                (
                    m.key.clone(),
                    m.file_path.clone(),
                    m.updated_at,
                    m.created_at,
                    m.favorite,
                    m.pinned,
                )
            })
            .collect::<Vec<_>>();
        values.sort();
        values
    };
    ensure!(
        signature(&new.sessions) == signature(&old.sessions) && new.targets == old.targets,
        "Session or files changed since review; refresh and review again"
    );
    for meta in &new.sessions {
        let parsed =
            supported_adapter(adapters, meta)?.parse_session(&SessionFileRef::from_meta(meta))?;
        ensure!(
            parsed.unknown_line_count == 0,
            "Session has unrecognized content"
        );
    }
    // Parsing can take time; stat again before allowing any move.
    for old in &new.targets {
        ensure!(target(&old.path)? == *old, "Source changed while checking");
    }
    let index = store.cleanup_index()?;
    let root = index
        .iter()
        .find(|s| s.meta.key == old.root.key)
        .context("Session no longer exists")?;
    ensure!(root.parent.is_empty(), "Session tree changed");
    let checked = build_plan(root, &index, adapters, &ownership(&index, adapters))?;
    ensure!(
        signature(&checked.sessions) == signature(&old.sessions) && checked.targets == old.targets,
        "Session or files changed since review; refresh and review again"
    );
    Ok(())
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum TargetStatus {
    Pending,
    Moving,
    Moved,
    Failed(String),
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CleanupRecord {
    pub candidate: CleanupCandidate,
    pub targets: Vec<TargetStatus>,
    /// Kept when reading old journals; file recovery is handled by the OS.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub trash_paths: Vec<Option<PathBuf>>,
    /// Captured and journaled before any move, independent of inode/allocation
    /// changes caused by the OS restoring a file across volumes.
    #[serde(default)]
    pub content_hashes: HashMap<PathBuf, String>,
    pub indexed: bool,
    pub restored: bool,
    pub error: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CleanupBatch {
    pub id: String,
    pub stamp: i64,
    pub records: Vec<CleanupRecord>,
    pub finished: bool,
}
impl CleanupBatch {
    pub fn new(candidates: Vec<CleanupCandidate>, stamp: i64) -> Self {
        let nonce = std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        Self {
            id: format!("{stamp}-{nonce}"),
            stamp,
            records: candidates
                .into_iter()
                .map(|candidate| CleanupRecord {
                    targets: vec![TargetStatus::Pending; candidate.targets.len()],
                    trash_paths: Vec::new(),
                    content_hashes: HashMap::new(),
                    candidate,
                    indexed: false,
                    restored: false,
                    error: None,
                })
                .collect(),
            finished: false,
        }
    }
    pub fn save(&self, store: &Store) -> Result<()> {
        store.pref_set(
            &format!("cleanup.batch.{}", self.id),
            &serde_json::to_string(self)?,
        )
    }
}
pub fn history(store: &Store) -> Result<Vec<CleanupBatch>> {
    store
        .cleanup_journals()?
        .iter()
        .map(|s| serde_json::from_str(s).map_err(Into::into))
        .collect()
}

/// Retry only the index transaction for targets durably recorded as moved.
/// An interrupted `Moving` status remains ambiguous and requires user review.
pub fn retry_index_updates(store: &Store, batch: &mut CleanupBatch) -> Result<usize> {
    let mut count = 0;
    for i in 0..batch.records.len() {
        let r = &batch.records[i];
        if r.indexed || r.restored || !r.targets.iter().all(|s| *s == TargetStatus::Moved) {
            continue;
        }
        let result = (|| {
            for target in &r.candidate.targets {
                ensure!(
                    !target.path.try_exists()?,
                    "Source is present again; review it before updating the index"
                );
            }
            store.complete_cleanup(&r.candidate.sessions, batch.stamp)
        })();
        match result {
            Ok(()) => {
                batch.records[i].indexed = true;
                batch.records[i].error = None;
                count += 1;
            }
            Err(e) => batch.records[i].error = Some(e.to_string()),
        }
        batch.save(store)?;
    }
    Ok(count)
}
pub fn execute(
    store: &Store,
    adapters: &[Box<dyn AgentAdapter>],
    batch: &mut CleanupBatch,
    cancel: &AtomicBool,
    progress: impl Fn(usize),
) -> Result<()> {
    execute_with(store, adapters, batch, cancel, progress, |path| {
        terminal::trash_paths(&[path.to_string_lossy().into_owned()])
    })
}
fn execute_with(
    store: &Store,
    adapters: &[Box<dyn AgentAdapter>],
    batch: &mut CleanupBatch,
    cancel: &AtomicBool,
    progress: impl Fn(usize),
    trash: impl Fn(&Path) -> Result<()>,
) -> Result<()> {
    batch.save(store)?;
    for i in 0..batch.records.len() {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let plan = batch.records[i].candidate.clone();
        if let Err(e) = revalidate(store, adapters, &plan) {
            batch.records[i].error = Some(e.to_string());
            batch.save(store)?;
            progress(i + 1);
            continue;
        }
        match content_hashes(&plan.targets).and_then(|hashes| {
            // Hashing large trees can take time; recheck source ownership and
            // user metadata as well as the file snapshots before moving.
            revalidate(store, adapters, &plan)?;
            Ok(hashes)
        }) {
            Ok(hashes) => batch.records[i].content_hashes = hashes,
            Err(e) => {
                batch.records[i].error = Some(e.to_string());
                batch.save(store)?;
                progress(i + 1);
                continue;
            }
        }
        // Recovery evidence must be durable before the first move.
        batch.save(store)?;
        for (j, expected) in plan.targets.iter().enumerate() {
            batch.records[i].targets[j] = TargetStatus::Moving;
            batch.save(store)?;
            let result = (|| {
                ensure!(
                    target(&expected.path)? == *expected,
                    "Source changed before move"
                );
                trash(&expected.path)?;
                ensure!(
                    !expected.path.try_exists()?,
                    "Source is still present after move"
                );
                Ok(())
            })();
            match result {
                Ok(()) => batch.records[i].targets[j] = TargetStatus::Moved,
                Err(e) => {
                    batch.records[i].targets[j] = TargetStatus::Failed(format!("{e:#}"));
                    batch.records[i].error = Some(format!("{e:#}"));
                    batch.save(store)?;
                    break;
                }
            }
            batch.save(store)?;
        }
        if batch.records[i]
            .targets
            .iter()
            .all(|s| *s == TargetStatus::Moved)
        {
            match store.complete_cleanup(&plan.sessions, batch.stamp) {
                Ok(()) => batch.records[i].indexed = true,
                Err(e) => batch.records[i].error = Some(e.to_string()),
            }
        }
        batch.save(store)?;
        progress(i + 1);
    }
    batch.finished = true;
    batch.save(store)
}

fn hash_file(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn content_hashes(targets: &[CleanupTarget]) -> Result<HashMap<PathBuf, String>> {
    let mut hashes = HashMap::new();
    for expected in targets {
        ensure!(
            target(&expected.path)? == *expected,
            "Source changed while checking"
        );
        for file in expected.files.iter().filter(|file| !file.directory) {
            hashes.insert(file.path.clone(), hash_file(&file.path)?);
        }
        ensure!(
            target(&expected.path)? == *expected,
            "Source changed while checking"
        );
    }
    Ok(hashes)
}

fn verify_restored_files(record: &CleanupRecord) -> Result<Vec<CleanupTarget>> {
    let mut checked = Vec::new();
    for expected in &record.candidate.targets {
        for file in &expected.files {
            ensure!(
                file.path.try_exists()?,
                "Restored files are still missing from their original locations."
            );
        }
        // This also rejects replaced targets/ancestors that are now symlinks.
        let current = target(&expected.path)?;
        let files: HashMap<_, _> = current.files.iter().map(|f| (&f.path, f)).collect();
        for old in &expected.files {
            let file = files
                .get(&old.path)
                .context("Restored session could not be verified")?;
            ensure!(
                file.directory == old.directory,
                "Restored files do not match the deleted files"
            );
            if !old.directory {
                ensure!(
                    file.logical_bytes == old.logical_bytes,
                    "Restored files do not match the deleted files"
                );
                if record.content_hashes.is_empty() {
                    // Legacy journals lack content evidence. Require matching
                    // type, length and modification time, plus session parsing.
                    ensure!(
                        file.modified == old.modified,
                        "Restored files do not match the deleted files"
                    );
                } else {
                    ensure!(
                        record.content_hashes.get(&old.path) == Some(&hash_file(&old.path)?),
                        "Restored files do not match the deleted files"
                    );
                }
            }
        }
        ensure!(
            target(&current.path)? == current,
            "Restored files changed before verification"
        );
        checked.push(current);
    }
    Ok(checked)
}

/// Users restore source files using their OS first. Validate the full tree and
/// remove only this batch's tombstones. The caller starts a normal rescan.
pub fn restore(
    store: &Store,
    adapters: &[Box<dyn AgentAdapter>],
    batch: &mut CleanupBatch,
) -> Result<usize> {
    let mut count = 0;
    for i in 0..batch.records.len() {
        if batch.records[i].restored || !batch.records[i].can_restore() {
            continue;
        }
        let result = (|| {
            let record = &batch.records[i];
            store.validate_cleanup_restore(&record.candidate.sessions, batch.stamp)?;
            let checked = verify_restored_files(record)?;
            for meta in &record.candidate.sessions {
                let parsed = supported_adapter(adapters, meta)?
                    .parse_session(&SessionFileRef::from_meta(meta))?;
                ensure!(
                    parsed.unknown_line_count == 0 && parsed.meta.id == meta.id,
                    "Restored session could not be verified"
                );
            }
            for expected in checked {
                ensure!(
                    target(&expected.path)? == expected,
                    "Restored files changed before verification"
                );
            }
            store.restore_cleanup(&record.candidate.sessions, batch.stamp)?;
            Ok(())
        })();
        match result {
            Ok(()) => {
                batch.records[i].restored = true;
                batch.records[i].error = None;
                count += 1;
            }
            Err(e) => batch.records[i].error = Some(format!("{e:#}")),
        }
        batch.save(store)?;
    }
    if count == 0 {
        bail!("No restored sessions found. Check the file locations below.");
    }
    Ok(count)
}

impl CleanupRecord {
    pub fn can_restore(&self) -> bool {
        !self.restored
            && self.targets.iter().any(|s| {
                matches!(
                    s,
                    TargetStatus::Moved | TargetStatus::Moving | TargetStatus::Failed(_)
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        cell::Cell,
        fs::{self, File, FileTimes},
        time::Duration,
    };

    struct FilesAdapter {
        root: PathBuf,
        supported: bool,
    }
    impl AgentAdapter for FilesAdapter {
        fn agent(&self) -> AgentId {
            AgentId::ClaudeCode
        }
        fn data_roots(&self) -> Vec<PathBuf> {
            vec![self.root.clone()]
        }
        fn with_custom_root(&self, root: PathBuf) -> Box<dyn AgentAdapter> {
            Box::new(Self {
                root,
                supported: self.supported,
            })
        }
        fn list_session_files(&self) -> Result<Vec<SessionFileRef>> {
            Ok(vec![])
        }
        fn parse_session(&self, r: &SessionFileRef) -> Result<ParsedSession> {
            let text = fs::read_to_string(&r.file_path)?;
            Ok(ParsedSession {
                meta: meta(&self.root, &r.native_id, 100),
                units: vec![],
                unknown_line_count: u32::from(text == "BAD"),
            })
        }
        fn parse_transcript(&self, r: &SessionFileRef) -> Result<ParsedTranscript> {
            let p = self.parse_session(r)?;
            Ok(ParsedTranscript {
                meta: p.meta,
                mainline: vec![],
                sidechains: vec![],
                unknown_line_count: p.unknown_line_count,
            })
        }
        fn session_paths(&self, m: &SessionMeta) -> Vec<String> {
            let mut paths = vec![m.file_path.clone()];
            let dir = self.root.join(&m.id);
            if dir.exists() {
                paths.push(dir.to_string_lossy().into_owned());
            }
            paths
        }
        fn cleanup_paths(&self, m: &SessionMeta) -> Option<Vec<String>> {
            self.supported.then(|| self.session_paths(m))
        }
    }
    fn now() -> i64 {
        chrono::Utc::now().timestamp_millis()
    }
    fn meta(root: &Path, id: &str, age: i64) -> SessionMeta {
        SessionMeta {
            key: format!("claude-code:{id}"),
            id: id.into(),
            host: String::new(),
            agent: AgentId::ClaudeCode,
            title: id.into(),
            project_path: "/synthetic/project".into(),
            project_name: "project".into(),
            file_path: root
                .join(format!("{id}.jsonl"))
                .to_string_lossy()
                .into_owned(),
            created_at: now() - 400 * DAY,
            updated_at: now() - age * DAY,
            message_count: 2,
            size_bytes: 4,
            git_branch: None,
            model: None,
            tokens_used: None,
            archived: false,
            source: None,
            favorite: false,
            pinned: false,
        }
    }
    fn age(path: &Path) {
        #[cfg(windows)]
        let file = {
            use std::os::windows::fs::OpenOptionsExt;
            use windows_sys::Win32::Storage::FileSystem::{
                FILE_FLAG_BACKUP_SEMANTICS, FILE_WRITE_ATTRIBUTES,
            };
            fs::OpenOptions::new()
                .access_mode(FILE_WRITE_ATTRIBUTES)
                .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
                .open(path)
                .unwrap()
        };
        #[cfg(not(windows))]
        let file = File::open(path).unwrap();
        file.set_times(
            FileTimes::new()
                .set_modified(UNIX_EPOCH + Duration::from_millis((now() - 100 * DAY) as u64)),
        )
        .unwrap();
    }
    fn setup() -> (
        tempfile::TempDir,
        Store,
        Vec<Box<dyn AgentAdapter>>,
        PathBuf,
    ) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap().join("sessions");
        fs::create_dir(&root).unwrap();
        let store = Store::open(&temp.path().join("index.db")).unwrap();
        let adapters: Vec<Box<dyn AgentAdapter>> = vec![Box::new(FilesAdapter {
            root: root.clone(),
            supported: true,
        })];
        (temp, store, adapters, root)
    }
    fn insert(store: &Store, root: &Path, id: &str, days: i64) -> SessionMeta {
        let m = meta(root, id, days);
        fs::write(&m.file_path, "test").unwrap();
        age(Path::new(&m.file_path));
        store
            .write_session(
                &m,
                m.updated_at,
                &[IndexUnit {
                    seq: 0,
                    sidechain_id: None,
                    role: Role::User,
                    timestamp: Some(m.updated_at),
                    text: "synthetic prompt".into(),
                }],
            )
            .unwrap();
        m
    }
    #[test]
    fn unavailable_entries_follow_date_source_and_tree_flag_filters() {
        let (_temp, store, adapters, root) = setup();
        let parent = insert(&store, &root, "unavailable-parent", 180);
        let child = insert(&store, &root, "unavailable-child", 60);
        store
            .replace_parent_links(
                AgentId::ClaudeCode,
                &[(child.key.clone(), parent.key.clone())],
            )
            .unwrap();
        store
            .set_user_data(&child.key, Some(true), Some(true))
            .unwrap();
        fs::remove_file(&parent.file_path).unwrap(); // Synthetic missing source.
        let entry =
            CleanupEntry::Unavailable(inventory(&store, &adapters).unwrap().unavailable.remove(0));
        let mut options = CleanupOptions::default();
        assert!(options.matches_entry(&entry, now()));
        options.created_days = 90;
        options.updated_days = 90;
        assert!(
            !options.matches_entry(&entry, now()),
            "A recent child must affect the update filter"
        );
        options.updated_days = 30;
        assert!(options.matches_entry(&entry, now()));
        options.exclude_starred = true;
        assert!(!options.matches_entry(&entry, now()));
        options.exclude_starred = false;
        options.exclude_pinned = true;
        assert!(!options.matches_entry(&entry, now()));
        options.exclude_pinned = false;
        options.agents = BTreeSet::from([AgentId::Codex]);
        assert!(!options.matches_entry(&entry, now()));
        options.agents = BTreeSet::from([parent.agent]);
        options.projects = BTreeSet::from(["different-project".into()]);
        assert!(!options.matches_entry(&entry, now()));
        options.projects = BTreeSet::from([parent.project_path]);
        assert!(options.matches_entry(&entry, now()));
        options.only_cleanable = true;
        assert!(!options.matches_entry(&entry, now()));
        assert!(
            entry.candidate().is_none(),
            "Unavailable entries never yield a cleanup target"
        );
        let restored: CleanupOptions =
            serde_json::from_value(serde_json::to_value(options).unwrap()).unwrap();
        assert!(restored.only_cleanable);
        let old: CleanupOptions = serde_json::from_str(r#"{"days":0}"#).unwrap();
        assert!(
            !old.only_cleanable,
            "Existing preferences must not hide unavailable sessions"
        );
    }

    #[test]
    fn mixed_entries_sort_together_with_unknown_size_last_in_both_directions() {
        let (_temp, store, adapters, root) = setup();
        insert(&store, &root, "old", 180);
        let missing = insert(&store, &root, "middle", 90);
        insert(&store, &root, "new", 60);
        fs::remove_file(missing.file_path).unwrap();
        let inv = inventory(&store, &adapters).unwrap();
        let mut entries: Vec<_> = inv
            .candidates
            .into_iter()
            .map(|mut c| {
                c.bytes = if c.root.id == "old" { 0 } else { 128 };
                CleanupEntry::Available(c)
            })
            .chain(inv.unavailable.into_iter().map(CleanupEntry::Unavailable))
            .collect();
        let mut options = CleanupOptions::default();
        for (sort, ascending, expected) in [
            (CleanupSort::Size, true, ["old", "new", "middle"]),
            (CleanupSort::Size, false, ["new", "old", "middle"]),
            (CleanupSort::Created, true, ["old", "middle", "new"]),
            (CleanupSort::Updated, false, ["new", "middle", "old"]),
        ] {
            options.sort = sort;
            options.ascending = Some(ascending);
            options.sort_entries(&mut entries);
            assert_eq!(
                entries
                    .iter()
                    .map(|e| e.session().id.as_str())
                    .collect::<Vec<_>>(),
                expected
            );
        }
        assert_eq!(
            entries.iter().filter_map(CleanupEntry::candidate).count(),
            2
        );
    }

    #[test]
    fn snapshot_uses_mainline_prompt_count_and_keeps_archived_children() {
        let (_temp, store, adapters, root) = setup();
        let parent = insert(&store, &root, "parent", 120);
        let mut child = insert(&store, &root, "child", 90);
        child.archived = true;
        let units: Vec<_> = [
            (Role::User, None),
            (Role::Assistant, None),
            (Role::User, Some("sidechain")),
        ]
        .into_iter()
        .enumerate()
        .map(|(seq, (role, sidechain))| IndexUnit {
            seq: seq as i64,
            sidechain_id: sidechain.map(str::to_string),
            role,
            timestamp: None,
            text: "synthetic".into(),
        })
        .collect();
        store
            .write_session(&child, child.updated_at, &units)
            .unwrap();
        store
            .replace_parent_links(
                AgentId::ClaudeCode,
                &[(child.key.clone(), parent.key.clone())],
            )
            .unwrap();
        let inv = inventory(&store, &adapters).unwrap();
        assert_eq!(inv.candidates.len(), 1);
        let c = &inv.candidates[0];
        assert_eq!(c.sessions.len(), 2);
        assert_eq!(c.prompts, 2);
        assert_eq!(c.updated_at, child.updated_at);
        let mut options = CleanupOptions::default();
        store.set_user_data(&child.key, Some(true), None).unwrap();
        let starred = inventory(&store, &adapters).unwrap().candidates.remove(0);
        assert!(options.matches(&starred, now()));
        options.exclude_starred = true;
        assert!(!options.matches(&starred, now()));
        options.exclude_starred = false;
        assert!(options.matches(&starred, now()));
        store
            .set_user_data(&child.key, Some(false), Some(true))
            .unwrap();
        let pinned = inventory(&store, &adapters).unwrap().candidates.remove(0);
        assert!(options.matches(&pinned, now()));
        options.exclude_pinned = true;
        assert!(!options.matches(&pinned, now()));
        options.exclude_pinned = false;
        assert!(options.matches(&pinned, now()));
        child.updated_at = now() - DAY;
        store.write_session(&child, child.updated_at, &[]).unwrap();
        let recent = inventory(&store, &adapters).unwrap().candidates.remove(0);
        assert!(!options.matches(&recent, now()));
        options.created_days = 30;
        assert!(
            !options.matches(&recent, now()),
            "Both date filters must match"
        );
        options.updated_days = 0;
        assert!(
            options.matches(&recent, now()),
            "Creation can be filtered independently"
        );
        options.created_days = 0;
        assert!(options.matches(&recent, now()));
    }
    #[test]
    fn reviewed_starred_pinned_and_recent_files_can_be_cleaned_up() {
        let (temp, store, adapters, root) = setup();
        let m = insert(&store, &root, "recent", 1);
        store.set_user_data(&m.key, Some(true), Some(true)).unwrap();
        // Fresh file modification time is not an invisible age filter.
        fs::write(&m.file_path, "recent content").unwrap();
        let candidate = inventory(&store, &adapters).unwrap().candidates.remove(0);
        assert!(candidate.has_starred() && candidate.has_pinned());
        let mut options: CleanupOptions = serde_json::from_str(r#"{"days":0}"#).unwrap();
        assert!(
            options.matches(&candidate, now()),
            "Old preferences do not introduce hidden exclusions"
        );
        options.exclude_starred = true;
        options.exclude_pinned = true;
        let restored: CleanupOptions =
            serde_json::from_str(&serde_json::to_string(&options).unwrap()).unwrap();
        assert!(restored.exclude_starred && restored.exclude_pinned);
        assert!(!restored.matches(&candidate, now()));
        let mut batch = CleanupBatch::new(vec![candidate], now());
        execute_with(
            &store,
            &adapters,
            &mut batch,
            &AtomicBool::new(false),
            |_| {},
            |p| {
                fs::rename(p, temp.path().join("fake-trash"))?;
                Ok(())
            },
        )
        .unwrap();
        assert!(batch.records[0].indexed);
        assert_eq!(batch.records[0].targets, vec![TargetStatus::Moved]);
        assert!(store.is_key_tombstoned(&m.key));
    }

    #[test]
    fn date_filters_have_independent_boundaries_and_sort_does_not_change_them() {
        let (_temp, store, adapters, root) = setup();
        insert(&store, &root, "a", 100);
        let mut c = inventory(&store, &adapters).unwrap().candidates.remove(0);
        let now = now();
        let mut options = CleanupOptions::default();
        for days in [30, 90, 180, 365] {
            options.updated_days = days;
            c.updated_at = now - days * DAY;
            assert!(options.matches(&c, now));
            c.updated_at += 1;
            assert!(!options.matches(&c, now));
        }
        options.updated_days = 0;
        for days in [30, 90, 180, 365] {
            options.created_days = days;
            c.root.created_at = now - days * DAY;
            assert!(options.matches(&c, now));
            c.root.created_at += 1;
            assert!(!options.matches(&c, now));
        }
        options.created_days = 180;
        options.updated_days = 30;
        c.root.created_at = now - 180 * DAY;
        c.updated_at = now - 30 * DAY;
        assert!(options.matches(&c, now));
        c.root.created_at += 1;
        assert!(
            !options.matches(&c, now),
            "A matching update date cannot override creation"
        );
        c.root.created_at -= 1;
        c.updated_at += 1;
        assert!(
            !options.matches(&c, now),
            "A matching creation date cannot override updates"
        );
        c.root.created_at = 0;
        c.updated_at = 0;
        assert!(!options.matches(&c, now));
        options.updated_days = 0;
        assert!(!options.matches(&c, now));
        options.created_days = 0;
        assert!(options.matches(&c, now));
        let mut a = c.clone();
        a.root.key = "a".into();
        a.root.created_at = 1;
        a.updated_at = 100;
        a.bytes = 30;
        let mut b = c;
        b.root.key = "b".into();
        b.root.created_at = 2;
        b.updated_at = 50;
        b.bytes = 80;
        let mut items = vec![a, b];
        options.sort = CleanupSort::Created;
        options.sort(&mut items);
        assert_eq!(items[0].root.key, "a");
        options.sort = CleanupSort::Updated;
        options.sort(&mut items);
        assert_eq!(items[0].root.key, "b");
        options.sort = CleanupSort::Size;
        options.sort(&mut items);
        assert_eq!(items[0].root.key, "b");
        for (sort, ascending, first) in [
            (CleanupSort::Created, true, "a"),
            (CleanupSort::Created, false, "b"),
            (CleanupSort::Updated, true, "b"),
            (CleanupSort::Updated, false, "a"),
            (CleanupSort::Size, true, "a"),
            (CleanupSort::Size, false, "b"),
        ] {
            options.sort = sort;
            options.ascending = Some(ascending);
            options.sort(&mut items);
            assert_eq!(items[0].root.key, first);
            assert_eq!(options.ascending, Some(ascending));
        }
        items[0].bytes = items[1].bytes;
        options.sort(&mut items);
        assert_eq!(
            items[0].root.key, "a",
            "Ties stay stable in descending order"
        );
        assert_eq!((options.created_days, options.updated_days), (0, 0));
    }
    #[test]
    fn independent_date_preferences_migrate_without_extra_restrictions() {
        for (saved, expected) in [
            (serde_json::json!({}), (0, 30)),
            (serde_json::json!({"time":"Created", "days":90}), (90, 0)),
            (serde_json::json!({"time":"Updated", "days":180}), (0, 180)),
            (serde_json::json!({"time":"Created"}), (30, 0)),
            (serde_json::json!({"time":"Created", "days":0}), (0, 0)),
            (serde_json::json!({"days":0}), (0, 0)),
            (
                serde_json::json!({"created_days":180, "updated_days":30}),
                (180, 30),
            ),
            (serde_json::json!({"created_days":90}), (90, 0)),
            (serde_json::json!({"updated_days":90}), (0, 90)),
            (
                serde_json::json!({"created_days":0, "updated_days":0, "time":"Created", "days":90}),
                (0, 0),
            ),
            (
                serde_json::json!({"created_days":i64::MAX, "updated_days":-1}),
                (0, 30),
            ),
        ] {
            let mut saved = saved;
            saved["sort"] = serde_json::json!("Created");
            saved["ascending"] = serde_json::json!(false);
            saved["project"] = serde_json::json!("/synthetic/project");
            saved["exclude_starred"] = serde_json::json!(true);
            saved["exclude_pinned"] = serde_json::json!(true);
            let options: CleanupOptions = serde_json::from_value(saved).unwrap();
            assert_eq!((options.created_days, options.updated_days), expected);
            assert_eq!(options.sort, CleanupSort::Created);
            assert_eq!(options.ascending, Some(false));
            assert_eq!(
                options.projects,
                BTreeSet::from(["/synthetic/project".into()])
            );
            assert!(options.exclude_starred && options.exclude_pinned);
            let encoded = serde_json::to_value(&options).unwrap();
            assert!(encoded.get("time").is_none() && encoded.get("days").is_none());
            let restored: CleanupOptions = serde_json::from_value(encoded).unwrap();
            assert_eq!((restored.created_days, restored.updated_days), expected);
            assert_eq!(restored.ascending, Some(false));
            assert!(restored.exclude_starred && restored.exclude_pinned);
        }
    }

    #[test]
    fn source_filters_union_each_group_and_intersect_with_other_filters() {
        let mut options = CleanupOptions {
            agents: BTreeSet::from([AgentId::ClaudeCode, AgentId::Codex]),
            projects: BTreeSet::from(["/synthetic/a".into(), "/synthetic/b".into()]),
            ..CleanupOptions::default()
        };
        for agent in [AgentId::ClaudeCode, AgentId::Codex, AgentId::Cursor] {
            for project in ["/synthetic/a", "/synthetic/b", "/synthetic/c"] {
                let mut session = meta(Path::new("/synthetic/sessions"), "source-filter", 100);
                session.agent = agent;
                session.project_path = project.into();
                let matches = options.matches_metadata(
                    &session,
                    session.updated_at,
                    std::slice::from_ref(&session),
                    now(),
                );
                assert_eq!(
                    matches,
                    agent != AgentId::Cursor && project != "/synthetic/c"
                );
            }
        }
        let mut session = meta(Path::new("/synthetic/sessions"), "source-filter", 1);
        session.project_path = "/synthetic/a".into();
        assert!(
            !options.matches_metadata(
                &session,
                session.updated_at,
                std::slice::from_ref(&session),
                now()
            ),
            "Date filters still apply to selected sources"
        );
        options.updated_days = 0;
        assert!(options.matches_metadata(
            &session,
            session.updated_at,
            std::slice::from_ref(&session),
            now()
        ));
        options.agents.clear();
        session.agent = AgentId::Cursor;
        assert!(
            options.matches_metadata(
                &session,
                session.updated_at,
                std::slice::from_ref(&session),
                now()
            ),
            "All agents removes only the agent restriction"
        );
        session.project_path = "/synthetic/c".into();
        assert!(!options.matches_metadata(
            &session,
            session.updated_at,
            std::slice::from_ref(&session),
            now()
        ));
        options.projects.clear();
        assert!(
            options.matches_metadata(
                &session,
                session.updated_at,
                std::slice::from_ref(&session),
                now()
            ),
            "Empty source sets include every source"
        );
    }

    #[test]
    fn source_preferences_migrate_and_preserve_explicit_clears() {
        let old: CleanupOptions = serde_json::from_value(serde_json::json!({
            "agent": "codex", "project": "/synthetic/a"
        }))
        .unwrap();
        assert_eq!(old.agents, BTreeSet::from([AgentId::Codex]));
        assert_eq!(old.projects, BTreeSet::from(["/synthetic/a".into()]));
        let selected: CleanupOptions = serde_json::from_value(serde_json::json!({
            "agents": ["claude-code", "codex", "codex"],
            "projects": ["/synthetic/a", "/synthetic/b"],
            "agent": "cursor", "project": "/synthetic/ignored"
        }))
        .unwrap();
        assert_eq!(
            selected.agents,
            BTreeSet::from([AgentId::ClaudeCode, AgentId::Codex])
        );
        let saved = serde_json::to_value(&selected).unwrap();
        assert!(saved.get("agent").is_none() && saved.get("project").is_none());
        let restored: CleanupOptions = serde_json::from_value(saved).unwrap();
        assert_eq!(restored.agents, selected.agents);
        assert_eq!(restored.projects, selected.projects);
        let cleared: CleanupOptions = serde_json::from_value(serde_json::json!({
            "agents": [], "projects": [], "agent": "codex", "project": "/synthetic/a"
        }))
        .unwrap();
        assert!(cleared.agents.is_empty() && cleared.projects.is_empty());
    }

    #[test]
    fn retired_content_preferences_do_not_hide_candidates() {
        let (_temp, store, adapters, root) = setup();
        insert(&store, &root, "small-session", 100);
        let mut candidate = inventory(&store, &adapters).unwrap().candidates.remove(0);
        candidate.bytes = 1024;
        candidate.prompts = 5;
        candidate.empty = false;
        assert!(CleanupOptions::default().matches(&candidate, now()));
        for preset in ["All", "Large", "Short", "Empty"] {
            let options: CleanupOptions = serde_json::from_value(serde_json::json!({
                "content": preset,
                "days": 90,
                "sort": "Created",
                "ascending": true
            }))
            .unwrap();
            assert!(options.matches(&candidate, now()));
            assert_eq!((options.created_days, options.updated_days), (0, 90));
            assert_eq!(options.sort, CleanupSort::Created);
            assert!(options.sort_ascending());
            assert!(serde_json::to_value(&options)
                .unwrap()
                .get("content")
                .is_none());
        }
    }
    #[test]
    fn sort_direction_preferences_preserve_old_order_and_roundtrip() {
        for (key, old_ascending) in [("Created", true), ("Updated", true), ("Size", false)] {
            let mut options: CleanupOptions =
                serde_json::from_value(serde_json::json!({ "sort": key })).unwrap();
            assert_eq!(options.sort_ascending(), old_ascending);
            options.ascending = Some(!old_ascending);
            let saved = serde_json::to_string(&options).unwrap();
            let restored: CleanupOptions = serde_json::from_str(&saved).unwrap();
            assert_eq!(restored.sort_ascending(), !old_ascending);
            assert_eq!(restored.sort, options.sort);
        }
    }
    #[test]
    fn changed_files_new_children_and_favorites_invalidate_review() {
        let (_temp, store, adapters, root) = setup();
        let m = insert(&store, &root, "a", 100);
        let c = inventory(&store, &adapters).unwrap().candidates.remove(0);
        assert!(revalidate(&store, &adapters, &c).is_ok());
        store.set_user_data(&m.key, Some(true), None).unwrap();
        assert!(revalidate(&store, &adapters, &c).is_err());
        store.set_user_data(&m.key, Some(false), None).unwrap();
        let child = insert(&store, &root, "child", 100);
        store
            .replace_parent_links(AgentId::ClaudeCode, &[(child.key, m.key.clone())])
            .unwrap();
        assert!(revalidate(&store, &adapters, &c).is_err());
        store
            .replace_parent_links(AgentId::ClaudeCode, &[])
            .unwrap();
        fs::write(&m.file_path, "changed").unwrap();
        assert!(revalidate(&store, &adapters, &c).is_err());
    }
    #[test]
    fn nested_paths_deduplicate_and_shared_session_prevents_cleanup() {
        let (_temp, store, adapters, root) = setup();
        let parent = insert(&store, &root, "a", 100);
        fs::create_dir(root.join("a")).unwrap();
        fs::write(root.join("a/sidecar"), "sidecar").unwrap();
        age(&root.join("a/sidecar"));
        age(&root.join("a"));
        let c = inventory(&store, &adapters).unwrap().candidates.remove(0);
        assert_eq!(c.targets.len(), 2);
        assert_eq!(
            c.targets
                .iter()
                .flat_map(|t| &t.files)
                .filter(|f| !f.directory)
                .count(),
            2
        );
        let mut child = insert(&store, &root, "child", 100);
        let original = child.file_path.clone();
        child.file_path = root.join("a/child.jsonl").to_string_lossy().into_owned();
        fs::rename(original, &child.file_path).unwrap();
        store.remove_session(&child.key, false).unwrap();
        store.write_session(&child, child.updated_at, &[]).unwrap();
        age(&root.join("a"));
        assert!(inventory(&store, &adapters)
            .unwrap()
            .candidates
            .iter()
            .all(|c| c.root.key != parent.key));
        store
            .replace_parent_links(AgentId::ClaudeCode, &[(child.key, parent.key)])
            .unwrap();
        let c = inventory(&store, &adapters).unwrap().candidates.remove(0);
        assert_eq!(c.targets.len(), 2);
        assert_eq!(c.sessions.len(), 2);
    }
    #[test]
    fn review_reports_every_failed_tree_and_preserves_ready_selection() {
        let (_temp, store, adapters, root) = setup();
        let good = insert(&store, &root, "good", 100);
        let bad = insert(&store, &root, "bad", 100);
        let changed = insert(&store, &root, "changed", 100);
        fs::write(&bad.file_path, "BAD").unwrap();
        let mut chosen = inventory(&store, &adapters).unwrap().candidates;
        // A failure in the first item must not prevent checking later trees.
        chosen.sort_by_key(|c| c.root.id.clone());
        fs::write(&changed.file_path, "new content after selection").unwrap();
        let mut progress = vec![];
        let checked = review(&store, &adapters, chosen, &AtomicBool::new(false), |n| {
            progress.push(n);
        })
        .unwrap();
        assert_eq!(progress, vec![1, 2, 3]);
        assert_eq!(checked.ready.len(), 1);
        assert_eq!(checked.ready[0].root.key, good.key);
        assert_eq!(checked.skipped.len(), 2);
        assert_eq!(checked.skipped[0].session.key, bad.key);
        assert_eq!(
            checked.skipped[0].reason,
            "Session has unrecognized content"
        );
        assert_eq!(checked.skipped[1].session.key, changed.key);
        assert!(checked.skipped[1].reason.contains("changed since review"));
        for meta in [&good, &bad, &changed] {
            assert!(Path::new(&meta.file_path).exists());
            assert!(store.get_session(&meta.key).unwrap().is_some());
            assert!(!store.is_key_tombstoned(&meta.key));
        }
        assert!(
            history(&store).unwrap().is_empty(),
            "Review never starts a cleanup batch"
        );
        fs::write(&good.file_path, "changed again").unwrap();
        let all_failed = review(
            &store,
            &adapters,
            checked.ready,
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap();
        assert!(all_failed.ready.is_empty());
        assert_eq!(all_failed.skipped.len(), 1);
    }

    #[test]
    fn cancelled_review_discards_partial_results_without_moving_files() {
        let (_temp, store, adapters, root) = setup();
        let first = insert(&store, &root, "first", 100);
        let second = insert(&store, &root, "second", 100);
        let chosen = inventory(&store, &adapters).unwrap().candidates;
        let cancel = AtomicBool::new(true);
        assert!(review(&store, &adapters, chosen.clone(), &cancel, |_| {
            panic!("A cancelled review must not start checking");
        })
        .is_none());
        for stop_after in [1, 2] {
            cancel.store(false, Ordering::Relaxed);
            let mut checked = 0;
            assert!(review(&store, &adapters, chosen.clone(), &cancel, |n| {
                checked = n;
                if n == stop_after {
                    cancel.store(true, Ordering::Relaxed);
                }
            })
            .is_none());
            assert_eq!(checked, stop_after);
        }
        for meta in [first, second] {
            assert!(Path::new(&meta.file_path).exists());
            assert!(store.get_session(&meta.key).unwrap().is_some());
            assert!(!store.is_key_tombstoned(&meta.key));
        }
        assert!(history(&store).unwrap().is_empty());
    }

    #[test]
    fn unsupported_remote_missing_and_bad_content_are_not_moved() {
        let (_temp, store, adapters, root) = setup();
        let mut m = insert(&store, &root, "a", 100);
        let unsupported: Vec<Box<dyn AgentAdapter>> = vec![Box::new(FilesAdapter {
            root: root.clone(),
            supported: false,
        })];
        assert!(inventory(&store, &unsupported)
            .unwrap()
            .candidates
            .is_empty());
        m.host = "remote".into();
        store.write_session(&m, m.updated_at, &[]).unwrap();
        assert!(inventory(&store, &adapters).unwrap().candidates.is_empty());
        m.host.clear();
        store.write_session(&m, m.updated_at, &[]).unwrap();
        fs::write(&m.file_path, "BAD").unwrap();
        age(Path::new(&m.file_path));
        let c = inventory(&store, &adapters).unwrap().candidates.remove(0);
        let mut batch = CleanupBatch::new(vec![c], now());
        execute_with(
            &store,
            &adapters,
            &mut batch,
            &AtomicBool::new(false),
            |_| {},
            |_| panic!("must not move unrecognized content"),
        )
        .unwrap();
        assert!(batch.records[0].error.is_some());
        assert!(Path::new(&m.file_path).exists());
        fs::remove_file(&m.file_path).unwrap();
        assert!(inventory(&store, &adapters).unwrap().candidates.is_empty());
    }
    #[test]
    fn hard_links_are_excluded() {
        let (_temp, store, adapters, root) = setup();
        let m = insert(&store, &root, "a", 100);
        fs::hard_link(&m.file_path, root.join("other")).unwrap();
        assert!(inventory(&store, &adapters).unwrap().candidates.is_empty());
        fs::remove_file(root.join("other")).unwrap();
        assert_eq!(inventory(&store, &adapters).unwrap().candidates.len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn symbolic_links_are_excluded() {
        let (_temp, store, adapters, root) = setup();
        let m = insert(&store, &root, "a", 100);
        fs::rename(&m.file_path, root.join("actual")).unwrap();
        std::os::unix::fs::symlink(root.join("actual"), &m.file_path).unwrap();
        assert!(inventory(&store, &adapters).unwrap().candidates.is_empty());
    }

    #[test]
    fn canonical_target_accepts_regular_paths_and_rejects_changed_locations() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let path = root.join("session.jsonl");
        fs::write(&path, "synthetic").unwrap();
        assert!(target(&path).is_ok());
        assert!(same_canonical_path(&path, &path.canonicalize().unwrap()));
        assert!(!same_canonical_path(&root.join("other.jsonl"), &path));
        assert!(target(Path::new("relative-session.jsonl")).is_err());
    }

    #[test]
    fn same_size_and_timestamp_replacement_is_rejected_before_deletion() {
        let (_temp, store, adapters, root) = setup();
        let m = insert(&store, &root, "a", 100);
        let candidate = inventory(&store, &adapters).unwrap().candidates.remove(0);
        let original = stamp(Path::new(&m.file_path)).unwrap();
        let replacement = root.join("replacement");
        fs::write(&replacement, "evil").unwrap();
        File::options()
            .write(true)
            .open(&replacement)
            .unwrap()
            .set_times(
                FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_nanos(original.modified)),
            )
            .unwrap();
        fs::rename(&m.file_path, root.join("old-file")).unwrap();
        fs::rename(replacement, &m.file_path).unwrap();
        let replaced = stamp(Path::new(&m.file_path)).unwrap();
        assert_eq!(original.logical_bytes, replaced.logical_bytes);
        assert_eq!(original.modified, replaced.modified);
        assert_ne!(original.identity, replaced.identity);
        assert!(revalidate(&store, &adapters, &candidate).is_err());
        let mut batch = CleanupBatch::new(vec![candidate], now());
        execute_with(
            &store,
            &adapters,
            &mut batch,
            &AtomicBool::new(false),
            |_| {},
            |_| panic!("A replaced file must never be moved"),
        )
        .unwrap();
        assert!(!store.is_key_tombstoned(&m.key));
        assert!(batch.records[0].error.is_some());
    }

    #[cfg(unix)]
    #[test]
    fn symbolic_link_in_target_ancestor_is_excluded() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        fs::create_dir(root.join("actual")).unwrap();
        fs::write(root.join("actual/session.jsonl"), "synthetic").unwrap();
        std::os::unix::fs::symlink(root.join("actual"), root.join("alias")).unwrap();
        assert!(target(&root.join("alias/session.jsonl")).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn windows_canonical_prefixes_do_not_hide_regular_sessions() {
        use std::path::{Component, Prefix};
        let temp = tempfile::tempdir().unwrap();
        let canonical = temp.path().canonicalize().unwrap();
        let mut components = canonical.components();
        let mut root = match components.next().unwrap() {
            Component::Prefix(prefix) => match prefix.kind() {
                Prefix::VerbatimDisk(drive) => PathBuf::from(format!("{}:", drive as char)),
                _ => panic!("Test expects a local temporary directory"),
            },
            _ => panic!("Test expects an absolute temporary directory"),
        };
        for component in components {
            root.push(component.as_os_str());
        }
        root.push("sessions");
        fs::create_dir(&root).unwrap();
        let store = Store::open(&temp.path().join("index.db")).unwrap();
        let adapters: Vec<Box<dyn AgentAdapter>> = vec![Box::new(FilesAdapter {
            root: root.clone(),
            supported: true,
        })];
        insert(&store, &root, "ordinary-path", 100);
        let inv = inventory(&store, &adapters).unwrap();
        assert_eq!(inv.candidates.len(), 1);
        revalidate(&store, &adapters, &inv.candidates[0]).unwrap();

        for (ordinary, extended) in [
            (r"C:\sessions\a.jsonl", r"\\?\C:\sessions\a.jsonl"),
            (r"\\server\share\a.jsonl", r"\\?\UNC\server\share\a.jsonl"),
        ] {
            assert!(same_canonical_path(
                Path::new(ordinary),
                Path::new(extended)
            ));
        }
        for different in [r"\\?\D:\sessions\a.jsonl", r"\\?\C:\elsewhere\a.jsonl"] {
            assert!(!same_canonical_path(
                Path::new(r"C:\sessions\a.jsonl"),
                Path::new(different)
            ));
        }
    }

    #[test]
    fn revalidation_does_not_depend_on_the_message_index() {
        let (temp, store, adapters, root) = setup();
        let m = insert(&store, &root, "a", 100);
        let c = inventory(&store, &adapters).unwrap().candidates.remove(0);
        assert_eq!(c.prompts, 1);
        // Any accidental transcript-count query now fails deterministically,
        // without a timing assertion or depending on a contributor's database.
        rusqlite::Connection::open(temp.path().join("index.db"))
            .unwrap()
            .execute_batch("DROP TABLE messages")
            .unwrap();
        revalidate(&store, &adapters, &c).unwrap();
        store.set_user_data(&m.key, None, Some(true)).unwrap();
        assert!(revalidate(&store, &adapters, &c).is_err());
    }

    #[test]
    fn ownership_is_checked_again_after_parsing() {
        use std::sync::Arc;
        struct IndexOwnerDuringParse {
            inner: FilesAdapter,
            store: Arc<Store>,
            owner: SessionMeta,
        }
        impl AgentAdapter for IndexOwnerDuringParse {
            fn agent(&self) -> AgentId {
                self.inner.agent()
            }
            fn data_roots(&self) -> Vec<PathBuf> {
                self.inner.data_roots()
            }
            fn with_custom_root(&self, root: PathBuf) -> Box<dyn AgentAdapter> {
                self.inner.with_custom_root(root)
            }
            fn list_session_files(&self) -> Result<Vec<SessionFileRef>> {
                self.inner.list_session_files()
            }
            fn parse_session(&self, r: &SessionFileRef) -> Result<ParsedSession> {
                let parsed = self.inner.parse_session(r)?;
                // Simulate a scanner discovering an owner while validation parses
                // the original tree. The filesystem snapshot does not change.
                self.store
                    .write_session(&self.owner, self.owner.updated_at, &[])?;
                Ok(parsed)
            }
            fn parse_transcript(&self, r: &SessionFileRef) -> Result<ParsedTranscript> {
                self.inner.parse_transcript(r)
            }
            fn session_paths(&self, m: &SessionMeta) -> Vec<String> {
                self.inner.session_paths(m)
            }
            fn cleanup_paths(&self, m: &SessionMeta) -> Option<Vec<String>> {
                self.inner.cleanup_paths(m)
            }
        }
        let (_temp, store, adapters, root) = setup();
        let m = insert(&store, &root, "a", 100);
        fs::create_dir(root.join("a")).unwrap();
        let sidecar = root.join("a/sidecar.jsonl");
        fs::write(&sidecar, "test").unwrap();
        let c = inventory(&store, &adapters).unwrap().candidates.remove(0);
        let mut owner = meta(&root, "new-owner", 100);
        owner.file_path = sidecar.to_string_lossy().into_owned();
        let store = Arc::new(store);
        let adapters: Vec<Box<dyn AgentAdapter>> = vec![Box::new(IndexOwnerDuringParse {
            inner: FilesAdapter {
                root,
                supported: true,
            },
            store: store.clone(),
            owner,
        })];
        let error = revalidate(&store, &adapters, &c).unwrap_err();
        assert_eq!(error.to_string(), "Target also contains another session");
        assert!(Path::new(&m.file_path).exists());
        assert!(!store.is_key_tombstoned(&m.key));
    }
    #[test]
    fn partial_failure_is_journaled_and_restored_files_can_be_reindexed() {
        let (temp, store, adapters, root) = setup();
        let m = insert(&store, &root, "a", 100);
        fs::create_dir(root.join("a")).unwrap();
        fs::write(root.join("a/sidecar"), "side").unwrap();
        age(&root.join("a/sidecar"));
        age(&root.join("a"));
        let c = inventory(&store, &adapters).unwrap().candidates.remove(0);
        let moved_path = c.targets[0].path.clone();
        let dest = temp.path().join("fake-trash");
        let calls = Cell::new(0);
        let mut batch = CleanupBatch::new(vec![c], now());
        execute_with(
            &store,
            &adapters,
            &mut batch,
            &AtomicBool::new(false),
            |_| {},
            |p| {
                calls.set(calls.get() + 1);
                if calls.get() == 2 {
                    bail!("fake permission failure");
                }
                fs::rename(p, &dest)?;
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(batch.records[0].targets[0], TargetStatus::Moved);
        assert!(matches!(
            batch.records[0].targets[1],
            TargetStatus::Failed(_)
        ));
        assert!(!batch.records[0].indexed);
        assert!(!store.is_key_tombstoned(&m.key));
        assert_eq!(history(&store).unwrap().len(), 1);
        fs::rename(dest, moved_path).unwrap();
        assert_eq!(restore(&store, &adapters, &mut batch).unwrap(), 1);
        assert!(batch.records[0].restored);
        store.rebuild_all().unwrap();
        assert_eq!(history(&store).unwrap().len(), 1);
    }
    #[test]
    fn watcher_race_still_writes_tombstones_and_restore_respects_later_deletion() {
        let (temp, store, adapters, root) = setup();
        let m = insert(&store, &root, "a", 100);
        let c = inventory(&store, &adapters).unwrap().candidates.remove(0);
        let dest = temp.path().join("fake-trash");
        let mut batch = CleanupBatch::new(vec![c], now());
        execute_with(
            &store,
            &adapters,
            &mut batch,
            &AtomicBool::new(false),
            |_| {},
            |p| {
                fs::rename(p, &dest)?;
                store.remove_session(&m.key, false)?;
                Ok(())
            },
        )
        .unwrap();
        assert!(batch.records[0].indexed);
        assert!(store.is_key_tombstoned(&m.key));
        assert!(store.is_tombstoned(&m.file_path));
        assert!(restore(&store, &adapters, &mut batch).is_err());
        fs::rename(dest, &m.file_path).unwrap();
        store
            .complete_cleanup(std::slice::from_ref(&m), batch.stamp + 1)
            .unwrap();
        assert!(restore(&store, &adapters, &mut batch).is_err());
        assert!(store.is_key_tombstoned(&m.key));
    }
    #[test]
    fn cancellation_stops_before_next_tree_and_never_claims_it_moved() {
        let (temp, store, adapters, root) = setup();
        insert(&store, &root, "a", 100);
        insert(&store, &root, "b", 100);
        let items = inventory(&store, &adapters).unwrap().candidates;
        let mut batch = CleanupBatch::new(items, now());
        let cancel = AtomicBool::new(false);
        execute_with(
            &store,
            &adapters,
            &mut batch,
            &cancel,
            |_| cancel.store(true, Ordering::Relaxed),
            |p| {
                fs::rename(p, temp.path().join(p.file_name().unwrap()))?;
                Ok(())
            },
        )
        .unwrap();
        assert!(batch.records[0].indexed);
        assert!(!batch.records[1].indexed);
        assert_eq!(batch.records[1].targets, vec![TargetStatus::Pending]);
    }

    #[test]
    fn index_conflict_can_be_retried_and_complete_restore_clears_only_its_tombstone() {
        let (temp, store, adapters, root) = setup();
        let m = insert(&store, &root, "a", 100);
        let item = inventory(&store, &adapters).unwrap().candidates.remove(0);
        let dest = temp.path().join("fake-trash");
        let mut batch = CleanupBatch::new(vec![item], now());
        execute_with(
            &store,
            &adapters,
            &mut batch,
            &AtomicBool::new(false),
            |_| {},
            |p| {
                fs::rename(p, &dest)?;
                store.set_user_data(&m.key, Some(true), None)?;
                Ok(())
            },
        )
        .unwrap();
        assert!(!batch.records[0].indexed);
        assert_eq!(batch.records[0].targets, vec![TargetStatus::Moved]);
        assert_eq!(retry_index_updates(&store, &mut batch).unwrap(), 0);
        assert!(store.get_session(&m.key).unwrap().is_some());
        store.set_user_data(&m.key, Some(false), None).unwrap();
        assert_eq!(retry_index_updates(&store, &mut batch).unwrap(), 1);
        assert!(store.is_key_tombstoned(&m.key));
        fs::rename(dest, &m.file_path).unwrap();
        assert_eq!(restore(&store, &adapters, &mut batch).unwrap(), 1);
        assert!(!store.is_key_tombstoned(&m.key));
        assert!(!store.is_tombstoned(&m.file_path));
        assert!(history(&store).unwrap()[0].records[0].restored);
    }

    #[test]
    fn manual_restore_check_never_moves_files_and_requires_complete_sidecars() {
        let (temp, store, adapters, root) = setup();
        let m = insert(&store, &root, "a", 100);
        fs::create_dir(root.join("a")).unwrap();
        fs::write(root.join("a/sidecar"), "sidecar").unwrap();
        let trash = temp.path().join("fake-trash");
        fs::create_dir(&trash).unwrap();
        let mut batch = CleanupBatch::new(inventory(&store, &adapters).unwrap().candidates, now());
        execute_with(
            &store,
            &adapters,
            &mut batch,
            &AtomicBool::new(false),
            |_| {},
            |path| {
                fs::rename(path, trash.join(path.file_name().unwrap()))?;
                Ok(())
            },
        )
        .unwrap();
        assert!(restore(&store, &adapters, &mut batch).is_err());
        assert!(trash.join("a.jsonl").exists() && trash.join("a/sidecar").exists());
        assert!(!Path::new(&m.file_path).exists());
        assert!(store.is_key_tombstoned(&m.key));
        // Simulate the user putting back only the main transcript.
        fs::rename(trash.join("a.jsonl"), &m.file_path).unwrap();
        assert!(restore(&store, &adapters, &mut batch).is_err());
        assert!(trash.join("a/sidecar").exists());
        assert!(store.is_key_tombstoned(&m.key));
        // Only a complete manual restoration clears this batch's tombstones.
        fs::create_dir(root.join("a")).unwrap();
        fs::create_dir(root.join("a/sidecar")).unwrap();
        assert!(restore(&store, &adapters, &mut batch).is_err());
        assert!(store.is_key_tombstoned(&m.key));
        fs::remove_dir(root.join("a/sidecar")).unwrap();
        // Equal length and timestamp still must not pass with different content.
        fs::copy(trash.join("a/sidecar"), root.join("a/sidecar")).unwrap();
        fs::write(root.join("a/sidecar"), "corrupt").unwrap();
        let modified = fs::metadata(trash.join("a/sidecar"))
            .unwrap()
            .modified()
            .unwrap();
        File::options()
            .write(true)
            .open(root.join("a/sidecar"))
            .unwrap()
            .set_times(FileTimes::new().set_modified(modified))
            .unwrap();
        assert!(restore(&store, &adapters, &mut batch).is_err());
        assert!(!batch.records[0].restored);
        assert!(store.is_key_tombstoned(&m.key));
        fs::remove_file(root.join("a/sidecar")).unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(trash.join("a/sidecar"), root.join("a/sidecar")).unwrap();
            assert!(restore(&store, &adapters, &mut batch).is_err());
            fs::remove_file(root.join("a/sidecar")).unwrap();
        }
        fs::remove_dir(root.join("a")).unwrap();
        fs::rename(trash.join("a"), root.join("a")).unwrap();
        assert_eq!(restore(&store, &adapters, &mut batch).unwrap(), 1);
        assert!(!store.is_key_tombstoned(&m.key));
        assert!(history(&store).unwrap()[0].records[0].restored);
    }

    #[test]
    fn restored_copies_can_have_new_file_identities() {
        let (temp, store, adapters, root) = setup();
        let m = insert(&store, &root, "a", 100);
        let trash = temp.path().join("fake-trash");
        let mut batch = CleanupBatch::new(inventory(&store, &adapters).unwrap().candidates, now());
        execute_with(
            &store,
            &adapters,
            &mut batch,
            &AtomicBool::new(false),
            |_| {},
            |p| {
                fs::rename(p, &trash)?;
                Ok(())
            },
        )
        .unwrap();
        // Reload the journal, proving content evidence was saved before moving.
        batch = history(&store).unwrap().remove(0);
        assert_eq!(batch.records[0].content_hashes.len(), 1);
        fs::copy(&trash, &m.file_path).unwrap();
        assert_ne!(
            stamp(&trash).unwrap().identity,
            stamp(Path::new(&m.file_path)).unwrap().identity
        );
        assert_eq!(restore(&store, &adapters, &mut batch).unwrap(), 1);
        assert!(!store.is_key_tombstoned(&m.key));
    }

    #[test]
    fn legacy_journals_require_matching_type_length_and_timestamp() {
        let (_temp, store, adapters, root) = setup();
        let m = insert(&store, &root, "a", 100);
        let batch = CleanupBatch::new(inventory(&store, &adapters).unwrap().candidates, now());
        let mut saved = serde_json::to_value(batch).unwrap();
        saved["records"][0]
            .as_object_mut()
            .unwrap()
            .remove("content_hashes");
        let mut batch: CleanupBatch = serde_json::from_value(saved).unwrap();
        batch.records[0].targets.fill(TargetStatus::Moved);
        store
            .complete_cleanup(std::slice::from_ref(&m), batch.stamp)
            .unwrap();
        let modified = fs::metadata(&m.file_path).unwrap().modified().unwrap();
        fs::write(&m.file_path, "truncated").unwrap();
        assert!(restore(&store, &adapters, &mut batch).is_err());
        fs::write(&m.file_path, "test").unwrap();
        assert!(restore(&store, &adapters, &mut batch).is_err());
        File::options()
            .write(true)
            .open(&m.file_path)
            .unwrap()
            .set_times(FileTimes::new().set_modified(modified))
            .unwrap();
        assert_eq!(restore(&store, &adapters, &mut batch).unwrap(), 1);
    }

    #[cfg(any(target_os = "linux", windows))]
    #[test]
    #[ignore = "Uses the system trash for synthetic temporary files; run explicitly in CI"]
    fn system_trash_cleanup_round_trip() {
        let (_temp, store, adapters, root) = setup();
        let m = insert(&store, &root, "会话 with spaces", 100);
        fs::create_dir(root.join(&m.id)).unwrap();
        fs::write(root.join(&m.id).join("sidecar.jsonl"), "synthetic sidecar").unwrap();
        let mut batch = CleanupBatch::new(inventory(&store, &adapters).unwrap().candidates, now());
        assert_eq!(batch.records.len(), 1);
        execute(
            &store,
            &adapters,
            &mut batch,
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap();
        assert!(batch.records[0].indexed, "{:?}", batch.records[0].error);
        assert!(store.is_key_tombstoned(&m.key));
        assert!(!Path::new(&m.file_path).exists());
        // Only restore the uniquely named temp tree created by this test. Never
        // restore or purge unrelated entries in the user's/runner's recycle bin.
        let paths = &batch.records[0].candidate.targets;
        let items: Vec<_> = trash::os_limited::list()
            .unwrap()
            .into_iter()
            .filter(|item| {
                paths
                    .iter()
                    .any(|t| same_canonical_path(&item.original_path(), &t.path))
            })
            .collect();
        assert_eq!(items.len(), paths.len());
        trash::os_limited::restore_all(items).unwrap();
        assert_eq!(restore(&store, &adapters, &mut batch).unwrap(), 1);
        assert_eq!(
            fs::read_to_string(root.join(&m.id).join("sidecar.jsonl")).unwrap(),
            "synthetic sidecar"
        );
        assert!(!store.is_key_tombstoned(&m.key));
    }
}
