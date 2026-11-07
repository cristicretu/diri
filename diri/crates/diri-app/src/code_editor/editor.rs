// Adapted from Ely GPUI Components (MIT OR Apache-2.0), https://github.com/ZacharyZhang-NY/Ely-GPUI-Components

//! The editor's state and every operation on it. Rendering lives in
//! `render`, key and IME wiring in `input`.

use std::collections::{BTreeSet, HashMap};
use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use gpui::{
    App, ClipboardItem, Context, EventEmitter, FocusHandle, Focusable, Pixels, ScrollStrategy,
    SharedString, Task, UniformListScrollHandle, px,
};

use crate::code_intelligence::{
    CodeIntelligence, CodeIntelligenceError, SourceLanguage, SourceSnapshot,
};
use crate::query_editor::QueryEditor;

use super::buffer::Buffer;
use super::cursor::{Motion, Selection, carets_after, joined, merged, moved, shifted};
use super::find::{self, FindOptions};
use super::history::History;
use super::intel::{self, Candidate, CandidateKind, Lens};
use super::palette::EditorPalette;
use super::syntax::{self, Analysis, Fold};

/// Pairs typing an opener closes at once.
const PAIRS: [(char, char); 5] = [('(', ')'), ('[', ']'), ('{', '}'), ('"', '"'), ('\'', '\'')];
const BLINK: Duration = Duration::from_millis(530);
/// A caret stops blinking (and stops waking the app) after this long idle.
const BLINK_IDLE: Duration = Duration::from_secs(12);
pub(crate) const HOVER_DELAY: Duration = Duration::from_millis(450);
/// Completion items shown at once.
const COMPLETIONS: usize = 12;
/// Rows a page key moves.
pub(crate) const PAGE: isize = 30;

/// What the editor tells the Files surface.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EditorEvent {
    /// Unsaved edits appeared or went away.
    DirtyChanged,
    /// Open another file at a one-based line (go to definition, hover links).
    OpenLocation { path: PathBuf, line: usize },
    /// Search the workspace for this text, whole word.
    SearchWorkspace(String),
    /// The file on disk should be loaded again, dropping edits.
    Reload,
    /// The file was written.
    Saved,
    /// A save finished after its document was switched away; the Files
    /// surface applies it to that document wherever it now lives (a draft).
    SavedAway(SavedAway),
}

/// A finished save whose document is no longer the one shown.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SavedAway {
    pub document: DocumentId,
    pub absolute_path: PathBuf,
    /// The buffer version that was written; edits made after it stay dirty.
    pub version: u64,
    pub modified: Option<SystemTime>,
}

/// One document's identity for its lifetime. Buffer versions are per buffer
/// and collide across documents, so async results key on this.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DocumentId(u64);

impl DocumentId {
    pub(crate) fn fresh() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        Self(NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
    }
}

/// One row the editor draws, all the same height.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Row {
    Line(usize),
    /// A code lens above a line, by its place among the lenses.
    Lens(usize),
}

/// A file's editing state; it outlives the view while another file is shown.
pub struct Document {
    pub(crate) id: DocumentId,
    pub relative_path: PathBuf,
    pub absolute_path: PathBuf,
    pub language: SourceLanguage,
    pub(crate) buffer: Buffer,
    pub(crate) history: History,
    pub(crate) selections: Vec<Selection>,
    pub(crate) folded: BTreeSet<usize>,
    pub modified: Option<SystemTime>,
    pub read_only: bool,
    pub(crate) scroll_row: usize,
}

impl Document {
    pub fn from_snapshot(snapshot: &SourceSnapshot) -> Self {
        let buffer = Buffer::new(snapshot.text.clone());
        let caret = snapshot.target.map_or(0, |target| {
            buffer.offset(
                target.line.saturating_sub(1),
                target.column.saturating_sub(1),
            )
        });
        Self {
            id: DocumentId::fresh(),
            relative_path: snapshot.relative_path.clone(),
            absolute_path: snapshot.absolute_path.clone(),
            language: snapshot.language,
            buffer,
            history: History::default(),
            selections: vec![Selection::caret(caret)],
            folded: BTreeSet::new(),
            modified: snapshot.modified,
            read_only: snapshot.read_only,
            scroll_row: 0,
        }
    }

    pub fn is_dirty(&self) -> bool {
        !self.history.is_clean()
    }

    pub fn text(&self) -> &str {
        self.buffer.text()
    }

    /// Applies a save that finished while this document was not shown.
    /// Returns false when `saved` was for another document.
    pub(crate) fn apply_saved_away(&mut self, saved: &SavedAway) -> bool {
        if saved.document != self.id {
            return false;
        }
        self.modified = saved.modified;
        if self.buffer.version() == saved.version {
            self.history.mark_saved();
        }
        true
    }
}

/// The find widget's state.
#[derive(Default)]
pub(crate) struct FindState {
    pub open: bool,
    pub replace_open: bool,
    pub query: QueryEditor,
    pub replacement: QueryEditor,
    /// Which field types go to: false for the query, true for the replacement.
    pub in_replacement: bool,
    pub options: FindOptions,
    pub matches: Vec<Range<usize>>,
    pub current: Option<usize>,
    pub error: Option<String>,
    /// The buffer version the matches were found in.
    pub version: Option<u64>,
}

/// The completion menu over the caret.
pub(crate) struct Completion {
    pub word: Range<usize>,
    pub items: Vec<Candidate>,
    pub selected: usize,
}

/// A hover card's subject and what was found about it.
pub(crate) struct Hover {
    pub word: Range<usize>,
    pub name: String,
    pub definitions: Vec<crate::code_intelligence::SearchHit>,
}

/// Signature help for the call around the caret.
pub(crate) struct Signature {
    pub name: String,
    pub argument: usize,
    pub declaration: String,
    pub location: (PathBuf, usize),
}

/// Where the code sat when last painted, in window pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Metrics {
    /// The left edge of column zero (already shifted by horizontal scroll).
    pub left: Pixels,
    /// The top of the list's viewport.
    pub top: Pixels,
    pub advance: Pixels,
    pub line: Pixels,
    /// The width the code may use, for horizontal reveal.
    pub width: Pixels,
}

/// Derived data, rebuilt when the text or the folds change.
#[derive(Default)]
pub(crate) struct Layout {
    pub version: Option<(u64, u64)>,
    pub rows: Vec<Row>,
    /// Each line's row, or `usize::MAX` while a fold hides it.
    pub line_rows: Vec<usize>,
    pub lenses: Vec<Lens>,
}

pub struct CodeEditor {
    pub(crate) focus: FocusHandle,
    pub(crate) find_focus: FocusHandle,
    pub(crate) doc: Option<Document>,
    pub(crate) analysis: Analysis,
    pub(crate) layout: Layout,
    fold_generation: u64,
    pub(crate) scroll: UniformListScrollHandle,
    pub(crate) h_offset: Pixels,
    pub(crate) metrics: Metrics,
    pub(crate) palette: EditorPalette,
    pub(crate) colors: diri_ui::SemanticColors,
    pub(crate) font_family: SharedString,
    pub(crate) font_size: Pixels,
    pub(crate) tab: usize,
    pub(crate) minimap: bool,
    pub(crate) lenses_enabled: bool,
    pub(crate) marked: Option<Range<usize>>,
    /// The selections before an input method's composition began.
    composing: Option<Vec<Selection>>,
    pub(crate) dragging: bool,
    pub(crate) caret_on: bool,
    pub(crate) focused: bool,
    blink_epoch: u64,
    last_activity: Instant,
    _blink: Option<Task<()>>,
    pub(crate) find: FindState,
    pub(crate) completion: Option<Completion>,
    pub(crate) hover: Option<Hover>,
    pub(crate) hover_pending: Option<Range<usize>>,
    _hover_task: Option<Task<()>>,
    pub(crate) signature: Option<Signature>,
    _signature_task: Option<Task<()>>,
    _completion_task: Option<Task<()>>,
    word_counts: Option<(u64, Arc<HashMap<String, usize>>)>,
    pub(crate) intelligence: Option<Arc<CodeIntelligence>>,
    pub(crate) save_error: Option<String>,
    pub(crate) conflict: bool,
    _save_task: Option<Task<()>>,
    was_dirty: bool,
    /// The minimap's box, from its last paint.
    pub(crate) minimap_bounds: gpui::Bounds<Pixels>,
}

impl EventEmitter<EditorEvent> for CodeEditor {}

