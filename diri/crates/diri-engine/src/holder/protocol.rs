//! The holder wire types and the in-band exit marker.
//!
//! JSON key spelling matches Swift's synthesized Codable exactly
//! (`sessionID`, `childPID`, `managerPID`, `kill-tree`, …) and optionals are
//! omitted when absent, which is what `encodeIfPresent` does. Golden-string
//! tests below pin both directions.

use std::collections::HashMap;

use base64::Engine as _;
use serde::{Deserialize, Serialize};

use super::paths::MANAGER_PROTOCOL_VERSION;

/// Everything a holder needs to own one session: what to run, where its
/// control endpoints live, and where output goes.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct HolderLaunchSpec {
    #[serde(rename = "sessionID")]
    pub session_id: String,
    #[serde(rename = "socketPath")]
    pub socket_path: String,
    #[serde(rename = "pidFilePath")]
    pub pid_file_path: String,
    #[serde(rename = "logFilePath")]
    pub log_file_path: String,
    pub argv: Vec<String>,
    pub cwd: String,
    pub environment: HashMap<String, String>,
    pub cols: u16,
    pub rows: u16,
    #[serde(rename = "diskCapacity")]
    pub disk_capacity: i64,
}

/// Default output-log spill cap, matching the Swift spec default.
pub const DEFAULT_DISK_CAPACITY: i64 = 32 << 20;

/// Additive local Holder input protocol negotiated over the legacy NDJSON
/// connection. Version 1 frames are `[kind u8][length u32][payload]` and each
/// frame receives one acknowledgement byte after the PTY operation completes.
pub const HOLDER_STREAM_VERSION: u16 = 1;
pub const HOLDER_STREAM_INPUT: u8 = 1;
pub const HOLDER_STREAM_RESIZE: u8 = 2;
pub const HOLDER_STREAM_ACK: u8 = 0;
pub const HOLDER_STREAM_MAX_PAYLOAD: usize = 1 << 20;

/// A (pid, start time) pair. The start time is the identity check that makes
/// signalling a recycled pid safe.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct HolderProcessSample {
    pub pid: i32,
    #[serde(rename = "startSec")]
    pub start_sec: i64,
}

/// A holder's answer to `stat`: the child, its liveness, and the log tail.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct HolderStat {
    #[serde(rename = "childPID")]
    pub child_pid: i32,
    pub alive: bool,
    #[serde(rename = "logOffset")]
    pub log_offset: u64,
    #[serde(
        rename = "foregroundPID",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub foreground_pid: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cols: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rows: Option<u16>,
    /// Stream offset of the first byte THIS holder incarnation wrote. The
    /// per-session log survives relaunches under the same session id, so bytes
    /// below this offset — including a prior incarnation's exit marker —
    /// belong to previous incarnations and must not be attributed to this
    /// child. `None` when talking to a holder built before this field existed.
    #[serde(
        rename = "epochOffset",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub epoch_offset: Option<u64>,
}

/// How the held child ended.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct HolderExitStatus {
    pub reason: HolderExitReason,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<i32>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum HolderExitReason {
    Exited,
    Signaled,
}

/// The per-session request set.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum HolderOperation {
    /// Upgrade this connection to the acknowledged binary input protocol.
    /// Older live holders reject the unknown operation, which is the explicit
    /// signal for a new daemon to keep using one-request NDJSON.
    #[serde(rename = "stream")]
    Stream,
    #[serde(rename = "write")]
    Write,
    #[serde(rename = "resize")]
    Resize,
    #[serde(rename = "signal")]
    Signal,
    #[serde(rename = "kill-tree")]
    KillTree,
    #[serde(rename = "stat")]
    Stat,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct HolderRequest {
    pub op: HolderOperation,
    #[serde(
        rename = "streamVersion",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub stream_version: Option<u16>,
    /// base64 payload for `write`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cols: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rows: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sig: Option<i32>,
}

