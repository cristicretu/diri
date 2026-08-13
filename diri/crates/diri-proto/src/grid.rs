//! Terminal grid and row RLE codecs.
//!
//! This is byte-for-byte compatible with
//! `Sources/DirijorProtocol/Grid.swift`. All integers are big-endian.

use std::error::Error;
use std::fmt;
use std::ops::{BitAnd, BitAndAssign, BitOr, BitOrAssign};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum TermColor {
    Default,
    DefaultInverted,
    Ansi(u8),
    Rgb(u8, u8, u8),
}

impl TermColor {
    /// Four-byte packed form `[tag][a][b][c]` used on the wire.
    #[must_use]
    pub const fn packed(self) -> u32 {
        match self {
            Self::Default => 0,
            Self::DefaultInverted => 1 << 24,
            Self::Ansi(color) => (2 << 24) | color as u32,
            Self::Rgb(red, green, blue) => {
                (3 << 24) | ((red as u32) << 16) | ((green as u32) << 8) | blue as u32
            }
        }
    }

    /// Decodes Swift's packed form. As in Swift, every tag other than 0, 1,
    /// and 2 is interpreted as RGB.
    #[must_use]
    pub const fn unpack(packed: u32) -> Self {
        match packed >> 24 {
            0 => Self::Default,
            1 => Self::DefaultInverted,
            2 => Self::Ansi((packed & 0xff) as u8),
            _ => Self::Rgb(
                ((packed >> 16) & 0xff) as u8,
                ((packed >> 8) & 0xff) as u8,
                (packed & 0xff) as u8,
            ),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
#[repr(transparent)]
pub struct TermStyle(u16);

impl TermStyle {
    // Exact SwiftTerm CharacterStyle bit layout.
    pub const BOLD: Self = Self(1 << 0);
    pub const UNDERLINE: Self = Self(1 << 1);
    pub const BLINK: Self = Self(1 << 2);
    pub const INVERSE: Self = Self(1 << 3);
    pub const INVISIBLE: Self = Self(1 << 4);
    pub const DIM: Self = Self(1 << 5);
    pub const ITALIC: Self = Self(1 << 6);
    pub const CROSSED_OUT: Self = Self(1 << 7);

    #[must_use]
    pub const fn empty() -> Self {
        Self(0)
    }

    #[must_use]
    pub const fn from_bits_retain(bits: u16) -> Self {
        Self(bits)
    }

    #[must_use]
    pub const fn bits(self) -> u16 {
        self.0
    }

    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
}

impl BitOr for TermStyle {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

impl BitOrAssign for TermStyle {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

impl BitAnd for TermStyle {
    type Output = Self;

    fn bitand(self, rhs: Self) -> Self::Output {
        Self(self.0 & rhs.0)
    }
}

impl BitAndAssign for TermStyle {
    fn bitand_assign(&mut self, rhs: Self) {
        self.0 &= rhs.0;
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct GridCell {
    /// Unicode scalar value; zero is rendered as blank by the client.
    pub scalar: u32,
    pub fg: TermColor,
    pub bg: TermColor,
    pub style: TermStyle,
}

impl GridCell {
    pub const BLANK: Self = Self {
        scalar: 32,
        fg: TermColor::Default,
        bg: TermColor::DefaultInverted,
        style: TermStyle::empty(),
    };

    #[must_use]
    pub const fn new(scalar: u32, fg: TermColor, bg: TermColor, style: TermStyle) -> Self {
        Self {
            scalar,
            fg,
            bg,
            style,
        }
    }
}

impl Default for GridCell {
    fn default() -> Self {
        Self::BLANK
    }
}

/// A changed row and its zero-based grid coordinate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChangedRow {
    pub y: u16,
    pub cells: Vec<GridCell>,
}

impl ChangedRow {
    #[must_use]
    pub fn new(y: u16, cells: Vec<GridCell>) -> Self {
        Self { y, cells }
    }
}

/// Grid geometry, cursor state, and either a full snapshot or changed rows.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GridUpdate {
    pub cols: u16,
    pub rows: u16,
    pub cursor_col: u16,
    pub cursor_row: u16,
    pub cursor_visible: bool,
    pub is_full_snapshot: bool,
    pub changed_rows: Vec<ChangedRow>,
}

impl GridUpdate {
    /// Folds a newer contiguous update into this one while preserving the
    /// exact final grid. The newest row wins; a new snapshot or geometry
    /// replaces everything before it. This is the safe primitive used to
    /// collapse transport bursts before touching the renderer.
    pub fn coalesce(&mut self, newer: Self) {
        if newer.is_full_snapshot || newer.cols != self.cols || newer.rows != self.rows {
            *self = newer;
            return;
        }
        self.cursor_col = newer.cursor_col;
        self.cursor_row = newer.cursor_row;
        self.cursor_visible = newer.cursor_visible;
        for row in newer.changed_rows {
            // Decoding intentionally accepts duplicate row ids. Sequential
            // application makes the last one authoritative, so remove every
            // older occurrence before appending the newer row.
            self.changed_rows.retain(|existing| existing.y != row.y);
            self.changed_rows.push(row);
        }
        // Stable ordering preserves the original last-write-wins meaning for
        // any untouched duplicate rows in an accepted input update.
        self.changed_rows.sort_by_key(|row| row.y);
    }

    pub fn encode(&self) -> Result<Vec<u8>, GridCodecError> {
        let row_count = u16::try_from(self.changed_rows.len())
            .map_err(|_| GridCodecError::TooManyRows(self.changed_rows.len()))?;
        let mut encoded = Vec::new();
        put_u16(&mut encoded, self.cols);
        put_u16(&mut encoded, self.rows);
        put_u16(&mut encoded, self.cursor_col);
        put_u16(&mut encoded, self.cursor_row);
        let flags = u8::from(self.cursor_visible) | (u8::from(self.is_full_snapshot) << 1);
        encoded.push(flags);
        put_u16(&mut encoded, row_count);
        for row in &self.changed_rows {
            put_u16(&mut encoded, row.y);
            GridRowCodec::append_row(&row.cells, &mut encoded)?;
        }
        Ok(encoded)
    }

    pub fn decode(payload: &[u8]) -> Result<Self, GridCodecError> {
        let mut cursor = Cursor::new(payload);
        let cols = cursor.u16()?;
        let rows = cursor.u16()?;
        let cursor_col = cursor.u16()?;
        let cursor_row = cursor.u16()?;
        let flags = cursor.u8()?;
        let row_count = cursor.u16()?;
        let mut changed_rows = Vec::with_capacity(row_count as usize);
        for _ in 0..row_count {
            let y = cursor.u16()?;
            let cells = GridRowCodec::read_row_from_cursor(&mut cursor)?;
            changed_rows.push(ChangedRow { y, cells });
        }
        Ok(Self {
            cols,
            rows,
            cursor_col,
            cursor_row,
            cursor_visible: flags & 1 != 0,
            is_full_snapshot: flags & 2 != 0,
            changed_rows,
        })
    }

    /// Applies the update to a row-major cell buffer.
    ///
    /// A full snapshot (or a mismatched buffer length) clears the buffer to
    /// blanks first. Changed rows are padded or truncated to `cols`, matching
    /// the Swift renderers, and out-of-range row indices are ignored.
    pub fn apply(&self, buffer: &mut Vec<GridCell>) {
        let cols = usize::from(self.cols);
        let rows = usize::from(self.rows);
        let cell_count = cols * rows;
        if self.is_full_snapshot || buffer.len() != cell_count {
            buffer.clear();
            buffer.resize(cell_count, GridCell::BLANK);
        }

        for changed in &self.changed_rows {
            let y = usize::from(changed.y);
            if y >= rows {
                continue;
            }
            let start = y * cols;
            let copied = changed.cells.len().min(cols);
            buffer[start..start + copied].copy_from_slice(&changed.cells[..copied]);
            buffer[start + copied..start + cols].fill(GridCell::BLANK);
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GridCodecError {
    UnexpectedEnd {
        offset: usize,
        needed: usize,
        remaining: usize,
    },
    TooManyRows(usize),
    TooManyCells(usize),
    DecodedRowTooLong(usize),
}

impl fmt::Display for GridCodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnexpectedEnd {
                offset,
                needed,
                remaining,
            } => write!(
                f,
                "truncated grid payload at byte {offset}: need {needed} bytes, have {remaining}"
            ),
            Self::TooManyRows(count) => write!(f, "row count {count} does not fit in u16"),
            Self::TooManyCells(count) => {
                write!(f, "row cell count {count} does not fit in u16")
            }
            Self::DecodedRowTooLong(count) => {
                write!(f, "decoded row exceeds the u16 grid width ({count} cells)")
            }
        }
    }
}

impl Error for GridCodecError {}

/// RLE codec shared by grid frames and `session.read_scrollback_cells`.
pub struct GridRowCodec;

impl GridRowCodec {
    /// Appends `[runCount u16]` and the row's RLE runs, without a `y` prefix.
    pub fn append_row(cells: &[GridCell], encoded: &mut Vec<u8>) -> Result<(), GridCodecError> {
        if cells.len() > usize::from(u16::MAX) {
            return Err(GridCodecError::TooManyCells(cells.len()));
        }

        // Reserve the run-count field and backfill it after writing runs
        // directly into the caller's buffer. The old implementation allocated
        // and copied a temporary Vec for every changed terminal row.
        let count_offset = encoded.len();
        encoded.extend_from_slice(&0_u16.to_be_bytes());
        let mut run_count = 0_u16;
        let mut index = 0;
        while index < cells.len() {
            let cell = cells[index];
            let mut repeat = 1_u16;
            let mut next = index + 1;
            while next < cells.len() && cells[next] == cell && repeat < u16::MAX {
                repeat += 1;
                next += 1;
            }
            put_u16(encoded, repeat);
            put_u32(encoded, cell.scalar);
            put_u32(encoded, cell.fg.packed());
            put_u32(encoded, cell.bg.packed());
            put_u16(encoded, cell.style.bits());
            run_count += 1;
            index = next;
        }
        encoded[count_offset..count_offset + 2].copy_from_slice(&run_count.to_be_bytes());
        Ok(())
    }

    pub fn encode_row(cells: &[GridCell]) -> Result<Vec<u8>, GridCodecError> {
        let mut encoded = Vec::new();
        Self::append_row(cells, &mut encoded)?;
        Ok(encoded)
    }

    /// Reads one row, advancing `offset` past its body.
    pub fn read_row(payload: &[u8], offset: &mut usize) -> Result<Vec<GridCell>, GridCodecError> {
        let mut cursor = Cursor::at(payload, *offset);
        let cells = Self::read_row_from_cursor(&mut cursor)?;
        *offset = cursor.offset;
        Ok(cells)
    }

    pub fn encode_rows(rows: &[Vec<GridCell>]) -> Result<Vec<u8>, GridCodecError> {
        let mut encoded = Vec::new();
        for row in rows {
            Self::append_row(row, &mut encoded)?;
        }
        Ok(encoded)
    }

    /// Decodes exactly `row_count` consecutive rows. Like Swift, trailing bytes
    /// after those rows are ignored.
    pub fn decode_rows(
        payload: &[u8],
        row_count: usize,
    ) -> Result<Vec<Vec<GridCell>>, GridCodecError> {
        let mut offset = 0;
        let mut rows = Vec::with_capacity(row_count);
        for _ in 0..row_count {
            rows.push(Self::read_row(payload, &mut offset)?);
        }
        Ok(rows)
    }

    fn read_row_from_cursor(cursor: &mut Cursor<'_>) -> Result<Vec<GridCell>, GridCodecError> {
        let run_count = cursor.u16()?;
        let mut cells = Vec::new();
        for _ in 0..run_count {
            let repeat = cursor.u16()?;
            let scalar = cursor.u32()?;
            let fg = TermColor::unpack(cursor.u32()?);
            let bg = TermColor::unpack(cursor.u32()?);
            let style = TermStyle::from_bits_retain(cursor.u16()?);
            let new_len = cells.len() + usize::from(repeat);
            if new_len > usize::from(u16::MAX) {
                return Err(GridCodecError::DecodedRowTooLong(new_len));
            }
            cells.resize(new_len, GridCell::new(scalar, fg, bg, style));
        }
        Ok(cells)
    }
}

struct Cursor<'a> {
    payload: &'a [u8],
    offset: usize,
}

impl<'a> Cursor<'a> {
    fn new(payload: &'a [u8]) -> Self {
        Self::at(payload, 0)
    }

    fn at(payload: &'a [u8], offset: usize) -> Self {
        Self { payload, offset }
    }

    fn u8(&mut self) -> Result<u8, GridCodecError> {
        self.take::<1>().map(|bytes| bytes[0])
    }

    fn u16(&mut self) -> Result<u16, GridCodecError> {
        self.take::<2>().map(u16::from_be_bytes)
    }

    fn u32(&mut self) -> Result<u32, GridCodecError> {
        self.take::<4>().map(u32::from_be_bytes)
    }

    fn take<const N: usize>(&mut self) -> Result<[u8; N], GridCodecError> {
        let remaining = self.payload.len().saturating_sub(self.offset);
        if remaining < N {
            return Err(GridCodecError::UnexpectedEnd {
                offset: self.offset,
                needed: N,
                remaining,
            });
        }
        let bytes = self.payload[self.offset..self.offset + N]
            .try_into()
            .expect("slice length checked");
        self.offset += N;
        Ok(bytes)
    }
}

fn put_u16(encoded: &mut Vec<u8>, value: u16) {
    encoded.extend_from_slice(&value.to_be_bytes());
}

fn put_u32(encoded: &mut Vec<u8>, value: u32) {
    encoded.extend_from_slice(&value.to_be_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    const STYLE_FLAGS: [TermStyle; 8] = [
        TermStyle::BOLD,
        TermStyle::UNDERLINE,
        TermStyle::BLINK,
        TermStyle::INVERSE,
        TermStyle::INVISIBLE,
        TermStyle::DIM,
        TermStyle::ITALIC,
        TermStyle::CROSSED_OUT,
    ];

    #[test]
    fn color_pack_unpack_is_exhaustive() {
        assert_eq!(TermColor::Default.packed(), 0);
        assert_eq!(TermColor::DefaultInverted.packed(), 0x0100_0000);
        assert_eq!(TermColor::unpack(0), TermColor::Default);
        assert_eq!(TermColor::unpack(0x0100_0000), TermColor::DefaultInverted);

        for color in u8::MIN..=u8::MAX {
            let value = TermColor::Ansi(color);
            assert_eq!(value.packed(), 0x0200_0000 | u32::from(color));
            assert_eq!(TermColor::unpack(value.packed()), value);
        }

        // Every representable RGB color, not just boundary examples.
        for rgb in 0_u32..=0x00ff_ffff {
            let value = TermColor::Rgb(
                ((rgb >> 16) & 0xff) as u8,
                ((rgb >> 8) & 0xff) as u8,
                (rgb & 0xff) as u8,
            );
            assert_eq!(value.packed(), 0x0300_0000 | rgb);
            assert_eq!(TermColor::unpack(value.packed()), value);
        }

        // Swift's `default` switch branch treats unknown tags as RGB.
        assert_eq!(
            TermColor::unpack(0xff12_3456),
            TermColor::Rgb(0x12, 0x34, 0x56)
        );
    }

    #[test]
    fn every_style_bit_matches_swiftterm() {
        let mut combined = TermStyle::empty();
        for (bit, flag) in STYLE_FLAGS.into_iter().enumerate() {
            assert_eq!(flag.bits(), 1 << bit);
            assert!(flag.contains(flag));
            combined |= flag;
        }
        assert_eq!(combined.bits(), 0x00ff);
        assert!(!combined.is_empty());
        assert_eq!(TermStyle::from_bits_retain(u16::MAX).bits(), u16::MAX);
        assert_eq!((combined & TermStyle::ITALIC).bits(), 1 << 6);
    }

    #[test]
    fn known_grid_update_is_byte_exact() {
        let update = GridUpdate {
            cols: 3,
            rows: 1,
            cursor_col: 2,
            cursor_row: 0,
            cursor_visible: true,
            is_full_snapshot: true,
            changed_rows: vec![ChangedRow::new(
                0,
                vec![
                    GridCell::BLANK,
                    GridCell::BLANK,
                    GridCell::new(
                        u32::from('A'),
                        TermColor::Ansi(7),
                        TermColor::Rgb(1, 2, 3),
                        TermStyle::BOLD | TermStyle::ITALIC,
                    ),
                ],
            )],
        };
        let expected = vec![
            0, 3, 0, 1, 0, 2, 0, 0, 3, 0, 1, // header
            0, 0, 0, 2, // y, run count
            0, 2, 0, 0, 0, 32, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, // 2 blanks
            0, 1, 0, 0, 0, 65, 2, 0, 0, 7, 3, 1, 2, 3, 0, 65, // A
        ];
        let encoded = update.encode().unwrap();
        assert_eq!(encoded, expected);
        assert_eq!(GridUpdate::decode(&encoded).unwrap(), update);
    }

    #[test]
    fn coalescing_contiguous_updates_preserves_the_final_grid() {
        let blank = vec![GridCell::BLANK; 3];
        let mut first = GridUpdate {
            cols: 3,
            rows: 2,
            cursor_col: 0,
            cursor_row: 0,
            cursor_visible: true,
            is_full_snapshot: true,
            changed_rows: vec![
                ChangedRow::new(0, blank.clone()),
                ChangedRow::new(1, blank.clone()),
            ],
        };
        let mut row_zero = blank.clone();
        row_zero[0].scalar = u32::from('a');
        let second = GridUpdate {
            cols: 3,
            rows: 2,
            cursor_col: 1,
            cursor_row: 0,
            cursor_visible: true,
            is_full_snapshot: false,
            changed_rows: vec![ChangedRow::new(0, row_zero)],
        };
        let mut row_one = blank;
        row_one[0].scalar = u32::from('b');
        let third = GridUpdate {
            cols: 3,
            rows: 2,
            cursor_col: 1,
            cursor_row: 1,
            cursor_visible: false,
            is_full_snapshot: false,
            changed_rows: vec![ChangedRow::new(1, row_one)],
        };

        let mut sequential = Vec::new();
        first.apply(&mut sequential);
        second.apply(&mut sequential);
        third.apply(&mut sequential);

        first.coalesce(second);
        first.coalesce(third);
        let mut coalesced = Vec::new();
        first.apply(&mut coalesced);
        assert_eq!(coalesced, sequential);
        assert_eq!((first.cursor_col, first.cursor_row), (1, 1));
        assert!(!first.cursor_visible);
        assert!(first.is_full_snapshot);
    }

    #[test]
    fn row_codec_round_trips_scrollback_blocks() {
        let rows = vec![
            Vec::new(),
            vec![GridCell::BLANK; 80],
            vec![
                GridCell::new(
                    u32::from('x'),
                    TermColor::Rgb(255, 1, 2),
                    TermColor::Ansi(9),
                    TermStyle::UNDERLINE,
                ),
                GridCell::BLANK,
            ],
        ];
        let encoded = GridRowCodec::encode_rows(&rows).unwrap();
        assert_eq!(
            GridRowCodec::decode_rows(&encoded, rows.len()).unwrap(),
            rows
        );
    }

    #[test]
    fn randomized_grid_round_trips_and_rle_stays_within_rows() {
        let mut random = Random::new(0xd121_601d_5eed_u64);
        for case in 0..512 {
            let cols = random.bounded(41) as u16;
            let rows = random.bounded(21) as u16;
            let mut changed_rows = Vec::with_capacity(rows as usize);
            for y in 0..rows {
                let mut cells = Vec::with_capacity(cols as usize);
                while cells.len() < cols as usize {
                    let cell = random.cell();
                    let remaining = cols as usize - cells.len();
                    let repeat = 1 + random.bounded(remaining.min(8) as u32) as usize;
                    cells.resize(cells.len() + repeat, cell);
                }
                changed_rows.push(ChangedRow::new(y, cells));
            }
            let update = GridUpdate {
                cols,
                rows,
                cursor_col: random.bounded(u32::from(cols) + 1) as u16,
                cursor_row: random.bounded(u32::from(rows) + 1) as u16,
                cursor_visible: random.bool(),
                is_full_snapshot: random.bool(),
                changed_rows,
            };
            let encoded = update.encode().unwrap();
            assert_eq!(GridUpdate::decode(&encoded).unwrap(), update, "case {case}");
            assert_rle_rows_are_independent(&encoded, &update);
        }
    }

    #[test]
    fn apply_replaces_or_patches_rows_and_normalizes_width() {
        let red = GridCell::new(
            u32::from('r'),
            TermColor::Rgb(255, 0, 0),
            TermColor::Default,
            TermStyle::BOLD,
        );
        let full = GridUpdate {
            cols: 3,
            rows: 2,
            cursor_col: 0,
            cursor_row: 0,
            cursor_visible: false,
            is_full_snapshot: true,
            changed_rows: vec![
                ChangedRow::new(0, vec![red]),
                ChangedRow::new(1, vec![red, red, red, red]),
                ChangedRow::new(2, vec![red]),
            ],
        };
        let mut buffer = vec![red; 100];
        full.apply(&mut buffer);
        assert_eq!(
            buffer,
            vec![
                GridCell::new(
                    u32::from('r'),
                    TermColor::Rgb(255, 0, 0),
                    TermColor::Default,
                    TermStyle::BOLD
                ),
                GridCell::BLANK,
                GridCell::BLANK,
                red,
                red,
                red
            ]
        );

        let patch = GridUpdate {
            is_full_snapshot: false,
            changed_rows: vec![ChangedRow::new(0, vec![GridCell::BLANK; 3])],
            ..full
        };
        patch.apply(&mut buffer);
        assert_eq!(&buffer[..3], &[GridCell::BLANK; 3]);
        assert_eq!(&buffer[3..], &[red; 3]);
    }

    #[test]
    fn malformed_payloads_are_rejected_without_panics() {
        let valid = GridUpdate {
            cols: 1,
            rows: 1,
            cursor_col: 0,
            cursor_row: 0,
            cursor_visible: true,
            is_full_snapshot: true,
            changed_rows: vec![ChangedRow::new(0, vec![GridCell::BLANK])],
        }
        .encode()
        .unwrap();
        for end in 0..valid.len() {
            assert!(GridUpdate::decode(&valid[..end]).is_err(), "prefix {end}");
        }
    }

    fn assert_rle_rows_are_independent(encoded: &[u8], update: &GridUpdate) {
        let mut cursor = Cursor::at(encoded, 11);
        for expected_row in &update.changed_rows {
            assert_eq!(cursor.u16().unwrap(), expected_row.y);
            let run_count = cursor.u16().unwrap();
            let mut repeats = 0_usize;
            for _ in 0..run_count {
                let repeat = cursor.u16().unwrap();
                assert_ne!(repeat, 0);
                repeats += repeat as usize;
                cursor.u32().unwrap();
                cursor.u32().unwrap();
                cursor.u32().unwrap();
                cursor.u16().unwrap();
            }
            assert_eq!(repeats, usize::from(update.cols));
        }
        assert_eq!(cursor.offset, encoded.len());
    }

    struct Random(u64);

    impl Random {
        fn new(seed: u64) -> Self {
            Self(seed)
        }

        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn bounded(&mut self, upper_exclusive: u32) -> u32 {
            if upper_exclusive == 0 {
                return 0;
            }
            (self.next() % u64::from(upper_exclusive)) as u32
        }

        fn bool(&mut self) -> bool {
            self.next() & 1 != 0
        }

        fn cell(&mut self) -> GridCell {
            let fg = self.color();
            let bg = self.color();
            GridCell::new(
                self.bounded(0x11_0000),
                fg,
                bg,
                TermStyle::from_bits_retain(self.next() as u16),
            )
        }

        fn color(&mut self) -> TermColor {
            match self.bounded(4) {
                0 => TermColor::Default,
                1 => TermColor::DefaultInverted,
                2 => TermColor::Ansi(self.next() as u8),
                _ => TermColor::Rgb(self.next() as u8, self.next() as u8, self.next() as u8),
            }
        }
    }
}
