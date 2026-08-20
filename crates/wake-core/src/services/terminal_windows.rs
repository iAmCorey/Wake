//! Windows implementations for session resume, shell integration and file actions.
//!
//! Keep this module separate from the macOS implementation.  Apart from making
//! the platform boundary explicit, this prevents AppleScript tools from being
//! pulled into a Windows build at all.

use crate::models::{AgentId, SessionMeta};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;

#[derive(Debug, Clone)]
pub struct ResumeOutcome {
    pub ok: bool,
    pub command: String,
    pub error: Option<String>,
}

static CLI_CACHE: Mutex<Option<HashMap<String, Option<String>>>> = Mutex::new(None);

fn resolve_clis(bins: &[&str]) -> HashMap<String, Option<String>> {
    let mut cache = CLI_CACHE.lock().unwrap();
    let map = cache.get_or_insert_with(HashMap::new);
    let missing: Vec<&str> = bins
        .iter()
        .filter(|bin| !map.contains_key(**bin))
        .copied()
        .collect();

    for bin in missing {
        let path = Command::new("where.exe")
            .arg(bin)
            .output()
            .ok()
            .filter(|output| output.status.success())
            .and_then(|output| {
                String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .map(str::trim)
                    .find(|line| !line.is_empty())
                    .map(ToOwned::to_owned)
            });
        map.insert(bin.to_string(), path);
    }

    bins.iter()
        .map(|bin| ((*bin).to_string(), map.get(*bin).cloned().flatten()))
        .collect()
}

pub fn cli_path(agent: AgentId) -> Option<String> {
    let bin = agent_bin(agent)?;
    resolve_clis(&[bin]).get(bin).cloned().flatten()
}

fn session_bin(meta: &SessionMeta) -> Option<&'static str> {
    if meta.agent == AgentId::Opencode && meta.source.as_deref() == Some("opencode2") {
        Some("opencode2")
    } else {
        agent_bin(meta.agent)
    }
}

pub fn agent_bin(agent: AgentId) -> Option<&'static str> {
    match agent {
        AgentId::ClaudeCode => Some("claude"),
        AgentId::Codex => Some("codex"),
        AgentId::Copilot => Some("copilot"),
        AgentId::Cursor => Some("cursor-agent"),
        AgentId::Opencode => Some("opencode"),
        AgentId::Kiro => Some("kiro"),
        AgentId::Gemini => Some("gemini"),
        AgentId::Pi => Some("pi"),
        AgentId::Omp => Some("omp"),
        AgentId::Grok => Some("grok"),
        AgentId::Kimi => Some("kimi"),
        AgentId::Antigravity => Some("agy"),
    }
}

fn resume_args(agent: AgentId, id: &str) -> Option<(Vec<String>, bool)> {
    match agent {
        AgentId::ClaudeCode => Some((vec!["--resume".into(), id.into()], true)),
        AgentId::Codex => Some((vec!["resume".into(), id.into()], false)),
        AgentId::Copilot => Some((vec![format!("--resume={id}")], false)),
        AgentId::Cursor => Some((vec!["--resume".into(), id.into()], false)),
        AgentId::Opencode => Some((vec!["--session".into(), id.into()], false)),
        AgentId::Pi => Some((vec!["--session".into(), id.into()], false)),
        AgentId::Omp => Some((vec!["--resume".into(), id.into()], false)),
        AgentId::Grok => Some((vec!["--resume".into(), id.into()], false)),
        AgentId::Kimi => Some((vec!["--session".into(), id.into()], false)),
        AgentId::Antigravity => Some((vec![format!("--conversation={id}")], false)),
        AgentId::Gemini | AgentId::Kiro => None,
    }
}

fn powershell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn cmd_quote(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\\\""))
}

fn command_line(cli: &str, args: &[String]) -> String {
    std::iter::once(powershell_quote(cli))
        .chain(args.iter().map(|arg| powershell_quote(arg)))
        .collect::<Vec<_>>()
        .join(" ")
}

