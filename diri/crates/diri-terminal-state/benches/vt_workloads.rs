//! Headless VT throughput on the six `kitten __benchmark__` workloads.
//!
//! ASCII, Unicode, unique multi-codepoint cells, CSI with few characters,
//! Kitty images and long OSCs, parsed into a 120x80 terminal configured as
//! the local Engine configures it (notifications, inline images, host
//! replies, kitty keyboard). Every byte is parsed, and the bench proves it:
//! each timed run counts the bytes it fed, and its final state (grid,
//! cursor, every retained history row with its cells and metadata, links,
//! title, modes, stored images, replies and notifications) must equal a
//! parse of the same bytes fed one at a time, or the bench panics without a
//! number. Nothing a run starts is left working when its clock stops.
//!
//! The data is the kitten's own when `DIRI_VT_BENCH_DIR` names a directory
//! holding `ascii.bin`, `unicode.bin`, `unique_unicode.bin`, `csi.bin`,
//! `images.bin` and `long_escape_codes.bin` (one repetition of each payload,
//! captured from the kitten's output). Otherwise the payloads are generated
//! the way kitty 0.48 generates them (`tools/cmd/benchmark/main.go`), with a
//! fixed seed.
//!
//! ```sh
//! cargo bench -p diri-terminal-state --bench vt_workloads
//! DIRI_VT_BENCH_READ=1024 cargo bench -p diri-terminal-state --bench vt_workloads
//! ```
//!
//! `DIRI_VT_BENCH_WRITE=<dir>` writes the payloads it uses there and exits,
//! for `diri-engine`'s `vtbench` (the same workloads through Holder and
//! Engine).
//!
//! `DIRI_VT_BENCH_ONLY=ascii,unicode` runs only the named workloads.
//! `DIRI_VT_BENCH_REFERENCE=<dir>` keeps the byte-at-a-time reference states
//! there, so interleaved runs of several builds compute each once and are
//! all checked against it.
//!
//! `DIRI_VT_BENCH_READ` sets the read size (default 64 KiB; a macOS PTY hands
//! over about 1 KiB per read). Reported: MB/s (10^6 bytes) of wall time for
//! `feed` alone, median of 7 runs of at least 64 MB each.

use std::hint::black_box;
use std::time::{Duration, Instant};

use diri_terminal_state::HeadlessScreen;

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

/// What a run leaves behind: the screen and everything it handed out.
struct Fed {
    screen: HeadlessScreen,
    /// Bytes handed to the parser.
    bytes: usize,
    replies: Vec<u8>,
    notifications: Vec<String>,
    clipboard: Vec<String>,
}

impl Fed {
    fn new() -> Self {
        Self {
            screen: screen(),
            bytes: 0,
            replies: Vec::new(),
            notifications: Vec::new(),
            clipboard: Vec::new(),
        }
    }

    /// One read, handled the way the Engine pump handles it.
    #[inline]
    fn feed(&mut self, chunk: &[u8]) {
        self.bytes += chunk.len();
        self.screen.feed(black_box(chunk));
        self.replies.extend(self.screen.take_replies());
        for notification in self.screen.take_notifications() {
            self.notifications.push(format!("{notification:?}"));
        }
        self.clipboard.extend(self.screen.take_clipboard());
    }

