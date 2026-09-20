//! The multi-line prompt field behind Command-N.
//!
//! [`QueryEditor`](crate::query_editor::QueryEditor) is a buffer: it knows
//! where the caret is in BYTES and nothing about where that lands on screen.
//! That is all a one-line search field needs, and the Command-N composer
//! inherited it — which is why a prompt longer than the box scrolled out of
//! sight with no way to follow it, why ↑/↓ did nothing, and why ⌘← jumped to
//! the top of the whole prompt instead of the start of the line you were on.
//!
//! This adds the missing half: the buffer soft-wrapped into VISUAL lines at
//! the field's real width, which is what makes "the line the caret is on"
//! a thing that exists. From that follows caret-following scroll, vertical
//! motion that keeps its column, and a box that grows with the prompt until
//! it hits a ceiling and starts scrolling instead.
//!
//! Wrapping needs the text system, so it is recomputed during render (which
//! has a `Window`) rather than on each keystroke, and cached against the text
//! it was computed from.

use std::ops::Range;

use gpui::{AnyElement, Pixels, ScrollHandle, SharedString, Window, div, prelude::*, px};
use unicode_segmentation::UnicodeSegmentation;

use crate::query_editor::{self, EditHistory, EditKind, LocalEdit, Motion, QueryEditor};

/// One soft-wrapped display line: a byte range of the buffer, plus whether it
/// ended because the text wrapped rather than because the user pressed Return.
/// The distinction matters for the caret: at a soft break the caret belongs to
/// the start of the FOLLOWING line (there is no position after the last
/// character of a wrapped line — it is the same position), while at a hard
/// break both ends are real places to be.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VisualLine {
    pub range: Range<usize>,
    pub soft_wrapped: bool,
}

/// A prompt buffer plus everything needed to draw and navigate it as a
/// multi-line field.
pub struct PromptComposer {
    editor: QueryEditor,
    /// Undo/redo for the draft in `editor`, and only that draft: loading
    /// another one through [`Self::reset`] starts over, so ⌘Z can never pull
    /// one session's text into another's prompt.
    history: EditHistory,
    scroll: ScrollHandle,
    lines: Vec<VisualLine>,
    /// The (text, width) the cached `lines` were computed from.
    wrapped_from: Option<(String, Pixels)>,
    /// Set by any edit; cleared once render has scrolled the caret into view.
    reveal_caret: bool,
    /// Column the caret returns to when vertical motion passes through a
    /// short line, in graphemes. `None` outside a run of ↑/↓.
    goal_column: Option<usize>,
}

impl Default for PromptComposer {
    fn default() -> Self {
        Self {
            editor: QueryEditor::default(),
            history: EditHistory::default(),
            scroll: ScrollHandle::new(),
            lines: Vec::new(),
            wrapped_from: None,
            reveal_caret: false,
            goal_column: None,
        }
    }
}

impl PromptComposer {
    pub fn text(&self) -> &str {
        self.editor.text()
    }

    pub fn is_empty(&self) -> bool {
        self.editor.is_empty()
    }

    pub fn editor(&self) -> &QueryEditor {
        &self.editor
    }

    pub fn clear(&mut self) {
        self.editor.clear();
        self.history.clear();
        self.lines.clear();
        self.wrapped_from = None;
        self.goal_column = None;
        self.reveal_caret = true;
    }

    /// Replaces the draft with another one — a saved draft, a recipe, a
    /// handoff summary. Loading is not an edit: the history starts empty, so
    /// the first ⌘Z cannot blank a draft the user only just came back to.
    pub fn reset(&mut self, text: &str) {
        self.clear();
        self.editor.insert_multiline(text);
    }

    /// Runs an edit against the buffer and records it as one undo step if it
    /// changed the text. An edit that only moved the caret or the selection
    /// ends the open typing run instead.
    fn record<T>(&mut self, kind: EditKind, edit: impl FnOnce(&mut QueryEditor) -> T) -> T {
        let before = self.editor.clone();
        let result = edit(&mut self.editor);
        if self.editor.text() == before.text() {
            self.history.break_run();
        } else {
            self.history.record(kind, before, &self.editor);
        }
        self.goal_column = None;
        self.reveal_caret = true;
        result
    }

