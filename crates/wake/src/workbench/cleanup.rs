//! Cleanup UI shares the existing sidebar, reader, controls and theme.
use super::*;
mod filter;
mod history;
mod rows;
use gpui_component::{checkbox::Checkbox, popover::Popover, Disableable as _};
use std::sync::atomic::AtomicBool;
use wake_core::cleanup::{
    self, CleanupBatch, CleanupCandidate, CleanupEntry, CleanupInventory, CleanupOptions,
    CleanupReview, CleanupSort, TargetStatus, TimeField, UnavailableSession,
};

pub(super) struct CleanupState {
    pub open: bool,
    options: CleanupOptions,
    inventory: CleanupInventory,
    shown: Rc<Vec<CleanupEntry>>,
    selected: HashSet<String>,
    focus: Option<String>,
    pub(super) preview_open: bool,
    pub(super) saved_detail: Option<DetailState>,
    filter_input: Option<Entity<InputState>>,
    filter_query: String,
    filter_scroll: ScrollHandle,
    now: i64,
    loading: bool,
    busy: bool,
    executing: bool,
    progress: usize,
    progress_total: usize,
    cancel: Arc<AtomicBool>,
    task: Option<Task<()>>,
    error: Option<String>,
    result: Option<CleanupBatch>,
    history: Vec<CleanupBatch>,
    history_open: bool,
    history_scroll: UniformListScrollHandle,
    result_scroll: ScrollHandle,
    expanded_results: HashSet<String>,
    refresh_after_history: bool,
    scroll: UniformListScrollHandle,
}
impl Default for CleanupState {
    fn default() -> Self {
        Self {
            open: false,
            options: CleanupOptions::default(),
            inventory: CleanupInventory::default(),
            shown: Rc::new(vec![]),
            selected: HashSet::new(),
            focus: None,
            preview_open: false,
            saved_detail: None,
            filter_input: None,
            filter_query: String::new(),
            filter_scroll: ScrollHandle::new(),
            now: 0,
            loading: false,
            busy: false,
            executing: false,
            progress: 0,
            progress_total: 0,
            cancel: Arc::new(AtomicBool::new(false)),
            task: None,
            error: None,
            result: None,
            history: vec![],
            history_open: false,
            history_scroll: UniformListScrollHandle::new(),
            result_scroll: ScrollHandle::new(),
            expanded_results: HashSet::new(),
            refresh_after_history: false,
            scroll: UniformListScrollHandle::new(),
        }
    }
}
const CLEANUP_DATE_WIDTH: Pixels = px(112.);
const CLEANUP_SIZE_WIDTH: Pixels = px(88.);

fn bytes(n: u64) -> String {
    if n == 0 {
        return "0 B".into();
    }
    if n >= 1024 * 1024 * 1024 {
        format!("{:.2} GB", n as f64 / 1073741824.)
    } else {
        crate::format::human_bytes(n as usize)
    }
}
fn sort_label(s: CleanupSort) -> &'static str {
    match s {
        CleanupSort::Created => t("Date created"),
        CleanupSort::Updated => t("Date updated"),
        CleanupSort::Size => t("File size"),
    }
}
fn time_label(ti: TimeField) -> &'static str {
    match ti {
        TimeField::Created => t("Date created"),
        TimeField::Updated => t("Date updated"),
    }
}
fn age_label(days: i64) -> &'static str {
    match days {
        30 => t("30 days ago"),
        90 => t("90 days ago"),
        180 => t("180 days ago"),
        365 => t("1 year ago"),
        _ => t("Any time"),
    }
}
fn manual_restore_pending(reason: &str) -> bool {
    reason.starts_with("Finder could not list Trash:")
        || reason.starts_with("Cannot access Trash:")
        || ((reason.contains("/.Trash/") || reason.contains("/.Trashes/"))
            && (reason.contains("Operation not permitted") || reason.contains("Permission denied")))
        || reason.starts_with("Could not restore this file from Trash: Operation not permitted")
        || reason.starts_with("Could not restore this file from Trash: Permission denied")
        || reason == "Restored files are still missing from their original locations."
        || reason.starts_with("Restore this file first:")
}

fn manual_restore_hint() -> &'static str {
    #[cfg(target_os = "macos")]
    return t("To restore, choose Put Back in Trash, then click Check.");
    #[cfg(not(target_os = "macos"))]
    t("Restore the files in your system trash, then click Check.")
}

