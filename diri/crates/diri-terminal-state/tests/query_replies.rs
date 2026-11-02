//! Replies a program gets from a local terminal, one query at a time and in
//! the order programs rely on. Every expected byte string here is what a
//! reference libghostty-based multiplexer answers for the same query, except
//! where the comment says Diri reports its own name or palette.

use diri_proto::grid::TermColor;
use diri_proto::{TerminalAppearance, TerminalPalette};
use diri_terminal_state::{DEFAULT_CELL_PIXELS, HeadlessScreen};

const VERSION: &str = "diri 0.0.0-test";

const DARK: TerminalPalette = TerminalPalette {
    appearance: TerminalAppearance::Dark,
    foreground: [0xe6, 0xe6, 0xe6],
    background: [0x12, 0x13, 0x18],
    cursor: [0xe6, 0xe6, 0xe5],
    ansi: [
        [0x00, 0x00, 0x00],
        [0xcd, 0x31, 0x31],
        [0x0d, 0xbc, 0x79],
        [0xe5, 0xe5, 0x10],
        [0x24, 0x72, 0xc8],
        [0xbc, 0x3f, 0xbc],
        [0x11, 0xa8, 0xcd],
        [0xe5, 0xe5, 0xe5],
        [0x66, 0x66, 0x66],
        [0xf1, 0x4c, 0x4c],
        [0x23, 0xd1, 0x8b],
        [0xf5, 0xf5, 0x43],
        [0x3b, 0x8e, 0xea],
        [0xd6, 0x70, 0xd6],
        [0x29, 0xb8, 0xdb],
        [0xff, 0xff, 0xff],
    ],
};

const LIGHT: TerminalPalette = TerminalPalette {
    appearance: TerminalAppearance::Light,
    foreground: [0x20, 0x20, 0x20],
    background: [0xff, 0xff, 0xff],
    ..DARK
};

/// The screen a local Engine session runs, as `session.rs` builds it.
fn local() -> HeadlessScreen {
    let mut screen = HeadlessScreen::new_with_keyboard_enhancements(80, 24)
        .with_notifications()
        .with_inline_images()
        .with_host_replies(VERSION);
    screen.set_palette(Some(DARK));
    screen
}

/// The remote Holder's screen, as `diri-remote` builds it.
fn holder() -> HeadlessScreen {
    HeadlessScreen::new(80, 24)
        .with_inline_images()
        .answering_image_queries_only()
}

fn reply(screen: &mut HeadlessScreen, query: &str) -> String {
    screen.feed(query.as_bytes());
    String::from_utf8(screen.take_replies()).unwrap()
}

#[test]
fn xtversion_reports_the_terminal_name_and_version() {
    let mut screen = local();
    assert_eq!(
        reply(&mut screen, "\x1b[>0q"),
        "\x1bP>|diri 0.0.0-test\x1b\\"
    );
    assert_eq!(
        reply(&mut screen, "\x1b[>q"),
        "\x1bP>|diri 0.0.0-test\x1b\\"
    );
    // Only parameter 0 is XTVERSION.
    assert_eq!(reply(&mut screen, "\x1b[>1q"), "");
}

#[test]
fn kitty_keyboard_query_answers_and_flags_stack() {
    let mut screen = local();
    assert_eq!(reply(&mut screen, "\x1b[?u"), "\x1b[?0u");
    assert_eq!(reply(&mut screen, "\x1b[>5u\x1b[?u"), "\x1b[?5u");
    assert_eq!(reply(&mut screen, "\x1b[>1u\x1b[?u"), "\x1b[?1u");
    assert_eq!(reply(&mut screen, "\x1b[=3;2u\x1b[?u"), "\x1b[?3u");
    assert_eq!(reply(&mut screen, "\x1b[<u\x1b[?u"), "\x1b[?5u");
    assert_eq!(reply(&mut screen, "\x1b[<u\x1b[?u"), "\x1b[?0u");
    // Flags survive a resize; a resize used to rebuild the parser options
    // from defaults and silently turn negotiation off.
    screen.feed(b"\x1b[>7u");
    screen.resize(100, 30);
    assert_eq!(reply(&mut screen, "\x1b[?u"), "\x1b[?7u");
    assert_eq!(
        reply(&mut screen, "\x1b[>0q"),
        "\x1bP>|diri 0.0.0-test\x1b\\"
    );
}

