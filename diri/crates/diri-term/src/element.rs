use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::{Duration, Instant};

use diri_proto::grid::{GridCell, GridUpdate};
use diri_proto::terminal::MouseModes;
use gpui::{
    App, Bounds, ContentMask, Element, ElementId, FocusHandle, Font, FontFallbacks, FontId,
    GlobalElementId, InputHandler, InspectorElementId, IntoElement, LayoutId, PaintQuad, Pixels,
    Point, ShapedLine, SharedString, Style, TextAlign, TextRun, UTF16Selection, Window, fill, font,
    point, px, relative, size,
};

use crate::blocks::BlockGlyph;
use crate::buffer::{ApplySummary, ChangedRenderRow, GridBuffer};
use crate::cursor_motion::{CursorCell, CursorDamage, CursorDriver, CursorFrame, CursorSchedule};
use crate::find::{
    FindSnapshot, FindSpan, NavigationTarget, SearchJob, SearchRequest, SearchResult,
    TerminalFindModel,
};
use crate::metrics::CellMetrics;
use crate::scrollback::{
    ScrollRouter, ScrollbackApplyError, ScrollbackRequest, ScrollbackViewport, ScrolledState,
    TerminalModes, WheelDelta, WheelEvent, WheelRoute,
};
use crate::selection::{SelectionPoint, TerminalSelection};
use crate::selection_shimmer::SelectionShimmer;
use crate::sprites::{AntialiasedShape, Sprite, SpriteGrid};
use crate::theme::{ResolvedCellStyle, TermTheme, is_default_background};

mod selection_paint;

use selection_paint::SelectionPaint;

static NEXT_ELEMENT_ID: AtomicU64 = AtomicU64::new(1);

pub type SharedGridBuffer = Arc<RwLock<GridBuffer>>;
type TextInputCallback = Arc<dyn Fn(&str) + Send + Sync>;

/// A command-clickable reference rendered in a terminal row.
///
/// Web URLs stay distinct so hosts can preserve their normal external-opening
/// behavior while routing paths and `file://` references into an editor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TerminalReference {
    Url(String),
    File(String),
}

pub type ReferenceSpan = (i64, usize, usize);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReferenceHit {
    pub reference: TerminalReference,
    /// Absolute row, start column, exclusive end column.
    pub spans: Vec<ReferenceSpan>,
}

