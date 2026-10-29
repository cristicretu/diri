//! Differential tests: the prototype must end in the state the current
//! core reaches on a byte-at-a-time parse, whatever the read sizes.

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};

use super::Core;
use super::check::compare;

fn reference(lines: usize, cols: usize, history: usize, input: &[u8]) -> Term<VoidListener> {
    let config = Config {
        scrolling_history: history,
        ..Config::default()
    };
    let mut term = Term::new(config, &TermSize::new(cols, lines), VoidListener);
    let mut parser = Processor::<StdSyncHandler>::new();
    for byte in input.chunks(1) {
        parser.advance(&mut term, byte);
    }
    term
}

/// A packed layout: four-row pages and a budget of `budget` bytes.
#[derive(Clone, Copy)]
struct Packing {
    budget: usize,
}

fn core(
    lines: usize,
    cols: usize,
    history: usize,
    reads: &[&[u8]],
    raw: bool,
    packing: Option<Packing>,
) -> Core {
    let mut core = match packing {
        None => Core::new(lines, cols, history),
        Some(p) => Core::with_layout(lines, cols, history, Some(p.budget), Some(4)),
    };
    core.raw_text = raw;
    let mut parser = Processor::<StdSyncHandler>::new();
    for read in reads {
        parser.advance(&mut core, read);
    }
    core
}

/// One read, one byte per read, every two-way split, and pseudo-random
/// reads, with and without the raw-bytes text path.
fn check(lines: usize, cols: usize, history: usize, input: &[u8]) {
    let want = reference(lines, cols, history, input);
    let mut splits: Vec<Vec<&[u8]>> = vec![vec![input], input.chunks(1).collect()];
    for at in (0..=input.len()).step_by((input.len() / 512).max(1)) {
        splits.push(vec![&input[..at], &input[at..]]);
    }
    let mut seed = 0x9E37_79B9_u32;
    for _ in 0..16 {
        let mut reads = Vec::new();
        let mut rest = input;
        while !rest.is_empty() {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            let n = (seed as usize % 97 + 1).min(rest.len());
            reads.push(&rest[..n]);
            rest = &rest[n..];
        }
        splits.push(reads);
    }
    for raw in [true, false] {
        for reads in &splits {
            let got = core(lines, cols, history, reads, raw, None);
            if let Err(error) = compare(&got, &want) {
                panic!(
                    "raw={raw} reads={:?}: {error}\ninput {:?}",
                    reads.iter().map(|r| r.len()).collect::<Vec<_>>(),
                    String::from_utf8_lossy(input)
                );
            }
        }
    }
    // Packed history under budgets that evict: lossless, and the history
    // kept is the same for every split of the input.
    let dense_page = 4 * (cols * 8 + 8);
    for budget in [dense_page / 2, 2 * dense_page, 6 * dense_page, 1 << 20] {
        let packing = Some(Packing { budget });
        let first = core(lines, cols, history, &splits[0], true, packing);
        let kept = first.history_size();
        let want = reference(lines, cols, kept, input);
        assert!(
            first.history_bytes() <= budget.max(2 * dense_page + 1024),
            "within budget"
        );
        for raw in [true, false] {
            for reads in &splits {
                let got = core(lines, cols, history, reads, raw, packing);
                if let Err(error) = compare(&got, &want) {
                    panic!(
                        "budget={budget} raw={raw} reads={:?}: {error}\ninput {:?}",
                        reads.iter().map(|r| r.len()).collect::<Vec<_>>(),
                        String::from_utf8_lossy(input)
                    );
                }
            }
        }
    }
}

/// The current core and v2 fed the same reads (one-shot, byte by byte,
/// pseudo-random) must agree. Unlike [`check`], this does not also demand
/// that the parser treat every split alike: on some corpus files it does
/// not (a parser matter for #927, not a grid one).
fn check_same_reads(lines: usize, cols: usize, history: usize, input: &[u8]) {
    let mut splits: Vec<Vec<&[u8]>> = vec![vec![input], input.chunks(1).collect()];
    let mut seed = 0x9E37_79B9_u32;
    for _ in 0..8 {
        let mut reads = Vec::new();
        let mut rest = input;
        while !rest.is_empty() {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            let n = (seed as usize % 97 + 1).min(rest.len());
            reads.push(&rest[..n]);
            rest = &rest[n..];
        }
        splits.push(reads);
    }
    for reads in &splits {
        let config = Config {
            scrolling_history: history,
            ..Config::default()
        };
        let mut want = Term::new(config, &TermSize::new(cols, lines), VoidListener);
        let mut parser = Processor::<StdSyncHandler>::new();
        for read in reads {
            parser.advance(&mut want, read);
        }
        for raw in [true, false] {
            let got = core(lines, cols, history, reads, raw, None);
            if let Err(error) = compare(&got, &want) {
                panic!(
                    "raw={raw} reads={:?}: {error}",
                    reads.iter().map(|r| r.len()).collect::<Vec<_>>()
                );
            }
        }
    }
}

#[test]
fn ascii_wrap_linefeed_and_history() {
    let mut input = Vec::new();
    for i in 0..60 {
        input.extend_from_slice(format!("line {i} {}\r\n", "x".repeat(i % 23)).as_bytes());
    }
    check(4, 10, 7, &input);
    check(4, 10, 0, &input);
    check(4, 10, 40, &input);
}

