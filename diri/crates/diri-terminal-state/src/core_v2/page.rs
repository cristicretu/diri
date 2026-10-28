//! Pages: fixed-size blocks of rows holding both the screen and history.
//!
//! The screen is the newest `lines` rows of the page list. A linefeed at the
//! bottom of a full-screen scroll region appends one row (no cell is
//! touched) and the oldest screen row becomes history by definition, so
//! scrolling into history costs nothing. Whole pages past the history limit
//! are recycled as the next tail page, so steady-state output allocates
//! nothing.

use std::collections::{HashMap, VecDeque};

use super::cell::{Cell, Style};

/// Per-row facts. Cells at or past `occ` were never written since the row
/// was appended or wholly erased: they read as blanks of style `fill`, and
/// whatever bytes they hold are garbage from a previous use of the slot.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct RowMeta {
    pub occ: u16,
    pub fill: u16,
    pub wrapped: bool,
    pub dirty: bool,
}

#[derive(Default)]
pub struct StyleTable {
    table: Vec<Style>,
    index: HashMap<Style, u16>,
}

impl StyleTable {
    #[inline]
    pub fn intern(&mut self, style: &Style) -> u16 {
        if *style == Style::default() {
            return 0;
        }
        if self.table.is_empty() {
            self.table.push(Style::default());
        }
        if let Some(&id) = self.index.get(style) {
            return id;
        }
        // Callers intern only for a cell or row fill they write, so a page
        // holds at most rows * (cols + 1) styles, far below 65,535.
        let id = self.table.len() as u16;
        self.table.push(*style);
        self.index.insert(*style, id);
        id
    }

    #[inline]
    pub fn get(&self, id: u16) -> Style {
        if id == 0 {
            Style::default()
        } else {
            self.table[id as usize]
        }
    }

    pub fn len(&self) -> usize {
        self.table.len()
    }

    pub fn is_empty(&self) -> bool {
        self.table.is_empty()
    }

    fn clear(&mut self) {
        self.table.clear();
        self.index.clear();
    }
}

pub struct Page {
    /// Changes whenever the page is (re)initialised, so a pen can cache
    /// its interned id per page.
    pub serial: u64,
    pub len: usize,
    pub cells: Box<[Cell]>,
    pub meta: Box<[RowMeta]>,
    pub styles: StyleTable,
    /// Extra codepoints of a cluster, keyed by `row * cols + col`. Only
    /// meaningful while the cell has the GRAPHEME bit.
    pub graphemes: HashMap<u32, Vec<char>>,
}

impl Page {
    fn new(rows: usize, cols: usize, serial: u64) -> Page {
        Page {
            serial,
            len: 0,
            cells: vec![Cell::default(); rows * cols].into_boxed_slice(),
            meta: vec![RowMeta::default(); rows].into_boxed_slice(),
            styles: StyleTable::default(),
            graphemes: HashMap::new(),
        }
    }

    fn recycle(&mut self, serial: u64) {
        // Cells and metadata are not cleared: rows are appended with
        // occ = 0, so stale cells are never read.
        self.serial = serial;
        self.len = 0;
        self.styles.clear();
        self.graphemes.clear();
    }
}

pub struct Pages {
    pub cols: usize,
    pub lines: usize,
    shift: u32,
    pub history_limit: usize,
    pages: VecDeque<Page>,
    /// Absolute index of the first row of `pages[0]`.
    base: u64,
    /// Absolute index one past the newest row.
    total: u64,
    spare: Option<Page>,
    next_serial: u64,
}

/// Rows per page: a power of two near 64 KiB of cells, at least 16.
fn rows_shift(cols: usize) -> u32 {
    let rows = (65536 / (cols.max(1) * 8)).max(16);
    usize::BITS - 1 - rows.leading_zeros()
}

impl Pages {
    pub fn new(lines: usize, cols: usize, history_limit: usize) -> Pages {
        let mut pages = Pages {
            cols,
            lines,
            shift: rows_shift(cols),
            history_limit,
            pages: VecDeque::new(),
            base: 0,
            total: 0,
            spare: None,
            next_serial: 1,
        };
        for _ in 0..lines {
            pages.push_row(&Style::default());
        }
        pages
    }

    #[inline(always)]
    pub fn rows_per_page(&self) -> usize {
        1 << self.shift
    }

    /// Page index and row within it of absolute row `abs`.
    #[inline(always)]
    fn locate_abs(&self, abs: u64) -> (usize, usize) {
        let off = (abs - self.base) as usize;
        (off >> self.shift, off & ((1 << self.shift) - 1))
    }

    /// Page index and row of screen line `y`.
    #[inline(always)]
    pub fn locate(&self, y: usize) -> (usize, usize) {
        self.locate_abs(self.total - self.lines as u64 + y as u64)
    }

    #[inline(always)]
    pub fn page(&self, index: usize) -> &Page {
        &self.pages[index]
    }

    #[inline(always)]
    pub fn page_mut(&mut self, index: usize) -> &mut Page {
        &mut self.pages[index]
    }

    pub fn history_size(&self) -> usize {
        let behind = (self.total - self.base) as usize - self.lines;
        behind.min(self.history_limit)
    }

    /// Page and row of line `line`, where negative lines are history
    /// (`-1` is the newest history row), as alacritty numbers them.
    pub fn locate_line(&self, line: i32) -> (usize, usize) {
        let abs = (self.total as i64 - self.lines as i64 + line as i64) as u64;
        self.locate_abs(abs)
    }

    /// Appends a blank row whose erase fill is `fill`; the oldest screen
    /// row becomes history.
    #[inline]
    pub fn push_row(&mut self, fill: &Style) {
        let rows = self.rows_per_page();
        let need_page = self.pages.back().is_none_or(|page| page.len == rows);
        if need_page {
            let serial = self.next_serial;
            self.next_serial += 1;
            let page = match self.spare.take() {
                Some(mut page) => {
                    page.recycle(serial);
                    page
                }
                None => Page::new(rows, self.cols, serial),
            };
            self.pages.push_back(page);
        }
        let page = self.pages.back_mut().expect("a tail page");
        let fill = page.styles.intern(fill);
        let row = page.len;
        page.meta[row] = RowMeta {
            occ: 0,
            fill,
            wrapped: false,
            dirty: true,
        };
        page.len += 1;
        self.total += 1;
        self.trim();
    }

    /// Recycles whole pages that hold only rows past the history limit.
    #[inline]
    fn trim(&mut self) {
        let keep = (self.lines + self.history_limit) as u64;
        let rows = self.rows_per_page() as u64;
        while self.pages.len() > 1 && self.total - (self.base + rows) >= keep {
            let page = self.pages.pop_front().expect("more than one page");
            self.base += rows;
            if self.spare.is_none() {
                self.spare = Some(page);
            }
        }
    }

    /// Heap bytes held by pages (cells and row metadata), for footprint.
    pub fn cell_bytes(&self) -> usize {
        let per_page = self.rows_per_page()
            * (self.cols * std::mem::size_of::<Cell>() + std::mem::size_of::<RowMeta>());
        (self.pages.len() + usize::from(self.spare.is_some())) * per_page
    }

    pub fn page_count(&self) -> usize {
        self.pages.len()
    }
}
