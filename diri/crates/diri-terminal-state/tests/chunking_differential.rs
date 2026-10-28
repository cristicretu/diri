//! Every byte reaches the parser, in order, whatever the read boundaries.
//!
//! A terminal that parses only some of what a program printed, or whose state
//! depends on where reads happened to split the output, shows a screen,
//! scrollback, title or mode the program never produced. This feeds one
//! deterministic mixed stream (SGR, cursor and erase CSIs, scroll regions,
//! OSC 8 links, titles and the title stack, alternate screen, mouse/paste/
//! keyboard modes, wide, emoji, ZWJ and combining text, Kitty graphics APCs,
//! shell marks, progress, queries, synchronized output and storms of
//! state-free controls) in one call, a byte at a time, split in two at every
//! offset, and in random chunkings, and requires the same final state from
//! all of them: visible grid, cursor, every retained history row with its
//! cells and metadata, hyperlinks, title, modes, replies, notifications and
//! stored images.
//!
//! `a_dropped_span_is_detected` keeps the comparison honest: feeding the
//! same stream with one span missing (as a terminal that overwrites a read
//! ring would) must not compare equal.

use diri_terminal_state::HeadlessScreen;

const COLS: usize = 48;
const ROWS: usize = 12;

/// Small deterministic generator; no dependency, stable across platforms.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64*
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, bound: usize) -> usize {
        (self.next() % bound as u64) as usize
    }
}

fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(TABLE[(n >> (18 - 6 * i)) as usize & 63] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Escape-dense fragments. Each one leaves state behind that a lost or
/// misparsed byte would change.
fn fragments() -> Vec<Vec<u8>> {
    let pixels: Vec<u8> = (0..2 * 2 * 3).map(|i| (i * 21) as u8).collect();
    let encoded = base64(&pixels);
    let (head, tail) = encoded.split_at(8);
    let mut list: Vec<Vec<u8>> = [
        // SGR: indexed, truecolor (both separators), styles, underline colour.
        "\x1b[31;1mred bold\x1b[0m ",
        "\x1b[38;2;10;200;30;48;5;17mtrue\x1b[39;49m ",
        "\x1b[38:2::1:2:3mcolon\x1b[m ",
        "\x1b[3;4:3;58;5;9mcurly\x1b[23;24;59m ",
        "\x1b[7;9;2mrev\x1b[27;29;22m ",
        "\x1b[5;53mblink\x1b[25;55m ",
        // Cursor movement and erasure.
        "\x1b[2;5Hpos\x1b[3Gx\x1b[2Ay\x1b[B\x1b[4Cz\x1b[D",
        "\x1b[44m\x1b[K\x1b[0m\x1b[1K\x1b[2X",
        "\x1b[s\x1b[10;1Hsaved\x1b[u",
        "\x1b7\x1b[1;1H\x1b[1@#\x1b8",
        "\x1b[3;8r\x1b[5;1Hregion\x1b[2S\x1b[1T\x1b[r",
        "\x1b[2L\x1b[1M\x1b[3@\x1b[2P",
        "\x1b[4hins\x1b[4l",
        "\x1b[?7lno-wrap-------------------------------------------------\x1b[?7h",
        "\x1b(0lqqk\x1b(B",
        "\x1bH\t\x1b[3g\t\x1b[0g",
        "\x1bD\x1bM\x1bE",
        // OSC 8 links, with and without ids, both terminators.
        "\x1b]8;id=a;https://example.invalid/a\x07link-a\x1b]8;;\x07 ",
        "\x1b]8;;https://example.invalid/b?q=1\x1b\\link-b\x1b]8;;\x1b\\ ",
        // Titles and the title stack.
        "\x1b]0;zero\x07",
        "\x1b]2;two\x1b\\",
        "\x1b[22;0t\x1b]2;pushed\x07\x1b[23;0t",
        // Modes.
        "\x1b[?2004h",
        "\x1b[?1000h\x1b[?1006h",
        "\x1b[?1002h\x1b[?1002l",
        "\x1b[?1004h",
        "\x1b[?1h\x1b=",
        "\x1b[?25l\x1b[?25h",
        "\x1b[5 q",
        "\x1b[>1u",
        "\x1b[<u",
        // Alternate screen with its own content, then back.
        "\x1b[?1049h\x1b[Halt screen\x1b[?1049l",
        "\x1b[?1047h\x1b[2Jalt2\x1b[?1047l",
        // Wide, emoji, ZWJ, regional indicators, variation selectors,
        // combining marks, Indic.
        "漢字かな ",
        "👩\u{200d}💻 🇷🇴 ❤\u{fe0f} ",
        "e\u{301}a\u{308}\u{323} ",
        "क्षि ",
        // Shell integration and progress.
        "\x1b]133;A\x07$ \x1b]133;B\x07cmd\x1b]133;C\x07\r\n\x1b]133;D;0\x07",
        "\x1b]9;4;1;40\x07",
        "\x1b]7;file://host/tmp\x07",
        // Queries: answers must be complete and in order.
        "\x1b[6n",
        "\x1b[c",
        "\x1b[>c",
        "\x1b[?u",
        "\x1b[?2004$p",
        // Synchronized output.
        "\x1b[?2026hheld\x1b[1;1Hframe\x1b[?2026l",
        // Notifications.
        "\x1b]9;hello\x07",
        "\x1b]777;notify;title;body\x07",
        // Line ends.
        "\r\n",
        "\r\n",
        "\r\n",
    ]
    .iter()
    .map(|s| s.as_bytes().to_vec())
    .collect();
    // Kitty graphics: one direct upload with display, one chunked upload, and
    // a placement of the first image again.
    list.push(format!("\x1b_Ga=T,f=24,s=2,v=2,i=7,q=1;{encoded}\x1b\\").into_bytes());
    list.push(
        format!("\x1b_Ga=T,f=24,s=2,v=2,i=8,m=1;{head}\x1b\\\x1b_Gm=0;{tail}\x1b\\").into_bytes(),
    );
    list.push(b"\x1b_Ga=p,i=7\x1b\\".to_vec());
    list
}

/// Controls that change nothing a reader sees, emitted in bulk. A terminal
/// that bounds per-read bookkeeping can lose the state change after them.
fn storms() -> Vec<Vec<u8>> {
    vec![
        [&b"\x07".repeat(200)[..], b"\x1b]2;after bells\x07"].concat(),
        [
            &b"\x1b[?1000h\x1b[?1000l".repeat(80)[..],
            b"\x1b]2;after mouse toggles\x07",
        ]
        .concat(),
        [
            &b"\x1b[?12h\x1b[?12l".repeat(80)[..],
            b"\x1b]2;after blink toggles\x07",
        ]
        .concat(),
        [&b"\x1b[c".repeat(100)[..], b"\x1b]2;after queries\x07"].concat(),
    ]
}

/// Plain and styled lines, plus progress-bar style rewrites of one row.
fn text_line(rng: &mut Rng, line: usize) -> Vec<u8> {
    let mut out = Vec::new();
    match rng.below(4) {
        0 => out.extend_from_slice(format!("line {line:06} plain ascii text").as_bytes()),
        1 => out.extend_from_slice(
            format!(
                "\x1b[38;5;{}mline {line:06}\x1b[0m \x1b[1mbold\x1b[22m {}",
                rng.below(256),
                "wrapping ".repeat(1 + rng.below(9))
            )
            .as_bytes(),
        ),
        2 => {
            for step in 0..20 {
                out.extend_from_slice(
                    format!("\rline {line:06} progress {step:02}/19 ").as_bytes(),
                );
            }
        }
        _ => out.extend_from_slice(format!("line {line:06} 漢字 👍🏽 e\u{301}").as_bytes()),
    }
    out.extend_from_slice(b"\r\n");
    out
}

/// The dense stream: every fragment and storm, shuffled, with text between.
fn dense_stream(seed: u64) -> Vec<u8> {
    let mut rng = Rng(seed);
    let mut pieces = fragments();
    pieces.extend(storms());
    let mut out = Vec::new();
    let mut line = 0;
    while !pieces.is_empty() {
        let piece = pieces.swap_remove(rng.below(pieces.len()));
        out.extend_from_slice(&piece);
        if rng.below(3) == 0 {
            out.extend(text_line(&mut rng, line));
            line += 1;
        }
    }
    out
}

/// Thousands of lines with fragments mixed in, enough to scroll most rows
/// into history.
fn long_stream(seed: u64, lines: usize) -> Vec<u8> {
    let mut rng = Rng(seed);
    let pieces = fragments();
    let storms = storms();
    let mut out = Vec::new();
    for line in 0..lines {
        if rng.below(4) == 0 {
            out.extend_from_slice(&pieces[rng.below(pieces.len())]);
        }
        if rng.below(200) == 0 {
            out.extend_from_slice(&storms[rng.below(storms.len())]);
        }
        out.extend(text_line(&mut rng, line));
    }
    // Leave something open at the end: a mode, a link, a pen and a title.
    out.extend_from_slice(
        b"\x1b[?1003h\x1b]8;;https://example.invalid/end\x07\x1b[35mopen link\x1b]2;final\x07",
    );
    out
}

/// Lines that stay where they were printed, each with a visible change:
/// styled text, links, wide and emoji text, inline images (a title is
/// replaced by the next, so it is checked separately). No cursor
/// movement, erasure, scroll region, rewrite or neutral control, so any
/// lost byte leaves a trace.
fn lasting_stream(lines: usize) -> Vec<u8> {
    let mut rng = Rng(3);
    let image = fragments().pop().expect("image placement");
    let mut out = Vec::new();
    for line in 0..lines {
        let text = match rng.below(5) {
            0 => format!("\x1b[38;5;{}mline {line:06}\x1b[0m", rng.below(256)),
            1 => format!("\x1b]8;;https://example.invalid/{line}\x07line {line:06}\x1b]8;;\x07"),
            2 => format!("line {line:06} 漢字 👍🏽 e\u{301}"),
            3 => format!(
                "line {line:06} \x1b[4:3;58;5;{}mcurly\x1b[24;59m",
                rng.below(256)
            ),
            _ => format!("line {line:06} \x1b[1;3;4mstyled\x1b[m"),
        };
        out.extend_from_slice(text.as_bytes());
        if line == 0 {
            // The image every later placement shows.
            out.extend_from_slice(&fragments()[fragments().len() - 3]);
        } else if rng.below(50) == 0 {
            out.extend_from_slice(&image);
        }
        out.extend_from_slice(b"\r\n");
    }
    out
}

fn screen() -> HeadlessScreen {
    // The local Engine's configuration.
    HeadlessScreen::new_with_keyboard_enhancements(COLS, ROWS)
        .with_notifications()
        .with_inline_images()
        .with_host_replies("diri-test")
}

/// Everything observable that does not count reads.
#[derive(Debug, PartialEq)]
struct Observed {
    replies: Vec<u8>,
    notifications: Vec<String>,
    clipboard: Vec<String>,
    scrollback_lines: Vec<String>,
    scrollback_text_cells: String,
    visible_start_row: i64,
    cells_payload: Vec<u8>,
    cells_metadata: String,
    full_snapshot: String,
    link_text: String,
    links: Vec<(usize, String)>,
    title: Option<String>,
    modes: String,
    cursor: (u16, u16, bool),
    keyboard: String,
    progress: Option<(i64, i64)>,
    /// Whether any synchronized update was closed, and whether one is open.
    /// How many closed is not compared: the parser counts at most one per
    /// read, which is all the Engine asks of it (did a frame complete).
    synchronized: (bool, bool),
    image_bytes: usize,
    program_status: String,
}

struct Feeder {
    screen: HeadlessScreen,
    replies: Vec<u8>,
    notifications: Vec<String>,
    clipboard: Vec<String>,
}

impl Feeder {
    fn new() -> Self {
        Self {
            screen: screen(),
            replies: Vec::new(),
            notifications: Vec::new(),
            clipboard: Vec::new(),
        }
    }

    /// One read, handled the way the Engine pump handles it.
    fn feed(&mut self, bytes: &[u8]) {
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

    fn observe(mut self) -> Observed {
        let screen = &mut self.screen;
        let scrollback = screen.scrollback();
        let cells = screen.scrollback_cells(0, i64::MAX);
        let links = screen.link_source(usize::MAX);
        Observed {
            replies: self.replies,
            notifications: self.notifications,
            clipboard: self.clipboard,
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
            program_status: format!("{:?}", screen.program_status()),
        }
    }
}

/// Image anchors name a per-screen store; only the rest must match.
fn without_store_nonce(text: String) -> String {
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

fn observe_chunks<'a>(chunks: impl IntoIterator<Item = &'a [u8]>) -> Observed {
    let mut feeder = Feeder::new();
    for chunk in chunks {
        feeder.feed(chunk);
    }
    feeder.observe()
}

fn random_chunks<'a>(bytes: &'a [u8], rng: &mut Rng, max: usize) -> Vec<&'a [u8]> {
    let mut chunks = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        // Mostly small reads (a macOS PTY hands over ~1 KiB), sometimes big.
        let len = match rng.below(4) {
            0 => 1 + rng.below(8),
            1 => 1 + rng.below(1024),
            _ => 1 + rng.below(max),
        };
        let end = (at + len).min(bytes.len());
        chunks.push(&bytes[at..end]);
        at = end;
    }
    chunks
}

