//! The boundary: a v2 core's state as a `HeadlessScreen`, so every existing
//! snapshot, diff, scrollback and checkpoint encoder (and the benchmark's
//! golden digests) can read it unchanged. The production adapter (migration
//! step 4) encodes the wire types from pages directly; this copy is for
//! verification and is not on any hot path.

#[cfg(not(feature = "core-v2-screen"))]
use alacritty_terminal::grid::Dimensions;
#[cfg(not(feature = "core-v2-screen"))]
use alacritty_terminal::index::{Column, Line, Point};
#[cfg(not(feature = "core-v2-screen"))]
use alacritty_terminal::term::cell::{Cell as GridCell, Hyperlink};

use super::Core;
use crate::HeadlessScreen;

#[cfg(feature = "core-v2-screen")]
impl HeadlessScreen {
    /// A screen holding `core`'s grid, history and cursor, cell for cell.
    pub fn from_core_v2(core: &Core) -> HeadlessScreen {
        let mut screen = HeadlessScreen::new(core.cols(), core.lines());
        screen.term.copy_from(core);
        screen.fingerprints_track_cells = false;
        screen.last_grid_cols = 0;
        screen.last_grid_rows = 0;
        screen
    }
}

#[cfg(not(feature = "core-v2-screen"))]
impl HeadlessScreen {
    /// A screen holding `core`'s grid, history and cursor, cell for cell.
    pub fn from_core_v2(core: &Core) -> HeadlessScreen {
        let (lines, cols) = (core.lines(), core.cols());
        let mut screen = HeadlessScreen::new(cols, lines);
        let history = core.history_size();
        let grid = screen.term.grid_mut();
        if history > 0 {
            grid.scroll_up(&(Line(0)..Line(lines as i32)), history);
        }
        let kept = grid.history_size();
        assert_eq!(kept, history, "the screen keeps the core's history");
        for line in -(history as i32)..lines as i32 {
            for col in 0..cols {
                let cell = core.cell(line, col);
                let style = core.style_of(line, cell);
                let flags = super::check::flags_of(core, line, col, cell, &style);
                let mut out = GridCell {
                    c: cell.ch(),
                    fg: style.fg,
                    bg: style.bg,
                    flags,
                    extra: None,
                };
                out.set_underline_color(style.underline);
                for z in core.zerowidth(line, col) {
                    out.push_zerowidth(z);
                }
                if let Some(link) = core.link_at(line, col) {
                    out.set_hyperlink(Some(Hyperlink::new(link.id, link.uri)));
                }
                grid[Line(line)][Column(col)] = out;
            }
        }
        grid.cursor.point = Point::new(Line(core.line as i32), Column(core.col));
        grid.cursor.input_needs_wrap = core.wrap_pending;
        screen.fingerprints_track_cells = false;
        screen.last_grid_cols = 0;
        screen.last_grid_rows = 0;
        screen
    }
}
