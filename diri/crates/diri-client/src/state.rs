use diri_proto::control::JsonValue;
use diri_proto::methods::HelloResult;

/// Observable state of the daemon control connection.
#[derive(Clone, Debug, PartialEq)]
pub enum ConnectionState {
    Connecting,
    Connected(HelloResult),
    Disconnected(String),
}

impl ConnectionState {
    /// A stable wire name for the state: `connecting`, `connected` or
    /// `disconnected`. Unlike `Debug`, it carries no Engine details and does
    /// not change shape when the Hello payload grows.
    #[must_use]
    pub const fn label(&self) -> &'static str {
        match self {
            Self::Connecting => "connecting",
            Self::Connected(_) => "connected",
            Self::Disconnected(_) => "disconnected",
        }
    }
}

/// A sequence-stamped event delivered by the daemon.
#[derive(Clone, Debug, PartialEq)]
pub struct EventEnvelope {
    pub name: String,
    pub seq: u64,
    pub params: JsonValue,
}
