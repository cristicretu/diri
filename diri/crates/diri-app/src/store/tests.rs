use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use diri_proto::{
    AgentDescriptor, AgentKind, AgentReadinessItem, AgentReadinessResult, AttentionLevel,
    DateMillis, ExitInfo, ExitReason, Project, ProjectId, Resumability, SessionId,
    SessionListResult, SessionRecord, SessionStatus, TitleSource,
};
use tempfile::tempdir;
use tokio::sync::mpsc;

use crate::notifications::NotificationSound;

use super::{
    ClickModifiers, EventEnvelope, InspectorTab, Prefs, SessionStore, SidebarProjection,
    StoreEffect, StoreEventChange, TerminalResidency, WindowMode, WindowPlacement,
    event_publication_policy,
};
use crate::switcher::{OverviewFilter, OverviewLane, SwitcherKey};

fn id(value: &str) -> SessionId {
    SessionId::new(value)
}

fn pid(value: &str) -> ProjectId {
    ProjectId::new(value)
}

fn session(value: &str, project: &str, created: f64) -> SessionRecord {
    SessionRecord {
        id: id(value),
        kind: AgentKind::CLAUDE_CODE,
        cwd: format!("/work/{project}"),
        project_id: pid(project),
        worktree_path: None,
        git_branch: None,
        title: value.to_owned(),
        title_source: TitleSource::Placeholder,
        agent_session_id: None,
        transcript_path: None,
        status: SessionStatus::Idle,
        status_evidence: None,
        needs_input: None,
        resumability: Resumability::Live,
        parent: None,
        created_at: DateMillis(created),
        updated_at: DateMillis(created),
        last_turn_completed_at: None,
        last_seen_at: None,
        pinned: false,
        archived_at: None,
        host: None,
        remote_persistence: None,
        hibernation: None,
        memory_bytes: None,
        artifacts: None,
        pull_requests: None,
        listening_ports: None,
        foreground_agent: None,
    }
}

fn project(value: &str, name: &str) -> Project {
    Project {
        id: pid(value),
        root: format!("/work/{value}"),
        name: name.to_owned(),
        pinned_order: None,
        host: None,
    }
}

fn hydrated(
    sessions: Vec<SessionRecord>,
    projects: Vec<Project>,
    prefs: Prefs,
) -> (SessionStore, mpsc::UnboundedReceiver<StoreEffect>) {
    let (mut store, effects) = SessionStore::headless(prefs);
    store.hydrate(SessionListResult { sessions, projects });
    (store, effects)
}

fn drain(effects: &mut mpsc::UnboundedReceiver<StoreEffect>) -> Vec<StoreEffect> {
    let mut drained = Vec::new();
    while let Ok(effect) = effects.try_recv() {
        drained.push(effect);
    }
    drained
}

#[test]
fn switcher_store_integration_commits_only_on_control_release() {
    let sessions = vec![
        session("one", "a", 3.0),
        session("two", "a", 2.0),
        session("three", "a", 1.0),
    ];
    let (mut store, _effects) = hydrated(sessions, vec![project("a", "A")], Prefs::default());
    store.select(id("two"));
    store.select(id("three"));

    assert!(store.handle_switcher_key(SwitcherKey::Tab {
        control: true,
        shift: false,
    }));
    assert_eq!(store.selected_session_id(), Some(&id("three")));
    assert_eq!(store.switcher_state().highlighted(), Some(&id("two")));

    assert!(!store.handle_switcher_modifiers_changed(false));
    assert_eq!(store.selected_session_id(), Some(&id("two")));
    assert!(!store.switcher_state().is_visible());

    store.handle_switcher_key(SwitcherKey::Tab {
        control: true,
        shift: true,
    });
    assert_eq!(store.switcher_state().highlighted(), Some(&id("one")));
    assert!(store.handle_switcher_key(SwitcherKey::Escape));
    assert_eq!(store.selected_session_id(), Some(&id("two")));
}

#[test]
fn selecting_a_session_wakes_its_artifact_refresh_even_when_already_seen() {
    let (mut store, mut effects) = hydrated(
        vec![session("one", "a", 2.0), session("two", "a", 1.0)],
        vec![project("a", "A")],
        Prefs::default(),
    );
    drain(&mut effects);

    store.select(id("two"));
    assert!(
        drain(&mut effects)
            .into_iter()
            .any(|effect| matches!(effect, StoreEffect::MarkSeen(session) if session == id("two")))
    );
}

#[test]
fn hydrate_restores_the_last_selected_session_instead_of_the_first() {
    let prefs = Prefs {
        last_selected_session: Some(id("two")),
        ..Prefs::default()
    };
    let (store, _) = hydrated(
        vec![session("one", "a", 2.0), session("two", "a", 1.0)],
        vec![project("a", "A")],
        prefs,
    );

    assert_eq!(store.selected_session_id(), Some(&id("two")));
    assert!(store.terminal_residency().contains(&id("two")));
}

#[test]
fn stale_restored_selection_falls_back_and_is_replaced() {
    let prefs = Prefs {
        last_selected_session: Some(id("gone")),
        ..Prefs::default()
    };
    let (store, _) = hydrated(
        vec![session("one", "a", 1.0), session("two", "a", 2.0)],
        vec![project("a", "A")],
        prefs,
    );

    assert_eq!(store.selected_session_id(), Some(&id("one")));
    assert_eq!(store.preferences().last_selected_session, Some(id("one")));
}

#[test]
fn overview_store_integration_filters_selects_and_bulk_closes() {
    let live = session("live", "a", 1.0);
    let mut ended = session("ended", "a", 2.0);
    ended.status = SessionStatus::Exited(ExitInfo {
        reason: ExitReason::Exited,
        code: Some(0),
        signal: None,
    });
    let (mut store, mut effects) =
        hydrated(vec![live, ended], vec![project("a", "A")], Prefs::default());
    drain(&mut effects);

    store.toggle_overview();
    store.set_overview_filter(OverviewFilter::Lane(OverviewLane::Ended));
    store.select_all_overview_sessions();
    assert_eq!(
        store.overview_state().selection(),
        &HashSet::from([id("ended")])
    );
    assert!(store.close_overview_selection());
    assert_eq!(drain(&mut effects), vec![StoreEffect::Remove(id("ended"))]);
    assert!(store.overview_state().selection().is_empty());
}

#[test]
fn projection_keeps_manual_ranks_and_appends_the_rest_in_arrival_order() {
    let prefs = Prefs {
        sidebar_project_order: vec![pid("z")],
        sidebar_session_order: vec![id("old-ranked")],
        ..Prefs::default()
    };
    let (mut store, _) = hydrated(
        vec![
            session("old-ranked", "a", 1.0),
            session("new", "a", 3.0),
            session("middle", "a", 2.0),
            session("z-session", "z", 4.0),
        ],
        vec![project("a", "Alpha"), project("z", "Zulu")],
        prefs,
    );

    let projection = store.sidebar_projection();
    assert_eq!(projection.projects[0].project.id, pid("z"));
    assert_eq!(
        rows(&projection, 1),
        // Oldest first behind the one row that was ranked by hand. The newest
        // session is last, which is the whole point: a session created now
        // belongs at the bottom, not wherever its timestamp happens to sort.
        vec![id("old-ranked"), id("middle"), id("new")]
    );
}

/// The reported bug, from both ends: a session and a project created after the
/// sidebar already has contents must land at the bottom, not in the middle.
#[test]
fn a_new_session_and_a_new_project_land_at_the_bottom() {
    let (mut store, _) = hydrated(
        vec![session("first", "a", 1.0), session("second", "a", 2.0)],
        vec![project("a", "Alpha")],
        Prefs::default(),
    );
    assert_eq!(
        rows(&store.sidebar_projection(), 0),
        vec![id("first"), id("second")]
    );

    // "Zed" sorts after "Alpha" alphabetically and "Ada" sorts before it; the
    // old projection put a new project wherever its name fell, so use a name
    // that would have jumped the queue.
    store.upsert_session(session("third", "a", 3.0));
    store.upsert_session(session("fresh", "ada", 4.0));

    let projection = store.sidebar_projection();
    assert_eq!(
        rows(&projection, 0),
        vec![id("first"), id("second"), id("third")],
        "a new session appends to its project"
    );
    assert_eq!(
        projection
            .projects
            .iter()
            .map(|group| group.project.id.clone())
            .collect::<Vec<_>>(),
        vec![pid("a"), pid("ada")],
        "a new project appends to the list, whatever it is called"
    );
}

