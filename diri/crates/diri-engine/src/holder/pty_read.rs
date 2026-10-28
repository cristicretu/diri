//! How the Holder's pump reads a PTY that is streaming output.
//!
//! A macOS PTY master hands over about a kilobyte per read however large the
//! buffer: the writer sleeps once the kernel's output queue reaches its high
//! water mark, and each read wakes it for the next kilobyte. Waiting for those
//! kilobytes with `poll` costs more than the reads themselves: the readiness
//! path wakes the reader later than a read sleeping on the queue does, and
//! every kilobyte paid a `poll`, a read and a read returning `EAGAIN`. A
//! 100 MB `cat` drained in 0.73 s that way against 0.57 s with plain blocking
//! reads, on the same machine, with no other change.
//!
//! So while output streams the pump sleeps in `read` itself, and once it
//! pauses the pump goes back to waiting in `poll` on the PTY and the exit
//! path's wake pipe, as before: a silent session still sleeps with no
//! deadline and no wakeups.
//!
//! `O_NONBLOCK` lives on the master's shared file description, so streaming
//! reads clear it, and the writers, which must never wait on a child that
//! stopped reading its input, set it again around each write (see
//! [`ReadMode`]). The exit path, which cannot wake a sleeping `read` through
//! the pipe, interrupts it with a signal instead ([`interrupt`]).

use std::sync::Once;
use std::sync::atomic::{AtomicBool, Ordering};

/// A read at least this long found the kernel's output queue full or close to
/// it: the writer is producing faster than one wakeup per read.
pub(super) const STREAMING_READ_BYTES: usize = 512;

/// How many such reads in a row turn streaming reads on. A prompt or a short
/// redraw stays on the polling path and never touches the descriptor's flags.
pub(super) const STREAMING_READS_TO_ENTER: u32 = 4;

/// Whether the pump is reading in blocking mode, and so whether the master's
/// file description is blocking.
///
/// It is only changed with the PTY lock held, which is also what every writer
/// holds, so a writer always knows which flags to put back.
#[derive(Default)]
pub(super) struct ReadMode {
    blocking: AtomicBool,
}

impl ReadMode {
    /// Whether the descriptor is in blocking mode. The exit path reads this
    /// after setting `finished` and the pump writes it before reading
    /// `finished`, both sequentially consistent, so one of the two always
    /// sees the other.
    pub(super) fn is_blocking(&self) -> bool {
        self.blocking.load(Ordering::SeqCst)
    }

    /// Switches the description, with the PTY lock held (`_pty`). Returns
    /// whether the switch took.
    pub(super) fn set_blocking<T>(
        &self,
        _pty: &std::sync::MutexGuard<'_, T>,
        fd: i32,
        on: bool,
    ) -> bool {
        if self.is_blocking() == on {
            return true;
        }
        if !set_nonblocking_flag(fd, !on) {
            return false;
        }
        self.blocking.store(on, Ordering::SeqCst);
        true
    }

    /// Makes the description nonblocking for one write, with the PTY lock
    /// held (`_pty`), and blocking again when the returned guard drops.
    /// While the pump only polls, the description is already nonblocking
    /// and this costs nothing.
    pub(super) fn nonblocking_for_write<T>(
        &self,
        _pty: &std::sync::MutexGuard<'_, T>,
        fd: i32,
    ) -> WriteFlags {
        let restore = self.is_blocking() && set_nonblocking_flag(fd, true);
        WriteFlags { fd, restore }
    }
}

/// Puts a streaming reader's blocking flag back after a write.
pub(super) struct WriteFlags {
    fd: i32,
    restore: bool,
}

impl Drop for WriteFlags {
    fn drop(&mut self) {
        if self.restore {
            set_nonblocking_flag(self.fd, false);
        }
    }
}

fn set_nonblocking_flag(fd: i32, on: bool) -> bool {
    // SAFETY: fcntl on a descriptor the caller keeps open.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags < 0 {
            return false;
        }
        let wanted = if on {
            flags | libc::O_NONBLOCK
        } else {
            flags & !libc::O_NONBLOCK
        };
        wanted == flags || libc::fcntl(fd, libc::F_SETFL, wanted) == 0
    }
}

