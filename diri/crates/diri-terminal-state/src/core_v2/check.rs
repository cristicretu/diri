//! Cell-for-cell comparison with the current core, for the bench's state
//! check and the differential tests.

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::Term;
use alacritty_terminal::term::cell::Flags;

use super::Core;
use super::cell::{LEADING, SPACER, WIDE};

/// First difference between the v2 core and the reference, if any.
pub fn compare<T>(core: &Core, reference: &Term<T>) -> Result<(), String> {
    if core.unsupported != 0 {
        return Err(format!("{} unsupported sequences", core.unsupported));
    }
    let grid = reference.grid();
    let history = grid.history_size();
    if core.history_size() != history {
        return Err(format!(
            "history {} rows, reference {history}",
            core.history_size()
        ));
    }
    let cursor = grid.cursor.point;
    if (core.line, core.col, core.wrap_pending)
        != (
            cursor.line.0 as usize,
            cursor.column.0,
            grid.cursor.input_needs_wrap,
        )
    {
        return Err(format!(
            "cursor {:?}, reference {:?}",
            (core.line, core.col, core.wrap_pending),
            (cursor, grid.cursor.input_needs_wrap)
        ));
    }
    for line in -(history as i32)..core.lines() as i32 {
        let row = &grid[Line(line)];
        for col in 0..core.cols() {
            let want = &row[Column(col)];
            let got = core.cell(line, col);
            let style = core.style_of(line, got);
            let mut flags = Flags::from_bits_retain(style.flags);
            flags.set(Flags::WIDE_CHAR, got.has(WIDE));
            flags.set(Flags::WIDE_CHAR_SPACER, got.has(SPACER));
            flags.set(Flags::LEADING_WIDE_CHAR_SPACER, got.has(LEADING));
            flags.set(
                Flags::WRAPLINE,
                col + 1 == core.cols() && core.wrapped(line),
            );
            let same = got.ch() == want.c
                && flags == want.flags
                && style.fg == want.fg
                && style.bg == want.bg
                && style.underline == want.underline_color()
                && core.zerowidth(line, col) == want.zerowidth().unwrap_or(&[]);
            if !same {
                return Err(format!(
                    "line {line} col {col}: got {:?} {:?} {:?} {:?} zw {:?}, want {:?} {:?} {:?} {:?} zw {:?}",
                    got.ch(),
                    flags,
                    style.fg,
                    style.bg,
                    core.zerowidth(line, col),
                    want.c,
                    want.flags,
                    want.fg,
                    want.bg,
                    want.zerowidth(),
                ));
            }
        }
    }
    Ok(())
}
