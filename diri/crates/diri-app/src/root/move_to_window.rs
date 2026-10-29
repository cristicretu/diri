//! Moving a tab to another window: from the palette (next window, new
//! window) or by dragging its sidebar row or strip tab out of the window,
//! which opens a new window where it was let go.
//!
//! A window does not own sessions; every window reads the same catalog, and
//! windows showing the same project workspace show the same selected tab.
//! So a moved one-pane tab arrives *alone* (the window's All sessions view,
//! which each window keeps for itself), and the source simply shows its
//! next tab. A tab with split panes has to arrive as its workspace to keep
//! the panes; then the source leaves for a tab of another project, or
//! closes when there is none, since staying would mirror the destination.
//! The program in the tab keeps running throughout: nothing is respawned
//! or reattached beyond what showing a session in a window already does.
use super::*;
use crate::store::{WindowMode, WindowPlacement};
use diri_proto::workspace::{LayoutNode, WorkspaceId, WorkspaceSnapshot};
use gpui::{Bounds, Pixels, Point};

/// The workspace whose tabs show `session` in some pane, and whether that
/// tab has more than one pane.
pub(super) fn placement_of(
    snapshot: &WorkspaceSnapshot,
    session: &SessionId,
) -> Option<(WorkspaceId, bool)> {
    fn shows(node: &LayoutNode, session: &SessionId) -> bool {
        match node {
            LayoutNode::Pane { session_id, .. } => session_id == session,
            LayoutNode::Split { first, second, .. } => {
                shows(first, session) || shows(second, session)
            }
        }
    }
    snapshot.workspaces.iter().find_map(|workspace| {
        workspace
            .tabs
            .iter()
            .find(|tab| shows(&tab.layout, session))
            .map(|tab| (workspace.id.clone(), tab.layout.pane_count() > 1))
    })
}

/// What the source window shows once `moved` leaves it: the most recent
/// other tab for which `follows` is false (`follows` marks the tabs whose
/// selection would also change the destination). None closes the window.
pub(super) fn after_move(
    mru: &[SessionId],
    moved: &SessionId,
    is_tab: impl Fn(&SessionId) -> bool,
    follows: impl Fn(&SessionId) -> bool,
) -> Option<SessionId> {
    mru.iter()
        .find(|id| *id != moved && is_tab(id) && !follows(id))
        .cloned()
}

/// The frame of a window torn off at `drop` (global points): the source's
/// size, its title bar under the pointer, on the display that holds the
/// drop. `displays` pairs each display's uuid with its bounds.
pub(crate) fn tear_off_placement(
    source: &WindowPlacement,
    drop: Point<Pixels>,
    displays: &[(Option<String>, Bounds<Pixels>)],
) -> WindowPlacement {
    // Where the pointer lands in the new window: over its title bar, a
    // little in from the leading edge, as if it had been picked up there.
    const GRAB: (f32, f32) = (160.0, 22.0);
    let display_uuid = displays
        .iter()
        .find(|(_, bounds)| bounds.contains(&drop))
        .map(|(uuid, _)| uuid.clone())
        .unwrap_or_else(|| source.display_uuid.clone());
    WindowPlacement {
        display_uuid,
        mode: WindowMode::Windowed,
        x: f32::from(drop.x) - GRAB.0,
        y: f32::from(drop.y) - GRAB.1,
        width: source.width,
        height: source.height,
    }
}

/// How a moved tab arrives: its split workspace (`Some`), or alone.
type Arrival = Option<WorkspaceId>;

impl RootView {
    /// The tab this window shows, as its sidebar row: a focused pane shell
    /// stands for the agent that owns its tab.
    pub(super) fn shown_tab_session(&self) -> Option<SessionId> {
        let store = self.window_store.read().expect("store");
        let session = store.selected_session_id()?;
        Some(store.pane_owner(session))
    }

    /// Where `moved` goes and what this window shows instead (None: close).
    fn plan_move(&self, moved: &SessionId) -> (Arrival, Option<SessionId>) {
        let mut store = self.window_store.write().expect("store");
        let mru = store.mru_sessions();
        let snapshot = store.workspace_catalog().snapshot().cloned();
        let place = |id: &SessionId| {
            snapshot
                .as_ref()
                .and_then(|snapshot| placement_of(snapshot, id))
        };
        let arrival = place(moved).and_then(|(workspace, split)| split.then_some(workspace));
        let project = |id: &SessionId| store.sessions().get(id).map(|s| s.project_id.clone());
        let next = after_move(
            &mru,
            moved,
            |id| store.pane_owner(id) == *id,
            |id| {
                // Only a workspace arrival is mirrored: by its own tabs, and
                // by any tab of its project, which opens into it.
                arrival.as_ref().is_some_and(|home| {
                    place(id).is_some_and(|(workspace, _)| &workspace == home)
                        || project(id) == project(moved)
                })
            },
        );
        (arrival, next)
    }

