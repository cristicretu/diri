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

/// Random mixes of every family above, against the current core.
#[test]
fn random_sequences_match_the_current_core() {
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
        "🇷🇴",
        "\u{1F3FB}",
        "\x1b[1;1H\x1b[2J",
        "lqqk",
    ];
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
