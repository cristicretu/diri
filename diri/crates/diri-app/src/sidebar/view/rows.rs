//! Session rows as cached child views of the sidebar.
//!
//! The sidebar re-renders whenever any of its rows or chrome changes. Before
//! this, it rebuilt, laid out and painted every row each time, and a working
//! row's 8 Hz activity mark alone did that constantly. (Working marks no
//! longer notify at all: the window swaps their pre-rasterized frames in the
//! drawn scene, see `diri_ui::FrameLoop`.) Each row is now a cached
//! view (`SessionRowView`) that renders from a [`SessionRowProps`] snapshot
//! the sidebar computes while it renders. A row re-renders only when:
//!
//! - its props changed;
//! - it is renaming, its title is settling, or it is growing in or collapsing
//!   out (`row_motion`);
//! - it is fading out of hover (`hover_linger`, a prop that changes each
//!   frame of the fade);
//! - it was notified itself (the activity tick notifies only working rows);
//! - the sidebar was notified for anything other than a store publication or
//!   one of its own animation ticks (`rows_stale`, a safety net for inputs the
//!   props might miss).
//!
//! GPUI's cache key still re-renders a row whose bounds, clip or opacity
//! changed, such as rows below one that grows in, or rows under a clip that
//! resizes.
//!
//! Every other row is reused by the vendored GPUI's nested view cache (see
//! `vendor/gpui/DIRI_PATCHES.md`), including the opacity the list wraps
//! around it.
//!
//! Rows far from the viewport are not mounted at all: the list walks them
//! and lays out a spacer in their place (`row_window.rs`). Their views are
//! kept while they stay in the list, so scrolling back reuses them.

use super::*;

/// Everything `Sidebar::session_row` reads. Equal props render an identical
/// row, apart from the title settle and rename editor, which force a render
/// while active.
#[derive(Clone, PartialEq)]
pub(in crate::sidebar) struct SessionRowProps {
    pub(super) row: crate::store::SidebarRow,
    pub(super) shortcut: Option<usize>,
    pub(super) drop: Option<RowDrop>,
    pub(super) host_marked_above: bool,
    pub(super) colors: SemanticColors,
    pub(super) selected: bool,
    pub(super) multi: bool,
    pub(super) drag_selection: Option<Vec<SessionId>>,
    pub(super) migrating: bool,
    pub(super) activity_state: StatusState,
    /// The question or permission the row waits on
    /// (`session_presentation::blocker_line`), as a second line under its
    /// title, and how far that line is open.
    pub(super) question: Option<super::question_lines::QuestionLine>,
    /// The row's natural height: taller while it shows a second line.
    pub(super) height: f32,
    /// Under the Activity filter, the second line: the state and its age.
    pub(super) activity: Option<(String, Option<String>)>,
    /// Panes in this row's tab, when it is split.
    pub(super) panes: usize,
    /// Terminal progress, when the session reports any.
    pub(super) progress: Option<crate::progress_mark::ProgressFace>,
    pub(super) marked: bool,
    pub(super) hovered: bool,
    pub(super) focused: bool,
    pub(super) lineage: Option<LineageRole>,
    pub(super) width: f32,
    pub(super) filter: String,
    pub(super) renaming: bool,
    pub(super) settling: bool,
    /// The held-⌘ shortcut hint's opacity, for rows that have a shortcut.
    pub(super) held_hint: f32,
    /// How much of the hover fill a row the pointer just left still wears.
    /// It changes every frame of the fade, so only fading rows re-render.
    pub(super) hover_linger: f32,
    /// The selection and hover look for the window's key status.
    pub(super) look: crate::chrome_focus::ChromeLook,
}

pub(in crate::sidebar) struct SessionRowView {
    sidebar: WeakEntity<Sidebar>,
    pub(super) props: SessionRowProps,
    #[cfg(all(test, target_os = "macos"))]
    pub(super) renders: usize,
}

#[cfg(all(test, target_os = "macos"))]
impl SessionRowView {
    pub(super) fn held_hint_for_test(&self) -> f32 {
        self.props.held_hint
    }

}

