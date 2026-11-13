//! CodeBuddy memory snapshots. Remote code uses only sh and rsync; parsing,
//! project ID compression, and SHA-256 verification run in Wake.
use super::{
    allowed_memory_path, recover_interrupted_swap, replace_generation, run_with_input, Stage,
    STAGE_ID,
};
use crate::adapters::codebuddy_memory::{
    self, MemorySettings, IDE_LINUX, IDE_MAC, MANIFEST, REMOTE_MIRROR,
};
use crate::remote::SSH_OPTS;
use crate::services::terminal::sh_quote;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const MARKER: &[u8] = b"WAKE_MEMORY_V1\0";
const MAX_RECORDS: usize = 100_000;
const MAX_OUTPUT: usize = 16 * 1024 * 1024;

// A connected SSH session can stall while traversing a filesystem or parsing
// damaged settings. Bound wall time and captured output, not just connect time.
fn probe_output(mut cmd: Command, input: Vec<u8>, timeout: Duration) -> Result<Vec<u8>, String> {
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot start CodeBuddy memory probe: {e}"))?;
    let mut stdin = child.stdin.take().ok_or("missing memory probe input")?;
    let writer = std::thread::spawn(move || stdin.write_all(&input));
    let overflow = Arc::new(AtomicBool::new(false));
    fn reader(
        mut pipe: impl Read + Send + 'static,
        limit: usize,
        overflow: Arc<AtomicBool>,
    ) -> std::thread::JoinHandle<std::io::Result<Vec<u8>>> {
        std::thread::spawn(move || {
            let mut result = Vec::new();
            let mut buf = [0; 8192];
            loop {
                let size = pipe.read(&mut buf)?;
                if size == 0 {
                    return Ok(result);
                }
                if size > limit.saturating_sub(result.len()) {
                    overflow.store(true, Ordering::Relaxed);
                } else {
                    result.extend_from_slice(&buf[..size]);
                }
            }
        })
    }
    let stdout = reader(child.stdout.take().unwrap(), MAX_OUTPUT, overflow.clone());
    let stderr = reader(child.stderr.take().unwrap(), 16 * 1024, overflow.clone());
    let start = Instant::now();
    let status = loop {
        if overflow.load(Ordering::Relaxed) || start.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            break Err("CodeBuddy memory probe exceeded time or output limit".to_string());
        }
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                break Err(format!("cannot wait for memory probe: {e}"));
            }
        }
    };
    let stdout = stdout.join().map_err(|_| "memory output reader failed")?;
    let stderr = stderr.join().map_err(|_| "memory error reader failed")?;
    let written = writer.join().map_err(|_| "memory input writer failed")?;
    let status = status?;
    if overflow.load(Ordering::Relaxed) {
        return Err("CodeBuddy memory probe exceeded output limit".into());
    }
    if !status.success() {
        let stderr = stderr.map_err(|e| e.to_string())?;
        let tail: String = String::from_utf8_lossy(&stderr).chars().take(500).collect();
        return Err(format!("CodeBuddy memory probe failed ({status}): {tail}"));
    }
    written.map_err(|e| format!("cannot send memory attribution paths: {e}"))?;
    stdout.map_err(|e| e.to_string())
}

#[derive(Debug, PartialEq, Eq)]
struct Inventory {
    dirs: BTreeSet<String>,
    files: BTreeSet<String>,
    settings: MemorySettings,
    ide_settings: BTreeMap<String, bool>,
}

fn script(mode: &str) -> String {
    format!(
        "sh -c {} sh {}",
        sh_quote(&format!(
            "{}\n{}",
            include_str!("codebuddy_memory_json.sh"),
            include_str!("codebuddy_memory_remote.sh")
        )),
        mode
    )
}

fn remote(host: &str, mode: &str, input: Vec<u8>) -> Result<Vec<u8>, String> {
    let mut cmd = Command::new("ssh");
    cmd.args(SSH_OPTS).arg(host).arg(script(mode));
    probe_output(cmd, input, Duration::from_secs(120))
}

