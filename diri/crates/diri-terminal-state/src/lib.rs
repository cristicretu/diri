//! Headless terminal emulation for status detection.
//!
//! The daemon has to know what an agent *painted*, not just what bytes it
//! wrote: "do you want to proceed?" only means a blocker if it is still on the
//! visible screen after all the cursor movement, erases and redraws that
//! preceded it. So every session runs a real VT emulator with no renderer
//! attached, and detection reads plain text off its grid.
//!
//! The shared Rust implementation wraps `alacritty_terminal`, a portable
//! headless terminal core used by both the local Engine and remote Holder.
//!
//! One gap is filled by hand: OSC 9;4 (progress) is a ConEmu extension that the
//! emulator does not model, so it is scanned out of the byte stream directly —
//! see [`scan_progress`].

mod notifications;
mod program_status;
pub use notifications::{AGENT_EXIT_OSC, TerminalNotification};
pub use program_status::{BlockedKind, ProgramRecord, ProgramState};

use std::sync::mpsc::{self, Receiver, SyncSender};

use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::{CONTENT_CHANGED, Config, Term, TermMode};
use alacritty_terminal::vte::ansi::{Color, NamedColor, Processor, Rgb};
use diri_proto::grid::{
    ChangedRow, GridCell, GridRowCodec, GridUpdate, LinkSpan, RowMetadata, TermColor, TermStyle,
};
use diri_proto::terminal::{
    MouseEncoding, MouseModes, MouseTrackingMode, TerminalMouseEvent, TerminalMouseModifiers,
    encode_mouse_event,
};

/// Receiver-side authority for a remote terminal stream. A mirror accepts a
/// full snapshot as a new baseline and then only contiguous sequenced diffs;
/// a gap forces the caller to request another full snapshot instead of
/// silently displaying a plausible but incorrect screen.
#[derive(Clone, Debug, Default)]
pub struct GridMirror {
    cells: Vec<GridCell>,
    cols: u16,
    rows: u16,
    cursor_col: u16,
    cursor_row: u16,
    cursor_visible: bool,
    sequence: Option<u64>,
    annotations: Vec<RowMetadata>,
    alt_screen: bool,
    bracketed_paste: bool,
    mouse: MouseModes,
}

impl GridMirror {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn apply_snapshot(
        &mut self,
        sequence: u64,
        grid: &GridUpdate,
        alt_screen: bool,
        bracketed_paste: bool,
        mouse: MouseModes,
    ) -> Result<(), MirrorError> {
        if !grid.is_full_snapshot {
            return Err(MirrorError::SnapshotRequired);
        }
        validate_grid(grid)?;
        grid.apply(&mut self.cells);
        self.update_metadata(grid);
        self.sequence = Some(sequence);
        self.alt_screen = alt_screen;
        self.bracketed_paste = bracketed_paste;
        self.mouse = mouse;
        Ok(())
    }

    pub fn apply_delta(
        &mut self,
        sequence: u64,
        grid: &GridUpdate,
        alt_screen: bool,
        bracketed_paste: bool,
        mouse: MouseModes,
    ) -> Result<(), MirrorError> {
        let Some(previous) = self.sequence else {
            return Err(MirrorError::SnapshotRequired);
        };
        let expected = previous
            .checked_add(1)
            .ok_or(MirrorError::SequenceOverflow)?;
        if sequence != expected {
            return Err(MirrorError::SequenceGap {
                expected,
                actual: sequence,
            });
        }
        if grid.is_full_snapshot || grid.cols != self.cols || grid.rows != self.rows {
            return Err(MirrorError::SnapshotRequired);
        }
        validate_grid(grid)?;
        grid.apply(&mut self.cells);
        self.update_metadata(grid);
        self.sequence = Some(sequence);
        self.alt_screen = alt_screen;
        self.bracketed_paste = bracketed_paste;
        self.mouse = mouse;
        Ok(())
    }

    fn update_metadata(&mut self, grid: &GridUpdate) {
        if grid.is_full_snapshot {
            self.annotations.clear();
        }
        self.annotations
            .resize(usize::from(grid.rows), RowMetadata::default());
        for row in &grid.changed_rows {
            if let Some(target) = self.annotations.get_mut(usize::from(row.y)) {
                target.clone_from(&row.metadata);
            }
        }
        self.cols = grid.cols;
        self.rows = grid.rows;
        self.cursor_col = grid.cursor_col;
        self.cursor_row = grid.cursor_row;
        self.cursor_visible = grid.cursor_visible;
    }

    #[must_use]
    pub fn cells(&self) -> &[GridCell] {
        &self.cells
    }

    #[must_use]
    pub const fn size(&self) -> (u16, u16) {
        (self.cols, self.rows)
    }

    #[must_use]
    pub const fn cursor(&self) -> (u16, u16, bool) {
        (self.cursor_col, self.cursor_row, self.cursor_visible)
    }

    #[must_use]
    pub const fn sequence(&self) -> Option<u64> {
        self.sequence
    }

    #[must_use]
    pub const fn modes(&self) -> (bool, bool, MouseModes) {
        (self.alt_screen, self.bracketed_paste, self.mouse)
    }

    #[must_use]
    pub fn full_update(&self) -> Option<GridUpdate> {
        self.sequence?;
        let cols = usize::from(self.cols);
        let rows = usize::from(self.rows);
        if self.cells.len() != cols.saturating_mul(rows) {
            return None;
        }
        let changed_rows = (0..rows)
            .map(|row| {
                let mut changed = ChangedRow::new(
                    row as u16,
                    self.cells[row * cols..(row + 1) * cols].to_vec(),
                );
                changed.metadata = self.annotations.get(row).cloned().unwrap_or_default();
                changed
            })
            .collect();
        Some(GridUpdate {
            cols: self.cols,
            rows: self.rows,
            cursor_col: self.cursor_col,
            cursor_row: self.cursor_row,
            cursor_visible: self.cursor_visible,
            is_full_snapshot: true,
            changed_rows,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MirrorError {
    SnapshotRequired,
    SequenceGap { expected: u64, actual: u64 },
    SequenceOverflow,
    InvalidGrid(&'static str),
}

impl std::fmt::Display for MirrorError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SnapshotRequired => formatter.write_str("a full terminal snapshot is required"),
            Self::SequenceGap { expected, actual } => {
                write!(
                    formatter,
                    "terminal sequence gap: expected {expected}, got {actual}"
                )
            }
            Self::SequenceOverflow => formatter.write_str("terminal sequence overflow"),
            Self::InvalidGrid(detail) => write!(formatter, "invalid terminal grid: {detail}"),
        }
    }
}

impl std::error::Error for MirrorError {}

fn validate_grid(grid: &GridUpdate) -> Result<(), MirrorError> {
    if grid.cols == 0 || grid.rows == 0 {
        return Err(MirrorError::InvalidGrid("dimensions must be non-zero"));
    }
    if grid.cursor_col >= grid.cols || grid.cursor_row >= grid.rows {
        return Err(MirrorError::InvalidGrid("cursor is outside the grid"));
    }
    for row in &grid.changed_rows {
        if row.y >= grid.rows
            || row.cells.len() != usize::from(grid.cols)
            || !row.metadata.validate(usize::from(grid.cols))
        {
            return Err(MirrorError::InvalidGrid(
                "changed row is outside the grid or has the wrong width",
            ));
        }
    }
    Ok(())
}

/// Screen text and hyperlink targets for the artifact scanner; see
/// [`HeadlessScreen::link_source`].
#[derive(Clone, Debug, Default)]
pub struct LinkSource {
    /// Logical lines: soft-wrapped rows are joined without a newline.
    pub text: String,
    /// OSC 8 targets in screen order, consecutive duplicates collapsed, each
    /// with the byte offset in `text` where its link text starts.
    pub hyperlinks: Vec<(usize, String)>,
    /// Terminal width, so the scanner can recognize a row an application
    /// filled and broke by hand.
    pub cols: usize,
}

/// Plain-text terminal state consumed by the local status detector.
#[derive(Clone, Debug, Default)]
pub struct ScreenSnapshot {
    pub lines: Vec<String>,
    pub osc_title: Option<String>,
    pub osc_progress_state: Option<i64>,
    /// Bumps whenever visible content changes.
    pub content_seq: u64,
}

impl ScreenSnapshot {
    pub fn from_lines<I, S>(lines: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            lines: lines.into_iter().map(Into::into).collect(),
            ..Self::default()
        }
    }
}

/// Retain up to 10,000 physical history rows within 4 MiB of stored row
/// representation. Cold rows are losslessly compressed; visible cells and
/// temporary caller-owned responses are separate. Dense differential builds
/// translate this allowance into a width-dependent row count.
const HISTORY_STORAGE_BUDGET_BYTES: usize = 4 << 20;

fn history_line_limit(cols: usize) -> usize {
    #[cfg(feature = "compact-history")]
    {
        let _ = cols;
        10_000
    }
    #[cfg(not(feature = "compact-history"))]
    {
        let bytes_per_line = cols.max(1).saturating_mul(std::mem::size_of::<Cell>());
        HISTORY_STORAGE_BUDGET_BYTES / bytes_per_line
    }
}

/// Fixed screen geometry handed to the emulator.
#[derive(Clone, Copy, Debug)]
struct Geometry {
    cols: usize,
    rows: usize,
}

impl Dimensions for Geometry {
    fn total_lines(&self) -> usize {
        // History beyond the visible screen is not useful for detection: rules
        // read the current screen. Keeping scrollback at zero also bounds the
        // memory a runaway session can cost the daemon.
        self.rows
    }

    fn screen_lines(&self) -> usize {
        self.rows
    }

    fn columns(&self) -> usize {
        self.cols
    }
}

/// Collects the events the emulator emits. Only the title is interesting.
#[derive(Clone)]
struct Collector(SyncSender<Event>);

const EVENT_QUEUE_CAPACITY: usize = 64;

impl EventListener for Collector {
    fn send_event(&self, event: Event) {
        // A full channel means nobody is draining it, which is not worth
        // failing a session over.
        let _ = self.0.try_send(event);
    }
}

pub use alacritty_terminal::term::keyboard::KeyboardSnapshot;

pub struct HeadlessScreen {
    // False after an old/partial cache restore. Capability negotiation never
    // changes this observation or enables parser support. Exact parking must
    // preserve it independently of Term flags.
    keyboard_enhancements_known: bool,
    term: Term<Collector>,
    parser: Processor,
    events: Receiver<Event>,
    geometry: Geometry,

    title: Option<String>,
    progress_state: Option<i64>,
    progress_value: Option<i64>,
    progress_reports: u64,

    content_seq: u64,
    filled_cells: usize,
    /// Per-row text fingerprints let status detection distinguish real text
    /// changes from cursor/mode damage without hashing the full viewport.
    row_digests: Vec<u64>,
    row_filled_cells: Vec<usize>,
    /// Rows whose fingerprint may predate their cells: the parser changed
    /// them without line damage (it clears a wide character's leading spacer
    /// on the line above the cursor). Partial damage leaves those
    /// fingerprints as they are; full damage hashes them again.
    stale_fingerprints: Vec<bool>,
    /// The previous settle's fingerprints, reused when scrolling only moved
    /// rows. Kept to avoid an allocation per settle.
    moved_digests: Vec<u64>,
    moved_filled_cells: Vec<usize>,
    moved_stale: Vec<bool>,
    /// False after cells changed outside the parser (restore), until full
    /// damage re-fingerprints every row. Moved fingerprints are only reused
    /// while this holds.
    fingerprints_track_cells: bool,
    #[cfg(test)]
    fingerprinted_rows: std::sync::atomic::AtomicUsize,
    /// Tests compare against hashing every row after full damage.
    #[cfg(test)]
    reuse_moved_fingerprints: bool,
    /// Damage produced by the current parser advance, reused to avoid a fresh
    /// allocation for every PTY read.
    current_damage_rows: Vec<bool>,
    /// Damage accumulated until the next attached-client grid publication.
    /// This remains valid across multiple parser advances in one output batch.
    pending_damage_rows: Vec<bool>,
    /// Trailing bytes of the previous chunk, so an OSC split across a read
    /// boundary is still recognized.
    progress_carry: Vec<u8>,
    notifications: Option<notifications::NotificationParser>,
    /// Answers the emulator owes the child: a cursor-position report, a device
    /// attributes reply, and anything else generated in response to a query.
    /// A program that asks and is never answered blocks until its own timeout,
    /// or forever, so the pump must write these back to the PTY.
    replies: Vec<u8>,
    /// Whether the title moved since the last settle, tracked where it is
    /// assigned so no per-chunk clone is needed to notice.
    title_changed: bool,

    /// Diff baseline for [`grid_update`]: the cells last handed out, so the
    /// next call sends only changed rows.
    ///
    /// [`grid_update`]: HeadlessScreen::grid_update
    last_cells: Vec<GridCell>,
    last_annotations: Vec<RowMetadata>,
    last_grid_cols: usize,
    last_grid_rows: usize,
}

impl HeadlessScreen {
    pub fn new(cols: usize, rows: usize) -> Self {
        Self::new_with_keyboard_config(cols, rows, false)
    }

    /// Explicit parser opt-in. Only a fully capable input owner may choose
    /// this constructor; a peer capability or restored cache never enables it.
    pub fn new_with_keyboard_enhancements(cols: usize, rows: usize) -> Self {
        Self::new_with_keyboard_config(cols, rows, true)
    }

    fn new_with_keyboard_config(cols: usize, rows: usize, kitty_keyboard: bool) -> Self {
        let geometry = Geometry {
            cols: cols.max(1),
            rows: rows.max(1),
        };
        let (sender, events) = mpsc::sync_channel(EVENT_QUEUE_CAPACITY);
        let config = Config {
            kitty_keyboard,
            scrolling_history: history_line_limit(geometry.cols),
            ..Config::default()
        };
        let term = Term::new(config, &geometry, Collector(sender));
        #[cfg(feature = "compact-history")]
        let term = {
            let mut term = term;
            term.grid_mut().enable_compact_history();
            term
        };
        let mut screen = Self {
            keyboard_enhancements_known: true,
            term,
            parser: Processor::new(),
            events,
            geometry,
            title: None,
            progress_state: None,
            progress_value: None,
            progress_reports: 0,
            content_seq: 0,
            filled_cells: 0,
            row_digests: Vec::new(),
            row_filled_cells: Vec::new(),
            stale_fingerprints: Vec::new(),
            moved_digests: Vec::new(),
            moved_filled_cells: Vec::new(),
            moved_stale: Vec::new(),
            fingerprints_track_cells: true,
            #[cfg(test)]
            fingerprinted_rows: Default::default(),
            #[cfg(test)]
            reuse_moved_fingerprints: true,
            current_damage_rows: vec![false; geometry.rows],
            pending_damage_rows: vec![true; geometry.rows],
            progress_carry: Vec::new(),
            notifications: None,
            replies: Vec::new(),
            title_changed: false,
            last_cells: Vec::new(),
            last_annotations: Vec::new(),
            last_grid_cols: 0,
            last_grid_rows: 0,
        };
        screen.rebuild_content_cache();
        screen
    }

