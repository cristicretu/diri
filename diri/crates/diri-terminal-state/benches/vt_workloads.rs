//! Headless VT throughput on the six `kitten __benchmark__` workloads.
//!
//! ASCII, Unicode, unique multi-codepoint cells, CSI with few characters,
//! Kitty images and long OSCs, parsed into a 120x80 terminal configured as
//! the local Engine configures it (compact history with its default
//! scrollback, notifications, inline images, host replies, kitty keyboard),
//! with replies, notifications and clipboard writes taken after every read.
//!
//! Every byte is parsed, and each number is checked before it is printed:
//! every timed run must hand the parser exactly the payload's bytes and end
//! in the same state as a reference parse of the same bytes fed one at a
//! time: visible grid, every scrollback row's cells and metadata, cursor,
//! modes, title, hyperlinks, replies, notifications and stored images (ids,
//! sizes and a hash of their bytes). Kitten's image payload deletes its image
//! at the end, so it is also compared once with the deletes removed. A run
//! that differs panics instead of reporting.
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
//! `DIRI_VT_BENCH_ONLY=csi,images` runs only the named workloads, and
//! `DIRI_VT_BENCH_MB=1024` makes each run at least that many MiB (for
//! profiling).
//!
//! `DIRI_VT_BENCH_READ` sets the read size (default 64 KiB; a macOS PTY hands
//! over about 1 KiB per read). Reported: MB/s (10^6 bytes) of wall time for
//! the reads alone (the checks are untimed), median of 7 runs of at least 64
//! MiB each.

use std::hint::black_box;
use std::time::{Duration, Instant};

#[path = "../tests/support/observed.rs"]
mod observed;
use observed::{Feeder, assert_same};

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

/// One run: feed `reps` copies of `data` in `read`-sized pieces, taking
/// replies, notifications and clipboard writes after each read as the Engine
/// pump does. Returns the time spent and the bytes handed to the parser.
fn run(data: &[u8], reps: usize, read: usize) -> (Duration, usize, Feeder) {
    let mut feeder = Feeder::new(COLS, ROWS);
    let mut fed = 0;
    let started = Instant::now();
    for _ in 0..reps {
        for chunk in data.chunks(read) {
            feeder.feed(black_box(chunk));
            fed += chunk.len();
        }
    }
    (started.elapsed(), fed, feeder)
}

/// The reference: the same bytes fed one at a time, so every escape, string
/// body and character crosses a read boundary and goes through the parsers'
/// per-byte paths.
fn reference(data: &[u8], reps: usize) -> Feeder {
    let mut feeder = Feeder::new(COLS, ROWS);
    for _ in 0..reps {
        for byte in data.chunks(1) {
            feeder.feed(byte);
        }
    }
    feeder
}

/// Requires `actual` to end where `expected` did. Past the history storage
/// budget, how many of the oldest rows are still held depends on where reads
/// ended; the rows both hold are compared, and they must be nearly all.
fn assert_same_state(expected: &mut Feeder, actual: &mut Feeder, how: &str) {
    let (held, reference_held) = (actual.screen.history_rows(), expected.screen.history_rows());
    let keep = held.min(reference_held);
    assert!(
        held.abs_diff(reference_held) <= 1 + reference_held / 100,
        "{how}: holds {held} history rows, the reference {reference_held}"
    );
    assert_same(
        &expected.observe_newest(Some(keep)),
        &actual.observe_newest(Some(keep)),
        how,
    );
}

/// `data` without Kitty delete commands (`a=d`), so the images it transmits
/// stay stored and the comparison covers their decoded bytes. `None` when
/// there is none to remove.
fn without_image_deletes(data: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(data.len());
    let mut at = 0;
    let mut removed = false;
    while let Some(start) = memchr::memmem::find(&data[at..], b"\x1b_G").map(|start| at + start) {
        let end = memchr::memmem::find(&data[start..], b"\x1b\\")
            .map_or(data.len(), |end| start + end + 2);
        let control_end = memchr::memchr(b';', &data[start..end]).map_or(end, |semi| start + semi);
        out.extend_from_slice(&data[at..start]);
        if data[start + 3..control_end]
            .split(|&byte| byte == b',')
            .any(|key| key == b"a=d")
        {
            removed = true;
        } else {
            out.extend_from_slice(&data[start..end]);
        }
        at = end;
    }
    out.extend_from_slice(&data[at..]);
    removed.then_some(out)
}

fn main() {
    let read = std::env::var("DIRI_VT_BENCH_READ")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(64 << 10);
    let dir = std::env::var_os("DIRI_VT_BENCH_DIR");
    let only = std::env::var("DIRI_VT_BENCH_ONLY").ok();
    let min_bytes = std::env::var("DIRI_VT_BENCH_MB")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .map_or(MIN_BYTES_PER_RUN, |mb| mb << 20);
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
    println!(
        "workload            MB/s  (median of {RUNS}; min..max)   source  read={read}  {COLS}x{ROWS}"
    );
    for name in WORKLOADS {
        if only
            .as_deref()
            .is_some_and(|only| !only.split(',').any(|wanted| wanted == name))
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
        let reps = min_bytes.div_ceil(data.len());
        // The images a payload deletes again are compared too.
        if let Some(kept) = without_image_deletes(&data) {
            let (_, _, mut chunked) = run(&kept, 1, read);
            let mut expected = reference(&kept, 1);
            assert!(
                !expected.screen.inline_image_store_summary().is_empty(),
                "{name}: images kept"
            );
            assert_same_state(
                &mut expected,
                &mut chunked,
                &format!("{name} without deletes"),
            );
        }
        let mut expected = reference(&data, reps);
        let _ = run(&data, 1, read); // warm-up
        let mut rates: Vec<f64> = (0..RUNS)
            .map(|_| {
                let (elapsed, fed, mut feeder) = run(&data, reps, read);
                // A run that dropped or changed anything reports nothing.
                assert_eq!(fed, data.len() * reps, "{name}: bytes fed");
                assert_same_state(
                    &mut expected,
                    &mut feeder,
                    &format!("{name} in {read}-byte reads"),
                );
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
