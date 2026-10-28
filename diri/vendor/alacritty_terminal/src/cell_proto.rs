//! Step-1 prototype, not shipped: the print path (ASCII and non-ASCII row
//! segments, wrap, linefeed with scrollback, tabs, SGR) written once over
//! two cell layouts, the 24-byte `Cell` and an 8-byte packed cell with a
//! deduplicated style table, driven by the real parser and the real
//! `Grid`/`Storage`/`Row` (dense history). Both run the same code, so the
//! difference is the cell layout.

use std::collections::HashMap;
use std::ops::Range;

use unicode_width::UnicodeWidthChar;

use crate::grid::{Dimensions, Grid, GridCell};
use crate::index::{Column, Line};
use crate::term::cell::{Cell, Flags};
use crate::vte::ansi::{Attr, Color, Handler, NamedColor, Processor, StdSyncHandler};

/// Style of a packed cell: everything but the character and the cell's
/// positional flags.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Style {
    pub fg: Color,
    pub bg: Color,
    pub flags: Flags,
}

fn color_key(color: Color) -> u32 {
    match color {
        Color::Named(named) => 1 << 24 | named as u32,
        Color::Spec(rgb) => 2 << 24 | (rgb.r as u32) << 16 | (rgb.g as u32) << 8 | rgb.b as u32,
        Color::Indexed(index) => 3 << 24 | index as u32,
    }
}

impl std::hash::Hash for Style {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        (color_key(self.fg), color_key(self.bg), self.flags.bits()).hash(state);
    }
}

impl Default for Style {
    fn default() -> Self {
        Style {
            fg: Color::Named(NamedColor::Foreground),
            bg: Color::Named(NamedColor::Background),
            flags: Flags::empty(),
        }
    }
}

#[derive(Default)]
pub struct Styles {
    table: Vec<Style>,
    index: HashMap<Style, u32>,
}

impl Styles {
    fn intern(&mut self, style: Style) -> u32 {
        if style == Style::default() {
            return 0;
        }
        if self.table.is_empty() {
            self.table.push(Style::default());
        }
        *self.index.entry(style).or_insert_with(|| {
            self.table.push(style);
            (self.table.len() - 1) as u32
        })
    }
}

/// 8 bytes: codepoint (21 bits), positional flags (bits 21..32), style id
/// (bits 32..64).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Packed(pub u64);

const CP: u64 = 0x1F_FFFF;
const P_WIDE: u64 = 1 << 21;
const P_SPACER: u64 = 1 << 22;
const P_LEADING: u64 = 1 << 23;
const P_WRAP: u64 = 1 << 24;
const P_MARKS: u64 = 0x1F << 25;

impl Default for Packed {
    fn default() -> Self {
        Packed(' ' as u64)
    }
}

static NO_FLAGS: Flags = Flags::empty();

impl GridCell for Packed {
    fn is_empty(&self) -> bool {
        let cp = self.0 & CP;
        (cp == ' ' as u64 || cp == '\t' as u64)
            && self.0 >> 32 == 0
            && self.0 & (P_WRAP | P_SPACER | P_LEADING) == 0
    }

    fn reset(&mut self, template: &Self) {
        *self = *template;
    }

    fn flags(&self) -> &Flags {
        &NO_FLAGS
    }

    fn flags_mut(&mut self) -> &mut Flags {
        unimplemented!("prototype")
    }
}

pub trait ProtoCell: GridCell + Default + Clone + PartialEq + Sized {
    type Pen: Default;
    fn erase(pen: &Self::Pen, styles: &mut Styles) -> Self;
    fn sgr(pen: &mut Self::Pen, attr: Attr, styles: &mut Styles);
    fn write_ascii(cells: &mut [Self], text: &[u8], pen: &Self::Pen) -> usize;
    /// Writes `c` taking `cells.len()` (1 or 2) columns; false when a wide
    /// character or spacer is in the way.
    fn write_char(cells: &mut [Self], c: char, pen: &Self::Pen) -> bool;
    /// Writes into cells at or past the row's occupancy: blanks with no
    /// flags or extra storage, so nothing is read or dropped.
    fn write_fresh(cells: &mut [Self], c: char, pen: &Self::Pen);
    fn set_wrap(&mut self);
    fn set_leading(&mut self);
    fn tab(&mut self);
    fn ch(&self) -> char;
    fn scroll_up(grid: &mut Grid<Self>, region: Range<Line>);
}

impl ProtoCell for Cell {
    type Pen = Cell;

    fn erase(pen: &Cell, _: &mut Styles) -> Cell {
        Cell {
            bg: pen.bg,
            ..Cell::default()
        }
    }

