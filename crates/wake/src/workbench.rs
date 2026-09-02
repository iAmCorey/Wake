// ============================================================================
// DIRECTION CONTRACT (impeccable)
// THESIS: 找回任何一段 agent 对话只需几秒;界面以 macOS 原生语言隐入背景,
//   拒绝"开发者工具=黑底霓虹终端风"的品类默认。
// OWN-WORLD: Things/Bear + Claude 客户端基准的原生 macOS 质感——暖白/暖黑双模式、
//   色差分区(无 hairline 依赖)、8px 圆角胶囊选中态(按钮 6px)、系统蓝 accent、
//   lucide 单线图标、SF 系统字体 14px 基准;agent 品牌色仅作识别圆点。
// STORY: 打开即见全部会话按时间流动;左栏收窄范围,中栏定位会话,右栏读全文;
//   ⌘K 直达任意一句话;一键回到终端继续。
// FIRST VIEWPORT: 全高三栏——224px 侧栏(全局搜索/全部/收藏/智能体/项目)、
//   336px 会话列表(上下文标题+会话数量+双行列表)，余宽为详情阅读器。
// FORM: brief-pinned canon(用户指定"现代 macOS 设计规范",对标 Things/Bear);
//   concept tournament 依规跳过,canon at full fidelity。
// FINISH: unreviewed and undocumented is unfinished; this build ends with the
//   finish review, the verdict, and DESIGN.md
// ============================================================================
use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

use chrono::{Datelike as _, Local, NaiveDate, TimeZone as _};
use futures::StreamExt;
use gpui::prelude::FluentBuilder as _;
use gpui::*;
use gpui_component::button::{Button, ButtonCustomVariant, ButtonVariants as _};
use gpui_component::highlighter::HighlightTheme;
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::list::{List, ListDelegate, ListEvent, ListItem, ListState};
use gpui_component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_component::notification::Notification;
use gpui_component::scroll::{AutoScroll, ScrollableElement as _};
use gpui_component::spinner::Spinner;
use gpui_component::text::{TextView, TextViewStyle};
use gpui_component::{
    h_flex, v_flex, ActiveTheme as _, Icon, IndexPath, Root, Sizable as _, StyledExt as _,
    TitleBar, WindowExt as _,
};

use wake_core::adapters::{
    adapter_for, create_adapter_roster_for, path_owns, AdapterLocation, AgentAdapter,
};
use wake_core::db::Store;
use wake_core::models::Role as MessageRole;
use wake_core::models::*;
use wake_core::scanner::{run_scan, ScanEvents, ScanProgress};
use wake_core::services::{exporter, terminal};
use wake_core::watcher::{start_watcher, SessionWatcher};

use crate::format::{
    abs_date, clip_display, expand_tilde, fmt_tokens, month_year, one_line, smart_time, thousands,
    tilde_path,
};
use crate::settings::{SettingsPage, SettingsView};
use crate::ui::*;
use crate::update::{self, UpdateStatus};

actions!(
    wake,
    [
        ToggleSearch,
        RefreshSessions,
        OpenSettings,
        OpenUpdates,
        OpenAbout,
        PaletteUp,
        PaletteDown
    ]
);

pub const KEY_CONTEXT: &str = "Workbench";
/// ⌘K 面板容器的 key context(main.rs 的 ↑↓ 绑定与 dialog 元素共用)
pub const PALETTE_CONTEXT: &str = "WakePalette";
/// ⌘K 面板内容总高(输入行 + 结果列表 + footer);列表 flex_1 吃剩余空间
const PALETTE_HEIGHT: Pixels = px(492.);
/// location 表单标签列宽(Agent/Folder 两行共用)
const FORM_LABEL_W: Pixels = px(52.);
/// 左栏顶部由 44px 窗口控制区 + 44px 品牌行组成；中栏标题区共享总高度。
const LIBRARY_IDENTITY_HEIGHT: Pixels = px(88.);
/// 侧栏底部常态工具栏内容高；加上父容器 1px 顶部分隔线，总高 44px。
const SIDEBAR_FOOTER_ROW_HEIGHT: Pixels = px(43.);
/// 三栏固定结构宽度；工具摘要需要从窗口宽度反算真实可用空间。
const SIDEBAR_WIDTH: Pixels = px(224.);
const SESSION_STREAM_WIDTH: Pixels = px(336.);
const READER_MAX_WIDTH: Pixels = px(720.);
/// FONT_MSG_THINKING 使用等宽字体时，一个 ASCII 显示格的实测近似宽度。
const TOOL_MONO_CELL_WIDTH: f32 = 6.9;
/// Insights 主区块间距，明确落在 4px 网格上。
const INSIGHTS_SECTION_GAP: Pixels = px(32.);

type SharedAdapters = Arc<Vec<Box<dyn AgentAdapter>>>;
type SharedLocations = Arc<Vec<AdapterLocation>>;

fn icon(path: &'static str) -> Icon {
    Icon::empty().path(path)
}

/// 起一条后台扫描线程。启动时的自动扫描(full=false)与用户主动重扫(full=true)
/// 共用;返回的 Result 由 run_scan 的终态事件代为上报,这里只需丢弃。
fn spawn_scan(
    adapters: SharedAdapters,
    store: Arc<Store>,
    events: Arc<dyn ScanEvents>,
    full: bool,
) {
    std::thread::spawn(move || {
        let _ = run_scan(&adapters, &store, events.as_ref(), full);
    });
}

// ---------------- 后台事件桥 ----------------

enum BgEvent {
    Progress(ScanProgress),
    Changed,
    /// 监听后端丢过事件,需要一轮增量兜底
    RescanNeeded,
}

struct ChannelEvents(futures::channel::mpsc::UnboundedSender<BgEvent>);

impl ScanEvents for ChannelEvents {
    fn on_progress(&self, p: &ScanProgress) {
        let _ = self.0.unbounded_send(BgEvent::Progress(p.clone()));
    }
    fn on_sessions_changed(&self) {
        let _ = self.0.unbounded_send(BgEvent::Changed);
    }
    fn on_rescan_needed(&self) {
        let _ = self.0.unbounded_send(BgEvent::RescanNeeded);
    }
}

// ---------------- 会话列表 delegate ----------------

/// 336px 会话流在 14px 系统字体下约容纳 42 个西文显示单元；CJK 字符由
/// unicode-width 按两个单元计算。尾部每个状态图标再预留三个单元。
const SESSION_TITLE_MAX_WIDTH: usize = 42;
/// 会话流首批与后续每页的条数。列表组件会在距末尾 20 行时预取下一页。
const SESSION_PAGE_SIZE: i64 = 100;
static SESSION_PAGINATION_GENERATION: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, PartialEq, Eq)]
struct SessionGroup {
    label: SharedString,
    range: Range<usize>,
}

struct SessionPagination {
    store: Arc<Store>,
    filter: SessionFilter,
    total: i64,
    next_offset: i64,
    generation: u64,
    loading: bool,
    failed: bool,
}

fn session_group_timestamp(session: &SessionMeta, sort: SortKey) -> i64 {
    match sort {
        SortKey::Created => session.created_at,
        SortKey::Updated | SortKey::Messages => session.updated_at,
    }
}

fn session_group_label(ts: i64, today: NaiveDate) -> SharedString {
    let Some(date) = Local
        .timestamp_millis_opt(ts)
        .single()
        .map(|dt| dt.date_naive())
    else {
        return "Undated".into();
    };

    if date >= today {
        return "Today".into();
    }
    if date == today.pred_opt().unwrap_or(today) {
        return "Yesterday".into();
    }

    let week_start =
        today - chrono::Duration::days(i64::from(today.weekday().num_days_from_monday()));
    if date >= week_start {
        return "Earlier this week".into();
    }
    if date.year() == today.year() {
        return date.format("%B").to_string().into();
    }
    date.format("%B %Y").to_string().into()
}

#[derive(Debug, Clone)]
struct SessionListRow {
    meta: SessionMeta,
    depth: u8,
    child_count: i64,
    expanded: bool,
    /// 子会话必须紧跟父会话，不能因自己的时间戳被拆进另一个日期 section。
    group_timestamp: i64,
    group_pinned: bool,
}

fn build_row_groups_at(
    rows: &[SessionListRow],
    sort: SortKey,
    ascending: bool,
    today: NaiveDate,
) -> Vec<SessionGroup> {
    let time_descending = !ascending && matches!(sort, SortKey::Updated | SortKey::Created);
    if rows.is_empty() || !time_descending {
        return vec![SessionGroup {
            label: "".into(),
            range: 0..rows.len(),
        }];
    }

    let mut groups = Vec::new();
    let pinned_count = rows.iter().take_while(|row| row.group_pinned).count();
    if pinned_count > 0 {
        groups.push(SessionGroup {
            label: "Pinned".into(),
            range: 0..pinned_count,
        });
    }
    let mut start = pinned_count;
    while start < rows.len() {
        let label = session_group_label(rows[start].group_timestamp, today);
        let mut end = start + 1;
        while end < rows.len() && session_group_label(rows[end].group_timestamp, today) == label {
            end += 1;
        }
        groups.push(SessionGroup {
            label,
            range: start..end,
        });
        start = end;
    }
    groups
}

fn same_session_query(left: &SessionFilter, right: &SessionFilter) -> bool {
    left.agents == right.agents
        && left.project_path == right.project_path
        && left.favorite_only == right.favorite_only
        && left.include_archived == right.include_archived
        && left.roots_only == right.roots_only
        && left.title_query == right.title_query
        && left.sort == right.sort
        && left.ascending == right.ascending
}

fn session_matches_filter(session: &SessionMeta, filter: &SessionFilter) -> bool {
    (filter.agents.is_empty() || filter.agents.contains(&session.agent))
        && filter
            .project_path
            .as_ref()
            .is_none_or(|project| project == &session.project_path)
        && (!filter.favorite_only || session.favorite)
        && (filter.include_archived || !session.archived)
        && filter
            .title_query
            .as_deref()
            .map(str::trim)
            .filter(|query| !query.is_empty())
            .is_none_or(|query| {
                let query = query.to_lowercase();
                session.title.to_lowercase().contains(&query)
                    || session.project_name.to_lowercase().contains(&query)
            })
}

pub struct SessionsDelegate {
    /// 分页查询返回的根会话；next_offset 只能按它推进，展开的孩子不参与分页。
    pub sessions: Vec<SessionMeta>,
    rows: Vec<SessionListRow>,
    children: HashMap<String, Vec<SessionMeta>>,
    child_counts: HashMap<String, i64>,
    expanded: HashSet<String>,
    groups: Vec<SessionGroup>,
    grouped_on: NaiveDate,
    sort: SortKey,
    ascending: bool,
    pagination: Option<SessionPagination>,
}

impl SessionsDelegate {
    fn new(sessions: Vec<SessionMeta>, sort: SortKey, ascending: bool) -> Self {
        Self::new_at(sessions, sort, ascending, Local::now().date_naive())
    }

    fn new_at(
        sessions: Vec<SessionMeta>,
        sort: SortKey,
        ascending: bool,
        grouped_on: NaiveDate,
    ) -> Self {
        let mut delegate = Self {
            sessions,
            rows: Vec::new(),
            children: HashMap::new(),
            child_counts: HashMap::new(),
            expanded: HashSet::new(),
            groups: Vec::new(),
            grouped_on,
            sort,
            ascending,
            pagination: None,
        };
        delegate.rebuild_rows_at(grouped_on);
        delegate
    }

    fn paged(
        sessions: Vec<SessionMeta>,
        filter: SessionFilter,
        total: i64,
        store: Arc<Store>,
    ) -> Self {
        let child_counts = if filter.roots_only {
            store.child_counts(&filter).unwrap_or_default()
        } else {
            HashMap::new()
        };
        let mut delegate = Self::new(sessions, filter.sort, filter.ascending);
        delegate.child_counts = child_counts;
        delegate.pagination = Some(SessionPagination {
            store,
            total: total.max(0),
            next_offset: i64::try_from(delegate.sessions.len()).unwrap_or(i64::MAX),
            filter,
            generation: SESSION_PAGINATION_GENERATION.fetch_add(1, Ordering::Relaxed),
            loading: false,
            failed: false,
        });
        delegate.rebuild_rows();
        delegate
    }

    fn tree_mode(&self) -> bool {
        self.pagination
            .as_ref()
            .is_some_and(|page| page.filter.roots_only)
    }

    fn rebuild_rows(&mut self) {
        self.rebuild_rows_at(Local::now().date_naive());
    }

    fn rebuild_rows_at(&mut self, today: NaiveDate) {
        let tree_mode = self.tree_mode();
        let mut rows = Vec::new();
        for root in &self.sessions {
            let child_count = if tree_mode {
                self.child_counts.get(&root.key).copied().unwrap_or(0)
            } else {
                0
            };
            let expanded = child_count > 0 && self.expanded.contains(&root.key);
            let group_timestamp = session_group_timestamp(root, self.sort);
            let group_pinned = root.pinned;
            rows.push(SessionListRow {
                meta: root.clone(),
                depth: 0,
                child_count,
                expanded,
                group_timestamp,
                group_pinned,
            });
            if expanded {
                if let Some(children) = self.children.get(&root.key) {
                    rows.extend(children.iter().cloned().map(|child| SessionListRow {
                        meta: child,
                        depth: 1,
                        child_count: 0,
                        expanded: false,
                        group_timestamp,
                        group_pinned,
                    }));
                }
            }
        }
        self.rows = rows;
        self.groups = build_row_groups_at(&self.rows, self.sort, self.ascending, today);
        self.grouped_on = today;
    }

    fn rebuild_groups_at(&mut self, today: NaiveDate) -> bool {
        if self.grouped_on == today {
            return false;
        }
        let groups = build_row_groups_at(&self.rows, self.sort, self.ascending, today);
        let changed = groups != self.groups;
        self.groups = groups;
        self.grouped_on = today;
        changed
    }

    fn append_sessions(&mut self, page: Vec<SessionMeta>) {
        let mut keys: HashSet<String> = self
            .sessions
            .iter()
            .map(|session| session.key.clone())
            .collect();
        self.sessions.extend(
            page.into_iter()
                .filter(|session| keys.insert(session.key.clone())),
        );
        if let Some(pagination) = &self.pagination {
            if pagination.filter.roots_only {
                self.child_counts = pagination
                    .store
                    .child_counts(&pagination.filter)
                    .unwrap_or_default();
            }
        }
        self.rebuild_rows();
    }

    fn restore_expanded(&mut self, expanded: HashSet<String>) {
        if !self.tree_mode() {
            return;
        }
        let root_keys: HashSet<&str> = self.sessions.iter().map(|root| root.key.as_str()).collect();
        self.expanded = expanded
            .into_iter()
            .filter(|key| root_keys.contains(key.as_str()))
            .collect();
        self.reload_expanded();
    }

    fn reload_expanded(&mut self) {
        let Some(pagination) = &self.pagination else {
            return;
        };
        let store = pagination.store.clone();
        let filter = pagination.filter.clone();
        self.children.clear();
        for key in &self.expanded {
            if let Ok(children) = store.list_children(key, &filter) {
                self.children.insert(key.clone(), children);
            }
        }
        self.rebuild_rows();
    }

    fn toggle(&mut self, key: &str) -> bool {
        if !self.tree_mode() || self.child_counts.get(key).copied().unwrap_or(0) == 0 {
            return false;
        }
        if self.expanded.remove(key) {
            self.children.remove(key);
        } else {
            let Some(pagination) = &self.pagination else {
                return false;
            };
            let Ok(children) = pagination.store.list_children(key, &pagination.filter) else {
                return false;
            };
            self.expanded.insert(key.to_string());
            self.children.insert(key.to_string(), children);
        }
        self.rebuild_rows();
        true
    }

    fn ensure_expanded(&mut self, key: &str) -> bool {
        if self.expanded.contains(key) {
            return true;
        }
        self.toggle(key)
    }

    fn row_index(&self, key: &str) -> Option<usize> {
        self.rows.iter().position(|row| row.meta.key == key)
    }

    fn row_key(&self, ix: IndexPath) -> Option<&str> {
        self.flat_index(ix)
            .and_then(|flat| self.rows.get(flat))
            .map(|row| row.meta.key.as_str())
    }

    fn row_has_children(&self, key: &str) -> bool {
        self.child_counts.get(key).copied().unwrap_or(0) > 0
    }

    /// 把组件发出的分组坐标还原为可见 `rows` 的平铺下标。
    fn flat_index(&self, ix: IndexPath) -> Option<usize> {
        let group = self.groups.get(ix.section)?;
        let flat = group.range.start.checked_add(ix.row)?;
        (flat < group.range.end).then_some(flat)
    }

    /// 搜索/选择以可见 `rows` 平铺下标定位；进入列表前转成分组坐标。
    fn index_path(&self, flat: usize) -> Option<IndexPath> {
        self.groups
            .iter()
            .enumerate()
            .find(|(_, group)| group.range.contains(&flat))
            .map(|(section, group)| IndexPath::new(flat - group.range.start).section(section))
    }
}

impl ListDelegate for SessionsDelegate {
    type Item = ListItem;

    fn sections_count(&self, _cx: &App) -> usize {
        self.groups.len()
    }

    fn items_count(&self, section: usize, _cx: &App) -> usize {
        self.groups
            .get(section)
            .map(|group| group.range.len())
            .unwrap_or_default()
    }

