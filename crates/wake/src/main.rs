// release 挂 windows 子系统:双击启动不带控制台黑窗。debug 保留控制台,
// eprintln 的诊断日志还有处落;GUI 子系统下致命错误走 MessageBox
//(wake-core 的 show_fatal_alert),不依赖 stderr。
#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]

mod assets;
mod format;
mod main_window;
mod prefs;
mod settings;
mod theme;
mod ui;
mod update;
mod workbench;

use assets::Assets;
use gpui::*;
use gpui_component::Root;
use main_window::MainWindow;
use workbench::{
    OpenAbout, OpenSettings, OpenUpdates, PaletteDown, PaletteUp, RefreshSessions, ToggleSearch,
    Workbench, KEY_CONTEXT, PALETTE_CONTEXT,
};

actions!(
    wake_app,
    [
        Quit,
        CloseWindow,
        Hide,
        HideOthers,
        ShowAll,
        Minimize,
        Zoom,
        ToggleFullScreen,
        ShowMainWindow
    ]
);

/// 开主窗并登记句柄。启动、Dock 重开、Window → Main Window 共用
fn open_main_window(cx: &mut App) -> anyhow::Result<WindowHandle<Root>> {
    // 上次关掉时在哪块屏、什么位置就开回哪;bounds 是所选屏幕内的相对坐标,
    // 必须和 display_id 一起给(见 main_window 模块头)
    let (window_bounds, display_id) = main_window::restore(cx);
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
    let handle = cx.open_window(
        WindowOptions {
            titlebar: Some(titlebar),
            window_bounds: Some(window_bounds),
            display_id,
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
            cx.new(|cx| {
                main_window::track(window, cx);
                Root::new(workbench, window, cx)
            })
        },
    )?;
    cx.set_global(MainWindow(handle));
    Ok(handle)
}

/// 开窗失败必须自己报:release 的 Windows 子系统没有 stderr,panic 消息无处
/// 可去,用户看到的就是任务栏闪一下然后什么都没有(GPU/驱动起不来、RDP 会话
/// 等都会走到这)。show_fatal_alert 会弹系统对话框,并始终先往 stderr 落一份
///(2026-08-25 review)。`what` 是 "open"/"reopen",拼进提示语
fn open_main_window_or_exit(cx: &mut App, what: &str) -> WindowHandle<Root> {
    match open_main_window(cx) {
        Ok(handle) => handle,
        Err(e) => {
            wake_core::services::terminal::show_fatal_alert(&format!(
                "Wake couldn't {what} its window: {e}"
            ));
            std::process::exit(1);
        }
    }
}

/// 主窗还在就拉到前台并交出它的 Workbench;窗已关(update 失败)给 None。
/// 必须在没有窗口被借出时调用——defer 里,或刚开完窗
fn activate_main(handle: WindowHandle<Root>, cx: &mut App) -> Option<Entity<Workbench>> {
    handle
        .update(cx, |root, window, _| {
            window.activate_window();
            root.view().clone().downcast::<Workbench>().ok()
        })
        .ok()
        .flatten()
}

/// 主窗还在就拉到前台,否则重建;随后在主窗的 Workbench 上执行 `then`(无窗时
/// 菜单里点 Settings/About 就靠它转发)。整段推迟到下一轮前台任务里跑,两个
/// 原因:①全局 action 监听器是在派发它的那扇窗口的 update 期间被调用的,此刻
/// 该窗口已被借出,再 update 它必失败(gpui 不重入)——同步判断会把活着的主窗
/// 误判成已关;②不能用 `cx.defer`:Dock 的 `on_reopen` 回调是裸 `borrow_mut`,
/// 不在任何 `App::update` 里,defer 压进队列后没人 flush,无窗又无任务时会
/// 一直挂着(2026-09-02 Codex review)。前台任务经 `AsyncApp::update` 执行,
/// 那条路必然 flush,内层的 defer 也在同一轮里处理
fn show_main_window(
    cx: &mut App,
    then: impl FnOnce(&mut Workbench, &mut Context<Workbench>) + 'static,
) {
    cx.spawn(async move |cx| {
        cx.update(|cx| show_main_window_now(cx, then));
    })
    .detach();
}

