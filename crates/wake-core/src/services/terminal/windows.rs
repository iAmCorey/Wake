//! Windows 平台原语:终端宿主(Windows Terminal / PowerShell / cmd /
//! Alacritty / WezTerm)、回收站(trash crate → IFileOperation)、资源管理器
//! 进入/选中、Win32 剪贴板与 MessageBox。接口与 macos.rs / linux.rs 同形,
//! 策略在 mod.rs;mod.rs 的三个 POSIX 前提(login shell、sh_quote 方言、单一
//! shell 方言)在本端由 probe_clis / compose_command / launch_shell 接管。
//!
//! 方言分两派:cmd 宿主用 cmd 方言内联注入(raw_arg 直达,无 argv 引号层);
//! 其余宿主一律装 PowerShell 会话——脚本全单引号、不含双引号,经宿主的
//! argv 重引号往返无损(cmd 方言的内层双引号过不了 wt 这类会重拼命令行的
//! 宿主)。展示/剪贴板形态同为 PowerShell 方言:现代 Windows 的默认 shell,
//! pwsh / Windows PowerShell / wt 默认 profile 粘贴均可跑。

use super::{resolve_cli, resolve_clis, spawn_and_reap, ResumeOutcome};
use crate::models::{AgentId, SessionMeta};
use std::collections::HashMap;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use windows_sys::Win32::System::Threading::{CREATE_NEW_CONSOLE, CREATE_NO_WINDOW};

/// Open In 下拉的目标终端(Windows 家族)。wt/pwsh/powershell/cmd 覆盖
/// 系统自带面(cmd 与 Windows PowerShell 必装,探测恒真),第三方只列
/// 装了的。Ghostty/kitty 无 Windows 发行版,不列。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalApp {
    /// Windows Terminal(wt)——Win11 默认终端宿主
    WindowsTerminal,
    /// PowerShell 7+(pwsh),与内置 Windows PowerShell 是两个产品
    Pwsh,
    WindowsPowershell,
    Cmd,
    Alacritty,
    WezTerm,
}

impl TerminalApp {
    /// 声明序即偏好序(installed_terminals 保序,UI 回退取首个)
    const ALL: [TerminalApp; 6] = [
        TerminalApp::WindowsTerminal,
        TerminalApp::Pwsh,
        TerminalApp::WindowsPowershell,
        TerminalApp::Cmd,
        TerminalApp::Alacritty,
        TerminalApp::WezTerm,
    ];

    /// 命名对齐 wt 自己的 profile 名("PowerShell" = 7+,"Windows PowerShell"
    /// = 内置 5.1),用户在 wt 里见到的就是这两个词
    pub fn display_name(&self) -> &'static str {
        match self {
            TerminalApp::WindowsTerminal => "Windows Terminal",
            TerminalApp::Pwsh => "PowerShell",
            TerminalApp::WindowsPowershell => "Windows PowerShell",
            TerminalApp::Cmd => "Command Prompt",
            TerminalApp::Alacritty => "Alacritty",
            TerminalApp::WezTerm => "WezTerm",
        }
    }

    /// 稳定短 id(图标缓存文件名、last-used 记忆用)。恰好也全部是可执行名,
    /// 探测与启动直接复用(PATHEXT 扩展名由 probe_clis 补)
    pub fn id(&self) -> &'static str {
        match self {
            TerminalApp::WindowsTerminal => "wt",
            TerminalApp::Pwsh => "pwsh",
            TerminalApp::WindowsPowershell => "powershell",
            TerminalApp::Cmd => "cmd",
            TerminalApp::Alacritty => "alacritty",
            TerminalApp::WezTerm => "wezterm",
        }
    }

    /// 内嵌品牌图标覆盖(macOS 的 Codex desktop 用;此平台无深链目标)
    pub fn brand_icon(&self) -> Option<&'static str> {
        None
    }

    /// 此宿主自身不是 shell,得再装一个 PowerShell 会话才跑得起命令
    /// (terminals_for 据此在没有 PowerShell 的机器上藏掉它们)
    fn needs_powershell(&self) -> bool {
        matches!(
            self,
            TerminalApp::WindowsTerminal | TerminalApp::Alacritty | TerminalApp::WezTerm
        )
    }
}

