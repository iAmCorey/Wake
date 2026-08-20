//! 中栏会话列表:顶层 + 可展开子代理。
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use gpui::prelude::FluentBuilder as _;
use gpui::*;
use gpui_component::list::{ListDelegate, ListEvent, ListItem, ListState};
use gpui_component::{
    h_flex, v_flex, ActiveTheme as _, Icon, IndexPath, Sizable as _, StyledExt as _,
};

use wake_core::db::Store;
use wake_core::models::*;
use wake_core::nest::nest_session_rows;

use crate::format::relative_time;
use crate::theme::STAR_YELLOW;
use crate::ui::*;

fn icon(path: &'static str) -> Icon {
    Icon::empty().path(path)
}

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

pub struct SessionsDelegate {
    pub rows: Vec<SessionRow>,
    pub roots: Vec<SessionMeta>,
    pub children: HashMap<String, Vec<SessionMeta>>,
    pub child_counts: HashMap<String, i64>,
    pub expanded: HashSet<String>,
    pub sort_key: SortKey,
    pub ascending: bool,
    pub tree_mode: bool,
    pub store: Arc<Store>,
}

impl SessionsDelegate {
    pub fn new(store: Arc<Store>) -> Self {
        Self {
            rows: Vec::new(),
            roots: Vec::new(),
            children: HashMap::new(),
            child_counts: HashMap::new(),
            expanded: HashSet::new(),
            sort_key: SortKey::Updated,
            ascending: false,
            tree_mode: true,
            store,
        }
    }

    pub fn rebuild(&mut self) {
        self.rows = nest_session_rows(
            self.roots.clone(),
            &self.child_counts,
            &self.children,
            &self.expanded,
        );
    }

    /// 顶层刷新:保留仍可见的展开集,对展开节点重拉孩子。
    pub fn apply_roots(
        &mut self,
        roots: Vec<SessionMeta>,
        child_counts: HashMap<String, i64>,
        tree_mode: bool,
        sort_key: SortKey,
        ascending: bool,
    ) {
        self.roots = roots;
        self.sort_key = sort_key;
        self.ascending = ascending;
        self.tree_mode = tree_mode;
        if tree_mode {
            self.child_counts = child_counts;
            let root_keys: HashSet<String> = self.roots.iter().map(|s| s.key.clone()).collect();
            self.expanded.retain(|k| root_keys.contains(k));
            self.children.retain(|k, _| self.expanded.contains(k));
            let open: Vec<String> = self.expanded.iter().cloned().collect();
            for k in open {
                if let Ok(kids) = self.store.list_children(&k, self.sort_key, self.ascending) {
                    self.children.insert(k, kids);
                }
            }
        } else {
            self.child_counts.clear();
            self.children.clear();
            self.expanded.clear();
        }
        self.rebuild();
    }

    pub fn toggle(&mut self, key: &str) {
        if !self.expanded.remove(key) {
            self.expanded.insert(key.to_string());
            if let Ok(kids) = self.store.list_children(key, self.sort_key, self.ascending) {
                self.children.insert(key.to_string(), kids);
            }
        } else {
            self.children.remove(key);
        }
        self.rebuild();
    }

    pub fn ensure_expanded(&mut self, parent_key: &str) {
        if self.expanded.insert(parent_key.to_string()) {
            if let Ok(kids) = self
                .store
                .list_children(parent_key, self.sort_key, self.ascending)
            {
                self.children.insert(parent_key.to_string(), kids);
            }
            self.rebuild();
        }
    }
}

impl ListDelegate for SessionsDelegate {
    type Item = ListItem;

    fn items_count(&self, _section: usize, _cx: &App) -> usize {
        self.rows.len()
    }

