// ============================================================================
// DIRECTION CONTRACT (impeccable)
// THESIS: 找回任何一段 agent 对话只需几秒;界面以 macOS 原生语言隐入背景,
//   拒绝"开发者工具=黑底霓虹终端风"的品类默认。
// OWN-WORLD: Things/Bear + Claude 客户端基准的原生 macOS 质感——暖白/暖黑双模式、
//   色差分区(无 hairline 依赖)、8px 圆角胶囊选中态(按钮 6px)、系统蓝 accent、
//   lucide 单线图标、SF 系统字体 14px 基准;agent 品牌色仅作识别圆点。
// STORY: 打开即见全部会话按时间流动;左栏收窄范围,中栏定位会话,右栏读全文;
//   ⌘K 直达任意一句话;一键回到终端继续。
// FIRST VIEWPORT: 全高三栏——240px 侧栏(全局搜索/全部/收藏/智能体/项目)、
//   372px 会话列表(22px 上下文标题+双行卡),余宽详情(标题+操作+markdown 正文)。
// FORM: brief-pinned canon(用户指定"现代 macOS 设计规范",对标 Things/Bear);
//   concept tournament 依规跳过,canon at full fidelity。
// FINISH: unreviewed and undocumented is unfinished; this build ends with the
//   finish review, the verdict, and DESIGN.md
// ============================================================================
use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use futures::StreamExt;
use gpui::prelude::FluentBuilder as _;
use gpui::*;
use gpui_component::button::{Button, ButtonCustomVariant, ButtonVariants as _};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::list::{List, ListDelegate, ListEvent, ListItem, ListState};
use gpui_component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_component::notification::Notification;
use gpui_component::progress::Progress;
use gpui_component::scroll::ScrollableElement as _;
use gpui_component::spinner::Spinner;
use gpui_component::text::{TextView, TextViewStyle};
use gpui_component::{
    h_flex, v_flex, ActiveTheme as _, Icon, IndexPath, Root, Sizable as _, StyledExt as _,
    TitleBar, WindowExt as _,
};

use wake_core::adapters::{create_adapters, AgentAdapter};
use wake_core::db::Store;
use wake_core::models::*;
use wake_core::scanner::{run_scan, ScanEvents, ScanProgress};

use crate::session_list::SessionsDelegate;
use wake_core::services::{exporter, terminal};
use wake_core::watcher::{start_watcher, SessionWatcher};

use crate::format::{abs_date, display_file_path, fmt_tokens, one_line, relative_time};
use crate::ui::*;

actions!(
    wake,
    [ToggleSearch, RefreshSessions, PaletteUp, PaletteDown]
);

pub const KEY_CONTEXT: &str = "Workbench";
/// ⌘K 面板容器的 key context(main.rs 的 ↑↓ 绑定与 dialog 元素共用)
pub const PALETTE_CONTEXT: &str = "WakePalette";
/// ⌘K 面板内容总高(输入行 + 结果列表 + footer);列表 flex_1 吃剩余空间
const PALETTE_HEIGHT: Pixels = px(492.);

fn icon(path: &'static str) -> Icon {
    Icon::empty().path(path)
}

/// 起一条后台扫描线程。启动时的自动扫描(full=false)与用户主动重扫(full=true)
/// 共用;返回的 Result 由 run_scan 的终态事件代为上报,这里只需丢弃。
fn spawn_scan(
    adapters: Arc<Vec<Box<dyn AgentAdapter>>>,
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
}

struct ChannelEvents(futures::channel::mpsc::UnboundedSender<BgEvent>);

impl ScanEvents for ChannelEvents {
    fn on_progress(&self, p: &ScanProgress) {
        let _ = self.0.unbounded_send(BgEvent::Progress(p.clone()));
    }
    fn on_sessions_changed(&self) {
        let _ = self.0.unbounded_send(BgEvent::Changed);
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
                    .px_2()
                    .py_2p5()
                    .gap_1p5()
                    .child(
                        h_flex()
                            .gap_2()
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
                                    .flex_shrink_0()
                                    .text_size(FONT_CAPTION)
                                    .text_color(theme.muted_foreground)
                                    .child(format!(
                                        "{} · {}",
                                        h.session.project_name,
                                        relative_time(h.timestamp.unwrap_or(0))
                                    )),
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
            .gap_3()
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
                .px_2()
                .pb_1()
                .text_size(FONT_LABEL)
                .text_color(cx.theme().muted_foreground)
                .child("Short query — using fallback search. Longer keywords are faster."),
        )
    }
}

// ---------------- 详情状态 ----------------

struct DetailState {
    meta: SessionMeta,
    /// 过滤后的可见消息。Rc 让行渲染以引用计数克隆代替整条消息深拷贝
    transcript: Rc<Vec<TranscriptMessage>>,
    loading: bool,
    /// 逐消息不等高列表(gpui 原生 ListState,惰性测量)
    msg_list: gpui::ListState,
    /// 展开的工具簇/thinking(按消息在 transcript 里的下标)
    expanded_rows: HashSet<usize>,
    /// 搜索跳转目标(FTS seq,契约=消息 seq);解析完成后滚到该消息并保持高亮
    jump_seq: Option<i64>,
}

// ---------------- Workbench ----------------

