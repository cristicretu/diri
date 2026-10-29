//! Horizontal strip session tabs as cached child views.
//!
//! `RootView` paints the strip inline, so every window frame rebuilt every
//! tab: each terminal output frame, each working mark's tick, each store
//! publication. Each tab is now a cached view (`StripTabView`) that renders
//! from a [`StripTabProps`] snapshot the strip computes while it renders,
//! the same pattern as the sidebar rows (`rows.rs`). A tab re-renders only
//! when:
//!
//! - its props changed (selection, activity mark or its frame, title, kind,
//!   theme, held-⌘ hint, ordering mode, hover-out fade, whether the shared
//!   selection pill stands in for its fill);
//! - it is lifted or sliding in a drag reorder, or its title is settling;
//! - it was notified itself (its own hover, an animation frame it requested);
//! - the sidebar was notified for anything other than a store publication or
//!   one of its own animation ticks (`tabs_stale`, a safety net for inputs the
//!   props might miss).
//!
//! The selection pill (`TabPill`) is a separate layer the strip draws inline
//! beside the tabs, so its glide keeps moving on reused tabs; its frames
//! notify the sidebar without staling them.
//!
//! GPUI's cache key still re-renders a tab whose bounds or clip changed, such
//! as every tab while the strip scrolls or slides in. A drag refreshes the
//! window on every pointer move, which renders every tab.

use super::tabs::{TAB_HEIGHT, TAB_WIDTH};
use super::*;

/// Everything `Sidebar::strip_tab` reads. Equal props render an identical
/// tab, apart from the title settle, the lift and the reorder slide, which
/// force a render while active.
#[derive(Clone, PartialEq)]
pub(in crate::sidebar) struct StripTabProps {
    pub(super) id: SessionId,
    pub(super) title: String,
    pub(super) kind: ProtoAgentKind,
    pub(super) active: bool,
    /// ⌘1–⌘8, then ⌘9 for the last tab.
    pub(super) rank: Option<usize>,
    pub(super) state: StatusState,
    /// Terminal progress, when the session reports any.
    pub(super) progress: Option<crate::progress_mark::ProgressFace>,
    pub(super) colors: SemanticColors,
    pub(super) custom_ordering: bool,
    /// The strip's shared pill layer draws the selection this frame, so the
    /// selected tab leaves its own fill off (see `TabPill`).
    pub(super) pill_drawn: bool,
    /// The held-⌘ hint's opacity, for tabs that show a hint.
    pub(super) held_hint: f32,
    /// The lifted tab's offset from its slot.
    pub(super) lift: Option<Pixels>,
    /// A displaced tab's slide: its starting offset and the reorder it
    /// belongs to.
    pub(super) shift: Option<(f32, u64)>,
    pub(super) settling: bool,
    /// A terminal's directory, shown when the pointer rests on its tab.
    pub(super) location: Option<String>,
    /// The tab's layout with each pane's state, when it is split.
    pub(super) split: Option<super::split_mark::SplitMark>,
    /// The fraction of the hover fill a tab the pointer just left still
    /// wears; it changes each frame of the fade (see `hover_linger`).
    pub(super) hover_linger: f32,
    /// The selection and hover look for the window's key status.
    pub(super) look: crate::chrome_focus::ChromeLook,
}

impl StripTabProps {
    /// Drawn from something other than its props every frame.
    fn moving(&self) -> bool {
        self.lift.is_some() || self.shift.is_some() || self.settling
    }
}

pub(in crate::sidebar) struct StripTabView {
    sidebar: WeakEntity<Sidebar>,
    props: StripTabProps,
    #[cfg(test)]
    pub(super) renders: usize,
}

#[cfg(test)]
impl StripTabView {
    pub(super) fn props_for_test(&self) -> &StripTabProps {
        &self.props
    }
}