    fn render_item(
        &mut self,
        ix: IndexPath,
        _window: &mut Window,
        cx: &mut Context<ListState<Self>>,
    ) -> Option<Self::Item> {
        let s = self.rows.get(ix.row)?;
        let theme = cx.theme();
        let key = s.meta.key.clone();
        let toggle_key = key.clone();
        let open_key = key.clone();
        let child_count = s.child_count;
        let expanded = s.expanded;
        let depth = s.depth;
        let title = s.meta.title.clone();
        let pinned = s.meta.pinned;
        let favorite = s.meta.favorite;
        let agent_icon = s.meta.agent.brand_icon(theme.mode.is_dark());
        let project_name = s.meta.project_name.clone();
        let updated_at = s.meta.updated_at;
        let show_chevron = self.tree_mode && child_count > 0;

        Some(
            // 行身份用会话 key,不能用 row:展开后子会话占用原下一行的下标,
            // GPUI 会复用旧元素的 click 状态,第一次点第一条孩子对不上 Confirm。
            ListItem::new(SharedString::from(key.clone()))
                .rounded(theme.radius)
                .mx(SPACE_SM)
                .child(
                    v_flex()
                        .id(SharedString::from(format!("row:{key}")))
                        .w_full()
                        .px(SPACE_XS)
                        .py(SPACE_SM)
                        .gap(SPACE_XS)
                        .when(depth > 0, |el| el.pl(px(16.)))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _, window, cx| {
                                // 按 key 定位,不靠 List 外层 on_click 的捕获下标。
                                // mouse_down 立刻 Confirm,避开展开后 hitbox 位移吞掉 click。
                                cx.stop_propagation();
                                let Some(row) = this
                                    .delegate()
                                    .rows
                                    .iter()
                                    .position(|r| r.meta.key == open_key)
                                else {
                                    return;
                                };
                                let already = this.selected_index().is_some_and(|ix| ix.row == row);
                                let grouped = this.delegate().tree_mode
                                    && this
                                        .delegate()
                                        .rows
                                        .get(row)
                                        .is_some_and(|r| r.child_count > 0);
                                // 已选中的分组行再点一次:展开/收起,不重复打开详情
                                if already && grouped {
                                    this.delegate_mut().toggle(&open_key);
                                    cx.notify();
                                    return;
                                }
                                let ix = IndexPath::new(row);
                                this.focus(window, cx);
                                this.set_selected_index(Some(ix), window, cx);
                                cx.emit(ListEvent::Confirm(ix));
                                cx.notify();
                            }),
                        )
                        .child(
                            h_flex()
                                .gap_1p5()
                                .when(show_chevron, |this| {
                                    this.child(
                                        div()
                                            .id(SharedString::from(format!("exp:{toggle_key}")))
                                            .size(px(16.))
                                            .flex_shrink_0()
                                            .rounded(px(4.))
                                            .cursor_pointer()
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .hover(|s| s.bg(theme.muted))
                                            .on_mouse_down(
                                                MouseButton::Left,
                                                cx.listener(move |this, _, _, cx| {
                                                    cx.stop_propagation();
                                                    this.delegate_mut().toggle(&toggle_key);
                                                    cx.notify();
                                                }),
                                            )
                                            .child(
                                                icon("icons/chevron-right.svg")
                                                    .with_size(px(13.))
                                                    .text_color(theme.muted_foreground)
                                                    .when(expanded, |ic| {
                                                        ic.rotate(gpui::Radians(
                                                            std::f32::consts::FRAC_PI_2,
                                                        ))
                                                    }),
                                            ),
                                    )
                                })
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .text_size(FONT_BODY)
                                        .font_medium()
                                        .text_color(theme.foreground)
                                        .truncate()
                                        .child(title),
                                )
                                .when(pinned, |this| {
                                    this.child(
                                        icon("icons/pin-filled.svg")
                                            .with_size(px(11.))
                                            .text_color(theme.primary),
                                    )
                                })
                                .when(favorite, |this| {
                                    this.child(
                                        icon("icons/star-filled.svg")
                                            .with_size(px(11.))
                                            .text_color(rgb(STAR_YELLOW)),
                                    )
                                }),
                        )
                        .child(
                            h_flex()
                                .gap_1p5()
                                .text_size(FONT_LABEL)
                                .text_color(theme.muted_foreground)
                                .child(img(agent_icon).size(px(15.)).flex_shrink_0())
                                .child(badge(project_name, theme.muted, theme.muted_foreground))
                                .when(show_chevron, |this| {
                                    this.child(
                                        div().flex_shrink_0().child(format!("{child_count}")),
                                    )
                                })
                                .child(div().flex_1())
                                .child(div().flex_shrink_0().child(relative_time(updated_at))),
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
}
