//! Local scrollback composition and mode-aware wheel routing.
//!
//! Rows are cached by absolute, scroll-invariant terminal row. The live grid
//! starts at [`ScrollbackViewport::live_start_row`], and a window row projects
//! to `live_start_row - view_offset + window_row`.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::future::Future;
use std::ops::Range;
use std::pin::Pin;

use diri_proto::grid::{GridCell, GridCodecError, GridRowCodec};
use diri_proto::methods::ReadScrollbackCellsResult;
use diri_proto::model::SessionId;

use crate::buffer::GridBuffer;

const MAX_SCROLLBACK_CACHE_ROWS: usize = 512;

pub type FetchFuture<'a> = Pin<
    Box<dyn Future<Output = Result<ReadScrollbackCellsResult, ScrollbackFetchError>> + Send + 'a>,
>;

/// Session-independent adapter point for `DaemonClient::read_scrollback_cells`.
///
/// The app supplies an implementation that delegates to its client. Keeping
/// the trait here avoids coupling the renderer to `diri-client` or a runtime.
pub trait ScrollbackFetcher: Send + Sync {
    fn read_scrollback_cells<'a>(
        &'a self,
        session_id: &'a SessionId,
        first_row: i64,
        max_rows: i64,
    ) -> FetchFuture<'a>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScrollbackFetchError {
    message: String,
}

impl ScrollbackFetchError {
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for ScrollbackFetchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for ScrollbackFetchError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScrollbackRequest {
    pub first_row: i64,
    pub max_rows: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScrolledState {
    pub offset_lines: i64,
}

impl ScrolledState {
    #[must_use]
    pub fn label(&self) -> String {
        format!("{} lines · Return to live", self.offset_lines)
    }
}

impl ScrollbackRequest {
    #[must_use]
    pub fn range(&self) -> Range<i64> {
        self.first_row..self.first_row.saturating_add(self.max_rows)
    }

    pub async fn fetch(
        &self,
        fetcher: &dyn ScrollbackFetcher,
        session_id: &SessionId,
    ) -> Result<ReadScrollbackCellsResult, ScrollbackFetchError> {
        fetcher
            .read_scrollback_cells(session_id, self.first_row, self.max_rows)
            .await
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ScrollbackApplyError {
    NegativeRowCount(i64),
    RowCountMismatch { declared: usize, decoded: usize },
    Codec(GridCodecError),
}

impl fmt::Display for ScrollbackApplyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NegativeRowCount(count) => {
                write!(formatter, "negative scrollback row count {count}")
            }
            Self::RowCountMismatch { declared, decoded } => write!(
                formatter,
                "scrollback row count mismatch: declared {declared}, decoded {decoded}"
            ),
            Self::Codec(error) => write!(formatter, "invalid scrollback cell payload: {error}"),
        }
    }
}

impl Error for ScrollbackApplyError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Codec(error) => Some(error),
            Self::NegativeRowCount(_) | Self::RowCountMismatch { .. } => None,
        }
    }
}

impl From<GridCodecError> for ScrollbackApplyError {
    fn from(error: GridCodecError) -> Self {
        Self::Codec(error)
    }
}

#[derive(Clone, Debug, Default)]
pub struct ScrollbackViewport {
    view_offset: i64,
    /// Absolute row pinned to the top of the window while scrolled back.
    ///
    /// `view_offset` alone anchors the view to the *live edge*, so history
    /// growing under a scrolled reader slid the window forward onto rows it
    /// had not fetched — the view crawled while you were reading it, and every
    /// response landed stale by however much output arrived during the round
    /// trip. Past one screen per round trip that never converged: each
    /// completion queued the next window and pumped it, forever, with the
    /// wheel untouched. Pinning content instead makes `view_offset` a derived
    /// value that grows as the live edge moves away.
    anchor: Option<i64>,
    live_start_row: i64,
    total_rows: i64,
    geometry_known: bool,
    cache_seq: Option<u64>,
    cache: BTreeMap<i64, Vec<GridCell>>,
    in_flight: Option<Range<i64>>,
    queued: Option<Range<i64>>,
}

impl ScrollbackViewport {
    #[must_use]
    pub const fn view_offset(&self) -> i64 {
        self.view_offset
    }

