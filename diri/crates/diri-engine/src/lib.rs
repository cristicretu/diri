//! The session engine: PTY ownership, output logging, and session state.
//!
//! This crate is the portable replacement for the Swift `DirijorDaemonKit`
//! stack. Everything here is written against the standard library and a thin
//! platform layer, so the parts that cannot be portable are visible as such
//! rather than diffused through the codebase.
//!
//! # Porting rules
//!
//! - **On-disk and on-wire formats are load-bearing.** A log written by the
//!   Swift holder must be readable here byte for byte, and vice versa, or a
//!   user's live sessions cannot survive the switch. Formats are documented
//!   where they are implemented and covered by tests that assert exact bytes.
//! - **Platform-specific code lives behind `cfg` and a named seam**, never
//!   inline in logic. Unix is implemented; Windows is a gap with a defined
//!   shape (see `pty`), not an unbounded rewrite.
//! - **No dependency on the running Swift daemon.** This engine is additive:
//!   it can be built and tested while the existing daemon keeps serving live
//!   sessions.

pub mod agent;
pub mod artifacts;
pub mod attach;
pub mod browser;
pub mod checkpoint;
pub mod control;
pub mod detect;
pub mod directories;
pub mod events;
pub mod git;
pub mod governor;
pub mod history;
#[cfg(unix)]
pub mod holder;
pub mod hooks;
pub mod hosts;
pub mod inject;
pub mod legacy_remote;
pub mod log;
pub mod mcp;
pub mod migrate;
pub mod pr_monitor;
pub mod pty;
pub mod registry;
pub mod remote;
pub mod screen;
pub mod session;
pub mod status;

pub use control::ControlServer;
pub use detect::{ManifestEngine, ManifestState, ScreenObservation, ScreenSnapshot};
pub use log::OutputLog;
pub use pty::{Exit, Pty, PtySpec};
pub use registry::Registry;
pub use screen::HeadlessScreen;
pub use session::{
    HolderConfig, RemoteAdoptSpec, RemoteSessionSpec, Session, SessionSpec, SessionView,
};
pub use status::{Authority, ReducerOutcome, StatusReducer, StatusSignal};