impl Focusable for CodeEditor {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl CodeEditor {
    pub fn new(
        palette: EditorPalette,
        colors: diri_ui::SemanticColors,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            focus: cx.focus_handle().tab_stop(true),
            find_focus: cx.focus_handle(),
            doc: None,
            analysis: Analysis::default(),
            layout: Layout::default(),
            fold_generation: 0,
            scroll: UniformListScrollHandle::new(),
            h_offset: px(0.0),
            metrics: Metrics::default(),
            palette,
            colors,
            font_family: crate::fonts::mono_family().into(),
            font_size: px(12.0),
            tab: 4,
            minimap: true,
            lenses_enabled: true,
            marked: None,
            composing: None,
            dragging: false,
            caret_on: true,
            focused: false,
            blink_epoch: 0,
            last_activity: Instant::now(),
            _blink: None,
            find: FindState::default(),
            completion: None,
            hover: None,
            hover_pending: None,
            _hover_task: None,
            signature: None,
            _signature_task: None,
            _completion_task: None,
            word_counts: None,
            intelligence: None,
            save_error: None,
            conflict: false,
            _save_task: None,
            was_dirty: false,
            minimap_bounds: gpui::Bounds::default(),
        }
    }

    pub fn set_style(
        &mut self,
        palette: EditorPalette,
        colors: diri_ui::SemanticColors,
        font_family: SharedString,
        cx: &mut Context<Self>,
    ) {
        if self.palette == palette && self.colors == colors && self.font_family == font_family {
            return;
        }
        self.palette = palette;
        self.colors = colors;
        self.font_family = font_family;
        cx.notify();
    }

    pub fn set_intelligence(&mut self, intelligence: Option<Arc<CodeIntelligence>>) {
        self.intelligence = intelligence;
    }

    /// Shows a document, handing back the one shown before.
    pub fn set_document(
        &mut self,
        doc: Option<Document>,
        cx: &mut Context<Self>,
    ) -> Option<Document> {
        let mut previous = self.doc.take();
        if let Some(previous) = previous.as_mut() {
            previous.scroll_row = self.top_row();
        }
        self.tab = doc.as_ref().map_or(4, |doc| detect_tab(&doc.buffer));
        let scroll_row = doc.as_ref().map_or(0, |doc| doc.scroll_row);
        self.doc = doc;
        self.analysis = Analysis::default();
        self.layout = Layout::default();
        self.scroll = UniformListScrollHandle::new();
        self.h_offset = px(0.0);
        self.marked = None;
        self.composing = None;
        self.close_completion();
        self.hover = None;
        self.hover_pending = None;
        self.close_signature();
        self.save_error = None;
        self.conflict = false;
        self.word_counts = None;
        self.find.version = None;
        self.was_dirty = self.is_dirty();
        if scroll_row > 0 {
            self.scroll.scroll_to_item(scroll_row, ScrollStrategy::Top);
        } else if self.doc.is_some() {
            self.reveal_primary(ScrollStrategy::Center);
        }
        self.restart_blink(cx);
        previous
    }

    /// The first row in view.
    pub(crate) fn top_row(&self) -> usize {
        let line = self.metrics.line;
        if line <= px(0.0) {
            return 0;
        }
        let scrolled = -self.scroll.0.borrow().base_handle.offset().y;
        (scrolled / line).floor().max(0.0) as usize
    }

    pub fn document(&self) -> Option<&Document> {
        self.doc.as_ref()
    }

    pub fn is_dirty(&self) -> bool {
        self.doc.as_ref().is_some_and(Document::is_dirty)
    }

    pub fn relative_path(&self) -> Option<&std::path::Path> {
        self.doc.as_ref().map(|doc| doc.relative_path.as_path())
    }

    pub fn text(&self) -> &str {
        self.doc.as_ref().map_or("", |doc| doc.buffer.text())
    }

    pub fn selections(&self) -> &[Selection] {
        self.doc
            .as_ref()
            .map_or(&[], |doc| doc.selections.as_slice())
    }

    pub(crate) fn editable(&self) -> bool {
        self.doc.as_ref().is_some_and(|doc| !doc.read_only)
    }

    /// The last cursor placed, which scrolling follows.
    pub fn primary(&self) -> Selection {
        self.doc
            .as_ref()
            .and_then(|doc| doc.selections.last().copied())
            .unwrap_or(Selection::caret(0))
    }

    /// Line and column of the primary cursor, both from one.
    pub fn position(&self) -> (usize, usize) {
        let Some(doc) = &self.doc else {
            return (1, 1);
        };
        let (line, column) = doc.buffer.point(self.primary().head);
        (line + 1, column + 1)
    }

    /// Places one caret at a one-based line and column and centres it.
    pub fn go_to(&mut self, line: usize, column: usize, cx: &mut Context<Self>) {
        let Some(doc) = &self.doc else {
            return;
        };
        let at = doc
            .buffer
            .offset(line.saturating_sub(1), column.saturating_sub(1));
        self.set_selections(vec![Selection::caret(at)], cx);
        self.reveal_primary(ScrollStrategy::Center);
    }

    // ---- derived data ----------------------------------------------------

    /// Brings tokens, brackets, folds, and rows up to date with the text.
    pub(crate) fn refresh_layout(&mut self) {
        let Some(doc) = &self.doc else {
            self.analysis = Analysis::default();
            self.layout = Layout::default();
            return;
        };
        let version = doc.buffer.version();
        let analysed = self.analysis.version == version && !self.analysis.tokens.is_empty();
        if !analysed {
            self.analysis = syntax::analyze(&doc.buffer, doc.language, self.tab);
        }
        let key = (version, self.fold_generation);
        if self.layout.version == Some(key) {
            return;
        }
        let doc = self.doc.as_mut().expect("checked above");
        // Folds whose header no longer opens a block are dropped.
        let headers: std::collections::HashSet<usize> =
            self.analysis.folds.iter().map(|fold| fold.header).collect();
        doc.folded.retain(|header| headers.contains(header));
        let lines = doc.buffer.lines();
        let mut hidden = vec![false; lines];
        for fold in self
            .analysis
            .folds
            .iter()
            .filter(|fold| doc.folded.contains(&fold.header))
        {
            for slot in &mut hidden[fold.header + 1..=fold.end.min(lines - 1)] {
                *slot = true;
            }
        }
        let lenses = if self.lenses_enabled {
            let counts = Self::counts_for(&mut self.word_counts, &doc.buffer);
            intel::lenses(&doc.buffer, doc.language, &counts)
        } else {
            Vec::new()
        };
        let mut rows = Vec::with_capacity(lines + lenses.len());
        let mut line_rows = vec![usize::MAX; lines];
        let mut lens = lenses.iter().enumerate().peekable();
        for line in 0..lines {
            while let Some((ix, _)) = lens.next_if(|(_, lens)| lens.line <= line) {
                if !hidden[line] && lenses[ix].line == line {
                    rows.push(Row::Lens(ix));
                }
            }
            if !hidden[line] {
                line_rows[line] = rows.len();
                rows.push(Row::Line(line));
            }
        }
        self.layout = Layout {
            version: Some(key),
            rows,
            line_rows,
            lenses,
        };
    }

    fn counts_for(
        cache: &mut Option<(u64, Arc<HashMap<String, usize>>)>,
        buffer: &Buffer,
    ) -> Arc<HashMap<String, usize>> {
        match cache {
            Some((version, counts)) if *version == buffer.version() => counts.clone(),
            _ => {
                let counts = Arc::new(intel::word_counts(buffer.text()));
                *cache = Some((buffer.version(), counts.clone()));
                counts
            }
        }
    }

    pub(crate) fn folds(&self) -> &[Fold] {
        &self.analysis.folds
    }

    /// The row a line draws on, or the row of the fold header hiding it.
    pub(crate) fn row_of_line(&self, line: usize) -> usize {
        let mut line = line.min(self.layout.line_rows.len().saturating_sub(1));
        loop {
            match self.layout.line_rows.get(line) {
                Some(row) if *row != usize::MAX => return *row,
                Some(_) if line > 0 => line -= 1,
                _ => return 0,
            }
        }
    }

    pub(crate) fn reveal_primary(&mut self, strategy: ScrollStrategy) {
        self.refresh_layout();
        let Some(doc) = &self.doc else {
            return;
        };
        let head = self.primary().head;
        let line = doc.buffer.line_of(head);
        self.scroll.scroll_to_item(self.row_of_line(line), strategy);
        // Horizontal: keep the caret's column inside the visible width.
        let metrics = self.metrics;
        if metrics.advance > px(0.0) && metrics.width > px(0.0) {
            let text = doc.buffer.line(line);
            let column =
                syntax::column_at(text, head - doc.buffer.line_range(line).start, self.tab);
            let x = metrics.advance * column as f32;
            let margin = metrics.advance * 4.0;
            if x < self.h_offset + margin {
                self.h_offset = (x - margin).max(px(0.0));
            } else if x > self.h_offset + metrics.width - margin {
                self.h_offset = x - metrics.width + margin;
            }
        }
    }

