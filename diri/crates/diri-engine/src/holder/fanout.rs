//! A bounded queue of output frames, one per attached subscriber.
//!
//! This exists instead of `mpsc::sync_channel` for one reason: the PTY pump
//! needs to wait for room *with a deadline*. `SyncSender::send` waits forever,
//! which lets a hung daemon hold the PTY, and `try_send` in a sleep loop pays a
//! millisecond of dead time per chunk — enough, at PTY chunk sizes, to cut
//! throughput to a third. A condvar gives an exact bounded wait: the pump
//! sleeps only until the writer actually drains a frame.
//!
//! Waiting at all is deliberate. It is the backpressure a single-process
//! terminal gets for free — output cannot race far ahead of the screen showing
//! it — and the deadline is what keeps that from becoming a way to wedge the
//! PTY.
//!
//! The bound is in bytes, and bytes pushed behind a waiting frame join it. A
//! macOS PTY read is one kilobyte, so a bound counted in frames was sixteen
//! kilobytes of slack rather than the intended megabyte, and a subscriber that
//! fell a little behind received — and paid a header, a wakeup and a read
//! for — every kilobyte separately. Joining contiguous bytes is invisible on
//! the wire: a frame is just an offset and the bytes that start there.

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// One frame: where it starts in the session's output stream, and its bytes.
pub(super) type Frame = (u64, Vec<u8>);

/// The largest frame joining will build. Well under
/// [`super::protocol::HOLDER_OUTPUT_MAX_FRAME`], so every subscriber that
/// ever spoke this stream accepts it.
pub(super) const JOINED_FRAME_BYTES: usize = 256 << 10;
const _: () = assert!(JOINED_FRAME_BYTES <= super::protocol::HOLDER_OUTPUT_MAX_FRAME);

struct State {
    frames: VecDeque<Frame>,
    /// Bytes across `frames`.
    queued: usize,
    /// Set when the subscriber is gone. Both ends check it so neither waits on
    /// the other after the socket has failed.
    closed: bool,
}

pub(super) struct FrameQueue {
    state: Mutex<State>,
    /// Signals both directions: room appeared, or a frame did.
    changed: Condvar,
    /// Bytes that may wait before the pump has to.
    capacity: usize,
    /// Completed predicate waits, excluding spurious condvar wakeups.
    #[cfg(test)]
    pub(super) wakeups: std::sync::atomic::AtomicUsize,
}