#[test]
fn xtgettcap_answers_each_capability_in_order() {
    let mut screen = local();
    let cases = [
        ("TN", "\x1bP1+r544E=787465726D2D323536636F6C6F72\x1b\\"),
        ("Co", "\x1bP1+r436F=323536\x1b\\"),
        ("colors", "\x1bP1+r636F6C6F7273=323536\x1b\\"),
        ("RGB", "\x1bP1+r524742=38\x1b\\"),
        ("Tc", "\x1bP1+r5463\x1b\\"),
        ("Smulx", "\x1bP1+r536D756C78=5C455B343A25703125646D\x1b\\"),
        ("Ss", "\x1bP1+r5373=5C455B25703125642071\x1b\\"),
        ("Se", "\x1bP1+r5365=5C455B322071\x1b\\"),
        ("kbs", "\x1bP1+r6B6273=7F\x1b\\"),
        ("bogus", "\x1bP0+r626F677573\x1b\\"),
    ];
    for (name, expected) in cases {
        let hex: String = name.bytes().map(|byte| format!("{byte:02X}")).collect();
        assert_eq!(
            reply(&mut screen, &format!("\x1bP+q{hex}\x1b\\")),
            expected,
            "{name}"
        );
        // Requesters may spell the hex in lower case.
        let lower = hex.to_ascii_lowercase();
        assert_eq!(
            reply(&mut screen, &format!("\x1bP+q{lower}\x1b\\")),
            expected,
            "{name}"
        );
    }
    // Several names in one request: one reply each, in request order.
    assert_eq!(
        reply(&mut screen, "\x1bP+q524742;5463;436F\x1b\\"),
        "\x1bP1+r524742=38\x1b\\\x1bP1+r5463\x1b\\\x1bP1+r436F=323536\x1b\\"
    );
    // setrgbf, which Neovim reads to choose truecolor.
    assert_eq!(
        reply(&mut screen, "\x1bP+q73657472676266\x1b\\"),
        format!(
            "\x1bP1+r73657472676266={}\x1b\\",
            "\\E[38;2;%p1%d;%p2%d;%p3%dm"
                .bytes()
                .map(|byte| format!("{byte:02X}"))
                .collect::<String>()
        )
    );
    // An oversized request is dropped, not truncated into a wrong answer.
    let huge = "41".repeat(4096);
    assert_eq!(reply(&mut screen, &format!("\x1bP+q{huge}\x1b\\")), "");
}

#[test]
fn decrqss_reports_sgr_margins_and_cursor_style() {
    let mut screen = local();
    assert_eq!(reply(&mut screen, "\x1bP$qm\x1b\\"), "\x1bP1$r0m\x1b\\");
    assert_eq!(
        reply(&mut screen, "\x1b[1;31m\x1bP$qm\x1b\\\x1b[0m"),
        "\x1bP1$r0;1;31m\x1b\\"
    );
    assert_eq!(
        reply(
            &mut screen,
            "\x1b[3;4:3;38;5;208;48;2;1;2;3;58:2::9:8:7m\x1bP$qm\x1b\\\x1b[0m"
        ),
        "\x1bP1$r0;3;4:3;38:5:208;48:2::1:2:3;58:2::9:8:7m\x1b\\"
    );
    assert_eq!(
        reply(&mut screen, "\x1b[97;104m\x1bP$qm\x1b\\\x1b[0m"),
        "\x1bP1$r0;97;104m\x1b\\"
    );
    assert_eq!(reply(&mut screen, "\x1bP$qr\x1b\\"), "\x1bP1$r1;24r\x1b\\");
    assert_eq!(
        reply(&mut screen, "\x1b[5;10r\x1bP$qr\x1b\\\x1b[r"),
        "\x1bP1$r5;10r\x1b\\"
    );
    // An unset cursor is the blinking block Diri draws (DECSCUSR 1).
    assert_eq!(reply(&mut screen, "\x1bP$q q\x1b\\"), "\x1bP1$r1 q\x1b\\");
    assert_eq!(
        reply(&mut screen, "\x1b[5 q\x1bP$q q\x1b\\"),
        "\x1bP1$r5 q\x1b\\"
    );
    assert_eq!(
        reply(&mut screen, "\x1b[4 q\x1bP$q q\x1b\\"),
        "\x1bP1$r4 q\x1b\\"
    );
    // A setting this terminal does not report is answered as invalid.
    assert_eq!(reply(&mut screen, "\x1bP$q\"p\x1b\\"), "\x1bP0$r\x1b\\");
}

