//! The horizontal header's project menu is a window overlay. Opening it must
//! never change sidebar visibility or resize an attached terminal.
use super::*;
use diri_proto::Project;

pub(super) struct ProjectPicker {
    open: bool,
    pub(super) new_agent: bool,
    query: query_editor::QueryEditor,
    focus: FocusHandle,
    previous_focus: Option<FocusHandle>,
    highlighted: usize,
    scroll: ScrollHandle,
    anchor: Rc<RefCell<Option<Bounds<Pixels>>>>,
}
impl ProjectPicker {
    pub(super) fn new(cx: &mut App) -> Self {
        Self {
            open: false,
            new_agent: false,
            query: Default::default(),
            focus: cx.focus_handle(),
            previous_focus: None,
            highlighted: 0,
            scroll: ScrollHandle::new(),
            anchor: Default::default(),
        }
    }
}

fn projects(store: &SessionStore, query: &str) -> Vec<Project> {
    let query = query.trim().to_lowercase();
    let mut projects: Vec<_> = store
        .projects()
        .values()
        .filter(|project| {
            query.is_empty()
                || format!(
                    "{} {} {}",
                    project.name,
                    project.root,
                    project.host.as_deref().unwrap_or("This Mac")
                )
                .to_lowercase()
                .contains(&query)
        })
        .cloned()
        .collect();
    let order = &store.preferences().sidebar_project_order;
    projects.sort_by(|a, b| {
        let rank = |project: &Project| {
            order
                .iter()
                .position(|id| id == &project.id)
                .unwrap_or(usize::MAX)
        };
        rank(a)
            .cmp(&rank(b))
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| a.id.0.cmp(&b.id.0))
    });
    projects
}

fn project_agent(store: &SessionStore, project: &ProjectId) -> Option<SessionId> {
    let usable = |session: &&Arc<SessionRecord>| {
        &session.project_id == project
            && !session.is_archived()
            && !crate::store::is_auxiliary_terminal(session)
    };
    if let Some(selected) = store.selected_session().filter(|session| {
        &session.project_id == project
            && !session.is_archived()
            && !crate::store::is_auxiliary_terminal(session)
    }) {
        return Some(selected.id.clone());
    }
    fn focused(
        node: &diri_proto::workspace::LayoutNode,
        pane: &diri_proto::workspace::PaneId,
    ) -> Option<SessionId> {
        match node {
            diri_proto::workspace::LayoutNode::Pane { id, session_id } => {
                (id == pane).then(|| session_id.clone())
            }
            diri_proto::workspace::LayoutNode::Split { first, second, .. } => {
                focused(first, pane).or_else(|| focused(second, pane))
            }
        }
    }
    if let Some(workspace) = store.workspace_catalog().snapshot().and_then(|snapshot| {
        snapshot
            .workspaces
            .iter()
            .find(|workspace| workspace.project_id.as_ref() == Some(project))
    }) && let Some(tab) = workspace
        .tabs
        .iter()
        .find(|tab| Some(&tab.id) == workspace.selected_tab.as_ref())
        && let Some(session) =
            focused(&tab.layout, &tab.focused_pane).and_then(|id| store.sessions().get(&id))
        && usable(&session)
    {
        return Some(session.id.clone());
    }
    store
        .sessions()
        .values()
        .filter(usable)
        .max_by(|a, b| {
            a.updated_at
                .0
                .total_cmp(&b.updated_at.0)
                .then_with(|| a.id.0.cmp(&b.id.0))
        })
        .map(|session| session.id.clone())
}