    fn render_item(
        &mut self,
        ix: IndexPath,
        _window: &mut Window,
        cx: &mut Context<ListState<Self>>,
    ) -> Option<Self::Item> {
        let flat_ix = self.flat_index(ix)?;
        let row = self.rows.get(flat_ix)?.clone();
        let s = &row.meta;
        let theme = cx.theme();
        let shown_ts = session_group_timestamp(s, self.sort);
        let shown_time: SharedString = smart_time(shown_ts).into();
        let shown_tooltip: SharedString = abs_date(shown_ts).into();
        let title_tooltip: SharedString = s.title.clone().into();
        let show_chevron = self.tree_mode() && row.child_count > 0;
        let icon_width =
            3 * (usize::from(s.pinned) + usize::from(s.favorite) + usize::from(show_chevron));
        let title: SharedString =
            clip_display(&s.title, SESSION_TITLE_MAX_WIDTH.saturating_sub(icon_width)).into();
        let key = s.key.clone();
        let open_key = key.clone();
        let toggle_key = key.clone();
        let child_count = row.child_count;
        let expanded = row.expanded;
        let depth = row.depth;
        let child_line_color = theme.muted_foreground.opacity(0.38);
        // 2px 线的中心与父行 15px Grok 图标的 11.5px 中轴一致。
        let child_line_left = SPACE_XS + px(6.5);

        Some(
            // 稳定元素身份必须是 session key：展开插入孩子后，IndexPath 会整体
            // 位移；继续拿下标当 id 会复用到另一会话的点击状态。
            ListItem::new(SharedString::from(key.clone()))
                .rounded(theme.radius)
                .mx(SPACE_SM)
                .child(
                    v_flex()
                        .id(SharedString::from(format!("session-row:{key}")))
                        .relative()
                        .w_full()
                        .px(SPACE_XS)
                        .py(SPACE_SM)
                        .gap(SPACE_XS)
                        .when(depth > 0, |this| {
                            this.pl(px(20.)).child(
                                div()
                                    .absolute()
                                    .left(child_line_left)
                                    .top(SPACE_SM)
                                    .bottom(SPACE_SM)
                                    .w(px(2.))
                                    .rounded_full()
                                    .bg(child_line_color),
                            )
                        })
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                let Some(flat) = this.delegate().row_index(&open_key) else {
                                    return;
                                };
                                let Some(path) = this.delegate().index_path(flat) else {
                                    return;
                                };
                                let already_selected = this
                                    .selected_index()
                                    .and_then(|selected| this.delegate().row_key(selected))
                                    == Some(open_key.as_str());
                                if already_selected && this.delegate().row_has_children(&open_key) {
                                    this.delegate_mut().toggle(&open_key);
                                    if let Some(new_path) = this
                                        .delegate()
                                        .row_index(&open_key)
                                        .and_then(|flat| this.delegate().index_path(flat))
                                    {
                                        this.set_selected_index(Some(new_path), window, cx);
                                    }
                                    cx.notify();
                                    return;
                                }
                                this.focus(window, cx);
                                this.set_selected_index(Some(path), window, cx);
                                cx.emit(ListEvent::Confirm(path));
                                cx.notify();
                            }),
                        )
                        .child(
                            h_flex()
                                .gap(px(6.))
                                .when(show_chevron, |this| {
                                    this.child(
                                        div()
                                            .id(SharedString::from(format!(
                                                "session-expand:{toggle_key}"
                                            )))
                                            .size(px(16.))
                                            .flex_shrink_0()
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .rounded(px(4.))
                                            .cursor_pointer()
                                            .hover(|style| style.bg(theme.muted))
                                            .on_mouse_down(
                                                MouseButton::Left,
                                                cx.listener(move |this, _, window, cx| {
                                                    cx.stop_propagation();
                                                    let selected_key = this
                                                        .selected_index()
                                                        .and_then(|selected| {
                                                            this.delegate()
                                                                .row_key(selected)
                                                                .map(str::to_string)
                                                        });
                                                    if !this.delegate_mut().toggle(&toggle_key) {
                                                        return;
                                                    }
                                                    let selected_path = selected_key
                                                        .as_deref()
                                                        .and_then(|key| {
                                                            this.delegate().row_index(key).and_then(
                                                                |flat| {
                                                                    this.delegate().index_path(flat)
                                                                },
                                                            )
                                                        })
                                                        .or_else(|| {
                                                            this.delegate()
                                                                .row_index(&toggle_key)
                                                                .and_then(|flat| {
                                                                    this.delegate().index_path(flat)
                                                                })
                                                        });
                                                    this.set_selected_index(
                                                        selected_path,
                                                        window,
                                                        cx,
                                                    );
                                                    cx.notify();
                                                }),
                                            )
                                            .child(
                                                icon("icons/chevron-right.svg")
                                                    .with_size(px(13.))
                                                    .text_color(theme.muted_foreground)
                                                    .when(expanded, |icon| {
                                                        icon.rotate(Radians(
                                                            std::f32::consts::FRAC_PI_2,
                                                        ))
                                                    }),
                                            ),
                                    )
                                })
                                .child(
                                    div()
                                        .id(("session-title", flat_ix))
                                        .flex_1()
                                        .min_w_0()
                                        .overflow_hidden()
                                        .whitespace_nowrap()
                                        .text_size(FONT_BODY)
                                        .font_medium()
                                        .text_color(theme.foreground)
                                        .child(title)
                                        .tooltip(move |window, cx| {
                                            gpui_component::tooltip::Tooltip::new(
                                                title_tooltip.clone(),
                                            )
                                            .build(window, cx)
                                        }),
                                )
                                .when(s.pinned, |this| {
                                    this.child(
                                        icon("icons/pin-filled.svg")
                                            .with_size(px(11.))
                                            .text_color(theme.primary),
                                    )
                                })
                                .when(s.favorite, |this| {
                                    this.child(
                                        icon("icons/star-filled.svg")
                                            .with_size(px(11.))
                                            .text_color(rgb(crate::theme::STAR_YELLOW)),
                                    )
                                }),
                        )
                        .child(
                            h_flex()
                                .gap(px(6.))
                                .text_size(FONT_LABEL)
                                .text_color(theme.muted_foreground)
                                .child(
                                    img(s.agent.brand_icon(theme.mode.is_dark()))
                                        .size(px(15.))
                                        .flex_shrink_0(),
                                )
                                .child(badge(
                                    s.project_name.clone(),
                                    theme.muted,
                                    theme.muted_foreground,
                                ))
                                .when(show_chevron, |this| {
                                    this.child(
                                        div()
                                            .id(("session-child-count", flat_ix))
                                            .flex_shrink_0()
                                            .child(format!("{child_count}"))
                                            .tooltip(move |window, cx| {
                                                gpui_component::tooltip::Tooltip::new(format!(
                                                    "{child_count} nested session{}",
                                                    if child_count == 1 { "" } else { "s" }
                                                ))
                                                .build(window, cx)
                                            }),
                                    )
                                })
                                .child(div().flex_1())
                                .child(
                                    div()
                                        .id(("session-time", flat_ix))
                                        .flex_shrink_0()
                                        .child(shown_time)
                                        .tooltip(move |window, cx| {
                                            gpui_component::tooltip::Tooltip::new(
                                                shown_tooltip.clone(),
                                            )
                                            .build(window, cx)
                                        }),
                                ),
                        ),
                ),
        )
    }

    fn render_section_header(
        &mut self,
        section: usize,
        _window: &mut Window,
        cx: &mut Context<ListState<Self>>,
    ) -> Option<impl IntoElement> {
        let label = self.groups.get(section)?.label.clone();
        if label.is_empty() {
            return None;
        }
        let theme = cx.theme();

        Some(
            h_flex()
                .h(px(32.))
                .w_full()
                .items_center()
                .gap(SPACE_SM)
                .px(SPACE_MD)
                .pt(SPACE_XS)
                .text_size(FONT_LABEL)
                .font_medium()
                .text_color(theme.muted_foreground)
                .child(div().flex_shrink_0().child(label))
                .child(div().h(px(1.)).flex_1().bg(theme.border.opacity(0.72))),
        )
    }

    // 还有下一页时返回 true。失败后停住，等待用户刷新重建 delegate，
    // 避免触底重试风暴。
    fn has_more(&self, _cx: &App) -> bool {
        self.pagination
            .as_ref()
            .is_some_and(|page| !page.loading && !page.failed && page.next_offset < page.total)
    }

    fn load_more(&mut self, window: &mut Window, cx: &mut Context<ListState<Self>>) {
        let Some(page) = self.pagination.as_mut() else {
            return;
        };
        if page.loading || page.failed || page.next_offset >= page.total {
            return;
        }

        page.loading = true;
        let generation = page.generation;
        let query_offset = page.next_offset;
        let store = page.store.clone();
        let mut filter = page.filter.clone();
        filter.limit = SESSION_PAGE_SIZE;
        filter.offset = query_offset;

        let query = cx.background_spawn(async move { store.list_sessions(&filter) });
        cx.spawn_in(window, async move |this, cx| {
            let result = query.await;
            this.update(cx, |state, cx| {
                let delegate = state.delegate_mut();
                let Some(current) = delegate.pagination.as_mut() else {
                    return;
                };
                if current.generation != generation {
                    return;
                }
                current.loading = false;

                match result {
                    Ok((sessions, total)) => {
                        let received = i64::try_from(sessions.len()).unwrap_or(i64::MAX);
                        current.total = total.max(0);
                        let next_offset = if received == 0 {
                            current.total
                        } else {
                            query_offset.saturating_add(received).min(current.total)
                        };
                        // 搜索定位可能同时批量翻过更多页，较晚完成的单页请求
                        // 不能把它已经推进的 offset 倒退。
                        current.next_offset =
                            current.next_offset.max(next_offset).min(current.total);
                        delegate.append_sessions(sessions);
                    }
                    Err(_) => current.failed = true,
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    // 点击/回车走 ListEvent::Confirm(ix),无需自存选中态
    fn set_selected_index(
        &mut self,
        _ix: Option<IndexPath>,
        _window: &mut Window,
        _cx: &mut Context<ListState<Self>>,
    ) {
    }
}

#[cfg(test)]
mod session_group_tests {
    use std::collections::HashSet;
    use std::path::PathBuf;
    use std::sync::Arc;

    use chrono::{Local, NaiveDate, TimeZone as _};
    use gpui_component::IndexPath;
    use wake_core::adapters::AgentAdapter;
    use wake_core::db::Store;
    use wake_core::models::{
        AgentId, ParsedSession, ParsedTranscript, SessionFileRef, SessionFilter, SessionMeta,
        SortKey, ToolCallView,
    };

    use super::{
        clip_tool_text, load_visible_transcript, non_blank_tool_text, same_session_query,
        session_group_label, session_matches_filter, toggle_expanded_row, tool_cluster_heading,
        visible_git_branch, SessionsDelegate, SESSION_PAGE_SIZE,
    };

    struct FailingAdapter;

    impl AgentAdapter for FailingAdapter {
        fn agent(&self) -> AgentId {
            AgentId::Codex
        }

        fn list_session_files(&self) -> anyhow::Result<Vec<SessionFileRef>> {
            Ok(Vec::new())
        }

        fn parse_session(&self, _r: &SessionFileRef) -> anyhow::Result<ParsedSession> {
            Err(anyhow::anyhow!("unused session parser"))
        }

        fn parse_transcript(&self, _r: &SessionFileRef) -> anyhow::Result<ParsedTranscript> {
            Err(anyhow::anyhow!("broken transcript fixture"))
        }

        fn data_roots(&self) -> Vec<PathBuf> {
            vec![PathBuf::from("/tmp/wake-test")]
        }

        fn with_custom_root(&self, _dir: PathBuf) -> Box<dyn AgentAdapter> {
            Box::new(FailingAdapter)
        }
    }

    fn local_ms(year: i32, month: u32, day: u32) -> i64 {
        Local
            .with_ymd_and_hms(year, month, day, 12, 0, 0)
            .single()
            .expect("test date should be unambiguous in the local timezone")
            .timestamp_millis()
    }

    fn session(key: &str, updated_at: i64, pinned: bool) -> SessionMeta {
        SessionMeta {
            key: key.to_string(),
            id: key.to_string(),
            agent: AgentId::Codex,
            title: key.to_string(),
            project_path: "/tmp/wake-test".to_string(),
            project_name: "wake-test".to_string(),
            file_path: "/tmp/wake-test/session.jsonl".to_string(),
            created_at: updated_at,
            updated_at,
            message_count: 1,
            size_bytes: 1,
            git_branch: None,
            model: None,
            tokens_used: None,
            archived: false,
            source: None,
            favorite: false,
            pinned,
        }
    }

    fn tool_call(name: &str, input_preview: &str) -> ToolCallView {
        ToolCallView {
            id: name.to_string(),
            name: name.to_string(),
            input_preview: input_preview.to_string(),
            input: Some(input_preview.to_string()),
            output: None,
            is_error: false,
            sidechain_ref: None,
        }
    }

    #[test]
    fn date_labels_respect_calendar_week_boundaries() {
        let wednesday = NaiveDate::from_ymd_opt(2026, 9, 2).unwrap();
        assert_eq!(
            session_group_label(local_ms(2026, 9, 1), wednesday),
            "Yesterday"
        );
        assert_eq!(
            session_group_label(local_ms(2026, 8, 31), wednesday),
            "Earlier this week"
        );
        assert_eq!(
            session_group_label(local_ms(2026, 8, 30), wednesday),
            "August"
        );

        let monday = NaiveDate::from_ymd_opt(2026, 9, 7).unwrap();
        assert_eq!(
            session_group_label(local_ms(2026, 9, 6), monday),
            "Yesterday"
        );
        assert_eq!(
            session_group_label(local_ms(2026, 9, 5), monday),
            "September"
        );
    }

    #[test]
    fn time_descending_groups_pinned_and_calendar_ranges() {
        let sessions = vec![
            session("pinned", local_ms(2025, 1, 1), true),
            session("today", local_ms(2026, 9, 2), false),
            session("yesterday", local_ms(2026, 9, 1), false),
            session("week", local_ms(2026, 8, 31), false),
            session("month", local_ms(2026, 8, 30), false),
        ];
        let delegate = SessionsDelegate::new_at(
            sessions,
            SortKey::Updated,
            false,
            NaiveDate::from_ymd_opt(2026, 9, 2).unwrap(),
        );
        let groups = &delegate.groups;

        let labels: Vec<&str> = groups.iter().map(|group| group.label.as_ref()).collect();
        assert_eq!(
            labels,
            [
                "Pinned",
                "Today",
                "Yesterday",
                "Earlier this week",
                "August"
            ]
        );
        assert_eq!(groups[0].range, 0..1);
        assert_eq!(groups[3].range, 3..4);

        assert_eq!(delegate.flat_index(IndexPath::new(0).section(3)), Some(3));
        assert_eq!(delegate.index_path(4), Some(IndexPath::new(0).section(4)));
    }

    #[test]
    fn non_time_descending_sorts_stay_flat() {
        let sessions = vec![
            session("one", local_ms(2026, 9, 2), false),
            session("two", local_ms(2026, 9, 1), false),
        ];
        for (sort, ascending) in [(SortKey::Messages, false), (SortKey::Updated, true)] {
            let delegate = SessionsDelegate::new_at(
                sessions.clone(),
                sort,
                ascending,
                NaiveDate::from_ymd_opt(2026, 9, 2).unwrap(),
            );
            let groups = &delegate.groups;
            assert_eq!(groups.len(), 1);
            assert!(groups[0].label.is_empty());
            assert_eq!(groups[0].range, 0..2);
        }
    }

    #[test]
    fn appended_pages_deduplicate_and_rebuild_flat_indexes() {
        let one = session("one", local_ms(2026, 9, 2), false);
        let duplicate = one.clone();
        let two = session("two", local_ms(2026, 9, 1), false);
        let mut delegate = SessionsDelegate::new(vec![one], SortKey::Messages, false);

        delegate.append_sessions(vec![duplicate, two]);

        assert_eq!(delegate.sessions.len(), 2);
        assert_eq!(delegate.sessions[0].key, "one");
        assert_eq!(delegate.sessions[1].key, "two");
        assert_eq!(delegate.groups[0].range, 0..2);
        assert_eq!(delegate.index_path(1), Some(IndexPath::new(1)));
    }

    #[test]
    fn tree_delegate_expands_children_without_changing_root_pagination() {
        let temp = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(&temp.path().join("tree.db")).unwrap());
        let mut parent = session("grok:parent", local_ms(2026, 9, 1), false);
        parent.agent = AgentId::Grok;
        parent.file_path = "/tmp/grok/parent/updates.jsonl".into();
        let mut child = session("grok:child", local_ms(2026, 9, 2), false);
        child.agent = AgentId::Grok;
        child.file_path = "/tmp/grok/child/updates.jsonl".into();
        let mut other = session("grok:other", local_ms(2026, 8, 31), false);
        other.agent = AgentId::Grok;
        other.file_path = "/tmp/grok/other/updates.jsonl".into();
        store
            .write_meta_only(&[
                (parent.clone(), parent.updated_at),
                (child.clone(), child.updated_at),
                (other.clone(), other.updated_at),
            ])
            .unwrap();
        store
            .replace_parent_links(AgentId::Grok, &[(child.key.clone(), parent.key.clone())])
            .unwrap();
        let filter = SessionFilter {
            roots_only: true,
            sort: SortKey::Messages,
            limit: SESSION_PAGE_SIZE,
            ..Default::default()
        };
        let (roots, total) = store.list_sessions(&filter).unwrap();
        let mut delegate = SessionsDelegate::paged(roots, filter, total, store);

        assert_eq!(delegate.sessions.len(), 2);
        assert_eq!(delegate.rows.len(), 2);
        assert_eq!(delegate.rows[0].child_count, 1);
        assert_eq!(delegate.row_index(&other.key), Some(1));
        assert!(delegate.toggle(&parent.key));
        assert_eq!(
            delegate.sessions.len(),
            2,
            "children must not advance pagination"
        );
        assert_eq!(delegate.rows.len(), 3);
        assert_eq!(delegate.rows[0].meta.key, parent.key);
        assert_eq!(delegate.rows[1].meta.key, child.key);
        assert_eq!(delegate.rows[1].depth, 1);
        assert_eq!(delegate.index_path(1), Some(IndexPath::new(1)));
        assert_eq!(delegate.row_index(&other.key), Some(2));
        assert!(delegate.toggle(&parent.key));
        assert_eq!(delegate.rows.len(), 2);
        assert_eq!(delegate.row_index(&other.key), Some(1));
    }

    #[test]
    fn date_change_rebuilds_groups_and_stable_indexes() {
        let september_second = NaiveDate::from_ymd_opt(2026, 9, 2).unwrap();
        let september_third = NaiveDate::from_ymd_opt(2026, 9, 3).unwrap();
        let sessions = vec![
            session("recent", local_ms(2026, 9, 2), false),
            session("older", local_ms(2026, 9, 1), false),
        ];
        let mut delegate =
            SessionsDelegate::new_at(sessions, SortKey::Updated, false, september_second);

        assert_eq!(delegate.groups[0].label.as_ref(), "Today");
        assert!(delegate.rebuild_groups_at(september_third));
        assert_eq!(delegate.groups[0].label.as_ref(), "Yesterday");
        assert_eq!(delegate.groups[1].label.as_ref(), "Earlier this week");
        assert_eq!(delegate.index_path(1), Some(IndexPath::new(0).section(1)));
        assert!(!delegate.rebuild_groups_at(september_third));
    }

    #[test]
    fn refresh_query_comparison_ignores_only_page_extent() {
        let mut current = SessionFilter {
            limit: SESSION_PAGE_SIZE,
            ..Default::default()
        };
        let mut reloaded = current.clone();
        reloaded.limit = SESSION_PAGE_SIZE * 3;
        reloaded.offset = SESSION_PAGE_SIZE * 2;
        assert!(same_session_query(&current, &reloaded));

        current.favorite_only = true;
        assert!(!same_session_query(&current, &reloaded));
    }

    #[test]
    fn archived_search_hits_are_not_visible_in_active_list() {
        let mut archived = session("archived", local_ms(2026, 9, 2), false);
        archived.archived = true;
        let active_filter = SessionFilter::default();
        assert!(!session_matches_filter(&archived, &active_filter));

        let archived_filter = SessionFilter {
            include_archived: true,
            ..Default::default()
        };
        assert!(session_matches_filter(&archived, &archived_filter));
    }

    #[test]
    fn meaningless_git_branches_are_hidden() {
        assert_eq!(visible_git_branch(None), None);
        assert_eq!(visible_git_branch(Some("")), None);
        assert_eq!(visible_git_branch(Some(" HEAD ")), None);
        assert_eq!(visible_git_branch(Some("detached")), None);
        assert_eq!(visible_git_branch(Some("(detached HEAD)")), None);
        assert_eq!(
            visible_git_branch(Some("feature/session-detail")),
            Some("feature/session-detail")
        );
    }

    #[test]
    fn detail_load_reports_missing_adapter_and_parse_reason() {
        let meta = session("broken", local_ms(2026, 9, 2), false);

        let missing = load_visible_transcript(&[], &meta).expect_err("adapter should be missing");
        assert!(missing.contains("No Codex adapter"));

        let adapters: Vec<Box<dyn AgentAdapter>> = vec![Box::new(FailingAdapter)];
        let parse_error = load_visible_transcript(&adapters, &meta)
            .expect_err("fixture transcript should fail to parse");
        assert!(parse_error.contains("broken transcript fixture"));
    }

    #[test]
    fn thinking_and_tools_keep_independent_expansion_state() {
        let mut thinking = HashSet::new();
        let mut tools = HashSet::new();

        toggle_expanded_row(&mut thinking, 4);
        assert!(thinking.contains(&4));
        assert!(!tools.contains(&4));

        toggle_expanded_row(&mut tools, 4);
        assert!(thinking.contains(&4));
        assert!(tools.contains(&4));

        toggle_expanded_row(&mut thinking, 4);
        assert!(!thinking.contains(&4));
        assert!(tools.contains(&4));
    }

    #[test]
    fn tool_summaries_use_unicode_width_and_long_sections_are_marked() {
        let calls = vec![tool_call("Bash", "搜索中文项目路径")];
        let (name, argument) = tool_cluster_heading(&calls, 8);
        assert_eq!(name.as_ref(), "Bash");
        assert_eq!(
            argument.as_ref().map(|value| value.as_ref()),
            Some("搜索中…")
        );

        let (shown, truncated) = clip_tool_text("abcdef", 3);
        assert_eq!(shown, "abc…");
        assert!(truncated);

        let (shown, truncated) = clip_tool_text("abc", 3);
        assert_eq!(shown, "abc");
        assert!(!truncated);
    }

    #[test]
    fn tool_copy_source_preserves_edge_whitespace() {
        let original = "\n  result  \n";
        assert_eq!(non_blank_tool_text(original), Some(original));
        assert_eq!(non_blank_tool_text(" \n\t "), None);
    }
}

// ---------------- 搜索面板 delegate ----------------

pub struct SearchDelegate {
    pub hits: Vec<SearchHit>,
    pub degraded: bool,
    store: Arc<Store>,
    last_query: String,
}

impl ListDelegate for SearchDelegate {
    type Item = ListItem;

    fn items_count(&self, _section: usize, _cx: &App) -> usize {
        self.hits.len()
    }

    fn render_item(
        &mut self,
        ix: IndexPath,
        _window: &mut Window,
        cx: &mut Context<ListState<Self>>,
    ) -> Option<Self::Item> {
        let h = self.hits.get(ix.row)?;
        let theme = cx.theme();
        let timestamp = h.timestamp.unwrap_or(0);
        let hit_time: SharedString = smart_time(timestamp).into();
        let hit_time_tooltip: SharedString = abs_date(timestamp).into();
        let snippet = h
            .snippet
            .replace(HL_OPEN, "「")
            .replace(HL_CLOSE, "」")
            .replace('\n', " ");
        Some(
            // ListItem 无默认 margin,行块与内容区同宽(胶囊边对齐 Scope 行/
            // 输入行);块内文字缩进保持组件默认(px_3)+ 内容 px_2
            ListItem::new(ix.row).rounded(theme.radius).child(
                v_flex()
                    .w_full()
                    .px(SPACE_SM)
                    .py(SPACE_SM)
                    .gap(px(6.))
                    .child(
                        h_flex()
                            .gap(SPACE_SM)
                            .text_size(FONT_CAPTION)
                            .child(
                                img(h.session.agent.brand_icon(theme.mode.is_dark()))
                                    .size(px(15.))
                                    .flex_shrink_0(),
                            )
                            .child(
                                div()
                                    .min_w_0()
                                    .font_medium()
                                    .text_color(theme.foreground)
                                    .truncate()
                                    .child(h.session.title.clone()),
                            )
                            .child(div().flex_1())
                            .child(
                                div()
                                    .id(("search-hit-time", ix.row))
                                    .flex_shrink_0()
                                    .text_size(FONT_CAPTION)
                                    .text_color(theme.muted_foreground)
                                    .child(format!("{} · {}", h.session.project_name, hit_time))
                                    .tooltip(move |window, cx| {
                                        gpui_component::tooltip::Tooltip::new(
                                            hit_time_tooltip.clone(),
                                        )
                                        .build(window, cx)
                                    }),
                            ),
                    )
                    .child(
                        div()
                            .text_size(FONT_CAPTION)
                            .text_color(theme.muted_foreground)
                            .truncate()
                            .child(snippet),
                    ),
            ),
        )
    }

    fn set_selected_index(
        &mut self,
        _ix: Option<IndexPath>,
        _window: &mut Window,
        _cx: &mut Context<ListState<Self>>,
    ) {
    }

    fn perform_search(
        &mut self,
        query: &str,
        window: &mut Window,
        cx: &mut Context<ListState<Self>>,
    ) -> gpui::Task<()> {
        let q = query.to_string();
        self.last_query = q.clone();
        let store = self.store.clone();
        let bg = cx.background_spawn(async move {
            if q.trim().is_empty() {
                (Vec::new(), false)
            } else {
                store
                    .search(&q, &[], None, 60)
                    .unwrap_or((Vec::new(), false))
            }
        });
        cx.spawn_in(window, async move |this, cx| {
            let (hits, degraded) = bg.await;
            this.update(cx, |state, cx| {
                let d = state.delegate_mut();
                d.hits = hits;
                d.degraded = degraded;
                cx.notify();
            })
            .ok();
        })
    }

    // 查询为空时的引导页也走这里:搜索框已拆出 List 自管(searchable(false)),
    // ListState 不再有 query_input,render_initial 永不触发
    fn render_empty(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<ListState<Self>>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        if self.last_query.trim().is_empty() {
            return v_flex()
                .h(px(250.))
                .w_full()
                .justify_center()
                .child(empty_state(
                    "icons/search.svg",
                    px(48.),
                    px(22.),
                    "Search full conversation text",
                    "Matches natural language and code, like \"useEffect(\".",
                    cx,
                ));
        }
        v_flex()
            .h(px(250.))
            .w_full()
            .items_center()
            .justify_center()
            .gap(SPACE_MD)
            .text_color(theme.muted_foreground)
            .child(icon("icons/inbox.svg").with_size(px(24.)))
            .child(
                div()
                    .text_size(FONT_BODY)
                    .font_medium()
                    .child(format!("No results for \"{}\"", self.last_query)),
            )
            .child(
                div()
                    .text_size(FONT_CAPTION)
                    .child("Try a different or shorter query."),
            )
    }

    fn render_section_header(
        &mut self,
        _section: usize,
        _window: &mut Window,
        cx: &mut Context<ListState<Self>>,
    ) -> Option<impl IntoElement> {
        if !self.degraded {
            return None;
        }
        Some(
            div()
                .px(SPACE_SM)
                .pb(SPACE_XS)
                .text_size(FONT_LABEL)
                .text_color(cx.theme().muted_foreground)
                .child("Short query — using fallback search. Longer keywords are faster."),
        )
    }
}

// ---------------- 详情状态 ----------------

fn visible_git_branch(branch: Option<&str>) -> Option<&str> {
    let branch = branch?.trim();
    let normalized = branch.trim_matches(['(', ')']).trim();
    (!normalized.is_empty()
        && !normalized.eq_ignore_ascii_case("head")
        && !normalized.eq_ignore_ascii_case("detached")
        && !normalized.eq_ignore_ascii_case("detached head"))
    .then_some(branch)
}

fn load_visible_transcript(
    adapters: &[Box<dyn AgentAdapter>],
    meta: &SessionMeta,
) -> Result<Vec<TranscriptMessage>, String> {
    // adapter_for 按文件路径挑实例：自定义 location 的会话必须由拥有其根的
    // 实例解析；完全找不到对应 agent 时保留可读错误，不再静默变成空详情。
    let adapter = adapter_for(adapters, meta.agent, &meta.file_path).ok_or_else(|| {
        format!(
            "No {} adapter is available for this session path.",
            meta.agent.display_name()
        )
    })?;
    let session_ref = SessionFileRef::from_meta(meta);
    let transcript = adapter.parse_transcript(&session_ref).map_err(|error| {
        format!(
            "Failed to parse {} session data: {error:#}",
            meta.agent.display_name()
        )
    })?;

    Ok(transcript
        .mainline
        .into_iter()
        .filter(|message| {
            message.kind != MessageKind::Meta
                && (!message.text.trim().is_empty()
                    || !message.tool_calls.is_empty()
                    || message.thinking.is_some()
                    || !message.images.is_empty()
                    || message.kind == MessageKind::CompactSummary)
        })
        .collect())
}

fn toggle_expanded_row(rows: &mut HashSet<usize>, ix: usize) {
    if !rows.insert(ix) {
        rows.remove(&ix);
    }
}

struct DetailState {
    meta: SessionMeta,
    /// 过滤后的可见消息。Rc 让行渲染以引用计数克隆代替整条消息深拷贝
    transcript: Rc<Vec<TranscriptMessage>>,
    loading: bool,
    /// 详情解析的具体失败原因；None 表示加载中或成功。
    error: Option<SharedString>,
    /// 逐消息不等高列表(gpui 原生 ListState,惰性测量)
    msg_list: gpui::ListState,
    /// 展开的工具簇(按消息在 transcript 里的下标)
    expanded_tools: HashSet<usize>,
    /// 展开的 Thinking。与工具簇分开存，避免其中一个把另一个一起展开。
    expanded_thinking: HashSet<usize>,
    /// 搜索跳转目标(FTS seq,契约=消息 seq);解析完成后滚到该消息并保持高亮
    jump_seq: Option<i64>,
    /// 与 transcript 同下标；原始字节只保留在 Arc<Image> 内，避免双份内存。
    images: Vec<Vec<ImageSlot>>,
    /// 放大预览中的图片：(消息下标, 消息内图片下标)。
    zoom: Option<(usize, usize)>,
    /// 放大预览操作的短暂就地反馈。绑定具体图片，避免切换预览后把上一张的
    /// 成功状态带过来；generation 让较早的复位计时器不能清掉较新的反馈。
    image_action_feedback: [Option<ImageActionFeedback>; 2],
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ImageAction {
    Copy,
    Save,
}

impl ImageAction {
    const fn index(self) -> usize {
        match self {
            Self::Copy => 0,
            Self::Save => 1,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct ImageActionFeedback {
    target: (usize, usize),
    action: ImageAction,
    generation: u64,
}

#[derive(Clone)]
enum ImageSlot {
    Ready {
        image: Arc<gpui::Image>,
        dims: Option<(u32, u32)>,
        text_offset: usize,
    },
    Unsupported {
        media_type: SharedString,
        bytes: Arc<Vec<u8>>,
        text_offset: usize,
    },
    Omitted {
        text_offset: usize,
    },
}

impl ImageSlot {
    fn text_offset(&self) -> usize {
        match self {
            Self::Ready { text_offset, .. }
            | Self::Unsupported { text_offset, .. }
            | Self::Omitted { text_offset } => *text_offset,
        }
    }
}

fn take_image_slots(messages: &mut [TranscriptMessage]) -> Vec<Vec<ImageSlot>> {
    const TRANSCRIPT_IMAGE_BYTES: usize = 64 * 1024 * 1024;
    const TRANSCRIPT_DECODED_PIXELS: u64 = 96_000_000;
    let mut remaining_bytes = TRANSCRIPT_IMAGE_BYTES;
    let mut remaining_pixels = TRANSCRIPT_DECODED_PIXELS;

    messages
        .iter_mut()
        .map(|message| {
            std::mem::take(&mut message.images)
                .into_iter()
                .map(|attachment| {
                    let text_offset = attachment.text_offset.min(message.text.len());
                    if attachment.bytes.len() > remaining_bytes {
                        return ImageSlot::Omitted { text_offset };
                    }
                    remaining_bytes -= attachment.bytes.len();
                    match image_format_of(&attachment.media_type).and_then(|format| {
                        image_probe(&attachment.bytes, format).map(|probe| (format, probe))
                    }) {
                        Some((format, probe)) if probe.decoded_pixels <= remaining_pixels => {
                            remaining_pixels -= probe.decoded_pixels;
                            ImageSlot::Ready {
                                image: Arc::new(gpui::Image::from_bytes(format, attachment.bytes)),
                                dims: Some(probe.dims),
                                text_offset,
                            }
                        }
                        _ => ImageSlot::Unsupported {
                            media_type: attachment.media_type.into(),
                            bytes: Arc::new(attachment.bytes),
                            text_offset,
                        },
                    }
                })
                .collect()
        })
        .collect()
}

#[cfg(test)]
mod detail_selection_tests {
    use std::time::Duration;

    use super::image_probe;
    use gpui::{
        div, list, point, px, AppContext as _, Context, Element as _, InteractiveElement as _,
        IntoElement, ListAlignment, ListState, Modifiers, MouseButton, MouseMoveEvent,
        ParentElement as _, Render, Styled as _, TestAppContext, VisualTestContext, Window,
    };
    use gpui_component::{scroll::AutoScroll, text::TextView, Root, WindowExt as _};

    struct VirtualMessageList {
        list: ListState,
    }

    struct AutoScrollingVirtualMessageList {
        list: ListState,
        auto_scroll: AutoScroll,
    }

    impl Render for VirtualMessageList {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            list(self.list.clone(), |ix, _, _| {
                let text = if ix == 0 {
                    "First message"
                } else {
                    "Second message"
                };
                div()
                    .h(px(60.))
                    .w_full()
                    .child(TextView::markdown(("selection-message", ix), text).selectable(true))
                    .into_any()
            })
            .size_full()
        }
    }

    impl Render for AutoScrollingVirtualMessageList {
        #[allow(deprecated)]
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let list_state = self.list.clone();
            div()
                .size_full()
                .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, window, cx| {
                    let bounds = this.list.viewport_bounds();
                    let delta = (event.dragging() && window.has_text_selection(cx))
                        .then(|| AutoScroll::compute_delta(event.position.y, bounds))
                        .flatten();
                    this.auto_scroll.set(delta, cx, |delta, this, cx| {
                        this.list.scroll_by(delta);
                        cx.notify();
                    });
                }))
                .on_mouse_up(
                    MouseButton::Left,
                    cx.listener(|this, _, _, _| this.auto_scroll.stop()),
                )
                .child(
                    list(list_state, |ix, _, _| {
                        div()
                            .h(px(60.))
                            .w_full()
                            .child(
                                TextView::markdown(
                                    ("auto-scroll-selection-message", ix),
                                    format!("Message {ix}"),
                                )
                                .selectable(true),
                            )
                            .into_any()
                    })
                    .size_full(),
                )
        }
    }

    /// gpui-component fd3bc2b 的遮罩点击关闭是坏的,由 ui.rs 的 sentinel 补:经
    /// open_closable_dialog 打开的弹窗面板内点不关、面板上方/下方点一下即关;裸
    /// open_dialog 的(AlertDialog 一类没登记的)面板外点击不关
    #[gpui::test]
    fn dialog_closes_on_outside_click(cx: &mut TestAppContext) {
        use crate::ui::{open_closable_dialog, overlay_layers};
        use gpui_component::input::{Input, InputState};

        struct Host;
        impl Render for Host {
            fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
                div().size_full().children(overlay_layers(window, cx))
            }
        }
        cx.update(|cx| {
            gpui_component::init(cx);
            // 入场动画一帧到位,不用按真实时钟等它落位
            cx.set_reduce_motion(true);
        });
        let (_root, cx) = cx.add_window_view(|window, cx| {
            let host = cx.new(|_| Host);
            Root::new(host, window, cx)
        });
        let cx: &mut VisualTestContext = cx;
        let draw = |cx: &mut VisualTestContext| {
            cx.update(|w, cx| {
                let _ = w.draw(cx);
            });
            cx.run_until_parked();
        };
        let click = |cx: &mut VisualTestContext, position| {
            cx.simulate_click(position, Modifiers::default());
            draw(cx);
        };
        let size = cx.update(|w, _| w.viewport_size());
        // 面板居中、宽 400、顶边在视口高度 1/10 处
        let inside = point(size.width / 2., size.height / 10. + px(60.));
        // 面板上方(标题栏高度以内)与下方各取一点:上方曾被照抄上游的标题栏豁免吞掉
        let above = point(px(20.), px(20.));
        let below = point(px(20.), size.height - px(20.));

        // 真实表单里有个会拿焦点的输入框,照样放一个
        let input = cx.update(|window, cx| cx.new(|cx| InputState::new(window, cx)));
        cx.update(|window, cx| {
            open_closable_dialog(window, cx, move |dialog, _, _| {
                dialog
                    .title("Probe")
                    .w(px(400.))
                    .child(div().h(px(120.)).child(Input::new(&input)))
            });
        });
        draw(cx);
        click(cx, inside);
        click(cx, inside);
        assert!(
            cx.update(|w, cx| w.has_active_dialog(cx)),
            "click inside the panel must not close the dialog"
        );
        click(cx, above);
        assert!(
            !cx.update(|w, cx| w.has_active_dialog(cx)),
            "click above the panel must close the dialog"
        );

        cx.update(|window, cx| {
            open_closable_dialog(window, cx, |dialog, _, _| {
                dialog
                    .title("Probe")
                    .w(px(400.))
                    .child(div().h(px(120.)).child("body"))
            });
        });
        draw(cx);
        click(cx, below);
        assert!(
            !cx.update(|w, cx| w.has_active_dialog(cx)),
            "click below the panel must close the dialog"
        );

        // 裸 open_dialog(AlertDialog 一类没登记的):面板外点击不关
        cx.update(|window, cx| {
            window.open_dialog(cx, |dialog, _, _| {
                dialog
                    .title("Alert-like")
                    .w(px(400.))
                    .child(div().h(px(120.)).child("body"))
            });
        });
        draw(cx);
        click(cx, below);
        assert!(
            cx.update(|w, cx| w.has_active_dialog(cx)),
            "an unregistered dialog must survive outside clicks"
        );
        cx.update(|window, cx| window.close_dialog(cx));
    }

    #[gpui::test]
    #[allow(deprecated)]
    fn window_selection_spans_text_views_inside_gpui_list(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let (_, cx) = cx.add_window_view(|window, cx| {
            let view = cx.new(|_| VirtualMessageList {
                list: ListState::new(2, ListAlignment::Top, px(60.)),
            });
            Root::new(view, window, cx)
        });
        let cx: &mut VisualTestContext = cx;
        cx.run_until_parked();
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });

        cx.simulate_mouse_down(
            point(px(1.), px(15.)),
            MouseButton::Left,
            Modifiers::default(),
        );
        cx.simulate_mouse_move(
            point(px(300.), px(75.)),
            Some(MouseButton::Left),
            Modifiers::default(),
        );
        cx.simulate_mouse_up(
            point(px(300.), px(75.)),
            MouseButton::Left,
            Modifiers::default(),
        );
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });

        let selected = cx.update(|window, cx| window.selected_text(cx));
        assert!(selected.contains("First message"), "got {selected:?}");
        assert!(selected.contains("Second message"), "got {selected:?}");
    }

    #[gpui::test]
    #[allow(deprecated)]
    fn shift_click_extends_across_messages_inside_gpui_list(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let (_, cx) = cx.add_window_view(|window, cx| {
            let view = cx.new(|_| VirtualMessageList {
                list: ListState::new(2, ListAlignment::Top, px(60.)),
            });
            Root::new(view, window, cx)
        });
        let cx: &mut VisualTestContext = cx;
        cx.run_until_parked();
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });

        cx.simulate_mouse_down(
            point(px(1.), px(15.)),
            MouseButton::Left,
            Modifiers::default(),
        );
        cx.simulate_mouse_up(
            point(px(1.), px(15.)),
            MouseButton::Left,
            Modifiers::default(),
        );
        let shift = Modifiers {
            shift: true,
            ..Modifiers::default()
        };
        cx.simulate_mouse_down(point(px(300.), px(75.)), MouseButton::Left, shift);
        cx.simulate_mouse_up(point(px(300.), px(75.)), MouseButton::Left, shift);
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });

        let selected = cx.update(|window, cx| window.selected_text(cx));
        assert!(selected.contains("First message"), "got {selected:?}");
        assert!(selected.contains("Second message"), "got {selected:?}");
    }

    #[gpui::test]
    fn dragging_selection_at_viewport_edge_auto_scrolls_gpui_list(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let (root, cx) = cx.add_window_view(|window, cx| {
            let view = cx.new(|_| AutoScrollingVirtualMessageList {
                list: ListState::new(100, ListAlignment::Top, px(60.)),
                auto_scroll: AutoScroll::default(),
            });
            Root::new(view, window, cx)
        });
        let view = root.read_with(cx, |root, _| {
            root.view()
                .clone()
                .downcast::<AutoScrollingVirtualMessageList>()
                .unwrap()
        });
        let cx: &mut VisualTestContext = cx;
        cx.run_until_parked();
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        let bounds = view.read_with(cx, |view, _| view.list.viewport_bounds());
        let x = bounds.left() + px(10.);
        let anchor = point(x, bounds.top() + px(15.));

        cx.simulate_mouse_down(anchor, MouseButton::Left, Modifiers::default());
        cx.simulate_mouse_move(
            point(x, bounds.bottom() - px(20.)),
            Some(MouseButton::Left),
            Modifiers::default(),
        );
        cx.simulate_mouse_move(
            point(x, bounds.bottom() - px(1.)),
            Some(MouseButton::Left),
            Modifiers::default(),
        );
        assert!(view.read_with(cx, |view, _| view.auto_scroll.is_active()));

        cx.executor().advance_clock(Duration::from_millis(32));
        cx.run_until_parked();
        let scroll_top = view.read_with(cx, |view, _| view.list.logical_scroll_top());
        assert!(scroll_top.item_ix > 0 || scroll_top.offset_in_item > px(0.));

        cx.simulate_mouse_up(
            point(x, bounds.bottom() - px(1.)),
            MouseButton::Left,
            Modifiers::default(),
        );
        assert!(!view.read_with(cx, |view, _| view.auto_scroll.is_active()));
    }

    #[test]
    fn image_probe_accepts_small_matching_images_and_rejects_mime_mismatch() {
        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgba8(2, 3)
            .write_to(&mut png, image::ImageFormat::Png)
            .expect("png fixture");
        let bytes = png.into_inner();

        let probe = image_probe(&bytes, gpui::ImageFormat::Png).expect("safe png");
        assert_eq!(probe.dims, (2, 3));
        assert_eq!(probe.decoded_pixels, 6);
        assert!(image_probe(&bytes, gpui::ImageFormat::Jpeg).is_none());
    }
}

// ---------------- Workbench ----------------

pub struct Workbench {
    focus_handle: FocusHandle,
    store: Arc<Store>,
    /// 扫描/监听只含启用 location；管理面板另读 data_locations，停用行不消失。
    adapters: SharedAdapters,
    /// 与 adapters 同一次 roster 构造得到的全部 location 路径快照。
    data_locations: SharedLocations,

    selected_agent: Option<AgentId>,
    selected_project: Option<String>,
    favorite_only: bool,
    sort_key: SortKey,
    sort_ascending: bool,

    agent_counts: Vec<(AgentId, i64)>,
    projects: Vec<ProjectInfo>,
    agents_collapsed: bool,
    projects_collapsed: bool,
    starred_count: i64,
    /// 后台扫描的最新状态,启动时的自动扫描与用户主动重扫共用。整份留存而不是
    /// 摊成几个字段:刷新入口的守卫、侧栏状态文案、按钮 busy 态全部从它派生,
    /// 单一写入点就不会互相失步——把派生出的文案反过来当守卫,正是
    /// "扫描失败后刷新按钮再也点不动"的来源
    scan: ScanProgress,
    /// 用户主动发起的重扫。与 scan.scanning 正交：前者决定终态通知，后者是
    /// 所有自动/手动扫描共用的实际运行状态。
    refreshing: bool,
    /// location 变更撞上进行中的扫描时置位:那轮扫描持旧 roster,不补扫的话
    /// 新根不收录、被移根不出清,要等手动 ⌘R(2026-08-24 Codex review)。
    /// 终态事件到达后由 on_bg_event 消费,用新 roster 补一轮增量
    pending_rescan: bool,
    /// Settings 是单例窗口；句柄不保活，关闭后下次点击会检测失败并重建。
    settings_window: Option<AnyWindowHandle>,
    settings_page: SettingsPage,
    update_status: UpdateStatus,
    total_sessions: i64,

