use super::tests::Fixture;
use super::*;

fn inventory(session: usize, project: &str) -> ProjectAgentInventory {
    ProjectAgentInventory {
        session_id: SessionId::new(format!("session_{session}")),
        project: Project {
            id: ProjectId(project.into()),
            root: format!("/projects/{project}"),
            name: "same-name".into(),
            pinned_order: None,
            host: None,
        },
        session_projects: (0..12)
            .map(|i| {
                (
                    SessionId::new(format!("session_{i}")),
                    ProjectId(project.into()),
                )
            })
            .collect(),
    }
}
fn open(f: &mut Fixture, context: &ProjectAgentInventory, preferred: Option<WorkspaceId>) {
    f.snapshot = f
        .store
        .apply_with_project_agent(
            WorkspaceMutationParams {
                expected_revision: f.snapshot.revision,
                mutation: WorkspaceMutation::OpenProjectAgent {
                    session_id: context.session_id.clone(),
                    preferred_workspace: preferred,
                },
            },
            &f.sessions,
            Some(context),
        )
        .unwrap();
}

#[test]
fn project_agent_open_is_lazy_identity_bound_and_repeatable_after_restart() {
    let mut f = Fixture::new();
    open(&mut f, &inventory(0, "local"), None);
    let first = f.snapshot.workspaces[0].clone();
    assert_eq!(first.project_id, Some(ProjectId("local".into())));
    assert_eq!(first.tabs.len(), 1);
    assert_eq!(first.tabs[0].title, None);
    f.store = WorkspaceStore::new(&f.path);
    open(&mut f, &inventory(0, "local"), None);
    assert_eq!(
        f.snapshot.workspaces.as_slice(),
        std::slice::from_ref(&first)
    );
    open(&mut f, &inventory(1, "remote"), None);
    assert_eq!(f.snapshot.workspaces.len(), 2);
    assert_eq!(f.snapshot.workspaces[0], first);
    assert_eq!(f.snapshot.workspaces[1].name, first.name);
    assert_ne!(f.snapshot.workspaces[1].project_id, first.project_id);
    assert_ne!(f.snapshot.workspaces[1].id, first.id);
}

#[test]
fn closing_a_tab_stays_closed_when_another_agent_is_opened() {
    let mut f = Fixture::new();
    open(&mut f, &inventory(0, "p"), None);
    let tab = f.snapshot.workspaces[0].tabs[0].id.clone();
    open(&mut f, &inventory(1, "p"), None);
    f.apply(WorkspaceMutation::RemoveTab { tab_id: tab });
    open(&mut f, &inventory(1, "p"), None);
    assert_eq!(f.snapshot.workspaces.len(), 1);
    assert_eq!(f.snapshot.workspaces[0].tabs.len(), 1);
}

#[test]
fn same_project_legacy_split_is_adopted_without_changing_layout_or_titles() {
    let mut f = Fixture::new();
    let workspace = f.create_workspace("My deliberate name");
    let (tab, pane) = f.create_tab(workspace, 0);
    f.apply(WorkspaceMutation::RenameTab {
        tab_id: tab.clone(),
        title: Some("Review split".into()),
    });
    f.apply(WorkspaceMutation::SplitPane {
        tab_id: tab.clone(),
        target: pane.clone(),
        session_id: SessionId::new("session_1"),
        edge: DockEdge::Right,
    });
    f.apply(WorkspaceMutation::ZoomPane {
        tab_id: tab,
        pane_id: Some(pane),
    });
    let original = f.snapshot.workspaces[0].clone();
    open(&mut f, &inventory(1, "p"), None);
    let adopted = &f.snapshot.workspaces[0];
    assert_eq!(f.snapshot.workspaces.len(), 1);
    assert_eq!(adopted.id, original.id);
    assert_eq!(adopted.name, original.name);
    assert_eq!(adopted.tabs[0].layout, original.tabs[0].layout);
    assert_eq!(adopted.tabs[0].title, original.tabs[0].title);
    assert_ne!(adopted.tabs[0].focused_pane, original.tabs[0].focused_pane);
    assert_eq!(adopted.tabs[0].zoomed_pane, None);
    assert_eq!(adopted.project_id, Some(ProjectId("p".into())));
}

