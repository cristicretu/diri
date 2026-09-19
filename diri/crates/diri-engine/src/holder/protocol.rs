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

/// One-way output protocol. After the NDJSON handshake the holder writes
/// `[offset u64][length u32][payload]` frames until the child exits or the
/// subscriber falls too far behind.
///
/// Frames are contiguous by construction, and each still carries its offset so
/// the subscriber can check rather than trust: a mismatch means fall back to
/// the log, which turns any race here into a resynchronization instead of a
/// silently corrupted screen.
pub const HOLDER_OUTPUT_STREAM_VERSION: u16 = 1;
/// Largest single output frame. A PTY read cannot exceed the pump's buffer,
/// so this only bounds what a subscriber must be willing to allocate.
pub const HOLDER_OUTPUT_MAX_FRAME: usize = 1 << 20;

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
    /// Immutable owned-child birth, verified around this observation. Old
    /// Holders omit it; consumers must not invent it from childPID/startSec.
    #[serde(
        rename = "childIdentity",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub child_identity: Option<diri_proto::process::ProcessIdentity>,
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
    /// Historical wire spelling; this value is the PTY foreground PGID.
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
    /// Whether the PTY's line discipline is reading a secret, sampled from
    /// the master when this stat was taken (see `Pty::secret_input`). `None`
    /// from a holder built before this field existed, which consumers must
    /// read as "not known to be secret", never as a reason to refuse it.
    #[serde(
        rename = "secretInput",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub secret_input: Option<bool>,
}

impl HolderStat {
    /// Reject absent/unsupported identity and inconsistent sibling PID fields.
    /// Presence is evidence supplied by the owning Holder, not permission to
    /// inspect or signal an arbitrary numeric PID on another host.
    pub fn verified_child_identity(&self) -> Option<diri_proto::process::ProcessIdentity> {
        self.child_identity
            .filter(|identity| Some(identity.pid()) == u32::try_from(self.child_pid).ok())
    }
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
    /// Upgrade this connection to a one-way stream of PTY output. Older
    /// holders reject the unknown operation, which is the signal for the
    /// daemon to keep tailing the log file instead.
    #[serde(rename = "output-stream")]
    OutputStream,
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
    /// Stream offset the first output frame will carry. Everything below it
    /// belongs to the log, which the subscriber must finish reading first.
    #[serde(
        rename = "startOffset",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub start_offset: Option<u64>,
}

impl HolderResponse {
    /// Accepts an output-stream upgrade, naming the offset the first frame
    /// will carry.
    pub fn output_stream(version: u16, start_offset: u64) -> Self {
        Self {
            ok: true,
            stream_version: Some(version),
            start_offset: Some(start_offset),
            ..Self::success()
        }
    }