impl HolderRequest {
    pub fn op(op: HolderOperation) -> Self {
        Self {
            op,
            stream_version: None,
            data: None,
            cols: None,
            rows: None,
            sig: None,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct HolderResponse {
    pub ok: bool,
    #[serde(
        rename = "streamVersion",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub stream_version: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stat: Option<HolderStat>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tree: Option<Vec<HolderProcessSample>>,
}

impl HolderResponse {
    pub fn success() -> Self {
        Self {
            ok: true,
            stream_version: None,
            error: None,
            stat: None,
            tree: None,
        }
    }

    pub fn with_stat(stat: HolderStat) -> Self {
        Self {
            stat: Some(stat),
            ..Self::success()
        }
    }

    pub fn with_tree(tree: Vec<HolderProcessSample>) -> Self {
        Self {
            tree: Some(tree),
            ..Self::success()
        }
    }

    pub fn failure(message: impl Into<String>) -> Self {
        Self {
            ok: false,
            stream_version: None,
            error: Some(message.into()),
            stat: None,
            tree: None,
        }
    }

    pub fn stream(version: u16) -> Self {
        Self {
            stream_version: Some(version),
            ..Self::success()
        }
    }
}

/// The manager request set: create session holders, nothing else. Session
/// traffic never flows through the manager socket.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum HolderManagerOperation {
    Ping,
    Launch,
    #[serde(rename = "shutdown-if-idle")]
    ShutdownIfIdle,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct HolderManagerRequest {
    pub version: u32,
    pub op: HolderManagerOperation,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spec: Option<HolderLaunchSpec>,
}

impl HolderManagerRequest {
    pub fn ping() -> Self {
        Self {
            version: MANAGER_PROTOCOL_VERSION,
            op: HolderManagerOperation::Ping,
            spec: None,
        }
    }

    pub fn launch(spec: HolderLaunchSpec) -> Self {
        Self {
            version: MANAGER_PROTOCOL_VERSION,
            op: HolderManagerOperation::Launch,
            spec: Some(spec),
        }
    }

    pub fn shutdown_if_idle() -> Self {
        Self {
            version: MANAGER_PROTOCOL_VERSION,
            op: HolderManagerOperation::ShutdownIfIdle,
            spec: None,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct HolderManagerResponse {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(
        rename = "managerPID",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub manager_pid: Option<i32>,
}

impl HolderManagerResponse {
    pub fn success(manager_pid: i32) -> Self {
        Self {
            ok: true,
            error: None,
            manager_pid: Some(manager_pid),
        }
    }

    pub fn failure(message: impl Into<String>) -> Self {
        Self {
            ok: false,
            error: Some(message.into()),
            manager_pid: None,
        }
    }
}

/// The in-band exit record: an OSC sequence that is invisible to terminal
/// clients but remains part of the monotonic byte stream, so an exit that
/// happens while no daemon is running is still observed later. The daemon
/// strips it before feeding the emulator.
pub struct HolderExitMarker;

impl HolderExitMarker {
    pub const PREFIX: &'static [u8] = b"\x1b]777;dirijor-exit=";
    pub const TERMINATOR: u8 = 0x07;

    pub fn encode(status: &HolderExitStatus) -> Vec<u8> {
        let Ok(payload) = serde_json::to_vec(status) else {
            return Vec::new();
        };
        let mut marker = Self::PREFIX.to_vec();
        marker.extend_from_slice(
            base64::engine::general_purpose::STANDARD
                .encode(payload)
                .as_bytes(),
        );
        marker.push(Self::TERMINATOR);
        marker
    }

    /// Pulls complete output and markers from a chunk accumulator. A possible
    /// split marker prefix stays buffered for the next append. Returns the
    /// displayable bytes and the last complete exit status found, if any.
    pub fn drain(buffer: &mut Vec<u8>) -> (Vec<u8>, Option<HolderExitStatus>) {
        let mut output = Vec::new();
        let mut exit_status = None;

        while !buffer.is_empty() {
            if let Some(marker_start) = find(buffer, Self::PREFIX) {
                if marker_start > 0 {
                    output.extend_from_slice(&buffer[..marker_start]);
                    buffer.drain(..marker_start);
                    continue;
                }
                let Some(end) = buffer.iter().position(|&byte| byte == Self::TERMINATOR) else {
                    break; // incomplete marker; wait for more bytes
                };
                let payload = &buffer[Self::PREFIX.len()..end];
                if let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(payload)
                    && let Ok(status) = serde_json::from_slice::<HolderExitStatus>(&decoded)
                {
                    exit_status = Some(status);
                }
                buffer.drain(..=end);
                continue;
            }

            let keep = longest_suffix_of_prefix(buffer);
            let emit = buffer.len() - keep;
            if emit > 0 {
                output.extend_from_slice(&buffer[..emit]);
                buffer.drain(..emit);
            }
            break;
        }
        (output, exit_status)
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn longest_suffix_of_prefix(data: &[u8]) -> usize {
    let max_length = data.len().min(HolderExitMarker::PREFIX.len() - 1);
    (1..=max_length)
        .rev()
        .find(|&length| data[data.len() - length..] == HolderExitMarker::PREFIX[..length])
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_launch_spec_uses_swift_codable_key_spelling() {
        let spec = HolderLaunchSpec {
            session_id: "s_1".into(),
            socket_path: "/h/s_1.sock".into(),
            pid_file_path: "/h/s_1.pid".into(),
            log_file_path: "/l/s_1.bin".into(),
            argv: vec!["/bin/cat".into()],
            cwd: "/tmp".into(),
            environment: HashMap::from([("TERM".to_string(), "xterm-256color".to_string())]),
            cols: 120,
            rows: 32,
            disk_capacity: DEFAULT_DISK_CAPACITY,
        };
        let json: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&spec).expect("encode")).expect("parse");
        for key in [
            "sessionID",
            "socketPath",
            "pidFilePath",
            "logFilePath",
            "argv",
            "cwd",
            "environment",
            "cols",
            "rows",
            "diskCapacity",
        ] {
            assert!(json.get(key).is_some(), "missing Swift key {key}: {json}");
        }
        assert_eq!(json["diskCapacity"], 32 << 20);
    }

    #[test]
    fn a_swift_encoded_stat_decodes_with_optionals_present_or_absent() {
        // What Swift's JSONEncoder produces with every optional set…
        let full: HolderStat = serde_json::from_str(
            r#"{"childPID":123,"alive":true,"logOffset":4096,"foregroundPID":456,"cols":120,"rows":32,"epochOffset":1024}"#,
        )
        .expect("full stat");
        assert_eq!(full.child_pid, 123);
        assert_eq!(full.epoch_offset, Some(1024));

        // …and with them omitted, as a pre-epoch holder would send.
        let sparse: HolderStat =
            serde_json::from_str(r#"{"childPID":9,"alive":false,"logOffset":0}"#).expect("sparse");
        assert_eq!(sparse.foreground_pid, None);
        assert_eq!(sparse.epoch_offset, None);
    }

    #[test]
    fn requests_spell_operations_the_swift_way() {
        let kill =
            serde_json::to_string(&HolderRequest::op(HolderOperation::KillTree)).expect("encode");
        assert_eq!(
            kill, r#"{"op":"kill-tree"}"#,
            "hyphenated, optionals omitted"
        );

        let decoded: HolderRequest =
            serde_json::from_str(r#"{"op":"write","data":"aGk="}"#).expect("decode");
        assert_eq!(decoded.op, HolderOperation::Write);
        assert_eq!(decoded.data.as_deref(), Some("aGk="));
    }

    #[test]
    fn manager_messages_round_trip_with_swift_keys() {
        let ping = serde_json::to_string(&HolderManagerRequest::ping()).expect("encode");
        assert_eq!(ping, r#"{"version":1,"op":"ping"}"#);
        let shutdown =
            serde_json::to_string(&HolderManagerRequest::shutdown_if_idle()).expect("encode");
        assert_eq!(shutdown, r#"{"version":1,"op":"shutdown-if-idle"}"#);

        let response: HolderManagerResponse =
            serde_json::from_str(r#"{"ok":true,"managerPID":4242}"#).expect("decode");
        assert_eq!(response.manager_pid, Some(4242));

        let encoded = serde_json::to_string(&HolderManagerResponse::success(7)).expect("encode");
        assert!(encoded.contains(r#""managerPID":7"#), "{encoded}");
    }

    #[test]
    fn the_exit_marker_round_trips() {
        let status = HolderExitStatus {
            reason: HolderExitReason::Signaled,
            code: None,
            signal: Some(15),
        };
        let mut buffer = b"before".to_vec();
        buffer.extend_from_slice(&HolderExitMarker::encode(&status));
        buffer.extend_from_slice(b"after");

        let (output, exit) = HolderExitMarker::drain(&mut buffer);
        assert_eq!(output, b"beforeafter");
        assert_eq!(exit, Some(status));
        assert!(buffer.is_empty());
    }

    #[test]
    fn a_marker_split_across_chunks_stays_buffered() {
        let status = HolderExitStatus {
            reason: HolderExitReason::Exited,
            code: Some(0),
            signal: None,
        };
        let marker = HolderExitMarker::encode(&status);
        let (head, tail) = marker.split_at(7); // inside the OSC prefix

        let mut buffer = b"output".to_vec();
        buffer.extend_from_slice(head);
        let (output, exit) = HolderExitMarker::drain(&mut buffer);
        assert_eq!(output, b"output", "the possible prefix must not be emitted");
        assert_eq!(exit, None);
        assert_eq!(buffer, head, "the partial marker stays buffered");

        buffer.extend_from_slice(tail);
        let (output, exit) = HolderExitMarker::drain(&mut buffer);
        assert!(output.is_empty());
        assert_eq!(exit, Some(status));
    }

    #[test]
    fn the_marker_bytes_match_the_swift_construction() {
        let status = HolderExitStatus {
            reason: HolderExitReason::Exited,
            code: Some(3),
            signal: None,
        };
        let marker = HolderExitMarker::encode(&status);
        assert!(marker.starts_with(b"\x1b]777;dirijor-exit="));
        assert_eq!(*marker.last().expect("terminator"), 0x07);
        // The payload is base64 of the JSON body, exactly.
        let payload = &marker[HolderExitMarker::PREFIX.len()..marker.len() - 1];
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(payload)
            .expect("base64");
        let parsed: HolderExitStatus = serde_json::from_slice(&decoded).expect("json");
        assert_eq!(parsed, status);
    }

    #[test]
    fn a_corrupt_marker_is_consumed_without_a_status() {
        let mut buffer = HolderExitMarker::PREFIX.to_vec();
        buffer.extend_from_slice(b"not-base64!!");
        buffer.push(HolderExitMarker::TERMINATOR);
        buffer.extend_from_slice(b"rest");

        let (output, exit) = HolderExitMarker::drain(&mut buffer);
        assert_eq!(exit, None);
        assert_eq!(output, b"rest", "the broken marker itself is swallowed");
    }
}