/// The order is total, so a row dragged to the end of its group stays there.
/// Under the old ranked-before-unranked comparator it sprang back above every
/// sibling that had never been dragged.
#[test]
fn a_session_dragged_to_the_end_stays_at_the_end() {
    let (mut store, _) = hydrated(
        vec![
            session("one", "a", 1.0),
            session("two", "a", 2.0),
            session("three", "a", 3.0),
        ],
        vec![project("a", "Alpha")],
        Prefs::default(),
    );

    let mut order = store.sidebar_session_order();
    super::super::sidebar::move_to_end(&mut order, &id("one"));
    store.set_session_order(order).expect("persist order");

    assert_eq!(
        rows(&store.sidebar_projection(), 0),
        vec![id("two"), id("three"), id("one")]
    );
}

/// Order and collapse state for departed sessions would otherwise pile up in
/// prefs.json forever and reattach to a recycled id.
#[test]
fn removing_a_session_prunes_it_from_the_persisted_order() {
    let (mut store, _) = hydrated(
        vec![session("one", "a", 1.0), session("two", "a", 2.0)],
        vec![project("a", "Alpha")],
        Prefs::default(),
    );
    store
        .toggle_session_collapsed(id("one"))
        .expect("collapse one");
    assert!(
        store
            .preferences()
            .sidebar_session_order
            .contains(&id("one"))
    );

    store.remove_session_record(&id("one"));

    assert_eq!(store.preferences().sidebar_session_order, vec![id("two")]);
    assert!(store.preferences().sidebar_collapsed_sessions.is_empty());
}

/// An MCP-spawned agent hangs off the session that spawned it, and a new child
/// arrives at the bottom of its own sibling run rather than the project's.
#[test]
fn spawned_sessions_nest_under_their_parent() {
    let child = |name: &str, parent: &str, created: f64| {
        let mut record = session(name, "p", created);
        record.parent = Some(id(parent));
        record
    };
    let (mut store, _) = hydrated(
        vec![
            session("root", "p", 1.0),
            child("child-b", "root", 3.0),
            child("child-a", "root", 2.0),
            child("grandchild", "child-a", 4.0),
            session("sibling", "p", 5.0),
        ],
        vec![project("p", "P")],
        Prefs::default(),
    );

    let projection = store.sidebar_projection();
    let shape: Vec<_> = projection.projects[0]
        .sessions
        .iter()
        .map(|row| (row.id().0.as_str().to_owned(), row.depth))
        .collect();
    assert_eq!(
        shape,
        vec![
            ("root".to_owned(), 0),
            ("child-a".to_owned(), 1),
            ("grandchild".to_owned(), 2),
            ("child-b".to_owned(), 1),
            ("sibling".to_owned(), 0),
        ]
    );
    assert!(projection.projects[0].sessions[0].has_children);
    assert!(!projection.projects[0].sessions[4].has_children);
}

/// Collapsing a parent hides its whole subtree from the rows and from ⌘1…⌘9,
/// but the sessions stay addressable for selection ranges.
#[test]
fn collapsing_a_parent_folds_away_its_subtree() {
    let mut child = session("child", "p", 2.0);
    child.parent = Some(id("root"));
    let (mut store, _) = hydrated(
        vec![session("root", "p", 1.0), child, session("other", "p", 3.0)],
        vec![project("p", "P")],
        Prefs::default(),
    );

    store
        .toggle_session_collapsed(id("root"))
        .expect("collapse");

    let projection = store.sidebar_projection();
    assert_eq!(rows(&projection, 0), vec![id("root"), id("other")]);
    assert!(projection.projects[0].sessions[0].collapsed);
    assert!(
        !projection
            .ordered_sessions
            .iter()
            .any(|session| session.id == id("child")),
        "a folded row must not consume a ⌘1…⌘9 slot"
    );
    assert!(
        projection.display_order.contains(&id("child")),
        "it is hidden, not gone"
    );
}

/// Selecting a hidden session unfolds whatever is covering it. Without this a
/// ⌘J or an MCP focus lands on a row the sidebar never shows.
#[test]
fn selecting_a_folded_session_reveals_it() {
    let mut child = session("child", "p", 2.0);
    child.parent = Some(id("root"));
    let prefs = Prefs {
        sidebar_collapsed_projects: vec![pid("p")],
        sidebar_collapsed_sessions: vec![id("root")],
        ..Prefs::default()
    };
    let (mut store, _) = hydrated(
        vec![session("root", "p", 1.0), child],
        vec![project("p", "P")],
        prefs,
    );

    store.select(id("child"));

    assert!(store.preferences().sidebar_collapsed_projects.is_empty());
    assert!(store.preferences().sidebar_collapsed_sessions.is_empty());
    assert_eq!(
        rows(&store.sidebar_projection(), 0),
        vec![id("root"), id("child")]
    );
}

/// Folding the ancestor of the selection would hide it, so the selection walks
/// up to the row being folded instead of vanishing under it.
#[test]
fn folding_over_the_selection_moves_it_to_the_fold() {
    let mut child = session("child", "p", 2.0);
    child.parent = Some(id("root"));
    let (mut store, _) = hydrated(
        vec![session("root", "p", 1.0), child],
        vec![project("p", "P")],
        Prefs::default(),
    );
    store.select(id("child"));

    store
        .toggle_session_collapsed(id("root"))
        .expect("collapse");

    assert_eq!(store.selected_session_id(), Some(&id("root")));
    assert_eq!(rows(&store.sidebar_projection(), 0), vec![id("root")]);
}

/// A parent in another project, or one that points back at its own descendant,
/// must leave the row at the top level rather than dropping or hanging it.
#[test]
fn unusable_parents_leave_the_row_at_the_root() {
    let mut foreign = session("foreign", "p", 2.0);
    foreign.parent = Some(id("elsewhere"));
    let mut left = session("left", "p", 3.0);
    left.parent = Some(id("right"));
    let mut right = session("right", "p", 4.0);
    right.parent = Some(id("left"));
    let (mut store, _) = hydrated(
        vec![session("elsewhere", "other", 1.0), foreign, left, right],
        vec![project("p", "P"), project("other", "Other")],
        Prefs::default(),
    );

    let projection = store.sidebar_projection();
    let group = projection
        .projects
        .iter()
        .find(|group| group.project.id == pid("p"))
        .expect("project p");
    assert_eq!(
        group
            .sessions
            .iter()
            .map(|row| row.depth)
            .collect::<Vec<_>>(),
        vec![0, 0, 1],
        "the cross-project child roots, and the cycle keeps exactly one edge"
    );
    assert_eq!(group.sessions.len(), 3, "no row is lost to a cycle");
}

/// Pinning sorts a row to the top of its own siblings instead of cloning it
/// into a separate section, so one session is never two rows.
#[test]
fn pinned_rows_lead_their_siblings() {
    let prefs = Prefs {
        sidebar_pinned_sessions: vec![id("third")],
        sidebar_pinned_projects: vec![pid("b")],
        ..Prefs::default()
    };
    let (mut store, _) = hydrated(
        vec![
            session("first", "a", 1.0),
            session("second", "a", 2.0),
            session("third", "a", 3.0),
            session("only", "b", 4.0),
        ],
        vec![project("a", "Alpha"), project("b", "Beta")],
        prefs,
    );

    let projection = store.sidebar_projection();
    assert_eq!(
        projection
            .projects
            .iter()
            .map(|group| group.project.id.clone())
            .collect::<Vec<_>>(),
        vec![pid("b"), pid("a")],
        "a pinned project leads even though it arrived last"
    );
    assert_eq!(
        rows(&projection, 1),
        vec![id("third"), id("first"), id("second")]
    );
    assert!(projection.projects[1].sessions[0].pinned);
}

fn rows(projection: &SidebarProjection, group: usize) -> Vec<SessionId> {
    projection.projects[group]
        .sessions
        .iter()
        .map(|row| row.id().clone())
        .collect()
}

