use super::*;
use diri_proto::workspace::{
    DockEdge, PaneId, TabId, WorkspaceId, WorkspaceMutation, WorkspaceRecord,
};

#[derive(Clone)]
struct DraggedWorkspaceTab {
    tab: TabId,
    revision: u64,
}
impl Render for DraggedWorkspaceTab {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .px(px(10.0))
            .py(px(6.0))
            .rounded(px(6.0))
            .bg(gpui::rgba(0x34363aff))
            .text_color(gpui::white())
            .child("Move tab")
    }
}

#[derive(Clone)]
pub(super) enum WorkspaceEditor {
    Create,
    Rename(WorkspaceId),
    RenameTab(TabId),
}
#[derive(Clone)]
pub(super) enum SessionDestination {
    Tab(WorkspaceId),
    Split {
        tab: TabId,
        pane: PaneId,
        edge: DockEdge,
    },
}
pub(super) struct WorkspaceNavigation {
    pub active: Option<WorkspaceId>,
    menu: bool,
    editor: Option<WorkspaceEditor>,
    destination: Option<SessionDestination>,
    query: query_editor::QueryEditor,
    focus: FocusHandle,
    awaiting_create: bool,
    scroll: ScrollHandle,
    vertical_scroll: ScrollHandle,
    last_selection: Option<(WorkspaceId, Option<TabId>, bool, u32)>,
    pub(super) available_width: f32,
}
impl WorkspaceNavigation {
    pub fn new(cx: &mut App, active: Option<WorkspaceId>) -> Self {
        Self {
            active,
            menu: false,
            editor: None,
            destination: None,
            query: Default::default(),
            focus: cx.focus_handle(),
            awaiting_create: false,
            scroll: ScrollHandle::new(),
            vertical_scroll: ScrollHandle::new(),
            last_selection: None,
            available_width: 0.0,
        }
    }
}

fn tab_title(tab: &diri_proto::workspace::WorkspaceTab, store: &SessionStore) -> String {
    if let Some(title) = &tab.title {
        return title.clone();
    }
    fn first_session(node: &diri_proto::workspace::LayoutNode) -> &SessionId {
        match node {
            diri_proto::workspace::LayoutNode::Pane { session_id, .. } => session_id,
            diri_proto::workspace::LayoutNode::Split { first, .. } => first_session(first),
        }
    }
    store
        .sessions()
        .get(first_session(&tab.layout))
        .map(|session| display_title(session))
        .unwrap_or_else(|| "Unavailable session".into())
}

impl Sidebar {
    fn move_workspace_tab(
        &mut self,
        dragged: &DraggedWorkspaceTab,
        workspace: WorkspaceId,
        index: usize,
        cx: &mut Context<Self>,
    ) {
        let mut store = self.store.write().expect("store");
        if store
            .workspace_catalog()
            .snapshot()
            .map(|snapshot| snapshot.revision)
            == Some(dragged.revision)
        {
            store.edit_workspace(WorkspaceMutation::MoveTab {
                tab_id: dragged.tab.clone(),
                workspace_id: workspace,
                index,
            });
        }
        cx.stop_propagation();
        cx.notify();
    }