    fn reveal(&mut self, motion_backward: bool, cx: &mut Context<Self>) {
        self.reveal_primary(if motion_backward {
            ScrollStrategy::Top
        } else {
            ScrollStrategy::Bottom
        });
        cx.notify();
    }

    // ---- selections ------------------------------------------------------

    pub(crate) fn set_selections(&mut self, selections: Vec<Selection>, cx: &mut Context<Self>) {
        let Some(doc) = self.doc.as_mut() else {
            return;
        };
        let len = doc.buffer.len();
        let clamped: Vec<Selection> = selections
            .into_iter()
            .map(|selection| Selection {
                anchor: clamp_boundary(doc.buffer.text(), selection.anchor.min(len)),
                head: clamp_boundary(doc.buffer.text(), selection.head.min(len)),
                goal: selection.goal,
            })
            .collect();
        doc.selections = merged(clamped);
        self.unfold_cursors();
        self.dismiss_transient_for_caret();
        self.restart_blink(cx);
    }

    /// Closes the completion menu with its lookup, so a lookup still
    /// running cannot reopen what Escape or a keystroke closed. Returns
    /// whether the menu was open.
    pub(crate) fn close_completion(&mut self) -> bool {
        self._completion_task = None;
        self.completion.take().is_some()
    }

    /// Closes signature help with its lookup; see [`Self::close_completion`].
    pub(crate) fn close_signature(&mut self) -> bool {
        self._signature_task = None;
        self.signature.take().is_some()
    }

    /// Completion and signature help follow the caret; leaving their word
    /// or call closes them.
    fn dismiss_transient_for_caret(&mut self) {
        let head = self.primary().head;
        if let Some(completion) = &self.completion
            && !(completion.word.start..=completion.word.end).contains(&head)
        {
            self.close_completion();
        }
        if self.signature.is_some() && self.call_at_caret().is_none() {
            self.close_signature();
        }
    }

    pub(crate) fn motion(&mut self, motion: Motion, extend: bool, cx: &mut Context<Self>) {
        self.refresh_layout();
        let Some(doc) = &self.doc else {
            return;
        };
        let vertical = matches!(motion, Motion::Up | Motion::Down | Motion::Page(_));
        let hidden = |line: usize| self.layout.line_rows.get(line) == Some(&usize::MAX);
        let next: Vec<Selection> = doc
            .selections
            .iter()
            .map(|selection| {
                let mut next = moved(&doc.buffer, *selection, motion, extend);
                while vertical && hidden(doc.buffer.line_of(next.head)) {
                    let again = moved(&doc.buffer, next, motion, extend);
                    if again.head == next.head {
                        break;
                    }
                    next = again;
                }
                next
            })
            .collect();
        self.set_selections(next, cx);
        self.reveal(motion.backward(), cx);
    }

    pub(crate) fn select_all(&mut self, cx: &mut Context<Self>) {
        let len = self.text().len();
        self.set_selections(vec![Selection::span(0..len)], cx);
    }

    /// A new cursor a line above the topmost cursor or below the lowest,
    /// at the primary cursor's column.
    pub(crate) fn add_cursor(&mut self, below: bool, cx: &mut Context<Self>) {
        let Some(doc) = &self.doc else {
            return;
        };
        let primary = self.primary();
        let edge = if below {
            doc.selections.iter().max_by_key(|selection| selection.head)
        } else {
            doc.selections.iter().min_by_key(|selection| selection.head)
        }
        .copied()
        .unwrap_or(primary);
        let column = primary
            .goal
            .unwrap_or_else(|| doc.buffer.point(primary.head).1);
        let from = Selection {
            goal: Some(column),
            ..Selection::caret(edge.head)
        };
        let motion = if below { Motion::Down } else { Motion::Up };
        let next = moved(&doc.buffer, from, motion, false);
        if doc.buffer.line_of(next.head) == doc.buffer.line_of(edge.head) {
            return;
        }
        let mut all = doc.selections.clone();
        all.push(next);
        self.set_selections(all, cx);
        self.reveal(!below, cx);
    }

    /// Selects the word at the primary cursor, then each next place its text appears.
    pub(crate) fn select_next(&mut self, cx: &mut Context<Self>) {
        let Some(doc) = &self.doc else {
            return;
        };
        let primary = self.primary();
        if primary.is_empty() {
            let word = doc.buffer.word_at(primary.head);
            let mut all = doc.selections.clone();
            all.pop();
            all.push(Selection::span(word));
            return self.set_selections(all, cx);
        }
        let needle = doc.buffer.text()[primary.range()].to_string();
        let Some(range) = next_occurrence(
            doc.buffer.text(),
            &needle,
            primary.range().end,
            &doc.selections,
        ) else {
            return;
        };
        let mut all = doc.selections.clone();
        all.push(Selection::span(range));
        self.set_selections(all, cx);
        self.reveal(false, cx);
    }

    /// Selects every place the primary selection's text (or word) appears.
    pub(crate) fn select_all_occurrences(&mut self, cx: &mut Context<Self>) {
        let Some(doc) = &self.doc else {
            return;
        };
        let primary = self.primary();
        let range = if primary.is_empty() {
            doc.buffer.word_at(primary.head)
        } else {
            primary.range()
        };
        if range.is_empty() {
            return;
        }
        let needle = &doc.buffer.text()[range.clone()];
        let whole = needle.chars().all(super::buffer::is_word_char);
        let options = FindOptions {
            case: true,
            word: whole,
            regex: false,
        };
        let Ok(found) = find::find_all(doc.buffer.text(), needle, options) else {
            return;
        };
        let mut all: Vec<Selection> = found
            .into_iter()
            .filter(|hit| *hit != range)
            .map(Selection::span)
            .collect();
        all.push(Selection::span(range));
        self.set_selections(all, cx);
    }

    /// Escape: one cursor again, and nothing floating over the code.
    pub(crate) fn escape(&mut self, cx: &mut Context<Self>) {
        if self.close_completion() || self.close_signature() || self.hover.take().is_some() {
            cx.notify();
            return;
        }
        if self.find.open {
            self.close_find(cx);
            return;
        }
        let primary = self.primary();
        self.set_selections(vec![Selection::caret(primary.head)], cx);
    }

    // ---- folding ---------------------------------------------------------

    /// Folds the block that opens on a line, or opens it again; cursors
    /// inside move to its header.
    pub(crate) fn toggle_fold(&mut self, line: usize, cx: &mut Context<Self>) {
        self.refresh_layout();
        let Some(fold) = self
            .analysis
            .folds
            .iter()
            .find(|fold| fold.header == line)
            .copied()
        else {
            return;
        };
        let Some(doc) = self.doc.as_mut() else {
            return;
        };
        if !doc.folded.remove(&line) {
            let header_end = doc.buffer.line_range(line).end;
            let kept: Vec<Selection> = doc
                .selections
                .iter()
                .map(|selection| {
                    if (line + 1..=fold.end).contains(&doc.buffer.line_of(selection.head)) {
                        Selection::caret(header_end)
                    } else {
                        *selection
                    }
                })
                .collect();
            doc.selections = merged(kept);
            doc.folded.insert(line);
        }
        self.fold_generation += 1;
        cx.notify();
    }

    /// Folds the innermost open block around the primary cursor.
    pub(crate) fn fold_at_cursor(&mut self, cx: &mut Context<Self>) {
        self.refresh_layout();
        let Some(doc) = &self.doc else {
            return;
        };
        let line = doc.buffer.line_of(self.primary().head);
        let header = self
            .analysis
            .folds
            .iter()
            .filter(|fold| {
                fold.header <= line && line <= fold.end && !doc.folded.contains(&fold.header)
            })
            .map(|fold| fold.header)
            .next_back();
        if let Some(header) = header {
            self.toggle_fold(header, cx);
        }
    }

    pub(crate) fn unfold_at_cursor(&mut self, cx: &mut Context<Self>) {
        let Some(doc) = &self.doc else {
            return;
        };
        let line = doc.buffer.line_of(self.primary().head);
        if doc.folded.contains(&line) {
            self.toggle_fold(line, cx);
        }
    }

