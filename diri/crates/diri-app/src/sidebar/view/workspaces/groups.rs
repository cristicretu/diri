//! Window-local navigation projection over the authoritative workspace catalog.
use super::*;
use diri_proto::workspace::WorkspaceSnapshot;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum WorkspaceRowKey {
    Heading(WorkspaceId),
    Tab(WorkspaceId, TabId),
}
impl WorkspaceRowKey {
    pub(super) fn workspace(&self) -> &WorkspaceId {
        match self {
            Self::Heading(id) | Self::Tab(id, _) => id,
        }
    }
}

pub(super) struct TabRow {
    pub id: TabId,
    pub title: String,
    pub index: usize,
}
pub(super) struct WorkspaceGroup {
    pub id: WorkspaceId,
    pub name: String,
    pub selected: Option<TabId>,
    pub collapsed: bool,
    pub tabs: Vec<TabRow>,
    pub total: usize,
}
impl WorkspaceGroup {
    pub(super) fn row_keys(&self) -> impl Iterator<Item = WorkspaceRowKey> + '_ {
        std::iter::once(WorkspaceRowKey::Heading(self.id.clone())).chain(
            self.tabs
                .iter()
                .filter(|_| !self.collapsed)
                .map(|tab| WorkspaceRowKey::Tab(self.id.clone(), tab.id.clone())),
        )
    }
}

pub(super) fn project_groups(
    snapshot: &WorkspaceSnapshot,
    store: &SessionStore,
    query: &str,
) -> Vec<WorkspaceGroup> {
    let filtering = !query.trim().is_empty();
    snapshot
        .workspaces
        .iter()
        .map(|workspace| {
            let collapsed = !filtering
                && store
                    .preferences()
                    .sidebar_collapsed_workspaces
                    .contains(&workspace.id);
            let tabs = workspace
                .tabs
                .iter()
                .enumerate()
                .filter_map(|(index, tab)| {
                    let title = tab_title(tab, store);
                    let highlight = crate::sidebar::filter::label_match(&title, query);
                    (!filtering || highlight.is_some()).then(|| TabRow {
                        id: tab.id.clone(),
                        title,
                        index,
                    })
                })
                .collect();
            WorkspaceGroup {
                id: workspace.id.clone(),
                name: workspace.name.clone(),
                selected: workspace.selected_tab.clone(),
                collapsed,
                tabs,
                total: workspace.tabs.len(),
            }
        })
        .collect()
}