    fn sgr(pen: &mut Cell, attr: Attr, _: &mut Styles) {
        match attr {
            Attr::Foreground(c) => pen.fg = c,
            Attr::Background(c) => pen.bg = c,
            Attr::Reset => {
                pen.fg = Color::Named(NamedColor::Foreground);
                pen.bg = Color::Named(NamedColor::Background);
                pen.flags = Flags::empty();
            }
            Attr::Bold => pen.flags.insert(Flags::BOLD),
            Attr::Italic => pen.flags.insert(Flags::ITALIC),
            Attr::Reverse => pen.flags.insert(Flags::INVERSE),
            Attr::Underline => pen.flags.insert(Flags::UNDERLINE),
            _ => {}
        }
    }

    #[inline(always)]
    fn write_ascii(cells: &mut [Cell], text: &[u8], pen: &Cell) -> usize {
        let (fg, bg, flags) = (pen.fg, pen.bg, pen.flags);
        let extra = pen.extra.clone();
        let mut written = 0;
        for (cell, &byte) in cells.iter_mut().zip(text) {
            if cell
                .flags
                .intersects(Flags::WIDE_CHAR | Flags::WIDE_CHAR_SPACER)
            {
                break;
            }
            cell.c = char::from(byte);
            cell.fg = fg;
            cell.bg = bg;
            cell.flags = flags | (cell.flags & Flags::SHELL_MARKS);
            if extra.is_some() || cell.extra.is_some() {
                cell.extra = extra.clone();
            }
            written += 1;
        }
        written
    }

    #[inline(always)]
    fn write_char(cells: &mut [Cell], c: char, pen: &Cell) -> bool {
        let wide = Flags::WIDE_CHAR | Flags::WIDE_CHAR_SPACER;
        if cells.iter().any(|cell| cell.flags.intersects(wide)) {
            return false;
        }
        let (fg, bg, flags) = (pen.fg, pen.bg, pen.flags);
        let two = cells.len() == 2;
        let cell = &mut cells[0];
        cell.c = c;
        cell.fg = fg;
        cell.bg = bg;
        cell.extra = pen.extra.clone();
        if !two {
            cell.flags = flags | (cell.flags & Flags::SHELL_MARKS);
        } else {
            cell.flags = flags | Flags::WIDE_CHAR | (cell.flags & Flags::SHELL_MARKS);
            let cell = &mut cells[1];
            cell.c = ' ';
            cell.fg = fg;
            cell.bg = bg;
            cell.flags = flags | Flags::WIDE_CHAR_SPACER | (cell.flags & Flags::SHELL_MARKS);
            cell.extra = pen.extra.clone();
        }
        true
    }

    #[inline(always)]
    fn write_fresh(cells: &mut [Cell], c: char, pen: &Cell) {
        let cell = Cell {
            c,
            fg: pen.fg,
            bg: pen.bg,
            flags: pen.flags
                | if cells.len() == 2 {
                    Flags::WIDE_CHAR
                } else {
                    Flags::empty()
                },
            extra: pen.extra.clone(),
        };
        unsafe { std::ptr::write(&mut cells[0], cell) };
        if cells.len() == 2 {
            let spacer = Cell {
                c: ' ',
                fg: pen.fg,
                bg: pen.bg,
                flags: pen.flags | Flags::WIDE_CHAR_SPACER,
                extra: pen.extra.clone(),
            };
            unsafe { std::ptr::write(&mut cells[1], spacer) };
        }
    }

    fn set_wrap(&mut self) {
        self.flags.insert(Flags::WRAPLINE);
    }

    fn set_leading(&mut self) {
        self.flags.insert(Flags::LEADING_WIDE_CHAR_SPACER);
    }

    fn tab(&mut self) {
        if self.c == ' ' {
            self.c = '\t';
        }
    }

    fn ch(&self) -> char {
        self.c
    }

    fn scroll_up(grid: &mut Grid<Cell>, region: Range<Line>) {
        grid.scroll_up::<Color>(&region, 1);
    }
}

impl ProtoCell for Packed {
    /// The style id, pre-shifted.
    type Pen = (u64, Style);

    fn erase(pen: &(u64, Style), styles: &mut Styles) -> Packed {
        let style = Style {
            bg: pen.1.bg,
            ..Style::default()
        };
        Packed(' ' as u64 | (styles.intern(style) as u64) << 32)
    }

    fn sgr(pen: &mut (u64, Style), attr: Attr, styles: &mut Styles) {
        let mut style = pen.1;
        match attr {
            Attr::Foreground(c) => style.fg = c,
            Attr::Background(c) => style.bg = c,
            Attr::Reset => style = Style::default(),
            Attr::Bold => style.flags.insert(Flags::BOLD),
            Attr::Italic => style.flags.insert(Flags::ITALIC),
            Attr::Reverse => style.flags.insert(Flags::INVERSE),
            Attr::Underline => style.flags.insert(Flags::UNDERLINE),
            _ => {}
        }
        if style != pen.1 {
            *pen = ((styles.intern(style) as u64) << 32, style);
        }
    }