    list_state: Entity<ListState<SessionsDelegate>>,
    palette_list: Entity<ListState<SearchDelegate>>,
    /// ⌘K 搜索输入框(自管,不用 List 内置 searchable:清除钮可控)
    palette_input: Entity<InputState>,
    /// 进行中的搜索任务;新输入覆盖旧值即取消过期搜索
    _palette_search_task: Option<Task<()>>,
    /// 搜索命中不在首批会话时，按页补齐到命中项；新搜索覆盖旧任务。
    _list_seek_task: Option<Task<()>>,
    /// delegate 刷新或日期分组变化后，旧 IndexPath 已失效；下一帧按稳定 key
    /// 恢复选择，必要时继续分页找到目标。
    pending_list_selection: Option<String>,
    /// 每分钟触发一次轻量重绘，让跨午夜的 Today/Yesterday 分组自动换日。
    _calendar_task: Option<Task<()>>,

    /// 文本拖选靠近详情视口边缘时，持续推动外层消息列表。
    detail_selection_auto_scroll: AutoScroll,
    detail: Option<DetailState>,
    /// 图片操作反馈的全局递增代次。即使关闭后重新打开同一会话和图片，旧计时
    /// 器也不会误清新状态。
    image_action_feedback_generation: u64,

    /// Insights 页(侧栏底部入口):打开时替换中栏+右栏。与其他导航目的地
    /// 互斥(侧栏单选模型);数据在 open/refresh 时后台重算,Rc 免深拷贝
    insights_open: bool,
    insights: Option<Rc<InsightsData>>,
    insights_loading: bool,
    insights_range: InsightsRange,
    /// 三个榜单各自的度量档,按 UsageBoard 序数索引
    insights_metrics: [InsightsMetric; 3],
    /// 进行中的统计查询;新查询覆盖旧值即取消,扫描风暴下不堆积读锁竞争
    insights_task: Option<Task<()>>,

    scan_events: Arc<dyn ScanEvents>,
    watcher: Option<SessionWatcher>,
    /// 终端 id → 提取好的应用图标 png(后台 JXA 提取,详情页 Open In 用)
    terminal_icons: HashMap<String, PathBuf>,
    /// Open In 上次选择(split 按钮左段直开目标),None = 已装列表首个
    /// Open In 上次选择,按 agent 分记:agent 专属目标(两家 desktop、Kooky)
    /// 只对自家会话有意义,单一全局值会被后选的专属目标冲掉,回头另一家
    /// 会话就回退 Terminal(2026-09-01 用户反馈)。跨 agent 的次级回退走
    /// `last_terminal`
    preferred_terminal: HashMap<AgentId, terminal::TerminalApp>,
    /// 最近一次显式选择(不分 agent),per-agent 无记录时若仍在该会话的
    /// 可用列表内则用它——让"一直用 Ghostty"的人不用每家 agent 都选一遍
    last_terminal: Option<terminal::TerminalApp>,
    _subs: Vec<Subscription>,
}

/// Insights 分布图的维度(‹ › 循环切换)。数据三份都在 InsightsData 里,
/// 切换是纯视图状态,不触发重查
#[derive(Clone, Copy, PartialEq, Eq)]
enum InsightsRange {
    Hour,
    Weekday,
    Month,
}

impl InsightsRange {
    fn title(self) -> &'static str {
        match self {
            Self::Hour => "By hour",
            Self::Weekday => "By weekday",
            Self::Month => "By month",
        }
    }

    fn prev(self) -> Self {
        match self {
            Self::Hour => Self::Month,
            Self::Weekday => Self::Hour,
            Self::Month => Self::Weekday,
        }
    }

    fn next(self) -> Self {
        match self {
            Self::Hour => Self::Weekday,
            Self::Weekday => Self::Month,
            Self::Month => Self::Hour,
        }
    }
}

/// 榜单(Agents/Top projects/Models)的度量维度,‹ › 循环切换、每个榜单
/// 各自记忆档位。Tokens 只在组内有人报过用量时进入循环——tokens=0 语义
/// 是"不报"而非"用了 0"
#[derive(Clone, Copy, PartialEq, Eq)]
enum InsightsMetric {
    Sessions,
    Prompts,
    Tokens,
}

impl InsightsMetric {
    fn caption(self) -> &'static str {
        match self {
            Self::Sessions => "Sessions",
            Self::Prompts => "Prompts",
            Self::Tokens => "Tokens",
        }
    }

    fn value(self, u: &UsageTally) -> i64 {
        match self {
            Self::Sessions => u.sessions,
            Self::Prompts => u.prompts,
            Self::Tokens => u.tokens,
        }
    }

    fn display(self, u: &UsageTally) -> String {
        match self {
            Self::Tokens => fmt_tokens(Some(u.tokens)),
            _ => thousands(self.value(u)),
        }
    }
}

/// 三个榜单的静态规格。`Workbench::insights_metrics` 按其序数索引各自
/// 记忆档位——同一个渲染方法靠它读写自己的状态,不必外传 setter
#[derive(Clone, Copy)]
enum UsageBoard {
    Agents,
    Projects,
    Models,
}

impl UsageBoard {
    fn title(self) -> &'static str {
        match self {
            Self::Agents => "Agents",
            Self::Projects => "Projects",
            Self::Models => "Models",
        }
    }

    fn arrow_id(self) -> &'static str {
        match self {
            Self::Agents => "agents-arrow",
            Self::Projects => "projects-arrow",
            Self::Models => "models-arrow",
        }
    }

    /// Agents 全量列出(总共十四家);项目/模型长尾长,取前 6
    fn limit(self) -> usize {
        match self {
            Self::Agents => usize::MAX,
            _ => 6,
        }
    }

    fn name_w(self) -> Pixels {
        match self {
            Self::Models => px(176.),
            _ => px(128.),
        }
    }
}

/// Settings/Locations 的一行(= 一个数据源路径)。文本字段用 SharedString，
/// 让跨窗口快照与菜单闭包的 clone 只做引用计数。
#[derive(Clone)]
pub(crate) struct DataSourceRow {
    pub(crate) agent: AgentId,
    /// `~/…` 展示形态
    pub(crate) display: SharedString,
    /// 原始完整路径,交给 Finder
    pub(crate) raw: SharedString,
    /// 会话数,或路径不可用时的状态词
    pub(crate) tally: SharedString,
    /// 路径当前存在(行是否可点)
    pub(crate) exists: bool,
    /// Some(落库路径) = 自定义行(路径可能是本行根的上层目录);None = 预设行
    pub(crate) custom: Option<SharedString>,
    /// 预设行是否能只压制本路径，而不关闭该 agent 的整个默认实例。
    pub(crate) individual_default: bool,
    pub(crate) enabled: bool,
}

#[derive(Clone)]
pub(crate) struct LocationSettingsSnapshot {
    pub(crate) rows: Vec<DataSourceRow>,
    pub(crate) diverged: bool,
}

#[derive(Clone)]
pub(crate) struct DataSettingsSnapshot {
    pub(crate) display_path: SharedString,
    pub(crate) raw_path: SharedString,
    pub(crate) size_bytes: u64,
    pub(crate) session_count: i64,
}

/// location 表单的语义目标。预设行的“编辑”落库为**压制默认 + 记自定义**；
/// 真正的 Remove 只对自定义 location 出现，预设行由开关启停。
#[derive(Clone)]
enum FormTarget {
    Add,
    /// 编辑既有一行。custom=true:path 是落库的自定义路径(编辑单位);
    /// custom=false:path 是预填表单的路径；root 始终是被点行的真实数据根。
    Edit {
        agent: AgentId,
        path: SharedString,
        root: SharedString,
        custom: bool,
        individual_default: bool,
    },
}

impl Workbench {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let db_path = wake_core::db::default_db_path();
        // 库损坏时降级重建而不是崩掉:GUI 秒退什么都不告诉用户,他们也无从知道
        // 删掉那个文件就能自愈。重建也失败说明是目录权限/磁盘问题,那才没救——
        // 但至少要用系统弹窗把话说清楚再退。
        let (store, db_note) = match wake_core::db::open_or_rebuild(&db_path) {
            Ok(v) => v,
            Err(e) => {
                terminal::show_fatal_alert(&format!(
                    "Wake couldn't open or rebuild its index at {}. {e}",
                    db_path.display()
                ));
                std::process::exit(1);
            }
        };
        let store = Arc::new(store);
        let (adapters, data_locations) = Self::build_roster(&store);

