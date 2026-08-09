//! The single-line text field behind ⌘K and ⌘P.
//!
//! The overlays used to keep their query in a bare `String` that only grew by
//! `push_str` and shrank by `pop`, so there was no caret to move, nothing to
//! select, and no way to clear a query short of holding backspace. This is the
//! smallest editor that behaves the way a macOS text field is expected to:
//! a caret, a selection anchor, and word/line-granularity motion.
//!
//! Offsets are byte indices into `text` and always land on `char` boundaries.

use std::ops::Range;

use gpui::Keystroke;
use unicode_segmentation::UnicodeSegmentation;

/// How far a motion or deletion reaches.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Motion {
    /// One grapheme — what an unmodified arrow or backspace does.
    Character,
    /// To the next word boundary (⌥).
    Word,
    /// To the start or end of the line (⌘).
    Line,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct QueryEditor {
    text: String,
    cursor: usize,
    /// The fixed end of the selection. Equal to `cursor` when nothing is
    /// selected, which is the only state the caret is drawn in.
    anchor: usize,
}

impl QueryEditor {
    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub const fn cursor(&self) -> usize {
        self.cursor
    }

    /// The selected byte range, or `None` when the selection is empty.
    pub fn selection(&self) -> Option<Range<usize>> {
        (self.cursor != self.anchor).then(|| {
            let start = self.cursor.min(self.anchor);
            start..self.cursor.max(self.anchor)
        })
    }

    pub fn selected_text(&self) -> Option<&str> {
        self.selection().map(|range| &self.text[range])
    }

    /// What to draw: the text with `caret` spliced in at the cursor, plus the
    /// byte range to paint as a selection. macOS hides the caret while a
    /// selection exists — the selection *is* the cursor — so the two are
    /// mutually exclusive and callers never have to decide.
    pub fn display(&self, caret: &str) -> (String, Option<Range<usize>>) {
        match self.selection() {
            Some(range) => (self.text.clone(), Some(range)),
            None => {
                let mut display = String::with_capacity(self.text.len() + caret.len());
                display.push_str(&self.text[..self.cursor]);
                display.push_str(caret);
                display.push_str(&self.text[self.cursor..]);
                (display, None)
            }
        }
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
        self.anchor = 0;
    }

    pub fn select_all(&mut self) {
        self.anchor = 0;
        self.cursor = self.text.len();
    }

    /// Replace the selection (if any) with `insertion`, leaving the caret after
    /// it. Control characters are dropped so a stray paste cannot corrupt the
    /// single-line invariant. Returns whether the text changed — callers use
    /// that to avoid re-ranking on a keystroke that did nothing.
    pub fn insert(&mut self, insertion: &str) -> bool {
        let filtered: String = insertion.chars().filter(|c| !c.is_control()).collect();
        self.replace_selection(&filtered)
    }

    /// Insert composer text while preserving line breaks. Search fields call
    /// [`Self::insert`] and stay strictly single-line; prompt composers use
    /// this variant so Shift-Return and pasted paragraphs retain their shape.
    pub fn insert_multiline(&mut self, insertion: &str) -> bool {
        let filtered: String = insertion
            .chars()
            .filter(|character| !character.is_control() || *character == '\n')
            .collect();
        self.replace_selection(&filtered)
    }

    fn replace_selection(&mut self, filtered: &str) -> bool {
        if filtered.is_empty() && self.selection().is_none() {
            return false;
        }
        let range = self.selection().unwrap_or(self.cursor..self.cursor);
        self.text.replace_range(range.clone(), filtered);
        self.cursor = range.start + filtered.len();
        self.anchor = self.cursor;
        true
    }

    /// Backspace: delete the selection, else reach backwards by `motion`.
    pub fn delete_backward(&mut self, motion: Motion) -> bool {
        if let Some(range) = self.selection() {
            self.text.replace_range(range.clone(), "");
            self.cursor = range.start;
            self.anchor = self.cursor;
            return true;
        }
        let target = self.offset_before(self.cursor, motion);
        if target == self.cursor {
            return false;
        }
        self.text.replace_range(target..self.cursor, "");
        self.cursor = target;
        self.anchor = target;
        true
    }

    /// Forward delete (fn-delete), mirroring `delete_backward`.
    pub fn delete_forward(&mut self, motion: Motion) -> bool {
        if self.selection().is_some() {
            return self.delete_backward(motion);
        }
        let target = self.offset_after(self.cursor, motion);
        if target == self.cursor {
            return false;
        }
        self.text.replace_range(self.cursor..target, "");
        self.anchor = self.cursor;
        true
    }