fn unavailable_reason(reason: &str) -> &str {
    match reason {
        reason if manual_restore_pending(reason) => manual_restore_hint(),
        "No restored sessions found. Check the file locations below." => {
            t("No restored sessions found. Check the file locations below.")
        }
        "Files in Trash changed; restore them manually" => {
            t("Files in Trash changed; restore them manually")
        }
        "File identity is unavailable; restore files manually" => {
            t("File identity is unavailable; restore files manually")
        }
        "Files are no longer in Trash. They may have been permanently deleted." => {
            t("Files are no longer in Trash. They may have been permanently deleted.")
        }
        "Original folder is missing; recreate it before restoring" => {
            t("Original folder is missing; recreate it before restoring")
        }
        "Original folder contains a symbolic link" => t("Original folder contains a symbolic link"),
        "The original location contains different files. Nothing was overwritten." => {
            t("The original location contains different files. Nothing was overwritten.")
        }
        "A file that was not deleted is missing" => t("A file that was not deleted is missing"),
        "Restored files changed before verification" => {
            t("Restored files changed before verification")
        }
        "Restored files do not match the deleted files" => t(
            "Some restored files differ from the deleted files. Check the file locations below.",
        ),
        "No sessions could be restored. Review the errors below." => {
            t("No sessions could be restored. Review the errors below.")
        }
        "Session has unrecognized content" => t("Wake cannot read part of this session's format."),
        "Session or files changed since review; refresh and review again"
        | "Source changed while checking"
        | "Session tree changed" => t("This session changed. Refresh the list and try again."),
        "Session no longer exists" => t("This session is no longer available. Refresh the list."),
        "Remote sessions are read-only" | "Remote mirrors are read-only" => {
            t("Remote sessions are read-only")
        }
        "This source does not support independent file cleanup"
        | "No independently owned files" => {
            t("This source does not support independent file cleanup")
        }
        "Shared cleanup target" | "Target also contains another session" => {
            t("Files are shared with another session")
        }
        "Source file is missing" => t("Source file is missing"),
        "Source is not enabled" => t("Source is not enabled"),
        "Network locations cannot be moved to the Recycle Bin" => {
            t("Network locations cannot be moved to the Recycle Bin")
        }
        "This location does not support safe recycling" => {
            t("This location does not support safe recycling")
        }
        "Parent session is unavailable" | "Session tree contains a cycle" => {
            t("The complete session tree could not be verified")
        }
        "Symbolic links are not supported"
        | "Shared hard links are not supported"
        | "Source path contains links or is not absolute"
        | "Source is outside the enabled location"
        | "Cleanup target is outside the source location" => {
            t("File ownership could not be verified")
        }
        _ => reason,
    }
}

fn cleanup_review_issues(items: &[UnavailableSession], cx: &App) -> AnyElement {
    v_flex()
        .gap(SPACE_MD)
        .children(items.iter().map(|item| {
            v_flex()
                .gap(SPACE_XS)
                .child(
                    div()
                        .text_size(FONT_BODY)
                        .font_medium()
                        .child(item.session.title.clone()),
                )
                .child(
                    div()
                        .text_size(FONT_LABEL)
                        .text_color(cx.theme().muted_foreground)
                        .child(format!(
                            "{} · {} · {}",
                            item.session.agent.display_name(),
                            item.session.project_name,
                            abs_date(item.session.updated_at)
                        )),
                )
                .child(
                    div()
                        .text_size(FONT_CAPTION)
                        .text_color(cx.theme().muted_foreground)
                        .child(unavailable_reason(&item.reason).to_string()),
                )
        }))
        .into_any_element()
}

fn cleanup_path(path: String, id: impl Into<ElementId>, cx: &App) -> AnyElement {
    let name = std::path::Path::new(&path)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.clone());
    div()
        .id(id)
        .min_w_0()
        .truncate()
        .text_size(FONT_CAPTION)
        .font_family(cx.theme().mono_font_family.clone())
        .text_color(cx.theme().muted_foreground)
        .child(name)
        .tooltip(move |window, cx| {
            gpui_component::tooltip::Tooltip::new(path.clone()).build(window, cx)
        })
        .into_any_element()
}

// File details belong to the final review, with session identity visible above them.
fn cleanup_review_item(
    item: &CleanupCandidate,
    expanded: &Rc<std::cell::RefCell<HashSet<String>>>,
    cx: &App,
) -> AnyElement {
    let key = item.root.key.clone();
    let open = expanded.borrow().contains(&key);
    let expanded = expanded.clone();
    let files: Vec<_> = item
        .targets
        .iter()
        .flat_map(|target| &target.files)
        .filter(|file| !file.directory)
        .collect();
    let theme = cx.theme();
    v_flex()
        .min_w_0()
        .py(SPACE_MD)
        .gap(SPACE_SM)
        .child(
            h_flex()
                .gap(SPACE_SM)
                .min_w_0()
                .child(
                    img(item.root.agent.brand_icon(theme.mode.is_dark()))
                        .size(px(16.))
                        .flex_shrink_0(),
                )
                .child(div().flex_1().min_w_0().child(cleanup_title(
                    item,
                    SharedString::from(format!("review-title-{key}")),
                    cx,
                )))
                .child(
                    div()
                        .text_size(FONT_CAPTION)
                        .text_color(theme.muted_foreground)
                        .flex_shrink_0()
                        .child(bytes(item.bytes)),
                ),
        )
        .child(
            h_flex()
                .justify_between()
                .gap(SPACE_SM)
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_size(FONT_LABEL)
                        .text_color(theme.muted_foreground)
                        .child(format!(
                            "{} · {}",
                            item.root.project_name,
                            session_tally(item.sessions.len() as i64)
                        )),
                )
                .child(
                    Button::new(SharedString::from(format!("review-files-{key}")))
                        .ghost()
                        .h(px(28.))
                        .px(SPACE_SM)
                        .text_size(FONT_CAPTION)
                        .rounded(RADIUS_BUTTON)
                        .icon(
                            icon(if open {
                                "icons/chevron-down.svg"
                            } else {
                                "icons/chevron-right.svg"
                            })
                            .with_size(px(12.)),
                        )
                        .label(crate::tf!("Source files ({})", files.len()))
                        .on_click(move |_, window, _| {
                            let mut expanded = expanded.borrow_mut();
                            if !expanded.remove(&key) {
                                expanded.insert(key.clone());
                            }
                            window.refresh();
                        }),
                ),
        )
        .when(open, |row| {
            row.child(
                v_flex()
                    .pl(SPACE_MD)
                    .gap(SPACE_SM)
                    .border_l_1()
                    .border_color(theme.border)
                    .children(files.iter().enumerate().map(|(ix, file)| {
                        let path = file.path.to_string_lossy().into_owned();
                        let folder = file
                            .path
                            .parent()
                            .map(|p| tilde_path(&p.to_string_lossy()))
                            .unwrap_or_default();
                        h_flex()
                            .min_w_0()
                            .gap(SPACE_MD)
                            .items_center()
                            .child(
                                v_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .gap(px(2.))
                                    .child(cleanup_path(
                                        path,
                                        SharedString::from(format!(
                                            "review-file-{}-{ix}",
                                            item.root.key
                                        )),
                                        cx,
                                    ))
                                    .child(
                                        div()
                                            .truncate()
                                            .text_size(FONT_LABEL)
                                            .text_color(theme.muted_foreground)
                                            .child(folder),
                                    ),
                            )
                            .child(
                                div()
                                    .flex_shrink_0()
                                    .text_size(FONT_LABEL)
                                    .text_color(theme.muted_foreground)
                                    .child(bytes(file.bytes)),
                            )
                    })),
            )
        })
        .into_any_element()
}

