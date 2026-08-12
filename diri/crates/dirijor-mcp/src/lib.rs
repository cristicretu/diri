//! Rust automation surfaces that run beside the Diri Engine.
//!
//! `dirijor-mcp` is the long-lived stdio frontend used by agents. `dirijor`
//! keeps the hook, notify, and shell-automation argv contracts. Both speak the
//! Engine's control protocol directly; neither embeds or launches another
//! language runtime.

pub mod bridge;
pub mod control;
pub mod tools;

pub use bridge::Bridge;
pub use control::{ControlClient, ControlFailure, default_socket_path};
