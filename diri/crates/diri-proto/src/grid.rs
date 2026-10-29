//! Terminal grid and row RLE codecs.
//!
//! This is byte-for-byte compatible with
//! `Sources/DirijorProtocol/Grid.swift`. All integers are big-endian.

use serde::{Deserialize, Serialize};
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

/// The outcome an OSC 133 D mark reports.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum CommandEnd {
    /// Exit status 0.
    Succeeded,
    /// A non-zero exit status.
    Failed,
    /// The shell did not report a usable status.
    Unknown,
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
    /// Additive semantic bits; older renderers ignore these.
    pub const SOFT_WRAP: Self = Self(1 << 8);
    pub const WIDE_SPACER: Self = Self(1 << 9);
    pub const PROMPT_START: Self = Self(1 << 10);
    /// OSC 133 B/C/D shell-integration marks, carried like PROMPT_START.
    /// A command end is `COMMAND_SUCCEEDED`, `COMMAND_FAILED`, or both
    /// when the shell reported no status: see [`Self::command_end`].
    pub const INPUT_START: Self = Self(1 << 11);
    pub const OUTPUT_START: Self = Self(1 << 12);
    pub const COMMAND_SUCCEEDED: Self = Self(1 << 13);
    pub const COMMAND_FAILED: Self = Self(1 << 14);
    pub const COMMAND_END: Self = Self(Self::COMMAND_SUCCEEDED.0 | Self::COMMAND_FAILED.0);

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

    /// How the command whose end (OSC 133 D) is marked on this cell ended.
    #[must_use]
    pub const fn command_end(self) -> Option<CommandEnd> {
        match (
            self.0 & Self::COMMAND_SUCCEEDED.0 != 0,
            self.0 & Self::COMMAND_FAILED.0 != 0,
        ) {
            (false, false) => None,
            (true, false) => Some(CommandEnd::Succeeded),
            (false, true) => Some(CommandEnd::Failed),
            (true, true) => Some(CommandEnd::Unknown),
        }
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

/// DECSCUSR cursor shape.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub enum CursorShape {
    #[default]
    Block,
    Underline,
    Bar,
}

/// The cursor a program asked for with DECSCUSR (or mode 12).
///
/// It rides grid flag bits 3-5, which every earlier decoder ignores, so the
/// default (a blinking block: what Diri drew before) is all-zero bits and an
/// older Engine or Holder reads as exactly that.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct CursorStyle {
    pub shape: CursorShape,
    /// The program asked for a cursor that does not blink.
    pub steady: bool,
}

impl CursorStyle {
    const SHAPE_SHIFT: u8 = 3;
    const STEADY_BIT: u8 = 1 << 5;
    const MASK: u8 = 0b11 << Self::SHAPE_SHIFT | Self::STEADY_BIT;

    #[must_use]
    pub const fn flag_bits(self) -> u8 {
        let shape = match self.shape {
            CursorShape::Block => 0,
            CursorShape::Underline => 1,
            CursorShape::Bar => 2,
        };
        shape << Self::SHAPE_SHIFT | if self.steady { Self::STEADY_BIT } else { 0 }
    }

    /// Reads bits 3-5 of a grid flags byte. The unused shape value 3 reads
    /// as a block, so a future shape degrades to the old cursor.
    #[must_use]
    pub const fn from_flag_bits(flags: u8) -> Self {
        Self {
            shape: match (flags >> Self::SHAPE_SHIFT) & 0b11 {
                1 => CursorShape::Underline,
                2 => CursorShape::Bar,
                _ => CursorShape::Block,
            },
            steady: flags & Self::STEADY_BIT != 0,
        }
    }
}

/// Underline kinds beyond the one `TermStyle::UNDERLINE` bit (SGR `4:n`).
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum UnderlineKind {
    #[default]
    None,
    Single,
    Double,
    Curly,
    Dotted,
    Dashed,
}

impl UnderlineKind {
    #[must_use]
    pub const fn is_none(&self) -> bool {
        matches!(self, Self::None)
    }
}