pub struct Workbench {
    focus_handle: FocusHandle,
    store: Arc<Store>,
    adapters: Arc<Vec<Box<dyn AgentAdapter>>>,

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
    /// 用户主动重扫中(模态进度弹窗开着,阻断其他操作)。与 scan.scanning 正交:
    /// 那是"扫没扫",这是"要不要弹模态"
    refreshing: bool,
    /// 全量解析进度 (done, total);None = 仍在枚举文件。
    /// 用 Rc<Cell> 而非 entity 字段:进度弹窗 builder 在本 entity 的
    /// render 期间执行,entity.read(cx) 会 double-lease panic。
    refresh_progress: Rc<Cell<Option<(usize, usize)>>>,
    total_sessions: i64,

    list_state: Entity<ListState<SessionsDelegate>>,
    palette_list: Entity<ListState<SearchDelegate>>,
    /// ⌘K 搜索输入框(自管,不用 List 内置 searchable:清除钮可控)
    palette_input: Entity<InputState>,
    /// 进行中的搜索任务;新输入覆盖旧值即取消过期搜索
    _palette_search_task: Option<Task<()>>,

    detail: Option<DetailState>,

    scan_events: Arc<dyn ScanEvents>,
    watcher: Option<SessionWatcher>,
    /// 终端 id → 提取好的应用图标 png(后台 JXA 提取,详情页 Open In 用)
    terminal_icons: HashMap<String, PathBuf>,
    /// Open In 上次选择(split 按钮左段直开目标),None = 已装列表首个
    preferred_terminal: Option<terminal::TerminalApp>,
    _subs: Vec<Subscription>,
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
        let adapters: Arc<Vec<Box<dyn AgentAdapter>>> = Arc::new(create_adapters());

