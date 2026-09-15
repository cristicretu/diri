//! Lossless cold row blocks with explicitly owned decode caches.
//!
//! Rows use the grid storage order: newest first. References returned by `row`
//! remain valid until exclusive access is regained. Cache reclamation therefore
//! requires `&mut self`; it never evicts behind an outstanding shared reference.

use std::collections::{HashMap, VecDeque};
use std::hash::{Hash, Hasher};
use std::io::{Read, Write};
use std::sync::OnceLock;

use flate2::Compression;
use flate2::read::DeflateDecoder;
use flate2::write::DeflateEncoder;
use serde::{Deserialize, Serialize};

use super::Row;
use crate::index::Column;
use crate::term::cell::Cell;
use crate::vte::ansi::Color;

const MAX_BLOCK_ROWS: usize = 64;
const BLOCK_CELL_BYTES: usize = 128 * 1024;

/// The codec is a typed storage operation, never a second terminal parser.
#[derive(Debug)]
pub struct RowCodec<T> {
    encode: fn(&[Row<T>]) -> Box<[u8]>,
    decode: fn(&[u8]) -> Vec<Row<T>>,
}

impl<T> Copy for RowCodec<T> {}
impl<T> Clone for RowCodec<T> {
    fn clone(&self) -> Self {
        *self
    }
}

#[derive(Clone, Debug)]
struct Block<T> {
    bytes: Box<[u8]>,
    count: usize,
    decoded: OnceLock<Vec<Row<T>>>,
    dirty: bool,
}

impl<T> Block<T> {
    fn new(rows: Vec<Row<T>>, codec: RowCodec<T>) -> Self {
        Self {
            bytes: (codec.encode)(&rows),
            count: rows.len(),
            decoded: OnceLock::new(),
            dirty: false,
        }
    }

    fn rows(&self, codec: RowCodec<T>) -> &[Row<T>] {
        self.decoded.get_or_init(|| (codec.decode)(&self.bytes))
    }

    fn row_mut(&mut self, index: usize, codec: RowCodec<T>) -> &mut Row<T> {
        self.rows(codec);
        self.dirty = true;
        &mut self.decoded.get_mut().expect("initialized row block")[index]
    }

    fn release_cache(&mut self, codec: RowCodec<T>) {
        if let Some(rows) = self.decoded.take() {
            if self.dirty {
                self.bytes = (codec.encode)(&rows);
                self.dirty = false;
            }
        }
    }