impl Sidebar {
    pub(super) fn workspace_heading(
        &self,
        group: &WorkspaceGroup,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = group.id.clone();
        let fold_id = id.clone();
        let destination = id.clone();
        let index = group.total;
        let active = self.workspace_nav.active.as_ref() == Some(&id);
        let count = if self.filter_query.text().trim().is_empty() {
            group.total.to_string()
        } else {
            format!("{}/{}", group.tabs.len(), group.total)
        };
        div()
            .id(SharedString::from(format!("workspace-heading-{}", id.0)))
            .debug_selector({
                let key = format!("workspace-heading-{}", id.0);
                move || key.clone()
            })
            .role(Role::Button)
            .aria_label(format!("Workspace {}, {count} tabs", group.name))
            .h(px(32.0))
            .flex_none()
            .flex()
            .items_center()
            .gap(px(6.0))
            .px(px(6.0))
            .rounded(px(6.0))
            .text_size(px(11.0))
            .font_weight(if active {
                FontWeight::SEMIBOLD
            } else {
                FontWeight::MEDIUM
            })
            .text_color(if active {
                colors.primary
            } else {
                colors.secondary
            })
            .cursor_pointer()
            .border_1()
            .border_color(
                if self.workspace_nav.cursor.as_ref() == Some(&WorkspaceRowKey::Heading(id.clone()))
                {
                    colors.primary.alpha(0.22)
                } else {
                    colors.primary.alpha(0.0)
                },
            )
            .hover(move |row| row.bg(colors.primary.alpha(0.04)))
            .child(if !self.filter_query.text().trim().is_empty() {
                div().size(px(18.0)).flex_none().into_any_element()
            } else {
                div()
                    .id(SharedString::from(format!("workspace-fold-{}", id.0)))
                    .debug_selector({
                        let key = format!("workspace-fold-{}", id.0);
                        move || key.clone()
                    })
                    .role(Role::Button)
                    .aria_label(format!(
                        "{} workspace {}",
                        if group.collapsed {
                            "Expand"
                        } else {
                            "Collapse"
                        },
                        group.name
                    ))
                    .aria_expanded(!group.collapsed)
                    .size(px(18.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(sf_symbol(
                        if group.collapsed {
                            "chevron.right"
                        } else {
                            "chevron.down"
                        },
                        8.0,
                        colors.tertiary,
                    ))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.toggle_workspace_collapsed(fold_id.clone(), cx);
                        cx.stop_propagation();
                    }))
                    .into_any_element()
            })
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .overflow_hidden()
                    .text_ellipsis()
                    .child(group.name.clone()),
            )
            .child(div().text_color(colors.tertiary).child(count))
            .drag_over::<DraggedWorkspaceTab>(move |row, _, _, _| {
                row.bg(colors.primary.alpha(0.10))
            })
            .on_drop(
                cx.listener(move |this, dragged: &DraggedWorkspaceTab, _, cx| {
                    this.move_workspace_tab(dragged, destination.clone(), index, cx)
                }),
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                this.workspace_nav.cursor = Some(WorkspaceRowKey::Heading(id.clone()));
                this.activate_workspace(Some(id.clone()), cx);
            }))
            .into_any_element()
    }

    fn toggle_workspace_collapsed(&mut self, id: WorkspaceId, cx: &mut Context<Self>) {
        if !self.filter_query.text().trim().is_empty() {
            return;
        }
        let mut store = self.store.write().expect("store");
        let live = store
            .workspace_catalog()
            .snapshot()
            .map(|snapshot| {
                snapshot
                    .workspaces
                    .iter()
                    .map(|workspace| workspace.id.clone())
                    .collect::<std::collections::HashSet<_>>()
            })
            .unwrap_or_default();
        if !live.contains(&id) {
            return;
        }
        if let Err(error) = store.update_preferences(|prefs| {
            prefs
                .sidebar_collapsed_workspaces
                .retain(|id| live.contains(id));
            if prefs.sidebar_collapsed_workspaces.contains(&id) {
                prefs
                    .sidebar_collapsed_workspaces
                    .retain(|current| current != &id);
            } else {
                prefs.sidebar_collapsed_workspaces.push(id.clone());
            }
        }) {
            eprintln!("diri: could not remember workspace disclosure: {error}");
        }
        drop(store);
        self.workspace_nav.cursor = Some(WorkspaceRowKey::Heading(id));
        cx.notify();
    }

    pub(super) fn request_workspace_tab(
        &mut self,
        workspace: WorkspaceId,
        tab: TabId,
        cx: &mut Context<Self>,
    ) {
        self.workspace_nav.cursor = Some(WorkspaceRowKey::Tab(workspace.clone(), tab.clone()));
        let (already_selected, revision) = {
            let store = self.store.read().expect("store");
            let Some(snapshot) = store.workspace_catalog().snapshot() else {
                return;
            };
            (
                snapshot
                    .workspaces
                    .iter()
                    .find(|group| group.id == workspace)
                    .is_some_and(|group| group.selected_tab.as_ref() == Some(&tab)),
                snapshot.revision,
            )
        };
        if already_selected {
            if self.workspace_nav.active.as_ref() != Some(&workspace) {
                self.activate_workspace(Some(workspace), cx);
            } else {
                cx.emit(SidebarEvent::WorkspaceTabActivated);
            }
        } else if self
            .store
            .write()
            .expect("store")
            .edit_workspace(WorkspaceMutation::SelectTab {
                workspace_id: workspace.clone(),
                tab_id: tab.clone(),
            })
        {
            self.workspace_nav.pending_activation = Some((workspace, tab, revision + 1));
        }
        cx.notify();
    }

    pub(super) fn reconcile_workspace_activation(&mut self, cx: &mut Context<Self>) {
        let Some((workspace, tab, revision)) = self.workspace_nav.pending_activation.clone() else {
            return;
        };
        let outcome = {
            let store = self.store.read().expect("store");
            let catalog = store.workspace_catalog();
            if !catalog.can_edit() {
                return;
            }
            catalog.error.is_none()
                && catalog.snapshot().is_some_and(|snapshot| {
                    snapshot.revision >= revision
                        && snapshot
                            .workspaces
                            .iter()
                            .find(|group| group.id == workspace)
                            .is_some_and(|group| group.selected_tab.as_ref() == Some(&tab))
                })
        };
        self.workspace_nav.pending_activation = None;
        if outcome {
            if self.workspace_nav.active.as_ref() != Some(&workspace) {
                self.activate_workspace(Some(workspace), cx);
            } else {
                cx.emit(SidebarEvent::WorkspaceTabActivated);
                cx.notify();
            }
        }
    }

    fn workspace_row_keys(&self) -> Vec<WorkspaceRowKey> {
        let store = self.store.read().expect("store");
        store
            .workspace_catalog()
            .snapshot()
            .map(|snapshot| {
                project_groups(snapshot, &store, self.filter_query.text())
                    .iter()
                    .flat_map(|group| group.row_keys())
                    .collect()
            })
            .unwrap_or_default()
    }

    pub(crate) fn focus_workspace_rows(&mut self) {
        let keys = self.workspace_row_keys();
        if self
            .workspace_nav
            .cursor
            .as_ref()
            .is_none_or(|key| !keys.contains(key))
        {
            self.workspace_nav.cursor = self
                .workspace_record()
                .and_then(|workspace| {
                    workspace
                        .selected_tab
                        .map(|tab| WorkspaceRowKey::Tab(workspace.id, tab))
                })
                .filter(|key| keys.contains(key))
                .or_else(|| keys.first().cloned());
        }
        if let Some(index) = self
            .workspace_nav
            .cursor
            .as_ref()
            .and_then(|key| keys.iter().position(|row| row == key))
        {
            self.workspace_nav.vertical_scroll.scroll_to_item(index);
        }
    }

    pub(crate) fn workspace_navigation_key(
        &mut self,
        event: &gpui::KeyDownEvent,
        cx: &mut Context<Self>,
    ) -> bool {
        self.focus_workspace_rows();
        let keys = self.workspace_row_keys();
        let Some(current) = self.workspace_nav.cursor.clone() else {
            return false;
        };
        match event.keystroke.key.as_str() {
            "up" | "down" => {
                let index = keys.iter().position(|key| key == &current).unwrap_or(0);
                let next = if event.keystroke.key == "up" {
                    index.saturating_sub(1)
                } else {
                    (index + 1).min(keys.len().saturating_sub(1))
                };
                self.workspace_nav.cursor = keys.get(next).cloned();
                self.workspace_nav.vertical_scroll.scroll_to_item(next);
            }
            "left" | "right" => {
                let id = current.workspace().clone();
                let collapsed = self
                    .store
                    .read()
                    .expect("store")
                    .preferences()
                    .sidebar_collapsed_workspaces
                    .contains(&id);
                if collapsed == (event.keystroke.key == "right") {
                    self.toggle_workspace_collapsed(id, cx);
                }
            }
            "enter" => match current {
                WorkspaceRowKey::Heading(id) => self.activate_workspace(Some(id), cx),
                WorkspaceRowKey::Tab(workspace, tab) => {
                    self.request_workspace_tab(workspace, tab, cx)
                }
            },
            "escape" => cx.emit(SidebarEvent::WorkspaceTabActivated),
            _ => return false,
        }
        cx.stop_propagation();
        cx.notify();
        true
    }
}

