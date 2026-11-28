//! 开机自启、静默启动、托盘与全局热键。
//!
//! 偏好存在 `<config dir>/wake/startup.json`（`prefs`），和外观/语言/窗口几何一样
//! 不依赖数据库——托盘与"要不要开窗"在 Workbench/Store 建立之前就要决定。
//!
//! Windows 上托盘图标与全局热键跑在一条专用线程：`tray-icon` 与 `global-hotkey`
//! 都在创建它们的线程上要求一个 Win32 消息循环。那条线程泵消息（让两者的隐藏窗口
//! 收到通知）、把事件转发给 GPUI，再也不碰 UI 对象。GPUI 主线程通过 mpsc 收事件，
//! 走既有的 `show_main_window` / `main_window::quit` 路径，避免在窗口被借出时重入。
//!
//! GPUI 的 Windows 事件循环按 `GetMessage` 一直跑到 `PostQuitMessage`，不随"最后一个
//! 窗口关闭"退出——所以关窗到托盘只是销毁窗口，进程与托盘图标仍在，热键/点击能重开。

use serde::{Deserialize, Serialize};

const PREF: &str = "startup.json";

/// 默认热键：和搜索的 Ctrl/Cmd+K 不冲突，也不占系统保留组合。
pub const DEFAULT_HOTKEY: &str = "Ctrl+Shift+W";

/// 设置页下拉里的候选（同时是 `parse_hotkey` 支持的写法）。
pub const HOTKEY_OPTIONS: &[&str] = &[
    "Ctrl+Shift+W",
    "Ctrl+Alt+W",
    "Ctrl+Shift+K",
    "Alt+Shift+W",
];

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartupPrefs {
    /// 登录 Windows 时自动运行 Wake（HKCU Run 键带 `--autostart` 参数）。
    #[serde(default)]
    pub autostart: bool,
    /// 仅在开机自启时生效：不弹主窗，静默躲在托盘里。
    #[serde(default)]
    pub silent_start: bool,
    /// 关掉主窗时留在托盘（否则关窗退出程序）。
    #[serde(default)]
    pub minimize_to_tray: bool,
    /// 全局热键，用来从托盘唤出主窗。
    #[serde(default = "default_hotkey")]
    pub hotkey: String,
}

fn default_hotkey() -> String {
    DEFAULT_HOTKEY.to_string()
}

impl Default for StartupPrefs {
    fn default() -> Self {
        Self {
            autostart: false,
            silent_start: false,
            minimize_to_tray: false,
            hotkey: default_hotkey(),
        }
    }
}

/// 托盘/热键->GPUI 的事件。
#[derive(Clone, Copy, Debug)]
pub enum TrayEvent {
    /// 点托盘图标或按热键：把主窗拉回前台（没有就重建）。
    Show,
    /// 托盘菜单里的 Quit：正常退出，给 on_app_quit 落盘的机会。
    Quit,
}

