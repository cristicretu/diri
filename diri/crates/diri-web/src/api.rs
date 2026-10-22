//! The JSON surface the phone frontend drives.
//!
//! Every handler is a thin translation of one `DaemonClient` call. Nothing
//! here holds state: the daemon is the only source of truth, so a phone that
//! reconnects on a new cell tower sees the same world it left.

use std::sync::Arc;
use std::time::Duration;

use diri_client::{ClientError, DaemonClient};
use diri_proto::voice::{
    LatestSpokenParams, LatestSpokenResult, PhoneReaderParams, PhoneReaderResult,
};
use diri_proto::{
    AgentKind, ReadTranscriptParams, ReadTranscriptResult, SessionDiffBase, SessionId,
    SessionSpawnParams,
};
use serde_json::{Value, json};

use crate::auth::{self, Auth};
use crate::http::{Request, Response};
use crate::input_ledger::InputLedger;
use crate::turns::TurnWatcher;

/// Long enough for a cold `git diff` on a large repo, short enough that a
/// wedged daemon shows up as an error on the phone instead of a spinner.
const CALL_TIMEOUT: Duration = Duration::from_secs(20);
/// A summary lookup sits in the event stream; it must not stall it.
const SPOKEN_TIMEOUT: Duration = Duration::from_secs(2);
const DEFAULT_TRANSCRIPT_TURNS: u32 = 20;
const MAX_TRANSCRIPT_TURNS: u32 = 100;

pub struct Api {
    pub client: Arc<DaemonClient>,
    pub auth: Auth,
    pub host_label: String,
    pub inputs: InputLedger,
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
            ("GET", ["api", "agents"]) => self.agents(request).await,
            ("GET", ["api", "hosts"]) => match self
                .client
                .request::<()>(diri_proto::Method::HOST_LIST, None, Some(CALL_TIMEOUT))
                .await
            {
                Ok(value) => Response::json(&value),
                Err(error) => daemon_error(&error),
            },
            ("GET", ["api", "directories"]) => match self
                .client
                .list_directories(
                    request.query.get("host").cloned(),
                    request
                        .query
                        .get("path")
                        .cloned()
                        .unwrap_or_else(|| "~".into()),
                )
                .await
            {
                Ok(value) => Response::json(&serde_json::to_value(value).unwrap_or(Value::Null)),
                Err(error) => daemon_error(&error),
            },
            ("POST", ["api", "spawn"]) => self.spawn(request).await,
            ("GET", ["api", "voice"]) => self.phone_reader(None).await,
            ("POST", ["api", "voice"]) => {
                match request.json().get("readAloud").and_then(Value::as_bool) {
                    Some(read_aloud) => self.phone_reader(Some(read_aloud)).await,
                    None => Response::error(400, "missing readAloud"),
                }
            }

            ("GET", ["api", "session", id, "screen"]) => self.screen(id).await,
            ("GET", ["api", "session", id, "grid"]) => self.grid(id).await,
            ("GET", ["api", "session", id, "history"]) => self.history(id, request).await,
            ("POST", ["api", "session", id, "input"]) => self.input(id, request).await,
            ("GET", ["api", "session", id, "scrollback"]) => self.scrollback(id).await,
            ("GET", ["api", "session", id, "diff"]) => self.diff(id).await,
            ("GET", ["api", "session", id, "transcript"]) => self.transcript(id, request).await,
            ("GET", ["api", "session", id, "spoken"]) => self.spoken(id).await,
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

