use crate::models::{AgentId, SessionMeta};
use std::collections::HashMap;
use std::path::Path;
use std::process::Command;
use std::sync::Mutex;

#[derive(Debug, Clone)]
pub struct ResumeOutcome {
    pub ok: bool,
    pub command: String,
    pub error: Option<String>,
}

/// GUI 进程 PATH 不含 ~/.local/bin 等,经 login shell 批量解析并缓存
static CLI_CACHE: Mutex<Option<HashMap<String, Option<String>>>> = Mutex::new(None);

fn resolve_clis(bins: &[&str]) -> HashMap<String, Option<String>> {
    let mut cache = CLI_CACHE.lock().unwrap();
    let map = cache.get_or_insert_with(HashMap::new);
    let missing: Vec<&str> = bins.iter().filter(|b| !map.contains_key(**b)).copied().collect();
    if !missing.is_empty() {
        let script = missing
            .iter()
            .map(|b| format!("printf '%s\\t' {b}; command -v {b} || echo"))
            .collect::<Vec<_>>()
            .join("; ");
        let out = Command::new("/bin/zsh").args(["-lic", &script]).output();
        let stdout = out
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
            .unwrap_or_default();
        let mut found: HashMap<&str, String> = HashMap::new();
        for line in stdout.lines() {
            if let Some((name, path)) = line.split_once('\t') {
                if path.starts_with('/') {
                    found.insert(name.trim(), path.trim().to_string());
                }
            }
        }
        for b in missing {
            map.insert(b.to_string(), found.get(b).cloned());
        }
    }
    bins.iter()
        .map(|b| (b.to_string(), map.get(*b).cloned().flatten()))
        .collect()
}

pub fn cli_path(agent: AgentId) -> Option<String> {
    let bin = agent_bin(agent)?;
    resolve_clis(&[bin]).get(bin).cloned().flatten()
}

/// 会话级二进制:OpenCode v2 beta 与 v1 并存、装为 `opencode2`,
/// v2 会话(source = "opencode2")必须由它恢复,其余会话走 agent 默认 bin
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
        // 参数形制与 kooky 的 resume 集成一致(空格/等号是各家 CLI 实测约束)
        // OpenCode 两代 CLI 同为 --session;v2 会话由 session_bin 换 opencode2
        AgentId::Opencode => Some((vec!["--session".into(), id.into()], false)),
        AgentId::Pi => Some((vec!["--session".into(), id.into()], false)),
        AgentId::Omp => Some((vec!["--resume".into(), id.into()], false)),
        AgentId::Grok => Some((vec!["--resume".into(), id.into()], false)),
        AgentId::Kimi => Some((vec!["--session".into(), id.into()], false)),
        AgentId::Antigravity => Some((vec![format!("--conversation={id}")], false)),
        _ => None,
    }
}

