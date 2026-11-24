//! Structured Agent conversations: the Engine owns the Agent child process,
//! feeds each stdout line through `diri-proto`'s codecs, and hands the GUI
//! sequenced events. Nothing is spawned until the first turn is sent, so a
//! fresh conversation and any model change before turn 1 cost no process.
//!
//! Claude Code runs as `claude -p --input-format stream-json
//! --output-format stream-json`, one process per native conversation. Codex
//! runs as `codex app-server` with one JSON-RPC thread. Antigravity has no
//! driver yet and fails closed with `StructuredError::Unsupported`.
use std::collections::{HashSet, VecDeque};
use std::ffi::OsString;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TryRecvError};
use std::thread;
use std::time::Duration;

use diri_proto::structured::{
    AcpInbound, AgentModel, CodexInbound, CodexModel, StructuredEvent, acp_prompt_params,
    acp_usage, claude_initialize_line, claude_permission_allow_line, claude_permission_deny_line,
    claude_user_message_line, codex_approval_line, codex_notification_line,
    codex_permission_request, codex_request_line, codex_thread_id, codex_turn_start_params,
    parse_acp_line, parse_acp_models, parse_claude_line, parse_codex_line, parse_codex_models,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::inject::uuid_v4;

/// Frames buffered between the reader thread and the Engine. Bounded so a
/// stalled consumer applies back-pressure to the child pipe, not memory.
const FRAME_QUEUE: usize = 1024;
/// Codex handshake replies come back in milliseconds; this bounds a hang.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(20);
/// Largest stdout line the Engine keeps. A longer line (a huge tool output)
/// is dropped with a notice and never buffered whole.
pub const MAX_LINE_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StructuredHarness {
    Claude,
    Codex,
    Antigravity,
}

#[derive(Debug)]
pub enum StructuredError {
    /// The harness has no structured driver in this build.
    Unsupported {
        harness: StructuredHarness,
        reason: String,
    },
    Spawn(String),
    Protocol(String),
    Handshake(String),
}

impl std::fmt::Display for StructuredError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unsupported { harness, reason } => {
                write!(f, "{harness:?} has no structured driver: {reason}")
            }
            Self::Spawn(message) => write!(f, "could not start the Agent: {message}"),
            Self::Protocol(message) => write!(f, "protocol error: {message}"),
            Self::Handshake(message) => write!(f, "Agent handshake failed: {message}"),
        }
    }
}

impl std::error::Error for StructuredError {}

/// One event with its position in this conversation's total order.
#[derive(Clone, Debug, PartialEq)]
pub struct SequencedEvent {
    pub sequence: u64,
    pub event: StructuredEvent,
}

/// What the reader thread forwards. Codex replies are routed to the
/// handshake or the turn, so they stay separate from GUI events.
enum Frame {
    Events(Vec<StructuredEvent>),
    Response {
        id: u64,
        result: Value,
    },
    Failure {
        id: Option<u64>,
        message: String,
    },
    ServerRequest {
        id: Value,
        method: String,
        params: Value,
    },
    Malformed(String),
    /// A line over `MAX_LINE_BYTES`; the turn continues.
    Dropped(String),
    Closed,
}

/// A prompt awaiting an answer, with what the protocol needs to reply.
#[derive(Clone, Debug, PartialEq)]
pub struct PendingPermission {
    pub request_id: String,
    pub tool: String,
    pub input: Value,
    pub reason: Option<String>,
    raw_id: Value,
    harness: StructuredHarness,
}

/// The user's answer to one prompt. `Deny` ends the tool call with `message`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PermissionDecision {
    Allow,
    AllowForSession,
    Deny,
}

struct Worker {
    child: Child,
    stdin: ChildStdin,
    frames: Receiver<Frame>,
    next_request_id: u64,
    /// Events read while waiting for a reply; delivered on the next poll.
    backlog: VecDeque<Frame>,
}

