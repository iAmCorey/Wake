//! A single-level filter popover: stable date controls and searchable sources.
use super::*;
use gpui_component::Selectable as _;
use std::collections::BTreeSet;

const FILTER_WIDTH: Pixels = px(468.);
const SOURCE_WIDTH: Pixels = px(232.);
const AGENT_WIDTH: Pixels = px(153.);

fn source_choices(
    inventory: &CleanupInventory,
    options: &CleanupOptions,
) -> (BTreeSet<AgentId>, Vec<(String, String)>) {
    // Sidebar counts omit archived sessions. Choices must cover the entire
    // cleanup inventory, including unavailable entries and currently hidden rows.
    let mut roots: Vec<_> = inventory
        .candidates
        .iter()
        .map(|candidate| (&candidate.root, candidate.updated_at))
        .chain(
            inventory
                .unavailable
                .iter()
                .map(|item| (&item.session, item.updated_at())),
        )
        .collect();
    roots.sort_by(|(a, a_time), (b, b_time)| b_time.cmp(a_time).then_with(|| a.key.cmp(&b.key)));
    let mut agents = options.agents.clone();
    let mut projects = Vec::new();
    let mut paths = HashSet::new();
    for (session, _) in roots {
        agents.insert(session.agent);
        if paths.insert(session.project_path.clone()) {
            projects.push((session.project_path.clone(), session.project_name.clone()));
        }
    }
    // A selected source may disappear after a refresh; keep it removable.
    for path in &options.projects {
        if paths.insert(path.clone()) {
            let name = std::path::Path::new(path)
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or(path)
                .to_string();
            projects.push((path.clone(), name));
        }
    }
    (agents, projects)
}

fn has_filters(options: &CleanupOptions) -> bool {
    options.created_days > 0
        || options.updated_days > 0
        || !options.agents.is_empty()
        || !options.projects.is_empty()
        || options.exclude_starred
        || options.exclude_pinned
        || options.only_cleanable
}

fn choice_button(id: impl Into<ElementId>, selected: bool, cx: &App) -> Button {
    let theme = cx.theme();
    Button::new(id)
        .small()
        .h(px(28.))
        .px(SPACE_SM)
        .rounded(RADIUS_BUTTON)
        .custom(
            ButtonCustomVariant::new(cx)
                .color(theme.transparent)
                .foreground(if selected {
                    theme.primary
                } else {
                    theme.foreground
                })
                .hover(if selected {
                    theme.list_active
                } else {
                    theme.list_hover
                })
                .active(theme.list_active),
        )
        .selected(selected)
        .toggled(selected)
        .text_size(FONT_CAPTION)
}

fn choice(
    entity: &Entity<Workbench>,
    id: impl Into<ElementId>,
    selected: bool,
    change: impl Fn(&mut CleanupOptions) + 'static,
    cx: &App,
) -> Button {
    let entity = entity.clone();
    choice_button(id, selected, cx).on_click(move |_, window, cx| {
        if !selected {
            entity.update(cx, |this, cx| {
                change(&mut this.cleanup.options);
                this.apply_cleanup_options(true, window, cx);
            });
        }
    })
}

fn multi_choice(
    entity: &Entity<Workbench>,
    id: impl Into<ElementId>,
    selected: bool,
    change: impl Fn(&mut CleanupOptions, bool) + 'static,
    cx: &App,
) -> Button {
    let entity = entity.clone();
    let theme = cx.theme();
    choice_button(id, selected, cx)
        .custom(
            ButtonCustomVariant::new(cx)
                .color(theme.secondary.opacity(0.45))
                .foreground(if selected {
                    theme.primary
                } else {
                    theme.foreground
                })
                .hover(if selected {
                    theme.list_active
                } else {
                    theme.secondary_hover
                })
                .active(theme.list_active),
        )
        .border_1()
        .border_color(if selected {
            theme.primary.opacity(0.3)
        } else {
            theme.border
        })
        .role(gpui::Role::CheckBox)
        .on_click(move |_, window, cx| {
            entity.update(cx, |this, cx| {
                change(&mut this.cleanup.options, !selected);
                this.apply_cleanup_options(true, window, cx);
            });
        })
}

