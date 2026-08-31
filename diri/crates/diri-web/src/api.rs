//! The JSON surface the phone frontend drives.
//!
//! Every handler is a thin translation of one `DaemonClient` call. Nothing
//! here holds state: the daemon is the only source of truth, so a phone that
//! reconnects on a new cell tower sees the same world it left.

use std::sync::Arc;
use std::time::Duration;

use diri_client::{ClientError, DaemonClient};
use diri_proto::{AgentKind, SessionDiffBase, SessionId, SessionSpawnParams};
use serde_json::{Value, json};

use crate::auth::{self, Auth};
use crate::http::{Request, Response};

/// Long enough for a cold `git diff` on a large repo, short enough that a
/// wedged daemon shows up as an error on the phone instead of a spinner.
const CALL_TIMEOUT: Duration = Duration::from_secs(20);

pub struct Api {
    pub client: Arc<DaemonClient>,
    pub auth: Auth,
    pub host_label: String,
}

impl Api {
    pub async fn route(&self, request: &Request) -> Response {
        if !self.auth.authorizes(request) {
            return auth::unauthorized();
        }
        // Reads are safe to serve cross-origin; state changes are not.
        let mutating = matches!(request.method.as_str(), "POST" | "PUT" | "DELETE");
        if mutating && !auth::origin_is_acceptable(request) {
            return Response::error(401, "cross-origin request refused");
        }

        let segments = request.segments();
        match (request.method.as_str(), segments.as_slice()) {
            ("GET" | "HEAD", ["api", "health"]) => self.health().await,
            ("GET", ["api", "sessions"]) => self.sessions().await,
            ("GET", ["api", "agents"]) => self.agents().await,
            ("POST", ["api", "spawn"]) => self.spawn(request).await,

            ("GET", ["api", "session", id, "screen"]) => self.screen(id).await,
            ("GET", ["api", "session", id, "scrollback"]) => self.scrollback(id).await,
            ("GET", ["api", "session", id, "diff"]) => self.diff(id).await,
            ("POST", ["api", "session", id, "send"]) => self.send(id, request).await,
            ("POST", ["api", "session", id, "key"]) => self.key(id, request).await,
            ("POST", ["api", "session", id, "resize"]) => self.resize(id, request).await,
            ("POST", ["api", "session", id, "seen"]) => self.seen(id).await,
            ("POST", ["api", "session", id, "kill"]) => self.kill(id).await,

            // A browser asks for things nobody wrote a route for — /favicon.ico
            // above all. That is a 404, not a 405; answering "method not
            // allowed" to a plain GET is just wrong, and it shows up as a
            // console error on every load.
            ("GET" | "HEAD" | "POST", _) => Response::error(404, "no such endpoint"),
            _ => Response::error(405, "method not allowed"),
        }
    }

    async fn health(&self) -> Response {
        let state = format!("{:?}", *self.client.connection_state().borrow());
        Response::json(&json!({
            "ok": true,
            "host": self.host_label,
            "daemon": state,
            "socket": self.client.socket_path().to_string_lossy(),
        }))
    }

    async fn sessions(&self) -> Response {
        match self.client.sessions().await {
            // Projects travel with the sessions. Without them a client can only
            // guess a group's name from a path, and the daemon's project ids
            // are opaque (`p_6e5a8d7b38f5`) — so the guess is a raw id on
            // screen where the desktop shows a repository name.
            Ok(result) => Response::json(&json!({
                "host": self.host_label,
                "sessions": serde_json::to_value(&result.sessions).unwrap_or(Value::Null),
                "projects": serde_json::to_value(&result.projects).unwrap_or(Value::Null),
            })),
            Err(error) => daemon_error(&error),
        }
    }