    #[inline(always)]
    fn write_ascii(cells: &mut [Packed], text: &[u8], pen: &(u64, Style)) -> usize {
        let style = pen.0;
        let mut written = 0;
        for (cell, &byte) in cells.iter_mut().zip(text) {
            let old = cell.0;
            if old & (P_WIDE | P_SPACER) != 0 {
                break;
            }
            cell.0 = byte as u64 | style | (old & P_MARKS);
            written += 1;
        }
        written
    }

    #[inline(always)]
    fn write_char(cells: &mut [Packed], c: char, pen: &(u64, Style)) -> bool {
        if cells.iter().any(|cell| cell.0 & (P_WIDE | P_SPACER) != 0) {
            return false;
        }
        let style = pen.0;
        if cells.len() == 1 {
            cells[0].0 = c as u64 | style | (cells[0].0 & P_MARKS);
        } else {
            cells[0].0 = c as u64 | style | P_WIDE | (cells[0].0 & P_MARKS);
            cells[1].0 = ' ' as u64 | style | P_SPACER | (cells[1].0 & P_MARKS);
        }
        true
    }

    #[inline(always)]
    fn write_fresh(cells: &mut [Packed], c: char, pen: &(u64, Style)) {
        if cells.len() == 1 {
            cells[0].0 = c as u64 | pen.0;
        } else {
            cells[0].0 = c as u64 | pen.0 | P_WIDE;
            cells[1].0 = ' ' as u64 | pen.0 | P_SPACER;
        }
    }

    fn set_wrap(&mut self) {
        self.0 |= P_WRAP;
    }

    fn set_leading(&mut self) {
        self.0 |= P_LEADING;
    }

    fn tab(&mut self) {
        if self.0 & CP == ' ' as u64 {
            self.0 = (self.0 & !CP) | '\t' as u64;
        }
    }

    fn ch(&self) -> char {
        char::from_u32((self.0 & CP) as u32).unwrap_or('?')
    }

    fn scroll_up(grid: &mut Grid<Packed>, region: Range<Line>) {
        grid.scroll_up::<Packed>(&region, 1);
    }
}

pub struct MiniTerm<C: ProtoCell> {
    pub grid: Grid<C>,
    pen: C::Pen,
    pub styles: Styles,
}

impl<C: ProtoCell> MiniTerm<C> {
    pub fn new(lines: usize, columns: usize, history_bytes: usize) -> Self {
        let history = history_bytes / (columns * std::mem::size_of::<C>());
        MiniTerm {
            grid: Grid::new(lines, columns, history),
            pen: C::Pen::default(),
            styles: Styles::default(),
        }
    }

    fn wrapline(&mut self) {
        let point = self.grid.cursor.point;
        self.grid[point].set_wrap();
        self.linefeed();
        self.grid.cursor.point.column = Column(0);
        self.grid.cursor.input_needs_wrap = false;
    }

    /// The visible screen and history as text, for comparing layouts.
    pub fn text(&self) -> String {
        let mut out = String::new();
        let top = -(self.grid.history_size() as i32);
        for line in top..self.grid.screen_lines() as i32 {
            let row = &self.grid[Line(line)];
            for column in 0..self.grid.columns() {
                out.push(row[Column(column)].ch());
            }
            out.push('\n');
        }
        out
    }
}

impl<C: ProtoCell> Handler for MiniTerm<C> {
    fn input(&mut self, c: char) {
        let width = c.width().unwrap_or(1).clamp(1, 2);
        let columns = self.grid.columns();
        if self.grid.cursor.input_needs_wrap {
            self.wrapline();
        }
        let mut point = self.grid.cursor.point;
        if width == 2 && point.column.0 + 2 > columns {
            self.grid[point].set_leading();
            self.wrapline();
            point = self.grid.cursor.point;
        }
        let row = &mut self.grid[point.line];
        if !C::write_char(&mut row[point.column..point.column + width], c, &self.pen) {
            for cell in &mut row[point.column..point.column + width] {
                *cell = C::default();
            }
            C::write_char(&mut row[point.column..point.column + width], c, &self.pen);
        }
        let end = point.column.0 + width;
        if end >= columns {
            self.grid.cursor.point.column = Column(columns - 1);
            self.grid.cursor.input_needs_wrap = true;
        } else {
            self.grid.cursor.point.column = Column(end);
        }
    }