    pub fn success() -> Self {
        Self {
            ok: true,
            stream_version: None,
            error: None,
            stat: None,
            tree: None,
            start_offset: None,
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
            start_offset: None,
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
    // v1 has a reason ("exited"/"signaled") and two optional i32 fields.
    // 128 JSON bytes cover their compact Rust/Swift encodings, including both
    // signed extremes, with room for field order/ordinary whitespace. This is
    // an envelope limit, not permission for unbounded JSON padding/extensions.
    const MAX_STATUS_JSON_BYTES: usize = 128;
    pub const MAX_RETAINED_BYTES: usize =
        Self::PREFIX.len() + Self::MAX_STATUS_JSON_BYTES.div_ceil(3) * 4;

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

    /// Whether a chunk can go straight to the emulator.
    ///
    /// True when it holds no marker and does not end in something that could
    /// become one, which is every chunk of ordinary output. The caller can
    /// then feed the bytes where they lie instead of copying them through an
    /// accumulator to have the same bytes handed back.
    #[must_use]
    pub fn absent_from(chunk: &[u8]) -> bool {
        find(chunk, Self::PREFIX).is_none() && longest_suffix_of_prefix(chunk) == 0
    }

    /// Pulls complete output and markers from a chunk accumulator. A possible
    /// split marker prefix stays buffered for the next append. Returns the
    /// displayable bytes and the last complete exit status found, if any.
    pub fn drain(buffer: &mut Vec<u8>) -> (Vec<u8>, Option<HolderExitStatus>) {
        let mut output = Vec::new();
        let mut exit_status = None;

        let mut consumed = 0;
        while consumed < buffer.len() {
            let remaining = &buffer[consumed..];
            if let Some(marker_start) = find(remaining, Self::PREFIX) {
                if marker_start > 0 {
                    output.extend_from_slice(&remaining[..marker_start]);
                    consumed += marker_start;
                    continue;
                }
                let end = remaining
                    .iter()
                    .take(Self::MAX_RETAINED_BYTES + 1)
                    .position(|&byte| byte == Self::TERMINATOR);
                let payload_end = end.unwrap_or(remaining.len().min(Self::MAX_RETAINED_BYTES + 1));
                let plausible = remaining[Self::PREFIX.len()..payload_end]
                    .iter()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/' | b'='));
                if !plausible || (end.is_none() && remaining.len() > Self::MAX_RETAINED_BYTES) {
                    // This prefix cannot be a bounded marker. Release its first
                    // byte, then preserve normal prefix detection for the rest.
                    output.push(remaining[0]);
                    consumed += 1;
                    continue;
                }
                let Some(end) = end else {
                    break;
                };
                let payload = &remaining[Self::PREFIX.len()..end];
                let status = base64::engine::general_purpose::STANDARD
                    .decode(payload)
                    .ok()
                    .filter(|bytes| bytes.len() <= Self::MAX_STATUS_JSON_BYTES)
                    .and_then(|bytes| serde_json::from_slice::<HolderExitStatus>(&bytes).ok());
                if let Some(status) = status {
                    exit_status = Some(status);
                } else {
                    // Malformed metadata is ordinary terminal input, not a
                    // fabricated process exit and not output we may discard.
                    output.extend_from_slice(&remaining[..=end]);
                }
                consumed += end + 1;
                continue;
            }

            let keep = longest_suffix_of_prefix(remaining);
            let emit = remaining.len() - keep;
            if emit > 0 {
                output.extend_from_slice(&remaining[..emit]);
                consumed += emit;
            }
            break;
        }
        buffer.drain(..consumed);
        // A previous oversized append must not leave its allocation resident.
        if buffer.capacity() > Self::MAX_RETAINED_BYTES * 2 {
            *buffer = buffer.to_vec();
        }
        (output, exit_status)
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    // Anchored on the first byte rather than comparing the whole needle at
    // every offset. Every byte of every session's output passes through here,
    // and a nineteen-byte window comparison per position was costing more than
    // parsing the bytes did.
    let (first, rest) = needle.split_first()?;
    let mut offset = 0;
    while let Some(hit) = haystack[offset..].iter().position(|byte| byte == first) {
        let start = offset + hit;
        if haystack[start + 1..].starts_with(rest) {
            return Some(start);
        }
        offset = start + 1;
    }
    None
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
        assert_eq!(sparse.child_identity, None);
        // A holder that predates the field is never taken to be reading a
        // secret, and the field it does not know stays off its wire.
        assert_eq!(sparse.secret_input, None);
        assert!(
            !serde_json::to_string(&sparse)
                .expect("encode")
                .contains("secretInput")
        );
        let prompting: HolderStat =
            serde_json::from_str(r#"{"childPID":9,"alive":true,"logOffset":0,"secretInput":true}"#)
                .expect("secret stat");
        assert_eq!(prompting.secret_input, Some(true));
        assert_eq!(sparse.verified_child_identity(), None);
        let mut inconsistent = sparse.clone();
        inconsistent.child_identity = Some(
            diri_proto::process::ProcessIdentity::new(
                10,
                diri_proto::process::ProcessBirth::Linux {
                    boot_id: diri_proto::process::BootId::parse(
                        "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
                    )
                    .unwrap(),
                    start_ticks: 42,
                    clock_ticks_per_second: 100,
                },
            )
            .unwrap(),
        );
        assert_eq!(inconsistent.verified_child_identity(), None);
        inconsistent.child_pid = 10;
        assert!(inconsistent.verified_child_identity().is_some());
        let round_trip: HolderStat =
            serde_json::from_str(&serde_json::to_string(&inconsistent).unwrap()).unwrap();
        assert_eq!(
            round_trip.verified_child_identity(),
            inconsistent.child_identity
        );
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
    fn a_corrupt_marker_is_released_without_a_status() {
        let mut buffer = HolderExitMarker::PREFIX.to_vec();
        buffer.extend_from_slice(b"not-base64!!");
        buffer.push(HolderExitMarker::TERMINATOR);
        buffer.extend_from_slice(b"rest");

        let original = buffer.clone();
        let (output, exit) = HolderExitMarker::drain(&mut buffer);
        assert_eq!(exit, None);
        assert_eq!(
            output, original,
            "malformed marker bytes retain their order"
        );
    }
    #[test]
    fn every_marker_boundary_preserves_legitimate_status_and_surrounding_output() {
        for reason in [HolderExitReason::Exited, HolderExitReason::Signaled] {
            for value in [i32::MIN, 0, i32::MAX] {
                let status = HolderExitStatus {
                    reason,
                    code: Some(value),
                    signal: Some(value),
                };
                let marker = HolderExitMarker::encode(&status);
                assert!(marker.len() <= HolderExitMarker::MAX_RETAINED_BYTES + 1);
                let mut input = b"before".to_vec();
                input.extend(&marker);
                input.extend(b"after");
                for split in 0..=input.len() {
                    let mut buffer = input[..split].to_vec();
                    let (mut output, first) = HolderExitMarker::drain(&mut buffer);
                    assert!(buffer.len() <= HolderExitMarker::MAX_RETAINED_BYTES);
                    buffer.extend(&input[split..]);
                    let (tail, second) = HolderExitMarker::drain(&mut buffer);
                    output.extend(tail);
                    assert_eq!(output, b"beforeafter", "split {split}");
                    assert_eq!(second.or(first), Some(status));
                    assert!(buffer.is_empty());
                }
            }
        }
    }
    #[test]
    fn malformed_and_oversized_markers_are_lossless_at_every_boundary() {
        let mut cases = Vec::new();
        for payload in [
            b"not-base64!!".to_vec(),
            b"e30=".to_vec(),
            vec![b'A'; HolderExitMarker::MAX_RETAINED_BYTES * 3],
        ] {
            let mut input = b"before".to_vec();
            input.extend(HolderExitMarker::PREFIX);
            input.extend(payload);
            input.push(HolderExitMarker::TERMINATOR);
            input.extend(b"after");
            cases.push(input);
        }
        for input in cases {
            for split in 0..=input.len() {
                let mut buffer = input[..split].to_vec();
                let (mut output, exit) = HolderExitMarker::drain(&mut buffer);
                assert!(exit.is_none());
                assert!(buffer.len() <= HolderExitMarker::MAX_RETAINED_BYTES);
                buffer.extend(&input[split..]);
                let (tail, exit) = HolderExitMarker::drain(&mut buffer);
                assert!(exit.is_none());
                output.extend(tail);
                assert_eq!(output, input, "split {split}");
                assert!(buffer.is_empty());
            }
        }
    }
    #[test]
    fn unterminated_prefix_stream_retains_bounded_bytes_and_all_output() {
        let mut buffer = HolderExitMarker::PREFIX.to_vec();
        let mut expected = buffer.clone();
        let mut output = Vec::new();
        let chunk = vec![b'A'; 64 * 1024];
        for _ in 0..64 {
            expected.extend(&chunk);
            buffer.extend(&chunk);
            let (bytes, exit) = HolderExitMarker::drain(&mut buffer);
            assert!(exit.is_none());
            output.extend(bytes);
            assert!(buffer.len() <= HolderExitMarker::MAX_RETAINED_BYTES);
            assert!(buffer.capacity() <= HolderExitMarker::MAX_RETAINED_BYTES * 2);
        }
        output.extend(&buffer);
        assert_eq!(output, expected);
    }
    #[test]
    fn repeated_invalid_prefixes_are_drained_in_order_without_retention_growth() {
        let mut piece = HolderExitMarker::PREFIX.to_vec();
        piece.extend(b"AAAA");
        let input = piece.repeat(10_000);
        let mut buffer = input.clone();
        let (mut output, exit) = HolderExitMarker::drain(&mut buffer);
        assert!(exit.is_none());
        assert!(buffer.len() <= HolderExitMarker::MAX_RETAINED_BYTES);
        assert!(buffer.capacity() <= HolderExitMarker::MAX_RETAINED_BYTES * 2);
        output.extend(buffer);
        assert_eq!(output, input);
    }
}
