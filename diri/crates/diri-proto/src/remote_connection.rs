//! Engine-observed remote transport state, independent of Agent process status.
use crate::DateMillis;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum RemoteConnectionState {
    Connecting,
    Connected,
    Reconnecting,
    Failed,
    Exited,
    #[serde(other)]
    Unknown,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteConnection {
    pub state: RemoteConnectionState,
    /// When the Engine observed this transition; not a heartbeat or output age.
    pub since: DateMillis,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn future_states_remain_unknown_instead_of_becoming_connected() {
        let value: RemoteConnection =
            serde_json::from_str(r#"{"state":"future","since":123}"#).unwrap();
        assert_eq!(value.state, RemoteConnectionState::Unknown);
        assert_eq!(value.since, DateMillis(123.0));
    }
}