        let list_state = cx.new(|cx| {
            ListState::new(
                SessionsDelegate::new(Vec::new(), SortKey::Updated, false),
                window,
                cx,
            )
            .searchable(false)
        });
        let palette_list = cx.new(|cx| {
            ListState::new(
                SearchDelegate {
                    hits: Vec::new(),
                    degraded: false,
                    store: store.clone(),
                    last_query: String::new(),
                },
                window,
                cx,
            )
            .searchable(false)
        });
        let palette_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder("Search everything \u{2014} prose or code")
        });

        // 后台:全量扫描线程 + 文件监听
        let (tx, mut rx) = futures::channel::mpsc::unbounded::<BgEvent>();
        let events: Arc<dyn ScanEvents> = Arc::new(ChannelEvents(tx));
        spawn_scan(adapters.clone(), store.clone(), events.clone(), false);
        let watcher = start_watcher(adapters.clone(), store.clone(), events.clone());
        let scan_events = events.clone();

        // 事件泵跟 Workbench entity 走，而不是跟主窗口走：Settings 会在主窗口
        // 关闭后继续持有 Workbench；若这里绑定 window，update_in 会失败并让
        // scan.scanning 永远收不到终态，后续 location 变更也无法再补扫。
        let main_window = window.window_handle();
        cx.spawn(async move |this, cx| {
            while let Some(ev) = rx.next().await {
                let note = match this.update(cx, |this, cx| this.on_bg_event(ev, cx)) {
                    Ok(note) => note,
                    Err(_) => break,
                };
                // 主窗口已关闭时状态仍正常收尾，只是不再尝试展示无处承载的
                // 完成通知。后台刷新不应顺手关闭用户正在使用的搜索面板。
                if let Some(note) = note {
                    main_window
                        .update(cx, |_, window, cx| window.push_notification(note, cx))
                        .ok();
                }
            }
        })
        .detach();

        let subs = vec![
            cx.subscribe_in(&list_state, window, Self::on_list_event),
            cx.subscribe_in(&palette_list, window, Self::on_palette_event),
            cx.subscribe_in(&palette_input, window, Self::on_palette_input_event),
        ];

        let mut this = Self {
            focus_handle: cx.focus_handle(),
            store,
            adapters,
            data_locations,
            selected_agent: None,
            selected_project: None,
            sort_key: SortKey::Updated,
            sort_ascending: false,
            favorite_only: false,
            agent_counts: Vec::new(),
            projects: Vec::new(),
            agents_collapsed: false,
            projects_collapsed: false,
            starred_count: 0,
            // 扫描线程已在上面 spawn,首个 Progress 事件到达前先占位为"扫描中",
            // 否则这个窗口内按 ⌘R 会起第二条并发全量扫描
            scan: ScanProgress {
                scanning: true,
                ..Default::default()
            },
            refreshing: false,
            pending_rescan: false,
            settings_window: None,
            settings_page: SettingsPage::General,
            update_status: UpdateStatus::Idle,
            total_sessions: 0,
            list_state,
            palette_list,
            palette_input,
            _palette_search_task: None,
            _list_seek_task: None,
            pending_list_selection: None,
            _calendar_task: None,
            detail_selection_auto_scroll: AutoScroll::default(),
            detail: None,
            image_action_feedback_generation: 0,
            insights_open: false,
            insights: None,
            insights_loading: false,
            insights_range: InsightsRange::Hour,
            insights_metrics: [InsightsMetric::Sessions; 3],
            insights_task: None,
            scan_events,
            watcher,
            terminal_icons: HashMap::new(),
            preferred_terminal: HashMap::new(),
            last_terminal: None,
            _subs: subs,
        };
        this.load_open_in_prefs();
        this.refresh(cx);
        this._calendar_task = Some(cx.spawn(async move |this, cx| loop {
            cx.background_executor()
                .timer(std::time::Duration::from_secs(60))
                .await;
            if this.update(cx, |_, cx| cx.notify()).is_err() {
                break;
            }
        }));

        // 索引重建过就告诉用户一声——收藏/置顶没了,总得让人知道为什么。
        // defer 到下一帧:此刻 Root 还没建好,notification 层挂不上
        if let Some(note) = db_note {
            cx.defer_in(window, move |_, window, cx| {
                window.push_notification(Notification::warning(note), cx);
            });
        }

        // 终端应用图标后台提取(首次数百 ms,之后命中缓存)
        let icons_task = cx.background_spawn(async {
            let dir = dirs::data_dir()
                .unwrap_or_default()
                .join("wake")
                .join("app-icons");
            terminal::ensure_app_icons(&dir)
        });
        cx.spawn_in(window, async move |this, cx| {
            let icons = icons_task.await;
            this.update(cx, |this, cx| {
                this.terminal_icons = icons;
                cx.notify();
            })
            .ok();
        })
        .detach();
        this
    }

    // ---------- 数据刷新 ----------

    fn current_filter(&self) -> SessionFilter {
        SessionFilter {
            agents: self.selected_agent.into_iter().collect(),
            project_path: self.selected_project.clone(),
            favorite_only: self.favorite_only,
            include_archived: false,
            roots_only: !self.favorite_only,
            title_query: None,
            sort: self.sort_key,
            ascending: self.sort_ascending,
            limit: SESSION_PAGE_SIZE,
            offset: 0,
        }
    }

    fn selected_list_key(&self, cx: &App) -> Option<String> {
        let list = self.list_state.read(cx);
        let delegate = list.delegate();
        list.selected_index()
            .and_then(|path| delegate.row_key(path))
            .map(str::to_string)
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        let mut filter = self.current_filter();
        let selected_key = self
            .pending_list_selection
            .clone()
            .or_else(|| self.selected_list_key(cx));
        let previously_loaded = {
            let list = self.list_state.read(cx);
            let delegate = list.delegate();
            delegate
                .pagination
                .as_ref()
                .filter(|page| same_session_query(&page.filter, &filter))
                .map(|_| delegate.sessions.len())
                .unwrap_or_default()
        };
        let expanded = self.list_state.read(cx).delegate().expanded.clone();
        if previously_loaded > SESSION_PAGE_SIZE as usize {
            filter.limit = i64::try_from(previously_loaded).unwrap_or(i64::MAX);
        }
        if let Ok((sessions, total)) = self.store.list_sessions(&filter) {
            self.total_sessions = total;
            let store = self.store.clone();
            self.list_state.update(cx, |state, cx| {
                let mut delegate = SessionsDelegate::paged(sessions, filter, total, store);
                delegate.restore_expanded(expanded);
                *state.delegate_mut() = delegate;
                cx.notify();
            });
            self.pending_list_selection = selected_key;
        }
        let mut counts: Vec<(AgentId, i64)> = self
            .store
            .agent_counts()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|(k, v)| AgentId::from_str(&k).map(|a| (a, v)))
            .collect();
        // 固定排序(AgentId 声明序):按会话数排会在平局时抖动
        // (HashMap 迭代无序),每次刷新侧栏顺序都会跳
        counts.sort_by_key(|&(a, _)| a);
        self.agent_counts = counts;
        self.projects = self.store.list_projects().unwrap_or_default();
        self.starred_count = self.store.starred_count().unwrap_or(0);
        // Insights 打开着就顺带重算:扫描增量/收藏变更等一切走 refresh 的
        // 路径都会让页面数据跟上,不设第二条失效通道
        self.reload_insights(cx);
        cx.notify();
    }

    fn refresh_session_group_date(&mut self, cx: &mut Context<Self>) {
        let today = Local::now().date_naive();
        let selected_key = self
            .pending_list_selection
            .clone()
            .or_else(|| self.selected_list_key(cx));
        let changed = self.list_state.update(cx, |state, cx| {
            let changed = state.delegate_mut().rebuild_groups_at(today);
            if changed {
                cx.notify();
            }
            changed
        });
        if changed {
            self.pending_list_selection = selected_key;
        }
    }

    fn restore_pending_list_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(key) = self.pending_list_selection.take() else {
            return;
        };
        if !self.select_list_key(&key, false, window, cx) {
            self.seek_list_key(&key, window, cx);
        }
    }

    /// 侧栏底部入口。再点一次(或点任意导航行)退回会话列表
    fn toggle_insights(&mut self, cx: &mut Context<Self>) {
        if self.insights_open {
            self.insights_open = false;
            cx.notify();
            return;
        }
        self.insights_open = true;
        // 互斥单选:Insights 是独立目的地,退出时落回 All Sessions
        self.selected_agent = None;
        self.selected_project = None;
        self.favorite_only = false;
        self.refresh(cx);
    }

    /// messages 全表分桶几十毫秒量级,走后台;已有数据时静默换新不闪 loading。
    /// 扫描进行中 Changed 事件每秒都来,有旧数据就先按住——终态 Progress
    /// 会补最后一次;新任务覆盖 insights_task 即取消旧查询,不堆积读锁竞争
    fn reload_insights(&mut self, cx: &mut Context<Self>) {
        if !self.insights_open {
            return;
        }
        if self.scan.scanning && self.insights.is_some() {
            return;
        }
        self.insights_loading = self.insights.is_none();
        let store = self.store.clone();
        let task =
            cx.background_spawn(async move { store.insights(chrono::Local::now().date_naive()) });
        self.insights_task = Some(cx.spawn(async move |this, cx| {
            let data = task.await;
            this.update(cx, |this, cx| {
                if let Ok(data) = data {
                    this.insights = Some(Rc::new(data));
                }
                this.insights_loading = false;
                cx.notify();
            })
            .ok();
        }));
    }

    /// 手动全量重扫(菜单 File → Refresh Sessions,⌘R)。刷新中忽略重复触发。
    /// 扫描在后台进行，侧栏持续显示进度；浏览、搜索和阅读不被阻断。
    fn refresh_sessions(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if self.scan.scanning {
            return;
        }
        self.scan = ScanProgress {
            scanning: true,
            ..Default::default()
        };
        self.refreshing = true;
        cx.notify();
        spawn_scan(
            self.adapters.clone(),
            self.store.clone(),
            self.scan_events.clone(),
            true,
        );
    }

    /// Settings/Locations 页的数据快照。路径与 active roster 来自同一次
    /// adapter 构造，因此停用项仍然可见，环境变量根也不会二次探测后错位。
    pub(crate) fn location_settings_snapshot(&self) -> LocationSettingsSnapshot {
        let mut flat = self.data_locations.as_ref().clone();
        // 自定义根紧随所属 agent 的默认根；同家内部保持 adapter 声明顺序。
        flat.sort_by_key(|location| location.agent);
        let prefixes: Vec<(String, String)> = flat
            .iter()
            .map(|location| {
                (
                    location.agent.as_str().to_string(),
                    location.path.to_string_lossy().to_string(),
                )
            })
            .collect();
        let counts = self
            .store
            .counts_by_path_prefix(&prefixes)
            .unwrap_or_else(|_| vec![0; prefixes.len()]);
        let (customs, removed, removed_roots) = self.store.location_overrides();
        let customs: Vec<(AgentId, SharedString)> = customs
            .into_iter()
            .map(|(agent, path)| {
                (
                    agent,
                    SharedString::from(path.to_string_lossy().to_string()),
                )
            })
            .collect();
        let rows = flat
            .iter()
            .zip(prefixes)
            .zip(counts)
            .map(|((location, (_, raw)), count)| {
                let exists = location.path.exists();
                DataSourceRow {
                    agent: location.agent,
                    display: tilde_path(&raw).into(),
                    raw: raw.clone().into(),
                    tally: if exists {
                        match count {
                            1 => "1 session".into(),
                            n => format!("{n} sessions").into(),
                        }
                    } else {
                        "Folder not found".into()
                    },
                    exists,
                    custom: custom_owner(&customs, location.agent, &raw).cloned(),
                    individual_default: location.individually_removable,
                    enabled: location.enabled,
                }
            })
            .collect();
        let diverged = !customs.is_empty()
            || !removed.is_empty()
            || !removed_roots.is_empty()
            || self.data_locations.iter().any(|location| !location.enabled);
        LocationSettingsSnapshot { rows, diverged }
    }

    pub(crate) fn data_settings_snapshot(&self) -> DataSettingsSnapshot {
        let path = wake_core::db::default_db_path();
        let raw = path.to_string_lossy().to_string();
        let size_bytes = ["", "-wal", "-shm"]
            .iter()
            .filter_map(|suffix| std::fs::metadata(format!("{raw}{suffix}")).ok())
            .map(|metadata| metadata.len())
            .sum();
        DataSettingsSnapshot {
            display_path: tilde_path(&raw).into(),
            raw_path: raw.into(),
            size_bytes,
            session_count: self.agent_counts.iter().map(|(_, count)| count).sum(),
        }
    }

    pub(crate) fn settings_page(&self) -> SettingsPage {
        self.settings_page
    }

    pub(crate) fn update_status(&self) -> &UpdateStatus {
        &self.update_status
    }

    pub(crate) fn select_settings_page(&mut self, page: SettingsPage, cx: &mut Context<Self>) {
        self.settings_page = page;
        cx.notify();
    }

    pub(crate) fn open_about(&mut self, cx: &mut Context<Self>) {
        self.settings_page = SettingsPage::About;
        cx.notify();
        self.open_settings(cx);
    }

    pub(crate) fn open_updates(&mut self, cx: &mut Context<Self>) {
        self.settings_page = SettingsPage::Updates;
        cx.notify();
        self.open_settings(cx);
        self.check_for_updates(cx);
    }

    pub(crate) fn check_for_updates(&mut self, cx: &mut Context<Self>) {
        if matches!(self.update_status, UpdateStatus::Checking) {
            return;
        }
        self.update_status = UpdateStatus::Checking;
        cx.notify();

        // reqwest::blocking 会自行创建 Tokio runtime；若直接放进 GPUI 的异步
        // executor，runtime 嵌套会 panic，界面便会永远停在 Checking。让阻塞
        // 请求待在独立系统线程里，再用 oneshot 把结果送回 UI executor。
        let (sender, receiver) = futures::channel::oneshot::channel();
        std::thread::spawn(|| {
            let _ = sender.send(update::check_latest_release(env!("CARGO_PKG_VERSION")));
        });
        cx.spawn(async move |this, cx| {
            let status = match receiver.await {
                Ok(Ok(info)) if info.update_available => UpdateStatus::Available {
                    latest: info.latest_version.to_string(),
                },
                Ok(Ok(info)) => UpdateStatus::UpToDate {
                    latest: info.latest_version.to_string(),
                },
                Ok(Err(error)) => {
                    eprintln!("update check failed: {error:#}");
                    UpdateStatus::Failed
                }
                Err(_) => UpdateStatus::Failed,
            };
            this.update(cx, |this, cx| {
                this.update_status = status;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// 打开单例 Settings 窗口。开窗必须 defer 到当前 Workbench update 退出
    /// 之后：Settings 首帧会读取 location 快照，同步开窗会反读仍被独占借用
    /// 的 Workbench，触发 GPUI double lease。
    pub(crate) fn open_settings(&mut self, cx: &mut Context<Self>) {
        let workbench = cx.entity();
        cx.defer(move |cx| Self::show_settings_window(workbench, cx));
    }

    fn show_settings_window(workbench: Entity<Self>, cx: &mut App) {
        // 先把 Copy 句柄取出，确保 read lease 在后续 update 前结束。
        let existing = workbench.read(cx).settings_window;
        if let Some(handle) = existing {
            if handle
                .update(cx, |_, window, _| window.activate_window())
                .is_ok()
            {
                cx.activate(true);
                return;
            }
            workbench.update(cx, |this, _| this.settings_window = None);
        }
        let (bounds, display_id) =
            crate::main_window::centered_over_main(size(px(820.), px(600.)), cx);
        let titlebar = if cfg!(target_os = "macos") {
            TitlebarOptions {
                title: None,
                appears_transparent: true,
                traffic_light_position: Some(point(px(20.), px(11.))),
            }
        } else {
            TitlebarOptions {
                title: Some("Wake Settings".into()),
                appears_transparent: false,
                traffic_light_position: None,
            }
        };
        let settings_workbench = workbench.clone();
        match cx.open_window(
            WindowOptions {
                titlebar: Some(titlebar),
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_min_size: Some(size(px(720.), px(520.))),
                display_id,
                app_id: Some("wake-settings".into()),
                window_decorations: Some(WindowDecorations::Client),
                ..Default::default()
            },
            move |window, cx| {
                window
                    .observe_window_appearance(|window, cx| {
                        crate::theme::sync_appearance(Some(window), cx);
                    })
                    .detach();
                crate::theme::sync_appearance(Some(window), cx);
                let settings = cx.new(|cx| SettingsView::new(settings_workbench, window, cx));
                window.focus(&settings.read(cx).focus_handle(cx), cx);
                cx.new(|cx| Root::new(settings, window, cx))
            },
        ) {
            Ok(handle) => {
                workbench.update(cx, |this, _| this.settings_window = Some(handle.into()));
                cx.activate(true);
            }
            Err(error) => eprintln!("failed to open Wake settings: {error}"),
        }
    }

    /// store 的 location 配置 → active roster + 全量路径快照。new 与
    /// rebuild_roster 共用；解析与组装都在 wake-core，与 scan CLI 同一条路。
    fn build_roster(store: &Arc<Store>) -> (SharedAdapters, SharedLocations) {
        let roster = create_adapter_roster_for(store);
        (Arc::new(roster.active), Arc::new(roster.locations))
    }

    /// location 配置变更后的唯一 roster 换代点:同一处换 Arc + 重启 watcher,
    /// 新旧两份实例不共存(不变量 8 的运行时补充:"单实例"指任一时刻只有一份
    /// 在服务,换代必须整体换、所有消费方跟随新 Arc)
    fn rebuild_roster(&mut self, cx: &mut Context<Self>) {
        // 先撤旧 watcher 并等它退出(SessionWatcher::Drop 内 join):旧线程持
        // 旧 roster,不等收尾,已移除根的会话可能在补扫后被写回复活
        self.watcher = None;
        (self.adapters, self.data_locations) = Self::build_roster(&self.store);
        self.watcher = start_watcher(
            self.adapters.clone(),
            self.store.clone(),
            self.scan_events.clone(),
        );
        cx.notify();
    }

    /// location 表单(添加/编辑共用一套 UI,2026-08-24 定稿):agent 下拉 +
    /// 路径输入框(可手输,~ 展开)+ 目录选择按钮;Cancel/Save 只在有改动时
    /// 出现。表单作为 Settings 窗口的模态层，esc/取消回到 Locations 页。
    pub(crate) fn open_add_location_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_location_form(FormTarget::Add, window, cx);
    }

    pub(crate) fn open_edit_location_form(
        &mut self,
        row: DataSourceRow,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // location 的配置单位是目录；SQLite 型 adapter 的展示根可能是文件，
        // 编辑时预填父目录，避免保存成 <db>/<db>。
        let path = row.custom.clone().unwrap_or_else(|| {
            let raw = row.raw.as_ref();
            if std::path::Path::new(raw).is_file() {
                std::path::Path::new(raw)
                    .parent()
                    .map(|path| SharedString::from(path.to_string_lossy().to_string()))
                    .unwrap_or_else(|| row.raw.clone())
            } else {
                row.raw.clone()
            }
        });
        self.open_location_form(
            FormTarget::Edit {
                agent: row.agent,
                path,
                root: row.raw,
                custom: row.custom.is_some(),
                individual_default: row.individual_default,
            },
            window,
            cx,
        );
    }

    fn open_location_form(
        &mut self,
        target: FormTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (title, ok_label, init_agent, init_path): (
            &'static str,
            &'static str,
            AgentId,
            SharedString,
        ) = match &target {
            FormTarget::Add => ("Add location", "Add", AgentId::ClaudeCode, "".into()),
            FormTarget::Edit { agent, path, .. } => ("Edit location", "Save", *agent, path.clone()),
        };
        // 占位符须与校验规则(Path::is_absolute)同形:Windows 上没有盘符
        // 的 `/absolute/...` 并不算绝对路径,照着占位符敲会被拒
        let placeholder = if cfg!(target_os = "windows") {
            r"C:\absolute\folder\path"
        } else {
            "/absolute/folder/path"
        };
        let path_input = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder));
        if !init_path.is_empty() {
            let v = init_path.clone();
            path_input.update(cx, |st, cx| st.set_value(v, window, cx));
        }
        // 编辑态动作行的 Finder 只挂真实存在的路径——目录或 SQLite 库文件都算
        // (open_in_finder 对文件走 reveal 选中;用 is_dir 会把三家库文件行的
        // Finder 恒隐藏,2026-08-24 Codex review)。fs 探测一次,不进每帧 builder
        let edit_exists = match &target {
            FormTarget::Add => None,
            FormTarget::Edit { path, .. } => Some(std::path::Path::new(path.as_ref()).exists()),
        };
        // 表单状态放 Rc<Cell>/entity 而非宿主字段:builder 每帧重跑,闭包内
        // read 宿主 entity 必 double-lease panic(与 refresh 进度弹窗同一约束)
        let selected: Rc<Cell<AgentId>> = Rc::new(Cell::new(init_agent));
        let dirty_init_agent = init_agent;
        let dirty_init_path = init_path;
        let entity = cx.entity();
        let title: SharedString = title.into();
        let ok_label: SharedString = ok_label.into();
        open_closable_dialog(window, cx, move |dialog, _window, cx| {
            let theme = cx.theme();
            let dark = theme.mode.is_dark();
            // 内容轴缩进 = small 按钮的水平内边距:字段行/标题/footer 以它为轴,
            // 动作行的胶囊钮**不缩进**——可见内容(内边距之后)恰好落轴,hover
            // 胶囊完整留在内容盒内。负 margin 会溢出被裁(locations 面板同一教训)
            let field_inset = BUTTON_SM_PX;
            let sel = selected.get();
            // 脏状态:与初始 (agent, path) 有差且路径非空才亮出 Cancel/Save
            // ——没改动时无可保存,收手走 esc/关闭即可(2026-08-24 用户定稿)。
            // Rope 直接与 &str 比较 + chars 扫描:builder 每帧跑,禁 to_string
            let dirty = {
                let text = path_input.read(cx).text();
                (sel != dirty_init_agent || *text != dirty_init_path.as_ref())
                    && text.chars().any(|c| !c.is_whitespace())
            };
            let sel_cell = selected.clone();
            let browse_entity = entity.clone();
            let browse_input = path_input.clone();
            let ok_entity = entity.clone();
            let ok_input = path_input.clone();
            let ok_sel = selected.clone();
            let ok_target = target.clone();
            let action_target = target.clone();
            let dialog = dialog
                .title(
                    div()
                        .pl(field_inset)
                        .text_size(FONT_HEADING)
                        .font_semibold()
                        .child(title.clone()),
                )
                .w(px(500.))
                .button_props(
                    gpui_component::dialog::DialogButtonProps::default().ok_text(ok_label.clone()),
                )
                .child(
                    v_flex()
                        .gap(SPACE_MD)
                        .child(
                            h_flex()
                                .px(field_inset)
                                .gap(SPACE_SM)
                                .items_center()
                                .child(
                                    div()
                                        .w(FORM_LABEL_W)
                                        .flex_shrink_0()
                                        .text_size(FONT_CAPTION)
                                        .text_color(theme.muted_foreground)
                                        .child("Agent"),
                                )
                                .child(
                                    // Button 走 ParentElement 自组内容:品牌图标是
                                    // PNG(img),进不了 .icon()(那只收单色 SVG),
                                    // 图标+名字+箭头必须同住按钮内(2026-08-24 反馈)
                                    Button::new("loc-agent")
                                        .outline()
                                        .rounded(RADIUS_BUTTON)
                                        .child(
                                            h_flex()
                                                .gap(SPACE_SM)
                                                .items_center()
                                                .child(
                                                    img(sel.brand_icon(dark))
                                                        .size(px(14.))
                                                        .flex_shrink_0(),
                                                )
                                                .child(
                                                    div()
                                                        .text_size(FONT_CAPTION)
                                                        .child(sel.display_name()),
                                                )
                                                .child(
                                                    icon("icons/chevron-down.svg")
                                                        .with_size(px(12.))
                                                        .text_color(theme.muted_foreground),
                                                ),
                                        )
                                        .dropdown_menu(move |menu, _, _| {
                                            let mut menu = menu.min_w(px(200.));
                                            for a in AgentId::ALL {
                                                let cell = sel_cell.clone();
                                                // element 变体:菜单项带品牌 PNG
                                                //(纯文本项的 icon 只收单色 SVG)
                                                menu = menu.item(
                                                    PopupMenuItem::element(move |_, _| {
                                                        h_flex()
                                                            .gap(SPACE_SM)
                                                            .items_center()
                                                            .child(
                                                                img(a.brand_icon(dark))
                                                                    .size(px(14.))
                                                                    .flex_shrink_0(),
                                                            )
                                                            .child(a.display_name())
                                                    })
                                                    .checked(sel == a)
                                                    .on_click(move |_, window, _| {
                                                        cell.set(a);
                                                        window.refresh();
                                                    }),
                                                );
                                            }
                                            menu
                                        }),
                                ),
                        )
                        .child(
                            h_flex()
                                .px(field_inset)
                                .gap(SPACE_SM)
                                .items_center()
                                .child(
                                    div()
                                        .w(FORM_LABEL_W)
                                        .flex_shrink_0()
                                        .text_size(FONT_CAPTION)
                                        .text_color(theme.muted_foreground)
                                        .child("Folder"),
                                )
                                .child(div().flex_1().min_w_0().child(Input::new(&browse_input)))
                                .child(
                                    Button::new("loc-browse")
                                        .outline()
                                        .rounded(RADIUS_BUTTON)
                                        .icon(icon("icons/folder.svg").with_size(px(13.)))
                                        .tooltip("Choose a folder")
                                        .on_click({
                                            let entity = browse_entity.clone();
                                            let input = browse_input.clone();
                                            move |_, window, cx| {
                                                let input = input.clone();
                                                entity.update(cx, |this, cx| {
                                                    this.browse_for_location(input, window, cx)
                                                });
                                            }
                                        }),
                                ),
                        )
                        .when_some(edit_exists, |el, exists| {
                            let FormTarget::Edit {
                                agent,
                                path,
                                root: _,
                                custom,
                                individual_default: _,
                            } = action_target.clone()
                            else {
                                unreachable!("edit_exists 仅在 Edit 目标下为 Some")
                            };
                            let remove_entity = entity.clone();
                            el.when(custom || exists, |el| {
                                el.child(
                                    // 动作行遵循破坏性靠左惯例:Remove 靠左、Show in
                                    // Finder 靠右。Remove 只属于真正可删除的自定义
                                    // location；内置 location 由行内开关停用，不删除。
                                    // 两钮手排:内边距 = 轴缩进,Remove
                                    // 左侧再减 1.5 补 lucide 字形内白(24 视框留 3
                                    // 单位),字形左缘正落标签轴;右钮文字右缘正落
                                    // 浏览钮右缘。全正值内边距,胶囊完整在内容盒里
                                    h_flex()
                                        .pt(SPACE_XS)
                                        .items_center()
                                        .justify_between()
                                        .when(custom, |el| {
                                            el.child(
                                                h_flex()
                                                    .id("loc-remove")
                                                    .h(BUTTON_SM_H)
                                                    .pl(BUTTON_SM_PX - px(1.5))
                                                    .pr(BUTTON_SM_PX)
                                                    .rounded(RADIUS_BUTTON)
                                                    .items_center()
                                                    .gap(px(6.))
                                                    .cursor_pointer()
                                                    .text_size(FONT_BODY)
                                                    .text_color(theme.danger)
                                                    .hover(|s| s.bg(theme.danger.opacity(0.1)))
                                                    .active(|s| s.bg(theme.danger.opacity(0.16)))
                                                    .on_click({
                                                        let remove_entity = remove_entity.clone();
                                                        let stored = path.clone();
                                                        move |_, window, cx| {
                                                            // 整栈收场(表单+过期面板);
                                                            // delete 内会重开新快照面板
                                                            window.close_all_dialogs(cx);
                                                            let stored = stored.clone();
                                                            remove_entity.update(cx, |this, cx| {
                                                                this.delete_location(
                                                                    agent, stored, window, cx,
                                                                )
                                                            });
                                                        }
                                                    })
                                                    .child(
                                                        icon("icons/trash-2.svg")
                                                            .with_size(px(13.))
                                                            .flex_shrink_0(),
                                                    )
                                                    .child("Remove"),
                                            )
                                        })
                                        .when(exists, |el| {
                                            el.child(
                                                h_flex()
                                                    .id("loc-reveal")
                                                    .h(BUTTON_SM_H)
                                                    .px(BUTTON_SM_PX)
                                                    .rounded(RADIUS_BUTTON)
                                                    .items_center()
                                                    .gap(px(6.))
                                                    .cursor_pointer()
                                                    .text_size(FONT_BODY)
                                                    .text_color(theme.foreground)
                                                    .hover(|s| s.bg(theme.secondary_hover))
                                                    .active(|s| s.bg(theme.secondary_active))
                                                    .on_click(move |_, _, _| {
                                                        terminal::open_in_file_manager(&path)
                                                    })
                                                    .child(
                                                        icon("icons/folder.svg")
                                                            .with_size(px(13.))
                                                            .flex_shrink_0()
                                                            .text_color(theme.muted_foreground),
                                                    )
                                                    .child(SHOW_IN_FM),
                                            )
                                        }),
                                )
                            })
                        }),
                )
                .on_ok(move |_, window, cx| {
                    let path_text = ok_input.read(cx).text().to_string();
                    let agent = ok_sel.get();
                    ok_entity.update(cx, |this, cx| {
                        this.commit_location_form(ok_target.clone(), agent, path_text, window, cx)
                    })
                });
            if dirty {
                dialog.footer(
                    gpui_component::dialog::DialogFooter::new()
                        .w_full()
                        .px(field_inset)
                        .child(
                            gpui_component::dialog::DialogClose::new()
                                .child(Button::new("location-cancel").label("Cancel").outline()),
                        )
                        .child(
                            gpui_component::dialog::DialogAction::new().child(
                                Button::new("location-save")
                                    .label(ok_label.clone())
                                    .primary(),
                            ),
                        ),
                )
            } else {
                dialog
            }
        });
    }

    /// 表单的目录选择按钮:系统选择器,选中即回填输入框(取消无事发生)
    fn browse_for_location(
        &mut self,
        input: Entity<InputState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Choose".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let dir = match rx.await {
                Ok(Ok(Some(paths))) if !paths.is_empty() => paths.into_iter().next().unwrap(),
                _ => return,
            };
            let text = dir.to_string_lossy().to_string();
            this.update_in(cx, |_, window, cx| {
                input.update(cx, |st, cx| st.set_value(text, window, cx));
            })
            .ok();
        })
        .detach();
    }

    /// 表单落库。返回值交给 on_ok:false = 表单留着(校验没过,或已手工收场)。
    /// 纯路径管理:不校验目录内容(2026-08-24 用户定稿),只拒空/相对路径与
    /// 同家重叠;预设行的编辑落库为"压默认 + 记自定义"
    fn commit_location_form(
        &mut self,
        target: FormTarget,
        agent_new: AgentId,
        raw_text: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let expanded = expand_tilde(raw_text.trim());
        // Windows 上把手输的 '/' 折成 '\':`~/.claude` 展开后是
        // `C:\Users\me/.claude` 这种混分隔符形态,而 path_owns 的重叠判定是
        // 字节精确比较、explorer 只认反斜杠——不在入口归一,同一目录就会以
        // 两种拼写各注册一份(POSIX 不动:'\' 在那边是合法文件名字符)
        let expanded = if cfg!(target_os = "windows") {
            expanded.replace('/', "\\")
        } else {
            expanded
        };
        // 绝对性先判、只判一次:is_absolute 三端同判据(starts_with('/') 会把
        // 所有 Windows 盘符路径误拒),空串它也判 false,无需另设 is_empty 关。
        // **必须判在剪尾之前**:`//` 剪完是空串,若拿剪后的结果去判就会退回
        // 未剪形态放行,而旧版是拒的(2026-08-25 review)
        if !std::path::Path::new(&expanded).is_absolute() {
            window.push_notification(Notification::warning("Enter an absolute folder path"), cx);
            return false;
        }
        // 尾分隔符归一(展示与重叠判定都吃这份);裸根("/"、"C:\")剪完会
        // 失去绝对性,原样保留
        let trimmed = expanded.trim_end_matches(std::path::is_separator);
        let path = if std::path::Path::new(trimmed).is_absolute() {
            trimmed.to_string()
        } else {
            expanded.clone()
        };
        // 各家归一化(codex:直选 sessions 树/平铺 archived 上提到家层,侧档
        // 找回)。静态分派,不依赖该家实例是否还在 roster(默认被移除时也要
        // 生效);归一化后再做无改动/重叠判定,选中默认根数据子目录会正确判"已覆盖"
        let path =
            wake_core::adapters::normalize_custom_root(agent_new, std::path::PathBuf::from(&path))
                .to_string_lossy()
                .to_string();
        // 没改就没事:直接让机制关表单,面板未过期。旧目标也要**同规归一化**
        // 再比——默认 Codex 的 sessions/archived 行原路径归一化后即 home,不归一
        // 化就比,单按 Enter 会被误判成编辑、静默把默认改成"压默认+记自定义"
        //(2026-08-24 Codex review)
        let unchanged = match &target {
            FormTarget::Add => false,
            FormTarget::Edit { agent, path: p, .. } => {
                *agent == agent_new
                    && wake_core::adapters::normalize_custom_root(
                        *agent,
                        std::path::PathBuf::from(p.as_ref()),
                    )
                    .to_string_lossy()
                        == path
            }
        };
        if unchanged {
            return true;
        }
        // 同家重叠检查,排除被编辑单元自身派生的根(自定义单元 = 其落库路径
        // 之下的根;预设单元 = 不属于任何该家自定义的根)。嵌进**别家**树里
        // 是合法场景(env 根的先例),同家嵌套才是重复读取
        let customs: Vec<(AgentId, SharedString)> = self
            .store
            .location_overrides()
            .0
            .into_iter()
            .map(|(a, p)| (a, SharedString::from(p.to_string_lossy().to_string())))
            .collect();
        let covered = self
            .data_locations
            .iter()
            .filter(|location| location.agent == agent_new)
            .any(|location| {
                let rs = location.path.to_string_lossy().to_string();
                let excluded = match &target {
                    FormTarget::Add => false,
                    FormTarget::Edit {
                        agent,
                        path: unit,
                        custom: true,
                        ..
                    } => *agent == agent_new && path_owns(unit.as_ref(), &rs),
                    FormTarget::Edit {
                        agent,
                        root,
                        custom: false,
                        individual_default: true,
                        ..
                    } => *agent == agent_new && rs == root.as_ref(),
                    FormTarget::Edit {
                        agent,
                        custom: false,
                        ..
                    } => *agent == agent_new && custom_owner(&customs, agent_new, &rs).is_none(),
                };
                !excluded && (path_owns(&path, &rs) || path_owns(&rs, &path))
            });
        if covered {
            window.push_notification(
                Notification::info("This folder is already in Wake's session locations"),
                cx,
            );
            return false;
        }
        let res = match &target {
            FormTarget::Add => self.store.add_custom_root(agent_new.as_str(), &path),
            // 全形态单事务(含换 agent 的编辑):半程失败不得把配置改成半生效
            //(Codex review P2)
            FormTarget::Edit {
                agent,
                path: old,
                root,
                custom,
                individual_default,
            } => self.store.replace_location(
                agent.as_str(),
                custom.then(|| old.as_ref()),
                (!custom && *individual_default).then(|| root.as_ref()),
                root.as_ref(),
                agent_new.as_str(),
                &path,
            ),
        };
        if let Err(e) = res {
            window.push_notification(Notification::error(format!("Save failed: {e}")), cx);
            return false;
        }
        // Settings 页本身是动态快照，只需收起表单；Workbench notify 会让
        // SettingsView 观察者刷新列表。
        window.close_all_dialogs(cx);
        let note = match &target {
            FormTarget::Add => "Location added",
            FormTarget::Edit { .. } => "Location updated",
        };
        self.apply_location_change(Ok(()), "", Notification::success(note), window, cx);
        false
    }

    /// 真正删除一个自定义 location。内置 location 不提供 Remove，只能用
    /// 行内开关暂时停用；磁盘文件始终不动。
    pub(crate) fn delete_location(
        &mut self,
        agent: AgentId,
        stored: SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let res = self
            .store
            .remove_custom_root(agent.as_str(), stored.as_ref());
        self.apply_location_change(
            res,
            "Remove failed",
            Notification::info("Location removed"),
            window,
            cx,
        );
    }

    /// Restore defaults:清空全部偏离（自定义、被移除的预设与停用状态），
    /// 回到全部启用的内置默认 location。
    pub(crate) fn restore_default_locations(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let res = self.store.clear_location_overrides();
        self.apply_location_change(
            res,
            "Restore failed",
            Notification::info("Locations restored to defaults"),
            window,
            cx,
        );
    }

    /// Session locations 行内开关。状态先落库，再整体换 active roster 与 watcher。
    pub(crate) fn set_location_enabled(
        &mut self,
        agent: AgentId,
        path: SharedString,
        enabled: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match self
            .store
            .set_location_enabled(agent.as_str(), path.as_ref(), enabled)
        {
            Err(e) => window.push_notification(
                Notification::error(format!("Couldn't update location: {e}")),
                cx,
            ),
            Ok(()) => {
                self.rebuild_roster(cx);
                self.kick_incremental_scan(cx);
                window.push_notification(
                    Notification::info(if enabled {
                        "Location enabled"
                    } else {
                        "Location disabled"
                    }),
                    cx,
                );
                window.refresh();
            }
        }
    }

    /// location 变更的统一收尾(删/恢复/表单提交成功共用)。Settings 页
    /// 观察 Workbench 的 notify 并重读快照，不需要关闭/重开管理面板。
    fn apply_location_change(
        &mut self,
        res: anyhow::Result<()>,
        err_prefix: &'static str,
        ok_note: Notification,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match res {
            Err(e) => {
                window.push_notification(Notification::error(format!("{err_prefix}: {e}")), cx);
            }
            Ok(()) => {
                self.rebuild_roster(cx);
                self.kick_incremental_scan(cx);
                window.push_notification(ok_note, cx);
            }
        }
    }

    /// roster 换代后补一轮增量:新根收录、被移根出清。撞上进行中的扫描
    /// 撞上进行中的扫描则排队，由终态事件补扫。
    fn kick_incremental_scan(&mut self, cx: &mut Context<Self>) {
        if self.scan.scanning {
            self.pending_rescan = true;
            return;
        }
        self.scan = ScanProgress {
            scanning: true,
            ..Default::default()
        };
        cx.notify();
        spawn_scan(
            self.adapters.clone(),
            self.store.clone(),
            self.scan_events.clone(),
            false,
        );
    }

    fn on_bg_event(&mut self, ev: BgEvent, cx: &mut Context<Self>) -> Option<Notification> {
        match ev {
            BgEvent::Progress(p) => {
                let note = if !p.scanning && self.refreshing {
                    self.refreshing = false;
                    Some(match &p.error {
                        None => Notification::success("Sessions refreshed"),
                        Some(err) => Notification::error(format!("Refresh failed: {err}")),
                    })
                } else {
                    None
                };
                self.scan = p;
                // 扫描期间发生过 location 变更:那轮用的是旧 roster,
                // 终态一到立刻用当前 roster 补一轮增量
                if !self.scan.scanning && self.pending_rescan {
                    self.pending_rescan = false;
                    self.kick_incremental_scan(cx);
                }
                // 扫描期间 reload_insights 被按住(见其注释),终态补最后一次
                if !self.scan.scanning {
                    self.reload_insights(cx);
                }
                cx.notify();
                note
            }
            BgEvent::Changed => {
                self.refresh(cx);
                None
            }
            BgEvent::RescanNeeded => {
                // 撞上进行中的扫描就排队,由终态事件补扫(kick_incremental_scan
                // 自带这条状态机);连续多条 rescan 也只会排一次
                self.kick_incremental_scan(cx);
                None
            }
        }
    }

    // ---------- 事件处理 ----------

    fn on_list_event(
        &mut self,
        list: &Entity<ListState<SessionsDelegate>>,
        ev: &ListEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let ix = match ev {
            ListEvent::Select(ix) | ListEvent::Confirm(ix) => *ix,
            ListEvent::Cancel => return,
        };
        let key = {
            let list = list.read(cx);
            let delegate = list.delegate();
            delegate.row_key(ix).map(str::to_string)
        };
        if let Some(key) = key {
            if matches!(ev, ListEvent::Confirm(_))
                && self
                    .detail
                    .as_ref()
                    .is_some_and(|detail| detail.meta.key == key)
                && self.toggle_list_key(&key, window, cx)
            {
                return;
            }
            // 用户手动选择优先于尚未完成的搜索定位，避免旧任务稍后抢回高亮。
            self._list_seek_task = None;
            self.open_detail(&key, None, window, cx);
        }
    }

    fn on_palette_event(
        &mut self,
        _list: &Entity<ListState<SearchDelegate>>,
        ev: &ListEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match ev {
            ListEvent::Confirm(ix) => self.open_palette_hit(ix.row, window, cx),
            // Select 仅表示高亮移动,不打开
            ListEvent::Select(_) => {}
            // 焦点在结果列表内(鼠标点过行)时 esc 走这里;焦点在输入框时
            // esc 由 Input 冒泡给 Dialog 的 keyboard Cancel 关闭
            ListEvent::Cancel => window.close_dialog(cx),
        }
    }

    pub fn toggle_search(&mut self, _: &ToggleSearch, window: &mut Window, cx: &mut Context<Self>) {
        if window.has_active_dialog(cx) {
            window.close_dialog(cx);
            return;
        }
        let list = self.palette_list.clone();
        let input = self.palette_input.clone();
        let this = cx.entity();
        open_closable_dialog(window, cx, move |dialog, window, cx| {
            let theme = cx.theme();
            let has_query = input.read(cx).text().len() > 0;
            // 输入框尺寸;清除钮的 suffix 补偿 margin 从它派生,改档自动跟随
            let input_size = gpui_component::Size::Large;
            dialog
                .w(px(680.))
                .margin_top(px(72.))
                // Dialog 默认内容 padding 24px 四边;水平 20,用户定稿(2026-08-18)
                .px(SPACE_XL)
                .close_button(false)
                .child(
                    v_flex()
                        // ↑↓ 在 Input 内不被消费,冒泡到这里走 main.rs 的
                        // PALETTE_CONTEXT 键位(Input 拆出 List 后原生 List 绑定够不着)
                        .key_context(PALETTE_CONTEXT)
                        .relative()
                        .on_action(window.listener_for(
                            &this,
                            |wb: &mut Self, _: &PaletteUp, window, cx| {
                                wb.palette_move(-1, window, cx)
                            },
                        ))
                        .on_action(window.listener_for(
                            &this,
                            |wb: &mut Self, _: &PaletteDown, window, cx| {
                                wb.palette_move(1, window, cx)
                            },
                        ))
                        // 定高 + 列表 flex_1:输入行/footer 尺寸变化时列表自适应,
                        // 不用手工重算列表高度
                        .h(PALETTE_HEIGHT)
                        .gap(SPACE_MD)
                        .child(
                            div()
                                .flex_shrink_0()
                                .px(SPACE_SM)
                                .border_b_1()
                                .border_color(theme.border)
                                .child(
                                    Input::new(&input)
                                        .with_size(input_size)
                                        .prefix(
                                            icon("icons/search.svg")
                                                .with_size(px(16.))
                                                .text_color(theme.muted_foreground),
                                        )
                                        // 清除钮自绘,不用内置 cleanable:内置钮固定
                                        // xsmall(实渲 10.5px 图标)且无尺寸配置口
                                        .when(has_query, |i| {
                                            i.suffix(
                                                div()
                                                    .id("palette-clear")
                                                    .size(px(24.))
                                                    // 抵消组件对 suffix 区强加的
                                                    // pr(input_px(size)),它在 p_0
                                                    // 之后应用盖不掉;不抵消则清除钮
                                                    // 右缩进与左侧放大镜不对称
                                                    .mr(-input_size.input_px())
                                                    .flex()
                                                    .items_center()
                                                    .justify_center()
                                                    .rounded(theme.radius)
                                                    .cursor_pointer()
                                                    .text_color(theme.muted_foreground)
                                                    // 图标-only:尺寸走 with_size,
                                                    // hover 裸改色不踩 text 替换陷阱
                                                    .hover(|s| {
                                                        s.bg(theme.secondary_hover)
                                                            .text_color(theme.foreground)
                                                    })
                                                    .on_click({
                                                        let input = input.clone();
                                                        move |_, window, cx| {
                                                            input.update(cx, |st, cx| {
                                                                st.set_value("", window, cx);
                                                                st.focus(window, cx);
                                                            });
                                                        }
                                                    })
                                                    .child(
                                                        icon("icons/circle-x.svg")
                                                            .with_size(px(16.)),
                                                    ),
                                            )
                                        })
                                        .p_0()
                                        .appearance(false),
                                ),
                        )
                        .child(
                            List::new(&list)
                                .with_size(gpui_component::Size::Large)
                                .flex_1()
                                .min_h_0(),
                        )
                        .child(
                            h_flex()
                                .flex_shrink_0()
                                .border_t_1()
                                .border_color(theme.border)
                                .pt(SPACE_SM)
                                // 与输入行的壳同缩进:文字与放大镜/清除钮左右对齐
                                .px(SPACE_SM)
                                .justify_between()
                                .text_size(FONT_LABEL)
                                .text_color(theme.muted_foreground)
                                .child("Scope: all sessions")
                                .child(
                                    h_flex()
                                        .gap(SPACE_MD)
                                        .child("\u{2191}\u{2193} navigate")
                                        .child("\u{21a9} open")
                                        .child("esc close"),
                                ),
                        ),
                )
        });
        let focus_input = self.palette_input.clone();
        cx.defer_in(window, move |_, window, cx| {
            focus_input.update(cx, |st, cx| st.focus(window, cx));
        });
        cx.notify();
    }

    /// ⌘K 输入框事件:文字变化驱动搜索,回车打开选中项
    fn on_palette_input_event(
        &mut self,
        input: &Entity<InputState>,
        ev: &InputEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match ev {
            InputEvent::Change => {
                let q = input.read(cx).value().trim().to_string();
                if self.palette_list.read(cx).delegate().last_query == q {
                    return;
                }
                // 清空(点清除钮/删光)结果已知,同步清态,省掉线程往返
                if q.is_empty() {
                    self._palette_search_task = None;
                    self.palette_list.update(cx, |state, cx| {
                        let d = state.delegate_mut();
                        d.hits = Vec::new();
                        d.degraded = false;
                        d.last_query = String::new();
                        state.set_selected_index(None, window, cx);
                        cx.notify();
                    });
                    return;
                }
                let task = self.palette_list.update(cx, |state, cx| {
                    state.set_selected_index(None, window, cx);
                    state.delegate_mut().perform_search(&q, window, cx)
                });
                // 搜索回填后选中首条并回滚到顶(覆盖旧任务 = 取消过期搜索)
                self._palette_search_task = Some(cx.spawn_in(window, async move |this, cx| {
                    task.await;
                    this.update_in(cx, |this, window, cx| {
                        this.palette_list.update(cx, |state, cx| {
                            let has_hits = !state.delegate().hits.is_empty();
                            state.set_selected_index(has_hits.then(IndexPath::default), window, cx);
                            // scroll_to_item 自带 notify,列表随之重绘
                            state.scroll_to_item(
                                IndexPath::default(),
                                ScrollStrategy::Top,
                                window,
                                cx,
                            );
                        });
                    })
                    .ok();
                }));
            }
            InputEvent::PressEnter { .. } => {
                let row = self
                    .palette_list
                    .read(cx)
                    .selected_index()
                    .map(|i| i.row)
                    .unwrap_or(0);
                self.open_palette_hit(row, window, cx);
            }
            _ => {}
        }
    }

    /// 打开第 row 条搜索命中(回车与鼠标点击共用),定位到命中消息
    fn open_palette_hit(&mut self, row: usize, window: &mut Window, cx: &mut Context<Self>) {
        let hit = self
            .palette_list
            .read(cx)
            .delegate()
            .hits
            .get(row)
            .map(|h| (h.session.key.clone(), h.seq));
        if let Some((key, seq)) = hit {
            window.close_dialog(cx);
            self.open_detail(&key, Some(seq), window, cx);
        }
    }

    /// ⌘K 面板 ↑↓:焦点在输入框,选中态手动挪(clamp 到两端,不循环)。
    /// 按 row 平移——SearchDelegate 单 section,section 恒 0
    fn palette_move(&mut self, delta: i64, window: &mut Window, cx: &mut Context<Self>) {
        self.palette_list.update(cx, |state, cx| {
            let n = state.delegate().hits.len() as i64;
            if n == 0 {
                return;
            }
            let cur = state.selected_index().map(|ix| ix.row as i64).unwrap_or(-1);
            let next = (cur + delta).clamp(0, n - 1) as usize;
            state.set_selected_index(Some(IndexPath::new(next)), window, cx);
            // scroll_to_selected_item 内部 notify,选中高亮随之重绘
            state.scroll_to_selected_item(window, cx);
        });
    }

    // ---------- 详情 ----------

    fn open_detail(
        &mut self,
        key: &str,
        jump_seq: Option<i64>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Ok(Some(meta)) = self.store.get_session(key) else {
            return;
        };
        self.detail = Some(DetailState {
            meta: meta.clone(),
            transcript: Rc::new(Vec::new()),
            loading: true,
            error: None,
            // Bottom 对齐 = 聊天语义:打开落在最新消息,向上翻历史
            msg_list: gpui::ListState::new(0, gpui::ListAlignment::Bottom, px(512.)),
            expanded_tools: HashSet::new(),
            expanded_thinking: HashSet::new(),
            jump_seq,
            images: Vec::new(),
            zoom: None,
            image_action_feedback: [None; 2],
        });
        // 搜索路径:中栏列表同步选中并滚到该会话。
        // 列表点击路径(jump=None)不走——List 点击自带选中,再滚会跳视口
        if jump_seq.is_some() {
            self.sync_list_selection(key, window, cx);
        }
        cx.notify();

        let adapters = self.adapters.clone();
        let task = cx.background_spawn(async move {
            let result = load_visible_transcript(&adapters, &meta).map(|mut messages| {
                let images = take_image_slots(&mut messages);
                (messages, images)
            });
            (meta.key.clone(), result)
        });
        cx.spawn_in(window, async move |this, cx| {
            let (key, result) = task.await;
            this.update_in(cx, |this, _window, cx| {
                if let Some(detail) = &mut this.detail {
                    if key == detail.meta.key {
                        detail.loading = false;
                        match result {
                            Ok((messages, images)) => {
                                detail.msg_list = gpui::ListState::new(
                                    messages.len(),
                                    gpui::ListAlignment::Bottom,
                                    px(512.),
                                );
                                // 搜索跳转:seq → 可见消息下标,滚到视口顶。
                                // FTS 命中的行可能被详情过滤(如空文本),用 >= 落到
                                // 其后最近一条;找不到(尾部被滤)则保持默认落底。
                                // jump_seq 归一为落点消息的实际 seq——高亮按精确相等
                                // 渲染,不归一则命中被滤时滚动与高亮指向不同行
                                if let Some(seq) = detail.jump_seq {
                                    if let Some(ix) = messages.iter().position(|m| m.seq >= seq) {
                                        detail.jump_seq = Some(messages[ix].seq);
                                        detail.msg_list.scroll_to(gpui::ListOffset {
                                            item_ix: ix,
                                            offset_in_item: px(0.),
                                        });
                                    }
                                }
                                detail.transcript = Rc::new(messages);
                                detail.images = images;
                                detail.zoom = None;
                                detail.error = None;
                            }
                            Err(error) => {
                                detail.transcript = Rc::new(Vec::new());
                                detail.images.clear();
                                detail.zoom = None;
                                detail.msg_list =
                                    gpui::ListState::new(0, gpui::ListAlignment::Bottom, px(512.));
                                detail.error = Some(error.into());
                            }
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// 侧栏目的地互斥的唯一写入点:三个筛选字段与"离开 Insights"一起落,
    /// 每个导航行只算自己的目标值——新目的地不必再逐个 listener 补互斥
    fn set_scope(
        &mut self,
        agent: Option<AgentId>,
        project: Option<String>,
        favorite: bool,
        cx: &mut Context<Self>,
    ) {
        self.selected_agent = agent;
        self.selected_project = project;
        self.favorite_only = favorite;
        self.insights_open = false;
        self.refresh(cx);
    }

    /// 清空过滤回 All Sessions 视图(侧栏点击与搜索打开共用;
    /// 已在 All Sessions 时 refresh 幂等,微秒级)
    fn show_all_sessions(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.set_scope(None, None, false, cx);
    }

    /// 搜索命中打开:侧栏切回 All Sessions(搜索是全库范围,过滤视图下
    /// 命中可能不在列表里),中栏定位选中该会话并滚到可见
    fn sync_list_selection(&mut self, key: &str, window: &mut Window, cx: &mut Context<Self>) {
        self._list_seek_task = None;
        // 全文搜索包含归档会话，而 All Sessions 明确排除归档。归档命中仍可在
        // 右栏打开，但不能为了一个永远不可见的 key 把所有活动页全部读进内存。
        if !self
            .store
            .get_session(key)
            .ok()
            .flatten()
            .is_some_and(|session| !session.archived)
        {
            return;
        }
        self.show_all_sessions(window, cx);
        // show_all_sessions 的通用刷新会暂存刷新前的旧选择；搜索有更明确的新
        // 目标，不能让下一帧的恢复逻辑把刚选中的搜索命中覆盖回去。
        self.pending_list_selection = None;
        if !self.select_list_key(key, true, window, cx) {
            self.seek_list_key(key, window, cx);
        }
    }

    /// 搜索命中可能落在尚未加载的页。后台逐页补齐到命中项，再用分组后的
    /// IndexPath 选中；generation 防止筛选/排序变化后旧任务污染新列表。
    fn seek_list_key(&mut self, key: &str, window: &mut Window, cx: &mut Context<Self>) {
        let request = {
            let list = self.list_state.read(cx);
            let Some(page) = list.delegate().pagination.as_ref() else {
                return;
            };
            if page.next_offset >= page.total {
                return;
            }
            (
                page.generation,
                page.filter.clone(),
                page.next_offset,
                page.total,
            )
        };
        let (generation, mut filter, mut offset, mut total) = request;
        let root_key = if filter.roots_only {
            self.visible_parent_key(key)
                .unwrap_or_else(|| key.to_string())
        } else {
            key.to_string()
        };
        if !self
            .store
            .get_session(&root_key)
            .ok()
            .flatten()
            .is_some_and(|session| session_matches_filter(&session, &filter))
        {
            return;
        }
        let store = self.store.clone();
        let key = key.to_string();
        let wanted_key = root_key.clone();
        let query = cx.background_spawn(async move {
            let mut loaded = Vec::new();
            let mut found = false;
            while offset < total {
                filter.limit = SESSION_PAGE_SIZE;
                filter.offset = offset;
                let (page, current_total) = store.list_sessions(&filter).ok()?;
                total = current_total.max(0);
                let received = i64::try_from(page.len()).unwrap_or(i64::MAX);
                if received == 0 {
                    offset = total;
                    break;
                }
                found |= page.iter().any(|session| session.key == wanted_key);
                offset = offset.saturating_add(received).min(total);
                loaded.extend(page);
                if found {
                    break;
                }
            }
            Some((generation, loaded, total, offset, found))
        });

        self._list_seek_task = Some(cx.spawn_in(window, async move |this, cx| {
            let Some((generation, sessions, total, next_offset, found)) = query.await else {
                return;
            };
            this.update_in(cx, |this, window, cx| {
                let applied = this.list_state.update(cx, |state, cx| {
                    let delegate = state.delegate_mut();
                    let Some(page) = delegate.pagination.as_mut() else {
                        return false;
                    };
                    if page.generation != generation {
                        return false;
                    }
                    page.total = total;
                    page.next_offset = page.next_offset.max(next_offset).min(total);
                    page.failed = false;
                    delegate.append_sessions(sessions);
                    cx.notify();
                    true
                });
                if applied && found {
                    this.select_list_key(&key, true, window, cx);
                }
            })
            .ok();
        }));
    }

    /// 按稳定 session key 恢复列表选择；排序、置顶和分组都可能改变 IndexPath。
    fn select_list_key(
        &mut self,
        key: &str,
        scroll: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let parent_key = self
            .list_state
            .read(cx)
            .delegate()
            .tree_mode()
            .then(|| self.visible_parent_key(key))
            .flatten();
        self.list_state.update(cx, |state, cx| {
            if state.delegate().row_index(key).is_none() {
                if let Some(parent) = parent_key.as_deref() {
                    if state.delegate().row_index(parent).is_some() {
                        state.delegate_mut().ensure_expanded(parent);
                    }
                }
            }
            let path = state
                .delegate()
                .row_index(key)
                .and_then(|flat| state.delegate().index_path(flat));
            state.set_selected_index(path, window, cx);
            if scroll {
                if let Some(path) = path {
                    // 组件无 strict-Top:非 Center 策略都是"最小滚动恰好可见",
                    // 目标从下方进入会贴底。先把 offset 拉到超底,deferred 消费时
                    // 目标位于视口上方,最小滚动分支即把它对齐到视口顶。
                    // 耦合 gpui-component 0.5.1 行为;上游 DeferredScrollToItem 的
                    // scroll_strict 字段目前写死 false 未被读——它被接通之日,
                    // 换成 strict-Top 调用并删掉这行 set_offset
                    state.scroll_handle().set_offset(point(px(0.), px(-1e9)));
                    state.scroll_to_item(path, ScrollStrategy::Top, window, cx);
                }
            }
            path.is_some()
        })
    }

    fn visible_parent_key(&self, key: &str) -> Option<String> {
        let parent_key = self.store.parent_key_of(key).ok().flatten()?;
        self.store
            .get_session(&parent_key)
            .ok()
            .flatten()
            .filter(|parent| !parent.archived)
            .map(|_| parent_key)
    }

    /// 展开/收起后按 session key 恢复选择，避免插入的子行让 IndexPath 漂到
    /// 下一条会话。若被收起的正是已选子会话，则选择回父会话。
    fn toggle_list_key(&mut self, key: &str, window: &mut Window, cx: &mut Context<Self>) -> bool {
        self.list_state.update(cx, |state, cx| {
            if !state.delegate().row_has_children(key) {
                return false;
            }
            let selected_key = state
                .selected_index()
                .and_then(|path| state.delegate().row_key(path))
                .map(str::to_string);
            if !state.delegate_mut().toggle(key) {
                return false;
            }
            let selected_path = selected_key
                .as_deref()
                .and_then(|selected| state.delegate().row_index(selected))
                .and_then(|flat| state.delegate().index_path(flat))
                .or_else(|| {
                    state
                        .delegate()
                        .row_index(key)
                        .and_then(|flat| state.delegate().index_path(flat))
                });
            state.set_selected_index(selected_path, window, cx);
            cx.notify();
            true
        })
    }

    // ---------- 操作 ----------

    /// 后台任务完成 → 推通知的通用桥(do_resume 在用)
    fn notify_when_done<T: Send + 'static>(
        window: &mut Window,
        cx: &mut Context<Self>,
        task: gpui::Task<T>,
        to_note: impl FnOnce(T) -> Notification + Send + 'static,
    ) {
        cx.spawn_in(window, async move |_this, cx| {
            let result = task.await;
            cx.update(|window, cx| {
                window.push_notification(to_note(result), cx);
            })
            .ok();
        })
        .detach();
    }

    // ---------- Open In 目标记忆 ----------

    /// Open In 目标解析:本 agent 的记忆 → 最近全局选择(仍可用时)→
    /// 列表首项
    fn open_in_target(
        &self,
        agent: AgentId,
        terms: &[terminal::TerminalApp],
    ) -> Option<terminal::TerminalApp> {
        self.preferred_terminal
            .get(&agent)
            .copied()
            .filter(|t| terms.contains(t))
            .or_else(|| self.last_terminal.filter(|t| terms.contains(t)))
            .or_else(|| terms.first().copied())
    }

    /// Open In 记忆持久化(prefs 表,key `open_in`):
    /// `{"agents":{"claude-code":"claude-desktop",…},"last":"ghostty"}`。
    /// 读取时按 id 对回本机已装终端——卸载/换平台后的残值静默丢弃
    fn load_open_in_prefs(&mut self) {
        let Some(raw) = self.store.pref_get("open_in") else {
            return;
        };
        let Ok(prefs) = serde_json::from_str::<OpenInPrefs>(&raw) else {
            return;
        };
        let by_id = |s: &str| {
            terminal::installed_terminals()
                .iter()
                .find(|t| t.id() == s)
                .copied()
        };
        for (k, val) in &prefs.agents {
            if let (Some(agent), Some(term)) = (AgentId::from_str(k), by_id(val)) {
                self.preferred_terminal.insert(agent, term);
            }
        }
        self.last_terminal = prefs.last.as_deref().and_then(by_id);
    }

    fn save_open_in_prefs(&self) {
        let prefs = OpenInPrefs {
            agents: self
                .preferred_terminal
                .iter()
                .map(|(a, t)| (a.as_str().to_string(), t.id().to_string()))
                .collect(),
            last: self.last_terminal.map(|t| t.id().to_string()),
        };
        if let Ok(raw) = serde_json::to_string(&prefs) {
            let _ = self.store.pref_set("open_in", &raw);
        }
    }

    /// per-agent 记忆任何点击都写(只影响本 agent,收敛到用户实际用的);
    /// 全局 last_terminal 只在 explicit(下拉显式选择)时写——左段点的可能
    /// 是回退值,写进全局会让一个 dsh 会话把 Kooky 偏好冲成 Terminal
    fn do_resume(
        &mut self,
        term: terminal::TerminalApp,
        explicit: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(detail) = &self.detail else { return };
        self.preferred_terminal.insert(detail.meta.agent, term);
        if explicit {
            self.last_terminal = Some(term);
        }
        self.save_open_in_prefs();
        cx.notify(); // split 按钮左段立即切到本次选择
        let meta = detail.meta.clone();
        let task = cx.background_spawn(async move { terminal::resume_session_in(&meta, term) });
        Self::notify_when_done(window, cx, task, |outcome| {
            if outcome.ok {
                Notification::success(format!("Opened in terminal: {}", outcome.command))
            } else {
                Notification::error(outcome.error.unwrap_or_else(|| "Resume failed".into()))
            }
        });
    }

    fn toggle_favorite(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(detail) = &mut self.detail {
            let key = detail.meta.key.clone();
            let v = !detail.meta.favorite;
            let _ = self.store.set_user_data(&detail.meta.key, Some(v), None);
            detail.meta.favorite = v;
            self.refresh(cx);
            self.select_list_key(&key, false, window, cx);
        }
    }

    fn toggle_pinned(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(detail) = &mut self.detail {
            let key = detail.meta.key.clone();
            let v = !detail.meta.pinned;
            let _ = self.store.set_user_data(&detail.meta.key, None, Some(v));
            detail.meta.pinned = v;
            self.refresh(cx);
            self.select_list_key(&key, true, window, cx);
        }
    }

    /// 导出:系统"另存为"选路径(issue #25,此前直接写进 Downloads),后台解析写文件;
    /// 取消无事发生
    fn do_export(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(detail) = &self.detail else { return };
        let meta = detail.meta.clone();
        let adapters = self.adapters.clone();
        let name = exporter::default_file_name(&meta, "md");
        save_as(
            window,
            cx,
            self.store.clone(),
            name,
            "Exported",
            "Export failed",
            move |path| {
                let adapter = adapter_for(&adapters, meta.agent, &meta.file_path)
                    .ok_or_else(|| anyhow::anyhow!("no adapter for this session"))?;
                std::fs::write(path, exporter::render_markdown(adapter, &meta)?)?;
                Ok(())
            },
        )
        .detach();
    }

    /// 执行删除:文件进废纸篓 + 自库 tombstone。trash_paths 可能长阻塞
    /// (契约见 terminal/mod.rs,平台缘由见各实现 doc)——必须离开 UI 线程,
    /// 否则界面在授权框弹出的整段时间里完全冻结。
    fn do_delete(
        &mut self,
        keys: Vec<String>,
        targets: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let store = self.store.clone();
        let trash_keys = keys.clone();
        let count = keys.len();
        let task = cx.background_spawn(async move {
            terminal::trash_paths(&targets)?;
            store.remove_sessions(&trash_keys, true)
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            this.update_in(cx, |this, window, cx| match result {
                Ok(()) => {
                    // 等待期间用户可能已翻到别的会话，只在仍停在被删子树时清空。
                    if this
                        .detail
                        .as_ref()
                        .is_some_and(|detail| keys.iter().any(|key| key == &detail.meta.key))
                    {
                        this.detail = None;
                    }
                    let message = if count == 1 {
                        SESSION_TRASHED.to_string()
                    } else {
                        format!(
                            "{count} sessions {}",
                            SESSION_TRASHED
                                .strip_prefix("Session ")
                                .unwrap_or(SESSION_TRASHED)
                        )
                    };
                    window.push_notification(Notification::success(message), cx);
                    // 立刻把它从列表摘掉,不等 watcher 那 800ms 去抖
                    this.refresh(cx);
                }
                Err(e) => {
                    window.push_notification(Notification::error(format!("Delete failed: {e}")), cx)
                }
            })
            .ok();
        })
        .detach();
    }

    fn confirm_delete(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(detail) = &self.detail else { return };
        let meta = detail.meta.clone();
        let mut sessions = vec![meta.clone()];
        sessions.extend(self.store.all_descendants(&meta.key).unwrap_or_default());
        let nested_count = sessions.len().saturating_sub(1);
        let keys: Vec<String> = sessions.iter().map(|session| session.key.clone()).collect();
        // 每个子会话按自己的胜出 file_path 找 adapter；多 location 下不能沿用
        // 根会话所属实例。磁盘目标保持稳定顺序并去重。
        let mut seen_targets = HashSet::new();
        let mut targets = Vec::new();
        for session in &sessions {
            let paths = adapter_for(&self.adapters, session.agent, &session.file_path)
                .map(|adapter| adapter.session_paths(session))
                .unwrap_or_else(|| vec![session.file_path.clone()]);
            for path in paths {
                if seen_targets.insert(path.clone()) {
                    targets.push(path);
                }
            }
        }
        let entity = cx.entity();
        window.open_alert_dialog(cx, move |dialog, _window, cx| {
            let meta = meta.clone();
            let keys = keys.clone();
            let targets = targets.clone();
            let entity = entity.clone();
            let theme = cx.theme();
            dialog
                .title(
                    div()
                        .text_size(FONT_HEADING)
                        .font_semibold()
                        .child(if nested_count > 0 {
                            "Delete this session tree?"
                        } else {
                            "Delete this session?"
                        }),
                )
                .width(px(440.))
                // 破坏性确认:主按钮点名动作并用 danger 形态,不留裸 "OK"。
                // .confirm() 必须显式调用——Dialog 只在设了 footer 时才渲染
                // 按钮行,只挂 on_ok 的弹窗实际无按钮(仅回车可确认)
                .confirm()
                .button_props(
                    gpui_component::dialog::DialogButtonProps::default()
                        .ok_text(MOVE_TO_TRASH)
                        .ok_variant(gpui_component::button::ButtonVariant::Danger),
                )
                .child(
                    v_flex()
                        .gap(SPACE_SM)
                        .text_size(FONT_BODY)
                        .child(if nested_count > 0 {
                            format!(
                                "This session and {nested_count} nested session{} will be moved to {}. You can restore them anytime:",
                                if nested_count == 1 { "" } else { "s" },
                                if cfg!(target_os = "windows") { "the Recycle Bin" } else { "Trash" },
                            )
                        } else {
                            TRASH_CONFIRM_BODY.to_string()
                        })
                        .child(
                            div()
                                .px(SPACE_SM)
                                .py(SPACE_XS)
                                .rounded(theme.radius)
                                .bg(theme.muted)
                                .text_size(FONT_CAPTION)
                                // 等宽走主题 token(Menlo 只有 macOS 有;
                                // Windows 上找不到会静默回落到比例字体的
                                // 系统 UI 字体,与其他路径 chip 不一致)
                                .font_family(theme.mono_font_family.clone())
                                .child(meta.file_path.clone()),
                        )
                        .when(meta.agent == AgentId::Codex, |this| {
                            this.child(
                                div()
                                    .text_size(FONT_CAPTION)
                                    .text_color(theme.muted_foreground)
                                    .child("Only the local file is removed — Codex's own records stay intact."),
                            )
                        }),
                )
                .on_ok(move |_, window, cx| {
                    entity.update(cx, |this, cx| {
                        this.do_delete(keys.clone(), targets.clone(), window, cx);
                    });
                    true
                })
        });
    }

    fn context_title(&self) -> String {
        if self.favorite_only {
            return "Starred".to_string();
        }
        if let Some(p) = &self.selected_project {
            // 项目显示名以侧栏列表(store 的 ProjectInfo)为准,不再重推
            return self
                .projects
                .iter()
                .find(|info| info.path == *p)
                .map(|info| info.name.clone())
                .unwrap_or_else(|| "Projects".to_string());
        }
        match self.selected_agent {
            None => "All Sessions".to_string(),
            Some(one) => one.display_name().to_string(),
        }
    }

    // ---------- 渲染 ----------

    fn render_sidebar(&self, window: &Window, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let all_active = self.selected_agent.is_none()
            && self.selected_project.is_none()
            && !self.favorite_only
            && !self.insights_open;
        // 常态沉默,仅刷新中/监听失效时出现;None 时状态栏整行不渲染。
        // 文案在此按 scan 现算,不另存字段——存下来就会有第二个写入点要维护
        let note = if self.scan.scanning {
            Some(match self.scan.total {
                0 => "Refreshing…".to_string(),
                total => format!("Refreshing {}/{}", self.scan.done, total),
            })
        } else {
            self.scan
                .error
                .as_ref()
                .map(|e| format!("Refresh failed: {e}"))
        };
        let status: Option<AnyElement> = if let Some(note) = note {
            Some(
                h_flex()
                    .w_full()
                    .gap(SPACE_SM)
                    .text_color(theme.muted_foreground)
                    .child(
                        icon("icons/refresh-cw.svg")
                            .with_size(px(12.))
                            .flex_shrink_0(),
                    )
                    .child(div().min_w_0().truncate().child(note))
                    .into_any_element(),
            )
        } else if self.watcher.is_none() {
            Some(
                h_flex()
                    .w_full()
                    .gap(SPACE_SM)
                    .text_color(theme.muted_foreground)
                    .child(
                        div()
                            .size(px(7.))
                            .rounded_full()
                            .flex_shrink_0()
                            .bg(theme.warning),
                    )
                    .child(div().min_w_0().truncate().child("Live updates off"))
                    .into_any_element(),
            )
        } else {
            None
        };

        // macOS 恒挂 TitleBar(traffic light 占位 + 拖拽区);Linux/Windows 按
        // 运行时装饰状态:系统给了标题栏(Server)就不挂——TitleBar 的非 mac
        // 实现无条件画 min/max/close 三按钮,会与系统标题栏成双套控制;系统
        // 不给(GNOME Wayland 无 SSD 回落 Client)才挂,此时它是唯一的拖拽区
        // 与窗口按钮(按钮图标 window-*.svg 在 assets 注册表,缺了就是隐形
        // 热区)。Windows 走 appears_transparent=false 的原生 caption,装饰
        // 恒报 Server,这里天然不挂。
        let show_titlebar = cfg!(target_os = "macos")
            || matches!(window.window_decorations(), Decorations::Client { .. });
        v_flex()
            .w(SIDEBAR_WIDTH)
            .h_full()
            .flex_shrink_0()
            .bg(theme.sidebar)
            // 压平 titlebar 靠 theme.rs 的 title_bar/title_bar_border token；主窗口
            // 使用 44px 高度，与详情顶部行共享同一垂直节奏。
            .when(show_titlebar, |this| {
                this.child(TitleBar::new().h(WINDOW_TITLEBAR_HEIGHT))
            })
            .child(
                div()
                    .flex_shrink_0()
                    .h(WINDOW_TITLEBAR_HEIGHT)
                    .px(SIDEBAR_EDGE)
                    .pt(SPACE_XS)
                    .pb(SPACE_LG)
                    .child(
                        div()
                            .pl(TITLE_INSET)
                            .pr(SIDEBAR_EDGE)
                            .text_size(FONT_HEADING)
                            .font_semibold()
                            .text_color(theme.foreground)
                            .child("Wake"),
                    ),
            )
            .child(
                div().flex_shrink_0().px(SIDEBAR_EDGE).pb(SPACE_MD).child(
                    h_flex().gap(SPACE_SM).child(
                        h_flex()
                            .id("sidebar-search")
                            .flex_1()
                            .min_w_0()
                            .h(ROW_HEIGHT)
                            .px(SIDEBAR_EDGE)
                            .gap(SPACE_SM)
                            .rounded(theme.radius)
                            .cursor_pointer()
                            .bg(theme.secondary)
                            .text_size(FONT_CAPTION)
                            .text_color(theme.muted_foreground)
                            .hover(|s| {
                                s.bg(theme.secondary_hover)
                                    .text_colored(theme.foreground, FONT_CAPTION)
                            })
                            .active(|s| {
                                s.bg(theme.secondary_active)
                                    .text_colored(theme.foreground, FONT_CAPTION)
                            })
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.toggle_search(&ToggleSearch, window, cx)
                            }))
                            .child(icon("icons/search.svg").with_size(px(13.)).flex_shrink_0())
                            // flex_1 + min_w_0 + truncate:空间不足时压这里,
                            // 绝不把右侧刷新按钮挤出侧栏
                            .child(div().flex_1().min_w_0().truncate().child("Search sessions"))
                            .child(
                                div()
                                    .flex_shrink_0()
                                    .text_size(FONT_LABEL)
                                    .child(search_key_hint()),
                            ),
                    ),
                ),
            )
            .child(
                v_flex()
                    .flex_shrink_0()
                    .px(SIDEBAR_EDGE)
                    .pb(SPACE_XS)
                    .gap(SPACE_XS)
                    .child(sidebar_row(
                        "all",
                        RowLead::Icon(icon("icons/layers.svg")),
                        "All Sessions",
                        Some(self.agent_counts.iter().map(|(_, n)| n).sum()),
                        all_active,
                        RowLevel::Primary,
                        cx.listener(|this, _, window, cx| {
                            this.show_all_sessions(window, cx);
                        }),
                        cx,
                    ))
                    .child(sidebar_row(
                        "fav",
                        RowLead::Icon(icon("icons/star.svg")),
                        "Starred",
                        if self.starred_count > 0 {
                            Some(self.starred_count)
                        } else {
                            None
                        },
                        self.favorite_only,
                        RowLevel::Primary,
                        cx.listener(|this, _, _window, cx| {
                            // 取消收藏过滤时 agent/project 必已是 None(互斥),
                            // 两个方向都归 set_scope
                            let favorite = !this.favorite_only;
                            this.set_scope(None, None, favorite, cx);
                        }),
                        cx,
                    )),
            )
            .child(
                v_flex()
                    .id("sidebar-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px(SIDEBAR_EDGE)
                    .pt(SPACE_XS)
                    .pb(SPACE_LG)
                    .gap(SPACE_XS)
                    .child(group_header(
                        "agents-header",
                        "Agents",
                        self.agents_collapsed,
                        cx.listener(|this, _, _window, cx| {
                            this.agents_collapsed = !this.agents_collapsed;
                            cx.notify();
                        }),
                        cx,
                    ))
                    .when(!self.agents_collapsed, |this| {
                        this.children(self.agent_counts.iter().map(|(agent, count)| {
                            let agent = *agent;
                            sidebar_row(
                                agent.as_str(),
                                RowLead::Brand(agent.brand_icon(theme.mode.is_dark())),
                                agent.display_name(),
                                Some(*count),
                                self.selected_agent == Some(agent),
                                RowLevel::Sub,
                                cx.listener(move |this, _, _window, cx| {
                                    let next = if this.selected_agent == Some(agent) {
                                        None
                                    } else {
                                        Some(agent)
                                    };
                                    this.set_scope(next, None, false, cx);
                                }),
                                cx,
                            )
                        }))
                    })
                    .child(group_header(
                        "projects-header",
                        "Projects",
                        self.projects_collapsed,
                        cx.listener(|this, _, _window, cx| {
                            this.projects_collapsed = !this.projects_collapsed;
                            cx.notify();
                        }),
                        cx,
                    ))
                    .when(!self.projects_collapsed, |this| {
                        this.children(self.projects.iter().enumerate().map(|(ix, p)| {
                            let path = p.path.clone();
                            sidebar_row(
                                ("proj", ix),
                                RowLead::Icon(icon("icons/folder.svg")),
                                p.name.clone(),
                                Some(p.session_count),
                                self.selected_project.as_deref() == Some(p.path.as_str()),
                                RowLevel::Sub,
                                cx.listener(move |this, _, _window, cx| {
                                    let next = if this.selected_project.as_deref()
                                        == Some(path.as_str())
                                    {
                                        None
                                    } else {
                                        Some(path.clone())
                                    };
                                    this.set_scope(None, next, false, cx);
                                }),
                                cx,
                            )
                        }))
                    }),
            )
            // 底部工具条:次要操作(数据源、刷新)与扫描状态同处一区,与上方
            // 导航行只用一条 border 分隔。按钮透明底、hover 才出色,不跟导航
            // 行的选中态抢注意力;图标-only 元素改 text_color 不丢字号
            .child(
                v_flex()
                    .flex_shrink_0()
                    .border_t_1()
                    .border_color(theme.sidebar_border)
                    .when_some(status, |this, status| {
                        this.child(
                            h_flex()
                                .px(SPACE_XL)
                                .pt(SPACE_MD)
                                .text_size(FONT_LABEL)
                                .child(status),
                        )
                    })
                    .child(
                        h_flex()
                            .h(SIDEBAR_FOOTER_ROW_HEIGHT)
                            .px(SIDEBAR_EDGE)
                            .items_center()
                            .justify_end()
                            .gap(SPACE_XS)
                            .child(sidebar_tool_btn(
                                "insights",
                                "Insights",
                                true,
                                // 页面打开时图标点亮 primary(显式设色后不被
                                // hover 的容器 text_color 覆盖)
                                {
                                    let mut ic = icon("icons/chart-column.svg").with_size(px(14.));
                                    if self.insights_open {
                                        ic = ic.text_color(theme.primary);
                                    }
                                    ic.into_any_element()
                                },
                                cx.listener(|this, _, _window, cx| this.toggle_insights(cx)),
                                cx,
                            ))
                            .child(sidebar_tool_btn(
                                "settings",
                                "Settings",
                                true,
                                icon("icons/settings.svg")
                                    .with_size(px(14.))
                                    .into_any_element(),
                                cx.listener(|this, _, _window, cx| this.open_settings(cx)),
                                cx,
                            ))
                            .child(sidebar_tool_btn(
                                "refresh",
                                "Refresh sessions",
                                !self.scan.scanning,
                                if self.scan.scanning {
                                    Spinner::new().small().into_any_element()
                                } else {
                                    icon("icons/refresh-cw.svg")
                                        .with_size(px(14.))
                                        .into_any_element()
                                },
                                cx.listener(|this, _, window, cx| {
                                    this.refresh_sessions(window, cx)
                                }),
                                cx,
                            )),
                    ),
            )
    }

    fn render_session_list(&self, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let shown = self.list_state.read(cx).delegate().rows.len();
        let library_empty = self.agent_counts.iter().map(|(_, n)| *n).sum::<i64>() == 0;
        let listed_count = self.total_sessions.max(0);
        let shown_label: SharedString = format!(
            "{} {}",
            listed_count,
            if listed_count == 1 {
                "session"
            } else {
                "sessions"
            }
        )
        .into();
        let sort_key = self.sort_key;
        let sort_ascending = self.sort_ascending;
        let sort_entity = cx.entity();
        let sort_label = match sort_key {
            SortKey::Updated => "Date updated",
            SortKey::Created => "Date created",
            SortKey::Messages => "Message count",
        };
        let sort_tooltip = format!(
            "Sort by {} · {}",
            sort_label,
            if sort_ascending {
                "Ascending"
            } else {
                "Descending"
            }
        );
        // 与详情工具栏统一：icon-only ghost，常态透明、hover 才出现背景。
        let sort_menu = Button::new("sort-sessions")
            .ghost()
            .rounded(RADIUS_BUTTON)
            .icon(icon("icons/arrow-up-down.svg").with_size(px(16.)))
            .tooltip(sort_tooltip)
            .dropdown_menu(move |menu, _, _| {
                let mk_key = |label: &'static str, key: SortKey| {
                    let entity = sort_entity.clone();
                    PopupMenuItem::new(label).checked(sort_key == key).on_click(
                        move |_, window, cx| {
                            entity.update(cx, |this, cx| {
                                let selected =
                                    this.detail.as_ref().map(|detail| detail.meta.key.clone());
                                this.sort_key = key;
                                this.refresh(cx);
                                if let Some(selected) = selected {
                                    this.select_list_key(&selected, true, window, cx);
                                }
                            });
                        },
                    )
                };
                let mk_dir = |label: &'static str, ascending: bool| {
                    let entity = sort_entity.clone();
                    PopupMenuItem::new(label)
                        .checked(sort_ascending == ascending)
                        .on_click(move |_, window, cx| {
                            entity.update(cx, |this, cx| {
                                let selected =
                                    this.detail.as_ref().map(|detail| detail.meta.key.clone());
                                this.sort_ascending = ascending;
                                this.refresh(cx);
                                if let Some(selected) = selected {
                                    this.select_list_key(&selected, true, window, cx);
                                }
                            });
                        })
                };
                menu.min_w(px(180.))
                    .item(mk_key("Date updated", SortKey::Updated))
                    .item(mk_key("Date created", SortKey::Created))
                    .item(mk_key("Message count", SortKey::Messages))
                    .separator()
                    .item(mk_dir("Descending", false))
                    .item(mk_dir("Ascending", true))
            })
            .anchor(Anchor::TopRight);
        v_flex()
            .w(SESSION_STREAM_WIDTH)
            .h_full()
            .flex_shrink_0()
            .bg(theme.colors.list)
            .child(
                v_flex()
                    .id("list-header")
                    .w_full()
                    .h(LIBRARY_IDENTITY_HEIGHT)
                    .flex_shrink_0()
                    .window_control_area(WindowControlArea::Drag)
                    .px(SPACE_LG)
                    .justify_center()
                    .child(
                        h_flex()
                            .w_full()
                            .items_start()
                            .justify_between()
                            .child(
                                v_flex()
                                    .min_w_0()
                                    .gap(px(2.))
                                    .child(
                                        div()
                                            .truncate()
                                            .text_size(FONT_TITLE)
                                            .font_semibold()
                                            .child(self.context_title()),
                                    )
                                    .child(
                                        div()
                                            .text_size(FONT_LABEL)
                                            .text_color(theme.muted_foreground)
                                            .child(shown_label),
                                    ),
                            )
                            .child(div().flex_shrink_0().pt(px(2.)).child(sort_menu)),
                    ),
            )
            .child(if shown == 0 {
                if library_empty && self.scan.scanning {
                    v_flex()
                        .flex_1()
                        .w_full()
                        .items_center()
                        .justify_center()
                        .gap(SPACE_MD)
                        .text_color(theme.muted_foreground)
                        .child(Spinner::new())
                        .child(
                            div()
                                .text_size(FONT_BODY)
                                .font_medium()
                                .text_color(theme.foreground)
                                .child("Building your library"),
                        )
                        .child(
                            div()
                                .text_size(FONT_CAPTION)
                                .child("Looking for local agent sessions…"),
                        )
                        .into_any_element()
                } else if library_empty {
                    v_flex()
                        .flex_1()
                        .w_full()
                        .items_center()
                        .justify_center()
                        .gap(SPACE_LG)
                        .child(empty_state(
                            "icons/layers.svg",
                            px(48.),
                            px(22.),
                            "Your library is empty",
                            "Add a local session location to get started.",
                            cx,
                        ))
                        .child(
                            Button::new("empty-open-settings")
                                .primary()
                                .small()
                                .rounded(RADIUS_BUTTON)
                                .label("Add a location")
                                .on_click(cx.listener(|this, _, _window, cx| {
                                    this.settings_page = SettingsPage::Locations;
                                    this.open_settings(cx);
                                })),
                        )
                        .into_any_element()
                } else {
                    v_flex()
                        .flex_1()
                        .w_full()
                        .justify_center()
                        .child(empty_state(
                            "icons/inbox.svg",
                            px(48.),
                            px(22.),
                            "No matching sessions",
                            "Try a different agent, project, or filter.",
                            cx,
                        ))
                        .into_any_element()
                }
            } else {
                List::new(&self.list_state)
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .into_any_element()
            })
    }

    fn show_image_action_success(
        &mut self,
        target: (usize, usize),
        action: ImageAction,
        cx: &mut Context<Self>,
    ) {
        self.image_action_feedback_generation =
            self.image_action_feedback_generation.wrapping_add(1);
        let feedback = ImageActionFeedback {
            target,
            action,
            generation: self.image_action_feedback_generation,
        };
        let Some(detail) = &mut self.detail else {
            return;
        };
        if detail.zoom != Some(target) {
            return;
        }
        detail.image_action_feedback[action.index()] = Some(feedback);
        cx.notify();

        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(1_600))
                .await;
            this.update(cx, |this, cx| {
                if let Some(detail) = &mut this.detail {
                    if detail.image_action_feedback[action.index()] == Some(feedback) {
                        detail.image_action_feedback[action.index()] = None;
                        cx.notify();
                    }
                }
            })
            .ok();
        })
        .detach();
    }

    fn render_image_zoom(&mut self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let Some((message_index, image_index)) =
            self.detail.as_ref().and_then(|detail| detail.zoom)
        else {
            return div().into_any_element();
        };
        let Some(ImageSlot::Ready { image, dims, .. }) = self
            .detail
            .as_ref()
            .and_then(|detail| detail.images.get(message_index))
            .and_then(|images| images.get(image_index))
            .cloned()
        else {
            return div().into_any_element();
        };

        let kind = image
            .format
            .mime_type()
            .trim_start_matches("image/")
            .to_ascii_uppercase();
        let size = crate::format::human_bytes(image.bytes.len());
        let metadata = match dims {
            Some((width, height)) => format!("{kind} · {width} × {height} · {size}"),
            None => format!("{kind} · {size}"),
        };
        let shown = zoom_fit(dims, window.viewport_size());
        let feedback = self
            .detail
            .as_ref()
            .map(|detail| detail.image_action_feedback)
            .unwrap_or([None; 2]);
        let target = (message_index, image_index);
        let copy_succeeded =
            feedback[ImageAction::Copy.index()].is_some_and(|feedback| feedback.target == target);
        let save_succeeded =
            feedback[ImageAction::Save.index()].is_some_and(|feedback| feedback.target == target);
        let success = cx.theme().success;

        let close_backdrop = cx.listener(|this, _, _, cx| {
            if let Some(detail) = &mut this.detail {
                detail.zoom = None;
                detail.image_action_feedback = [None; 2];
                cx.notify();
            }
        });
        let close_button = cx.listener(|this, _, _, cx| {
            if let Some(detail) = &mut this.detail {
                detail.zoom = None;
                detail.image_action_feedback = [None; 2];
                cx.notify();
            }
        });
        let copy_image = image.clone();
        let save_image = image.clone();
        let copy_action = cx.listener(move |this, _, window, cx| {
            cx.stop_propagation();
            cx.write_to_clipboard(ClipboardItem::new_image(&copy_image));
            this.show_image_action_success(target, ImageAction::Copy, cx);
            window.push_notification(Notification::success("Image copied"), cx);
        });
        let save_action = cx.listener(move |this, _, window, cx| {
            cx.stop_propagation();
            let image = save_image.clone();
            // 文件名用 gpui 解码时算好的内容 id,同一张图两次保存得到同一个名字
            let name = format!(
                "wake-image-{:016x}.{}",
                image.id,
                image_extension(image.format.mime_type())
            );
            let saved = save_as(
                window,
                cx,
                this.store.clone(),
                name,
                "Saved",
                "Couldn't save the image",
                move |path| Ok(std::fs::write(path, &image.bytes)?),
            );
            cx.spawn_in(window, async move |this, cx| {
                if saved.await.is_some() {
                    this.update_in(cx, |this, _, cx| {
                        this.show_image_action_success(target, ImageAction::Save, cx)
                    })
                    .ok();
                }
            })
            .detach();
        });

        div()
            .id("image-zoom")
            .absolute()
            .inset_0()
            .occlude()
            .bg(gpui::black().opacity(IMAGE_SCRIM))
            .on_click(close_backdrop)
            .child(
                v_flex()
                    .size_full()
                    .items_center()
                    .justify_center()
                    .gap(SPACE_XL)
                    .px(px(56.))
                    .py(px(52.))
                    .child(
                        gpui::img(image)
                            .id("image-zoom-figure")
                            .w(shown.width)
                            .h(shown.height)
                            .rounded(SPACE_SM)
                            .shadow(vec![zoom_shadow(px(18.), px(48.), 0.45)])
                            .on_click(|_, _, cx| cx.stop_propagation()),
                    )
                    .child(
                        h_flex()
                            .id("image-zoom-actions")
                            .flex_shrink_0()
                            .h(px(40.))
                            .items_center()
                            .gap(SPACE_SM)
                            .pl(SPACE_LG)
                            .pr(SPACE_SM)
                            .rounded(px(20.))
                            .bg(gpui::black().opacity(0.78))
                            .border_1()
                            .border_color(gpui::white().opacity(0.13))
                            .shadow(vec![zoom_shadow(px(8.), px(26.), 0.35)])
                            .on_click(|_, _, cx| cx.stop_propagation())
                            .child(
                                div()
                                    .text_size(FONT_LABEL)
                                    .text_color(gpui::white().opacity(0.64))
                                    .child(metadata),
                            )
                            .child(div().w(px(1.)).h(px(16.)).bg(gpui::white().opacity(0.16)))
                            .child(
                                h_flex()
                                    .id("image-copy")
                                    .size(px(30.))
                                    .items_center()
                                    .justify_center()
                                    .rounded(RADIUS_BUTTON)
                                    .cursor_pointer()
                                    .when(copy_succeeded, |style| style.bg(success.opacity(0.2)))
                                    .hover(move |style| {
                                        style.bg(if copy_succeeded {
                                            success.opacity(0.3)
                                        } else {
                                            gpui::white().opacity(0.16)
                                        })
                                    })
                                    .tooltip(move |window, cx| {
                                        gpui_component::tooltip::Tooltip::new(if copy_succeeded {
                                            "Copied"
                                        } else {
                                            "Copy image"
                                        })
                                        .build(window, cx)
                                    })
                                    .on_click(copy_action)
                                    .child(
                                        icon(if copy_succeeded {
                                            "icons/check.svg"
                                        } else {
                                            "icons/copy.svg"
                                        })
                                        .with_size(px(15.))
                                        .text_color(
                                            if copy_succeeded {
                                                success
                                            } else {
                                                gpui::white().opacity(0.84)
                                            },
                                        ),
                                    ),
                            )
                            .child(
                                h_flex()
                                    .id("image-save")
                                    .size(px(30.))
                                    .items_center()
                                    .justify_center()
                                    .rounded(RADIUS_BUTTON)
                                    .cursor_pointer()
                                    .when(save_succeeded, |style| style.bg(success.opacity(0.2)))
                                    .hover(move |style| {
                                        style.bg(if save_succeeded {
                                            success.opacity(0.3)
                                        } else {
                                            gpui::white().opacity(0.16)
                                        })
                                    })
                                    .tooltip(move |window, cx| {
                                        gpui_component::tooltip::Tooltip::new(if save_succeeded {
                                            "Saved"
                                        } else {
                                            "Save image"
                                        })
                                        .build(window, cx)
                                    })
                                    .on_click(save_action)
                                    .child(
                                        icon(if save_succeeded {
                                            "icons/check.svg"
                                        } else {
                                            "icons/download.svg"
                                        })
                                        .with_size(px(15.))
                                        .text_color(
                                            if save_succeeded {
                                                success
                                            } else {
                                                gpui::white().opacity(0.84)
                                            },
                                        ),
                                    ),
                            ),
                    ),
            )
            .child(
                h_flex()
                    .id("image-zoom-close")
                    .absolute()
                    .top(SPACE_LG)
                    .right(SPACE_LG)
                    .size(px(32.))
                    .items_center()
                    .justify_center()
                    .rounded(px(9.))
                    .bg(gpui::white().opacity(0.11))
                    .hover(|style| style.bg(gpui::white().opacity(0.2)))
                    .cursor_pointer()
                    .tooltip(|window, cx| {
                        gpui_component::tooltip::Tooltip::new("Close").build(window, cx)
                    })
                    .on_click(close_button)
                    .child(
                        icon("icons/close.svg")
                            .with_size(px(15.))
                            .text_color(gpui::white().opacity(0.86)),
                    ),
            )
            .into_any_element()
    }

    // ---------- 对话区逐消息渲染(设计语言:用户右气泡 / 助手平铺 / 工具折叠簇) ----------

    /// gpui::list 的行渲染。在布局阶段经 entity.update 调用(render 已返回,
    /// lease 已释放,无 double-lease 风险——与 dialog builder 的时机不同)。
    fn render_msg_row(
        &mut self,
        ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // 详情宽度随窗口变化；工具头要用当前可用像素反算显示格数，再交给
        // unicode-width 截断，避免窄窗口溢出、宽窗口仍停在固定长度。
        let reader_width =
            (window.viewport_size().width - SIDEBAR_WIDTH - SESSION_STREAM_WIDTH - SPACE_XXL * 2.)
                .clamp(px(220.), READER_MAX_WIDTH);
        let tool_summary_width = (reader_width - px(156.)).max(px(80.));
        let tool_arg_cells =
            ((f32::from(tool_summary_width) / TOOL_MONO_CELL_WIDTH) as usize).max(8);
        let theme = cx.theme();
        let dark = theme.mode.is_dark();
        // 尾部要用的 Copy 值提前取出,theme 借用不跨越 inner 构建期的 &mut cx
        let jump_bg = theme.primary.opacity(0.09);
        let jump_radius = theme.radius;
        let image_border = theme.border;
        let image_panel = theme.popover;
        let image_muted = theme.muted_foreground;
        let Some(detail) = &self.detail else {
            return div().into_any_element();
        };
        let total = detail.transcript.len();
        let tools_open = detail.expanded_tools.contains(&ix);
        let thinking_open = detail.expanded_thinking.contains(&ix);
        let jump_seq = detail.jump_seq;
        // Rc 克隆只加引用计数;逐行借用,避免每帧深拷贝整条消息(text 可达 32KB)
        let transcript = detail.transcript.clone();
        let shots = detail.images.get(ix).cloned().unwrap_or_default();
        let Some(m) = transcript.get(ix) else {
            return div().into_any_element();
        };
        // 只有 thinking、没有回复正文或工具调用的中间事件属于运行日志，
        // 连续铺在阅读视图里会把真正的对话切碎；完整原始记录仍保留在源文件中。
        if matches!(m.role, MessageRole::Assistant)
            && m.text.is_empty()
            && m.tool_calls.is_empty()
            && m.thinking.is_some()
            && shots.is_empty()
        {
            return div().into_any_element();
        }
        // 搜索跳转的落点消息:淡 primary 底色保持高亮,直到换会话
        let is_jump_target = jump_seq == Some(m.seq);

        let inner: AnyElement = if m.kind == MessageKind::CompactSummary {
            centered_pill("Context compacted", cx).into_any_element()
        } else {
            match m.role {
                MessageRole::User => {
                    let has_text = !m.text.trim().is_empty();
                    let mut bubble = v_flex()
                        .max_w(px(540.))
                        .min_w_0()
                        .gap(SPACE_SM)
                        .rounded(theme.radius_lg)
                        .bg(theme.muted)
                        .text_size(FONT_MSG_USER)
                        .line_height(relative(1.85));
                    bubble = if !shots.is_empty() && !has_text {
                        bubble.p(px(7.))
                    } else {
                        bubble.px(px(14.)).py(SPACE_SM)
                    };
                    // 气泡靠内容撑宽,各段直接挂在气泡上、中间不加任何包装层
                    //(原因见 message_content 的文档注释)
                    bubble = bubble.children(message_content(
                        ix,
                        m.seq,
                        &m.text,
                        &shots,
                        FONT_MSG_USER,
                        gpui::rems(0.5),
                        dark,
                        image_border,
                        image_panel,
                        image_muted,
                        cx.entity(),
                        cx,
                    ));
                    h_flex()
                        .w_full()
                        .justify_end()
                        .child(bubble)
                        .into_any_element()
                }
                MessageRole::Assistant => {
                    let mut col = v_flex().w_full().min_w_0().gap(SPACE_SM);
                    if let Some(thinking) = &m.thinking {
                        col = col.child(thinking_panel(
                            ix,
                            thinking,
                            thinking_open,
                            cx.listener(move |this, _, _window, cx| {
                                if let Some(detail) = &mut this.detail {
                                    toggle_expanded_row(&mut detail.expanded_thinking, ix);
                                    detail.msg_list.splice(ix..ix + 1, 1);
                                }
                                cx.notify();
                            }),
                            cx,
                        ));
                    }
                    let parts = message_content(
                        ix,
                        m.seq,
                        &m.text,
                        &shots,
                        FONT_MSG_BODY,
                        gpui::rems(0.6),
                        dark,
                        image_border,
                        image_panel,
                        image_muted,
                        cx.entity(),
                        cx,
                    );
                    if !parts.is_empty() {
                        // 这一层只是正文的字号/行高作用域(thinking、工具簇各有自己的字阶,
                        // 不能提到 col 上);col 是 w_full 定宽列,不会走 max-content 探测,
                        // 与用户气泡"不加包装层"的约束不冲突
                        col = col.child(
                            v_flex()
                                .gap(SPACE_SM)
                                .text_size(FONT_MSG_BODY)
                                .line_height(relative(1.9))
                                .children(parts),
                        );
                    }
                    if !m.tool_calls.is_empty() {
                        col = col.child(tool_cluster(
                            ix,
                            &m.tool_calls,
                            tool_arg_cells,
                            tools_open,
                            cx.listener(move |this, _, _window, cx| {
                                if let Some(detail) = &mut this.detail {
                                    toggle_expanded_row(&mut detail.expanded_tools, ix);
                                    // 行高随展开变化,让 list 重测该行
                                    detail.msg_list.splice(ix..ix + 1, 1);
                                }
                                cx.notify();
                            }),
                            cx,
                        ));
                    }
                    col.into_any_element()
                }
                MessageRole::System => centered_pill(one_line(&m.text, 120), cx).into_any_element(),
            }
        };

        div()
            .w_full()
            .flex()
            .justify_center()
            // 与详情头共用 24px 阅读轴，正文、标题和元信息形成一条竖线。
            .px(SPACE_XXL)
            .py(SPACE_SM)
            .when(ix == 0, |d| d.pt(SPACE_LG))
            .when(ix + 1 == total, |d| d.pb(SPACE_XXL))
            .child(
                div()
                    .w_full()
                    .max_w(READER_MAX_WIDTH)
                    .min_w_0()
                    // 淡 primary 底:标记搜索命中落点(尾部命中不滚动,全靠它识别)。
                    // 负 margin + 等量 padding:背景向外扩出呼吸边,内容原位不推挤,
                    // 与相邻消息的对齐和行距都不变
                    .when(is_jump_target, |d| {
                        d.rounded(jump_radius)
                            .bg(jump_bg)
                            .mx(-SPACE_SM)
                            .px(SPACE_SM)
                            .my(-SPACE_XS)
                            .py(SPACE_XS)
                    })
                    .child(inner),
            )
            .into_any_element()
    }

    // ---------------- Insights ----------------

    /// Insights 整页(替换中栏+右栏)。头部沿用中栏 88px 标题节奏兼窗口
    /// 拖拽区;内容 720px 阅读宽居中,区块只用留白与组头分隔,不做卡片墙
    fn render_insights(&self, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        // 副标题只说 "Since {首会话月份}"(用户定稿);拿不到时间时回落默认句
        let subtitle: SharedString = match &self.insights {
            Some(d) if d.sessions > 0 => match month_year(d.first_ts) {
                my if my.is_empty() => "Your coding agent activity".into(),
                my => format!("Since {my}").into(),
            },
            _ => "Your coding agent activity".into(),
        };

        let body: AnyElement = match &self.insights {
            Some(d) if d.sessions > 0 => self.render_insights_content(d, cx),
            _ if self.insights_loading => div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .child(Spinner::new())
                .into_any_element(),
            _ => v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .child(empty_state_card(
                    "icons/chart-column.svg",
                    px(58.),
                    px(24.),
                    "No activity yet",
                    "Refresh sessions to see your activity here.",
                    cx,
                ))
                .into_any_element(),
        };

        v_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .bg(theme.background)
            .child(
                v_flex()
                    .id("insights-header")
                    .w_full()
                    .h(LIBRARY_IDENTITY_HEIGHT)
                    .flex_shrink_0()
                    .window_control_area(WindowControlArea::Drag)
                    .px(SPACE_XXL)
                    .justify_center()
                    .child(
                        v_flex()
                            .gap(px(2.))
                            .child(
                                div()
                                    .text_size(FONT_TITLE)
                                    .font_semibold()
                                    .child("Insights"),
                            )
                            .child(
                                div()
                                    .text_size(FONT_LABEL)
                                    .text_color(theme.muted_foreground)
                                    .child(subtitle),
                            ),
                    ),
            )
            .child(body)
            .into_any_element()
    }

    fn render_insights_content(&self, d: &InsightsData, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();

        // ---- 概览大数字行 ----
        let stat = |value: String, label: &'static str| {
            v_flex()
                .gap(px(2.))
                .child(
                    div()
                        .text_size(FONT_TITLE)
                        .font_semibold()
                        .text_color(theme.foreground)
                        .child(value),
                )
                .child(
                    div()
                        .text_size(FONT_CAPTION)
                        .text_color(theme.muted_foreground)
                        .child(label),
                )
        };
        // 序:Sessions / Tokens / Prompts / Agents / Projects / Active days
        // (用户钉的)
        let overview = h_flex()
            .gap(px(40.))
            .child(stat(thousands(d.sessions), "Sessions"))
            .when(d.tokens > 0, |row| {
                row.child(stat(fmt_tokens(Some(d.tokens)), "Tokens"))
            })
            .child(stat(thousands(d.prompts), "Prompts"))
            .child(stat(thousands(d.agents.len() as i64), "Agents"))
            .child(stat(thousands(d.project_count), "Projects"))
            .child(stat(thousands(d.active_days()), "Active days"));

        // ---- 三个榜单的行首/名称(闭包只捕获 Copy 的色值) ----
        let dark = theme.mode.is_dark();
        let muted = theme.muted_foreground;
        let agent_head = move |u: &UsageTally| match AgentId::from_str(&u.name) {
            Some(agent) => (
                Some(img(agent.brand_icon(dark)).size(px(15.)).into_any_element()),
                agent.display_name().into(),
            ),
            // 库里出现未知 agent_id(降级防御):无图标裸名,比整行消失诚实
            None => (None, u.name.clone().into()),
        };
        let project_head = move |u: &UsageTally| {
            (
                Some(
                    icon("icons/folder.svg")
                        .with_size(px(14.))
                        .text_color(muted)
                        .into_any_element(),
                ),
                u.name.clone().into(),
            )
        };
        let model_head = |u: &UsageTally| (None, u.name.clone().into());

        div()
            .id("insights-scroll")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .child(
                div().w_full().flex().justify_center().px(SPACE_XXL).child(
                    v_flex()
                        .w_full()
                        .max_w(READER_MAX_WIDTH)
                        .pt(SPACE_SM)
                        .pb(px(40.))
                        .gap(INSIGHTS_SECTION_GAP)
                        .child(overview)
                        .child(
                            v_flex()
                                .gap(SPACE_MD)
                                .child(switch_section_head(
                                    "Activity",
                                    Some("Prompts you sent, day by day".into()),
                                    None,
                                    cx,
                                ))
                                .child(render_heatmap(d, cx)),
                        )
                        .child(self.render_distribution_section(d, cx))
                        .child(self.render_usage_section(
                            UsageBoard::Agents,
                            &d.agents,
                            agent_head,
                            cx,
                        ))
                        .when(!d.projects.is_empty(), |col| {
                            col.child(self.render_usage_section(
                                UsageBoard::Projects,
                                &d.projects,
                                project_head,
                                cx,
                            ))
                        })
                        .when(!d.models.is_empty(), |col| {
                            col.child(self.render_usage_section(
                                UsageBoard::Models,
                                &d.models,
                                model_head,
                                cx,
                            ))
                        }),
                ),
            )
            .into_any_element()
    }

    /// 分布区块:标题右侧 ‹ › 在 hour/weekday/month 三个维度间循环切换
    fn render_distribution_section(&self, d: &InsightsData, cx: &Context<Self>) -> AnyElement {
        let range = self.insights_range;
        let values: &[i64] = match range {
            InsightsRange::Hour => &d.hourly,
            InsightsRange::Weekday => &d.weekday,
            InsightsRange::Month => &d.monthly,
        };
        // 峰值只算这一次:caption 点名的与图里高亮的必须是同一根柱
        let (peak, peak_n) = values
            .iter()
            .enumerate()
            .max_by_key(|(_, n)| **n)
            .map(|(i, n)| (i, *n))
            .unwrap_or((0, 0));
        let arrows = insight_arrows(
            "dist-arrow",
            None,
            cx.listener(move |this, _, _window, cx| {
                this.insights_range = this.insights_range.prev();
                cx.notify();
            }),
            cx.listener(move |this, _, _window, cx| {
                this.insights_range = this.insights_range.next();
                cx.notify();
            }),
            cx,
        );
        v_flex()
            .gap(SPACE_MD)
            .child(switch_section_head(
                range.title(),
                Some(dist_caption(range, peak, peak_n).into()),
                Some(arrows.into_any_element()),
                cx,
            ))
            .child(render_distribution(range, values, peak, cx))
            .into_any_element()
    }

    /// 榜单区块(Agents/Projects/Models 同构):‹ 度量名 › 循环切换,行按
    /// 当前度量降序重排再截断 top-N。可用档位就是一个切片:组内无人报
    /// token 时 Tokens 不在其中,归一、循环、行过滤都从这一个事实推出
    fn render_usage_section(
        &self,
        board: UsageBoard,
        rows: &[UsageTally],
        row_head: impl Fn(&UsageTally) -> (Option<AnyElement>, SharedString),
        cx: &Context<Self>,
    ) -> AnyElement {
        use InsightsMetric::*;
        let has_tokens = rows.iter().any(|u| u.tokens > 0);
        // 循环序与概览行同:Sessions / Tokens / Prompts(用户钉的)
        let avail: &[InsightsMetric] = if has_tokens {
            &[Sessions, Tokens, Prompts]
        } else {
            &[Sessions, Prompts]
        };
        let slot = board as usize;
        // position 找不到 = 存的档位已不可用(如数据刷新后 token 清零),
        // 落回首档;循环即可用档位上的环形下标
        let i = avail
            .iter()
            .position(|m| *m == self.insights_metrics[slot])
            .unwrap_or(0);
        let metric = avail[i];
        let to_prev = avail[(i + avail.len() - 1) % avail.len()];
        let to_next = avail[(i + 1) % avail.len()];
        let arrows = insight_arrows(
            board.arrow_id(),
            Some(metric.caption().into()),
            cx.listener(move |this, _, _window, cx| {
                this.insights_metrics[slot] = to_prev;
                cx.notify();
            }),
            cx.listener(move |this, _, _window, cx| {
                this.insights_metrics[slot] = to_next;
                cx.notify();
            }),
            cx,
        );
        let mut sorted: Vec<&UsageTally> = rows
            .iter()
            // Tokens 档只列报了用量的组:空条不是"用了 0",是没数据
            .filter(|u| metric != Tokens || u.tokens > 0)
            .collect();
        // stable sort:平局保持 SQL 的 sessions desc + 名称序
        sorted.sort_by_key(|u| std::cmp::Reverse(metric.value(u)));
        sorted.truncate(board.limit());
        let max = sorted.iter().map(|u| metric.value(u)).max().unwrap_or(1);
        v_flex()
            .gap(SPACE_MD)
            .child(switch_section_head(
                board.title(),
                None,
                Some(arrows.into_any_element()),
                cx,
            ))
            .children(sorted.into_iter().map(|u| {
                let (lead, label) = row_head(u);
                usage_bar_row(
                    lead,
                    label,
                    metric.display(u).into(),
                    metric.value(u),
                    max,
                    board.name_w(),
                    cx,
                )
            }))
            .into_any_element()
    }

    #[allow(deprecated)]
    fn update_detail_selection_auto_scroll(
        &mut self,
        event: &MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let has_selection = event.dragging() && window.has_text_selection(cx);
        let delta = self.detail.as_ref().and_then(|detail| {
            let bounds = detail.msg_list.viewport_bounds();
            (has_selection
                && !detail.msg_list.is_scrollbar_dragging()
                && event.position.x >= bounds.left()
                && event.position.x <= bounds.right())
            .then(|| AutoScroll::compute_delta(event.position.y, bounds))
            .flatten()
        });

        self.detail_selection_auto_scroll
            .set(delta, cx, |delta, this, cx| {
                let Some(detail) = &this.detail else {
                    return;
                };
                detail.msg_list.scroll_by(delta);
                cx.notify();
            });
    }

    fn stop_detail_selection_auto_scroll(&mut self) {
        self.detail_selection_auto_scroll.stop();
    }

    fn render_detail(&self, _window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let Some(detail) = &self.detail else {
            return v_flex()
                .flex_1()
                .h_full()
                .items_center()
                .justify_center()
                .bg(theme.background)
                .child(empty_state_card(
                    "icons/message-square.svg",
                    px(58.),
                    px(26.),
                    "No session selected",
                    format!(
                        "Pick one from the list, or press {} to search.",
                        search_key_hint()
                    ),
                    cx,
                ))
                .into_any_element();
        };
        let meta = &detail.meta;
        let detail_title_tooltip: SharedString = meta.title.clone().into();
        let session_id = meta.id.clone();
        let export_entity = cx.entity();
        let reveal_entity = export_entity.clone();
        let delete_entity = export_entity.clone();
        let more_menu = Button::new("more-actions")
            .ghost()
            .rounded(RADIUS_BUTTON)
            .icon(icon("icons/more-horizontal.svg").with_size(px(16.)))
            .dropdown_menu(move |menu, _, cx| {
                let export_entity = export_entity.clone();
                let reveal_entity = reveal_entity.clone();
                let delete_entity = delete_entity.clone();
                menu.min_w(px(210.))
                    .item(
                        PopupMenuItem::new(" Export as Markdown")
                            .icon(icon("icons/download.svg").with_size(px(15.)))
                            .on_click(move |_, window, cx| {
                                export_entity.update(cx, |this, cx| {
                                    this.do_export(window, cx);
                                });
                            }),
                    )
                    .item(
                        PopupMenuItem::new(format!(" {REVEAL_IN_FM}"))
                            .icon(icon("icons/folder.svg").with_size(px(15.)))
                            .on_click(move |_, _, cx| {
                                reveal_entity.update(cx, |this, _| {
                                    if let Some(detail) = &this.detail {
                                        terminal::reveal_in_file_manager(&detail.meta.file_path);
                                    }
                                });
                            }),
                    )
                    .item(
                        PopupMenuItem::new(" Copy Session ID")
                            .icon(icon("icons/copy.svg").with_size(px(15.)))
                            .on_click({
                                let id = session_id.clone();
                                move |_, _, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(id.clone()));
                                }
                            }),
                    )
                    .separator()
                    .item(
                        PopupMenuItem::element(|_, cx| {
                            div().text_color(cx.theme().danger).child(MOVE_TO_TRASH)
                        })
                        .icon(
                            icon("icons/trash-2.svg")
                                .with_size(px(15.))
                                .text_color(cx.theme().danger),
                        )
                        .on_click(move |_, window, cx| {
                            delete_entity.update(cx, |this, cx| {
                                this.confirm_delete(window, cx);
                            });
                        }),
                    )
            })
            .anchor(Anchor::TopRight);

        let detail_path: SharedString = if meta.project_path.is_empty() {
            "Unknown project".to_string()
        } else {
            meta.project_path.clone()
        }
        .into();
        let mut detail_facts: Vec<String> = Vec::new();
        if meta.message_count > 0 {
            detail_facts.push(format!("{} messages", meta.message_count));
        }
        if let Some(tokens) = meta.tokens_used {
            detail_facts.push(format!("{} tokens", fmt_tokens(Some(tokens))));
        }
        let has_detail_facts = !detail_facts.is_empty();
        let detail_fact_line: SharedString = detail_facts.join(" · ").into();
        let created_time: Option<(SharedString, SharedString)> = (meta.created_at > 0).then(|| {
            (
                format!("Created {}", smart_time(meta.created_at)).into(),
                format!("Created {}", abs_date(meta.created_at)).into(),
            )
        });
        let updated_time: Option<(SharedString, SharedString)> = (meta.updated_at > 0).then(|| {
            (
                format!("Updated {}", smart_time(meta.updated_at)).into(),
                format!("Updated {}", abs_date(meta.updated_at)).into(),
            )
        });
        let branch: Option<SharedString> =
            visible_git_branch(meta.git_branch.as_deref()).map(|branch| branch.to_string().into());
        let project_badge: AnyElement = {
            let badge = badge(
                meta.project_name.clone(),
                theme.muted,
                theme.muted_foreground,
            );
            if meta.project_path.is_empty() {
                div().child(badge).into_any_element()
            } else {
                let project_path = meta.project_path.clone();
                div()
                    .id("detail-project")
                    .cursor_pointer()
                    .tooltip(|window, cx| {
                        gpui_component::tooltip::Tooltip::new(SHOW_IN_FM).build(window, cx)
                    })
                    .on_click(move |_, _, _| terminal::open_in_file_manager(&project_path))
                    .child(badge)
                    .into_any_element()
            }
        };

        v_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .bg(theme.background)
            .child(
                v_flex()
                    .id("detail-header")
                    .flex_shrink_0()
                    .window_control_area(WindowControlArea::Drag)
                    .px(SPACE_XXL)
                    .pb(SPACE_SM)
                    .border_b_1()
                    .border_color(theme.border)
                    .child(
                        h_flex()
                            .w_full()
                            .h(WINDOW_TITLEBAR_HEIGHT)
                            .items_center()
                            .justify_between()
                            .gap(SPACE_MD)
                            .child(
                                h_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .gap(SPACE_SM)
                                    .items_center()
                                    .text_size(FONT_LABEL)
                                    .text_color(theme.muted_foreground)
                                    .child(img(meta.agent.brand_icon(theme.mode.is_dark())).size(px(15.)).flex_shrink_0())
                                    .child(div().flex_shrink_0().child(meta.agent.display_name()))
                                    .child(project_badge)
                                    .when_some(branch, |this, branch| {
                                        this.child(
                                            h_flex()
                                                .min_w_0()
                                                .gap(SPACE_XS)
                                                .child(icon("icons/git-branch.svg").with_size(px(11.)).flex_shrink_0())
                                                .child(div().min_w_0().truncate().child(branch)),
                                        )
                                    }),
                            )
                            .child(
                                h_flex()
                                    .flex_shrink_0()
                                    .gap(SPACE_XS)
                                    .child({
                                        // Open In split 按钮(Codex/kooky 风):左段 = 上次
                                        // 目标的应用图标,点击直开;右段 chevron 展开列表。
                                        // 目标列表按 agent 过滤(Kooky 深链不认 dsh);
                                        // 偏好目标不在列表时(如 dsh 会话 + 偏好 Kooky)回退首项
                                        let terms = terminal::terminals_for(meta.agent);
                                        let current = self.open_in_target(meta.agent, &terms);
                                        let current_icon = current
                                            .and_then(|t| self.terminal_icons.get(t.id()).cloned());
                                        let term_items: Vec<(terminal::TerminalApp, Option<PathBuf>)> =
                                            terms
                                                .iter()
                                                .map(|t| {
                                                    (*t, self.terminal_icons.get(t.id()).cloned())
                                                })
                                                .collect();
                                        let menu_entity = cx.entity();
                                        // 无常显分隔线,hover 分段高亮暗示两段(Codex 同款);
                                        // 右段 Button 用 custom variant 与左段 hover 完全一致
                                        h_flex()
                                            .h(px(28.))
                                            .rounded(RADIUS_BUTTON)
                                            .border_1()
                                            .border_color(theme.border)
                                            .bg(theme.secondary)
                                            .overflow_hidden()
                                            .child(
                                                div()
                                                    .id("open-in-main")
                                                    .h_full()
                                                    .px(px(7.))
                                                    .flex()
                                                    .items_center()
                                                    .cursor_pointer()
                                                    .hover(|s| s.bg(theme.secondary_hover))
                                                    .active(|s| s.bg(theme.secondary_active))
                                                    .child(open_in_icon(
                                                        current,
                                                        current_icon.as_ref(),
                                                        icon("icons/terminal.svg")
                                                            .with_size(px(13.))
                                                            .text_color(theme.secondary_foreground),
                                                    ))
                                                    .tooltip({
                                                        let label: SharedString = match current {
                                                            Some(t) => format!("Open this session in {}", t.display_name()).into(),
                                                            None => "Open this session".into(),
                                                        };
                                                        move |window, cx| {
                                                            gpui_component::tooltip::Tooltip::new(label.clone()).build(window, cx)
                                                        }
                                                    })
                                                    .on_click(cx.listener(move |this, _, window, cx| {
                                                        if let Some(term) = current {
                                                            this.do_resume(term, false, window, cx);
                                                        } else {
                                                            // 空列表在 macOS 不可能(Terminal.app 恒在),
                                                            // Windows/Linux 上 PATH 被启动器改写时会发生
                                                            // ——静默无操作是死按钮,至少说一声为什么
                                                            window.push_notification(
                                                                Notification::warning(
                                                                    "No terminal application found on PATH",
                                                                ),
                                                                cx,
                                                            );
                                                        }
                                                    })),
                                            )
                                            .child(
                                                div()
                                                    .w(px(1.))
                                                    .h_full()
                                                    .flex_shrink_0()
                                                    .bg(theme.border),
                                            )
                                            .child(
                                                Button::new("open-in-more")
                                                    .custom(
                                                        ButtonCustomVariant::new(cx)
                                                            .foreground(theme.muted_foreground)
                                                            .hover(theme.secondary_hover)
                                                            .active(theme.secondary_active),
                                                    )
                                                    .rounded(px(0.))
                                                    .h(px(26.))
                                                    .w(px(22.))
                                                    .icon(
                                                        icon("icons/chevron-down.svg")
                                                            .with_size(px(12.)),
                                                    )
                                                    .tooltip("Open this session in…")
                                                    .dropdown_menu(move |menu, _, _| {
                                                        let mut menu = menu.min_w(px(170.));
                                                        for (term, icon_path) in term_items.clone() {
                                                            let entity = menu_entity.clone();
                                                            menu = menu.item(
                                                                PopupMenuItem::element(move |_, _| {
                                                                    h_flex()
                                                                        .gap(SPACE_SM)
                                                                        .items_center()
                                                                        .child(open_in_icon(
                                                                            Some(term),
                                                                            icon_path.as_ref(),
                                                                            icon("icons/terminal.svg")
                                                                                .with_size(px(15.)),
                                                                        ))
                                                                        .child(term.display_name())
                                                                })
                                                                .on_click(move |_, window, cx| {
                                                                    entity.update(cx, |this, cx| {
                                                                        this.do_resume(term, true, window, cx);
                                                                    });
                                                                }),
                                                            );
                                                        }
                                                        menu
                                                    })
                                                    .anchor(Anchor::TopRight),
                                            )
                                    })
                                    .child(tool_btn(
                                        "fav",
                                        "icons/star.svg",
                                        "icons/star-filled.svg",
                                        rgb(crate::theme::STAR_YELLOW).into(),
                                        if meta.favorite {
                                            "Unstar"
                                        } else {
                                            "Star"
                                        },
                                        meta.favorite,
                                        cx.listener(|this, _, window, cx| {
                                            this.toggle_favorite(window, cx)
                                        }),
                                    ))
                                    .child(tool_btn(
                                        "pin",
                                        "icons/pin.svg",
                                        "icons/pin-filled.svg",
                                        theme.primary,
                                        if meta.pinned {
                                            "Unpin"
                                        } else {
                                            "Pin"
                                        },
                                        meta.pinned,
                                        cx.listener(|this, _, window, cx| {
                                            this.toggle_pinned(window, cx)
                                        }),
                                    ))
                                    .child(more_menu),
                            ),
                    )
                    .child(
                        div()
                            .w_full()
                            .min_h(WINDOW_TITLEBAR_HEIGHT)
                            .min_w_0()
                            .py(SPACE_SM)
                            .flex()
                            .items_center()
                            .child(
                                div()
                                    .id("detail-title")
                                    .w_full()
                                    .min_w_0()
                                    .whitespace_normal()
                                    .text_size(FONT_TITLE)
                                    .line_height(relative(1.15))
                                    .font_semibold()
                                    .child(meta.title.clone())
                                    .tooltip(move |window, cx| {
                                        gpui_component::tooltip::Tooltip::new(
                                            detail_title_tooltip.clone(),
                                        )
                                        .build(window, cx)
                                    }),
                            ),
                    )
                    .child(
                        v_flex()
                            .w_full()
                            .min_w_0()
                            .pt(SPACE_SM)
                            .gap(SPACE_SM)
                            .text_size(FONT_LABEL)
                            .text_color(theme.muted_foreground)
                            .child(
                                h_flex()
                                    .w_full()
                                    .min_w_0()
                                    .gap(SPACE_MD)
                                    .items_center()
                                    .when_some(meta.model.clone(), |this, model| {
                                        this.child(outline_badge(
                                            model,
                                            rgb(crate::theme::MODEL_BADGE_BG).into(),
                                        ))
                                    })
                                    .when_some(
                                        meta.source.clone().filter(|s| !s.is_empty()),
                                        |this, source| {
                                            let color = if source == "opencode2" {
                                                theme.primary
                                            } else {
                                                theme.success
                                            };
                                            this.child(outline_badge(source, color))
                                        },
                                    )
                                    .when(has_detail_facts, |this| {
                                        this.child(
                                            div()
                                                .flex_1()
                                                .min_w_0()
                                                .truncate()
                                                .child(detail_fact_line),
                                        )
                                    }),
                            )
                            .child(
                                h_flex()
                                    .min_w_0()
                                    .gap(px(6.))
                                    .child(
                                        icon("icons/folder.svg")
                                            .with_size(px(12.))
                                            .flex_shrink_0(),
                                    )
                                    .child(div().min_w_0().truncate().child(detail_path)),
                            )
                            .when(created_time.is_some() || updated_time.is_some(), |this| {
                                this.child(
                                    h_flex()
                                        .w_full()
                                        .min_w_0()
                                        .gap(px(6.))
                                        .items_center()
                                        .child(
                                            icon("icons/calendar.svg")
                                                .with_size(px(12.))
                                                .flex_shrink_0(),
                                        )
                                        .child(
                                            h_flex()
                                                .min_w_0()
                                                .gap(SPACE_MD)
                                                .when_some(
                                                    created_time.clone(),
                                                    |row, (created, tooltip)| {
                                                        row.child(
                                                            div()
                                                                .id("detail-created-time")
                                                                .min_w_0()
                                                                .truncate()
                                                                .child(created)
                                                                .tooltip(move |window, cx| {
                                                                    gpui_component::tooltip::Tooltip::new(
                                                                        tooltip.clone(),
                                                                    )
                                                                    .build(window, cx)
                                                                }),
                                                        )
                                                    },
                                                )
                                                .when(
                                                    created_time.is_some()
                                                        && updated_time.is_some(),
                                                    |row| {
                                                        row.child(
                                                            div()
                                                                .flex_shrink_0()
                                                                .text_color(theme.border)
                                                                .child("·"),
                                                        )
                                                    },
                                                )
                                                .when_some(
                                                    updated_time.clone(),
                                                    |row, (updated, tooltip)| {
                                                        row.child(
                                                            div()
                                                                .id("detail-updated-time")
                                                                .flex_shrink_0()
                                                                .child(updated)
                                                                .tooltip(move |window, cx| {
                                                                    gpui_component::tooltip::Tooltip::new(
                                                                        tooltip.clone(),
                                                                    )
                                                                    .build(window, cx)
                                                                }),
                                                        )
                                                    },
                                                ),
                                        ),
                                )
                            }),
                    ),
            )
            .child(if detail.loading {
                h_flex()
                    .flex_1()
                    .bg(theme.popover)
                    .items_center()
                    .justify_center()
                    .gap(SPACE_SM)
                    .text_color(theme.muted_foreground)
                    .child(Spinner::new().small())
                    .child(div().text_size(FONT_BODY).child("Loading session…"))
                    .into_any_element()
            } else if let Some(error) = detail.error.clone() {
                let reveal_path = meta.file_path.clone();
                v_flex()
                    .flex_1()
                    .bg(theme.popover)
                    .items_center()
                    .justify_center()
                    .px(SPACE_XXL)
                    .child(
                        v_flex()
                            .w_full()
                            .max_w(px(520.))
                            .items_center()
                            .gap(SPACE_MD)
                            .child(
                                icon("icons/circle-x.svg")
                                    .with_size(px(24.))
                                    .text_color(theme.danger),
                            )
                            .child(
                                div()
                                    .text_size(FONT_HEADING)
                                    .font_semibold()
                                    .text_color(theme.foreground)
                                    .child("Couldn't load this session"),
                            )
                            .child(
                                div()
                                    .w_full()
                                    .whitespace_normal()
                                    .text_center()
                                    .text_size(FONT_CAPTION)
                                    .text_color(theme.muted_foreground)
                                    .child(error),
                            )
                            .child(
                                Button::new("reveal-failed-session")
                                    .outline()
                                    .small()
                                    .rounded(RADIUS_BUTTON)
                                    .icon(icon("icons/folder.svg").with_size(px(13.)))
                                    .label(REVEAL_IN_FM)
                                    .on_click(move |_, _, _| {
                                        terminal::reveal_in_file_manager(&reveal_path)
                                    }),
                            ),
                    )
                    .into_any_element()
            } else {
                let entity = cx.entity().downgrade();
                div()
                    .flex_1()
                    .min_h_0()
                    .bg(theme.popover)
                    .relative()
                    .child(
                        gpui::list(detail.msg_list.clone(), move |ix, window, cx| {
                            entity
                                .upgrade()
                                .map(|e| {
                                    e.update(cx, |this, cx| this.render_msg_row(ix, window, cx))
                                })
                                .unwrap_or_else(|| div().into_any_element())
                        })
                        .size_full(),
                    )
                    .vertical_scrollbar(&detail.msg_list)
                    .into_any_element()
            })
            .into_any_element()
    }
}