#[test]
fn wide_characters_at_the_row_end_and_overwritten() {
    let input = "漢字漢字漢\r\n123456789漢x\rab\x1b[2C漢字\r\x1b[1Cz\n\t\t漢\x08\x08yy".as_bytes();
    check(3, 10, 5, input);
    let long = "漢字かな交じり文、日本語の文章です。".repeat(12);
    check(4, 11, 20, long.as_bytes());
    check(4, 9, 3, long.as_bytes());
}

#[test]
fn clusters_marks_and_emoji() {
    let input = "e\u{301}x\u{0321}\u{0310} ❤\u{FE0F}❤ 👨\u{200D}👩\u{200D}👧 🇷🇴🇷 👍🏽 1\u{FE0F}\u{20E3} हिन्दी 각\u{1100}\u{1161}\r\n\
                 123456789❤\u{FE0F}ab 😀\u{200D}a \u{AD}z";
    for cols in [7, 9, 10, 13] {
        check(4, cols, 6, input.as_bytes());
    }
}

#[test]
fn invalid_utf8_c1_controls_and_del() {
    let input = b"ab\xC3(\xE4\xB8x\xF0\x9F\x98\xED\xA0\x80z\xC2\x85q\x7Fw\xC0\xAFk\xE4\xB8\xAD\x07\x0B\x0Cend";
    check(3, 8, 4, input);
}

#[test]
fn sgr_rep_tabs_and_line_erase() {
    let input = b"\x1b[1;31;48;5;20mred\x1b[4:3;58:2::1:2:3mcurl\x1b[mplain\x1b[100b\r\n\
                  \x1b[44mA\tB\t\tC\x1b[K\r\x1b[3C\x1b[1K\x1b[2K\x1b[7mrev\x1b[0m\x1b[10A\x1b[3E\
                  \x1b[10`a\x1b[5bZ\x1b[?1h\x1b[?1l\x1b[38:5:24;48:2:125:136:147mq\x1b[m\n\n\n\n\n\n";
    check(5, 12, 9, input);
    check(5, 12, 0, input);
}

#[test]
fn kitten_shaped_text_without_carriage_returns() {
    let mut input = Vec::new();
    for i in 0..200u32 {
        let width = (i * 37 % 140) as usize;
        input.extend((0..width).map(|k| b'!' + ((k as u32 * 7 + i) % 94) as u8));
        input.extend_from_slice(if i % 3 == 0 { b"\t\n" } else { b"\n" });
        if i % 5 == 0 {
            input.extend_from_slice("旦海司有幼雞讀松鼻種比門真目怪少：扒裝虎".as_bytes());
            input.extend_from_slice("c\u{321}\u{353}\u{310}X😀½«»\n".as_bytes());
        }
    }
    check(8, 40, 30, &input);
}

#[test]
fn text_rows_with_orphaned_halves_and_indents() {
    let input = "\x1b[5G漢字かな\x1b[6G\x1b[K\r\n\x1b[10Gabc\x1b[3Gxy\r\n\
                 \x1b[4G漢字\x1b[5G\x1b[1K\n\x1b[2G\t\tq\x1b[44m\x1b[K\x1b[m\n\
                 漢字漢字漢字\x1b[3Dw\x1b[1Gz\n"
        .repeat(6);
    check(4, 12, 40, input.as_bytes());
}

#[test]
fn marks_emoji_and_astral_on_the_text_path() {
    // Marks on narrow, wide and blank cells, at the row's end and at its
    // start (after a wrap), stacked, and after a spacer; emoji and Indic
    // consonants after plain cells and after clusters; astral characters
    // that never join; all through packed history.
    let input = "abcdefghi\u{301}\u{302}漢\u{301}x 👍😀\u{301}a\u{301}👍🏽\r\n\
                 \u{301}q ©\u{300}® ‼ क\u{94D}क e\u{301}😀 𠀀𠀁\u{301}𝐀 \u{200B}z\u{200B}\r\n\
                 1234567890\u{301}\u{302}\u{303}\r\n🇷\u{301}x 🏽\u{301}y \u{20E3}\u{301}w 🇷\u{FE0F}\u{301}\r\n\x1b[5C\u{301}\x1b[3C漢\u{302}\r\n"
        .repeat(5);
    for cols in [7, 10, 11, 16] {
        check(4, cols, 12, input.as_bytes());
    }
}

#[test]
fn scroll_regions_insert_and_delete_lines() {
    let mut input = Vec::new();
    for i in 0..12 {
        input.extend_from_slice(format!("row{i}\r\n").as_bytes());
    }
    input.extend_from_slice(
        b"\x1b[2;4r\x1b[4;1Hx\ny\nz\x1b[2;1H\x1bMup\x1bM\x1b[2L\x1b[1M\x1b[3S\x1b[2T\
          \x1b[r\x1b[1;3r\x1b[3;1Hq\n\n\nw\x1b[5S\x1b[r\x1b[?6h\x1b[2;3r\x1b[1;1Ho\x1b[5;9Hp\
          \x1b[?6l\x1b[H\x1b[3M\x1b[4L\x1b[r\x1b[6;1H\x1b[9L\x1b[9M",
    );
    check(6, 10, 20, &input);
    check(6, 10, 0, &input);
}

