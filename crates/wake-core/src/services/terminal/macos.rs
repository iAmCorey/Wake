//! macOS 平台原语:AppleScript/`open` 驱动的终端与 Finder、Finder 废纸篓、
//! Kooky 深链。接口与 linux.rs 同形,策略(dir/file 分发、路径过滤、
//! 线程化)在 mod.rs,这里只做动作本身。

use super::{
    percent_encode, pipe_to, posix_quote, resolve_cli, resume_args, session_bin, spawn_and_reap,
    ResumeOutcome,
};
use crate::models::{AgentId, SessionMeta};
use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

/// AppleScript 双引号字符串转义
fn applescript_quote(s: &str) -> String {
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
        let home_app = dirs::home_dir()
            .unwrap_or_default()
            .join("Applications")
            .join(format!("{}.app", self.display_name()));
        home_app.is_dir().then_some(home_app)
    }

    fn is_installed(&self) -> bool {
        self.resolved_app_path().is_some()
    }
}

/// 深链类目标整锅接管 resume(不经 agent CLI)。Kooky 是本平台唯一一例;
/// 新增非 shell 目标在此声明,mod.rs 的 resume_session_in 无需加旁路。
pub(super) fn deep_link_resume(meta: &SessionMeta, term: TerminalApp) -> Option<ResumeOutcome> {
    (term == TerminalApp::Kooky).then(|| launch_kooky(meta))
}

/// Shell 类目标的命令注入分发(深链目标已被 deep_link_resume 拦下,
/// Kooky 臂只是护栏)
pub(super) fn launch_shell(term: TerminalApp, command: &str) -> anyhow::Result<()> {
    match term {
        TerminalApp::Terminal => launch_terminal_app(command),
        TerminalApp::ITerm => launch_iterm(command),
        TerminalApp::Warp => launch_warp(command),
        TerminalApp::Ghostty => launch_ghostty(command),
        TerminalApp::Kooky => anyhow::bail!("Kooky is a deep-link target"),
    }
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
        let Some(app) = t.resolved_app_path() else {
            continue;
        };
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

/// kooky-cli 的稳定副本(kooky ≥0.51 每次启动刷新;bundle 内那份会被
/// Gatekeeper 拦,kooky README 指定外部工具用这份)。存在与否同时是
/// "这台机器的 kooky 会不会说 CLI"的探测——旧版没有它。
/// 路径构造缓存(terminals_for 挂在详情页 render 上,每帧问一次),但
/// **存在性每次现查**:副本随 kooky 启动刷新,把 None 缓存死会让用户装/升级
/// kooky 后仍不列 Kooky,直到重启 Wake。一次 stat 比这个代价便宜
fn kooky_cli_path() -> Option<&'static Path> {
    use std::sync::OnceLock;
    static PATH: OnceLock<Option<std::path::PathBuf>> = OnceLock::new();
    let p = PATH
        .get_or_init(|| {
            Some(dirs::home_dir()?.join("Library/Application Support/kooky/bin/kooky-cli"))
        })
        .as_deref()?;
    p.is_file().then_some(p)
}

/// kooky 深链能点名的 agent —— 即 kooky 的 `AgentSessionScanner.supportedAgentIds`
/// (AgentSessionHistory.swift 的 stores 表),`resumeSession` 拿它做 guard。
/// **名单外的 agent 会被拒**(unknown agent),而 `open kooky://…` 只要 scheme
/// 注册就退 0,Wake 这边看不出失败——所以名单外必须改走 kooky-cli 的哑管道。
/// 注意 Antigravity 不在里面:kooky 的 stores 表注释写明其 CLI 数据布局未经
/// 核实、"Absent by design"。新增 agent 时按 kooky 那张表核对,别默认落进深链
const KOOKY_ROSTER: &[AgentId] = &[
    AgentId::ClaudeCode,
    AgentId::Codex,
    AgentId::Copilot,
    AgentId::Cursor,
    AgentId::Opencode,
    AgentId::Kiro,
    AgentId::Gemini,
    AgentId::Pi,
    AgentId::Omp,
    AgentId::Grok,
    AgentId::Kimi,
];

/// Kooky 能否打开该 agent 的会话:roster 内走 resume 深链(旧版 kooky 也认,
/// 不依赖 CLI 副本);名单外靠 kooky-cli 传命令文本,没有 CLI 副本就不列 Kooky
fn kooky_speaks(agent: AgentId) -> bool {
    KOOKY_ROSTER.contains(&agent) || kooky_cli_path().is_some()
}