// The window-key test that reads these compiles on every platform (Linux and
// Windows CI run `clippy --all-targets`), so they must not be macOS-only.
#[cfg(test)]
impl SessionRowView {
    pub(super) fn look_for_test(&self) -> crate::chrome_focus::ChromeLook {
        self.props.look
    }

    pub(super) fn selected_for_test(&self) -> bool {
        self.props.selected
    }
}

impl Render for SessionRowView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::perf_overlay::rendered("sidebar row");
        #[cfg(all(test, target_os = "macos"))]
        {
            self.renders += 1;
        }
        let props = self.props.clone();
        let row = self
            .sidebar
            .update(cx, |sidebar, cx| sidebar.session_row(&props, window, cx))
            .unwrap_or_else(|_| gpui::Empty.into_any_element());
        div().size_full().child(row)
    }
}

impl Sidebar {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn session_row_props(
        &mut self,
        row: &crate::store::SidebarRow,
        shortcut: Option<usize>,
        drop: Option<RowDrop>,
        host_marked_above: bool,
        colors: SemanticColors,
        (question, height): (Option<super::question_lines::QuestionLine>, f32),
        reduce_motion: bool,
        window: &Window,
    ) -> SessionRowProps {
        let session = &row.session;
        let id = &session.id;
        let (selected, multi, drag_selection, migrating, activity_state, pane_group, unseen) = {
            let mut store = self.store.write().expect("session store lock poisoned");
            (
                // While the To-dos page covers the workbench, no row is the
                // one on screen.
                store.selected_session_id() == Some(id) && !self.todos_active,
                store.sidebar_selection().contains(id),
                (store.sidebar_selection().len() > 1).then(|| store.sidebar_selection_ordered()),
                store.migrating().contains(id),
                // One row per tab: it shows its most urgent pane.
                super::activity::row_state(&store, session),
                store.pane_group(id),
                self.activity_filter
                    .then(|| store.notifications().latest_unread(id).cloned())
                    .flatten(),
            )
        };
        let (panes, shells) = pane_group.unwrap_or_default();
        // The pane that makes the row ask: the row's own session, unless a
        // split pane is the one waiting.
        let asking = super::question_lines::asking_pane(session, &shells);
        let activity = self.activity_filter.then(|| {
            let line = crate::session_presentation::activity_line(
                asking,
                activity_state,
                unseen.as_ref(),
            );
            let age = line.since_ms.map(|since| {
                let (label, next) = crate::session_presentation::age_label(since, self.age_now());
                self.note_age_change(next);
                label
            });
            (line.text, age)
        });
        SessionRowProps {
            row: row.clone(),
            shortcut,
            drop,
            host_marked_above,
            colors,
            selected,
            multi,
            drag_selection,
            migrating,
            activity_state,
            question,
            height,
            activity,
            panes,
            progress: crate::progress_mark::face(session, self.activity_frame, reduce_motion),
            marked: self.ui.delegation_mark.as_ref() == Some(id),
            hovered: self.ui.hovered_session.as_ref() == Some(id),
            focused: self.focus_handle.is_focused(window)
                && self.ui.renaming.is_none()
                && self.ui.focus_cursor.as_ref() == Some(id),
            lineage: self.lineage_roles.get(id).copied(),
            width: self.ui.width,
            filter: self.filter_query.text().to_owned(),
            renaming: self.ui.renaming.as_ref() == Some(id),
            settling: self.settling_title(id, 0.0).is_some(),
            held_hint: if shortcut.is_some() {
                self.row_held_hint
            } else {
                0.0
            },
            hover_linger: self.session_hover_linger(id),
            look: self.chrome_look,
        }
    }

