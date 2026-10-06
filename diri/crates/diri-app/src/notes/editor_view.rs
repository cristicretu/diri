//! The rich note editor: blocks rendered as they will read, edited in place.
//!
//! Behaviour lives in `diri_notes::edit::Editor`; this view adds what needs
//! pixels — text layout, hit testing, vertical motion, IME, the caret and
//! selection, checkboxes, the `/` block menu, and `@` mentions. Each frame
//! records one `TextLayout` per block, and pointer/motion code reads those
//! layouts back.
//!
//! The view is layout-agnostic: it fills whatever box its host gives it and
//! centres a [`MEASURE`]-wide column inside, so the same entity works in the
//! Notes window and as a main-window content view in place of a terminal.
//! Hosts talk to it only through [`EditorEvent`], [`NoteEditorView::reload`],
//! [`NoteEditorView::set_colors`], and [`NoteEditorView::set_mentions`].

use std::ops::Range;
use std::rc::Rc;
use std::time::{Duration, Instant};

use diri_notes::doc::{Block, BlockKind, Mark, Style, Tone};
use diri_notes::edit::{Editor, Granularity, Pos, Selection, Turn};
use diri_notes::mention::{self, Candidate, MentionTarget};
use diri_ui::{
    AgentKind as UiAgentKind, AgentLogo, Ink, Palette, SemanticColors, StatusGlyph, StatusState,
    Typo,
};
use gpui::{
    Animation, AnimationExt, AnyElement, App, Bounds, ClipboardItem, Context, ElementInputHandler,
    Entity, EntityInputHandler, EventEmitter, FocusHandle, Focusable, FontStyle, FontWeight,
    HighlightStyle, KeyBinding, KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, Pixels, Point, Render, ScrollHandle, SharedString, StrikethroughStyle,
    StyledText, Task, TextLayout, UTF16Selection, UnderlineStyle, Window, actions, anchored,
    canvas, deferred, div, fill, point, prelude::*, px, size,
};

use crate::floating;

#[path = "table_view.rs"]
mod table_view;
use table_view::{TABLE_MENU, TableMenu};
#[path = "block_handle.rs"]
mod block_handle;
use block_handle::{BLOCK_MENU, BlockDrag, BlockMenu};

pub(crate) const EDITOR_CONTEXT: &str = "DiriNoteEditor";
/// The editor's context while the ⌘K link panel owns the keyboard: none of
/// the editor's bindings match, so keys reach the panel's field.
const LINK_EDITOR_CONTEXT: &str = "DiriNoteLinkEditor";

actions!(
    diri_notes,
    [
        Backspace,
        BackspaceWord,
        BackspaceLine,
        Delete,
        DeleteWord,
        Newline,
        SoftNewline,
        Indent,
        Outdent,
        MoveLeft,
        MoveRight,
        MoveUp,
        MoveDown,
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
        DocStart,
        DocEnd,
        SelectDocStart,
        SelectDocEnd,
        SelectAll,
        Copy,
        Cut,
        Paste,
        Undo,
        Redo,
        Bold,
        Italic,
        InlineCode,
        Strike,
        Link,
        ToggleTodo,
        ToggleFold,
        TableRowAbove,
        TableRowBelow,
        TableColLeft,
        TableColRight,
        TableMenuAction,
        TurnParagraph,
        TurnHeading1,
        TurnHeading2,
        TurnHeading3,
        TurnBullet,
        TurnNumbered,
        TurnTodo,
        TurnQuote,
        TurnCode,
        MoveBlockUp,
        MoveBlockDown,
        Escape,
        ShowCharacterPalette,
        StartWork,
    ]
);

// Table shortcuts, shared by the keymap and the table menu that prints them.
// ⌃⇧ arrows are free app-wide; outside a table they fall through.
const KEY_TABLE_ROW_ABOVE: &str = "ctrl-shift-up";
const KEY_TABLE_ROW_BELOW: &str = "ctrl-shift-down";
const KEY_TABLE_COL_LEFT: &str = "ctrl-shift-left";
const KEY_TABLE_COL_RIGHT: &str = "ctrl-shift-right";
// Turn-into shortcuts, shared by the keymap and the `/` menu that prints them.
const KEY_TURN_PARAGRAPH: &str = "cmd-alt-0";
const KEY_TURN_H1: &str = "cmd-alt-1";
const KEY_TURN_H2: &str = "cmd-alt-2";
const KEY_TURN_H3: &str = "cmd-alt-3";
const KEY_TURN_BULLET: &str = "cmd-shift-8";
const KEY_TURN_NUMBERED: &str = "cmd-shift-7";
const KEY_TURN_TODO: &str = "cmd-shift-9";
const KEY_TURN_QUOTE: &str = "cmd-alt-q";
const KEY_TURN_CODE: &str = "cmd-alt-c";

pub(crate) fn key_bindings() -> Vec<KeyBinding> {
    let c = Some(EDITOR_CONTEXT);
    vec![
        // ⌘⇧↩ zooms the pane, ⌘↩ ticks, ⌥⌘↩ folds.
        KeyBinding::new("ctrl-cmd-enter", StartWork, c),
        KeyBinding::new("backspace", Backspace, c),
        KeyBinding::new("shift-backspace", Backspace, c),
        KeyBinding::new("alt-backspace", BackspaceWord, c),
        KeyBinding::new("cmd-backspace", BackspaceLine, c),
        KeyBinding::new("delete", Delete, c),
        KeyBinding::new("ctrl-d", Delete, c),
        KeyBinding::new("alt-delete", DeleteWord, c),
        KeyBinding::new("enter", Newline, c),
        KeyBinding::new("shift-enter", SoftNewline, c),
        KeyBinding::new("tab", Indent, c),
        KeyBinding::new("shift-tab", Outdent, c),
        KeyBinding::new("left", MoveLeft, c),
        KeyBinding::new("right", MoveRight, c),
        KeyBinding::new("up", MoveUp, c),
        KeyBinding::new("down", MoveDown, c),
        KeyBinding::new("ctrl-b", MoveLeft, c),
        KeyBinding::new("ctrl-f", MoveRight, c),
        KeyBinding::new("ctrl-p", MoveUp, c),
        KeyBinding::new("ctrl-n", MoveDown, c),
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
        KeyBinding::new("ctrl-a", LineStart, c),
        KeyBinding::new("ctrl-e", LineEnd, c),
        KeyBinding::new("home", LineStart, c),
        KeyBinding::new("end", LineEnd, c),
        KeyBinding::new("cmd-shift-left", SelectLineStart, c),
        KeyBinding::new("cmd-shift-right", SelectLineEnd, c),
        KeyBinding::new("cmd-up", DocStart, c),
        KeyBinding::new("cmd-down", DocEnd, c),
        KeyBinding::new("cmd-shift-up", SelectDocStart, c),
        KeyBinding::new("cmd-shift-down", SelectDocEnd, c),
        KeyBinding::new("cmd-a", SelectAll, c),
        KeyBinding::new("cmd-c", Copy, c),
        KeyBinding::new("cmd-x", Cut, c),
        KeyBinding::new("cmd-v", Paste, c),
        KeyBinding::new("cmd-z", Undo, c),
        KeyBinding::new("cmd-shift-z", Redo, c),
        KeyBinding::new("cmd-b", Bold, c),
        KeyBinding::new("cmd-i", Italic, c),
        KeyBinding::new("cmd-e", InlineCode, c),
        KeyBinding::new("cmd-shift-x", Strike, c),
        KeyBinding::new("cmd-k", Link, c),
        KeyBinding::new("cmd-enter", ToggleTodo, c),
        KeyBinding::new("cmd-alt-enter", ToggleFold, c),
        KeyBinding::new(KEY_TABLE_ROW_ABOVE, TableRowAbove, c),
        KeyBinding::new(KEY_TABLE_ROW_BELOW, TableRowBelow, c),
        KeyBinding::new(KEY_TABLE_COL_LEFT, TableColLeft, c),
        KeyBinding::new(KEY_TABLE_COL_RIGHT, TableColRight, c),
        KeyBinding::new("ctrl-enter", TableMenuAction, c),
        KeyBinding::new(KEY_TURN_PARAGRAPH, TurnParagraph, c),
        KeyBinding::new(KEY_TURN_H1, TurnHeading1, c),
        KeyBinding::new(KEY_TURN_H2, TurnHeading2, c),
        KeyBinding::new(KEY_TURN_H3, TurnHeading3, c),
        KeyBinding::new(KEY_TURN_BULLET, TurnBullet, c),
        KeyBinding::new(KEY_TURN_NUMBERED, TurnNumbered, c),
        KeyBinding::new(KEY_TURN_TODO, TurnTodo, c),
        KeyBinding::new(KEY_TURN_QUOTE, TurnQuote, c),
        KeyBinding::new(KEY_TURN_CODE, TurnCode, c),
        KeyBinding::new("alt-shift-up", MoveBlockUp, c),
        KeyBinding::new("alt-shift-down", MoveBlockDown, c),
        KeyBinding::new("escape", Escape, c),
        KeyBinding::new("ctrl-cmd-space", ShowCharacterPalette, c),
    ]
}

pub(crate) enum EditorEvent {
    /// The note's content changed and should be saved.
    Changed,
    /// Escape with nothing to dismiss: the host takes focus back.
    Dismiss,
    /// A mention chip was clicked: the host reveals that session or note.
    OpenMention(MentionTarget),
    /// A to-do's work item needs the host (see `work_item`).
    Work(super::work_item::WorkRequest),
}

/// One thing the `@` menu offers, with what its chip needs to stay live.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct MentionEntry {
    pub(crate) candidate: Candidate,
    /// The agent behind a session; `None` for notes.
    pub(crate) agent: Option<UiAgentKind>,
    pub(crate) status: Option<StatusState>,
    /// A quiet second line: the project, or "Note".
    pub(crate) detail: SharedString,
}

/// Everything mentionable right now, most recent first. The host rebuilds it
/// from the session store and the note list and hands it to the editor; the
/// editor never reaches into either.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct MentionDirectory {
    pub(crate) entries: Vec<MentionEntry>,
}

impl MentionDirectory {
    pub(crate) fn find(&self, target: &MentionTarget) -> Option<&MentionEntry> {
        self.entries.iter().find(|e| &e.candidate.target == target)
    }

    fn candidates(&self) -> Vec<Candidate> {
        self.entries.iter().map(|e| e.candidate.clone()).collect()
    }
}

/// How many rows the `@` menu shows at most.
const MENTION_LIMIT: usize = 8;
/// A query this long without a match is prose, not a mention.
const MENTION_QUERY_MAX: usize = 40;

type RowHeights = Rc<std::cell::RefCell<std::collections::HashMap<u64, f32>>>;

/// Each image block's on-screen bounds, written while painting and read by
/// hit testing.
type ImageRects = Rc<std::cell::RefCell<Vec<(usize, Bounds<Pixels>)>>>;

/// Where a note's pictures live: the store that writes them and the note
/// they belong to. Set by the host; without it paste and drop insert no
/// images and relative images do not resolve.
#[derive(Clone)]
pub(crate) struct AssetHome {
    pub(crate) store: std::sync::Arc<diri_notes::store::NoteStore>,
    pub(crate) note_id: String,
}

/// The ⌘K panel: a URL field over the selection or the link at the caret,
/// and what Return does with it.
struct LinkEditor {
    query: crate::query_editor::QueryEditor,
    /// The selection ⌘K was pressed on, restored when the link applies.
    selection: Selection,
    /// The link being edited: (block, range, url).
    existing: Option<(usize, Range<usize>, String)>,
    selected: usize,
}

#[derive(Clone, Debug, PartialEq)]
enum LinkRow {
    Apply { url: String },
    Open { url: String },
    Remove,
}

enum ChipHit {
    Mention(MentionTarget),
    Link(String),
}

struct MentionMenu {
    block_id: u64,
    /// Byte offset of the `@` (or of the `[[`).
    at: usize,
    selected: usize,
    /// Opened by `[[`: links to notes only.
    notes_only: bool,
}

impl MentionMenu {
    fn trigger(&self) -> &'static str {
        if self.notes_only { "[[" } else { "@" }
    }
}

const CARET_BLINK: Duration = Duration::from_millis(530);
pub(super) const MARKER_WIDTH: f32 = 26.0;
const INDENT_STEP: f32 = 24.0;
/// Space above the title inside the scroll area.
const PAGE_TOP: f32 = 56.0;
/// Space either side of the text column: room for the block handle, and a
/// list item's fold chevron beside it.
const PAGE_SIDE: f32 = 56.0;
/// Gutter width left of a list item that holds its fold chevron.
const DISCLOSURE_WIDTH: f32 = 20.0;
pub(crate) const MEASURE: f32 = 700.0;
use super::chip::{DOT as CHIP_DOT, PAD_X as CHIP_PAD_X};
/// A tool chip's glyph.
const CHIP_ICON: f32 = 13.0;

/// The notes accent: Diri's ember, shared with the brand mark.
pub(crate) fn accent() -> gpui::Rgba {
    Palette::CLAY
}

/// One row of the `/` menu: a block kind, its glyph, and the key equivalent
/// that turns the current block into it, printed like a native menu's.
#[derive(Clone, Copy, PartialEq)]
enum SlashAction {
    Turn(Turn),
    /// Pick picture files and insert them.
    Image,
    /// A 3 × 3 table.
    Table,
    /// `[[`: pick a note to link.
    LinkNote,
    /// `@`: pick a session or a note to mention.
    Mention,
}

#[derive(Clone, Copy)]
struct SlashItem {
    label: &'static str,
    icon: &'static str,
    keys: Option<&'static str>,
    action: SlashAction,
    /// Rows in different groups are divided by a separator: text, lists,
    /// then blocks that set content apart.
    group: u8,
    keywords: &'static str,
}

const SLASH_ITEMS: &[SlashItem] = &[
    SlashItem {
        label: "Text",
        icon: "textformat",
        keys: Some(KEY_TURN_PARAGRAPH),
        action: SlashAction::Turn(Turn::Kind(BlockKind::Paragraph)),
        group: 0,
        keywords: "text paragraph plain",
    },
    SlashItem {
        label: "Heading 1",
        icon: "textformat.h1",
        keys: Some(KEY_TURN_H1),
        action: SlashAction::Turn(Turn::Kind(BlockKind::Heading(1))),
        group: 0,
        keywords: "heading h1 title big",
    },
    SlashItem {
        label: "Heading 2",
        icon: "textformat.h2",
        keys: Some(KEY_TURN_H2),
        action: SlashAction::Turn(Turn::Kind(BlockKind::Heading(2))),
        group: 0,
        keywords: "heading h2 subtitle",
    },
    SlashItem {
        label: "Heading 3",
        icon: "textformat.h3",
        keys: Some(KEY_TURN_H3),
        action: SlashAction::Turn(Turn::Kind(BlockKind::Heading(3))),
        group: 0,
        keywords: "heading h3 small",
    },
    SlashItem {
        label: "To-do",
        icon: "checkmark.square",
        keys: Some(KEY_TURN_TODO),
        action: SlashAction::Turn(Turn::Kind(BlockKind::Todo { checked: false })),
        group: 1,
        keywords: "todo task checkbox check list",
    },
    SlashItem {
        label: "Bulleted list",
        icon: "list.bullet",
        keys: Some(KEY_TURN_BULLET),
        action: SlashAction::Turn(Turn::Kind(BlockKind::Bullet)),
        group: 1,
        keywords: "bullet list unordered",
    },
    SlashItem {
        label: "Numbered list",
        icon: "list.number",
        keys: Some(KEY_TURN_NUMBERED),
        action: SlashAction::Turn(Turn::Kind(BlockKind::Numbered)),
        group: 1,
        keywords: "numbered list ordered",
    },
    SlashItem {
        label: "Quote",
        icon: "text.quote",
        keys: Some(KEY_TURN_QUOTE),
        action: SlashAction::Turn(Turn::Kind(BlockKind::Quote)),
        group: 2,
        keywords: "quote blockquote citation",
    },
    SlashItem {
        label: "Code",
        icon: "chevron.left.forwardslash.chevron.right",
        keys: Some(KEY_TURN_CODE),
        action: SlashAction::Turn(Turn::Kind(BlockKind::Code)),
        group: 2,
        keywords: "code snippet monospace",
    },
    SlashItem {
        label: "Callout",
        icon: "info.circle",
        keys: None,
        action: SlashAction::Turn(Turn::Kind(BlockKind::Callout(Tone::Note))),
        group: 2,
        keywords: "callout note tip important warning caution aside info alert",
    },
    SlashItem {
        label: "Divider",
        icon: "divider",
        keys: None,
        action: SlashAction::Turn(Turn::Divider),
        group: 2,
        keywords: "divider rule line separator",
    },
    SlashItem {
        label: "Table",
        icon: "tablecells",
        keys: None,
        action: SlashAction::Table,
        group: 3,
        keywords: "table grid rows columns spreadsheet sheet",
    },
    SlashItem {
        label: "Image",
        icon: "photo",
        keys: None,
        action: SlashAction::Image,
        group: 3,
        keywords: "image picture photo screenshot upload",
    },
    SlashItem {
        label: "Link to note",
        icon: "link",
        keys: None,
        action: SlashAction::LinkNote,
        group: 4,
        keywords: "link note page wiki backlink reference [[",
    },
    SlashItem {
        label: "Mention",
        icon: "doc.text",
        keys: None,
        action: SlashAction::Mention,
        group: 4,
        keywords: "mention session agent note person at @",
    },
];

