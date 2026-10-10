use crate::i18n::t;
use gpui::prelude::FluentBuilder as _;
use gpui::*;
use gpui_component::button::{Button, ButtonCustomVariant, ButtonVariants as _};
use gpui_component::menu::{DropdownMenu as _, PopupMenu, PopupMenuItem};
use gpui_component::switch::Switch;
use gpui_component::{
    h_flex, v_flex, ActiveTheme as _, Disableable as _, Icon, Selectable as _, Sizable as _,
    StyledExt as _, TitleBar, WindowExt as _,
};

use wake_core::models::AgentId;

use crate::format::tilde_path;
use crate::ui::{
    action_button, overlay_layers, rails_factor, show_in_fm, zpx, Zpx, BUTTON_SM_H, FONT_BODY,
    FONT_CAPTION, FONT_DISPLAY, FONT_HEADING, FONT_LABEL, FONT_TITLE, ICON_TEXT_GAP, RADIUS_BUTTON,
    SPACE_LG, SPACE_MD, SPACE_SM, SPACE_XL, SPACE_XS, SPACE_XXL,
};
use crate::update::{self, UpdateStatus};
use crate::workbench::{
    DataSourceRow, LocationSettingsSnapshot, MemoryLocationRow, MemoryLocationSettingsSnapshot,
    OpenAbout, OpenSettings, OpenUpdates, Workbench,
};
use crate::{theme, theme::AppearancePreference};
use std::rc::Rc;

const SETTINGS_SIDEBAR_W: Zpx = Zpx(180.);
/// 侧栏顶上的标题栏条(gpui-component TitleBar 默认的 34px)。显式给成随缩放的同值:
/// traffic light 随缩放挪位,这条不跟着变高的话灯就落到它的下沿上
const SETTINGS_TITLEBAR_H: Zpx = Zpx(34.);
const SETTINGS_PAGE_TOP: Zpx = Zpx(38.);
/// Connect 页两条 Setup guide 的去处:MCP 面与命令行面各自的完整文档
const CONNECT_GUIDE_URL: &str = "https://github.com/iAmCorey/Wake/blob/main/docs/mcp.md";
const CONNECT_CLI_GUIDE_URL: &str = "https://github.com/iAmCorey/Wake/blob/main/docs/cli.md";
const CONNECT_CLAUDE_PLUGIN_GUIDE_URL: &str =
    "https://github.com/iAmCorey/Wake/blob/main/docs/cli.md#claude-code-plugin";
const CONNECT_CODEX_PLUGIN_GUIDE_URL: &str =
    "https://github.com/iAmCorey/Wake/blob/main/docs/cli.md#codex-plugin";

fn icon(path: &'static str) -> Icon {
    Icon::empty().path(path)
}