    pub(crate) fn fold_all(&mut self, fold: bool, cx: &mut Context<Self>) {
        self.refresh_layout();
        let Some(doc) = self.doc.as_mut() else {
            return;
        };
        if fold {
            // Fold the outermost blocks only, so unfolding one reveals its
            // own structure folded as it was.
            let mut covered_until = None;
            for fold in &self.analysis.folds {
                if covered_until.is_some_and(|end| fold.header <= end) {
                    continue;
                }
                doc.folded.insert(fold.header);
                covered_until = Some(fold.end);
            }
            let line_of = |offset| doc.buffer.line_of(offset);
            let selections: Vec<Selection> = doc
                .selections
                .iter()
                .map(|selection| {
                    let line = line_of(selection.head);
                    match self.analysis.folds.iter().find(|fold| {
                        doc.folded.contains(&fold.header) && fold.header < line && line <= fold.end
                    }) {
                        Some(fold) => Selection::caret(doc.buffer.line_range(fold.header).end),
                        None => *selection,
                    }
                })
                .collect();
            doc.selections = merged(selections);
        } else {
            doc.folded.clear();
        }
        self.fold_generation += 1;
        cx.notify();
    }

    /// Opens every fold that hides a cursor.
    fn unfold_cursors(&mut self) {
        let Some(doc) = self.doc.as_mut() else {
            return;
        };
        if doc.folded.is_empty() {
            return;
        }
        let lines: Vec<usize> = doc
            .selections
            .iter()
            .map(|selection| doc.buffer.line_of(selection.head))
            .collect();
        let folds = &self.analysis.folds;
        let before = doc.folded.len();
        doc.folded.retain(|header| {
            folds
                .iter()
                .find(|fold| fold.header == *header)
                .is_some_and(|fold| {
                    !lines
                        .iter()
                        .any(|line| (header + 1..=fold.end).contains(line))
                })
        });
        if doc.folded.len() != before {
            self.fold_generation += 1;
        }
    }

    // ---- editing ---------------------------------------------------------

    /// Replaces ranges all at once and leaves a caret after each insert.
    pub(crate) fn apply(
        &mut self,
        edits: Vec<(Range<usize>, String)>,
        typing: bool,
        cx: &mut Context<Self>,
    ) {
        if !self.editable() || edits.is_empty() {
            return;
        }
        let primary = self.primary().range().start;
        let edits = joined(edits);
        let lead = edits
            .iter()
            .rposition(|(range, _)| range.start <= primary)
            .unwrap_or(0);
        let mut carets = carets_after(&edits);
        let lead = carets.remove(lead);
        carets.push(lead);
        self.apply_with(edits, carets, typing, cx);
    }

    /// Applies edits and sets the given selections after them, as one step.
    pub(crate) fn apply_with(
        &mut self,
        edits: Vec<(Range<usize>, String)>,
        after: Vec<Selection>,
        typing: bool,
        cx: &mut Context<Self>,
    ) {
        if !self.editable() {
            return;
        }
        let Some(doc) = self.doc.as_mut() else {
            return;
        };
        let before = doc.selections.clone();
        let composing = self.composing.is_some();
        if !doc.history.apply(
            &mut doc.buffer,
            &edits,
            &before,
            typing || composing,
            Instant::now(),
        ) {
            return;
        }
        doc.selections = merged(after);
        doc.history.set_selections_after(&doc.selections);
        self.marked = None;
        self.after_edit(cx);
    }

    fn after_edit(&mut self, cx: &mut Context<Self>) {
        self.refresh_layout();
        self.unfold_cursors();
        self.refresh_find_matches();
        self.hover = None;
        self.save_error = None;
        let dirty = self.is_dirty();
        if dirty != self.was_dirty {
            self.was_dirty = dirty;
            cx.emit(EditorEvent::DirtyChanged);
        }
        self.restart_blink(cx);
        self.reveal_primary(ScrollStrategy::Bottom);
        cx.notify();
    }

    /// Replaces each selection with `text`.
    pub(crate) fn insert(&mut self, text: &str, typing: bool, cx: &mut Context<Self>) {
        let edits = self
            .selections()
            .iter()
            .map(|selection| (selection.range(), text.to_string()))
            .collect();
        self.apply(edits, typing, cx);
    }

    /// Types text at each cursor; an opener brings its closer, a closer
    /// already there is stepped over, and word characters drive completion.
    pub(crate) fn type_text(&mut self, text: &str, cx: &mut Context<Self>) {
        if !self.editable() {
            return;
        }
        let Some(doc) = &self.doc else {
            return;
        };
        let mut chars = text.chars();
        let (Some(ch), None) = (chars.next(), chars.next()) else {
            self.insert(text, true, cx);
            return;
        };
        let next = |caret: usize| doc.buffer.text()[caret..].chars().next();
        let previous = |caret: usize| doc.buffer.text()[..caret].chars().next_back();
        let all_empty = doc.selections.iter().all(Selection::is_empty);
        if all_empty
            && PAIRS.iter().any(|(_, close)| *close == ch)
            && doc
                .selections
                .iter()
                .all(|selection| next(selection.head) == Some(ch))
        {
            self.motion(Motion::Right, false, cx);
            self.after_typed(ch, cx);
            return;
        }
        let pair = PAIRS.iter().find(|(open, _)| *open == ch);
        let free = |caret: usize| {
            next(caret).is_none_or(|after| after.is_whitespace() || ")]},;".contains(after))
        };
        // Quotes pair only between words, not inside one (it's, don't).
        let quote_ok = |caret: usize| {
            !matches!(ch, '"' | '\'')
                || previous(caret).is_none_or(|before| !super::buffer::is_word_char(before))
        };
        match pair {
            Some((open, close))
                if all_empty
                    && doc
                        .selections
                        .iter()
                        .all(|selection| free(selection.head) && quote_ok(selection.head)) =>
            {
                let edits: Vec<(Range<usize>, String)> = doc
                    .selections
                    .iter()
                    .map(|selection| (selection.range(), format!("{open}{close}")))
                    .collect();
                let carets = carets_after(&edits)
                    .into_iter()
                    .map(|caret| Selection::caret(caret.head - close.len_utf8()))
                    .collect();
                self.apply_with(edits, carets, true, cx);
            }
            Some((open, close)) if !all_empty && *open != '\'' => {
                // Wrap each selection.
                let edits: Vec<(Range<usize>, String)> = doc
                    .selections
                    .iter()
                    .map(|selection| {
                        (
                            selection.range(),
                            format!("{open}{}{close}", &doc.buffer.text()[selection.range()]),
                        )
                    })
                    .collect();
                let after = carets_after(&edits)
                    .into_iter()
                    .zip(&edits)
                    .map(|(caret, (range, _))| {
                        let end = caret.head - close.len_utf8();
                        Selection::span(end - range.len()..end)
                    })
                    .collect();
                self.apply_with(edits, after, false, cx);
            }
            _ => self.insert(text, true, cx),
        }
        self.after_typed(ch, cx);
    }

    /// Opens or narrows completion after a word character, and signature
    /// help after an opening parenthesis or a comma.
    fn after_typed(&mut self, ch: char, cx: &mut Context<Self>) {
        if super::buffer::is_word_char(ch) {
            self.update_completion(false, cx);
        } else {
            self.close_completion();
        }
        if matches!(ch, '(' | ',') {
            self.update_signature(cx);
        } else if ch == ')' {
            self.close_signature();
        }
    }

    /// Deletes each selection, or what `reach` finds from each bare caret.
    pub(crate) fn delete_by(
        &mut self,
        reach: fn(&Buffer, usize) -> Range<usize>,
        cx: &mut Context<Self>,
    ) {
        let Some(doc) = &self.doc else {
            return;
        };
        let edits = doc
            .selections
            .iter()
            .map(|selection| {
                let range = if selection.is_empty() {
                    reach(&doc.buffer, selection.head)
                } else {
                    selection.range()
                };
                (range, String::new())
            })
            .collect();
        self.apply(edits, false, cx);
        if self.completion.is_some() {
            self.update_completion(false, cx);
        }
    }

    /// Backspace: an empty pair the caret sits inside goes as one.
    pub(crate) fn backspace(&mut self, cx: &mut Context<Self>) {
        self.delete_by(
            |buffer, at| {
                let before = buffer.text()[..at].chars().next_back();
                let after = buffer.text()[at..].chars().next();
                match (before, after) {
                    (Some(open), Some(close)) if PAIRS.contains(&(open, close)) => {
                        at - open.len_utf8()..at + close.len_utf8()
                    }
                    _ => buffer.previous(at)..at,
                }
            },
            cx,
        );
    }

