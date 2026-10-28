//! Sidebar rows and strip tabs as targets for a pane dragged by its title
//! (`crate::pane_drag`). Resting on a row or tab opens it after the
//! spring-load delay, so the drop can pick an edge inside it; releasing on
//! it docks the pane beside that tab's focused pane; releasing on the free
//! space after the rows or tabs gives the pane a tab of its own.
use super::*;
use crate::pane_drag::{self, DropTarget, PaneDragState};

impl Sidebar {
    /// The pointer moved while a pane is dragged. `area` is the bounds of
    /// the sidebar panel or of the tab strip, `horizontal` says which.
    pub(super) fn pane_drag_move(
        &mut self,
        position: Point<Pixels>,
        area: Bounds<Pixels>,
        horizontal: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(source) = cx
            .try_global::<PaneDragState>()
            .filter(|state| state.active())
            .and_then(|state| state.source.clone())
        else {
            return;
        };
        if !area.contains(&position) {
            // The other surface (panel or strip) may own the target.
            if self.pane_drop_area != Some(horizontal) {
                return;
            }
            self.pane_drop_area = None;
        } else {
            self.pane_drop_area = Some(horizontal);
            self.pane_drop_bounds = Some(area);
        }
        let inside = self.pane_drop_area == Some(horizontal);
        let (row, free) = if !inside {
            (None, false)
        } else if horizontal {
            let bounds = self.fade_bounds.borrow();
            let tabs = bounds
                .iter()
                .filter_map(|(key, bounds)| Some((key.strip_prefix("tab:")?, *bounds)));
            let mut row = None;
            let mut end = None::<Pixels>;
            for (id, bounds) in tabs {
                if bounds.contains(&position) {
                    row = Some(SessionId::new(id));
                }
                end = Some(end.map_or(bounds.right(), |end| end.max(bounds.right())));
            }
            let free = row.is_none() && end.is_some_and(|end| position.x > end);
            (row, free)
        } else {
            let bounds = self.row_bounds.borrow();
            let row = bounds
                .iter()
                .find(|(_, bounds)| bounds.contains(&position))
                .map(|(id, _)| id.clone());
            let end = bounds.values().map(|bounds| bounds.bottom()).max();
            (row.clone(), row.is_none() && end.is_some_and(|end| position.y > end))
        };
        let (target, spring_row) = {
            let store = self.store.read().expect("store");
            let row = row.filter(|id| {
                store
                    .sessions()
                    .get(id)
                    .is_some_and(|record| !record.is_note() && !record.is_archived())
            });
            let target = row
                .clone()
                .map(DropTarget::Tab)
                .or(free.then_some(DropTarget::NewTab))
                .filter(|target| {
                    store.workspace_catalog().can_edit()
                        && store
                        .workspace_catalog()
                        .snapshot()
                        .and_then(|snapshot| pane_drag::drop_mutation(&source, target, snapshot))
                        .is_some()
                });
            // A row opens under the pane unless it already is on screen,
            // or the pane could not leave for it anyway.
            let spring = row.filter(|id| {
                source.can_leave && store.selected_session_id() != Some(id)
            });
            (target, spring)
        };
        let now = pane_drag::now();
        let state = cx.global_mut::<PaneDragState>();
        let changed = state.row_target != target;
        if changed && let Some(DropTarget::Tab(id)) = &target {
            haptics::perform(Haptic::Snap, haptics::key("pane-drop-row", id));
        }
        state.row_target = target;
        let started = state.spring.hover(spring_row.as_ref(), now);
        if let Some(row) = spring_row.filter(|_| started) {
            self.schedule_spring_load(row, cx);
        }
        if changed || started {
            cx.notify();
        }
    }

