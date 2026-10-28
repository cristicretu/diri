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
