use diri_proto::grid::RowMetadata;
use diri_proto::inline_image::{ImageAnchor, InlineImageFormat};

use super::{DEFAULT_CELL_PIXELS, STORE_BUDGET_BYTES, sniff};
use crate::HeadlessScreen;

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, &b)| n | (u32::from(b) << (16 - 8 * i)));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// A PNG header naming `width`×`height`; the Engine never decodes pixels.
fn png(width: u32, height: u32) -> Vec<u8> {
    let mut bytes = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR".to_vec();
    bytes.extend_from_slice(&width.to_be_bytes());
    bytes.extend_from_slice(&height.to_be_bytes());
    bytes.extend_from_slice(&[8, 6, 0, 0, 0, 0, 0, 0, 0]);
    bytes
}

fn kitty(control: &str, payload: &[u8]) -> Vec<u8> {
    format!("\x1b_G{control};{}\x1b\\", base64(payload)).into_bytes()
}

fn screen() -> HeadlessScreen {
    HeadlessScreen::new(40, 10).with_inline_images()
}

/// Every anchor on the visible screen, by row.
fn anchors(screen: &HeadlessScreen) -> Vec<(u16, RowMetadata)> {
    screen
        .full_snapshot()
        .changed_rows
        .into_iter()
        .filter(|row| !row.metadata.links.is_empty())
        .map(|row| (row.y, row.metadata))
        .collect()
}

fn single_anchor(metadata: &RowMetadata) -> (ImageAnchor, u16, u16) {
    assert_eq!(metadata.links.len(), 1, "{metadata:?}");
    let link = &metadata.links[0];
    (
        ImageAnchor::parse(&link.uri).expect("anchor"),
        link.start,
        link.end,
    )
}

#[test]
fn kitty_support_query_is_answered_before_device_attributes() {
    let query = b"\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\\x1b[c";
    let mut screen = screen();
    screen.feed(query);
    let replies = String::from_utf8(screen.take_replies()).unwrap();
    assert_eq!(replies, "\x1b_Gi=31;OK\x1b\\\x1b[?62;22c");
    assert_eq!(screen.inline_image_bytes(), 0, "a query stores nothing");
    assert!(anchors(&screen).is_empty());

    // Without the opt-in (the remote Holder) nothing advertises support.
    let mut plain = HeadlessScreen::new(40, 10);
    plain.feed(query);
    // The emulator's own, shorter attributes.
    assert_eq!(plain.take_replies(), b"\x1b[?6c");
}

#[test]
fn kitty_png_is_placed_at_the_cursor_and_moves_it_after_the_image() {
    let mut screen = screen();
    screen.feed(b"ab");
    // 64×102 px at 16×34 px cells: 4 columns by 3 rows.
    screen.feed(&kitty("a=T,f=100,i=7", &png(64, 102)));
    assert_eq!(
        String::from_utf8(screen.take_replies()).unwrap(),
        "\x1b_Gi=7;OK\x1b\\"
    );
    let rows = anchors(&screen);
    assert_eq!(rows.len(), 3);
    for (offset, (y, metadata)) in rows.iter().enumerate() {
        let (anchor, start, end) = single_anchor(metadata);
        assert_eq!(*y as usize, offset);
        assert_eq!((start, end), (2, 6));
        assert_eq!(
            (anchor.row, anchor.col, anchor.cols, anchor.rows),
            (offset as u16, 2, 4, 3)
        );
    }
    // Kitty leaves the cursor after the image's last column, on its last row.
    assert_eq!(screen.cursor(), (6, 2, true));
    let (anchor, ..) = single_anchor(&rows[0].1);
    let stored = screen.inline_image(&anchor.key).expect("stored");
    assert_eq!(stored.format, InlineImageFormat::Png);
    assert_eq!((stored.width, stored.height), (64, 102));
    assert_eq!(&*stored.data, png(64, 102).as_slice());
    // The image cells read as blanks: status detection sees only the text.
    assert_eq!(screen.row_text(0), "ab");
}