    /// Opt in only in the local Engine. Holders do not interpret product alerts.
    pub fn with_notifications(mut self) -> Self {
        self.notifications = Some(Default::default());
        self
    }

    /// Resets the emulator at its current dimensions without touching a PTY.
    ///
    /// Both screens and history, terminal modes, title/progress, queued replies,
    /// notifications and incomplete escape/synchronized-update sequences are
    /// discarded. Constructor options remain enabled exactly as before. A reset
    /// establishes known default keyboard state even after incomplete recovery.
    /// The content revision advances and the next `grid_update(false)` is full.
    ///
    /// The terminal owner must order this with output, publication and durable
    /// replay. This primitive does not implement a session reset or log boundary.
    pub fn reset(&mut self) {
        let mut reset = Self::new_with_keyboard_config(
            self.geometry.cols,
            self.geometry.rows,
            self.keyboard_enhancements_enabled(),
        );
        if self.notifications.is_some() {
            reset = reset.with_notifications();
        }
        reset.content_seq = self.content_seq.saturating_add(1);
        *self = reset;
    }

    pub fn reset_notification_sequence(&mut self) {
        if let Some(parser) = &mut self.notifications {
            parser.reset_sequence();
        }
    }

    /// Whether notifications or an OSC 52 clipboard write await delivery.
    pub fn has_notifications(&self) -> bool {
        self.notifications
            .as_ref()
            .is_some_and(|parser| !parser.ready.is_empty() || parser.clipboard.is_some())
    }

    /// The newest undelivered OSC 52 clipboard write, still base64-encoded.
    pub fn take_clipboard(&mut self) -> Option<String> {
        self.notifications.as_mut()?.clipboard.take()
    }

    /// The exit status the login-shell wrapper reported for its agent since
    /// the last call. See [`notifications::AGENT_EXIT_OSC`].
    pub fn take_agent_exit(&mut self) -> Option<i32> {
        self.notifications.as_mut()?.agent_exit.take()
    }

    /// Bumps whenever an `OSC 7501` report changes a program-status record.
    /// Zero, and never moving, without notifications.
    pub fn program_status_generation(&self) -> u64 {
        self.notifications
            .as_ref()
            .map_or(0, |parser| parser.program.generation())
    }

    /// The `OSC 7501` record that best describes what the program is doing:
    /// blocked before working before a result. `None` when nothing reports.
    pub fn program_status(&self) -> Option<ProgramRecord> {
        self.notifications.as_ref()?.program.summary()
    }

    /// Drops the `OSC 7501` records of a program that has ended. Returns
    /// whether there were any.
    pub fn end_program_status(&mut self) -> bool {
        self.notifications
            .as_mut()
            .is_some_and(|parser| parser.program.end_program())
    }

    pub fn take_notifications(&mut self) -> Vec<TerminalNotification> {
        self.notifications
            .as_mut()
            .map(|parser| parser.ready.drain(..).collect())
            .unwrap_or_default()
    }

    /// Feeds raw PTY output into the emulator.
    ///
    /// The whole chunk goes to the parser in one call — vte has a batched
    /// fast path for plain text that byte-at-a-time feeding defeats, and the
    /// difference is multi-x on heavy output like build logs.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.feed_with_history(bytes, 0);
    }

    /// Parses all terminal bytes while excluding a replay prefix from product
    /// notifications. A partial historical OSC cannot leak into live output.
    pub fn feed_with_history(&mut self, bytes: &[u8], historical_bytes: usize) {
        self.scan_progress(bytes);
        if let Some(parser) = &mut self.notifications {
            if historical_bytes != 0 {
                parser.reset_sequence();
            }
            parser.feed(&bytes[historical_bytes.min(bytes.len())..]);
            // An `OSC 7501` support query is answered ahead of the emulator's
            // replies for the same chunk. Programs detect support by sending
            // the query and then a device-attributes request, and read an
            // answer arriving before the attributes as support.
            self.replies.append(&mut parser.replies);
        }
        self.parser.advance(&mut self.term, bytes);
        self.settle();
    }

    /// Synchronized updates (DECSET 2026) the child has closed with its ESU,
    /// so far. A feed that raises it, and leaves no update open, ended on a
    /// frame the child declared complete.
    pub fn synchronized_updates_completed(&self) -> u64 {
        self.parser.synchronized_updates_completed()
    }

    /// Whether a synchronized update is open (its bytes are held back).
    pub fn in_synchronized_update(&self) -> bool {
        self.parser.sync_timeout().sync_timeout().is_some()
    }

    /// Ends a synchronized update (DECSET 2026) whose deadline has passed.
    ///
    /// Between `\e[?2026h` and `\e[?2026l` the parser holds every byte back so
    /// the repaint lands as one atomic screen — which is exactly what we want,
    /// and the reason a TUI that speaks this protocol never flickers. But the
    /// escape hatch is the host's job: vte's timeout only records a deadline,
    /// and nothing expires it. A child that opens a synchronized update and
    /// then dies, stalls, or simply overruns 150ms would otherwise freeze the
    /// pane until 2 MiB of output had piled up behind it. Callers tick this.
    ///
    /// Returns true when a held update was released.
    pub fn flush_expired_sync(&mut self) -> bool {
        let Some(deadline) = self.parser.sync_timeout().sync_timeout() else {
            return false;
        };
        if std::time::Instant::now() < deadline {
            return false;
        }
        self.parser.stop_sync(&mut self.term);
        self.settle();
        true
    }

    /// Post-parse bookkeeping shared by `feed` and `flush_expired_sync`.
    ///
    /// Damage is preserved at row granularity for both status detection and
    /// the next wire diff. Cursor-only damage hashes at most the touched rows;
    /// a one-line echo never scans the rest of the viewport.
    fn settle(&mut self) {
        #[cfg(feature = "compact-history")]
        self.term
            .bound_primary_history_storage(HISTORY_STORAGE_BUDGET_BYTES);
        self.drain_events();
        let rows = self.geometry.rows;
        self.current_damage_rows.resize(rows, false);
        self.current_damage_rows.fill(false);
        self.pending_damage_rows.resize(rows, false);
        let full = match self.term.damage() {
            alacritty_terminal::term::TermDamage::Full => {
                self.current_damage_rows.fill(true);
                self.pending_damage_rows.fill(true);
                true
            }
            alacritty_terminal::term::TermDamage::Partial(lines) => {
                for damage in lines {
                    if damage.line < rows {
                        self.current_damage_rows[damage.line] = true;
                        self.pending_damage_rows[damage.line] = true;
                    }
                }
                false
            }
        };
        // The title is compared where it is assigned, so settling costs no
        // clone of it per chunk of output.
        let mut content_changed = std::mem::take(&mut self.title_changed);
        if self.row_digests.len() != rows || self.row_filled_cells.len() != rows {
            self.rebuild_content_cache();
            content_changed = true;
        } else {
            // Scrolling damages the whole screen but only moves most rows:
            // a moved row keeps the fingerprint it had at the last settle.
            // Rows that any write touched since then are hashed again.
            let sources = self.term.damage_content_sources();
            #[cfg(test)]
            let sources = sources.filter(|_| self.reuse_moved_fingerprints);
            let moved = if full && self.fingerprints_track_cells {
                sources
            } else {
                None
            };
            if moved.is_some() {
                self.moved_digests.clone_from(&self.row_digests);
                self.moved_filled_cells.clone_from(&self.row_filled_cells);
                self.moved_stale.clone_from(&self.stale_fingerprints);
            }
            if !full && sources.is_none() {
                // Changes without line damage are unknown: hash every row at
                // the next full damage.
                self.fingerprints_track_cells = false;
            }
            for row in 0..rows {
                if !self.current_damage_rows[row] {
                    // Only partial damage skips rows. A change there without
                    // line damage leaves the fingerprint stale, as before.
                    if sources.is_some_and(|sources| sources[row] == CONTENT_CHANGED) {
                        self.stale_fingerprints[row] = true;
                    }
                    continue;
                }
                let source = moved.map_or(usize::MAX, |sources| sources[row] as usize);
                let (digest, filled) = if source < rows && !self.moved_stale[source] {
                    (self.moved_digests[source], self.moved_filled_cells[source])
                } else {
                    self.fingerprint_row(row)
                };
                self.stale_fingerprints[row] = false;
                if self.row_digests[row] != digest {
                    self.row_digests[row] = digest;
                    self.filled_cells = self
                        .filled_cells
                        .saturating_sub(self.row_filled_cells[row])
                        .saturating_add(filled);
                    self.row_filled_cells[row] = filled;
                    content_changed = true;
                }
            }
        }
        self.term.reset_damage();
        // Full damage fingerprinted every row that was not only moved.
        self.fingerprints_track_cells |= full;
        if content_changed {
            self.content_seq = self.content_seq.saturating_add(1);
        }
    }

    /// How many cells currently hold something other than a blank.
    ///
    /// A repaint that has erased but not yet redrawn is the one screen state
    /// no observer should ever see, and this is what makes it recognizable:
    /// output that only *removes* content is a repaint caught halfway. Free to
    /// maintain — it falls out of the fingerprint walk already done per feed.
    #[must_use]
    pub fn filled_cells(&self) -> usize {
        self.filled_cells
    }

    pub fn resize(&mut self, cols: usize, rows: usize) {
        let geometry = Geometry {
            cols: cols.max(1),
            rows: rows.max(1),
        };
        if self.geometry.cols == geometry.cols && self.geometry.rows == geometry.rows {
            return;
        }
        let old_limit = history_line_limit(self.geometry.cols);
        let new_limit = history_line_limit(geometry.cols);
        // A narrower screen needs the larger row allowance before reflow to
        // retain wrapped history. When widening, reflow first: rows may merge,
        // and trimming beforehand would discard history that still fits.
        if new_limit > old_limit {
            self.term.set_options(Config {
                scrolling_history: new_limit,
                ..Config::default()
            });
        }
        self.geometry = geometry;
        self.term.resize(self.geometry);
        if new_limit < old_limit {
            // set_options updates the primary history even while the alternate
            // screen is active, and preserves the limit across terminal reset.
            self.term.set_options(Config {
                scrolling_history: new_limit,
                ..Config::default()
            });
        }
        self.settle();
    }

    pub fn content_seq(&self) -> u64 {
        self.content_seq
    }

    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    /// True while the child is on the alternate screen (a full-screen program
    /// like an editor or pager).
    pub fn is_alt_screen(&self) -> bool {
        self.term
            .mode()
            .contains(alacritty_terminal::term::TermMode::ALT_SCREEN)
    }

    /// Whether the child has bracketed-paste mode on — submitted prompts are
    /// then framed as a paste so embedded newlines don't submit early.
    pub fn bracketed_paste(&self) -> bool {
        self.term
            .mode()
            .contains(alacritty_terminal::term::TermMode::BRACKETED_PASTE)
    }

    /// The current grid geometry.
    /// Whether the child asked for focus in/out reports (DEC 1004): left on
    /// under a shell, every window switch types `^[[I` / `^[[O` at its prompt.
    pub fn focus_reporting(&self) -> bool {
        self.term
            .mode()
            .contains(alacritty_terminal::term::TermMode::FOCUS_IN_OUT)
    }

    pub fn size(&self) -> (usize, usize) {
        (self.geometry.cols, self.geometry.rows)
    }

    pub fn invalidate_keyboard_enhancements(&mut self) {
        self.keyboard_enhancements_known = false;
    }

    pub fn keyboard_enhancements_enabled(&self) -> bool {
        self.term.keyboard_enhancements_enabled()
    }

    /// Unknown enhanced state may never masquerade as legacy-only input.
    /// A disabled legacy parser can retain its old cursor/keypad projection.
    pub fn input_keyboard_state(&self) -> Option<diri_proto::terminal_input::KeyboardState> {
        (self.keyboard_enhancements_known || !self.keyboard_enhancements_enabled())
            .then(|| self.keyboard_state())
    }

    /// Keyboard modes from the same parser that owns the visible terminal.
    pub fn keyboard_state(&self) -> diri_proto::terminal_input::KeyboardState {
        let mode = self.term.mode();
        diri_proto::terminal_input::KeyboardState {
            enhancements: self.keyboard_enhancements_known.then(|| {
                diri_proto::terminal_input::enhanced::KeyboardEnhancements::try_from(
                    alacritty_terminal::vte::ansi::KeyboardModes::from(*mode).bits(),
                )
                .expect("parser keyboard flags are bounded")
            }),
            application_cursor_keys: mode.contains(TermMode::APP_CURSOR),
            application_keypad: mode.contains(TermMode::APP_KEYPAD),
        }
    }

    /// A bounded projection of both keyboard stacks; unknown historical state
    /// must remain absent rather than becoming a fabricated known zero.
    pub fn keyboard_snapshot(&self) -> Option<KeyboardSnapshot> {
        self.keyboard_enhancements_known
            .then(|| self.term.keyboard_snapshot())
    }

    pub fn can_restore_keyboard_snapshot(&self, snapshot: &KeyboardSnapshot) -> bool {
        self.term.can_restore_keyboard_snapshot(snapshot)
    }

    pub fn restore_keyboard_snapshot(&mut self, snapshot: &KeyboardSnapshot) -> bool {
        if !self.term.restore_keyboard_snapshot(snapshot) {
            return false;
        }
        self.keyboard_enhancements_known = true;
        true
    }

    /// Restore checkpointed modes through the existing parser after grid restore.
    pub fn restore_keyboard_state(&mut self, state: diri_proto::terminal_input::KeyboardState) {
        // Current flags alone cannot reconstruct the saved inactive/pop state.
        // Only restore_keyboard_snapshot can establish enhanced state as known.
        self.keyboard_enhancements_known = false;
        self.feed(if state.application_cursor_keys {
            b"\x1b[?1h"
        } else {
            b"\x1b[?1l"
        });
        self.feed(if state.application_keypad {
            b"\x1b="
        } else {
            b"\x1b>"
        });
    }

