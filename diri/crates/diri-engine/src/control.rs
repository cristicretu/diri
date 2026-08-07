//! The control channel: newline-delimited JSON over a Unix socket.
//!
//! This is the daemon's front door — what the app, the CLI and the MCP shim all
//! talk to. The wire format is not ours to choose: `diri-client` already speaks
//! it to the Swift daemon, so a Rust engine has to be indistinguishable on the
//! socket or every existing client breaks.
//!
//! What is implemented here is the core of that surface — handshake, list,
//! spawn, input, resize, read, kill. The rest of the method table (worktrees,
//! history, migration, hosts) is not yet ported; unknown methods return a
//! `not_found` control error, which is what an older daemon does for a method
//! it does not know, rather than dropping the connection.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use diri_proto::control::MAX_CONTROL_LINE_BYTES;
use diri_proto::{ControlError, ControlMessage, JsonValue, Method, WIRE_VERSION};
use serde_json::{Value, json};

use crate::registry::Registry;

/// Identifies this engine in the handshake, so a client can tell which
/// implementation it reached.
pub const BUILD: &str = concat!("diri-engine-", env!("CARGO_PKG_VERSION"));

pub struct ControlServer {
    registry: Arc<Mutex<Registry>>,
    socket_path: PathBuf,
    logs_dir: PathBuf,
    holder: Option<crate::session::HolderConfig>,
    events: crate::events::EventBus,
    attach: crate::attach::AttachHub,
    pr_monitor_wake: crate::pr_monitor::PrMonitorWake,
    injection: Option<InjectionConfig>,
    governor: std::sync::Arc<Mutex<crate::governor::GovernorConfig>>,
    browser: std::sync::OnceLock<crate::browser::BrowserPool>,
}

/// Where injection files live and which CLI they point at. Present, spawns
/// become hook-driven and get the dirijor MCP tools.
#[derive(Clone, Debug)]
pub struct InjectionConfig {
    pub inject_dir: PathBuf,
    pub cli_path: PathBuf,
}

impl ControlServer {
    pub fn new(registry: Arc<Mutex<Registry>>, socket_path: impl Into<PathBuf>) -> Self {
        let socket_path = socket_path.into();
        let logs_dir = socket_path
            .parent()
            .map(|parent| parent.join("logs"))
            .unwrap_or_else(|| PathBuf::from("logs"));
        Self {
            registry,
            socket_path,
            logs_dir,
            holder: None,
            events: crate::events::EventBus::new(),
            attach: crate::attach::AttachHub::new(),
            pr_monitor_wake: crate::pr_monitor::PrMonitorWake::default(),
            injection: None,
            governor: std::sync::Arc::new(Mutex::new(crate::governor::GovernorConfig::default())),
            browser: std::sync::OnceLock::new(),
        }
    }

    /// Enables spawn-time hook/MCP injection: writes the shim files (like the
    /// Swift daemon does at startup) and applies each manifest's mechanisms
    /// to future spawns.
    pub fn with_injection(mut self, config: InjectionConfig) -> Self {
        let _ = crate::inject::write_claude_hooks_file(&config.inject_dir);
        let _ = crate::inject::write_claude_mcp_file(&config.inject_dir, &config.cli_path);
        self.injection = Some(config);
        self
    }

    /// The bus this server publishes to — the daemon shares it with the
    /// registry watcher (see [`crate::events::spawn_registry_watcher`]).
    pub fn events(&self) -> crate::events::EventBus {
        self.events.clone()
    }

    /// The attach hub, for the resource governor's attached-session checks.
    pub fn attach_hub(&self) -> crate::attach::AttachHub {
        self.attach.clone()
    }

    /// Event-driven invalidation shared by selection/focus, artifact
    /// discovery, and the background PR monitor.
    pub fn pr_monitor_wake(&self) -> crate::pr_monitor::PrMonitorWake {
        self.pr_monitor_wake.clone()
    }

    /// The governor tunables `governor.configure` updates in place.
    pub fn governor_config(&self) -> std::sync::Arc<Mutex<crate::governor::GovernorConfig>> {
        std::sync::Arc::clone(&self.governor)
    }

    /// Where session output logs are written. Defaults to `logs/` beside the
    /// socket, matching the Swift daemon's layout.
    pub fn with_logs_dir(mut self, logs_dir: impl Into<PathBuf>) -> Self {
        self.logs_dir = logs_dir.into();
        self
    }

    /// Spawn sessions through holders, so they survive this process. This is
    /// how the daemon runs; tests and embedded callers may stay direct.
    pub fn with_holder(mut self, holder: crate::session::HolderConfig) -> Self {
        self.holder = Some(holder);
        self
    }