#[test]
fn character_insert_delete_and_erase() {
    let input = "\x1b[44mabcdefghijkl漢字mnop\r\x1b[3C\x1b[2@\x1b[1P\x1b[3X\x1b[m\
                 \r\n0123456789abcdefghij\x1b[1A\x1b[5G\x1b[2P\x1b[20P\r\n\x1b[7G漢\x1b[6G\x1b[@\
                 \x1b[2K漢字かなカナ漢字\x1b[3G\x1b[P\x1b[1;1H\x1b[3@z\x1b[99X\n";
    for cols in [8, 10, 13] {
        check(5, cols, 10, input.as_bytes());
    }
}

#[test]
fn erase_in_display_and_history() {
    let mut input = Vec::new();
    for i in 0..15 {
        input.extend_from_slice(format!("line {i}\r\n").as_bytes());
    }
    input.extend_from_slice(
        b"\x1b[3;4H\x1b[0J\x1b[2;2H\x1b[1Jtop\x1b[2J\x1b[Hafter\x1b[44m\x1b[2J\x1b[m",
    );
    input.extend_from_slice(b"\r\nmore\r\n\x1b[3J\x1b[1;1H\x1b[1J\x1b[2;1H\x1b[1J\x1b[J");
    check(5, 12, 30, &input);
    check(5, 12, 3, &input);
    // A blank screen clears without scrolling anything.
    check(4, 8, 10, b"\x1b[2J\x1b[2J ab\x1b[H\x1b[2J");
}

#[test]
fn alternate_screens_and_saved_cursors() {
    let input = b"primary\r\nsecond\x1b7\x1b[31m\x1b[?1049hALT\x1b[2;3Hx\x1b[?1049lback\x1b8!\
                  \x1b[?47hlegacy\x1b[?47l\x1b[?1047hclear\x1b[3;1Hme\x1b[?1047l\x1b[?1047h\
                  \x1b[?1049h\x1b[?1049l\x1b[?1047lend\x1b[?1049h\r\n\r\n\r\n\r\n\r\nscroll\x1b[?1049l\
                  \x1b[5;5H\x1b7\x1b[H\x1b8z";
    check(4, 10, 10, input);
}

#[test]
fn charsets_links_and_marks() {
    let input = b"\x1b(0lqqk\x1b(Bx\x0e\x1b)0lqk\x0fq\r\n\
                  \x1b]8;;https://a.example\x07link\x1b]8;id=7;https://b.example\x1b\\two\x1b]8;;\x07plain\
                  \x1b]8;;https://c.example\x07wrapping across the row end\x1b]8;;\x07\r\n\
                  \x1b]133;A\x07$ \x1b]133;B\x07ls\x1b]133;C\x07\r\nout\r\n\x1b]133;D;0\x07\x1b]133;D;3\x07\
                  \x1b]133;D\x07\x1b]133;A\x07\r\n\r\n\r\n\r\n\r\n";
    check(4, 12, 20, input);
}

#[test]
fn insert_mode_tabs_and_resets() {
    let input = "abcdefgh\r\x1b[4hXY漢\x1b[2Gz\x1b[4l\r\n\x1b[3G\x1bH\x1b[7G\x1bH\r\t.\t.\x1b[g\t,\x1b[3g\
                 \r\t;\x1b[2I:\x1b[2Z!\r\n\x1b[20h\x1bElnm\x1b[20l\x1b#8\x1b[?3h\x1b[?3l\x1b[!pafter\
                 \x1b[?25l\x1b[?1000h\x1b[?1006h\x1b[?2004h\x1b[?1h\x1b=\x1bcreset\x1b[?7lnowrap and long text";
    check(4, 10, 10, input.as_bytes());
}

/// Tokens of every family above, for the random tests.
const TOKENS: &[&str] = &[
    "abc",
    "hello world ",
    "漢字",
    "e\u{301}",
    "❤\u{FE0F}",
    "👍🏽",
    "\t",
    "\r",
    "\n",
    "\x08",
    "\x1b[2;4r",
    "\x1b[r",
    "\x1bM",
    "\x1b[2L",
    "\x1b[M",
    "\x1b[2S",
    "\x1b[T",
    "\x1b[3@",
    "\x1b[2P",
    "\x1b[4X",
    "\x1b[J",
    "\x1b[1J",
    "\x1b[2J",
    "\x1b[3J",
    "\x1b[K",
    "\x1b[1K",
    "\x1b[?1049h",
    "\x1b[?1049l",
    "\x1b[?47h",
    "\x1b[?47l",
    "\x1b[?1047h",
    "\x1b[?1047l",
    "\x1b7",
    "\x1b8",
    "\x1b(0",
    "\x1b(B",
    "\x1b]8;;u://x\x07",
    "\x1b]8;;\x07",
    "\x1b]133;A\x07",
    "\x1b]133;D;1\x07",
    "\x1b[4h",
    "\x1b[4l",
    "\x1b[?6h",
    "\x1b[?6l",
    "\x1b[?7l",
    "\x1b[?7h",
    "\x1b[H",
    "\x1b[3;5H",
    "\x1b[2A",
    "\x1b[3C",
    "\x1bH",
    "\x1b[g",
    "\x1b[31;44m",
    "\x1b[m",
    "\x1b[5b",
    "\x1b#8",
    "\x1b[!p",
    "\x1bE",
    "\x1b]8;id=1;u://y\x07",
    "\x1b[?3h",
    "\x1bc",
    "\x0e",
    "\x0f",
    "\x1b)0",
    "\x1b[2I",
    "\x1b[Z",
    "\x1b[20h",
    "\x1b[20l",
    "\x1b[?2027h",
    "\x1b[?2027l",
    "\x1b[@",
    "\x1b[2;9r",
    "\x1b]133;B\x07",
    "\x1b]133;C\x07",
    "\x1b]133;D\x07",
    "\x1b[?1000h",
    "\x1b[?1006h",
    "\x1b[3g",
    "\x1b[?1016h",
    "\x1b[?1005h",
    "\x1b[?1016l",
    "🇷🇴",
    "\u{1F3FB}",
    "\x1b[1;1H\x1b[2J",
    "lqqk",
];

