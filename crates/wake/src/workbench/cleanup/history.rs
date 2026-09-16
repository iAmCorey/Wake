//! Cleanup history is a navigable list, with file diagnostics disclosed per session.
use super::*;
use wake_core::cleanup::CleanupRecord;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Outcome {
    Restored,
    Moved,
    PartlyRestored,
    Partial,
    Interrupted,
    NeedsAttention,
    NotStarted,
}
impl Outcome {
    fn label(self) -> &'static str {
        match self {
            Self::Restored => t("Restored"),
            Self::Moved => t("Moved to Trash"),
            Self::PartlyRestored => t("Partially restored"),
            Self::Partial => t("Partially completed"),
            Self::Interrupted => t("Interrupted"),
            Self::NeedsAttention => t("Needs attention"),
            Self::NotStarted => t("Not started"),
        }
    }
    fn description(self) -> &'static str {
        match self {
            Self::Restored => t("These sessions are available in Wake again."),
            Self::Moved => t("Session files were moved to the system trash."),
            Self::PartlyRestored => t("Some sessions have been restored to Wake."),
            Self::Partial | Self::NeedsAttention => t("Review the affected sessions below."),
            Self::Interrupted => {
                t("This cleanup was interrupted. Review the file details before continuing.")
            }
            Self::NotStarted => t("No session files were moved."),
        }
    }
    fn icon(self) -> &'static str {
        match self {
            Self::Restored | Self::PartlyRestored => "icons/refresh-cw.svg",
            Self::Moved => "icons/check.svg",
            Self::Partial | Self::Interrupted | Self::NeedsAttention => "icons/info.svg",
            Self::NotStarted => "icons/brush-cleaning.svg",
        }
    }
    fn color(self, cx: &App) -> Hsla {
        if self.needs_attention() {
            cx.theme().danger
        } else {
            match self {
                Self::Restored | Self::PartlyRestored => cx.theme().primary,
                Self::Moved => cx.theme().success,
                _ => cx.theme().muted_foreground,
            }
        }
    }
    fn needs_attention(self) -> bool {
        matches!(
            self,
            Self::Partial | Self::Interrupted | Self::NeedsAttention
        )
    }
}

fn record_outcome(restored: bool, indexed: bool, error: bool, targets: &[TargetStatus]) -> Outcome {
    if restored {
        Outcome::Restored
    } else if !error && indexed && targets.iter().all(|s| *s == TargetStatus::Moved) {
        Outcome::Moved
    } else if !error && targets.iter().all(|s| *s == TargetStatus::Pending) {
        Outcome::NotStarted
    } else {
        Outcome::NeedsAttention
    }
}
fn outcome(record: &CleanupRecord) -> Outcome {
    record_outcome(
        record.restored,
        record.indexed,
        record
            .error
            .as_deref()
            .is_some_and(|error| !manual_restore_pending(error)),
        &record.targets,
    )
}
fn batch_outcome(finished: bool, states: &[Outcome], any_moved: bool) -> Outcome {
    if states.is_empty() {
        return Outcome::NotStarted;
    }
    if states.iter().all(|s| *s == Outcome::Restored) {
        return Outcome::Restored;
    }
    if !finished {
        return Outcome::Interrupted;
    }
    if states.contains(&Outcome::NeedsAttention) {
        return if any_moved || states.contains(&Outcome::Restored) {
            Outcome::Partial
        } else {
            Outcome::NeedsAttention
        };
    }
    if states.iter().all(|s| *s == Outcome::NotStarted) {
        return Outcome::NotStarted;
    }
    if states.contains(&Outcome::NotStarted) {
        return Outcome::Partial;
    }
    if states.contains(&Outcome::Restored) {
        return Outcome::PartlyRestored;
    }
    Outcome::Moved
}
fn batch_status(batch: &CleanupBatch) -> Outcome {
    batch_outcome(
        batch.finished,
        &batch.records.iter().map(outcome).collect::<Vec<_>>(),
        batch
            .records
            .iter()
            .any(|r| r.targets.contains(&TargetStatus::Moved)),
    )
}
fn batch_size(batch: &CleanupBatch) -> u64 {
    batch.records.iter().map(|r| r.candidate.bytes).sum()
}
fn batch_sessions(batch: &CleanupBatch) -> usize {
    batch
        .records
        .iter()
        .map(|r| r.candidate.sessions.len())
        .sum()
}