        let list_state = cx.new(|cx| {
            ListState::new(SessionsDelegate::new(store.clone()), window, cx).searchable(false)
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

        cx.spawn_in(window, async move |this, cx| {
            while let Some(ev) = rx.next().await {
                if this
                    .update_in(cx, |this, window, cx| this.on_bg_event(ev, window, cx))
                    .is_err()
                {
                    break;
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
            refresh_progress: Rc::new(Cell::new(None)),
            total_sessions: 0,
            list_state,
            palette_list,
            palette_input,
            _palette_search_task: None,
            detail: None,
            scan_events,
            watcher,
            terminal_icons: HashMap::new(),
            preferred_terminal: None,
            _subs: subs,
        };
        this.refresh(window, cx);

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
            title_query: None,
            sort: self.sort_key,
            ascending: self.sort_ascending,
            limit: 500,
            offset: 0,
            roots_only: !self.favorite_only,
        }
    }

    fn refresh(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let filter = self.current_filter();
        if let Ok((sessions, total)) = self.store.list_sessions(&filter) {
            self.total_sessions = total;
            let tree_mode = filter.roots_only;
            let child_counts = if tree_mode {
                self.store.child_counts().unwrap_or_default()
            } else {
                Default::default()
            };
            let sort_key = self.sort_key;
            let ascending = self.sort_ascending;
            self.list_state.update(cx, |state, cx| {
                state.delegate_mut().apply_roots(
                    sessions,
                    child_counts,
                    tree_mode,
                    sort_key,
                    ascending,
                );
                cx.notify();
            });
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
        cx.notify();
    }

    /// 手动全量重扫(菜单 File → Refresh Sessions,⌘R)。刷新中忽略重复触发。
    /// 弹出模态进度弹窗阻断其他操作,完成后 on_bg_event 自动关闭。
    fn refresh_sessions(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.scan.scanning {
            return;
        }
        self.scan = ScanProgress {
            scanning: true,
            ..Default::default()
        };
        self.refresh_progress.set(None);
        self.refreshing = true;
        cx.notify();
        spawn_scan(
            self.adapters.clone(),
            self.store.clone(),
            self.scan_events.clone(),
            true,
        );

        let shared_progress = self.refresh_progress.clone();
        window.open_dialog(cx, move |dialog, _window, cx| {
            let progress = shared_progress.get();
            let theme = cx.theme();
            let (pct, note) = match progress {
                Some((done, total)) if total > 0 => (
                    ((done as f32 / total as f32) * 100.).max(3.),
                    format!("{done} of {total} sessions"),
                ),
                _ => (3., "Looking for sessions…".to_string()),
            };
            dialog
                .w(px(380.))
                .close_button(false)
                .overlay_closable(false)
                .keyboard(false)
                .child(
                    v_flex()
                        .gap(SPACE_LG)
                        .child(
                            h_flex().gap_2().child(Spinner::new().small()).child(
                                div()
                                    .text_size(FONT_HEADING)
                                    .font_semibold()
                                    .text_color(theme.foreground)
                                    .child("Refreshing sessions"),
                            ),
                        )
                        .child(Progress::new().value(pct))
                        .child(
                            div()
                                .text_size(FONT_CAPTION)
                                .text_color(theme.muted_foreground)
                                .child(note),
                        ),
                )
        });
    }

    fn on_bg_event(&mut self, ev: BgEvent, window: &mut Window, cx: &mut Context<Self>) {
        match ev {
            BgEvent::Progress(p) => {
                self.refresh_progress.set(if p.total > 0 {
                    Some((p.done, p.total))
                } else {
                    None
                });
                if !p.scanning && self.refreshing {
                    self.refreshing = false;
                    window.close_dialog(cx);
                    let note = match &p.error {
                        None => Notification::success("Sessions refreshed"),
                        Some(err) => Notification::error(format!("Refresh failed: {err}")),
                    };
                    window.push_notification(note, cx);
                }
                self.scan = p;
                cx.notify();
            }
            BgEvent::Changed => self.refresh(window, cx),
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
        let key = list
            .read(cx)
            .delegate()
            .rows
            .get(ix.row)
            .map(|s| s.meta.key.clone());
        if let Some(key) = key {
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
        if self.refreshing {
            return;
        }
        if window.has_active_dialog(cx) {
            window.close_dialog(cx);
            return;
        }
        let list = self.palette_list.clone();
        let input = self.palette_input.clone();
        let this = cx.entity();
        window.open_dialog(cx, move |dialog, window, cx| {
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
                .overlay_closable(true)
                .child(
                    v_flex()
                        // ↑↓ 在 Input 内不被消费,冒泡到这里走 main.rs 的
                        // PALETTE_CONTEXT 键位(Input 拆出 List 后原生 List 绑定够不着)
                        .key_context(PALETTE_CONTEXT)
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
                        .gap_3()
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
                                        .gap_3()
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
            // Bottom 对齐 = 聊天语义:打开落在最新消息,向上翻历史
            msg_list: gpui::ListState::new(0, gpui::ListAlignment::Bottom, px(512.)),
            expanded_rows: HashSet::new(),
            jump_seq,
        });
        // 搜索路径:中栏列表同步选中并滚到该会话。
        // 列表点击路径(jump=None)不走——List 点击自带选中,再滚会跳视口
        if jump_seq.is_some() {
            self.sync_list_selection(key, window, cx);
        }
        cx.notify();

        let adapters = self.adapters.clone();
        let task = cx.background_spawn(async move {
            let adapter = adapters.iter().find(|a| a.agent() == meta.agent)?;
            let r = SessionFileRef::from_meta(&meta);
            let t = adapter.parse_transcript(&r).ok()?;
            let visible: Vec<TranscriptMessage> = t
                .mainline
                .into_iter()
                .filter(|m| {
                    m.kind != MessageKind::Meta
                        && (!m.text.trim().is_empty()
                            || !m.tool_calls.is_empty()
                            || m.thinking.is_some()
                            || m.kind == MessageKind::CompactSummary)
                })
                .collect();
            Some((meta.key.clone(), visible))
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            this.update_in(cx, |this, _window, cx| {
                if let Some(detail) = &mut this.detail {
                    match result {
                        Some((key, messages)) if key == detail.meta.key => {
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
                            detail.loading = false;
                        }
                        _ => detail.loading = false,
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// 清空过滤回 All Sessions 视图(侧栏点击与搜索打开共用;
    /// 已在 All Sessions 时 refresh 幂等,微秒级)
    fn show_all_sessions(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.selected_agent = None;
        self.selected_project = None;
        self.favorite_only = false;
        self.refresh(window, cx);
    }

    /// 搜索命中打开:侧栏切回 All Sessions(搜索是全库范围,过滤视图下
    /// 命中可能不在列表里),中栏定位选中该会话并滚到可见
    fn sync_list_selection(&mut self, key: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.show_all_sessions(window, cx);
        if let Ok(Some(pk)) = self.store.parent_key_of(key) {
            self.list_state.update(cx, |state, _| {
                state.delegate_mut().ensure_expanded(&pk);
            });
        }
        let row = self
            .list_state
            .read(cx)
            .delegate()
            .rows
            .iter()
            .position(|s| s.meta.key == key);
        if let Some(row) = row {
            self.list_state.update(cx, |state, cx| {
                state.set_selected_index(Some(IndexPath::new(row)), window, cx);
                // 组件无 strict-Top:非 Center 策略都是"最小滚动恰好可见",
                // 目标从下方进入会贴底。先把 offset 拉到超底,deferred 消费时
                // 目标位于视口上方,最小滚动分支即把它对齐到视口顶。
                // 耦合 gpui-component 0.5.1 行为;上游 DeferredScrollToItem 的
                // scroll_strict 字段目前写死 false 未被读——它被接通之日,
                // 换成 strict-Top 调用并删掉这行 set_offset
                state.scroll_handle().set_offset(point(px(0.), px(-1e9)));
                state.scroll_to_item(IndexPath::new(row), ScrollStrategy::Top, window, cx);
            });
        }
    }

    // ---------- 操作 ----------

    /// 后台任务完成 → 推通知的通用桥(do_resume/do_export 共用)
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

    fn do_resume(
        &mut self,
        term: terminal::TerminalApp,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(detail) = &self.detail else { return };
        self.preferred_terminal = Some(term);
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
            let v = !detail.meta.favorite;
            let _ = self.store.set_user_data(&detail.meta.key, Some(v), None);
            detail.meta.favorite = v;
            self.refresh(window, cx);
        }
    }

    fn toggle_pinned(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(detail) = &mut self.detail {
            let v = !detail.meta.pinned;
            let _ = self.store.set_user_data(&detail.meta.key, None, Some(v));
            detail.meta.pinned = v;
            self.refresh(window, cx);
        }
    }

    fn do_export(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(detail) = &self.detail else { return };
        let meta = detail.meta.clone();
        let adapters = self.adapters.clone();
        let task = cx.background_spawn(async move {
            let adapter = adapters.iter().find(|a| a.agent() == meta.agent)?;
            // from_meta 对虚拟路径(SQLite 型)自动回退,导出不再依赖真实文件存在
            let r = SessionFileRef::from_meta(&meta);
            let t = adapter.parse_transcript(&r).ok()?;
            let sidechains: Vec<(SidechainInfo, Vec<TranscriptMessage>)> = t
                .sidechains
                .iter()
                .map(|sc| {
                    let msgs = adapter.load_sidechain(&r, &sc.id).unwrap_or_default();
                    (sc.clone(), msgs)
                })
                .collect();
            let md = exporter::to_markdown(&t.meta, &t.mainline, &sidechains);
            let name = exporter::default_file_name(&meta, "md");
            let path = dirs::download_dir()?.join(name);
            std::fs::write(&path, md).ok()?;
            Some(path)
        });
        Self::notify_when_done(window, cx, task, |path| match path {
            Some(p) => Notification::success(format!("Exported to {}", p.display())),
            None => Notification::error("Export failed"),
        });
    }

    /// 执行删除:文件进废纸篓 + 自库 tombstone。trash_paths 走 osascript 驱动
    /// Finder,首次触发自动化授权时会一直停着等用户点选——必须离开 UI 线程,
    /// 否则界面在授权框弹出的整段时间里完全冻结。
    fn do_delete(
        &mut self,
        keys: Vec<String>,
        targets: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let store = self.store.clone();
        let n = keys.len();
        let trash_keys = keys.clone();
        let task = cx.background_spawn(async move {
            terminal::trash_paths(&targets)?;
            for k in &trash_keys {
                store.remove_session(k, true)?;
            }
            anyhow::Ok(())
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            this.update_in(cx, |this, window, cx| match result {
                Ok(()) => {
                    // 等待期间用户可能已翻到别的会话,只在仍停在被删树内时才清空
                    if this
                        .detail
                        .as_ref()
                        .is_some_and(|d| keys.iter().any(|k| k == &d.meta.key))
                    {
                        this.detail = None;
                    }
                    let msg = if n > 1 {
                        format!("{n} sessions moved to Trash")
                    } else {
                        "Session moved to Trash".into()
                    };
                    window.push_notification(Notification::success(msg), cx);
                    // 立刻把它从列表摘掉,不等 watcher 那 800ms 去抖
                    this.refresh(window, cx);
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
        let children = self.store.all_children(&meta.key).unwrap_or_default();
        let n_children = children.len();
        // 会话归属哪些磁盘路径(主文件/边车目录)是 adapter 的布局知识
        let adapter = self.adapters.iter().find(|a| a.agent() == meta.agent);
        let mut targets = Vec::new();
        let mut keys = Vec::with_capacity(1 + n_children);
        let mut push = |m: &SessionMeta| {
            keys.push(m.key.clone());
            let paths = adapter
                .map(|a| a.session_paths(m))
                .unwrap_or_else(|| vec![m.file_path.clone()]);
            for p in paths {
                if !targets.contains(&p) {
                    targets.push(p);
                }
            }
        };
        push(&meta);
        for c in &children {
            push(c);
        }
        let entity = cx.entity();
        window.open_dialog(cx, move |dialog, _window, cx| {
            let keys = keys.clone();
            let targets = targets.clone();
            let entity = entity.clone();
            let theme = cx.theme();
            let title = if n_children > 0 {
                "Delete this session and nested sessions?"
            } else {
                "Delete this session?"
            };
            let lead = if n_children > 0 {
                format!(
                    "This session and {n_children} nested sessions will be moved to Trash. You can restore them anytime:"
                )
            } else {
                "The session file will be moved to Trash. You can restore it anytime:".into()
            };
            dialog
                .title(div().font_semibold().child(title))
                .w(px(440.))
                .child(
                    v_flex()
                        .gap_2()
                        .text_size(FONT_BODY)
                        .child(lead)
                        .child(
                            div()
                                .px_2()
                                .py_1()
                                .rounded(theme.radius)
                                .bg(theme.muted)
                                .text_size(FONT_CAPTION)
                                .font_family("Menlo")
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

    fn render_sidebar(&self, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let all_active =
            self.selected_agent.is_none() && self.selected_project.is_none() && !self.favorite_only;
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

        v_flex()
            .w(px(224.))
            .h_full()
            .flex_shrink_0()
            .bg(theme.sidebar)
            // 压平 titlebar 靠 theme.rs 的 title_bar/title_bar_border token,不再叠加覆写
            .child(TitleBar::new())
            .child(
                div()
                    .flex_shrink_0()
                    .px(SIDEBAR_EDGE)
                    .pt(SPACE_SM)
                    .pb(SPACE_MD)
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
                div().flex_shrink_0().px(SIDEBAR_EDGE).pb_3().child(
                    h_flex()
                        .gap_2()
                        .child(
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
                                .child(div().flex_shrink_0().text_size(FONT_LABEL).child("⌘K")),
                        )
                        .child({
                            let busy = self.scan.scanning;
                            div()
                                .id("refresh")
                                .size(ROW_HEIGHT)
                                .flex_shrink_0()
                                .rounded(theme.radius)
                                .bg(theme.secondary)
                                .flex()
                                .items_center()
                                .justify_center()
                                .text_color(theme.muted_foreground)
                                .when(!busy, |el| {
                                    el.cursor_pointer()
                                        .hover(|s| {
                                            s.bg(theme.secondary_hover).text_color(theme.foreground)
                                        })
                                        .active(|s| {
                                            s.bg(theme.secondary_active)
                                                .text_color(theme.foreground)
                                        })
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.refresh_sessions(window, cx)
                                        }))
                                })
                                .child(if busy {
                                    Spinner::new().small().into_any_element()
                                } else {
                                    icon("icons/refresh-cw.svg")
                                        .with_size(px(14.))
                                        .into_any_element()
                                })
                        }),
                ),
            )
            .child(
                v_flex()
                    .flex_shrink_0()
                    .px(SIDEBAR_EDGE)
                    .pb_1()
                    .gap_1()
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
                        cx.listener(|this, _, window, cx| {
                            this.favorite_only = !this.favorite_only;
                            if this.favorite_only {
                                this.selected_agent = None;
                                this.selected_project = None;
                            }
                            this.refresh(window, cx);
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
                    .pt_1()
                    .pb_4()
                    .gap_1()
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
                                cx.listener(move |this, _, window, cx| {
                                    this.selected_agent = if this.selected_agent == Some(agent) {
                                        None
                                    } else {
                                        Some(agent)
                                    };
                                    this.selected_project = None;
                                    this.favorite_only = false;
                                    this.refresh(window, cx);
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
                                cx.listener(move |this, _, window, cx| {
                                    this.selected_project = if this.selected_project.as_deref()
                                        == Some(path.as_str())
                                    {
                                        None
                                    } else {
                                        Some(path.clone())
                                    };
                                    this.selected_agent = None;
                                    this.favorite_only = false;
                                    this.refresh(window, cx);
                                }),
                                cx,
                            )
                        }))
                    }),
            )
            .when_some(status, |this, status| {
                this.child(
                    h_flex()
                        .flex_shrink_0()
                        .px(SPACE_XL)
                        .py_3()
                        .border_t_1()
                        .border_color(theme.sidebar_border)
                        .text_size(FONT_LABEL)
                        .child(status),
                )
            })
    }

    fn render_session_list(&self, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let shown = self.list_state.read(cx).delegate().rows.len();
        let sort_key = self.sort_key;
        let sort_ascending = self.sort_ascending;
        let sort_entity = cx.entity();
        let sort_label = match sort_key {
            SortKey::Updated => "Date updated",
            SortKey::Created => "Date created",
            SortKey::Messages => "Message count",
        };
        // ghost + 当前排序文案(outline/muted 胶囊都试过,用户定的 ghost)
        let sort_menu = Button::new("sort-sessions")
            .ghost()
            .small()
            .rounded(px(6.))
            .icon(icon("icons/arrow-up-down.svg").with_size(px(13.)))
            .label(sort_label)
            .tooltip("Sort sessions")
            .dropdown_menu(move |menu, _, _| {
                let mk_key = |label: &'static str, key: SortKey| {
                    let entity = sort_entity.clone();
                    PopupMenuItem::new(label).checked(sort_key == key).on_click(
                        move |_, window, cx| {
                            entity.update(cx, |this, cx| {
                                this.sort_key = key;
                                this.refresh(window, cx);
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
                                this.sort_ascending = ascending;
                                this.refresh(window, cx);
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
            .anchor(Corner::TopRight);
        v_flex()
            .w(px(336.))
            .h_full()
            .flex_shrink_0()
            .bg(theme.list)
            .child(
                v_flex()
                    .id("list-header")
                    .flex_shrink_0()
                    .window_control_area(WindowControlArea::Drag)
                    .px(SPACE_LG)
                    .pt(SPACE_XL)
                    .pb(SPACE_MD)
                    .child(
                        h_flex()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .text_size(FONT_TITLE)
                                    .font_semibold()
                                    .child(self.context_title()),
                            )
                            .child(sort_menu),
                    ),
            )
            .child(if shown == 0 {
                v_flex()
                    .flex_1()
                    .justify_center()
                    .child(empty_state(
                        "icons/inbox.svg",
                        px(48.),
                        px(22.),
                        "No matching sessions",
                        "Try different filters or clear the query",
                        cx,
                    ))
                    .into_any_element()
            } else {
                List::new(&self.list_state)
                    .flex_1()
                    .min_h_0()
                    .into_any_element()
            })
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
        let theme = cx.theme();
        let dark = theme.mode.is_dark();
        let muted_fg = theme.muted_foreground;
        // 尾部要用的 Copy 值提前取出,theme 借用不跨越 inner 构建期的 &mut cx
        let jump_bg = theme.primary.opacity(0.09);
        let jump_radius = theme.radius;
        let Some(detail) = &self.detail else {
            return div().into_any_element();
        };
        let total = detail.transcript.len();
        let expanded = detail.expanded_rows.contains(&ix);
        let jump_seq = detail.jump_seq;
        // Rc 克隆只加引用计数;逐行借用,避免每帧深拷贝整条消息(text 可达 32KB)
        let transcript = detail.transcript.clone();
        let Some(m) = transcript.get(ix) else {
            return div().into_any_element();
        };
        // 搜索跳转的落点消息:淡 primary 底色保持高亮,直到换会话
        let is_jump_target = jump_seq == Some(m.seq);

        let inner: AnyElement = if m.kind == MessageKind::CompactSummary {
            centered_pill("Context compacted", cx).into_any_element()
        } else {
            match m.role {
                Role::User => h_flex()
                    .w_full()
                    .justify_end()
                    .child(
                        div()
                            .max_w(px(540.))
                            .min_w_0()
                            .rounded(px(12.))
                            .bg(theme.muted)
                            .px(px(12.))
                            .py(px(7.))
                            .text_size(FONT_MSG_USER)
                            .child(
                                TextView::markdown(
                                    SharedString::from(format!("dmsg-{}", m.seq)),
                                    m.text.clone(),
                                    window,
                                    cx,
                                )
                                .style(TextViewStyle {
                                    heading_base_font_size: FONT_MSG_USER,
                                    paragraph_gap: gpui::rems(0.5),
                                    is_dark: dark,
                                    ..Default::default()
                                })
                                .selectable(true),
                            ),
                    )
                    .into_any_element(),
                Role::Assistant => {
                    let mut col = v_flex().w_full().min_w_0().gap(px(6.));
                    if let Some(th) = &m.thinking {
                        col = col.child(
                            div()
                                .text_size(FONT_MSG_THINKING)
                                .italic()
                                .text_color(muted_fg)
                                .truncate()
                                .child(format!("Thinking · {}", one_line(th, 200))),
                        );
                    }
                    if !m.text.is_empty() {
                        col = col.child(
                            div().text_size(FONT_MSG_BODY).child(
                                TextView::markdown(
                                    SharedString::from(format!("dmsg-{}", m.seq)),
                                    m.text.clone(),
                                    window,
                                    cx,
                                )
                                .style(TextViewStyle {
                                    heading_base_font_size: FONT_MSG_BODY,
                                    paragraph_gap: gpui::rems(0.6),
                                    is_dark: dark,
                                    ..Default::default()
                                })
                                .selectable(true),
                            ),
                        );
                    }
                    if !m.tool_calls.is_empty() {
                        col = col.child(tool_cluster(
                            ix,
                            &m.tool_calls,
                            expanded,
                            cx.listener(move |this, _, _window, cx| {
                                if let Some(detail) = &mut this.detail {
                                    if !detail.expanded_rows.insert(ix) {
                                        detail.expanded_rows.remove(&ix);
                                    }
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
                Role::System => centered_pill(one_line(&m.text, 120), cx).into_any_element(),
            }
        };

        div()
            .w_full()
            .flex()
            .justify_center()
            // 10px:用户定稿(2026-08-18),16 显大 8 显小,不在 4px 网格上是有意的
            .px(px(10.))
            .py(px(6.))
            .when(ix == 0, |d| d.pt(SPACE_LG))
            .when(ix + 1 == total, |d| d.pb(SPACE_XXL))
            .child(
                div()
                    .w_full()
                    .max_w(px(720.))
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

    fn render_detail(&self, _window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let Some(detail) = &self.detail else {
            return v_flex()
                .flex_1()
                .h_full()
                .items_center()
                .justify_center()
                .bg(theme.background)
                .child(
                    div()
                        .w(px(360.))
                        .px(SPACE_XXL)
                        .py(SPACE_XXL)
                        .rounded(theme.radius_lg)
                        .bg(theme.popover)
                        .child(empty_state(
                            "icons/message-square.svg",
                            px(58.),
                            px(26.),
                            "No session selected",
                            "Pick one from the list, or press ⌘K to search.",
                            cx,
                        )),
                )
                .into_any_element();
        };
        let meta = &detail.meta;
        let session_id = meta.id.clone();
        let export_entity = cx.entity();
        let reveal_entity = export_entity.clone();
        let delete_entity = export_entity.clone();
        let more_menu = Button::new("more-actions")
            .ghost()
            .rounded(px(6.))
            .icon(icon("icons/more-horizontal.svg").with_size(px(16.)))
            .dropdown_menu(move |menu, _, _| {
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
                        PopupMenuItem::new(" Reveal in Finder")
                            .icon(icon("icons/folder.svg").with_size(px(15.)))
                            .on_click(move |_, _, cx| {
                                reveal_entity.update(cx, |this, _| {
                                    if let Some(detail) = &this.detail {
                                        terminal::reveal_in_finder(&detail.meta.file_path);
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
                        PopupMenuItem::new(" Move to Trash")
                            .icon(icon("icons/trash-2.svg").with_size(px(15.)))
                            .on_click(move |_, window, cx| {
                                delete_entity.update(cx, |this, cx| {
                                    this.confirm_delete(window, cx);
                                });
                            }),
                    )
            })
            .anchor(Corner::TopRight);

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
                    .px(SPACE_LG)
                    .pt(SPACE_XL)
                    .pb(SPACE_MD)
                    .gap(SPACE_SM)
                    .child(
                        h_flex()
                            .gap_2()
                            .text_size(FONT_LABEL)
                            .text_color(theme.muted_foreground)
                            .child(img(meta.agent.brand_icon(theme.mode.is_dark())).size(px(15.)).flex_shrink_0())
                            .child(div().flex_shrink_0().child(meta.agent.display_name()))
                            .child(badge(meta.project_name.clone(), theme.muted, theme.muted_foreground))
                            .when_some(meta.git_branch.clone(), |this, branch| {
                                this.child(
                                    h_flex()
                                        .min_w_0()
                                        .gap_1()
                                        .child(icon("icons/git-branch.svg").with_size(px(11.)).flex_shrink_0())
                                        .child(div().min_w_0().truncate().child(branch)),
                                )
                            }),
                    )
                    .child(
                        h_flex()
                            .justify_between()
                            .gap_5()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_size(FONT_TITLE)
                                    .font_semibold()
                                    .truncate()
                                    .child(meta.title.clone()),
                            )
                            .child(
                                h_flex()
                                    .flex_shrink_0()
                                    .gap_1()
                                    .child({
                                        // Open In split 按钮(Codex/kooky 风):左段 = 上次
                                        // 目标的应用图标,点击直开;右段 chevron 展开列表
                                        let terms = terminal::installed_terminals();
                                        let current = self
                                            .preferred_terminal
                                            .or_else(|| terms.first().copied());
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
                                            .rounded(px(6.))
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
                                                    .child(match &current_icon {
                                                        Some(p) => img(p.clone())
                                                            .size(px(14.))
                                                            .into_any_element(),
                                                        None => icon("icons/terminal.svg")
                                                            .with_size(px(13.))
                                                            .text_color(theme.secondary_foreground)
                                                            .into_any_element(),
                                                    })
                                                    .on_click(cx.listener(move |this, _, window, cx| {
                                                        if let Some(term) = current {
                                                            this.do_resume(term, window, cx);
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
                                                                        .gap_2()
                                                                        .items_center()
                                                                        .child(match &icon_path {
                                                                            Some(p) => img(p.clone())
                                                                                .size(px(16.))
                                                                                .into_any_element(),
                                                                            None => icon("icons/terminal.svg")
                                                                                .with_size(px(15.))
                                                                                .into_any_element(),
                                                                        })
                                                                        .child(term.display_name())
                                                                })
                                                                .on_click(move |_, window, cx| {
                                                                    entity.update(cx, |this, cx| {
                                                                        this.do_resume(term, window, cx);
                                                                    });
                                                                }),
                                                            );
                                                        }
                                                        menu
                                                    })
                                                    .anchor(Corner::TopRight),
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
                        h_flex()
                            .gap_1p5()
                            .text_size(FONT_LABEL)
                            .text_color(theme.muted_foreground)
                            .child(icon("icons/folder.svg").with_size(px(12.)).flex_shrink_0())
                            .child(div().min_w_0().truncate().child(
                                if meta.project_path.is_empty() {
                                    "Unknown project".to_string()
                                } else {
                                    meta.project_path.clone()
                                },
                            )),
                    )
                    .child({
                        // 属性行:model / source outline badge + 统计;空值自动省略
                        let mut stats: Vec<String> = Vec::new();
                        if meta.message_count > 0 {
                            stats.push(format!("{} messages", meta.message_count));
                        }
                        if let Some(tokens) = meta.tokens_used {
                            stats.push(format!("{} tokens", fmt_tokens(Some(tokens))));
                        }
                        h_flex()
                            .gap_2()
                            .text_size(FONT_LABEL)
                            .text_color(theme.muted_foreground)
                            .when_some(meta.model.clone(), |this, model| {
                                this.child(outline_badge(
                                    model,
                                    rgb(crate::theme::MODEL_BADGE_BG).into(),
                                ))
                            })
                            .when_some(
                                meta.source.clone().filter(|s| !s.is_empty()),
                                |this, source| {
                                    // opencode2 是版本代际标记而非发起平台,
                                    // 用 primary 蓝与 via 徽章(绿)区分
                                    let color = if source == "opencode2" {
                                        theme.primary
                                    } else {
                                        theme.success
                                    };
                                    this.child(outline_badge(source, color))
                                },
                            )
                            .child(div().min_w_0().truncate().child(stats.join("  ·  ")))
                    })
                    .child({
                        // 时间行:创建 / 最后活动,精确时间
                        let mut times: Vec<String> = Vec::new();
                        if meta.created_at > 0 {
                            times.push(format!("Created {}", abs_date(meta.created_at)));
                        }
                        if meta.updated_at > 0 {
                            times.push(format!("Updated {}", abs_date(meta.updated_at)));
                        }
                        div()
                            .text_size(FONT_LABEL)
                            .text_color(theme.muted_foreground)
                            .truncate()
                            .child(times.join("  ·  "))
                    })
                    .child({
                        // 会话文件路径(header 末行):展示用折叠形态(~ 缩写 +
                        // 中段省略),点击在 Finder 中显示(传原始完整路径)
                        let file_path = meta.file_path.clone();
                        h_flex()
                            .id("detail-file-path")
                            .gap_1p5()
                            .text_size(FONT_LABEL)
                            .text_color(theme.muted_foreground)
                            .cursor_pointer()
                            .hover(|s| s.text_colored(theme.foreground, FONT_LABEL))
                            .on_click(move |_, _, _| {
                                terminal::reveal_in_finder(&file_path);
                            })
                            .child(icon("icons/file-text.svg").with_size(px(12.)).flex_shrink_0())
                            .child(div().min_w_0().truncate().child(display_file_path(&meta.file_path)))
                    }),
            )
            .child(if detail.loading {
                h_flex()
                    .flex_1()
                    .items_center()
                    .justify_center()
                    .gap_2()
                    .text_color(theme.muted_foreground)
                    .child(Spinner::new().small())
                    .child(div().text_size(FONT_BODY).child("Loading session…"))
                    .into_any_element()
            } else {
                let entity = cx.entity().downgrade();
                div()
                    .flex_1()
                    .min_h_0()
                    .px(SPACE_MD)
                    .pb(SPACE_MD)
                    .child(
                        div()
                            .size_full()
                            .rounded(theme.radius_lg)
                            .bg(theme.popover)
                            .relative()
                            .child(
                                gpui::list(detail.msg_list.clone(), move |ix, window, cx| {
                                    entity
                                        .upgrade()
                                        .map(|e| {
                                            e.update(cx, |this, cx| {
                                                this.render_msg_row(ix, window, cx)
                                            })
                                        })
                                        .unwrap_or_else(|| div().into_any_element())
                                })
                                .size_full(),
                            )
                            .vertical_scrollbar(&detail.msg_list),
                    )
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

/// 工具调用折叠簇:头行(chevron + 名字序列 + 失败计数)默认收起,
/// 展开为左竖线缩进列表;仅失败项内联输出(错误才是回顾重点)。
fn tool_cluster(
    ix: usize,
    calls: &[ToolCallView],
    expanded: bool,
    on_toggle: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    cx: &App,
) -> Div {
    let theme = cx.theme();
    let failed = calls.iter().filter(|c| c.is_error).count();
    let names = calls
        .iter()
        .map(|c| c.name.as_str())
        .collect::<Vec<_>>()
        .join(" · ");
    let head_label = if calls.len() == 1 {
        names.clone()
    } else {
        format!("{} tools · {}", calls.len(), names)
    };
    let mono = theme.mono_font_family.clone();

    let mut cluster = v_flex().w_full().min_w_0().gap(px(4.)).child(
        h_flex()
            .id(("tool-cluster", ix))
            .w_full()
            .min_w_0()
            .gap(px(6.))
            .cursor_pointer()
            .text_size(FONT_CAPTION)
            .text_color(theme.muted_foreground)
            .hover(|s| s.text_colored(theme.foreground, FONT_CAPTION))
            .child(
                icon("icons/chevron-right.svg")
                    .with_size(px(11.))
                    .flex_shrink_0()
                    .when(expanded, |ic| {
                        ic.rotate(gpui::Radians(std::f32::consts::FRAC_PI_2))
                    }),
            )
            .child(div().min_w_0().truncate().font_medium().child(head_label))
            .when(failed > 0, |this| {
                this.child(
                    div()
                        .flex_shrink_0()
                        .text_color(theme.danger)
                        .child(format!("{failed} failed")),
                )
            })
            .on_click(on_toggle),
    );
    if expanded {
        let mut items = v_flex()
            .w_full()
            .min_w_0()
            .ml(px(5.))
            .pl(px(12.))
            .border_l_1()
            .border_color(theme.border)
            .gap(px(6.));
        for tc in calls {
            let mut item = v_flex().w_full().min_w_0().gap(px(2.)).child(
                h_flex()
                    .w_full()
                    .min_w_0()
                    .gap(px(6.))
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_size(FONT_CAPTION)
                            .font_medium()
                            .text_color(if tc.is_error {
                                theme.danger
                            } else {
                                theme.foreground
                            })
                            .child(tc.name.clone()),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_size(FONT_MSG_THINKING)
                            .font_family(mono.clone())
                            .text_color(theme.muted_foreground)
                            .child(tc.input_preview.clone()),
                    ),
            );
            if tc.is_error {
                if let Some(out) = &tc.output {
                    let shown: String = out.trim().chars().take(400).collect();
                    if !shown.is_empty() {
                        item = item.child(
                            div()
                                .w_full()
                                .min_w_0()
                                .px(px(8.))
                                .py(px(5.))
                                .rounded(px(5.))
                                .bg(theme.muted)
                                .text_size(FONT_LABEL)
                                .font_family(mono.clone())
                                .text_color(theme.muted_foreground)
                                .child(shown),
                        );
                    }
                }
            }
            items = items.child(item);
        }
        cluster = cluster.child(items);
    }
    cluster
}

/// 小胶囊 badge(项目名/model/source 共用):4px 圆角,内部截断。
/// 项目名用 muted 灰;model/source 用主题色 tint(淡底+同色文字)。
fn badge(name: impl Into<SharedString>, bg: Hsla, fg: Hsla) -> impl IntoElement {
    div()
        .min_w_0()
        .px(px(6.))
        .py(px(1.))
        .rounded(px(4.))
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
        .rounded(px(4.))
        .border_1()
        .border_color(color)
        .text_color(color)
        .font_medium()
        .child(div().truncate().child(name.into()))
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
                    this.child(
                        div()
                            .flex_shrink_0()
                            .text_size(FONT_LABEL)
                            .text_color(theme.muted_foreground)
                            .child(n.to_string()),
                    )
                }),
        )
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
        .rounded(px(6.))
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
        let theme = cx.theme();
        div()
            .id("workbench")
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::toggle_search))
            .on_action(cx.listener(|this, _: &RefreshSessions, window, cx| {
                this.refresh_sessions(window, cx)
            }))
            .size_full()
            .bg(theme.background)
            .text_color(theme.foreground)
            .child(
                h_flex()
                    .size_full()
                    .child(self.render_sidebar(cx))
                    .child(self.render_session_list(cx))
                    .child(self.render_detail(window, cx)),
            )
            .children(Root::render_dialog_layer(window, cx))
            .children(Root::render_notification_layer(window, cx))
    }
}