    /// ⌘Z. Restores the text, caret and selection from before the last step.
    pub fn undo(&mut self) -> bool {
        self.goal_column = None;
        self.reveal_caret = true;
        self.history.undo(&mut self.editor)
    }

    /// ⇧⌘Z.
    pub fn redo(&mut self) -> bool {
        self.goal_column = None;
        self.reveal_caret = true;
        self.history.redo(&mut self.editor)
    }

    /// Drops every undo and redo step while keeping the draft as it is.
    pub fn forget_history(&mut self) {
        self.history.clear();
    }

    /// ⌘X. Returns whether there was a selection to cut.
    pub fn cut_selection(&mut self, cx: &mut gpui::App) -> bool {
        self.record(EditKind::Other, |editor| {
            query_editor::cut_selection(editor, cx)
        })
    }

    /// How many visual lines the prompt currently occupies, as last wrapped.
    /// One even when empty — the field is always at least a line tall.
    pub fn line_count(&self) -> usize {
        self.lines.len().max(1)
    }

    /// Applies an edit from the shared key map, keeping the caret in view.
    /// Line-granular edits act on the caret's VISUAL line rather than the
    /// whole buffer — `QueryEditor` reads `Motion::Line` as "everything",
    /// which is right for a search field and wrong for a prompt — so ⌘⌫ on
    /// the third line of a prompt deletes that line and not the prompt.
    pub fn apply(&mut self, edit: LocalEdit) {
        let line = self
            .caret_line()
            .map(|index| self.lines[index].range.clone());
        self.record(EditKind::of(&edit), |editor| match (edit, line) {
            (LocalEdit::MoveLeft(Motion::Line, extend), Some(line)) => {
                editor.move_to(line.start, extend);
            }
            (LocalEdit::MoveRight(Motion::Line, extend), Some(line)) => {
                editor.move_to(line.end, extend);
            }
            (LocalEdit::DeleteBackward(Motion::Line), Some(line)) => {
                editor.delete_to(line.start);
            }
            (LocalEdit::DeleteForward(Motion::Line), Some(line)) => {
                editor.delete_to(line.end);
            }
            (edit, _) => {
                editor.apply(edit);
            }
        });
    }

    /// ↑ / ↓. Kept off the shared key map on purpose: in the palette and
    /// Quick Open those keys move the highlighted row, and only a field that
    /// has visual lines can answer them at all.
    pub fn move_up(&mut self, extend: bool) {
        self.move_vertically(-1, extend);
    }

    pub fn move_down(&mut self, extend: bool) {
        self.move_vertically(1, extend);
    }

    /// A paste or a ⇧↵ line break: always an undo step of its own.
    pub fn insert_multiline(&mut self, text: &str) {
        self.record(EditKind::Other, |editor| {
            editor.insert_multiline(text);
        });
    }

    /// Append staged context without replacing a draft the user already
    /// wrote. A new block starts on its own line unless the draft already
    /// ends in whitespace, keeping quoted paths visually separate while
    /// preserving the existing text and the staged block's semantic
    /// whitespace.
    pub fn append_context(&mut self, context: &str) {
        if context.is_empty() {
            return;
        }
        self.record(EditKind::Other, |editor| {
            if !editor.is_empty()
                && !editor
                    .text()
                    .chars()
                    .next_back()
                    .is_some_and(char::is_whitespace)
            {
                editor.insert_context("\n");
            }
            editor.insert_context(context);
        });
    }

    /// Index of the visual line the caret sits on. `None` before the first
    /// wrap, when there is no layout to consult.
    pub fn caret_line(&self) -> Option<usize> {
        let cursor = self.editor.cursor();
        if self.lines.is_empty() {
            return None;
        }
        // The LAST line that starts at or before the caret: at a soft break
        // this puts the caret at the head of the new line, where the next
        // typed character will actually appear.
        self.lines
            .iter()
            .rposition(|line| line.range.start <= cursor)
            .or(Some(0))
    }