/// 某会话可用的恢复目标(Kooky 按 agent 过滤,见 kooky_speaks)
pub fn terminals_for(agent: AgentId) -> Vec<TerminalApp> {
    installed_terminals()
        .iter()
        .copied()
        .filter(|t| *t != TerminalApp::Kooky || kooky_speaks(agent))
        .collect()
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

pub(super) fn copy_to_clipboard(text: &str) -> bool {
    pipe_to("pbcopy", &[], text)
}

/// KOOKY_ROSTER 之外的 agent 在 kooky 里点名会被拒——改走 kooky-cli 的哑管道:
/// `open --cwd <项目> -e "<命令>"`,与 Terminal/iTerm 收命令文本同级。
/// 命令从 session_bin + resume_args 拼(与真终端路径同一来源),cd 由 --cwd 承担
fn launch_kooky_cli(meta: &SessionMeta) -> ResumeOutcome {
    let fail = |command: String, error: String| ResumeOutcome {
        ok: false,
        command,
        error: Some(error),
    };
    let Some(cli) = kooky_cli_path() else {
        return fail(
            String::new(),
            "kooky-cli not found — update Kooky to 0.51+".into(),
        );
    };
    let Some((bin, (args, _))) = session_bin(meta).zip(resume_args(meta.agent, &meta.id)) else {
        return fail(
            String::new(),
            format!(
                "Resume isn't supported for {} yet",
                meta.agent.display_name()
            ),
        );
    };
    // agent CLI 走与真终端路径同一次解析(GUI 进程 PATH 不全,过 login shell),
    // 顺带承担"装没装"的判断:少了这步,kooky 会开出一个只写着 command not
    // found 的 tab,而 Wake 这边照报成功
    let Some(exe) = resolve_cli(bin) else {
        return fail(
            String::new(),
            format!(
                "CLI `{bin}` (for {}) not found in shell PATH — is it installed?",
                meta.agent.display_name()
            ),
        );
    };
    let cmd = std::iter::once(exe.as_str())
        .chain(args.iter().map(|s| s.as_str()))
        .map(posix_quote)
        .collect::<Vec<_>>()
        .join(" ");
    let shown = format!(
        "kooky-cli open --cwd {} -e {}",
        posix_quote(&meta.project_path),
        posix_quote(&cmd)
    );
    // 手动兜底的是能直接粘进任何终端跑的那条(kooky 的 --cwd 得自己 cd 回来)
    let cwd_ok = !meta.project_path.is_empty() && Path::new(&meta.project_path).is_dir();
    let manual = if cwd_ok {
        format!("cd {} && {cmd}", posix_quote(&meta.project_path))
    } else {
        cmd.clone()
    };
    let bail = |reason: String| {
        fail(
            shown.clone(),
            format!("{reason}. {}", super::clipboard_fallback(&manual)),
        )
    };
    // 项目挪走了 kooky 的 --cwd 就没法落地。走同一条兜底,别让 Kooky 目标
    // 比 Terminal 少一份可手动执行的命令
    if !cwd_ok {
        return bail(format!(
            "Project directory no longer exists: {}",
            meta.project_path
        ));
    }
    match Command::new(cli)
        .args(["open", "--cwd", &meta.project_path, "-e", &cmd])
        .output()
    {
        Ok(out) if out.status.success() => ResumeOutcome {
            ok: true,
            command: shown,
            error: None,
        },
        Ok(out) => {
            let reason = String::from_utf8_lossy(&out.stderr).trim().to_string();
            bail(if reason.is_empty() {
                "kooky-cli refused the request".into()
            } else {
                reason
            })
        }
        Err(e) => bail(format!("Couldn't run kooky-cli: {e}")),
    }
}

/// Kooky 深链恢复(kooky://resume,约定见 kooky 的 DeepLink.swift):
/// id 校验与 kooky 端同规则;深链失败(未发布的旧版没注册 scheme)退回激活应用。
/// 不依赖 agent CLI——kooky 自己会起 agent。
fn launch_kooky(meta: &SessionMeta) -> ResumeOutcome {
    if !KOOKY_ROSTER.contains(&meta.agent) {
        return launch_kooky_cli(meta);
    }
    let id_ok = meta
        .id
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphanumeric())
        && meta.id.len() <= 200
        && meta
            .id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'));
    if id_ok {
        let mut url = format!(
            "kooky://resume?agent={}&id={}",
            meta.agent.as_str(),
            meta.id
        );
        if !meta.project_path.is_empty() && Path::new(&meta.project_path).is_dir() {
            url.push_str("&cwd=");
            url.push_str(&percent_encode(&meta.project_path, false));
        }
        let deep_ok = Command::new("open")
            .arg(&url)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if deep_ok {
            return ResumeOutcome {
                ok: true,
                command: url,
                error: None,
            };
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

/// 逐文件让 Finder 删进废纸篓(osascript 首次触发自动化授权会停等用户
/// 点选,可能长阻塞;首错即断,已删的留在废纸篓可恢复)
pub(super) fn trash_existing(paths: &[&str]) -> anyhow::Result<()> {
    for p in paths {
        let esc = applescript_quote(p);
        let out = osascript(&[format!(
            "tell application \"Finder\" to delete (POSIX file \"{esc}\" as alias)"
        )])?;
        if !out.status.success() {
            anyhow::bail!(
                "Failed to move to Trash: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }
    Ok(())
}

/// 致命错误对话框。换行先压平——AppleScript 字符串字面量不能跨行。
pub(super) fn alert_dialog(message: &str) {
    let flat = message.replace(['\n', '\r'], " ");
    let _ = osascript(&[format!(
        "display alert \"Wake can't start\" message \"{}\" as critical",
        applescript_quote(&flat)
    )]);
}

/// 在 Finder 里进入目录
pub(super) fn open_dir(path: &str) {
    let mut cmd = Command::new("open");
    cmd.arg(path);
    let _ = spawn_and_reap(cmd);
}

/// 在 Finder 里选中文件(收 mod.rs 已剥好虚拟后缀的真实路径)
pub(super) fn reveal_path(path: &str) {
    let mut cmd = Command::new("open");
    cmd.args(["-R", path]);
    let _ = spawn_and_reap(cmd);
}