/// 可折叠分组头:chevron + 文字,点击切换
fn group_header(
    id: &'static str,
    text: &'static str,
    collapsed: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    cx: &Context<Workbench>,
) -> Stateful<Div> {
    let theme = cx.theme();
    div()
        .id(id)
        .flex_shrink_0()
        .pl(GROUP_HEAD_INSET)
        .pr(SIDEBAR_EDGE)
        .pt(SPACE_MD)
        .pb(SPACE_XS)
        .cursor_pointer()
        .active(|s| s.opacity(0.7))
        .on_click(on_click)
        .child(
            h_flex()
                .gap(SPACE_XS)
                // 与主导航行同字号同字重(FONT_BODY / 常规),仅靠 muted 色
                // 与"无行首图标"区分——加粗会让组头压过它统辖的行
                .text_size(FONT_BODY)
                .text_color(theme.muted_foreground)
                .hover(|s| s.text_colored(theme.foreground, FONT_BODY))
                .child(text)
                .child(
                    icon("icons/chevron-right.svg")
                        .with_size(px(13.))
                        .when(!collapsed, |ic| {
                            ic.rotate(gpui::Radians(std::f32::consts::FRAC_PI_2))
                        }),
                ),
        )
}

/// 侧栏行层级:Primary=固定主导航(32px/FONT_BODY),Sub=分组展开项(26px/FONT_CAPTION)。
/// 行首元素一律对齐同一条中轴(见 ui.rs LEAD_BOX),子级不再缩进——
/// 行高与字号是仅剩的层级来源,禁止把子级行改回主导航尺度。
#[derive(Clone, Copy, PartialEq)]
enum RowLevel {
    Primary,
    Sub,
}

