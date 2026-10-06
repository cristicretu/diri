//! Cocoa text input for the single-line `QueryEditor` fields that paint their
//! own text (the terminal Find field, the sidebar's inline rename). An input
//! method only reaches a field that registers a GPUI `InputHandler`; this is
//! that handler, its marked-text state, and the painter whose geometry places
//! the candidate window at the caret.
use crate::query_editor::QueryEditor;
use diri_ui::{SemanticColors, Typo};
use gpui::{
    AnyElement, App, Bounds, ContentMask, FocusHandle, InputHandler, Pixels, Point, Rgba,
    ShapedLine, SharedString, TextAlign, TextRun, UTF16Selection, Window, canvas, fill, font,
    point, prelude::*, px, size,
};
use std::ops::Range;

#[derive(Debug, Default)]
pub(crate) struct Composition {
    pub epoch: u64,
    marked: Option<Range<usize>>,
    original: Option<QueryEditor>,
}

impl Composition {
    pub fn is_composing(&self) -> bool {
        self.original.is_some()
    }

    pub fn finish(&mut self) {
        self.marked = None;
        self.original = None;
    }

    pub fn commit(&mut self, query: &mut QueryEditor, text: &str) {
        self.replace(query, None, text, None, false);
    }

    pub fn cancel(&mut self, query: &mut QueryEditor) {
        if let Some(original) = self.original.take() {
            *query = original;
        }
        self.marked = None;
        self.epoch = self.epoch.wrapping_add(1);
    }

    /// Marks `text` as preedit at the caret, as an input method would. Only
    /// the macOS sidebar screenshot drives it.
    #[cfg(all(test, target_os = "macos"))]
    pub fn compose(&mut self, query: &mut QueryEditor, text: &str, selected: Range<usize>) {
        self.replace(query, None, text, Some(selected), true);
    }

    fn replace(
        &mut self,
        query: &mut QueryEditor,
        range: Option<Range<usize>>,
        text: &str,
        selected: Option<Range<usize>>,
        composing: bool,
    ) {
        let range = range
            .map(|range| byte_range(query.text(), range))
            .or_else(|| self.marked.clone())
            .or_else(|| query.selection())
            .unwrap_or(query.cursor()..query.cursor());
        if composing && self.original.is_none() {
            self.original = Some(query.clone());
        }
        query.set_cursor(range.start, false);
        query.set_cursor(range.end, true);
        query.insert(text);
        let end = query.cursor();
        if composing && end > range.start {
            self.marked = Some(range.start..end);
            if let Some(selected) = selected {
                let local = byte_range(&query.text()[range.start..end], selected);
                query.set_cursor(range.start + local.start, false);
                query.set_cursor(range.start + local.end, true);
            }
        } else {
            self.marked = None;
            self.original = None;
        }
    }
}

// Cocoa offsets count UTF-16 units. A malformed half-surrogate range expands
// to scalar boundaries, never slicing UTF-8 or dropping half an emoji.
fn byte_offset(text: &str, units: usize, ceil: bool) -> usize {
    let mut count = 0;
    for (byte, ch) in text.char_indices() {
        if count >= units {
            return byte;
        }
        count += ch.len_utf16();
        if count > units {
            return if ceil { byte + ch.len_utf8() } else { byte };
        }
    }
    text.len()
}
fn byte_range(text: &str, range: Range<usize>) -> Range<usize> {
    let start = byte_offset(text, range.start, false);
    let end = if range.is_empty() {
        start
    } else {
        byte_offset(text, range.end.max(range.start), true)
    };
    start..end
}
fn utf16_range(text: &str, range: Range<usize>) -> Range<usize> {
    text[..range.start].encode_utf16().count()..text[..range.end].encode_utf16().count()
}

/// The field a handler was installed for. Native callbacks can arrive after
/// the field closed or moved on; `read` and `edit` return without touching
/// anything unless the same field, at the same epoch, still owns input.
pub(crate) trait Owner: 'static {
    fn read<T>(
        &self,
        window: &mut Window,
        cx: &mut App,
        f: impl FnOnce(&QueryEditor, &Composition, &Window) -> T,
    ) -> Option<T>;
    fn edit(
        &self,
        window: &mut Window,
        cx: &mut App,
        edit: impl FnOnce(&mut Composition, &mut QueryEditor),
    );
}

