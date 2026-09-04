//! Process-level resource hygiene for the daemon.
//!
//! The Engine multiplexes every session a user has: one PTY holder socket and
//! output log per local session, one long-lived `ssh` child (three pipes) per
//! remote session, a control connection per app, MCP bridge, and hook, plus
//! logs and checkpoints. macOS launches GUI apps with a 256-descriptor soft
//! limit and the daemon inherits it, so a few dozen sessions exhaust it while
//! the hard limit sits orders of magnitude higher. Once `EMFILE` hits,
//! attaches close before their first frame, resizes never reach the PTY, and
//! the app's terminal goes blank until something happens to free a
//! descriptor — and the moment `accept` itself fails the daemon used to exit
//! outright, taking every session's terminal with it.

use std::io;
use std::time::Duration;

/// Where the descriptor limit ended up after [`raise_fd_limit`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FdLimit {
    pub soft: u64,
    pub hard: u64,
}

impl FdLimit {
    /// The hard limit as a log-friendly word: Darwin's `RLIM_INFINITY` is
    /// `i64::MAX`, which reads as noise in a boot log.
    #[must_use]
    pub fn hard_label(&self) -> String {
        if self.hard >= i64::MAX as u64 {
            "unlimited".to_owned()
        } else {
            self.hard.to_string()
        }
    }
}

/// Raises the soft `RLIMIT_NOFILE` to the hard limit (or the highest value
/// the kernel accepts below it), never lowering it. Returns the resulting
/// limits, or `None` when the platform refused to report them.
#[cfg(unix)]
pub fn raise_fd_limit() -> Option<FdLimit> {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `limit` is a valid, writable rlimit and RLIMIT_NOFILE is a
    // documented resource.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } != 0 {
        return None;
    }
    // `rlim_t` is platform-sized (u64 here, narrower on some 32-bit libcs).
    #[allow(clippy::unnecessary_cast)]
    let current = FdLimit {
        soft: limit.rlim_cur as u64,
        hard: limit.rlim_max as u64,
    };
    let target = raise_target(current, platform_fd_ceiling());
    if target <= current.soft {
        return Some(current);
    }
    // macOS rejects a soft limit above `kern.maxfilesperproc` even when the
    // hard limit is unlimited, so step down from the target until the kernel
    // accepts a value; the last candidate is the OPEN_MAX every Darwin honors.
    for candidate in fallback_candidates(target, current.soft) {
        let request = libc::rlimit {
            rlim_cur: candidate as libc::rlim_t,
            rlim_max: limit.rlim_max,
        };
        // SAFETY: `request` is a fully initialised rlimit.
        if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &request) } == 0 {
            return Some(FdLimit {
                soft: candidate,
                hard: current.hard,
            });
        }
    }
    Some(current)
}

#[cfg(not(unix))]
pub fn raise_fd_limit() -> Option<FdLimit> {
    None
}

/// The soft limit to ask for: the hard limit, capped by the kernel's
/// per-process ceiling when one is advertised. Pure so the arithmetic is
/// testable without touching the real process limits.
fn raise_target(current: FdLimit, ceiling: Option<u64>) -> u64 {
    let mut target = current.hard;
    if let Some(ceiling) = ceiling {
        target = target.min(ceiling);
    }
    target.max(current.soft)
}

/// Values to try, highest first. Each is strictly above the soft limit we
/// already hold, so a successful call always improves matters.
fn fallback_candidates(target: u64, soft: u64) -> Vec<u64> {
    let mut candidates = vec![target, 65_536, 10_240, 4_096, 1_024];
    candidates.sort_unstable_by(|a, b| b.cmp(a));
    candidates.dedup();
    candidates.retain(|candidate| *candidate > soft && *candidate <= target);
    candidates
}

#[cfg(target_os = "macos")]
fn platform_fd_ceiling() -> Option<u64> {
    let mut value: libc::c_int = 0;
    let mut size = std::mem::size_of::<libc::c_int>();
    let name = c"kern.maxfilesperproc";
    // SAFETY: the buffer and its size describe a valid c_int; sysctlbyname
    // writes at most `size` bytes.
    let status = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            (&raw mut value).cast(),
            &raw mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    (status == 0 && value > 0).then_some(value as u64)
}

#[cfg(not(target_os = "macos"))]
fn platform_fd_ceiling() -> Option<u64> {
    None
}

/// How long the accept loop should pause after `accept` fails before trying
/// again. Descriptor exhaustion and aborted handshakes are transient: the
/// right response is to wait for a descriptor to free up, not to stop
/// serving. Anything else still gets retried — a daemon that exits strands
/// every attached terminal — just at a pace that keeps the log readable.
pub fn accept_retry_delay(error: &io::Error) -> Duration {
    match error.raw_os_error() {
        Some(libc::EMFILE | libc::ENFILE | libc::ENOBUFS | libc::ENOMEM) => {
            Duration::from_millis(100)
        }
        Some(libc::ECONNABORTED | libc::EAGAIN) => Duration::from_millis(10),
        _ => Duration::from_secs(1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_target_is_the_hard_limit_capped_by_the_kernel_ceiling() {
        let current = FdLimit {
            soft: 256,
            hard: u64::MAX,
        };
        assert_eq!(raise_target(current, Some(138_240)), 138_240);
        assert_eq!(
            raise_target(
                FdLimit {
                    soft: 1_024,
                    hard: 524_288
                },
                None
            ),
            524_288
        );
    }

    #[test]
    fn the_target_never_drops_below_the_soft_limit_already_held() {
        let generous = FdLimit {
            soft: 1_048_576,
            hard: u64::MAX,
        };
        assert_eq!(raise_target(generous, Some(138_240)), 1_048_576);
    }

    #[test]
    fn fallbacks_only_ever_improve_on_the_current_soft_limit() {
        let candidates = fallback_candidates(138_240, 256);
        assert_eq!(candidates, vec![138_240, 65_536, 10_240, 4_096, 1_024]);
        assert!(fallback_candidates(10_240, 10_240).is_empty());
        assert_eq!(fallback_candidates(2_048, 1_024), vec![2_048]);
    }

    #[cfg(unix)]
    #[test]
    fn raising_the_limit_is_monotonic_and_stays_within_the_hard_limit() {
        let before = raise_fd_limit().expect("rlimit readable");
        let after = raise_fd_limit().expect("rlimit readable");
        assert!(after.soft >= before.soft);
        assert!(after.soft <= after.hard);
        assert_eq!(after.hard, before.hard);
    }

    #[test]
    fn descriptor_exhaustion_pauses_briefly_instead_of_ending_the_daemon() {
        let emfile = io::Error::from_raw_os_error(libc::EMFILE);
        assert_eq!(accept_retry_delay(&emfile), Duration::from_millis(100));
        let aborted = io::Error::from_raw_os_error(libc::ECONNABORTED);
        assert_eq!(accept_retry_delay(&aborted), Duration::from_millis(10));
        let unknown = io::Error::other("listener vanished");
        assert_eq!(accept_retry_delay(&unknown), Duration::from_secs(1));
    }
}