impl Render for StripTabView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        #[cfg(test)]
        let started = std::time::Instant::now();
        #[cfg(test)]
        {
            self.renders += 1;
        }
        let props = self.props.clone();
        let tab = self
            .sidebar
            .update(cx, |sidebar, cx| sidebar.strip_tab(&props, cx))
            .unwrap_or_else(|_| gpui::Empty.into_any_element());
        #[cfg(test)]
        render_probe::strip_time(started.elapsed());
        div().size_full().child(tab)
    }
}

impl Sidebar {
    /// The props for the strip's `index`th of `count` tabs.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn strip_tab_props(
        &self,
        session: &SessionRecord,
        index: usize,
        count: usize,
        active: bool,
        marks: super::tabs::TabMarks,
        colors: SemanticColors,
        custom_ordering: bool,
        reduce_motion: bool,
        pill_drawn: bool,
    ) -> StripTabProps {
        let id = &session.id;
        let super::tabs::TabMarks {
            state,
            asking,
            split,
        } = marks;
        // The pane that asks names the question.
        let asking = asking.as_deref().unwrap_or(session);
        // The same rule the sidebar rows follow: ⌘1–⌘8, then ⌘9 = last.
        let rank = if index < 8 {
            Some(index + 1)
        } else {
            (index + 1 == count).then_some(9)
        };
        let lift = self.lift_offset(&LiftKey::SessionTab(id.clone()));
        // The lifted tab never slides: it is drawn from the pointer, and its
        // slot simply moves.
        let shift = if reduce_motion || lift.is_some() {
            None
        } else {
            self.tab_shift
                .deltas
                .get(id)
                .map(|delta| (*delta, self.tab_shift.generation))
        };
        StripTabProps {
            id: id.clone(),
            // A tab waiting on a named question shows the question.
            title: crate::session_presentation::blocker_line(asking)
                .unwrap_or_else(|| display_title(session)),
            kind: session.effective_kind().clone(),
            active,
            rank,
            state,
            progress: crate::progress_mark::face(session, self.activity_frame, reduce_motion),
            colors,
            custom_ordering,
            // Only the selected tab paints a fill the pill can replace.
            pill_drawn: active && pill_drawn,
            // Only a ranked tab's mark and the selected tab's ✕ carry a hint.
            held_hint: if rank.is_some() || active {
                self.strip_held_hint
            } else {
                0.0
            },
            lift,
            shift,
            settling: self.settling_title(id, 0.0).is_some(),
            location: crate::switcher::terminal_location(session),
            split,
            hover_linger: if active {
                0.0
            } else {
                self.tab_hover_linger(id)
            },
            look: self.chrome_look,
        }
    }

    /// One strip tab, mounted as its cached view.
    pub(super) fn mount_strip_tab(
        &mut self,
        props: StripTabProps,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // An indeterminate progress sweep rides the sidebar's tick. A working
        // mark needs no tick: the window advances its frames by itself.
        self.working_row_rendered |= props.progress.is_some_and(|face| face.animates());
        if props.shift.is_none() {
            self.tab_shift.applied.borrow_mut().remove(&props.id);
        }
        let force = self.tabs_stale || props.moving();
        let sidebar = self.weak_self.clone();
        let mut created = false;
        let view = self
            .strip_tab_views
            .entry(props.id.clone())
            .or_insert_with(|| {
                created = true;
                cx.new(|_| StripTabView {
                    sidebar,
                    props: props.clone(),
                    #[cfg(test)]
                    renders: 0,
                })
            })
            .clone();
        // Set without notifying: a notify while drawing lands a frame late.
        // `force_render_if` re-renders the tab in this frame instead.
        let changed = !created
            && view.update(cx, |view, _| {
                let changed = view.props != props;
                if changed {
                    view.props = props;
                }
                changed
            });
        view.cached(
            gpui::StyleRefinement::default()
                .flex_none()
                .w(px(TAB_WIDTH))
                .h(px(TAB_HEIGHT)),
        )
        .force_render_if(force || changed)
        .into_any_element()
    }
}