fn option_label(button: Button, label: &str, selected: bool, cx: &App) -> Button {
    button
        .relative()
        .pr(px(20.))
        .label(clip_display(label, 24))
        .tooltip(label.to_owned())
        .child(div().flex_1())
        .child(
            icon("icons/check.svg")
                .with_size(px(10.))
                .absolute()
                .right(SPACE_XS - px(20.))
                .top(px(9.))
                .text_color(if selected {
                    cx.theme().primary
                } else {
                    cx.theme().transparent
                }),
        )
}

fn agent_label(
    button: Button,
    label: &str,
    leading: impl IntoElement,
    selected: bool,
    cx: &App,
) -> Button {
    option_label(button, label, selected, cx).pl(px(26.)).child(
        // The button's content excludes its padding; return the brand icon
        // to the reserved leading slot without shifting text on selection.
        div()
            .absolute()
            .left(px(6. - 26.))
            .top(px(6.5))
            .size(px(15.))
            .child(leading),
    )
}

fn section(title: &'static str, selected: usize, options: Vec<Button>) -> AnyElement {
    v_flex()
        .w_full()
        .gap(SPACE_SM)
        .child(
            h_flex()
                .gap(SPACE_SM)
                .text_size(FONT_CAPTION)
                .font_medium()
                .child(title)
                .when(selected > 0, |row| row.child(format!("({selected})"))),
        )
        .child(
            h_flex()
                .w_full()
                .gap(SPACE_XS)
                .flex_wrap()
                .children(options),
        )
        .into_any_element()
}