    /// A new line at each cursor, indented like its own; one step deeper
    /// after an opener, with the closer on a line of its own.
    pub(crate) fn newline(&mut self, cx: &mut Context<Self>) {
        let Some(doc) = &self.doc else {
            return;
        };
        let unit = indent_unit(&doc.buffer, self.tab);
        let line_break = doc.buffer.line_break();
        let edits: Vec<(Range<usize>, String)> = doc
            .selections
            .iter()
            .map(|selection| {
                let at = selection.range().start;
                let line = doc.buffer.line_of(at);
                let indent: String = doc
                    .buffer
                    .line(line)
                    .chars()
                    .take(doc.buffer.indent(line))
                    .take(doc.buffer.point(at).1)
                    .collect();
                let before = doc.buffer.text()[doc.buffer.line_range(line).start..at]
                    .trim_end()
                    .chars()
                    .next_back();
                let after = doc.buffer.text()[selection.range().end..].chars().next();
                let opens = before.is_some_and(|ch| "{[(".contains(ch))
                    || (before == Some(':')
                        && matches!(doc.language, SourceLanguage::Python | SourceLanguage::Yaml));
                let text = match (opens, after) {
                    (true, Some(close)) if "}])".contains(close) => {
                        format!("{line_break}{indent}{unit}{line_break}{indent}")
                    }
                    (true, _) => format!("{line_break}{indent}{unit}"),
                    _ => format!("{line_break}{indent}"),
                };
                (selection.range(), text)
            })
            .collect();
        // The caret goes to the end of the first inserted line.
        let carets: Vec<Selection> = carets_after(&edits)
            .into_iter()
            .zip(&edits)
            .map(|(caret, (_, text))| {
                let split = text.matches('\n').count() == 2;
                if split {
                    let tail =
                        text.rfind('\n').map_or(0, |at| text.len() - at) + line_break.len() - 1;
                    Selection::caret(caret.head - tail)
                } else {
                    caret
                }
            })
            .collect();
        let primary_index = primary_index(&edits, self.primary().range().start);
        let mut carets = carets;
        let lead = carets.remove(primary_index);
        carets.push(lead);
        self.close_completion();
        self.apply_with(edits, carets, false, cx);
    }

    /// The lines the selections touch, each once.
    fn touched(&self) -> Vec<usize> {
        let Some(doc) = &self.doc else {
            return Vec::new();
        };
        let mut lines: Vec<usize> = doc
            .selections
            .iter()
            .flat_map(|selection| {
                let range = selection.range();
                let last = if range.end > range.start && doc.buffer.point(range.end).1 == 0 {
                    range.end - 1
                } else {
                    range.end
                };
                doc.buffer.line_of(range.start)..=doc.buffer.line_of(last)
            })
            .collect();
        lines.sort_unstable();
        lines.dedup();
        lines
    }

    /// Tab: accepts a completion, indents touched lines when a selection
    /// spans lines, or fills to the next tab stop.
    pub(crate) fn indent(&mut self, cx: &mut Context<Self>) {
        if self.completion.is_some() {
            self.accept_completion(cx);
            return;
        }
        let Some(doc) = &self.doc else {
            return;
        };
        let unit = indent_unit(&doc.buffer, self.tab);
        let spans_lines = doc.selections.iter().any(|selection| {
            doc.buffer.line_of(selection.range().start) != doc.buffer.line_of(selection.range().end)
        });
        if !spans_lines {
            let edits = doc
                .selections
                .iter()
                .map(|selection| {
                    let text = if unit == "\t" {
                        "\t".to_owned()
                    } else {
                        let line = doc.buffer.line_of(selection.range().start);
                        let column = syntax::column_at(
                            doc.buffer.line(line),
                            selection.range().start - doc.buffer.line_range(line).start,
                            self.tab,
                        );
                        " ".repeat(unit.len() - column % unit.len())
                    };
                    (selection.range(), text)
                })
                .collect();
            return self.apply(edits, false, cx);
        }
        let edits = self
            .touched()
            .into_iter()
            .filter(|line| !doc.buffer.line(*line).trim().is_empty())
            .map(|line| {
                let start = doc.buffer.line_range(line).start;
                (start..start, unit.clone())
            })
            .collect();
        self.shift_lines(edits, cx);
    }

    /// Shift-Tab: takes one indent step off each touched line.
    pub(crate) fn outdent(&mut self, cx: &mut Context<Self>) {
        let Some(doc) = &self.doc else {
            return;
        };
        let unit = indent_unit(&doc.buffer, self.tab);
        let edits = self
            .touched()
            .into_iter()
            .filter_map(|line| {
                let start = doc.buffer.line_range(line).start;
                let text = doc.buffer.line(line);
                let cut = if text.starts_with('\t') {
                    1
                } else {
                    text.bytes()
                        .take_while(|byte| *byte == b' ')
                        .count()
                        .min(unit.len().max(1))
                };
                (cut > 0).then(|| (start..start + cut, String::new()))
            })
            .collect();
        self.shift_lines(edits, cx);
    }

    /// Applies line-start edits and keeps each selection over the same text.
    fn shift_lines(&mut self, edits: Vec<(Range<usize>, String)>, cx: &mut Context<Self>) {
        let Some(doc) = &self.doc else {
            return;
        };
        let kept: Vec<Selection> = doc
            .selections
            .iter()
            .map(|selection| Selection {
                anchor: shifted(selection.anchor, &edits),
                head: shifted(selection.head, &edits),
                goal: None,
            })
            .collect();
        self.apply_with(edits, kept, false, cx);
    }

    /// Comments the touched lines, or takes the comment off when every one has it.
    pub(crate) fn toggle_comment(&mut self, cx: &mut Context<Self>) {
        let Some(doc) = &self.doc else {
            return;
        };
        let Some(prefix) = syntax::comment_prefix(doc.language) else {
            return;
        };
        let lines: Vec<usize> = self
            .touched()
            .into_iter()
            .filter(|line| !doc.buffer.line(*line).trim().is_empty())
            .collect();
        let commented = |line: usize| doc.buffer.line(line).trim_start().starts_with(prefix);
        let edits: Vec<(Range<usize>, String)> =
            if !lines.is_empty() && lines.iter().all(|line| commented(*line)) {
                lines
                    .iter()
                    .map(|line| {
                        let start = doc.buffer.line_range(*line).start + doc.buffer.indent(*line);
                        let rest = &doc.buffer.text()[start..];
                        let cut = if rest[prefix.len()..].starts_with(' ') {
                            prefix.len() + 1
                        } else {
                            prefix.len()
                        };
                        (start..start + cut, String::new())
                    })
                    .collect()
            } else {
                let column = lines
                    .iter()
                    .map(|line| doc.buffer.indent(*line))
                    .min()
                    .unwrap_or(0);
                lines
                    .iter()
                    .map(|line| {
                        let at = doc.buffer.offset(*line, column);
                        (at..at, format!("{prefix} "))
                    })
                    .collect()
            };
        self.shift_lines(edits, cx);
    }

    /// Moves the touched lines up or down one, as a block.
    pub(crate) fn move_lines(&mut self, down: bool, cx: &mut Context<Self>) {
        let Some(doc) = &self.doc else {
            return;
        };
        let lines = self.touched();
        let (Some(&first), Some(&last)) = (lines.first(), lines.last()) else {
            return;
        };
        let line_count = doc.buffer.lines();
        if (down && last + 1 >= line_count) || (!down && first == 0) {
            return;
        }
        let line_break = doc.buffer.line_break();
        let block_start = doc.buffer.line_range(first).start;
        let block_end = doc.buffer.line_range(last).end;
        let block = doc.buffer.text()[block_start..block_end].to_string();
        let (edit, delta): ((Range<usize>, String), isize) = if down {
            let next = doc.buffer.line_range(last + 1);
            let neighbour = doc.buffer.text()[next.clone()].to_string();
            (
                (
                    block_start..next.end,
                    format!("{neighbour}{line_break}{block}"),
                ),
                (neighbour.len() + line_break.len()) as isize,
            )
        } else {
            let previous = doc.buffer.line_range(first - 1);
            let neighbour = doc.buffer.text()[previous.clone()].to_string();
            (
                (
                    previous.start..block_end,
                    format!("{block}{line_break}{neighbour}"),
                ),
                -((neighbour.len() + line_break.len()) as isize),
            )
        };
        let moved: Vec<Selection> = doc
            .selections
            .iter()
            .map(|selection| Selection {
                anchor: (selection.anchor as isize + delta) as usize,
                head: (selection.head as isize + delta) as usize,
                goal: selection.goal,
            })
            .collect();
        self.apply_with(vec![edit], moved, false, cx);
        self.reveal(!down, cx);
    }

