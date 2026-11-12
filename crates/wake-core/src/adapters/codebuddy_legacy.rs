//! Read the CodeBuddy IDE 4.x transcripts imported into the 5.x catalog.
//! Catalog membership and visibility are authoritative; source files are read-only.

use super::parse_utils::*;
use super::AgentAdapter;
use crate::models::*;
use anyhow::{anyhow, Context, Result};
use md5::{Digest, Md5};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

/// Synchronization swaps a whole remote generation while scans and detail reads
/// use the same path. Protect the brief swap from concurrent in-process reads.
pub(crate) static REMOTE_CACHE_SWITCH: RwLock<()> = RwLock::new(());

const PROVIDER: &str = "codebuddy-ide-4";
const INDEX: &str = "index.json";
pub const LEGACY_SOURCE: &str = "CodeBuddy IDE 4.x";
pub(crate) const REMOTE_DATA_ROOT: &str = ".local/share/CodeBuddyExtension/Data";
// Matches the source recorder's per-message bound and limits malformed inputs.
const MAX_HISTORY_MESSAGE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_HISTORY_INDEX_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Clone, Debug, Hash)]
pub(crate) struct CatalogRow {
    pub id: String,
    pub user_id: String,
    pub workspace: String,
    pub authority: String,
    pub title: String,
    pub custom_title: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub deleted: bool,
    pub archived: bool,
    pub provider: String,
    pub runtime_id: String,
    pub revision: i64,
}

pub(crate) fn catalog_path() -> PathBuf {
    let default = super::home_dir().unwrap_or_default().join(".codebuddy");
    super::env_dir("CODEBUDDY_CONFIG_DIR")
        .filter(|p| p.join("codebuddy.db").is_file())
        .unwrap_or(default)
        .join("codebuddy.db")
}

/// Include WAL size and sub-millisecond timestamps. A catalog transaction can
/// change only the WAL, and several transactions can land within one ms.
pub(crate) fn catalog_stamp(path: &Path) -> u64 {
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    super::sqlite_ro::db_cache_stamp(path).hash(&mut hash);
    let mut wal = path.as_os_str().to_owned();
    wal.push("-wal");
    for part in [path, Path::new(&wal)] {
        match fs::metadata(part) {
            Ok(meta) => {
                meta.len().hash(&mut hash);
                meta.modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_nanos())
                    .hash(&mut hash);
            }
            Err(_) => 0_u8.hash(&mut hash),
        }
    }
    hash.finish()
}

/// Do not copy the catalog into remote caches: other tables may contain account
/// state. A readonly SQLite connection sees committed WAL entries.
pub(crate) fn read_catalog(path: &Path) -> Result<Vec<CatalogRow>> {
    match fs::metadata(path) {
        Ok(meta) if meta.is_file() => {}
        Ok(_) => anyhow::bail!("CodeBuddy catalog is not a file"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).context("inspect CodeBuddy catalog"),
    }
    // Do not use the generic copy fallback: this database can contain other
    // account data and must never be copied to a temporary directory.
    let db =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .context("open CodeBuddy catalog read-only")?;
    if !super::sqlite_ro::has_table(&db, "conversations")? {
        // CLI-only installations can have a database with no IDE catalog.
        // Ordinary CBC transcripts must remain discoverable in that case.
        return Ok(Vec::new());
    }
    let mut stmt = db.prepare(
        "SELECT id, user_id, workspace, COALESCE(workspace_authority,''),
                COALESCE(title,''), COALESCE(custom_title,''),
                COALESCE(created_at,0), COALESCE(last_message_at,0),
                deleted_at IS NOT NULL, archived_at IS NOT NULL,
                COALESCE(runtime_provider,''), COALESCE(runtime_session_id,''),
                COALESCE(revision,0)
         FROM conversations WHERE runtime_provider IN ('codebuddy-ide-4','cbc')",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(CatalogRow {
            id: r.get(0)?,
            user_id: r.get(1)?,
            workspace: r.get(2)?,
            authority: r.get(3)?,
            title: r.get(4)?,
            custom_title: r.get(5)?,
            created_at: r.get(6)?,
            updated_at: r.get(7)?,
            deleted: r.get(8)?,
            archived: r.get(9)?,
            provider: r.get(10)?,
            runtime_id: r.get(11)?,
            revision: r.get(12)?,
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

/// CodeBuddy encodes bytes in remote SSH authorities as _xHH_. Match the
/// configured SSH alias, not just the session ID: IDs are scoped by host.
pub(crate) fn authority_matches(authority: &str, host: Option<&str>) -> bool {
    let Some(host) = host else {
        return authority.is_empty();
    };
    let Some(encoded) = authority
        .strip_prefix("codebuddy-remote-ssh+")
        .or_else(|| authority.strip_prefix("ssh-remote+"))
    else {
        return false;
    };
    let bytes = encoded.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if i + 4 < bytes.len() && bytes[i] == b'_' && bytes[i + 1] == b'x' && bytes[i + 4] == b'_' {
            if let Ok(hex) = std::str::from_utf8(&bytes[i + 2..i + 4]) {
                if let Ok(byte) = u8::from_str_radix(hex, 16) {
                    decoded.push(byte);
                    i += 5;
                    continue;
                }
            }
        }
        decoded.push(bytes[i]);
        i += 1;
    }
    std::str::from_utf8(&decoded).is_ok_and(|name| name == host)
}

pub(crate) fn safe_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 200
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}

