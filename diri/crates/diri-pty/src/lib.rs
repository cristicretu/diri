//! Cross-process PTY ownership shared by Diri's local and remote Holders.
//!
//! The child receives an exact argv/environment/cwd tuple. The parent process
//! environment is never inherited implicitly.

use std::path::PathBuf;

/// What to launch on a PTY.
#[derive(Clone, Debug)]
pub struct PtySpec {
    /// argv[0] is the executable path.
    pub argv: Vec<String>,
    /// Complete child environment; the parent environment is cleared.
    pub env: Vec<(String, String)>,
    pub cwd: PathBuf,
    pub cols: u16,
    pub rows: u16,
}

impl PtySpec {
    #[must_use]
    pub fn new(argv: Vec<String>, cwd: impl Into<PathBuf>) -> Self {
        Self {
            argv,
            env: Vec::new(),
            cwd: cwd.into(),
            cols: 80,
            rows: 24,
        }
    }

    #[must_use]
    pub fn env(mut self, key: &str, value: &str) -> Self {
        self.env.push((key.to_string(), value.to_string()));
        self
    }

    #[must_use]
    pub fn size(mut self, cols: u16, rows: u16) -> Self {
        self.cols = cols;
        self.rows = rows;
        self
    }
}

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::{ExitWatcher, Pty, PtyStream};

/// How a child ended.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Exit {
    Code(i32),
    Signal(i32),
}

impl Exit {
    #[must_use]
    pub const fn is_success(self) -> bool {
        matches!(self, Self::Code(0))
    }
}
