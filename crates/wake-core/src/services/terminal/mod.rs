//! 会话恢复 / 系统集成服务。策略层(恢复命令拼装、CLI 解析、打开/选中/
//! 删除的平台无关流程)在本文件;macos.rs / linux.rs / windows.rs 只提供
//! 原语(起终端、开目录、选中文件、进废纸篓、弹对话框、写剪贴板),各端
//! 导出同形接口(TerminalApp 变体集合各异,UI 只遍历不点名)。
//!
//! POSIX 双端(macOS/Linux)的共享层在 posix.rs(login shell 探测、
//! posix_quote 拼装);Windows 的这些前提全不成立(无 login shell、引号
//! 不是 POSIX 规则、命令方言按终端宿主分 cmd/PowerShell 两派),故
//! probe_clis / compose_command / launch_shell 三个接缝按平台各取一份,
//! 策略流程本身不分叉。

use crate::models::{AgentId, SessionMeta};
use std::collections::HashMap;
use std::path::Path;
use std::process::Command;
use std::sync::Mutex;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
use macos as platform;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
use linux as platform;

// POSIX 共享层**正向**圈定,新平台不会静默继承(见 posix.rs 头注)。
// pub(crate) 三件是 macos.rs / linux.rs 经 `super::` 取用的中转。
#[cfg(any(target_os = "macos", target_os = "linux"))]
mod posix;
#[cfg(any(target_os = "macos", target_os = "linux"))]
use posix::{compose_command, probe_clis};
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(crate) use posix::{percent_encode, pipe_to};
// posix_quote 只有 macos.rs 消费(kooky CLI 的命令拼装),Linux 上中转会空挂
#[cfg(target_os = "macos")]
pub(crate) use posix::posix_quote;

#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
use windows as platform;
#[cfg(target_os = "windows")]
use windows::{compose_command, probe_clis};

// 新平台必须给出自己的模块并接上 probe_clis / compose_command /
// launch_shell 三个接缝(windows.rs 是完整先例)——POSIX 共享层是正向
// cfg,漏接的接缝直接是编译错误,不存在静默沿用
#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
compile_error!("wake terminal services: unsupported platform — add a platform module wired into the probe_clis / compose_command / launch_shell seams");

pub use platform::{ensure_app_icons, installed_terminals, terminals_for, TerminalApp};

#[derive(Debug, Clone)]
pub struct ResumeOutcome {
    pub ok: bool,
    pub command: String,
    pub error: Option<String>,
}

/// GUI 进程 PATH 不全(macOS/Linux 缺 ~/.local/bin 等),批量解析并缓存
static CLI_CACHE: Mutex<Option<HashMap<String, Option<String>>>> = Mutex::new(None);

fn resolve_clis(bins: &[&str]) -> HashMap<String, Option<String>> {
    let mut cache = CLI_CACHE.lock().unwrap();
    let map = cache.get_or_insert_with(HashMap::new);
    let missing: Vec<&str> = bins
        .iter()
        .filter(|b| !map.contains_key(**b))
        .copied()
        .collect();
    if !missing.is_empty() {
        let found = probe_clis(&missing);
        for b in missing {
            map.insert(b.to_string(), found.get(b).cloned());
        }
    }
    bins.iter()
        .map(|b| (b.to_string(), map.get(*b).cloned().flatten()))
        .collect()
}

/// 单 bin 解析(命中缓存则不起 shell)
fn resolve_cli(bin: &str) -> Option<String> {
    resolve_clis(&[bin]).get(bin).cloned().flatten()
}

pub fn cli_path(agent: AgentId) -> Option<String> {
    agent_bin(agent).and_then(resolve_cli)
}

/// 会话级二进制:OpenCode 2 next preview 与 stable 并存、装为 `opencode2`,
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
        AgentId::Qoder => Some("qoder"),
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
        // dsh 官方唯一分发形态是 npx(README 只有 `npx @deepseek-ai/dsh web`,
        // 不发全局命令);包名由 resume_args 作首参带上
        AgentId::Dsh => Some("npx"),
    }
}

fn resume_args(agent: AgentId, id: &str) -> Option<(Vec<String>, bool)> {
    match agent {
        AgentId::ClaudeCode => Some((vec!["--resume".into(), id.into()], true)),
        AgentId::Codex => Some((vec!["resume".into(), id.into()], false)),
        // Qoder 的历史会话按 cwd 分桶，`--resume <id>` 需在原项目目录启动。
        AgentId::Qoder => Some((vec!["--resume".into(), id.into()], true)),
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
        // dsh 官方 tui bundle 未发布(rc.8 shipped profile 只有 web/headless,
        // help 里的 --profile tui --resume 当下无消费端),web 是唯一交互
        // surface 且无 per-session 深链——resume 退而求其次:cd 到会话 cwd
        // 按官方原样 `npx @deepseek-ai/dsh web` 拉起(workspace 由启动目录
        // 决定),会话在 UI 里即点即续。官方发布 tui bundle 后切回定点 resume
        AgentId::Dsh => Some((vec!["@deepseek-ai/dsh".into(), "web".into()], true)),
        _ => None,
    }
}