/// Decoration the u16 cell style cannot hold: the underline kind, its color
/// (SGR 58) and overline (SGR 53), for the half-open cell range
/// `start..end`. Cells keep `TermStyle::UNDERLINE`, so a renderer that does
/// not know spans still draws a single underline.
#[derive(Clone, Debug, Default, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StyleSpan {
    pub start: u16,
    pub end: u16,
    #[serde(default, skip_serializing_if = "UnderlineKind::is_none")]
    pub underline: UnderlineKind,
    /// Packed [`TermColor`]; absent means the cell's own foreground.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub underline_color: Option<u32>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub overline: bool,
}

impl StyleSpan {
    /// Whether two spans decorate their cells identically.
    #[must_use]
    pub fn same_decoration(&self, other: &Self) -> bool {
        self.underline == other.underline
            && self.underline_color == other.underline_color
            && self.overline == other.overline
    }
}

/// Bounded per-row annotations. Targets are spans, never one String per cell.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RowMetadata {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<LinkSpan>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub graphemes: Vec<(u16, String)>,
    /// `terminal-styles-v1`: sorted, non-overlapping decoration spans. Only a
    /// peer that negotiated the capability receives them; older decoders
    /// reject unknown fields here.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub styles: Vec<StyleSpan>,
}

/// The pre-`terminal-styles-v1` annotation form, borrowed for encoding.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct LegacyRowMetadata<'a> {
    #[serde(skip_serializing_if = "<[LinkSpan]>::is_empty")]
    links: &'a [LinkSpan],
    #[serde(skip_serializing_if = "<[(u16, String)]>::is_empty")]
    graphemes: &'a [(u16, String)],
}

/// Most spans one row may carry: one per cell.
pub const MAX_STYLE_SPANS_PER_ROW: usize = 512;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LinkSpan {
    pub start: u16,
    pub end: u16,
    pub uri: String,
}

pub const MAX_GRID_METADATA_BYTES: usize = 256 * 1024;
pub const MAX_LINK_URI_BYTES: usize = 2048;

impl RowMetadata {
    pub fn is_empty(&self) -> bool {
        self.links.is_empty() && self.graphemes.is_empty() && self.styles.is_empty()
    }

    /// True when only `terminal-styles-v1` content is missing from the
    /// legacy form.
    fn is_legacy_empty(&self) -> bool {
        self.links.is_empty() && self.graphemes.is_empty()
    }

    /// The decoration span covering `col`, if any. Spans are sorted.
    #[must_use]
    pub fn style_at(&self, col: usize) -> Option<&StyleSpan> {
        let index = self
            .styles
            .partition_point(|span| usize::from(span.end) <= col);
        self.styles
            .get(index)
            .filter(|span| usize::from(span.start) <= col)
    }

    pub fn validate(&self, cols: usize) -> bool {
        let mut end = 0;
        for link in &self.links {
            if link.start < end
                || link.start >= link.end
                || usize::from(link.end) > cols
                || link.uri.is_empty()
                || link.uri.len() > MAX_LINK_URI_BYTES
                || link.uri.chars().any(char::is_control)
            {
                return false;
            }
            end = link.end;
        }
        let mut previous = None;
        for (col, text) in &self.graphemes {
            if usize::from(*col) >= cols
                || previous.is_some_and(|p| p >= *col)
                || text.len() > 64
                || text.chars().any(char::is_control)
            {
                return false;
            }
            previous = Some(*col);
        }
        if self.styles.len() > MAX_STYLE_SPANS_PER_ROW {
            return false;
        }
        let mut end = 0;
        for span in &self.styles {
            if span.start < end
                || span.start >= span.end
                || usize::from(span.end) > cols
                || (span.underline.is_none() && !span.overline)
                || (span.underline.is_none() && span.underline_color.is_some())
            {
                return false;
            }
            end = span.end;
        }
        true
    }
}

/// A changed row and its zero-based grid coordinate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChangedRow {
    pub y: u16,
    pub cells: Vec<GridCell>,
    pub metadata: RowMetadata,
}