/// The `/` menu's rows for `query` (what follows the `/`), in menu order:
/// a label containing the query, or a keyword starting with it.
/// The shown name of a block kind or block action, from the English label
/// the menus keep for filtering and tests.
pub(super) fn block_label(label: &'static str) -> &'static str {
    let id = match label {
        "Text" => "notes.block.text",
        "Heading 1" => "notes.block.heading_1",
        "Heading 2" => "notes.block.heading_2",
        "Heading 3" => "notes.block.heading_3",
        "To-do" => "notes.block.todo",
        "Bulleted list" => "notes.block.bulleted_list",
        "Numbered list" => "notes.block.numbered_list",
        "Quote" => "notes.block.quote",
        "Code" => "notes.block.code",
        "Callout" => "notes.block.callout",
        "Divider" => "notes.block.divider",
        "Table" => "notes.block.table",
        "Image" => "notes.block.image",
        "Link to note" => "notes.block.link_to_note",
        "Mention" => "notes.block.mention",
        "Add block below" => "notes.block.add_below",
        "Duplicate" => "notes.block.duplicate",
        "Move up" => "notes.block.move_up",
        "Move down" => "notes.block.move_down",
        "Delete" => "notes.block.delete",
        _ => return label,
    };
    crate::i18n::t(id)
}

fn slash_filter(query: &str) -> Vec<SlashItem> {
    let query = query.trim().to_lowercase();
    SLASH_ITEMS
        .iter()
        .filter(|item| {
            query.is_empty()
                || item.label.to_lowercase().contains(&query)
                || block_label(item.label).to_lowercase().contains(&query)
                || item.keywords.split(' ').any(|k| k.starts_with(&query))
        })
        .copied()
        .collect()
}

struct SlashMenu {
    block_id: u64,
    /// Byte offset of the `/`.
    slash: usize,
    selected: usize,
}

pub(crate) struct NoteEditorView {
    pub(crate) editor: Editor,
    focus: FocusHandle,
    colors: SemanticColors,
    /// One layout per block from the last frame, valid for `layout_revision`.
    /// `None` for blocks this frame did not lay out (off screen or folded).
    layouts: Vec<Option<TextLayout>>,
    /// Each block's laid-out text, parallel to `layouts`.
    shown: Vec<Shown>,
    layout_revision: u64,
    layout_count: usize,
    /// IME composition range, in bytes of the caret's block.
    marked: Option<Range<usize>>,
    goal_x: Option<Pixels>,
    selecting: bool,
    scroll: ScrollHandle,
    autoscroll: bool,
    caret_visible: bool,
    blink_epoch: usize,
    blinking: bool,
    /// Whether the last frame had focus: the blink loop's cue to stop.
    focused_last_frame: bool,
    _blink: Task<()>,
    slash: Option<SlashMenu>,
    mention: Option<MentionMenu>,
    link_editor: Option<LinkEditor>,
    table_menu: Option<TableMenu>,
    /// The menu opened from a block's grip.
    block_menu: Option<BlockMenu>,
    /// The block whose grip is being dragged.
    dragging: Option<u64>,
    /// Shown under the note's last block: its backlinks.
    footer: Option<gpui::AnyView>,
    /// Each table's sideways scroll, by its first cell's id.
    table_scrolls: std::cell::RefCell<std::collections::HashMap<u64, ScrollHandle>>,
    mentions: Rc<MentionDirectory>,
    assets: Option<AssetHome>,
    /// Each image's on-screen bounds from the last paint, for clicks.
    image_rects: ImageRects,
    /// Pixel sizes read from picture headers, so an image lays out at its
    /// final size before it loads.
    image_sizes:
        std::cell::RefCell<std::collections::HashMap<std::path::PathBuf, Option<(u32, u32)>>>,
    /// Each block's rendered height by id, measured the last time it was on
    /// screen: what an off-screen block's spacer is sized from.
    row_heights: RowHeights,
    caret_bounds: Option<Bounds<Pixels>>,
    /// Checkboxes ticked this session, for their pop animation.
    ticked: Vec<(u64, Instant)>,
    /// To-dos as agent work: folds, starts in flight, their panels.
    pub(super) work: super::work_item::WorkView,
}

impl EventEmitter<EditorEvent> for NoteEditorView {}

impl Focusable for NoteEditorView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

impl NoteEditorView {
    pub(crate) fn new(editor: Editor, colors: SemanticColors, cx: &mut Context<Self>) -> Self {
        Self {
            editor,
            focus: cx.focus_handle(),
            colors,
            layouts: Vec::new(),
            shown: Vec::new(),
            layout_revision: u64::MAX,
            layout_count: 0,
            marked: None,
            goal_x: None,
            selecting: false,
            scroll: ScrollHandle::new(),
            autoscroll: true,
            caret_visible: true,
            blink_epoch: 0,
            blinking: false,
            focused_last_frame: false,
            _blink: Task::ready(()),
            slash: None,
            mention: None,
            link_editor: None,
            table_menu: None,
            block_menu: None,
            dragging: None,
            footer: None,
            table_scrolls: Default::default(),
            mentions: Rc::default(),
            assets: None,
            image_rects: Rc::default(),
            image_sizes: Default::default(),
            row_heights: Rc::default(),
            caret_bounds: None,
            ticked: Vec::new(),
            work: super::work_item::WorkView::default(),
        }
    }

    pub(super) fn colors(&self) -> SemanticColors {
        self.colors
    }

    /// Where the `/` or `@` that opened a menu sits. Menus hang from their
    /// trigger character, not the caret: the caret's painted bounds lag a
    /// frame behind the keystroke (the menu would open at the old caret, then
    /// hop right by the `/`'s width) and move as the query is typed. The
    /// trigger's start is the same in last frame's layout and this one.
    fn trigger_anchor(&self) -> Option<Bounds<Pixels>> {
        if self.block_menu.is_some() {
            return self.block_menu_anchor();
        }
        let (block_id, offset) = if let Some(menu) = &self.slash {
            (menu.block_id, menu.slash)
        } else if let Some(menu) = &self.mention {
            (menu.block_id, menu.at)
        } else {
            return None;
        };
        let index = self.editor.blocks().iter().position(|b| b.id == block_id)?;
        let (point, line) = self.caret_point(Pos::new(index, offset))?;
        Some(Bounds::new(point, size(px(2.0), line)))
    }

    /// Menus open under `pos` this frame, wherever the caret was painted.
    pub(super) fn anchor_menus_at(&mut self, pos: Pos) {
        if let Some((point, line)) = self.caret_point(pos) {
            self.caret_bounds = Some(Bounds::new(point, size(px(2.0), line)));
        }
    }

    pub(crate) fn set_colors(&mut self, colors: SemanticColors) {
        self.colors = colors;
    }

    /// A view shown under the note's last block (the backlinks).
    pub(crate) fn set_footer(&mut self, footer: Option<gpui::AnyView>) {
        self.footer = footer;
    }

    #[cfg(test)]
    pub(crate) fn has_footer_for_test(&self) -> bool {
        self.footer.is_some()
    }

    #[cfg(test)]
    pub(crate) fn is_blinking(&self) -> bool {
        self.blinking
    }

    #[cfg(test)]
    pub(crate) fn scroll_by_for_test(&mut self, dy: Pixels, cx: &mut Context<Self>) {
        let offset = self.scroll.offset();
        let max = self.scroll.max_offset().y;
        self.scroll
            .set_offset(point(offset.x, (offset.y + dy).clamp(-max, px(0.0))));
        cx.notify();
    }

    /// Where this note's pictures are saved and resolved from.
    pub(crate) fn set_asset_home(&mut self, home: AssetHome) {
        self.assets = Some(home);
    }

    /// What `@` offers and what chips read their live status from.
    #[cfg_attr(
        not(test),
        allow(dead_code, reason = "API for the to-do handoff (notes/todo-handoff)")
    )]
    pub(crate) fn mentions(&self) -> &MentionDirectory {
        &self.mentions
    }

    /// The live chip for session `id`, for any note surface outside the
    /// text flow (a to-do's work state, say): status from the same
    /// directory the inline chips use, hollow once the session is gone.
    #[cfg_attr(
        not(test),
        allow(dead_code, reason = "API for the to-do handoff (notes/todo-handoff)")
    )]
    pub(crate) fn session_chip(&self, id: &str, fallback: &str) -> super::chip::SessionChip {
        super::chip::SessionChip::for_session(id, fallback, &self.mentions, self.colors)
    }

    /// Inserts a live session chip at `pos`, as if picked from `@`.
    #[cfg_attr(
        not(test),
        allow(dead_code, reason = "API for the to-do handoff (notes/todo-handoff)")
    )]
    pub(crate) fn insert_session_mention(
        &mut self,
        pos: diri_notes::edit::Pos,
        id: &str,
        cx: &mut Context<Self>,
    ) {
        let target = MentionTarget::Session(id.to_owned());
        let label = self
            .mentions
            .find(&target)
            .map(|e| e.candidate.label.clone())
            .unwrap_or_else(|| mention::session_label("", "Session"));
        self.editor.set_caret(pos);
        self.editor
            .insert_mention(pos.offset..pos.offset, &target, &label, now_ms());
        self.edited(cx);
    }

    /// Replaces what `@` offers and what chips show. Cheap when unchanged,
    /// so hosts may call it on every store change.
    pub(crate) fn set_mentions(&mut self, directory: MentionDirectory, cx: &mut Context<Self>) {
        if *self.mentions == directory {
            return;
        }
        self.mentions = Rc::new(directory);
        self.sync_mention();
        cx.notify();
    }

    /// Replaces the content after an outside change, keeping the caret close.
    pub(crate) fn reload(&mut self, editor: Editor, cx: &mut Context<Self>) {
        let selection = self.editor.selection;
        self.work.remap(self.editor.blocks(), editor.blocks());
        // Folds are view state keyed by runtime ids; carry each one to the
        // block at the same place with the same text.
        let folded: Vec<(usize, String)> = (0..self.editor.blocks().len())
            .filter(|i| self.editor.is_collapsed(*i))
            .map(|i| (i, self.editor.block(i).text.clone()))
            .collect();
        let mut editor = editor;
        for (index, text) in folded {
            if editor.blocks().get(index).is_some_and(|b| b.text == text) {
                editor.set_collapsed(index, true);
            }
        }
        self.editor = editor;
        self.editor.set_selection(selection);
        self.slash = None;
        self.mention = None;
        self.marked = None;
        cx.notify();
    }

    // -----------------------------------------------------------------------
    // Bookkeeping

    fn touched(&mut self, cx: &mut Context<Self>) {
        self.autoscroll = true;
        self.restart_blink(cx);
        cx.notify();
    }

    fn edited(&mut self, cx: &mut Context<Self>) {
        self.goal_x = None;
        self.marked = None;
        self.sync_slash();
        self.sync_mention();
        self.touched(cx);
        cx.emit(EditorEvent::Changed);
    }

    pub(super) fn moved(&mut self, cx: &mut Context<Self>) {
        self.marked = None;
        self.sync_slash();
        self.sync_mention();
        self.touched(cx);
    }

    /// Blinks the caret while the editor has focus. An unfocused editor
    /// shows no caret, so the loop ends instead of scheduling frames for
    /// nothing; the next focused render starts it again.
    fn restart_blink(&mut self, cx: &mut Context<Self>) {
        self.caret_visible = true;
        self.blinking = true;
        self.blink_epoch += 1;
        let epoch = self.blink_epoch;
        self._blink = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(CARET_BLINK).await;
                let alive = this
                    .update(cx, |this, cx| {
                        if this.blink_epoch != epoch {
                            return false;
                        }
                        if !this.focused_last_frame {
                            this.blinking = false;
                            this.caret_visible = true;
                            return false;
                        }
                        this.caret_visible = !this.caret_visible;
                        cx.notify();
                        true
                    })
                    .unwrap_or(false);
                if !alive {
                    break;
                }
            }
        });
    }

    fn layout(&self, index: usize) -> Option<&TextLayout> {
        (self.layout_revision == self.editor.revision
            && self.layout_count == self.editor.blocks().len())
        .then(|| self.layouts.get(index).and_then(Option::as_ref))
        .flatten()
    }

    /// Where block `index`'s reserved tail room starts, relative to its text.
    /// Reads the last frame's layout and text, which agree with each other
    /// even while typing changes this one: the room trails the words by a
    /// frame instead of vanishing on every keystroke.
    pub(super) fn tail_point(&self, index: usize) -> Option<Point<Pixels>> {
        let tail = self.shown.get(index)?.tail?;
        let layout = self.layouts.get(index)?.as_ref()?;
        Some(layout.position_for_index(tail)? - layout.bounds().origin)
    }

    fn text_len(&self, index: usize) -> usize {
        self.editor.block(index).text.len()
    }

    // -----------------------------------------------------------------------
    // Geometry

    fn caret_point(&self, pos: Pos) -> Option<(Point<Pixels>, Pixels)> {
        let layout = self.layout(pos.block)?;
        let offset = if self.editor.block(pos.block).text.is_empty() {
            0
        } else {
            pos.offset
        };
        let offset = self
            .shown
            .get(pos.block)
            .map_or(offset, |s| s.to_display(offset));
        let point = layout.position_for_index(offset)?;
        Some((point, layout.line_height()))
    }

    fn hit(&self, at: Point<Pixels>) -> Option<Pos> {
        // A click anywhere on a picture selects it.
        if let Some((index, _)) = self
            .image_rects
            .borrow()
            .iter()
            .find(|(_, bounds)| bounds.contains(&at))
        {
            return Some(Pos::new(*index, 0));
        }
        let point_at = at;
        let hidden = self.editor.hidden();
        let mut chosen = None;
        let mut previous: Option<usize> = None;
        // Folded blocks keep a layout but take no space; only visible
        // blocks can be hit.
        for index in (0..hidden.len()).filter(|i| !hidden[*i]) {
            // Off-screen blocks have no layout; a click never lands there.
            let Some(layout) = self.layout(index) else {
                continue;
            };
            let bounds = layout.bounds();
            chosen = Some(index);
            if point_at.y <= bounds.bottom() {
                if let Some(previous) = previous
                    && point_at.y < bounds.top()
                {
                    let above = self.layout(previous)?.bounds();
                    if point_at.y - above.bottom() < bounds.top() - point_at.y {
                        chosen = Some(previous);
                    }
                }
                break;
            }
            previous = Some(index);
        }
        let index = chosen?;
        let index = if self.editor.block(index).kind.is_cell() {
            self.cell_at_x(index, point_at.x)
        } else {
            index
        };
        let block = self.editor.block(index);
        if block.text.is_empty() {
            return Some(Pos::new(index, 0));
        }
        let layout = self.layout(index)?;
        let bounds = layout.bounds();
        let clamped = point(
            point_at.x.clamp(bounds.left(), bounds.right() + px(2000.0)),
            point_at.y.clamp(bounds.top(), bounds.bottom() - px(1.0)),
        );
        let offset = match layout.index_for_position(clamped) {
            Ok(i) | Err(i) => i,
        };
        let offset = self.shown.get(index).map_or(offset, |s| s.to_model(offset));
        let text = &block.text;
        let mut offset = offset.min(text.len());
        while !text.is_char_boundary(offset) {
            offset -= 1;
        }
        Some(Pos::new(index, offset))
    }

    fn vertical_target(&mut self, down: bool) -> Option<Pos> {
        let head = self.editor.selection.head;
        let (caret, line_height) = self.caret_point(head)?;
        let x = *self.goal_x.get_or_insert(caret.x);
        let layout = self.layout(head.block)?;
        let bounds = layout.bounds();
        let y = if down {
            caret.y + line_height * 1.5
        } else {
            caret.y - line_height * 0.5
        };
        if y >= bounds.top() && y < bounds.bottom() {
            return self.hit(point(x, y));
        }
        // In a table ↑/↓ move by row in the same column and leave the table
        // at its first and last rows.
        if let Some(target) = self.table_vertical(down, x) {
            return target;
        }
        let count = self.editor.blocks().len();
        let mut index = head.block;
        loop {
            if down {
                if index + 1 >= count {
                    return Some(Pos::new(index, self.text_len(index)));
                }
                index += 1;
            } else {
                if index == 0 {
                    return Some(Pos::new(0, 0));
                }
                index -= 1;
            }
            if self.editor.block(index).kind != BlockKind::Divider && !self.editor.is_hidden(index)
            {
                break;
            }
        }
        let target = self.layout(index)?.bounds();
        let line = self.layout(index)?.line_height();
        let y = if down {
            target.top() + line * 0.5
        } else {
            target.bottom() - line * 0.5
        };
        self.hit(point(x, y))
    }

    fn visual_line_edge(&self, end: bool) -> Option<Pos> {
        let head = self.editor.selection.head;
        let (caret, line_height) = self.caret_point(head)?;
        let bounds = self.layout(head.block)?.bounds();
        let x = if end {
            bounds.right() + px(4000.0)
        } else {
            bounds.left() - px(1.0)
        };
        let _ = line_height;
        self.hit(point(x, caret.y + px(1.0)))
    }

    // -----------------------------------------------------------------------
    // Slash menu

    fn sync_slash(&mut self) {
        let Some(menu) = &self.slash else { return };
        let head = self.editor.selection.head;
        let valid = self.editor.selection.is_collapsed()
            && self.editor.block(head.block).id == menu.block_id
            && head.offset > menu.slash
            && self
                .editor
                .block(head.block)
                .text
                .get(menu.slash..menu.slash + 1)
                == Some("/");
        if !valid {
            self.slash = None;
            return;
        }
        let query = self.slash_query();
        if query.contains(char::is_whitespace) && self.slash_matches().is_empty()
            || query.len() > 24
        {
            self.slash = None;
            return;
        }
        let count = self.slash_matches().len();
        if let Some(menu) = &mut self.slash {
            menu.selected = menu.selected.min(count.saturating_sub(1));
        }
    }

    fn slash_query(&self) -> String {
        let Some(menu) = &self.slash else {
            return String::new();
        };
        let head = self.editor.selection.head;
        self.editor.block(head.block).text[menu.slash + 1..head.offset].to_lowercase()
    }

    fn slash_matches(&self) -> Vec<SlashItem> {
        slash_filter(&self.slash_query())
    }

    fn maybe_open_slash(&mut self) {
        let head = self.editor.selection.head;
        let block = self.editor.block(head.block);
        if matches!(block.kind, BlockKind::Title | BlockKind::Code)
            || block.kind.is_cell()
            || head.offset == 0
        {
            return;
        }
        let slash = head.offset - 1;
        if block.text.get(slash..head.offset) != Some("/") {
            return;
        }
        let before = block.text[..slash].chars().next_back();
        if before.is_some_and(|c| !c.is_whitespace()) {
            return;
        }
        self.slash = Some(SlashMenu {
            block_id: block.id,
            slash,
            selected: 0,
        });
    }

    fn apply_slash(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(item) = self.slash_matches().get(index).copied() else {
            self.slash = None;
            cx.notify();
            return;
        };
        let Some(menu) = self.slash.take() else {
            return;
        };
        let head = self.editor.selection.head;
        let now = now_ms();
        self.editor.set_selection(Selection {
            anchor: Pos::new(head.block, menu.slash),
            head,
        });
        self.editor.delete_selection(now);
        match item.action {
            SlashAction::Turn(turn) => {
                if let Turn::Kind(BlockKind::Callout(_)) = turn {
                    crate::telemetry::notes_event("notes.callout.added", "");
                }
                self.editor.turn_into(turn, now);
            }
            SlashAction::Image => self.pick_images(cx),
            SlashAction::Table => {
                self.editor.insert_empty_table(3, 3, now);
                crate::telemetry::notes_event("notes.table.inserted", "slash");
            }
            SlashAction::LinkNote => {
                self.editor.insert_text("[[", now);
                self.open_mention_menu(true);
            }
            SlashAction::Mention => {
                self.editor.insert_text("@", now);
                self.open_mention_menu(false);
            }
        }
        self.edited(cx);
    }

    /// `/image`: a native Open panel for pictures, inserted at the caret.
    fn pick_images(&mut self, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some(crate::i18n::t("notes.image.insert").into()),
        });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = paths.await else {
                return;
            };
            let _ = this.update(cx, |this, cx| this.insert_image_files(&paths, "picker", cx));
        })
        .detach();
    }

    /// Copies picture files into the note's assets and inserts them after
    /// the caret, in order. Anything that is not a picture is skipped.
    pub(crate) fn insert_image_files(
        &mut self,
        paths: &[std::path::PathBuf],
        source: &'static str,
        cx: &mut Context<Self>,
    ) -> usize {
        let Some(home) = self.assets.clone() else {
            return 0;
        };
        let mut inserted = 0;
        for path in paths.iter().filter(|p| diri_notes::store::is_image_path(p)) {
            match home.store.import_asset(&home.note_id, path) {
                Ok(src) => {
                    self.editor.insert_image(&src, "", now_ms());
                    inserted += 1;
                }
                Err(_) => crate::telemetry::notes_event("notes.image.failed", source),
            }
        }
        if inserted > 0 {
            crate::telemetry::notes_event("notes.image.added", source);
            self.edited(cx);
        }
        inserted
    }

    /// A picture on the pasteboard (a screenshot, an image copied from a
    /// browser) becomes an image in the note.
    fn paste_image(&mut self, image: &gpui::Image, cx: &mut Context<Self>) -> bool {
        let Some(home) = self.assets.clone() else {
            return false;
        };
        let extension = match image.format {
            gpui::ImageFormat::Png => "png",
            gpui::ImageFormat::Jpeg => "jpg",
            gpui::ImageFormat::Webp => "webp",
            gpui::ImageFormat::Gif => "gif",
            gpui::ImageFormat::Svg => "svg",
            gpui::ImageFormat::Bmp => "bmp",
            gpui::ImageFormat::Tiff => "tiff",
            _ => return false,
        };
        match home
            .store
            .save_asset(&home.note_id, &image.bytes, extension)
        {
            Ok(src) => {
                self.run(cx, |e, now| e.insert_image(&src, "", now));
                crate::telemetry::notes_event("notes.image.added", "paste");
                true
            }
            Err(_) => {
                crate::telemetry::notes_event("notes.image.failed", "paste");
                false
            }
        }
    }

    // -----------------------------------------------------------------------
    // Mention menu

    fn sync_mention(&mut self) {
        let Some(menu) = &self.mention else { return };
        let head = self.editor.selection.head;
        let block = self.editor.block(head.block);
        let trigger = menu.trigger();
        let valid = self.editor.selection.is_collapsed()
            && block.id == menu.block_id
            && head.offset >= menu.at + trigger.len()
            && block.text.get(menu.at..menu.at + trigger.len()) == Some(trigger)
            // Typing into an existing chip is editing, not mentioning.
            && mention::at(block, menu.at + 1, false).is_none();
        if !valid {
            self.mention = None;
            return;
        }
        let query = self.mention_query();
        let matches = self.mention_matches().len();
        if query.len() > MENTION_QUERY_MAX
            || query.contains('\n')
            || query.starts_with(' ')
            || (matches == 0 && query.ends_with("  "))
        {
            self.mention = None;
            return;
        }
        if let Some(menu) = &mut self.mention {
            menu.selected = menu.selected.min(matches.saturating_sub(1));
        }
    }

    fn mention_query(&self) -> String {
        let Some(menu) = &self.mention else {
            return String::new();
        };
        let head = self.editor.selection.head;
        let query = self
            .editor
            .block(head.block)
            .text
            .get(menu.at + menu.trigger().len()..head.offset)
            .unwrap_or_default();
        // `[[Title]]` typed out whole still matches its note.
        if menu.notes_only {
            query.trim_end_matches(']').to_owned()
        } else {
            query.to_owned()
        }
    }

    #[cfg(test)]
    pub(super) fn mention_open(&self) -> bool {
        self.mention.is_some()
    }

    pub(super) fn mention_matches(&self) -> Vec<MentionEntry> {
        let notes_only = self.mention.as_ref().is_some_and(|m| m.notes_only);
        let candidates: Vec<Candidate> = self
            .mentions
            .candidates()
            .into_iter()
            .filter(|c| !notes_only || matches!(c.target, MentionTarget::Note(_)))
            .collect();
        mention::rank(&self.mention_query(), &candidates, MENTION_LIMIT)
            .into_iter()
            .filter_map(|c| self.mentions.find(&c.target).cloned())
            .collect()
    }

    fn maybe_open_mention(&mut self) {
        let head = self.editor.selection.head;
        let block = self.editor.block(head.block);
        if matches!(block.kind, BlockKind::Title | BlockKind::Code) || head.offset == 0 {
            return;
        }
        let at = head.offset - 1;
        if block.text.get(at..head.offset) != Some("@") {
            return;
        }
        // `me@host` is an address, not a mention.
        let before = block.text[..at].chars().next_back();
        if before.is_some_and(|c| !c.is_whitespace() && !"([{\"'".contains(c)) {
            return;
        }
        self.mention = Some(MentionMenu {
            block_id: block.id,
            at,
            selected: 0,
            notes_only: false,
        });
    }

    /// `[[` typed: the same menu, offering notes to link.
    fn maybe_open_note_link(&mut self) {
        let head = self.editor.selection.head;
        let block = self.editor.block(head.block);
        if matches!(block.kind, BlockKind::Title | BlockKind::Code) || head.offset < 2 {
            return;
        }
        let at = head.offset - 2;
        if block.text.get(at..head.offset) != Some("[[") {
            return;
        }
        let before = block.text[..at].chars().next_back();
        if before.is_some_and(|c| !c.is_whitespace() && !"([{\"'".contains(c)) {
            return;
        }
        self.open_mention_menu(true);
    }

    /// Opens the mention menu on the trigger just before the caret.
    fn open_mention_menu(&mut self, notes_only: bool) {
        let head = self.editor.selection.head;
        let trigger = if notes_only { 2 } else { 1 };
        if head.offset < trigger {
            return;
        }
        self.slash = None;
        self.mention = Some(MentionMenu {
            block_id: self.editor.block(head.block).id,
            at: head.offset - trigger,
            selected: 0,
            notes_only,
        });
    }

    fn apply_mention(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(entry) = self.mention_matches().get(index).cloned() else {
            self.mention = None;
            cx.notify();
            return;
        };
        let Some(menu) = self.mention.take() else {
            return;
        };
        let head = self.editor.selection.head;
        self.editor.insert_mention(
            menu.at..head.offset,
            &entry.candidate.target,
            &entry.candidate.label,
            now_ms(),
        );
        crate::telemetry::notes_event(
            "notes.mention.inserted",
            match (&entry.candidate.target, menu.notes_only) {
                (MentionTarget::Session(_), _) => "session",
                (MentionTarget::Note(_), false) => "note",
                (MentionTarget::Note(_), true) => "note_link",
            },
        );
        self.edited(cx);
    }

    /// The chip under a window point, if any: a mention or a tool link.
    fn chip_at(&self, at: Point<Pixels>) -> Option<ChipHit> {
        let hidden = self.editor.hidden();
        for (index, block) in self.editor.blocks().iter().enumerate() {
            if hidden[index] || block.marks.is_empty() {
                continue;
            }
            let layout = self.layout(index)?;
            let shown = self.shown.get(index)?;
            let hits = |range: &Range<usize>| {
                chip_rects(layout, &shown.text, shown.range(range))
                    .iter()
                    .any(|rect| rect.dilate(px(CHIP_PAD_X)).contains(&at))
            };
            for chip in mention::in_block(block) {
                if hits(&chip.range) {
                    return Some(ChipHit::Mention(chip.target));
                }
            }
            for (range, _) in link_chips(block) {
                if hits(&range) {
                    let url = block.marks.iter().find_map(|m| match &m.style {
                        Style::Link(url) if m.range == range => Some(url.clone()),
                        _ => None,
                    })?;
                    return Some(ChipHit::Link(url));
                }
            }
        }
        None
    }

    // -----------------------------------------------------------------------
    // Actions

    fn run(&mut self, cx: &mut Context<Self>, f: impl FnOnce(&mut Editor, u64)) {
        let revision = self.editor.revision;
        f(&mut self.editor, now_ms());
        if self.editor.revision != revision {
            self.edited(cx);
        } else {
            self.moved(cx);
        }
    }

    fn backspace(&mut self, _: &Backspace, _: &mut Window, cx: &mut Context<Self>) {
        self.run(cx, |e, now| e.backspace(Granularity::Grapheme, now));
    }

    fn backspace_word(&mut self, _: &BackspaceWord, _: &mut Window, cx: &mut Context<Self>) {
        self.run(cx, |e, now| e.backspace(Granularity::Word, now));
    }

    fn backspace_line(&mut self, _: &BackspaceLine, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(start) = self.visual_line_edge(false)
            && self.editor.selection.is_collapsed()
            && start.block == self.editor.selection.head.block
            && start.offset < self.editor.selection.head.offset
        {
            let head = self.editor.selection.head;
            self.editor.set_selection(Selection {
                anchor: start,
                head,
            });
        }
        self.run(cx, |e, now| e.backspace(Granularity::Block, now));
    }

    fn delete(&mut self, _: &Delete, _: &mut Window, cx: &mut Context<Self>) {
        self.run(cx, |e, now| e.delete_forward(Granularity::Grapheme, now));
    }

    fn delete_word(&mut self, _: &DeleteWord, _: &mut Window, cx: &mut Context<Self>) {
        self.run(cx, |e, now| e.delete_forward(Granularity::Word, now));
    }

    pub(crate) fn newline(&mut self, _: &Newline, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(menu) = &self.table_menu {
            let selected = menu.selected;
            self.apply_table_row(selected, cx);
            return;
        }
        if self.work_panel_key(super::work_item::PanelKey::Enter, cx) {
            return;
        }
        if let Some(menu) = &self.mention {
            let selected = menu.selected;
            self.apply_mention(selected, cx);
            return;
        }
        if let Some(menu) = &self.slash {
            let selected = menu.selected;
            self.apply_slash(selected, cx);
            return;
        }
        self.run(cx, |e, now| e.enter(now));
    }

    fn soft_newline(&mut self, _: &SoftNewline, _: &mut Window, cx: &mut Context<Self>) {
        self.run(cx, |e, now| e.soft_break(now));
    }

    pub(super) fn indent(&mut self, _: &Indent, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(menu) = &self.mention {
            let selected = menu.selected;
            self.apply_mention(selected, cx);
            return;
        }
        if let Some(menu) = &self.slash {
            let selected = menu.selected;
            self.apply_slash(selected, cx);
            return;
        }
        if self.table_tab(true, cx) {
            return;
        }
        self.run(cx, |e, now| {
            e.indent(false, now);
        });
    }

    pub(super) fn outdent(&mut self, _: &Outdent, _: &mut Window, cx: &mut Context<Self>) {
        if self.table_tab(false, cx) {
            return;
        }
        self.run(cx, |e, now| {
            e.indent(true, now);
        });
    }

    fn horizontal(
        &mut self,
        forward: bool,
        granularity: Granularity,
        extend: bool,
        cx: &mut Context<Self>,
    ) {
        self.goal_x = None;
        self.editor.move_horizontal(forward, granularity, extend);
        self.moved(cx);
    }

    fn move_left(&mut self, _: &MoveLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.horizontal(false, Granularity::Grapheme, false, cx);
    }
    fn move_right(&mut self, _: &MoveRight, _: &mut Window, cx: &mut Context<Self>) {
        self.horizontal(true, Granularity::Grapheme, false, cx);
    }
    fn select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.horizontal(false, Granularity::Grapheme, true, cx);
    }
    fn select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        self.horizontal(true, Granularity::Grapheme, true, cx);
    }
    fn word_left(&mut self, _: &WordLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.horizontal(false, Granularity::Word, false, cx);
    }
    fn word_right(&mut self, _: &WordRight, _: &mut Window, cx: &mut Context<Self>) {
        self.horizontal(true, Granularity::Word, false, cx);
    }
    fn select_word_left(&mut self, _: &SelectWordLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.horizontal(false, Granularity::Word, true, cx);
    }
    fn select_word_right(&mut self, _: &SelectWordRight, _: &mut Window, cx: &mut Context<Self>) {
        self.horizontal(true, Granularity::Word, true, cx);
    }
    fn doc_start(&mut self, _: &DocStart, _: &mut Window, cx: &mut Context<Self>) {
        self.horizontal(false, Granularity::Document, false, cx);
    }
    fn doc_end(&mut self, _: &DocEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.horizontal(true, Granularity::Document, false, cx);
    }
    fn select_doc_start(&mut self, _: &SelectDocStart, _: &mut Window, cx: &mut Context<Self>) {
        self.horizontal(false, Granularity::Document, true, cx);
    }
    fn select_doc_end(&mut self, _: &SelectDocEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.horizontal(true, Granularity::Document, true, cx);
    }

    fn line_edge(&mut self, end: bool, extend: bool, cx: &mut Context<Self>) {
        let target = self.visual_line_edge(end).unwrap_or_else(|| {
            let head = self.editor.selection.head;
            Pos::new(head.block, if end { self.text_len(head.block) } else { 0 })
        });
        self.goal_x = None;
        self.editor.move_to(target, extend);
        self.moved(cx);
    }

    fn line_start(&mut self, _: &LineStart, _: &mut Window, cx: &mut Context<Self>) {
        self.line_edge(false, false, cx);
    }
    fn line_end(&mut self, _: &LineEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.line_edge(true, false, cx);
    }
    fn select_line_start(&mut self, _: &SelectLineStart, _: &mut Window, cx: &mut Context<Self>) {
        self.line_edge(false, true, cx);
    }
    fn select_line_end(&mut self, _: &SelectLineEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.line_edge(true, true, cx);
    }

    pub(super) fn vertical(&mut self, down: bool, extend: bool, cx: &mut Context<Self>) {
        let key = if down {
            super::work_item::PanelKey::Down
        } else {
            super::work_item::PanelKey::Up
        };
        if !extend && self.work_panel_key(key, cx) {
            return;
        }
        let table_rows = self.table_menu_len();
        if !extend && let Some(menu) = &mut self.table_menu {
            let count = table_rows;
            menu.selected = if down {
                (menu.selected + 1) % count
            } else {
                (menu.selected + count - 1) % count
            };
            cx.notify();
            return;
        }
        if !extend && self.mention.is_some() {
            let count = self.mention_matches().len().max(1);
            if let Some(menu) = &mut self.mention {
                menu.selected = if down {
                    (menu.selected + 1) % count
                } else {
                    (menu.selected + count - 1) % count
                };
            }
            cx.notify();
            return;
        }
        let count = self.slash_matches().len().max(1);
        if !extend && let Some(menu) = &mut self.slash {
            menu.selected = if down {
                (menu.selected + 1) % count
            } else {
                (menu.selected + count - 1) % count
            };
            cx.notify();
            return;
        }
        if !extend && !self.editor.selection.is_collapsed() {
            let pos = if down {
                self.editor.selection.end()
            } else {
                self.editor.selection.start()
            };
            self.editor.set_caret(pos);
        }
        let goal = self.goal_x;
        if let Some(target) = self.vertical_target(down) {
            let keep = self.goal_x.or(goal);
            self.editor.move_to(target, extend);
            self.goal_x = keep;
        }
        self.marked = None;
        self.sync_slash();
        self.sync_mention();
        self.touched(cx);
    }

    fn move_up(&mut self, _: &MoveUp, _: &mut Window, cx: &mut Context<Self>) {
        self.vertical(false, false, cx);
    }
    fn move_down(&mut self, _: &MoveDown, _: &mut Window, cx: &mut Context<Self>) {
        self.vertical(true, false, cx);
    }
    fn select_up(&mut self, _: &SelectUp, _: &mut Window, cx: &mut Context<Self>) {
        self.vertical(false, true, cx);
    }
    fn select_down(&mut self, _: &SelectDown, _: &mut Window, cx: &mut Context<Self>) {
        self.vertical(true, true, cx);
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.editor.select_all();
        self.moved(cx);
    }

    /// What ⌘C puts on the pasteboard: cells inside one table as TSV, so a
    /// spreadsheet takes them, else the selection as Markdown.
    fn copied_text(&self) -> String {
        self.editor
            .selection_tsv()
            .unwrap_or_else(|| self.editor.selected_markdown())
    }

    pub(super) fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        let text = self.copied_text();
        if !text.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    fn cut(&mut self, _: &Cut, _: &mut Window, cx: &mut Context<Self>) {
        let text = self.copied_text();
        if text.is_empty() {
            return;
        }
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        self.run(cx, |e, now| e.delete_selection(now));
    }

    pub(super) fn paste(&mut self, _: &Paste, _: &mut Window, cx: &mut Context<Self>) {
        let Some(item) = cx.read_from_clipboard() else {
            return;
        };
        let image = item.entries().iter().find_map(|entry| match entry {
            gpui::ClipboardEntry::Image(image) => Some(image.clone()),
            _ => None,
        });
        if let Some(image) = image
            && self.paste_image(&image, cx)
        {
            return;
        }
        let files: Vec<std::path::PathBuf> = item
            .entries()
            .iter()
            .filter_map(|entry| match entry {
                gpui::ClipboardEntry::ExternalPaths(paths) => Some(paths.paths().to_vec()),
                _ => None,
            })
            .flatten()
            .collect();
        if !files.is_empty() && self.insert_image_files(&files, "paste", cx) > 0 {
            return;
        }
        let Some(text) = item.text() else {
            return;
        };
        // Spreadsheet rows (Sheets, Excel, Numbers) and Markdown tables
        // become a table, or fill the one the caret is in.
        if text.contains(['\t', '\n', ',', '|']) {
            let before = self.editor.caret_table();
            let revision = self.editor.revision;
            if let Some(source) = self.editor.paste_table(&text, now_ms()) {
                crate::telemetry::notes_event("notes.table.pasted", source);
                if let Some(before) = before {
                    self.count_table_growth(&before);
                }
                if self.editor.revision != revision {
                    self.edited(cx);
                }
                return;
            }
        }
        // Pasting a URL over a selection links it, as Notion and Bear do.
        let trimmed = text.trim();
        if !self.editor.selection.is_collapsed() && is_url(trimmed) {
            let url = trimmed.to_owned();
            self.run(cx, |e, now| e.toggle_style(Style::Link(url), now));
            return;
        }
        // A bare URL becomes a link, titled when it opens a known tool.
        if is_url(trimmed) {
            let url = trimmed.to_owned();
            crate::telemetry::notes_event("notes.link.pasted", link_kind(&url));
            self.run(cx, |e, now| e.paste_url(&url, now));
            return;
        }
        self.run(cx, |e, now| e.paste(&text, now));
    }

    fn undo(&mut self, _: &Undo, _: &mut Window, cx: &mut Context<Self>) {
        self.run(cx, |e, _| {
            e.undo();
        });
    }

    fn redo(&mut self, _: &Redo, _: &mut Window, cx: &mut Context<Self>) {
        self.run(cx, |e, _| {
            e.redo();
        });
    }

    fn style(&mut self, style: Style, cx: &mut Context<Self>) {
        self.run(cx, |e, now| e.toggle_style(style, now));
        cx.notify();
    }

    fn bold(&mut self, _: &Bold, _: &mut Window, cx: &mut Context<Self>) {
        self.style(Style::Bold, cx);
    }
    fn italic(&mut self, _: &Italic, _: &mut Window, cx: &mut Context<Self>) {
        self.style(Style::Italic, cx);
    }
    fn inline_code(&mut self, _: &InlineCode, _: &mut Window, cx: &mut Context<Self>) {
        self.style(Style::Code, cx);
    }
    fn strike(&mut self, _: &Strike, _: &mut Window, cx: &mut Context<Self>) {
        self.style(Style::Strike, cx);
    }

    /// ⌘K links the selection to the URL on the clipboard, or unlinks.
    /// ⌘K opens the link panel on the selection, or on the link at the
    /// caret, prefilled with that link's URL (or a URL on the clipboard).
    pub(crate) fn link(&mut self, _: &Link, _: &mut Window, cx: &mut Context<Self>) {
        let selection = self.editor.selection;
        let existing = if selection.is_collapsed() {
            self.editor.link_at(selection.head)
        } else {
            self.editor.link_at(selection.start())
        };
        let prefill = existing
            .as_ref()
            .map(|(_, _, url)| url.clone())
            .or_else(|| {
                cx.read_from_clipboard()
                    .and_then(|item| item.text())
                    .map(|t| t.trim().to_owned())
                    .filter(|t| is_url(t))
            });
        let mut query = crate::query_editor::QueryEditor::default();
        if let Some(url) = prefill {
            query.insert(&url);
            query.select_all();
        }
        self.slash = None;
        self.mention = None;
        self.link_editor = Some(LinkEditor {
            query,
            selection,
            existing,
            selected: 0,
        });
        crate::telemetry::notes_event("notes.link_editor.opened", "");
        cx.notify();
    }

    #[cfg(test)]
    pub(super) fn link_row_labels(&self) -> Vec<String> {
        self.link_rows()
            .iter()
            .map(|row| match row {
                LinkRow::Apply { url } => format!("apply {url}"),
                LinkRow::Open { url } => format!("open {url}"),
                LinkRow::Remove => "remove".to_owned(),
            })
            .collect()
    }

    fn link_rows(&self) -> Vec<LinkRow> {
        let Some(editor) = &self.link_editor else {
            return Vec::new();
        };
        let mut rows = Vec::new();
        if let Some(url) = normalize_url(editor.query.text()) {
            let unchanged = editor.existing.as_ref().is_some_and(|(_, _, u)| *u == url);
            if !unchanged {
                rows.push(LinkRow::Apply { url });
            }
        }
        if let Some((_, _, url)) = &editor.existing {
            rows.push(LinkRow::Open { url: url.clone() });
            rows.push(LinkRow::Remove);
        }
        rows
    }

    fn close_link_editor(&mut self, cx: &mut Context<Self>) {
        if self.link_editor.take().is_some() {
            cx.notify();
        }
    }

    pub(super) fn apply_link_row(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(row) = self.link_rows().get(index).cloned() else {
            return;
        };
        let Some(editor) = self.link_editor.take() else {
            return;
        };
        let now = now_ms();
        let target = match (&editor.existing, editor.selection.is_collapsed()) {
            (Some((block, range, _)), true) => Selection {
                anchor: Pos::new(*block, range.start),
                head: Pos::new(*block, range.end),
            },
            _ => editor.selection,
        };
        match row {
            LinkRow::Open { url } => {
                cx.open_url(&url);
                cx.notify();
                return;
            }
            LinkRow::Remove => {
                self.editor.set_selection(target);
                self.editor.remove_link(now);
                self.editor.set_caret(target.end());
                crate::telemetry::notes_event("notes.link.removed", "");
            }
            LinkRow::Apply { url } => {
                self.editor.set_selection(target);
                let kind = link_kind(&url);
                if target.is_collapsed() {
                    self.editor.paste_url(&url, now);
                } else {
                    self.editor.set_link(&url, now);
                    self.editor.set_caret(target.end());
                }
                crate::telemetry::notes_event("notes.link.set", kind);
            }
        }
        self.edited(cx);
    }

    /// Keys while the link panel is open: its field edits with the shared
    /// query-field map; typed text arrives through the input handler.
    fn link_editor_key_down(
        &mut self,
        event: &KeyDownEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use crate::query_editor::{self, ClipboardEdit, Edit, LocalEdit};
        if self.link_editor.is_none() {
            return;
        }
        let keystroke = &event.keystroke;
        let rows = self.link_rows().len().max(1);
        let editor = self.link_editor.as_mut().expect("checked above");
        match keystroke.key.as_str() {
            "escape" => {
                self.link_editor = None;
            }
            "enter" if keystroke.modifiers.platform => {
                if let Some(index) = self
                    .link_rows()
                    .iter()
                    .position(|row| matches!(row, LinkRow::Open { .. }))
                {
                    self.apply_link_row(index, cx);
                }
            }
            "enter" => {
                let selected = editor.selected;
                self.apply_link_row(selected, cx);
            }
            "up" => editor.selected = (editor.selected + rows - 1) % rows,
            "down" => editor.selected = (editor.selected + 1) % rows,
            _ => match query_editor::edit_for(keystroke) {
                // Text goes through the input handler, like every field.
                None | Some(Edit::Local(LocalEdit::Insert(_))) => return,
                Some(Edit::Local(local)) => {
                    editor.query.apply(local);
                    editor.selected = 0;
                }
                Some(Edit::Clipboard(ClipboardEdit::Copy)) => {
                    query_editor::copy_selection(&editor.query, cx);
                }
                Some(Edit::Clipboard(ClipboardEdit::Cut)) => {
                    query_editor::cut_selection(&mut editor.query, cx);
                }
                Some(Edit::Clipboard(ClipboardEdit::Paste)) => {
                    if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                        editor
                            .query
                            .insert(text.trim().lines().next().unwrap_or_default());
                        editor.selected = 0;
                    }
                }
            },
        }
        cx.stop_propagation();
        cx.notify();
    }

    /// ⌥⌘↩ folds or unfolds the list item under the caret, or the nearest
    /// list item above it that has children.
    pub(super) fn toggle_fold(&mut self, _: &ToggleFold, _: &mut Window, cx: &mut Context<Self>) {
        let head = self.editor.selection.head.block;
        let target = (1..=head)
            .rev()
            .take_while(|i| self.editor.block(*i).kind.is_list())
            .find(|i| {
                self.editor.has_children(*i)
                    && (*i == head || self.editor.children(*i).contains(&head))
            });
        if let Some(index) = target {
            crate::telemetry::notes_event("notes.fold.toggled", "keyboard");
            self.set_folded(index, !self.editor.is_collapsed(index), cx);
        }
    }

    /// Folds or unfolds the children of the list item at `index`. View
    /// state only: nothing is saved and undo is untouched. Other note
    /// features (a to-do's work context) build on this.
    /// Starts this editor is waiting on, for fixtures that answer them.
    // Only the macOS What's New clip fixture uses this.
    #[cfg(all(test, target_os = "macos"))]
    pub(crate) fn work_tickets_for_test(&self) -> Vec<(diri_notes::doc::BlockId, u64)> {
        self.work.pending_tickets()
    }

    pub(crate) fn set_folded(&mut self, index: usize, folded: bool, cx: &mut Context<Self>) {
        self.editor.set_collapsed(index, folded);
        self.slash = None;
        self.mention = None;
        self.moved(cx);
    }

    fn toggle_todo(&mut self, _: &ToggleTodo, _: &mut Window, cx: &mut Context<Self>) {
        let head = self.editor.selection.head.block;
        if self.editor.selection.is_collapsed() && self.guard_tick(head, cx) {
            return;
        }
        let id = self.editor.block(head).id;
        self.run(cx, |e, now| e.toggle_todo(now));
        if self.editor.block(head).kind == (BlockKind::Todo { checked: true }) {
            self.ticked.push((id, Instant::now()));
        }
    }

    fn turn(&mut self, kind: BlockKind, cx: &mut Context<Self>) {
        self.run(cx, |e, now| e.turn_into(Turn::Kind(kind), now));
    }

    fn turn_paragraph(&mut self, _: &TurnParagraph, _: &mut Window, cx: &mut Context<Self>) {
        self.turn(BlockKind::Paragraph, cx);
    }
    fn turn_h1(&mut self, _: &TurnHeading1, _: &mut Window, cx: &mut Context<Self>) {
        self.turn(BlockKind::Heading(1), cx);
    }
    fn turn_h2(&mut self, _: &TurnHeading2, _: &mut Window, cx: &mut Context<Self>) {
        self.turn(BlockKind::Heading(2), cx);
    }
    fn turn_h3(&mut self, _: &TurnHeading3, _: &mut Window, cx: &mut Context<Self>) {
        self.turn(BlockKind::Heading(3), cx);
    }
    fn turn_bullet(&mut self, _: &TurnBullet, _: &mut Window, cx: &mut Context<Self>) {
        self.turn(BlockKind::Bullet, cx);
    }
    fn turn_numbered(&mut self, _: &TurnNumbered, _: &mut Window, cx: &mut Context<Self>) {
        self.turn(BlockKind::Numbered, cx);
    }
    fn turn_todo(&mut self, _: &TurnTodo, _: &mut Window, cx: &mut Context<Self>) {
        self.turn(BlockKind::Todo { checked: false }, cx);
    }
    fn turn_quote(&mut self, _: &TurnQuote, _: &mut Window, cx: &mut Context<Self>) {
        self.turn(BlockKind::Quote, cx);
    }
    fn turn_code(&mut self, _: &TurnCode, _: &mut Window, cx: &mut Context<Self>) {
        self.turn(BlockKind::Code, cx);
    }

    fn move_block_up(&mut self, _: &MoveBlockUp, _: &mut Window, cx: &mut Context<Self>) {
        self.run(cx, |e, now| e.move_blocks(true, now));
    }
    fn move_block_down(&mut self, _: &MoveBlockDown, _: &mut Window, cx: &mut Context<Self>) {
        self.run(cx, |e, now| e.move_blocks(false, now));
    }

    fn escape(&mut self, _: &Escape, _: &mut Window, cx: &mut Context<Self>) {
        if self.work_panel_key(super::work_item::PanelKey::Escape, cx) {
            return;
        }
        if self.slash.take().is_some()
            || self.mention.take().is_some()
            || self.link_editor.take().is_some()
            || self.table_menu.take().is_some()
            || self.block_menu.take().is_some()
        {
            cx.notify();
            return;
        }
        if !self.editor.selection.is_collapsed() {
            let head = self.editor.selection.head;
            self.editor.set_caret(head);
            self.moved(cx);
            return;
        }
        cx.emit(EditorEvent::Dismiss);
    }

    fn show_character_palette(
        &mut self,
        _: &ShowCharacterPalette,
        window: &mut Window,
        _: &mut Context<Self>,
    ) {
        window.show_character_palette();
    }

    fn start_work(&mut self, _: &StartWork, _: &mut Window, cx: &mut Context<Self>) {
        self.start_work_at_caret(cx);
    }

    fn check(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(block) = self.editor.blocks().get(index) else {
            return;
        };
        let BlockKind::Todo { checked } = block.kind else {
            return;
        };
        if self.guard_tick(index, cx) {
            return;
        }
        self.set_todo_checked(index, !checked, cx);
    }

    pub(super) fn set_todo_checked(&mut self, index: usize, checked: bool, cx: &mut Context<Self>) {
        let Some(block) = self.editor.blocks().get(index) else {
            return;
        };
        let was = block.kind == (BlockKind::Todo { checked: true });
        let id = block.id;
        if was == checked {
            return;
        }
        self.editor.set_checked(index, checked, now_ms());
        if checked {
            self.ticked.push((id, Instant::now()));
        }
        self.edited(cx);
    }

    // -----------------------------------------------------------------------
    // Pointer

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus, cx);
        if event.click_count == 1
            && !event.modifiers.shift
            && let Some(hit) = self.chip_at(event.position)
        {
            self.mention = None;
            self.slash = None;
            match hit {
                ChipHit::Mention(target) => cx.emit(EditorEvent::OpenMention(target)),
                ChipHit::Link(url) => cx.open_url(&url),
            }
            return;
        }
        let Some(pos) = self.hit(event.position) else {
            return;
        };
        if event.modifiers.platform
            && let Some(url) = self.link_at(pos)
        {
            cx.open_url(&url);
            return;
        }
        self.goal_x = None;
        match event.click_count {
            2 => {
                let text = &self.editor.block(pos.block).text;
                let range = word_range(text, pos.offset);
                self.editor.set_selection(Selection {
                    anchor: Pos::new(pos.block, range.start),
                    head: Pos::new(pos.block, range.end),
                });
            }
            n if n >= 3 => {
                let len = self.text_len(pos.block);
                self.editor.set_selection(Selection {
                    anchor: Pos::new(pos.block, 0),
                    head: Pos::new(pos.block, len),
                });
            }
            _ => {
                self.selecting = true;
                self.editor.move_to(pos, event.modifiers.shift);
            }
        }
        self.slash = None;
        self.mention = None;
        self.moved(cx);
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if !self.selecting || event.pressed_button != Some(MouseButton::Left) {
            return;
        }
        if let Some(pos) = self.hit(event.position)
            && pos != self.editor.selection.head
        {
            self.editor.move_to(pos, true);
            self.moved(cx);
        }
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.selecting = false;
        // A grip let go anywhere but on a block moves nothing.
        if self.dragging.take().is_some() {
            cx.notify();
        }
    }

    fn link_at(&self, pos: Pos) -> Option<String> {
        self.editor
            .block(pos.block)
            .marks
            .iter()
            .find_map(|mark| match &mark.style {
                Style::Link(url)
                    if mark.range.start <= pos.offset && pos.offset < mark.range.end =>
                {
                    Some(url.clone())
                }
                _ => None,
            })
    }

    // -----------------------------------------------------------------------
    // UTF-16 for the platform input handler, relative to the caret's block.

    fn ime_block(&self) -> usize {
        self.editor.selection.head.block
    }

    fn offset_to_utf16(&self, offset: usize) -> usize {
        let text = &self.editor.block(self.ime_block()).text;
        text[..offset.min(text.len())].encode_utf16().count()
    }

    fn utf16_to_offset(&self, utf16: usize) -> usize {
        let text = &self.editor.block(self.ime_block()).text;
        let mut count = 0;
        for (i, ch) in text.char_indices() {
            if count >= utf16 {
                return i;
            }
            count += ch.len_utf16();
        }
        text.len()
    }

    fn range_utf16_to_offset(&self, range: &Range<usize>) -> Range<usize> {
        self.utf16_to_offset(range.start)..self.utf16_to_offset(range.end)
    }
}

