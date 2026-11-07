// Adapted from Ely GPUI Components (MIT OR Apache-2.0), https://github.com/ZacharyZhang-NY/Ely-GPUI-Components

//! Keys, the pointer, and the platform input method.

use std::ops::Range;

use gpui::{
    Bounds, Context, EntityInputHandler, InteractiveElement, KeyBinding, KeyDownEvent,
    MouseDownEvent, MouseMoveEvent, Pixels, Point, UTF16Selection, Window, actions, point,
};

use super::buffer::Buffer;
use super::cursor::{Motion, Selection};
use super::editor::{CodeEditor, PAGE, Row};
use super::syntax;
use crate::query_editor::{self, ClipboardEdit, Edit};

actions!(
    diri_code_editor,
    [
        Left,
        Right,
        Up,
        Down,
        SelectLeft,
        SelectRight,
        SelectUp,
        SelectDown,
        WordLeft,
        WordRight,
        SelectWordLeft,
        SelectWordRight,
        LineStart,
        LineEnd,
        SelectLineStart,
        SelectLineEnd,
        DocumentStart,
        DocumentEnd,
        SelectDocumentStart,
        SelectDocumentEnd,
        PageUp,
        PageDown,
        Backspace,
        Delete,
        DeleteWordLeft,
        DeleteWordRight,
        DeleteLineLeft,
        Newline,
        Indent,
        Outdent,
        Undo,
        Redo,
        SelectAll,
        Copy,
        Cut,
        Paste,
        AddCursorAbove,
        AddCursorBelow,
        SelectNext,
        SelectAllOccurrences,
        Escape,
        ToggleComment,
        Fold,
        Unfold,
        FoldAll,
        UnfoldAll,
        MoveLineUp,
        MoveLineDown,
        DuplicateLine,
        DeleteLine,
        Find,
        FindReplace,
        FindNext,
        FindPrevious,
        Save,
        GoToDefinition,
        TriggerCompletion,
        TriggerSignature,
    ]
);

pub(crate) const EDITOR_CONTEXT: &str = "DiriCodeEditor";

