//! What text a selection puts on the clipboard, and what the Engine's
//! `read_screen` / `read_scrollback` return, for the same program output.
//!
//! Every case runs real bytes through the shared parser, the grid wire and
//! the client's history cache, then selects absolute cells the way a drag,
//! a double or triple click, or an Option-Shift block drag does. Pointer
//! selections run between cell boundaries (`0..=cols`). The expected text
//! follows xterm and Ghostty: soft wraps join, hard newlines stay, padding is
//! trimmed per line but not inside it, clusters copy once, a tab copies as
//! the blank it left, and a link copies its text.

use diri_proto::terminal_input::{paste, paste_needs_confirmation};
use diri_term::buffer::GridBuffer;
use diri_term::scrollback::ScrollbackViewport;
use diri_term::selection::{SelectionPoint, TerminalSelection};
use diri_terminal_state::HeadlessScreen;

/// One program's output as the app holds it: live grid plus fetched history.
struct Pane {
    screen: HeadlessScreen,
    buffer: GridBuffer,
    viewport: ScrollbackViewport,
    rows: usize,
    live_start: i64,
}

impl Pane {
    fn new(cols: usize, rows: usize, bytes: &[u8]) -> Self {
        let mut screen = HeadlessScreen::new(cols, rows);
        screen.feed(bytes);
        let mut pane = Self {
            screen,
            buffer: GridBuffer::new(cols as u16, rows as u16),
            viewport: ScrollbackViewport::default(),
            rows,
            live_start: 0,
        };
        pane.sync();
        pane
    }

    fn feed(&mut self, bytes: &[u8]) {
        self.screen.feed(bytes);
        self.sync();
    }

    /// Republishes the live grid and fetches every history row.
    fn sync(&mut self) {
        self.buffer.apply(self.screen.full_snapshot());
        let page = self.screen.scrollback_cells(0, i64::MAX / 4);
        self.live_start = page.live_start_row;
        self.viewport
            .complete_fetch(page, self.rows)
            .expect("history page");
    }

    /// The absolute row of a live (on-screen) row.
    fn live(&self, row: usize) -> i64 {
        self.live_start + row as i64
    }

    /// A pointer drag between two cell boundaries.
    fn copy(&self, start: (i64, usize), end: (i64, usize)) -> String {
        let mut selection = TerminalSelection::default();
        selection.begin(point(start));
        selection.drag_to(point(end));
        selection.selected_text(&self.viewport, &self.buffer)
    }

    fn copy_block(&self, start: (i64, usize), end: (i64, usize)) -> String {
        let mut selection = TerminalSelection::default();
        selection.begin_rectangle(point(start));
        selection.drag_to(point(end));
        selection.selected_text(&self.viewport, &self.buffer)
    }

    /// Keyboard copy mode: `v` on one cell, the cursor moved to another.
    fn copy_cells(&self, anchor: (i64, usize), cursor: (i64, usize)) -> String {
        let mut selection = TerminalSelection::default();
        selection.begin(point(anchor));
        selection.drag_cells_to(point(cursor));
        selection.selected_text(&self.viewport, &self.buffer)
    }

    /// Double click on a live cell.
    fn word(&self, row: usize, col: usize) -> String {
        let mut selection = TerminalSelection::default();
        selection.select_word(&self.viewport, &self.buffer, row, col);
        selection.selected_text(&self.viewport, &self.buffer)
    }

    /// Triple click on a live row.
    fn line(&self, row: usize) -> String {
        let mut selection = TerminalSelection::default();
        selection.select_line(&self.viewport, &self.buffer, row);
        selection.selected_text(&self.viewport, &self.buffer)
    }

    fn read_screen(&self) -> String {
        self.screen.lines().join("\n")
    }

    fn read_scrollback(&mut self) -> String {
        let mut lines = self.screen.scrollback().lines;
        while lines.last().is_some_and(String::is_empty) {
            lines.pop();
        }
        lines.join("\n")
    }
}

fn point((row, col): (i64, usize)) -> SelectionPoint {
    SelectionPoint { row, col }
}

/// A selection of on-screen cells.
#[derive(Clone, Copy)]
enum Pick {
    /// Pointer drag from one live `(row, edge)` to another.
    Drag((usize, usize), (usize, usize)),
    /// Option-Shift block drag between two live `(row, edge)` corners.
    Block((usize, usize), (usize, usize)),
    /// Double click on a live `(row, col)`.
    Word(usize, usize),
    /// Triple click on a live row.
    Line(usize),
}