impl Drop for Worker {
    fn drop(&mut self) {
        // Closing stdin asks the Agent to finish; the kill is the backstop so
        // a detached conversation never leaves a process behind.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// One GUI conversation. A harness switch keeps the same value and records
/// a divider segment; it does not create a child session.
pub struct StructuredConversation {
    cwd: PathBuf,
    /// Overrides the CLI binary (tests substitute scripted fakes).
    program: Option<OsString>,
    harness: StructuredHarness,
    /// Model and effort the user chose. Updated only by the user, never by
    /// what the child reports at startup.
    model: Option<String>,
    effort: Option<String>,
    /// Claude session UUID or Codex thread id: the native conversation.
    native_id: Option<String>,
    reported_model: Option<String>,
    /// Set when Claude must restart with new flags (model or effort changed).
    respawn_needed: bool,
    worker: Option<Worker>,
    sequence: u64,
    seen_tools: HashSet<String>,
    turn_in_flight: bool,
    /// Models the Antigravity session offered at `session/new`.
    available_models: Vec<AgentModel>,
    /// The id of the in-flight `session/prompt`, whose reply ends the turn.
    prompt_request: Option<u64>,
    /// Prompts the Agent is waiting on, by request id.
    pending: std::collections::BTreeMap<String, PendingPermission>,
    /// Number of child processes started; the pre-turn-1 contract is 0.
    spawns: u32,
}

impl StructuredConversation {
    /// Creates the conversation without starting any process.
    pub fn new(harness: StructuredHarness, cwd: impl Into<PathBuf>) -> Self {
        Self {
            cwd: cwd.into(),
            program: None,
            harness,
            model: None,
            effort: None,
            native_id: None,
            reported_model: None,
            respawn_needed: false,
            worker: None,
            sequence: 0,
            seen_tools: HashSet::new(),
            turn_in_flight: false,
            available_models: Vec::new(),
            prompt_request: None,
            pending: Default::default(),
            spawns: 0,
        }
    }

    /// Runs `program` instead of the installed `claude` or `codex` binary.
    pub fn with_program(mut self, program: impl Into<OsString>) -> Self {
        self.program = Some(program.into());
        self
    }

    pub fn harness(&self) -> StructuredHarness {
        self.harness
    }

    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    pub fn effort(&self) -> Option<&str> {
        self.effort.as_deref()
    }

    /// The model the child reported at startup. Display only: the chosen
    /// model above always wins.
    pub fn reported_model(&self) -> Option<&str> {
        self.reported_model.as_deref()
    }

    pub fn native_id(&self) -> Option<&str> {
        self.native_id.as_deref()
    }

    pub fn spawn_count(&self) -> u32 {
        self.spawns
    }

    /// Models the Antigravity session offers; empty for other harnesses.
    pub fn available_models(&self) -> &[AgentModel] {
        &self.available_models
    }

    pub fn is_turn_in_flight(&self) -> bool {
        self.turn_in_flight
    }

    /// Prompts awaiting an answer, oldest id first.
    pub fn pending_permissions(&self) -> Vec<PendingPermission> {
        self.pending.values().cloned().collect()
    }

    /// Answers one prompt and resumes the Agent. Unknown ids are an error:
    /// the Agent is no longer waiting on them.
    pub fn answer_permission(
        &mut self,
        request_id: &str,
        decision: PermissionDecision,
        message: Option<&str>,
    ) -> Result<(), StructuredError> {
        let pending = self
            .pending
            .remove(request_id)
            .ok_or_else(|| StructuredError::Protocol(format!("no open prompt {request_id}")))?;
        let worker = self
            .worker
            .as_mut()
            .ok_or_else(|| StructuredError::Protocol("the Agent is not running".to_owned()))?;
        let line = match pending.harness {
            StructuredHarness::Claude => match decision {
                PermissionDecision::Deny => {
                    claude_permission_deny_line(request_id, message.unwrap_or("Denied in Diri"))
                }
                _ => claude_permission_allow_line(request_id, &pending.input),
            },
            _ => {
                let verdict = match decision {
                    PermissionDecision::Allow => "accept",
                    PermissionDecision::AllowForSession => "acceptForSession",
                    PermissionDecision::Deny => "decline",
                };
                codex_approval_line(&pending.raw_id, verdict)
            }
        };
        write_line(&mut worker.stdin, &line)
    }

    /// Ends the running Agent without forgetting the native conversation,
    /// so the next turn resumes it. Used to park for the PTY handoff.
    pub fn stop(&mut self) {
        self.worker = None;
        self.turn_in_flight = false;
        self.prompt_request = None;
        self.pending.clear();
    }

    /// Records a model/effort choice. Before turn 1 this is a field write.
    /// Mid-conversation, Claude restarts on the same UUID at the next turn;
    /// Codex passes the choice in its next `turn/start`.
    pub fn set_model(&mut self, model: Option<String>, effort: Option<String>) {
        if self.model != model || self.effort != effort {
            if self.harness == StructuredHarness::Claude && self.worker.is_some() {
                self.respawn_needed = true;
            }
            self.model = model;
            self.effort = effort;
        }
    }

    /// Switches agent inside this conversation. The native conversation of
    /// the previous harness is left behind; the caller inserts the divider.
    pub fn switch_harness(&mut self, harness: StructuredHarness) {
        if harness == self.harness {
            return;
        }
        self.worker = None;
        self.harness = harness;
        self.native_id = None;
        self.reported_model = None;
        self.respawn_needed = false;
        self.turn_in_flight = false;
        self.seen_tools.clear();
    }

    /// Sends one user turn, starting the Agent on first use.
    pub fn send_turn(&mut self, text: &str) -> Result<(), StructuredError> {
        self.ensure_supported()?;
        if self.turn_in_flight {
            return Err(StructuredError::Protocol(
                "a turn is already running; steering is not implemented".to_owned(),
            ));
        }
        if self.worker.is_none() || self.respawn_needed {
            self.worker = None;
            self.start_worker()?;
            self.respawn_needed = false;
        }
        let harness = self.harness;
        let effort = self.effort.clone();
        let model = self.model.clone();
        let worker = self.worker.as_mut().expect("worker started above");
        match harness {
            StructuredHarness::Claude => {
                write_line(&mut worker.stdin, &claude_user_message_line(text))?;
            }
            StructuredHarness::Codex => {
                let thread = self
                    .native_id
                    .clone()
                    .ok_or_else(|| StructuredError::Protocol("no Codex thread".to_owned()))?;
                let mut params = codex_turn_start_params(&thread, text, effort.as_deref());
                if let Some(model) = model {
                    params["model"] = Value::String(model);
                }
                let id = worker.next_request_id;
                worker.next_request_id += 1;
                write_line(
                    &mut worker.stdin,
                    &codex_request_line(id, "turn/start", params),
                )?;
            }
            StructuredHarness::Antigravity => {
                let session = self.native_id.clone().ok_or_else(|| {
                    StructuredError::Protocol("no Antigravity session".to_owned())
                })?;
                let id = worker.next_request_id;
                worker.next_request_id += 1;
                let params = acp_prompt_params(&session, text);
                write_line(
                    &mut worker.stdin,
                    &codex_request_line(id, "session/prompt", params),
                )?;
                self.prompt_request = Some(id);
            }
        }
        self.turn_in_flight = true;
        Ok(())
    }

    /// Returns everything the Agent produced since the last call, in order.
    /// Never blocks: reads only what the reader thread has already queued.
    pub fn poll(&mut self) -> Vec<SequencedEvent> {
        let mut out = Vec::new();
        loop {
            let frame = match self.worker.as_mut() {
                None => break,
                Some(worker) => match worker.backlog.pop_front() {
                    Some(frame) => frame,
                    None => match worker.frames.try_recv() {
                        Ok(frame) => frame,
                        Err(TryRecvError::Empty) => break,
                        Err(TryRecvError::Disconnected) => Frame::Closed,
                    },
                },
            };
            self.absorb(frame, &mut out);
        }
        out
    }

    fn absorb(&mut self, frame: Frame, out: &mut Vec<SequencedEvent>) {
        match frame {
            Frame::Events(events) => {
                for event in events {
                    if let Some(event) = self.accept(event) {
                        self.push(event, out);
                    }
                }
            }
            Frame::Response { id, result } => {
                // Antigravity ends a turn with its `session/prompt` reply;
                // Codex replies to `turn/start` carry nothing the GUI needs.
                if self.prompt_request == Some(id) {
                    self.prompt_request = None;
                    let stop = result
                        .get("stopReason")
                        .and_then(Value::as_str)
                        .unwrap_or("end_turn")
                        .to_owned();
                    let event = StructuredEvent::TurnCompleted {
                        is_error: stop == "refusal",
                        subtype: stop,
                        usage: acp_usage(&result),
                        cost_usd: None,
                    };
                    if let Some(event) = self.accept(event) {
                        self.push(event, out);
                    }
                }
            }
            Frame::Failure { message, .. } => {
                self.turn_in_flight = false;
                self.push(StructuredEvent::Error { message }, out);
            }
            Frame::ServerRequest { id, method, params } => {
                match codex_permission_request(&id, &method, &params) {
                    Some(event) => {
                        if let StructuredEvent::PermissionRequested {
                            request_id,
                            tool,
                            input,
                            reason,
                        } = &event
                        {
                            self.pending.insert(
                                request_id.clone(),
                                PendingPermission {
                                    request_id: request_id.clone(),
                                    tool: tool.clone(),
                                    input: input.clone(),
                                    reason: reason.clone(),
                                    raw_id: id.clone(),
                                    harness: StructuredHarness::Codex,
                                },
                            );
                        }
                        self.push(event, out);
                    }
                    None => {
                        // Refuse what we do not render, so the turn cannot stall.
                        if let Some(worker) = self.worker.as_mut() {
                            let refusal = json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "error": { "code": -32601, "message": "not supported by Diri" },
                            });
                            let _ = write_line(&mut worker.stdin, &refusal.to_string());
                        }
                        self.push(
                            StructuredEvent::Error {
                                message: format!(
                                    "the Agent asked for {method}; Diri cannot answer it yet"
                                ),
                            },
                            out,
                        );
                    }
                }
            }
            Frame::Dropped(message) => {
                self.push(StructuredEvent::Error { message }, out);
            }
            Frame::Malformed(message) => {
                self.turn_in_flight = false;
                self.push(
                    StructuredEvent::Error {
                        message: format!("protocol error: {message}"),
                    },
                    out,
                );
            }
            Frame::Closed => {
                self.worker = None;
                self.pending.clear();
                if self.turn_in_flight {
                    self.turn_in_flight = false;
                    self.push(
                        StructuredEvent::Error {
                            message: "the Agent process exited".to_owned(),
                        },
                        out,
                    );
                }
            }
        }
    }