/// 侧栏行首元素——每行必须有一个,槽位定宽,保证同组文字起点对齐。
/// Lucide 图标随选中态着色;品牌 PNG 保留原色不着色。
enum RowLead {
    Icon(Icon),
    /// Agent 品牌图标,取 `AgentId::brand_icon()`
    Brand(&'static str),
}

// ---------------- Insights 附件 ----------------

/// 可切换区块共用的 ‹ › ghost 按钮组。label 显示在两键中间(当前档位名),
/// 定宽居中让按钮位置不随文本长短跳动;分布图的档位名就是区块标题,传 None
fn insight_arrows(
    id: &'static str,
    label: Option<SharedString>,
    on_prev: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    on_next: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    cx: &App,
) -> impl IntoElement {
    let theme = cx.theme();
    h_flex()
        .flex_shrink_0()
        .gap(SPACE_XS)
        .child(
            Button::new((id, 0usize))
                .ghost()
                .rounded(RADIUS_BUTTON)
                .icon(icon("icons/chevron-left.svg").with_size(px(14.)))
                .tooltip("Previous view")
                .on_click(on_prev),
        )
        .when_some(label, |row, label| {
            row.child(
                div()
                    .w(px(64.))
                    .flex()
                    .justify_center()
                    .whitespace_nowrap()
                    .text_size(FONT_CAPTION)
                    .text_color(theme.muted_foreground)
                    .child(label),
            )
        })
        .child(
            Button::new((id, 1usize))
                .ghost()
                .rounded(RADIUS_BUTTON)
                .icon(icon("icons/chevron-right.svg").with_size(px(14.)))
                .tooltip("Next view")
                .on_click(on_next),
        )
}

/// Insights 区块头:标题 + 可选 caption + 可选右上角切换按钮组。
/// caption 有则双行(按钮对齐首行),无则单行居中对齐
fn switch_section_head(
    title: &'static str,
    caption: Option<SharedString>,
    arrows: Option<AnyElement>,
    cx: &App,
) -> impl IntoElement {
    let theme = cx.theme();
    let two_line = caption.is_some();
    h_flex()
        .justify_between()
        .map(|head| {
            if two_line {
                head.items_start()
            } else {
                head.items_center()
            }
        })
        .child(
            v_flex()
                .gap(px(2.))
                .child(
                    div()
                        .text_size(FONT_BODY)
                        .font_semibold()
                        .text_color(theme.foreground)
                        .child(title),
                )
                .when_some(caption, |head, caption| {
                    head.child(
                        div()
                            .text_size(FONT_CAPTION)
                            .text_color(theme.muted_foreground)
                            .child(caption),
                    )
                }),
        )
        .when_some(arrows, |head, arrows| head.child(arrows))
}

/// 榜单行:行首 + 名称 + 轨道条 + 计数。value_text 与 count 分开传:
/// Tokens 档显示 "1.2M" 缩写,条仍按原值归一
fn usage_bar_row(
    lead: Option<AnyElement>,
    label: SharedString,
    value_text: SharedString,
    count: i64,
    max: i64,
    name_w: Pixels,
    cx: &App,
) -> impl IntoElement {
    let theme = cx.theme();
    let frac = (count as f32 / max.max(1) as f32).clamp(0., 1.);
    h_flex()
        .h(SPACE_XXL)
        .gap(SPACE_SM)
        .items_center()
        .when_some(lead, |row, lead| {
            row.child(
                div()
                    .w(px(15.))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(lead),
            )
        })
        .child(
            div()
                .w(name_w)
                .flex_shrink_0()
                .min_w_0()
                .text_size(FONT_CAPTION)
                .text_color(theme.foreground)
                .truncate()
                .child(label),
        )
        .child(
            div()
                .flex_1()
                .h(px(6.))
                .rounded_full()
                .bg(theme.muted)
                .child(
                    div()
                        .h_full()
                        .w(relative(frac))
                        .rounded_full()
                        .bg(theme.primary),
                ),
        )
        .child(
            div()
                .w(px(56.))
                .flex_shrink_0()
                .flex()
                .justify_end()
                .text_size(FONT_LABEL)
                .text_color(theme.muted_foreground)
                .child(value_text),
        )
}

fn prompts_label(n: i64) -> String {
    match n {
        0 => "No prompts".into(),
        1 => "1 prompt".into(),
        n => format!("{} prompts", thousands(n)),
    }
}

/// 0–23 时 → "12 AM" / "2 PM" 形制(与 smart_time 的 12 小时制一致)
fn hour_label(h: usize) -> String {
    match h % 24 {
        0 => "12 AM".into(),
        12 => "12 PM".into(),
        h if h < 12 => format!("{h} AM"),
        h => format!("{} PM", h - 12),
    }
}

const DOW_SHORT: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
const DOW_PLURAL: [&str; 7] = [
    "Mondays",
    "Tuesdays",
    "Wednesdays",
    "Thursdays",
    "Fridays",
    "Saturdays",
    "Sundays",
];
const MONTH_SHORT: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];
const MONTH_FULL: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

