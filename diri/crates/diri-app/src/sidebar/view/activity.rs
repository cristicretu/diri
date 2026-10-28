//! The Activity filter (⌘.): the same sidebar, narrowed to the sessions that
//! are working, waiting on you, or hold news you have not seen, each as a
//! two-line row with its state and how long it has been that way.
//!
//! It is a filter, not a panel: the projection is the sidebar's own, so the
//! headings, selection, keyboard navigation and ⌘1–9 all keep working. What
//! "news you have not seen" means comes from the notification inbox (its
//! unread entries), the same ledger that clears when you look at a session.
//!
//! Cost: nothing while it is off. While it is on, ages ("now", "7m") are
//! recomputed only when one of them would read differently, by one timer
//! armed for that moment; rows whose text did not change are reused.

use super::*;
use std::collections::HashSet;

/// A session row's height while the Activity filter shows its second line.
pub(super) const ACTIVITY_ROW_HEIGHT: f32 = 44.0;

impl Sidebar {
    /// Whether the list is narrowed: a typed filter, or Activity.
    pub(super) fn filtering(&self) -> bool {
        self.activity_filter || !self.filter_query.text().trim().is_empty()
    }

    pub fn activity_filter(&self) -> bool {
        self.activity_filter
    }

    pub fn toggle_activity_filter(&mut self, cx: &mut Context<Self>) {
        self.activity_filter = !self.activity_filter;
        if !self.activity_filter {
            self.age_wake = None;
        }
        self.dismiss_hover_card(cx);
        cx.notify();
    }

    /// How tall a session row is: two lines under the Activity filter.
    pub(super) fn session_row_height(&self) -> f32 {
        if self.activity_filter {
            ACTIVITY_ROW_HEIGHT
        } else {
            SIDEBAR_NAV_ROW_HEIGHT
        }
    }

    /// The sessions the sidebar lists: the store's projection, narrowed by
    /// the typed filter and then by Activity.
    pub(super) fn visible_projection(
        &self,
        store: &mut crate::store::WindowWrite<'_>,
    ) -> Arc<crate::store::SidebarProjection> {
        let projection =
            super::super::filter::filter_projection(store.sidebar_projection(), self.filter_query.text());
        if !self.activity_filter {
            return projection;
        }
        let selected = store.selected_session_id().cloned();
        let unread = unread_sessions(store);
        let keep: HashSet<SessionId> = projection
            .projects
            .iter()
            .flat_map(|group| &group.active)
            .filter(|session| {
                // The session on screen stays listed, so looking at it (which
                // marks it seen) never pulls the row out from under you.
                selected.as_ref() == Some(&session.id)
                    || has_activity(row_state_with(
                        store,
                        session,
                        unread.contains(&session.id),
                    ))
            })
            .map(|session| session.id.clone())
            .collect();
        super::super::filter::activity_projection(projection, &keep)
    }

    /// The clock ages are read against.
    pub(super) fn age_now(&self) -> u64 {
        #[cfg(test)]
        if let Some(now) = self.age_now {
            return now;
        }
        wall_clock_millis().max(0.0) as u64
    }

    /// Notes the moment an age shown in this render next changes.
    pub(super) fn note_age_change(&mut self, at_ms: u64) {
        self.age_next = Some(self.age_next.map_or(at_ms, |next| next.min(at_ms)));
    }

