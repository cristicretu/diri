//! Lossless cold row blocks with explicitly owned decode caches.
//!
//! Rows use the grid storage order: newest first. References returned by `row`
//! remain valid until exclusive access is regained. Cache reclamation therefore
//! requires `&mut self`; it never evicts behind an outstanding shared reference.

use std::collections::hash_map::Entry;
use std::collections::{HashMap, VecDeque};
use std::hash::{BuildHasherDefault, Hash, Hasher};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use super::{GridCell, Row};
use crate::term::cell::ResetDiscriminant;

mod lz;
use crate::term::cell::{Cell, CellExtra, Flags, Hyperlink};
use crate::vte::ansi::{Color, NamedColor, Rgb};

const MAX_BLOCK_ROWS: usize = 64;
/// The most decoded cells deferred history keeps for lines that are
/// reflowed eagerly (see [`LineExtra::Eager`]).
const MAX_EAGER_BYTES: usize = 256 * 1024;
const BLOCK_CELL_BYTES: usize = 128 * 1024;

/// The codec is a typed storage operation, never a second terminal parser.
#[derive(Debug)]
pub struct RowCodec<T> {
    /// Encodes at most 64 rows. A row may be marked `blank_on_reset` only
    /// when it has the first row's width, is not empty, and every cell at or
    /// beyond its occupancy equals `T::default()`. `Row::reset` of such a row
    /// yields only template cells: when the template's discriminant differs
    /// from the last (default) cell's, every cell is reset; otherwise the
    /// cells it keeps are default cells, and for the cell type a template
    /// with the default discriminant resets any cell to the default cell.
    /// `Cell` resets to the template's background and default attributes.
    encode: EncodeRows<T>,
    /// `encode`, then leaves every row default cells with occupancy zero,
    /// as `clear_row` would: for rows that are kept as spare allocations.
    encode_recycled: fn(&mut [Row<T>]) -> Encoded,
    decode: fn(&[u8]) -> Vec<Row<T>>,
    resize_row: fn(&mut Row<T>, usize),
    /// Decompresses a payload once for single-row access by `reset_row`.
    index: fn(&[u8]) -> EvictIndex<T>,
    /// The row at `row` of an indexed payload, already reset to `template`:
    /// exactly `Row::reset` of the decoded row, without materializing cells
    /// that the reset overwrites. `None` when the encoded width is not
    /// `columns`, so the caller must decode and resize first.
    reset_row: ResetRow<T>,
    /// What a column change needs to know about a row without its cells
    /// (see [`RowShape`]), with its wide character runs, relative to the
    /// row, appended to the vector; `None` when a soft wrap is not the
    /// row's last cell, so reflow may join it to another logical line.
    shape: ShapeRow<T>,
    /// The rows of one greedy logical line at `columns`, oldest first: its
    /// source rows (oldest first, all but the newest soft-wrapped), the
    /// content cells already dropped from its start and its content cells.
    line_rows: LineRows<T>,
    /// Eager reflow of one logical line's rows (oldest first, all but the
    /// newest soft-wrapped) to `columns`: exactly what `Grid::resize` does
    /// to them in history.
    reflow_line: fn(Vec<Row<T>>, usize) -> Vec<Row<T>>,
    clone_row: fn(&Row<T>) -> Row<T>,
    /// Reset a row to default cells: equal to `Row::new` of its width.
    clear_row: fn(&mut Row<T>),
    #[cfg(test)]
    rows_equal: RowsEqual<T>,
}

type EncodeRows<T> = fn(&[Row<T>]) -> Encoded;
type LineRows<T> = fn(Vec<Row<T>>, usize, usize, usize, &mut Vec<Row<T>>);
type ShapeRow<T> = fn(&Row<T>, &mut Vec<(u32, u32)>) -> Option<RowShape>;
#[cfg(test)]
type RowsEqual<T> = fn(&[Row<T>], &[Row<T>]) -> bool;

/// A row's part in a logical line, for deferring a column change.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RowShape {
    /// The row soft-wraps into the next (newer) row.
    wraps: bool,
    /// The row's last cell is the spacer before a wide character that did
    /// not fit; it is not content.
    padded: bool,
    /// Cells through the last cell that is not empty, ignoring the wrap
    /// flag of a wrapping row's last cell.
    content: usize,
    /// Every cell after `content` is the default cell.
    default_tail: bool,
    /// Wide characters are whole, a leading spacer only ends a wrapping
    /// row, and every cell above the occupancy is a default cell.
    plain: bool,
    /// The first cell is a wide character.
    starts_wide: bool,
}

/// A logical line of history whose column changes are deferred.
///
/// History reflow never moves cells across a hard line break, so each
/// logical line reflows on its own. Eager reflow of a greedy line (see
/// [`CompactRows::prepare_reflow`]) always yields its content cells laid out
/// greedily in rows of the terminal width: a row ends with a soft wrap when
/// the next cell (or wide character) does not fit, a wide character that
/// would start in the last column leaves a leading spacer there, and the last
/// row is padded with default cells. Its rows are therefore known from
/// `cells`, `skip` and `wide` alone at any width, whatever widths it went
/// through. Other lines are kept as rows (see [`LineExtra::Eager`]).
#[derive(Clone, Debug)]
struct DeferredLine<T> {
    /// Source rows holding the line.
    rows: u32,
    /// Content cells in the source rows: every cell of the soft-wrapped
    /// rows (without a trailing leading spacer), then the newest row's cells
    /// through its last one that is not empty. Cells after that are default
    /// cells.
    cells: u32,
    /// `None` for a greedy line of single-width cells with nothing dropped.
    extra: Option<LineExtra<T>>,
}

#[derive(Clone, Debug)]
enum LineExtra<T> {
    Greedy {
        /// Leading content cells already dropped with the oldest rows.
        skip: u32,
        /// Runs of wide characters as (first content cell, characters).
        wide: Option<Box<[(u32, u32)]>>,
    },
    /// Any other line (styled blanks after its content, for example), kept
    /// as its rows at the current width, oldest first, and reflowed eagerly.
    Eager(Vec<Row<T>>),
}

impl<T> LineExtra<T> {
    fn eager_cells(&self) -> usize {
        match self {
            LineExtra::Eager(rows) => rows.iter().map(Row::len).sum(),
            LineExtra::Greedy { .. } => 0,
        }
    }
}

/// Rows at `columns` of a line with `cells` and `extra`, and the content
/// cells the first of them holds.
fn line_layout<T>(cells: u32, extra: Option<&LineExtra<T>>, columns: usize) -> (usize, usize) {
    let (skip, runs) = match extra {
        None => (0, None),
        Some(LineExtra::Eager(rows)) => return (rows.len(), 0),
        Some(LineExtra::Greedy { skip, wide }) => (*skip as usize, wide.as_deref()),
    };
    let end = cells as usize;
    let Some(runs) = runs else {
        let cells = end - skip;
        return (cells.div_ceil(columns).max(1), cells.min(columns));
    };
    // Deferred history with wide characters is never narrower than 2.
    debug_assert!(columns >= 2);
    let mut layout = Layout {
        columns,
        rows: 1,
        column: 0,
        position: skip,
        first: None,
    };
    for &(start, count) in runs {
        let (start, end) = (start as usize, start as usize + 2 * count as usize);
        if end > layout.position {
            layout.narrow(start.max(layout.position));
            layout.wide((end - layout.position) / 2);
        }
    }
    layout.narrow(end);
    (layout.rows, layout.first.unwrap_or(end) - skip)
}

impl<T> DeferredLine<T> {
    fn layout(&self, columns: usize) -> (usize, usize) {
        line_layout(self.cells, self.extra.as_ref(), columns)
    }

    fn rows_at(&self, columns: usize) -> usize {
        self.layout(columns).0
    }

    fn eager_cells(&self) -> usize {
        self.extra.as_ref().map_or(0, LineExtra::eager_cells)
    }

    /// The line's rows at `columns`, oldest first, from its source rows.
    fn into_rows(self, sources: Vec<Row<T>>, columns: usize, codec: RowCodec<T>) -> Vec<Row<T>> {
        let skip = match self.extra {
            Some(LineExtra::Eager(rows)) => return rows,
            Some(LineExtra::Greedy { skip, .. }) => skip,
            None => 0,
        };
        let mut rows = Vec::new();
        (codec.line_rows)(
            sources,
            skip as usize,
            self.cells as usize,
            columns,
            &mut rows,
        );
        rows
    }
}

/// A packed [`DeferredLine`]: most lines have no extra.
#[derive(Clone, Copy, Debug)]
struct PackedLine {
    /// Source rows, with [`HAS_EXTRA`].
    rows: u32,
    cells: u32,
}

const HAS_EXTRA: u32 = 1 << 31;

/// Deferred lines, newest first, 8 bytes each; the extras of the lines that
/// have one are kept in the same order.
#[derive(Clone, Debug)]
struct Lines<T> {
    packed: VecDeque<PackedLine>,
    extras: VecDeque<LineExtra<T>>,
    /// Heap bytes the extras own, kept current so the history bound, which
    /// measures storage after every eviction, does not walk every line.
    extra_heap: usize,
}

impl<T> LineExtra<T> {
    fn heap_bytes(&self) -> usize {
        match self {
            LineExtra::Greedy { wide, .. } => wide
                .as_ref()
                .map_or(0, |runs| std::mem::size_of_val(&**runs)),
            LineExtra::Eager(rows) => rows.capacity() * std::mem::size_of::<Row<T>>(),
        }
    }
}

impl<T> Lines<T> {
    fn new() -> Self {
        Self {
            packed: VecDeque::new(),
            extras: VecDeque::new(),
            extra_heap: 0,
        }
    }

    fn is_empty(&self) -> bool {
        self.packed.is_empty()
    }

    fn pack(line: DeferredLine<T>) -> (PackedLine, Option<LineExtra<T>>) {
        debug_assert!(line.rows < HAS_EXTRA);
        let flag = if line.extra.is_some() { HAS_EXTRA } else { 0 };
        let packed = PackedLine {
            rows: line.rows | flag,
            cells: line.cells,
        };
        (packed, line.extra)
    }

    fn push_front(&mut self, line: DeferredLine<T>) {
        let (packed, extra) = Self::pack(line);
        self.packed.push_front(packed);
        if let Some(extra) = extra {
            self.extra_heap += extra.heap_bytes();
            self.extras.push_front(extra);
        }
    }

    fn push_back(&mut self, line: DeferredLine<T>) {
        let (packed, extra) = Self::pack(line);
        self.packed.push_back(packed);
        if let Some(extra) = extra {
            self.extra_heap += extra.heap_bytes();
            self.extras.push_back(extra);
        }
    }

    fn unpack(packed: PackedLine, extra: impl FnOnce() -> LineExtra<T>) -> DeferredLine<T> {
        DeferredLine {
            rows: packed.rows & !HAS_EXTRA,
            cells: packed.cells,
            extra: (packed.rows & HAS_EXTRA != 0).then(extra),
        }
    }

    fn pop_front(&mut self) -> Option<DeferredLine<T>> {
        let packed = self.packed.pop_front()?;
        Some(Self::unpack(packed, || {
            let extra = self.extras.pop_front().expect("line extra");
            self.extra_heap -= extra.heap_bytes();
            extra
        }))
    }

    fn pop_back(&mut self) -> Option<DeferredLine<T>> {
        let packed = self.packed.pop_back()?;
        Some(Self::unpack(packed, || {
            let extra = self.extras.pop_back().expect("line extra");
            self.extra_heap -= extra.heap_bytes();
            extra
        }))
    }

    /// The oldest line's rows at `columns`.
    fn back_rows_at(&self, columns: usize) -> Option<usize> {
        let packed = self.packed.back()?;
        let extra = (packed.rows & HAS_EXTRA != 0).then(|| self.extras.back().expect("line extra"));
        Some(line_layout(packed.cells, extra, columns).0)
    }

    /// Whether the oldest line is kept as rows (see [`LineExtra::Eager`]).
    fn back_is_eager(&self) -> bool {
        self.packed
            .back()
            .is_some_and(|packed| packed.rows & HAS_EXTRA != 0)
            && matches!(self.extras.back(), Some(LineExtra::Eager(_)))
    }

    /// Each line's source rows, content cells and extra, newest first.
    fn iter(&self) -> impl Iterator<Item = (u32, u32, Option<&LineExtra<T>>)> {
        let mut extras = self.extras.iter();
        self.packed.iter().map(move |packed| {
            let extra = (packed.rows & HAS_EXTRA != 0).then(|| extras.next().expect("line extra"));
            (packed.rows & !HAS_EXTRA, packed.cells, extra)
        })
    }

    fn heap_bytes(&self) -> usize {
        debug_assert_eq!(
            self.extra_heap,
            self.extras.iter().map(LineExtra::heap_bytes).sum::<usize>()
        );
        self.packed.capacity() * std::mem::size_of::<PackedLine>()
            + self.extras.capacity() * std::mem::size_of::<LineExtra<T>>()
            + self.extra_heap
    }

    /// `heap_bytes` with the line indexes at their lengths.
    fn live_bytes(&self) -> usize {
        self.packed.len() * std::mem::size_of::<PackedLine>()
            + self.extras.len() * std::mem::size_of::<LineExtra<T>>()
            + self.extra_heap
    }

    /// Recompute `extra_heap` after extras were changed in place.
    fn recount_heap(&mut self) {
        self.extra_heap = self.extras.iter().map(LineExtra::heap_bytes).sum();
    }
}

/// Greedy placement of content cells in rows, counting rows.
struct Layout {
    columns: usize,
    rows: usize,
    column: usize,
    position: usize,
    /// The content position the second row starts at.
    first: Option<usize>,
}

impl Layout {
    fn break_row(&mut self) {
        self.rows += 1;
        self.first.get_or_insert(self.position);
        self.column = 0;
    }

    /// Place single-width cells up to content position `to`.
    fn narrow(&mut self, to: usize) {
        while self.position < to {
            if self.column == self.columns {
                self.break_row();
            }
            let take = (to - self.position).min(self.columns - self.column);
            self.column += take;
            self.position += take;
        }
    }

    /// Place `count` wide characters.
    fn wide(&mut self, mut count: usize) {
        while count != 0 {
            if self.column + 2 > self.columns {
                self.break_row();
            }
            let take = count.min((self.columns - self.column) / 2);
            self.column += 2 * take;
            self.position += 2 * take;
            count -= take;
        }
    }
}

/// History older than the rows a column change reflowed, kept at the width
/// it was stored at. Rows are produced at `columns` when read.
#[derive(Clone, Debug)]
struct Deferred<T> {
    /// Source blocks, newest first. Rows decode at `Block::columns`.
    sources: VecDeque<Block<T>>,
    /// Logical lines, newest first; together they hold every source row.
    lines: Lines<T>,
    columns: usize,
    /// Rows at `columns`.
    rows: usize,
    /// The payload term of `history_storage_bytes` for `sources`.
    payload: usize,
    /// Cells of eager lines' rows.
    eager_cells: usize,
    /// Every row at `columns`, newest first, built by the first read.
    read: OnceLock<Vec<Row<T>>>,
}

impl<T> Deferred<T> {
    /// `rows`, `payload` and `eager_cells` computed from scratch.
    fn counted(&self) -> (usize, usize, usize) {
        let rows = self
            .lines
            .iter()
            .map(|(_, cells, extra)| line_layout(cells, extra, self.columns).0)
            .sum();
        let eager = self
            .lines
            .iter()
            .map(|(_, _, extra)| extra.map_or(0, LineExtra::eager_cells))
            .sum();
        let mut previous = std::ptr::null();
        let mut payload = 0;
        for block in &self.sources {
            let ptr = block.bytes.as_ptr();
            if ptr != previous {
                payload += payload_bytes(block);
            }
            previous = ptr;
        }
        (rows, payload, eager)
    }

    /// Give back index capacity that evicted lines left behind, which the
    /// history budget does not count.
    fn shrink_evicted(&mut self) {
        debug_assert!(self.lines.heap_bytes() >= self.lines.live_bytes());
        let slack = |len: usize| len.saturating_mul(2).saturating_add(64);
        if self.lines.packed.capacity() > slack(self.lines.packed.len()) {
            self.lines.packed.shrink_to_fit();
        }
        if self.lines.extras.capacity() > slack(self.lines.extras.len()) {
            self.lines.extras.shrink_to_fit();
        }
        if self.sources.capacity() > slack(self.sources.len()) {
            self.sources.shrink_to_fit();
        }
    }

    fn recount(&mut self) {
        (self.rows, self.payload, self.eager_cells) = self.counted();
    }

    /// Pop an emptied source block from either end, keeping `payload`.
    fn pop_source(&mut self, front: bool) {
        let block = if front {
            self.sources.pop_front()
        } else {
            self.sources.pop_back()
        };
        let block = block.expect("deferred source block");
        let neighbour = if front {
            self.sources.front()
        } else {
            self.sources.back()
        };
        if !neighbour.is_some_and(|next| next.bytes.as_ptr() == block.bytes.as_ptr()) {
            self.payload -= payload_bytes(&block);
        }
    }

    /// Remove the newest line's rows at `columns`, oldest first.
    fn pop_newest_line(&mut self, codec: RowCodec<T>) -> Vec<Row<T>> {
        let line = self.lines.pop_front().expect("deferred line");
        self.rows -= line.rows_at(self.columns);
        self.eager_cells -= line.eager_cells();
        let sources = match line.extra {
            Some(LineExtra::Eager(_)) => {
                self.drop_newest_sources(line.rows as usize);
                Vec::new()
            }
            _ => {
                let mut sources = self.take_newest_sources(line.rows as usize, codec);
                sources.reverse();
                sources
            }
        };
        line.into_rows(sources, self.columns, codec)
    }

    /// Remove the oldest line's rows at `columns`, oldest first.
    fn pop_oldest_line(&mut self, codec: RowCodec<T>) -> Vec<Row<T>> {
        let line = self.lines.pop_back().expect("deferred line");
        self.rows -= line.rows_at(self.columns);
        self.eager_cells -= line.eager_cells();
        let sources = match line.extra {
            Some(LineExtra::Eager(_)) => {
                self.drop_oldest_sources(line.rows as usize);
                Vec::new()
            }
            _ => self.take_oldest_sources(line.rows as usize, codec),
        };
        line.into_rows(sources, self.columns, codec)
    }

    /// Drop the oldest line and its source rows without decoding them.
    fn drop_oldest_line(&mut self) {
        let line = self.lines.pop_back().expect("deferred line");
        self.rows -= line.rows_at(self.columns);
        self.eager_cells -= line.eager_cells();
        self.drop_oldest_sources(line.rows as usize);
    }

    /// Drop the oldest row: the start of the oldest line.
    fn drop_oldest_row(&mut self) {
        let mut line = self.lines.pop_back().expect("deferred line");
        let (rows, first) = line.layout(self.columns);
        if rows == 1 {
            self.rows -= 1;
            self.eager_cells -= line.eager_cells();
            self.drop_oldest_sources(line.rows as usize);
            return;
        }
        // Eager reflow would leave the rest of the line as the oldest line.
        match &mut line.extra {
            None => {
                line.extra = Some(LineExtra::Greedy {
                    skip: first as u32,
                    wide: None,
                });
            }
            Some(LineExtra::Greedy { skip, .. }) => *skip += first as u32,
            Some(LineExtra::Eager(rows)) => self.eager_cells -= rows.remove(0).len(),
        }
        self.lines.push_back(line);
        self.rows -= 1;
    }

    /// Every row at `columns`, newest first.
    fn build_rows(&self, codec: RowCodec<T>) -> Vec<Row<T>> {
        let mut out = Vec::with_capacity(self.rows);
        self.for_each_row_chunk(codec, usize::MAX, |rows| out.append(rows));
        out
    }