fn format_storage_size(bytes: u64) -> String {
    const KIB: f64 = 1024.;
    const MIB: f64 = KIB * 1024.;
    const GIB: f64 = MIB * 1024.;
    let bytes = bytes as f64;
    if bytes >= GIB {
        format!("{:.1} GB", bytes / GIB)
    } else if bytes >= MIB {
        format!("{:.1} MB", bytes / MIB)
    } else if bytes >= KIB {
        format!("{:.1} KB", bytes / KIB)
    } else {
        format!("{} bytes", bytes as u64)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum SettingsPage {
    General,
    Appearance,
    Locations,
    MemoryLocations,
    Remotes,
    Connect,
    Data,
    Updates,
    About,
}

/// Settings 内常规文字按钮共用一套尺寸和材质；避免页面各自混用
/// outline / primary / 默认 ButtonGroup 后形成多套视觉语言。
/// pub(crate):详情页的单段 Open In 按钮(workbench)用同一配方。
pub(crate) fn settings_button(button: Button, cx: &App) -> Button {
    let theme = cx.theme();
    button
        .custom(
            ButtonCustomVariant::new(cx)
                .color(theme.secondary)
                .foreground(theme.secondary_foreground)
                .hover(theme.secondary_hover)
                .active(theme.secondary_active),
        )
        .border_1()
        .border_color(theme.border)
        .small()
        .rounded(RADIUS_BUTTON)
}

/// 设置页里真正需要用户继续完成的主操作。保持 6px 圆角，但使用中号高度、
/// primary 填充和轻阴影，让它与普通的重试 / 再检查动作拉开层级。
pub(crate) fn settings_primary_button(button: Button, cx: &App) -> Button {
    let theme = cx.theme();
    button
        .custom(
            ButtonCustomVariant::new(cx)
                .color(theme.primary)
                .foreground(theme.primary_foreground)
                .hover(theme.primary_hover)
                .active(theme.primary_active)
                .shadow(true),
        )
        .border_1()
        .border_color(theme.primary)
        .rounded(RADIUS_BUTTON)
        .map(action_button)
}

/// Settings 各页顶部的标题 + 一句说明(版式只写一次:SPACE_XXL 边距、
/// SETTINGS_PAGE_TOP 顶距、FONT_TITLE semibold + FONT_CAPTION muted)
fn settings_page_header(title: &'static str, subtitle: &'static str, cx: &App) -> Div {
    let theme = cx.theme();
    v_flex()
        .flex_shrink_0()
        .px(SPACE_XXL)
        .pt(SETTINGS_PAGE_TOP)
        .pb(SPACE_XL)
        .gap(zpx(5.))
        .child(
            div()
                .text_size(FONT_TITLE)
                .font_semibold()
                .text_color(theme.foreground)
                .child(title),
        )
        .child(
            div()
                .text_size(FONT_CAPTION)
                .text_color(theme.muted_foreground)
                .child(subtitle),
        )
}

/// 设置行右侧的下拉(语言、缩放):描边小按钮 + 当前值 + chevron,菜单项由调用方填
fn select_control(
    id: &'static str,
    label: impl Into<SharedString>,
    menu_min_w: Zpx,
    items: impl Fn(PopupMenu) -> PopupMenu + 'static,
) -> AnyElement {
    Button::new(id)
        .outline()
        .small()
        .rounded(RADIUS_BUTTON)
        .label(label)
        .icon(icon("icons/chevron-down.svg").with_size(zpx(14.)))
        .dropdown_menu(move |menu, _, _| items(menu.min_w(menu_min_w)))
        .into_any_element()
}

/// "一张卡一行"的信息卡(Data、Connect、Updates 的卡与 General / Appearance 的设置行
/// 共用):popover 底圆角卡,行高由调用方给(设置行 72、其余 84),主信息 FONT_BODY,
/// 副行由调用方给(caption 级),右侧一个操作
fn settings_info_card(
    primary: impl Into<SharedString>,
    details: Vec<AnyElement>,
    trailing: AnyElement,
    min_h: Pixels,
    cx: &App,
) -> Div {
    let theme = cx.theme();
    // overflow_hidden 让 taffy 把这张卡的 min-height 当 0,放进滚动列里会被
    // 压扁到只剩一行;固定内容的卡必须 flex_shrink_0(CLAUDE.md 的 flex 陷阱)
    v_flex()
        .w_full()
        .flex_shrink_0()
        .overflow_hidden()
        .rounded(theme.radius_lg)
        .border_1()
        .border_color(theme.border)
        .bg(theme.popover)
        .child(
            h_flex()
                .min_h(min_h)
                .px(SPACE_LG)
                .py(SPACE_MD)
                .gap(SPACE_LG)
                .items_center()
                // 放不下时右侧控件换到下一行,而不是把标题挤成一字一行(界面放大后
                // 设置窗常常比内容窄:窗口大小不随档位变)。100% 档宽度够,不换行,
                // py 也小于 min_h 留的余量,版式不变
                .flex_wrap()
                .child(
                    v_flex()
                        .flex_1()
                        .min_w(zpx(180.))
                        .gap(zpx(3.))
                        .child(
                            div()
                                .text_size(FONT_BODY)
                                .text_color(theme.foreground)
                                .child(primary.into()),
                        )
                        .children(details),
                )
                .child(trailing),
        )
}

/// 一个并排辅助二进制的三件事。两张卡同构,所以探测也只写一遍
struct BinaryFacts {
    path: String,
    display: SharedString,
    exists: bool,
}

impl BinaryFacts {
    fn probe(stem: &str) -> Self {
        let found = wake_core::mcp::sibling_named(stem);
        let exists = found.as_ref().is_some_and(|p| p.is_file());
        // 找不到就退回裸名,展示时至少还是个可辨认的东西
        let path = found
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|| stem.to_string());
        Self {
            display: tilde_path(&path).into(),
            exists,
            path,
        }
    }
}

/// Connect 页展示的事实,开窗时算一次:current_exe/stat/片段拼装都不该跑在
/// 每帧的 render 里(CLAUDE.md:render 里的路径探测必须缓存)
struct ConnectInfo {
    mcp: BinaryFacts,
    cli: BinaryFacts,
    /// 把 wake-cli 放进 PATH 的那条命令;deb/tar 已在 `…/bin` 里、或 Windows
    /// 上没有一行命令能说清时为 None,那时就不给这个按钮
    cli_path_command: Option<String>,
    /// MCP clients 卡的三行:文案与片段来自 wake-core,与 `wake-mcp setup`
    /// 同源;GUI 只展示与复制,不代写别家配置
    snippets: Vec<wake_core::mcp::SetupSnippet>,
    /// Claude Code plugin 与 Codex plugin 两块各一行,与 `wake-cli setup` 同源;这个平台不给
    /// (`cli::AGENT_PLUGINS` 为假,即 Windows)时是 None,两块都不画
    claude_plugin: Option<wake_core::mcp::SetupSnippet>,
    codex_plugin: Option<wake_core::mcp::SetupSnippet>,
}

impl ConnectInfo {
    fn probe() -> Self {
        let mcp = BinaryFacts::probe("wake-mcp");
        let cli = BinaryFacts::probe("wake-cli");
        Self {
            snippets: wake_core::mcp::setup_snippets(std::path::Path::new(&mcp.path)),
            claude_plugin: wake_core::cli::AGENT_PLUGINS
                .then(wake_core::cli::claude_plugin_snippet),
            codex_plugin: wake_core::cli::AGENT_PLUGINS.then(wake_core::cli::codex_plugin_snippet),
            cli_path_command: wake_core::cli::path_command(std::path::Path::new(&cli.path)),
            mcp,
            cli,
        }
    }
}

pub(crate) struct SettingsView {
    focus_handle: FocusHandle,
    workbench: Entity<Workbench>,
    appearance: AppearancePreference,
    show_unavailable: bool,
    connect: ConnectInfo,
    /// Connect 页里展开了代码片段的行(按 snippets 下标);Settings 重开即复位
    connect_shown: std::collections::HashSet<usize>,
    /// 刚复制过的按钮 id:图标原地变对勾(tooltip 说 Copied)1.6s。不用 toast——gpui-component 的
    /// 通知在窗口失活或被悬停时会暂停自动关闭,Settings 这种从属窗里它常常就
    /// 挂着不走(用户 2026-09-08 反馈);主界面的 Copy Session ID / Copy code 也
    /// 从不弹通知
    copied: Option<SharedString>,
    /// 连点时只有最后一次的定时器能清掉 copied
    copied_generation: u64,
    /// Session locations / Memory locations 两页的快照:各是三条 SQL + 每路径一次 stat,
    /// 在 observe Workbench 的回调里取一次(配置变更、扫描进度、记忆同步收尾都会
    /// notify),不在 render 里每帧算——原先 hover 一下就重查一遍库(2026-09-22 /simplify)
    locations: LocationSettingsSnapshot,
    memory_locations: MemoryLocationSettingsSnapshot,
    /// 上次挪 traffic light 时的缩放档位(`ui_zoom::changed`)
    zoom: u16,
    _workbench_observer: Option<Subscription>,
}

/// location 行菜单里的一个动作(Edit… / Remove):Settings 只转发给 Workbench
type RowAction = Rc<dyn Fn(&mut Window, &mut App)>;

/// 行末的 `…` 菜单(两页共用):Edit…(可编辑的行)、Show in Finder(路径存在时)、分隔线 +
/// Remove(可删的自定义行);每项都是 Option,两页按自己的规则给
fn row_menu(
    ix: usize,
    edit: Option<RowAction>,
    reveal: Option<SharedString>,
    remove: Option<RowAction>,
) -> impl IntoElement {
    Button::new(("settings-location-menu", ix))
        .ghost()
        .small()
        .rounded(RADIUS_BUTTON)
        .icon(icon("icons/more-horizontal.svg").with_size(zpx(14.)))
        .dropdown_menu(move |menu, _, _| {
            let mut menu = menu.min_w(zpx(180.));
            if let Some(edit) = edit.clone() {
                menu = menu.item(
                    PopupMenuItem::new(t("Edit…")).on_click(move |_, window, cx| edit(window, cx)),
                );
            }
            if let Some(path) = reveal.clone() {
                menu = menu.item(PopupMenuItem::new(show_in_fm()).on_click(move |_, _, _| {
                    wake_core::services::terminal::open_in_file_manager(path.as_ref())
                }));
            }
            if let Some(remove) = remove.clone() {
                menu = menu.separator().item(
                    PopupMenuItem::new(t("Remove"))
                        .on_click(move |_, window, cx| remove(window, cx)),
                );
            }
            menu
        })
}

/// 快照的行按 agent 成组(快照已按 AgentId 声明序排好,相邻同家即一组)
fn group_by_agent<R>(rows: Vec<R>, agent: impl Fn(&R) -> AgentId) -> Vec<(AgentId, Vec<R>)> {
    let mut groups: Vec<(AgentId, Vec<R>)> = Vec::new();
    for row in rows {
        match groups.last_mut() {
            Some((a, rows)) if *a == agent(&row) => rows.push(row),
            _ => groups.push((agent(&row), vec![row])),
        }
    }
    groups
}

impl SettingsView {
    pub(crate) fn new(
        workbench: Entity<Workbench>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let observed = workbench.clone();
        // Settings 是在 Workbench::open_settings 的 update 栈内创建的；此处
        // 立即 observe 会尝试反读仍被独占借用的 Workbench，触发 double lease。
        // 下一帧注册时外层 update 已退出。
        cx.on_next_frame(window, move |this, _, cx| {
            this.refresh_snapshots(observed.read(cx));
            this._workbench_observer = Some(cx.observe(&observed, |this, workbench, cx| {
                this.refresh_snapshots(workbench.read(cx));
                cx.notify()
            }));
            cx.notify();
        });
        Self {
            focus_handle: cx.focus_handle(),
            workbench,
            appearance: theme::appearance_preference(),
            show_unavailable: false,
            connect: ConnectInfo::probe(),
            connect_shown: Default::default(),
            copied: None,
            copied_generation: 0,
            zoom: crate::ui_zoom::percent(),
            locations: LocationSettingsSnapshot {
                rows: Vec::new(),
                diverged: false,
            },
            memory_locations: MemoryLocationSettingsSnapshot {
                rows: Vec::new(),
                diverged: false,
            },
            _workbench_observer: None,
        }
    }

    fn render_nav_item(
        &self,
        id: &'static str,
        label: &'static str,
        icon_path: &'static str,
        page: SettingsPage,
        cx: &Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let active = self.workbench.read(cx).settings_page() == page;
        let workbench = self.workbench.clone();
        h_flex()
            .id(id)
            .h(zpx(34.))
            .w_full()
            .px(SPACE_MD)
            .gap(SPACE_SM)
            .items_center()
            .rounded(theme.radius)
            .cursor_pointer()
            .when(active, |this| {
                this.bg(theme.sidebar_accent)
                    .text_color(theme.sidebar_accent_foreground)
            })
            .when(!active, |this| {
                this.text_color(theme.sidebar_foreground)
                    .hover(|style| style.bg(theme.sidebar_accent.opacity(0.55)))
            })
            .on_click(move |_, _, cx| {
                workbench.update(cx, |this, cx| this.select_settings_page(page, cx));
            })
            .child(icon(icon_path).with_size(zpx(15.)).flex_shrink_0())
            // 放大档位下侧栏可能比标签窄:单行截断,行高是定的,换成两行会压到下一项
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .text_size(FONT_BODY)
                    .font_medium()
                    .child(label),
            )
            .into_any_element()
    }

    fn render_sidebar(&self, window: &Window, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let show_titlebar = cfg!(target_os = "macos")
            || matches!(window.window_decorations(), Decorations::Client { .. });

        // 侧栏随界面缩放放大,窗口不够宽时给内容区让位(与主窗的两条栏同一条规则)
        let scale = rails_factor(
            SETTINGS_SIDEBAR_W.0,
            window.viewport_size().width,
            crate::main_window::SETTINGS_MIN_SIZE.width,
        );
        v_flex()
            .w(px(SETTINGS_SIDEBAR_W.0 * scale))
            .h_full()
            .flex_shrink_0()
            .bg(theme.sidebar)
            .border_r_1()
            .border_color(theme.sidebar_border)
            .when(show_titlebar, |this| {
                this.child(TitleBar::new().h(SETTINGS_TITLEBAR_H))
            })
            .child(
                div()
                    .px(SPACE_LG)
                    .pt(SPACE_SM)
                    .pb(SPACE_XL)
                    .text_size(FONT_HEADING)
                    .font_semibold()
                    .text_color(theme.sidebar_foreground)
                    .child(t("Settings")),
            )
            // 放大档位下两组导航可能比窗口高:包进滚动容器。上面那组 flex_1 把下面那组
            // 顶到底;放不下时它不会被压到内容以下,于是溢出、可滚
            .child(
                v_flex()
                    .id("settings-nav-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(
                        v_flex()
                            .flex_1()
                            .px(SPACE_SM)
                            .gap(zpx(2.))
                            .child(self.render_nav_item(
                                "settings-general-nav",
                                t("General"),
                                "icons/settings.svg",
                                SettingsPage::General,
                                cx,
                            ))
                            .child(self.render_nav_item(
                                "settings-appearance-nav",
                                t("Appearance"),
                                "icons/palette.svg",
                                SettingsPage::Appearance,
                                cx,
                            ))
                            .child(self.render_nav_item(
                                "settings-locations-nav",
                                t("Session locations"),
                                "icons/hard-drive.svg",
                                SettingsPage::Locations,
                                cx,
                            ))
                            .child(self.render_nav_item(
                                "settings-memory-locations-nav",
                                t("Memory locations"),
                                "icons/brain.svg",
                                SettingsPage::MemoryLocations,
                                cx,
                            ))
                            .child(self.render_nav_item(
                                "settings-remotes-nav",
                                t("Remote hosts"),
                                "icons/server.svg",
                                SettingsPage::Remotes,
                                cx,
                            ))
                            .child(self.render_nav_item(
                                "settings-connect-nav",
                                t("Connect"),
                                "icons/plug.svg",
                                SettingsPage::Connect,
                                cx,
                            ))
                            .child(self.render_nav_item(
                                "settings-data-nav",
                                t("Data"),
                                "icons/database.svg",
                                SettingsPage::Data,
                                cx,
                            )),
                    )
                    .child(
                        v_flex()
                            .px(SPACE_SM)
                            .pb(SPACE_SM)
                            .gap(zpx(2.))
                            .child(self.render_nav_item(
                                "settings-updates-nav",
                                t("Updates"),
                                "icons/download.svg",
                                SettingsPage::Updates,
                                cx,
                            ))
                            .child(self.render_nav_item(
                                "settings-about-nav",
                                t("About"),
                                "icons/info.svg",
                                SettingsPage::About,
                                cx,
                            )),
                    ),
            )
            .into_any_element()
    }

    /// General / Appearance 页的设置行:与 Data / Connect / Updates 同一张信息卡,只是矮
    /// 一档(72 是 Theme 行的用户定稿值),副行是 caption 级说明
    fn setting_row(
        title: &'static str,
        subtitle: impl IntoElement,
        control: impl IntoElement,
        cx: &Context<Self>,
    ) -> AnyElement {
        let caption = div()
            .text_size(FONT_CAPTION)
            .text_color(cx.theme().muted_foreground)
            .child(subtitle)
            .into_any_element();
        settings_info_card(
            title,
            vec![caption],
            control.into_any_element(),
            zpx(72.),
            cx,
        )
        .into_any_element()
    }

    /// 语言选择:System + English + 装好的语言包。选项数量随语言包增减,
    /// 所以是下拉而不是 Theme 那样的分段控件
    fn language_control(&self) -> AnyElement {
        // 直接读全局:镜像成字段就要在每个切换点写回,而 `apply_language`
        // 改的是全局(appearance 那个字段正是这么漂的)
        let current = crate::i18n::preference().map(|locale| locale.tag);
        let label = current.map_or(t("System"), |tag| {
            crate::i18n::available()
                .iter()
                .find(|locale| locale.tag == tag)
                .map_or(t("System"), |locale| locale.name)
        });
        select_control("settings-language", label, Zpx(160.), move |mut menu| {
            // 选项只在菜单打开时才需要,`available()` 又是 'static 切片
            // ——留在闭包里就不必每帧建一个 Vec 再 clone 一份进来
            let options = std::iter::once((None, t("System"))).chain(
                crate::i18n::available()
                    .iter()
                    .map(|locale| (Some(locale.tag), locale.name)),
            );
            for (tag, name) in options {
                menu = menu.item(PopupMenuItem::new(name).checked(tag == current).on_click(
                    move |_, window, cx| {
                        if let Err(error) = crate::i18n::set_language(tag, cx) {
                            window.push_notification(
                                gpui_component::notification::Notification::error(crate::tf!(
                                    "Couldn't save language: {}",
                                    error
                                )),
                                cx,
                            );
                        }
                    },
                ));
            }
            menu
        })
    }

    /// 界面缩放档位:与 ⌘+ / ⌘− / ⌘0、显示菜单是同一个设置。档位多,与语言一样用下拉
    fn zoom_control(&self) -> AnyElement {
        let current = crate::ui_zoom::percent();
        select_control(
            "settings-zoom",
            crate::ui_zoom::label(current),
            Zpx(120.),
            move |mut menu| {
                for level in crate::ui_zoom::LEVELS {
                    menu = menu.item(
                        PopupMenuItem::new(crate::ui_zoom::label(level))
                            .checked(level == current)
                            .on_click(move |_, _, cx| crate::ui_zoom::change(level, cx)),
                    );
                }
                menu
            },
        )
    }

    fn appearance_button(
        &self,
        id: &'static str,
        label: &'static str,
        preference: AppearancePreference,
        cx: &Context<Self>,
    ) -> Button {
        let theme = cx.theme();
        let selected = self.appearance == preference;
        Button::new(id)
            .custom(
                ButtonCustomVariant::new(cx)
                    .color(theme.transparent)
                    .foreground(if selected {
                        theme.foreground
                    } else {
                        theme.muted_foreground
                    })
                    .hover(theme.secondary_hover)
                    .active(theme.popover),
            )
            .small()
            .w(zpx(64.))
            .rounded(RADIUS_BUTTON)
            .label(label)
            .selected(selected)
            .when(selected, |this| this.shadow_xs())
            .on_click(cx.listener(move |this, _, window, cx| {
                match theme::set_appearance(preference, Some(window), cx) {
                    Ok(()) => {
                        this.appearance = preference;
                        cx.notify();
                    }
                    Err(error) => window.push_notification(
                        gpui_component::notification::Notification::error(crate::tf!(
                            "Couldn't save appearance: {}",
                            error
                        )),
                        cx,
                    ),
                }
            }))
    }

    /// 设置各页(Session / Memory locations 与 Remote hosts 之外)的骨架:页头 + 可滚动的
    /// 内容列(界面放大后卡片会换行变高,一屏放不下)
    fn rows_page(
        id: &'static str,
        title: &'static str,
        subtitle: &'static str,
        gap: Zpx,
        rows: impl IntoIterator<Item = AnyElement>,
        cx: &App,
    ) -> AnyElement {
        v_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .bg(cx.theme().background)
            .child(settings_page_header(title, subtitle, cx))
            .child(
                v_flex()
                    .id(id)
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px(SPACE_XXL)
                    .pb(SPACE_XXL)
                    .gap(gap)
                    .children(rows),
            )
            .into_any_element()
    }

    fn render_general(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let rows = [Self::setting_row(
            t("Language"),
            t("Follow the system language, or pick one."),
            self.language_control(),
            cx,
        )];
        Self::rows_page(
            "settings-general-scroll",
            t("General"),
            t("Choose the language Wake uses."),
            SPACE_MD,
            rows,
            cx,
        )
    }

    /// 外观与界面缩放(2026-10-08 用户定:自成一页,不放 General)
    fn render_appearance(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let modes = h_flex()
            .h(BUTTON_SM_H + zpx(4.))
            .p(zpx(2.))
            .rounded(theme.radius)
            .border_1()
            .border_color(theme.border)
            .bg(theme.secondary)
            .child(self.appearance_button(
                "appearance-system",
                t("System"),
                AppearancePreference::System,
                cx,
            ))
            .child(self.appearance_button(
                "appearance-light",
                t("Light"),
                AppearancePreference::Light,
                cx,
            ))
            .child(self.appearance_button(
                "appearance-dark",
                t("Dark"),
                AppearancePreference::Dark,
                cx,
            ));
        // 快捷键自成一段、不在中间断行:gpui 在每个符号前都允许折行,窄的时候会把
        // "⌘" 和 "+" 拆到两行。放不下就整段换到下一行
        let zoom_caption = h_flex()
            .w_full()
            .flex_wrap()
            .gap_x(zpx(4.))
            .child(t("Make text and controls larger or smaller."))
            .child(div().whitespace_nowrap().child(crate::ui_zoom::key_hints()));
        let rows = [
            Self::setting_row(
                t("Theme"),
                t("Follow the system or keep Wake light or dark."),
                modes,
                cx,
            ),
            Self::setting_row(t("Zoom"), zoom_caption, self.zoom_control(), cx),
        ];
        Self::rows_page(
            "settings-appearance-scroll",
            t("Appearance"),
            t("Customize how Wake looks and reads."),
            SPACE_MD,
            rows,
            cx,
        )
    }

    fn render_data(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let snapshot = self.workbench.read(cx).data_settings_snapshot();
        // 会话计数的措辞单点在 workbench 的 session_tally(Locations 与
        // Remote hosts 也用它);这里只补分隔符,不再复制一遍句子。占用已含
        // remotes/ 镜像(Workbench 后台算好放进快照),镜像非零时另注一句
        let mut summary = format!(
            "{} · {}",
            crate::workbench::session_tally(snapshot.session_count),
            format_storage_size(snapshot.size_bytes)
        );
        if snapshot.remote_bytes > 0 {
            summary.push(' ');
            summary.push_str(&crate::tf!(
                "({} in remote mirrors)",
                format_storage_size(snapshot.remote_bytes)
            ));
        }
        let summary: SharedString = summary.into();
        let reveal_path = snapshot.raw_path.clone();
        let show_in_finder = settings_button(
            Button::new("settings-show-data")
                .icon(icon("icons/folder.svg").with_size(zpx(13.)))
                .label(show_in_fm()),
            cx,
        )
        .on_click(move |_, _, _| {
            wake_core::services::terminal::open_in_file_manager(reveal_path.as_ref())
        });
        let details = vec![
            div()
                .w_full()
                .truncate()
                .text_size(FONT_CAPTION)
                .text_color(theme.muted_foreground)
                .child(snapshot.display_path)
                .into_any_element(),
            div()
                .text_size(FONT_CAPTION)
                .text_color(theme.muted_foreground)
                .child(summary)
                .into_any_element(),
        ];

        let rows = [
            div()
                .text_size(FONT_CAPTION)
                .font_semibold()
                .text_color(theme.foreground)
                .child(t("Storage"))
                .into_any_element(),
            settings_info_card(
                t("Wake data"),
                details,
                show_in_finder.into_any_element(),
                zpx(84.),
                cx,
            )
            .into_any_element(),
        ];
        Self::rows_page(
            "settings-data-scroll",
            t("Data"),
            t("See where Wake stores local data. Sessions refresh automatically."),
            SPACE_SM,
            rows,
            cx,
        )
    }

    /// Settings → Connect 的复制钮:只有图标,说明在 tooltip——卡上已经摆着要复制的东西或
    /// 一句用法,按钮再写一遍 "Copy path" / "Copy command" 是重复(用户 2026-10-09 定,整页
    /// 统一)。写剪贴板后图标原地变对勾、tooltip 说 Copied,1.6s 复原
    fn copy_button(
        &self,
        id: SharedString,
        tooltip: &'static str,
        text: String,
        cx: &Context<Self>,
    ) -> Button {
        self.copy_button_with("icons/copy.svg", id, tooltip, text, cx)
    }

    /// 同一张卡上的第二个复制钮换个字形:wake-cli 卡的路径与"放进 PATH"的命令并排,
    /// 两个一样的图标分不清
    fn copy_button_with(
        &self,
        glyph: &'static str,
        id: SharedString,
        tooltip: &'static str,
        text: String,
        cx: &Context<Self>,
    ) -> Button {
        let copied = self.copied.as_ref() == Some(&id);
        let clicked_id = id.clone();
        let button = Button::new(id)
            .icon(icon(if copied { "icons/check.svg" } else { glyph }).with_size(zpx(13.)))
            .tooltip(if copied { t("Copied") } else { tooltip });
        settings_button(button, cx).on_click(cx.listener(move |this, _, _, cx| {
            cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
            this.show_copied(clicked_id.clone(), cx);
        }))
    }

    fn show_copied(&mut self, id: SharedString, cx: &mut Context<Self>) {
        self.copied_generation = self.copied_generation.wrapping_add(1);
        let generation = self.copied_generation;
        self.copied = Some(id);
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(1_600))
                .await;
            this.update(cx, |this, cx| {
                if this.copied_generation == generation {
                    this.copied = None;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// Settings → Connect:把 Wake 的索引只读接给 coding agent。页面只放**状态与动作**:
    /// 每块一张卡(二进制路径、一句用法或每家一行)+ 只有图标的复制钮,区块标题右端挂
    /// Setup guide。代码块与工具表都不放——那是 README 的内容,塞进设置窗怎么排都像教程
    /// (用户 2026-09-08 三轮定稿)。只展示与复制,不代写别家配置;片段与 `wake-mcp setup` /
    /// `wake-cli setup` 同源(wake-core 的 mcp / cli)
    fn render_connect(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let dark = theme.mode.is_dark();
        let info = &self.connect;
        let mono = theme.mono_font_family.clone();
        let count = info.snippets.len();

        let agent_rows: Vec<AnyElement> = info
            .snippets
            .iter()
            .enumerate()
            .map(|(ix, s)| {
                let shown = self.connect_shown.contains(&ix);
                let copy = self.copy_button(
                    format!("connect-copy-{ix}").into(),
                    t(s.copy_label),
                    s.text.clone(),
                    cx,
                );
                // 低强调的展开切换:片段默认收起,想核对再看
                let toggle = Button::new(SharedString::from(format!("connect-show-{ix}")))
                    .custom(
                        ButtonCustomVariant::new(cx)
                            .color(theme.transparent)
                            .foreground(theme.muted_foreground)
                            .hover(theme.secondary_hover)
                            .active(theme.popover),
                    )
                    .small()
                    .rounded(RADIUS_BUTTON)
                    .icon(
                        icon(if shown {
                            "icons/chevron-down.svg"
                        } else {
                            "icons/chevron-right.svg"
                        })
                        .with_size(zpx(13.)),
                    )
                    .label(if shown { t("Hide") } else { t("Show") })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if !this.connect_shown.remove(&ix) {
                            this.connect_shown.insert(ix);
                        }
                        cx.notify();
                    }));
                v_flex()
                    .w_full()
                    .when(ix + 1 < count, |this| {
                        this.border_b_1().border_color(theme.border)
                    })
                    .child(
                        h_flex()
                            .w_full()
                            .min_h(zpx(52.))
                            .px(SPACE_LG)
                            .py(SPACE_SM)
                            .gap(ICON_TEXT_GAP)
                            .items_center()
                            .child(img(s.agent.brand_icon(dark)).size(zpx(17.)).flex_shrink_0())
                            .child(
                                v_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .gap(zpx(2.))
                                    .child(
                                        div()
                                            .text_size(FONT_BODY)
                                            .text_color(theme.foreground)
                                            .child(s.client),
                                    )
                                    .child(
                                        div()
                                            .text_size(FONT_CAPTION)
                                            .text_color(theme.muted_foreground)
                                            .child(t(s.hint)),
                                    ),
                            )
                            .child(toggle)
                            .child(copy),
                    )
                    .when(shown, |this| {
                        // 默认收起,所以行元素只在展开时才建
                        let lines = s
                            .text
                            .lines()
                            .map(|l| div().child(l.to_string()).into_any_element());
                        this.child(
                            // 左缘对齐到文字轴:行内边距 + 图标 17 + 间距 12
                            v_flex()
                                .ml(SPACE_LG + zpx(17.) + SPACE_MD)
                                .mr(SPACE_LG)
                                .mb(SPACE_MD)
                                .px(SPACE_MD)
                                .py(SPACE_SM)
                                .rounded(theme.radius)
                                .bg(theme.secondary)
                                .font_family(mono.clone())
                                .text_size(FONT_CAPTION)
                                .text_color(theme.foreground)
                                .children(lines),
                        )
                    })
                    .into_any_element()
            })
            .collect();

        // 信息卡里那条 mono 副信息;exists=false 再补一句红字
        let mono_details = |display: SharedString, exists: bool| {
            let mut out = vec![div()
                .w_full()
                .truncate()
                .text_size(FONT_CAPTION)
                .font_family(mono.clone())
                .text_color(theme.muted_foreground)
                .child(display)
                .into_any_element()];
            if !exists {
                out.push(
                    div()
                        .text_size(FONT_CAPTION)
                        .text_color(theme.danger)
                        .child(t("Not found next to the Wake app — reinstall Wake"))
                        .into_any_element(),
                );
            }
            out
        };
        // wake-mcp 与 wake-cli 的卡完全同构,只差名字、路径与按钮 id
        let binary_card = |primary: &'static str,
                           id: &'static str,
                           f: &BinaryFacts,
                           extra: Option<AnyElement>| {
            settings_info_card(
                primary,
                mono_details(f.display.clone(), f.exists),
                // 复制路径的钮恒在最右,两张卡的同一个动作才对得齐;附加动作放它左边
                h_flex()
                    .flex_shrink_0()
                    .gap(SPACE_SM)
                    .items_center()
                    .children(extra)
                    .child(self.copy_button(id.into(), t("Copy path"), f.path.clone(), cx))
                    .into_any_element(),
                zpx(84.),
                cx,
            )
            .into_any_element()
        };
        let copy_cli_command = info.cli_path_command.clone().map(|cmd| {
            self.copy_button_with(
                "icons/terminal.svg",
                "connect-copy-cli-command".into(),
                t("Copy the command that adds wake-cli to PATH"),
                cmd,
                cx,
            )
            .into_any_element()
        });
        let skill_card = settings_info_card(
            t("Wake skill"),
            mono_details(wake_core::cli::SKILL_INSTALL.into(), true),
            self.copy_button(
                "connect-copy-skill".into(),
                t("Copy command"),
                wake_core::cli::SKILL_INSTALL.to_string(),
                cx,
            )
            .into_any_element(),
            zpx(84.),
            cx,
        );
        // 一行卡:区块标题已经点了名,卡里只放一句怎么用 + 复制钮(用户 2026-10-09:名字行
        // 多余、两行没必要、一张等宽一张正文挨着太杂;命令在复制钮里,不必摆出来)
        let one_line_card = |s: &wake_core::mcp::SetupSnippet, id: &'static str| {
            settings_info_card(
                t(s.hint),
                Vec::new(),
                self.copy_button(id.into(), t(s.copy_label), s.text.clone(), cx)
                    .into_any_element(),
                zpx(48.),
                cx,
            )
            .into_any_element()
        };

        let section = |label: &'static str| {
            div()
                .flex_shrink_0()
                .text_size(FONT_CAPTION)
                .font_semibold()
                .text_color(theme.foreground)
                .child(label)
        };
        // 区块标题右侧的文档链接:MCP 面与命令行面各链自己那份,页尾只放一条
        // 会让读 CLI 那半页的人点进 MCP 的参考里。设置窗里不放文档,所以这页
        // 只有状态、动作与这几个去处,没有解释性的散文(用户 2026-09-11 定)
        let titled = |label: &'static str, guide: Option<(&'static str, &'static str)>| {
            h_flex()
                .flex_shrink_0()
                .gap(SPACE_SM)
                .items_center()
                .child(section(label).flex_1())
                .children(guide.map(|(id, url)| {
                    div()
                        .id(id)
                        .cursor_pointer()
                        .flex_shrink_0()
                        .text_size(FONT_CAPTION)
                        .text_color(theme.primary)
                        .on_click(move |_, _, cx| cx.open_url(url))
                        .child(t("Setup guide"))
                }))
        };

        // 区块同一种写法:(标题, 文档链接, 卡),有没有链接、有没有这块都写在数据里,
        // 间距一律"不是第一块就加",不靠各块自己记。链接的规则一句话:**每个面的第一个
        // 区块挂自己的文档**——Claude Code plugin、Codex plugin 各是一面,MCP 面 = MCP server
        // + MCP clients,命令行面 = Command line + Skill。两家的插件放最前:一条命令就把 MCP
        // 与开场交接一起装好,是最省事的那条路。标题用各家的正名(用户 2026-10-09 定:不自己
        // 起名字;Codex 那块原先是它官方的 hooks 配置,2026-10-10 有了真正的 Codex 插件才改名);
        // 这两块 Windows 上没有(片段给 None)
        let sections = [
            (
                t("Claude Code plugin"),
                Some((
                    "connect-claude-plugin-setup-guide",
                    CONNECT_CLAUDE_PLUGIN_GUIDE_URL,
                )),
                info.claude_plugin
                    .as_ref()
                    .map(|s| one_line_card(s, "connect-copy-claude-plugin")),
            ),
            (
                t("Codex plugin"),
                Some((
                    "connect-codex-plugin-setup-guide",
                    CONNECT_CODEX_PLUGIN_GUIDE_URL,
                )),
                info.codex_plugin
                    .as_ref()
                    .map(|s| one_line_card(s, "connect-copy-codex-plugin")),
            ),
            (
                t("MCP server"),
                Some(("connect-setup-guide", CONNECT_GUIDE_URL)),
                Some(binary_card(
                    "wake-mcp",
                    "connect-copy-path",
                    &info.mcp,
                    None,
                )),
            ),
            (
                t("MCP clients"),
                None,
                Some(
                    v_flex()
                        .w_full()
                        .flex_shrink_0()
                        .overflow_hidden()
                        .rounded(theme.radius_lg)
                        .border_1()
                        .border_color(theme.border)
                        .bg(theme.popover)
                        .children(agent_rows)
                        .into_any_element(),
                ),
            ),
            (
                t("Command line"),
                Some(("connect-cli-setup-guide", CONNECT_CLI_GUIDE_URL)),
                Some(binary_card(
                    "wake-cli",
                    "connect-copy-cli-path",
                    &info.cli,
                    copy_cli_command,
                )),
            ),
            (t("Skill"), None, Some(skill_card.into_any_element())),
        ];
        let rows = sections
            .into_iter()
            .filter_map(|(title, guide, card)| card.map(|card| (title, guide, card)))
            .enumerate()
            .flat_map(|(ix, (title, guide, card))| {
                [
                    titled(title, guide)
                        .when(ix > 0, |this| this.pt(SPACE_LG))
                        .into_any_element(),
                    card,
                ]
            });
        Self::rows_page(
            "settings-connect-scroll",
            t("Connect"),
            t("Let your coding agents look up your past sessions from Wake."),
            SPACE_SM,
            rows,
            cx,
        )
    }

    fn about_link(
        &self,
        id: &'static str,
        label: &'static str,
        url: &'static str,
        cx: &Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        div()
            .id(id)
            .cursor_pointer()
            .text_size(FONT_LABEL)
            .font_family(theme.mono_font_family.clone())
            .text_color(theme.foreground)
            .hover(|style| style.text_decoration_1())
            .on_click(move |_, _, cx| cx.open_url(url))
            .child(label)
            .into_any_element()
    }

    /// 与 Kooky / Birth 的 About 面板使用同一信息层级，但落在 Wake 已有的
    /// Settings 场景内：产品图标、名称、版本、tagline、仓库和作者署名。
    fn render_about(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let faint = theme.muted_foreground.opacity(0.72);
        let version: SharedString = crate::tf!("Version {}", env!("CARGO_PKG_VERSION")).into();

        v_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .items_center()
            .bg(theme.background)
            .child(
                v_flex()
                    .w(zpx(360.))
                    .items_center()
                    .pt(zpx(52.))
                    .child(
                        img("brands/wake.svg")
                            .size(zpx(78.))
                            .flex_shrink_0()
                            .mb(SPACE_MD),
                    )
                    .child(
                        div()
                            .text_size(FONT_DISPLAY)
                            .font_medium()
                            .text_color(theme.foreground)
                            .child("Wake"),
                    )
                    .child(
                        div()
                            .mt(SPACE_XS)
                            .text_size(FONT_LABEL)
                            .font_family(theme.mono_font_family.clone())
                            .text_color(theme.muted_foreground)
                            .child(version),
                    )
                    .child(
                        div()
                            .mt(SPACE_MD)
                            .text_size(FONT_CAPTION)
                            .text_color(theme.muted_foreground)
                            .child(t("All your AI agent sessions, in one place.")),
                    )
                    .child(div().mt(zpx(14.)).child(self.about_link(
                        "about-github",
                        "GitHub ↗",
                        "https://github.com/iAmCorey/Wake",
                        cx,
                    )))
                    .child(div().w(zpx(32.)).h(px(1.)).my(SPACE_LG).bg(theme.border))
                    .child(
                        div()
                            .text_size(FONT_LABEL)
                            .font_family(theme.mono_font_family.clone())
                            .text_color(faint)
                            .child(t("© 2026 Corey Chiu · MIT License")),
                    )
                    .child(
                        h_flex()
                            .mt(SPACE_XS)
                            .text_size(FONT_LABEL)
                            .font_family(theme.mono_font_family.clone())
                            .text_color(faint)
                            .child(t("Built with ❤️ by "))
                            .child(self.about_link(
                                "about-author",
                                "Corey Chiu",
                                "https://coreychiu.com?utm_source=wake",
                                cx,
                            )),
                    ),
            )
            .into_any_element()
    }

    fn render_updates(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let status = self.workbench.read(cx).update_status().clone();
        let checking = matches!(status, UpdateStatus::Checking);
        let update_available = matches!(status, UpdateStatus::Available { .. });
        let status_message: SharedString = match &status {
            UpdateStatus::Idle => t("Check GitHub Releases for a newer version.").into(),
            UpdateStatus::Checking => t("Checking GitHub Releases…").into(),
            UpdateStatus::UpToDate { latest } => {
                crate::tf!("No newer release is available (latest: {}).", latest).into()
            }
            UpdateStatus::Available { latest } => crate::tf!(
                "Wake {} is available. Open the release page to download it.",
                latest
            )
            .into(),
            UpdateStatus::Failed => {
                t("Couldn't check for updates. Check your connection and try again.").into()
            }
        };
        let button_label = match status {
            UpdateStatus::Idle => t("Check for Updates"),
            UpdateStatus::Checking => t("Checking…"),
            UpdateStatus::UpToDate { .. } => t("Check Again"),
            UpdateStatus::Available { .. } => t("View Update"),
            UpdateStatus::Failed => t("Try Again"),
        };
        let button = Button::new("settings-check-updates")
            .icon(icon(if update_available {
                "icons/download.svg"
            } else {
                "icons/refresh-cw.svg"
            }))
            .label(button_label)
            .disabled(checking);
        let mut action = if update_available {
            settings_primary_button(button, cx)
        } else {
            settings_button(button, cx)
        };
        if update_available {
            action = action.on_click(|_, _, cx| cx.open_url(update::LATEST_RELEASE_PAGE));
        } else {
            let workbench = self.workbench.clone();
            action = action.on_click(move |_, _, cx| {
                workbench.update(cx, |this, cx| this.check_for_updates(cx));
            });
        }

        let card = settings_info_card(
            concat!("Wake ", env!("CARGO_PKG_VERSION")),
            vec![div()
                .text_size(FONT_CAPTION)
                .text_color(if matches!(status, UpdateStatus::Failed) {
                    theme.danger
                } else {
                    theme.muted_foreground
                })
                .child(status_message)
                .into_any_element()],
            action.into_any_element(),
            zpx(84.),
            cx,
        );
        Self::rows_page(
            "settings-updates-scroll",
            t("Updates"),
            t("Keep Wake up to date."),
            SPACE_MD,
            [card.into_any_element()],
            cx,
        )
    }

    /// 两页 location 面板的快照重读(见字段注释):只刷当前显示的那页——切页走 Workbench
    /// 的 select_settings_page、本身就 notify,目标页在那一次刷到;设置窗开着时主窗的每次
    /// notify(敲一个搜索字)都会到这里,两页都刷是白付两遍 SQL + stat
    fn refresh_snapshots(&mut self, workbench: &Workbench) {
        match workbench.settings_page() {
            SettingsPage::Locations => self.locations = workbench.location_settings_snapshot(),
            SettingsPage::MemoryLocations => {
                self.memory_locations = workbench.memory_location_settings_snapshot()
            }
            _ => {}
        }
    }

    /// Session locations 的一行:Edit… 恒有,Show in Finder 挂存在的路径,Remove 只给自定义行
    fn render_location_row(&self, row: DataSourceRow, ix: usize, cx: &Context<Self>) -> AnyElement {
        let workbench = self.workbench.clone();
        let edit: RowAction = {
            let workbench = workbench.clone();
            let row = row.clone();
            Rc::new(move |window, cx| {
                let row = row.clone();
                workbench.update(cx, |this, cx| this.open_edit_location_form(row, window, cx));
            })
        };
        let reveal = row.exists.then(|| row.raw.clone());
        let remove: Option<RowAction> = row.custom.clone().map(|stored| {
            let workbench = workbench.clone();
            let agent = row.agent;
            Rc::new(move |window: &mut Window, cx: &mut App| {
                let stored = stored.clone();
                workbench.update(cx, |this, cx| {
                    this.delete_location(agent, stored, window, cx)
                });
            }) as RowAction
        });
        let toggle = {
            let (agent, path) = (row.agent, row.raw.clone());
            move |enabled: &bool, window: &mut Window, cx: &mut App| {
                let path = path.clone();
                workbench.update(cx, |this, cx| {
                    this.set_location_enabled(agent, path, *enabled, window, cx)
                });
            }
        };
        self.location_row(
            ix,
            row.display,
            row.tally,
            row.enabled,
            row.exists,
            row_menu(ix, Some(edit), reveal, remove),
            toggle,
            cx,
        )
    }

    /// Memory locations 的一行:默认来源只有开关与 Show in Finder,自定义来源多 Edit /
    /// Remove;项目模式行(`<project>/CLAUDE.md`)没有 Finder
    fn render_memory_location_row(
        &self,
        row: MemoryLocationRow,
        ix: usize,
        cx: &Context<Self>,
    ) -> AnyElement {
        let workbench = self.workbench.clone();
        let edit: Option<RowAction> = row.custom.then(|| {
            let workbench = workbench.clone();
            let row = row.clone();
            Rc::new(move |window: &mut Window, cx: &mut App| {
                let row = row.clone();
                workbench.update(cx, |this, cx| {
                    this.open_edit_memory_location_form(row, window, cx)
                });
            }) as RowAction
        });
        let reveal = (!row.pattern && row.exists).then(|| row.raw.clone());
        let remove: Option<RowAction> = row.custom.then(|| {
            let workbench = workbench.clone();
            let (agent, stored) = (row.agent, row.raw.clone());
            Rc::new(move |window: &mut Window, cx: &mut App| {
                let stored = stored.clone();
                workbench.update(cx, |this, cx| {
                    this.delete_memory_source(agent, stored, window, cx)
                });
            }) as RowAction
        });
        let toggle = {
            let (agent, id) = (row.agent, row.raw.clone());
            move |enabled: &bool, window: &mut Window, cx: &mut App| {
                let id = id.clone();
                workbench.update(cx, |this, cx| {
                    this.set_memory_source_enabled(agent, id, *enabled, window, cx)
                });
            }
        };
        self.location_row(
            ix,
            row.display,
            row.tally,
            row.enabled,
            row.exists,
            row_menu(ix, edit, reveal, remove),
            toggle,
            cx,
        )
    }

    /// location 行的壳(Session locations 与 Memory locations 共用):路径 + 计数 / 状态词、
    /// `…` 菜单、开关;停用的行文字降为 muted,路径不在时状态词用 warning 色。两页从不
    /// 同时渲染,元素 id 共用一套
    #[allow(clippy::too_many_arguments)]
    fn location_row(
        &self,
        ix: usize,
        display: SharedString,
        tally: SharedString,
        enabled: bool,
        exists: bool,
        menu: impl IntoElement,
        on_toggle: impl Fn(&bool, &mut Window, &mut App) + 'static,
        cx: &Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        h_flex()
            .id(("settings-location-row", ix))
            .min_h(zpx(60.))
            .w_full()
            .px(SPACE_LG)
            .gap(SPACE_MD)
            .items_center()
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap(zpx(3.))
                    .child(
                        div()
                            .w_full()
                            .truncate()
                            .text_size(FONT_BODY)
                            .text_color(if enabled {
                                theme.foreground
                            } else {
                                theme.muted_foreground
                            })
                            .child(display),
                    )
                    .child(
                        div()
                            .text_size(FONT_CAPTION)
                            .text_color(if !enabled || exists {
                                theme.muted_foreground
                            } else {
                                theme.warning
                            })
                            .child(tally),
                    ),
            )
            .child(menu)
            .child(
                Switch::new(("settings-location-enabled", ix))
                    .checked(enabled)
                    .small()
                    .tooltip(if enabled {
                        t("Disable location")
                    } else {
                        t("Enable location")
                    })
                    .on_click(on_toggle),
            )
            .into_any_element()
    }

    /// 一家的一组行(Session locations 与 Memory locations 共用):品牌图 + 名字的组头,
    /// 下面一张圆角卡,行与行之间一条 hairline
    fn render_agent_group(
        &self,
        agent: AgentId,
        rows: Vec<AnyElement>,
        cx: &Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let dark = theme.mode.is_dark();
        v_flex()
            .gap(SPACE_SM)
            .child(
                h_flex()
                    .h(zpx(24.))
                    .gap(ICON_TEXT_GAP)
                    .items_center()
                    .child(img(agent.brand_icon(dark)).size(zpx(17.)).flex_shrink_0())
                    .child(
                        div()
                            .text_size(FONT_CAPTION)
                            .font_semibold()
                            .text_color(theme.foreground)
                            .child(agent.display_name()),
                    ),
            )
            .child(
                v_flex()
                    .w_full()
                    .overflow_hidden()
                    .rounded(theme.radius_lg)
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.popover)
                    .children(rows.into_iter().enumerate().map(|(ix, row)| {
                        div()
                            .w_full()
                            .when(ix > 0, |this| this.border_t_1().border_color(theme.border))
                            .child(row)
                    })),
            )
            .into_any_element()
    }

    /// Settings → Memory locations:Session locations 同形制的页(页壳 `locations_page`)
    fn render_memory_locations(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let snapshot = self.memory_locations.clone();
        let mut row_offset = 0usize;
        let list = self.render_agent_groups(
            group_by_agent(snapshot.rows, |row| row.agent),
            &mut row_offset,
            Self::render_memory_location_row,
            cx,
        );
        self.locations_page(
            t("Memory locations"),
            t("Choose which memory and instruction files Wake indexes."),
            snapshot.diverged,
            Workbench::open_add_memory_location_form,
            Workbench::restore_default_memory_locations,
            list,
            cx,
        )
    }

    fn render_locations(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let snapshot = self.locations.clone();
        let (available, unavailable): (Vec<_>, Vec<_>) =
            group_by_agent(snapshot.rows, |row| row.agent)
                .into_iter()
                .partition(|(_, rows)| rows.iter().any(|row| row.exists || row.custom.is_some()));
        let unavailable_count = unavailable.len();
        let mut row_offset = 0usize;
        let mut list =
            self.render_agent_groups(available, &mut row_offset, Self::render_location_row, cx);
        if unavailable_count > 0 {
            let unavailable_elements = if self.show_unavailable {
                self.render_agent_groups(
                    unavailable,
                    &mut row_offset,
                    Self::render_location_row,
                    cx,
                )
            } else {
                Vec::new()
            };
            list.push(
                v_flex()
                    .gap(SPACE_LG)
                    .child(
                        h_flex()
                            .id("settings-unavailable-locations")
                            .h(zpx(36.))
                            .w_full()
                            .pr(SPACE_SM)
                            .gap(SPACE_SM)
                            .items_center()
                            .rounded(theme.radius)
                            .cursor_pointer()
                            .text_color(theme.muted_foreground)
                            .hover(|style| style.bg(theme.secondary_hover))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.show_unavailable = !this.show_unavailable;
                                cx.notify();
                            }))
                            .child(
                                div()
                                    .w(zpx(17.))
                                    .flex_shrink_0()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .child(
                                        icon(if self.show_unavailable {
                                            "icons/chevron-down.svg"
                                        } else {
                                            "icons/chevron-right.svg"
                                        })
                                        .with_size(zpx(13.)),
                                    ),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .text_size(FONT_CAPTION)
                                    .font_medium()
                                    .child(t("Not detected")),
                            )
                            .child(
                                div()
                                    .text_size(FONT_LABEL)
                                    .child(unavailable_count.to_string()),
                            ),
                    )
                    .children(unavailable_elements)
                    .into_any_element(),
            );
        }
        self.locations_page(
            t("Session locations"),
            t("Choose where Wake looks for local agent sessions."),
            snapshot.diverged,
            Workbench::open_add_location_form,
            Workbench::restore_default_locations,
            list,
            cx,
        )
    }

    /// 按 agent 成组渲染(两页共用):行号在整页内连续,各组的元素 id 不重叠
    fn render_agent_groups<R>(
        &self,
        groups: Vec<(AgentId, Vec<R>)>,
        row_offset: &mut usize,
        render_row: impl Fn(&Self, R, usize, &Context<Self>) -> AnyElement,
        cx: &Context<Self>,
    ) -> Vec<AnyElement> {
        groups
            .into_iter()
            .map(|(agent, rows)| {
                let start = *row_offset;
                *row_offset += rows.len();
                let rendered = rows
                    .into_iter()
                    .enumerate()
                    .map(|(ix, row)| render_row(self, row, start + ix, cx))
                    .collect();
                self.render_agent_group(agent, rendered, cx)
            })
            .collect()
    }

    /// location 页的壳(Session locations 与 Memory locations 共用):页头 = 标题 + 一句
    /// 说明 + Add location + `…` 里的 Restore defaults(没有偏离时禁用),下面是可滚动的
    /// 分组列表。两页从不同时渲染,元素 id 共用一套
    #[allow(clippy::too_many_arguments)]
    fn locations_page(
        &self,
        title: &'static str,
        caption: &'static str,
        diverged: bool,
        on_add: fn(&mut Workbench, &mut Window, &mut Context<Workbench>),
        on_restore: fn(&mut Workbench, &mut Window, &mut Context<Workbench>),
        list: Vec<AnyElement>,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let add_workbench = self.workbench.clone();
        let restore_workbench = self.workbench.clone();
        v_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .bg(theme.background)
            .child(
                h_flex()
                    .flex_shrink_0()
                    .px(SPACE_XXL)
                    .pt(SETTINGS_PAGE_TOP)
                    .pb(SPACE_XL)
                    .gap(SPACE_LG)
                    .items_start()
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap(zpx(5.))
                            .child(
                                div()
                                    .text_size(FONT_TITLE)
                                    .font_semibold()
                                    .text_color(theme.foreground)
                                    .child(title),
                            )
                            .child(
                                div()
                                    .text_size(FONT_CAPTION)
                                    .text_color(theme.muted_foreground)
                                    .child(caption),
                            ),
                    )
                    .child(
                        settings_button(
                            Button::new("settings-add-location")
                                .icon(icon("icons/plus.svg").with_size(zpx(13.)))
                                .label(t("Add location")),
                            cx,
                        )
                        .on_click(move |_, window, cx| {
                            add_workbench.update(cx, |this, cx| on_add(this, window, cx));
                        }),
                    )
                    .child(
                        Button::new("settings-location-more")
                            .ghost()
                            .small()
                            .rounded(RADIUS_BUTTON)
                            .icon(icon("icons/more-horizontal.svg").with_size(zpx(14.)))
                            .dropdown_menu(move |menu, _, _| {
                                let workbench = restore_workbench.clone();
                                menu.min_w(zpx(180.)).item(
                                    PopupMenuItem::new(t("Restore defaults"))
                                        .disabled(!diverged)
                                        .on_click(move |_, window, cx| {
                                            workbench.update(cx, |this, cx| {
                                                on_restore(this, window, cx)
                                            });
                                        }),
                                )
                            }),
                    ),
            )
            .child(
                v_flex()
                    .id("settings-location-list")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px(SPACE_XXL)
                    .pb(zpx(40.))
                    .gap(SPACE_XL)
                    .children(list),
            )
    }

    /// Settings → Remote hosts:SSH 会话聚合(阶段 1:只读镜像)。版式与
    /// Locations 页同一套:标题 + 说明,右侧低强调的 Sync now / Add host;host
    /// 列表是一张 popover 底的圆角卡,一行一台——名字为主信息、同步状态为
    /// muted 副信息(失败用 danger),`…` 菜单集中 Sync now / Remove,最右是
    /// 开关;添加走与 location 同材质的表单弹窗(2026-09-03 用户要求统一)。
    fn render_remotes(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let rows = self.workbench.read(cx).remote_hosts_snapshot();
        let syncing = self.workbench.read(cx).remote_sync_in_progress();
        let sync_workbench = self.workbench.clone();
        let add_workbench = self.workbench.clone();
        let has_hosts = !rows.is_empty();

        let row_elements: Vec<AnyElement> = rows
            .into_iter()
            .enumerate()
            .map(|(ix, row)| {
                let menu_workbench = self.workbench.clone();
                let toggle_workbench = self.workbench.clone();
                let menu_name = row.name.clone();
                let toggle_name = row.name.clone();
                let enabled = row.enabled;
                let menu = Button::new(("settings-remote-host-menu", ix))
                    .ghost()
                    .small()
                    .rounded(RADIUS_BUTTON)
                    .icon(icon("icons/more-horizontal.svg").with_size(zpx(14.)))
                    .dropdown_menu(move |menu, _, _| {
                        let sync_workbench = menu_workbench.clone();
                        let sync_name = menu_name.clone();
                        let remove_workbench = menu_workbench.clone();
                        let remove_name = menu_name.clone();
                        menu.min_w(zpx(180.))
                            .item(
                                PopupMenuItem::new(t("Sync now"))
                                    .disabled(syncing || !enabled)
                                    .on_click(move |_, _, cx| {
                                        let name = sync_name.to_string();
                                        sync_workbench.update(cx, |this, cx| {
                                            this.spawn_remote_sync(vec![name], cx)
                                        });
                                    }),
                            )
                            .separator()
                            .item(
                                PopupMenuItem::new(t("Remove")).on_click(move |_, window, cx| {
                                    let name = remove_name.clone();
                                    remove_workbench.update(cx, |this, cx| {
                                        this.confirm_remove_remote_host(name, window, cx)
                                    });
                                }),
                            )
                    });
                div()
                    .w_full()
                    .when(ix > 0, |this| this.border_t_1().border_color(theme.border))
                    .child(
                        h_flex()
                            .id(("settings-remote-host-row", ix))
                            .min_h(zpx(60.))
                            .w_full()
                            .px(SPACE_LG)
                            .gap(SPACE_MD)
                            .items_center()
                            .child(
                                v_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .gap(zpx(3.))
                                    .child(
                                        div()
                                            .w_full()
                                            .truncate()
                                            .text_size(FONT_BODY)
                                            .text_color(if enabled {
                                                theme.foreground
                                            } else {
                                                theme.muted_foreground
                                            })
                                            .child(row.name.clone()),
                                    )
                                    .child(
                                        div()
                                            .w_full()
                                            .truncate()
                                            .text_size(FONT_CAPTION)
                                            .text_color(if row.failed && enabled {
                                                theme.danger
                                            } else {
                                                theme.muted_foreground
                                            })
                                            .child(row.status.clone()),
                                    ),
                            )
                            .child(menu)
                            .child(
                                Switch::new(("settings-remote-host-enabled", ix))
                                    .checked(enabled)
                                    .small()
                                    .tooltip(if enabled {
                                        t("Disable host")
                                    } else {
                                        t("Enable host")
                                    })
                                    .on_click(move |checked, window, cx| {
                                        let enabled = *checked;
                                        toggle_workbench.update(cx, |this, cx| {
                                            this.set_remote_host_enabled(
                                                toggle_name.as_ref(),
                                                enabled,
                                                window,
                                                cx,
                                            );
                                        });
                                    }),
                            ),
                    )
                    .into_any_element()
            })
            .collect();

        v_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .bg(theme.background)
            .child(
                h_flex()
                    .flex_shrink_0()
                    .px(SPACE_XXL)
                    .pt(SETTINGS_PAGE_TOP)
                    .pb(SPACE_XL)
                    .gap(SPACE_LG)
                    .items_start()
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap(zpx(5.))
                            .child(
                                div()
                                    .text_size(FONT_TITLE)
                                    .font_semibold()
                                    .text_color(theme.foreground)
                                    .child(t("Remote hosts")),
                            )
                            .child(
                                div()
                                    .text_size(FONT_CAPTION)
                                    .text_color(theme.muted_foreground)
                                    .child(
                                        "Mirror agent sessions from other machines over SSH. \
                                         Read-only: nothing on the remote is ever written.",
                                    ),
                            ),
                    )
                    .when(has_hosts, |this| {
                        this.child(
                            settings_button(
                                Button::new("settings-sync-remotes")
                                    .icon(icon("icons/refresh-cw.svg").with_size(zpx(13.)))
                                    .label(if syncing { t("Syncing…") } else { t("Sync now") }),
                                cx,
                            )
                            .disabled(syncing)
                            .on_click(move |_, _, cx| {
                                sync_workbench
                                    .update(cx, |this, cx| this.sync_all_remote_hosts(cx));
                            }),
                        )
                    })
                    .child(
                        settings_button(
                            Button::new("settings-add-remote-host")
                                .icon(icon("icons/plus.svg").with_size(zpx(13.)))
                                .label(t("Add host")),
                            cx,
                        )
                        .on_click(move |_, window, cx| {
                            add_workbench
                                .update(cx, |this, cx| this.open_add_remote_host_form(window, cx));
                        }),
                    ),
            )
            .child(
                v_flex()
                    .id("settings-remote-list")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px(SPACE_XXL)
                    .pb(zpx(40.))
                    .gap(SPACE_XL)
                    .when(has_hosts, |this| {
                        this.child(
                            v_flex()
                                .w_full()
                                // 同 Connect 页:overflow_hidden 的卡在滚动列里 min-height
                                // 视为 0,host 一多会被压扁裁切而不是滚动
                                .flex_shrink_0()
                                .overflow_hidden()
                                .rounded(theme.radius_lg)
                                .border_1()
                                .border_color(theme.border)
                                .bg(theme.popover)
                                .children(row_elements),
                        )
                    })
                    .when(!has_hosts, |this| {
                        this.child(
                            div()
                                .text_size(FONT_CAPTION)
                                .text_color(theme.muted_foreground)
                                .child(t("No remote hosts yet. Add one to mirror its sessions into Wake.")),
                        )
                    }),
            )
            .into_any_element()
    }
}

