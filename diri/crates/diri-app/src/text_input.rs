//! The pieces every app-owned text field needs to sit behind a native input
//! handler: marked-text (IME preedit) state over a [`QueryEditor`], the
//! UTF-16 ⇄ byte range mapping Cocoa's offsets require, and the way to tell
//! AppKit a composition is over. The terminal's Find field and the prompt
//! composers share them; what differs between the two — who owns the editor,
//! where its text is drawn — stays with each field's own handler.
use crate::query_editor::QueryEditor;
use gpui::{App, Window};
use std::ops::Range;

#[derive(Default)]
pub struct Composition {
    pub epoch: u64,
    marked: Option<Range<usize>>,
    original: Option<QueryEditor>,
    /// Whether inserted text keeps its line breaks. Search fields are
    /// strictly single-line; a prompt is not, and dictation can commit a
    /// paragraph.
    multiline: bool,
}

impl Composition {
    pub fn multiline() -> Self {
        Self {
            multiline: true,
            ..Self::default()
        }
    }

    pub fn is_composing(&self) -> bool {
        self.original.is_some()
    }

    /// The marked (preedit) text, as a byte range of the editor.
    pub fn marked(&self) -> Option<Range<usize>> {
        self.marked.clone()
    }

    /// The editor as it was before the composition touched it: what a cancel
    /// returns to, and what an undo step for the committed text starts from.
    pub fn original(&self) -> Option<&QueryEditor> {
        self.original.as_ref()
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

    pub fn replace(
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
        if self.multiline {
            query.insert_multiline(text);
        } else {
            query.insert(text);
        }
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
pub fn byte_range(text: &str, range: Range<usize>) -> Range<usize> {
    let start = byte_offset(text, range.start, false);
    let end = if range.is_empty() {
        start
    } else {
        byte_offset(text, range.end.max(range.start), true)
    };
    start..end
}
pub fn utf16_range(text: &str, range: Range<usize>) -> Range<usize> {
    text[..range.start].encode_utf16().count()..text[..range.end].encode_utf16().count()
}

/// Defer Cocoa cancellation until the owning view's update has completed:
/// discard can synchronously query its installed input handler. Never commit
/// preedit on the field's behalf.
pub fn discard_native(window: &Window, cx: &mut App) {
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

    #[test]
    fn only_a_multiline_composition_keeps_committed_line_breaks() {
        let mut query = QueryEditor::default();
        Composition::default().commit(&mut query, "one\ntwo");
        assert_eq!(query.text(), "onetwo");
        let mut query = QueryEditor::default();
        Composition::multiline().commit(&mut query, "one\ntwo");
        assert_eq!(query.text(), "one\ntwo");
    }
}
