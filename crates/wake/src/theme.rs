//! Wake 的双模式主题:Things / Bear 基准的 macOS 原生质感。
//! 浅色 = 纸感白 + 暖灰侧栏;深色 = 暖黑 + 柔和层次。accent 取系统蓝。
use gpui::{App, Hsla, Rgba, Window};
use gpui_component::{ActiveTheme as _, Theme, ThemeMode};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppearancePreference {
    System,
    Light,
    Dark,
}

impl AppearancePreference {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Light => "light",
            Self::Dark => "dark",
        }
    }

    fn parse(value: &str) -> Self {
        match value.trim() {
            "light" => Self::Light,
            "dark" => Self::Dark,
            _ => Self::System,
        }
    }
}

pub fn appearance_preference() -> AppearancePreference {
    crate::prefs::read("appearance")
        .map(|value| AppearancePreference::parse(&value))
        .unwrap_or(AppearancePreference::System)
}

fn apply_appearance(preference: AppearancePreference, window: Option<&mut Window>, cx: &mut App) {
    match preference {
        AppearancePreference::System => Theme::sync_system_appearance(window, cx),
        AppearancePreference::Light => Theme::change(ThemeMode::Light, window, cx),
        AppearancePreference::Dark => Theme::change(ThemeMode::Dark, window, cx),
    }
    apply_wake_theme(cx);
    cx.refresh_windows();
}

pub fn set_appearance(
    preference: AppearancePreference,
    window: Option<&mut Window>,
    cx: &mut App,
) -> std::io::Result<()> {
    crate::prefs::write("appearance", preference.as_str().as_bytes())?;
    apply_appearance(preference, window, cx);
    Ok(())
}

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

/// Claude 品牌橙:model badge、趋势图 Claude 层、模型色板第二档共用
const CLAUDE_ORANGE: u32 = 0xD97757;

/// 详情页 model badge 色——用户指定的 Claude 橙(非 agent 语义,纯配色选择);
/// outline badge 形态,两模式通用
pub const MODEL_BADGE_BG: u32 = CLAUDE_ORANGE;

/// 收藏星选中色——macOS systemYellow(Finder/Mail 星标惯例),两模式通用
pub const STAR_YELLOW: u32 = 0xFFCC00;

/// Insights 趋势图的 agent 序列色:有彩色品牌的家取品牌色(从内嵌 PNG 取主色,
/// 与侧栏图标同调),单色字形的家(Copilot/Cursor/OpenCode/Pi/Grok/Kimi)
/// 给固定备选色——同一家在任何机器上颜色都不变,相邻常见组合(Claude/Codex/
/// Cursor/Copilot)互不撞色。两模式共用(用户 2026-09-03:单色阶梯分不清)
pub fn agent_series_color(agent: wake_core::models::AgentId) -> u32 {
    use wake_core::models::AgentId::*;
    match agent {
        ClaudeCode => CLAUDE_ORANGE,
        Codex => 0x7A8DFF,
        Grok => 0x8A7F73,
        Dsh => 0x4D6BFE,
        Cursor => 0x2DB6C8,
        Opencode => 0xC98A2E,
        Pi => 0x3AAE8C,
        Omp => 0xB05CE6,
        Kiro => 0x9148FF,
        Kimi => 0xE4739E,
        Gemini => 0x3B8BD9,
        Copilot => 0x6E9C3F,
        Antigravity => 0x648AB5,
        Qoder => 0x2BB454,
        Hermes => 0xE0B040,
        Openclaw => 0xE04A4A,
    }
}

/// 趋势图模型序列的分类色板(按排名取色);模型没有品牌可依,六色足够区分
/// 前五 + Other
pub const SERIES_PALETTE: [u32; 6] = [
    0x4C8DFF,
    CLAUDE_ORANGE,
    0x2BB454,
    0x9148FF,
    0xE4739E,
    0xE0B040,
];

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

        theme.colors.list = c(0x20201F);
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

        theme.colors.list = c(0xF7F7F5);
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

    // 系统字体与 macOS 圆角习惯。`.SystemUIFont` 是 gpui 各平台后端都认的
    // 别名(macOS 解到 .AppleSystemUIFont,Windows 的 DirectWrite 后端解到
    // 系统 UI 字体);写死 `.AppleSystemUIFont` 会让非 mac 平台每个字重都
    // 走一遍"找不到→回落"的错误路径(2026-08-25 review)
    theme.font_family = ".SystemUIFont".into();
    theme.font_size = gpui::px(14.);
    theme.radius = gpui::px(8.);
    // 聚焦态只把边框染成 ring 色,不画框外的环:gpui-component 的环是元素框外
    // 2px 的绝对定位子元素,任何裁切祖先(弹窗面板、滚动列表、圆角容器)都会
    // 切掉一截——Add host / Add location 表单与设置页里实测残缺(2026-09-03
    // 用户反馈两轮)。上游自己的建议就是版式裁切多的应用关掉它,边框不占空间
    theme.focus_ring = false;
    theme.radius_lg = gpui::px(12.);
    theme.shadow = true;
}

/// 跟随系统外观:同步模式 → 重套 Wake token。
pub fn sync_appearance(window: Option<&mut Window>, cx: &mut App) {
    // WAKE_THEME=dark|light 可强制模式(调试/截图用)
    let preference = match std::env::var("WAKE_THEME").as_deref() {
        Ok("dark") => AppearancePreference::Dark,
        Ok("light") => AppearancePreference::Light,
        _ => appearance_preference(),
    };
    apply_appearance(preference, window, cx);
}
