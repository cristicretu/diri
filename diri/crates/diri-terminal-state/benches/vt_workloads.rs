//! Headless VT throughput on the six `kitten __benchmark__` workloads.
//!
//! ASCII, Unicode, unique multi-codepoint cells, CSI with few characters,
//! Kitty images and long OSCs, parsed into a 120x80 terminal configured as
//! the local Engine configures it (compact history at its default depth,
//! notifications, inline images, host replies, kitty keyboard).
//!
//! The data is the kitten's own when `DIRI_VT_BENCH_DIR` names a directory
//! holding `ascii.bin`, `unicode.bin`, `unique_unicode.bin`, `csi.bin`,
//! `images.bin` and `long_escape_codes.bin` (one repetition of each payload,
//! captured from the kitten's output). Otherwise the payloads are generated
//! the way kitty 0.48 generates them (`tools/cmd/benchmark/main.go`), with a
//! fixed seed.
//!
//! ```sh
//! cargo bench -p diri-terminal-state --features bench-verify --bench vt_workloads
//! DIRI_VT_BENCH_READ=1024 cargo bench -p diri-terminal-state --features bench-verify --bench vt_workloads
//! ```
//!
//! # Every run is checked against a golden reference
//!
//! A number is printed only if every timed run ends exactly where a
//! reference parse of the same bytes ended. The reference is not the code
//! under test: `vt_workloads.golden` holds digests computed once, at the
//! commit named in its header, by feeding each payload one byte at a time
//! (see `DIRI_VT_BENCH_GOLDEN` below). A bug that behaves the same at every
//! read size, which comparing the code with itself cannot see, changes a
//! digest and stops the bench. Each digest covers:
//!
//! - bytes fed (must equal payload length x repetitions);
//! - the visible grid with styles, every scrollback row with its cells and
//!   metadata, the cursor and its style, modes, keyboard state, title,
//!   hyperlinks, progress and program status;
//! - replies, notifications and clipboard writes, in order;
//! - every image stored during the run (it may be deleted before the end):
//!   exact count, Kitty id, format, size and SHA-256 of the pixels;
//! - every OSC the parser dispatched: count, parameters, parameter bytes and
//!   BEL terminators, so a parser that skips the bodies of an OSC nothing
//!   reads (the long-escapes workload is OSC 6) is caught.
//!
//! Timed runs keep the OSC counts but not the per-byte OSC checksum, which
//! would add a pass over every body to the timer. One untimed run per
//! workload at the same read size turns the checksum on and must match the
//! golden checksum too. The image log keeps each stored image alive until
//! the run ends; it is hashed after the clock stops.
//!
//! The checks need the test-only `bench-verify` feature (the parser's OSC
//! totals and the image store's log); the bench does not build without it.
//!
//! `DIRI_VT_BENCH_GOLDEN=<file>` recomputes the digests (one byte per read,
//! checksum on) for the payloads in use, merges them into that file and
//! exits. Run it only at a known-good reference commit and record that
//! commit in the file's header. Captured payloads are matched by SHA-256, so
//! a different capture has no golden digest and is refused.
//!
//! Any PR that changes history retention (how many rows the budget keeps,
//! or what they hold) must regenerate the golden in the same PR, generated
//! and kitten sections both, and declare the row delta per workload.
//! The delta is declared in `DECLARED_HISTORY_ROWS` and the golden is
//! regenerated at the reference with that declaration: the content of the
//! rows kept is still checked against the reference, and their number
//! against the declaration.
//!
//! `DIRI_VT_BENCH_WRITE=<dir>` writes the payloads it uses there and exits,
//! for `diri-engine`'s `vtbench` (the same workloads through Holder and
//! Engine).
//!
//! `DIRI_VT_BENCH_ONLY=images,csi` measures only the named workloads.
//!
//! `DIRI_VT_BENCH_READ` sets the read size (default 64 KiB; a macOS PTY hands
//! over about 1 KiB per read). Reported: MB/s (10^6 bytes) of wall time for
//! `feed` and the pump's `take_*` calls, median of 7 runs of at least 64 MB
//! each.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::hint::black_box;
use std::path::Path;
use std::time::{Duration, Instant};