/// POSIX 单引号 quote
pub fn posix_quote(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "_-./:=".contains(c))
    {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// AppleScript 双引号字符串转义
pub fn applescript_quote(s: &str) -> String {
    s.replace('\\', r"\\").replace('"', r#"\""#)
}

fn osascript(lines: &[String]) -> std::io::Result<std::process::Output> {
    let mut cmd = Command::new("osascript");
    for l in lines {
        cmd.args(["-e", l]);
    }
    cmd.output()
}

/// Open In 下拉的目标终端。只列本机实际安装且可程序化注入命令的
/// (Tabby 等无稳定命令接口的不做)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalApp {
    Terminal,
    ITerm,
    Warp,
    Ghostty,
    /// 用户自己的 agent 宿主 app。暂无外部接口(无 URL scheme、不收 open
    /// 参数),只能激活应用,不能带会话跳转。
    Kooky,
}

impl TerminalApp {
    pub fn display_name(&self) -> &'static str {
        match self {
            TerminalApp::Terminal => "Terminal",
            TerminalApp::ITerm => "iTerm",
            TerminalApp::Warp => "Warp",
            TerminalApp::Ghostty => "Ghostty",
            TerminalApp::Kooky => "Kooky",
        }
    }

    /// 稳定短 id(图标缓存文件名、last-used 记忆用)
    pub fn id(&self) -> &'static str {
        match self {
            TerminalApp::Terminal => "terminal",
            TerminalApp::ITerm => "iterm",
            TerminalApp::Warp => "warp",
            TerminalApp::Ghostty => "ghostty",
            TerminalApp::Kooky => "kooky",
        }
    }

    /// 已安装的 .app 绝对路径(首个命中)
    pub fn resolved_app_path(&self) -> Option<std::path::PathBuf> {
        let candidates: &[&str] = match self {
            TerminalApp::Terminal => &["/System/Applications/Utilities/Terminal.app"],
            TerminalApp::ITerm => &["/Applications/iTerm.app"],
            TerminalApp::Warp => &["/Applications/Warp.app"],
            TerminalApp::Ghostty => &["/Applications/Ghostty.app"],
            TerminalApp::Kooky => &["/Applications/Kooky.app"],
        };
        for p in candidates {
            let p = std::path::PathBuf::from(p);
            if p.is_dir() {
                return Some(p);
            }
        }
        let home_app = crate::home_dir()
            .unwrap_or_default()
            .join("Applications")
            .join(format!("{}.app", self.display_name()));
        home_app.is_dir().then_some(home_app)
    }

    fn is_installed(&self) -> bool {
        self.resolved_app_path().is_some()
    }

    /// 目标消费什么:shell 命令串,还是会话深链。新增非 shell 目标时
    /// 在此声明,resume_session_in 无需再加旁路。
    fn launch_mode(&self) -> Launch {
        match self {
            TerminalApp::Terminal => Launch::Shell(launch_terminal_app),
            TerminalApp::ITerm => Launch::Shell(launch_iterm),
            TerminalApp::Warp => Launch::Shell(launch_warp),
            TerminalApp::Ghostty => Launch::Shell(launch_ghostty),
            TerminalApp::Kooky => Launch::DeepLink,
        }
    }
}

enum Launch {
    /// 在该终端里执行拼好的 shell 命令
    Shell(fn(&str) -> anyhow::Result<()>),
    /// 走会话深链(kooky://),不经过 agent CLI
    DeepLink,
}

/// 确保已装终端的应用图标提取到 `cache_dir/<id>.png`(64px),返回 id → 路径。
/// JXA 走 NSWorkspace.iconForFile,icns/Assets.car 格式都拿得到;已存在直接
/// 复用。阻塞数百 ms,调用方放后台线程。
pub fn ensure_app_icons(cache_dir: &Path) -> HashMap<String, std::path::PathBuf> {
    let _ = std::fs::create_dir_all(cache_dir);
    let mut out = HashMap::new();
    let mut jobs: Vec<(TerminalApp, std::path::PathBuf)> = Vec::new();
    for t in installed_terminals() {
        let png = cache_dir.join(format!("{}.png", t.id()));
        if png.is_file() {
            out.insert(t.id().to_string(), png);
        } else {
            jobs.push((*t, png));
        }
    }
    if jobs.is_empty() {
        return out;
    }
    let mut script =
        String::from("ObjC.import('AppKit');\nconst ws = $.NSWorkspace.sharedWorkspace;\n");
    for (t, png) in &jobs {
        let Some(app) = t.resolved_app_path() else { continue };
        script.push_str(&format!(
            "{{ const i = ws.iconForFile('{app}'); const rep = $.NSBitmapImageRep.imageRepWithData(i.TIFFRepresentation); \
             const png = rep.representationUsingTypeProperties(4, $.NSDictionary.dictionary); \
             png.writeToFileAtomically('{out}', true); }}\n",
            app = app.display(),
            out = png.display(),
        ));
    }
    let ok = Command::new("osascript")
        .args(["-l", "JavaScript", "-e", &script])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if ok {
        for (t, png) in &jobs {
            if png.is_file() {
                // NSWorkspace 给的是 1024px,缩到 64 省内存;缩失败原图也能用
                let _ = Command::new("sips").args(["-Z", "64"]).arg(png).output();
                out.insert(t.id().to_string(), png.clone());
            }
        }
    }
    out
}