/// PATH × PATHEXT 纯 Rust 遍历,语义即 CreateProcess 的查找规则。不走
/// where.exe:它把输出编码成控制台 codepage,非 ASCII 用户名的路径经
/// from_utf8_lossy 必坏;env::var/Path 全程 Unicode,无此折损,也免掉
/// GUI 进程起 console 子进程的窗口闪现。无扩展名裸文件(npm 的 bash
/// shim)不参与候选,天然滤掉。
pub(super) fn probe_clis(missing: &[&str]) -> HashMap<String, String> {
    let mut found = HashMap::new();
    let path_var = std::env::var_os("PATH").unwrap_or_default();
    let exts: Vec<String> = std::env::var("PATHEXT")
        .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into())
        .split(';')
        .filter(|e| e.starts_with('.'))
        .map(str::to_string)
        .collect();
    // dirs 外层单遍 PATH、命中即从 pending 摘除:PATH 目录序 × PATHEXT 序的
    // 首个命中语义不变,但 is_file(Windows 上每次都是一次 CreateFileW,还要
    // 过 Defender)只付 O(PATH),不是 O(PATH × bins);候选文件名先算好,
    // 循环里零 format!
    let mut pending: Vec<(&str, Vec<String>)> = missing
        .iter()
        .map(|w| (*w, exts.iter().map(|e| format!("{w}{e}")).collect()))
        .collect();
    for dir in std::env::split_paths(&path_var) {
        if pending.is_empty() {
            break;
        }
        // 相对 PATH 项(`.`、`bin`,或快捷方式 "起始位置" 带进来的)一律跳过:
        // 缓存里的路径会直接喂给 Command::new 和用户可粘的命令串,相对项会
        // 让两者各自解析到不同的东西(POSIX 侧同样只收 `/` 开头的结果)
        if dir.as_os_str().is_empty() || !dir.is_absolute() {
            continue;
        }
        pending.retain(|(want, names)| {
            for name in names {
                let cand = dir.join(name);
                // WindowsApps 的 app-execution alias(wt.exe)是 0 字节
                // reparse 文件,is_file 为真、可直接 spawn
                if cand.is_file() {
                    found.insert(want.to_string(), cand.to_string_lossy().to_string());
                    return false;
                }
            }
            true
        });
    }
    found
}

/// PowerShell 单引号字面量:唯一转义是 ' → ''。`$`、反引号、反斜杠全是
/// 普通字符,且命令行经 CreateProcessW 以 UTF-16 直达,无 codepage 折损
fn ps_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// cmd 方言 quote:一律双引号包裹(Windows 文件名不允许 `"`,无内层转义面)。
/// 不留裸词快路径——cmd 的引号保全规则按"整行引号个数 + 首字符是否引号"
/// 分支,让引号数随内容浮动等于让 mangling 随机出现;定长形制配合 cmd_line
/// 的 `call` 前缀,行首恒为裸词,规则可预测。
///
/// `%` 无法在此中和:命令行(非批处理)语境下 `%%` 不折叠、引号内 `^` 是
/// 字面量,故含 `%` 的会话交由 launch_shell 拒绝走 cmd 宿主,不静默改写
fn cmd_quote(s: &str) -> String {
    format!("\"{s}\"")
}

