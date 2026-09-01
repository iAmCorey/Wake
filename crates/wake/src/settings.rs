use gpui::prelude::FluentBuilder as _;
use gpui::*;
use gpui_component::button::{Button, ButtonCustomVariant, ButtonVariants as _};
use gpui_component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_component::switch::Switch;
use gpui_component::{
    h_flex, v_flex, ActiveTheme as _, Disableable as _, Icon, Root, Selectable as _, Sizable as _,
    StyledExt as _, TitleBar, WindowExt as _,
};

use wake_core::models::AgentId;

use crate::ui::{
    BUTTON_SM_H, FONT_BODY, FONT_CAPTION, FONT_DISPLAY, FONT_HEADING, FONT_LABEL, FONT_TITLE,
    RADIUS_BUTTON, SHOW_IN_FM, SPACE_LG, SPACE_MD, SPACE_SM, SPACE_XL, SPACE_XS, SPACE_XXL,
};
use crate::update::{self, UpdateStatus};
use crate::workbench::{DataSourceRow, OpenAbout, OpenSettings, OpenUpdates, Workbench};
use crate::{theme, theme::AppearancePreference};

const SETTINGS_SIDEBAR_W: Pixels = px(180.);
const SETTINGS_PAGE_TOP: Pixels = px(38.);

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
    Locations,
    Data,
    Updates,
    About,
}