    /// [`Self::build_rows`] handed to `sink` in chunks of exactly `chunk`
    /// rows (the last one shorter), newest first, so a caller that encodes
    /// each chunk never holds every reflowed row at once. `sink` takes the
    /// rows it keeps out of the buffer.
    fn for_each_row_chunk(
        &self,
        codec: RowCodec<T>,
        chunk: usize,
        mut sink: impl FnMut(&mut Vec<Row<T>>),
    ) {
        let mut out = Vec::with_capacity(self.rows.min(chunk));
        let mut flush = |out: &mut Vec<Row<T>>, all: bool| {
            while out.len() >= chunk || (all && !out.is_empty()) {
                if out.len() > chunk {
                    let rest = out.split_off(chunk);
                    sink(out);
                    out.clear();
                    out.extend(rest);
                } else {
                    sink(out);
                    out.clear();
                }
            }
        };
        let mut sources = self.sources.iter();
        let mut decoded = Vec::new().into_iter();
        let mut chunks = Vec::new();
        for (source_rows, cells, extra) in self.lines.iter() {
            let mut rows = Vec::with_capacity(source_rows as usize);
            while rows.len() < source_rows as usize {
                match decoded.next() {
                    Some(row) => rows.push(row),
                    None => {
                        let block = sources.next().expect("deferred source row");
                        decoded = block.decode_rows(codec).into_iter();
                    }
                }
            }
            let skip = match extra {
                Some(LineExtra::Eager(rows)) => {
                    out.extend(rows.iter().rev().map(codec.clone_row));
                    flush(&mut out, false);
                    continue;
                }
                Some(LineExtra::Greedy { skip, .. }) => *skip as usize,
                None => 0,
            };
            rows.reverse();
            (codec.line_rows)(rows, skip, cells as usize, self.columns, &mut chunks);
            out.extend(chunks.drain(..).rev());
            flush(&mut out, false);
        }
        flush(&mut out, true);
    }

    /// Remove the newest `count` source rows, newest first.
    fn take_newest_sources(&mut self, mut count: usize, codec: RowCodec<T>) -> Vec<Row<T>> {
        let mut rows = Vec::with_capacity(count);
        while count != 0 {
            let block = self.sources.front_mut().expect("deferred source row");
            let take = count.min(block.count);
            let mut decoded = block
                .decoded
                .take()
                .unwrap_or_else(|| block.decode_rows(codec));
            let rest = decoded.split_off(take);
            rows.append(&mut decoded);
            block.start += take as u32;
            block.count -= take;
            if block.count == 0 {
                self.pop_source(true);
            } else {
                let _ = block.decoded.set(rest);
            }
            count -= take;
        }
        rows
    }

    /// Drop the newest `count` source rows.
    fn drop_newest_sources(&mut self, mut count: usize) {
        while count != 0 {
            let block = self.sources.front_mut().expect("deferred source row");
            let take = count.min(block.count);
            block.start += take as u32;
            block.count -= take;
            if let Some(rows) = block.decoded.get_mut() {
                rows.drain(..take);
            }
            if block.count == 0 {
                self.pop_source(true);
            }
            count -= take;
        }
    }

    /// Drop the oldest `count` source rows.
    fn drop_oldest_sources(&mut self, mut count: usize) {
        while count != 0 {
            let block = self.sources.back_mut().expect("deferred source row");
            let take = count.min(block.count);
            block.count -= take;
            if let Some(rows) = block.decoded.get_mut() {
                rows.truncate(block.count);
            }
            if block.count == 0 {
                self.pop_source(false);
            }
            count -= take;
        }
    }

    /// Remove the oldest `count` source rows, oldest first. The rest of a
    /// partly taken block stays decoded for the next oldest line.
    fn take_oldest_sources(&mut self, mut count: usize, codec: RowCodec<T>) -> Vec<Row<T>> {
        let mut rows = Vec::with_capacity(count);
        while count != 0 {
            let block = self.sources.back_mut().expect("deferred source row");
            let take = count.min(block.count);
            let mut decoded = block
                .decoded
                .take()
                .unwrap_or_else(|| block.decode_rows(codec));
            let taken = decoded.split_off(decoded.len() - take);
            rows.extend(taken.into_iter().rev());
            block.count -= take;
            if block.count == 0 {
                self.pop_source(false);
            } else {
                let _ = block.decoded.set(decoded);
            }
            count -= take;
        }
        rows
    }
}

/// An encoded block and the facts storage needs without decoding it.
struct Encoded {
    bytes: Arc<[u8]>,
    /// The narrowest width every row can reflow to without discarding
    /// content, or `None` when any row soft-wraps.
    resize_floor: Option<usize>,
    /// Bit `i` is set when `Row::reset` of payload row `i`, at the block's
    /// encoded width, overwrites every cell for any template (see
    /// [`RowCodec::encode`]). Recycling such a row needs no payload access.
    blank_on_reset: u64,
}
type ResetRow<T> = fn(&EvictIndex<T>, usize, &T, usize) -> Option<Row<T>>;

/// A decompressed history payload with row offsets, kept for the oldest block
/// while bounded-history scrolling recycles its rows one at a time.
#[derive(Clone, Debug)]
pub struct EvictIndex<T> {
    raw: Vec<u8>,
    rows: Vec<usize>,
    styles: Vec<T>,
}

#[derive(Clone, Debug)]
struct EvictCache<T> {
    bytes: Arc<[u8]>,
    index: EvictIndex<T>,
}

impl<T> Copy for RowCodec<T> {}
impl<T> Clone for RowCodec<T> {
    fn clone(&self) -> Self {
        *self
    }
}

#[derive(Clone, Debug)]
struct Block<T> {
    bytes: Arc<[u8]>,
    /// First payload row of this range (payloads hold at most 64 rows). The
    /// narrow type keeps `Block`, which the history budget counts, the same
    /// size with `blank_on_reset`.
    start: u32,
    count: usize,
    columns: usize,
    resize_floor: Option<usize>,
    decoded: OnceLock<Vec<Row<T>>>,
    dirty: bool,
    /// Payload rows that recycle as template cells (see `Encoded`). Cleared
    /// when `columns` no longer matches the encoded width.
    blank_on_reset: u64,
}

impl<T> Block<T> {
    fn new(rows: &[Row<T>], codec: RowCodec<T>) -> Self {
        Self::encoded(rows, (codec.encode)(rows))
    }

    /// A block of `rows`, which are left default cells with occupancy zero
    /// (see [`RowCodec::encode_recycled`]).
    fn recycling(rows: &mut [Row<T>], codec: RowCodec<T>) -> Self {
        let encoded = (codec.encode_recycled)(rows);
        Self::encoded(rows, encoded)
    }

    fn encoded(rows: &[Row<T>], encoded: Encoded) -> Self {
        Self {
            bytes: encoded.bytes,
            start: 0,
            count: rows.len(),
            columns: rows.first().map_or(0, Row::len),
            resize_floor: encoded.resize_floor,
            decoded: OnceLock::new(),
            dirty: false,
            blank_on_reset: encoded.blank_on_reset,
        }
    }

    /// Whether the oldest row recycles as template cells without decoding.
    fn oldest_blank_on_reset(&self) -> bool {
        self.decoded.get().is_none()
            && self.count != 0
            && self.blank_on_reset >> (self.start as usize + self.count - 1) & 1 == 1
    }

    fn rows(&self, codec: RowCodec<T>) -> &[Row<T>] {
        self.decoded.get_or_init(|| self.decode_rows(codec))
    }

    fn decode_rows(&self, codec: RowCodec<T>) -> Vec<Row<T>> {
        let rows = (codec.decode)(&self.bytes);
        if self.start == 0
            && self.count == rows.len()
            && rows.iter().all(|row| row.len() == self.columns)
        {
            return rows;
        }
        rows.into_iter()
            .skip(self.start as usize)
            .take(self.count)
            .map(|mut row| {
                if row.len() != self.columns {
                    (codec.resize_row)(&mut row, self.columns);
                }
                row
            })
            .collect()
    }

    fn row_mut(&mut self, index: usize, codec: RowCodec<T>) -> &mut Row<T> {
        self.rows(codec);
        self.dirty = true;
        &mut self.decoded.get_mut().expect("initialized row block")[index]
    }

    fn release_cache(&mut self, codec: RowCodec<T>) {
        if let Some(rows) = self.decoded.take() {
            if self.dirty {
                let encoded = (codec.encode)(&rows);
                self.bytes = encoded.bytes;
                self.start = 0;
                self.resize_floor = encoded.resize_floor;
                self.blank_on_reset = encoded.blank_on_reset;
                self.dirty = false;
            }
        }
    }

    fn into_rows(mut self, codec: RowCodec<T>) -> Vec<Row<T>> {
        self.decoded
            .take()
            .unwrap_or_else(|| self.decode_rows(codec))
    }
}

/// Recent editable rows followed by compressed history and an oldest row tail.
///
/// The tail lets bounded-history scrolling reuse one decoded oldest block. A
/// newly initialized blank row also lives there only until the next rotation.
#[derive(Clone, Debug)]
pub struct CompactRows<T> {
    recent: VecDeque<Row<T>>,
    blocks: VecDeque<Block<T>>,
    oldest: VecDeque<Row<T>>,
    codec: RowCodec<T>,
    visible: usize,
    block_rows: usize,
    len: usize,
    reflowing_recent: bool,
    needs_maintenance: bool,
    last_budget: usize,
    /// Index of the oldest block while its rows are recycled one by one.
    evict_cache: Option<Box<EvictCache<T>>>,
    /// Row allocations of the newest sealed block, reused for recycled rows
    /// until the next history bound. Never counted: they are released at
    /// every `bound_history_bytes`, before stored bytes are measured, or,
    /// with `keep_spare`, by `release_spare_rows` once output goes quiet.
    spare: Vec<Row<T>>,
    /// Whether spare rows outlive a history bound. An owner that bounds
    /// history after every small read would otherwise drop them before most
    /// are reused, and allocate a row for nearly every line.
    keep_spare: bool,
    /// The payload term of `history_storage_bytes`, kept current by block
    /// pushes and pops at the ends that streaming output uses, and
    /// recomputed after any other change to `blocks`.
    payload_bytes: Option<usize>,
    /// Whether a block may hold a read cache, so releasing caches while
    /// output streams does not visit every block.
    read_cache: ReadCacheFlag,
    /// History a column change did not reflow, older than every block and
    /// newer than `oldest`.
    deferred: Option<Box<Deferred<T>>>,
    /// Reflow every column change eagerly: the oracle for deferral tests.
    #[cfg(test)]
    pub(crate) eager_reflow: bool,
}

#[derive(Debug, Default)]
struct ReadCacheFlag(AtomicBool);

impl Clone for ReadCacheFlag {
    fn clone(&self) -> Self {
        Self(AtomicBool::new(self.0.load(Ordering::Relaxed)))
    }
}

/// A block's share of the payload term of `history_storage_bytes`.
fn payload_bytes<T>(block: &Block<T>) -> usize {
    block.bytes.len() + 2 * std::mem::size_of::<usize>()
}

fn block_rows<T>(columns: usize) -> usize {
    let bytes = columns
        .saturating_mul(std::mem::size_of::<T>())
        .saturating_add(std::mem::size_of::<Row<T>>())
        .max(1);
    let count = (BLOCK_CELL_BYTES / bytes).clamp(1, MAX_BLOCK_ROWS);
    // Power-of-two blocks can split into smaller ranges without re-encoding.
    1usize << count.ilog2()
}

impl<T> CompactRows<T> {
    pub fn new(rows: Vec<Row<T>>, visible: usize, columns: usize, codec: RowCodec<T>) -> Self {
        assert!(rows.len() >= visible);
        let mut storage = Self {
            len: rows.len(),
            recent: rows.into(),
            blocks: VecDeque::new(),
            oldest: VecDeque::new(),
            codec,
            visible,
            block_rows: block_rows::<T>(columns),
            reflowing_recent: false,
            needs_maintenance: true,
            last_budget: 0,
            evict_cache: None,
            spare: Vec::new(),
            keep_spare: false,
            payload_bytes: Some(0),
            read_cache: ReadCacheFlag::default(),
            deferred: None,
            #[cfg(test)]
            eager_reflow: false,
        };
        storage.seal_recent();
        storage.recent.shrink_to_fit();
        storage
    }