    async fn agents(&self, request: &Request) -> Response {
        // This frontend always serves the daemon it is attached to, so the
        // catalog is this host's. `force_refresh` stays false: the phone asks
        // on every sheet open, and re-probing PATH each time would make the
        // picker slow for a list that changes about once a month.
        let params = diri_proto::AgentReadinessParams {
            host: request.query.get("host").cloned(),
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

    /// The styled visible grid, read from a momentary read-only preview.
    async fn grid(&self, id: &str) -> Response {
        match crate::terminal::open(&self.client, &session_id(id)).await {
            Ok((mut preview, mut mirror)) => {
                preview.close().await;
                Response::json(&mirror.snapshot())
            }
            Err(error) => open_error(&error),
        }
    }

    async fn history(&self, id: &str, request: &Request) -> Response {
        let number = |name: &str| match request.query.get(name) {
            None => Ok(None),
            Some(raw) => raw.parse::<i64>().map(Some).map_err(|_| name.to_owned()),
        };
        let (before, limit) = match (number("before"), number("limit")) {
            (Ok(before), Ok(limit)) => (before, limit),
            (Err(name), _) | (_, Err(name)) => {
                return Response::error(400, &format!("{name} must be a number"));
            }
        };
        let limit = limit
            .unwrap_or(crate::terminal::DEFAULT_HISTORY_PAGE)
            .clamp(1, crate::terminal::MAX_HISTORY_PAGE);
        match crate::terminal::history(&self.client, &session_id(id), before, limit).await {
            Ok(page) => Response::json(&page),
            Err(error) => daemon_error(&error),
        }
    }

    /// Raw keyboard bytes for the PTY, written unframed. Never logged.
    async fn input(&self, id: &str, request: &Request) -> Response {
        use base64::Engine as _;
        let body = request.json();
        let Some(encoded) = body.get("data").and_then(Value::as_str) else {
            return Response::error(400, "missing data");
        };
        let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(encoded) else {
            return Response::error(400, "data must be base64");
        };
        if bytes.is_empty() {
            return Response::json(&json!({ "ok": true }));
        }
        // The Engine's text path carries a string; every key a phone keyboard
        // produces (text, controls, escape sequences) is UTF-8.
        let Ok(text) = String::from_utf8(bytes) else {
            return Response::error(400, "data must be UTF-8 terminal input");
        };
        let session = session_id(id);
        // `id` lets the phone resend a batch whose reply it lost; `once`
        // tells it this gateway is safe to resend to.
        let written = match body.get("id").and_then(Value::as_str) {
            Some(batch) => {
                // A timed-out write may still have landed, so its id counts
                // as delivered: a resend is never written twice.
                let mut timed_out = None;
                let delivered = self
                    .inputs
                    .deliver(&format!("{id}/{batch}"), || async {
                        match self.client.send_text(&session, text, false).await {
                            Err(error @ ClientError::Timeout(_)) => {
                                timed_out = Some(error);
                                Ok(())
                            }
                            other => other,
                        }
                    })
                    .await;
                match timed_out {
                    Some(error) => Err(error),
                    None => delivered,
                }
            }
            None => self.client.send_text(&session, text, false).await,
        };
        match written {
            Ok(()) => Response::json(&json!({ "ok": true, "once": true })),
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

    /// Text turns oldest-first, for a voice client. Never logged: they are
    /// the developer's prompts and the Agent's replies.
    async fn transcript(&self, id: &str, request: &Request) -> Response {
        let limit = match request.query.get("limit") {
            None => DEFAULT_TRANSCRIPT_TURNS,
            Some(raw) => match raw.parse::<u32>() {
                Ok(limit) => limit.clamp(1, MAX_TRANSCRIPT_TURNS),
                Err(_) => return Response::error(400, "limit must be a number"),
            },
        };
        let params = ReadTranscriptParams {
            session_id: session_id(id),
            turns: Some(limit),
        };
        match self
            .client
            .request(
                diri_proto::Method::SESSION_READ_TRANSCRIPT,
                Some(&params),
                Some(CALL_TIMEOUT),
            )
            .await
            .and_then(|value| {
                serde_json::from_value::<ReadTranscriptResult>(value)
                    .map_err(|error| ClientError::Json(error.to_string()))
            }) {
            Ok(result) => Response::json(&json!({
                "available": result.available,
                "turns": result.turns.iter().map(|turn| json!({
                    "role": turn.role,
                    "text": turn.text,
                    "turn": turn.turn,
                })).collect::<Vec<_>>(),
            })),
            Err(error) => daemon_error(&error),
        }
    }

    /// The session's newest spoken summary, whether or not the Mac read it.
    async fn spoken(&self, id: &str) -> Response {
        match self.latest_spoken(id).await {
            Ok(latest) => Response::json(&json!({ "turn": latest.turn, "text": latest.text })),
            Err(error) => daemon_error(&error),
        }
    }

    async fn latest_spoken(&self, id: &str) -> Result<LatestSpokenResult, ClientError> {
        let params = LatestSpokenParams {
            session_id: id.to_string(),
        };
        let value = self
            .client
            .request(
                diri_proto::Method::VOICE_LATEST_SPOKEN,
                Some(&params),
                Some(SPOKEN_TIMEOUT),
            )
            .await?;
        serde_json::from_value(value).map_err(|error| ClientError::Json(error.to_string()))
    }

    /// Reads, or sets, whether a phone wants every session's spoken summary.
    async fn phone_reader(&self, read_aloud: Option<bool>) -> Response {
        let params = PhoneReaderParams { read_aloud };
        match self
            .client
            .request(
                diri_proto::Method::VOICE_PHONE_READER,
                Some(&params),
                Some(CALL_TIMEOUT),
            )
            .await
            .and_then(|value| {
                serde_json::from_value::<PhoneReaderResult>(value)
                    .map_err(|error| ClientError::Json(error.to_string()))
            }) {
            Ok(result) => Response::json(&json!({ "readAloud": result.read_aloud })),
            Err(error) => daemon_error(&error),
        }
    }

    /// Baselines finished-turn detection on the sessions as they are now. A
    /// failure leaves the old baseline; the next update re-establishes it.
    pub async fn seed_turns(&self, turns: &mut TurnWatcher) {
        if let Ok(Ok(list)) = tokio::time::timeout(CALL_TIMEOUT, self.client.sessions()).await {
            turns.seed_records(list.sessions);
        }
    }

    /// The `turn_finished` payload, with the summary when the Agent gave one
    /// for this turn; otherwise the phone fetches the transcript.
    pub async fn turn_finished(&self, session: String, turn: u64) -> Value {
        let spoken = self
            .latest_spoken(&session)
            .await
            .ok()
            .filter(|latest| latest.turn == Some(turn))
            .and_then(|latest| latest.text);
        json!({ "sessionID": session, "turn": turn, "spoken": spoken })
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
        if !(20..=500).contains(&cols) || !(8..=300).contains(&rows) {
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
            appearance: None,
            kind: AgentKind::new(kind),
            cwd: cwd.to_string(),
            new_worktree: body.get("worktree").and_then(Value::as_bool),
            worktree_base: body.get("base").and_then(Value::as_str).map(str::to_owned),
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
            account_profile_id: None,
            same_repo_as: None,
            start_directory: None,
            note_id: None,
            argv: None,
        };

        match self
            .client
            .request(
                diri_proto::Method::SESSION_SPAWN,
                Some(&params),
                // Outlast the Engine's 180s first-prompt readiness window.
                Some(Duration::from_secs(300)),
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

pub fn open_error(error: &crate::terminal::OpenError) -> Response {
    match error {
        crate::terminal::OpenError::NotFound => {
            Response::error(404, "no terminal for this session")
        }
        crate::terminal::OpenError::Unavailable(reason) => Response::error(502, reason),
    }
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

    #[tokio::test]
    async fn phone_routes_preserve_engine_host_and_workspace_semantics() {
        use diri_proto::{ControlMessage, Method};
        use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
        let temp = tempfile::tempdir_in("/tmp").unwrap();
        let socket = temp.path().join("engine.sock");
        let listener = diri_platform::ipc::asynchronous::UnixListener::bind(&socket).unwrap();
        let (calls_tx, mut calls_rx) = tokio::sync::mpsc::channel(16);
        let engine = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (reader, mut writer) = stream.into_split();
            let mut lines = BufReader::new(reader).lines();
            while let Some(line) = lines.next_line().await.unwrap() {
                let ControlMessage::Request { id, method, params } =
                    serde_json::from_str(&line).unwrap()
                else {
                    continue;
                };
                let result = if method == Method::HELLO {
                    json!({"proto": diri_proto::WIRE_VERSION, "build":"test", "pid":1, "engineKind":diri_proto::RUST_ENGINE_KIND})
                } else {
                    calls_tx
                        .send((method.clone(), params.clone()))
                        .await
                        .unwrap();
                    match method.as_str() {
                        Method::HOST_LIST => {
                            json!({"hosts":[{"id":"remote-a","name":"Remote","defaultCwd":"/repo"}]})
                        }
                        Method::AGENT_READINESS => json!({"agents":[]}),
                        Method::HOST_LIST_DIRECTORIES => {
                            json!({"path":"/repo", "entries":[], "truncated":false})
                        }
                        Method::SESSION_SPAWN => params.unwrap(),
                        _ => json!({}),
                    }
                };
                let mut bytes = serde_json::to_vec(&ControlMessage::Response {
                    id,
                    result: Ok(result),
                })
                .unwrap();
                bytes.push(b'\n');
                writer.write_all(&bytes).await.unwrap();
            }
        });
        let client = Arc::new(DaemonClient::with_socket_path(socket));
        client.connect();
        client
            .wait_until_connected(Duration::from_secs(2))
            .await
            .unwrap();
        let api = Api {
            client,
            auth: Auth::temporary().unwrap(),
            host_label: "Mac".into(),
            inputs: Default::default(),
        };
        for (path, method, body, expected) in [
            ("/api/hosts", "GET", "", Method::HOST_LIST),
            (
                "/api/agents?host=remote-a",
                "GET",
                "",
                Method::AGENT_READINESS,
            ),
            (
                "/api/directories?host=remote-a&path=%2Frepo%20name",
                "GET",
                "",
                Method::HOST_LIST_DIRECTORIES,
            ),
            (
                "/api/spawn",
                "POST",
                r#"{"kind":"shell","cwd":"/repo","host":"remote-a","worktree":true,"base":"main","branch":"phone/fix"}"#,
                Method::SESSION_SPAWN,
            ),
        ] {
            let raw = format!(
                "{method} {path} HTTP/1.1\r\nHost: test\r\nX-Diri-Token: {}\r\nContent-Length: {}\r\n\r\n{body}",
                api.auth.token(),
                body.len()
            );
            let request = crate::http::read_request(&mut BufReader::new(raw.as_bytes()))
                .await
                .unwrap()
                .unwrap();
            let response = api.route(&request).await;
            assert_eq!(response.status, 200, "{path}");
            let (method, params) = calls_rx.recv().await.unwrap();
            assert_eq!(method, expected);
            if expected != Method::HOST_LIST {
                assert_eq!(params.as_ref().unwrap()["host"], "remote-a");
            }
            if expected == Method::HOST_LIST_DIRECTORIES {
                assert_eq!(params.unwrap()["path"], "/repo name");
            }
            if expected == Method::SESSION_SPAWN {
                let result: SessionSpawnParams = serde_json::from_slice(&response.body).unwrap();
                assert_eq!(result.worktree_base.as_deref(), Some("main"));
                assert_eq!(result.worktree_branch.as_deref(), Some("phone/fix"));
                assert_eq!(result.new_worktree, Some(true));
            }
        }
        for path in [
            "/api/hosts",
            "/api/directories",
            "/api/agents",
            "/api/spawn",
        ] {
            let raw = format!("POST {path} HTTP/1.1\r\nHost: test\r\n\r\n");
            let request = crate::http::read_request(&mut BufReader::new(raw.as_bytes()))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(api.route(&request).await.status, 401);
        }
        assert!(calls_rx.try_recv().is_err());
        api.client.begin_shutdown();
        engine.abort();
    }

    async fn call(api: &Api, method: &str, path: &str, body: &str) -> (u16, Value) {
        use tokio::io::BufReader;
        let raw = format!(
            "{method} {path} HTTP/1.1\r\nHost: test\r\nX-Diri-Token: {}\r\nContent-Length: {}\r\n\r\n{body}",
            api.auth.token(),
            body.len()
        );
        let request = crate::http::read_request(&mut BufReader::new(raw.as_bytes()))
            .await
            .unwrap()
            .unwrap();
        let response = api.route(&request).await;
        let body = serde_json::from_slice(&response.body).unwrap_or(Value::Null);
        (response.status, body)
    }

    #[tokio::test]
    async fn voice_routes_keep_the_phone_contract() {
        use diri_proto::Method;
        let mut engine = crate::testing::fake_engine(|method, params| match method {
            Method::SESSION_READ_TRANSCRIPT => json!({
                "available": true,
                "turns": [
                    {"role": "user", "text": "run the tests"},
                    {"role": "agent", "text": "All green.", "turn": 3},
                ],
            }),
            Method::VOICE_LATEST_SPOKEN => json!({"turn": 3, "text": "All green."}),
            Method::VOICE_PHONE_READER => json!({
                "readAloud": params
                    .and_then(|params| params.get("readAloud"))
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            }),
            _ => json!({}),
        })
        .await;
        let api = Api {
            client: Arc::clone(&engine.client),
            auth: Auth::temporary().unwrap(),
            host_label: "Mac".into(),
            inputs: Default::default(),
        };

        let (status, body) = call(&api, "GET", "/api/session/s_a/transcript", "").await;
        assert_eq!(status, 200);
        assert_eq!(
            body["turns"],
            json!([
                {"role": "user", "text": "run the tests", "turn": null},
                {"role": "agent", "text": "All green.", "turn": 3},
            ])
        );
        let (method, params) = engine.calls.recv().await.unwrap();
        assert_eq!(method, Method::SESSION_READ_TRANSCRIPT);
        assert_eq!(params.unwrap(), json!({"sessionID": "s_a", "turns": 20}));

        for (limit, sent) in [("500", 100), ("0", 1), ("7", 7)] {
            let path = format!("/api/session/s_a/transcript?limit={limit}");
            assert_eq!(call(&api, "GET", &path, "").await.0, 200);
            let (_, params) = engine.calls.recv().await.unwrap();
            assert_eq!(params.unwrap()["turns"], sent);
        }
        assert_eq!(
            call(&api, "GET", "/api/session/s_a/transcript?limit=all", "")
                .await
                .0,
            400
        );

        let (status, body) = call(&api, "GET", "/api/session/s_a/spoken", "").await;
        assert_eq!(status, 200);
        assert_eq!(body, json!({"turn": 3, "text": "All green."}));
        let (method, params) = engine.calls.recv().await.unwrap();
        assert_eq!(method, Method::VOICE_LATEST_SPOKEN);
        assert_eq!(params.unwrap(), json!({"sessionID": "s_a"}));

        assert_eq!(
            call(&api, "GET", "/api/voice", "").await,
            (200, json!({"readAloud": false}))
        );
        let (_, params) = engine.calls.recv().await.unwrap();
        assert_eq!(params.unwrap(), json!({}), "a read changes nothing");
        assert_eq!(
            call(&api, "POST", "/api/voice", r#"{"readAloud":true}"#).await,
            (200, json!({"readAloud": true}))
        );
        let (method, params) = engine.calls.recv().await.unwrap();
        assert_eq!(method, Method::VOICE_PHONE_READER);
        assert_eq!(params.unwrap(), json!({"readAloud": true}));
        assert_eq!(call(&api, "POST", "/api/voice", "{}").await.0, 400);

        // The summary rides along only for the turn it was given in.
        assert_eq!(
            api.turn_finished("s_a".into(), 3).await,
            json!({"sessionID": "s_a", "turn": 3, "spoken": "All green."})
        );
        assert_eq!(
            api.turn_finished("s_a".into(), 4).await,
            json!({"sessionID": "s_a", "turn": 4, "spoken": null})
        );
    }

    /// A fake scrollback of `history` rows above a 4-row screen, where row
    /// `n` reads "row n". Answers exactly the window it was asked for.
    fn scrollback(params: Option<&Value>, history: i64) -> Value {
        use diri_proto::grid::{GridCell, GridRowCodec, TermColor, TermStyle};
        let params = params.unwrap();
        let total = history + 4;
        let first = params["firstRow"].as_i64().unwrap().clamp(0, total);
        let end = (first + params["maxRows"].as_i64().unwrap()).min(total);
        let rows: Vec<Vec<GridCell>> = (first..end)
            .map(|n| {
                format!("row {n}")
                    .chars()
                    .map(|ch| {
                        GridCell::new(
                            ch as u32,
                            TermColor::Default,
                            TermColor::DefaultInverted,
                            TermStyle::empty(),
                        )
                    })
                    .collect()
            })
            .collect();
        let payload = GridRowCodec::encode_rows(&rows).unwrap();
        let mut result = serde_json::to_value(diri_proto::ReadScrollbackCellsResult {
            metadata: Vec::new(),
            payload,
            first_row: first,
            row_count: end - first,
            total_rows: total,
            live_start_row: history,
            cols: 10,
            content_seq: 1,
        })
        .unwrap();
        result["cols"] = json!(10);
        result
    }

    #[tokio::test]
    async fn history_pages_are_bounded_oldest_first_and_never_visible_rows() {
        use diri_proto::Method;
        let mut engine = crate::testing::fake_engine(|method, params| match method {
            Method::SESSION_READ_SCROLLBACK_CELLS => scrollback(params, 2500),
            _ => json!({}),
        })
        .await;
        let api = Api {
            client: Arc::clone(&engine.client),
            auth: Auth::temporary().unwrap(),
            host_label: "Mac".into(),
            inputs: Default::default(),
        };
        let text = |line: &Value| line["runs"][0]["t"].as_str().unwrap().to_owned();

        // No `before`: the newest 200 history rows, ending at the screen.
        let (status, page) = call(&api, "GET", "/api/session/s_a/history", "").await;
        assert_eq!(status, 200);
        assert_eq!(page["historySize"], 2500);
        assert_eq!(page["start"], 2300);
        let lines = page["lines"].as_array().unwrap();
        assert_eq!(lines.len(), 200);
        assert_eq!(text(&lines[0]), "row 2300");
        assert_eq!(text(&lines[199]), "row 2499");
        while engine.calls.try_recv().is_ok() {}

        // `before` is exclusive; the page runs up to it.
        let (_, page) = call(
            &api,
            "GET",
            "/api/session/s_a/history?before=50&limit=10",
            "",
        )
        .await;
        assert_eq!(page["start"], 40);
        let lines = page["lines"].as_array().unwrap();
        assert_eq!((text(&lines[0]), lines.len()), ("row 40".into(), 10));
        let (_, params) = engine.calls.recv().await.unwrap();
        assert_eq!(params.unwrap()["maxRows"], 10, "one read, exactly the page");

        // The page size is capped, and the start of history clamps it.
        let (_, page) = call(
            &api,
            "GET",
            "/api/session/s_a/history?before=2400&limit=5000",
            "",
        )
        .await;
        assert_eq!(page["lines"].as_array().unwrap().len(), 1000);
        assert_eq!(page["start"], 1400);
        let (_, page) = call(
            &api,
            "GET",
            "/api/session/s_a/history?before=3&limit=10",
            "",
        )
        .await;
        assert_eq!(
            (
                page["start"].clone(),
                page["lines"].as_array().unwrap().len()
            ),
            (json!(0), 3)
        );
        let (_, page) = call(&api, "GET", "/api/session/s_a/history?before=0", "").await;
        assert_eq!(page["lines"], json!([]));

        // Past the end of history: visible rows are never served as history.
        let (_, page) = call(
            &api,
            "GET",
            "/api/session/s_a/history?before=2503&limit=10",
            "",
        )
        .await;
        let lines = page["lines"].as_array().unwrap();
        assert_eq!(page["start"], 2493);
        assert_eq!(lines.len(), 7);
        assert_eq!(text(&lines[6]), "row 2499");

        for bad in ["before=x", "limit=-"] {
            let path = format!("/api/session/s_a/history?{bad}");
            assert_eq!(call(&api, "GET", &path, "").await.0, 400, "{bad}");
        }
    }

    #[tokio::test]
    async fn input_writes_the_decoded_bytes_unframed() {
        use diri_proto::Method;
        let mut engine = crate::testing::fake_engine(|_, _| json!({})).await;
        let api = Api {
            client: Arc::clone(&engine.client),
            auth: Auth::temporary().unwrap(),
            host_label: "Mac".into(),
            inputs: Default::default(),
        };
        // Ctrl-C, Up, Escape and a paste: exactly these bytes, no Enter, no
        // bracketing.
        let bytes = b"\x03\x1b[A\x1bpasted text \xe2\x9c\x93";
        let body = json!({ "data": base64_encode(bytes) }).to_string();
        let (status, _) = call(&api, "POST", "/api/session/s_a/input", &body).await;
        assert_eq!(status, 200);
        let (method, params) = engine.calls.recv().await.unwrap();
        assert_eq!(method, Method::SESSION_SEND_TEXT);
        let params = params.unwrap();
        assert_eq!(params["submit"], false);
        assert_eq!(params["text"].as_str().unwrap().as_bytes(), bytes);

        for body in [r#"{}"#, r#"{"data":"%%%"}"#, r#"{"data":"/w=="}"#] {
            assert_eq!(
                call(&api, "POST", "/api/session/s_a/input", body).await.0,
                400,
                "{body}"
            );
        }
        assert!(
            engine.calls.try_recv().is_err(),
            "nothing invalid was written"
        );
    }

    #[tokio::test]
    async fn a_resent_input_batch_is_written_once() {
        let mut engine = crate::testing::fake_engine(|_, _| json!({})).await;
        let api = Api {
            client: Arc::clone(&engine.client),
            auth: Auth::temporary().unwrap(),
            host_label: "Mac".into(),
            inputs: Default::default(),
        };
        let batch = json!({ "data": base64_encode(b"deploy\r"), "id": "b1" }).to_string();
        for _ in 0..2 {
            let (status, body) = call(&api, "POST", "/api/session/s_a/input", &batch).await;
            assert_eq!(status, 200);
            assert_eq!(body["once"], true);
        }
        assert!(engine.calls.recv().await.is_some());
        assert!(
            engine.calls.try_recv().is_err(),
            "the resend was not written"
        );

        // The same batch id on another session is a different batch.
        let (status, _) = call(&api, "POST", "/api/session/s_b/input", &batch).await;
        assert_eq!(status, 200);
        assert!(engine.calls.recv().await.is_some());
    }

    fn base64_encode(bytes: &[u8]) -> String {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    #[tokio::test]
    async fn every_terminal_route_requires_the_credential() {
        use tokio::io::BufReader;
        let mut engine = crate::testing::fake_engine(|_, _| json!({})).await;
        let api = Api {
            client: Arc::clone(&engine.client),
            auth: Auth::temporary().unwrap(),
            host_label: "Mac".into(),
            inputs: Default::default(),
        };
        for (method, path, body) in [
            ("GET", "/api/session/s_a/grid", ""),
            ("GET", "/api/session/s_a/history?before=10", ""),
            ("POST", "/api/session/s_a/input", r#"{"data":"aGk="}"#),
        ] {
            let raw = format!(
                "{method} {path} HTTP/1.1\r\nHost: test\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
            let request = crate::http::read_request(&mut BufReader::new(raw.as_bytes()))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(api.route(&request).await.status, 401, "{path}");
        }
        assert!(engine.calls.try_recv().is_err());
        assert!(engine.previews.try_recv().is_err());
    }

    #[tokio::test]
    async fn the_grid_is_one_styled_snapshot_from_a_preview() {
        use diri_proto::Method;
        use tokio::io::AsyncWriteExt as _;
        let mut engine = crate::testing::fake_engine(|method, params| match method {
            Method::SESSION_READ_SCROLLBACK_CELLS => scrollback(params, 7),
            _ => json!({}),
        })
        .await;
        let api = Arc::new(Api {
            client: Arc::clone(&engine.client),
            auth: Auth::temporary().unwrap(),
            host_label: "Mac".into(),
            inputs: Default::default(),
        });
        let request = tokio::spawn({
            let api = Arc::clone(&api);
            async move { call(&api, "GET", "/api/session/s_a/grid", "").await }
        });
        let (_, mut preview) = engine.previews.recv().await.unwrap();
        let update = diri_proto::grid::GridUpdate {
            cols: 3,
            rows: 2,
            cursor_col: 0,
            cursor_row: 1,
            cursor_visible: false,
            is_full_snapshot: true,
            changed_rows: vec![diri_proto::grid::ChangedRow::new(
                0,
                vec![
                    diri_proto::grid::GridCell::new(
                        'x' as u32,
                        diri_proto::grid::TermColor::Rgb(255, 0, 16),
                        diri_proto::grid::TermColor::Ansi(200),
                        diri_proto::grid::TermStyle::UNDERLINE,
                    );
                    3
                ],
            )],
        };
        use diri_proto::frames::{Frame, FrameCodec};
        preview
            .write_all(&FrameCodec::encode(&Frame::grid(&update).unwrap()).unwrap())
            .await
            .unwrap();
        preview
            .write_all(
                &FrameCodec::encode(&Frame::modes(
                    false,
                    diri_proto::terminal::MouseModes {
                        tracking: diri_proto::terminal::MouseTrackingMode::ButtonEvents,
                        encoding: diri_proto::terminal::MouseEncoding::Sgr,
                    },
                ))
                .unwrap(),
            )
            .await
            .unwrap();
        let (status, grid) = request.await.unwrap();
        assert_eq!(status, 200);
        assert_eq!(
            (grid["cols"].clone(), grid["rows"].clone()),
            (json!(3), json!(2))
        );
        assert_eq!(grid["historySize"], 7);
        assert_eq!(grid["cursor"]["visible"], false);
        assert_eq!(grid["modes"]["mouse"], true);
        assert_eq!(
            grid["lines"],
            json!([
                {"runs": [{"t": "xxx", "fg": "#ff0010", "bg": 200, "a": 8}], "wrapped": false},
                {"runs": [], "wrapped": false},
            ])
        );
        let palette = &grid["palette"];
        assert!(palette["fg"].as_str().unwrap().starts_with('#'));
        assert_eq!(palette["ansi"][1], "#cd3131");
    }

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