#[cfg(all(test, target_os = "macos"))]
impl Sidebar {
    pub(crate) fn seed_workspace_filter_for_test(&mut self, query: &str, cx: &mut Context<Self>) {
        self.filter_open = true;
        self.filter_query.clear();
        self.filter_query.insert(query);
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use diri_proto::workspace::{LayoutNode, WorkspaceTab};
    fn snapshot() -> WorkspaceSnapshot {
        let tab = |id: &str, title: &str| WorkspaceTab {
            id: TabId::new(id),
            title: Some(title.into()),
            focused_pane: PaneId::new(format!("pane-{id}")),
            zoomed_pane: None,
            layout: LayoutNode::Pane {
                id: PaneId::new(format!("pane-{id}")),
                session_id: SessionId::new("preview-claude"),
            },
        };
        WorkspaceSnapshot {
            workspaces: vec![
                WorkspaceRecord {
                    id: WorkspaceId::new("release"),
                    name: "Release".into(),
                    selected_tab: Some(TabId::new("build")),
                    tabs: vec![tab("build", "Build frontend"), tab("ship", "Ship café")],
                },
                WorkspaceRecord {
                    id: WorkspaceId::new("remote"),
                    name: "Remote review".into(),
                    selected_tab: Some(TabId::new("logs")),
                    tabs: vec![tab("logs", "Watch server logs")],
                },
            ],
            revision: 7,
            ..Default::default()
        }
    }
    #[gpui::test]
    fn cross_workspace_selection_waits_for_ack_and_rejects_stale_activation(
        cx: &mut gpui::TestAppContext,
    ) {
        let sidebar =
            cx.new(|cx| Sidebar::new(None, true, crate::sidebar::PreviewScenario::Typical, cx));
        let mut initial = snapshot();
        let mut second = initial.workspaces[1].tabs[0].clone();
        second.id = TabId::new("second");
        second.focused_pane = PaneId::new("second-pane");
        second.layout = LayoutNode::Pane {
            id: second.focused_pane.clone(),
            session_id: SessionId::new("preview-codex"),
        };
        initial.workspaces[1].selected_tab = Some(second.id.clone());
        initial.workspaces[1].tabs.push(second);
        sidebar.update(cx, |sidebar, cx| {
            sidebar
                .store
                .write()
                .unwrap()
                .seed_workspace_snapshot_for_test(initial.clone());
            sidebar.workspace_nav.active = Some(WorkspaceId::new("release"));
            sidebar.request_workspace_tab(WorkspaceId::new("remote"), TabId::new("logs"), cx);
            assert_eq!(
                sidebar.workspace_nav.active,
                Some(WorkspaceId::new("release"))
            );
            assert!(sidebar.workspace_nav.pending_activation.is_some());
            let mut acknowledged = initial.clone();
            acknowledged.revision += 1;
            acknowledged.workspaces[1].selected_tab = Some(TabId::new("logs"));
            sidebar
                .store
                .write()
                .unwrap()
                .finish_workspace_edit_for_test(acknowledged);
            sidebar.reconcile_workspace_activation(cx);
            assert_eq!(
                sidebar.workspace_nav.active,
                Some(WorkspaceId::new("remote"))
            );
            assert!(sidebar.workspace_nav.pending_activation.is_none());
            sidebar
                .store
                .write()
                .unwrap()
                .seed_workspace_snapshot_for_test(initial.clone());
            sidebar.workspace_nav.active = Some(WorkspaceId::new("release"));
            sidebar.request_workspace_tab(WorkspaceId::new("remote"), TabId::new("logs"), cx);
            let mut conflicting = initial.clone();
            conflicting.revision += 1;
            sidebar
                .store
                .write()
                .unwrap()
                .finish_workspace_edit_for_test(conflicting);
            sidebar.reconcile_workspace_activation(cx);
            assert_eq!(
                sidebar.workspace_nav.active,
                Some(WorkspaceId::new("release")),
                "another selection cannot acknowledge this request"
            );
            assert!(sidebar.workspace_nav.pending_activation.is_none());
        });
    }

    #[test]
    fn filtering_keeps_headings_order_active_work_and_saved_folds() {
        let fixture =
            crate::sidebar::SidebarPreviewFixture::make(crate::sidebar::PreviewScenario::Typical);
        let (mut store, _) = SessionStore::headless(fixture.prefs);
        store.hydrate(fixture.list);
        store.select(SessionId::new("preview-claude"));
        store
            .update_preferences(|prefs| {
                prefs
                    .sidebar_collapsed_workspaces
                    .push(WorkspaceId::new("release"))
            })
            .unwrap();
        let prefs = store.preferences().clone();
        let records = store.sessions().clone();
        let snapshot = snapshot();
        let groups = project_groups(&snapshot, &store, "CAFÉ");
        assert_eq!(groups.len(), 2, "headings remain visible");
        assert_eq!(groups[0].name, "Release");
        assert_eq!(
            groups[0].selected,
            Some(TabId::new("build")),
            "hidden selection stays selected"
        );
        assert!(
            !groups[0].collapsed,
            "search reveals matches without rewriting disclosure"
        );
        assert_eq!(
            groups[0]
                .tabs
                .iter()
                .map(|row| (row.id.clone(), row.index))
                .collect::<Vec<_>>(),
            [(TabId::new("ship"), 1)]
        );
        assert!(groups[1].tabs.is_empty());
        assert_eq!(store.preferences(), &prefs);
        assert_eq!(store.sessions(), &records);
        assert_eq!(
            store.selected_session_id(),
            Some(&SessionId::new("preview-claude"))
        );
        let restored = project_groups(&snapshot, &store, "");
        assert!(restored[0].collapsed);
        assert_eq!(restored[0].tabs.len(), 2);
        assert_eq!(
            restored[0].row_keys().collect::<Vec<_>>(),
            [WorkspaceRowKey::Heading(WorkspaceId::new("release"))]
        );
        let restored_prefs: crate::store::Prefs =
            serde_json::from_str(&serde_json::to_string(&prefs).unwrap()).unwrap();
        assert_eq!(
            restored_prefs.sidebar_collapsed_workspaces,
            [WorkspaceId::new("release")]
        );
        assert!(
            serde_json::from_str::<crate::store::Prefs>("{}")
                .unwrap()
                .sidebar_collapsed_workspaces
                .is_empty()
        );
    }
}
