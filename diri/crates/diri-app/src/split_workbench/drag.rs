//! Drag presentation never owns a terminal or changes geometry until drop.
use std::time::{Duration, Instant};

use diri_ui::SemanticColors;
use gpui::{Animation, AnimationExt, Pixels, Point, Window, div, prelude::*, px};

use super::*;
use crate::split_layout::{DockEdge, SplitNode};

#[derive(Clone, Debug)]
pub struct DraggedWorkspace {
    pub session: SessionId,
    pub whole_workspace: bool,
}

#[derive(Clone)]
pub struct WorkspacePreview {
    pub tree: SplitNode,
    pub labels: HashMap<SessionId, String>,
    pub colors: SemanticColors,
}

impl WorkspacePreview {
    pub fn element(&self, reduce_motion: bool) -> AnyElement {
        let (panes, _) = self.tree.geometry(Rect {
            width: 184.0,
            height: 112.0,
            ..Rect::default()
        });
        let colors = self.colors;
        let preview =
            div()
                .w(px(192.0))
                .h(px(120.0))
                .p(px(4.0))
                .rounded(px(10.0))
                .bg(colors.background)
                .border_1()
                .border_color(colors.primary.alpha(0.24))
                .shadow_lg()
                .child(div().relative().size_full().children(panes.into_iter().map(
                    |(id, rect)| {
                        div()
                            .absolute()
                            .left(px(rect.x))
                            .top(px(rect.y))
                            .w(px(rect.width))
                            .h(px(rect.height))
                            .rounded(px(5.0))
                            .bg(colors.primary.alpha(0.06))
                            .overflow_hidden()
                            .p(px(7.0))
                            .text_size(px(10.0))
                            .text_color(colors.primary)
                            .child(
                                self.labels
                                    .get(&id)
                                    .cloned()
                                    .unwrap_or_else(|| "Terminal".into()),
                            )
                            .child(
                                div()
                                    .mt(px(10.0))
                                    .w_full()
                                    .h(px(2.0))
                                    .bg(colors.primary.alpha(0.12)),
                            )
                            .child(
                                div()
                                    .mt(px(5.0))
                                    .w(px((rect.width * 0.55).max(0.0)))
                                    .h(px(2.0))
                                    .bg(colors.primary.alpha(0.08)),
                            )
                    },
                )));
        if reduce_motion {
            return preview.into_any_element();
        }
        preview
            .with_animation(
                "workspace-lift",
                Animation::new(Duration::from_millis(160)).with_easing(diri_ui::motion::settle),
                |view, progress| view.opacity(0.65 + progress * 0.35),
            )
            .into_any_element()
    }
}

impl Render for WorkspacePreview {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.element(cx.reduce_motion())
    }
}

#[derive(Clone, Copy, Debug)]
struct TargetMotion {
    from: Rect,
    to: Rect,
    started: Instant,
}
impl TargetMotion {
    fn sample(&self, now: Instant) -> Rect {
        let progress = diri_ui::motion::settle(
            (now.duration_since(self.started).as_secs_f32() / 0.18).min(1.0),
        );
        Rect {
            x: self.from.x + (self.to.x - self.from.x) * progress,
            y: self.from.y + (self.to.y - self.from.y) * progress,
            width: self.from.width + (self.to.width - self.from.width) * progress,
            height: self.from.height + (self.to.height - self.from.height) * progress,
        }
    }
}

#[derive(Default)]
pub(super) struct DockPresentation {
    source: Option<DraggedWorkspace>,
    pane: Option<SessionId>,
    motion: Vec<TargetMotion>,
    hover: Option<SessionId>,
    hover_task: Option<gpui::Task<()>>,
}

/// Hit testing is independent of the animated artwork: a moving target never
/// oscillates between hover and non-hover beneath a stationary pointer.
fn edge_at(rect: Rect, x: f32, y: f32) -> Option<DockEdge> {
    if rect.width <= 0.0
        || rect.height <= 0.0
        || x < rect.x
        || y < rect.y
        || x >= rect.x + rect.width
        || y >= rect.y + rect.height
    {
        return None;
    }
    let horizontal = (x - rect.x) / rect.width;
    let vertical = (y - rect.y) / rect.height;
    if horizontal < 0.25 {
        Some(DockEdge::Left)
    } else if horizontal > 0.75 {
        Some(DockEdge::Right)
    } else if vertical < 0.22 {
        Some(DockEdge::Top)
    } else if vertical > 0.78 {
        Some(DockEdge::Bottom)
    } else {
        None
    }
}

