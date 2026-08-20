//! Wake 的双模式主题。
//! macOS 保留暖色原生风格；Windows 使用更接近 Fluent 的中性灰、蓝色强调和
//! 低圆角控件层级。
use gpui::{App, Hsla, Rgba, Window};
use gpui_component::{ActiveTheme as _, Theme, ThemeMode};

/// 带透明度的色值(文字选区等叠加层用)
fn ca(hex: u32, a: f32) -> Hsla {
    let mut col = c(hex);
    col.a = a;
    col
}

fn c(hex: u32) -> Hsla {
    Rgba {
        r: ((hex >> 16) & 0xff) as f32 / 255.0,
        g: ((hex >> 8) & 0xff) as f32 / 255.0,
        b: (hex & 0xff) as f32 / 255.0,
        a: 1.0,
    }
    .into()
}

/// 详情页 model badge 色——用户指定的 Claude 橙(非 agent 语义,纯配色选择);
/// outline badge 形态,两模式通用
pub const MODEL_BADGE_BG: u32 = 0xD97757;

/// 收藏星选中色——macOS systemYellow(Finder/Mail 星标惯例),两模式通用
pub const STAR_YELLOW: u32 = 0xFFCC00;

/// 在 gpui-component 默认主题之上覆写 Wake 的 token。
/// 每次外观切换后都要重新调用(sync_system_appearance 会重置为默认)。
pub fn apply_wake_theme(cx: &mut App) {
    let dark = cx.theme().mode.is_dark();
    let theme = Theme::global_mut(cx);

    if dark {
        // ---- 深色：石墨材质，层级从侧栏向阅读面逐步提亮 ----
        theme.background = c(0x242422);
        theme.foreground = c(0xF0EFED);
        theme.border = c(0x353431);
        theme.ring = c(0x4C8DFF);

        theme.sidebar = c(0x1B1B1A);
        theme.sidebar_foreground = c(0xD3D2CE);
        theme.sidebar_border = c(0x292927);
        theme.sidebar_accent = c(0x343432);
        theme.sidebar_accent_foreground = c(0xF5F4F1);
        theme.sidebar_primary = c(0x4C8DFF);
        theme.sidebar_primary_foreground = c(0xFFFFFF);

        theme.list = c(0x20201F);
        theme.list_hover = c(0x2A2A28);
        theme.list_active = c(0x303B4C);
        theme.list_active_border = c(0x303B4C);
        theme.list_even = c(0x20201F);
        theme.list_head = c(0x20201F);

        theme.muted = c(0x323230);
        theme.muted_foreground = c(0xA9A8A2);
        theme.accent = c(0x383836);
        theme.accent_foreground = c(0xF4F3F0);

        theme.primary = c(0x4C8DFF);
        theme.primary_hover = c(0x5E99FF);
        theme.primary_active = c(0x3D7EF0);
        theme.primary_foreground = c(0xFFFFFF);

        theme.secondary = c(0x30302E);
        theme.secondary_hover = c(0x3A3A37);
        theme.secondary_active = c(0x41413D);
        theme.secondary_foreground = c(0xECEBE8);

        theme.popover = c(0x2C2C2A);
        theme.popover_foreground = c(0xF0EFED);
        theme.group_box = c(0x2C2C2A);
        theme.group_box_foreground = c(0xF0EFED);
        theme.input = c(0x44423E);
        theme.selection = ca(0x4C8DFF, 0.32);
        theme.caret = c(0x4C8DFF);

        theme.title_bar = c(0x1B1B1A);
        theme.title_bar_border = c(0x1B1B1A);

        theme.scrollbar_thumb = c(0x3E3E3B);
        theme.scrollbar_thumb_hover = c(0x4A4A47);

        theme.link = c(0x6EA3FF);
        theme.danger = c(0xFF6B60);
        theme.danger_hover = c(0xFF7B72);
        theme.danger_active = c(0xEE5D54);
        theme.danger_foreground = c(0xFFFFFF);
        theme.success = c(0x56C789);
        theme.success_foreground = c(0x10271B);
        // 琥珀(侧栏 "Live updates off" 状态点用)
        theme.warning = c(0xD9A741);
        theme.warning_foreground = c(0x2A2105);
        theme.overlay = gpui::hsla(0., 0., 0., 0.45);
    } else {
        // ---- 浅色：银白材质，阅读卡悬在分区背景之上 ----
        theme.background = c(0xF1F1EF);
        theme.foreground = c(0x1D1D1F);
        theme.border = c(0xE3E3DF);
        theme.ring = c(0x0A84FF);

        theme.sidebar = c(0xEDEDEA);
        theme.sidebar_foreground = c(0x41413D);
        theme.sidebar_border = c(0xE1E1DD);
        theme.sidebar_accent = c(0xDEDEDA);
        theme.sidebar_accent_foreground = c(0x1D1D1F);
        theme.sidebar_primary = c(0x0A84FF);
        theme.sidebar_primary_foreground = c(0xFFFFFF);

        theme.list = c(0xF7F7F5);
        theme.list_hover = c(0xEDEDEA);
        theme.list_active = c(0xE3EBF6);
        theme.list_active_border = c(0xE3EBF6);
        theme.list_even = c(0xF7F7F5);
        theme.list_head = c(0xF7F7F5);

        theme.muted = c(0xE8E8E5);
        theme.muted_foreground = c(0x686761);
        theme.accent = c(0xE7E7E3);
        theme.accent_foreground = c(0x1D1D1F);

        theme.primary = c(0x0A84FF);
        theme.primary_hover = c(0x268FFF);
        theme.primary_active = c(0x0071E3);
        theme.primary_foreground = c(0xFFFFFF);

        theme.secondary = c(0xE8E8E5);
        theme.secondary_hover = c(0xDEDEDA);
        theme.secondary_active = c(0xD4D4D0);
        theme.secondary_foreground = c(0x2B2B2D);

        theme.popover = c(0xFDFDFC);
        theme.popover_foreground = c(0x1D1D1F);
        theme.group_box = c(0xFDFDFC);
        theme.group_box_foreground = c(0x1D1D1F);
        theme.input = c(0xDADAD6);
        theme.selection = ca(0x0A7AFF, 0.22);
        theme.caret = c(0x0A84FF);

        theme.title_bar = c(0xEDEDEA);
        theme.title_bar_border = c(0xEDEDEA);

        theme.scrollbar_thumb = c(0xD2D2CF);
        theme.scrollbar_thumb_hover = c(0xBFBFBC);

        theme.link = c(0x0A84FF);
        theme.danger = c(0xE5484D);
        theme.danger_hover = c(0xEE5C61);
        theme.danger_active = c(0xD93C42);
        theme.danger_foreground = c(0xFFFFFF);
        theme.success = c(0x2F9E63);
        theme.success_foreground = c(0xFFFFFF);
        // 琥珀(侧栏 "Live updates off" 状态点用),白字对比 4.9:1
        theme.warning = c(0xA16207);
        theme.warning_foreground = c(0xFFFFFF);
        theme.overlay = gpui::hsla(0., 0., 0., 0.25);
    }

    // Windows 采用 Fluent 的中性灰层级。单独覆盖 token，而不是在组件里散落
    // 大量平台判断，这样 hover、菜单、输入框和滚动条会保持同一套语义颜色。
    if cfg!(target_os = "windows") {
        if dark {
            theme.background = c(0x202020);
            theme.foreground = c(0xF5F5F5);
            theme.border = c(0x3A3A3A);
            theme.ring = c(0x60CDFF);

            theme.sidebar = c(0x202020);
            theme.sidebar_foreground = c(0xE6E6E6);
            theme.sidebar_border = c(0x333333);
            theme.sidebar_accent = c(0x2D2D2D);
            theme.sidebar_accent_foreground = c(0xFFFFFF);
            theme.sidebar_primary = c(0x60CDFF);
            theme.sidebar_primary_foreground = c(0x001B2E);

            theme.list = c(0x252525);
            theme.list_hover = c(0x2D2D2D);
            theme.list_active = c(0x2D4B63);
            theme.list_active_border = c(0x60CDFF);
            theme.list_even = c(0x252525);
            theme.list_head = c(0x252525);

            theme.muted = c(0x2D2D2D);
            theme.muted_foreground = c(0xB8B8B8);
            theme.accent = c(0x333333);
            theme.accent_foreground = c(0xFFFFFF);

            theme.primary = c(0x60CDFF);
            theme.primary_hover = c(0x78D4FF);
            theme.primary_active = c(0x4DB8E8);
            theme.primary_foreground = c(0x001B2E);

            theme.secondary = c(0x2D2D2D);
            theme.secondary_hover = c(0x383838);
            theme.secondary_active = c(0x424242);
            theme.secondary_foreground = c(0xF5F5F5);

            theme.popover = c(0x2B2B2B);
            theme.popover_foreground = c(0xF5F5F5);
            theme.group_box = c(0x2B2B2B);
            theme.group_box_foreground = c(0xF5F5F5);
            theme.input = c(0x333333);
            theme.selection = ca(0x60CDFF, 0.26);
            theme.caret = c(0x60CDFF);

            theme.title_bar = c(0x202020);
            theme.title_bar_border = c(0x333333);
            theme.scrollbar_thumb = c(0x555555);
            theme.scrollbar_thumb_hover = c(0x6B6B6B);

            theme.link = c(0x60CDFF);
            theme.danger = c(0xFF99A4);
            theme.danger_hover = c(0xFFB3BB);
            theme.danger_active = c(0xE87F8B);
            theme.danger_foreground = c(0x2B0A0E);
            theme.success = c(0x6CCB8A);
            theme.success_foreground = c(0x062B15);
            theme.warning = c(0xF4BF4F);
            theme.warning_foreground = c(0x2A1B00);
            theme.overlay = gpui::hsla(0., 0., 0., 0.52);
        } else {
            theme.background = c(0xF9F9F9);
            theme.foreground = c(0x1A1A1A);
            theme.border = c(0xE5E5E5);
            theme.ring = c(0x0067C0);

            theme.sidebar = c(0xF3F3F3);
            theme.sidebar_foreground = c(0x2B2B2B);
            theme.sidebar_border = c(0xE5E5E5);
            theme.sidebar_accent = c(0xE5F1FB);
            theme.sidebar_accent_foreground = c(0x1A1A1A);
            theme.sidebar_primary = c(0x0067C0);
            theme.sidebar_primary_foreground = c(0xFFFFFF);

            theme.list = c(0xFFFFFF);
            theme.list_hover = c(0xF5F5F5);
            theme.list_active = c(0xE5F1FB);
            theme.list_active_border = c(0x0067C0);
            theme.list_even = c(0xFFFFFF);
            theme.list_head = c(0xFFFFFF);

            theme.muted = c(0xF3F3F3);
            theme.muted_foreground = c(0x616161);
            theme.accent = c(0xE5F1FB);
            theme.accent_foreground = c(0x1A1A1A);

            theme.primary = c(0x0067C0);
            theme.primary_hover = c(0x005AAB);
            theme.primary_active = c(0x004A8F);
            theme.primary_foreground = c(0xFFFFFF);

            theme.secondary = c(0xF3F3F3);
            theme.secondary_hover = c(0xEDEDED);
            theme.secondary_active = c(0xE5E5E5);
            theme.secondary_foreground = c(0x2B2B2B);

            theme.popover = c(0xFFFFFF);
            theme.popover_foreground = c(0x1A1A1A);
            theme.group_box = c(0xFFFFFF);
            theme.group_box_foreground = c(0x1A1A1A);
            theme.input = c(0xFFFFFF);
            theme.selection = ca(0x0067C0, 0.18);
            theme.caret = c(0x0067C0);

            theme.title_bar = c(0xF3F3F3);
            theme.title_bar_border = c(0xE5E5E5);
            theme.scrollbar_thumb = c(0xC8C8C8);
            theme.scrollbar_thumb_hover = c(0xAFAFAF);

            theme.link = c(0x0067C0);
            theme.danger = c(0xC42B1C);
            theme.danger_hover = c(0xA5261A);
            theme.danger_active = c(0x8F2016);
            theme.danger_foreground = c(0xFFFFFF);
            theme.success = c(0x107C10);
            theme.success_foreground = c(0xFFFFFF);
            theme.warning = c(0x9D5D00);
            theme.warning_foreground = c(0xFFFFFF);
            theme.overlay = gpui::hsla(0., 0., 0., 0.28);
        }
    }

    // Windows 使用 Segoe UI，避免回退到不匹配的衬线字体。
    theme.font_family = if cfg!(target_os = "macos") {
        ".AppleSystemUIFont".into()
    } else {
        "Segoe UI".into()
    };
    theme.font_size = gpui::px(14.);
    theme.radius = if cfg!(target_os = "windows") {
        gpui::px(4.)
    } else {
        gpui::px(8.)
    };
    theme.radius_lg = if cfg!(target_os = "windows") {
        gpui::px(8.)
    } else {
        gpui::px(12.)
    };
    theme.shadow = true;
}

/// 跟随系统外观:同步模式 → 重套 Wake token。
pub fn sync_appearance(window: Option<&mut Window>, cx: &mut App) {
    // WAKE_THEME=dark|light 可强制模式(调试/截图用)
    match std::env::var("WAKE_THEME").as_deref() {
        Ok("dark") => Theme::change(ThemeMode::Dark, window, cx),
        Ok("light") => Theme::change(ThemeMode::Light, window, cx),
        _ => Theme::sync_system_appearance(window, cx),
    }
    apply_wake_theme(cx);
}

/// 手动切换(调试/后续设置页用)
#[allow(dead_code)]
pub fn set_mode(mode: ThemeMode, window: Option<&mut Window>, cx: &mut App) {
    Theme::change(mode, window, cx);
    apply_wake_theme(cx);
}