/// Random mixes of every family above, against the current core.
#[test]
fn random_sequences_match_the_current_core() {
    let mut seed = 0x2545_F491_u32;
    let mut next = || {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        seed
    };
    // `CORE_V2_FUZZ=<n>` runs n inputs instead of 24.
    let runs = std::env::var("CORE_V2_FUZZ").map_or(24, |n| n.parse().expect("count"));
    for run in 0..runs {
        let mut input = String::new();
        for _ in 0..60 {
            input.push_str(TOKENS[next() as usize % TOKENS.len()]);
        }
        let (lines, cols) = [(5, 9), (3, 4), (7, 16)][run % 3];
        check(lines, cols, 12, input.as_bytes());
    }
}

/// A cluster that widens at the right margin moves to the next row with
/// its hyperlink (it was read after the leading spacer overwrote it).
#[test]
fn a_cluster_widening_at_the_margin_keeps_its_link() {
    for cols in [3, 4, 5] {
        check(3, cols, 6, "ab\x1b]8;;u://x\x07🇴\x1b]8;;\x07🇴".as_bytes());
        check(
            3,
            cols,
            6,
            "a\x1b]8;id=7;u://y\x07b❤\x1b]8;;\x07\u{FE0F}x\x1b]8;;u://z\x07🇷🇴\x1b]8;;\x07"
                .as_bytes(),
        );
    }
}

/// Hyperlinks (explicit ids and none) over clusters that widen and narrow
/// at the margins, wide characters, wraps and scrolls, at narrow widths.
#[test]
fn random_links_over_clusters_and_margins() {
    const TOKENS: &[&str] = &[
        "a",
        "bc",
        "漢",
        "🇴",
        "🇷",
        "❤",
        "\u{FE0F}",
        "\u{FE0E}",
        "\u{200D}",
        "👍",
        "\u{1F3FD}",
        "e\u{301}",
        "\r\n",
        "\n",
        "\x1b]8;;u://x\x07",
        "\x1b]8;id=1;u://y\x07",
        "\x1b]8;id=2;u://y\x07",
        "\x1b]8;;\x07",
        "\x1b[?2027h",
        "\x1b[?2027l",
        "\x1b[?7l",
        "\x1b[?7h",
        "\x1b[2D",
        "\x1b[K",
    ];
    let mut seed = 0x9E37_79B9_u32;
    let mut next = || {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        seed
    };
    let runs = std::env::var("CORE_V2_FUZZ").map_or(48, |n| n.parse().expect("count"));
    for run in 0..runs {
        let mut input = String::new();
        for _ in 0..50 {
            input.push_str(TOKENS[next() as usize % TOKENS.len()]);
        }
        let (lines, cols) = [(3, 3), (4, 4), (3, 5), (5, 7)][run % 4];
        check(lines, cols, 8, input.as_bytes());
    }
}

/// `CORE_V2_BISECT=<file> CORE_V2_SIZE=lines,cols,history`: the shortest
/// prefix of the file's bytes on which v2 and the current core differ.
#[test]
#[ignore]
fn bisect_first_difference() {
    let input = std::fs::read(std::env::var("CORE_V2_BISECT").expect("file")).expect("read");
    let size: Vec<usize> = std::env::var("CORE_V2_SIZE")
        .expect("size")
        .split(',')
        .map(|n| n.parse().expect("number"))
        .collect();
    for end in 1..=input.len() {
        let prefix = &input[..end];
        let want = reference(size[0], size[1], size[2], prefix);
        let got = core(size[0], size[1], size[2], &[prefix], true, None);
        if let Err(error) = compare(&got, &want) {
            let before = &input[..end - 1];
            let dump = |label: &str, bytes: &[u8]| {
                let want = reference(size[0], size[1], size[2], bytes);
                let got = core(size[0], size[1], size[2], &[bytes], true, None);
                let grid = want.grid();
                eprintln!(
                    "{label}: cursor got {:?} want {:?}",
                    (got.line, got.col, got.wrap_pending),
                    (grid.cursor.point, grid.cursor.input_needs_wrap)
                );
                for line in 0..size[0] as i32 {
                    let row: String = (0..size[1])
                        .map(|c| {
                            let w = &grid[alacritty_terminal::index::Line(line)]
                                [alacritty_terminal::index::Column(c)];
                            let cell = got.cell(line, c);
                            let style = got.style_of(line, cell);
                            let f = super::check::flags_of(&got, line, c, cell, &style);
                            format!(
                                "[{}{:x}|{}{:x}]",
                                w.c,
                                w.flags.bits() & 0xFFFF,
                                cell.ch(),
                                f.bits() & 0xFFFF
                            )
                        })
                        .collect();
                    eprintln!("  {line}: {row}");
                }
            };
            if std::env::var_os("CORE_V2_BISECT_RAW").is_some() {
                let got = core(size[0], size[1], size[2], &[prefix], true, None);
                for c in 0..4 {
                    eprintln!(
                        "raw line {} col {c}: {:#x}",
                        got.line,
                        got.cell(got.line as i32, c).0
                    );
                }
            } else {
                dump("before", before);
                dump("after", prefix);
            }
            panic!(
                "first difference after {end} bytes: {:?}\n{error}",
                String::from_utf8_lossy(prefix)
            );
        }
    }
}