    /// Applies engine-side state to one event; returns what the GUI sees.
    fn accept(&mut self, event: StructuredEvent) -> Option<StructuredEvent> {
        match &event {
            StructuredEvent::SessionStarted {
                conversation_id,
                model,
            } => {
                if self.native_id.is_none() && !conversation_id.is_empty() {
                    self.native_id = Some(conversation_id.clone());
                }
                self.reported_model = model.clone();
            }
            StructuredEvent::ToolStarted { id, .. } => {
                if !self.seen_tools.insert(id.clone()) {
                    return None;
                }
            }
            StructuredEvent::TurnCompleted { .. } => {
                self.turn_in_flight = false;
            }
            StructuredEvent::PermissionRequested {
                request_id,
                tool,
                input,
                reason,
            } => {
                self.pending.insert(
                    request_id.clone(),
                    PendingPermission {
                        request_id: request_id.clone(),
                        tool: tool.clone(),
                        input: input.clone(),
                        reason: reason.clone(),
                        raw_id: Value::String(request_id.clone()),
                        harness: StructuredHarness::Claude,
                    },
                );
            }
            _ => {}
        }
        Some(event)
    }

    fn push(&mut self, event: StructuredEvent, out: &mut Vec<SequencedEvent>) {
        self.sequence += 1;
        out.push(SequencedEvent {
            sequence: self.sequence,
            event,
        });
    }

