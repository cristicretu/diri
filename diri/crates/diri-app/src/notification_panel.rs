//! Notification tray. Uses the app's existing type, color and motion tokens.
use super::*;
use crate::notification_feed::{NotificationEntry, NotificationKind};
use crate::palette_chrome::{PaletteTooltip, keycap, scroll_fades};
use diri_ui::{Fill, HairlineDivider, Icon, IconName};
use gpui::{ScrollStrategy, uniform_list};

const NOTIFICATION_ROW_HEIGHT: f32 = 52.0;

impl RootView {
    pub(super) fn toggle_notifications(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.notification_panel_open = !self.notification_panel_open;
        self.notification_selected = 0;
        self.notification_options_open = false;
        self.notification_scroll
            .scroll_to_item(0, ScrollStrategy::Top);
        #[cfg(target_os = "macos")]
        if self.notification_panel_open {
            self.notifier.refresh_health();
        }
        if self.notification_panel_open {
            self.notification_focus.focus(window, cx);
        } else if let Some(terminal) = &self.terminal {
            terminal.update(cx, |terminal, cx| terminal.focus(window, cx));
        }
        cx.notify();
    }

    pub(super) fn notification_rows(&self) -> Vec<usize> {
        self.services
            .store
            .store
            .read()
            .expect("store")
            .notifications()
            .entries()
            .iter()
            .enumerate()
            .filter(|(_, entry)| !self.notification_filter_unread || !entry.read)
            .map(|(index, _)| index)
            .collect()
    }

    pub(super) fn open_notification(
        &mut self,
        session: SessionId,
        event: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if session.0.is_empty() {
            if !self.notification_panel_open {
                self.toggle_notifications(window, cx);
            }
            cx.activate(true);
            window.activate_window();
            return;
        }
        if !self
            .services
            .store
            .store
            .read()
            .expect("store")
            .has_hydrated_sessions()
        {
            self.pending_notification_open = Some((session, event));
            cx.activate(true);
            window.activate_window();
            return;
        }
        let available = {
            let mut store = self.services.store.store.write().expect("store");
            let available = store.sessions().get(&session).is_some_and(|record| {
                !record.is_archived()
                    && event
                        .as_ref()
                        .and_then(|id| {
                            store
                                .notifications()
                                .entries()
                                .iter()
                                .find(|entry| &entry.id == id)
                        })
                        .is_none_or(|entry| entry.incarnation == record.created_at.0.to_bits())
            });
            if available {
                store.select(session.clone());
                store.mark_notifications_read(&session);
                if let Some(id) = event {
                    store.set_notification_read(&id, true);
                }
            }
            available
        };
        if !available {
            self.show_quote_feedback(
                "Session unavailable",
                "This session was closed or archived. Its notification remains in your history.",
                cx,
            );
            return;
        }
        self.notification_panel_open = false;
        self.launcher
            .update(cx, |launcher, cx| launcher.dismiss(cx));
        if let Some(surfaces) = &self.utility_surfaces {
            surfaces.update(cx, |surfaces, cx| surfaces.dismiss(cx));
        }
        cx.activate(true);
        window.activate_window();
        if let Some(terminal) = &self.terminal {
            terminal.update(cx, |terminal, cx| terminal.focus(window, cx));
        }
        cx.notify();
    }

