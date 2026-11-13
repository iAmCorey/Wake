//! Selective CodeBuddy IDE 4.x remote sync. The 5.x catalog stays local and
//! only names the imported session directories; no credential database is sent.

use crate::adapters::codebuddy_legacy::{
    asset_relative, authority_matches, catalog_path, legacy_relative_dir, read_catalog,
    read_session_file, safe_id, REMOTE_CACHE_SWITCH, REMOTE_DATA_ROOT,
};
use crate::remote::{CHECK_MISSING_PARENT, SSH_OPTS};
use crate::services::terminal::sh_quote;
use serde_json::Value;
use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

const DATA: &str = REMOTE_DATA_ROOT;
static STAGE_ID: AtomicU64 = AtomicU64::new(0);

struct Stage(PathBuf);
impl Stage {
    fn create(cache: &Path, counter: &AtomicU64) -> std::io::Result<Self> {
        loop {
            let path = cache.join(format!(
                ".codebuddy-stage-{}-{}",
                std::process::id(),
                counter.fetch_add(1, Ordering::Relaxed)
            ));
            match fs::create_dir(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
    }
}
impl Drop for Stage {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[path = "remote_codebuddy_memory.rs"]
mod memory;
pub(crate) use memory::sync_memories;

fn allowed_memory_path(raw: &str, file: bool) -> bool {
    use crate::adapters::codebuddy_memory::{IDE_LINUX, IDE_MAC};
    let path = Path::new(raw);
    if raw.contains(['\n', '\r'])
        || path
            .components()
            .any(|c| !matches!(c, std::path::Component::Normal(_)))
    {
        return false;
    }
    let relative = if let Ok(p) = path.strip_prefix(".codebuddy/projects") {
        let parts: Vec<_> = p.components().collect();
        if parts.is_empty() {
            return !file;
        }
        if parts.len() < 2 || parts[1].as_os_str() != "memory" {
            return false;
        }
        p
    } else if let Ok(p) = path.strip_prefix(".codebuddy/memories") {
        p
    } else if let Some(p) = [IDE_LINUX, IDE_MAC]
        .iter()
        .find_map(|r| path.strip_prefix(r).ok())
    {
        p
    } else {
        return false;
    };
    !relative
        .components()
        .any(|c| c.as_os_str().to_string_lossy().starts_with('.'))
        && (!file || path.extension().is_some_and(|e| e == "md" || e == "mdc"))
}

/// Mirror only content-addressed images referenced by the cached CBC records.
/// The source catalog and unrelated blobs are never copied from the host.
pub(crate) fn sync_blobs(host: &str, cache: &Path) -> Result<(), String> {
    const BLOBS: &str = ".codebuddy/blobs";
    let mut paths = HashSet::new();
    let projects = cache.join(".codebuddy/projects");
    if !projects.is_dir() {
        return Ok(());
    }
    for entry in walkdir::WalkDir::new(&projects).follow_links(false) {
        let entry = entry.map_err(|e| format!("cannot inspect CBC cache: {e}"))?;
        if !entry.file_type().is_file() || entry.path().extension().is_none_or(|e| e != "jsonl") {
            continue;
        }
        let file =
            fs::File::open(entry.path()).map_err(|e| format!("cannot read CBC cache: {e}"))?;
        for row in crate::adapters::parse_utils::jsonl_values(std::io::BufReader::new(file)) {
            let row = row.map_err(|e| format!("cannot read CBC cache: {e}"))?;
            let Some(parts) = row
                .as_ref()
                .and_then(|r| r.get("content"))
                .and_then(Value::as_array)
            else {
                continue;
            };
            for block in parts {
                if let Some(relative) = crate::adapters::codebuddy_content::blob_relative(block) {
                    paths.insert(Path::new(BLOBS).join(relative));
                }
            }
        }
    }
    if paths.is_empty() {
        return Ok(());
    }
    let mut paths: Vec<_> = paths.into_iter().collect();
    paths.sort();
    let current = cache.join(BLOBS);
    let backup = current.with_file_name("blobs.wake-previous");
    recover_interrupted_swap(&current, &backup)?;
    let available = probe_files(host, &paths)?;
    let stage =
        Stage::create(cache, &STAGE_ID).map_err(|e| format!("cannot stage CBC images: {e}"))?;
    let staged = stage.0.join(BLOBS);
    fs::create_dir_all(&staged).map_err(|e| e.to_string())?;
    // Missing source images keep their previously downloaded copy. Transfer
    // failure keeps the entire previous generation, including its symlinks guard.
    for path in paths.iter().filter(|p| !available.contains(*p)) {
        let old = cache.join(path);
        if let Ok(bytes) = read_session_file(&current, &old) {
            let target = stage.0.join(path);
            fs::create_dir_all(target.parent().unwrap()).map_err(|e| e.to_string())?;
            fs::write(target, bytes).map_err(|e| e.to_string())?;
        }
    }
    paths.retain(|p| available.contains(p));
    sync_files(host, &stage.0, cache, &paths)?;
    replace_generation(&staged, &current, &backup)
}

/// Keep network failures and partial transfers away from the live cache. The
/// old generation is swapped only after all three selective transfers succeed.
pub(crate) fn sync_legacy(host: &str, cache: &Path) -> Result<(), String> {
    let catalog = catalog_path();
    match fs::metadata(&catalog) {
        Ok(meta) if meta.is_file() => {}
        Ok(_) => return Err("CodeBuddy catalog is not a file".into()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && !cache.join(DATA).exists() => {
            return Ok(())
        }
        Err(e) => return Err(format!("CodeBuddy catalog unavailable: {e}")),
    }
    let rows = read_catalog(&catalog).map_err(|e| format!("CodeBuddy catalog: {e:#}"))?;
    let mut sessions = Vec::new();
    for row in rows.iter().filter(|row| {
        !row.deleted
            && matches!(row.provider.as_str(), "codebuddy-ide-4" | "cbc")
            && authority_matches(&row.authority, Some(host))
    }) {
        match legacy_relative_dir(row) {
            Some(path) => sessions.push(path),
            None if row.provider == "codebuddy-ide-4" => {
                return Err("an imported CodeBuddy session has an invalid source path; the previous cache was kept".into());
            }
            None => {}
        }
    }
    sessions.sort();
    sessions.dedup();

    let stage = Stage::create(cache, &STAGE_ID)
        .map_err(|e| format!("cannot create CodeBuddy staging directory: {e}"))?;
    let staged_data = stage.0.join(DATA);
    fs::create_dir_all(&staged_data)
        .map_err(|e| format!("cannot create CodeBuddy staging cache: {e}"))?;
    let current = cache.join(DATA);
    let backup = current.with_file_name("Data.wake-previous");
    recover_interrupted_swap(&current, &backup)?;

    let index_paths: Vec<PathBuf> = sessions
        .iter()
        .map(|s| Path::new(DATA).join(s).join("index.json"))
        .collect();
    let available = probe_files(host, &index_paths)?;
    // A catalog entry can outlive a temporarily unavailable directory. Keep
    // its last successfully cached data rather than silently losing history.
    for session in &sessions {
        let index = Path::new(DATA).join(session).join("index.json");
        if !available.contains(&index) {
            let old = current.join(session);
            if old.is_dir() {
                copy_existing(&old, &staged_data.join(session))?;
            } else if rows.iter().any(|row| {
                row.provider == "codebuddy-ide-4"
                    && !row.deleted
                    && authority_matches(&row.authority, Some(host))
                    && legacy_relative_dir(row).as_deref() == Some(session.as_path())
            }) {
                return Err("an imported CodeBuddy history directory is unavailable; the previous cache was kept".into());
            }
        }
    }
    let indexes: Vec<_> = index_paths
        .iter()
        .filter(|p| available.contains(*p))
        .cloned()
        .collect();
    sync_files(host, &stage.0, cache, &indexes)?;

    let mut messages = Vec::new();
    let mut expected_indexes = std::collections::HashMap::new();
    for session in &sessions {
        let index_rel = Path::new(DATA).join(session).join("index.json");
        if !available.contains(&index_rel) {
            continue;
        }
        let bytes = read_session_file(&staged_data.join(session), &stage.0.join(&index_rel))
            .map_err(|e| format!("cannot read synced CodeBuddy index: {e}"))?;
        expected_indexes.insert(index_rel.clone(), bytes.clone());
        let index: Value = serde_json::from_slice(&bytes)
            .map_err(|e| format!("invalid synced CodeBuddy index: {e}"))?;
        let entries = index
            .get("messages")
            .and_then(Value::as_array)
            .ok_or_else(|| "synced CodeBuddy index has no messages array".to_string())?;
        for entry in entries {
            if let Some(id) = entry
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| safe_id(id))
            {
                messages.push(
                    Path::new(DATA)
                        .join(session)
                        .join("messages")
                        .join(format!("{id}.json")),
                );
            }
        }
    }
    messages.sort();
    messages.dedup();
    let existing_messages = probe_files(host, &messages)?;
    let transfer_messages: Vec<_> = messages
        .iter()
        .filter(|p| existing_messages.contains(*p))
        .cloned()
        .collect();
    sync_files(host, &stage.0, cache, &transfer_messages)?;

    let mut attachments = Vec::new();
    for session in &sessions {
        let session_root = Path::new(DATA).join(session);
        let message_dir = stage.0.join(&session_root).join("messages");
        let Ok(entries) = fs::read_dir(message_dir) else {
            continue;
        };
        for entry in entries.flatten() {
            if !entry.file_type().is_ok_and(|t| t.is_file()) {
                continue;
            }
            let bytes = match read_session_file(&staged_data.join(session), &entry.path()) {
                Ok(bytes) => bytes,
                Err(e) if e.kind() == std::io::ErrorKind::InvalidData => continue,
                Err(e) => return Err(format!("cannot read synced CodeBuddy message: {e}")),
            };
            let Ok(outer) = serde_json::from_slice::<Value>(&bytes) else {
                continue;
            };
            let inner = match outer.get("message") {
                Some(Value::String(s)) => serde_json::from_str::<Value>(s).ok(),
                Some(v) => Some(v.clone()),
                None => None,
            };
            let Some(blocks) = inner
                .as_ref()
                .and_then(|v| v.get("content"))
                .and_then(Value::as_array)
            else {
                continue;
            };
            for block in blocks {
                collect_assets(block, session, &session_root, &mut attachments, 0);
            }
        }
    }
    attachments.sort();
    attachments.dedup();
    let existing_assets = probe_files(host, &attachments)?;
    let transfer_assets: Vec<_> = attachments
        .into_iter()
        .filter(|p| existing_assets.contains(p))
        .collect();
    sync_files(host, &stage.0, cache, &transfer_assets)?;

    // The index determines the complete file list. If CodeBuddy changed it
    // while transfers were running, keep the previous generation and retry on
    // the next sync rather than publishing a mismatched snapshot.
    let verify = stage.0.join(".verify-indexes");
    fs::create_dir_all(&verify)
        .map_err(|e| format!("cannot stage CodeBuddy index verification: {e}"))?;
    sync_files(host, &verify, &stage.0, &indexes)?;
    for (path, expected) in expected_indexes {
        let actual = fs::read(verify.join(&path))
            .map_err(|e| format!("cannot verify synced CodeBuddy index: {e}"))?;
        if actual != expected {
            return Err(
                "CodeBuddy history changed during sync; the previous cache was kept".into(),
            );
        }
    }
    replace_generation(&staged_data, &current, &backup)
}

fn collect_assets(
    value: &Value,
    session: &Path,
    root: &Path,
    out: &mut Vec<PathBuf>,
    depth: usize,
) {
    if depth > 12 {
        return;
    }
    match value {
        Value::Array(items) => {
            for item in items {
                collect_assets(item, session, root, out, depth + 1);
            }
        }
        Value::Object(map) => {
            if map.get("type").and_then(Value::as_str) == Some("image") {
                if let Some(relative) = map
                    .get("image")
                    .and_then(Value::as_str)
                    .and_then(|s| asset_relative(s, session))
                {
                    out.push(root.join(relative));
                }
            }
            for item in map.values() {
                collect_assets(item, session, root, out, depth + 1);
            }
        }
        _ => {}
    }
}

fn path_text(path: &Path) -> Result<String, String> {
    let text = path
        .to_str()
        .ok_or_else(|| "non-UTF8 CodeBuddy source path".to_string())?;
    if text.is_empty() || text.chars().any(char::is_control) || path.is_absolute() {
        return Err("unsafe CodeBuddy source path".into());
    }
    #[cfg(windows)]
    let text = text.replace('\\', "/");
    #[cfg(not(windows))]
    let text = text.to_string();
    Ok(text)
}

fn manifest(paths: &[PathBuf]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    for path in paths {
        out.extend_from_slice(path_text(path)?.as_bytes());
        out.push(b'\n');
    }
    Ok(out)
}

/// Feed paths through stdin, never through shell interpolation. Ignore remote
/// banners and accept only exact members of the locally generated allowlist.
fn probe_files(host: &str, paths: &[PathBuf]) -> Result<HashSet<PathBuf>, String> {
    if paths.is_empty() {
        return Ok(HashSet::new());
    }
    let script = format!(
        "{}\n{}",
        CHECK_MISSING_PARENT,
        r#"
while IFS= read -r p; do
    if [ -f "$p" ]; then
        if [ ! -r "$p" ]; then
            printf 'Cannot read a CodeBuddy history file\n' >&2
            exit 2
        fi
        printf '%s\n' "$p"
    elif [ -e "$p" ]; then
        printf 'A CodeBuddy history file is not a regular file\n' >&2
        exit 2
    elif [ -L "$p" ]; then
        printf 'A CodeBuddy history symlink is unavailable\n' >&2
        exit 2
    else
        wake_check_parent "$p" || exit 2
    fi
done
"#
    );
    let mut cmd = Command::new("ssh");
    cmd.args(SSH_OPTS)
        .arg(host)
        .arg(format!("sh -c {}", sh_quote(&script)));
    let result = run_with_input(cmd, manifest(paths)?, "ssh")?;
    let allowed: HashSet<&Path> = paths.iter().map(PathBuf::as_path).collect();
    Ok(String::from_utf8_lossy(&result.stdout)
        .lines()
        .map(PathBuf::from)
        .filter(|p| allowed.contains(p.as_path()))
        .collect())
}

fn sync_files(host: &str, stage: &Path, previous: &Path, paths: &[PathBuf]) -> Result<(), String> {
    if paths.is_empty() {
        return Ok(());
    }
    let mut cmd = Command::new("rsync");
    cmd.current_dir(stage)
        .args([
            // Compare file content even when source size and mtime are equal.
            // CodeBuddy may rewrite a message in the same timestamp interval.
            "-azc",
            "--no-specials",
            "--timeout=120",
            "--files-from=-",
            "-e",
        ])
        .arg(format!("ssh {}", SSH_OPTS.join(" ")))
        .arg(format!("--link-dest={}", previous.display()))
        .arg(format!("{host}:./"))
        .arg(".");
    run_with_input(cmd, manifest(paths)?, "rsync").map(|_| ())
}

fn run_with_input(mut cmd: Command, data: Vec<u8>, name: &str) -> Result<Output, String> {
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot start {name}: {e}"))?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| format!("cannot open {name} input"))?;
    let writer = std::thread::spawn(move || stdin.write_all(&data));
    let output = child
        .wait_with_output()
        .map_err(|e| format!("cannot wait for {name}: {e}"))?;
    let written = writer
        .join()
        .map_err(|_| format!("{name} input thread failed"))?;
    if !output.status.success() {
        let tail: String = String::from_utf8_lossy(&output.stderr)
            .chars()
            .take(500)
            .collect();
        return Err(format!("{name} failed ({}): {tail}", output.status));
    }
    written.map_err(|e| format!("cannot send {name} file list: {e}"))?;
    Ok(output)
}

fn copy_existing(from: &Path, to: &Path) -> Result<(), String> {
    for entry in walkdir::WalkDir::new(from).follow_links(false) {
        let entry = entry.map_err(|e| format!("cannot inspect prior CodeBuddy cache: {e}"))?;
        let relative = entry.path().strip_prefix(from).map_err(|e| e.to_string())?;
        let target = to.join(relative);
        if entry.file_type().is_dir() {
            fs::create_dir_all(&target)
                .map_err(|e| format!("cannot stage prior CodeBuddy cache: {e}"))?;
        } else if entry.file_type().is_file() {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            if fs::hard_link(entry.path(), &target).is_err() {
                fs::copy(entry.path(), &target)
                    .map_err(|e| format!("cannot stage prior CodeBuddy cache: {e}"))?;
            }
        }
    }
    Ok(())
}

fn recover_interrupted_swap(current: &Path, backup: &Path) -> Result<(), String> {
    let _guard = REMOTE_CACHE_SWITCH.write().unwrap();
    if backup.exists() && !current.exists() {
        fs::rename(backup, current).map_err(|e| format!("cannot recover CodeBuddy cache: {e}"))?;
    } else if backup.exists() {
        fs::remove_dir_all(backup).map_err(|e| format!("cannot clear old CodeBuddy cache: {e}"))?;
    }
    Ok(())
}

fn replace_generation(stage: &Path, current: &Path, backup: &Path) -> Result<(), String> {
    let _guard = REMOTE_CACHE_SWITCH.write().unwrap();
    if let Some(parent) = current.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create CodeBuddy cache parent: {e}"))?;
    }
    let had_current = current.exists();
    if had_current {
        fs::rename(current, backup).map_err(|e| format!("cannot preserve CodeBuddy cache: {e}"))?;
    }
    if let Err(e) = fs::rename(stage, current) {
        if had_current {
            let _ = fs::rename(backup, current);
        }
        return Err(format!("cannot activate CodeBuddy cache: {e}"));
    }
    drop(_guard);
    if had_current {
        if let Err(e) = fs::remove_dir_all(backup) {
            eprintln!("cannot clear previous CodeBuddy cache: {e}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod verification {
    use super::*;
    use crate::models::{AgentId, MessageKind, SessionFileRef, SessionFilter};
    use crate::scanner::{run_scan, NullEvents};
    use std::sync::Arc;

    #[test]
    fn staging_never_reuses_a_previous_interrupted_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let counter = AtomicU64::new(0);
        let stale = tmp
            .path()
            .join(format!(".codebuddy-stage-{}-0", std::process::id()));
        fs::create_dir(&stale).unwrap();
        fs::write(stale.join("partial"), b"previous incomplete transfer").unwrap();
        let stage = Stage::create(tmp.path(), &counter).unwrap();
        assert_ne!(stage.0, stale);
        assert!(stale.join("partial").is_file());
        assert!(fs::read_dir(&stage.0).unwrap().next().is_none());
    }

    #[test]
    #[ignore = "reads real CBC transcripts and downloads referenced images from an explicitly selected host"]
    fn real_cbc_images_are_mirrored_in_isolation() {
        use crate::adapters::{codebuddy::CodebuddyAdapter, AgentAdapter};
        let host = std::env::var("WAKE_VERIFY_CODEBUDDY_HOST").expect("set host");
        assert!(crate::remote::valid_host_name(&host));
        let source = PathBuf::from(std::env::var("WAKE_VERIFY_CBC_CACHE").expect("set cache"));
        let tmp = tempfile::tempdir().unwrap();
        let mut copied = 0;
        let mut referenced = Vec::new();
        for entry in walkdir::WalkDir::new(source.join(".codebuddy/projects")) {
            let entry = entry.unwrap();
            if !entry.file_type().is_file() || entry.path().extension().is_none_or(|e| e != "jsonl")
            {
                continue;
            }
            let bytes = fs::read(entry.path()).unwrap();
            if !String::from_utf8_lossy(&bytes).contains("image_blob_ref") {
                continue;
            }
            let dest = tmp.path().join(entry.path().strip_prefix(&source).unwrap());
            fs::create_dir_all(dest.parent().unwrap()).unwrap();
            for row in String::from_utf8_lossy(&bytes)
                .lines()
                .filter_map(|s| serde_json::from_str::<Value>(s).ok())
            {
                for block in row["content"].as_array().into_iter().flatten() {
                    if let Some(relative) = crate::adapters::codebuddy_content::blob_relative(block)
                    {
                        referenced.push(relative);
                    }
                }
            }
            fs::write(dest, bytes).unwrap();
            copied += 1;
        }
        sync_blobs(&host, tmp.path()).unwrap();
        let missing = referenced
            .iter()
            .filter(|r| !tmp.path().join(".codebuddy/blobs").join(r).is_file())
            .count();
        let adapter = CodebuddyAdapter::new().with_remote_root(
            tmp.path().join(".codebuddy/projects"),
            tmp.path(),
            &host,
        );
        let mut images = 0;
        for r in adapter.list_session_files().unwrap() {
            images += adapter
                .parse_transcript(&r)
                .unwrap()
                .mainline
                .iter()
                .map(|m| m.images.len())
                .sum::<usize>();
        }
        assert!(images > 0);
        println!("isolated CBC image audit: transcripts={copied}, decoded_images={images}, missing_source_references={missing}");
    }

    /// Opt-in acceptance against a real local catalog and SSH alias. The Wake
    /// database and downloaded files live under a new temporary directory.
    #[test]
    #[ignore = "requires an explicitly selected SSH host and reads real conversation files"]
    fn real_catalog_ids_match_an_isolated_index() {
        let host =
            std::env::var("WAKE_VERIFY_CODEBUDDY_HOST").expect("set WAKE_VERIFY_CODEBUDDY_HOST");
        assert!(crate::remote::valid_host_name(&host));
        let remote_paths =
            crate::remote::probe_remote(&host).expect("probe regular remote sources");
        println!(
            "regular remote source paths detected={}",
            remote_paths.len()
        );
        let rows = read_catalog(&catalog_path()).unwrap();
        let expected: HashSet<String> = rows
            .iter()
            .filter(|row| !row.deleted && row.provider == "codebuddy-ide-4")
            .filter_map(|row| {
                if authority_matches(&row.authority, None) {
                    Some(format!("codebuddy:{}", row.id))
                } else if authority_matches(&row.authority, Some(&host)) {
                    Some(format!("codebuddy:{host}:{}", row.id))
                } else {
                    None
                }
            })
            .collect();
        let local_count = expected
            .iter()
            .filter(|key| !key.starts_with(&format!("codebuddy:{host}:")))
            .count();
        let remote_count = expected.len() - local_count;
        assert!(
            !expected.is_empty(),
            "catalog contains no eligible sessions"
        );
        let tmp = tempfile::tempdir_in("/tmp").unwrap();
        let store = Arc::new(crate::db::Store::open(&tmp.path().join("wake.db")).unwrap());
        store.add_remote_host(&host).unwrap();
        let cache = crate::remote::host_cache_dir(tmp.path(), &host);
        fs::create_dir_all(&cache).unwrap();
        sync_legacy(&host, &cache).expect("selective remote sync");
        let adapters: Vec<_> = crate::adapters::create_adapters_for(&store)
            .into_iter()
            .filter(|a| a.agent() == AgentId::Codebuddy)
            .collect();
        run_scan(&adapters, &store, &NullEvents, false).unwrap();
        let (all, _) = store
            .list_sessions(&SessionFilter {
                agents: vec![AgentId::Codebuddy],
                include_archived: true,
                limit: 100_000,
                ..Default::default()
            })
            .unwrap();
        let actual: HashSet<String> = all.iter().map(|meta| meta.key.clone()).collect();
        let missing = expected.difference(&actual).count();
        let eligible: HashSet<String> = rows
            .iter()
            .filter(|row| !row.deleted)
            .filter_map(|row| {
                if authority_matches(&row.authority, None) {
                    Some(format!("codebuddy:{}", row.id))
                } else if authority_matches(&row.authority, Some(&host)) {
                    Some(format!("codebuddy:{host}:{}", row.id))
                } else {
                    None
                }
            })
            .collect();
        let unexpected_legacy = all
            .iter()
            .filter(|meta| {
                crate::adapters::codebuddy_legacy::is_imported_legacy(meta)
                    && !eligible.contains(&meta.key)
            })
            .count();
        let deleted_indexed = rows
            .iter()
            .filter(|row| row.deleted)
            .filter(|row| {
                let prefix = if authority_matches(&row.authority, None) {
                    "codebuddy:".to_string()
                } else if authority_matches(&row.authority, Some(&host)) {
                    format!("codebuddy:{host}:")
                } else {
                    return false;
                };
                actual.contains(&format!("{prefix}{}", row.id))
                    || (!row.runtime_id.is_empty()
                        && actual.contains(&format!("{prefix}{}", row.runtime_id)))
            })
            .count();
        println!("isolated CodeBuddy audit: local={local_count}, remote={remote_count}, missing={missing}, unlisted_legacy={unexpected_legacy}, deleted_indexed={deleted_indexed}");
        assert_eq!(missing, 0, "catalog IDs missing from isolated index");
        assert_eq!(
            unexpected_legacy, 0,
            "legacy session outside the catalog was indexed"
        );
        assert_eq!(deleted_indexed, 0, "catalog-deleted sessions were indexed");

        // Verify the known incomplete source sessions against the actual
        // transcript placeholders, without printing any conversation text.
        let mut missing_files = 0;
        let mut incomplete_sessions = 0;
        for row in rows
            .iter()
            .filter(|row| !row.deleted && row.provider == "codebuddy-ide-4")
        {
            let (key, data) = if authority_matches(&row.authority, None) {
                (
                    format!("codebuddy:{}", row.id),
                    crate::adapters::codebuddy_legacy::default_data_root(),
                )
            } else if authority_matches(&row.authority, Some(&host)) {
                (format!("codebuddy:{host}:{}", row.id), cache.join(DATA))
            } else {
                continue;
            };
            let dir = data.join(legacy_relative_dir(row).unwrap());
            let Ok(index) = read_session_file(&dir, &dir.join("index.json")) else {
                continue;
            };
            let index: Value = serde_json::from_slice(&index).unwrap();
            let count = index
                .get("messages")
                .and_then(Value::as_array)
                .unwrap()
                .iter()
                .filter_map(|entry| entry.get("id").and_then(Value::as_str))
                .filter(|id| {
                    safe_id(id) && !dir.join("messages").join(format!("{id}.json")).is_file()
                })
                .count();
            if count == 0 {
                continue;
            }
            missing_files += count;
            incomplete_sessions += 1;
            let meta = store.get_session(&key).unwrap().unwrap();
            let adapter =
                crate::adapters::adapter_for(&adapters, AgentId::Codebuddy, &meta.file_path)
                    .unwrap();
            let transcript = adapter
                .parse_transcript(&SessionFileRef::from_meta(&meta))
                .unwrap();
            assert_eq!(
                count,
                transcript
                    .mainline
                    .iter()
                    .filter(|m| m.kind == MessageKind::SourceWarning
                        && m.text.starts_with("[Missing legacy message:"))
                    .count()
            );
        }

        // Check real converted sessions whose CBC side has no nonzero usage:
        // their displayed total must equal the old request totals. Their human
        // answers are checked in the isolated index as user messages.
        let index_db = rusqlite::Connection::open_with_flags(
            tmp.path().join("wake.db"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let mut token_samples = 0;
        let mut answer_samples = 0;
        let mut answer_missing = 0;
        let mut answer_duplicate = 0;
        for meta in all.iter().filter(|m| {
            m.host.is_empty()
                && crate::adapters::codebuddy_legacy::is_imported_legacy(m)
                && m.file_path.ends_with(".jsonl")
        }) {
            let Some(row) = rows
                .iter()
                .find(|r| format!("codebuddy:{}", r.id) == meta.key)
            else {
                continue;
            };
            let Some(relative) = legacy_relative_dir(row) else {
                continue;
            };
            let dir = crate::adapters::codebuddy_legacy::default_data_root().join(relative);
            let Ok(bytes) = read_session_file(&dir, &dir.join("index.json")) else {
                continue;
            };
            let Ok(index) = serde_json::from_slice::<Value>(&bytes) else {
                continue;
            };
            let Ok(cbc) = fs::read_to_string(&meta.file_path) else {
                continue;
            };
            let has_cbc_usage = cbc
                .lines()
                .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                .filter_map(|v| v.pointer("/providerData/rawUsage").cloned())
                .any(|v| {
                    v.as_object()
                        .is_some_and(|m| m.values().any(|n| n.as_i64().is_some_and(|n| n > 0)))
                });
            if !has_cbc_usage {
                if let Some(usage) = crate::adapters::codebuddy_legacy::request_usage(&index) {
                    let total: i64 = usage.values().sum();
                    assert_eq!(
                        meta.tokens_used,
                        Some(total),
                        "converted legacy usage differs"
                    );
                    token_samples += 1;
                }
            }
            let mut answers = HashSet::new();
            for entry in index
                .get("messages")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let Some(id) = entry
                    .get("id")
                    .and_then(Value::as_str)
                    .filter(|id| safe_id(id))
                else {
                    continue;
                };
                let path = dir.join("messages").join(format!("{id}.json"));
                let Ok(bytes) = read_session_file(&dir, &path) else {
                    continue;
                };
                let Ok(outer) = serde_json::from_slice::<Value>(&bytes) else {
                    continue;
                };
                let Some(extra) = outer
                    .get("extra")
                    .and_then(Value::as_str)
                    .and_then(|s| serde_json::from_str::<Value>(s).ok())
                else {
                    continue;
                };
                for question in extra
                    .pointer("/questionAnswer/questions")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    for answer in question
                        .get("answers")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                    {
                        if !answer.trim().is_empty() && cbc.contains(answer) {
                            answers.insert(answer.to_string());
                        }
                    }
                }
            }
            for answer in answers {
                let count: i64 = index_db.query_row(
                    "SELECT count(*) FROM messages WHERE session_key=?1 AND role='user' AND instr(text,?2)>0",
                    rusqlite::params![meta.key, answer], |r| r.get(0),
                ).unwrap();
                answer_samples += 1;
                answer_missing += usize::from(count == 0);
                answer_duplicate += usize::from(count > 1);
            }
        }
        println!("source audit: incomplete_sessions={incomplete_sessions}, missing_messages={missing_files}, converted_token_samples={token_samples}, human_answers={answer_samples}, missing_answers={answer_missing}, repeated_answers={answer_duplicate}");
        assert_eq!(answer_missing, 0);
        assert_eq!(answer_duplicate, 0);
    }
}

#[cfg(test)]
mod memory_tests {
    use super::*;

    #[test]
    #[ignore = "read-only SSH memory mirror into an isolated cache"]
    fn real_remote_memories_are_mirrored_in_isolation() {
        use crate::adapters::{codebuddy::CodebuddyAdapter, remote::RemoteAdapter, AgentAdapter};
        use crate::models::*;
        let host = std::env::var("WAKE_VERIFY_CODEBUDDY_HOST").expect("host required");
        let tmp = tempfile::tempdir().unwrap();
        crate::remote::sync_host(&host, tmp.path()).unwrap();
        let projects = tmp.path().join(".codebuddy/projects");
        if projects.is_dir() {
            for entry in fs::read_dir(projects).unwrap() {
                let entry = entry.unwrap();
                if entry.file_type().unwrap().is_dir() {
                    assert!(
                        matches!(fs::symlink_metadata(entry.path().join("memory")), Err(e) if e.kind() == std::io::ErrorKind::NotFound),
                        "regular session sync must not copy the memory tree"
                    );
                }
            }
        }
        sync_memories(&host, tmp.path()).unwrap();
        let adapter = RemoteAdapter::new(
            Box::new(CodebuddyAdapter::remote(
                tmp.path().join(".codebuddy/projects"),
                tmp.path(),
                &host,
            )),
            &host,
        );
        let docs = adapter
            .list_memories(&adapter.memory_sources(), &[])
            .unwrap();
        assert!(!docs.is_empty());
        let store = crate::db::Store::open(&tmp.path().join("audit.db")).unwrap();
        store
            .replace_memories(AgentId::Codebuddy, &host, &docs, &[])
            .unwrap();
        for doc in &docs {
            assert_eq!(doc.host, host);
            assert!(doc.key.starts_with(&format!("codebuddy:{host}:")));
            if doc.scope == MemoryScope::Project {
                assert!(!doc.project_path.is_empty(), "{}", doc.path);
            }
            let query: String = doc.title.chars().take(8).collect();
            assert!(
                store
                    .search_memories(
                        &query,
                        &MemoryFilter {
                            limit: 1000,
                            ..Default::default()
                        }
                    )
                    .unwrap()
                    .iter()
                    .any(|h| h.doc.key == doc.key),
                "{}",
                doc.title
            );
        }
        eprintln!(
            "Remote memory audit: host={host}, files={}, indexes={}, user={}",
            docs.len(),
            docs.iter().filter(|d| d.details.is_index).count(),
            docs.iter().filter(|d| d.scope == MemoryScope::User).count()
        );
    }
}