impl ChangedRow {
    #[must_use]
    pub fn new(y: u16, cells: Vec<GridCell>) -> Self {
        Self {
            y,
            cells,
            metadata: RowMetadata::default(),
        }
    }
}

/// Where the primary screen's history stood when a grid was published
/// (`grid-history-v1`).
///
/// Both counts are absolute, so a client compares any two publications it
/// applied, across coalesced, discarded or reseeded diffs:
/// `(origin + rows) - (origin' + rows')` lines scrolled into history between
/// them, and `origin - origin'` of the oldest left it. History row `i` of a
/// `session.read_scrollback_cells` reply made at the same moment is
/// `origin + i` of everything the terminal has held, so `rows` is that
/// reply's `liveStartRow`.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct GridHistory {
    /// Rows that have left history from its oldest end (evicted or cleared).
    pub origin: u64,
    /// Rows history holds: the absolute row the live grid starts at.
    pub rows: u64,
}

impl GridHistory {
    /// Every row this terminal has pushed into history.
    #[must_use]
    pub const fn pushed(self) -> u64 {
        self.origin.saturating_add(self.rows)
    }

    /// Lines that scrolled into history after `earlier`, and how many rows
    /// left it from its oldest end, when this position can follow `earlier`
    /// on the same screen. `None` when either count went backwards: a new
    /// terminal (Engine restart, Holder replacement) or a reflow.
    #[must_use]
    pub const fn since(self, earlier: Self) -> Option<(u64, u64)> {
        if self.origin < earlier.origin || self.pushed() < earlier.pushed() {
            return None;
        }
        Some((
            self.pushed() - earlier.pushed(),
            self.origin - earlier.origin,
        ))
    }
}

/// Grid flag bit 6: a [`GridHistory`] follows the changed rows.
const HISTORY_FLAG: u8 = 1 << 6;

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
    /// DECSCUSR shape and blink; see [`CursorStyle`].
    pub cursor_style: CursorStyle,
    /// The primary screen's history position, for a `grid-history-v1` peer.
    /// `None` on the alternate screen, which has no history, and from a
    /// publisher or for a peer without it.
    pub history: Option<GridHistory>,
}

impl GridUpdate {
    /// Whether this update carries anything only a `terminal-styles-v1`
    /// peer understands, so a second, legacy encoding is needed.
    #[must_use]
    pub fn has_style_extensions(&self) -> bool {
        self.cursor_style != CursorStyle::default()
            || self
                .changed_rows
                .iter()
                .any(|row| !row.metadata.styles.is_empty())
    }

    /// The update as a peer without `terminal-styles-v1` sees it: no
    /// decoration spans and the default cursor. A legacy peer never has
    /// `grid-history-v1` either.
    pub fn strip_styles(&mut self) {
        self.history = None;
        self.cursor_style = CursorStyle::default();
        for row in &mut self.changed_rows {
            row.metadata.styles.clear();
        }
    }

    /// The exact bytes a pre-`terminal-styles-v1` Engine produced for this
    /// screen. Equivalent to `strip_styles` then `encode`, without a copy.
    pub fn encode_without_styles(&self) -> Result<Vec<u8>, GridCodecError> {
        self.encode_with(false, false)
    }

    /// The styled form for a peer without `grid-history-v1`: the exact bytes
    /// [`Self::encode`] produced before history positions existed.
    pub fn encode_without_history(&self) -> Result<Vec<u8>, GridCodecError> {
        self.encode_with(true, false)
    }
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
        self.cursor_style = newer.cursor_style;
        // Absolute, so the newest position describes the folded update.
        self.history = newer.history;
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

    /// The full form, `terminal-styles-v1` and `grid-history-v1` included.
    pub fn encode(&self) -> Result<Vec<u8>, GridCodecError> {
        self.encode_with(true, true)
    }