#[test]
fn projection_synthesizes_projects_and_handles_archived_selection() {
    let active = session("active", "missing", 2.0);
    let mut archived = session("archived", "missing", 1.0);
    archived.worktree_path = Some("/repo/worktrees/feature-one".to_owned());
    archived.archived_at = Some(DateMillis(20.0));
    let (mut store, _) = hydrated(vec![active, archived], vec![], Prefs::default());

    let first = store.sidebar_projection();
    assert_eq!(first.projects[0].project.id, pid("missing"));
    assert_eq!(first.projects[0].project.root, "/work/missing");
    assert_eq!(first.ordered_sessions.len(), 1);
    assert!(Arc::ptr_eq(&first, &store.sidebar_projection()));

    store.select(id("archived"));
    let selected = store.sidebar_projection();
    assert_eq!(
        selected
            .ordered_sessions
            .iter()
            .map(|session| session.id.clone())
            .collect::<Vec<_>>(),
        vec![id("active"), id("archived")]
    );

    let mut lone = session("lone", "synthetic", 1.0);
    lone.worktree_path = Some("/repo/worktrees/feature-two".to_owned());
    let (mut lone_store, _) = hydrated(vec![lone], vec![], Prefs::default());
    let synthesized = lone_store.sidebar_projection();
    assert_eq!(
        synthesized.projects[0].project.root,
        "/repo/worktrees/feature-two"
    );
    assert_eq!(synthesized.projects[0].project.name, "feature-two");
}

#[test]
fn projection_reuses_one_session_record_per_sidebar_row() {
    let (mut store, _) = hydrated(
        vec![session("one", "p", 1.0)],
        vec![project("p", "P")],
        Prefs::default(),
    );

    let projection = store.sidebar_projection();
    let grouped: &SessionRecord = &projection.projects[0].sessions[0].session;
    let ordered: &SessionRecord = &projection.ordered_sessions[0];
    assert!(
        std::ptr::eq(grouped, ordered),
        "sidebar order must share the row record instead of cloning its transcript metadata"
    );
}

#[test]
fn multi_select_matches_finder_command_and_visible_shift_ranges() {
    let mut archived = session("archived", "p", 0.0);
    archived.archived_at = Some(DateMillis(10.0));
    let prefs = Prefs {
        sidebar_expanded_archives: vec![pid("p")],
        ..Prefs::default()
    };
    let (mut store, _) = hydrated(
        vec![
            session("one", "p", 1.0),
            session("two", "p", 2.0),
            session("three", "p", 3.0),
            archived,
        ],
        vec![project("p", "Project")],
        prefs,
    );
    store.select(id("one"));

    store.sidebar_click(
        id("three"),
        ClickModifiers {
            command: true,
            shift: false,
        },
    );
    assert_eq!(
        store.sidebar_selection,
        HashSet::from([id("one"), id("three")])
    );
    assert_eq!(store.selected_session_id, Some(id("one")));

    store.sidebar_click(
        id("archived"),
        ClickModifiers {
            command: false,
            shift: true,
        },
    );
    assert_eq!(
        store.sidebar_selection,
        HashSet::from([id("three"), id("archived")])
    );

    store.sidebar_click(id("two"), ClickModifiers::default());
    assert!(store.sidebar_selection.is_empty());
    assert_eq!(store.selected_session_id, Some(id("two")));
}

#[test]
fn focus_neighbor_prefers_same_project_below_then_above_then_global() {
    let records = vec![
        session("a-top", "a", 1.0),
        session("a-mid", "a", 2.0),
        session("a-low", "a", 3.0),
        session("b-top", "b", 4.0),
    ];
    let projects = vec![project("a", "A"), project("b", "B")];

    let (mut below, _) = hydrated(records.clone(), projects.clone(), Prefs::default());
    below.select(id("a-mid"));
    below.focus_neighbor(&HashSet::from([id("a-mid")]));
    assert_eq!(below.selected_session_id, Some(id("a-low")));

    let (mut above, _) = hydrated(records.clone(), projects.clone(), Prefs::default());
    above.select(id("a-low"));
    above.focus_neighbor(&HashSet::from([id("a-low")]));
    assert_eq!(above.selected_session_id, Some(id("a-mid")));

    let (mut global_below, _) = hydrated(records.clone(), projects.clone(), Prefs::default());
    global_below.select(id("a-mid"));
    global_below.focus_neighbor(&HashSet::from([id("a-top"), id("a-mid"), id("a-low")]));
    assert_eq!(global_below.selected_session_id, Some(id("b-top")));

    let (mut global_above, _) = hydrated(records, projects, Prefs::default());
    global_above.select(id("b-top"));
    global_above.focus_neighbor(&HashSet::from([id("b-top")]));
    assert_eq!(global_above.selected_session_id, Some(id("a-low")));
}

#[test]
fn selection_drives_mru_and_residency_eviction_signals_detach() {
    let (mut store, mut effects) = hydrated(
        vec![
            session("one", "p", 1.0),
            session("two", "p", 2.0),
            session("three", "p", 3.0),
            session("four", "p", 4.0),
        ],
        vec![project("p", "P")],
        Prefs::default(),
    );
    drain(&mut effects);
    store.select(id("two"));
    store.select(id("three"));
    store.select(id("four"));
    assert_eq!(
        store.mru_sessions(),
        vec![id("four"), id("three"), id("two"), id("one")]
    );
    assert!(drain(&mut effects).contains(&StoreEffect::DetachAttachment(id("one"))));

    let mut residency = TerminalResidency::new(3);
    residency.touch(id("a"));
    residency.touch(id("b"));
    residency.touch(id("c"));
    let update = residency.touch(id("d"));
    assert_eq!(update.evicted, Some(id("a")));
    assert_eq!(update.resident, vec![id("d"), id("c"), id("b")]);
}

#[test]
fn default_residency_keeps_only_the_visible_terminal_attached() {
    let mut residency = TerminalResidency::default();
    residency.touch(id("visible"));
    let update = residency.touch(id("next"));

    assert_eq!(update.evicted, Some(id("visible")));
    assert_eq!(update.resident, vec![id("next")]);
}

#[test]
fn spawn_response_focuses_session_when_event_arrives_first() {
    let (mut store, mut effects) = hydrated(
        vec![session("existing", "p", 1.0)],
        vec![project("p", "P")],
        Prefs::default(),
    );
    drain(&mut effects);

    // The daemon can publish session.updated before the spawn RPC response.
    // At event time the old tab is still selected, so the new record is not
    // granted terminal residency yet.
    store.upsert_session(session("spawned", "p", 2.0));
    assert!(!store.terminal_residency().contains(&id("spawned")));

    // Applying the later RPC result must do everything a real tab selection
    // does; otherwise the pane remains on "Preparing terminal…" until clicked.
    store.apply_spawn_result(id("spawned"));

    assert_eq!(store.selected_session_id(), Some(&id("spawned")));
    assert!(store.terminal_residency().contains(&id("spawned")));
}

#[test]
fn attention_rollup_and_needs_input_sort_use_proto_derivation() {
    let mut done = session("done", "p", 1.0);
    done.last_turn_completed_at = Some(DateMillis(50.0));
    done.last_seen_at = Some(DateMillis(40.0));
    let mut older_input = session("older-input", "p", 2.0);
    older_input.status = SessionStatus::NeedsInput(diri_proto::NeedsInputKind::Question);
    older_input.updated_at = DateMillis(100.0);
    let mut newer_input = session("newer-input", "p", 3.0);
    newer_input.status = SessionStatus::NeedsInput(diri_proto::NeedsInputKind::Permission);
    newer_input.updated_at = DateMillis(200.0);
    let (store, _) = hydrated(
        vec![done, older_input, newer_input],
        vec![project("p", "P")],
        Prefs::default(),
    );

    assert_eq!(store.global_attention(), AttentionLevel::NeedsInput);
    assert_eq!(
        store
            .needs_input_sessions()
            .iter()
            .map(|session| session.id.clone())
            .collect::<Vec<_>>(),
        vec![id("newer-input"), id("older-input")]
    );
}