impl EntityInputHandler for NoteEditorView {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        adjusted: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.range_utf16_to_offset(&range_utf16);
        adjusted.replace(self.offset_to_utf16(range.start)..self.offset_to_utf16(range.end));
        Some(self.editor.block(self.ime_block()).text[range].to_owned())
    }

    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        let selection = self.editor.selection;
        let block = self.ime_block();
        let clamp = |pos: Pos| {
            if pos.block < block {
                0
            } else if pos.block > block {
                self.editor.block(block).text.len()
            } else {
                pos.offset
            }
        };
        let start = clamp(selection.start());
        let end = clamp(selection.end());
        Some(UTF16Selection {
            range: self.offset_to_utf16(start)..self.offset_to_utf16(end),
            reversed: selection.reversed(),
        })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked
            .as_ref()
            .map(|range| self.offset_to_utf16(range.start)..self.offset_to_utf16(range.end))
    }

    fn unmark_text(&mut self, _: &mut Window, _: &mut Context<Self>) {
        self.marked = None;
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(editor) = self.link_editor.as_mut() {
            editor.query.insert(text);
            editor.selected = 0;
            cx.notify();
            return;
        }
        let block = self.ime_block();
        let range = range_utf16
            .as_ref()
            .map(|r| self.range_utf16_to_offset(r))
            .or(self.marked.clone());
        if let Some(range) = range {
            self.editor.set_selection(Selection {
                anchor: Pos::new(block, range.start),
                head: Pos::new(block, range.end),
            });
        }
        self.marked = None;
        let was_slash_open = self.slash.is_some();
        let was_mention_open = self.mention.is_some();
        self.editor.insert_text(text, now_ms());
        if text == "/" && !was_slash_open {
            self.maybe_open_slash();
        }
        if text == "@" && !was_mention_open && !was_slash_open {
            self.maybe_open_mention();
        }
        if text == "[" && !was_mention_open && !was_slash_open {
            self.maybe_open_note_link();
        }
        self.edited(cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // A composition in the link field lands when it commits.
        if self.link_editor.is_some() {
            let _ = (new_text, new_selected_range_utf16);
            return;
        }
        let block = self.ime_block();
        let range = range_utf16
            .as_ref()
            .map(|r| self.range_utf16_to_offset(r))
            .or(self.marked.clone())
            .unwrap_or_else(|| {
                let sel = self.editor.selection;
                if sel.start().block == block && sel.end().block == block {
                    sel.start().offset..sel.end().offset
                } else {
                    sel.head.offset..sel.head.offset
                }
            });
        self.editor.set_selection(Selection {
            anchor: Pos::new(block, range.start),
            head: Pos::new(block, range.end),
        });
        // Composition text is provisional: it bypasses shortcuts and undo
        // coalescing by going through paste, which inserts it verbatim.
        self.editor.paste(new_text, now_ms());
        self.marked = (!new_text.is_empty()).then(|| range.start..range.start + new_text.len());
        if let Some(selected) = new_selected_range_utf16 {
            let base = self.offset_to_utf16(range.start);
            let start = self.utf16_to_offset(base + selected.start);
            let end = self.utf16_to_offset(base + selected.end);
            self.editor.set_selection(Selection {
                anchor: Pos::new(block, start),
                head: Pos::new(block, end),
            });
        }
        self.touched(cx);
        cx.emit(EditorEvent::Changed);
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        _: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let block = self.ime_block();
        let range = self.range_utf16_to_offset(&range_utf16);
        let (start, line) = self.caret_point(Pos::new(block, range.start))?;
        let (end, _) = self.caret_point(Pos::new(block, range.end))?;
        Some(Bounds::from_corners(
            start,
            point(end.x.max(start.x), start.y + line),
        ))
    }

    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        let pos = self.hit(point)?;
        (pos.block == self.ime_block()).then(|| self.offset_to_utf16(pos.offset))
    }
}