    pub fn len(&self) -> usize {
        self.len
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[inline(always)]
    pub fn row(&self, index: usize) -> &Row<T> {
        // The visible screen and newest history are the parser's hot path.
        if index < self.recent.len() {
            return &self.recent[index];
        }
        self.cold_row(index)
    }

    /// Rows held in blocks. Only the oldest block can be partial: bounded
    /// history drops its rows one at a time without decoding the others.
    fn cold_rows(&self) -> usize {
        self.blocks.back().map_or(0, |last| {
            (self.blocks.len() - 1) * self.block_rows + last.count
        })
    }

    #[inline(never)]
    fn cold_row(&self, mut index: usize) -> &Row<T> {
        assert!(index < self.len);
        index -= self.recent.len();
        let cold_rows = self.cold_rows();
        if index < cold_rows {
            let block = &self.blocks[index / self.block_rows];
            self.read_cache.0.store(true, Ordering::Relaxed);
            return &block.rows(self.codec)[index % self.block_rows];
        }
        index -= cold_rows;
        if let Some(deferred) = &self.deferred {
            if index < deferred.rows {
                self.read_cache.0.store(true, Ordering::Relaxed);
                return &deferred
                    .read
                    .get_or_init(|| deferred.build_rows(self.codec))[index];
            }
            index -= deferred.rows;
        }
        &self.oldest[index]
    }

    /// Rows before `oldest`, from the newest.
    fn rows_before_oldest(&self) -> usize {
        self.recent.len() + self.cold_rows() + self.deferred.as_ref().map_or(0, |d| d.rows)
    }

    #[inline(always)]
    pub fn row_mut(&mut self, index: usize) -> &mut Row<T> {
        // Editing a visible row needs no history maintenance. Rotation can
        // briefly leave fewer recent rows than visible ones.
        if index < self.visible && index < self.recent.len() {
            return &mut self.recent[index];
        }
        self.cold_row_mut(index)
    }

    #[inline(never)]
    fn cold_row_mut(&mut self, mut index: usize) -> &mut Row<T> {
        assert!(index < self.len);
        if index >= self.visible {
            self.needs_maintenance = true;
        }
        if index < self.recent.len() {
            return &mut self.recent[index];
        }
        if self.deferred.is_some()
            && index >= self.recent.len() + self.cold_rows()
            && index < self.rows_before_oldest()
        {
            // Editing deferred history reflows it first.
            self.materialize_deferred();
        }
        index -= self.recent.len();
        let cold_rows = self.cold_rows();
        if index < cold_rows {
            let block = &mut self.blocks[index / self.block_rows];
            *self.read_cache.0.get_mut() = true;
            return block.row_mut(index % self.block_rows, self.codec);
        }
        index -= cold_rows;
        if let Some(deferred) = &self.deferred {
            index -= deferred.rows;
        }
        &mut self.oldest[index]
    }

    pub fn initialize(&mut self, count: usize, columns: usize)
    where
        T: Default,
    {
        self.needs_maintenance |= count != 0;
        // Growing history allocates a row per new line; reuse the cells of
        // the block sealed last instead (see `seal_recent`).
        for _ in 0..count {
            let row = match self.spare.pop() {
                Some(mut row) if row.len() == columns => {
                    (self.codec.clear_row)(&mut row);
                    row
                }
                _ => Row::new(columns),
            };
            self.oldest.push_back(row);
        }
        self.len += count;
    }

    /// Rotate left for positive counts, matching the dense storage's zero shift.
    pub fn rotate(&mut self, count: isize) {
        assert!(count.unsigned_abs() <= self.len);
        self.needs_maintenance |= count != 0;
        if count < 0 {
            for _ in 0..count.unsigned_abs() {
                let row = self.pop_oldest().expect("nonempty rotated storage");
                self.recent.push_front(row);
            }
        } else {
            for _ in 0..count as usize {
                self.ensure_recent();
                let row = self.recent.pop_front().expect("nonempty rotated storage");
                self.oldest.push_back(row);
            }
        }
        self.seal_recent();
    }

    pub fn swap(&mut self, a: usize, b: usize) {
        self.needs_maintenance |= a >= self.visible || b >= self.visible;
        if a == b {
            return;
        }
        if a < self.recent.len() && b < self.recent.len() {
            self.recent.swap(a, b);
        } else {
            // No layout-dependent pointer swaps: ownership moves through safe
            // replacements even for rows decoded from different cold blocks.
            let first = std::mem::replace(self.row_mut(a), Row::from_vec(Vec::new(), 0));
            let second = std::mem::replace(self.row_mut(b), first);
            *self.row_mut(a) = second;
        }
    }

    pub fn truncate(&mut self, len: usize) {
        assert!(len <= self.len);
        self.needs_maintenance |= self.len != len;
        while self.len > len {
            if self.oldest.is_empty() {
                if let Some(deferred) = &mut self.deferred {
                    let rows = deferred.lines.back_rows_at(deferred.columns).expect("line");
                    if self.len - len >= rows {
                        deferred.drop_oldest_line();
                        self.len -= rows;
                        self.deferred_changed();
                        continue;
                    }
                } else if let Some(block) = self.blocks.back() {
                    if self.len - len >= block.count {
                        self.len -= self.pop_block_back().expect("last block").count;
                        continue;
                    }
                }
            }
            self.drop_oldest();
            self.len -= 1;
        }
    }

    pub fn set_visible(&mut self, visible: usize) {
        assert!(visible <= self.len);
        self.needs_maintenance |= self.visible != visible;
        self.visible = visible;
        while self.recent.len() < visible && !self.blocks.is_empty() {
            self.payload_bytes = None;
            let block = self.blocks.pop_front().expect("first block");
            self.recent.extend(block.into_rows(self.codec));
            if self.blocks.is_empty() {
                self.evict_cache = None;
            }
        }
        while self.recent.len() < visible && self.deferred.is_some() {
            let rows = self.take_deferred_line();
            self.recent.extend(rows);
        }
        while self.recent.len() < visible {
            self.recent
                .push_back(self.oldest.pop_front().expect("visible row"));
        }
        self.seal_recent();
    }

    /// Release all history read caches at a caller's exclusive borrow boundary.
    pub fn release_read_cache(&mut self) {
        if !std::mem::take(self.read_cache.0.get_mut()) {
            return;
        }
        for block in &mut self.blocks {
            if block.dirty {
                // Re-encoding replaces the payload.
                self.payload_bytes = None;
            }
            block.release_cache(self.codec);
        }
        if self
            .deferred
            .as_ref()
            .is_some_and(|d| d.read.get().is_some())
        {
            // History that is read is reflowed once, not per read.
            self.materialize_deferred();
        } else if let Some(deferred) = &mut self.deferred {
            for block in &mut deferred.sources {
                block.decoded.take();
            }
        }
    }

    /// Give back what streaming output leaves behind once it stops: decode
    /// caches, the oldest block's eviction index and spare row allocations,
    /// and deque capacity beyond what the rows need. Stored history is
    /// untouched; each cache is rebuilt on its next use.
    pub fn release_idle(&mut self) {
        self.release_read_cache();
        self.evict_cache = None;
        self.release_spare_rows();
        self.recent.shrink_to_fit();
        self.oldest.shrink_to_fit();
        self.blocks.shrink_to_fit();
    }

    #[cfg(test)]
    fn payload_bytes(&self) -> usize {
        self.payload_bytes
            .unwrap_or_else(|| self.stored_payload_bytes())
    }

    /// The payload term of `history_storage_bytes`, computed from scratch.
    fn stored_payload_bytes(&self) -> usize {
        // Split ranges are adjacent and share one immutable allocation. Dirty
        // edits can separate siblings; counting those allocations again is
        // conservative and requires no allocation on the idle/cursor path.
        let mut previous = std::ptr::null();
        let mut payload = 0;
        for block in &self.blocks {
            let ptr = block.bytes.as_ptr();
            if ptr != previous {
                payload += payload_bytes(block);
            }
            previous = ptr;
        }
        payload
    }

    /// Push a newly sealed block, keeping the payload term current.
    fn push_block_front(&mut self, block: Block<T>) {
        let shared = self
            .blocks
            .front()
            .is_some_and(|front| front.bytes.as_ptr() == block.bytes.as_ptr());
        if let Some(payload) = &mut self.payload_bytes {
            if !shared {
                *payload += payload_bytes(&block);
            }
        }
        self.blocks.push_front(block);
    }

    /// Pop the oldest block, keeping the payload term current.
    fn pop_block_back(&mut self) -> Option<Block<T>> {
        let block = self.blocks.pop_back()?;
        let shared = self
            .blocks
            .back()
            .is_some_and(|back| back.bytes.as_ptr() == block.bytes.as_ptr());
        if let Some(payload) = &mut self.payload_bytes {
            if !shared {
                *payload -= payload_bytes(&block);
            }
        }
        Some(block)
    }

    /// Stored bytes, excluding the visible cells and temporary decode work.
    pub fn history_storage_bytes(&self) -> usize {
        let row_bytes = |row: &Row<T>| row.len().saturating_mul(std::mem::size_of::<T>());
        let payload = match self.payload_bytes {
            Some(payload) => {
                debug_assert_eq!(payload, self.stored_payload_bytes());
                payload
            }
            None => self.stored_payload_bytes(),
        };
        // Live bytes, as a function of the history alone, so the rows kept
        // never depend on when the bound runs (after every read, whatever its
        // size): indexes count their lengths, not their capacities (bounds
        // shrink those, see `bound_history_bytes`), and rows not yet sealed
        // into a block count as the full block they grow into between seals.
        // The oldest block's decode index, a cache that exists while full
        // history recycles rows, is not counted.
        let deferred = self.deferred.as_ref().map_or(0, |deferred| {
            std::mem::size_of::<Deferred<T>>()
                + deferred.payload
                + deferred.sources.len() * std::mem::size_of::<Block<T>>()
                + deferred.lines.live_bytes()
                + deferred.eager_cells * std::mem::size_of::<T>()
        });
        let unsealed = self.recent.len().saturating_sub(self.visible);
        let unsealed_bytes = if self.blocks.is_empty() && self.deferred.is_none() {
            // No history to evict yet: count only what exists.
            self.recent.iter().skip(self.visible).map(row_bytes).sum()
        } else if unsealed < self.block_rows {
            self.recent
                .front()
                .map_or(0, |row| self.block_rows * row_bytes(row))
        } else {
            self.recent.iter().skip(self.visible).map(row_bytes).sum()
        };
        let row_structs = self.recent.len().max(self.visible + self.block_rows) + self.oldest.len();
        payload
            + deferred
            + self.blocks.len() * std::mem::size_of::<Block<T>>()
            + row_structs * std::mem::size_of::<Row<T>>()
            + unsealed_bytes
            + self.oldest.iter().map(row_bytes).sum::<usize>()
    }

    /// Keep spare rows across history bounds until `release_spare_rows`.
    pub fn keep_spare_rows(&mut self, keep: bool) {
        self.keep_spare = keep;
        if !keep {
            self.release_spare_rows();
        }
    }

    /// Drop the row allocations kept for recycling (see `spare`), and this
    /// thread's encoding buffers with them.
    pub fn release_spare_rows(&mut self) {
        self.spare = Vec::new();
        release_encode_scratch();
    }

    /// Discard only the oldest history when the retained representation is full.
    #[inline]
    pub fn bound_history_bytes(&mut self, budget: usize) {
        if !self.keep_spare && self.spare.capacity() != 0 {
            self.spare = Vec::new();
            release_encode_scratch();
        }
        if !self.needs_maintenance && self.last_budget == budget {
            return;
        }
        self.release_read_cache();
        if self.payload_bytes.is_none() {
            self.payload_bytes = Some(self.stored_payload_bytes());
        }
        // Release a block index emptied by deferral or eviction before
        // measuring, so its spare capacity does not evict history.
        if self.blocks.capacity() > self.blocks.len().saturating_mul(2).saturating_add(64) {
            self.blocks.shrink_to_fit();
        }
        while self.len > self.visible && self.history_storage_bytes() > budget {
            if self.oldest.is_empty() && self.deferred.is_none() && self.blocks.len() > 1 {
                self.len -= self.pop_block_back().expect("oldest block").count;
                self.evict_cache = None;
            } else {
                self.drop_oldest();
                self.len -= 1;
            }
        }
        // Do not retain large index allocations after erasing history: the
        // budget counts their lengths.
        if self.blocks.capacity() > self.blocks.len().saturating_mul(2).saturating_add(64) {
            self.blocks.shrink_to_fit();
        }
        if let Some(deferred) = &mut self.deferred {
            deferred.shrink_evicted();
        }
        self.needs_maintenance = false;
        self.last_budget = budget;
    }

    pub fn into_rows(mut self) -> Vec<Row<T>> {
        self.materialize_deferred();
        let mut rows = Vec::with_capacity(self.len);
        rows.extend(self.recent);
        for block in self.blocks {
            rows.extend(block.into_rows(self.codec));
        }
        rows.extend(self.oldest);
        rows
    }

    pub fn drain_rows(&mut self) -> Vec<Row<T>> {
        if self.reflowing_recent {
            let rows: Vec<_> = self.recent.drain(..).collect();
            self.len -= rows.len();
            return rows;
        }
        self.materialize_deferred();
        let mut rows = Vec::with_capacity(self.len);
        rows.extend(self.recent.drain(..));
        for block in self.blocks.drain(..) {
            rows.extend(block.into_rows(self.codec));
        }
        rows.extend(self.oldest.drain(..));
        self.payload_bytes = Some(0);
        self.evict_cache = None;
        self.len = 0;
        rows
    }

    pub fn replace_rows(&mut self, rows: Vec<Row<T>>, visible: usize, columns: usize) {
        self.needs_maintenance = true;
        if self.reflowing_recent {
            self.reflowing_recent = false;
            self.len += rows.len();
            self.recent = rows.into();
            self.visible = visible;
            self.seal_recent();
            return;
        }
        #[cfg(test)]
        let eager_reflow = self.eager_reflow;
        *self = Self::new(rows, visible, columns, self.codec);
        #[cfg(test)]
        {
            self.eager_reflow = eager_reflow;
        }
    }

    /// Choose the rows a column change reflows eagerly; returns how many.
    ///
    /// Hard lines that fit the new width keep their compressed payloads;
    /// only requested read rows gain padding. Otherwise, when every older
    /// logical line reflows as a plain re-chunking of its cells (see
    /// [`DeferredLine`]) and none of them lies below `display_offset`,
    /// those lines are deferred: their row count at the new width is known
    /// without decoding them, and their rows are produced when read or
    /// changed. The remaining rows are the ones that eager reflow can change
    /// in other ways (the screen and the lines ending on it). Rows kept back
    /// follow a hard line break and are already at the new width, so the
    /// caller's reflow of the rest matches reflowing every row.
    pub fn prepare_reflow(&mut self, columns: usize, reflow: bool, display_offset: usize) -> usize {
        self.release_read_cache();
        if self.deferred.is_none() && self.keep_hard_lines(columns) {
            return self.recent.len();
        }
        #[cfg(test)]
        let reflow = reflow && !self.eager_reflow;
        if reflow && self.defer_history(columns, display_offset) {
            self.reflowing_recent = true;
            return self.recent.len();
        }
        self.materialize_deferred();
        self.len
    }

    #[cfg(test)]
    pub(crate) fn has_deferred(&self) -> bool {
        self.deferred.is_some()
    }

    /// Rows `prepare_reflow` kept back, while the caller reflows the rest.
    pub fn reflow_suffix_rows(&self) -> usize {
        if self.reflowing_recent { self.len } else { 0 }
    }

    /// Remove at least `count` of the newest rows kept back from a reflow
    /// (fewer only when none remain), newest first, at the new width.
    pub fn take_reflowed_newest(&mut self, count: usize) -> Vec<Row<T>> {
        debug_assert!(self.reflowing_recent);
        let mut rows = Vec::with_capacity(count);
        while rows.len() < count {
            if let Some(block) = self.blocks.pop_front() {
                self.payload_bytes = None;
                if self.blocks.is_empty() {
                    self.evict_cache = None;
                }
                rows.extend(block.into_rows(self.codec));
            } else if self.deferred.is_some() {
                let line = self.take_deferred_line();
                rows.extend(line);
            } else if let Some(row) = self.oldest.pop_front() {
                rows.push(row);
            } else {
                break;
            }
        }
        self.needs_maintenance = true;
        self.len -= rows.len();
        rows
    }

    /// The hard-line case of `prepare_reflow`: cold history needs no reflow.
    fn keep_hard_lines(&mut self, columns: usize) -> bool {
        // Only the oldest block may be partial (bounded history drops its
        // rows one at a time); splitting keeps that true.
        if !self.oldest.is_empty()
            || self.blocks.is_empty()
            || self
                .blocks
                .iter()
                .any(|block| block.resize_floor.is_none_or(|floor| floor > columns))
        {
            return false;
        }
        self.payload_bytes = None;
        self.coalesce_shared_ranges(block_rows::<T>(columns));
        let next_rows = self.block_rows.min(block_rows::<T>(columns));
        if next_rows != self.block_rows {
            let mut blocks =
                VecDeque::with_capacity(self.blocks.len() * self.block_rows / next_rows);
            for block in self.blocks.drain(..) {
                for offset in (0..block.count).step_by(next_rows) {
                    blocks.push_back(Block {
                        bytes: block.bytes.clone(),
                        start: block.start + offset as u32,
                        count: next_rows.min(block.count - offset),
                        columns,
                        resize_floor: block.resize_floor,
                        decoded: OnceLock::new(),
                        dirty: false,
                        // Recycling at another width must decode and resize.
                        blank_on_reset: 0,
                    });
                }
            }
            self.blocks = blocks;
            self.block_rows = next_rows;
        } else {
            for block in &mut self.blocks {
                if block.columns != columns {
                    block.columns = columns;
                    block.blank_on_reset = 0;
                }
            }
        }
        self.reflowing_recent = true;
        true
    }

    /// The deferring case of `prepare_reflow`; `false` leaves every row in
    /// order, possibly with more of them in `recent`.
    fn defer_history(&mut self, columns: usize, display_offset: usize) -> bool {
        let codec = self.codec;
        // A wide character never fits one column; eager reflow handles it.
        if columns < 2 {
            return false;
        }
        // A logical line ending among the eagerly reflowed rows is reflowed
        // with them.
        while let Some(block) = self.blocks.front() {
            match (codec.shape)(&block.rows(codec)[0], &mut Vec::new()) {
                Some(shape) if !shape.wraps => break,
                Some(_) => {}
                None => return false,
            }
            self.payload_bytes = None;
            let block = self.blocks.pop_front().expect("first block");
            self.recent.extend(block.into_rows(codec));
            if self.blocks.is_empty() {
                self.evict_cache = None;
            }
        }
        if display_offset > self.recent.len() {
            return false;
        }
        let oldest_wraps = self
            .oldest
            .front()
            .map(|row| (codec.shape)(row, &mut Vec::new()));
        match oldest_wraps {
            Some(Some(shape)) if !shape.wraps => {}
            Some(_) => return false,
            None => {}
        }

        // Lines of the blocks, which are newer than deferred history, and of
        // the oldest rows, which are older.
        let mut scan = LineScan::new(codec, columns);
        for block in self.blocks.iter_mut().rev() {
            // The newest block was decoded above; others are decoded one at
            // a time and dropped.
            let rows = block
                .decoded
                .take()
                .unwrap_or_else(|| block.decode_rows(codec));
            if rows
                .into_iter()
                .rev()
                .try_for_each(|row| scan.push(row))
                .is_none()
            {
                return false;
            }
        }
        let Some(newer) = scan.finish() else {
            return false;
        };
        let mut scan = LineScan::new(codec, columns);
        let oldest = self.oldest.iter().rev().map(codec.clone_row);
        if oldest
            .into_iter()
            .try_for_each(|row| scan.push(row))
            .is_none()
        {
            return false;
        }
        let Some(older) = scan.finish() else {
            return false;
        };

        let deferred = self.deferred.get_or_insert_with(|| {
            Box::new(Deferred {
                sources: VecDeque::new(),
                lines: Lines::new(),
                columns,
                rows: 0,
                payload: 0,
                eager_cells: 0,
                read: OnceLock::new(),
            })
        });
        // Lines deferred earlier: only eager ones change.
        for extra in &mut deferred.lines.extras {
            if let LineExtra::Eager(rows) = extra {
                *rows = (codec.reflow_line)(std::mem::take(rows), columns);
            }
        }
        deferred.lines.recount_heap();
        deferred.sources.reserve_exact(self.blocks.len());
        for block in self.blocks.drain(..).rev() {
            deferred.sources.push_front(block);
        }
        // The line index counts against the history budget; doubling growth
        // could leave up to half of it unused.
        // `older` is empty without oldest rows.
        deferred
            .lines
            .packed
            .reserve_exact(newer.len() + older.len());
        let extras = newer
            .iter()
            .chain(&older)
            .filter(|line| line.extra.is_some())
            .count();
        deferred.lines.extras.reserve_exact(extras);
        for line in newer {
            deferred.lines.push_front(line);
        }
        if !self.oldest.is_empty() {
            let rows: Vec<_> = self.oldest.drain(..).collect();
            for chunk in rows.chunks(MAX_BLOCK_ROWS) {
                deferred.sources.push_back(Block::new(chunk, codec));
            }
            for line in older.into_iter().rev() {
                deferred.lines.push_back(line);
            }
        }
        deferred.columns = columns;
        deferred.read = OnceLock::new();
        deferred.recount();
        self.payload_bytes = Some(0);
        self.evict_cache = None;
        self.block_rows = block_rows::<T>(columns);
        self.needs_maintenance = true;
        self.len = self.recent.len() + deferred.rows;
        if deferred.lines.is_empty() {
            self.deferred = None;
        } else if deferred.eager_cells * std::mem::size_of::<T>() > MAX_EAGER_BYTES {
            // Too many lines that must be reflowed eagerly: store them all.
            self.materialize_deferred();
        }
        true
    }

    /// Reflow deferred history into blocks now, one block at a time, so a
    /// read of every history row (a checkpoint, a capture) decodes a block
    /// at a time instead of first reflowing all of it into one copy.
    pub fn prepare_history_read(&mut self) {
        if self
            .deferred
            .as_ref()
            .is_some_and(|d| d.read.get().is_none())
        {
            self.materialize_deferred();
        }
    }

    /// Reflow deferred history into blocks after every newer block.
    fn materialize_deferred(&mut self) {
        let Some(deferred) = self.deferred.take() else {
            return;
        };
        let codec = self.codec;
        let columns = deferred.columns;
        let mut deferred = *deferred;
        debug_assert!(
            self.blocks
                .iter()
                .all(|block| block.count == self.block_rows)
        );
        debug_assert_eq!(self.block_rows, block_rows::<T>(columns));
        match deferred.read.take() {
            Some(rows) => {
                for chunk in rows.chunks(self.block_rows) {
                    self.blocks.push_back(Block::new(chunk, codec));
                }
            }
            // Nothing read it yet: encode one block at a time rather than
            // reflowing every row first (tens of MB for full history).
            None => {
                let blocks = &mut self.blocks;
                deferred.for_each_row_chunk(codec, self.block_rows, |rows| {
                    blocks.push_back(Block::new(rows, codec));
                });
            }
        }
        self.payload_bytes = None;
        self.evict_cache = None;
        self.needs_maintenance = true;
    }

    /// Remove the newest deferred line's rows, newest first.
    fn take_deferred_line(&mut self) -> Vec<Row<T>> {
        let codec = self.codec;
        let deferred = self.deferred.as_mut().expect("deferred history");
        let mut rows = deferred.pop_newest_line(codec);
        rows.reverse();
        self.deferred_changed();
        rows
    }

    /// Move the oldest deferred line into `oldest` as rows at the width.
    fn deferred_line_to_oldest(&mut self) {
        debug_assert!(self.oldest.is_empty());
        let codec = self.codec;
        let deferred = self.deferred.as_mut().expect("deferred history");
        let rows = deferred.pop_oldest_line(codec);
        for row in rows {
            self.oldest.push_front(row);
        }
        // The rest of a partly taken source block stays decoded until the
        // next read cache release.
        *self.read_cache.0.get_mut() = true;
        self.deferred_changed();
    }

    /// Deferred history lost its newest or oldest line.
    fn deferred_changed(&mut self) {
        self.needs_maintenance = true;
        if let Some(deferred) = &mut self.deferred {
            deferred.read = OnceLock::new();
            debug_assert_eq!(
                (deferred.rows, deferred.payload, deferred.eager_cells),
                deferred.counted()
            );
            if deferred.lines.is_empty() {
                debug_assert!(deferred.sources.is_empty());
                self.deferred = None;
            }
        }
    }

    /// Undo a wide resize's index splitting without decoding or recompressing
    /// immutable payloads. Groups align from the oldest end; a small unmatched
    /// newest prefix becomes editable. Independently edited ranges remain split.
    fn coalesce_shared_ranges(&mut self, desired: usize) {
        let mut next = desired;
        while next > self.block_rows {
            let group = next / self.block_rows;
            let prefix = self.blocks.len() % group;
            if self.blocks.len() < group {
                next /= 2;
                continue;
            }
            let mergeable = (prefix..self.blocks.len()).step_by(group).all(|start| {
                let first = &self.blocks[start];
                (1..group).all(|offset| {
                    let block = &self.blocks[start + offset];
                    Arc::ptr_eq(&first.bytes, &block.bytes)
                        && block.start as usize == first.start as usize + offset * self.block_rows
                })
            });
            if !mergeable {
                next /= 2;
                continue;
            }
            for _ in 0..prefix {
                self.recent.extend(
                    self.blocks
                        .pop_front()
                        .expect("range prefix")
                        .into_rows(self.codec),
                );
            }
            let mut merged = VecDeque::with_capacity(self.blocks.len() / group);
            while let Some(mut first) = self.blocks.pop_front() {
                for _ in 1..group {
                    let block = self.blocks.pop_front().expect("complete range group");
                    first.count += block.count;
                    first.resize_floor = first
                        .resize_floor
                        .zip(block.resize_floor)
                        .map(|(a, b)| a.max(b));
                }
                merged.push_back(first);
            }
            self.blocks = merged;
            self.block_rows = next;
            return;
        }
    }

    fn pop_oldest(&mut self) -> Option<Row<T>> {
        if let Some(row) = self.oldest.pop_back() {
            return Some(row);
        }
        if self.deferred.is_some() {
            self.deferred_line_to_oldest();
            return self.oldest.pop_back();
        }
        if let Some(block) = self.pop_block_back() {
            self.evict_cache = None;
            self.oldest = block.into_rows(self.codec).into();
            return self.oldest.pop_back();
        }
        self.recent.pop_back()
    }

    /// Discard the oldest row without decoding any other row.
    fn drop_oldest(&mut self) {
        if self.oldest.pop_back().is_some() {
            return;
        }
        if let Some(deferred) = &mut self.deferred {
            deferred.drop_oldest_row();
            self.deferred_changed();
            return;
        }
        if let Some(block) = self.blocks.back_mut() {
            if let Some(rows) = block.decoded.get_mut() {
                rows.pop();
            }
            block.count -= 1;
            if block.count == 0 {
                self.pop_block_back();
                self.evict_cache = None;
            }
            return;
        }
        self.recent.pop_back();
    }

    /// The oldest row after `Row::reset(template)`. A cold row is produced
    /// from its payload directly, so recycling history never decodes a block.
    fn pop_oldest_reset<D>(&mut self, template: &T) -> Row<T>
    where
        T: ResetDiscriminant<D> + GridCell + Default,
        D: PartialEq,
    {
        if self.oldest.is_empty() {
            if let Some(deferred) = &self.deferred {
                if !deferred.lines.back_is_eager() {
                    // Every cell of a greedy line's row after its occupancy
                    // is a default cell, so the reset leaves template cells
                    // only (see `RowCodec::encode`).
                    let columns = deferred.columns;
                    self.drop_oldest();
                    let mut row = self
                        .spare
                        .pop()
                        .filter(|row| row.len() == columns)
                        .unwrap_or_else(|| Row::new(columns));
                    row.reset(template);
                    return row;
                }
            }
        }
        let blank = self.oldest.is_empty()
            && self.deferred.is_none()
            && self.blocks.back().is_some_and(Block::oldest_blank_on_reset);
        if blank {
            let columns = self.blocks.back().expect("oldest block").columns;
            self.drop_oldest();
            // A spare row resets to template cells (see `seal_recent`), and
            // so does a new row of default cells.
            let mut row = self
                .spare
                .pop()
                .filter(|row| row.len() == columns)
                .unwrap_or_else(|| Row::new(columns));
            row.reset(template);
            return row;
        }
        let fast = self.oldest.is_empty()
            && self.deferred.is_none()
            && self
                .blocks
                .back()
                .is_some_and(|block| block.decoded.get().is_none());
        if fast {
            let block = self.blocks.back().expect("oldest block");
            let cached = self
                .evict_cache
                .as_ref()
                .is_some_and(|cache| Arc::ptr_eq(&cache.bytes, &block.bytes));
            if !cached {
                self.evict_cache = Some(Box::new(EvictCache {
                    bytes: block.bytes.clone(),
                    index: (self.codec.index)(&block.bytes),
                }));
            }
            let cache = self.evict_cache.as_ref().expect("oldest block index");
            let row = (self.codec.reset_row)(
                &cache.index,
                block.start as usize + block.count - 1,
                template,
                block.columns,
            );
            if let Some(row) = row {
                self.drop_oldest();
                return row;
            }
        }
        let mut row = self.pop_oldest().expect("nonempty rotated storage");
        row.reset(template);
        row
    }

    /// `rotate(-count)` followed by `Row::reset(template)` of every rotated
    /// row, for callers that clear those rows before reading them.
    pub fn rotate_reset<D>(&mut self, count: usize, template: &T)
    where
        T: ResetDiscriminant<D> + GridCell + Default,
        D: PartialEq,
    {
        assert!(count <= self.len);
        self.needs_maintenance |= count != 0;
        for _ in 0..count {
            let row = self.pop_oldest_reset(template);
            self.recent.push_front(row);
        }
        self.seal_recent();
    }

    fn ensure_recent(&mut self) {
        if !self.recent.is_empty() {
            return;
        }
        if let Some(block) = self.blocks.pop_front() {
            self.payload_bytes = None;
            self.recent.extend(block.into_rows(self.codec));
            if self.blocks.is_empty() {
                self.evict_cache = None;
            }
        } else if self.deferred.is_some() {
            let rows = self.take_deferred_line();
            self.recent.extend(rows);
        } else {
            self.recent.append(&mut self.oldest);
        }
    }

    fn seal_recent(&mut self) {
        while self.recent.len() >= self.visible + self.block_rows {
            let mut rows: Vec<_> = self
                .recent
                .drain(self.recent.len() - self.block_rows..)
                .collect();
            // Only full history recycles rows, and it does so one block at a
            // time; keep one block of allocations for that. The encoder
            // leaves them default cells, so recycling one writes nothing for
            // the default template, and `Row::reset` overwrites every cell
            // for any other.
            let block = if self.spare.is_empty() {
                let block = Block::recycling(&mut rows, self.codec);
                self.spare = rows;
                block
            } else {
                Block::new(&rows, self.codec)
            };
            self.push_block_front(block);
        }
    }
}

/// The logical lines of consecutive rows fed oldest first, reflowed to
/// `columns`.
struct LineScan<T> {
    codec: RowCodec<T>,
    columns: usize,
    lines: Vec<DeferredLine<T>>,
    /// Rows of the line not ended yet.
    open: Vec<Row<T>>,
    open_cells: usize,
    open_plain: bool,
    /// The last open row ends with a leading spacer.
    open_padded: bool,
    wide: Vec<(u32, u32)>,
    row_wide: Vec<(u32, u32)>,
}

impl<T> LineScan<T> {
    fn new(codec: RowCodec<T>, columns: usize) -> Self {
        Self {
            codec,
            columns,
            lines: Vec::new(),
            open: Vec::new(),
            open_cells: 0,
            open_plain: true,
            open_padded: false,
            wide: Vec::new(),
            row_wide: Vec::new(),
        }
    }