/// Require framing, completion, and valid UTF-8. Do not interpret login banners
/// as records or publish a truncated inventory as an empty memory source.
fn records(stdout: &[u8]) -> Result<Vec<&str>, String> {
    if stdout.len() > MAX_OUTPUT {
        return Err("CodeBuddy memory inventory too large".into());
    }
    let start = stdout
        .windows(MARKER.len())
        .position(|w| w == MARKER)
        .ok_or("missing CodeBuddy memory inventory")?
        + MARKER.len();
    let mut records = Vec::new();
    let mut rest = &stdout[start..];
    loop {
        let end = rest
            .iter()
            .position(|b| *b == 0)
            .ok_or("incomplete CodeBuddy memory inventory")?;
        let word =
            std::str::from_utf8(&rest[..end]).map_err(|_| "non-UTF8 CodeBuddy memory metadata")?;
        rest = &rest[end + 1..];
        if word == "E" {
            if !rest.iter().all(u8::is_ascii_whitespace) {
                return Err("unexpected memory inventory output".into());
            }
            return Ok(records);
        }
        records.push(word);
        if records.len() > MAX_RECORDS * 3 {
            return Err("too many CodeBuddy memory records".into());
        }
    }
}

fn flag(value: &Value, key: &str, default: bool) -> Result<bool, String> {
    match value.get(key) {
        None => Ok(default),
        Some(value) => value
            .as_bool()
            .ok_or_else(|| format!("invalid CodeBuddy memory setting: {key}")),
    }
}

fn inventory(stdout: &[u8]) -> Result<Inventory, String> {
    let words = records(stdout)?;
    let mut words = words.into_iter();
    let mut dirs = BTreeSet::new();
    let mut files = BTreeSet::new();
    let mut configs = BTreeMap::new();
    while let Some(kind) = words.next() {
        let path = words.next().ok_or("incomplete memory record")?;
        match kind {
            "D" | "F" => {
                let file = kind == "F";
                if !allowed_memory_path(path, file)
                    || path
                        .split('/')
                        .any(|s| s.is_empty() || s == "." || s == "..")
                    || path.len() > 4096
                    || path.chars().any(char::is_control)
                    || (file
                        && !path.starts_with(IDE_LINUX)
                        && !path.starts_with(IDE_MAC)
                        && !path.ends_with(".md"))
                {
                    return Err("unsafe CodeBuddy memory path".into());
                }
                let inserted = if file {
                    files.insert(path.to_string())
                } else {
                    dirs.insert(path.to_string())
                };
                if !inserted {
                    return Err("duplicate memory path".into());
                }
            }
            "C" => {
                if !matches!(path, "cli" | "project" | "global_linux" | "global_mac") {
                    return Err("unexpected memory settings source".into());
                }
                let raw = words.next().ok_or("incomplete memory settings record")?;
                let value: Value = serde_json::from_str(raw)
                    .map_err(|_| "invalid sanitized CodeBuddy settings JSON")?;
                if !value.is_object() {
                    return Err("CodeBuddy memory settings must be an object".into());
                }
                if configs.insert(path, value).is_some() {
                    return Err("duplicate memory settings record".into());
                }
            }
            _ => return Err("unknown memory record".into()),
        }
    }
    let empty = json!({});
    let cli_config = configs.get("cli").unwrap_or(&empty);
    let cli = match cli_config.get("memory") {
        None => true,
        Some(memory) if memory.is_object() => flag(memory, "autoMemoryEnabled", true)?,
        Some(_) => return Err("invalid CodeBuddy CLI memory settings".into()),
    };
    let project = flag(
        configs.get("project").unwrap_or(&empty),
        "projectEnabled",
        cli,
    )?;
    let mut ide_settings = BTreeMap::new();
    for (key, root) in [("global_linux", IDE_LINUX), ("global_mac", IDE_MAC)] {
        if let Some(config) = configs.get(key) {
            ide_settings.insert(root.into(), flag(config, "enabled", true)?);
        }
        if dirs.contains(root) != configs.contains_key(key) {
            return Err("missing IDE memory settings".into());
        }
    }
    if (dirs.contains(".codebuddy/projects") || dirs.contains(".codebuddy/memories"))
        && (!configs.contains_key("cli") || !configs.contains_key("project"))
    {
        return Err("missing CLI memory settings".into());
    }
    if files
        .iter()
        .any(|p| !dirs.contains(Path::new(p).parent().and_then(Path::to_str).unwrap_or("")))
    {
        return Err("memory file has no inventoried directory".into());
    }
    let global = ide_settings.values().next().copied().unwrap_or(true);
    Ok(Inventory {
        dirs,
        files,
        settings: MemorySettings {
            cli,
            project,
            global,
        },
        ide_settings,
    })
}

