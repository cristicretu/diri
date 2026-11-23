//! `sandbox.*`: Ghost Desktops, small isolated surfaces an Agent can drive
//! without touching the person's screen, cursor, keyboard focus or windows.
//!
//! The Engine owns a bounded pool of them (four by default). Each sandbox is
//! leased to the session that started it: only that session (or the person,
//! who calls without a session) may act on it, snapshot it or stop it. When
//! the session ends, goes idle for too long, or the Engine exits, the sandbox
//! and everything it created are reclaimed.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{DateMillis, SessionId};

/// Slots in the pool unless the Engine is told otherwise.
pub const DEFAULT_CAPACITY: usize = 4;
/// Viewport when `sandbox.start` does not ask for one.
pub const DEFAULT_WIDTH: u32 = 1280;
pub const DEFAULT_HEIGHT: u32 = 800;
pub const MAX_DIMENSION: u32 = 3840;
pub const MIN_DIMENSION: u32 = 200;
/// How long a person's click in the preview holds the sandbox: Agent
/// actions are refused with [`HUMAN_CONTROL`] until it lapses.
pub const HUMAN_TAKEOVER_MS: u64 = 3_000;
/// The longest a `wait` or `wait_for` action may block.
pub const MAX_WAIT_MS: u64 = 30_000;

/// Error codes an Agent can act on.
pub const POOL_FULL: &str = "sandbox_pool_full";
pub const NOT_FOUND: &str = "sandbox_not_found";
pub const NOT_OWNER: &str = "sandbox_not_owner";
pub const HUMAN_CONTROL: &str = "sandbox_human_control";
pub const BACKEND_UNAVAILABLE: &str = "sandbox_backend_unavailable";
pub const ACTION_FAILED: &str = "sandbox_action_failed";
pub const EXITED: &str = "sandbox_exited";

/// Which isolation mechanism backs a sandbox.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum SandboxBackendKind {
    /// A windowless browser with its own throwaway profile, driven over a
    /// private pipe. No display, no window, no port, no focus.
    #[default]
    Ghost,
    /// A Linux micro-VM (kernel, filesystem and network isolation).
    /// Reported, never silently substituted: unavailable fails closed.
    MicroVm,
}

impl SandboxBackendKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ghost => "ghost",
            Self::MicroVm => "micro_vm",
        }
    }
}

/// `sandbox.start`.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SandboxStartParams {
    /// The owner. Absent: the person, from the app or a plain shell.
    #[serde(rename = "sessionID", default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
    #[serde(default)]
    pub backend: SandboxBackendKind,
    /// Opened once the sandbox is up.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    /// A short name for the preview tile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// One step inside a sandbox. Input goes to the sandbox's own surface
/// through its private channel; nothing reaches the person's input devices.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum SandboxAction {
    /// Load a URL and wait for its load event.
    Navigate {
        url: String,
    },
    /// Click a point in viewport pixels, or the centre of the first element
    /// a CSS selector matches.
    Click {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        x: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        y: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        selector: Option<String>,
    },
    /// Insert text at the focused element.
    Type {
        text: String,
    },
    /// One key: `Enter`, `Tab`, `Escape`, `Backspace`, `ArrowDown`, a
    /// single character.
    Press {
        key: String,
    },
    Scroll {
        #[serde(default)]
        x: f64,
        #[serde(default)]
        y: f64,
        #[serde(default)]
        dx: f64,
        dy: f64,
    },
    /// Evaluate a JavaScript expression in the page; promises are awaited
    /// and the JSON value returned.
    Eval {
        expression: String,
    },
    /// Wait until a selector matches (or `timeoutMs`).
    WaitFor {
        selector: String,
        #[serde(rename = "timeoutMs", default, skip_serializing_if = "Option::is_none")]
        timeout_ms: Option<u64>,
    },
    Wait {
        ms: u64,
    },
    Resize {
        width: u32,
        height: u32,
    },
}

/// `sandbox.act`.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SandboxActParams {
    /// The acting session. Absent: the person, whose action takes the
    /// sandbox over for [`HUMAN_TAKEOVER_MS`].
    #[serde(rename = "sessionID", default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
    #[serde(rename = "sandboxID")]
    pub sandbox_id: String,
    #[serde(flatten)]
    pub action: SandboxAction,
}

