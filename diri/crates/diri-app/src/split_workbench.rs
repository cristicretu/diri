//! Desktop split views. Closing a pane drops its attachment, never its session.
use std::{collections::HashMap, sync::Arc};

use diri_proto::{AgentKind, SessionId, SessionSpawnParams};
use gpui::{
    AnyElement, App, Context, CursorStyle, DragMoveEvent, Entity, EventEmitter, FocusHandle,
    KeyDownEvent, MouseButton, Render, ScrollHandle, SharedString, Window, div, prelude::*, px,
};

use crate::{
    icons::sf_symbol,
    navigation::NavigationOverlay,
    quote::Quote,
    split_layout::{Direction, Divider, MAX_PANES, Rect, SplitAxis, SplitLayouts},
    store::StoreRuntime,
    surface_shell::UtilitySurfaces,
    terminal_pane::{TerminalPane, TerminalPaneEvent, TerminalViewport},
};

#[derive(Clone)]
struct DraggedDivider(Divider);
impl Render for DraggedDivider {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        gpui::Empty
    }
}

pub struct SplitWorkbench {
    runtime: Arc<StoreRuntime>,
    tokio: Arc<tokio::runtime::Runtime>,
    fallback: Option<Entity<TerminalPane>>,
    focus: FocusHandle,
    picker_index: usize,
    picker_scroll: ScrollHandle,
    focus_pending: bool,
    resize_active: bool,
    layouts: SplitLayouts,
    panes: HashMap<SessionId, Entity<TerminalPane>>,
    selected: Option<SessionId>,
    viewport: Rect,
    navigation: Option<Entity<NavigationOverlay>>,
    utilities: Option<Entity<UtilitySurfaces>>,
    sidebar_visible: bool,
    inspector_open: bool,
    pending: bool,
    auxiliary_pending: Option<SessionId>,
    picker: Option<SplitAxis>,
    error: Option<String>,
}

impl EventEmitter<TerminalPaneEvent> for SplitWorkbench {}

impl SplitWorkbench {
    pub fn new(
        runtime: Arc<StoreRuntime>,
        tokio: Arc<tokio::runtime::Runtime>,
        fallback: Option<Entity<TerminalPane>>,
        navigation: Option<Entity<NavigationOverlay>>,
        utilities: Option<Entity<UtilitySurfaces>>,
        cx: &mut Context<Self>,
    ) -> Self {
        let layouts = runtime
            .store
            .read()
            .expect("session store lock poisoned")
            .preferences()
            .split_layouts
            .clone();
        Self {
            runtime,
            tokio,
            fallback,
            focus: cx.focus_handle(),
            picker_index: 0,
            picker_scroll: ScrollHandle::new(),
            focus_pending: false,
            resize_active: false,
            layouts,
            panes: HashMap::new(),
            selected: None,
            viewport: Rect::default(),
            navigation,
            utilities,
            sidebar_visible: true,
            inspector_open: false,
            pending: false,
            auxiliary_pending: None,
            picker: None,
            error: None,
        }
    }

    pub fn active_for_selection(&self) -> bool {
        let store = self
            .runtime
            .store
            .read()
            .expect("session store lock poisoned");
        store
            .selected_session_id()
            .is_some_and(|id| self.layouts.containing(id).is_some())
    }