fn is_url(text: &str) -> bool {
    (text.starts_with("https://") || text.starts_with("http://") || text.starts_with("mailto:"))
        && !text.contains(char::is_whitespace)
}

fn word_range(text: &str, offset: usize) -> Range<usize> {
    use unicode_segmentation::UnicodeSegmentation;
    for (start, word) in text.split_word_bound_indices() {
        let end = start + word.len();
        if offset >= start && offset < end || (offset == end && end == text.len()) {
            return start..end;
        }
    }
    offset..offset
}

// ---------------------------------------------------------------------------
// Rendering

struct BlockLook {
    size: f32,
    line: f32,
    weight: FontWeight,
    top: f32,
    bottom: f32,
}

fn look(kind: BlockKind) -> BlockLook {
    let body = BlockLook {
        size: 15.0,
        line: 24.0,
        weight: FontWeight::NORMAL,
        top: 2.0,
        bottom: 2.0,
    };
    match kind {
        BlockKind::Title => BlockLook {
            size: 30.0,
            line: 38.0,
            weight: FontWeight::BOLD,
            top: 0.0,
            bottom: 14.0,
        },
        BlockKind::Heading(1) => BlockLook {
            size: 23.0,
            line: 30.0,
            weight: FontWeight::BOLD,
            top: 20.0,
            bottom: 4.0,
        },
        BlockKind::Heading(2) => BlockLook {
            size: 19.0,
            line: 26.0,
            weight: FontWeight::SEMIBOLD,
            top: 16.0,
            bottom: 3.0,
        },
        BlockKind::Heading(_) => BlockLook {
            size: 16.0,
            line: 24.0,
            weight: FontWeight::SEMIBOLD,
            top: 12.0,
            bottom: 2.0,
        },
        BlockKind::Code => BlockLook {
            size: 13.0,
            line: 20.0,
            top: 6.0,
            bottom: 6.0,
            ..body
        },
        BlockKind::Divider => BlockLook {
            top: 10.0,
            bottom: 10.0,
            ..body
        },
        BlockKind::Image | BlockKind::Callout(_) => BlockLook {
            top: 8.0,
            bottom: 8.0,
            ..body
        },
        _ => body,
    }
}