#[test]
fn chunked_rgba_split_at_every_byte_matches_one_read() {
    let pixels: Vec<u8> = (0..(5 * 3 * 4)).map(|i| i as u8).collect();
    let encoded = base64(&pixels);
    let (first, rest) = encoded.split_at(32);
    let mut stream = format!("\x1b_Ga=T,f=32,s=5,v=3,i=9,q=1,m=1;{first}\x1b\\").into_bytes();
    stream.extend_from_slice(format!("\x1b_Gm=0;{rest}\x1b\\").as_bytes());
    stream.extend_from_slice(b"after");

    let mut whole = screen();
    whole.feed(&stream);
    let mut bytewise = screen();
    for byte in &stream {
        bytewise.feed(std::slice::from_ref(byte));
    }
    for screen in [&whole, &bytewise] {
        let rows = anchors(screen);
        assert_eq!(rows.len(), 1, "5×3 px fits one cell");
        let (anchor, ..) = single_anchor(&rows[0].1);
        let stored = screen.inline_image(&anchor.key).unwrap();
        assert_eq!(stored.format, InlineImageFormat::Rgba);
        assert_eq!(&*stored.data, pixels.as_slice());
        assert_eq!(screen.row_text(0), " after", "the image cell reads blank");
    }
    assert!(whole.take_replies().is_empty(), "q=1 silences OK");
    assert_eq!(whole.row_text(0), bytewise.row_text(0));
    assert_eq!(whole.cursor(), bytewise.cursor());
    assert_eq!(bytewise.image_carry_bytes(), 0);
}

#[test]
fn iterm_inline_file_is_placed_and_downloads_are_ignored() {
    let file = png(320, 68); // 20×2 cells
    let mut screen = screen();
    screen.feed(
        format!(
            "\x1b]1337;File=name={};size={};inline=1:{}\x07\r\nnext",
            base64(b"pic.png"),
            file.len(),
            base64(&file)
        )
        .as_bytes(),
    );
    let rows = anchors(&screen);
    assert_eq!(rows.len(), 2);
    let (anchor, start, end) = single_anchor(&rows[1].1);
    assert_eq!((start, end), (0, 20));
    assert_eq!((anchor.row, anchor.cols, anchor.rows), (1, 20, 2));
    assert_eq!(screen.row_text(2), "next");

    let mut download = super::super::HeadlessScreen::new(40, 10).with_inline_images();
    download.feed(format!("\x1b]1337;File=inline=0:{}\x07x", base64(&file)).as_bytes());
    assert!(anchors(&download).is_empty());
    assert_eq!(download.inline_image_bytes(), 0);
    assert_eq!(download.row_text(0), "x");
}

#[test]
fn iterm_sizes_in_cells_percent_and_pixels() {
    for (args, expected) in [
        ("width=10;height=2", (10, 2)),
        ("width=50%", (20, 1)),
        // Twice the height in pixels: twice the width too.
        ("height=68px", (40, 2)),
        // No size: a 1600×1200 photo shrinks to fit the 40×10 screen,
        // height-limited, aspect kept.
        ("", (29, 10)),
    ] {
        let file = png(
            if args.is_empty() { 1600 } else { 320 },
            if args.is_empty() { 1200 } else { 34 },
        );
        let mut screen = screen();
        screen.feed(format!("\x1b]1337;File=inline=1;{args}:{}\x1b\\", base64(&file)).as_bytes());
        let rows = anchors(&screen);
        let (anchor, ..) = single_anchor(&rows[0].1);
        assert_eq!((anchor.cols, anchor.rows), expected, "{args}");
    }
}

#[test]
fn multipart_iterm_file_assembles_parts() {
    let file = png(16, 34);
    let encoded = base64(&file);
    let (a, b) = encoded.split_at(16);
    let mut screen = screen();
    screen.feed(
        format!(
            "\x1b]1337;MultipartFile=inline=1\x07\x1b]1337;FilePart={a}\x07\x1b]1337;FilePart={b}\x07\x1b]1337;FileEnd\x07"
        )
        .as_bytes(),
    );
    let rows = anchors(&screen);
    let (anchor, ..) = single_anchor(&rows[0].1);
    assert_eq!(
        &*screen.inline_image(&anchor.key).unwrap().data,
        file.as_slice()
    );
}