#[derive(Clone)]
pub(crate) struct Style {
    pub colors: SemanticColors,
    pub selection: Rgba,
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

struct Handler<O> {
    owner: O,
    geometry: Geometry,
}
impl<O: Owner> InputHandler for Handler<O> {
    fn selected_text_range(
        &mut self,
        _: bool,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<UTF16Selection> {
        self.owner.read(window, cx, |query, _, _| {
            let range = query.selection().unwrap_or(query.cursor()..query.cursor());
            UTF16Selection {
                reversed: query.cursor() == range.start && !range.is_empty(),
                range: utf16_range(query.text(), range),
            }
        })
    }
    fn marked_text_range(&mut self, window: &mut Window, cx: &mut App) -> Option<Range<usize>> {
        self.owner
            .read(window, cx, |query, composition, _| {
                composition
                    .marked
                    .clone()
                    .map(|range| utf16_range(query.text(), range))
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
        self.owner.read(window, cx, |query, _, _| {
            let text = query.text();
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
        self.owner.read(window, cx, |query, _, window| {
            self.geometry.range(query, range, window)
        })
    }
    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<usize> {
        self.owner.read(window, cx, |query, _, window| {
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

/// Paints `query` (or `placeholder` while it is empty) with its selection,
/// marked-text underline and caret, and installs `owner`'s input handler
/// while `focus` holds keyboard focus.
pub(crate) fn render<O: Owner + Clone>(
    query: QueryEditor,
    composition: &Composition,
    placeholder: SharedString,
    style: Style,
    focus: FocusHandle,
    owner: O,
) -> AnyElement {
    let marked = composition.marked.clone();
    let colors = style.colors;
    canvas(
        |bounds, _, _| bounds,
        move |_, bounds, window, cx| {
            let geometry = Geometry { bounds, colors };
            window.with_content_mask(Some(ContentMask { bounds }), |window| {
                let text = if query.is_empty() {
                    placeholder.as_ref()
                } else {
                    query.text()
                };
                let line = geometry.line(text, window);
                let origin = geometry.origin(&line, query.cursor());
                if let Some(range) = query.selection() {
                    window.paint_quad(fill(
                        geometry.range(&query, utf16_range(query.text(), range), window),
                        style.selection,
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

/// The handler `render` installs, for driving input-method callbacks in tests.
#[cfg(test)]
pub(crate) fn test_handler<O: Owner>(
    owner: O,
    bounds: Bounds<Pixels>,
    colors: SemanticColors,
) -> impl InputHandler {
    Handler {
        owner,
        geometry: Geometry { bounds, colors },
    }
}

/// Defer Cocoa cancellation until the owner's update has completed: discard
/// can synchronously query its installed input handler. Never commit preedit.
pub(crate) fn discard_native(window: &Window, cx: &mut App) {
    // AppKit input contexts exist only on its main thread. GPUI's worker-thread
    // test platform has no native window handle and still exercises model cancellation.
    #[cfg(target_os = "macos")]
    if objc2::MainThreadMarker::new().is_none() {
        return;
    }
    #[cfg(target_os = "macos")]
    window.defer(cx, |window, _| {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        if let Ok(handle) = window.window_handle()
            && let RawWindowHandle::AppKit(handle) = handle.as_raw()
        {
            unsafe {
                let view = &*handle.ns_view.as_ptr().cast::<objc2::runtime::AnyObject>();
                let context: *mut objc2::runtime::AnyObject = objc2::msg_send![view, inputContext];
                if !context.is_null() {
                    let _: () = objc2::msg_send![context, discardMarkedText];
                }
            }
        }
    });
    #[cfg(not(target_os = "macos"))]
    let _ = (window, cx);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cocoa_ranges_round_trip_wide_combining_and_surrogate_text() {
        let text = "a😀e\u{301}界";
        assert_eq!(byte_range(text, 1..3), 1..5);
        assert_eq!(byte_range(text, 2..3), 1..5);
        assert_eq!(byte_range(text, 2..2), 1..1);
        assert_eq!(utf16_range(text, 5..8), 3..5);
        assert_eq!(byte_range(text, 0..usize::MAX), 0..text.len());
        for (index, _) in text
            .char_indices()
            .chain(std::iter::once((text.len(), '\0')))
        {
            let units = text[..index].encode_utf16().count();
            assert_eq!(byte_range(text, units..units), index..index);
        }
    }

    #[test]
    fn composition_replaces_selected_text_commits_once_and_cancel_restores() {
        let mut query = QueryEditor::default();
        query.insert("left 😀 right");
        query.set_cursor(5, false);
        query.set_cursor(9, true);
        let original = query.clone();
        let mut composition = Composition::default();
        composition.replace(&mut query, None, "ni", Some(1..2), true);
        assert_eq!(query.text(), "left ni right");
        assert_eq!(query.selected_text(), Some("i"));
        assert_eq!(composition.marked, Some(5..7));
        composition.replace(&mut query, None, "你😀", Some(3..3), true);
        assert_eq!(query.text(), "left 你😀 right");
        assert_eq!(query.cursor(), 12);
        composition.cancel(&mut query);
        assert_eq!(query, original);
        assert_eq!(composition.epoch, 1);
        composition.replace(&mut query, None, "ni", Some(2..2), true);
        composition.replace(&mut query, None, "你", None, false);
        assert_eq!(query.text(), "left 你 right");
        assert!(composition.marked.is_none());
        composition.cancel(&mut query);
        assert_eq!(query.text(), "left 你 right");
    }

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