    pub(crate) fn activate_workspace(&mut self, id: Option<WorkspaceId>, cx: &mut Context<Self>) {
        self.workspace_nav.active = id.clone();
        if let Err(error) = self
            .store
            .write()
            .expect("store")
            .update_preferences(|prefs| prefs.active_workspace = id.clone())
        {
            eprintln!("diri: could not remember workspace selection: {error}");
        }
        self.workspace_nav.awaiting_create = false;
        self.workspace_nav.menu = false;
        self.workspace_nav.destination = None;
        self.workspace_nav.editor = None;
        cx.emit(SidebarEvent::WorkspaceActivated(id));
        cx.notify();
    }
    pub(crate) fn choose_split_session(
        &mut self,
        tab: TabId,
        pane: PaneId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.workspace_nav.destination = Some(SessionDestination::Split {
            tab,
            pane,
            edge: DockEdge::Right,
        });
        self.workspace_nav.menu = true;
        self.workspace_nav.query.clear();
        self.workspace_nav.focus.focus(window, cx);
        self.peek(window, cx);
        cx.notify();
    }
    pub(super) fn reconcile_workspace_navigation(&mut self, cx: &mut Context<Self>) {
        let (created, active_exists, ready) = {
            let store = self.store.read().expect("store");
            let catalog = store.workspace_catalog();
            (
                catalog.created_workspace.clone(),
                self.workspace_nav.active.as_ref().is_none_or(|id| {
                    catalog.snapshot().is_some_and(|snapshot| {
                        snapshot
                            .workspaces
                            .iter()
                            .any(|workspace| &workspace.id == id)
                    })
                }),
                catalog.can_edit(),
            )
        };
        if self.workspace_nav.awaiting_create
            && let Some(created) = created
        {
            self.workspace_nav.awaiting_create = false;
            self.activate_workspace(Some(created), cx);
        } else if ready && !active_exists {
            self.activate_workspace(None, cx);
        }
    }
    fn begin_workspace_editor(
        &mut self,
        editor: WorkspaceEditor,
        value: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.workspace_nav.editor = Some(editor);
        self.workspace_nav.query.clear();
        self.workspace_nav.query.insert(value);
        self.workspace_nav.menu = true;
        self.workspace_nav.focus.focus(window, cx);
        cx.notify();
    }
    pub(super) fn workspace_key(
        &mut self,
        event: &gpui::KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.workspace_nav.menu || !self.workspace_nav.focus.is_focused(window) {
            return false;
        }
        match event.keystroke.key.as_str() {
            "escape" => {
                self.workspace_nav.menu = false;
                self.workspace_nav.editor = None;
                self.workspace_nav.destination = None;
                self.focus_handle.focus(window, cx);
            }
            "enter" if self.workspace_nav.editor.is_some() => {
                let name = self.workspace_nav.query.text().trim().to_owned();
                if name.is_empty() {
                    return true;
                }
                let editor = self.workspace_nav.editor.clone().unwrap();
                let creating = matches!(&editor, WorkspaceEditor::Create);
                let mutation = match editor {
                    WorkspaceEditor::Create => WorkspaceMutation::CreateWorkspace { name },
                    WorkspaceEditor::Rename(workspace_id) => {
                        WorkspaceMutation::RenameWorkspace { workspace_id, name }
                    }
                    WorkspaceEditor::RenameTab(tab_id) => WorkspaceMutation::RenameTab {
                        tab_id,
                        title: Some(name),
                    },
                };
                if self.store.write().expect("store").edit_workspace(mutation) {
                    self.workspace_nav.editor = None;
                    self.workspace_nav.awaiting_create = creating;
                    self.workspace_nav.query.clear();
                }
            }
            _ => {
                let Some(edit) = query_editor::edit_for(&event.keystroke) else {
                    return false;
                };
                match edit {
                    Edit::Local(edit) => {
                        self.workspace_nav.query.apply(edit);
                    }
                    Edit::Clipboard(ClipboardEdit::Copy) => {
                        query_editor::copy_selection(&self.workspace_nav.query, cx)
                    }
                    Edit::Clipboard(ClipboardEdit::Cut) => {
                        query_editor::cut_selection(&mut self.workspace_nav.query, cx);
                    }
                    Edit::Clipboard(ClipboardEdit::Paste) => {
                        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                            self.workspace_nav.query.insert(&text);
                        }
                    }
                }
            }
        }
        cx.stop_propagation();
        cx.notify();
        true
    }
    pub(super) fn workspace_control(
        &self,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let label = self
            .workspace_nav
            .active
            .as_ref()
            .and_then(|id| {
                self.store
                    .read()
                    .expect("store")
                    .workspace_catalog()
                    .snapshot()
                    .and_then(|snapshot| {
                        snapshot
                            .workspaces
                            .iter()
                            .find(|workspace| &workspace.id == id)
                    })
                    .map(|workspace| workspace.name.clone())
            })
            .unwrap_or_else(|| "All sessions".into());
        div()
            .id("workspace-picker")
            .debug_selector(|| "workspace-picker".into())
            .role(Role::Button)
            .aria_label("Choose workspace")
            .h(px(30.0))
            .px(px(9.0))
            .flex()
            .items_center()
            .gap(px(7.0))
            .min_w(px(0.0))
            .rounded(px(7.0))
            .cursor_pointer()
            .hover(move |row| row.bg(colors.primary.alpha(0.06)))
            .child(sf_symbol("square.stack.3d.up", 12.0, colors.secondary))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .overflow_hidden()
                    .text_ellipsis()
                    .text_size(px(Typo::META.size))
                    .child(label),
            )
            .child(sf_symbol("chevron.down", 8.0, colors.tertiary))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(|this, _, window, cx| {
                this.workspace_nav.menu = !this.workspace_nav.menu;
                this.workspace_nav.query.clear();
                this.workspace_nav.editor = None;
                this.workspace_nav.destination = None;
                this.workspace_nav.focus.focus(window, cx);
                this.peek(window, cx);
                cx.notify();
            }))
            .into_any_element()
    }
    pub(super) fn workspace_record(&self) -> Option<WorkspaceRecord> {
        let id = self.workspace_nav.active.as_ref()?;
        self.store
            .read()
            .expect("store")
            .workspace_catalog()
            .snapshot()?
            .workspaces
            .iter()
            .find(|workspace| &workspace.id == id)
            .cloned()
    }
    pub(super) fn workspace_rows(
        &mut self,
        horizontal: bool,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(workspace) = self.workspace_record() else {
            return div()
                .p(px(14.0))
                .text_color(colors.secondary)
                .child("Loading workspace…")
                .into_any_element();
        };
        let fingerprint = (
            workspace.id.clone(),
            workspace.selected_tab.clone(),
            horizontal,
            self.workspace_nav.available_width.to_bits(),
        );
        if self.workspace_nav.last_selection.as_ref() != Some(&fingerprint) {
            if let Some(index) = workspace
                .tabs
                .iter()
                .position(|tab| workspace.selected_tab.as_ref() == Some(&tab.id))
            {
                if horizontal {
                    self.workspace_nav
                        .scroll
                        .set_offset(point(px(-(index as f32) * 167.0), px(0.0)));
                } else {
                    self.workspace_nav
                        .vertical_scroll
                        .set_offset(point(px(0.0), px(-(index as f32) * 35.0)));
                }
            }
            self.workspace_nav.last_selection = Some(fingerprint);
        }
        let store = self.store.read().expect("store");
        let mut rows = div()
            .id("workspace-tabs")
            .role(Role::TabList)
            .aria_label("Workspace tabs")
            .flex()
            .gap(px(3.0));
        if horizontal {
            rows = rows
                .flex_1()
                .min_w(px(0.0))
                .overflow_x_scroll()
                .track_scroll(&self.workspace_nav.scroll);
        } else {
            rows = rows
                .flex_col()
                .flex_1()
                .min_h(px(0.0))
                .overflow_y_scroll()
                .track_scroll(&self.workspace_nav.vertical_scroll);
        }
        for (index, tab) in workspace.tabs.iter().enumerate() {
            let id = tab.id.clone();
            let workspace_id = workspace.id.clone();
            let active = workspace.selected_tab.as_ref() == Some(&tab.id);
            let title = tab_title(tab, &store);
            let rename_id = id.clone();
            let rename_title = title.clone();
            let source = DraggedWorkspaceTab {
                tab: id.clone(),
                revision: store
                    .workspace_catalog()
                    .snapshot()
                    .map_or(0, |snapshot| snapshot.revision),
            };
            let destination = workspace.id.clone();
            let remove = id.clone();
            let mut row = div()
                .id(SharedString::from(format!("workspace-tab-{}", id.0)))
                .role(Role::Tab)
                .aria_label(title.clone())
                .h(px(32.0))
                .px(px(9.0))
                .flex_none()
                .flex()
                .items_center()
                .gap(px(6.0))
                .rounded(px(7.0))
                .cursor_pointer()
                .bg(if active {
                    colors.primary.alpha(0.08)
                } else {
                    colors.primary.alpha(0.0)
                })
                .hover(move |row| row.bg(colors.primary.alpha(0.06)))
                .child(sf_symbol(
                    "rectangle",
                    11.0,
                    if active {
                        colors.primary
                    } else {
                        colors.secondary
                    },
                ))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .overflow_hidden()
                        .text_ellipsis()
                        .text_size(px(Typo::ROW.size))
                        .child(title),
                )
                .child(
                    div()
                        .id(SharedString::from(format!(
                            "close-workspace-tab-{}",
                            remove.0
                        )))
                        .role(Role::Button)
                        .aria_label("Remove tab from workspace")
                        .size(px(14.0))
                        .flex_none()
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(sf_symbol("xmark", 8.0, colors.tertiary))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.store.write().expect("store").edit_workspace(
                                WorkspaceMutation::RemoveTab {
                                    tab_id: remove.clone(),
                                },
                            );
                            cx.stop_propagation();
                            cx.notify();
                        })),
                )
                .on_drag(source, |source, _, _, cx| cx.new(|_| source.clone()))
                .drag_over::<DraggedWorkspaceTab>(move |row, _, _, _| {
                    row.bg(colors.primary.alpha(0.12))
                })
                .on_drop(
                    cx.listener(move |this, dragged: &DraggedWorkspaceTab, _, cx| {
                        this.move_workspace_tab(dragged, destination.clone(), index, cx);
                    }),
                )
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_click(
                    cx.listener(move |this, event: &gpui::ClickEvent, window, cx| {
                        if event.click_count() == 2 {
                            this.begin_workspace_editor(
                                WorkspaceEditor::RenameTab(rename_id.clone()),
                                &rename_title,
                                window,
                                cx,
                            );
                        } else {
                            this.store.write().expect("store").edit_workspace(
                                WorkspaceMutation::SelectTab {
                                    workspace_id: workspace_id.clone(),
                                    tab_id: id.clone(),
                                },
                            );
                            cx.emit(SidebarEvent::WorkspaceTabActivated);
                            cx.notify();
                        }
                    }),
                );
            if horizontal {
                row = row.w(px(164.0));
            }
            rows = rows.child(row);
        }
        rows.into_any_element()
    }
    pub(super) fn workspace_body(
        &mut self,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = self.workspace_nav.active.clone();
        div()
            .flex_1()
            .min_h(px(0.0))
            .flex()
            .flex_col()
            .px(px(10.0))
            .gap(px(8.0))
            .child(self.workspace_control(colors, cx))
            .child(self.workspace_rows(false, colors, cx))
            .child(
                div()
                    .id("workspace-add-tab")
                    .role(Role::Button)
                    .aria_label("Add existing session to workspace")
                    .h(px(30.0))
                    .flex()
                    .items_center()
                    .gap(px(7.0))
                    .px(px(9.0))
                    .cursor_pointer()
                    .text_size(px(Typo::META.size))
                    .text_color(colors.secondary)
                    .child(sf_symbol("plus", 11.0, colors.secondary))
                    .child("Add session")
                    .on_click(cx.listener(move |this, _, window, cx| {
                        if let Some(id) = &id {
                            this.workspace_nav.destination =
                                Some(SessionDestination::Tab(id.clone()));
                            this.workspace_nav.menu = true;
                            this.workspace_nav.query.clear();
                            this.workspace_nav.focus.focus(window, cx);
                            cx.notify();
                        }
                    })),
            )
            .into_any_element()
    }
}