    fn schedule_spring_load(&mut self, row: SessionId, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(pane_drag::SPRING_LOAD).await;
            this.update(cx, |this, cx| this.spring_load(&row, cx)).ok();
        })
        .detach();
    }

    /// The pane rested on `row` for the whole delay: open its tab, the way
    /// a click on the row would, while the drag goes on.
    fn spring_load(&mut self, row: &SessionId, cx: &mut Context<Self>) {
        if !cx.has_active_drag() {
            return;
        }
        if !cx.has_global::<PaneDragState>() {
            return;
        }
        let state = cx.global_mut::<PaneDragState>();
        if !state.active() || !state.spring.fire(row) {
            return;
        }
        haptics::perform(Haptic::Snap, haptics::key("pane-spring", row));
        self.store.write().expect("store").sidebar_click(
            row.clone(),
            ClickModifiers {
                command: false,
                shift: false,
            },
        );
        cx.emit(SidebarEvent::SessionActivated);
        cx.notify();
    }

    /// Released over the panel or the strip.
    pub(super) fn pane_drop(&mut self, cx: &mut Context<Self>) {
        let session = cx
            .try_global::<PaneDragState>()
            .and_then(|state| state.source.as_ref())
            .map(|source| source.session.clone());
        let target = cx
            .try_global::<PaneDragState>()
            .and_then(|state| state.target().cloned());
        let mutation = {
            let store = self.store.read().expect("store");
            pane_drag::take_drop(store.workspace_catalog().snapshot(), None, cx)
        };
        self.pane_drop_area = None;
        let Some(mutation) = mutation else {
            cx.notify();
            return;
        };
        // The window follows the pane: into the tab it joined, or onto its
        // own new tab.
        let follow = match target {
            Some(DropTarget::Tab(row)) => Some(row),
            Some(DropTarget::NewTab) => session,
            _ => None,
        };
        let mut store = self.store.write().expect("store");
        store.edit_workspace(mutation);
        if let Some(row) = follow {
            store.sidebar_click(
                row,
                ClickModifiers {
                    command: false,
                    shift: false,
                },
            );
            drop(store);
            cx.emit(SidebarEvent::SessionActivated);
        }
        cx.notify();
    }

    /// The fill of `row` while the dragged pane rests on it: it deepens
    /// over the spring-load delay, then holds.
    pub(super) fn pane_drop_fill(&self, row: &SessionId, cx: &App) -> Option<gpui::Rgba> {
        let state = cx.try_global::<PaneDragState>().filter(|s| s.active())?;
        if let Some((resting, progress)) = state.spring.progress(pane_drag::now())
            && resting == row
        {
            return Some(pane_drag::spring_fill(progress));
        }
        matches!(&state.row_target, Some(DropTarget::Tab(id)) if id == row)
            .then(|| pane_drag::spring_fill(1.0))
    }

    /// Keeps frames coming while a resting row's fill deepens.
    pub(super) fn animate_pane_drop(&self, window: &mut Window, cx: &App) {
        if cx
            .try_global::<PaneDragState>()
            .filter(|s| s.active())
            .and_then(|state| state.spring.progress(pane_drag::now()))
            .is_some_and(|(_, progress)| progress < 1.0)
        {
            window.request_animation_frame();
        }
    }

    /// Where a pane dropped on the free space would become a tab: a slot
    /// the size of a row (or a tab) right after the last one.
    pub(super) fn pane_new_tab_slot(&self, horizontal: bool, cx: &App) -> Option<AnyElement> {
        let state = cx.try_global::<PaneDragState>().filter(|s| s.active())?;
        if state.row_target != Some(DropTarget::NewTab) || self.pane_drop_area != Some(horizontal)
        {
            return None;
        }
        let area = self.pane_drop_bounds?;
        let slot = if horizontal {
            let bounds = self.fade_bounds.borrow();
            let last = bounds
                .iter()
                .filter(|(key, _)| key.starts_with("tab:"))
                .map(|(_, bounds)| *bounds)
                .max_by(|a, b| f32::from(a.right()).total_cmp(&f32::from(b.right())))?;
            Bounds::new(
                point(last.right() + px(4.0), last.top()),
                last.size,
            )
        } else {
            let bounds = self.row_bounds.borrow();
            let last = bounds
                .values()
                .copied()
                .max_by(|a, b| f32::from(a.bottom()).total_cmp(&f32::from(b.bottom())))?;
            Bounds::new(point(last.left(), last.bottom() + px(2.0)), last.size)
        };
        let origin = slot.origin - area.origin;
        Some(
            div()
                .debug_selector(|| "pane-new-tab-slot".into())
                .absolute()
                .left(origin.x)
                .top(origin.y)
                .w(slot.size.width)
                .h(slot.size.height)
                .rounded(px(SIDEBAR_ROW_RADIUS))
                .bg(pane_drag::spring_fill(0.5))
                .border_1()
                .border_color(Palette::CLAY.alpha(0.55))
                .into_any_element(),
        )
    }

    /// ⌥⌘-free keyboard twin of dropping on a row: move `source` into the
    /// next (or previous) tab in sidebar order that can take it, and go
    /// there with it. False when no other tab can.
    pub(crate) fn move_pane_to_adjacent_tab(
        &mut self,
        source: pane_drag::DraggedPane,
        forward: bool,
        cx: &mut Context<Self>,
    ) -> bool {
        let (row, mutation) = {
            let mut store = self.store.write().expect("store");
            let order: Vec<SessionId> = self
                .navigation_sessions(&mut store)
                .iter()
                .map(|session| session.id.clone())
                .collect();
            if order.is_empty() {
                return false;
            }
            let start = store
                .selected_session_id()
                .and_then(|id| order.iter().position(|row| row == id))
                .unwrap_or(0);
            let Some(snapshot) = store.workspace_catalog().snapshot() else {
                return false;
            };
            let len = order.len();
            let found = (1..len).find_map(|step| {
                let index = if forward {
                    (start + step) % len
                } else {
                    (start + len - step) % len
                };
                let row = &order[index];
                pane_drag::drop_mutation(&source, &DropTarget::Tab(row.clone()), snapshot)
                    .map(|mutation| (row.clone(), mutation))
            });
            let Some(found) = found else {
                return false;
            };
            found
        };
        pane_drag::arm(source.pane.clone(), cx);
        let mut store = self.store.write().expect("store");
        store.edit_workspace(mutation);
        store.sidebar_click(
            row,
            ClickModifiers {
                command: false,
                shift: false,
            },
        );
        drop(store);
        cx.emit(SidebarEvent::SessionActivated);
        cx.notify();
        true
    }
}

#[cfg(all(test, target_os = "macos"))]
impl Sidebar {
    /// The spring-load timer for `row`, run on a frame render's clock.
    pub(crate) fn spring_load_for_test(&mut self, row: &SessionId, cx: &mut Context<Self>) {
        self.spring_load(row, cx);
    }

    pub(crate) fn last_row_bottom_for_test(&self) -> Pixels {
        self.row_bounds
            .borrow()
            .values()
            .map(|bounds| bounds.bottom())
            .max()
            .unwrap()
    }
}