/// Every file of the adversarial corpus (`tests/fixtures/adversarial`, or
/// `CORE_V2_CORPUS`), when present: v2 must match the current core on each
/// at every split.
#[test]
fn adversarial_corpus_matches_the_current_core() {
    let dir = std::env::var("CORE_V2_CORPUS").unwrap_or_else(|_| {
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/adversarial").to_owned()
    });
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    let mut files: Vec<_> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "bin"))
        .collect();
    files.sort();
    for path in files {
        let input = std::fs::read(&path).expect("fixture");
        let input = &input[..input.len().min(4096)];
        eprintln!("{}", path.display());
        check_same_reads(24, 80, 100, input);
    }
}

/// Events as comparable text: closures are applied to fixed inputs.
fn event_text(event: &alacritty_terminal::event::Event) -> String {
    use alacritty_terminal::event::{Event, WindowSize};
    use alacritty_terminal::vte::ansi::Rgb;
    let size = WindowSize {
        num_lines: 24,
        num_cols: 80,
        cell_width: 9,
        cell_height: 18,
    };
    match event {
        Event::ClipboardLoad(kind, reply) => format!("load {kind:?} {}", reply("pasted")),
        Event::ColorRequest(index, reply) => {
            format!("color {index} {}", reply(Rgb { r: 1, g: 2, b: 3 }))
        }
        Event::TextAreaSizeRequest(reply) => format!("area {}", reply(size)),
        Event::CellSizeRequest(reply) => format!("cell {}", reply(size)),
        other => format!("{other:?}"),
    }
}

#[derive(Clone, Default)]
struct Recorder(std::sync::Arc<std::sync::Mutex<Vec<String>>>);

impl alacritty_terminal::event::EventListener for Recorder {
    fn send_event(&self, event: alacritty_terminal::event::Event) {
        self.0.lock().unwrap().push(event_text(&event));
    }
}

#[test]
fn events_titles_colors_replies_keyboard_and_cursor_style() {
    use alacritty_terminal::term::Osc52;
    let input = b"\x1b]0;first\x07\x1b[22;0t\x1b]2;second\x1b\\\x1b[23;0t\x1b]0;\x07\
                  \x1b]4;1;rgb:10/20/30\x07\x1b]10;?\x07\x1b]11;rgb:ff/00/00\x07\x1b]104;1\x07\x1b]110\x07\
                  \x1b]52;c;aGVsbG8=\x07\x1b]52;p;?\x07\x1b]52;x;Zg==\x07\
                  \x1b[c\x1b[>c\x1b[5n\x1b[3;4H\x1b[6n\x1b[?6n\x1b[?996n\x1b[14t\x1b[16t\x1b[18t\x1b[19t\x1b[>q\
                  \x1bP+q544E;436F;626164\x1b\\\x1b[1;31;48;5;20;58:2::1:2:3m\x1bP$qm\x1b\\\x1b[2;5r\x1bP$qr\x1b\\\
                  \x1b[5 q\x1bP$q q\x1b\\\x1b[?12l\x1b[?12$p\x1b[?1016h\x1b[?1016$p\x1b[?1006h\x1b[?1016$p\x1b[?1049$p\x1b[4$p\x1b[99$p\x1b[?7$p\x1b[?69$p\
                  \x1b[>1u\x1b[>5u\x1b[?u\x1b[=3;2u\x1b[?u\x1b[<u\x1b[?u\x1b[?1049h\x1b[>2u\x1b[?u\x1b[?1049l\x1b[?u\
                  \x07\x1b[?1000h\x1b[?1000l\x1b[?2048h\x1b[?12h\x1b[!p\x1b]0;again\x07\x1b]4;2;rgb:01/02/03\x07\x1bc";
    let config = Config {
        scrolling_history: 10,
        kitty_keyboard: true,
        osc52: Osc52::CopyPaste,
        terminal_version: Some("diri 1.0".into()),
        ..Config::default()
    };
    for reads in [
        vec![&input[..]],
        input.chunks(1).collect::<Vec<_>>(),
        input.chunks(7).collect(),
    ] {
        let want_events = Recorder::default();
        let mut want = Term::new(config.clone(), &TermSize::new(20, 6), want_events.clone());
        let mut parser = Processor::<StdSyncHandler>::new();
        for read in &reads {
            parser.advance(&mut want, read);
        }
        let got_events = Recorder::default();
        let mut got = Core::new(6, 20, 10);
        got.set_events(config.clone(), Box::new(got_events.clone()));
        let mut parser = Processor::<StdSyncHandler>::new();
        for read in &reads {
            parser.advance(&mut got, read);
        }
        assert_eq!(
            *got_events.0.lock().unwrap(),
            *want_events.0.lock().unwrap()
        );
        assert!(
            want_events.0.lock().unwrap().len() > 30,
            "the input exercises events"
        );
        compare(&got, &want).unwrap();
        assert_eq!(got.colors_generation(), want.colors_generation());
        for index in 0..alacritty_terminal::term::color::COUNT {
            assert_eq!(got.colors()[index], want.colors()[index], "color {index}");
        }
        assert_eq!(
            format!("{:?}", got.cursor_style()),
            format!("{:?}", want.cursor_style())
        );
    }
}

