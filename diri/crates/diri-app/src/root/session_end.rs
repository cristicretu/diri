//! When the session in the focused workspace pane ends, the window shows the
//! next session in the same frame. The Engine's layout catches up behind it:
//! the session's removal and the tab switch are separate round trips, and
//! waiting for either leaves an ended or empty pane on screen in between.
use super::*;
use crate::store::{SessionStore, WindowWrite};
use diri_proto::ExitReason;
use diri_proto::workspace::{
    LayoutNode, PaneId, TabId, WorkspaceId, WorkspaceMutation, WorkspaceSnapshot, WorkspaceTab,
};

/// A tab this window shows ahead of the Engine. The edit that selects it is
/// sent from here, and the override ends once the snapshot agrees, the
/// placement is gone, or the Engine decided otherwise.
pub(super) struct PendingTab {
    placement: Placement,
    /// The catalog revision the confirming edit was sent against.
    sent: Option<u64>,
    attempts: u8,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct Placement {
    pub(super) workspace: WorkspaceId,
    pub(super) tab: TabId,
    pub(super) pane: PaneId,
    pub(super) session: SessionId,
}

/// Engine conflicts are retried; a layout that keeps moving elsewhere wins.
const CONFIRM_ATTEMPTS: u8 = 3;

fn panes(node: &LayoutNode, output: &mut Vec<(PaneId, SessionId)>) {
    match node {
        LayoutNode::Pane { id, session_id } => output.push((id.clone(), session_id.clone())),
        LayoutNode::Split { first, second, .. } => {
            panes(first, output);
            panes(second, output);
        }
    }
}

/// Where the window goes when `ended` leaves the focused pane of
/// `current_tab`: a live pane of the same tab, else a live tab of the same
/// workspace, else one in another workspace, the most recently used first.
pub(super) fn survivor(
    snapshot: &WorkspaceSnapshot,
    active: &WorkspaceId,
    current_tab: &TabId,
    ended: &SessionId,
    mru: &[SessionId],
    runs: impl Fn(&SessionId) -> bool,
) -> Option<Placement> {
    let mut best: Option<((u8, usize, usize), Placement)> = None;
    let mut order = 0;
    for workspace in &snapshot.workspaces {
        for tab in &workspace.tabs {
            let mut placed = Vec::new();
            panes(&tab.layout, &mut placed);
            for (pane, session) in placed {
                order += 1;
                if &session == ended || !runs(&session) {
                    continue;
                }
                let tier = if &workspace.id != active {
                    2
                } else if &tab.id != current_tab {
                    1
                } else {
                    0
                };
                let recency = mru
                    .iter()
                    .position(|id| id == &session)
                    .unwrap_or(usize::MAX);
                let rank = (tier, recency, order);
                if best.as_ref().is_none_or(|(current, _)| rank < *current) {
                    best = Some((
                        rank,
                        Placement {
                            workspace: workspace.id.clone(),
                            tab: tab.id.clone(),
                            pane,
                            session,
                        },
                    ));
                }
            }
        }
    }
    best.map(|(_, placement)| placement)
}

/// The process behind the session is still running: open, not closing, and
/// not exited.
fn runs(store: &SessionStore, id: &SessionId) -> bool {
    store.is_open(id)
        && store
            .sessions()
            .get(id)
            .is_some_and(|session| !matches!(session.status, SessionStatus::Exited(_)))
}

/// The session on screen stopped: closed (a clean `exit` closes it), removed
/// elsewhere, or its process exited or was killed. Not an archive, a daemon
/// restart that resumes it, or a list that has not loaded yet.
fn ended(store: &SessionStore, id: &SessionId) -> bool {
    match store.sessions().get(id) {
        None => store.has_hydrated_sessions(),
        Some(_) if !store.is_open(id) => true,
        Some(session) => matches!(
            &session.status,
            SessionStatus::Exited(info)
                if matches!(info.reason, ExitReason::Exited | ExitReason::Signaled)
        ),
    }
}

/// Said once the window has moved away from a session that stays listed
/// because it did not end cleanly, so the failure is not missed.
fn unclean_exit_notice(session: &SessionRecord) -> Option<crate::toast::Toast> {
    let SessionStatus::Exited(info) = &session.status else {
        return None;
    };
    let how = match (info.reason, info.code, info.signal) {
        (ExitReason::Exited, Some(0), _) => return None,
        (ExitReason::Exited, Some(code), _) => format!("exited with code {code}"),
        (ExitReason::Signaled, _, Some(signal)) => format!("was stopped by signal {signal}"),
        (ExitReason::Signaled, _, None) => "was stopped".to_owned(),
        _ => return None,
    };
    // The title goes on the second line: a long one in the sentence wraps it
    // and strands the exit code on a line of its own.
    Some(
        crate::toast::Toast::warning(format!("Session {how}.")).detail(format!(
            "“{}” stays in the sidebar with its last screen.",
            crate::switcher::display_title(session)
        )),
    )
}

impl RootView {
    /// The tab the workbench shows: the Engine's selected tab, or the
    /// survivor this window moved to while the Engine catches up. Moves on
    /// when the session the focused pane was running has just ended.
    pub(super) fn workspace_tab_to_show(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<WorkspaceTab> {
        let active = self.active_workspace.clone()?;
        let on_screen = self
            .workspace_workbench
            .as_ref()
            .and_then(|workbench| workbench.read(cx).focused_session_id());
        let window_store = self.window_store.clone();
        let just_ended = {
            let store = window_store.read().expect("store");
            let just_ended = self
                .live_on_screen
                .take()
                .filter(|live| on_screen.as_ref() == Some(live) && ended(&store, live));
            // Every frame takes this path: read only, unless there is
            // something to move to or to confirm.
            if just_ended.is_none() && self.pending_tab.is_none() {
                let tab = self.shown_tab(&store, &active)?;
                self.live_on_screen = focused_pane_session(&tab).filter(|id| runs(&store, id));
                return Some(tab);
            }
            just_ended
        };
        let mut store = window_store.write().expect("store");
        self.settle_pending_tab(&mut store, &active);
        let mut tab = self.shown_tab(&store, &active)?;
        if let Some(ended_id) = just_ended {
            let mru = store.mru_sessions();
            let placement = store.workspace_catalog().snapshot().and_then(|snapshot| {
                survivor(snapshot, &active, &tab.id, &ended_id, &mru, |id| {
                    runs(&store, id)
                })
            });
            let notice = store
                .sessions()
                .get(&ended_id)
                .filter(|_| store.is_open(&ended_id))
                .and_then(|session| unclean_exit_notice(session));
            match placement {
                Some(placement) if placement.workspace == active => {
                    self.pending_tab = Some(PendingTab {
                        placement,
                        sent: None,
                        attempts: 0,
                    });
                    self.settle_pending_tab(&mut store, &active);
                    tab = self.shown_tab(&store, &active)?;
                }
                Some(placement) => {
                    let workspace = placement.workspace.clone();
                    self.pending_tab = Some(PendingTab {
                        placement,
                        sent: None,
                        attempts: 0,
                    });
                    let sidebar = self.sidebar.clone();
                    cx.defer_in(window, move |_, _, cx| {
                        sidebar.update(cx, |sidebar, cx| {
                            sidebar.activate_workspace(Some(workspace), cx)
                        });
                    });
                }
                None => {
                    let sidebar = self.sidebar.clone();
                    cx.defer_in(window, move |_, _, cx| {
                        sidebar.update(cx, |sidebar, cx| sidebar.leave_layout_for_survivor(cx));
                    });
                }
            }
            if let Some(notice) = notice {
                cx.defer_in(window, move |this, _, cx| {
                    this.show_feedback("session_ended", notice, cx);
                });
            }
        }
        self.live_on_screen = focused_pane_session(&tab).filter(|id| runs(&store, id));
        Some(tab)
    }

    fn shown_tab(&self, store: &SessionStore, active: &WorkspaceId) -> Option<WorkspaceTab> {
        let workspace = store
            .workspace_catalog()
            .snapshot()?
            .workspaces
            .iter()
            .find(|workspace| &workspace.id == active)?;
        if let Some(pending) = self
            .pending_tab
            .as_ref()
            .filter(|pending| &pending.placement.workspace == active)
            && let Some(tab) = workspace
                .tabs
                .iter()
                .find(|tab| tab.id == pending.placement.tab)
        {
            let mut tab = tab.clone();
            if tab.focused_pane != pending.placement.pane {
                tab.zoomed_pane = tab
                    .zoomed_pane
                    .as_ref()
                    .map(|_| pending.placement.pane.clone());
                tab.focused_pane = pending.placement.pane.clone();
            }
            return Some(tab);
        }
        workspace
            .tabs
            .iter()
            .find(|tab| Some(&tab.id) == workspace.selected_tab.as_ref())
            .cloned()
    }

    /// Ends the override once the Engine agrees or the placement is gone,
    /// and otherwise (re)sends the edit that makes it the Engine's choice.
    fn settle_pending_tab(&mut self, store: &mut WindowWrite<'_>, active: &WorkspaceId) {
        let Some(pending) = &mut self.pending_tab else {
            return;
        };
        let target = &pending.placement;
        let (state, revision, can_edit) = {
            let catalog = store.workspace_catalog();
            let Some(snapshot) = catalog.snapshot() else {
                return;
            };
            let state = snapshot
                .workspaces
                .iter()
                .find(|workspace| workspace.id == target.workspace)
                .and_then(|workspace| {
                    let tab = workspace.tabs.iter().find(|tab| tab.id == target.tab)?;
                    let mut placed = Vec::new();
                    panes(&tab.layout, &mut placed);
                    placed
                        .contains(&(target.pane.clone(), target.session.clone()))
                        .then(|| {
                            workspace.selected_tab.as_ref() == Some(&target.tab)
                                && tab.focused_pane == target.pane
                        })
                });
            (state, snapshot.revision, catalog.can_edit())
        };
        let mutation = WorkspaceMutation::OpenProjectAgent {
            session_id: target.session.clone(),
            preferred_workspace: Some(target.workspace.clone()),
        };
        let confirmed = match state {
            // Gone from the layout, or the survivor itself has ended.
            None => true,
            Some(_) if !runs(store, &target.session) => true,
            Some(acknowledged) => {
                acknowledged
                    || store.workspace_edit_rejected(&mutation).is_some()
                    || pending.attempts >= CONFIRM_ATTEMPTS
                        && pending.sent.is_some_and(|sent| revision > sent)
            }
        };
        if confirmed {
            self.pending_tab = None;
            return;
        }
        if &target.workspace != active
            || !can_edit
            || pending.sent.is_some_and(|sent| revision <= sent)
        {
            return;
        }
        if store.edit_workspace(mutation) {
            pending.sent = Some(revision);
            pending.attempts += 1;
        }
    }

    /// Activating a layout (or leaving one) starts over: an override for
    /// another workspace, or a session last seen running, no longer applies.
    pub(super) fn reset_session_end_tracking(&mut self, workspace: Option<&WorkspaceId>) {
        self.live_on_screen = None;
        if self
            .pending_tab
            .as_ref()
            .is_some_and(|pending| Some(&pending.placement.workspace) != workspace)
        {
            self.pending_tab = None;
        }
    }
}

fn focused_pane_session(tab: &WorkspaceTab) -> Option<SessionId> {
    let mut placed = Vec::new();
    panes(&tab.layout, &mut placed);
    placed
        .into_iter()
        .find(|(pane, _)| pane == &tab.focused_pane)
        .map(|(_, session)| session)
}

#[cfg(test)]
mod tests {
    use super::*;
    use diri_proto::workspace::{LayoutAxis, SplitId, WorkspaceRecord};