    fn encode_with(&self, styles: bool, history: bool) -> Result<Vec<u8>, GridCodecError> {
        let row_count = u16::try_from(self.changed_rows.len())
            .map_err(|_| GridCodecError::TooManyRows(self.changed_rows.len()))?;
        let mut encoded = Vec::new();
        put_u16(&mut encoded, self.cols);
        put_u16(&mut encoded, self.rows);
        put_u16(&mut encoded, self.cursor_col);
        put_u16(&mut encoded, self.cursor_row);
        let annotated = |row: &&ChangedRow| {
            if styles {
                !row.metadata.is_empty()
            } else {
                !row.metadata.is_legacy_empty()
            }
        };
        let has_metadata = self.changed_rows.iter().any(|row| annotated(&row));
        let mut flags = u8::from(self.cursor_visible)
            | (u8::from(self.is_full_snapshot) << 1)
            | (u8::from(has_metadata) << 2);
        if styles {
            flags |= self.cursor_style.flag_bits();
        }
        let position = self.history.filter(|_| history);
        if position.is_some() {
            flags |= HISTORY_FLAG;
        }
        encoded.push(flags);
        put_u16(&mut encoded, row_count);
        for row in &self.changed_rows {
            put_u16(&mut encoded, row.y);
            GridRowCodec::append_row(&row.cells, &mut encoded)?;
        }
        // Before the annotation block, whose length must end the payload.
        if let Some(position) = position {
            put_varint(&mut encoded, position.origin);
            put_varint(&mut encoded, position.rows);
        }
        if has_metadata {
            if self
                .changed_rows
                .iter()
                .any(|row| !row.metadata.validate(row.cells.len()))
            {
                return Err(GridCodecError::InvalidMetadata);
            }
            let rows = self.changed_rows.iter().filter(annotated);
            let data = if styles {
                serde_json::to_vec(&rows.map(|row| (row.y, &row.metadata)).collect::<Vec<_>>())
            } else {
                serde_json::to_vec(
                    &rows
                        .map(|row| {
                            (
                                row.y,
                                LegacyRowMetadata {
                                    links: &row.metadata.links,
                                    graphemes: &row.metadata.graphemes,
                                },
                            )
                        })
                        .collect::<Vec<_>>(),
                )
            }
            .map_err(|_| GridCodecError::InvalidMetadata)?;
            if data.len() > MAX_GRID_METADATA_BYTES {
                return Err(GridCodecError::InvalidMetadata);
            }
            encoded.push(1); // annotation extension version
            encoded.extend_from_slice(&(data.len() as u32).to_be_bytes());
            encoded.extend_from_slice(&data);
        }
        Ok(encoded)
    }

