mod assets;
mod format;
mod session_list;
mod theme;
mod ui;
mod workbench;

use assets::Assets;
use gpui::*;
use gpui_component::Root;
use workbench::{
    PaletteDown, PaletteUp, RefreshSessions, ToggleSearch, Workbench, KEY_CONTEXT, PALETTE_CONTEXT,
};

actions!(wake_app, [Quit, CloseWindow]);

fn main() {
    let app = Application::new().with_assets(Assets);
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
        cx.bind_keys([
            KeyBinding::new("cmd-k", ToggleSearch, Some(KEY_CONTEXT)),
            KeyBinding::new("cmd-r", RefreshSessions, Some(KEY_CONTEXT)),
            KeyBinding::new("cmd-q", Quit, None),
            KeyBinding::new("cmd-w", CloseWindow, None),
            // ⌘K 面板:焦点在搜索输入框,↑↓ 冒泡到面板容器挪选中
            KeyBinding::new("up", PaletteUp, Some(PALETTE_CONTEXT)),
            KeyBinding::new("down", PaletteDown, Some(PALETTE_CONTEXT)),
        ]);
        cx.set_menus(vec![
            Menu {
                name: "Wake".into(),
                items: vec![MenuItem::action("Quit Wake", Quit)],
            },
            Menu {
                name: "File".into(),
                items: vec![
                    MenuItem::action("Refresh Sessions", RefreshSessions),
                    MenuItem::separator(),
                    MenuItem::action("Close Window", CloseWindow),
                ],
            },
        ]);

        let bounds = Bounds::centered(None, size(px(1180.), px(760.)), cx);
        cx.open_window(
            WindowOptions {
                titlebar: Some(TitlebarOptions {
                    title: None,
                    appears_transparent: true,
                    traffic_light_position: Some(point(px(20.), px(11.))),
                }),
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_min_size: Some(size(px(940.), px(620.))),
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
                window.focus(&workbench.read(cx).focus_handle(cx));
                cx.new(|cx| Root::new(workbench, window, cx))
            },
        )
        .expect("failed to open window");
        cx.activate(true);
    });
}