fn fetch_inventory(host: &str) -> Result<Inventory, String> {
    inventory(&remote(host, "inventory", Vec::new())?)
}

fn transfer(host: &str, stage: &Path, inventory: &Inventory) -> Result<(), String> {
    for dir in &inventory.dirs {
        fs::create_dir_all(stage.join(dir)).map_err(|e| e.to_string())?;
    }
    if inventory.files.is_empty() {
        return Ok(());
    }
    // Always use an independent destination: rsync's negotiated checksum must
    // not substitute for comparing independently read bytes with SHA-256.
    let mut cmd = Command::new("rsync");
    cmd.current_dir(stage)
        .args([
            "-az",
            "--no-specials",
            "--max-size=4194304",
            "--from0",
            "--files-from=-",
            "--timeout=120",
            "-e",
        ])
        .arg(format!("ssh {}", SSH_OPTS.join(" ")))
        .arg(format!("{host}:./"))
        .arg(".");
    let mut paths = Vec::new();
    for path in &inventory.files {
        paths.extend_from_slice(path.as_bytes());
        paths.push(0);
    }
    run_with_input(cmd, paths, "rsync CodeBuddy memories").map(|_| ())
}

fn hashes(stage: &Path, inventory: &Inventory) -> Result<BTreeMap<String, String>, String> {
    inventory
        .files
        .iter()
        .map(|path| {
            // Includes completeness checking: rsync skips oversized files with exit
            // code 0, so success alone is insufficient to replace the old snapshot.
            let raw = codebuddy_memory::safe_text(stage, &stage.join(path))
                .map_err(|_| "incomplete or invalid CodeBuddy memory snapshot")?;
            Ok((
                path.clone(),
                format!("{:x}", Sha256::digest(raw.as_bytes())),
            ))
        })
        .collect()
}