    /// Move the caret. `extend` keeps the anchor put, growing the selection;
    /// otherwise a collapsing selection drops the caret at its near edge, the
    /// way macOS does.
    pub fn move_left(&mut self, motion: Motion, extend: bool) {
        let from = match (extend, self.selection()) {
            (false, Some(range)) => {
                self.cursor = range.start;
                self.anchor = range.start;
                if motion == Motion::Character {
                    return;
                }
                range.start
            }
            _ => self.cursor,
        };
        self.cursor = self.offset_before(from, motion);
        if !extend {
            self.anchor = self.cursor;
        }
    }

    pub fn move_right(&mut self, motion: Motion, extend: bool) {
        let from = match (extend, self.selection()) {
            (false, Some(range)) => {
                self.cursor = range.end;
                self.anchor = range.end;
                if motion == Motion::Character {
                    return;
                }
                range.end
            }
            _ => self.cursor,
        };
        self.cursor = self.offset_after(from, motion);
        if !extend {
            self.anchor = self.cursor;
        }
    }

    fn offset_before(&self, from: usize, motion: Motion) -> usize {
        match motion {
            Motion::Line => 0,
            Motion::Character => self.text[..from]
                .grapheme_indices(true)
                .next_back()
                .map_or(0, |(index, _)| index),
            Motion::Word => self.text[..from]
                .split_word_bound_indices()
                .rfind(|(_, word)| !is_separator(word))
                .map_or(0, |(index, _)| index),
        }
    }

    fn offset_after(&self, from: usize, motion: Motion) -> usize {
        match motion {
            Motion::Line => self.text.len(),
            Motion::Character => self.text[from..]
                .graphemes(true)
                .next()
                .map_or(from, |grapheme| from + grapheme.len()),
            Motion::Word => self.text[from..]
                .split_word_bound_indices()
                .find(|(_, word)| !is_separator(word))
                .map_or(self.text.len(), |(index, word)| from + index + word.len()),
        }
    }
}

/// Whitespace and punctuation between words: skipped when hunting for the next
/// word boundary so ⌥← from "foo bar|" lands on "bar", not on the space.
fn is_separator(word: &str) -> bool {
    word.chars().all(|c| !c.is_alphanumeric())
}

/// An edit a query field can apply on its own.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LocalEdit {
    Insert(String),
    SelectAll,
    DeleteBackward(Motion),
    DeleteForward(Motion),
    MoveLeft(Motion, bool),
    MoveRight(Motion, bool),
}

/// An edit that needs the host's pasteboard, so the surface performs it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClipboardEdit {
    Copy,
    Cut,
    Paste,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Edit {
    Local(LocalEdit),
    Clipboard(ClipboardEdit),
}

/// The one key map every query field shares — the command palette, Quick Open,
/// and the terminal's find bar. Keeping it here is the point: "⌃W deletes a
/// word" has to mean the same thing in all three, and three hand-rolled key
/// matches would drift apart the first time one of them gained a key.
///
/// Alongside the macOS bindings (⌘A/⌘C/⌘X/⌘V, ⌥⌫ by word, ⌘⌫ by line) it
/// answers the readline/fzf keys that anyone who lives in a shell already has
/// in their fingers. `None` means the keystroke is not a text edit and the
/// caller should handle or ignore it.
pub fn edit_for(keystroke: &Keystroke) -> Option<Edit> {
    use ClipboardEdit as Clip;
    use LocalEdit as Local;

    let modifiers = keystroke.modifiers;
    // ⌘ reaches the line, ⌥ the word, neither the character — the standard
    // macOS text-field granularities.
    let motion = if modifiers.platform {
        Motion::Line
    } else if modifiers.alt {
        Motion::Word
    } else {
        Motion::Character
    };
    let extend = modifiers.shift;

    let local = match keystroke.key.as_str() {
        "a" if modifiers.platform => Local::SelectAll,
        "c" if modifiers.platform => return Some(Edit::Clipboard(Clip::Copy)),
        "x" if modifiers.platform => return Some(Edit::Clipboard(Clip::Cut)),
        "v" if modifiers.platform => return Some(Edit::Clipboard(Clip::Paste)),

        // Readline, as used by zsh, fzf, and nvim's insert mode.
        "a" if modifiers.control => Local::MoveLeft(Motion::Line, extend),
        "e" if modifiers.control => Local::MoveRight(Motion::Line, extend),
        "w" if modifiers.control => Local::DeleteBackward(Motion::Word),
        "u" if modifiers.control => Local::DeleteBackward(Motion::Line),
        "k" if modifiers.control => Local::DeleteForward(Motion::Line),
        "h" if modifiers.control => Local::DeleteBackward(Motion::Character),
        "d" if modifiers.control => Local::DeleteForward(Motion::Character),
        "b" if modifiers.control => Local::MoveLeft(Motion::Character, extend),
        "f" if modifiers.control => Local::MoveRight(Motion::Character, extend),

        "backspace" => Local::DeleteBackward(motion),
        "delete" => Local::DeleteForward(motion),
        "left" => Local::MoveLeft(motion, extend),
        "right" => Local::MoveRight(motion, extend),
        "home" => Local::MoveLeft(Motion::Line, extend),
        "end" => Local::MoveRight(Motion::Line, extend),

        // Anything else is only text if it produced a character and no
        // command/control modifier claimed it first.
        _ if !modifiers.platform && !modifiers.control && !modifiers.function => {
            Local::Insert(keystroke.key_char.clone()?)
        }
        _ => return None,
    };
    Some(Edit::Local(local))
}