/// When a streak of full reads turns streaming reads on, and when a short one
/// turns them off.
#[derive(Default)]
pub(super) struct Streak {
    full_reads: u32,
}

impl Streak {
    /// Records one batch of `bytes` read and returns whether the next read
    /// should sleep in `read` (`true`) or in `poll` (`false`), given that
    /// it would currently be `blocking`.
    pub(super) fn after_read(&mut self, bytes: usize, blocking: bool) -> bool {
        if bytes < STREAMING_READ_BYTES {
            // The queue was not full, so the writer is not waiting on it:
            // output is pausing, and the next wait must also see the exit
            // wake.
            self.full_reads = 0;
            return false;
        }
        self.full_reads = self.full_reads.saturating_add(1);
        blocking || self.full_reads >= STREAMING_READS_TO_ENTER
    }
}

/// The signal that interrupts a pump sleeping in `read`. Its default action
/// is to be ignored, so one arriving anywhere else in the process, or before
/// the handler is installed, does nothing.
const INTERRUPT_SIGNAL: i32 = libc::SIGURG;

static INSTALL: Once = Once::new();

extern "C" fn on_interrupt(_: libc::c_int) {}

/// Installs a handler for [`INTERRUPT_SIGNAL`] without `SA_RESTART`, so a
/// `read` it interrupts returns `EINTR` instead of resuming. Children do not
/// inherit it: exec resets caught signals to their default.
pub(super) fn install_interrupt() {
    INSTALL.call_once(|| {
        // SAFETY: a zeroed sigaction is valid; the handler is async-signal
        // safe (it does nothing).
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = on_interrupt as *const () as usize;
            action.sa_flags = 0;
            libc::sigemptyset(&mut action.sa_mask);
            libc::sigaction(INTERRUPT_SIGNAL, &action, std::ptr::null_mut());
        }
    });
}

/// Interrupts whatever system call `thread` is sleeping in. A signal that
/// lands between calls is lost, so callers repeat until the thread is done.
pub(super) fn interrupt(thread: libc::pthread_t) {
    // SAFETY: the caller still holds the thread's JoinHandle, so the
    // pthread_t is valid even if the thread has returned.
    unsafe {
        libc::pthread_kill(thread, INTERRUPT_SIGNAL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_streak_of_full_reads_turns_blocking_on_and_a_short_read_turns_it_off() {
        let mut streak = Streak::default();
        let mut blocking = false;
        for _ in 1..STREAMING_READS_TO_ENTER {
            blocking = streak.after_read(1017, blocking);
            assert!(!blocking, "a few full reads stay on the polling path");
        }
        blocking = streak.after_read(1017, blocking);
        assert!(blocking);
        blocking = streak.after_read(4096, blocking);
        assert!(blocking);
        blocking = streak.after_read(37, blocking);
        assert!(!blocking, "a short read ends streaming at once");
        // and the count starts over
        blocking = streak.after_read(1017, blocking);
        assert!(!blocking);
    }

    /// The exit path's only way to reach a pump asleep in `read`.
    #[test]
    fn the_interrupt_ends_a_read_that_would_sleep_forever() {
        use std::io::Read;
        use std::os::unix::thread::JoinHandleExt;
        install_interrupt();
        let (mut reader, writer) = std::io::pipe().expect("pipe");
        let sleeper = std::thread::spawn(move || {
            let mut byte = [0u8; 1];
            reader.read(&mut byte).map_err(|error| error.kind())
        });
        let started = std::time::Instant::now();
        while !sleeper.is_finished() {
            assert!(started.elapsed() < std::time::Duration::from_secs(5));
            interrupt(sleeper.as_pthread_t());
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert_eq!(
            sleeper.join().expect("sleeper"),
            Err(std::io::ErrorKind::Interrupted)
        );
        drop(writer);
    }

    #[test]
    fn a_prompt_sized_burst_never_leaves_the_polling_path() {
        let mut streak = Streak::default();
        let mut blocking = false;
        for bytes in [1017, 1017, 900, 120, 1017, 300, 1017, 1017, 64] {
            blocking = streak.after_read(bytes, blocking);
            assert!(!blocking);
        }
    }
}