fn attribution(
    host: &str,
    cache: &Path,
    inventory: &Inventory,
) -> Result<BTreeMap<String, BTreeSet<String>>, String> {
    use crate::adapters::codebuddy_legacy;
    let ids: BTreeSet<_> = inventory
        .dirs
        .iter()
        .filter_map(|dir| {
            let parts: Vec<_> = dir.split('/').collect();
            match parts.as_slice() {
                [".codebuddy", "projects", id, "memory"] => Some((*id).to_owned()),
                [".codebuddy", "memories", id] if *id != "global" => Some((*id).to_owned()),
                _ => None,
            }
        })
        .collect();
    if ids.is_empty() {
        return Ok(BTreeMap::new());
    }
    let mut candidates = BTreeSet::new();
    let catalog = codebuddy_legacy::catalog_path();
    if catalog.exists() {
        for row in codebuddy_legacy::read_catalog(&catalog)
            .map_err(|e| format!("cannot attribute CodeBuddy memory: {e}"))?
        {
            if codebuddy_legacy::authority_matches(&row.authority, Some(host)) {
                candidates.insert(row.workspace);
            }
        }
    }
    for id in &ids {
        let dir = cache.join(".codebuddy/projects").join(id);
        if !codebuddy_memory::exists(&dir).map_err(|e| e.to_string())? {
            continue;
        }
        for entry in fs::read_dir(&dir).map_err(|e| e.to_string())? {
            let path = entry.map_err(|e| e.to_string())?.path();
            if path.extension().is_some_and(|e| e == "jsonl") {
                if let Some(cwd) =
                    codebuddy_memory::transcript_cwd(&path).map_err(|e| e.to_string())?
                {
                    candidates.insert(cwd);
                }
            }
        }
    }
    let candidates: Vec<_> = candidates
        .into_iter()
        .filter(|p| p.starts_with('/') && !p.chars().any(char::is_control))
        .collect();
    if candidates.is_empty() {
        return Ok(BTreeMap::new());
    }
    let data = candidates
        .iter()
        .map(|p| format!("{p}\n"))
        .collect::<String>()
        .into_bytes();
    let stdout = remote(host, "resolve", data)?;
    let words = records(&stdout)?;
    let mut words = words.into_iter();
    let mut result = BTreeMap::<String, BTreeSet<String>>::new();
    for (index, original) in candidates.iter().enumerate() {
        if words.next() != Some("P")
            || words.next().and_then(|n| n.parse::<usize>().ok()) != Some(index + 1)
        {
            return Err("incomplete memory project attribution".into());
        }
        let physical = words.next().ok_or("missing physical project path")?;
        if !physical.starts_with('/') || physical.chars().any(char::is_control) {
            return Err("unsafe physical project path".into());
        }
        let physical_id = codebuddy_memory::project_id(physical);
        let original_id = codebuddy_memory::project_id(original);
        if ids.contains(&physical_id) {
            result
                .entry(physical_id)
                .or_default()
                .insert(physical.to_owned());
        } else if ids.contains(&original_id) {
            result
                .entry(original_id)
                .or_default()
                .insert(original.clone());
        }
    }
    if words.next().is_some() {
        return Err("unexpected project attribution output".into());
    }
    Ok(result)
}