/// Names the first field that differs, so a failure says what was lost.
fn assert_same(expected: &Observed, actual: &Observed, how: &str) {
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

#[test]
fn the_dense_stream_parses_the_same_split_at_every_offset() {
    let stream = dense_stream(0x5eed);
    let expected = observe_chunks([&stream[..]]);
    assert!(
        expected.title.is_some() && !expected.replies.is_empty() && expected.image_bytes > 0,
        "the stream exercises titles, replies and images: {expected:?}"
    );
    assert_same(
        &expected,
        &observe_chunks(stream.chunks(1)),
        "a byte at a time",
    );
    for split in 1..stream.len() {
        assert_same(
            &expected,
            &observe_chunks([&stream[..split], &stream[split..]]),
            &format!("split at {split} of {}", stream.len()),
        );
    }
}

#[test]
fn dense_streams_parse_the_same_in_random_chunks() {
    for seed in 1..=24u64 {
        let stream = dense_stream(seed);
        let expected = observe_chunks([&stream[..]]);
        let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
        for round in 0..8 {
            let chunks = random_chunks(&stream, &mut rng, 256);
            assert_same(
                &expected,
                &observe_chunks(chunks),
                &format!("seed {seed} round {round}"),
            );
        }
    }
}

#[test]
fn a_long_stream_keeps_every_history_row_whatever_the_chunking() {
    let stream = long_stream(42, 4_000);
    let expected = observe_chunks([&stream[..]]);
    // Most of it scrolled into history, all of it still retained.
    assert!(
        expected.visible_start_row > 3_000,
        "history rows: {}",
        expected.visible_start_row
    );
    assert!(
        expected
            .scrollback_lines
            .iter()
            .any(|l| l.contains("line 000000"))
            && expected
                .scrollback_lines
                .iter()
                .any(|l| l.contains("line 003999")),
        "first and last lines retained"
    );
    assert_eq!(expected.title.as_deref(), Some("final"));
    // Desktop alerts keep only the newest 32 per read, by design (a program
    // printing thousands in one burst must not raise thousands), so how many
    // arrive depends on reads. The newest is compared; the rest of the state
    // must match exactly.
    let newest = |mut observed: Observed| {
        observed.notifications = observed.notifications.pop().into_iter().collect();
        observed
    };
    let expected = newest(expected);
    let mut rng = Rng(7);
    for (round, max) in [64, 1024, 4096, 65_536, 1 << 20].into_iter().enumerate() {
        let chunks = random_chunks(&stream, &mut rng, max);
        assert_same(
            &expected,
            &newest(observe_chunks(chunks)),
            &format!("round {round}, reads up to {max} bytes"),
        );
    }
}

#[test]
fn a_dropped_span_is_detected() {
    // What a terminal that skips output would parse instead. Each must be
    // told apart from the full parse.
    let stream = long_stream(42, 4_000);
    let expected = observe_chunks([&stream[..]]);
    let differs = |lossy: &[u8]| observe_chunks([lossy]) != expected;

    // A 64 KiB ring overwritten per tick: only the first 1% of every 64 KiB
    // window is parsed.
    let ring: Vec<u8> = stream
        .chunks(64 << 10)
        .flat_map(|window| &window[..window.len() / 100])
        .copied()
        .collect();
    assert!(differs(&ring), "a ring that parses 1% went unnoticed");

    // A cut inside a CSI that loses the rest of it, so its parameters
    // would print as text.
    let csi = find(&stream, b"\x1b[38;2;10;200;30;48;5;17m");
    assert!(differs(&[&stream[..csi + 9], &stream[csi + 20..]].concat()));

    // A dropped span holding only a state change: SGR, a mode, a title, the
    // alternate screen, a hyperlink.
    for change in [
        &b"\x1b[35m"[..],
        b"\x1b[?1003h",
        b"\x1b]2;final\x07",
        b"\x1b]8;;https://example.invalid/end\x07",
    ] {
        let at = stream
            .windows(change.len())
            .rposition(|w| w == change)
            .expect("change present");
        let lossy = [&stream[..at], &stream[at + change.len()..]].concat();
        assert!(
            differs(&lossy),
            "dropping {:?} went unnoticed",
            String::from_utf8_lossy(change)
        );
    }
    let alt = find(&stream, b"\x1b[?1049h\x1b[Halt screen\x1b[?1049l");
    assert!(differs(&[&stream[..alt], &stream[alt + 10..]].concat()));

    // Random lost spans in a stream where nothing is overwritten, erased or
    // scrolled away: every one must show, escapes cut after it and all.
    let stream = lasting_stream(4_000);
    let expected = observe_chunks([&stream[..]]);
    let mut rng = Rng(99);
    for _ in 0..100 {
        let at = rng.below(stream.len() - 64);
        let len = 1 + rng.below(64);
        let lossy = [&stream[..at], &stream[at + len..]].concat();
        assert!(
            observe_chunks([&lossy[..]]) != expected,
            "dropping {len} bytes at {at} went unnoticed: {:?}",
            String::from_utf8_lossy(&stream[at..at + len])
        );
    }
}

/// The last occurrence: the least likely to have been overwritten since.
fn find(haystack: &[u8], needle: &[u8]) -> usize {
    haystack
        .windows(needle.len())
        .rposition(|w| w == needle)
        .expect("present")
}

#[test]
fn a_cut_inside_an_escape_never_prints_its_parameters() {
    // Split every CSI, OSC and APC fragment at each of its bytes, with plain
    // text after it: the text must land where it lands in a one-shot parse
    // and no parameter bytes may appear on screen.
    for fragment in fragments() {
        let mut stream = b"start ".to_vec();
        stream.extend_from_slice(&fragment);
        stream.extend_from_slice(b" end");
        let expected = observe_chunks([&stream[..]]);
        for split in 1..stream.len() {
            assert_same(
                &expected,
                &observe_chunks([&stream[..split], &stream[split..]]),
                &format!("{:?} split at {split}", String::from_utf8_lossy(&fragment)),
            );
        }
    }
}

#[test]
fn state_after_hundreds_of_events_in_one_read_survives() {
    // One read with more events than a small bounded queue holds: the title
    // set after them, and every query's answer, must still arrive.
    let mut screen = screen();
    screen.feed(
        &[
            &b"\x07".repeat(500)[..],
            &b"\x1b[?1000h\x1b[?1000l".repeat(100),
            b"\x1b]2;kept\x07",
            &b"\x1b[5n".repeat(300),
        ]
        .concat(),
    );
    assert_eq!(screen.title(), Some("kept"));
    assert_eq!(screen.take_replies(), b"\x1b[0n".repeat(300));
}

/// Hand-made and random escape soups from an independent review
/// (`fixtures/adversarial-gen.py`, seed 922; the inputs over 64 KiB are left
/// out): C1 controls, CAN/SUB inside every string type, OSC 8 and titles
/// with odd terminators, DCS queries, notifications, Kitty uploads chunked,
/// padded, URL-safe, whitespace-wrapped, invalid and interleaved with text,
/// iTerm2 images, REP and CSI edge cases. Each must end in the same state in
/// one read, a byte at a time, split in two anywhere, and in random reads.
#[test]
fn adversarial_inputs_parse_the_same_however_they_are_read() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/adversarial");
    let mut paths: Vec<_> = std::fs::read_dir(&dir)
        .expect("fixtures")
        .map(|entry| entry.expect("entry").path())
        .collect();
    paths.sort();
    assert!(paths.len() >= 40, "fixtures missing: {}", paths.len());
    // The pump hands over only the newest clipboard write of a read (two in
    // one read set the clipboard once, to the second), so how many arrive
    // depends on reads; the clipboard's final contents must not.
    let observe_chunks = |chunks: Vec<&[u8]>| {
        let mut observed = observe_chunks(chunks);
        observed.clipboard = observed.clipboard.pop().into_iter().collect();
        observed
    };
    for path in paths {
        let name = path
            .file_name()
            .expect("name")
            .to_string_lossy()
            .into_owned();
        let stream = std::fs::read(&path).expect("fixture");
        let expected = observe_chunks(vec![&stream[..]]);
        assert_same(
            &expected,
            &observe_chunks(stream.chunks(1).collect()),
            &format!("{name} a byte at a time"),
        );
        // Every split for small inputs; for larger ones every split in the
        // first 256 bytes and 256 spread through the rest.
        let splits: Vec<usize> = if stream.len() <= 6_000 {
            (1..stream.len()).collect()
        } else {
            (1..256)
                .chain((0..256).map(|i| 256 + i * (stream.len() - 256) / 256))
                .collect()
        };
        for split in splits {
            assert_same(
                &expected,
                &observe_chunks(vec![&stream[..split], &stream[split..]]),
                &format!("{name} split at {split} of {}", stream.len()),
            );
        }
        let mut rng = Rng(stream.len() as u64 | 1);
        for round in 0..4 {
            assert_same(
                &expected,
                &observe_chunks(random_chunks(&stream, &mut rng, 4096)),
                &format!("{name} random reads, round {round}"),
            );
        }
    }
}