    /// `None` when reflow may join the row's line to another line.
    fn push(&mut self, row: Row<T>) -> Option<()> {
        self.row_wide.clear();
        let shape = (self.codec.shape)(&row, &mut self.row_wide)?;
        // A leading spacer stays where a merged row ends with it unless the
        // wide character it made room for follows.
        self.open_plain &= shape.plain && (!self.open_padded || shape.starts_wide);
        self.open_padded = shape.padded;
        if self.open_plain {
            let offset = u32::try_from(self.open_cells).ok()?;
            self.wide.extend(
                self.row_wide
                    .iter()
                    .map(|&(start, count)| (start + offset, count)),
            );
        }
        if shape.wraps {
            self.open_cells += row.len() - usize::from(shape.padded);
            self.open.push(row);
            return Some(());
        }
        let rows = u32::try_from(self.open.len() + 1)
            .ok()
            .filter(|rows| *rows < HAS_EXTRA)?;
        let cells = u32::try_from(self.open_cells + shape.content).ok()?;
        // A continuation row without content, or styled blanks after the
        // content, do not reflow as greedy placement of the content.
        let greedy =
            self.open_plain && shape.default_tail && (self.open.is_empty() || shape.content != 0);
        // Tests check every greedy line against eager reflow of its rows.
        #[cfg(test)]
        if greedy {
            let clone = self.codec.clone_row;
            let sources = || self.open.iter().chain([&row]).map(clone).collect();
            let eager = (self.codec.reflow_line)(sources(), self.columns);
            let mut rows = Vec::new();
            (self.codec.line_rows)(sources(), 0, cells as usize, self.columns, &mut rows);
            assert!(
                (self.codec.rows_equal)(&eager, &rows),
                "greedy placement is not reflow"
            );
        }
        let extra = if greedy {
            self.open.clear();
            (!self.wide.is_empty()).then(|| LineExtra::Greedy {
                skip: 0,
                wide: Some(std::mem::take(&mut self.wide).into_boxed_slice()),
            })
        } else {
            let mut sources = std::mem::take(&mut self.open);
            sources.push(row);
            Some(LineExtra::Eager((self.codec.reflow_line)(
                sources,
                self.columns,
            )))
        };
        self.lines.push(DeferredLine { rows, cells, extra });
        self.open_cells = 0;
        self.open_plain = true;
        self.open_padded = false;
        self.wide.clear();
        Some(())
    }

    /// The lines, oldest first; `None` when the newest row did not end one.
    fn finish(self) -> Option<Vec<DeferredLine<T>>> {
        self.open.is_empty().then_some(self.lines)
    }
}

/// Lossless cell/style encoding for process-local history. This is not a
/// persistent format and must not be used to decode external protocol data.
pub fn cell_codec() -> RowCodec<Cell> {
    RowCodec {
        encode: encode_cells,
        encode_recycled: encode_recycled_cells,
        decode: decode_cells,
        resize_row: resize_cell_row,
        index: index_cells,
        reset_row: reset_cell_row,
        shape: cell_shape,
        line_rows: cell_line_rows,
        reflow_line: cell_reflow_line,
        clone_row: Row::clone,
        clear_row: |row| row.reset(&Cell::default()),
        #[cfg(test)]
        rows_equal: |a, b| a == b,
    }
}

/// See [`RowCodec::shape`]. A plain row has whole wide characters (a
/// character cell followed by its spacer) and a leading spacer only as the
/// last cell of a wrapping row.
fn cell_shape(row: &Row<Cell>, wide: &mut Vec<(u32, u32)>) -> Option<RowShape> {
    let cells = &row[..];
    let len = cells.len();
    let wraps = cells
        .last()
        .is_some_and(|cell| cell.flags.contains(Flags::WRAPLINE));
    let mut content = 0;
    let mut stored = 0;
    let mut padded = false;
    let mut plain = true;
    for (index, cell) in cells.iter().enumerate() {
        if cell.is_default() {
            continue;
        }
        stored = index + 1;
        let flags = cell.flags;
        let last = index + 1 == len;
        if flags.contains(Flags::WRAPLINE) && !last {
            return None;
        }
        if flags.contains(Flags::LEADING_WIDE_CHAR_SPACER) {
            if last && wraps && !flags.intersects(Flags::WIDE_CHAR | Flags::WIDE_CHAR_SPACER) {
                padded = true;
                continue;
            }
            plain = false;
        } else if flags.contains(Flags::WIDE_CHAR) {
            let whole = !flags.contains(Flags::WIDE_CHAR_SPACER)
                && cells.get(index + 1).is_some_and(|next| {
                    next.flags.contains(Flags::WIDE_CHAR_SPACER)
                        && !next
                            .flags
                            .intersects(Flags::WIDE_CHAR | Flags::LEADING_WIDE_CHAR_SPACER)
                });
            plain &= whole;
            match wide.last_mut() {
                Some((start, count)) if *start as usize + 2 * *count as usize == index => {
                    *count += 1;
                }
                _ => wide.push((index as u32, 1)),
            }
        } else if flags.contains(Flags::WIDE_CHAR_SPACER)
            && !(index > 0 && cells[index - 1].flags.contains(Flags::WIDE_CHAR))
        {
            plain = false;
        }
        let empty = if flags.contains(Flags::WRAPLINE) {
            let mut cell = cell.clone();
            cell.flags.remove(Flags::WRAPLINE);
            cell.is_empty()
        } else {
            cell.is_empty()
        };
        if !empty {
            content = index + 1;
        }
    }
    Some(RowShape {
        wraps,
        padded,
        content,
        default_tail: wraps || cells[content..].iter().all(Cell::is_default),
        // A row reset by a coloured template keeps coloured blanks above its
        // occupancy; `Row::reset` treats them unlike greedy rows' padding.
        plain: plain && row.occ >= stored,
        starts_wide: cells
            .first()
            .is_some_and(|cell| cell.flags.contains(Flags::WIDE_CHAR)),
    })
}

/// See [`RowCodec::reflow_line`]. The line's rows become the history of a
/// one-line grid whose blank screen row holds the cursor, so `Grid::resize`
/// applies to them exactly the reflow it applies to any history line.
fn cell_reflow_line(rows: Vec<Row<Cell>>, columns: usize) -> Vec<Row<Cell>> {
    let width = rows[0].len();
    let count = rows.len();
    // No history limit: nothing may be truncated.
    let mut grid: super::Grid<Cell> = super::Grid::new(1, width, usize::MAX / 4);
    let mut raw = Vec::with_capacity(count + 1);
    raw.push(Row::new(width));
    raw.extend(rows.into_iter().rev());
    grid.raw.replace_inner(raw);
    grid.resize(true, 1, columns);
    debug_assert!(grid.cursor.point == crate::index::Point::default());
    let mut rows = grid.raw.take_all();
    let screen = rows.remove(0);
    debug_assert!(screen.is_clear() && screen.len() == columns);
    rows.reverse();
    rows
}

/// See [`RowCodec::line_rows`].
fn cell_line_rows(
    mut sources: Vec<Row<Cell>>,
    skip: usize,
    cells: usize,
    columns: usize,
    out: &mut Vec<Row<Cell>>,
) {
    if sources.len() == 1 && skip == 0 && cells <= columns {
        // A hard line that fits: the row itself at the new width.
        let mut row = sources.pop().expect("one source row");
        resize_cell_row(&mut row, columns);
        out.push(row);
        return;
    }
    let mut content = Vec::with_capacity(cells);
    let last = sources.len() - 1;
    for (index, mut row) in sources.into_iter().enumerate() {
        let len = row.len();
        let mut row = row.front_split_off(len);
        if index != last {
            if row
                .last()
                .is_some_and(|cell| cell.flags.contains(Flags::LEADING_WIDE_CHAR_SPACER))
            {
                row.pop();
            } else if let Some(cell) = row.last_mut() {
                cell.flags.remove(Flags::WRAPLINE);
            }
        }
        content.append(&mut row);
    }
    content.truncate(cells);
    let content = &content[skip..];
    let mut row = Vec::with_capacity(columns);
    let mut index = 0;
    while index < content.len() {
        let width = if content[index].flags.contains(Flags::WIDE_CHAR) {
            2
        } else {
            1
        };
        if row.len() + width > columns {
            if row.len() < columns {
                let mut spacer = Cell::default();
                spacer.flags.insert(Flags::LEADING_WIDE_CHAR_SPACER);
                row.push(spacer);
            }
            let mut full =
                Row::from_vec(std::mem::replace(&mut row, Vec::with_capacity(columns)), 0);
            full.last_mut()
                .expect("nonempty row")
                .flags
                .insert(Flags::WRAPLINE);
            out.push(full);
        }
        row.extend_from_slice(&content[index..index + width]);
        index += width;
    }
    let occ = row.len();
    row.resize(columns, Cell::default());
    out.push(Row::from_vec(row, occ));
}

fn resize_cell_row(row: &mut Row<Cell>, columns: usize) {
    if columns > row.len() {
        row.grow(columns);
    } else {
        assert!(
            row.shrink(columns).is_none(),
            "cold resize must not discard content"
        );
    }
}

// Block layout. Integers are LEB128 varints unless stated otherwise.
//
//   body_len (u32, little-endian), row*, style_count, style*
//                                     (a style is a cell without its character)
//   row: occupancy, columns, stored, text_len, UTF-8 text, (code, cells)*
//
// `stored` excludes the row's suffix of cells equal to `Cell::default()`;
// decoding restores that suffix, so rows keep their exact length and cells.
// The runs cover exactly the `stored` cells. A run's code is its style id
// shifted left once; the low bit marks a paired run, which alternates wide
// characters of that style with the spacers that follow them, so each pair
// takes one character of text and the spacers no style of their own.

/// Minimal FxHash-style hasher. Keys are process-local and never adversarial
/// in a way that matters here: collisions only cost a comparison.
#[derive(Default)]
struct FxHasher(u64);

impl Hasher for FxHasher {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        for chunk in bytes.chunks(8) {
            let mut word = [0u8; 8];
            word[..chunk.len()].copy_from_slice(chunk);
            self.write_u64(u64::from_le_bytes(word));
        }
    }

    #[inline]
    fn write_u8(&mut self, value: u8) {
        self.write_u64(u64::from(value));
    }

    #[inline]
    fn write_u32(&mut self, value: u32) {
        self.write_u64(u64::from(value));
    }

    #[inline]
    fn write_u64(&mut self, value: u64) {
        self.0 = (self.0.rotate_left(5) ^ value).wrapping_mul(0x51_7c_c1_b7_27_22_0a_95);
    }

    #[inline]
    fn write_usize(&mut self, value: usize) {
        self.write_u64(value as u64);
    }

    #[inline]
    fn finish(&self) -> u64 {
        self.0
    }
}

type FxBuild = BuildHasherDefault<FxHasher>;

/// Hash key for a style with no extra storage: both colors and the flags.
#[inline]
fn color_key(value: Color) -> u32 {
    match value {
        Color::Named(value) => value as u32,
        Color::Spec(value) => {
            1 << 24 | u32::from(value.r) << 16 | u32::from(value.g) << 8 | u32::from(value.b)
        }
        Color::Indexed(value) => 2 << 24 | u32::from(value),
    }
}

#[inline]
fn plain_style_key(cell: &Cell) -> u128 {
    u128::from(color_key(cell.fg))
        | u128::from(color_key(cell.bg)) << 32
        | u128::from(cell.flags.bits()) << 64
}

/// Hashes everything but the character: the style a run shares.
fn hash_style<H: Hasher>(cell: &Cell, state: &mut H) {
    plain_style_key(cell).hash(state);
    if let Some(extra) = &cell.extra {
        extra.zerowidth.hash(state);
        extra.underline_color.map(color_key).hash(state);
        extra.hyperlink.hash(state);
    }
}