#[test]
fn osc_color_queries_answer_from_the_palette() {
    let mut screen = local();
    assert_eq!(
        reply(&mut screen, "\x1b]10;?\x1b\\"),
        "\x1b]10;rgb:e6e6/e6e6/e6e6\x1b\\"
    );
    assert_eq!(
        reply(&mut screen, "\x1b]11;?\x1b\\"),
        "\x1b]11;rgb:1212/1313/1818\x1b\\"
    );
    assert_eq!(
        reply(&mut screen, "\x1b]12;?\x1b\\"),
        "\x1b]12;rgb:e6e6/e6e6/e5e5\x1b\\"
    );
    // The terminator the program used comes back.
    assert_eq!(
        reply(&mut screen, "\x1b]11;?\x07"),
        "\x1b]11;rgb:1212/1313/1818\x07"
    );
    assert_eq!(
        reply(&mut screen, "\x1b]4;1;?\x1b\\"),
        "\x1b]4;1;rgb:cdcd/3131/3131\x1b\\"
    );
    // 16–255 are xterm's fixed cube and gray ramp.
    assert_eq!(
        reply(&mut screen, "\x1b]4;196;?\x1b\\"),
        "\x1b]4;196;rgb:ffff/0000/0000\x1b\\"
    );
    assert_eq!(
        reply(&mut screen, "\x1b]4;244;?\x1b\\"),
        "\x1b]4;244;rgb:8080/8080/8080\x1b\\"
    );
    // A color the program set is the color it reads back.
    assert_eq!(
        reply(&mut screen, "\x1b]4;1;rgb:12/34/56\x1b\\\x1b]4;1;?\x1b\\"),
        "\x1b]4;1;rgb:1212/3434/5656\x1b\\"
    );
    assert_eq!(
        reply(&mut screen, "\x1b]11;#010203\x1b\\\x1b]11;?\x1b\\"),
        "\x1b]11;rgb:0101/0202/0303\x1b\\"
    );
    assert_eq!(
        reply(&mut screen, "\x1b]111\x1b\\\x1b]11;?\x1b\\"),
        "\x1b]11;rgb:1212/1313/1818\x1b\\"
    );
    // A light theme answers light colors.
    screen.set_palette(Some(LIGHT));
    assert_eq!(
        reply(&mut screen, "\x1b]11;?\x1b\\"),
        "\x1b]11;rgb:ffff/ffff/ffff\x1b\\"
    );
}

#[test]
fn color_queries_wait_for_a_palette() {
    let mut screen = local();
    screen.set_palette(None);
    // Unknown colors are not guessed; the program falls back after its DA.
    assert_eq!(reply(&mut screen, "\x1b]11;?\x1b\\\x1b[c"), "\x1b[?62;22c");
    assert_eq!(reply(&mut screen, "\x1b]4;1;?\x1b\\"), "");
    assert_eq!(reply(&mut screen, "\x1b[?996n"), "");
    // The fixed cube needs no palette.
    assert_eq!(
        reply(&mut screen, "\x1b]4;16;?\x1b\\"),
        "\x1b]4;16;rgb:0000/0000/0000\x1b\\"
    );
}

#[test]
fn color_scheme_query_and_reports_follow_the_appearance() {
    let mut screen = local();
    assert_eq!(reply(&mut screen, "\x1b[?996n"), "\x1b[?997;1n");
    assert_eq!(reply(&mut screen, "\x1b[?2031$p"), "\x1b[?2031;2$y");
    // Not asked for: a theme change is silent.
    screen.set_palette(Some(LIGHT));
    assert!(screen.take_unsolicited().is_empty());
    assert_eq!(reply(&mut screen, "\x1b[?996n"), "\x1b[?997;2n");

    assert_eq!(
        reply(&mut screen, "\x1b[?2031h\x1b[?2031$p"),
        "\x1b[?2031;1$y"
    );
    screen.set_palette(Some(DARK));
    assert_eq!(screen.take_unsolicited(), b"\x1b[?997;1n");
    // The same appearance again (another dark theme) is not a change.
    screen.set_palette(Some(TerminalPalette {
        background: [0, 0, 0],
        ..DARK
    }));
    assert!(screen.take_unsolicited().is_empty());
    screen.set_palette(Some(LIGHT));
    // `take_replies` carries them too, for the pump.
    assert_eq!(screen.take_replies(), b"\x1b[?997;2n");
    screen.feed(b"\x1b[?2031l");
    screen.set_palette(Some(DARK));
    assert!(screen.take_unsolicited().is_empty());
}

