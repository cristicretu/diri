//! Immutable styled search rows and process-wide admission limits.
//!
//! A reservation precedes IPC and survives through every shared view of the
//! decoded rows. Four 8 MiB reservations bound retained data across windows;
//! one capture at a time bounds transient control/decode work. No new mutex or
//! output-path work is introduced.
use std::mem::size_of;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use diri_proto::grid::{GridCell, GridRowCodec, RowMetadata};
use diri_proto::{CaptureFindResult, FIND_CAPTURE_MAX_CELLS, FIND_CAPTURE_MAX_ROWS};

use crate::buffer::GridBuffer;

pub const RETAINED_FIND_BYTES: usize = 8 * 1024 * 1024;
pub const RETAINED_FIND_VIEWS: usize = 4;
static VIEWS: AtomicUsize = AtomicUsize::new(0);
static RETAINED: AtomicUsize = AtomicUsize::new(0);
static CAPTURING: AtomicBool = AtomicBool::new(false);

#[derive(Debug)]
pub struct FindReservation;
impl FindReservation {
    pub fn acquire() -> Option<Arc<Self>> {
        VIEWS
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count < RETAINED_FIND_VIEWS).then_some(count + 1)
            })
            .ok()?;
        Some(Arc::new(Self))
    }
}
impl Drop for FindReservation {
    fn drop(&mut self) {
        VIEWS.fetch_sub(1, Ordering::AcqRel);
    }
}

pub struct FindCapturePermit;
impl FindCapturePermit {
    pub fn acquire() -> Option<Self> {
        CAPTURING
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()?;
        Some(Self)
    }
}
impl Drop for FindCapturePermit {
    fn drop(&mut self) {
        CAPTURING.store(false, Ordering::Release);
    }
}

#[derive(Debug)]
pub struct RetainedFindSnapshot {
    pub owner: String,
    pub capture_revision: u64,
    pub content_seq: u64,
    pub first_row: i64,
    pub live_start_row: i64,
    pub cols: usize,
    pub visible_rows: usize,
    pub partial: bool,
    pub is_alt_screen: bool,
    rows: Vec<Box<[GridCell]>>,
    metadata: Vec<RowMetadata>,
    retained_bytes: usize,
    _reservation: Arc<FindReservation>,
    _memory: RetainedMemory,
}
impl PartialEq for RetainedFindSnapshot {
    fn eq(&self, other: &Self) -> bool {
        self.owner == other.owner && self.capture_revision == other.capture_revision
    }
}
impl Eq for RetainedFindSnapshot {}

#[derive(Debug)]
struct RetainedMemory(usize);
impl Drop for RetainedMemory {
    fn drop(&mut self) {
        RETAINED.fetch_sub(self.0, Ordering::AcqRel);
    }
}

