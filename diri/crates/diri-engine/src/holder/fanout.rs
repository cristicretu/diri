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

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// One frame: where it starts in the session's output stream, and its bytes.
pub(super) type Frame = (u64, Arc<[u8]>);

struct State {
    frames: VecDeque<Frame>,
    /// Set when the subscriber is gone. Both ends check it so neither waits on
    /// the other after the socket has failed.
    closed: bool,
}

pub(super) struct FrameQueue {
    state: Mutex<State>,
    /// Signals both directions: room appeared, or a frame did.
    changed: Condvar,
    capacity: usize,
}

impl FrameQueue {
    pub(super) fn new(capacity: usize) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State {
                frames: VecDeque::with_capacity(capacity),
                closed: false,
            }),
            changed: Condvar::new(),
            capacity,
        })
    }

    /// Queues a frame, waiting up to `patience` for room.
    ///
    /// Returns false if the subscriber is gone or is still full when patience
    /// runs out — in both cases the caller drops it, and it resumes from the
    /// log, which has every byte.
    pub(super) fn push(&self, frame: Frame, patience: Duration) -> bool {
        let deadline = Instant::now() + patience;
        let mut state = self.state.lock().expect("frame queue");
        while !state.closed && state.frames.len() >= self.capacity {
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
        if state.closed {
            return false;
        }
        state.frames.push_back(frame);
        drop(state);
        self.changed.notify_all();
        true
    }

    /// Takes the next frame, waiting up to `patience` for one to arrive.
    ///
    /// `None` means the queue closed, or nothing came in time; the caller can
    /// tell the difference with [`FrameQueue::is_closed`].
    pub(super) fn pop(&self, patience: Duration) -> Option<Frame> {
        let deadline = Instant::now() + patience;
        let mut state = self.state.lock().expect("frame queue");
        loop {
            if let Some(frame) = state.frames.pop_front() {
                drop(state);
                self.changed.notify_all();
                return Some(frame);
            }
            if state.closed {
                return None;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return None;
            }
            let (guard, _) = self
                .changed
                .wait_timeout(state, remaining)
                .expect("frame queue");
            state = guard;
        }
    }

    /// Marks the subscriber gone and wakes anyone waiting on it.
    pub(super) fn close(&self) {
        self.state.lock().expect("frame queue").closed = true;
        self.changed.notify_all();
    }

    pub(super) fn is_closed(&self) -> bool {
        self.state.lock().expect("frame queue").closed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(offset: u64) -> Frame {
        (offset, Arc::from(&b"x"[..]))
    }

    #[test]
    fn a_full_queue_makes_the_writer_wait_only_until_there_is_room() {
        let queue = FrameQueue::new(1);
        assert!(queue.push(frame(0), Duration::from_millis(10)));

        let drainer = {
            let queue = Arc::clone(&queue);
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(20));
                queue.pop(Duration::from_secs(1))
            })
        };
        // Room appears only once the drainer runs, so this push must wait for
        // it rather than give up early or block forever.
        assert!(queue.push(frame(1), Duration::from_secs(1)));
        assert_eq!(drainer.join().expect("drainer").map(|f| f.0), Some(0));
    }

    #[test]
    fn a_subscriber_that_never_drains_is_given_up_on() {
        let queue = FrameQueue::new(1);
        assert!(queue.push(frame(0), Duration::from_millis(10)));
        let started = Instant::now();
        assert!(!queue.push(frame(1), Duration::from_millis(20)));
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "push should give up at its deadline, not hang"
        );
    }

    #[test]
    fn closing_releases_both_ends() {
        let queue = FrameQueue::new(1);
        queue.close();
        assert!(!queue.push(frame(0), Duration::from_secs(5)));
        assert!(queue.pop(Duration::from_secs(5)).is_none());
        assert!(queue.is_closed());
    }
}
