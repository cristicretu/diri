//! A selection over live output stays on its text while that text scrolls
//! into history (`grid-history-v1`), and is dropped once the program
//! rewrites or clears it. Real escape sequences go through the shared
//! parser, the grid wire and the renderer's damage path.

use diri_proto::grid::GridUpdate;
use diri_term::{buffer::GridBuffer, element::TerminalElement};
use diri_terminal_state::HeadlessScreen;

const COLS: usize = 20;
const ROWS: usize = 5;

/// How the Engine and the client agreed to speak.
#[derive(Clone, Copy, PartialEq)]
enum Peer {
    /// Both sides speak `grid-history-v1`.
    Current,
    /// An Engine or Holder from before it: the styled grid only.
    Old,
}

struct Pane {
    screen: HeadlessScreen,
    element: TerminalElement,
    peer: Peer,
}

impl Pane {
    fn new(peer: Peer) -> Self {
        let mut pane = Self {
            screen: HeadlessScreen::new(COLS, ROWS),
            element: TerminalElement::with_buffer(GridBuffer::new(COLS as u16, ROWS as u16)),
            peer,
        };
        let seed = pane.screen.grid_update(true);
        pane.apply(seed);
        pane
    }

    fn apply(&self, update: GridUpdate) {
        let bytes = match self.peer {
            Peer::Current => update.encode(),
            Peer::Old => update.encode_without_history(),
        }
        .unwrap();
        self.element
            .apply_damage(GridUpdate::decode(&bytes).unwrap());
    }

    /// Feeds bytes and publishes the result as one grid.
    fn print(&mut self, bytes: impl AsRef<[u8]>) {
        self.screen.feed(bytes.as_ref());
        let update = self.screen.grid_update(false);
        self.apply(update);
    }

    /// Selects `len` cells of window row `row` from column `col`.
    fn select(&self, row: usize, col: usize, len: usize) {
        self.element.begin_selection(col, row);
        self.element
            .drag_selection_to_edge(col + len - 1, col + len, row);
    }

    fn selected(&self) -> String {
        self.element.selected_text()
    }

    /// The text of window row `row` as the view paints it.
    fn window_text(&self, row: usize) -> String {
        let buffer = self.element.buffer();
        let buffer = buffer.read().unwrap();
        self.element
            .viewport()
            .window_row(&buffer, row)
            .iter()
            .map(|cell| {
                char::from_u32(cell.scalar)
                    .filter(|c| *c != '\0')
                    .unwrap_or(' ')
            })
            .collect::<String>()
            .trim_end()
            .to_owned()
    }

    /// Answers the view's pending scrollback fetch from the terminal as it
    /// is now.
    fn answer_fetch(&mut self) {
        while let Some(request) = self.element.begin_scrollback_fetch(ROWS) {
            let reply = self
                .screen
                .scrollback_cells(request.first_row, request.max_rows);
            self.element.complete_scrollback_fetch(reply, ROWS).unwrap();
        }
    }

    /// The window row the selection starts on, and the text painted there.
    fn highlighted(&self) -> Option<(i64, String)> {
        let range = self.element.selection_range()?;
        let row = self
            .element
            .viewport()
            .window_row_for_absolute(range.start.row)?;
        let text = usize::try_from(row)
            .ok()
            .filter(|row| *row < ROWS)
            .map(|row| self.window_text(row))?;
        Some((row, text))
    }
}

#[test]
fn a_selection_stays_on_its_line_while_a_build_log_streams_past_it() {
    let mut pane = Pane::new(Peer::Current);
    pane.print("$ cargo build\r\n   Compiling diri\r\n");
    pane.select(1, 3, 15);
    assert_eq!(pane.selected(), "Compiling diri");
    let selected_row = pane.element.selection_range().unwrap().start.row;

    for step in 0..40 {
        pane.print(format!("   Compiling dep{step}\r\n"));
        assert_eq!(pane.selected(), "Compiling diri", "after step {step}");
    }
    // The line is history row 1 now, and the selection names it there.
    let range = pane.element.selection_range().unwrap();
    assert_eq!(range.start.row, selected_row);
    assert_eq!(range.start.row, 1);

    // Scrolled back to, the highlight is drawn over the same text.
    // The first step learns how much history there is.
    pane.element.set_view_offset(ROWS as i64, ROWS);
    pane.answer_fetch();
    pane.element.set_view_offset(40, ROWS);
    pane.answer_fetch();
    let (row, text) = pane.highlighted().expect("selection on screen");
    assert_eq!(text, "   Compiling diri", "window row {row}");
    assert_eq!(pane.selected(), "Compiling diri");
}

#[test]
fn an_agent_footer_repainting_under_streamed_lines_keeps_the_selection() {
    // An Ink-style transcript: each step prints a line above a two-row
    // footer the program erases and redraws.
    let mut pane = Pane::new(Peer::Current);
    let footer = |step: usize| format!("> thinking {step}\r\n  esc to interrupt");
    pane.print(format!("result: ok\r\n{}", footer(0)));
    pane.select(0, 0, 10);
    assert_eq!(pane.selected(), "result: ok");
    for step in 1..30 {
        pane.print(format!(
            "\r\x1b[1A\x1b[J  tool call {step}\r\n{}",
            footer(step)
        ));
        assert_eq!(pane.selected(), "result: ok", "after step {step}");
    }

    // A selection on the footer itself goes when the footer is redrawn.
    pane.select(ROWS - 2, 0, 12);
    assert_eq!(pane.selected(), "> thinking 2");
    pane.print(format!("\r\x1b[1A\x1b[J  tool call 30\r\n{}", footer(30)));
    assert_eq!(pane.selected(), "");
}

