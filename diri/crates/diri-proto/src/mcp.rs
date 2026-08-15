//! Stable, machine-readable MCP tool failures.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::control::ControlError;

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct McpToolErrorEnvelope {
    pub error: McpToolError,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpToolError {
    pub code: String,
    pub message: String,
    pub model_guidance: String,
    pub retryable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suggested_tool: Option<String>,
    #[serde(default = "empty_details")]
    pub details: Value,
}

impl McpToolErrorEnvelope {
    pub fn new(
        code: impl Into<String>,
        message: impl Into<String>,
        model_guidance: impl Into<String>,
        retryable: bool,
        suggested_tool: Option<&str>,
    ) -> Self {
        Self {
            error: McpToolError {
                code: code.into(),
                message: message.into(),
                model_guidance: model_guidance.into(),
                retryable,
                suggested_tool: suggested_tool.map(str::to_owned),
                details: empty_details(),
            },
        }
    }

    pub fn with_details(mut self, details: Value) -> Self {
        self.error.details = details;
        self
    }

    /// Encodes the envelope as the MCP text content. This deliberately stays
    /// JSON text rather than structuredContent so older clients still show it.
    pub fn to_text(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| {
            "{\"error\":{\"code\":\"internal\",\"message\":\"Could not encode the tool failure.\",\"modelGuidance\":\"Report this Diri error and do not assume the mutation succeeded.\",\"retryable\":false,\"details\":{}}}".to_owned()
        })
    }

    /// Preserves an already-typed failure and upgrades legacy prose to a
    /// conservative envelope.
    pub fn normalize_text(message: &str) -> String {
        if serde_json::from_str::<Self>(message).is_ok() {
            message.to_owned()
        } else {
            Self::from_legacy(message).to_text()
        }
    }

    pub fn from_legacy(message: &str) -> Self {
        let lower = message.to_ascii_lowercase();
        if lower.contains("missing required argument")
            || lower.contains("requires a tool name")
            || lower.contains(" is required")
        {
            return Self::new(
                "invalid_arguments",
                message,
                "Correct the named arguments, then call the tool again.",
                false,
                None,
            );
        }
        if lower.contains("no session")
            || lower.contains("no such session")
            || lower.contains("session not found")
        {
            return Self::new(
                "session_not_found",
                message,
                "Call list_agents, choose an existing session ID, and retry with a new requestKey.",
                false,
                Some("list_agents"),
            );
        }
        if lower.contains("cannot target")
            || lower.contains("cannot terminate")
            || lower.contains("refuses to kill")
            || lower.contains("must run inside a diri session")
            || lower.contains("did not identify itself")
            || lower.contains("dirijor_session_id is unset")
        {
            return Self::new(
                "authorization_denied",
                message,
                "Choose a session allowed by Diri's parent-child policy or answer in the current session.",
                false,
                Some("list_agents"),
            );
        }
        if lower.contains("unknown tool") {
            return Self::new(
                "tool_not_found",
                message,
                "Call tools/list and choose one of the advertised Diri tools.",
                false,
                None,
            );
        }
        if lower.contains("idempotency_conflict") {
            return Self::new(
                "idempotency_conflict",
                message,
                "Use the original arguments for this requestKey, or choose a new requestKey for a different spawn.",
                false,
                None,
            );
        }
        if lower.contains("timed out") || lower.contains("timeout") {
            return Self::new(
                "timeout",
                message,
                "Inspect current Diri state before retrying a mutation with the same requestKey.",
                true,
                Some("list_agents"),
            );
        }
        if lower.contains("poisoned") || lower.contains("internal error") {
            return Self::new(
                "internal",
                message,
                "Inspect current Diri state, then retry once if no mutation occurred.",
                true,
                Some("list_agents"),
            );
        }
        Self::new(
            "tool_failed",
            message,
            "Inspect the error, verify current Diri state, and retry only if the operation is safe.",
            false,
            None,
        )
    }

    /// Translates the Engine's stable control failures into the equally
    /// stable recovery contract exposed by every MCP adapter. Keeping this in
    /// `diri-proto` prevents the stdio bridge and embedded host from assigning
    /// different codes or retry guidance to the same daemon outcome.
    pub fn from_control(error: ControlError) -> Self {
        let lower = error.message.to_ascii_lowercase();
        match error.code.as_str() {
            "idempotency_conflict" => Self::new(
                "idempotency_conflict",
                error.message,
                "Use the original arguments for this requestKey, or choose a new requestKey for a different spawn.",
                false,
                None,
            ),
            "idempotency_requires_caller" => Self::new(
                "idempotency_requires_caller",
                error.message,
                "Run spawn_agent inside a Diri session, or omit requestKey for an unscoped call.",
                false,
                Some("whoami"),
            ),
            "idempotency_caller_not_found" | "idempotency_caller_retired" => Self::new(
                error.code,
                error.message,
                "The calling session no longer exists. Start or reopen a Diri session before spawning a child.",
                false,
                Some("whoami"),
            ),
            "idempotency_capacity" => Self::new(
                "idempotency_capacity",
                error.message,
                "Wait for current spawns to settle, then retry the same requestKey.",
                true,
                Some("list_agents"),
            ),
            "idempotency_outcome_uncertain" => Self::new(
                "idempotency_outcome_uncertain",
                error.message,
                "Do not spawn again with this key. Inspect list_agents and the repository worktrees before deciding how to recover.",
                false,
                Some("list_agents"),
            ),
            "bad_request" if lower.contains("cwd") && lower.contains("not a directory") => {
                Self::new(
                    "cwd_not_found",
                    error.message,
                    "Choose an existing project directory and retry with a new requestKey.",
                    false,
                    Some("list_agents"),
                )
            }
            "bad_request" => Self::new(
                "invalid_arguments",
                error.message,
                "Correct the requested arguments, then retry with a new requestKey.",
                false,
                None,
            ),
            "not_found" => Self::new(
                "resource_not_found",
                error.message,
                "Refresh Diri state, choose an existing resource, and retry with a new requestKey.",
                false,
                Some("list_agents"),
            ),
            "unauthorized" => Self::new(
                "authorization_denied",
                error.message,
                "Reconnect through the current Diri session; do not retry with the same credentials.",
                false,
                Some("whoami"),
            ),
            "version_mismatch" => Self::new(
                "version_mismatch",
                error.message,
                "Restart or update Diri so the MCP adapter and Engine use the same protocol.",
                false,
                None,
            ),
            "initial_prompt_delivery_failed" => Self::new(
                "initial_prompt_delivery_failed",
                error.message,
                "The session exists. Inspect list_agents and send_prompt to that session instead of spawning another one.",
                false,
                Some("list_agents"),
            ),
            "internal" => Self::new(
                "internal",
                error.message,
                "Inspect list_agents before retrying; if no mutation occurred, retry once with the same requestKey.",
                true,
                Some("list_agents"),
            ),
            code => Self::new(
                code,
                error.message,
                "Inspect current Diri state, then retry only when the operation is safe.",
                false,
                Some("list_agents"),
            ),
        }
    }
}