#[test]
fn hidden_needs_input_update_chimes_and_posts_once_its_window_expires() {
    let (mut store, mut effects) = hydrated(
        vec![session("visible", "p", 2.0)],
        vec![project("p", "P")],
        Prefs::default(),
    );
    drain(&mut effects);

    let mut hidden = session("hidden", "p", 1.0);
    hidden.status = SessionStatus::NeedsInput(diri_proto::NeedsInputKind::Permission);
    store.upsert_session(hidden);

    // Nothing is announced on the transition itself, and nothing is due yet.
    assert!(
        drain(&mut effects)
            .into_iter()
            .all(|effect| !matches!(effect, StoreEffect::StatusTransition(_)))
    );
    assert!(store.drain_settled_attention(Instant::now()).is_empty());

    let transitions = store.drain_settled_attention(
        store
            .next_attention_deadline()
            .expect("needs-input update should arm a settle window"),
    );
    let transition = transitions
        .first()
        .expect("a session still blocked at its deadline is announced");
    assert_eq!(transition.sound, Some(NotificationSound::NeedsInput));
    assert!(transition.notification.is_some());
    assert!(store.next_attention_deadline().is_none());
}

#[tokio::test(start_paused = true)]
async fn the_settle_task_sleeps_on_the_window_and_then_publishes() {
    let (store, _effects) = SessionStore::headless(Prefs::default());
    let store = Arc::new(std::sync::RwLock::new(store));
    let (status_tx, mut status_rx) = tokio::sync::broadcast::channel(8);
    let task = tokio::spawn(super::run_attention_settle(
        Arc::clone(&store),
        status_tx.clone(),
    ));

    // The session the user is looking at, so the blocked one is not the
    // selected row and earns a banner as well as a chime.
    store
        .write()
        .expect("session store lock poisoned")
        .upsert_session(session("visible", "p", 2.0));
    let mut blocked = session("blocked", "p", 1.0);
    blocked.status = SessionStatus::NeedsInput(diri_proto::NeedsInputKind::Permission);
    store
        .write()
        .expect("session store lock poisoned")
        .upsert_session(blocked);

    // Nothing is published until the window is served — the task is asleep on
    // the deadline it was woken to install.
    assert!(status_rx.try_recv().is_err());

    let transition = tokio::time::timeout(Duration::from_secs(30), status_rx.recv())
        .await
        .expect("the settle task must publish once the window expires")
        .expect("status channel stays open");
    assert_eq!(transition.sound, Some(NotificationSound::NeedsInput));
    assert!(transition.notification.is_some());

    task.abort();
}

#[test]
fn a_session_that_unblocks_inside_its_window_is_never_announced() {
    let (mut store, mut effects) = hydrated(
        vec![session("visible", "p", 2.0)],
        vec![project("p", "P")],
        Prefs::default(),
    );
    drain(&mut effects);

    let mut blocked = session("hidden", "p", 1.0);
    blocked.status = SessionStatus::NeedsInput(diri_proto::NeedsInputKind::Permission);
    store.upsert_session(blocked.clone());
    assert!(store.next_attention_deadline().is_some());

    // Answered before the window expired: the pending state goes with it, so
    // the settle task is not even kept awake for it.
    blocked.status = SessionStatus::Working;
    blocked.needs_input = None;
    store.upsert_session(blocked);

    assert!(store.next_attention_deadline().is_none());
    assert!(
        store
            .drain_settled_attention(Instant::now() + Duration::from_secs(60))
            .is_empty()
    );
}

#[test]
fn selecting_a_session_inside_its_window_leaves_the_chime_but_drops_the_banner() {
    let (mut store, mut effects) = hydrated(
        vec![session("visible", "p", 2.0)],
        vec![project("p", "P")],
        Prefs::default(),
    );
    drain(&mut effects);

    let mut blocked = session("hidden", "p", 1.0);
    blocked.status = SessionStatus::NeedsInput(diri_proto::NeedsInputKind::Permission);
    store.upsert_session(blocked);
    store.select(id("hidden"));

    let transitions = store.drain_settled_attention(
        store
            .next_attention_deadline()
            .expect("needs-input update should arm a settle window"),
    );
    let transition = transitions.first().expect("the chime still fires");
    assert_eq!(transition.sound, Some(NotificationSound::NeedsInput));
    assert!(
        transition.notification.is_none(),
        "the session the user is looking at needs no system banner"
    );
}

#[test]
fn auto_resume_is_attempted_once_per_run() {
    let mut record = session("restart", "p", 1.0);
    record.status = SessionStatus::Exited(ExitInfo {
        reason: ExitReason::DaemonRestart,
        code: None,
        signal: None,
    });
    record.resumability = Resumability::Resumable;
    let (mut store, mut effects) = hydrated(
        vec![record.clone()],
        vec![project("p", "P")],
        Prefs::default(),
    );

    assert_eq!(
        drain(&mut effects)
            .into_iter()
            .filter(|effect| matches!(
                effect,
                StoreEffect::Resume {
                    automatic: true,
                    ..
                }
            ))
            .count(),
        1
    );
    store.upsert_session(record);
    assert!(!store.auto_resume_if_needed(&id("restart")));
    assert!(drain(&mut effects).is_empty());
}

#[test]
fn cold_boot_only_auto_resumes_the_selected_session() {
    let restart_session = |value: &str, created: f64| {
        let mut record = session(value, "p", created);
        record.status = SessionStatus::Exited(ExitInfo {
            reason: ExitReason::DaemonRestart,
            code: None,
            signal: None,
        });
        record.resumability = Resumability::Resumable;
        record
    };
    let (mut store, mut effects) = hydrated(
        vec![
            restart_session("newest", 3.0),
            restart_session("middle", 2.0),
            restart_session("oldest", 1.0),
        ],
        vec![project("p", "P")],
        Prefs::default(),
    );

    // Nothing was remembered, so the selection falls to the top of the sidebar
    // — which is the oldest session now that rows arrive at the bottom.
    assert_eq!(store.selected_session_id(), Some(&id("oldest")));
    let automatic_resumes: Vec<_> = drain(&mut effects)
        .into_iter()
        .filter_map(|effect| match effect {
            StoreEffect::Resume {
                id,
                automatic: true,
            } => Some(id),
            _ => None,
        })
        .collect();
    assert_eq!(
        automatic_resumes,
        vec![id("oldest")],
        "cold boot must not revive every previously running agent"
    );

    store.select(id("middle"));
    let selected_resumes: Vec<_> = drain(&mut effects)
        .into_iter()
        .filter_map(|effect| match effect {
            StoreEffect::Resume {
                id,
                automatic: true,
            } => Some(id),
            _ => None,
        })
        .collect();
    assert_eq!(
        selected_resumes,
        vec![id("middle")],
        "an offline conversation should revive when the user selects it"
    );
}

#[test]
fn close_confirmation_only_gates_running_sessions() {
    let running = session("running", "p", 2.0);
    let mut exited = session("exited", "p", 1.0);
    exited.status = SessionStatus::Exited(ExitInfo {
        reason: ExitReason::Exited,
        code: Some(0),
        signal: None,
    });
    let (mut store, mut effects) = hydrated(
        vec![running, exited],
        vec![project("p", "P")],
        Prefs::default(),
    );
    drain(&mut effects);

    store.request_close(vec![id("exited")]);
    assert!(store.pending_close.is_none());
    assert!(drain(&mut effects).contains(&StoreEffect::Remove(id("exited"))));

    store.request_close(vec![id("running")]);
    assert_eq!(
        store.pending_close.as_ref().map(|pending| &pending.ids),
        Some(&vec![id("running")])
    );
    assert!(drain(&mut effects).is_empty());
    store.confirm_pending_close();
    assert!(drain(&mut effects).contains(&StoreEffect::Remove(id("running"))));
}

#[test]
fn a_real_process_exit_immediately_detaches_and_removes_the_agent() {
    let (mut store, mut effects) = hydrated(
        vec![session("one", "p", 1.0)],
        vec![project("p", "P")],
        Prefs::default(),
    );
    drain(&mut effects);

    let mut exited = session("one", "p", 1.0);
    exited.status = SessionStatus::Exited(ExitInfo {
        reason: ExitReason::Exited,
        code: Some(0),
        signal: None,
    });
    store.upsert_session(exited);

    let emitted = drain(&mut effects);
    assert!(emitted.contains(&StoreEffect::DetachAttachment(id("one"))));
    assert!(emitted.contains(&StoreEffect::Remove(id("one"))));
    assert!(store.ordered_sessions().is_empty());
}

