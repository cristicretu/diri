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

use gpui::{
    AnyElement, Bounds, HighlightStyle, Pixels, Point, ScrollHandle, ShapedLine, SharedString,
    TextRun, UTF16Selection, UnderlineStyle, Window, div, point, prelude::*, px, size,
};
use unicode_segmentation::UnicodeSegmentation;

use crate::query_editor::{self, EditHistory, EditKind, LocalEdit, Motion, QueryEditor};
use crate::text_input::{Composition, byte_range, utf16_range};

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
    /// Marked (IME preedit) text. It lives in `editor` while it is being
    /// composed so it wraps and scrolls like any other text, but it is not
    /// part of the draft until the input method commits it.
    composition: Composition,
    scroll: ScrollHandle,
    lines: Vec<VisualLine>,
    /// The (text, width) the cached `lines` were computed from.
    wrapped_from: Option<(String, Pixels)>,
    /// The font the lines were wrapped in, which is the one they are drawn
    /// in. A pointer is resolved against lines shaped with it again.
    font: Option<(gpui::Font, Pixels)>,
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
            composition: Composition::multiline(),
            scroll: ScrollHandle::new(),
            lines: Vec::new(),
            wrapped_from: None,
            font: None,
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
        self.composition.cancel(&mut self.editor);
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
        self.finish_composition();
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
        self.finish_composition();
        self.goal_column = None;
        self.reveal_caret = true;
        self.history.undo(&mut self.editor)
    }

    /// ⇧⌘Z.
    pub fn redo(&mut self) -> bool {
        self.finish_composition();
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

    pub fn is_composing(&self) -> bool {
        self.composition.is_composing()
    }

    /// Changes whenever a composition is cancelled from this side. An input
    /// handler carries the epoch it was registered under, so a native
    /// callback that arrives late cannot write into a draft that moved on.
    pub fn composition_epoch(&self) -> u64 {
        self.composition.epoch
    }

    /// Text from the input method. `range` and `selected` count UTF-16 units,
    /// as Cocoa does. While `composing`, the text is marked: it is drawn
    /// underlined and replaced wholesale by the next update. Otherwise it is
    /// committed, and everything since the composition began becomes one
    /// undo step that continues a typing run the way a typed key would.
    pub fn replace_from_input(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        selected: Option<Range<usize>>,
        composing: bool,
    ) {
        let before = self
            .composition
            .original()
            .cloned()
            .unwrap_or_else(|| self.editor.clone());
        self.composition
            .replace(&mut self.editor, range, text, selected, composing);
        if !self.composition.is_composing() {
            self.commit_input(before);
        }
        self.goal_column = None;
        self.reveal_caret = true;
    }

    /// Accepts the marked text as it stands. Any edit that does not come from
    /// the input method does this first: a marked range cannot survive the
    /// text moving underneath it.
    pub fn finish_composition(&mut self) {
        if let Some(before) = self.composition.original().cloned() {
            self.composition.finish();
            self.commit_input(before);
        }
    }

    /// Drops the marked text and restores the draft from before it, as when
    /// focus leaves mid-composition. Returns whether there was one.
    pub fn cancel_composition(&mut self) -> bool {
        let composing = self.composition.is_composing();
        if composing {
            self.composition.cancel(&mut self.editor);
            self.goal_column = None;
            self.reveal_caret = true;
        }
        composing
    }

    fn commit_input(&mut self, before: QueryEditor) {
        if self.editor.text() == before.text() {
            self.history.break_run();
        } else {
            self.history.record(EditKind::Typing, before, &self.editor);
        }
    }

    /// The selection as Cocoa asks for it.
    pub fn utf16_selection(&self) -> UTF16Selection {
        let cursor = self.editor.cursor();
        let range = self.editor.selection().unwrap_or(cursor..cursor);
        UTF16Selection {
            reversed: cursor == range.start && !range.is_empty(),
            range: utf16_range(self.editor.text(), range),
        }
    }

    pub fn utf16_marked_range(&self) -> Option<Range<usize>> {
        self.composition
            .marked()
            .map(|range| utf16_range(self.editor.text(), range))
    }

    /// The text in a UTF-16 range, and the range it was actually taken from
    /// once widened to whole characters.
    pub fn text_for_utf16_range(&self, range: Range<usize>) -> (String, Range<usize>) {
        let text = self.editor.text();
        let range = byte_range(text, range);
        (text[range.clone()].to_owned(), utf16_range(text, range))
    }

    /// Where a UTF-16 range is drawn, in window coordinates, for the input
    /// method's candidate window. `origin` is where the first visual line
    /// starts when nothing is scrolled. A range that runs past its first row
    /// is reported up to that row's end, which is where candidates belong.
    ///
    /// AppKit asks straight after handing over marked text, before any frame
    /// has wrapped it, so the lines are brought up to date first.
    pub fn bounds_for_utf16_range(
        &mut self,
        range: Range<usize>,
        origin: Point<Pixels>,
        line_height: Pixels,
        caret: Option<&str>,
        window: &Window,
    ) -> Option<Bounds<Pixels>> {
        if let (Some((_, width)), Some((font, font_size))) = (&self.wrapped_from, &self.font) {
            self.layout(*width, font.clone(), *font_size, window);
        }
        let range = byte_range(self.editor.text(), range);
        let row = self
            .lines
            .iter()
            .rposition(|line| line.range.start <= range.start)
            .unwrap_or(0);
        // An empty draft draws its placeholder instead of the scrolled lines,
        // and the handle may still hold the offset of the text before it.
        let scrolled = if self.editor.is_empty() {
            px(0.0)
        } else {
            self.scroll.offset().y
        };
        let top = origin.y + line_height * row as f32 + scrolled;
        let Some(line) = self.lines.get(row) else {
            // Nothing laid out yet: an empty draft, whose text starts at the
            // origin.
            return Some(Bounds::new(origin, size(px(1.0), line_height)));
        };
        let (shaped, splice) = self.shape_row(row, caret, window)?;
        let x = |offset: usize| {
            let local = offset.clamp(line.range.start, line.range.end) - line.range.start;
            shaped.x_for_index(display_index(local, splice))
        };
        let (left, right) = (x(range.start), x(range.end));
        Some(Bounds::new(
            point(origin.x + left, top),
            size((right - left).max(px(1.0)), line_height),
        ))
    }

    /// Visual line `row` shaped the way it was drawn, plus the caret glyph
    /// `(at, len)` spliced into it, if any.
    fn shape_row(
        &self,
        row: usize,
        caret: Option<&str>,
        window: &Window,
    ) -> Option<(ShapedLine, Option<(usize, usize)>)> {
        let (font, font_size) = self.font.clone()?;
        let line = self.lines.get(row)?;
        let splice = self.caret_splice(row, caret);
        let mut display = self.editor.text()[line.range.clone()].to_owned();
        if let (Some(at), Some(caret)) = (splice, caret) {
            display.insert_str(at, caret);
        }
        let run = TextRun {
            len: display.len(),
            font,
            ..TextRun::default()
        };
        let shaped = window
            .text_system()
            .shape_line(display.into(), font_size, &[run], None);
        Some((shaped, splice.zip(caret.map(str::len))))
    }

    /// Where a pointer lands in the buffer. Resolved against the visual lines
    /// the last render drew — the same ranges, the same font, the caret glyph
    /// spliced in where it was drawn — and against the scroll handle those
    /// lines are children of, so a scrolled field answers for the rows that
    /// are actually under the pointer. `None` before the first layout.
    pub fn hit_test(
        &self,
        position: Point<Pixels>,
        line_height: Pixels,
        caret: Option<&str>,
        window: &Window,
    ) -> Option<PointerHit> {
        if self.editor.is_empty() {
            // The placeholder is drawn instead of the lines; there is only
            // one place to be.
            return Some(PointerHit::default());
        }
        let bounds = self.scroll.bounds();
        let row = row_at(
            position.y,
            bounds.top(),
            self.scroll.offset().y,
            line_height,
            self.lines.len(),
        )?;
        let line = &self.lines[row];
        let (shaped, splice) = self.shape_row(row, caret, window)?;
        let x = position.x - bounds.left();
        let text = self.editor.text();
        // Past the end of a row there is no character under the pointer; the
        // row's last one stands in, so a double-click there takes the last
        // word instead of the line break.
        let character = shaped.index_for_x(x).map_or_else(
            || {
                text[line.range.clone()]
                    .grapheme_indices(true)
                    .next_back()
                    .map_or(0, |(index, _)| index)
            },
            |index| buffer_index(index, splice).min(line.range.len()),
        );
        Some(PointerHit {
            caret: caret_offset(text, line, shaped.closest_index_for_x(x), splice),
            character: line.range.start + character,
        })
    }

    /// A press. One click places the caret (⇧ extends the selection from
    /// where it was anchored); a second selects the word under the pointer.
    pub fn pointer_down(&mut self, hit: PointerHit, extend: bool, click_count: usize) {
        self.finish_composition();
        if click_count >= 2 {
            self.editor.select_word_at(hit.character);
        } else {
            self.editor.set_cursor(hit.caret, extend);
        }
        self.pointer_moved();
    }

    /// A drag with the button held: the anchor stays where the press put it.
    pub fn pointer_drag(&mut self, hit: PointerHit) {
        self.finish_composition();
        self.editor.set_cursor(hit.caret, true);
        self.pointer_moved();
    }

    fn pointer_moved(&mut self) {
        self.history.break_run();
        self.goal_column = None;
        self.reveal_caret = true;
    }

    /// Where in visual line `index` the caret glyph is spliced, as a byte
    /// offset into that line. `None` when the caret is elsewhere, hidden by a
    /// selection, or not drawn at all because the field has no focus.
    fn caret_splice(&self, index: usize, caret: Option<&str>) -> Option<usize> {
        caret?;
        if self.editor.selection().is_some() || self.caret_line() != Some(index) {
            return None;
        }
        let line = &self.lines[index].range;
        Some(
            self.editor
                .cursor()
                .saturating_sub(line.start)
                .min(line.len()),
        )
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
        self.finish_composition();
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
        self.font = Some((font.clone(), font_size));
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
        selection_style: HighlightStyle,
    ) -> Vec<AnyElement> {
        if self.lines.is_empty() {
            return vec![div().h(line_height).into_any_element()];
        }
        let text = self.editor.text();
        let selection = self.editor.selection();
        // Marked text is underlined, the way every macOS field shows that
        // the input method still owns it.
        let marked = self.composition.marked();
        let marked_style = HighlightStyle {
            underline: Some(UnderlineStyle {
                thickness: px(1.0),
                ..UnderlineStyle::default()
            }),
            ..HighlightStyle::default()
        };

        self.lines
            .iter()
            .enumerate()
            .map(|(index, line)| {
                let mut display = text[line.range.clone()].to_owned();
                // The caret is spliced into the string rather than positioned,
                // matching the single-line fields; a real positioned caret
                // would need per-line text layout on every frame.
                let splice = self.caret_splice(index, caret);
                if let (Some(caret), Some(at)) = (caret, splice) {
                    display.insert_str(at, caret);
                }
                let splice = splice.zip(caret.map(str::len));
                let drawn = |range: &Option<Range<usize>>| {
                    range
                        .as_ref()
                        .and_then(|range| intersect(range, &line.range))
                        .map(|range| {
                            display_index(range.start - line.range.start, splice)
                                ..display_index(range.end - line.range.start, splice)
                        })
                };
                let highlights: Vec<_> = gpui::combine_highlights(
                    drawn(&selection).map(|range| (range, selection_style)),
                    drawn(&marked).map(|range| (range, marked_style)),
                )
                .collect();
                let text: SharedString = display.into();
                let line = div().h(line_height).child(if highlights.is_empty() {
                    text.into_any_element()
                } else {
                    gpui::StyledText::new(text)
                        .with_highlights(highlights)
                        .into_any_element()
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

/// What a pointer position resolves to.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PointerHit {
    /// The nearest place a caret can be: what a click or a drag moves to.
    pub caret: usize,
    /// The start of the character under the pointer: what a double-click
    /// asks about. The nearest caret position would pick the neighbouring
    /// word from the right half of a word's last letter.
    pub character: usize,
}

/// The visual row under a pointer at `y`. `top` is where the scroll container
/// starts and `scroll_y` its offset, which GPUI counts negative once content
/// has moved up. A pointer above or below the text takes the first or last
/// row, so dragging out of the field keeps selecting towards that edge.
fn row_at(
    y: Pixels,
    top: Pixels,
    scroll_y: Pixels,
    line_height: Pixels,
    rows: usize,
) -> Option<usize> {
    let last = rows.checked_sub(1)?;
    let row = ((y - top - scroll_y) / line_height).floor().max(0.0) as usize;
    Some(row.min(last))
}

/// Where byte `local` of a line's own text ends up once the caret glyph
/// `(at, len)` has been spliced into the drawn line. The inverse of
/// [`buffer_index`].
fn display_index(local: usize, splice: Option<(usize, usize)>) -> usize {
    match splice {
        Some((at, len)) if local > at => local + len,
        _ => local,
    }
}

/// Maps an index into a DRAWN line back into the line's own text by taking
/// out the caret glyph `(at, len)` that was spliced into it. A position
/// inside the glyph is the caret's own position.
fn buffer_index(display_index: usize, splice: Option<(usize, usize)>) -> usize {
    match splice {
        Some((at, len)) if display_index >= at + len => display_index - len,
        Some((at, _)) if display_index > at => at,
        _ => display_index,
    }
}

/// The buffer offset for a caret placed at `display_index` of `line`. At the
/// end of a soft-wrapped line the offset would equal the next line's start,
/// which is where [`PromptComposer::caret_line`] draws it; stopping one
/// grapheme short keeps the caret on the row that was clicked.
fn caret_offset(
    text: &str,
    line: &VisualLine,
    display_index: usize,
    splice: Option<(usize, usize)>,
) -> usize {
    let local = buffer_index(display_index, splice).min(line.range.len());
    let offset = line.range.start + local;
    if line.soft_wrapped && offset == line.range.end {
        return text[line.range.clone()]
            .grapheme_indices(true)
            .next_back()
            .map_or(offset, |(index, _)| line.range.start + index);
    }
    offset
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
    fn the_row_under_the_pointer_accounts_for_scroll_and_clamps_to_the_text() {
        let (top, height) = (px(100.0), px(19.0));
        let row = |y: f32, scroll: f32| row_at(px(y), top, px(scroll), height, 12);
        assert_eq!(row(100.0, 0.0), Some(0));
        assert_eq!(row(118.9, 0.0), Some(0));
        assert_eq!(row(119.0, 0.0), Some(1));
        // Three rows scrolled away: the same pixel is now the fourth row.
        assert_eq!(row(100.0, -57.0), Some(3));
        assert_eq!(row(110.0, -66.5), Some(4));
        // Out of the field: the nearest edge row, not a panic or a wrap.
        assert_eq!(row(20.0, -57.0), Some(0));
        assert_eq!(row(4000.0, -57.0), Some(11));
        assert_eq!(row_at(px(100.0), top, px(0.0), height, 0), None);
    }

    #[test]
    fn a_drawn_index_maps_back_past_the_spliced_caret_glyph() {
        let caret = Some((3, "▏".len()));
        assert_eq!(buffer_index(2, caret), 2);
        assert_eq!(buffer_index(3, caret), 3);
        assert_eq!(buffer_index(3 + "▏".len(), caret), 3);
        assert_eq!(buffer_index(4 + "▏".len(), caret), 4);
        assert_eq!(buffer_index(7, None), 7);
    }

    #[test]
    fn a_click_past_a_soft_wrap_stays_on_the_clicked_row_and_on_a_boundary() {
        // "ab界" wrapped before "cd"; the first row ends in a 3-byte character.
        let text = "ab界cd\nef";
        let rows = lines(&[(0, 5, true), (5, 7, false), (8, 10, false)]);
        assert_eq!(caret_offset(text, &rows[0], 5, None), 2);
        assert_eq!(caret_offset(text, &rows[0], 2, None), 2);
        // A hard break has a real end-of-line position.
        assert_eq!(caret_offset(text, &rows[1], 2, None), 7);
        assert_eq!(caret_offset(text, &rows[2], 99, None), 10);
    }

    #[test]
    fn click_shift_click_drag_and_double_click_drive_the_selection() {
        let mut composer = composer("fix naïve 界 parser", &[(0, 21, false)]);
        let hit = |caret: usize, character: usize| PointerHit { caret, character };
        composer.pointer_down(hit(4, 4), false, 1);
        assert_eq!(composer.editor.cursor(), 4);
        assert_eq!(composer.editor.selection(), None);
        composer.pointer_down(hit(10, 9), true, 1);
        assert_eq!(composer.editor.selected_text(), Some("naïve"));
        // A drag keeps the anchor of the press, in either direction.
        composer.pointer_down(hit(11, 11), false, 1);
        composer.pointer_drag(hit(14, 11));
        assert_eq!(composer.editor.selected_text(), Some("界"));
        composer.pointer_drag(hit(4, 4));
        assert_eq!(composer.editor.selected_text(), Some("naïve "));
        // The nearest caret position is after "naïve"; the character under
        // the pointer is still its last letter.
        composer.pointer_down(hit(10, 9), false, 2);
        assert_eq!(composer.editor.selected_text(), Some("naïve"));
        // An offset inside a multibyte character cannot split it.
        composer.pointer_down(hit(12, 12), false, 1);
        assert_eq!(composer.editor.cursor(), 11);
    }

    #[test]
    fn a_click_between_two_typed_characters_ends_the_typing_run() {
        let mut composer = PromptComposer::default();
        composer.apply(LocalEdit::Insert("a".into()));
        composer.pointer_down(
            PointerHit {
                caret: 1,
                character: 0,
            },
            false,
            1,
        );
        composer.apply(LocalEdit::Insert("b".into()));
        assert!(composer.undo());
        assert_eq!(composer.text(), "a");
    }

    #[test]
    fn a_composition_is_marked_until_committed_and_then_is_one_undo_step() {
        let mut composer = PromptComposer::default();
        composer.apply(LocalEdit::Insert("a".into()));
        composer.apply(LocalEdit::Insert("😀".into()));
        composer.replace_from_input(None, "ni", Some(2..2), true);
        assert_eq!(composer.text(), "a😀ni");
        assert!(composer.is_composing());
        // Cocoa counts UTF-16 units: the emoji before the marked text is two.
        assert_eq!(composer.utf16_marked_range(), Some(3..5));
        assert_eq!(composer.utf16_selection().range, 5..5);
        composer.replace_from_input(None, "你", Some(1..1), true);
        assert_eq!(composer.text(), "a😀你");
        composer.replace_from_input(None, "你好", None, false);
        assert_eq!(composer.text(), "a😀你好", "committed exactly once");
        assert!(!composer.is_composing());
        assert_eq!(composer.utf16_marked_range(), None);

        // Committed text continues the typing run it landed in, and no
        // intermediate preedit is ever an undo step.
        assert!(composer.undo());
        assert_eq!(composer.text(), "");
        assert!(composer.redo());
        assert_eq!(composer.text(), "a😀你好");

        // A replacement range from the input method (press-and-hold accents,
        // reconversion) is UTF-16 too, and is its own way back.
        composer.pointer_down(PointerHit::default(), false, 1);
        composer.replace_from_input(Some(1..3), "é", None, false);
        assert_eq!(composer.text(), "aé你好");
        assert!(composer.undo());
        assert_eq!(composer.text(), "a😀你好");
    }

    #[test]
    fn a_cancelled_composition_leaves_no_trace_and_an_edit_accepts_it_as_typed() {
        let mut composer = PromptComposer::default();
        composer.insert_multiline("draft ");
        composer.apply(LocalEdit::SelectAll);
        let epoch = composer.composition_epoch();
        composer.replace_from_input(None, "k", Some(1..1), true);
        assert_eq!(composer.text(), "k");
        assert!(composer.cancel_composition());
        assert_eq!(composer.text(), "draft ");
        assert_eq!(composer.editor.selected_text(), Some("draft "));
        assert_ne!(composer.composition_epoch(), epoch);
        assert!(!composer.cancel_composition());
        assert!(composer.undo(), "the cancelled preedit recorded nothing");
        assert_eq!(composer.text(), "");
        assert!(composer.redo());

        // ⌘A, ⌘V, ⌘Z and the pointer all act on real text: whatever is
        // marked when they arrive is accepted first, as one step.
        composer.apply(LocalEdit::MoveRight(Motion::Character, false));
        composer.replace_from_input(None, "ka", Some(2..2), true);
        composer.apply(LocalEdit::SelectAll);
        assert!(!composer.is_composing());
        assert_eq!(composer.editor.selected_text(), Some("draft ka"));
        assert!(composer.undo());
        assert_eq!(composer.text(), "draft ");
    }

    #[test]
    fn text_for_a_utf16_range_widens_to_whole_characters() {
        let composer = composer("a😀界", &[(0, 8, false)]);
        assert_eq!(
            composer.text_for_utf16_range(2..4),
            ("😀界".to_owned(), 1..4)
        );
        assert_eq!(
            composer.text_for_utf16_range(0..99),
            ("a😀界".to_owned(), 0..4)
        );
    }

    #[test]
    fn highlights_step_over_the_spliced_caret_glyph() {
        let caret = Some((2, 3));
        assert_eq!(display_index(1, caret), 1);
        assert_eq!(display_index(2, caret), 2);
        assert_eq!(display_index(3, caret), 6);
        for local in 0..6 {
            assert_eq!(buffer_index(display_index(local, caret), caret), local);
        }
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