#[test]
fn preferred_mixed_layout_is_focused_without_binding_or_duplication() {
    let mut f = Fixture::new();
    let workspace = f.create_workspace("Across projects");
    let (tab, pane) = f.create_tab(workspace.clone(), 0);
    f.apply(WorkspaceMutation::SplitPane {
        tab_id: tab,
        target: pane,
        session_id: SessionId::new("session_1"),
        edge: DockEdge::Bottom,
    });
    let layout = f.snapshot.workspaces[0].tabs[0].layout.clone();
    let mut context = inventory(0, "p");
    context
        .session_projects
        .insert(SessionId::new("session_1"), ProjectId("other".into()));
    open(&mut f, &context, Some(workspace));
    assert_eq!(f.snapshot.workspaces.len(), 1);
    assert_eq!(f.snapshot.workspaces[0].project_id, None);
    assert_eq!(f.snapshot.workspaces[0].tabs[0].layout, layout);
    assert_eq!(
        matching_pane(&f.snapshot.workspaces[0].tabs[0], &context.session_id),
        Some(f.snapshot.workspaces[0].tabs[0].focused_pane.clone())
    );
}

#[test]
fn ambiguous_or_unknown_membership_never_adopts_legacy_layouts() {
    for unknown in [false, true] {
        let mut f = Fixture::new();
        let first = f.create_workspace("First");
        f.create_tab(first.clone(), 0);
        let mut context = inventory(0, "p");
        if unknown {
            f.create_tab(first, 1);
            context
                .session_projects
                .remove(&SessionId::new("session_1"));
        } else {
            let second = f.create_workspace("Second");
            f.create_tab(second, 0);
        }
        let legacy = f.snapshot.workspaces.clone();
        open(&mut f, &context, None);
        assert_eq!(&f.snapshot.workspaces[..legacy.len()], legacy.as_slice());
        assert_eq!(f.snapshot.workspaces.len(), legacy.len() + 1);
        assert_eq!(
            f.snapshot.workspaces.last().unwrap().project_id,
            Some(context.project.id)
        );
    }
}

#[test]
fn duplicate_agent_references_keep_selected_tab_and_focused_pane() {
    let mut f = Fixture::new();
    let workspace = f.create_workspace("View");
    f.create_tab(workspace.clone(), 0);
    let (tab, pane) = f.create_tab(workspace.clone(), 0);
    f.apply(WorkspaceMutation::SplitPane {
        tab_id: tab,
        target: pane,
        session_id: SessionId::new("session_0"),
        edge: DockEdge::Right,
    });
    let original = f.snapshot.workspaces[0].clone();
    open(&mut f, &inventory(0, "p"), Some(workspace));
    assert_eq!(f.snapshot.workspaces, [original]);
}

#[test]
fn project_open_limit_failure_keeps_the_file_unchanged() {
    let mut f = Fixture::new();
    let mut snapshot = WorkspaceSnapshot::default();
    for i in 0..MAX_WORKSPACES {
        snapshot.workspaces.push(WorkspaceRecord {
            project_id: None,
            id: WorkspaceId::new(format!("workspace_{i}")),
            name: format!("Workspace {i}"),
            tabs: vec![],
            selected_tab: None,
        });
    }
    std::fs::write(
        &f.path,
        serde_json::to_vec(&serde_json::json!({"workspaceState": snapshot})).unwrap(),
    )
    .unwrap();
    f.snapshot = snapshot;
    let before = std::fs::read(&f.path).unwrap();
    let context = inventory(0, "p");
    let error = f
        .store
        .apply_with_project_agent(
            WorkspaceMutationParams {
                expected_revision: 0,
                mutation: WorkspaceMutation::OpenProjectAgent {
                    session_id: context.session_id.clone(),
                    preferred_workspace: None,
                },
            },
            &f.sessions,
            Some(&context),
        )
        .unwrap_err();
    assert_eq!(error.code, "workspace_limit_reached");
    assert_eq!(std::fs::read(&f.path).unwrap(), before);
}