/// Pictures never grow taller than this; wider ones fit the column.
const IMAGE_MAX_HEIGHT: f32 = 480.0;
/// The ring around an image: padding plus border on each side.
const IMAGE_RING: f32 = 5.0;

/// A picture's size in the note: its natural size in points (pixels over
/// the display scale), shrunk to fit the column and the height cap, aspect
/// kept. An unknown size gets a 16:9 frame the width of the column.
fn image_fit(pixels: Option<(u32, u32)>, scale: f32) -> (f32, f32) {
    let column = MEASURE - 2.0 * IMAGE_RING;
    let Some((w, h)) = pixels.filter(|(w, h)| *w > 0 && *h > 0) else {
        return (column, column * 9.0 / 16.0);
    };
    let (w, h) = (w as f32, h as f32);
    let mut width = (w / scale.max(1.0)).min(column);
    let mut height = width * h / w;
    if height > IMAGE_MAX_HEIGHT {
        height = IMAGE_MAX_HEIGHT;
        width = height * w / h;
    }
    (width.max(1.0), height.max(1.0))
}

/// A callout tone's glyph and ink: GitHub's alert palette in diri's inks.
fn callout_look(tone: Tone, colors: SemanticColors) -> (&'static str, gpui::Rgba) {
    let purple = diri_ui::rgba_f32(0.56, 0.38, 0.93, 1.0);
    match tone {
        Tone::Note => ("info.circle", Ink::on_surface(Palette::GEMINI_BLUE, colors)),
        Tone::Tip => ("sparkle", Ink::on_surface(Ink::FRESH, colors)),
        Tone::Important => ("bell", Ink::on_surface(purple, colors)),
        Tone::Warning => (
            "exclamationmark.triangle",
            Ink::on_surface(Ink::ATTENTION, colors),
        ),
        Tone::Caution => ("xmark.circle", Ink::on_surface(Ink::DANGER, colors)),
    }
}

fn placeholder(kind: BlockKind, only_block: bool) -> &'static str {
    match kind {
        BlockKind::Title => crate::i18n::t("notes.untitled"),
        BlockKind::Heading(1) => crate::i18n::t("notes.block.heading_1"),
        BlockKind::Heading(2) => crate::i18n::t("notes.block.heading_2"),
        BlockKind::Heading(_) => crate::i18n::t("notes.block.heading_3"),
        BlockKind::Todo { .. } => crate::i18n::t("notes.block.todo"),
        BlockKind::Bullet | BlockKind::Numbered => crate::i18n::t("notes.placeholder.list"),
        BlockKind::Quote => crate::i18n::t("notes.block.quote"),
        BlockKind::Code => crate::i18n::t("notes.block.code"),
        BlockKind::Callout(tone) => tone.label(),
        BlockKind::Paragraph if only_block => crate::i18n::t("notes.placeholder.first"),
        _ => crate::i18n::t("notes.placeholder.paragraph"),
    }
}

/// Highlight runs for a block as laid out (`shown`): its marks moved to
/// layout offsets, plus `fades`, the laid-out ranges drawn faded (a chip's
/// padding and glyph room, a session chip's `@` under its dot).
fn highlights(
    block: &Block,
    shown: &Shown,
    colors: SemanticColors,
    faded: bool,
) -> Vec<(Range<usize>, HighlightStyle)> {
    // Inserts without marks are still drawn faded: a to-do's Start room.
    if block.marks.is_empty() && !faded && shown.inserts.is_empty() {
        return Vec::new();
    }
    let text = &shown.text;
    let marks: Vec<Mark> = block
        .marks
        .iter()
        .map(|m| Mark {
            range: shown.range(&m.range),
            style: m.style.clone(),
        })
        .collect();
    let fades = shown.fades(block);
    let mut cuts: Vec<usize> = vec![0, text.len()];
    for mark in &marks {
        cuts.push(mark.range.start);
        cuts.push(mark.range.end);
    }
    for (range, _) in &fades {
        cuts.push(range.start);
        cuts.push(range.end);
    }
    cuts.sort_unstable();
    cuts.dedup();
    let mut out = Vec::new();
    for pair in cuts.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        if a == b {
            continue;
        }
        let mut style = HighlightStyle::default();
        if faded {
            style.color = Some(colors.tertiary.into());
            style.strikethrough = Some(StrikethroughStyle {
                thickness: px(1.0),
                color: Some(colors.tertiary.into()),
            });
        }
        for mark in marks
            .iter()
            .filter(|m| m.range.start <= a && b <= m.range.end)
        {
            match &mark.style {
                Style::Bold => style.font_weight = Some(FontWeight::BOLD),
                Style::Italic => style.font_style = Some(FontStyle::Italic),
                Style::Strike => {
                    style.strikethrough = Some(StrikethroughStyle {
                        thickness: px(1.0),
                        color: None,
                    });
                }
                Style::Code => {
                    style.background_color = Some(colors.primary.alpha(0.07).into());
                    if !faded {
                        style.color = Some(accent().into());
                    }
                }
                Style::Link(url)
                    if MentionTarget::parse(url).is_some()
                        || diri_notes::links::recognize(url).is_some() =>
                {
                    style.font_weight = Some(FontWeight::MEDIUM);
                    if !faded {
                        style.color = Some(colors.primary.into());
                    }
                }
                Style::Link(_) => {
                    if !faded {
                        style.color = Some(accent().into());
                    }
                    style.underline = Some(UnderlineStyle {
                        thickness: px(1.0),
                        color: Some(accent().alpha(0.45).into()),
                        wavy: false,
                    });
                }
            }
        }
        // Highlight colors blend over the base, so only `fade_out` can hide
        // text: chip padding, glyph room, the session `@` under its dot.
        if let Some((_, fade)) = fades.iter().find(|(r, _)| r.start <= a && b <= r.end) {
            style.fade_out = Some(*fade);
        }
        out.push((a..b, style));
    }
    out
}

/// What one chip paints behind its text.
struct ChipPaint {
    block: usize,
    text: String,
    range: Range<usize>,
    /// Layout offset of the chip's first stored character.
    head: usize,
    dot: super::chip::ChipDot,
    /// A tool link's glyph, painted in the room laid out before its title.
    icon: Option<&'static str>,
}

trait AlphaExt {
    fn alpha(self, a: f32) -> Self;
}

impl AlphaExt for gpui::Rgba {
    fn alpha(self, a: f32) -> Self {
        gpui::Rgba {
            a: self.a * a,
            ..self
        }
    }
}

struct PaintState {
    selection: Vec<Bounds<Pixels>>,
    caret: Option<Bounds<Pixels>>,
}