    /// The independent tracking and encoding modes requested by the child.
    pub fn mouse_modes(&self) -> MouseModes {
        let mode = self.term.mode();
        let tracking = if mode.contains(TermMode::MOUSE_MOTION) {
            MouseTrackingMode::AnyMotion
        } else if mode.contains(TermMode::MOUSE_DRAG) {
            MouseTrackingMode::ButtonMotion
        } else if mode.contains(TermMode::MOUSE_REPORT_CLICK) {
            MouseTrackingMode::ButtonEvents
        } else {
            MouseTrackingMode::Off
        };
        let encoding = if mode.contains(TermMode::SGR_MOUSE) {
            MouseEncoding::Sgr
        } else {
            MouseEncoding::Legacy
        };
        MouseModes::new(tracking, encoding)
    }

    /// Whether any tracking mode is active.
    pub fn mouse_reporting(&self) -> bool {
        self.mouse_modes().is_reporting()
    }

    /// Cursor (col, row, visible) without touching cell data.
    pub fn cursor(&self) -> (u16, u16, bool) {
        let point = self.term.grid().cursor.point;
        (
            point.column.0 as u16,
            point.line.0.max(0) as u16,
            self.term.mode().contains(TermMode::SHOW_CURSOR),
        )
    }

    /// Builds a `GridUpdate` from the current screen. When `full` is true (or
    /// the geometry changed) every row is included and the diff baseline
    /// resets; otherwise only rows that changed since the last call are
    /// included.
    pub fn grid_update(&mut self, full: bool) -> GridUpdate {
        let cols = self.geometry.cols;
        let rows = self.geometry.rows;
        let geometry_changed = self.last_grid_cols != cols || self.last_grid_rows != rows;
        let force_full = full || geometry_changed;
        if geometry_changed {
            self.last_cells = vec![GridCell::BLANK; cols * rows];
            self.last_annotations = vec![RowMetadata::default(); rows];
            self.last_grid_cols = cols;
            self.last_grid_rows = rows;
        }

        let grid = self.term.grid();
        let candidate_count = if force_full {
            rows
        } else {
            self.pending_damage_rows
                .iter()
                .filter(|dirty| **dirty)
                .count()
        };
        let mut changed = Vec::new();
        for y in 0..rows {
            if !force_full && !self.pending_damage_rows.get(y).copied().unwrap_or(false) {
                continue;
            }
            let line = Line(y as i32);
            let base = y * cols;
            let metadata = self.row_metadata(line);
            let row = &mut self.last_cells[base..base + cols];
            let mut row_changed = force_full || self.last_annotations[y] != metadata;
            let source = &grid[line];
            for (x, previous) in row.iter_mut().enumerate() {
                let cell = wire_cell(&source[Column(x)]);
                row_changed |= *previous != cell;
                *previous = cell;
            }
            if !row_changed {
                continue;
            }
            if changed.is_empty() {
                changed.reserve_exact(candidate_count);
            }
            let mut changed_row = ChangedRow::new(y as u16, row.to_vec());
            self.last_annotations[y].clone_from(&metadata);
            changed_row.metadata = metadata;
            changed.push(changed_row);
        }
        self.pending_damage_rows.fill(false);

        let cursor = grid.cursor.point;
        GridUpdate {
            cols: cols as u16,
            rows: rows as u16,
            cursor_col: cursor.column.0 as u16,
            cursor_row: cursor.line.0.max(0) as u16,
            cursor_visible: self.term.mode().contains(TermMode::SHOW_CURSOR),
            is_full_snapshot: force_full,
            changed_rows: changed,
        }
    }

    /// A full-screen snapshot that does NOT disturb the diff baseline. Used
    /// to seed a fresh sink (or repair one that fell behind) without breaking
    /// other sinks' diffs.
    pub fn full_snapshot(&self) -> GridUpdate {
        let cols = self.geometry.cols;
        let rows = self.geometry.rows;
        let grid = self.term.grid();
        let mut all = Vec::with_capacity(rows);
        for y in 0..rows {
            let line = Line(y as i32);
            let mut row = Vec::with_capacity(cols);
            let source = &grid[line];
            for x in 0..cols {
                row.push(wire_cell(&source[Column(x)]));
            }
            let mut changed = ChangedRow::new(y as u16, row);
            changed.metadata = self.row_metadata(line);
            all.push(changed);
        }
        let cursor = grid.cursor.point;
        GridUpdate {
            cols: cols as u16,
            rows: rows as u16,
            cursor_col: cursor.column.0 as u16,
            cursor_row: cursor.line.0.max(0) as u16,
            cursor_visible: self.term.mode().contains(TermMode::SHOW_CURSOR),
            is_full_snapshot: true,
            changed_rows: all,
        }
    }

    /// Restores a persisted visible grid into a fresh emulator by synthesizing
    /// the byte stream that would have painted it. Each non-blank cell is
    /// cursor-addressed independently, because a wide glyph consumes two
    /// terminal columns while the grid also carries its blank continuation
    /// cell — a naive row string would shift everything after it.
    /// Styled rows above the visible grid, oldest first. Checkpoints persist
    /// these alongside the visible snapshot so adoption does not collapse a
    /// long session to a single scrollback row.
    pub fn history_snapshot(&mut self) -> Vec<Vec<GridCell>> {
        let history = self.term.grid().history_size();
        let cols = self.geometry.cols;
        let mut rows = Vec::with_capacity(history);
        for index in 0..history {
            let line = Line(index as i32 - history as i32);
            let mut row = Vec::with_capacity(cols);
            let source = &self.term.grid()[line];
            for x in 0..cols {
                row.push(wire_cell(&source[Column(x)]));
            }
            rows.push(row);
            self.finish_history_read_batch(index + 1, index + 1 == history);
        }
        rows
    }

    pub fn history_metadata(&mut self) -> Vec<RowMetadata> {
        let count = self.term.grid().history_size();
        let mut rows = Vec::with_capacity(count);
        for index in 0..count {
            rows.push(self.row_metadata(Line(index as i32 - count as i32)));
            self.finish_history_read_batch(index + 1, index + 1 == count);
        }
        rows
    }

    fn finish_history_read_batch(&mut self, completed: usize, finished: bool) {
        #[cfg(feature = "compact-history")]
        {
            let row_bytes = self
                .geometry
                .cols
                .saturating_mul(std::mem::size_of::<Cell>())
                .saturating_add(std::mem::size_of::<alacritty_terminal::grid::Row<Cell>>())
                .max(1);
            let batch = (128 * 1024 / row_bytes).clamp(1, 64);
            if finished || completed.is_multiple_of(batch) {
                self.term.grid_mut().release_history_read_cache();
            }
        }
        #[cfg(not(feature = "compact-history"))]
        let _ = (completed, finished);
    }

    pub fn restore_history_metadata(&mut self, rows: &[RowMetadata]) {
        let count = self.term.grid().history_size();
        if rows.len() != count {
            return;
        }
        for (index, metadata) in rows.iter().enumerate() {
            let line = Line(index as i32 - count as i32);
            for link in &metadata.links {
                let link_value = alacritty_terminal::term::cell::Hyperlink::new(
                    None::<String>,
                    link.uri.clone(),
                );
                for x in link.start..link.end {
                    self.term.grid_mut()[line][Column(usize::from(x))]
                        .set_hyperlink(Some(link_value.clone()));
                }
            }
            for (x, text) in &metadata.graphemes {
                for ch in text.chars() {
                    self.term.grid_mut()[line][Column(usize::from(*x))].push_zerowidth(ch);
                }
            }
            self.finish_history_read_batch(index + 1, index + 1 == count);
        }
        #[cfg(feature = "compact-history")]
        self.term
            .bound_primary_history_storage(HISTORY_STORAGE_BUDGET_BYTES);
    }

    pub fn restore(
        &mut self,
        history: &[Vec<GridCell>],
        update: &GridUpdate,
        alt_screen: bool,
        bracketed_paste: bool,
        mouse: MouseModes,
    ) -> bool {
        let cols = self.geometry.cols;
        let rows = self.geometry.rows;
        if !update.is_full_snapshot
            || update.cols as usize != cols
            || update.rows as usize != rows
            || update.changed_rows.len() != rows
            || history.len() > history_line_limit(cols)
            || history.iter().any(|row| row.len() != cols)
        {
            return false;
        }

        // Visible cells cannot prove the child's negotiated keyboard state.
        self.keyboard_enhancements_known = false;

        // Allocate scrollback in the emulator, then replace those rows with
        // the persisted cells. The visible grid is painted below; CSI 2 J
        // clears only that viewport and deliberately leaves history intact.
        if !history.is_empty() {
            let grid = self.term.grid_mut();
            grid.scroll_up(&(Line(0)..Line(rows as i32)), history.len());
            let restored_history = grid.history_size();
            if restored_history != history.len() {
                return false;
            }
            for (index, row) in history.iter().enumerate() {
                let line = Line(index as i32 - restored_history as i32);
                for (x, cell) in row.iter().enumerate() {
                    grid[line][Column(x)] = emulator_cell(*cell);
                }
            }
        }

        let mut bytes = Vec::with_capacity(cols * rows * 2);
        if alt_screen {
            bytes.extend_from_slice(b"\x1b[?1049h");
        }
        bytes.extend_from_slice(b"\x1b[H\x1b[2J");

        let mut sorted: Vec<&ChangedRow> = update.changed_rows.iter().collect();
        sorted.sort_by_key(|row| row.y);
        for row in sorted {
            if row.y as usize >= rows || row.cells.len() != cols {
                return false;
            }
            let mut previous: Option<&GridCell> = None;
            for (x, cell) in row.cells.iter().enumerate() {
                // The initial clear already produced true blank cells;
                // leaving them untouched keeps sparse checkpoints cheap.
                if *cell == GridCell::BLANK || cell.style.contains(TermStyle::WIDE_SPACER) {
                    continue;
                }
                bytes.extend_from_slice(format!("\x1b[{};{}H", row.y + 1, x + 1).as_bytes());
                if previous.is_none_or(|prev| {
                    prev.fg != cell.fg || prev.bg != cell.bg || prev.style != cell.style
                }) {
                    bytes.extend_from_slice(sgr(cell).as_bytes());
                }
                match char::from_u32(cell.scalar) {
                    Some(glyph) if cell.scalar != 0 => {
                        let mut buffer = [0u8; 4];
                        bytes.extend_from_slice(glyph.encode_utf8(&mut buffer).as_bytes());
                    }
                    _ => bytes.push(b' '),
                }
                previous = Some(cell);
            }
        }

        bytes.extend_from_slice(b"\x1b[0m");
        if bracketed_paste {
            bytes.extend_from_slice(b"\x1b[?2004h");
        }
        if let Some(mode) = mouse.tracking.dec_private_mode() {
            bytes.extend_from_slice(format!("\x1b[?{mode}h").as_bytes());
        }
        if matches!(mouse.encoding, MouseEncoding::Sgr) {
            bytes.extend_from_slice(b"\x1b[?1006h");
        }
        bytes.extend_from_slice(if update.cursor_visible {
            b"\x1b[?25h"
        } else {
            b"\x1b[?25l"
        });
        bytes.extend_from_slice(
            format!(
                "\x1b[{};{}H",
                (update.cursor_row as usize + 1).min(rows),
                (update.cursor_col as usize + 1).min(cols)
            )
            .as_bytes(),
        );
        self.feed(&bytes);
        for row in &update.changed_rows {
            for (x, cell) in row.cells.iter().enumerate() {
                let target = &mut self.term.grid_mut()[Line(i32::from(row.y))][Column(x)];
                restore_semantic_flags(target, *cell);
                if let Some(link) = row
                    .metadata
                    .links
                    .iter()
                    .find(|link| usize::from(link.start) <= x && x < usize::from(link.end))
                {
                    target.set_hyperlink(Some(alacritty_terminal::term::cell::Hyperlink::new(
                        None::<String>,
                        link.uri.clone(),
                    )));
                }
                if let Some((_, text)) = row
                    .metadata
                    .graphemes
                    .iter()
                    .find(|(col, _)| usize::from(*col) == x)
                {
                    for ch in text.chars() {
                        target.push_zerowidth(ch);
                    }
                }
            }
        }
        // Semantic flags and links were written after the parser settled;
        // fingerprints catch up only when rows are hashed again, so a moved
        // row must not keep its fingerprint until then.
        self.fingerprints_track_cells = false;
        // Force the next grid_update to be a full frame: the diff baseline
        // predates the restore.
        self.last_grid_cols = 0;
        self.last_grid_rows = 0;
        true
    }

    /// Encodes a wheel event for the child, when it asked for mouse
    /// reporting. Returns the bytes to write to the PTY — empty means the
    /// child doesn't care and the client should scroll its own scrollback.
    pub fn mouse_wheel(&self, up: bool, lines: usize, col: usize, row: usize) -> Vec<u8> {
        if !self.mouse_reporting() || lines == 0 {
            return Vec::new();
        }
        let x = col.min(self.geometry.cols.saturating_sub(1));
        let y = row.min(self.geometry.rows.saturating_sub(1));
        let mut out = Vec::new();
        for _ in 0..lines {
            let event = if up {
                TerminalMouseEvent::WheelUp
            } else {
                TerminalMouseEvent::WheelDown
            };
            let Some(report) = encode_mouse_event(
                self.mouse_modes(),
                event,
                TerminalMouseModifiers::default(),
                x as u16,
                y as u16,
            ) else {
                break;
            };
            out.extend_from_slice(&report);
        }
        out
    }

    /// Scrollback plus the visible screen as plain text, for search.
    ///
    /// Row indices are relative to the oldest line this emulator still
    /// retains. They slide once the history budget evicts, so a client caching
    /// deep scrollback across heavy output may refetch; the visible region and
    /// recent history are exact.
    pub fn scrollback(&mut self) -> diri_proto::ReadScrollbackResult {
        let history = self.term.grid().history_size();
        let rows = self.geometry.rows;
        let cols = self.geometry.cols;
        let total = history + rows;
        let mut lines = Vec::with_capacity(total);
        let mut text_cells = std::collections::BTreeMap::new();
        let mut ranges = Vec::with_capacity(cols);
        for index in 0..total {
            let line = Line(index as i32 - history as i32);
            let mut text = String::with_capacity(cols);
            ranges.clear();
            let source = &self.term.grid()[line];
            for x in 0..cols {
                let cell = &source[Column(x)];
                if cell
                    .flags
                    .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
                {
                    continue;
                }
                let c = cell.c;
                let width = if cell.flags.contains(Flags::WIDE_CHAR) {
                    2
                } else {
                    1
                };
                let range = [x as u16, (x + width).min(cols) as u16];
                text.push(if c < ' ' && c != '\t' { ' ' } else { c });
                ranges.push(range);
                if let Some(combining) = cell.zerowidth() {
                    for &ch in combining {
                        text.push(ch);
                        ranges.push(range);
                    }
                }
            }
            text.truncate(text.trim_end().len());
            ranges.truncate(text.chars().count());
            if ranges
                .iter()
                .enumerate()
                .any(|(i, range)| usize::from(range[0]) != i || usize::from(range[1]) != i + 1)
            {
                text_cells.insert(index, ranges.clone());
            }
            lines.push(text);
            self.finish_history_read_batch(index + 1, index + 1 == total);
        }
        diri_proto::ReadScrollbackResult {
            lines,
            text_cells,
            first_row: 0,
            visible_start_row: history as i64,
            cols: cols as i64,
            rows: rows as i64,
            content_seq: self.content_seq,
            is_alt_screen: self.is_alt_screen(),
        }
    }