/// cmd 方言整行。`call` 前缀让行首恒为裸词:cmd 对 `/K` 载荷有一条遗留
/// 规则——首字符是引号且全行引号数不为二时,砍掉首尾各一个引号——路径
/// 带空格(`C:\Program Files\…`)时整条命令会被腰斩成 `C:\Program`,而
/// spawn 成功、Wake 照报 ok(2026-08-25 review)。`call` 对 exe 与
/// bat/cmd 同样是同步调用,语义不变。
/// `/d`:跨盘符 cd 也生效(会话在 D: 而 cmd 起在 C: 的情形)。
fn cmd_line(cli: &str, args: &[String], cwd: Option<&str>) -> String {
    let mut line = String::new();
    if let Some(dir) = cwd {
        line.push_str(&format!("cd /d {} && ", cmd_quote(dir)));
    }
    line.push_str("call ");
    line.push_str(&cmd_quote(cli));
    for a in args {
        line.push(' ');
        line.push_str(&cmd_quote(a));
    }
    line
}

/// cmd 宿主无法安全承载的内容:`%VAR%` 在双引号内照样展开,而命令行语境
/// 下没有任何转义能关掉它——含 `%` 就会静默跑成另一条命令。会话 id 取自
/// 文件名(parse_utils),用户目录更是任意,概率并非零。
fn cmd_hostile(cli: &str, args: &[String], cwd: Option<&str>) -> bool {
    std::iter::once(cli)
        .chain(args.iter().map(|s| s.as_str()))
        .chain(cwd)
        .any(|s| s.contains('%'))
}

/// PowerShell 方言:`Set-Location -LiteralPath 'dir' -ErrorAction Stop;
/// & 'cli' 'args…'`。-LiteralPath 防路径里的 `[ ]` 被当通配符;
/// **-ErrorAction Stop 不可省**:Windows PowerShell 5.1 不认 `&&`,而
/// Set-Location 默认是 non-terminating error,裸 `;` 会在 cd 失败后照样
/// 起 agent——落进错误的工作目录且 Wake 已报成功(dsh 的 workspace 完全
/// 由启动目录决定,后果尤重)。加了它才与 POSIX 的 `cd X && cmd`、
/// cmd 宿主的 `cd /d X && …` 同为失败即止(2026-08-25 review)。
fn ps_line(cli: &str, args: &[String], cwd: Option<&str>) -> String {
    let core = ps_call(cli, args);
    match cwd {
        Some(dir) => format!(
            "Set-Location -LiteralPath {} -ErrorAction Stop; {core}",
            ps_quote(dir)
        ),
        None => core,
    }
}

/// 展示/剪贴板/启动共用的那一条命令,**按用户实际选的宿主取方言**。
/// 三端共有的契约是 ResumeOutcome.command 即真正跑的那条(workbench 的
/// 成功 toast 与失败兜底都渲染它);早先 Windows 恒给 PowerShell 形态,
/// 于是选了 Command Prompt 的用户会拿到一条粘进 cmd 必报语法错的命令
/// ——而失败兜底的两个触发点(cwd 不存在、spawn 失败)都在 launch_shell
/// 之前/之外,"cmd 用户不经剪贴板"的假设不成立(2026-08-25 review)
pub(super) fn compose_command(
    term: TerminalApp,
    cli: &str,
    args: &[String],
    cwd: Option<&str>,
) -> String {
    match term {
        TerminalApp::Cmd => cmd_line(cli, args, cwd),
        _ => ps_line(cli, args, cwd),
    }
}

/// 纯调用段(无 cd、无分号):`& 'cli' 'args…'`。wt 宿主用它配合 `-d`
/// 传工作目录——wt 对命令行做无视引号的 `;` 分面板切分,脚本里带分号
/// 必被腰斩,cd 只能交给 wt 自己的 -d。
fn ps_call(cli: &str, args: &[String]) -> String {
    let mut core = format!("& {}", ps_quote(cli));
    for a in args {
        core.push(' ');
        core.push_str(&ps_quote(a));
    }
    core
}

/// wt 命令行的 `;` 转义(wt 文档规定 `\;`;引号不豁免,对 option 值同样
/// 生效)。路径/id 里出现分号是合法但极罕见的形态,统一转义兜住。
fn wt_escape(s: &str) -> String {
    s.replace(';', "\\;")
}