const HISTORY_ROW_HEIGHT: Pixels = px(88.);

fn history_status(status: Outcome, cx: &App) -> impl IntoElement {
    let color = status.color(cx);
    div()
        .flex_shrink_0()
        .px(SPACE_SM)
        .py(px(3.))
        .rounded(RADIUS_BUTTON)
        .bg(color.opacity(0.08))
        .text_size(FONT_LABEL)
        .text_color(color)
        .child(status.label())
}

fn history_date(stamp: i64) -> String {
    Local
        .timestamp_millis_opt(stamp)
        .single()
        .map(|date| date.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| abs_date(stamp))
}

impl Workbench {
    fn cleanup_history_header(
        &self,
        title: &'static str,
        subtitle: String,
        cx: &Context<Self>,
    ) -> AnyElement {
        let parent = if self.cleanup.history_open && self.cleanup.result.is_some() {
            t("Cleanup history")
        } else {
            t("Clean Up Sessions")
        };
        h_flex()
            .h(LIBRARY_IDENTITY_HEIGHT)
            .flex_shrink_0()
            .px(SPACE_XXL)
            .gap(SPACE_MD)
            .items_start()
            .window_control_area(WindowControlArea::Drag)
            .child(
                div().pt(px(24.)).child(
                    Button::new("cleanup-history-back")
                        .ghost()
                        .map(action_button)
                        .with_size(gpui_component::Size::Medium)
                        .w(px(32.))
                        .px_0()
                        .icon(icon("icons/chevron-left.svg").with_size(px(16.)))
                        .tooltip(parent)
                        .disabled(self.cleanup.busy)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.navigate_cleanup_back(window, cx);
                        })),
                ),
            )
            .child(div().flex_1().min_w_0().child(library_header(
                "cleanup-history-header",
                title,
                subtitle,
                px(0.),
                None,
                cx,
            )))
            .into_any_element()
    }

    pub(super) fn render_cleanup_history_page(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let body = if let Some(batch) = &self.cleanup.result {
            self.render_cleanup_record(batch, cx)
        } else {
            self.render_cleanup_history(window, cx)
        };
        v_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .bg(cx.theme().background)
            .child(body)
            .when_some(self.cleanup.error.as_ref(), |view, error| {
                view.child(
                    div()
                        .px(SPACE_XXL)
                        .py(SPACE_SM)
                        .text_size(FONT_CAPTION)
                        .text_color(cx.theme().danger)
                        .child(unavailable_reason(error).to_owned()),
                )
            })
            .into_any_element()
    }

    fn render_cleanup_history(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let count = self.cleanup.history.len();
        let available_height =
            (window.viewport_size().height - LIBRARY_IDENTITY_HEIGHT - SPACE_SM - SPACE_XXL)
                .max(HISTORY_ROW_HEIGHT);
        let panel_height = (HISTORY_ROW_HEIGHT * count as f32 + px(2.)).min(available_height);
        v_flex()
            .flex_1()
            .min_h_0()
            .child(self.cleanup_history_header(
                t("Cleanup history"),
                crate::tp!("{} cleanup", "{} cleanups", count),
                cx,
            ))
            .when(count == 0, |view| {
                view.child(div().flex_1().flex().items_center().justify_center().child(
                    empty_state_card(
                        "icons/brush-cleaning.svg",
                        px(58.),
                        px(26.),
                        t("No cleanup history yet"),
                        t("Your cleanup results will appear here."),
                        cx,
                    ),
                ))
            })
            .when(count > 0, |view| {
                view.child(
                    div()
                        .flex()
                        .justify_center()
                        .px(SPACE_XXL)
                        .pt(SPACE_SM)
                        .pb(SPACE_XXL)
                        .child(
                            div()
                                .w_full()
                                .max_w(READER_MAX_WIDTH)
                                .h(panel_height)
                                .rounded(cx.theme().radius_lg)
                                .border_1()
                                .border_color(cx.theme().border)
                                .bg(cx.theme().popover)
                                .overflow_hidden()
                                .child(
                                    uniform_list(
                                        "cleanup-history-list",
                                        count,
                                        cx.processor(|this, range: Range<usize>, _, cx| {
                                            range
                                                .map(|ix| this.cleanup_history_row(ix, cx))
                                                .collect()
                                        }),
                                    )
                                    .track_scroll(&self.cleanup.history_scroll)
                                    .size_full(),
                                ),
                        ),
                )
            })
            .into_any_element()
    }

    fn open_cleanup_record(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.cleanup.busy {
            return;
        }
        let Some(batch) = self.cleanup.history.get(ix).cloned() else {
            return;
        };
        self.cleanup.result = Some(batch);
        self.cleanup.expanded_results.clear();
        self.cleanup.result_scroll = ScrollHandle::new();
        self.cleanup.error = None;
        self.focus_handle.focus(window, cx);
        cx.notify();
    }

    fn cleanup_history_row(&self, ix: usize, cx: &Context<Self>) -> AnyElement {
        let batch = &self.cleanup.history[ix];
        let status = batch_status(batch);
        let theme = cx.theme();
        let first = batch.records.first();
        let title = first
            .map(|r| r.candidate.root.title.clone())
            .unwrap_or_else(|| t("Cleanup details").to_owned());
        let title = if batch.records.len() > 1 {
            crate::tf!("{} + {} more", title, batch.records.len() - 1)
        } else {
            title
        };
        h_flex()
            .id(("cleanup-history-row", ix))
            .role(accesskit::Role::Button)
            .aria_label(format!(
                "{} · {} · {} · {}",
                title,
                abs_date(batch.stamp),
                status.label(),
                bytes(batch_size(batch))
            ))
            .h(HISTORY_ROW_HEIGHT)
            .w_full()
            .px(SPACE_LG)
            .gap(SPACE_LG)
            .items_center()
            .when(ix > 0, |row| row.border_t_1().border_color(theme.border))
            .cursor_pointer()
            .focusable()
            .tab_stop(true)
            .hover(|style| style.bg(theme.list_hover))
            .focus(|style| style.bg(theme.list_hover))
            .on_click(
                cx.listener(move |this, _, window, cx| this.open_cleanup_record(ix, window, cx)),
            )
            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                if event.keystroke.key == "enter" || event.keystroke.key == "space" {
                    this.open_cleanup_record(ix, window, cx);
                    cx.stop_propagation();
                }
            }))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_center()
                    .size(px(36.))
                    .flex_shrink_0()
                    .rounded(theme.radius)
                    .bg(theme.background)
                    .when_some(first, |view, record| {
                        view.child(
                            img(record.candidate.root.agent.brand_icon(theme.mode.is_dark()))
                                .size(px(22.)),
                        )
                    }),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap(px(6.))
                    .child(
                        div()
                            .id(("cleanup-history-title", ix))
                            .truncate()
                            .text_size(FONT_BODY)
                            .font_medium()
                            .child(title.clone())
                            .tooltip(move |window, cx| {
                                gpui_component::tooltip::Tooltip::new(title.clone())
                                    .build(window, cx)
                            }),
                    )
                    .child(
                        div()
                            .truncate()
                            .text_size(FONT_CAPTION)
                            .text_color(theme.muted_foreground)
                            .child(format!(
                                "{} · {}",
                                history_date(batch.stamp),
                                session_tally(batch_sessions(batch) as i64)
                            )),
                    ),
            )
            .child(
                v_flex()
                    .flex_shrink_0()
                    .items_end()
                    .gap(px(6.))
                    .child(
                        div()
                            .text_size(FONT_HEADING)
                            .font_medium()
                            .child(bytes(batch_size(batch))),
                    )
                    .child(history_status(status, cx)),
            )
            .child(
                icon("icons/chevron-right.svg")
                    .with_size(px(14.))
                    .text_color(theme.muted_foreground)
                    .flex_shrink_0(),
            )
            .into_any_element()
    }

    fn render_cleanup_record(&self, batch: &CleanupBatch, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let status = batch_status(batch);
        let can_restore = batch.records.iter().any(CleanupRecord::can_restore);
        let can_retry = batch.records.iter().any(|r| {
            !r.indexed
                && !r.restored
                && !r.targets.is_empty()
                && r.targets.iter().all(|s| *s == TargetStatus::Moved)
        });
        v_flex()
            .flex_1()
            .min_h_0()
            .child(self.cleanup_history_header(t("Cleanup details"), abs_date(batch.stamp), cx))
            .child(
                div()
                    .id("cleanup-record-sessions")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.cleanup.result_scroll)
                    .child(
                        div().flex().justify_center().px(SPACE_XXL).child(
                            v_flex()
                                .w_full()
                                .max_w(READER_MAX_WIDTH)
                                .pt(SPACE_SM)
                                .pb(px(40.))
                                .gap(SPACE_XXL)
                                .child(
                                    v_flex()
                                        .flex_shrink_0()
                                        .rounded(theme.radius_lg)
                                        .border_1()
                                        .border_color(theme.border)
                                        .bg(theme.popover)
                                        .child(
                                            h_flex()
                                                .p(SPACE_XL)
                                                .gap(SPACE_LG)
                                                .items_center()
                                                .child(
                                                    div()
                                                        .flex()
                                                        .items_center()
                                                        .justify_center()
                                                        .size(px(36.))
                                                        .flex_shrink_0()
                                                        .rounded(theme.radius)
                                                        .bg(status.color(cx).opacity(0.08))
                                                        .child(
                                                            icon(status.icon())
                                                                .with_size(px(18.))
                                                                .text_color(status.color(cx)),
                                                        ),
                                                )
                                                .child(
                                                    v_flex()
                                                        .flex_1()
                                                        .min_w_0()
                                                        .gap(px(6.))
                                                        .child(
                                                            div()
                                                                .text_size(FONT_HEADING)
                                                                .font_medium()
                                                                .child(status.label()),
                                                        )
                                                        .child(
                                                            div()
                                                                .text_size(FONT_CAPTION)
                                                                .text_color(theme.muted_foreground)
                                                                .child(status.description()),
                                                        ),
                                                )
                                                .child(
                                                    v_flex()
                                                        .flex_shrink_0()
                                                        .items_end()
                                                        .gap(px(4.))
                                                        .child(
                                                            div()
                                                                .text_size(FONT_TITLE)
                                                                .font_semibold()
                                                                .child(bytes(batch_size(batch))),
                                                        )
                                                        .child(
                                                            div()
                                                                .text_size(FONT_CAPTION)
                                                                .text_color(theme.muted_foreground)
                                                                .child(session_tally(
                                                                    batch_sessions(batch) as i64,
                                                                )),
                                                        ),
                                                ),
                                        )
                                        .when(can_restore, |view| {
                                            view.child(
                                                v_flex()
                                                    .px(SPACE_XL)
                                                    .pb(SPACE_LG)
                                                    .gap(SPACE_SM)
                                                    .text_size(FONT_CAPTION)
                                                    .text_color(theme.muted_foreground)
                                                    .child(manual_restore_hint())
                                                    .child(t("Space is freed when the files are removed from Trash.")),
                                            )
                                        })
                                        .when(
                                            can_restore || can_retry || self.cleanup.busy,
                                            |view| {
                                                view.child(self.cleanup_record_actions(
                                                    can_restore,
                                                    can_retry,
                                                    cx,
                                                ))
                                            },
                                        ),
                                )
                                .child(
                                    v_flex()
                                        .gap(SPACE_SM)
                                        .child(
                                            div()
                                                .text_size(FONT_CAPTION)
                                                .font_medium()
                                                .text_color(theme.muted_foreground)
                                                .child(t("Sessions")),
                                        )
                                        .children(batch.records.iter().enumerate().map(
                                            |(ix, record)| self.cleanup_record_row(record, ix, cx),
                                        )),
                                ),
                        ),
                    ),
            )
            .into_any_element()
    }

    fn cleanup_record_actions(
        &self,
        can_restore: bool,
        can_retry: bool,
        cx: &Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        h_flex()
            .flex_shrink_0()
            .px(SPACE_XL)
            .pb(SPACE_XL)
            .gap(SPACE_SM)
            .flex_wrap()
            .when(self.cleanup.busy, |row| {
                row.child(Spinner::new().small()).child(
                    div()
                        .text_size(FONT_CAPTION)
                        .text_color(theme.muted_foreground)
                        .child(t("Checking files…")),
                )
            })
            .when(can_restore, |row| {
                row.child(
                    crate::settings::settings_button(Button::new("cleanup-open-trash"), cx)
                        .map(action_button)
                        .label(t("Trash"))
                        .tooltip(t("Open system trash"))
                        .disabled(self.cleanup.busy)
                        .on_click(|_, _, _| terminal::open_trash()),
                )
                .child(
                    crate::settings::settings_button(Button::new("cleanup-reindex"), cx)
                        .map(action_button)
                        .label(t("Check"))
                        .disabled(self.cleanup.busy)
                        .tooltip(t("Check restored files"))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.check_cleanup_batch(true, window, cx)
                        })),
                )
            })
            .when(can_retry, |row| {
                row.child(
                    crate::settings::settings_button(Button::new("cleanup-retry-index"), cx)
                        .map(action_button)
                        .label(t("Update session list"))
                        .disabled(self.cleanup.busy)
                        .tooltip(t(
                            "Files were moved, but Wake could not update its session list.",
                        ))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.check_cleanup_batch(false, window, cx)
                        })),
                )
            })
            .into_any_element()
    }

    fn cleanup_record_row(
        &self,
        record: &CleanupRecord,
        ix: usize,
        cx: &Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let candidate = &record.candidate;
        let status = outcome(record);
        let key = candidate.root.key.clone();
        let expanded = self.cleanup.expanded_results.contains(&key);
        let show_error = !record.restored
            && record
                .error
                .as_deref()
                .is_some_and(|error| !manual_restore_pending(error));
        let click_key = key.clone();
        let file_label = crate::tf!("File locations ({})", candidate.targets.len());
        v_flex()
            .w_full()
            .min_w_0()
            .flex_shrink_0()
            .rounded(theme.radius_lg)
            .border_1()
            .border_color(theme.border)
            .bg(theme.popover)
            .overflow_hidden()
            .child(
                h_flex()
                    .p(SPACE_LG)
                    .gap(SPACE_MD)
                    .items_center()
                    .child(
                        img(candidate.root.agent.brand_icon(theme.mode.is_dark()))
                            .size(px(24.))
                            .flex_shrink_0(),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap(px(6.))
                            .child(cleanup_title(candidate, ("history-session-title", ix), cx))
                            .child(
                                div()
                                    .truncate()
                                    .text_size(FONT_CAPTION)
                                    .text_color(theme.muted_foreground)
                                    .child(format!(
                                        "{} · {}",
                                        candidate.root.project_name,
                                        bytes(candidate.bytes)
                                    ))
                                    .when(candidate.sessions.len() > 1, |row| {
                                        row.child(format!(
                                            " · {}",
                                            session_tally(candidate.sessions.len() as i64)
                                        ))
                                    }),
                            ),
                    )
                    .child(history_status(status, cx)),
            )
            .when(show_error, |view| {
                view.child(
                    div()
                        .px(SPACE_LG)
                        .pb(SPACE_LG)
                        .text_size(FONT_CAPTION)
                        .text_color(theme.danger)
                        .child(
                            unavailable_reason(record.error.as_deref().unwrap_or_default())
                                .to_owned(),
                        ),
                )
            })
            .child(
                h_flex()
                    .id(("cleanup-result-files", ix))
                    .role(accesskit::Role::Button)
                    .aria_label(format!("{} · {}", candidate.root.title, file_label))
                    .h(px(40.))
                    .px(SPACE_LG)
                    .gap(SPACE_SM)
                    .border_t_1()
                    .border_color(theme.border)
                    .text_size(FONT_CAPTION)
                    .text_color(theme.muted_foreground)
                    .cursor_pointer()
                    .focusable()
                    .tab_stop(true)
                    .hover(|style| style.bg(theme.list_hover))
                    .focus(|style| style.bg(theme.list_hover))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if !this.cleanup.expanded_results.remove(&click_key) {
                            this.cleanup.expanded_results.insert(click_key.clone());
                        }
                        cx.notify();
                    }))
                    .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                        if event.keystroke.key == "enter" || event.keystroke.key == "space" {
                            if !this.cleanup.expanded_results.remove(&key) {
                                this.cleanup.expanded_results.insert(key.clone());
                            }
                            cx.notify();
                            cx.stop_propagation();
                        }
                    }))
                    .child(icon("icons/folder.svg").with_size(px(14.)))
                    .child(div().flex_1().child(file_label))
                    .child(
                        icon(if expanded {
                            "icons/chevron-down.svg"
                        } else {
                            "icons/chevron-right.svg"
                        })
                        .with_size(px(14.)),
                    ),
            )
            .when(expanded, |view| {
                view.child(
                    v_flex()
                        .min_w_0()
                        .px(SPACE_LG)
                        .pb(SPACE_LG)
                        .pt(SPACE_SM)
                        .gap(SPACE_LG)
                        .children(
                            candidate
                                .targets
                                .iter()
                                .zip(&record.targets)
                                .enumerate()
                                .map(|(target_ix, (target, target_status))| {
                                    let path = target.path.to_string_lossy().into_owned();
                                    let parent = target
                                        .path
                                        .parent()
                                        .map(|p| tilde_path(&p.to_string_lossy()))
                                        .unwrap_or_default();
                                    let label = if record.restored {
                                        t("Restored")
                                    } else {
                                        match target_status {
                                            TargetStatus::Pending => t("Not started"),
                                            TargetStatus::Moving => t("Verify in system trash"),
                                            TargetStatus::Moved => t("Moved to Trash"),
                                            TargetStatus::Failed(_) => t("Failed"),
                                        }
                                    };
                                    v_flex()
                                        .min_w_0()
                                        .gap(SPACE_XS)
                                        .child(
                                            h_flex()
                                                .gap(SPACE_MD)
                                                .min_w_0()
                                                .child(div().flex_1().min_w_0().child(
                                                    cleanup_path(
                                                        path,
                                                        SharedString::from(format!(
                                                            "history-file-{ix}-{target_ix}"
                                                        )),
                                                        cx,
                                                    ),
                                                ))
                                                .child(
                                                    div()
                                                        .flex_shrink_0()
                                                        .text_size(FONT_LABEL)
                                                        .text_color(theme.muted_foreground)
                                                        .child(label),
                                                ),
                                        )
                                        .child(
                                            div()
                                                .truncate()
                                                .text_size(FONT_LABEL)
                                                .text_color(theme.muted_foreground)
                                                .child(parent),
                                        )
                                }),
                        ),
                )
            })
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::{batch_outcome, record_outcome, Outcome, TargetStatus};
    #[test]
    fn trash_permission_errors_offer_manual_recovery_without_hiding_other_failures() {
        use super::{manual_restore_hint, manual_restore_pending, unavailable_reason};
        let expected = manual_restore_hint();
        for error in [
            "Cannot access Trash: Operation not permitted (os error 1)",
            "Finder could not list Trash: Not authorized to send Apple events",
            "Could not restore this file from Trash: Permission denied (os error 13)",
            "IO error for operation on /Users/tester/.Trash/session/: Operation not permitted (os error 1): Operation not permitted (os error 1)",
            "Restored files are still missing from their original locations.",
        ] {
            assert_eq!(unavailable_reason(error), expected);
            assert!(manual_restore_pending(error));
        }
        let conflict = "The original location contains different files. Nothing was overwritten.";
        assert_eq!(unavailable_reason(conflict), conflict);
        assert!(!manual_restore_pending(conflict));
    }
    #[test]
    fn unfinished_moves_and_failed_indexing_need_attention() {
        assert_eq!(
            record_outcome(false, false, false, &[TargetStatus::Moved]),
            Outcome::NeedsAttention
        );
        assert_eq!(
            record_outcome(false, false, false, &[TargetStatus::Moving]),
            Outcome::NeedsAttention
        );
        assert_eq!(
            record_outcome(false, true, false, &[TargetStatus::Moved]),
            Outcome::Moved
        );
        assert_eq!(
            record_outcome(false, true, true, &[TargetStatus::Moved]),
            Outcome::NeedsAttention
        );
        assert_eq!(
            record_outcome(false, false, false, &[TargetStatus::Pending]),
            Outcome::NotStarted
        );
        assert_eq!(
            record_outcome(false, false, true, &[TargetStatus::Pending]),
            Outcome::NeedsAttention
        );
    }
    #[test]
    fn historical_outcomes_distinguish_partial_work_and_restoration() {
        use Outcome::*;
        assert_eq!(batch_outcome(true, &[], false), NotStarted);
        assert_eq!(batch_outcome(false, &[Moved], true), Interrupted);
        assert_eq!(batch_outcome(true, &[Moved, NotStarted], true), Partial);
        assert_eq!(batch_outcome(true, &[NeedsAttention], true), Partial);
        assert_eq!(
            batch_outcome(true, &[NeedsAttention], false),
            NeedsAttention
        );
        assert_eq!(
            batch_outcome(true, &[Moved, Restored], true),
            PartlyRestored
        );
        assert_eq!(batch_outcome(true, &[Restored], true), Restored);
    }
}