    async fn agents(&self) -> Response {
        // This frontend always serves the daemon it is attached to, so the
        // catalog is this host's. `force_refresh` stays false: the phone asks
        // on every sheet open, and re-probing PATH each time would make the
        // picker slow for a list that changes about once a month.
        let params = diri_proto::AgentReadinessParams {
            host: None,
            force_refresh: false,
        };
        match self.client.agent_readiness(params).await {
            Ok(result) => Response::json(&serde_json::to_value(&result).unwrap_or(Value::Null)),
            Err(error) => daemon_error(&error),
        }
    }

    async fn screen(&self, id: &str) -> Response {
        match self.client.read_screen(&session_id(id)).await {
            Ok(result) => Response::json(&json!({
                "text": result.text,
                "cols": result.cols,
                "rows": result.rows,
            })),
            Err(error) => daemon_error(&error),
        }
    }

    async fn scrollback(&self, id: &str) -> Response {
        match self.client.read_scrollback(&session_id(id)).await {
            Ok(result) => Response::json(&serde_json::to_value(&result).unwrap_or(Value::Null)),
            Err(error) => daemon_error(&error),
        }
    }

    async fn diff(&self, id: &str) -> Response {
        match self
            .client
            .read_diff(&session_id(id), SessionDiffBase::Head)
            .await
        {
            Ok(result) => Response::json(&json!({
                "patch": String::from_utf8_lossy(&result.patch),
                "repoRoot": result.repo_root,
                "truncated": result.truncated,
            })),
            Err(error) => daemon_error(&error),
        }
    }

    async fn send(&self, id: &str, request: &Request) -> Response {
        let body = request.json();
        let Some(text) = body.get("text").and_then(Value::as_str) else {
            return Response::error(400, "missing text");
        };
        let submit = body.get("submit").and_then(Value::as_bool).unwrap_or(true);
        match self
            .client
            .send_text(&session_id(id), text.to_string(), submit)
            .await
        {
            Ok(()) => Response::json(&json!({ "ok": true })),
            Err(error) => daemon_error(&error),
        }
    }

    async fn key(&self, id: &str, request: &Request) -> Response {
        let body = request.json();
        let Some(name) = body.get("key").and_then(Value::as_str) else {
            return Response::error(400, "missing key");
        };
        let Some(sequence) = key_sequence(name) else {
            return Response::error(400, "unknown key");
        };
        // `submit: false` writes the bytes verbatim; `true` would frame them
        // as a bracketed paste and append its own Enter, which would turn
        // every one of these into a mangled prompt.
        match self
            .client
            .send_text(&session_id(id), sequence.to_string(), false)
            .await
        {
            Ok(()) => Response::json(&json!({ "ok": true })),
            Err(error) => daemon_error(&error),
        }
    }

    async fn resize(&self, id: &str, request: &Request) -> Response {
        let body = request.json();
        let cols = body.get("cols").and_then(Value::as_i64).unwrap_or(0);
        let rows = body.get("rows").and_then(Value::as_i64).unwrap_or(0);
        if cols < 20 || rows < 8 {
            return Response::error(400, "implausible terminal size");
        }
        match self.client.resize(&session_id(id), cols, rows).await {
            Ok(()) => Response::json(&json!({ "ok": true })),
            Err(error) => daemon_error(&error),
        }
    }

    async fn seen(&self, id: &str) -> Response {
        match self.client.mark_seen(&session_id(id)).await {
            Ok(()) => Response::json(&json!({ "ok": true })),
            Err(error) => daemon_error(&error),
        }
    }

    async fn kill(&self, id: &str) -> Response {
        match self.client.kill(&session_id(id)).await {
            Ok(()) => Response::json(&json!({ "ok": true })),
            Err(error) => daemon_error(&error),
        }
    }