/// Node path.normalize(URI.file(workspace).fsPath) for absolute POSIX paths.
/// This is lexical and deliberately never resolves symlinks.
pub(crate) fn workspace_hash(workspace: &str) -> Option<String> {
    // A remote Linux workspace must use POSIX rules even when Wake itself is
    // running on Windows. Node preserves a trailing slash during normalize.
    if workspace.starts_with('/') {
        let mut parts = Vec::new();
        for part in workspace.split('/') {
            match part {
                "" | "." => {}
                ".." => {
                    parts.pop();
                }
                other => parts.push(other),
            }
        }
        let mut clean = format!("/{}", parts.join("/"));
        if workspace.ends_with('/') && clean != "/" {
            clean.push('/');
        }
        return Some(format!("{:x}", Md5::digest(clean.as_bytes())));
    }
    let path = Path::new(workspace);
    if !path.is_absolute() {
        return None;
    }
    let mut clean = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                clean.pop();
            }
            Component::CurDir => {}
            other => clean.push(other.as_os_str()),
        }
    }
    let mut clean = clean.to_string_lossy().into_owned();
    if cfg!(windows) && clean.as_bytes().get(1) == Some(&b':') {
        let drive = clean[..1].to_ascii_lowercase();
        clean.replace_range(..1, &drive);
    }
    if workspace.ends_with(std::path::MAIN_SEPARATOR) && !clean.ends_with(std::path::MAIN_SEPARATOR)
    {
        clean.push(std::path::MAIN_SEPARATOR);
    }
    Some(format!("{:x}", Md5::digest(clean.as_bytes())))
}

/// Relative to CodeBuddyExtension/Data. After conversion, the old directory is
/// named by the catalog conversation ID, even if the CBC runtime ID differs.
pub(crate) fn legacy_relative_dir(row: &CatalogRow) -> Option<PathBuf> {
    let id = if row.provider == PROVIDER && !row.runtime_id.is_empty() {
        &row.runtime_id
    } else {
        &row.id
    };
    if !safe_id(&row.user_id) || !safe_id(id) {
        return None;
    }
    Some(
        PathBuf::from(&row.user_id)
            .join("CodeBuddyIDE")
            .join(&row.user_id)
            .join("history")
            .join(workspace_hash(&row.workspace)?)
            .join(id),
    )
}

pub fn is_legacy_history(path: &str) -> bool {
    Path::new(path).file_name().is_some_and(|n| n == INDEX)
}

/// Includes a CBC transcript converted from a 4.x conversation. Those sources
/// remain owned by CodeBuddy even when CBC is the readable winning copy.
pub fn is_imported_legacy(meta: &SessionMeta) -> bool {
    meta.agent == AgentId::Codebuddy
        && (is_legacy_history(&meta.file_path) || meta.source.as_deref() == Some(LEGACY_SOURCE))
}

/// Rsync writes changed files to new inodes; unchanged staged files retain
/// their identities through hard links. This detects replacements even when
/// their timestamps and lengths are equal.
pub(crate) fn hash_file_identity<H: Hasher>(metadata: &fs::Metadata, state: &mut H) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        metadata.dev().hash(state);
        metadata.ino().hash(state);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.creation_time().hash(state);
    }
}

/// Immutable catalog generation with constant-time logical-ID lookup.
pub(crate) struct CatalogSnapshot {
    rows: Vec<CatalogRow>,
    by_id: HashMap<String, usize>,
}
impl std::ops::Deref for CatalogSnapshot {
    type Target = [CatalogRow];
    fn deref(&self) -> &Self::Target {
        &self.rows
    }
}
impl CatalogSnapshot {
    fn get_id(&self, id: &str) -> Option<&CatalogRow> {
        self.by_id.get(id).map(|i| &self.rows[*i])
    }
}
#[derive(Default)]
pub(crate) struct CatalogCache {
    snapshot: Mutex<Option<(u64, Arc<CatalogSnapshot>)>>,
    saw_catalog: std::sync::atomic::AtomicBool,
}
impl CatalogCache {
    pub(crate) fn clear(&self) {
        *self.snapshot.lock().unwrap() = None;
    }
    pub(crate) fn read(
        &self,
        path: &Path,
        protect_missing: bool,
        cached_data: bool,
    ) -> Result<Arc<CatalogSnapshot>> {
        use std::sync::atomic::Ordering;
        let metadata = fs::metadata(path);
        if metadata.as_ref().is_ok_and(|m| m.is_file()) {
            self.saw_catalog.store(true, Ordering::Relaxed);
        }
        if protect_missing
            && (cached_data || self.saw_catalog.load(Ordering::Relaxed))
            && metadata.is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)
        {
            anyhow::bail!("CodeBuddy conversation catalog is temporarily unavailable");
        }
        let stamp = catalog_stamp(path);
        let mut snapshot = self.snapshot.lock().unwrap();
        if let Some((previous, rows)) = snapshot.as_ref() {
            if *previous == stamp {
                return Ok(rows.clone());
            }
        }
        let rows = read_catalog(path)?;
        let by_id = rows
            .iter()
            .enumerate()
            .map(|(i, row)| (row.id.clone(), i))
            .collect();
        let rows = Arc::new(CatalogSnapshot { rows, by_id });
        *snapshot = Some((stamp, rows.clone()));
        Ok(rows)
    }
}

pub struct CodebuddyLegacyAdapter {
    catalog: PathBuf,
    data: PathBuf,
    host: Option<String>,
    cache: CatalogCache,
    protect_missing: bool,
}

impl Default for CodebuddyLegacyAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl CodebuddyLegacyAdapter {
    pub fn new() -> Self {
        Self::at(catalog_path(), default_data_root(), None)
    }

