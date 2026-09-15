//! Cached, virtualized conversation page of the command palette.
use super::*;
use diri_proto::HistoryEntry;
use diri_ui::{AgentLogo, Typo};
use gpui::CursorStyle;
use std::collections::HashSet;
use std::time::{SystemTime, UNIX_EPOCH};

impl NavigationOverlay {
    pub(super) fn refresh_history(&mut self, cx: &mut Context<Self>) {
        // Reopening uses the last result immediately and shares any in-flight
        // scan. History is local data: a disconnected Engine must not delay it.
        let Some(mut scanner) = self.history_scanner.take() else {
            return;
        };
        self.history_loading = true;
        self.history_error = None;
        cx.notify();

        let roots = crate::history::HistoryRoots::current_user();
        let tracked = self
            .store
            .read()
            .expect("session store lock poisoned")
            .sessions()
            .values()
            .filter_map(|session| session.agent_session_id.clone())
            .collect();
        let runtime = Arc::clone(&self.tokio);
        cx.spawn(async move |this, cx| {
            let task = runtime.spawn(async move {
                tokio::task::spawn_blocking(move || {
                    let entries = scanner.scan(&roots, &tracked);
                    (scanner, entries)
                })
                .await
                .map_err(|error| error.to_string())
            });
            let result = task
                .await
                .map_err(|error| error.to_string())
                .and_then(|r| r);
            let _ = this.update(cx, |this, cx| {
                this.history_loading = false;
                match result {
                    Ok((scanner, mut entries)) => {
                        // A session can become tracked while the disk scan is
                        // running. Reconcile against the current store as well.
                        let tracked = this
                            .store
                            .read()
                            .expect("session store lock poisoned")
                            .sessions()
                            .values()
                            .filter_map(|session| session.agent_session_id.clone())
                            .collect::<HashSet<_>>();
                        entries.retain(|entry| !tracked.contains(&entry.id));
                        let selected_id = this.highlighted_history().map(|entry| entry.id.clone());
                        this.history_scanner = Some(scanner);
                        this.history = entries;
                        this.history_search.rebuild(&this.history);
                        if this.overlay == Some(Overlay::History) {
                            this.filter_history();
                        }
                        if this.overlay == Some(Overlay::History)
                            && let Some(index) = this.history_matches.iter().position(|index| {
                                Some(&this.history[*index].id) == selected_id.as_ref()
                            })
                        {
                            this.highlight = index;
                            this.scroll_to_highlight();
                        }
                        if this.overlay != Some(Overlay::History) {
                            this.history_matches.clear();
                        }
                    }
                    Err(error) => {
                        this.history_scanner = Some(crate::history::HistoryScanner::default());
                        this.history_error = Some(error);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn resume_history(
        &mut self,
        entry: HistoryEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.history_resuming.is_some() {
            return;
        }
        let existing = self
            .store
            .read()
            .expect("session store lock poisoned")
            .sessions()
            .values()
            .find(|session| {
                session.agent_session_id.as_deref() == Some(&entry.id) && session.kind == entry.kind
            })
            .map(|session| session.id.clone());
        if let Some(id) = existing {
            self.store
                .write()
                .expect("session store lock poisoned")
                .select(id);
            self.close_overlay(window, cx);
            cx.notify();
            return;
        }
        if !entry.cwd_exists || !Path::new(&entry.cwd).is_dir() {
            self.history_error = Some("The conversation folder is no longer available".to_owned());
            cx.notify();
            return;
        }
        self.history_resuming = Some(entry.id.clone());
        self.history_error = None;
        cx.notify();
        let client = Arc::clone(self._runtime.client());
        let runtime = Arc::clone(&self.tokio);
        let conversation_id = entry.id.clone();
        cx.spawn_in(window, async move |this, cx| {
            let task = runtime.spawn(async move {
                client.wait_until_connected(Duration::from_secs(5)).await?;
                crate::history::resume(&client, &entry).await
            });
            let result = match task.await {
                Ok(result) => result.map_err(|error| error.to_string()),
                Err(error) => Err(error.to_string()),
            };
            let _ = this.update_in(cx, |this, window, cx| {
                this.history_resuming = None;
                match result {
                    Ok(id) => {
                        this.history.retain(|entry| entry.id != conversation_id);
                        this.history_search.rebuild(&this.history);
                        if this.overlay == Some(Overlay::History) {
                            this.filter_history();
                        }
                        this.store
                            .write()
                            .expect("session store lock poisoned")
                            .apply_spawn_result(id.clone());
                        if this.overlay == Some(Overlay::History) {
                            this.close_overlay(window, cx);
                        }
                    }
                    Err(error) => this.history_error = Some(error),
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn filter_history(&mut self) {
        self.history_matches = self.history_search.rank(self.query.text());
        self.highlight = self
            .highlight
            .min(self.history_matches.len().saturating_sub(1));
    }

    pub(super) fn highlighted_history(&self) -> Option<&HistoryEntry> {
        self.history_matches
            .get(self.highlight)
            .and_then(|index| self.history.get(*index))
    }

    pub(super) fn render_history_row(&self, index: usize, cx: &mut Context<Self>) -> AnyElement {
        let colors = self.colors();
        let entry = self.history[self.history_matches[index]].clone();
        let selected = index == self.highlight;
        let resumable = entry.cwd_exists;
        let opening = self.history_resuming.as_deref() == Some(&entry.id);
        let busy = self.history_resuming.is_some();
        let title = entry
            .title
            .clone()
            .unwrap_or_else(|| "Untitled conversation".to_owned());
        let detail = format!(
            "{title}\n{} · {}{}",
            entry.kind.id(),
            entry.cwd,
            if resumable {
                ""
            } else {
                "\nFolder unavailable"
            }
        );
        let age = relative_time(entry.last_active_at.0);
        let agent = crate::surface_shell::ui_agent(&entry.kind);
        div()
            .h(px(ROW_HEIGHT))
            .px(px(6.0))
            .py(px(2.0))
            .child(
                div()
                    .id(("history-row", index))
                    .debug_selector(move || format!("history-row-{index}"))
                    .group("history-row")
                    .h_full()
                    .px(px(10.0))
                    .rounded(px(Radius::ROW))
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .bg(Fill::selected(colors, selected))
                    .when(resumable && !busy, |row| {
                        row.cursor_pointer()
                            .active(move |style| style.bg(colors.primary.alpha(0.14)))
                    })
                    .when(!resumable, |row| {
                        row.cursor(CursorStyle::OperationNotAllowed)
                    })
                    .hover(move |style| {
                        style.bg(if selected {
                            Fill::selected(colors, true)
                        } else {
                            Fill::hover(colors, true)
                        })
                    })
                    .tooltip(move |_, cx| cx.new(|_| PaletteTooltip(detail.clone(), colors)).into())
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.highlight = index;
                        this.resume_history(entry.clone(), window, cx);
                    }))
                    .child(AgentLogo::new(agent, 28.0, colors).badged(false))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .text_size(px(Typo::ROW.size))
                            .text_color(if selected {
                                colors.primary
                            } else {
                                colors.text(diri_ui::TextTone::Unselected)
                            })
                            .truncate()
                            .child(title),
                    )
                    .child(
                        div()
                            .relative()
                            .flex_none()
                            .w(px(KEYCAP_WIDTH))
                            .h(px(KEYCAP_HEIGHT))
                            .flex()
                            .items_center()
                            .justify_end()
                            .child(if opening {
                                LoadingIndicator::new("history-opening", 12.0, colors.secondary)
                                    .into_any_element()
                            } else if !resumable {
                                sf_symbol("exclamationmark.triangle", 12.0, colors.secondary)
                            } else {
                                div()
                                    .text_size(px(Typo::META.size))
                                    .text_color(colors.tertiary)
                                    .when(selected && !busy, |age| age.invisible())
                                    .when(!busy, |age| {
                                        age.group_hover("history-row", |style| style.invisible())
                                    })
                                    .child(age)
                                    .into_any_element()
                            })
                            .when(resumable && !busy, |slot| {
                                slot.child(
                                    keycap(colors)
                                        .debug_selector(move || format!("history-return-{index}"))
                                        .absolute()
                                        .right_0()
                                        .top_0()
                                        .when(!selected, |cue| cue.invisible())
                                        .group_hover("history-row", |style| style.visible())
                                        .child(Icon::new(IconName::Return, 14.0, colors.secondary)),
                                )
                            }),
                    ),
            )
            .into_any_element()
    }
}

fn relative_time(milliseconds: f64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |duration| duration.as_secs_f64() * 1000.0);
    let seconds = ((now - milliseconds).max(0.0) / 1000.0) as u64;
    match seconds {
        0..=59 => "now".to_owned(),
        60..=3_599 => format!("{}m", seconds / 60),
        3_600..=86_399 => format!("{}h", seconds / 3_600),
        _ => format!("{}d", seconds / 86_400),
    }
}