/// The editor's keymap. Bound in its own key context, so none of these
/// shadow terminal or app shortcuts unless the editor holds focus.
pub(crate) fn key_bindings() -> Vec<KeyBinding> {
    let c = Some(EDITOR_CONTEXT);
    vec![
        KeyBinding::new("left", Left, c),
        KeyBinding::new("right", Right, c),
        KeyBinding::new("up", Up, c),
        KeyBinding::new("down", Down, c),
        KeyBinding::new("shift-left", SelectLeft, c),
        KeyBinding::new("shift-right", SelectRight, c),
        KeyBinding::new("shift-up", SelectUp, c),
        KeyBinding::new("shift-down", SelectDown, c),
        KeyBinding::new("alt-left", WordLeft, c),
        KeyBinding::new("alt-right", WordRight, c),
        KeyBinding::new("alt-shift-left", SelectWordLeft, c),
        KeyBinding::new("alt-shift-right", SelectWordRight, c),
        KeyBinding::new("cmd-left", LineStart, c),
        KeyBinding::new("cmd-right", LineEnd, c),
        KeyBinding::new("home", LineStart, c),
        KeyBinding::new("end", LineEnd, c),
        KeyBinding::new("ctrl-a", LineStart, c),
        KeyBinding::new("ctrl-e", LineEnd, c),
        KeyBinding::new("cmd-shift-left", SelectLineStart, c),
        KeyBinding::new("cmd-shift-right", SelectLineEnd, c),
        KeyBinding::new("shift-home", SelectLineStart, c),
        KeyBinding::new("shift-end", SelectLineEnd, c),
        KeyBinding::new("cmd-up", DocumentStart, c),
        KeyBinding::new("cmd-down", DocumentEnd, c),
        KeyBinding::new("cmd-shift-up", SelectDocumentStart, c),
        KeyBinding::new("cmd-shift-down", SelectDocumentEnd, c),
        KeyBinding::new("pageup", PageUp, c),
        KeyBinding::new("pagedown", PageDown, c),
        KeyBinding::new("backspace", Backspace, c),
        KeyBinding::new("shift-backspace", Backspace, c),
        KeyBinding::new("delete", Delete, c),
        KeyBinding::new("alt-backspace", DeleteWordLeft, c),
        KeyBinding::new("alt-delete", DeleteWordRight, c),
        KeyBinding::new("cmd-backspace", DeleteLineLeft, c),
        KeyBinding::new("enter", Newline, c),
        KeyBinding::new("shift-enter", Newline, c),
        KeyBinding::new("tab", Indent, c),
        KeyBinding::new("shift-tab", Outdent, c),
        KeyBinding::new("cmd-]", Indent, c),
        KeyBinding::new("cmd-[", Outdent, c),
        KeyBinding::new("cmd-z", Undo, c),
        KeyBinding::new("cmd-shift-z", Redo, c),
        KeyBinding::new("cmd-a", SelectAll, c),
        KeyBinding::new("cmd-c", Copy, c),
        KeyBinding::new("cmd-x", Cut, c),
        KeyBinding::new("cmd-v", Paste, c),
        KeyBinding::new("cmd-alt-up", AddCursorAbove, c),
        KeyBinding::new("cmd-alt-down", AddCursorBelow, c),
        KeyBinding::new("cmd-d", SelectNext, c),
        KeyBinding::new("cmd-shift-l", SelectAllOccurrences, c),
        KeyBinding::new("escape", Escape, c),
        KeyBinding::new("cmd-/", ToggleComment, c),
        KeyBinding::new("cmd-alt-[", Fold, c),
        KeyBinding::new("cmd-alt-]", Unfold, c),
        KeyBinding::new("cmd-alt-shift-[", FoldAll, c),
        KeyBinding::new("cmd-alt-shift-]", UnfoldAll, c),
        KeyBinding::new("alt-up", MoveLineUp, c),
        KeyBinding::new("alt-down", MoveLineDown, c),
        KeyBinding::new("alt-shift-down", DuplicateLine, c),
        KeyBinding::new("cmd-shift-k", DeleteLine, c),
        KeyBinding::new("cmd-f", Find, c),
        KeyBinding::new("cmd-alt-f", FindReplace, c),
        KeyBinding::new("cmd-g", FindNext, c),
        KeyBinding::new("cmd-shift-g", FindPrevious, c),
        KeyBinding::new("cmd-s", Save, c),
        KeyBinding::new("f12", GoToDefinition, c),
        KeyBinding::new("ctrl-space", TriggerCompletion, c),
        KeyBinding::new("cmd-shift-space", TriggerSignature, c),
    ]
}

fn line_left(buffer: &Buffer, at: usize) -> Range<usize> {
    let start = buffer.line_range(buffer.line_of(at)).start;
    if at == start {
        buffer.previous(at)..at
    } else {
        start..at
    }
}

