//! The Find field owns Cocoa text input while open. Its handler is tied to one
//! pane, residency and editing epoch; delayed native callbacks never retarget
//! whichever session happens to be selected later.
use super::{AttachmentGeneration, QueryEditor, SessionId, TerminalPane};
pub(super) use crate::text_input::{Composition, discard_native};
use crate::text_input::{byte_range, utf16_range};
use diri_ui::{SemanticColors, Typo};
use gpui::{
    AnyElement, App, Bounds, ContentMask, Context, FocusHandle, InputHandler, Pixels, Point,
    ShapedLine, TextAlign, TextRun, UTF16Selection, WeakEntity, Window, canvas, fill, font, point,
    prelude::*, px, size,
};
use std::{ops::Range, time::Duration};

#[derive(Clone)]
struct Owner {
    pane: WeakEntity<TerminalPane>,
    session: SessionId,
    residency: AttachmentGeneration,
    epoch: u64,
}
impl Owner {
    fn with<T>(
        &self,
        window: &mut Window,
        cx: &mut App,
        f: impl FnOnce(&mut TerminalPane, &mut Window, &mut Context<TerminalPane>) -> T,
    ) -> Option<T> {
        self.pane
            .update(cx, |pane, cx| {
                if pane.selected_id().as_ref() != Some(&self.session) {
                    return None;
                }
                if !pane.focus.is_focused(window) {
                    // Input callbacks can precede the frame which emits blur.
                    // Cancel preedit immediately when its logical owner is gone.
                    pane.cancel_find_composition(window, cx);
                    return None;
                }
                let resident = pane.residents.get(&self.session)?;
                if resident.attachment_generation != self.residency
                    || resident.find.is_none()
                    || resident.find_composition.epoch != self.epoch
                {
                    return None;
                }
                Some(f(pane, window, cx))
            })
            .ok()
            .flatten()
    }
    fn edit(
        &self,
        window: &mut Window,
        cx: &mut App,
        edit: impl FnOnce(&mut Composition, &mut QueryEditor),
    ) {
        self.with(window, cx, |pane, window, cx| {
            let resident = pane.residents.get_mut(&self.session).unwrap();
            edit(&mut resident.find_composition, &mut resident.find_query);
            if resident.find.as_mut().unwrap().set_query(
                resident.find_query.text().to_owned(),
                pane.started_at.elapsed(),
            ) {
                resident.element.set_find_highlights(Vec::new());
            }
            pane.schedule_find(self.session.clone(), Duration::from_millis(200), window, cx);
            window.invalidate_character_coordinates();
            cx.notify();
        });
    }
}

#[derive(Clone)]
struct Geometry {
    bounds: Bounds<Pixels>,
    colors: SemanticColors,
}
impl Geometry {
    fn line(&self, text: &str, window: &Window) -> ShapedLine {
        window.text_system().shape_line(
            text.to_owned().into(),
            px(Typo::ROW.size),
            &[TextRun {
                len: text.len(),
                font: font(crate::fonts::ui_family()),
                color: self.colors.primary.into(),
                ..Default::default()
            }],
            None,
        )
    }
    fn origin(&self, line: &ShapedLine, cursor: usize) -> Point<Pixels> {
        let scroll = (line.x_for_index(cursor) - self.bounds.size.width + px(2.0)).max(px(0.0));
        self.bounds.origin - point(scroll, px(0.0))
    }
    fn range(&self, query: &QueryEditor, range: Range<usize>, window: &Window) -> Bounds<Pixels> {
        let range = byte_range(query.text(), range);
        let line = self.line(query.text(), window);
        let origin = self.origin(&line, query.cursor());
        let left = (origin.x + line.x_for_index(range.start))
            .clamp(self.bounds.left(), self.bounds.right());
        let right = (origin.x + line.x_for_index(range.end)).clamp(left, self.bounds.right());
        Bounds::new(
            point(left, origin.y),
            size((right - left).max(px(1.0)), self.bounds.size.height),
        )
    }
}