#[test]
fn in_band_resize_reports_on_enable_and_on_every_resize() {
    let (cell_width, cell_height) = DEFAULT_CELL_PIXELS;
    let report = |cols: usize, rows: usize| {
        format!(
            "\x1b[48;{rows};{cols};{};{}t",
            rows * usize::from(cell_height),
            cols * usize::from(cell_width)
        )
    };
    let mut screen = local();
    assert_eq!(reply(&mut screen, "\x1b[?2048h"), report(80, 24));
    assert_eq!(reply(&mut screen, "\x1b[?2048$p"), "\x1b[?2048;1$y");
    screen.resize(120, 40);
    assert_eq!(
        String::from_utf8(screen.take_unsolicited()).unwrap(),
        report(120, 40)
    );
    // The same size again is not a resize.
    screen.resize(120, 40);
    assert!(screen.take_unsolicited().is_empty());
    assert_eq!(reply(&mut screen, "\x1b[?2048l"), "");
    screen.resize(80, 24);
    assert!(screen.take_unsolicited().is_empty());
}

#[test]
fn decrqm_reports_the_newly_recognized_modes() {
    let mut screen = local();
    for (query, expected) in [
        ("\x1b[?47$p", "\x1b[?47;2$y"),
        ("\x1b[?1047$p", "\x1b[?1047;2$y"),
        ("\x1b[?2031$p", "\x1b[?2031;2$y"),
        ("\x1b[?2048$p", "\x1b[?2048;2$y"),
        ("\x1b[?1016$p", "\x1b[?1016;2$y"),
        ("\x1b[?1016h\x1b[?1016$p", "\x1b[?1016;1$y"),
        ("\x1b[?1016l\x1b[?1016$p", "\x1b[?1016;2$y"),
        // Recognized but not implemented: permanently reset, so a program
        // falls back instead of enabling something that will not happen.
        ("\x1b[?69$p", "\x1b[?69;4$y"),
        ("\x1b[?69h\x1b[?69$p", "\x1b[?69;4$y"),
        ("\x1b[?47h\x1b[?47$p", "\x1b[?47;1$y"),
        ("\x1b[?47l\x1b[?1047h\x1b[?1047$p", "\x1b[?1047;1$y"),
    ] {
        assert_eq!(reply(&mut screen, query), expected, "{query:?}");
    }
}

#[test]
fn extended_cursor_position_and_screen_size_reports() {
    let mut screen = local();
    assert_eq!(reply(&mut screen, "\x1b[3;7H\x1b[?6n"), "\x1b[?3;7;1R");
    assert_eq!(reply(&mut screen, "\x1b[19t"), "\x1b[9;24;80t");
}

/// Claude Code sends its program-status probe, XTVERSION and the kitty
/// keyboard query together, then a DA1 flush. It reads every answer that
/// arrives before DA1 as support, so DA1 must come last.
#[test]
fn replies_keep_query_order_with_device_attributes_last() {
    let mut screen = local();
    let probe =
        "\x1b]7501;?\x1b\\\x1b[>0q\x1b[?u\x1b]11;?\x1b\\\x1b[?996n\x1bP+q524742\x1b\\\x1b[c";
    assert_eq!(
        reply(&mut screen, probe),
        concat!(
            "\x1b]7501;?\x1b\\",
            "\x1bP>|diri 0.0.0-test\x1b\\",
            "\x1b[?0u",
            "\x1b]11;rgb:1212/1313/1818\x1b\\",
            "\x1b[?997;1n",
            "\x1bP1+r524742=38\x1b\\",
            "\x1b[?62;22c",
        )
    );
    // Split at every byte: order survives a read boundary anywhere.
    let mut split = local();
    let mut replies = Vec::new();
    for byte in probe.bytes() {
        split.feed(&[byte]);
        replies.extend(split.take_replies());
    }
    assert_eq!(
        String::from_utf8(replies).unwrap(),
        reply(&mut local(), probe)
    );
}

#[test]
fn a_remote_holder_answers_none_of_the_new_queries() {
    let mut screen = holder();
    for query in [
        "\x1b[>0q",
        "\x1b[?u",
        "\x1bP+q524742\x1b\\",
        "\x1bP$qm\x1b\\",
        "\x1b]11;?\x1b\\",
        "\x1b]4;196;?\x1b\\",
        "\x1b[?996n",
        "\x1b[?2048h",
        "\x1b[?6n",
        "\x1b[19t",
    ] {
        assert_eq!(reply(&mut screen, query), "", "{query:?}");
    }
    screen.resize(100, 30);
    assert!(screen.take_replies().is_empty());
    // Nor does it publish redefined colors: its wire output is unchanged.
    screen.feed(b"\x1b]4;1;rgb:12/34/56\x1b\\\x1b[31mX");
    let update = screen.grid_update(true);
    assert_eq!(update.changed_rows[0].cells[0].fg, TermColor::Ansi(1));
}