/// NUL 结尾的 UTF-16(*W 系 API 的入参形制;NUL 是防越界读的护栏,
/// 所有 Win32 宽字符串都走这一份)
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Windows 无深链类恢复目标(Kooky 仅 macOS),全部走 shell 命令
pub(super) fn deep_link_resume(_meta: &SessionMeta, _term: TerminalApp) -> Option<ResumeOutcome> {
    None
}

/// 已安装终端(启动后不变,进程内缓存;PATH 遍历是纯文件系统操作,首扫
/// 即毫秒级)
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

/// 某会话可用的恢复目标。Windows 不按 agent 过滤,但 wt/Alacritty/WezTerm
/// 只是宿主、内部还要装一个 PowerShell 会话——两个 PowerShell 都不在 PATH
/// 时它们同样起不来,得跟着藏掉(macOS 用同一个钩子藏 Kooky)。否则用户点
/// 下去只能拿到 "PowerShell not found" 加一条自己没有 shell 去跑的命令
pub fn terminals_for(_agent: AgentId) -> Vec<TerminalApp> {
    // 这里挂在每个会话行的 render 上,不能调 powershell_bin:
    // CLI miss 不缓存后,没装 pwsh 的机器会每帧重扫 PATH × PATHEXT。
    // 已安装终端清单本就是启动期快照,用它判断即可;真正点击
    // 启动时 powershell_bin 仍会活查,新安装的 pwsh 能立即生效。
    let installed = installed_terminals();
    let has_ps = installed
        .iter()
        .any(|t| matches!(t, TerminalApp::Pwsh | TerminalApp::WindowsPowershell));
    installed
        .iter()
        .copied()
        .filter(|t| has_ps || !t.needs_powershell())
        .collect()
}

/// wt/第三方宿主内装的 shell:优先 pwsh(用户装了 7+ 就是它的默认),
/// 缺席退必装的 Windows PowerShell
fn powershell_bin() -> anyhow::Result<String> {
    resolve_cli("pwsh")
        .or_else(|| resolve_cli("powershell"))
        .ok_or_else(|| anyhow::anyhow!("PowerShell not found"))
}

/// PowerShell 会话前缀,`-Command` 后接脚本;`-NoExit` 即 keep-open
const PS_SESSION: [&str; 3] = ["-NoLogo", "-NoExit", "-Command"];

/// 按宿主方言起终端。keep-open 与 POSIX 的 `exec $SHELL` 同位:cmd 用
/// `/K`,PowerShell 用 `-NoExit`,命令跑完留在交互提示符。command 即
/// mod.rs 经 compose_command 拼好的 PowerShell 形态,PowerShell 系宿主
/// 直接复用;cmd 宿主方言不同,只能从结构化件重拼。
pub(super) fn launch_shell(
    term: TerminalApp,
    cli: &str,
    args: &[String],
    cwd: Option<&str>,
    command: &str,
) -> anyhow::Result<()> {
    let bin = term.id();
    let exe = resolve_cli(bin).ok_or_else(|| anyhow::anyhow!("{bin} not found"))?;
    let mut cmd = Command::new(&exe);
    match term {
        TerminalApp::Cmd => {
            if cmd_hostile(cli, args, cwd) {
                // 拒绝而非静默改写:调用方会把 command(此刻已是 cmd 方言)
                // 送进剪贴板兜底,用户仍拿得到可手动执行的那条
                anyhow::bail!("path or session id contains '%', which Command Prompt would expand — pick another terminal");
            }
            // raw_arg 绕过 std 的 argv 引号规则——cmd 不做 argv 解析,std 把
            // 整串再包一层引号反而会毁掉内层结构。command 即 compose_command
            // 给的 cmd 方言原文(见 cmd_line 的 `call` 前缀说明)
            cmd.raw_arg(format!("/K {command}"));
        }
        TerminalApp::Pwsh | TerminalApp::WindowsPowershell => {
            cmd.args(PS_SESSION).arg(command);
        }
        TerminalApp::WindowsTerminal => {
            // wt 装 PowerShell 会话。工作目录走 wt 自己的 -d,脚本只剩纯调用
            // 段(见 ps_call);所有透传参数过 wt_escape——wt 的 `;` 切分
            // 无视引号,不转义的分号会把命令行腰斩成两个面板
            if let Some(dir) = cwd {
                cmd.args(["-d", &wt_escape(dir)]);
            }
            cmd.arg(wt_escape(&powershell_bin()?));
            cmd.args(PS_SESSION).arg(wt_escape(&ps_call(cli, args)));
        }
        TerminalApp::Alacritty | TerminalApp::WezTerm => {
            // 第三方宿主装 PowerShell 会话:脚本全单引号、无内层双引号,经
            // 宿主的 argv 重引号(std → 宿主 → CreateProcess)往返无损;
            // 两家 argv 直传、无 wt 那套 `;` 语义,cd 留在脚本里
            if term == TerminalApp::WezTerm {
                cmd.args(["start", "--"]);
            } else {
                cmd.arg("-e");
            }
            cmd.arg(powershell_bin()?);
            cmd.args(PS_SESSION).arg(command);
        }
    }
    // 控制台宿主开自己的新窗即终端本体;wt/第三方的 CLI stub 是 console
    // 子系统,NO_WINDOW 压掉从 GUI 进程起动时闪现的空控制台(对 GUI 子进程
    // 无效果)
    cmd.creation_flags(match term {
        TerminalApp::Cmd | TerminalApp::Pwsh | TerminalApp::WindowsPowershell => CREATE_NEW_CONSOLE,
        TerminalApp::WindowsTerminal | TerminalApp::Alacritty | TerminalApp::WezTerm => {
            CREATE_NO_WINDOW
        }
    });
    spawn_and_reap(cmd)?;
    Ok(())
}

