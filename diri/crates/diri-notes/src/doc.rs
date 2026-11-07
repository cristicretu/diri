//! The rich note model: a title and a flat list of blocks with inline marks.
//!
//! Notes are edited as blocks (Notion-style) and stored as Markdown. Every
//! block owns plain text plus byte-range marks, so an editor never has to
//! interpret Markdown syntax while the user types; `markdown` converts at the
//! file boundary only. Offsets are byte indices on `char` boundaries.

use std::ops::Range;

/// Stable identity for a block while a note is open. Not persisted.
pub type BlockId = u64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockKind {
    /// The note's title while it is open in the editor. Never serialized:
    /// the editor lifts it back into [`Document::title`].
    Title,
    Paragraph,
    /// 1..=3. Deeper Markdown headings clamp to 3.
    Heading(u8),
    Bullet,
    Numbered,
    Todo {
        checked: bool,
    },
    Quote,
    Code,
    Divider,
    /// A picture. `Block::src` is its path (relative to the notes folder) or
    /// URL; `text` is its alt text, which the editor never lays out.
    Image,
    /// A highlighted aside, stored as a GitHub-style alert (`> [!NOTE]`).
    Callout(Tone),
    /// One cell of a table. A table is a run of cells, row by row; its
    /// first cell is column 0 of the header row, which is how two adjacent
    /// tables stay apart. Every cell carries its table's column count and
    /// its column's alignment, kept consistent by the editor.
    Cell(Cell),
}

/// A table cell's place and look. Cells are single-line, as in GFM.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Cell {
    pub col: u16,
    pub cols: u16,
    pub align: Align,
    pub header: bool,
}

/// A table column's alignment: the colons of the delimiter row.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Align {
    #[default]
    None,
    Left,
    Center,
    Right,
}

/// The most columns a table keeps; wider Markdown is cut to this.
pub const MAX_TABLE_COLS: usize = 64;

/// A callout's kind, GitHub's alert set: each has a glyph and a tint.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Tone {
    Note,
    Tip,
    Important,
    Warning,
    Caution,
}

impl Tone {
    pub const ALL: [Self; 5] = [
        Self::Note,
        Self::Tip,
        Self::Important,
        Self::Warning,
        Self::Caution,
    ];

    /// The alert tag Markdown stores: `NOTE` in `> [!NOTE]`.
    pub fn tag(self) -> &'static str {
        match self {
            Self::Note => "NOTE",
            Self::Tip => "TIP",
            Self::Important => "IMPORTANT",
            Self::Warning => "WARNING",
            Self::Caution => "CAUTION",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Note => "Note",
            Self::Tip => "Tip",
            Self::Important => "Important",
            Self::Warning => "Warning",
            Self::Caution => "Caution",
        }
    }

    /// Reads a tag case-insensitively, as GitHub and Obsidian both do.
    pub fn from_tag(tag: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|tone| tone.tag().eq_ignore_ascii_case(tag))
    }
}

impl BlockKind {
    pub fn is_list(self) -> bool {
        matches!(self, Self::Bullet | Self::Numbered | Self::Todo { .. })
    }

    /// Whether Enter on this block continues with a block of the same kind.
    pub fn continues(self) -> bool {
        self.is_list() || matches!(self, Self::Quote)
    }

    pub fn has_text(self) -> bool {
        !self.is_atomic()
    }

    /// Blocks the caret can select but not type into: a divider, an image.
    pub fn is_atomic(self) -> bool {
        matches!(self, Self::Divider | Self::Image)
    }

    pub fn is_cell(self) -> bool {
        matches!(self, Self::Cell(_))
    }

    pub fn cell(self) -> Option<Cell> {
        match self {
            Self::Cell(cell) => Some(cell),
            _ => None,
        }
    }

    /// The first cell of a table: column 0 of its header row.
    pub fn starts_table(self) -> bool {
        matches!(
            self,
            Self::Cell(Cell {
                col: 0,
                header: true,
                ..
            })
        )
    }
}