impl Render for NoteEditorView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::perf_overlay::rendered("note editor");
        let colors = self.colors;
        let focused = self.focus.is_focused(window);
        self.focused_last_frame = focused;
        if focused && !self.blinking {
            self.restart_blink(cx);
        }
        let ui = crate::fonts::ui_family();
        let mono = crate::fonts::mono_family();
        let head = self.editor.selection.head;
        let only_block = self.editor.blocks().len() == 2 && self.editor.block(1).text.is_empty();
        self.ticked
            .retain(|(_, at)| at.elapsed() < Duration::from_millis(600));

        self.anchor_work_menu();
        self.place_block_menu();
        self.image_rects.borrow_mut().clear();
        let mut layouts = Vec::with_capacity(self.editor.blocks().len());
        let mut shown_all = Vec::with_capacity(self.editor.blocks().len());
        let hidden = self.editor.hidden();
        let mut column = div().flex().flex_col().w_full();
        // Only blocks near the viewport are laid out; the rest are spacers
        // sized from their last measured (or estimated) height. The caret's
        // and the anchor's blocks are always laid out so motion, autoscroll
        // and menus have their geometry.
        let rendered = self.visible_blocks(&hidden);
        let heights = Rc::clone(&self.row_heights);
        let mut spacer = 0.0f32;
        let mut table_end = 0;
        let autoscroll_now = self.autoscroll;
        for (index, block) in self.editor.blocks().iter().enumerate() {
            if index < table_end {
                continue;
            }
            // A table is drawn whole, as one grid of its cells.
            if block.kind.is_cell() {
                let frame =
                    self.render_table(index, &rendered, focused, colors, autoscroll_now, cx);
                table_end = index + frame.layouts.len();
                layouts.extend(frame.layouts);
                shown_all.extend(frame.shown);
                match frame.element {
                    Some(element) => {
                        if spacer > 0.0 {
                            column = column.child(div().flex_none().h(px(spacer)));
                            spacer = 0.0;
                        }
                        column = column.child(element);
                    }
                    None => spacer += frame.offscreen,
                }
                continue;
            }
            if !rendered[index] {
                layouts.push(None);
                shown_all.push(Shown::default());
                if !hidden[index] {
                    spacer += self.row_height(block);
                }
                continue;
            }
            if spacer > 0.0 {
                column = column.child(div().flex_none().h(px(spacer)));
                spacer = 0.0;
            }
            let look = look(block.kind);
            let checked = block.kind == BlockKind::Todo { checked: true };
            let shown = if block.kind.is_atomic() {
                Shown::default()
            } else {
                Shown::with_tail(block, self.wants_start(block).then_some(START_ROOM))
            };
            let text: SharedString = if block.text.is_empty() || block.kind.is_atomic() {
                "\u{200B}".into()
            } else {
                shown.text.clone().into()
            };
            let mut styled = StyledText::new(text).with_highlights(
                if block.text.is_empty() || block.kind.is_atomic() {
                    Vec::new()
                } else {
                    highlights(block, &shown, colors, checked)
                },
            );
            let code_ranges: Vec<(Range<usize>, SharedString)> = block
                .marks
                .iter()
                .filter(|m| m.style == Style::Code)
                .map(|m| (shown.range(&m.range), SharedString::from(mono)))
                .collect();
            if !code_ranges.is_empty() {
                styled = styled.with_font_family_overrides(code_ranges);
            }
            layouts.push(Some(styled.layout().clone()));
            shown_all.push(shown);

            let show_placeholder = block.text.is_empty()
                && block.kind != BlockKind::Divider
                && (block.kind == BlockKind::Title
                    || block.kind != BlockKind::Paragraph
                    || (focused && head.block == index));
            let text_color = if checked {
                colors.tertiary
            } else if matches!(block.kind, BlockKind::Quote) {
                colors.primary.alpha(0.82)
            } else {
                colors.primary
            };
            let mut content = div()
                .relative()
                .flex_1()
                .min_w_0()
                .text_size(px(look.size))
                .line_height(px(look.line))
                .font_weight(look.weight)
                .text_color(text_color)
                .font_family(if block.kind == BlockKind::Code {
                    mono
                } else {
                    ui
                })
                .child(styled);
            // Start follows the to-do's last word, placed from the last
            // frame's layout of the room reserved for it.
            content = content.children(self.work_accessory(
                index,
                block,
                focused && head.block == index,
                format!("note-row-{}", block.id).into(),
                look.line,
                colors,
                cx,
            ));
            if show_placeholder {
                content = content.child(
                    div()
                        .absolute()
                        .top_0()
                        .left_0()
                        .text_color(colors.tertiary.alpha(if block.kind == BlockKind::Title {
                            0.9
                        } else {
                            0.8
                        }))
                        .child(placeholder(block.kind, only_block)),
                );
            }

            let indent = f32::from(block.indent) * INDENT_STEP;
            let folded_away = hidden[index];
            let group: SharedString = format!("note-row-{}", block.id).into();
            let row = div()
                .id(("block", block.id))
                .group(group.clone())
                .relative()
                .flex()
                .flex_row()
                .items_start()
                .w_full()
                .pl(px(indent));
            // A folded-away block still lays out its text, so every block
            // keeps a layout, but it takes no space and cannot be seen.
            let row = if folded_away {
                row.h(px(0.0)).overflow_hidden().opacity(0.0)
            } else {
                row.pt(px(look.top)).pb(px(look.bottom))
            };
            let row = if !folded_away && self.editor.has_children(index) {
                row.child(self.disclosure(index, indent, look.top, look.line, group.clone(), cx))
            } else {
                row
            };
            let row = match block.kind {
                BlockKind::Bullet | BlockKind::Numbered | BlockKind::Todo { .. } => {
                    let marker = self.marker(index, block, look.line, cx);
                    row.child(marker).child(content)
                }
                BlockKind::Quote => row.child(
                    div()
                        .flex()
                        .flex_row()
                        .flex_1()
                        .min_w_0()
                        .child(
                            div()
                                .w(px(3.0))
                                .mr(px(14.0))
                                .rounded(px(2.0))
                                .bg(accent().alpha(0.55))
                                .self_stretch(),
                        )
                        .child(content.italic()),
                ),
                BlockKind::Code => row.child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .rounded(px(8.0))
                        .bg(colors.primary.alpha(0.05))
                        .border_1()
                        .border_color(colors.primary.alpha(0.06))
                        .px(px(14.0))
                        .py(px(10.0))
                        .child(content),
                ),
                BlockKind::Divider => row.child(
                    div()
                        .relative()
                        .flex_1()
                        .h(px(look.line))
                        .flex()
                        .items_center()
                        .child(div().w_full().h(px(1.0)).bg(colors.primary.alpha(0.12)))
                        .child(
                            div()
                                .absolute()
                                .top_0()
                                .left_0()
                                .w_full()
                                .child(content.opacity(0.0)),
                        ),
                ),
                BlockKind::Image => {
                    let selected =
                        focused && self.editor.selection.is_collapsed() && head.block == index;
                    let scale = window.scale_factor();
                    row.child(self.image_row(index, block, selected, scale, content, cx))
                }
                BlockKind::Callout(tone) => {
                    let (icon, ink) = callout_look(tone, colors);
                    let id = block.id;
                    row.child(
                        div()
                            .flex()
                            .flex_row()
                            .flex_1()
                            .min_w_0()
                            .gap(px(10.0))
                            .px(px(14.0))
                            .py(px(10.0))
                            .rounded(px(10.0))
                            .bg(ink.alpha(0.08))
                            .border_1()
                            .border_color(ink.alpha(0.16))
                            .child(
                                div()
                                    .id(("callout-tone", id))
                                    .flex_none()
                                    .h(px(look.line))
                                    .flex()
                                    .items_center()
                                    .cursor_pointer()
                                    .child(crate::icons::sf_symbol(icon, 13.0, ink))
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                                            cx.stop_propagation();
                                            this.cycle_callout(id, cx);
                                        }),
                                    ),
                            )
                            .child(content),
                    )
                }
                _ => row.child(content),
            };
            // The handle in the gutter, and where a dragged block would land.
            let row = if !folded_away && index > 0 {
                let gutter = if self.editor.has_children(index) {
                    indent - DISCLOSURE_WIDTH
                } else {
                    indent
                };
                let target = block.id;
                row.child(self.block_handle(index, gutter, look.top, look.line, group.clone(), cx))
                    .children(self.drop_line(index, group.clone()))
                    .on_drop(cx.listener(move |this, drag: &BlockDrag, _, cx| {
                        this.drop_block(drag.block_id, target, cx);
                    }))
            } else {
                row
            };
            let id = block.id;
            let heights = Rc::clone(&heights);
            let row = row.child(
                canvas(
                    move |bounds, _, _| {
                        heights
                            .borrow_mut()
                            .insert(id, f32::from(bounds.size.height));
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .inset_0(),
            );
            column = column.child(row.debug_selector(move || format!("note-row-{id}")));
            if !folded_away && let Some(status) = self.work_status_line(block, indent, colors, cx) {
                column = column.child(status);
            }
        }
        if spacer > 0.0 {
            column = column.child(div().flex_none().h(px(spacer)));
        }
        self.layouts = layouts.clone();
        self.shown = shown_all.clone();
        self.layout_revision = self.editor.revision;
        self.layout_count = self.editor.blocks().len();

        let mut chips = Vec::new();
        for (index, block) in self.editor.blocks().iter().enumerate() {
            if hidden[index] || !rendered[index] {
                continue;
            }
            let shown = &shown_all[index];
            for chip in mention::in_block(block) {
                let dot = super::chip::ChipDot::for_target(&chip.target, &self.mentions, colors);
                chips.push(ChipPaint {
                    block: index,
                    text: shown.text.clone(),
                    range: shown.range(&chip.range),
                    head: shown.head_of(chip.range.start),
                    dot,
                    icon: None,
                });
            }
            for (range, found) in link_chips(block) {
                chips.push(ChipPaint {
                    block: index,
                    text: shown.text.clone(),
                    range: shown.range(&range),
                    head: shown.head_of(range.start),
                    dot: super::chip::ChipDot::None,
                    icon: Some(service_icon(found.service)),
                });
            }
        }
        let icon_ink = colors.secondary;
        let chip_layouts = layouts.clone();
        let chip_fill = super::chip::fill(colors);
        let chip_border = super::chip::border(colors);
        let chip_backdrop = canvas(
            |_, _, _| {},
            move |_, _, window, cx| {
                for chip in &chips {
                    let Some(Some(layout)) = chip_layouts.get(chip.block) else {
                        continue;
                    };
                    for rect in chip_rects(layout, &chip.text, chip.range.clone()) {
                        let rect = Bounds::from_corners(
                            point(rect.left(), rect.top() + px(1.0)),
                            point(rect.right(), rect.bottom() - px(1.0)),
                        );
                        window.paint_quad(
                            fill(rect, chip_fill)
                                .corner_radii(px(super::chip::RADIUS))
                                .border_widths(px(super::chip::BORDER))
                                .border_color(chip_border),
                        );
                    }
                    if let Some(icon) = chip.icon
                        && let Some(room) = chip_rects(
                            layout,
                            &chip.text,
                            chip.range.start + CHIP_EDGE.len()
                                ..chip.range.start + CHIP_EDGE.len() + CHIP_ICON_ROOM.len(),
                        )
                        .first()
                    {
                        let side = px(CHIP_ICON);
                        let center = room.center();
                        let bounds = Bounds::new(
                            point(center.x - side / 2.0, center.y - side / 2.0),
                            size(side, side),
                        );
                        let _ = window.paint_svg(
                            bounds,
                            icon.into(),
                            None,
                            gpui::TransformationMatrix::unit(),
                            icon_ink.into(),
                            cx,
                        );
                        continue;
                    }
                    let (ink, hollow) = match chip.dot {
                        super::chip::ChipDot::Status(ink) => (ink, false),
                        super::chip::ChipDot::Gone => (colors.tertiary, true),
                        super::chip::ChipDot::None => continue,
                    };
                    let Some(at) = char_rect(layout, &chip.text, chip.head) else {
                        continue;
                    };
                    let center = at.center();
                    let dot = Bounds::new(
                        point(center.x - px(CHIP_DOT / 2.0), center.y - px(CHIP_DOT / 2.0)),
                        size(px(CHIP_DOT), px(CHIP_DOT)),
                    );
                    let quad = if hollow {
                        fill(dot, gpui::transparent_black())
                            .border_widths(px(1.25))
                            .border_color(ink)
                    } else {
                        fill(dot, ink)
                    };
                    window.paint_quad(quad.corner_radii(px(CHIP_DOT / 2.0)));
                }
            },
        )
        .absolute()
        .top_0()
        .left_0()
        .size_full();

        let entity = cx.entity();
        let selection = self.editor.selection;
        let caret_on = focused && self.caret_visible && self.marked.is_none()
            || (focused && self.marked.is_some());
        let focus = self.focus.clone();
        let selection_color = accent().alpha(if focused { 0.28 } else { 0.14 });
        let caret_color = accent();
        let blocks_meta: Vec<(usize, bool)> = self
            .editor
            .blocks()
            .iter()
            .map(|b| (b.text.len(), b.text.is_empty()))
            .collect();
        let folded_away = hidden.clone();
        // A selected image or divider shows as selected, not with a caret.
        let head_atomic = self.editor.block(selection.head.block).kind.is_atomic();
        let overlay_shown = shown_all;
        let autoscroll = std::mem::take(&mut self.autoscroll);
        let scroll = self.scroll.clone();
        let overlay = canvas(
            move |_, _, _| {
                let mut rects = Vec::new();
                let start = selection.start();
                let end = selection.end();
                if !selection.is_collapsed() {
                    #[allow(clippy::needless_range_loop)]
                    for index in start.block..=end.block {
                        let Some(Some(layout)) = layouts.get(index) else {
                            continue;
                        };
                        if folded_away[index] {
                            continue;
                        }
                        let (len, empty) = blocks_meta[index];
                        let from = if index == start.block {
                            start.offset
                        } else {
                            0
                        };
                        let to = if index == end.block { end.offset } else { len };
                        let shown = &overlay_shown[index];
                        let (from, to) = (shown.to_display(from), shown.to_display(to));
                        rects.extend(selection_rects(layout, from, to, empty, index != end.block));
                    }
                }
                // The head's position is tracked for every selection (menus
                // anchor to it, autoscroll follows it); only a collapsed one
                // paints a caret.
                let caret = {
                    layouts
                        .get(selection.head.block)
                        .and_then(Option::as_ref)
                        .and_then(|layout| {
                            let (_, empty) = blocks_meta[selection.head.block];
                            let offset = if empty {
                                0
                            } else {
                                overlay_shown[selection.head.block]
                                    .to_display(selection.head.offset)
                            };
                            let at = layout.position_for_index(offset)?;
                            Some(Bounds::new(at, size(px(2.0), layout.line_height())))
                        })
                };
                PaintState {
                    selection: rects,
                    caret,
                }
            },
            move |bounds, state, window, cx| {
                window.handle_input(&focus, ElementInputHandler::new(bounds, entity.clone()), cx);
                for rect in &state.selection {
                    window.paint_quad(fill(*rect, selection_color).corner_radii(px(3.0)));
                }
                if let Some(caret) = state.caret {
                    if caret_on && selection.is_collapsed() && !head_atomic {
                        window.paint_quad(fill(caret, caret_color));
                    }
                    if autoscroll {
                        scroll_into_view(&scroll, caret, window);
                    }
                    entity.update(cx, |this, _| this.caret_bounds = Some(caret));
                }
            },
        )
        .absolute()
        .top_0()
        .left_0()
        .size_full();

        let slash_menu = if self.slash.is_some() {
            let height = self.slash_menu_height();
            self.host_menu(
                SLASH_MENU,
                Self::slash_menu_rows,
                SLASH_MENU_WIDTH,
                height,
                window,
                cx,
            )
        } else {
            None
        };
        let table_menu = if self.table_menu.is_some() && self.editor.caret_table().is_some() {
            let height = self.table_menu_height();
            self.host_menu(TABLE_MENU, Self::table_menu_rows, 248.0, height, window, cx)
        } else {
            self.table_menu = None;
            None
        };
        let link_menu = if self.link_editor.is_some() {
            let height = self.link_menu_height();
            self.host_menu(
                LINK_MENU,
                Self::link_menu_rows,
                LINK_MENU_WIDTH,
                height,
                window,
                cx,
            )
        } else {
            None
        };
        let mention_menu = if self.mention.is_some() {
            let height = self.mention_menu_height();
            self.host_menu(
                MENTION_MENU,
                Self::mention_menu_rows,
                MENTION_MENU_WIDTH,
                height,
                window,
                cx,
            )
        } else {
            None
        };
        let block_menu = if self.block_menu.is_some() {
            let height = self.block_menu_height();
            self.host_menu(
                BLOCK_MENU,
                Self::block_menu_rows,
                Self::block_menu_width(),
                height,
                window,
                cx,
            )
        } else {
            None
        };
        let work_menu = self.work_menu(window, cx);
        let footer = self.footer.clone().map(|footer| {
            div()
                .w_full()
                .max_w(px(MEASURE))
                .mx_auto()
                .mt(px(56.0))
                .cursor_default()
                .debug_selector(|| "note-footer".into())
                .child(footer)
        });

        div()
            .id("note-editor")
            .key_context(if self.link_editor.is_some() {
                LINK_EDITOR_CONTEXT
            } else {
                EDITOR_CONTEXT
            })
            .on_key_down(cx.listener(Self::link_editor_key_down))
            .track_focus(&self.focus)
            .relative()
            .size_full()
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::backspace_word))
            .on_action(cx.listener(Self::backspace_line))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::delete_word))
            .on_action(cx.listener(Self::newline))
            .on_action(cx.listener(Self::soft_newline))
            .on_action(cx.listener(Self::indent))
            .on_action(cx.listener(Self::outdent))
            .on_action(cx.listener(Self::move_left))
            .on_action(cx.listener(Self::move_right))
            .on_action(cx.listener(Self::move_up))
            .on_action(cx.listener(Self::move_down))
            .on_action(cx.listener(Self::select_left))
            .on_action(cx.listener(Self::select_right))
            .on_action(cx.listener(Self::select_up))
            .on_action(cx.listener(Self::select_down))
            .on_action(cx.listener(Self::word_left))
            .on_action(cx.listener(Self::word_right))
            .on_action(cx.listener(Self::select_word_left))
            .on_action(cx.listener(Self::select_word_right))
            .on_action(cx.listener(Self::line_start))
            .on_action(cx.listener(Self::line_end))
            .on_action(cx.listener(Self::select_line_start))
            .on_action(cx.listener(Self::select_line_end))
            .on_action(cx.listener(Self::doc_start))
            .on_action(cx.listener(Self::doc_end))
            .on_action(cx.listener(Self::select_doc_start))
            .on_action(cx.listener(Self::select_doc_end))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::cut))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::undo))
            .on_action(cx.listener(Self::redo))
            .on_action(cx.listener(Self::bold))
            .on_action(cx.listener(Self::italic))
            .on_action(cx.listener(Self::inline_code))
            .on_action(cx.listener(Self::strike))
            .on_action(cx.listener(Self::link))
            .on_action(cx.listener(Self::toggle_todo))
            .on_action(cx.listener(Self::toggle_fold))
            .on_action(cx.listener(Self::table_row_above))
            .on_action(cx.listener(Self::table_row_below))
            .on_action(cx.listener(Self::table_col_left))
            .on_action(cx.listener(Self::table_col_right))
            .on_action(cx.listener(Self::table_menu_key))
            .on_action(cx.listener(Self::turn_paragraph))
            .on_action(cx.listener(Self::turn_h1))
            .on_action(cx.listener(Self::turn_h2))
            .on_action(cx.listener(Self::turn_h3))
            .on_action(cx.listener(Self::turn_bullet))
            .on_action(cx.listener(Self::turn_numbered))
            .on_action(cx.listener(Self::turn_todo))
            .on_action(cx.listener(Self::turn_quote))
            .on_action(cx.listener(Self::turn_code))
            .on_action(cx.listener(Self::move_block_up))
            .on_action(cx.listener(Self::move_block_down))
            .on_action(cx.listener(Self::escape))
            .on_action(cx.listener(Self::show_character_palette))
            .on_action(cx.listener(Self::start_work))
            .child(
                div()
                    .id("note-editor-scroll")
                    .size_full()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .cursor_text()
                    .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
                    .on_mouse_move(cx.listener(Self::on_mouse_move))
                    .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
                    .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
                    .drag_over::<gpui::ExternalPaths>(|area, _, _, _| area.bg(accent().alpha(0.04)))
                    .on_drop(
                        cx.listener(|this, paths: &gpui::ExternalPaths, window, cx| {
                            cx.stop_propagation();
                            this.drop_files(paths, window, cx);
                        }),
                    )
                    .child(
                        // Block layout, centred by auto margins: as a flex
                        // column's item the page took its height from wrapped
                        // text measured wider than it is drawn, ending short
                        // of its last rows, so the backlinks painted over them
                        // and the note would not scroll to its end.
                        div()
                            .w_full()
                            .px(px(PAGE_SIDE))
                            .pt(px(PAGE_TOP))
                            .pb(px(240.0))
                            .child(
                                div()
                                    .relative()
                                    .w_full()
                                    .max_w(px(MEASURE))
                                    .mx_auto()
                                    .child(chip_backdrop)
                                    .child(column)
                                    .child(overlay),
                            )
                            .children(footer),
                    ),
            )
            .children(slash_menu)
            .children(mention_menu)
            .children(work_menu)
            .children(link_menu)
            .children(table_menu)
            .children(block_menu)
    }
}

impl NoteEditorView {
    /// A picture: the file fitted to the column, a ring while selected, and
    /// a quiet placeholder when the file is missing or lives on the web.
    fn image_row(
        &self,
        index: usize,
        block: &Block,
        selected: bool,
        scale: f32,
        content: gpui::Div,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        use gpui::StyledImage as _;
        let colors = self.colors;
        let _ = cx;
        let path = self
            .assets
            .as_ref()
            .and_then(|home| home.store.resolve_asset(&block.src));
        let placeholder = |label: String| {
            div()
                .h(px(44.0))
                .px(px(12.0))
                .flex()
                .items_center()
                .gap(px(8.0))
                .rounded(px(8.0))
                .bg(colors.primary.alpha(0.05))
                .border_1()
                .border_color(colors.primary.alpha(0.08))
                .text_size(px(13.0))
                .text_color(colors.tertiary)
                .child(crate::icons::sf_symbol("photo", 13.0, colors.tertiary))
                .child(label)
                .into_any_element()
        };
        let picture = match path {
            Some(path) => {
                let missing = placeholder(crate::i18n::t("notes.image.missing").into());
                let missing = std::cell::RefCell::new(Some(missing));
                let pixels = *self
                    .image_sizes
                    .borrow_mut()
                    .entry(path.clone())
                    .or_insert_with(|| diri_notes::store::image_size(&path));
                let (width, height) = image_fit(pixels, scale);
                gpui::img(path)
                    .w(px(width))
                    .h(px(height))
                    .object_fit(gpui::ObjectFit::Contain)
                    .rounded(px(8.0))
                    .with_fallback(move || {
                        missing
                            .borrow_mut()
                            .take()
                            .unwrap_or_else(|| div().into_any_element())
                    })
                    .into_any_element()
            }
            None => {
                let label = if block.src.contains("://") {
                    crate::i18n::tf("notes.image.web", &[("url", &short_url(&block.src))])
                } else {
                    crate::i18n::t("notes.image.missing").to_owned()
                };
                placeholder(label)
            }
        };
        let rects = Rc::clone(&self.image_rects);
        div()
            .relative()
            .flex()
            .flex_row()
            .items_start()
            .flex_1()
            .min_w_0()
            .child(
                div()
                    .relative()
                    .flex()
                    .flex_none()
                    .p(px(3.0))
                    .rounded(px(11.0))
                    .border_2()
                    .border_color(if selected {
                        accent().alpha(0.7)
                    } else {
                        accent().alpha(0.0)
                    })
                    .child(picture)
                    .child(
                        canvas(
                            move |bounds, _, _| rects.borrow_mut().push((index, bounds)),
                            |_, _, _, _| {},
                        )
                        .absolute()
                        .inset_0(),
                    ),
            )
            .child(
                div()
                    .absolute()
                    .top_0()
                    .left_0()
                    .child(content.opacity(0.0)),
            )
            .into_any_element()
    }