    /// Preserve the original wire form for a surviving pre-1.6 controller.
    pub fn without_annotations(mut self) -> Self {
        for row in &mut self.changed_rows {
            row.metadata = RowMetadata::default();
        }
        self
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
            changed_rows.push(ChangedRow::new(y, cells));
        }
        let history = if flags & HISTORY_FLAG != 0 {
            Some(GridHistory {
                origin: cursor.varint()?,
                rows: cursor.varint()?,
            })
        } else {
            None
        };
        if flags & 4 != 0 {
            if cursor.u8()? != 1 {
                return Err(GridCodecError::InvalidMetadata);
            }
            let length = cursor.u32()? as usize;
            if length > MAX_GRID_METADATA_BYTES
                || payload.len().saturating_sub(cursor.offset) != length
            {
                return Err(GridCodecError::InvalidMetadata);
            }
            let metadata: Vec<(u16, RowMetadata)> =
                serde_json::from_slice(&payload[cursor.offset..])
                    .map_err(|_| GridCodecError::InvalidMetadata)?;
            let mut seen = std::collections::HashSet::new();
            for (y, data) in metadata {
                if !seen.insert(y) || !data.validate(usize::from(cols)) {
                    return Err(GridCodecError::InvalidMetadata);
                }
                let row = changed_rows
                    .iter_mut()
                    .find(|row| row.y == y)
                    .ok_or(GridCodecError::InvalidMetadata)?;
                row.metadata = data;
            }
        }
        Ok(Self {
            cols,
            rows,
            cursor_col,
            cursor_row,
            cursor_visible: flags & 1 != 0,
            is_full_snapshot: flags & 2 != 0,
            changed_rows,
            cursor_style: CursorStyle::from_flag_bits(flags & CursorStyle::MASK),
            history,
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
    InvalidMetadata,
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
            Self::InvalidMetadata => f.write_str("invalid or oversized terminal annotations"),
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

/// One encoded run: repeat (u16), scalar, fg and bg (u32 each), style (u16).
const RUN_BYTES: usize = 2 + 4 + 4 + 4 + 2;

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

    /// The most bytes [`Self::append_row`] appends for a row of `cells`
    /// cells: its run count and one run per cell.
    pub const fn max_row_len(cells: usize) -> usize {
        2 + cells * RUN_BYTES
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

    fn varint(&mut self) -> Result<u64, GridCodecError> {
        let mut value = 0_u64;
        for shift in (0..64).step_by(7) {
            let byte = self.u8()?;
            value |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
        }
        Err(GridCodecError::InvalidMetadata)
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

/// LEB128: one byte below 128, at most ten for a `u64`.
fn put_varint(encoded: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        encoded.push((value as u8) | 0x80);
        value >>= 7;
    }
    encoded.push(value as u8);
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
            cursor_style: Default::default(),
            history: None,
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
            cursor_style: Default::default(),
            history: None,
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
            cursor_style: Default::default(),
            history: None,
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
            cursor_style: Default::default(),
            history: None,
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
                cursor_style: Default::default(),
                history: None,
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
            cursor_style: Default::default(),
            history: None,
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
            cursor_style: Default::default(),
            history: None,
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

#[cfg(test)]
mod annotation_tests {
    use super::*;
    fn annotated() -> GridUpdate {
        let mut row = ChangedRow::new(0, vec![GridCell::BLANK; 8]);
        row.metadata.links.push(LinkSpan {
            start: 0,
            end: 4,
            uri: "https://diri.dev".into(),
        });
        row.metadata.graphemes.push((0, "\u{301}".into()));
        GridUpdate {
            cols: 8,
            rows: 1,
            cursor_col: 0,
            cursor_row: 0,
            cursor_visible: true,
            is_full_snapshot: true,
            changed_rows: vec![row],
            cursor_style: Default::default(),
            history: None,
        }
    }
    #[test]
    fn history_positions_round_trip_and_cost_a_few_bytes() {
        for metadata in [false, true] {
            let mut grid = annotated();
            if !metadata {
                grid = grid.without_annotations();
            }
            let plain = grid.encode().unwrap();
            for (position, extra) in [
                (GridHistory { origin: 0, rows: 0 }, 2),
                (
                    GridHistory {
                        origin: 0,
                        rows: 9_976,
                    },
                    3,
                ),
                (
                    GridHistory {
                        origin: 3_000_000,
                        rows: 10_000,
                    },
                    6,
                ),
                (
                    GridHistory {
                        origin: u64::MAX - 1,
                        rows: u64::MAX,
                    },
                    20,
                ),
            ] {
                grid.history = Some(position);
                let bytes = grid.encode().unwrap();
                assert_eq!(bytes.len(), plain.len() + extra, "{position:?}");
                assert_ne!(bytes[8] & HISTORY_FLAG, 0);
                assert_eq!(GridUpdate::decode(&bytes).unwrap(), grid);
                // A peer without grid-history-v1 gets exactly what it got
                // before: no flag, no bytes, the same styled form.
                assert_eq!(grid.encode_without_history().unwrap(), plain);
                assert_eq!(bytes[8] & !HISTORY_FLAG, plain[8]);
                // And the legacy form never carries it.
                let legacy = GridUpdate::decode(&grid.encode_without_styles().unwrap()).unwrap();
                assert_eq!(legacy.history, None);
                // Without styles to drop, legacy and styled are one form,
                // which lets the Engine encode a single stripped copy.
                assert!(!grid.has_style_extensions());
                assert_eq!(grid.encode_without_styles().unwrap(), plain);
            }
        }
    }

    #[test]
    fn a_truncated_history_position_fails_to_decode() {
        let mut grid = annotated().without_annotations();
        grid.history = Some(GridHistory {
            origin: 1 << 40,
            rows: 300,
        });
        let bytes = grid.encode().unwrap();
        for cut in 1..=7 {
            assert!(
                GridUpdate::decode(&bytes[..bytes.len() - cut]).is_err(),
                "{cut}"
            );
        }
        // Eleven continuation bytes is no u64.
        let mut overlong = grid.encode_without_history().unwrap();
        overlong[8] |= HISTORY_FLAG;
        overlong.extend_from_slice(&[0xff; 11]);
        assert!(GridUpdate::decode(&overlong).is_err());
    }

    #[test]
    fn history_positions_measure_what_scrolled_and_what_left() {
        let before = GridHistory {
            origin: 5,
            rows: 100,
        };
        assert_eq!(
            GridHistory {
                origin: 5,
                rows: 103
            }
            .since(before),
            Some((3, 0))
        );
        // At the row limit, three new lines recycle the three oldest.
        assert_eq!(
            GridHistory {
                origin: 8,
                rows: 100
            }
            .since(before),
            Some((3, 3))
        );
        // A clear: nothing scrolled, all of it left.
        assert_eq!(
            GridHistory {
                origin: 105,
                rows: 0
            }
            .since(before),
            Some((0, 100))
        );
        // A new terminal or a reflow does not follow.
        assert_eq!(
            GridHistory {
                origin: 0,
                rows: 200
            }
            .since(before),
            None
        );
        assert_eq!(
            GridHistory {
                origin: 5,
                rows: 90
            }
            .since(before),
            None
        );
    }

    #[test]
    fn coalescing_keeps_the_newest_history_position() {
        let mut older = annotated();
        older.history = Some(GridHistory { origin: 0, rows: 4 });
        let mut newer = annotated().without_annotations();
        newer.is_full_snapshot = false;
        newer.history = Some(GridHistory { origin: 0, rows: 9 });
        older.coalesce(newer.clone());
        assert_eq!(older.history, newer.history);
        newer.history = None;
        older.coalesce(newer);
        assert_eq!(older.history, None);
    }

    #[test]
    fn annotations_round_trip_and_old_wire_stays_readable() {
        let grid = annotated();
        assert_eq!(GridUpdate::decode(&grid.encode().unwrap()).unwrap(), grid);
        let legacy = grid.without_annotations();
        let bytes = legacy.encode().unwrap();
        assert_eq!(bytes[8] & 4, 0);
        assert_eq!(GridUpdate::decode(&bytes).unwrap(), legacy);
    }
    #[test]
    fn malformed_annotation_versions_lengths_and_coordinates_fail() {
        let grid = annotated();
        let mut bytes = grid.encode().unwrap();
        let offset = grid.clone().without_annotations().encode().unwrap().len();
        bytes[offset] = 9;
        assert_eq!(
            GridUpdate::decode(&bytes),
            Err(GridCodecError::InvalidMetadata)
        );
        let mut bytes = grid.encode().unwrap();
        bytes.pop();
        assert_eq!(
            GridUpdate::decode(&bytes),
            Err(GridCodecError::InvalidMetadata)
        );
        let mut bad = grid.clone();
        bad.changed_rows[0].metadata.links[0].end = 9;
        assert_eq!(bad.encode(), Err(GridCodecError::InvalidMetadata));
        let mut bad = grid;
        bad.changed_rows[0].metadata.links[0].uri.push('\x1b');
        assert_eq!(bad.encode(), Err(GridCodecError::InvalidMetadata));
    }
    fn styled() -> GridUpdate {
        let mut grid = annotated();
        grid.cursor_style = CursorStyle {
            shape: CursorShape::Bar,
            steady: true,
        };
        grid.changed_rows[0].metadata.styles = vec![
            StyleSpan {
                start: 1,
                end: 3,
                underline: UnderlineKind::Curly,
                underline_color: Some(TermColor::Rgb(224, 108, 117).packed()),
                overline: false,
            },
            StyleSpan {
                start: 5,
                end: 6,
                underline: UnderlineKind::None,
                underline_color: None,
                overline: true,
            },
        ];
        grid
    }

    #[test]
    fn styles_and_cursor_shape_round_trip() {
        let grid = styled();
        let decoded = GridUpdate::decode(&grid.encode().unwrap()).unwrap();
        assert_eq!(decoded, grid);
        assert_eq!(
            decoded.changed_rows[0]
                .metadata
                .style_at(2)
                .unwrap()
                .underline,
            UnderlineKind::Curly
        );
        assert!(decoded.changed_rows[0].metadata.style_at(3).is_none());
        assert!(
            decoded.changed_rows[0]
                .metadata
                .style_at(5)
                .unwrap()
                .overline
        );
        for shape in [CursorShape::Block, CursorShape::Underline, CursorShape::Bar] {
            for steady in [false, true] {
                let style = CursorStyle { shape, steady };
                assert_eq!(CursorStyle::from_flag_bits(style.flag_bits()), style);
                assert_eq!(style.flag_bits() & 0b111, 0, "bits 0-2 stay theirs");
            }
        }
        assert_eq!(CursorStyle::default().flag_bits(), 0);
    }

    #[test]
    fn legacy_encoding_is_the_pre_styles_wire() {
        let grid = styled();
        let mut stripped = grid.clone();
        stripped.strip_styles();
        assert!(grid.has_style_extensions());
        assert!(!stripped.has_style_extensions());
        let legacy = grid.encode_without_styles().unwrap();
        assert_eq!(legacy, stripped.encode().unwrap());
        // No `styles` key: an older decoder's `deny_unknown_fields` accepts it.
        assert!(!String::from_utf8_lossy(&legacy).contains("styles"));
        assert_eq!(legacy[8] & CursorStyle::MASK, 0);

        // A row whose only annotation is a style span has none in legacy form.
        let mut only = GridUpdate {
            cols: 8,
            rows: 1,
            cursor_col: 0,
            cursor_row: 0,
            cursor_visible: true,
            is_full_snapshot: true,
            changed_rows: vec![ChangedRow::new(0, vec![GridCell::BLANK; 8])],
            cursor_style: CursorStyle::default(),
            history: None,
        };
        let plain = only.encode().unwrap();
        only.changed_rows[0].metadata.styles = styled().changed_rows[0].metadata.styles.clone();
        assert_eq!(only.encode_without_styles().unwrap(), plain);
        assert_eq!(plain[8] & 4, 0);
    }

    #[test]
    fn invalid_style_spans_fail() {
        for bad in [
            StyleSpan {
                start: 2,
                end: 2,
                underline: UnderlineKind::Double,
                ..StyleSpan::default()
            },
            StyleSpan {
                start: 0,
                end: 9,
                underline: UnderlineKind::Double,
                ..StyleSpan::default()
            },
            StyleSpan {
                start: 0,
                end: 1,
                ..StyleSpan::default()
            },
            StyleSpan {
                start: 0,
                end: 1,
                underline_color: Some(1),
                ..StyleSpan::default()
            },
        ] {
            let mut grid = annotated();
            grid.changed_rows[0].metadata.styles = vec![bad];
            assert_eq!(grid.encode(), Err(GridCodecError::InvalidMetadata));
        }
        let mut overlapping = styled();
        overlapping.changed_rows[0].metadata.styles[1].start = 2;
        assert_eq!(overlapping.encode(), Err(GridCodecError::InvalidMetadata));
    }

    #[test]
    fn coalescing_carries_the_newest_cursor_style() {
        let mut first = annotated();
        let mut second = styled();
        second.is_full_snapshot = false;
        first.coalesce(second);
        assert_eq!(first.cursor_style.shape, CursorShape::Bar);
    }

    #[test]
    fn coalescing_replaces_annotations_even_when_text_is_unchanged() {
        let mut first = annotated();
        let mut second = first.clone();
        second.is_full_snapshot = false;
        second.changed_rows[0].metadata = RowMetadata::default();
        first.coalesce(second);
        assert!(first.changed_rows[0].metadata.is_empty());
    }
}