    #[must_use]
    pub const fn live_start_row(&self) -> i64 {
        self.live_start_row
    }

    #[must_use]
    pub const fn total_rows(&self) -> i64 {
        self.total_rows
    }

    #[must_use]
    pub const fn geometry_known(&self) -> bool {
        self.geometry_known
    }

    #[must_use]
    pub fn cached_row(&self, absolute_row: i64) -> Option<&[GridCell]> {
        self.cache.get(&absolute_row).map(Vec::as_slice)
    }

    #[must_use]
    pub fn cached_row_count(&self) -> usize {
        self.cache.len()
    }

    /// Content sequence the fetched-row cache belongs to. Renderer-side caches
    /// derived from those rows must invalidate when this moves.
    #[must_use]
    pub const fn cache_seq(&self) -> Option<u64> {
        self.cache_seq
    }

    #[must_use]
    pub fn max_offset(&self, visible_rows: usize) -> i64 {
        if self.geometry_known {
            // The history the daemon actually retains ends where the live grid
            // starts. Clamping to total_rows (history + visible) let the
            // viewport scroll a full screen past the oldest retained row,
            // which painted as a large blank region above real content.
            self.live_start_row.max(0)
        } else {
            self.view_offset
                .saturating_add(i64::try_from(visible_rows).unwrap_or(i64::MAX))
                .max(0)
        }
    }

    /// Sets the local offset and records any newly needed fetch. Returns true
    /// when the displayed window changed.
    pub fn set_view_offset(&mut self, offset: i64, visible_rows: usize) -> bool {
        let clamped = offset.clamp(0, self.max_offset(visible_rows));
        if clamped == self.view_offset {
            return false;
        }
        self.view_offset = clamped;
        self.sync_anchor();
        self.queue_missing_window(visible_rows);
        true
    }

    /// Re-pins the anchor to whatever content the window now shows. Returning
    /// to live drops the anchor so the view follows the bottom again.
    fn sync_anchor(&mut self) {
        self.anchor =
            (self.view_offset > 0).then(|| self.live_start_row.saturating_sub(self.view_offset));
    }

    pub fn scroll_by(&mut self, lines: i64, visible_rows: usize) -> bool {
        self.set_view_offset(self.view_offset.saturating_add(lines), visible_rows)
    }

    pub fn scroll_to_live(&mut self, visible_rows: usize) -> bool {
        self.set_view_offset(0, visible_rows)
    }

    /// Places an absolute history row at approximately `anchor` of the window.
    pub fn scroll_to_absolute(
        &mut self,
        absolute_row: i64,
        anchor: f32,
        visible_rows: usize,
    ) -> bool {
        let window_row = (anchor.clamp(0.0, 1.0) * visible_rows as f32).round() as i64;
        self.set_view_offset(
            self.live_start_row
                .saturating_add(window_row)
                .saturating_sub(absolute_row),
            visible_rows,
        )
    }

    #[must_use]
    pub fn absolute_row(&self, window_row: usize) -> i64 {
        self.live_start_row
            .saturating_sub(self.view_offset)
            .saturating_add(i64::try_from(window_row).unwrap_or(i64::MAX))
    }

    #[must_use]
    pub fn window_row_for_absolute(&self, absolute_row: i64) -> Option<i64> {
        absolute_row.checked_sub(self.absolute_row(0))
    }

    #[must_use]
    pub fn row_at_absolute(&self, buffer: &GridBuffer, absolute_row: i64) -> Vec<GridCell> {
        let cols = usize::from(buffer.cols);
        let source = if absolute_row >= self.live_start_row {
            usize::try_from(absolute_row - self.live_start_row)
                .ok()
                .and_then(|row| buffer.row(row))
        } else {
            self.cache.get(&absolute_row).map(Vec::as_slice)
        };
        normalized_row(source.unwrap_or_default(), cols)
    }

    #[must_use]
    pub fn window_row(&self, buffer: &GridBuffer, window_row: usize) -> Vec<GridCell> {
        if self.view_offset == 0 {
            return normalized_row(
                buffer.row(window_row).unwrap_or_default(),
                usize::from(buffer.cols),
            );
        }
        self.row_at_absolute(buffer, self.absolute_row(window_row))
    }

