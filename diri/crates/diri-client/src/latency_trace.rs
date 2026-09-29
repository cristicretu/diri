//! Keystroke → echo → present latency, hop by hop, for the desktop client.
//!
//! Off unless `DIRI_LATENCY_TRACE=1` is in the environment when the process
//! first asks. Off, every probe is one relaxed atomic load and nothing else:
//! no clock read, no lock, no allocation.
//!
//! On, the trace follows one keystroke at a time, which is what typing is:
//! a key starts a record, each later hop stamps it once and only if the hop
//! before it already fired, and the record closes at the last hop the
//! platform reports (an on-screen present, or GPU completion when rendering
//! headlessly). A new key abandons an unfinished record, so an echo that
//! never changed the screen cannot pair with the next key's frame.
//!
//! The hops are stamped by the code that performs them: the pane's key
//! handler and input admission, the attachment's socket writer and frame
//! decoder, the transport task's mailbox hand-off, the GPUI thread's grid
//! apply and pane notification, and GPUI/Metal frame callbacks.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::{Duration, Instant};

/// One step on the keystroke's way to the screen, in path order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum Hop {
    /// GPUI delivered the key to the terminal pane.
    KeyDown = 0,
    /// The encoded bytes were admitted to the attachment's queue.
    InputQueued,
    /// The attachment task finished writing the input frame to the socket.
    SocketWritten,
    /// A grid frame arrived and decoded after that write (the echo).
    EchoDecoded,
    /// The transport task handed the echo to the pane mailbox.
    MailboxQueued,
    /// The GPUI thread applied the echo to the shared grid and it changed.
    GridApplied,
    /// The pane handled the damage and invalidated its view.
    PaneNotified,
    /// GPUI started drawing the window frame that includes the echo.
    DrawStart,
    /// GPUI finished building that frame's scene.
    DrawEnd,
    /// The frame's Metal command buffer was committed.
    Committed,
    /// The GPU finished executing that command buffer.
    GpuCompleted,
    /// The compositor reported the drawable on screen.
    Presented,
}

pub const HOPS: [Hop; 12] = [
    Hop::KeyDown,
    Hop::InputQueued,
    Hop::SocketWritten,
    Hop::EchoDecoded,
    Hop::MailboxQueued,
    Hop::GridApplied,
    Hop::PaneNotified,
    Hop::DrawStart,
    Hop::DrawEnd,
    Hop::Committed,
    Hop::GpuCompleted,
    Hop::Presented,
];

impl Hop {
    pub const fn label(self) -> &'static str {
        match self {
            Self::KeyDown => "key down",
            Self::InputQueued => "input queued",
            Self::SocketWritten => "socket written",
            Self::EchoDecoded => "echo decoded",
            Self::MailboxQueued => "mailbox queued",
            Self::GridApplied => "grid applied",
            Self::PaneNotified => "pane notified",
            Self::DrawStart => "draw start",
            Self::DrawEnd => "draw end",
            Self::Committed => "committed",
            Self::GpuCompleted => "gpu completed",
            Self::Presented => "presented",
        }
    }
}

const UNKNOWN: u8 = 0;
const OFF: u8 = 1;
const ON: u8 = 2;
static STATE: AtomicU8 = AtomicU8::new(UNKNOWN);

/// Whether hops are being recorded. Reads the environment once.
#[inline]
pub fn enabled() -> bool {
    match STATE.load(Ordering::Relaxed) {
        ON => true,
        OFF => false,
        _ => init(),
    }
}

#[cold]
fn init() -> bool {
    let on = std::env::var_os("DIRI_LATENCY_TRACE").is_some_and(|value| value == "1");
    STATE.store(if on { ON } else { OFF }, Ordering::Relaxed);
    on
}

/// Turns recording on or off regardless of the environment (harnesses).
pub fn set_enabled(on: bool) {
    STATE.store(if on { ON } else { OFF }, Ordering::Relaxed);
}

struct Record {
    at: [Option<Instant>; HOPS.len()],
}

impl Record {
    fn new(key: Instant) -> Self {
        let mut at = [None; HOPS.len()];
        at[Hop::KeyDown as usize] = Some(key);
        Self { at }
    }
}

#[derive(Default)]
struct Trace {
    current: Option<Record>,
    finished: Vec<[Option<Instant>; HOPS.len()]>,
    /// How many finished records the last periodic summary covered.
    printed: usize,
}

static TRACE: Mutex<Option<Trace>> = Mutex::new(None);

/// How many finished records are kept for [`summary`]; older ones roll off.
const KEEP: usize = 4096;
/// With `DIRI_LATENCY_TRACE=1` the app prints a summary every this many keys.
const PRINT_EVERY: usize = 100;

/// Stamps `hop` now. A no-op unless tracing is on.
#[inline]
pub fn mark(hop: Hop) {
    if enabled() {
        mark_at(hop, Instant::now());
    }
}