    #[inline(never)]
    fn input_ascii(&mut self, text: &str) {
        let bytes = text.as_bytes();
        let mut index = 0;
        while index < bytes.len() {
            if self.grid.cursor.input_needs_wrap {
                self.input(char::from(bytes[index]));
                index += 1;
                continue;
            }
            let point = self.grid.cursor.point;
            let columns = self.grid.columns();
            let count = (columns - point.column.0).min(bytes.len() - index);
            let text = &bytes[index..index + count];
            let pen = &self.pen;
            let count = self.grid[point.line]
                .write_from(point.column.0, |cells| C::write_ascii(cells, text, pen));
            if count == 0 {
                self.input(char::from(bytes[index]));
                index += 1;
                continue;
            }
            let end = point.column + count;
            index += count;
            if end.0 < columns {
                self.grid.cursor.point.column = end;
            } else {
                self.grid.cursor.point.column = Column(columns - 1);
                self.grid.cursor.input_needs_wrap = true;
            }
        }
    }

    #[inline(never)]
    fn input_text(&mut self, text: &str) {
        let bytes = text.as_bytes();
        let mut index = 0;
        while index < bytes.len() {
            let first = text[index..].chars().next().unwrap();
            if self.grid.cursor.input_needs_wrap {
                self.input(first);
                index += first.len_utf8();
                continue;
            }
            let columns = self.grid.columns();
            let point = self.grid.cursor.point;
            let start_column = point.column.0;
            let row = &mut self.grid[point.line];
            let occ = row.occ.saturating_sub(start_column);
            let pen = &self.pen;
            let mut at = index;
            let written = row.write_from(start_column, |cells| {
                let mut column = 0;
                while at < bytes.len() {
                    let b0 = bytes[at];
                    let (c, len) = if b0 & 0xF0 == 0xE0 {
                        // Validated by the parser.
                        let c = (b0 as u32 & 0x0F) << 12
                            | (bytes[at + 1] as u32 & 0x3F) << 6
                            | (bytes[at + 2] as u32 & 0x3F);
                        (unsafe { char::from_u32_unchecked(c) }, 3)
                    } else {
                        let c = text[at..].chars().next().unwrap();
                        (c, c.len_utf8())
                    };
                    let info = crate::term::grapheme::Info::of(c);
                    if crate::term::grapheme::may_join(c, info) {
                        break;
                    }
                    let width = match info.width() {
                        w @ (1 | 2) => w,
                        _ => break,
                    };
                    let Some(target) = cells.get_mut(column..column + width) else {
                        break;
                    };
                    if column >= occ {
                        C::write_fresh(target, c, pen);
                    } else if !C::write_char(target, c, pen) {
                        break;
                    }
                    at += len;
                    column += width;
                }
                column
            });
            if written == 0 {
                self.input(first);
                index += first.len_utf8();
                continue;
            }
            index = at;
            let column = start_column + written;
            if column == columns {
                self.grid.cursor.point.column = Column(columns - 1);
                self.grid.cursor.input_needs_wrap = true;
            } else {
                self.grid.cursor.point.column = Column(column);
            }
        }
    }

    fn put_tab(&mut self, mut count: u16) {
        if self.grid.cursor.input_needs_wrap {
            self.wrapline();
            return;
        }
        let columns = self.grid.columns();
        while self.grid.cursor.point.column.0 < columns && count != 0 {
            count -= 1;
            self.grid.cursor_cell().tab();
            loop {
                if self.grid.cursor.point.column.0 + 1 == columns {
                    break;
                }
                self.grid.cursor.point.column += 1;
                if self.grid.cursor.point.column.0 % 8 == 0 {
                    break;
                }
            }
        }
    }

    fn carriage_return(&mut self) {
        self.grid.cursor.point.column = Column(0);
        self.grid.cursor.input_needs_wrap = false;
    }

    fn linefeed(&mut self) {
        let next: Line = self.grid.cursor.point.line + 1i32;
        let lines = self.grid.screen_lines() as i32;
        if next.0 == lines {
            C::scroll_up(&mut self.grid, Line(0)..Line(lines));
        } else {
            self.grid.cursor.point.line = next;
        }
    }

    fn terminal_attribute(&mut self, attr: Attr) {
        C::sgr(&mut self.pen, attr, &mut self.styles);
        self.grid.cursor.template = C::erase(&self.pen, &mut self.styles);
    }
}

/// Feeds `payload` in `read`-byte slices `reps` times.
pub fn feed<C: ProtoCell>(
    term: &mut MiniTerm<C>,
    parser: &mut Processor<StdSyncHandler>,
    payload: &[u8],
    read: usize,
    reps: usize,
) {
    for _ in 0..reps {
        for chunk in payload.chunks(read) {
            parser.advance(term, chunk);
        }
    }
}