#[inline]
fn put_varint(out: &mut Vec<u8>, mut value: usize) {
    while value >= 0x80 {
        out.push(value as u8 | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

/// Writes `c` as UTF-8 at `out` and returns how many bytes it took.
///
/// # Safety
///
/// `out` must be valid for writes of four bytes.
#[inline(always)]
unsafe fn write_scalar(out: *mut u8, c: char) -> usize {
    let c = u32::from(c);
    // SAFETY: the caller provides four writable bytes; each branch writes at
    // most that many.
    unsafe {
        if c < 0x80 {
            *out = c as u8;
            1
        } else if c < 0x800 {
            *out = 0xc0 | (c >> 6) as u8;
            *out.add(1) = 0x80 | (c & 0x3f) as u8;
            2
        } else if c < 0x1_0000 {
            *out = 0xe0 | (c >> 12) as u8;
            *out.add(1) = 0x80 | (c >> 6 & 0x3f) as u8;
            *out.add(2) = 0x80 | (c & 0x3f) as u8;
            3
        } else {
            *out = 0xf0 | (c >> 18) as u8;
            *out.add(1) = 0x80 | (c >> 12 & 0x3f) as u8;
            *out.add(2) = 0x80 | (c >> 6 & 0x3f) as u8;
            *out.add(3) = 0x80 | (c & 0x3f) as u8;
            4
        }
    }
}

/// Whether `cell` has no extra storage and the plain style `key` (see
/// [`plain_style_key`]): it joins a plain run of that style.
#[inline(always)]
fn has_plain_style(cell: &Cell, key: u128) -> bool {
    cell.extra.is_none() && plain_style_key(cell) == key
}

/// Appends the text of the leading cells of `cells` that have the plain
/// style of `style` (see [`has_plain_style`]), and returns how many there
/// were.
///
/// Most of a history row is one such run. Named colors, the common case,
/// are compared as such rather than through [`plain_style_key`].
#[inline]
fn extend_plain_run(cells: &[Cell], style: &Cell, text: &mut Vec<u8>) -> usize {
    let flags = style.flags;
    match (style.fg, style.bg) {
        (Color::Named(fg), Color::Named(bg)) => extend_run_with(cells, text, |cell| {
            cell.extra.is_none()
                && cell.flags == flags
                && matches!(cell.fg, Color::Named(color) if color == fg)
                && matches!(cell.bg, Color::Named(color) if color == bg)
        }),
        _ => {
            let key = plain_style_key(style);
            extend_run_with(cells, text, |cell| has_plain_style(cell, key))
        }
    }
}

/// Appends the text of the leading cells of `cells` that `joins`, and
/// returns how many there were. The text goes straight into reserved
/// capacity, so the length stays out of memory between characters.
#[inline(always)]
fn extend_run_with(cells: &[Cell], text: &mut Vec<u8>, joins: impl Fn(&Cell) -> bool) -> usize {
    text.reserve(4 * cells.len());
    let out = text.spare_capacity_mut().as_mut_ptr().cast::<u8>();
    let mut written = 0;
    let mut count = 0;
    for cell in cells {
        if !joins(cell) {
            break;
        }
        // SAFETY: at most `cells.len()` scalars of four bytes are written
        // into the capacity reserved above.
        written += unsafe { write_scalar(out.add(written), cell.c) };
        count += 1;
    }
    // SAFETY: the first `written` spare bytes were just written.
    unsafe { text.set_len(text.len() + written) };
    count
}

/// Appends the text of the leading wide characters of `cells`, each followed
/// by its spacer (see [`is_wide_spacer_of`]), that have the plain style of
/// `style`, and returns how many cells they took (twice the characters).
#[inline]
fn extend_wide_pairs(cells: &[Cell], style: &Cell, text: &mut Vec<u8>) -> usize {
    let flags = style.flags;
    let spacer_flags = wide_spacer_flags(flags);
    match (style.fg, style.bg) {
        (Color::Named(fg), Color::Named(bg)) => {
            let named = |cell: &Cell, flags: Flags| {
                cell.extra.is_none()
                    && cell.flags == flags
                    && matches!(cell.fg, Color::Named(color) if color == fg)
                    && matches!(cell.bg, Color::Named(color) if color == bg)
            };
            extend_pairs_with(cells, text, |wide, spacer| {
                named(wide, flags) && spacer.c == ' ' && named(spacer, spacer_flags)
            })
        }
        _ => {
            let key = plain_style_key(style);
            let spacer_key = plain_style_key(&wide_spacer(style));
            extend_pairs_with(cells, text, |wide, spacer| {
                has_plain_style(wide, key) && spacer.c == ' ' && has_plain_style(spacer, spacer_key)
            })
        }
    }
}

/// Appends the text of the leading pairs of `cells` that `joins`, and
/// returns how many cells they took (twice the characters).
#[inline(always)]
fn extend_pairs_with(
    cells: &[Cell],
    text: &mut Vec<u8>,
    joins: impl Fn(&Cell, &Cell) -> bool,
) -> usize {
    text.reserve(2 * cells.len());
    let out = text.spare_capacity_mut().as_mut_ptr().cast::<u8>();
    let mut written = 0;
    let mut count = 0;
    for pair in cells.chunks_exact(2) {
        if !joins(&pair[0], &pair[1]) {
            break;
        }
        // SAFETY: at most `cells.len() / 2` scalars of four bytes are
        // written into the capacity reserved above.
        written += unsafe { write_scalar(out.add(written), pair[0].c) };
        count += 2;
    }
    // SAFETY: the first `written` spare bytes were just written.
    unsafe { text.set_len(text.len() + written) };
    count
}

/// The flags of the spacer a paired run stores implicitly after each wide
/// character of `style`.
#[inline]
fn wide_spacer_flags(style: Flags) -> Flags {
    style
        .difference(Flags::WIDE_CHAR)
        .union(Flags::WIDE_CHAR_SPACER)
}

/// Whether `spacer` is exactly the cell a paired run rebuilds after `wide`.
#[inline]
fn is_wide_spacer_of(spacer: &Cell, wide: &Cell) -> bool {
    wide.flags.contains(Flags::WIDE_CHAR)
        && spacer.extra.is_none()
        && spacer.c == ' '
        && spacer.flags == wide_spacer_flags(wide.flags)
        && spacer.fg == wide.fg
        && spacer.bg == wide.bg
}

fn put_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    put_varint(out, bytes.len());
    out.extend_from_slice(bytes);
}

fn put_color(out: &mut Vec<u8>, value: Color) {
    match value {
        Color::Named(value) => {
            out.push(0);
            put_varint(out, value as usize);
        }
        Color::Spec(value) => out.extend_from_slice(&[1, value.r, value.g, value.b]),
        Color::Indexed(value) => out.extend_from_slice(&[2, value]),
    }
}

fn put_style(out: &mut Vec<u8>, style: &Cell) {
    put_color(out, style.fg);
    put_color(out, style.bg);
    // A varint: ordinary styles fit one byte; shell marks use high bits.
    put_varint(out, style.flags.bits() as usize);
    let Some(extra) = &style.extra else {
        out.push(0);
        return;
    };
    out.push(1);
    put_varint(out, extra.zerowidth.len());
    for character in &extra.zerowidth {
        put_varint(out, *character as usize);
    }
    match extra.underline_color {
        Some(color) => {
            out.push(1);
            put_color(out, color);
        }
        None => out.push(0),
    }
    match &extra.hyperlink {
        Some(link) => {
            out.push(1);
            put_bytes(out, link.id().as_bytes());
            put_bytes(out, link.uri().as_bytes());
        }
        None => out.push(0),
    }
}

/// Style table for one block. Each run's style is found in a small
/// direct-mapped cache first, so a block that cycles through many styles (a
/// colored log) still finds it in one probe; a miss searches the table.
struct StyleTable {
    styles: Vec<Cell>,
    /// `(key, id + 1)` of recent plain styles; an id of zero is empty.
    recent: [(u128, u32); StyleTable::SLOTS],
    /// Plain style keys by id order, searched on a cache miss while short.
    plain: Vec<(u128, u32)>,
    plain_map: HashMap<u128, u32, FxBuild>,
    /// Styles with extra storage by hash: the first style with each hash.
    /// A block of unique combining marks holds thousands of these, so a
    /// linear search made encoding quadratic in the block's cells.
    extra: HashMap<u64, u32, FxBuild>,
    /// Later styles whose hash equals an earlier, different style's; told
    /// apart by comparing with the stored style.
    extra_collisions: Vec<(u64, u32)>,
    /// The last extra style seen, by its shared storage: cells written with
    /// one template (a hyperlink, an underline color) share it.
    last_extra: Option<(*const CellExtra, u32)>,
}

impl Default for StyleTable {
    fn default() -> Self {
        Self {
            styles: Vec::new(),
            recent: [(0, 0); Self::SLOTS],
            plain: Vec::new(),
            plain_map: HashMap::default(),
            extra: HashMap::default(),
            extra_collisions: Vec::new(),
            last_extra: None,
        }
    }
}

impl StyleTable {
    const SLOTS: usize = 32;
    const LINEAR_PLAIN: usize = 32;

    /// Empties the table for the next block, keeping its capacity.
    fn clear(&mut self) {
        self.styles.clear();
        self.recent = [(0, 0); Self::SLOTS];
        self.plain.clear();
        self.plain_map.clear();
        self.extra.clear();
        self.extra_collisions.clear();
        self.last_extra = None;
    }

    fn push(&mut self, cell: &Cell) -> u32 {
        let mut style = cell.clone();
        style.c = ' ';
        self.styles.push(style);
        (self.styles.len() - 1) as u32
    }

    #[inline]
    fn slot(key: u128) -> usize {
        let folded = (key as u64) ^ ((key >> 64) as u64).rotate_left(29);
        (folded.wrapping_mul(0x9e37_79b9_7f4a_7c15) >> (64 - Self::SLOTS.trailing_zeros())) as usize
    }

    #[inline]
    fn id(&mut self, cell: &Cell) -> u32 {
        if cell.extra.is_some() {
            return self.extra_id(cell);
        }
        let key = plain_style_key(cell);
        let slot = Self::slot(key);
        let (cached, id) = self.recent[slot];
        if cached == key && id != 0 {
            return id - 1;
        }
        self.plain_id(cell, key, slot)
    }

    #[cold]
    fn plain_id(&mut self, cell: &Cell, key: u128, slot: usize) -> u32 {
        let found = if self.plain.len() < Self::LINEAR_PLAIN {
            self.plain
                .iter()
                .find(|entry| entry.0 == key)
                .map(|entry| entry.1)
        } else {
            self.plain_map.get(&key).copied()
        };
        let id = found.unwrap_or_else(|| {
            let id = self.push(cell);
            if self.plain.len() < Self::LINEAR_PLAIN {
                self.plain.push((key, id));
                if self.plain.len() == Self::LINEAR_PLAIN {
                    self.plain_map.extend(self.plain.iter().copied());
                }
            } else {
                self.plain_map.insert(key, id);
            }
            id
        });
        self.recent[slot] = (key, id + 1);
        id
    }

    fn extra_id(&mut self, cell: &Cell) -> u32 {
        let extra = cell.extra.as_ref().expect("a style with extra storage");
        let shared = Arc::as_ptr(extra);
        if let Some((last, id)) = self.last_extra {
            if last == shared && plain_style_key(&self.styles[id as usize]) == plain_style_key(cell)
            {
                return id;
            }
        }
        let mut hasher = FxHasher::default();
        hash_style(cell, &mut hasher);
        let hash = hasher.finish();
        let styles = &self.styles;
        let same = |id: u32| {
            let style = &styles[id as usize];
            plain_style_key(style) == plain_style_key(cell) && style.extra == cell.extra
        };
        // The id a new style gets; one map probe finds or claims the hash.
        let next = styles.len() as u32;
        let found = match self.extra.entry(hash) {
            Entry::Vacant(slot) => {
                slot.insert(next);
                None
            }
            Entry::Occupied(slot) if same(*slot.get()) => Some(*slot.get()),
            Entry::Occupied(_) => {
                let found = self
                    .extra_collisions
                    .iter()
                    .find(|&&(entry, id)| entry == hash && same(id))
                    .map(|&(_, id)| id);
                if found.is_none() {
                    self.extra_collisions.push((hash, next));
                }
                found
            }
        };
        let id = found.unwrap_or_else(|| self.push(cell));
        self.last_extra = Some((shared, id));
        id
    }
}

/// One run being extended: its encoded style (`id << 1 | paired`) and cells.
struct PendingRun {
    code: u32,
    count: usize,
}

/// Encodes up to 64 rows into one compressed payload (see the block layout
/// above).
fn encode_cells(rows: &[Row<Cell>]) -> Encoded {
    let mut encoder = BlockEncoder::new(rows);
    for row in rows {
        encoder.push(row, None);
    }
    encoder.finish()
}

/// [`encode_cells`], then leaves every row default cells with occupancy zero
/// (see [`RowCodec::encode_recycled`]). Each row is cleared right after it
/// is encoded, while its cells are still in cache, and a run of the default
/// style only needs its characters cleared.
fn encode_recycled_cells(rows: &mut [Row<Cell>]) -> Encoded {
    let mut encoder = BlockEncoder::new(rows);
    let mut runs = std::mem::take(&mut encoder.clear_runs);
    for row in rows.iter_mut() {
        runs.clear();
        encoder.push(row, Some(&mut runs));
        let mut start = 0;
        for &(count, default_style) in &runs {
            let cells = &mut row[..][start..start + count];
            if default_style {
                for cell in cells {
                    cell.c = ' ';
                }
            } else {
                cells.fill(Cell::default());
            }
            start += count;
        }
        // Every cell from `start` (the stored cells' end) on is a default
        // cell already.
        row.occ = 0;
    }
    encoder.clear_runs = runs;
    encoder.finish()
}

/// Whether a run of `style` (without extra storage) is made of default
/// cells once its characters are spaces.
#[inline]
fn is_default_style(style: &Cell) -> bool {
    style.flags.is_empty()
        && matches!(style.fg, Color::Named(NamedColor::Foreground))
        && matches!(style.bg, Color::Named(NamedColor::Background))
}

/// One block being encoded, a row at a time.
struct BlockEncoder {
    width: usize,
    index: usize,
    blank_on_reset: u64,
    floor: Option<usize>,
    table: StyleTable,
    raw: Vec<u8>,
    text: Vec<u8>,
    runs: Vec<u8>,
    packed: Vec<u8>,
    /// [`encode_recycled_cells`]'s runs of the row being cleared.
    clear_runs: Vec<(usize, bool)>,
}

impl BlockEncoder {
    fn new(rows: &[Row<Cell>]) -> Self {
        assert!(
            rows.len() <= u64::BITS as usize,
            "history blocks hold at most 64 rows"
        );
        // The working buffers keep their capacity from one block to the next.
        let (mut raw, text, runs, packed, clear_runs) = ENCODE_SCRATCH.take();
        raw.clear();
        raw.reserve(rows.iter().map(|row| row.len() + 8).sum::<usize>() + 64);
        raw.extend_from_slice(&[0; 4]);
        Self {
            width: rows.first().map_or(0, Row::len),
            index: 0,
            blank_on_reset: 0,
            floor: Some(1),
            // Kept empty between blocks with the buffers (see
            // [`STYLE_SCRATCH`]).
            table: STYLE_SCRATCH.take(),
            raw,
            text,
            runs,
            packed,
            clear_runs,
        }
    }

    /// Encodes the next row. With `cleared`, also lists its stored cells as
    /// runs: their length and whether they have the default style.
    fn push(&mut self, row: &Row<Cell>, mut cleared: Option<&mut Vec<(usize, bool)>>) {
        let Self {
            table,
            raw,
            text,
            runs,
            ..
        } = self;
        let cells = &row[..];
        let stored = row.content_len(Cell::is_default);
        if cells.len() == self.width && self.width != 0 && row.occ >= stored {
            self.blank_on_reset |= 1 << self.index;
        }
        self.index += 1;
        let mut floor = self.floor.map(|floor: usize| floor.max(stored));
        put_varint(raw, row.occ);
        put_varint(raw, cells.len());
        put_varint(raw, stored);
        text.clear();
        runs.clear();
        let mut pending: Option<PendingRun> = None;
        let mut start = 0;
        while start < stored {
            let cell = &cells[start];
            // A soft wrap anywhere prevents cold reflow. Cells in one run
            // share flags (a paired run's spacers differ only in the wide
            // flags), so checking each run's first cell suffices.
            if cell.flags.contains(Flags::WRAPLINE) {
                floor = None;
            }
            let id = table.id(cell);
            let (count, paired) = if cell.extra.is_some() {
                text.reserve(4);
                // SAFETY: four bytes were just reserved.
                let written =
                    unsafe { write_scalar(text.spare_capacity_mut().as_mut_ptr().cast(), cell.c) };
                // SAFETY: those bytes were just written.
                unsafe { text.set_len(text.len() + written) };
                (1, false)
            } else if start + 1 < stored && is_wide_spacer_of(&cells[start + 1], cell) {
                // Wide characters alternate with their spacers, which would
                // make every cell its own run.
                (extend_wide_pairs(&cells[start..stored], cell, text), true)
            } else {
                (extend_plain_run(&cells[start..stored], cell, text), false)
            };
            if let Some(cleared) = cleared.as_deref_mut() {
                let default_style = !paired && cell.extra.is_none() && is_default_style(cell);
                match cleared.last_mut() {
                    Some(last) if last.1 == default_style => last.0 += count,
                    _ => cleared.push((count, default_style)),
                }
            }
            let code = id << 1 | u32::from(paired);
            match &mut pending {
                Some(run) if run.code == code => run.count += count,
                _ => {
                    if let Some(run) = pending.replace(PendingRun { code, count }) {
                        put_varint(runs, run.code as usize);
                        put_varint(runs, run.count);
                    }
                }
            }
            start += count;
        }
        if let Some(run) = pending {
            put_varint(runs, run.code as usize);
            put_varint(runs, run.count);
        }
        put_bytes(raw, text);
        raw.extend_from_slice(runs);
        self.floor = floor;
    }

    fn finish(mut self) -> Encoded {
        let raw = &mut self.raw;
        let body = (raw.len() - 4) as u32;
        raw[..4].copy_from_slice(&body.to_le_bytes());
        put_varint(raw, self.table.styles.len());
        for style in &self.table.styles {
            put_style(raw, style);
        }
        // One allocation of the exact size, shared by the block from here on.
        lz::compress_into(raw, &mut self.packed);
        let bytes = Arc::from(&self.packed[..]);
        ENCODE_SCRATCH.set((self.raw, self.text, self.runs, self.packed, self.clear_runs));
        // Emptied now so the scratch table holds no cell's extra storage.
        let mut table = std::mem::take(&mut self.table);
        table.clear();
        STYLE_SCRATCH.set(table);
        Encoded {
            bytes,
            resize_floor: self.floor,
            blank_on_reset: self.blank_on_reset,
        }
    }
}

/// Frees [`ENCODE_SCRATCH`] and [`STYLE_SCRATCH`]: whatever keeps spare rows
/// only while output streams keeps these the same way, so an idle terminal
/// holds neither.
fn release_encode_scratch() {
    drop(ENCODE_SCRATCH.take());
    drop(STYLE_SCRATCH.take());
}

type EncodeScratch = (Vec<u8>, Vec<u8>, Vec<u8>, Vec<u8>, Vec<(usize, bool)>);

thread_local! {
    /// [`encode_cells`]'s payload, row text, row run and compressed payload
    /// buffers, kept from one block to the next while output streams (about
    /// 20 KiB at 160 columns).
    #[allow(clippy::type_complexity)]
    static ENCODE_SCRATCH: std::cell::Cell<EncodeScratch> = const {
        std::cell::Cell::new((Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new()))
    };

    /// [`encode_cells`]'s style table, empty between blocks. A block of
    /// unique combining marks holds a style per cell (about 7,700 at 120
    /// columns), and regrowing its map and list for every block showed in
    /// profiles of such output.
    static STYLE_SCRATCH: std::cell::Cell<StyleTable> = std::cell::Cell::new(StyleTable::default());
}

struct Reader<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> Reader<'a> {
    #[inline]
    fn byte(&mut self) -> u8 {
        let byte = self.bytes[self.position];
        self.position += 1;
        byte
    }

    #[inline]
    fn varint(&mut self) -> usize {
        let mut value = 0usize;
        let mut shift = 0;
        loop {
            let byte = self.byte();
            value |= usize::from(byte & 0x7f) << shift;
            if byte < 0x80 {
                return value;
            }
            shift += 7;
        }
    }

    fn slice(&mut self, len: usize) -> &'a [u8] {
        let slice = &self.bytes[self.position..self.position + len];
        self.position += len;
        slice
    }

    fn string(&mut self) -> String {
        let len = self.varint();
        String::from_utf8(self.slice(len).to_vec()).expect("internally encoded UTF-8")
    }

    fn color(&mut self) -> Color {
        match self.byte() {
            0 => {
                let value = self.varint();
                Color::Named(
                    NAMED_COLORS
                        .iter()
                        .copied()
                        .find(|named| *named as usize == value)
                        .expect("internally encoded named color"),
                )
            }
            1 => {
                let [r, g, b] = self.slice(3) else {
                    unreachable!()
                };
                Color::Spec(Rgb {
                    r: *r,
                    g: *g,
                    b: *b,
                })
            }
            2 => Color::Indexed(self.byte()),
            tag => panic!("internally encoded color tag {tag}"),
        }
    }

    fn style(&mut self) -> Cell {
        let fg = self.color();
        let bg = self.color();
        let flags = Flags::from_bits_retain(self.varint() as u32);
        let extra = (self.byte() == 1).then(|| {
            let zerowidth = (0..self.varint())
                .map(|_| char::from_u32(self.varint() as u32).expect("internally encoded scalar"))
                .collect();
            let underline_color = (self.byte() == 1).then(|| self.color());
            let hyperlink = (self.byte() == 1).then(|| {
                let id = self.string();
                Hyperlink::new(Some(id), self.string())
            });
            Arc::new(CellExtra {
                zerowidth,
                underline_color,
                hyperlink,
            })
        });
        Cell {
            c: ' ',
            fg,
            bg,
            flags,
            extra,
        }
    }
}

const NAMED_COLORS: [NamedColor; 29] = [
    NamedColor::Black,
    NamedColor::Red,
    NamedColor::Green,
    NamedColor::Yellow,
    NamedColor::Blue,
    NamedColor::Magenta,
    NamedColor::Cyan,
    NamedColor::White,
    NamedColor::BrightBlack,
    NamedColor::BrightRed,
    NamedColor::BrightGreen,
    NamedColor::BrightYellow,
    NamedColor::BrightBlue,
    NamedColor::BrightMagenta,
    NamedColor::BrightCyan,
    NamedColor::BrightWhite,
    NamedColor::Foreground,
    NamedColor::Background,
    NamedColor::Cursor,
    NamedColor::DimBlack,
    NamedColor::DimRed,
    NamedColor::DimGreen,
    NamedColor::DimYellow,
    NamedColor::DimBlue,
    NamedColor::DimMagenta,
    NamedColor::DimCyan,
    NamedColor::DimWhite,
    NamedColor::BrightForeground,
    NamedColor::DimForeground,
];

/// The style table of an uncompressed payload and the end of its body; rows
/// start at byte 4 (see [`encode_cells`]).
fn read_styles(raw: &[u8]) -> (Vec<Cell>, usize) {
    let body_end = 4 + u32::from_le_bytes(raw[..4].try_into().expect("payload header")) as usize;
    let mut reader = Reader {
        bytes: raw,
        position: body_end,
    };
    let styles = (0..reader.varint()).map(|_| reader.style()).collect();
    (styles, body_end)
}

/// One encoded row, with its runs and text still encoded.
struct EncodedRow<'a> {
    occ: usize,
    len: usize,
    stored: usize,
    /// Whether any run is paired (see [`encode_cells`]).
    paired: bool,
    runs: Reader<'a>,
    text: &'a [u8],
}

impl<'a> EncodedRow<'a> {
    fn read(reader: &mut Reader<'a>) -> Self {
        let occ = reader.varint();
        let len = reader.varint();
        let stored = reader.varint();
        assert!(occ <= len && stored <= len, "internally encoded row");
        let text_len = reader.varint();
        let text = reader.slice(text_len);
        let runs_start = reader.position;
        let (mut covered, mut paired) = (0, false);
        while covered < stored {
            paired |= reader.varint() & 1 == 1;
            covered += reader.varint();
        }
        let runs = Reader {
            bytes: &reader.bytes[..reader.position],
            position: runs_start,
        };
        Self {
            occ,
            len,
            stored,
            paired,
            runs,
            text,
        }
    }

    /// The next run: its style index, whether it is paired, and its cells.
    #[inline]
    fn next_run(&mut self) -> Option<(usize, bool, usize)> {
        if self.runs.position == self.runs.bytes.len() {
            return None;
        }
        let code = self.runs.varint();
        Some((code >> 1, code & 1 == 1, self.runs.varint()))
    }
}