    /// A window of scrollback rows as encoded cells, clamped to what exists.
    pub fn scrollback_cells(
        &mut self,
        first_row: i64,
        max_rows: i64,
    ) -> diri_proto::ReadScrollbackCellsResult {
        let history = self.term.grid().history_size();
        let cols = self.geometry.cols;
        let total = history + self.geometry.rows;

        let start = first_row.max(0).min(total as i64) as usize;
        let end = (start + max_rows.max(0) as usize).min(total);
        let mut rows = Vec::with_capacity(end.saturating_sub(start));
        let mut metadata = Vec::with_capacity(end.saturating_sub(start));
        for index in start..end {
            let line = Line(index as i32 - history as i32);
            let mut row = Vec::with_capacity(cols);
            let source = &self.term.grid()[line];
            for x in 0..cols {
                row.push(wire_cell(&source[Column(x)]));
            }
            rows.push(row);
            metadata.push(self.row_metadata_with_budget(
                line,
                (diri_proto::grid::MAX_GRID_METADATA_BYTES / 8) / (end - start).max(1),
            ));
            self.finish_history_read_batch(index - start + 1, index + 1 == end);
        }
        diri_proto::ReadScrollbackCellsResult {
            metadata,
            payload: GridRowCodec::encode_rows(&rows).unwrap_or_default(),
            first_row: start as i64,
            row_count: rows.len() as i64,
            total_rows: total as i64,
            live_start_row: history as i64,
            cols: cols as i64,
            content_seq: self.content_seq,
        }
    }

    /// Local retained Find capture. Unlike a viewport page, every included
    /// annotation must be complete; older rows may be omitted explicitly.
    pub fn find_capture_cells(
        &self,
    ) -> Result<diri_proto::ReadScrollbackCellsResult, &'static str> {
        let (cols, visible) = self.size();
        let grid = self.term.grid();
        let history = grid.history_size();
        let total = history + visible;
        let limit = (diri_proto::FIND_CAPTURE_MAX_CELLS / cols.max(1))
            .min(diri_proto::FIND_CAPTURE_MAX_ROWS)
            .min(total);
        if limit < visible {
            return Err("Terminal exceeds retained search limits");
        }
        let mut rows = Vec::with_capacity(limit);
        let mut metadata = Vec::with_capacity(limit);
        let mut remaining = diri_proto::grid::MAX_GRID_METADATA_BYTES / 4;
        // Walk newest-first, then reverse once: the admitted rows form one
        // contiguous immutable tail, with complete graphemes and link spans.
        for index in (total - limit..total).rev() {
            let line = Line(index as i32 - history as i32);
            let Some((annotations, bytes)) = self.row_metadata_budgeted(line, remaining, true)
            else {
                break;
            };
            remaining -= bytes;
            rows.push(
                (0..cols)
                    .map(|x| wire_cell(&grid[line][Column(x)]))
                    .collect(),
            );
            metadata.push(annotations);
        }
        if rows.len() < visible {
            return Err("Visible annotations exceed retained search limits");
        }
        rows.reverse();
        metadata.reverse();
        Ok(diri_proto::ReadScrollbackCellsResult {
            first_row: (total - rows.len()) as i64,
            row_count: rows.len() as i64,
            total_rows: total as i64,
            live_start_row: history as i64,
            cols: cols as i64,
            content_seq: self.content_seq,
            metadata,
            payload: GridRowCodec::encode_rows(&rows).map_err(|_| "Invalid capture cells")?,
        })
    }

    /// Export only bounded annotations. Oversized targets remain ordinary text.
    fn row_metadata(&self, line: Line) -> RowMetadata {
        self.row_metadata_with_budget(
            line,
            (diri_proto::grid::MAX_GRID_METADATA_BYTES / 8) / self.geometry.rows.max(1),
        )
    }

    fn row_metadata_with_budget(&self, line: Line, budget: usize) -> RowMetadata {
        self.row_metadata_budgeted(line, budget, false)
            .expect("lossy export always completes")
            .0
    }

    fn row_metadata_budgeted(
        &self,
        line: Line,
        budget: usize,
        complete: bool,
    ) -> Option<(RowMetadata, usize)> {
        let mut result = RowMetadata::default();
        let grid = self.term.grid();
        // Account for framing as well as text before allocating. Retained Find
        // uses complete mode; existing live/page exports keep their old limits.
        let mut used = 0;
        let source = &grid[line];
        for x in 0..self.geometry.cols {
            let cell = &source[Column(x)];
            if let Some(link) = cell.hyperlink() {
                let uri = link.uri();
                if let Some(last) = result.links.last_mut()
                    && last.end == x as u16
                    && last.uri == uri
                {
                    last.end += 1;
                } else if !uri.is_empty()
                    && uri.len() <= diri_proto::grid::MAX_LINK_URI_BYTES
                    && !uri.chars().any(char::is_control)
                {
                    let needed = uri.len() + 48;
                    if used + needed <= budget {
                        used += needed;
                        result.links.push(LinkSpan {
                            start: x as u16,
                            end: x as u16 + 1,
                            uri: uri.to_owned(),
                        });
                    } else if complete {
                        return None;
                    }
                }
            }
            if let Some(chars) = cell.zerowidth() {
                let text_len = chars
                    .iter()
                    .filter(|ch| !ch.is_control())
                    .map(|ch| ch.len_utf8())
                    .sum::<usize>();
                if text_len == 0 {
                    continue;
                }
                if text_len <= 64 && used + text_len + 16 <= budget {
                    used += text_len + 16;
                    let text = chars
                        .iter()
                        .copied()
                        .filter(|ch| !ch.is_control())
                        .collect();
                    result.graphemes.push((x as u16, text));
                } else if complete {
                    return None;
                }
            }
        }
        Some((result, used))
    }

    /// The visible grid as plain text, trailing blank lines removed.
    pub fn lines(&self) -> Vec<String> {
        let mut lines: Vec<String> = (0..self.geometry.rows)
            .map(|row| self.row_text(row))
            .collect();
        while lines.last().is_some_and(|line| line.trim().is_empty()) {
            lines.pop();
        }
        lines
    }

    /// One visible row as plain text, trailing blanks removed; empty past
    /// the bottom of the screen.
    pub fn row_text(&self, row: usize) -> String {
        if row >= self.geometry.rows {
            return String::new();
        }
        let source = &self.term.grid()[Line(row as i32)];
        let mut text = String::with_capacity(self.geometry.cols);
        for column in 0..self.geometry.cols {
            let cell = &source[Column(column)];
            // These occupy terminal columns but are not textual spaces.
            if cell
                .flags
                .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
            {
                continue;
            }
            text.push(cell.c);
            if let Some(combining) = cell.zerowidth() {
                text.extend(combining.iter().copied());
            }
        }
        text.trim_end().to_string()
    }

    /// What a link scanner needs from the screen and the newest `history_rows`
    /// of scrollback: text with soft-wrapped rows rejoined (a URL longer than
    /// the terminal is one logical line), and every OSC 8 hyperlink target,
    /// which is often the only place the URL of a `[title](url)` link exists.
    pub fn link_source(&self, history_rows: usize) -> LinkSource {
        let grid = self.term.grid();
        let history = grid.history_size().min(history_rows);
        let cols = self.geometry.cols;
        let mut source = LinkSource {
            text: String::with_capacity((history + self.geometry.rows) * (cols + 1)),
            hyperlinks: Vec::new(),
            cols,
        };
        let mut row_text = String::with_capacity(cols);
        for index in -(history as i32)..self.geometry.rows as i32 {
            let row = &grid[Line(index)];
            row_text.clear();
            let mut wrapped = false;
            for column in 0..cols {
                let cell = &row[Column(column)];
                if column + 1 == cols {
                    wrapped = cell.flags.contains(Flags::WRAPLINE);
                }
                if let Some(link) = cell.hyperlink() {
                    let uri = link.uri();
                    if source.hyperlinks.last().is_none_or(|(_, last)| last != uri)
                        && uri.len() <= diri_proto::grid::MAX_LINK_URI_BYTES
                    {
                        let at = source.text.len() + row_text.len();
                        source.hyperlinks.push((at, uri.to_owned()));
                    }
                }
                if cell
                    .flags
                    .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
                {
                    continue;
                }
                row_text.push(cell.c);
                if let Some(combining) = cell.zerowidth() {
                    row_text.extend(combining.iter().copied());
                }
            }
            if wrapped {
                source.text.push_str(&row_text);
            } else {
                source.text.push_str(row_text.trim_end());
                source.text.push('\n');
            }
        }
        source
    }

    /// A snapshot for the detection engine.
    pub fn snapshot(&self) -> ScreenSnapshot {
        ScreenSnapshot {
            lines: self.lines(),
            osc_title: self.title.clone(),
            osc_progress_state: self.progress_state,
            content_seq: self.content_seq,
        }
    }

    pub fn progress(&self) -> Option<(i64, i64)> {
        Some((self.progress_state?, self.progress_value.unwrap_or(0)))
    }

    /// Counts valid `OSC 9;4` reports, including repeats of the same value,
    /// so a consumer can tell a program still reporting from one that went
    /// quiet without clearing its progress. Wraps.
    pub fn progress_reports(&self) -> u64 {
        self.progress_reports
    }

    fn drain_events(&mut self) {
        while let Ok(event) = self.events.try_recv() {
            match event {
                Event::Title(title) => {
                    let title = Some(title);
                    self.title_changed |= self.title != title;
                    self.title = title;
                }
                Event::ResetTitle => {
                    self.title_changed |= self.title.is_some();
                    self.title = None;
                }
                // Replies to queries the child made — CSI 6n, device
                // attributes, and so on. Collected here and written back by
                // the pump, which owns the write side of the PTY.
                Event::PtyWrite(text) => self.replies.extend_from_slice(text.as_bytes()),
                Event::TextAreaSizeRequest(format) => {
                    // Cell pixel size is a rendering concern the daemon does
                    // not know; report the grid it does know. Callers use this
                    // to size output, and cols/rows is the part they act on.
                    let size = WindowSize {
                        num_cols: self.geometry.cols as u16,
                        num_lines: self.geometry.rows as u16,
                        cell_width: 0,
                        cell_height: 0,
                    };
                    self.replies.extend_from_slice(format(size).as_bytes());
                }
                _ => {}
            }
        }
    }

    /// Takes the answers owed to the child. The pump writes these to the PTY
    /// after feeding a chunk; nothing else may consume them.
    #[must_use]
    pub fn take_replies(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.replies)
    }

    // (cell mapping lives at module level; see `wire_cell`)

    fn fingerprint_row(&self, row: usize) -> (u64, usize) {
        // A fast multiply-fold hash is sufficient for change detection. Grid
        // publication still compares the actual cells, so this fingerprint
        // never decides wire correctness. It hashes the wire projection of
        // each cell (glyph, zero-width marks, link, style bits and both
        // colors): Cursor paints its composer caret as inverse video, and
        // Claude Code moves a menu highlight or a mouse selection by
        // recoloring cells, all without changing glyphs. Those frames must
        // advance `content_seq` or the attach pump suppresses them, while a
        // change that is invisible on the wire must not.
        //
        // Four independent lanes keep the multiplies from forming one serial
        // dependency chain across the row.
        #[cfg(test)]
        self.fingerprinted_rows
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let grid = self.term.grid();
        let source = &grid[Line(row as i32)];
        let cells = &source[..];
        let cells = &cells[..self.geometry.cols.min(cells.len())];
        let mut state = RowHashState::default();
        let mut lanes = [
            0xcbf2_9ce4_8422_2325u64,
            0x8422_2325_cbf2_9ce4,
            0x243f_6a88_85a3_08d3,
            0x1319_8a2e_0370_7344,
        ];
        let mut chunks = cells.chunks_exact(4);
        let mut column = 0;
        for chunk in &mut chunks {
            let words = [
                state.word(column, &chunk[0]),
                state.word(column + 1, &chunk[1]),
                state.word(column + 2, &chunk[2]),
                state.word(column + 3, &chunk[3]),
            ];
            lanes[0] = fold_mix(lanes[0], words[0]);
            lanes[1] = fold_mix(lanes[1], words[1]);
            lanes[2] = fold_mix(lanes[2], words[2]);
            lanes[3] = fold_mix(lanes[3], words[3]);
            column += 4;
        }
        for (offset, cell) in chunks.remainder().iter().enumerate() {
            lanes[offset] = fold_mix(lanes[offset], state.word(column + offset, cell));
        }
        let mut digest = fold_mix(state.marks, cells.len() as u64);
        for lane in lanes {
            digest = fold_mix(digest, lane);
        }
        (digest, state.filled)
    }

    fn rebuild_content_cache(&mut self) {
        self.stale_fingerprints.clear();
        self.stale_fingerprints.resize(self.geometry.rows, false);
        self.row_digests.clear();
        self.row_filled_cells.clear();
        self.row_digests.reserve(self.geometry.rows);
        self.row_filled_cells.reserve(self.geometry.rows);
        let mut total_filled = 0;
        for row in 0..self.geometry.rows {
            let (digest, filled) = self.fingerprint_row(row);
            self.row_digests.push(digest);
            self.row_filled_cells.push(filled);
            total_filled += filled;
        }
        self.filled_cells = total_filled;
    }

    /// Extracts `ESC ] 9 ; 4 ; state ; value` progress reports.
    ///
    /// Agents use this to say "I am working, 40% through"; the emulator has no
    /// concept of it and would silently drop the sequence.
    fn scan_progress(&mut self, bytes: &[u8]) {
        if self.progress_carry.is_empty() {
            if memchr::memchr(0x1b, bytes).is_none() {
                return;
            }
            // Scan the chunk where it lies. Joining it to an empty carry would
            // copy every byte of every read, and reads that leave a carry
            // behind are the rare case — the common one is output that merely
            // contains color escapes.
            if let Some(start) = self.scan_progress_within(bytes) {
                self.progress_carry.extend_from_slice(&bytes[start..]);
            }
            return;
        }
        let mut haystack = std::mem::take(&mut self.progress_carry);
        haystack.extend_from_slice(bytes);
        if let Some(start) = self.scan_progress_within(&haystack) {
            haystack.drain(..start);
            self.progress_carry = haystack;
        }
    }

    /// Applies one `state;percent` payload. A state outside ConEmu's 0–4 or a
    /// payload that is not two small numbers is ignored whole, so junk cannot
    /// clear or invent progress. The percent is clamped to 0–100; an error or
    /// pause without one keeps the percent it interrupts.
    fn take_progress_report(&mut self, payload: &[u8]) {
        let Ok(payload) = std::str::from_utf8(payload) else {
            return;
        };
        let mut parts = payload.split(';');
        let Some(Ok(state)) = parts.next().map(|value| value.trim().parse::<u8>()) else {
            return;
        };
        if state > 4 {
            return;
        }
        let value = match parts.next().map(str::trim) {
            None | Some("") => None,
            Some(value) => match value.parse::<u32>() {
                Ok(value) => Some(i64::from(value.min(100))),
                Err(_) => return,
            },
        };
        if parts.next().is_some() {
            return;
        }
        self.progress_value = match state {
            1 => Some(value.unwrap_or(0)),
            2 | 4 => value.or(self
                .progress_value
                .filter(|_| self.progress_state.is_some())),
            _ => None,
        };
        self.progress_state = Some(i64::from(state));
        self.progress_reports = self.progress_reports.wrapping_add(1);
    }

    /// Scans one buffer for progress reports, returning where a carry for the
    /// next chunk should begin if the buffer ends mid-sequence.
    fn scan_progress_within(&mut self, haystack: &[u8]) -> Option<usize> {
        const PREFIX: &[u8] = b"\x1b]9;4;";
        const MAX_INCOMPLETE_BYTES: usize = 64;
        let mut search_from = 0;
        let mut incomplete = None;
        while let Some(found) = find(&haystack[search_from..], PREFIX) {
            let prefix_start = search_from + found;
            let start = prefix_start + PREFIX.len();
            // Terminated by BEL or ST (ESC \).
            let Some(end) = haystack[start..]
                .iter()
                .position(|&b| b == 0x07 || b == 0x1b)
                .map(|offset| start + offset)
            else {
                // Truncated: keep it for the next chunk.
                incomplete = Some(prefix_start);
                break;
            };
            self.take_progress_report(&haystack[start..end]);
            search_from = end;
        }

        if let Some(start) = incomplete
            && haystack.len() - start <= MAX_INCOMPLETE_BYTES
        {
            return Some(start);
        }
        // A malformed unterminated OSC must not turn the scanner into an
        // unbounded copy of the PTY stream. Fall through and retain only a
        // possible prefix suffix.
        (1..PREFIX.len())
            .rev()
            .find(|length| haystack.ends_with(&PREFIX[..*length]))
            .map(|length| haystack.len() - length)
    }
}