    pub(super) fn notification_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let rows = self.notification_rows();
        match event.keystroke.key.as_str() {
            "escape" => self.toggle_notifications(window, cx),
            "down" => {
                self.notification_selected =
                    (self.notification_selected + 1).min(rows.len().saturating_sub(1))
            }
            "up" => self.notification_selected = self.notification_selected.saturating_sub(1),
            "enter" => {
                let entry = rows.get(self.notification_selected).and_then(|index| {
                    self.services
                        .store
                        .store
                        .read()
                        .expect("store")
                        .notifications()
                        .entries()
                        .get(*index)
                        .cloned()
                });
                if let Some(entry) = entry {
                    self.open_notification(
                        entry.session_id.clone(),
                        Some(entry.id.clone()),
                        window,
                        cx,
                    );
                }
            }
            _ => return false,
        }
        self.notification_scroll
            .scroll_to_item(self.notification_selected, ScrollStrategy::Nearest);
        cx.stop_propagation();
        cx.notify();
        true
    }

    fn notification_row(
        &self,
        index: usize,
        entry: NotificationEntry,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let selected = index == self.notification_selected;
        let muted = self
            .services
            .store
            .store
            .read()
            .expect("store")
            .preferences()
            .muted_notification_sessions
            .contains(&entry.session_id.0);
        let (icon, label, tone) = if entry.resolved {
            (IconName::CheckCircle, "Resolved", colors.secondary)
        } else {
            match entry.kind {
                NotificationKind::NeedsInput => (IconName::Comment, "Needs you", Ink::ATTENTION),
                NotificationKind::Done => (IconName::CheckCircle, "Completed", Ink::FRESH),
                NotificationKind::Failed => (IconName::Warning, "Stopped", Ink::ATTENTION),
                NotificationKind::Custom => (IconName::Bell, "Notification", colors.secondary),
            }
        };
        let detail = format!(
            "{label} · {}\n{}\n{}",
            age(entry.created_at_ms),
            entry.title,
            entry.body
        );
        let read_id = entry.id.clone();
        let mute_session = entry.session_id.clone();
        let read = entry.read;
        // Put the chat/task first instead of repeating “Agent finished” on every row.
        let (title, subtitle) = match entry.kind {
            NotificationKind::Done | NotificationKind::NeedsInput if !entry.body.is_empty() => {
                (entry.body, entry.title)
            }
            _ => (entry.title, entry.body),
        };
        div()
            .h(px(NOTIFICATION_ROW_HEIGHT))
            .px(px(6.0))
            .py(px(2.0))
            .child(
                div()
                    .id(("notification-row", index))
                    .debug_selector(move || format!("notification-row-{index}"))
                    .group("notification-row")
                    .h_full()
                    .px(px(10.0))
                    .rounded(px(Radius::ROW))
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .cursor_pointer()
                    .bg(Fill::selected(colors, selected))
                    .hover(move |style| {
                        style.bg(if selected {
                            Fill::selected(colors, true)
                        } else {
                            Fill::hover(colors, true)
                        })
                    })
                    .active(move |style| style.bg(colors.primary.alpha(0.14)))
                    .tooltip(move |_, cx| cx.new(|_| PaletteTooltip(detail.clone(), colors)).into())
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_notification(
                            entry.session_id.clone(),
                            Some(entry.id.clone()),
                            window,
                            cx,
                        );
                    }))
                    .child(
                        div()
                            .w(px(28.0))
                            .flex_none()
                            .flex()
                            .justify_center()
                            .child(Icon::new(
                                icon,
                                16.0,
                                if read { colors.tertiary } else { tone },
                            )),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .flex()
                            .flex_col()
                            .gap(px(3.0))
                            .child(
                                div()
                                    .text_size(px(Typo::ROW.size))
                                    .text_color(if read {
                                        colors.secondary
                                    } else {
                                        colors.primary
                                    })
                                    .truncate()
                                    .child(title),
                            )
                            .child(
                                div()
                                    .text_size(px(Typo::META.size))
                                    .text_color(colors.secondary)
                                    .truncate()
                                    .child(subtitle),
                            ),
                    )
                    .child(
                        div()
                            .relative()
                            .w(px(48.0))
                            .h(px(24.0))
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_end()
                            .child(
                                div()
                                    .text_size(px(Typo::META.size))
                                    .text_color(colors.tertiary)
                                    .when(selected, |view| view.invisible())
                                    .group_hover("notification-row", |view| view.invisible())
                                    .child(age(entry.created_at_ms)),
                            )
                            .child(
                                div()
                                    .absolute()
                                    .inset_0()
                                    .flex()
                                    .items_center()
                                    .when(!selected, |view| view.invisible())
                                    .group_hover("notification-row", |view| view.visible())
                                    .child(
                                        action_button(
                                            ("notification-mute", index),
                                            if muted {
                                                IconName::Bell
                                            } else {
                                                IconName::Moon
                                            },
                                            if muted {
                                                "Unmute this chat"
                                            } else {
                                                "Mute this chat"
                                            },
                                            colors,
                                        )
                                        .debug_selector(move || {
                                            format!("notification-mute-{index}")
                                        })
                                        .on_click(
                                            cx.listener(move |this, _, _, cx| {
                                                cx.stop_propagation();
                                                this.services
                                                    .store
                                                    .store
                                                    .write()
                                                    .expect("store")
                                                    .toggle_notification_mute(mute_session.clone());
                                                cx.notify();
                                            }),
                                        ),
                                    )
                                    .child(
                                        action_button(
                                            ("notification-read", index),
                                            if read {
                                                IconName::Bell
                                            } else {
                                                IconName::Check
                                            },
                                            if read { "Mark unread" } else { "Mark read" },
                                            colors,
                                        )
                                        .debug_selector(move || {
                                            format!("notification-read-{index}")
                                        })
                                        .on_click(
                                            cx.listener(move |this, _, _, cx| {
                                                cx.stop_propagation();
                                                this.services
                                                    .store
                                                    .store
                                                    .write()
                                                    .expect("store")
                                                    .set_notification_read(&read_id, !read);
                                                this.clamp_notification_selection();
                                                cx.notify();
                                            }),
                                        ),
                                    ),
                            ),
                    ),
            )
            .into_any_element()
    }

    fn clamp_notification_selection(&mut self) {
        self.notification_selected = self
            .notification_selected
            .min(self.notification_rows().len().saturating_sub(1));
    }

    pub(super) fn notification_panel(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !self.notification_panel_open {
            return None;
        }
        let rows = self.notification_rows();
        let count = rows.len();
        let (colors, sounds, alerts) = {
            let store = self.services.store.store.read().expect("store");
            (
                crate::app_theme::sidebar_colors(store.theme_id()),
                store.preferences().status_sounds,
                store.preferences().status_notifications,
            )
        };
        let settings_open = self
            .utility_surfaces
            .as_ref()
            .is_some_and(|surfaces| surfaces.read(cx).is_settings_open());
        let panel_top = if settings_open {
            14.0
        } else {
            Metrics::TITLE_BAR + 6.0
        };
        let viewport = window.inner_window_bounds().get_bounds().size;
        let list_height = (count.max(1) as f32 * NOTIFICATION_ROW_HEIGHT)
            .min(NOTIFICATION_ROW_HEIGHT * 7.0)
            .min(
                (f32::from(viewport.height)
                    - panel_top
                    - 74.0
                    - if self.notification_options_open {
                        44.0
                    } else {
                        0.0
                    })
                .max(0.0),
            );
        let entity = cx.entity();
        let content = div()
            .flex()
            .flex_col()
            .text_color(colors.primary)
            .child(
                div()
                    .h(px(48.0))
                    .px(px(16.0))
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .child(
                        div()
                            .size(px(28.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(Icon::new(IconName::Bell, 16.0, colors.secondary)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(Typo::ROW.size))
                            .child("Notifications"),
                    )
                    .child(
                        div()
                            .id("notification-filter")
                            .debug_selector(|| "notification-filter".into())
                            .h(px(24.0))
                            .px(px(6.0))
                            .rounded(px(Radius::CHIP))
                            .flex()
                            .items_center()
                            .cursor_pointer()
                            .text_size(px(Typo::META.size))
                            .text_color(colors.secondary)
                            .hover(move |style| style.bg(Fill::hover(colors, true)))
                            .tooltip(move |_, cx| {
                                cx.new(|_| {
                                    PaletteTooltip(
                                        "Show unread or all notifications".into(),
                                        colors,
                                    )
                                })
                                .into()
                            })
                            .child(if self.notification_filter_unread {
                                "Unread"
                            } else {
                                "All"
                            })
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.notification_filter_unread = !this.notification_filter_unread;
                                this.notification_selected = 0;
                                this.notification_scroll
                                    .scroll_to_item(0, ScrollStrategy::Top);
                                cx.notify();
                            })),
                    )
                    .child(
                        action_button(
                            "notification-read-all",
                            IconName::CheckCircle,
                            "Mark all read",
                            colors,
                        )
                        .debug_selector(|| "notification-read-all".into())
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.services
                                .store
                                .store
                                .write()
                                .expect("store")
                                .mark_all_notifications_read();
                            this.clamp_notification_selection();
                            cx.notify();
                        })),
                    )
                    .child(
                        action_button(
                            "notification-options",
                            IconName::More,
                            "Notification options",
                            colors,
                        )
                        .debug_selector(|| "notification-options".into())
                        .when(self.notification_options_open, |button| {
                            button.bg(Fill::selected(colors, true))
                        })
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.notification_options_open = !this.notification_options_open;
                            cx.notify();
                        })),
                    )
                    .child(
                        keycap(colors)
                            .id("close-notifications")
                            .cursor_pointer()
                            .hover(move |style| style.bg(Fill::hover(colors, true)))
                            .child("esc")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.toggle_notifications(window, cx)
                            })),
                    ),
            )
            .child(HairlineDivider::horizontal(colors))
            .child(
                div()
                    .relative()
                    .my(px(6.0))
                    .h(px(list_height))
                    .overflow_hidden()
                    .when(count > 0, |view| {
                        view.child(
                            uniform_list("notification-list", count, move |range, _, cx| {
                                entity.update(cx, |this, cx| {
                                    let entries = {
                                        let store =
                                            this.services.store.store.read().expect("store");
                                        range
                                            .filter_map(|index| {
                                                store
                                                    .notifications()
                                                    .entries()
                                                    .get(rows[index])
                                                    .cloned()
                                                    .map(|entry| (index, entry))
                                            })
                                            .collect::<Vec<_>>()
                                    };
                                    entries
                                        .into_iter()
                                        .map(|(index, entry)| {
                                            this.notification_row(index, entry, colors, cx)
                                        })
                                        .collect()
                                })
                            })
                            .track_scroll(&self.notification_scroll)
                            .size_full(),
                        )
                        .child(scroll_fades(self.notification_scroll.clone(), colors))
                    })
                    .when(count == 0, |view| {
                        view.child(
                            div()
                                .size_full()
                                .flex()
                                .items_center()
                                .justify_center()
                                .gap(px(8.0))
                                .child(Icon::new(IconName::CheckCircle, 16.0, colors.secondary))
                                .child(
                                    div()
                                        .text_size(px(Typo::ROW.size))
                                        .text_color(colors.secondary)
                                        .child(if self.notification_filter_unread {
                                            "You're all caught up"
                                        } else {
                                            "No notifications yet"
                                        }),
                                ),
                        )
                    }),
            )
            .when(self.notification_options_open, |view| {
                view.child(self.notification_options(sounds, alerts, colors, cx))
            });
        let panel = div()
            .id("notification-panel")
            .debug_selector(|| "notification-panel".into())
            .track_focus(&self.notification_focus)
            .absolute()
            .top(px(panel_top))
            .right(px(14.0))
            .w(px((f32::from(viewport.width) - 28.0).clamp(0.0, 440.0)))
            .occlude()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(|_, _, cx| cx.stop_propagation())
            .child(FloatingSurface::new(colors, content).radius(Radius::PANEL));
        Some(
            div()
                .absolute()
                .inset_0()
                .id("notification-dismiss-layer")
                // Consume wheels even at list boundaries and over the header/footer.
                .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
                .on_click(cx.listener(|this, _, window, cx| {
                    cx.stop_propagation();
                    this.toggle_notifications(window, cx);
                }))
                .child(panel)
                .when(settings_open, |layer| {
                    layer.child(
                        div()
                            .id("notification-inbox-toggle-close")
                            .absolute()
                            .top(px(7.0))
                            .right(px(14.0))
                            .size(px(Metrics::TOOLBAR_CONTROL_SIZE))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(Radius::BADGE))
                            .bg(colors.background)
                            .cursor_pointer()
                            .hover(move |button| button.bg(Fill::hover(colors, true)))
                            .child(Icon::new(IconName::Close, 14.0, colors.secondary))
                            .on_click(cx.listener(|this, _, window, cx| {
                                cx.stop_propagation();
                                this.toggle_notifications(window, cx);
                            })),
                    )
                })
                .into_any_element(),
        )
    }

    fn notification_options(
        &self,
        sounds: bool,
        alerts: bool,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let health = self.notification_health.clone();
        let options = div().border_t_1().border_color(colors.floating_stroke()).h(px(44.0)).px(px(16.0))
            .flex().items_center().gap(px(8.0))
            .child(option_button("notification-alerts", if alerts { "Alerts on" } else { "Alerts off" }, colors)
                .tooltip(move |_, cx| cx.new(|_| PaletteTooltip(health.clone(), colors)).into())
                .on_click(cx.listener(|this, _, _, cx| { this.services.store.store.write().expect("store").toggle_notification_alerts(); cx.notify(); })))
            .child(option_button("notification-sounds", if sounds { "Sounds on" } else { "Sounds off" }, colors)
                .on_click(cx.listener(|this, _, _, cx| {
                    let _ = this.services.store.store.write().expect("store").update_preferences(|prefs| prefs.status_sounds = !prefs.status_sounds); cx.notify();
                })))
            .child(div().flex_1())
            .child(option_button("notification-test", "Test alert", colors)
                .on_click(cx.listener(|this, _, _, cx| {
                    #[cfg(target_os = "macos")]
                    this.notifier.post(&crate::notifications::NotificationRequest {
                        identifier: "diri-notification-test".into(), title: "Diri notifications are ready".into(),
                        body: "You'll find agent updates in Notifications, even when Mac alerts are silenced.".into(),
                        thread_identifier: None, action_data: None, use_system_sound: false,
                    });
                    #[cfg(not(target_os = "macos"))]
                    { this.notification_health = "System alerts are available on macOS. Your inbox works here.".into(); }
                    cx.notify();
                })))
            .child(option_button("notification-clear", "Clear all", colors)
                .on_click(cx.listener(|this, _, _, cx| {
                    this.services.store.store.write().expect("store").clear_notifications(); this.notification_selected = 0; cx.notify();
                })))
            ;
        if cx.reduce_motion() {
            options.into_any_element()
        } else {
            options
                .with_animation(
                    "notification-options-arrival",
                    Animation::new(Duration::from_millis(140)).with_easing(ease_out_quint()),
                    |view, delta| view.opacity(delta),
                )
                .into_any_element()
        }
    }
}

fn action_button(
    id: impl Into<gpui::ElementId>,
    icon: IconName,
    label: &'static str,
    colors: SemanticColors,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .size(px(24.0))
        .rounded(px(Radius::CHIP))
        .cursor_pointer()
        .flex()
        .items_center()
        .justify_center()
        .hover(move |style| style.bg(Fill::hover(colors, true)))
        .tooltip(move |_, cx| cx.new(|_| PaletteTooltip(label.into(), colors)).into())
        .child(Icon::new(icon, 14.0, colors.secondary))
}

fn option_button(
    id: &'static str,
    label: &'static str,
    colors: SemanticColors,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .h(px(24.0))
        .px(px(6.0))
        .rounded(px(Radius::CHIP))
        .cursor_pointer()
        .flex()
        .items_center()
        .text_size(px(Typo::META.size))
        .text_color(colors.secondary)
        .hover(move |style| style.bg(Fill::hover(colors, true)))
        .child(label)
}

fn age(ms: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let seconds = now.saturating_sub(ms) / 1000;
    match seconds {
        0..60 => "now".into(),
        60..3600 => format!("{}m", seconds / 60),
        3600..86400 => format!("{}h", seconds / 3600),
        _ => format!("{}d", seconds / 86400),
    }
}