    pub fn sync(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(target) = self.auxiliary_pending.clone() {
            let (auxiliary, failed) = {
                let store = self
                    .runtime
                    .store
                    .read()
                    .expect("session store lock poisoned");
                (
                    store.auxiliary_terminal_for(&target),
                    store.last_action_error().is_some(),
                )
            };
            if let Some(auxiliary) = auxiliary {
                self.auxiliary_pending = None;
                if self
                    .layouts
                    .split(target.clone(), auxiliary.id.clone(), SplitAxis::Below)
                {
                    self.persist();
                    if self
                        .runtime
                        .store
                        .read()
                        .expect("session store lock poisoned")
                        .selected_session_id()
                        == Some(&target)
                    {
                        self.select(auxiliary.id.clone(), window, cx);
                    }
                }
            } else if failed {
                self.auxiliary_pending = None;
            }
        }

        let (selected, live, connected) = {
            let store = self
                .runtime
                .store
                .read()
                .expect("session store lock poisoned");
            (
                store.selected_session_id().cloned(),
                store
                    .sessions()
                    .values()
                    .filter(|session| !session.is_archived())
                    .map(|session| session.id.clone())
                    .collect::<std::collections::HashSet<_>>(),
                store.has_hydrated_sessions(),
            )
        };
        if connected {
            let before = self.layouts.clone();
            self.layouts.reconcile(|id| live.contains(id));
            if self.layouts != before {
                self.persist();
            }
        }
        let changed_selection = self.selected != selected;
        self.selected = selected;
        let ids = self
            .selected
            .as_ref()
            .and_then(|id| self.layouts.containing(id))
            .map(|tree| tree.ids())
            .unwrap_or_default();
        // Drop old owners before making any new attachments.
        self.panes.retain(|id, _| ids.contains(id));
        for id in ids {
            if self.panes.contains_key(&id) {
                continue;
            }
            let terminal = cx.new(|cx| {
                TerminalPane::new_fixed(
                    Arc::clone(&self.runtime),
                    Arc::clone(&self.tokio),
                    id.clone(),
                    window,
                    cx,
                )
            });
            terminal.update(cx, |terminal, _| {
                terminal.set_select_on_focus();
                if let (Some(nav), Some(utilities)) = (&self.navigation, &self.utilities) {
                    terminal.set_shell_entities(nav.clone(), utilities.clone());
                }
            });
            cx.subscribe(&terminal, |_, _, event: &TerminalPaneEvent, cx| {
                cx.emit(event.clone())
            })
            .detach();
            self.panes.insert(id, terminal);
        }
        if changed_selection || self.focus_pending {
            self.focus_selected(window, cx);
            self.focus_pending = false;
        }
        cx.notify();
    }

    fn persist(&self) {
        self.runtime
            .store
            .write()
            .expect("session store lock poisoned")
            .remember_split_layouts(self.layouts.clone());
    }

    pub fn set_viewport(&mut self, viewport: Rect, sidebar_visible: bool, inspector_open: bool) {
        self.viewport = viewport;
        self.sidebar_visible = sidebar_visible;
        self.inspector_open = inspector_open;
    }

    pub fn focused_terminal(&self, window: &Window, cx: &App) -> Option<&Entity<TerminalPane>> {
        self.panes
            .values()
            .find(|pane| pane.read(cx).is_focused(window))
    }

    pub fn quote_selection(&self, cx: &App) -> Option<Quote> {
        self.selected
            .as_ref()
            .and_then(|id| self.panes.get(id))
            .and_then(|pane| pane.read(cx).quote_selection())
    }

    pub fn focus_selected(&self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(terminal) = self.selected.as_ref().and_then(|id| self.panes.get(id)) {
            terminal.update(cx, |terminal, cx| terminal.focus(window, cx));
        } else if let Some(terminal) = &self.fallback {
            terminal.update(cx, |terminal, cx| terminal.focus(window, cx));
        }
    }

    fn select(&mut self, id: SessionId, window: &mut Window, cx: &mut Context<Self>) {
        self.runtime
            .store
            .write()
            .expect("session store lock poisoned")
            .select(id.clone());
        self.focus_pending = !self.panes.contains_key(&id);
        self.selected = Some(id);
        self.focus_selected(window, cx);
        self.runtime.publish_local_change();
        cx.notify();
    }

    pub fn focus_direction(
        &mut self,
        direction: Direction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(selected) = &self.selected else {
            return;
        };
        if let Some(next) = self
            .layouts
            .containing(selected)
            .and_then(|tree| tree.neighbor(selected, direction, self.viewport))
        {
            self.select(next, window, cx);
        }
    }

    pub fn focus_next(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(selected) = &self.selected else {
            return;
        };
        if let Some(tree) = self.layouts.containing(selected) {
            let ids = tree.ids();
            let index = ids.iter().position(|id| id == selected).unwrap_or_default();
            self.select(ids[(index + 1) % ids.len()].clone(), window, cx);
        }
    }

