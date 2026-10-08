//! Memory 页:agent 自己写下的记忆文档(Claude Code 的 auto-memory、Codex 的
//! memories)的只读浏览。侧栏底部入口,整页目的地——与 Insights 同形制:替换中栏与
//! 阅读区、与导航行互斥(`Workbench::page`)。左列按项目分组的文件列表,右侧阅读面
//! 渲染 Markdown。数据来自 memories 表(scanner 每轮扫描收尾刷新),正文阅读时先读
//! 磁盘、读不到用库里那份。只看不改:动作只有 Reveal 与 Copy path——写别家的记忆
//! 是别家的事(用户 2026-09-17 定:整页目的地、只读;MCP 面另有 wake_list_memories)

use super::*;
use wake_core::adapters::memory_body;
use wake_core::models::{
    MemoryCounts, MemoryDoc, MemoryFilter, MemoryGroup, MemoryScope, UserMemories,
};

/// 左列的一行:时间分组头(带该组第一份文档的下标,标签从它的时间算)或一份文档
#[derive(Clone, Copy)]
enum MemoryRow {
    Group(usize),
    Doc(usize),
}

/// 选中文档在后台读好的部分:正文(SharedString——TextView 每帧要一份,Arc 一拨就够),
/// 以及解析过的来源路径(SQLite 型的虚拟路径落到库文件本体——解析要 stat,不能在
/// render 里每帧做)
struct LoadedMemory {
    body: SharedString,
    source_path: String,
}

/// 侧栏导航的筛选(单选互斥,与会话侧栏同一模型:再点当前项回到 All)
#[derive(Debug, Clone, PartialEq, Eq)]
enum MemoryNav {
    All,
    /// All Memory 下的 User memory 行(会话侧栏 Starred 的同位):只列用户记忆
    User,
    Agent(AgentId),
    /// 空串是没归属的一组(侧栏的 Unknown project 行)
    Project(String),
}

pub(super) struct MemoryState {
    /// 按更新时间倒序(GUI 自己排;store 给 MCP 的序是按项目、用户级最后);行里**不带
    /// 正文**(列表列给的是空串),选中时按 key 另取
    docs: Vec<MemoryDoc>,
    /// 组头 + 文档扁平成行,交 gpui::list 虚拟化——只画看得见的几行,行文案在画
    /// 的时候现算(译文与相对时间都不缓存,换语言不会留旧串)
    rows: Vec<MemoryRow>,
    list: gpui::ListState,
    loading: bool,
    selected: Option<String>,
    /// 选中后后台读,读到前是 None(转圈)
    loaded: Option<LoadedMemory>,
    /// 侧栏导航的计数(All Memory / User memory / Agents / Projects),与 docs 同一次
    /// 查询刷新
    counts: MemoryCounts,
    nav: MemoryNav,
    /// 筛选刚换过(进页归零、点了导航行):下一次重载必须查(扫描进行中也不按住)并把
    /// 列表拉回顶部、不恢复旧滚动位置
    scope_changed: bool,
    /// 进行中的列表/正文任务;新任务覆盖旧值即取消
    load_task: Option<Task<()>>,
    body_task: Option<Task<()>>,
    /// 页头 Refresh 起的记忆同步(只同步记忆,不重扫会话)进行中;期间再来的请求
    /// (Settings 改了 Memory locations)记 `sync_pending`,跑完补一次。任务本身 detach:
    /// 有 `syncing` 守着就不会重叠,不必再攥着句柄
    pub(super) syncing: bool,
    sync_pending: bool,
}

impl MemoryState {
    /// 换过缩放档位:行高按旧倍数量过,按比例重量(见 Workbench::sync_zoom)
    pub(super) fn remeasure(&self) {
        self.list.remeasure();
    }
}

impl Default for MemoryState {
    fn default() -> Self {
        Self {
            docs: Vec::new(),
            rows: Vec::new(),
            list: gpui::ListState::new(0, gpui::ListAlignment::Top, zpx(200.)),
            loading: false,
            selected: None,
            loaded: None,
            counts: MemoryCounts::default(),
            nav: MemoryNav::All,
            scope_changed: false,
            load_task: None,
            body_task: None,
            syncing: false,
            sync_pending: false,
        }
    }
}