    async fn spawn(&self, request: &Request) -> Response {
        let body = request.json();
        let Some(cwd) = body.get("cwd").and_then(Value::as_str) else {
            return Response::error(400, "missing cwd");
        };
        let kind = body
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or(AgentKind::CLAUDE_CODE_ID);
        let prompt = body
            .get("prompt")
            .and_then(Value::as_str)
            .filter(|text| !text.trim().is_empty())
            .map(str::to_string);

        let params = SessionSpawnParams {
            kind: AgentKind::new(kind),
            cwd: cwd.to_string(),
            new_worktree: body.get("worktree").and_then(Value::as_bool),
            worktree_branch: body
                .get("branch")
                .and_then(Value::as_str)
                .map(str::to_string),
            title: body
                .get("title")
                .and_then(Value::as_str)
                .map(str::to_string),
            initial_prompt: prompt,
            parent: None,
            // A phone is a narrow viewport, but the session outlives the
            // phone. Give it a desktop-shaped grid so the transcript is not
            // permanently reflowed to 40 columns by the device that started
            // it; the frontend soft-wraps instead.
            initial_cols: Some(120),
            initial_rows: Some(32),
            host: body.get("host").and_then(Value::as_str).map(str::to_string),
            same_repo_as: None,
        };

        match self
            .client
            .request(
                diri_proto::Method::SESSION_SPAWN,
                Some(&params),
                Some(CALL_TIMEOUT),
            )
            .await
        {
            Ok(value) => Response::json(&value),
            Err(error) => daemon_error(&error),
        }
    }
}

fn session_id(raw: &str) -> SessionId {
    SessionId(raw.to_string())
}

/// A daemon that is down is a `502`: the frontend is fine, the thing behind it
/// is not, and the phone should say so rather than show an empty list.
fn daemon_error(error: &ClientError) -> Response {
    let status = match error {
        ClientError::Disconnected(_) | ClientError::Timeout(_) => 502,
        _ => 400,
    };
    Response::error(status, &error.to_string())
}

/// The raw bytes behind each button in the frontend's key row.
///
/// These go to the PTY verbatim, so they are exactly what a terminal emulator
/// would send — not what a TUI library happens to accept today.
fn key_sequence(name: &str) -> Option<&'static str> {
    Some(match name {
        "enter" => "\r",
        "escape" => "\x1b",
        "tab" => "\t",
        "shift-tab" => "\x1b[Z",
        "backspace" => "\x7f",
        "up" => "\x1b[A",
        "down" => "\x1b[B",
        "left" => "\x1b[D",
        "right" => "\x1b[C",
        "page-up" => "\x1b[5~",
        "page-down" => "\x1b[6~",
        "ctrl-c" => "\x03",
        "ctrl-d" => "\x04",
        "ctrl-u" => "\x15",
        "ctrl-r" => "\x12",
        // Option+Enter: the newline-without-submitting chord these TUIs use.
        "alt-enter" => "\x1b\r",
        "yes" => "y",
        "no" => "n",
        // Numbered answers to a permission prompt. Enumerated rather than
        // "any single printable character" so this stays an allowlist.
        "digit-1" => "1",
        "digit-2" => "2",
        "digit-3" => "3",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_key_the_frontend_offers_has_a_sequence() {
        // Kept in lockstep with the button row in `ui/index.html`.
        for name in [
            "enter",
            "escape",
            "tab",
            "shift-tab",
            "backspace",
            "up",
            "down",
            "left",
            "right",
            "page-up",
            "page-down",
            "ctrl-c",
            "ctrl-d",
            "ctrl-u",
            "ctrl-r",
            "alt-enter",
            "yes",
            "no",
            "digit-1",
            "digit-2",
            "digit-3",
        ] {
            assert!(key_sequence(name).is_some(), "{name} has no sequence");
        }
    }

    #[test]
    fn an_unknown_key_is_refused_rather_than_guessed() {
        assert!(key_sequence("self-destruct").is_none());
        assert!(key_sequence("").is_none());
    }

    #[test]
    fn arrow_keys_are_the_sequences_a_real_terminal_sends() {
        assert_eq!(key_sequence("up"), Some("\x1b[A"));
        assert_eq!(key_sequence("ctrl-c"), Some("\x03"));
        assert_eq!(key_sequence("alt-enter"), Some("\x1b\r"));
    }
}