    /// Clicking a callout's glyph steps it to the next tone.
    pub(super) fn cycle_callout(&mut self, id: u64, cx: &mut Context<Self>) {
        let Some(index) = self.editor.blocks().iter().position(|b| b.id == id) else {
            return;
        };
        let BlockKind::Callout(tone) = self.editor.block(index).kind else {
            return;
        };
        let at = Tone::ALL.iter().position(|t| *t == tone).unwrap_or(0);
        let next = Tone::ALL[(at + 1) % Tone::ALL.len()];
        self.run(cx, |e, now| {
            e.set_block_kind(index, BlockKind::Callout(next), now)
        });
    }

    /// Pictures dropped from Finder land after the block under the pointer.
    fn drop_files(
        &mut self,
        paths: &gpui::ExternalPaths,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(pos) = self.hit(window.mouse_position()) {
            self.editor
                .set_caret(Pos::new(pos.block, self.text_len(pos.block)));
        }
        self.insert_image_files(paths.paths(), "drop", cx);
    }

    /// Which blocks this frame lays out: those within a screen of the
    /// viewport (found by walking measured or estimated heights), the title,
    /// and the blocks holding the selection's ends. Folded blocks never are.
    fn visible_blocks(&self, hidden: &[bool]) -> Vec<bool> {
        let blocks = self.editor.blocks();
        let viewport = f32::from(self.scroll.bounds().size.height);
        let viewport = if viewport > 0.0 { viewport } else { 1200.0 };
        let top = -f32::from(self.scroll.offset().y) - PAGE_TOP;
        let (lo, hi) = (top - viewport, top + 2.0 * viewport);
        let selection = self.editor.selection;
        let mut y = 0.0;
        let mut rendered = vec![false; blocks.len()];
        let mut index = 0;
        while index < blocks.len() {
            if hidden[index] {
                index += 1;
                continue;
            }
            // A table row is one unit: its cells sit side by side.
            let unit = match blocks[index].kind.cell() {
                Some(cell) => index..(index + usize::from(cell.cols.max(1))).min(blocks.len()),
                None => index..index + 1,
            };
            let height = if blocks[index].kind.is_cell() {
                self.table_row_height(index)
            } else {
                self.row_height(&blocks[index])
            };
            let holds_selection =
                |i: usize| i == selection.head.block || i == selection.anchor.block;
            let show =
                (y + height >= lo && y <= hi) || index == 0 || unit.clone().any(holds_selection);
            for i in unit.clone() {
                rendered[i] = show;
            }
            y += height;
            index = unit.end;
        }
        rendered
    }

    /// A block's height from its last paint, or an estimate from its kind
    /// and text length until it has been on screen.
    fn row_height(&self, block: &Block) -> f32 {
        if let Some(height) = self.row_heights.borrow().get(&block.id) {
            return *height;
        }
        let look = look(block.kind);
        let chrome = look.top + look.bottom;
        match block.kind {
            BlockKind::Divider => chrome + look.line,
            BlockKind::Image => chrome + 2.0 * IMAGE_RING + image_fit(None, 2.0).1,
            BlockKind::Code => {
                chrome + 20.0 + look.line * (block.text.lines().count().max(1) as f32)
            }
            _ => {
                let per_line = (MEASURE / (look.size * 0.5)).max(1.0);
                let lines: f32 = block
                    .text
                    .split('\n')
                    .map(|line| (line.chars().count() as f32 / per_line).ceil().max(1.0))
                    .sum();
                let callout = if matches!(block.kind, BlockKind::Callout(_)) {
                    20.0
                } else {
                    0.0
                };
                chrome + callout + look.line * lines
            }
        }
    }

    /// The fold chevron in the gutter left of a list item with children:
    /// shown while the row is hovered, and always while folded.
    fn disclosure(
        &self,
        index: usize,
        indent: f32,
        top: f32,
        line: f32,
        group: SharedString,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let folded = self.editor.is_collapsed(index);
        let id = self.editor.block(index).id;
        let colors = self.colors;
        div()
            .id(("fold", id))
            .absolute()
            .left(px(indent - DISCLOSURE_WIDTH))
            .top(px(top))
            .w(px(DISCLOSURE_WIDTH))
            .h(px(line))
            .flex()
            .items_center()
            .justify_center()
            .cursor_pointer()
            .opacity(if folded { 1.0 } else { 0.0 })
            .group_hover(group, |chevron| chevron.opacity(1.0))
            .child(crate::icons::sf_symbol(
                if folded {
                    "chevron.right"
                } else {
                    "chevron.down"
                },
                9.0,
                colors.tertiary,
            ))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                    cx.stop_propagation();
                    if let Some(index) = this.editor.blocks().iter().position(|b| b.id == id) {
                        let folded = this.editor.is_collapsed(index);
                        crate::telemetry::notes_event("notes.fold.toggled", "chevron");
                        this.set_folded(index, !folded, cx);
                    }
                }),
            )
            .into_any_element()
    }

    fn marker(
        &self,
        index: usize,
        block: &Block,
        line: f32,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let colors = self.colors;
        let base = div()
            .w(px(MARKER_WIDTH))
            .h(px(line))
            .flex_none()
            .flex()
            .items_center();
        match block.kind {
            BlockKind::Bullet => {
                let glyph = match block.indent % 3 {
                    0 => "•",
                    1 => "◦",
                    _ => "▪",
                };
                // A folded bullet wears a soft ring, the outliner convention
                // for "there is more in here".
                let folded = self.editor.is_collapsed(index);
                base.child(
                    div()
                        .size(px(18.0))
                        .rounded_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .when(folded, |ring| ring.bg(colors.primary.alpha(0.1)))
                        .text_size(px(18.0))
                        .text_color(colors.secondary)
                        .child(glyph),
                )
                .into_any_element()
            }
            BlockKind::Numbered => base
                .text_size(px(14.0))
                .text_color(colors.secondary)
                .child(format!("{}.", self.editor.ordinal(index)))
                .into_any_element(),
            BlockKind::Todo { checked } => {
                let id = block.id;
                let popping = self.ticked.iter().any(|(b, _)| *b == id);
                let box_el = div()
                    .id(("todo-box", id))
                    .size(px(16.0))
                    .rounded(px(5.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .when(checked, |el| el.bg(accent()))
                    .when(!checked, |el| {
                        el.border(px(1.5))
                            .border_color(colors.primary.alpha(0.32))
                            .hover(|el| el.border_color(accent()).bg(accent().alpha(0.08)))
                    })
                    .when(checked, |el| {
                        el.child(
                            div()
                                .text_size(px(11.0))
                                .font_weight(FontWeight::BOLD)
                                .text_color(gpui::white())
                                .child("✓"),
                        )
                    })
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                            cx.stop_propagation();
                            if let Some(index) =
                                this.editor.blocks().iter().position(|b| b.id == id)
                            {
                                this.check(index, cx);
                            }
                        }),
                    );
                let box_el: gpui::AnyElement = if popping {
                    box_el
                        .with_animation(
                            ("todo-pop", id),
                            Animation::new(Duration::from_millis(320))
                                .with_easing(gpui::ease_out_quint()),
                            |el, t| {
                                // Overshoot then settle: 0.8 → 1.12 → 1.
                                let scale = if t < 0.45 {
                                    0.8 + (t / 0.45) * 0.32
                                } else {
                                    1.12 - ((t - 0.45) / 0.55) * 0.12
                                };
                                el.size(px(16.0 * scale))
                            },
                        )
                        .into_any_element()
                } else {
                    box_el.into_any_element()
                };
                base.child(
                    div()
                        .size(px(18.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(box_el),
                )
                .into_any_element()
            }
            _ => base.into_any_element(),
        }
    }

    // -----------------------------------------------------------------------
    // Menus
    //
    // The `/` and `@` menus are diri menus: `floating` hosts them in a blurred
    // panel window under the glass material and in the window otherwise,
    // their rows share the New Agent menu's shape, and the keyboard never
    // leaves the editor.

    /// Mounts `target`'s menu at the caret for this frame, with a scrim that
    /// turns a click anywhere else into a dismissal.
    pub(super) fn host_menu(
        &mut self,
        target: floating::Target<Self>,
        rows: fn(&mut Self, &mut Context<Self>) -> Option<gpui::Div>,
        width: f32,
        height: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let caret = self.trigger_anchor().or(self.caret_bounds)?;
        let viewport = window.viewport_size();
        let (position, anchor) = menu_placement(caret, viewport.height, height);
        let dismiss = move |this: &mut Self,
                            _: &MouseDownEvent,
                            window: &mut Window,
                            cx: &mut Context<Self>| {
            (target.dismiss)(this, window, cx);
        };
        let scrim = deferred(
            anchored().position(point(px(0.0), px(0.0))).child(
                div()
                    .w(viewport.width)
                    .h(viewport.height)
                    .occlude()
                    .on_mouse_down(MouseButton::Left, cx.listener(dismiss))
                    .on_mouse_down(MouseButton::Right, cx.listener(dismiss)),
            ),
        )
        .with_priority(1);
        let host = div().absolute().inset_0();
        if floating::uses_panels(false, self.colors, cx) {
            let probe = (target.content)(self, cx)?;
            let panel = floating::host_element(
                target,
                probe,
                width,
                position,
                anchor,
                MENU_MARGIN,
                window,
                cx,
            );
            return Some(host.child(panel).child(scrim).into_any_element());
        }
        // In the window the menu is the same floating surface every other
        // in-window diri menu uses, with its shadow and entry motion.
        let content = diri_ui::FloatingSurface::new(
            self.colors,
            div().w(px(width)).overflow_hidden().child(rows(self, cx)?),
        )
        .radius(floating::MENU_RADIUS);
        Some(
            host.child(scrim)
                .child(
                    deferred(
                        anchored()
                            .anchor(anchor)
                            .position(position)
                            .snap_to_window_with_margin(px(MENU_MARGIN))
                            .child(div().occlude().child(content)),
                    )
                    .with_priority(2),
                )
                .into_any_element(),
        )
    }

    fn slash_menu_content(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let rows = self.slash_menu_rows(cx)?;
        Some(
            floating::surface(self.colors, floating::MENU_RADIUS, SLASH_MENU_WIDTH, rows)
                .into_any_element(),
        )
    }

    fn slash_menu_rows(&mut self, cx: &mut Context<Self>) -> Option<gpui::Div> {
        let selected = self.slash.as_ref()?.selected;
        let colors = self.colors;
        let matches = self.slash_matches();
        let mut list = div().flex().flex_col().py(px(floating::MENU_PADDING_Y));
        if matches.is_empty() {
            list = list.child(menu_empty(crate::i18n::t("notes.menu.no_blocks"), colors));
        }
        let mut group = None;
        for (i, item) in matches.iter().enumerate() {
            if group.is_some_and(|g| g != item.group) {
                list = list.child(floating::menu_separator(colors));
            }
            group = Some(item.group);
            let row = floating::menu_row(
                ("note-slash-row", i),
                crate::icons::sf_symbol(item.icon, MENU_ICON, colors.secondary),
                colors,
                i == selected,
            )
            .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                if *hovered {
                    this.hover_menu_row(i, cx);
                }
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                    cx.stop_propagation();
                    this.apply_slash(i, cx);
                }),
            )
            .child(menu_label(block_label(item.label), colors))
            .when_some(item.keys, |row, keys| {
                row.child(floating::menu_shortcut(
                    crate::commands::keystroke_label(keys),
                    colors,
                ))
            });
            list = list.child(row);
        }
        Some(list)
    }

    /// The pointer resting on a row selects it: hover and the arrow keys
    /// move one highlight, so a menu never shows two lit rows.
    pub(super) fn hover_menu_row(&mut self, index: usize, cx: &mut Context<Self>) {
        let selected = if let Some(menu) = &mut self.slash {
            &mut menu.selected
        } else if let Some(menu) = &mut self.mention {
            &mut menu.selected
        } else if let Some(menu) = &mut self.table_menu {
            &mut menu.selected
        } else {
            return;
        };
        if *selected != index {
            *selected = index;
            cx.notify();
        }
    }

    #[cfg(test)]
    pub(super) fn menu_selected(&self) -> Option<usize> {
        self.slash
            .as_ref()
            .map(|m| m.selected)
            .or(self.mention.as_ref().map(|m| m.selected))
    }

    fn slash_menu_height(&self) -> f32 {
        let matches = self.slash_matches();
        let separators = matches
            .windows(2)
            .filter(|pair| pair[0].group != pair[1].group)
            .count();
        menu_height(matches.len().max(1), separators)
    }

    fn mention_menu_content(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let rows = self.mention_menu_rows(cx)?;
        Some(
            floating::surface(self.colors, floating::MENU_RADIUS, MENTION_MENU_WIDTH, rows)
                .into_any_element(),
        )
    }

    fn mention_menu_rows(&mut self, cx: &mut Context<Self>) -> Option<gpui::Div> {
        let selected = self.mention.as_ref()?.selected;
        let colors = self.colors;
        let matches = self.mention_matches();
        let mut list = div().flex().flex_col().py(px(floating::MENU_PADDING_Y));
        if matches.is_empty() {
            let notes_only = self.mention.as_ref().is_some_and(|m| m.notes_only);
            list = list.child(menu_empty(
                match (notes_only, self.mentions.entries.is_empty()) {
                    (true, _) => crate::i18n::t("notes.menu.no_notes"),
                    (false, true) => crate::i18n::t("notes.menu.nothing_to_mention"),
                    (false, false) => crate::i18n::t("notes.menu.no_mentions"),
                },
                colors,
            ));
        }
        for (i, entry) in matches.iter().enumerate() {
            // Sessions lead; notes follow below a separator.
            if i > 0 && entry.agent.is_none() && matches[i - 1].agent.is_some() {
                list = list.child(floating::menu_separator(colors));
            }
            // A session's mark wears its status the way its sidebar row does.
            let icon = match (entry.agent, entry.status) {
                // Agents wear their status the way their sidebar rows do,
                // drawn at the New Agent menu's mark size.
                (Some(agent), Some(state)) if agent != UiAgentKind::Shell => {
                    StatusGlyph::new(agent, state, MENU_STATUS_MARK, colors).rendered_mark()
                }
                (Some(agent), _) => AgentLogo::new(agent, MENU_LOGO, colors)
                    .badged(false)
                    .inset(MENU_LOGO_INSET)
                    .monochrome(true)
                    .into_any_element(),
                (None, _) => crate::icons::sf_symbol("doc.text", MENU_ICON, colors.secondary),
            };
            let title = entry.candidate.label.trim_start_matches('@').to_owned();
            let row = floating::menu_row(("note-mention-row", i), icon, colors, i == selected)
                .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                    if *hovered {
                        this.hover_menu_row(i, cx);
                    }
                }))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                        cx.stop_propagation();
                        this.apply_mention(i, cx);
                    }),
                )
                .child(menu_label(title, colors))
                .when(!entry.detail.is_empty(), |row| {
                    row.child(
                        floating::menu_shortcut(entry.detail.clone(), colors)
                            .max_w(px(120.0))
                            .whitespace_nowrap()
                            .overflow_hidden()
                            .text_ellipsis(),
                    )
                });
            list = list.child(row);
        }
        Some(list)
    }

    fn mention_menu_height(&self) -> f32 {
        let matches = self.mention_matches();
        let separators = matches
            .windows(2)
            .filter(|pair| pair[0].agent.is_some() && pair[1].agent.is_none())
            .count();
        menu_height(matches.len().max(1), separators)
    }
}

impl NoteEditorView {
    fn link_menu_content(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let rows = self.link_menu_rows(cx)?;
        Some(
            floating::surface(self.colors, floating::MENU_RADIUS, LINK_MENU_WIDTH, rows)
                .into_any_element(),
        )
    }

    fn link_menu_rows(&mut self, cx: &mut Context<Self>) -> Option<gpui::Div> {
        let editor = self.link_editor.as_ref()?;
        let colors = self.colors;
        let field = div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(7.0))
            .h(px(LINK_FIELD_HEIGHT))
            .mx(px(floating::MENU_ROW_MARGIN))
            .px(px(floating::MENU_ROW_INSET))
            .rounded(px(floating::MENU_ROW_RADIUS))
            .bg(colors.primary.alpha(0.06))
            .text_size(px(Typo::ROW.size))
            .text_color(colors.primary)
            .child(crate::icons::sf_symbol("link", MENU_ICON, colors.tertiary))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .child(if editor.query.is_empty() {
                        div()
                            .text_color(colors.tertiary)
                            .child(format!(
                                "{}{}",
                                crate::navigation::CARET,
                                crate::i18n::t("notes.link.placeholder")
                            ))
                            .into_any_element()
                    } else {
                        crate::navigation::query_label(&field_view(&editor.query))
                    }),
            );
        let selected = editor.selected;
        let mut list = div()
            .flex()
            .flex_col()
            .py(px(floating::MENU_PADDING_Y))
            .child(field);
        let rows = self.link_rows();
        if !rows.is_empty() {
            list = list.child(floating::menu_separator(colors));
        }
        for (i, row) in rows.iter().enumerate() {
            let (icon, label, keys): (AnyElement, SharedString, &str) = match row {
                LinkRow::Apply { url } => {
                    let found = diri_notes::links::recognize(url);
                    let icon = found.as_ref().map_or_else(
                        || crate::icons::sf_symbol("link", MENU_ICON, colors.secondary),
                        |found| {
                            gpui::svg()
                                .path(service_icon(found.service))
                                .size(px(CHIP_ICON))
                                .text_color(colors.secondary)
                                .into_any_element()
                        },
                    );
                    let label = match (&found, self.editor.selection.is_collapsed()) {
                        (Some(found), true) => {
                            crate::i18n::tf("notes.link.insert", &[("target", &found.title)])
                        }
                        (Some(found), false) => {
                            crate::i18n::tf("notes.link.link_to", &[("target", &found.name)])
                        }
                        (None, _) => {
                            crate::i18n::tf("notes.link.link_to", &[("target", &short_url(url))])
                        }
                    };
                    (icon, label.into(), "↩")
                }
                LinkRow::Open { url } => (
                    crate::icons::sf_symbol("square.and.arrow.up", MENU_ICON, colors.secondary),
                    crate::i18n::tf("notes.link.open", &[("target", &short_url(url))]).into(),
                    "⌘↩",
                ),
                LinkRow::Remove => (
                    crate::icons::sf_symbol("xmark", MENU_ICON, colors.secondary),
                    crate::i18n::t("notes.link.remove").into(),
                    "",
                ),
            };
            let item = floating::menu_row(("note-link-row", i), icon, colors, i == selected)
                .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                    if *hovered
                        && let Some(editor) = &mut this.link_editor
                        && editor.selected != i
                    {
                        editor.selected = i;
                        cx.notify();
                    }
                }))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                        cx.stop_propagation();
                        this.apply_link_row(i, cx);
                    }),
                )
                .child(menu_label(label, colors))
                .when(!keys.is_empty(), |item| {
                    item.child(floating::menu_shortcut(keys, colors))
                });
            list = list.child(item);
        }
        Some(list)
    }

    fn link_menu_height(&self) -> f32 {
        let rows = self.link_rows().len();
        let separator = if rows > 0 { MENU_SEPARATOR_HEIGHT } else { 0.0 };
        menu_height(rows, 0) + LINK_FIELD_HEIGHT + separator
    }
}