struct Case {
    name: &'static str,
    cols: usize,
    rows: usize,
    bytes: &'static [u8],
    pick: Pick,
    clipboard: &'static str,
    /// `read_screen`, one row per line as the screen shows it.
    screen: &'static str,
}

const CASES: &[Case] = &[
    Case {
        name: "a soft wrap joins without a newline",
        cols: 8,
        rows: 3,
        bytes: b"abcdefghij\r\nnext",
        pick: Pick::Drag((0, 0), (2, 4)),
        clipboard: "abcdefghij\nnext",
        screen: "abcdefgh\nij\nnext",
    },
    Case {
        name: "blanks at a wrap are inside the line and kept",
        cols: 8,
        rows: 3,
        bytes: b"abc     def",
        pick: Pick::Drag((0, 0), (1, 3)),
        clipboard: "abc     def",
        screen: "abc\ndef",
    },
    Case {
        name: "a selection ending on a wrapped row trims its padding",
        cols: 8,
        rows: 3,
        bytes: b"abc     def",
        pick: Pick::Drag((0, 0), (0, 8)),
        clipboard: "abc",
        screen: "abc\ndef",
    },
    Case {
        name: "hard newlines stay and padding is trimmed per line, not inside",
        cols: 10,
        rows: 3,
        bytes: b"a  b   \r\nc",
        pick: Pick::Drag((0, 0), (1, 10)),
        clipboard: "a  b\nc",
        screen: "a  b\nc",
    },
    Case {
        name: "the last column is reachable",
        cols: 6,
        rows: 2,
        bytes: b"abcdef\r\n",
        pick: Pick::Drag((0, 0), (0, 6)),
        clipboard: "abcdef",
        screen: "abcdef",
    },
    Case {
        name: "wide CJK glyphs copy once",
        cols: 10,
        rows: 2,
        bytes: "日本語ab".as_bytes(),
        pick: Pick::Drag((0, 0), (0, 10)),
        clipboard: "日本語ab",
        screen: "日本語ab",
    },
    Case {
        name: "a selection starting inside a wide glyph leaves it out, as painted",
        cols: 10,
        rows: 2,
        bytes: "日本語ab".as_bytes(),
        pick: Pick::Drag((0, 1), (0, 10)),
        clipboard: "本語ab",
        screen: "日本語ab",
    },
    Case {
        name: "a selection ending inside a wide glyph takes all of it, as painted",
        cols: 10,
        rows: 2,
        bytes: "日本語ab".as_bytes(),
        pick: Pick::Drag((0, 0), (0, 3)),
        clipboard: "日本",
        screen: "日本語ab",
    },
    Case {
        name: "a wide glyph pushed to the next row by a wrap joins without its spacer",
        cols: 5,
        rows: 3,
        bytes: "abcd日本".as_bytes(),
        pick: Pick::Drag((0, 0), (1, 4)),
        clipboard: "abcd日本",
        screen: "abcd\n日本",
    },
    Case {
        name: "emoji ZWJ and VS16 clusters copy once",
        cols: 12,
        rows: 2,
        bytes: "a👩\u{200d}💻b❤\u{fe0f}c".as_bytes(),
        pick: Pick::Drag((0, 0), (0, 12)),
        clipboard: "a👩\u{200d}💻b❤\u{fe0f}c",
        screen: "a👩\u{200d}💻b❤\u{fe0f}c",
    },
    Case {
        name: "half of a wide emoji copies the whole emoji",
        cols: 12,
        rows: 2,
        bytes: "a🙂b".as_bytes(),
        pick: Pick::Drag((0, 0), (0, 2)),
        clipboard: "a🙂",
        screen: "a🙂b",
    },
    Case {
        name: "combining marks stay on their base",
        cols: 8,
        rows: 2,
        bytes: "e\u{301}x\u{327} z".as_bytes(),
        pick: Pick::Drag((0, 0), (0, 8)),
        clipboard: "e\u{301}x\u{327} z",
        screen: "e\u{301}x\u{327} z",
    },
    Case {
        name: "a tab copies as the blank it left, not a tab plus spaces",
        cols: 20,
        rows: 2,
        bytes: b"a\tb\tc",
        pick: Pick::Drag((0, 0), (0, 20)),
        clipboard: "a       b       c",
        screen: "a       b       c",
    },
    Case {
        name: "a trailing tab is padding",
        cols: 20,
        rows: 3,
        bytes: b"a\t\r\nb",
        pick: Pick::Drag((0, 0), (1, 20)),
        clipboard: "a\nb",
        screen: "a\nb",
    },
    Case {
        name: "box drawing copies as drawn",
        cols: 6,
        rows: 3,
        bytes: "┌──┐\r\n│ab│\r\n└──┘".as_bytes(),
        pick: Pick::Drag((0, 0), (2, 6)),
        clipboard: "┌──┐\n│ab│\n└──┘",
        screen: "┌──┐\n│ab│\n└──┘",
    },
    Case {
        name: "an OSC 8 link copies its text, not its URL",
        cols: 20,
        rows: 2,
        bytes: b"\x1b]8;;https://example.com/x\x1b\\link\x1b]8;;\x1b\\ text",
        pick: Pick::Drag((0, 0), (0, 20)),
        clipboard: "link text",
        screen: "link text",
    },
    Case {
        name: "a block selection copies its columns of each row",
        cols: 8,
        rows: 3,
        bytes: b"a 11 x\r\nb 22 y\r\nc 33 z",
        pick: Pick::Block((2, 4), (0, 2)),
        clipboard: "11\n22\n33",
        screen: "a 11 x\nb 22 y\nc 33 z",
    },
    Case {
        name: "a block selection never joins soft-wrapped rows",
        cols: 4,
        rows: 3,
        bytes: b"abcdefgh",
        pick: Pick::Block((0, 1), (1, 3)),
        clipboard: "bc\nfg",
        screen: "abcd\nefgh",
    },
    Case {
        name: "double click keeps a path with its line number",
        cols: 40,
        rows: 2,
        bytes: b"see ./crates/diri-app:42 now",
        pick: Pick::Word(0, 10),
        clipboard: "./crates/diri-app:42",
        screen: "see ./crates/diri-app:42 now",
    },
    Case {
        name: "double click keeps a URL with its query and drops sentence punctuation",
        cols: 60,
        rows: 2,
        bytes: b"open (https://diri.dev/a?q=1&v=2#x), then",
        pick: Pick::Word(0, 12),
        clipboard: "https://diri.dev/a?q=1&v=2#x",
        screen: "open (https://diri.dev/a?q=1&v=2#x), then",
    },
    Case {
        name: "double click stops at quotes",
        cols: 30,
        rows: 2,
        bytes: b"say \"hi there\" 'now'",
        pick: Pick::Word(0, 5),
        clipboard: "hi",
        screen: "say \"hi there\" 'now'",
    },
    Case {
        name: "double click on CJK takes the whole run, spacers and all",
        cols: 12,
        rows: 2,
        bytes: "日本語 ab".as_bytes(),
        pick: Pick::Word(0, 2),
        clipboard: "日本語",
        screen: "日本語 ab",
    },
    Case {
        name: "double click follows a URL through a soft wrap",
        cols: 12,
        rows: 4,
        bytes: b"go https://example.com/abc/def ok",
        pick: Pick::Word(1, 2),
        clipboard: "https://example.com/abc/def",
        screen: "go https://e\nxample.com/a\nbc/def ok",
    },
    Case {
        name: "double click follows a word through a soft wrap",
        cols: 8,
        rows: 4,
        bytes: b"run build_all_targets now",
        pick: Pick::Word(0, 5),
        clipboard: "build_all_targets",
        screen: "run buil\nd_all_ta\nrgets no\nw",
    },
    Case {
        name: "triple click takes the whole wrapped line",
        cols: 8,
        rows: 4,
        bytes: b"$ cargo test --all\r\nok",
        pick: Pick::Line(1),
        clipboard: "$ cargo test --all",
        screen: "$ cargo\ntest --a\nll\nok",
    },
    Case {
        name: "triple click on an unwrapped row takes only that row",
        cols: 8,
        rows: 3,
        bytes: b"one\r\ntwo\r\nthree",
        pick: Pick::Line(1),
        clipboard: "two",
        screen: "one\ntwo\nthree",
    },
    Case {
        name: "the alternate screen copies what it shows",
        cols: 12,
        rows: 3,
        bytes: b"shell\r\n\x1b[?1049h\x1b[Halt one\r\nalt two",
        pick: Pick::Drag((0, 0), (1, 12)),
        clipboard: "alt one\nalt two",
        screen: "alt one\nalt two",
    },
];