/// `sandbox.snapshot`: a full-resolution PNG on disk, for before/after
/// proof, and optionally a smaller JPEG inline for a vision model.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SandboxSnapshotParams {
    #[serde(rename = "sessionID", default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
    #[serde(rename = "sandboxID")]
    pub sandbox_id: String,
    /// Becomes part of the file name (`before`, `after`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default)]
    pub inline: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SandboxSnapshot {
    pub path: String,
    pub width: u32,
    pub height: u32,
    pub bytes: u64,
    /// SHA-256 of the PNG: equal hashes mean nothing visible changed.
    pub sha256: String,
    /// Base64 JPEG, when `inline` was asked for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jpeg: Option<String>,
}

/// `sandbox.stop`.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SandboxStopParams {
    #[serde(rename = "sessionID", default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
    #[serde(rename = "sandboxID")]
    pub sandbox_id: String,
}

/// `sandbox.frame`: the latest live preview frame, for the app's tiles.
/// Asking keeps the sandbox streaming for a couple of seconds; nobody
/// asking means no frames are produced at all.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SandboxFrameParams {
    #[serde(rename = "sandboxID")]
    pub sandbox_id: String,
    /// Skip the payload when the frame is still this one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_seq: Option<u64>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SandboxFrame {
    pub seq: u64,
    pub width: u32,
    pub height: u32,
    /// Base64 JPEG; absent when `after_seq` is still current.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jpeg: Option<String>,
}

/// What one sandbox costs right now, measured, not estimated.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SandboxUsage {
    /// Physical footprint of the sandbox's whole process tree (macOS
    /// `phys_footprint`, Linux PSS).
    pub memory_bytes: u64,
    pub processes: u32,
    /// Bytes allocated on disk for the sandbox's private state. Shared,
    /// read-only binaries are not counted.
    pub disk_bytes: u64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SandboxInfo {
    pub id: String,
    /// Absent: started by the person.
    #[serde(rename = "ownerID", default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<SessionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub backend: SandboxBackendKind,
    pub url: String,
    pub title: String,
    pub width: u32,
    pub height: u32,
    pub started_at: DateMillis,
    pub last_active_at: DateMillis,
    /// From launch to ready for the first action.
    pub spin_up_ms: u64,
    pub usage: SandboxUsage,
    /// The person holds it until then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub human_until: Option<DateMillis>,
    /// Trimmed after a quiet spell: caches dropped, still running.
    #[serde(default)]
    pub sleeping: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SandboxBackendStatus {
    pub kind: SandboxBackendKind,
    pub available: bool,
    /// Why not, or what runs it.
    pub detail: String,
}

/// `sandbox.list`.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SandboxListResult {
    pub capacity: usize,
    pub sandboxes: Vec<SandboxInfo>,
    pub backends: Vec<SandboxBackendStatus>,
}

/// `sandbox.list` takes nothing but tolerates a session, so every tool
/// call can carry one.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SandboxListParams {
    #[serde(rename = "sessionID", default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
}

/// Clamp a requested viewport edge into the supported range.
pub fn clamp_dimension(requested: Option<u32>, default: u32) -> u32 {
    requested
        .unwrap_or(default)
        .clamp(MIN_DIMENSION, MAX_DIMENSION)
}

/// Used by `sandbox.act` results that carry nothing else.
pub fn ok() -> Value {
    serde_json::json!({ "ok": true })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn act_params_flatten_the_action_tag() {
        let params: SandboxActParams = serde_json::from_value(json!({
            "sessionID": "s1",
            "sandboxID": "g1",
            "action": "click",
            "selector": "#go"
        }))
        .unwrap();
        assert_eq!(params.sandbox_id, "g1");
        assert_eq!(
            params.action,
            SandboxAction::Click {
                x: None,
                y: None,
                selector: Some("#go".into())
            }
        );
        let round = serde_json::to_value(&params).unwrap();
        assert_eq!(round["action"], "click");
        assert_eq!(round["selector"], "#go");
    }

    #[test]
    fn person_calls_carry_no_session() {
        let params: SandboxActParams = serde_json::from_value(json!({
            "sandboxID": "g1", "action": "press", "key": "Enter"
        }))
        .unwrap();
        assert!(params.session_id.is_none());
    }

    #[test]
    fn dimensions_clamp() {
        assert_eq!(clamp_dimension(None, DEFAULT_WIDTH), DEFAULT_WIDTH);
        assert_eq!(clamp_dimension(Some(10), DEFAULT_WIDTH), MIN_DIMENSION);
        assert_eq!(clamp_dimension(Some(99_999), DEFAULT_WIDTH), MAX_DIMENSION);
    }
}