/// Closing the tab deletes the Engine record and the session's output log.
/// A signalled death is exactly when that log matters most: macOS kills agents
/// with SIGTERM under memory pressure, and silently deleting the row plus its
/// scrollback leaves nothing to explain where the session went.
#[test]
fn a_signalled_agent_keeps_its_row_and_its_scrollback() {
    let (mut store, mut effects) = hydrated(
        vec![session("one", "p", 1.0)],
        vec![project("p", "P")],
        Prefs::default(),
    );
    drain(&mut effects);

    let mut killed = session("one", "p", 1.0);
    killed.status = SessionStatus::Exited(ExitInfo {
        reason: ExitReason::Signaled,
        code: None,
        signal: Some(15),
    });
    store.upsert_session(killed);

    let emitted = drain(&mut effects);
    assert!(
        !emitted.contains(&StoreEffect::Remove(id("one"))),
        "a signalled exit must not delete the session or its log"
    );
    assert_eq!(store.ordered_sessions().len(), 1);
}

/// A conversation that can be re-entered is the case every Resume affordance
/// exists for. Removing the row on exit makes the exit pill, the resume card
/// and the sidebar entry unreachable.
#[test]
fn an_exited_but_resumable_agent_stays_listed_for_resume() {
    let (mut store, mut effects) = hydrated(
        vec![session("one", "p", 1.0)],
        vec![project("p", "P")],
        Prefs::default(),
    );
    drain(&mut effects);

    let mut exited = session("one", "p", 1.0);
    exited.status = SessionStatus::Exited(ExitInfo {
        reason: ExitReason::Exited,
        code: Some(0),
        signal: None,
    });
    exited.resumability = Resumability::Resumable;
    store.upsert_session(exited);

    let emitted = drain(&mut effects);
    assert!(
        !emitted.contains(&StoreEffect::Remove(id("one"))),
        "a resumable conversation must stay listed"
    );
    assert_eq!(store.ordered_sessions().len(), 1);
}

/// A crash is not a tidy exit: the non-zero status and the scrollback are the
/// only record of what happened.
#[test]
fn a_nonzero_exit_keeps_its_row() {
    let (mut store, mut effects) = hydrated(
        vec![session("one", "p", 1.0)],
        vec![project("p", "P")],
        Prefs::default(),
    );
    drain(&mut effects);

    let mut crashed = session("one", "p", 1.0);
    crashed.status = SessionStatus::Exited(ExitInfo {
        reason: ExitReason::Exited,
        code: Some(1),
        signal: None,
    });
    store.upsert_session(crashed);

    let emitted = drain(&mut effects);
    assert!(
        !emitted.contains(&StoreEffect::Remove(id("one"))),
        "a failed exit must not delete the session or its log"
    );
    assert_eq!(store.ordered_sessions().len(), 1);
}

#[test]
fn daemon_restart_exit_remains_available_for_automatic_resume() {
    let (mut store, mut effects) = hydrated(
        vec![session("one", "p", 1.0)],
        vec![project("p", "P")],
        Prefs::default(),
    );
    drain(&mut effects);

    let mut restart = session("one", "p", 1.0);
    restart.status = SessionStatus::Exited(ExitInfo {
        reason: ExitReason::DaemonRestart,
        code: None,
        signal: None,
    });
    restart.resumability = Resumability::Resumable;
    store.upsert_session(restart);

    assert!(
        !drain(&mut effects)
            .iter()
            .any(|effect| matches!(effect, StoreEffect::Remove(_)))
    );
}

#[test]
fn stale_directory_responses_cannot_overwrite_newer_navigation() {
    let (mut store, mut effects) = SessionStore::headless(Prefs::default());
    store.request_directory_listing(Some("forge".into()), "/srv".into());
    let first = drain(&mut effects)
        .into_iter()
        .find_map(|effect| match effect {
            StoreEffect::ListDirectories { request_id, .. } => Some(request_id),
            _ => None,
        })
        .expect("first request");
    store.request_directory_listing(Some("forge".into()), "/srv/app".into());
    let second = drain(&mut effects)
        .into_iter()
        .find_map(|effect| match effect {
            StoreEffect::ListDirectories { request_id, .. } => Some(request_id),
            _ => None,
        })
        .expect("second request");

    store.finish_directory_listing(first, Err("stale".into()));
    assert_eq!(
        store.directory_listing(Some("forge"), "/srv/app"),
        Some(&super::DirectoryListingState::Loading)
    );
    store.finish_directory_listing(second, Err("latest".into()));
    assert_eq!(
        store.directory_listing(Some("forge"), "/srv/app"),
        Some(&super::DirectoryListingState::Error("latest".into()))
    );
}

#[test]
fn a_closing_row_leaves_at_once_and_ignores_further_clicks() {
    let (mut store, mut effects) = hydrated(
        vec![session("one", "p", 2.0), session("two", "p", 1.0)],
        vec![project("p", "P")],
        Prefs {
            confirm_before_closing_session: false,
            ..Prefs::default()
        },
    );
    drain(&mut effects);

    store.request_close(vec![id("one")]);
    // The daemon still has to terminate the process tree, but the row is gone.
    assert_eq!(
        store
            .ordered_sessions()
            .iter()
            .map(|s| s.id.clone())
            .collect::<Vec<_>>(),
        vec![id("two")]
    );
    assert!(drain(&mut effects).contains(&StoreEffect::Remove(id("one"))));

    // A second ✕ on the same row is a no-op rather than a repeat request.
    store.request_close(vec![id("one")]);
    assert!(drain(&mut effects).is_empty());

    // A resync that still lists the session means the close never landed.
    store.hydrate(SessionListResult {
        sessions: vec![session("one", "p", 2.0), session("two", "p", 1.0)],
        projects: vec![project("p", "P")],
    });
    assert_eq!(store.ordered_sessions().len(), 2);
}

#[test]
fn a_pending_confirmation_hides_the_row_only_once_confirmed() {
    let (mut store, mut effects) = hydrated(
        vec![session("one", "p", 2.0)],
        vec![project("p", "P")],
        Prefs::default(),
    );
    drain(&mut effects);

    store.request_close(vec![id("one")]);
    assert_eq!(store.ordered_sessions().len(), 1);
    store.confirm_pending_close();
    assert!(store.ordered_sessions().is_empty());
}

#[test]
fn prefs_round_trip_and_zoom_clamp() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("nested/prefs.json");
    let prefs = Prefs {
        default_agent: AgentKind::GEMINI,
        default_spawn_host: Some("forge".to_owned()),
        terminal_font_size: 19.5,
        window_placement: Some(WindowPlacement {
            display_uuid: Some("display-one".to_owned()),
            mode: WindowMode::Fullscreen,
            x: 120.0,
            y: 80.0,
            width: 1440.0,
            height: 900.0,
        }),
        sidebar_visible: false,
        sidebar_width: 284.0,
        inspector_width: 516.0,
        inspector_tab: InspectorTab::Artifacts,
        last_selected_session: Some(id("s")),
        quick_open_roots: "~/fun\n~/src".to_owned(),
        sidebar_project_order: vec![pid("p")],
        sidebar_pinned_sessions: vec![id("s")],
        ..Prefs::default()
    };
    prefs.save(&path).unwrap();
    assert_eq!(Prefs::load(&path).unwrap(), prefs);

    let (mut store, _) = SessionStore::load(&path).unwrap();
    store.zoom_terminal(100.0).unwrap();
    assert_eq!(store.prefs.terminal_font_size, 20.0);
    store.zoom_terminal(-100.0).unwrap();
    assert_eq!(store.prefs.terminal_font_size, 10.0);
    store.reset_terminal_zoom().unwrap();
    assert_eq!(Prefs::load(&path).unwrap().terminal_font_size, 13.0);
}