    /// Copies the touched lines below (or above) themselves.
    pub(crate) fn duplicate_lines(&mut self, cx: &mut Context<Self>) {
        let Some(doc) = &self.doc else {
            return;
        };
        let lines = self.touched();
        let (Some(&first), Some(&last)) = (lines.first(), lines.last()) else {
            return;
        };
        let line_break = doc.buffer.line_break();
        let start = doc.buffer.line_range(first).start;
        let end = doc.buffer.line_range(last).end;
        let block = doc.buffer.text()[start..end].to_string();
        let delta = block.len() + line_break.len();
        let moved: Vec<Selection> = doc
            .selections
            .iter()
            .map(|selection| Selection {
                anchor: selection.anchor + delta,
                head: selection.head + delta,
                goal: selection.goal,
            })
            .collect();
        self.apply_with(
            vec![(end..end, format!("{line_break}{block}"))],
            moved,
            false,
            cx,
        );
    }

    /// Deletes the touched lines whole.
    pub(crate) fn delete_lines(&mut self, cx: &mut Context<Self>) {
        let Some(doc) = &self.doc else {
            return;
        };
        let lines = self.touched();
        let (Some(&first), Some(&last)) = (lines.first(), lines.last()) else {
            return;
        };
        let start = doc.buffer.line_range(first).start;
        let end = if last + 1 < doc.buffer.lines() {
            doc.buffer.line_range(last + 1).start
        } else {
            doc.buffer.len()
        };
        let start = if end == doc.buffer.len() && first > 0 {
            doc.buffer.line_range(first - 1).end
        } else {
            start
        };
        self.apply_with(
            vec![(start..end, String::new())],
            vec![Selection::caret(start)],
            false,
            cx,
        );
    }

    pub(crate) fn undo(&mut self, cx: &mut Context<Self>) {
        if !self.editable() {
            return;
        }
        let Some(doc) = self.doc.as_mut() else {
            return;
        };
        if let Some(selections) = doc.history.undo(&mut doc.buffer) {
            doc.selections = merged(if selections.is_empty() {
                vec![Selection::caret(0)]
            } else {
                selections
            });
            self.after_edit(cx);
        }
    }

    pub(crate) fn redo(&mut self, cx: &mut Context<Self>) {
        if !self.editable() {
            return;
        }
        let Some(doc) = self.doc.as_mut() else {
            return;
        };
        if let Some(selections) = doc.history.redo(&mut doc.buffer) {
            let len = doc.buffer.len();
            doc.selections = merged(if selections.is_empty() {
                vec![Selection::caret(len)]
            } else {
                selections
            });
            self.after_edit(cx);
        }
    }

    // ---- clipboard -------------------------------------------------------

    /// Each selection's text, top to bottom; a bare caret copies its whole line.
    fn copied(&self) -> Vec<(Range<usize>, String)> {
        let Some(doc) = &self.doc else {
            return Vec::new();
        };
        let mut selections = doc.selections.clone();
        selections.sort_by_key(|selection| selection.range().start);
        selections
            .iter()
            .map(|selection| {
                let range = if selection.is_empty() {
                    let line = doc.buffer.line_of(selection.head);
                    let range = doc.buffer.line_range(line);
                    range.start..doc.buffer.next(range.end)
                } else {
                    selection.range()
                };
                (range.clone(), doc.buffer.text()[range].to_string())
            })
            .collect()
    }