#[test]
fn placements_scroll_into_history_with_their_rows() {
    let mut screen = screen();
    screen.feed(&kitty("a=T,f=100", &png(32, 68)));
    screen.feed(b"\r\n");
    for line in 0..30 {
        screen.feed(format!("line {line}\r\n").as_bytes());
    }
    assert!(anchors(&screen).is_empty(), "scrolled off the screen");
    let history = screen.history_metadata();
    let carried: Vec<_> = history
        .iter()
        .filter(|metadata| !metadata.links.is_empty())
        .map(|metadata| single_anchor(metadata).0)
        .collect();
    assert_eq!(carried.len(), 2);
    assert_eq!((carried[0].row, carried[1].row), (0, 1));
    assert!(screen.inline_image(&carried[0].key).is_some());
}

#[test]
fn erasing_the_screen_removes_placements() {
    let mut screen = screen();
    screen.feed(&kitty("a=T,f=100", &png(32, 34)));
    assert_eq!(anchors(&screen).len(), 1);
    screen.feed(b"\x1b[2J");
    assert!(anchors(&screen).is_empty());
}

#[test]
fn kitty_delete_blanks_cells_and_uppercase_frees_the_image() {
    let mut screen = screen();
    screen.feed(&kitty("a=T,f=100,i=1,q=2", &png(16, 34)));
    screen.feed(b" ");
    screen.feed(&kitty("a=T,f=100,i=2,q=2", &png(16, 34)));
    let keys: Vec<_> = anchors(&screen)
        .iter()
        .flat_map(|(_, metadata)| metadata.links.iter())
        .map(|link| ImageAnchor::parse(&link.uri).unwrap().key)
        .collect();
    assert_eq!(keys.len(), 2);
    let cursor = screen.cursor();

    screen.feed(b"\x1b_Ga=d,d=i,i=1\x1b\\");
    let left: Vec<_> = anchors(&screen)
        .iter()
        .flat_map(|(_, metadata)| metadata.links.iter())
        .map(|link| ImageAnchor::parse(&link.uri).unwrap().key)
        .collect();
    assert_eq!(left, vec![keys[1].clone()]);
    assert!(
        screen.inline_image(&keys[0]).is_some(),
        "lowercase keeps data"
    );
    assert_eq!(screen.cursor(), cursor, "deleting never moves the cursor");

    screen.feed(b"\x1b_Ga=d,d=A\x1b\\");
    assert!(anchors(&screen).is_empty());
    assert!(screen.inline_image(&keys[0]).is_none());
    assert!(screen.inline_image(&keys[1]).is_none());
    assert_eq!(screen.inline_image_bytes(), 0);
}

#[test]
fn kitty_put_places_a_transmitted_image_again() {
    let mut screen = screen();
    screen.feed(&kitty("a=t,f=100,i=4", &png(16, 34)));
    assert!(anchors(&screen).is_empty(), "transmit alone shows nothing");
    screen.feed(b"\x1b_Ga=p,i=4,c=3,r=2\x1b\\");
    screen.feed(b"\x1b_Ga=p,i=5\x1b\\");
    assert_eq!(
        String::from_utf8(screen.take_replies()).unwrap(),
        "\x1b_Gi=4;OK\x1b\\\x1b_Gi=4;OK\x1b\\\x1b_Gi=5;ENOENT:no such image\x1b\\"
    );
    let rows = anchors(&screen);
    assert_eq!(rows.len(), 2);
    assert_eq!(single_anchor(&rows[0].1).0.cols, 3);
}

#[test]
fn image_numbers_get_terminal_assigned_ids() {
    let mut screen = screen();
    screen.feed(&kitty("a=t,f=100,I=13", &png(16, 34)));
    let reply = String::from_utf8(screen.take_replies()).unwrap();
    assert!(
        reply.starts_with("\x1b_Gi=") && reply.ends_with(",I=13;OK\x1b\\"),
        "{reply:?}"
    );
    screen.feed(b"\x1b_Ga=p,I=13,q=1\x1b\\");
    assert_eq!(anchors(&screen).len(), 1);
}