    fn pane(id: &str) -> LayoutNode {
        LayoutNode::Pane {
            id: PaneId::new(format!("{id}-pane")),
            session_id: SessionId::new(id),
        }
    }

    fn tab(id: &str, layout: LayoutNode) -> WorkspaceTab {
        let mut placed = Vec::new();
        panes(&layout, &mut placed);
        WorkspaceTab {
            id: TabId::new(id),
            title: None,
            focused_pane: placed[0].0.clone(),
            zoomed_pane: None,
            layout,
        }
    }

    fn snapshot() -> WorkspaceSnapshot {
        let split = LayoutNode::Split {
            id: SplitId::new("split"),
            axis: LayoutAxis::Horizontal,
            fraction: 0.5,
            first: Box::new(pane("ended")),
            second: Box::new(pane("sibling")),
        };
        WorkspaceSnapshot {
            revision: 1,
            workspaces: vec![
                WorkspaceRecord {
                    project_id: None,
                    id: WorkspaceId::new("here"),
                    name: "Here".into(),
                    selected_tab: Some(TabId::new("current")),
                    tabs: vec![
                        tab("current", split),
                        tab("older", pane("older")),
                        tab("recent", pane("recent")),
                    ],
                },
                WorkspaceRecord {
                    project_id: None,
                    id: WorkspaceId::new("there"),
                    name: "There".into(),
                    selected_tab: Some(TabId::new("elsewhere")),
                    tabs: vec![tab("elsewhere", pane("elsewhere"))],
                },
            ],
            ..Default::default()
        }
    }