/// Maps an alacritty cell to the wire cell the client renders.
///
/// The existing Rust wire vocabulary uses `Default`/`DefaultInverted` for the
/// two default colors, ANSI indices for 0–255, and stable protocol style bits.
fn wire_cell(cell: &alacritty_terminal::term::cell::Cell) -> GridCell {
    let scalar = if cell
        .flags
        .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
    {
        0
    } else if cell.c == '\0' {
        32
    } else {
        cell.c as u32
    };
    GridCell::new(
        scalar,
        wire_color(cell.fg),
        wire_color(cell.bg),
        wire_style(cell.flags),
    )
}

fn wire_color(color: Color) -> TermColor {
    match color {
        Color::Named(NamedColor::Foreground) => TermColor::Default,
        Color::Named(NamedColor::Background) => TermColor::DefaultInverted,
        Color::Named(named) => {
            let index = named as usize;
            if index < 16 {
                TermColor::Ansi(index as u8)
            } else if (NamedColor::DimBlack as usize..=NamedColor::DimWhite as usize)
                .contains(&index)
            {
                // Dim variants render as their base color; DIM rides the style.
                TermColor::Ansi((index - NamedColor::DimBlack as usize) as u8)
            } else {
                TermColor::Default
            }
        }
        Color::Indexed(index) => TermColor::Ansi(index),
        Color::Spec(rgb) => TermColor::Rgb(rgb.r, rgb.g, rgb.b),
    }
}

/// Folded 64×64→128-bit multiply: one step of a fast non-cryptographic hash.
#[inline]
fn fold_mix(state: u64, word: u64) -> u64 {
    let product = u128::from(state ^ word) * u128::from(0x9e37_79b9_7f4a_7c15u64);
    (product as u64) ^ (product >> 64) as u64
}

/// Per-row fingerprint state outside the four multiply lanes.
struct RowHashState {
    filled: usize,
    /// Links and zero-width marks are rare; they fold into their own lane.
    marks: u64,
    previous_link: Option<alacritty_terminal::term::cell::Hyperlink>,
    /// The wire style is derived only when the raw style changes, which
    /// consecutive cells rarely do.
    raw_style: u128,
    style_hash: u64,
}

impl Default for RowHashState {
    fn default() -> Self {
        Self {
            filled: 0,
            marks: 0x0a40_9382_2299_f31d,
            previous_link: None,
            raw_style: u128::MAX,
            style_hash: 0,
        }
    }
}

impl RowHashState {
    #[inline(always)]
    fn word(&mut self, column: usize, cell: &Cell) -> u64 {
        if cell.extra.is_some() {
            self.marks(column, cell);
        } else if self.previous_link.is_some() {
            self.marks = fold_mix(self.marks, column as u64 | 1 << 63);
            self.previous_link = None;
        }
        let raw = raw_style_key(cell);
        if raw != self.raw_style {
            self.raw_style = raw;
            self.style_hash = style_word(cell.flags, cell.fg, cell.bg);
        }
        let character = cell.c;
        self.filled += usize::from(character != ' ' && character != '\0');
        u64::from(character) ^ self.style_hash
    }

    #[inline(never)]
    fn marks(&mut self, column: usize, cell: &Cell) {
        let link = cell.hyperlink();
        if link != self.previous_link {
            let uri = link.as_ref().map_or(&b""[..], |link| link.uri().as_bytes());
            for chunk in uri.chunks(8) {
                let mut bytes = [0u8; 8];
                bytes[..chunk.len()].copy_from_slice(chunk);
                self.marks = fold_mix(self.marks, u64::from_le_bytes(bytes));
            }
            self.marks = fold_mix(self.marks, column as u64 | 1 << 63);
            self.previous_link = link;
        }
        if let Some(chars) = cell.zerowidth() {
            for ch in chars {
                self.marks = fold_mix(self.marks, u64::from(*ch) | (column as u64) << 32);
            }
        }
    }
}

/// Exact raw style identity (both colors and all flags), cheap to compare.
#[inline]
fn raw_style_key(cell: &Cell) -> u128 {
    #[inline]
    fn color(value: Color) -> u128 {
        match value {
            Color::Named(value) => value as u128,
            Color::Spec(rgb) => {
                1 << 24 | u128::from(rgb.r) << 16 | u128::from(rgb.g) << 8 | u128::from(rgb.b)
            }
            Color::Indexed(index) => 2 << 24 | u128::from(index),
        }
    }
    color(cell.fg) | color(cell.bg) << 32 | u128::from(cell.flags.bits()) << 64
}

/// Hash of a cell's wire style bits and both wire colors.
#[inline]
fn style_word(flags: Flags, fg: Color, bg: Color) -> u64 {
    let colors = u64::from(wire_color(fg).packed()) | u64::from(wire_color(bg).packed()) << 32;
    fold_mix(
        fold_mix(0x243f_6a88_85a3_08d3, colors),
        u64::from(wire_style(flags).bits()),
    )
}

fn wire_style(flags: Flags) -> TermStyle {
    let mut style = TermStyle::empty();
    if flags.contains(Flags::WRAPLINE) {
        style |= TermStyle::SOFT_WRAP;
    }
    if flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER) {
        style |= TermStyle::WIDE_SPACER;
    }
    if flags.contains(Flags::PROMPT_START) {
        style |= TermStyle::PROMPT_START;
    }
    if flags.intersects(Flags::BOLD | Flags::DIM_BOLD) {
        style |= TermStyle::BOLD;
    }
    if flags.intersects(Flags::ALL_UNDERLINES) {
        style |= TermStyle::UNDERLINE;
    }
    if flags.contains(Flags::INVERSE) {
        style |= TermStyle::INVERSE;
    }
    if flags.contains(Flags::HIDDEN) {
        style |= TermStyle::INVISIBLE;
    }
    if flags.intersects(Flags::DIM | Flags::DIM_BOLD) {
        style |= TermStyle::DIM;
    }
    if flags.contains(Flags::ITALIC) {
        style |= TermStyle::ITALIC;
    }
    if flags.contains(Flags::STRIKEOUT) {
        style |= TermStyle::CROSSED_OUT;
    }
    style
}

/// The SGR sequence that reproduces a cell's attributes.
fn sgr(cell: &GridCell) -> String {
    let mut codes: Vec<String> = vec!["0".into()];
    if cell.style.contains(TermStyle::BOLD) {
        codes.push("1".into());
    }
    if cell.style.contains(TermStyle::DIM) {
        codes.push("2".into());
    }
    if cell.style.contains(TermStyle::ITALIC) {
        codes.push("3".into());
    }
    if cell.style.contains(TermStyle::UNDERLINE) {
        codes.push("4".into());
    }
    if cell.style.contains(TermStyle::BLINK) {
        codes.push("5".into());
    }
    if cell.style.contains(TermStyle::INVERSE) {
        codes.push("7".into());
    }
    if cell.style.contains(TermStyle::INVISIBLE) {
        codes.push("8".into());
    }
    if cell.style.contains(TermStyle::CROSSED_OUT) {
        codes.push("9".into());
    }
    append_color(cell.fg, true, &mut codes);
    append_color(cell.bg, false, &mut codes);
    format!("\x1b[{}m", codes.join(";"))
}

fn append_color(color: TermColor, foreground: bool, codes: &mut Vec<String>) {
    match color {
        TermColor::Default | TermColor::DefaultInverted => {
            codes.push(if foreground { "39" } else { "49" }.into());
        }
        TermColor::Ansi(value) => {
            codes.push(if foreground { "38" } else { "48" }.into());
            codes.push("5".into());
            codes.push(value.to_string());
        }
        TermColor::Rgb(red, green, blue) => {
            codes.push(if foreground { "38" } else { "48" }.into());
            codes.push("2".into());
            codes.push(red.to_string());
            codes.push(green.to_string());
            codes.push(blue.to_string());
        }
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return None;
    }
    memchr::memmem::find(haystack, needle)
}

fn restore_semantic_flags(target: &mut Cell, source: GridCell) {
    if source.style.contains(TermStyle::SOFT_WRAP) {
        target.flags.insert(Flags::WRAPLINE);
    }
    if source.style.contains(TermStyle::PROMPT_START) {
        target.flags.insert(Flags::PROMPT_START);
    }
    if source.style.contains(TermStyle::WIDE_SPACER) {
        target.flags.insert(Flags::WIDE_CHAR_SPACER);
    }
}

fn emulator_cell(cell: GridCell) -> Cell {
    let mut flags = Flags::empty();
    if cell.style.contains(TermStyle::BOLD) {
        flags.insert(Flags::BOLD);
    }
    if cell.style.contains(TermStyle::UNDERLINE) {
        flags.insert(Flags::UNDERLINE);
    }
    if cell.style.contains(TermStyle::INVERSE) {
        flags.insert(Flags::INVERSE);
    }
    if cell.style.contains(TermStyle::INVISIBLE) {
        flags.insert(Flags::HIDDEN);
    }
    if cell.style.contains(TermStyle::DIM) {
        flags.insert(Flags::DIM);
    }
    if cell.style.contains(TermStyle::ITALIC) {
        flags.insert(Flags::ITALIC);
    }
    if cell.style.contains(TermStyle::CROSSED_OUT) {
        flags.insert(Flags::STRIKEOUT);
    }
    let mut result = Cell {
        c: char::from_u32(cell.scalar)
            .filter(|_| cell.scalar != 0)
            .unwrap_or(' '),
        fg: emulator_color(cell.fg),
        bg: emulator_color(cell.bg),
        flags,
        extra: None,
    };
    restore_semantic_flags(&mut result, cell);
    result
}

