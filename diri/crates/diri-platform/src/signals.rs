//! Logical POSIX requests. Windows translates supported requests into Job/PTY
//! operations; these numbers must never be passed to Win32 as exit reasons.
#[cfg(unix)]
pub use libc::{
    SIGCHLD, SIGCONT, SIGHUP, SIGINT, SIGKILL, SIGQUIT, SIGSTOP, SIGTERM, SIGUSR1, SIGUSR2,
};
#[cfg(windows)]
pub const SIGHUP: i32 = 1;
#[cfg(windows)]
pub const SIGINT: i32 = 2;
#[cfg(windows)]
pub const SIGQUIT: i32 = 3;
#[cfg(windows)]
pub const SIGKILL: i32 = 9;
#[cfg(windows)]
pub const SIGTERM: i32 = 15;
#[cfg(windows)]
pub const SIGSTOP: i32 = 19;
#[cfg(windows)]
pub const SIGCONT: i32 = 18;
#[cfg(windows)]
pub const SIGUSR1: i32 = 10;
#[cfg(windows)]
pub const SIGUSR2: i32 = 12;
#[cfg(windows)]
pub const SIGCHLD: i32 = 17;