#[test]
fn can_or_sub_cancels_an_image_command_like_any_other_string() {
    // The parser drops an APC at CAN or SUB, executes the byte and goes on
    // with what follows. With images on, the Kitty command is cut out before
    // the parser sees it; the text after it must still print, and the DCS
    // query after that must be cancelled by its own SUB, not answered.
    let stream = b"\x1b_Gf=32,s=1,v=1;AAAA\x18tail\x1bP+q54\x1a!\x1b_Gi=3;AA\x1amore";
    let mut parser_only =
        HeadlessScreen::new_with_keyboard_enhancements(COLS, ROWS).with_host_replies("diri-test");
    parser_only.feed(stream);
    let with_images = observe_chunks([&stream[..]]);
    assert!(
        with_images.link_text.starts_with("tail!more"),
        "{:?}",
        with_images.link_text
    );
    assert!(
        parser_only.row_text(0).starts_with("tail!more"),
        "{:?}",
        parser_only.row_text(0)
    );
    let mut screen = screen();
    screen.feed(stream);
    assert_eq!(screen.row_text(0), parser_only.row_text(0));
    assert_eq!(screen.take_replies(), parser_only.take_replies());
    assert_eq!(screen.inline_image_bytes(), 0);
    for split in 1..stream.len() {
        assert_same(
            &with_images,
            &observe_chunks([&stream[..split], &stream[split..]]),
            &format!("split at {split}"),
        );
    }
}

#[test]
fn can_or_sub_ends_an_inline_image_osc_like_any_other_osc() {
    // The parser dispatches an OSC at CAN or SUB, then executes the byte. An
    // iTerm2 image ended that way is shown, and the text after it prints.
    let png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\0\x01\0\0\0\x01\x08\x06\0\0\0\x1f\x15\xc4\x89\0\0\0\rIDATx\x9cc\xf8\x0f\0\0\x01\x01\0\x05\x18\xd8N\0\0\0\0IEND\xaeB`\x82";
    let mut stream = b"\x1b]1337;File=inline=1:".to_vec();
    stream.extend_from_slice(base64(png).as_bytes());
    stream.extend_from_slice(b"\x18after");
    let expected = observe_chunks([&stream[..]]);
    assert!(expected.image_bytes > 0, "the image was stored");
    assert!(
        expected.link_text.contains("after"),
        "{:?}",
        expected.link_text
    );
    for split in 1..stream.len() {
        assert_same(
            &expected,
            &observe_chunks([&stream[..split], &stream[split..]]),
            &format!("split at {split}"),
        );
    }
}