    fn into_rows(self, codec: RowCodec<T>) -> Vec<Row<T>> {
        self.decoded
            .into_inner()
            .unwrap_or_else(|| (codec.decode)(&self.bytes))
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
}

impl<T> CompactRows<T> {
    pub fn new(rows: Vec<Row<T>>, visible: usize, columns: usize, codec: RowCodec<T>) -> Self {
        assert!(rows.len() >= visible);
        let row_bytes = columns
            .saturating_mul(std::mem::size_of::<T>())
            .saturating_add(std::mem::size_of::<Row<T>>())
            .max(1);
        let mut storage = Self {
            len: rows.len(),
            recent: rows.into(),
            blocks: VecDeque::new(),
            oldest: VecDeque::new(),
            codec,
            visible,
            block_rows: (BLOCK_CELL_BYTES / row_bytes).clamp(1, MAX_BLOCK_ROWS),
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

    pub fn row(&self, mut index: usize) -> &Row<T> {
        assert!(index < self.len);
        if index < self.recent.len() {
            return &self.recent[index];
        }
        index -= self.recent.len();
        let cold_rows = self.blocks.len() * self.block_rows;
        if index < cold_rows {
            let block = &self.blocks[index / self.block_rows];
            return &block.rows(self.codec)[index % self.block_rows];
        }
        &self.oldest[index - cold_rows]
    }

    pub fn row_mut(&mut self, mut index: usize) -> &mut Row<T> {
        assert!(index < self.len);
        if index < self.recent.len() {
            return &mut self.recent[index];
        }
        index -= self.recent.len();
        let cold_rows = self.blocks.len() * self.block_rows;
        if index < cold_rows {
            let block = &mut self.blocks[index / self.block_rows];
            return block.row_mut(index % self.block_rows, self.codec);
        }
        &mut self.oldest[index - cold_rows]
    }

    pub fn initialize(&mut self, count: usize, columns: usize)
    where
        T: Default,
    {
        self.oldest.extend((0..count).map(|_| Row::new(columns)));
        self.len += count;
    }

    /// Rotate left for positive counts, matching the dense storage's zero shift.
    pub fn rotate(&mut self, count: isize) {
        assert!(count.unsigned_abs() <= self.len);
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
        while self.len > len {
            if self.oldest.is_empty() {
                if let Some(block) = self.blocks.back() {
                    if self.len - len >= block.count {
                        self.len -= self.blocks.pop_back().expect("last block").count;
                        continue;
                    }
                }
            }
            self.pop_oldest();
            self.len -= 1;
        }
    }

    pub fn set_visible(&mut self, visible: usize) {
        assert!(visible <= self.len);
        self.visible = visible;
        while self.recent.len() < visible && !self.blocks.is_empty() {
            let block = self.blocks.pop_front().expect("first block");
            self.recent.extend(block.into_rows(self.codec));
        }
        while self.recent.len() < visible {
            self.recent
                .push_back(self.oldest.pop_front().expect("visible row"));
        }
        self.seal_recent();
    }

    /// Release all history read caches at a caller's exclusive borrow boundary.
    pub fn release_read_cache(&mut self) {
        for block in &mut self.blocks {
            block.release_cache(self.codec);
        }
    }

    /// Stored bytes, excluding the visible cells and temporary decode work.
    pub fn history_storage_bytes(&self) -> usize {
        let row_bytes = |row: &Row<T>| row.len().saturating_mul(std::mem::size_of::<T>());
        self.blocks
            .iter()
            .map(|block| block.bytes.len())
            .sum::<usize>()
            + self.blocks.capacity() * std::mem::size_of::<Block<T>>()
            + self.recent.capacity() * std::mem::size_of::<Row<T>>()
            + self.oldest.capacity() * std::mem::size_of::<Row<T>>()
            + self
                .recent
                .iter()
                .skip(self.visible)
                .map(row_bytes)
                .sum::<usize>()
            + self.oldest.iter().map(row_bytes).sum::<usize>()
    }

    /// Discard only the oldest history when the retained representation is full.
    pub fn bound_history_bytes(&mut self, budget: usize) {
        self.release_read_cache();
        while self.len > self.visible && self.history_storage_bytes() > budget {
            if self.oldest.is_empty() && self.blocks.len() > 1 {
                self.len -= self.blocks.pop_back().expect("oldest block").count;
            } else {
                self.pop_oldest();
                self.len -= 1;
            }
        }
        // Do not retain a large block-index allocation after erasing history.
        if self.blocks.capacity() > self.blocks.len().saturating_mul(2).saturating_add(64) {
            self.blocks.shrink_to_fit();
        }
    }

    pub fn into_rows(self) -> Vec<Row<T>> {
        let mut rows = Vec::with_capacity(self.len);
        rows.extend(self.recent);
        for block in self.blocks {
            rows.extend(block.into_rows(self.codec));
        }
        rows.extend(self.oldest);
        rows
    }

    pub fn drain_rows(&mut self) -> Vec<Row<T>> {
        let mut rows = Vec::with_capacity(self.len);
        rows.extend(self.recent.drain(..));
        for block in self.blocks.drain(..) {
            rows.extend(block.into_rows(self.codec));
        }
        rows.extend(self.oldest.drain(..));
        self.len = 0;
        rows
    }

    pub fn replace_rows(&mut self, rows: Vec<Row<T>>, visible: usize, columns: usize) {
        *self = Self::new(rows, visible, columns, self.codec);
    }

    fn pop_oldest(&mut self) -> Option<Row<T>> {
        if let Some(row) = self.oldest.pop_back() {
            return Some(row);
        }
        if let Some(block) = self.blocks.pop_back() {
            self.oldest = block.into_rows(self.codec).into();
            return self.oldest.pop_back();
        }
        self.recent.pop_back()
    }

    fn ensure_recent(&mut self) {
        if !self.recent.is_empty() {
            return;
        }
        if let Some(block) = self.blocks.pop_front() {
            self.recent.extend(block.into_rows(self.codec));
        } else {
            self.recent.append(&mut self.oldest);
        }
    }

    fn seal_recent(&mut self) {
        while self.recent.len() >= self.visible + self.block_rows {
            let rows = self.recent.split_off(self.recent.len() - self.block_rows);
            self.blocks.push_front(Block::new(rows.into(), self.codec));
        }
    }
}

#[derive(Serialize, Deserialize)]
struct PackedRows {
    styles: Vec<Cell>,
    rows: Vec<PackedRow>,
}

#[derive(Serialize, Deserialize)]
struct PackedRow(usize, String, Vec<(usize, usize)>);

#[derive(Clone, Eq, PartialEq)]
struct Style(Cell);

impl Hash for Style {
    fn hash<H: Hasher>(&self, state: &mut H) {
        fn color<H: Hasher>(value: Color, state: &mut H) {
            match value {
                Color::Named(value) => {
                    0u8.hash(state);
                    (value as usize).hash(state);
                }
                Color::Spec(value) => {
                    1u8.hash(state);
                    (value.r, value.g, value.b).hash(state);
                }
                Color::Indexed(value) => {
                    2u8.hash(state);
                    value.hash(state);
                }
            }
        }
        self.0.c.hash(state);
        color(self.0.fg, state);
        color(self.0.bg, state);
        self.0.flags.hash(state);
        self.0.zerowidth().hash(state);
        self.0.hyperlink().hash(state);
        self.0.underline_color().is_some().hash(state);
        if let Some(value) = self.0.underline_color() {
            color(value, state);
        }
    }
}

/// Lossless cell/style encoding for process-local history. This is not a
/// persistent format and must not be used to decode external protocol data.
pub fn cell_codec() -> RowCodec<Cell> {
    RowCodec {
        encode: encode_cells,
        decode: decode_cells,
    }
}

fn encode_cells(rows: &[Row<Cell>]) -> Box<[u8]> {
    let mut packed = PackedRows {
        styles: Vec::new(),
        rows: Vec::with_capacity(rows.len()),
    };
    let mut style_ids = HashMap::new();
    for row in rows {
        let mut text = String::with_capacity(row.len());
        let mut runs: Vec<(usize, usize)> = Vec::new();
        for col in 0..row.len() {
            let cell = &row[Column(col)];
            text.push(cell.c);
            let mut style = Style(cell.clone());
            style.0.c = ' ';
            let previous = runs.last().map(|run| run.0);
            let id = previous
                .filter(|&id| packed.styles[id] == style.0)
                .or_else(|| style_ids.get(&style).copied())
                .unwrap_or_else(|| {
                    let id = packed.styles.len();
                    style_ids.insert(style.clone(), id);
                    packed.styles.push(style.0);
                    id
                });
            if let Some(run) = runs.last_mut().filter(|run| run.0 == id) {
                run.1 += 1;
            } else {
                runs.push((id, 1));
            }
        }
        packed.rows.push(PackedRow(row.occ, text, runs));
    }
    let raw = serde_json::to_vec(&packed).expect("serialize typed terminal rows");
    let mut encoder = DeflateEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(&raw).expect("compress into memory");
    encoder
        .finish()
        .expect("finish in-memory compression")
        .into_boxed_slice()
}

fn decode_cells(bytes: &[u8]) -> Vec<Row<Cell>> {
    let mut raw = Vec::new();
    DeflateDecoder::new(bytes)
        .read_to_end(&mut raw)
        .expect("decode internally encoded rows");
    let packed: PackedRows = serde_json::from_slice(&raw).expect("internally encoded row layout");
    packed
        .rows
        .into_iter()
        .map(|PackedRow(occ, text, runs)| {
            let mut chars = text.chars();
            let count: usize = runs.iter().map(|run| run.1).sum();
            let mut cells = Vec::with_capacity(count);
            for (id, count) in runs {
                for _ in 0..count {
                    let mut cell = packed.styles[id].clone();
                    cell.c = chars.next().expect("one scalar per encoded cell");
                    cells.push(cell);
                }
            }
            assert!(chars.next().is_none());
            assert!(occ <= cells.len());
            Row::from_vec(cells, occ)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Term;
    use crate::event::VoidListener;
    use crate::grid::Dimensions;
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

    fn assert_rows(actual: &CompactRows<Cell>, expected: &[Row<Cell>]) {
        assert_eq!(actual.len(), expected.len());
        for (index, row) in expected.iter().enumerate() {
            // Row equality intentionally omits occupancy; compare it explicitly
            // because reset/reflow behavior depends on that private metadata.
            assert_eq!(actual.row(index), row, "row {index}");
            assert_eq!(actual.row(index).occ, row.occ, "occupancy {index}");
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
        for step in 0..350 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            let n = (seed >> 32) as usize;
            match n % 6 {
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
                    let len = expected.len().saturating_sub(n % 5).max(30);
                    actual.truncate(len);
                    expected.truncate(len);
                }
            }
            actual.set_visible(24);
            assert_rows(&actual, &expected);
            actual.release_read_cache();
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
}