fn index_cells(bytes: &[u8]) -> EvictIndex<Cell> {
    let raw = lz::decompress(bytes);
    let (styles, body_end) = read_styles(&raw);
    let mut reader = Reader {
        bytes: &raw[..body_end],
        position: 4,
    };
    let mut rows = Vec::new();
    while reader.position < body_end {
        rows.push(reader.position);
        EncodedRow::read(&mut reader);
    }
    EvictIndex { raw, rows, styles }
}

/// `Row::reset(template)` of the encoded row, with `Cell`'s background as the
/// reset discriminant. Cells below the row's occupancy are replaced by the
/// reset, so only the cells above it are decoded.
fn reset_cell_row(
    index: &EvictIndex<Cell>,
    row: usize,
    template: &Cell,
    columns: usize,
) -> Option<Row<Cell>> {
    let mut reader = Reader {
        bytes: &index.raw,
        position: index.rows[row],
    };
    let mut encoded = EncodedRow::read(&mut reader);
    let (occ, len, stored) = (encoded.occ, encoded.len, encoded.stored);
    if len != columns || len == 0 {
        return None;
    }
    let mut last_style = None;
    while let Some((style, _, _)) = encoded.next_run() {
        last_style = Some(style);
    }
    let default = Cell::default();
    let last_bg = match last_style {
        Some(style) if stored == len => index.styles[style].bg,
        _ => default.bg,
    };
    let mut blank = default.clone();
    blank.reset(template);
    // A differing last cell makes the reset clear the whole row.
    let reset = if last_bg == template.bg { occ } else { len };
    let mut cells = Vec::with_capacity(len);
    cells.extend(std::iter::repeat_n(blank, reset));
    if reset < stored {
        // Decode the stored cells the reset keeps.
        let mut encoded = EncodedRow::read(&mut Reader {
            bytes: &index.raw,
            position: index.rows[row],
        });
        let mut characters = std::str::from_utf8(encoded.text)
            .expect("internally encoded text")
            .chars();
        let mut column = 0;
        while let Some((style, paired, count)) = encoded.next_run() {
            let style = &index.styles[style];
            if paired {
                for _ in 0..count / 2 {
                    let c = characters.next().expect("one scalar per wide pair");
                    if column >= reset {
                        cells.push(Cell { c, ..style.clone() });
                    }
                    if column + 1 >= reset {
                        cells.push(wide_spacer(style));
                    }
                    column += 2;
                }
                continue;
            }
            for _ in 0..count {
                let c = characters.next().expect("one scalar per encoded cell");
                if column >= reset {
                    let mut cell = style.clone();
                    cell.c = c;
                    cells.push(cell);
                }
                column += 1;
            }
        }
    }
    cells.extend(std::iter::repeat_n(default, len - cells.len()));
    Some(Row::from_vec(cells, 0))
}

fn decode_cells(bytes: &[u8]) -> Vec<Row<Cell>> {
    let raw = lz::decompress(bytes);
    let (styles, body_end) = read_styles(&raw);
    let mut reader = Reader {
        bytes: &raw[..body_end],
        position: 4,
    };
    let mut rows = Vec::new();
    while reader.position < body_end {
        let mut encoded = EncodedRow::read(&mut reader);
        let (occ, len, stored, text) = (encoded.occ, encoded.len, encoded.stored, encoded.text);
        let mut cells = Vec::with_capacity(len);
        if !encoded.paired && text.len() == stored {
            // ASCII: one byte per cell.
            let mut offset = 0;
            while let Some((style, _, count)) = encoded.next_run() {
                let style = &styles[style];
                let bytes = &text[offset..offset + count];
                offset += count;
                if style.extra.is_none() {
                    let (fg, bg, flags) = (style.fg, style.bg, style.flags);
                    cells.extend(bytes.iter().map(|&byte| Cell {
                        c: char::from(byte),
                        fg,
                        bg,
                        flags,
                        extra: None,
                    }));
                } else {
                    push_run(
                        &mut cells,
                        style,
                        count,
                        bytes.iter().map(|&byte| char::from(byte)),
                    );
                }
            }
        } else {
            let mut text = std::str::from_utf8(text)
                .expect("internally encoded text")
                .chars();
            while let Some((style, paired, count)) = encoded.next_run() {
                let style = &styles[style];
                if paired {
                    push_pairs(&mut cells, style, count, &mut text);
                } else {
                    push_run(&mut cells, style, count, &mut text);
                }
            }
            assert!(text.next().is_none());
        }
        assert_eq!(cells.len(), stored, "internally encoded runs");
        // Cloning one default is measurably cheaper than `resize_with`.
        cells.extend(std::iter::repeat_n(Cell::default(), len - stored));
        rows.push(Row::from_vec(cells, occ));
    }
    rows
}

/// The spacer a paired run rebuilds after each wide character of `style`.
fn wide_spacer(style: &Cell) -> Cell {
    Cell {
        c: ' ',
        fg: style.fg,
        bg: style.bg,
        flags: wide_spacer_flags(style.flags),
        extra: None,
    }
}

/// A paired run: `count / 2` wide characters, each followed by its spacer.
fn push_pairs(cells: &mut Vec<Cell>, style: &Cell, count: usize, text: impl Iterator<Item = char>) {
    assert!(
        count % 2 == 0 && style.extra.is_none(),
        "internally encoded pairs"
    );
    let before = cells.len();
    let spacer = wide_spacer(style);
    for c in text.take(count / 2) {
        cells.push(Cell { c, ..style.clone() });
        cells.push(spacer.clone());
    }
    assert_eq!(cells.len() - before, count, "one scalar per wide pair");
}