impl Sidebar {
    pub(super) fn project_control(
        &self,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let anchor = Rc::clone(&self.project_picker.anchor);
        div()
            .id("horizontal-tab-project")
            .debug_selector(|| "horizontal-tab-project".into())
            .role(Role::Button)
            .aria_label("Projects")
            .relative()
            .size(px(26.0))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(SIDEBAR_ROW_RADIUS))
            .cursor_pointer()
            .bg(colors
                .primary
                .alpha(if self.project_picker.open { 0.08 } else { 0.0 }))
            .hover(move |row| row.bg(colors.primary.alpha(0.06)))
            .child(sf_symbol("rectangle.stack", 16.0, colors.secondary))
            .child(
                gpui::canvas(
                    move |bounds, _, _| {
                        *anchor.borrow_mut() = Some(bounds);
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(|this, _, window, cx| {
                if this.project_picker.open {
                    this.dismiss_project_picker(window, cx);
                } else {
                    this.project_picker.previous_focus = window.focused(cx);
                    this.project_picker.open = true;
                    this.project_picker.new_agent = false;
                    this.project_picker.query.clear();
                    this.project_picker.highlighted = 0;
                    this.project_picker.focus.focus(window, cx);
                    cx.notify();
                }
                cx.stop_propagation();
            }))
            .into_any_element()
    }

    pub(super) fn dismiss_project_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.project_picker.open = false;
        cx.emit(SidebarEvent::ProjectPickerChanged);
        if let Some(previous) = self.project_picker.previous_focus.take() {
            previous.focus(window, cx);
        }
        cx.notify();
    }

    fn choose_project(&mut self, project: Project, window: &mut Window, cx: &mut Context<Self>) {
        let session = project_agent(&self.store.read().expect("store"), &project.id);
        let previous_focus = self.project_picker.previous_focus.clone();
        self.dismiss_project_picker(window, cx);
        if let Some(session) = session {
            self.store.write().expect("store").select(session);
            cx.emit(SidebarEvent::SessionActivated);
        } else {
            self.activate_workspace(None, cx);
            self.open_new_agent_popover_at(Some(project.root), project.host, cx);
            self.new_agent_anchor = self
                .project_picker
                .anchor
                .borrow()
                .map(|bounds| point(bounds.left(), bounds.bottom() + px(4.0)));
            self.project_picker.new_agent = true;
            self.project_picker.previous_focus = previous_focus;
            self.project_picker.focus.focus(window, cx);
        }
        cx.notify();
    }

    fn project_picker_key(
        &mut self,
        event: &gpui::KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let choices = projects(
            &self.store.read().expect("store"),
            self.project_picker.query.text(),
        );
        match event.keystroke.key.as_str() {
            "escape" => self.dismiss_project_picker(window, cx),
            "up" => {
                self.project_picker.highlighted = self.project_picker.highlighted.saturating_sub(1)
            }
            "down" => {
                self.project_picker.highlighted =
                    (self.project_picker.highlighted + 1).min(choices.len().saturating_sub(1))
            }
            "enter" => {
                if let Some(project) = choices.get(self.project_picker.highlighted) {
                    self.choose_project(project.clone(), window, cx);
                }
            }
            _ => {
                if let Some(edit) = query_editor::edit_for(&event.keystroke) {
                    match edit {
                        Edit::Local(edit) => {
                            self.project_picker.query.apply(edit);
                        }
                        Edit::Clipboard(ClipboardEdit::Copy) => {
                            query_editor::copy_selection(&self.project_picker.query, cx);
                        }
                        Edit::Clipboard(ClipboardEdit::Cut) => {
                            query_editor::cut_selection(&mut self.project_picker.query, cx);
                        }
                        Edit::Clipboard(ClipboardEdit::Paste) => {
                            if let Some(text) =
                                cx.read_from_clipboard().and_then(|item| item.text())
                            {
                                self.project_picker.query.insert(&text);
                            }
                        }
                    }
                    self.project_picker.highlighted = 0;
                }
            }
        }
        self.project_picker
            .scroll
            .scroll_to_item(self.project_picker.highlighted);
        cx.notify();
        cx.stop_propagation();
    }

    pub(super) fn open_header_new_agent(
        &mut self,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.project_picker.previous_focus = window.focused(cx);
        self.open_new_agent_popover(None, cx);
        self.new_agent_anchor = Some(point(
            position.x,
            px(crate::tab_navigation::TAB_STRIP_HEIGHT + 4.0),
        ));
        self.project_picker.new_agent = true;
        self.project_picker.focus.focus(window, cx);
        cx.emit(SidebarEvent::ProjectPickerChanged);
        cx.notify();
    }