fn empty_details() -> Value {
    json!({})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_uses_stable_camel_case_json() {
        let envelope = McpToolErrorEnvelope::new(
            "cwd_not_found",
            "The directory is gone.",
            "Choose an existing project directory.",
            false,
            Some("list_agents"),
        )
        .with_details(json!({"cwd": "/gone"}));
        let value: Value = serde_json::from_str(&envelope.to_text()).expect("valid JSON");
        assert_eq!(value["error"]["code"], "cwd_not_found");
        assert_eq!(
            value["error"]["modelGuidance"],
            "Choose an existing project directory."
        );
        assert_eq!(value["error"]["suggestedTool"], "list_agents");
        assert_eq!(value["error"]["details"]["cwd"], "/gone");
    }

    #[test]
    fn normalize_preserves_typed_errors_and_upgrades_prose() {
        let typed =
            McpToolErrorEnvelope::new("internal", "boom", "Retry once.", true, None).to_text();
        assert_eq!(McpToolErrorEnvelope::normalize_text(&typed), typed);
        let upgraded: Value = serde_json::from_str(&McpToolErrorEnvelope::normalize_text(
            "missing required argument: cwd",
        ))
        .expect("valid JSON");
        assert_eq!(upgraded["error"]["code"], "invalid_arguments");

        for (message, code, retryable) in [
            ("no such session: s_gone", "session_not_found", false),
            (
                "release_agent cannot terminate its caller",
                "authorization_denied",
                false,
            ),
            (
                "this session did not identify itself; DIRIJOR_SESSION_ID is unset",
                "authorization_denied",
                false,
            ),
            ("engine state is poisoned", "internal", true),
            ("operation timed out", "timeout", true),
        ] {
            let upgraded: McpToolErrorEnvelope =
                serde_json::from_str(&McpToolErrorEnvelope::normalize_text(message))
                    .expect("typed JSON");
            assert_eq!(upgraded.error.code, code, "{message}");
            assert_eq!(upgraded.error.retryable, retryable, "{message}");
        }
    }

    #[test]
    fn control_mapping_covers_safe_idempotency_recovery() {
        let uncertain = McpToolErrorEnvelope::from_control(ControlError::new(
            "idempotency_outcome_uncertain",
            "the spawn panicked after it may have committed",
        ));
        assert_eq!(uncertain.error.code, "idempotency_outcome_uncertain");
        assert!(!uncertain.error.retryable);
        assert_eq!(
            uncertain.error.suggested_tool.as_deref(),
            Some("list_agents")
        );

        let validation = McpToolErrorEnvelope::from_control(ControlError::bad_request(
            "requestKey must be a string containing 1 to 128 bytes",
        ));
        assert_eq!(validation.error.code, "invalid_arguments");
    }
}