pub(crate) fn sync_memories(host: &str, cache: &Path) -> Result<(), String> {
    fs::create_dir_all(cache).map_err(|e| e.to_string())?;
    let current = cache.join(REMOTE_MIRROR);
    let backup = current.with_file_name(".codebuddy-memory.wake-previous");
    recover_interrupted_swap(&current, &backup)?;
    let inventory = fetch_inventory(host)?;
    let projects = attribution(host, cache, &inventory)?;
    let first = Stage::create(cache, &STAGE_ID).map_err(|e| e.to_string())?;
    let second = Stage::create(cache, &STAGE_ID).map_err(|e| e.to_string())?;
    transfer(host, &first.0, &inventory)?;
    let content = hashes(&first.0, &inventory)?;
    if inventory != fetch_inventory(host)? {
        return Err("CodeBuddy memory changed during sync; previous cache kept".into());
    }
    transfer(host, &second.0, &inventory)?;
    if content != hashes(&second.0, &inventory)?
        || inventory != fetch_inventory(host)?
        || projects != attribution(host, cache, &inventory)?
    {
        return Err("CodeBuddy memory changed during sync; previous cache kept".into());
    }
    let manifest = json!({"dirs":inventory.dirs, "files":content, "settings":inventory.settings, "ide_settings":inventory.ide_settings, "projects":projects});
    // Preserve file identities when nothing changed, avoiding a full reindex on
    // every refresh. A damaged previous cache is repaired by a fresh generation.
    if fs::read(current.join(MANIFEST))
        .ok()
        .and_then(|v| serde_json::from_slice::<Value>(&v).ok())
        .as_ref()
        == Some(&manifest)
        && hashes(&current, &inventory).ok().as_ref() == Some(&content)
    {
        return Ok(());
    }
    fs::write(
        first.0.join(MANIFEST),
        serde_json::to_vec(&manifest).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    replace_generation(&first.0, &current, &backup)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::io::Write;
    use std::process::{Output, Stdio};

    fn shell(shell: &str, source: &str, cwd: &Path, input: &[u8], mode: &str) -> Output {
        let mut child = Command::new(shell)
            .args(["-c", source, "sh", mode])
            .current_dir(cwd)
            .env("PATH", "/nonexistent")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        let data = input.to_vec();
        let writer = std::thread::spawn(move || stdin.write_all(&data));
        let output = child.wait_with_output().unwrap();
        let _ = writer.join().unwrap();
        output
    }

    fn sanitized(shell_path: &str, raw: &str) -> Result<Value, ()> {
        let source = format!(
            "{}\nLC_ALL=C; export LC_ALL; wake_json",
            include_str!("codebuddy_memory_json.sh")
        );
        let output = shell(shell_path, &source, Path::new("/tmp"), raw.as_bytes(), "");
        assert!(!String::from_utf8_lossy(&output.stdout).contains("SECRET"));
        assert!(!String::from_utf8_lossy(&output.stderr).contains("SECRET"));
        if !output.status.success() {
            return Err(());
        }
        serde_json::from_slice(&output.stdout).map_err(|_| ())
    }

    #[test]
    fn json_lexer_handles_escaped_keys_duplicates_and_unrelated_values_without_utilities() {
        for shell in ["/bin/sh", "/bin/bash"] {
            for (raw, expected) in [
                (
                    r#"{"memory":{"autoMemoryEnabled":false},"token":"SECRET"}"#,
                    false,
                ),
                (
                    r#"{"me\u006dory":{"autoMemory\u0045nabled":false},"other":{"autoMemoryEnabled":true}}"#,
                    false,
                ),
                (
                    r#"{"memory":{"autoMemoryEnabled":false},"memory":{"autoMemoryEnabled":true}}"#,
                    true,
                ),
                (
                    r#"{"nested":[1,-2.3e+4,null,"SECRET\\\"abc",{"enabled":false}],"memory":{"autoMemoryEnabled":true}}"#,
                    true,
                ),
                (
                    "{\r\n\t\"memory\": {\"autoMemoryEnabled\": false}\r\n}",
                    false,
                ),
            ] {
                let value = sanitized(shell, raw).unwrap();
                assert_eq!(
                    value["memory"]["autoMemoryEnabled"], expected,
                    "{shell}: {raw}"
                );
            }
            for text in [
                "",
                "SECRET",
                "SECRET\\",
                "SECRET\"",
                "SECRET\nline",
                "SECRET\r\t",
                "SECRET中文🙂",
                "SECRET/*[a];$(id)",
            ] {
                for enabled in [true, false] {
                    let raw = serde_json::to_string(
                        &json!({"memory":{"autoMemoryEnabled":enabled},"unrelated":text}),
                    )
                    .unwrap();
                    assert_eq!(
                        sanitized(shell, &raw).unwrap()["memory"]["autoMemoryEnabled"],
                        enabled
                    );
                }
            }
        }
    }

    #[test]
    fn json_lexer_rejects_invalid_grammar_and_does_not_echo_error_input() {
        for shell in ["/bin/sh", "/bin/bash"] {
            for raw in [
                "",
                "{",
                "{}{}",
                "[1,]",
                "{\"SECRET\":1 2}",
                "{\"SECRET\":01}",
                "{\"SECRET\":1.}",
                "{\"SECRET\":1.2.}",
                "{\"SECRET\":1e2e}",
                "{\"SECRET\":1e+}",
                "{\"SECRET\":1e-+1}",
                "{\"SECRET\":.5}",
                "{\"SECRET\":NaN}",
                "{\"SECRET\":truefalse}",
                "{\"SECRET\":\"line\nline\"}",
                r#"{"SECRET":"\q"}"#,
                r#"{"SECRET":"\u0xz1"}"#,
                r#"{"SECRET":"\u000"}"#,
                r#"{"SECRET":"\ud800"}"#,
                r#"{"SECRET":"\udc00"}"#,
                r#"{"SECRET":"\ud800x\udc00"}"#,
                r#"{"SECRET":"unterminated}"#,
            ] {
                assert!(
                    sanitized(shell, raw).is_err(),
                    "{shell} accepted invalid input"
                );
            }
        }
    }

    #[test]
    fn probe_rejects_discarded_nul_and_invalid_utf8_and_accepts_unicode_pairs() {
        let tmp = tempfile::tempdir().unwrap();
        put(tmp.path(), ".codebuddy/projects/p/memory/a.md", "memory");
        let config = tmp.path().join(".codebuddy/settings.json");
        for invalid in [
            b"{\"memory\":{\"autoMemoryEnabled\":fal\0se}}".as_slice(),
            b"{\"SECRET\":\"\xff\"}",
            b"{\"SECRET\":\"\xc0\x80\"}",
            b"{\"SECRET\":\"\xed\xa0\x80\"}",
            b"{\"SECRET\":\"\xf4\x90\x80\x80\"}",
        ] {
            fs::write(&config, invalid).unwrap();
            let output = probe(tmp.path());
            assert!(!output.status.success());
            assert!(!String::from_utf8_lossy(&output.stdout).contains("SECRET"));
        }
        for valid in [
            r#"{"memory":{"autoMemoryEnabled":false},"text":"\ud83d\ude42"}"#,
            "{\"memory\":{\"autoMemoryEnabled\":false},\"text\":\"中文🙂\"}\n",
        ] {
            fs::write(&config, valid).unwrap();
            let output = probe(tmp.path());
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(!inventory(&output.stdout).unwrap().settings.cli);
        }
    }

    #[test]
    fn stalled_probe_has_a_deadline_and_does_not_hang_sync() {
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", "while :; do :; done"]);
        let start = Instant::now();
        assert!(probe_output(cmd, Vec::new(), Duration::from_millis(50)).is_err());
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn json_lexer_streams_large_strings_and_bounds_numbers_and_document_size() {
        let raw = serde_json::to_string(
            &json!({"memory":{"autoMemoryEnabled":false},"unrelated":"SECRET".repeat(40_000)}),
        )
        .unwrap();
        let start = std::time::Instant::now();
        for shell in ["/bin/sh", "/bin/bash"] {
            assert_eq!(
                sanitized(shell, &raw).unwrap()["memory"]["autoMemoryEnabled"],
                false
            );
        }
        assert!(
            start.elapsed().as_secs() < 10,
            "large string handling became quadratic"
        );
        assert!(sanitized("/bin/sh", &format!("{{\"x\":{}}}", "1".repeat(257))).is_err());
        assert!(sanitized(
            "/bin/sh",
            &format!("{{\"x\":\"{}\"}}", "s".repeat(4 * 1024 * 1024))
        )
        .is_err());
    }

    fn probe(path: &Path) -> Output {
        let rsync = std::env::split_paths(&std::env::var_os("PATH").unwrap())
            .map(|dir| dir.join("rsync"))
            .find(|p| p.is_file())
            .unwrap();
        let bin = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(rsync, bin.path().join("rsync")).unwrap();
        let source = format!(
            "PATH={}; export PATH\n{}\n{}",
            sh_quote(bin.path().to_str().unwrap()),
            include_str!("codebuddy_memory_json.sh"),
            include_str!("codebuddy_memory_remote.sh")
        )
        .replace("cd \"$HOME\"", "cd .");
        shell("/bin/sh", &source, path, &[], "inventory")
    }

    fn put(root: &Path, path: &str, body: &str) {
        let path = root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }

    #[test]
    fn discovery_preserves_empty_roots_and_both_ide_layouts_and_sanitizes_settings() {
        let tmp = tempfile::tempdir().unwrap();
        for (path, body) in [
            (".codebuddy/projects/demo/memory/a.md", "memory"),
            (
                ".codebuddy/projects/demo/memory/nested/中文 ; 'x'*.md",
                "nested",
            ),
            (
                ".codebuddy/projects/demo/memory/.ide-memory-backups/old.md",
                "excluded",
            ),
            (".codebuddy/projects/demo/chat.jsonl", "SECRET"),
            (".codebuddy/memories/demo/fallback.md", "fallback"),
            (".codebuddy/memories/global/cli.md", "global"),
            ("workspace/.codebuddy/memory/old.md", "excluded"),
            (
                ".codebuddy/settings.json",
                r#"{"memory":{"autoMemoryEnabled":false},"token":"SECRET"}"#,
            ),
            (
                ".codebuddy/ide-memory-settings.json",
                r#"{"projectEnabled":true}"#,
            ),
        ] {
            put(tmp.path(), path, body);
        }
        fs::create_dir_all(tmp.path().join(".codebuddy/projects/empty/memory")).unwrap();
        for (root, enabled) in [(IDE_LINUX, false), (IDE_MAC, true)] {
            put(tmp.path(), &format!("{root}/global.mdc"), "global");
            put(tmp.path(), &format!("{root}/subdir/ignored.md"), "excluded");
            put(
                tmp.path(),
                &format!("{root}/.memory-global-config.json"),
                &json!({"enabled":enabled,"token":"SECRET"}).to_string(),
            );
        }
        let output = probe(tmp.path());
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!String::from_utf8_lossy(&output.stdout).contains("SECRET"));
        let inv = inventory(&output.stdout).unwrap();
        assert_eq!(inv.files.len(), 6);
        assert!(inv.dirs.contains(".codebuddy/projects/empty/memory"));
        assert!(!inv.settings.cli);
        assert!(inv.settings.project);
        assert_eq!(inv.ide_settings[IDE_LINUX], false);
        assert_eq!(inv.ide_settings[IDE_MAC], true);
    }

    #[test]
    fn unrelated_host_and_removed_sources_need_no_settings_or_interpreter() {
        let tmp = tempfile::tempdir().unwrap();
        put(tmp.path(), ".codebuddy/settings.json", "malformed SECRET");
        let output = probe(tmp.path());
        assert!(output.status.success());
        let inv = inventory(&output.stdout).unwrap();
        assert!(inv.files.is_empty());
        assert!(inv.dirs.is_empty());
        assert!(inv.settings.cli && inv.settings.project);
    }

    #[test]
    fn discovery_rejects_symlink_ancestors_and_permission_errors_and_invalid_switches() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let tmp = tempfile::tempdir().unwrap();
        put(tmp.path(), ".codebuddy/projects/p/memory/a.md", "memory");
        let source = tmp.path().join(".codebuddy/projects/p/memory");
        symlink("missing", source.join("link")).unwrap();
        assert!(!probe(tmp.path()).status.success());
        fs::remove_file(source.join("link")).unwrap();
        fs::set_permissions(&source, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::read_dir(&source).is_err() {
            assert!(!probe(tmp.path()).status.success());
        }
        fs::set_permissions(&source, fs::Permissions::from_mode(0o755)).unwrap();
        put(
            tmp.path(),
            ".codebuddy/settings.json",
            r#"{"memory":{"autoMemoryEnabled":"false"}}"#,
        );
        assert!(inventory(&probe(tmp.path()).stdout).is_err());
        fs::remove_dir_all(tmp.path().join(".codebuddy/projects")).unwrap();
        symlink("missing", tmp.path().join(".codebuddy/projects")).unwrap();
        assert!(!probe(tmp.path()).status.success());
    }

    #[test]
    fn protocol_rejects_incomplete_duplicate_and_unsafe_records_and_ignores_banner() {
        for payload in [
            b"".as_slice(),
            b"WAKE_MEMORY_V1\0",
            b"WAKE_MEMORY_V1\0D\0E\0",
            b"WAKE_MEMORY_V1\0F\0.codebuddy/settings.json\0E\0",
            b"WAKE_MEMORY_V1\0D\0.codebuddy/projects/x/memory/../bad\0E\0",
            b"WAKE_MEMORY_V1\0D\0.codebuddy/memories\0D\0.codebuddy/memories\0E\0",
            b"WAKE_MEMORY_V1\0E\0unexpected",
        ] {
            assert!(inventory(payload).is_err());
        }
        let inv = inventory(b"login banner\nWAKE_MEMORY_V1\0E\0").unwrap();
        assert!(inv.dirs.is_empty());
    }
}