    fn ensure_supported(&self) -> Result<(), StructuredError> {
        Ok(())
    }

    fn start_worker(&mut self) -> Result<(), StructuredError> {
        self.ensure_supported()?;
        self.spawns += 1;
        match self.harness {
            StructuredHarness::Claude => self.start_claude(),
            StructuredHarness::Codex => self.start_codex(),
            StructuredHarness::Antigravity => self.start_antigravity(),
        }
    }

    fn start_antigravity(&mut self) -> Result<(), StructuredError> {
        let program = self.program.clone().unwrap_or_else(|| "agy".into());
        let mut worker = spawn_child(
            program,
            &["acp".to_owned()],
            &self.cwd,
            StructuredHarness::Antigravity,
        )?;
        handshake_request(
            &mut worker,
            "initialize",
            json!({
                "protocolVersion": 1,
                "clientCapabilities": {},
            }),
        )?;
        let cwd = self.cwd.display().to_string();
        let result = handshake_request(
            &mut worker,
            "session/new",
            json!({
                "cwd": cwd,
                "mcpServers": [],
            }),
        )?;
        let session = result
            .get("sessionId")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                StructuredError::Handshake("session/new carried no session id".to_owned())
            })?;
        self.available_models = parse_acp_models(&result);
        self.native_id = Some(session.to_owned());
        self.worker = Some(worker);
        Ok(())
    }

    fn start_claude(&mut self) -> Result<(), StructuredError> {
        // A fresh conversation gets a Diri-chosen UUID up front, so `--resume`
        // on the next restart names the same native conversation.
        let (id, resume) = match &self.native_id {
            Some(id) => (id.clone(), true),
            None => {
                let id = uuid_v4();
                self.native_id = Some(id.clone());
                (id, false)
            }
        };
        let args = claude_args(&id, resume, self.model.as_deref(), self.effort.as_deref());
        let program = self.program.clone().unwrap_or_else(|| "claude".into());
        let mut worker = spawn_child(program, &args, &self.cwd, StructuredHarness::Claude)?;
        // Register for prompts before the first user line.
        write_line(&mut worker.stdin, &claude_initialize_line())?;
        self.worker = Some(worker);
        Ok(())
    }

    fn start_codex(&mut self) -> Result<(), StructuredError> {
        let program = self.program.clone().unwrap_or_else(|| "codex".into());
        let mut worker = spawn_child(
            program,
            &["app-server".to_owned()],
            &self.cwd,
            StructuredHarness::Codex,
        )?;
        let cwd = self.cwd.display().to_string();
        handshake_request(
            &mut worker,
            "initialize",
            json!({
                "clientInfo": { "name": "diri", "title": "Diri", "version": env!("CARGO_PKG_VERSION") },
                "capabilities": { "experimentalApi": true },
            }),
        )?;
        write_line(&mut worker.stdin, &codex_notification_line("initialized"))?;
        // Ask before anything outside the sandbox, and route the question to
        // the user, not the user's configured auto-reviewer.
        let common = json!({
            "cwd": cwd,
            "approvalPolicy": "untrusted",
            "approvalsReviewer": "user",
            "sandbox": "workspace-write",
        });
        let result = match &self.native_id {
            Some(thread) => {
                let mut params = common.clone();
                params["threadId"] = Value::String(thread.clone());
                handshake_request(&mut worker, "thread/resume", params)?
            }
            None => handshake_request(&mut worker, "thread/start", common)?,
        };
        let thread = codex_thread_id(&result).ok_or_else(|| {
            StructuredError::Handshake("thread reply carried no thread id".to_owned())
        })?;
        self.native_id = Some(thread);
        self.worker = Some(worker);
        Ok(())
    }
}