impl Workbench {
    /// 用户记忆要不要置顶单列一组:侧栏选了 User memory 行时整页都是它,组头没意义、照时间分
    fn memory_pins_user(&self) -> bool {
        self.memory.nav != MemoryNav::User
    }

    /// 进 Memory 页时从"全部"开始
    pub(super) fn reset_memory_scope(&mut self) {
        self.memory.nav = MemoryNav::All;
        self.memory.scope_changed = true;
    }

    /// 侧栏导航行的唯一写入点(与会话侧栏的 set_scope 同形)
    fn set_memory_scope(&mut self, nav: MemoryNav, cx: &mut Context<Self>) {
        self.memory.nav = nav;
        self.memory.scope_changed = true;
        self.reload_memories(cx);
    }

    /// Memory 页页头的 Refresh:只同步记忆来源——读设置里当下的 Memory locations,不重扫
    /// 会话、不碰远程、不弹进度框;会话页的 Refresh 是整库重扫(用户 2026-09-22 定各刷
    /// 各的)。进行中侧栏状态行转圈 + 不定值进度条,跑完弹 "Memory refreshed"(会话那边是
    /// "Sessions refreshed")——同步只要几十毫秒,没有这两样就是"点了没反应"
    pub(super) fn refresh_memories(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.start_memory_sync(
            Some((
                window.window_handle(),
                Notification::success(t("Memory refreshed")),
            )),
            cx,
        );
    }

    /// Settings → Memory locations 变更的收尾:同一条同步,不另弹通知(变更本身已提示)
    pub(super) fn sync_memories_quietly(&mut self, cx: &mut Context<Self>) {
        self.start_memory_sync(None, cx);
    }

    /// 扫描终态:扫描期间排队的记忆同步补上(`on_bg_event` 的终态分支调)
    pub(super) fn drain_pending_memory_sync(&mut self, cx: &mut Context<Self>) {
        if !self.memory.syncing && std::mem::take(&mut self.memory.sync_pending) {
            self.sync_memories_quietly(cx);
        }
    }

    /// 同步或扫描进行中都排队(`sync_pending`),跑完 / 扫描终态再补一次:扫描收尾的
    /// sync_memories 跑完到 UI 收到终态事件之间有空档,那时落库的 Memory locations 变更
    /// 会被漏掉(2026-09-22 review);按钮那时是禁用的,能撞上的只有 Settings 变更。
    /// 跑完重载列表并 notify,Settings 的 Memory locations 页据此重读计数;`done` 的通知
    /// 经窗口句柄投递——同步本身不依赖窗口,终态补跑那条路手里没有窗口
    fn start_memory_sync(
        &mut self,
        done: Option<(gpui::AnyWindowHandle, Notification)>,
        cx: &mut Context<Self>,
    ) {
        if self.memory.syncing || self.scan.scanning {
            self.memory.sync_pending = true;
            return;
        }
        self.memory.syncing = true;
        cx.notify();
        let adapters = self.adapters.clone();
        let store = self.store.clone();
        let lock = self.index_lock.clone();
        let task = cx.background_spawn(async move {
            let _lock = lock;
            wake_core::scanner::run_memory_sync(&adapters, &store)
        });
        cx.spawn(async move |this, cx| {
            task.await;
            this.update(cx, |this, cx| {
                this.memory.syncing = false;
                if std::mem::take(&mut this.memory.sync_pending) {
                    this.sync_memories_quietly(cx);
                }
                this.reload_memories(cx);
                cx.notify();
            })
            .ok();
            if let Some((window, note)) = done {
                cx.update_window(window, |_, window, cx| window.push_notification(note, cx))
                    .ok();
            }
        })
        .detach();
    }