/// Wires every action to the editor that renders `root`.
pub(crate) fn listen<E: InteractiveElement>(root: E, cx: &mut Context<CodeEditor>) -> E {
    macro_rules! on {
        ($root:expr, $action:ty, $body:expr) => {
            $root.on_action(cx.listener(
                move |editor: &mut CodeEditor,
                      _: &$action,
                      window: &mut Window,
                      cx: &mut Context<CodeEditor>| {
                    let run: fn(&mut CodeEditor, &mut Window, &mut Context<CodeEditor>) = $body;
                    run(editor, window, cx)
                },
            ))
        };
    }
    let root = on!(root, Left, |e, _, cx| e.motion(Motion::Left, false, cx));
    let root = on!(root, Right, |e, _, cx| e.motion(Motion::Right, false, cx));
    let root = on!(root, Up, |e, _, cx| {
        if !e.move_completion(-1, cx) {
            e.motion(Motion::Up, false, cx)
        }
    });
    let root = on!(root, Down, |e, _, cx| {
        if !e.move_completion(1, cx) {
            e.motion(Motion::Down, false, cx)
        }
    });
    let root = on!(root, SelectLeft, |e, _, cx| e.motion(
        Motion::Left,
        true,
        cx
    ));
    let root = on!(root, SelectRight, |e, _, cx| e.motion(
        Motion::Right,
        true,
        cx
    ));
    let root = on!(root, SelectUp, |e, _, cx| e.motion(Motion::Up, true, cx));
    let root = on!(root, SelectDown, |e, _, cx| e.motion(
        Motion::Down,
        true,
        cx
    ));
    let root = on!(root, WordLeft, |e, _, cx| e.motion(
        Motion::WordLeft,
        false,
        cx
    ));
    let root = on!(root, WordRight, |e, _, cx| e.motion(
        Motion::WordRight,
        false,
        cx
    ));
    let root = on!(root, SelectWordLeft, |e, _, cx| e.motion(
        Motion::WordLeft,
        true,
        cx
    ));
    let root = on!(root, SelectWordRight, |e, _, cx| e.motion(
        Motion::WordRight,
        true,
        cx
    ));
    let root = on!(root, LineStart, |e, _, cx| e.motion(
        Motion::LineStart,
        false,
        cx
    ));
    let root = on!(root, LineEnd, |e, _, cx| e.motion(
        Motion::LineEnd,
        false,
        cx
    ));
    let root = on!(root, SelectLineStart, |e, _, cx| e.motion(
        Motion::LineStart,
        true,
        cx
    ));
    let root = on!(root, SelectLineEnd, |e, _, cx| e.motion(
        Motion::LineEnd,
        true,
        cx
    ));
    let root = on!(root, DocumentStart, |e, _, cx| e.motion(
        Motion::Start,
        false,
        cx
    ));
    let root = on!(root, DocumentEnd, |e, _, cx| e.motion(
        Motion::End,
        false,
        cx
    ));
    let root = on!(root, SelectDocumentStart, |e, _, cx| e.motion(
        Motion::Start,
        true,
        cx
    ));
    let root = on!(root, SelectDocumentEnd, |e, _, cx| e.motion(
        Motion::End,
        true,
        cx
    ));
    let root = on!(root, PageUp, |e, _, cx| e.motion(
        Motion::Page(-PAGE),
        false,
        cx
    ));
    let root = on!(root, PageDown, |e, _, cx| e.motion(
        Motion::Page(PAGE),
        false,
        cx
    ));
    let root = on!(root, Backspace, |e, _, cx| e.backspace(cx));
    let root = on!(root, Delete, |e, _, cx| e
        .delete_by(|buffer, at| at..buffer.next(at), cx));
    let root = on!(root, DeleteWordLeft, |e, _, cx| {
        e.delete_by(|buffer, at| buffer.word_start(at)..at, cx)
    });
    let root = on!(root, DeleteWordRight, |e, _, cx| {
        e.delete_by(|buffer, at| at..buffer.word_end(at), cx)
    });
    let root = on!(root, DeleteLineLeft, |e, _, cx| e.delete_by(line_left, cx));
    let root = on!(root, Newline, |e, _, cx| {
        if !e.accept_completion(cx) {
            e.newline(cx)
        }
    });
    let root = on!(root, Indent, |e, _, cx| e.indent(cx));
    let root = on!(root, Outdent, |e, _, cx| e.outdent(cx));
    let root = on!(root, Undo, |e, _, cx| e.undo(cx));
    let root = on!(root, Redo, |e, _, cx| e.redo(cx));
    let root = on!(root, SelectAll, |e, _, cx| e.select_all(cx));
    let root = on!(root, Copy, |e, _, cx| e.copy(cx));
    let root = on!(root, Cut, |e, _, cx| e.cut(cx));
    let root = on!(root, Paste, |e, _, cx| {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            e.paste(&text, cx)
        }
    });
    let root = on!(root, AddCursorAbove, |e, _, cx| e.add_cursor(false, cx));
    let root = on!(root, AddCursorBelow, |e, _, cx| e.add_cursor(true, cx));
    let root = on!(root, SelectNext, |e, _, cx| e.select_next(cx));
    let root = on!(root, SelectAllOccurrences, |e, _, cx| e
        .select_all_occurrences(cx));
    let root = on!(root, Escape, |e, _, cx| e.escape(cx));
    let root = on!(root, ToggleComment, |e, _, cx| e.toggle_comment(cx));
    let root = on!(root, Fold, |e, _, cx| e.fold_at_cursor(cx));
    let root = on!(root, Unfold, |e, _, cx| e.unfold_at_cursor(cx));
    let root = on!(root, FoldAll, |e, _, cx| e.fold_all(true, cx));
    let root = on!(root, UnfoldAll, |e, _, cx| e.fold_all(false, cx));
    let root = on!(root, MoveLineUp, |e, _, cx| e.move_lines(false, cx));
    let root = on!(root, MoveLineDown, |e, _, cx| e.move_lines(true, cx));
    let root = on!(root, DuplicateLine, |e, _, cx| e.duplicate_lines(cx));
    let root = on!(root, DeleteLine, |e, _, cx| e.delete_lines(cx));
    let root = on!(root, Find, |e, window, cx| e.open_find(false, window, cx));
    let root = on!(root, FindReplace, |e, window, cx| e
        .open_find(true, window, cx));
    let root = on!(root, FindNext, |e, _, cx| e.find_step(true, cx));
    let root = on!(root, FindPrevious, |e, _, cx| e.find_step(false, cx));
    let root = on!(root, Save, |e, _, cx| e.save(false, cx));
    let root = on!(root, GoToDefinition, |e, _, cx| {
        let head = e.primary().head;
        e.go_to_definition(head, cx)
    });
    let root = on!(root, TriggerCompletion, |e, _, cx| e
        .update_completion(true, cx));
    on!(root, TriggerSignature, |e, _, cx| e.update_signature(cx))
}