    pub fn close_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(selected) = self.selected.clone() else {
            return false;
        };
        self.close(&selected, window, cx)
    }

    fn close(&mut self, id: &SessionId, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(next) = self.layouts.close(id) else {
            return false;
        };
        self.panes.remove(id);
        let parent = self
            .runtime
            .store
            .read()
            .expect("session store lock poisoned")
            .sessions()
            .get(id)
            .filter(|session| crate::store::is_auxiliary_terminal(session))
            .and_then(|session| session.parent.clone());
        if let Some(parent) = parent
            && !self.layouts.hidden_auxiliary_parents.contains(&parent)
        {
            self.layouts.hidden_auxiliary_parents.push(parent);
            if self.layouts.hidden_auxiliary_parents.len() > 64 {
                self.layouts.hidden_auxiliary_parents.remove(0);
            }
        }
        self.persist();
        self.select(next, window, cx);
        // A two-pane workspace collapses to the following terminal. Focus it
        // again after sync drops the old fixed entity.
        self.focus_pending = true;
        true
    }

    pub fn auxiliary_hidden_for(&self, parent: &SessionId) -> bool {
        self.layouts.hidden_auxiliary_parents.contains(parent)
    }

    pub fn show_auxiliary_for(&mut self, parent: &SessionId) -> bool {
        let hidden = self.auxiliary_hidden_for(parent);
        if hidden {
            self.layouts
                .hidden_auxiliary_parents
                .retain(|id| id != parent);
            self.persist();
        }
        hidden
    }

    pub fn include_auxiliary(&mut self, auxiliary: SessionId) {
        let target = self
            .runtime
            .store
            .read()
            .expect("session store lock poisoned")
            .selected_session_id()
            .cloned();
        if let Some(target) = target
            && self.layouts.split(target, auxiliary, SplitAxis::Below)
        {
            self.persist();
            self.runtime.publish_local_change();
        }
    }

    pub fn toggle_auxiliary(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(selected) = self.selected.clone() else {
            return;
        };
        let (target, auxiliary) = {
            let store = self
                .runtime
                .store
                .read()
                .expect("session store lock poisoned");
            let target = store
                .sessions()
                .get(&selected)
                .filter(|session| crate::store::is_auxiliary_terminal(session))
                .and_then(|session| session.parent.clone())
                .unwrap_or(selected);
            let auxiliary = store.auxiliary_terminal_for(&target);
            (target, auxiliary)
        };
        if let Some(auxiliary) = auxiliary {
            self.show_auxiliary_for(&target);
            if self
                .layouts
                .containing(&target)
                .is_some_and(|tree| tree.contains(&auxiliary.id))
            {
                self.close(&auxiliary.id, window, cx);
            } else if self
                .layouts
                .split(target, auxiliary.id.clone(), SplitAxis::Below)
            {
                self.persist();
                self.select(auxiliary.id.clone(), window, cx);
            }
        } else if self.auxiliary_pending.is_none()
            && self
                .layouts
                .containing(&target)
                .is_none_or(|tree| tree.ids().len() < MAX_PANES)
            && self
                .runtime
                .store
                .write()
                .expect("session store lock poisoned")
                .spawn_auxiliary_terminal(target.clone())
        {
            self.auxiliary_pending = Some(target);
        }
        cx.notify();
    }

    pub fn has_overlay(&self) -> bool {
        self.picker.is_some() || self.error.is_some() || self.pending
    }

    pub fn open_picker(&mut self, axis: SplitAxis, window: &mut Window, cx: &mut Context<Self>) {
        self.picker = Some(axis);
        self.picker_index = 0;
        self.picker_scroll.scroll_to_item(0);
        window.focus(&self.focus, cx);
        self.error = None;
        cx.notify();
    }

    pub fn add_existing(
        &mut self,
        id: SessionId,
        axis: SplitAxis,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.pending {
            return;
        }
        let target = self
            .runtime
            .store
            .read()
            .expect("session store lock poisoned")
            .selected_session_id()
            .cloned();
        if let Some(target) = target {
            if self.layouts.split(target, id.clone(), axis) {
                self.persist();
                self.picker = None;
                self.select(id, window, cx);
            } else {
                self.error = Some(format!(
                    "A workspace can contain up to {MAX_PANES} distinct panes."
                ));
                cx.notify();
            }
        }
    }

    pub fn spawn(&mut self, axis: SplitAxis, window: &mut Window, cx: &mut Context<Self>) {
        if self.pending {
            return;
        }
        let session = self
            .runtime
            .store
            .read()
            .expect("session store lock poisoned")
            .selected_session()
            .cloned();
        let Some(session) = session else {
            return;
        };
        let target = session.id.clone();
        if self
            .layouts
            .containing(&target)
            .is_some_and(|tree| tree.ids().len() >= MAX_PANES)
        {
            self.error = Some(format!("This workspace already has {MAX_PANES} panes."));
            cx.notify();
            return;
        }
        let params = SessionSpawnParams {
            kind: AgentKind::SHELL,
            cwd: session.cwd.clone(),
            new_worktree: None,
            worktree_branch: None,
            worktree_base: None,
            title: Some("Split terminal".to_owned()),
            initial_prompt: None,
            parent: None,
            initial_cols: None,
            initial_rows: None,
            host: session.host.clone(),
            account_profile_id: None,
            same_repo_as: None,
        };
        let client = Arc::clone(self.runtime.client());
        let spawn = self.tokio.spawn(async move {
            let id = client
                .spawn(params)
                .await
                .map_err(|error| format!("Could not open terminal: {error}"))?;
            let sessions = client.sessions().await.map_err(|error| {
                format!(
                    "Terminal created; could not load its pane: {error}. Open it from the sidebar."
                )
            })?;
            sessions
                .sessions
                .into_iter()
                .find(|session| session.id == id)
                .ok_or_else(|| "Terminal created; its session is no longer available.".to_owned())
        });
        self.pending = true;
        self.error = None;
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let result = spawn.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.pending = false;
                match result {
                    Ok(Ok(record)) => {
                        let id = record.id.clone();
                        let mut store = this.runtime.store.write().expect("session store lock poisoned");
                        if !store.sessions().contains_key(&id) { store.upsert_session(record); }
                        let still_selected = store.selected_session_id() == Some(&target);
                        let target_exists = store.sessions().contains_key(&target);
                        drop(store);
                        if target_exists && this.layouts.split(target, id.clone(), axis) {
                            this.persist();
                            if still_selected { this.select(id, window, cx); }
                            this.picker = None;
                        } else {
                            this.error = Some("Terminal created. Open it from the sidebar; the original pane changed while it was opening.".to_owned());
                        }
                        this.runtime.publish_local_change();
                    }
                    Ok(Err(error)) => this.error = Some(error),
                    Err(_) => this.error = Some("Could not open terminal. Try again.".to_owned()),
                }
                cx.notify();
            });
        }).detach();
    }

    fn divider(&self, divider: Divider, cx: &mut Context<Self>) -> AnyElement {
        let colors = {
            let store = self
                .runtime
                .store
                .read()
                .expect("session store lock poisoned");
            crate::app_theme::colors(&store.preferences().terminal_theme)
        };
        let reset_path = divider.path.clone();
        let drag = DraggedDivider(divider.clone());
        div()
            .id(SharedString::from(format!(
                "split-divider-{:?}",
                divider.path
            )))
            .absolute()
            .left(px(divider.rect.x))
            .top(px(divider.rect.y))
            .w(px(divider.rect.width))
            .h(px(divider.rect.height))
            .cursor(match divider.axis {
                SplitAxis::Right => CursorStyle::ResizeLeftRight,
                SplitAxis::Below => CursorStyle::ResizeUpDown,
            })
            .bg(colors.primary.alpha(0.05))
            .hover(|s| s.bg(colors.primary.alpha(0.20)))
            .on_drag(drag, |drag, _, _, cx| {
                cx.stop_propagation();
                cx.new(|_| drag.clone())
            })
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(move |this, event: &gpui::MouseUpEvent, _, cx| {
                    if event.click_count == 2
                        && let Some(tree) = this
                            .selected
                            .as_ref()
                            .and_then(|id| this.layouts.containing_mut(id))
                    {
                        tree.resize(&reset_path, 0.5);
                    }
                    this.persist();
                    cx.notify();
                }),
            )
            .into_any_element()
    }

    fn picker_sessions(&self) -> Vec<Arc<diri_proto::SessionRecord>> {
        let store = self
            .runtime
            .store
            .read()
            .expect("session store lock poisoned");
        let selected = store.selected_session_id();
        let mut sessions: Vec<_> = store
            .sessions()
            .values()
            .filter(|session| {
                !session.is_archived()
                    && selected != Some(&session.id)
                    && !selected
                        .and_then(|id| self.layouts.containing(id))
                        .is_some_and(|tree| tree.contains(&session.id))
            })
            .cloned()
            .collect();
        sessions.sort_by(|a, b| a.title.cmp(&b.title).then(a.id.0.cmp(&b.id.0)));
        sessions
    }

    fn picker_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let Some(axis) = self.picker else {
            return;
        };
        let sessions = self.picker_sessions();
        match event.keystroke.key.as_str() {
            "escape" => {
                self.picker = None;
                self.error = None;
                self.focus_selected(window, cx);
            }
            "up" => self.picker_index = self.picker_index.saturating_sub(1),
            "down" => self.picker_index = (self.picker_index + 1).min(sessions.len()),
            "left" => self.picker = Some(SplitAxis::Right),
            "right" => self.picker = Some(SplitAxis::Below),
            "enter" if self.picker_index == 0 => self.spawn(axis, window, cx),
            "enter" => {
                if let Some(session) = sessions.get(self.picker_index - 1) {
                    self.add_existing(session.id.clone(), axis, window, cx);
                }
            }
            _ => return,
        }
        if self.picker_index > 0 {
            self.picker_scroll.scroll_to_item(self.picker_index - 1);
        }
        cx.stop_propagation();
        cx.notify();
    }

    fn picker(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let axis = self.picker?;
        let store = self
            .runtime
            .store
            .read()
            .expect("session store lock poisoned");
        let colors = crate::app_theme::colors(&store.preferences().terminal_theme);
        drop(store);
        let sessions = self.picker_sessions();
        Some(
            div()
                .absolute()
                .inset_0()
                .bg(colors.background.alpha(0.65))
                .flex()
                .items_center()
                .justify_center()
                .occlude()
                .child(
                    div()
                        .w(px(360.0))
                        .max_h(px(460.0))
                        .p(px(14.0))
                        .rounded(px(12.0))
                        .bg(colors.background)
                        .border_1()
                        .border_color(colors.primary.alpha(0.15))
                        .shadow_lg()
                        .flex()
                        .flex_col()
                        .gap(px(8.0))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .justify_between()
                                .child("Add a pane")
                                .child(
                                    div()
                                        .id("dismiss-split-picker")
                                        .cursor_pointer()
                                        .p(px(6.0))
                                        .child(sf_symbol("xmark", 11.0, colors.secondary))
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.picker = None;
                                            this.error = None;
                                            this.focus_selected(window, cx);
                                            cx.notify();
                                        })),
                                ),
                        )
                        .child(
                            div().flex().gap(px(6.0)).children(
                                [(SplitAxis::Right, "Right"), (SplitAxis::Below, "Below")]
                                    .into_iter()
                                    .map(|(candidate, label)| {
                                        div()
                                            .id(SharedString::from(format!("split-axis-{label}")))
                                            .px(px(12.0))
                                            .py(px(6.0))
                                            .rounded(px(5.0))
                                            .cursor_pointer()
                                            .bg(colors.primary.alpha(if axis == candidate {
                                                0.14
                                            } else {
                                                0.04
                                            }))
                                            .text_size(px(12.0))
                                            .child(label)
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                this.picker = Some(candidate);
                                                cx.notify();
                                            }))
                                    }),
                            ),
                        )
                        .child(
                            div()
                                .id("split-new-shell")
                                .when(self.picker_index == 0, |row| {
                                    row.bg(colors.primary.alpha(0.1))
                                })
                                .p(px(10.0))
                                .rounded(px(6.0))
                                .cursor_pointer()
                                .hover(|s| s.bg(colors.primary.alpha(0.08)))
                                .child(if self.pending {
                                    "Opening terminal…"
                                } else {
                                    "New terminal in this directory"
                                })
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.spawn(axis, window, cx)
                                })),
                        )
                        .child(
                            div()
                                .text_size(px(11.0))
                                .text_color(colors.secondary)
                                .child("Or choose an existing session"),
                        )
                        .child(
                            div()
                                .id("split-session-options")
                                .track_scroll(&self.picker_scroll)
                                .overflow_y_scroll()
                                .min_h_0()
                                .children(sessions.into_iter().enumerate().map(
                                    |(index, session)| {
                                        let id = session.id.clone();
                                        div()
                                            .id(SharedString::from(format!(
                                                "split-existing-{}",
                                                id.0
                                            )))
                                            .when(self.picker_index == index + 1, |row| {
                                                row.bg(colors.primary.alpha(0.1))
                                            })
                                            .p(px(10.0))
                                            .rounded(px(6.0))
                                            .cursor_pointer()
                                            .hover(|s| s.bg(colors.primary.alpha(0.08)))
                                            .child(
                                                div()
                                                    .text_size(px(12.0))
                                                    .text_ellipsis()
                                                    .child(session.title.clone()),
                                            )
                                            .child(
                                                div()
                                                    .text_size(px(10.0))
                                                    .text_color(colors.secondary)
                                                    .text_ellipsis()
                                                    .child(session.cwd.clone()),
                                            )
                                            .on_click(cx.listener(move |this, _, window, cx| {
                                                this.add_existing(id.clone(), axis, window, cx)
                                            }))
                                    },
                                )),
                        )
                        .children(self.error.as_ref().map(|error| {
                            div()
                                .text_size(px(12.0))
                                .text_color(colors.secondary)
                                .child(error.clone())
                        })),
                )
                .into_any_element(),
        )
    }
}