/// 终端图标提取:Windows 得走 SHGetFileInfo/ExtractIcon + HICON→PNG 编码
/// 一整条 GDI 链,v1 不做——UI 对无图标的终端行本就有无图兜底。与 Linux
/// 同策略,顺手在启动的 background 线程里预热 installed_terminals。
pub fn ensure_app_icons(_cache_dir: &Path) -> HashMap<String, PathBuf> {
    let _ = installed_terminals();
    HashMap::new()
}

/// Win32 剪贴板(CF_UNICODETEXT),UTF-16 直达。别家占着剪贴板时
/// OpenClipboard 会瞬时失败(剪贴板管理器常见),小步重试几轮;仍失败
/// 返回 false——调用方(clipboard_fallback)据此决定还敢不敢说 "copied"。
///
/// **必须带 owner 窗口**:OpenClipboard(NULL) 之后 EmptyClipboard 会把
/// clipboard owner 置空,而 owner 为空时 SetClipboardData 按文档必失败
/// ——即"清掉用户剪贴板 + 永远写不进去"。wake-core 够不到 gpui 的窗口,
/// 故就地开一个 message-only 窗口(HWND_MESSAGE 父级,不进 z-order、不
/// 收广播、无绘制),用完即毁。类名借系统预注册的 "STATIC",免注册。
///
/// 另:写入成功前不碰 EmptyClipboard——失败路径不该顺手毁掉用户已有内容。
pub(super) fn copy_to_clipboard(text: &str) -> bool {
    use windows_sys::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
    };
    // GlobalFree 归 Foundation(windows-sys 的分模块与 MSDN 头文件不同名)
    use windows_sys::Win32::Foundation::GlobalFree;
    use windows_sys::Win32::System::Memory::{
        GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, HWND_MESSAGE,
    };
    // windows-sys 里它归 Win32_System_Ole feature,为一个钉死的小常量不拉
    const CF_UNICODETEXT: u32 = 13;

    let wide_text = wide(text);
    let bytes = wide_text.len() * 2;
    unsafe {
        let class = wide("STATIC");
        // 样式在 windows-sys 里是裸 u32(非 windows crate 的 newtype)
        let owner = CreateWindowExW(
            0,
            class.as_ptr(),
            std::ptr::null(),
            0,
            0,
            0,
            0,
            0,
            HWND_MESSAGE,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        );
        if owner.is_null() {
            return false;
        }
        let mut opened = false;
        for _ in 0..5 {
            if OpenClipboard(owner) != 0 {
                opened = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        if !opened {
            DestroyWindow(owner);
            return false;
        }
        let ok = 'set: {
            // 先备好数据,拿到手再清——清完才发现分配不出来的话,用户
            // 原有剪贴板就白丢了
            let hmem = GlobalAlloc(GMEM_MOVEABLE, bytes);
            if hmem.is_null() {
                break 'set false;
            }
            let dst = GlobalLock(hmem);
            if dst.is_null() {
                GlobalFree(hmem);
                break 'set false;
            }
            std::ptr::copy_nonoverlapping(wide_text.as_ptr().cast::<u8>(), dst.cast::<u8>(), bytes);
            GlobalUnlock(hmem);
            if EmptyClipboard() == 0 {
                GlobalFree(hmem);
                break 'set false;
            }
            if SetClipboardData(CF_UNICODETEXT, hmem).is_null() {
                // 交接失败,所有权仍在我们手上
                GlobalFree(hmem);
                break 'set false;
            }
            // 成功后内存归系统所有,不得再碰
            true
        };
        CloseClipboard();
        DestroyWindow(owner);
        ok
    }
}

