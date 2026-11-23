//! Context Ring actions: recent screen/browser captures. Deferred: they are
//! never in `tools/list`, so they add nothing to an Agent's prompt; Agents
//! find them through `get_skill` and run them with `act` or a direct call.

use super::*;

impl Bridge {
    pub(super) fn list_clips(&self, args: &Value) -> Result<Value, String> {
        let limit = optional_number(args, "limit")
            .unwrap_or(10.0)
            .clamp(1.0, 50.0) as u32;
        self.request(
            Method::CLIP_LIST,
            json!({ "limit": limit }),
            DEFAULT_TIMEOUT,
        )
    }

    /// A clip with its text and the local screenshot path an Agent can open.
    pub(super) fn get_clip(&self, args: &Value) -> Result<Value, String> {
        let id = optional_string(args, "id").unwrap_or_else(|| "latest".into());
        self.request(Method::CLIP_GET, json!({ "id": id }), DEFAULT_TIMEOUT)
    }

    pub(super) fn capture_clip(&self, args: &Value) -> Result<Value, String> {
        let mut params = json!({});
        if let Some(mode) = optional_string(args, "mode") {
            params["mode"] = json!(mode);
        }
        self.request(Method::CLIP_CAPTURE, params, DEFAULT_TIMEOUT)?;
        Ok(json!({
            "ok": true,
            "note": "Diri asked the person to capture; the clip appears in list_clips once they finish.",
        }))
    }

    /// Sends a clip to another Session under the same reach as send_prompt.
    pub(super) fn send_clip(&self, args: &Value) -> Result<Value, String> {
        let target = required_string(args, "session_id")?;
        let id = optional_string(args, "id").unwrap_or_else(|| "latest".into());
        let prompt = optional_string(args, "prompt").unwrap_or_default();
        let snapshot = self.snapshot()?;
        McpPolicy::new(
            &snapshot.sessions,
            &snapshot.projects,
            self.caller.as_deref(),
        )?
        .authorize(WriteAction::SendPrompt { target: &target })?;
        self.request(
            Method::CLIP_DISPATCH,
            json!({
                "id": id,
                "prompt": prompt,
                "destination": {"type": "session", "sessionID": target},
            }),
            Duration::from_secs(30),
        )
    }
}
