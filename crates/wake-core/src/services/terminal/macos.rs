//! macOS 平台原语:AppleScript/`open` 驱动的终端与 Finder、Finder 废纸篓、
//! 深链目标(Kooky、Claude Desktop、Codex Desktop)。接口与 linux.rs 同形,
//! 策略(dir/file 分发、路径过滤、线程化)在 mod.rs,这里只做动作本身。

use super::{
    cli_not_found_error, percent_encode, pipe_to, resolve_cli, resume_args, session_bin, sh_quote,
    spawn_and_reap, ResumeOutcome,
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

/// Open In 下拉的目标。shell 类只列本机实际安装且可程序化注入命令的
/// (Tabby 等无稳定命令接口的不做);深链类(Kooky/两家 desktop)不跑
/// shell,由 deep_link_resume 整锅接管。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalApp {
    Terminal,
    ITerm,
    Warp,
    Ghostty,
    /// 用户自己的 agent 宿主 app:roster 内走 kooky://resume 深链,
    /// 名单外经 kooky-cli 哑管道传命令文本(见 launch_kooky)。
    Kooky,
    /// Claude Desktop 内嵌的 Claude Code:claude://resume?session=<uuid>
    /// 把磁盘上的 CLI transcript 导入 desktop 会话并聚焦(实测 2026-09-01,
    /// 处理端是 app.asar 的 claudeURLHandler,id 校验为裸 UUID)。
    ClaudeDesktop,
    /// Codex 桌面版(app 名叫 ChatGPT.app,bundle id com.openai.codex):
    /// codex://threads/<uuid> 打开本地会话(路由 kind "localConversation",
    /// desktop 的 threads 表与 CLI 共库、行内 rollout_path 即 CLI 文件)。
    CodexDesktop,
}

impl TerminalApp {
    pub fn display_name(&self) -> &'static str {
        match self {
            TerminalApp::Terminal => "Terminal",
            TerminalApp::ITerm => "iTerm",
            TerminalApp::Warp => "Warp",
            TerminalApp::Ghostty => "Ghostty",
            TerminalApp::Kooky => "Kooky",
            TerminalApp::ClaudeDesktop => "Claude Desktop",
            TerminalApp::CodexDesktop => "Codex Desktop",
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
            TerminalApp::ClaudeDesktop => "claude-desktop",
            TerminalApp::CodexDesktop => "codex-desktop",
        }
    }

    /// 内嵌品牌图标覆盖:Codex desktop 的 .app 图标是 ChatGPT 的(留白也
    /// 比别家多,渲染出来偏小),Open In 里应与 Codex agent 图标一致,
    /// 直接用内嵌 brands 资源,不走 ensure_app_icons 的提取缓存
    pub fn brand_icon(&self) -> Option<&'static str> {
        match self {
            TerminalApp::CodexDesktop => Some("brands/codex.png"),
            _ => None,
        }
    }

    /// 已安装的 .app 绝对路径(首个命中;/Applications 之外再试
    /// ~/Applications 下同名 bundle)
    pub fn resolved_app_path(&self) -> Option<std::path::PathBuf> {
        let candidates: &[&str] = match self {
            TerminalApp::Terminal => &["/System/Applications/Utilities/Terminal.app"],
            TerminalApp::ITerm => &["/Applications/iTerm.app"],
            TerminalApp::Warp => &["/Applications/Warp.app"],
            TerminalApp::Ghostty => &["/Applications/Ghostty.app"],
            TerminalApp::Kooky => &["/Applications/Kooky.app"],
            TerminalApp::ClaudeDesktop => &["/Applications/Claude.app"],
            // Codex desktop 的 app 名与旧版 ChatGPT 同名,靠 bundle id 区分
            TerminalApp::CodexDesktop => &["/Applications/ChatGPT.app"],
        };
        let home = dirs::home_dir().unwrap_or_default().join("Applications");
        for c in candidates {
            let sys = std::path::PathBuf::from(c);
            let alt = std::path::Path::new(c).file_name().map(|n| home.join(n));
            for p in std::iter::once(sys).chain(alt) {
                if p.is_dir() && self.bundle_marker_ok(&p) {
                    return Some(p);
                }
            }
        }
        None
    }

    /// display 名不足以认 app 的变体按 bundle id 验明正身(Info.plist 里
    /// bundle id 是明文 ASCII,XML/二进制两种 plist 都直接搜得到):
    /// ChatGPT.app 只有 Codex 版(com.openai.codex)才注册 codex:// scheme,
    /// 旧版 ChatGPT 同名不同物;Claude.app 同理防重名壳。一次性探测
    /// (installed_terminals 进程内缓存),读几 KB plist 无感。
    fn bundle_marker_ok(&self, app: &Path) -> bool {
        let marker: &[u8] = match self {
            TerminalApp::ClaudeDesktop => b"com.anthropic.claudefordesktop",
            TerminalApp::CodexDesktop => b"com.openai.codex",
            _ => return true,
        };
        std::fs::read(app.join("Contents/Info.plist"))
            .map(|data| data.windows(marker.len()).any(|w| w == marker))
            .unwrap_or(false)
    }

    fn is_installed(&self) -> bool {
        self.resolved_app_path().is_some()
    }
}