pub fn resume_session_in(meta: &SessionMeta, term: TerminalApp) -> ResumeOutcome {
    // 深链类目标(macOS Kooky)由平台整锅接管,不走 shell 命令构建;
    // 新增非 shell 目标在平台的 deep_link_resume 里声明,这里无需加旁路
    if let Some(outcome) = platform::deep_link_resume(meta, term) {
        return outcome;
    }
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
    let bin = session_bin(meta);
    let Some(cli) = bin.and_then(resolve_cli) else {
        return ResumeOutcome {
            ok: false,
            command: String::new(),
            error: Some(format!(
                "CLI `{}` (for {}) not found in shell PATH — is it installed?",
                bin.unwrap_or("?"),
                meta.agent.display_name()
            )),
        };
    };
    let cwd_ok = !meta.project_path.is_empty() && Path::new(&meta.project_path).is_dir();
    let cwd = cwd_ok.then(|| meta.project_path.as_str());
    // 按用户选的宿主取方言:command 既是成功 toast 的展示面,也是失败时
    // 塞进剪贴板的那条,必须与真正跑的一致(Windows 的 cmd 宿主方言不同)
    let command = compose_command(term, &cli, &args, cwd);
    if requires_cwd && !cwd_ok {
        let hint = clipboard_fallback(&command);
        return ResumeOutcome {
            ok: false,
            command,
            error: Some(format!(
                "Project directory no longer exists: {}. {hint}",
                meta.project_path
            )),
        };
    }

    // 起终端:POSIX 直接投喂拼好的 command;Windows 的 launch_shell 另收
    // 结构化件按宿主重拼方言(cmd 宿主 cmd 方言、其余复用 command 的
    // PowerShell 形态)——一条字符串塞不进两种引号规则,平台差异只摊在
    // 这一处调用点上。
    #[cfg(not(target_os = "windows"))]
    let result = platform::launch_shell(term, &command);
    #[cfg(target_os = "windows")]
    let result = platform::launch_shell(term, &cli, &args, cwd, &command);
    match result {
        Ok(()) => ResumeOutcome {
            ok: true,
            command,
            error: None,
        },
        Err(e) => {
            let hint = clipboard_fallback(&command);
            ResumeOutcome {
                ok: false,
                command,
                error: Some(format!("Couldn't open terminal ({e}). {hint}")),
            }
        }
    }
}

/// 失败兜底通知的后半句:剪贴板写成了才说 copied(Linux 可能三个剪贴板
/// 工具都不在),没写成把命令本体给出来——error 是失败通知唯一展示面
/// (workbench 只渲染 error 文案),命令不能只活在 ResumeOutcome.command 里
fn clipboard_fallback(command: &str) -> String {
    if platform::copy_to_clipboard(command) {
        "Command copied to clipboard — paste to run.".to_string()
    } else {
        format!("Run manually: {command}")
    }
}

/// 删除会话文件到系统回收站(可恢复)。虚拟路径 `<db>#<id>` 与已消失的
/// 文件在此过滤(不变量 3:SQLite 型只 tombstone),平台原语只收真实路径。
/// 部分失败语义:平台实现可能删到一半报错(macOS 逐文件、Linux/Windows
/// 批量),调用方按"整批可疑"处理即可——已进回收站的文件可恢复,无害。
pub fn trash_paths(paths: &[String]) -> anyhow::Result<()> {
    let existing: Vec<&str> = paths
        .iter()
        .map(|s| s.as_str())
        .filter(|p| Path::new(p).exists())
        .collect();
    if existing.is_empty() {
        return Ok(());
    }
    platform::trash_existing(&existing)
}

/// 致命错误提示。GPUI 窗口还没起来时这是唯一能让用户看见的通道,
/// 否则应用就是无提示秒退;stderr 始终先落一份。
pub fn show_fatal_alert(message: &str) {
    eprintln!("[wake] fatal: {message}");
    platform::alert_dialog(message);
}

/// 在文件管理器里打开这个位置:目录直接进入,文件则退回在父目录中选中它
/// ——SQLite 型的数据源是库文件,直接交给 opener 会把它丢给默认应用打开
pub fn open_in_file_manager(path: &str) {
    if Path::new(path).is_dir() {
        platform::open_dir(path);
    } else {
        reveal_in_file_manager(path);
    }
}

/// 选中文件。调用点都是 UI 线程的 on_click,而平台原语可能长阻塞
/// (Linux 的 D-Bus ShowItems 会等文件管理器 activation 冷启)——统一
/// 甩给短命线程,UI 零等待。
pub fn reveal_in_file_manager(path: &str) {
    let real = crate::adapters::sqlite_ro::strip_virtual_path(path).to_string();
    std::thread::spawn(move || platform::reveal_path(&real));
}

/// 起子进程并**收尸**,spawn 失败上抛。`status()` 不能用:把调用线程阻塞
/// 到进程结束(文件管理器冷启上百毫秒);裸 `spawn()` 丢掉 Child 也不行:
/// Unix 上 Child 的 Drop 不 wait,实测点几次就攒几个 `<defunct>`,直到
/// Wake 退出才回收。故 spawn 后交给一个短命线程 wait。Windows 无僵尸进程
/// 概念,但 wait 同样及时归还进程句柄,三端共用。
fn spawn_and_reap(mut cmd: Command) -> std::io::Result<()> {
    let mut child = cmd.spawn()?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}
