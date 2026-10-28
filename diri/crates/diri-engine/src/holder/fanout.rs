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
    /// Bumped each time the subscriber takes bytes that never passed through
    /// `frames`: the log backlog of a resumed subscription.
    progress: u64,
    /// No writer thread is serving the queue: none was started yet, or the
    /// last one went idle and ended (see [`FrameQueue::park_writer`]). The
    /// next push starts one; a close while it is set leaves the subscriber's
    /// teardown to whoever closed.
    writer_parked: bool,
    /// The writer is in [`FrameQueue::linger`]: it wakes at its own
    /// deadline, so a push need not wake it until a batch has built.
    lingering: bool,
    /// Buffers of frames the writer has sent, handed back (see
    /// [`FrameQueue::recycle`]) so a streaming session's next frame starts
    /// with the room it will grow into instead of reallocating its way up
    /// from one PTY read. At most [`SPARE_FRAMES`], and emptied whenever the
    /// writer parks, so a quiet session keeps none.
    spare: Vec<Vec<u8>>,
}

/// How many sent frames' buffers a queue keeps for reuse.
const SPARE_FRAMES: usize = 2;

/// What became of a frame offered to a subscriber.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Pushed {
    /// Queued for the running writer.
    Queued,
    /// Queued, and no writer is running: the caller must start one.
    WakeWriter,
    /// The subscriber is gone; nothing was queued.
    Closed,
    /// The queue stayed full while the subscriber took nothing for the whole
    /// patience; nothing was queued, so the same bytes can be offered again.
    Stalled,
}

/// What a writer waiting with a deadline got.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Popped {
    Frame(Frame),
    Closed,
    /// Nothing arrived in time.
    Idle,
}