use diri_terminal_state::{HeadlessScreen, osc_observe};
use sha2::{Digest as _, Sha256};

const COLS: usize = 120;
const ROWS: usize = 80;
const RUNS: usize = 7;
const MIN_BYTES_PER_RUN: usize = 64 << 20;

const WORKLOADS: [&str; 6] = [
    "ascii",
    "unicode",
    "unique_unicode",
    "csi",
    "images",
    "long_escape_codes",
];

struct Rng(u64);

impl Rng {
    fn below(&mut self, bound: usize) -> usize {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        (self.0.wrapping_mul(0x2545_F491_4F6C_DD1D) % bound as u64) as usize
    }

    fn string(&mut self, len: usize, alphabet: &[u8]) -> Vec<u8> {
        (0..len)
            .map(|_| alphabet[self.below(alphabet.len())])
            .collect()
    }
}

const ASCII_PRINTABLE: &[u8] =
    b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ  `~!@#$%^&*()_+-=[]{}\\|;:'\",<.>/?";

fn ascii_and_controls() -> Vec<u8> {
    [ASCII_PRINTABLE, b"\n\t"].concat()
}

const CHINESE: &str = "
旦海司有幼雞讀松鼻種比門真目怪少：扒裝虎怕您跑綠蝶黃，位香法士錯乙音造活羽詞坡村目園尺封鳥朋；法松夕點我冬停雪因科對只貓息加黃住蝶，明鴨乾春呢風乙時昔孝助？小紅女父故去。
飯躲裝個哥害共買去隻把氣年，己你校跟飛百拉！快石牙飽知唱想土人吹象毛吉每浪四又連見、欠耍外豆雞秋鼻。住步帶。
打六申幾麼：或皮又荷隻乙犬孝習秋還何氣；幾裏活打能花是入海乙山節會。種第共後陽沒喜姐三拍弟海肖，行知走亮包，他字幾，的木卜流旦乙左杯根毛。
您皮買身苦八手牛目地止哥彩第合麻讀午。原朋河乾種果「才波久住這香松」兄主衣快他玉坐要羽和亭但小山吉也吃耳怕，也爪斗斥可害朋許波怎祖葉卜。
行花兩耍許車丟學「示想百吃門高事」不耳見室九星枝買裝，枝十新央發旁品丁青給，科房火；事出出孝肉古：北裝愛升幸百東鼻到從會故北「可休笑物勿三游細斗」娘蛋占犬。我羊波雨跳風。
牛大燈兆新七馬，叫這牙後戶耳、荷北吃穿停植身玩間告或西丟再呢，他禾七愛干寺服石安：他次唱息它坐屋父見這衣發現來，苗會開條弓世者吃英定豆哭；跳風掃叫美神。
寸再了耍休壯植己，燈錯和，蝶幾欠雞定和愛，司紅後弓第樹會金拉快喝夕見往，半瓜日邊出讀雞苦歌許開；發火院爸乙；四帶亮錯鳥洋個讀。
";

const MISC_UNICODE: &str = "
‘’“”‹›«»‚„ 😀😛😇😈😉😍😎😮👍👎 —–§¶†‡©®™ →⇒•·°±−×÷¼½½¾
…µ¢£€¿¡¨´¸ˆ˜ ÀÁÂÃÄÅÆÇÈÉÊË ÌÍÎÏÐÑÒÓÔÕÖØ ŒŠÙÚÛÜÝŸÞßàá âãäåæçèéêëìí
îïðñòóôõöøœš ùúûüýÿþªºαΩ∞ ū̀n̂o᷵H̨a̠b̡͓̐c̡͓̐X̡͓̐
";

fn generate(name: &str) -> Vec<u8> {
    let mut rng = Rng(0x6b69_7474_7962_656e);
    match name {
        "ascii" => rng.string(1024 * 2048 + 13, &ascii_and_controls()),
        "unicode" => format!("{CHINESE}{MISC_UNICODE}\n\t")
            .repeat(1024)
            .into_bytes(),
        "unique_unicode" => {
            let mut out = String::with_capacity(256 * 1024 * 7);
            for i in 0..256 * 1024u32 {
                let mut q = i;
                out.push('a');
                for _ in 0..3 {
                    out.push(char::from_u32(0x300 + q % 0x70).unwrap());
                    q /= 0x70;
                }
            }
            out.into_bytes()
        }
        "csi" => {
            let mut out = Vec::with_capacity(1024 * 1024 + 64);
            while out.len() < 1024 * 1024 + 17 {
                let q = rng.below(100);
                let chunk: Vec<u8> = match q {
                    0..10 => {
                        let len = rng.below(72) + 1;
                        rng.string(len, &ascii_and_controls())
                    }
                    10..30 => b"\x1b[m\x1b[?1h\x1b[H".to_vec(),
                    30..40 => b"\x1b[1;2;3;4:3;31m".to_vec(),
                    40..50 => b"\x1b[38:5:24;48:2:125:136:147m".to_vec(),
                    50..60 => b"\x1b[58;5;44;2m".to_vec(),
                    60..80 => b"\x1b[m\x1b[10A\x1b[3E\x1b[2K".to_vec(),
                    _ => b"\x1b[39m\x1b[10`a\x1b[100b\x1b[?1l".to_vec(),
                };
                out.extend_from_slice(&chunk);
            }
            out.extend_from_slice(b"\x1b[m");
            out
        }
        "images" => {
            // A 1024x1024 RGBA transmit in 4096-byte base64 chunks, then a
            // delete by id, as the kitten's graphics writer sends it.
            let payload = "A".repeat((4 * 1024 * 1024usize).div_ceil(3) * 4);
            let mut out = Vec::with_capacity(payload.len() + 64 * 1024);
            let chunks: Vec<&[u8]> = payload.as_bytes().chunks(4096).collect();
            for (index, chunk) in chunks.iter().enumerate() {
                let more = u8::from(index + 1 < chunks.len());
                if index == 0 {
                    out.extend_from_slice(
                        format!("\x1b_Gq=2,m={more},s=1024,v=1024,i=12345;").as_bytes(),
                    );
                } else {
                    out.extend_from_slice(format!("\x1b_Gm={more};").as_bytes());
                }
                out.extend_from_slice(chunk);
                out.extend_from_slice(b"\x1b\\");
            }
            out.extend_from_slice(b"\x1b_Ga=d,d=I,i=12345,q=2\x1b\\");
            out
        }
        "long_escape_codes" => {
            let data = rng.string(8024, ASCII_PRINTABLE);
            [b"\x1b]6;".as_slice(), &data, b"\x07"]
                .concat()
                .repeat(1024)
        }
        _ => unreachable!(),
    }
}