impl Workbench {
    fn prepare_cleanup_filter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(input) = &self.cleanup.filter_input {
            input.update(cx, |input, cx| input.set_value("", window, cx));
        } else {
            let input = cx
                .new(|cx| InputState::new(window, cx).placeholder(t("Search agents or projects")));
            cx.subscribe_in(&input, window, |this, input, event, _, cx| {
                if matches!(event, InputEvent::Change) {
                    this.cleanup.filter_query = input.read(cx).value().trim().to_lowercase();
                    this.cleanup.filter_scroll.set_offset(point(px(0.), px(0.)));
                    cx.notify();
                }
            })
            .detach();
            self.cleanup.filter_input = Some(input);
        }
        self.cleanup.filter_query.clear();
        self.cleanup.filter_scroll.set_offset(point(px(0.), px(0.)));
        cx.notify();
    }

    fn cleanup_date_filter(entity: &Entity<Self>, field: TimeField, cx: &App) -> AnyElement {
        let theme = cx.theme();
        let options = &entity.read(cx).cleanup.options;
        let (id, selected) = match field {
            TimeField::Created => ("cleanup-created-age", options.created_days),
            TimeField::Updated => ("cleanup-updated-age", options.updated_days),
        };
        v_flex()
            .gap(SPACE_SM)
            .child(
                div()
                    .text_size(FONT_CAPTION)
                    .font_medium()
                    .child(time_label(field)),
            )
            .child(
                h_flex()
                    .w_full()
                    .p(px(2.))
                    .rounded(theme.radius)
                    .bg(theme.secondary)
                    .children(
                        [0, 30, 90, 180, 365]
                            .into_iter()
                            .enumerate()
                            .map(|(i, value)| {
                                choice(
                                    entity,
                                    (id, i),
                                    selected == value,
                                    move |options| match field {
                                        TimeField::Created => options.created_days = value,
                                        TimeField::Updated => options.updated_days = value,
                                    },
                                    cx,
                                )
                                .flex_1()
                                .min_w_0()
                                .px(SPACE_XS)
                                // 与 Settings 的 Appearance 分段控件同一材质。
                                .custom(
                                    ButtonCustomVariant::new(cx)
                                        .color(theme.transparent)
                                        .foreground(if selected == value {
                                            theme.foreground
                                        } else {
                                            theme.muted_foreground
                                        })
                                        .hover(theme.secondary_hover)
                                        .active(if theme.mode.is_dark() {
                                            theme.secondary_active
                                        } else {
                                            theme.popover
                                        }),
                                )
                                .when(selected == value, |button| button.shadow_xs())
                                .text_size(FONT_CAPTION)
                                .label(age_label(value))
                            }),
                    ),
            )
            .into_any_element()
    }

    fn cleanup_filter_sources(entity: &Entity<Self>, window: &Window, cx: &App) -> AnyElement {
        let this = entity.read(cx);
        let options = &this.cleanup.options;
        let query = &this.cleanup.filter_query;
        let theme = cx.theme();
        let mut agents = Vec::new();
        let (available_agents, available_projects) =
            source_choices(&this.cleanup.inventory, options);
        for agent in available_agents {
            let label = agent.display_name();
            if !query.is_empty() && !label.to_lowercase().contains(query) {
                continue;
            }
            let selected = options.agents.contains(&agent);
            agents.push(agent_label(
                multi_choice(
                    entity,
                    SharedString::from(format!("cleanup-agent-{}", agent.as_str())),
                    selected,
                    move |o, checked| {
                        if checked {
                            o.agents.insert(agent);
                        } else {
                            o.agents.remove(&agent);
                        }
                    },
                    cx,
                )
                .w(AGENT_WIDTH),
                label,
                img(agent.brand_icon(theme.mode.is_dark())).size(px(15.)),
                selected,
                cx,
            ));
        }
        let mut projects = Vec::new();
        for (path, name) in available_projects {
            if !query.is_empty()
                && !name.to_lowercase().contains(query)
                && !path.to_lowercase().contains(query)
            {
                continue;
            }
            let selected = options.projects.contains(&path);
            let selection_path = path.clone();
            projects.push(
                option_label(
                    multi_choice(
                        entity,
                        SharedString::from(format!("cleanup-project-{path}")),
                        selected,
                        move |o, checked| {
                            if checked {
                                o.projects.insert(selection_path.clone());
                            } else {
                                o.projects.remove(&selection_path);
                            }
                        },
                        cx,
                    )
                    .w(SOURCE_WIDTH),
                    &name,
                    selected,
                    cx,
                )
                .tooltip(path),
            );
        }
        let show_agents = !agents.is_empty();
        let show_projects = !projects.is_empty();
        let no_matches = !show_agents && !show_projects;
        v_flex()
            .id("cleanup-filter-sources")
            .w_full()
            .max_h((window.viewport_size().height - px(542.)).clamp(px(100.), px(264.)))
            .overflow_y_scroll()
            .track_scroll(&this.cleanup.filter_scroll)
            .gap(SPACE_LG)
            .text_color(theme.muted_foreground)
            .when(show_agents, |v| {
                v.child(section(t("Agents"), options.agents.len(), agents))
            })
            .when(show_projects, |v| {
                v.child(section(t("Projects"), options.projects.len(), projects))
            })
            .when(no_matches, |v| {
                v.child(
                    v_flex()
                        .w_full()
                        .min_h(px(80.))
                        .items_center()
                        .justify_center()
                        .gap(SPACE_SM)
                        .child(icon("icons/search.svg").with_size(px(20.)))
                        .child(
                            div()
                                .text_size(FONT_CAPTION)
                                .child(t("No matching agents or projects")),
                        ),
                )
            })
            .vertical_scrollbar(&this.cleanup.filter_scroll)
            .into_any_element()
    }

    fn cleanup_source_search(
        entity: &Entity<Self>,
        input: &Entity<InputState>,
        cx: &App,
    ) -> AnyElement {
        let focus_input = input.clone();
        let clear_input = input.clone();
        let clear_entity = entity.clone();
        search_field_frame(cx)
            .id("cleanup-source-search")
            .w_full()
            .flex_shrink_0()
            .cursor_text()
            .on_click(move |_, window, cx| {
                focus_input.update(cx, |input, cx| input.focus(window, cx));
            })
            .child(icon("icons/search.svg").with_size(px(13.)).flex_shrink_0())
            .child(
                Input::new(input)
                    .appearance(false)
                    .p_0()
                    .flex_1()
                    .min_w_0()
                    .text_size(FONT_CAPTION),
            )
            .when(!input.read(cx).value().is_empty(), |row| {
                row.child(
                    Button::new("cleanup-clear-source-search")
                        .ghost()
                        .small()
                        .size(px(24.))
                        .p_0()
                        .rounded(RADIUS_BUTTON)
                        .icon(icon("icons/circle-x.svg").with_size(px(14.)))
                        .tooltip(t("Clear search"))
                        .on_click(move |_, window, cx| {
                            clear_input.update(cx, |input, cx| {
                                input.set_value("", window, cx);
                                input.focus(window, cx);
                            });
                            clear_entity.update(cx, |this, cx| {
                                this.cleanup.filter_query.clear();
                                this.cleanup.filter_scroll.set_offset(point(px(0.), px(0.)));
                                cx.notify();
                            });
                        }),
                )
            })
            .into_any_element()
    }

    pub(super) fn cleanup_filter(&self, cx: &Context<Self>) -> Popover {
        let entity = cx.entity();
        let on_open = entity.clone();
        let theme = cx.theme();
        Popover::new("cleanup-filter")
            .anchor(Anchor::TopRight)
            .p(SPACE_LG)
            .trigger(
                Button::new("cleanup-filter-trigger")
                    .ghost()
                    .disabled(self.cleanup.busy)
                    .rounded(RADIUS_BUTTON)
                    .icon(
                        icon("icons/sliders-horizontal.svg")
                            .with_size(px(16.))
                            .text_color(if has_filters(&self.cleanup.options) {
                                theme.primary
                            } else {
                                theme.muted_foreground
                            }),
                    )
                    .tooltip(t("Filter")),
            )
            .on_open_change(move |open, window, cx| {
                if *open {
                    on_open.update(cx, |this, cx| this.prepare_cleanup_filter(window, cx));
                }
            })
            .content(move |_, window, cx| {
                let this = entity.read(cx);
                let options = &this.cleanup.options;
                let popover = cx.entity();
                let clear_entity = entity.clone();
                v_flex()
                    .w(FILTER_WIDTH)
                    .gap(SPACE_LG)
                    .child(
                        h_flex().h(px(28.)).items_center().child(
                            div()
                                .text_size(FONT_HEADING)
                                .font_semibold()
                                .child(t("Filter")),
                        ),
                    )
                    .child(
                        v_flex()
                            .gap(SPACE_MD)
                            .child(Self::cleanup_date_filter(&entity, TimeField::Created, cx))
                            .child(Self::cleanup_date_filter(&entity, TimeField::Updated, cx)),
                    )
                    .child(section(
                        t("Exclude"),
                        usize::from(options.exclude_starred)
                            + usize::from(options.exclude_pinned)
                            + usize::from(options.only_cleanable),
                        vec![
                            option_label(
                                multi_choice(
                                    &entity,
                                    "cleanup-exclude-starred",
                                    options.exclude_starred,
                                    |options, checked| options.exclude_starred = checked,
                                    cx,
                                )
                                .w(AGENT_WIDTH),
                                t("Starred"),
                                options.exclude_starred,
                                cx,
                            )
                            .tooltip(t("Applies to the session and its nested sessions.")),
                            option_label(
                                multi_choice(
                                    &entity,
                                    "cleanup-exclude-pinned",
                                    options.exclude_pinned,
                                    |options, checked| options.exclude_pinned = checked,
                                    cx,
                                )
                                .w(AGENT_WIDTH),
                                t("Pinned"),
                                options.exclude_pinned,
                                cx,
                            )
                            .tooltip(t("Applies to the session and its nested sessions.")),
                            option_label(
                                multi_choice(
                                    &entity,
                                    "cleanup-exclude-unavailable",
                                    options.only_cleanable,
                                    |options, checked| options.only_cleanable = checked,
                                    cx,
                                )
                                .w(AGENT_WIDTH),
                                t("Unavailable"),
                                options.only_cleanable,
                                cx,
                            )
                            .tooltip(t("Hide sessions that cannot be deleted.")),
                        ],
                    ))
                    .child(
                        v_flex()
                            .gap(SPACE_MD)
                            .pt(SPACE_LG)
                            .border_t_1()
                            .border_color(cx.theme().border)
                            .child(
                                h_flex()
                                    .items_center()
                                    .justify_between()
                                    .gap(SPACE_LG)
                                    .child(
                                        div()
                                            .text_size(FONT_CAPTION)
                                            .font_medium()
                                            .child(t("Sources")),
                                    )
                                    .when_some(this.cleanup.filter_input.as_ref(), |row, input| {
                                        row.child(
                                            div().w(px(284.)).child(Self::cleanup_source_search(
                                                &entity, input, cx,
                                            )),
                                        )
                                    }),
                            )
                            .child(Self::cleanup_filter_sources(&entity, window, cx)),
                    )
                    .child(
                        h_flex()
                            .gap(SPACE_MD)
                            .items_center()
                            .justify_between()
                            .pt(SPACE_LG)
                            .border_t_1()
                            .border_color(cx.theme().border)
                            .child(
                                crate::settings::settings_button(
                                    Button::new("cleanup-clear-filters")
                                        .label(t("Clear"))
                                        .tooltip(t("Clear filters")),
                                    cx,
                                )
                                .map(action_button)
                                .bg(cx.theme().secondary)
                                .disabled(
                                    !has_filters(options) && this.cleanup.filter_query.is_empty(),
                                )
                                .on_click(move |_, window, cx| {
                                    clear_entity.update(cx, |this, cx| {
                                        let o = &mut this.cleanup.options;
                                        o.created_days = 0;
                                        o.updated_days = 0;
                                        o.agents.clear();
                                        o.projects.clear();
                                        o.exclude_starred = false;
                                        o.exclude_pinned = false;
                                        o.only_cleanable = false;
                                        if let Some(input) = &this.cleanup.filter_input {
                                            input.update(cx, |input, cx| {
                                                input.set_value("", window, cx)
                                            });
                                        }
                                        this.cleanup.filter_query.clear();
                                        this.cleanup
                                            .filter_scroll
                                            .set_offset(point(px(0.), px(0.)));
                                        this.apply_cleanup_options(true, window, cx);
                                    })
                                }),
                            )
                            .child(
                                crate::settings::settings_primary_button(
                                    Button::new("cleanup-filter-done").label(t("Done")),
                                    cx,
                                )
                                .map(action_button)
                                .min_w(px(80.))
                                .bg(cx.theme().primary)
                                .on_click(move |_, window, cx| {
                                    popover.update(cx, |state, cx| state.dismiss(window, cx))
                                }),
                            ),
                    )
            })
    }
}