fn push_run(cells: &mut Vec<Cell>, style: &Cell, count: usize, text: impl Iterator<Item = char>) {
    let before = cells.len();
    cells.extend(text.take(count).map(|c| {
        let mut cell = style.clone();
        cell.c = c;
        cell
    }));
    assert_eq!(cells.len() - before, count, "one scalar per encoded cell");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Term;
    use crate::event::VoidListener;
    use crate::grid::Dimensions;
    use crate::index::Column;
    use crate::index::Line;
    use crate::term::Config;
    use crate::vte::ansi::Processor;

    struct Size;
    impl Dimensions for Size {
        fn total_lines(&self) -> usize {
            24
        }
        fn screen_lines(&self) -> usize {
            24
        }
        fn columns(&self) -> usize {
            80
        }
    }

    fn parser_rows() -> Vec<Row<Cell>> {
        let mut term = Term::new(
            Config {
                scrolling_history: 10000,
                ..Config::default()
            },
            &Size,
            VoidListener,
        );
        let mut parser: Processor = Processor::new();
        for n in 0..300 {
            let text = format!(
                "\x1b]133;A\x07\x1b[38;2;{};{};{}m\x1b]8;id={n};https://example.invalid/{n}\x07{n:06} 界 e\u{301}\x1b]8;;\x07\x1b[0m\r\n",
                n % 256,
                n * 3 % 256,
                n * 7 % 256
            );
            parser.advance(&mut term, text.as_bytes());
        }
        let grid = term.grid();
        (0..grid.total_lines())
            .map(|i| grid[Line(23 - i as i32)].clone())
            .collect()
    }

    /// Sets the occupancy of a row built cell by cell, making the cells at
    /// and after it equal to the last cell as `Row` guarantees: an
    /// occupancy below the content leaves a uniform tail, as a reset with a
    /// coloured template does.
    fn obey_occupancy(row: &mut Row<Cell>, occ: usize) {
        let len = row.len();
        let last = row[Column(len - 1)].clone();
        for column in occ..len {
            row[Column(column)] = last.clone();
        }
        row.occ = occ;
    }

    fn assert_rows(actual: &CompactRows<Cell>, expected: &[Row<Cell>]) {
        assert_eq!(actual.len(), expected.len());
        for (index, row) in expected.iter().enumerate() {
            // Row equality intentionally omits occupancy; compare it explicitly
            // because reset/reflow behavior depends on that private metadata.
            assert_eq!(actual.row(index), row, "row {index}");
            assert_eq!(actual.row(index).occ, row.occ, "occupancy {index}");
        }
    }

    /// The previous resize-floor definition, kept as the oracle.
    fn reference_resize_floor(rows: &[Row<Cell>]) -> Option<usize> {
        let default = Cell::default();
        let mut floor = 1;
        for row in rows {
            for column in 0..row.len() {
                let cell = &row[Column(column)];
                if cell.flags.contains(Flags::WRAPLINE) {
                    return None;
                }
                if cell != &default {
                    floor = floor.max(column + 1);
                }
            }
        }
        Some(floor)
    }

    fn assert_codec_round_trip(rows: &[Row<Cell>]) {
        let Encoded {
            bytes,
            resize_floor: floor,
            ..
        } = encode_cells(rows);
        assert_eq!(floor, reference_resize_floor(rows));
        let decoded = decode_cells(&bytes);
        assert_eq!(decoded.len(), rows.len());
        for (index, (actual, expected)) in decoded.iter().zip(rows).enumerate() {
            assert_eq!(actual, expected, "row {index}");
            assert_eq!(actual.occ, expected.occ, "occupancy {index}");
            assert_eq!(actual.len(), expected.len(), "width {index}");
        }
        // The recycling encoder writes the same payload and leaves default
        // rows: what `clear_row` makes of them, with nothing left to reset.
        let encoded = encode_cells(rows);
        let mut recycled = rows.to_vec();
        let recycled_encoded = encode_recycled_cells(&mut recycled);
        assert_eq!(recycled_encoded.bytes, encoded.bytes);
        assert_eq!(recycled_encoded.resize_floor, encoded.resize_floor);
        assert_eq!(recycled_encoded.blank_on_reset, encoded.blank_on_reset);
        for (index, row) in recycled.iter().enumerate() {
            assert_eq!(*row, Row::new(row.len()), "recycled row {index}");
            assert_eq!(row.occ, 0, "recycled occupancy {index}");
        }
    }

    #[test]
    fn every_cell_attribute_round_trips_through_the_codec() {
        use crate::vte::ansi::Rgb;
        let colors: Vec<Color> = NAMED_COLORS
            .iter()
            .map(|&named| Color::Named(named))
            .chain((0..=255).step_by(37).map(Color::Indexed))
            .chain([
                Color::Spec(Rgb { r: 0, g: 0, b: 0 }),
                Color::Spec(Rgb {
                    r: 255,
                    g: 128,
                    b: 1,
                }),
            ])
            .collect();
        let characters = ['a', ' ', '\0', '~', 'é', '界', '🦀', '\u{7f}', '\t'];
        let mut seed = 0x9e37_79b9u64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for width in [1, 2, 7, 80, 333] {
            let mut rows = Vec::new();
            for row_index in 0..40 {
                let mut row = Row::<Cell>::new(width);
                let content = next() as usize % (width + 1);
                for column in 0..content {
                    let random = next();
                    let cell = &mut row[Column(column)];
                    cell.c = characters[random as usize % characters.len()];
                    if random % 3 == 0 {
                        cell.fg = colors[(random >> 8) as usize % colors.len()];
                        cell.bg = colors[(random >> 16) as usize % colors.len()];
                        // Includes WRAPLINE for some rows.
                        cell.flags = Flags::from_bits_retain((random >> 24) as u32 & 0x00ff_ffff);
                    }
                    match (random >> 40) % 11 {
                        0 => cell.push_zerowidth('\u{301}'),
                        1 => cell.set_underline_color(Some(
                            colors[(random >> 44) as usize % colors.len()],
                        )),
                        2 => cell.set_hyperlink(Some(Hyperlink::new(
                            Some("id"),
                            format!("https://x/{}", random % 5),
                        ))),
                        3 => cell.extra = Some(Arc::new(CellExtra::default())),
                        4 => {
                            cell.push_zerowidth('\u{308}');
                            cell.push_zerowidth('\u{20e3}');
                            cell.set_hyperlink(Some(Hyperlink::new(
                                None::<String>,
                                String::from("u"),
                            )));
                        }
                        _ => {}
                    }
                }
                // Occupancy below, at and beyond the content.
                let occ = if row_index % 4 == 0 {
                    width
                } else {
                    next() as usize % (width + 1)
                };
                obey_occupancy(&mut row, occ);
                rows.push(row);
            }
            assert_codec_round_trip(&rows);
            // Hard lines only, so the floor is a width, not `None`.
            for row in &mut rows {
                for cell in &mut row[..] {
                    cell.flags.remove(Flags::WRAPLINE);
                }
            }
            assert_codec_round_trip(&rows);
        }
        // A block with far more styles than the linear table holds.
        let mut row = Row::<Cell>::new(600);
        for (column, cell) in row[..].iter_mut().enumerate() {
            cell.c = 'x';
            cell.fg = Color::Spec(Rgb {
                r: column as u8,
                g: (column >> 8) as u8,
                b: 3,
            });
            if column % 5 == 0 {
                cell.push_zerowidth(char::from_u32(0x300 + column as u32 % 64).unwrap());
            }
        }
        assert_codec_round_trip(&[row.clone(), Row::new(600), row]);
        assert_codec_round_trip(&[]);
    }

    #[test]
    fn styles_with_extra_storage_are_told_apart_on_a_hash_collision() {
        let mut first = Cell::default();
        first.push_zerowidth('\u{301}');
        let mut second = Cell::default();
        second.push_zerowidth('\u{302}');
        let hash = |cell: &Cell| {
            let mut hasher = FxHasher::default();
            hash_style(cell, &mut hasher);
            hasher.finish()
        };
        let mut table = StyleTable::default();
        assert_eq!(table.extra_id(&first), 0);
        // Pretend `second` hashes like `first`: its hash now names style 0.
        table.extra.insert(hash(&second), 0);
        table.last_extra = None;
        assert_eq!(table.extra_id(&second), 1);
        table.last_extra = None;
        assert_eq!(table.extra_id(&second), 1);
        assert_eq!(table.extra_collisions, vec![(hash(&second), 1)]);
        assert_eq!(table.styles.len(), 2);
        table.clear();
        assert!(table.styles.is_empty() && table.extra.is_empty());
        assert!(table.extra_collisions.is_empty() && table.last_extra.is_none());
    }

    #[test]
    fn blocks_of_unique_combining_marks_round_trip_one_after_another() {
        // A style per cell, as `kitten __benchmark__`'s unique Unicode
        // writes; the second block reuses the first's emptied style table.
        for offset in [0u32, 7] {
            let rows: Vec<Row<Cell>> = (0..64u32)
                .map(|line| {
                    let mut row = Row::<Cell>::new(120);
                    for (column, cell) in row[..].iter_mut().enumerate() {
                        let mut q = line * 120 + column as u32 + offset;
                        cell.c = 'a';
                        for _ in 0..3 {
                            cell.push_zerowidth(char::from_u32(0x300 + q % 0x70).unwrap());
                            q /= 0x70;
                        }
                    }
                    row
                })
                .collect();
            assert_codec_round_trip(&rows);
        }
    }

    #[test]
    fn recycled_rows_equal_decoded_rows_after_reset() {
        use crate::vte::ansi::{NamedColor, Rgb};
        let backgrounds = [
            Color::Named(NamedColor::Background),
            Color::Named(NamedColor::Red),
            Color::Indexed(4),
            Color::Spec(Rgb { r: 1, g: 2, b: 3 }),
        ];
        let mut seed = 0x1234_5678_9abc_def1u64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for width in [1, 3, 80] {
            let mut rows = Vec::new();
            for _ in 0..64 {
                let mut row = Row::<Cell>::new(width);
                let content = next() as usize % (width + 1);
                for column in 0..content {
                    let random = next();
                    let cell = &mut row[Column(column)];
                    cell.c = ['a', ' ', '界', 'é'][random as usize % 4];
                    cell.bg = backgrounds[(random >> 8) as usize % backgrounds.len()];
                    if random % 7 == 0 {
                        cell.push_zerowidth('\u{301}');
                    }
                }
                // Includes occupancy below, at and above the stored content.
                obey_occupancy(&mut row, next() as usize % (width + 1));
                rows.push(row);
            }
            let encoded = encode_cells(&rows);
            let index = index_cells(&encoded.bytes);
            assert_eq!(index.rows.len(), rows.len());
            let mut blank_rows = 0;
            for (position, row) in rows.iter().enumerate() {
                let blank_on_reset = encoded.blank_on_reset >> position & 1 == 1;
                blank_rows += usize::from(blank_on_reset);
                // A spare row, as `seal_recent` prepares it for recycling.
                let mut spare = row.clone();
                if !blank_on_reset {
                    spare.occ = spare.len();
                }
                for background in backgrounds {
                    let template = Cell {
                        bg: background,
                        ..Cell::default()
                    };
                    let mut expected = row.clone();
                    expected.reset(&template);
                    let actual =
                        reset_cell_row(&index, position, &template, width).expect("matching width");
                    assert_eq!(actual, expected, "row {position} template {background:?}");
                    assert_eq!(actual.occ, expected.occ);
                    assert_eq!(actual.len(), expected.len());
                    // Rows marked blank recycle as template cells without
                    // the payload, and every spare row resets to them.
                    let blank = Row::from_vec(vec![Cell::default(); width], 0);
                    let mut blank = blank;
                    blank.occ = width;
                    blank.reset(&template);
                    if blank_on_reset {
                        assert_eq!(expected, blank, "blank row {position} {background:?}");
                    }
                    let mut recycled = spare.clone();
                    recycled.reset(&template);
                    assert_eq!(recycled, blank, "spare row {position} {background:?}");
                    assert_eq!(recycled.occ, 0);
                }
            }
            assert!(
                blank_rows > 0 && blank_rows < rows.len(),
                "{blank_rows} blank rows"
            );
            assert!(reset_cell_row(&index, 0, &Cell::default(), width + 1).is_none());
        }
    }

    /// Output streaming through full history recycles rows from their
    /// recipe bits and reuses sealed row allocations: no block is
    /// decompressed and the recycled rows are the sealed rows' storage.
    #[test]
    fn wide_character_pairs_round_trip_with_their_spacers() {
        let wide = |c: char, bg: Color| Cell {
            c,
            bg,
            flags: Flags::WIDE_CHAR,
            ..Cell::default()
        };
        let spacer = |bg: Color, flags: Flags| Cell {
            bg,
            flags: Flags::WIDE_CHAR_SPACER | flags,
            ..Cell::default()
        };
        let red = Color::Named(NamedColor::Red);
        let default_bg = Color::Named(NamedColor::Background);
        let row = |cells: Vec<Cell>| {
            let len = cells.len();
            let mut row = Row::from_vec(cells, len);
            row.grow(12);
            row
        };
        let mut joined = wide('界', default_bg);
        joined.push_zerowidth('\u{301}');
        let mut text_spacer = spacer(default_bg, Flags::empty());
        text_spacer.c = 'x';
        let rows = vec![
            // Pairs of one style, then pairs of another, then ASCII.
            row(vec![
                wide('日', default_bg),
                spacer(default_bg, Flags::empty()),
                wide('本', default_bg),
                spacer(default_bg, Flags::empty()),
                wide('語', red),
                spacer(red, Flags::empty()),
                Cell {
                    c: 'a',
                    ..Cell::default()
                },
            ]),
            // A spacer whose style differs from its character's.
            row(vec![wide('日', red), spacer(default_bg, Flags::empty())]),
            // A spacer with flags of its own, one with a character, a lone
            // spacer, and a wide character with marks.
            row(vec![
                wide('日', default_bg),
                spacer(default_bg, Flags::BOLD),
                wide('本', default_bg),
                text_spacer,
                spacer(default_bg, Flags::empty()),
                joined,
                spacer(default_bg, Flags::empty()),
            ]),
            // A wide character in the last column, without its spacer.
            row(vec![Cell::default(); 11]
                .into_iter()
                .chain([wide('日', red)])
                .collect()),
        ];
        assert_codec_round_trip(&rows);
        let encoded = encode_cells(&rows);
        let index = index_cells(&encoded.bytes);
        // Recycling reads the pairs back the same way.
        for (position, row) in rows.iter().enumerate() {
            for background in [default_bg, red] {
                let template = Cell {
                    bg: background,
                    ..Cell::default()
                };
                let mut expected = row.clone();
                expected.reset(&template);
                let actual = reset_cell_row(&index, position, &template, 12).expect("width");
                assert_eq!(actual, expected, "row {position} {background:?}");
            }
        }
    }

    #[test]
    fn spare_rows_survive_history_bounds_until_released() {
        // An owner that bounds history after every read keeps reusing the
        // rows of the block it sealed last, then lets them go when idle.
        let rows = (0..24 + 512).map(|_| Row::new(80)).collect();
        let mut storage = CompactRows::new(rows, 24, 80, cell_codec());
        storage.keep_spare_rows(true);
        let template = Cell::default();
        let mut sealed = Vec::new();
        let mut reused = 0;
        for line in 0..5000 {
            storage.rotate_reset(1, &template);
            let recycled = storage.row(0)[..].as_ptr();
            reused += usize::from(sealed.contains(&recycled));
            for (column, c) in format!("line {line}").chars().enumerate() {
                storage.row_mut(0)[Column(column)].c = c;
            }
            if storage.recent.len() + 1 == storage.visible + storage.block_rows {
                sealed = storage
                    .recent
                    .iter()
                    .rev()
                    .take(storage.block_rows)
                    .map(|row| row[..].as_ptr())
                    .collect();
            }
            // A read of a few lines, then the settle that bounds history.
            if line % 3 == 0 {
                storage.bound_history_bytes(4 << 20);
            }
        }
        assert!(
            reused > 4000,
            "{reused} of 5000 recycled rows reused sealed storage"
        );
        assert!(!storage.spare.is_empty());
        storage.release_spare_rows();
        assert_eq!(storage.spare.capacity(), 0);
        // Without the option every bound drops them, as before.
        storage.keep_spare_rows(false);
        storage.rotate_reset(storage.block_rows + 1, &template);
        storage.bound_history_bytes(4 << 20);
        assert_eq!(storage.spare.capacity(), 0);
    }

    #[test]
    fn streaming_full_history_recycles_without_decoding() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static DECODED: AtomicUsize = AtomicUsize::new(0);
        fn decode(bytes: &[u8]) -> Vec<Row<Cell>> {
            DECODED.fetch_add(1, Ordering::Relaxed);
            decode_cells(bytes)
        }
        fn index(bytes: &[u8]) -> EvictIndex<Cell> {
            DECODED.fetch_add(1, Ordering::Relaxed);
            index_cells(bytes)
        }
        let mut codec = cell_codec();
        codec.decode = decode;
        codec.index = index;
        let rows = (0..24 + 512).map(|_| Row::new(80)).collect();
        let mut storage = CompactRows::new(rows, 24, 80, codec);
        let template = Cell::default();
        let mut sealed = Vec::new();
        let mut reused = 0;
        for line in 0..5000 {
            storage.rotate_reset(1, &template);
            let recycled = storage.row(0)[..].as_ptr();
            reused += usize::from(sealed.contains(&recycled));
            for (column, c) in format!("line {line} of streaming output")
                .chars()
                .enumerate()
            {
                storage.row_mut(0)[Column(column)].c = c;
            }
            // Pointers of the rows the next seal will take.
            if storage.recent.len() + 1 == storage.visible + storage.block_rows {
                sealed = storage
                    .recent
                    .iter()
                    .rev()
                    .take(storage.block_rows)
                    .map(|row| row[..].as_ptr())
                    .collect();
            }
        }
        assert_eq!(
            DECODED.load(Ordering::Relaxed),
            0,
            "recycling decoded history"
        );
        assert!(
            reused > 4000,
            "{reused} of 5000 recycled rows reused sealed storage"
        );
        let expected = storage.clone().into_rows();
        assert_eq!(expected.len(), 24 + 512);
    }

    /// `Block` is counted by the history budget; its recipe bits must not
    /// change how much history a budget retains.
    #[test]
    #[cfg(target_pointer_width = "64")]
    fn recycle_bits_keep_the_counted_block_size() {
        assert_eq!(std::mem::size_of::<Block<Cell>>(), 96);
    }

    /// Full bounded history recycles its oldest rows on every scroll. Dense
    /// storage is the oracle, including colored-background resets, scroll
    /// regions, scrollback reads and edits, erase and alternate screen.
    /// Geometry stays fixed: after resizing a full history, dense and compact
    /// storage already diverge on the base revision, a pre-existing difference
    /// outside this test.
    #[test]
    fn full_history_recycling_matches_dense_storage() {
        let config = Config {
            scrolling_history: 70,
            ..Config::default()
        };
        let size = crate::term::test::TermSize::new(12, 5);
        let mut dense = Term::new(config.clone(), &size, VoidListener);
        let mut compact = Term::new(config, &size, VoidListener);
        compact.grid_mut().enable_compact_history();
        let mut dense_parser: Processor = Processor::new();
        let mut compact_parser: Processor = Processor::new();
        let actions = [
            "\x1b[41m",
            "\x1b[48;5;20m",
            "\x1b[48;2;9;9;9m",
            "\x1b[0m",
            "\x1b[2;4r",
            "\x1b[1;5r",
            "\x1b[r",
            "\x1b[2S",
            "\x1b[7S",
            "\x1b[1T",
            "\x1bD",
            "\x1bM",
            "\x1b[2L",
            "\x1b[1M",
            "\x1b[2J",
            "\x1b[3J",
            "\x1b[K",
            "\x1b[44m\x1b[K\x1b[0m",
            "\x1b[45mcolored tail\x1b[K\r\n\x1b[0m",
            "\x1b[42m\x1b[2K\r\n\r\n",
            "\x1b[?1049h",
            "\x1b[?1049l",
            "\x1b[5;1H",
            "\x1b[H",
            "界界界",
            "e\u{301}",
            "\x1b]8;id=k;https://example.invalid\x07link\x1b]8;;\x07",
        ];
        let mut seed = 0x0bad_5eed_1234_5678u64;
        for step in 0..4000 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let text = match seed % 5 {
                0 | 1 => format!("{step} line {}\r\n", "x".repeat((seed >> 8) as usize % 20)),
                2 => format!("{}\n", step % 7),
                3 => actions[(seed >> 16) as usize % actions.len()].to_string(),
                _ => "\r\n".repeat(1 + (seed >> 20) as usize % 8),
            };
            dense_parser.advance(&mut dense, text.as_bytes());
            compact_parser.advance(&mut compact, text.as_bytes());
            if seed % 13 == 0 {
                // Edit a scrollback row, as selection and reflow can.
                let history = compact.grid().history_size() as i32;
                if history > 0 {
                    let line = Line(-(1 + (seed >> 40) as i32 % history));
                    compact.grid_mut()[line][Column(0)].c = '#';
                    dense.grid_mut()[line][Column(0)].c = '#';
                }
            }
            if seed % 3 == 0 || step % 50 == 0 {
                let (a, e) = (compact.grid(), dense.grid());
                assert_eq!(a.history_size(), e.history_size(), "history at {step}");
                assert_eq!(a.cursor, e.cursor, "cursor at {step}");
                for line in -(e.history_size() as i32)..e.screen_lines() as i32 {
                    assert_eq!(a[Line(line)], e[Line(line)], "line {line} at {step}");
                    assert_eq!(
                        a[Line(line)].occ,
                        e[Line(line)].occ,
                        "occupancy {line} at {step}"
                    );
                }
            }
            compact.grid_mut().release_history_read_cache();
        }
    }

    #[test]
    fn history_compression_round_trips_arbitrary_bytes() {
        let mut seed = 0x51_7c_c1_b7u64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let mut inputs: Vec<Vec<u8>> = vec![
            Vec::new(),
            vec![7],
            vec![0; 3],
            vec![0; 70_000],
            b"abcabcabcabcabcabcabcabcabc".repeat(900),
        ];
        // Long matches that end on a mismatch rather than at the input end.
        for run in [3, 4, 15, 19, 270, 300, 301, 600, 5000] {
            let block: Vec<u8> = (0..run).map(|_| next() as u8).collect();
            let mut input = block.clone();
            input.push(b'X');
            input.extend_from_slice(&block);
            input.push(b'Y');
            input.extend_from_slice(&block[..run / 2]);
            input.extend_from_slice(b"tail bytes");
            inputs.push(input);
        }
        for length in [5, 8, 9, 13, 64, 255, 256, 270, 271, 4096, 70_000] {
            // Random, low-entropy and repeated structure at each length.
            inputs.push((0..length).map(|_| next() as u8).collect());
            inputs.push(
                (0..length)
                    .map(|_| b"ab \x1b"[next() as usize % 4])
                    .collect(),
            );
            let phrase: Vec<u8> = (0..1 + next() as usize % 300)
                .map(|_| next() as u8)
                .collect();
            inputs.push(phrase.iter().copied().cycle().take(length).collect());
        }
        for input in inputs {
            let packed = lz::compress(&input);
            assert_eq!(lz::decompress(&packed), input, "length {}", input.len());
        }
    }

    #[test]
    fn actual_parser_cells_and_occupancy_round_trip_losslessly() {
        let expected = parser_rows();
        let mut actual = CompactRows::new(expected.clone(), 24, 80, cell_codec());
        assert!(!actual.blocks.is_empty());
        assert_rows(&actual, &expected);
        assert!(
            actual
                .blocks
                .iter()
                .all(|block| block.decoded.get().is_some())
        );
        actual.release_read_cache();
        assert!(
            actual
                .blocks
                .iter()
                .all(|block| block.decoded.get().is_none())
        );
        assert_eq!(actual.into_rows(), expected);
    }

    #[test]
    fn random_storage_operations_match_an_uncompressed_sequence() {
        let mut expected = parser_rows();
        let mut actual = CompactRows::new(expected.clone(), 24, 80, cell_codec());
        let mut seed = 0x59fa_8261_u64;
        let templates = [
            Cell::default(),
            Cell {
                bg: Color::Named(NamedColor::Red),
                ..Cell::default()
            },
        ];
        for step in 0..600 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            let n = (seed >> 32) as usize;
            match n % 8 {
                6 => {
                    // Recycling, the full-history scroll path.
                    let count = (1 + n % 40).min(expected.len());
                    let template = &templates[(n >> 8) % templates.len()];
                    actual.rotate_reset(count, template);
                    expected.rotate_right(count);
                    for row in &mut expected[..count] {
                        row.reset(template);
                    }
                }
                7 => {
                    // The stored-byte bound only ever drops the oldest rows;
                    // debug builds also check the incrementally kept payload.
                    let budget = [usize::MAX, 64 * 1024, 16 * 1024][(n >> 8) % 3];
                    actual.bound_history_bytes(budget);
                    assert!(actual.len() >= 24);
                    expected.truncate(actual.len());
                }
                0 => {
                    let count = 1 + n % 7;
                    actual.initialize(count, 80);
                    expected.extend((0..count).map(|_| Row::new(80)));
                }
                1 => {
                    let count = 1 + n % 25;
                    actual.rotate(-(count as isize));
                    expected.rotate_right(count);
                }
                2 => {
                    let count = 1 + n % 25;
                    actual.rotate(count as isize);
                    expected.rotate_left(count);
                }
                3 => {
                    let a = n % expected.len();
                    let b = (n / 7) % expected.len();
                    actual.swap(a, b);
                    expected.swap(a, b);
                }
                4 => {
                    let index = n % expected.len();
                    let column = Column(step % 80);
                    actual.row_mut(index)[column].c = '雪';
                    expected[index][column].c = '雪';
                }
                _ => {
                    let len = expected
                        .len()
                        .saturating_sub(n % 5)
                        .max(30)
                        .min(expected.len());
                    actual.truncate(len);
                    expected.truncate(len);
                }
            }
            actual.set_visible(24);
            assert_rows(&actual, &expected);
            actual.release_read_cache();
            assert_eq!(actual.payload_bytes(), actual.stored_payload_bytes());
        }
        assert_eq!(actual.into_rows(), expected);
    }

    #[test]
    fn frozen_history_can_be_edited_then_recompressed() {
        let mut expected = parser_rows();
        let mut actual = CompactRows::new(expected.clone(), 24, 80, cell_codec());
        let index = actual.recent.len() + 10;
        actual.row_mut(index)[Column(12)].push_zerowidth('\u{308}');
        expected[index][Column(12)].push_zerowidth('\u{308}');
        actual.release_read_cache();
        assert!(
            actual
                .blocks
                .iter()
                .all(|block| block.decoded.get().is_none())
        );
        assert_rows(&actual, &expected);
    }

    #[test]
    fn stored_budget_evicts_oldest_rows_and_preserves_the_visible_grid() {
        let expected = parser_rows();
        let mut actual = CompactRows::new(expected.clone(), 24, 80, cell_codec());
        let original_bytes = actual.history_storage_bytes();
        // An unchanged grid must still obey a smaller subsequent allowance.
        actual.bound_history_bytes(original_bytes);
        let budget = original_bytes / 2;
        actual.bound_history_bytes(budget);
        assert!(actual.history_storage_bytes() <= budget);
        assert!(actual.len() < expected.len());
        assert!(actual.len() >= 24);
        assert_rows(&actual, &expected[..actual.len()]);
        actual.release_read_cache();
        assert!(actual.history_storage_bytes() <= budget);
    }

    #[test]
    fn high_entropy_styles_obey_storage_budget_without_touching_visible_cells() {
        use crate::vte::ansi::Rgb;
        let mut random = 17u32;
        let mut rows: Vec<Row<Cell>> = (0..1024).map(|_| Row::new(80)).collect();
        for row in &mut rows {
            for x in 0..80 {
                random ^= random << 13;
                random ^= random >> 17;
                random ^= random << 5;
                let cell = &mut row[Column(x)];
                cell.c = char::from_u32(33 + random % 90).unwrap();
                cell.fg = Color::Spec(Rgb {
                    r: random as u8,
                    g: (random >> 8) as u8,
                    b: (random >> 16) as u8,
                });
            }
        }
        let expected = rows.clone();
        let mut actual = CompactRows::new(rows, 24, 80, cell_codec());
        let original = actual.history_storage_bytes();
        assert!(original > 256 * 1024);
        actual.bound_history_bytes(256 * 1024);
        assert!(actual.history_storage_bytes() <= 256 * 1024);
        assert!(actual.len() < expected.len());
        assert!(actual.len() >= 24);
        assert_rows(&actual, &expected[..actual.len()]);
        actual.release_read_cache();
        assert!(actual.history_storage_bytes() <= 256 * 1024);
    }

    #[test]
    fn hard_line_resize_keeps_cold_payloads_compressed_and_splits_read_ranges() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static DECODED: AtomicUsize = AtomicUsize::new(0);
        fn decode(bytes: &[u8]) -> Vec<Row<Cell>> {
            DECODED.fetch_add(1, Ordering::Relaxed);
            decode_cells(bytes)
        }
        let original = parser_rows();
        let mut codec = cell_codec();
        codec.decode = decode;
        let mut storage = CompactRows::new(original.clone(), 24, 80, codec);
        let original_ptr = storage.blocks.back().unwrap().bytes.as_ptr();
        let count = storage.prepare_reflow(320, true, 0);
        assert!(count < storage.len());
        let mut recent = storage.drain_rows();
        for row in &mut recent {
            resize_cell_row(row, 320);
        }
        storage.replace_rows(recent, 24, 320);
        assert_eq!(
            DECODED.load(Ordering::Relaxed),
            0,
            "resize decoded cold history"
        );
        assert_eq!(storage.block_rows, 16);
        assert_eq!(storage.blocks.back().unwrap().bytes.as_ptr(), original_ptr);
        for (index, row) in original.iter().cloned().enumerate() {
            let mut expected = row;
            resize_cell_row(&mut expected, 320);
            assert_eq!(storage.row(index), &expected);
        }
        storage.release_read_cache();
        assert!(DECODED.load(Ordering::Relaxed) > 0);
        for columns in [4096, 80] {
            storage.prepare_reflow(columns, true, 0);
            let mut recent = storage.drain_rows();
            for row in &mut recent {
                resize_cell_row(row, columns);
            }
            storage.replace_rows(recent, 24, columns);
        }
        assert_eq!(storage.block_rows, block_rows::<Cell>(80));
        assert_eq!(storage.blocks.back().unwrap().bytes.as_ptr(), original_ptr);
        assert_rows(&storage, &original);
    }

    #[test]
    fn hard_line_parser_resize_matches_dense_across_cold_block_splits() {
        struct Geometry(usize, usize);
        impl Dimensions for Geometry {
            fn total_lines(&self) -> usize {
                self.1
            }
            fn screen_lines(&self) -> usize {
                self.1
            }
            fn columns(&self) -> usize {
                self.0
            }
        }
        let config = Config {
            scrolling_history: 1000,
            ..Config::default()
        };
        let mut dense = Term::new(config.clone(), &Size, VoidListener);
        let mut compact = Term::new(config, &Size, VoidListener);
        compact.grid_mut().enable_compact_history();
        let mut dense_parser: Processor = Processor::new();
        let mut compact_parser: Processor = Processor::new();
        for index in 0..500 {
            let line = format!("\x1b[31m{index:06} 界 e\u{301}\x1b[0m\r\n");
            dense_parser.advance(&mut dense, line.as_bytes());
            compact_parser.advance(&mut compact, line.as_bytes());
        }
        for (columns, lines) in [
            (320, 50),
            (120, 40),
            (139, 49),
            (4096, 24),
            (80, 24),
            (9, 24),
        ] {
            dense.resize(Geometry(columns, lines));
            compact.resize(Geometry(columns, lines));
            assert_eq!(compact.grid().cursor, dense.grid().cursor);
            assert_eq!(compact.grid().history_size(), dense.grid().history_size());
            for line in -(dense.grid().history_size() as i32)..lines as i32 {
                assert_eq!(
                    compact.grid()[Line(line)],
                    dense.grid()[Line(line)],
                    "row {line} at {columns}x{lines}"
                );
                compact.grid_mut().release_history_read_cache();
            }
        }
    }

    #[test]
    fn full_history_rotation_reuses_rows_without_losing_order() {
        let mut expected = parser_rows();
        let mut actual = CompactRows::new(expected.clone(), 24, 80, cell_codec());
        for n in 0..300 {
            actual.rotate(-1);
            expected.rotate_right(1);
            actual.row_mut(0)[Column(0)].c = char::from_u32(0x100 + n).unwrap();
            expected[0][Column(0)].c = char::from_u32(0x100 + n).unwrap();
        }
        assert_rows(&actual, &expected);
        actual.truncate(24);
        expected.truncate(24);
        assert_rows(&actual, &expected);
        assert!(actual.blocks.is_empty());
    }

    #[test]
    fn wide_rows_reduce_the_number_of_rows_per_block() {
        let rows = (0..40).map(|_| Row::new(1000)).collect();
        let actual = CompactRows::new(rows, 24, 1000, cell_codec());
        assert!(actual.block_rows < 6);
        assert!(actual.recent.len() < 30);
    }

    #[test]
    fn compact_parser_matches_dense_history_through_reflow_and_screen_changes() {
        struct Geometry(usize, usize);
        impl Dimensions for Geometry {
            fn total_lines(&self) -> usize {
                self.1
            }
            fn screen_lines(&self) -> usize {
                self.1
            }
            fn columns(&self) -> usize {
                self.0
            }
        }
        fn same(actual: &mut Term<VoidListener>, expected: &Term<VoidListener>, step: &str) {
            assert_eq!(actual.mode(), expected.mode(), "mode after {step}");
            let a = actual.grid();
            let e = expected.grid();
            assert_eq!(
                a.total_lines(),
                e.total_lines(),
                "history length after {step}"
            );
            assert_eq!(a.cursor, e.cursor, "cursor after {step}");
            assert_eq!(a.saved_cursor, e.saved_cursor, "saved cursor after {step}");
            for line in -(e.history_size() as i32)..e.screen_lines() as i32 {
                assert_eq!(a[Line(line)], e[Line(line)], "line {line} after {step}");
            }
            actual.grid_mut().release_history_read_cache();
        }
        let config = Config {
            scrolling_history: 1000,
            ..Config::default()
        };
        let mut dense = Term::new(config.clone(), &Size, VoidListener);
        let mut compact = Term::new(config, &Size, VoidListener);
        compact.grid_mut().enable_compact_history();
        let mut dense_parser: Processor = Processor::new();
        let mut compact_parser: Processor = Processor::new();
        let mut actions = Vec::new();
        for n in 0..400 {
            actions.push(format!(
                "{n:06} 界 e\u{301} {}\r\n",
                "wide wrapped words ".repeat(n % 9)
            ));
        }
        actions.extend(
            [
                "\x1b[4;18r\x1b[17;1H\x1b[3S\x1b[2T\x1b[r",
                "\x1b[3;7H\x1b7\x1b[4L\x1b[2M\x1b8",
                "\x1b[?1049h alternate 界\x1b[?1049l",
                "\x1b[?47h again\x1b[?47l",
                "\x1b[?1047h\x1b[2J\x1b[?1047l",
            ]
            .into_iter()
            .map(str::to_string),
        );
        for (index, action) in actions.iter().enumerate() {
            // Split every escape and UTF-8 sequence across parser feed boundaries.
            for chunk in action.as_bytes().chunks(3) {
                dense_parser.advance(&mut dense, chunk);
                compact_parser.advance(&mut compact, chunk);
            }
            if index % 31 == 0 || index >= 400 {
                same(&mut compact, &dense, &format!("output {index}"));
            }
        }
        for (columns, lines) in [(41, 24), (120, 60), (2, 4), (80, 24), (320, 50), (80, 24)] {
            dense.resize(Geometry(columns, lines));
            compact.resize(Geometry(columns, lines));
            same(&mut compact, &dense, &format!("resize {columns}x{lines}"));
        }
        for action in ["\x1b[3J", "new history\r\n".repeat(150).as_str(), "\x1bc"] {
            dense_parser.advance(&mut dense, action.as_bytes());
            compact_parser.advance(&mut compact, action.as_bytes());
            same(&mut compact, &dense, "erase or reset");
        }
    }

    struct Geometry(usize, usize);
    impl Dimensions for Geometry {
        fn total_lines(&self) -> usize {
            self.1
        }
        fn screen_lines(&self) -> usize {
            self.1
        }
        fn columns(&self) -> usize {
            self.0
        }
    }

    /// Everything a reader can observe: history length, cursors and every
    /// row, compared with dense storage.
    /// Retained history is a function of the output alone: bounding it after
    /// every byte or after every 64 KiB read keeps the same rows. Terminals
    /// bound history once per read, and read sizes vary from run to run.
    #[test]
    fn bounded_history_does_not_depend_on_read_boundaries() {
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        // Lines of varied length and compressibility, some coloured, some
        // wrapping, so blocks differ in size.
        let mut text = Vec::new();
        let mut line = 0;
        while text.len() < 3 << 20 {
            let len = (next() % 150) as usize;
            if next() % 4 == 0 {
                text.extend_from_slice(format!("\x1b[3{}m", next() % 8).as_bytes());
            }
            for _ in 0..len {
                let byte = if next() % 3 == 0 {
                    b'a' + (line % 26) as u8
                } else {
                    b' ' + (next() % 95) as u8
                };
                text.push(byte);
            }
            text.extend_from_slice(b"\x1b[0m\r\n");
            line += 1;
        }
        let budget = 192 * 1024;
        let config = Config {
            scrolling_history: 150_000,
            ..Config::default()
        };
        let parse = |read: usize| {
            let mut term = Term::new(config.clone(), &Size, VoidListener);
            term.grid_mut().enable_compact_history();
            let mut parser: Processor = Processor::new();
            for chunk in text.chunks(read) {
                parser.advance(&mut term, chunk);
                term.bound_primary_history_storage(budget);
            }
            term
        };
        let mut whole = parse(64 << 10);
        let bytes = parse(1);
        assert!(bytes.grid().history_size() > 1000);
        same_as_dense(&mut whole, &bytes, "64 KiB reads vs single bytes");
        // Nor on reads that happen to align with neither.
        let mut odd = parse(4093);
        same_as_dense(&mut odd, &bytes, "4093-byte reads vs single bytes");
    }

    fn same_as_dense(actual: &mut Term<VoidListener>, expected: &Term<VoidListener>, step: &str) {
        let (a, e) = (actual.grid(), expected.grid());
        assert_eq!(a.history_size(), e.history_size(), "history after {step}");
        assert_eq!(
            a.display_offset(),
            e.display_offset(),
            "offset after {step}"
        );
        assert_eq!(a.cursor, e.cursor, "cursor after {step}");
        assert_eq!(a.saved_cursor, e.saved_cursor, "saved cursor after {step}");
        for line in -(e.history_size() as i32)..e.screen_lines() as i32 {
            assert_eq!(a[Line(line)], e[Line(line)], "line {line} after {step}");
        }
        actual.grid_mut().release_history_read_cache();
    }

    fn visible_same_as_dense(
        actual: &Term<VoidListener>,
        expected: &Term<VoidListener>,
        step: &str,
    ) {
        let (a, e) = (actual.grid(), expected.grid());
        assert_eq!(a.history_size(), e.history_size(), "history after {step}");
        assert_eq!(a.cursor, e.cursor, "cursor after {step}");
        for line in 0..e.screen_lines() as i32 {
            assert_eq!(a[Line(line)], e[Line(line)], "line {line} after {step}");
        }
    }

    #[test]
    fn grow_pulls_kept_history_into_the_viewport_like_dense_storage() {
        // Hard-line history keeps its payloads through a column change. When
        // growing merges more wrapped screen rows than the newest editable
        // history holds, history rows must come down into the viewport, as
        // in dense storage, not blank rows appear at the bottom.
        for extra in 0..70 {
            let config = Config {
                scrolling_history: 1000,
                ..Config::default()
            };
            let mut dense = Term::new(config.clone(), &Size, VoidListener);
            let mut compact = Term::new(config, &Size, VoidListener);
            compact.grid_mut().enable_compact_history();
            let mut text = String::new();
            for i in 0..(200 + extra) {
                text.push_str(&format!("short {i}\r\n"));
            }
            for i in 0..10 {
                text.push_str(&format!("{i} {}\r\n", "w".repeat(150)));
            }
            let mut dense_parser: Processor = Processor::new();
            let mut compact_parser: Processor = Processor::new();
            dense_parser.advance(&mut dense, text.as_bytes());
            compact_parser.advance(&mut compact, text.as_bytes());
            dense.resize(Geometry(160, 24));
            compact.resize(Geometry(160, 24));
            same_as_dense(
                &mut compact,
                &dense,
                &format!("grow with {extra} extra rows"),
            );
        }
    }

    /// Output that exercises every reflow rule: soft-wrapped lines of any
    /// length, exact-width lines, trailing styled blanks, background erases,
    /// tabs, wide and zero-width characters, empty lines, rewrites of wrapped
    /// lines, scroll regions, reverse index and the alternate screen.
    fn reflow_output(random: u64, n: usize, coloured_scroll: bool) -> String {
        let words = |count: usize| "word ".repeat(count);
        match random % 28 {
            0..=5 => format!(
                "\x1b[3{}m{n:05} {}\x1b[0m\r\n",
                random % 8,
                words((random >> 8) as usize % 40)
            ),
            6 => format!("{n:05} {}\r\n", "x".repeat((random >> 8) as usize % 400)),
            7 => format!("\x1b[1m{n:05} bold blanks    \x1b[0m\r\n"),
            8 => format!("\x1b[44m{n:05} blue\x1b[K\x1b[0m\r\n"),
            9 => format!("{n:05}\tt\tab\r\n"),
            10 => "\r\n\r\n".to_string(),
            11 => format!("{n:05} 界面 {} 🦀\r\n", words((random >> 8) as usize % 30)),
            12 => format!("{n:05} e\u{301} {}\r\n", words((random >> 8) as usize % 30)),
            13 => format!(
                "{n:05} {}\x1b[4m   \x1b[0m\r\n",
                words((random >> 8) as usize % 30)
            ),
            // Rewrite part of a wrapped line above the cursor.
            14 => "\x1b[2A\rrewritten\x1b[2B\r".to_string(),
            15 => "\x1b[3;9r\x1b[5;1H\x1b[2S\x1b[1T\x1b[r\x1b[99;1H".to_string(),
            16 => "\x1b[H\x1bM\x1bM\x1b[99;1H".to_string(),
            17 => format!("\x1b[?1049h{n:05} alternate {}\x1b[?1049l", words(20)),
            18 => format!("{n:05} {}", words((random >> 8) as usize % 50)),
            19 => "\x1b[5;3H\x1b[2L\x1b[1M\x1b[99;1H".to_string(),
            20 => format!("\x1b[{}G{n:05} placed\r\n", 1 + (random >> 8) % 100),
            21 if (random >> 8) % 32 == 0 => "\x1b[3J".to_string(),
            22 => format!("{n:05}{}\r\n", "界面🦀".repeat((random >> 8) as usize % 60)),
            // Overwrite cells that may hold half of a wide character.
            23 => format!("\x1b[1A\x1b[{}Gx\x1b[1B\r", 1 + (random >> 8) % 40),
            24 => format!("a{}\r\n", "界".repeat((random >> 8) as usize % 90)),
            // Shift cells, and so a soft wrap, within a row above.
            25 if (random >> 8) % 2 == 0 => "\x1b[1A\x1b[3P\x1b[1B\r".to_string(),
            25 => "\x1b[1A\x1b[4@\x1b[1B\r".to_string(),
            // Scroll with a coloured template, so recycled rows reset to it.
            26 if coloured_scroll => format!("\x1b[42m{n:05} green\r\n\r\n\x1b[0m"),
            _ => format!("{n:05}{}\r\n", "=".repeat((random >> 8) as usize % 160)),
        }
    }

    #[test]
    fn deferred_reflow_matches_dense_through_random_resizes() {
        random_resizes(false);
    }

    /// Dense storage recycles rows beyond its history length, whose cells a
    /// coloured-template reset may keep; eager compact reflow is the oracle
    /// for output that scrolls with a coloured template.
    #[test]
    fn deferred_reflow_matches_eager_compact_reflow_through_random_resizes() {
        random_resizes(true);
    }

    fn random_resizes(oracle_is_compact: bool) {
        // Resizes that deferred history, by seed parity.
        let mut deferred_resizes = [0; 2];
        // DIRI_REFLOW_SEEDS=<n> fuzzes more sequences.
        let seeds: u64 = std::env::var("DIRI_REFLOW_SEEDS").map_or(12, |n| n.parse().unwrap());
        for seed in 1..=seeds {
            let mut state = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
            let mut next = move || {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state
            };
            let config = Config {
                scrolling_history: 300 + seed as usize * 40,
                ..Config::default()
            };
            let mut dense = Term::new(config.clone(), &Size, VoidListener);
            if oracle_is_compact {
                dense.grid_mut().enable_compact_history();
                dense.grid_mut().raw.compact_mut().unwrap().eager_reflow = true;
            }
            let mut compact = Term::new(config, &Size, VoidListener);
            compact.grid_mut().enable_compact_history();
            let mut dense_parser: Processor = Processor::new();
            let mut compact_parser: Processor = Processor::new();
            let (mut columns, mut lines): (usize, usize) = (80, 24);
            let mut output = 0;
            for step in 0..700 {
                let random = next();
                if random % 3 == 0 {
                    // Drags move one column at a time; jumps go anywhere.
                    columns = match (random >> 4) % 4 {
                        0 => columns.saturating_sub(1).max(2),
                        1 => columns + 1,
                        2 => [2, 3, 7, 13, 40, 79, 80, 81, 120, 160, 200]
                            [(random >> 8) as usize % 11],
                        _ => columns,
                    };
                    if (random >> 12) % 5 == 0 {
                        lines = [1, 3, 10, 24, 50][(random >> 16) as usize % 5];
                    }
                    dense.resize(Geometry(columns, lines));
                    compact.resize(Geometry(columns, lines));
                    if compact
                        .grid()
                        .raw
                        .compact()
                        .is_some_and(CompactRows::has_deferred)
                    {
                        deferred_resizes[seed as usize % 2] += 1;
                    }
                    let label = format!("seed {seed} step {step}: resize {columns}x{lines}");
                    visible_same_as_dense(&compact, &dense, &label);
                    if (random >> 20) % 7 == 0 {
                        same_as_dense(&mut compact, &dense, &label);
                    }
                } else {
                    for _ in 0..1 + (random >> 4) % 12 {
                        let mut kind = next();
                        // Odd seeds print only lines that reflow as greedy
                        // placement (no styled trailing blanks, tabs, broken
                        // wide characters or shifted soft wraps).
                        while seed % 2 == 1 && matches!(kind % 28, 7 | 9 | 23 | 25 | 26) {
                            kind = next();
                        }
                        let text = reflow_output(kind, output, oracle_is_compact);
                        output += 1;
                        dense_parser.advance(&mut dense, text.as_bytes());
                        compact_parser.advance(&mut compact, text.as_bytes());
                    }
                    let label = format!("seed {seed} step {step}: output");
                    visible_same_as_dense(&compact, &dense, &label);
                    match (random >> 24) % 9 {
                        // A few history reads, as a scrolled viewport does.
                        0 => {
                            let history = dense.grid().history_size() as i32;
                            for _ in 0..3 {
                                let line = -((next() % (history as u64 + 1)) as i32);
                                if line < 0 {
                                    assert_eq!(
                                        compact.grid()[Line(line)],
                                        dense.grid()[Line(line)],
                                        "read {line} at {label}"
                                    );
                                }
                            }
                            compact.grid_mut().release_history_read_cache();
                        }
                        1 => compact.bound_primary_history_storage(usize::MAX / 2),
                        2 => same_as_dense(&mut compact, &dense, &label),
                        // A full in-order read (a checkpoint): deferred
                        // history is stored a block at a time first.
                        3 => {
                            compact.grid_mut().prepare_history_read();
                            same_as_dense(&mut compact, &dense, &label);
                        }
                        _ => {}
                    }
                }
            }
            same_as_dense(&mut compact, &dense, &format!("seed {seed} end"));
        }
        let [mixed, greedy] = deferred_resizes;
        assert!(
            greedy > 40 * seeds as usize,
            "{greedy} greedy resizes deferred history"
        );
        assert!(
            mixed > 30 * seeds as usize,
            "{mixed} mixed resizes deferred history"
        );
    }

    #[test]
    fn greedy_lines_stay_greedy_through_any_widths() {
        // Arbitrary cells, not only what a parser writes: every line that
        // scans as greedy must reflow, one width after another, to exactly
        // its greedy placement at each width (the scan itself checks the
        // first width against eager reflow).
        let mut state = 0x2545_f491_4f6c_dd1du64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let mut greedy_lines = 0;
        for _ in 0..20_000 {
            let width = 2 + (next() % 12) as usize;
            let count = 1 + (next() % 4) as usize;
            let mut rows = Vec::new();
            for index in 0..count {
                let mut cells = Vec::new();
                while cells.len() < width {
                    let mut cell = Cell::default();
                    match next() % 12 {
                        0..=2 => {}
                        3 | 4 => cell.c = 'a',
                        5 => {
                            cell.c = ' ';
                            cell.flags = Flags::BOLD;
                        }
                        6 => {
                            cell.c = ' ';
                            cell.bg = Color::Named(NamedColor::Blue);
                        }
                        7 => cell.c = '\t',
                        8 | 9 if cells.len() + 1 < width => {
                            cell.c = '界';
                            cell.flags = Flags::WIDE_CHAR;
                            cells.push(cell);
                            cells.push(Cell {
                                flags: Flags::WIDE_CHAR_SPACER,
                                ..Cell::default()
                            });
                            continue;
                        }
                        10 => cell.flags = Flags::WIDE_CHAR_SPACER,
                        _ => cell.c = 'b',
                    }
                    cells.push(cell);
                }
                if index + 1 != count {
                    if next() % 3 == 0 && !cells[width - 2].flags.contains(Flags::WIDE_CHAR) {
                        cells[width - 1] = Cell {
                            flags: Flags::LEADING_WIDE_CHAR_SPACER,
                            ..Cell::default()
                        };
                    }
                    cells[width - 1].flags.insert(Flags::WRAPLINE);
                }
                rows.push(Row::from_vec(cells, width));
            }
            let codec = cell_codec();
            let first = 2 + (next() % 15) as usize;
            let mut scan = LineScan::new(codec, first);
            for row in rows.iter().cloned() {
                scan.push(row).expect("soft wraps end rows");
            }
            let line = scan.finish().expect("one line").pop().expect("one line");
            let mut line = line;
            if matches!(line.extra, Some(LineExtra::Eager(_))) {
                continue;
            }
            greedy_lines += 1;
            let mut eager = cell_reflow_line(rows.clone(), first);
            let mut columns = first;
            for _ in 0..6 {
                if next() % 3 == 0 && eager.len() > 1 {
                    // History drops the line's oldest row.
                    eager.remove(0);
                    let first = line.layout(columns).1;
                    match &mut line.extra {
                        Some(LineExtra::Greedy { skip, .. }) => *skip += first as u32,
                        extra => {
                            *extra = Some(LineExtra::Greedy {
                                skip: first as u32,
                                wide: None,
                            })
                        }
                    }
                }
                columns = 2 + (next() % 15) as usize;
                eager = cell_reflow_line(eager, columns);
                let skip = match &line.extra {
                    Some(LineExtra::Greedy { skip, .. }) => *skip as usize,
                    _ => 0,
                };
                let mut greedy = Vec::new();
                cell_line_rows(
                    rows.clone(),
                    skip,
                    line.cells as usize,
                    columns,
                    &mut greedy,
                );
                assert_eq!(greedy, eager, "{rows:?} at {columns}");
                assert_eq!(line.rows_at(columns), eager.len(), "{rows:?} at {columns}");
            }
        }
        assert!(greedy_lines > 2_000, "only {greedy_lines} greedy lines");
    }

    #[test]
    fn column_changes_after_the_first_decode_only_new_history() {
        thread_local! {
            static DECODED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
            static ENCODED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
        }
        fn decode(bytes: &[u8]) -> Vec<Row<Cell>> {
            DECODED.with(|count| count.set(count.get() + 1));
            decode_cells(bytes)
        }
        fn encode(rows: &[Row<Cell>]) -> Encoded {
            ENCODED.with(|count| count.set(count.get() + 1));
            encode_cells(rows)
        }
        let taken = || {
            (
                DECODED.with(|count| count.replace(0)),
                ENCODED.with(|count| count.replace(0)),
            )
        };
        let config = Config {
            scrolling_history: 2000,
            ..Config::default()
        };
        let mut dense = Term::new(config.clone(), &Size, VoidListener);
        let mut compact = Term::new(config, &Size, VoidListener);
        fn encode_recycled(rows: &mut [Row<Cell>]) -> Encoded {
            ENCODED.with(|count| count.set(count.get() + 1));
            encode_recycled_cells(rows)
        }
        let mut codec = cell_codec();
        codec.decode = decode;
        codec.encode = encode;
        codec.encode_recycled = encode_recycled;
        compact.grid_mut().raw.enable_compact(codec);
        let mut text = String::new();
        for n in 0..1500 {
            // Every third line wraps below 120 columns; some hold wide
            // characters.
            let tail = if n % 3 == 0 {
                " and a tail that wraps".repeat(4)
            } else {
                String::new()
            };
            text.push_str(&format!(
                "\x1b[3{}m{n:05} 界 building{tail}\x1b[0m\r\n",
                n % 8
            ));
        }
        let mut dense_parser: Processor = Processor::new();
        let mut compact_parser: Processor = Processor::new();
        dense_parser.advance(&mut dense, text.as_bytes());
        compact_parser.advance(&mut compact, text.as_bytes());
        let blocks = compact.grid().raw.compact().unwrap().blocks.len();
        assert!(blocks > 20, "{blocks} history blocks");
        taken();
        for (step, columns) in (60..80).rev().chain(61..=100).enumerate() {
            dense.resize(Geometry(columns, 24));
            compact.resize(Geometry(columns, 24));
            let (decoded, encoded) = taken();
            if step != 0 {
                // Only blocks sealed since the last change are read, and
                // only the rows reflowed eagerly are sealed again.
                assert!(
                    decoded <= 2,
                    "{decoded} blocks decoded at {columns} columns"
                );
                assert!(
                    encoded <= 4,
                    "{encoded} blocks encoded at {columns} columns"
                );
            }
        }
        same_as_dense(&mut compact, &dense, "the drag");
    }
}