impl CodeEditor {
    fn scrolled(&self) -> Pixels {
        -self.scroll.0.borrow().base_handle.offset().y
    }

    /// The row under a window y, clamped to the rows that exist.
    pub(crate) fn row_at(&self, y: Pixels) -> Option<usize> {
        let metrics = self.metrics;
        if metrics.line <= Pixels::ZERO || self.layout.rows.is_empty() {
            return None;
        }
        let row = ((y - metrics.top + self.scrolled()) / metrics.line).floor();
        Some((row.max(0.0) as usize).min(self.layout.rows.len() - 1))
    }

    /// The offset under a window point: on a lens row, the line below it.
    pub(crate) fn offset_at(&self, position: Point<Pixels>) -> Option<usize> {
        let doc = self.doc.as_ref()?;
        if self.metrics.advance <= Pixels::ZERO {
            return None;
        }
        let row = self.row_at(position.y)?;
        let line = match self.layout.rows[row] {
            Row::Line(line) => line,
            Row::Lens(lens) => self.layout.lenses.get(lens)?.line,
        };
        let text = doc.buffer.line(line);
        let column = ((position.x - self.metrics.left) / self.metrics.advance)
            .round()
            .max(0.0) as usize;
        Some(doc.buffer.line_range(line).start + syntax::byte_at_column(text, column, self.tab))
    }