fn claude_args(id: &str, resume: bool, model: Option<&str>, effort: Option<&str>) -> Vec<String> {
    let mut args = vec![
        "-p".to_owned(),
        "--input-format".to_owned(),
        "stream-json".to_owned(),
        "--output-format".to_owned(),
        "stream-json".to_owned(),
        "--include-partial-messages".to_owned(),
        "--verbose".to_owned(),
        // Prompts go to this host over stdio; nothing is denied silently.
        "--permission-prompt-tool".to_owned(),
        "stdio".to_owned(),
        if resume { "--resume" } else { "--session-id" }.to_owned(),
        id.to_owned(),
    ];
    if let Some(model) = model {
        args.extend(["--model".to_owned(), model.to_owned()]);
    }
    if let Some(effort) = effort {
        args.extend(["--effort".to_owned(), effort.to_owned()]);
    }
    args
}

fn spawn_child(
    program: impl Into<OsString>,
    args: &[String],
    cwd: &Path,
    harness: StructuredHarness,
) -> Result<Worker, StructuredError> {
    let mut child = Command::new(program.into())
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| StructuredError::Spawn(error.to_string()))?;
    let stdin = child.stdin.take().expect("stdin was piped");
    let stdout = child.stdout.take().expect("stdout was piped");
    let (tx, frames) = mpsc::sync_channel(FRAME_QUEUE);
    thread::Builder::new()
        .name("diri-structured-reader".to_owned())
        .spawn(move || read_frames(harness, stdout, tx))
        .map_err(|error| StructuredError::Spawn(error.to_string()))?;
    Ok(Worker {
        child,
        stdin,
        frames,
        next_request_id: 1,
        backlog: VecDeque::new(),
    })
}

