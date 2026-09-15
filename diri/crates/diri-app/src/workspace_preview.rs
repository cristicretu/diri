//! Paint-only split thumbnails. All buffers are borrowed client grids.
use crate::workspace_geometry::{Rect, WorkspaceGeometry};
use diri_proto::SessionId;
use diri_term::{element::TerminalElement, theme::TermTheme};
use diri_ui::SemanticColors;
use gpui::{AnyElement, SharedString, div, prelude::*, px};
use std::collections::HashMap;

pub(crate) fn render_workspace_preview(
    settled: &WorkspaceGeometry,
    width: f32,
    height: f32,
    buffers: &HashMap<SessionId, TerminalElement>,
    theme: TermTheme,
    colors: SemanticColors,
) -> AnyElement {
    let mut root = div()
        .relative()
        .w(px(width))
        .h(px(height))
        .overflow_hidden()
        .bg(colors.background);
    let Some(geometry) = settled.fit(Rect {
        width,
        height,
        ..Rect::default()
    }) else {
        return root.into_any_element();
    };
    for divider in geometry.dividers {
        root = root.child(
            div()
                .absolute()
                .left(px(divider.x))
                .top(px(divider.y))
                .w(px(divider.width))
                .h(px(divider.height))
                .bg(colors.primary.alpha(0.18)),
        );
    }
    for pane in geometry.panes {
        let bounds = pane.bounds;
        let mut view = div()
            .id(SharedString::from(format!(
                "workspace-preview-pane-{}",
                pane.identity.pane.0
            )))
            .absolute()
            .left(px(bounds.x))
            .top(px(bounds.y))
            .w(px(bounds.width))
            .h(px(bounds.height))
            .overflow_hidden()
            .p(px(2.0));
        if let Some(buffer) = buffers.get(&pane.identity.session) {
            let font_size = ((bounds.width - 4.0) / (f32::from(buffer.grid_cols().max(1)) * 0.65))
                .min((bounds.height - 4.0) / (f32::from(buffer.grid_rows().max(1)) * 1.5))
                .max(0.1);
            view = view.child(
                buffer
                    .clone()
                    .focused(false)
                    .font(gpui::font(crate::fonts::mono_family()))
                    .font_size(px(font_size))
                    .theme(theme),
            );
        } else {
            // The containing card reports its exact available/visible count.
            // Missing a source must not invent terminal text or a live state.
            view = view.bg(colors.primary.alpha(0.04));
        }
        root = root.child(view);
    }
    root.into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace_geometry::PaneIdentity;
    use diri_proto::workspace::{LayoutAxis, LayoutNode, PaneId, SplitId, TabId, WorkspaceTab};
    use diri_term::buffer::GridBuffer;
    use gpui::{Context, Render, Window, size};

    struct PreviewHarness {
        geometry: WorkspaceGeometry,
        buffers: HashMap<SessionId, TerminalElement>,
        selected: Option<PaneIdentity>,
    }
    impl Render for PreviewHarness {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let colors = crate::app_theme::colors("dirijor-dark");
            div().size_full().bg(colors.background).p(px(20.0)).child(
                div()
                    .id("select-split-tab")
                    .debug_selector(|| "select-split-tab".into())
                    .w(px(400.0))
                    .h(px(240.0))
                    .child(render_workspace_preview(
                        &self.geometry,
                        400.0,
                        240.0,
                        &self.buffers,
                        crate::app_theme::terminal_theme("dirijor-dark"),
                        colors,
                    ))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.selected = Some(this.geometry.focused.clone());
                        cx.notify();
                    })),
            )
        }
    }
    fn fixture() -> PreviewHarness {
        let node = |id: &str| LayoutNode::Pane {
            id: PaneId::new(id),
            session_id: SessionId::new(format!("session-{id}")),
        };
        let tab = WorkspaceTab {
            id: TabId::new("tab-work"),
            title: Some("Nested workspace".into()),
            focused_pane: PaneId::new("right-bottom"),
            zoomed_pane: None,
            layout: LayoutNode::Split {
                id: SplitId::new("horizontal"),
                axis: LayoutAxis::Horizontal,
                fraction: 0.7,
                first: Box::new(node("left")),
                second: Box::new(LayoutNode::Split {
                    id: SplitId::new("vertical"),
                    axis: LayoutAxis::Vertical,
                    fraction: 0.3,
                    first: Box::new(node("right-top")),
                    second: Box::new(node("right-bottom")),
                }),
            },
        };
        let geometry = WorkspaceGeometry::settled(
            &tab,
            Rect {
                width: 1005.0,
                height: 605.0,
                ..Rect::default()
            },
        )
        .unwrap();
        let buffers = geometry
            .panes
            .iter()
            .map(|pane| {
                let mut grid = GridBuffer::new(80, 24);
                let text = format!(
                    "$ echo {}\n{}\n\n$ cargo test\n\nAll checks passed\n\n$ ",
                    pane.identity.pane.0, pane.identity.session.0
                );
                for (y, line) in text.lines().enumerate() {
                    for (x, ch) in line.chars().enumerate() {
                        grid.cells[y * 80 + x].scalar = ch as u32;
                    }
                }
                (
                    pane.identity.session.clone(),
                    TerminalElement::with_buffer(grid).focused(false),
                )
            })
            .collect();
        PreviewHarness {
            geometry,
            buffers,
            selected: None,
        }
    }
    #[gpui::test]
    fn split_card_selects_the_saved_pane_and_session_without_resizing_buffers(
        cx: &mut gpui::TestAppContext,
    ) {
        let (view, cx) = cx.add_window_view(|_, _| fixture());
        cx.simulate_resize(size(px(460.0), px(300.0)));
        let card = cx.debug_bounds("select-split-tab").unwrap();
        cx.simulate_click(card.center(), gpui::Modifiers::default());
        view.read_with(cx, |view, _| {
            assert_eq!(
                view.selected,
                Some(PaneIdentity {
                    pane: PaneId::new("right-bottom"),
                    session: SessionId::new("session-right-bottom")
                })
            );
            assert_eq!(view.geometry.tab, TabId::new("tab-work"));
            for buffer in view.buffers.values() {
                assert_eq!((buffer.grid_cols(), buffer.grid_rows()), (80, 24));
            }
        });
    }
}