    #[must_use]
    pub fn compose(&self, buffer: &GridBuffer, visible_rows: usize) -> Vec<Vec<GridCell>> {
        (0..visible_rows)
            .map(|row| self.window_row(buffer, row))
            .collect()
    }

    /// Returns the next coalesced request and marks it in flight. Until it is
    /// completed, further viewport movement is merged into one queued range.
    pub fn begin_fetch(&mut self, visible_rows: usize) -> Option<ScrollbackRequest> {
        self.queue_missing_window(visible_rows);
        if self.in_flight.is_some() {
            return None;
        }
        let range = self.queued.take()?;
        if range.start >= range.end {
            return None;
        }
        self.in_flight = Some(range.clone());
        Some(ScrollbackRequest {
            first_row: range.start,
            max_rows: range.end - range.start,
        })
    }

    /// Completes the active request and ingests decoded rows. A content-sequence
    /// change invalidates all old cached absolute rows before inserting data.
    pub fn complete_fetch(
        &mut self,
        result: ReadScrollbackCellsResult,
        visible_rows: usize,
    ) -> Result<(), ScrollbackApplyError> {
        self.in_flight = None;
        let row_count = usize::try_from(result.row_count)
            .map_err(|_| ScrollbackApplyError::NegativeRowCount(result.row_count))?;
        let decoded = GridRowCodec::decode_rows(&result.payload, row_count)?;
        if decoded.len() != row_count {
            return Err(ScrollbackApplyError::RowCountMismatch {
                declared: row_count,
                decoded: decoded.len(),
            });
        }
        self.apply_rows(
            decoded,
            result.first_row,
            result.live_start_row,
            result.total_rows,
            result.content_seq,
            visible_rows,
        );
        Ok(())
    }

    pub fn fail_fetch(&mut self) {
        if let Some(range) = self.in_flight.take() {
            merge_range(&mut self.queued, range);
        }
    }

    pub fn apply_rows(
        &mut self,
        rows: Vec<Vec<GridCell>>,
        first_row: i64,
        live_start_row: i64,
        total_rows: i64,
        content_seq: u64,
        visible_rows: usize,
    ) {
        let old_sequence = self.cache_seq;
        if old_sequence != Some(content_seq) {
            self.cache.clear();
            self.cache_seq = Some(content_seq);
        }
        for (index, row) in rows.into_iter().enumerate() {
            let absolute = first_row.saturating_add(i64::try_from(index).unwrap_or(i64::MAX));
            self.cache.insert(absolute, row);
        }
        self.live_start_row = live_start_row;
        self.total_rows = total_rows.max(0);
        self.geometry_known = true;
        // Output that scrolls lines into history moves the live edge, not the
        // content being read: hold the anchored row in place and let the
        // distance to live grow instead. Without this the window slides onto
        // unfetched rows and refetches for as long as output keeps flowing.
        if let Some(anchor) = self.anchor {
            self.view_offset = self.live_start_row.saturating_sub(anchor);
        }
        self.view_offset = self.view_offset.clamp(0, self.max_offset(visible_rows));
        self.sync_anchor();
        self.cap_cache_near_viewport();
        // Recompute rather than replaying a range queued against old geometry
        // or rows the just-completed request may already have filled.
        self.queued = None;
        self.queue_missing_window(visible_rows);
    }

    /// Adopts the geometry/content sequence carried by a text scrollback
    /// snapshot used for find. This lets find be the first history feature used
    /// in a session while preserving the same absolute coordinate space.
    pub fn apply_geometry(
        &mut self,
        live_start_row: i64,
        total_rows: i64,
        content_seq: u64,
        visible_rows: usize,
    ) {
        self.apply_rows(
            Vec::new(),
            live_start_row,
            live_start_row,
            total_rows,
            content_seq,
            visible_rows,
        );
    }

    /// Alternate screen has no history. Entering it always returns to live.
    pub fn enter_alt_screen(&mut self) -> bool {
        if self.view_offset == 0 {
            return false;
        }
        self.view_offset = 0;
        self.anchor = None;
        self.queued = None;
        true
    }

