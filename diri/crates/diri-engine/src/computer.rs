//! Computer use behind `computer.act`: every Agent drives Mac apps through
//! one shared, lazily-launched Cua Driver.
//!
//! The Engine owns a single `cua-driver mcp` child (stdio JSON-RPC). On macOS
//! that process proxies to the CuaDriver.app daemon, which holds the
//! Accessibility and Screen Recording grants, so ten Agents cost one pipe,
//! not ten processes. Each Diri session gets its own named Cua session: its
//! own agent cursor, badged with the tab's name. Apps are leased per process:
//! a session that acts on a pid holds it until it goes quiet, so two Agents
//! never type into the same window. The child is recycled after a quiet
//! spell; the daemon's own sessions expire on their own.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};

/// No calls for this long → close the driver pipe. Matches the daemon's own
/// session expiry, so cursors and the pipe go away together.
const IDLE_TIMEOUT: Duration = Duration::from_secs(300);
/// One action's patience: `run_steps`/`run_script` batch several actions.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(20);
/// A lease outlives its last use by this long, so an Agent that pauses to
/// think keeps its app; then any other session may take it.
const LEASE_IDLE: Duration = Duration::from_secs(90);
/// Long edge of screenshots when the Agent does not ask for a size: what
/// vision models read at full fidelity, and well inside a control frame.
const DEFAULT_IMAGE_DIMENSION: u64 = 1568;
const MAX_IMAGE_DIMENSION: u64 = 2560;
/// Results are relayed on the control socket, one line of at most
/// `MAX_CONTROL_LINE_BYTES`; leave room for the envelope.
const MAX_RESULT_BYTES: usize = diri_proto::control::MAX_CONTROL_LINE_BYTES - 64 * 1024;
/// Cua caps badge labels at 28 characters.
const MAX_LABEL_CHARS: usize = 28;

/// Driver tools Diri does not hand to Agents: Diri owns each session's
/// lifecycle and cursor, and driver-wide configuration is the person's.
const HIDDEN_TOOLS: &[&str] = &[
    "start_session",
    "end_session",
    "escalate_session",
    "get_session_state",
    "set_agent_cursor_enabled",
    "set_agent_cursor_theme",
    "set_agent_cursor_motion",
    "set_config",
    "check_for_update",
    "install_extension",
    "install_ffmpeg",
    "replay_trajectory",
    "page",
];

/// Tools that only look. Everything else may change an app, so a `pid` it
/// names must be leased to the caller.
const READ_ONLY_TOOLS: &[&str] = &[
    "check_permissions",
    "get_accessibility_tree",
    "get_agent_cursor_state",
    "get_browser_state",
    "get_config",
    "get_cursor_position",
    "get_desktop_state",
    "get_recording_state",
    "get_screen_size",
    "get_session",
    "get_window_state",
    "health_report",
    "list_apps",
    "list_sessions",
    "list_windows",
    "parse_visual_regions",
    "verify_state",
    "zoom",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComputerError {
    pub code: &'static str,
    pub message: String,
}

impl ComputerError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    fn driver(message: impl Into<String>) -> Self {
        Self::new(DRIVER_ERROR, message)
    }
}

/// The driver is not installed: a fact about this Mac, retrying cannot help.
pub const UNAVAILABLE: &str = "computer_unavailable";
/// Another session is driving that app right now.
pub const APP_BUSY: &str = "computer_app_busy";
/// The tool is unknown or not offered to Agents.
pub const UNKNOWN_TOOL: &str = "computer_unknown_tool";
/// The driver process failed, timed out, or answered out of protocol.
pub const DRIVER_ERROR: &str = "computer_driver";

/// Who is calling: the Diri session, and the words for its cursor badge.
pub struct Caller<'a> {
    pub session_id: &'a str,
    pub badge: &'a str,
}

#[derive(Clone)]
pub struct ComputerUse {
    shared: Arc<Shared>,
}

struct Shared {
    inner: Mutex<Inner>,
    /// Waiting requests, apart from `inner`: the reader delivers replies
    /// while a launch holds `inner` through its handshake.
    pending: Mutex<HashMap<u64, Waiter>>,
    /// The live launch, readable without `inner` (see `read_replies`).
    incarnation: AtomicU64,
    binary: Option<PathBuf>,
}

type Waiter = mpsc::Sender<Result<Value, String>>;