    /// Every observable piece of state, for comparing two runs: the grid,
    /// cursor, all retained history with its cells and metadata, links,
    /// title, modes, progress, stored images, replies and notifications.
    fn state(mut self) -> String {
        let screen = &mut self.screen;
        let scrollback = screen.scrollback();
        let cells = screen.scrollback_cells(0, i64::MAX);
        let links = screen.link_source(usize::MAX);
        format!(
            "{:?}\n{:?}\n{}\n{:?}\n{:?}\n{}\n{:?}\n{:?}\n{:?} {} {} {:?} {:?}\n{:?}\n{:?}\n{:?}\n{}\n{:?}\n{:?}\n{:?}\n{:?}",
            scrollback.lines,
            scrollback.text_cells,
            scrollback.visible_start_row,
            cells.payload,
            without_store_nonce(format!("{:?}", cells.metadata)),
            without_store_nonce(format!("{:?}", screen.full_snapshot())),
            links.text,
            links
                .hyperlinks
                .into_iter()
                .map(|(at, uri)| (at, without_store_nonce(uri)))
                .collect::<Vec<_>>(),
            screen.title(),
            screen.is_alt_screen(),
            screen.bracketed_paste(),
            screen.mouse_modes(),
            screen.cursor_style(),
            screen.cursor(),
            screen.keyboard_state(),
            screen.progress(),
            screen.inline_image_bytes(),
            format!("{:?}", screen.program_status()),
            self.replies,
            self.notifications,
            self.clipboard,
        )
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

/// One run: feed `reps` copies of `data` in `read`-sized pieces, handling
/// replies and notifications the way the Engine pump does. The clock stops
/// after the last read returns; nothing the run started is still working
/// then (history blocks being encoded are collected by each read).
fn run(data: &[u8], reps: usize, read: usize) -> (Duration, Fed) {
    let mut fed = Fed::new();
    let started = Instant::now();
    for _ in 0..reps {
        for chunk in data.chunks(read) {
            fed.feed(chunk);
        }
    }
    let elapsed = started.elapsed();
    assert_eq!(fed.bytes, data.len() * reps, "every payload byte fed");
    (elapsed, fed)
}

/// The reference: the same bytes parsed one at a time, which never takes
/// the parser's multi-byte fast paths.
fn reference(data: &[u8], reps: usize) -> String {
    let mut fed = Fed::new();
    for _ in 0..reps {
        for byte in data.chunks(1) {
            fed.feed(byte);
        }
    }
    assert_eq!(fed.bytes, data.len() * reps);
    fed.state()
}

/// [`reference`], kept in `DIRI_VT_BENCH_REFERENCE` (a directory) when set,
/// so interleaved runs of several builds parse each payload one byte at a
/// time once and are all held to that same state. Delete the directory when
/// the expected terminal state changes.
fn cached_reference(name: &str, data: &[u8], reps: usize) -> String {
    let Some(dir) = std::env::var_os("DIRI_VT_BENCH_REFERENCE") else {
        return reference(data, reps);
    };
    // FNV-1a of the payload names the file.
    let hash = data.iter().fold(0xcbf2_9ce4_8422_2325u64, |hash, &byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x100_0000_01b3)
    });
    let path = std::path::Path::new(&dir).join(format!("{name}-{reps}x-{hash:016x}.state"));
    if let Ok(state) = std::fs::read_to_string(&path) {
        return state;
    }
    let state = reference(data, reps);
    std::fs::create_dir_all(&dir).expect("reference directory");
    std::fs::write(&path, &state).expect("write reference");
    state
}

/// Panics, naming the first difference, unless a run ended in the
/// reference's state.
fn check(name: &str, expected: &str, fed: Fed) {
    let actual = fed.state();
    if actual == expected {
        return;
    }
    let at = expected
        .bytes()
        .zip(actual.bytes())
        .position(|(a, b)| a != b)
        .unwrap_or(expected.len().min(actual.len()));
    let window = |s: &str| {
        let start = s.floor_char_boundary(at.saturating_sub(80));
        let end = s.ceil_char_boundary((at + 80).min(s.len()));
        s[start..end].to_owned()
    };
    panic!(
        "{name}: final state differs from a byte-at-a-time parse at {at}\n  reference: {}\n  run:       {}",
        window(expected),
        window(&actual)
    );
}

fn main() {
    let read = std::env::var("DIRI_VT_BENCH_READ")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(64 << 10);
    let dir = std::env::var_os("DIRI_VT_BENCH_DIR");
    println!(
        "workload            MB/s  (median of {RUNS}; min..max)   source  read={read}  {COLS}x{ROWS}"
    );
    if let Some(out) = std::env::var_os("DIRI_VT_BENCH_WRITE") {
        let out = std::path::Path::new(&out);
        std::fs::create_dir_all(out).expect("output directory");
        for name in WORKLOADS {
            let data = match &dir {
                Some(dir) => std::fs::read(std::path::Path::new(dir).join(format!("{name}.bin")))
                    .expect("captured payload"),
                None => generate(name),
            };
            std::fs::write(out.join(format!("{name}.bin")), data).expect("write payload");
        }
        return;
    }
    let only = std::env::var("DIRI_VT_BENCH_ONLY").ok();
    for name in WORKLOADS {
        if only
            .as_deref()
            .is_some_and(|only| !only.split(',').any(|n| n == name))
        {
            continue;
        }
        let (data, source) = match &dir {
            Some(dir) => (
                std::fs::read(std::path::Path::new(dir).join(format!("{name}.bin")))
                    .unwrap_or_else(|error| panic!("{name}.bin: {error}")),
                "kitten",
            ),
            None => (generate(name), "generated"),
        };
        let reps = MIN_BYTES_PER_RUN.div_ceil(data.len());
        // Every run must end where parsing the same bytes one at a time ends.
        let expected = cached_reference(name, &data, reps);
        let _ = run(&data, 1, read); // warm-up
        let mut rates: Vec<f64> = (0..RUNS)
            .map(|_| {
                let (elapsed, fed) = run(&data, reps, read);
                check(name, &expected, fed);
                (data.len() * reps) as f64 / elapsed.as_secs_f64() / 1e6
            })
            .collect();
        rates.sort_by(f64::total_cmp);
        println!(
            "{name:<18} {:8.1}  ({:.1}..{:.1})   {source}",
            rates[RUNS / 2],
            rates[0],
            rates[RUNS - 1]
        );
    }
}
