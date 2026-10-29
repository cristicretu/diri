//! Cell-for-cell comparison with the current core, for the bench's state
//! check and the differential tests.

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::Term;
use alacritty_terminal::term::cell::Flags;

use alacritty_terminal::term::TermMode;

use super::Core;
use super::cell::{Cell, LEADING, MARKS, MARKS_SHIFT, SPACER, Style, WIDE, WRAP};

/// The current core's `Flags` of a v2 cell: its style flags, width bits,
/// soft wrap and shell marks.
pub fn flags_of(core: &Core, line: i32, col: usize, cell: Cell, style: &Style) -> Flags {
    let mut flags = Flags::from_bits_retain(style.flags);
    flags.set(Flags::WIDE_CHAR, cell.has(WIDE));
    flags.set(Flags::WIDE_CHAR_SPACER, cell.has(SPACER));
    flags.set(Flags::LEADING_WIDE_CHAR_SPACER, cell.has(LEADING));
    flags.set(
        Flags::WRAPLINE,
        cell.has(WRAP) || (col + 1 == core.cols() && core.wrapped(line)),
    );
    let marks = (cell.0 & MARKS) >> MARKS_SHIFT;
    for (bit, flag) in [
        Flags::PROMPT_START,
        Flags::INPUT_START,
        Flags::OUTPUT_START,
        Flags::COMMAND_SUCCEEDED,
        Flags::COMMAND_FAILED,
    ]
    .into_iter()
    .enumerate()
    {
        flags.set(flag, marks & (1 << bit) != 0);
    }
    flags
}

/// First difference between the v2 core and the reference, if any.
pub fn compare<T>(core: &Core, reference: &Term<T>) -> Result<(), String> {
    if core.unsupported != 0 {
        return Err(format!("{} unsupported sequences", core.unsupported));
    }
    let modes = !(TermMode::KITTY_KEYBOARD_PROTOCOL | TermMode::VI);
    if core.mode & modes != *reference.mode() & modes {
        return Err(format!(
            "modes {:?}, reference {:?}",
            core.mode & modes,
            *reference.mode() & modes
        ));
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
            let flags = flags_of(core, line, col, got, &style);
            let link = core.link_at(line, col);
            let want_link = want.hyperlink();
            let same_link = match (&link, &want_link) {
                (None, None) => true,
                // An explicit id must match exactly. Without one the current
                // core generates a process-unique `<n>_alacritty` id, which
                // v2 does not imitate: the reference must then carry a
                // generated id, not an explicit one.
                (Some(got), Some(want)) => {
                    got.uri == want.uri()
                        && match &got.id {
                            Some(id) => id == want.id(),
                            None => want.id().ends_with("_alacritty"),
                        }
                }
                _ => false,
            };
            let same = same_link
                && got.ch() == want.c
                && flags == want.flags
                && style.fg == want.fg
                && style.bg == want.bg
                && style.underline == want.underline_color()
                && core.zerowidth(line, col).as_slice() == want.zerowidth().unwrap_or(&[]);
            if !same {
                return Err(format!(
                    "line {line} col {col}: got {:?} {:?} {:?} {:?} zw {:?} link {link:?}, want {:?} {:?} {:?} {:?} zw {:?} link {want_link:?}",
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