    /// Arms the one wake that re-reads ages, for the earliest change noted
    /// while rendering. Re-arms only when that moment moved earlier; drops
    /// the wake when no age is shown.
    pub(super) fn schedule_age_wake(&mut self, cx: &mut Context<Self>) {
        let Some(next) = self.age_next.take().filter(|_| self.activity_filter) else {
            self.age_wake = None;
            return;
        };
        if self
            .age_wake
            .as_ref()
            .is_some_and(|(armed, _)| *armed <= next)
        {
            return;
        }
        let wait = next.saturating_sub(self.age_now());
        let task = cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(wait))
                .await;
            let _ = this.update(cx, |this, cx| {
                this.age_wake = None;
                // Only rows whose age text changed re-render: the rest have
                // equal props.
                this.notify_without_staling_rows(cx);
            });
        });
        self.age_wake = Some((next, task));
    }

    /// The Activity toggle at the end of the filter row. A dot marks it while
    /// it is off and some session waits on you or holds unseen news.
    pub(super) fn activity_toggle(
        &self,
        pending: bool,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let on = self.activity_filter;
        let hovered = self.ui.hovered_control == Some("sidebar-activity");
        let tooltip = crate::commands::command(CommandId::ToggleActivity)
            .shortcut_label()
            .map_or_else(
                || t("sidebar.activity").to_owned(),
                |shortcut| format!("{}  {shortcut}", t("sidebar.activity")),
            );
        div()
            .id("sidebar-activity")
            .debug_selector(|| "sidebar-activity".into())
            .role(Role::Button)
            .aria_label(t("sidebar.activity"))
            .relative()
            .size(px(26.0))
            .flex_none()
            .rounded(px(SIDEBAR_ROW_RADIUS))
            .flex()
            .items_center()
            .justify_center()
            .cursor_pointer()
            .when(on || hovered, |button| {
                button.bg(colors.primary.alpha(if on { 0.09 } else { 0.05 }))
            })
            .warm_tooltip(move |_, cx| {
                cx.new(|_| crate::palette_chrome::PaletteTooltip(tooltip.clone(), colors))
                    .into()
            })
            .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                this.ui.hovered_control = hovered.then_some("sidebar-activity");
                cx.notify();
            }))
            .on_click(cx.listener(|this, _, _, cx| {
                cx.stop_propagation();
                this.toggle_activity_filter(cx);
            }))
            .child(sf_symbol(
                "waveform.circle",
                13.0,
                if on { colors.primary } else { colors.secondary },
            ))
            .when(pending && !on, |button| {
                button.child(
                    div()
                        .debug_selector(|| "sidebar-activity-dot".into())
                        .absolute()
                        .top(px(4.0))
                        .right(px(4.0))
                        .size(px(6.0))
                        .rounded(px(3.0))
                        .bg(Ink::on_surface(Ink::ATTENTION, colors)),
                )
            })
            .into_any_element()
    }

    /// Whether any session waits on you or holds unseen news: the toggle's
    /// dot. One pass over the sessions and one over the inbox.
    pub(super) fn activity_pending(&self, store: &crate::store::SessionStore) -> bool {
        store.notifications().entries().iter().any(|entry| {
            !entry.read && entry.kind != crate::notification_feed::NotificationKind::Custom
        }) || store.sessions().values().any(|session| {
            !session.is_archived()
                && matches!(status_state(session, false), StatusState::NeedsInput { .. })
        })
    }
}

/// The state a session's row shows: its own, or for a split tab its most
/// urgent pane; finished-but-unread news reads as done-unseen.
pub(super) fn row_state(store: &crate::store::SessionStore, session: &SessionRecord) -> StatusState {
    row_state_with(store, session, store.notifications().session_unread(&session.id))
}

/// [`row_state`] with the session's unread flag already known.
fn row_state_with(
    store: &crate::store::SessionStore,
    session: &SessionRecord,
    unread: bool,
) -> StatusState {
    let migrating = store.migrating().contains(&session.id);
    let (_, shells) = store.pane_group(&session.id).unwrap_or_default();
    shells.iter().fold(
        sidebar_activity_state(status_state(session, migrating), unread),
        |state, shell| rows::most_urgent(state, status_state(shell, false)),
    )
}

/// Sessions with an unread inbox entry, in one pass over the inbox.
fn unread_sessions(store: &crate::store::SessionStore) -> HashSet<SessionId> {
    store
        .notifications()
        .entries()
        .iter()
        .filter(|entry| !entry.read)
        .map(|entry| entry.session_id.clone())
        .collect()
}

/// Whether a row belongs in the Activity filter.
pub(super) fn has_activity(state: StatusState) -> bool {
    matches!(
        state,
        StatusState::Working | StatusState::NeedsInput { .. } | StatusState::DoneUnseen
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_working_waiting_and_unseen_rows_count_as_activity() {
        assert!(has_activity(StatusState::Working));
        assert!(has_activity(StatusState::NeedsInput { destructive: false }));
        assert!(has_activity(StatusState::DoneUnseen));
        assert!(!has_activity(StatusState::IdleSeen));
        assert!(!has_activity(StatusState::None));
        assert!(!has_activity(StatusState::Hibernated));
    }
}
