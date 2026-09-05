//! Notification tray. Uses the app's existing type, color and motion tokens.
use super::*;
use crate::notification_feed::{NotificationEntry, NotificationKind};

impl RootView {
    pub(super) fn toggle_notifications(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.notification_panel_open = !self.notification_panel_open;
        self.notification_selected = 0;
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

    pub(super) fn notification_rows(&self) -> Vec<NotificationEntry> {
        self.services
            .store
            .store
            .read()
            .expect("store")
            .notifications()
            .entries()
            .iter()
            .filter(|entry| !self.notification_filter_unread || !entry.read)
            .cloned()
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
                if let Some(entry) = rows.get(self.notification_selected) {
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
        cx.stop_propagation();
        cx.notify();
        true
    }

    pub(super) fn notification_panel(
        &self,
        colors: SemanticColors,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !self.notification_panel_open {
            return None;
        }
        let rows = self.notification_rows();
        let (unread, sounds, alerts, reduce_motion) = {
            let store = self.services.store.store.read().expect("store");
            (
                store.notifications().unread_count(),
                store.preferences().status_sounds,
                store.preferences().status_notifications,
                cx.reduce_motion(),
            )
        };
        let height = (f32::from(window.inner_window_bounds().get_bounds().size.height) - 90.0)
            .clamp(200.0, 620.0);
        let mut list = div()
            .id("notification-list")
            .min_h(px(80.0))
            .max_h(px(height - 180.0))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap(px(4.0));
        if rows.is_empty() {
            list = list.child(div().p(px(28.0)).flex().flex_col().items_center().gap(px(10.0))
                .child(sf_symbol("checkmark.circle", 26.0, colors.secondary))
                .child(div().text_color(colors.primary).child(if self.notification_filter_unread { "You're all caught up" } else { "A place for what needs you" }))
                .child(div().text_size(px(Typo::META.size)).text_color(colors.secondary)
                    .child("Agent results, questions, and failures appear here. Keep working—we'll keep track.")));
        }
        for (index, entry) in rows.into_iter().enumerate() {
            let session = entry.session_id.clone();
            let id = entry.id.clone();
            let read_id = entry.id.clone();
            let mute_session = entry.session_id.clone();
            let muted = self
                .services
                .store
                .store
                .read()
                .expect("store")
                .preferences()
                .muted_notification_sessions
                .contains(&mute_session.0);
            let read = entry.read;
            let tone = match entry.kind {
                NotificationKind::NeedsInput | NotificationKind::Failed => Ink::ATTENTION,
                _ => Ink::FRESH,
            };
            let label = if entry.resolved {
                "Resolved"
            } else {
                match entry.kind {
                    NotificationKind::NeedsInput => "Needs you",
                    NotificationKind::Done => "Completed",
                    NotificationKind::Failed => "Stopped",
                    NotificationKind::Custom => "Notification",
                }
            };
            list = list.child(
                div()
                    .id(("notification-row", index))
                    .px(px(12.0))
                    .py(px(10.0))
                    .rounded(px(Radius::ROW))
                    .cursor_pointer()
                    .border_1()
                    .border_color(if index == self.notification_selected {
                        colors.primary.alpha(0.13)
                    } else {
                        colors.primary.alpha(0.03)
                    })
                    .bg(colors
                        .primary
                        .alpha(if index == self.notification_selected {
                            0.05
                        } else {
                            0.015
                        }))
                    .hover(|style| style.bg(colors.primary.alpha(0.07)))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_notification(session.clone(), Some(id.clone()), window, cx)
                    }))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.0))
                            .child(div().size(px(6.0)).rounded_full().bg(if read {
                                colors.primary.alpha(0.12)
                            } else {
                                tone
                            }))
                            .child(
                                div()
                                    .flex_1()
                                    .text_size(px(Typo::META.size))
                                    .text_color(tone)
                                    .child(label),
                            )
                            .child(
                                div()
                                    .text_size(px(Typo::META.size))
                                    .text_color(colors.tertiary)
                                    .child(age(entry.created_at_ms)),
                            )
                            .child(
                                div()
                                    .id(("notification-mute", index))
                                    .px(px(5.0))
                                    .cursor_pointer()
                                    .text_size(px(Typo::META.size))
                                    .text_color(colors.secondary)
                                    .child(if muted { "Unmute" } else { "Mute" })
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        cx.stop_propagation();
                                        this.services
                                            .store
                                            .store
                                            .write()
                                            .expect("store")
                                            .toggle_notification_mute(mute_session.clone());
                                        cx.notify();
                                    })),
                            )
                            .child(
                                div()
                                    .id(("notification-read", index))
                                    .px(px(5.0))
                                    .cursor_pointer()
                                    .text_size(px(Typo::META.size))
                                    .text_color(colors.secondary)
                                    .child(if read { "Mark unread" } else { "Mark read" })
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        cx.stop_propagation();
                                        this.services
                                            .store
                                            .store
                                            .write()
                                            .expect("store")
                                            .set_notification_read(&read_id, !read);
                                        cx.notify();
                                    })),
                            ),
                    )
                    .child(
                        div()
                            .mt(px(5.0))
                            .text_size(px(Typo::ROW.size))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(colors.primary)
                            .child(entry.title),
                    )
                    .child(
                        div()
                            .mt(px(3.0))
                            .text_size(px(Typo::META.size))
                            .text_color(colors.secondary)
                            .child(entry.body),
                    ),
            );
        }
        let panel = div().id("notification-panel").track_focus(&self.notification_focus)
            .absolute().top(px(48.0)).right(px(14.0)).w(px(440.0)).max_h(px(height))
            .p(px(12.0)).rounded(px(Radius::PANEL)).border_1().border_color(colors.primary.alpha(0.12))
            .bg(colors.background).shadow_lg().flex().flex_col().gap(px(12.0))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(div().flex().items_center().gap(px(8.0))
                .child(sf_symbol("bell", 17.0, colors.primary))
                .child(div().flex_1().text_size(px(16.0)).font_weight(FontWeight::SEMIBOLD).child(format!("Notifications · {unread}")))
                .child(div().id("close-notifications").cursor_pointer().p(px(5.0)).child(sf_symbol("xmark", 12.0, colors.secondary))
                    .on_click(cx.listener(|this, _, window, cx| this.toggle_notifications(window, cx)))))
            .child(div().flex().items_center().gap(px(14.0)).text_size(px(Typo::META.size))
                .child(div().id("notification-filter").cursor_pointer().text_color(Ink::FRESH).child(if self.notification_filter_unread { "Unread ▾" } else { "All notifications ▾" })
                    .on_click(cx.listener(|this, _, _, cx| { this.notification_filter_unread = !this.notification_filter_unread; this.notification_selected = 0; cx.notify(); })))
                .child(div().flex_1())
                .child(div().id("notification-read-all").cursor_pointer().child("Mark all read").on_click(cx.listener(|this, _, _, cx| {
                    this.services.store.store.write().expect("store").mark_all_notifications_read(); cx.notify();
                })))
                .child(div().id("notification-clear").cursor_pointer().text_color(colors.secondary).child("Clear").on_click(cx.listener(|this, _, _, cx| {
                    this.services.store.store.write().expect("store").clear_notifications(); cx.notify();
                }))))
            .child(list)
            .child(div().border_t_1().border_color(colors.primary.alpha(0.07)).pt(px(10.0)).flex().flex_col().gap(px(6.0))
                .child(div().flex().items_center().gap(px(12.0)).text_size(px(Typo::META.size))
                    .child(div().id("notification-alerts").cursor_pointer().child(if alerts { "Alerts on" } else { "Alerts off" }).on_click(cx.listener(|this, _, _, cx| {
                        this.services.store.store.write().expect("store").toggle_notification_alerts(); cx.notify();
                    })))
                    .child(div().id("notification-sounds").flex_1().cursor_pointer().child(if sounds { "Sounds on" } else { "Sounds off" }).on_click(cx.listener(|this, _, _, cx| {
                        let _ = this.services.store.store.write().expect("store").update_preferences(|prefs| prefs.status_sounds = !prefs.status_sounds); cx.notify();
                    })))
                    .child(div().id("notification-test").cursor_pointer().text_color(Ink::FRESH).child("Test alert").on_click(cx.listener(|this, _, _, cx| {
                        #[cfg(target_os = "macos")]
                        this.notifier.post(&crate::notifications::NotificationRequest {
                            identifier: "diri-notification-test".into(), title: "Diri notifications are ready".into(),
                            body: "You'll find agent updates in Notifications, even when Mac alerts are silenced.".into(),
                            thread_identifier: None, action_data: None, use_system_sound: false,
                        });
                        cx.notify();
                    }))))
                .child(div().text_size(px(11.0)).text_color(colors.tertiary).child(self.notification_health.clone())));
        let panel = if reduce_motion {
            panel.into_any_element()
        } else {
            panel
                .with_animation(
                    "notification-panel-arrival",
                    Animation::new(Duration::from_millis(160)).with_easing(ease_out_quint()),
                    |panel, delta| panel.opacity(delta).top(px(48.0 - 6.0 * (1.0 - delta))),
                )
                .into_any_element()
        };
        Some(
            div()
                .absolute()
                .inset_0()
                .id("notification-dismiss-layer")
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, window, cx| this.toggle_notifications(window, cx)),
                )
                .child(panel)
                .into_any_element(),
        )
    }
}

fn age(ms: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let seconds = now.saturating_sub(ms) / 1000;
    match seconds {
        0..60 => "now".into(),
        60..3600 => format!("{}m ago", seconds / 60),
        3600..86400 => format!("{}h ago", seconds / 3600),
        _ => format!("{}d ago", seconds / 86400),
    }
}