/// peak 由 render_distribution_section 算一次传入——caption 点名的与
/// 图里高亮的必须是同一根柱
fn dist_caption(range: InsightsRange, peak: usize, peak_n: i64) -> String {
    if peak_n == 0 {
        return "When you talk to your agents".into();
    }
    match range {
        InsightsRange::Hour => format!("Most active around {}", hour_label(peak)),
        InsightsRange::Weekday => format!("Most active on {}", DOW_PLURAL[peak]),
        InsightsRange::Month => format!("Most active in {}", MONTH_FULL[peak]),
    }
}

/// 竖柱分布图(hour 24 根 / weekday 7 根 / month 12 根共用)。峰值柱全饱和
/// primary,其余 55%;零值留 2px muted 底座维持基线连续。宽柱配大缝:
/// 柱数越少 gap 越大,免得 7 根 90px 宽柱糊成一片
fn render_distribution(range: InsightsRange, values: &[i64], peak: usize, cx: &App) -> AnyElement {
    let theme = cx.theme();
    let max = values.iter().copied().max().unwrap_or(0).max(1);
    let gap = match range {
        InsightsRange::Hour => px(4.),
        InsightsRange::Weekday => px(8.),
        InsightsRange::Month => px(6.),
    };
    const CHART_H: f32 = 72.;
    v_flex()
        .gap(px(6.))
        .child(
            h_flex()
                .items_end()
                .gap(gap)
                .h(px(CHART_H))
                .children((0..values.len()).map(|i| {
                    let n = values[i];
                    let (height, bg) = if n == 0 {
                        (px(2.), theme.muted)
                    } else {
                        let frac = (n as f32 / max as f32).max(0.05);
                        (
                            px((frac * CHART_H).max(3.)),
                            if i == peak {
                                theme.primary
                            } else {
                                theme.primary.opacity(0.55)
                            },
                        )
                    };
                    let label: SharedString = match range {
                        InsightsRange::Hour => format!(
                            "{} · {} – {}",
                            prompts_label(n),
                            hour_label(i),
                            hour_label(i + 1)
                        ),
                        InsightsRange::Weekday => {
                            format!("{} · {}", prompts_label(n), DOW_PLURAL[i])
                        }
                        InsightsRange::Month => {
                            format!("{} · {}", prompts_label(n), MONTH_FULL[i])
                        }
                    }
                    .into();
                    div()
                        .id(("dist", i))
                        .flex_1()
                        .h(height)
                        .rounded(RADIUS_CELL)
                        .bg(bg)
                        .tooltip(move |window, cx| {
                            gpui_component::tooltip::Tooltip::new(label.clone()).build(window, cx)
                        })
                })),
        )
        .child(
            // 刻度行:与柱同宽的等分槽。hour 只标 6 小时锚点(文字溢出槽宽
            // 不裁剪),weekday/month 每柱都标
            h_flex()
                .gap(gap)
                .text_size(FONT_LABEL)
                .text_color(theme.muted_foreground)
                .children((0..values.len()).map(|i| {
                    let tick: Option<&'static str> = match range {
                        InsightsRange::Hour => None,
                        InsightsRange::Weekday => Some(DOW_SHORT[i]),
                        InsightsRange::Month => Some(MONTH_SHORT[i]),
                    };
                    div().flex_1().whitespace_nowrap().map(|slot| match tick {
                        // 每柱都有标签时与柱居中;hour 的稀疏锚点靠左
                        Some(t) => slot.flex().justify_center().child(t),
                        None if i % 6 == 0 => slot.child(hour_label(i)),
                        None => slot,
                    })
                })),
        )
        .into_any_element()
}

/// 热力图强度阶梯(primary 不透明度四档)。图例与格子同引本表,
/// 调阶只改这里
const HEAT: [f32; 4] = [0.25, 0.5, 0.75, 1.];

/// GitHub 风活跃热力图:53 周 × 7 天(周一起始),最右列为本周(d.as_of,
/// 与 streak 同一天,渲染层不再读时钟)。格子 9px:网格总宽 26 + 3 +
/// 53×9 + 52×3 = 662,必须收进最小窗口的内容宽 668(940 − 224 侧栏 −
/// 两侧 24 padding)——10px 格的 715 会在最小窗口被裁掉右缘
/// (2026-08-27 Codex review)。daily 升序,二分出窗口后填定长数组——
/// 渲染路径零哈希零日期运算;tooltip 文案 hover 才格式化
fn render_heatmap(d: &InsightsData, cx: &App) -> AnyElement {
    use chrono::Datelike as _;
    let theme = cx.theme();
    let today = d.as_of;
    let this_monday = today - chrono::Days::new(today.weekday().num_days_from_monday() as u64);
    let start = this_monday - chrono::Days::new(52 * 7);
    let today_ix = (today - start).num_days();

    const DAYS: usize = 53 * 7;
    let mut window = [0i64; DAYS];
    let mut heat_max = 1i64;
    let from = d.daily.partition_point(|(day, _)| *day < start);
    for &(day, n) in &d.daily[from..] {
        let ix = (day - start).num_days();
        if (0..DAYS as i64).contains(&ix) {
            window[ix as usize] = n;
            heat_max = heat_max.max(n);
        }
    }
    let heat_color = |n: i64| -> Hsla {
        if n == 0 {
            return theme.muted;
        }
        let quartile = ((n as f32 / heat_max as f32) * 4.).ceil().clamp(1., 4.) as usize;
        theme.primary.opacity(HEAT[quartile - 1])
    };
    const CELL: f32 = 9.;
    const GAP: f32 = 3.;
    const STEP: f32 = CELL + GAP;
    const DOW_W: f32 = 26.;

    // 月份标签:该列周一进入新月份时标注(与前一周比,首列同规则,
    // 相邻标签由此天然隔开 ≥4 列不会叠)
    let mut months = div()
        .relative()
        .w_full()
        .h(px(14.))
        .text_size(FONT_LABEL)
        .text_color(theme.muted_foreground);
    for c in 0..53u64 {
        let monday = start + chrono::Days::new(c * 7);
        if monday.month() != (monday - chrono::Days::new(7)).month() {
            months = months.child(
                div()
                    .absolute()
                    .top_0()
                    .left(px(DOW_W + GAP + c as f32 * STEP))
                    .child(MONTH_SHORT[monday.month0() as usize]),
            );
        }
    }

    // 星期标签列:行 r 的格子 y = r×STEP,文字行高 ≈13px,
    // (CELL−13)/2 = −2 光学对行
    let dow_col = div()
        .relative()
        .w(px(DOW_W))
        .h(px(7. * STEP - GAP))
        .flex_shrink_0()
        .text_size(FONT_LABEL)
        .text_color(theme.muted_foreground)
        .children([0usize, 2, 4].map(|r| {
            div()
                .absolute()
                .top(px(r as f32 * STEP - 2.))
                .left_0()
                .child(DOW_SHORT[r])
        }));

    let mut grid = h_flex().gap(px(GAP)).items_start().child(dow_col);
    for c in 0..53usize {
        let mut col = v_flex().gap(px(GAP));
        for r in 0..7usize {
            let ix = c * 7 + r;
            if ix as i64 > today_ix {
                col = col.child(div().size(px(CELL)));
                continue;
            }
            let n = window[ix];
            col = col.child(
                div()
                    .id(("hm", ix))
                    .size(px(CELL))
                    .rounded(RADIUS_CELL)
                    .bg(heat_color(n))
                    // 只捕获 Copy 的 (start, ix, n),hover 到的那格才格式化
                    .tooltip(move |window, cx| {
                        let day = start + chrono::Days::new(ix as u64);
                        let label = format!("{} · {}", prompts_label(n), day.format("%b %-d, %Y"));
                        gpui_component::tooltip::Tooltip::new(SharedString::from(label))
                            .build(window, cx)
                    }),
            );
        }
        grid = grid.child(col);
    }

    // 底注:streak/最忙一天(左) + Less…More 图例(右)
    let mut notes: Vec<String> = Vec::new();
    if d.current_streak > 0 {
        notes.push(format!("{}-day streak", d.current_streak));
    }
    if d.longest_streak > 0 {
        notes.push(format!("Longest {} days", d.longest_streak));
    }
    if let Some((day, n)) = d.busiest_day() {
        notes.push(format!(
            "Busiest {} ({})",
            day.format("%b %-d"),
            prompts_label(n)
        ));
    }
    let legend = h_flex()
        .justify_between()
        .items_center()
        .text_size(FONT_LABEL)
        .text_color(theme.muted_foreground)
        .child(div().min_w_0().truncate().child(notes.join(" · ")))
        .child(
            h_flex()
                .gap(px(GAP))
                .items_center()
                .flex_shrink_0()
                .child("Less")
                .children(std::iter::once(0.).chain(HEAT).map(|a: f32| {
                    div().size(px(CELL)).rounded(RADIUS_CELL).bg(if a == 0. {
                        theme.muted
                    } else {
                        theme.primary.opacity(a)
                    })
                }))
                .child("More"),
        );

    v_flex()
        .gap(px(6.))
        .child(months)
        .child(grid)
        .child(div().pt(SPACE_XS).child(legend))
        .into_any_element()
}

/// 阅读材质空态卡(360px `popover` 圆角面):详情空态与 Insights 空态共用,
/// 形制齐步走
fn empty_state_card(
    icon_path: &'static str,
    circle: Pixels,
    icon_size: Pixels,
    title: impl Into<SharedString>,
    caption: impl Into<SharedString>,
    cx: &App,
) -> Div {
    let theme = cx.theme();
    div()
        .w(px(360.))
        .px(SPACE_XXL)
        .py(SPACE_XXL)
        .rounded(theme.radius_lg)
        .bg(theme.popover)
        .child(empty_state(
            icon_path, circle, icon_size, title, caption, cx,
        ))
}

/// 空态占位(⌘K 初始 / 列表空 / 详情未选中共用):muted 圆底图标 + 标题 + 说明。
/// 圆径/图标径按场景传入,字阶与间距固定,保证三处视觉一致。
fn empty_state(
    icon_path: &'static str,
    circle: Pixels,
    icon_size: Pixels,
    title: impl Into<SharedString>,
    caption: impl Into<SharedString>,
    cx: &App,
) -> Div {
    let theme = cx.theme();
    v_flex()
        .items_center()
        .gap(SPACE_MD)
        .text_color(theme.muted_foreground)
        .child(
            div()
                .size(circle)
                .rounded_full()
                .bg(theme.muted)
                .flex()
                .items_center()
                .justify_center()
                .child(
                    icon(icon_path)
                        .with_size(icon_size)
                        .text_color(theme.muted_foreground),
                ),
        )
        .child(
            div()
                .text_size(FONT_BODY)
                .font_medium()
                .text_color(theme.foreground)
                .child(title.into()),
        )
        .child(div().text_size(FONT_CAPTION).child(caption.into()))
}

fn image_format_of(media_type: &str) -> Option<gpui::ImageFormat> {
    use gpui::ImageFormat as Format;
    Some(match media_type.trim().to_ascii_lowercase().as_str() {
        "image/png" => Format::Png,
        "image/jpeg" | "image/jpg" => Format::Jpeg,
        "image/webp" => Format::Webp,
        "image/gif" => Format::Gif,
        // SVG 的画布与滤镜开销无法在 GPUI 解码前可靠约束，保留原始下载即可。
        "image/svg+xml" => return None,
        "image/bmp" => Format::Bmp,
        "image/tiff" | "image/tif" => Format::Tiff,
        _ => return None,
    })
}

#[derive(Clone, Copy)]
struct ImageProbe {
    dims: (u32, u32),
    decoded_pixels: u64,
}

fn image_probe(bytes: &[u8], format: gpui::ImageFormat) -> Option<ImageProbe> {
    use image::AnimationDecoder as _;

    const MAX_DIMENSION: u32 = 8_192;
    const MAX_STATIC_PIXELS: u64 = 16_000_000;
    const MAX_ANIMATED_PIXELS: u64 = 32_000_000;
    const MAX_ANIMATION_FRAMES: usize = 120;

    let reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    let guessed = reader.format()?;
    let expected = match format {
        gpui::ImageFormat::Png => image::ImageFormat::Png,
        gpui::ImageFormat::Jpeg => image::ImageFormat::Jpeg,
        gpui::ImageFormat::Gif => image::ImageFormat::Gif,
        gpui::ImageFormat::Webp => image::ImageFormat::WebP,
        gpui::ImageFormat::Bmp => image::ImageFormat::Bmp,
        gpui::ImageFormat::Tiff => image::ImageFormat::Tiff,
        gpui::ImageFormat::Svg | gpui::ImageFormat::Ico | gpui::ImageFormat::Pnm => return None,
    };
    if guessed != expected {
        return None;
    }
    let dims = reader.into_dimensions().ok()?;
    if dims.0 == 0 || dims.1 == 0 || dims.0 > MAX_DIMENSION || dims.1 > MAX_DIMENSION {
        return None;
    }
    let pixels = u64::from(dims.0) * u64::from(dims.1);
    if pixels > MAX_STATIC_PIXELS {
        return None;
    }

    let frame_count = match format {
        gpui::ImageFormat::Gif => {
            let decoder = image::codecs::gif::GifDecoder::new(std::io::Cursor::new(bytes)).ok()?;
            bounded_animation_frames(decoder.into_frames(), pixels, MAX_ANIMATED_PIXELS)?
        }
        gpui::ImageFormat::Webp => {
            let decoder =
                image::codecs::webp::WebPDecoder::new(std::io::Cursor::new(bytes)).ok()?;
            if decoder.has_animation() {
                bounded_animation_frames(decoder.into_frames(), pixels, MAX_ANIMATED_PIXELS)?
            } else {
                1
            }
        }
        _ => 1,
    };
    if frame_count > MAX_ANIMATION_FRAMES {
        return None;
    }
    Some(ImageProbe {
        dims,
        decoded_pixels: pixels.checked_mul(frame_count as u64)?,
    })
}