    pub(crate) fn at(catalog: PathBuf, data: PathBuf, host: Option<String>) -> Self {
        Self {
            catalog,
            data,
            host,
            cache: CatalogCache::default(),
            protect_missing: true,
        }
    }

    fn rows(&self) -> Result<Arc<CatalogSnapshot>> {
        // The history directory survives app restarts. A missing catalog is
        // not proof that its imported sessions were deleted; freeze this
        // source while independent CBC files remain free to refresh.
        self.cache
            .read(&self.catalog, self.protect_missing, self.data.is_dir())
    }

    fn owns_row(&self, row: &CatalogRow) -> bool {
        authority_matches(&row.authority, self.host.as_deref())
    }

    fn session_dir(&self, row: &CatalogRow) -> Option<PathBuf> {
        Some(self.data.join(legacy_relative_dir(row)?))
    }

    fn row_for(&self, reference: &SessionFileRef) -> Result<CatalogRow> {
        self.rows()?
            .get_id(&reference.native_id)
            .filter(|row| {
                !row.deleted
                    && self.owns_row(row)
                    && self
                        .session_dir(row)
                        .is_some_and(|d| d.join(INDEX) == Path::new(&reference.file_path))
            })
            .cloned()
            .ok_or_else(|| anyhow!("legacy session is no longer in the catalog"))
    }

    fn reference(&self, row: &CatalogRow) -> Result<Option<SessionFileRef>> {
        let Some(dir) = self.session_dir(row) else {
            return Ok(None);
        };
        // CBC rows without an old directory are ordinary CLI sessions. An
        // IDE-4 row stays listed if its source is temporarily absent, so a
        // previously indexed session is not pruned by a transient failure.
        if row.provider != PROVIDER {
            match fs::metadata(&dir) {
                Ok(meta) if meta.is_dir() => {}
                Ok(_) => return Ok(None),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(error.into()),
            }
        }
        let (fingerprint, bytes) = session_stamp(&dir, row)?;
        Ok(Some(SessionFileRef {
            agent: AgentId::Codebuddy,
            native_id: row.id.clone(),
            file_path: dir.join(INDEX).to_string_lossy().into_owned(),
            mtime_ms: fingerprint,
            size: bytes,
        }))
    }

    fn parse(
        &self,
        r: &SessionFileRef,
        decode_images: bool,
    ) -> Result<(SessionMeta, Vec<TranscriptMessage>, u32)> {
        let _guard = self
            .host
            .as_ref()
            .map(|_| REMOTE_CACHE_SWITCH.read().unwrap());
        let row = self.row_for(r)?;
        let dir = Path::new(&r.file_path)
            .parent()
            .ok_or_else(|| anyhow!("invalid history path"))?;
        let LegacyHistory {
            messages,
            unknown,
            tokens_used: tokens,
            model,
        } = parse_history(dir, decode_images)?;
        let title = [&row.custom_title, &row.title]
            .into_iter()
            .map(|s| clean_title_candidate(s))
            .find(|s| !s.is_empty())
            .or_else(|| title_from_messages(&messages))
            .unwrap_or_else(|| UNTITLED.to_string());
        let (_, bytes) = session_stamp(dir, &row)?;
        let meta = SessionMeta {
            key: session_key(AgentId::Codebuddy, "", &row.id),
            id: row.id.clone(),
            host: String::new(),
            agent: AgentId::Codebuddy,
            title,
            project_path: row.workspace.clone(),
            project_name: project_name_of(&row.workspace),
            file_path: r.file_path.clone(),
            created_at: row.created_at,
            updated_at: row.updated_at,
            message_count: messages
                .iter()
                .filter(|m| m.kind == MessageKind::Text)
                .count() as i64,
            size_bytes: bytes,
            git_branch: None,
            model,
            tokens_used: tokens,
            archived: row.archived,
            source: Some(LEGACY_SOURCE.into()),
            spawned: false,
            favorite: false,
            pinned: false,
        };
        Ok((meta, messages, unknown))
    }
}

pub(crate) fn default_data_root() -> PathBuf {
    let home = super::home_dir().unwrap_or_default();
    if cfg!(target_os = "macos") {
        home.join("Library/Application Support/CodeBuddyExtension/Data")
    } else if cfg!(target_os = "windows") {
        home.join("AppData/Roaming/CodeBuddyExtension/Data")
    } else {
        home.join(REMOTE_DATA_ROOT)
    }
}

impl AgentAdapter for CodebuddyLegacyAdapter {
    fn agent(&self) -> AgentId {
        AgentId::Codebuddy
    }
    fn begin_scan(&self) {
        self.cache.clear();
    }
    fn dedup_rank(&self, _path: &str) -> u8 {
        1
    }
    fn copy_mtime(&self, r: &SessionFileRef) -> i64 {
        fs::metadata(&r.file_path)
            .map(|m| mtime_ms(&m))
            .unwrap_or(0)
    }
    fn scan_priority(&self, r: &SessionFileRef) -> i64 {
        self.row_for(r)
            .map(|row| row.updated_at)
            .unwrap_or_else(|_| self.copy_mtime(r))
    }
    fn native_id_aliases(&self, r: &SessionFileRef) -> Vec<String> {
        match self.row_for(r) {
            Ok(row)
                if row.provider == "cbc"
                    && !row.runtime_id.is_empty()
                    && row.runtime_id != row.id =>
            {
                vec![row.runtime_id]
            }
            _ => Vec::new(),
        }
    }
    fn data_roots(&self) -> Vec<PathBuf> {
        vec![self.data.clone()]
    }
    fn watch_paths(&self) -> Vec<PathBuf> {
        Vec::new()
    }
    fn claims_custom_root(&self, dir: &Path) -> bool {
        dir.file_name()
            .is_some_and(|n| n == "CodeBuddyExtension" || n == "Data")
    }
    fn with_custom_root(&self, dir: PathBuf) -> Box<dyn AgentAdapter> {
        let data = if dir.file_name().is_some_and(|n| n == "CodeBuddyExtension") {
            dir.join("Data")
        } else {
            dir
        };
        let mut custom = Self::at(self.catalog.clone(), data, self.host.clone());
        custom.protect_missing = false;
        Box::new(custom)
    }

