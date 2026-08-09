use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::{Duration, Instant};

use diri_proto::grid::{GridCell, GridUpdate};
use gpui::{
    App, Bounds, ContentMask, Element, ElementId, FocusHandle, Font, FontFallbacks, FontId,
    GlobalElementId, InspectorElementId, IntoElement, LayoutId, PaintQuad, Pixels, Point,
    ShapedLine, SharedString, Style, TextAlign, TextRun, Window, fill, font, point, px, relative,
    size,
};

use crate::buffer::{ApplySummary, GridBuffer};
use crate::find::{FindSnapshot, FindSpan, NavigationTarget, SearchRequest, TerminalFindModel};
use crate::metrics::CellMetrics;
use crate::scrollback::{
    ScrollRouter, ScrollbackApplyError, ScrollbackRequest, ScrollbackViewport, ScrolledState,
    TerminalModes, WheelEvent, WheelRoute,
};
use crate::selection::{SelectionPoint, TerminalSelection};
use crate::theme::{ResolvedCellStyle, TermTheme, is_default_background};

static NEXT_ELEMENT_ID: AtomicU64 = AtomicU64::new(1);

pub type SharedGridBuffer = Arc<RwLock<GridBuffer>>;

/// A command-clickable reference rendered in a terminal row.
///
/// Web URLs stay distinct so hosts can preserve their normal external-opening
/// behavior while routing paths and `file://` references into an editor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TerminalReference {
    Url(String),
    File(String),
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
    font: Font,
    font_size: Pixels,
    focus_handle: Option<FocusHandle>,
    focus_override: Option<bool>,
    suspended: bool,
}

struct ElementSharedState {
    id: u64,
    row_cache: Mutex<Vec<Option<CachedRow>>>,
    render_generations: Mutex<Vec<u64>>,
    render_context: Mutex<Option<RowRenderContext>>,
    stats: Mutex<RendererStats>,
    viewport: Mutex<ScrollbackViewport>,
    selection: Mutex<TerminalSelection>,
    find_spans: Mutex<Vec<FindSpan>>,
    modes: Mutex<TerminalModes>,
    scroll_router: Mutex<ScrollRouter>,
    history_lines: Mutex<HistoryLineCache>,
}

/// Shaped lines for history rows, keyed by absolute row and content-addressed
/// by a digest of the row's cells, so shaping survives across scrolled frames
/// instead of being redone per frame.
///
/// The digest replaces following the viewport's `content_seq`: that sequence
/// advances on *any* visible change — a spinner in the live grid was enough —
/// which dumped the shaping of history rows that had not moved a pixel.
/// Comparing the cells cannot go stale, and costs a hash against a reshape.
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
}

fn digest_cells(cells: &[GridCell]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    cells.hash(&mut hasher);
    hasher.finish()
}

#[derive(Clone)]
struct CachedRow {
    cells: Vec<GridCell>,
    background_quads: Vec<PaintQuad>,
    decoration_quads: Vec<PaintQuad>,
    line: ShapedLine,
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
    visible_cols: usize,
    visible_rows: usize,
}

pub struct TerminalPrepaintState {
    started_at: Option<Instant>,
    background_quads: Vec<PaintQuad>,
    decoration_quads: Vec<PaintQuad>,
    overlay_quads: Vec<PaintQuad>,
    lines: Vec<(u16, ShapedLine)>,
    metrics: Option<CellMetrics>,
    cursor: Option<CursorPaint>,
    cache_hits: u64,
    cache_misses: u64,
    /// Live path: paint straight out of the shared row cache instead of
    /// composed copies, so an unchanged frame clones nothing.
    paint_from_cache: bool,
}

struct CursorPaint {
    row: u16,
    col: u16,
    quad: PaintQuad,
    glyph: Option<ShapedLine>,
}

