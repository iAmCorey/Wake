//! Linux 平台原语:终端仿真器 argv 直传(无 AppleScript 层)、freedesktop
//! 废纸篓(trash crate)、文件管理器(D-Bus ShowItems / xdg-open)、剪贴板
//! (wl-copy / xclip / xsel)。接口与 macos.rs 同形,策略在 mod.rs。
//!
//! 家族探测走 `resolve_clis`(login shell PATH,连 flatpak 的 exports/bin
//! 一起覆盖),只列装了的。spawn 形制各家见 exec_prefix 注释;失败面统一由
//! mod.rs 的剪贴板兜底接住(命令已拼好,粘进任何终端都能跑)。

use super::{percent_encode, pipe_to, resolve_cli, resolve_clis, spawn_and_reap, ResumeOutcome};
use crate::models::{AgentId, SessionMeta};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Open In 下拉的目标终端(Linux 家族)。全部是 shell 命令目标:
/// `<bin> <exec_prefix…> /bin/sh -c '<command>; exec "$SHELL"'`,命令跑完
/// 交还交互 shell 保持窗口(与 macOS Ghostty 同策略)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalApp {
    GnomeTerminal,
    /// GNOME Console(kgx)——新 GNOME/Ubuntu 的默认终端
    GnomeConsole,
    Konsole,
    XfceTerminal,
    Ghostty,
    Kitty,
    Alacritty,
    WezTerm,
    Xterm,
}

impl TerminalApp {
    const ALL: [TerminalApp; 9] = [
        TerminalApp::GnomeTerminal,
        TerminalApp::GnomeConsole,
        TerminalApp::Konsole,
        TerminalApp::XfceTerminal,
        TerminalApp::Ghostty,
        TerminalApp::Kitty,
        TerminalApp::Alacritty,
        TerminalApp::WezTerm,
        TerminalApp::Xterm,
    ];

    pub fn display_name(&self) -> &'static str {
        match self {
            TerminalApp::GnomeTerminal => "GNOME Terminal",
            TerminalApp::GnomeConsole => "Console",
            TerminalApp::Konsole => "Konsole",
            TerminalApp::XfceTerminal => "Xfce Terminal",
            TerminalApp::Ghostty => "Ghostty",
            TerminalApp::Kitty => "kitty",
            TerminalApp::Alacritty => "Alacritty",
            TerminalApp::WezTerm => "WezTerm",
            TerminalApp::Xterm => "XTerm",
        }
    }

    /// 稳定短 id(图标缓存文件名、last-used 记忆用)。恰好也全部是可执行名,
    /// 探测与启动直接复用
    pub fn id(&self) -> &'static str {
        match self {
            TerminalApp::GnomeTerminal => "gnome-terminal",
            TerminalApp::GnomeConsole => "kgx",
            TerminalApp::Konsole => "konsole",
            TerminalApp::XfceTerminal => "xfce4-terminal",
            TerminalApp::Ghostty => "ghostty",
            TerminalApp::Kitty => "kitty",
            TerminalApp::Alacritty => "alacritty",
            TerminalApp::WezTerm => "wezterm",
            TerminalApp::Xterm => "xterm",
        }
    }

    /// 内嵌品牌图标覆盖(macOS 的 Codex desktop 用;此平台无深链目标)
    pub fn brand_icon(&self) -> Option<&'static str> {
        None
    }

    /// spawn 前缀:`<bin> <prefix…> <argv…>`,prefix 是各家"后面全是命令 argv"
    /// 的开关——gnome-terminal/kgx 用 `--`,wezterm 是子命令 `start --`,
    /// xfce4-terminal 用 `-x`(-e 是单串旧式),kitty 裸接,其余 `-e`
    fn exec_prefix(&self) -> &'static [&'static str] {
        match self {
            TerminalApp::GnomeTerminal | TerminalApp::GnomeConsole => &["--"],
            TerminalApp::WezTerm => &["start", "--"],
            TerminalApp::XfceTerminal => &["-x"],
            TerminalApp::Kitty => &[],
            TerminalApp::Konsole
            | TerminalApp::Ghostty
            | TerminalApp::Alacritty
            | TerminalApp::Xterm => &["-e"],
        }
    }
}

/// Linux 无深链类恢复目标(Kooky 仅 macOS),全部走 shell 命令
pub(super) fn deep_link_resume(_meta: &SessionMeta, _term: TerminalApp) -> Option<ResumeOutcome> {
    None
}

/// 已安装终端(启动后不变,进程内缓存;一次 login shell 批量探测)
pub fn installed_terminals() -> &'static [TerminalApp] {
    use std::sync::OnceLock;
    static CACHE: OnceLock<Vec<TerminalApp>> = OnceLock::new();
    CACHE.get_or_init(|| {
        let bins: Vec<&str> = TerminalApp::ALL.iter().map(|t| t.id()).collect();
        let found = resolve_clis(&bins);
        TerminalApp::ALL
            .into_iter()
            .filter(|t| matches!(found.get(t.id()), Some(Some(_))))
            .collect()
    })
}