    /// Binds the socket, owner-only.
    ///
    /// The socket carries a user's terminal contents and can spawn processes as
    /// them, so the permissions are part of the security model, not a detail.
    /// A stale socket file from a dead daemon is replaced; a *live* one is not,
    /// which is what stops two engines fighting over the same endpoint.
    pub fn bind(&self) -> std::io::Result<UnixListener> {
        if self.socket_path.exists() {
            if UnixStream::connect(&self.socket_path).is_ok() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::AddrInUse,
                    format!(
                        "something is already serving {}",
                        self.socket_path.display()
                    ),
                ));
            }
            std::fs::remove_file(&self.socket_path)?;
        }
        if let Some(parent) = self.socket_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let listener = UnixListener::bind(&self.socket_path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&self.socket_path, std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(listener)
    }

    /// Serves one connection to completion.
    ///
    /// The FIRST line decides what this connection is: an [`AttachRequest`]
    /// makes it a binary session data channel, anything else is control
    /// NDJSON — the same sniff the Swift `ConnectionHub` does, so one socket
    /// path serves both.
    ///
    /// The write half is shared: after `events.subscribe`, a forwarder thread
    /// pushes event frames onto the same socket while this loop keeps
    /// answering requests — one connection carries both, as the Swift daemon's
    /// does.
    pub fn serve(&self, stream: UnixStream) -> std::io::Result<()> {
        let mut reader = BufReader::new(stream.try_clone()?);
        let writer = Arc::new(Mutex::new(stream));
        let mut subscription: Option<SubscriptionHandle> = None;

        let mut first = true;
        loop {
            let mut line = Vec::new();
            let read = reader.read_until(b'\n', &mut line)?;
            if read == 0 {
                return Ok(());
            }
            if line.last() == Some(&b'\n') {
                line.pop();
            }
            if line.is_empty() {
                continue;
            }
            if first {
                first = false;
                if let Ok(attach) = serde_json::from_slice::<diri_proto::AttachRequest>(&line) {
                    // Attaching means this session is visible. Record that
                    // before waking the PR monitor so its immediate pass sees
                    // the session as foreground/recent even if registration
                    // has not completed yet.
                    if let Ok(mut registry) = self.registry.lock() {
                        let _ = registry.mark_seen(&attach.attach.0);
                        let _ = registry.persist();
                        self.publish_updated(&registry, &attach.attach.0);
                    }
                    self.pr_monitor_wake.wake_session(attach.attach.0.clone());
                    // Bytes the line reader buffered past the attach line are
                    // already binary frames; hand them over.
                    let buffered = reader.buffer().to_vec();
                    self.attach.serve(
                        &self.registry,
                        &attach.attach.0,
                        reader.into_inner(),
                        buffered,
                        writer,
                    );
                    return Ok(());
                }
            }
            if line.len() > MAX_CONTROL_LINE_BYTES {
                // A client that sends an oversized frame is out of contract;
                // answering would mean buffering unbounded input.
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "control line exceeded the protocol maximum",
                ));
            }
            let Some(response) = self.handle_line(&line, &writer, &mut subscription) else {
                continue;
            };
            write_message(&writer, &response)?;
        }
    }

    fn handle_line(
        &self,
        line: &[u8],
        writer: &Arc<Mutex<UnixStream>>,
        subscription: &mut Option<SubscriptionHandle>,
    ) -> Option<ControlMessage> {
        let message: ControlMessage = match serde_json::from_slice(line) {
            Ok(message) => message,
            Err(error) => {
                // Malformed input gets an error with id 0 rather than silence:
                // a client waiting on a reply should learn it will not come.
                return Some(ControlMessage::Response {
                    id: 0,
                    result: Err(ControlError::bad_request(format!(
                        "could not parse control message: {error}"
                    ))),
                });
            }
        };

        match message {
            ControlMessage::Request { id, method, params }
                if method == Method::EVENTS_SUBSCRIBE =>
            {
                Some(ControlMessage::Response {
                    id,
                    result: self.events_subscribe(params, writer, subscription),
                })
            }
            ControlMessage::Request { id, method, params } => Some(ControlMessage::Response {
                id,
                result: self.dispatch(&method, params),
            }),
            // Responses and events are the daemon's to send, not receive.
            ControlMessage::Response { .. } | ControlMessage::Event { .. } => None,
        }
    }

    /// Turns this connection into an event sink: a forwarder thread streams
    /// matching events as they publish, replaying from `sinceSeq` first.
    /// Re-subscribing replaces the previous subscription, as in Swift.
    fn events_subscribe(
        &self,
        params: Option<JsonValue>,
        writer: &Arc<Mutex<UnixStream>>,
        subscription: &mut Option<SubscriptionHandle>,
    ) -> Result<JsonValue, ControlError> {
        let p: diri_proto::EventsSubscribeParams = decode(params).unwrap_or_default();
        if let Some(previous) = subscription.take() {
            previous
                .stop
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
        let stream = self.events.subscribe(
            p.since_seq,
            crate::events::Filter::new(
                p.sessions
                    .map(|sessions| sessions.into_iter().map(|id| id.0).collect()),
                p.kinds,
            ),
        );
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let handle = {
            let stop = Arc::clone(&stop);
            let writer = Arc::clone(writer);
            std::thread::Builder::new()
                .name("diri-control-events".into())
                .spawn(move || {
                    while !stop.load(std::sync::atomic::Ordering::SeqCst) {
                        let Some(event) = stream.recv(std::time::Duration::from_millis(250)) else {
                            continue;
                        };
                        let frame = ControlMessage::Event {
                            name: event.name,
                            seq: event.seq,
                            params: event.params,
                        };
                        if write_message(&writer, &frame).is_err() {
                            break; // peer is gone; dropping the stream unsubscribes
                        }
                    }
                })
                .map_err(|error| ControlError::internal(error.to_string()))?
        };
        *subscription = Some(SubscriptionHandle {
            stop,
            _thread: handle,
        });
        Ok(json!({ "subscribed": true }))
    }

    /// One-shot long poll for a session reaching one of the `until` statuses.
    fn events_wait(&self, params: Option<JsonValue>) -> Result<JsonValue, ControlError> {
        let p: diri_proto::EventsWaitParams = decode(params)?;
        if p.until.is_empty() {
            return Err(ControlError::bad_request(
                "events.wait needs `until` statuses",
            ));
        }
        let deadline = std::time::Instant::now()
            + std::time::Duration::from_millis(p.timeout_ms.clamp(0, 600_000) as u64);

        // Subscribe before the pre-check, so a transition landing between the
        // two is buffered rather than lost.
        let stream = self.events.subscribe(
            None,
            crate::events::Filter::new(
                Some(vec![p.session_id.0.clone()]),
                Some(vec![diri_proto::EventName::SESSION_UPDATED.to_string()]),
            ),
        );

        let current = |registry: &Registry| -> Option<diri_proto::SessionRecord> {
            registry
                .records()
                .into_iter()
                .find(|record| record.id.0 == p.session_id.0)
        };
        let matches = |record: &diri_proto::SessionRecord| {
            p.until
                .iter()
                .any(|target| crate::events::satisfies_wait_target(&record.status, target))
        };

        let mut latest = {
            let registry = self.registry.lock().map_err(poisoned)?;
            current(&registry).ok_or_else(|| ControlError::not_found(p.session_id.0.clone()))?
        };
        loop {
            if matches(&latest) {
                return encode(&diri_proto::EventsWaitResult {
                    session: latest,
                    timed_out: false,
                });
            }
            let Some(remaining) = deadline.checked_duration_since(std::time::Instant::now()) else {
                return encode(&diri_proto::EventsWaitResult {
                    session: latest,
                    timed_out: true,
                });
            };
            if stream.recv(remaining).is_some() {
                let registry = self.registry.lock().map_err(poisoned)?;
                if let Some(record) = current(&registry) {
                    latest = record;
                }
            }
        }
    }

    fn dispatch(&self, method: &str, params: Option<JsonValue>) -> Result<JsonValue, ControlError> {
        match method {
            Method::HELLO => self.hello(params),
            Method::SESSION_SPAWN => self.session_spawn(params),
            Method::SESSION_LIST | Method::STATE_SNAPSHOT => self.session_list(),
            Method::SESSION_SEND_TEXT => self.session_send_text(params),
            Method::SESSION_RESIZE => self.session_resize(params),
            Method::SESSION_READ_SCREEN => self.session_read_screen(params),
            Method::SESSION_READ_SCROLLBACK => self.session_read_scrollback(params),
            Method::SESSION_READ_SCROLLBACK_CELLS => self.session_read_scrollback_cells(params),
            Method::SESSION_KILL => self.session_kill(params),
            Method::SESSION_REMOVE => self.session_remove(params),
            Method::SESSION_RENAME => self.session_rename(params),
            Method::SESSION_MARK_SEEN => self.session_mark_seen(params),
            Method::SESSION_ARCHIVE => self.session_archive(params),
            Method::SESSION_UNARCHIVE => self.session_unarchive(params),
            Method::SESSION_HISTORY => self.session_history(),
            Method::WORKTREE_CREATE => self.worktree_create(params),
            Method::WORKTREE_LIST => self.worktree_list(params),
            Method::WORKTREE_REMOVE => self.worktree_remove(params),
            Method::WORKTREE_OVERVIEW => self.worktree_overview(),
            Method::TEST_RUN => self.browser_call("run", params),
            "browser.act" => self.browser_call("browser", params),
            Method::EVENTS_WAIT => self.events_wait(params),
            Method::HOST_SYNC_PREFS => self.host_sync_prefs(params),
            Method::SESSION_MIGRATE => self.session_migrate(params),
            Method::HOST_LOCATE_REPO => self.host_locate_repo(params),
            Method::HOOK_REPORT => self.hook_report(params),
            Method::SESSION_RESUME => self.session_resume(params),
            Method::SESSION_RESUME_FROM_HISTORY => self.session_resume_from_history(params),
            Method::SESSION_REOPEN_LAST => self.session_reopen_last(),
            Method::AGENT_READINESS => self.agent_readiness(),
            Method::PROJECT_ADD => self.project_add(params),
            Method::SESSION_READ_DIFF => self.session_read_diff(params),
            Method::SESSION_HIBERNATE => self.session_hibernate(params),
            Method::SESSION_WAKE => self.session_wake(params),
            Method::DAEMON_PREPARE_SHUTDOWN => self.daemon_prepare_shutdown(),
            Method::DAEMON_SHUTDOWN => self.daemon_shutdown(),
            Method::GOVERNOR_CONFIGURE => self.governor_configure(params),
            Method::CLIENT_SET_ACTIVE => self.client_set_active(params),
            // Ownership arbitration is a desktop/mobile feature this engine
            // does not model yet; accepting it keeps clients on their happy path.
            Method::SESSION_SET_OWNER => Ok(json!({})),
            other => Err(ControlError::not_found(format!(
                "method {other:?} is not implemented by this engine yet"
            ))),
        }
    }

    fn hello(&self, params: Option<JsonValue>) -> Result<JsonValue, ControlError> {
        let proto = params
            .as_ref()
            .and_then(|value| value.get("proto"))
            .and_then(Value::as_u64)
            .unwrap_or(WIRE_VERSION as u64);
        if proto != WIRE_VERSION as u64 {
            return Err(ControlError::version_mismatch(format!(
                "client speaks protocol {proto}, this engine speaks {WIRE_VERSION}"
            )));
        }
        Ok(json!({
            "proto": WIRE_VERSION,
            "build": BUILD,
            "pid": std::process::id() as i32,
        }))
    }

    /// Starts an agent and begins watching it.
    ///
    /// The command line comes from the manifest's agent descriptor, so this
    /// works for any agent that has one without code changes. Two limits worth
    /// stating: hook and MCP injection are not ported yet, so a Claude session
    /// started here is screen-detected rather than hook-driven; and `shell` and
    /// `generic` need an explicit `argv`, since their manifests declare no
    /// binary.
    fn session_spawn(&self, params: Option<JsonValue>) -> Result<JsonValue, ControlError> {
        let raw = params.ok_or_else(|| ControlError::bad_request("params are required"))?;
        // Tests and scripts may pass a raw argv; the app never does. Read it
        // before the typed decode consumes the value.
        let argv: Vec<String> = raw
            .get("argv")
            .and_then(Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        let p: diri_proto::SessionSpawnParams = decode(Some(raw))?;
        if let Some(host_id) = &p.host {
            return self.session_spawn_remote(&p, host_id);
        }
        let kind = p.kind.id().to_string();
        // A generic kind carries the user's command line inside itself.
        let argv = if argv.is_empty() {
            match p.kind.command() {
                Some(command) if !command.is_empty() => {
                    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
                    vec![shell, "-lc".into(), command.to_string()]
                }
                _ if kind == diri_proto::AgentKind::SHELL_ID => {
                    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
                    vec![shell, "-l".into()]
                }
                _ => Vec::new(),
            }
        } else {
            argv
        };

        // A worktree spawn creates the checkout first, then lands in it.
        let mut cwd = p.cwd.clone();
        let mut worktree_path = None;
        let mut git_branch = None;
        if p.new_worktree.unwrap_or(false) {
            let info =
                crate::git::create_worktree(Path::new(&p.cwd), p.worktree_branch.as_deref(), None)
                    .map_err(io_control_error)?;
            git_branch.clone_from(&info.branch);
            cwd.clone_from(&info.path);
            worktree_path = Some(info.path);
        }
        let cwd_path = PathBuf::from(&cwd);
        if !cwd_path.is_dir() {
            return Err(ControlError::bad_request(format!(
                "cwd {cwd:?} is not a directory"
            )));
        }

        let mut registry = self.registry.lock().map_err(poisoned)?;
        let engine = registry.engine();
        let manifest = engine
            .manifest(&kind)
            .ok_or_else(|| ControlError::not_found(format!("no manifest for agent {kind:?}")))?;
        let descriptor = manifest.agent.clone().unwrap_or_default();
        let authority = descriptor.authority();

        let id = next_session_id();
        // Build the complete agent argv before `spawn_spec`: agents declaring
        // `returnToLoginShell` need every manifest and injection argument
        // quoted inside the shell's `-c` command.
        let mut launch_args = argv.clone();
        let mut agent_session_id = None;
        if descriptor.binary.is_some() {
            launch_args.extend(descriptor.spawn_args.iter().cloned());
            agent_session_id = descriptor.session_id_flag.as_ref().map(|flag| {
                let uuid = crate::inject::uuid_v4();
                launch_args.push(flag.clone());
                launch_args.push(uuid.clone());
                uuid
            });
            if let Some(injection) = &self.injection {
                launch_args.extend(crate::inject::injection_args(
                    &descriptor.injection,
                    &injection.inject_dir,
                    &injection.cli_path,
                ));
            }
        }

        let inherited: Vec<(String, String)> = std::env::vars().collect();
        let mut pty = match descriptor.spawn_spec(&cwd_path, inherited.clone(), &launch_args) {
            Some(spec) => spec,
            // No binary in the manifest: the caller has to say what to run.
            None if !argv.is_empty() => {
                let mut spec = crate::pty::PtySpec::new(argv.clone(), &cwd_path);
                spec.env = inherited;
                spec.env.retain(|(key, _)| key != "NO_COLOR");
                spec
            }
            None => {
                return Err(ControlError::bad_request(format!(
                    "agent {kind:?} declares no binary, so argv is required"
                )));
            }
        };

        let mut record = new_record(&id, &kind, &cwd);
        if let Some(title) = &p.title {
            record.title = title.clone();
            record.title_source = diri_proto::TitleSource::DirijorAssigned;
        }
        record.worktree_path = worktree_path;
        record.git_branch = git_branch.or_else(|| crate::git::branch(&cwd_path));
        record.parent = p.parent.clone();
        if let (Some(cols), Some(rows)) = (p.initial_cols, p.initial_rows) {
            pty.cols = cols.clamp(2, u16::MAX as i64) as u16;
            pty.rows = rows.clamp(2, u16::MAX as i64) as u16;
        }

        // Injection environment and the caller-minted conversation UUID. The
        // argv side was assembled before `spawn_spec` so its shell wrapper
        // contains the complete command.
        if descriptor.binary.is_some() {
            if let Some(injection) = &self.injection {
                pty.env
                    .push((crate::inject::SESSION_ID_ENV.into(), id.clone()));
                pty.env.push((
                    crate::inject::SOCKET_ENV.into(),
                    self.socket_path.to_string_lossy().into_owned(),
                ));
                pty.env.push((
                    crate::inject::CLI_ENV.into(),
                    injection.cli_path.to_string_lossy().into_owned(),
                ));
            }
            if let Some(uuid) = &agent_session_id {
                record.agent_session_id = Some(uuid.clone());
                if descriptor.injection.claude_hooks
                    && let Ok(home) = std::env::var("HOME")
                {
                    record.transcript_path = Some(
                        crate::inject::claude_transcript_path(Path::new(&home), &cwd, uuid)
                            .to_string_lossy()
                            .into_owned(),
                    );
                }
            }
        }
        let spec = crate::session::SessionSpec {
            id: id.clone(),
            pty,
            manifest_id: kind,
            authority,
            logs_dir: self.logs_dir.clone(),
            holder: self.holder.clone(),
            defer_launch: true,
        };
        registry
            .spawn(spec, record)
            .map_err(|error| ControlError::internal(error.to_string()))?;
        let _ = registry.persist();
        self.publish_updated(&registry, &id);

        // An initial prompt is typed once the TUI can actually receive input,
        // and verified on screen afterward — ported from the Swift
        // `injectInitialPrompt`, which replaced a blind fixed delay that
        // raced Claude Code's boot and lost keystrokes into a composer that
        // did not exist yet.
        if let Some(prompt) = p.initial_prompt.clone().filter(|prompt| !prompt.is_empty()) {
            let registry = Arc::clone(&self.registry);
            let session_id = id.clone();
            std::thread::spawn(move || {
                inject_initial_prompt(&registry, &session_id, &prompt);
            });
        }

        let record = registry
            .records()
            .into_iter()
            .find(|record| record.id.0 == id)
            .ok_or_else(|| ControlError::internal("the new session vanished"))?;
        // SessionSpawnResult is the record itself, as the Swift daemon
        // answers — not wrapped.
        serde_json::to_value(&record).map_err(|error| ControlError::internal(error.to_string()))
    }

    /// Spawn on a remote host: the local PTY runs ssh, which runs tmux on the
    /// host — tmux is what keeps the agent alive across SSH drops, and the
    /// `-A` reattach semantics make respawn, reconnect, and resume one path.
    fn session_spawn_remote(
        &self,
        p: &diri_proto::SessionSpawnParams,
        host_id: &str,
    ) -> Result<JsonValue, ControlError> {
        let entry = self.resolve_host(host_id)?;
        let kind = p.kind.id().to_string();
        let registry_engine = {
            let registry = self.registry.lock().map_err(poisoned)?;
            registry.engine()
        };
        let manifest = registry_engine
            .manifest(&kind)
            .ok_or_else(|| ControlError::not_found(format!("no manifest for agent {kind}")))?;
        let descriptor = manifest.agent.clone().unwrap_or_default();
        let authority = descriptor.authority();

        let remote_cwd = if p.cwd.is_empty() {
            entry.default_cwd.clone().unwrap_or_else(|| "~".into())
        } else {
            p.cwd.clone()
        };
        let id = next_session_id();
        // Only agents that accept a caller-minted id get one; for the rest
        // the remote conversation id never reaches us.
        let agent_session_id = descriptor
            .session_id_flag
            .is_some()
            .then(crate::inject::uuid_v4);
        let argv = crate::remote::remote_argv(
            &kind,
            &descriptor,
            &id,
            &entry,
            &remote_cwd,
            agent_session_id.as_deref(),
            false,
        );

        // The local PTY runs ssh from home; hooks and MCP flags reference
        // local paths that don't exist on the other machine, so none are
        // injected — but the DIRIJOR env triplet stays local-side.
        let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
        let mut pty = crate::pty::PtySpec::new(argv, &home);
        pty.env = std::env::vars().collect();
        pty.env.retain(|(key, _)| key != "NO_COLOR");
        absolutize_remote_argv0(&mut pty);
        if let (Some(cols), Some(rows)) = (p.initial_cols, p.initial_rows) {
            pty.cols = cols.clamp(2, u16::MAX as i64) as u16;
            pty.rows = rows.clamp(2, u16::MAX as i64) as u16;
        }
        if let Some(injection) = &self.injection {
            pty.env
                .push((crate::inject::SESSION_ID_ENV.into(), id.clone()));
            pty.env.push((
                crate::inject::SOCKET_ENV.into(),
                self.socket_path.to_string_lossy().into_owned(),
            ));
            pty.env.push((
                crate::inject::CLI_ENV.into(),
                injection.cli_path.to_string_lossy().into_owned(),
            ));
        }

        let mut record = new_record(&id, &kind, &remote_cwd);
        record.host = Some(entry.id.clone());
        record.agent_session_id = agent_session_id;
        if let Some(title) = &p.title {
            record.title = title.clone();
            record.title_source = diri_proto::TitleSource::DirijorAssigned;
        }
        record.parent = p.parent.clone();

        let spec = crate::session::SessionSpec {
            id: id.clone(),
            pty,
            manifest_id: kind,
            authority,
            logs_dir: self.logs_dir.clone(),
            holder: self.holder.clone(),
            defer_launch: true,
        };
        let mut registry = self.registry.lock().map_err(poisoned)?;
        registry
            .spawn(spec, record)
            .map_err(|error| ControlError::internal(error.to_string()))?;
        let _ = registry.persist();
        self.publish_updated(&registry, &id);
        let record = registry
            .records()
            .into_iter()
            .find(|record| record.id.0 == id)
            .ok_or_else(|| ControlError::internal("the new session vanished"))?;
        serde_json::to_value(&record).map_err(|error| ControlError::internal(error.to_string()))
    }

    /// Revives a remote session under its existing record: ssh + `tmux
    /// new-session -A` reattaches or restarts, resuming the conversation.
    fn session_resume_remote(
        &self,
        record: &diri_proto::SessionRecord,
        host_id: &str,
    ) -> Result<JsonValue, ControlError> {
        let entry = self.resolve_host(host_id)?;
        let kind = record.kind.id().to_string();
        let registry_engine = {
            let registry = self.registry.lock().map_err(poisoned)?;
            registry.engine()
        };
        let manifest = registry_engine
            .manifest(&kind)
            .ok_or_else(|| ControlError::not_found(format!("no manifest for agent {kind}")))?;
        let descriptor = manifest.agent.clone().unwrap_or_default();
        let argv = crate::remote::remote_argv(
            &kind,
            &descriptor,
            &record.id.0,
            &entry,
            &record.cwd,
            record.agent_session_id.as_deref(),
            true,
        );
        let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
        let mut pty = crate::pty::PtySpec::new(argv, &home);
        pty.env = std::env::vars().collect();
        pty.env.retain(|(key, _)| key != "NO_COLOR");
        absolutize_remote_argv0(&mut pty);
        let spec = crate::session::SessionSpec {
            id: record.id.0.clone(),
            pty,
            manifest_id: kind,
            authority: descriptor.authority(),
            logs_dir: self.logs_dir.clone(),
            holder: self.holder.clone(),
            defer_launch: true,
        };
        let mut registry = self.registry.lock().map_err(poisoned)?;
        registry
            .respawn(spec)
            .map_err(|error| ControlError::internal(error.to_string()))?;
        let _ = registry.persist();
        self.publish_updated(&registry, &record.id.0);
        let record = registry
            .records()
            .into_iter()
            .find(|current| current.id.0 == record.id.0)
            .ok_or_else(|| ControlError::internal("the resumed session vanished"))?;
        serde_json::to_value(&record).map_err(|error| ControlError::internal(error.to_string()))
    }

    /// `test.run` / `browser.act`: the Playwright sidecar, launched lazily.
    fn browser_call(
        &self,
        method: &str,
        params: Option<JsonValue>,
    ) -> Result<JsonValue, ControlError> {
        let params = params.ok_or_else(|| ControlError::bad_request("params are required"))?;
        let pool = self
            .browser
            .get_or_init(|| crate::browser::BrowserPool::new(&self.logs_dir));
        let result = if method == "run" {
            pool.run(params)
        } else {
            pool.browse(params)
        };
        result.map_err(|error| ControlError {
            code: "browser_pool".into(),
            message: error,
        })
    }

    /// The aggregated staleness view: every worktree of every project,
    /// joined with the session (live wins) occupying it, its dirtiness,
    /// merged-ness into the default branch, and age — plus the "safe to
    /// clean up" suggestion.
    fn worktree_overview(&self) -> Result<JsonValue, ControlError> {
        let (records, mut roots) = {
            let registry = self.registry.lock().map_err(poisoned)?;
            let roots: Vec<String> = registry
                .projects_raw()
                .iter()
                .filter_map(|project| project.get("root").and_then(|value| value.as_str()))
                .map(str::to_string)
                .collect();
            (registry.records(), roots)
        };
        roots.sort();

        // Join sessions by worktree path (fallback cwd); a live session wins
        // over an exited one sharing the path.
        let mut session_by_path: std::collections::HashMap<String, &diri_proto::SessionRecord> =
            std::collections::HashMap::new();
        let running = |record: &diri_proto::SessionRecord| {
            !matches!(
                record.status,
                diri_proto::SessionStatus::Exited(_) | diri_proto::SessionStatus::Unknown
            )
        };
        for record in &records {
            let path = record
                .worktree_path
                .clone()
                .unwrap_or_else(|| record.cwd.clone());
            match session_by_path.get(&path) {
                Some(existing) if running(existing) || !running(record) => {}
                _ => {
                    session_by_path.insert(path, record);
                }
            }
        }

        let run_git = |args: &[&str], dir: &str| -> Option<String> {
            let output = std::process::Command::new("git")
                .args(args)
                .current_dir(dir)
                .output()
                .ok()?;
            output
                .status
                .success()
                .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
        };

        let mut entries = Vec::new();
        let mut seen_paths = std::collections::HashSet::new();
        for root in roots {
            if !crate::git::is_repository(Path::new(&root)) {
                continue;
            }
            let Ok(worktrees) = crate::git::list_worktrees(Path::new(&root)) else {
                continue;
            };
            // Repo's default branch: origin/HEAD symbolic ref, else "main".
            let default_branch = run_git(
                &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
                &root,
            )
            .and_then(|full| full.rsplit('/').next().map(str::to_string))
            .filter(|short| !short.is_empty())
            .unwrap_or_else(|| "main".into());
            let merged_branches: std::collections::HashSet<String> = run_git(
                &[
                    "branch",
                    "--merged",
                    &default_branch,
                    "--format=%(refname:short)",
                ],
                &root,
            )
            .map(|output| output.lines().map(str::to_string).collect())
            .unwrap_or_default();

            for worktree in worktrees {
                if worktree.is_bare || !seen_paths.insert(worktree.path.clone()) {
                    continue;
                }
                let is_main = worktree.path == root;
                let dirty = run_git(&["status", "--porcelain"], &worktree.path)
                    .is_some_and(|output| !output.is_empty());
                let merged = worktree.branch.as_ref().is_some_and(|branch| {
                    branch != &default_branch && merged_branches.contains(branch)
                });
                let age_days = std::fs::metadata(&worktree.path)
                    .ok()
                    .and_then(|meta| meta.created().or_else(|_| meta.modified()).ok())
                    .and_then(|at| at.elapsed().ok())
                    .map(|elapsed| (elapsed.as_secs() / 86_400) as i64)
                    .unwrap_or(0);
                let record = session_by_path.get(&worktree.path);
                let session_alive = record.is_some_and(|record| running(record));
                entries.push(diri_proto::WorktreeOverviewEntry {
                    path: worktree.path.clone(),
                    branch: worktree.branch.clone(),
                    project_root: root.clone(),
                    session_id: record.map(|record| record.id.clone()),
                    session_status: record.map(|record| record.status.clone()),
                    dirty,
                    merged,
                    age_days,
                    stale_suggestion: !is_main
                        && !session_alive
                        && merged
                        && !dirty
                        && age_days > 7,
                });
            }
        }
        encode(&diri_proto::WorktreeOverviewResult { entries })
    }

    /// One-click handoff of a live Claude session between hosts: WIP commit
    /// plus push plus hard-sync of the target checkout (phase 1, retryable),
    /// stop the source, shuttle the transcript, rewrite the record in place,
    /// and revive on the target through the normal resume path.
    fn session_migrate(&self, params: Option<JsonValue>) -> Result<JsonValue, ControlError> {
        let p: diri_proto::SessionMigrateParams = decode(params)?;
        let id = p.session_id.0.clone();
        let record = {
            let registry = self.registry.lock().map_err(poisoned)?;
            registry
                .records()
                .into_iter()
                .find(|record| record.id.0 == id)
                .ok_or_else(|| ControlError::not_found(id.clone()))?
        };
        if record.kind.id() != diri_proto::AgentKind::CLAUDE_CODE_ID {
            return Err(ControlError::bad_request(
                "only Claude Code sessions can move between hosts",
            ));
        }
        if record.host == p.target_host {
            return Err(ControlError::bad_request(match &p.target_host {
                Some(host) => format!("session is already on {host}"),
                None => "session is already local".to_string(),
            }));
        }
        let source_host = record
            .host
            .as_deref()
            .map(|host| self.resolve_host(host))
            .transpose()?;
        let target_host = p
            .target_host
            .as_deref()
            .map(|host| self.resolve_host(host))
            .transpose()?;
        let home = std::env::var("HOME")
            .map(PathBuf::from)
            .map_err(|_| ControlError::internal("HOME is not set"))?;

        // Locate the target checkout by origin (shared with host.locate_repo).
        let origin =
            crate::hosts::origin_of_cwd(&record.cwd, source_host.as_ref()).ok_or_else(|| {
                ControlError::bad_request(format!(
                    "session cwd is not inside a git repository with an 'origin' remote: {}",
                    record.cwd
                ))
            })?;
        let local_roots: Vec<String> = {
            let registry = self.registry.lock().map_err(poisoned)?;
            registry
                .projects_raw()
                .iter()
                .filter_map(|project| project.get("root").and_then(|value| value.as_str()))
                .map(str::to_string)
                .collect()
        };
        let target_repo = crate::hosts::locate(&origin, target_host.as_ref(), &local_roots)
            .ok_or_else(|| match &target_host {
                Some(host) => ControlError::bad_request(format!(
                    "repo not cloned on {} — clone {origin} under {} first",
                    host.display_name(),
                    host.default_cwd.as_deref().unwrap_or("~")
                )),
                None => ControlError::bad_request(format!(
                    "repo not cloned locally — no known project has origin {origin}"
                )),
            })?;

        // Phase 1 (source agent still alive, everything retryable).
        let prepared = crate::migrate::prepare(
            &record.cwd,
            source_host.as_ref(),
            target_host.as_ref(),
            &target_repo,
            target_host
                .as_ref()
                .map(|host| host.display_name())
                .unwrap_or("local"),
        )
        .map_err(migrate_control_error)?;

        // Point of no return: stop the source agent.
        let mut warnings: Vec<String> = Vec::new();
        {
            let mut registry = self.registry.lock().map_err(poisoned)?;
            let _ = registry.terminate(&id, std::time::Duration::from_secs(3));
        }
        if let Some(source) = &source_host
            && let Some(warning) = crate::migrate::kill_remote_tmux(source, &id)
        {
            warnings.push(warning);
        }

        // Phase 2: transcript shuttle (source stopped ⇒ the jsonl is final).
        let shuttle = crate::migrate::shuttle_transcript(
            &record.cwd,
            record.transcript_path.as_deref(),
            record.agent_session_id.as_deref(),
            source_host.as_ref(),
            target_host.as_ref(),
            &prepared,
            &home,
        );
        if let Some(warning) = shuttle.warning.clone() {
            warnings.push(warning);
        }

        // Rewrite the record in place: same id/title/sidebar position, new
        // host + cwd.
        {
            let mut registry = self.registry.lock().map_err(poisoned)?;
            let target_id = target_host.as_ref().map(|host| host.id.clone());
            let branch = prepared.branch.clone();
            let cwd = prepared.target_repo_root.clone();
            let transcript = shuttle.local_target_path.clone();
            let local = target_host.is_none();
            registry.update_record(&id, |record| {
                record.host = target_id;
                record.cwd = cwd;
                record.worktree_path = None;
                record.git_branch = Some(branch);
                record.transcript_path = if local { transcript } else { None };
                record.status = diri_proto::SessionStatus::Exited(diri_proto::ExitInfo {
                    reason: diri_proto::ExitReason::Exited,
                    code: Some(0),
                    signal: None,
                });
                record.needs_input = None;
                record.hibernation = None;
                record.memory_bytes = None;
                record.listening_ports = None;
                record.resumability = diri_proto::Resumability::Resumable;
            });
            let _ = registry.persist();
            self.publish_updated(&registry, &id);
        }

        // Cutover: the normal resume path revives the conversation on the
        // target; without a transcript there is nothing to resume, so the
        // record is left revivable and the client's next open resumes fresh.
        let revived = self.session_resume(Some(json!({ "sessionID": id })))?;
        let session: diri_proto::SessionRecord = serde_json::from_value(revived)
            .map_err(|error| ControlError::internal(error.to_string()))?;
        encode(&diri_proto::SessionMigrateResult {
            session,
            transcript_migrated: shuttle.migrated,
            warning: (!warnings.is_empty()).then(|| warnings.join("; ")),
        })
    }

    /// `host.sync_prefs`: push the local agent preferences to a host so
    /// agents there behave like local ones. Additive rsync, fixed include
    /// list, per-tool reporting.
    fn host_sync_prefs(&self, params: Option<JsonValue>) -> Result<JsonValue, ControlError> {
        let p: diri_proto::HostSyncPrefsParams = decode(params)?;
        let entry = self.resolve_host(&p.host)?;
        let home = std::env::var("HOME")
            .map(PathBuf::from)
            .map_err(|_| ControlError::internal("HOME is not set"))?;
        encode(&crate::hosts::sync_prefs(&entry, &home))
    }

    /// `host.locate_repo`: find a checkout by origin URL (given directly, or
    /// derived from a session's cwd + host).
    fn host_locate_repo(&self, params: Option<JsonValue>) -> Result<JsonValue, ControlError> {
        let p: diri_proto::HostLocateRepoParams = decode(params)?;
        let target = p
            .host
            .as_deref()
            .map(|id| self.resolve_host(id))
            .transpose()?;

        let mut origin = p.origin_url.clone();
        if origin.is_none()
            && let Some(session_id) = &p.session_id
        {
            let (cwd, source_host) = {
                let registry = self.registry.lock().map_err(poisoned)?;
                let record = registry
                    .records()
                    .into_iter()
                    .find(|record| record.id.0 == session_id.0)
                    .ok_or_else(|| ControlError::not_found(session_id.0.clone()))?;
                (record.cwd, record.host)
            };
            let source = source_host
                .as_deref()
                .map(|id| self.resolve_host(id))
                .transpose()?;
            origin = crate::hosts::origin_of_cwd(&cwd, source.as_ref());
        }
        let Some(origin) = origin else {
            return encode(&diri_proto::HostLocateRepoResult {
                path: None,
                origin_url: None,
            });
        };

        let local_roots: Vec<String> = {
            let registry = self.registry.lock().map_err(poisoned)?;
            registry
                .projects_raw()
                .iter()
                .filter_map(|project| project.get("root").and_then(|value| value.as_str()))
                .map(str::to_string)
                .collect()
        };
        let path = crate::hosts::locate(&origin, target.as_ref(), &local_roots);
        encode(&diri_proto::HostLocateRepoResult {
            path,
            origin_url: Some(origin),
        })
    }

    /// Resolves a host id against `hosts.json`, read fresh each call so
    /// Settings edits apply without a daemon restart.
    fn resolve_host(&self, host_id: &str) -> Result<diri_proto::HostEntry, ControlError> {
        diri_proto::HostsConfig::load(self.hosts_file())
            .hosts
            .into_iter()
            .find(|entry| entry.id == host_id)
            .ok_or_else(|| {
                ControlError::bad_request(format!("unknown host {host_id:?}; check hosts.json"))
            })
    }

    fn hosts_file(&self) -> PathBuf {
        self.socket_path
            .parent()
            .map(|parent| parent.join("hosts.json"))
            .unwrap_or_else(|| PathBuf::from("hosts.json"))
    }

    /// `session.list` and `state.snapshot` are the same view: every record
    /// plus the project list, exactly as the Swift daemon answers them.
    fn session_list(&self) -> Result<JsonValue, ControlError> {
        let registry = self.registry.lock().map_err(poisoned)?;
        serde_json::to_value(json!({
            "sessions": registry.records(),
            "projects": registry.projects_raw(),
        }))
        .map_err(|error| ControlError::internal(error.to_string()))
    }

    fn session_send_text(&self, params: Option<JsonValue>) -> Result<JsonValue, ControlError> {
        let p: diri_proto::SendTextParams = decode(params)?;
        let mut registry = self.registry.lock().map_err(poisoned)?;
        // Typing into a hibernated session wakes it; the text is queued and
        // flushed after SIGCONT, so no keystroke is lost.
        let _ = registry.wake_session(&p.session_id.0);
        self.publish_updated(&registry, &p.session_id.0);
        let session = registry
            .get(&p.session_id.0)
            .ok_or_else(|| ControlError::not_found(p.session_id.0.clone()))?;
        session
            .send_text(&p.text, p.submit)
            .map_err(|error| ControlError::internal(error.to_string()))?;
        Ok(json!({}))
    }

    fn session_resize(&self, params: Option<JsonValue>) -> Result<JsonValue, ControlError> {
        let p: diri_proto::ResizeParams = decode(params)?;
        let cols = u16::try_from(p.cols.clamp(2, u16::MAX as i64)).expect("clamped");
        let rows = u16::try_from(p.rows.clamp(2, u16::MAX as i64)).expect("clamped");
        let registry = self.registry.lock().map_err(poisoned)?;
        let session = registry
            .get(&p.session_id.0)
            .ok_or_else(|| ControlError::not_found(p.session_id.0.clone()))?;
        session
            .resize(cols, rows)
            .map_err(|error| ControlError::internal(error.to_string()))?;
        Ok(json!({}))
    }

    fn session_read_screen(&self, params: Option<JsonValue>) -> Result<JsonValue, ControlError> {
        let p: diri_proto::SessionIdParams = decode(params)?;
        let registry = self.registry.lock().map_err(poisoned)?;
        let session = registry
            .get(&p.session_id.0)
            .ok_or_else(|| ControlError::not_found(p.session_id.0.clone()))?;
        let (cols, rows) = session.screen_size();
        encode(&diri_proto::ReadScreenResult {
            text: session.screen_lines().join("\n"),
            cols: cols as i64,
            rows: rows as i64,
        })
    }

    fn session_read_scrollback(
        &self,
        params: Option<JsonValue>,
    ) -> Result<JsonValue, ControlError> {
        let p: diri_proto::SessionIdParams = decode(params)?;
        let registry = self.registry.lock().map_err(poisoned)?;
        let session = registry
            .get(&p.session_id.0)
            .ok_or_else(|| ControlError::not_found(p.session_id.0.clone()))?;
        encode(&session.read_scrollback())
    }

    fn session_read_scrollback_cells(
        &self,
        params: Option<JsonValue>,
    ) -> Result<JsonValue, ControlError> {
        let p: diri_proto::ReadScrollbackCellsParams = decode(params)?;
        let registry = self.registry.lock().map_err(poisoned)?;
        let session = registry
            .get(&p.session_id.0)
            .ok_or_else(|| ControlError::not_found(p.session_id.0.clone()))?;
        encode(&session.read_scrollback_cells(p.first_row, p.max_rows))
    }

    fn session_kill(&self, params: Option<JsonValue>) -> Result<JsonValue, ControlError> {
        let p: diri_proto::SessionIdParams = decode(params)?;
        let mut registry = self.registry.lock().map_err(poisoned)?;
        let exit = registry
            .terminate(&p.session_id.0, std::time::Duration::from_secs(3))
            .map_err(|error| ControlError::internal(error.to_string()))?;
        if exit.is_none() {
            return Err(ControlError::not_found(p.session_id.0.clone()));
        }
        let _ = registry.persist();
        self.publish_updated(&registry, &p.session_id.0);
        Ok(json!({}))
    }

    fn session_remove(&self, params: Option<JsonValue>) -> Result<JsonValue, ControlError> {
        let p: diri_proto::SessionIdParams = decode(params)?;
        let mut registry = self.registry.lock().map_err(poisoned)?;
        registry
            .remove(&p.session_id.0, &self.logs_dir)
            .map_err(io_control_error)?;
        let _ = registry.persist();
        self.events.publish(
            diri_proto::EventName::SESSION_REMOVED,
            json!({ "id": p.session_id.0, "reason": "released" }),
            Some(&p.session_id.0),
        );
        Ok(json!({}))
    }

    fn session_rename(&self, params: Option<JsonValue>) -> Result<JsonValue, ControlError> {
        let p: diri_proto::SessionRenameParams = decode(params)?;
        let mut registry = self.registry.lock().map_err(poisoned)?;
        registry
            .rename(&p.session_id.0, &p.title)
            .map_err(io_control_error)?;
        let _ = registry.persist();
        self.publish_updated(&registry, &p.session_id.0);
        Ok(json!({}))
    }

    fn session_mark_seen(&self, params: Option<JsonValue>) -> Result<JsonValue, ControlError> {
        let p: diri_proto::SessionIdParams = decode(params)?;
        let mut registry = self.registry.lock().map_err(poisoned)?;
        registry
            .mark_seen(&p.session_id.0)
            .map_err(io_control_error)?;
        let _ = registry.persist();
        self.publish_updated(&registry, &p.session_id.0);
        self.pr_monitor_wake.wake_session(p.session_id.0);
        Ok(json!({}))
    }

    fn client_set_active(&self, params: Option<JsonValue>) -> Result<JsonValue, ControlError> {
        let p: diri_proto::ClientActiveParams = decode(params)?;
        self.pr_monitor_wake.set_foreground_active(p.active);
        Ok(json!({}))
    }

    fn session_archive(&self, params: Option<JsonValue>) -> Result<JsonValue, ControlError> {
        let p: diri_proto::SessionIdParams = decode(params)?;
        let mut registry = self.registry.lock().map_err(poisoned)?;
        registry
            .archive(&p.session_id.0)
            .map_err(io_control_error)?;
        let _ = registry.persist();
        self.publish_updated(&registry, &p.session_id.0);
        Ok(json!({}))
    }

    fn session_unarchive(&self, params: Option<JsonValue>) -> Result<JsonValue, ControlError> {
        let p: diri_proto::SessionIdParams = decode(params)?;
        let mut registry = self.registry.lock().map_err(poisoned)?;
        registry
            .unarchive(&p.session_id.0)
            .map_err(io_control_error)?;
        let _ = registry.persist();
        self.publish_updated(&registry, &p.session_id.0);
        Ok(json!({}))
    }

    /// A hook or notify callback from inside an agent session: the signal
    /// that makes hook-authority agents' status precise. Parsed by the same
    /// rules the Swift daemon used, metadata folded into the record, signal
    /// fed to the session's reducer.
    fn hook_report(&self, params: Option<JsonValue>) -> Result<JsonValue, ControlError> {
        let p: diri_proto::HookReportParams = decode(params)?;
        let Some(session_id) = p.dirijor_session_id else {
            return Ok(json!({}));
        };
        let parsed = match p.kind.as_str() {
            "claude-hook" => p.event.as_deref().and_then(|event| {
                crate::hooks::parse_claude_hook(event, &p.payload, std::time::SystemTime::now())
            }),
            "codex-notify" => crate::hooks::parse_codex_notify(&p.payload),
            _ => None,
        };
        let Some((signal, meta)) = parsed else {
            return Ok(json!({}));
        };
        let mut registry = self.registry.lock().map_err(poisoned)?;
        let changed = registry.apply_hook_metadata(&session_id.0, &meta);
        if let Some(session) = registry.get(&session_id.0) {
            session.feed_signal(signal);
        }
        if changed {
            let _ = registry.persist();
        }
        self.publish_updated(&registry, &session_id.0);
        Ok(json!({}))
    }

    /// Revives an exited session's conversation under the SAME record id.
    fn session_resume(&self, params: Option<JsonValue>) -> Result<JsonValue, ControlError> {
        let p: diri_proto::SessionIdParams = decode(params)?;
        let mut registry = self.registry.lock().map_err(poisoned)?;
        let record = registry
            .records()
            .into_iter()
            .find(|record| record.id.0 == p.session_id.0)
            .ok_or_else(|| ControlError::not_found(p.session_id.0.clone()))?;
        if registry.get(&p.session_id.0).is_some() {
            // Already live: resuming is a no-op, not an error.
            return serde_json::to_value(&record)
                .map_err(|error| ControlError::internal(error.to_string()));
        }
        if let Some(host_id) = record.host.clone() {
            // Remote revive: the same tmux name reattaches the live remote
            // session when it survived, else a fresh agent resumes the
            // conversation from the remote-side transcript.
            drop(registry);
            return self.session_resume_remote(&record, &host_id);
        }
        let spec = self.resume_spec(
            &registry,
            &record.id.0,
            record.kind.id(),
            &record.cwd,
            record.agent_session_id.as_deref(),
        )?;
        registry
            .respawn(spec)
            .map_err(|error| ControlError::internal(error.to_string()))?;
        let _ = registry.persist();
        self.publish_updated(&registry, &p.session_id.0);
        let record = registry
            .records()
            .into_iter()
            .find(|record| record.id.0 == p.session_id.0)
            .ok_or_else(|| ControlError::internal("the resumed session vanished"))?;
        serde_json::to_value(&record).map_err(|error| ControlError::internal(error.to_string()))
    }

    /// Revives a conversation found in an agent's own history: a NEW record
    /// whose agent-side id is the transcript's.
    fn session_resume_from_history(
        &self,
        params: Option<JsonValue>,
    ) -> Result<JsonValue, ControlError> {
        let p: diri_proto::ResumeFromHistoryParams = decode(params)?;
        let mut registry = self.registry.lock().map_err(poisoned)?;
        let id = next_session_id();
        let kind = p.entry.kind.id().to_string();
        let mut record = new_record(&id, &kind, &p.entry.cwd);
        record.agent_session_id = Some(p.entry.id.clone());
        record.transcript_path = Some(p.entry.transcript_path.clone());
        if let Some(title) = &p.entry.title {
            record.title = title.clone();
            record.title_source = diri_proto::TitleSource::FirstPrompt;
        }
        let spec = self.resume_spec(&registry, &id, &kind, &p.entry.cwd, Some(&p.entry.id))?;
        registry
            .spawn(spec, record)
            .map_err(|error| ControlError::internal(error.to_string()))?;
        let _ = registry.persist();
        self.publish_updated(&registry, &id);
        let record = registry
            .records()
            .into_iter()
            .find(|record| record.id.0 == id)
            .ok_or_else(|| ControlError::internal("the resumed session vanished"))?;
        serde_json::to_value(&record).map_err(|error| ControlError::internal(error.to_string()))
    }

    /// The spawn spec that re-enters a conversation: the manifest's resume
    /// argv plus the same hook/MCP wiring a fresh spawn gets — a resumed
    /// Claude must not silently lose status detection or the dirijor tools.
    fn resume_spec(
        &self,
        registry: &Registry,
        id: &str,
        kind: &str,
        cwd: &str,
        agent_session_id: Option<&str>,
    ) -> Result<crate::session::SessionSpec, ControlError> {
        let engine = registry.engine();
        let manifest = engine
            .manifest(kind)
            .ok_or_else(|| ControlError::not_found(format!("no manifest for agent {kind}")))?;
        let descriptor = manifest.agent.clone().unwrap_or_default();
        descriptor
            .binary
            .as_ref()
            .ok_or_else(|| ControlError::bad_request(format!("agent {kind} declares no binary")))?;
        let tail = descriptor.resume_args(agent_session_id).ok_or_else(|| {
            ControlError::bad_request(format!("agent {kind} does not support resume"))
        })?;

        let mut launch_args = descriptor.spawn_args.clone();
        launch_args.extend(tail);
        if let Some(injection) = &self.injection {
            // Only the appendable flag mechanisms replay on resume, exactly
            // as in Swift: Codex's global `-c` overrides must precede the
            // resume SUBCOMMAND and are deliberately not replayed.
            let claude_only = crate::agent::InjectionSpec {
                claude_hooks: descriptor.injection.claude_hooks,
                claude_mcp: descriptor.injection.claude_mcp,
                ..Default::default()
            };
            launch_args.extend(crate::inject::injection_args(
                &claude_only,
                &injection.inject_dir,
                &injection.cli_path,
            ));
        }

        let inherited: Vec<(String, String)> = std::env::vars().collect();
        let mut pty = descriptor
            .spawn_spec(Path::new(cwd), inherited, &launch_args)
            .ok_or_else(|| ControlError::internal("resume spec without a binary"))?;
        if let Some(injection) = &self.injection {
            pty.env
                .push((crate::inject::SESSION_ID_ENV.into(), id.to_string()));
            pty.env.push((
                crate::inject::SOCKET_ENV.into(),
                self.socket_path.to_string_lossy().into_owned(),
            ));
            pty.env.push((
                crate::inject::CLI_ENV.into(),
                injection.cli_path.to_string_lossy().into_owned(),
            ));
        }
        Ok(crate::session::SessionSpec {
            id: id.to_string(),
            pty,
            manifest_id: kind.to_string(),
            authority: descriptor.authority(),
            logs_dir: self.logs_dir.clone(),
            holder: self.holder.clone(),
            defer_launch: true,
        })
    }

    /// Pops the most recently closed session whose folder still exists and
    /// re-lists it (exited), ready for the resume path.
    fn session_reopen_last(&self) -> Result<JsonValue, ControlError> {
        let mut registry = self.registry.lock().map_err(poisoned)?;
        let record = registry
            .reopen_last_closed()
            .ok_or_else(|| ControlError::bad_request("no recently closed session"))?;
        let _ = registry.persist();
        self.publish_updated(&registry, &record.id.0);
        serde_json::to_value(&record).map_err(|error| ControlError::internal(error.to_string()))
    }

    /// Which agent binaries actually resolve, plus each manifest's descriptor
    /// — this doubles as the agent catalog the client's picker renders.
    fn agent_readiness(&self) -> Result<JsonValue, ControlError> {
        let registry = self.registry.lock().map_err(poisoned)?;
        let engine = registry.engine();
        let mut agents = Vec::new();
        for id in engine.ids() {
            let Some(manifest) = engine.manifest(id) else {
                continue;
            };
            let Some(descriptor) = &manifest.agent else {
                continue;
            };
            let Some(binary) = &descriptor.binary else {
                continue;
            };
            agents.push(json!({
                "kind": id,
                "binary": binary,
                "path": resolve_on_path(binary),
                "descriptor": engine.raw_agent(id),
            }));
        }
        Ok(json!({ "agents": agents }))
    }

    fn project_add(&self, params: Option<JsonValue>) -> Result<JsonValue, ControlError> {
        let p: diri_proto::ProjectAddParams = decode(params)?;
        let mut registry = self.registry.lock().map_err(poisoned)?;
        let project = registry.add_project(&p.root);
        let _ = registry.persist();
        Ok(project)
    }

    /// The working tree's diff against a base ref, for the app's diff pane.
    fn session_read_diff(&self, params: Option<JsonValue>) -> Result<JsonValue, ControlError> {
        let p: diri_proto::SessionReadDiffParams = decode(params)?;
        let cwd = {
            let registry = self.registry.lock().map_err(poisoned)?;
            registry
                .records()
                .into_iter()
                .find(|record| record.id.0 == p.session_id.0)
                .map(|record| record.cwd)
                .ok_or_else(|| ControlError::not_found(p.session_id.0.clone()))?
        };
        let result =
            crate::git::working_diff(Path::new(&cwd), p.base.as_ref()).map_err(io_control_error)?;
        encode(&result)
    }

    /// SIGSTOPs the session's whole tree and records it as hibernated. The
    /// PTY and holder stay alive; wake is one SIGCONT away.
    /// Updates the two governor tunables the app exposes; the rest keep the
    /// Swift defaults. Applies on the governor's next sweep.
    fn governor_configure(&self, params: Option<JsonValue>) -> Result<JsonValue, ControlError> {
        let p: diri_proto::GovernorSettingsParams = decode(params)?;
        let mut config = self.governor.lock().map_err(poisoned)?;
        config.idle_threshold_seconds = p.idle_threshold_seconds.max(0.0);
        config.hard_memory_bytes = p.hard_memory_bytes;
        Ok(json!({}))
    }

    fn session_hibernate(&self, params: Option<JsonValue>) -> Result<JsonValue, ControlError> {
        let p: diri_proto::SessionIdParams = decode(params)?;
        let mut registry = self.registry.lock().map_err(poisoned)?;
        registry
            .hibernate(&p.session_id.0, diri_proto::HibernationReason::Manual)
            .map_err(io_control_error)?;
        let _ = registry.persist();
        self.publish_updated(&registry, &p.session_id.0);
        Ok(json!({}))
    }

    fn session_wake(&self, params: Option<JsonValue>) -> Result<JsonValue, ControlError> {
        let p: diri_proto::SessionIdParams = decode(params)?;
        let mut registry = self.registry.lock().map_err(poisoned)?;
        registry
            .wake_session(&p.session_id.0)
            .map_err(|error| ControlError::internal(error.to_string()))?;
        let _ = registry.persist();
        self.publish_updated(&registry, &p.session_id.0);
        Ok(json!({}))
    }

    fn daemon_prepare_shutdown(&self) -> Result<JsonValue, ControlError> {
        let mut registry = self.registry.lock().map_err(poisoned)?;
        let _ = registry.persist();
        Ok(json!({}))
    }

    /// Ack first, then exit: the response has to flush before the process
    /// dies, so the client sees a clean reply followed by a socket drop and
    /// relaunches the fresh binary.
    fn daemon_shutdown(&self) -> Result<JsonValue, ControlError> {
        {
            let mut registry = self.registry.lock().map_err(poisoned)?;
            let _ = registry.persist();
        }
        std::thread::spawn(|| {
            std::thread::sleep(std::time::Duration::from_millis(200));
            std::process::exit(0);
        });
        Ok(json!({}))
    }

    /// Publishes `session.updated` with the session's current record.
    fn publish_updated(&self, registry: &Registry, id: &str) {
        if let Some(record) = registry
            .records()
            .into_iter()
            .find(|record| record.id.0 == id)
        {
            self.events
                .publish_encoded(diri_proto::EventName::SESSION_UPDATED, &record, Some(id));
        }
    }

    /// Resumable past conversations from the agents' own transcript stores,
    /// excluding ones already represented by live records.
    fn session_history(&self) -> Result<JsonValue, ControlError> {
        let tracked = {
            let registry = self.registry.lock().map_err(poisoned)?;
            registry.tracked_agent_session_ids()
        };
        let home = std::env::var("HOME")
            .map(PathBuf::from)
            .map_err(|_| ControlError::internal("HOME is not set"))?;
        let entries: Vec<diri_proto::HistoryEntry> = crate::history::scan(&home, &tracked)
            .into_iter()
            .map(history_entry_to_wire)
            .collect();
        encode(&diri_proto::SessionHistoryResult { entries })
    }

    fn worktree_create(&self, params: Option<JsonValue>) -> Result<JsonValue, ControlError> {
        let p: diri_proto::WorktreeCreateParams = decode(params)?;
        let info = crate::git::create_worktree(
            Path::new(&p.repo_path),
            p.branch.as_deref(),
            p.base.as_deref(),
        )
        .map_err(io_control_error)?;
        self.events.publish(
            "worktree.created",
            json!({ "repoPath": p.repo_path, "path": info.path, "branch": info.branch }),
            None,
        );
        encode(&worktree_to_wire(info))
    }

    fn worktree_list(&self, params: Option<JsonValue>) -> Result<JsonValue, ControlError> {
        let p: diri_proto::WorktreeListParams = decode(params)?;
        let list = crate::git::list_worktrees(Path::new(&p.repo_path)).map_err(io_control_error)?;
        encode(&list.into_iter().map(worktree_to_wire).collect::<Vec<_>>())
    }

    fn worktree_remove(&self, params: Option<JsonValue>) -> Result<JsonValue, ControlError> {
        let p: diri_proto::WorktreeRemoveParams = decode(params)?;
        crate::git::remove_worktree(Path::new(&p.repo_path), &p.worktree_path, p.force)
            .map_err(io_control_error)?;
        self.events.publish(
            "worktree.removed",
            json!({ "repoPath": p.repo_path, "path": p.worktree_path }),
            None,
        );
        Ok(json!({}))
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }
}

impl Drop for ControlServer {
    fn drop(&mut self) {
        // Leaving the socket file behind would make the next start think a
        // daemon is already running.
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

/// A session id in the daemon's format: `s_` plus twelve hex digits.
pub(crate) fn next_session_id() -> String {
    let mut bytes = [0u8; 6];
    getrandom::fill(&mut bytes).expect("the OS random source");
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("s_{hex}")
}

pub(crate) fn new_record(id: &str, kind: &str, cwd: &str) -> diri_proto::SessionRecord {
    use diri_proto::{AgentKind, DateMillis, ProjectId, Resumability, SessionId, TitleSource};
    let now: DateMillis = std::time::SystemTime::now().into();
    diri_proto::SessionRecord {
        id: SessionId(id.to_string()),
        kind: AgentKind::new(kind),
        cwd: cwd.to_string(),
        project_id: ProjectId(cwd.to_string()),
        worktree_path: None,
        git_branch: None,
        title: kind.to_string(),
        title_source: TitleSource::Placeholder,
        agent_session_id: None,
        transcript_path: None,
        status: diri_proto::SessionStatus::Starting,
        needs_input: None,
        resumability: Resumability::Live,
        parent: None,
        created_at: now,
        updated_at: now,
        last_turn_completed_at: None,
        last_seen_at: None,
        pinned: false,
        archived_at: None,
        remote_active: false,
        host: None,
        hibernation: None,
        memory_bytes: None,
        artifacts: None,
        pull_requests: None,
        listening_ports: None,
        foreground_agent: None,
    }
}

/// A connection's live event subscription: stopping it ends the forwarder,
/// whose stream-drop unsubscribes from the bus.
struct SubscriptionHandle {
    stop: Arc<std::sync::atomic::AtomicBool>,
    _thread: std::thread::JoinHandle<()>,
}

/// Serializes one message onto the shared write half. Responses and event
/// frames interleave here; the mutex keeps each line whole.
fn write_message(writer: &Arc<Mutex<UnixStream>>, message: &ControlMessage) -> std::io::Result<()> {
    let mut bytes = serde_json::to_vec(message)?;
    bytes.push(b'\n');
    let mut stream = writer
        .lock()
        .map_err(|_| std::io::Error::other("writer poisoned"))?;
    stream.write_all(&bytes)?;
    stream.flush()
}

fn poisoned<T>(_: T) -> ControlError {
    ControlError::internal("engine state is poisoned")
}

/// Decodes params into the shared `diri-proto` type for the method — the same
/// types the app itself serializes, so a shape drift is a compile error, not
/// a wire bug.
fn decode<T: serde::de::DeserializeOwned>(params: Option<JsonValue>) -> Result<T, ControlError> {
    serde_json::from_value(params.unwrap_or_else(|| json!({})))
        .map_err(|error| ControlError::bad_request(error.to_string()))
}

fn encode<T: serde::Serialize>(value: &T) -> Result<JsonValue, ControlError> {
    serde_json::to_value(value).map_err(|error| ControlError::internal(error.to_string()))
}

/// Resolves a binary on the daemon's PATH, as the readiness check needs.
fn resolve_on_path(binary: &str) -> Option<String> {
    if binary.contains('/') {
        return Path::new(binary).exists().then(|| binary.to_string());
    }
    let path = std::env::var("PATH").ok()?;
    for dir in path.split(':') {
        let candidate = Path::new(dir).join(binary);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if std::fs::metadata(&candidate)
                .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
                .unwrap_or(false)
            {
                return Some(candidate.to_string_lossy().into_owned());
            }
        }
        #[cfg(not(unix))]
        {
            if candidate.is_file() {
                return Some(candidate.to_string_lossy().into_owned());
            }
        }
    }
    None
}

/// Resolves a spec's bare argv[0] to an absolute path against its own env
/// PATH (daemon PATH as fallback). The process that execs it may be a
/// long-lived holder manager whose environment predates this daemon —
/// program lookup happens THERE, so a bare "ssh" can exit 127 no matter
/// what env the spec carries.
pub(crate) fn absolutize_remote_argv0(pty: &mut crate::pty::PtySpec) {
    // The daemon itself has no terminal (launchd env): without an asserted
    // TERM, `ssh -t` hands the remote an empty one and tmux refuses to start
    // ("terminal does not support clear") — the same assertion spawn_spec
    // makes for local agents.
    pty.env
        .retain(|(key, _)| key != "TERM" && key != "COLORTERM");
    pty.env.push(("TERM".into(), "xterm-256color".into()));
    pty.env.push(("COLORTERM".into(), "truecolor".into()));
    absolutize_argv0(pty);
}

fn absolutize_argv0(pty: &mut crate::pty::PtySpec) {
    if let Some(first) = pty.argv.first_mut()
        && !first.contains('/')
    {
        let path = pty
            .env
            .iter()
            .rev()
            .find(|(key, _)| key == "PATH")
            .map(|(_, value)| value.clone())
            .or_else(|| std::env::var("PATH").ok());
        if let Some(resolved) = path
            .as_deref()
            .and_then(|path| crate::agent::resolve_on_path(first, path))
        {
            *first = resolved;
        }
    }
}

fn migrate_control_error(error: crate::migrate::MigrateError) -> ControlError {
    match error {
        crate::migrate::MigrateError::BadRequest(message) => ControlError::bad_request(message),
        crate::migrate::MigrateError::Internal(message) => ControlError::internal(message),
    }
}

fn io_control_error(error: std::io::Error) -> ControlError {
    match error.kind() {
        std::io::ErrorKind::NotFound => ControlError::not_found(error.to_string()),
        _ => ControlError::internal(error.to_string()),
    }
}

fn history_entry_to_wire(entry: crate::history::HistoryEntry) -> diri_proto::HistoryEntry {
    diri_proto::HistoryEntry {
        id: entry.id,
        kind: match entry.kind {
            crate::history::HistoryKind::ClaudeCode => diri_proto::AgentKind::CLAUDE_CODE,
            crate::history::HistoryKind::Codex => diri_proto::AgentKind::CODEX,
        },
        cwd: entry.cwd,
        title: entry.title,
        transcript_path: entry.transcript_path,
        last_active_at: diri_proto::DateMillis::from(entry.last_active_at),
        created_at: entry.created_at.map(diri_proto::DateMillis::from),
        cwd_exists: entry.cwd_exists,
    }
}

fn worktree_to_wire(info: crate::git::WorktreeInfo) -> diri_proto::WorktreeInfo {
    diri_proto::WorktreeInfo {
        path: info.path,
        branch: info.branch,
        is_bare: info.is_bare,
        is_detached: info.is_detached,
        is_prunable: info.is_prunable,
    }
}

/// Reads one fact about a live session under a short registry lock; `None`
/// once the session is gone. The injection thread must never hold the lock
/// across its sleeps.
fn with_session<T>(
    registry: &Arc<Mutex<Registry>>,
    session_id: &str,
    read: impl FnOnce(&crate::session::Session) -> T,
) -> Option<T> {
    registry
        .lock()
        .ok()
        .and_then(|guard| guard.get(session_id).map(read))
}

/// Types an initial prompt into a freshly spawned agent, gated on the TUI
/// actually being ready and verified afterward. Ported from the Swift
/// `AgentSession.injectInitialPrompt`: up to three attempts, each abandoned
/// only when the screen shows no evidence at all that input landed — a
/// changed screen means it did, and a second submit would duplicate it.
fn inject_initial_prompt(registry: &Arc<Mutex<Registry>>, session_id: &str, prompt: &str) {
    if !wait_until_ready(registry, session_id) {
        return;
    }
    let probe = verification_probe(prompt);
    for _attempt in 0..3 {
        let Some(view) = with_session(registry, session_id, |session| session.view()) else {
            return;
        };
        if view.exited {
            return;
        }
        let Some(before) = with_session(registry, session_id, |session| {
            session.screen_lines().join("\n")
        }) else {
            return;
        };
        let sent = with_session(registry, session_id, |session| {
            session.send_text(prompt, true)
        });
        if sent.is_none() {
            return;
        }
        if prompt_settled(registry, session_id, &probe, &before) {
            return;
        }
    }
}

/// Waits until the agent can actually receive typed input. First for the
/// exec (a deferred launch fires within its fallback window), then for the
/// input line to come alive — bracketed-paste mode is the tell across
/// Claude/Codex/Cursor/Gemini. Falls back to "screen non-blank and settled"
/// for agents that never enable paste mode, and hard-caps the wait. False
/// means stop: the session exited or vanished.
fn wait_until_ready(registry: &Arc<Mutex<Registry>>, session_id: &str) -> bool {
    for _ in 0..40 {
        // ≤ ~4s for the PTY to be spawned (deferred launch included).
        match with_session(registry, session_id, |session| {
            (session.view().exited, session.child_pid())
        }) {
            None | Some((true, _)) => return false,
            Some((false, pid)) if pid > 0 => break,
            Some(_) => std::thread::sleep(Duration::from_millis(100)),
        }
    }
    let mut last_text = String::new();
    let mut stable_ticks = 0;
    for tick in 0..200 {
        // ≤ ~20s hard cap; Claude's first paint can be slow.
        let Some((exited, paste, text)) = with_session(registry, session_id, |session| {
            (
                session.view().exited,
                session.bracketed_paste(),
                session.screen_lines().join("\n"),
            )
        }) else {
            return false;
        };
        if exited {
            return false;
        }
        if paste {
            // One more frame so the composer finishes painting.
            std::thread::sleep(Duration::from_millis(80));
            return true;
        }
        if !text.trim().is_empty() && text == last_text {
            stable_ticks += 1;
            if stable_ticks >= 6 && tick >= 10 {
                return true; // ~600ms stable, at least ~1s in
            }
        } else {
            stable_ticks = 0;
            last_text = text;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    true
}

/// Polls the screen (≤ ~2s) for evidence the prompt was received. True as
/// soon as the probe is visible OR the screen diverged from `before` —
/// either means input landed, and a retry would duplicate the prompt. Only
/// an entirely unchanged screen returns false → safe to retype.
fn prompt_settled(
    registry: &Arc<Mutex<Registry>>,
    session_id: &str,
    probe: &str,
    before: &str,
) -> bool {
    for _ in 0..20 {
        std::thread::sleep(Duration::from_millis(100));
        let Some((exited, now)) = with_session(registry, session_id, |session| {
            (session.view().exited, session.screen_lines().join("\n"))
        }) else {
            return true; // session gone: don't retype into it
        };
        if exited {
            return true; // dead pty: don't retype into it
        }
        if !probe.is_empty() && now.contains(probe) {
            return true;
        }
        if now != before {
            return true;
        }
    }
    false
}

/// A distinctive slice of the prompt to look for on screen: the first
/// non-empty line, trimmed and capped short enough to survive composer
/// wrapping or a transcript truncating a long prompt when it echoes back.
fn verification_probe(prompt: &str) -> String {
    let first_line = prompt
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or(prompt);
    first_line.trim().chars().take(24).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::detect::ManifestEngine;

    fn engine() -> Arc<ManifestEngine> {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../Sources/DirijorCore/Resources/manifests")
            .canonicalize()
            .expect("manifests");
        let (engine, _) = ManifestEngine::load_dir(&dir).expect("load");
        Arc::new(engine)
    }

    fn server(temp: &Path) -> ControlServer {
        let registry = Registry::new(engine(), temp.join("state.json"));
        ControlServer::new(Arc::new(Mutex::new(registry)), temp.join("daemon.sock"))
    }

    fn test_record(id: &str) -> diri_proto::SessionRecord {
        use diri_proto::*;
        SessionRecord {
            id: SessionId(id.into()),
            kind: AgentKind::SHELL,
            cwd: "/tmp".into(),
            project_id: ProjectId("p".into()),
            worktree_path: None,
            git_branch: None,
            title: "test".into(),
            title_source: TitleSource::Placeholder,
            agent_session_id: None,
            transcript_path: None,
            status: SessionStatus::Idle,
            needs_input: None,
            resumability: Resumability::NotResumable,
            parent: None,
            created_at: DateMillis(0.0),
            updated_at: DateMillis(0.0),
            last_turn_completed_at: None,
            last_seen_at: None,
            pinned: false,
            archived_at: None,
            remote_active: false,
            host: None,
            hibernation: None,
            memory_bytes: None,
            artifacts: None,
            pull_requests: None,
            listening_ports: None,
            foreground_agent: None,
        }
    }

    /// Round-trips one request through the dispatcher the way a client would.
    /// Dispatches one line the way `serve` would, with a throwaway socket
    /// standing in for the connection's write half.
    fn handle(server: &ControlServer, line: &[u8]) -> Option<ControlMessage> {
        let (writer, _peer) = UnixStream::pair().expect("socketpair");
        server.handle_line(line, &Arc::new(Mutex::new(writer)), &mut None)
    }

    fn call(server: &ControlServer, method: &str, params: Option<JsonValue>) -> ControlMessage {
        let request = ControlMessage::Request {
            id: 1,
            method: method.into(),
            params,
        };
        let line = serde_json::to_vec(&request).expect("encode");
        handle(server, &line).expect("a request gets a response")
    }

    fn ok_of(message: ControlMessage) -> JsonValue {
        match message {
            ControlMessage::Response { result: Ok(ok), .. } => ok,
            other => panic!("expected success, got {other:?}"),
        }
    }

    fn err_of(message: ControlMessage) -> ControlError {
        match message {
            ControlMessage::Response {
                result: Err(error), ..
            } => error,
            other => panic!("expected an error, got {other:?}"),
        }
    }

    #[test]
    fn hello_reports_the_protocol_and_the_engine_build() {
        let temp = tempfile::tempdir().expect("temp");
        let server = server(temp.path());
        let result = ok_of(call(
            &server,
            "hello",
            Some(json!({ "proto": WIRE_VERSION, "build": "test-client" })),
        ));

        assert_eq!(result["proto"], WIRE_VERSION);
        assert!(
            result["build"]
                .as_str()
                .is_some_and(|b| b.contains("diri-engine")),
            "the handshake should say which engine answered: {result}"
        );
        assert!(result["pid"].as_i64().is_some_and(|pid| pid > 0));
    }

    #[test]
    fn client_activity_drives_pr_monitor_visibility() {
        let temp = tempfile::tempdir().expect("temp");
        let server = server(temp.path());
        assert!(server.pr_monitor_wake().foreground_active());

        let _ = ok_of(call(
            &server,
            diri_proto::Method::CLIENT_SET_ACTIVE,
            Some(json!({ "active": false })),
        ));
        assert!(!server.pr_monitor_wake().foreground_active());
    }

    #[test]
    fn a_client_on_another_protocol_is_told_so() {
        let temp = tempfile::tempdir().expect("temp");
        let server = server(temp.path());
        let error = err_of(call(
            &server,
            "hello",
            Some(json!({ "proto": 99, "build": "future-client" })),
        ));
        assert_eq!(error.code, "version_mismatch");
    }

    #[test]
    fn the_claude_manifest_declares_its_injection_mechanisms() {
        // The spawn path reads these; a manifest-parsing regression would
        // silently ship screen-detected Claudes with no MCP tools.
        let engine = engine();
        let manifest = engine.manifest("claude-code").expect("claude manifest");
        let descriptor = manifest.agent.clone().expect("agent");
        assert!(descriptor.injection.claude_hooks);
        assert!(descriptor.injection.claude_mcp);
        assert!(descriptor.session_id_flag.is_some());

        let codex = engine.manifest("codex").expect("codex manifest");
        let codex_descriptor = codex.agent.clone().expect("agent");
        assert!(
            codex_descriptor.injection.codex_notify || codex_descriptor.injection.codex_mcp,
            "codex opts into at least one shim"
        );
    }

    #[test]
    fn resuming_an_agent_keeps_the_login_shell_as_session_leader() {
        let temp = tempfile::tempdir().expect("temp");
        let registry = Registry::new(engine(), temp.path().join("state.json"));
        let server = ControlServer::new(
            Arc::new(Mutex::new(Registry::new(
                engine(),
                temp.path().join("server-state.json"),
            ))),
            temp.path().join("daemon.sock"),
        );

        let spec = server
            .resume_spec(&registry, "s_resume", "claude-code", "/tmp", Some("uuid-1"))
            .expect("resume spec");
        assert_eq!(&spec.pty.argv[1..4], &["-i", "-l", "-c"]);
        let command = &spec.pty.argv[4];
        assert!(
            command.contains("'claude' '--resume' 'uuid-1'"),
            "the complete resume command must run inside the shell: {command}"
        );
        assert!(
            command.contains("; exec "),
            "the shell must survive: {command}"
        );
    }

    #[test]
    fn listing_sessions_returns_records_and_projects() {
        // The app decodes SessionListResult { sessions, projects }; both keys
        // must be present, as the Swift daemon answers.
        let temp = tempfile::tempdir().expect("temp");
        let server = server(temp.path());
        let result = ok_of(call(&server, "session.list", None));
        assert!(result["sessions"].is_array());
        assert!(result["projects"].is_array());
        // state.snapshot is the same view under another name.
        let snapshot = ok_of(call(&server, "state.snapshot", None));
        assert!(snapshot["sessions"].is_array());
    }

    #[test]
    fn an_unimplemented_method_is_not_found_rather_than_a_dropped_connection() {
        // A client that asks for something this engine has not ported yet must
        // get a clean error, the same as an older daemon would give.
        let temp = tempfile::tempdir().expect("temp");
        let server = server(temp.path());
        let error = err_of(call(&server, "session.never_implemented", Some(json!({}))));
        assert_eq!(error.code, "not_found");
    }

    #[test]
    fn addressing_a_session_that_does_not_exist_is_an_error() {
        // Params use the wire spelling the app sends: `sessionID`, not `id`.
        let temp = tempfile::tempdir().expect("temp");
        let server = server(temp.path());
        let error = err_of(call(
            &server,
            "session.send_text",
            Some(json!({ "sessionID": "s_missing", "text": "hi", "submit": false })),
        ));
        assert_eq!(error.code, "not_found");
    }

    #[test]
    fn record_mutations_round_trip_over_the_wire() {
        // rename → mark_seen → archive → unarchive against a record-only
        // session (no live process needed).
        let temp = tempfile::tempdir().expect("temp");
        let registry = Arc::new(Mutex::new(Registry::new(
            engine(),
            temp.path().join("state.json"),
        )));
        registry
            .lock()
            .expect("registry")
            .insert_record(test_record("s_rec"));
        let server = ControlServer::new(registry, temp.path().join("daemon.sock"));

        let params = json!({ "sessionID": "s_rec", "title": "renamed by hand" });
        ok_of(call(&server, "session.rename", Some(params)));
        ok_of(call(
            &server,
            "session.mark_seen",
            Some(json!({ "sessionID": "s_rec" })),
        ));
        ok_of(call(
            &server,
            "session.archive",
            Some(json!({ "sessionID": "s_rec" })),
        ));

        let list = ok_of(call(&server, "session.list", None));
        let record = &list["sessions"][0];
        assert_eq!(record["title"], "renamed by hand");
        // TitleSource is numeric on the wire (Swift Int-raw enum);
        // serialize the variant rather than hardcoding its index.
        assert_eq!(
            record["titleSource"],
            serde_json::to_value(diri_proto::TitleSource::UserRename).expect("encode")
        );
        assert!(record["lastSeenAt"].is_number());
        assert!(record["archivedAt"].is_number());

        ok_of(call(
            &server,
            "session.unarchive",
            Some(json!({ "sessionID": "s_rec" })),
        ));
        let list = ok_of(call(&server, "session.list", None));
        assert!(list["sessions"][0].get("archivedAt").is_none());

        ok_of(call(
            &server,
            "session.remove",
            Some(json!({ "sessionID": "s_rec" })),
        ));
        let list = ok_of(call(&server, "session.list", None));
        assert_eq!(list["sessions"].as_array().map(Vec::len), Some(0));
    }

    #[test]
    fn a_hook_report_folds_identity_into_the_record() {
        let temp = tempfile::tempdir().expect("temp");
        let registry = Arc::new(Mutex::new(Registry::new(
            engine(),
            temp.path().join("state.json"),
        )));
        registry
            .lock()
            .expect("registry")
            .insert_record(test_record("s_hook"));
        let server = ControlServer::new(registry, temp.path().join("daemon.sock"));

        ok_of(call(
            &server,
            "hook.report",
            Some(json!({
                "kind": "claude-hook",
                "dirijorSessionID": "s_hook",
                "event": "UserPromptSubmit",
                "payload": {
                    "session_id": "uuid-from-hook",
                    "transcript_path": "/tmp/t.jsonl",
                    "prompt": "fix the flaky test in ci",
                },
            })),
        ));

        let list = ok_of(call(&server, "session.list", None));
        let record = &list["sessions"][0];
        assert_eq!(record["agentSessionID"], "uuid-from-hook");
        assert_eq!(record["transcriptPath"], "/tmp/t.jsonl");
        assert_eq!(
            record["title"], "fix the flaky test in ci",
            "the first prompt titles a placeholder session"
        );
    }

    #[test]
    fn project_ids_are_deterministic_and_idempotent() {
        let temp = tempfile::tempdir().expect("temp");
        let server = server(temp.path());
        let first = ok_of(call(
            &server,
            "project.add",
            Some(json!({ "root": "/Users/x/code/app" })),
        ));
        let second = ok_of(call(
            &server,
            "project.add",
            Some(json!({ "root": "/Users/x/code/app" })),
        ));
        assert_eq!(first["id"], second["id"], "re-adding never duplicates");
        assert!(
            first["id"].as_str().expect("id").starts_with("p_"),
            "{first}"
        );
        assert_eq!(first["name"], "app");
        let list = ok_of(call(&server, "session.list", None));
        assert_eq!(list["projects"].as_array().map(Vec::len), Some(1));
    }

    #[test]
    fn agent_readiness_serves_the_catalog_with_descriptors() {
        let temp = tempfile::tempdir().expect("temp");
        let server = server(temp.path());
        let result = ok_of(call(&server, "agent.readiness", None));
        let agents = result["agents"].as_array().expect("agents");
        assert!(!agents.is_empty());
        let claude = agents
            .iter()
            .find(|agent| agent["kind"] == "claude-code")
            .expect("claude in the catalog");
        assert_eq!(claude["binary"], "claude");
        assert!(
            claude["descriptor"]["injection"]["claudeHooks"]
                .as_bool()
                .unwrap_or(false),
            "the raw manifest descriptor rides along: {claude}"
        );
    }

    #[test]
    fn a_removed_session_can_be_reopened() {
        let temp = tempfile::tempdir().expect("temp");
        let registry = Arc::new(Mutex::new(Registry::new(
            engine(),
            temp.path().join("state.json"),
        )));
        registry
            .lock()
            .expect("registry")
            .insert_record(test_record("s_gone"));
        let server = ControlServer::new(registry, temp.path().join("daemon.sock"));

        ok_of(call(
            &server,
            "session.remove",
            Some(json!({ "sessionID": "s_gone" })),
        ));
        let list = ok_of(call(&server, "session.list", None));
        assert_eq!(list["sessions"].as_array().map(Vec::len), Some(0));

        let reopened = ok_of(call(&server, "session.reopen_last", None));
        assert_eq!(reopened["id"], "s_gone");
        let list = ok_of(call(&server, "session.list", None));
        assert_eq!(list["sessions"].as_array().map(Vec::len), Some(1));

        // The stack is spent.
        let empty = err_of(call(&server, "session.reopen_last", None));
        assert_eq!(empty.code, "bad_request");
    }

    #[test]
    fn read_diff_reports_working_changes() {
        let temp = tempfile::tempdir().expect("temp");
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir");
        let git = |arguments: &[&str]| {
            let status = std::process::Command::new("git")
                .args(arguments)
                .current_dir(&repo)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .status()
                .expect("git");
            assert!(status.success(), "git {arguments:?}");
        };
        git(&["init", "-q", "-b", "main"]);
        std::fs::write(repo.join("file.txt"), "original\n").expect("write");
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "root"]);
        std::fs::write(repo.join("file.txt"), "changed by the session\n").expect("write");

        let registry = Arc::new(Mutex::new(Registry::new(
            engine(),
            temp.path().join("state.json"),
        )));
        let mut record = test_record("s_diff");
        record.cwd = repo.to_string_lossy().into_owned();
        registry.lock().expect("registry").insert_record(record);
        let server = ControlServer::new(registry, temp.path().join("daemon.sock"));

        let result = ok_of(call(
            &server,
            "session.read_diff",
            Some(json!({ "sessionID": "s_diff" })),
        ));
        assert_eq!(result["truncated"], false);
        // The patch travels base64-encoded, as the Swift daemon sends it.
        use base64::Engine as _;
        let patch = base64::engine::general_purpose::STANDARD
            .decode(result["patch"].as_str().expect("patch"))
            .expect("base64");
        let patch = String::from_utf8_lossy(&patch);
        assert!(
            patch.contains("changed by the session"),
            "the working change is in the patch: {patch}"
        );
    }

    #[test]
    fn worktrees_are_managed_over_the_wire() {
        let temp = tempfile::tempdir().expect("temp");
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir");
        for arguments in [
            vec!["init", "-b", "main"],
            vec!["commit", "--allow-empty", "-m", "root"],
        ] {
            let status = std::process::Command::new("git")
                .args(&arguments)
                .arg("--quiet")
                .current_dir(&repo)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .status()
                .expect("git");
            assert!(status.success(), "git {arguments:?}");
        }
        let server = server(temp.path());
        let repo_path = repo.to_string_lossy();

        let created = ok_of(call(
            &server,
            "worktree.create",
            Some(json!({ "repoPath": repo_path, "branch": "feature/x" })),
        ));
        assert_eq!(created["branch"], "feature/x");

        let list = ok_of(call(
            &server,
            "worktree.list",
            Some(json!({ "repoPath": repo_path })),
        ));
        let listed = list.as_array().expect("array");
        assert!(
            listed
                .iter()
                .any(|worktree| worktree["branch"] == "feature/x"),
            "{list}"
        );

        ok_of(call(
            &server,
            "worktree.remove",
            Some(json!({
                "repoPath": repo_path,
                "worktreePath": created["path"],
                "force": true,
            })),
        ));
        let list = ok_of(call(
            &server,
            "worktree.list",
            Some(json!({ "repoPath": repo_path })),
        ));
        assert!(
            !list
                .as_array()
                .expect("array")
                .iter()
                .any(|worktree| worktree["branch"] == "feature/x")
        );
    }

    #[test]
    fn missing_parameters_are_rejected_before_anything_happens() {
        let temp = tempfile::tempdir().expect("temp");
        let server = server(temp.path());
        assert_eq!(
            err_of(call(&server, "session.send_text", None)).code,
            "bad_request"
        );
        assert_eq!(
            err_of(call(&server, "session.resize", Some(json!({ "id": "s" })))).code,
            "bad_request"
        );
    }

    #[test]
    fn malformed_json_gets_an_error_rather_than_silence() {
        // A client waiting on a reply should learn that none is coming.
        let temp = tempfile::tempdir().expect("temp");
        let server = server(temp.path());
        let response = handle(&server, b"{ not json").expect("a response");
        assert_eq!(err_of(response).code, "bad_request");
    }

    #[test]
    fn responses_and_events_from_a_client_are_ignored() {
        let temp = tempfile::tempdir().expect("temp");
        let server = server(temp.path());
        let event = serde_json::to_vec(&ControlMessage::Event {
            name: "session.updated".into(),
            seq: 1,
            params: json!({}),
        })
        .expect("encode");
        assert!(
            handle(&server, &event).is_none(),
            "the daemon sends events; it does not answer them"
        );
    }

    #[test]
    fn the_socket_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().expect("temp");
        let server = server(temp.path());
        let _listener = server.bind().expect("bind");

        let mode = std::fs::metadata(server.socket_path())
            .expect("stat")
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "the control socket can spawn processes as the user"
        );
    }

    #[test]
    fn binding_over_a_live_socket_is_refused() {
        let temp = tempfile::tempdir().expect("temp");
        let server = server(temp.path());
        let _listener = server.bind().expect("first bind");

        let second = ControlServer::new(
            Arc::new(Mutex::new(Registry::new(
                engine(),
                temp.path().join("state.json"),
            ))),
            server.socket_path(),
        );
        let error = second
            .bind()
            .expect_err("two engines must not share a socket");
        assert_eq!(error.kind(), std::io::ErrorKind::AddrInUse);
    }

    #[test]
    fn a_stale_socket_file_is_replaced() {
        // The daemon died without cleaning up; the next start must not be
        // blocked by the leftover file.
        let temp = tempfile::tempdir().expect("temp");
        let path = temp.path().join("daemon.sock");
        std::fs::write(&path, b"").expect("leave a stale file");

        let server = ControlServer::new(
            Arc::new(Mutex::new(Registry::new(
                engine(),
                temp.path().join("state.json"),
            ))),
            &path,
        );
        let _listener = server.bind().expect("a stale socket should be replaced");
    }
}