#[test]
fn project_open_at_the_workspace_limit_reclaims_empty_project_workspaces() {
    let mut f = Fixture::new();
    let mut snapshot = WorkspaceSnapshot::default();
    for i in 0..MAX_WORKSPACES {
        // Odd ones are a person's own empty workspaces and must survive.
        snapshot.workspaces.push(WorkspaceRecord {
            project_id: (i % 2 == 0).then(|| ProjectId(format!("old_{i}"))),
            id: WorkspaceId::new(format!("workspace_{i}")),
            name: format!("Workspace {i}"),
            tabs: vec![],
            selected_tab: None,
        });
    }
    // A project workspace that still holds a tab is kept too.
    snapshot.workspaces[0]
        .tabs
        .push(single_pane_tab(0, "session_1"));
    snapshot.workspaces[0].selected_tab = Some(TabId::new("tab_0"));
    std::fs::write(
        &f.path,
        serde_json::to_vec(&serde_json::json!({"workspaceState": snapshot})).unwrap(),
    )
    .unwrap();
    f.snapshot = snapshot;
    open(&mut f, &inventory(0, "p"), None);
    let workspaces = &f.snapshot.workspaces;
    assert_eq!(workspaces.len(), 1 + MAX_WORKSPACES / 2 + 1);
    assert_eq!(workspaces[0].id, WorkspaceId::new("workspace_0"));
    assert!(
        workspaces[1..MAX_WORKSPACES / 2 + 1]
            .iter()
            .all(|w| w.project_id.is_none())
    );
    let opened = workspaces.last().unwrap();
    assert_eq!(opened.project_id, Some(ProjectId("p".into())));
    assert_eq!(opened.tabs.len(), 1);
}

#[test]
fn old_workspaces_decode_unbound_and_duplicate_project_bindings_fail_closed() {
    let old = serde_json::json!({"schemaVersion": 1, "revision": 0, "workspaces": [{"id":"old", "name":"Old", "tabs":[], "selectedTab":null}]});
    let mut state: WorkspaceSnapshot = serde_json::from_value(old).unwrap();
    assert_eq!(state.workspaces[0].project_id, None);
    state.workspaces[0].project_id = Some(ProjectId("p".into()));
    let mut duplicate = state.workspaces[0].clone();
    duplicate.id = WorkspaceId::new("another");
    state.workspaces.push(duplicate);
    assert_eq!(validate(&state).unwrap_err().code, "invalid_workspace");
}

fn single_pane_tab(tab: usize, session: &str) -> WorkspaceTab {
    let pane = PaneId::new(format!("pane_{tab}"));
    WorkspaceTab {
        id: TabId::new(format!("tab_{tab}")),
        title: None,
        layout: LayoutNode::Pane {
            id: pane.clone(),
            session_id: SessionId::new(session),
        },
        focused_pane: pane,
        zoomed_pane: None,
    }
}

/// Tabs outlive their deleted sessions, so a long-used install reaches the
/// tab limit with placements nobody can use (228 of 256 on the report).
fn full_of_deleted_sessions(f: &mut Fixture) {
    let mut tabs: Vec<_> = (0..MAX_WORKSPACE_TABS - 1)
        .map(|i| single_pane_tab(i, &format!("deleted_{i}")))
        .collect();
    // A split that keeps one live pane is still a usable layout.
    let mut split = single_pane_tab(MAX_WORKSPACE_TABS, "session_1");
    split.layout = LayoutNode::Split {
        id: SplitId::new("split_mixed"),
        axis: LayoutAxis::Horizontal,
        fraction: 0.5,
        first: Box::new(split.layout),
        second: Box::new(LayoutNode::Pane {
            id: PaneId::new("pane_gone"),
            session_id: SessionId::new("deleted_split"),
        }),
    };
    tabs.push(split);
    let snapshot = WorkspaceSnapshot {
        workspaces: vec![WorkspaceRecord {
            project_id: Some(ProjectId("p".into())),
            id: WorkspaceId::new("workspace_p"),
            name: "Project".into(),
            selected_tab: Some(tabs[0].id.clone()),
            tabs,
        }],
        ..WorkspaceSnapshot::default()
    };
    std::fs::write(
        &f.path,
        serde_json::to_vec(&serde_json::json!({"workspaceState": snapshot})).unwrap(),
    )
    .unwrap();
    f.snapshot = f.store.snapshot().unwrap();
    assert_eq!(f.snapshot.workspaces[0].tabs.len(), MAX_WORKSPACE_TABS);
}