/// 已安装终端(启动后不变,进程内缓存)
pub fn installed_terminals() -> &'static [TerminalApp] {
    use std::sync::OnceLock;
    static CACHE: OnceLock<Vec<TerminalApp>> = OnceLock::new();
    CACHE.get_or_init(|| {
        [
            TerminalApp::Terminal,
            TerminalApp::Kooky,
            TerminalApp::ITerm,
            TerminalApp::Warp,
            TerminalApp::Ghostty,
        ]
        .into_iter()
        .filter(|t| t.is_installed())
        .collect()
    })
}

fn launch_terminal_app(command: &str) -> anyhow::Result<()> {
    let esc = applescript_quote(command);
    let out = osascript(&[
        "tell application \"Terminal\" to activate".to_string(),
        format!("tell application \"Terminal\" to do script \"{esc}\""),
    ])?;
    if !out.status.success() {
        anyhow::bail!("{}", String::from_utf8_lossy(&out.stderr));
    }
    Ok(())
}

fn launch_iterm(command: &str) -> anyhow::Result<()> {
    let esc = applescript_quote(command);
    let out = osascript(&[
        "tell application \"iTerm\" to activate".to_string(),
        "tell application \"iTerm\" to create window with default profile".to_string(),
        format!("tell current session of current window of application \"iTerm\" to write text \"{esc}\""),
    ])?;
    if !out.status.success() {
        anyhow::bail!("{}", String::from_utf8_lossy(&out.stderr));
    }
    Ok(())
}

/// Ghostty:open --args 直传 argv,无二次 shell 解析;命令跑完 exec 交互
/// shell 保持窗口
fn launch_ghostty(command: &str) -> anyhow::Result<()> {
    let status = Command::new("open")
        .args([
            "-na",
            "Ghostty",
            "--args",
            "-e",
            "/bin/zsh",
            "-lic",
            &format!("{command}; exec /bin/zsh -il"),
        ])
        .status()?;
    if !status.success() {
        anyhow::bail!("open -na Ghostty failed");
    }
    Ok(())
}

/// Warp 无命令注入 CLI,走官方 Launch Configuration:写临时 yaml 再
/// open warp://launch/<path>。exec 用 yaml 块标量,免转义。
fn launch_warp(command: &str) -> anyhow::Result<()> {
    if command.contains('\n') {
        anyhow::bail!("multi-line command");
    }
    let yaml = format!(
        "name: Wake Resume\nwindows:\n  - tabs:\n      - layout:\n          commands:\n            - exec: |-\n                {command}\n"
    );
    let path = std::env::temp_dir().join("wake-warp-resume.yaml");
    std::fs::write(&path, yaml)?;
    let status = Command::new("open")
        .arg(format!("warp://launch/{}", path.display()))
        .status()?;
    if !status.success() {
        anyhow::bail!("open warp:// failed");
    }
    Ok(())
}

fn copy_to_clipboard(text: &str) {
    use std::io::Write;
    if let Ok(mut child) = Command::new("pbcopy").stdin(std::process::Stdio::piped()).spawn() {
        if let Some(stdin) = child.stdin.as_mut() {
            let _ = stdin.write_all(text.as_bytes());
        }
        let _ = child.wait();
    }
}