impl Render for SplitWorkbench {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = {
            let store = self
                .runtime
                .store
                .read()
                .expect("session store lock poisoned");
            crate::app_theme::colors(&store.preferences().terminal_theme)
        };
        let tree = self
            .selected
            .as_ref()
            .and_then(|id| self.layouts.containing(id));
        let (panes, dividers) = tree
            .map(|tree| {
                tree.geometry(Rect {
                    width: self.viewport.width,
                    height: self.viewport.height,
                    ..Rect::default()
                })
            })
            .unwrap_or_default();
        let mut surface = div()
            .id("split-workbench")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::picker_key))
            .relative()
            .size_full()
            .overflow_hidden()
            .on_drag_move(
                cx.listener(|this, event: &DragMoveEvent<DraggedDivider>, _, cx| {
                    let divider = &event.drag(cx).0;
                    let (position, start, available) = match divider.axis {
                        SplitAxis::Right => (
                            f32::from(event.event.position.x) - this.viewport.x,
                            divider.parent.x,
                            divider.parent.width - crate::split_layout::DIVIDER,
                        ),
                        SplitAxis::Below => (
                            f32::from(event.event.position.y) - this.viewport.y,
                            divider.parent.y,
                            divider.parent.height - crate::split_layout::DIVIDER,
                        ),
                    };
                    if available > 0.0
                        && let Some(tree) = this
                            .selected
                            .as_ref()
                            .and_then(|id| this.layouts.containing_mut(id))
                    {
                        tree.resize(&divider.path, (position - start) / available);
                        this.resize_active = true;
                        cx.notify();
                    }
                }),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, _| {
                    if this.resize_active {
                        this.resize_active = false;
                        this.persist();
                    }
                }),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _, _, _| {
                    if this.resize_active {
                        this.resize_active = false;
                        this.persist();
                    }
                }),
            );
        for (index, (id, rect)) in panes.into_iter().enumerate() {
            let Some(terminal) = self.panes.get(&id) else {
                continue;
            };
            terminal.update(cx, |terminal, cx| {
                terminal.set_workbench_primary(index == 0);
                terminal.set_shell_chrome(self.sidebar_visible, self.inspector_open, cx);
                terminal.set_viewport(
                    TerminalViewport {
                        x: self.viewport.x + rect.x,
                        y: self.viewport.y + rect.y,
                        width: rect.width,
                        height: rect.height,
                    },
                    cx,
                );
            });
            let close_id = id.clone();
            let debug_id = id.clone();
            let active = self.selected.as_ref() == Some(&id);
            surface =
                surface.child(
                    div()
                        .id(SharedString::from(format!("split-pane-{}", id.0)))
                        .debug_selector(move || format!("SPLIT_PANE_{}", debug_id.0))
                        .absolute()
                        .left(px(rect.x))
                        .top(px(rect.y))
                        .w(px(rect.width))
                        .h(px(rect.height))
                        .overflow_hidden()
                        .child(terminal.clone())
                        .child(div().absolute().left_0().top_0().w(px(3.0)).h(px(42.0)).bg(
                            if active {
                                colors.primary.alpha(0.7)
                            } else {
                                colors.primary.alpha(0.12)
                            },
                        ))
                        .child(
                            div()
                                .id(SharedString::from(format!("split-pane-controls-{}", id.0)))
                                .absolute()
                                .right(px(6.0))
                                .top(px(7.0))
                                .flex()
                                .items_center()
                                .gap(px(2.0))
                                .bg(colors.background)
                                .child(
                                    div()
                                        .text_size(px(10.0))
                                        .text_color(colors.secondary)
                                        .px(px(4.0))
                                        .child(format!("{}", index + 1)),
                                )
                                .child(
                                    div()
                                        .id(SharedString::from(format!("split-add-{}", id.0)))
                                        .size(px(26.0))
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .rounded(px(4.0))
                                        .cursor_pointer()
                                        .hover(|s| s.bg(colors.primary.alpha(0.1)))
                                        .child(sf_symbol("plus", 11.0, colors.secondary))
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            this.select(id.clone(), window, cx);
                                            this.open_picker(SplitAxis::Right, window, cx);
                                        })),
                                )
                                .child(
                                    div()
                                        .id(SharedString::from(format!(
                                            "split-close-{}",
                                            close_id.0
                                        )))
                                        .size(px(26.0))
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .rounded(px(4.0))
                                        .cursor_pointer()
                                        .hover(|s| s.bg(colors.primary.alpha(0.1)))
                                        .child(sf_symbol("xmark", 10.0, colors.secondary))
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            this.close(&close_id, window, cx);
                                        })),
                                ),
                        ),
                );
        }
        surface = surface.children(
            dividers
                .into_iter()
                .map(|divider| self.divider(divider, cx)),
        );
        if self.picker.is_none() && (self.pending || self.error.is_some()) {
            surface = surface.child(
                div()
                    .absolute()
                    .bottom(px(16.0))
                    .left(px(16.0))
                    .right(px(16.0))
                    .p(px(12.0))
                    .rounded(px(8.0))
                    .bg(colors.background)
                    .border_1()
                    .border_color(colors.primary.alpha(0.16))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .child(
                        div().flex_1().text_size(px(12.0)).child(
                            self.error
                                .clone()
                                .unwrap_or_else(|| "Opening terminal…".to_owned()),
                        ),
                    )
                    .when(self.error.is_some(), |row| {
                        row.child(
                            div()
                                .id("dismiss-split-error")
                                .cursor_pointer()
                                .p(px(6.0))
                                .child(sf_symbol("xmark", 11.0, colors.secondary))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.error = None;
                                    cx.notify();
                                })),
                        )
                    }),
            );
        }
        if let Some(picker) = self.picker(cx) {
            surface = surface.child(picker);
        }
        surface
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use diri_proto::{SessionListResult, SessionRecord};
    use gpui::{TestAppContext, size};

    fn session(id: &str) -> SessionRecord {
        let envelope: serde_json::Value = serde_json::from_str(include_str!(
            "../../diri-proto/tests/fixtures/session_list_response.json"
        ))
        .unwrap();
        let list: SessionListResult = serde_json::from_value(envelope["ok"].clone()).unwrap();
        let mut session = list.sessions[0].clone();
        session.id = SessionId::new(id);
        session
    }

    fn runtime() -> Arc<tokio::runtime::Runtime> {
        Arc::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap(),
        )
    }

    #[gpui::test]
    fn pane_navigation_keeps_entities_and_close_keeps_engine_sessions(cx: &mut TestAppContext) {
        let runtime = Arc::new(StoreRuntime::inert());
        let a = session("a");
        let b = session("b");
        let c = session("c");
        {
            let mut store = runtime.store.write().unwrap();
            store.hydrate(SessionListResult {
                sessions: vec![a.clone(), b.clone(), c.clone()],
                projects: vec![],
            });
            store.select(a.id.clone());
        }
        let shared = runtime.clone();
        let tokio = self::runtime();
        let (view, cx) = cx.add_window_view(move |window, cx| {
            let mut view = SplitWorkbench::new(shared, tokio, None, None, None, cx);
            view.layouts
                .split(SessionId::new("a"), SessionId::new("b"), SplitAxis::Right);
            view.layouts
                .split(SessionId::new("b"), SessionId::new("c"), SplitAxis::Below);
            view.set_viewport(
                Rect {
                    width: 805.0,
                    height: 605.0,
                    ..Rect::default()
                },
                true,
                false,
            );
            view.sync(window, cx);
            view
        });
        cx.simulate_resize(size(px(805.0), px(605.0)));
        let first_bounds = cx.debug_bounds("SPLIT_PANE_a").expect("first pane renders");
        let third_bounds = cx
            .debug_bounds("SPLIT_PANE_c")
            .expect("nested pane renders");
        assert_eq!(first_bounds.size.width, px(400.0));
        assert_eq!(third_bounds.size.height, px(300.0));
        assert_eq!(third_bounds.origin.y, px(305.0));
        view.update_in(cx, |view, window, cx| {
            assert_eq!(view.panes.len(), 3);
            let entities: HashMap<_, _> = view
                .panes
                .iter()
                .map(|(id, pane)| (id.clone(), pane.entity_id()))
                .collect();
            view.focus_direction(Direction::Right, window, cx);
            assert_eq!(view.selected, Some(b.id.clone()));
            assert!(view.panes[&b.id].read(cx).is_focused(window));
            assert_eq!(
                runtime.store.read().unwrap().selected_session_id(),
                Some(&b.id)
            );
            view.sync(window, cx);
            assert!(
                view.panes
                    .iter()
                    .all(|(id, pane)| entities[id] == pane.entity_id())
            );
            assert!(view.close_selected(window, cx));
            view.sync(window, cx);
            assert_eq!(view.panes.len(), 2);
            assert!(
                runtime.store.read().unwrap().sessions().contains_key(&b.id),
                "closing a pane must never remove its Engine session"
            );
            assert!(view.close_selected(window, cx));
            view.sync(window, cx);
            assert!(view.panes.is_empty());
            assert_eq!(runtime.store.read().unwrap().sessions().len(), 3);
        });
    }

    #[gpui::test]
    fn pending_hydration_keeps_layout_and_picker_does_not_steal_terminal_keys(
        cx: &mut TestAppContext,
    ) {
        let runtime = Arc::new(StoreRuntime::inert());
        let shared = runtime.clone();
        let tokio = self::runtime();
        let (view, cx) = cx.add_window_view(move |window, cx| {
            let mut view = SplitWorkbench::new(shared, tokio, None, None, None, cx);
            view.layouts
                .split(SessionId::new("a"), SessionId::new("b"), SplitAxis::Right);
            view.sync(window, cx);
            assert_eq!(view.layouts.layouts.len(), 1);
            view
        });
        runtime.store.write().unwrap().hydrate(SessionListResult {
            sessions: vec![session("a"), session("b")],
            projects: vec![],
        });
        runtime.store.write().unwrap().select(SessionId::new("a"));
        view.update_in(cx, |view, window, cx| {
            view.sync(window, cx);
            assert_eq!(view.panes.len(), 2);
            view.open_picker(SplitAxis::Right, window, cx);
            assert!(view.focus.is_focused(window));
            let event = KeyDownEvent {
                keystroke: gpui::Keystroke::parse("escape").unwrap(),
                is_held: false,
                prefer_character_input: false,
            };
            view.picker_key(&event, window, cx);
            assert!(view.picker.is_none());
            assert!(view.panes[&SessionId::new("a")].read(cx).is_focused(window));
        });
    }
}
