//! Remote PTY transport integration.
//!
//! The previous SSH-PTY/terminal-multiplexer transport was removed at the
//! start of the refactor. This module now owns Helper bootstrap, SSH byte
//! channels, authenticated local bindings, session control and reconnect.
//! `transport_unavailable` remains the fail-closed result for builds without
//! a valid complete supported-platform Helper catalog.

use diri_proto::ControlError;

pub mod binding;
pub mod bootstrap;
pub mod client;
pub mod executor;
pub mod manager;
pub mod ssh;

pub const TRANSPORT_UNAVAILABLE_CODE: &str = "remote_transport_unavailable";

#[must_use]
pub fn transport_unavailable() -> ControlError {
    ControlError::new(
        TRANSPORT_UNAVAILABLE_CODE,
        "the Rust remote PTY Holder transport is not available in this build",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unavailable_is_a_stable_structured_error() {
        let error = transport_unavailable();
        assert_eq!(error.code, TRANSPORT_UNAVAILABLE_CODE);
        assert!(error.message.contains("remote PTY Holder"));
    }
}