#[test]
fn palette_changes_reach_the_grid() {
    let mut screen = local();
    let _ = screen.grid_update(true);
    screen.feed(b"\x1b]4;1;rgb:12/34/56\x1b\\\x1b[31;48;5;1mX\x1b[0m");
    let update = screen.grid_update(false);
    let cell = update.changed_rows[0].cells[0];
    assert_eq!(cell.fg, TermColor::Rgb(0x12, 0x34, 0x56));
    assert_eq!(cell.bg, TermColor::Rgb(0x12, 0x34, 0x56));
    assert_eq!(
        screen.full_snapshot().changed_rows[0].cells[0].fg,
        TermColor::Rgb(0x12, 0x34, 0x56)
    );
    // Resetting the color restores the theme's ANSI red everywhere.
    screen.feed(b"\x1b]104;1\x1b\\");
    let update = screen.grid_update(false);
    assert_eq!(update.changed_rows[0].cells[0].fg, TermColor::Ansi(1));
}

#[test]
fn soft_reset_restores_modes_and_attributes_but_keeps_the_screen() {
    let mut screen = local();
    screen.feed(b"keep-text\x1b[?2004h\x1b[?25l\x1b[?6h\x1b[1;31m\x1b[5;10r\x1b[!pafter-decstr");
    assert_eq!(screen.lines()[0].trim_end(), "keep-text");
    assert!(
        screen
            .lines()
            .iter()
            .any(|line| line.contains("after-decstr"))
    );
    assert!(screen.cursor().2, "DECSTR shows the cursor");
    assert!(screen.bracketed_paste(), "DECSTR keeps bracketed paste");
    assert_eq!(reply(&mut screen, "\x1bP$qm\x1b\\"), "\x1bP1$r0m\x1b\\");
    assert_eq!(reply(&mut screen, "\x1bP$qr\x1b\\"), "\x1bP1$r1;24r\x1b\\");
    assert_eq!(reply(&mut screen, "\x1b[?6$p"), "\x1b[?6;2$y");
}

#[test]
fn erase_in_line_cancels_a_pending_wrap() {
    let mut screen = local();
    screen.feed(format!("{}\x1b[KG", "F".repeat(80)).as_bytes());
    let lines = screen.lines();
    assert_eq!(lines[0], format!("{}G", "F".repeat(79)));
    assert!(lines.get(1).is_none_or(|line| line.trim_end().is_empty()));
}

#[test]
fn full_reset_clears_the_title() {
    let mut screen = local();
    screen.feed(b"\x1b]2;t\x07");
    assert_eq!(screen.title(), Some("t"));
    screen.feed(b"\x1bcafter-ris");
    assert_eq!(screen.title(), None);
}

#[test]
fn legacy_alternate_screens_share_the_cursor() {
    let mut screen = local();
    // 47: no save/restore, nothing cleared; the cursor carries across.
    screen.feed(b"primary-line\x1b[1;6H\x1b[?47h\x1b[2J\x1b[HALT47\x1b[?47l|after47");
    assert_eq!(screen.lines()[0].trim_end(), "prima|after47");
    assert!(!screen.is_alt_screen());
    // 1047: the alternate screen is cleared on exit, the cursor shared.
    screen.feed(b"\x1b[3;1Hp1047\x1b[?1047hALT\x1b[?1047l|x");
    assert_eq!(screen.lines()[2].trim_end(), "p1047   |x");
    screen.feed(b"\x1b[?1047h");
    assert!(screen.is_alt_screen());
    assert!(screen.lines().iter().all(|line| !line.contains("ALT")));
    screen.feed(b"\x1b[?1047l");
    assert!(!screen.is_alt_screen());
}

#[test]
fn a_checkpoint_from_before_negotiation_proves_zero_flags() {
    let mut screen = local();
    screen.restore_keyboard_state(Default::default());
    assert_eq!(
        screen.input_keyboard_state(),
        None,
        "unknown after an old cache"
    );
    assert!(screen.mark_keyboard_never_negotiated());
    assert_eq!(
        screen
            .input_keyboard_state()
            .unwrap()
            .enhancements
            .unwrap()
            .bits(),
        0
    );
    // A screen that holds flags is never declared unnegotiated.
    screen.feed(b"\x1b[>1u");
    screen.restore_keyboard_state(Default::default());
    assert!(!screen.mark_keyboard_never_negotiated());
    assert_eq!(screen.input_keyboard_state(), None);
}