#[test]
fn default_agent_preferences_migrate_legacy_values_and_persist_manifest_ids() {
    for (saved, expected) in [
        ("claudeCode", AgentKind::CLAUDE_CODE),
        ("codex", AgentKind::CODEX),
        ("cursor", AgentKind::CURSOR),
        ("gemini", AgentKind::GEMINI),
    ] {
        let prefs: Prefs = serde_json::from_value(serde_json::json!({
            "defaultAgent": saved
        }))
        .expect("legacy prefs decode");
        assert_eq!(prefs.default_agent, expected);
    }

    let prefs = Prefs {
        default_agent: AgentKind::new("amp"),
        ..Prefs::default()
    };
    let encoded = serde_json::to_value(&prefs).expect("prefs encode");
    assert_eq!(encoded["defaultAgent"], "amp");
    let decoded: Prefs = serde_json::from_value(encoded).expect("manifest id decodes");
    assert_eq!(decoded.default_agent, AgentKind::new("amp"));
}

#[test]
fn unknown_saved_default_repairs_to_available_first_class_then_shell() {
    let prefs = Prefs {
        default_agent: AgentKind::new("removed-agent"),
        ..Prefs::default()
    };
    let (mut store, _) = SessionStore::headless(prefs);
    store.set_agent_catalog(AgentReadinessResult {
        agents: vec![AgentReadinessItem {
            kind: AgentKind::CODEX,
            binary: "codex".into(),
            path: Some("/bin/codex".into()),
            descriptor: Some(AgentDescriptor {
                id: AgentKind::CODEX_ID.into(),
                display_name: "Codex".into(),
                first_class: true,
                ..AgentDescriptor::default()
            }),
            ..AgentReadinessItem::default()
        }],
        ..AgentReadinessResult::default()
    });
    assert_eq!(store.preferences().default_agent, AgentKind::CODEX);

    store
        .update_preferences(|prefs| prefs.default_agent = AgentKind::new("removed-again"))
        .expect("headless prefs update");
    store.set_agent_catalog(AgentReadinessResult {
        agents: vec![AgentReadinessItem {
            kind: AgentKind::new("amp"),
            binary: "amp".into(),
            path: Some("/bin/amp".into()),
            descriptor: Some(AgentDescriptor {
                id: "amp".into(),
                display_name: "Amp".into(),
                first_class: false,
                ..AgentDescriptor::default()
            }),
            ..AgentReadinessItem::default()
        }],
        ..AgentReadinessResult::default()
    });
    assert_eq!(store.preferences().default_agent, AgentKind::SHELL);
}

#[test]
fn a_configure_issued_during_an_inflight_scan_still_reaches_the_engine() {
    let (mut store, mut effects) = SessionStore::headless(Prefs::default());
    store.request_agent_catalog(Some("forge".into()), false);
    assert!(matches!(
        effects.try_recv(),
        Ok(StoreEffect::RefreshAgents { .. })
    ));
    // The scan has not answered yet. A settings toggle flipped in that window
    // is a user mutation, not a cache refresh — dropping it would silently
    // revert the control on the next catalog paint.
    store.configure_agent(diri_proto::AgentConfigureParams {
        host: Some("forge".into()),
        kind: AgentKind::CLAUDE_CODE,
        executable_path: None,
        show_in_quick_create: false,
    });
    assert!(matches!(
        effects.try_recv(),
        Ok(StoreEffect::ConfigureAgent(_))
    ));
}

#[test]
fn a_failed_scan_is_retried_only_by_an_explicit_rescan() {
    let (mut store, mut effects) = SessionStore::headless(Prefs::default());
    store.request_agent_catalog(Some("forge".into()), false);
    assert!(matches!(
        effects.try_recv(),
        Ok(StoreEffect::RefreshAgents { .. })
    ));
    // The effect loop reports the failure: the scan retires, error recorded.
    store.finish_agent_catalog_request("forge");
    store
        .agent_catalog_errors
        .insert("forge".into(), "ssh: connect timed out".into());
    // Render paths ask again on every paint of a surface whose catalog is
    // absent; after a failure that must not become a scan loop against an
    // unreachable host.
    store.request_agent_catalog(Some("forge".into()), false);
    assert!(effects.try_recv().is_err());
    store.request_agent_catalog(Some("forge".into()), true);
    assert!(matches!(
        effects.try_recv(),
        Ok(StoreEffect::RefreshAgents { force: true, .. })
    ));
}

#[test]
fn refresh_reaches_the_engine_while_a_stalled_scan_is_still_outstanding() {
    let (mut store, mut effects) = SessionStore::headless(Prefs::default());
    store.request_agent_catalog(Some("forge".into()), false);
    assert!(matches!(
        effects.try_recv(),
        Ok(StoreEffect::RefreshAgents { force: false, .. })
    ));
    // Nothing has answered. A passive re-ask on every paint must not pile up
    // scans, but Refresh is the user's only escape from a wedged remote scan
    // and has to reach the daemon.
    store.request_agent_catalog(Some("forge".into()), false);
    assert!(effects.try_recv().is_err());
    store.request_agent_catalog(Some("forge".into()), true);
    assert!(matches!(
        effects.try_recv(),
        Ok(StoreEffect::RefreshAgents { force: true, .. })
    ));
    // One rescue attempt is the cap: further clicks would queue daemon-side
    // scans behind the same per-target lock.
    store.request_agent_catalog(Some("forge".into()), true);
    assert!(effects.try_recv().is_err());

    // The rescue answers first. The target keeps reading as loading until the
    // stalled scan retires too, and only then does a passive request resume.
    store.finish_agent_catalog_request("forge");
    assert!(store.agent_catalog_is_loading(Some("forge")));
    store.finish_agent_catalog_request("forge");
    assert!(!store.agent_catalog_is_loading(Some("forge")));
    store.request_agent_catalog(Some("forge".into()), false);
    assert!(matches!(
        effects.try_recv(),
        Ok(StoreEffect::RefreshAgents { .. })
    ));
}

#[test]
fn an_installed_catalog_leaves_an_outstanding_scan_running() {
    let (mut store, _effects) = SessionStore::headless(Prefs::default());
    store.request_agent_catalog(Some("forge".into()), false);
    store.request_agent_catalog(Some("forge".into()), true);
    // A catalog arriving out of band (the reply to the rescue, a connect-time
    // install) is not a reply to every request: retiring the target here would
    // drop the spinner while a scan is still on the wire.
    store.set_agent_catalog(AgentReadinessResult {
        host: Some("forge".into()),
        ..AgentReadinessResult::default()
    });
    assert!(store.agent_catalog_is_loading(Some("forge")));
}

#[test]
fn legacy_last_spawn_host_migrates_to_the_explicit_default() {
    let prefs: Prefs = serde_json::from_str(r#"{"lastSpawnHost":"forge"}"#).unwrap();
    assert_eq!(prefs.default_spawn_host.as_deref(), Some("forge"));
}

#[test]
fn selected_session_persists_across_store_reloads() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("prefs.json");
    Prefs::default().save(&path).unwrap();

    let (mut store, _) = SessionStore::load(&path).unwrap();
    store.hydrate(SessionListResult {
        sessions: vec![session("one", "a", 2.0), session("two", "a", 1.0)],
        projects: vec![project("a", "A")],
    });
    store.select(id("two"));
    drop(store);

    let (mut restored, _) = SessionStore::load(&path).unwrap();
    restored.hydrate(SessionListResult {
        sessions: vec![session("one", "a", 2.0), session("two", "a", 1.0)],
        projects: vec![project("a", "A")],
    });
    assert_eq!(restored.selected_session_id(), Some(&id("two")));
}

#[test]
fn synthetic_events_upsert_project_and_remove_with_neighbor_focus() {
    let (mut store, _) = hydrated(
        vec![session("one", "p", 1.0), session("two", "p", 2.0)],
        vec![],
        Prefs::default(),
    );
    store.select(id("one"));
    store.handle_event(EventEnvelope {
        name: diri_proto::EventName::SESSION_UPDATED.to_owned(),
        seq: 1,
        params: serde_json::to_value(session("three", "p", 0.0)).unwrap(),
    });
    assert!(store.sessions.contains_key(&id("three")));

    let updated_project = project("p", "Renamed");
    store.handle_event(EventEnvelope {
        name: diri_proto::EventName::PROJECT_UPDATED.to_owned(),
        seq: 2,
        params: serde_json::to_value(updated_project).unwrap(),
    });
    assert_eq!(
        store.sidebar_projection().projects[0].project.name,
        "Renamed"
    );

    store.handle_event(EventEnvelope {
        name: diri_proto::EventName::SESSION_REMOVED.to_owned(),
        seq: 3,
        params: serde_json::json!({"id": "one"}),
    });
    assert_eq!(store.selected_session_id, Some(id("two")));
    assert!(!store.sessions.contains_key(&id("one")));
}