fn cleanup_facts(item: &CleanupCandidate) -> String {
    crate::tf!(
        "{} · {} · approx. {}",
        session_tally(item.sessions.len() as i64),
        crate::tp!("{} prompt", "{} prompts", item.prompts),
        bytes(item.bytes)
    )
}

fn cleanup_title(item: &CleanupCandidate, id: impl Into<ElementId>, cx: &App) -> AnyElement {
    cleanup_session_title(&item.root, item.has_starred(), item.has_pinned(), id, cx)
}

fn cleanup_session_title(
    meta: &SessionMeta,
    starred: bool,
    pinned: bool,
    id: impl Into<ElementId>,
    cx: &App,
) -> AnyElement {
    let mut tooltip = meta.title.clone();
    if starred {
        tooltip.push_str(&format!("\n{}", t("Includes a starred session")));
    }
    if pinned {
        tooltip.push_str(&format!("\n{}", t("Includes a pinned session")));
    }
    h_flex()
        .min_w_0()
        .items_center()
        .gap(SPACE_XS)
        .child(
            div()
                .id(id)
                .flex_1()
                .min_w_0()
                .truncate()
                .text_size(FONT_BODY)
                .font_medium()
                .child(meta.title.clone())
                .tooltip(move |window, cx| {
                    gpui_component::tooltip::Tooltip::new(tooltip.clone()).build(window, cx)
                }),
        )
        .when(pinned, |v| {
            v.child(
                icon("icons/pin-filled.svg")
                    .with_size(px(11.))
                    .text_color(cx.theme().primary),
            )
        })
        .when(starred, |v| {
            v.child(
                icon("icons/star-filled.svg")
                    .with_size(px(11.))
                    .text_color(rgb(crate::theme::STAR_YELLOW)),
            )
        })
        .into_any_element()
}

