mod groups;
use super::*;
use crate::store::TabOrientation;
use diri_proto::workspace::{
    DockEdge, PaneId, TabId, WorkspaceId, WorkspaceMutation, WorkspaceRecord,
};
use gpui::{Div, Stateful};
use groups::{WorkspaceRowKey, project_groups};

const WORKSPACE_MENU_WIDTH: f32 = 272.0;

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
    pub(super) focus: FocusHandle,
    awaiting_create: Option<u64>,
    highlighted_session: Option<SessionId>,
    highlighted_workspace: Option<Option<WorkspaceId>>,
    cursor: Option<WorkspaceRowKey>,
    pending_activation: Option<(WorkspaceId, TabId, u64)>,
    menu_scroll: ScrollHandle,
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
            awaiting_create: None,
            highlighted_session: None,
            highlighted_workspace: None,
            cursor: None,
            pending_activation: None,
            menu_scroll: ScrollHandle::new(),
            scroll: ScrollHandle::new(),
            vertical_scroll: ScrollHandle::new(),
            last_selection: None,
            available_width: 0.0,
        }
    }
}

fn session_choices(store: &SessionStore, query: &str) -> Vec<Arc<SessionRecord>> {
    let query = query.trim().to_lowercase();
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
    sessions.truncate(100);
    sessions
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

fn workspace_menu_targets(
    snapshot: Option<&diri_proto::workspace::WorkspaceSnapshot>,
    query: &str,
) -> Vec<Option<WorkspaceId>> {
    let query = query.trim().to_lowercase();
    let mut targets = Vec::new();
    if "all sessions".contains(&query) {
        targets.push(None);
    }
    if let Some(snapshot) = snapshot {
        targets.extend(
            snapshot
                .workspaces
                .iter()
                .filter(|workspace| workspace.name.to_lowercase().contains(&query))
                .map(|workspace| Some(workspace.id.clone())),
        );
    }
    targets
}

impl Sidebar {
    pub(crate) fn run_workspace_palette(
        &mut self,
        command: crate::palette_workspace::WorkspaceCommand,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        use crate::palette_workspace::WorkspaceCommand;
        if matches!(
            &command,
            WorkspaceCommand::Create
                | WorkspaceCommand::Browse
                | WorkspaceCommand::Rename(_)
                | WorkspaceCommand::RenameTab(_)
                | WorkspaceCommand::RenameSession(_)
        ) {
            self.peek(window, cx);
        }
        match command {
            WorkspaceCommand::Create => {
                self.begin_workspace_editor(WorkspaceEditor::Create, "", window, cx);
            }
            WorkspaceCommand::Browse => {
                self.workspace_nav.editor = None;
                self.workspace_nav.destination = None;
                self.workspace_nav.highlighted_workspace = Some(self.workspace_nav.active.clone());
                self.workspace_nav.query.clear();
                self.workspace_nav.menu = true;
                self.workspace_nav.focus.focus(window, cx);
                cx.notify();
            }
            WorkspaceCommand::Switch(id) => {
                if id.as_ref().is_some_and(|id| {
                    !self
                        .store
                        .read()
                        .expect("store")
                        .workspace_catalog()
                        .snapshot()
                        .is_some_and(|snapshot| {
                            snapshot
                                .workspaces
                                .iter()
                                .any(|workspace| &workspace.id == id)
                        })
                }) {
                    return false;
                }
                self.activate_workspace(id, cx);
            }
            WorkspaceCommand::Rename(id) => {
                let name = self
                    .store
                    .read()
                    .expect("store")
                    .workspace_catalog()
                    .snapshot()
                    .and_then(|snapshot| {
                        snapshot
                            .workspaces
                            .iter()
                            .find(|workspace| workspace.id == id)
                    })
                    .map(|workspace| workspace.name.clone());
                let Some(name) = name else { return false };
                self.begin_workspace_editor(WorkspaceEditor::Rename(id), &name, window, cx);
            }
            WorkspaceCommand::RenameTab(id) => {
                let title = {
                    let store = self.store.read().expect("store");
                    store
                        .workspace_catalog()
                        .snapshot()
                        .and_then(|snapshot| {
                            snapshot
                                .workspaces
                                .iter()
                                .flat_map(|workspace| &workspace.tabs)
                                .find(|tab| tab.id == id)
                        })
                        .map(|tab| tab_title(tab, &store))
                };
                let Some(title) = title else { return false };
                self.begin_workspace_editor(WorkspaceEditor::RenameTab(id), &title, window, cx);
            }
            WorkspaceCommand::CloseTab(id) => {
                return self
                    .store
                    .write()
                    .expect("store")
                    .edit_workspace(WorkspaceMutation::RemoveTab { tab_id: id });
            }
            WorkspaceCommand::RenameSession(id) => {
                let session = self
                    .store
                    .read()
                    .expect("store")
                    .sessions()
                    .get(&id)
                    .cloned();
                let Some(session) = session else { return false };
                self.begin_rename(&session, window, cx);
            }
            WorkspaceCommand::CloseSession(id) => {
                self.store.write().expect("store").request_close(vec![id]);
                cx.notify();
            }
        }
        true
    }

    pub(crate) fn workspace_menu_is_open(&self) -> bool {
        self.workspace_nav.menu
    }
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

    /// Copy window context without changing preferences or emitting a
    /// navigation event before the root view finishes construction.
    pub(crate) fn set_initial_workspace(&mut self, id: Option<WorkspaceId>) {
        self.workspace_nav.active = id;
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
        self.workspace_nav.awaiting_create = None;
        self.workspace_nav.pending_activation = None;
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
        edge: DockEdge,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.workspace_nav.editor = None;
        self.workspace_nav.destination = Some(SessionDestination::Split { tab, pane, edge });
        self.workspace_nav.highlighted_session = None;
        self.workspace_nav.menu = true;
        self.workspace_nav.query.clear();
        self.workspace_nav.focus.focus(window, cx);
        self.peek(window, cx);
        cx.notify();
    }
    pub(super) fn reconcile_workspace_navigation(&mut self, cx: &mut Context<Self>) {
        self.reconcile_workspace_activation(cx);
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
        if let Some(expected) = self.workspace_nav.awaiting_create
            && let Some((request, created)) = created
            && expected == request
        {
            self.workspace_nav.awaiting_create = None;
            self.activate_workspace(Some(created), cx);
        } else if ready && self.workspace_nav.awaiting_create.is_some() {
            // A failed or superseded request must never select a later creation.
            self.workspace_nav.awaiting_create = None;
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
        if self.workspace_nav.destination.is_none()
            && self.workspace_nav.editor.is_none()
            && matches!(event.keystroke.key.as_str(), "up" | "down" | "enter")
        {
            let targets = workspace_menu_targets(
                self.store
                    .read()
                    .expect("store")
                    .workspace_catalog()
                    .snapshot(),
                self.workspace_nav.query.text(),
            );
            if !targets.is_empty() {
                let current = self
                    .workspace_nav
                    .highlighted_workspace
                    .as_ref()
                    .and_then(|id| targets.iter().position(|target| target == id))
                    .unwrap_or(0);
                let next = match event.keystroke.key.as_str() {
                    "up" => current.saturating_sub(1),
                    "down" => (current + 1).min(targets.len() - 1),
                    _ => current,
                };
                let target = targets[next].clone();
                self.workspace_nav.highlighted_workspace = Some(target.clone());
                self.workspace_nav.menu_scroll.scroll_to_item(next);
                if event.keystroke.key == "enter" {
                    self.activate_workspace(target, cx);
                }
            } else if event.keystroke.key == "enter" {
                // "Filter or create": a query that matches nothing becomes
                // the new workspace's name, as in the explicit editor.
                let name = self.workspace_nav.query.text().trim().to_owned();
                if !name.is_empty()
                    && self
                        .store
                        .write()
                        .expect("store")
                        .edit_workspace(WorkspaceMutation::CreateWorkspace { name })
                {
                    self.workspace_nav.awaiting_create = Some(
                        self.store
                            .read()
                            .expect("store")
                            .workspace_catalog()
                            .create_request_id,
                    );
                    self.workspace_nav.query.clear();
                }
            }
            cx.stop_propagation();
            cx.notify();
            return true;
        }
        if self.workspace_nav.destination.is_some()
            && self.workspace_nav.editor.is_none()
            && matches!(event.keystroke.key.as_str(), "up" | "down" | "enter")
        {
            let choices = session_choices(
                &self.store.read().expect("store"),
                self.workspace_nav.query.text(),
            );
            if !choices.is_empty() {
                let current = self
                    .workspace_nav
                    .highlighted_session
                    .as_ref()
                    .and_then(|id| choices.iter().position(|session| &session.id == id))
                    .unwrap_or(0);
                let next = match event.keystroke.key.as_str() {
                    "up" => current.saturating_sub(1),
                    "down" => (current + 1).min(choices.len() - 1),
                    _ => current,
                };
                let id = choices[next].id.clone();
                self.workspace_nav.highlighted_session = Some(id.clone());
                self.workspace_nav.menu_scroll.scroll_to_item(next);
                if event.keystroke.key == "enter" {
                    self.place_workspace_session(
                        self.workspace_nav.destination.clone().unwrap(),
                        id,
                        cx,
                    );
                }
            }
            cx.stop_propagation();
            cx.notify();
            return true;
        }
        match event.keystroke.key.as_str() {
            "escape" => {
                self.workspace_nav.menu = false;
                self.workspace_nav.editor = None;
                self.workspace_nav.destination = None;
                if self.tab_orientation() == TabOrientation::Horizontal {
                    cx.emit(SidebarEvent::FocusTerminal);
                } else {
                    self.focus_handle.focus(window, cx);
                }
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
                    self.workspace_nav.awaiting_create = creating.then(|| {
                        self.store
                            .read()
                            .expect("store")
                            .workspace_catalog()
                            .create_request_id
                    });
                    self.workspace_nav.query.clear();
                }
            }
            _ => {
                let Some(edit) = query_editor::edit_for(&event.keystroke) else {
                    return false;
                };
                self.workspace_nav.highlighted_session = None;
                self.workspace_nav.highlighted_workspace = None;
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
    fn place_workspace_session(
        &mut self,
        destination: SessionDestination,
        id: SessionId,
        cx: &mut Context<Self>,
    ) {
        let mutation = match destination {
            SessionDestination::Tab(workspace_id) => WorkspaceMutation::CreateTab {
                select: true,
                workspace_id,
                session_id: id,
                title: None,
            },
            SessionDestination::Split { tab, pane, edge } => WorkspaceMutation::SplitPane {
                tab_id: tab,
                target: pane,
                session_id: id,
                edge,
            },
        };
        if self.store.write().expect("store").edit_workspace(mutation) {
            self.workspace_nav.menu = false;
            self.workspace_nav.destination = None;
            cx.emit(SidebarEvent::WorkspaceTabActivated);
        }
        cx.notify();
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
                this.toggle_workspace_menu(window, cx);
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
        let groups = {
            let store = self.store.read().expect("store");
            let Some(snapshot) = store.workspace_catalog().snapshot() else {
                return div()
                    .p(px(14.0))
                    .text_color(colors.secondary)
                    .child("Loading workspaces…")
                    .into_any_element();
            };
            project_groups(
                snapshot,
                &store,
                if horizontal {
                    ""
                } else {
                    self.filter_query.text()
                },
            )
        };
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
        let selected_key = groups
            .iter()
            .find(|group| self.workspace_nav.active.as_ref() == Some(&group.id))
            .and_then(|group| {
                group
                    .selected
                    .as_ref()
                    .map(|tab| WorkspaceRowKey::Tab(group.id.clone(), tab.clone()))
            });
        let fingerprint = self.workspace_nav.active.clone().map(|workspace| {
            (
                workspace,
                selected_key.as_ref().and_then(|key| match key {
                    WorkspaceRowKey::Tab(_, tab) => Some(tab.clone()),
                    _ => None,
                }),
                horizontal,
                self.workspace_nav.available_width.to_bits(),
            )
        });
        if self.workspace_nav.last_selection != fingerprint {
            let keys = groups
                .iter()
                .flat_map(|group| group.row_keys())
                .collect::<Vec<_>>();
            if let Some(index) = selected_key
                .as_ref()
                .and_then(|selected| keys.iter().position(|key| key == selected))
            {
                if horizontal {
                    if let Some(group) = groups
                        .iter()
                        .find(|group| self.workspace_nav.active.as_ref() == Some(&group.id))
                        && let Some(index) = group
                            .tabs
                            .iter()
                            .position(|row| group.selected.as_ref() == Some(&row.id))
                    {
                        self.workspace_nav
                            .scroll
                            .set_offset(point(px(-(index as f32) * 167.0), px(0.0)));
                    }
                } else {
                    self.workspace_nav
                        .vertical_scroll
                        .set_offset(point(px(0.0), px(-(index as f32) * 35.0)));
                }
            }
            self.workspace_nav.last_selection = fingerprint;
        }
        for group in groups {
            let active_group = self.workspace_nav.active.as_ref() == Some(&group.id);
            if horizontal && !active_group {
                continue;
            }
            if !horizontal {
                rows = rows.child(self.workspace_heading(&group, colors, cx));
            }
            if horizontal || !group.collapsed {
                for row in &group.tabs {
                    rows = rows.child(self.workspace_tab_row(
                        &group.id,
                        row,
                        active_group && group.selected.as_ref() == Some(&row.id),
                        horizontal,
                        colors,
                        cx,
                    ));
                }
            }
        }
        rows.into_any_element()
    }
    fn workspace_tab_row(
        &self,
        workspace: &WorkspaceId,
        tab: &groups::TabRow,
        active: bool,
        horizontal: bool,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let store = self.store.read().expect("store");
        let id = tab.id.clone();
        let index = tab.index;
        let workspace_id = workspace.clone();
        let title = tab.title.clone();
        let rename_id = id.clone();
        let rename_title = title.clone();
        let source = DraggedWorkspaceTab {
            tab: id.clone(),
            revision: store
                .workspace_catalog()
                .snapshot()
                .map_or(0, |snapshot| snapshot.revision),
        };
        let destination = workspace.clone();
        let remove = id.clone();
        let mut row = div()
            .id(SharedString::from(format!("workspace-tab-{}", id.0)))
            .role(Role::Tab)
            .aria_label(title.clone())
            .aria_selected(active)
            .debug_selector({
                let key = format!("workspace-tab-{}", id.0);
                move || key.clone()
            })
            .h(px(32.0))
            .px(px(9.0))
            .flex_none()
            .flex()
            .items_center()
            .gap(px(6.0))
            .rounded(px(7.0))
            .cursor_pointer()
            .border_1()
            .border_color(
                if self.workspace_nav.cursor.as_ref()
                    == Some(&WorkspaceRowKey::Tab(workspace_id.clone(), id.clone()))
                {
                    colors.primary.alpha(0.22)
                } else {
                    colors.primary.alpha(0.0)
                },
            )
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
                    .child(
                        if let Some(range) = crate::sidebar::filter::label_match(
                            &title,
                            if horizontal {
                                ""
                            } else {
                                self.filter_query.text()
                            },
                        ) {
                            gpui::StyledText::new(title.clone()).with_highlights([(
                                range,
                                gpui::HighlightStyle {
                                    color: Some(Palette::CLAY.into()),
                                    font_weight: Some(FontWeight::SEMIBOLD),
                                    ..Default::default()
                                },
                            )])
                        } else {
                            gpui::StyledText::new(title.clone())
                        },
                    ),
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
                        this.request_workspace_tab(workspace_id.clone(), id.clone(), cx);
                    }
                }),
            );
        if horizontal {
            row = row.w(px(164.0));
        }
        row.into_any_element()
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
    /// Opens or closes the workspace menu from the picker pill or ⌘B. In
    /// horizontal mode the menu floats under the tab strip, so the sidebar is
    /// never revealed just to host it.
    pub(crate) fn toggle_workspace_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.workspace_nav.menu = !self.workspace_nav.menu;
        self.workspace_nav.query.clear();
        self.workspace_nav.editor = None;
        self.workspace_nav.destination = None;
        if self.workspace_nav.menu {
            self.workspace_nav.focus.focus(window, cx);
            if self.tab_orientation() != TabOrientation::Horizontal {
                self.peek(window, cx);
            }
        } else if self.tab_orientation() == TabOrientation::Horizontal {
            cx.emit(SidebarEvent::FocusTerminal);
        } else {
            self.focus_handle.focus(window, cx);
        }
        cx.notify();
    }

    #[cfg(test)]
    pub(crate) fn workspace_query_for_test(&self) -> String {
        self.workspace_nav.query.text().to_owned()
    }

    /// The menu as rendered inside the sidebar (vertical tabs). Horizontal
    /// tabs use [`Self::floating_workspace_popup`] instead.
    pub(super) fn workspace_popup(
        &mut self,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let panel = self.workspace_menu_panel(colors, cx)?;
        Some(
            panel
                .absolute()
                .top(px(78.0))
                .left(px(8.0))
                .right(px(8.0))
                .into_any_element(),
        )
    }

    /// The menu anchored under the workspace pill in the horizontal tab strip.
    /// Deferred so it paints above the terminal instead of being clipped by
    /// the strip, and closes on any click outside it.
    pub(super) fn floating_workspace_popup(
        &mut self,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let panel = self.workspace_menu_panel(colors, cx)?;
        let left = if cfg!(target_os = "macos") && !self.ui.visible {
            84.0
        } else {
            10.0
        };
        Some(
            deferred(
                anchored()
                    .position(point(
                        px(left),
                        px(crate::tab_navigation::TAB_STRIP_HEIGHT - 4.0),
                    ))
                    .anchor(Anchor::TopLeft)
                    .snap_to_window_with_margin(px(8.0))
                    .child(panel.w(px(WORKSPACE_MENU_WIDTH))),
            )
            .with_priority(1)
            .into_any_element(),
        )
    }

    fn workspace_menu_panel(
        &mut self,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> Option<Stateful<Div>> {
        if !self.workspace_nav.menu {
            return None;
        }
        let store = self.store.read().expect("store");
        let catalog = store.workspace_catalog();
        let hairline = colors.primary.alpha(0.08);
        let mut panel = div()
            .id("workspace-menu")
            .debug_selector(|| "workspace-menu".into())
            .max_h(px(480.0))
            .flex()
            .flex_col()
            .p(px(6.0))
            .rounded(px(Radius::PANEL))
            .border_1()
            .border_color(colors.primary.alpha(0.10))
            .bg(colors.background)
            .shadow_lg()
            .text_color(colors.primary)
            .occlude()
            .track_focus(&self.workspace_nav.focus)
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_key_down(cx.listener(|this, event, window, cx| {
                this.workspace_key(event, window, cx);
            }))
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                this.workspace_nav.menu = false;
                this.workspace_nav.editor = None;
                this.workspace_nav.destination = None;
                cx.notify();
            }));
        match catalog.status() {
            crate::store::WorkspaceCatalogStatus::Loading => {
                panel = panel.child(
                    div()
                        .px(px(8.0))
                        .py(px(6.0))
                        .text_size(px(12.0))
                        .text_color(colors.secondary)
                        .child("Loading workspaces…"),
                );
            }
            crate::store::WorkspaceCatalogStatus::Unavailable(detail) => {
                panel = panel
                    .child(
                        div()
                            .px(px(8.0))
                            .py(px(6.0))
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
                            .h(px(30.0))
                            .px(px(8.0))
                            .flex()
                            .items_center()
                            .rounded(px(Radius::ROW))
                            .hover(move |row| row.bg(colors.primary.alpha(0.06)))
                            .text_size(px(12.0))
                            .child("Retry")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.store.write().expect("store").refresh_workspaces();
                                cx.notify();
                            })),
                    );
                return Some(panel);
            }
            crate::store::WorkspaceCatalogStatus::Ready => {}
        }
        if let Some(error) = &catalog.error {
            panel = panel.child(
                div()
                    .px(px(8.0))
                    .py(px(4.0))
                    .text_size(px(11.0))
                    .text_color(colors.secondary)
                    .child(error.clone()),
            );
        }
        let editing = self.workspace_nav.editor.is_some();
        let picking = self.workspace_nav.destination.is_some();
        let (heading, placeholder) = if matches!(
            self.workspace_nav.editor,
            Some(WorkspaceEditor::RenameTab(_))
        ) {
            ("Tab name", "Tab name")
        } else if editing {
            ("Workspace name", "Workspace name")
        } else if picking {
            ("Find a session", "Find a session…")
        } else {
            ("Workspaces", "Filter or create…")
        };
        let query_empty = self.workspace_nav.query.is_empty();
        panel = panel.child(
            div()
                .id("workspace-query")
                .debug_selector(|| "workspace-query".into())
                .role(Role::TextInput)
                .aria_label(heading)
                .h(px(30.0))
                .mb(px(4.0))
                .px(px(9.0))
                .flex()
                .items_center()
                .gap(px(7.0))
                .rounded(px(Radius::ROW))
                .bg(colors.primary.alpha(0.05))
                .border_1()
                .border_color(colors.primary.alpha(0.07))
                .text_size(px(12.0))
                .overflow_hidden()
                .child(sf_symbol("magnifyingglass", 11.0, colors.tertiary))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .overflow_hidden()
                        .when(query_empty, |field| {
                            field.text_color(colors.tertiary).child(placeholder)
                        })
                        .when(!query_empty, |field| {
                            field.child(query_label(&self.workspace_nav.query))
                        }),
                )
                .on_click(
                    cx.listener(|this, _, window, cx| this.workspace_nav.focus.focus(window, cx)),
                ),
        );
        if editing {
            panel = panel.child(
                div()
                    .px(px(8.0))
                    .py(px(6.0))
                    .text_size(px(11.0))
                    .text_color(colors.tertiary)
                    .child("Return to save · Escape to cancel"),
            );
            return Some(panel);
        }
        let query = self.workspace_nav.query.text().trim().to_lowercase();
        let mut choices = div()
            .id("workspace-menu-choices")
            .track_scroll(&self.workspace_nav.menu_scroll)
            .flex()
            .flex_col()
            .min_h(px(0.0))
            .max_h(px(300.0))
            .overflow_y_scroll();
        if let Some(destination) = &self.workspace_nav.destination {
            panel = panel.child(
                div()
                    .px(px(8.0))
                    .pb(px(4.0))
                    .text_size(px(10.0))
                    .text_color(colors.tertiary)
                    .child("↑ ↓ to choose · Return to add"),
            );
            if let SessionDestination::Split { tab, pane, edge } = destination {
                let right = *edge == DockEdge::Right;
                let tab = tab.clone();
                let pane = pane.clone();
                panel = panel.child(
                    div()
                        .id("workspace-split-direction")
                        .role(Role::Button)
                        .aria_label("Change split direction")
                        .h(px(28.0))
                        .px(px(8.0))
                        .flex()
                        .items_center()
                        .rounded(px(Radius::ROW))
                        .cursor_pointer()
                        .hover(move |row| row.bg(colors.primary.alpha(0.06)))
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
            let sessions = session_choices(&store, &query);
            if self
                .workspace_nav
                .highlighted_session
                .as_ref()
                .is_none_or(|id| !sessions.iter().any(|session| &session.id == id))
            {
                self.workspace_nav.highlighted_session =
                    sessions.first().map(|session| session.id.clone());
            }
            if sessions.is_empty() {
                choices = choices.child(
                    div()
                        .px(px(8.0))
                        .py(px(7.0))
                        .text_size(px(12.0))
                        .text_color(colors.secondary)
                        .child("No matching sessions"),
                );
            }
            for session in sessions {
                let selected = self.workspace_nav.highlighted_session.as_ref() == Some(&session.id);
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
                        .aria_selected(selected)
                        .when(selected, |row| row.bg(colors.primary.alpha(0.08)))
                        .px(px(8.0))
                        .py(px(5.0))
                        .rounded(px(Radius::ROW))
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
                            this.place_workspace_session(destination.clone(), id.clone(), cx);
                        })),
                );
            }
            panel = panel.child(choices);
            return Some(panel);
        }

        let targets = workspace_menu_targets(catalog.snapshot(), &query);
        if self
            .workspace_nav
            .highlighted_workspace
            .as_ref()
            .is_none_or(|id| !targets.contains(id))
        {
            self.workspace_nav.highlighted_workspace = targets.first().cloned();
        }
        if targets.contains(&None) {
            let active = self.workspace_nav.active.is_none();
            let highlighted = self.workspace_nav.highlighted_workspace == Some(None);
            choices = choices.child(
                div()
                    .id("workspace-all-sessions")
                    .role(Role::Button)
                    .aria_label("Browse all sessions")
                    .aria_selected(active)
                    .h(px(30.0))
                    .px(px(8.0))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .rounded(px(Radius::ROW))
                    .cursor_pointer()
                    .when(highlighted, |row| row.bg(colors.primary.alpha(0.10)))
                    .hover(move |row| row.bg(colors.primary.alpha(0.06)))
                    .child(
                        div()
                            .w(px(14.0))
                            .flex_none()
                            .flex()
                            .justify_center()
                            .when(active, |slot| {
                                slot.child(sf_symbol("checkmark", 10.0, colors.primary))
                            }),
                    )
                    .child(div().flex_1().text_size(px(12.5)).child("All sessions"))
                    .on_click(cx.listener(|this, _, _, cx| this.activate_workspace(None, cx))),
            );
        }
        if let Some(snapshot) = catalog.snapshot() {
            for workspace in &snapshot.workspaces {
                if !workspace.name.to_lowercase().contains(&query) {
                    continue;
                }
                let id = workspace.id.clone();
                let rename_id = id.clone();
                let name = workspace.name.clone();
                let rename_name = name.clone();
                let active = self.workspace_nav.active.as_ref() == Some(&id);
                let highlighted =
                    self.workspace_nav.highlighted_workspace == Some(Some(id.clone()));
                let group = SharedString::from(format!("workspace-row-{}", id.0));
                choices = choices.child(
                    div()
                        .id(SharedString::from(format!("choose-workspace-{}", id.0)))
                        .group(group.clone())
                        .role(Role::Button)
                        .aria_label(name.clone())
                        .aria_selected(active)
                        .h(px(30.0))
                        .px(px(8.0))
                        .flex()
                        .items_center()
                        .gap(px(8.0))
                        .rounded(px(Radius::ROW))
                        .cursor_pointer()
                        .when(highlighted, |row| row.bg(colors.primary.alpha(0.10)))
                        .hover(move |row| row.bg(colors.primary.alpha(0.06)))
                        .child(
                            div()
                                .w(px(14.0))
                                .flex_none()
                                .flex()
                                .justify_center()
                                .when(active, |slot| {
                                    slot.child(sf_symbol("checkmark", 10.0, colors.primary))
                                }),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.0))
                                .overflow_hidden()
                                .text_ellipsis()
                                .text_size(px(12.5))
                                .child(name),
                        )
                        .child(
                            div()
                                .text_size(px(10.5))
                                .text_color(colors.tertiary)
                                .group_hover(group.clone(), |count| count.invisible())
                                .child(workspace.tabs.len().to_string()),
                        )
                        .child(
                            div()
                                .id(SharedString::from(format!("rename-workspace-{}", id.0)))
                                .role(Role::Button)
                                .aria_label("Rename workspace")
                                .absolute()
                                .right(px(6.0))
                                .size(px(20.0))
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded(px(Radius::CHIP))
                                .invisible()
                                .group_hover(group, |button| button.visible())
                                .hover(move |button| button.bg(colors.primary.alpha(0.08)))
                                .child(sf_symbol("pencil", 10.0, colors.secondary))
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
        if targets.is_empty() && !query.is_empty() {
            choices = choices.child(
                div()
                    .px(px(8.0))
                    .py(px(6.0))
                    .text_size(px(11.0))
                    .text_color(colors.tertiary)
                    .child(format!(
                        "Return creates “{}”",
                        self.workspace_nav.query.text().trim()
                    )),
            );
        }
        panel = panel.child(choices);

        if let Some(snapshot) = catalog.snapshot()
            && let Some(index) = snapshot
                .workspaces
                .iter()
                .position(|workspace| Some(&workspace.id) == self.workspace_nav.active.as_ref())
        {
            let workspace = &snapshot.workspaces[index];
            let mut actions = div()
                .flex()
                .items_center()
                .gap(px(2.0))
                .px(px(2.0))
                .pt(px(4.0))
                .child(
                    div()
                        .flex_1()
                        .px(px(6.0))
                        .text_size(px(10.5))
                        .text_color(colors.tertiary)
                        .overflow_hidden()
                        .text_ellipsis()
                        .child(workspace.name.clone()),
                );
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
                        .size(px(24.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(Radius::CHIP))
                        .opacity(if enabled { 1.0 } else { 0.35 })
                        .when(enabled, |button| {
                            button
                                .cursor_pointer()
                                .hover(move |button| button.bg(colors.primary.alpha(0.07)))
                        })
                        .child(sf_symbol(icon, 11.0, colors.secondary))
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
            panel = panel.child(actions);
        }

        panel = panel.child(div().h(px(1.0)).mx(px(4.0)).my(px(5.0)).bg(hairline));
        panel = panel.child(
            div()
                .id("workspace-menu-new-session")
                .debug_selector(|| "workspace-menu-new-session".into())
                .role(Role::Button)
                .aria_label("New session")
                .h(px(30.0))
                .px(px(8.0))
                .flex()
                .items_center()
                .gap(px(8.0))
                .rounded(px(Radius::ROW))
                .cursor_pointer()
                .hover(move |row| row.bg(colors.primary.alpha(0.06)))
                .child(
                    div()
                        .w(px(14.0))
                        .flex_none()
                        .flex()
                        .justify_center()
                        .child(sf_symbol("terminal", 11.0, colors.secondary)),
                )
                .child(div().flex_1().text_size(px(12.5)).child("New Session"))
                .child(
                    div()
                        .text_size(px(10.5))
                        .text_color(colors.tertiary)
                        .child("⌘N"),
                )
                .on_click(cx.listener(|this, _, window, cx| {
                    this.workspace_nav.menu = false;
                    cx.notify();
                    window.dispatch_action(Box::new(crate::commands::OpenLauncher), cx);
                })),
        );
        panel = panel.child(
            div()
                .id("new-workspace")
                .role(Role::Button)
                .aria_label("Create workspace")
                .h(px(30.0))
                .px(px(8.0))
                .flex()
                .items_center()
                .gap(px(8.0))
                .rounded(px(Radius::ROW))
                .cursor_pointer()
                .hover(move |row| row.bg(colors.primary.alpha(0.06)))
                .child(
                    div()
                        .w(px(14.0))
                        .flex_none()
                        .flex()
                        .justify_center()
                        .child(sf_symbol("plus", 11.0, colors.secondary)),
                )
                .child(div().flex_1().text_size(px(12.5)).child("New Workspace"))
                .on_click(cx.listener(|this, _, window, cx| {
                    this.begin_workspace_editor(WorkspaceEditor::Create, "", window, cx)
                })),
        );
        panel = panel.child(
            div()
                .id("workspace-menu-add-remote-host")
                .debug_selector(|| "workspace-menu-add-remote-host".into())
                .role(Role::Button)
                .aria_label("Add remote host")
                .h(px(30.0))
                .px(px(8.0))
                .flex()
                .items_center()
                .gap(px(8.0))
                .rounded(px(Radius::ROW))
                .cursor_pointer()
                .hover(move |row| row.bg(colors.primary.alpha(0.06)))
                .child(
                    div()
                        .w(px(14.0))
                        .flex_none()
                        .flex()
                        .justify_center()
                        .child(sf_symbol("server.rack", 11.0, colors.secondary)),
                )
                .child(div().flex_1().text_size(px(12.5)).child("Add Remote Host…"))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.workspace_nav.menu = false;
                    cx.notify();
                    cx.emit(SidebarEvent::AddRemoteHost);
                })),
        );
        Some(panel)
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