/// Settings 内常规文字按钮共用一套尺寸和材质；避免页面各自混用
/// outline / primary / 默认 ButtonGroup 后形成多套视觉语言。
fn settings_button(button: Button, cx: &App) -> Button {
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
fn settings_primary_button(button: Button, cx: &App) -> Button {
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
}

pub(crate) struct SettingsView {
    focus_handle: FocusHandle,
    workbench: Entity<Workbench>,
    appearance: AppearancePreference,
    show_unavailable: bool,
    _workbench_observer: Option<Subscription>,
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
            this._workbench_observer = Some(cx.observe(&observed, |_, _, cx| cx.notify()));
        });
        Self {
            focus_handle: cx.focus_handle(),
            workbench,
            appearance: theme::appearance_preference(),
            show_unavailable: false,
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
            .h(px(34.))
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
            .child(icon(icon_path).with_size(px(15.)).flex_shrink_0())
            .child(div().text_size(FONT_BODY).font_medium().child(label))
            .into_any_element()
    }

    fn render_sidebar(&self, window: &Window, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let show_titlebar = cfg!(target_os = "macos")
            || matches!(window.window_decorations(), Decorations::Client { .. });

        v_flex()
            .w(SETTINGS_SIDEBAR_W)
            .h_full()
            .flex_shrink_0()
            .bg(theme.sidebar)
            .border_r_1()
            .border_color(theme.sidebar_border)
            .when(show_titlebar, |this| this.child(TitleBar::new()))
            .child(
                div()
                    .px(SPACE_LG)
                    .pt(SPACE_SM)
                    .pb(SPACE_XL)
                    .text_size(FONT_HEADING)
                    .font_semibold()
                    .text_color(theme.sidebar_foreground)
                    .child("Settings"),
            )
            .child(
                v_flex()
                    .flex_1()
                    .px(SPACE_SM)
                    .gap(px(2.))
                    .child(self.render_nav_item(
                        "settings-general-nav",
                        "General",
                        "icons/settings.svg",
                        SettingsPage::General,
                        cx,
                    ))
                    .child(self.render_nav_item(
                        "settings-locations-nav",
                        "Locations",
                        "icons/hard-drive.svg",
                        SettingsPage::Locations,
                        cx,
                    ))
                    .child(self.render_nav_item(
                        "settings-data-nav",
                        "Data",
                        "icons/database.svg",
                        SettingsPage::Data,
                        cx,
                    )),
            )
            .child(
                v_flex()
                    .px(SPACE_SM)
                    .pb(SPACE_SM)
                    .gap(px(2.))
                    .child(self.render_nav_item(
                        "settings-updates-nav",
                        "Updates",
                        "icons/download.svg",
                        SettingsPage::Updates,
                        cx,
                    ))
                    .child(self.render_nav_item(
                        "settings-about-nav",
                        "About",
                        "icons/info.svg",
                        SettingsPage::About,
                        cx,
                    )),
            )
            .into_any_element()
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
            .w(px(64.))
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
                        gpui_component::notification::Notification::error(format!(
                            "Couldn't save appearance: {error}"
                        )),
                        cx,
                    ),
                }
            }))
    }

    fn render_general(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        v_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .bg(theme.background)
            .child(
                v_flex()
                    .flex_shrink_0()
                    .px(SPACE_XXL)
                    .pt(SETTINGS_PAGE_TOP)
                    .pb(SPACE_XL)
                    .gap(px(5.))
                    .child(
                        div()
                            .text_size(FONT_TITLE)
                            .font_semibold()
                            .text_color(theme.foreground)
                            .child("General"),
                    )
                    .child(
                        div()
                            .text_size(FONT_CAPTION)
                            .text_color(theme.muted_foreground)
                            .child("Customize how Wake looks."),
                    ),
            )
            .child(
                v_flex().px(SPACE_XXL).child(
                    h_flex()
                        .min_h(px(72.))
                        .w_full()
                        .px(SPACE_LG)
                        .gap(SPACE_LG)
                        .items_center()
                        .rounded(theme.radius_lg)
                        .border_1()
                        .border_color(theme.border)
                        .bg(theme.popover)
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .gap(px(3.))
                                .child(
                                    div()
                                        .text_size(FONT_BODY)
                                        .text_color(theme.foreground)
                                        .child("Appearance"),
                                )
                                .child(
                                    div()
                                        .text_size(FONT_CAPTION)
                                        .text_color(theme.muted_foreground)
                                        .child("Follow the system or keep Wake light or dark."),
                                ),
                        )
                        .child(
                            h_flex()
                                .h(BUTTON_SM_H + px(4.))
                                .p(px(2.))
                                .rounded(theme.radius)
                                .border_1()
                                .border_color(theme.border)
                                .bg(theme.secondary)
                                .child(self.appearance_button(
                                    "appearance-system",
                                    "System",
                                    AppearancePreference::System,
                                    cx,
                                ))
                                .child(self.appearance_button(
                                    "appearance-light",
                                    "Light",
                                    AppearancePreference::Light,
                                    cx,
                                ))
                                .child(self.appearance_button(
                                    "appearance-dark",
                                    "Dark",
                                    AppearancePreference::Dark,
                                    cx,
                                )),
                        ),
                ),
            )
            .into_any_element()
    }

    fn render_data(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let snapshot = self.workbench.read(cx).data_settings_snapshot();
        let session_word = if snapshot.session_count == 1 {
            "session"
        } else {
            "sessions"
        };
        let summary: SharedString = format!(
            "{} {session_word} · {}",
            snapshot.session_count,
            format_storage_size(snapshot.size_bytes)
        )
        .into();
        let reveal_path = snapshot.raw_path.clone();
        let show_in_finder = settings_button(
            Button::new("settings-show-data")
                .icon(icon("icons/folder.svg").with_size(px(13.)))
                .label(SHOW_IN_FM),
            cx,
        )
        .on_click(move |_, _, _| {
            wake_core::services::terminal::open_in_file_manager(reveal_path.as_ref())
        });

        v_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .bg(theme.background)
            .child(
                v_flex()
                    .flex_shrink_0()
                    .px(SPACE_XXL)
                    .pt(SETTINGS_PAGE_TOP)
                    .pb(SPACE_XL)
                    .gap(px(5.))
                    .child(
                        div()
                            .text_size(FONT_TITLE)
                            .font_semibold()
                            .text_color(theme.foreground)
                            .child("Data"),
                    )
                    .child(
                        div()
                            .text_size(FONT_CAPTION)
                            .text_color(theme.muted_foreground)
                            .child(
                                "See where Wake stores local data. Sessions refresh automatically.",
                            ),
                    ),
            )
            .child(
                v_flex()
                    .px(SPACE_XXL)
                    .gap(SPACE_SM)
                    .child(
                        div()
                            .text_size(FONT_CAPTION)
                            .font_semibold()
                            .text_color(theme.foreground)
                            .child("Storage"),
                    )
                    .child(
                        v_flex()
                            .w_full()
                            .overflow_hidden()
                            .rounded(theme.radius_lg)
                            .border_1()
                            .border_color(theme.border)
                            .bg(theme.popover)
                            .child(
                                h_flex()
                                    .min_h(px(84.))
                                    .px(SPACE_LG)
                                    .gap(SPACE_LG)
                                    .items_center()
                                    .child(
                                        v_flex()
                                            .flex_1()
                                            .min_w_0()
                                            .gap(px(3.))
                                            .child(
                                                div()
                                                    .text_size(FONT_BODY)
                                                    .text_color(theme.foreground)
                                                    .child("Wake data"),
                                            )
                                            .child(
                                                div()
                                                    .w_full()
                                                    .truncate()
                                                    .text_size(FONT_CAPTION)
                                                    .text_color(theme.muted_foreground)
                                                    .child(snapshot.display_path),
                                            )
                                            .child(
                                                div()
                                                    .text_size(FONT_CAPTION)
                                                    .text_color(theme.muted_foreground)
                                                    .child(summary),
                                            ),
                                    )
                                    .child(show_in_finder),
                            ),
                    ),
            )
            .into_any_element()
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
        let version: SharedString = format!("Version {}", env!("CARGO_PKG_VERSION")).into();

        v_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .items_center()
            .bg(theme.background)
            .child(
                v_flex()
                    .w(px(360.))
                    .items_center()
                    .pt(px(52.))
                    .child(
                        img("brands/wake.svg")
                            .size(px(78.))
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
                            .child("All your AI agent sessions, in one place."),
                    )
                    .child(div().mt(px(14.)).child(self.about_link(
                        "about-github",
                        "GitHub ↗",
                        "https://github.com/iAmCorey/Wake",
                        cx,
                    )))
                    .child(div().w(px(32.)).h(px(1.)).my(SPACE_LG).bg(theme.border))
                    .child(
                        div()
                            .text_size(FONT_LABEL)
                            .font_family(theme.mono_font_family.clone())
                            .text_color(faint)
                            .child("© 2026 Corey Chiu · MIT License"),
                    )
                    .child(
                        h_flex()
                            .mt(SPACE_XS)
                            .text_size(FONT_LABEL)
                            .font_family(theme.mono_font_family.clone())
                            .text_color(faint)
                            .child("Built with ❤️ by ")
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
            UpdateStatus::Idle => "Check GitHub Releases for a newer version.".into(),
            UpdateStatus::Checking => "Checking GitHub Releases…".into(),
            UpdateStatus::UpToDate { latest } => {
                format!("No newer release is available (latest: {latest}).").into()
            }
            UpdateStatus::Available { latest } => {
                format!("Wake {latest} is available. Open the release page to download it.").into()
            }
            UpdateStatus::Failed => {
                "Couldn't check for updates. Check your connection and try again.".into()
            }
        };
        let button_label = match status {
            UpdateStatus::Idle => "Check for Updates",
            UpdateStatus::Checking => "Checking…",
            UpdateStatus::UpToDate { .. } => "Check Again",
            UpdateStatus::Available { .. } => "View Update",
            UpdateStatus::Failed => "Try Again",
        };
        let button = Button::new("settings-check-updates")
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

        v_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .bg(theme.background)
            .child(
                v_flex()
                    .flex_shrink_0()
                    .px(SPACE_XXL)
                    .pt(SETTINGS_PAGE_TOP)
                    .pb(SPACE_XL)
                    .gap(px(5.))
                    .child(
                        div()
                            .text_size(FONT_TITLE)
                            .font_semibold()
                            .text_color(theme.foreground)
                            .child("Updates"),
                    )
                    .child(
                        div()
                            .text_size(FONT_CAPTION)
                            .text_color(theme.muted_foreground)
                            .child("Keep Wake up to date."),
                    ),
            )
            .child(
                v_flex().px(SPACE_XXL).child(
                    h_flex()
                        .min_h(px(84.))
                        .w_full()
                        .px(SPACE_LG)
                        .gap(SPACE_LG)
                        .items_center()
                        .rounded(theme.radius_lg)
                        .border_1()
                        .border_color(theme.border)
                        .bg(theme.popover)
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .gap(px(3.))
                                .child(
                                    div()
                                        .text_size(FONT_BODY)
                                        .text_color(theme.foreground)
                                        .child(format!("Wake {}", env!("CARGO_PKG_VERSION"))),
                                )
                                .child(
                                    div()
                                        .text_size(FONT_CAPTION)
                                        .text_color(if matches!(status, UpdateStatus::Failed) {
                                            theme.danger
                                        } else {
                                            theme.muted_foreground
                                        })
                                        .child(status_message),
                                ),
                        )
                        .child(action),
                ),
            )
            .into_any_element()
    }

    fn render_location_row(&self, row: DataSourceRow, ix: usize, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let enabled = row.enabled;
        let exists = row.exists;
        let raw = row.raw.clone();

        let edit_workbench = self.workbench.clone();
        let edit_row = row.clone();
        let reveal_path = row.raw.clone();
        let remove_target = row.custom.clone();
        let remove_agent = row.agent;
        let menu = Button::new(("settings-location-menu", ix))
            .ghost()
            .small()
            .rounded(RADIUS_BUTTON)
            .icon(icon("icons/more-horizontal.svg").with_size(px(14.)))
            .dropdown_menu(move |menu, _, _| {
                let workbench = edit_workbench.clone();
                let edit_row = edit_row.clone();
                let mut menu = menu
                    .min_w(px(180.))
                    .item(PopupMenuItem::new("Edit…").on_click(move |_, window, cx| {
                        let row = edit_row.clone();
                        workbench
                            .update(cx, |this, cx| this.open_edit_location_form(row, window, cx));
                    }));
                if exists {
                    let path = reveal_path.clone();
                    menu = menu.item(PopupMenuItem::new(SHOW_IN_FM).on_click(move |_, _, _| {
                        wake_core::services::terminal::open_in_file_manager(path.as_ref())
                    }));
                }
                if let Some(stored) = remove_target.clone() {
                    let workbench = edit_workbench.clone();
                    menu = menu.separator().item(PopupMenuItem::new("Remove").on_click(
                        move |_, window, cx| {
                            let stored = stored.clone();
                            workbench.update(cx, |this, cx| {
                                this.delete_location(remove_agent, stored, window, cx)
                            });
                        },
                    ));
                }
                menu
            });

        let toggle_workbench = self.workbench.clone();
        let toggle_path = raw;
        let toggle_agent = row.agent;
        h_flex()
            .id(("settings-location-row", ix))
            .min_h(px(60.))
            .w_full()
            .px(SPACE_LG)
            .gap(SPACE_MD)
            .items_center()
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap(px(3.))
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
                            .child(row.display),
                    )
                    .child(
                        div()
                            .text_size(FONT_CAPTION)
                            .text_color(if !enabled || exists {
                                theme.muted_foreground
                            } else {
                                theme.warning
                            })
                            .child(row.tally),
                    ),
            )
            .child(menu)
            .child(
                Switch::new(("settings-location-enabled", ix))
                    .checked(enabled)
                    .small()
                    .tooltip(if enabled {
                        "Disable location"
                    } else {
                        "Enable location"
                    })
                    .on_click(move |enabled, window, cx| {
                        let path = toggle_path.clone();
                        toggle_workbench.update(cx, |this, cx| {
                            this.set_location_enabled(toggle_agent, path, *enabled, window, cx)
                        });
                    }),
            )
            .into_any_element()
    }

    fn render_agent_group(
        &self,
        agent: AgentId,
        rows: Vec<DataSourceRow>,
        row_offset: usize,
        cx: &Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let dark = theme.mode.is_dark();
        v_flex()
            .gap(SPACE_SM)
            .child(
                h_flex()
                    .h(px(24.))
                    .gap(SPACE_SM)
                    .items_center()
                    .child(img(agent.brand_icon(dark)).size(px(17.)).flex_shrink_0())
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
                            .child(self.render_location_row(row, row_offset + ix, cx))
                    })),
            )
            .into_any_element()
    }

    fn render_locations(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let snapshot = self.workbench.read(cx).location_settings_snapshot();
        let mut groups: Vec<(AgentId, Vec<DataSourceRow>)> = Vec::new();
        for row in snapshot.rows {
            match groups.last_mut() {
                Some((agent, rows)) if *agent == row.agent => rows.push(row),
                _ => groups.push((row.agent, vec![row])),
            }
        }
        let (available, unavailable): (Vec<_>, Vec<_>) = groups
            .into_iter()
            .partition(|(_, rows)| rows.iter().any(|row| row.exists || row.custom.is_some()));
        let unavailable_count = unavailable.len();
        let add_workbench = self.workbench.clone();
        let restore_workbench = self.workbench.clone();
        let diverged = snapshot.diverged;

        let mut row_offset = 0usize;
        let available_elements: Vec<AnyElement> = available
            .into_iter()
            .map(|(agent, rows)| {
                let start = row_offset;
                row_offset += rows.len();
                self.render_agent_group(agent, rows, start, cx)
            })
            .collect();
        let unavailable_elements: Vec<AnyElement> = if self.show_unavailable {
            unavailable
                .into_iter()
                .map(|(agent, rows)| {
                    let start = row_offset;
                    row_offset += rows.len();
                    self.render_agent_group(agent, rows, start, cx)
                })
                .collect()
        } else {
            Vec::new()
        };

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
                            .gap(px(5.))
                            .child(
                                div()
                                    .text_size(FONT_TITLE)
                                    .font_semibold()
                                    .text_color(theme.foreground)
                                    .child("Session locations"),
                            )
                            .child(
                                div()
                                    .text_size(FONT_CAPTION)
                                    .text_color(theme.muted_foreground)
                                    .child("Choose where Wake looks for local agent sessions."),
                            ),
                    )
                    .child(
                        settings_button(
                            Button::new("settings-add-location")
                                .icon(icon("icons/plus.svg").with_size(px(13.)))
                                .label("Add location"),
                            cx,
                        )
                        .on_click(move |_, window, cx| {
                            add_workbench
                                .update(cx, |this, cx| this.open_add_location_form(window, cx));
                        }),
                    )
                    .child(
                        Button::new("settings-location-more")
                            .ghost()
                            .small()
                            .rounded(RADIUS_BUTTON)
                            .icon(icon("icons/more-horizontal.svg").with_size(px(14.)))
                            .dropdown_menu(move |menu, _, _| {
                                let workbench = restore_workbench.clone();
                                menu.min_w(px(180.)).item(
                                    PopupMenuItem::new("Restore defaults")
                                        .disabled(!diverged)
                                        .on_click(move |_, window, cx| {
                                            workbench.update(cx, |this, cx| {
                                                this.restore_default_locations(window, cx)
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
                    .pb(px(40.))
                    .gap(SPACE_XL)
                    .children(available_elements)
                    .when(unavailable_count > 0, |this| {
                        this.child(
                            v_flex()
                                .gap(SPACE_LG)
                                .child(
                                    h_flex()
                                        .id("settings-unavailable-locations")
                                        .h(px(36.))
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
                                                .w(px(17.))
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
                                                    .with_size(px(13.)),
                                                ),
                                        )
                                        .child(
                                            div()
                                                .flex_1()
                                                .text_size(FONT_CAPTION)
                                                .font_medium()
                                                .child("Not detected"),
                                        )
                                        .child(
                                            div()
                                                .text_size(FONT_LABEL)
                                                .child(unavailable_count.to_string()),
                                        ),
                                )
                                .children(unavailable_elements),
                        )
                    }),
            )
    }
}

impl Focusable for SettingsView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for SettingsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let background = cx.theme().background;
        let foreground = cx.theme().foreground;
        let sidebar = self.render_sidebar(window, cx);
        let selected_page = self.workbench.read(cx).settings_page();
        let content = match selected_page {
            SettingsPage::General => self.render_general(cx),
            SettingsPage::Locations => self.render_locations(cx).into_any_element(),
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
            .children(Root::render_dialog_layer(window, cx))
            .children(Root::render_notification_layer(window, cx))
    }
}