fn read_frames(
    harness: StructuredHarness,
    stdout: std::process::ChildStdout,
    tx: SyncSender<Frame>,
) {
    let mut reader = BufReader::new(stdout);
    let mut buffer = Vec::new();
    loop {
        let line = match read_bounded_line(&mut reader, &mut buffer) {
            Ok(LineRead::Eof) | Err(_) => {
                let _ = tx.send(Frame::Closed);
                return;
            }
            Ok(LineRead::Oversized) => {
                let _ = tx.send(Frame::Dropped(format!(
                    "the Agent wrote a line over {MAX_LINE_BYTES} bytes; it was dropped"
                )));
                continue;
            }
            Ok(LineRead::Line) => String::from_utf8_lossy(&buffer).trim_end().to_owned(),
        };
        let text = line.as_str();
        if text.is_empty() {
            continue;
        }
        let frame = match harness {
            StructuredHarness::Claude => match parse_claude_line(text) {
                Ok(events) => Frame::Events(events),
                Err(error) => Frame::Malformed(error.to_string()),
            },
            StructuredHarness::Codex => match parse_codex_line(text) {
                Ok(CodexInbound::Response { id, result }) => Frame::Response { id, result },
                Ok(CodexInbound::Failure { id, message }) => Frame::Failure { id, message },
                Ok(CodexInbound::Events(events)) => Frame::Events(events),
                Ok(CodexInbound::ServerRequest { id, method, params }) => {
                    Frame::ServerRequest { id, method, params }
                }
                Ok(CodexInbound::Ignored) => Frame::Events(Vec::new()),
                Err(error) => Frame::Malformed(error.to_string()),
            },
            StructuredHarness::Antigravity => match parse_acp_line(text) {
                Ok(AcpInbound::Response { id, result }) => Frame::Response { id, result },
                Ok(AcpInbound::Failure { id, message }) => Frame::Failure { id, message },
                Ok(AcpInbound::Events(events)) => Frame::Events(events),
                Ok(AcpInbound::ServerRequest { id, method, params }) => {
                    Frame::ServerRequest { id, method, params }
                }
                Ok(AcpInbound::Ignored) => Frame::Events(Vec::new()),
                Err(error) => Frame::Malformed(error.to_string()),
            },
        };
        if tx.send(frame).is_err() {
            return;
        }
    }
}