fn powershell_resume(command: &str, cwd: Option<&Path>) -> anyhow::Result<()> {
    let mut child = Command::new("powershell.exe");
    child.args(["-NoLogo", "-NoProfile", "-NoExit", "-Command", command]);
    if let Some(cwd) = cwd {
        child.current_dir(cwd);
    }
    child.spawn()?;
    Ok(())
}

fn windows_terminal_resume(command: &str, cwd: Option<&Path>) -> anyhow::Result<()> {
    let mut child = Command::new("wt.exe");
    child.args(["-w", "new"]);
    if let Some(cwd) = cwd {
        child.arg("-d").arg(cwd);
    }
    child.args([
        "powershell.exe",
        "-NoLogo",
        "-NoProfile",
        "-NoExit",
        "-Command",
        command,
    ]);
    child.spawn()?;
    Ok(())
}

fn command_prompt_resume(command: &str, cwd: Option<&Path>) -> anyhow::Result<()> {
    let mut child = Command::new(std::env::var_os("COMSPEC").unwrap_or_else(|| "cmd.exe".into()));
    child.args(["/D", "/K", command]);
    if let Some(cwd) = cwd {
        child.current_dir(cwd);
    }
    child.spawn()?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalApp {
    WindowsTerminal,
    PowerShell,
    CommandPrompt,
}

impl TerminalApp {
    pub fn display_name(&self) -> &'static str {
        match self {
            Self::WindowsTerminal => "Windows Terminal",
            Self::PowerShell => "PowerShell",
            Self::CommandPrompt => "Command Prompt",
        }
    }

    pub fn id(&self) -> &'static str {
        match self {
            Self::WindowsTerminal => "windows-terminal",
            Self::PowerShell => "powershell",
            Self::CommandPrompt => "command-prompt",
        }
    }

    pub fn resolved_app_path(&self) -> Option<PathBuf> {
        match self {
            Self::WindowsTerminal => resolve_executable("wt.exe"),
            Self::PowerShell => {
                resolve_executable("pwsh.exe").or_else(|| resolve_executable("powershell.exe"))
            }
            Self::CommandPrompt => std::env::var_os("COMSPEC")
                .map(PathBuf::from)
                .filter(|path| path.is_file()),
        }
    }

    fn is_installed(&self) -> bool {
        self.resolved_app_path().is_some()
    }
}

fn resolve_executable(name: &str) -> Option<PathBuf> {
    Command::new("where.exe")
        .arg(name)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| {
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .map(str::trim)
                .find(|line| !line.is_empty())
                .map(PathBuf::from)
        })
        .filter(|path| path.is_file())
}

pub fn ensure_app_icons(cache_dir: &Path) -> HashMap<String, PathBuf> {
    // Windows does not need a platform API just to launch a terminal.  Keep
    // user-provided/cached images working and let the UI use its terminal icon
    // fallback for terminals whose shell icon is not cached.
    let _ = std::fs::create_dir_all(cache_dir);
    installed_terminals()
        .iter()
        .filter_map(|terminal| {
            let path = cache_dir.join(format!("{}.png", terminal.id()));
            path.is_file().then_some((terminal.id().to_string(), path))
        })
        .collect()
}

pub fn installed_terminals() -> &'static [TerminalApp] {
    use std::sync::OnceLock;
    static CACHE: OnceLock<Vec<TerminalApp>> = OnceLock::new();
    CACHE.get_or_init(|| {
        [
            TerminalApp::WindowsTerminal,
            TerminalApp::PowerShell,
            TerminalApp::CommandPrompt,
        ]
        .into_iter()
        .filter(|terminal| terminal.is_installed())
        .collect()
    })
}