/// Stamps `hop` at `at` (for platform callbacks that report their own time).
pub fn mark_at(hop: Hop, at: Instant) {
    if !enabled() {
        return;
    }
    let mut guard = TRACE.lock().unwrap_or_else(|poison| poison.into_inner());
    let trace = guard.get_or_insert_with(Trace::default);
    if hop == Hop::KeyDown {
        trace.current = Some(Record::new(at));
        return;
    }
    let Some(record) = trace.current.as_mut() else {
        return;
    };
    let index = hop as usize;
    if record.at[index].is_some() || record.at[index - 1].is_none() {
        return;
    }
    record.at[index] = Some(at);
    let closes = hop == Hop::Presented || (hop == Hop::GpuCompleted && !expects_present());
    if closes {
        let record = trace.current.take().expect("current record");
        if trace.finished.len() >= KEEP {
            trace.finished.remove(0);
        }
        trace.finished.push(record.at);
        if std::env::var_os("DIRI_LATENCY_TRACE").is_some()
            && trace.finished.len() >= trace.printed + PRINT_EVERY
        {
            trace.printed = trace.finished.len();
            let report = render(&trace.finished);
            drop(guard);
            eprintln!("{report}");
        }
    }
}

static EXPECTS_PRESENT: AtomicU8 = AtomicU8::new(1);

/// Headless renderers never present; their records close at GPU completion.
pub fn set_expects_present(expects: bool) {
    EXPECTS_PRESENT.store(u8::from(expects), Ordering::Relaxed);
}

fn expects_present() -> bool {
    EXPECTS_PRESENT.load(Ordering::Relaxed) != 0
}

/// The record in flight has a stamp for `hop` (tests and harnesses).
pub fn current_has(hop: Hop) -> bool {
    let guard = TRACE.lock().unwrap_or_else(|poison| poison.into_inner());
    guard
        .as_ref()
        .and_then(|trace| trace.current.as_ref())
        .is_some_and(|record| record.at[hop as usize].is_some())
}

/// How many records have finished since the last [`drain`].
pub fn finished_len() -> usize {
    let guard = TRACE.lock().unwrap_or_else(|poison| poison.into_inner());
    guard.as_ref().map_or(0, |trace| trace.finished.len())
}

/// Finished records' stamps, oldest first, and clears them.
pub fn drain() -> Vec<[Option<Instant>; HOPS.len()]> {
    let mut guard = TRACE.lock().unwrap_or_else(|poison| poison.into_inner());
    guard
        .as_mut()
        .map(|trace| {
            trace.printed = 0;
            trace.current = None;
            std::mem::take(&mut trace.finished)
        })
        .unwrap_or_default()
}

/// p50/p95/max per hop over finished records, one line each.
pub fn render(records: &[[Option<Instant>; HOPS.len()]]) -> String {
    let mut out = format!("diri latency trace: {} keystrokes\n", records.len());
    for pair in HOPS.windows(2) {
        let samples = records
            .iter()
            .filter_map(|record| {
                Some(record[pair[1] as usize]?.saturating_duration_since(record[pair[0] as usize]?))
            })
            .collect::<Vec<_>>();
        out.push_str(&line(
            &format!("{} -> {}", pair[0].label(), pair[1].label()),
            samples,
        ));
    }
    let last =
        |record: &[Option<Instant>; HOPS.len()]| record.iter().rev().flatten().next().copied();
    let total = records
        .iter()
        .filter_map(|record| Some(last(record)?.saturating_duration_since(record[0]?)))
        .collect::<Vec<_>>();
    out.push_str(&line("key down -> last hop", total));
    out
}

fn line(label: &str, mut samples: Vec<Duration>) -> String {
    if samples.is_empty() {
        return format!("  {label:<34} no samples\n");
    }
    samples.sort();
    let pick = |q: f64| {
        samples[((samples.len() as f64 - 1.0) * q).round() as usize].as_secs_f64() * 1000.0
    };
    format!(
        "  {label:<34} p50 {:>8.3} ms  p95 {:>8.3} ms  max {:>8.3} ms  (n={})\n",
        pick(0.5),
        pick(0.95),
        pick(1.0),
        samples.len()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // One test owns the process-wide switch so parallel tests cannot race it.
    #[test]
    fn hops_stamp_in_order_and_a_new_key_abandons_an_unfinished_record() {
        set_enabled(true);
        set_expects_present(false);
        drain();
        let start = Instant::now();
        let at = |ms: u64| start + Duration::from_millis(ms);

        // Out of order: an echo before any write is not this key's echo.
        mark_at(Hop::KeyDown, at(0));
        mark_at(Hop::EchoDecoded, at(1));
        assert!(!current_has(Hop::EchoDecoded));
        // Abandoned by the next key.
        mark_at(Hop::KeyDown, at(10));
        for (offset, hop) in HOPS[1..=Hop::GpuCompleted as usize].iter().enumerate() {
            mark_at(*hop, at(11 + offset as u64));
            // A repeat stamp never moves a hop later.
            mark_at(*hop, at(100));
        }
        let records = drain();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0][0], Some(at(10)));
        assert_eq!(records[0][Hop::GpuCompleted as usize], Some(at(20)));
        assert!(render(&records).contains("key down -> last hop"));

        set_enabled(false);
        mark_at(Hop::KeyDown, at(30));
        assert!(!current_has(Hop::KeyDown));
        set_expects_present(true);
    }
}