#[derive(Default)]
struct Inner {
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    next_id: u64,
    incarnation: u64,
    last_activity: Option<Instant>,
    idle_sweeper_running: bool,
    /// The driver's `tools/list`, fetched once per launch.
    catalog: Option<Vec<Value>>,
    /// The CuaDriver daemon was not running until Diri needed it, so an
    /// idle Diri stops it again rather than leave it resident.
    started_daemon: bool,
    state: Sessions,
}

/// Which badge each Diri session drives under, and who holds which app.
/// Pure bookkeeping, kept apart from the pipe so it can be tested directly.
#[derive(Default, Debug)]
struct Sessions {
    labels: HashMap<String, String>,
    leases: HashMap<i64, Lease>,
}

#[derive(Debug, Clone)]
struct Lease {
    session_id: String,
    label: String,
    touched: Instant,
}

impl Sessions {
    /// The caller's badge label: stable for the session's life, unique among
    /// the badges this Engine has handed out.
    fn label_for(&mut self, caller: &Caller<'_>) -> String {
        if let Some(label) = self.labels.get(caller.session_id) {
            return label.clone();
        }
        let base = truncate_label(caller.badge.trim(), MAX_LABEL_CHARS);
        let base = if base.is_empty() {
            "Diri agent".to_owned()
        } else {
            base
        };
        let taken = |candidate: &str| self.labels.values().any(|label| label == candidate);
        let mut label = base.clone();
        let mut n = 2;
        while taken(&label) {
            let suffix = format!(" {n}");
            label = format!(
                "{}{suffix}",
                truncate_label(&base, MAX_LABEL_CHARS - suffix.chars().count())
            );
            n += 1;
        }
        self.labels
            .insert(caller.session_id.to_owned(), label.clone());
        label
    }

    /// Checks (and on success takes or renews) the caller's lease on every
    /// pid. `is_live` says whether a holder session still exists; a dead or
    /// quiet holder loses its lease to the caller.
    fn lease(
        &mut self,
        session_id: &str,
        label: &str,
        pids: &[i64],
        now: Instant,
        is_live: &dyn Fn(&str) -> bool,
    ) -> Result<(), ComputerError> {
        for pid in pids {
            if let Some(lease) = self.leases.get(pid)
                && lease.session_id != session_id
                && now.saturating_duration_since(lease.touched) < LEASE_IDLE
                && is_live(&lease.session_id)
            {
                let wait = LEASE_IDLE.saturating_sub(now.saturating_duration_since(lease.touched));
                return Err(ComputerError::new(
                    APP_BUSY,
                    format!(
                        "pid {pid} is being driven by \"{}\" (free in at most {}s if it stops). \
                         Launch your own copy instead: launch_app with \
                         creates_new_application_instance:true returns a fresh pid and window.",
                        lease.label,
                        wait.as_secs().max(1)
                    ),
                ));
            }
        }
        for pid in pids {
            self.leases.insert(
                *pid,
                Lease {
                    session_id: session_id.to_owned(),
                    label: label.to_owned(),
                    touched: now,
                },
            );
        }
        Ok(())
    }

    fn forget_session(&mut self, session_id: &str) {
        self.labels.remove(session_id);
        self.leases
            .retain(|_, lease| lease.session_id != session_id);
    }
}

impl ComputerUse {
    pub fn new() -> Self {
        Self::with_binary(None)
    }

    /// Tests point this at a fake driver.
    pub fn with_binary(binary: Option<PathBuf>) -> Self {
        Self {
            shared: Arc::new(Shared {
                inner: Mutex::new(Inner::default()),
                pending: Mutex::new(HashMap::new()),
                incarnation: AtomicU64::new(0),
                binary,
            }),
        }
    }

    fn binary(&self) -> Option<PathBuf> {
        match &self.shared.binary {
            Some(path) => is_executable(path).then(|| path.clone()),
            None => locate_driver(),
        }
    }

    pub fn unavailable_reason(&self) -> Option<String> {
        self.binary().is_none().then(|| {
            "Computer use needs Cua Driver, which is not installed. Install it with \
             /bin/bash -c \"$(curl -fsSL https://cua.ai/driver/install.sh)\", then run \
             `cua-driver permissions grant` to allow Accessibility and Screen Recording."
                .to_owned()
        })
    }

