//! The block editor's behaviour, independent of any UI toolkit.
//!
//! [`Editor`] owns the open note as a list of blocks whose first entry is the
//! title, plus a selection that may span blocks. Every keyboard behaviour that
//! does not need pixel layout lives here — typing, Enter and Backspace rules,
//! Markdown shortcuts, inline autoformat, paste, indentation, and undo — so it
//! is tested without a window. The GPUI view adds only layout-driven motion
//! (up/down, clicks) and drawing.

use std::collections::HashSet;
use std::ops::Range;

use unicode_segmentation::UnicodeSegmentation;

use crate::doc::{Block, BlockKind, Document, MAX_INDENT, Mark, Style, floor_boundary};
use crate::markdown;
use crate::mention::{self, MentionTarget};

mod blocks;
mod table;
pub use table::{RowsSource, TablePos, parse_rows};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Pos {
    pub block: usize,
    pub offset: usize,
}

impl Pos {
    pub const fn new(block: usize, offset: usize) -> Self {
        Self { block, offset }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Selection {
    pub anchor: Pos,
    pub head: Pos,
}

impl Selection {
    pub const fn caret(pos: Pos) -> Self {
        Self {
            anchor: pos,
            head: pos,
        }
    }

    pub fn is_collapsed(&self) -> bool {
        self.anchor == self.head
    }

    pub fn start(&self) -> Pos {
        self.anchor.min(self.head)
    }

    pub fn end(&self) -> Pos {
        self.anchor.max(self.head)
    }

    pub fn reversed(&self) -> bool {
        self.head < self.anchor
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Granularity {
    Grapheme,
    Word,
    /// Start/end of the block (the view maps ⌘← to visual lines itself).
    Block,
    Document,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EditKind {
    Typing,
    Deleting,
    Other,
}

#[derive(Clone)]
struct Snapshot {
    blocks: Vec<Block>,
    selection: Selection,
}

const UNDO_LIMIT: usize = 300;
/// Typing within this many milliseconds coalesces into one undo step.
const COALESCE_MS: u64 = 1_200;

/// A slash-menu / shortcut target.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Turn {
    Kind(BlockKind),
    Divider,
    /// A Mermaid diagram: a code block fenced `mermaid`.
    Diagram,
}

/// What an empty block turned into a diagram starts with, so it draws at
/// once and shows the syntax.
pub const DIAGRAM_STARTER: &str = "flowchart LR\n  A[Idea] --> B[Plan] --> C[Ship]";

pub struct Editor {
    blocks: Vec<Block>,
    pub selection: Selection,
    /// Styles toggled at a collapsed caret (⌘B then type).
    pending: Option<Vec<Style>>,
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    last_edit: Option<(EditKind, u64)>,
    next_id: u64,
    /// Bumped on every content change; views key caches off it.
    pub revision: u64,
    /// List items whose children are folded away, by block id. View state:
    /// never saved to the note and not part of undo.
    collapsed: HashSet<u64>,
}

impl Editor {
    pub fn new(doc: &Document) -> Self {
        let mut editor = Self {
            blocks: Vec::with_capacity(doc.blocks.len() + 1),
            selection: Selection::default(),
            pending: None,
            undo: Vec::new(),
            redo: Vec::new(),
            last_edit: None,
            next_id: 1,
            revision: 0,
            collapsed: HashSet::new(),
        };
        let title_id = editor.fresh_id();
        editor
            .blocks
            .push(Block::new(title_id, BlockKind::Title, doc.title.clone()));
        for block in &doc.blocks {
            let mut block = block.clone();
            block.id = editor.fresh_id();
            editor.blocks.push(block);
        }
        editor.ensure_trailing();
        editor.selection = if doc.title.is_empty() {
            Selection::caret(Pos::new(0, 0))
        } else {
            let last = editor.blocks.len() - 1;
            Selection::caret(Pos::new(last, editor.blocks[last].text.len()))
        };
        editor
    }

    fn fresh_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    pub fn blocks(&self) -> &[Block] {
        &self.blocks
    }

    pub fn block(&self, index: usize) -> &Block {
        &self.blocks[index]
    }

    pub fn title(&self) -> &str {
        &self.blocks[0].text
    }

    /// The note as a document, ready to save.
    pub fn document(&self) -> Document {
        Document::new(
            self.blocks[0].text.trim().to_owned(),
            self.blocks[1..].to_vec(),
        )
    }

    /// The 1-based ordinal a numbered block displays.
    pub fn ordinal(&self, index: usize) -> usize {
        let indent = self.blocks[index].indent;
        let mut n = 1;
        for previous in self.blocks[1..index].iter().rev() {
            if previous.kind.is_list() && previous.indent > indent {
                continue;
            }
            if previous.kind == BlockKind::Numbered && previous.indent == indent {
                n += 1;
                continue;
            }
            break;
        }
        n
    }

    /// The body always ends in a paragraph the caret can land in below a
    /// list, code block, or divider — Notion's "click below to write".
    fn ensure_trailing(&mut self) {
        let needs = self
            .blocks
            .last()
            .is_none_or(|last| last.kind != BlockKind::Paragraph || self.blocks.len() == 1);
        if needs {
            let id = self.fresh_id();
            self.blocks.push(Block::new(id, BlockKind::Paragraph, ""));
        }
    }

    fn clamp_pos(&self, pos: Pos) -> Pos {
        let block = pos.block.min(self.blocks.len() - 1);
        // An image or divider is selected whole: its caret has one place.
        if self.blocks[block].kind.is_atomic() {
            return Pos::new(block, 0);
        }
        let text = &self.blocks[block].text;
        Pos::new(block, floor_boundary(text, pos.offset))
    }

    pub fn set_selection(&mut self, selection: Selection) {
        let selection = Selection {
            anchor: self.clamp_pos(selection.anchor),
            head: self.clamp_pos(selection.head),
        };
        self.reveal(selection.anchor.block);
        self.reveal(selection.head.block);
        if selection != self.selection {
            self.pending = None;
            self.last_edit = None;
        }
        self.selection = selection;
    }

    pub fn set_caret(&mut self, pos: Pos) {
        self.set_selection(Selection::caret(pos));
    }

    /// Moves the head (extending) or both ends.
    pub fn move_to(&mut self, pos: Pos, extend: bool) {
        let pos = self.clamp_pos(pos);
        let selection = if extend {
            Selection {
                anchor: self.selection.anchor,
                head: pos,
            }
        } else {
            Selection::caret(pos)
        };
        self.set_selection(selection);
    }

    pub fn select_all(&mut self) {
        // First ⌘A selects the block, the second the whole note.
        let head = self.selection.head;
        let block_range = Selection {
            anchor: Pos::new(head.block, 0),
            head: Pos::new(head.block, self.blocks[head.block].text.len()),
        };
        if self.selection != block_range && !self.blocks[head.block].text.is_empty() {
            self.set_selection(block_range);
            return;
        }
        let last = self.blocks.len() - 1;
        self.set_selection(Selection {
            anchor: Pos::new(0, 0),
            head: Pos::new(last, self.blocks[last].text.len()),
        });
    }

    // -----------------------------------------------------------------------
    // Folding

    /// The blocks nested under the list item at `index`: the run of deeper
    /// list items right after it. Empty for anything that is not a list item.
    pub fn children(&self, index: usize) -> Range<usize> {
        let Some(block) = self.blocks.get(index) else {
            return index..index;
        };
        let mut end = index + 1;
        if block.kind.is_list() {
            while self
                .blocks
                .get(end)
                .is_some_and(|b| b.kind.is_list() && b.indent > block.indent)
            {
                end += 1;
            }
        }
        index + 1..end
    }

    pub fn has_children(&self, index: usize) -> bool {
        !self.children(index).is_empty()
    }

    /// Whether the item at `index` hides its children.
    pub fn is_collapsed(&self, index: usize) -> bool {
        self.blocks
            .get(index)
            .is_some_and(|b| self.collapsed.contains(&b.id))
            && self.has_children(index)
    }

    /// Folds or unfolds the children of the list item at `index`. Folding
    /// with the caret inside the subtree moves it to the end of the item.
    /// A view-state change: the note's content and revision are untouched.
    pub fn set_collapsed(&mut self, index: usize, collapsed: bool) {
        if !self.has_children(index) {
            return;
        }
        let id = self.blocks[index].id;
        if !collapsed {
            self.collapsed.remove(&id);
            return;
        }
        self.collapsed.insert(id);
        let inside = self.children(index);
        if inside.contains(&self.selection.head.block)
            || inside.contains(&self.selection.anchor.block)
        {
            let end = Pos::new(index, self.blocks[index].text.len());
            self.selection = Selection::caret(end);
            self.pending = None;
            self.last_edit = None;
        }
    }

    pub fn toggle_collapsed(&mut self, index: usize) {
        let collapsed = self.is_collapsed(index);
        self.set_collapsed(index, !collapsed);
    }

    /// For each block, whether a folded ancestor hides it.
    pub fn hidden(&self) -> Vec<bool> {
        let mut hidden = vec![false; self.blocks.len()];
        let mut fold: Option<u8> = None;
        for (index, block) in self.blocks.iter().enumerate() {
            if let Some(depth) = fold {
                if block.kind.is_list() && block.indent > depth {
                    hidden[index] = true;
                    continue;
                }
                fold = None;
            }
            if block.kind.is_list() && self.collapsed.contains(&block.id) {
                fold = Some(block.indent);
            }
        }
        hidden
    }

    pub fn is_hidden(&self, index: usize) -> bool {
        self.hidden().get(index).copied().unwrap_or(false)
    }

    /// Unfolds every ancestor hiding `index`.
    fn reveal(&mut self, index: usize) {
        if self.collapsed.is_empty() || !self.is_hidden(index) {
            return;
        }
        let mut depth = self.blocks[index].indent;
        for ancestor in (0..index).rev() {
            let block = &self.blocks[ancestor];
            if !block.kind.is_list() {
                break;
            }
            if block.indent < depth {
                depth = block.indent;
                self.collapsed.remove(&block.id);
                if depth == 0 {
                    break;
                }
            }
        }
    }

    fn visible_after(&self, index: usize) -> Option<usize> {
        let hidden = self.hidden();
        (index + 1..self.blocks.len()).find(|i| !hidden[*i])
    }

    fn visible_before(&self, index: usize) -> Option<usize> {
        let hidden = self.hidden();
        (0..index).rev().find(|i| !hidden[*i])
    }

    // -----------------------------------------------------------------------
    // Horizontal motion

    pub fn step(&self, pos: Pos, forward: bool, granularity: Granularity) -> Pos {
        let text = &self.blocks[pos.block].text;
        match granularity {
            Granularity::Document => {
                if forward {
                    let last = self.blocks.len() - 1;
                    Pos::new(last, self.blocks[last].text.len())
                } else {
                    Pos::new(0, 0)
                }
            }
            Granularity::Block => Pos::new(pos.block, if forward { text.len() } else { 0 }),
            Granularity::Grapheme | Granularity::Word => {
                let atomic = self.blocks[pos.block].kind.is_atomic();
                if forward && (atomic || pos.offset >= text.len()) {
                    return match self.visible_after(pos.block) {
                        Some(next) => Pos::new(next, 0),
                        None => pos,
                    };
                }
                if !forward && (atomic || pos.offset == 0) {
                    return match self.visible_before(pos.block) {
                        Some(previous) => Pos::new(previous, self.blocks[previous].text.len()),
                        None => pos,
                    };
                }
                let mut offset = if granularity == Granularity::Grapheme {
                    grapheme_step(text, pos.offset, forward)
                } else {
                    word_step(text, pos.offset, forward)
                };
                // A mention chip is one unit: the caret never rests inside it.
                if let Some(chip) = mention::at(&self.blocks[pos.block], offset, false) {
                    offset = if forward {
                        chip.range.end
                    } else {
                        chip.range.start
                    };
                }
                Pos::new(pos.block, offset)
            }
        }
    }

    /// ←/→ with optional ⌥/⌘ granularity and ⇧ extension.
    pub fn move_horizontal(&mut self, forward: bool, granularity: Granularity, extend: bool) {
        if !extend && !self.selection.is_collapsed() && granularity == Granularity::Grapheme {
            let pos = if forward {
                self.selection.end()
            } else {
                self.selection.start()
            };
            self.set_caret(pos);
            return;
        }
        let pos = self.step(self.selection.head, forward, granularity);
        self.move_to(pos, extend);
    }

    // -----------------------------------------------------------------------
    // Undo

    fn checkpoint(&mut self, kind: EditKind, now_ms: u64) {
        let coalesce = matches!(
            (self.last_edit, kind),
            (Some((last, at)), k) if last == k && k != EditKind::Other && now_ms.saturating_sub(at) < COALESCE_MS
        );
        if !coalesce {
            self.undo.push(Snapshot {
                blocks: self.blocks.clone(),
                selection: self.selection,
            });
            if self.undo.len() > UNDO_LIMIT {
                self.undo.remove(0);
            }
        }
        self.redo.clear();
        self.last_edit = Some((kind, now_ms));
    }

    fn changed(&mut self) {
        self.revision += 1;
        self.ensure_trailing();
        // An edit never leaves the caret inside a folded subtree.
        self.reveal(self.selection.anchor.block);
        self.reveal(self.selection.head.block);
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn undo(&mut self) -> bool {
        let Some(snapshot) = self.undo.pop() else {
            return false;
        };
        self.redo.push(Snapshot {
            blocks: std::mem::replace(&mut self.blocks, snapshot.blocks),
            selection: self.selection,
        });
        self.selection = snapshot.selection;
        self.after_history();
        true
    }

    pub fn redo(&mut self) -> bool {
        let Some(snapshot) = self.redo.pop() else {
            return false;
        };
        self.undo.push(Snapshot {
            blocks: std::mem::replace(&mut self.blocks, snapshot.blocks),
            selection: self.selection,
        });
        self.selection = snapshot.selection;
        self.after_history();
        true
    }

    /// Takes in a write made outside the editor (an agent appending, the CLI
    /// ticking a to-do) while the person has unsaved typing. `merged` comes
    /// from [`crate::merge::merge3`] with this editor's document as "mine".
    /// It is one undo step; the person's blocks keep their identity and the
    /// caret stays with the text it was in. Returns whether anything changed.
    pub fn absorb(&mut self, merged: crate::merge::Merged, now_ms: u64) -> bool {
        // Editor block `i + 1` is document block `i`; block 0 is the title.
        let mut merged_to_mine = vec![None; merged.doc.blocks.len()];
        for (mine, target) in merged.mine_to_merged.iter().enumerate() {
            if let Some(target) = target {
                merged_to_mine[*target] = Some(mine);
            }
        }
        let mut blocks = Vec::with_capacity(merged.doc.blocks.len() + 2);
        let mut title = self.blocks[0].clone();
        title.text = merged.doc.title.clone();
        blocks.push(title);
        for (index, mut block) in merged.doc.blocks.into_iter().enumerate() {
            block.id = match merged_to_mine[index] {
                Some(mine) => self.blocks[mine + 1].id,
                None => self.fresh_id(),
            };
            blocks.push(block);
        }
        let map = |pos: Pos| -> Pos {
            if pos.block == 0 {
                return pos;
            }
            let mine = pos.block - 1;
            if let Some(Some(target)) = merged.mine_to_merged.get(mine) {
                return Pos::new(target + 1, pos.offset);
            }
            // A block the merge replaced: just after the nearest earlier
            // block that survived (the typing slot at the end, typically).
            let before = (0..mine)
                .rev()
                .find_map(|m| merged.mine_to_merged[m].map(|t| t + 1))
                .unwrap_or(0);
            Pos::new(before + 1, 0)
        };
        let selection = Selection {
            anchor: map(self.selection.anchor),
            head: map(self.selection.head),
        };
        let same = blocks.len() == self.blocks.len()
            && blocks.iter().zip(&self.blocks).all(|(a, b)| {
                a.kind == b.kind && a.indent == b.indent && a.text == b.text && a.marks == b.marks
            });
        if same {
            return false;
        }
        self.checkpoint(EditKind::Other, now_ms);
        self.blocks = blocks;
        self.pending = None;
        self.last_edit = None;
        self.changed();
        self.set_selection(selection);
        true
    }

    fn after_history(&mut self) {
        self.pending = None;
        self.last_edit = None;
        self.revision += 1;
        let selection = self.selection;
        self.set_selection(selection);
    }

    // -----------------------------------------------------------------------
    // Text editing

    /// Deletes the selection, leaving a collapsed caret. Returns whether
    /// anything was removed.
    fn delete_selection_inner(&mut self) -> bool {
        if self.selection.is_collapsed() {
            return false;
        }
        let start = self.selection.start();
        let end = self.selection.end();
        if start.block == end.block {
            self.blocks[start.block].replace(start.offset..end.offset, "", &[]);
        } else if (start.block..=end.block).any(|i| self.blocks[i].kind.is_cell()) {
            self.delete_across_tables(start, end);
        } else {
            let tail = {
                let last = &mut self.blocks[end.block];
                last.split_off(end.offset, 0, last.kind)
            };
            let first = &mut self.blocks[start.block];
            let len = first.text.len();
            first.replace(start.offset..len, "", &[]);
            if first.kind == BlockKind::Title {
                let text = tail.text.clone();
                first.replace(start.offset..start.offset, &text, &[]);
            } else {
                first.append(&tail);
            }
            self.blocks.drain(start.block + 1..=end.block);
        }
        self.selection = Selection::caret(start);
        true
    }

    /// Deletes a selection that touches a table without merging blocks:
    /// tables wholly inside it go, cells it partly covers are emptied
    /// (rows stay whole), other blocks inside it go, and the two ends lose
    /// the selected part of their text.
    fn delete_across_tables(&mut self, start: Pos, end: Pos) {
        let mut remove = vec![false; self.blocks.len()];
        let mut index = start.block + 1;
        while index < end.block {
            if let Some(range) = crate::doc::table_range(&self.blocks, index) {
                let inside = range.start > start.block && range.end <= end.block;
                let span = range.start.max(index)..range.end.min(end.block);
                if inside {
                    remove[span].fill(true);
                } else {
                    for block in &mut self.blocks[span] {
                        let len = block.text.len();
                        block.replace(0..len, "", &[]);
                    }
                }
                index = range.end.min(end.block).max(index + 1);
            } else {
                remove[index] = true;
                index += 1;
            }
        }
        let last = &mut self.blocks[end.block];
        last.replace(0..end.offset, "", &[]);
        let first = &mut self.blocks[start.block];
        let len = first.text.len();
        first.replace(start.offset..len, "", &[]);
        let mut i = 0;
        self.blocks.retain(|_| {
            let keep = !remove[i];
            i += 1;
            keep
        });
    }

    pub fn delete_selection(&mut self, now_ms: u64) {
        if self.selection.is_collapsed() {
            return;
        }
        self.checkpoint(EditKind::Other, now_ms);
        self.delete_selection_inner();
        self.changed();
    }

    /// Typing. Runs Markdown block shortcuts and inline autoformat.
    pub fn insert_text(&mut self, text: &str, now_ms: u64) {
        if text.contains('\n') {
            self.paste(text, now_ms);
            return;
        }
        if text.is_empty() {
            self.delete_selection(now_ms);
            return;
        }
        let replaced = !self.selection.is_collapsed();
        self.checkpoint(
            if replaced {
                EditKind::Other
            } else {
                EditKind::Typing
            },
            now_ms,
        );
        let styles_before = self.pending.clone();
        self.delete_selection_inner();
        self.leave_atomic();
        let pos = self.selection.head;
        let block = &mut self.blocks[pos.block];
        let styles = if block.kind == BlockKind::Title || block.kind == BlockKind::Code {
            Vec::new()
        } else {
            styles_before.unwrap_or_else(|| block.styles_at(pos.offset))
        };
        block.replace(pos.offset..pos.offset, text, &styles);
        let caret = Pos::new(pos.block, pos.offset + text.len());
        self.selection = Selection::caret(caret);
        if !replaced {
            if text == " " {
                self.block_shortcut(caret);
            } else if text == "-" {
                self.divider_shortcut(caret);
            } else if text == "`" {
                self.code_fence_shortcut(caret);
            }
            if matches!(text, "*" | "_" | "`" | "~") {
                self.inline_autoformat(text);
            }
        }
        self.changed();
    }

    /// `# `, `- `, `1. `, `[] `, `> ` at the start of a paragraph.
    fn block_shortcut(&mut self, caret: Pos) {
        let block = &self.blocks[caret.block];
        if !matches!(block.kind, BlockKind::Paragraph | BlockKind::Bullet) {
            return;
        }
        let prefix = &block.text[..caret.offset];
        let turn = match (block.kind, prefix) {
            (BlockKind::Paragraph, "# ") => BlockKind::Heading(1),
            (BlockKind::Paragraph, "## ") => BlockKind::Heading(2),
            (BlockKind::Paragraph, "### ") => BlockKind::Heading(3),
            (BlockKind::Paragraph, "- " | "* " | "+ ") => BlockKind::Bullet,
            (BlockKind::Paragraph, "1. " | "1) ") => BlockKind::Numbered,
            (BlockKind::Paragraph, "> " | "\" ") => BlockKind::Quote,
            (BlockKind::Paragraph, "[] " | "[ ] ") => BlockKind::Todo { checked: false },
            (BlockKind::Paragraph, "[x] ") => BlockKind::Todo { checked: true },
            (BlockKind::Bullet, "[] " | "[ ] ") => BlockKind::Todo { checked: false },
            (BlockKind::Bullet, "[x] ") => BlockKind::Todo { checked: true },
            _ => return,
        };
        let block = &mut self.blocks[caret.block];
        block.replace(0..caret.offset, "", &[]);
        block.set_kind(turn);
        self.selection = Selection::caret(Pos::new(caret.block, 0));
    }

    /// `---` in an otherwise empty paragraph becomes a divider.
    fn divider_shortcut(&mut self, caret: Pos) {
        let block = &self.blocks[caret.block];
        if block.kind != BlockKind::Paragraph || block.text != "---" {
            return;
        }
        self.blocks[caret.block].set_kind(BlockKind::Divider);
        let id = self.fresh_id();
        self.blocks
            .insert(caret.block + 1, Block::new(id, BlockKind::Paragraph, ""));
        self.selection = Selection::caret(Pos::new(caret.block + 1, 0));
    }

    /// ```` ``` ```` in an empty paragraph opens a code block.
    fn code_fence_shortcut(&mut self, caret: Pos) {
        let block = &self.blocks[caret.block];
        if block.kind != BlockKind::Paragraph || block.text != "```" {
            return;
        }
        let block = &mut self.blocks[caret.block];
        block.replace(0..3, "", &[]);
        block.set_kind(BlockKind::Code);
        self.selection = Selection::caret(Pos::new(caret.block, 0));
    }

    /// `**bold**`, `*italic*`, `_italic_`, `` `code` ``, `~strike~` convert
    /// the moment the closing delimiter is typed.
    fn inline_autoformat(&mut self, typed: &str) {
        let caret = self.selection.head;
        let block = &self.blocks[caret.block];
        if matches!(block.kind, BlockKind::Title | BlockKind::Code) {
            return;
        }
        let before = block.text[..caret.offset].to_owned();
        let before = before.as_str();
        let candidates: &[(&str, Style)] = match typed {
            "*" => &[("**", Style::Bold), ("*", Style::Italic)],
            "_" => &[("__", Style::Bold), ("_", Style::Italic)],
            "`" => &[("`", Style::Code)],
            "~" => &[("~~", Style::Strike), ("~", Style::Strike)],
            _ => return,
        };
        for (delim, style) in candidates {
            let Some(body_end) = before.len().checked_sub(delim.len()) else {
                continue;
            };
            if !before.ends_with(delim) {
                continue;
            }
            let search = &before[..body_end];
            let Some(open) = search.rfind(delim) else {
                continue;
            };
            let body = &search[open + delim.len()..];
            if body.is_empty() || body.trim() != body {
                continue;
            }
            // `**` must not be read as two single-star delimiters, and the
            // opener must start a word (`2*3*4` is arithmetic, not italic).
            let marker = delim.as_bytes()[0] as char;
            if delim.len() == 1
                && (body.starts_with(marker)
                    || search[..open].ends_with(marker)
                    || before[..body_end].ends_with(marker) && body.ends_with(marker))
            {
                continue;
            }
            if search[..open]
                .chars()
                .next_back()
                .is_some_and(char::is_alphanumeric)
            {
                continue;
            }
            if *style == Style::Code && body.contains('`') {
                continue;
            }
            let block = &mut self.blocks[caret.block];
            let body_start = open + delim.len();
            let body_len = body.len();
            block.replace(body_end..caret.offset, "", &[]);
            block.replace(open..body_start, "", &[]);
            let range = open..open + body_len;
            // Plain the body first so the mark covers exactly it.
            if *style == Style::Code {
                block.remove_mark(range.clone(), &Style::Bold);
                block.remove_mark(range.clone(), &Style::Italic);
            }
            block.add_mark(range.clone(), style.clone());
            self.selection = Selection::caret(Pos::new(caret.block, range.end));
            // Typing on continues plain.
            self.pending = Some(
                self.blocks[caret.block]
                    .styles_at(range.end)
                    .into_iter()
                    .filter(|s| s != style)
                    .collect(),
            );
            return;
        }
    }

    pub fn backspace(&mut self, granularity: Granularity, now_ms: u64) {
        if !self.selection.is_collapsed() {
            self.delete_selection(now_ms);
            return;
        }
        let pos = self.selection.head;
        if pos.offset > 0 {
            self.checkpoint(EditKind::Deleting, now_ms);
            let text = &self.blocks[pos.block].text;
            let start = match granularity {
                Granularity::Grapheme => grapheme_step(text, pos.offset, false),
                Granularity::Word => word_step(text, pos.offset, false),
                _ => 0,
            };
            let range = self.whole_mentions(pos.block, start..pos.offset);
            self.blocks[pos.block].replace(range.clone(), "", &[]);
            self.selection = Selection::caret(Pos::new(pos.block, range.start));
            self.changed();
            return;
        }
        if pos.block == 0 {
            return;
        }
        // At a cell's start Backspace steps back a cell; it never merges.
        if self.blocks[pos.block].kind.is_cell() {
            let previous = pos.block - 1;
            if self.blocks[previous].kind.is_cell() && !self.blocks[pos.block].kind.starts_table() {
                self.set_caret(self.end_of(previous));
            }
            return;
        }
        // Right after a table, an empty line goes and the caret lands in the
        // last cell; a line with text keeps it.
        if self.blocks[pos.block - 1].kind.is_cell() {
            let previous = pos.block - 1;
            if self.blocks[pos.block].text.is_empty()
                && self.blocks[pos.block].kind == BlockKind::Paragraph
            {
                self.checkpoint(EditKind::Other, now_ms);
                self.blocks.remove(pos.block);
                self.selection = Selection::caret(self.end_of(previous));
                self.changed();
            } else {
                self.set_caret(self.end_of(previous));
            }
            return;
        }
        self.checkpoint(EditKind::Other, now_ms);
        let kind = self.blocks[pos.block].kind;
        // Formatting unwinds before blocks merge: outdent, then plain text.
        if kind.is_list() && self.blocks[pos.block].indent > 0 {
            self.blocks[pos.block].indent -= 1;
        } else if !matches!(kind, BlockKind::Paragraph) {
            if kind.is_atomic() {
                self.blocks.remove(pos.block);
                self.selection = Selection::caret(self.end_of(pos.block - 1));
            } else {
                self.blocks[pos.block].set_kind(BlockKind::Paragraph);
            }
        } else {
            self.merge_into_previous(pos.block);
        }
        self.changed();
    }

    fn end_of(&self, block: usize) -> Pos {
        Pos::new(block, self.blocks[block].text.len())
    }

    fn merge_into_previous(&mut self, index: usize) {
        let previous = index - 1;
        if self.blocks[previous].kind.is_atomic() {
            self.blocks.remove(previous);
            self.selection = Selection::caret(Pos::new(previous, 0));
            return;
        }
        let block = self.blocks.remove(index);
        let join = self.blocks[previous].text.len();
        if self.blocks[previous].kind == BlockKind::Title {
            let title = &mut self.blocks[previous];
            title.replace(join..join, &block.text.replace('\n', " "), &[]);
        } else {
            self.blocks[previous].append(&block);
        }
        self.selection = Selection::caret(Pos::new(previous, join));
    }

    pub fn delete_forward(&mut self, granularity: Granularity, now_ms: u64) {
        if !self.selection.is_collapsed() {
            self.delete_selection(now_ms);
            return;
        }
        let pos = self.selection.head;
        let len = self.blocks[pos.block].text.len();
        if pos.offset < len {
            self.checkpoint(EditKind::Deleting, now_ms);
            let text = &self.blocks[pos.block].text;
            let end = match granularity {
                Granularity::Grapheme => grapheme_step(text, pos.offset, true),
                Granularity::Word => word_step(text, pos.offset, true),
                _ => len,
            };
            let range = self.whole_mentions(pos.block, pos.offset..end);
            self.blocks[pos.block].replace(range.clone(), "", &[]);
            self.selection = Selection::caret(Pos::new(pos.block, range.start));
            self.changed();
            return;
        }
        if pos.block + 1 >= self.blocks.len() {
            return;
        }
        // Delete never merges into, out of or across a table.
        if self.blocks[pos.block].kind.is_cell() || self.blocks[pos.block + 1].kind.is_cell() {
            return;
        }
        self.checkpoint(EditKind::Other, now_ms);
        self.merge_into_previous(pos.block + 1);
        self.changed();
    }

    /// Grows `range` to swallow every mention chip it touches, so deleting
    /// one character of a chip deletes the whole chip.
    fn whole_mentions(&self, block: usize, range: Range<usize>) -> Range<usize> {
        let mut range = range;
        for chip in mention::in_block(&self.blocks[block]) {
            let touches = chip.range.start < range.end && range.start < chip.range.end;
            if touches {
                range = range.start.min(chip.range.start)..range.end.max(chip.range.end);
            }
        }
        range
    }

    /// Replaces `range` of the caret's block — the typed `@query` — with a
    /// mention chip linking to `target`, followed by a space.
    pub fn insert_mention(
        &mut self,
        range: Range<usize>,
        target: &MentionTarget,
        label: &str,
        now_ms: u64,
    ) {
        let index = self.selection.head.block;
        if matches!(self.blocks[index].kind, BlockKind::Title | BlockKind::Code) || label.is_empty()
        {
            return;
        }
        self.checkpoint(EditKind::Other, now_ms);
        let block = &mut self.blocks[index];
        let range = block.clamp(range);
        let label = label.replace('\n', " ");
        block.replace(range.clone(), &label, &[]);
        let end = range.start + label.len();
        block.add_mark(range.start..end, Style::Link(target.url()));
        if !block.text[end..].starts_with(' ') {
            block.replace(end..end, " ", &[]);
        }
        self.selection = Selection::caret(Pos::new(index, end + 1));
        self.pending = None;
        self.changed();
    }

    /// With the caret on an image or a divider, typing goes to a paragraph
    /// right after it (the next one if it is empty, else a new one).
    fn leave_atomic(&mut self) {
        let at = self.selection.head.block;
        if !self.blocks[at].kind.is_atomic() {
            return;
        }
        let next_is_empty = self
            .blocks
            .get(at + 1)
            .is_some_and(|b| b.kind == BlockKind::Paragraph && b.text.is_empty());
        if next_is_empty {
            self.selection = Selection::caret(Pos::new(at + 1, 0));
        } else {
            self.insert_block_after(at, BlockKind::Paragraph);
        }
    }

    /// Inserts an image after the caret's block (or in place of an empty
    /// paragraph), then leaves the caret on a line below it.
    pub fn insert_image(&mut self, src: &str, alt: &str, now_ms: u64) {
        self.checkpoint(EditKind::Other, now_ms);
        self.delete_selection_inner();
        if let Some(table) = self.caret_table() {
            // Pictures do not go in cells: below the table instead.
            self.selection = Selection::caret(Pos::new(table.range.end - 1, 0));
            let id = self.fresh_id();
            self.blocks
                .insert(table.range.end, Block::image(id, src, alt));
            self.leave_atomic_from(table.range.end);
            self.changed();
            return;
        }
        let head = self.selection.head.block;
        let id = self.fresh_id();
        let image = Block::image(id, src, alt);
        // An empty line of any kind (a fresh to-do, a bullet) becomes the
        // image; anything with text keeps it and the image goes below.
        let replace =
            head > 0 && !self.blocks[head].kind.is_atomic() && self.blocks[head].text.is_empty();
        let at = if replace {
            self.blocks[head] = image;
            head
        } else {
            let at = if head == 0 { 1 } else { head + 1 };
            self.blocks.insert(at, image);
            at
        };
        self.leave_atomic_from(at);
        self.changed();
    }

    fn leave_atomic_from(&mut self, at: usize) {
        self.selection = Selection::caret(Pos::new(at, 0));
        self.leave_atomic();
    }

    /// Pastes a bare URL at a collapsed caret as a link: a tool the note
    /// recognises (a Notion page, a Linear issue, a Google Sheet) gets its
    /// readable title, anything else shows the URL itself. In a title or a
    /// code block the URL goes in as plain text.
    pub fn paste_url(&mut self, url: &str, now_ms: u64) {
        let index = self.selection.head.block;
        if matches!(self.blocks[index].kind, BlockKind::Title | BlockKind::Code) {
            self.paste(url, now_ms);
            return;
        }
        let title = crate::links::recognize(url).map_or_else(|| url.to_owned(), |r| r.title);
        self.checkpoint(EditKind::Other, now_ms);
        self.delete_selection_inner();
        let pos = self.selection.head;
        let block = &mut self.blocks[pos.block];
        let styles: Vec<Style> = block
            .styles_at(pos.offset)
            .into_iter()
            .filter(|s| !matches!(s, Style::Link(_) | Style::Code))
            .collect();
        block.replace(pos.offset..pos.offset, &title, &styles);
        let end = pos.offset + title.len();
        block.add_mark(pos.offset..end, Style::Link(url.to_owned()));
        self.selection = Selection::caret(Pos::new(pos.block, end));
        self.pending = None;
        self.changed();
    }

    /// Return.
    pub fn enter(&mut self, now_ms: u64) {
        self.checkpoint(EditKind::Other, now_ms);
        self.delete_selection_inner();
        // Return in a cell adds a row below, caret in the same column.
        if let Some(table) = self.caret_table() {
            self.add_row_inner(&table, true);
            self.changed();
            return;
        }
        if self.blocks[self.selection.head.block].kind.is_atomic() {
            // Return on a selected image or divider opens a line below it.
            let at = self.selection.head.block;
            self.insert_block_after(at, BlockKind::Paragraph);
            self.changed();
            return;
        }
        let pos = self.selection.head;
        let block = &self.blocks[pos.block];
        let kind = block.kind;

        if kind == BlockKind::Code {
            // Return on a blank last line leaves the code block.
            let at_end = pos.offset == block.text.len();
            if at_end
                && (block.text.ends_with('\n') || block.text.is_empty())
                && !block.text.is_empty()
            {
                let len = block.text.len();
                self.blocks[pos.block].replace(len - 1..len, "", &[]);
                self.insert_block_after(pos.block, BlockKind::Paragraph);
            } else {
                self.blocks[pos.block].replace(pos.offset..pos.offset, "\n", &[]);
                self.selection = Selection::caret(Pos::new(pos.block, pos.offset + 1));
            }
            self.changed();
            return;
        }

        if block.text.is_empty() && kind.continues() {
            // Return on an empty list item leaves the list (one level at a time).
            if block.indent > 0 {
                self.blocks[pos.block].indent -= 1;
            } else {
                self.blocks[pos.block].set_kind(BlockKind::Paragraph);
            }
            self.changed();
            return;
        }

        let next_kind = match kind {
            BlockKind::Todo { .. } => BlockKind::Todo { checked: false },
            kind if kind.continues() => kind,
            _ => BlockKind::Paragraph,
        };
        if pos.offset == 0 && !block.text.is_empty() && kind != BlockKind::Title {
            // Return at the start opens a line above and keeps this block.
            let block_indent = block.indent;
            let id = self.fresh_id();
            let mut above = Block::new(
                id,
                if kind.is_list() {
                    next_kind
                } else {
                    BlockKind::Paragraph
                },
                "",
            );
            if kind.is_list() {
                above.indent = block_indent;
            }
            self.blocks.insert(pos.block, above);
            self.selection = Selection::caret(Pos::new(pos.block + 1, 0));
            self.changed();
            return;
        }
        // Return on a folded item starts its next sibling, after the subtree.
        let at = if self.is_collapsed(pos.block) {
            self.children(pos.block).end
        } else {
            pos.block + 1
        };
        let id = self.fresh_id();
        let mut tail = self.blocks[pos.block].split_off(pos.offset, id, next_kind);
        if kind == BlockKind::Title {
            tail.marks.clear();
        }
        self.blocks.insert(at, tail);
        self.selection = Selection::caret(Pos::new(at, 0));
        self.changed();
    }

    fn insert_block_after(&mut self, index: usize, kind: BlockKind) {
        let id = self.fresh_id();
        self.blocks.insert(index + 1, Block::new(id, kind, ""));
        self.selection = Selection::caret(Pos::new(index + 1, 0));
    }

    /// Shift-Return: a line break inside the block.
    pub fn soft_break(&mut self, now_ms: u64) {
        let head = self.selection.head;
        // Cells are single-line, as GFM tables are.
        if self.blocks[head.block].kind.is_cell() {
            return;
        }
        if self.blocks[head.block].kind == BlockKind::Title {
            self.enter(now_ms);
            return;
        }
        self.checkpoint(EditKind::Other, now_ms);
        self.delete_selection_inner();
        let pos = self.selection.head;
        let styles = self.blocks[pos.block].styles_at(pos.offset);
        self.blocks[pos.block].replace(pos.offset..pos.offset, "\n", &styles);
        self.selection = Selection::caret(Pos::new(pos.block, pos.offset + 1));
        self.changed();
    }

    fn selected_blocks(&self) -> Range<usize> {
        let start = self.selection.start();
        let end = self.selection.end();
        // A selection ending at offset 0 of a block does not include it.
        let last = if end.offset == 0 && end.block > start.block {
            end.block - 1
        } else {
            end.block
        };
        // A folded item moves and converts with the subtree it hides.
        let mut end = last + 1;
        for index in start.block.max(1)..=last {
            if self.is_collapsed(index) {
                end = end.max(self.children(index).end);
            }
        }
        start.block.max(1)..end
    }

    /// Tab / Shift-Tab.
    pub fn indent(&mut self, outdent: bool, now_ms: u64) -> bool {
        let range = self.selected_blocks();
        if range.is_empty() {
            return false;
        }
        let head = self.selection.head;
        if range.len() == 1 && self.blocks[head.block].kind == BlockKind::Code {
            if outdent {
                return false;
            }
            self.insert_text("    ", now_ms);
            return true;
        }
        if !self.blocks[range.clone()].iter().any(|b| b.kind.is_list()) {
            return false;
        }
        self.checkpoint(EditKind::Other, now_ms);
        for index in range {
            if !self.blocks[index].kind.is_list() {
                continue;
            }
            let limit = if index > 1 && self.blocks[index - 1].kind.is_list() {
                (self.blocks[index - 1].indent + 1).min(MAX_INDENT)
            } else {
                0
            };
            let block = &mut self.blocks[index];
            block.indent = if outdent {
                block.indent.saturating_sub(1)
            } else {
                (block.indent + 1).min(limit)
            };
        }
        self.changed();
        true
    }

    /// Changes one block's kind in place, as an undoable edit: a callout's
    /// tone chosen from its glyph, say.
    pub fn set_block_kind(&mut self, index: usize, kind: BlockKind, now_ms: u64) {
        let Some(block) = self.blocks.get(index) else {
            return;
        };
        if block.kind == kind
            || block.kind.is_atomic()
            || kind.is_atomic()
            || block.kind.is_cell()
            || kind.is_cell()
            || index == 0
        {
            return;
        }
        self.checkpoint(EditKind::Other, now_ms);
        self.blocks[index].set_kind(kind);
        self.changed();
    }

    /// Converts every selected block (slash menu, ⌘⌥ shortcuts).
    pub fn turn_into(&mut self, turn: Turn, now_ms: u64) {
        if self.blocks[self.selection.head.block].kind.is_cell() {
            return;
        }
        self.checkpoint(EditKind::Other, now_ms);
        let range = self.selected_blocks();
        match turn {
            Turn::Divider => {
                let at = range.end.max(1);
                let head = self.selection.head;
                if range.len() == 1 && self.blocks[head.block].text.is_empty() && head.block > 0 {
                    self.blocks[head.block].set_kind(BlockKind::Divider);
                    self.insert_block_after(head.block, BlockKind::Paragraph);
                } else {
                    let id = self.fresh_id();
                    self.blocks
                        .insert(at, Block::new(id, BlockKind::Divider, ""));
                    self.insert_block_after(at, BlockKind::Paragraph);
                }
            }
            Turn::Kind(kind) => {
                for index in range {
                    let block = &mut self.blocks[index];
                    // An image has no text to carry into another kind, and a
                    // cell belongs to its table.
                    if block.kind == BlockKind::Image || block.kind.is_cell() {
                        continue;
                    }
                    let text = block.text.clone();
                    let was_diagram = block.is_diagram();
                    block.set_kind(kind);
                    if kind == BlockKind::Code {
                        block.text = text;
                        // Code is plain code: a diagram turns back into
                        // its source. Any other fence keeps its language.
                        if was_diagram {
                            block.info.clear();
                        }
                    }
                }
                let selection = self.selection;
                self.set_selection(selection);
            }
            Turn::Diagram => {
                let mut seeded = None;
                for index in range {
                    let block = &mut self.blocks[index];
                    if index == 0 || block.kind.is_atomic() || block.kind.is_cell() {
                        continue;
                    }
                    let text = block.text.clone();
                    block.set_kind(BlockKind::Code);
                    block.info = crate::doc::DIAGRAM_LANG.to_owned();
                    block.text = if text.trim().is_empty() {
                        seeded = Some(index);
                        DIAGRAM_STARTER.to_owned()
                    } else {
                        text
                    };
                }
                match seeded {
                    Some(index) => {
                        let end = self.blocks[index].text.len();
                        self.set_selection(Selection::caret(Pos::new(index, end)));
                    }
                    None => {
                        let selection = self.selection;
                        self.set_selection(selection);
                    }
                }
            }
        }
        self.changed();
    }

    /// ⌘Return: check/uncheck a to-do, or turn the block into one.
    pub fn toggle_todo(&mut self, now_ms: u64) {
        self.checkpoint(EditKind::Other, now_ms);
        let range = self.selected_blocks();
        let all_todo = self.blocks[range.clone()]
            .iter()
            .all(|b| matches!(b.kind, BlockKind::Todo { .. }));
        let all_checked = self.blocks[range.clone()]
            .iter()
            .all(|b| b.kind == BlockKind::Todo { checked: true });
        for index in range {
            let block = &mut self.blocks[index];
            if all_todo {
                block.kind = BlockKind::Todo {
                    checked: !all_checked,
                };
            } else if !block.kind.is_atomic()
                && !block.kind.is_cell()
                && block.kind != BlockKind::Title
            {
                let indent = block.indent;
                block.set_kind(BlockKind::Todo { checked: false });
                block.indent = indent;
            }
        }
        self.changed();
    }

    /// Clicking a checkbox.
    pub fn set_checked(&mut self, index: usize, checked: bool, now_ms: u64) {
        if !matches!(
            self.blocks.get(index).map(|b| b.kind),
            Some(BlockKind::Todo { .. })
        ) {
            return;
        }
        self.checkpoint(EditKind::Other, now_ms);
        self.blocks[index].kind = BlockKind::Todo { checked };
        self.changed();
    }

    /// Links a session to the to-do at `index` with a trailing mention chip
    /// (an agent started from it). One undo step; `false` when the block is
    /// not a to-do or already links that session.
    pub fn link_session(
        &mut self,
        index: usize,
        label: &str,
        session_id: &str,
        now_ms: u64,
    ) -> bool {
        let target = crate::mention::MentionTarget::Session(session_id.to_owned());
        let Some(block) = self.blocks.get(index) else {
            return false;
        };
        if !matches!(block.kind, BlockKind::Todo { .. })
            || crate::mention::in_block(block)
                .iter()
                .any(|m| m.target == target)
        {
            return false;
        }
        self.checkpoint(EditKind::Other, now_ms);
        crate::handoff::append_chip(&mut self.blocks[index], label, &target);
        self.changed();
        true
    }

    /// Writes `text` from `session_id` as an update line under the to-do at
    /// `index`, the way an agent's report lands in the file: one undo step.
    pub fn add_update(
        &mut self,
        index: usize,
        date: &str,
        label: &str,
        session_id: &str,
        text: &str,
        now_ms: u64,
    ) -> bool {
        if !self
            .blocks
            .get(index)
            .is_some_and(|block| matches!(block.kind, BlockKind::Todo { .. }))
        {
            return false;
        }
        self.checkpoint(EditKind::Other, now_ms);
        let mut blocks = std::mem::take(&mut self.blocks);
        let added = crate::work::insert_todo_update(
            &mut blocks,
            index,
            date,
            label,
            session_id,
            text,
            &mut || self.fresh_id(),
        );
        self.blocks = blocks;
        self.changed();
        added.is_some()
    }

    /// ⌘B / ⌘I / ⌘E / ⌘⇧X, and ⌘K with a URL.
    pub fn toggle_style(&mut self, style: Style, now_ms: u64) {
        if self.selection.is_collapsed() {
            let pos = self.selection.head;
            let mut styles = self
                .pending
                .clone()
                .unwrap_or_else(|| self.blocks[pos.block].styles_at(pos.offset));
            if let Some(i) = styles.iter().position(|s| *s == style) {
                styles.remove(i);
            } else {
                styles.push(style);
            }
            self.pending = Some(styles);
            return;
        }
        self.checkpoint(EditKind::Other, now_ms);
        let ranges = self.selected_ranges();
        let applies = |b: &Block| !matches!(b.kind, BlockKind::Title | BlockKind::Code);
        let has = ranges
            .iter()
            .filter(|(i, r)| !r.is_empty() && applies(&self.blocks[*i]))
            .all(|(i, r)| self.blocks[*i].has_style(r.clone(), &style));
        for (index, range) in ranges {
            if !applies(&self.blocks[index]) {
                continue;
            }
            let block = &mut self.blocks[index];
            if has {
                block.remove_mark(range, &style);
            } else {
                let trimmed = trim_range(&block.text, range);
                block.add_mark(trimmed, style.clone());
            }
        }
        self.changed();
    }

    /// Removes a link from the selection or the link under the caret.
    pub fn remove_link(&mut self, now_ms: u64) {
        self.checkpoint(EditKind::Other, now_ms);
        let pos = self.selection.head;
        let ranges = if self.selection.is_collapsed() {
            let block = &self.blocks[pos.block];
            block
                .marks
                .iter()
                .filter(|m| {
                    matches!(m.style, Style::Link(_))
                        && m.range.start <= pos.offset
                        && pos.offset <= m.range.end
                })
                .map(|m| (pos.block, m.range.clone()))
                .collect()
        } else {
            self.selected_ranges()
        };
        for (index, range) in ranges {
            let block = &mut self.blocks[index];
            let links: Vec<Style> = block
                .marks
                .iter()
                .filter(|m| matches!(m.style, Style::Link(_)))
                .map(|m| m.style.clone())
                .collect();
            for link in links {
                block.remove_mark(range.clone(), &link);
            }
        }
        self.changed();
    }

    /// Links every selected run to `url`, replacing any link already there
    /// (⌘K's editor). Code and titles cannot hold links.
    pub fn set_link(&mut self, url: &str, now_ms: u64) {
        if self.selection.is_collapsed() {
            return;
        }
        self.checkpoint(EditKind::Other, now_ms);
        for (index, range) in self.selected_ranges() {
            let block = &mut self.blocks[index];
            if matches!(block.kind, BlockKind::Title | BlockKind::Code) {
                continue;
            }
            let range = trim_range(&block.text, range);
            block.add_mark(range, Style::Link(url.to_owned()));
        }
        self.changed();
    }

    /// The link at `pos` as (block, range, url): a caret inside it or at
    /// either end of it.
    pub fn link_at(&self, pos: Pos) -> Option<(usize, Range<usize>, String)> {
        self.blocks
            .get(pos.block)?
            .marks
            .iter()
            .find_map(|m| match &m.style {
                Style::Link(url) if m.range.start <= pos.offset && pos.offset <= m.range.end => {
                    Some((pos.block, m.range.clone(), url.clone()))
                }
                _ => None,
            })
    }

    /// The link under the caret, if any.
    pub fn link_at_caret(&self) -> Option<String> {
        let pos = self.selection.head;
        self.blocks[pos.block]
            .marks
            .iter()
            .find_map(|m| match &m.style {
                Style::Link(url) if m.range.start <= pos.offset && pos.offset <= m.range.end => {
                    Some(url.clone())
                }
                _ => None,
            })
    }

    /// Styles the next typed character receives — for toolbar state.
    pub fn active_styles(&self) -> Vec<Style> {
        if let Some(pending) = &self.pending {
            return pending.clone();
        }
        let pos = self.selection.head;
        if self.selection.is_collapsed() {
            return self.blocks[pos.block].styles_at(pos.offset);
        }
        let ranges = self.selected_ranges();
        [Style::Bold, Style::Italic, Style::Strike, Style::Code]
            .into_iter()
            .filter(|style| {
                ranges
                    .iter()
                    .filter(|(_, r)| !r.is_empty())
                    .all(|(i, r)| self.blocks[*i].has_style(r.clone(), style))
            })
            .collect()
    }

    /// Per-block byte ranges covered by the selection.
    pub fn selected_ranges(&self) -> Vec<(usize, Range<usize>)> {
        let start = self.selection.start();
        let end = self.selection.end();
        (start.block..=end.block)
            .map(|index| {
                let len = self.blocks[index].text.len();
                let from = if index == start.block {
                    start.offset
                } else {
                    0
                };
                let to = if index == end.block { end.offset } else { len };
                (index, from..to)
            })
            .collect()
    }

    /// Moves the selected blocks up or down (⌥⇧↑/↓).
    pub fn move_blocks(&mut self, up: bool, now_ms: u64) {
        let mut range = self.selected_blocks();
        if range.is_empty() {
            return;
        }
        // A selection in a table moves the table; a table next to what moves
        // is stepped over whole.
        if let Some(table) = self.table_pos(range.start) {
            range.start = table.range.start;
        }
        if let Some(table) = self.table_pos(range.end - 1) {
            range.end = range.end.max(table.range.end);
        }
        let span = |index: usize| {
            self.table_pos(index)
                .map_or(index..index + 1, |table| table.range)
        };
        let last_body = self.blocks.len() - 1;
        if (up && range.start <= 1) || (!up && range.end > last_body) {
            return;
        }
        let neighbor = if up {
            span(range.start - 1)
        } else {
            span(range.end)
        };
        if up && neighbor.start < 1 {
            return;
        }
        self.checkpoint(EditKind::Other, now_ms);
        let by = neighbor.len();
        if up {
            self.blocks[neighbor.start..range.end].rotate_left(by);
        } else {
            self.blocks[range.start..neighbor.end].rotate_right(by);
        }
        let shift = |pos: Pos| {
            if up {
                Pos::new(pos.block - by, pos.offset)
            } else {
                Pos::new(pos.block + by, pos.offset)
            }
        };
        self.selection = Selection {
            anchor: shift(self.selection.anchor),
            head: shift(self.selection.head),
        };
        self.changed();
    }

    // -----------------------------------------------------------------------
    // Clipboard

    /// The selection as Markdown (what ⌘C puts on the pasteboard).
    pub fn selected_markdown(&self) -> String {
        if self.selection.is_collapsed() {
            return String::new();
        }
        let ranges = self.selected_ranges();
        if ranges.len() == 1 {
            let (index, range) = &ranges[0];
            let block = &self.blocks[*index];
            let mut piece = block.clone();
            let tail = piece.split_off(range.end, 0, piece.kind);
            drop(tail);
            let piece = piece.split_off(range.start, 0, piece.kind);
            if block.kind == BlockKind::Code || block.kind == BlockKind::Title {
                return piece.text;
            }
            return markdown::write_inline(&piece, false);
        }
        let mut blocks = Vec::new();
        let mut copied_to = 0;
        for (index, range) in ranges {
            // A table the selection touches is copied whole: a table cut to
            // some of its cells has no Markdown form.
            if let Some(table) = self.table_pos(index) {
                if index >= copied_to {
                    blocks.extend(self.blocks[table.range.clone()].iter().cloned());
                    copied_to = table.range.end;
                }
                continue;
            }
            let mut piece = self.blocks[index].clone();
            piece.split_off(range.end, 0, piece.kind);
            let mut piece = piece.split_off(range.start, 0, piece.kind);
            if piece.kind == BlockKind::Title {
                piece.kind = BlockKind::Heading(1);
            }
            blocks.push(piece);
        }
        let doc = Document::new("", blocks);
        markdown::write(&markdown::FrontMatter::default(), &doc)
            .trim_end()
            .to_owned()
    }

    /// Pastes text, reading Markdown structure when it spans lines.
    /// Pastes text that is a table: spreadsheet rows (TSV or CSV) or a
    /// Markdown table, into the table at the caret or as a new table.
    /// Returns where the rows came from, or `None` (nothing changed) when
    /// the text is not a table. A single spreadsheet row only counts in a
    /// table or on an empty line, where a stray tab is not meant as text.
    pub fn paste_table(&mut self, text: &str, now_ms: u64) -> Option<&'static str> {
        let text = text.replace("\r\n", "\n");
        let head = &self.blocks[self.selection.head.block];
        let lone_row_ok = head.kind.is_cell() || head.text.is_empty();
        if let Some((source, rows)) = parse_rows(&text)
            && (rows.len() >= 2 || lone_row_ok)
        {
            self.paste_rows(&rows, now_ms);
            return Some(source.name());
        }
        let (_, parsed) = markdown::parse(&format!("\n{text}"));
        let only_table = !parsed.blocks.is_empty()
            && parsed.blocks.iter().all(|b| b.kind.is_cell())
            && parsed
                .blocks
                .iter()
                .filter(|b| b.kind.starts_table())
                .count()
                == 1;
        if !only_table {
            return None;
        }
        let cols = usize::from(parsed.blocks[0].kind.cell()?.cols.max(1));
        let rows: Vec<Vec<String>> = parsed
            .blocks
            .chunks(cols)
            .map(|row| row.iter().map(|b| b.text.clone()).collect())
            .collect();
        if self.caret_table().is_some() {
            self.paste_rows(&rows, now_ms);
        } else {
            // A new table keeps the pasted marks and alignments.
            self.checkpoint(EditKind::Other, now_ms);
            self.delete_selection_inner();
            let mut cells = parsed.blocks;
            for cell in &mut cells {
                cell.id = self.fresh_id();
            }
            let at = self.block_slot_after_caret_for_paste();
            let len = cells.len();
            self.blocks.splice(at..at, cells);
            if self.blocks.get(at + len).is_none_or(|b| b.kind.is_cell()) {
                let id = self.fresh_id();
                self.blocks
                    .insert(at + len, Block::new(id, BlockKind::Paragraph, ""));
            }
            self.selection = Selection::caret(self.end_of(at + len - 1));
            self.changed();
        }
        Some("markdown")
    }

    pub fn paste(&mut self, text: &str, now_ms: u64) {
        let text = text.replace("\r\n", "\n");
        // A cell holds one line: pasted lines join with spaces.
        if self.blocks[self.selection.start().block].kind.is_cell() && text.contains('\n') {
            let joined: Vec<&str> = text
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .collect();
            self.paste(&joined.join(" "), now_ms);
            return;
        }
        let head_kind = self.blocks[self.selection.start().block].kind;
        if !text.contains('\n') || head_kind == BlockKind::Code {
            self.checkpoint(EditKind::Other, now_ms);
            self.delete_selection_inner();
            self.leave_atomic();
            let pos = self.selection.head;
            let inserted = if head_kind == BlockKind::Title {
                text.replace('\n', " ")
            } else {
                text.clone()
            };
            let styles = if head_kind == BlockKind::Code {
                Vec::new()
            } else {
                self.blocks[pos.block].styles_at(pos.offset)
            };
            self.blocks[pos.block].replace(pos.offset..pos.offset, &inserted, &styles);
            self.selection = Selection::caret(Pos::new(pos.block, pos.offset + inserted.len()));
            self.changed();
            return;
        }
        self.checkpoint(EditKind::Other, now_ms);
        self.delete_selection_inner();
        let (_, parsed) = markdown::parse(&format!("\n{text}"));
        let mut pasted: Vec<Block> = parsed.blocks;
        if pasted.is_empty() {
            self.changed();
            return;
        }
        // Pasted tables go in whole, as blocks of their own: no cell merges
        // into the caret's line.
        if pasted.iter().any(|b| b.kind.is_cell()) {
            for block in &mut pasted {
                block.id = self.fresh_id();
            }
            let at = self.block_slot_after_caret_for_paste();
            let len = pasted.len();
            self.blocks.splice(at..at, pasted);
            if self.blocks.get(at + len).is_none_or(|b| b.kind.is_cell()) {
                let id = self.fresh_id();
                self.blocks
                    .insert(at + len, Block::new(id, BlockKind::Paragraph, ""));
            }
            self.selection = Selection::caret(self.end_of(at + len - 1));
            self.changed();
            return;
        }
        let pos = self.selection.head;
        let tail_id = self.fresh_id();
        let current_kind = self.blocks[pos.block].kind;
        let tail = self.blocks[pos.block].split_off(pos.offset, tail_id, current_kind);
        let first = pasted.remove(0);
        {
            let head = &mut self.blocks[pos.block];
            if head.text.is_empty() && head.kind == BlockKind::Paragraph {
                head.set_kind(first.kind);
                head.indent = first.indent;
            }
            if head.kind == BlockKind::Title {
                let at = head.text.len();
                head.replace(at..at, &first.text.replace('\n', " "), &[]);
            } else {
                head.append(&first);
            }
        }
        let mut index = pos.block;
        for mut block in pasted {
            block.id = self.fresh_id();
            index += 1;
            self.blocks.insert(index, block);
        }
        let caret = self.end_of(index);
        if !tail.text.is_empty() {
            let last = &mut self.blocks[index];
            if last.kind == BlockKind::Title {
                let at = last.text.len();
                last.replace(at..at, &tail.text, &[]);
            } else {
                last.append(&tail);
            }
        }
        self.selection = Selection::caret(caret);
        self.changed();
    }
}

fn grapheme_step(text: &str, offset: usize, forward: bool) -> usize {
    if forward {
        text[offset..]
            .grapheme_indices(true)
            .nth(1)
            .map_or(text.len(), |(i, _)| offset + i)
    } else {
        text[..offset]
            .grapheme_indices(true)
            .next_back()
            .map_or(0, |(i, _)| i)
    }
}

/// macOS ⌥-arrow: skip whitespace/punctuation, then a word.
fn word_step(text: &str, offset: usize, forward: bool) -> usize {
    let is_word = |s: &str| s.chars().any(char::is_alphanumeric);
    if forward {
        let mut seen_word = false;
        for (i, piece) in text[offset..].split_word_bound_indices() {
            if is_word(piece) {
                seen_word = true;
            } else if seen_word {
                return offset + i;
            }
        }
        text.len()
    } else {
        let mut seen_word = false;
        for (i, piece) in text[..offset].split_word_bound_indices().rev() {
            if is_word(piece) {
                seen_word = true;
            } else if seen_word {
                return i + piece.len();
            }
        }
        0
    }
}

/// Shrinks a range to exclude edge whitespace (Markdown cannot style it).
fn trim_range(text: &str, range: Range<usize>) -> Range<usize> {
    let slice = &text[range.clone()];
    let lead = slice.len() - slice.trim_start().len();
    let trail = slice.len() - slice.trim_end().len();
    if lead + trail >= slice.len() {
        return range;
    }
    range.start + lead..range.end - trail
}

/// Marks exposed for renderers that want the raw spans.
pub fn marks_in(block: &Block) -> &[Mark] {
    &block.marks
}

#[cfg(test)]
mod tests {
    use super::*;

    fn editor(md: &str) -> Editor {
        let (_, doc) = markdown::parse(md);
        Editor::new(&doc)
    }

    fn type_str(e: &mut Editor, text: &str) {
        for ch in text.chars() {
            e.insert_text(&ch.to_string(), 0);
        }
    }

    fn kinds(e: &Editor) -> Vec<BlockKind> {
        e.blocks().iter().map(|b| b.kind).collect()
    }

    fn body_md(e: &Editor) -> String {
        markdown::write(&markdown::FrontMatter::default(), &e.document())
    }

    #[test]
    fn new_note_starts_in_the_title_and_enter_moves_to_the_body() {
        let mut e = Editor::new(&Document::default());
        assert_eq!(e.selection.head, Pos::new(0, 0));
        type_str(&mut e, "Plan");
        e.enter(0);
        type_str(&mut e, "hello");
        assert_eq!(body_md(&e), "# Plan\n\nhello\n");
    }

    #[test]
    fn markdown_shortcuts_turn_blocks() {
        let mut e = editor("# T\n");
        let cases: &[(&str, BlockKind)] = &[
            ("# ", BlockKind::Heading(1)),
            ("## ", BlockKind::Heading(2)),
            ("- ", BlockKind::Bullet),
            ("1. ", BlockKind::Numbered),
            ("[] ", BlockKind::Todo { checked: false }),
            ("> ", BlockKind::Quote),
        ];
        for (prefix, kind) in cases {
            let last = e.blocks().len() - 1;
            e.set_caret(Pos::new(last, 0));
            type_str(&mut e, prefix);
            assert_eq!(e.block(last).kind, *kind, "{prefix}");
            assert_eq!(e.block(last).text, "");
            type_str(&mut e, "x");
            e.enter(0);
            e.enter(0); // leaves lists; plain paragraphs split
            let last = e.blocks().len() - 1;
            if e.block(last).kind != BlockKind::Paragraph {
                e.backspace(Granularity::Grapheme, 0);
            }
        }
        // "- " then "[] " makes a to-do too.
        let last = e.blocks().len() - 1;
        e.set_caret(Pos::new(last, 0));
        type_str(&mut e, "- [] ");
        assert_eq!(e.block(last).kind, BlockKind::Todo { checked: false });
    }

    #[test]
    fn enter_continues_lists_and_leaves_them_when_empty() {
        let mut e = editor("# T\n\n- [x] done\n");
        e.set_caret(Pos::new(1, 4));
        e.enter(0);
        assert_eq!(e.block(2).kind, BlockKind::Todo { checked: false });
        type_str(&mut e, "next");
        e.enter(0);
        e.enter(0);
        assert_eq!(e.block(3).kind, BlockKind::Paragraph);
        assert_eq!(body_md(&e), "# T\n\n- [x] done\n- [ ] next\n");
    }

    #[test]
    fn backspace_unwinds_formatting_before_merging() {
        let mut e = editor("# T\n\nabc\n\n  - child\n");
        // indent 1 bullet
        e.set_caret(Pos::new(2, 0));
        assert_eq!(e.block(2).indent, 1);
        e.backspace(Granularity::Grapheme, 0);
        assert_eq!((e.block(2).kind, e.block(2).indent), (BlockKind::Bullet, 0));
        e.backspace(Granularity::Grapheme, 0);
        assert_eq!(e.block(2).kind, BlockKind::Paragraph);
        e.backspace(Granularity::Grapheme, 0);
        assert_eq!(e.block(1).text, "abcchild");
        assert_eq!(e.selection.head, Pos::new(1, 3));
    }

    #[test]
    fn inline_autoformat_marks_and_stops() {
        let mut e = editor("# T\n");
        e.set_caret(Pos::new(1, 0));
        type_str(&mut e, "a **bold** and `code` and *it* x");
        let block = e.block(1);
        assert_eq!(block.text, "a bold and code and it x");
        assert_eq!(
            block.marks,
            vec![
                Mark {
                    range: 2..6,
                    style: Style::Bold
                },
                Mark {
                    range: 11..15,
                    style: Style::Code
                },
                Mark {
                    range: 20..22,
                    style: Style::Italic
                },
            ]
        );
        // Arithmetic is left alone.
        let mut e = editor("# T\n");
        e.set_caret(Pos::new(1, 0));
        type_str(&mut e, "2*3*4");
        assert!(e.block(1).marks.is_empty());
    }

    #[test]
    fn divider_and_code_fence_shortcuts() {
        let mut e = editor("# T\n");
        e.set_caret(Pos::new(1, 0));
        type_str(&mut e, "---");
        assert_eq!(e.block(1).kind, BlockKind::Divider);
        assert_eq!(e.selection.head, Pos::new(2, 0));
        type_str(&mut e, "```");
        assert_eq!(e.block(2).kind, BlockKind::Code);
        type_str(&mut e, "let x;");
        e.enter(0);
        type_str(&mut e, "y");
        e.enter(0);
        e.enter(0);
        assert_eq!(e.block(2).text, "let x;\ny");
        assert_eq!(e.block(3).kind, BlockKind::Paragraph);
    }

    #[test]
    fn cross_block_selection_delete_and_copy() {
        let mut e = editor("# T\n\n- one\n- two\n- three\n");
        e.set_selection(Selection {
            anchor: Pos::new(1, 1),
            head: Pos::new(3, 2),
        });
        assert_eq!(e.selected_markdown(), "- ne\n- two\n- th");
        e.delete_selection(0);
        assert_eq!(e.block(1).text, "oree");
        assert_eq!(kinds(&e)[1..], [BlockKind::Bullet, BlockKind::Paragraph]);
    }

    #[test]
    fn paste_markdown_splits_into_blocks() {
        let mut e = editor("# T\n\nhead tail\n");
        e.set_caret(Pos::new(1, 5));
        e.paste("**x**\n- [ ] a\n- b", 0);
        assert_eq!(body_md(&e), "# T\n\nhead **x**\n\n- [ ] a\n- btail\n");
        assert_eq!(e.selection.head, Pos::new(3, 1));
    }

    #[test]
    fn undo_coalesces_typing_and_restores_structure() {
        let mut e = editor("# T\n");
        e.set_caret(Pos::new(1, 0));
        for (i, ch) in "hello".chars().enumerate() {
            e.insert_text(&ch.to_string(), i as u64 * 100);
        }
        e.enter(600);
        e.insert_text("x", 700);
        assert!(e.undo()); // "x"
        assert!(e.undo()); // enter
        assert_eq!(e.blocks().len(), 2);
        assert!(e.undo()); // "hello"
        assert_eq!(e.block(1).text, "");
        assert!(e.redo());
        assert_eq!(e.block(1).text, "hello");
    }

    #[test]
    fn tab_nests_under_the_previous_item_only() {
        let mut e = editor("# T\n\n- a\n- b\n");
        e.set_caret(Pos::new(1, 0));
        e.indent(false, 0);
        assert_eq!(e.block(1).indent, 0, "first item cannot nest");
        e.set_caret(Pos::new(2, 0));
        e.indent(false, 0);
        e.indent(false, 0);
        assert_eq!(e.block(2).indent, 1);
    }

    #[test]
    fn toggle_style_on_a_selection_and_pending_at_caret() {
        let mut e = editor("# T\n\nhello world\n");
        e.set_selection(Selection {
            anchor: Pos::new(1, 0),
            head: Pos::new(1, 6),
        });
        e.toggle_style(Style::Bold, 0);
        assert_eq!(
            e.block(1).marks,
            vec![Mark {
                range: 0..5,
                style: Style::Bold
            }]
        );
        e.set_caret(Pos::new(1, 11));
        e.toggle_style(Style::Italic, 0);
        type_str(&mut e, "!");
        assert!(e.block(1).has_style(11..12, &Style::Italic));
    }

    #[test]
    fn move_blocks_keeps_the_title_first() {
        let mut e = editor("# T\n\n- a\n- b\n");
        e.set_caret(Pos::new(1, 0));
        e.move_blocks(true, 0);
        assert_eq!(e.block(0).kind, BlockKind::Title);
        e.move_blocks(false, 0);
        assert_eq!(e.block(2).text, "a");
        assert_eq!(e.selection.head.block, 2);
    }

    #[test]
    fn word_motion_matches_macos() {
        assert_eq!(word_step("hello, world", 0, true), 5);
        assert_eq!(word_step("hello, world", 5, true), 12);
        assert_eq!(word_step("hello, world", 12, false), 7);
        assert_eq!(word_step("hello, world", 7, false), 0);
    }

    fn mention_editor() -> Editor {
        let mut e = editor("# T\n\nping");
        e.set_caret(Pos::new(1, 4));
        type_str(&mut e, " @cod");
        e
    }

    #[test]
    fn inserting_a_mention_replaces_the_query_with_a_linked_chip() {
        let mut e = mention_editor();
        let target = MentionTarget::Session("s_1".into());
        e.insert_mention(5..9, &target, "@Codex: fix resize", 0);
        let block = e.block(1);
        assert_eq!(block.text, "ping @Codex: fix resize ");
        assert_eq!(
            block.marks,
            vec![Mark {
                range: 5..23,
                style: Style::Link("diri://session/s_1".into())
            }]
        );
        assert_eq!(e.selection, Selection::caret(Pos::new(1, 24)));
        // Text typed after the chip is not part of it.
        type_str(&mut e, "ok");
        assert_eq!(e.block(1).marks[0].range, 5..23);
        assert_eq!(
            markdown::write_inline(e.block(1), true),
            "ping [@Codex: fix resize](diri://session/s_1) ok"
        );
        assert!(e.undo());
        assert!(e.undo());
        assert_eq!(e.block(1).text, "ping @cod");
    }

    #[test]
    fn a_mention_chip_deletes_and_steps_as_one_unit() {
        let mut e = mention_editor();
        let target = MentionTarget::Note("n-1".into());
        e.insert_mention(5..9, &target, "@Plan", 0);
        assert_eq!(e.block(1).text, "ping @Plan ");
        // Arrowing left from after the space jumps over the whole chip.
        e.move_horizontal(false, Granularity::Grapheme, false);
        assert_eq!(e.selection.head, Pos::new(1, 10));
        e.move_horizontal(false, Granularity::Grapheme, false);
        assert_eq!(e.selection.head, Pos::new(1, 5));
        e.move_horizontal(true, Granularity::Grapheme, false);
        assert_eq!(e.selection.head, Pos::new(1, 10));
        e.backspace(Granularity::Grapheme, 0);
        assert_eq!(e.block(1).text, "ping  ");
        assert!(e.block(1).marks.is_empty());
        assert!(e.document().mentions().is_empty());

        let mut e = mention_editor();
        e.insert_mention(5..9, &target, "@Plan", 0);
        e.set_caret(Pos::new(1, 5));
        e.delete_forward(Granularity::Grapheme, 0);
        assert_eq!(e.block(1).text, "ping  ");
        assert_eq!(e.selection.head, Pos::new(1, 5));
    }

    #[test]
    fn mentions_are_refused_in_titles_and_code() {
        let mut e = editor("# T\n\n```\n@x\n```\n");
        e.set_caret(Pos::new(0, 1));
        e.insert_mention(0..1, &MentionTarget::Note("n".into()), "@n", 0);
        assert_eq!(e.title(), "T");
        e.set_caret(Pos::new(1, 2));
        e.insert_mention(0..2, &MentionTarget::Note("n".into()), "@n", 0);
        assert_eq!(e.block(1).text, "@x");
    }

    fn outline() -> Editor {
        editor(
            "# T\n\n- parent\n  - child one\n    - grandchild\n  - child two\n- sibling\n\nafter\n",
        )
    }

    fn texts_visible(e: &Editor) -> Vec<String> {
        let hidden = e.hidden();
        e.blocks()
            .iter()
            .enumerate()
            .filter(|(i, _)| !hidden[*i])
            .map(|(_, b)| b.text.clone())
            .collect()
    }

    #[test]
    fn children_are_the_run_of_deeper_list_items() {
        let e = outline();
        assert_eq!(e.children(1), 2..5);
        assert_eq!(e.children(2), 3..4);
        assert!(e.children(3).is_empty());
        assert!(e.children(5).is_empty());
        assert!(e.children(6).is_empty(), "paragraphs never have children");
    }

    #[test]
    fn folding_hides_the_subtree_and_is_not_an_edit() {
        let mut e = outline();
        let revision = e.revision;
        e.set_caret(Pos::new(3, 2));
        e.set_collapsed(1, true);
        assert!(e.is_collapsed(1));
        assert_eq!(texts_visible(&e), ["T", "parent", "sibling", "after"]);
        // The caret was inside the fold: it lands on the folded item.
        assert_eq!(e.selection, Selection::caret(Pos::new(1, 6)));
        assert_eq!(e.revision, revision);
        assert!(!e.can_undo());
        // Items without children cannot fold.
        e.set_collapsed(5, true);
        assert!(!e.is_collapsed(5));
        e.toggle_collapsed(1);
        assert_eq!(texts_visible(&e).len(), e.blocks().len());
    }

    #[test]
    fn arrows_skip_a_folded_subtree() {
        let mut e = outline();
        e.set_collapsed(1, true);
        e.set_caret(Pos::new(1, 6));
        e.move_horizontal(true, Granularity::Grapheme, false);
        assert_eq!(e.selection.head, Pos::new(5, 0));
        e.move_horizontal(false, Granularity::Grapheme, false);
        assert_eq!(e.selection.head, Pos::new(1, 6));
    }

    #[test]
    fn return_on_a_folded_item_adds_a_sibling_after_its_subtree() {
        let mut e = outline();
        e.set_collapsed(1, true);
        e.set_caret(Pos::new(1, 6));
        e.enter(0);
        assert_eq!(e.selection.head, Pos::new(5, 0));
        assert_eq!(e.block(5).kind, BlockKind::Bullet);
        assert_eq!(e.block(5).indent, 0);
        assert!(e.is_collapsed(1), "the fold stays");
        assert_eq!(e.block(4).text, "child two");
    }

    #[test]
    fn a_caret_placed_inside_a_fold_unfolds_it() {
        let mut e = outline();
        e.set_collapsed(2, true);
        e.set_collapsed(1, true);
        e.set_caret(Pos::new(3, 0));
        assert!(!e.is_collapsed(1) && !e.is_collapsed(2));
        assert!(!e.is_hidden(3));
    }

    #[test]
    fn a_folded_item_moves_with_its_subtree() {
        let mut e = outline();
        e.set_collapsed(1, true);
        e.set_caret(Pos::new(1, 0));
        e.move_blocks(false, 0);
        let texts: Vec<&str> = e.blocks().iter().map(|b| b.text.as_str()).collect();
        assert_eq!(
            texts,
            [
                "T",
                "sibling",
                "parent",
                "child one",
                "grandchild",
                "child two",
                "after"
            ]
        );
        assert!(e.is_collapsed(2));
    }

    #[test]
    fn absorbing_an_outside_append_keeps_the_caret_and_is_one_undo_step() {
        let base = crate::markdown::parse("# Plan\n\nIntro\n\n- [ ] venue\n").1;
        let mut e = Editor::new(&base);
        // The person is typing at the end of "Intro".
        e.set_caret(Pos::new(1, 5));
        e.insert_text(" and more", 1_000);
        let theirs =
            crate::markdown::parse("# Plan\n\nIntro\n\n- [x] venue\n\n- agent: booked\n").1;
        let merged = crate::merge::merge3(&base, &e.document(), &theirs);
        assert!(e.absorb(merged, 2_000));
        assert_eq!(e.block(1).text, "Intro and more");
        assert_eq!(e.block(2).kind, BlockKind::Todo { checked: true });
        assert!(e.blocks().iter().any(|b| b.text == "agent: booked"));
        assert_eq!(
            e.selection,
            Selection::caret(Pos::new(1, 14)),
            "caret stays in the typing"
        );
        // Typing continues where it was.
        e.insert_text("!", 3_000);
        assert_eq!(e.block(1).text, "Intro and more!");
        // Undo peels the typing, then the outside write, then the earlier typing.
        e.undo();
        e.undo();
        assert_eq!(e.block(1).text, "Intro and more");
        assert!(!e.blocks().iter().any(|b| b.text == "agent: booked"));
    }

    #[test]
    fn a_pasted_tool_url_becomes_a_titled_link() {
        let mut e = editor("# T\n\nsee ");
        e.set_caret(Pos::new(1, 3));
        e.insert_text(" ", 0);
        e.paste_url("https://linear.app/acme/issue/ENG-7/ship-notes", 0);
        assert_eq!(e.block(1).text, "see ENG-7 Ship notes");
        assert_eq!(
            markdown::write_inline(e.block(1), true),
            "see [ENG-7 Ship notes](https://linear.app/acme/issue/ENG-7/ship-notes)"
        );
        // Typing after it is plain text.
        e.insert_text("!", 0);
        assert_eq!(e.block(1).marks.len(), 1);
        e.paste_url("https://diri.sh", 0);
        assert!(e.block(1).text.ends_with("!https://diri.sh"));
        assert!(e.undo());
        assert_eq!(e.block(1).text, "see ENG-7 Ship notes!");
    }

    #[test]
    fn set_link_replaces_a_link_and_link_at_finds_it() {
        let mut e = editor("# T\n\nread [the brief](https://a.dev) today\n");
        let (block, range, url) = e.link_at(Pos::new(1, 7)).expect("caret in link");
        assert_eq!(
            (block, range.clone(), url.as_str()),
            (1, 5..14, "https://a.dev")
        );
        e.set_selection(Selection {
            anchor: Pos::new(1, range.start),
            head: Pos::new(1, range.end),
        });
        e.set_link("https://docs.google.com/document/d/x", 0);
        assert_eq!(
            markdown::write_inline(e.block(1), true),
            "read [the brief](https://docs.google.com/document/d/x) today"
        );
        assert!(e.undo());
        assert_eq!(e.link_at(Pos::new(1, 7)).unwrap().2, "https://a.dev");
        assert_eq!(e.link_at(Pos::new(1, 2)), None);
    }

    #[test]
    fn images_insert_select_and_step_aside_for_typing() {
        let mut e = editor("# T\n\nabove\n\nbelow\n");
        e.set_caret(Pos::new(1, 5));
        e.insert_image("assets/n/1.png", "chart", 0);
        assert_eq!(e.block(2).kind, BlockKind::Image);
        assert_eq!(e.block(2).src, "assets/n/1.png");
        // The caret lands on a fresh line below the image.
        assert_eq!(e.selection.head, Pos::new(3, 0));
        assert_eq!(e.block(3).text, "");
        assert_eq!(e.block(4).text, "below");
        // Typing with the image selected goes below it, not into it.
        e.set_caret(Pos::new(2, 0));
        type_str(&mut e, "x");
        assert_eq!(e.block(2).text, "chart");
        assert_eq!(e.block(3).text, "x");
        // Backspace on a selected image removes it.
        e.set_caret(Pos::new(2, 0));
        e.backspace(Granularity::Grapheme, 0);
        assert!(e.blocks().iter().all(|b| b.kind != BlockKind::Image));
        assert!(e.undo());
        assert_eq!(e.block(2).kind, BlockKind::Image);
        // Slash-menu conversions skip images.
        e.set_caret(Pos::new(2, 0));
        e.turn_into(Turn::Kind(BlockKind::Heading(1)), 0);
        assert_eq!(e.block(2).kind, BlockKind::Image);
    }

    #[test]
    fn an_image_replaces_an_empty_line() {
        let mut e = editor("# T\n\nabove\n\n&nbsp;\n");
        let last = e.blocks().len() - 1;
        e.set_caret(Pos::new(last, 0));
        e.insert_image("a.png", "", 0);
        assert_eq!(e.block(last).kind, BlockKind::Image);
        assert_eq!(e.block(last + 1).kind, BlockKind::Paragraph);
        assert_eq!(
            markdown::write(&markdown::FrontMatter::default(), &e.document()),
            "# T\n\nabove\n\n![](a.png)\n"
        );
    }

    #[test]
    fn callouts_take_soft_breaks_and_leave_on_return() {
        use crate::doc::Tone;
        let mut e = editor("# T\n\nheads up\n");
        e.set_caret(Pos::new(1, 8));
        e.turn_into(Turn::Kind(BlockKind::Callout(Tone::Warning)), 0);
        e.soft_break(0);
        type_str(&mut e, "second line");
        e.enter(0);
        type_str(&mut e, "after");
        assert_eq!(e.block(1).kind, BlockKind::Callout(Tone::Warning));
        assert_eq!(e.block(1).text, "heads up\nsecond line");
        assert_eq!(e.block(2).kind, BlockKind::Paragraph);
        assert_eq!(
            markdown::write(&markdown::FrontMatter::default(), &e.document()),
            "# T\n\n> [!WARNING]\n> heads up\n> second line\n\nafter\n"
        );
    }

    fn cell_texts(e: &Editor) -> Vec<String> {
        e.blocks()
            .iter()
            .filter(|b| b.kind.is_cell())
            .map(|b| b.text.clone())
            .collect()
    }

    fn md(e: &Editor) -> String {
        markdown::write(&markdown::FrontMatter::default(), &e.document())
    }

    #[test]
    fn slash_table_inserts_a_three_by_three_table_on_the_empty_line() {
        let mut e = editor("# T\n\nabove\n\n&nbsp;\n\nbelow\n");
        e.set_caret(Pos::new(2, 0));
        e.insert_empty_table(3, 3, 0);
        assert_eq!(cell_texts(&e).len(), 9);
        let table = e.caret_table().unwrap();
        assert_eq!((table.row, table.col, table.rows, table.cols), (0, 0, 3, 3));
        assert_eq!(e.block(1).text, "above");
        assert_eq!(e.block(table.range.end).text, "below");
        assert!(e.undo());
        assert!(cell_texts(&e).is_empty());
    }

    #[test]
    fn tab_moves_through_cells_and_adds_a_row_at_the_end() {
        let mut e = editor("# T\n\n");
        e.set_caret(Pos::new(1, 0));
        e.insert_empty_table(2, 2, 0);
        for word in ["Metric", "Owner", "CTR", "Ana"] {
            type_str(&mut e, word);
            assert!(e.table_tab(true, 0));
        }
        // Tab in the last cell added a row, caret in its first cell.
        let table = e.caret_table().unwrap();
        assert_eq!((table.rows, table.row, table.col), (3, 2, 0));
        assert!(e.table_tab(false, 0));
        assert_eq!(e.caret_table().unwrap().row, 1);
        assert_eq!(e.selection.head.offset, 3, "caret at the end of the cell");
        assert_eq!(
            md(&e),
            "# T\n\n| Metric | Owner |\n| ------ | ----- |\n| CTR    | Ana   |\n|        |       |\n"
        );
    }

    #[test]
    fn return_adds_a_row_and_shift_return_does_nothing_in_a_cell() {
        let mut e = editor("# T\n\n| a | b |\n| - | - |\n| 1 | 2 |\n");
        e.set_caret(Pos::new(4, 1));
        e.soft_break(0);
        assert_eq!(e.block(4).text, "2");
        e.enter(0);
        let table = e.caret_table().unwrap();
        assert_eq!((table.rows, table.row, table.col), (3, 2, 1));
        assert!(e.undo());
        assert_eq!(e.caret_table().unwrap().rows, 2);
    }

    #[test]
    fn rows_columns_alignment_and_deletion_are_single_undo_steps() {
        use crate::doc::Align;
        let mut e = editor("# T\n\n| a | b |\n| - | - |\n| 1 | 2 |\n\nafter\n");
        e.set_caret(Pos::new(1, 1)); // header "a"
        assert!(e.table_add_col(true, 0));
        type_str(&mut e, "new");
        assert!(e.table_set_align(Align::Right, 0));
        assert_eq!(
            md(&e),
            "# T\n\n| a   | new | b   |\n| --- | --: | --- |\n| 1   |     | 2   |\n\nafter\n"
        );
        assert!(e.table_add_row(false, 0));
        assert_eq!(e.caret_table().unwrap().rows, 3);
        // The new top row is the header now.
        assert!(e.block(2).kind.cell().unwrap().header);
        assert!(e.table_delete_row(0));
        assert!(e.table_delete_col(0));
        assert_eq!(e.caret_table().unwrap().cols, 2);
        assert!(e.table_delete(0));
        assert!(cell_texts(&e).is_empty());
        assert_eq!(e.block(e.selection.head.block).text, "after");
        // Undo walks back through each step.
        for _ in 0..5 {
            assert!(e.undo());
        }
        assert_eq!(cell_texts(&e), ["a", "new", "b", "1", "", "2"]);
    }

    #[test]
    fn backspace_and_delete_never_merge_cells() {
        let mut e = editor("# T\n\n| a | b |\n| - | - |\n| 1 | 2 |\n\n&nbsp;\n\nafter\n");
        // Cells: a=1, b=2, 1=3, 2=4; the empty line is 5, "after" 6.
        e.set_caret(Pos::new(3, 0));
        e.backspace(Granularity::Grapheme, 0);
        assert_eq!(cell_texts(&e), ["a", "b", "1", "2"]);
        assert_eq!(
            e.selection.head,
            Pos::new(2, 1),
            "stepped back to the end of b"
        );
        e.set_caret(Pos::new(4, 1));
        e.delete_forward(Granularity::Grapheme, 0);
        assert_eq!(cell_texts(&e), ["a", "b", "1", "2"]);
        // An empty line after the table goes, the caret lands in the table.
        e.set_caret(Pos::new(5, 0));
        e.backspace(Granularity::Grapheme, 0);
        assert_eq!(e.block(5).text, "after");
        assert_eq!(e.selection.head, Pos::new(4, 1));
    }

    #[test]
    fn deleting_a_selection_keeps_tables_whole_or_removes_them_whole() {
        let mut e = editor("# T\n\nbefore\n\n| a | b |\n| - | - |\n| 1 | 2 |\n\nafter\n");
        // From inside a paragraph into the table: cells empty, rows stay.
        e.set_selection(Selection {
            anchor: Pos::new(1, 3),
            head: Pos::new(4, 0),
        });
        e.delete_selection(0);
        assert_eq!(e.block(1).text, "bef");
        assert_eq!(cell_texts(&e), ["", "", "1", "2"]);
        assert!(e.undo());
        // Around the whole table: it goes.
        e.set_selection(Selection {
            anchor: Pos::new(1, 0),
            head: Pos::new(6, 2),
        });
        e.delete_selection(0);
        assert!(cell_texts(&e).is_empty());
        assert_eq!(e.block(1).text, "");
        assert_eq!(e.block(2).text, "ter");
    }

    #[test]
    fn spreadsheet_rows_paste_as_a_table_or_into_one() {
        let mut e = editor("# T\n\n");
        e.set_caret(Pos::new(1, 0));
        let sheet =
            "Channel\tSpend\tCPA\nSearch\t$1,200\t$14\n\"Social, paid\"\t$800\t\"$2\"\"1\"\n";
        assert_eq!(e.paste_table(sheet, 0), Some("tsv"));
        assert_eq!(
            cell_texts(&e),
            [
                "Channel",
                "Spend",
                "CPA",
                "Search",
                "$1,200",
                "$14",
                "Social, paid",
                "$800",
                "$2\"1"
            ]
        );
        // Into the table at a cell: fills right and down, growing it.
        let first_body = e.caret_table().unwrap().range.start + 3;
        e.set_caret(Pos::new(first_body + 1, 0));
        assert_eq!(e.paste_table("a\tb\tc\nd\te\tf\ng\th\ti", 0), Some("tsv"));
        let table = e.caret_table().unwrap();
        assert_eq!((table.rows, table.cols), (4, 4));
        assert_eq!(e.block(table.index(1, 1)).text, "a");
        assert_eq!(e.block(table.index(3, 3)).text, "i");
        assert!(e.undo());
        assert_eq!(e.caret_table().unwrap().cols, 3);
        // CSV needs two or more consistent lines; prose with commas is text.
        let mut f = editor("# T\n\n");
        f.set_caret(Pos::new(1, 0));
        assert_eq!(f.paste_table("Yes, we shipped it, finally.", 0), None);
        assert_eq!(f.paste_table("a,b\n1,2\n", 0), Some("csv"));
        // A Markdown table pastes with its marks.
        let mut g = editor("# T\n\n");
        g.set_caret(Pos::new(1, 0));
        assert_eq!(
            g.paste_table("| **Goal** | Due |\n|---|--:|\n| Launch | Fri |", 0),
            Some("markdown")
        );
        assert_eq!(g.block(1).marks[0].style, Style::Bold);
        assert_eq!(cell_texts(&g), ["Goal", "Due", "Launch", "Fri"]);
    }

    #[test]
    fn copying_inside_a_table_gives_tsv_and_outside_gives_markdown() {
        let mut e = editor(
            "# T\n\nintro\n\n| a | b | c |\n| - | - | - |\n| 1 | 2 | 3 |\n| 4 | 5\t | 6 |\n",
        );
        e.set_selection(Selection {
            anchor: Pos::new(6, 0), // "2"
            head: Pos::new(10, 1),  // "6"
        });
        assert_eq!(e.selection_tsv().as_deref(), Some("2\t3\n5\t6"));
        e.set_selection(Selection {
            anchor: Pos::new(1, 0),
            head: Pos::new(6, 1),
        });
        assert_eq!(e.selection_tsv(), None);
        let copied = e.selected_markdown();
        assert!(copied.starts_with("intro\n\n| a "), "{copied}");
        assert!(
            copied.contains("| 4 "),
            "the table is copied whole:\n{copied}"
        );
    }

    #[test]
    fn moving_blocks_steps_over_a_table_and_moves_it_whole() {
        let mut e = editor("# T\n\none\n\n| a | b |\n| - | - |\n| 1 | 2 |\n\ntwo\n");
        e.set_caret(Pos::new(1, 0));
        e.move_blocks(false, 0);
        let texts: Vec<&str> = e.blocks().iter().map(|b| b.text.as_str()).collect();
        assert_eq!(texts, ["T", "a", "b", "1", "2", "one", "two"]);
        e.set_caret(Pos::new(3, 0));
        e.move_blocks(false, 0);
        let texts: Vec<&str> = e.blocks().iter().map(|b| b.text.as_str()).collect();
        assert_eq!(texts, ["T", "one", "a", "b", "1", "2", "two"]);
    }

    #[test]
    fn an_empty_block_turns_into_a_starter_diagram() {
        let mut e = editor("# T\n\n&nbsp;\n");
        e.set_caret(Pos::new(1, 0));
        e.turn_into(Turn::Diagram, 0);
        assert!(e.block(1).is_diagram());
        assert_eq!(e.block(1).text, DIAGRAM_STARTER);
        assert_eq!(e.selection.head, Pos::new(1, DIAGRAM_STARTER.len()));
        assert!(
            markdown::write(&markdown::FrontMatter::default(), &e.document())
                .contains("```mermaid\nflowchart LR")
        );
    }

    #[test]
    fn code_keeps_another_fence_language() {
        let mut e = editor("# T\n\n```rust title=main.rs\nfn main() {}\n```\n");
        e.set_caret(Pos::new(1, 0));
        e.turn_into(Turn::Kind(BlockKind::Code), 0);
        assert_eq!(e.block(1).info, "rust title=main.rs");
        assert!(
            markdown::write(&markdown::FrontMatter::default(), &e.document())
                .contains("```rust title=main.rs\nfn main() {}")
        );
    }

    #[test]
    fn code_turns_a_diagram_back_into_its_source() {
        let mut e = editor("# T\n\n```mermaid\ngraph TD; A-->B\n```\n");
        assert!(e.block(1).is_diagram());
        e.set_caret(Pos::new(1, 0));
        e.turn_into(Turn::Kind(BlockKind::Code), 0);
        assert_eq!(e.block(1).kind, BlockKind::Code);
        assert!(!e.block(1).is_diagram());
        assert_eq!(e.block(1).text, "graph TD; A-->B");
        // A paragraph keeps the text and drops the fence's language.
        e.turn_into(Turn::Diagram, 0);
        assert!(e.block(1).is_diagram());
        e.turn_into(Turn::Kind(BlockKind::Paragraph), 0);
        assert_eq!(e.block(1).info, "");
        assert!(e.undo());
        assert!(e.block(1).is_diagram());
    }
}