fn bounded_animation_frames<'a>(
    frames: image::Frames<'a>,
    pixels_per_frame: u64,
    max_pixels: u64,
) -> Option<usize> {
    let limit = (max_pixels / pixels_per_frame).clamp(1, 120) as usize;
    let mut count = 0usize;
    for frame in frames.take(limit + 1) {
        frame.ok()?;
        count += 1;
        if count > limit {
            return None;
        }
    }
    (count > 0).then_some(count)
}

fn image_extension(media_type: &str) -> &'static str {
    match media_type.trim().to_ascii_lowercase().as_str() {
        "image/jpeg" | "image/jpg" => "jpg",
        "image/webp" => "webp",
        "image/gif" => "gif",
        "image/svg+xml" => "svg",
        "image/bmp" => "bmp",
        "image/tiff" | "image/tif" => "tiff",
        "image/heic" | "image/heif" => "heic",
        "image/avif" => "avif",
        _ => "png",
    }
}

/// 「另存为」一站式:系统对话框(起点是上次导出/保存的目录)→ 后台 `write` → 记住目录
/// → 通知。取消返回 None;写失败弹错并返回 None;成功返回落盘路径。对话框本身开不了
/// (Linux 缺 xdg-desktop-portal 时 gpui 报 Err)退回旧行为:直接写到起始目录、照常通知,
/// 不让按钮变成静默 no-op
fn save_as(
    window: &mut Window,
    cx: &mut App,
    store: Arc<Store>,
    suggested_name: String,
    done: &'static str,
    failed: &'static str,
    write: impl FnOnce(&std::path::Path) -> anyhow::Result<()> + Send + 'static,
) -> Task<Option<PathBuf>> {
    let start = export_dir(&store);
    let rx = cx.prompt_for_new_path(&start, Some(&suggested_name));
    window.spawn(cx, async move |cx| {
        let path = match rx.await {
            Ok(Ok(Some(path))) => path,
            Ok(Err(_)) => start.join(&suggested_name),
            _ => return None,
        };
        let written = cx
            .background_spawn({
                let path = path.clone();
                async move { write(&path) }
            })
            .await;
        cx.update(|window, cx| {
            let note = match &written {
                Ok(()) => {
                    if let Some(dir) = path.parent() {
                        let _ = store.pref_set("export_dir", &dir.to_string_lossy());
                    }
                    Notification::success(format!("{done} to {}", path.display()))
                }
                Err(e) => Notification::error(format!("{failed}: {e}")),
            };
            window.push_notification(note, cx);
        })
        .ok();
        written.ok().map(|()| path)
    })
}

/// 「另存为」的起始目录:上次导出/保存的目录(Store 的 prefs KV,与 Open In 记忆同表;
/// 目录还在)> Downloads > home
fn export_dir(store: &Store) -> PathBuf {
    store
        .pref_get("export_dir")
        .map(PathBuf::from)
        .filter(|dir| dir.is_dir())
        .or_else(dirs::download_dir)
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn zoom_fit(dims: Option<(u32, u32)>, viewport: Size<Pixels>) -> Size<Pixels> {
    let available_width = (f32::from(viewport.width) - 112.0).max(120.0);
    let available_height = (f32::from(viewport.height) - 164.0).max(120.0);
    let Some((width, height)) = dims.filter(|(width, height)| *width > 0 && *height > 0) else {
        return gpui::size(
            px(available_width.min(720.0)),
            px(available_height.min(480.0)),
        );
    };
    let (width, height) = (width as f32, height as f32);
    let scale = (available_width / width)
        .min(available_height / height)
        .min(1.0);
    gpui::size(px(width * scale), px(height * scale))
}

fn zoom_shadow(y: Pixels, blur: Pixels, alpha: f32) -> gpui::BoxShadow {
    gpui::BoxShadow {
        color: gpui::black().opacity(alpha),
        offset: gpui::point(px(0.), y),
        blur_radius: blur,
        spread_radius: px(0.),
        inset: false,
    }
}

/// 把消息正文按图片插入点切成「markdown 段 / 图片条」序列,交给调用方的容器
///(用户气泡 / 助手列)直接 `.children(..)` 承载,中间**不要加包装层**。
///
/// 已核实的机制:gpui 的 text 元素缓存上一次测量尺寸,之后凡是 wrap_width 为 None
/// 的探测(MinContent/MaxContent)一律返回缓存值,不管那次是在多窄的定宽下量的
///(gpui `elements/text.rs` 的缓存条件;上游只给 truncate 补了防中毒,wrap 没有)。
/// 实测的触发点:靠内容撑宽的用户气泡(`max_w` 无 `w_full`)下再套一层
/// `v_flex().min_w_0()` / `div().min_w_0()` 放 TextView,taffy 会先用 0 宽量文字,
/// "一字一行"的尺寸进了缓存,气泡 max-content 宽度随之变 0,只剩 28px 内边距
///(v0.3.5 回归,中英文都中招);各段直接挂在气泡上则中英文、图片、多段、代码块全正常。
/// 上游若把该缓存改成按 wrap_width 分键,此约束即可解除。
#[allow(clippy::too_many_arguments)]
fn message_content(
    message_index: usize,
    seq: i64,
    text: &str,
    slots: &[ImageSlot],
    base: Pixels,
    paragraph_gap: gpui::Rems,
    dark: bool,
    border: Hsla,
    panel: Hsla,
    muted: Hsla,
    workbench: Entity<Workbench>,
    cx: &mut App,
) -> Vec<AnyElement> {
    let mut content = Vec::new();
    let mut cursor = 0usize;
    let mut image_index = 0usize;

    // 每轮吃掉「下一组图片之前的文字」+ 该组图片;图片用尽后最后一轮吃掉尾段文字
    loop {
        let offset = slots
            .get(image_index)
            .map_or(text.len(), |slot| slot.text_offset().min(text.len()));
        let offset = text.floor_char_boundary(offset).max(cursor);
        let segment = text[cursor..offset].trim_matches('\n');
        if !segment.trim().is_empty() {
            // 序号只要在本条消息内唯一且跨帧稳定即可,段数就够
            let part = content.len();
            content.push(
                markdown_body(
                    format!("dmsg-{seq}-part-{part}").into(),
                    segment,
                    base,
                    paragraph_gap,
                    dark,
                    cx,
                )
                .into_any_element(),
            );
        }
        if image_index >= slots.len() {
            break;
        }

        let group_start = image_index;
        image_index += 1;
        while image_index < slots.len()
            && slots[image_index].text_offset().min(text.len())
                == slots[group_start].text_offset().min(text.len())
        {
            image_index += 1;
        }
        content.push(
            image_strip(
                message_index,
                group_start,
                &slots[group_start..image_index],
                border,
                panel,
                muted,
                workbench.clone(),
            )
            .into_any_element(),
        );
        cursor = offset;
    }
    content
}

fn image_strip(
    message_index: usize,
    image_index_start: usize,
    slots: &[ImageSlot],
    border: Hsla,
    panel: Hsla,
    muted: Hsla,
    workbench: Entity<Workbench>,
) -> Div {
    let mut row = h_flex().flex_wrap().gap(SPACE_SM);
    for (local_index, slot) in slots.iter().enumerate() {
        let image_index = image_index_start + local_index;
        row = row.child(match slot {
            ImageSlot::Ready { image, dims, .. } => {
                let entity = workbench.clone();
                let figure = gpui::img(image.clone());
                let figure = if dims.is_some_and(|(width, height)| width < height) {
                    figure.w(IMAGE_THUMB)
                } else {
                    figure.h(IMAGE_THUMB)
                };
                div()
                    .id(SharedString::from(format!(
                        "message-image:{message_index}:{image_index}"
                    )))
                    .size(IMAGE_THUMB)
                    .flex()
                    .items_center()
                    .justify_center()
                    .overflow_hidden()
                    .rounded(RADIUS_IMAGE)
                    .border_1()
                    .border_color(border)
                    .bg(panel)
                    .cursor_pointer()
                    .child(figure)
                    .on_click(move |_, _, cx| {
                        entity.update(cx, |this, cx| {
                            if let Some(detail) = &mut this.detail {
                                detail.zoom = Some((message_index, image_index));
                                detail.image_action_feedback = [None; 2];
                                cx.notify();
                            }
                        });
                    })
                    .into_any_element()
            }
            ImageSlot::Unsupported {
                media_type, bytes, ..
            } => {
                let media_type_for_save = media_type.to_string();
                let bytes_for_save = bytes.clone();
                let entity = workbench.clone();
                v_flex()
                    .id(SharedString::from(format!(
                        "unsupported-message-image:{message_index}:{image_index}"
                    )))
                    .size(IMAGE_THUMB)
                    .items_center()
                    .justify_center()
                    .gap(SPACE_XS)
                    .p(SPACE_SM)
                    .rounded(RADIUS_IMAGE)
                    .border_1()
                    .border_color(border)
                    .bg(panel)
                    .text_size(FONT_LABEL)
                    .text_color(muted)
                    .cursor_pointer()
                    .tooltip(|window, cx| {
                        gpui_component::tooltip::Tooltip::new("Save original image")
                            .build(window, cx)
                    })
                    .on_click(move |_, window, cx| {
                        let bytes = bytes_for_save.clone();
                        let name = format!(
                            "wake-image-{:016x}.{}",
                            gpui::hash(&*bytes),
                            image_extension(&media_type_for_save)
                        );
                        let store = entity.read(cx).store.clone();
                        save_as(
                            window,
                            cx,
                            store,
                            name,
                            "Saved",
                            "Couldn't save the image",
                            move |path| Ok(std::fs::write(path, &*bytes)?),
                        )
                        .detach();
                    })
                    .child("Preview unavailable")
                    .child(div().max_w_full().truncate().child(media_type.clone()))
                    .child(icon("icons/download.svg").with_size(px(14.)))
                    .into_any_element()
            }
            ImageSlot::Omitted { .. } => v_flex()
                .id(SharedString::from(format!(
                    "omitted-message-image:{message_index}:{image_index}"
                )))
                .size(IMAGE_THUMB)
                .items_center()
                .justify_center()
                .gap(SPACE_XS)
                .p(SPACE_SM)
                .rounded(RADIUS_IMAGE)
                .border_1()
                .border_color(border)
                .bg(panel)
                .text_size(FONT_LABEL)
                .text_color(muted)
                .child("Image omitted")
                .child("Transcript limit reached")
                .into_any_element(),
        });
    }
    row
}

/// 对话区居中小胶囊(System 消息 / Context compacted)
fn centered_pill(text: impl Into<SharedString>, cx: &App) -> Div {
    let theme = cx.theme();
    div().w_full().flex().justify_center().child(
        div()
            .px(px(10.))
            .py(px(3.))
            .rounded_full()
            .bg(theme.muted)
            .text_size(FONT_LABEL)
            .text_color(theme.muted_foreground)
            .max_w(px(520.))
            .truncate()
            .child(text.into()),
    )
}

/// 对话正文共用的 Markdown 视图。表格、引用块与分隔线由组件原生解析；
/// Wake 只覆写标题层级、代码块表面、语法配色和代码操作区。
fn markdown_body(
    id: SharedString,
    text: &str,
    base: Pixels,
    paragraph_gap: gpui::Rems,
    dark: bool,
    cx: &mut App,
) -> TextView {
    let theme = cx.theme();
    let code_bg = theme.muted;
    let code_border = theme.border;
    let code_radius = theme.radius;
    // TextView 的语法颜色在解析阶段固化；把主题写进 id，让模式切换走同步
    // 首次解析路径，避免代码块短暂保留上一种模式的颜色。
    let themed_id: SharedString = format!("{id}-{}", if dark { "dark" } else { "light" }).into();

    TextView::markdown(themed_id, text)
        .style(
            TextViewStyle {
                heading_base_font_size: base,
                paragraph_gap,
                is_dark: dark,
                highlight_theme: if dark {
                    HighlightTheme::default_dark()
                } else {
                    HighlightTheme::default_light()
                },
                ..Default::default()
            }
            .heading_font_size(|level, base| match level {
                1 => base * 1.45,
                2 => base * 1.28,
                3 => base * 1.14,
                4 => base * 1.05,
                _ => base,
            })
            .code_block(
                StyleRefinement::default()
                    .bg(code_bg)
                    .border_1()
                    .border_color(code_border)
                    .rounded(code_radius)
                    .px(SPACE_LG)
                    .py(SPACE_MD),
            ),
        )
        .code_block_actions(|block, _window, cx| {
            let theme = cx.theme();
            let code = block.code();
            h_flex()
                .items_center()
                .gap(SPACE_SM)
                .px(px(6.))
                .text_size(FONT_LABEL)
                .text_color(theme.muted_foreground)
                .when_some(block.lang(), |actions, lang| actions.child(lang))
                .child(
                    div()
                        .id("code-copy")
                        .size(px(20.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(RADIUS_BADGE)
                        .cursor_pointer()
                        .hover(|style| style.bg(theme.secondary_hover))
                        .tooltip(|window, cx| {
                            gpui_component::tooltip::Tooltip::new("Copy code").build(window, cx)
                        })
                        .on_click(move |_, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(code.to_string()));
                        })
                        .child(icon("icons/copy.svg").with_size(px(12.))),
                )
        })
        .selectable(true)
}

/// Thinking 折叠面板：收起是一行摘要，展开后显示完整原文。
fn thinking_panel(
    ix: usize,
    text: &str,
    expanded: bool,
    on_toggle: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    cx: &App,
) -> Div {
    let theme = cx.theme();
    let summary = clip_display(&one_line(text, 400), 88);
    let mut panel = v_flex()
        .w_full()
        .min_w_0()
        .rounded(theme.radius)
        .border_1()
        .border_color(theme.border)
        .bg(theme.muted.opacity(0.45))
        .child(
            h_flex()
                .id(("thinking", ix))
                .w_full()
                .min_w_0()
                .items_center()
                .gap(px(7.))
                .px(SPACE_MD)
                .py(SPACE_SM)
                .cursor_pointer()
                .text_size(FONT_MSG_THINKING)
                .text_color(theme.muted_foreground)
                .hover(|style| style.text_colored(theme.foreground, FONT_MSG_THINKING))
                .child(
                    icon("icons/chevron-right.svg")
                        .with_size(px(11.))
                        .flex_shrink_0()
                        .when(expanded, |icon| {
                            icon.rotate(gpui::Radians(std::f32::consts::FRAC_PI_2))
                        }),
                )
                .child(div().flex_shrink_0().font_medium().child("Thinking"))
                .when(!expanded, |header| {
                    header.child(
                        div()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .italic()
                            .child(summary),
                    )
                })
                .on_click(on_toggle),
        );

    if expanded {
        panel = panel.child(
            div()
                .w_full()
                .min_w_0()
                .pl(px(30.))
                .pr(SPACE_MD)
                .pb(SPACE_MD)
                .whitespace_normal()
                .text_size(FONT_MSG_THINKING)
                .line_height(relative(1.7))
                .text_color(theme.muted_foreground)
                .child(text.to_string()),
        );
    }

    panel
}

const TOOL_TEXT_PREVIEW_LIMIT: usize = 600;

fn tool_cluster_heading(
    calls: &[ToolCallView],
    arg_cells: usize,
) -> (SharedString, Option<SharedString>) {
    match calls {
        [] => ("No tool calls".into(), None),
        [only] => {
            let arg = (!only.input_preview.trim().is_empty())
                .then(|| clip_display(&only.input_preview, arg_cells).into());
            (only.name.clone().into(), arg)
        }
        many => {
            let names = many
                .iter()
                .map(|call| call.name.as_str())
                .collect::<Vec<_>>()
                .join(" · ");
            (
                format!("{} tool calls", many.len()).into(),
                Some(clip_display(&names, arg_cells).into()),
            )
        }
    }
}

fn clip_tool_text(text: &str, limit: usize) -> (String, bool) {
    let mut chars = text.chars();
    let mut shown: String = chars.by_ref().take(limit).collect();
    let truncated = chars.next().is_some();
    if truncated {
        shown.push('…');
    }
    (shown, truncated)
}

/// 工具调用折叠卡：收起显示名称/数量、Unicode 宽度感知的参数摘要和失败数；
/// 展开显示可用的完整 Input 与成功/失败 Output。正文只展示前 600 字符，
/// 但复制按钮始终复制完整内容，不使用会抢主阅读区滚轮的嵌套滚动容器。
fn tool_cluster(
    ix: usize,
    calls: &[ToolCallView],
    arg_cells: usize,
    expanded: bool,
    on_toggle: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    cx: &App,
) -> Div {
    let theme = cx.theme();
    let failed = calls.iter().filter(|c| c.is_error).count();
    let (head_name, head_arg) = tool_cluster_heading(calls, arg_cells);
    let mono = theme.mono_font_family.clone();
    let panel_border = theme.border;

    let mut cluster = v_flex()
        .w_full()
        .min_w_0()
        .rounded(theme.radius)
        .border_1()
        .border_color(panel_border)
        .bg(theme.muted.opacity(0.35))
        .child(
            h_flex()
                .id(("tool-cluster", ix))
                .w_full()
                .min_w_0()
                .items_center()
                .gap(px(7.))
                .px(SPACE_MD)
                .py(SPACE_SM)
                .cursor_pointer()
                .text_size(FONT_MSG_THINKING)
                .text_color(theme.muted_foreground)
                .hover(|style| style.text_colored(theme.foreground, FONT_MSG_THINKING))
                .child(
                    icon("icons/chevron-right.svg")
                        .with_size(px(11.))
                        .flex_shrink_0()
                        .when(expanded, |icon| {
                            icon.rotate(gpui::Radians(std::f32::consts::FRAC_PI_2))
                        }),
                )
                .child(
                    div()
                        .flex_shrink_0()
                        .font_medium()
                        .text_color(theme.foreground)
                        .child(head_name),
                )
                .when_some(head_arg, |header, arg| {
                    header.child(
                        div()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .font_family(mono.clone())
                            .child(arg),
                    )
                })
                .child(div().flex_1())
                .when(failed > 0, |header| {
                    header.child(
                        div()
                            .flex_shrink_0()
                            .px(px(7.))
                            .rounded(RADIUS_BADGE)
                            .text_size(FONT_LABEL)
                            .text_color(theme.danger)
                            .bg(theme.danger.opacity(0.12))
                            .child(format!("{failed} failed")),
                    )
                })
                .on_click(on_toggle),
        );

    if expanded {
        let mut items = v_flex()
            .w_full()
            .min_w_0()
            .border_t_1()
            .border_color(panel_border);
        for (call_ix, call) in calls.iter().enumerate() {
            let mut item = v_flex()
                .w_full()
                .min_w_0()
                .px(SPACE_MD)
                .py(px(10.))
                .gap(px(6.))
                .when(call_ix > 0, |item| {
                    item.border_t_1().border_color(panel_border)
                });

            if calls.len() > 1 {
                item = item.child(
                    h_flex()
                        .w_full()
                        .min_w_0()
                        .gap(px(7.))
                        .child(
                            div()
                                .flex_shrink_0()
                                .text_size(FONT_MSG_THINKING)
                                .font_medium()
                                .text_color(if call.is_error {
                                    theme.danger
                                } else {
                                    theme.foreground
                                })
                                .child(call.name.clone()),
                        )
                        .child(
                            div()
                                .min_w_0()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_size(FONT_MSG_THINKING)
                                .font_family(mono.clone())
                                .text_color(theme.muted_foreground)
                                .child(clip_display(&call.input_preview, arg_cells)),
                        ),
                );
            }

            let input = call.input.as_deref().and_then(non_blank_tool_text);
            if let Some(input) = input {
                item = item.child(tool_section(
                    format!("tool-copy-{ix}-{call_ix}-input").into(),
                    "Input",
                    input,
                    false,
                    mono.clone(),
                    cx,
                ));
            } else if let Some(input_preview) = non_blank_tool_text(&call.input_preview) {
                item = item.child(tool_section(
                    format!("tool-copy-{ix}-{call_ix}-input-preview").into(),
                    "Input preview",
                    input_preview,
                    false,
                    mono.clone(),
                    cx,
                ));
            }

            if let Some(output) = call.output.as_deref().and_then(non_blank_tool_text) {
                item = item.child(tool_section(
                    format!("tool-copy-{ix}-{call_ix}-output").into(),
                    "Output",
                    output,
                    call.is_error,
                    mono.clone(),
                    cx,
                ));
            }

            items = items.child(item);
        }
        cluster = cluster.child(items);
    }
    cluster
}

/// 空白只参与“是否有内容”的判断；展示与复制必须保留工具原始输出的边界空白。
fn non_blank_tool_text(text: &str) -> Option<&str> {
    (!text.trim().is_empty()).then_some(text)
}

fn tool_section(
    copy_id: SharedString,
    label: &'static str,
    body: &str,
    is_error: bool,
    mono: SharedString,
    cx: &App,
) -> Div {
    let theme = cx.theme();
    let (shown, truncated) = clip_tool_text(body, TOOL_TEXT_PREVIEW_LIMIT);
    let full = body.to_string();
    let copy_tooltip: SharedString = format!("Copy full {}", label.to_lowercase()).into();

    v_flex()
        .w_full()
        .min_w_0()
        .gap(SPACE_XS)
        .child(
            h_flex()
                .w_full()
                .items_center()
                .gap(SPACE_SM)
                .text_size(FONT_LABEL)
                .text_color(if is_error {
                    theme.danger
                } else {
                    theme.muted_foreground
                })
                .child(label)
                .when(truncated, |header| {
                    header.child(
                        div()
                            .text_color(theme.muted_foreground)
                            .child(format!("First {TOOL_TEXT_PREVIEW_LIMIT} characters")),
                    )
                })
                .child(div().flex_1())
                .child(
                    div()
                        .id(copy_id)
                        .size(px(20.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(RADIUS_BADGE)
                        .cursor_pointer()
                        .text_color(theme.muted_foreground)
                        .hover(|style| style.bg(theme.secondary_hover).text_color(theme.foreground))
                        .tooltip(move |window, cx| {
                            gpui_component::tooltip::Tooltip::new(copy_tooltip.clone())
                                .build(window, cx)
                        })
                        .on_click(move |_, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(full.clone()));
                        })
                        .child(icon("icons/copy.svg").with_size(px(12.))),
                ),
        )
        .child(
            div()
                .w_full()
                .min_w_0()
                .px(px(10.))
                .py(px(7.))
                .rounded(RADIUS_KBD)
                .bg(if is_error {
                    theme.danger.opacity(0.08)
                } else {
                    theme.muted
                })
                .whitespace_normal()
                .text_size(FONT_MSG_THINKING)
                .line_height(relative(1.6))
                .font_family(mono)
                .text_color(if is_error {
                    theme.danger
                } else {
                    theme.muted_foreground
                })
                .child(shown),
        )
}

/// 小胶囊 badge(项目名/model/source 共用):4px 圆角,内部截断。
/// 项目名用 muted 灰;model/source 用主题色 tint(淡底+同色文字)。
/// 该数据根是否派生自某条自定义 location(with_custom_root 契约保证派生根
/// 全在其落库目录之下);返回落库路径。面板行标记与表单重叠排除共用同一判据
fn custom_owner<'a>(
    customs: &'a [(AgentId, SharedString)],
    agent: AgentId,
    root: &str,
) -> Option<&'a SharedString> {
    customs
        .iter()
        .find(|(a, p)| *a == agent && path_owns(p.as_ref(), root))
        .map(|(_, p)| p)
}

fn badge(name: impl Into<SharedString>, bg: Hsla, fg: Hsla) -> impl IntoElement {
    div()
        .min_w_0()
        .px(px(6.))
        .py(px(1.))
        .rounded(RADIUS_BADGE)
        .bg(bg)
        .text_color(fg)
        .font_medium()
        .child(div().truncate().child(name.into()))
}

/// outline 变体(model/source 用):透明底,边框与文字同色
fn outline_badge(name: impl Into<SharedString>, color: Hsla) -> impl IntoElement {
    div()
        .min_w_0()
        .px(px(6.))
        .py(px(1.))
        .rounded(RADIUS_BADGE)
        .border_1()
        .border_color(color)
        .text_color(color)
        .font_medium()
        .child(div().truncate().child(name.into()))
}

/// 侧栏底部工具条的图标按钮。透明底、hover 才出色——底部是次要操作区,
/// 不与导航行的选中态抢注意力;图标-only 元素改 text_color 不丢字号。
/// enabled=false(刷新进行中)只留静态内容,连 tooltip 与点击一起摘掉
fn sidebar_tool_btn(
    id: &'static str,
    tooltip: &'static str,
    enabled: bool,
    content: AnyElement,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    cx: &Context<Workbench>,
) -> Stateful<Div> {
    let theme = cx.theme();
    div()
        .id(id)
        .size(ROW_HEIGHT)
        .flex_shrink_0()
        .rounded(theme.radius)
        .flex()
        .items_center()
        .justify_center()
        .text_color(theme.muted_foreground)
        .when(enabled, |el| {
            el.cursor_pointer()
                .hover(|s| s.bg(theme.secondary_hover).text_color(theme.foreground))
                .active(|s| s.bg(theme.secondary_active))
                .tooltip(move |window, cx| {
                    gpui_component::tooltip::Tooltip::new(tooltip).build(window, cx)
                })
                .on_click(on_click)
        })
        .child(content)
}

/// Things 风源列表行:图标 + 文字 + 计数,6px 圆角选中胶囊
#[allow(clippy::too_many_arguments)]
fn sidebar_row(
    id: impl Into<ElementId>,
    lead: RowLead,
    label: impl Into<SharedString>,
    count: Option<i64>,
    active: bool,
    level: RowLevel,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    cx: &Context<Workbench>,
) -> Stateful<Div> {
    let theme = cx.theme();
    let sub = level == RowLevel::Sub;
    div()
        .id(id)
        .h(if sub { ROW_HEIGHT_SUB } else { ROW_HEIGHT })
        .flex_shrink_0()
        // 分组项整行右移一档表达从属:行首因此落在轴右侧 SUB_INDENT 处,
        // 压轴的是主导航与组头,不是这里
        .pl(if sub {
            LEAD_INSET + SUB_INDENT
        } else {
            LEAD_INSET
        })
        .pr(SIDEBAR_EDGE)
        .rounded(theme.radius)
        .cursor_pointer()
        .flex()
        .items_center()
        .when(active, |s| {
            s.bg(theme.sidebar_accent)
                .text_color(theme.sidebar_accent_foreground)
        })
        .when(!active, |s| {
            s.text_color(theme.sidebar_foreground)
                .hover(|s| s.bg(theme.sidebar_accent.opacity(0.55)))
                .active(|s| s.bg(theme.sidebar_accent))
        })
        .on_click(on_click)
        .child(
            h_flex()
                .w_full()
                .gap(SPACE_SM)
                .child(
                    // 定宽槽位保证文字起点统一;内部居中,使小图标的中心也落在
                    // LEAD_AXIS 上(左对齐会让 14/15px 图标的中心偏离轴 1.5~2pt)
                    div()
                        .w(LEAD_BOX)
                        .flex_shrink_0()
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(match lead {
                            // 线条图标比实心品牌图视觉轻,给它小一档才平衡
                            RowLead::Icon(ic) => ic
                                .with_size(if sub { px(14.) } else { px(15.) })
                                .text_color(if active {
                                    theme.sidebar_accent_foreground
                                } else {
                                    theme.muted_foreground
                                })
                                .into_any_element(),
                            // 品牌图不着色:img 走 AssetSource 取内嵌 PNG,原色渲染
                            // (侧栏单色化试过,用户否决——保持彩色)
                            RowLead::Brand(path) => img(path).size(LEAD_BOX).into_any_element(),
                        }),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_size(if sub { FONT_CAPTION } else { FONT_BODY })
                        .truncate()
                        .child(label.into()),
                )
                .when_some(count, |this, n| {
                    // 与 Session locations 面板同款胶囊。底色随行态切换而不是
                    // 固定 muted:常态行底是 sidebar,用 accent 衬;选中行底本身
                    // 就是 accent,退回 sidebar 材质反衬。固定 muted 会在浅色
                    // 常态(#E8E8E5 vs #EDEDEA)和深色选中(#323230 vs #343432)
                    // 两处糊进背景里
                    let bg = if active {
                        theme.sidebar
                    } else {
                        theme.sidebar_accent
                    };
                    this.child(div().flex_shrink_0().text_size(FONT_LABEL).child(badge(
                        n.to_string(),
                        bg,
                        theme.muted_foreground,
                    )))
                }),
        )
}

/// Open In 记忆的落盘形态(prefs 表 `open_in`,id 字符串在加载时对回
/// 本机已装终端,残值静默丢弃)
#[derive(serde::Serialize, serde::Deserialize)]
struct OpenInPrefs {
    #[serde(default)]
    agents: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    last: Option<String>,
}

/// Open In 的目标图标:内嵌品牌覆盖(见 TerminalApp::brand_icon)→
/// 提取的 .app 图标 → 通用终端 svg。两处渲染(split 左段/下拉)共用,
/// 精度规则只此一份
fn open_in_icon(
    term: Option<terminal::TerminalApp>,
    icon_path: Option<&PathBuf>,
    fallback: Icon,
) -> AnyElement {
    match (term.and_then(|t| t.brand_icon()), icon_path) {
        (Some(b), _) => img(b).size(px(16.)).into_any_element(),
        (None, Some(p)) => img(p.clone()).size(px(16.)).into_any_element(),
        (None, None) => fallback.into_any_element(),
    }
}

/// 详情工具栏图标按钮。选中态 = 填充版图标 + 语义色(macOS 惯例),
/// 按钮本身不落底色(.selected 的 hover 底被否)。
fn tool_btn(
    id: &'static str,
    icon_path: &'static str,
    filled_icon_path: &'static str,
    active_color: Hsla,
    tooltip: &'static str,
    highlighted: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Button {
    let ic = if highlighted {
        icon(filled_icon_path)
            .with_size(px(16.))
            .text_color(active_color)
    } else {
        icon(icon_path).with_size(px(16.))
    };
    Button::new(id)
        .ghost()
        .rounded(RADIUS_BUTTON)
        .icon(ic)
        .tooltip(tooltip)
        .on_click(on_click)
}

impl Focusable for Workbench {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for Workbench {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.refresh_session_group_date(cx);
        self.restore_pending_list_selection(window, cx);
        let theme = cx.theme();
        div()
            .id("workbench")
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::toggle_search))
            .on_action(cx.listener(|this, _: &RefreshSessions, window, cx| {
                this.refresh_sessions(window, cx)
            }))
            .on_action(cx.listener(|this, _: &OpenSettings, _window, cx| this.open_settings(cx)))
            .on_action(cx.listener(|this, _: &OpenUpdates, _window, cx| this.open_updates(cx)))
            .on_action(cx.listener(|this, _: &OpenAbout, _window, cx| this.open_about(cx)))
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, window, cx| {
                this.update_detail_selection_auto_scroll(event, window, cx)
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.stop_detail_selection_auto_scroll()),
            )
            .size_full()
            .bg(theme.background)
            .text_color(theme.foreground)
            .child(
                h_flex()
                    .size_full()
                    .child(self.render_sidebar(window, cx))
                    // Insights 是整页目的地:替换中栏+右栏,侧栏导航保持在场
                    .map(|this| {
                        if self.insights_open {
                            this.child(self.render_insights(cx))
                        } else {
                            this.child(self.render_session_list(cx))
                                .child(self.render_detail(window, cx))
                        }
                    }),
            )
            .child(self.render_image_zoom(window, cx))
            .children(overlay_layers(window, cx))
    }
}