impl TerminalReference {
    pub fn destination(&self) -> &str {
        match self {
            Self::Url(value) | Self::File(value) => value,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct RendererStats {
    pub frames: u64,
    pub total_frame_time: Duration,
    pub max_frame_time: Duration,
    pub shape_cache_hits: u64,
    pub shape_cache_misses: u64,
}

impl RendererStats {
    #[must_use]
    pub fn average_frame_time(self) -> Duration {
        if self.frames == 0 {
            Duration::ZERO
        } else {
            self.total_frame_time.div_f64(self.frames as f64)
        }
    }
}

#[derive(Clone)]
pub struct TerminalElement {
    buffer: SharedGridBuffer,
    shared: Arc<ElementSharedState>,
    theme: TermTheme,
    /// Coverage of the default-background fill under the grid. Cells with
    /// their own background keep painting opaque; only the theme background
    /// lets a translucent host surface show through.
    background_opacity: f32,
    font: Font,
    font_size: Pixels,
    focus_handle: Option<FocusHandle>,
    text_input: Option<TextInputCallback>,
    ime_state: Arc<Mutex<TerminalImeState>>,
    focus_override: Option<bool>,
    suspended: bool,
    hovered_reference: Option<ReferenceHit>,
    reduce_motion: bool,
    cursor_hidden: bool,
    seen_marker: Option<SeenMarker>,
}

/// Where the reader stopped looking: a hairline is drawn above `row`, the
/// first absolute row they have not seen.
///
/// The client learns absolute rows only from scrollback reads, so the live
/// view cannot place the line from its own state. `live_start_row` is the
/// live grid's first absolute row as of the read that produced this marker;
/// the host refreshes it once output settles. A reading view places the line
/// from its pinned viewport instead.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SeenMarker {
    pub row: i64,
    pub live_start_row: i64,
}

impl SeenMarker {
    /// Still inside the live grid, where output moves it and `live_start_row`
    /// goes stale. Once it has scrolled into history it never returns.
    #[must_use]
    pub const fn is_live(&self) -> bool {
        self.row >= self.live_start_row
    }

    /// The window row the line sits above. Row zero is excluded: a line on
    /// the top edge separates nothing.
    fn window_row(&self, top: i64, visible_rows: usize) -> Option<u16> {
        let row = usize::try_from(self.row.checked_sub(top)?).ok()?;
        (1..visible_rows)
            .contains(&row)
            .then(|| u16::try_from(row).ok())
            .flatten()
    }
}

/// Selection and reading state only: deliberately does not retain an input
/// callback, focus handle or IME composition when a session registers a view.
#[derive(Clone)]
pub struct TerminalDamageObserver {
    buffer: SharedGridBuffer,
    shared: Arc<ElementSharedState>,
}

/// Moves the reading view, dropping a selection that returning to live would
/// leave attached to rows the next frame replaces.
/// Moves a view that is pinned to a find capture. The capture holds every row
/// on the way, so nothing is fetched and the pin is kept.
fn place_glide(viewport: &mut ScrollbackViewport, rows: f64, visible_rows: usize) {
    let position =
        crate::smooth_scroll::ScrollPosition::from_rows(rows, viewport.max_offset(visible_rows));
    viewport.set_scroll_position(position, visible_rows);
}

fn set_view_offset(
    shared: &ElementSharedState,
    buffer: &SharedGridBuffer,
    offset: i64,
    visible_rows: usize,
) -> bool {
    // Keys, typing back to live, and the scroller all come through here:
    // whoever moves the view owns it, and a find glide lets go.
    mutex_lock(&shared.scroll_glide).glide = None;
    let mut viewport = mutex_lock(&shared.viewport);
    let changed = viewport.set_view_offset(offset, visible_rows);
    viewport.hold_reading_view(&read_lock(buffer));
    if changed && !viewport.is_reading() {
        mutex_lock(&shared.selection).clear();
    }
    changed
}

impl TerminalDamageObserver {
    pub fn prepare(&self, update: &GridUpdate) {
        // Absolute rows keep a selection attached while the viewport moves,
        // but not when the daemon replaces cells at those rows. Damage is
        // row-granular, so unrelated live output and history remain selected.
        mutex_lock(&self.shared.selection_shimmer).yield_to_output(
            if update.is_full_snapshot {
                usize::from(update.rows)
            } else {
                update.changed_rows.len()
            },
            usize::from(update.rows),
        );
        let mut viewport = mutex_lock(&self.shared.viewport);
        let live_start_row = viewport.live_start_row();
        let buffer = read_lock(&self.buffer);
        viewport.hold_reading_view(&buffer);
        let reading_held = viewport.is_reading();
        drop(viewport);
        let replaces_grid =
            update.is_full_snapshot || buffer.cols != update.cols || buffer.rows != update.rows;
        note_cursor_damage(&self.shared.cursor, &buffer, update, replaces_grid);
        let damaged_cols = usize::from(if replaces_grid {
            buffer.cols.max(update.cols)
        } else {
            update.cols
        });
        let mut selection = mutex_lock(&self.shared.selection);
        let selection_overlaps_damage = if reading_held || selection.range().is_none() {
            false
        } else if replaces_grid {
            (0..buffer.rows.max(update.rows)).any(|row| {
                selection.overlaps_row(live_start_row.saturating_add(i64::from(row)), damaged_cols)
            })
        } else {
            update.changed_rows.iter().any(|changed| {
                changed.y < update.rows
                    && selection.overlaps_row(
                        live_start_row.saturating_add(i64::from(changed.y)),
                        damaged_cols,
                    )
            })
        };
        if selection_overlaps_damage {
            selection.clear();
        }
    }
}

struct TerminalImeState {
    marked_text: String,
    enabled: bool,
}
impl Default for TerminalImeState {
    fn default() -> Self {
        Self {
            marked_text: String::new(),
            enabled: true,
        }
    }
}

impl TerminalImeState {
    fn marked_range(&self) -> Option<std::ops::Range<usize>> {
        (!self.marked_text.is_empty()).then(|| 0..self.marked_text.encode_utf16().count())
    }
}

struct TerminalInputHandler {
    text_input: TextInputCallback,
    ime_state: Arc<Mutex<TerminalImeState>>,
    /// Reading state only, so the handler can show the prompt it types into
    /// without retaining the element that owns its callback.
    view: TerminalDamageObserver,
    cursor_bounds: Bounds<Pixels>,
    cell_width: Pixels,
    cursor: Arc<Mutex<CursorDriver>>,
}

impl TerminalInputHandler {
    /// Forwards committed text and reports whether the caller has to repaint:
    /// either it brought a reading view back to live, or a dimmed cursor has
    /// to come back solid.
    fn commit_text(&self, text: &str) -> bool {
        let mut state = mutex_lock(&self.ime_state);
        state.marked_text.clear();
        let enabled = state.enabled;
        drop(state);
        // Native input is for committed printable/IME text. AppKit can still
        // send a control character after dispatching a Command shortcut; in
        // particular, Command-C may arrive as ETX, which would interrupt the
        // foreground process if forwarded to the PTY. Control keys have their
        // own key-down encoder, so dropping them here cannot remove a valid
        // terminal command.
        if !enabled || text.chars().any(char::is_control) {
            return false;
        }
        // Committed text is typing like any key: it lands at the live prompt,
        // which a held reading view would keep off screen with no cursor. The
        // target offset is zero, so the visible row count cannot clamp it.
        let returned =
            !text.is_empty() && set_view_offset(&self.view.shared, &self.view.buffer, 0, 0);
        let solid = note_keystroke(&self.cursor);
        (self.text_input)(text);
        returned || solid
    }

    fn mark_text(&self, text: &str) {
        let mut state = mutex_lock(&self.ime_state);
        if state.enabled {
            text.clone_into(&mut state.marked_text);
        }
    }
}

impl InputHandler for TerminalInputHandler {
    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: 0..0,
            reversed: false,
        })
    }

    fn marked_text_range(
        &mut self,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Option<std::ops::Range<usize>> {
        mutex_lock(&self.ime_state).marked_range()
    }

    fn text_for_range(
        &mut self,
        _range_utf16: std::ops::Range<usize>,
        _adjusted_range: &mut Option<std::ops::Range<usize>>,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Option<String> {
        None
    }

    fn replace_text_in_range(
        &mut self,
        _replacement_range: Option<std::ops::Range<usize>>,
        text: &str,
        window: &mut Window,
        _cx: &mut App,
    ) {
        if self.commit_text(text) {
            window.refresh();
        }
        window.invalidate_character_coordinates();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        _range_utf16: Option<std::ops::Range<usize>>,
        new_text: &str,
        _new_selected_range: Option<std::ops::Range<usize>>,
        window: &mut Window,
        _cx: &mut App,
    ) {
        self.mark_text(new_text);
        window.invalidate_character_coordinates();
    }

    fn unmark_text(&mut self, window: &mut Window, _cx: &mut App) {
        mutex_lock(&self.ime_state).marked_text.clear();
        window.invalidate_character_coordinates();
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: std::ops::Range<usize>,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Option<Bounds<Pixels>> {
        let mut bounds = self.cursor_bounds;
        bounds.origin.x += self.cell_width * range_utf16.start as f32;
        Some(bounds)
    }

    fn character_index_for_point(
        &mut self,
        _point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Option<usize> {
        None
    }

    fn apple_press_and_hold_enabled(&mut self) -> bool {
        false
    }

    fn prefers_ime_for_printable_keys(&mut self, _window: &mut Window, _cx: &mut App) -> bool {
        true
    }
}

struct ElementSharedState {
    id: u64,
    row_cache: Mutex<Vec<Option<CachedRow>>>,
    render_generations: Mutex<Vec<u64>>,
    render_context: Mutex<Option<RowRenderContext>>,
    stats: Mutex<RendererStats>,
    viewport: Mutex<ScrollbackViewport>,
    selection: Mutex<TerminalSelection>,
    selection_shimmer: Mutex<SelectionShimmer>,
    find_highlights: Mutex<FindHighlights>,
    modes: Mutex<TerminalModes>,
    scroll_router: Mutex<ScrollRouter>,
    history_lines: Mutex<HistoryLineCache>,
    metrics: Mutex<Option<(Font, u32, CellMetrics)>>,
    /// Behind its own `Arc` so the input handler and the blink wake can hold
    /// it without holding the rest of the view's state.
    cursor: Arc<Mutex<CursorDriver>>,
    scroll_glide: Mutex<GlideState>,
}

/// The glide to a find match, if one is running, and the clock it reads.
#[derive(Default)]
struct GlideState {
    glide: Option<crate::scroll_glide::ScrollGlide>,
    /// Pinned by tests and frame-by-frame renders; the wall clock otherwise.
    clock: Option<Instant>,
}

impl GlideState {
    fn now(&self) -> Instant {
        self.clock.unwrap_or_else(Instant::now)
    }
}

#[derive(Default)]
struct FindHighlights {
    retained: Option<(
        Arc<crate::find::RetainedFindSnapshot>,
        Vec<crate::find::FindMatch>,
        usize,
    )>,
    spans: Vec<FindSpan>,
    current_bounds: Option<Bounds<Pixels>>,
}

/// Shaped lines for every row of a reading view (history and the held live
/// rows under it), keyed by absolute row and content-addressed
/// by a digest of the row's cells and combining text, so shaping survives across scrolled frames
/// instead of being redone per frame.
///
/// The digest replaces following the viewport's `content_seq`: that sequence
/// advances on *any* visible change — a spinner in the live grid was enough —
/// which dumped the shaping of history rows that had not moved a pixel.
/// Comparing the complete painted content costs a hash against a reshape.
#[derive(Default)]
struct HistoryLineCache {
    key: Option<HistoryShapeKey>,
    lines: HashMap<i64, (u64, ShapedLine)>,
}

/// Everything shaping depends on besides the cells themselves. Row position is
/// deliberately absent: a `ShapedLine` is position-independent and stays valid
/// as a history row slides through the window.
#[derive(Clone, Copy, Eq, PartialEq)]
struct HistoryShapeKey {
    theme_signature: u64,
    font_id: FontId,
    font_size_bits: u32,
    cell_width_bits: u32,
    visible_cols: usize,
}

impl HistoryLineCache {
    const MAX_ROWS: usize = 1024;

    /// Rows within this distance of the window survive an overflow eviction.
    const RETAINED_RADIUS: i64 = (Self::MAX_ROWS / 2) as i64;

    fn validate(&mut self, key: HistoryShapeKey, anchor_row: i64) {
        if self.key != Some(key) {
            self.lines.clear();
            self.key = Some(key);
            return;
        }
        if self.lines.len() > Self::MAX_ROWS {
            // Evict by distance rather than clearing: dumping the whole map
            // re-shaped the entire window on the very next frame, and deep
            // scrollback now reaches far enough past MAX_ROWS to hit this
            // repeatedly while scrolling.
            self.lines
                .retain(|row, _| row.abs_diff(anchor_row) <= Self::RETAINED_RADIUS as u64);
        }
    }

    /// The shaped line for `absolute_row`, only if it was shaped from exactly
    /// these cells.
    fn get(&self, absolute_row: i64, digest: u64) -> Option<&ShapedLine> {
        self.lines
            .get(&absolute_row)
            .filter(|(cached, _)| *cached == digest)
            .map(|(_, line)| line)
    }

    fn insert(&mut self, absolute_row: i64, digest: u64, line: ShapedLine) {
        self.lines.insert(absolute_row, (digest, line));
    }

    /// Frees the table once the pane is back on the live grid. A `ShapedLine`
    /// carries about 3 KB of inline decoration runs, so a long scroll leaves
    /// a multi-megabyte table that `clear` and `retain` would keep allocated.
    fn release(&mut self) {
        if self.lines.capacity() != 0 {
            *self = Self::default();
        }
    }
}

#[cfg(test)]
fn digest_cells(cells: &[GridCell]) -> u64 {
    digest_row(cells, &[], &[])
}

fn digest_row(cells: &[GridCell], graphemes: &[(u16, String)], tints: &[Tint]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    cells.hash(&mut hasher);
    graphemes.hash(&mut hasher);
    for tint in tints {
        (tint.start, tint.end).hash(&mut hasher);
        for channel in [tint.color.r, tint.color.g, tint.color.b, tint.color.a] {
            channel.to_bits().hash(&mut hasher);
        }
    }
    hasher.finish()
}

/// A selection or find overlay across part of one row. It is painted between
/// the cells' backgrounds and their glyphs, so the glyphs under it are colored
/// to stay readable against it and a row's shaped line depends on its tints.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Tint {
    start: usize,
    end: usize,
    color: gpui::Rgba,
}

/// The combined overlay above `column`, later tints painted over earlier ones.
fn tint_at(tints: &[Tint], column: usize) -> Option<gpui::Rgba> {
    tints
        .iter()
        .filter(|tint| (tint.start..tint.end).contains(&column))
        .map(|tint| tint.color)
        .reduce(|below, above| crate::contrast::over(above, below))
}

#[derive(Clone)]
struct CachedRow {
    cells: Vec<GridCell>,
    graphemes: Vec<(u16, String)>,
    tints: Vec<Tint>,
    background_quads: Vec<PaintQuad>,
    decoration_quads: Vec<PaintQuad>,
    sprite_shapes: Vec<AntialiasedShape>,
    line: ShapedLine,
}

impl CachedRow {
    /// Whether the row moved. Sprites are laid out on whole device pixels, so
    /// a row holding any moves only by a whole number of them.
    fn move_vertically(&mut self, dy: Pixels, grid: SpriteGrid) -> bool {
        if !grid.keeps_snapping(dy)
            && self
                .cells
                .iter()
                .any(|cell| Sprite::from_scalar(cell.scalar).is_some())
        {
            return false;
        }
        for quad in self
            .background_quads
            .iter_mut()
            .chain(self.decoration_quads.iter_mut())
        {
            quad.bounds.origin.y += dy;
        }
        for shape in &mut self.sprite_shapes {
            shape.move_vertically(dy);
        }
        true
    }
}

/// Move the existing viewport cache with a full-height scroll. This is only a
/// reuse hint: the caller compares every cell before accepting a prepared row.
/// Sparse damage and render-context changes never rotate the cache.
fn align_scrolled_rows(cache: &mut [Option<CachedRow>], damage: &[ChangedRenderRow]) -> usize {
    if cache.len() < 2 || damage.len() != cache.len() {
        return 0;
    }
    for changed in [damage.first().unwrap(), damage.last().unwrap()] {
        if let Some(previous) = cache.iter().position(|entry| {
            entry
                .as_ref()
                .is_some_and(|row| row.cells == changed.cells && row.graphemes == changed.graphemes)
        }) {
            let offset = (previous + cache.len() - changed.row) % cache.len();
            if offset != 0 {
                cache.rotate_left(offset);
                return offset;
            }
        }
    }
    0
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RowRenderContext {
    theme_signature: u64,
    font_id: FontId,
    font_size_bits: u32,
    cell_width_bits: u32,
    line_height_bits: u32,
    origin_x_bits: u32,
    origin_y_bits: u32,
    scale_bits: u32,
    visible_cols: usize,
    visible_rows: usize,
}

pub struct TerminalPrepaintState {
    started_at: Option<Instant>,
    background_quads: Vec<PaintQuad>,
    decoration_quads: Vec<PaintQuad>,
    sprite_shapes: Vec<AntialiasedShape>,
    overlay_quads: Vec<PaintQuad>,
    selection: SelectionPaint,
    /// Reading path: window row and the absolute row whose shape paint reads
    /// from the history cache, as the live path reads the row cache.
    lines: Vec<(u16, i64)>,
    metrics: Option<CellMetrics>,
    cursor: Option<CursorPaint>,
    cache_hits: u64,
    cache_misses: u64,
    /// Live path: paint straight out of the shared row cache instead of
    /// composed copies, so an unchanged frame clones nothing.
    paint_from_cache: bool,
    scroll: Option<ScrollPaint>,
}

/// A reading view resting between rows. Everything row-positioned is built
/// on whole rows and moved up by `shift` in one place at the end of
/// `prepaint`; only glyph lines, which paint from the shape cache, apply it
/// in `paint`.
struct ScrollPaint {
    shift: Pixels,
    /// The terminal fill, which stays put and spans the full bounds.
    backdrop: PaintQuad,
    /// The grid's rows. The extra row slides in under this edge rather than
    /// showing through the partial-row strip below the grid, which would
    /// blink off whenever the view came to rest on a whole row.
    clip: Bounds<Pixels>,
}

struct CursorPaint {
    row: u16,
    col: u16,
    /// Cells the block spans: two on a double-width glyph.
    cols: u16,
    quad: PaintQuad,
    /// Set while the pane does not hold the keyboard. Painted over the row's
    /// own text, after whatever is left of the fill.
    outline: Option<PaintQuad>,
    /// Color of the glyph or block element drawn inside the cursor.
    text: gpui::Rgba,
    glyph: Option<ShapedLine>,
    block: Option<BlockGlyph>,
    sprite: Option<Sprite>,
    frame: CursorFrame,
    /// While the block is between cells it inverts whatever it covers: these
    /// are the covered cells' glyphs in the cursor's text color, painted
    /// clipped to the block so the glyphs stay put and only the block moves.
    covered: Vec<CoveredGlyph>,
}

struct CoveredGlyph {
    col: u16,
    row: u16,
    glyph: Option<ShapedLine>,
    block: Option<BlockGlyph>,
    sprite: Option<Sprite>,
}

impl CursorPaint {
    /// The static cursor replaces its cell's text; a moving or translucent
    /// one is painted over the row's own text instead.
    fn replaces_text_at(&self, row: u16) -> bool {
        self.row == row && self.frame.is_static()
    }
}

impl TerminalElement {
    #[must_use]
    pub fn new(buffer: SharedGridBuffer) -> Self {
        let terminal_font = default_terminal_font();
        Self {
            buffer,
            shared: Arc::new(ElementSharedState {
                id: NEXT_ELEMENT_ID.fetch_add(1, Ordering::Relaxed),
                row_cache: Mutex::new(Vec::new()),
                render_generations: Mutex::new(Vec::new()),
                render_context: Mutex::new(None),
                stats: Mutex::new(RendererStats::default()),
                viewport: Mutex::new(ScrollbackViewport::default()),
                selection: Mutex::new(TerminalSelection::default()),
                selection_shimmer: Mutex::new(SelectionShimmer::default()),
                find_highlights: Mutex::new(FindHighlights::default()),
                modes: Mutex::new(TerminalModes::default()),
                scroll_router: Mutex::new(ScrollRouter::default()),
                history_lines: Mutex::new(HistoryLineCache::default()),
                metrics: Mutex::new(None),
                cursor: Arc::new(Mutex::new(CursorDriver::default())),
                scroll_glide: Mutex::new(GlideState::default()),
            }),
            theme: TermTheme::default(),
            background_opacity: 1.0,
            font: terminal_font,
            font_size: px(13.0),
            focus_handle: None,
            text_input: None,
            ime_state: Arc::new(Mutex::new(TerminalImeState::default())),
            focus_override: None,
            suspended: false,
            hovered_reference: None,
            reduce_motion: false,
            cursor_hidden: false,
            seen_marker: None,
        }
    }

    #[must_use]
    pub fn with_buffer(buffer: GridBuffer) -> Self {
        Self::new(Arc::new(RwLock::new(buffer)))
    }

    #[must_use]
    pub fn buffer(&self) -> SharedGridBuffer {
        self.buffer.clone()
    }

    /// True when the mirrored screen has painted glyphs.
    #[must_use]
    pub fn has_content(&self) -> bool {
        !read_lock(&self.buffer).is_blank()
    }

    /// Rows in the mirrored screen. The daemon owns this number, so it trails
    /// the pane for as long as a resize takes to round-trip; the pane reads it
    /// to place the grid rather than assuming the two already agree.
    #[must_use]
    pub fn grid_rows(&self) -> u16 {
        read_lock(&self.buffer).rows
    }

    /// Columns in the mirrored screen, used to clamp pointer reports to the
    /// authoritative grid while a resize is in flight.
    #[must_use]
    pub fn grid_cols(&self) -> u16 {
        read_lock(&self.buffer).cols
    }

    #[must_use]
    pub fn theme(mut self, theme: TermTheme) -> Self {
        self.theme = theme;
        self
    }

    /// Sets how much of the theme background the grid paints itself. Pass
    /// `0.0` when the surface underneath already supplies a (translucent)
    /// fill so the desktop can show through default-background cells.
    #[must_use]
    pub fn background_opacity(mut self, opacity: f32) -> Self {
        self.background_opacity = opacity.clamp(0.0, 1.0);
        self
    }

    #[must_use]
    pub fn font(mut self, font: Font) -> Self {
        self.font = font;
        self
    }

    #[must_use]
    pub fn hovered_reference(mut self, hit: Option<ReferenceHit>) -> Self {
        self.hovered_reference = hit;
        self
    }

    #[must_use]
    pub fn seen_marker(mut self, marker: Option<SeenMarker>) -> Self {
        self.seen_marker = marker;
        self
    }

    pub fn font_size(mut self, font_size: Pixels) -> Self {
        self.font_size = font_size;
        self
    }

    #[must_use]
    pub fn focus_handle(mut self, focus_handle: FocusHandle) -> Self {
        self.focus_handle = Some(focus_handle);
        self.focus_override = None;
        self
    }

    /// Temporarily hands text ownership to an overlay. Existing native handlers
    /// observe the same gate, including callbacks delivered before the next paint.
    pub fn set_text_input_enabled(&self, enabled: bool) {
        let mut state = mutex_lock(&self.ime_state);
        state.enabled = enabled;
        state.marked_text.clear();
    }

    /// Receives committed platform text, including multi-stage IME input.
    #[must_use]
    pub fn on_text_input(mut self, handler: impl Fn(&str) + Send + Sync + 'static) -> Self {
        self.text_input = Some(Arc::new(handler));
        self
    }

    /// Primarily useful for previews and deterministic visual tests.
    #[must_use]
    pub fn focused(mut self, focused: bool) -> Self {
        self.focus_override = Some(focused);
        self
    }

    /// A picture of a terminal rather than a pane: thumbnails and peeks have
    /// no insertion point to mark, so they draw no cursor, not a hollow one.
    #[must_use]
    pub fn without_cursor(mut self) -> Self {
        self.cursor_hidden = true;
        self
    }

    /// Holds the cursor static: no blink, no glide.
    #[must_use]
    pub fn reduce_motion(mut self, reduce_motion: bool) -> Self {
        self.reduce_motion = reduce_motion;
        self
    }

    /// A key went to the program through the host rather than through
    /// committed text. Keeps the cursor solid and marks the next short cursor
    /// move as the user's, which is what lets it glide. Returns true when the
    /// cursor is dimmed on screen and the host should repaint it solid.
    #[must_use]
    pub fn note_user_input(&self) -> bool {
        note_keystroke(&self.shared.cursor)
    }

    /// Drives cursor motion from a caller-owned clock instead of the wall
    /// clock, and schedules no frames. For tests and frame-by-frame renders.
    pub fn set_cursor_clock(&self, now: Option<Instant>) {
        mutex_lock(&self.shared.cursor).set_clock(now);
    }

    /// Whether the cursor was last painted filled (`true`) or hollow.
    #[cfg(test)]
    pub(crate) fn cursor_painted_focused(&self) -> Option<bool> {
        mutex_lock(&self.shared.cursor).painted_focused()
    }

    /// What the last painted frame asked for next. `Rest` means the cursor
    /// will not cause another frame.
    #[must_use]
    pub fn cursor_schedule(&self) -> Option<CursorSchedule> {
        mutex_lock(&self.shared.cursor).last_schedule()
    }

    #[must_use]
    pub fn suspended(mut self, suspended: bool) -> Self {
        self.suspended = suspended;
        self
    }

    /// Apply damage and refresh the window only if visible output changed.
    pub fn apply(&self, update: GridUpdate, window: &mut Window) -> ApplySummary {
        let summary = self.apply_damage(update);
        if summary.changed {
            window.refresh();
        }
        summary
    }

    /// Apply every grid update without deciding when its host should repaint.
    ///
    /// Terminal hosts use this to keep the authoritative buffer current while
    /// coalescing bursts and suppressing paints for offscreen residents.
    pub fn apply_damage(&self, update: GridUpdate) -> ApplySummary {
        self.damage_observer().prepare(&update);
        write_lock(&self.buffer).apply(update)
    }

    /// View-local damage bookkeeping for a session-owned live buffer. Hosts
    /// prepare every mounted view before applying the frame once to that buffer.
    #[must_use]
    pub fn damage_observer(&self) -> TerminalDamageObserver {
        TerminalDamageObserver {
            buffer: self.buffer.clone(),
            shared: self.shared.clone(),
        }
    }

    #[must_use]
    pub fn stats(&self) -> RendererStats {
        *mutex_lock(&self.shared.stats)
    }

    pub fn reset_stats(&self) {
        *mutex_lock(&self.shared.stats) = RendererStats::default();
    }

    /// Cache key without cloning the potentially multi-megabyte reading view.
    pub fn reference_revision(&self) -> (u64, i64, Option<u64>) {
        let viewport = mutex_lock(&self.shared.viewport);
        (
            read_lock(&self.buffer).generation(),
            viewport.view_offset(),
            viewport.cache_seq(),
        )
    }

    #[must_use]
    pub fn viewport(&self) -> ScrollbackViewport {
        mutex_lock(&self.shared.viewport).clone()
    }

    #[must_use]
    pub fn view_offset(&self) -> i64 {
        mutex_lock(&self.shared.viewport).view_offset()
    }

    /// How many lines the viewport can scroll back, without cloning the
    /// viewport's row cache the way `viewport()` does.
    #[must_use]
    pub fn max_view_offset(&self, visible_rows: usize) -> i64 {
        mutex_lock(&self.shared.viewport).max_offset(visible_rows)
    }

    /// [`Self::max_view_offset`] as a scroll indicator should depict it:
    /// following live, the last reported history length rather than the
    /// one-screen navigation guess.
    #[must_use]
    pub fn indicator_max_view_offset(&self, visible_rows: usize) -> i64 {
        mutex_lock(&self.shared.viewport).indicator_max_offset(visible_rows)
    }

    /// The content generation to stamp a history-length probe with, when the
    /// indicator is drawn from an estimate that a probe could correct.
    #[must_use]
    pub fn indicator_probe_generation(&self) -> Option<u64> {
        let estimated = mutex_lock(&self.shared.viewport).indicator_extent_is_estimated();
        (estimated && !self.alt_screen()).then(|| read_lock(&self.buffer).generation())
    }

    /// Adopts the history length a probe read reported.
    pub fn note_history_rows(&self, live_start_row: i64) {
        mutex_lock(&self.shared.viewport).note_history_rows(live_start_row);
    }

    /// True while the foreground program owns the whole screen, when there is
    /// no scrollback to indicate.
    #[must_use]
    pub fn alt_screen(&self) -> bool {
        mutex_lock(&self.shared.modes).alt_screen
    }

    #[must_use]
    pub fn scrolled_state(&self) -> Option<ScrolledState> {
        let offset_lines = self.view_offset();
        (offset_lines > 0).then_some(ScrolledState { offset_lines })
    }

    pub fn set_view_offset(&self, offset: i64, visible_rows: usize) -> bool {
        set_view_offset(&self.shared, &self.buffer, offset, visible_rows)
    }

    /// Keep the displayed text stable while keyboard selection owns input.
    pub fn pin_keyboard_selection(&self, pinned: bool) {
        mutex_lock(&self.shared.viewport).pin_keyboard(pinned, &read_lock(&self.buffer));
    }

    pub fn scroll_to_live(&self, visible_rows: usize) -> bool {
        self.set_view_offset(0, visible_rows)
    }

    pub fn adopt_history_geometry(&self, live_start: i64, total: i64, sequence: u64, rows: usize) {
        mutex_lock(&self.shared.viewport).apply_geometry(live_start, total, sequence, rows);
    }

    pub fn scroll_to_absolute(&self, absolute_row: i64, anchor: f32, visible_rows: usize) -> bool {
        let mut viewport = mutex_lock(&self.shared.viewport);
        let changed = viewport.scroll_to_absolute(absolute_row, anchor, visible_rows);
        viewport.hold_reading_view(&read_lock(&self.buffer));
        if changed && !viewport.is_reading() {
            mutex_lock(&self.shared.selection).clear();
        }
        changed
    }

    /// Updates daemon-owned terminal modes and reports whether entering the
    /// alternate screen also moved a scrolled viewport back to live. Either
    /// alternate-screen transition invalidates the previous screen's rows.
    pub fn set_modes(&self, alt_screen: bool, mouse: MouseModes) -> bool {
        let mut modes = mutex_lock(&self.shared.modes);
        let entered_alt = alt_screen && !modes.alt_screen;
        let alt_screen_changed = alt_screen != modes.alt_screen;
        *modes = TerminalModes { alt_screen, mouse };
        drop(modes);
        if alt_screen_changed {
            mutex_lock(&self.shared.selection).clear();
        }
        entered_alt && mutex_lock(&self.shared.viewport).enter_alt_screen()
    }

    /// True while the foreground program consumes mouse events, in which case
    /// pointer gestures belong to it rather than local selection.
    pub fn mouse_modes(&self) -> MouseModes {
        mutex_lock(&self.shared.modes).mouse
    }

    /// Resolves a wheel event and applies local scrollback movement. Daemon
    /// routes are returned for the app to pass to `SessionAttachment::scroll`.
    pub fn route_wheel(&self, event: WheelEvent) -> Option<WheelRoute> {
        self.cancel_scroll_glide();
        let modes = *mutex_lock(&self.shared.modes);
        if let WheelDelta::PrecisePoints(points) = event.delta
            && ScrollRouter::is_local(modes)
        {
            return self.scroll_by_pixels(points, event);
        }
        let route = mutex_lock(&self.shared.scroll_router).route(modes, event)?;
        match route {
            WheelRoute::Local { lines } => {
                let mut viewport = mutex_lock(&self.shared.viewport);
                let changed = viewport.scroll_by(lines, usize::from(event.visible_rows));
                viewport.hold_reading_view(&read_lock(&self.buffer));
                if changed && !viewport.is_reading() {
                    mutex_lock(&self.shared.selection).clear();
                }
            }
            // This route leaves the viewport still while the foreground
            // program may repaint beneath the selection's coordinates.
            WheelRoute::Daemon { .. } => mutex_lock(&self.shared.selection).clear(),
        }
        Some(route)
    }

    /// A trackpad moves local scrollback by the pixel. `lines` reports the
    /// whole rows crossed, which is zero for most events of a slow gesture.
    fn scroll_by_pixels(&self, points: f32, event: WheelEvent) -> Option<WheelRoute> {
        mutex_lock(&self.shared.scroll_router).reset();
        let mut viewport = mutex_lock(&self.shared.viewport);
        let before = viewport.view_offset();
        if !viewport.scroll_by_pixels(points, event.line_height, usize::from(event.visible_rows)) {
            return None;
        }
        viewport.hold_reading_view(&read_lock(&self.buffer));
        if !viewport.is_reading() {
            mutex_lock(&self.shared.selection).clear();
        }
        Some(WheelRoute::Local {
            lines: viewport.view_offset().saturating_sub(before),
        })
    }

    /// Rows scrolled back from the live edge, with the sub-row part a
    /// trackpad gesture left. For indicators; content is addressed in rows.
    #[must_use]
    pub fn scroll_position(&self) -> f64 {
        mutex_lock(&self.shared.viewport)
            .scroll_position()
            .as_rows()
    }

    /// [`Self::set_view_offset`] for a fractional position, as a dragged
    /// scroller knob produces.
    pub fn set_scroll_position(&self, rows: f64, visible_rows: usize) -> bool {
        self.cancel_scroll_glide();
        let mut viewport = mutex_lock(&self.shared.viewport);
        let position = crate::smooth_scroll::ScrollPosition::from_rows(
            rows,
            viewport.max_offset(visible_rows),
        );
        let changed = viewport.set_scroll_position(position, visible_rows);
        viewport.hold_reading_view(&read_lock(&self.buffer));
        if changed && !viewport.is_reading() {
            mutex_lock(&self.shared.selection).clear();
        }
        changed
    }

    /// How far the reading view is painted above its integral rows, in
    /// logical pixels. Pointer hit-testing adds this before dividing by the
    /// line height; it is the exact value `prepaint` translates by.
    #[must_use]
    pub fn scroll_shift(&self, line_height: Pixels, scale_factor: f32) -> Pixels {
        px(mutex_lock(&self.shared.viewport)
            .scroll_position()
            .shift(f32::from(line_height), scale_factor))
    }

    pub fn begin_scrollback_fetch(&self, visible_rows: usize) -> Option<ScrollbackRequest> {
        mutex_lock(&self.shared.viewport).begin_fetch(visible_rows)
    }

    pub fn complete_scrollback_fetch(
        &self,
        result: diri_proto::methods::ReadScrollbackCellsResult,
        visible_rows: usize,
    ) -> Result<(), ScrollbackApplyError> {
        mutex_lock(&self.shared.viewport).complete_fetch(result, visible_rows)
    }

    pub fn fail_scrollback_fetch(&self) {
        mutex_lock(&self.shared.viewport).fail_fetch();
    }

    pub fn begin_selection(&self, col: usize, window_row: usize) {
        let absolute_row = mutex_lock(&self.shared.viewport).absolute_row(window_row);
        mutex_lock(&self.shared.selection).begin(SelectionPoint {
            row: absolute_row,
            col,
        });
    }

    pub fn drag_selection(&self, col: usize, window_row: usize) {
        let viewport = mutex_lock(&self.shared.viewport);
        let buffer = read_lock(&self.buffer);
        mutex_lock(&self.shared.selection).drag_in_view(&viewport, &buffer, window_row, col);
    }

    pub fn begin_rectangle_selection(&self, col: usize, row: usize) {
        let row = mutex_lock(&self.shared.viewport).absolute_row(row);
        mutex_lock(&self.shared.selection).begin_rectangle(SelectionPoint { row, col });
    }

    pub fn select_word(&self, col: usize, window_row: usize) {
        let viewport = mutex_lock(&self.shared.viewport);
        let buffer = read_lock(&self.buffer);
        mutex_lock(&self.shared.selection).select_word(&viewport, &buffer, window_row, col);
    }

    pub fn select_line(&self, window_row: usize) {
        let viewport = mutex_lock(&self.shared.viewport);
        let buffer = read_lock(&self.buffer);
        mutex_lock(&self.shared.selection).select_line(&viewport, &buffer, window_row);
    }

    /// The user finished a selection gesture: a drag was released, or a word
    /// or line was picked by a multi-click. Starts the one-time sheen over the
    /// selection and returns whether there is anything to repaint for.
    /// Programmatic selection changes must not call this.
    pub fn complete_selection(&self) -> bool {
        let range = mutex_lock(&self.shared.selection).range();
        mutex_lock(&self.shared.selection_shimmer).begin(range)
    }

    /// Whether the sheen still has frames to draw. It is false again on the
    /// same prepaint that finds the sweep over, cancelled, or disallowed.
    #[must_use]
    pub fn selection_shimmer_running(&self) -> bool {
        mutex_lock(&self.shared.selection_shimmer).is_running()
    }

    /// Test and offline-render seam: the sheen reads this instant instead of
    /// the wall clock, so a frame is a pure function of the time given here.
    pub fn pin_selection_shimmer_clock(&self, now: Option<Instant>) {
        mutex_lock(&self.shared.selection_shimmer).pin_clock(now);
    }

    pub fn clear_selection(&self) {
        mutex_lock(&self.shared.selection).clear();
    }

    /// The web URL whose text spans the given window cell, if any.
    #[must_use]
    pub fn link_at(&self, col: usize, window_row: usize) -> Option<String> {
        match self.reference_at(col, window_row) {
            Some(TerminalReference::Url(url)) => Some(url),
            Some(TerminalReference::File(_)) | None => None,
        }
    }

    /// The command-clickable URL or file reference spanning a window cell.
    ///
    /// The full whitespace-delimited row run is inspected, so clicking a line
    /// number or punctuation wrapper resolves the same reference as clicking
    /// the path itself. URLs can continue across terminal rows or within an
    /// indented/table column; file references remain confined to one row.
    #[must_use]
    pub fn reference_at(&self, col: usize, window_row: usize) -> Option<TerminalReference> {
        self.reference_hit_at(col, window_row)
            .map(|hit| hit.reference)
    }

    pub fn reference_hit_at(&self, col: usize, window_row: usize) -> Option<ReferenceHit> {
        let viewport = mutex_lock(&self.shared.viewport);
        let buffer = read_lock(&self.buffer);
        let absolute_row = viewport.absolute_row(window_row);
        let metadata = viewport.row_metadata(&buffer, absolute_row);
        if let Some(link) = metadata
            .links
            .iter()
            .find(|link| usize::from(link.start) <= col && col < usize::from(link.end))
        {
            return reference_from_run(&link.uri).map(|reference| ReferenceHit {
                reference,
                spans: vec![(absolute_row, usize::from(link.start), usize::from(link.end))],
            });
        }
        let row = viewport.row_at_absolute(&buffer, absolute_row);
        let chars: Vec<char> = row
            .iter()
            .map(|cell| crate::selection::cell_char(*cell))
            .collect();
        if col >= chars.len() {
            return None;
        }
        let is_reference_char = |c: char| !c.is_whitespace() && c != '\0';
        if !is_reference_char(chars[col]) {
            return None;
        }
        if let Some((url, spans)) = wrapped_url_at(col, absolute_row, |row| {
            viewport.row_at_absolute(&buffer, row)
        }) {
            return Some(ReferenceHit {
                reference: TerminalReference::Url(url),
                spans,
            });
        }
        drop(buffer);
        let mut start = col;
        while start > 0 && is_reference_char(chars[start - 1]) {
            start -= 1;
        }
        let mut end = col + 1;
        while end < chars.len() && is_reference_char(chars[end]) {
            end += 1;
        }
        let candidate: String = chars[start..end].iter().collect();
        reference_from_run(&candidate).map(|reference| {
            // Underline the target text, excluding punctuation wrappers.
            let target = reference.destination();
            let prefix = candidate
                .find(target)
                .map(|byte| candidate[..byte].chars().count())
                .unwrap_or(0);
            let target_start = start + prefix;
            ReferenceHit {
                spans: vec![(
                    absolute_row,
                    target_start,
                    (target_start + target.chars().count()).min(end),
                )],
                reference,
            }
        })
    }

    #[must_use]
    pub fn selected_text(&self) -> String {
        let viewport = mutex_lock(&self.shared.viewport);
        let buffer = read_lock(&self.buffer);
        mutex_lock(&self.shared.selection).selected_text(&viewport, &buffer)
    }

    #[must_use]
    pub fn selection_range(&self) -> Option<crate::selection::SelectionRange> {
        mutex_lock(&self.shared.selection).range()
    }

    pub fn set_find_highlights(&self, spans: Vec<FindSpan>) {
        *mutex_lock(&self.shared.find_highlights) = FindHighlights {
            spans,
            retained: None,
            current_bounds: None,
        };
    }

    /// Window-space bounds of the active highlight from this element's latest
    /// prepaint. Overlay siblings must read this after the terminal prepaints,
    /// so font, clipping, and viewport changes use the same geometry as paint.
    #[must_use]
    pub fn current_find_match_bounds(&self) -> Option<Bounds<Pixels>> {
        mutex_lock(&self.shared.find_highlights).current_bounds
    }

    /// Captures the small live grid and packages it with daemon history for a
    /// lock-free background scan. No history matching happens on the GPUI
    /// thread.
    pub fn prepare_find_search(
        &self,
        model: &TerminalFindModel,
        request: &SearchRequest,
        snapshot: FindSnapshot,
    ) -> Option<SearchJob> {
        let buffer = read_lock(&self.buffer);
        model.prepare_search(request, snapshot, &buffer)
    }

    pub fn apply_find_result(&self, model: &mut TerminalFindModel, result: SearchResult) -> bool {
        model.apply_result(result, &mut mutex_lock(&self.shared.viewport))
    }

    pub fn find_next(&self, model: &mut TerminalFindModel) -> Option<NavigationTarget> {
        self.navigate_find(model, false)
    }

    pub fn find_previous(&self, model: &mut TerminalFindModel) -> Option<NavigationTarget> {
        self.navigate_find(model, true)
    }

    /// Steps to a match and, when that moves a history view, carries the view
    /// there instead of cutting to it (see [`crate::scroll_glide`]). Navigation
    /// has already pinned the capture and chosen the resting row; the glide
    /// only decides what is painted on the way. A match on the live grid
    /// returns to live at once, because the live edge is where the terminal
    /// is, not a place in the capture.
    fn navigate_find(
        &self,
        model: &mut TerminalFindModel,
        backwards: bool,
    ) -> Option<NavigationTarget> {
        let mut viewport = mutex_lock(&self.shared.viewport);
        let buffer = read_lock(&self.buffer);
        let mut state = mutex_lock(&self.shared.scroll_glide);
        // Mid-glide, what is on screen is the sample, not the last target.
        let shown = viewport.scroll_position().as_rows();
        let target = model.navigate_with_live(backwards, &mut viewport, &buffer);
        state.glide = None;
        if matches!(target, Some(NavigationTarget::History { .. })) {
            let visible_rows = usize::from(buffer.rows);
            let resting = viewport.scroll_position().as_rows();
            let now = state.now();
            if let Some(glide) =
                crate::scroll_glide::ScrollGlide::new(shown, resting, visible_rows, now)
            {
                place_glide(&mut viewport, glide.start(), visible_rows);
                state.glide = Some(glide);
            }
        }
        target
    }

    /// Drives the find glide from a caller-owned clock and schedules no
    /// frames. For tests and frame-by-frame renders.
    pub fn set_scroll_glide_clock(&self, now: Option<Instant>) {
        mutex_lock(&self.shared.scroll_glide).clock = now;
    }

    /// Whether a find glide still has frames to paint.
    #[must_use]
    pub fn scroll_glide_running(&self) -> bool {
        mutex_lock(&self.shared.scroll_glide).glide.is_some()
    }

    /// Places the view for this frame: one sample per painted frame, and
    /// Reduce Motion lands it at once. Returns whether another frame should
    /// be requested, which is only while the glide runs on the wall clock.
    fn step_scroll_glide(&self, viewport: &mut ScrollbackViewport, visible_rows: usize) -> bool {
        let mut state = mutex_lock(&self.shared.scroll_glide);
        let Some(glide) = state.glide else {
            return false;
        };
        let now = state.now();
        let finished = self.reduce_motion || glide.is_finished(now);
        let position = if finished {
            glide.target()
        } else {
            glide.sample(now)
        };
        place_glide(viewport, position, visible_rows);
        if finished {
            state.glide = None;
        }
        !finished && state.clock.is_none()
    }

    #[cfg(test)]
    pub(crate) fn step_scroll_glide_for_test(&self, visible_rows: usize) {
        let mut viewport = mutex_lock(&self.shared.viewport);
        self.step_scroll_glide(&mut viewport, visible_rows);
    }

    /// Anything else that moves the view owns it from then on.
    fn cancel_scroll_glide(&self) {
        mutex_lock(&self.shared.scroll_glide).glide = None;
    }

    pub fn sync_find_highlights(&self, model: &TerminalFindModel) {
        if let Some((source, matches, current)) = model.retained_highlights() {
            let mut highlights = mutex_lock(&self.shared.find_highlights);
            if !highlights
                .retained
                .as_ref()
                .is_some_and(|(old, old_matches, index)| {
                    old == source && old_matches == matches && *index == current
                })
            {
                highlights.retained = Some((Arc::clone(source), matches.to_vec(), current));
                highlights.current_bounds = None;
            }
        } else {
            let viewport = mutex_lock(&self.shared.viewport);
            self.set_find_highlights(
                model.visible_spans_with_live(&viewport, &read_lock(&self.buffer)),
            );
        }
    }

    pub fn clear_find_source(&self) {
        self.cancel_scroll_glide();
        mutex_lock(&self.shared.viewport).clear_find_source();
    }

    fn is_focused(&self, window: &Window) -> bool {
        self.focus_override.unwrap_or_else(|| {
            self.focus_handle
                .as_ref()
                .is_some_and(|focus| focus.is_focused(window))
                && window.is_window_active()
        })
    }

    fn shape_row(
        &self,
        row: &[GridCell],
        graphemes: &[(u16, String)],
        tints: &[Tint],
        metrics: CellMetrics,
        window: &mut Window,
    ) -> ShapedLine {
        let (text, runs) = self.row_text_and_runs(row, graphemes, tints);
        window.text_system().shape_line(
            SharedString::from(text),
            self.font_size,
            &runs,
            Some(metrics.cell_width),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn prepare_row(
        &self,
        cells: Vec<GridCell>,
        graphemes: Vec<(u16, String)>,
        tints: Vec<Tint>,
        row: u16,
        grid: SpriteGrid,
        window: &mut Window,
    ) -> CachedRow {
        let mut background_quads = Vec::new();
        let mut decoration_quads = Vec::new();
        let mut sprite_shapes = Vec::new();
        append_row_quads(
            &cells,
            row,
            grid,
            self.theme,
            &tints,
            &mut background_quads,
            &mut decoration_quads,
            &mut sprite_shapes,
        );
        let line = self.shape_row(&cells, &graphemes, &tints, grid.metrics(), window);
        CachedRow {
            cells,
            graphemes,
            tints,
            background_quads,
            decoration_quads,
            sprite_shapes,
            line,
        }
    }

    /// How many leading cells of `row` reach the shaper.
    ///
    /// Blanks paint no glyph, yet each one was shaped, stored and looked up in
    /// the glyph cache on every paint, and most rows are mostly trailing
    /// blanks. Their backgrounds and decorations are quads built from the
    /// cells, not from this text. Glyphs are positioned left to right from the
    /// ones before them, so dropping the tail cannot move what remains; one
    /// blank is kept so the last glyph still shapes as followed by a space.
    /// A row with right-to-left text is positioned from the whole line and is
    /// shaped whole.
    fn shaped_cells(&self, row: &[GridCell], graphemes: &[(u16, String)]) -> usize {
        let reorders = row.iter().any(|cell| is_bidi_sensitive(cell.scalar))
            || graphemes
                .iter()
                .any(|(_, text)| text.chars().any(|ch| is_bidi_sensitive(u32::from(ch))));
        if reorders {
            return row.len();
        }
        let last_glyph = row.iter().enumerate().rposition(|(column, cell)| {
            let visible = self.theme.resolve_cell(*cell).visible;
            render_char(*cell, visible) != ' '
                || (visible
                    && cell.scalar != 0
                    && graphemes.iter().any(|(col, _)| usize::from(*col) == column))
        });
        last_glyph.map_or(1, |last| last + 2).min(row.len())
    }

    fn row_text_and_runs(
        &self,
        row: &[GridCell],
        graphemes: &[(u16, String)],
        tints: &[Tint],
    ) -> (String, Vec<TextRun>) {
        let row = &row[..self.shaped_cells(row, graphemes)];
        let mut text = String::with_capacity(row.len());
        let mut runs = Vec::<TextRun>::new();

        let mut graphemes = graphemes.iter().peekable();
        for (column, cell) in row.iter().enumerate() {
            let resolved = self.theme.resolve_cell_under(*cell, tint_at(tints, column));
            let ch = render_char(*cell, resolved.visible);
            let mut byte_len = ch.len_utf8();
            text.push(ch);
            while graphemes
                .peek()
                .is_some_and(|(col, _)| usize::from(*col) < column)
            {
                graphemes.next();
            }
            if let Some((_, combining)) = graphemes.next_if(|(col, _)| usize::from(*col) == column)
                && resolved.visible
                && cell.scalar != 0
            {
                text.push_str(combining);
                byte_len += combining.len();
            }

            let run_font = styled_font(&self.font, resolved);
            let color = resolved.foreground.into();
            if let Some(previous) = runs.last_mut()
                && previous.font == run_font
                && previous.color == color
            {
                previous.len += byte_len;
            } else {
                runs.push(TextRun {
                    len: byte_len,
                    font: run_font,
                    color,
                    background_color: None,
                    underline: None,
                    strikethrough: None,
                });
            }
        }
        (text, runs)
    }

    fn shape_cursor_glyph(
        &self,
        cell: GridCell,
        color: gpui::Rgba,
        combining: &str,
        metrics: CellMetrics,
        window: &mut Window,
    ) -> Option<ShapedLine> {
        self.shape_glyph_under_cursor(cell, combining, color, metrics, window)
    }

    fn shape_glyph_under_cursor(
        &self,
        cell: GridCell,
        combining: &str,
        color: gpui::Rgba,
        metrics: CellMetrics,
        window: &mut Window,
    ) -> Option<ShapedLine> {
        let resolved = self.theme.resolve_cell(cell);
        let ch = render_char(cell, resolved.visible);
        if !resolved.visible || (ch == ' ' && combining.is_empty()) {
            return None;
        }
        let text = SharedString::from(format!("{ch}{combining}"));
        let run = TextRun {
            len: text.len(),
            font: styled_font(&self.font, resolved),
            color: color.into(),
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        Some(window.text_system().shape_line(
            text,
            self.font_size,
            &[run],
            Some(metrics.cell_width),
        ))
    }

    /// Applies blink and glide to the static cursor. A static frame returns
    /// the cursor untouched, so rest paints exactly what it always has.
    fn animate_cursor(
        &self,
        cursor: Option<CursorPaint>,
        focused: bool,
        metrics: CellMetrics,
        visible_cols: usize,
        window: &mut Window,
    ) -> Option<CursorPaint> {
        let mut driver = mutex_lock(&self.shared.cursor);
        let Some(mut cursor) = cursor else {
            driver.rest();
            return None;
        };
        let cell = CursorCell {
            col: cursor.col,
            row: cursor.row,
        };
        let (frame, focus) = driver.sample_in_pane(cell, focused, self.reduce_motion);
        drop(driver);
        cursor.frame = frame;
        if frame.is_static() {
            return Some(cursor);
        }
        if focus.outlined() {
            cursor.outline = Some(crate::cursor_focus::outline_quad(
                cursor.quad.bounds,
                window.scale_factor(),
                cursor.quad.background.as_solid().unwrap_or_default(),
            ));
            if frame.opacity <= 0.0 {
                // Hollow at rest: the row paints its own text and the fill
                // and inverted glyph are not there to paint.
                cursor.glyph = None;
                cursor.block = None;
                return Some(cursor);
            }
        }
        cursor.quad.bounds.origin.x += metrics.cell_width * frame.offset_cols;
        cursor.quad.bounds.origin.y += metrics.line_height * frame.offset_rows;
        cursor.quad.background = cursor.quad.background.opacity(frame.opacity);
        let cache = mutex_lock(&self.shared.row_cache);
        if !frame.is_gliding() {
            // Fading in place: the row's own text shows through the block and
            // the inverted glyph fades with it.
            if cursor.glyph.is_some()
                && let Some(prepared) = cache.get(usize::from(cursor.row)).and_then(Option::as_ref)
                && let Some(cell) = prepared.cells.get(usize::from(cursor.col)).copied()
            {
                let combining = prepared
                    .graphemes
                    .iter()
                    .find(|(col, _)| *col == cursor.col)
                    .map_or("", |(_, text)| text.as_str());
                let color = faded(cursor.text, frame.opacity);
                cursor.glyph =
                    self.shape_glyph_under_cursor(cell, combining, color, metrics, window);
            }
            drop(cache);
            return Some(cursor);
        }
        // The block overlaps at most two columns and two rows.
        let col = f32::from(cursor.col) + frame.offset_cols;
        let row = f32::from(cursor.row) + frame.offset_rows;
        for covered_row in [row.floor(), row.ceil()] {
            for covered_col in [col.floor(), col.ceil()] {
                let (covered_col, covered_row) = (covered_col as u16, covered_row as u16);
                if usize::from(covered_col) >= visible_cols
                    || cursor
                        .covered
                        .iter()
                        .any(|seen| (seen.col, seen.row) == (covered_col, covered_row))
                {
                    continue;
                }
                let Some(prepared) = cache.get(usize::from(covered_row)).and_then(Option::as_ref)
                else {
                    continue;
                };
                let Some(cell) = prepared.cells.get(usize::from(covered_col)).copied() else {
                    continue;
                };
                let combining = prepared
                    .graphemes
                    .iter()
                    .find(|(col, _)| *col == covered_col)
                    .map_or("", |(_, text)| text.as_str());
                cursor.covered.push(CoveredGlyph {
                    col: covered_col,
                    row: covered_row,
                    glyph: self.shape_cursor_glyph(cell, cursor.text, combining, metrics, window),
                    block: self
                        .theme
                        .resolve_cell(cell)
                        .visible
                        .then(|| BlockGlyph::from_scalar(cell.scalar))
                        .flatten(),
                    sprite: self
                        .theme
                        .resolve_cell(cell)
                        .visible
                        .then(|| Sprite::from_scalar(cell.scalar))
                        .flatten(),
                });
            }
        }
        Some(cursor)
    }

    fn paint_moving_cursor(
        &self,
        cursor: CursorPaint,
        bounds: Bounds<Pixels>,
        metrics: CellMetrics,
        window: &mut Window,
        cx: &mut App,
    ) {
        let block_bounds = cursor.quad.bounds;
        if cursor.frame.opacity > 0.0 {
            window.paint_quad(cursor.quad);
        }
        if cursor.frame.is_gliding() {
            let mask = ContentMask {
                bounds: block_bounds.intersect(&bounds),
            };
            window.with_content_mask(Some(mask), |window| {
                for covered in cursor.covered {
                    paint_cursor_glyph(
                        covered.glyph.as_ref(),
                        covered.block,
                        covered.sprite,
                        (covered.col, covered.row),
                        cursor.text,
                        bounds,
                        metrics,
                        window,
                        cx,
                    );
                }
            });
            return;
        }
        paint_cursor_glyph(
            cursor.glyph.as_ref(),
            cursor.block,
            cursor.sprite,
            (cursor.col, cursor.row),
            faded(cursor.text, cursor.frame.opacity),
            bounds,
            metrics,
            window,
            cx,
        );
        if let Some(outline) = cursor.outline {
            window.paint_quad(outline);
        }
    }
}

fn faded(color: gpui::Rgba, opacity: f32) -> gpui::Rgba {
    gpui::Rgba {
        a: color.a * opacity,
        ..color
    }
}

/// The inverted glyph of one cell: block elements and sprites as geometry,
/// everything else as shaped text, all at the cell's own origin.
#[allow(clippy::too_many_arguments)]
fn paint_cursor_glyph(
    glyph: Option<&ShapedLine>,
    block: Option<BlockGlyph>,
    sprite: Option<Sprite>,
    (col, row): (u16, u16),
    color: gpui::Rgba,
    bounds: Bounds<Pixels>,
    metrics: CellMetrics,
    window: &mut Window,
    cx: &mut App,
) {
    if let Some(block) = block {
        for rect in block.rectangles(bounds.origin, metrics, usize::from(col), row) {
            window.paint_quad(fill(rect, color));
        }
    }
    if let Some(sprite) = sprite {
        let grid = SpriteGrid::new(bounds.origin, metrics, window.scale_factor());
        grid.paint(sprite, grid.cell(usize::from(col), row), color, window);
    }
    if let Some(glyph) = glyph {
        let origin = point(
            bounds.left() + metrics.x_for_col(col),
            bounds.top() + metrics.y_for_row(row),
        );
        let _ = glyph.paint(
            origin,
            metrics.line_height,
            TextAlign::Left,
            None,
            window,
            cx,
        );
    }
}

fn note_keystroke(cursor: &Mutex<CursorDriver>) -> bool {
    mutex_lock(cursor).note_keystroke()
}

fn note_cursor_damage(
    cursor: &Mutex<CursorDriver>,
    buffer: &GridBuffer,
    update: &GridUpdate,
    replaces_grid: bool,
) {
    let previous = buffer.cursor.visible.then_some(CursorCell {
        col: buffer.cursor.col,
        row: buffer.cursor.row,
    });
    let next = update.cursor_visible.then_some(CursorCell {
        col: update.cursor_col,
        row: update.cursor_row,
    });
    let mut cursor = mutex_lock(cursor);
    let now = cursor.now();
    cursor.motion.note_damage(
        CursorDamage {
            previous,
            next,
            rows_damaged: if replaces_grid {
                usize::MAX
            } else {
                update.changed_rows.len()
            },
            touches_cursor_row: replaces_grid
                || update
                    .changed_rows
                    .iter()
                    .any(|changed| changed.y == update.cursor_row),
        },
        now,
    );
}

#[cfg(target_os = "macos")]
fn default_terminal_font() -> Font {
    let mut terminal_font = font(".SF NS Mono");
    terminal_font.fallbacks = Some(FontFallbacks::from_fonts(
        [
            "Menlo",
            "Apple Symbols",
            "STIX Two Math",
            "Apple Color Emoji",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect(),
    ));
    terminal_font
}

#[cfg(not(target_os = "macos"))]
fn default_terminal_font() -> Font {
    let mut terminal_font = font("monospace");
    terminal_font.fallbacks = Some(FontFallbacks::from_fonts(
        [
            "Noto Sans Mono",
            "DejaVu Sans Mono",
            "Noto Sans Symbols 2",
            "STIX Two Math",
            "Noto Color Emoji",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect(),
    ));
    terminal_font
}

impl IntoElement for TerminalElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for TerminalElement {
    type RequestLayoutState = ();
    type PrepaintState = TerminalPrepaintState;

    fn id(&self) -> Option<ElementId> {
        Some(ElementId::NamedInteger(
            SharedString::new_static("terminal-grid"),
            self.shared.id,
        ))
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = relative(1.0).into();
        style.size.height = relative(1.0).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        if self.suspended {
            mutex_lock(&self.shared.find_highlights).current_bounds = None;
            mutex_lock(&self.shared.row_cache).clear();
            mutex_lock(&self.shared.render_generations).clear();
            *mutex_lock(&self.shared.render_context) = None;
            return TerminalPrepaintState {
                started_at: None,
                background_quads: Vec::new(),
                decoration_quads: Vec::new(),
                sprite_shapes: Vec::new(),
                overlay_quads: Vec::new(),
                selection: SelectionPaint::default(),
                lines: Vec::new(),
                metrics: None,
                cursor: None,
                cache_hits: 0,
                cache_misses: 0,
                paint_from_cache: false,
                scroll: None,
            };
        }

        let (grid_cols, grid_rows, grid_is_empty) = {
            let buffer = read_lock(&self.buffer);
            (buffer.cols, buffer.rows, buffer.cells.is_empty())
        };
        let focused = self.is_focused(window);

        if grid_is_empty {
            mutex_lock(&self.shared.find_highlights).current_bounds = None;
            return TerminalPrepaintState {
                started_at: None,
                background_quads: Vec::new(),
                decoration_quads: Vec::new(),
                sprite_shapes: Vec::new(),
                overlay_quads: Vec::new(),
                selection: SelectionPaint::default(),
                lines: Vec::new(),
                metrics: None,
                cursor: None,
                cache_hits: 0,
                cache_misses: 0,
                paint_from_cache: false,
                scroll: None,
            };
        }

        let started_at = Instant::now();
        let font_size_bits = f32::from(self.font_size).to_bits();
        let metrics = {
            let mut cached = mutex_lock(&self.shared.metrics);
            if let Some((cached_font, cached_size, metrics)) = cached.as_ref()
                && cached_font == &self.font
                && *cached_size == font_size_bits
            {
                *metrics
            } else {
                let font_id = window.text_system().resolve_font(&self.font);
                let metrics =
                    CellMetrics::measure_font(window.text_system(), font_id, self.font_size);
                *cached = Some((self.font.clone(), font_size_bits, metrics));
                metrics
            }
        };
        let visible_rows =
            usize::from(grid_rows).min(usize::from(metrics.rows_for_height(bounds.size.height)));
        let visible_cols =
            usize::from(grid_cols).min(usize::from(metrics.cols_for_width(bounds.size.width)));
        // Hold the viewport lock for the whole prepaint instead of deep-cloning
        // it: every party that touches these mutexes runs on the main thread,
        // and the clone copied the entire fetched-history cell cache per frame.
        let mut viewport = mutex_lock(&self.shared.viewport);
        if self.step_scroll_glide(&mut viewport, visible_rows) {
            window.request_animation_frame();
        }
        let viewport = viewport;
        // Zero on the live grid and on a reading view resting on a whole row,
        // where `painted_rows` is `visible_rows` and nothing below differs.
        let scroll_shift = px(viewport
            .scroll_position()
            .shift(f32::from(metrics.line_height), window.scale_factor()));
        let painted_rows = visible_rows + usize::from(scroll_shift > px(0.0));
        let mut background_quads = vec![fill(
            bounds,
            self.theme.background.alpha(self.background_opacity),
        )];
        let mut decoration_quads = Vec::new();
        let mut sprite_shapes = Vec::new();
        let mut overlay_quads = Vec::new();
        let mut lines = Vec::with_capacity(visible_rows);
        let grid = SpriteGrid::new(bounds.origin, metrics, window.scale_factor());
        let cursor;
        let cache_hits;
        let cache_misses;
        let mut paint_from_cache = false;

        // Tints are gathered before any row is shaped: a glyph under a
        // selection or find highlight is colored to stay readable against it.
        // The spans are the ones the selection shape is built from, so a glyph
        // is recolored exactly when the tint covers it, double-width ones too.
        // `painted_rows` includes the partial row a sub-row scroll brings in.
        let (selected, ..) = self.snapped_selection(&viewport, painted_rows, visible_cols);
        let mut row_tints = vec![Vec::new(); painted_rows];
        for span in &selected {
            if let Some(tints) = row_tints.get_mut(span.row) {
                tints.push(Tint {
                    start: span.start_col,
                    end: span.end_col_exclusive,
                    color: self.theme.selection,
                });
            }
        }
        {
            let buffer = read_lock(&self.buffer);
            let mut highlights = mutex_lock(&self.shared.find_highlights);
            if let Some((source, matches, current)) = &highlights.retained {
                let pinned = viewport.has_find_source(source);
                let top = if pinned {
                    viewport.absolute_row(0)
                } else {
                    source.live_start_row
                };
                highlights.spans = matches
                    .iter()
                    .enumerate()
                    .filter_map(|(index, item)| {
                        if !pinned
                            && (!source.matches_live_row(item.absolute_row, &buffer)
                                || viewport.is_reading())
                        {
                            return None;
                        }
                        let row = usize::try_from(item.absolute_row.checked_sub(top)?).ok()?;
                        (row < painted_rows).then_some(FindSpan {
                            row,
                            start_col: item.start_col,
                            end_col_exclusive: item.end_col_exclusive,
                            is_current: index == *current,
                        })
                    })
                    .collect();
            }
            for span in &highlights.spans {
                if let Some(tints) = row_tints.get_mut(span.row) {
                    tints.push(Tint {
                        start: span.start_col,
                        end: span.end_col_exclusive,
                        color: if span.is_current {
                            self.theme.find_match_current
                        } else {
                            self.theme.find_match
                        },
                    });
                }
            }
        }

        if viewport.is_reading() {
            // History browsing composes owned rows per frame; quads are cheap
            // arithmetic, but shaping is not, so shaped lines are reused from
            // the absolute-row cache. Returning live still forces one complete
            // live-cache re-seed.
            *mutex_lock(&self.shared.render_context) = None;
            let buffer = read_lock(&self.buffer);
            cursor = buffer.cursor;
            let key = HistoryShapeKey {
                theme_signature: self.theme.signature(),
                font_id: metrics.font_id,
                font_size_bits: f32::from(self.font_size).to_bits(),
                cell_width_bits: f32::from(metrics.cell_width).to_bits(),
                visible_cols,
            };
            let mut history = mutex_lock(&self.shared.history_lines);
            history.validate(key, viewport.absolute_row(0));
            let mut hits = 0u64;
            let mut cells = Vec::with_capacity(usize::from(grid_cols));
            for (row_index, tints) in row_tints.iter().enumerate() {
                let absolute = viewport.absolute_row(row_index);
                viewport.window_row_into(&buffer, row_index, &mut cells);
                cells.truncate(visible_cols);
                append_row_quads(
                    &cells,
                    row_index as u16,
                    grid,
                    self.theme,
                    tints,
                    &mut background_quads,
                    &mut decoration_quads,
                    &mut sprite_shapes,
                );
                let graphemes = viewport.row_graphemes(&buffer, absolute);
                let digest = digest_row(&cells, graphemes, tints);
                if history.get(absolute, digest).is_some() {
                    hits += 1;
                } else {
                    // A row the viewport has not fetched yet composes as
                    // blank. Caching it is safe now that entries are content
                    // addressed: the blank's digest stops matching the moment
                    // the fetch lands. The same holds for the held live rows
                    // under the history, which used to be reshaped and copied
                    // every frame.
                    let line = self.shape_row(&cells, graphemes, tints, metrics, window);
                    history.insert(absolute, digest, line);
                }
                lines.push((row_index as u16, absolute));
            }
            cache_hits = hits;
            cache_misses = (painted_rows as u64).saturating_sub(hits);
        } else {
            mutex_lock(&self.shared.history_lines).release();
            let context = RowRenderContext {
                theme_signature: self.theme.signature(),
                font_id: metrics.font_id,
                font_size_bits: f32::from(self.font_size).to_bits(),
                cell_width_bits: f32::from(metrics.cell_width).to_bits(),
                line_height_bits: f32::from(metrics.line_height).to_bits(),
                origin_x_bits: f32::from(bounds.origin.x).to_bits(),
                origin_y_bits: f32::from(bounds.origin.y).to_bits(),
                scale_bits: window.scale_factor().to_bits(),
                visible_cols,
                visible_rows,
            };
            let mut remembered_context = mutex_lock(&self.shared.render_context);
            let mut force = remembered_context.as_ref() != Some(&context);
            *remembered_context = Some(context);
            drop(remembered_context);
            {
                let cache = mutex_lock(&self.shared.row_cache);
                force |= cache.len() < visible_rows
                    || cache.iter().take(visible_rows).any(Option::is_none);
            }
            let damage = {
                let buffer = read_lock(&self.buffer);
                let mut generations = mutex_lock(&self.shared.render_generations);
                buffer.snapshot_damage(&mut generations, visible_rows, visible_cols, force)
            };
            cursor = damage.cursor;
            let mut misses = 0;

            let mut cache = mutex_lock(&self.shared.row_cache);
            cache.truncate(visible_rows);
            cache.resize_with(visible_rows, || None);
            let offset = if force {
                0
            } else {
                align_scrolled_rows(&mut cache, &damage.changed_rows)
            };
            for changed in damage.changed_rows {
                if !force
                    && let Some(prepared) = cache[changed.row].as_mut()
                    && prepared.cells == changed.cells
                    && prepared.graphemes == changed.graphemes
                    && prepared.tints == row_tints[changed.row]
                {
                    // Shapes are independent of row position. Backgrounds and
                    // decorations carry absolute bounds and must move with it.
                    let old_row = (changed.row + offset) % visible_rows;
                    let dy =
                        metrics.y_for_row(changed.row as u16) - metrics.y_for_row(old_row as u16);
                    if prepared.move_vertically(dy, grid) {
                        continue;
                    }
                }
                misses += 1;
                cache[changed.row] = Some(self.prepare_row(
                    changed.cells,
                    changed.graphemes,
                    row_tints[changed.row].clone(),
                    changed.row as u16,
                    grid,
                    window,
                ));
            }
            // A selection drag or find step changes tints on rows whose cells
            // did not change; only those rows are prepared again.
            for (row, tints) in row_tints.into_iter().enumerate() {
                let Some(prepared) = cache[row].as_ref() else {
                    continue;
                };
                if prepared.tints == tints {
                    continue;
                }
                misses += 1;
                let (cells, graphemes) = (prepared.cells.clone(), prepared.graphemes.clone());
                cache[row] =
                    Some(self.prepare_row(cells, graphemes, tints, row as u16, grid, window));
            }
            cache_misses = misses;
            cache_hits = visible_rows as u64 - misses;
            // No composed copies: paint reads the row cache directly (see
            // `paint_from_cache`), so a frame with zero changed rows clones
            // nothing — previously every prepaint re-cloned all rows' quads
            // and shaped lines even on a 100% cache hit.
            paint_from_cache = true;
        }

        // `painted_rows` covers the partial row a sub-row scroll brings in, so
        // the shape reaches into it like every other layer.
        let mut selection = self.prepare_selection(
            &viewport,
            painted_rows,
            visible_cols,
            bounds,
            metrics,
            &mut overlay_quads,
            cx,
        );
        if let Some(marker) = self.seen_marker
            && !mutex_lock(&self.shared.modes).alt_screen
        {
            let top = if viewport.is_reading() {
                viewport.geometry_known().then(|| viewport.absolute_row(0))
            } else {
                Some(marker.live_start_row)
            };
            // `painted_rows` reaches into the partial row a sub-row scroll
            // brings in; the line then travels with `overlay_quads`.
            if let Some(row) = top.and_then(|top| marker.window_row(top, painted_rows)) {
                // The palette's red is the theme's own "unread" color.
                overlay_quads.push(fill(
                    Bounds::new(
                        point(bounds.left(), bounds.top() + metrics.y_for_row(row)),
                        size(bounds.size.width, px(1.0)),
                    ),
                    self.theme.ansi[1].opacity(0.7),
                ));
            }
        }
        if let Some(hit) = &self.hovered_reference {
            let top = viewport.absolute_row(0);
            for &(row, start, end) in &hit.spans {
                if row < top || row >= top + painted_rows as i64 {
                    continue;
                }
                let origin = point(
                    bounds.left() + metrics.cell_width * start as f32,
                    bounds.top() + metrics.line_height * (row - top + 1) as f32 - px(2.0),
                );
                overlay_quads.push(fill(
                    Bounds::new(
                        origin,
                        size(
                            metrics.cell_width * end.saturating_sub(start) as f32,
                            px(1.0),
                        ),
                    ),
                    self.theme.foreground,
                ));
            }
        }
        let mut highlights = mutex_lock(&self.shared.find_highlights);
        highlights.current_bounds = highlights.spans.iter().find_map(|span| {
            if !span.is_current || span.row >= painted_rows {
                return None;
            }
            let start = span.start_col.min(visible_cols);
            let end = span.end_col_exclusive.min(visible_cols);
            (end > start).then(|| {
                Bounds::new(
                    point(
                        bounds.left() + metrics.cell_width * start as f32,
                        bounds.top() + metrics.line_height * span.row as f32 - scroll_shift,
                    ),
                    size(
                        metrics.cell_width * (end - start) as f32,
                        metrics.line_height,
                    ),
                )
            })
        });
        for span in highlights.spans.iter().copied() {
            append_overlay_quad(
                span.row,
                span.start_col,
                span.end_col_exclusive,
                bounds.origin,
                metrics,
                if span.is_current {
                    self.theme.find_match_current
                } else {
                    self.theme.find_match
                },
                &mut overlay_quads,
            );
        }

        drop(highlights);

        let cursor = if cursor_should_render(!self.cursor_hidden, cursor.visible)
            && !viewport.is_reading()
            && usize::from(cursor.row) < visible_rows
            && usize::from(cursor.col) < visible_cols
        {
            let cache = mutex_lock(&self.shared.row_cache);
            let cell = cache[usize::from(cursor.row)]
                .as_ref()
                .and_then(|row| row.cells.get(usize::from(cursor.col)))
                .copied()
                .unwrap_or(GridCell::BLANK);
            let cursor_cols = cache[usize::from(cursor.row)].as_ref().map_or(1, |row| {
                crate::cursor_focus::cursor_cols(&row.cells, cursor.col)
            });
            let origin = point(
                bounds.left() + metrics.x_for_col(cursor.col),
                bounds.top() + metrics.y_for_row(cursor.row),
            );
            let cursor_tint = cache[usize::from(cursor.row)]
                .as_ref()
                .and_then(|row| tint_at(&row.tints, usize::from(cursor.col)));
            let (cursor_fill, cursor_text) = self.theme.cursor_colors(cell, cursor_tint);
            let visible = self.theme.resolve_cell(cell).visible;
            Some(CursorPaint {
                row: cursor.row,
                col: cursor.col,
                cols: cursor_cols,
                quad: fill(
                    Bounds::new(
                        origin,
                        size(
                            metrics.cell_width * f32::from(cursor_cols),
                            metrics.line_height,
                        ),
                    ),
                    cursor_fill,
                ),
                outline: None,
                text: cursor_text,
                glyph: self.shape_cursor_glyph(
                    cell,
                    cursor_text,
                    cache[usize::from(cursor.row)]
                        .as_ref()
                        .and_then(|row| row.graphemes.iter().find(|(col, _)| *col == cursor.col))
                        .map_or("", |(_, text)| text.as_str()),
                    metrics,
                    window,
                ),
                block: visible
                    .then(|| BlockGlyph::from_scalar(cell.scalar))
                    .flatten(),
                sprite: visible.then(|| Sprite::from_scalar(cell.scalar)).flatten(),
                frame: CursorFrame::REST,
                covered: Vec::new(),
            })
        } else {
            None
        };
        let cursor = self.animate_cursor(cursor, focused, metrics, visible_cols, window);

        let scroll = (scroll_shift > px(0.0)).then(|| {
            let backdrop = background_quads.remove(0);
            for quad in background_quads
                .iter_mut()
                .chain(&mut overlay_quads)
                .chain(&mut decoration_quads)
            {
                quad.bounds.origin.y -= scroll_shift;
            }
            for shape in &mut sprite_shapes {
                shape.move_vertically(-scroll_shift);
            }
            // A stepped selection is a path rather than a quad, so it is not
            // in the vectors above; its sheen ramps are.
            selection.shift_up(scroll_shift);
            ScrollPaint {
                shift: scroll_shift,
                backdrop,
                clip: Bounds::new(
                    bounds.origin,
                    size(bounds.size.width, metrics.line_height * visible_rows as f32),
                ),
            }
        });

        TerminalPrepaintState {
            started_at: Some(started_at),
            background_quads,
            decoration_quads,
            sprite_shapes,
            overlay_quads,
            selection,
            lines,
            metrics: Some(metrics),
            cursor,
            cache_hits,
            cache_misses,
            paint_from_cache,
            scroll,
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        if let (Some(focus_handle), Some(text_input)) = (&self.focus_handle, &self.text_input) {
            let (cursor_bounds, cell_width) = match (prepaint.metrics, prepaint.cursor.as_ref()) {
                (Some(metrics), Some(cursor)) => (
                    Bounds::new(
                        point(
                            bounds.left() + metrics.x_for_col(cursor.col),
                            bounds.top() + metrics.y_for_row(cursor.row),
                        ),
                        size(metrics.cell_width, metrics.line_height),
                    ),
                    metrics.cell_width,
                ),
                (Some(metrics), None) => (
                    Bounds::new(bounds.origin, size(metrics.cell_width, metrics.line_height)),
                    metrics.cell_width,
                ),
                (None, _) => (
                    Bounds::new(bounds.origin, size(px(1.0), self.font_size * 1.4)),
                    px(1.0),
                ),
            };
            window.handle_input(
                focus_handle,
                TerminalInputHandler {
                    text_input: Arc::clone(text_input),
                    ime_state: Arc::clone(&self.ime_state),
                    view: self.damage_observer(),
                    cursor_bounds,
                    cell_width,
                    cursor: Arc::clone(&self.shared.cursor),
                },
                cx,
            );
        }
        let Some(metrics) = prepaint.metrics else {
            return;
        };

        let scroll_shift = prepaint
            .scroll
            .as_ref()
            .map_or(px(0.0), |scroll| scroll.shift);
        let content = prepaint
            .scroll
            .as_ref()
            .map_or(bounds, |scroll| scroll.clip);
        if let Some(scroll) = &prepaint.scroll {
            window.with_content_mask(Some(ContentMask { bounds }), |window| {
                window.paint_quad(scroll.backdrop.clone());
            });
        }
        window.with_content_mask(Some(ContentMask { bounds: content }), |window| {
            // Live path: rows come straight from the shared cache. Quads are
            // plain structs (a stack copy each) and `ShapedLine::paint` takes
            // a reference, so nothing per-row is heap-cloned per frame.
            let cache = prepaint
                .paint_from_cache
                .then(|| mutex_lock(&self.shared.row_cache));

            // Outside a layer every quad is inserted into the scene's bounds
            // tree to be given its own draw order; a row of block elements
            // pays that per cell. A layer gives its contents one order, and
            // the scene's stable sort keeps quads of equal order in insertion
            // order, which is the order overlapping quads already had. Each
            // layer spans the terminal, so the passes stack as they did:
            // backgrounds, overlays, text, decorations, cursor.
            //
            // Glyphs stay outside: sprites of equal order are drawn sorted by
            // atlas tile, which reorders overlapping ink, and measured no
            // faster than per-glyph ordering.
            let selection_path = prepaint.selection.take_path();
            window.paint_layer(bounds, |window| {
                for quad in prepaint.background_quads.drain(..) {
                    window.paint_quad(quad);
                }
                if let Some(cache) = &cache {
                    for prepared in cache.iter().flatten() {
                        for quad in &prepared.background_quads {
                            window.paint_quad(quad.clone());
                        }
                    }
                }
                if selection_path.is_none() {
                    for quad in prepaint.overlay_quads.drain(..) {
                        window.paint_quad(quad);
                    }
                }
            });
            // A stepped selection is one path, so its translucent tint covers
            // every pixel once. Paths sort after the quads of their layer, so
            // the remaining overlays move to a layer of their own to stay on
            // top of it.
            if let Some(path) = selection_path {
                window.paint_path(path, self.theme.selection);
                window.paint_layer(bounds, |window| {
                    for quad in prepaint.overlay_quads.drain(..) {
                        window.paint_quad(quad);
                    }
                });
            }
            prepaint.selection.request_frame(window);

            if let Some(cache) = &cache {
                for (row_index, prepared) in cache.iter().enumerate() {
                    let Some(prepared) = prepared else { continue };
                    let row = row_index as u16;
                    let origin = point(bounds.left(), bounds.top() + metrics.y_for_row(row));
                    if prepaint
                        .cursor
                        .as_ref()
                        .is_some_and(|cursor| cursor.replaces_text_at(row))
                    {
                        let cursor = prepaint.cursor.as_ref().unwrap();
                        paint_line_around_cursor(
                            &prepared.line,
                            origin,
                            bounds,
                            metrics,
                            (cursor.col, cursor.cols),
                            window,
                            cx,
                        );
                    } else {
                        let _ = prepared.line.paint(
                            origin,
                            metrics.line_height,
                            TextAlign::Left,
                            None,
                            window,
                            cx,
                        );
                    }
                }
                window.paint_layer(bounds, |window| {
                    for prepared in cache.iter().flatten() {
                        for quad in &prepared.decoration_quads {
                            window.paint_quad(quad.clone());
                        }
                        for shape in &prepared.sprite_shapes {
                            shape.paint(window);
                        }
                    }
                });
            }

            // Reading path: shapes stay in the history cache, which only
            // prepaint evicts, so a frame copies no `ShapedLine` (about 3 KB
            // each, inline).
            let history =
                (!prepaint.lines.is_empty()).then(|| mutex_lock(&self.shared.history_lines));
            let lines = prepaint.lines.iter().filter_map(|(row, absolute)| {
                let (_, line) = history.as_ref()?.lines.get(absolute)?;
                Some((row, line))
            });
            for (row, line) in lines {
                let origin = point(
                    bounds.left(),
                    bounds.top() + metrics.y_for_row(*row) - scroll_shift,
                );
                if prepaint
                    .cursor
                    .as_ref()
                    .is_some_and(|cursor| cursor.replaces_text_at(*row))
                {
                    let cursor = prepaint.cursor.as_ref().unwrap();
                    paint_line_around_cursor(
                        line,
                        origin,
                        bounds,
                        metrics,
                        (cursor.col, cursor.cols),
                        window,
                        cx,
                    );
                } else {
                    let _ = line.paint(
                        origin,
                        metrics.line_height,
                        TextAlign::Left,
                        None,
                        window,
                        cx,
                    );
                }
            }

            window.paint_layer(bounds, |window| {
                for quad in prepaint.decoration_quads.drain(..) {
                    window.paint_quad(quad);
                }
                for shape in prepaint.sprite_shapes.drain(..) {
                    shape.paint(window);
                }
            });

            let cursor_schedule = prepaint
                .cursor
                .as_ref()
                .map_or(CursorSchedule::Rest, |cursor| cursor.frame.schedule);
            let moving_cursor = prepaint.cursor.take_if(|cursor| !cursor.frame.is_static());
            if let Some(cursor) = moving_cursor {
                self.paint_moving_cursor(cursor, bounds, metrics, window, cx);
            }
            if cursor_schedule != CursorSchedule::Rest {
                crate::cursor_motion::request_frame(
                    &self.shared.cursor,
                    cursor_schedule,
                    window,
                    cx,
                );
            }
            if let Some(cursor) = prepaint.cursor.take() {
                window.paint_quad(cursor.quad);
                if let Some(block) = cursor.block {
                    for rect in block.rectangles(
                        bounds.origin,
                        metrics,
                        usize::from(cursor.col),
                        cursor.row,
                    ) {
                        window.paint_quad(fill(rect, cursor.text));
                    }
                }
                if let Some(sprite) = cursor.sprite {
                    let grid = SpriteGrid::new(bounds.origin, metrics, window.scale_factor());
                    let cell = grid.cell(usize::from(cursor.col), cursor.row);
                    grid.paint(sprite, cell, cursor.text, window);
                }
                if let Some(glyph) = cursor.glyph {
                    let origin = point(
                        bounds.left() + metrics.x_for_col(cursor.col),
                        bounds.top() + metrics.y_for_row(cursor.row),
                    );
                    let _ = glyph.paint(
                        origin,
                        metrics.line_height,
                        TextAlign::Left,
                        None,
                        window,
                        cx,
                    );
                }
            }
        });

        if let Some(started_at) = prepaint.started_at {
            let elapsed = started_at.elapsed();
            let mut stats = mutex_lock(&self.shared.stats);
            stats.frames = stats.frames.saturating_add(1);
            stats.total_frame_time = stats.total_frame_time.saturating_add(elapsed);
            stats.max_frame_time = stats.max_frame_time.max(elapsed);
            stats.shape_cache_hits = stats.shape_cache_hits.saturating_add(prepaint.cache_hits);
            stats.shape_cache_misses = stats
                .shape_cache_misses
                .saturating_add(prepaint.cache_misses);
        }
    }
}

/// Focus no longer decides whether the cursor is painted, only how: filled in
/// the pane that holds the keyboard, outlined in every other. A cursor the
/// program hid (DECTCEM) stays hidden in both, and a thumbnail has none.
const fn cursor_should_render(host_shows_cursor: bool, protocol_visible: bool) -> bool {
    host_shows_cursor && protocol_visible
}

fn append_background_quads(
    row: &[GridCell],
    row_index: u16,
    origin: Point<Pixels>,
    metrics: CellMetrics,
    theme: TermTheme,
    quads: &mut Vec<PaintQuad>,
) {
    let mut col = 0;
    while col < row.len() {
        let cell = row[col];
        let inverse = cell.style.contains(diri_proto::grid::TermStyle::INVERSE);
        if !inverse && is_default_background(cell.bg) {
            col += 1;
            continue;
        }
        let color = theme.resolve_cell(cell).background;
        let mut end = col + 1;
        while end < row.len() {
            let next = row[end];
            let next_inverse = next.style.contains(diri_proto::grid::TermStyle::INVERSE);
            if next_inverse != inverse || theme.resolve_cell(next).background != color {
                break;
            }
            end += 1;
        }
        let quad_origin = point(
            origin.x + metrics.cell_width * col as f32,
            origin.y + metrics.y_for_row(row_index),
        );
        quads.push(fill(
            Bounds::new(
                quad_origin,
                size(metrics.cell_width * (end - col) as f32, metrics.line_height),
            ),
            color,
        ));
        col = end;
    }
}

#[allow(clippy::too_many_arguments)]
fn append_row_quads(
    row: &[GridCell],
    row_index: u16,
    grid: SpriteGrid,
    theme: TermTheme,
    tints: &[Tint],
    background_quads: &mut Vec<PaintQuad>,
    decoration_quads: &mut Vec<PaintQuad>,
    sprite_shapes: &mut Vec<AntialiasedShape>,
) {
    let (origin, metrics) = (grid.origin(), grid.metrics());
    // Plain terminal output is overwhelmingly default-background text with
    // no decorations. Recognize the entire row in one cheap pass instead of
    // scanning it once for backgrounds and again for decorations.
    let is_plain = row.iter().all(|cell| {
        !cell.style.contains(diri_proto::grid::TermStyle::INVERSE)
            && is_default_background(cell.bg)
            && !cell.style.contains(diri_proto::grid::TermStyle::UNDERLINE)
            && !cell
                .style
                .contains(diri_proto::grid::TermStyle::CROSSED_OUT)
            && !is_procedural(cell.scalar)
    });
    if is_plain {
        return;
    }
    append_background_quads(row, row_index, origin, metrics, theme, background_quads);
    // Keep blocks and sprites in the foreground layer, above selection/search
    // backgrounds and below the cursor. The same path serves cached live rows
    // and history.
    let mut col = 0;
    while col < row.len() {
        let cell = row[col];
        let start = col;
        col += 1;
        if !is_procedural(cell.scalar) {
            continue;
        }
        let style = theme.resolve_cell_under(cell, tint_at(tints, start));
        if !style.visible {
            continue;
        }
        let continues = |(next, column): (&GridCell, usize)| {
            let next_style = theme.resolve_cell_under(*next, tint_at(tints, column));
            next.scalar == cell.scalar
                && next_style.visible
                && next_style.foreground == style.foreground
        };
        let Some(block) = BlockGlyph::from_scalar(cell.scalar) else {
            let Some(sprite) = Sprite::from_scalar(cell.scalar) else {
                continue;
            };
            let mut bounds = grid.cell(start, row_index);
            if sprite.spans_cell_width() {
                // A rule is one stroke repeated: the run is the same strokes
                // across one wider cell, on the same snapped edges.
                while row.get(col).map(|next| (next, col)).is_some_and(continues) {
                    col += 1;
                }
                bounds.right = grid.x(col);
            }
            grid.append(
                sprite,
                bounds,
                style.foreground,
                decoration_quads,
                sprite_shapes,
            );
            continue;
        };
        let mut rectangles = block.rectangles(origin, metrics, start, row_index);
        if block.spans_cell_width()
            && let Some(mut bar) = rectangles.next()
        {
            // A progress bar is one block repeated; paint the run as one quad
            // for as long as that is provably the same pixels.
            while row.get(col).map(|next| (next, col)).is_some_and(continues)
                && let Some(joined) = block
                    .rectangles(origin, metrics, col, row_index)
                    .next()
                    .and_then(|bounds| crate::blocks::join_horizontally(bar, bounds))
            {
                bar = joined;
                col += 1;
            }
            decoration_quads.push(fill(bar, style.foreground));
        } else {
            decoration_quads.extend(rectangles.map(|bounds| fill(bounds, style.foreground)));
        }
    }
    append_decoration_quads(
        row,
        row_index,
        origin,
        metrics,
        theme,
        tints,
        decoration_quads,
    );
}

fn append_decoration_quads(
    row: &[GridCell],
    row_index: u16,
    origin: Point<Pixels>,
    metrics: CellMetrics,
    theme: TermTheme,
    tints: &[Tint],
    quads: &mut Vec<PaintQuad>,
) {
    let mut col = 0;
    while col < row.len() {
        let style = theme.resolve_cell_under(row[col], tint_at(tints, col));
        if !style.underline && !style.strikethrough {
            col += 1;
            continue;
        }
        let mut end = col + 1;
        while end < row.len() {
            let next = theme.resolve_cell_under(row[end], tint_at(tints, end));
            if next.foreground != style.foreground
                || next.underline != style.underline
                || next.strikethrough != style.strikethrough
            {
                break;
            }
            end += 1;
        }
        let x = origin.x + metrics.cell_width * col as f32;
        let width = metrics.cell_width * (end - col) as f32;
        let row_top = origin.y + metrics.y_for_row(row_index);
        if style.underline {
            quads.push(fill(
                Bounds::new(
                    point(x, row_top + metrics.line_height - px(1.5)),
                    size(width, px(1.0)),
                ),
                style.foreground,
            ));
        }
        if style.strikethrough {
            quads.push(fill(
                Bounds::new(
                    point(x, row_top + metrics.line_height * 0.55),
                    size(width, px(1.0)),
                ),
                style.foreground,
            ));
        }
        col = end;
    }
}

fn append_overlay_quad(
    row: usize,
    start_col: usize,
    end_col_exclusive: usize,
    origin: Point<Pixels>,
    metrics: CellMetrics,
    color: gpui::Rgba,
    quads: &mut Vec<PaintQuad>,
) {
    let start_col = start_col.min(usize::from(u16::MAX));
    let end_col_exclusive = end_col_exclusive.min(usize::from(u16::MAX));
    let row = row.min(usize::from(u16::MAX));
    if start_col >= end_col_exclusive {
        return;
    }
    quads.push(fill(
        Bounds::new(
            point(
                origin.x + metrics.cell_width * start_col as f32,
                origin.y + metrics.y_for_row(row as u16),
            ),
            size(
                metrics.cell_width * (end_col_exclusive - start_col) as f32,
                metrics.line_height,
            ),
        ),
        color,
    ));
}

fn paint_line_around_cursor(
    line: &ShapedLine,
    origin: Point<Pixels>,
    terminal_bounds: Bounds<Pixels>,
    metrics: CellMetrics,
    (cursor_col, cursor_cols): (u16, u16),
    window: &mut Window,
    cx: &mut App,
) {
    let cursor_left = origin.x + metrics.x_for_col(cursor_col);
    let cursor_right = cursor_left + metrics.cell_width * f32::from(cursor_cols);
    let row_top = origin.y;
    if cursor_left > terminal_bounds.left() {
        let mask = Bounds::from_corners(
            point(terminal_bounds.left(), row_top),
            point(cursor_left, row_top + metrics.line_height),
        );
        window.with_content_mask(Some(ContentMask { bounds: mask }), |window| {
            let _ = line.paint(
                origin,
                metrics.line_height,
                TextAlign::Left,
                None,
                window,
                cx,
            );
        });
    }
    if cursor_right < terminal_bounds.right() {
        let mask = Bounds::from_corners(
            point(cursor_right, row_top),
            point(terminal_bounds.right(), row_top + metrics.line_height),
        );
        window.with_content_mask(Some(ContentMask { bounds: mask }), |window| {
            let _ = line.paint(
                origin,
                metrics.line_height,
                TextAlign::Left,
                None,
                window,
                cx,
            );
        });
    }
}

fn styled_font(base: &Font, style: ResolvedCellStyle) -> Font {
    if style.bold {
        base.clone().bold()
    } else if style.italic {
        base.clone().italic()
    } else {
        base.clone()
    }
}

/// Strong right-to-left scalars and explicit bidi controls: the presence of
/// one makes glyph order and position a property of the entire line.
fn is_bidi_sensitive(scalar: u32) -> bool {
    matches!(
        scalar,
        0x0590..=0x08FF
            | 0x200F
            | 0x202A..=0x202E
            | 0x2066..=0x2069
            | 0xFB1D..=0xFDFF
            | 0xFE70..=0xFEFF
            | 0x10800..=0x10FFF
            | 0x1E800..=0x1EFFF
    )
}

/// Whether the cell is painted as geometry rather than as a font glyph. Both
/// families lie above U+2500, so text is answered by the first comparison.
fn is_procedural(scalar: u32) -> bool {
    scalar >= 0x2500
        && (BlockGlyph::from_scalar(scalar).is_some() || Sprite::from_scalar(scalar).is_some())
}

fn render_char(cell: GridCell, visible: bool) -> char {
    if !visible || cell.scalar == 0 || is_procedural(cell.scalar) {
        return ' ';
    }
    char::from_u32(cell.scalar)
        .filter(|ch| *ch != '\n' && *ch != '\r')
        .unwrap_or(' ')
}

/// Extracts a URL from a whitespace-delimited run of characters,
/// shedding the punctuation that wraps prose-embedded links.
fn url_from_run(run: &str) -> Option<String> {
    let stripped = trim_reference_run(run);
    if stripped.starts_with("http://") || stripped.starts_with("https://") {
        Some(stripped.to_owned())
    } else if stripped.starts_with("www.") && stripped["www.".len()..].contains('.') {
        Some(format!("https://{stripped}"))
    } else {
        None
    }
}

/// Grid cells do not carry soft-wrap or OSC 8 targets. Reconstruct visible URLs
/// conservatively: bare links continue only at the terminal's right edge;
/// prose/table links need an opening wrapper and its matching closing wrapper.
/// Read through the viewport so cached history and held reading views agree
/// with what the user actually clicked. Both lookbehind and URL size are bounded.
fn wrapped_url_at(
    col: usize,
    clicked_row: i64,
    row_at: impl Fn(i64) -> Vec<GridCell>,
) -> Option<(String, Vec<ReferenceSpan>)> {
    const MAX_ROWS: usize = 16;
    const MAX_URL_BYTES: usize = 4096;
    let read_row = |row| {
        row_at(row)
            .into_iter()
            .map(crate::selection::cell_char)
            .collect::<Vec<_>>()
    };
    for behind in 0..MAX_ROWS {
        let start_row = clicked_row.checked_sub(behind as i64)?;
        let chars = read_row(start_row);
        let mut start = 0;
        while start < chars.len() {
            if chars[start].is_whitespace() {
                start += 1;
                continue;
            }
            let end = reference_run_end(&chars, start);
            let mut candidate: String = chars[start..end].iter().collect();
            let closer = match chars[start] {
                '(' => Some(')'),
                '[' => Some(']'),
                '{' => Some('}'),
                '<' => Some('>'),
                '\'' => Some('\''),
                '"' => Some('"'),
                _ => None,
            };
            let already_closed = closer.is_some_and(|close| candidate[1..].contains(close));
            if url_from_run(&candidate).is_some()
                && !already_closed
                && (closer.is_some() || end == chars.len())
                && candidate.len() <= MAX_URL_BYTES
            {
                // A double-space gutter (or a drawn table border) separates
                // columns. Single spaces belong to the label before the URL.
                let mut lane_start = (0..start)
                    .rev()
                    .find(|&i| {
                        matches!(chars[i], '|' | '│')
                            || (chars[i].is_whitespace()
                                && (i == 0 || chars[i - 1].is_whitespace()))
                    })
                    .map_or(0, |i| i + 1);
                while lane_start < start && chars[lane_start].is_whitespace() {
                    lane_start += 1;
                }
                let mut spans = vec![(start_row, start, end)];
                let mut hit = behind == 0 && (start..end).contains(&col);
                let mut at_edge = end == chars.len();
                for ahead in 1..MAX_ROWS {
                    let row_number = start_row.checked_add(ahead as i64)?;
                    let next = read_row(row_number);
                    let mut next_start = if at_edge { 0 } else { lane_start };
                    // Apps that hard-wrap at the edge (Claude Code, Codex)
                    // indent the continuation under their own gutter.
                    if closer.is_some() || at_edge {
                        while next_start < next.len() && next[next_start].is_whitespace() {
                            next_start += 1;
                        }
                    }
                    if next_start >= next.len() || next[next_start].is_whitespace() {
                        if closer.is_none() && ahead > 1 && hit {
                            return url_from_run(&candidate).map(|url| (url, spans.clone()));
                        }
                        break;
                    }
                    // Do not jump to another table column across an empty cell,
                    // nor to text indented past where the URL itself began.
                    if (!at_edge && next_start != lane_start)
                        || (at_edge && closer.is_none() && next_start > start)
                    {
                        break;
                    }
                    let next_end = reference_run_end(&next, next_start);
                    let fragment: String = next[next_start..next_end].iter().collect();
                    if url_from_run(&fragment).is_some() || fragment.contains(['|', '│', '─', '━'])
                    {
                        if closer.is_none() && ahead > 1 && hit {
                            return url_from_run(&candidate).map(|url| (url, spans.clone()));
                        }
                        break;
                    }
                    if candidate.len() + fragment.len() > MAX_URL_BYTES {
                        break;
                    }
                    candidate.push_str(&fragment);
                    spans.push((row_number, next_start, next_end));
                    hit |= row_number == clicked_row && (next_start..next_end).contains(&col);
                    at_edge = next_end == next.len();
                    let complete = closer.map_or(!at_edge, |close| fragment.contains(close));
                    if complete {
                        if hit {
                            return url_from_run(&candidate).map(|url| (url, spans.clone()));
                        }
                        break;
                    }
                    if !at_edge
                        && (closer.is_none()
                            || next.get(next_end + 1).is_some_and(|ch| !ch.is_whitespace()))
                    {
                        break;
                    }
                }
            }
            start = end;
        }
    }
    None
}

fn reference_run_end(chars: &[char], start: usize) -> usize {
    chars[start..]
        .iter()
        .position(|ch| ch.is_whitespace())
        .map_or(chars.len(), |length| start + length)
}

fn trim_reference_run(run: &str) -> &str {
    run.trim()
        .trim_start_matches(['(', '[', '{', '<', '\'', '"'])
        .trim_end_matches(['.', ',', ';', ':', ')', ']', '}', '\'', '"', '>', '!', '?'])
}

fn reference_from_run(run: &str) -> Option<TerminalReference> {
    if let Some(url) = url_from_run(run) {
        return Some(TerminalReference::Url(url));
    }

    file_reference_from_run(run).map(TerminalReference::File)
}

/// Recognizes common compiler/test output locations without promoting every
/// terminal token to a file. Slash-containing paths and names with a plausible
/// extension are accepted; ordinary words and numeric positions are not.
fn file_reference_from_run(run: &str) -> Option<String> {
    let candidate = trim_file_reference_run(run);
    if candidate.is_empty() {
        return None;
    }
    let path_candidate = if let Some(path) = candidate.strip_prefix("file://") {
        path
    } else if candidate.contains("://") {
        return None;
    } else {
        candidate
    };

    // Ignore up to the conventional `:line:column` suffix while deciding
    // whether the preceding text looks like a path. The original candidate is
    // returned so the host can retain the navigation position.
    let mut path = parenthesized_location_path(path_candidate).unwrap_or(path_candidate);
    for _ in 0..2 {
        let Some((prefix, position)) = path.rsplit_once(':') else {
            break;
        };
        if prefix.is_empty()
            || position.is_empty()
            || !position.bytes().all(|byte| byte.is_ascii_digit())
        {
            break;
        }
        path = prefix;
    }

    let has_separator = path.contains('/') || path.contains('\\');
    let file_name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    let has_extension = file_name.rsplit_once('.').is_some_and(|(stem, extension)| {
        (!stem.is_empty() || file_name.starts_with('.'))
            && !extension.is_empty()
            && extension
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    });

    (path != "/" && path != "\\" && (has_separator || has_extension)).then(|| candidate.to_owned())
}

/// Trims prose punctuation while retaining compiler locations such as
/// `src/main.rs(42,7)`. A closing parenthesis is only preserved when it closes
/// one or two comma-separated numeric positions; other closing parentheses
/// continue to behave as ordinary wrappers.
fn trim_file_reference_run(run: &str) -> &str {
    let mut candidate = run
        .trim()
        .trim_start_matches(['(', '[', '{', '<', '\'', '"'])
        .trim_end_matches(['.', ',', ';', ':', ']', '}', '\'', '"', '>', '!', '?']);
    while candidate.ends_with(')') && parenthesized_location_path(candidate).is_none() {
        candidate = candidate[..candidate.len() - 1]
            .trim_end_matches(['.', ',', ';', ':', ']', '}', '\'', '"', '>', '!', '?']);
    }
    candidate
}

fn parenthesized_location_path(candidate: &str) -> Option<&str> {
    let without_close = candidate.strip_suffix(')')?;
    let (path, location) = without_close.rsplit_once('(')?;
    if path.is_empty() {
        return None;
    }
    let mut positions = location.split(',');
    let line = positions.next()?;
    let column = positions.next();
    if positions.next().is_some()
        || line.is_empty()
        || !line.bytes().all(|byte| byte.is_ascii_digit())
        || column.is_some_and(|column| {
            column.is_empty() || !column.bytes().all(|byte| byte.is_ascii_digit())
        })
    {
        return None;
    }
    Some(path)
}

fn mutex_lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn read_lock<T>(lock: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    lock.read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn write_lock<T>(lock: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    lock.write()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod seen_marker_tests {
    use super::SeenMarker;

    #[test]
    fn the_line_sits_between_seen_and_unseen_rows_or_not_at_all() {
        let marker = SeenMarker {
            row: 110,
            live_start_row: 100,
        };
        assert_eq!(marker.window_row(100, 24), Some(10));
        assert_eq!(marker.window_row(109, 24), Some(1));
        assert_eq!(
            marker.window_row(110, 24),
            None,
            "everything on screen is new"
        );
        assert_eq!(marker.window_row(111, 24), None, "scrolled past");
        assert_eq!(marker.window_row(80, 24), None, "still below the window");
        assert_eq!(marker.window_row(87, 24), Some(23));
    }

    #[test]
    fn a_marker_that_scrolled_into_history_is_no_longer_live() {
        let mut marker = SeenMarker {
            row: 110,
            live_start_row: 100,
        };
        assert!(marker.is_live());
        marker.live_start_row = 111;
        assert!(!marker.is_live());
        assert_eq!(marker.window_row(marker.live_start_row, 24), None);
    }
}

#[cfg(test)]
mod block_tests {
    use super::*;
    use diri_proto::grid::{TermColor, TermStyle};

    #[test]
    fn anara_blocks_fill_their_cell_and_keep_adjacent_text_in_place() {
        let metrics =
            CellMetrics::from_measurements(px(8.5), px(12.0), px(5.0), px(0.0), FontId(0));
        for (ch, top, height) in [('█', 0.0, 17.0), ('▀', 0.0, 8.5), ('▄', 8.5, 8.5)] {
            let cell = GridCell::new(
                ch as u32,
                TermColor::Default,
                TermColor::DefaultInverted,
                TermStyle::empty(),
            );
            let mut backgrounds = Vec::new();
            let mut foregrounds = Vec::new();
            append_row_quads(
                &[cell],
                1,
                SpriteGrid::new(point(px(2.0), px(3.0)), metrics, 1.0),
                TermTheme::default(),
                &[],
                &mut backgrounds,
                &mut foregrounds,
                &mut Vec::new(),
            );
            assert_eq!(
                foregrounds.len(),
                1,
                "{ch} must use cell geometry, not font ink bounds"
            );
            assert_eq!(
                foregrounds[0].bounds,
                Bounds::new(point(px(2.0), px(20.0 + top)), size(px(8.5), px(height)))
            );
            let terminal = TerminalElement::with_buffer(GridBuffer::default());
            let (text, _) = terminal.row_text_and_runs(
                &[
                    cell,
                    GridCell::new('A' as u32, cell.fg, cell.bg, cell.style),
                ],
                &[],
                &[],
            );
            assert_eq!(
                text, " A",
                "the block must reserve one text column without painting a second glyph"
            );
        }
    }

    #[test]
    fn a_bar_of_one_block_in_one_colour_is_one_quad() {
        let metrics =
            CellMetrics::from_measurements(px(8.5), px(12.0), px(5.0), px(0.0), FontId(0));
        fn bar(text: &str, color: u8) -> impl Iterator<Item = GridCell> + '_ {
            text.chars().map(move |ch| {
                GridCell::new(
                    ch as u32,
                    TermColor::Ansi(color),
                    TermColor::DefaultInverted,
                    TermStyle::empty(),
                )
            })
        }
        let row: Vec<_> = bar("████", 2)
            .chain(bar("██", 1))
            .chain(bar("▄▄▀", 1))
            .chain(bar("▌▌", 1))
            .collect();
        let mut backgrounds = Vec::new();
        let mut foregrounds = Vec::new();
        append_row_quads(
            &row,
            0,
            SpriteGrid::new(point(px(0.0), px(0.0)), metrics, 1.0),
            TermTheme::DIRIJOR_DARK,
            &[],
            &mut backgrounds,
            &mut foregrounds,
            &mut Vec::new(),
        );
        let widths: Vec<_> = foregrounds
            .iter()
            .map(|quad| f32::from(quad.bounds.size.width) / 8.5)
            .collect();
        // Colour, glyph, and partial-width blocks each end a run.
        assert_eq!(widths, [4.0, 2.0, 2.0, 1.0, 0.5, 0.5]);
    }

    fn sprite_row(
        row: &[GridCell],
        grid: SpriteGrid,
        theme: TermTheme,
    ) -> (Vec<PaintQuad>, Vec<PaintQuad>, Vec<AntialiasedShape>) {
        let (mut backgrounds, mut foregrounds, mut shapes) = (Vec::new(), Vec::new(), Vec::new());
        append_row_quads(
            row,
            2,
            grid,
            theme,
            &[],
            &mut backgrounds,
            &mut foregrounds,
            &mut shapes,
        );
        (backgrounds, foregrounds, shapes)
    }

    fn colored(text: &str, fg: TermColor, bg: TermColor) -> impl Iterator<Item = GridCell> + '_ {
        text.chars()
            .map(move |ch| GridCell::new(ch as u32, fg, bg, TermStyle::empty()))
    }

    #[test]
    fn a_rule_of_one_stroke_in_one_colour_is_one_quad() {
        let metrics =
            CellMetrics::from_measurements(px(7.8265624), px(12.0), px(3.0), px(0.0), FontId(0));
        let default_bg = TermColor::DefaultInverted;
        for scale in [1.0, 2.0] {
            let grid = SpriteGrid::new(point(px(13.1), px(3.5)), metrics, scale);
            let row: Vec<_> = colored(&"─".repeat(120), TermColor::Ansi(4), default_bg)
                .chain(colored("──", TermColor::Ansi(2), default_bg))
                .chain(colored("━━━", TermColor::Ansi(2), default_bg))
                .chain(colored("══", TermColor::Ansi(2), default_bg))
                .chain(colored("─┬─", TermColor::Ansi(2), default_bg))
                .collect();
            let (_, foregrounds, shapes) = sprite_row(&row, grid, TermTheme::DIRIJOR_DARK);
            assert!(shapes.is_empty());
            // Colour and glyph each end a run; ═ is two strokes, ┬ three.
            let cells: Vec<_> = foregrounds
                .iter()
                .map(|quad| (f32::from(quad.bounds.size.width) / 7.8265624).round())
                .collect();
            assert_eq!(cells[..5], [120.0, 2.0, 3.0, 2.0, 2.0]);
            assert_eq!(foregrounds.len(), 5 + 1 + 2 + 1);
            // The run ends on the device pixels its first and last cells own.
            let rule = foregrounds[0].bounds;
            assert_eq!(f32::from(rule.left()) * scale, grid.x(0));
            assert_eq!((f32::from(rule.right()) * scale).round(), grid.x(120));
        }
    }

    #[test]
    fn a_separator_starts_on_the_pixel_where_the_background_it_continues_ends() {
        for (width, origin_x) in [(7.25, 0.0), (7.8265624, 13.1), (8.5, 2.25)] {
            let metrics =
                CellMetrics::from_measurements(px(width), px(12.0), px(3.0), px(0.0), FontId(0));
            for scale in [1.0_f32, 2.0] {
                let grid = SpriteGrid::new(point(px(origin_x), px(3.5)), metrics, scale);
                let row: Vec<_> = colored(" main ", TermColor::Ansi(0), TermColor::Ansi(4))
                    .chain(colored(
                        "\u{e0b0}",
                        TermColor::Ansi(4),
                        TermColor::DefaultInverted,
                    ))
                    .collect();
                let (backgrounds, foregrounds, shapes) =
                    sprite_row(&row, grid, TermTheme::DIRIJOR_DARK);
                assert!(foregrounds.is_empty());
                let [AntialiasedShape::Polygon { path, color }] = &shapes[..] else {
                    panic!("a solid separator is one polygon");
                };
                assert_eq!(*color, backgrounds[0].background);
                // GPUI rounds the quad's edge half toward zero.
                let edge = f32::from(backgrounds[0].bounds.right()) * scale;
                let snapped = (edge.abs() - 0.5).ceil();
                assert_eq!(f32::from(path.bounds.left()) * scale, snapped);
                assert_eq!(f32::from(path.bounds.right()) * scale, grid.x(7));
            }
        }
    }

    #[test]
    fn sprites_preserve_terminal_colors_styles_and_their_text_column() {
        let metrics =
            CellMetrics::from_measurements(px(8.0), px(12.0), px(4.0), px(0.0), FontId(0));
        let grid = SpriteGrid::new(Point::default(), metrics, 2.0);
        for theme in [TermTheme::DIRIJOR_DARK, TermTheme::DIRIJOR_LIGHT] {
            for style in [
                TermStyle::empty(),
                TermStyle::DIM,
                TermStyle::INVERSE,
                TermStyle::BOLD | TermStyle::ITALIC,
                TermStyle::INVISIBLE,
            ] {
                for ch in ['┼', '╭', '⣿', '╳', '\u{e0b1}'] {
                    let cell =
                        GridCell::new(ch as u32, TermColor::Ansi(2), TermColor::Ansi(0), style);
                    let (_, foregrounds, shapes) = sprite_row(&[cell], grid, theme);
                    if style.contains(TermStyle::INVISIBLE) {
                        assert!(foregrounds.is_empty() && shapes.is_empty());
                        continue;
                    }
                    assert!(!foregrounds.is_empty() || !shapes.is_empty());
                    let foreground = theme.resolve_cell(cell).foreground;
                    for quad in &foregrounds {
                        assert_eq!(quad.background, fill(quad.bounds, foreground).background);
                    }
                    for shape in &shapes {
                        match shape {
                            AntialiasedShape::Arc { quad, .. } => {
                                assert_eq!(quad.border_color, gpui::Hsla::from(foreground));
                            }
                            AntialiasedShape::Polygon { color, .. } => {
                                assert_eq!(*color, foreground.into());
                            }
                        }
                    }
                    let terminal = TerminalElement::with_buffer(GridBuffer::default());
                    let (text, _) = terminal.row_text_and_runs(
                        &[
                            cell,
                            GridCell::new('A' as u32, cell.fg, cell.bg, cell.style),
                        ],
                        &[],
                        &[],
                    );
                    assert_eq!(text, " A", "{ch} must not also paint a font glyph");
                }
            }
        }
    }

    #[test]
    fn blocks_preserve_terminal_colors_styles_and_source_text() {
        let metrics =
            CellMetrics::from_measurements(px(8.0), px(12.0), px(4.0), px(0.0), FontId(0));
        for theme in [TermTheme::DIRIJOR_DARK, TermTheme::DIRIJOR_LIGHT] {
            for style in [
                TermStyle::empty(),
                TermStyle::DIM,
                TermStyle::INVERSE,
                TermStyle::BOLD | TermStyle::ITALIC,
                TermStyle::INVISIBLE,
            ] {
                let cell = GridCell::new(
                    '█' as u32,
                    TermColor::Rgb(120, 150, 180),
                    TermColor::Rgb(20, 30, 40),
                    style,
                );
                let mut backgrounds = Vec::new();
                let mut foregrounds = Vec::new();
                append_row_quads(
                    &[cell],
                    0,
                    SpriteGrid::new(Point::default(), metrics, 1.0),
                    theme,
                    &[],
                    &mut backgrounds,
                    &mut foregrounds,
                    &mut Vec::new(),
                );
                assert_eq!(backgrounds.len(), 1);
                if style.contains(TermStyle::INVISIBLE) {
                    assert!(foregrounds.is_empty());
                } else {
                    assert_eq!(foregrounds.len(), 1);
                    assert_eq!(
                        foregrounds[0].background,
                        fill(foregrounds[0].bounds, theme.resolve_cell(cell).foreground).background
                    );
                }
                let mut buffer = GridBuffer::new(1, 1);
                buffer.cells[0] = cell;
                assert_eq!(buffer.row_text_with_columns(0).unwrap().0, "█");
            }
        }
    }
}

#[cfg(test)]
mod link_tests {
    use std::sync::{Arc, Mutex};

    use gpui::{Bounds, point, px, size};

    use super::{
        TerminalImeState, TerminalInputHandler, TerminalReference, file_reference_from_run,
        mutex_lock, reference_from_run, url_from_run,
    };

    fn terminal_with_rows(rows: &[&str]) -> super::TerminalElement {
        let cols = rows.iter().map(|row| row.chars().count()).max().unwrap();
        let mut buffer = crate::buffer::GridBuffer::new(cols as u16, rows.len() as u16);
        for (row, text) in rows.iter().enumerate() {
            for (col, ch) in text.chars().enumerate() {
                buffer.cells[row * cols + col].scalar = u32::from(ch);
            }
        }
        super::TerminalElement::new(Arc::new(std::sync::RwLock::new(buffer)))
    }

    #[test]
    fn wrapped_url_in_table_opens_full_pr_from_either_row() {
        let terminal = terminal_with_rows(&[
            "  #6396 — Safari banner (https://github.com/anaralabs/anara/   Updates the existing PR",
            "  pull/6396)                                                 routing.",
        ]);
        for (row, start, end) in [(0, 24, 56), (1, 2, 12)] {
            for col in start..end {
                assert_eq!(
                    terminal.link_at(col, row).as_deref(),
                    Some("https://github.com/anaralabs/anara/pull/6396"),
                    "click at row {row}, col {col}"
                );
            }
        }
        assert_eq!(terminal.link_at(60, 0), None);
        assert_eq!(terminal.link_at(60, 1), None);
        assert_eq!(terminal.link_at(1, 1), None);
    }

    #[test]
    fn wrapped_url_spans_three_indented_rows_and_keeps_query_and_fragment() {
        let terminal = terminal_with_rows(&[
            "  See (https://github.com/anaralabs/",
            "  anara/pull/6396?diff=split&",
            "  view=1#discussion).",
            "                                        ",
        ]);
        for (col, row) in [(8, 0), (4, 1), (5, 2)] {
            assert_eq!(
                terminal.link_at(col, row).as_deref(),
                Some("https://github.com/anaralabs/anara/pull/6396?diff=split&view=1#discussion")
            );
        }
    }

    #[test]
    fn wrapped_url_at_terminal_edge_works_from_each_fragment() {
        let terminal =
            terminal_with_rows(&["https://github.com/anaralabs/", "anara/pull/6396 next"]);
        for (col, row) in [(12, 0), (5, 1)] {
            assert_eq!(
                terminal.link_at(col, row).as_deref(),
                Some("https://github.com/anaralabs/anara/pull/6396")
            );
        }
        assert_eq!(terminal.link_at(16, 1), None);
    }

    #[test]
    fn wrapped_url_with_indented_continuation_rows_opens_full_url() {
        // Claude Code hard-wraps at the terminal edge and indents every
        // continuation row under its `⏺ ` gutter.
        let terminal = terminal_with_rows(&[
            "⏺ Explore https://exampl",
            "  e.com/search?q=laptop&",
            "  color=silver to filter",
            "                        ",
        ]);
        for (col, row) in [(12, 0), (2, 1), (23, 1), (4, 2)] {
            assert_eq!(
                terminal.link_at(col, row).as_deref(),
                Some("https://example.com/search?q=laptop&color=silver"),
                "click at row {row}, col {col}"
            );
        }
        assert_eq!(terminal.link_at(15, 2), None);

        let prose = terminal_with_rows(&[
            "  See https://example.com/a ",
            "  to filter by budget.      ",
        ]);
        assert_eq!(
            prose.link_at(8, 0).as_deref(),
            Some("https://example.com/a")
        );
        assert_eq!(prose.link_at(2, 1), None);

        let outdented = terminal_with_rows(&["  https://example.com/", "        more text"]);
        assert_eq!(
            outdented.link_at(4, 0).as_deref(),
            Some("https://example.com/")
        );
    }

    #[test]
    fn wrapped_url_can_end_exactly_at_terminal_edge() {
        let terminal = terminal_with_rows(&["https://example.com/", "12345678901234567890"]);
        for row in 0..2 {
            assert_eq!(
                terminal.link_at(5, row).as_deref(),
                Some("https://example.com/12345678901234567890")
            );
        }
    }

    #[test]
    fn wrapped_url_in_second_table_column_stays_in_its_column() {
        let terminal = terminal_with_rows(&[
            "  PR  See (https://github.com/anaralabs/   Details",
            "  42  anara/pull/6396)                   More",
        ]);
        assert_eq!(
            terminal.link_at(7, 1).as_deref(),
            Some("https://github.com/anaralabs/anara/pull/6396")
        );
        assert_eq!(terminal.link_at(2, 1), None);
        assert_eq!(terminal.link_at(42, 1), None);
        let bordered = terminal_with_rows(&[
            "│ (https://github.com/anaralabs/ │ Details │",
            "│ anara/pull/6396)              │ More    │",
        ]);
        assert_eq!(
            bordered.link_at(3, 1).as_deref(),
            Some("https://github.com/anaralabs/anara/pull/6396")
        );
    }

    #[test]
    fn wrapped_url_does_not_join_unrelated_rows_or_table_cells() {
        for rows in [
            vec![
                "  (https://example.com)",
                "  unrelated/path)",
                "                              ",
            ],
            vec![
                "  (https://example.com/",
                "  ────────────────────",
                "  pull/6396)",
            ],
            vec![
                "  (https://example.com/   Text",
                "                         unrelated/path)",
            ],
            vec![
                "  (https://example.com/",
                "  unrelated prose)",
                "                              ",
            ],
            vec![
                "  https://example.com/",
                "  unrelated/path)",
                "                              ",
            ],
            vec!["https://example.com/", "https://example.org/"],
        ] {
            let terminal = terminal_with_rows(&rows);
            assert_eq!(
                terminal.link_at(3, 0).as_deref(),
                Some(
                    rows[0]
                        .split_whitespace()
                        .next()
                        .unwrap()
                        .trim_matches(['(', ')'])
                ),
                "{rows:?}"
            );
        }
    }

    #[test]
    fn wrapped_url_resolves_across_cached_history_and_live_grid() {
        let terminal = terminal_with_rows(&[
            "  pull/6396)",
            "                                                                ",
        ]);
        let history = terminal_with_rows(&["  PR (https://github.com/anaralabs/anara/"]);
        let cells = super::read_lock(&history.buffer).cells.clone();
        {
            let mut viewport = mutex_lock(&terminal.shared.viewport);
            viewport.apply_rows(vec![cells], 0, 1, 3, 1, 2);
            viewport.set_view_offset(1, 2);
        }
        for (col, row) in [(8, 0), (5, 1)] {
            assert_eq!(
                terminal.link_at(col, row).as_deref(),
                Some("https://github.com/anaralabs/anara/pull/6396")
            );
        }
    }

    #[test]
    fn terminal_renderer_never_creates_autonomous_frame_tasks() {
        let source = include_str!("element.rs");
        let foreground_task = ["cx.", "spawn(async move"].concat();
        let periodic_timer = ["background_executor()", ".timer("].concat();

        assert!(
            !source.contains(&foreground_task),
            "terminal rendering must stay event-driven"
        );
        assert!(
            !source.contains(&periodic_timer),
            "the terminal cursor must not own a periodic frame timer"
        );
        // The blink's wake is the one timer in the renderer. It is one-shot,
        // armed only by a painted frame, and `CursorSchedule::Rest` ends the
        // chain (`an_idle_cursor_paints_a_bounded_number_of_frames_then_none`).
        let motion = include_str!("cursor_motion.rs");
        assert_eq!(motion.matches(&periodic_timer).count(), 1);
        assert_eq!(motion.matches(&foreground_task).count(), 1);
    }

    fn cursor_update(col: u16, row: u16, rows: &[u16], full: bool) -> super::GridUpdate {
        super::GridUpdate {
            cols: 8,
            rows: 4,
            cursor_col: col,
            cursor_row: row,
            cursor_visible: true,
            is_full_snapshot: full,
            changed_rows: rows
                .iter()
                .map(|y| diri_proto::grid::ChangedRow::new(*y, vec![super::GridCell::BLANK; 8]))
                .collect(),
        }
    }

    fn cursor_frame(terminal: &super::TerminalElement, col: u16, row: u16) -> super::CursorFrame {
        mutex_lock(&terminal.shared.cursor).sample(super::CursorCell { col, row }, false)
    }

    #[test]
    fn typed_cursor_moves_glide_and_redraws_snap() {
        let terminal = super::TerminalElement::with_buffer(crate::buffer::GridBuffer::new(8, 4));
        let start = std::time::Instant::now();
        let at = |ms: u64| Some(start + std::time::Duration::from_millis(ms));
        terminal.set_cursor_clock(at(0));
        terminal.apply_damage(cursor_update(2, 1, &[0, 1, 2, 3], true));
        assert!(cursor_frame(&terminal, 2, 1).is_static());

        // A key, then its echo moves the cursor one cell on one damaged row.
        terminal.set_cursor_clock(at(1_000));
        assert!(!terminal.note_user_input());
        terminal.set_cursor_clock(at(1_006));
        terminal.apply_damage(cursor_update(3, 1, &[1], false));
        let frame = cursor_frame(&terminal, 3, 1);
        assert_eq!((frame.offset_cols, frame.offset_rows), (-1.0, 0.0));
        assert_eq!(frame.schedule, super::CursorSchedule::NextFrame);

        // The same move without a keystroke is the program's: it snaps.
        terminal.set_cursor_clock(at(3_000));
        terminal.apply_damage(cursor_update(4, 1, &[1], false));
        assert!(!cursor_frame(&terminal, 4, 1).is_gliding());

        // A keystroke whose echo repaints the screen snaps too.
        terminal.set_cursor_clock(at(4_000));
        let _ = terminal.note_user_input();
        terminal.apply_damage(cursor_update(5, 1, &[0, 1, 2], false));
        assert!(!cursor_frame(&terminal, 5, 1).is_gliding());
        let _ = terminal.note_user_input();
        terminal.set_cursor_clock(at(4_100));
        terminal.apply_damage(cursor_update(6, 1, &[1], true));
        assert!(!cursor_frame(&terminal, 6, 1).is_gliding());
    }

    #[test]
    fn a_keystroke_asks_for_a_repaint_only_while_the_cursor_is_dimmed() {
        let terminal = super::TerminalElement::with_buffer(crate::buffer::GridBuffer::new(8, 4));
        let start = std::time::Instant::now();
        terminal.set_cursor_clock(Some(start));
        terminal.apply_damage(cursor_update(2, 1, &[1], true));
        assert_eq!(cursor_frame(&terminal, 2, 1).opacity, 1.0);
        assert!(!terminal.note_user_input());

        let low = crate::cursor_motion::BLINK_IDLE_DELAY + crate::cursor_motion::BLINK_FADE;
        terminal.set_cursor_clock(Some(start + low));
        assert!(cursor_frame(&terminal, 2, 1).opacity < 1.0);
        assert!(terminal.note_user_input());
        // Once asked, the repaint is on its way.
        assert!(!terminal.note_user_input());
        assert_eq!(cursor_frame(&terminal, 2, 1).opacity, 1.0);
    }

    #[test]
    fn focused_terminal_registers_a_platform_input_handler_for_ime() {
        let source = include_str!("element.rs");
        let registration = ["window.", "handle_input("].concat();
        let ime_priority = ["fn prefers_ime", "_for_printable_keys"].concat();
        assert!(
            source.contains(&registration),
            "a key-down listener cannot receive marked or committed IME text"
        );
        assert!(
            source.contains(&ime_priority),
            "composition input sources must reach the IME before terminal key bindings"
        );
    }

    #[test]
    fn ime_tracks_marked_utf16_and_commits_utf8_exactly_once() {
        let committed = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink = Arc::clone(&committed);
        let state = Arc::new(Mutex::new(TerminalImeState::default()));
        let handler = TerminalInputHandler {
            text_input: Arc::new(move |text| {
                mutex_lock(&sink).push(text.to_owned());
            }),
            ime_state: Arc::clone(&state),
            view: terminal_with_rows(&["test"]).damage_observer(),
            cursor_bounds: Bounds::new(point(px(0.0), px(0.0)), size(px(8.0), px(16.0))),
            cell_width: px(8.0),
            cursor: Arc::default(),
        };

        handler.mark_text("ni");
        assert_eq!(mutex_lock(&state).marked_range(), Some(0..2));
        handler.commit_text("你");

        assert!(mutex_lock(&state).marked_range().is_none());
        assert_eq!(&*mutex_lock(&committed), &["你"]);
    }

    #[test]
    fn native_text_commit_never_forwards_terminal_control_bytes() {
        let committed = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink = Arc::clone(&committed);
        let handler = TerminalInputHandler {
            text_input: Arc::new(move |text| mutex_lock(&sink).push(text.to_owned())),
            ime_state: Arc::new(Mutex::new(TerminalImeState::default())),
            view: terminal_with_rows(&["test"]).damage_observer(),
            cursor_bounds: Bounds::new(point(px(0.0), px(0.0)), size(px(8.0), px(16.0))),
            cell_width: px(8.0),
            cursor: Arc::default(),
        };

        // AppKit may commit ETX after handling Command-C. ETX is Ctrl-C to a
        // terminal, so it must never reach the live PTY through the IME path.
        handler.commit_text("\u{3}");

        assert!(mutex_lock(&committed).is_empty());
    }

    #[test]
    fn overlay_gate_rejects_existing_native_handler_until_terminal_restored() {
        let terminal = terminal_with_rows(&["test"]);
        let committed = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink = Arc::clone(&committed);
        let handler = TerminalInputHandler {
            text_input: Arc::new(move |text| mutex_lock(&sink).push(text.to_owned())),
            ime_state: Arc::clone(&terminal.ime_state),
            view: terminal.damage_observer(),
            cursor_bounds: Bounds::new(point(px(0.0), px(0.0)), size(px(8.0), px(16.0))),
            cell_width: px(8.0),
            cursor: Arc::default(),
        };
        handler.mark_text("old");
        terminal.set_text_input_enabled(false);
        handler.mark_text("ni");
        handler.commit_text("你");
        assert!(mutex_lock(&terminal.ime_state).marked_range().is_none());
        assert!(mutex_lock(&committed).is_empty());
        terminal.set_text_input_enabled(true);
        handler.commit_text("terminal");
        assert_eq!(&*mutex_lock(&committed), &["terminal"]);
    }

    #[test]
    fn committed_text_returns_a_reading_view_to_live_unless_an_overlay_owns_input() {
        let terminal = terminal_with_rows(&["one", "two"]);
        let committed = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink = Arc::clone(&committed);
        let handler = TerminalInputHandler {
            text_input: Arc::new(move |text| mutex_lock(&sink).push(text.to_owned())),
            ime_state: Arc::clone(&terminal.ime_state),
            view: terminal.damage_observer(),
            cursor_bounds: Bounds::new(point(px(0.0), px(0.0)), size(px(8.0), px(16.0))),
            cell_width: px(8.0),
            cursor: Arc::clone(&terminal.shared.cursor),
        };
        terminal.adopt_history_geometry(100, 102, 1, 2);
        let read_history = || {
            terminal.set_view_offset(5, 2);
            assert_eq!(terminal.view_offset(), 5);
        };

        // Composing is not input yet, and neither is the ETX AppKit commits
        // after Command-C: copying while reading must not lose the place.
        read_history();
        handler.mark_text("ni");
        assert!(!handler.commit_text("\u{3}"));
        assert_eq!(terminal.view_offset(), 5);

        // Find owns text while it is open; its query is not PTY input.
        terminal.set_text_input_enabled(false);
        assert!(!handler.commit_text("你"));
        assert_eq!(terminal.view_offset(), 5);
        assert!(mutex_lock(&committed).is_empty());

        terminal.set_text_input_enabled(true);
        assert!(
            handler.commit_text("你"),
            "the view moved and owes a repaint"
        );
        assert_eq!(terminal.view_offset(), 0);
        assert_eq!(&*mutex_lock(&committed), &["你"]);

        assert!(
            !handler.commit_text("好"),
            "already live: nothing to repaint"
        );
        assert_eq!(&*mutex_lock(&committed), &["你", "好"]);
    }

    #[test]
    fn the_cursor_follows_the_host_and_protocol_visibility() {
        assert!(super::cursor_should_render(true, true));
        assert!(!super::cursor_should_render(true, false));
        assert!(!super::cursor_should_render(false, true));
        assert!(!super::cursor_should_render(false, false));
    }

    #[test]
    fn extracts_bare_and_wrapped_urls() {
        assert_eq!(
            url_from_run("https://example.com/a?b=1"),
            Some("https://example.com/a?b=1".to_owned())
        );
        assert_eq!(
            url_from_run("(https://example.com/path)."),
            Some("https://example.com/path".to_owned())
        );
        assert_eq!(
            url_from_run("<https://example.com>,"),
            Some("https://example.com".to_owned())
        );
        assert_eq!(
            url_from_run("www.example.com"),
            Some("https://www.example.com".to_owned())
        );
        assert_eq!(url_from_run("not-a-url"), None);
        assert_eq!(url_from_run("www."), None);
        assert_eq!(url_from_run("http:/broken"), None);
    }

    #[test]
    fn routes_file_urls_as_local_file_references() {
        assert_eq!(
            reference_from_run("<file:///tmp/foo.swift>"),
            Some(TerminalReference::File("file:///tmp/foo.swift".to_owned()))
        );
        assert_eq!(
            file_reference_from_run("file:///tmp/foo.swift"),
            Some("file:///tmp/foo.swift".to_owned())
        );
    }

    #[test]
    fn extracts_punctuation_wrapped_file_locations() {
        assert_eq!(
            file_reference_from_run("[src/main.rs:42:7],"),
            Some("src/main.rs:42:7".to_owned())
        );
        assert_eq!(
            file_reference_from_run("(/tmp/foo.swift:9)."),
            Some("/tmp/foo.swift:9".to_owned())
        );
        assert_eq!(
            reference_from_run("src/main.rs:42"),
            Some(TerminalReference::File("src/main.rs:42".to_owned()))
        );
        assert_eq!(
            reference_from_run("[src/main.rs(42,7)],"),
            Some(TerminalReference::File("src/main.rs(42,7)".to_owned()))
        );
        assert_eq!(
            file_reference_from_run("(src/main.rs(42))."),
            Some("src/main.rs(42)".to_owned())
        );
    }

    #[test]
    fn rejects_plain_terminal_words_as_file_references() {
        assert_eq!(file_reference_from_run("Finished"), None);
        assert_eq!(file_reference_from_run("warning"), None);
        assert_eq!(file_reference_from_run("42:7"), None);
        assert_eq!(file_reference_from_run("warning(42,7)"), None);
    }
}

#[cfg(test)]
mod history_cache_tests {
    use diri_proto::grid::{GridCell, TermColor, TermStyle};
    use gpui::{FontId, ShapedLine};

    use super::{HistoryLineCache, HistoryShapeKey, digest_cells, digest_row};

    #[test]
    fn combining_only_changes_invalidate_history_shapes() {
        let cells = row("e");
        let mut cache = HistoryLineCache::default();
        cache.validate(key(), 0);
        let original = digest_row(&cells, &[(0, "\u{301}".into())], &[]);
        cache.insert(3, original, ShapedLine::default());
        assert!(cache.get(3, original).is_some());
        assert!(
            cache
                .get(3, digest_row(&cells, &[(0, "\u{308}".into())], &[]))
                .is_none()
        );
        assert!(cache.get(3, digest_row(&cells, &[], &[])).is_none());
    }

    fn key() -> HistoryShapeKey {
        HistoryShapeKey {
            theme_signature: 1,
            font_id: FontId(0),
            font_size_bits: 13f32.to_bits(),
            cell_width_bits: 8f32.to_bits(),
            visible_cols: 80,
        }
    }

    fn row(text: &str) -> Vec<GridCell> {
        text.chars()
            .map(|ch| {
                GridCell::new(
                    u32::from(ch),
                    diri_proto::grid::TermColor::Default,
                    TermColor::DefaultInverted,
                    TermStyle::empty(),
                )
            })
            .collect()
    }

    #[test]
    fn shaping_survives_churn_that_leaves_the_row_alone() {
        let cells = row("cargo build --release");
        let digest = digest_cells(&cells);
        let mut cache = HistoryLineCache::default();
        cache.validate(key(), 0);
        cache.insert(42, digest, ShapedLine::default());

        // A spinner repaints the live grid many times over. The history row is
        // untouched, so its shaping must still be there.
        for _ in 0..100 {
            cache.validate(key(), 0);
        }
        assert!(cache.get(42, digest).is_some());
    }

    #[test]
    fn a_row_whose_cells_changed_is_reshaped() {
        let mut cache = HistoryLineCache::default();
        cache.validate(key(), 0);
        cache.insert(7, digest_cells(&row("       ")), ShapedLine::default());

        // The blank placeholder was cached before the fetch landed; the real
        // cells must not hit it.
        assert!(cache.get(7, digest_cells(&row("error[E0499]"))).is_none());
    }

    #[test]
    fn returning_live_frees_the_table_not_just_its_entries() {
        let digest = digest_cells(&row("x"));
        let mut cache = HistoryLineCache::default();
        cache.validate(key(), 0);
        for absolute in 0..900 {
            cache.insert(absolute, digest, ShapedLine::default());
        }
        assert!(cache.lines.capacity() >= 900);

        cache.release();
        assert_eq!(cache.lines.capacity(), 0);
        assert!(cache.get(1, digest).is_none());

        // Scrolling back again starts from a clean, valid cache.
        cache.validate(key(), 0);
        cache.insert(1, digest, ShapedLine::default());
        assert!(cache.get(1, digest).is_some());
    }

    #[test]
    fn a_changed_font_drops_everything() {
        let digest = digest_cells(&row("x"));
        let mut cache = HistoryLineCache::default();
        cache.validate(key(), 0);
        cache.insert(1, digest, ShapedLine::default());

        let mut resized = key();
        resized.font_size_bits = 16f32.to_bits();
        cache.validate(resized, 0);
        assert!(cache.get(1, digest).is_none());
    }

    #[test]
    fn overflow_keeps_the_rows_around_the_window() {
        let digest = digest_cells(&row("x"));
        let mut cache = HistoryLineCache::default();
        cache.validate(key(), 0);
        for absolute in 0..(HistoryLineCache::MAX_ROWS as i64 + 200) {
            cache.insert(absolute, digest, ShapedLine::default());
        }

        // Evicting by distance, not by clearing: the window keeps its shaping.
        let anchor = 1_000;
        cache.validate(key(), anchor);
        assert!(cache.get(anchor, digest).is_some(), "the window survives");
        assert!(cache.get(0, digest).is_none(), "distant rows are dropped");
        assert!(cache.lines.len() <= HistoryLineCache::MAX_ROWS);
    }
}

#[cfg(test)]
mod selection_repaint_tests {
    use diri_proto::grid::{ChangedRow, GridCell, GridUpdate, TermColor, TermStyle};
    use diri_proto::terminal::MouseModes;

    use super::{TerminalElement, mutex_lock};
    use crate::buffer::GridBuffer;
    use crate::scrollback::{WheelDelta, WheelEvent, WheelRoute};

    const COLS: u16 = 8;
    const ROWS: u16 = 3;

    fn cell(ch: char) -> GridCell {
        GridCell::new(
            u32::from(ch),
            TermColor::Default,
            TermColor::DefaultInverted,
            TermStyle::empty(),
        )
    }

    fn row(text: &str) -> Vec<GridCell> {
        let mut cells: Vec<_> = text.chars().map(cell).collect();
        cells.resize(usize::from(COLS), GridCell::BLANK);
        cells
    }

    fn update(full: bool, rows: &[(u16, &str)]) -> GridUpdate {
        GridUpdate {
            cols: COLS,
            rows: ROWS,
            cursor_col: 0,
            cursor_row: 0,
            cursor_visible: false,
            is_full_snapshot: full,
            changed_rows: rows
                .iter()
                .map(|(y, text)| ChangedRow::new(*y, row(text)))
                .collect(),
        }
    }

    fn populated_element() -> TerminalElement {
        let element = TerminalElement::with_buffer(GridBuffer::new(COLS, ROWS));
        element.apply_damage(update(true, &[(0, "zero"), (1, "one"), (2, "two")]));
        element
    }

    #[test]
    fn streaming_redraw_preserves_scrolled_reading_window() {
        let element = populated_element();
        mutex_lock(&element.shared.viewport).apply_rows(
            vec![row("history")],
            7,
            8,
            11,
            1,
            usize::from(ROWS),
        );
        element.set_view_offset(1, usize::from(ROWS));
        let reading = element
            .viewport()
            .compose(&super::read_lock(&element.buffer), usize::from(ROWS));

        // An agent redraws its live screen while the reader is one row up.
        element.apply_damage(update(true, &[(0, "new"), (1, "output"), (2, "below")]));

        assert_eq!(
            element
                .viewport()
                .compose(&super::read_lock(&element.buffer), usize::from(ROWS)),
            reading,
            "streaming must not replace text already visible to a scrolled reader",
        );
        element.scroll_to_live(usize::from(ROWS));
        assert_eq!(
            element
                .viewport()
                .window_row(&super::read_lock(&element.buffer), 0),
            row("new")
        );
    }

    fn wheel(delta: f32) -> WheelEvent {
        WheelEvent {
            delta: WheelDelta::Lines(delta),
            col: 2,
            row: 1,
            visible_rows: ROWS,
            line_height: 16.0,
        }
    }

    fn history_reply(
        first: i64,
        live: i64,
        seq: u64,
        texts: &[&str],
    ) -> diri_proto::methods::ReadScrollbackCellsResult {
        diri_proto::methods::ReadScrollbackCellsResult {
            metadata: Vec::new(),
            payload: diri_proto::grid::GridRowCodec::encode_rows(
                &texts.iter().map(|text| row(text)).collect::<Vec<_>>(),
            )
            .unwrap(),
            first_row: first,
            row_count: texts.len() as i64,
            live_start_row: live,
            total_rows: live + i64::from(ROWS),
            cols: i64::from(COLS),
            content_seq: seq,
        }
    }

    #[test]
    fn named_link_resolves_label_span_and_rejects_unsafe_destination() {
        use diri_proto::grid::LinkSpan;
        let element = populated_element();
        let mut frame = update(false, &[(0, "label")]);
        frame.changed_rows[0].metadata.links.push(LinkSpan {
            start: 0,
            end: 5,
            uri: "https://example.com/pr/1".into(),
        });
        element.apply_damage(frame.clone());
        let hit = element.reference_hit_at(2, 0).unwrap();
        assert_eq!(hit.reference.destination(), "https://example.com/pr/1");
        assert_eq!(hit.spans, vec![(0, 0, 5)]);
        assert!(element.reference_hit_at(5, 0).is_none());
        frame.changed_rows[0].metadata.links[0].uri = "javascript:alert(1)".into();
        element.apply_damage(frame);
        assert!(element.reference_hit_at(2, 0).is_none());
    }

    #[test]
    fn keyboard_copy_holds_live_text_until_exit() {
        let element = populated_element();
        element.pin_keyboard_selection(true);
        element.begin_selection(0, 0);
        element.drag_selection(4, 0);
        element.apply_damage(update(false, &[(0, "new")]));
        assert_eq!(element.selected_text(), "zero");
        assert_eq!(
            element
                .viewport()
                .window_row(&super::read_lock(&element.buffer), 0),
            row("zero")
        );
        element.pin_keyboard_selection(false);
        assert_eq!(
            element
                .viewport()
                .window_row(&super::read_lock(&element.buffer), 0),
            row("new")
        );
    }

    #[test]
    fn scrolling_before_first_fetch_holds_live_text_and_selection() {
        let element = populated_element();
        element.route_wheel(wheel(1.0));
        element.apply_damage(update(false, &[(0, "new")]));
        element
            .complete_scrollback_fetch(history_reply(7, 8, 2, &["history"]), usize::from(ROWS))
            .unwrap();
        assert_eq!(
            element
                .viewport()
                .compose(&super::read_lock(&element.buffer), 3),
            vec![row("history"), row("zero"), row("one")]
        );
        element.begin_selection(0, 1);
        element.drag_selection(4, 1);
        element.apply_damage(update(false, &[(0, "again")]));
        assert_eq!(element.selected_text(), "zero");
        element.scroll_to_live(3);
        assert_eq!(element.selected_text(), "");
        element.route_wheel(wheel(1.0));
        element
            .complete_scrollback_fetch(history_reply(17, 18, 3, &["fresh"]), 3)
            .unwrap();
        assert_eq!(
            element.view_offset(),
            1,
            "a new scroll starts at the current live edge"
        );
        assert_eq!(
            element
                .viewport()
                .compose(&super::read_lock(&element.buffer), 3),
            vec![row("fresh"), row("again"), row("one")]
        );
    }

    #[test]
    fn overlapping_history_replies_and_live_growth_preserve_reading_text() {
        let element = populated_element();
        element
            .complete_scrollback_fetch(history_reply(6, 8, 1, &["hist six", "hist sev"]), 3)
            .unwrap();
        element.set_view_offset(2, 3);
        let reading = element
            .viewport()
            .compose(&super::read_lock(&element.buffer), 3);
        for seq in 2..10 {
            element.apply_damage(update(true, &[(0, "new"), (1, "output"), (2, "below")]));
            element
                .complete_scrollback_fetch(
                    history_reply(7, 8 + seq as i64, seq, &["changed", "changed"]),
                    3,
                )
                .unwrap();
            assert_eq!(
                element
                    .viewport()
                    .compose(&super::read_lock(&element.buffer), 3),
                reading
            );
            assert_eq!(element.viewport().absolute_row(0), 6);
        }
        element.set_modes(true, MouseModes::OFF);
        assert_eq!(element.view_offset(), 0);
        assert!(element.viewport().cached_row(6).is_none());
        assert_eq!(
            element
                .viewport()
                .window_row(&super::read_lock(&element.buffer), 0),
            row("new")
        );
    }

    fn visible_selection_count(element: &TerminalElement) -> usize {
        let viewport = mutex_lock(&element.shared.viewport).clone();
        mutex_lock(&element.shared.selection)
            .visible_spans(&viewport, usize::from(ROWS), usize::from(COLS))
            .len()
    }

    #[test]
    fn daemon_wheel_drops_the_selection_before_the_tui_can_repaint() {
        for modes in [(true, MouseModes::OFF), (false, MouseModes::UNKNOWN)] {
            let element = populated_element();
            element.set_modes(modes.0, modes.1);
            element.begin_selection(0, 1);
            element.drag_selection(3, 1);
            assert_eq!(element.selected_text(), "one");

            assert!(matches!(
                element.route_wheel(wheel(1.0)),
                Some(WheelRoute::Daemon { .. })
            ));
            assert_eq!(element.selected_text(), "", "modes: {modes:?}");
            assert_eq!(visible_selection_count(&element), 0, "modes: {modes:?}");
        }
    }

    #[test]
    fn local_scroll_keeps_selection_attached_across_the_history_live_seam() {
        let element = populated_element();
        {
            let mut viewport = mutex_lock(&element.shared.viewport);
            viewport.apply_rows(
                vec![row("hist six"), row("hist sev")],
                6,
                8,
                11,
                1,
                usize::from(ROWS),
            );
            assert!(viewport.set_view_offset(2, usize::from(ROWS)));
        }
        element.begin_selection(0, 0);
        element.drag_selection(3, 2);
        let selected = element.selected_text();
        assert_eq!(selected, "hist six\nhist sev\nzer");

        assert_eq!(
            element.route_wheel(wheel(1.0)),
            Some(WheelRoute::Local { lines: 1 })
        );
        assert_eq!(element.selected_text(), selected);
        assert_eq!(visible_selection_count(&element), 2);
    }

    fn trackpad(points: f32) -> WheelEvent {
        WheelEvent {
            delta: WheelDelta::PrecisePoints(points),
            ..wheel(0.0)
        }
    }

    #[test]
    fn a_trackpad_scrolls_history_by_the_pixel_and_addresses_the_extra_row() {
        let element = populated_element();
        mutex_lock(&element.shared.viewport).apply_rows(
            vec![row("hist six"), row("hist sev")],
            6,
            8,
            11,
            1,
            usize::from(ROWS),
        );
        // Four pixels of a 16 px line: no row is crossed, the view still moves.
        assert_eq!(
            element.route_wheel(trackpad(4.0)),
            Some(WheelRoute::Local { lines: 1 })
        );
        assert_eq!(element.view_offset(), 1);
        assert!((element.scroll_position() - 0.25).abs() < 1e-6);
        assert_eq!(element.scroll_shift(gpui::px(16.0), 2.0), gpui::px(12.0));
        assert_eq!(
            element.route_wheel(trackpad(4.0)),
            Some(WheelRoute::Local { lines: 0 })
        );
        assert_eq!(element.scroll_shift(gpui::px(16.0), 2.0), gpui::px(8.0));

        // The window is rows 7..10 slid up by half a row, so the row under
        // its bottom edge (window row ROWS) is live row "two".
        element.select_line(usize::from(ROWS));
        assert_eq!(element.selected_text(), "two");
        let viewport = mutex_lock(&element.shared.viewport);
        let painted = viewport.painted_rows(usize::from(ROWS));
        assert_eq!(painted, usize::from(ROWS) + 1);
        let spans =
            mutex_lock(&element.shared.selection).visible_spans(&viewport, painted, COLS.into());
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].row, usize::from(ROWS));
        drop(viewport);

        // Momentum carries the view back onto the live grid, exactly.
        assert!(element.route_wheel(trackpad(-90.0)).is_some());
        assert_eq!(element.view_offset(), 0);
        assert_eq!(element.scroll_shift(gpui::px(16.0), 2.0), gpui::px(0.0));
        assert_eq!(element.route_wheel(trackpad(-90.0)), None);
    }

    #[test]
    fn a_trackpad_over_a_mouse_reporting_program_still_sends_whole_lines() {
        let element = populated_element();
        element.set_modes(
            false,
            MouseModes::new(
                diri_proto::terminal::MouseTrackingMode::ButtonEvents,
                diri_proto::terminal::MouseEncoding::Legacy,
            ),
        );
        assert_eq!(element.route_wheel(trackpad(9.0)), None);
        assert_eq!(
            element.route_wheel(trackpad(9.0)),
            Some(WheelRoute::Daemon {
                direction: 0,
                lines: 1,
                col: 2,
                row: 1,
            })
        );
        assert_eq!(element.scroll_position(), 0.0, "scrollback never moved");

        element.set_modes(true, MouseModes::OFF);
        assert!(matches!(
            element.route_wheel(trackpad(20.0)),
            Some(WheelRoute::Daemon { .. })
        ));
        assert_eq!(element.scroll_position(), 0.0);
    }

    #[test]
    fn entering_and_leaving_alt_screen_drop_active_selections() {
        let element = populated_element();
        element.begin_selection(0, 1);
        element.drag_selection(3, 1);

        element.set_modes(true, MouseModes::OFF);
        assert_eq!(element.selected_text(), "", "entering alt screen");

        element.begin_selection(0, 2);
        element.drag_selection(3, 2);
        element.set_modes(false, MouseModes::OFF);
        assert_eq!(element.selected_text(), "", "leaving alt screen");
    }

    #[test]
    fn alt_screen_repaint_only_drops_a_selection_when_damage_overlaps_it() {
        let element = populated_element();
        element.set_modes(true, MouseModes::OFF);
        element.begin_selection(0, 1);
        element.drag_selection(3, 1);

        element.apply_damage(update(false, &[(0, "ZERO")]));
        assert_eq!(element.selected_text(), "one");
        assert_eq!(visible_selection_count(&element), 1);

        element.apply_damage(update(false, &[(1, "new")]));
        assert_eq!(element.selected_text(), "");
        assert_eq!(visible_selection_count(&element), 0);
    }

    #[test]
    fn normal_screen_output_missing_the_selection_preserves_highlight_and_copy() {
        let element = populated_element();
        element.begin_selection(0, 1);
        element.drag_selection(3, 1);

        element.apply_damage(update(false, &[(2, "TWO")]));

        assert_eq!(element.selected_text(), "one");
        assert_eq!(visible_selection_count(&element), 1);
    }

    #[test]
    fn normal_screen_output_replacing_selected_text_drops_highlight_and_copy() {
        let element = populated_element();
        element.begin_selection(0, 1);
        element.drag_selection(3, 1);

        element.apply_damage(update(false, &[(1, "new")]));

        assert_eq!(element.selected_text(), "");
        assert_eq!(visible_selection_count(&element), 0);
    }

    #[test]
    fn full_snapshot_reseed_drops_any_selection_in_the_live_grid() {
        let element = populated_element();
        element.begin_selection(0, 1);
        element.drag_selection(3, 1);

        element.apply_damage(update(true, &[(0, "zero"), (1, "one"), (2, "two")]));

        assert_eq!(element.selected_text(), "");
        assert_eq!(visible_selection_count(&element), 0);
    }
}

#[cfg(test)]
mod live_scroll_cache_tests {
    use super::*;

    fn cells(ch: u8) -> Vec<GridCell> {
        let mut cell = GridCell::BLANK;
        cell.scalar = u32::from(ch);
        vec![cell; 8]
    }

    fn cache() -> Vec<Option<CachedRow>> {
        (b'a'..=b'd')
            .enumerate()
            .map(|(row, ch)| {
                Some(CachedRow {
                    cells: cells(ch),
                    graphemes: Vec::new(),
                    tints: Vec::new(),
                    background_quads: vec![fill(
                        Bounds::new(point(px(3.), px(row as f32 * 20.)), size(px(80.), px(20.))),
                        gpui::black(),
                    )],
                    decoration_quads: vec![fill(
                        Bounds::new(
                            point(px(3.), px(row as f32 * 20. + 18.)),
                            size(px(80.), px(1.)),
                        ),
                        gpui::white(),
                    )],
                    sprite_shapes: Vec::new(),
                    line: ShapedLine::default(),
                })
            })
            .collect()
    }

    fn damage(text: &[u8]) -> Vec<ChangedRenderRow> {
        text.iter()
            .enumerate()
            .map(|(row, &ch)| ChangedRenderRow {
                row,
                generation: 1,
                cells: cells(ch),
                graphemes: Vec::new(),
            })
            .collect()
    }

    #[test]
    fn scrolling_reuses_shapes_and_moves_backgrounds_and_decorations_both_ways() {
        for (text, expected_offset, matched_rows) in [
            (b"bcde", 1, vec![0, 1, 2]),
            (b"zabc", 3, vec![1, 2, 3]),
            (b"cdef", 2, vec![0, 1]),
        ] {
            let mut cache = cache();
            let damage = damage(text);
            let offset = align_scrolled_rows(&mut cache, &damage);
            assert_eq!(offset, expected_offset);
            for row in matched_rows {
                let prepared = cache[row].as_mut().unwrap();
                assert_eq!(prepared.cells, damage[row].cells);
                let previous = (row + offset) % 4;
                let metrics =
                    CellMetrics::from_measurements(px(10.), px(16.), px(4.), px(0.), FontId(0));
                let grid = SpriteGrid::new(point(px(3.), px(0.)), metrics, 2.0);
                assert!(prepared.move_vertically(px((row as f32 - previous as f32) * 20.), grid));
                assert_eq!(
                    prepared.background_quads[0].bounds.origin,
                    point(px(3.), px(row as f32 * 20.))
                );
                assert_eq!(
                    prepared.decoration_quads[0].bounds.origin,
                    point(px(3.), px(row as f32 * 20. + 18.))
                );
            }
        }
    }

    #[test]
    fn sparse_damage_and_unrelated_redraw_do_not_rotate_the_cache() {
        let mut cache = cache();
        assert_eq!(align_scrolled_rows(&mut cache, &damage(b"b")), 0);
        assert_eq!(align_scrolled_rows(&mut cache, &damage(b"wxyz")), 0);
        for (row, ch) in (b'a'..=b'd').enumerate() {
            assert_eq!(cache[row].as_ref().unwrap().cells, cells(ch));
        }
    }

    #[test]
    fn a_scroll_hint_does_not_make_edited_rows_reusable() {
        let mut cache = cache();
        let mut damage = damage(b"bcde");
        damage[1].cells[0].bg = diri_proto::grid::TermColor::Ansi(1);
        assert_eq!(align_scrolled_rows(&mut cache, &damage), 1);
        assert_ne!(cache[1].as_ref().unwrap().cells, damage[1].cells);
    }
}

#[cfg(test)]
mod grapheme_paint_tests {
    use super::*;
    use diri_proto::grid::TermStyle;

    #[test]
    fn parser_combining_text_reaches_shaping_and_invisible_cells_stay_hidden() {
        let mut parser = diri_terminal_state::HeadlessScreen::new(12, 2);
        parser.feed("e\u{301} A🙂B".as_bytes());
        let mut grid = GridBuffer::default();
        grid.apply(parser.full_snapshot());
        let terminal = TerminalElement::with_buffer(grid.clone());
        let (text, runs) =
            terminal.row_text_and_runs(grid.row(0).unwrap(), &grid.annotations[0].graphemes, &[]);
        assert!(text.starts_with("e\u{301} A🙂 B"));
        assert_eq!(runs.iter().map(|run| run.len).sum::<usize>(), text.len());
        grid.cells[0].style = TermStyle::INVISIBLE;
        let (hidden, _) =
            terminal.row_text_and_runs(grid.row(0).unwrap(), &grid.annotations[0].graphemes, &[]);
        assert!(!hidden.contains('\u{301}'));
        assert!(hidden.starts_with(' '));
    }

    #[test]
    fn glyphs_under_a_tint_are_colored_against_it_and_the_rest_are_not() {
        // Light text on the bright current-match highlight of a dark theme is
        // the unreadable case: 2.6:1 on Dirijor Dark, 1.0:1 on Solarized Dark.
        let theme = TermTheme::SOLARIZED_DARK;
        let terminal = TerminalElement::with_buffer(GridBuffer::default()).theme(theme);
        let row: Vec<_> = "find me here"
            .chars()
            .map(|ch| {
                GridCell::new(
                    u32::from(ch),
                    diri_proto::grid::TermColor::Default,
                    diri_proto::grid::TermColor::Default,
                    TermStyle::empty(),
                )
            })
            .collect();
        let tints = [Tint {
            start: 5,
            end: 7,
            color: theme.find_match_current,
        }];

        let (_, plain) = terminal.row_text_and_runs(&row, &[], &[]);
        assert_eq!(plain.len(), 1);

        let (_, runs) = terminal.row_text_and_runs(&row, &[], &tints);
        assert_eq!(
            runs.iter().map(|run| run.len).collect::<Vec<_>>(),
            [5, 2, 5],
            "only the glyphs under the tint change run"
        );
        assert_eq!(runs[0].color, plain[0].color);
        assert_eq!(runs[2].color, plain[0].color);
        assert_ne!(runs[1].color, plain[0].color);

        assert_ne!(
            digest_row(&row, &[], &tints),
            digest_row(&row, &[], &[]),
            "a history line shaped without the tint must not be reused under it"
        );
    }

    #[test]
    fn overlapping_tints_combine_and_columns_outside_have_none() {
        let selection = Tint {
            start: 2,
            end: 6,
            color: TermTheme::DIRIJOR_DARK.selection,
        };
        let find = Tint {
            start: 4,
            end: 8,
            color: TermTheme::DIRIJOR_DARK.find_match,
        };
        let tints = [selection, find];
        assert_eq!(tint_at(&tints, 1), None);
        assert_eq!(tint_at(&tints, 2), Some(selection.color));
        assert_eq!(tint_at(&tints, 7), Some(find.color));
        assert_eq!(
            tint_at(&tints, 5),
            Some(crate::contrast::over(find.color, selection.color))
        );
        assert_eq!(tint_at(&tints, 8), None);
    }

    #[test]
    fn trailing_blanks_are_not_shaped_unless_the_line_reorders() {
        let shaped = |text: &str, graphemes: &[(u16, String)]| {
            let mut row: Vec<_> = text
                .chars()
                .map(|ch| GridCell {
                    scalar: u32::from(ch),
                    ..GridCell::BLANK
                })
                .collect();
            row.resize(12, GridCell::BLANK);
            TerminalElement::with_buffer(GridBuffer::default())
                .row_text_and_runs(&row, graphemes, &[])
                .0
        };
        assert_eq!(shaped("ab  c", &[]), "ab  c ");
        assert_eq!(shaped("", &[]), " ");
        assert_eq!(shaped("full  width!", &[]), "full  width!");
        // Block elements are quads; a mark on a blank cell is still a glyph.
        assert_eq!(shaped("a█", &[]), "a ");
        assert_eq!(shaped("a", &[(3, "\u{301}".into())]), "a   \u{301} ");
        assert_eq!(shaped("שלום", &[]).chars().count(), 12);
    }

    #[test]
    fn combining_only_output_damages_the_row_and_preserves_cell_identity() {
        let mut parser = diri_terminal_state::HeadlessScreen::new(8, 2);
        parser.feed(b"e");
        let mut grid = GridBuffer::default();
        grid.apply(parser.full_snapshot());
        let before = grid.cells.clone();
        let mut known = Vec::new();
        assert_eq!(
            grid.snapshot_damage(&mut known, 2, 8, true)
                .changed_rows
                .len(),
            2
        );
        parser.feed("\u{301}".as_bytes());
        grid.apply(parser.full_snapshot());
        assert_eq!(grid.cells, before);
        let damage = grid.snapshot_damage(&mut known, 2, 8, false);
        assert_eq!(
            damage.changed_rows[0].graphemes,
            vec![(0, "\u{301}".into())]
        );
        let terminal = TerminalElement::with_buffer(grid);
        let changed = &damage.changed_rows[0];
        assert!(
            terminal
                .row_text_and_runs(&changed.cells, &changed.graphemes, &[])
                .0
                .starts_with("e\u{301}")
        );
    }
}