/// 深链类目标整锅接管 resume(不经 agent CLI)。新增非 shell 目标在此
/// 声明,mod.rs 的 resume_session_in 无需加旁路。
pub(super) fn deep_link_resume(meta: &SessionMeta, term: TerminalApp) -> Option<ResumeOutcome> {
    match term {
        TerminalApp::Kooky => Some(launch_kooky(meta)),
        TerminalApp::ClaudeDesktop => Some(launch_claude_desktop(meta)),
        TerminalApp::CodexDesktop => Some(launch_desktop_id(&meta.id, "codex://threads/", term)),
        _ => None,
    }
}

/// Shell 类目标的命令注入分发(深链目标已被 deep_link_resume 拦下,
/// 深链臂只是护栏)
pub(super) fn launch_shell(term: TerminalApp, command: &str) -> anyhow::Result<()> {
    match term {
        TerminalApp::Terminal => launch_terminal_app(command),
        TerminalApp::ITerm => launch_iterm(command),
        TerminalApp::Warp => launch_warp(command),
        TerminalApp::Ghostty => launch_ghostty(command),
        TerminalApp::Kooky | TerminalApp::ClaudeDesktop | TerminalApp::CodexDesktop => {
            anyhow::bail!("{} is a deep-link target", term.display_name())
        }
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

/// 某会话可用的恢复目标(Kooky 按 agent 过滤见 kooky_speaks;两家 desktop
/// 只认自家 agent 的会话——深链 id 命名空间互不相通)
pub fn terminals_for(agent: AgentId) -> Vec<TerminalApp> {
    installed_terminals()
        .iter()
        .copied()
        .filter(|t| match t {
            TerminalApp::Kooky => kooky_speaks(agent),
            TerminalApp::ClaudeDesktop => agent == AgentId::ClaudeCode,
            TerminalApp::CodexDesktop => agent == AgentId::Codex,
            _ => true,
        })
        .collect()
}

/// 已安装终端(启动后不变,进程内缓存)。首位是 terminals_for 的回退值
/// (偏好被会话过滤时左段按钮显示它),恒为 Terminal——desktop 目标排在
/// Kooky 侧属"宿主 app"一档,不抢默认。
pub fn installed_terminals() -> &'static [TerminalApp] {
    use std::sync::OnceLock;
    static CACHE: OnceLock<Vec<TerminalApp>> = OnceLock::new();
    CACHE.get_or_init(|| {
        [
            TerminalApp::Terminal,
            TerminalApp::Kooky,
            TerminalApp::ClaudeDesktop,
            TerminalApp::CodexDesktop,
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
    let Some((bin, (args, _))) = session_bin(meta).zip(resume_args(meta)) else {
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
        return fail(String::new(), cli_not_found_error(meta.agent, bin));
    };
    let cmd = super::sh_command_line(exe.as_str(), &args, None);
    let shown = format!(
        "kooky-cli open --cwd {} -e {}",
        sh_quote(&meta.project_path),
        sh_quote(&cmd)
    );
    // 手动兜底的是能直接粘进任何终端跑的那条(kooky 的 --cwd 得自己 cd 回来)
    let cwd_ok = !meta.project_path.is_empty() && Path::new(&meta.project_path).is_dir();
    let manual = super::sh_command_line(
        exe.as_str(),
        &args,
        cwd_ok.then_some(meta.project_path.as_str()),
    );
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

/// Claude/Codex 桌面版深链。两端处理器只认裸 UUID 形态的会话 id(实测
/// 2026-09-01:Claude 的 claudeURLHandler 与 Codex 的 threads 路由同为
/// 8-4-4-4-12 hex 校验),而 Wake 这两家 adapter 的 native id 恰是该形态
/// ——不匹配就地报错,别发一条对端静默丢弃的 URL(`open` 对已注册 scheme
/// 恒退 0,拒绝在 Wake 侧看不见)。app 侧的失败(transcript 不在盘上、
/// 会话正被 CLI 占用)由 desktop 自己弹 toast,Wake 与 Kooky 深链同款
/// 只报"已送达"。
///
/// Claude Desktop 的深链导入只按 `local_<传入id>` 精确查重,不认会话链
/// 归属(desktop 内 resume picker 走 resolveForResume 才认)。已导入的
/// desktop 会话续聊后,其 CLI 链会推进到新 id——此时 Wake 直发盘上
/// transcript 的 id,desktop 会给同一条对话再复制一个会话。所以深链前
/// 先只读扫一遍 desktop 的会话索引(claude-code-sessions/<acct>/<org>/
/// local_*.json,每条含 cliSessionId/unarchivedCliSessionId/
/// preClearCliSessionId),命中就改发那条记录自己的 stem uuid,desktop
/// 对已存在的 local_<stem> 走复用+聚焦(实测 2026-09-01)。
fn launch_claude_desktop(meta: &SessionMeta) -> ResumeOutcome {
    // 非 UUID(含坏文件产生的空 id)直接走 launch_desktop_id 的报错路径,
    // 不进索引扫描——空 needle 会让预筛的 windows(0) panic
    let id = is_uuid(&meta.id)
        .then(|| claude_desktop_owned_stem(&meta.id))
        .flatten()
        .unwrap_or_else(|| meta.id.clone());
    launch_desktop_id(&id, "claude://resume?session=", TerminalApp::ClaudeDesktop)
}

/// 在 desktop 会话索引里找拥有 `cli_id` 的会话,返回其 stem uuid。
/// 索引是数百个 KB 级 json,每次点击现扫(读几 ms,别缓存——desktop
/// 侧随时在写)。解析失败/结构不符的文件跳过,扫不到回 None 走原 id。
fn claude_desktop_owned_stem(cli_id: &str) -> Option<String> {
    // home_dir 走 adapters 的 WAKE_HOME 改道,fixture 测试可以铺假索引
    let root = crate::adapters::home_dir()?
        .join("Library/Application Support/Claude/claude-code-sessions");
    let sub = |d: std::fs::DirEntry| std::fs::read_dir(d.path()).into_iter().flatten().flatten();
    // desktop 可能留有多个 <acct>/<org> 目录,而 claude://resume 只在当前
    // 登录账户下找 local_<stem>;当前账户 Wake 无从得知,多份命中时取
    // mtime 最新的记录(基本即活跃账户),别听目录枚举顺序的
    std::fs::read_dir(root)
        .ok()?
        .flatten()
        .flat_map(sub)
        .flat_map(sub)
        .filter_map(|f| {
            let p = f.path();
            let stem = owned_stem_in(&p, cli_id)?;
            let mtime = std::fs::metadata(&p).and_then(|m| m.modified()).ok();
            Some((mtime, stem))
        })
        .max_by_key(|(mtime, _)| *mtime)
        .map(|(_, stem)| stem)
}

/// 单条索引记录的归属判定。先做字节级预筛——绝大多数文件不含这个 id,
/// 省掉整篇 json 解析(命中的通常恰一份)。调用方保证 cli_id 非空
/// (空 needle 会 panic)
fn owned_stem_in(p: &Path, cli_id: &str) -> Option<String> {
    if p.extension().and_then(|e| e.to_str()) != Some("json") {
        return None;
    }
    let data = std::fs::read(p).ok()?;
    let needle = cli_id.as_bytes();
    if !data.windows(needle.len()).any(|w| w == needle) {
        return None;
    }
    let v = serde_json::from_slice::<serde_json::Value>(&data).ok()?;
    // 标量三字段 + 历史数组(多次 clear/resume 后旧 CLI id 归档在
    // priorCliSessionIds 里,漏了它老会话又会被重复导入)
    let owns = [
        "cliSessionId",
        "unarchivedCliSessionId",
        "preClearCliSessionId",
    ]
    .iter()
    .any(|k| v.get(k).and_then(|x| x.as_str()) == Some(cli_id))
        || v.get("priorCliSessionIds")
            .and_then(|x| x.as_array())
            .is_some_and(|a| a.iter().any(|x| x.as_str() == Some(cli_id)));
    owns.then(|| {
        v.get("sessionId")?
            .as_str()?
            .strip_prefix("local_")
            .filter(|s| is_uuid(s))
            .map(str::to_string)
    })
    .flatten()
}

fn launch_desktop_id(id: &str, prefix: &str, term: TerminalApp) -> ResumeOutcome {
    if !is_uuid(id) {
        return ResumeOutcome {
            ok: false,
            command: String::new(),
            error: Some(format!(
                "{} can only open sessions with a UUID id (got `{id}`)",
                term.display_name(),
            )),
        };
    }
    let url = format!("{prefix}{id}");
    let ok = Command::new("open")
        .arg(&url)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    ResumeOutcome {
        ok,
        command: url,
        error: (!ok).then(|| format!("Couldn't open {}", term.display_name())),
    }
}

/// 裸 UUID(8-4-4-4-12 hex,大小写不限)——与两家 desktop 深链处理器的
/// 校验正则同构
fn is_uuid(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 36
        && b.iter().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => *c == b'-',
            _ => c.is_ascii_hexdigit(),
        })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuid_gate_matches_desktop_handlers() {
        assert!(is_uuid("01937a56-3a2c-43af-af68-a8d4e00d60ed"));
        assert!(is_uuid("01A05A8B-2BB4-7DB3-B72C-3E9AAB7DF290")); // 大小写不限
        assert!(!is_uuid("last"));
        assert!(!is_uuid("local_01937a56-3a2c-43af-af68-a8d4e00d60ed"));
        assert!(!is_uuid("01937a56-3a2c-43af-af68-a8d4e00d60e")); // 短一位
        assert!(!is_uuid("01937a56x3a2c-43af-af68-a8d4e00d60ed")); // 错位分隔符
        assert!(!is_uuid(""));
    }

    /// 非 UUID id 必须就地报错而不是发 URL——`open` 对已注册 scheme 恒退 0,
    /// 发出去的坏 id 在 Wake 侧看不见失败
    #[test]
    fn desktop_deep_link_rejects_non_uuid_id_without_launching() {
        let meta = crate::models::SessionMeta {
            key: "claude-code:not-a-uuid".into(),
            id: "not-a-uuid".into(),
            host: String::new(),
            agent: crate::models::AgentId::ClaudeCode,
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
            favorite: false,
            pinned: false,
        };
        for term in [TerminalApp::ClaudeDesktop, TerminalApp::CodexDesktop] {
            let outcome = deep_link_resume(&meta, term).expect("desktop targets are deep-link");
            assert!(!outcome.ok);
            assert!(
                outcome.command.is_empty(),
                "must not build a URL for a bad id"
            );
            assert!(outcome
                .error
                .as_deref()
                .is_some_and(|e| e.contains(term.display_name())));
        }
    }

    #[test]
    fn shell_launch_refuses_deep_link_targets() {
        for term in [
            TerminalApp::Kooky,
            TerminalApp::ClaudeDesktop,
            TerminalApp::CodexDesktop,
        ] {
            assert!(launch_shell(term, "echo hi").is_err());
        }
    }
}