impl Workbench {
    pub(super) fn cleanup_preview(&self) -> Option<&CleanupCandidate> {
        if !self.cleanup.open {
            return None;
        }
        let key = &self.detail.as_ref()?.meta.key;
        self.cleanup
            .shown
            .iter()
            .filter_map(CleanupEntry::candidate)
            .find(|c| c.root.key == *key)
    }
    pub(super) fn render_cleanup_detail_facts(&self, cx: &Context<Self>) -> AnyElement {
        let Some(c) = self.cleanup_preview() else {
            return div().into_any_element();
        };
        h_flex()
            .w_full()
            .gap(SPACE_SM)
            .flex_wrap()
            .items_center()
            .text_size(FONT_LABEL)
            .text_color(cx.theme().muted_foreground)
            .child(cleanup_facts(c))
            .into_any_element()
    }
    pub(super) fn load_cleanup_prefs(&mut self) {
        if let Some(options) = self
            .store
            .pref_get(cleanup::PREFS)
            .and_then(|s| serde_json::from_str::<CleanupOptions>(&s).ok())
        {
            self.cleanup.options = options;
        }
    }
    pub(super) fn leave_cleanup(&mut self, cx: &mut Context<Self>) {
        if !self.cleanup.open {
            return;
        }
        self.cleanup.open = false;
        self.cleanup.preview_open = false;
        if self.cleanup.busy && !self.cleanup.executing {
            self.cleanup.cancel.store(true, Ordering::Relaxed);
        }
        self.detail = self.cleanup.saved_detail.take();
        cx.notify();
    }
    pub(super) fn toggle_cleanup(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.cleanup.open {
            self.leave_cleanup(cx);
            return;
        }
        self.insights_open = false;
        self.cleanup.saved_detail = self.detail.take();
        self.cleanup.open = true;
        self.cleanup.history_open = false;
        self.cleanup.result = None;
        if !self.cleanup.busy {
            self.reload_cleanup(window, cx);
        }
        cx.notify();
    }
    fn reload_cleanup(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.cleanup.busy {
            return;
        }
        self.cleanup.loading = true;
        self.cleanup.selected.clear();
        self.cleanup.error = None;
        self.cleanup.now = chrono::Utc::now().timestamp_millis();
        let store = self.store.clone();
        let adapters = self.adapters.clone();
        let task = cx.background_spawn(async move {
            Ok::<_, anyhow::Error>((
                cleanup::inventory(&store, &adapters)?,
                cleanup::history(&store)?,
            ))
        });
        self.cleanup.task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            this.update_in(cx, |this, window, cx| {
                this.cleanup.loading = false;
                match result {
                    Ok((inventory, history)) => {
                        this.cleanup.inventory = inventory;
                        this.cleanup.history = history;
                        this.apply_cleanup_options(false, window, cx);
                    }
                    Err(e) => this.cleanup.error = Some(e.to_string()),
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }
    fn apply_cleanup_options(&mut self, clear: bool, _window: &mut Window, cx: &mut Context<Self>) {
        if clear {
            self.cleanup.selected.clear();
            self.cleanup.scroll = UniformListScrollHandle::new();
        }
        let mut items: Vec<_> = self
            .cleanup
            .inventory
            .candidates
            .iter()
            .cloned()
            .map(CleanupEntry::Available)
            .chain(
                self.cleanup
                    .inventory
                    .unavailable
                    .iter()
                    .cloned()
                    .map(CleanupEntry::Unavailable),
            )
            .filter(|item| self.cleanup.options.matches_entry(item, self.cleanup.now))
            .collect();
        self.cleanup.options.sort_entries(&mut items);
        self.cleanup.selected.retain(|key| {
            items
                .iter()
                .filter_map(CleanupEntry::candidate)
                .any(|c| c.root.key == *key)
        });
        self.cleanup.focus = self
            .cleanup
            .focus
            .take()
            .filter(|key| items.iter().any(|item| item.session().key == *key));
        self.cleanup.shown = Rc::new(items);
        if self.cleanup.open && self.cleanup.focus.is_none() {
            self.cleanup.preview_open = false;
            self.detail = None;
        }
        if let Ok(s) = serde_json::to_string(&self.cleanup.options) {
            let _ = self.store.pref_set(cleanup::PREFS, &s);
        }
        cx.notify();
    }
    fn cleanup_chosen(&self) -> Vec<CleanupCandidate> {
        self.cleanup
            .shown
            .iter()
            .filter_map(CleanupEntry::candidate)
            .filter(|c| self.cleanup.selected.contains(&c.root.key))
            .cloned()
            .collect()
    }

    pub(super) fn refresh_cleanup_flags(
        &mut self,
        key: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.cleanup.open {
            return;
        }
        let Ok(Some(meta)) = self.store.get_session(key) else {
            return;
        };
        for candidate in &mut self.cleanup.inventory.candidates {
            if let Some(member) = candidate.sessions.iter_mut().find(|s| s.key == key) {
                member.favorite = meta.favorite;
                member.pinned = meta.pinned;
                if candidate.root.key == key {
                    candidate.root.favorite = meta.favorite;
                    candidate.root.pinned = meta.pinned;
                }
                self.cleanup.selected.remove(&candidate.root.key);
            }
        }
        for item in &mut self.cleanup.inventory.unavailable {
            if let Some(member) = item.sessions.iter_mut().find(|s| s.key == key) {
                member.favorite = meta.favorite;
                member.pinned = meta.pinned;
                if item.session.key == key {
                    item.session.favorite = meta.favorite;
                    item.session.pinned = meta.pinned;
                }
            }
        }
        self.apply_cleanup_options(false, window, cx);
    }

    fn review_cleanup(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let chosen = self.cleanup_chosen();
        if chosen.is_empty() || self.cleanup.busy || self.cleanup.loading {
            return;
        }
        self.cleanup.busy = true;
        self.cleanup.progress = 0;
        self.cleanup.progress_total = chosen.len();
        self.cleanup.cancel = Arc::new(AtomicBool::new(false));
        self.cleanup.error = None;
        let cancel = self.cleanup.cancel.clone();
        let store = self.store.clone();
        let adapters = self.adapters.clone();
        let (tx, mut rx) = futures::channel::mpsc::unbounded();
        let task = cx.background_spawn(async move {
            let review = cleanup::review(&store, &adapters, chosen, &cancel, |n| {
                let _ = tx.unbounded_send(Ok(n));
            });
            let _ = tx.unbounded_send(Err(review));
        });
        cx.spawn_in(window, async move |this, cx| {
            while let Some(event) = rx.next().await {
                if this
                    .update_in(cx, |this, window, cx| {
                        match event {
                            Ok(n) => this.cleanup.progress = n,
                            Err(review) => {
                                this.cleanup.busy = false;
                                // A cancel can arrive after the worker has finished but before
                                // this UI update; never open a late confirmation in that case.
                                if this.cleanup.open && !this.cleanup.cancel.load(Ordering::Relaxed)
                                {
                                    if let Some(review) = review {
                                        this.confirm_cleanup(review, window, cx);
                                    }
                                }
                            }
                        }
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
            task.await;
        })
        .detach();
        cx.notify();
    }
    fn confirm_cleanup(&self, review: CleanupReview, window: &mut Window, cx: &mut Context<Self>) {
        if review.ready.is_empty() {
            let entity = cx.entity();
            open_closable_dialog(window, cx, move |dialog, window, cx| {
                let entity = entity.clone();
                dialog.title(t("Unable to clean up these sessions")).width(px(640.))
                    .child(v_flex().gap(SPACE_MD)
                        .child(div().text_size(FONT_CAPTION).text_color(cx.theme().muted_foreground)
                            .child(t("The selected sessions could not be verified. No files were moved.")))
                        .child(v_flex().id("cleanup-blocked-list")
                            .max_h((window.viewport_size().height - px(380.)).min(px(360.)))
                            .overflow_y_scroll()
                            .child(cleanup_review_issues(&review.skipped, cx)))
                        .child(h_flex().justify_end().child(
                            Button::new("cleanup-review-refresh").map(action_button)
                                .label(t("Refresh"))
                                .on_click(move |_, window, cx| {
                                    window.close_dialog(cx);
                                    entity.update(cx, |this, cx| this.reload_cleanup(window, cx));
                                })
                        )))
            });
            return;
        }
        let chosen = Rc::new(review.ready);
        let skipped = Rc::new(review.skipped);
        let expanded = Rc::new(std::cell::RefCell::new(HashSet::new()));
        let entity = cx.entity();
        window.open_alert_dialog(cx, move |dialog, window, cx| {
            let items = chosen.clone();
            let entity = entity.clone();
            let theme = cx.theme();
            let count: usize = items.iter().map(|c| c.sessions.len()).sum();
            let title = if count == 1 {
                t("Delete this session?").to_string()
            } else {
                crate::tf!("Delete {} sessions?", count)
            };
            dialog
                .title(div().text_size(FONT_HEADING).font_semibold().child(title))
                .width(px(640.))
                .confirm()
                .button_props(
                    gpui_component::dialog::DialogButtonProps::default()
                        .show_cancel(true)
                        .cancel_text(t("Cancel"))
                        .ok_text(t("Confirm"))
                        .ok_variant(gpui_component::button::ButtonVariant::Danger),
                )
                .footer(
                    v_flex()
                        .w_full()
                        .flex_shrink_0()
                        .border_t_1()
                        .border_color(theme.border)
                        .pt(SPACE_MD)
                        .gap(SPACE_LG)
                        .child(
                            v_flex()
                                .gap(SPACE_XS)
                                .text_size(FONT_CAPTION)
                                .text_color(theme.muted_foreground)
                                .child(cleanup_trash_hint())
                                .child(t("Agents may no longer resume these sessions.")),
                        )
                        .child(
                            gpui_component::dialog::DialogFooter::new()
                                .gap(SPACE_SM)
                                .child(
                                    crate::settings::settings_button(
                                        Button::new("cleanup-confirm-cancel"),
                                        cx,
                                    )
                                    .map(action_button)
                                    .label(t("Cancel"))
                                    .on_click(
                                        |_, window, cx| {
                                            window.dispatch_action(
                                                Box::new(gpui_component::dialog::Cancel),
                                                cx,
                                            )
                                        },
                                    ),
                                )
                                .child(
                                    Button::new("cleanup-confirm-trash")
                                        .danger()
                                        .map(action_button)
                                        .label(t("Confirm"))
                                        .on_click(|_, window, cx| {
                                            window.dispatch_action(
                                                Box::new(gpui_component::dialog::Confirm {
                                                    secondary: false,
                                                }),
                                                cx,
                                            )
                                        }),
                                ),
                        ),
                )
                .child(
                    v_flex()
                        .gap(SPACE_MD)
                        .text_size(FONT_BODY)
                        .child(
                            div()
                                .text_size(FONT_CAPTION)
                                .text_color(theme.muted_foreground)
                                .child(crate::tf!(
                                    "Local files · About {}",
                                    bytes(items.iter().map(|c| c.bytes).sum())
                                )),
                        )
                        .when(!skipped.is_empty(), |v| {
                            v.child(
                                div()
                                    .text_size(FONT_CAPTION)
                                    .child(crate::tf!("{} items will be skipped.", skipped.len())),
                            )
                        })
                        .child(
                            v_flex()
                                .id("cleanup-review-files")
                                .max_h((window.viewport_size().height - px(420.)).min(px(300.)))
                                .overflow_y_scroll()
                                .when(!skipped.is_empty(), |v| {
                                    v.child(div().font_medium().child(t("To delete")))
                                })
                                .children(items.iter().enumerate().map(|(ix, item)| {
                                    div()
                                        .when(ix > 0, |row| {
                                            row.border_t_1().border_color(theme.border)
                                        })
                                        .child(cleanup_review_item(item, &expanded, cx))
                                }))
                                .when(!skipped.is_empty(), |v| {
                                    v.child(
                                        div()
                                            .font_medium()
                                            .pt(SPACE_SM)
                                            .child(t("Will be skipped")),
                                    )
                                    .child(cleanup_review_issues(&skipped, cx))
                                }),
                        ),
                )
                .on_ok(move |_, window, cx| {
                    entity.update(cx, |this, cx| {
                        this.execute_cleanup(items.as_ref().clone(), window, cx)
                    });
                    true
                })
        });
    }
    fn execute_cleanup(
        &mut self,
        items: Vec<CleanupCandidate>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.cleanup.busy {
            return;
        }
        self.cleanup.busy = true;
        self.cleanup.executing = true;
        self.cleanup.progress = 0;
        self.cleanup.progress_total = items.len();
        self.cleanup.result = None;
        self.cleanup.history_open = false;
        self.cleanup.expanded_results.clear();
        self.cleanup.result_scroll = ScrollHandle::new();
        self.cleanup.cancel = Arc::new(AtomicBool::new(false));
        let cancel = self.cleanup.cancel.clone();
        let store = self.store.clone();
        let adapters = self.adapters.clone();
        let (tx, mut rx) = futures::channel::mpsc::unbounded();
        std::thread::spawn(move || {
            let mut batch = CleanupBatch::new(items, chrono::Utc::now().timestamp_millis());
            let result = cleanup::execute(&store, &adapters, &mut batch, &cancel, |n| {
                let _ = tx.unbounded_send(Ok(n));
            });
            let _ = tx.unbounded_send(Err((batch, result.err().map(|e| e.to_string()))));
        });
        cx.spawn(async move |this, cx| {
            while let Some(event) = rx.next().await {
                if this
                    .update(cx, |this, cx| {
                        match event {
                            Ok(n) => this.cleanup.progress = n,
                            Err((batch, error)) => {
                                let deleted: HashSet<_> = batch
                                    .records
                                    .iter()
                                    .filter(|r| r.indexed)
                                    .flat_map(|r| {
                                        r.candidate.sessions.iter().map(|s| s.key.clone())
                                    })
                                    .collect();
                                this.cleanup
                                    .inventory
                                    .candidates
                                    .retain(|c| !deleted.contains(&c.root.key));
                                this.cleanup.shown = Rc::new(
                                    this.cleanup
                                        .shown
                                        .iter()
                                        .filter(|item| !deleted.contains(&item.session().key))
                                        .cloned()
                                        .collect(),
                                );
                                if this
                                    .detail
                                    .as_ref()
                                    .is_some_and(|d| deleted.contains(&d.meta.key))
                                {
                                    this.detail = None;
                                }
                                if this
                                    .cleanup
                                    .saved_detail
                                    .as_ref()
                                    .is_some_and(|d| deleted.contains(&d.meta.key))
                                {
                                    this.cleanup.saved_detail = None;
                                }
                                this.cleanup.busy = false;
                                this.cleanup.executing = false;
                                this.cleanup.error = error;
                                this.cleanup.selected.clear();
                                this.cleanup.history.insert(0, batch.clone());
                                this.cleanup.result = Some(batch);
                                this.cleanup.refresh_after_history = true;
                                this.refresh(cx);
                            }
                        }
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        cx.notify();
    }
    fn check_cleanup_batch(
        &mut self,
        verify_restored_files: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.cleanup.busy {
            return;
        }
        let Some(mut batch) = self.cleanup.result.clone() else {
            return;
        };
        self.cleanup.busy = true;
        self.cleanup.error = None;
        let store = self.store.clone();
        let adapters = self.adapters.clone();
        let task = cx.background_spawn(async move {
            let result = if verify_restored_files {
                cleanup::restore(&store, &adapters, &mut batch)
            } else {
                cleanup::retry_index_updates(&store, &mut batch)
            };
            (batch, result)
        });
        cx.spawn_in(window, async move |this, cx| {
            let (batch, result) = task.await;
            this.update_in(cx, |this, window, cx| {
                this.cleanup.busy = false;
                this.cleanup.result = Some(batch.clone());
                this.cleanup.refresh_after_history = true;
                if let Some(old) = this.cleanup.history.iter_mut().find(|b| b.id == batch.id) {
                    *old = batch.clone();
                }
                match result {
                    Ok(_) => {
                        this.cleanup.error = None;
                        let failed = batch.records.iter().any(|r| {
                            !r.restored
                                && r.error
                                    .as_deref()
                                    .is_some_and(|error| !manual_restore_pending(error))
                        });
                        let notification = if failed {
                            Notification::warning(t(
                                "Some sessions could not be updated. Review the errors below.",
                            ))
                        } else if verify_restored_files
                            && batch.records.iter().any(|r| r.can_restore())
                        {
                            Notification::info(t("Some sessions have been restored to Wake."))
                        } else if verify_restored_files {
                            Notification::success(t("Sessions restored"))
                        } else {
                            Notification::success(t("Session list updated"))
                        };
                        window.push_notification(notification, cx);
                        if verify_restored_files {
                            this.kick_incremental_scan(cx);
                        } else {
                            this.refresh(cx);
                        }
                    }
                    Err(_)
                        if verify_restored_files
                            && batch.records.iter().any(|record| record.can_restore())
                            && batch
                                .records
                                .iter()
                                .filter(|record| record.can_restore())
                                .all(|record| {
                                    record.error.as_deref().is_some_and(manual_restore_pending)
                                }) =>
                    {
                        this.cleanup.error = None;
                        window.push_notification(Notification::info(manual_restore_hint()), cx);
                    }
                    Err(e) => this.cleanup.error = Some(e.to_string()),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    pub(super) fn render_cleanup(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        if self.cleanup.preview_open {
            return self.render_detail(window, cx);
        }
        if self.cleanup.result.is_some() || self.cleanup.history_open {
            return self.render_cleanup_history_page(window, cx);
        }
        let theme = cx.theme().clone();
        let state = &self.cleanup;
        let row_count = state.shown.len();
        let available_count = state
            .shown
            .iter()
            .filter_map(CleanupEntry::candidate)
            .count();
        let unavailable_count = row_count - available_count;
        let filter = self.cleanup_filter(cx);
        let current = state.options.sort;
        let ascending = state.options.sort_ascending();
        let e = cx.entity();
        let sort = Button::new("cleanup-sort")
            .ghost()
            .disabled(state.busy)
            .rounded(RADIUS_BUTTON)
            .icon(icon("icons/arrow-up-down.svg").with_size(px(16.)))
            .tooltip(crate::tf!(
                "Sort by {} · {}",
                sort_label(current),
                if ascending {
                    t("Ascending")
                } else {
                    t("Descending")
                }
            ))
            .dropdown_menu(move |mut menu, _, _| {
                for value in [
                    CleanupSort::Updated,
                    CleanupSort::Created,
                    CleanupSort::Size,
                ] {
                    let e = e.clone();
                    menu = menu.item(
                        PopupMenuItem::new(sort_label(value))
                            .checked(current == value)
                            .on_click(move |_, window, cx| {
                                e.update(cx, |this, cx| {
                                    this.cleanup.options.sort = value;
                                    this.cleanup.options.ascending = Some(ascending);
                                    this.apply_cleanup_options(false, window, cx);
                                })
                            }),
                    );
                }
                menu = menu.separator();
                for (label, value) in [(t("Descending"), false), (t("Ascending"), true)] {
                    let e = e.clone();
                    menu = menu.item(
                        PopupMenuItem::new(label)
                            .checked(ascending == value)
                            .on_click(move |_, window, cx| {
                                e.update(cx, |this, cx| {
                                    this.cleanup.options.ascending = Some(value);
                                    this.apply_cleanup_options(false, window, cx);
                                });
                            }),
                    );
                }
                menu.min_w(px(180.))
            })
            .anchor(Anchor::TopRight);
        let chosen = self.cleanup_chosen();
        let selected_bytes: u64 = chosen.iter().map(|c| c.bytes).sum();
        let selected_sessions: usize = chosen.iter().map(|c| c.sessions.len()).sum();
        let total_bytes: u64 = state
            .shown
            .iter()
            .filter_map(CleanupEntry::candidate)
            .map(|c| c.bytes)
            .sum();
        let date_field = match state.options.sort {
            CleanupSort::Created => TimeField::Created,
            CleanupSort::Updated => TimeField::Updated,
            CleanupSort::Size
                if state.options.created_days > 0 && state.options.updated_days == 0 =>
            {
                TimeField::Created
            }
            CleanupSort::Size => TimeField::Updated,
        };
        let mut dates = vec![];
        for (field, days) in [
            (TimeField::Created, state.options.created_days),
            (TimeField::Updated, state.options.updated_days),
        ] {
            if days > 0 {
                dates.push(format!("{} · {}", time_label(field), age_label(days)));
            }
        }
        let mut summary = if dates.is_empty() {
            t("Any time").to_string()
        } else {
            dates.join(" / ")
        };
        if state.options.agents.len() == 1 {
            if let Some(agent) = state.options.agents.first() {
                summary.push_str(&format!(" · {}", agent.display_name()));
            }
        } else if !state.options.agents.is_empty() {
            summary.push_str(&format!(
                " · {} ({})",
                t("Agents"),
                state.options.agents.len()
            ));
        }
        if state.options.only_cleanable {
            summary.push_str(&format!(" · {}", t("Exclude unavailable")));
        }
        if state.options.exclude_starred {
            summary.push_str(&format!(" · {}", t("Exclude starred")));
        }
        if state.options.exclude_pinned {
            summary.push_str(&format!(" · {}", t("Exclude pinned")));
        }
        if state.options.projects.len() == 1 {
            let project = state.options.projects.first().unwrap();
            summary.push_str(&format!(
                " · {}",
                self.projects
                    .iter()
                    .find(|p| &p.path == project)
                    .map(|p| p.name.as_str())
                    .unwrap_or(project)
            ));
        } else if !state.options.projects.is_empty() {
            summary.push_str(&format!(
                " · {} ({})",
                t("Projects"),
                state.options.projects.len()
            ));
        }
        let history = Button::new("cleanup-history")
            .map(action_button)
            .ghost()
            .label(t("History"))
            .tooltip(t("Cleanup history"))
            .disabled(state.busy)
            .on_click(cx.listener(|this, _, window, cx| {
                this.cleanup.history_open = true;
                this.focus_handle.focus(window, cx);
                cx.notify();
            }));
        let heading = library_header(
            "cleanup-header",
            t("Clean Up Sessions"),
            summary,
            SPACE_XXL,
            Some(
                h_flex()
                    .gap(SPACE_XS)
                    .items_center()
                    .child(filter)
                    .child(sort)
                    .child(
                        Button::new("cleanup-refresh")
                            .ghost()
                            .rounded(RADIUS_BUTTON)
                            .icon(icon("icons/refresh-cw.svg").with_size(px(16.)))
                            .tooltip(t("Refresh"))
                            .disabled(state.loading || state.busy)
                            .on_click(
                                cx.listener(|this, _, window, cx| this.reload_cleanup(window, cx)),
                            ),
                    )
                    .into_any_element(),
            ),
            cx,
        );
        let list =
            v_flex()
                .flex_1()
                .min_h_0()
                .child(
                    h_flex()
                        .h(px(44.))
                        .flex_shrink_0()
                        .px(SPACE_XXL)
                        .gap(SPACE_MD)
                        .items_center()
                        .child(
                            h_flex()
                                .flex_1()
                                .min_w_0()
                                .gap(SPACE_MD)
                                .items_center()
                                .child(
                                    Checkbox::new("cleanup-all")
                                        .label(if state.loading {
                                            t("Select all").to_string()
                                        } else {
                                            crate::tf!("Select all ({})", available_count)
                                        })
                                        .flex_shrink_0()
                                        .text_size(FONT_CAPTION)
                                        .tooltip(crate::tf!(
                                            "Select all {} cleanable sessions",
                                            available_count
                                        ))
                                        .checked(
                                            available_count > 0
                                                && state.selected.len() == available_count,
                                        )
                                        .disabled(
                                            state.loading || state.busy || available_count == 0,
                                        )
                                        .on_click(cx.listener(|this, checked, _, cx| {
                                            this.cleanup.selected = if *checked {
                                                this.cleanup
                                                    .shown
                                                    .iter()
                                                    .filter_map(CleanupEntry::candidate)
                                                    .map(|c| c.root.key.clone())
                                                    .collect()
                                            } else {
                                                HashSet::new()
                                            };
                                            cx.notify();
                                        })),
                                )
                                .child(
                                    div()
                                        .min_w_0()
                                        .truncate()
                                        .text_size(FONT_CAPTION)
                                        .text_color(theme.muted_foreground)
                                        .child(if state.loading {
                                            "—".to_string()
                                        } else if unavailable_count > 0 {
                                            format!(
                                                "{} · {}",
                                                session_tally(row_count as i64),
                                                crate::tf!("{} unavailable", unavailable_count)
                                            )
                                        } else {
                                            session_tally(row_count as i64)
                                        }),
                                ),
                        )
                        .child(
                            div()
                                .w(CLEANUP_DATE_WIDTH)
                                .flex_shrink_0()
                                .text_right()
                                .text_size(FONT_LABEL)
                                .text_color(theme.muted_foreground)
                                .child(time_label(date_field)),
                        )
                        .child(
                            div()
                                .w(CLEANUP_SIZE_WIDTH)
                                .flex_shrink_0()
                                .text_right()
                                .text_size(FONT_LABEL)
                                .text_color(theme.muted_foreground)
                                .child(t("File size")),
                        ),
                )
                .when(state.loading, |v| {
                    v.child(
                        v_flex()
                            .flex_1()
                            .items_center()
                            .justify_center()
                            .gap(SPACE_MD)
                            .child(Spinner::new())
                            .child(
                                div()
                                    .text_size(FONT_CAPTION)
                                    .text_color(theme.muted_foreground)
                                    .child(t("Checking local files…")),
                            ),
                    )
                })
                .when(!state.loading && row_count == 0, |v| {
                    v.child(v_flex().flex_1().items_center().justify_center().child(
                        empty_state_card(
                            "icons/inbox.svg",
                            px(58.),
                            px(24.),
                            t("No sessions match these filters"),
                            t("Try a different agent, project, or filter."),
                            cx,
                        ),
                    ))
                })
                .when(!state.loading && row_count > 0, |v| {
                    v.child(
                        uniform_list(
                            "cleanup-candidates",
                            row_count,
                            cx.processor(move |this, range: Range<usize>, _, cx| {
                                range
                                    .map(|ix| this.render_cleanup_row(ix, date_field, cx))
                                    .collect()
                            }),
                        )
                        .track_scroll(&state.scroll)
                        .flex_1()
                        .min_h_0(),
                    )
                });
        let footer = h_flex()
            .h(SIDEBAR_FOOTER_ROW_HEIGHT + px(1.))
            .flex_shrink_0()
            .px(SPACE_XXL)
            .gap(SPACE_MD)
            .items_center()
            .border_t_1()
            .border_color(theme.border)
            .when(state.busy, |v| v.child(Spinner::new().small()))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(FONT_LABEL)
                    .text_color(if chosen.is_empty() {
                        theme.muted_foreground
                    } else {
                        theme.foreground
                    })
                    .child(if state.busy && state.cancel.load(Ordering::Relaxed) {
                        t("Stopping after the current session…").to_string()
                    } else if state.executing {
                        crate::tf!(
                            "Moving files… {} / {}",
                            state.progress,
                            state.progress_total
                        )
                    } else if state.busy {
                        crate::tf!(
                            "Checking sessions… {} / {}",
                            state.progress,
                            state.progress_total
                        )
                    } else if chosen.is_empty() && available_count == 0 && row_count > 0 {
                        t("No cleanable sessions in these results").to_string()
                    } else if chosen.is_empty() && available_count > 0 && !state.loading {
                        crate::tf!("{} available for cleanup", bytes(total_bytes))
                    } else if chosen.is_empty() {
                        t("Select sessions to clean up").to_string()
                    } else if chosen.len() == selected_sessions {
                        crate::tf!("{} selected · {}", selected_sessions, bytes(selected_bytes))
                    } else {
                        crate::tf!(
                            "{} selected · {} sessions · {}",
                            chosen.len(),
                            selected_sessions,
                            bytes(selected_bytes)
                        )
                    }),
            )
            .when(chosen.is_empty() && !state.busy, |v| v.child(history))
            .when(state.busy, |v| {
                v.child(
                    Button::new("cleanup-cancel")
                        .map(action_button)
                        .label(if state.executing {
                            t("Stop after current session")
                        } else {
                            t("Cancel")
                        })
                        .disabled(state.cancel.load(Ordering::Relaxed))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.cleanup.cancel.store(true, Ordering::Relaxed);
                            cx.notify();
                        })),
                )
            })
            .when(!chosen.is_empty() && !state.busy, |v| {
                v.child(
                    Button::new("cleanup-clear-selection")
                        .map(action_button)
                        .ghost()
                        .label(t("Clear"))
                        .tooltip(t("Clear selection"))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.cleanup.selected.clear();
                            cx.notify();
                        })),
                )
                .child(
                    Button::new("cleanup-review")
                        .map(action_button)
                        .danger()
                        .label(t("Delete"))
                        .tooltip(move_to_trash())
                        .disabled(state.loading)
                        .on_click(
                            cx.listener(|this, _, window, cx| this.review_cleanup(window, cx)),
                        ),
                )
            });
        let body = v_flex()
            .flex_1()
            .min_h_0()
            .child(heading)
            .child(list)
            .child(footer);
        v_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .bg(theme.background)
            .child(body)
            .when_some(state.error.as_ref(), |v, error| {
                v.child(
                    div()
                        .px(SPACE_XXL)
                        .py(SPACE_SM)
                        .text_size(FONT_CAPTION)
                        .text_color(theme.danger)
                        .child(error.clone()),
                )
            })
            .into_any_element()
    }
    fn show_cleanup_preview(&mut self, key: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.cleanup.busy {
            return;
        }
        self.open_detail(key, None, window, cx);
        if self
            .detail
            .as_ref()
            .is_some_and(|detail| detail.meta.key == key)
        {
            self.cleanup.focus = Some(key.to_owned());
            self.cleanup.preview_open = true;
            self.focus_handle.focus(window, cx);
            cx.notify();
        }
    }
    pub(super) fn close_cleanup_preview(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.cleanup.preview_open = false;
        self.stop_detail_selection_auto_scroll();
        self.focus_handle.focus(window, cx);
        cx.notify();
    }
    pub(super) fn navigate_cleanup_back(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.cleanup.busy {
            return false;
        }
        if self.cleanup.preview_open {
            self.close_cleanup_preview(window, cx);
        } else if self.cleanup.result.take().is_some() {
            self.cleanup.expanded_results.clear();
            self.cleanup.error = None;
            self.focus_handle.focus(window, cx);
            cx.notify();
        } else if self.cleanup.history_open {
            self.cleanup.history_open = false;
            self.cleanup.error = None;
            self.focus_handle.focus(window, cx);
            cx.notify();
        } else {
            return false;
        }
        if !self.cleanup.history_open
            && self.cleanup.result.is_none()
            && self.cleanup.refresh_after_history
        {
            self.cleanup.refresh_after_history = false;
            self.reload_cleanup(window, cx);
        }
        true
    }
}