    fn with_remote_root(
        &self,
        dir: PathBuf,
        _host_cache: &Path,
        host: &str,
    ) -> Box<dyn AgentAdapter> {
        Box::new(Self::at(self.catalog.clone(), dir, Some(host.to_string())))
    }
    fn list_session_files(&self) -> Result<Vec<SessionFileRef>> {
        let _guard = self
            .host
            .as_ref()
            .map(|_| REMOTE_CACHE_SWITCH.read().unwrap());
        let mut refs = Vec::new();
        for row in self
            .rows()?
            .iter()
            .filter(|row| !row.deleted && self.owns_row(row))
        {
            match self.reference(row) {
                Ok(Some(r)) => refs.push(r),
                Ok(None) => {}
                Err(error) => {
                    // Preserve this source path for deletion reconciliation and
                    // retry parsing without discarding its prior index entry.
                    eprintln!("[codebuddy] legacy source is temporarily unreadable: {error:#}");
                    if let Some(dir) = self.session_dir(row) {
                        refs.push(SessionFileRef {
                            agent: AgentId::Codebuddy,
                            native_id: row.id.clone(),
                            file_path: dir.join(INDEX).to_string_lossy().into_owned(),
                            mtime_ms: i64::MAX,
                            size: 0,
                        });
                    }
                }
            }
        }
        Ok(refs)
    }
    fn file_ref(&self, path: &Path) -> Option<SessionFileRef> {
        let _guard = self
            .host
            .as_ref()
            .map(|_| REMOTE_CACHE_SWITCH.read().unwrap());
        if path.file_name()? != INDEX {
            return None;
        }
        self.rows()
            .ok()?
            .iter()
            .filter(|row| !row.deleted && self.owns_row(row))
            .find(|row| self.session_dir(row).is_some_and(|d| d.join(INDEX) == path))
            .and_then(|row| self.reference(row).ok().flatten())
    }
    fn cross_source_dependents(&self, path: &Path) -> Vec<PathBuf> {
        if path.extension().and_then(|s| s.to_str()) != Some("jsonl")
            || subagent_parent(path).is_some()
        {
            return Vec::new();
        }
        let Some(runtime) = path.file_stem().and_then(|s| s.to_str()) else {
            return Vec::new();
        };
        self.rows()
            .ok()
            .into_iter()
            .flat_map(|rows| {
                rows.iter()
                    .filter(|row| {
                        !row.deleted
                            && self.owns_row(row)
                            && row.provider == "cbc"
                            && (row.runtime_id == runtime
                                || (row.runtime_id.is_empty() && row.id == runtime))
                    })
                    .filter_map(|row| self.session_dir(row))
                    .filter(|dir| dir.is_dir())
                    .map(|dir| dir.join(INDEX))
                    .collect::<Vec<_>>()
            })
            .collect()
    }
    fn parse_session(&self, r: &SessionFileRef) -> Result<ParsedSession> {
        let (meta, messages, unknown) = self.parse(r, false)?;
        Ok(ParsedSession::derive(meta, &messages, unknown))
    }
    fn parse_transcript(&self, r: &SessionFileRef) -> Result<ParsedTranscript> {
        let (meta, mainline, unknown_line_count) = self.parse(r, true)?;
        Ok(ParsedTranscript {
            meta,
            mainline,
            sidechains: Vec::new(),
            unknown_line_count,
        })
    }
    fn session_paths(&self, _meta: &SessionMeta) -> Vec<String> {
        Vec::new()
    }
}

/// Include each parser dependency: messages may be rewritten after the index.
/// The mtime field is an opaque scanner fingerprint; size remains actual bytes.
fn session_stamp(dir: &Path, row: &CatalogRow) -> Result<(i64, i64)> {
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    row.hash(&mut hash);
    let mut paths = vec![dir.join(INDEX)];
    for name in ["messages", "assets"] {
        let sub = dir.join(name);
        match fs::symlink_metadata(&sub) {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => continue,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        }
        for entry in walkdir::WalkDir::new(sub)
            .follow_links(false)
            .sort_by_file_name()
        {
            let entry = entry?;
            if entry.file_type().is_file() && !entry.file_name().to_string_lossy().starts_with('.')
            {
                paths.push(entry.into_path());
            }
        }
    }
    let mut bytes = 0_i64;
    for path in paths {
        path.strip_prefix(dir).unwrap_or(&path).hash(&mut hash);
        match fs::metadata(&path) {
            Ok(m) => {
                m.len().hash(&mut hash);
                m.modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_nanos())
                    .hash(&mut hash);
                hash_file_identity(&m, &mut hash);
                bytes = bytes.saturating_add(m.len() as i64);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => 0_u8.hash(&mut hash),
            Err(e) => return Err(e.into()),
        }
    }
    Ok(((hash.finish() & i64::MAX as u64) as i64, bytes))
}