const LINK_MENU: floating::Target<NoteEditorView> = floating::Target {
    key: "note-link-editor",
    radius: floating::MENU_RADIUS,
    content: NoteEditorView::link_menu_content,
    dismiss: |this, _, cx| this.close_link_editor(cx),
};
const LINK_MENU_WIDTH: f32 = 340.0;
const LINK_FIELD_HEIGHT: f32 = 32.0;

/// The link field as drawn: a long URL with the caret at its end shows its
/// tail, the way a native field scrolls to keep the caret in view.
fn field_view(query: &crate::query_editor::QueryEditor) -> crate::query_editor::QueryEditor {
    const VISIBLE: usize = 38;
    let text = query.text();
    let count = text.chars().count();
    if count <= VISIBLE || query.selection().is_some() || query.cursor() != text.len() {
        return query.clone();
    }
    let tail: String = text.chars().skip(count - (VISIBLE - 1)).collect();
    let mut shown = crate::query_editor::QueryEditor::default();
    shown.insert(&format!("…{tail}"));
    shown
}

/// What ⌘K's field holds as a link: a URL, a `diri://` mention, or a bare
/// domain (`notion.so/…`), which gets `https://`.
fn normalize_url(text: &str) -> Option<String> {
    let text = text.trim();
    if text.is_empty() || text.contains(char::is_whitespace) {
        return None;
    }
    if is_url(text) || MentionTarget::parse(text).is_some() {
        return Some(text.to_owned());
    }
    let host = text.split('/').next().unwrap_or_default();
    (host.contains('.') && !host.starts_with('.') && !host.ends_with('.'))
        .then(|| format!("https://{text}"))
}

/// `https://www.notion.so/acme/Q4…` → `notion.so/acme/Q4…`, cut to fit a row.
fn short_url(url: &str) -> String {
    let bare = url
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_start_matches("www.");
    if bare.chars().count() > 34 {
        format!("{}…", bare.chars().take(33).collect::<String>())
    } else {
        bare.to_owned()
    }
}

/// A link's kind for counts-only telemetry: the tool family, never the URL.
fn link_kind(url: &str) -> &'static str {
    use diri_notes::links::Service;
    match diri_notes::links::recognize(url).map(|r| r.service) {
        Some(Service::Notion) => "notion",
        Some(
            Service::GoogleDocs
            | Service::GoogleSheets
            | Service::GoogleSlides
            | Service::GoogleDrive,
        ) => "google",
        Some(Service::Linear) => "linear",
        Some(Service::HubSpot) => "hubspot",
        Some(Service::Figma) => "figma",
        Some(Service::Slack) => "slack",
        Some(Service::GitHub) => "github",
        Some(Service::Dashboard) => "dashboard",
        None if MentionTarget::parse(url).is_some() => "mention",
        None => "web",
    }
}

const SLASH_MENU: floating::Target<NoteEditorView> = floating::Target {
    key: "note-slash-menu",
    radius: floating::MENU_RADIUS,
    content: NoteEditorView::slash_menu_content,
    dismiss: |this, _, cx| {
        this.slash = None;
        cx.notify();
    },
};

const MENTION_MENU: floating::Target<NoteEditorView> = floating::Target {
    key: "note-mention-menu",
    radius: floating::MENU_RADIUS,
    content: NoteEditorView::mention_menu_content,
    dismiss: |this, _, cx| {
        this.mention = None;
        cx.notify();
    },
};

const SLASH_MENU_WIDTH: f32 = 240.0;
const MENTION_MENU_WIDTH: f32 = 340.0;
/// Glyphs and agent marks at the New Agent menu's sizes.
pub(super) const MENU_ICON: f32 = 13.0;
pub(super) const MENU_LOGO: f32 = crate::floating::MENU_AGENT_LOGO;
use crate::floating::MENU_AGENT_LOGO_INSET as MENU_LOGO_INSET;
/// A status mark is inset 0.08; this size draws the mark exactly as large
/// as the New Agent menu's logos.
const MENU_STATUS_MARK: f32 = MENU_LOGO * (1.0 - 2.0 * MENU_LOGO_INSET) / (1.0 - 2.0 * 0.08);
const MENU_GAP: f32 = 6.0;
const MENU_MARGIN: f32 = 8.0;
/// A separator's hairline plus its padding.
const MENU_SEPARATOR_HEIGHT: f32 = 9.0;

pub(super) fn menu_height(rows: usize, separators: usize) -> f32 {
    2.0 * floating::MENU_PADDING_Y
        + rows as f32 * floating::MENU_ROW_HEIGHT
        + separators as f32 * MENU_SEPARATOR_HEIGHT
        + 2.0
}

pub(super) fn menu_label(label: impl Into<SharedString>, colors: SemanticColors) -> gpui::Div {
    div()
        .min_w_0()
        .flex_1()
        .whitespace_nowrap()
        .overflow_hidden()
        .text_ellipsis()
        .text_size(px(Typo::ROW.size))
        .text_color(colors.primary)
        .child(label.into())
}

pub(super) fn menu_empty(text: &'static str, colors: SemanticColors) -> gpui::Div {
    div()
        .h(px(floating::MENU_ROW_HEIGHT))
        .px(px(floating::MENU_ROW_MARGIN + floating::MENU_ROW_INSET))
        .flex()
        .items_center()
        .text_size(px(Typo::ROW.size))
        .text_color(colors.tertiary)
        .child(text)
}

/// A caret menu opens below the line, or above it when the window has no
/// room below, never on top of the text being typed.
fn menu_placement(
    caret: Bounds<Pixels>,
    viewport: Pixels,
    height: f32,
) -> (Point<Pixels>, gpui::Anchor) {
    let left = caret.left() - px(floating::MENU_ROW_MARGIN + floating::MENU_ROW_INSET);
    let room = px(MENU_GAP + height + MENU_MARGIN);
    let below = caret.bottom() + room <= viewport || caret.top() - room < px(0.0);
    if below {
        (
            point(left, caret.bottom() + px(MENU_GAP)),
            gpui::Anchor::TopLeft,
        )
    } else {
        (
            point(left, caret.top() - px(MENU_GAP)),
            gpui::Anchor::BottomLeft,
        )
    }
}

/// A block's text as laid out: the stored text plus room inside each chip.
/// Every chip gets a thin space at each end, so its pill has padding without
/// reaching into the space beside it, and a tool chip gets room for its
/// glyph. Offsets in the note (model) and in the layout (display) differ only
/// by those inserts, and every layout lookup goes through
/// [`Shown::to_display`] / [`Shown::to_model`].
#[derive(Clone, Debug, Default)]
struct Shown {
    text: String,
    /// Ascending by model offset, a trailing insert before a leading one.
    inserts: Vec<Insert>,
    /// Where the room reserved for an inline accessory starts in the layout,
    /// past its breakable space: the to-do's Start sits there.
    tail: Option<usize>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Insert {
    at: usize,
    text: &'static str,
    /// Belongs to the chip ending at `at` (a caret there sits after it)
    /// rather than the chip starting there (a caret there sits before it).
    trailing: bool,
}

// Chip inserts are word characters drawn fully faded, not spaces: gpui's
// line wrapper may break before any non-word character, which split a
// chip's padding or glyph room from its title across lines.
/// A chip's inner padding at either end.
const CHIP_EDGE: &str = ".";
/// Room for a tool chip's glyph: two digits, about a 13 pt icon at body size.
const CHIP_ICON_ROOM: &str = "00";
/// What leads a tool chip: its edge, then its glyph room.
const CHIP_LEAD: &str = ".00";
/// Room after a to-do's last word for its Start button, so the button
/// follows the words and wraps with them instead of hanging off the column.
/// The space lets the wrapper move the room to its own line; the digits keep
/// it whole.
const START_ROOM: &str = " 000000000000";

impl Shown {
    fn of(block: &Block) -> Self {
        Self::with_tail(block, None)
    }

    /// The block's layout text, with `room` reserved after its last word.
    fn with_tail(block: &Block, room: Option<&'static str>) -> Self {
        let mut inserts = Vec::new();
        for chip in mention::in_block(block) {
            inserts.push(Insert {
                at: chip.range.start,
                text: CHIP_EDGE,
                trailing: false,
            });
            inserts.push(Insert {
                at: chip.range.end,
                text: CHIP_EDGE,
                trailing: true,
            });
        }
        for (range, _) in link_chips(block) {
            inserts.push(Insert {
                at: range.start,
                text: CHIP_LEAD,
                trailing: false,
            });
            inserts.push(Insert {
                at: range.end,
                text: CHIP_EDGE,
                trailing: true,
            });
        }
        if inserts.is_empty() && room.is_none() {
            return Self {
                text: block.text.clone(),
                inserts,
                tail: None,
            };
        }
        inserts.sort_by_key(|i| (i.at, !i.trailing));
        inserts.dedup();
        // Leading, so a caret at the end of the words stays before it.
        if let Some(room) = room {
            inserts.push(Insert {
                at: block.text.len(),
                text: room,
                trailing: false,
            });
        }
        let extra: usize = inserts.iter().map(|i| i.text.len()).sum();
        let mut text = String::with_capacity(block.text.len() + extra);
        let mut last = 0;
        for insert in &inserts {
            text.push_str(&block.text[last..insert.at]);
            text.push_str(insert.text);
            last = insert.at;
        }
        text.push_str(&block.text[last..]);
        let tail = room.map(|room| text.len() - room.len() + 1);
        Self {
            text,
            inserts,
            tail,
        }
    }

    fn to_display(&self, model: usize) -> usize {
        model
            + self
                .inserts
                .iter()
                .filter(|i| i.at < model || (i.trailing && i.at == model))
                .map(|i| i.text.len())
                .sum::<usize>()
    }

    fn to_model(&self, display: usize) -> usize {
        let mut shift = 0;
        for insert in &self.inserts {
            let start = insert.at + shift;
            if display < start {
                break;
            }
            if display < start + insert.text.len() {
                return insert.at;
            }
            shift += insert.text.len();
        }
        display - shift
    }

    /// A chip's model range in the layout, including its inserts.
    fn range(&self, range: &Range<usize>) -> Range<usize> {
        self.to_display(range.start)..self.to_display(range.end)
    }

    /// Laid-out ranges drawn faded, with how much: every insert fully, and
    /// a mention's `@` (hidden under a session's dot, softened for a note).
    fn fades(&self, block: &Block) -> Vec<(Range<usize>, f32)> {
        let mut shift = 0;
        let mut fades = Vec::with_capacity(self.inserts.len());
        for insert in &self.inserts {
            let start = insert.at + shift;
            fades.push((start..start + insert.text.len(), 1.0));
            shift += insert.text.len();
        }
        for chip in mention::in_block(block) {
            let head = self.head_of(chip.range.start);
            let fade = match chip.target {
                MentionTarget::Session(_) => 1.0,
                MentionTarget::Note(_) => 0.55,
            };
            fades.push((head..head + 1, fade));
        }
        fades
    }

    /// Where a chip's first stored character (a mention's `@`) is laid out.
    fn head_of(&self, chip_start: usize) -> usize {
        self.to_display(chip_start + 1) - 1
    }
}

/// Links that open a known tool, which render as chips with its glyph.
fn link_chips(
    block: &Block,
) -> impl Iterator<Item = (Range<usize>, diri_notes::links::Recognized)> + '_ {
    block.marks.iter().filter_map(|mark| match &mark.style {
        Style::Link(url) => diri_notes::links::recognize(url).map(|r| (mark.range.clone(), r)),
        _ => None,
    })
}

fn service_icon(service: diri_notes::links::Service) -> &'static str {
    use diri_notes::links::Service;
    match service {
        Service::Notion => "icons/notion.svg",
        Service::GoogleDocs => "icons/google-doc.svg",
        Service::GoogleSheets => "icons/google-sheet.svg",
        Service::GoogleSlides => "icons/google-slides.svg",
        Service::GoogleDrive => "icons/google-drive.svg",
        Service::Linear => "icons/linear.svg",
        Service::HubSpot => "icons/hubspot.svg",
        Service::Figma => "icons/figma.svg",
        Service::Slack => "icons/slack.svg",
        Service::GitHub => "icons/github.svg",
        Service::Dashboard => "icons/chart-bar.svg",
    }
}

/// Where one character sits, or `None` for a space that wrapped away.
/// A wrap boundary index resolves to the end of the earlier line, so a
/// character that opens a line is recovered from its right edge.
fn char_rect(layout: &TextLayout, text: &str, at: usize) -> Option<Bounds<Pixels>> {
    let next = text[at..].chars().next().map_or(at, |c| at + c.len_utf8());
    let from = layout.position_for_index(at)?;
    let to = layout.position_for_index(next)?;
    let line = layout.line_height();
    if to.y == from.y {
        return Some(Bounds::from_corners(from, point(to.x, from.y + line)));
    }
    let left = layout.bounds().left();
    (to.x > left + px(0.5))
        .then(|| Bounds::from_corners(point(left, to.y), point(to.x, to.y + line)))
}

/// One rectangle per visual line a chip covers, hugging its glyphs (unlike
/// selection rects, which run to the margin).
fn chip_rects(layout: &TextLayout, text: &str, range: Range<usize>) -> Vec<Bounds<Pixels>> {
    let mut rects: Vec<Bounds<Pixels>> = Vec::new();
    for (i, _) in text[range.clone()].char_indices() {
        let Some(rect) = char_rect(layout, text, range.start + i) else {
            continue;
        };
        match rects.last_mut() {
            Some(last) if last.top() == rect.top() => *last = last.union(&rect),
            _ => rects.push(rect),
        }
    }
    rects
}

fn selection_rects(
    layout: &TextLayout,
    from: usize,
    to: usize,
    empty: bool,
    includes_break: bool,
) -> Vec<Bounds<Pixels>> {
    let line = layout.line_height();
    let bounds = layout.bounds();
    if empty {
        return vec![Bounds::new(bounds.origin, size(px(7.0), line))];
    }
    let (Some(a), Some(b)) = (
        layout.position_for_index(from),
        layout.position_for_index(to),
    ) else {
        return Vec::new();
    };
    let tail = if includes_break { px(7.0) } else { px(0.0) };
    if a.y == b.y {
        return vec![Bounds::from_corners(a, point(b.x + tail, b.y + line))];
    }
    let mut rects = vec![Bounds::from_corners(a, point(bounds.right(), a.y + line))];
    if b.y > a.y + line {
        rects.push(Bounds::from_corners(
            point(bounds.left(), a.y + line),
            point(bounds.right(), b.y),
        ));
    }
    rects.push(Bounds::from_corners(
        point(bounds.left(), b.y),
        point(b.x + tail, b.y + line),
    ));
    rects
}

fn scroll_into_view(scroll: &ScrollHandle, caret: Bounds<Pixels>, window: &mut Window) {
    let viewport = scroll.bounds();
    if viewport.size.height <= px(0.0) {
        return;
    }
    let margin = px(48.0);
    let offset = scroll.offset();
    let mut y = offset.y;
    if caret.top() - margin < viewport.top() {
        y += viewport.top() - (caret.top() - margin);
    } else if caret.bottom() + margin > viewport.bottom() {
        y -= caret.bottom() + margin - viewport.bottom();
    }
    if y != offset.y {
        let max = scroll.max_offset().y;
        scroll.set_offset(point(offset.x, y.clamp(-max, px(0.0))));
        window.refresh();
    }
}

#[allow(dead_code)]
pub(crate) fn editor_entity_focus(
    editor: &Entity<NoteEditorView>,
    window: &mut Window,
    cx: &mut App,
) {
    let focus = editor.read(cx).focus.clone();
    window.focus(&focus, cx);
}

#[cfg(test)]
mod shown_tests {
    use super::*;

    #[test]
    fn chips_get_edges_and_glyph_room_and_offsets_map_both_ways() {
        let mut block = Block::new(1, BlockKind::Paragraph, "see ENG-7 and @Plan!");
        block.add_mark(4..9, Style::Link("https://linear.app/a/issue/ENG-7".into()));
        block.add_mark(14..19, Style::Link("diri://note/n-1".into()));
        block.add_mark(0..3, Style::Link("https://diri.sh".into()));
        let shown = Shown::of(&block);
        assert_eq!(
            shown.text,
            format!("see {CHIP_LEAD}ENG-7{CHIP_EDGE} and {CHIP_EDGE}@Plan{CHIP_EDGE}!"),
            "plain links get nothing"
        );
        // A caret before a chip sits outside it; right after, outside too.
        assert_eq!(shown.to_display(4), 4);
        assert_eq!(
            &shown.text[shown.to_display(9)..shown.to_display(9) + 1],
            " "
        );
        for model in 0..=block.text.len() {
            assert_eq!(shown.to_model(shown.to_display(model)), model);
        }
        // A click on the glyph lands before the chip.
        assert_eq!(shown.to_model(5), 4);
        let chip = shown.range(&(14..19));
        assert!(shown.text[chip.clone()].starts_with(CHIP_EDGE));
        assert!(shown.text[chip].ends_with(CHIP_EDGE));
        assert_eq!(&shown.text[shown.head_of(14)..shown.head_of(14) + 1], "@");
    }
}
