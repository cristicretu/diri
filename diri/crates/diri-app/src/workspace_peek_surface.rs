use super::*;
use crate::{
    tab_peek::{card_rect, visible_card_indices},
    workspace_geometry::{Rect, WorkspaceGeometry, preview_demand},
};
use diri_proto::workspace::WorkspaceId;

impl SessionSurfaces {
    pub(crate) fn set_workspace_peek(
        &mut self,
        workspace: Option<WorkspaceId>,
        bounds: Rect,
        cx: &mut Context<Self>,
    ) {
        if self.peek_workspace != workspace {
            self.cancel_tab_peek_immediately(cx);
            self.peek_workspace = workspace;
        }
        if self.peek_settled_bounds != bounds {
            self.peek_settled_bounds = bounds;
            if self.peek.paint_visible() {
                cx.notify();
            }
        }
    }
    pub(super) fn render_saved_tab_peek(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = self.colors();
        let reduced = cx.reduce_motion();
        let width = self.peek_width.max(1.0);
        let height = (f32::from(window.viewport_size().height) - self.peek_top).max(1.0);
        let scroll = f32::from(self.peek_scroll.offset().y);
        let (tabs, theme, eligible) = {
            let store = self.store.read().expect("store");
            let workspace = store.workspace_catalog().snapshot().and_then(|snapshot| {
                snapshot
                    .workspaces
                    .iter()
                    .find(|workspace| Some(&workspace.id) == self.peek_workspace.as_ref())
            });
            let focused = self.peek.selected();
            self.peek.sessions.retain(|item| match item {
                PeekItem::Tab(id) => workspace
                    .is_some_and(|workspace| workspace.tabs.iter().any(|tab| &tab.id == id)),
                _ => false,
            });
            self.peek.focused = focused
                .as_ref()
                .and_then(|focused| self.peek.sessions.iter().position(|item| item == focused))
                .unwrap_or_else(|| {
                    self.peek
                        .focused
                        .min(self.peek.sessions.len().saturating_sub(1))
                });
            let visible = visible_card_indices(&self.peek, width, height, scroll, reduced);
            let tabs = visible
                .into_iter()
                .filter_map(|index| {
                    let PeekItem::Tab(id) = &self.peek.sessions[index] else {
                        return None;
                    };
                    workspace
                        .and_then(|workspace| workspace.tabs.iter().find(|tab| &tab.id == id))
                        .map(|tab| (index, tab.clone()))
                })
                .collect::<Vec<_>>();
            let eligible = store
                .sessions()
                .iter()
                .filter(|(_, session)| {
                    !matches!(session.status, diri_proto::SessionStatus::Exited(_))
                })
                .map(|(id, _)| id.clone())
                .collect::<HashSet<_>>();
            (
                tabs,
                crate::app_theme::terminal_theme(store.theme_id()),
                eligible,
            )
        };
        let mut settled = HashMap::new();
        let mut projected = Vec::new();
        for (index, tab) in &tabs {
            let Some(geometry) = WorkspaceGeometry::settled(tab, self.peek_settled_bounds) else {
                continue;
            };
            let card = card_rect(
                *index,
                self.peek.sessions.len(),
                width,
                height,
                &self.peek,
                reduced,
            );
            if let Some(preview) = geometry.fit(Rect {
                x: card.x + 6.0,
                y: card.y + 6.0 + scroll,
                width: (card.width - 12.0).max(1.0),
                height: (card.height - 40.0).max(1.0),
            }) {
                projected.push(preview);
            }
            settled.insert(tab.id.clone(), geometry);
        }
        let resident: HashSet<_> = self.resident_previews.keys().cloned().collect();
        for layout in &mut projected {
            layout.panes.retain(|pane| {
                eligible.contains(&pane.identity.session)
                    || resident.contains(&pane.identity.session)
            });
        }
        let demand = preview_demand(
            &projected,
            Rect {
                width,
                height,
                ..Default::default()
            },
            &resident,
            crate::workspace_preview_source::MAX_SOURCES,
        );
        let wanted = demand
            .subscriptions
            .iter()
            .filter(|id| eligible.contains(*id))
            .cloned()
            .collect();
        if self.peek.visible() {
            if let Some(runtime) = &self.tokio {
                self.workspace_previews.sync(
                    wanted,
                    runtime,
                    self.client.socket_path().to_owned(),
                    cx,
                );
            }
        } else {
            self.workspace_previews.clear();
        }
        let mut buffers = self.workspace_previews.elements();
        buffers.extend(
            self.closing_previews
                .iter()
                .map(|(id, element)| (id.clone(), element.clone())),
        );
        buffers.extend(
            self.resident_previews
                .iter()
                .map(|(id, element)| (id.clone(), element.clone())),
        );
        let visible_panes = settled
            .values()
            .flat_map(|geometry| geometry.panes.iter().map(|pane| pane.identity.pane.clone()))
            .collect::<HashSet<_>>();
        self.workspace_preview_views
            .retain(|pane, _| visible_panes.contains(pane));
        let mut body = div()
            .id("saved-tab-peek-cards")
            .relative()
            .w_full()
            .min_h_full();
        let mut content_height = height;
        for index in 0..self.peek.sessions.len() {
            let bounds = card_rect(
                index,
                self.peek.sessions.len(),
                width,
                height,
                &self.peek,
                reduced,
            );
            content_height = content_height.max(bounds.y + bounds.height + 24.0);
        }
        for (index, tab) in tabs {
            let Some(geometry) = settled.get(&tab.id) else {
                continue;
            };
            let bounds = card_rect(
                index,
                self.peek.sessions.len(),
                width,
                height,
                &self.peek,
                reduced,
            );
            let selected = self.peek.focused == index;
            let unique = geometry
                .panes
                .iter()
                .map(|pane| pane.identity.session.clone())
                .collect::<HashSet<_>>();
            let available = unique
                .iter()
                .filter(|id| {
                    buffers
                        .get(*id)
                        .is_some_and(|buffer| buffer.grid_cols() > 0)
                })
                .count();
            let deferred = unique
                .iter()
                .filter(|id| demand.deferred_sessions.contains(id))
                .count();
            let loading = unique.iter().any(|id| {
                self.workspace_previews.get(id).is_some_and(|preview| {
                    *preview.state.borrow() == crate::tab_preview::PreviewState::Loading
                })
            });
            let disconnected = unique.iter().any(|id| {
                self.workspace_previews.get(id).is_some_and(|preview| {
                    *preview.state.borrow() == crate::tab_preview::PreviewState::Disconnected
                })
            });
            let status = if disconnected {
                "Last received".into()
            } else if deferred > 0 {
                format!("{available}/{} · {deferred} deferred", unique.len())
            } else if loading {
                "Loading…".into()
            } else if available < unique.len() {
                format!("{available}/{} available", unique.len())
            } else {
                format!(
                    "{} {}",
                    geometry.panes.len(),
                    if geometry.panes.len() == 1 {
                        "pane"
                    } else {
                        "panes"
                    }
                )
            };
            let id = tab.id.clone();
            let title = tab.title.unwrap_or_else(|| "Untitled tab".into());
            let preview = crate::workspace_preview::render_workspace_preview(
                geometry,
                (bounds.width - 12.0).max(1.0),
                (bounds.height - 40.0).max(1.0),
                &buffers,
                &mut self.workspace_preview_views,
                theme,
                colors,
            );
            body = body.child(
                div()
                    .id(SharedString::from(format!("saved-tab-peek-{}", id.0)))
                    .debug_selector(move || format!("TAB_PEEK_CARD_{index}"))
                    .absolute()
                    .left(px(bounds.x))
                    .top(px(bounds.y))
                    .w(px(bounds.width))
                    .h(px(bounds.height))
                    .flex()
                    .flex_col()
                    .overflow_hidden()
                    .rounded(px(Radius::CARD))
                    .border_1()
                    .border_color(colors.primary.alpha(if selected { 0.65 } else { 0.15 }))
                    .bg(colors.background)
                    .cursor_pointer()
                    .hover(|card| card.border_color(colors.primary.alpha(0.65)))
                    .child(
                        div()
                            .flex_1()
                            .min_h(px(0.0))
                            .p(px(6.0))
                            .overflow_hidden()
                            .child(preview),
                    )
                    .child(
                        div()
                            .h(px(28.0))
                            .px(px(8.0))
                            .flex()
                            .items_center()
                            .gap(px(6.0))
                            .bg(colors.floating_surface())
                            .text_size(px(11.0))
                            .text_color(colors.primary)
                            .child(div().flex_1().min_w(px(0.0)).text_ellipsis().child(title))
                            .child(
                                div()
                                    .flex_none()
                                    .text_size(px(9.0))
                                    .text_color(colors.secondary)
                                    .child(status),
                            ),
                    )
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if this.peek.visible() {
                            this.commit_tab_peek(PeekItem::Tab(id.clone()), cx);
                            cx.stop_propagation();
                        }
                    })),
            );
        }
        self.render_peek_frame(body.into_any_element(), content_height, width, height, cx)
    }
}
