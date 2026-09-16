//! Both availability states use the same two-line row and column layout.
use super::*;

fn reason_label(reason: &str) -> &str {
    match reason {
        "This source does not support independent file cleanup"
        | "No independently owned files" => t("Individual cleanup unsupported"),
        "Remote sessions are read-only" | "Remote mirrors are read-only" => t("Remote · read-only"),
        "Shared cleanup target" | "Target also contains another session" => {
            t("Shared session files")
        }
        "Source file is missing" => t("Source file missing"),
        "Source is not enabled" => t("Source disabled"),
        "Network locations cannot be moved to the Recycle Bin" => t("Network location"),
        "This location does not support safe recycling" => t("Recycling unavailable"),
        "Parent session is unavailable" | "Session tree contains a cycle" => {
            t("Session tree unverified")
        }
        "Symbolic links are not supported"
        | "Shared hard links are not supported"
        | "Source path contains links or is not absolute"
        | "Source is outside the enabled location"
        | "Cleanup target is outside the source location" => t("File ownership unverified"),
        "Session has unrecognized content" => t("Unrecognized session format"),
        _ => unavailable_reason(reason),
    }
}

impl Workbench {
    pub(super) fn render_cleanup_row(
        &self,
        ix: usize,
        date_field: TimeField,
        cx: &Context<Self>,
    ) -> AnyElement {
        let item = &self.cleanup.shown[ix];
        let meta = item.session();
        let candidate = item.candidate();
        let (members, reason) = match item {
            CleanupEntry::Available(c) => (&c.sessions, None),
            CleanupEntry::Unavailable(item) => (&item.sessions, Some(item.reason.as_str())),
        };
        let selectable = candidate.is_some();
        let theme = cx.theme();
        let key = meta.key.clone();
        let preview_key = key.clone();
        let keyboard_key = key.clone();
        let selected = selectable && self.cleanup.selected.contains(&key);
        let entity = cx.entity();
        let source = format!("{} · {}", meta.agent.display_name(), meta.project_name);
        let date = match date_field {
            TimeField::Created => meta.created_at,
            TimeField::Updated => item.updated_at(),
        };
        div()
            .w_full()
            .px(SPACE_LG)
            .py(px(2.))
            .child(
                h_flex()
                    .id(SharedString::from(format!("cleanup-{key}")))
                    .w_full()
                    .min_w_0()
                    .h(px(60.))
                    .px(SPACE_SM)
                    .rounded(theme.radius)
                    .gap(SPACE_MD)
                    .items_center()
                    .when(selected, |row| row.bg(theme.list_active))
                    .when(!selected, |row| {
                        row.hover(|style| style.bg(theme.list_hover))
                    })
                    .child(
                        h_flex()
                            .flex_1()
                            .min_w_0()
                            .gap(SPACE_MD)
                            .items_center()
                            .child(
                                Checkbox::new(SharedString::from(format!("select-{key}")))
                                    .checked(selected)
                                    .disabled(
                                        !selectable || self.cleanup.busy || self.cleanup.loading,
                                    )
                                    .when_some(reason, |checkbox, reason| {
                                        checkbox.tooltip(unavailable_reason(reason).to_owned())
                                    })
                                    .on_click(move |checked, _, cx| {
                                        if !selectable {
                                            return;
                                        }
                                        entity.update(cx, |this, cx| {
                                            if *checked {
                                                this.cleanup.selected.insert(key.clone());
                                            } else {
                                                this.cleanup.selected.remove(&key);
                                            }
                                            cx.notify();
                                        });
                                    }),
                            )
                            .child(
                                v_flex()
                                    .id(("cleanup-preview", ix))
                                    .focusable()
                                    .tab_stop(true)
                                    .cursor_pointer()
                                    .hover(|style| style.text_color(theme.primary))
                                    .focus(|style| {
                                        style.bg(theme.list_hover).rounded(RADIUS_BUTTON)
                                    })
                                    .on_key_down(cx.listener(
                                        move |this, event: &KeyDownEvent, window, cx| {
                                            if event.keystroke.key == "enter"
                                                || event.keystroke.key == "space"
                                            {
                                                this.show_cleanup_preview(
                                                    &keyboard_key,
                                                    window,
                                                    cx,
                                                );
                                                cx.stop_propagation();
                                            }
                                        },
                                    ))
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.show_cleanup_preview(&preview_key, window, cx);
                                    }))
                                    .flex_1()
                                    .min_w_0()
                                    .gap(SPACE_XS)
                                    .child(cleanup_session_title(
                                        meta,
                                        members.iter().any(|s| s.favorite),
                                        members.iter().any(|s| s.pinned),
                                        ("cleanup-title", ix),
                                        cx,
                                    ))
                                    .child(
                                        h_flex()
                                            .min_w_0()
                                            .gap(SPACE_XS)
                                            .items_center()
                                            .text_size(FONT_LABEL)
                                            .text_color(theme.muted_foreground)
                                            .child(
                                                img(meta.agent.brand_icon(theme.mode.is_dark()))
                                                    .size(px(15.))
                                                    .flex_shrink_0(),
                                            )
                                            .child(
                                                div()
                                                    .id(("cleanup-source", ix))
                                                    .min_w_0()
                                                    .max_w(px(128.))
                                                    .child(badge(
                                                        meta.project_name.clone(),
                                                        theme.muted,
                                                        theme.muted_foreground,
                                                    ))
                                                    .tooltip(move |window, cx| {
                                                        gpui_component::tooltip::Tooltip::new(
                                                            source.clone(),
                                                        )
                                                        .build(window, cx)
                                                    }),
                                            )
                                            .when_some(reason, |row, reason| {
                                                let full_reason =
                                                    unavailable_reason(reason).to_owned();
                                                row.child(div().px(SPACE_XS).child("·")).child(
                                                    div()
                                                        .id(("cleanup-reason", ix))
                                                        .flex_1()
                                                        .min_w_0()
                                                        .truncate()
                                                        .child(reason_label(reason).to_owned())
                                                        .tooltip(move |window, cx| {
                                                            gpui_component::tooltip::Tooltip::new(
                                                                full_reason.clone(),
                                                            )
                                                            .build(window, cx)
                                                        }),
                                                )
                                            })
                                            .when(selectable && members.len() > 1, |row| {
                                                row.child(crate::tp!(
                                                    "{} nested session",
                                                    "{} nested sessions",
                                                    members.len() - 1
                                                ))
                                            }),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .id(("cleanup-date", ix))
                            .w(CLEANUP_DATE_WIDTH)
                            .flex_shrink_0()
                            .text_right()
                            .text_size(FONT_LABEL)
                            .text_color(theme.muted_foreground)
                            .child(if date > 0 {
                                smart_time(date)
                            } else {
                                "—".into()
                            })
                            .tooltip({
                                let dates = format!(
                                    "{} · {}\n{} · {}",
                                    t("Date created"),
                                    abs_date(meta.created_at),
                                    t("Date updated"),
                                    abs_date(item.updated_at())
                                );
                                move |window, cx| {
                                    gpui_component::tooltip::Tooltip::new(dates.clone())
                                        .build(window, cx)
                                }
                            }),
                    )
                    .child(
                        div()
                            .id(("cleanup-size", ix))
                            .w(CLEANUP_SIZE_WIDTH)
                            .flex_shrink_0()
                            .text_right()
                            .text_size(FONT_BODY)
                            .when(!selectable, |size| {
                                size.text_color(theme.muted_foreground)
                                    .tooltip(|window, cx| {
                                        gpui_component::tooltip::Tooltip::new(t(
                                            "File size unavailable",
                                        ))
                                        .build(window, cx)
                                    })
                            })
                            .child(
                                candidate
                                    .map(|c| bytes(c.bytes))
                                    .unwrap_or_else(|| "—".into()),
                            ),
                    ),
            )
            .into_any_element()
    }
}