#[test]
fn unsupported_transmissions_are_refused_with_errors() {
    let mut screen = screen();
    for (control, payload, error) in [
        ("a=q,i=1,t=f,f=100", b"/tmp/x.png".as_slice(), "EBADF"),
        ("a=q,i=2,t=s,f=24,s=1,v=1", b"shm", "EBADF"),
        ("a=q,i=3,f=24", b"\0\0\0", "EINVAL"),
        ("a=q,i=4,f=32,s=2,v=2", b"\0\0\0\0", "ENODATA"),
        ("a=q,i=5,f=100", b"not a png", "EBADPNG"),
        ("a=q,i=6,f=24,s=1,v=1,U=1", b"\0\0\0", "ENOTSUP"),
        ("a=q,i=7,f=24,s=20000,v=1,o=z", b"x", "EFBIG"),
    ] {
        screen.feed(&kitty(control, payload));
        let reply = String::from_utf8(screen.take_replies()).unwrap();
        assert!(
            reply.contains(&format!(";{error}:")),
            "{control}: {reply:?}"
        );
    }
    screen.feed(b"\x1b_Ga=q,i=8,f=24,s=1,v=1;!!!!\x1b\\");
    assert!(
        String::from_utf8(screen.take_replies())
            .unwrap()
            .contains("EINVAL:invalid base64")
    );
    assert_eq!(screen.inline_image_bytes(), 0);
}

#[test]
fn replayed_history_is_parsed_but_never_answered() {
    let mut stream = kitty("a=T,f=100,i=3", &png(16, 34));
    let historical = stream.len();
    stream.extend_from_slice(b"\x1b_Gi=9,s=1,v=1,a=q,f=24;AAAA\x1b\\");
    let mut screen = screen();
    screen.feed_with_history(&stream, historical);
    assert_eq!(
        String::from_utf8(screen.take_replies()).unwrap(),
        "\x1b_Gi=9;OK\x1b\\"
    );
    assert_eq!(
        anchors(&screen).len(),
        1,
        "history still rebuilds the screen"
    );
}

#[test]
fn device_attributes_are_long_enough_for_kittens_probe() {
    // `kitten icat`'s probe, verbatim: three transmission media, then DA1.
    let probe = b"\x1b[?s\x1b_Ga=q,f=24,s=1,v=1,S=3,i=1;MTIz\x1b\\\
        \x1b_Ga=q,f=24,t=t,s=1,v=1,S=12,i=2;L3RtcC9pbWFnZQ\x1b\\\
        \x1b_Ga=q,f=24,t=s,s=1,v=1,S=4,i=3;aWNhdA\x1b\\\x1b[c\x1b[0c";
    let mut screen = screen();
    screen.feed(probe);
    assert_eq!(
        String::from_utf8(screen.take_replies()).unwrap(),
        "\x1b_Gi=1;OK\x1b\\\
         \x1b_Gi=2;EBADF:only direct transmission is supported\x1b\\\
         \x1b_Gi=3;EBADF:only direct transmission is supported\x1b\\\
         \x1b[?62;22c\x1b[?62;22c"
    );
    // Secondary attributes are still the emulator's.
    screen.feed(b"\x1b[>c");
    assert!(screen.take_replies().starts_with(b"\x1b[>"));
}

#[test]
fn cell_size_queries_report_device_pixels() {
    let mut screen = screen();
    screen.feed(b"\x1b[16t\x1b[14t\x1b]1337;ReportCellSize\x07");
    let (width, height) = DEFAULT_CELL_PIXELS;
    assert_eq!(
        String::from_utf8(screen.take_replies()).unwrap(),
        format!(
            "\x1b[6;{height};{width}t\x1b[4;{};{}t\x1b]1337;ReportCellSize={height}.0;{width}.0;1.0\x1b\\",
            u32::from(height) * 10,
            u32::from(width) * 40
        )
    );
    screen.set_cell_pixels(20, 40);
    screen.feed(b"\x1b[16t");
    assert_eq!(screen.take_replies(), b"\x1b[6;40;20t");

    // The emulator still ignores the query it never answered.
    let mut plain = HeadlessScreen::new(40, 10);
    plain.feed(b"\x1b[16t");
    assert!(plain.take_replies().is_empty());
}