impl Sidebar {
    pub(super) fn workspace_popup(
        &self,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !self.workspace_nav.menu {
            return None;
        }
        let store = self.store.read().expect("store");
        let catalog = store.workspace_catalog();
        let mut panel = div()
            .id("workspace-menu")
            .debug_selector(|| "workspace-menu".into())
            .absolute()
            .top(px(78.0))
            .left(px(8.0))
            .right(px(8.0))
            .max_h(px(480.0))
            .flex()
            .flex_col()
            .gap(px(5.0))
            .p(px(8.0))
            .rounded(px(11.0))
            .border_1()
            .border_color(colors.primary.alpha(0.10))
            .bg(colors.background)
            .shadow_lg()
            .occlude()
            .track_focus(&self.workspace_nav.focus)
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                this.workspace_nav.menu = false;
                cx.notify();
            }));
        match catalog.status() {
            crate::store::WorkspaceCatalogStatus::Loading => {
                panel = panel.child(
                    div()
                        .text_size(px(12.0))
                        .text_color(colors.secondary)
                        .child("Loading workspaces…"),
                );
            }
            crate::store::WorkspaceCatalogStatus::Unavailable(detail) => {
                panel = panel
                    .child(
                        div()
                            .text_size(px(12.0))
                            .text_color(colors.secondary)
                            .child(detail.clone()),
                    )
                    .child(
                        div()
                            .id("retry-workspaces")
                            .role(Role::Button)
                            .aria_label("Retry loading workspaces")
                            .cursor_pointer()
                            .p(px(6.0))
                            .text_size(px(12.0))
                            .child("Retry")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.store.write().expect("store").refresh_workspaces();
                                cx.notify();
                            })),
                    );
                return Some(panel.into_any_element());
            }
            crate::store::WorkspaceCatalogStatus::Ready => {}
        }
        if let Some(error) = &catalog.error {
            panel = panel.child(
                div()
                    .text_size(px(11.0))
                    .text_color(colors.secondary)
                    .child(error.clone()),
            );
        }
        let heading = if matches!(
            self.workspace_nav.editor,
            Some(WorkspaceEditor::RenameTab(_))
        ) {
            "Tab name"
        } else if self.workspace_nav.editor.is_some() {
            "Workspace name"
        } else if self.workspace_nav.destination.is_some() {
            "Find a session"
        } else {
            "Workspaces"
        };
        panel = panel.child(
            div()
                .text_size(px(11.0))
                .text_color(colors.tertiary)
                .px(px(5.0))
                .child(heading),
        );
        panel = panel.child(
            div()
                .id("workspace-query")
                .debug_selector(|| "workspace-query".into())
                .role(Role::TextInput)
                .aria_label(heading)
                .h(px(30.0))
                .px(px(7.0))
                .flex()
                .items_center()
                .rounded(px(6.0))
                .bg(colors.primary.alpha(0.05))
                .text_size(px(12.0))
                .overflow_hidden()
                .child(query_label(&self.workspace_nav.query))
                .on_click(
                    cx.listener(|this, _, window, cx| this.workspace_nav.focus.focus(window, cx)),
                ),
        );
        if self.workspace_nav.editor.is_some() {
            panel = panel.child(
                div()
                    .px(px(5.0))
                    .py(px(6.0))
                    .text_size(px(11.0))
                    .text_color(colors.tertiary)
                    .child("Return to save · Escape to cancel"),
            );
            return Some(panel.into_any_element());
        }
        let query = self.workspace_nav.query.text().trim().to_lowercase();
        let mut choices = div()
            .id("workspace-menu-choices")
            .flex()
            .flex_col()
            .min_h(px(0.0))
            .max_h(px(330.0))
            .overflow_y_scroll();
        if let Some(destination) = &self.workspace_nav.destination {
            if let SessionDestination::Split { tab, pane, edge } = destination {
                let right = *edge == DockEdge::Right;
                let tab = tab.clone();
                let pane = pane.clone();
                panel = panel.child(
                    div()
                        .id("workspace-split-direction")
                        .role(Role::Button)
                        .aria_label("Change split direction")
                        .p(px(6.0))
                        .cursor_pointer()
                        .text_size(px(12.0))
                        .child(if right {
                            "Side by side  ↔"
                        } else {
                            "Top and bottom  ↕"
                        })
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.workspace_nav.destination = Some(SessionDestination::Split {
                                tab: tab.clone(),
                                pane: pane.clone(),
                                edge: if right {
                                    DockEdge::Bottom
                                } else {
                                    DockEdge::Right
                                },
                            });
                            cx.notify();
                        })),
                );
            }
            let mut sessions = store
                .sessions()
                .values()
                .filter(|session| {
                    query.is_empty()
                        || format!(
                            "{} {} {}",
                            session.title,
                            session.cwd,
                            session.host.as_deref().unwrap_or("local")
                        )
                        .to_lowercase()
                        .contains(&query)
                })
                .cloned()
                .collect::<Vec<_>>();
            sessions.sort_by(|a, b| a.title.cmp(&b.title).then_with(|| a.id.0.cmp(&b.id.0)));
            for session in sessions.into_iter().take(100) {
                let destination = destination.clone();
                let id = session.id.clone();
                let title = display_title(&session);
                let detail = format!(
                    "{} · {}",
                    session.host.as_deref().unwrap_or("Local"),
                    session.cwd
                );
                choices = choices.child(
                    div()
                        .id(SharedString::from(format!("workspace-session-{}", id.0)))
                        .role(Role::Button)
                        .aria_label(format!("Add {title}"))
                        .px(px(7.0))
                        .py(px(6.0))
                        .rounded(px(6.0))
                        .cursor_pointer()
                        .hover(move |row| row.bg(colors.primary.alpha(0.06)))
                        .child(
                            div()
                                .text_size(px(12.0))
                                .overflow_hidden()
                                .text_ellipsis()
                                .child(title),
                        )
                        .child(
                            div()
                                .text_size(px(10.0))
                                .text_color(colors.tertiary)
                                .overflow_hidden()
                                .text_ellipsis()
                                .child(detail),
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            let mutation = match &destination {
                                SessionDestination::Tab(workspace_id) => {
                                    WorkspaceMutation::CreateTab {
                                        workspace_id: workspace_id.clone(),
                                        session_id: id.clone(),
                                        title: None,
                                    }
                                }
                                SessionDestination::Split { tab, pane, edge } => {
                                    WorkspaceMutation::SplitPane {
                                        tab_id: tab.clone(),
                                        target: pane.clone(),
                                        session_id: id.clone(),
                                        edge: *edge,
                                    }
                                }
                            };
                            if this.store.write().expect("store").edit_workspace(mutation) {
                                this.workspace_nav.menu = false;
                                this.workspace_nav.destination = None;
                            }
                            cx.emit(SidebarEvent::WorkspaceTabActivated);
                            cx.notify();
                        })),
                );
            }
        } else {
            choices = choices.child(
                div()
                    .id("workspace-all-sessions")
                    .role(Role::Button)
                    .aria_label("Browse all sessions")
                    .h(px(30.0))
                    .px(px(7.0))
                    .flex()
                    .items_center()
                    .text_size(px(12.0))
                    .rounded(px(6.0))
                    .cursor_pointer()
                    .hover(move |row| row.bg(colors.primary.alpha(0.06)))
                    .child("All sessions")
                    .on_click(cx.listener(|this, _, _, cx| this.activate_workspace(None, cx))),
            );
            if let Some(snapshot) = catalog.snapshot() {
                for workspace in &snapshot.workspaces {
                    if !workspace.name.to_lowercase().contains(&query) {
                        continue;
                    }
                    let id = workspace.id.clone();
                    let rename_id = id.clone();
                    let name = workspace.name.clone();
                    let rename_name = name.clone();
                    choices = choices.child(
                        div()
                            .id(SharedString::from(format!("choose-workspace-{}", id.0)))
                            .role(Role::Button)
                            .aria_label(name.clone())
                            .h(px(32.0))
                            .px(px(7.0))
                            .flex()
                            .items_center()
                            .gap(px(5.0))
                            .rounded(px(6.0))
                            .cursor_pointer()
                            .hover(move |row| row.bg(colors.primary.alpha(0.06)))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.0))
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    .text_size(px(12.0))
                                    .child(name),
                            )
                            .child(
                                div()
                                    .text_size(px(10.0))
                                    .text_color(colors.tertiary)
                                    .child(workspace.tabs.len().to_string()),
                            )
                            .child(
                                div()
                                    .id(SharedString::from(format!("rename-workspace-{}", id.0)))
                                    .role(Role::Button)
                                    .aria_label("Rename workspace")
                                    .size(px(20.0))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .child(sf_symbol("pencil", 10.0, colors.tertiary))
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.begin_workspace_editor(
                                            WorkspaceEditor::Rename(rename_id.clone()),
                                            &rename_name,
                                            window,
                                            cx,
                                        );
                                        cx.stop_propagation();
                                    })),
                            )
                            .drag_over::<DraggedWorkspaceTab>(move |row, _, _, _| {
                                row.bg(colors.primary.alpha(0.12))
                            })
                            .on_drop(cx.listener({
                                let destination = id.clone();
                                let index = workspace.tabs.len();
                                move |this, dragged: &DraggedWorkspaceTab, _, cx| {
                                    this.move_workspace_tab(dragged, destination.clone(), index, cx)
                                }
                            }))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.activate_workspace(Some(id.clone()), cx)
                            })),
                    );
                }
            }
            if let Some(snapshot) = catalog.snapshot()
                && let Some(index) = snapshot
                    .workspaces
                    .iter()
                    .position(|workspace| Some(&workspace.id) == self.workspace_nav.active.as_ref())
            {
                let workspace = &snapshot.workspaces[index];
                let mut actions = div().flex().items_center().gap(px(4.0)).pt(px(5.0));
                for (label, icon, mutation, enabled) in [
                    (
                        "Move workspace up",
                        "arrow.up",
                        WorkspaceMutation::MoveWorkspace {
                            workspace_id: workspace.id.clone(),
                            index: index.saturating_sub(1),
                        },
                        index > 0,
                    ),
                    (
                        "Move workspace down",
                        "arrow.down",
                        WorkspaceMutation::MoveWorkspace {
                            workspace_id: workspace.id.clone(),
                            index: index + 1,
                        },
                        index + 1 < snapshot.workspaces.len(),
                    ),
                    (
                        "Remove workspace",
                        "trash",
                        WorkspaceMutation::RemoveWorkspace {
                            workspace_id: workspace.id.clone(),
                        },
                        true,
                    ),
                ] {
                    actions = actions.child(
                        div()
                            .id(SharedString::from(label))
                            .role(Role::Button)
                            .aria_label(label)
                            .h(px(28.0))
                            .px(px(8.0))
                            .flex()
                            .items_center()
                            .gap(px(5.0))
                            .rounded(px(5.0))
                            .opacity(if enabled { 1.0 } else { 0.35 })
                            .when(enabled, |button| {
                                button
                                    .cursor_pointer()
                                    .hover(move |button| button.bg(colors.primary.alpha(0.07)))
                            })
                            .child(sf_symbol(icon, 11.0, colors.secondary))
                            .when(label == "Remove workspace", |button| {
                                button.child(div().text_size(px(11.0)).child("Remove"))
                            })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if enabled {
                                    this.store
                                        .write()
                                        .expect("store")
                                        .edit_workspace(mutation.clone());
                                    cx.notify();
                                }
                            })),
                    );
                }
                panel = panel.child(actions).child(
                    div()
                        .px(px(5.0))
                        .text_size(px(10.0))
                        .text_color(colors.tertiary)
                        .child("Removing a workspace keeps its sessions running."),
                );
            }
            panel = panel.child(
                div()
                    .id("new-workspace")
                    .role(Role::Button)
                    .aria_label("Create workspace")
                    .h(px(30.0))
                    .px(px(7.0))
                    .flex()
                    .items_center()
                    .gap(px(7.0))
                    .rounded(px(6.0))
                    .cursor_pointer()
                    .hover(move |row| row.bg(colors.primary.alpha(0.06)))
                    .child(sf_symbol("plus", 11.0, colors.secondary))
                    .child(div().text_size(px(12.0)).child("New workspace"))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.begin_workspace_editor(WorkspaceEditor::Create, "", window, cx)
                    })),
            );
        }
        panel = panel.child(choices);
        Some(panel.into_any_element())
    }

    pub(super) fn workspace_strip(
        &mut self,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .id("horizontal-workspace-tabs")
            .debug_selector(|| "horizontal-workspace-tabs".into())
            .h(px(crate::tab_navigation::TAB_STRIP_HEIGHT))
            .w_full()
            .flex_none()
            .flex()
            .items_center()
            .gap(px(8.0))
            .pl(px(if cfg!(target_os = "macos") && !self.ui.visible {
                84.0
            } else {
                10.0
            }))
            .pr(px(10.0))
            .border_b_1()
            .border_color(colors.primary.alpha(0.07))
            .bg(colors.sidebar_surface())
            .child(
                div()
                    .w(px(150.0))
                    .flex_none()
                    .child(self.workspace_control(colors, cx)),
            )
            .child(self.workspace_rows(true, colors, cx))
            .child(
                div()
                    .id("horizontal-workspace-add-tab")
                    .role(Role::Button)
                    .aria_label("Add session to workspace")
                    .size(px(26.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .child(sf_symbol("plus", 12.0, colors.secondary))
                    .on_click(cx.listener(|this, _, window, cx| {
                        if let Some(id) = &this.workspace_nav.active {
                            this.workspace_nav.destination =
                                Some(SessionDestination::Tab(id.clone()));
                            this.workspace_nav.menu = true;
                            this.workspace_nav.query.clear();
                            this.workspace_nav.focus.focus(window, cx);
                            this.peek(window, cx);
                            cx.notify();
                        }
                    })),
            )
            .into_any_element()
    }
}

impl Sidebar {
    pub(super) fn select_workspace_tab(&mut self, index: usize, cx: &mut Context<Self>) -> bool {
        let Some(workspace) = self.workspace_record() else {
            return false;
        };
        let Some(tab) = workspace.tabs.get(index) else {
            return false;
        };
        let accepted =
            self.store
                .write()
                .expect("store")
                .edit_workspace(WorkspaceMutation::SelectTab {
                    workspace_id: workspace.id,
                    tab_id: tab.id.clone(),
                });
        if accepted {
            cx.emit(SidebarEvent::WorkspaceTabActivated);
            cx.notify();
        }
        accepted
    }
    pub(super) fn relative_workspace_tab(&mut self, delta: isize, cx: &mut Context<Self>) -> bool {
        let Some(workspace) = self.workspace_record() else {
            return false;
        };
        if workspace.tabs.is_empty() {
            return false;
        }
        let current = workspace
            .tabs
            .iter()
            .position(|tab| Some(&tab.id) == workspace.selected_tab.as_ref())
            .unwrap_or(0);
        self.select_workspace_tab(
            (current as isize + delta).rem_euclid(workspace.tabs.len() as isize) as usize,
            cx,
        )
    }
    pub(super) fn reorder_workspace_tab(&mut self, delta: isize, cx: &mut Context<Self>) -> bool {
        let Some(workspace) = self.workspace_record() else {
            return false;
        };
        let Some(current) = workspace
            .tabs
            .iter()
            .position(|tab| Some(&tab.id) == workspace.selected_tab.as_ref())
        else {
            return false;
        };
        let index = (current as isize + delta).clamp(0, workspace.tabs.len() as isize - 1) as usize;
        let accepted =
            self.store
                .write()
                .expect("store")
                .edit_workspace(WorkspaceMutation::MoveTab {
                    tab_id: workspace.tabs[current].id.clone(),
                    workspace_id: workspace.id,
                    index,
                });
        cx.notify();
        accepted
    }
    pub(super) fn rename_workspace_tab(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(workspace) = self.workspace_record() else {
            return false;
        };
        let Some(tab) = workspace
            .tabs
            .iter()
            .find(|tab| Some(&tab.id) == workspace.selected_tab.as_ref())
        else {
            return false;
        };
        let title = tab_title(tab, &self.store.read().expect("store"));
        self.begin_workspace_editor(
            WorkspaceEditor::RenameTab(tab.id.clone()),
            &title,
            window,
            cx,
        );
        true
    }
    pub(super) fn remove_workspace_tab(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(tab_id) = self
            .workspace_record()
            .and_then(|workspace| workspace.selected_tab)
        else {
            return false;
        };
        self.store
            .write()
            .expect("store")
            .edit_workspace(WorkspaceMutation::RemoveTab { tab_id });
        cx.notify();
        true // closing a placement must never fall through to session removal
    }
    pub(super) fn workspace_focused_session(&self) -> Option<SessionId> {
        fn find(node: &diri_proto::workspace::LayoutNode, id: &PaneId) -> Option<SessionId> {
            match node {
                diri_proto::workspace::LayoutNode::Pane {
                    id: pane,
                    session_id,
                } => (pane == id).then(|| session_id.clone()),
                diri_proto::workspace::LayoutNode::Split { first, second, .. } => {
                    find(first, id).or_else(|| find(second, id))
                }
            }
        }
        let workspace = self.workspace_record()?;
        let tab = workspace
            .tabs
            .iter()
            .find(|tab| Some(&tab.id) == workspace.selected_tab.as_ref())?;
        find(&tab.layout, &tab.focused_pane)
    }
}