impl TerminalElement {
    #[must_use]
    pub fn new(buffer: SharedGridBuffer) -> Self {
        let mut terminal_font = font(".SF NS Mono");
        terminal_font.fallbacks = Some(FontFallbacks::from_fonts(vec![
            "Menlo".to_owned(),
            "Apple Symbols".to_owned(),
            "STIX Two Math".to_owned(),
            "Apple Color Emoji".to_owned(),
        ]));
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
                find_spans: Mutex::new(Vec::new()),
                modes: Mutex::new(TerminalModes::default()),
                scroll_router: Mutex::new(ScrollRouter::default()),
                history_lines: Mutex::new(HistoryLineCache::default()),
            }),
            theme: TermTheme::default(),
            font: terminal_font,
            font_size: px(13.0),
            focus_handle: None,
            focus_override: None,
            suspended: false,
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

    #[must_use]
    pub fn theme(mut self, theme: TermTheme) -> Self {
        self.theme = theme;
        self
    }

    #[must_use]
    pub fn font(mut self, font: Font) -> Self {
        self.font = font;
        self
    }

    #[must_use]
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

    /// Primarily useful for previews and deterministic visual tests.
    #[must_use]
    pub fn focused(mut self, focused: bool) -> Self {
        self.focus_override = Some(focused);
        self
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
        write_lock(&self.buffer).apply(update)
    }

    #[must_use]
    pub fn stats(&self) -> RendererStats {
        *mutex_lock(&self.shared.stats)
    }

    pub fn reset_stats(&self) {
        *mutex_lock(&self.shared.stats) = RendererStats::default();
    }

    #[must_use]
    pub fn viewport(&self) -> ScrollbackViewport {
        mutex_lock(&self.shared.viewport).clone()
    }

    #[must_use]
    pub fn view_offset(&self) -> i64 {
        mutex_lock(&self.shared.viewport).view_offset()
    }

    #[must_use]
    pub fn scrolled_state(&self) -> Option<ScrolledState> {
        let offset_lines = self.view_offset();
        (offset_lines > 0).then_some(ScrolledState { offset_lines })
    }

    pub fn set_view_offset(&self, offset: i64, visible_rows: usize) -> bool {
        mutex_lock(&self.shared.viewport).set_view_offset(offset, visible_rows)
    }

    pub fn scroll_to_live(&self, visible_rows: usize) -> bool {
        mutex_lock(&self.shared.viewport).scroll_to_live(visible_rows)
    }

    pub fn scroll_to_absolute(&self, absolute_row: i64, anchor: f32, visible_rows: usize) -> bool {
        mutex_lock(&self.shared.viewport).scroll_to_absolute(absolute_row, anchor, visible_rows)
    }

    pub fn set_modes(&self, alt_screen: bool, mouse_reporting: bool) -> bool {
        let mut modes = mutex_lock(&self.shared.modes);
        let entered_alt = alt_screen && !modes.alt_screen;
        *modes = TerminalModes {
            alt_screen,
            mouse_reporting,
        };
        drop(modes);
        entered_alt && mutex_lock(&self.shared.viewport).enter_alt_screen()
    }

    /// True while the foreground program consumes mouse events, in which case
    /// pointer gestures belong to it rather than local selection.
    pub fn mouse_reporting(&self) -> bool {
        mutex_lock(&self.shared.modes).mouse_reporting
    }

    /// Resolves a wheel event and applies local scrollback movement. Daemon
    /// routes are returned for the app to pass to `SessionAttachment::scroll`.
    pub fn route_wheel(&self, event: WheelEvent) -> Option<WheelRoute> {
        let modes = *mutex_lock(&self.shared.modes);
        let route = mutex_lock(&self.shared.scroll_router).route(modes, event)?;
        if let WheelRoute::Local { lines } = route {
            mutex_lock(&self.shared.viewport).scroll_by(lines, usize::from(event.visible_rows));
        }
        Some(route)
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
        let absolute_row = mutex_lock(&self.shared.viewport).absolute_row(window_row);
        mutex_lock(&self.shared.selection).drag_to(SelectionPoint {
            row: absolute_row,
            col,
        });
    }

    pub fn select_word(&self, col: usize, window_row: usize) {
        let viewport = mutex_lock(&self.shared.viewport).clone();
        let buffer = read_lock(&self.buffer);
        mutex_lock(&self.shared.selection).select_word(&viewport, &buffer, window_row, col);
    }

    pub fn clear_selection(&self) {
        mutex_lock(&self.shared.selection).clear();
    }

    /// The web URL whose text spans the given window cell, if any.
    /// Wrapped multi-row URLs are out of scope: the scan is per logical row.
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
    /// the path itself. Multi-row references are deliberately out of scope.
    #[must_use]
    pub fn reference_at(&self, col: usize, window_row: usize) -> Option<TerminalReference> {
        let viewport = mutex_lock(&self.shared.viewport).clone();
        let buffer = read_lock(&self.buffer);
        let absolute_row = viewport.absolute_row(window_row);
        let row = viewport.row_at_absolute(&buffer, absolute_row);
        let chars: Vec<char> = row
            .iter()
            .map(|cell| crate::selection::cell_char(*cell))
            .collect();
        drop(buffer);
        if col >= chars.len() {
            return None;
        }
        let is_reference_char = |c: char| !c.is_whitespace() && c != '\0';
        if !is_reference_char(chars[col]) {
            return None;
        }
        let mut start = col;
        while start > 0 && is_reference_char(chars[start - 1]) {
            start -= 1;
        }
        let mut end = col + 1;
        while end < chars.len() && is_reference_char(chars[end]) {
            end += 1;
        }
        let candidate: String = chars[start..end].iter().collect();
        reference_from_run(&candidate)
    }

    #[must_use]
    pub fn selected_text(&self) -> String {
        let viewport = mutex_lock(&self.shared.viewport).clone();
        let buffer = read_lock(&self.buffer);
        mutex_lock(&self.shared.selection).selected_text(&viewport, &buffer)
    }

    pub fn set_find_highlights(&self, spans: Vec<FindSpan>) {
        *mutex_lock(&self.shared.find_spans) = spans;
    }

    pub fn apply_find_snapshot(
        &self,
        model: &mut TerminalFindModel,
        request: &SearchRequest,
        snapshot: FindSnapshot,
    ) -> bool {
        let buffer = read_lock(&self.buffer);
        model.apply_snapshot(
            request,
            snapshot,
            &buffer,
            &mut mutex_lock(&self.shared.viewport),
        )
    }

    pub fn find_next(&self, model: &mut TerminalFindModel) -> Option<NavigationTarget> {
        model.next(&mut mutex_lock(&self.shared.viewport))
    }

    pub fn find_previous(&self, model: &mut TerminalFindModel) -> Option<NavigationTarget> {
        model.previous(&mut mutex_lock(&self.shared.viewport))
    }

    pub fn sync_find_highlights(&self, model: &TerminalFindModel) {
        let viewport = mutex_lock(&self.shared.viewport);
        self.set_find_highlights(model.visible_spans(&viewport));
    }

    fn is_focused(&self, window: &Window) -> bool {
        self.focus_override.unwrap_or_else(|| {
            self.focus_handle
                .as_ref()
                .is_some_and(|focus| focus.is_focused(window))
                && window.is_window_active()
        })
    }

    fn shape_row(&self, row: &[GridCell], metrics: CellMetrics, window: &mut Window) -> ShapedLine {
        let (text, runs) = self.row_text_and_runs(row);
        window.text_system().shape_line(
            SharedString::from(text),
            self.font_size,
            &runs,
            Some(metrics.cell_width),
        )
    }

    fn prepare_row(
        &self,
        cells: Vec<GridCell>,
        row: u16,
        origin: Point<Pixels>,
        metrics: CellMetrics,
        window: &mut Window,
    ) -> CachedRow {
        let mut background_quads = Vec::new();
        append_background_quads(
            &cells,
            row,
            origin,
            metrics,
            self.theme,
            &mut background_quads,
        );
        let mut decoration_quads = Vec::new();
        append_decoration_quads(
            &cells,
            row,
            origin,
            metrics,
            self.theme,
            &mut decoration_quads,
        );
        let line = self.shape_row(&cells, metrics, window);
        CachedRow {
            cells,
            background_quads,
            decoration_quads,
            line,
        }
    }

    fn row_text_and_runs(&self, row: &[GridCell]) -> (String, Vec<TextRun>) {
        let mut text = String::with_capacity(row.len());
        let mut runs = Vec::<TextRun>::new();

        for cell in row {
            let resolved = self.theme.resolve_cell(*cell);
            let ch = render_char(*cell, resolved.visible);
            let byte_len = ch.len_utf8();
            text.push(ch);

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
        metrics: CellMetrics,
        window: &mut Window,
    ) -> Option<ShapedLine> {
        let resolved = self.theme.resolve_cell(cell);
        let ch = render_char(cell, resolved.visible);
        if ch == ' ' {
            return None;
        }
        let text = SharedString::from(ch.to_string());
        let run = TextRun {
            len: text.len(),
            font: styled_font(&self.font, resolved),
            color: self.theme.cursor_text.into(),
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
        _cx: &mut App,
    ) -> Self::PrepaintState {
        if self.suspended {
            mutex_lock(&self.shared.row_cache).clear();
            mutex_lock(&self.shared.render_generations).clear();
            *mutex_lock(&self.shared.render_context) = None;
            return TerminalPrepaintState {
                started_at: None,
                background_quads: Vec::new(),
                decoration_quads: Vec::new(),
                overlay_quads: Vec::new(),
                lines: Vec::new(),
                metrics: None,
                cursor: None,
                cache_hits: 0,
                cache_misses: 0,
                paint_from_cache: false,
            };
        }

        let (grid_cols, grid_rows, grid_is_empty) = {
            let buffer = read_lock(&self.buffer);
            (buffer.cols, buffer.rows, buffer.cells.is_empty())
        };
        let focused = self.is_focused(window);

        if grid_is_empty {
            return TerminalPrepaintState {
                started_at: None,
                background_quads: Vec::new(),
                decoration_quads: Vec::new(),
                overlay_quads: Vec::new(),
                lines: Vec::new(),
                metrics: None,
                cursor: None,
                cache_hits: 0,
                cache_misses: 0,
                paint_from_cache: false,
            };
        }

        let started_at = Instant::now();
        let metrics = CellMetrics::measure(window.text_system(), &self.font, self.font_size);
        let visible_rows =
            usize::from(grid_rows).min(usize::from(metrics.rows_for_height(bounds.size.height)));
        let visible_cols =
            usize::from(grid_cols).min(usize::from(metrics.cols_for_width(bounds.size.width)));
        // Hold the viewport lock for the whole prepaint instead of deep-cloning
        // it: every party that touches these mutexes runs on the main thread,
        // and the clone copied the entire fetched-history cell cache per frame.
        let viewport = mutex_lock(&self.shared.viewport);
        let mut background_quads = vec![fill(bounds, self.theme.background)];
        let mut decoration_quads = Vec::new();
        let mut overlay_quads = Vec::new();
        let mut lines = Vec::with_capacity(visible_rows);
        let cursor;
        let cache_hits;
        let cache_misses;
        let mut paint_from_cache = false;

        if viewport.view_offset() > 0 {
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
            for row_index in 0..visible_rows {
                let absolute = viewport.absolute_row(row_index);
                let mut cells = viewport.window_row(&buffer, row_index);
                cells.truncate(visible_cols);
                append_background_quads(
                    &cells,
                    row_index as u16,
                    bounds.origin,
                    metrics,
                    self.theme,
                    &mut background_quads,
                );
                append_decoration_quads(
                    &cells,
                    row_index as u16,
                    bounds.origin,
                    metrics,
                    self.theme,
                    &mut decoration_quads,
                );
                let is_history = absolute < viewport.live_start_row();
                let digest = digest_cells(&cells);
                let line = if let Some(line) =
                    is_history.then(|| history.get(absolute, digest)).flatten()
                {
                    hits += 1;
                    line.clone()
                } else {
                    let line = self.shape_row(&cells, metrics, window);
                    // A row the viewport has not fetched yet composes as
                    // blank. Caching it is safe now that entries are content
                    // addressed: the blank's digest stops matching the moment
                    // the fetch lands.
                    if is_history {
                        history.insert(absolute, digest, line.clone());
                    }
                    line
                };
                lines.push((row_index as u16, line));
            }
            cache_hits = hits;
            cache_misses = (visible_rows as u64).saturating_sub(hits);
        } else {
            let context = RowRenderContext {
                theme_signature: self.theme.signature(),
                font_id: metrics.font_id,
                font_size_bits: f32::from(self.font_size).to_bits(),
                cell_width_bits: f32::from(metrics.cell_width).to_bits(),
                line_height_bits: f32::from(metrics.line_height).to_bits(),
                origin_x_bits: f32::from(bounds.origin.x).to_bits(),
                origin_y_bits: f32::from(bounds.origin.y).to_bits(),
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
            cache_misses = damage.changed_rows.len() as u64;
            cache_hits = visible_rows.saturating_sub(damage.changed_rows.len()) as u64;

            let mut cache = mutex_lock(&self.shared.row_cache);
            cache.truncate(visible_rows);
            cache.resize_with(visible_rows, || None);
            for changed in damage.changed_rows {
                cache[changed.row] = Some(self.prepare_row(
                    changed.cells,
                    changed.row as u16,
                    bounds.origin,
                    metrics,
                    window,
                ));
            }
            // No composed copies: paint reads the row cache directly (see
            // `paint_from_cache`), so a frame with zero changed rows clones
            // nothing — previously every prepaint re-cloned all rows' quads
            // and shaped lines even on a 100% cache hit.
            paint_from_cache = true;
        }

        let selection =
            mutex_lock(&self.shared.selection).visible_spans(&viewport, visible_rows, visible_cols);
        for span in selection {
            append_overlay_quad(
                span.row,
                span.start_col,
                span.end_col_exclusive,
                bounds.origin,
                metrics,
                self.theme.selection,
                &mut overlay_quads,
            );
        }
        for span in mutex_lock(&self.shared.find_spans).iter().copied() {
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

        let cursor_visible = cursor_should_render(focused, cursor.visible);
        let cursor = if cursor_visible
            && viewport.view_offset() == 0
            && usize::from(cursor.row) < visible_rows
            && usize::from(cursor.col) < visible_cols
        {
            let cache = mutex_lock(&self.shared.row_cache);
            let cell = cache[usize::from(cursor.row)]
                .as_ref()
                .and_then(|row| row.cells.get(usize::from(cursor.col)))
                .copied()
                .unwrap_or(GridCell::BLANK);
            let origin = point(
                bounds.left() + metrics.x_for_col(cursor.col),
                bounds.top() + metrics.y_for_row(cursor.row),
            );
            Some(CursorPaint {
                row: cursor.row,
                col: cursor.col,
                quad: fill(
                    Bounds::new(origin, size(metrics.cell_width, metrics.line_height)),
                    self.theme.cursor,
                ),
                glyph: self.shape_cursor_glyph(cell, metrics, window),
            })
        } else {
            None
        };

        TerminalPrepaintState {
            started_at: Some(started_at),
            background_quads,
            decoration_quads,
            overlay_quads,
            lines,
            metrics: Some(metrics),
            cursor,
            cache_hits,
            cache_misses,
            paint_from_cache,
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
        let Some(metrics) = prepaint.metrics else {
            return;
        };

        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            for quad in prepaint.background_quads.drain(..) {
                window.paint_quad(quad);
            }

            // Live path: rows come straight from the shared cache. Quads are
            // plain structs (a stack copy each) and `ShapedLine::paint` takes
            // a reference, so nothing per-row is heap-cloned per frame.
            let cache = prepaint
                .paint_from_cache
                .then(|| mutex_lock(&self.shared.row_cache));

            if let Some(cache) = &cache {
                for prepared in cache.iter().flatten() {
                    for quad in &prepared.background_quads {
                        window.paint_quad(quad.clone());
                    }
                }
            }

            for quad in prepaint.overlay_quads.drain(..) {
                window.paint_quad(quad);
            }

            if let Some(cache) = &cache {
                for (row_index, prepared) in cache.iter().enumerate() {
                    let Some(prepared) = prepared else { continue };
                    let row = row_index as u16;
                    let origin = point(bounds.left(), bounds.top() + metrics.y_for_row(row));
                    if prepaint
                        .cursor
                        .as_ref()
                        .is_some_and(|cursor| cursor.row == row)
                    {
                        let cursor = prepaint.cursor.as_ref().unwrap();
                        paint_line_around_cursor(
                            &prepared.line,
                            origin,
                            bounds,
                            metrics,
                            cursor.col,
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
                for prepared in cache.iter().flatten() {
                    for quad in &prepared.decoration_quads {
                        window.paint_quad(quad.clone());
                    }
                }
            }

            for (row, line) in &prepaint.lines {
                let origin = point(bounds.left(), bounds.top() + metrics.y_for_row(*row));
                if prepaint
                    .cursor
                    .as_ref()
                    .is_some_and(|cursor| cursor.row == *row)
                {
                    let cursor = prepaint.cursor.as_ref().unwrap();
                    paint_line_around_cursor(line, origin, bounds, metrics, cursor.col, window, cx);
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

            for quad in prepaint.decoration_quads.drain(..) {
                window.paint_quad(quad);
            }

            if let Some(cursor) = prepaint.cursor.take() {
                window.paint_quad(cursor.quad);
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

const fn cursor_should_render(focused: bool, protocol_visible: bool) -> bool {
    focused && protocol_visible
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

fn append_decoration_quads(
    row: &[GridCell],
    row_index: u16,
    origin: Point<Pixels>,
    metrics: CellMetrics,
    theme: TermTheme,
    quads: &mut Vec<PaintQuad>,
) {
    let mut col = 0;
    while col < row.len() {
        let style = theme.resolve_cell(row[col]);
        if !style.underline && !style.strikethrough {
            col += 1;
            continue;
        }
        let mut end = col + 1;
        while end < row.len() {
            let next = theme.resolve_cell(row[end]);
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
    cursor_col: u16,
    window: &mut Window,
    cx: &mut App,
) {
    let cursor_left = origin.x + metrics.x_for_col(cursor_col);
    let cursor_right = cursor_left + metrics.cell_width;
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

fn render_char(cell: GridCell, visible: bool) -> char {
    if !visible || cell.scalar == 0 {
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
mod link_tests {
    use super::{TerminalReference, file_reference_from_run, reference_from_run, url_from_run};

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
    }

    #[test]
    fn static_cursor_follows_focus_and_protocol_visibility() {
        assert!(super::cursor_should_render(true, true));
        assert!(!super::cursor_should_render(false, true));
        assert!(!super::cursor_should_render(true, false));
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

    use super::{HistoryLineCache, HistoryShapeKey, digest_cells};

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
                    TermColor::Default,
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