#[test]
fn identical_or_unrelated_daemon_events_do_not_publish_ui_changes() {
    let existing = session("one", "p", 1.0);
    let (mut store, _) = hydrated(
        vec![existing.clone()],
        vec![project("p", "Project")],
        Prefs::default(),
    );

    assert!(!store.handle_event(EventEnvelope {
        name: diri_proto::EventName::SESSION_UPDATED.to_owned(),
        seq: 1,
        params: serde_json::to_value(existing.clone()).unwrap(),
    }));
    assert!(!store.handle_event(EventEnvelope {
        name: "terminal.grid".to_owned(),
        seq: 2,
        params: serde_json::json!({}),
    }));

    let mut changed = existing;
    changed.title = "Renamed".to_owned();
    assert!(store.handle_event(EventEnvelope {
        name: diri_proto::EventName::SESSION_UPDATED.to_owned(),
        seq: 3,
        params: serde_json::to_value(changed).unwrap(),
    }));
}

#[test]
fn compact_resource_events_patch_only_resource_fields() {
    let existing = session("one", "p", 1.0);
    let original_title = existing.title.clone();
    let (mut store, _) = hydrated(
        vec![existing],
        vec![project("p", "Project")],
        Prefs::default(),
    );

    assert!(store.handle_event(EventEnvelope {
        name: diri_proto::EventName::SESSION_RESOURCES.to_owned(),
        seq: 1,
        params: serde_json::json!({"id":"one","memoryBytes":42000000}),
    }));

    let patched = store.sessions().get(&id("one")).unwrap();
    assert_eq!(patched.memory_bytes, Some(42_000_000));
    assert_eq!(patched.title, original_title);
}

#[test]
fn background_resource_samples_do_not_wake_views() {
    assert_eq!(
        event_publication_policy(StoreEventChange::Resources, false),
        (false, false)
    );
    assert_eq!(
        event_publication_policy(StoreEventChange::Model, false),
        (true, false),
        "model changes still keep the menu snapshot current"
    );
    assert_eq!(
        event_publication_policy(StoreEventChange::Resources, true),
        (true, true)
    );
}

#[test]
fn auxiliary_terminal_inherits_context_without_becoming_sidebar_selection() {
    let mut primary = session("one", "p", 2.0);
    primary.cwd = "/work/p/subdir".to_owned();
    primary.host = Some("forge".to_owned());
    let (mut store, mut effects) = hydrated(
        vec![primary],
        vec![project("p", "Project")],
        Prefs::default(),
    );
    store.select(id("one"));
    drain(&mut effects);

    assert!(store.spawn_auxiliary_terminal(id("one")));
    let effects = drain(&mut effects);
    let Some(StoreEffect::SpawnAuxiliary(params)) = effects.first() else {
        panic!("expected auxiliary spawn, got {effects:?}");
    };
    assert_eq!(params.kind, AgentKind::SHELL);
    assert_eq!(params.cwd, "/work/p/subdir");
    assert_eq!(params.host.as_deref(), Some("forge"));
    assert_eq!(params.parent, Some(id("one")));
    assert_eq!(store.selected_session_id(), Some(&id("one")));
}

#[test]
fn auxiliary_terminal_is_hidden_and_removed_with_its_parent() {
    let primary = session("one", "p", 2.0);
    let mut terminal = session("terminal", "p", 1.0);
    terminal.kind = AgentKind::SHELL;
    terminal.parent = Some(id("one"));
    terminal.title = super::AUXILIARY_TERMINAL_TITLE.to_owned();
    let (mut store, mut effects) = hydrated(
        vec![primary, terminal],
        vec![project("p", "Project")],
        Prefs::default(),
    );
    drain(&mut effects);

    assert_eq!(
        store
            .ordered_sessions()
            .into_iter()
            .map(|session| session.id)
            .collect::<Vec<_>>(),
        vec![id("one")]
    );
    assert_eq!(
        store
            .auxiliary_terminal_for(&id("one"))
            .map(|s| s.id.clone()),
        Some(id("terminal"))
    );

    store.remove_sessions(vec![id("one")]);
    let removed: HashSet<_> = drain(&mut effects)
        .into_iter()
        .filter_map(|effect| match effect {
            StoreEffect::Remove(id) => Some(id),
            _ => None,
        })
        .collect();
    assert_eq!(removed, HashSet::from([id("one"), id("terminal")]));
}

#[test]
fn remote_spawn_uses_host_default_cwd_and_drops_worktree() {
    let (mut store, mut effects) = hydrated(
        vec![session("one", "p", 1.0)],
        vec![project("p", "Project")],
        Prefs::default(),
    );
    store.set_hosts(vec![diri_proto::HostEntry {
        id: "forge".into(),
        name: Some("Forge".into()),
        ssh: "cristi@forge".into(),
        default_cwd: Some("~/code".into()),
        node: None,
    }]);
    store.select(id("one"));
    drain(&mut effects);

    fn spawn_params(
        effects: &mut mpsc::UnboundedReceiver<StoreEffect>,
    ) -> diri_proto::SessionSpawnParams {
        let spawned = drain(effects);
        match spawned.first() {
            Some(StoreEffect::Spawn(params)) => params.clone(),
            other => panic!("expected spawn effect, got {other:?}"),
        }
    }

    // Host set + no explicit cwd: the host's defaultCwd wins over the selected
    // session's LOCAL directory, and worktree options are dropped entirely.
    store.spawn_kind(
        AgentKind::CLAUDE_CODE,
        super::SpawnOptions {
            host: Some("forge".into()),
            worktree: Some(super::WorktreeSpawn {
                create: true,
                branch: None,
            }),
            ..super::SpawnOptions::default()
        },
    );
    let params = spawn_params(&mut effects);
    assert_eq!(params.host.as_deref(), Some("forge"));
    assert_eq!(params.cwd, "~/code");
    assert_eq!(params.new_worktree, None);

    // Explicit remote override beats the default cwd.
    store.spawn_kind(
        AgentKind::SHELL,
        super::SpawnOptions {
            host: Some("forge".into()),
            cwd: Some("~/deploys".into()),
            ..super::SpawnOptions::default()
        },
    );
    assert_eq!(spawn_params(&mut effects).cwd, "~/deploys");

    // Unknown host id (stale picker) still spawns, in the remote home.
    store.spawn_kind(
        AgentKind::SHELL,
        super::SpawnOptions {
            host: Some("gone".into()),
            ..super::SpawnOptions::default()
        },
    );
    assert_eq!(spawn_params(&mut effects).cwd, "~");

    // Local spawns are untouched: selected session's directory, no host.
    store.spawn_kind(AgentKind::SHELL, super::SpawnOptions::default());
    let params = spawn_params(&mut effects);
    assert_eq!(params.host, None);
    assert_eq!(params.cwd, "/work/p");

    // Host badge lookup falls back to the raw id when the entry is gone.
    assert_eq!(store.host_display_name("forge"), "Forge");
    assert_eq!(store.host_display_name("gone"), "gone");
}

#[test]
fn new_agent_target_uses_the_configured_default_instead_of_the_selected_session() {
    let (mut store, mut effects) = hydrated(
        vec![session("local", "p", 1.0)],
        vec![project("p", "P")],
        Prefs {
            default_spawn_host: Some("forge".into()),
            ..Prefs::default()
        },
    );
    store.set_hosts(vec![diri_proto::HostEntry {
        id: "forge".into(),
        name: Some("Forge".into()),
        ssh: "cristi@forge".into(),
        default_cwd: Some("~/code".into()),
        node: None,
    }]);
    store.select(id("local"));
    drain(&mut effects);

    assert_eq!(
        store.begin_repo_targeting().as_deref(),
        Some("forge"),
        "the picker should use the configured shortcut destination"
    );
}