    /// refresh 顺带重载(与 reload_insights 同规矩):扫描进行中且已有数据就按住,
    /// 终态 Progress 补最后一次——**筛选刚换过除外**:这里也是侧栏导航的唯一落点,
    /// 按住就是点了没反应、标题换了列表没换(2026-09-21 review)。选中项还在就保留,
    /// 否则落到第一份
    pub(super) fn reload_memories(&mut self, cx: &mut Context<Self>) {
        if self.page != Page::Memory {
            return;
        }
        if self.scan.scanning && !self.memory.docs.is_empty() && !self.memory.scope_changed {
            return;
        }
        self.memory.loading = self.memory.docs.is_empty();
        let store = self.store.clone();
        // 侧栏的 "Unknown project" 行是 `Project("")`:筛的是没归属的一组,不是路径为空串
        // 的项目。项目行只列项目记忆——用户记忆有自己的 User memory 行(用户 2026-09-22
        // 定);MCP 问一个项目仍连用户记忆一起给(`UserMemories` 的默认值 Alongside)
        let filter = match &self.memory.nav {
            MemoryNav::All => MemoryFilter::default(),
            MemoryNav::User => MemoryFilter {
                user: UserMemories::Only,
                ..Default::default()
            },
            MemoryNav::Agent(agent) => MemoryFilter {
                agents: vec![*agent],
                ..Default::default()
            },
            MemoryNav::Project(path) => MemoryFilter {
                project_paths: if path.is_empty() {
                    Vec::new()
                } else {
                    vec![path.clone()]
                },
                unattributed: path.is_empty(),
                user: UserMemories::Excluded,
                ..Default::default()
            },
        };
        let task = cx.background_spawn(async move {
            let docs = store.list_memories(&filter)?;
            let counts = store.memory_counts()?;
            anyhow::Ok((docs, counts))
        });
        self.memory.load_task = Some(cx.spawn(async move |this, cx| {
            let loaded = task.await;
            this.update(cx, |this, cx| {
                if let Ok((mut docs, counts)) = loaded {
                    this.memory.counts = counts;
                    // 用户记忆对每个项目都成立、没有"发生时间"的意义,像会话列表的 Pinned
                    // 那样单独一组置顶;其余按更新时间倒序进时间桶
                    docs.sort_by(|a, b| {
                        (a.scope != MemoryScope::User)
                            .cmp(&(b.scope != MemoryScope::User))
                            .then_with(|| b.updated_at.cmp(&a.updated_at))
                            .then_with(|| a.key.cmp(&b.key))
                    });
                    let pin_user = this.memory_pins_user();
                    let rows = memory_rows(&docs, Local::now().date_naive(), pin_user);
                    // 重载来自每轮扫描,滚动位置要留住:splice 整段会把落在段内的
                    // 滚动锚点归零(gpui 的 splice 只保段外的锚点),所以先记下再恢复;
                    // 筛选刚换过则回到顶部
                    let top = if this.memory.scope_changed {
                        gpui::ListOffset {
                            item_ix: 0,
                            offset_in_item: px(0.),
                        }
                    } else {
                        this.memory.list.logical_scroll_top()
                    };
                    this.memory.scope_changed = false;
                    let old = this.memory.rows.len();
                    this.memory.list.splice(0..old, rows.len());
                    this.memory.list.scroll_to(top);
                    // 选中项:还在且没变就不动;正文被 agent 改过(时间或大小变了)
                    // 重读,否则新标题配旧正文;没了就落到第一份
                    let stamp = |docs: &[MemoryDoc], key: &str| {
                        docs.iter()
                            .find(|d| d.key == key)
                            .map(|d| (d.updated_at, d.size_bytes))
                    };
                    let selected = this.memory.selected.clone();
                    let before = selected
                        .as_deref()
                        .and_then(|k| stamp(&this.memory.docs, k));
                    let after = selected.as_deref().and_then(|k| stamp(&docs, k));
                    this.memory.docs = docs;
                    this.memory.rows = rows;
                    match (selected, after) {
                        (Some(key), Some(now)) if before != Some(now) => {
                            this.select_memory(key, cx);
                        }
                        (Some(_), Some(_)) => {}
                        _ => {
                            this.memory.selected = None;
                            this.memory.loaded = None;
                            if let Some(first) = this.memory.docs.first().map(|d| d.key.clone()) {
                                this.select_memory(first, cx);
                            }
                        }
                    }
                }
                this.memory.loading = false;
                cx.notify();
            })
            .ok();
        }));
    }

