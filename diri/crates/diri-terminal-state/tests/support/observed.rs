//! What a terminal shows and reports after a parse, in one comparable
//! value. Shared by the chunking differential tests and the `vt_workloads`
//! bench, which both require two parses of the same bytes to agree on all of
//! it.

#![allow(dead_code)]

use diri_terminal_state::HeadlessScreen;

/// The local Engine's configuration.
pub fn screen(cols: usize, rows: usize) -> HeadlessScreen {
    HeadlessScreen::new_with_keyboard_enhancements(cols, rows)
        .with_notifications()
        .with_inline_images()
        .with_host_replies("diri-test")
}

/// Everything observable that does not count reads.
#[derive(Debug, PartialEq)]
pub struct Observed {
    pub replies: Vec<u8>,
    pub notifications: Vec<String>,
    pub clipboard: Vec<String>,
    pub scrollback_lines: Vec<String>,
    pub scrollback_text_cells: String,
    pub visible_start_row: i64,
    pub cells_payload: Vec<u8>,
    pub cells_metadata: String,
    pub full_snapshot: String,
    pub link_text: String,
    pub links: Vec<(usize, String)>,
    pub title: Option<String>,
    pub modes: String,
    pub cursor: (u16, u16, bool),
    pub keyboard: String,
    pub progress: Option<(i64, i64)>,
    /// Whether any synchronized update was closed, and whether one is open.
    /// How many closed is not compared: the parser counts at most one per
    /// read, which is all the Engine asks of it (did a frame complete).
    pub synchronized: (bool, bool),
    pub image_bytes: usize,
    /// Each stored image's ids, size and a hash of its bytes.
    pub images: Vec<String>,
    pub program_status: String,
}

pub struct Feeder {
    pub screen: HeadlessScreen,
    replies: Vec<u8>,
    notifications: Vec<String>,
    clipboard: Vec<String>,
}

impl Feeder {
    pub fn new(cols: usize, rows: usize) -> Self {
        Self {
            screen: screen(cols, rows),
            replies: Vec::new(),
            notifications: Vec::new(),
            clipboard: Vec::new(),
        }
    }

    /// One read, handled the way the Engine pump handles it.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.screen.feed(bytes);
        self.replies.extend(self.screen.take_replies());
        self.notifications.extend(
            self.screen
                .take_notifications()
                .into_iter()
                .map(|n| format!("{n:?}")),
        );
        self.clipboard.extend(self.screen.take_clipboard());
    }

    pub fn observe(mut self) -> Observed {
        self.snapshot()
    }

    /// [`Self::observe`] without consuming the feeder, so one reference can
    /// be compared with many parses.
    pub fn snapshot(&mut self) -> Observed {
        let screen = &mut self.screen;
        let scrollback = screen.scrollback();
        let cells = screen.scrollback_cells(0, i64::MAX);
        let links = screen.link_source(usize::MAX);
        Observed {
            replies: self.replies.clone(),
            notifications: self.notifications.clone(),
            clipboard: self.clipboard.clone(),
            scrollback_lines: scrollback.lines,
            scrollback_text_cells: format!("{:?}", scrollback.text_cells),
            visible_start_row: scrollback.visible_start_row,
            cells_payload: cells.payload,
            cells_metadata: without_store_nonce(format!("{:?}", cells.metadata)),
            full_snapshot: without_store_nonce(format!("{:?}", screen.full_snapshot())),
            link_text: links.text,
            links: links
                .hyperlinks
                .into_iter()
                .map(|(at, uri)| (at, without_store_nonce(uri)))
                .collect(),
            title: screen.title().map(str::to_owned),
            modes: format!(
                "alt={} paste={} focus={} mouse={:?} cursor={:?}",
                screen.is_alt_screen(),
                screen.bracketed_paste(),
                screen.focus_reporting(),
                screen.mouse_modes(),
                screen.cursor_style(),
            ),
            cursor: screen.cursor(),
            keyboard: format!("{:?}", screen.keyboard_state()),
            progress: screen.progress(),
            synchronized: (
                screen.synchronized_updates_completed() > 0,
                screen.in_synchronized_update(),
            ),
            image_bytes: screen.inline_image_bytes(),
            images: screen
                .inline_image_store_summary()
                .into_iter()
                .map(without_store_nonce)
                .collect(),
            program_status: format!("{:?}", screen.program_status()),
        }
    }
}

/// Image anchors name a per-screen store; only the rest must match.
pub fn without_store_nonce(text: String) -> String {
    const ANCHOR: &str = "diri-image:";
    let mut out = String::with_capacity(text.len());
    let mut rest = text.as_str();
    while let Some(at) = rest.find(ANCHOR) {
        out.push_str(&rest[..at + ANCHOR.len()]);
        rest = &rest[at + ANCHOR.len()..];
        let nonce = rest.find('.').unwrap_or(0);
        out.push('#');
        rest = &rest[nonce..];
    }
    out.push_str(rest);
    out
}

pub fn assert_same(expected: &Observed, actual: &Observed, how: &str) {
    if expected == actual {
        return;
    }
    let e = format!("{expected:#?}");
    let a = format!("{actual:#?}");
    let field = e
        .lines()
        .zip(a.lines())
        .take_while(|(x, y)| x == y)
        .filter(|(x, _)| x.starts_with("    ") && !x.starts_with("     "))
        .last()
        .map_or("", |(x, _)| x);
    let line = e
        .lines()
        .zip(a.lines())
        .position(|(x, y)| x != y)
        .unwrap_or(0);
    let (x, y) = e.lines().zip(a.lines()).nth(line).unwrap_or(("", ""));
    let column = x
        .char_indices()
        .zip(y.chars())
        .find(|((_, p), q)| p != q)
        .map_or(0, |((i, _), _)| i);
    let window = |s: &str| {
        let start = s.floor_char_boundary(column.saturating_sub(120));
        let end = s.ceil_char_boundary((column + 120).min(s.len()));
        s[start..end].to_owned()
    };
    eprintln!(
        "first difference:\n  one shot: {}\n  {how}: {}",
        window(x),
        window(y)
    );
    let context = |s: &str| {
        s.lines()
            .skip(line.saturating_sub(3))
            .take(6)
            .collect::<Vec<_>>()
            .join("\n")
    };
    panic!(
        "{how}: state differs from a one-shot parse (after {field})\n--- one shot\n{}\n--- {how}\n{}",
        context(&e),
        context(&a)
    );
}