/// Bytes that wake a lingering reader before its deadline. While output
/// streams, a pump read is a kilobyte a few microseconds apart, and waking
/// the reader for each one cost a condvar signal and a context switch per
/// kilobyte: more than the PTY read itself. A lingering reader wakes at its
/// deadline anyway (100 µs) and finds them all joined in one frame.
const LINGER_WAKE_BYTES: usize = 64 << 10;

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
                progress: 0,
                writer_parked: true,
                lingering: false,
                spare: Vec::new(),
            }),
            changed: Condvar::new(),
            capacity: capacity.max(1),
            #[cfg(test)]
            wakeups: std::sync::atomic::AtomicUsize::new(0),
        })
    }

    /// Queues the bytes starting at `offset`, waiting for room for as long as
    /// the subscriber keeps taking bytes, and up to `patience` once it stops.
    ///
    /// A subscriber reading its log backlog takes nothing from the queue but
    /// is still consuming the stream, so it reports that with
    /// [`Self::note_progress`], and the wait goes on.
    ///
    /// Returns false if the subscriber is gone or has taken nothing for
    /// `patience` — in both cases the caller drops it, and it resumes from the
    /// log, which has every byte.
    #[cfg(any(windows, test))]
    pub(super) fn push(&self, offset: u64, bytes: &[u8], patience: Duration) -> bool {
        matches!(
            self.offer(offset, bytes, patience),
            Pushed::Queued | Pushed::WakeWriter
        )
    }

    /// [`Self::push`], also saying whether a writer has to be started for the
    /// frame (the queue's writer ends when it has been idle, and the push that
    /// finds none is the one that starts the next), and telling apart a
    /// subscriber that is gone from one that has merely taken nothing for
    /// `patience`.
    pub(super) fn offer(&self, offset: u64, bytes: &[u8], patience: Duration) -> Pushed {
        let mut state = self.state.lock().expect("frame queue");
        if !state.closed && state.queued >= self.capacity {
            let mut deadline = Instant::now() + patience;
            let mut seen = state.progress;
            while !state.closed && state.queued >= self.capacity {
                let mut remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() && state.progress != seen {
                    seen = state.progress;
                    deadline = Instant::now() + patience;
                    remaining = patience;
                }
                if remaining.is_zero() {
                    return Pushed::Stalled;
                }
                let (guard, _) = self
                    .changed
                    .wait_timeout(state, remaining)
                    .expect("frame queue");
                state = guard;
            }
        }
        if state.closed {
            return Pushed::Closed;
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
            _ => {
                let frame = match state.spare.pop() {
                    Some(mut frame) => {
                        frame.extend_from_slice(bytes);
                        frame
                    }
                    None => bytes.to_vec(),
                };
                state.frames.push_back((offset, frame));
            }
        }
        let unserved = std::mem::take(&mut state.writer_parked);
        // A reader parks only on an empty queue, so a push onto a non-empty
        // one has nobody to wake, and a lingering one is woken only once a
        // batch has built.
        let wake = if state.lingering {
            state.queued >= LINGER_WAKE_BYTES && state.queued - bytes.len() < LINGER_WAKE_BYTES
        } else {
            was_empty
        };
        drop(state);
        if wake {
            self.changed.notify_all();
        }
        if unserved {
            Pushed::WakeWriter
        } else {
            Pushed::Queued
        }
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

    /// Takes the next frame, waiting at most `idle` for one to arrive.
    pub(super) fn pop_within(&self, idle: Duration) -> Popped {
        let mut state = self.state.lock().expect("frame queue");
        let deadline = Instant::now() + idle;
        loop {
            if let Some(frame) = Self::take_front(&mut state) {
                return Popped::Frame(self.released(state, frame));
            }
            if state.closed {
                return Popped::Closed;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Popped::Idle;
            }
            let (guard, timeout) = self
                .changed
                .wait_timeout_while(state, remaining, |state| {
                    state.frames.is_empty() && !state.closed
                })
                .expect("frame queue");
            state = guard;
            #[cfg(test)]
            if !timeout.timed_out() {
                self.wakeups
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
            #[cfg(not(test))]
            let _ = timeout;
        }
    }

    /// Takes what arrives within `timeout`, joined into one frame where it
    /// is contiguous. Pushes do not cut the wait short until
    /// [`LINGER_WAKE_BYTES`] are waiting, so a stream of small frames costs
    /// one wakeup per `timeout` rather than one per frame; only the queue
    /// closing or filling ends it sooner.
    #[cfg(unix)]
    pub(super) fn linger(&self, timeout: Duration) -> Option<Frame> {
        let mut state = self.state.lock().expect("frame queue");
        if state.frames.is_empty() && !state.closed {
            state.lingering = true;
            state = self
                .changed
                .wait_timeout_while(state, timeout, |state| {
                    !state.closed && state.queued < LINGER_WAKE_BYTES
                })
                .expect("frame queue")
                .0;
            state.lingering = false;
            #[cfg(test)]
            self.wakeups
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        let frame = Self::take_front(&mut state)?;
        Some(self.released(state, frame))
    }

    /// Ends the writer's turn if nothing is waiting: returns true, and the
    /// next push asks for a new writer. Returns false if a frame or a close
    /// arrived meanwhile, so the writer must carry on.
    pub(super) fn park_writer(&self) -> bool {
        let mut state = self.state.lock().expect("frame queue");
        if state.frames.is_empty() && !state.closed {
            state.writer_parked = true;
            state.spare = Vec::new();
            true
        } else {
            false
        }
    }

    /// Hands back the buffer of a frame the writer has sent, for the next
    /// frame to reuse. Kept only while a writer serves the queue and only up
    /// to [`SPARE_FRAMES`] (and never one larger than a joined frame grows);
    /// any other is freed.
    #[cfg(unix)]
    pub(super) fn recycle(&self, mut frame: Vec<u8>) {
        if frame.capacity() > JOINED_FRAME_BYTES {
            return;
        }
        frame.clear();
        let mut state = self.state.lock().expect("frame queue");
        if !state.closed && !state.writer_parked && state.spare.len() < SPARE_FRAMES {
            state.spare.push(frame);
        }
    }

    /// Records that a writer was started for this queue other than by a push,
    /// so the next push does not ask for another.
    pub(super) fn claim_writer(&self) {
        self.state.lock().expect("frame queue").writer_parked = false;
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

    /// Records that the subscriber took bytes from outside the queue, which
    /// keeps a pump waiting for room from giving up on it.
    #[cfg(any(unix, test))]
    pub(super) fn note_progress(&self) {
        self.state.lock().expect("frame queue").progress += 1;
    }

    /// Marks the subscriber gone and wakes anyone waiting on it.
    ///
    /// Returns true when no writer was running, which makes the caller the
    /// one to release the subscriber's socket. A running writer sees the
    /// close and releases it itself, so exactly one party ever does.
    pub(super) fn close(&self) -> bool {
        let mut state = self.state.lock().expect("frame queue");
        state.closed = true;
        state.spare = Vec::new();
        let unserved = std::mem::take(&mut state.writer_parked);
        drop(state);
        self.changed.notify_all();
        unserved
    }

    #[cfg(any(unix, test))]
    pub(super) fn is_closed(&self) -> bool {
        self.state.lock().expect("frame queue").closed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

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

    /// A sent frame's buffer starts the next frame, with its room kept, and
    /// a writer that parks leaves none behind.
    #[test]
    fn a_sent_frames_buffer_is_reused_until_the_writer_parks() {
        let queue = FrameQueue::new(1 << 20);
        assert_eq!(queue.offer(0, b"abc", Duration::ZERO), Pushed::WakeWriter);
        let (_, mut sent) = queue.try_pop().expect("frame");
        sent.reserve(64 << 10);
        let room = sent.capacity();
        queue.recycle(sent);
        assert_eq!(queue.offer(3, b"de", Duration::ZERO), Pushed::Queued);
        let (offset, frame) = queue.try_pop().expect("frame");
        assert_eq!((offset, &frame[..]), (3, &b"de"[..]));
        assert_eq!(frame.capacity(), room, "the sent frame's buffer was reused");
        queue.recycle(frame);
        assert!(queue.park_writer());
        assert!(queue.state.lock().unwrap().spare.is_empty());
        // Parked, nothing is kept.
        queue.recycle(Vec::with_capacity(4096));
        assert!(queue.state.lock().unwrap().spare.is_empty());
    }

    #[test]
    fn a_stalled_offer_queues_nothing_and_can_be_offered_again() {
        let queue = FrameQueue::new(1);
        // The first frame asks for a writer; this test plays the writer.
        assert_eq!(queue.offer(0, b"a", Duration::ZERO), Pushed::WakeWriter);
        assert_eq!(
            queue.offer(1, b"b", Duration::from_millis(10)),
            Pushed::Stalled
        );
        assert_eq!(queue.try_pop(), Some((0, b"a".to_vec())));
        assert!(
            queue.try_pop().is_none(),
            "the stalled bytes were not queued"
        );
        assert_eq!(queue.offer(1, b"b", Duration::ZERO), Pushed::Queued);
        assert_eq!(queue.try_pop(), Some((1, b"b".to_vec())));
        queue.close();
        assert_eq!(queue.offer(2, b"c", Duration::ZERO), Pushed::Closed);
    }

    #[test]
    fn a_subscriber_reading_its_backlog_is_waited_for_until_it_stops() {
        let queue = FrameQueue::new(1);
        assert!(push(&queue, 0, Duration::from_millis(10)));
        // Taking backlog bytes for four patiences, then going quiet. The
        // queue itself is never drained, so only progress keeps the pump
        // waiting.
        let reader = {
            let queue = Arc::clone(&queue);
            std::thread::spawn(move || {
                for _ in 0..16 {
                    std::thread::sleep(Duration::from_millis(5));
                    queue.note_progress();
                }
            })
        };
        let started = Instant::now();
        assert!(!push(&queue, 1, Duration::from_millis(20)));
        let waited = started.elapsed();
        reader.join().expect("reader");
        assert!(
            waited >= Duration::from_millis(80),
            "gave up after {waited:?} while the subscriber was still reading"
        );
        assert!(waited < Duration::from_secs(2), "waited {waited:?}");
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

    /// A writer lingering for the next frame of a stream is not woken by
    /// each kilobyte the pump pushes: it collects them all, joined, at its
    /// deadline, and pays one wakeup instead of one per read.
    #[cfg(unix)]
    #[test]
    fn a_lingering_reader_collects_a_stream_of_small_frames_at_its_deadline() {
        let queue = FrameQueue::new(1 << 20);
        let lingering = {
            let queue = Arc::clone(&queue);
            std::thread::spawn(move || {
                let started = Instant::now();
                (queue.linger(Duration::from_millis(300)), started.elapsed())
            })
        };
        while !queue.state.lock().expect("frame queue").lingering {
            std::thread::yield_now();
        }
        for offset in 0..32 {
            assert!(queue.push(offset * 1024, &[b'x'; 1024], Duration::from_secs(1)));
        }
        let (frame, waited) = lingering.join().expect("reader");
        let (offset, bytes) = frame.expect("a frame");
        assert_eq!(offset, 0);
        assert_eq!(bytes.len(), 32 * 1024, "every kilobyte, in one frame");
        assert!(
            waited >= Duration::from_millis(250),
            "woken early: {waited:?}"
        );
    }

    /// A batch big enough to be worth writing, a close, or nothing at all:
    /// a lingering reader is woken at once for the first two.
    #[cfg(unix)]
    #[test]
    fn a_lingering_reader_wakes_early_for_a_full_batch_or_a_close() {
        let queue = FrameQueue::new(1 << 20);
        let reader = {
            let queue = Arc::clone(&queue);
            std::thread::spawn(move || {
                let started = Instant::now();
                let frame = queue.linger(Duration::from_secs(10));
                (frame.map(|frame| frame.1.len()), started.elapsed())
            })
        };
        while !queue.state.lock().expect("frame queue").lingering {
            std::thread::yield_now();
        }
        assert!(queue.push(0, &vec![b'x'; LINGER_WAKE_BYTES], Duration::from_secs(1)));
        let (bytes, waited) = reader.join().expect("reader");
        assert_eq!(bytes, Some(LINGER_WAKE_BYTES));
        assert!(waited < Duration::from_secs(5), "waited {waited:?}");

        let reader = {
            let queue = Arc::clone(&queue);
            std::thread::spawn(move || {
                let started = Instant::now();
                (queue.linger(Duration::from_secs(10)), started.elapsed())
            })
        };
        while !queue.state.lock().expect("frame queue").lingering {
            std::thread::yield_now();
        }
        queue.close();
        let (frame, waited) = reader.join().expect("reader");
        assert!(frame.is_none());
        assert!(waited < Duration::from_secs(5), "waited {waited:?}");
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
    fn a_writer_ends_when_idle_and_the_next_push_asks_for_another() {
        let queue = FrameQueue::new(1 << 20);
        assert_eq!(queue.offer(0, b"a", Duration::ZERO), Pushed::WakeWriter);
        assert_eq!(queue.offer(1, b"b", Duration::ZERO), Pushed::Queued);
        assert_eq!(
            queue.pop_within(Duration::from_millis(10)),
            Popped::Frame((0, b"ab".to_vec()))
        );
        assert_eq!(queue.pop_within(Duration::from_millis(10)), Popped::Idle);
        assert!(queue.park_writer(), "nothing waiting: the writer may end");
        assert_eq!(queue.offer(2, b"c", Duration::ZERO), Pushed::WakeWriter);
        assert!(!queue.park_writer(), "a frame is waiting");
        assert!(!queue.close(), "a running writer releases the subscriber");
        assert_eq!(
            queue.pop_within(Duration::ZERO),
            Popped::Frame((2, b"c".to_vec()))
        );
        assert_eq!(queue.pop_within(Duration::ZERO), Popped::Closed);

        let parked = FrameQueue::new(1);
        assert!(parked.close(), "with no writer the closer releases it");
        assert!(!parked.close(), "and only the first closer");
    }

    /// The pump, writers that end whenever they are idle (restarted by the
    /// push that finds none), and a random hangup. Every accepted byte must
    /// arrive exactly once and in order, no round may hang, and exactly one
    /// party may release the subscriber.
    fn parking_stress(first: u64, rounds: u64) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        for round in first..first + rounds {
            let seed = 0x5EED_0F5A_124B_1D07 ^ round.wrapping_mul(0x9E37_79B9_7F4A_7C15);
            let mut rng = Rng(seed | 1);
            let queue = FrameQueue::new((rng.next() % 96 + 1) as usize);
            let patience = Duration::from_micros(rng.next() % 2_000);
            let idle = Duration::from_micros(rng.next() % 400);
            let delivered = Arc::new(Mutex::new(Vec::new()));
            let releases = Arc::new(AtomicUsize::new(0));
            let writers = Arc::new(Mutex::new(Vec::new()));
            let start_writer = {
                let queue = Arc::clone(&queue);
                let delivered = Arc::clone(&delivered);
                let releases = Arc::clone(&releases);
                let writers = Arc::clone(&writers);
                move |seed: u64| {
                    let queue = Arc::clone(&queue);
                    let delivered = Arc::clone(&delivered);
                    let releases = Arc::clone(&releases);
                    let mut rng = Rng(seed | 1);
                    writers
                        .lock()
                        .expect("writers")
                        .push(std::thread::spawn(move || {
                            loop {
                                match queue.pop_within(idle) {
                                    Popped::Frame(frame) => {
                                        delivered.lock().expect("delivered").push(frame);
                                    }
                                    Popped::Closed => {
                                        releases.fetch_add(1, Ordering::SeqCst);
                                        return;
                                    }
                                    Popped::Idle => {
                                        if queue.park_writer() {
                                            return;
                                        }
                                    }
                                }
                                rng.pause();
                            }
                        }));
                }
            };
            let hangup = rng.next().is_multiple_of(4).then(|| {
                let queue = Arc::clone(&queue);
                let releases = Arc::clone(&releases);
                let after = Duration::from_micros(rng.next() % 3_000);
                std::thread::spawn(move || {
                    std::thread::sleep(after);
                    if queue.close() {
                        releases.fetch_add(1, Ordering::SeqCst);
                    }
                })
            });
            let mut accepted = Vec::new();
            let mut offset = 0_u64;
            for index in 0..(rng.next() % 300 + 20) {
                let length = (rng.next() % 48 + 1) as usize;
                let chunk: Vec<u8> = (0..length).map(|at| (index as usize ^ at) as u8).collect();
                match queue.offer(offset, &chunk, patience) {
                    Pushed::Queued => {}
                    Pushed::WakeWriter => start_writer(rng.next()),
                    Pushed::Closed | Pushed::Stalled => break,
                }
                accepted.extend_from_slice(&chunk);
                offset += length as u64;
                if rng.next().is_multiple_of(8) {
                    // Long enough, sometimes, for the writer to end idle.
                    std::thread::sleep(Duration::from_micros(rng.next() % 600));
                }
            }
            if queue.close() {
                releases.fetch_add(1, Ordering::SeqCst);
            }
            if let Some(hangup) = hangup {
                hangup.join().expect("hangup");
            }
            let (done, finished) = std::sync::mpsc::channel();
            {
                let writers = Arc::clone(&writers);
                std::thread::spawn(move || {
                    let handles = std::mem::take(&mut *writers.lock().expect("writers"));
                    for handle in handles {
                        handle.join().expect("writer");
                    }
                    let _ = done.send(());
                });
            }
            finished
                .recv_timeout(Duration::from_secs(10))
                .unwrap_or_else(|_| panic!("round {round} (seed {seed:#x}) hung"));
            assert_eq!(
                releases.load(Ordering::SeqCst),
                1,
                "round {round} (seed {seed:#x}): released other than once"
            );
            let mut next = 0_u64;
            let mut bytes = Vec::new();
            for (start, frame) in delivered.lock().expect("delivered").iter() {
                assert_eq!(*start, next, "round {round} (seed {seed:#x}): a gap");
                next += frame.len() as u64;
                bytes.extend_from_slice(frame);
            }
            assert!(
                bytes == accepted,
                "round {round} (seed {seed:#x}): accepted bytes lost or reordered"
            );
        }
    }

    #[test]
    fn idle_writers_and_hangups_never_lose_a_byte_or_release_twice() {
        parking_stress(0, 100);
    }

    /// `DIRI_STRESS_ROUNDS` rounds (default 20,000) from `DIRI_STRESS_FIRST`.
    #[test]
    #[ignore = "long; run on demand"]
    fn idle_writers_and_hangups_never_lose_a_byte_or_release_twice_at_length() {
        let setting = |name: &str, default: u64| {
            std::env::var(name)
                .ok()
                .and_then(|value| value.parse().ok())
                .unwrap_or(default)
        };
        parking_stress(
            setting("DIRI_STRESS_FIRST", 0),
            setting("DIRI_STRESS_ROUNDS", 20_000),
        );
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
