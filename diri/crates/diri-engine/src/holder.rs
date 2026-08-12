//! Session survival: a holder process owns each PTY so the daemon doesn't.
//!
//! The daemon crashes, upgrades, and restarts; an agent mid-task must not die
//! with it. The holder architecture separates ownership: one lightweight
//! *manager* process per registry hosts one [`server::HolderServer`] per
//! session, each owning exactly one PTY, child tree, and output log. The
//! daemon is just a client — it asks the manager to launch holders, then
//! drives each one over a per-session unix socket and tails its output log
//! from disk.
//!
//! All holder processes and protocols in the active architecture are
//! Rust-owned. The socket paths, NDJSON request/response shapes, pid-file
//! contents and in-band OSC 777 exit marker are versioned internal contracts.
//!
//! Control protocol, per connection: one JSON request line in, one JSON
//! response line out, connection closed. Input/resize may negotiate the
//! additive acknowledged binary stream; older live Holders remain valid and
//! receive the original one-request form.

pub mod client;
pub mod launcher;
pub mod manager;
pub mod paths;
pub mod process_tree;
pub mod protocol;
mod socket;

#[cfg(unix)]
pub mod server;

pub use client::{HolderClient, HolderManagerClient};
pub use launcher::HolderLauncher;
pub use paths::{HolderManagerPaths, HolderPaths};
pub use protocol::{
    HolderExitMarker, HolderExitStatus, HolderLaunchSpec, HolderProcessSample, HolderStat,
};

#[cfg(unix)]
pub use manager::HolderManagerServer;
#[cfg(unix)]
pub use server::HolderServer;

/// Failures across the local holder seam.
#[derive(Debug)]
pub enum HolderError {
    /// The request was malformed or violated a protocol rule.
    InvalidRequest(String),
    /// The socket, file, or process plumbing failed.
    Transport(String),
    /// The far side answered `ok: false`.
    Rejected(String),
    /// A holder or manager could not be started.
    Launch(String),
}

impl std::fmt::Display for HolderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidRequest(message) => write!(f, "holder request: {message}"),
            Self::Transport(message) => write!(f, "holder transport: {message}"),
            Self::Rejected(message) => write!(f, "holder rejected: {message}"),
            Self::Launch(message) => write!(f, "holder launch: {message}"),
        }
    }
}

impl std::error::Error for HolderError {}

impl HolderError {
    /// An io error surfaced by `operation`, in the same `op: strerror` shape
    /// the Swift kit produced.
    pub(crate) fn io(operation: &str, error: std::io::Error) -> Self {
        Self::Transport(format!("{operation}: {error}"))
    }
}

pub type HolderResult<T> = Result<T, HolderError>;