/// Tokens that reflow has to get right: long soft-wrapped runs, wide
/// characters landing on row ends, clusters, links and marks across wraps,
/// styled blanks, explicit spaces before the cursor, and the alternate screen.
const RESIZE_TOKENS: &[&str] = &[
    "abcdefghijklmnopqrstuvwxyz0123456789",
    "漢字漢字漢字漢字漢字漢字漢",
    "a漢b字c漢d字e",
    "x漢",
    "e\u{301}e\u{301}e\u{301}e\u{301}e\u{301}e\u{301}e\u{301}",
    "👨\u{200D}👩\u{200D}👧👍🏽❤\u{FE0F}🇷🇴",
    "\x1b]8;;u://wrap\x07link text that wraps around\x1b]8;;\x07",
    "\x1b]133;A\x07$ \x1b]133;B\x07command\x1b]133;C\x07\r\n",
    "\x1b[44m      \x1b[m",
    "\x1b[4;58:5:3mund\x1b[m",
    "        ",
    "\r\n",
    "\r\n\r\n\r\n",
    "\x1b[K",
    "\x1b[?1049h",
    "\x1b[?1049l",
    "\x1b[?47h",
    "\x1b[?47l",
    "\x1b7",
    "\x1b8",
    "\x1b[2;3r",
    "\x1b[5;9H",
    "\x1b[1;1H",
    "\x1bH",
    "\x1b[3g",
    "\x1b[?7l",
    "\x1b[?7h",
    "\x1b[3P",
    "\x1b[2@",
];

/// Applies `step` to the current core and to v2 alike.
enum Step<'a> {
    Feed(&'a [u8]),
    Resize(usize, usize),
}

/// The primary screen's history origin, while it is shown (the alternate
/// screen keeps no history) and keeps any.
fn compare_origin(got: &Core, want: &Term<VoidListener>) -> Result<(), String> {
    let alt = got
        .mode
        .contains(alacritty_terminal::term::TermMode::ALT_SCREEN);
    if !alt && got.pages.history_limit != 0 {
        let origin = want.grid().history_origin();
        if got.pages.history_origin() != origin {
            return Err(format!(
                "history origin {}, reference {origin}",
                got.pages.history_origin()
            ));
        }
    }
    Ok(())
}

/// Everything a resize touches beyond what [`compare`] reads: the saved
/// cursor, the history origin, and (through probes fed to both cores
/// afterwards) the other screen, its cursors, tab stops and the scroll
/// region.
fn compare_resized(
    got: &mut Core,
    want: &mut Term<VoidListener>,
    parsers: &mut (Processor<StdSyncHandler>, Processor<StdSyncHandler>),
) -> Result<(), String> {
    compare(got, want)?;
    let saved = want.grid().saved_cursor.point;
    if (got.saved.line, got.saved.col) != (saved.line.0 as usize, saved.column.0) {
        return Err(format!(
            "saved cursor {:?}, reference {saved:?}",
            (got.saved.line, got.saved.col)
        ));
    }
    compare_origin(got, want)?;
    let alt = got
        .mode
        .contains(alacritty_terminal::term::TermMode::ALT_SCREEN);
    let (lines, cols) = (got.lines(), got.cols());
    let swap: &[u8] = if alt { b"\x1b[?1049l" } else { b"\x1b[?47h" };
    let tabs = format!("\r{}", "\t*".repeat(cols));
    let region = format!("\x1b[1;1H{}z", "\n".repeat(lines + 1));
    for probe in [
        b"\x1b8" as &[u8],
        swap,
        b"\x1b8",
        tabs.as_bytes(),
        region.as_bytes(),
    ] {
        parsers.0.advance(got, probe);
        parsers.1.advance(want, probe);
        compare(got, want).map_err(|e| format!("after probe {probe:?}: {e}"))?;
    }
    Ok(())
}