#[test]
fn rewriting_the_selected_cells_in_place_drops_the_selection() {
    let mut pane = Pane::new(Peer::Current);
    pane.print("alpha\r\nbravo charlie\r\ndelta\r\n");
    pane.select(1, 0, 5);
    // Output elsewhere on the selected row leaves the selected cells alone.
    pane.print("\x1b[2;15Hecho\x1b[4;1H");
    assert_eq!(pane.selected(), "bravo");
    // So does output that scrolls it up a row and repaints it with the
    // same text.
    pane.print("echo\r\nfoxtrot\r\n\x1b[1;1Hbravo");
    assert_eq!(pane.selected(), "bravo");
    assert_eq!(pane.window_text(0), "bravo charlie echo");
    // A rewrite of what was selected does not.
    pane.print("\x1b[1;1Hbrave");
    assert_eq!(pane.selected(), "");
    assert_eq!(pane.element.selection_range(), None);
}

#[test]
fn clearing_the_terminal_drops_the_selection() {
    let mut pane = Pane::new(Peer::Current);
    pane.print("alpha\r\nbravo\r\n");
    pane.select(1, 0, 5);
    // `clear`: home, erase the screen, erase history.
    pane.print("\x1b[H\x1b[2J\x1b[3J");
    assert_eq!(pane.selected(), "");

    // Erasing only the screen moves its rows into history, untouched, and
    // the selection with them.
    pane.print("charlie\r\n");
    pane.select(0, 0, 7);
    pane.print("\x1b[H\x1b[2J");
    assert_eq!(pane.selected(), "charlie");
    pane.print("\x1b[3J");
    assert_eq!(pane.selected(), "", "its history row is gone");
}

#[test]
fn the_selection_goes_when_its_text_leaves_the_oldest_end_of_history() {
    let mut pane = Pane::new(Peer::Current);
    pane.print("keep me\r\n");
    pane.select(0, 0, 7);
    let mut printed = 0;
    while pane.element.selection_range().is_some() {
        assert_eq!(pane.selected(), "keep me", "after {printed} lines");
        pane.print("filler line\r\n".repeat(500));
        printed += 500;
        assert!(printed <= 20_000, "never evicted");
    }
    // The terminal keeps 10,000 rows of history.
    assert!(printed > 9_000, "{printed}");
}

#[test]
fn a_reseed_after_a_lagging_client_keeps_text_that_is_still_there() {
    let mut pane = Pane::new(Peer::Current);
    pane.print("one\r\ntwo\r\n");
    pane.select(1, 0, 3);
    // The client missed these diffs; the Engine reseeds it with a snapshot.
    pane.screen.feed("lost\r\n".repeat(30).as_bytes());
    let _ = pane.screen.grid_update(false);
    let reseed = pane.screen.full_snapshot();
    pane.apply(reseed);
    assert_eq!(pane.selected(), "two");
}

#[test]
fn reading_view_keeps_its_rows_while_output_arrives_during_the_fetch() {
    let mut pane = Pane::new(Peer::Current);
    for line in 0..20 {
        pane.print(format!("line {line}\r\n"));
    }
    pane.select(1, 0, 7);
    assert_eq!(pane.selected(), "line 17");
    // Scroll up two rows; output lands before the fetch is answered.
    pane.element.set_view_offset(2, ROWS);
    assert_eq!(pane.window_text(2), "line 16", "the held screen");
    for line in 20..26 {
        pane.print(format!("line {line}\r\n"));
    }
    pane.answer_fetch();
    assert_eq!(
        (0..ROWS)
            .map(|row| pane.window_text(row))
            .collect::<Vec<_>>(),
        ["line 14", "line 15", "line 16", "line 17", "line 18"],
        "the reader's rows did not move"
    );
    let (_, text) = pane.highlighted().unwrap();
    assert_eq!(text, "line 17");
    assert_eq!(pane.selected(), "line 17");
}

#[test]
fn alternate_screen_output_drops_a_selection_it_overwrites() {
    let mut pane = Pane::new(Peer::Current);
    pane.print("shell\r\n\x1b[?1049h");
    pane.element.set_modes(true, Default::default());
    pane.print("\x1b[Hmenu one\r\nmenu two\r\nmenu three");
    pane.select(1, 0, 8);
    assert_eq!(pane.selected(), "menu two");
    // No history on the alternate screen: a scroll there rewrites rows.
    pane.print("\x1b[5;1H\r\n");
    assert_eq!(pane.selected(), "");
}

#[test]
fn an_old_engine_without_history_positions_keeps_todays_behaviour() {
    let mut pane = Pane::new(Peer::Old);
    pane.print("$ cargo build\r\n   Compiling diri\r\n");
    pane.select(1, 3, 15);
    // Output below the selection that does not scroll leaves it.
    pane.print("   Compiling a");
    assert_eq!(pane.selected(), "Compiling diri");
    // The first scroll repaints its row, and without a position the client
    // cannot tell that from a rewrite.
    pane.print("\r\n   Compiling b\r\n   Compiling c\r\n");
    assert_eq!(pane.selected(), "");
}