fn target_rect(rect: Rect, edge: DockEdge, active: bool, pointer: (f32, f32)) -> Rect {
    let inset = 12.0_f32.min(rect.width.min(rect.height) * 0.06);
    let expansion = if active { 1.08 } else { 1.0 };
    let shift_x = ((pointer.0 - rect.x) / rect.width - 0.5).clamp(-0.5, 0.5) * 12.0;
    let shift_y = ((pointer.1 - rect.y) / rect.height - 0.5).clamp(-0.5, 0.5) * 12.0;
    match edge {
        DockEdge::Left | DockEdge::Right => {
            let width = (rect.width * 0.22 * expansion).min(190.0);
            let height = rect.height * 0.62 * expansion;
            Rect {
                x: if edge == DockEdge::Left {
                    rect.x + inset
                } else {
                    rect.x + rect.width - inset - width
                },
                y: rect.y + (rect.height - height) * 0.5 + shift_y,
                width,
                height,
            }
        }
        DockEdge::Top | DockEdge::Bottom => {
            let width = rect.width * 0.35 * expansion;
            let height = (rect.height * 0.16 * expansion).min(90.0);
            Rect {
                x: rect.x + (rect.width - width) * 0.5 + shift_x,
                y: if edge == DockEdge::Top {
                    rect.y + inset
                } else {
                    rect.y + rect.height - inset - height
                },
                width,
                height,
            }
        }
    }
}