    /// One `computer` call from an Agent. `tool` names a driver tool, or
    /// `tools` for the catalog. `is_live` answers whether a session exists,
    /// so leases held by ended sessions are reclaimed.
    pub fn act(
        &self,
        caller: &Caller<'_>,
        tool: &str,
        args: Value,
        is_live: &dyn Fn(&str) -> bool,
    ) -> Result<Value, ComputerError> {
        if let Some(reason) = self.unavailable_reason() {
            return Err(ComputerError::new(UNAVAILABLE, reason));
        }
        if tool == "tools" {
            return self.catalog_summary(args.get("name").and_then(Value::as_str));
        }
        if HIDDEN_TOOLS.contains(&tool) {
            return Err(ComputerError::new(
                UNKNOWN_TOOL,
                format!("{tool} is managed by Diri and not available to Agents"),
            ));
        }
        let mut args = match args {
            Value::Null => Map::new(),
            Value::Object(map) => map,
            _ => {
                return Err(ComputerError::new(
                    UNKNOWN_TOOL,
                    "args must be an object of the tool's arguments",
                ));
            }
        };
        let catalog = self.catalog()?;
        if !catalog.iter().any(|entry| entry["name"] == tool) {
            return Err(ComputerError::new(
                UNKNOWN_TOOL,
                format!("no computer tool named {tool}; call tool \"tools\" for the list"),
            ));
        }

        let label = {
            let mut inner = self.lock();
            let label = inner.state.label_for(caller);
            if !READ_ONLY_TOOLS.contains(&tool) {
                let pids = mentioned_pids(&Value::Object(args.clone()));
                inner
                    .state
                    .lease(caller.session_id, &label, &pids, Instant::now(), is_live)?;
            }
            label
        };
        args.insert("session".into(), Value::String(label.clone()));
        bound_screenshot(tool, &mut args);

        let result = self
            .request(
                "tools/call",
                json!({"name": tool, "arguments": Value::Object(args)}),
                REQUEST_TIMEOUT,
            )
            .map_err(ComputerError::driver)?;

        // A launched app belongs to whoever launched it.
        if tool == "launch_app"
            && let Some(pid) = result["structuredContent"]["pid"].as_i64()
        {
            let mut inner = self.lock();
            let _ = inner
                .state
                .lease(caller.session_id, &label, &[pid], Instant::now(), &|_| {
                    false
                });
        }
        fit_result(result)
    }

    /// Lets go of a session's badge and leases (its tab closed).
    pub fn forget_session(&self, session_id: &str) {
        self.lock().state.forget_session(session_id);
    }