fn json_field(value: Option<&Value>) -> Value {
    match value {
        Some(Value::String(s)) => {
            serde_json::from_str(s.trim_start_matches('\u{feff}')).unwrap_or(Value::Null)
        }
        Some(v) => v.clone(),
        None => Value::Null,
    }
}

pub(crate) fn read_session_file(dir: &Path, file: &Path) -> std::io::Result<Vec<u8>> {
    let real_dir = fs::canonicalize(dir)?;
    let real_file = fs::canonicalize(file)?;
    if !real_file.starts_with(real_dir) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "session file escapes its directory",
        ));
    }
    let limit = if file.file_name().is_some_and(|name| name == INDEX) {
        MAX_HISTORY_INDEX_BYTES
    } else {
        MAX_HISTORY_MESSAGE_BYTES
    };
    if fs::metadata(&real_file)?.len() > limit {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "legacy history file exceeds the size limit",
        ));
    }
    fs::read(real_file)
}

/// Latest entry for a request wins; cache tokens are already in inputTokens.
pub(crate) fn request_usage(index: &Value) -> Option<HashMap<String, i64>> {
    let mut requests = HashMap::new();
    for request in index.get("requests")?.as_array()? {
        let id = request
            .get("id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())?;
        let total = request
            .get("usage")
            .and_then(Value::as_object)
            .and_then(|usage| {
                usage
                    .get("totalTokens")
                    .and_then(Value::as_i64)
                    .or_else(|| {
                        Some(
                            usage
                                .get("inputTokens")?
                                .as_i64()?
                                .saturating_add(usage.get("outputTokens")?.as_i64()?),
                        )
                    })
            })
            .filter(|n| *n >= 0);
        // Preserve the latest revision of each request even when it is missing
        // usage; a partial total would be misleading for the whole session.
        requests.insert(id.to_string(), total);
    }
    if requests.is_empty() {
        return None;
    }
    requests
        .into_iter()
        .map(|(id, total)| total.map(|n| (id, n)))
        .collect()
}

fn question_answers(extra: &Value) -> Vec<String> {
    let mut out = Vec::new();
    let direct = extra
        .pointer("/questionAnswer/questions")
        .and_then(Value::as_array);
    let nested = extra
        .get("sourceContentBlocks")
        .and_then(Value::as_array)
        .and_then(|blocks| {
            blocks.iter().find_map(|b| {
                b.pointer("/_meta/questionAnswer/questions")
                    .and_then(Value::as_array)
            })
        });
    for question in direct.or(nested).into_iter().flatten() {
        for answer in question
            .get("answers")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(s) = answer.as_str().map(str::trim).filter(|s| !s.is_empty()) {
                out.push(s.to_string());
            }
        }
    }
    out
}

fn strip_question_xml(mut text: String) -> String {
    while let Some(start) = text.find("<question_answer") {
        let Some(end) = text[start..].find("</question_answer>") else {
            break;
        };
        text.replace_range(start..start + end + "</question_answer>".len(), "");
    }
    text.trim().to_string()
}

fn warning(text: &str) -> TranscriptMessage {
    let mut message = meta_msg(text, 0);
    message.kind = MessageKind::SourceWarning;
    message
}

/// Resolve only attachment paths inside the selected session. Absolute paths
/// from a remote machine may be rebased if their full Data/session suffix
/// matches; no arbitrary local or remote path is ever dereferenced.
pub(crate) fn asset_relative(source: &str, session_relative: &Path) -> Option<PathBuf> {
    let raw = percent_decode(source.strip_prefix("codebuddy-asset://")?);
    let path = Path::new(&raw);
    let mut suffix = PathBuf::new();
    if path.is_absolute() || raw.starts_with('/') {
        let mut expected = vec![
            std::ffi::OsStr::new("CodeBuddyExtension"),
            std::ffi::OsStr::new("Data"),
        ];
        expected.extend(session_relative.components().map(|c| c.as_os_str()));
        let parts: Vec<_> = path.components().map(|c| c.as_os_str()).collect();
        let start = parts
            .windows(expected.len())
            .position(|w| w == expected.as_slice())?
            + expected.len();
        for part in &parts[start..] {
            suffix.push(part);
        }
    } else {
        suffix.push(path);
    }
    if suffix.as_os_str().is_empty()
        || !suffix
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
        || suffix.to_string_lossy().chars().any(char::is_control)
    {
        return None;
    }
    Some(suffix)
}

fn image_content(block: &Value, dir: &Path, decode: bool) -> ParsedContent {
    let Some(source) = block.get("image").and_then(Value::as_str) else {
        return content_parts(block, decode);
    };
    if source.starts_with("data:") {
        return content_parts(&serde_json::json!({"type":"image","url":source}), decode);
    }
    if source.starts_with("codebuddy-asset://") {
        if decode {
            let session_relative = dir
                .ancestors()
                .find(|p| {
                    p.file_name().is_some_and(|n| n == "Data")
                        && p.parent()
                            .and_then(Path::file_name)
                            .is_some_and(|n| n == "CodeBuddyExtension")
                })
                .and_then(|root| dir.strip_prefix(root).ok())
                .unwrap_or(Path::new(""));
            if let Some(relative) = asset_relative(source, session_relative) {
                if let Some(image) = image_from_session_file(dir, &dir.join(relative)) {
                    return ParsedContent {
                        text: String::new(),
                        images: vec![image],
                    };
                }
            }
        }
        let mut content = ParsedContent::default();
        content.push_text(IMAGE_PLACEHOLDER);
        return content;
    }
    content_parts(block, decode)
}