#[test]
fn top_level_shortcuts_spawn_on_the_configured_default_host() {
    let (mut store, mut effects) = hydrated(
        vec![session("local", "p", 1.0)],
        vec![project("p", "P")],
        Prefs {
            default_spawn_host: Some("forge".into()),
            ..Prefs::default()
        },
    );
    store.set_hosts(vec![diri_proto::HostEntry {
        id: "forge".into(),
        name: Some("Forge".into()),
        ssh: "cristi@forge".into(),
        default_cwd: None,
        node: None,
    }]);
    drain(&mut effects);

    store.spawn_default(super::SpawnOptions::default());

    let params = match effects.try_recv() {
        Ok(StoreEffect::Spawn(params)) => params,
        other => panic!("expected spawn effect, got {other:?}"),
    };
    assert_eq!(params.host.as_deref(), Some("forge"));
    assert_eq!(params.cwd, "~");
}

#[test]
fn selecting_a_default_host_persists_across_store_reloads() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("prefs.json");
    Prefs::default().save(&path).unwrap();
    let (mut store, _effects) = SessionStore::load(&path).unwrap();
    store.set_hosts(vec![diri_proto::HostEntry {
        id: "forge".into(),
        name: Some("Forge".into()),
        ssh: "cristi@forge".into(),
        default_cwd: Some("~/code".into()),
        node: None,
    }]);

    store.set_default_spawn_host(Some("forge".into()));

    assert_eq!(
        Prefs::load(&path).unwrap().default_spawn_host.as_deref(),
        Some("forge")
    );
}

/// A persisted shortcut destination is only acceptable if it is reversible:
/// picking "This Mac" again must clear it on disk, survive a reload, and put
/// the top-level shortcuts back on this machine.
#[test]
fn a_remote_default_host_round_trips_back_to_local() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("prefs.json");
    Prefs::default().save(&path).unwrap();
    let (mut store, mut effects) = SessionStore::load(&path).unwrap();
    store.set_hosts(vec![diri_proto::HostEntry {
        id: "forge".into(),
        name: Some("Forge".into()),
        ssh: "cristi@forge".into(),
        default_cwd: Some("~/code".into()),
        node: None,
    }]);
    store.set_default_spawn_host(Some("forge".into()));
    assert_eq!(store.default_spawn_host().as_deref(), Some("forge"));

    store.set_default_spawn_host(None);

    assert_eq!(store.default_spawn_host(), None);
    assert_eq!(Prefs::load(&path).unwrap().default_spawn_host, None);
    drain(&mut effects);
    store.spawn_default(super::SpawnOptions::default());
    let params = match effects.try_recv() {
        Ok(StoreEffect::Spawn(params)) => params,
        other => panic!("expected spawn effect, got {other:?}"),
    };
    assert_eq!(params.host, None, "⌘T must be local again after a reset");

    let (reloaded, _effects) = SessionStore::load(&path).unwrap();
    assert_eq!(reloaded.default_spawn_host(), None);
}

#[test]
fn migrate_session_guards_kind_target_and_reentry() {
    let mut remote = session("two", "p", 2.0);
    remote.host = Some("forge".into());
    let mut shell = session("three", "p", 1.0);
    shell.kind = AgentKind::SHELL;
    let (mut store, mut effects) = hydrated(
        vec![session("one", "p", 3.0), remote, shell],
        vec![project("p", "P")],
        Prefs::default(),
    );
    drain(&mut effects);

    // Local Claude → forge emits exactly one migrate effect; a second click
    // while in flight is swallowed.
    store.migrate_session(id("one"), Some("forge".into()));
    store.migrate_session(id("one"), Some("forge".into()));
    let emitted = drain(&mut effects);
    assert_eq!(
        emitted,
        vec![StoreEffect::Migrate {
            id: id("one"),
            target_host: Some("forge".into()),
        }]
    );
    assert!(store.migrating().contains(&id("one")));
    store.finish_migration(&id("one"));
    assert!(!store.migrating().contains(&id("one")));

    // No-op moves (already there), non-Claude kinds, and unknown sessions
    // never emit.
    store.migrate_session(id("two"), Some("forge".into()));
    store.migrate_session(id("three"), None);
    store.migrate_session(id("missing"), None);
    assert!(drain(&mut effects).is_empty());

    // Remote Claude → local is eligible.
    store.migrate_session(id("two"), None);
    assert_eq!(
        drain(&mut effects),
        vec![StoreEffect::Migrate {
            id: id("two"),
            target_host: None,
        }]
    );
}

#[test]
fn sync_prefs_emits_once_per_host_until_finished() {
    let (mut store, mut effects) = hydrated(vec![], vec![], Prefs::default());
    store.set_hosts(vec![diri_proto::HostEntry {
        id: "forge".into(),
        name: Some("Forge".into()),
        ssh: "cristi@forge".into(),
        default_cwd: None,
        node: None,
    }]);
    drain(&mut effects);

    store.sync_prefs("forge".into());
    store.sync_prefs("forge".into());
    assert_eq!(
        drain(&mut effects),
        vec![StoreEffect::SyncPrefs {
            host: "forge".into(),
            host_name: "Forge".into(),
        }]
    );
    assert!(store.syncing_prefs().contains("forge"));
    store.finish_prefs_sync("forge");
    store.sync_prefs("forge".into());
    assert_eq!(drain(&mut effects).len(), 1);
}

#[test]
fn repo_targeting_tracks_the_selected_session_and_dedupes_requests() {
    let mut remote = session("one", "p", 2.0);
    remote.host = Some("forge".into());
    let (mut store, mut effects) = hydrated(
        vec![remote, session("two", "p", 1.0)],
        vec![project("p", "P")],
        Prefs {
            default_spawn_host: Some("forge".into()),
            ..Prefs::default()
        },
    );
    store.set_hosts(vec![diri_proto::HostEntry {
        id: "forge".into(),
        name: Some("Forge".into()),
        ssh: "cristi@forge".into(),
        default_cwd: None,
        node: None,
    }]);
    store.select(id("one"));
    drain(&mut effects);

    // Opening the picker restarts repo targeting against the selected session
    // and returns the remembered spawn destination.
    assert_eq!(store.begin_repo_targeting().as_deref(), Some("forge"));
    store.request_repo_target(None);
    store.request_repo_target(None); // deduped while pending
    assert_eq!(
        drain(&mut effects),
        vec![StoreEffect::LocateRepo {
            key: "local".into(),
            host: None,
            session_id: id("one"),
        }]
    );
    assert_eq!(store.repo_target(None), Some(&super::RepoTarget::Pending));

    // The async answer lands under the same key.
    store.set_repo_target(
        "local".into(),
        super::RepoTarget::Resolved("/work/p".into()),
    );
    assert_eq!(
        store.repo_target(None),
        Some(&super::RepoTarget::Resolved("/work/p".into()))
    );

    // Selection changes the repo reference, not the remembered destination.
    store.select(id("two"));
    assert_eq!(store.begin_repo_targeting().as_deref(), Some("forge"));
    assert_eq!(store.repo_target(None), None);
}

#[test]
fn inert_runtime_has_no_background_tasks_or_live_sessions() {
    let runtime = super::StoreRuntime::inert();
    assert!(
        runtime
            .tasks
            .lock()
            .expect("runtime task lock poisoned")
            .is_empty()
    );
    assert!(runtime.snapshots().borrow().sessions.is_empty());
}

#[test]
fn action_retry_policy_only_replays_idempotent_operations() {
    let rename = StoreEffect::Rename {
        id: id("one"),
        title: "New title".to_owned(),
    };
    let sync = StoreEffect::SyncPrefs {
        host: "forge".to_owned(),
        host_name: "Forge".to_owned(),
    };
    assert!(super::action_context(&rename).unwrap().retry.is_some());
    assert!(super::action_context(&sync).unwrap().retry.is_some());

    for unsafe_effect in [
        StoreEffect::Remove(id("one")),
        StoreEffect::Archive(id("one")),
        StoreEffect::Unarchive(id("one")),
        StoreEffect::Migrate {
            id: id("one"),
            target_host: Some("forge".to_owned()),
        },
        StoreEffect::ReopenLast,
    ] {
        assert!(
            super::action_context(&unsafe_effect)
                .expect("user action has failure context")
                .retry
                .is_none(),
            "unsafe operation unexpectedly became replayable: {unsafe_effect:?}"
        );
    }
}
