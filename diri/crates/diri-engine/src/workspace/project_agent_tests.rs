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
    assert_eq!(error.code, "invalid_workspace");
    assert_eq!(std::fs::read(&f.path).unwrap(), before);
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
