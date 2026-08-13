//! Stable, machine-readable MCP tool failures.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

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
        if lower.contains("no session") || lower.contains("session not found") {
            return Self::new(
                "session_not_found",
                message,
                "Call list_agents, choose an existing session ID, and retry with a new requestKey.",
                false,
                Some("list_agents"),
            );
        }
        if lower.contains("cannot target")
            || lower.contains("refuses to kill")
            || lower.contains("must run inside a diri session")
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
        Self::new(
            "tool_failed",
            message,
            "Inspect the error, verify current Diri state, and retry only if the operation is safe.",
            false,
            None,
        )
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
    }
}