impl RetainedFindSnapshot {
    /// Called off the GUI thread. Validate geometry and annotation allocations
    /// before expanding RLE; validate each row before retaining it.
    pub fn decode(
        result: CaptureFindResult,
        reservation: Arc<FindReservation>,
    ) -> Result<Arc<Self>, &'static str> {
        let cells = result.cells;
        let cols = usize::try_from(cells.cols).map_err(|_| "Invalid capture width")?;
        let count = usize::try_from(cells.row_count).map_err(|_| "Invalid capture height")?;
        if cols == 0
            || cols > 4096
            || count > FIND_CAPTURE_MAX_ROWS
            || count.saturating_mul(cols) > FIND_CAPTURE_MAX_CELLS
            || cells.first_row < 0
            || cells.live_start_row < cells.first_row
            || cells.first_row.saturating_add(count as i64) != cells.total_rows
            || cells.total_rows.saturating_sub(cells.live_start_row) != result.visible_rows as i64
            || (!cells.metadata.is_empty() && cells.metadata.len() != count)
            || cells.metadata.iter().any(|row| !row.validate(cols))
        {
            return Err("Invalid capture geometry");
        }
        let metadata_bytes = metadata_bytes(&cells.metadata);
        let bytes = count
            .saturating_mul(cols * size_of::<GridCell>() + size_of::<Box<[GridCell]>>())
            .saturating_add(metadata_bytes)
            .saturating_add(result.owner.capacity())
            .saturating_add(size_of::<Self>());
        if bytes > RETAINED_FIND_BYTES || cells.payload.len() > 3 * 1024 * 1024 {
            return Err("Search capture exceeds memory budget");
        }
        RETAINED
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                (current.saturating_add(bytes) <= RETAINED_FIND_BYTES * RETAINED_FIND_VIEWS)
                    .then_some(current + bytes)
            })
            .map_err(|_| "Search views are using the memory budget")?;
        let memory = RetainedMemory(bytes);
        let mut rows = Vec::with_capacity(count);
        let mut offset = 0;
        for _ in 0..count {
            let row = GridRowCodec::read_row(&cells.payload, &mut offset)
                .map_err(|_| "Invalid capture cells")?;
            if row.len() != cols {
                return Err("Invalid capture row width");
            }
            rows.push(row.into_boxed_slice());
        }
        if offset != cells.payload.len() {
            return Err("Trailing capture cells");
        }
        Ok(Arc::new(Self {
            owner: result.owner,
            capture_revision: result.capture_revision,
            content_seq: cells.content_seq,
            first_row: cells.first_row,
            live_start_row: cells.live_start_row,
            cols,
            visible_rows: result.visible_rows,
            partial: result.partial,
            is_alt_screen: result.is_alt_screen,
            rows,
            metadata: cells.metadata,
            retained_bytes: bytes,
            _reservation: reservation,
            _memory: memory,
        }))
    }

    pub fn retained_bytes(&self) -> usize {
        self.retained_bytes
    }
    pub fn row_count(&self) -> usize {
        self.rows.len()
    }
    pub fn row(&self, absolute: i64) -> Option<&[GridCell]> {
        self.rows
            .get(usize::try_from(absolute.checked_sub(self.first_row)?).ok()?)
            .map(AsRef::as_ref)
    }
    pub fn metadata(&self, absolute: i64) -> Option<&RowMetadata> {
        self.metadata
            .get(usize::try_from(absolute.checked_sub(self.first_row)?).ok()?)
    }
    pub fn row_text(&self, absolute: i64) -> Option<(String, Vec<[u16; 2]>)> {
        Some(GridBuffer::text_with_cell_ranges(
            self.row(absolute)?,
            self.metadata(absolute),
        ))
    }
    /// Proof is exact styled row and annotation equality at the same live row.
    /// History row offsets alone never authorize highlights on current output.
    pub fn matches_live_row(&self, absolute: i64, live: &GridBuffer) -> bool {
        let Ok(row) = usize::try_from(absolute - self.live_start_row) else {
            return false;
        };
        let left = self.metadata(absolute);
        let right = live.annotations.get(row);
        let annotations_match = match (left, right) {
            (Some(left), Some(right)) => left == right,
            (None, Some(row)) | (Some(row), None) => {
                row.links.is_empty() && row.graphemes.is_empty()
            }
            (None, None) => true,
        };
        usize::from(live.cols) == self.cols
            && self.row(absolute) == live.row(row)
            && annotations_match
    }
}
fn metadata_bytes(rows: &Vec<RowMetadata>) -> usize {
    rows.capacity() * size_of::<RowMetadata>()
        + rows
            .iter()
            .map(|row| {
                row.links.capacity() * size_of::<diri_proto::grid::LinkSpan>()
                    + row
                        .links
                        .iter()
                        .map(|link| link.uri.capacity())
                        .sum::<usize>()
                    + row.graphemes.capacity() * size_of::<(u16, String)>()
                    + row
                        .graphemes
                        .iter()
                        .map(|(_, text)| text.capacity())
                        .sum::<usize>()
            })
            .sum::<usize>()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::find::{FindSnapshot, SEARCH_DEBOUNCE, TerminalFindModel};
    use crate::scrollback::ScrollbackViewport;
    use diri_terminal_state::HeadlessScreen;
    use std::time::Duration;

    fn capture(screen: &HeadlessScreen, revision: u64) -> CaptureFindResult {
        let cells = screen.find_capture_cells().unwrap();
        CaptureFindResult {
            owner: "owner-a".into(),
            capture_revision: revision,
            session_id: diri_proto::SessionId::new("fixture"),
            is_alt_screen: screen.is_alt_screen(),
            visible_rows: screen.size().1,
            partial: cells.first_row > 0,
            cells,
        }
    }

    #[test]
    fn retained_rows_keep_identity_through_output_reflow_and_query_change_and_release_budget() {
        // All budget assertions stay in one test so parallel tests cannot
        // consume this process-wide admission allowance halfway through it.
        let base_views = VIEWS.load(Ordering::Acquire);
        let base_bytes = RETAINED.load(Ordering::Acquire);
        let mut model = TerminalFindModel::retained();
        let reservation = model.reservation().unwrap();
        let mut screen = HeadlessScreen::new(24, 3);
        screen.feed("old needle 界 e\u{301}\r\nsecond needle\r\nthird\r\nlive needle".as_bytes());
        let source =
            RetainedFindSnapshot::decode(capture(&screen, 1), reservation.clone()).unwrap();
        assert!(source.retained_bytes() < RETAINED_FIND_BYTES);
        let captured_oldest = source.row(0).unwrap().to_vec();
        let mut live = GridBuffer::default();
        live.apply(screen.full_snapshot());
        let mut viewport = ScrollbackViewport::default();
        model.set_query("needle", Duration::ZERO);
        let request = model.take_due_search(SEARCH_DEBOUNCE).unwrap();
        let result = model
            .prepare_search(&request, FindSnapshot::from(source.clone()), &live)
            .unwrap()
            .run();
        assert!(model.apply_result(result, &mut viewport));
        assert!(
            !model.is_paused(),
            "auto-selecting the first match must keep following output"
        );
        assert!(
            model
                .visible_spans_with_live(&viewport, &live)
                .iter()
                .all(|span| {
                    live.row_text_with_cell_ranges(span.row)
                        .unwrap()
                        .0
                        .contains("needle")
                })
        );
        // A same-sized screen rewrite invalidates live highlights immediately,
        // even before a new search can finish. Navigation shows the saved rows.
        screen.feed(b"\x1b[2J\x1b[Hreplacement without the queried word");
        live.apply(screen.full_snapshot());
        assert!(model.visible_spans_with_live(&viewport, &live).is_empty());
        model
            .navigate_with_live(true, &mut viewport, &live)
            .unwrap();
        assert!(model.is_paused());
        assert!(viewport.has_find_source(&source));
        assert_eq!(viewport.row_at_absolute(&live, 0), captured_oldest);
        let selected = model.matches()[model.current_index()].clone();
        // Trimming and reflow both renumber current history. Neither can alter
        // a selected source or start fetching unrelated rows into its view.
        screen.feed("unrelated rolling output\r\n".repeat(9000).as_bytes());
        screen.resize(40, 4);
        live.apply(screen.full_snapshot());
        assert!(!model.on_output(Duration::from_secs(2)));
        assert!(model.has_newer_output());
        assert!(viewport.begin_fetch(4).is_none());
        let latest = screen.scrollback_cells(0, 10);
        viewport.complete_fetch(latest, 4).unwrap();
        assert!(viewport.has_find_source(&source));
        assert_eq!(model.matches()[model.current_index()], selected);
        assert_eq!(
            &viewport.row_at_absolute(&live, 0)[..24],
            captured_oldest.as_slice()
        );
        assert!(viewport.scroll_to_live(4));
        assert!(
            !viewport.has_find_source(&source),
            "Return to live releases immutable reading content"
        );
        viewport.pin_find_source(source.clone(), selected.absolute_row, 4);
        model.set_query("界", Duration::from_secs(3));
        let query = model.take_due_search(Duration::from_secs(4)).unwrap();
        assert_eq!(model.paused_source().unwrap(), source);
        let result = model
            .prepare_search(&query, FindSnapshot::from(source.clone()), &live)
            .unwrap()
            .run();
        assert!(model.apply_result(result, &mut viewport));
        assert_eq!(model.matches().len(), 1);
        assert_eq!(
            model.matches()[0].end_col_exclusive - model.matches()[0].start_col,
            2
        );
        model.refresh(Duration::from_secs(5));
        viewport.clear_find_source();
        assert!(!model.is_paused());
        let refresh = model.take_due_search(Duration::from_secs(5)).unwrap();
        let replacement =
            RetainedFindSnapshot::decode(capture(&screen, 2), reservation.clone()).unwrap();
        let result = model
            .prepare_search(&refresh, FindSnapshot::from(replacement), &live)
            .unwrap()
            .run();
        assert!(model.apply_result(result, &mut viewport));
        assert!(model.matches().is_empty());
        model.refresh(Duration::from_secs(6));
        let request = model.take_due_search(Duration::from_secs(6)).unwrap();
        let mut other_owner = capture(&screen, 0);
        other_owner.owner = "replacement-session-owner".into();
        let other_owner = RetainedFindSnapshot::decode(other_owner, reservation.clone()).unwrap();
        let result = model
            .prepare_search(&request, FindSnapshot::from(other_owner), &live)
            .unwrap()
            .run();
        assert!(model.apply_result(result, &mut viewport));
        assert!(model.error().unwrap().contains("Session changed"));
        assert_eq!(model.retained_highlights().unwrap().0.owner, "owner-a");
        drop((model, source, reservation, viewport));
        assert_eq!(VIEWS.load(Ordering::Acquire), base_views);
        assert_eq!(RETAINED.load(Ordering::Acquire), base_bytes);

        let reservations: Vec<_> = (base_views..RETAINED_FIND_VIEWS)
            .map(|_| FindReservation::acquire().unwrap())
            .collect();
        assert!(FindReservation::acquire().is_none());
        drop(reservations);
        let permit = FindCapturePermit::acquire().unwrap();
        assert!(FindCapturePermit::acquire().is_none());
        drop(permit);
        assert!(FindCapturePermit::acquire().is_some());

        // Decoder rejects geometry before allocating expanded rows and returns
        // its admission/byte reservation after every malformed response.
        let reservation = FindReservation::acquire().unwrap();
        let mut oversized = capture(&screen, 3);
        oversized.cells.row_count = FIND_CAPTURE_MAX_ROWS as i64 + 1;
        assert!(RetainedFindSnapshot::decode(oversized, reservation.clone()).is_err());
        let mut wrong_width = capture(&screen, 4);
        wrong_width.cells.cols += 1;
        assert!(RetainedFindSnapshot::decode(wrong_width, reservation.clone()).is_err());
        assert_eq!(RETAINED.load(Ordering::Acquire), base_bytes);
        drop(reservation);
        assert_eq!(VIEWS.load(Ordering::Acquire), base_views);
    }
}