#[test]
fn clipboard_and_screen_reads_match_terminal_conventions() {
    let mut failures = Vec::new();
    for case in CASES {
        let pane = Pane::new(case.cols, case.rows, case.bytes);
        let live = |(row, col): (usize, usize)| (pane.live(row), col);
        let copied = match case.pick {
            Pick::Drag(start, end) => pane.copy(live(start), live(end)),
            Pick::Block(start, end) => pane.copy_block(live(start), live(end)),
            Pick::Word(row, col) => pane.word(row, col),
            Pick::Line(row) => pane.line(row),
        };
        if copied != case.clipboard {
            failures.push(format!(
                "{}: clipboard {copied:?}, want {:?}",
                case.name, case.clipboard
            ));
        }
        let screen = pane.read_screen();
        if screen != case.screen {
            failures.push(format!(
                "{}: read_screen {screen:?}, want {:?}",
                case.name, case.screen
            ));
        }
    }
    assert!(failures.is_empty(), "\n{}", failures.join("\n"));
}

#[test]
fn read_scrollback_reads_tabs_and_wraps_like_the_screen() {
    let mut pane = Pane::new(8, 2, b"a\tb\r\nabcdefghij\r\nx\r\ny");
    assert_eq!(pane.read_scrollback(), "a      b\nabcdefgh\nij\nx\ny");
}