impl SplitWorkbench {
    pub fn hover_tab(
        &mut self,
        id: SessionId,
        bounds: gpui::Bounds<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !bounds.contains(&window.mouse_position()) || self.dock.hover.as_ref() == Some(&id) {
            return;
        }
        self.dock.hover = Some(id.clone());
        self.dock.hover_task = Some(cx.spawn_in(window, async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(300))
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                if cx.has_active_drag()
                    && bounds.contains(&window.mouse_position())
                    && this.dock.hover.as_ref() == Some(&id)
                {
                    let focused = this
                        .layouts
                        .containing(&id)
                        .and_then(|tree| {
                            this.layouts
                                .focus_history
                                .iter()
                                .find(|member| tree.contains(member))
                                .cloned()
                        })
                        .unwrap_or(id);
                    this.select(focused, window, cx);
                }
            });
        }));
    }

    pub fn group_with(
        &mut self,
        target: SessionId,
        source: &DraggedWorkspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.valid_drag(source, &target)
            && self.layouts.dock(
                target,
                source.session.clone(),
                source.whole_workspace,
                DockEdge::Right,
            )
        {
            self.persist();
            self.select(source.session.clone(), window, cx);
        }
        self.cancel_drag(cx);
        cx.stop_propagation();
    }

    pub fn track_drag(&mut self, source: DraggedWorkspace, cx: &mut Context<Self>) {
        self.dock.source = Some(source);
        cx.notify();
    }

    pub fn cancel_drag(&mut self, cx: &mut Context<Self>) {
        self.dock = DockPresentation::default();
        cx.notify();
    }

    fn drop_pane(&self, point: Point<Pixels>) -> Option<(SessionId, Rect)> {
        let selected = self.selected.as_ref()?;
        let rect = Rect {
            width: self.viewport.width,
            height: self.viewport.height,
            ..Rect::default()
        };
        let panes = self
            .layouts
            .containing(selected)
            .map(|tree| tree.geometry(rect).0)
            .unwrap_or_else(|| vec![(selected.clone(), rect)]);
        let x = f32::from(point.x) - self.viewport.x;
        let y = f32::from(point.y) - self.viewport.y;
        panes.into_iter().find(|(_, rect)| {
            x >= rect.x && x < rect.x + rect.width && y >= rect.y && y < rect.y + rect.height
        })
    }

    fn valid_drag(&self, source: &DraggedWorkspace, target: &SessionId) -> bool {
        let store = self
            .runtime
            .store
            .read()
            .expect("session store lock poisoned");
        let ids = if source.whole_workspace {
            self.layouts
                .containing(&source.session)
                .map(|tree| tree.ids())
        } else {
            None
        }
        .unwrap_or_else(|| vec![source.session.clone()]);
        if !ids.iter().chain(std::iter::once(target)).all(|id| {
            store
                .sessions()
                .get(id)
                .is_some_and(|record| !record.is_archived())
        }) {
            return false;
        }
        let mut layouts = self.layouts.clone();
        layouts.dock(
            target.clone(),
            source.session.clone(),
            source.whole_workspace,
            DockEdge::Right,
        )
    }

    pub fn drop_workspace(
        &mut self,
        source: &DraggedWorkspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let point = window.mouse_position();
        if let Some((target, rect)) = self.drop_pane(point)
            && self.valid_drag(source, &target)
            && let Some(edge) = edge_at(
                rect,
                f32::from(point.x) - self.viewport.x,
                f32::from(point.y) - self.viewport.y,
            )
            && self
                .layouts
                .dock(target, source.session.clone(), source.whole_workspace, edge)
        {
            self.persist();
            self.select(source.session.clone(), window, cx);
        }
        self.cancel_drag(cx);
        cx.stop_propagation();
    }

    pub fn drag_overlay(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !cx.has_active_drag() {
            self.dock = DockPresentation::default();
            return None;
        }
        let source = self.dock.source.clone()?;
        let (pane, rect) = self.drop_pane(window.mouse_position())?;
        if !self.valid_drag(&source, &pane) {
            return None;
        }
        let point = window.mouse_position();
        let pointer = (
            f32::from(point.x) - self.viewport.x,
            f32::from(point.y) - self.viewport.y,
        );
        let edge = edge_at(rect, pointer.0, pointer.1);
        let colors = crate::app_theme::colors(
            &self
                .runtime
                .store
                .read()
                .expect("session store lock poisoned")
                .preferences()
                .terminal_theme,
        );
        let now = Instant::now();
        let fresh = self.dock.pane.as_ref() != Some(&pane);
        self.dock.pane = Some(pane);
        if fresh {
            self.dock.motion.clear();
        }
        let mut overlay = div()
            .absolute()
            .left_0()
            .top_0()
            .w_full()
            .h(px(self.viewport.height));
        for (index, candidate) in DockEdge::ALL.into_iter().enumerate() {
            let active = edge == Some(candidate);
            let to = target_rect(
                rect,
                candidate,
                active,
                if cx.reduce_motion() {
                    (rect.x + rect.width / 2.0, rect.y + rect.height / 2.0)
                } else {
                    pointer
                },
            );
            if self.dock.motion.len() <= index {
                self.dock.motion.push(TargetMotion {
                    from: Rect {
                        y: to.y + 8.0,
                        height: to.height * 0.92,
                        ..to
                    },
                    to,
                    started: now,
                });
            }
            let motion = &mut self.dock.motion[index];
            if motion.to != to {
                *motion = TargetMotion {
                    from: motion.sample(now),
                    to,
                    started: now,
                };
            }
            let painted = if cx.reduce_motion() {
                to
            } else {
                motion.sample(now)
            };
            if !cx.reduce_motion()
                && now.duration_since(motion.started) < Duration::from_millis(180)
            {
                window.request_animation_frame();
            }
            overlay = overlay.child(
                div()
                    .absolute()
                    .left(px(painted.x))
                    .top(px(painted.y))
                    .w(px(painted.width))
                    .h(px(painted.height))
                    .rounded(px(12.0))
                    .border_1()
                    .border_color(if active {
                        diri_ui::Palette::CLAY.alpha(0.85)
                    } else {
                        colors.primary.alpha(0.16)
                    })
                    .bg(colors.background.alpha(0.96))
                    .shadow_md()
                    .child(
                        div()
                            .size_full()
                            .rounded(px(12.0))
                            .bg(colors.primary.alpha(if active { 0.13 } else { 0.035 }))
                            .flex()
                            .flex_col()
                            .items_center()
                            .justify_center()
                            .gap(px(7.0))
                            .overflow_hidden()
                            .child(sf_symbol("rectangle.split.2x1", 14.0, colors.primary))
                            .child(
                                div()
                                    .text_size(px(11.0))
                                    .text_color(if active {
                                        colors.primary
                                    } else {
                                        colors.secondary
                                    })
                                    .child(candidate.label()),
                            ),
                    ),
            );
        }
        Some(overlay.into_any_element())
    }

    pub fn preview(&self, source: &DraggedWorkspace) -> WorkspacePreview {
        let store = self
            .runtime
            .store
            .read()
            .expect("session store lock poisoned");
        let tree = if source.whole_workspace {
            self.layouts.containing(&source.session).cloned()
        } else {
            None
        }
        .unwrap_or_else(|| SplitNode::session(source.session.clone()));
        let labels = tree
            .ids()
            .into_iter()
            .filter_map(|id| {
                store
                    .sessions()
                    .get(&id)
                    .map(|session| (id, session.title.clone()))
            })
            .collect();
        WorkspacePreview {
            tree,
            labels,
            colors: crate::app_theme::colors(&store.preferences().terminal_theme),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn drop_hitboxes_leave_a_cancel_region_and_reject_outside_points() {
        let rect = Rect {
            x: 10.0,
            y: 20.0,
            width: 800.0,
            height: 600.0,
        };
        assert_eq!(edge_at(rect, 15.0, 320.0), Some(DockEdge::Left));
        assert_eq!(edge_at(rect, 800.0, 320.0), Some(DockEdge::Right));
        assert_eq!(edge_at(rect, 410.0, 25.0), Some(DockEdge::Top));
        assert_eq!(edge_at(rect, 410.0, 615.0), Some(DockEdge::Bottom));
        assert_eq!(edge_at(rect, 410.0, 320.0), None);
        assert_eq!(edge_at(rect, 9.0, 320.0), None);
        assert_eq!(edge_at(Rect::default(), 0.0, 0.0), None);
    }
    #[test]
    fn target_motion_retargets_from_the_current_position() {
        let now = Instant::now();
        let from = Rect {
            width: 100.0,
            height: 200.0,
            ..Rect::default()
        };
        let to = Rect {
            x: 20.0,
            y: 12.0,
            width: 108.0,
            height: 216.0,
        };
        let motion = TargetMotion {
            from,
            to,
            started: now,
        };
        let middle = now + Duration::from_millis(70);
        let reverse = TargetMotion {
            from: motion.sample(middle),
            to: from,
            started: middle,
        };
        assert_eq!(reverse.sample(middle), motion.sample(middle));
        assert_eq!(reverse.sample(middle + Duration::from_millis(180)), from);
    }
}