#[test]
fn the_store_evicts_the_oldest_image_past_its_budget() {
    let mut screen = HeadlessScreen::new(40, 10).with_inline_images();
    // 1000×1000 RGBA: 4 MB each, so the 32 MiB budget holds eight.
    let pixels = vec![7u8; 1000 * 1000 * 4];
    let mut keys = Vec::new();
    for _ in 0..10 {
        screen.feed(&kitty("a=T,f=32,s=1000,v=1000,C=1,c=1,r=1", &pixels));
        let (_, metadata) = anchors(&screen).into_iter().next().unwrap();
        keys.push(single_anchor(&metadata).0.key);
    }
    assert!(screen.inline_image_bytes() <= STORE_BUDGET_BYTES);
    assert!(screen.inline_image(&keys[0]).is_none(), "oldest evicted");
    assert!(screen.inline_image(&keys[9]).is_some(), "newest kept");
}

#[test]
fn an_unterminated_sequence_has_bounded_memory_and_output_resumes() {
    let mut screen = screen();
    screen.feed(b"\x1b]1337;File=inline=1:");
    let chunk = vec![b'A'; 1 << 20];
    for _ in 0..40 {
        screen.feed(&chunk);
    }
    assert!(screen.image_carry_bytes() <= super::MAX_SEQUENCE_BYTES + 64);
    screen.feed(b"\x07visible");
    assert_eq!(screen.row_text(0), "visible");
    assert_eq!(screen.inline_image_bytes(), 0);
}

#[test]
fn a_stray_escape_inside_a_sequence_hands_control_back_to_the_parser() {
    let mut screen = screen();
    screen.feed(b"\x1b_Ga=T,f=100;AAAA\x1b[31mred");
    assert_eq!(screen.row_text(0), "red");
    assert!(anchors(&screen).is_empty());
}

#[test]
fn ordinary_output_parses_exactly_as_without_images() {
    let stream: &[u8] =
        b"\x1b[1;32mgreen\x1b[0m \x1b]8;;https://x.test\x1b\\link\x1b]8;;\x1b\\\r\n\
        \x1b[2;5H\x1b[Kmoved\x1b]0;title\x07\x1b[?2026h\x1b[3;1Hsync\x1b[?2026l\x1b[16";
    let mut with = screen();
    let mut without = HeadlessScreen::new(40, 10);
    for split in [1, 3, 7, stream.len()] {
        for chunk in stream.chunks(split) {
            with.feed(chunk);
            without.feed(chunk);
        }
    }
    assert_eq!(with.full_snapshot(), without.full_snapshot());
    assert_eq!(with.title(), without.title());
}

#[test]
fn a_synchronized_update_lands_before_an_image_inside_it() {
    let mut screen = screen();
    screen.feed(b"\x1b[?2026hbefore ");
    screen.feed(&kitty("a=T,f=100,q=2", &png(16, 34)));
    let rows = anchors(&screen);
    assert_eq!(screen.row_text(0), "before");
    assert_eq!(single_anchor(&rows[0].1).1, 7);
}

#[test]
fn resetting_keeps_the_opt_in_and_drops_images() {
    let mut screen = screen();
    screen.set_cell_pixels(10, 20);
    screen.feed(&kitty("a=T,f=100", &png(16, 34)));
    screen.reset();
    assert!(screen.inline_images_enabled());
    assert_eq!(screen.inline_image_bytes(), 0);
    screen.feed(b"\x1b[16t");
    assert_eq!(screen.take_replies(), b"\x1b[6;20;10t");
}

#[test]
fn headers_name_format_and_size() {
    assert_eq!(sniff(&png(3, 4)), Some((InlineImageFormat::Png, 3, 4)));
    assert_eq!(
        sniff(b"GIF89a\x05\x00\x06\x00rest"),
        Some((InlineImageFormat::Gif, 5, 6))
    );
    let jpeg = [
        0xff, 0xd8, 0xff, 0xe0, 0x00, 0x04, 0x00, 0x00, 0xff, 0xc0, 0x00, 0x11, 0x08, 0x00, 0x30,
        0x00, 0x40, 0x03,
    ];
    assert_eq!(sniff(&jpeg), Some((InlineImageFormat::Jpeg, 0x40, 0x30)));
    assert_eq!(sniff(b"plain text"), None);
}