/// Runs `steps` on the current core (byte at a time) and on v2 (in `reads`
/// of each feed: whole, byte by byte or pseudo-random), comparing after
/// every resize and, with probes, at the end.
fn check_resizes(lines: usize, cols: usize, history: usize, steps: &[Step], variant: usize) {
    let config = Config {
        scrolling_history: history,
        ..Config::default()
    };
    let mut want = Term::new(config, &TermSize::new(cols, lines), VoidListener);
    let mut got = match variant % 3 {
        // Packed pages of four rows, with a budget that evicts nothing.
        2 => Core::with_layout(lines, cols, history, Some(1 << 24), Some(4)),
        _ => Core::new(lines, cols, history),
    };
    got.raw_text = variant.is_multiple_of(2);
    let mut parsers = (Processor::<StdSyncHandler>::new(), Processor::new());
    let mut seed = 0x1234_5679_u32 ^ variant as u32;
    let mut log = String::new();
    for (index, step) in steps.iter().enumerate() {
        match step {
            Step::Feed(bytes) => {
                log.push_str(&String::from_utf8_lossy(bytes));
                for byte in bytes.chunks(1) {
                    parsers.1.advance(&mut want, byte);
                }
                let mut rest = *bytes;
                while !rest.is_empty() {
                    let n = match variant % 4 {
                        0 => rest.len(),
                        1 => 1,
                        _ => {
                            seed ^= seed << 13;
                            seed ^= seed >> 17;
                            seed ^= seed << 5;
                            (seed as usize % 13 + 1).min(rest.len())
                        }
                    };
                    parsers.0.advance(&mut got, &rest[..n]);
                    rest = &rest[n..];
                }
            }
            &Step::Resize(lines, cols) => {
                log.push_str(&format!("<resize {lines}x{cols}>"));
                want.resize(TermSize::new(cols, lines));
                got.resize(lines, cols);
                if let Err(error) = compare(&got, &want) {
                    panic!("variant {variant} step {index}: {error}\nsteps {log:?}");
                }
            }
        }
        if let Err(error) = compare_origin(&got, &want) {
            panic!("variant {variant} step {index}: {error}\nsteps {log:?}");
        }
    }
    if let Err(error) = compare_resized(&mut got, &mut want, &mut parsers) {
        panic!("variant {variant} at the end: {error}\nsteps {log:?}");
    }
}

#[test]
fn resizes_reflow_like_the_current_core() {
    let text = "first line that is long enough to wrap\r\n漢字漢字漢字かな\r\n\
                e\u{301}\u{301}x 👍🏽 tail\x1b]8;;u://l\x07linked words\x1b]8;;\x07 end";
    for variant in 0..6 {
        let steps = [
            Step::Feed(text.as_bytes()),
            Step::Resize(4, 7),
            Step::Resize(4, 3),
            Step::Resize(6, 2),
            Step::Resize(3, 2),
            Step::Resize(5, 9),
            Step::Feed(text.as_bytes()),
            Step::Resize(2, 31),
            Step::Resize(7, 5),
            Step::Feed(b"\x1b[?1049hALT screen text\x1b[2;2H"),
            Step::Resize(3, 4),
            Step::Resize(6, 12),
            Step::Feed(b"\x1b[?1049l"),
            Step::Resize(5, 11),
        ];
        check_resizes(5, 10, 20, &steps, variant);
        // The cursor on a continuation row that widening pulls up while
        // content stays after it: rows below it are dropped to keep it.
        let steps = [
            Step::Feed(b"h1\r\nh2\r\nh3\r\n0123456789abcdef\r\nnext\r\nmore\r\nlast\x1b[2;3H"),
            Step::Resize(5, 14),
            Step::Feed(b"\x1b[3;2H"),
            Step::Resize(5, 6),
            Step::Resize(4, 20),
        ];
        check_resizes(5, 10, 20, &steps, variant);
    }
}

/// Random output interleaved with random resizes (rows and columns growing
/// and shrinking, down to two columns), against the current core. One
/// column is [`random_narrow_resizes_match_the_current_core`]: with a wide
/// character on the screen or in history the current core never finishes
/// narrowing to one column.
#[test]
fn random_resizes_match_the_current_core() {
    random_resizes(0x6C07_8965, 48, 2, &[RESIZE_TOKENS, TOKENS]);
}