fn emulator_color(color: TermColor) -> Color {
    match color {
        TermColor::Default => Color::Named(NamedColor::Foreground),
        TermColor::DefaultInverted => Color::Named(NamedColor::Background),
        TermColor::Ansi(index) => Color::Indexed(index),
        TermColor::Rgb(r, g, b) => Color::Spec(Rgb { r, g, b }),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn keyboard_mode_only_changes_leave_cells_unchanged_and_reset_independently() {
        let mut screen = super::HeadlessScreen::new(20, 3);
        screen.feed(b"unchanged");
        let cells = screen.lines();
        assert_eq!(
            screen.keyboard_state(),
            diri_proto::terminal_input::KeyboardState {
                enhancements: Some(0.try_into().unwrap()),
                ..Default::default()
            }
        );
        screen.feed(b"\x1b[?1h\x1b=");
        assert_eq!(screen.lines(), cells);
        assert!(screen.keyboard_state().application_cursor_keys);
        assert!(screen.keyboard_state().application_keypad);
        screen.feed(b"\x1b[?1l");
        assert!(!screen.keyboard_state().application_cursor_keys);
        assert!(screen.keyboard_state().application_keypad);
        screen.feed(b"\x1b>");
        assert_eq!(
            screen.keyboard_state(),
            diri_proto::terminal_input::KeyboardState {
                enhancements: Some(0.try_into().unwrap()),
                ..Default::default()
            }
        );
    }

    use super::*;

    impl HeadlessScreen {
        /// Every kept row fingerprint and fill count equals a fresh walk.
        fn assert_fingerprints_current(&self, step: usize) {
            let mut filled = 0;
            for row in 0..self.geometry.rows {
                let (digest, row_filled) = self.fingerprint_row(row);
                assert_eq!(self.row_digests[row], digest, "row {row} at step {step}");
                assert_eq!(
                    self.row_filled_cells[row], row_filled,
                    "row {row} at {step}"
                );
                filled += row_filled;
            }
            assert_eq!(self.filled_cells, filled, "filled cells at step {step}");
        }

        fn take_fingerprinted_rows(&self) -> usize {
            self.fingerprinted_rows
                .swap(0, std::sync::atomic::Ordering::Relaxed)
        }
    }

    #[test]
    fn scrolling_fingerprints_only_rows_that_changed() {
        let mut screen = HeadlessScreen::new(40, 10);
        for line in 0..10 {
            screen.feed(format!("line {line}\r\n").as_bytes());
        }
        screen.take_fingerprinted_rows();
        let seq = screen.content_seq();
        // One scrolled line: the old cursor row and the new bottom row.
        screen.feed(b"next\r\n");
        assert_eq!(screen.take_fingerprinted_rows(), 2);
        assert_eq!(screen.content_seq(), seq + 1);
        screen.assert_fingerprints_current(0);
        screen.take_fingerprinted_rows();
        // A region scroll leaves rows outside the region alone and keeps
        // the moved rows' fingerprints; the cursor rows are damaged.
        screen.feed(b"\x1b[3;6r\x1b[2S\x1b[r\x1b[10;1H");
        assert_eq!(screen.take_fingerprinted_rows(), 4);
        screen.assert_fingerprints_current(1);
        screen.take_fingerprinted_rows();
        // Non-scroll full damage fingerprints every row.
        screen.feed(b"\x1b[2J");
        assert_eq!(screen.take_fingerprinted_rows(), 10);
        screen.assert_fingerprints_current(2);
    }

    fn assert_same_fingerprints(screen: &HeadlessScreen, reference: &HeadlessScreen, step: usize) {
        assert_eq!(
            screen.row_digests, reference.row_digests,
            "digests at step {step}"
        );
        assert_eq!(
            screen.row_filled_cells, reference.row_filled_cells,
            "filled at {step}"
        );
        assert_eq!(
            screen.filled_cells, reference.filled_cells,
            "filled cells at {step}"
        );
        assert_eq!(
            screen.content_seq, reference.content_seq,
            "content_seq at {step}"
        );
    }

    /// Reusing moved fingerprints is exact: after every read, fingerprints,
    /// fill counts and `content_seq` equal hashing every row after full
    /// damage, which status detection and publication were built on.
    #[test]
    fn fingerprints_after_scrolling_match_the_cells() {
        let actions: &[&str] = &[
            "\x1b[31;1m",
            "\x1b[38;2;1;2;3;48;5;17m",
            "\x1b[0m",
            "\x1b[7m",
            "\x1b[44m\x1b[K\x1b[0m",
            "\x1b[42m\x1b[2K\r\n",
            "\x1b]8;id=a;https://example.invalid/a\x07",
            "\x1b]8;;\x07",
            "\x1b]133;A\x07$ ",
            "\x1b[2;6r",
            "\x1b[r",
            "\x1b[3S",
            "\x1b[2T",
            "\x1b[2L",
            "\x1b[1M",
            "\x1b[3@",
            "\x1b[2P",
            "\x1b[4X",
            "\x1b[2J",
            "\x1b[H",
            "\x1b[5;7H",
            "\x1b[99;99H",
            "\x1b[4h",
            "\x1b[4l",
            "\x1b[?7l",
            "\x1b[?7h",
            "\x1b[?1049h",
            "\x1b[?1049l",
            "\x1b[?2026h",
            "\x1b[?2026l",
            "\x1b7",
            "\x1b8",
            "\x1bD",
            "\x1bM",
            "\x1bE",
            "\n\n\n",
            "\t",
            "\x08",
            "界面 e\u{301} 🦀",
            "abc\x1b[3b",
        ];
        let mut screen = HeadlessScreen::new(20, 6);
        let mut reference = HeadlessScreen::new(20, 6);
        reference.reuse_moved_fingerprints = false;
        let mut seed = 0x243f_6a88_85a3_08d3u64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let mut reused = 0;
        for step in 0..20_000 {
            let random = next();
            // Several pieces per read, so writes follow scrolls unsettled.
            let mut bytes = String::new();
            for piece in 0..1 + random % 4 {
                let random = next();
                bytes += &match random % 6 {
                    0 | 1 => format!("{step}.{piece} {}", "x".repeat((random >> 8) as usize % 30)),
                    2 => "\r\n".repeat(1 + (random >> 8) as usize % 4),
                    _ => actions[(random >> 16) as usize % actions.len()].to_string(),
                };
            }
            screen.take_fingerprinted_rows();
            screen.feed(bytes.as_bytes());
            reused += screen.geometry.rows - screen.take_fingerprinted_rows();
            reference.feed(bytes.as_bytes());
            assert_same_fingerprints(&screen, &reference, step);
            if random % 997 == 0 {
                let (cols, rows) = (
                    10 + (random >> 24) as usize % 30,
                    1 + (random >> 32) as usize % 8,
                );
                screen.resize(cols, rows);
                reference.resize(cols, rows);
                assert_same_fingerprints(&screen, &reference, step);
            }
        }
        assert!(reused > 20_000, "{reused} fingerprints reused");
    }

    fn screen_with(input: &[u8]) -> HeadlessScreen {
        let mut screen = HeadlessScreen::new(80, 24);
        screen.feed(input);
        screen
    }

    #[test]
    fn enabled_parser_old_visible_cache_keeps_enhancements_unknown() {
        let mut original = HeadlessScreen::new_with_keyboard_enhancements(20, 3);
        original.feed(b"\x1b[>5u");
        let grid = original.full_snapshot();
        let complete = original.keyboard_snapshot().unwrap();
        let mut restored = HeadlessScreen::new_with_keyboard_enhancements(20, 3);
        assert!(restored.restore(&[], &grid, false, false, MouseModes::OFF));
        restored.restore_keyboard_state(Default::default());
        assert_eq!(restored.keyboard_state().enhancements, None);
        assert_eq!(restored.input_keyboard_state(), None);
        restored.feed(b"\x1b[<u");
        assert_eq!(restored.keyboard_state().enhancements, None);
        assert!(restored.restore_keyboard_snapshot(&complete));
        assert_eq!(restored.keyboard_state().enhancements.unwrap().bits(), 5);
        restored.feed(b"\x1b[<u");
        assert_eq!(restored.keyboard_state().enhancements.unwrap().bits(), 0);
    }

    #[test]
    fn keyboard_cache_knownness_does_not_enable_negotiation() {
        let mut screen = HeadlessScreen::new(20, 3);
        let snapshot = screen.keyboard_snapshot().unwrap();
        assert_eq!(screen.keyboard_state().enhancements.unwrap().bits(), 0);
        screen.restore_keyboard_state(Default::default());
        assert_eq!(screen.keyboard_state().enhancements, None);
        assert_eq!(screen.keyboard_snapshot(), None);
        screen.feed(b"\x1b[=31u\x1b[?u");
        assert!(screen.take_replies().is_empty());
        assert!(screen.restore_keyboard_snapshot(&snapshot));
        assert_eq!(screen.keyboard_state().enhancements.unwrap().bits(), 0);
        let active = KeyboardSnapshot::decode(&[1, 1, 1, 0, 0, 0, 1]).unwrap();
        assert!(!screen.restore_keyboard_snapshot(&active));
        let inactive = KeyboardSnapshot::decode(&[1, 0, 0, 0, 1, 0, 1]).unwrap();
        assert!(!screen.restore_keyboard_snapshot(&inactive));
        assert_eq!(screen.keyboard_state().enhancements.unwrap().bits(), 0);
    }

    #[test]
    fn plain_output_lands_on_the_grid() {
        let screen = screen_with(b"hello world\r\nsecond line\r\n");
        assert_eq!(screen.lines(), vec!["hello world", "second line"]);
    }

    #[test]
    fn an_erase_actually_erases() {
        // The whole point of emulating rather than grepping the byte stream:
        // text that was overwritten must not still read as present.
        let screen = screen_with(b"do you want to proceed?\r\n\x1b[2J\x1b[Hall clear\r\n");
        let text = screen.lines().join("\n");
        assert!(
            !text.contains("proceed"),
            "erased text still visible: {text:?}"
        );
        assert!(text.contains("all clear"));
    }

    #[test]
    fn cursor_movement_overwrites_in_place() {
        let screen = screen_with(b"aaaa\rbb");
        assert_eq!(screen.lines(), vec!["bbaa"]);
    }

    #[test]
    fn an_osc_title_is_captured() {
        let screen = screen_with(b"\x1b]0;my-session\x07ready\r\n");
        assert_eq!(screen.title(), Some("my-session"));
    }

    #[test]
    fn osc_9_4_progress_is_scanned_out_of_the_stream() {
        // alacritty does not model this ConEmu extension, so the engine parses
        // it directly. State 1, 40 percent.
        let screen = screen_with(b"\x1b]9;4;1;40\x07working\r\n");
        assert_eq!(screen.progress(), Some((1, 40)));
        assert_eq!(screen.snapshot().osc_progress_state, Some(1));
    }

    #[test]
    fn a_progress_sequence_split_across_reads_is_still_found() {
        let mut screen = HeadlessScreen::new(80, 24);
        screen.feed(b"\x1b]9;4;1");
        screen.feed(b";75\x07");
        assert_eq!(screen.progress(), Some((1, 75)));
    }

    #[test]
    fn progress_states_percent_and_junk() {
        let mut screen = HeadlessScreen::new(80, 24);
        // ST-terminated, whitespace tolerated, percent clamped to 100.
        screen.feed(b"\x1b]9;4;1; 250 \x1b\\");
        assert_eq!(screen.progress(), Some((1, 100)));
        // An error or pause without a percent stops where the bar was.
        screen.feed(b"\x1b]9;4;1;35\x07\x1b]9;4;2\x07");
        assert_eq!(screen.progress(), Some((2, 35)));
        screen.feed(b"\x1b]9;4;4;\x07");
        assert_eq!(screen.progress(), Some((4, 35)));
        // Indeterminate carries no percent; 0 clears.
        screen.feed(b"\x1b]9;4;3;90\x07");
        assert_eq!(screen.progress(), Some((3, 0)));
        screen.feed(b"\x1b]9;4;0\x07");
        assert_eq!(screen.progress(), Some((0, 0)));
        screen.feed(b"\x1b]9;4;1\x07");
        assert_eq!(screen.progress(), Some((1, 0)));

        let reports = screen.progress_reports();
        for junk in [
            &b"\x1b]9;4;5;10\x07"[..],
            b"\x1b]9;4;x;10\x07",
            b"\x1b]9;4;1;-4\x07",
            b"\x1b]9;4;1;12.5\x07",
            b"\x1b]9;4;1;10;extra\x07",
            b"\x1b]9;4;\x07",
            b"\x1b]9;4;1;99999999999999999999\x07",
        ] {
            screen.feed(junk);
            assert_eq!(screen.progress(), Some((1, 0)), "{junk:?}");
        }
        assert_eq!(screen.progress_reports(), reports, "junk is not a report");
        // A repeat of the same value still counts as a report.
        screen.feed(b"\x1b]9;4;1\x07");
        assert_eq!(screen.progress_reports(), reports + 1);
    }

    #[test]
    fn progress_and_osc_9_notifications_never_cross() {
        let mut screen = HeadlessScreen::new(80, 24).with_notifications();
        screen.feed(b"\x1b]9;4;1;40\x07");
        assert!(!screen.has_notifications());
        assert_eq!(screen.progress(), Some((1, 40)));
        // `9;` followed by anything but `4;` is a notification, even one
        // whose text starts with a 4.
        screen.feed(b"\x1b]9;42 tests passed\x07\x1b]9;4\x07");
        let bodies: Vec<_> = screen
            .take_notifications()
            .into_iter()
            .map(|message| message.body)
            .collect();
        assert_eq!(bodies, ["42 tests passed", "4"]);
        assert_eq!(screen.progress(), Some((1, 40)));
        // Progress is parsed on screens that never opted into notifications
        // (the remote Holder), and drawn nowhere.
        let mut holder = HeadlessScreen::new(20, 2);
        holder.feed(b"a\x1b]9;4;1;7\x07b");
        assert_eq!(holder.progress(), Some((1, 7)));
        assert!(holder.lines()[0].starts_with("ab"));
    }

    #[test]
    fn an_unterminated_progress_sequence_has_bounded_carry() {
        let mut screen = HeadlessScreen::new(80, 24);
        let mut input = b"\x1b]9;4;1;".to_vec();
        input.extend(std::iter::repeat_n(b'9', 16 << 10));
        screen.feed(&input);
        assert!(screen.progress_carry.len() < 8);
    }

    #[test]
    fn a_synchronized_repaint_never_shows_its_erased_half() {
        // What DECSET 2026 exists for: the child erases and redraws inside the
        // bracket, and no observer may see the gap between the two.
        let mut screen = screen_with(b"before the repaint\r\n");
        let quiet = screen.content_seq();

        screen.feed(b"\x1b[?2026h\x1b[2J\x1b[H");
        assert_eq!(
            screen.lines(),
            vec!["before the repaint"],
            "the erase is held back until the update closes"
        );
        assert_eq!(screen.content_seq(), quiet);

        screen.feed(b"after the repaint\r\n\x1b[?2026l");
        assert_eq!(screen.lines(), vec!["after the repaint"]);
        assert!(screen.content_seq() > quiet);
    }

    #[test]
    fn a_synchronized_update_the_child_never_closes_is_released() {
        // vte records the 150ms deadline but nothing expires it, so without a
        // host-side flush an abandoned bracket freezes the pane until 2 MiB of
        // output has piled up behind it.
        let mut screen = screen_with(b"before the repaint\r\n");
        screen.feed(b"\x1b[?2026h\x1b[2J\x1b[Hafter the repaint\r\n");
        assert!(!screen.flush_expired_sync(), "the deadline has not passed");
        assert_eq!(screen.lines(), vec!["before the repaint"]);

        std::thread::sleep(std::time::Duration::from_millis(160));
        assert!(screen.flush_expired_sync(), "an overdue update is released");
        assert_eq!(screen.lines(), vec!["after the repaint"]);
        assert!(
            !screen.flush_expired_sync(),
            "releasing it once clears the deadline"
        );
    }

    #[test]
    fn link_source_rejoins_wrapped_rows_and_keeps_hyperlinks_and_history() {
        let mut screen = HeadlessScreen::new(20, 4);
        screen.feed(b"first https://a.dev/0123456789abcdef end\r\n");
        screen.feed(b"\x1b]8;;https://github.com/o/r/pull/9\x1b\\PR #9\x1b]8;;\x1b\\\r\n");
        for n in 0..6 {
            screen.feed(format!("line {n}\r\n").as_bytes());
        }
        // Both links have scrolled off the 4-row screen.
        let without_history = screen.link_source(0);
        assert!(!without_history.text.contains("a.dev"));
        assert!(without_history.hyperlinks.is_empty());

        let source = screen.link_source(100);
        assert_eq!(source.cols, 20);
        assert!(
            source
                .text
                .contains("first https://a.dev/0123456789abcdef end\n"),
            "{:?}",
            source.text
        );
        assert!(source.text.contains("PR #9\n"));
        assert_eq!(source.hyperlinks.len(), 1);
        let (at, uri) = &source.hyperlinks[0];
        assert_eq!(uri, "https://github.com/o/r/pull/9");
        assert!(source.text[*at..].starts_with("PR #9"));
    }

    #[test]
    fn content_seq_advances_only_when_the_screen_changes() {
        let mut screen = HeadlessScreen::new(80, 24);
        screen.feed(b"hello\r\n");
        let after_first = screen.content_seq();
        assert!(after_first > 0);

        // A no-op sequence paints nothing.
        screen.feed(b"\x1b[?25l");
        assert_eq!(
            screen.content_seq(),
            after_first,
            "an invisible change must not look like new content"
        );

        screen.feed(b"world\r\n");
        assert!(screen.content_seq() > after_first);
    }

    #[test]
    fn a_style_only_repaint_advances_content_seq() {
        // Cursor hides the hardware cursor and paints its composer caret as
        // inverse video. Arrows and space then restyle cells without changing
        // glyphs; those frames must still look like new content or the attach
        // pump suppresses them.
        let mut screen = HeadlessScreen::new(80, 24);
        screen.feed(b"hello");
        let after_text = screen.content_seq();
        let _ = screen.grid_update(true);

        screen.feed(b"\r\x1b[7mh\x1b[27m");
        assert!(
            screen.content_seq() > after_text,
            "a style-only caret move must look like new content"
        );
        assert_eq!(screen.lines(), vec!["hello"]);

        let update = screen.grid_update(false);
        let first = &update.changed_rows[0].cells[0];
        assert!(
            first.style.contains(TermStyle::INVERSE),
            "the restyled cell must reach the wire"
        );
    }

    #[test]
    fn a_color_only_repaint_advances_content_seq() {
        // Claude Code moves its `/` menu highlight by recoloring the same
        // glyphs, and paints a mouse selection as a background change. Neither
        // touches a glyph, a style bit or the parked cursor, so `content_seq`
        // is the only thing that can tell the attach pump a frame is owed.
        let mut screen = HeadlessScreen::new(80, 24);
        screen.feed(b"/help\r\n/clear\x1b[H");
        let _ = screen.grid_update(true);

        let before = screen.content_seq();
        screen.feed(b"\x1b[38;2;177;185;249m/help\x1b[39m\x1b[H");
        let after_foreground = screen.content_seq();
        assert!(
            after_foreground > before,
            "a foreground-only highlight must look like new content"
        );
        let update = screen.grid_update(false);
        assert_eq!(
            update.changed_rows[0].cells[0].fg,
            TermColor::Rgb(177, 185, 249)
        );

        screen.feed(b"\x1b[2H\x1b[48;5;4m/clear\x1b[49m\x1b[H");
        assert!(
            screen.content_seq() > after_foreground,
            "a background-only selection must look like new content"
        );
        let update = screen.grid_update(false);
        assert_eq!(update.changed_rows[0].cells[0].bg, TermColor::Ansi(4));
        assert_eq!(screen.lines()[..2], ["/help", "/clear"]);
    }

    #[test]
    fn reset_clears_screens_history_modes_title_progress_and_owed_replies() {
        let mut screen = HeadlessScreen::new(12, 3);
        // Enough lines to push rows into history, then a title, progress,
        // bracketed paste, SGR any-motion mouse, application cursor keys, a
        // pending cursor-position report and finally the alternate screen.
        screen.feed(b"one\r\ntwo\r\nthree\r\nfour\r\nfive\r\n");
        assert!(!screen.history_snapshot().is_empty());
        screen.feed(
            b"\x1b]0;busy\x07\x1b]9;4;1;40\x07\x1b[?2004h\x1b[?1003h\x1b[?1006h\x1b[?1h\x1b[6n",
        );
        screen.feed(b"\x1b[?1049h\x1b[Halt content");
        assert!(screen.is_alt_screen());
        assert!(screen.bracketed_paste());
        assert!(screen.mouse_reporting());
        assert_eq!(screen.title(), Some("busy"));
        assert_eq!(screen.progress(), Some((1, 40)));
        assert!(screen.keyboard_state().application_cursor_keys);
        let before = screen.content_seq();

        screen.reset();

        assert_eq!(
            screen.size(),
            (12, 3),
            "dimensions are the owner's, not the child's"
        );
        assert!(
            screen.lines().is_empty(),
            "the visible grid is blank: {:?}",
            screen.lines()
        );
        assert!(screen.history_snapshot().is_empty(), "history is discarded");
        assert!(!screen.is_alt_screen(), "the alternate screen is left");
        assert!(!screen.bracketed_paste());
        assert_eq!(screen.mouse_modes(), MouseModes::OFF);
        assert_eq!(screen.title(), None);
        assert_eq!(screen.progress(), None);
        assert_eq!(screen.cursor(), (0, 0, true));
        assert_eq!(
            screen.keyboard_state(),
            diri_proto::terminal_input::KeyboardState {
                enhancements: Some(0.try_into().unwrap()),
                ..Default::default()
            }
        );
        assert!(
            screen.take_replies().is_empty(),
            "a reply owed by the old emulator must not reach the child"
        );
        assert!(
            screen.content_seq() > before,
            "observers must notice the reset"
        );

        // The primary screen is genuinely empty too: leaving the alternate
        // screen after a reset must not reveal the pre-reset primary grid.
        screen.feed(b"\x1b[?1049h\x1b[?1049l");
        assert!(screen.lines().is_empty());
    }

    #[test]
    fn reset_discards_partial_sequences_and_an_open_synchronized_update() {
        let mut screen = screen_with(b"before\r\n");
        // An open DECSET 2026 bracket holding an erase, a split OSC progress
        // prefix and a truncated CSI parameter are all in flight.
        screen.feed(b"\x1b[?2026h\x1b[2J\x1b[H\x1b]9;4;1");
        screen.feed(b"\x1b[3");
        assert_eq!(
            screen.lines(),
            vec!["before"],
            "the bracket still holds the erase"
        );

        screen.reset();

        assert!(
            !screen.flush_expired_sync(),
            "no synchronized update survives a reset"
        );
        screen.feed(b"plain");
        assert_eq!(
            screen.lines(),
            vec!["plain"],
            "bytes after the reset are interpreted from a clean parser state"
        );
        assert_eq!(
            screen.progress(),
            None,
            "the split progress prefix was dropped"
        );
        assert!(screen.progress_carry.is_empty());
    }

    #[test]
    fn reset_forces_the_next_incremental_grid_update_to_be_full() {
        let mut screen = screen_with(b"row one\r\nrow two\r\n");
        let baseline = screen.grid_update(false);
        assert!(baseline.is_full_snapshot);
        screen.feed(b"row three\r\n");
        assert!(
            !screen.grid_update(false).is_full_snapshot,
            "steady state diffs"
        );

        screen.reset();

        let update = screen.grid_update(false);
        assert!(
            update.is_full_snapshot,
            "a diff against pre-reset cells is meaningless"
        );
        assert_eq!(update.changed_rows.len(), 24);
        assert!(
            update
                .changed_rows
                .iter()
                .flat_map(|row| &row.cells)
                .all(|cell| *cell == GridCell::BLANK)
        );
        assert_eq!(screen.filled_cells(), 0);
        assert!(
            screen
                .full_snapshot()
                .changed_rows
                .iter()
                .all(|row| row.cells.len() == 80)
        );
    }

    #[test]
    fn reset_keeps_constructor_options_and_makes_keyboard_state_known() {
        // Enhanced parser + product notifications, as the local Engine builds it.
        let mut screen = HeadlessScreen::new_with_keyboard_enhancements(40, 4).with_notifications();
        screen.feed(b"\x1b[>1u\x1b]777;notify;Tests;All green\x1b\\");
        assert!(screen.has_notifications());
        assert_ne!(screen.keyboard_snapshot().unwrap().current(), 0);
        screen.invalidate_keyboard_enhancements();
        assert!(
            screen.input_keyboard_state().is_none(),
            "unknown state stays unknown"
        );

        screen.reset();

        assert!(
            screen.keyboard_enhancements_enabled(),
            "the parser opt-in is preserved"
        );
        assert!(
            !screen.has_notifications(),
            "queued notifications are discarded"
        );
        assert_eq!(
            screen
                .keyboard_snapshot()
                .map(|snapshot| snapshot.current()),
            Some(0),
            "a reset establishes known default enhanced state"
        );
        assert!(screen.input_keyboard_state().is_some());
        screen.feed(b"\x1b]777;notify;Later;Still parsed\x1b\\");
        assert!(
            screen.has_notifications(),
            "notification parsing stays enabled"
        );
        screen.feed(b"\x1b[>1u");
        assert_ne!(screen.keyboard_snapshot().unwrap().current(), 0);

        // A legacy-only screen stays legacy-only after a reset.
        let mut legacy = HeadlessScreen::new(40, 4);
        legacy.reset();
        assert!(!legacy.keyboard_enhancements_enabled());
        assert!(!legacy.has_notifications());
        legacy.feed(b"\x1b[>1u\x1b]777;notify;Ignored;Not parsed\x1b\\");
        assert_eq!(
            legacy
                .keyboard_snapshot()
                .map(|snapshot| snapshot.current()),
            Some(0)
        );
        assert!(!legacy.has_notifications());
    }

    #[test]
    fn the_alternate_screen_is_detected() {
        let mut screen = HeadlessScreen::new(80, 24);
        assert!(!screen.is_alt_screen());
        screen.feed(b"\x1b[?1049h");
        assert!(screen.is_alt_screen(), "a pager or editor took the screen");
        screen.feed(b"\x1b[?1049l");
        assert!(!screen.is_alt_screen());
    }

    #[test]
    fn a_resize_reflows_to_the_new_width() {
        let mut screen = HeadlessScreen::new(80, 24);
        screen.feed(b"hello\r\n");
        screen.resize(40, 10);
        assert_eq!(screen.lines(), vec!["hello"]);
    }

    #[test]
    fn resize_keeps_history_within_the_cell_budget() {
        for alternate in [false, true] {
            let mut screen = HeadlessScreen::new(80, 24);
            screen.feed("retained history\r\n".repeat(6000).as_bytes());
            if alternate {
                screen.feed(b"\x1b[?1049h");
            }
            screen.resize(320, 24);
            if alternate {
                screen.feed(b"\x1b[?1049l");
            }
            let history = screen.term.grid().history_size();
            assert!(
                history <= history_line_limit(320),
                "{history} rows after widening (alternate={alternate})"
            );
            assert!(screen.lines().iter().any(|line| line == "retained history"));
            #[cfg(feature = "compact-history")]
            assert!(screen.term.grid().history_storage_bytes() <= HISTORY_STORAGE_BUDGET_BYTES);

            // Narrowing permits more rows again, including after an app reset.
            screen.resize(80, 24);
            screen.feed(b"\x1bc");
            screen.feed("new history\r\n".repeat(6000).as_bytes());
            assert_eq!(
                screen.term.grid().history_size(),
                history_line_limit(80).min(6000 + 1 - 24)
            );
        }
    }

    #[test]
    fn history_budget_does_not_have_a_wide_terminal_exception() {
        #[cfg(not(feature = "compact-history"))]
        assert!(
            history_line_limit(4096) * 4096 * std::mem::size_of::<Cell>()
                <= HISTORY_STORAGE_BUDGET_BYTES
        );
        #[cfg(feature = "compact-history")]
        {
            let mut screen = HeadlessScreen::new(4096, 24);
            screen.feed("wide retained history\r\n".repeat(1000).as_bytes());
            assert!(screen.term.grid().history_storage_bytes() <= HISTORY_STORAGE_BUDGET_BYTES);
            assert_eq!(screen.term.grid().history_size(), 1000 + 1 - 24);
        }
    }

    #[test]
    fn first_alternate_screen_matches_a_previously_initialized_screen() {
        for mode in [47, 1047, 1049] {
            let mut fresh = HeadlessScreen::new(80, 24);
            let mut initialized = HeadlessScreen::new(80, 24);
            initialized.feed(b"\x1b[?1049h\x1b[?1049l");
            for size in [(40, 10), (120, 30), (12, 3)] {
                fresh.resize(size.0, size.1);
                initialized.resize(size.0, size.1);
                let enter = format!("\x1b[?{mode}h");
                let leave = format!("\x1b[?{mode}l");
                for bytes in [
                    b"primary\r\n\x1b[31mstyled\x1b[0m\x1b7".as_slice(),
                    enter.as_bytes(),
                    "alternate: 界e\u{301}\r\nnext".as_bytes(),
                    leave.as_bytes(),
                    b"\x1b8!",
                    enter.as_bytes(),
                    b"\x1bc", // RIS while the alternate screen is active.
                    b"after reset",
                    leave.as_bytes(),
                ] {
                    fresh.feed(bytes);
                    initialized.feed(bytes);
                    assert_eq!(fresh.full_snapshot(), initialized.full_snapshot());
                    assert_eq!(fresh.lines(), initialized.lines());
                    assert_eq!(fresh.is_alt_screen(), initialized.is_alt_screen());
                }
            }
        }
    }

    #[test]
    fn incremental_grid_matches_fresh_snapshots_through_damage_and_resize() {
        let mut screen = HeadlessScreen::new(24, 8);
        let mut mirror = Vec::new();
        screen.grid_update(true).apply(&mut mirror);
        let operations: &[&[u8]] = &[
            b"hello\r\nworld",
            b"\x1b[1;31mcolored\x1b[0m",
            b"\x1b[H",
            b"\x1b[C\x1b[D",
            b"\x1b[2J",
            "wide: 界🙂".as_bytes(),
            b"\x1b[?1049h",
            b"\x1b[3;5Halternate",
            b"\x1b[?1049l",
            b"\x1b[2;6r\x1b[6;1H\n\n\x1b[r",
            b"\x1b[2;1H\x1b[L",
            b"\x1b[M",
            b"\x1b[?2026hheld\x1b[?2026l",
            b"\x1b[?25l",
        ];
        for round in 0..80 {
            if round % 7 == 0 {
                screen.resize(16 + round % 13, 4 + round % 8);
            }
            // Several feeds accumulate before each publication; byte-sized
            // chunks also exercise escapes and UTF-8 split across PTY reads.
            for bytes in operations.iter().cycle().skip(round).take(3) {
                for byte in bytes.iter() {
                    screen.feed(std::slice::from_ref(byte));
                }
            }
            let update = screen.grid_update(round % 11 == 0);
            update.apply(&mut mirror);
            let snapshot = screen.full_snapshot();
            let mut expected = Vec::new();
            snapshot.apply(&mut expected);
            assert_eq!(mirror, expected, "publication {round}");
            assert_eq!(
                (update.cursor_col, update.cursor_row, update.cursor_visible),
                (
                    snapshot.cursor_col,
                    snapshot.cursor_row,
                    snapshot.cursor_visible
                )
            );
            assert!(screen.grid_update(false).changed_rows.is_empty());
        }
    }

    #[test]
    fn a_restored_snapshot_reproduces_the_screen_and_modes() {
        let mut original = HeadlessScreen::new(40, 10);
        original.feed(b"\x1b[?2004h\x1b[?1000h\x1b[?1006h");
        original.feed(b"\x1b[1;31mred alert\x1b[0m\r\nplain line\r\n");
        original.feed("wide: ▶ done\r\n".as_bytes());
        let snapshot = original.full_snapshot();

        let mut restored = HeadlessScreen::new(40, 10);
        assert!(
            restored.restore(
                &[],
                &snapshot,
                false,
                true,
                MouseModes::new(MouseTrackingMode::ButtonEvents, MouseEncoding::Sgr),
            ),
            "restorable"
        );
        assert_eq!(restored.lines(), original.lines());
        assert!(restored.bracketed_paste(), "bracketed paste mode carried");
        assert!(restored.mouse_reporting(), "mouse mode carried");
        assert!(!restored.is_alt_screen());
        assert_eq!(restored.cursor(), original.cursor());
        // Attribute fidelity, not just text: the restored grid's cells match.
        assert_eq!(restored.full_snapshot().changed_rows, snapshot.changed_rows);
    }

    #[test]
    fn a_geometry_mismatch_refuses_to_restore() {
        let mut original = HeadlessScreen::new(40, 10);
        original.feed(b"hello\r\n");
        let snapshot = original.full_snapshot();

        let mut smaller = HeadlessScreen::new(39, 10);
        assert!(
            !smaller.restore(&[], &snapshot, false, false, MouseModes::OFF),
            "a checkpoint from another geometry is a cache miss"
        );
    }

    /// The extracted crate forked before checkpoints carried scrollback, so
    /// this travelled with the implementation. Without it, adopting a session
    /// after a daemon restart silently collapses its history to one row.
    #[test]
    fn a_restored_checkpoint_preserves_scrollback_history() {
        let mut original = HeadlessScreen::new(12, 2);
        original.feed(b"oldest\r\nmiddle\r\nvisible\r\n");
        let history = original.history_snapshot();
        let snapshot = original.full_snapshot();
        assert_eq!(history.len(), 2, "the minimized repro has two history rows");

        let mut restored = HeadlessScreen::new(12, 2);
        assert!(
            restored.restore(&history, &snapshot, false, false, MouseModes::OFF),
            "restorable"
        );
        assert_eq!(restored.scrollback(), original.scrollback());
    }

    #[cfg(feature = "compact-history")]
    #[test]
    fn compact_history_checkpoint_restores_cold_rows_and_annotations() {
        let mut original = HeadlessScreen::new(80, 24);
        for index in 0..3000 {
            original.feed(format!("\x1b]8;id={index};https://example.invalid/{index}\x07{index:06} 界 e\u{301}\x1b]8;;\x07\r\n").as_bytes());
        }
        let history = original.history_snapshot();
        let metadata = original.history_metadata();
        let snapshot = original.full_snapshot();
        assert_eq!(history.len(), 3000 + 1 - 24);
        let mut restored = HeadlessScreen::new(80, 24);
        assert!(restored.restore(&history, &snapshot, false, false, MouseModes::OFF));
        restored.restore_history_metadata(&metadata);
        assert_eq!(restored.scrollback().lines, original.scrollback().lines);
        assert_eq!(restored.history_metadata(), metadata);
        assert_eq!(restored.history_snapshot(), history);
        assert!(restored.term.grid().history_storage_bytes() <= HISTORY_STORAGE_BUDGET_BYTES);
    }

    #[test]
    fn a_boxed_prompt_survives_emulation_intact() {
        // What detection actually consumes, end to end.
        let mut screen = HeadlessScreen::new(80, 24);
        screen.feed("╭──────────────────────────╮\r\n".as_bytes());
        screen.feed("│ Do you want to proceed?  │\r\n".as_bytes());
        screen.feed("│ ❯ 1. Yes                 │\r\n".as_bytes());
        screen.feed("╰──────────────────────────╯\r\n".as_bytes());

        let snapshot = screen.snapshot();
        let text = snapshot.lines.join("\n");
        assert!(text.contains("Do you want to proceed?"));
        assert!(text.contains("❯ 1. Yes"), "wide glyphs survive: {text:?}");
    }

    #[test]
    fn grid_mirror_requires_a_snapshot_and_contiguous_deltas() {
        let mut screen = HeadlessScreen::new(8, 2);
        screen.feed(b"one");
        let snapshot = screen.full_snapshot();
        let mut mirror = GridMirror::new();
        mirror
            .apply_snapshot(9, &snapshot, false, true, MouseModes::OFF)
            .expect("snapshot");
        assert_eq!(mirror.sequence(), Some(9));
        assert_eq!(mirror.size(), (8, 2));
        assert_eq!(mirror.modes(), (false, true, MouseModes::OFF));

        let mut delta_source = HeadlessScreen::new(8, 2);
        delta_source.feed(b"one");
        let _ = delta_source.grid_update(true);
        delta_source.feed(b" two");
        let delta = delta_source.grid_update(false);
        mirror
            .apply_delta(
                10,
                &delta,
                true,
                false,
                MouseModes::new(MouseTrackingMode::AnyMotion, MouseEncoding::Sgr),
            )
            .expect("contiguous delta");
        assert_eq!(
            mirror.modes(),
            (
                true,
                false,
                MouseModes::new(MouseTrackingMode::AnyMotion, MouseEncoding::Sgr)
            )
        );
        assert!(matches!(
            mirror.apply_delta(12, &delta, false, false, MouseModes::OFF),
            Err(MirrorError::SequenceGap {
                expected: 11,
                actual: 12
            })
        ));
    }

    #[test]
    fn parser_preserves_each_tracking_mode_and_encoding_independently() {
        let mut screen = HeadlessScreen::new(300, 24);
        assert_eq!(screen.mouse_modes(), MouseModes::OFF);

        screen.feed(b"\x1b[?1000h");
        assert_eq!(
            screen.mouse_modes(),
            MouseModes::new(MouseTrackingMode::ButtonEvents, MouseEncoding::Legacy)
        );
        screen.feed(b"\x1b[?1002h\x1b[?1006h");
        assert_eq!(
            screen.mouse_modes(),
            MouseModes::new(MouseTrackingMode::ButtonMotion, MouseEncoding::Sgr)
        );
        screen.feed(b"\x1b[?1003h\x1b[?1006l");
        assert_eq!(
            screen.mouse_modes(),
            MouseModes::new(MouseTrackingMode::AnyMotion, MouseEncoding::Legacy)
        );
        screen.feed(b"\x1b[?1003l\x1b[?1006h");
        assert_eq!(
            screen.mouse_modes(),
            MouseModes::new(MouseTrackingMode::Off, MouseEncoding::Sgr),
            "1006 is independent of tracking"
        );
    }
}

#[cfg(test)]
mod qol_tests {
    use super::*;
    #[test]
    fn hyperlink_only_changes_advance_sequence_and_survive_mirror_reseed() {
        let mut screen = HeadlessScreen::new(40, 4);
        screen.feed(b"\x1b]8;;https://one.example\x07View PR\x1b]8;;\x07");
        let first = screen.grid_update(true);
        assert_eq!(
            first.changed_rows[0].metadata.links[0].uri,
            "https://one.example"
        );
        let before = screen.content_seq();
        screen.feed(b"\r\x1b]8;;https://two.example\x07View PR\x1b]8;;\x07");
        assert!(screen.content_seq() > before);
        let delta = screen.grid_update(false);
        assert_eq!(
            delta.changed_rows[0].metadata.links[0].uri,
            "https://two.example"
        );
        let mut mirror = GridMirror::new();
        mirror
            .apply_snapshot(1, &first, false, false, MouseModes::OFF)
            .unwrap();
        mirror
            .apply_delta(2, &delta, false, false, MouseModes::OFF)
            .unwrap();
        assert_eq!(mirror.full_update().unwrap(), screen.full_snapshot());
    }
    #[test]
    fn prompt_markers_obey_sync_erase_scrollback_and_alt_screen() {
        let mut screen = HeadlessScreen::new(8, 2);
        screen.feed(b"\x1b[?2026h\x1b]133;A\x07$ ");
        assert!(
            !screen
                .full_snapshot()
                .changed_rows
                .iter()
                .flat_map(|r| &r.cells)
                .any(|c| c.style.contains(TermStyle::PROMPT_START))
        );
        screen.feed(b"\x1b[?2026l");
        assert!(
            screen.full_snapshot().changed_rows[0].cells[0]
                .style
                .contains(TermStyle::PROMPT_START)
        );
        screen.feed(b"\r\ncommand\r\n");
        let history = screen.scrollback_cells(0, 10);
        let rows = GridRowCodec::decode_rows(&history.payload, history.row_count as usize).unwrap();
        assert!(rows[0][0].style.contains(TermStyle::PROMPT_START));
        screen.feed(b"\x1b[?1049h\x1b]133;A\x07$ ");
        assert!(
            !screen
                .full_snapshot()
                .changed_rows
                .iter()
                .flat_map(|r| &r.cells)
                .any(|c| c.style.contains(TermStyle::PROMPT_START))
        );
        screen.feed(b"\x1b[?1049l\x1b[H\x1b[2J");
        assert!(
            !screen
                .full_snapshot()
                .changed_rows
                .iter()
                .flat_map(|r| &r.cells)
                .any(|c| c.style.contains(TermStyle::PROMPT_START))
        );
    }
    #[test]
    fn visible_text_preserves_unicode_without_terminal_filler_cells() {
        let mut screen = HeadlessScreen::new(20, 4);
        screen.feed("<界> e\u{301}\r\nA🙂B".as_bytes());
        assert_eq!(screen.lines(), vec!["<界> e\u{301}", "A🙂B"]);
        assert_eq!(screen.snapshot().lines, screen.lines());

        let mut restored = HeadlessScreen::new(20, 4);
        assert!(restored.restore(&[], &screen.full_snapshot(), false, false, MouseModes::OFF));
        assert_eq!(restored.lines(), screen.lines());

        screen.feed("\x1b[?1049h\x1b[H<界> e\u{301}".as_bytes());
        assert_eq!(screen.lines(), vec!["<界> e\u{301}"]);
        screen.feed(b"\x1b[?1049l");
        assert_eq!(screen.lines(), vec!["<界> e\u{301}", "A🙂B"]);
    }

    #[test]
    fn visible_text_keeps_real_spaces_after_overwriting_a_wide_glyph() {
        let mut screen = HeadlessScreen::new(8, 2);
        screen.feed("界X\rA".as_bytes());
        assert_eq!(screen.lines(), vec!["A X"]);
    }

    #[test]
    fn history_text_maps_unicode_scalars_to_their_original_cells() {
        let mut screen = HeadlessScreen::new(12, 2);
        screen.feed("<界> e\u{301}\r\nA🙂B\r\nplain\r\nend".as_bytes());
        let history = screen.scrollback();
        assert_eq!(
            &history.lines[..4],
            &["<界> e\u{301}", "A🙂B", "plain", "end"]
        );
        assert_eq!(history.visible_start_row, 2);
        assert_eq!(
            history.text_cells[&0],
            vec![[0, 1], [1, 3], [3, 4], [4, 5], [5, 6], [5, 6]]
        );
        assert_eq!(history.text_cells[&1], vec![[0, 1], [1, 3], [3, 4]]);
        assert!(!history.text_cells.contains_key(&2));
        assert!(!history.text_cells.contains_key(&3));
        screen.feed(b"\x1b[?1049h\x1b[H");
        screen.feed("e\u{301}".as_bytes());
        let alternate = screen.scrollback();
        assert!(alternate.is_alt_screen);
        assert_eq!(alternate.lines[0], "e\u{301}");
        assert_eq!(alternate.text_cells[&0], vec![[0, 1], [0, 1]]);
    }

    #[test]
    fn wrap_wide_glyph_and_combining_metadata_survive_restore() {
        let mut screen = HeadlessScreen::new(6, 3);
        screen.feed("abcdefghi\r\n界e\u{301}".as_bytes());
        let snapshot = screen.full_snapshot();
        assert!(
            snapshot.changed_rows[0].cells[5]
                .style
                .contains(TermStyle::SOFT_WRAP)
        );
        assert_eq!(snapshot.changed_rows[2].cells[1].scalar, 0);
        assert_eq!(
            snapshot.changed_rows[2].metadata.graphemes,
            vec![(2, "\u{301}".into())]
        );
        let mut restored = HeadlessScreen::new(6, 3);
        assert!(restored.restore(&[], &snapshot, false, false, MouseModes::OFF));
        assert_eq!(restored.full_snapshot(), snapshot);
    }
    #[test]
    fn named_links_remain_available_in_history_and_after_restore() {
        let mut screen = HeadlessScreen::new(80, 2);
        screen.feed(b"\x1b]8;;https://example.org/pr/42\x07View PR\x1b]8;;\x07\r\nsecond\r\nthird");
        let history = screen.scrollback_cells(0, 128);
        assert_eq!(
            history.metadata[0].links[0].uri,
            "https://example.org/pr/42"
        );
        let mut restored = HeadlessScreen::new(80, 2);
        assert!(restored.restore(
            &screen.history_snapshot(),
            &screen.full_snapshot(),
            false,
            false,
            MouseModes::OFF
        ));
        restored.restore_history_metadata(&screen.history_metadata());
        assert_eq!(restored.scrollback_cells(0, 128).metadata, history.metadata);
    }
}

#[cfg(test)]
mod find_capture_tests {
    use super::*;

    #[test]
    fn oversized_visible_annotations_fail_instead_of_silently_losing_text() {
        let mut screen = HeadlessScreen::new(320, 40);
        let dense = "e\u{301}".repeat(320);
        screen.feed(dense.repeat(40).as_bytes());
        assert!(screen.find_capture_cells().is_err());
    }

    #[test]
    fn full_history_find_capture_preserves_combining_text_in_every_retained_row() {
        let mut screen = HeadlessScreen::new(40, 30);
        screen.feed("needle e\u{301}\r\n".repeat(9000).as_bytes());
        let capture = screen.find_capture_cells().unwrap();
        assert!(
            capture.first_row > 0,
            "capture explicitly reports a recent tail"
        );
        let rows = GridRowCodec::decode_rows(&capture.payload, capture.row_count as usize).unwrap();
        for (row, metadata) in rows.iter().zip(&capture.metadata) {
            if row.iter().any(|cell| cell.scalar == 'e' as u32) {
                assert!(
                    metadata.graphemes.iter().any(|(_, text)| text == "\u{301}"),
                    "search capture must not silently drop a combining mark"
                );
            }
        }
    }
}