/// Put the selection on the pasteboard, if there is one. Lives beside the key
/// map so ⌘C and ⌘X mean the same thing in every query field.
pub fn copy_selection(editor: &QueryEditor, cx: &mut gpui::App) {
    if let Some(text) = editor.selected_text() {
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(text.to_owned()));
    }
}

/// Copy and remove the current selection. Like a native text field, ⌘X with
/// no selection is a no-op rather than an alias for Backspace.
pub fn cut_selection(editor: &mut QueryEditor, cx: &mut gpui::App) -> bool {
    let Some(text) = editor.selected_text().map(str::to_owned) else {
        return false;
    };
    cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
    editor.delete_backward(Motion::Character)
}

impl QueryEditor {
    /// Apply an edit that needs no host services. Returns whether the text
    /// changed; a pure caret move returns false but still needs a repaint.
    pub fn apply(&mut self, edit: LocalEdit) -> bool {
        match edit {
            LocalEdit::Insert(text) => self.insert(&text),
            LocalEdit::DeleteBackward(motion) => self.delete_backward(motion),
            LocalEdit::DeleteForward(motion) => self.delete_forward(motion),
            LocalEdit::SelectAll => {
                self.select_all();
                false
            }
            LocalEdit::MoveLeft(motion, extend) => {
                self.move_left(motion, extend);
                false
            }
            LocalEdit::MoveRight(motion, extend) => {
                self.move_right(motion, extend);
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seeded(text: &str) -> QueryEditor {
        let mut editor = QueryEditor::default();
        editor.insert(text);
        editor
    }

    #[test]
    fn typing_appends_and_backspace_removes_one_grapheme() {
        let mut editor = seeded("diri");
        assert_eq!(editor.text(), "diri");
        assert_eq!(editor.cursor(), 4);
        editor.delete_backward(Motion::Character);
        assert_eq!(editor.text(), "dir");
        assert!(editor.selection().is_none());
    }

    #[test]
    fn select_all_then_typing_replaces_the_whole_query() {
        let mut editor = seeded("kairoskraft");
        editor.select_all();
        assert_eq!(editor.selected_text(), Some("kairoskraft"));
        editor.insert("diri");
        assert_eq!(editor.text(), "diri");
        assert_eq!(editor.cursor(), 4);
        assert!(editor.selection().is_none());
    }

    #[test]
    fn multiline_insert_preserves_paragraphs_without_admitting_other_controls() {
        let mut editor = QueryEditor::default();
        assert!(editor.insert_multiline("first\nsecond\tthird"));
        assert_eq!(editor.text(), "first\nsecondthird");
    }

    #[test]
    fn select_all_then_backspace_clears() {
        let mut editor = seeded("anara");
        editor.select_all();
        editor.delete_backward(Motion::Character);
        assert_eq!(editor.text(), "");
        assert_eq!(editor.cursor(), 0);
    }

    #[test]
    fn command_and_option_deletion_reach_line_and_word() {
        let mut editor = seeded("new claude code");
        editor.delete_backward(Motion::Word);
        assert_eq!(editor.text(), "new claude ");
        editor.delete_backward(Motion::Word);
        assert_eq!(editor.text(), "new ");
        editor.delete_backward(Motion::Line);
        assert_eq!(editor.text(), "");

        let mut editor = seeded("fun/kairoskraft");
        editor.delete_backward(Motion::Word);
        assert_eq!(editor.text(), "fun/");
    }

    #[test]
    fn arrows_move_the_caret_and_shift_extends_the_selection() {
        let mut editor = seeded("abc");
        editor.move_left(Motion::Character, false);
        assert_eq!(editor.cursor(), 2);
        editor.insert("X");
        assert_eq!(editor.text(), "abXc");

        let mut editor = seeded("abc");
        editor.move_left(Motion::Character, true);
        assert_eq!(editor.selected_text(), Some("c"));
        editor.move_left(Motion::Character, true);
        assert_eq!(editor.selected_text(), Some("bc"));
        // Collapsing a selection leftward parks the caret at its near edge.
        editor.move_left(Motion::Character, false);
        assert_eq!(editor.cursor(), 1);
        assert!(editor.selection().is_none());
    }

    #[test]
    fn line_motion_reaches_both_ends_and_clamps() {
        let mut editor = seeded("diri");
        editor.move_left(Motion::Line, false);
        assert_eq!(editor.cursor(), 0);
        editor.move_left(Motion::Character, false);
        assert_eq!(editor.cursor(), 0);
        editor.move_right(Motion::Line, false);
        assert_eq!(editor.cursor(), 4);
        editor.move_right(Motion::Character, false);
        assert_eq!(editor.cursor(), 4);
    }

    #[test]
    fn forward_delete_removes_the_character_after_the_caret() {
        let mut editor = seeded("abc");
        editor.move_left(Motion::Line, false);
        editor.delete_forward(Motion::Character);
        assert_eq!(editor.text(), "bc");
        assert_eq!(editor.cursor(), 0);
    }

    #[test]
    fn offsets_stay_on_character_boundaries_for_multibyte_text() {
        let mut editor = seeded("café-naïve");
        editor.delete_backward(Motion::Word);
        assert_eq!(editor.text(), "café-");
        editor.delete_backward(Motion::Character);
        assert_eq!(editor.text(), "café");
        editor.delete_backward(Motion::Character);
        assert_eq!(editor.text(), "caf");
    }

    fn keystroke(key: &str, modifiers: &str) -> Keystroke {
        Keystroke {
            modifiers: gpui::Modifiers {
                control: modifiers.contains('^'),
                alt: modifiers.contains('~'),
                shift: modifiers.contains('$'),
                platform: modifiers.contains('@'),
                function: false,
            },
            key: key.to_owned(),
            key_char: (key.len() == 1).then(|| key.to_owned()),
        }
    }

    /// Drive an editor the way a surface does: map the keystroke, apply it.
    fn press(editor: &mut QueryEditor, key: &str, modifiers: &str) {
        match edit_for(&keystroke(key, modifiers)) {
            Some(Edit::Local(local)) => {
                editor.apply(local);
            }
            other => panic!("{key:?} mapped to {other:?}"),
        }
    }

    #[test]
    fn readline_keys_match_what_a_shell_does() {
        let mut editor = seeded("new claude code");
        press(&mut editor, "w", "^");
        assert_eq!(editor.text(), "new claude ");
        press(&mut editor, "u", "^");
        assert_eq!(editor.text(), "");

        let mut editor = seeded("kairoskraft");
        press(&mut editor, "a", "^"); // to line start
        assert_eq!(editor.cursor(), 0);
        press(&mut editor, "e", "^"); // to line end
        assert_eq!(editor.cursor(), 11);
        press(&mut editor, "b", "^"); // back one
        assert_eq!(editor.cursor(), 10);
        press(&mut editor, "h", "^"); // backspace
        assert_eq!(editor.text(), "kairoskrat");

        let mut editor = seeded("diri");
        press(&mut editor, "a", "^");
        press(&mut editor, "d", "^"); // forward delete
        assert_eq!(editor.text(), "iri");
        press(&mut editor, "k", "^"); // kill to end
        assert_eq!(editor.text(), "");
    }

    #[test]
    fn the_key_map_separates_macos_clipboard_from_local_edits() {
        assert_eq!(
            edit_for(&keystroke("a", "@")),
            Some(Edit::Local(LocalEdit::SelectAll))
        );
        assert_eq!(
            edit_for(&keystroke("v", "@")),
            Some(Edit::Clipboard(ClipboardEdit::Paste))
        );
        // ⌘/⌥ change the granularity of the same key.
        assert_eq!(
            edit_for(&keystroke("backspace", "@")),
            Some(Edit::Local(LocalEdit::DeleteBackward(Motion::Line)))
        );
        assert_eq!(
            edit_for(&keystroke("backspace", "~")),
            Some(Edit::Local(LocalEdit::DeleteBackward(Motion::Word)))
        );
        // ⇧ extends instead of collapsing.
        assert_eq!(
            edit_for(&keystroke("left", "$")),
            Some(Edit::Local(LocalEdit::MoveLeft(Motion::Character, true)))
        );
        // Keys the field has no business claiming stay with the caller.
        assert_eq!(edit_for(&keystroke("enter", "")), None);
        assert_eq!(edit_for(&keystroke("escape", "")), None);
        assert_eq!(edit_for(&keystroke("n", "^")), None);
        assert_eq!(edit_for(&keystroke("p", "^")), None);
    }

    #[test]
    fn pasted_control_characters_never_reach_the_query() {
        let mut editor = seeded("a");
        editor.insert("b\nc\td\r");
        assert_eq!(editor.text(), "abcd");
    }
}