/// [`random_resizes_match_the_current_core`] down to one column, with only
/// narrow characters (clusters included).
#[test]
fn random_narrow_resizes_match_the_current_core() {
    let narrow = |tokens: &[&'static str]| -> Vec<&'static str> {
        tokens
            .iter()
            .copied()
            .filter(|token| token.chars().all(|c| (c as u32) < 0x1100))
            .collect()
    };
    let (resize_tokens, tokens) = (narrow(RESIZE_TOKENS), narrow(TOKENS));
    random_resizes(0x0BAD_5EED, 24, 1, &[&resize_tokens, &tokens]);
}

/// A wide character narrowed to one column stays there, its spacer on the
/// next row (the current core does not finish this resize), and widening
/// again keeps every character with a spacer after it.
#[test]
fn a_wide_character_narrowed_to_one_column_stays() {
    let mut core = Core::new(3, 4, 10);
    let mut parser = Processor::<StdSyncHandler>::new();
    parser.advance(&mut core, "a漢\r\n漢字".as_bytes());
    let text = |core: &Core| -> String {
        (-(core.history_size() as i32)..core.lines() as i32)
            .flat_map(|line| (0..core.cols()).map(move |col| (line, col)))
            .map(|(line, col)| core.cell(line, col))
            .filter(|cell| !cell.has(super::cell::SPACER | super::cell::LEADING))
            .map(|cell| cell.ch())
            .filter(|c| *c != ' ')
            .collect()
    };
    core.resize(3, 1);
    assert_eq!(text(&core), "a漢漢字");
    core.resize(3, 4);
    assert_eq!(text(&core), "a漢漢字");
    for line in -(core.history_size() as i32)..3 {
        for col in 0..4 {
            if core.cell(line, col).has(super::cell::WIDE) {
                assert!(col + 1 < 4 && core.cell(line, col + 1).has(super::cell::SPACER));
            }
        }
    }
}

/// `runs` random mixes of `tokens` (each feed draws from one of the sets)
/// with eight resizes each, to at least `min_cols` columns.
fn random_resizes(mut seed: u32, runs: usize, min_cols: usize, tokens: &[&[&str]]) {
    let mut next = || {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        seed as usize
    };
    // `CORE_V2_FUZZ=<n>` runs n inputs instead.
    let runs = std::env::var("CORE_V2_FUZZ").map_or(runs, |n| n.parse().expect("count"));
    for run in 0..runs {
        let mut feeds = Vec::new();
        let mut sizes = Vec::new();
        for _ in 0..8 {
            let mut feed = String::new();
            for _ in 0..next() % 12 {
                let set = tokens[next() % tokens.len()];
                feed.push_str(set[next() % set.len()]);
            }
            feeds.push(feed);
            let cols = if next() % 3 == 0 {
                min_cols + next() % 4
            } else {
                min_cols + next() % 24
            };
            sizes.push((1 + next() % 9, cols));
        }
        let mut steps = Vec::new();
        for (feed, &(lines, cols)) in feeds.iter().zip(&sizes) {
            steps.push(Step::Feed(feed.as_bytes()));
            steps.push(Step::Resize(lines, cols));
        }
        steps.push(Step::Feed(b"tail text after the last resize"));
        let history = [0, 3, 40][run % 3];
        let (lines, cols) = [(5, 9), (3, 4), (7, 16)][run / 3 % 3];
        check_resizes(lines, cols, history, &steps, run);
    }
}

/// `HeadlessScreen::resize` at capacity (#902) on both cores: the row limit
/// is raised to `RESIZE_ROW_ALLOWANCE` (4) times `base` for the re-wrap,
/// then held at the rows it produced (at least `base`), and output then
/// lowers it as `settle` does when history shrinks below it. Neither core
/// may drop a retained row, and v2 must not bring back one the limit hid.
#[test]
fn a_resize_at_capacity_keeps_history_like_the_current_core() {
    fn set_limit(got: &mut Core, want: &mut Term<VoidListener>, limit: usize) {
        want.set_scrolling_history(limit);
        got.primary_pages_mut().set_history_limit(limit);
    }
    fn settle(got: &mut Core, want: &mut Term<VoidListener>, base: usize) {
        let limit = want.config().scrolling_history;
        let history = want.primary_history_size();
        if limit > base && history < limit {
            set_limit(got, want, history.max(base));
        }
    }
    fn same(got: &Core, want: &Term<VoidListener>, what: &str) {
        if let Err(error) = compare(got, want).and_then(|()| compare_origin(got, want)) {
            panic!("{what}: {error}");
        }
        assert_eq!(got.history_size(), want.primary_history_size(), "{what}");
    }
    let base = 20;
    let line = |n: usize, width: usize| {
        let text = format!("L{n:03}{}", "abcdefghij".repeat(4));
        format!("{}\r\n", &text[..width])
    };
    for variant in 0..4 {
        let (lines, cols) = (5, 12);
        let config = Config {
            scrolling_history: base,
            ..Config::default()
        };
        let mut want = Term::new(config, &TermSize::new(cols, lines), VoidListener);
        let mut got = match variant % 2 {
            // Packed four-row pages under a budget that evicts nothing.
            0 => Core::with_layout(lines, cols, base, Some(1 << 24), Some(4)),
            _ => Core::new(lines, cols, base),
        };
        got.raw_text = variant >= 2;
        let mut parsers = (
            Processor::<StdSyncHandler>::new(),
            Processor::<StdSyncHandler>::new(),
        );
        let mut feed = |got: &mut Core, want: &mut Term<VoidListener>, bytes: &[u8]| {
            parsers.0.advance(got, bytes);
            for byte in bytes.chunks(1) {
                parsers.1.advance(want, byte);
            }
            settle(got, want, base);
        };
        // Past capacity, unwrapped lines and lines wrapping at 12 columns.
        let fill: String = (0..60).map(|n| line(n, [10, 18][n % 2])).collect();
        feed(&mut got, &mut want, fill.as_bytes());
        same(&got, &want, "filled");
        assert_eq!(got.history_size(), base);
        for (step, (lines, cols)) in [(5, 6), (5, 20), (5, 4), (7, 9), (3, 12), (5, 12)]
            .into_iter()
            .enumerate()
        {
            set_limit(&mut got, &mut want, base * 4);
            want.resize(TermSize::new(cols, lines));
            got.resize(lines, cols);
            let kept = want.primary_history_size().clamp(base, base * 4);
            set_limit(&mut got, &mut want, kept);
            settle(&mut got, &mut want, base);
            let what = format!("variant {variant} step {step}: {lines}x{cols}");
            same(&got, &want, &what);
            // Output after the resize recycles the oldest rows alike.
            let more: String = (100 + step * 3..103 + step * 3)
                .map(|n| line(n, 7))
                .collect();
            feed(&mut got, &mut want, more.as_bytes());
            same(&got, &want, &format!("{what}, then output"));
        }
    }
}