    fn queue_missing_window(&mut self, visible_rows: usize) {
        if self.view_offset <= 0 || visible_rows == 0 {
            return;
        }
        let visible_rows = i64::try_from(visible_rows).unwrap_or(i64::MAX);
        let top = self.live_start_row.saturating_sub(self.view_offset);
        let needed_end = self.live_start_row.min(top.saturating_add(visible_rows));
        if top >= needed_end || (top..needed_end).all(|row| self.cache.contains_key(&row)) {
            return;
        }

        let request =
            top.saturating_sub(visible_rows).max(0)..needed_end.saturating_add(visible_rows);
        if self.in_flight.as_ref() == Some(&request) || self.queued.as_ref() == Some(&request) {
            return;
        }
        merge_range(&mut self.queued, request);
    }

    fn cap_cache_near_viewport(&mut self) {
        let anchor = self.live_start_row.saturating_sub(self.view_offset);
        while self.cache.len() > MAX_SCROLLBACK_CACHE_ROWS {
            let Some((&first, _)) = self.cache.first_key_value() else {
                break;
            };
            let Some((&last, _)) = self.cache.last_key_value() else {
                break;
            };
            if first.abs_diff(anchor) >= last.abs_diff(anchor) {
                self.cache.remove(&first);
            } else {
                self.cache.remove(&last);
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TerminalModes {
    pub alt_screen: bool,
    pub mouse_reporting: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum WheelDelta {
    PrecisePoints(f32),
    Lines(f32),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WheelEvent {
    pub delta: WheelDelta,
    pub col: u16,
    pub row: u16,
    pub visible_rows: u16,
    pub line_height: f32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WheelRoute {
    Local {
        lines: i64,
    },
    Daemon {
        direction: u8,
        lines: u16,
        col: u16,
        row: u16,
    },
}

/// Stateful precise-scroll accumulator plus the two Swift routing regimes.
#[derive(Clone, Copy, Debug, Default)]
pub struct ScrollRouter {
    accumulated_points: f32,
}

impl ScrollRouter {
    pub fn route(&mut self, modes: TerminalModes, event: WheelEvent) -> Option<WheelRoute> {
        let steps = match event.delta {
            WheelDelta::PrecisePoints(delta) => self.precise_steps(delta, event.line_height),
            WheelDelta::Lines(delta) => classic_steps(delta, event.visible_rows),
        }?;
        if !modes.alt_screen && !modes.mouse_reporting {
            return Some(WheelRoute::Local {
                lines: i64::from(steps),
            });
        }
        Some(WheelRoute::Daemon {
            direction: u8::from(steps < 0),
            lines: steps.unsigned_abs().min(u16::MAX.into()) as u16,
            col: event.col,
            row: event.row,
        })
    }

    fn precise_steps(&mut self, delta: f32, line_height: f32) -> Option<i32> {
        if delta == 0.0 {
            return None;
        }
        if self.accumulated_points != 0.0
            && delta.is_sign_positive() != self.accumulated_points.is_sign_positive()
        {
            self.accumulated_points = 0.0;
        }
        self.accumulated_points += delta;
        let per_line = line_height.max(8.0);
        let steps = (self.accumulated_points / per_line).trunc() as i32;
        if steps == 0 {
            return None;
        }
        self.accumulated_points -= steps as f32 * per_line;
        Some(steps)
    }
}

fn classic_steps(delta: f32, visible_rows: u16) -> Option<i32> {
    if delta == 0.0 {
        return None;
    }
    let magnitude = delta.abs().trunc() as i32;
    let velocity = if magnitude > 9 {
        i32::from(visible_rows).max(20)
    } else if magnitude > 5 {
        10
    } else if magnitude > 1 {
        3
    } else {
        1
    };
    Some(if delta.is_sign_positive() {
        velocity
    } else {
        -velocity
    })
}

fn normalized_row(source: &[GridCell], cols: usize) -> Vec<GridCell> {
    let mut row = vec![GridCell::BLANK; cols];
    let copied = source.len().min(cols);
    row[..copied].copy_from_slice(&source[..copied]);
    row
}

fn merge_range(target: &mut Option<Range<i64>>, incoming: Range<i64>) {
    if incoming.start >= incoming.end {
        return;
    }
    if let Some(existing) = target {
        existing.start = existing.start.min(incoming.start);
        existing.end = existing.end.max(incoming.end);
    } else {
        *target = Some(incoming);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::task::{Context, Poll, Waker};

    use diri_proto::grid::{GridRowCodec, TermColor, TermStyle};

    use super::*;

    fn cell(ch: char) -> GridCell {
        GridCell::new(
            u32::from(ch),
            TermColor::Default,
            TermColor::DefaultInverted,
            TermStyle::empty(),
        )
    }

    fn row(text: &str, cols: usize) -> Vec<GridCell> {
        let mut cells: Vec<_> = text.chars().map(cell).collect();
        cells.resize(cols, GridCell::BLANK);
        cells
    }

    struct FakeFetcher {
        rows: Vec<Vec<GridCell>>,
        calls: Mutex<Vec<Range<i64>>>,
    }

    impl ScrollbackFetcher for FakeFetcher {
        fn read_scrollback_cells<'a>(
            &'a self,
            _session_id: &'a SessionId,
            first_row: i64,
            max_rows: i64,
        ) -> FetchFuture<'a> {
            Box::pin(async move {
                self.calls
                    .lock()
                    .unwrap()
                    .push(first_row..first_row.saturating_add(max_rows));
                let start = usize::try_from(first_row.max(0)).unwrap_or(usize::MAX);
                let requested = usize::try_from(max_rows.max(0)).unwrap_or(usize::MAX);
                let fetched: Vec<_> = self
                    .rows
                    .iter()
                    .skip(start)
                    .take(requested)
                    .cloned()
                    .collect();
                Ok(ReadScrollbackCellsResult {
                    payload: GridRowCodec::encode_rows(&fetched)
                        .map_err(|error| ScrollbackFetchError::new(error.to_string()))?,
                    first_row,
                    row_count: i64::try_from(fetched.len()).unwrap_or(i64::MAX),
                    total_rows: i64::try_from(self.rows.len()).unwrap_or(i64::MAX),
                    live_start_row: i64::try_from(self.rows.len()).unwrap_or(i64::MAX),
                    cols: self
                        .rows
                        .first()
                        .map_or(0, |row| i64::try_from(row.len()).unwrap_or(i64::MAX)),
                    content_seq: 1,
                })
            })
        }
    }

    fn ready<T>(mut future: Pin<Box<dyn Future<Output = T> + Send + '_>>) -> T {
        let mut context = Context::from_waker(Waker::noop());
        match future.as_mut().poll(&mut context) {
            Poll::Ready(output) => output,
            Poll::Pending => panic!("fake fetcher unexpectedly yielded"),
        }
    }

    #[test]
    fn max_offset_stops_at_the_oldest_retained_row() {
        // 546 retained history rows + 77 visible = 623 total. Clamping to
        // total_rows let the viewport scroll a full screen past the oldest
        // retained row, painting blank above real content.
        let mut viewport = ScrollbackViewport::default();
        viewport.apply_geometry(546, 623, 1, 77);
        assert_eq!(viewport.max_offset(77), 546);
        viewport.scroll_by(1_000, 77);
        assert_eq!(
            viewport.view_offset(),
            546,
            "scrolling clamps at the oldest retained row"
        );
    }

    /// Drives fetch → complete → fetch with the wheel untouched, while the
    /// session scrolls `lines_per_trip` new lines into history during each
    /// round trip. Returns how many fetches it took to settle.
    fn fetches_until_settled(lines_per_trip: i64, visible_rows: usize) -> usize {
        const LIMIT: usize = 200;
        let mut viewport = ScrollbackViewport::default();
        let mut live_start = 2_000i64;
        let mut seq = 1u64;
        viewport.apply_geometry(
            live_start,
            live_start + visible_rows as i64,
            seq,
            visible_rows,
        );
        viewport.set_view_offset(300, visible_rows);

        let mut fetches = 0;
        while let Some(request) = viewport.begin_fetch(visible_rows) {
            fetches += 1;
            if fetches > LIMIT {
                return fetches;
            }
            live_start += lines_per_trip;
            seq += 1;
            let total = live_start + visible_rows as i64;
            let start = request.first_row.max(0).min(total);
            let end = (start + request.max_rows.max(0)).min(total);
            let rows: Vec<_> = (start..end).map(|_| row("history", 8)).collect();
            viewport
                .complete_fetch(
                    ReadScrollbackCellsResult {
                        payload: GridRowCodec::encode_rows(&rows).unwrap(),
                        first_row: start,
                        row_count: i64::try_from(rows.len()).unwrap(),
                        total_rows: total,
                        live_start_row: live_start,
                        cols: 8,
                        content_seq: seq,
                    },
                    visible_rows,
                )
                .unwrap();
        }
        fetches
    }

    #[test]
    fn heavy_output_does_not_chain_fetches_under_a_still_finger() {
        // Anchored to the live edge, any session emitting more than one screen
        // per round trip left every response stale by more than its prefetch
        // margin: the completion queued the next window and pumped it, without
        // end. The cliff sat exactly at visible_rows.
        for lines_per_trip in [0, 40, 77, 78, 200, 5_000] {
            let fetches = fetches_until_settled(lines_per_trip, 77);
            assert!(
                fetches <= 2,
                "{lines_per_trip} new lines per round trip chained {fetches} fetches"
            );
        }
    }

    #[test]
    fn a_scrolled_view_holds_its_content_as_the_live_edge_moves_away() {
        let mut viewport = ScrollbackViewport::default();
        viewport.apply_geometry(1_000, 1_040, 1, 40);
        viewport.set_view_offset(100, 40);
        let anchored = viewport.absolute_row(0);
        assert_eq!(anchored, 900);

        // 500 lines of build log land while the reader sits still.
        viewport.apply_rows(Vec::new(), 0, 1_500, 1_540, 2, 40);

        assert_eq!(
            viewport.absolute_row(0),
            anchored,
            "the anchored row stays under the window"
        );
        assert_eq!(
            viewport.view_offset(),
            600,
            "distance to live grows instead of the content sliding"
        );
    }

    #[test]
    fn returning_to_live_resumes_following_the_bottom() {
        let mut viewport = ScrollbackViewport::default();
        viewport.apply_geometry(1_000, 1_040, 1, 40);
        viewport.set_view_offset(100, 40);
        assert!(viewport.scroll_to_live(40));

        viewport.apply_rows(Vec::new(), 0, 1_500, 1_540, 2, 40);
        assert_eq!(viewport.view_offset(), 0, "live view still follows output");
        assert_eq!(viewport.absolute_row(0), 1_500);
    }

    #[test]
    fn an_anchor_older_than_retained_history_clamps_to_the_oldest_row() {
        let mut viewport = ScrollbackViewport::default();
        viewport.apply_geometry(1_000, 1_040, 1, 40);
        viewport.set_view_offset(1_000, 40);
        assert_eq!(viewport.absolute_row(0), 0);

        // History is full: the live edge stops moving and old rows are evicted
        // beneath the anchor. The view must stay inside what is retained.
        viewport.apply_rows(Vec::new(), 0, 1_000, 1_040, 2, 40);
        assert_eq!(viewport.view_offset(), 1_000);
        assert!(viewport.view_offset() <= viewport.max_offset(40));
    }

    #[test]
    fn viewport_composes_history_and_live_across_the_seam() {
        let mut viewport = ScrollbackViewport::default();
        viewport.apply_rows(vec![row("h6", 3), row("h7", 3)], 6, 8, 8, 1, 3);
        assert!(viewport.set_view_offset(2, 3));
        let mut live = GridBuffer::new(3, 3);
        live.cells = [row("L0", 3), row("L1", 3), row("L2", 3)].concat();

        let composed = viewport.compose(&live, 3);
        assert_eq!(composed, vec![row("h6", 3), row("h7", 3), row("L0", 3)]);
        assert_eq!(viewport.absolute_row(2), 8);
    }

    #[test]
    fn new_live_output_does_not_change_scrolled_offset() {
        let mut viewport = ScrollbackViewport::default();
        viewport.apply_rows(vec![], 0, 20, 20, 1, 4);
        viewport.set_view_offset(7, 4);
        let before = viewport.absolute_row(0);
        let mut live = GridBuffer::new(2, 4);
        live.cells[0] = cell('x');

        assert_eq!(viewport.view_offset(), 7);
        assert_eq!(viewport.absolute_row(0), before);
    }

    #[test]
    fn fetches_are_cached_and_coalesced_while_one_is_in_flight() {
        let mut viewport = ScrollbackViewport::default();
        viewport.apply_rows(vec![], 0, 100, 100, 1, 10);
        viewport.set_view_offset(10, 10);
        let first = viewport.begin_fetch(10).unwrap();
        assert_eq!(first.range(), 80..110);
        assert!(viewport.begin_fetch(10).is_none());

        viewport.set_view_offset(30, 10);
        assert!(viewport.begin_fetch(10).is_none());
        let response_rows: Vec<_> = (80..100).map(|_| row("cached", 8)).collect();
        viewport
            .complete_fetch(
                ReadScrollbackCellsResult {
                    payload: GridRowCodec::encode_rows(&response_rows).unwrap(),
                    first_row: 80,
                    row_count: 20,
                    total_rows: 100,
                    live_start_row: 100,
                    cols: 8,
                    content_seq: 1,
                },
                10,
            )
            .unwrap();

        let coalesced = viewport.begin_fetch(10).unwrap();
        assert_eq!(coalesced.range(), 60..90);
        assert_eq!(viewport.cached_row_count(), 20);
    }

    #[test]
    fn fake_async_fetcher_populates_cache_without_duplicate_reads() {
        let fetcher = FakeFetcher {
            rows: vec![
                row("zero", 5),
                row("one", 5),
                row("two", 5),
                row("three", 5),
            ],
            calls: Mutex::new(Vec::new()),
        };
        let session_id = SessionId::new("fake");
        let mut viewport = ScrollbackViewport::default();
        viewport.apply_rows(Vec::new(), 0, 4, 4, 1, 2);
        viewport.set_view_offset(2, 2);
        let request = viewport.begin_fetch(2).unwrap();
        let response = ready(Box::pin(request.fetch(&fetcher, &session_id))).unwrap();
        viewport.complete_fetch(response, 2).unwrap();

        assert_eq!(viewport.cached_row(2), Some(row("two", 5).as_slice()));
        assert!(viewport.begin_fetch(2).is_none());
        assert_eq!(*fetcher.calls.lock().unwrap(), vec![0..6]);
    }

    #[test]
    fn content_sequence_change_invalidates_cache() {
        let mut viewport = ScrollbackViewport::default();
        viewport.apply_rows(vec![row("old", 3)], 4, 5, 5, 1, 2);
        viewport.apply_rows(vec![row("new", 3)], 8, 9, 9, 2, 2);
        assert!(viewport.cached_row(4).is_none());
        assert_eq!(viewport.cached_row(8), Some(row("new", 3).as_slice()));
    }

    #[test]
    fn scrollback_cache_keeps_a_bounded_window_near_the_viewport() {
        let mut viewport = ScrollbackViewport::default();
        viewport.apply_rows(Vec::new(), 0, 2_000, 2_000, 1, 40);
        viewport.set_view_offset(1_000, 40);
        let rows = (0..2_000).map(|_| row("cached", 8)).collect();
        viewport.apply_rows(rows, 0, 2_000, 2_000, 1, 40);

        assert_eq!(viewport.cached_row_count(), MAX_SCROLLBACK_CACHE_ROWS);
        assert!(viewport.cached_row(1_000).is_some());
        assert!(viewport.cached_row(0).is_none());
        assert!(viewport.cached_row(1_999).is_none());
    }

    #[test]
    fn wheel_routing_accumulates_trackpad_and_respects_modes() {
        let mut router = ScrollRouter::default();
        let event = |delta| WheelEvent {
            delta: WheelDelta::PrecisePoints(delta),
            col: 3,
            row: 4,
            visible_rows: 24,
            line_height: 10.0,
        };
        assert_eq!(router.route(TerminalModes::default(), event(4.0)), None);
        assert_eq!(
            router.route(TerminalModes::default(), event(7.0)),
            Some(WheelRoute::Local { lines: 1 })
        );
        assert_eq!(
            router.route(
                TerminalModes {
                    alt_screen: false,
                    mouse_reporting: true,
                },
                event(-12.0),
            ),
            Some(WheelRoute::Daemon {
                direction: 1,
                lines: 1,
                col: 3,
                row: 4,
            })
        );
    }
}