    #[cfg(test)]
    pub(crate) fn project_picker_center_for_test(&self) -> Option<Point<Pixels>> {
        self.project_picker
            .anchor
            .borrow()
            .map(|bounds| bounds.center())
    }

    #[cfg(test)]
    pub(crate) fn project_picker_is_open_for_test(&self) -> bool {
        self.project_picker.open
    }

    pub(crate) fn project_picker_active(&self) -> bool {
        self.project_picker.open || self.project_picker.new_agent
    }

    fn header_new_agent_menu(
        &self,
        directory: Option<String>,
        host: Option<String>,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if let Some(host) = host.as_ref()
            && self.store.read().expect("store").host(host).is_none()
        {
            // A project's explicit remote location is never a local fallback.
            // Host reload may still be in flight, so retain the exact intent;
            // a subsequent store update can paint its normal agent choices.
            let content = div()
                .id("project-picker-remote-unavailable")
                .debug_selector(|| "project-picker-remote-unavailable".into())
                .p(px(14.0)).flex().flex_col().gap(px(8.0))
                .child(div().text_size(px(Typo::ROW.size)).text_color(colors.primary)
                    .child("Remote host unavailable"))
                .child(div().text_size(px(Typo::META.size)).text_color(colors.secondary)
                    .child(format!("The saved host “{host}” is unavailable. Restore this host to start an agent in this project.")))
                .child(div().id("project-picker-unavailable-dismiss")
                    .debug_selector(|| "project-picker-unavailable-dismiss".into())
                    .role(Role::Button).aria_label("Dismiss")
                    .py(px(6.0)).px(px(9.0)).rounded(px(7.0)).cursor_pointer()
                    .bg(colors.primary.alpha(0.06)).text_size(px(Typo::META.size)).text_color(colors.primary)
                    .child("Dismiss")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.ui.popover = None;
                        this.project_picker.new_agent = false;
                        this.dismiss_project_picker(window, cx);
                        cx.stop_propagation();
                    })));
            return self.popover_shell_at(
                self.new_agent_anchor
                    .unwrap_or_else(|| point(px(12.0), px(46.0))),
                Anchor::TopLeft,
                300.0,
                content,
                colors,
                cx,
            );
        }
        self.new_agent_popover(directory, host, colors, cx)
    }

    pub(crate) fn render_project_picker_overlay(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let colors = self.colors();
        if !self.project_picker.open {
            return if self.project_picker.new_agent {
                if let Some(Popover::NewAgent { directory, host }) = self.ui.popover.clone() {
                    Some(
                        div()
                            .absolute()
                            .inset_0()
                            .track_focus(&self.project_picker.focus)
                            .on_key_down(cx.listener(
                                |this, event: &gpui::KeyDownEvent, window, cx| {
                                    if event.keystroke.key == "escape" {
                                        this.ui.popover = None;
                                        this.project_picker.new_agent = false;
                                        this.dismiss_project_picker(window, cx);
                                        cx.stop_propagation();
                                    }
                                },
                            ))
                            .child(self.header_new_agent_menu(directory, host, colors, cx))
                            .into_any_element(),
                    )
                } else {
                    // Shared menu actions and the outside-click scrim dismiss
                    // ui.popover. Restore the header's saved focus as that menu
                    // leaves the render tree, just as its Escape path does.
                    if let Some(previous) = self.project_picker.previous_focus.take() {
                        previous.focus(window, cx);
                    }
                    self.project_picker.new_agent = false;
                    None
                }
            } else {
                None
            };
        }
        let bounds = (*self.project_picker.anchor.borrow())?;
        let width = 300.0_f32.min((f32::from(window.viewport_size().width) - 16.0).max(0.0));
        let left = f32::from(bounds.left())
            .min((f32::from(window.viewport_size().width) - width - 8.0).max(8.0));
        let top = f32::from(bounds.bottom()) + 4.0;
        let height = (f32::from(window.viewport_size().height) - top - 12.0).clamp(0.0, 420.0);
        let choices = projects(
            &self.store.read().expect("store"),
            self.project_picker.query.text(),
        );
        self.project_picker.highlighted = self
            .project_picker
            .highlighted
            .min(choices.len().saturating_sub(1));
        let selected = self
            .store
            .read()
            .expect("store")
            .selected_session()
            .map(|session| session.project_id.clone());
        let mut list = div()
            .id("project-picker-list")
            .track_scroll(&self.project_picker.scroll)
            .min_h(px(0.0))
            .overflow_y_scroll()
            .flex_1()
            .p(px(5.0))
            .flex()
            .flex_col()
            .gap(px(2.0));
        for (index, project) in choices.into_iter().enumerate() {
            let debug_id = project.id.0.clone();
            let current = selected.as_ref() == Some(&project.id);
            let secondary = project.host.as_ref().map_or_else(
                || project.root.clone(),
                |host| format!("{} · {}", host, project.root),
            );
            list = list.child(
                div()
                    .id(format!("project-picker-{}", project.id.0))
                    .debug_selector(move || format!("project-picker-{debug_id}"))
                    .role(Role::MenuItem)
                    .aria_label(project.name.clone())
                    .h(px(48.0))
                    .flex_none()
                    .px(px(9.0))
                    .rounded(px(8.0))
                    .flex()
                    .items_center()
                    .gap(px(9.0))
                    .cursor_pointer()
                    .bg(colors
                        .primary
                        .alpha(if index == self.project_picker.highlighted {
                            0.07
                        } else {
                            0.0
                        }))
                    .hover(move |row| row.bg(colors.primary.alpha(0.07)))
                    .child(sf_symbol("folder", 13.0, colors.secondary))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .flex()
                            .flex_col()
                            .gap(px(2.0))
                            .child(
                                div()
                                    .text_size(px(Typo::ROW.size))
                                    .text_color(colors.primary)
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    .child(project.name.clone()),
                            )
                            .child(
                                div()
                                    .text_size(px(Typo::META.size))
                                    .text_color(colors.tertiary)
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    .child(secondary),
                            ),
                    )
                    .when(current, |row| {
                        row.child(sf_symbol("checkmark", 10.0, colors.secondary))
                    })
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.choose_project(project.clone(), window, cx);
                        cx.stop_propagation();
                    })),
            );
        }
        let query = if self.project_picker.query.is_empty() {
            div()
                .text_color(colors.tertiary)
                .child("Search projects…")
                .into_any_element()
        } else {
            query_label(&self.project_picker.query)
        };
        Some(
            div()
                .id("project-picker-overlay")
                .debug_selector(|| "project-picker-overlay".into())
                .absolute()
                .inset_0()
                .occlude()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, window, cx| {
                        this.dismiss_project_picker(window, cx);
                        cx.stop_propagation();
                    }),
                )
                .child(
                    div()
                        .id("project-picker-popup")
                        .debug_selector(|| "project-picker-popup".into())
                        .role(Role::Menu)
                        .absolute()
                        .left(px(left))
                        .top(px(top))
                        .w(px(width))
                        .max_h(px(height))
                        .flex()
                        .flex_col()
                        .rounded(px(12.0))
                        .bg(colors.background)
                        .border_1()
                        .border_color(colors.floating_stroke())
                        .shadow_lg()
                        .occlude()
                        .track_focus(&self.project_picker.focus)
                        .on_key_down(cx.listener(Self::project_picker_key))
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .child(
                            div()
                                .id("project-picker-search")
                                .debug_selector(|| "project-picker-search".into())
                                .role(Role::TextInput)
                                .aria_label("Search projects")
                                .text_size(px(Typo::META.size))
                                .text_color(colors.primary)
                                .h(px(42.0))
                                .flex_none()
                                .px(px(13.0))
                                .flex()
                                .items_center()
                                .gap(px(8.0))
                                .border_b_1()
                                .border_color(colors.floating_stroke())
                                .child(sf_symbol("magnifyingglass", 12.0, colors.tertiary))
                                .child(query),
                        )
                        .child(list),
                )
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use diri_proto::workspace::{
        LayoutNode, PaneId, TabId, WorkspaceId, WorkspaceRecord, WorkspaceSnapshot, WorkspaceTab,
    };
    use gpui::{Modifiers, TestAppContext, VisualTestContext};

    struct Harness {
        sidebar: Entity<Sidebar>,
    }
    impl Render for Harness {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .relative()
                .size_full()
                .flex()
                .flex_col()
                .child(
                    self.sidebar
                        .update(cx, |sidebar, cx| sidebar.render_horizontal_tabs(900.0, cx)),
                )
                .children(self.sidebar.update(cx, |sidebar, cx| {
                    sidebar.render_project_picker_overlay(window, cx)
                }))
        }
    }
    fn harness(cx: &mut TestAppContext, active: bool) -> (Entity<Sidebar>, &mut VisualTestContext) {
        cx.update(|cx| cx.set_reduce_motion(true));
        let (view, cx) = cx.add_window_view(move |_, cx| {
            let sidebar = cx.new(|cx| {
                let mut sidebar = Sidebar::new(None, true, PreviewScenario::Typical, cx);
                sidebar
                    .set_tab_orientation(crate::store::TabOrientation::Horizontal, cx)
                    .unwrap();
                if active {
                    let session_id = sidebar
                        .store
                        .read()
                        .unwrap()
                        .selected_session_id()
                        .unwrap()
                        .clone();
                    let workspace = WorkspaceId::new("project_view");
                    let tab = TabId::new("project_tab");
                    let pane = PaneId::new("project_pane");
                    sidebar
                        .store
                        .write()
                        .unwrap()
                        .seed_workspace_snapshot_for_test(WorkspaceSnapshot {
                            workspaces: vec![WorkspaceRecord {
                                project_id: None,
                                id: workspace.clone(),
                                name: "Legacy label".into(),
                                selected_tab: Some(tab.clone()),
                                tabs: vec![WorkspaceTab {
                                    id: tab,
                                    title: None,
                                    layout: LayoutNode::Pane {
                                        id: pane.clone(),
                                        session_id,
                                    },
                                    focused_pane: pane,
                                    zoomed_pane: None,
                                }],
                            }],
                            ..Default::default()
                        });
                    sidebar.workspace_nav.active = Some(workspace);
                }
                sidebar
            });
            cx.observe(&sidebar, |_, _, cx| cx.notify()).detach();
            Harness { sidebar }
        });
        (view.read_with(cx, |view, _| view.sidebar.clone()), cx)
    }

    fn open(cx: &mut VisualTestContext) -> Bounds<Pixels> {
        let button = cx
            .debug_bounds("horizontal-tab-project")
            .expect("single Projects header button");
        assert!(
            cx.debug_bounds("workspace-picker").is_none(),
            "duplicate All sessions/layout picker removed"
        );
        cx.simulate_click(button.center(), Modifiers::default());
        let popup = cx
            .debug_bounds("project-picker-popup")
            .expect("project popup");
        assert!(
            popup.top() >= button.bottom(),
            "popup anchors beneath header"
        );
        assert!(popup.left() >= button.left() - px(1.0));
        popup
    }

    #[gpui::test]
    fn project_picker_in_horizontal_header_never_reveals_or_changes_sidebar(
        cx: &mut TestAppContext,
    ) {
        let (sidebar, cx) = harness(cx, false);
        let before = sidebar.read_with(cx, |sidebar, _| {
            sidebar.store.read().unwrap().preferences().clone()
        });
        open(cx);
        sidebar.read_with(cx, |sidebar, _| {
            assert!(sidebar.project_picker_center_for_test().is_some());
            assert!(sidebar.project_picker_is_open_for_test());
            assert!(!sidebar.is_visible());
            assert!(!sidebar.is_peeking());
            assert_eq!(sidebar.store.read().unwrap().preferences(), &before);
        });
        cx.simulate_keystrokes("escape");
        assert!(cx.debug_bounds("project-picker-popup").is_none());
        open(cx);
        cx.simulate_click(point(px(700.0), px(500.0)), Modifiers::default());
        assert!(cx.debug_bounds("project-picker-popup").is_none());
        sidebar.read_with(cx, |sidebar, _| {
            assert!(!sidebar.is_visible());
            assert!(!sidebar.is_peeking());
            assert_eq!(sidebar.store.read().unwrap().preferences(), &before);
        });
    }

    #[gpui::test]
    fn a_dragged_horizontal_tab_lifts_itself_and_crosses_its_neighbour_at_the_midline(
        cx: &mut TestAppContext,
    ) {
        let (sidebar, cx) = harness(cx, false);
        let order = |sidebar: &Entity<Sidebar>, cx: &VisualTestContext| -> Vec<String> {
            sidebar.read_with(cx, |sidebar, _| {
                sidebar
                    .visible_tab_order()
                    .into_iter()
                    .map(|id| id.0)
                    .collect()
            })
        };
        // Two adjacent unpinned siblings: pinned rows always sort first, so
        // a tab can never trade places across the pin boundary.
        let (first, second) = sidebar.read_with(cx, |sidebar, _| {
            let mut store = sidebar.store.write().unwrap();
            let projection = store.sidebar_projection();
            let run = sibling_run(&projection, &SessionId::new("preview-claude"));
            let pinned = &store.preferences().sidebar_pinned_sessions;
            run.windows(2)
                .find(|pair| !pinned.contains(&pair[0]) && !pinned.contains(&pair[1]))
                .map(|pair| (pair[0].clone(), pair[1].clone()))
                .expect("two adjacent unpinned siblings")
        });
        let selector = |id: &SessionId| -> &'static str {
            Box::leak(format!("horizontal-tab-{}", id.0).into_boxed_str())
        };
        let before = order(&sidebar, cx);
        let a = cx
            .debug_bounds(selector(&first))
            .expect("first sibling tab");
        let b = cx
            .debug_bounds(selector(&second))
            .expect("second sibling tab");
        assert!(a.left() < b.left(), "siblings run left to right");
        assert!(cx.debug_bounds("LIFTED_ROW").is_none());

        let grab = a.center();
        cx.simulate_mouse_down(grab, MouseButton::Left, Modifiers::default());
        // Cross GPUI's drag threshold 4px to the right; the grab offset is
        // taken from this moment.
        cx.simulate_mouse_move(
            grab + point(px(4.0), px(0.0)),
            MouseButton::Left,
            Modifiers::default(),
        );
        let near_edge = point(b.left() + px(6.0), grab.y + px(20.0));
        cx.simulate_mouse_move(near_edge, MouseButton::Left, Modifiers::default());

        let lifted = cx
            .debug_bounds("LIFTED_ROW")
            .expect("dragging a tab lifts the tab itself");
        assert_eq!(lifted.size, a.size, "the lifted tab keeps its size");
        assert_eq!(
            lifted.origin,
            point(a.origin.x + (near_edge.x - grab.x - px(4.0)), a.origin.y),
            "a strip's tab travels only along the strip"
        );
        assert_eq!(
            cx.debug_bounds(selector(&first)),
            Some(a),
            "the tab stays in the strip as the slot it returns to"
        );
        assert_eq!(
            order(&sidebar, cx),
            before,
            "touching a neighbour's edge is not a crossing"
        );

        let past_midline = point(b.center().x + px(6.0), grab.y);
        cx.simulate_mouse_move(past_midline, MouseButton::Left, Modifiers::default());
        let after = order(&sidebar, cx);
        let position = |list: &[String], id: &SessionId| {
            list.iter()
                .position(|candidate| *candidate == id.0)
                .expect("tab is in the strip")
        };
        assert!(
            position(&after, &second) < position(&after, &first),
            "passing the midline trades places: {after:?}"
        );
        // The strip flattens a session tree, so a parent takes its subtree
        // along; every other tab keeps its relative order.
        let others = |list: &[String]| -> Vec<String> {
            list.iter()
                .filter(|id| **id != first.0 && **id != second.0)
                .cloned()
                .collect()
        };
        assert_eq!(others(&before), others(&after));

        cx.simulate_mouse_up(past_midline, MouseButton::Left, Modifiers::default());
        assert!(cx.debug_bounds("LIFTED_ROW").is_none());
        assert_eq!(
            order(&sidebar, cx),
            after,
            "the release keeps the new order"
        );
        assert!(
            sidebar.read_with(cx, |sidebar, _| !sidebar.ui.order_dirty),
            "the release wrote the staged order"
        );
    }

    #[gpui::test]
    fn active_layout_uses_the_same_single_project_header_picker(cx: &mut TestAppContext) {
        let (sidebar, cx) = harness(cx, true);
        let before = sidebar.read_with(cx, |sidebar, _| {
            sidebar
                .store
                .read()
                .unwrap()
                .workspace_catalog()
                .snapshot()
                .unwrap()
                .clone()
        });
        assert!(cx.debug_bounds("horizontal-workspace-tabs").is_some());
        open(cx);
        cx.simulate_keystrokes("escape");
        sidebar.read_with(cx, |sidebar, _| {
            assert!(!sidebar.is_visible());
            assert!(!sidebar.is_peeking());
            assert_eq!(
                sidebar.store.read().unwrap().workspace_catalog().snapshot(),
                Some(&before)
            );
        });
    }

    #[gpui::test]
    fn project_picker_filter_keyboard_selects_an_agent_without_sidebar_navigation(
        cx: &mut TestAppContext,
    ) {
        let (sidebar, cx) = harness(cx, false);
        open(cx);
        cx.simulate_keystrokes("d i r i j o r");
        let expected = sidebar.read_with(cx, |sidebar, _| {
            assert_eq!(sidebar.project_picker.query.text(), "dirijor");
            let store = sidebar.store.read().unwrap();
            let choices = projects(&store, "dirijor");
            assert_eq!(choices.len(), 1);
            project_agent(&store, &choices[0].id).unwrap()
        });
        cx.simulate_keystrokes("down up enter");
        assert!(cx.debug_bounds("project-picker-popup").is_none());
        sidebar.read_with(cx, |sidebar, _| {
            assert_eq!(
                sidebar.store.read().unwrap().selected_session_id(),
                Some(&expected)
            );
            assert!(!sidebar.is_visible());
            assert!(!sidebar.is_peeking());
        });
    }

    #[test]
    fn returning_to_a_project_prefers_saved_focus_over_background_activity() {
        let fixture = SidebarPreviewFixture::make(PreviewScenario::Typical);
        let (mut store, _effects) = SessionStore::headless(fixture.prefs);
        store.hydrate(fixture.list);
        let target = store
            .sessions()
            .values()
            .find(|session| !session.is_archived())
            .unwrap()
            .project_id
            .clone();
        let saved = store
            .sessions()
            .values()
            .filter(|session| session.project_id == target && !session.is_archived())
            .min_by(|a, b| a.updated_at.0.total_cmp(&b.updated_at.0))
            .unwrap()
            .id
            .clone();
        let elsewhere = store
            .sessions()
            .values()
            .find(|session| session.project_id != target && !session.is_archived())
            .unwrap()
            .id
            .clone();
        store.select(elsewhere);
        let pane = PaneId::new("saved_pane");
        let tab = TabId::new("saved_tab");
        store.seed_workspace_snapshot_for_test(WorkspaceSnapshot {
            workspaces: vec![WorkspaceRecord {
                project_id: Some(target.clone()),
                id: WorkspaceId::new("saved_workspace"),
                name: "Saved".into(),
                selected_tab: Some(tab.clone()),
                tabs: vec![WorkspaceTab {
                    id: tab,
                    title: None,
                    layout: LayoutNode::Pane {
                        id: pane.clone(),
                        session_id: saved.clone(),
                    },
                    focused_pane: pane,
                    zoomed_pane: None,
                }],
            }],
            ..Default::default()
        });
        assert_eq!(project_agent(&store, &target), Some(saved));
    }

    #[gpui::test]
    fn header_agent_menu_outside_dismissal_restores_previous_focus(cx: &mut TestAppContext) {
        let (sidebar, cx) = harness(cx, false);
        let previous = cx.update(|window, cx| {
            let focus = cx.focus_handle();
            focus.focus(window, cx);
            focus
        });
        open(cx);
        let project = Project {
            id: ProjectId("empty".into()),
            root: "/tmp/empty-project".into(),
            name: "Empty".into(),
            pinned_order: None,
            host: None,
        };
        cx.update(|window, cx| {
            sidebar.update(cx, |sidebar, cx| {
                sidebar.choose_project(project, window, cx)
            })
        });
        assert!(cx.debug_bounds("sidebar-popover").is_some());
        assert_ne!(
            cx.update(|window, cx| window.focused(cx)),
            Some(previous.clone())
        );
        cx.simulate_click(point(px(700.0), px(500.0)), Modifiers::default());
        assert!(cx.debug_bounds("sidebar-popover").is_none());
        assert_eq!(cx.update(|window, cx| window.focused(cx)), Some(previous));
        sidebar.read_with(cx, |sidebar, _| {
            assert!(!sidebar.project_picker_active());
            assert!(!sidebar.is_visible());
            assert!(!sidebar.is_peeking());
        });
    }

    #[gpui::test]
    fn empty_remote_project_with_missing_host_never_offers_local_agent_choices(
        cx: &mut TestAppContext,
    ) {
        let (sidebar, cx) = harness(cx, false);
        open(cx);
        let project = Project {
            id: ProjectId("remote-empty".into()),
            root: "/srv/remote-project".into(),
            name: "Remote project".into(),
            pinned_order: None,
            host: Some("removed-host".into()),
        };
        cx.update(|window, cx| {
            sidebar.update(cx, |sidebar, cx| {
                sidebar.choose_project(project, window, cx)
            })
        });
        assert!(
            cx.debug_bounds("project-picker-remote-unavailable")
                .is_some()
        );
        assert!(cx.debug_bounds("AGENT_OPTION_0").is_none());
        sidebar.read_with(cx, |sidebar, _| {
            assert_eq!(
                sidebar.ui.popover,
                Some(Popover::NewAgent {
                    directory: Some("/srv/remote-project".into()),
                    host: Some("removed-host".into())
                })
            );
            assert!(!sidebar.is_visible());
            assert!(!sidebar.is_peeking());
        });
        let dismiss = cx
            .debug_bounds("project-picker-unavailable-dismiss")
            .unwrap();
        cx.simulate_click(dismiss.center(), Modifiers::default());
        assert!(
            cx.debug_bounds("project-picker-remote-unavailable")
                .is_none()
        );
        sidebar.read_with(cx, |sidebar, _| assert!(!sidebar.project_picker_active()));
    }

    #[gpui::test]
    fn project_picker_empty_project_opens_agent_menu_at_its_exact_location(
        cx: &mut TestAppContext,
    ) {
        let (sidebar, cx) = harness(cx, true);
        let project = Project {
            id: ProjectId("empty".into()),
            root: "/tmp/empty-project".into(),
            name: "Empty".into(),
            pinned_order: None,
            host: None,
        };
        cx.update(|window, cx| {
            sidebar.update(cx, |sidebar, cx| {
                sidebar.choose_project(project, window, cx)
            })
        });
        sidebar.read_with(cx, |sidebar, _| {
            assert_eq!(
                sidebar.ui.popover,
                Some(Popover::NewAgent {
                    directory: Some("/tmp/empty-project".into()),
                    host: None
                })
            );
            assert!(
                sidebar.workspace_nav.active.is_none(),
                "launch must not inherit another project's layout"
            );
            assert!(!sidebar.is_visible());
            assert!(!sidebar.is_peeking());
        });
        cx.simulate_keystrokes("escape");
        sidebar.read_with(cx, |sidebar, _| assert!(sidebar.ui.popover.is_none()));
    }
}