    /// Show `session` alone, in this window's All sessions view.
    fn show_alone(&mut self, session: SessionId, window: &mut Window, cx: &mut Context<Self>) {
        self.window_store.write().expect("store").select(session);
        self.sidebar
            .update(cx, |sidebar, cx| sidebar.activate_workspace(None, cx));
        self.activate_saved_workspace(None, window, cx);
        self.services.store.publish_local_change();
        cx.notify();
    }

    fn arrive(
        &mut self,
        session: SessionId,
        arrival: Arrival,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.activate_window();
        match arrival {
            Some(_) => self.open_workspace_launch_session(session, window, cx),
            None => self.show_alone(session, window, cx),
        }
    }

    fn leave(&mut self, next: Option<SessionId>, window: &mut Window, cx: &mut Context<Self>) {
        match next {
            Some(next) => self.open_workspace_launch_session(next, window, cx),
            None => window.remove_window(),
        }
    }

    /// Palette: move the shown tab into a new window beside this one.
    pub(super) fn move_tab_to_new_window(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut placement = crate::current_window_placement(window, cx);
        placement.x += 28.0;
        placement.y += 28.0;
        placement.mode = WindowMode::Windowed;
        if let Some(session) = self.shown_tab_session() {
            self.move_tab_into_new_window(session, placement, window, cx);
        }
    }

    /// Palette: move the shown tab into the next window, or a new one when
    /// this is the only window.
    pub(super) fn move_tab_to_next_window(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self.shown_tab_session() else {
            return;
        };
        let own = window.window_handle().window_id();
        let windows: Vec<_> = cx
            .windows()
            .into_iter()
            .filter_map(|handle| handle.downcast::<RootView>())
            .collect();
        let next = windows
            .iter()
            .position(|handle| handle.window_id() == own)
            .filter(|_| windows.len() > 1)
            .map(|index| windows[(index + 1) % windows.len()]);
        let Some(target) = next else {
            return self.move_tab_to_new_window(window, cx);
        };
        let (arrival, after) = self.plan_move(&session);
        self.leave(after, window, cx);
        cx.defer(move |cx| {
            let _ = target.update(cx, |root, window, cx| {
                root.arrive(session, arrival, window, cx)
            });
        });
    }

    /// Opens a window at `placement` showing `session`, and makes this
    /// window leave it.
    pub(super) fn move_tab_into_new_window(
        &mut self,
        session: SessionId,
        placement: WindowPlacement,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (arrival, after) = self.plan_move(&session);
        let context = crate::NativeWindowContext {
            workspace: arrival.clone(),
            selected: Some(session.clone()),
            placement,
            policy: crate::window_restore::RestorePolicy::FRAME_ONLY,
        };
        let (services, preview, scenario) =
            (self.services.clone(), self.preview, self.preview_scenario);
        // Opened after this update returns: a window cannot open inside
        // another window's update.
        cx.defer(move |cx| {
            let opened =
                crate::open_main_window_with_context(cx, services, preview, scenario, context);
            let _ = opened.update(cx, |root, window, cx| {
                root.arrive(session, arrival, window, cx)
            });
        });
        self.leave(after, window, cx);
    }

    /// A sidebar row or strip tab is being carried: remember which tab, so
    /// a release outside the window can tear it off.
    pub(super) fn carry_tab(&mut self, session: Option<SessionId>) {
        self.carried_tab = session;
    }