struct Handler {
    owner: Owner,
    geometry: Geometry,
}
impl InputHandler for Handler {
    fn selected_text_range(
        &mut self,
        _: bool,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<UTF16Selection> {
        self.owner.with(window, cx, |pane, _, _| {
            let query = &pane.residents[&self.owner.session].find_query;
            let range = query.selection().unwrap_or(query.cursor()..query.cursor());
            UTF16Selection {
                reversed: query.cursor() == range.start && !range.is_empty(),
                range: utf16_range(query.text(), range),
            }
        })
    }
    fn marked_text_range(&mut self, window: &mut Window, cx: &mut App) -> Option<Range<usize>> {
        self.owner
            .with(window, cx, |pane, _, _| {
                let resident = &pane.residents[&self.owner.session];
                resident
                    .find_composition
                    .marked()
                    .map(|range| utf16_range(resident.find_query.text(), range))
            })
            .flatten()
    }
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        adjusted: &mut Option<Range<usize>>,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<String> {
        self.owner.with(window, cx, |pane, _, _| {
            let text = pane.residents[&self.owner.session].find_query.text();
            let range = byte_range(text, range);
            *adjusted = Some(utf16_range(text, range.clone()));
            text[range].to_owned()
        })
    }
    fn replace_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.owner.edit(window, cx, |composition, query| {
            composition.replace(query, range, text, None, false)
        });
    }
    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        selected: Option<Range<usize>>,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.owner.edit(window, cx, |composition, query| {
            composition.replace(query, range, text, selected, true)
        });
    }
    fn unmark_text(&mut self, window: &mut Window, cx: &mut App) {
        self.owner
            .edit(window, cx, |composition, _| composition.finish());
    }
    fn bounds_for_range(
        &mut self,
        range: Range<usize>,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<Bounds<Pixels>> {
        self.owner.with(window, cx, |pane, window, _| {
            self.geometry.range(
                &pane.residents[&self.owner.session].find_query,
                range,
                window,
            )
        })
    }
    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<usize> {
        self.owner.with(window, cx, |pane, window, _| {
            let query = &pane.residents[&self.owner.session].find_query;
            let line = self.geometry.line(query.text(), window);
            let byte =
                line.closest_index_for_x(point.x - self.geometry.origin(&line, query.cursor()).x);
            query.text()[..byte].encode_utf16().count()
        })
    }
    fn prefers_ime_for_printable_keys(&mut self, _: &mut Window, _: &mut App) -> bool {
        true
    }
}

pub(super) fn render(
    pane: &TerminalPane,
    session: &SessionId,
    colors: SemanticColors,
    cx: &Context<TerminalPane>,
) -> AnyElement {
    let resident = &pane.residents[session];
    let owner = Owner {
        pane: cx.entity().downgrade(),
        session: session.clone(),
        residency: resident.attachment_generation,
        epoch: resident.find_composition.epoch,
    };
    let query = resident.find_query.clone();
    let marked = resident.find_composition.marked();
    let focus: FocusHandle = pane.focus.clone();
    canvas(
        |bounds, _, _| bounds,
        move |_, bounds, window, cx| {
            let geometry = Geometry { bounds, colors };
            window.with_content_mask(Some(ContentMask { bounds }), |window| {
                let text = if query.is_empty() {
                    "Find"
                } else {
                    query.text()
                };
                let line = geometry.line(text, window);
                let origin = geometry.origin(&line, query.cursor());
                if let Some(range) = query.selection() {
                    window.paint_quad(fill(
                        geometry.range(&query, utf16_range(query.text(), range), window),
                        colors.primary.alpha(0.16),
                    ));
                }
                let _ = line.paint(
                    origin,
                    bounds.size.height,
                    TextAlign::Left,
                    None,
                    window,
                    cx,
                );
                if let Some(range) = &marked {
                    let mut rect =
                        geometry.range(&query, utf16_range(query.text(), range.clone()), window);
                    rect.origin.y = rect.bottom() - px(1.0);
                    rect.size.height = px(1.0);
                    window.paint_quad(fill(rect, colors.primary));
                }
                if query.selection().is_none() {
                    let caret = query.text()[..query.cursor()].encode_utf16().count();
                    window.paint_quad(fill(
                        geometry.range(&query, caret..caret, window),
                        colors.primary,
                    ));
                }
            });
            window.handle_input(
                &focus,
                Handler {
                    owner: owner.clone(),
                    geometry,
                },
                cx,
            );
        },
    )
    .w_full()
    .h(px(18.0))
    .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[gpui::test]
    fn candidate_geometry_follows_relocated_query_and_keeps_long_caret_visible(
        cx: &mut gpui::TestAppContext,
    ) {
        let (_, cx) = cx.add_window_view(|_, _| gpui::Empty);
        cx.update(|window, _| {
            let colors = SemanticColors::dark();
            let mut query = QueryEditor::default();
            query.insert("wide 界 e\u{301} 😀 long query that needs horizontal scrolling");
            let original = Geometry {
                bounds: Bounds::new(point(px(80.0), px(10.0)), size(px(100.0), px(18.0))),
                colors,
            };
            let moved = Geometry {
                bounds: Bounds::new(point(px(80.0), px(74.0)), size(px(100.0), px(18.0))),
                colors,
            };
            let caret = query.text().encode_utf16().count();
            let a = original.range(&query, caret..caret, window);
            let b = moved.range(&query, caret..caret, window);
            assert_eq!(b.origin.y - a.origin.y, px(64.0));
            assert!(b.left() >= moved.bounds.left() && b.right() <= moved.bounds.right());
            assert_eq!(b.size.width, px(1.0));
            let start = moved.range(&query, 0..0, window);
            assert_eq!(start.left(), moved.bounds.left());
        });
    }
}