/// `show_main_window` 的正文。要在 `App::update` 里、且没有窗口被借出时调用
fn show_main_window_now(
    cx: &mut App,
    then: impl FnOnce(&mut Workbench, &mut Context<Workbench>) + 'static,
) {
    let handle = cx.try_global::<MainWindow>().map(|main| main.0);
    if let Some(workbench) = handle.and_then(|handle| activate_main(handle, cx)) {
        cx.activate(true);
        workbench.update(cx, then);
        return;
    }
    // 主窗已关。Settings 允许比主窗活得久(它持有 Workbench,事件泵跟
    // entity 走),但那个 Workbench 的 list/input 订阅都绑在旧主窗上,挂进
    // 新窗不会再收事件——所以设计上是:先关掉 Settings 让旧 Workbench
    // 释放(watcher 随之 join),再建全新的主窗。开窗放到下一个 defer:
    // 释放发生在两次 defer 之间的 release 阶段,同一轮里建会让新旧
    // watcher 并存
    for w in cx.windows() {
        w.update(cx, |_, window, _| window.remove_window()).ok();
    }
    cx.defer(move |cx| {
        let handle = open_main_window_or_exit(cx, "reopen");
        cx.activate(true);
        if let Some(workbench) = activate_main(handle, cx) {
            workbench.update(cx, then);
        }
    });
}

/// 窗口级操作(关/最小化/缩放/全屏)从全局监听器发起时,目标窗口正处在派发
/// 这个 action 的 update 中(键位与菜单两条路都如此),被借出的窗口不能再次
/// update——返回 Err 被 .ok() 吞掉就是"⌘W 没反应"。推迟到本轮 update 结束再动
fn with_active_window(cx: &mut App, f: impl FnOnce(&mut Window) + 'static) {
    let Some(w) = cx.active_window() else {
        return;
    };
    cx.defer(move |cx| {
        w.update(cx, |_, window, _| f(window)).ok();
    });
}

fn key_bindings() -> Vec<KeyBinding> {
    // secondary = macOS 的 cmd、其他平台的 ctrl(gpui keystroke 内建别名)。
    // macOS 标准的应用/窗口键位 gpui 不会自动处理,没有绑定就是死键;
    // Linux/Windows 上 ctrl-h/ctrl-m 另有含义,不绑
    let mut keys = vec![
        KeyBinding::new(ui::SEARCH_KEYSTROKE, ToggleSearch, Some(KEY_CONTEXT)),
        KeyBinding::new("secondary-r", RefreshSessions, Some(KEY_CONTEXT)),
        KeyBinding::new("secondary-,", OpenSettings, None),
        KeyBinding::new("secondary-q", Quit, None),
        KeyBinding::new("secondary-w", CloseWindow, None),
        // ⌘K 面板:焦点在搜索输入框,↑↓ 冒泡到面板容器挪选中
        KeyBinding::new("up", PaletteUp, Some(PALETTE_CONTEXT)),
        KeyBinding::new("down", PaletteDown, Some(PALETTE_CONTEXT)),
    ];
    if cfg!(target_os = "macos") {
        keys.extend([
            KeyBinding::new("secondary-h", Hide, None),
            KeyBinding::new("secondary-alt-h", HideOthers, None),
            KeyBinding::new("secondary-m", Minimize, None),
            KeyBinding::new("ctrl-secondary-f", ToggleFullScreen, None),
        ]);
    }
    keys
}