/// The table containing `index`, as a range of block indices.
pub fn table_range(blocks: &[Block], index: usize) -> Option<std::ops::Range<usize>> {
    blocks.get(index)?.kind.cell()?;
    let mut start = index;
    while !blocks[start].kind.starts_table() {
        if start == 0 || !blocks[start - 1].kind.is_cell() {
            break;
        }
        start -= 1;
    }
    let mut end = index + 1;
    while blocks
        .get(end)
        .is_some_and(|b| b.kind.is_cell() && !b.kind.starts_table())
    {
        end += 1;
    }
    Some(start..end)
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Style {
    Bold,
    Italic,
    Strike,
    Code,
    Link(String),
}

impl Style {
    /// Canonical nesting order, outermost first. Code is innermost because its
    /// content is never escaped.
    pub(crate) fn rank(&self) -> u8 {
        match self {
            Self::Link(_) => 0,
            Self::Bold => 1,
            Self::Italic => 2,
            Self::Strike => 3,
            Self::Code => 4,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mark {
    pub range: Range<usize>,
    pub style: Style,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Block {
    pub id: BlockId,
    pub kind: BlockKind,
    /// List nesting depth; always 0 for non-list blocks.
    pub indent: u8,
    pub text: String,
    /// Sorted by start, non-empty, merged per style.
    pub marks: Vec<Mark>,
    /// An image's path or URL; empty for every other kind.
    pub src: String,
    /// A code block's info string: what follows its opening fence, such as
    /// `mermaid` or `rust`. Empty for every other kind.
    pub info: String,
}

pub const MAX_INDENT: u8 = 6;

/// The info string that makes a code block a diagram.
pub const DIAGRAM_LANG: &str = "mermaid";

impl Block {
    pub fn new(id: BlockId, kind: BlockKind, text: impl Into<String>) -> Self {
        Self {
            id,
            kind,
            indent: 0,
            text: text.into(),
            marks: Vec::new(),
            src: String::new(),
            info: String::new(),
        }
    }

    /// A table cell holding `text`.
    pub fn cell(id: BlockId, cell: Cell, text: impl Into<String>) -> Self {
        Self::new(id, BlockKind::Cell(cell), text)
    }

    /// A code block; `info` is its fence's info string (`mermaid`, `rust`).
    pub fn code(id: BlockId, info: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            info: info.into(),
            ..Self::new(id, BlockKind::Code, text)
        }
    }

    /// A code block's language: the first word of its info string.
    pub fn lang(&self) -> &str {
        if self.kind != BlockKind::Code {
            return "";
        }
        self.info.split_whitespace().next().unwrap_or("")
    }

    /// A code block fenced as `mermaid`, which the editor draws as its
    /// diagram.
    pub fn is_diagram(&self) -> bool {
        self.lang().eq_ignore_ascii_case(DIAGRAM_LANG)
    }

    /// An image block: `src` is a path relative to the notes folder or a URL.
    pub fn image(id: BlockId, src: impl Into<String>, alt: impl Into<String>) -> Self {
        Self {
            src: src.into(),
            ..Self::new(id, BlockKind::Image, alt)
        }
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// Styles active for text inserted at `offset`: a mark extends when the
    /// caret sits inside it or at its end, so typing after bold stays bold.
    /// Code and links do not extend at their end.
    pub fn styles_at(&self, offset: usize) -> Vec<Style> {
        let mut styles: Vec<Style> = self
            .marks
            .iter()
            .filter(|mark| {
                let extends_at_end = !matches!(mark.style, Style::Code | Style::Link(_));
                mark.range.start < offset
                    && (offset < mark.range.end || (extends_at_end && offset == mark.range.end))
            })
            .map(|mark| mark.style.clone())
            .collect();
        styles.sort();
        styles.dedup();
        styles
    }

    /// Replaces `range` with `text`, keeping marks attached to the characters
    /// they covered; inserted text receives exactly `styles`.
    pub fn replace(&mut self, range: Range<usize>, text: &str, styles: &[Style]) {
        let range = self.clamp(range);
        let removed = range.len();
        let inserted = text.len();
        self.text.replace_range(range.clone(), text);
        // A mark keeps covering inserted text only when it surrounds the
        // replaced range; at either edge the explicit `styles` decide.
        for mark in &mut self.marks {
            let start = mark.range.start;
            let end = mark.range.end;
            let new_start = if start < range.start {
                start
            } else if start < range.end {
                range.start + inserted
            } else {
                start - removed + inserted
            };
            let new_end = if end <= range.start {
                end
            } else if end <= range.end {
                range.start
            } else {
                end - removed + inserted
            };
            mark.range = new_start..new_end.max(new_start);
        }
        self.marks.retain(|mark| !mark.range.is_empty());
        if inserted > 0 {
            let inserted_range = range.start..range.start + inserted;
            for style in styles {
                self.add_mark(inserted_range.clone(), style.clone());
            }
        }
        self.normalize();
    }

    pub fn clamp(&self, range: Range<usize>) -> Range<usize> {
        let end = floor_boundary(&self.text, range.end.min(self.text.len()));
        let start = floor_boundary(&self.text, range.start.min(end));
        start..end
    }

    pub fn add_mark(&mut self, range: Range<usize>, style: Style) {
        let range = self.clamp(range);
        if range.is_empty() {
            return;
        }
        if matches!(style, Style::Link(_)) {
            // A range carries at most one link.
            self.remove_style_in(range.clone(), |s| matches!(s, Style::Link(_)));
        }
        self.marks.push(Mark { range, style });
        self.normalize();
    }

    pub fn remove_mark(&mut self, range: Range<usize>, style: &Style) {
        self.remove_style_in(range, |s| s == style);
    }

    fn remove_style_in(&mut self, range: Range<usize>, matches: impl Fn(&Style) -> bool) {
        let mut kept = Vec::with_capacity(self.marks.len() + 1);
        for mark in self.marks.drain(..) {
            if !matches(&mark.style)
                || mark.range.end <= range.start
                || mark.range.start >= range.end
            {
                kept.push(mark);
                continue;
            }
            if mark.range.start < range.start {
                kept.push(Mark {
                    range: mark.range.start..range.start,
                    style: mark.style.clone(),
                });
            }
            if mark.range.end > range.end {
                kept.push(Mark {
                    range: range.end..mark.range.end,
                    style: mark.style,
                });
            }
        }
        self.marks = kept;
        self.normalize();
    }

    /// True when every character of `range` carries `style`.
    pub fn has_style(&self, range: Range<usize>, style: &Style) -> bool {
        if range.is_empty() {
            return false;
        }
        let mut covered = range.start;
        let mut spans: Vec<&Range<usize>> = self
            .marks
            .iter()
            .filter(|mark| &mark.style == style)
            .map(|mark| &mark.range)
            .collect();
        spans.sort_by_key(|span| span.start);
        for span in spans {
            if span.start > covered {
                break;
            }
            covered = covered.max(span.end);
            if covered >= range.end {
                return true;
            }
        }
        false
    }

    /// Adds `style` to `range`, or removes it when the whole range already has
    /// it — the ⌘B behaviour of every rich text editor.
    pub fn toggle_mark(&mut self, range: Range<usize>, style: Style) {
        if self.has_style(range.clone(), &style) {
            self.remove_mark(range, &style);
        } else {
            self.add_mark(range, style);
        }
    }

    /// Splits at `offset`, returning the tail as a new block of `kind`.
    pub fn split_off(&mut self, offset: usize, id: BlockId, kind: BlockKind) -> Block {
        let offset = floor_boundary(&self.text, offset.min(self.text.len()));
        let tail_text = self.text.split_off(offset);
        let mut tail_marks = Vec::new();
        for mark in &mut self.marks {
            if mark.range.end > offset {
                let start = mark.range.start.max(offset) - offset;
                tail_marks.push(Mark {
                    range: start..mark.range.end - offset,
                    style: mark.style.clone(),
                });
                mark.range.end = offset;
            }
        }
        self.marks.retain(|mark| !mark.range.is_empty());
        let mut tail = Block {
            id,
            kind,
            indent: if kind.is_list() { self.indent } else { 0 },
            text: tail_text,
            marks: tail_marks,
            src: String::new(),
            info: String::new(),
        };
        tail.normalize();
        tail
    }

    /// Appends `other`'s text and marks.
    pub fn append(&mut self, other: &Block) {
        let base = self.text.len();
        self.text.push_str(&other.text);
        for mark in &other.marks {
            self.marks.push(Mark {
                range: mark.range.start + base..mark.range.end + base,
                style: mark.style.clone(),
            });
        }
        self.normalize();
    }

    pub fn set_kind(&mut self, kind: BlockKind) {
        self.kind = kind;
        if !kind.is_list() {
            self.indent = 0;
        }
        if matches!(kind, BlockKind::Code) {
            self.marks.clear();
        }
        if matches!(kind, BlockKind::Divider) {
            self.text.clear();
            self.marks.clear();
        }
        if kind != BlockKind::Image {
            self.src.clear();
        }
        if kind != BlockKind::Code {
            self.info.clear();
        }
    }

    pub(crate) fn normalize(&mut self) {
        let len = self.text.len();
        for mark in &mut self.marks {
            mark.range.end = mark.range.end.min(len);
            mark.range.start = mark.range.start.min(mark.range.end);
        }
        self.marks.retain(|mark| !mark.range.is_empty());
        self.marks.sort_by(|a, b| {
            a.style
                .cmp(&b.style)
                .then(a.range.start.cmp(&b.range.start))
        });
        let mut merged: Vec<Mark> = Vec::with_capacity(self.marks.len());
        for mark in self.marks.drain(..) {
            if let Some(last) = merged.last_mut()
                && last.style == mark.style
                && mark.range.start <= last.range.end
            {
                last.range.end = last.range.end.max(mark.range.end);
                continue;
            }
            merged.push(mark);
        }
        merged.sort_by(|a, b| {
            a.range
                .start
                .cmp(&b.range.start)
                .then(a.style.rank().cmp(&b.style.rank()))
        });
        self.marks = merged;
    }
}

pub fn floor_boundary(text: &str, mut offset: usize) -> usize {
    offset = offset.min(text.len());
    while !text.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Document {
    pub title: String,
    pub blocks: Vec<Block>,
    next_id: BlockId,
}

impl Document {
    pub fn new(title: impl Into<String>, blocks: Vec<Block>) -> Self {
        let mut doc = Self {
            title: title.into(),
            blocks: Vec::new(),
            next_id: 1,
        };
        for mut block in blocks {
            block.id = doc.fresh_id();
            doc.blocks.push(block);
        }
        doc.ensure_block();
        doc
    }

    pub fn fresh_id(&mut self) -> BlockId {
        let id = self.next_id.max(1);
        self.next_id = id + 1;
        id
    }

    /// A note always has at least one block for the caret to live in.
    pub fn ensure_block(&mut self) {
        if self.blocks.is_empty() {
            let id = self.fresh_id();
            self.blocks.push(Block::new(id, BlockKind::Paragraph, ""));
        }
    }

    pub fn index_of(&self, id: BlockId) -> Option<usize> {
        self.blocks.iter().position(|block| block.id == id)
    }

    /// Todo counts as (done, total).
    pub fn todo_progress(&self) -> (usize, usize) {
        let mut done = 0;
        let mut total = 0;
        for block in &self.blocks {
            if let BlockKind::Todo { checked } = block.kind {
                total += 1;
                done += usize::from(checked);
            }
        }
        (done, total)
    }

    /// The 1-based ordinal a numbered block displays.
    pub fn ordinal(&self, index: usize) -> usize {
        let Some(block) = self.blocks.get(index) else {
            return 1;
        };
        let indent = block.indent;
        let mut n = 1;
        for previous in self.blocks[..index].iter().rev() {
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

    /// Plain text of the body, one block per line — for search and previews.
    pub fn plain_text(&self) -> String {
        let mut out = String::new();
        for block in &self.blocks {
            if !block.text.is_empty() {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(&block.text);
            }
        }
        out
    }

    /// Structural equality ignoring runtime block ids.
    pub fn same_content(&self, other: &Document) -> bool {
        self.title == other.title
            && self.blocks.len() == other.blocks.len()
            && self.blocks.iter().zip(&other.blocks).all(|(a, b)| {
                a.kind == b.kind
                    && a.indent == b.indent
                    && a.text == b.text
                    && a.marks == b.marks
                    && a.src == b.src
                    && a.info == b.info
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(text: &str) -> Block {
        Block::new(1, BlockKind::Paragraph, text)
    }

    #[test]
    fn typing_at_the_end_of_bold_stays_bold() {
        let mut b = block("hello");
        b.add_mark(0..5, Style::Bold);
        let styles = b.styles_at(5);
        b.replace(5..5, "!", &styles);
        assert_eq!(
            b.marks,
            vec![Mark {
                range: 0..6,
                style: Style::Bold
            }]
        );
        // Typing before the mark does not inherit it.
        let styles = b.styles_at(0);
        b.replace(0..0, ">", &styles);
        assert_eq!(
            b.marks,
            vec![Mark {
                range: 1..7,
                style: Style::Bold
            }]
        );
    }

    #[test]
    fn deleting_across_a_mark_shrinks_it() {
        let mut b = block("abcdef");
        b.add_mark(2..5, Style::Italic);
        b.replace(1..3, "", &[]);
        assert_eq!(b.text, "adef");
        assert_eq!(
            b.marks,
            vec![Mark {
                range: 1..3,
                style: Style::Italic
            }]
        );
        b.replace(0..4, "", &[]);
        assert!(b.marks.is_empty());
    }

    #[test]
    fn toggle_removes_only_the_selected_part() {
        let mut b = block("abcdef");
        b.toggle_mark(0..6, Style::Bold);
        b.toggle_mark(2..4, Style::Bold);
        assert_eq!(
            b.marks,
            vec![
                Mark {
                    range: 0..2,
                    style: Style::Bold
                },
                Mark {
                    range: 4..6,
                    style: Style::Bold
                }
            ]
        );
        b.toggle_mark(1..5, Style::Bold);
        assert_eq!(
            b.marks,
            vec![Mark {
                range: 0..6,
                style: Style::Bold
            }]
        );
    }

    #[test]
    fn split_and_append_round_trip_marks() {
        let mut b = block("hello world");
        b.add_mark(3..8, Style::Code);
        let tail = b.split_off(5, 2, BlockKind::Paragraph);
        assert_eq!(b.text, "hello");
        assert_eq!(
            b.marks,
            vec![Mark {
                range: 3..5,
                style: Style::Code
            }]
        );
        assert_eq!(tail.text, " world");
        assert_eq!(
            tail.marks,
            vec![Mark {
                range: 0..3,
                style: Style::Code
            }]
        );
        b.append(&tail);
        assert_eq!(
            b.marks,
            vec![Mark {
                range: 3..8,
                style: Style::Code
            }]
        );
    }

    #[test]
    fn ordinals_restart_after_other_blocks_and_skip_children() {
        let mut blocks = vec![
            Block::new(0, BlockKind::Numbered, "a"),
            Block::new(0, BlockKind::Bullet, "child"),
            Block::new(0, BlockKind::Numbered, "b"),
            Block::new(0, BlockKind::Paragraph, "break"),
            Block::new(0, BlockKind::Numbered, "c"),
        ];
        blocks[1].indent = 1;
        let doc = Document::new("", blocks);
        assert_eq!(doc.ordinal(0), 1);
        assert_eq!(doc.ordinal(2), 2);
        assert_eq!(doc.ordinal(4), 1);
    }
}