    /// A press in the code: a caret, a word on a double press, the line on a
    /// triple; Shift extends, Alt adds a caret, Cmd jumps to a definition.
    pub(crate) fn press(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus, cx);
        let Some(at) = self.offset_at(event.position) else {
            return;
        };
        if self.doc.is_none() {
            return;
        }
        if event.modifiers.platform {
            self.set_selections(vec![Selection::caret(at)], cx);
            self.go_to_definition(at, cx);
            return;
        }
        self.close_completion();
        self.hover = None;
        let Some(doc) = &self.doc else {
            return;
        };
        let chosen = match event.click_count {
            2 => vec![Selection::span(doc.buffer.word_at(at))],
            3.. => {
                let range = doc.buffer.line_range(doc.buffer.line_of(at));
                vec![Selection::span(range.start..doc.buffer.next(range.end))]
            }
            _ if event.modifiers.shift => {
                let mut all = doc.selections.clone();
                let primary = all.last_mut().expect("an editor keeps a cursor");
                primary.head = at;
                primary.goal = None;
                all
            }
            _ if event.modifiers.alt => {
                let mut all = doc.selections.clone();
                // Alt-pressing an existing caret removes it.
                if all.len() > 1
                    && let Some(existing) = all
                        .iter()
                        .position(|selection| selection.is_empty() && selection.head == at)
                {
                    all.remove(existing);
                    self.set_selections(all, cx);
                    return;
                }
                all.push(Selection::caret(at));
                all
            }
            _ => {
                self.dragging = true;
                vec![Selection::caret(at)]
            }
        };
        self.set_selections(chosen, cx);
    }

    /// A drag stretches the primary selection to the pointer; with no
    /// button held, the pointer asks for a hover card.
    pub(crate) fn pointer_moved(&mut self, event: &MouseMoveEvent, cx: &mut Context<Self>) {
        if !event.dragging() {
            self.dragging = false;
            let offset = self.offset_at(event.position);
            self.hover_at(offset, cx);
            return;
        }
        if !self.dragging {
            return;
        }
        let Some(at) = self.offset_at(event.position) else {
            return;
        };
        let Some(doc) = &self.doc else {
            return;
        };
        let mut all = doc.selections.clone();
        let primary = all.last_mut().expect("an editor keeps a cursor");
        if primary.head == at {
            return;
        }
        primary.head = at;
        primary.goal = None;
        self.set_selections(all, cx);
        // Drags past the edges scroll the code along.
        let metrics = self.metrics;
        let scrolled = self.scrolled();
        let top = metrics.top;
        if event.position.y < top {
            let next = (scrolled - metrics.line).max(Pixels::ZERO);
            self.scroll
                .0
                .borrow()
                .base_handle
                .set_offset(point(Pixels::ZERO, -next));
        }
        cx.notify();
    }

    /// Keys typed into the find bar: its fields edit like the query fields
    /// elsewhere in diri, and Enter steps or replaces.
    pub(crate) fn find_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let keystroke = &event.keystroke;
        let modifiers = keystroke.modifiers;
        match keystroke.key.as_str() {
            "escape" => {
                self.close_find(cx);
                window.focus(&self.focus, cx);
            }
            "enter" if self.find.in_replacement && modifiers.platform => self.replace_all(cx),
            "enter" if self.find.in_replacement => self.replace_current(cx),
            "enter" => self.find_step(!modifiers.shift, cx),
            "g" if modifiers.platform => self.find_step(!modifiers.shift, cx),
            "f" if modifiers.platform && modifiers.alt => {
                self.find.replace_open = !self.find.replace_open;
                self.find.in_replacement = self.find.replace_open;
                cx.notify();
            }
            "f" if modifiers.platform => {
                self.find.in_replacement = false;
                self.find.query.select_all();
                cx.notify();
            }
            "tab" => {
                if self.find.replace_open {
                    self.find.in_replacement = !self.find.in_replacement;
                    cx.notify();
                }
            }
            "c" if modifiers.alt && !modifiers.platform => {
                self.toggle_find_option(|options| &mut options.case, cx)
            }
            "w" if modifiers.alt && !modifiers.platform => {
                self.toggle_find_option(|options| &mut options.word, cx)
            }
            "r" if modifiers.alt && !modifiers.platform => {
                self.toggle_find_option(|options| &mut options.regex, cx)
            }
            _ => {
                let Some(edit) = query_editor::edit_for(keystroke) else {
                    return;
                };
                let in_replacement = self.find.in_replacement;
                let field = if in_replacement {
                    &mut self.find.replacement
                } else {
                    &mut self.find.query
                };
                let changed = match edit {
                    Edit::Local(local) => field.apply(local),
                    Edit::Clipboard(ClipboardEdit::Copy) => {
                        query_editor::copy_selection(field, cx);
                        false
                    }
                    Edit::Clipboard(ClipboardEdit::Cut) => query_editor::cut_selection(field, cx),
                    Edit::Clipboard(ClipboardEdit::Paste) => cx
                        .read_from_clipboard()
                        .and_then(|item| item.text())
                        .is_some_and(|text| field.insert(&text.replace('\n', " "))),
                };
                if changed && !in_replacement {
                    self.find_changed(cx);
                }
                cx.notify();
            }
        }
        cx.stop_propagation();
    }

    pub(crate) fn toggle_find_option(
        &mut self,
        option: fn(&mut super::find::FindOptions) -> &mut bool,
        cx: &mut Context<Self>,
    ) {
        let flag = option(&mut self.find.options);
        *flag = !*flag;
        self.find_changed(cx);
    }

    fn utf16_range(&self, range: &Range<usize>) -> Range<usize> {
        let text = self.text();
        to_utf16(text, range.start)..to_utf16(text, range.end)
    }

    fn byte_range(&self, range: &Range<usize>) -> Range<usize> {
        let text = self.text();
        from_utf16(text, range.start)..from_utf16(text, range.end)
    }

    /// Where a byte offset draws, in window pixels.
    fn bounds_of(&self, offset: usize) -> Option<Bounds<Pixels>> {
        let doc = self.doc.as_ref()?;
        let line = doc.buffer.line_of(offset);
        let row = self.row_of_line(line);
        let text = doc.buffer.line(line);
        let start = doc.buffer.line_range(line).start;
        let column = syntax::column_at(text, offset.saturating_sub(start), self.tab);
        let metrics = self.metrics;
        Some(Bounds::new(
            point(
                metrics.left + metrics.advance * column as f32,
                metrics.top + metrics.line * row as f32 - self.scrolled(),
            ),
            gpui::size(metrics.advance, metrics.line),
        ))
    }
}