    fn move_vertically(&mut self, delta: isize, extend: bool) {
        self.reveal_caret = true;
        self.history.break_run();
        let Some(current) = self.caret_line() else {
            return;
        };
        let line = self.lines[current].range.clone();
        let caret = self.editor.cursor().clamp(line.start, line.end);
        // The column is remembered across a run of ↑/↓ so passing through a
        // short line does not permanently drag the caret left.
        let column = self
            .goal_column
            .unwrap_or_else(|| grapheme_count(&self.editor.text()[line.start..caret]));
        self.goal_column = Some(column);

        let Some(target) = current
            .checked_add_signed(delta)
            .filter(|index| *index < self.lines.len())
        else {
            // Past the first or last line: park at the buffer's edge, the way
            // every macOS text view does.
            let offset = if delta < 0 {
                0
            } else {
                self.editor.text().len()
            };
            self.editor.move_to(offset, extend);
            self.goal_column = None;
            return;
        };
        let target = self.lines[target].range.clone();
        let offset = offset_at_column(self.editor.text(), &target, column);
        self.editor.move_to(offset, extend);
    }

    /// Re-wraps if the text or width changed, then scrolls the caret into view
    /// if an edit asked for it. Call once per render, before building the
    /// element — [`Self::line_count`] is only meaningful afterwards.
    pub fn layout(&mut self, width: Pixels, font: gpui::Font, font_size: Pixels, window: &Window) {
        let text = self.editor.text();
        if self
            .wrapped_from
            .as_ref()
            .is_some_and(|(cached, cached_width)| cached == text && *cached_width == width)
        {
            self.reveal();
            return;
        }
        self.lines = wrap(text, width, font, font_size, window);
        self.wrapped_from = Some((text.to_owned(), width));
        self.reveal();
    }

    fn reveal(&mut self) {
        if !self.reveal_caret {
            return;
        }
        if let Some(line) = self.caret_line() {
            self.scroll.scroll_to_item(line);
            self.reveal_caret = false;
        }
    }

    /// The scroll container the rendered lines must be children of, so the
    /// handle's `scroll_to_item` indices line up with visual lines.
    pub fn scroll_handle(&self) -> &ScrollHandle {
        &self.scroll
    }

    /// One element per visual line, ready to be dropped into the scroll
    /// container. `caret` is drawn only when the field has focus.
    pub fn render_lines(
        &self,
        line_height: Pixels,
        caret: Option<&str>,
        selection_style: gpui::HighlightStyle,
    ) -> Vec<AnyElement> {
        if self.lines.is_empty() {
            return vec![div().h(line_height).into_any_element()];
        }
        let text = self.editor.text();
        let cursor = self.editor.cursor();
        let selection = self.editor.selection();
        let caret_line = self.caret_line();

        self.lines
            .iter()
            .enumerate()
            .map(|(index, line)| {
                let mut display = text[line.range.clone()].to_owned();
                let mut highlights = selection
                    .as_ref()
                    .and_then(|range| intersect(range, &line.range))
                    .map(|range| range.start - line.range.start..range.end - line.range.start);
                // The caret is spliced into the string rather than positioned,
                // matching the single-line fields; a real positioned caret
                // would need per-line text layout on every frame.
                if let (Some(caret), Some(caret_line)) = (caret, caret_line)
                    && caret_line == index
                    && selection.is_none()
                {
                    let at = cursor.saturating_sub(line.range.start).min(display.len());
                    display.insert_str(at, caret);
                    highlights = None;
                }
                let text: SharedString = display.into();
                let line = div().h(line_height).child(match highlights {
                    Some(range) => gpui::StyledText::new(text)
                        .with_highlights([(range, selection_style)])
                        .into_any_element(),
                    None => text.into_any_element(),
                });
                line.into_any_element()
            })
            .collect()
    }
}

/// Soft-wraps `text` at `width`, keeping hard line breaks as their own
/// boundaries. Always yields at least one line so an empty prompt still has a
/// caret row.
fn wrap(
    text: &str,
    width: Pixels,
    font: gpui::Font,
    font_size: Pixels,
    window: &Window,
) -> Vec<VisualLine> {
    let mut wrapper = window.text_system().line_wrapper(font, font_size);
    let mut lines = Vec::new();
    let mut paragraph_start = 0;
    // `split` (not `lines`) so a trailing newline yields the empty line after
    // it — the row the caret sits on when you have just pressed ⇧↵.
    for paragraph in text.split('\n') {
        let mut cut = paragraph_start;
        for boundary in
            wrapper.wrap_line(&[gpui::LineFragment::text(paragraph)], width.max(px(1.0)))
        {
            let at = paragraph_start + boundary.ix;
            if at > cut {
                lines.push(VisualLine {
                    range: cut..at,
                    soft_wrapped: true,
                });
                cut = at;
            }
        }
        lines.push(VisualLine {
            range: cut..paragraph_start + paragraph.len(),
            soft_wrapped: false,
        });
        paragraph_start += paragraph.len() + 1; // the '\n' itself
    }
    if lines.is_empty() {
        lines.push(VisualLine {
            range: 0..0,
            soft_wrapped: false,
        });
    }
    lines
}