/// 批量删进回收站(trash crate → IFileOperation + FOF_ALLOWUNDO,资源管理
/// 器里可恢复;COM 初始化由 trash 自理;收 mod.rs 已过滤的真实路径)
pub(super) fn trash_existing(paths: &[&str]) -> anyhow::Result<()> {
    trash::delete_all(paths).map_err(|e| anyhow::anyhow!("Failed to move to Recycle Bin: {e}"))
}

/// 致命错误对话框:MessageBoxW。release 构建挂 windows 子系统,stderr
/// 无处可去,这是 GPUI 窗口起不来时唯一的可见通道。
pub(super) fn alert_dialog(message: &str) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        MessageBoxW, MB_ICONERROR, MB_OK, MB_SETFOREGROUND, MB_TOPMOST,
    };
    let text = wide(message);
    let caption = wide("Wake can't start");
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            text.as_ptr(),
            caption.as_ptr(),
            MB_OK | MB_ICONERROR | MB_TOPMOST | MB_SETFOREGROUND,
        );
    }
}

/// explorer.exe 的绝对路径。裸名走 CreateProcess 查找序,而那个序列把
/// **当前目录排在 System32 之前**——Wake 从解包目录(zip 直接解到下载夹)
/// 启动时,旁边放一个 explorer.exe 就会被当成它执行。本文件其余 spawn 都
/// 用 resolve_cli 给的绝对路径,这两处不能例外。
fn explorer_exe() -> PathBuf {
    let root = std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
    PathBuf::from(root).join("explorer.exe")
}

/// 资源管理器只认反斜杠:`/` 拼出来的路径它会当成参数解析失败、转而打开
/// 默认视图(既不进目录也不选中)。展示层可以留用户手输的形态,交给
/// explorer 前必须归一。
fn win_path(path: &str) -> String {
    path.replace('/', "\\")
}

/// 在资源管理器里进入目录(explorer 常驻单实例、前台进程即刻退出,
/// 且退出码恒非零,spawn 成功即算送达)
pub(super) fn open_dir(path: &str) {
    let mut cmd = Command::new(explorer_exe());
    cmd.arg(win_path(path));
    let _ = spawn_and_reap(cmd);
}

/// 选中文件(收 mod.rs 已剥好虚拟后缀的真实路径)。`/select,"path"` 必须
/// 整体一个参数且引号形制固定——explorer 自解析命令行、不吃 argv 转义,
/// raw_arg 原样直达。
pub(super) fn reveal_path(path: &str) {
    let mut cmd = Command::new(explorer_exe());
    cmd.raw_arg(format!("/select,\"{}\"", win_path(path)));
    let _ = spawn_and_reap(cmd);
}