#[test]
fn a_selection_spans_history_and_the_live_grid_and_survives_more_output() {
    let mut pane = Pane::new(10, 3, b"");
    for n in 0..8 {
        pane.feed(format!("line {n}\r\n").as_bytes());
    }
    // History holds lines 0..=5; the screen shows 6, 7 and the prompt row.
    let start = (pane.live(0) - 2, 5);
    let end = (pane.live(1), 4);
    assert_eq!(pane.copy(start, end), "4\nline 5\nline 6\nline");

    // Output streaming below scrolls the screen; absolute rows still name
    // the same text, so the selection copies what it did.
    pane.feed(b"line 8\r\nline 9\r\n");
    assert_eq!(pane.copy(start, end), "4\nline 5\nline 6\nline");
}

#[test]
fn keyboard_copy_mode_includes_the_anchor_and_cursor_cells_both_ways() {
    let pane = Pane::new(10, 3, b"0123456789\r\nabcdefghij");
    let (top, bottom) = (pane.live(0), pane.live(1));
    assert_eq!(pane.copy_cells((top, 2), (top, 5)), "2345");
    assert_eq!(pane.copy_cells((top, 5), (top, 2)), "2345");
    assert_eq!(pane.copy_cells((top, 9), (top, 9)), "9");
    assert_eq!(pane.copy_cells((bottom, 1), (top, 8)), "89\nab");
}

#[test]
fn osc52_writes_reach_the_clipboard_and_reads_are_never_answered() {
    let mut screen = HeadlessScreen::new(20, 2).with_notifications();
    // "two\nlines"
    screen.feed(b"\x1b]52;c;dHdvCmxpbmVz\x07");
    assert_eq!(screen.take_clipboard().as_deref(), Some("dHdvCmxpbmVz"));
    screen.feed(b"\x1b]52;c;?\x07");
    assert_eq!(screen.take_clipboard(), None);
    assert!(
        screen.take_replies().is_empty(),
        "no program reads the clipboard"
    );
}

#[test]
fn multi_line_paste_is_framed_and_cannot_close_its_own_bracket() {
    assert_eq!(
        paste("one\ntwo\n", true),
        b"\x1b[200~one\ntwo\n\x1b[201~".to_vec()
    );
    // A copied end marker cannot end the paste early and run what follows.
    assert_eq!(
        paste("x\x1b[201~rm -rf ~\n", true),
        b"\x1b[200~x [201~rm -rf ~\n\x1b[201~".to_vec()
    );
    assert!(!paste_needs_confirmation("one\ntwo", true));
    assert!(paste_needs_confirmation("one\ntwo", false));
    assert!(paste_needs_confirmation("\x1b[201~", true));
}
