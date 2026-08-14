use diri_proto::control::JsonValue;
use diri_proto::methods::HelloResult;

/// Observable state of the daemon control connection.
#[derive(Clone, Debug, PartialEq)]
pub enum ConnectionState {
    Connecting,
    Connected {
        identity: HelloResult,
        event_generation: u64,
    },
    Disconnected(String),
}

/// A sequence-stamped event delivered by the daemon.
#[derive(Clone, Debug, PartialEq)]
pub struct EventEnvelope {
    /// Client connection generation that delivered this event. Sequence
    /// numbers are only comparable within one daemon connection barrier.
    pub generation: u64,
    pub name: String,
    pub seq: u64,
    pub params: JsonValue,
}