fn grapheme_count(text: &str) -> usize {
    text.graphemes(true).count()
}

/// The byte offset `column` graphemes into `line`, clamped to its end.
fn offset_at_column(text: &str, line: &Range<usize>, column: usize) -> usize {
    text[line.clone()]
        .grapheme_indices(true)
        .nth(column)
        .map_or(line.end, |(index, _)| line.start + index)
}

fn intersect(left: &Range<usize>, right: &Range<usize>) -> Option<Range<usize>> {
    let start = left.start.max(right.start);
    let end = left.end.min(right.end);
    (start < end).then_some(start..end)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Wrapping needs a window, so these exercise the parts that do not:
    /// the line-relative arithmetic every caret operation rests on.
    fn lines(spans: &[(usize, usize, bool)]) -> Vec<VisualLine> {
        spans
            .iter()
            .map(|(start, end, soft)| VisualLine {
                range: *start..*end,
                soft_wrapped: *soft,
            })
            .collect()
    }

    fn composer(text: &str, spans: &[(usize, usize, bool)]) -> PromptComposer {
        let mut composer = PromptComposer::default();
        composer.editor.insert_multiline(text);
        composer.lines = lines(spans);
        composer
    }

    #[test]
    fn the_caret_belongs_to_the_last_line_that_starts_before_it() {
        // "abcdef" wrapped as "abc" / "def".
        let mut composer = composer("abcdef", &[(0, 3, true), (3, 6, false)]);
        assert_eq!(composer.caret_line(), Some(1)); // caret at 6, end of text
        composer.editor.move_to(3, false);
        // At a soft break the caret shows at the head of the wrapped line,
        // where the next character typed will appear.
        assert_eq!(composer.caret_line(), Some(1));
        composer.editor.move_to(1, false);
        assert_eq!(composer.caret_line(), Some(0));
    }

    #[test]
    fn vertical_motion_keeps_its_column_across_a_short_line() {
        // "abcdef" / "gh" / "ijklmn"
        let mut composer = composer(
            "abcdef\ngh\nijklmn",
            &[(0, 6, false), (7, 9, false), (10, 16, false)],
        );
        composer.editor.move_to(5, false); // column 5 of "abcdef"
        composer.move_down(false);
        assert_eq!(composer.editor.cursor(), 9); // "gh" has no column 5
        composer.move_down(false);
        assert_eq!(composer.editor.cursor(), 15); // column 5 of "ijklmn"
        composer.move_up(false);
        assert_eq!(composer.editor.cursor(), 9);
        composer.move_up(false);
        assert_eq!(composer.editor.cursor(), 5); // back where it started
    }

    #[test]
    fn line_granular_edits_stop_at_the_visual_line() {
        let mut composer = composer("first\nsecond", &[(0, 5, false), (6, 12, false)]);
        composer.editor.move_to(12, false);
        composer.apply(LocalEdit::DeleteBackward(Motion::Line));
        // Only the caret's own line is gone, not the whole prompt.
        assert_eq!(composer.editor.text(), "first\n");
    }

    #[test]
    fn line_granular_motion_stops_at_the_visual_line() {
        let mut composer = composer("first\nsecond", &[(0, 5, false), (6, 12, false)]);
        composer.editor.move_to(9, false);
        composer.apply(LocalEdit::MoveLeft(Motion::Line, false));
        assert_eq!(composer.editor.cursor(), 6); // start of "second", not 0
        composer.apply(LocalEdit::MoveRight(Motion::Line, false));
        assert_eq!(composer.editor.cursor(), 12);
    }

    #[test]
    fn moving_past_the_last_line_parks_at_the_end_of_the_buffer() {
        let mut composer = composer("ab\ncd", &[(0, 2, false), (3, 5, false)]);
        composer.editor.move_to(4, false);
        composer.move_down(false);
        assert_eq!(composer.editor.cursor(), 5);
        composer.move_up(false);
        composer.move_up(false);
        assert_eq!(composer.editor.cursor(), 0);
    }

    #[test]
    fn a_selection_is_clipped_to_each_line_it_crosses() {
        assert_eq!(intersect(&(2..9), &(0..5)), Some(2..5));
        assert_eq!(intersect(&(2..9), &(5..12)), Some(5..9));
        assert_eq!(intersect(&(2..9), &(12..14)), None);
    }

    #[test]
    fn undo_walks_back_through_typing_line_edits_and_cut_with_the_selection() {
        let mut composer = PromptComposer::default();
        for character in "first".chars() {
            composer.apply(LocalEdit::Insert(character.to_string()));
        }
        composer.insert_multiline("\n"); // ⇧↵
        for character in "second".chars() {
            composer.apply(LocalEdit::Insert(character.to_string()));
        }
        composer.lines = lines(&[(0, 5, false), (6, 12, false)]);
        composer.apply(LocalEdit::DeleteBackward(Motion::Line));
        assert_eq!(composer.text(), "first\n");

        assert!(composer.undo());
        assert_eq!(composer.text(), "first\nsecond");
        assert_eq!(composer.editor.cursor(), 12);
        assert!(composer.undo());
        assert_eq!(composer.text(), "first\n");
        assert!(composer.undo());
        assert_eq!(composer.text(), "first");
        assert!(composer.redo());
        assert!(composer.redo());
        assert_eq!(composer.text(), "first\nsecond");

        // Select-all + Backspace is the accident the history exists for.
        composer.apply(LocalEdit::SelectAll);
        composer.apply(LocalEdit::DeleteBackward(Motion::Character));
        assert!(composer.is_empty());
        assert!(composer.undo());
        assert_eq!(composer.text(), "first\nsecond");
        assert_eq!(composer.editor.selected_text(), Some("first\nsecond"));
    }

    #[test]
    fn vertical_motion_ends_a_typing_run() {
        let mut composer = composer("ab\ncd", &[(0, 2, false), (3, 5, false)]);
        composer.apply(LocalEdit::Insert("e".into()));
        composer.move_up(false);
        composer.move_down(false);
        composer.editor.move_to(6, false);
        composer.apply(LocalEdit::Insert("f".into()));
        assert!(composer.undo());
        assert_eq!(composer.text(), "ab\ncde");
    }

    #[test]
    fn loading_another_draft_starts_a_history_of_its_own() {
        let mut composer = PromptComposer::default();
        composer.insert_multiline("draft for session one");
        composer.reset("draft for session two");
        assert!(!composer.undo(), "loading a draft is not an edit");
        composer.apply(LocalEdit::Insert("!".into()));
        assert!(composer.undo());
        assert_eq!(composer.text(), "draft for session two");
        assert!(!composer.undo(), "session one's text is out of reach");
        assert_eq!(composer.text(), "draft for session two");
    }

    #[test]
    fn staged_context_appends_without_replacing_the_existing_draft() {
        let mut composer = PromptComposer::default();
        composer.insert_multiline("Review the parser first");
        composer.append_context("'/tmp/a file.rs' '/tmp/tests'");
        assert_eq!(
            composer.text(),
            "Review the parser first\n'/tmp/a file.rs' '/tmp/tests'"
        );

        composer.append_context("'/tmp/more.rs'");
        assert_eq!(
            composer.text(),
            "Review the parser first\n'/tmp/a file.rs' '/tmp/tests'\n'/tmp/more.rs'"
        );
    }

    #[test]
    fn staged_context_uses_existing_trailing_whitespace() {
        let mut composer = PromptComposer::default();
        composer.insert_multiline("Look here: ");
        composer.append_context("'/tmp/example.rs'");
        assert_eq!(composer.text(), "Look here: '/tmp/example.rs'");
    }

    #[test]
    fn staged_context_preserves_tabs_and_normalizes_crlf() {
        let mut composer = PromptComposer::default();
        composer.append_context("first\r\n\tindented\rnext");
        assert_eq!(composer.text(), "first\n\tindented\nnext");
    }
}