pub fn resume_session_in(meta: &SessionMeta, term: TerminalApp) -> ResumeOutcome {
    let Some((args, requires_cwd)) = resume_args(meta.agent, &meta.id) else {
        return ResumeOutcome {
            ok: false,
            command: String::new(),
            error: Some(format!(
                "Resume isn't supported for {} yet",
                meta.agent.display_name()
            )),
        };
    };

    let Some(bin) = session_bin(meta) else {
        return ResumeOutcome {
            ok: false,
            command: String::new(),
            error: Some(format!(
                "No resume command is configured for {}",
                meta.agent.display_name()
            )),
        };
    };
    let Some(cli) = resolve_clis(&[bin]).get(bin).cloned().flatten() else {
        return ResumeOutcome {
            ok: false,
            command: String::new(),
            error: Some(format!("Command {bin} not found — is it installed?")),
        };
    };

    let cwd = (!meta.project_path.is_empty())
        .then(|| PathBuf::from(&meta.project_path))
        .filter(|path| path.is_dir());
    let command = if matches!(term, TerminalApp::CommandPrompt) {
        std::iter::once(cmd_quote(&cli))
            .chain(args.iter().map(|arg| cmd_quote(arg)))
            .collect::<Vec<_>>()
            .join(" ")
    } else {
        command_line(&cli, &args)
    };
    if requires_cwd && cwd.is_none() {
        copy_to_clipboard(&command);
        return ResumeOutcome {
            ok: false,
            command,
            error: Some(format!(
                "Project directory no longer exists: {} (command copied — run it manually)",
                meta.project_path
            )),
        };
    }

    let result = match term {
        TerminalApp::WindowsTerminal => windows_terminal_resume(&command, cwd.as_deref()),
        TerminalApp::PowerShell => powershell_resume(&command, cwd.as_deref()),
        TerminalApp::CommandPrompt => command_prompt_resume(&command, cwd.as_deref()),
    };

    match result {
        Ok(()) => ResumeOutcome {
            ok: true,
            command,
            error: None,
        },
        Err(error) => {
            copy_to_clipboard(&command);
            ResumeOutcome {
                ok: false,
                command,
                error: Some(format!(
                    "Couldn't open terminal ({error}). Command copied to clipboard — paste to run."
                )),
            }
        }
    }
}

fn copy_to_clipboard(text: &str) {
    if let Ok(mut child) = Command::new("clip.exe").stdin(Stdio::piped()).spawn() {
        if let Some(stdin) = child.stdin.as_mut() {
            let _ = stdin.write_all(text.as_bytes());
        }
        let _ = child.wait();
    }
}

fn powershell_script_quote(value: &str) -> String {
    powershell_quote(value)
}

pub fn trash_paths(paths: &[String]) -> anyhow::Result<()> {
    for raw in paths {
        let path = Path::new(raw);
        if !path.exists() {
            continue;
        }
        let quoted = powershell_script_quote(raw);
        let script = if path.is_dir() {
            format!(
                "Add-Type -AssemblyName Microsoft.VisualBasic; [Microsoft.VisualBasic.FileIO.FileSystem]::DeleteDirectory({quoted}, 'OnlyErrorDialogs', 'SendToRecycleBin')"
            )
        } else {
            format!(
                "Add-Type -AssemblyName Microsoft.VisualBasic; [Microsoft.VisualBasic.FileIO.FileSystem]::DeleteFile({quoted}, 'OnlyErrorDialogs', 'SendToRecycleBin')"
            )
        };
        let output = Command::new("powershell.exe")
            .args([
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                &script,
            ])
            .output()?;
        if !output.status.success() {
            anyhow::bail!(
                "Failed to move to Recycle Bin: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
    }
    Ok(())
}

pub fn show_fatal_alert(message: &str) {
    eprintln!("[wake] fatal: {message}");
    let script = format!(
        "Add-Type -AssemblyName PresentationFramework; [System.Windows.MessageBox]::Show({}, 'Wake cannot start', 'OK', 'Error') | Out-Null",
        powershell_quote(&message.replace(['\n', '\r'], " "))
    );
    let _ = Command::new("powershell.exe")
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &script,
        ])
        .status();
}

pub fn reveal_in_finder(path: &str) {
    let path = if Path::new(path).exists() {
        path
    } else {
        path.rsplit_once('#').map(|(db, _)| db).unwrap_or(path)
    };
    let mut command = Command::new("explorer.exe");
    if Path::new(path).is_file() {
        let _ = command.arg(format!("/select,{}", path)).status();
    } else {
        let _ = command.arg(path).status();
    }
}
