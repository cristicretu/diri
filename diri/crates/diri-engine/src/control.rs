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

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use diri_proto::control::MAX_CONTROL_LINE_BYTES;
use diri_proto::{ControlError, ControlMessage, JsonValue, Method, WIRE_VERSION};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::registry::Registry;

/// Identifies this engine in the handshake, so a client can tell which
/// implementation it reached.
pub const BUILD: &str = concat!("diri-engine-", env!("CARGO_PKG_VERSION"));

pub struct ControlServer {
    registry: Arc<Mutex<Registry>>,
    socket_path: PathBuf,
    logs_dir: PathBuf,
    holder: Option<crate::session::HolderConfig>,
    remote: Option<Arc<crate::remote::manager::RemoteManager>>,
    remote_bindings: Option<crate::remote::binding::RemoteBindingStore>,
    events: crate::events::EventBus,
    attach: crate::attach::AttachHub,
    pr_monitor_wake: crate::pr_monitor::PrMonitorWake,
    injection: Option<InjectionConfig>,
    governor: std::sync::Arc<Mutex<crate::governor::GovernorConfig>>,
    browser: std::sync::OnceLock<crate::browser::BrowserPool>,
    active_connections: Arc<AtomicUsize>,
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
        // Capture the bytes this process actually started from before an app
        // updater can replace the bundle path underneath the live daemon.
        let _ = process_executable_hash();
        let socket_path = socket_path.into();
        let logs_dir = socket_path
            .parent()
            .map(|parent| parent.join("logs"))
            .unwrap_or_else(|| PathBuf::from("logs"));
        let remote_bindings = socket_path.parent().and_then(|parent| {
            crate::remote::binding::RemoteBindingStore::new(parent.join("remote-bindings")).ok()
        });
        Self {
            registry,
            socket_path,
            logs_dir,
            holder: None,
            remote: None,
            remote_bindings,
            events: crate::events::EventBus::new(),
            attach: crate::attach::AttachHub::new(),
            pr_monitor_wake: crate::pr_monitor::PrMonitorWake::default(),
            injection: None,
            governor: std::sync::Arc::new(Mutex::new(crate::governor::GovernorConfig::default())),
            browser: std::sync::OnceLock::new(),
            active_connections: Arc::new(AtomicUsize::new(0)),
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

    /// Enables the SSH-bootstrapped remote Holder transport. The local app
    /// still talks only to this Engine; it never executes SSH itself.
    pub fn with_remote(mut self, manager: Arc<crate::remote::manager::RemoteManager>) -> Self {
        self.remote = Some(manager);
        self
    }

    /// Re-adopts remote Holder sessions in the background.
    ///
    /// Every binding costs at least one SSH round trip, and each carries a
    /// two-minute timeout. Doing that before `bind()` meant the control socket
    /// did not exist until the last host answered: one reachable-but-hung host
    /// kept the whole app disconnected, and because the executor forces
    /// `SSH_ASKPASS_REQUIRE`, a host needing a passphrase could raise a modal
    /// from a daemon with no UI behind it. Local sessions are served
    /// immediately now, and remote ones join as they are verified.
    pub fn spawn_remote_restore(self: &Arc<Self>) {
        if self.remote_bindings.is_none() {
            return;
        }
        let manager = self.remote.clone();
        let server = Arc::clone(self);
        if let Err(error) = std::thread::Builder::new()
            .name("diri-remote-restore".into())
            .spawn(move || {
                // Before adoption, not after: adoption prunes bindings for
                // sessions it finds dead, and a pruned binding is
                // indistinguishable from a record that never had one. Running
                // first is what keeps the legacy test — "has a host and no
                // binding" — from swallowing this launch's own casualties.
                server.retire_legacy_remote_sessions();
                let Some(manager) = manager else {
                    return;
                };
                let adopted = server.restore_remote_bindings(&manager);
                if !adopted.is_empty() {
                    eprintln!(
                        "diri-engine: adopted {} remote Holder session(s): {adopted:?}",
                        adopted.len()
                    );
                }
            })
        {
            eprintln!("diri-engine: could not start remote session restore: {error}");
        }
    }

    /// One-shot upgrade path for sessions the deleted `ssh -t` + tmux transport
    /// created. See [`crate::legacy_remote`] for what it does, what it refuses
    /// to do, and why this is not a tmux fallback.
    ///
    /// Deliberately independent of `with_remote`: a build with no Helper
    /// artifact still has the user's old records and still owes them a working
    /// Resume button and a cleaned-up host.
    fn retire_legacy_remote_sessions(&self) {
        let plan = crate::legacy_remote::Plan {
            registry: &self.registry,
            bindings: self.remote_bindings.as_ref(),
            hosts: &diri_proto::HostsConfig::load(self.hosts_file()),
            marker_path: self.legacy_remote_marker(),
        };
        let outcome =
            crate::legacy_remote::retire_legacy_remote_sessions(&plan, &crate::hosts::run_shell);
        if let Some(summary) = outcome.summary() {
            eprintln!("{summary}");
        }
        // These records have no live session, so the registry watcher — which
        // only diffs live ones — will never announce the rewrite. Without this
        // the sidebar keeps showing them as running until the next relaunch.
        if !outcome.migrated.is_empty()
            && let Ok(registry) = self.registry.lock()
        {
            for id in &outcome.migrated {
                self.publish_updated(&registry, id);
            }
        }
    }

    /// Beside the socket, next to `remote-bindings` — one file, deletable the
    /// day this migration is retired.
    fn legacy_remote_marker(&self) -> PathBuf {
        self.socket_path
            .parent()
            .map(|parent| parent.join("legacy-remote-migration.json"))
            .unwrap_or_else(|| PathBuf::from("legacy-remote-migration.json"))
    }

    fn restore_remote_bindings(
        &self,
        manager: &Arc<crate::remote::manager::RemoteManager>,
    ) -> Vec<String> {
        let Some(store) = &self.remote_bindings else {
            return Vec::new();
        };
        let Ok(bindings) = store.load_all() else {
            return Vec::new();
        };
        let hosts = diri_proto::HostsConfig::load(self.hosts_file());
        let mut registry = match self.registry.lock() {
            Ok(registry) => registry,
            Err(_) => return Vec::new(),
        };
        let records = registry
            .records()
            .into_iter()
            .map(|record| (record.id.0.clone(), record))
            .collect::<std::collections::HashMap<_, _>>();
        let mut adopted = Vec::new();
        for binding in bindings {
            let Some(record) = records.get(&binding.session_id) else {
                continue;
            };
            if record.host.as_deref() != Some(&binding.host_id) {
                continue;
            }
            let Some(host) = hosts.host(&binding.host_id) else {
                continue;
            };
            let Ok(helper) =
                manager.existing_helper(host, &binding.helper_build_id, binding.protocol)
            else {
                continue;
            };
            let selector = diri_proto::remote_pty::SessionSelector {
                session_id: binding.session_id.clone(),
                session_token: binding.session_token.clone(),
                expected_incarnation: Some(binding.session_incarnation.clone()),
            };
            let Ok(inspection) = manager.inspect(&helper, &selector) else {
                continue;
            };
            if matches!(record.status, diri_proto::SessionStatus::Exited(_))
                || matches!(
                    inspection.process_state,
                    diri_proto::remote_pty::RemoteProcessState::Exited { .. }
                )
            {
                let _ = manager.kill(&helper, &selector);
                let _ = store.remove(&binding.session_id);
                continue;
            }
            if !matches!(
                inspection.process_state,
                diri_proto::remote_pty::RemoteProcessState::Running { .. }
            ) {
                continue;
            }
            let manifest_id = record.kind.id().to_string();
            let spec = crate::session::SessionSpec {
                id: binding.session_id.clone(),
                pty: crate::pty::PtySpec::new(Vec::new(), &record.cwd)
                    .size(inspection.cols, inspection.rows),
                manifest_id: manifest_id.clone(),
                authority: crate::session::authority_for(&manifest_id, &registry.engine()),
                logs_dir: self.logs_dir.clone(),
                holder: None,
                remote: None,
                defer_launch: false,
            };
            let remote = crate::session::RemoteAdoptSpec {
                manager: Arc::clone(manager),
                helper,
                token: binding.session_token,
                incarnation: binding.session_incarnation,
                binding_store: store.clone(),
                output_offset: binding.last_output_offset,
            };
            if registry.adopt_remote(spec, remote).is_ok() {
                adopted.push(binding.session_id);
            }
        }
        adopted
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
        let _connection = ActiveConnectionGuard::new(Arc::clone(&self.active_connections));
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
                    // Attaching means this session is visible. Reconcile the
                    // actual process first: an adopted holder can be stopped
                    // even when stale persisted metadata says it is awake.
                    // This cold-boundary SIGCONT is harmless for a running
                    // tree and keeps process-tree work off the keystroke path.
                    // Recording visibility before waking the PR monitor keeps
                    // its immediate pass seeing a foreground/recent session
                    // even if registration has not completed yet.
                    if let Ok(mut registry) = self.registry.lock() {
                        let _ = registry.ensure_session_awake(&attach.attach.0);
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
            Method::HOST_INITIALIZE => self.host_initialize(params),
            Method::HOST_LIST_DIRECTORIES => self.host_list_directories(params),
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
            Method::DAEMON_SHUTDOWN_IF_IDLE => self.daemon_shutdown_if_idle(),
            Method::DAEMON_SHUTDOWN => self.daemon_shutdown(),
            Method::GOVERNOR_CONFIGURE => self.governor_configure(params),
            Method::CLIENT_SET_ACTIVE => self.client_set_active(params),
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
            "engineKind": diri_proto::RUST_ENGINE_KIND,
            "pid": std::process::id() as i32,
            "executableHash": process_executable_hash(),
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
        if p.host.is_some() {
            return self.session_spawn_remote(p, argv);
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
                launch_args.extend(crate::inject::injection_args_with_cursor(
                    &descriptor.injection,
                    &injection.inject_dir,
                    &injection.cli_path,
                    Some(crate::inject::CursorInject {
                        session_id: &id,
                        socket_path: &self.socket_path,
                    }),
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
        record.kind = p.kind.clone();
        // A linked worktree is an execution cwd inside the project selected
        // by the user; it does not become a new first-level sidebar project.
        record.project_id = crate::registry::session_project_id(&p.cwd, None);
        registry.ensure_session_project(&p.cwd, None);
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
            manifest_id: kind.clone(),
            authority,
            logs_dir: self.logs_dir.clone(),
            holder: self.holder.clone(),
            remote: None,
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
        let prompt = p.initial_prompt.clone().filter(|prompt| !prompt.is_empty());
        if kind == diri_proto::AgentKind::CLAUDE_CODE_ID || prompt.is_some() {
            let registry = Arc::clone(&self.registry);
            let session_id = id.clone();
            std::thread::spawn(move || {
                prepare_agent_input(
                    &registry,
                    &session_id,
                    kind == diri_proto::AgentKind::CLAUDE_CODE_ID,
                    prompt.as_deref(),
                );
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

    fn session_spawn_remote(
        &self,
        p: diri_proto::SessionSpawnParams,
        caller_argv: Vec<String>,
    ) -> Result<JsonValue, ControlError> {
        let manager = self
            .remote
            .as_ref()
            .cloned()
            .ok_or_else(crate::remote::transport_unavailable)?;
        let binding_store = self.remote_bindings.clone().ok_or_else(|| {
            ControlError::internal("owner-only remote binding store is unavailable")
        })?;
        let host_id = p
            .host
            .as_deref()
            .ok_or_else(|| ControlError::bad_request("remote host is required"))?;
        let host = self.resolve_host(host_id)?;
        if p.new_worktree.unwrap_or(false) {
            return Err(ControlError::bad_request(
                "remote worktree creation requires the structured workspace RPC",
            ));
        }
        if p.same_repo_as.is_some() {
            return Err(ControlError::bad_request(
                "sameRepoAs requires the structured remote workspace RPC",
            ));
        }

        let helper = manager.ensure_helper(&host).map_err(io_control_error)?;
        let persistence = manager
            .probe_persistence(&host, &helper)
            .map_err(io_control_error)?;
        let requested_cwd = if p.cwd.trim().is_empty() {
            host.default_cwd.clone().unwrap_or_else(|| "~".into())
        } else {
            p.cwd.clone()
        };
        let captured = manager
            .capture_environment(
                &helper,
                &diri_proto::remote_pty::EnvironmentCaptureRequest {
                    cwd: Some(requested_cwd),
                    timeout_millis: 10_000,
                },
            )
            .map_err(io_control_error)?;
        let cwd = PathBuf::from(&captured.cwd);
        if !cwd.is_absolute() {
            return Err(ControlError::internal(
                "remote Helper returned a non-absolute cwd",
            ));
        }

        let kind = p.kind.id().to_string();
        let (descriptor, engine) = {
            let registry = self.registry.lock().map_err(poisoned)?;
            let engine = registry.engine();
            let manifest = engine.manifest(&kind).ok_or_else(|| {
                ControlError::not_found(format!("no manifest for agent {kind:?}"))
            })?;
            (manifest.agent.clone().unwrap_or_default(), engine)
        };
        drop(engine);
        let authority = descriptor.authority();
        let inherited = captured
            .environment
            .into_iter()
            .map(|variable| (variable.name, variable.value))
            .collect::<Vec<_>>();

        let id = next_session_id();
        let mut agent_session_id = None;
        let mut launch_args = caller_argv.clone();
        if descriptor.binary.is_some() {
            launch_args.extend(descriptor.spawn_args.iter().cloned());
            agent_session_id = descriptor.session_id_flag.as_ref().map(|flag| {
                let uuid = crate::inject::uuid_v4();
                launch_args.push(flag.clone());
                launch_args.push(uuid.clone());
                uuid
            });
        }

        let argv = if descriptor.binary.is_some() {
            descriptor
                .remote_spawn_spec(&cwd, inherited.clone(), &launch_args)
                .ok_or_else(|| ControlError::internal("remote descriptor has no binary"))?
                .argv
        } else if !caller_argv.is_empty() {
            caller_argv
        } else if let Some(command) = p.kind.command().filter(|command| !command.is_empty()) {
            vec![captured.shell.clone(), "-lc".into(), command.to_string()]
        } else if kind == diri_proto::AgentKind::SHELL_ID {
            vec![captured.shell.clone(), "-l".into()]
        } else {
            return Err(ControlError::bad_request(format!(
                "agent {kind:?} declares no binary, so argv is required"
            )));
        };
        let mut pty = if descriptor.binary.is_some() {
            descriptor
                .remote_spawn_spec(&cwd, inherited, &launch_args)
                .ok_or_else(|| ControlError::internal("remote descriptor has no binary"))?
        } else {
            let mut spec = crate::pty::PtySpec::new(argv, &cwd);
            spec.env = inherited;
            spec.env.retain(|(key, _)| key != "NO_COLOR");
            spec.env.retain(|(key, _)| key != "TERM");
            spec.env.push(("TERM".into(), "xterm-256color".into()));
            spec
        };
        if let (Some(cols), Some(rows)) = (p.initial_cols, p.initial_rows) {
            pty.cols = cols.clamp(2, u16::MAX as i64) as u16;
            pty.rows = rows.clamp(2, u16::MAX as i64) as u16;
        }

        let token = random_session_token()?;
        let launch = diri_proto::remote_pty::LaunchRequest {
            session_id: id.clone(),
            session_token: token,
            argv: pty.argv.clone(),
            cwd: captured.cwd.clone(),
            environment: pty
                .env
                .iter()
                .map(
                    |(name, value)| diri_proto::remote_pty::EnvironmentVariable {
                        name: name.clone(),
                        value: value.clone(),
                    },
                )
                .collect(),
            cols: pty.cols,
            rows: pty.rows,
            persistence,
        };

        let mut record = new_record(&id, &kind, &captured.cwd);
        record.kind = p.kind.clone();
        record.host = Some(host.id.clone());
        record.project_id = crate::registry::session_project_id(&captured.cwd, Some(&host.id));
        record.remote_persistence = Some(persistence);
        record.parent = p.parent.clone();
        record.agent_session_id = agent_session_id;
        if let Some(title) = &p.title {
            record.title = title.clone();
            record.title_source = diri_proto::TitleSource::DirijorAssigned;
        }
        let spec = crate::session::SessionSpec {
            id: id.clone(),
            pty,
            manifest_id: kind.clone(),
            authority,
            logs_dir: self.logs_dir.clone(),
            holder: None,
            remote: Some(crate::session::RemoteSessionSpec {
                manager,
                helper,
                launch,
                host_id: host.id.clone(),
                binding_store,
            }),
            defer_launch: false,
        };
        let mut registry = self.registry.lock().map_err(poisoned)?;
        registry.ensure_session_project(&captured.cwd, Some(&host.id));
        registry
            .spawn(spec, record)
            .map_err(|error| ControlError::internal(error.to_string()))?;
        let _ = registry.persist();
        self.publish_updated(&registry, &id);

        let prompt = p.initial_prompt.filter(|prompt| !prompt.is_empty());
        if kind == diri_proto::AgentKind::CLAUDE_CODE_ID || prompt.is_some() {
            let registry = Arc::clone(&self.registry);
            let session_id = id.clone();
            std::thread::spawn(move || {
                prepare_agent_input(
                    &registry,
                    &session_id,
                    kind == diri_proto::AgentKind::CLAUDE_CODE_ID,
                    prompt.as_deref(),
                );
            });
        }
        let record = registry
            .records()
            .into_iter()
            .find(|record| record.id.0 == id)
            .ok_or_else(|| ControlError::internal("the new remote session vanished"))?;
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
                .env("LC_ALL", "C")
                .env("LANG", "C")
                .env("LANGUAGE", "C")
                .env("GIT_TERMINAL_PROMPT", "0")
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
        // Handoff needs no terminal multiplexer of its own. Its phases are
        // git preparation over `hosts::run_shell`, stopping the source through
        // the session's own transport (which signals the remote Agent via its
        // Holder), the transcript shuttle, and a normal resume on the target.
        // Refuse only when a leg is remote and no Helper transport exists to
        // carry it, rather than refusing every call.
        if (record.host.is_some() || p.target_host.is_some()) && self.remote.is_none() {
            return Err(crate::remote::transport_unavailable());
        }
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
            registry.ensure_session_project(&cwd, target_id.as_deref());
            registry.update_record(&id, |record| {
                record.host = target_id;
                record.cwd = cwd;
                record.project_id =
                    crate::registry::session_project_id(&record.cwd, record.host.as_deref());
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

    /// `host.initialize`: run the complete idempotent SSH bootstrap before a
    /// user creates the first session. No environment values cross back into
    /// the app; only facts suitable for a visible readiness summary do.
    fn host_initialize(&self, params: Option<JsonValue>) -> Result<JsonValue, ControlError> {
        let p: diri_proto::HostInitializeParams = decode(params)?;
        let manager = self
            .remote
            .as_ref()
            .ok_or_else(crate::remote::transport_unavailable)?;
        let host = self.resolve_host(&p.host)?;
        let helper = if p.force_reinstall {
            manager.reinstall_helper(&host)
        } else {
            manager.ensure_helper(&host)
        }
        .map_err(io_control_error)?;
        let persistence = manager
            .probe_persistence(&host, &helper)
            .map_err(io_control_error)?;
        let captured = manager
            .capture_environment(
                &helper,
                &diri_proto::remote_pty::EnvironmentCaptureRequest {
                    cwd: Some(host.default_cwd.clone().unwrap_or_else(|| "~".into())),
                    timeout_millis: 10_000,
                },
            )
            .map_err(io_control_error)?;
        encode(&diri_proto::HostInitializeResult {
            helper_build_id: helper.build_id,
            protocol: helper.protocol,
            persistence,
            cwd: captured.cwd,
            shell: captured.shell,
        })
    }

    /// `host.list_directories`: one shallow, bounded filesystem read on the
    /// requested execution machine. Remote work stays behind the Engine and
    /// uses the verified Helper over `ssh -T`; the app never executes SSH.
    fn host_list_directories(&self, params: Option<JsonValue>) -> Result<JsonValue, ControlError> {
        let p: diri_proto::HostListDirectoriesParams = decode(params)?;
        let request = diri_proto::remote_pty::DirectoryListRequest { path: p.path };
        let result = if let Some(host_id) = p.host {
            let manager = self
                .remote
                .as_ref()
                .ok_or_else(crate::remote::transport_unavailable)?;
            let host = self.resolve_host(&host_id)?;
            manager
                .list_directories(&host, &request)
                .map_err(io_control_error)?
        } else {
            crate::directories::list(&request).map_err(io_control_error)?
        };
        encode(&result)
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

    /// Applies the current application build's remote environment gate before
    /// a stateless SSH action. Live Holder operations deliberately use their
    /// session binding's creation-time Helper instead.
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
        if let Some(store) = &self.remote_bindings {
            let _ = store.remove(&p.session_id.0);
        }
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
        if let Some(store) = &self.remote_bindings {
            let _ = store.remove(&p.session_id.0);
        }
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
        let record = {
            let registry = self.registry.lock().map_err(poisoned)?;
            let record = registry
                .records()
                .into_iter()
                .find(|record| record.id.0 == p.session_id.0)
                .ok_or_else(|| ControlError::not_found(p.session_id.0.clone()))?;
            // Presence in the registry is not liveness: only an explicit kill
            // removes a session, so an agent that died on its own is still in
            // the map. Returning here on presence alone would hand back the
            // corpse this call was asked to revive; the exited case falls
            // through to the eviction path below.
            if registry.get(&p.session_id.0).is_some()
                && !matches!(record.status, diri_proto::SessionStatus::Exited(_))
            {
                // Genuinely live: resuming is a no-op, not an error.
                return serde_json::to_value(&record)
                    .map_err(|error| ControlError::internal(error.to_string()));
            }
            record
        };
        let spec = if record.host.is_some() {
            self.remote_resume_spec(&record)?
        } else {
            let registry = self.registry.lock().map_err(poisoned)?;
            self.resume_spec(
                &registry,
                &record.id.0,
                record.kind.id(),
                &record.cwd,
                record.agent_session_id.as_deref(),
            )?
        };
        let remote_persistence = spec.remote.as_ref().map(|remote| remote.launch.persistence);
        let mut registry = self.registry.lock().map_err(poisoned)?;
        let record = registry
            .records()
            .into_iter()
            .find(|record| record.id.0 == p.session_id.0)
            .ok_or_else(|| ControlError::not_found(p.session_id.0.clone()))?;
        let exited = matches!(record.status, diri_proto::SessionStatus::Exited(_));
        if registry.get(&p.session_id.0).is_some() {
            if !exited {
                // Already live: resuming is a no-op, not an error.
                return serde_json::to_value(&record)
                    .map_err(|error| ControlError::internal(error.to_string()));
            }
            // An agent that died on its own leaves its session behind: only an
            // explicit kill takes one out of the registry, so presence alone
            // does not mean alive. Evicting the corpse — which also releases
            // the holder still owning this id — is what keeps resume from
            // silently handing back the dead record it was asked to revive.
            let _ = registry.terminate(&p.session_id.0, std::time::Duration::from_millis(500));
        }
        registry
            .respawn(spec)
            .map_err(|error| ControlError::internal(error.to_string()))?;
        if let Some(persistence) = remote_persistence {
            registry.update_record(&p.session_id.0, |record| {
                record.remote_persistence = Some(persistence);
            });
        }
        let _ = registry.persist();
        self.publish_updated(&registry, &p.session_id.0);
        let record = registry
            .records()
            .into_iter()
            .find(|record| record.id.0 == p.session_id.0)
            .ok_or_else(|| ControlError::internal("the resumed session vanished"))?;
        serde_json::to_value(&record).map_err(|error| ControlError::internal(error.to_string()))
    }

    fn remote_resume_spec(
        &self,
        record: &diri_proto::SessionRecord,
    ) -> Result<crate::session::SessionSpec, ControlError> {
        let manager = self
            .remote
            .as_ref()
            .cloned()
            .ok_or_else(crate::remote::transport_unavailable)?;
        let binding_store = self.remote_bindings.clone().ok_or_else(|| {
            ControlError::internal("owner-only remote binding store is unavailable")
        })?;
        let host_id = record
            .host
            .as_deref()
            .ok_or_else(|| ControlError::bad_request("remote record has no host"))?;
        let host = self.resolve_host(host_id)?;
        let helper = manager.ensure_helper(&host).map_err(io_control_error)?;
        let persistence = manager
            .probe_persistence(&host, &helper)
            .map_err(io_control_error)?;
        let captured = manager
            .capture_environment(
                &helper,
                &diri_proto::remote_pty::EnvironmentCaptureRequest {
                    cwd: Some(record.cwd.clone()),
                    timeout_millis: 10_000,
                },
            )
            .map_err(io_control_error)?;
        let cwd = PathBuf::from(&captured.cwd);
        if !cwd.is_absolute() {
            return Err(ControlError::internal(
                "remote Helper returned a non-absolute cwd",
            ));
        }
        let (descriptor, authority) = {
            let registry = self.registry.lock().map_err(poisoned)?;
            let engine = registry.engine();
            let manifest = engine.manifest(record.kind.id()).ok_or_else(|| {
                ControlError::not_found(format!("no manifest for agent {}", record.kind.id()))
            })?;
            let descriptor = manifest.agent.clone().unwrap_or_default();
            let authority = descriptor.authority();
            (descriptor, authority)
        };
        let mut launch_args = descriptor.spawn_args.clone();
        launch_args.extend(
            descriptor
                .resume_args(record.agent_session_id.as_deref())
                .ok_or_else(|| {
                    ControlError::bad_request(format!(
                        "agent {} does not support resume",
                        record.kind.id()
                    ))
                })?,
        );
        let inherited = captured
            .environment
            .into_iter()
            .map(|variable| (variable.name, variable.value));
        let pty = descriptor
            .remote_spawn_spec(&cwd, inherited, &launch_args)
            .ok_or_else(|| {
                ControlError::bad_request(format!("agent {} declares no binary", record.kind.id()))
            })?;
        let launch = diri_proto::remote_pty::LaunchRequest {
            session_id: record.id.0.clone(),
            session_token: random_session_token()?,
            argv: pty.argv.clone(),
            cwd: captured.cwd,
            environment: pty
                .env
                .iter()
                .map(
                    |(name, value)| diri_proto::remote_pty::EnvironmentVariable {
                        name: name.clone(),
                        value: value.clone(),
                    },
                )
                .collect(),
            cols: pty.cols,
            rows: pty.rows,
            persistence,
        };
        Ok(crate::session::SessionSpec {
            id: record.id.0.clone(),
            pty,
            manifest_id: record.kind.id().to_string(),
            authority,
            logs_dir: self.logs_dir.clone(),
            holder: None,
            remote: Some(crate::session::RemoteSessionSpec {
                manager,
                helper,
                launch,
                host_id: host.id,
                binding_store,
            }),
            defer_launch: false,
        })
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
        registry.ensure_session_project(&p.entry.cwd, None);
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
            let replay = crate::agent::InjectionSpec {
                claude_hooks: descriptor.injection.claude_hooks,
                claude_mcp: descriptor.injection.claude_mcp,
                cursor_mcp: descriptor.injection.cursor_mcp,
                cursor_hooks: descriptor.injection.cursor_hooks,
                ..Default::default()
            };
            launch_args.extend(crate::inject::injection_args_with_cursor(
                &replay,
                &injection.inject_dir,
                &injection.cli_path,
                Some(crate::inject::CursorInject {
                    session_id: id,
                    socket_path: &self.socket_path,
                }),
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
            remote: None,
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
        let (cwd, host_id) = {
            let registry = self.registry.lock().map_err(poisoned)?;
            registry
                .records()
                .into_iter()
                .find(|record| record.id.0 == p.session_id.0)
                .map(|record| (record.cwd, record.host))
                .ok_or_else(|| ControlError::not_found(p.session_id.0.clone()))?
        };
        let result = if let Some(host_id) = host_id {
            let manager = self
                .remote
                .as_ref()
                .ok_or_else(crate::remote::transport_unavailable)?;
            let host = self.resolve_host(&host_id)?;
            crate::git::working_diff_remote(manager, &host, &cwd, p.base.as_ref())
                .map_err(io_control_error)?
        } else {
            crate::git::working_diff(Path::new(&cwd), p.base.as_ref()).map_err(io_control_error)?
        };
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

    /// Releases the detached Engine after the desktop App goes away, but only
    /// when doing so cannot strand a live Agent or interrupt another client.
    /// The delayed recheck happens after the acknowledgement has flushed and
    /// the requesting connection has had time to close.
    fn daemon_shutdown_if_idle(&self) -> Result<JsonValue, ControlError> {
        let live_sessions = {
            let mut registry = self.registry.lock().map_err(poisoned)?;
            let live_sessions = registry.live_count();
            if live_sessions == 0 {
                let _ = registry.persist();
            }
            live_sessions
        };
        let connections = self.active_connections.load(Ordering::Acquire);
        let refusal = idle_shutdown_refusal(live_sessions, connections);
        if let Some(reason) = refusal {
            return encode(&diri_proto::DaemonShutdownIfIdleResult {
                will_exit: false,
                reason: Some(reason.to_owned()),
            });
        }

        let registry = Arc::clone(&self.registry);
        let active_connections = Arc::clone(&self.active_connections);
        let remote = self.remote.clone();
        let holder = self.holder.clone();
        let browser = self.browser.get().cloned();
        let socket_path = self.socket_path.clone();
        std::thread::spawn(move || {
            // The control response must reach the App before its client shuts
            // down. Wait up to one second for precisely that connection to
            // disappear; any new/other client cancels the exit.
            for _ in 0..20 {
                std::thread::sleep(Duration::from_millis(50));
                if active_connections.load(Ordering::Acquire) == 0 {
                    let still_idle = registry
                        .lock()
                        .is_ok_and(|registry| registry.live_count() == 0);
                    if still_idle {
                        if let Some(remote) = remote {
                            remote.close_control_masters();
                        }
                        if let Some(holder) = holder {
                            let paths = crate::holder::HolderManagerPaths::new(&holder.holders_dir);
                            let _ = crate::holder::HolderManagerClient::new(paths.socket())
                                .shutdown_if_idle();
                        }
                        if let Some(browser) = browser {
                            browser.shutdown();
                        }
                        let _ = std::fs::remove_file(socket_path);
                        std::process::exit(0);
                    }
                    return;
                }
            }
        });
        encode(&diri_proto::DaemonShutdownIfIdleResult {
            will_exit: true,
            reason: None,
        })
    }

    /// Ack first, then exit: the response has to flush before the process
    /// dies, so the client sees a clean reply followed by a socket drop and
    /// relaunches the fresh binary.
    fn daemon_shutdown(&self) -> Result<JsonValue, ControlError> {
        {
            let mut registry = self.registry.lock().map_err(poisoned)?;
            let _ = registry.persist();
        }
        let browser = self.browser.get().cloned();
        let socket_path = self.socket_path.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(200));
            if let Some(browser) = browser {
                browser.shutdown();
            }
            let _ = std::fs::remove_file(socket_path);
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

/// Content identity of the running Engine. It is computed once, then reused by
/// every heartbeat so version coordination has no steady-state hashing cost.
fn process_executable_hash() -> Option<&'static str> {
    static HASH: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    HASH.get_or_init(|| {
        let executable = std::env::current_exe().ok()?;
        let mut file = std::fs::File::open(executable).ok()?;
        let mut digest = Sha256::new();
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let read = file.read(&mut buffer).ok()?;
            if read == 0 {
                break;
            }
            digest.update(&buffer[..read]);
        }
        Some(
            digest
                .finalize()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>(),
        )
    })
    .as_deref()
}

/// A session id in the daemon's format: `s_` plus twelve hex digits.
pub(crate) fn next_session_id() -> String {
    let mut bytes = [0u8; 6];
    getrandom::fill(&mut bytes).expect("the OS random source");
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("s_{hex}")
}

fn random_session_token() -> Result<diri_proto::remote_pty::SessionToken, ControlError> {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes)
        .map_err(|error| ControlError::internal(format!("secure random source failed: {error}")))?;
    let encoded = bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    diri_proto::remote_pty::SessionToken::new(encoded)
        .map_err(|error| ControlError::internal(error.to_string()))
}

pub(crate) fn new_record(id: &str, kind: &str, cwd: &str) -> diri_proto::SessionRecord {
    use diri_proto::{AgentKind, DateMillis, Resumability, SessionId, TitleSource};
    let now: DateMillis = std::time::SystemTime::now().into();
    diri_proto::SessionRecord {
        id: SessionId(id.to_string()),
        kind: AgentKind::new(kind),
        cwd: cwd.to_string(),
        project_id: crate::registry::session_project_id(cwd, None),
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
        host: None,
        remote_persistence: None,
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

impl Drop for SubscriptionHandle {
    fn drop(&mut self) {
        // Dropping a JoinHandle detaches rather than cancels its thread. Make
        // the subscription's 250 ms receive timeout a real upper bound on
        // cleanup instead of leaking one polling thread per reconnect.
        self.stop.store(true, std::sync::atomic::Ordering::Release);
    }
}

struct ActiveConnectionGuard {
    connections: Arc<AtomicUsize>,
}

impl ActiveConnectionGuard {
    fn new(connections: Arc<AtomicUsize>) -> Self {
        connections.fetch_add(1, Ordering::AcqRel);
        Self { connections }
    }
}

impl Drop for ActiveConnectionGuard {
    fn drop(&mut self) {
        self.connections.fetch_sub(1, Ordering::AcqRel);
    }
}

fn idle_shutdown_refusal(live_sessions: usize, connections: usize) -> Option<&'static str> {
    if live_sessions != 0 {
        Some("live sessions still require the Engine")
    } else if connections == 0 {
        Some("request is not associated with a live control connection")
    } else if connections > 1 {
        Some("another control client still requires the Engine")
    } else {
        None
    }
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

/// Handles the only startup prompt Diri can safely pre-authorize: the exact
/// workspace the user just selected for Claude. Current Claude has no launch
/// flag that skips only workspace trust; its documented bypass flag also
/// disables every tool permission and is deliberately not used.
fn prepare_agent_input(
    registry: &Arc<Mutex<Registry>>,
    session_id: &str,
    accept_claude_workspace: bool,
    prompt: Option<&str>,
) {
    if accept_claude_workspace {
        accept_claude_workspace_trust(registry, session_id);
    }
    if let Some(prompt) = prompt {
        inject_initial_prompt(registry, session_id, prompt);
    }
}

/// Answers Claude Code's "do you trust this folder?" picker on the user's
/// behalf so a spawn does not stall behind it and swallow the initial prompt.
///
/// This is a deliberate trade: it auto-grants workspace trust for whatever
/// directory the session was pointed at. That is defensible when the user
/// picked the directory in the UI, and weaker when they did not — an
/// orchestrator spawning into a freshly cloned repository gets trust without
/// anyone affirming it. The window is bounded (20s, and it stops at the first
/// non-matching screen), but a session whose own output contains the matched
/// phrases inside that window would also receive the keystroke.
fn accept_claude_workspace_trust(registry: &Arc<Mutex<Registry>>, session_id: &str) {
    for _ in 0..200 {
        let Some((exited, screen)) = with_session(registry, session_id, |session| {
            (session.view().exited, session.screen_lines().join("\n"))
        }) else {
            return;
        };
        if exited {
            return;
        }
        if is_claude_workspace_trust_screen(&screen) {
            let _ = with_session(registry, session_id, |session| session.send_text("1", true));
            // Let Claude persist trust and replace the picker before a caller's
            // initial prompt starts its own readiness/verification loop.
            for _ in 0..20 {
                std::thread::sleep(Duration::from_millis(100));
                let changed = with_session(registry, session_id, |session| {
                    !is_claude_workspace_trust_screen(&session.screen_lines().join("\n"))
                })
                .unwrap_or(true);
                if changed {
                    return;
                }
            }
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn is_claude_workspace_trust_screen(screen: &str) -> bool {
    let normalized = screen.to_ascii_lowercase();
    normalized.contains("yes, i trust this folder")
        && (normalized.contains("1.") || normalized.contains("1 "))
}

/// Types an initial prompt into a freshly spawned agent.
///
/// The old shape of this — paste-and-Enter in one go, then call it settled
/// the moment the screen changed at all — lost the prompt outright against
/// Claude Code: its banner and tips repaint for seconds after bracketed-paste
/// mode comes on, so the "screen changed" tell fired on a repaint while the
/// composer had quietly discarded the keystrokes. The user typed a prompt,
/// got a bare agent, and the prompt was gone.
///
/// So the Enter is no longer sent blind, and "it landed" is no longer
/// inferred from the screen merely moving. Each attempt TYPES the prompt
/// without submitting and watches for it to echo into the composer; if it
/// does, the Enter follows a prompt we can see. If it does not — which also
/// describes a line-mode reader that paints nothing before a newline — the
/// Enter goes out anyway and the prompt itself must then appear on screen.
/// Only when neither happens is the attempt treated as swallowed, and only
/// then is anything retyped.
///
/// And it keeps trying. A first-run agent can sit on a trust dialog or a
/// login for a minute before it has a composer at all (Codex asks whether it
/// trusts the directory), which the old three-quick-tries shape treated as
/// "prompt lost". The prompt is held rather than fired: a dialog does not
/// echo what it is handed, so those attempts simply fail their check and come
/// back a moment later, and the first attempt after the dialog closes is the
/// one that lands. Nothing here consults the session's status — Codex reads
/// as `Working` even at an idle composer, so the echo is the only tell worth
/// trusting.
fn inject_initial_prompt(registry: &Arc<Mutex<Registry>>, session_id: &str, prompt: &str) {
    if !wait_until_ready(registry, session_id) {
        return;
    }
    let give_up_at = Instant::now() + PROMPT_INJECTION_WINDOW;
    loop {
        let Some(before) = screen_text(registry, session_id) else {
            return;
        };
        // A word already on screen (a path in the banner, a word from the
        // tips panel) proves nothing, so the probe is chosen against the
        // pre-typing screen.
        let probe = verification_probe(prompt, &before);
        if with_session(registry, session_id, |session| session.paste_text(prompt)).is_none() {
            return;
        }
        match wait_for_echo(registry, session_id, probe.as_deref(), &before, ECHO_WINDOW) {
            EchoOutcome::Gone => return,
            // The composer is holding our text: the Enter is safe.
            EchoOutcome::Visible => {
                submit_typed_prompt(registry, session_id, probe.as_deref());
                return;
            }
            EchoOutcome::Missing => {}
        }

        // Nothing came back. Either the keystrokes were discarded, or this is
        // a reader that paints nothing until it sees a newline (a line-mode
        // shell with echo off). Submitting tells the two apart: the prompt
        // shows up when it landed, and nothing shows up when it did not.
        //
        // This Enter is a keypress into something we cannot see, and some of
        // those things are questions — Codex's "do you trust this directory?"
        // reads Enter as yes. Nothing here can tell a line-mode reader from a
        // dialog: both swallow a paste without repainting, and the difference
        // is canonical vs raw mode, which lives in the holder's pty and not
        // here. The old code sent this same blind Enter on every attempt, so
        // the exposure is unchanged; narrowing it would need the holder to
        // report termios.
        if with_session(registry, session_id, |session| session.submit_input()).is_none() {
            return;
        }
        match wait_for_echo(
            registry,
            session_id,
            probe.as_deref(),
            &before,
            LANDED_WINDOW,
        ) {
            EchoOutcome::Gone | EchoOutcome::Visible => return,
            EchoOutcome::Missing => {}
        }

        // Truly swallowed. Empty the composer before retyping so a late echo
        // cannot concatenate with the retry.
        if with_session(registry, session_id, |session| session.clear_input_line()).is_none() {
            return;
        }
        if !sleep_until(give_up_at, PROMPT_RETRY_DELAY) {
            break;
        }
    }
    eprintln!(
        "dirijord: {session_id} never accepted its initial prompt within \
         {}s — left untyped rather than submitted blind",
        PROMPT_INJECTION_WINDOW.as_secs()
    );
}

/// How long a prompt waits for a composer that will take it. Long enough to
/// outlast a trust dialog or a first-run login, short enough that a session
/// abandoned at a wall does not hold a thread forever.
const PROMPT_INJECTION_WINDOW: Duration = Duration::from_secs(180);

/// Quiet time between delivery attempts.
const PROMPT_RETRY_DELAY: Duration = Duration::from_secs(2);

/// Sleeps for `delay`, or reports false when that would pass `deadline`.
fn sleep_until(deadline: Instant, delay: Duration) -> bool {
    if Instant::now() + delay >= deadline {
        return false;
    }
    std::thread::sleep(delay);
    true
}

/// What the screen said about a prompt we just typed.
enum EchoOutcome {
    /// The prompt is visibly sitting in the composer: safe to submit.
    Visible,
    /// Nothing arrived; the composer can be cleared and the prompt retyped.
    Missing,
    /// The session exited or vanished — stop touching it.
    Gone,
}

/// How long to watch for the prompt to echo back as it is typed, and how long
/// to watch for it after submitting. The first is short because a TUI that
/// renders its composer does so immediately; the second is longer because it
/// covers a round trip through the agent.
const ECHO_WINDOW: Duration = Duration::from_millis(1500);
const LANDED_WINDOW: Duration = Duration::from_millis(2500);

/// Polls for the typed prompt to appear on screen. With no usable probe —
/// every word of the prompt was already on screen — any change from `before`
/// is taken as the echo, which is the best signal available in that case.
fn wait_for_echo(
    registry: &Arc<Mutex<Registry>>,
    session_id: &str,
    probe: Option<&str>,
    before: &str,
    window: Duration,
) -> EchoOutcome {
    let polls = (window.as_millis() / 100).max(1);
    for _ in 0..polls {
        std::thread::sleep(Duration::from_millis(100));
        let Some((exited, now)) = with_session(registry, session_id, |session| {
            (session.view().exited, session.screen_lines().join("\n"))
        }) else {
            return EchoOutcome::Gone;
        };
        if exited {
            return EchoOutcome::Gone;
        }
        let echoed = probe.map_or_else(|| now != before, |probe| now.contains(probe));
        if echoed {
            return EchoOutcome::Visible;
        }
    }
    EchoOutcome::Missing
}

/// Presses Enter on a prompt already verified to be in the composer, and
/// confirms the composer let go of it. A prompt still sitting there after the
/// first Enter gets exactly one more — never a retype, which is what would
/// double-send.
fn submit_typed_prompt(registry: &Arc<Mutex<Registry>>, session_id: &str, probe: Option<&str>) {
    for _ in 0..2 {
        if with_session(registry, session_id, |session| session.submit_input()).is_none() {
            return;
        }
        let Some(probe) = probe else {
            return;
        };
        // Submitting moves the prompt out of the composer and into the
        // transcript above it; either way the agent now owns it. Only a
        // screen that never moved at all means the Enter was swallowed.
        for _ in 0..20 {
            std::thread::sleep(Duration::from_millis(100));
            match screen_text(registry, session_id) {
                None => return,
                Some(now)
                    if !now.contains(probe) || agent_started_working(registry, session_id) =>
                {
                    return;
                }
                Some(_) => {}
            }
        }
    }
}

/// True once the session's own status reducer says the agent is doing
/// something — the prompt was received even if its text is still echoed in
/// the transcript above the composer.
fn agent_started_working(registry: &Arc<Mutex<Registry>>, session_id: &str) -> bool {
    with_session(registry, session_id, |session| {
        matches!(
            session.view().status,
            diri_proto::SessionStatus::Working | diri_proto::SessionStatus::NeedsInput(_)
        )
    })
    .unwrap_or(false)
}

fn screen_text(registry: &Arc<Mutex<Registry>>, session_id: &str) -> Option<String> {
    with_session(registry, session_id, |session| {
        (!session.view().exited).then(|| session.screen_lines().join("\n"))
    })
    .flatten()
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
            // Paste mode says the input line exists; it does NOT say the TUI
            // has stopped repainting over it. Claude Code turns paste mode on
            // while its banner and tips panel are still landing, and anything
            // typed into that window is discarded. Wait for the screen to
            // hold still before treating the composer as real.
            return screen_settled(registry, session_id);
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

/// Waits (≤ ~5s) for the screen to stop changing, so the prompt is typed into
/// a composer that has finished being drawn over. True unless the session
/// exited or vanished; a TUI that simply never goes quiet (an animated
/// spinner in the banner) still gets its prompt, verified by the echo.
fn screen_settled(registry: &Arc<Mutex<Registry>>, session_id: &str) -> bool {
    let mut last = String::new();
    let mut stable_ticks = 0;
    for _ in 0..50 {
        let Some((exited, text)) = with_session(registry, session_id, |session| {
            (session.view().exited, session.screen_lines().join("\n"))
        }) else {
            return false;
        };
        if exited {
            return false;
        }
        if text == last {
            stable_ticks += 1;
            if stable_ticks >= 3 {
                return true;
            }
        } else {
            stable_ticks = 0;
            last = text;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    true
}

/// A fragment of the prompt whose presence on screen means the composer
/// received it.
///
/// It has to be a WHOLE word, not a leading slice: composers soft-wrap, and
/// wrapping happens at word boundaries, so any prefix of the prompt can be
/// split across two screen lines while a single word survives intact. It also
/// has to be absent from `before`, or a word the banner already displays
/// would read as an echo the instant we looked. `None` when the prompt offers
/// nothing that qualifies — a prompt made entirely of words already on
/// screen, or of words too long to escape wrapping.
fn verification_probe(prompt: &str, before: &str) -> Option<String> {
    prompt
        .split_whitespace()
        .filter(|word| (MIN_PROBE_CHARS..=MAX_PROBE_CHARS).contains(&word.chars().count()))
        .filter(|word| !before.contains(*word))
        .max_by_key(|word| word.chars().count())
        .map(str::to_owned)
}

/// Short words appear by coincidence; long ones are the ones a narrow
/// composer breaks mid-word.
const MIN_PROBE_CHARS: usize = 4;
const MAX_PROBE_CHARS: usize = 20;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::detect::ManifestEngine;

    fn engine() -> Arc<ManifestEngine> {
        let dir = crate::detect::bundled_manifest_dir()
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
            host: None,
            remote_persistence: None,
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
        assert_eq!(result["engineKind"], diri_proto::RUST_ENGINE_KIND);
        assert_eq!(
            result["executableHash"].as_str().map(str::len),
            Some(64),
            "the app needs a stable content identity for upgrade coordination"
        );
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

        let cursor = engine.manifest("cursor").expect("cursor manifest");
        let cursor_descriptor = cursor.agent.clone().expect("agent");
        assert!(cursor_descriptor.injection.cursor_mcp);
        assert!(cursor_descriptor.injection.cursor_hooks);
    }

    #[test]
    fn resuming_an_agent_directly_executes_the_agent() {
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
        // Claude declares `returnToLoginShell`, so the agent runs inside the
        // PTY's login shell rather than as its argv[0]; the resume flags still
        // have to reach the agent itself.
        let command = spec.pty.argv.last().expect("argv");
        assert!(
            command.contains("'claude'") && command.contains("'--resume' 'uuid-1'"),
            "resume flags must reach the agent: {command:?}"
        );
    }

    /// An agent that dies on its own — a dropped ssh, a crash — leaves its
    /// session in the registry, because only an explicit kill takes one out.
    /// Resume used to read that presence as "already live", call itself a
    /// no-op and hand the corpse straight back, which left a dead session
    /// with no way at all to restart it.
    #[test]
    fn resume_relaunches_a_session_whose_agent_died_on_its_own() {
        let temp = tempfile::tempdir().expect("temp");
        // A manifest that resumes by flag, onto a binary that outlives the
        // call: `sh -c 'read line'` blocks on the PTY instead of exiting.
        let manifests = temp.path().join("manifests");
        std::fs::create_dir_all(&manifests).expect("manifests dir");
        std::fs::write(
            manifests.join("probe.json"),
            json!({
                "schemaVersion": 2,
                "id": "probe",
                "version": "test",
                "statusModel": "full",
                "agent": {
                    "binary": "/bin/sh",
                    "spawnArgs": ["-c", "read line"],
                    "resume": { "style": "flag", "token": "--resume" },
                },
                "rules": [],
            })
            .to_string(),
        )
        .expect("write manifest");
        let (probe, _) = ManifestEngine::load_dir(&manifests).expect("load");
        let probe = Arc::new(probe);

        let registry = Arc::new(Mutex::new(Registry::new(
            Arc::clone(&probe),
            temp.path().join("state.json"),
        )));
        {
            let mut guard = registry.lock().expect("registry");
            let mut record = test_record("s_dead");
            record.kind = diri_proto::AgentKind::new("probe");
            record.agent_session_id = Some("conv-1".into());
            // `true` exits the moment it is spawned, standing in for the agent
            // that went away while the daemon kept its session.
            guard
                .spawn(
                    crate::session::SessionSpec {
                        id: "s_dead".into(),
                        pty: crate::pty::PtySpec::new(vec!["/usr/bin/true".into()], "/tmp"),
                        manifest_id: "probe".into(),
                        authority: crate::session::authority_for("probe", &probe),
                        logs_dir: temp.path().join("logs"),
                        holder: None,
                        remote: None,
                        defer_launch: false,
                    },
                    record,
                )
                .expect("spawn");
        }
        for _ in 0..100 {
            let exited = registry
                .lock()
                .expect("registry")
                .record("s_dead")
                .is_some_and(|record| {
                    matches!(record.status, diri_proto::SessionStatus::Exited(_))
                });
            if exited {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            registry.lock().expect("registry").get("s_dead").is_some(),
            "the premise: a dead agent's session stays in the registry"
        );

        let server = ControlServer::new(Arc::clone(&registry), temp.path().join("daemon.sock"));
        let result = ok_of(call(
            &server,
            "session.resume",
            Some(json!({ "sessionID": "s_dead" })),
        ));

        assert!(
            result["status"].get("exited").is_none(),
            "resume handed back the corpse instead of relaunching: {}",
            result["status"]
        );
        assert!(
            registry
                .lock()
                .expect("registry")
                .get("s_dead")
                .is_some_and(|session| !session.view().exited),
            "the resumed session must be a live one"
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
    fn remote_spawn_fails_with_the_structured_transport_error() {
        let temp = tempfile::tempdir().expect("temp");
        let server = server(temp.path());
        let error = err_of(call(
            &server,
            "session.spawn",
            Some(json!({
                "kind": { "shell": {} },
                "cwd": "/tmp",
                "host": "forge",
            })),
        ));
        assert_eq!(error.code, crate::remote::TRANSPORT_UNAVAILABLE_CODE);
        assert!(
            server
                .registry
                .lock()
                .expect("registry")
                .records()
                .is_empty(),
            "an unavailable remote transport must not create a session record"
        );
    }

    #[test]
    fn host_initialization_fails_closed_without_the_remote_transport() {
        let temp = tempfile::tempdir().expect("temp");
        diri_proto::HostsConfig {
            hosts: vec![diri_proto::HostEntry {
                id: "forge".into(),
                name: Some("Forge".into()),
                ssh: "you@forge".into(),
                default_cwd: None,
                node: None,
            }],
        }
        .save(temp.path().join("hosts.json"))
        .expect("host catalog");
        let server = server(temp.path());

        let error = err_of(call(
            &server,
            Method::HOST_INITIALIZE,
            Some(json!({ "host": "forge" })),
        ));

        assert_eq!(error.code, crate::remote::TRANSPORT_UNAVAILABLE_CODE);
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

    #[test]
    fn workspace_trust_auto_accept_is_narrowly_scoped_to_claudes_exact_picker() {
        assert!(is_claude_workspace_trust_screen(
            "1. Yes, I trust this folder\n2. No, exit"
        ));
        assert!(!is_claude_workspace_trust_screen(
            "1. Yes, allow this shell command\n2. No"
        ));
        assert!(!is_claude_workspace_trust_screen(
            "Yes, I trust this folder"
        ));
    }

    #[test]
    fn idle_shutdown_requires_exactly_the_requesting_client_and_no_session() {
        assert_eq!(
            idle_shutdown_refusal(1, 1),
            Some("live sessions still require the Engine")
        );
        assert_eq!(
            idle_shutdown_refusal(0, 0),
            Some("request is not associated with a live control connection")
        );
        assert_eq!(
            idle_shutdown_refusal(0, 2),
            Some("another control client still requires the Engine")
        );
        assert_eq!(idle_shutdown_refusal(0, 1), None);
    }

    #[test]
    fn dropping_an_event_subscription_stops_its_detached_thread() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let stop = Arc::new(AtomicBool::new(false));
        let finished = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let worker_finished = Arc::clone(&finished);
        let thread = std::thread::spawn(move || {
            while !worker_stop.load(Ordering::Acquire) {
                std::thread::yield_now();
            }
            worker_finished.store(true, Ordering::Release);
        });
        drop(SubscriptionHandle {
            stop,
            _thread: thread,
        });
        for _ in 0..100 {
            if finished.load(Ordering::Acquire) {
                return;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        panic!("subscription worker did not observe Drop cancellation");
    }
}