impl Focusable for SettingsView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for SettingsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if crate::ui_zoom::changed(&mut self.zoom) {
            crate::ui_zoom::move_traffic_lights(crate::ui_zoom::SETTINGS_LIGHTS_TOP, window);
        }
        let background = cx.theme().background;
        let foreground = cx.theme().foreground;
        let sidebar = self.render_sidebar(window, cx);
        let selected_page = self.workbench.read(cx).settings_page();
        let content = match selected_page {
            SettingsPage::General => self.render_general(cx),
            SettingsPage::Appearance => self.render_appearance(cx),
            SettingsPage::Locations => self.render_locations(cx).into_any_element(),
            SettingsPage::MemoryLocations => self.render_memory_locations(cx).into_any_element(),
            SettingsPage::Remotes => self.render_remotes(cx),
            SettingsPage::Connect => self.render_connect(cx),
            SettingsPage::Data => self.render_data(cx),
            SettingsPage::Updates => self.render_updates(cx),
            SettingsPage::About => self.render_about(cx),
        };
        div()
            .id("wake-settings")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &OpenSettings, _window, cx| {
                this.workbench
                    .update(cx, |workbench, cx| workbench.open_settings(cx));
            }))
            .on_action(cx.listener(|this, _: &OpenAbout, _window, cx| {
                this.workbench
                    .update(cx, |workbench, cx| workbench.open_about(cx));
            }))
            .on_action(cx.listener(|this, _: &OpenUpdates, _window, cx| {
                this.workbench
                    .update(cx, |workbench, cx| workbench.open_updates(cx));
            }))
            .size_full()
            .bg(background)
            .text_color(foreground)
            .child(h_flex().size_full().child(sidebar).child(content))
            .children(overlay_layers(window, cx))
    }
}