    /// One session row, mounted as its cached view.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn mount_session_row(
        &mut self,
        row: &crate::store::SidebarRow,
        shortcut: Option<usize>,
        drop: Option<RowDrop>,
        host_marked_above: bool,
        colors: SemanticColors,
        question: (Option<super::question_lines::QuestionLine>, f32),
        moving: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.mounted_row_ids.insert(row.id().clone());
        let height = question.1;
        let props = self.session_row_props(
            row,
            shortcut,
            drop,
            host_marked_above,
            colors,
            question,
            cx.reduce_motion(),
            window,
        );
        // An indeterminate progress sweep rides the sidebar's tick. A working
        // mark needs no tick: the window advances its frames by itself.
        let working = props.progress.is_some_and(|face| face.animates());
        self.working_row_rendered |= working;
        // A row growing in or collapsing out renders every frame of its
        // motion, like a settling title.
        let force = self.rows_stale || props.renaming || props.settling || moving;
        let sidebar = self.weak_self.clone();
        let mut created = false;
        let view = self
            .session_row_views
            .entry(row.id().clone())
            .or_insert_with(|| {
                created = true;
                cx.new(|_| SessionRowView {
                    sidebar,
                    props: props.clone(),
                    #[cfg(all(test, target_os = "macos"))]
                    renders: 0,
                })
            })
            .clone();
        // Set without notifying: a notify while drawing lands a frame late.
        // `force_render_if` re-renders the row in this frame instead.
        let changed = !created
            && view.update(cx, |view, _| {
                let changed = view.props != props;
                if changed {
                    view.props = props;
                }
                changed
            });
        if working {
            self.animated_rows.push(view.downgrade());
        }

        view.cached(
            gpui::StyleRefinement::default()
                .w_full()
                .h(px(height))
                .flex_none(),
        )
        .force_render_if(force || changed)
        .into_any_element()
    }

    /// Advances indeterminate progress sweeps. While session rows are on
    /// screen, only the sweeping rows are notified: the sidebar re-renders as
    /// their ancestor, hands them their next frame, and reuses every other
    /// row. The
    /// horizontal strip, painted by `RootView`, still needs the sidebar
    /// notified; its tabs take the new frame through their props, so the
    /// notify does not stale them.
    pub(super) fn notify_activity_frame(&mut self, cx: &mut Context<Self>) {
        let mut notified = false;
        if self.rows_mounted {
            for row in &self.animated_rows {
                notified |= row.update(cx, |_, cx| cx.notify()).is_ok();
            }
        }
        if !notified {
            self.notify_without_staling_rows(cx);
        }
    }

    /// A notify whose effect on rows is fully captured by their props (store
    /// publications and the sidebar's own animation ticks), so it does not
    /// mark every row stale.
    pub(super) fn notify_without_staling_rows(&mut self, cx: &mut Context<Self>) {
        self.notify_keeps_rows = true;
        cx.notify();
    }

    /// Any other notify may have changed something a row or strip tab reads
    /// outside its props, so every row and tab renders once more.
    pub(super) fn note_self_notified(&mut self) {
        if std::mem::take(&mut self.notify_keeps_rows) {
            return;
        }
        self.rows_stale = true;
        self.tabs_stale = true;
    }
}

/// The state a split tab's row shows: needs you, then working, then
/// finished-unseen; otherwise the row's own.
pub(super) fn most_urgent(row: StatusState, pane: StatusState) -> StatusState {
    fn rank(state: StatusState) -> u8 {
        match state {
            StatusState::NeedsInput { .. } => 3,
            StatusState::Working => 2,
            StatusState::DoneUnseen => 1,
            _ => 0,
        }
    }
    if rank(pane) > rank(row) { pane } else { row }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_split_row_shows_its_most_urgent_pane() {
        let ask = StatusState::NeedsInput { destructive: false };
        assert_eq!(most_urgent(StatusState::Working, ask), ask);
        assert_eq!(
            most_urgent(StatusState::IdleSeen, StatusState::Working),
            StatusState::Working
        );
        assert_eq!(most_urgent(ask, StatusState::Working), ask);
        // A quiet shell never hides the row's own state.
        assert_eq!(
            most_urgent(StatusState::DoneUnseen, StatusState::None),
            StatusState::DoneUnseen
        );
        assert_eq!(
            most_urgent(StatusState::Hibernated, StatusState::IdleSeen),
            StatusState::Hibernated
        );
    }
}