pub fn load() -> StartupPrefs {
    crate::prefs::read(PREF)
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// 落盘偏好，并把自启注册表项与热键同步到系统。
pub fn save(prefs: &StartupPrefs) -> std::io::Result<()> {
    let bytes = serde_json::to_vec_pretty(prefs).map_err(std::io::Error::other)?;
    crate::prefs::write(PREF, &bytes)?;
    #[cfg(windows)]
    {
        platform::set_autostart(prefs.autostart)?;
        platform::send_hotkey(&prefs.hotkey);
    }
    Ok(())
}

/// 开机自启的命令行标记：`silent_start` 只对这条路径生效，用户手动双击仍看到主窗。
pub fn launched_at_boot() -> bool {
    std::env::args().any(|arg| arg == "--autostart")
}

/// 启动托盘线程。非 Windows 上是空操作（返回后 rx 永远不会有事件）。
pub fn start(prefs: &StartupPrefs, events: futures::channel::mpsc::UnboundedSender<TrayEvent>) {
    #[cfg(windows)]
    platform::start(prefs.clone(), events);
    #[cfg(not(windows))]
    {
        let _ = (prefs, events);
    }
}

/// 关主窗时要不要退出：Linux 总是退出；Windows 上关窗到托盘开启时留在托盘。
pub fn close_quits_app() -> bool {
    if cfg!(target_os = "linux") {
        true
    } else if cfg!(windows) {
        !load().minimize_to_tray
    } else {
        false
    }
}

#[cfg(windows)]
mod platform {
    use super::*;
    use global_hotkey::hotkey::{Code, HotKey, Modifiers};
    use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager};
    use std::sync::{mpsc, OnceLock};
    use std::time::Duration;
    use tray_icon::menu::{Menu, MenuEvent, MenuItem};
    use tray_icon::{Icon, TrayIconBuilder, TrayIconEvent};

    /// 设置页改动热键时从 GPUI 线程发给托盘线程的命令。
    enum TrayCommand {
        SetHotkey(String),
    }

    static CMD_TX: OnceLock<mpsc::Sender<TrayCommand>> = OnceLock::new();

    /// HKCU Run 键：只在当前用户下自启，不需要管理员权限，也不会写到系统目录。
    pub(super) fn set_autostart(enabled: bool) -> std::io::Result<()> {
        use winreg::enums::HKEY_CURRENT_USER;
        use winreg::RegKey;

        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let (key, _) = hkcu.create_subkey(r"Software\Microsoft\Windows\CurrentVersion\Run")?;
        if enabled {
            let exe = std::env::current_exe()?;
            let command = format!("\"{}\" --autostart", exe.to_string_lossy());
            key.set_value("Wake", &command)?;
        } else {
            // 值不存在时删除会报 NotFound，按"已经是关闭状态"处理
            let _ = key.delete_value("Wake");
        }
        Ok(())
    }

    pub(super) fn send_hotkey(spec: &str) {
        if let Some(tx) = CMD_TX.get() {
            let _ = tx.send(TrayCommand::SetHotkey(spec.to_string()));
        }
    }

    fn parse_hotkey(spec: &str) -> Option<(Modifiers, Code)> {
        let mut mods = Modifiers::empty();
        let mut code = None;
        for part in spec.split('+') {
            match part.trim().to_ascii_lowercase().as_str() {
                "ctrl" | "control" => mods |= Modifiers::CONTROL,
                "shift" => mods |= Modifiers::SHIFT,
                "alt" => mods |= Modifiers::ALT,
                "w" => code = Some(Code::KeyW),
                "k" => code = Some(Code::KeyK),
                "space" => code = Some(Code::Space),
                _ => return None,
            }
        }
        code.map(|c| (mods, c))
    }

    fn tray_icon() -> Option<Icon> {
        let image = image::load_from_memory(include_bytes!("../assets/tray.png")).ok()?;
        let image = image.to_rgba8();
        Icon::from_rgba(image.as_raw().clone(), image.width(), image.height()).ok()
    }

    /// 泵该线程上 tray-icon / global-hotkey 隐藏窗口的消息。
    fn pump_messages() {
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            DispatchMessageW, PeekMessageW, TranslateMessage, MSG, PM_REMOVE,
        };
        unsafe {
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }

    pub(super) fn start(
        prefs: StartupPrefs,
        events: futures::channel::mpsc::UnboundedSender<TrayEvent>,
    ) {
        let (cmd_tx, cmd_rx) = mpsc::channel::<TrayCommand>();
        let _ = CMD_TX.set(cmd_tx);

        let spawned = std::thread::Builder::new()
            .name("wake-tray".to_string())
            .spawn(move || {
                // 托盘图标与菜单
                let mut tray = None;
                let mut show_id = None;
                let mut quit_id = None;
                if let Some(icon) = tray_icon() {
                    let menu = Menu::new();
                    let show = MenuItem::new("Show Wake", true, None);
                    let quit = MenuItem::new("Quit Wake", true, None);
                    if menu.append(&show).is_ok() && menu.append(&quit).is_ok() {
                        show_id = Some(show.id().clone());
                        quit_id = Some(quit.id().clone());
                        tray = TrayIconBuilder::new()
                            .with_menu(Box::new(menu))
                            .with_tooltip("Wake")
                            .with_icon(icon)
                            .build()
                            .ok();
                    }
                }

                // 全局热键。管理器必须活到线程结束，且注册/注销都在本线程。
                let manager = GlobalHotKeyManager::new().ok();
                let mut current = prefs.hotkey.clone();
                let mut registered: Option<HotKey> = None;
                if let (Some(m), Some((mods, code))) = (&manager, parse_hotkey(&current)) {
                    let hotkey = HotKey::new(Some(mods), code);
                    if m.register(hotkey).is_ok() {
                        registered = Some(hotkey);
                    }
                }

                loop {
                    // 设置页改了热键：先注销旧键再注册新键
                    if let Ok(TrayCommand::SetHotkey(spec)) = cmd_rx.try_recv() {
                        if spec != current {
                            current = spec.clone();
                            if let (Some(m), Some(old)) = (&manager, registered.take()) {
                                let _ = m.unregister(old);
                            }
                            if let (Some(m), Some((mods, code))) = (&manager, parse_hotkey(&spec)) {
                                let hotkey = HotKey::new(Some(mods), code);
                                if m.register(hotkey).is_ok() {
                                    registered = Some(hotkey);
                                }
                            }
                        }
                    }

                    // 点图标（单击抬起或双击）即唤出
                    while let Ok(event) = TrayIconEvent::receiver().try_recv() {
                        let show = match event {
                            TrayIconEvent::Click {
                                button: tray_icon::MouseButton::Left,
                                button_state: tray_icon::MouseButtonState::Up,
                                ..
                            }
                            | TrayIconEvent::DoubleClick { .. } => true,
                            _ => false,
                        };
                        if show {
                            let _ = events.unbounded_send(TrayEvent::Show);
                        }
                    }

                    // 右键菜单
                    while let Ok(event) = MenuEvent::receiver().try_recv() {
                        if show_id.as_ref().is_some_and(|id| *id == event.id) {
                            let _ = events.unbounded_send(TrayEvent::Show);
                        } else if quit_id.as_ref().is_some_and(|id| *id == event.id) {
                            let _ = events.unbounded_send(TrayEvent::Quit);
                        }
                    }

                    // 热键按下（抬键不算，避免连发）
                    while let Ok(event) = GlobalHotKeyEvent::receiver().try_recv() {
                        if event.state == global_hotkey::HotKeyState::Pressed
                            && registered.as_ref().is_some_and(|h| h.id() == event.id)
                        {
                            let _ = events.unbounded_send(TrayEvent::Show);
                        }
                    }

                    pump_messages();
                    std::thread::sleep(Duration::from_millis(25));
                    // tray 需在循环里保持存活（Drop 会移除图标）
                    let _ = &tray;
                }
            });
        if spawned.is_err() {
            // 托盘起不来不该挡住主程序；热键/托盘不可用但 Wake 照常运行
            eprintln!("wake: failed to start tray thread");
        }
    }
}