/// 菜单栏。gpui 只按这里给的生成,不补任何标准项——Edit/Window/Hide 一族在
/// macOS 上必须自己给,否则对应快捷键全是死键
fn app_menus() -> Vec<Menu> {
    let mac = cfg!(target_os = "macos");
    let wake: Vec<MenuItem> = [
        Some(MenuItem::action("About Wake", OpenAbout)),
        mac.then(|| MenuItem::action("Check for Updates…", OpenUpdates)),
        Some(MenuItem::separator()),
        Some(MenuItem::action("Settings…", OpenSettings)),
        Some(MenuItem::separator()),
        mac.then(|| MenuItem::action("Hide Wake", Hide)),
        mac.then(|| MenuItem::action("Hide Others", HideOthers)),
        mac.then(|| MenuItem::action("Show All", ShowAll)),
        mac.then(MenuItem::separator),
        Some(MenuItem::action("Quit Wake", Quit)),
    ]
    .into_iter()
    .flatten()
    .collect();
    let file = vec![
        MenuItem::action("Refresh Sessions", RefreshSessions),
        MenuItem::separator(),
        MenuItem::action("Close Window", CloseWindow),
    ];
    // os_action:菜单项挂原生 cut:/copy:/paste:/selectAll: 选择器。gpui 窗口
    // 在前时由 gpui 接住、派发成右侧的 action(输入框消费);系统面板(目录
    // 选择器)在前时走原生响应链——没有这组菜单项,面板里的 ⌘C/⌘V 无人应答
    let edit = mac.then(|| Menu {
        name: "Edit".into(),
        items: vec![
            MenuItem::os_action("Undo", gpui_component::input::Undo, OsAction::Undo),
            MenuItem::os_action("Redo", gpui_component::input::Redo, OsAction::Redo),
            MenuItem::separator(),
            MenuItem::os_action("Cut", gpui_component::input::Cut, OsAction::Cut),
            MenuItem::os_action("Copy", gpui_component::input::Copy, OsAction::Copy),
            MenuItem::os_action("Paste", gpui_component::input::Paste, OsAction::Paste),
            MenuItem::os_action(
                "Select All",
                gpui_component::input::SelectAll,
                OsAction::SelectAll,
            ),
        ],
        disabled: false,
    });
    // 名字必须是 "Window":gpui 据此把它登记为系统 windows menu,窗口列表由
    // AppKit 自动追加在后面。只剩 Settings 时 Dock 点击不会触发 on_reopen
    //(系统认为还有可见窗口),Main Window 是拉回主窗的唯一入口
    let window = mac.then(|| Menu {
        name: "Window".into(),
        items: vec![
            MenuItem::action("Minimize", Minimize),
            MenuItem::action("Zoom", Zoom),
            MenuItem::action("Toggle Full Screen", ToggleFullScreen),
            MenuItem::separator(),
            MenuItem::action("Main Window", ShowMainWindow),
        ],
        disabled: false,
    });
    [
        Some(Menu {
            name: "Wake".into(),
            items: wake,
            disabled: false,
        }),
        Some(Menu {
            name: "File".into(),
            items: file,
            disabled: false,
        }),
        edit,
        window,
    ]
    .into_iter()
    .flatten()
    .collect()
}

fn main() {
    let app = gpui_platform::application().with_assets(Assets);
    // macOS:用户关掉最后一个窗后进程仍留 Dock,点 Dock 图标走 NSApplication 的
    // applicationShouldHandleReopen → gpui Platform::on_reopen(只在系统认为
    // 没有可见窗口时触发)。与 Window → Main Window 同一条路
    app.on_reopen(|cx| show_main_window(cx, |_, _| {}));
    app.run(move |cx: &mut App| {
        gpui_component::init(cx);
        gpui_component::set_locale("en");
        theme::sync_appearance(None, cx);

        cx.on_action(|_: &Quit, cx| cx.quit());
        // 退出前把主窗几何落盘(节流写盘可能还没触发);Dock 的 Quit、注销关机
        // 都走这里,不只 ⌘Q
        cx.on_app_quit(|cx| {
            main_window::flush(cx);
            async {}
        })
        .detach();
        cx.on_action(|_: &CloseWindow, cx| with_active_window(cx, |w| w.remove_window()));
        cx.on_action(|_: &Minimize, cx| with_active_window(cx, |w| w.minimize_window()));
        cx.on_action(|_: &Zoom, cx| with_active_window(cx, |w| w.zoom_window()));
        cx.on_action(|_: &ToggleFullScreen, cx| with_active_window(cx, |w| w.toggle_fullscreen()));
        cx.on_action(|_: &Hide, cx| cx.hide());
        cx.on_action(|_: &HideOthers, cx| cx.hide_other_apps());
        cx.on_action(|_: &ShowAll, cx| cx.unhide_other_apps());
        cx.on_action(|_: &ShowMainWindow, cx| show_main_window(cx, |_, _| {}));
        // 无窗时的菜单兜底:有窗时这些 action 由 Workbench/Settings 的视图
        // 消费(元素级监听器默认截断传播,不会走到这);全部窗口关掉后菜单项
        // 只认全局监听器,没有就置灰——先把主窗拉回来再转发
        cx.on_action(|_: &OpenSettings, cx| show_main_window(cx, Workbench::open_settings));
        cx.on_action(|_: &OpenAbout, cx| show_main_window(cx, Workbench::open_about));
        cx.on_action(|_: &OpenUpdates, cx| show_main_window(cx, Workbench::open_updates));

        cx.bind_keys(key_bindings());
        cx.set_menus(app_menus());

        open_main_window_or_exit(cx, "open");
        cx.activate(true);
    });
}