/// A UTF-8 byte offset as a UTF-16 code unit offset.
pub(crate) fn to_utf16(text: &str, byte: usize) -> usize {
    text[..byte.min(text.len())].encode_utf16().count()
}

/// A UTF-16 code unit offset as a UTF-8 byte offset on a character boundary.
pub(crate) fn from_utf16(text: &str, units: usize) -> usize {
    let mut seen = 0;
    for (at, ch) in text.char_indices() {
        if seen >= units {
            return at;
        }
        seen += ch.len_utf16();
    }
    text.len()
}

impl EntityInputHandler for CodeEditor {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.byte_range(&range_utf16);
        actual_range.replace(self.utf16_range(&range));
        Some(self.text()[range].to_string())
    }

    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        self.doc.as_ref()?;
        let primary = self.primary();
        Some(UTF16Selection {
            range: self.utf16_range(&primary.range()),
            reversed: primary.head < primary.anchor,
        })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked.as_ref().map(|range| self.utf16_range(range))
    }

    fn unmark_text(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        self.marked = None;
        self.end_composition();
        cx.notify();
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.editable() {
            return;
        }
        let range = range_utf16
            .map(|range| self.byte_range(&range))
            .or(self.marked.clone());
        match range {
            Some(range) => {
                self.marked = None;
                let end = range.start + text.len();
                self.apply_with(
                    vec![(range, text.to_string())],
                    vec![Selection::caret(end)],
                    true,
                    cx,
                );
            }
            None => self.type_text(text, cx),
        }
        self.end_composition();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        text: &str,
        selected_utf16: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.editable() {
            return;
        }
        let range = range_utf16
            .map(|range| self.byte_range(&range))
            .or(self.marked.clone())
            .unwrap_or(self.primary().range());
        self.begin_composition();
        let caret = match &selected_utf16 {
            Some(inner) => range.start + from_utf16(text, inner.end),
            None => range.start + text.len(),
        };
        self.apply_with(
            vec![(range.clone(), text.to_string())],
            vec![Selection::caret(caret)],
            true,
            cx,
        );
        self.marked = (!text.is_empty()).then(|| range.start..range.start + text.len());
        if self.marked.is_none() {
            self.end_composition();
        }
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        _: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let range = self.byte_range(&range_utf16);
        self.bounds_of(range.start)
    }

    fn character_index_for_point(
        &mut self,
        position: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        let offset = self.offset_at(position)?;
        Some(to_utf16(self.text(), offset))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf16_offsets_round_trip_through_wide_characters() {
        let text = "a😀b";
        assert_eq!(to_utf16(text, 1), 1);
        assert_eq!(to_utf16(text, 5), 3, "an emoji is two UTF-16 units");
        assert_eq!(from_utf16(text, 3), 5);
        assert_eq!(from_utf16(text, 2), 5, "inside a pair lands after it");
        assert_eq!(from_utf16(text, 99), text.len());
    }

    #[test]
    fn deleting_to_the_line_start_joins_lines_at_column_zero() {
        let buffer = Buffer::new("ab\ncd");
        assert_eq!(line_left(&buffer, 5), 3..5);
        assert_eq!(line_left(&buffer, 3), 2..3);
    }
}
