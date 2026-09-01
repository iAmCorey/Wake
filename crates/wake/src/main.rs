// release 挂 windows 子系统:双击启动不带控制台黑窗。debug 保留控制台,
// eprintln 的诊断日志还有处落;GUI 子系统下致命错误走 MessageBox
//(wake-core 的 show_fatal_alert),不依赖 stderr。
#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]

mod assets;
mod format;
mod settings;
mod theme;
mod ui;
mod update;
mod workbench;

use assets::Assets;
use gpui::*;
use gpui_component::Root;
use workbench::{
    OpenAbout, OpenSettings, OpenUpdates, PaletteDown, PaletteUp, RefreshSessions, ToggleSearch,
    Workbench, KEY_CONTEXT, PALETTE_CONTEXT,
};

actions!(wake_app, [Quit, CloseWindow]);

// macOS:用户关掉最后一个窗后进程仍留 Dock,点 Dock 图标走 NSApplication 的
// applicationShouldHandleReopen → gpui Platform::on_reopen。这里复用启动期
// 的开窗配置,空窗重建、有窗只是把已存在的拉到前台,不重复实例
fn open_main_window(cx: &mut App) -> anyhow::Result<WindowHandle<Root>> {
    let bounds = Bounds::centered(None, size(px(1180.), px(760.)), cx);
    // macOS:隐藏系统标题栏、内容顶到窗顶,traffic light 悬浮在侧栏上;
    // Linux/Windows:标准系统标题栏(appears_transparent=false)。Windows
    // 后端按此保留原生 caption(min/max/close、snap layouts、深色模式随
    // 系统由 gpui 设 DWMWA_USE_IMMERSIVE_DARK_MODE),title 即窗名;
    // GNOME Wayland 不给 SSD 时回落 CSD,见 window_decorations 注释。
    // cfg! 而非 #[cfg]:两支在任一平台都参与类型检查,别让另一支只有 CI 见得到
    let titlebar = if cfg!(target_os = "macos") {
        TitlebarOptions {
            title: None,
            appears_transparent: true,
            // 44px Wake 顶部净空中垂直居中 13.5px traffic lights。
            traffic_light_position: Some(point(px(20.), px(15.))),
        }
    } else {
        TitlebarOptions {
            title: Some("Wake".into()),
            appears_transparent: false,
            traffic_light_position: None,
        }
    };
    cx.open_window(
        WindowOptions {
            titlebar: Some(titlebar),
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            window_min_size: Some(size(px(940.), px(620.))),
            // Linux 桌面按它归组窗口、匹配 .desktop(StartupWMClass=wake)
            app_id: Some("wake".into()),
            // Wayland 显式请求 CSD(2026-08-24 Codex review):默认的 Server
            // 请求在 GNOME/Mutter(无 zxdg-decoration 协议)下会被 gpui 记成
            // Server 而 compositor 实际什么都不画——窗口既无系统标题栏、
            // workbench 又按 Server 不挂 TitleBar,彻底没有关窗/拖拽面。
            // 请求 Client 后:Wayland 全家走 CSD(TitleBar 补位),X11 侧
            // gpui 探测 compositor 不支持 CSD 时仍自动回报 Server(WM 标题
            // 栏照常、TitleBar 不挂),macOS/Windows 忽略此字段(Windows
            // 后端不实现 request_decorations,runtime 恒报 Server)
            window_decorations: Some(WindowDecorations::Client),
            ..Default::default()
        },
        |window, cx| {
            // 跟随系统深浅色切换
            window
                .observe_window_appearance(|window, cx| {
                    theme::sync_appearance(Some(window), cx);
                })
                .detach();
            theme::sync_appearance(Some(window), cx);

            let workbench = cx.new(|cx| Workbench::new(window, cx));
            window.focus(&workbench.read(cx).focus_handle(cx), cx);
            cx.new(|cx| Root::new(workbench, window, cx))
        },
    )
}

fn main() {
    let app = gpui_platform::application().with_assets(Assets);
    // macOS:用户关掉最后一个窗后进程仍留 Dock,点 Dock 图标走 NSApplication 的
    // applicationShouldHandleReopen → gpui Platform::on_reopen。空窗重建、
    // 有窗只是把已存在的拉到前台,不重复实例
    app.on_reopen(|cx| {
        if cx.windows().is_empty() {
            if let Err(e) = open_main_window(cx) {
                wake_core::services::terminal::show_fatal_alert(&format!(
                    "Wake couldn't reopen its window: {e}"
                ));
                std::process::exit(1);
            }
        }
        cx.activate(true);
    });
    app.run(move |cx: &mut App| {
        gpui_component::init(cx);
        gpui_component::set_locale("en");
        theme::sync_appearance(None, cx);

        cx.on_action(|_: &Quit, cx| cx.quit());
        cx.on_action(|_: &CloseWindow, cx| {
            if let Some(w) = cx.active_window() {
                w.update(cx, |_, window, _| window.remove_window()).ok();
            }
        });
        // secondary = macOS 的 cmd、其他平台的 ctrl(gpui keystroke 内建别名)
        cx.bind_keys([
            KeyBinding::new(ui::SEARCH_KEYSTROKE, ToggleSearch, Some(KEY_CONTEXT)),
            KeyBinding::new("secondary-r", RefreshSessions, Some(KEY_CONTEXT)),
            KeyBinding::new("secondary-,", OpenSettings, None),
            KeyBinding::new("secondary-q", Quit, None),
            KeyBinding::new("secondary-w", CloseWindow, None),
            // ⌘K 面板:焦点在搜索输入框,↑↓ 冒泡到面板容器挪选中
            KeyBinding::new("up", PaletteUp, Some(PALETTE_CONTEXT)),
            KeyBinding::new("down", PaletteDown, Some(PALETTE_CONTEXT)),
        ]);
        let mut wake_menu_items = vec![MenuItem::action("About Wake", OpenAbout)];
        #[cfg(target_os = "macos")]
        wake_menu_items.push(MenuItem::action("Check for Updates…", OpenUpdates));
        wake_menu_items.extend([
            MenuItem::separator(),
            MenuItem::action("Settings…", OpenSettings),
            MenuItem::separator(),
            MenuItem::action("Quit Wake", Quit),
        ]);
        cx.set_menus(vec![
            Menu {
                name: "Wake".into(),
                items: wake_menu_items,
                disabled: false,
            },
            Menu {
                name: "File".into(),
                items: vec![
                    MenuItem::action("Refresh Sessions", RefreshSessions),
                    MenuItem::separator(),
                    MenuItem::action("Close Window", CloseWindow),
                ],
                disabled: false,
            },
        ]);

        let window = open_main_window(cx);
        // 开窗失败必须自己报:release 的 Windows 子系统没有 stderr,panic
        // 消息无处可去,用户看到的就是任务栏闪一下然后什么都没有
        //(GPU/驱动起不来、RDP 会话等都会走到这)。show_fatal_alert 会弹
        // 系统对话框,并始终先往 stderr 落一份(2026-08-25 review)
        if let Err(e) = window {
            wake_core::services::terminal::show_fatal_alert(&format!(
                "Wake couldn't open its window: {e}"
            ));
            std::process::exit(1);
        }
        cx.activate(true);
    });
}