impl FrameQueue {
    /// A queue holding up to `capacity` bytes.
    pub(super) fn new(capacity: usize) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State {
                frames: VecDeque::new(),
                queued: 0,
                closed: false,
            }),
            changed: Condvar::new(),
            capacity: capacity.max(1),
            #[cfg(test)]
            wakeups: std::sync::atomic::AtomicUsize::new(0),
        })
    }

    /// Queues the bytes starting at `offset`, waiting up to `patience` for
    /// room.
    ///
    /// Returns false if the subscriber is gone or is still full when patience
    /// runs out — in both cases the caller drops it, and it resumes from the
    /// log, which has every byte.
    pub(super) fn push(&self, offset: u64, bytes: &[u8], patience: Duration) -> bool {
        let mut state = self.state.lock().expect("frame queue");
        if !state.closed && state.queued >= self.capacity {
            let deadline = Instant::now() + patience;
            while !state.closed && state.queued >= self.capacity {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return false;
                }
                let (guard, _) = self
                    .changed
                    .wait_timeout(state, remaining)
                    .expect("frame queue");
                state = guard;
            }
        }
        if state.closed {
            return false;
        }
        let was_empty = state.frames.is_empty();
        state.queued += bytes.len();
        // Only a frame still waiting can take more; the reader owns one it
        // has popped. Only contiguous bytes join, so a frame stays exactly
        // "these bytes, starting here".
        match state.frames.back_mut() {
            Some((start, frame))
                if *start + frame.len() as u64 == offset
                    && frame.len() + bytes.len() <= JOINED_FRAME_BYTES =>
            {
                frame.extend_from_slice(bytes);
            }
            _ => state.frames.push_back((offset, bytes.to_vec())),
        }
        drop(state);
        // A reader parks only on an empty queue, so a push onto a non-empty
        // one has nobody to wake.
        if was_empty {
            self.changed.notify_all();
        }
        true
    }

    /// Takes the next frame, waiting for as long as it takes one to arrive.
    ///
    /// There is no deadline on purpose: a silent session must leave its
    /// subscribers parked, not ticking. `None` means the queue closed, which
    /// is how every reason to stop waiting — the child exiting, the pump
    /// giving up, the peer hanging up — reaches the reader.
    pub(super) fn pop(&self) -> Option<Frame> {
        let mut state = self.state.lock().expect("frame queue");
        loop {
            if let Some(frame) = Self::take_front(&mut state) {
                return Some(self.released(state, frame));
            }
            if state.closed {
                return None;
            }
            state = self
                .changed
                .wait_while(state, |state| state.frames.is_empty() && !state.closed)
                .expect("frame queue");
            #[cfg(test)]
            self.wakeups
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// Takes a frame only if one is already waiting.
    #[cfg(any(unix, test))]
    pub(super) fn try_pop(&self) -> Option<Frame> {
        let mut state = self.state.lock().expect("frame queue");
        let frame = Self::take_front(&mut state)?;
        Some(self.released(state, frame))
    }

    fn take_front(state: &mut State) -> Option<Frame> {
        let frame = state.frames.pop_front()?;
        state.queued -= frame.1.len();
        Some(frame)
    }

    /// Wakes a pump waiting for room, if taking `frame` just made some.
    fn released(&self, state: std::sync::MutexGuard<'_, State>, frame: Frame) -> Frame {
        let was_full = state.queued + frame.1.len() >= self.capacity;
        drop(state);
        if was_full {
            self.changed.notify_all();
        }
        frame
    }

    /// Marks the subscriber gone and wakes anyone waiting on it.
    pub(super) fn close(&self) {
        self.state.lock().expect("frame queue").closed = true;
        self.changed.notify_all();
    }

    #[cfg(test)]
    pub(super) fn is_closed(&self) -> bool {
        self.state.lock().expect("frame queue").closed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn push(queue: &FrameQueue, offset: u64, patience: Duration) -> bool {
        queue.push(offset, b"x", patience)
    }

    #[test]
    fn a_full_queue_makes_the_writer_wait_only_until_there_is_room() {
        let queue = FrameQueue::new(1);
        assert!(push(&queue, 0, Duration::from_millis(10)));

        let drainer = {
            let queue = Arc::clone(&queue);
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(20));
                queue.pop()
            })
        };
        // Room appears only once the drainer runs, so this push must wait for
        // it rather than give up early or block forever.
        assert!(push(&queue, 1, Duration::from_secs(1)));
        assert_eq!(drainer.join().expect("drainer").map(|f| f.0), Some(0));
    }

    #[test]
    fn a_subscriber_that_never_drains_is_given_up_on() {
        let queue = FrameQueue::new(1);
        assert!(push(&queue, 0, Duration::from_millis(10)));
        let started = Instant::now();
        assert!(!push(&queue, 1, Duration::from_millis(20)));
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "push should give up at its deadline, not hang"
        );
    }

    #[test]
    fn a_parked_reader_wakes_only_for_a_frame_or_a_close() {
        let queue = FrameQueue::new(1);
        let reader = {
            let queue = Arc::clone(&queue);
            std::thread::spawn(move || (queue.pop().map(|f| f.0), queue.pop().map(|f| f.0)))
        };
        // Longer than several of the old 100 ms idle ticks.
        // A notification is not a frame. Condvars may also wake spuriously;
        // neither should count as a completed park in this regression check.
        for _ in 0..7 {
            std::thread::sleep(Duration::from_millis(50));
            queue.changed.notify_all();
        }
        std::thread::sleep(Duration::from_millis(10));
        assert_eq!(queue.wakeups.load(std::sync::atomic::Ordering::SeqCst), 0);

        assert!(push(&queue, 7, Duration::from_millis(10)));
        queue.close();
        assert_eq!(reader.join().expect("reader"), (Some(7), None));
    }

    #[test]
    fn closing_releases_both_ends() {
        let queue = FrameQueue::new(1);
        queue.close();
        assert!(!push(&queue, 0, Duration::from_secs(5)));
        assert!(queue.pop().is_none());
        assert!(queue.try_pop().is_none());
        assert!(queue.is_closed());
    }

    #[test]
    fn the_bound_is_bytes_so_kilobyte_reads_get_a_megabyte_of_slack() {
        let queue = FrameQueue::new(1 << 20);
        let chunk = [b'k'; 1024];
        // Sixteen frames used to fill it; a thousand kilobyte reads fit now,
        // without the pump ever waiting.
        for index in 0..1024_u64 {
            assert!(
                queue.push(index * 1024, &chunk, Duration::ZERO),
                "read {index}"
            );
        }
        assert!(
            !queue.push(1 << 20, &chunk, Duration::ZERO),
            "and no further"
        );
    }

    #[test]
    fn waiting_contiguous_bytes_join_one_frame_up_to_the_cap() {
        let queue = FrameQueue::new(4 << 20);
        let chunk = [b'j'; 1024];
        let reads = (JOINED_FRAME_BYTES / 1024) as u64 + 3;
        for index in 0..reads {
            assert!(queue.push(index * 1024, &chunk, Duration::ZERO));
        }
        let (first, bytes) = queue.try_pop().expect("joined frame");
        assert_eq!((first, bytes.len()), (0, JOINED_FRAME_BYTES));
        let (second, rest) = queue.try_pop().expect("the remainder");
        assert_eq!((second, rest.len()), (JOINED_FRAME_BYTES as u64, 3 * 1024));
        assert!(queue.try_pop().is_none());

        // A gap is never papered over: bytes that do not continue the waiting
        // frame start their own.
        assert!(queue.push(10, b"ab", Duration::ZERO));
        assert!(queue.push(20, b"cd", Duration::ZERO));
        assert_eq!(queue.try_pop(), Some((10, b"ab".to_vec())));
        assert_eq!(queue.try_pop(), Some((20, b"cd".to_vec())));
    }

    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn pause(&mut self) {
            match self.next() % 16 {
                0 => std::thread::sleep(Duration::from_micros(self.next() % 300)),
                1..=3 => std::thread::yield_now(),
                _ => {}
            }
        }
    }

    /// The pump, the output writer and the peer watcher on one queue, with a
    /// tiny byte bound, random delays, short patience and a random hangup.
    /// Every byte a push accepted must reach the reader exactly once, in
    /// order, in frames that start where the last ended — and no round may
    /// hang, whichever side gives up first.
    fn stress(first: u64, rounds: u64) {
        for round in first..first + rounds {
            let seed = 0xD1B5_4A32_D192_ED03 ^ round.wrapping_mul(0x9E37_79B9_7F4A_7C15);
            let mut rng = Rng(seed | 1);
            let queue = FrameQueue::new((rng.next() % 96 + 1) as usize);
            let patience = Duration::from_micros(rng.next() % 2_000);
            let reader = {
                let queue = Arc::clone(&queue);
                let mut rng = Rng(rng.next() | 1);
                std::thread::spawn(move || {
                    let mut frames = Vec::new();
                    loop {
                        let frame = if rng.next().is_multiple_of(2) {
                            queue.pop()
                        } else {
                            match queue.try_pop() {
                                Some(frame) => Some(frame),
                                None => queue.pop(),
                            }
                        };
                        let Some(frame) = frame else { break };
                        frames.push(frame);
                        rng.pause();
                    }
                    frames
                })
            };
            let hangup = rng.next().is_multiple_of(4).then(|| {
                let queue = Arc::clone(&queue);
                let after = Duration::from_micros(rng.next() % 3_000);
                std::thread::spawn(move || {
                    std::thread::sleep(after);
                    queue.close();
                })
            });
            let mut accepted = Vec::new();
            let mut offset = 0_u64;
            for index in 0..(rng.next() % 500 + 20) {
                let length = (rng.next() % 48 + 1) as usize;
                let chunk: Vec<u8> = (0..length).map(|at| (index as usize ^ at) as u8).collect();
                if !queue.push(offset, &chunk, patience) {
                    // As `broadcast_output` does with a subscriber it gives up on.
                    queue.close();
                    break;
                }
                accepted.extend_from_slice(&chunk);
                offset += length as u64;
                rng.pause();
            }
            queue.close();
            let (done, finished) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let _ = done.send(reader.join());
            });
            let frames = finished
                .recv_timeout(Duration::from_secs(10))
                .unwrap_or_else(|_| panic!("round {round} (seed {seed:#x}) hung"))
                .expect("reader");
            if let Some(hangup) = hangup {
                hangup.join().expect("hangup");
            }
            let mut next = 0_u64;
            let mut delivered = Vec::new();
            for (start, bytes) in frames {
                assert_eq!(start, next, "round {round} (seed {seed:#x}): a gap");
                assert!(!bytes.is_empty() && bytes.len() <= JOINED_FRAME_BYTES);
                next += bytes.len() as u64;
                delivered.extend_from_slice(&bytes);
            }
            assert!(
                delivered == accepted,
                "round {round} (seed {seed:#x}): accepted bytes lost or reordered"
            );
        }
    }

    #[test]
    fn pump_writer_and_hangup_never_lose_a_wakeup_or_an_accepted_byte() {
        stress(0, 100);
    }

    /// `DIRI_STRESS_ROUNDS` rounds (default 20,000) from `DIRI_STRESS_FIRST`.
    #[test]
    #[ignore = "long; run on demand"]
    fn pump_writer_and_hangup_never_lose_a_wakeup_or_an_accepted_byte_at_length() {
        let setting = |name: &str, default: u64| {
            std::env::var(name)
                .ok()
                .and_then(|value| value.parse().ok())
                .unwrap_or(default)
        };
        stress(
            setting("DIRI_STRESS_FIRST", 0),
            setting("DIRI_STRESS_ROUNDS", 20_000),
        );
    }
}