    fn select_memory(&mut self, key: String, cx: &mut Context<Self>) {
        let Some(path) = self
            .memory
            .docs
            .iter()
            .find(|d| d.key == key)
            .map(|d| d.path.clone())
        else {
            return;
        };
        self.memory.selected = Some(key.clone());
        self.memory.loaded = None;
        // 列表行不带正文,选中时按 key 另取一行;正文与来源路径都在后台算:两者都要
        // stat(虚拟路径解析、现场读文件),自定义 location 挂在慢盘上时不能卡 UI 线程
        let store = self.store.clone();
        let task = {
            let key = key.clone();
            cx.background_spawn(async move {
                let doc = store
                    .get_memory(&key)?
                    .ok_or_else(|| anyhow::anyhow!("memory {key} is no longer indexed"))?;
                anyhow::Ok(LoadedMemory {
                    source_path: session_source_path(&doc.path).to_string(),
                    body: memory_body(&doc).into(),
                })
            })
        };
        self.memory.body_task = Some(cx.spawn(async move |this, cx| {
            let loaded = task.await;
            this.update(cx, |this, cx| {
                // 只写回仍然选中的那份,快速切换时旧任务不覆盖新选择
                if this.memory.selected.as_deref() != Some(key.as_str()) {
                    return;
                }
                // 读不出(刚被扫描清掉、库读错)也得收场,不能让阅读面一直转圈;来源路径
                // 同样要解析过(虚拟路径落到库文件),Copy path 才不会给出 `<db>#<id>`
                this.memory.loaded = Some(loaded.unwrap_or_else(|e| LoadedMemory {
                    source_path: session_source_path(&path).to_string(),
                    body:
                        crate::tf!("Could not read this memory file: {}", format!("{e:#}")).into(),
                }));
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// 会话页同一形制(用户 2026-09-21:整页宽的页头压着两栏"看着很奇怪"):左列 =
    /// 列内页头 + 记忆流,右侧 = 阅读面(详情页同款头部 + popover 正文)
    pub(super) fn render_memory(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let count = self.memory.docs.len();
        let subtitle: SharedString = crate::tp!("{} memory file", "{} memory files", count).into();
        let column: AnyElement = if self.memory.docs.is_empty() {
            div().flex_1().into_any_element()
        } else {
            self.render_memory_list(cx)
        };
        // 列表非空时选中项恒有(reload 落到第一份),阅读面跟着它;空库给空态卡
        let selected = self
            .memory
            .selected
            .as_ref()
            .and_then(|k| self.memory.docs.iter().find(|d| &d.key == k));
        let pane: AnyElement = match selected {
            Some(doc) => self.render_memory_reader(doc, cx),
            None => v_flex()
                .flex_1()
                .h_full()
                .items_center()
                .justify_center()
                .bg(theme.background)
                .child(if self.memory.loading {
                    Spinner::new().into_any_element()
                } else {
                    empty_state_card(
                        "icons/file-text.svg",
                        zpx(58.),
                        zpx(24.),
                        t("No memory files yet"),
                        t("Agents write these as they work: Claude Code's auto-memory, Codex's memories, ZCode's project memory. They show up here once they exist."),
                        cx,
                    )
                    .into_any_element()
                })
                .into_any_element(),
        };
        h_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .child(
                v_flex()
                    .w(self.columns().1)
                    .h_full()
                    .flex_shrink_0()
                    .bg(theme.colors.list)
                    .child(library_header(
                        "memory-header",
                        self.memory_context_title(),
                        subtitle,
                        SPACE_LG,
                        Some(self.refresh_button(
                            self.scan.scanning || self.memory.syncing,
                            Self::refresh_memories,
                            cx,
                        )),
                        cx,
                    ))
                    .child(column),
            )
            .child(pane)
            .into_any_element()
    }

    /// 左列页头的标题 = 当前侧栏筛选(与会话页 context_title 同规矩)
    fn memory_context_title(&self) -> String {
        match &self.memory.nav {
            MemoryNav::All => t("All Memory").to_string(),
            MemoryNav::User => t("User memory").to_string(),
            MemoryNav::Agent(agent) => agent.display_name().to_string(),
            MemoryNav::Project(path) if path.is_empty() => t("Unknown project").to_string(),
            MemoryNav::Project(path) => self
                .memory
                .counts
                .projects
                .iter()
                .find(|p| &p.path == path)
                .map(|p| p.name.clone())
                .unwrap_or_else(|| t("Projects").to_string()),
        }
    }

    /// 记忆流:gpui::list 虚拟化(行高不等:分组头与两行的文档行)
    fn render_memory_list(&self, cx: &Context<Self>) -> AnyElement {
        let entity = cx.entity().downgrade();
        div()
            .flex_1()
            .min_h_0()
            .w_full()
            .relative()
            .child(
                gpui::list(self.memory.list.clone(), move |ix, _window, cx| {
                    entity
                        .upgrade()
                        .map(|e| e.update(cx, |this, cx| this.render_memory_row(ix, cx)))
                        .unwrap_or_else(|| div().into_any_element())
                })
                .size_full(),
            )
            .vertical_scrollbar(&self.memory.list)
            .into_any_element()
    }

    /// 一行:分组头(会话流的时间分割线同款),或两行的文档行(会话行同款:Body 14
    /// medium 标题一行;Label 11 的品牌图 + 归属徽章 + 右对齐时间)
    fn render_memory_row(&mut self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let Some(row) = self.memory.rows.get(ix).copied() else {
            return div().into_any_element();
        };
        match row {
            MemoryRow::Group(first) => {
                // 组头 = 置顶的 User memory,或会话流同一套时间分割线(Today / Yesterday / …);
                // 文案画的时候现算
                let pin_user = self.memory_pins_user();
                let label = self
                    .memory
                    .docs
                    .get(first)
                    .map(|d| memory_section_label(d, Local::now().date_naive(), pin_user))
                    .unwrap_or_default();
                section_header_row(label, &theme).into_any_element()
            }
            MemoryRow::Doc(di) => {
                let Some(d) = self.memory.docs.get(di) else {
                    return div().into_any_element();
                };
                let selected = self.memory.selected.as_deref() == Some(d.key.as_str());
                let key = d.key.clone();
                let title: SharedString = clip_display(&d.title, SESSION_TITLE_MAX_WIDTH).into();
                let title_tooltip: SharedString = d.title.clone().into();
                let shown_time: SharedString = smart_time(d.updated_at).into();
                let shown_tooltip: SharedString = abs_date(d.updated_at).into();
                // 盒模型逐项照会话行:ListItem 自带 px_3 / py_1(12 / 4)再套行内容的
                // SPACE_XS / SPACE_SM(4 / 8),高亮面 mx = SPACE_SM——这里没有 ListItem,
                // 把两层加成一层写死同样的数。不缩进、不画竖线(树的子行形制试过,用户
                // 2026-09-21 定没必要)
                div()
                    .w_full()
                    .px(SPACE_SM)
                    .child(
                        v_flex()
                            .id(("memory-row", ix))
                            .w_full()
                            .min_w_0()
                            .rounded(theme.radius)
                            .px(SPACE_MD + SPACE_XS)
                            .py(SPACE_XS + SPACE_SM)
                            .gap(SPACE_XS)
                            .cursor_pointer()
                            .when(selected, |row| row.bg(theme.list_active))
                            .when(!selected, |row| {
                                row.hover(|style| style.bg(theme.list_hover))
                            })
                            .on_click(cx.listener(move |this, _, _window, cx| {
                                this.select_memory(key.clone(), cx)
                            }))
                            .child(
                                div()
                                    .id(("memory-title", ix))
                                    .w_full()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_size(FONT_BODY)
                                    .font_medium()
                                    .text_color(theme.foreground)
                                    .child(title)
                                    .tooltip(move |window, cx| {
                                        gpui_component::tooltip::Tooltip::new(title_tooltip.clone())
                                            .build(window, cx)
                                    }),
                            )
                            .child(
                                h_flex()
                                    .gap(ICON_TEXT_GAP)
                                    .text_size(FONT_LABEL)
                                    .text_color(theme.muted_foreground)
                                    .child(
                                        img(d.agent.brand_icon(theme.mode.is_dark()))
                                            .size(zpx(15.))
                                            .flex_shrink_0(),
                                    )
                                    // 项目徽章与会话行同款(muted 胶囊;用户级 / 没归属的
                                    // 也用组头同一套文案),用户 2026-09-21 定:组头说明了
                                    // 也要挂,和会话列表看起来一致
                                    .child(badge(
                                        memory_group_label(d),
                                        theme.muted,
                                        theme.muted_foreground,
                                    ))
                                    .children(memory_badges(d, &theme))
                                    .child(div().flex_1())
                                    .child(
                                        div()
                                            .id(("memory-time", ix))
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
                    )
                    .into_any_element()
            }
        }
    }

    /// 阅读面,详情页同款头部(`detail_header_frame`):上下文行是品牌图 + agent + 归属
    /// 徽章,右端 Reveal / Copy path;元信息是文件路径与更新时间;正文 popover 底、
    /// 720 阅读宽居中,与消息正文同一套 Markdown 渲染
    fn render_memory_reader(&self, doc: &MemoryDoc, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let dark = theme.mode.is_dark();
        // SQLite 型的虚拟路径落到库文件本体,Reveal 与 Copy 才有东西可指;解析在
        // select_memory 的后台任务里做好,读到之前按 scope 剥虚拟后缀顶一下(不 stat)
        let source_path = self
            .memory
            .loaded
            .as_ref()
            .map(|l| l.source_path.clone())
            .unwrap_or_else(|| memory_display_path(doc));
        let reveal_path = source_path.clone();
        let copy_path = source_path.clone();
        // 相对链接按记忆文件所在目录解析(MEMORY.md 链接的是同目录的主题文件),
        // 不是项目根;SQLite 型没有目录,退项目路径
        let link_base = if source_path == doc.path {
            std::path::Path::new(&doc.path)
                .parent()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default()
        } else {
            doc.project_path.clone()
        };
        let updated: SharedString = crate::tf!("Updated {}", smart_time(doc.updated_at)).into();
        let updated_tooltip: SharedString =
            crate::tf!("Updated {}", abs_date(doc.updated_at)).into();
        let mut lead: Vec<AnyElement> = vec![
            img(doc.agent.brand_icon(dark))
                .size(zpx(15.))
                .flex_shrink_0()
                .into_any_element(),
            div()
                .flex_shrink_0()
                .child(doc.agent.display_name())
                .into_any_element(),
            project_badge(
                "memory-project",
                &doc.project_path,
                memory_group_label(doc),
                &theme,
            ),
        ];
        lead.extend(memory_badges(doc, &theme));
        let actions: Vec<AnyElement> = vec![
            Button::new("memory-reveal")
                .ghost()
                .rounded(RADIUS_BUTTON)
                .icon(icon("icons/folder.svg").with_size(zpx(16.)))
                .tooltip(reveal_in_fm())
                .on_click(move |_, _, _| {
                    terminal::reveal_in_file_manager(&reveal_path);
                })
                .into_any_element(),
            Button::new("memory-copy-path")
                .ghost()
                .rounded(RADIUS_BUTTON)
                .icon(icon("icons/copy.svg").with_size(zpx(16.)))
                .tooltip(t("Copy path"))
                .on_click(move |_, _, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(copy_path.clone()));
                })
                .into_any_element(),
        ];
        let meta_rows: Vec<AnyElement> = vec![
            h_flex()
                .min_w_0()
                .gap(ICON_TEXT_GAP)
                .child(
                    icon("icons/file-text.svg")
                        .with_size(zpx(12.))
                        .flex_shrink_0(),
                )
                .child(div().min_w_0().truncate().child(tilde_path(&source_path)))
                .into_any_element(),
            h_flex()
                .min_w_0()
                .gap(ICON_TEXT_GAP)
                .items_center()
                .child(
                    icon("icons/calendar.svg")
                        .with_size(zpx(12.))
                        .flex_shrink_0(),
                )
                .child(
                    div()
                        .id("memory-updated")
                        .min_w_0()
                        .truncate()
                        .child(updated)
                        .tooltip(move |window, cx| {
                            gpui_component::tooltip::Tooltip::new(updated_tooltip.clone())
                                .build(window, cx)
                        }),
                )
                .into_any_element(),
        ];
        let header = detail_header_frame(
            "memory-detail-header",
            lead,
            actions,
            doc.title.clone().into(),
            meta_rows,
            &theme,
        );
        let content: AnyElement = match &self.memory.loaded {
            Some(loaded) => markdown_body(
                format!("memory-{}", doc.key).into(),
                loaded.body.clone(),
                &doc.host,
                &link_base,
                FONT_MSG_BODY,
                gpui::rems(0.5),
                dark,
                cx,
            )
            .into_any_element(),
            None => div()
                .w_full()
                .flex()
                .justify_center()
                .py(SPACE_XXL)
                .child(Spinner::new())
                .into_any_element(),
        };
        v_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .bg(theme.background)
            .child(header)
            .child(
                div()
                    .id("memory-reader")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .bg(theme.popover)
                    // 与详情正文同一组数:24px 阅读轴,首条上留 16、末条下留 24
                    .px(SPACE_XXL)
                    .pt(SPACE_LG)
                    .pb(SPACE_XXL)
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .justify_center()
                            .child(div().w_full().max_w(READER_MAX_WIDTH).child(content)),
                    ),
            )
            .into_any_element()
    }

    /// Memory 页的侧栏导航,替换会话导航(搜索框、All Sessions / Starred、Agents /
    /// Projects):All Memory / User memory 两行,再接 Agents / Projects 两组
    /// (`sidebar_groups`,与会话侧栏同一套组件、同一个单选模型)按记忆文件计数
    pub(super) fn render_memory_nav(&self, cx: &Context<Self>) -> AnyElement {
        let counts = &self.memory.counts;
        let nav = &self.memory.nav;
        v_flex()
            .flex_1()
            .min_h_0()
            .child(
                v_flex()
                    .flex_shrink_0()
                    .px(SIDEBAR_EDGE)
                    .pb(SPACE_XS)
                    .gap(SPACE_XS)
                    // 不放模式标题:当前页由底部工具条点亮的那颗说明(曾加过一行
                    // brain + "Memory",用户 2026-09-21 定没必要)
                    .child(sidebar_row(
                        "memory-all",
                        RowLead::Icon(icon("icons/file-text.svg")),
                        t("All Memory"),
                        Some(counts.total),
                        *nav == MemoryNav::All,
                        RowLevel::Primary,
                        cx.listener(|this, _, _window, cx| {
                            this.set_memory_scope(MemoryNav::All, cx);
                        }),
                        cx,
                    ))
                    // 会话侧栏 Starred 的同位(用户 2026-09-22 定):只列用户记忆,项目行
                    // 里不再混它;再点一次回到全部,计数为零不挂徽章
                    .child(sidebar_row(
                        "memory-user",
                        RowLead::Icon(icon("icons/user.svg")),
                        t("User memory"),
                        (counts.user > 0).then_some(counts.user),
                        *nav == MemoryNav::User,
                        RowLevel::Primary,
                        cx.listener(|this, _, _window, cx| {
                            let next = if this.memory.nav == MemoryNav::User {
                                MemoryNav::All
                            } else {
                                MemoryNav::User
                            };
                            this.set_memory_scope(next, cx);
                        }),
                        cx,
                    )),
            )
            .child(self.sidebar_groups(
                "memory-sidebar-scroll",
                counts.agents.iter().map(|(agent, count)| {
                    (
                        *agent,
                        *count,
                        matches!(nav, MemoryNav::Agent(sel) if sel == agent),
                    )
                }),
                counts.projects.iter().map(|p| {
                    let label: SharedString = if p.path.is_empty() {
                        t("Unknown project").into()
                    } else {
                        p.name.clone().into()
                    };
                    (
                        p.path.clone(),
                        label,
                        p.count,
                        matches!(nav, MemoryNav::Project(sel) if *sel == p.path),
                    )
                }),
                |this, next, cx| {
                    this.set_memory_scope(next.map_or(MemoryNav::All, MemoryNav::Agent), cx)
                },
                |this, next, cx| {
                    this.set_memory_scope(next.map_or(MemoryNav::All, MemoryNav::Project), cx)
                },
                cx,
            ))
            .into_any_element()
    }
}

/// 组头 + 文档扁平成行:`docs` 已排成"用户记忆在前,其余按更新时间倒序",用户记忆
/// 自成一组置顶(会话列表的 Pinned 同位),其余按会话列表同一套时间桶
/// (`session_group_label`)切组。按项目分组的形制(树、平铺组头)都试过,行上有了
/// 项目徽章之后用户 2026-09-21 定与会话列表完全一致;用户记忆混进时间线"有点奇怪",
/// 同日再定置顶单列;侧栏选了 User memory 行时整页都是它(`pin_user` 为假),那个
/// 组头就没意义,照时间分
fn memory_rows(docs: &[MemoryDoc], today: NaiveDate, pin_user: bool) -> Vec<MemoryRow> {
    let mut rows = Vec::with_capacity(docs.len() + 8);
    let mut current: Option<SharedString> = None;
    for (ix, d) in docs.iter().enumerate() {
        let label = memory_section_label(d, today, pin_user);
        if current.as_ref() != Some(&label) {
            rows.push(MemoryRow::Group(ix));
            current = Some(label);
        }
        rows.push(MemoryRow::Doc(ix));
    }
    rows
}

/// 一份文档落在哪个组头下:用户记忆 = "User memory"(`pin_user`),其余按更新时间进
/// 会话列表的时间桶
fn memory_section_label(d: &MemoryDoc, today: NaiveDate, pin_user: bool) -> SharedString {
    if pin_user && d.scope == MemoryScope::User {
        t("User memory").into()
    } else {
        session_group_label(d.updated_at, today)
    }
}

/// 读到 LoadedMemory 之前阅读面要显示的来源路径:线程记忆(Codex 的逐会话摘要)的
/// 虚拟路径 `<db>#<id>` 只剥后缀、不 stat(render 每帧调);文件型就是它自己
fn memory_display_path(d: &MemoryDoc) -> String {
    if d.scope == MemoryScope::Thread {
        d.path
            .rsplit_once('#')
            .map_or_else(|| d.path.clone(), |(db, _)| db.to_string())
    } else {
        d.path.clone()
    }
}

/// 组头 / 归属徽章的文案:项目名;用户级记忆一组、没有项目的一组
fn memory_group_label(d: &MemoryDoc) -> SharedString {
    match d.group() {
        MemoryGroup::User => t("User memory").into(),
        MemoryGroup::Unknown => t("Unknown project").into(),
        MemoryGroup::Project { name, .. } => name.to_string().into(),
    }
}

/// 文档行与阅读面头部共用的两枚徽章(有则画):Codex 逐会话记忆的 "session memory",
/// 远程记忆的 @host。指令文件不另挂徽章——文件名(CLAUDE.md、AGENTS.md)已说明身份,
/// 用户 2026-09-21 定不分
fn memory_badges(d: &MemoryDoc, theme: &gpui_component::Theme) -> Vec<AnyElement> {
    let mut out = Vec::with_capacity(2);
    if d.scope == MemoryScope::Thread {
        out.push(
            badge(t("session memory"), theme.muted, theme.muted_foreground).into_any_element(),
        );
    }
    if !d.host.is_empty() {
        out.push(host_badge(&d.host, theme).into_any_element());
    }
    out
}