/// 某会话可用的恢复目标(Linux 无 Kooky 这类按 agent 过滤的目标,一律全量)
pub fn terminals_for(_agent: AgentId) -> Vec<TerminalApp> {
    installed_terminals().to_vec()
}

/// Shell 类目标的命令注入:argv 直传,无二次 shell 解析。
/// gnome-terminal/kgx 经 D-Bus 服务转发、前台进程即刻退出,spawn 成功即算送达
pub(super) fn launch_shell(term: TerminalApp, command: &str) -> anyhow::Result<()> {
    let bin = term.id();
    let exe = resolve_cli(bin).ok_or_else(|| anyhow::anyhow!("{bin} not found"))?;
    // 命令跑完 exec 用户 shell 保持窗口;$SHELL 缺省退 /bin/sh
    let keep_open = format!("{command}; exec \"${{SHELL:-/bin/sh}}\"");
    let mut cmd = Command::new(exe);
    cmd.args(term.exec_prefix());
    cmd.args(["/bin/sh", "-c", &keep_open]);
    spawn_and_reap(cmd)?;
    Ok(())
}

/// 终端图标提取:Linux 无 NSWorkspace 等价物(得解析 .desktop + icon theme
/// 查找),v1 不做——UI 对无图标的终端行本就有无图兜底。此函数跑在启动的
/// background 线程里,顺手预热 installed_terminals 的 login shell 探测,
/// 别让首次打开详情页的 render 帧付这几百毫秒(macOS 靠图标提取天然预热)。
pub fn ensure_app_icons(_cache_dir: &Path) -> HashMap<String, PathBuf> {
    let _ = installed_terminals();
    HashMap::new()
}

/// wl-copy(Wayland 会话优先)→ xclip → xsel,依次试,NotFound/退出失败落
/// 下家;都不成返回 false——调用方(clipboard_fallback)据此决定还敢不敢
/// 对用户说 "copied"
pub(super) fn copy_to_clipboard(text: &str) -> bool {
    let wl: (&str, &[&str]) = ("wl-copy", &[]);
    let xclip: (&str, &[&str]) = ("xclip", &["-selection", "clipboard"]);
    let xsel: (&str, &[&str]) = ("xsel", &["--clipboard", "--input"]);
    let order = if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        [wl, xclip, xsel]
    } else {
        [xclip, xsel, wl]
    };
    order
        .into_iter()
        .any(|(bin, args)| pipe_to(bin, args, text))
}

/// 批量删进回收站(freedesktop Trash spec,trash crate 纯库实现,文件
/// 管理器里可恢复;收 mod.rs 已过滤的真实路径)
pub(super) fn trash_existing(paths: &[&str]) -> anyhow::Result<()> {
    trash::delete_all(paths).map_err(|e| anyhow::anyhow!("Failed to move to Trash: {e}"))
}

/// 致命错误对话框:zenity → kdialog 尽力弹一个,桌面双双缺席时 mod.rs
/// 已落过 stderr(从 .desktop 启动的场景由 journald 收)
pub(super) fn alert_dialog(message: &str) {
    let zenity = Command::new("zenity")
        .args([
            "--error",
            "--title=Wake can't start",
            &format!("--text={message}"),
        ])
        .status();
    if zenity.map(|s| s.success()).unwrap_or(false) {
        return;
    }
    let _ = Command::new("kdialog")
        .args(["--title", "Wake can't start", "--error", message])
        .status();
}

/// file:// URL(percent_encode 保留 '/';逗号也会被编码,不会撞
/// dbus-send 的 array 逗号分隔语法)
fn file_url(p: &str) -> String {
    format!("file://{}", percent_encode(p, true))
}

/// 在文件管理器里进入目录
pub(super) fn open_dir(path: &str) {
    let mut cmd = Command::new("xdg-open");
    cmd.arg(path);
    let _ = spawn_and_reap(cmd);
}

/// 选中文件(收 mod.rs 已剥好虚拟后缀的真实路径):先试
/// org.freedesktop.FileManager1 的 ShowItems(Files/Dolphin/Nemo 都实现),
/// D-Bus 通道不在(headless、极简桌面)退 xdg-open 父目录。
/// `--print-reply` 会等到文件管理器 activation 冷启结束——调用方(mod.rs)
/// 已把整个函数放进短命线程,这里阻塞无妨。
pub(super) fn reveal_path(path: &str) {
    let shown = Command::new("dbus-send")
        .args([
            "--session",
            "--print-reply",
            "--dest=org.freedesktop.FileManager1",
            "/org/freedesktop/FileManager1",
            "org.freedesktop.FileManager1.ShowItems",
            &format!("array:string:{}", file_url(path)),
            "string:",
        ])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if shown {
        return;
    }
    let parent = Path::new(path)
        .parent()
        .and_then(|d| d.to_str())
        .unwrap_or(path);
    open_dir(parent);
}