fn legacy_tool_output(value: &Value, dir: &Path, decode: bool) -> ParsedContent {
    if let Some(parts) = value.as_array() {
        let mut out = ParsedContent::default();
        for part in parts {
            out.append(legacy_tool_output(part, dir, decode));
        }
        return out;
    }
    if is_image_part(value) {
        return image_content(value, dir, decode);
    }
    if let Some(parts) = value.get("content").and_then(Value::as_array) {
        return legacy_tool_output(&Value::Array(parts.clone()), dir, decode);
    }
    tool_result_parts(value, decode)
}

struct LegacyHistory {
    messages: Vec<TranscriptMessage>,
    unknown: u32,
    tokens_used: Option<i64>,
    model: Option<String>,
}

fn parse_history(dir: &Path, decode: bool) -> Result<LegacyHistory> {
    let _budget = transcript_image_decode_budget(decode);
    let index: Value = serde_json::from_slice(
        &read_session_file(dir, &dir.join(INDEX)).context("read legacy session index")?,
    )
    .context("parse legacy session index")?;
    let entries = index
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("legacy index has no messages array"))?;
    let tokens = request_usage(&index).map(|m| m.values().fold(0_i64, |a, b| a.saturating_add(*b)));
    let mut messages: Vec<TranscriptMessage> = Vec::new();
    let mut tools: HashMap<String, (usize, usize)> = HashMap::new();
    let mut seen_ids = HashSet::new();
    let mut unknown = 0;
    let mut model = None;
    for entry in entries {
        let Some(id) = entry
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| safe_id(id))
        else {
            messages.push(warning("[Invalid legacy message reference]"));
            unknown += 1;
            continue;
        };
        if !seen_ids.insert(id.to_string()) {
            continue;
        }
        let path = dir.join("messages").join(format!("{id}.json"));
        let bytes = match read_session_file(dir, &path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                messages.push(warning(&format!("[Missing legacy message: {id}]")));
                unknown += 1;
                continue;
            }
            Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
                messages.push(warning(&format!("[Unreadable legacy message: {id}]")));
                unknown += 1;
                continue;
            }
            Err(e) => return Err(e).context("read legacy message"),
        };
        let outer: Value = match serde_json::from_slice(&bytes) {
            Ok(value) => value,
            Err(_) => {
                messages.push(warning(&format!("[Unreadable legacy message: {id}]")));
                unknown += 1;
                continue;
            }
        };
        let inner = json_field(outer.get("message"));
        if !inner.is_object() {
            messages.push(warning(&format!("[Unreadable legacy message: {id}]")));
            unknown += 1;
            continue;
        }
        let extra = json_field(outer.get("extra"));
        let role = outer
            .get("role")
            .and_then(Value::as_str)
            .or_else(|| inner.get("role").and_then(Value::as_str))
            .unwrap_or("");
        let ts = outer.get("createdAt").map(to_epoch_ms).unwrap_or(0);
        let message_model = optional_string(extra.get("modelName"))
            .or_else(|| optional_string(extra.get("modelId")));
        if message_model.is_some() {
            model = message_model.clone();
        }
        let blocks = match inner.get("content") {
            Some(Value::Array(blocks)) => blocks.clone(),
            Some(Value::String(text)) => vec![serde_json::json!({"type":"text","text":text})],
            _ => Vec::new(),
        };
        let mut content = ParsedContent::default();
        let mut thinking = Vec::new();
        let mut calls = Vec::new();
        let answers = if role == "user" {
            question_answers(&extra)
        } else {
            Vec::new()
        };
        for block in &blocks {
            match block.get("type").and_then(Value::as_str).unwrap_or("") {
                "text" => {
                    if let Some(s) = block.get("text").and_then(Value::as_str) {
                        if answers.is_empty() {
                            content.push_text(s);
                        } else {
                            content.push_text(&strip_question_xml(s.to_string()));
                        }
                    }
                }
                "image" => content.append(image_content(block, dir, decode)),
                "reasoning" | "thinking" => {
                    if let Some(s) = block
                        .get("text")
                        .or_else(|| block.get("reasoning"))
                        .and_then(Value::as_str)
                    {
                        thinking.push(s.to_string());
                    }
                }
                "tool-call" => {
                    let id = block
                        .get("toolCallId")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let name = block.get("toolName").and_then(Value::as_str).unwrap_or("");
                    calls.push(tool_call_view(
                        id,
                        name,
                        block.get("args").unwrap_or(&Value::Null),
                        None,
                        false,
                    ));
                }
                "tool-result" => {
                    let id = block
                        .get("toolCallId")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    let result = legacy_tool_output(
                        block
                            .get("result")
                            .or_else(|| block.get("content"))
                            .unwrap_or(&Value::Null),
                        dir,
                        decode,
                    );
                    let error = block.get("isError").and_then(Value::as_bool) == Some(true);
                    if let Some(call) = calls
                        .iter_mut()
                        .find(|call| call.id == id && !id.is_empty())
                    {
                        call.output = Some(clip(&result.text, MAX_TOOL_IO).0);
                        call.is_error = error;
                        content.append(ParsedContent {
                            text: String::new(),
                            images: result.images,
                        });
                    } else if let Some(&(mi, ti)) = tools.get(id) {
                        messages[mi].tool_calls[ti].output =
                            Some(clip(&result.text, MAX_TOOL_IO).0);
                        messages[mi].tool_calls[ti].is_error = error;
                        append_images_to_message_end(&mut messages[mi], result.images);
                    } else {
                        let name = block
                            .get("toolName")
                            .and_then(Value::as_str)
                            .unwrap_or("tool");
                        let mut orphan = text_msg(Role::Assistant, "", ts);
                        orphan.tool_calls.push(tool_call_view(
                            id.to_string(),
                            name,
                            &Value::Null,
                            Some(result.text),
                            error,
                        ));
                        append_images_to_message_end(&mut orphan, result.images);
                        messages.push(orphan);
                    }
                }
                _ => {
                    unknown += 1;
                }
            }
        }
        match role {
            "user" => {
                if !answers.is_empty() {
                    for answer in answers {
                        if !content.text.contains(&answer) {
                            content.push_text(&answer);
                        }
                    }
                }
                if !content.text.is_empty() || !content.images.is_empty() {
                    let mut m = text_msg(Role::User, &content.text, ts);
                    if extra.get("isHelperMessage").and_then(Value::as_bool) == Some(true)
                        || extra.get("isMeta").and_then(Value::as_bool) == Some(true)
                    {
                        m.kind = MessageKind::Meta;
                    }
                    m.images = content.images;
                    messages.push(m);
                }
            }
            "assistant" => {
                if !content.text.is_empty()
                    || !content.images.is_empty()
                    || !thinking.is_empty()
                    || !calls.is_empty()
                {
                    let mut m = text_msg(Role::Assistant, &content.text, ts);
                    m.images = content.images;
                    m.model = message_model;
                    m.thinking =
                        (!thinking.is_empty()).then(|| clip(&thinking.join("\n\n"), MAX_TOOL_IO).0);
                    m.tool_calls = calls;
                    let mi = messages.len();
                    for (ti, call) in m.tool_calls.iter().enumerate() {
                        if !call.id.is_empty() {
                            tools.insert(call.id.clone(), (mi, ti));
                        }
                    }
                    messages.push(m);
                }
            }
            "tool" => {}
            _ => {
                unknown += 1;
            }
        }
    }
    assign_seq(&mut messages);
    Ok(LegacyHistory {
        messages,
        unknown,
        tokens_used: tokens,
        model,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fixture_row(id: &str, provider: &str) -> CatalogRow {
        CatalogRow {
            id: id.into(),
            user_id: "account-1".into(),
            workspace: "/tmp/link/../project".into(),
            authority: String::new(),
            title: "Catalog title".into(),
            custom_title: String::new(),
            created_at: 1_700_000_000_000,
            updated_at: 1_700_000_001_000,
            deleted: false,
            archived: false,
            provider: provider.into(),
            runtime_id: id.into(),
            revision: 1,
        }
    }

    fn write_message(dir: &Path, id: &str, role: &str, content: Value, extra: Value) {
        let outer = json!({
            "id": id, "role": role, "createdAt": "2024-01-01T00:00:00Z",
            "message": json!({"role": role, "content": content}).to_string(),
            "extra": extra.to_string(),
        });
        fs::write(
            dir.join("messages").join(format!("{id}.json")),
            outer.to_string(),
        )
        .unwrap();
    }

    #[test]
    fn path_mapping_is_lexical_and_authority_is_host_scoped() {
        assert_eq!(
            workspace_hash("/tmp/link/../project"),
            workspace_hash("/tmp/project")
        );
        assert_eq!(
            workspace_hash("/tmp/link/../project/"),
            Some(format!("{:x}", Md5::digest(b"/tmp/project/")))
        );
        assert_ne!(
            workspace_hash("/tmp/symlink/project"),
            workspace_hash("/tmp/real/project")
        );
        assert!(workspace_hash("relative/path").is_none());
        assert!(authority_matches("", None));
        assert!(authority_matches(
            "codebuddy-remote-ssh+_x43__x56__x4d_",
            Some("CVM")
        ));
        assert!(!authority_matches(
            "codebuddy-remote-ssh+_x43__x56__x4d_",
            Some("other")
        ));
        let mut row = fixture_row("good", PROVIDER);
        assert!(legacy_relative_dir(&row).is_some());
        row.user_id = "../outside".into();
        assert!(legacy_relative_dir(&row).is_none());
    }

    #[test]
    fn incomplete_request_usage_does_not_report_a_partial_total() {
        assert!(request_usage(&json!({"requests":[
            {"id":"r1","usage":{"totalTokens":10}}, {"id":"r2","state":"running"}
        ]}))
        .is_none());
        assert!(request_usage(&json!({"requests":[
            {"id":"r1","usage":{"totalTokens":10}}, {"id":"r1","state":"running"}
        ]}))
        .is_none());
        assert_eq!(
            request_usage(&json!({"requests":[
                {"id":"r1","state":"running"}, {"id":"r1","usage":{"totalTokens":10}}
            ]}))
            .unwrap()["r1"],
            10
        );
    }

    #[test]
    fn ordered_history_preserves_tools_images_answers_and_missing_messages() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        fs::create_dir_all(dir.join("messages")).unwrap();
        fs::create_dir_all(dir.join("assets")).unwrap();
        let png = b"\x89PNG\r\n\x1a\n";
        fs::write(dir.join("assets/picture.png"), png).unwrap();
        let entries: Vec<_> = ["first", "assistant", "tool", "answer", "missing", "images"]
            .into_iter()
            .map(|id| json!({"id":id}))
            .collect();
        let index = json!({
            "messages": entries,
            "requests": [
                {"id":"r1", "usage":{"inputTokens": 7,"outputTokens": 3,"totalTokens": 10,"cacheTokens":5}},
                {"id":"r1", "usage":{"inputTokens": 9,"outputTokens": 3,"totalTokens": 12}},
                {"id":"r2", "usage":{"inputTokens": 4,"outputTokens": 1}},
            ]
        });
        fs::write(dir.join(INDEX), index.to_string()).unwrap();
        write_message(
            dir,
            "first",
            "user",
            json!([{"type":"text","text":"first question"}]),
            json!({}),
        );
        write_message(
            dir,
            "assistant",
            "assistant",
            json!([
                {"type":"reasoning","text":"private thinking"},
                {"type":"text","text":"assistant reply"},
                {"type":"tool-call","toolName":"Read","toolCallId":"tool-1","args":{"path":"file"}}
            ]),
            json!({"modelName":"fixture-model"}),
        );
        write_message(
            dir,
            "tool",
            "tool",
            json!([
                {"type":"tool-result","toolName":"Read","toolCallId":"tool-1","result":[
                    {"type":"text","text":"unindexed output"},
                    {"type":"image","image":"data:image/png;base64,iVBORw0KGgo="}
                ]}
            ]),
            json!({}),
        );
        write_message(
            dir,
            "answer",
            "user",
            json!([
                {"type":"text","text":"<question_answer><question_item><answers>human choice</answers></question_item></question_answer>"}
            ]),
            json!({"questionAnswer":{"questions":[{"question":"Which?","answers":["human choice"]}]}}),
        );
        write_message(
            dir,
            "images",
            "user",
            json!([
                {"type":"image","image":"data:image/png;base64,iVBORw0KGgo="},
                {"type":"image","image":"codebuddy-asset://assets/picture.png"}
            ]),
            json!({}),
        );

        let LegacyHistory {
            messages: full,
            unknown,
            tokens_used: tokens,
            model,
        } = parse_history(dir, true).unwrap();
        let for_index = parse_history(dir, false).unwrap().messages;
        assert_eq!(unknown, 1);
        assert_eq!(tokens, Some(17));
        assert_eq!(model.as_deref(), Some("fixture-model"));
        assert_eq!(full.len(), 5);
        assert_eq!(full[0].text, "first question");
        assert_eq!(full[1].text, "assistant reply");
        assert_eq!(full[1].thinking.as_deref(), Some("private thinking"));
        assert_eq!(
            full[1].tool_calls[0].output.as_deref(),
            Some("unindexed output")
        );
        assert_eq!(full[1].images.len(), 1);
        assert_eq!(full[2].text, "human choice");
        assert_eq!(full[3].kind, MessageKind::SourceWarning);
        assert!(full[3].text.contains("missing"));
        assert_eq!(full[4].images.len(), 2);
        assert_eq!(
            full.iter().map(|m| m.seq).collect::<Vec<_>>(),
            for_index.iter().map(|m| m.seq).collect::<Vec<_>>()
        );
        let parsed = ParsedSession::derive(dummy_meta(), &for_index, unknown);
        assert_eq!(
            parsed
                .units
                .iter()
                .filter(|u| u.text.contains("human choice"))
                .count(),
            1
        );
        assert_eq!(
            parsed
                .units
                .iter()
                .find(|u| u.text.contains("human choice"))
                .unwrap()
                .seq,
            full[2].seq
        );
        assert!(!parsed
            .units
            .iter()
            .any(|u| u.text.contains("unindexed output") || u.text.contains("private thinking")));
    }

    fn dummy_meta() -> SessionMeta {
        SessionMeta {
            key: "codebuddy:synthetic".into(),
            id: "synthetic".into(),
            host: String::new(),
            agent: AgentId::Codebuddy,
            title: String::new(),
            project_path: String::new(),
            project_name: String::new(),
            file_path: String::new(),
            created_at: 0,
            updated_at: 0,
            message_count: 0,
            size_bytes: 0,
            git_branch: None,
            model: None,
            tokens_used: None,
            archived: false,
            source: None,
            spawned: false,
            favorite: false,
            pinned: false,
        }
    }

    #[test]
    fn asset_reference_cannot_escape_the_session() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("session");
        fs::create_dir_all(&dir).unwrap();
        fs::write(tmp.path().join("outside.png"), b"\x89PNG\r\n\x1a\n").unwrap();
        let image = image_content(
            &json!({"type":"image", "image":"codebuddy-asset://../outside.png"}),
            &dir,
            true,
        );
        assert!(image.images.is_empty());
        assert!(image.text.contains(IMAGE_PLACEHOLDER));
    }

    #[test]
    #[ignore = "reads real local CodeBuddy conversation files"]
    fn real_legacy_images_are_read_inside_their_session() {
        let adapter = CodebuddyLegacyAdapter::new();
        let rows = adapter.rows().unwrap();
        let mut image_sessions = 0;
        let mut images = 0;
        for row in rows
            .iter()
            .filter(|row| !row.deleted && row.provider == PROVIDER && adapter.owns_row(row))
            .take(100)
        {
            let Some(reference) = adapter.reference(row).unwrap() else {
                continue;
            };
            let transcript = adapter.parse_transcript(&reference).unwrap();
            let found: usize = transcript
                .mainline
                .iter()
                .map(|message| message.images.len())
                .sum();
            if found > 0 {
                image_sessions += 1;
                images += found;
            }
            if image_sessions >= 10 {
                break;
            }
        }
        println!("local legacy image audit: sessions={image_sessions}, decoded_images={images}");
        assert!(
            image_sessions > 0 && images > 0,
            "no image-bearing sessions were decoded"
        );
    }
}
