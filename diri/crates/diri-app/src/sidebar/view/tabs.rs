use super::*;
use crate::store::TabOrientation;
use crate::tab_navigation::{TAB_STRIP_HEIGHT, selected_project_tabs};

const TAB_WIDTH: f32 = 164.0;
const TAB_GAP: f32 = 4.0;

/// A session tab picked up in the horizontal strip. It carries no ghost:
/// the tab itself is lifted by the strip, locked to the strip's axis.
#[derive(Clone)]
pub(super) struct DraggedTab(pub(super) SessionId);

impl Render for DraggedTab {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

/// The static face of a session tab: leading mark and title. The mark is
/// the agent's brand while it has nothing to say, and the same activity
/// mark as the sidebar rows (working, needs input, done, sleeping) when it
/// does; both sit in one fixed slot so the title never shifts between them.
pub(super) fn session_tab_face(
    mark: AnyElement,
    title: SharedString,
    active: bool,
    colors: SemanticColors,
) -> gpui::Div {
    div()
        .px(px(10.0))
        .rounded(px(SIDEBAR_ROW_RADIUS))
        .flex()
        .items_center()
        .gap(px(7.0))
        .child(
            div()
                .size(px(18.0))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .child(mark),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .overflow_hidden()
                .text_ellipsis()
                .text_size(px(Typo::ROW.size))
                .text_color(if active {
                    colors.primary
                } else {
                    colors.secondary
                })
                .child(title),
        )
}

/// Focus bookkeeping for context menus opened from the horizontal strip.
///
/// The sidebar's own menus live inside its render tree, which never paints
/// while horizontal tabs hide the panel. The strip therefore hosts the same
/// menus in a window-level overlay, and this handle gives them keyboard
/// dismissal without leaving focus on a hidden sidebar afterwards.
pub(super) struct StripMenu {
    focus: FocusHandle,
    previous_focus: Option<FocusHandle>,
}

impl StripMenu {
    pub(super) fn new(cx: &mut App) -> Self {
        Self {
            focus: cx.focus_handle(),
            previous_focus: None,
        }
    }
}

impl Sidebar {
    /// Open a session or project context menu from the horizontal strip.
    pub(super) fn open_strip_menu(
        &mut self,
        popover: Popover,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.commit_rename();
        self.dismiss_hover_card(cx);
        if self.project_picker_is_open() {
            self.dismiss_project_picker(window, cx);
        }
        if self.strip_menu.previous_focus.is_none() {
            self.strip_menu.previous_focus = window.focused(cx);
        }
        self.ui.popover = Some(popover);
        self.strip_menu.focus.focus(window, cx);
        cx.notify();
    }

    fn close_strip_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.ui.popover = None;
        if let Some(previous) = self.strip_menu.previous_focus.take() {
            previous.focus(window, cx);
        }
        cx.notify();
    }