/// query 值的保守 percent-encode(unreserved 之外全编)
fn percent_encode_query(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Kooky 深链恢复(kooky://resume,约定见 kooky 的 DeepLink.swift):
/// id 校验与 kooky 端同规则;深链失败(未发布的旧版没注册 scheme)退回激活应用。
/// 不依赖 agent CLI——kooky 自己会起 agent。
fn launch_kooky(meta: &SessionMeta) -> ResumeOutcome {
    let id_ok = meta.id.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
        && meta.id.len() <= 200
        && meta
            .id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'));
    if id_ok {
        let mut url = format!("kooky://resume?agent={}&id={}", meta.agent.as_str(), meta.id);
        if !meta.project_path.is_empty() && Path::new(&meta.project_path).is_dir() {
            url.push_str("&cwd=");
            url.push_str(&percent_encode_query(&meta.project_path));
        }
        let deep_ok = Command::new("open")
            .arg(&url)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if deep_ok {
            return ResumeOutcome { ok: true, command: url, error: None };
        }
    }
    let ok = Command::new("open")
        .args(["-a", "Kooky"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    ResumeOutcome {
        ok,
        command: "open -a Kooky (deep link unavailable in this Kooky build)".into(),
        error: (!ok).then(|| "Couldn't open Kooky".to_string()),
    }
}

pub fn resume_session_in(meta: &SessionMeta, term: TerminalApp) -> ResumeOutcome {
    // 深链目标不走 shell 命令构建(不依赖 agent CLI)
    let Launch::Shell(run) = term.launch_mode() else {
        return launch_kooky(meta);
    };
    let Some((args, requires_cwd)) = resume_args(meta.agent, &meta.id) else {
        return ResumeOutcome {
            ok: false,
            command: String::new(),
            error: Some(format!("Resume isn't supported for {} yet", meta.agent.display_name())),
        };
    };
    let bin = session_bin(meta);
    let Some(cli) = bin.and_then(|b| resolve_clis(&[b]).get(b).cloned().flatten()) else {
        return ResumeOutcome {
            ok: false,
            command: String::new(),
            error: Some(format!(
                "Command {} not found — is it installed?",
                bin.unwrap_or("?")
            )),
        };
    };
    let cwd_ok = !meta.project_path.is_empty() && Path::new(&meta.project_path).is_dir();
    let core = std::iter::once(cli.as_str())
        .chain(args.iter().map(|s| s.as_str()))
        .map(posix_quote)
        .collect::<Vec<_>>()
        .join(" ");
    let command = if cwd_ok {
        format!("cd {} && {core}", posix_quote(&meta.project_path))
    } else {
        core
    };
    if requires_cwd && !cwd_ok {
        copy_to_clipboard(&command);
        return ResumeOutcome {
            ok: false,
            command,
            error: Some(format!("Project directory no longer exists: {} (command copied — run it manually)", meta.project_path)),
        };
    }

    let result = run(&command);
    match result {
        Ok(()) => ResumeOutcome {
            ok: true,
            command,
            error: None,
        },
        Err(e) => {
            copy_to_clipboard(&command);
            ResumeOutcome {
                ok: false,
                command,
                error: Some(format!("Couldn't open terminal ({e}). Command copied to clipboard — paste to run.")),
            }
        }
    }
}

/// 删除会话文件到废纸篓(macOS 用 Finder,可从废纸篓恢复)
pub fn trash_paths(paths: &[String]) -> anyhow::Result<()> {
    for p in paths {
        if !Path::new(p).exists() {
            continue;
        }
        let esc = applescript_quote(p);
        let out = osascript(&[format!(
            "tell application \"Finder\" to delete (POSIX file \"{esc}\" as alias)"
        )])?;
        if !out.status.success() {
            anyhow::bail!("Failed to move to Trash: {}", String::from_utf8_lossy(&out.stderr));
        }
    }
    Ok(())
}

/// 致命错误提示。GPUI 窗口还没起来时这是唯一能让用户看见的通道,
/// 否则应用就是无提示秒退。换行先压平——AppleScript 字符串字面量不能跨行。
pub fn show_fatal_alert(message: &str) {
    eprintln!("[wake] fatal: {message}");
    let flat = message.replace(['\n', '\r'], " ");
    let _ = osascript(&[format!(
        "display alert \"Wake can't start\" message \"{}\" as critical",
        applescript_quote(&flat)
    )]);
}

pub fn reveal_in_finder(path: &str) {
    // SQLite 型会话是 <db>#<id> 虚拟路径,磁盘上不存在——reveal 库文件本体
    let p = if Path::new(path).exists() {
        path
    } else {
        path.rsplit_once('#').map(|(db, _)| db).unwrap_or(path)
    };
    let _ = Command::new("open").args(["-R", p]).status();
}