#[test]
fn opening_an_agent_at_the_tab_limit_drops_tabs_of_deleted_sessions() {
    let mut f = Fixture::new();
    full_of_deleted_sessions(&mut f);
    open(&mut f, &inventory(0, "p"), None);
    let workspace = &f.snapshot.workspaces[0];
    let ids: Vec<_> = workspace.tabs.iter().map(|tab| tab.id.0.as_str()).collect();
    assert_eq!(ids.len(), 2, "{ids:?}");
    assert_eq!(ids[0], format!("tab_{MAX_WORKSPACE_TABS}"));
    assert!(matches!(
        &workspace.tabs[0].layout,
        LayoutNode::Split { .. }
    ));
    assert_eq!(workspace.selected_tab.as_ref(), Some(&workspace.tabs[1].id));
    assert!(matches!(
        &workspace.tabs[1].layout,
        LayoutNode::Pane { session_id, .. } if session_id.0 == "session_0"
    ));
    assert_eq!(f.store.snapshot().unwrap(), f.snapshot);
}

#[test]
fn placing_a_spawned_agent_at_the_tab_limit_drops_tabs_of_deleted_sessions() {
    let mut f = Fixture::new();
    full_of_deleted_sessions(&mut f);
    f.apply(WorkspaceMutation::CreateTab {
        select: false,
        workspace_id: WorkspaceId::new("workspace_p"),
        session_id: SessionId::new("session_2"),
        title: None,
    });
    let workspace = &f.snapshot.workspaces[0];
    assert_eq!(workspace.tabs.len(), 2);
    // The selected tab was an unavailable placement; selection falls back.
    assert_eq!(workspace.selected_tab.as_ref(), Some(&workspace.tabs[0].id));
}

#[test]
fn below_the_limit_unavailable_tabs_are_retained_and_can_be_closed() {
    let mut f = Fixture::new();
    full_of_deleted_sessions(&mut f);
    f.apply(WorkspaceMutation::RemoveTab {
        tab_id: TabId::new("tab_7"),
    });
    // Below the limit unavailable placements are retained as they were.
    let tabs = &f.snapshot.workspaces[0].tabs;
    assert_eq!(tabs.len(), MAX_WORKSPACE_TABS - 1);
    assert!(!tabs.iter().any(|tab| tab.id.0 == "tab_7"));
}

#[test]
fn a_tab_limit_of_live_placements_is_reported_as_a_limit() {
    let mut f = Fixture::new();
    let tabs: Vec<_> = (0..MAX_WORKSPACE_TABS)
        .map(|i| single_pane_tab(i, "session_1"))
        .collect();
    let snapshot = WorkspaceSnapshot {
        workspaces: vec![WorkspaceRecord {
            project_id: Some(ProjectId("p".into())),
            id: WorkspaceId::new("workspace_p"),
            name: "Project".into(),
            selected_tab: Some(tabs[0].id.clone()),
            tabs,
        }],
        ..WorkspaceSnapshot::default()
    };
    std::fs::write(
        &f.path,
        serde_json::to_vec(&serde_json::json!({"workspaceState": snapshot})).unwrap(),
    )
    .unwrap();
    f.snapshot = f.store.snapshot().unwrap();
    let before = std::fs::read(&f.path).unwrap();
    let context = inventory(0, "p");
    let error = f
        .store
        .apply_with_project_agent(
            WorkspaceMutationParams {
                expected_revision: f.snapshot.revision,
                mutation: WorkspaceMutation::OpenProjectAgent {
                    session_id: context.session_id.clone(),
                    preferred_workspace: None,
                },
            },
            &f.sessions,
            Some(&context),
        )
        .unwrap_err();
    assert_eq!(error.code, "workspace_limit_reached");
    assert_eq!(std::fs::read(&f.path).unwrap(), before);
}