fn screen() -> HeadlessScreen {
    HeadlessScreen::new_with_keyboard_enhancements(COLS, ROWS)
        .with_notifications()
        .with_inline_images()
        .with_host_replies("bench")
}

/// What a run leaves behind, beyond the screen itself.
struct Run {
    screen: HeadlessScreen,
    bytes_fed: usize,
    replies: Vec<u8>,
    notifications: Vec<String>,
    clipboard: Vec<String>,
}

/// One run: feed `reps` copies of `data` in `read`-sized pieces, handling
/// replies, notifications and clipboard writes the way the Engine pump
/// does. `checksum` turns on the per-byte OSC checksum (never for a timed
/// run).
fn run(data: &[u8], reps: usize, read: usize, checksum: bool) -> (Duration, Run) {
    let mut out = Run {
        screen: screen(),
        bytes_fed: 0,
        replies: Vec::new(),
        notifications: Vec::new(),
        clipboard: Vec::new(),
    };
    osc_observe::reset(checksum);
    let started = Instant::now();
    for _ in 0..reps {
        for chunk in data.chunks(read) {
            out.screen.feed(black_box(chunk));
            out.bytes_fed += chunk.len();
            out.replies.extend(out.screen.take_replies());
            let notifications = out.screen.take_notifications();
            if !notifications.is_empty() {
                out.notifications
                    .extend(notifications.iter().map(|n| format!("{n:?}")));
            }
            out.clipboard.extend(out.screen.take_clipboard());
        }
    }
    (started.elapsed(), out)
}