/// Lists Codex's models with a short-lived app-server: initialize, then
/// `model/list`. The process exits before returning; no conversation shares it.
pub fn probe_codex_models(program: OsString) -> Result<Vec<CodexModel>, StructuredError> {
    let mut worker = spawn_child(
        program,
        &["app-server".to_owned()],
        Path::new("."),
        StructuredHarness::Codex,
    )?;
    handshake_request(
        &mut worker,
        "initialize",
        json!({
            "clientInfo": { "name": "diri", "title": "Diri", "version": env!("CARGO_PKG_VERSION") },
            "capabilities": { "experimentalApi": true },
        }),
    )?;
    write_line(&mut worker.stdin, &codex_notification_line("initialized"))?;
    let result = handshake_request(&mut worker, "model/list", json!({}))?;
    Ok(parse_codex_models(&result))
}

/// Lists Antigravity's models from `session/new`. The probe session is
/// discarded; it is never prompted.
pub fn probe_antigravity_models(program: OsString) -> Result<Vec<AgentModel>, StructuredError> {
    let mut worker = spawn_child(
        program,
        &["acp".to_owned()],
        Path::new("."),
        StructuredHarness::Antigravity,
    )?;
    handshake_request(
        &mut worker,
        "initialize",
        json!({
            "protocolVersion": 1,
            "clientCapabilities": {},
        }),
    )?;
    let cwd = std::env::temp_dir().display().to_string();
    let result = handshake_request(
        &mut worker,
        "session/new",
        json!({ "cwd": cwd, "mcpServers": [] }),
    )?;
    Ok(parse_acp_models(&result))
}

enum LineRead {
    Line,
    Oversized,
    Eof,
}

/// Reads one line into `buffer`, holding at most `MAX_LINE_BYTES` of it. An
/// oversized line is consumed to its end and reported, never kept.
fn read_bounded_line(reader: &mut impl BufRead, buffer: &mut Vec<u8>) -> std::io::Result<LineRead> {
    buffer.clear();
    let limit = MAX_LINE_BYTES as u64 + 1;
    let read = (&mut *reader).take(limit).read_until(b'\n', buffer)?;
    if read == 0 {
        return Ok(LineRead::Eof);
    }
    if buffer.last() == Some(&b'\n') || buffer.len() <= MAX_LINE_BYTES {
        return Ok(LineRead::Line);
    }
    let mut sink = Vec::new();
    loop {
        sink.clear();
        let read = reader
            .by_ref()
            .take(64 * 1024)
            .read_until(b'\n', &mut sink)?;
        if read == 0 || sink.last() == Some(&b'\n') {
            break;
        }
    }
    buffer.clear();
    Ok(LineRead::Oversized)
}

/// Sends one JSON-RPC request and waits for its reply. Notifications that
/// arrive meanwhile are kept for the next poll, not dropped.
fn handshake_request(
    worker: &mut Worker,
    method: &str,
    params: Value,
) -> Result<Value, StructuredError> {
    let id = worker.next_request_id;
    worker.next_request_id += 1;
    write_line(&mut worker.stdin, &codex_request_line(id, method, params))?;
    loop {
        match worker.frames.recv_timeout(HANDSHAKE_TIMEOUT) {
            Ok(Frame::Response { id: reply, result }) if reply == id => return Ok(result),
            Ok(Frame::Failure {
                id: Some(reply),
                message,
            }) if reply == id => {
                return Err(StructuredError::Handshake(format!("{method}: {message}")));
            }
            Ok(Frame::Closed) => {
                return Err(StructuredError::Handshake(format!(
                    "{method}: Agent exited"
                )));
            }
            Ok(other) => worker.backlog.push_back(other),
            Err(RecvTimeoutError::Timeout) => {
                return Err(StructuredError::Handshake(format!("{method}: no reply")));
            }
            Err(RecvTimeoutError::Disconnected) => {
                return Err(StructuredError::Handshake(format!(
                    "{method}: reader ended"
                )));
            }
        }
    }
}

fn write_line(stdin: &mut ChildStdin, line: &str) -> Result<(), StructuredError> {
    stdin
        .write_all(line.as_bytes())
        .and_then(|()| stdin.write_all(b"\n"))
        .and_then(|()| stdin.flush())
        .map_err(|error| StructuredError::Protocol(format!("write to the Agent failed: {error}")))
}