    fn pick(mru: &[&str], dead: &[&str]) -> Option<String> {
        let mru: Vec<_> = mru.iter().map(|id| SessionId::new(*id)).collect();
        survivor(
            &snapshot(),
            &WorkspaceId::new("here"),
            &TabId::new("current"),
            &SessionId::new("ended"),
            &mru,
            |id| !dead.contains(&id.0.as_str()),
        )
        .map(|placement| placement.session.0)
    }

    #[test]
    fn a_split_sibling_comes_first_then_this_layout_then_another_by_recency() {
        assert_eq!(
            pick(&["recent", "sibling"], &[]).as_deref(),
            Some("sibling")
        );
        assert_eq!(
            pick(&["elsewhere", "older", "recent"], &["sibling"]).as_deref(),
            Some("older"),
            "this layout first, most recently used within it"
        );
        assert_eq!(
            pick(&[], &["sibling"]).as_deref(),
            Some("older"),
            "unused sessions keep layout order"
        );
        assert_eq!(
            pick(&["recent"], &["sibling", "older", "recent"]).as_deref(),
            Some("elsewhere")
        );
        assert_eq!(
            pick(&[], &["sibling", "older", "recent", "elsewhere"]),
            None
        );
    }

    #[test]
    fn only_a_crash_or_a_signal_is_announced_after_moving_away() {
        let fixture =
            crate::sidebar::SidebarPreviewFixture::make(crate::sidebar::PreviewScenario::Typical);
        let mut session = fixture.list.sessions[0].clone();
        let exit = |reason, code, signal| {
            SessionStatus::Exited(diri_proto::ExitInfo {
                reason,
                code,
                signal,
                system_restart: false,
            })
        };
        session.status = exit(ExitReason::Exited, Some(0), None);
        assert_eq!(unclean_exit_notice(&session), None);
        session.status = exit(ExitReason::DaemonRestart, None, None);
        assert_eq!(unclean_exit_notice(&session), None);
        session.status = exit(ExitReason::Exited, Some(1), None);
        assert!(
            unclean_exit_notice(&session)
                .unwrap()
                .message
                .contains("exited with code 1")
        );
        session.status = exit(ExitReason::Signaled, None, Some(9));
        assert!(
            unclean_exit_notice(&session)
                .unwrap()
                .message
                .contains("stopped by signal 9")
        );
    }
}