fn sha256(bytes: &[u8]) -> String {
    let mut hex = String::with_capacity(64);
    for byte in Sha256::digest(bytes) {
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

/// The visible grid without where history stands (#905's `grid-history-v1`
/// position, the snapshot's last field). Its origin counts evicted rows, so
/// it follows retention, which `history_rows_checked` already compares
/// against the reference or the declared count; a declared retention change
/// would otherwise change the grid digest too. Snapshots from before #905
/// have no such field and hash as they always did.
fn without_history_position(mut text: String) -> String {
    if let Some(at) = text.rfind(", history: ") {
        text.replace_range(at..text.len() - " }".len(), "");
    }
    text
}

/// Image anchors name a per-screen store nonce; only the rest must match.
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

/// History rows the code under test is declared to keep, by golden key
/// (`"workload payload-sha256"`), where that differs from what the
/// reference kept.
///
/// Retention depends only on the output, not on where reads end, but it does
/// depend on how well history compresses: a codec change that compresses a
/// little worse fits fewer rows in the 4 MiB budget. Such a change declares
/// its count here, as a reviewed behavior change, and the golden is
/// regenerated at the reference with these declarations: the reference then
/// hashes only its newest `declared` rows, and every run must keep exactly
/// `declared` rows whose content matches them. Undeclared payloads keep
/// exactly what the reference kept. A declaration above what the reference
/// kept cannot be checked and stops regeneration.
const DECLARED_HISTORY_ROWS: &[(&str, usize)] = &[
    // History sealing at the parser's pace (#928): faster LZ skipping.
    (
        "ascii 5b1de281ecd8d0f0db1696ae1dbf745069c6ff3815c809f02e31791d50cb131d",
        62_961,
    ),
    (
        "unicode 33d19ab233c8bb6434f594a1afa41aec58f12bc44005b87e0c059b11ced0033b",
        53_648,
    ),
    // The kitten 0.48 captures.
    (
        "ascii af0751e04c0301ae668fb64fee70eb53a45637e32ec2d5999c6367df6d10d515",
        63_217,
    ),
    (
        "unicode d5c85baa3324d1f35a7ac69f020494d7ec82fd281ceaf72bafa17ae871679c4a",
        53_648,
    ),
];

/// History rows compared: all the reference kept, or the declared count
/// (see [`DECLARED_HISTORY_ROWS`]).
fn history_checked(key: &str, kept: usize) -> usize {
    match DECLARED_HISTORY_ROWS
        .iter()
        .find(|(declared, _)| *declared == key)
    {
        Some(&(_, rows)) => {
            assert!(
                rows <= kept,
                "{key}: {rows} history rows declared, but the reference kept only {kept}"
            );
            rows
        }
        None => kept,
    }
}

/// Digest key of the per-byte OSC checksum, which timed runs leave off.
const OSC_CHECKSUM: &str = "osc_checksum";
/// Digest key recorded for the reader, not compared.
const REFERENCE_ONLY: &str = "history_rows_reference";

/// Everything observable at the end of a run, hashed where large. Keys are
/// stable; values are compared as text.
///
/// History is compared over its newest `checked_rows` rows (all of it; see
/// [`history_checked`]). `None` is the reference, which records what it kept.
fn digest(
    name: &str,
    data: &[u8],
    reps: usize,
    mut run: Run,
    checked_rows: Option<usize>,
) -> BTreeMap<String, String> {
    let mut d = BTreeMap::new();
    let mut put = |key: &str, value: String| {
        d.insert(key.to_owned(), value);
    };
    put("workload", name.to_owned());
    put("size", format!("{COLS}x{ROWS}"));
    put("payload_sha256", sha256(data));
    put("payload_bytes", data.len().to_string());
    put("reps", reps.to_string());
    put("bytes_fed", run.bytes_fed.to_string());

    let osc = osc_observe::totals();
    put(
        "osc_dispatches",
        format!(
            "{} params={} param_bytes={} bel={}",
            osc.dispatches, osc.params, osc.param_bytes, osc.bell_terminated
        ),
    );
    put(OSC_CHECKSUM, format!("{:016x}", osc.checksum));

    // Images: every one stored, in order, with runs of identical entries
    // collapsed so the file stays readable.
    let log = run.screen.take_stored_image_log();
    let mut images = format!("count={}", log.len());
    let mut previous: Option<(String, usize)> = None;
    let flush = |images: &mut String, entry: Option<(String, usize)>| {
        if let Some((text, times)) = entry {
            let _ = write!(images, " [{text}]x{times}");
        }
    };
    for (kitty_id, image) in &log {
        let text = format!(
            "id={} {:?}{} {}x{} {}B sha256={}",
            kitty_id.map_or_else(|| "-".to_owned(), |id| id.to_string()),
            image.format,
            if image.compressed { " z" } else { "" },
            image.width,
            image.height,
            image.data.len(),
            sha256(&image.data)
        );
        match &mut previous {
            Some((last, times)) if *last == text => *times += 1,
            _ => flush(&mut images, previous.replace((text, 1))),
        }
    }
    flush(&mut images, previous);
    put("images_stored", images);
    put(
        "images_held_bytes",
        run.screen.inline_image_bytes().to_string(),
    );

    put(
        "replies",
        format!("{}B sha256={}", run.replies.len(), sha256(&run.replies)),
    );
    put(
        "notifications",
        format!(
            "{} sha256={}",
            run.notifications.len(),
            sha256(run.notifications.join("\n").as_bytes())
        ),
    );
    put(
        "clipboard",
        format!(
            "{} sha256={}",
            run.clipboard.len(),
            sha256(run.clipboard.join("\n").as_bytes())
        ),
    );

    let screen = &mut run.screen;
    put(
        "grid_sha256",
        sha256(
            without_history_position(without_store_nonce(format!("{:?}", screen.full_snapshot())))
                .as_bytes(),
        ),
    );
    let kept = screen.history_rows();
    let checked =
        checked_rows.unwrap_or_else(|| history_checked(&format!("{name} {}", sha256(data)), kept));
    if checked_rows.is_none() {
        put("history_rows_reference", kept.to_string());
    }
    // A run keeps exactly the rows checked: newest-row hashes alone would
    // pass a run that kept extra, older rows. Core v2 (`core-v2-screen`)
    // must keep at least as many rows as the current core (the owner's
    // rule for its history), so it passes with more, and its newest
    // `checked` rows must match; `core_v2_bench` compares every row it
    // keeps, cell for cell, with a byte-at-a-time parse.
    let enough = if cfg!(feature = "core-v2-screen") {
        kept >= checked
    } else {
        kept == checked
    };
    put(
        "history_rows_checked",
        if checked_rows.is_none() || enough {
            checked.to_string()
        } else {
            format!("{checked} but {kept} kept")
        },
    );
    // The newest `checked` history rows and the screen, as text with cell
    // mappings re-indexed from the first row compared.
    let scrollback = screen.scrollback();
    let skip = scrollback.lines.len().saturating_sub(checked + ROWS);
    let mut text = scrollback.lines[skip..].join("\n");
    let text_cells: Vec<_> = scrollback
        .text_cells
        .range(skip..)
        .map(|(row, cells)| (row - skip, cells))
        .collect();
    let _ = write!(text, "\n{text_cells:?}\n{}", scrollback.is_alt_screen);
    put("scrollback_text_sha256", sha256(text.as_bytes()));
    let cells = screen.scrollback_cells(
        (kept.saturating_sub(checked)) as i64,
        (checked + ROWS) as i64,
    );
    put("scrollback_cells_sha256", sha256(&cells.payload));
    put(
        "scrollback_metadata_sha256",
        sha256(without_store_nonce(format!("{:?}", cells.metadata)).as_bytes()),
    );
    let links = screen.link_source(checked);
    let link_list: Vec<String> = links
        .hyperlinks
        .into_iter()
        .map(|(at, uri)| format!("{at} {}", without_store_nonce(uri)))
        .collect();
    put(
        "links",
        format!(
            "{} sha256={}",
            link_list.len(),
            sha256(format!("{}\n{}", links.text, link_list.join("\n")).as_bytes())
        ),
    );
    put("title", format!("{:?}", screen.title()));
    put("cursor", format!("{:?}", screen.cursor()));
    put(
        "modes",
        format!(
            "alt={} paste={} focus={} mouse={:?} cursor={:?}",
            screen.is_alt_screen(),
            screen.bracketed_paste(),
            screen.focus_reporting(),
            screen.mouse_modes(),
            screen.cursor_style(),
        ),
    );
    put("keyboard", format!("{:?}", screen.keyboard_state()));
    put("progress", format!("{:?}", screen.progress()));
    put("program_status", format!("{:?}", screen.program_status()));
    put(
        "synchronized",
        format!(
            "completed_any={} open={}",
            screen.synchronized_updates_completed() > 0,
            screen.in_synchronized_update()
        ),
    );
    d
}

/// Sections of a golden file, keyed by `name payload_sha256`.
type Golden = BTreeMap<String, BTreeMap<String, String>>;

const GOLDEN_FILE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/benches/vt_workloads.golden");

fn golden_key(digest: &BTreeMap<String, String>) -> String {
    format!("{} {}", digest["workload"], digest["payload_sha256"])
}

fn read_golden(path: &Path) -> (String, Golden) {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let mut header = String::new();
    let mut golden = Golden::new();
    let mut section: Option<String> = None;
    for line in text.lines() {
        if let Some(key) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            section = Some(key.to_owned());
            golden.entry(key.to_owned()).or_default();
        } else if let (Some(key), Some((field, value))) = (&section, line.split_once(" = ")) {
            golden
                .get_mut(key)
                .expect("section")
                .insert(field.to_owned(), value.to_owned());
        } else if section.is_none() {
            header.push_str(line);
            header.push('\n');
        }
    }
    (header, golden)
}

fn write_golden(path: &Path, header: &str, golden: &Golden) {
    let mut text = header.to_owned();
    for (key, fields) in golden {
        let _ = writeln!(text, "[{key}]");
        for (field, value) in fields {
            let _ = writeln!(text, "{field} = {value}");
        }
        text.push('\n');
    }
    std::fs::write(path, text).expect("write golden file");
}

/// The fields of `got` that differ from `want`, skipping `skip`.
fn differences(
    want: &BTreeMap<String, String>,
    got: &BTreeMap<String, String>,
    skip: &[&str],
) -> Vec<String> {
    want.keys()
        .chain(got.keys().filter(|key| !want.contains_key(*key)))
        .filter(|key| !skip.contains(&key.as_str()))
        .filter(|key| want.get(*key) != got.get(*key))
        .map(|key| {
            format!(
                "  {key}:\n    golden: {}\n    run:    {}",
                want.get(key).map_or("(missing)", String::as_str),
                got.get(key).map_or("(missing)", String::as_str)
            )
        })
        .collect()
}

fn refuse(name: &str, what: &str, diffs: &[String]) -> ! {
    eprintln!(
        "{name}: {what} does not match the golden reference; no number is reported.\n{}",
        diffs.join("\n")
    );
    std::process::exit(1);
}

fn load(name: &str, dir: Option<&std::ffi::OsString>) -> (Vec<u8>, &'static str) {
    match dir {
        Some(dir) => (
            std::fs::read(Path::new(dir).join(format!("{name}.bin")))
                .unwrap_or_else(|error| panic!("{name}.bin: {error}")),
            "kitten",
        ),
        None => (generate(name), "generated"),
    }
}

fn main() {
    let read = std::env::var("DIRI_VT_BENCH_READ")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(64 << 10);
    let dir = std::env::var_os("DIRI_VT_BENCH_DIR");
    if let Some(out) = std::env::var_os("DIRI_VT_BENCH_WRITE") {
        let out = Path::new(&out);
        std::fs::create_dir_all(out).expect("output directory");
        for name in WORKLOADS {
            let (data, _) = load(name, dir.as_ref());
            std::fs::write(out.join(format!("{name}.bin")), data).expect("write payload");
        }
        return;
    }
    if let Some(path) = std::env::var_os("DIRI_VT_BENCH_GOLDEN") {
        // The reference: one byte per read, checksum on.
        let path = Path::new(&path);
        let (header, mut golden) = read_golden(path);
        for name in WORKLOADS {
            let (data, source) = load(name, dir.as_ref());
            let reps = MIN_BYTES_PER_RUN.div_ceil(data.len());
            let started = Instant::now();
            let (_, reference) = run(&data, reps, 1, true);
            let digest = digest(name, &data, reps, reference, None);
            println!(
                "{name:<18} {source}: reference digest in {:.1} s",
                started.elapsed().as_secs_f64()
            );
            golden.insert(golden_key(&digest), digest);
        }
        write_golden(path, &header, &golden);
        return;
    }
    let only = std::env::var("DIRI_VT_BENCH_ONLY").ok();
    let selected = |name: &str| {
        only.as_deref()
            .is_none_or(|only| only.split(',').any(|item| item.trim() == name))
    };
    let (_, golden) = read_golden(Path::new(GOLDEN_FILE));
    println!(
        "workload            MB/s  (median of {RUNS}; min..max)   source  read={read}  {COLS}x{ROWS}  checked: golden"
    );
    for name in WORKLOADS.into_iter().filter(|name| selected(name)) {
        let (data, source) = load(name, dir.as_ref());
        let reps = MIN_BYTES_PER_RUN.div_ceil(data.len());
        let key = format!("{name} {}", sha256(&data));
        let Some(want) = golden.get(&key) else {
            eprintln!(
                "{name}: no golden digest for this {source} payload ({key}); no number is \
                 reported. Generate one with DIRI_VT_BENCH_GOLDEN at a known-good commit."
            );
            std::process::exit(1);
        };
        let checked_rows = want
            .get("history_rows_checked")
            .and_then(|rows| rows.parse().ok())
            .expect("golden history_rows_checked");
        let check = |run| digest(name, &data, reps, run, Some(checked_rows));
        // Untimed: the full check, OSC checksum included, at this read size.
        let (_, checked) = run(&data, reps, read, true);
        let diffs = differences(want, &check(checked), &[REFERENCE_ONLY]);
        if !diffs.is_empty() {
            refuse(name, "the checked run", &diffs);
        }
        let mut rates = Vec::with_capacity(RUNS);
        for index in 0..RUNS {
            let (elapsed, timed) = run(&data, reps, read, false);
            let diffs = differences(want, &check(timed), &[REFERENCE_ONLY, OSC_CHECKSUM]);
            if !diffs.is_empty() {
                refuse(name, &format!("timed run {}", index + 1), &diffs);
            }
            rates.push((data.len() * reps) as f64 / elapsed.as_secs_f64() / 1e6);
        }
        rates.sort_by(f64::total_cmp);
        println!(
            "{name:<18} {:8.1}  ({:.1}..{:.1})   {source}",
            rates[RUNS / 2],
            rates[0],
            rates[RUNS - 1]
        );
    }
}