#[cfg(test)]
mod tests {
    use super::source_choices;
    use std::collections::BTreeSet;
    use wake_core::{
        cleanup::{self, CleanupOptions},
        db::Store,
        models::{AgentId, SessionMeta},
    };

    #[test]
    fn sources_include_archived_unavailable_and_selected_missing_entries() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(&temp.path().join("index.db")).unwrap();
        for (agent, project, archived, updated_at) in [
            (AgentId::Codex, "archive-only", true, 100),
            (AgentId::ClaudeCode, "current", false, 200),
        ] {
            let meta = SessionMeta {
                key: format!("{}:{project}", agent.as_str()),
                id: project.into(),
                host: String::new(),
                agent,
                title: project.into(),
                project_path: format!("/synthetic/{project}"),
                project_name: project.into(),
                file_path: format!("/synthetic/{project}/session.jsonl"),
                created_at: 0,
                updated_at,
                message_count: 1,
                size_bytes: 0,
                git_branch: None,
                model: None,
                tokens_used: None,
                archived,
                source: None,
                favorite: false,
                pinned: false,
            };
            store.write_session(&meta, updated_at, &[]).unwrap();
        }
        // The old sidebar-based source omitted the archived agent and project.
        assert_eq!(store.agent_counts().unwrap().len(), 1);
        assert_eq!(store.list_projects(false).unwrap().len(), 1);
        let inventory = cleanup::inventory(&store, &[]).unwrap();
        assert_eq!(inventory.unavailable.len(), 2);
        let options = CleanupOptions {
            agents: BTreeSet::from([AgentId::Gemini]),
            projects: BTreeSet::from(["/synthetic/missing".into()]),
            only_cleanable: true,
            ..Default::default()
        };
        let (agents, projects) = source_choices(&inventory, &options);
        assert_eq!(
            agents,
            BTreeSet::from([AgentId::Codex, AgentId::ClaudeCode, AgentId::Gemini])
        );
        assert_eq!(
            projects,
            vec![
                ("/synthetic/current".into(), "current".into()),
                ("/synthetic/archive-only".into(), "archive-only".into()),
                ("/synthetic/missing".into(), "missing".into()),
            ]
        );
        // Clearing the selection keeps all real sources, even with unavailable
        // rows excluded. Options never determine which real sources are offered.
        let (_, projects) = source_choices(&inventory, &CleanupOptions::default());
        assert_eq!(projects.len(), 2);
    }
}