    /// The pointer let go outside this window. A carried tab opens in a new
    /// window there; anything else was not a tab and is ignored.
    pub(super) fn release_outside(
        &mut self,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.carried_tab.take() else {
            return;
        };
        let viewport = window.viewport_size();
        let inside = position.x >= px(0.0)
            && position.y >= px(0.0)
            && position.x < viewport.width
            && position.y < viewport.height;
        if inside || !self.window_store.read().expect("store").sessions().contains_key(&session)
        {
            return;
        }
        let drop = window.bounds().origin + position;
        let displays: Vec<_> = cx
            .displays()
            .iter()
            .map(|display| {
                (
                    display.uuid().ok().map(|uuid| uuid.to_string()),
                    display.bounds(),
                )
            })
            .collect();
        let placement =
            tear_off_placement(&crate::current_window_placement(window, cx), drop, &displays);
        self.move_tab_into_new_window(session, placement, window, cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use diri_proto::workspace::{PaneId, TabId, WorkspaceRecord, WorkspaceTab};
    use gpui::{point, size};

    fn id(value: &str) -> SessionId {
        SessionId::new(value)
    }

    fn snapshot() -> WorkspaceSnapshot {
        let tab = |tab: &str, session: &str| WorkspaceTab {
            id: TabId::new(tab),
            title: None,
            focused_pane: PaneId::new(tab),
            zoomed_pane: None,
            layout: LayoutNode::Pane {
                id: PaneId::new(tab),
                session_id: id(session),
            },
        };
        let workspace = |name: &str, tabs| WorkspaceRecord {
            project_id: None,
            id: WorkspaceId::new(name),
            name: name.into(),
            selected_tab: None,
            tabs,
        };
        WorkspaceSnapshot {
            revision: 1,
            workspaces: vec![
                workspace("diri", vec![tab("a", "claude"), tab("b", "codex")]),
                workspace("site", vec![tab("c", "site")]),
            ],
            ..Default::default()
        }
    }

    #[test]
    fn a_one_pane_tab_arrives_alone_and_a_split_tab_brings_its_workspace() {
        let mut snapshot = snapshot();
        assert_eq!(
            placement_of(&snapshot, &id("claude")),
            Some((WorkspaceId::new("diri"), false))
        );
        assert_eq!(placement_of(&snapshot, &id("loose")), None);
        snapshot.workspaces[1].tabs[0].layout = LayoutNode::Split {
            id: diri_proto::workspace::SplitId::new("s"),
            axis: diri_proto::workspace::LayoutAxis::Horizontal,
            fraction: 0.5,
            first: Box::new(LayoutNode::Pane {
                id: PaneId::new("c"),
                session_id: id("site"),
            }),
            second: Box::new(LayoutNode::Pane {
                id: PaneId::new("d"),
                session_id: id("site-shell"),
            }),
        };
        assert_eq!(
            placement_of(&snapshot, &id("site")),
            Some((WorkspaceId::new("site"), true))
        );
    }

    #[test]
    fn the_source_shows_the_most_recent_tab_the_destination_does_not_mirror() {
        let mru = [id("claude"), id("codex"), id("site"), id("loose")];
        let tab = |_: &SessionId| true;
        // Arriving alone: nothing is mirrored, the next recent tab it is.
        assert_eq!(
            after_move(&mru, &id("claude"), tab, |_| false),
            Some(id("codex"))
        );
        // Arriving as the `diri` workspace: Codex would be mirrored.
        assert_eq!(
            after_move(&mru, &id("claude"), tab, |s: &SessionId| s == &id("codex")),
            Some(id("site"))
        );
        // Pane shells are not tabs.
        assert_eq!(
            after_move(&mru, &id("site"), |s: &SessionId| s != &id("claude"), |_| false),
            Some(id("codex"))
        );
        // Nothing left: the window closes.
        assert_eq!(
            after_move(&[id("claude"), id("codex")], &id("claude"), tab, |_| true),
            None
        );
    }

    #[test]
    fn a_torn_off_window_opens_under_the_pointer_on_the_display_it_was_dropped_on() {
        let source = WindowPlacement {
            display_uuid: Some("built-in".into()),
            mode: WindowMode::Maximized,
            x: 0.0,
            y: 25.0,
            width: 1100.0,
            height: 700.0,
        };
        let displays = [
            (
                Some("built-in".to_owned()),
                Bounds::new(point(px(0.0), px(0.0)), size(px(1512.0), px(982.0))),
            ),
            (
                Some("studio".to_owned()),
                Bounds::new(point(px(1512.0), px(0.0)), size(px(2560.0), px(1440.0))),
            ),
        ];
        let placed = tear_off_placement(&source, point(px(2000.0), px(400.0)), &displays);
        assert_eq!(placed.display_uuid.as_deref(), Some("studio"));
        assert_eq!((placed.x, placed.y), (1840.0, 378.0));
        assert_eq!((placed.width, placed.height), (1100.0, 700.0));
        assert_eq!(placed.mode, WindowMode::Windowed);
        // Off every display: the source's display, clamped on open.
        let placed = tear_off_placement(&source, point(px(-300.0), px(-50.0)), &displays);
        assert_eq!(placed.display_uuid.as_deref(), Some("built-in"));
    }
}