    /// Closes the driver pipe and wakes every pending caller.
    pub fn shutdown(&self) {
        if let Ok(mut inner) = self.shared.inner.lock() {
            terminate(&self.shared, &mut inner, "computer use stopped");
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.shared
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn catalog(&self) -> Result<Vec<Value>, ComputerError> {
        if let Some(catalog) = self.lock().catalog.clone() {
            return Ok(catalog);
        }
        let listed = self
            .request("tools/list", json!({}), HANDSHAKE_TIMEOUT)
            .map_err(ComputerError::driver)?;
        let tools: Vec<Value> = listed["tools"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|tool| {
                tool["name"]
                    .as_str()
                    .is_some_and(|name| !HIDDEN_TOOLS.contains(&name))
            })
            .collect();
        if tools.is_empty() {
            return Err(ComputerError::driver("Cua Driver listed no tools"));
        }
        self.lock().catalog = Some(tools.clone());
        Ok(tools)
    }

    /// `tools`: one line per tool, or one tool's full schema by `name`.
    fn catalog_summary(&self, name: Option<&str>) -> Result<Value, ComputerError> {
        let catalog = self.catalog()?;
        if let Some(name) = name {
            return catalog
                .into_iter()
                .find(|tool| tool["name"] == name)
                .map(|tool| {
                    json!({"name": tool["name"], "description": tool["description"],
                           "args": strip_session(tool["inputSchema"].clone())})
                })
                .ok_or_else(|| {
                    ComputerError::new(UNKNOWN_TOOL, format!("no computer tool named {name}"))
                });
        }
        let tools: Vec<Value> = catalog
            .iter()
            .map(|tool| {
                json!({"name": tool["name"],
                       "summary": first_sentence(tool["description"].as_str().unwrap_or(""))})
            })
            .collect();
        Ok(json!({"tools": tools,
                  "hint": "Pass {\"tool\":\"tools\",\"args\":{\"name\":\"<tool>\"}} for one tool's full arguments."}))
    }

    fn request(&self, method: &str, params: Value, timeout: Duration) -> Result<Value, String> {
        let (id, receiver) = {
            let mut inner = self.lock();
            self.ensure_running(&mut inner)?;
            send(&self.shared, &mut inner, method, params)?
        };
        await_reply(&self.shared, id, &receiver, timeout)
    }

    fn ensure_running(&self, inner: &mut Inner) -> Result<(), String> {
        inner.last_activity = Some(Instant::now());
        if let Some(child) = inner.child.as_mut() {
            if child.try_wait().ok().flatten().is_none() {
                return Ok(());
            }
            inner.child = None;
            inner.stdin = None;
            inner.catalog = None;
        }
        let binary = self.binary().ok_or("Cua Driver is not installed")?;
        if !inner.started_daemon && self.shared.binary.is_none() {
            inner.started_daemon = !daemon_listening();
        }
        let mut child = Command::new(&binary)
            .arg("mcp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            // The driver must not see which Diri session asked: it would
            // otherwise hand that identity to every app it launches.
            .env_remove("DIRIJOR_SOCKET")
            .env_remove("DIRIJOR_SESSION_ID")
            .spawn()
            .map_err(|error| format!("failed to start {}: {error}", binary.display()))?;
        inner.stdin = child.stdin.take();
        let stdout = child.stdout.take().ok_or("driver has no stdout")?;
        inner.incarnation += 1;
        self.shared
            .incarnation
            .store(inner.incarnation, Ordering::Release);
        inner.catalog = None;
        // A new pipe is a new driver transport: its sessions start fresh, so
        // badges from the last one no longer exist there.
        inner.state.labels.clear();
        let incarnation = inner.incarnation;
        let shared = Arc::downgrade(&self.shared);
        std::thread::Builder::new()
            .name("diri-computer-driver".into())
            .spawn(move || read_replies(stdout, shared, incarnation))
            .map_err(|error| format!("failed to start driver reader: {error}"))?;
        inner.child = Some(child);

        // MCP handshake, under the lock: nothing else may reach the pipe
        // before the driver has been initialized.
        let (id, receiver) = send(
            &self.shared,
            inner,
            "initialize",
            json!({"protocolVersion": "2025-06-18", "capabilities": {},
                   "clientInfo": {"name": "diri", "version": env!("CARGO_PKG_VERSION")}}),
        )?;
        let reply = receiver.recv_timeout(HANDSHAKE_TIMEOUT);
        forget(&self.shared, id);
        match reply {
            Ok(Ok(_)) => {}
            Ok(Err(error)) => {
                terminate(&self.shared, inner, "driver refused to initialize");
                return Err(format!("Cua Driver refused to initialize: {error}"));
            }
            Err(_) => {
                terminate(&self.shared, inner, "driver did not initialize");
                return Err("Cua Driver did not answer initialize".into());
            }
        }
        write_line(
            inner,
            &json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        )?;

        if !inner.idle_sweeper_running {
            inner.idle_sweeper_running = true;
            let shared = Arc::downgrade(&self.shared);
            let _ = std::thread::Builder::new()
                .name("diri-computer-idle".into())
                .spawn(move || sweep_idle(shared));
        }
        Ok(())
    }
}

impl Default for ComputerUse {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for Shared {
    fn drop(&mut self) {
        if let Ok(inner) = self.inner.get_mut() {
            inner.stdin = None;
            if let Some(mut child) = inner.child.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
}

type Pending = (u64, mpsc::Receiver<Result<Value, String>>);

fn send(
    shared: &Shared,
    inner: &mut Inner,
    method: &str,
    params: Value,
) -> Result<Pending, String> {
    inner.next_id += 1;
    let id = inner.next_id;
    let (sender, receiver) = mpsc::channel();
    pending(shared).insert(id, sender);
    if let Err(error) = write_line(
        inner,
        &json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}),
    ) {
        forget(shared, id);
        return Err(error);
    }
    Ok((id, receiver))
}

fn pending(shared: &Shared) -> std::sync::MutexGuard<'_, HashMap<u64, Waiter>> {
    shared
        .pending
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn forget(shared: &Shared, id: u64) {
    pending(shared).remove(&id);
}

fn write_line(inner: &mut Inner, message: &Value) -> Result<(), String> {
    let stdin = inner.stdin.as_mut().ok_or("driver is not running")?;
    let mut line = serde_json::to_vec(message).map_err(|error| error.to_string())?;
    line.push(b'\n');
    stdin
        .write_all(&line)
        .and_then(|()| stdin.flush())
        .map_err(|error| format!("driver write failed: {error}"))
}

fn await_reply(
    shared: &Shared,
    id: u64,
    receiver: &mpsc::Receiver<Result<Value, String>>,
    timeout: Duration,
) -> Result<Value, String> {
    match receiver.recv_timeout(timeout) {
        Ok(result) => result,
        Err(_) => {
            forget(shared, id);
            Err(format!(
                "Cua Driver did not answer within {}s",
                timeout.as_secs()
            ))
        }
    }
}

/// Routes each reply line to its waiting request until the pipe closes.
fn read_replies(
    stdout: std::process::ChildStdout,
    shared: std::sync::Weak<Shared>,
    incarnation: u64,
) {
    for line in BufReader::new(stdout).lines() {
        let Ok(line) = line else { break };
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let Some(id) = message.get("id").and_then(Value::as_u64) else {
            // Notifications (progress, logs) need no answer.
            continue;
        };
        let Some(shared) = shared.upgrade() else {
            return;
        };
        // Ids are never reused across launches, so a stale reader cannot
        // answer its successor's requests.
        if let Some(sender) = pending(&shared).remove(&id) {
            let outcome = match message.get("error") {
                Some(error) => Err(error["message"]
                    .as_str()
                    .map_or_else(|| error.to_string(), str::to_owned)),
                None => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
            };
            let _ = sender.send(outcome);
        }
    }
    let Some(shared) = shared.upgrade() else {
        return;
    };
    if shared.incarnation.load(Ordering::Acquire) != incarnation {
        return;
    }
    // Fail the waiters first: a launch may hold `inner` while it waits for
    // the handshake this exit will never answer.
    for (_, sender) in pending(&shared).drain() {
        let _ = sender.send(Err("Cua Driver exited".into()));
    }
    let Ok(mut inner) = shared.inner.lock() else {
        return;
    };
    if inner.incarnation == incarnation {
        terminate(&shared, &mut inner, "Cua Driver exited");
    }
}

fn sweep_idle(shared: std::sync::Weak<Shared>) {
    loop {
        std::thread::sleep(Duration::from_secs(30));
        let Some(shared) = shared.upgrade() else {
            return;
        };
        let Ok(mut inner) = shared.inner.lock() else {
            return;
        };
        let quiet = inner
            .last_activity
            .is_some_and(|at| at.elapsed() > IDLE_TIMEOUT);
        if quiet && pending(&shared).is_empty() && inner.child.is_some() {
            terminate(&shared, &mut inner, "driver closed after idle timeout");
            inner.state = Sessions::default();
            if std::mem::take(&mut inner.started_daemon)
                && let Some(binary) = locate_driver()
            {
                // Off the lock; the daemon answers `stop` at once.
                std::thread::spawn(move || {
                    let _ = Command::new(binary)
                        .arg("stop")
                        .stdin(Stdio::null())
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .status();
                });
            }
        }
    }
}

/// Closing stdin ends `cua-driver mcp` (and with it the transport's sessions
/// and cursors); a driver that ignores it is killed.
fn terminate(shared: &Shared, inner: &mut Inner, reason: &str) {
    inner.stdin = None;
    inner.catalog = None;
    for (_, sender) in pending(shared).drain() {
        let _ = sender.send(Err(reason.to_owned()));
    }
    let Some(mut child) = inner.child.take() else {
        return;
    };
    let deadline = Instant::now() + Duration::from_millis(500);
    while Instant::now() < deadline {
        if child.try_wait().ok().flatten().is_some() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// Every integer `pid` anywhere in the arguments: `run_steps` nests actions.
fn mentioned_pids(value: &Value) -> Vec<i64> {
    fn walk(value: &Value, out: &mut Vec<i64>) {
        match value {
            Value::Object(map) => {
                for (key, value) in map {
                    if key == "pid"
                        && let Some(pid) = value.as_i64()
                    {
                        if !out.contains(&pid) {
                            out.push(pid);
                        }
                    } else {
                        walk(value, out);
                    }
                }
            }
            Value::Array(items) => items.iter().for_each(|item| walk(item, out)),
            _ => {}
        }
    }
    let mut out = Vec::new();
    walk(value, &mut out);
    out
}

/// Screenshots default to a model-sized long edge, and never exceed what the
/// control socket can carry.
fn bound_screenshot(tool: &str, args: &mut Map<String, Value>) {
    if !matches!(tool, "get_window_state" | "get_desktop_state") {
        return;
    }
    let requested = args.get("max_image_dimension").and_then(Value::as_u64);
    let bounded = match requested {
        None => DEFAULT_IMAGE_DIMENSION,
        Some(0) => MAX_IMAGE_DIMENSION,
        Some(n) => n.min(MAX_IMAGE_DIMENSION),
    };
    args.insert("max_image_dimension".into(), json!(bounded));
}

/// The driver's MCP result, relayed verbatim when it fits on the control
/// socket; otherwise its images are dropped with a note, never truncated.
fn fit_result(result: Value) -> Result<Value, ComputerError> {
    let wrapped = json!({"mcpToolResult": result});
    let size = serde_json::to_vec(&wrapped).map_or(usize::MAX, |bytes| bytes.len());
    if size <= MAX_RESULT_BYTES {
        return Ok(wrapped);
    }
    let mut result = wrapped["mcpToolResult"].clone();
    if let Some(content) = result.get_mut("content").and_then(Value::as_array_mut) {
        content.retain(|block| block["type"] != "image");
        content.push(json!({"type": "text", "text":
            "The screenshot was too large to relay and was dropped; ask again with a smaller \
             max_image_dimension, or use zoom on a region."}));
    }
    let wrapped = json!({"mcpToolResult": result});
    let size = serde_json::to_vec(&wrapped).map_or(usize::MAX, |bytes| bytes.len());
    if size <= MAX_RESULT_BYTES {
        return Ok(wrapped);
    }
    Err(ComputerError::driver(format!(
        "the result is {size} bytes, more than Diri can relay; narrow it (query, max_elements, \
         include_screenshot:false)"
    )))
}

fn strip_session(mut schema: Value) -> Value {
    if let Some(properties) = schema["properties"].as_object_mut() {
        properties.remove("session");
    }
    schema
}

fn first_sentence(text: &str) -> String {
    let end = text
        .char_indices()
        .find(|(i, c)| {
            *c == '\n'
                || (*c == '.' && text[i + 1..].chars().next().is_none_or(char::is_whitespace))
        })
        .map_or(text.len(), |(i, _)| i);
    text[..end].trim().to_owned()
}

fn truncate_label(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out = out.trim_end().to_owned();
    out.push('…');
    out
}

/// Whether a CuaDriver daemon already serves this user (on macOS the MCP
/// pipe proxies to it, launching it on demand).
fn daemon_listening() -> bool {
    #[cfg(target_os = "macos")]
    {
        diri_platform::home_dir().is_some_and(|home| {
            std::os::unix::net::UnixStream::connect(
                home.join("Library/Caches/cua-driver/cua-driver.sock"),
            )
            .is_ok()
        })
    }
    #[cfg(not(target_os = "macos"))]
    {
        // Elsewhere `cua-driver mcp` owns its runtime and exits with the pipe.
        true
    }
}

/// `DIRI_CUA_DRIVER`, then the installed app, then the CLI on PATH.
fn locate_driver() -> Option<PathBuf> {
    if let Some(configured) = std::env::var_os("DIRI_CUA_DRIVER") {
        let path = PathBuf::from(configured);
        return path.is_file().then_some(path);
    }
    let mut candidates = vec![PathBuf::from(
        "/Applications/CuaDriver.app/Contents/MacOS/cua-driver",
    )];
    if let Some(home) = diri_platform::home_dir() {
        candidates.push(home.join("Applications/CuaDriver.app/Contents/MacOS/cua-driver"));
        candidates.push(home.join(".local/bin/cua-driver"));
    }
    if let Some(path) = std::env::var_os("PATH") {
        candidates.extend(std::env::split_paths(&path).map(|dir| dir.join("cua-driver")));
    }
    candidates.into_iter().find(|path| is_executable(path))
}

fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        path.metadata()
            .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

#[cfg(test)]
mod tests;