    pub(crate) fn copy(&mut self, cx: &mut Context<Self>) {
        let texts: Vec<String> = self.copied().into_iter().map(|(_, text)| text).collect();
        if !texts.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(texts.join("\n")));
        }
    }

    pub(crate) fn cut(&mut self, cx: &mut Context<Self>) {
        if !self.editable() {
            return;
        }
        let copied = self.copied();
        let texts: Vec<String> = copied.iter().map(|(_, text)| text.clone()).collect();
        cx.write_to_clipboard(ClipboardItem::new_string(texts.join("\n")));
        self.apply(
            copied
                .into_iter()
                .map(|(range, _)| (range, String::new()))
                .collect(),
            false,
            cx,
        );
    }

    /// Pastes one line at each cursor when the counts match, else the whole
    /// text at every one.
    pub(crate) fn paste(&mut self, text: &str, cx: &mut Context<Self>) {
        let Some(doc) = &self.doc else {
            return;
        };
        let text = if doc.buffer.line_break() == "\r\n" {
            text.replace("\r\n", "\n").replace('\n', "\r\n")
        } else {
            text.replace("\r\n", "\n")
        };
        let lines: Vec<&str> = text.split('\n').collect();
        let mut selections = doc.selections.clone();
        selections.sort_by_key(|selection| selection.range().start);
        let edits = if lines.len() == selections.len() && lines.len() > 1 {
            selections
                .iter()
                .zip(lines)
                .map(|(selection, line)| {
                    (selection.range(), line.trim_end_matches('\r').to_string())
                })
                .collect()
        } else {
            doc.selections
                .iter()
                .map(|selection| (selection.range(), text.clone()))
                .collect()
        };
        self.apply(edits, false, cx);
    }

    // ---- find ------------------------------------------------------------

    pub(crate) fn open_find(
        &mut self,
        replace: bool,
        window: &mut gpui::Window,
        cx: &mut Context<Self>,
    ) {
        let Some(doc) = &self.doc else {
            return;
        };
        let primary = self.primary();
        let selected = &doc.buffer.text()[primary.range()];
        if !primary.is_empty() && !selected.contains('\n') {
            self.find.query.clear();
            self.find.query.insert(selected);
        } else if self.find.query.is_empty()
            && let Some(word) = doc.buffer.identifier_at(primary.head)
        {
            self.find.query.insert(&doc.buffer.text()[word]);
        }
        self.find.query.select_all();
        self.find.open = true;
        self.find.replace_open = replace || self.find.replace_open && self.find.open;
        if replace {
            self.find.replace_open = true;
        }
        self.find.in_replacement = false;
        self.find.version = None;
        self.refresh_find_matches();
        self.find.current = find::step_from(&self.find.matches, primary.range().start, true);
        window.focus(&self.find_focus, cx);
        cx.notify();
    }

    pub(crate) fn close_find(&mut self, cx: &mut Context<Self>) {
        self.find.open = false;
        self.find.matches.clear();
        self.find.current = None;
        self.find.version = None;
        cx.notify();
    }

    /// Re-finds matches when the query, options, or text changed.
    pub(crate) fn refresh_find_matches(&mut self) {
        if !self.find.open {
            return;
        }
        let Some(doc) = &self.doc else {
            return;
        };
        if self.find.version == Some(doc.buffer.version()) {
            return;
        }
        self.find.version = Some(doc.buffer.version());
        match find::find_all(doc.buffer.text(), self.find.query.text(), self.find.options) {
            Ok(matches) => {
                self.find.error = None;
                self.find.matches = matches;
            }
            Err(error) => {
                self.find.error = Some(error);
                self.find.matches.clear();
            }
        }
        let caret = self.primary().range().start;
        self.find.current = if self.find.matches.is_empty() {
            None
        } else {
            self.find
                .current
                .filter(|current| *current < self.find.matches.len())
                .or_else(|| find::step_from(&self.find.matches, caret, true))
        };
    }

    /// The query or its options changed: search again from the caret.
    pub(crate) fn find_changed(&mut self, cx: &mut Context<Self>) {
        self.find.version = None;
        self.find.current = None;
        self.refresh_find_matches();
        if let Some(current) = self.find.current {
            self.select_match(current, cx);
        }
        cx.notify();
    }

    fn select_match(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(range) = self.find.matches.get(index).cloned() else {
            return;
        };
        self.find.current = Some(index);
        self.set_selections(vec![Selection::span(range)], cx);
        self.reveal_primary(ScrollStrategy::Center);
    }

    pub(crate) fn find_step(&mut self, forward: bool, cx: &mut Context<Self>) {
        self.refresh_find_matches();
        let caret = match self
            .find
            .current
            .and_then(|current| self.find.matches.get(current))
        {
            Some(current) if forward => current.end,
            Some(current) => current.start,
            None => {
                let primary = self.primary().range();
                if forward { primary.end } else { primary.start }
            }
        };
        if let Some(next) = find::step_from(&self.find.matches, caret, forward) {
            self.select_match(next, cx);
        }
        cx.notify();
    }

    pub(crate) fn replace_current(&mut self, cx: &mut Context<Self>) {
        self.refresh_find_matches();
        let Some(index) = self.find.current else {
            return;
        };
        let Some(range) = self.find.matches.get(index).cloned() else {
            return;
        };
        let replacement = match find::replacement_for(
            self.text(),
            range.clone(),
            self.find.query.text(),
            self.find.replacement.text(),
            self.find.options,
        ) {
            Ok(text) => text,
            Err(error) => {
                self.find.error = Some(error);
                cx.notify();
                return;
            }
        };
        let end = range.start + replacement.len();
        self.apply_with(
            vec![(range, replacement)],
            vec![Selection::caret(end)],
            false,
            cx,
        );
        self.refresh_find_matches();
        if let Some(next) = find::step_from(&self.find.matches, end, true) {
            self.select_match(next, cx);
        }
    }

    pub(crate) fn replace_all(&mut self, cx: &mut Context<Self>) {
        let edits = match find::replace_all(
            self.text(),
            self.find.query.text(),
            self.find.replacement.text(),
            self.find.options,
        ) {
            Ok(edits) => edits,
            Err(error) => {
                self.find.error = Some(error);
                cx.notify();
                return;
            }
        };
        if edits.is_empty() {
            return;
        }
        let caret = self.primary().head;
        let after = vec![Selection::caret(shifted(caret, &edits))];
        self.apply_with(edits, after, false, cx);
    }

    // ---- saving ----------------------------------------------------------

    /// Writes the document. A file changed on disk since it was read is not
    /// overwritten unless `force`; the conflict banner offers both ways.
    pub(crate) fn save(&mut self, force: bool, cx: &mut Context<Self>) {
        let (Some(doc), Some(intelligence)) = (self.doc.as_ref(), self.intelligence.clone()) else {
            return;
        };
        if doc.read_only {
            self.save_error = Some("This file is read-only.".into());
            cx.notify();
            return;
        }
        if !force && !doc.is_dirty() && !self.conflict {
            return;
        }
        let relative = doc.relative_path.clone();
        let text = doc.buffer.text().to_string();
        let expected = doc.modified;
        let version = doc.buffer.version();
        let document = doc.id;
        let absolute_path = doc.absolute_path.clone();
        self._save_task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { intelligence.save_file(&relative, &text, expected, force) })
                .await;
            let _ = this.update(cx, |this, cx| {
                let saved = SavedAway {
                    document,
                    absolute_path,
                    version,
                    modified: result.as_ref().ok().copied().flatten(),
                };
                if this.doc.as_ref().is_none_or(|doc| doc.id != document) {
                    // The saved document was switched away: its outcome
                    // belongs to it, not to whatever is shown now.
                    if result.is_ok() {
                        cx.emit(EditorEvent::SavedAway(saved));
                    }
                    return;
                }
                match result {
                    Ok(_) => {
                        this.conflict = false;
                        this.save_error = None;
                        if let Some(doc) = this.doc.as_mut() {
                            doc.apply_saved_away(&saved);
                        }
                        let dirty = this.is_dirty();
                        if dirty != this.was_dirty {
                            this.was_dirty = dirty;
                            cx.emit(EditorEvent::DirtyChanged);
                        }
                        cx.emit(EditorEvent::Saved);
                    }
                    Err(CodeIntelligenceError::ChangedOnDisk { .. }) => {
                        this.conflict = true;
                    }
                    Err(error) => this.save_error = Some(error.to_string()),
                }
                cx.notify();
            });
        }));
    }

    pub(crate) fn reload(&mut self, cx: &mut Context<Self>) {
        cx.emit(EditorEvent::Reload);
    }

    // ---- intelligence ----------------------------------------------------

    /// The word being typed at the primary caret, when it is an identifier.
    fn word_before_caret(&self) -> Option<Range<usize>> {
        let doc = self.doc.as_ref()?;
        let head = self.primary().head;
        let line_start = doc.buffer.line_range(doc.buffer.line_of(head)).start;
        let before = &doc.buffer.text()[line_start..head];
        let len: usize = before
            .chars()
            .rev()
            .take_while(|ch| super::buffer::is_word_char(*ch))
            .map(char::len_utf8)
            .sum();
        (len > 0).then(|| head - len..head)
    }

    /// Opens or refreshes completion for the word before the caret: words in
    /// this file now, workspace symbols when the index answers.
    pub(crate) fn update_completion(&mut self, explicit: bool, cx: &mut Context<Self>) {
        let Some(word) = self.word_before_caret() else {
            self.close_completion();
            cx.notify();
            return;
        };
        let Some(doc) = &self.doc else {
            return;
        };
        let prefix = doc.buffer.text()[word.clone()].to_string();
        if (prefix.chars().count() < 2 && !explicit)
            || prefix.starts_with(|ch: char| ch.is_ascii_digit())
        {
            self.close_completion();
            cx.notify();
            return;
        }
        let counts = Self::counts_for(&mut self.word_counts, &doc.buffer);
        let words = counts.keys().map(|label| Candidate {
            label: label.clone(),
            detail: None,
            kind: CandidateKind::Word,
        });
        let mut known: Vec<Candidate> = words.collect();
        if let Some(completion) = &self.completion {
            known.extend(
                completion
                    .items
                    .iter()
                    .filter(|item| item.kind == CandidateKind::Symbol)
                    .cloned(),
            );
        }
        let items = intel::rank(&prefix, known, COMPLETIONS);
        self.completion = (!items.is_empty()).then_some(Completion {
            word: word.clone(),
            items,
            selected: 0,
        });
        cx.notify();
        let Some(intelligence) = self.intelligence.clone() else {
            return;
        };
        let query = prefix.clone();
        self._completion_task = Some(cx.spawn(async move |this, cx| {
            let symbols = cx
                .background_executor()
                .spawn(async move { intelligence.symbol_names(&query, 40) })
                .await;
            let _ = this.update(cx, |this, cx| {
                let Some(current) = this.word_before_caret() else {
                    return;
                };
                if this.text()[current.clone()] != *prefix {
                    return;
                }
                let mut candidates: Vec<Candidate> = this
                    .completion
                    .as_ref()
                    .map(|completion| completion.items.clone())
                    .unwrap_or_default();
                let counts = this.word_counts.as_ref().map(|(_, counts)| counts.clone());
                if let Some(counts) = counts {
                    candidates.extend(counts.keys().map(|label| Candidate {
                        label: label.clone(),
                        detail: None,
                        kind: CandidateKind::Word,
                    }));
                }
                candidates.extend(symbols.into_iter().map(|(label, detail)| Candidate {
                    label,
                    detail: Some(detail),
                    kind: CandidateKind::Symbol,
                }));
                let items = intel::rank(&prefix, candidates, COMPLETIONS);
                this.completion = (!items.is_empty()).then_some(Completion {
                    word: current,
                    items,
                    selected: 0,
                });
                cx.notify();
            });
        }));
    }

    pub(crate) fn move_completion(&mut self, delta: isize, cx: &mut Context<Self>) -> bool {
        let Some(completion) = self.completion.as_mut() else {
            return false;
        };
        let count = completion.items.len() as isize;
        completion.selected = (completion.selected as isize + delta).rem_euclid(count) as usize;
        cx.notify();
        true
    }

    pub(crate) fn accept_completion(&mut self, cx: &mut Context<Self>) -> bool {
        self._completion_task = None;
        let Some(completion) = self.completion.take() else {
            return false;
        };
        let Some(item) = completion.items.get(completion.selected) else {
            return false;
        };
        let Some(doc) = &self.doc else {
            return false;
        };
        // Every cursor whose word matches the primary one's takes the item.
        let typed = doc.buffer.text()[completion.word.clone()].to_string();
        let edits: Vec<(Range<usize>, String)> = doc
            .selections
            .iter()
            .filter_map(|selection| {
                let start = selection.head.checked_sub(typed.len())?;
                (doc.buffer.text().get(start..selection.head) == Some(typed.as_str()))
                    .then(|| (start..selection.head, item.label.clone()))
            })
            .collect();
        self.apply(edits, false, cx);
        true
    }

    pub(crate) fn accept_completion_at(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(completion) = self.completion.as_mut() {
            completion.selected = index;
        }
        self.accept_completion(cx);
    }

    /// The call around the primary caret, from its line.
    fn call_at_caret(&self) -> Option<(String, usize)> {
        let doc = self.doc.as_ref()?;
        let head = self.primary().head;
        let start = doc.buffer.line_range(doc.buffer.line_of(head)).start;
        intel::call_context(&doc.buffer.text()[start..head])
    }

    /// Looks up the callee's declaration for signature help.
    pub(crate) fn update_signature(&mut self, cx: &mut Context<Self>) {
        let Some((name, argument)) = self.call_at_caret() else {
            self.close_signature();
            cx.notify();
            return;
        };
        if let Some(signature) = self.signature.as_mut()
            && signature.name == name
        {
            signature.argument = argument;
            cx.notify();
            return;
        }
        let Some(intelligence) = self.intelligence.clone() else {
            return;
        };
        let lookup = name.clone();
        self._signature_task = Some(cx.spawn(async move |this, cx| {
            let definitions = cx
                .background_executor()
                .spawn(async move { intelligence.definitions(&lookup, 8) })
                .await;
            let _ = this.update(cx, |this, cx| {
                let Some((current, argument)) = this.call_at_caret() else {
                    return;
                };
                if current != name {
                    return;
                }
                this.signature = definitions
                    .into_iter()
                    .find(|hit| hit.preview.contains('('))
                    .map(|hit| Signature {
                        name,
                        argument,
                        declaration: hit.preview.clone(),
                        location: (hit.relative_path.clone(), hit.line.unwrap_or(1)),
                    });
                cx.notify();
            });
        }));
    }

    /// Starts (or keeps) a hover lookup for the identifier at `offset`.
    pub(crate) fn hover_at(&mut self, offset: Option<usize>, cx: &mut Context<Self>) {
        let word = offset.and_then(|offset| self.doc.as_ref()?.buffer.identifier_at(offset));
        let Some(word) = word else {
            if self.hover.take().is_some() | self.hover_pending.take().is_some() {
                self._hover_task = None;
                cx.notify();
            }
            return;
        };
        if self.hover.as_ref().is_some_and(|hover| hover.word == word)
            || self.hover_pending.as_ref() == Some(&word)
        {
            return;
        }
        self.hover = None;
        let Some(intelligence) = self.intelligence.clone() else {
            return;
        };
        let name = self.text()[word.clone()].to_string();
        if name.starts_with(|ch: char| ch.is_ascii_digit()) {
            return;
        }
        self.hover_pending = Some(word.clone());
        self._hover_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(HOVER_DELAY).await;
            let lookup = name.clone();
            let definitions = cx
                .background_executor()
                .spawn(async move { intelligence.definitions(&lookup, 6) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.hover_pending.as_ref() != Some(&word) {
                    return;
                }
                this.hover_pending = None;
                if definitions.is_empty() {
                    return;
                }
                this.hover = Some(Hover {
                    word,
                    name,
                    definitions,
                });
                cx.notify();
            });
        }));
    }

    /// Jumps to the declaration of the identifier at `offset`: in this file
    /// directly, elsewhere by asking the Files surface to open it.
    pub(crate) fn go_to_definition(&mut self, offset: usize, cx: &mut Context<Self>) {
        let Some(word) = self
            .doc
            .as_ref()
            .and_then(|doc| doc.buffer.identifier_at(offset))
        else {
            return;
        };
        let name = self.text()[word].to_string();
        let Some(intelligence) = self.intelligence.clone() else {
            return;
        };
        let current = self.relative_path().map(std::path::Path::to_path_buf);
        cx.spawn(async move |this, cx| {
            let lookup = name.clone();
            let definitions = cx
                .background_executor()
                .spawn(async move { intelligence.definitions(&lookup, 16) })
                .await;
            let _ = this.update(cx, |this, cx| {
                // Prefer a declaration in this file.
                let best = definitions
                    .iter()
                    .find(|hit| Some(&hit.relative_path) == current.as_ref())
                    .or_else(|| definitions.first());
                match best {
                    Some(hit) if Some(&hit.relative_path) == current.as_ref() => {
                        this.go_to(hit.line.unwrap_or(1), 1, cx);
                    }
                    Some(hit) => cx.emit(EditorEvent::OpenLocation {
                        path: hit.relative_path.clone(),
                        line: hit.line.unwrap_or(1),
                    }),
                    None => cx.emit(EditorEvent::SearchWorkspace(name)),
                }
            });
        })
        .detach();
    }

    // ---- caret blink -----------------------------------------------------

    /// Shows the cursors now and restarts their blink, only while focused.
    pub(crate) fn restart_blink(&mut self, cx: &mut Context<Self>) {
        self.caret_on = true;
        self.blink_epoch += 1;
        self.last_activity = Instant::now();
        let epoch = self.blink_epoch;
        self._blink = self.focused.then(|| {
            cx.spawn(async move |editor, cx| {
                loop {
                    cx.background_executor().timer(BLINK).await;
                    let alive = editor.update(cx, |editor, cx| {
                        if editor.blink_epoch != epoch || !editor.focused {
                            return false;
                        }
                        if editor.last_activity.elapsed() > BLINK_IDLE {
                            editor.caret_on = true;
                            cx.notify();
                            return false;
                        }
                        editor.caret_on = !editor.caret_on;
                        cx.notify();
                        true
                    });
                    if !matches!(alive, Ok(true)) {
                        return;
                    }
                }
            })
        });
        cx.notify();
    }

    /// Render tells the editor whether it holds focus; blink follows.
    pub(crate) fn sync_focus(&mut self, focused: bool, cx: &mut Context<Self>) {
        if self.focused == focused {
            return;
        }
        self.focused = focused;
        if focused {
            self.restart_blink(cx);
        } else {
            self._blink = None;
            self.dragging = false;
            self.caret_on = true;
        }
    }

    // ---- input method ----------------------------------------------------

    pub(crate) fn begin_composition(&mut self) {
        if self.composing.is_none() {
            self.composing = Some(self.selections().to_vec());
            if let Some(doc) = self.doc.as_mut() {
                doc.history.begin_group();
            }
        }
    }

    pub(crate) fn end_composition(&mut self) {
        if self.composing.take().is_some()
            && let Some(doc) = self.doc.as_mut()
        {
            doc.history.end_group();
        }
    }
}