    /// The strip's context menus, painted above the workbench when the
    /// sidebar itself is not on screen. Called on every root render so a
    /// menu dismissed by a row action or the outside-click scrim hands focus
    /// back as it leaves the tree, the same way its Escape path does.
    pub(crate) fn render_strip_menu_overlay(
        &mut self,
        sidebar_painted: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if self.ui.popover.is_none() {
            if let Some(previous) = self.strip_menu.previous_focus.take() {
                previous.focus(window, cx);
            }
            return None;
        }
        if sidebar_painted || self.project_picker.new_agent {
            return None;
        }
        let colors = self.colors();
        let spec = self.popover(colors, cx)?;
        let popover = self.host_popover(spec, window, cx);
        Some(
            div()
                .id("strip-menu-overlay")
                .absolute()
                .inset_0()
                .track_focus(&self.strip_menu.focus)
                .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, window, cx| {
                    if event.keystroke.key == "escape" {
                        this.close_strip_menu(window, cx);
                        cx.stop_propagation();
                    }
                }))
                .child(popover)
                .into_any_element(),
        )
    }

    pub(super) fn agent_tab_icon(kind: &ProtoAgentKind, colors: SemanticColors) -> AnyElement {
        match ui_agent_kind(kind).brand_mark() {
            Some(mark) => diri_ui::BrandMark::solid(mark, 16.0, colors.secondary)
                .inset(0.08)
                .into_any_element(),
            None => sf_symbol("terminal", 16.0, colors.secondary),
        }
    }

    pub(super) fn navigation_sessions(
        &self,
        store: &mut crate::store::WindowWrite<'_>,
    ) -> Vec<Arc<SessionRecord>> {
        if store.preferences().tab_orientation == TabOrientation::Horizontal {
            selected_project_tabs(store).sessions
        } else if !self.filter_query.text().trim().is_empty() {
            // Numeric shortcuts follow the displayed filtered rows, including
            // disclosed archives and the current project/recency grouping.
            self.focus_rows_for_store(store)
                .iter()
                .filter_map(|row| store.sessions().get(&row.id).cloned())
                .collect()
        } else {
            super::super::filter::filter_projection(
                store.sidebar_projection(),
                self.filter_query.text(),
            )
            .ordered_sessions
            .clone()
        }
    }

    pub fn tab_orientation(&self) -> TabOrientation {
        self.store
            .read()
            .expect("store")
            .preferences()
            .tab_orientation
    }

    pub fn horizontal_tabs_visible(&self) -> bool {
        let store = self.store.read().expect("store");
        store.preferences().tab_orientation == TabOrientation::Horizontal
            && store.preferences().horizontal_tabs_visible
    }

    pub fn toggle_horizontal_tabs(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> std::io::Result<()> {
        self.store
            .write()
            .expect("store")
            .update_preferences(|prefs| {
                prefs.horizontal_tabs_visible = !prefs.horizontal_tabs_visible;
            })?;
        if !self.horizontal_tabs_visible() && self.project_picker_active() {
            if self.project_picker.new_agent {
                self.ui.popover = None;
                self.project_picker.new_agent = false;
            }
            self.dismiss_project_picker(window, cx);
        }
        cx.notify();
        Ok(())
    }

    /// Commit the presentation preference before changing the visible chrome.
    /// Selection and all terminal entities stay owned by their existing views.
    pub fn set_tab_orientation(
        &mut self,
        orientation: TabOrientation,
        cx: &mut Context<Self>,
    ) -> std::io::Result<()> {
        let visible = orientation == TabOrientation::Vertical;
        self.store
            .write()
            .expect("store")
            .update_preferences(|prefs| {
                prefs.tab_orientation = orientation;
                prefs.sidebar_visible = visible;
            })?;
        self.ui.visible = visible;
        self.last_tab_selection = None;
        self.peek_open = false;
        self.peek_close = None;
        self.dismiss_hover_card(cx);
        cx.emit(SidebarEvent::TabOrientationChanged);
        cx.notify();
        Ok(())
    }

    pub(super) fn render_project_tab_rows(
        &mut self,
        available_width: f32,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = self.colors();
        let (tabs, selected, custom_ordering, marks) = {
            let mut store = self.store.write().expect("store");
            let selected = store.selected_session_id().cloned();
            let custom = store.preferences().sidebar_ordering == SidebarOrdering::Custom;
            let tabs = selected_project_tabs(&mut store);
            // The same reduction the sidebar rows use, so a tab and its row
            // never disagree about what a session is doing.
            let marks: Vec<StatusState> = tabs
                .sessions
                .iter()
                .map(|session| {
                    sidebar_activity_state(
                        status_state(session, store.migrating().contains(&session.id)),
                        store.notifications().session_unread(&session.id),
                    )
                })
                .collect();
            (tabs, selected, custom, marks)
        };
        let reduce_motion = cx.reduce_motion();
        if self.tab_shift.settled.get() {
            self.tab_shift.deltas.clear();
        }
        let entity = cx.entity();
        let mut rows = div()
            .id(SharedString::from(format!(
                "horizontal-tab-list-{}",
                tabs.project.as_ref().map_or("empty", |id| id.0.as_str())
            )))
            .flex()
            .items_center()
            .gap(px(TAB_GAP))
            .flex_1()
            .min_w(px(0.0))
            .h(px(30.0))
            .overflow_x_scroll()
            .track_scroll(&self.tab_scroll);
        if self.last_tab_selection != selected || self.last_tab_available_width != available_width {
            if let Some(index) = tabs
                .sessions
                .iter()
                .position(|session| Some(&session.id) == selected.as_ref())
            {
                // Fixed-width tabs have a known content position before the
                // first layout. GPUI clamps this offset to the final viewport.
                self.tab_scroll
                    .set_offset(point(px(-(index as f32) * (TAB_WIDTH + TAB_GAP)), px(0.0)));
            }
            self.last_tab_selection = selected.clone();
            self.last_tab_available_width = available_width;
        }
        for (session, state) in tabs.sessions.into_iter().zip(marks) {
            let id = session.id.clone();
            let active = selected.as_ref() == Some(&id);
            let title = display_title(&session);
            self.working_row_rendered |= state == StatusState::Working;
            let mark = match state {
                StatusState::IdleSeen | StatusState::None => div()
                    .id(SharedString::from(format!("horizontal-tab-logo-{}", id.0)))
                    .debug_selector({
                        let id = id.0.clone();
                        move || format!("horizontal-tab-logo-{id}")
                    })
                    .child(Self::agent_tab_icon(session.effective_kind(), colors))
                    .into_any_element(),
                state => div()
                    .id(SharedString::from(format!(
                        "horizontal-tab-status-{}",
                        id.0
                    )))
                    .debug_selector({
                        let id = id.0.clone();
                        move || format!("horizontal-tab-status-{id}")
                    })
                    .role(Role::Image)
                    .aria_label(state.label())
                    .child(activity_mark(state, self.activity_frame, colors))
                    .into_any_element(),
            };
            let debug_id = id.0.clone();
            let close_id = id.clone();
            let probe_key = SharedString::from(format!("tab:{}", id.0));
            let lifting = self.lift_offset(&LiftKey::SessionTab(id.clone()));
            let dragging_self = lifting.is_some();
            // The lifted tab never slides: it is drawn from the pointer, and
            // its slot simply moves.
            let shift = if reduce_motion || dragging_self {
                None
            } else {
                self.tab_shift.deltas.get(&id).copied()
            };
            if shift.is_none() {
                self.tab_shift.applied.borrow_mut().remove(&id);
            }
            let tab = session_tab_face(mark, title.clone().into(), active, colors)
                .id(SharedString::from(format!("horizontal-tab-{}", id.0)))
                .debug_selector(move || format!("horizontal-tab-{}", debug_id))
                .role(Role::Tab)
                .aria_label(title.clone())
                .aria_selected(active)
                .relative()
                .child(self.fade_probe(probe_key.clone()))
                .flex_none()
                .w(px(TAB_WIDTH))
                .h(px(30.0))
                .cursor_pointer()
                .border_1()
                .border_color(colors.primary.alpha(0.0))
                .glass_pill(colors, active)
                .hover(move |row| {
                    if active {
                        row
                    } else {
                        row.bg(colors.primary.alpha(0.06))
                    }
                })
                .when(custom_ordering, |row| {
                    let drag_id = id.clone();
                    let drag_entity = entity.clone();
                    row.on_drag(DraggedTab(id.clone()), move |dragged, grab, window, cx| {
                        // The tab itself lifts from where the pointer grabbed it
                        // and travels only along the strip.
                        let origin = window.mouse_position() - grab;
                        drag_entity.update(cx, |this, cx| {
                            let order = this.store.write().expect("store").sidebar_session_order();
                            this.ui.session_order_at_drag_start = Some(order);
                            this.lift = Some(Lift::new(
                                LiftKey::SessionTab(drag_id.clone()),
                                origin,
                                grab,
                                LiftAxis::Horizontal,
                            ));
                            cx.notify();
                        });
                        cx.new(|_| dragged.clone())
                    })
                    // Tabs trade places once the pointer crosses a tab's
                    // midline in its direction of travel, and the displaced
                    // tabs slide into their new slots.
                    .drag_over::<DraggedTab>({
                        let id = id.clone();
                        let entity = entity.clone();
                        move |row, dragged, _, cx| {
                            entity.update(cx, |this, cx| {
                                if this.pointer_crossed_tab(&dragged.0, &id)
                                    && this.reorder_tab(&dragged.0, &id, cx.reduce_motion())
                                {
                                    cx.notify();
                                }
                            });
                            row
                        }
                    })
                })
                .child(
                    div()
                        .id(SharedString::from(format!(
                            "close-horizontal-tab-{}",
                            close_id.0
                        )))
                        .role(Role::Button)
                        .aria_label("Close session")
                        .size(px(18.0))
                        .flex_none()
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(5.0))
                        .hover(move |button| button.bg(colors.primary.alpha(0.10)))
                        .child(sf_symbol("xmark", 8.0, colors.tertiary))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.close_sessions(vec![close_id.clone()], cx);
                            cx.stop_propagation();
                            cx.notify();
                        })),
                )
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener({
                        let id = id.clone();
                        move |this, event: &gpui::MouseDownEvent, window, cx| {
                            cx.stop_propagation();
                            this.ui.focus_cursor = Some(id.clone());
                            this.open_strip_menu(
                                Popover::SessionActions {
                                    id: id.clone(),
                                    position: event.position,
                                },
                                window,
                                cx,
                            );
                        }
                    }),
                )
                .on_click(cx.listener({
                    let id = id.clone();
                    move |this, _, _, cx| {
                        this.commit_rename();
                        this.store.write().expect("store").select(id.clone());
                        cx.emit(SidebarEvent::SessionActivated);
                        cx.notify();
                    }
                }));
            let tab = match (lifting, shift) {
                (Some(offset), _) => lift_in_place(tab, LiftAxis::Horizontal, offset, colors),
                (None, None) => tab.into_any_element(),
                (None, Some(delta)) => {
                    let applied = Rc::clone(&self.tab_shift.applied);
                    let settled = Rc::clone(&self.tab_shift.settled);
                    let id = id.clone();
                    tab.with_animation(
                        SharedString::from(format!(
                            "tab-shift:{}:{}",
                            id.0, self.tab_shift.generation
                        )),
                        Animation::new(SECTION_SHIFT_TIME)
                            .with_easing(|delta| Motion::SETTLE.settle(delta)),
                        move |tab, progress| {
                            let offset = delta * (1.0 - progress);
                            applied.borrow_mut().insert(id.clone(), offset);
                            if progress >= 1.0 {
                                settled.set(true);
                            }
                            tab.left(px(offset))
                        },
                    )
                    .into_any_element()
                }
            };
            rows = rows.child(tab);
        }
        rows.into_any_element()
    }

    /// Session tabs as the strip currently shows them, in order.
    pub(super) fn visible_tab_order(&self) -> Vec<SessionId> {
        let mut store = self.store.write().expect("store");
        selected_project_tabs(&mut store)
            .sessions
            .iter()
            .map(|session| session.id.clone())
            .collect()
    }

    /// Whether the pointer has passed `target`'s midline in the direction
    /// `moved` is travelling along the strip. Nothing crosses while a
    /// previous reorder's slide is still in flight.
    fn pointer_crossed_tab(&mut self, moved: &SessionId, target: &SessionId) -> bool {
        if moved == target || self.tab_shift.in_flight() {
            return false;
        }
        let Some(pointer) = self.lift.as_ref().map(|lift| lift.pointer) else {
            return false;
        };
        let Some(tab) = self
            .fade_bounds
            .borrow()
            .get(&SharedString::from(format!("tab:{}", target.0)))
            .copied()
        else {
            return false;
        };
        let order = self.visible_tab_order();
        let position = |id: &SessionId| order.iter().position(|candidate| candidate == id);
        let (Some(from), Some(to)) = (position(moved), position(target)) else {
            return false;
        };
        let midline = tab.origin.x + tab.size.width / 2.0;
        if from < to {
            pointer.x >= midline
        } else {
            pointer.x <= midline
        }
    }

    /// Live tab reorder; returns whether the strip's order changed. Tabs only
    /// trade places within their sibling run (the strip flattens a session
    /// tree, and a child cannot be ordered past its parent's peers) and never
    /// across the pin boundary, since pinned rows always sort first.
    fn reorder_tab(&mut self, moved: &SessionId, target: &SessionId, reduce_motion: bool) -> bool {
        let before = self.visible_tab_order();
        let staged = {
            let mut store = self.store.write().expect("store");
            if store.preferences().sidebar_ordering != SidebarOrdering::Custom {
                return false;
            }
            let projection = store.sidebar_projection();
            if !sibling_run(&projection, moved).contains(target) {
                return false;
            }
            let pinned = |id: &SessionId| {
                projection
                    .projects
                    .iter()
                    .flat_map(|group| group.sessions.iter())
                    .find(|row| row.id() == id)
                    .map(|row| row.pinned)
            };
            if pinned(moved) != pinned(target) {
                return false;
            }
            let mut order = store.sidebar_session_order();
            move_past(&mut order, moved, target);
            store.stage_session_order(order)
        };
        self.ui.order_dirty |= staged;
        let after = self.visible_tab_order();
        let changed = staged && before != after;
        if changed {
            self.shift_tabs(&before, &after, reduce_motion);
        }
        changed
    }

    /// Starts the slide from the tabs' current positions to their new slots.
    /// Every tab is the same width, so a slot is an index.
    pub(super) fn shift_tabs(
        &mut self,
        before: &[SessionId],
        after: &[SessionId],
        reduce_motion: bool,
    ) {
        let applied = self.tab_shift.applied.borrow().clone();
        let mut deltas = tab_shift_deltas(before, after, &applied, TAB_WIDTH + TAB_GAP);
        // The lifted tab does not slide; its slot moves under it.
        if let Some(lift) = self.lift.as_mut()
            && let LiftKey::SessionTab(session) = &lift.key
            && let Some(delta) = deltas.remove(session)
        {
            lift.slot.x -= px(delta);
        }
        self.tab_shift.start(deltas, reduce_motion);
    }

    /// `trailing` is the workbench's title-bar action cluster (links,
    /// inspector, notifications) hosted here beside the new-tab control, so
    /// the terminal pane below can drop its own title bar.
    pub fn render_horizontal_tabs(
        &mut self,
        available_width: f32,
        trailing: Option<AnyElement>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // While horizontal tabs hide the panel this strip is the sidebar's
        // only painted surface, so the per-frame work `Sidebar::render`
        // does has to happen here: settle workspace navigation (a pending
        // project-agent open, a created or removed workspace) and keep the
        // working marks' 8 Hz tick alive only while one is on screen.
        self.reconcile_workspace_navigation(cx);
        self.working_row_rendered = false;
        if cx.reduce_motion() {
            self.activity_frame = 0;
        }
        let strip = self.horizontal_strip(available_width, trailing, cx);
        self.schedule_activity_tick(cx);
        strip
    }

    fn horizontal_strip(
        &mut self,
        available_width: f32,
        trailing: Option<AnyElement>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = self.colors();
        self.end_lift_if_released(cx);
        if self.workspace_nav.active.is_some() {
            self.workspace_nav.available_width = available_width;
            return self.workspace_strip(colors, cx);
        }
        let rows = self.render_project_tab_rows(available_width, cx);
        div()
            .id("horizontal-tabs")
            .debug_selector(|| "horizontal-tabs".into())
            // The strip is rendered outside the sidebar's own root, so it
            // tracks the pointer for its lifted tab itself.
            .on_drag_move::<DraggedTab>(cx.listener(
                |this, event: &gpui::DragMoveEvent<DraggedTab>, _, cx| {
                    this.track_lift_pointer(event.event.position, cx);
                },
            ))
            .role(Role::TabList)
            .aria_label("Project sessions")
            .flex_none()
            .h(px(TAB_STRIP_HEIGHT))
            .w_full()
            .flex()
            .items_center()
            .relative()
            .py(px(6.0))
            .gap(px(0.0))
            .pl(px(if cfg!(target_os = "macos") && !self.ui.visible {
                92.0
            } else {
                10.0
            }))
            .pr(px(10.0))
            .child(
                div()
                    .absolute()
                    .left(px(0.0))
                    .right(px(0.0))
                    .bottom(px(0.0))
                    .h(px(1.0))
                    .bg(colors.primary.alpha(0.07)),
            )
            .bg(colors.sidebar_surface())
            .text_color(colors.primary)
            .child(self.project_control(colors, cx))
            .child(rows)
            .child(
                div()
                    .id("horizontal-peek-tabs")
                    .debug_selector(|| "horizontal-peek-tabs".into())
                    .role(Role::Button)
                    .aria_label("Peek tabs")
                    .size(px(28.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(7.0))
                    .cursor_pointer()
                    .hover(move |button| button.bg(colors.primary.alpha(0.06)))
                    .child(sf_symbol("square.grid.2x2", 12.0, colors.secondary))
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(|_, window, cx| {
                        window.dispatch_action(Box::new(crate::commands::ToggleTabPeek), cx)
                    }),
            )
            .child(
                div()
                    .id("horizontal-new-tab")
                    .debug_selector(|| "horizontal-new-tab".into())
                    .role(Role::Button)
                    .aria_label("New session")
                    .size(px(28.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(7.0))
                    .cursor_pointer()
                    .hover(move |button| button.bg(colors.primary.alpha(0.06)))
                    .child(sf_symbol("plus", 12.0, colors.secondary))
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(|_, window, cx| {
                        window.dispatch_action(Box::new(crate::commands::NewDefaultSession), cx)
                    }),
            )
            .when_some(trailing, |strip, trailing| {
                strip.child(
                    div()
                        .flex_none()
                        .ml(px(6.0))
                        .flex()
                        .items_center()
                        .child(trailing),
                )
            })
            .into_any_element()
    }
}

/// Offsets that carry each tab from where it is drawn to its new slot.
/// `pitch` is one slot: tab width plus gap.
fn tab_shift_deltas(
    before: &[SessionId],
    after: &[SessionId],
    applied: &HashMap<SessionId, f32>,
    pitch: f32,
) -> HashMap<SessionId, f32> {
    let mut deltas = HashMap::new();
    for (new_index, id) in after.iter().enumerate() {
        let Some(old_index) = before.iter().position(|candidate| candidate == id) else {
            continue;
        };
        let delta =
            (old_index as f32 - new_index as f32) * pitch + applied.get(id).copied().unwrap_or(0.0);
        if delta.abs() >= 0.5 {
            deltas.insert(id.clone(), delta);
        }
    }
    deltas
}

#[cfg(test)]
mod tests {
    use super::*;
    use diri_proto::workspace::{
        LayoutNode, PaneId, TabId, WorkspaceId, WorkspaceRecord, WorkspaceSnapshot, WorkspaceTab,
    };
    use gpui::{TestAppContext, VisualTestContext};

    /// Only the strip paints, exactly as the app does while horizontal tabs
    /// hide the sidebar panel.
    struct StripOnly {
        sidebar: Entity<Sidebar>,
    }
    impl Render for StripOnly {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .size_full()
                .child(self.sidebar.update(cx, |sidebar, cx| {
                    sidebar.render_horizontal_tabs(900.0, None, cx)
                }))
        }
    }
    fn strip_harness(
        cx: &mut TestAppContext,
        reduce_motion: bool,
    ) -> (Entity<Sidebar>, &mut VisualTestContext) {
        cx.update(|cx| cx.set_reduce_motion(reduce_motion));
        let (view, cx) = cx.add_window_view(move |_, cx| {
            let sidebar = cx.new(|cx| {
                let mut sidebar = Sidebar::new(None, true, PreviewScenario::Typical, cx);
                sidebar
                    .set_tab_orientation(TabOrientation::Horizontal, cx)
                    .unwrap();
                sidebar
            });
            cx.observe(&sidebar, |_, _, cx| cx.notify()).detach();
            StripOnly { sidebar }
        });
        (view.read_with(cx, |view, _| view.sidebar.clone()), cx)
    }

    #[gpui::test]
    fn horizontal_tabs_show_the_activity_mark_instead_of_the_logo(cx: &mut TestAppContext) {
        let (_sidebar, cx) = strip_harness(cx, true);
        // Working and needs-input sessions carry their state in the leading slot.
        assert!(
            cx.debug_bounds("horizontal-tab-status-preview-codex")
                .is_some()
        );
        assert!(
            cx.debug_bounds("horizontal-tab-logo-preview-codex")
                .is_none()
        );
        assert!(
            cx.debug_bounds("horizontal-tab-status-preview-claude")
                .is_some()
        );
        // A turn that finished after the session was last seen is unread.
        assert!(
            cx.debug_bounds("horizontal-tab-status-preview-cursor")
                .is_some()
        );
        // A session with nothing to report keeps the agent's brand mark.
        assert!(
            cx.debug_bounds("horizontal-tab-logo-preview-shell")
                .is_some()
        );
        assert!(
            cx.debug_bounds("horizontal-tab-status-preview-shell")
                .is_none()
        );
        // Both marks occupy the same slot, so the title never shifts.
        let status = cx
            .debug_bounds("horizontal-tab-status-preview-codex")
            .unwrap();
        let logo = cx
            .debug_bounds("horizontal-tab-logo-preview-shell")
            .unwrap();
        let status_tab = cx.debug_bounds("horizontal-tab-preview-codex").unwrap();
        let logo_tab = cx.debug_bounds("horizontal-tab-preview-shell").unwrap();
        assert_eq!(
            status.center().x - status_tab.left(),
            logo.center().x - logo_tab.left()
        );
    }

    #[gpui::test]
    fn horizontal_strip_keeps_the_working_mark_ticking_while_the_panel_is_hidden(
        cx: &mut TestAppContext,
    ) {
        let (sidebar, cx) = strip_harness(cx, false);
        cx.update(|window, _| window.activate_window());
        cx.run_until_parked();
        sidebar.read_with(cx, |sidebar, _| {
            assert!(!sidebar.is_visible(), "the panel is hidden");
            assert!(sidebar.working_row_rendered);
            assert!(sidebar.activity_tick.is_some());
        });
        for _ in 0..3 {
            let frame = sidebar.read_with(cx, |sidebar, _| sidebar.activity_frame);
            cx.executor().advance_clock(Duration::from_millis(125));
            cx.run_until_parked();
            assert_eq!(
                sidebar.read_with(cx, |sidebar, _| sidebar.activity_frame),
                (frame + 1) % 8,
            );
        }
        // Once nothing works, the strip lets the wake lapse.
        sidebar.update(cx, |sidebar, cx| {
            let mut store = sidebar.store.write().unwrap();
            let sessions: Vec<_> = store.sessions().values().cloned().collect();
            for session in sessions {
                let mut session = (*session).clone();
                session.status = diri_proto::SessionStatus::Idle;
                store.upsert_session(session);
            }
            drop(store);
            cx.notify();
        });
        cx.run_until_parked();
        cx.executor().advance_clock(Duration::from_millis(125));
        cx.run_until_parked();
        sidebar.read_with(cx, |sidebar, _| {
            assert!(!sidebar.working_row_rendered);
            assert!(sidebar.activity_tick.is_none());
        });
    }

    /// A floating menu is its own window and reads the sidebar while it
    /// draws, so GPUI comes to regard that window as the sidebar's. Closing
    /// it leaves the sidebar with no window until the main one draws again,
    /// and a tick landing in that gap must not strand the working marks.
    #[gpui::test]
    fn working_mark_keeps_ticking_after_a_floating_window_closes(cx: &mut TestAppContext) {
        struct Panel {
            sidebar: Entity<Sidebar>,
        }
        impl Render for Panel {
            fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
                let _ = self.sidebar.read(cx).is_visible();
                div()
            }
        }
        let (sidebar, cx) = strip_harness(cx, false);
        cx.update(|window, _| window.activate_window());
        cx.run_until_parked();
        let panel = cx.update(|_, cx| {
            let sidebar = sidebar.clone();
            cx.open_window(Default::default(), |_, cx| cx.new(|_| Panel { sidebar }))
                .unwrap()
        });
        cx.run_until_parked();
        panel
            .update(cx, |_, window, _| window.remove_window())
            .unwrap();
        cx.run_until_parked();
        for _ in 0..3 {
            let frame = sidebar.read_with(cx, |sidebar, _| sidebar.activity_frame);
            cx.executor().advance_clock(Duration::from_millis(125));
            cx.run_until_parked();
            assert_eq!(
                sidebar.read_with(cx, |sidebar, _| sidebar.activity_frame),
                (frame + 1) % 8,
            );
        }
    }

    #[gpui::test]
    fn horizontal_strip_settles_a_pending_project_agent_open(cx: &mut TestAppContext) {
        let (sidebar, cx) = strip_harness(cx, true);
        let claude = SessionId::new("preview-claude");
        let workspace = WorkspaceId::new("project-view");
        let snapshot = |revision: u64, project: &diri_proto::ProjectId| WorkspaceSnapshot {
            revision,
            workspaces: vec![WorkspaceRecord {
                id: workspace.clone(),
                project_id: Some(project.clone()),
                name: "Project".into(),
                selected_tab: Some(TabId::new("agent-tab")),
                tabs: vec![WorkspaceTab {
                    id: TabId::new("agent-tab"),
                    title: None,
                    layout: LayoutNode::Pane {
                        id: PaneId::new("pane"),
                        session_id: claude.clone(),
                    },
                    focused_pane: PaneId::new("pane"),
                    zoomed_pane: None,
                }],
            }],
            ..Default::default()
        };
        // A tab click: the session is selected and its project agent opens.
        let project = sidebar.update(cx, |sidebar, cx| {
            sidebar.preview = false;
            let mut store = sidebar.store.write().unwrap();
            let project = store.sessions()[&claude].project_id.clone();
            store.seed_workspace_snapshot_for_test(snapshot(4, &project));
            store.select(claude.clone());
            drop(store);
            assert!(sidebar.open_selected_project_agent(cx));
            assert!(sidebar.project_agent_open_pending());
            project
        });
        // The Engine answers while only the strip is painting.
        sidebar.update(cx, |sidebar, cx| {
            sidebar
                .store
                .write()
                .unwrap()
                .finish_workspace_edit_for_test(snapshot(5, &project));
            cx.notify();
        });
        cx.run_until_parked();
        sidebar.read_with(cx, |sidebar, _| {
            assert!(
                !sidebar.project_agent_open_pending(),
                "the strip must settle the open the way the panel's render does"
            );
            assert_eq!(sidebar.workspace_nav.active.as_ref(), Some(&workspace));
        });
    }
}
