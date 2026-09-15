//! Ordered, bounded persistence of immutable PTY session metadata.
//!
//! At most one write is active and one latest snapshot is pending. The worker
//! never owns a terminal, PTY, connection or lifecycle policy. It sleeps on a
//! condition variable when idle; disk I/O never holds the submission lock.
use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};

type Failure = (io::ErrorKind, String);
struct State<T> {
    pending: Option<(u64, T)>,
    submitted: u64,
    completed: u64,
    closed: bool,
    error: Option<Failure>,
}
struct Shared<T> {
    state: Mutex<State<T>>,
    changed: Condvar,
}

pub struct CheckpointWriter<T> {
    shared: Arc<Shared<T>>,
    worker: Option<JoinHandle<()>>,
    failure: UnixStream,
}

impl<T: Send + 'static> CheckpointWriter<T> {
    pub fn new(
        name: &str,
        mut persist: impl FnMut(T) -> io::Result<()> + Send + 'static,
    ) -> io::Result<Self> {
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                pending: None,
                submitted: 0,
                completed: 0,
                closed: false,
                error: None,
            }),
            changed: Condvar::new(),
        });
        let (failure, notifier) = UnixStream::pair()?;
        let owner = Arc::clone(&shared);
        let worker = thread::Builder::new()
            .name(name.into())
            .stack_size(128 * 1024)
            .spawn(move || {
                // Closure of this endpoint wakes a poll owner on failure, including
                // an unwinding persist callback. Success has no wakeup syscall.
                let _notifier = notifier;
                loop {
                    let mut state = owner.state.lock().expect("checkpoint state");
                    while state.pending.is_none() && !state.closed {
                        state = owner.changed.wait(state).expect("checkpoint state");
                    }
                    let Some((sequence, value)) = state.pending.take() else {
                        break;
                    };
                    drop(state);
                    let result =
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| persist(value)))
                            .unwrap_or_else(|_| {
                                Err(io::Error::other("checkpoint worker panicked"))
                            });
                    let mut state = owner.state.lock().expect("checkpoint state");
                    match result {
                        Ok(()) => state.completed = sequence,
                        Err(error) => {
                            state.error = Some((error.kind(), error.to_string()));
                            state.pending = None;
                        }
                    }
                    owner.changed.notify_all();
                    if state.error.is_some() {
                        break;
                    }
                }
            })?;
        Ok(Self {
            shared,
            worker: Some(worker),
            failure,
        })
    }

    /// Replace obsolete pending metadata. Never waits for the disk.
    pub fn submit(&self, value: T) -> io::Result<()> {
        let mut state = self.shared.state.lock().expect("checkpoint state");
        check_error(&state)?;
        if state.closed {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "checkpoint writer is closed",
            ));
        }
        state.submitted += 1;
        state.pending = Some((state.submitted, value));
        self.shared.changed.notify_all();
        Ok(())
    }

    /// Wait for everything accepted before this call. Use only at a durability
    /// boundary after the interactive owner has stopped producing metadata.
    pub fn flush(&self) -> io::Result<()> {
        let state = self.shared.state.lock().expect("checkpoint state");
        let target = state.submitted;
        let state = self
            .shared
            .changed
            .wait_while(state, |state| {
                state.completed < target && state.error.is_none()
            })
            .expect("checkpoint state");
        check_error(&state)
    }

    /// Permanently fence submissions, then durably finish the latest snapshot.
    /// This prevents a retained old client from rewriting a replacement binding.
    pub fn finish(&self) -> io::Result<()> {
        self.shared.state.lock().expect("checkpoint state").closed = true;
        self.shared.changed.notify_all();
        self.flush()
    }

    pub fn check_error(&self) -> io::Result<()> {
        check_error(&self.shared.state.lock().expect("checkpoint state"))
    }

    /// Poll for HUP to observe a persistence failure without idle polling.
    #[must_use]
    pub fn failure_fd(&self) -> RawFd {
        self.failure.as_raw_fd()
    }
}

fn check_error<T>(state: &State<T>) -> io::Result<()> {
    match &state.error {
        Some((kind, message)) => Err(io::Error::new(*kind, message.clone())),
        None => Ok(()),
    }
}

impl<T> Drop for CheckpointWriter<T> {
    fn drop(&mut self) {
        self.shared.state.lock().expect("checkpoint state").closed = true;
        self.shared.changed.notify_all();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    #[test]
    fn slow_checkpoint_does_not_stall_the_session_owner() {
        let saved = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&saved);
        let writer = CheckpointWriter::new("checkpoint-test", move |value| {
            std::thread::sleep(Duration::from_millis(200));
            sink.lock().unwrap().push(value);
            Ok(())
        })
        .unwrap();
        let started = Instant::now();
        writer.submit(1).unwrap();
        let elapsed = started.elapsed();
        writer.flush().unwrap();
        assert_eq!(*saved.lock().unwrap(), vec![1]);
        eprintln!("checkpoint submission: {elapsed:?}");
        assert!(
            elapsed < Duration::from_millis(40),
            "persistence stalled the session owner"
        );
    }
    #[test]
    fn slow_disk_coalesces_pending_state_and_finish_fences_old_writers() {
        let (entered_tx, entered) = std::sync::mpsc::channel();
        let (release, release_rx) = std::sync::mpsc::channel();
        let saved = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&saved);
        let writer = CheckpointWriter::new("ordered-checkpoints", move |value| {
            if value == 1 {
                entered_tx.send(()).unwrap();
                release_rx
                    .recv_timeout(Duration::from_secs(5))
                    .map_err(io::Error::other)?;
            }
            sink.lock().unwrap().push(value);
            Ok(())
        })
        .unwrap();
        writer.submit(1).unwrap();
        entered.recv_timeout(Duration::from_secs(2)).unwrap();
        for offset in 2..=1000 {
            writer.submit(offset).unwrap();
        }
        writer.submit(1001).unwrap(); // final exit snapshot
        release.send(()).unwrap();
        writer.finish().unwrap();
        assert_eq!(*saved.lock().unwrap(), [1, 1001]);
        assert_eq!(
            writer.submit(1002).unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
    }

    #[test]
    fn disk_failure_wakes_the_owner_and_fails_durability_fences() {
        let writer = CheckpointWriter::new("failed-checkpoint", |_: u64| {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "fixture disk failure",
            ))
        })
        .unwrap();
        writer.submit(1).unwrap();
        let mut fd = libc::pollfd {
            fd: writer.failure_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: the descriptor is owned by writer throughout the poll.
        assert_eq!(unsafe { libc::poll(&mut fd, 1, 2000) }, 1);
        assert_eq!(
            writer.flush().unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert!(writer.submit(2).is_err());
    }
}