/// The edit a primary caret at `primary` belongs to.
fn primary_index(edits: &[(Range<usize>, String)], primary: usize) -> usize {
    edits
        .iter()
        .rposition(|(range, _)| range.start <= primary)
        .unwrap_or(0)
}

fn clamp_boundary(text: &str, mut at: usize) -> usize {
    while !text.is_char_boundary(at) {
        at -= 1;
    }
    at
}

/// The next place `needle` appears after `from`, wrapping, that no current
/// selection already covers.
pub(crate) fn next_occurrence(
    text: &str,
    needle: &str,
    from: usize,
    selections: &[Selection],
) -> Option<Range<usize>> {
    if needle.is_empty() {
        return None;
    }
    let taken = |range: &Range<usize>| {
        selections
            .iter()
            .any(|selection| selection.range() == *range)
    };
    text[from..]
        .match_indices(needle)
        .map(|(at, _)| from + at..from + at + needle.len())
        .chain(
            text.match_indices(needle)
                .map(|(at, _)| at..at + needle.len()),
        )
        .find(|range| !taken(range))
}

/// The indent a file uses: a tab when most indented lines start with one,
/// otherwise the smallest common run of spaces (2 or 4), else `tab`.
pub(crate) fn indent_unit(buffer: &Buffer, tab: usize) -> String {
    let mut tabs = 0;
    let mut spaces = 0;
    let mut two = 0;
    for line in (0..buffer.lines()).take(2_000) {
        let text = buffer.line(line);
        if text.starts_with('\t') {
            tabs += 1;
        } else if text.starts_with(' ') && !text.trim().is_empty() {
            spaces += 1;
            let run = text.bytes().take_while(|byte| *byte == b' ').count();
            if run % 4 != 0 && run % 2 == 0 {
                two += 1;
            }
        }
    }
    if tabs > spaces {
        "\t".into()
    } else if two * 4 > spaces.max(1) {
        "  ".into()
    } else {
        " ".repeat(tab)
    }
}

/// The tab width a file reads best at: its space indent unit, else 4.
fn detect_tab(buffer: &Buffer) -> usize {
    match indent_unit(buffer, 4).as_str() {
        "  " => 2,
        _ => 4,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indent_units_follow_the_file() {
        assert_eq!(
            indent_unit(&Buffer::new("fn a() {\n\tx;\n\ty;\n}"), 4),
            "\t"
        );
        assert_eq!(
            indent_unit(&Buffer::new("a:\n  b:\n    c: 1\n  d: 2"), 4),
            "  "
        );
        assert_eq!(
            indent_unit(&Buffer::new("fn a() {\n    x;\n        y;\n}"), 4),
            "    "
        );
        assert_eq!(indent_unit(&Buffer::new(""), 4), "    ");
    }

    #[test]
    fn next_occurrence_wraps_and_skips_selected_ranges() {
        let text = "ab ab ab";
        let taken = [Selection::span(0..2), Selection::span(3..5)];
        assert_eq!(next_occurrence(text, "ab", 5, &taken), Some(6..8));
        let all = [
            Selection::span(0..2),
            Selection::span(3..5),
            Selection::span(6..8),
        ];
        assert_eq!(next_occurrence(text, "ab", 8, &all), None);
        assert_eq!(
            next_occurrence(text, "ab", 8, &taken[..1]),
            Some(3..5),
            "wraps to the start"
        );
    }
}
