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
mod account_handoff;
mod account_switch;
mod agent_relaunch;
mod agent_sign_in;
mod claude_accounts;
mod codex_accounts;
mod hook_queue;
mod message_delivery;
mod operations;
mod orchestration;
mod schedules;
mod tasks;
mod workspaces;

/// Identifies this engine in the handshake, so a client can tell which
/// implementation it reached.
pub const BUILD: &str = concat!(
    "diri-engine-",
    env!("CARGO_PKG_VERSION"),
    "+catalog.",
    env!("DIRI_AGENT_CATALOG_BUILD_ID")
);

#[cfg(target_os = "macos")]
const fn default_shell() -> &'static str {
    "/bin/zsh"
}

#[cfg(not(target_os = "macos"))]
const fn default_shell() -> &'static str {
    "/bin/sh"
}

/// Requests that act on a Session's terminal or process. A note Session
/// has neither, so these fail with `session_has_no_terminal`.
const TERMINAL_ONLY_METHODS: &[&str] = &[
    Method::SESSION_DELIVER_MESSAGE,
    Method::TASK_SUBMIT,
    Method::SESSION_SEND_KEY,
    Method::SESSION_SEND_TEXT,
    Method::SESSION_RESIZE,
    Method::SESSION_READ_SCREEN,
    Method::SESSION_TERMINAL_TITLE,
    Method::SESSION_RESET_TERMINAL,
    Method::SESSION_CAPTURE_FIND,
    Method::SESSION_READ_SCROLLBACK,
    Method::SESSION_READ_SCROLLBACK_CELLS,
    Method::SESSION_READ_TRANSCRIPT,
    Method::SESSION_RESUME,
    Method::SESSION_RECONNECT,
    Method::SESSION_HIBERNATE,
    Method::SESSION_WAKE,
    Method::SESSION_FORK,
    Method::SESSION_MIGRATE,
    Method::SESSION_CONTINUE_ACCOUNT,
];
/// A terminal's shell: the user's own, as a login shell.
fn login_shell_argv() -> Vec<String> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| default_shell().into());
    vec![shell, "-l".into()]
}

/// Where a fresh shell for an existing local terminal starts: the directory it
/// had `cd`'d to, while that is still an absolute directory on this host.
/// `None` sends it to the launch `cwd`, as before. Remote shells and Agents
/// always start in `cwd`.
fn restored_terminal_directory(record: &diri_proto::SessionRecord) -> Option<PathBuf> {
    if record.kind != diri_proto::AgentKind::SHELL || record.host.is_some() {
        return None;
    }
    record
        .terminal_cwd
        .as_deref()
        .map(PathBuf::from)
        .filter(|path| path.is_absolute() && path.is_dir())
}

pub struct ControlServer {
    engine_instance_id: String,
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
    background_requests: Arc<AtomicUsize>,
    worktree_scan: crate::worktree_scan::ScanStore,
    workspaces: crate::workspace::WorkspaceStore,
    agent_catalog: Arc<Mutex<crate::agent_catalog::AgentCatalogStore>>,
    accounts: Mutex<crate::accounts::AccountStore>,
    account_operations: std::sync::RwLock<()>,
    session_operations: Mutex<std::collections::HashSet<String>>,
    agent_scans: Arc<Mutex<std::collections::HashMap<String, Arc<Mutex<()>>>>>,
    hook_reports: hook_queue::HookQueue,
    /// Where note Sessions keep their files; `None` resolves the standard
    /// notes directory (tests pin a temporary one).
    notes_dir: Option<PathBuf>,
    scheduler: Arc<schedules::Scheduler>,
}

/// Where injection files live and which CLI they point at. Present, spawns
/// become hook-driven and get the dirijor MCP tools.
#[derive(Clone, Debug)]
pub struct InjectionConfig {
    pub inject_dir: PathBuf,
    pub cli_path: PathBuf,
}

#[derive(Clone, Copy)]
enum ConversationAction {
    Resume,
    Fork,
    /// A new provider conversation that keeps an already-minted id, for a tab
    /// whose transcript does not exist yet and therefore cannot be resumed.
    Fresh,
}

/// Whether a local session's execution directory is inside `target`.
///
/// Session cwd values intentionally preserve the spelling supplied at spawn,
/// so raw equality misses symlink aliases and sessions started below the
/// checkout root. Canonicalize when possible and retain a lexical fallback
/// for a live process whose cwd was unlinked after it started.
fn local_session_uses_worktree(record: &diri_proto::SessionRecord, target: &Path) -> bool {
    if record.host.is_some() {
        return false;
    }
    record
        .worktree_path
        .as_deref()
        .into_iter()
        .chain(std::iter::once(record.cwd.as_str()))
        .any(|path| {
            std::fs::canonicalize(path).map_or_else(
                |_| {
                    let path = Path::new(path);
                    path == target || path.starts_with(target)
                },
                |path| path == target || path.starts_with(target),
            )
        })
}

impl ControlServer {
    /// Configure private power endpoints before starting the scheduler.
    pub fn with_schedule_power(mut self, config: crate::wake::PowerConfig) -> Self {
        Arc::get_mut(&mut self.scheduler)
            .expect("scheduler not started")
            .power = config;
        self
    }

    pub fn new(registry: Arc<Mutex<Registry>>, socket_path: impl Into<PathBuf>) -> Self {
        // Capture the bytes this process actually started from before an app
        // updater can replace the bundle path underneath the live daemon.
        let _ = process_executable_hash();
        let socket_path = socket_path.into();
        let workspaces = crate::workspace::WorkspaceStore::with_state_file(
            registry.lock().expect("registry").state_file_handle(),
        );
        let logs_dir = socket_path
            .parent()
            .map(|parent| parent.join("logs"))
            .unwrap_or_else(|| PathBuf::from("logs"));
        let remote_bindings = socket_path.parent().and_then(|parent| {
            crate::remote::binding::RemoteBindingStore::new(parent.join("remote-bindings")).ok()
        });
        let agent_config_path = socket_path
            .parent()
            .map(|parent| parent.join("agents.json"))
            .unwrap_or_else(|| PathBuf::from("agents.json"));
        let agent_catalog = crate::agent_catalog::AgentCatalogStore::new(&agent_config_path)
            .unwrap_or_else(|error| {
                eprintln!("diri-engine: Agent configuration unavailable: {error}");
                crate::agent_catalog::AgentCatalogStore::empty(&agent_config_path)
            });
        let accounts = Mutex::new(crate::accounts::AccountStore::new(
            agent_config_path.with_file_name("accounts.json"),
        ));
        let events = crate::events::EventBus::new();
        let activity_path = logs_dir
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(diri_proto::paths::ACTIVITY_LOG_FILE_NAME);
        if let Err(error) = events.enable_activity_log(activity_path) {
            eprintln!("diri-engine: activity history unavailable: {error}");
        }
        Self {
            engine_instance_id: {
                let mut bytes = [0_u8; 16];
                getrandom::fill(&mut bytes).expect("the OS random source");
                bytes.iter().map(|byte| format!("{byte:02x}")).collect()
            },
            registry,
            socket_path,
            logs_dir,
            holder: None,
            remote: None,
            remote_bindings,
            events,
            attach: crate::attach::AttachHub::new(),
            pr_monitor_wake: crate::pr_monitor::PrMonitorWake::default(),
            injection: None,
            governor: std::sync::Arc::new(Mutex::new(crate::governor::GovernorConfig::default())),
            browser: std::sync::OnceLock::new(),
            active_connections: Arc::new(AtomicUsize::new(0)),
            background_requests: Arc::new(AtomicUsize::new(0)),
            worktree_scan: Default::default(),
            workspaces,
            agent_catalog: Arc::new(Mutex::new(agent_catalog)),
            accounts,
            account_operations: std::sync::RwLock::new(()),
            session_operations: Mutex::new(std::collections::HashSet::new()),
            agent_scans: Arc::new(Mutex::new(std::collections::HashMap::new())),
            hook_reports: hook_queue::HookQueue::new(),
            notes_dir: None,
            scheduler: Arc::new(schedules::Scheduler::default()),
        }
    }

    /// Enables spawn-time hook/MCP injection: writes the shim files (like the
    /// Swift daemon does at startup) and applies each manifest's mechanisms
    /// to future spawns.
    pub fn with_injection(mut self, config: InjectionConfig) -> Self {
        let _ = crate::inject::write_claude_hooks_file(&config.inject_dir);
        let _ = crate::inject::write_claude_mcp_file(&config.inject_dir, &config.cli_path);
        let _ = crate::inject::write_claude_skills_plugin(&config.inject_dir);
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
    pub fn with_notes_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.notes_dir = Some(dir.into());
        self
    }

    pub fn with_logs_dir(mut self, logs_dir: impl Into<PathBuf>) -> Self {
        self.logs_dir = logs_dir.into();
        let activity_path = self
            .logs_dir
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(diri_proto::paths::ACTIVITY_LOG_FILE_NAME);
        if let Err(error) = self.events.enable_activity_log(activity_path) {
            eprintln!("diri-engine: activity history unavailable: {error}");
        }
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
                let Some(manager) = manager else {
                    return;
                };
                let started = Instant::now();
                let adopted = server.restore_remote_bindings(&manager);
                diri_telemetry::event!(
                    "engine.remote_restore",
                    adopted = adopted.len(),
                    ms = started.elapsed(),
                );
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
        // Clone the engine under the lock, then let it go. Every step below is an SSH
        // round trip, and a delegated fleet is dozens of bindings on one host:
        // holding the Registry across all of them blocked attaches, hook
        // reports, and readiness probes for minutes after boot, which the app
        // showed as a blank pane for every session, local ones included.
        let engine = match self.registry.lock() {
            Ok(registry) => registry.engine(),
            Err(_) => return Vec::new(),
        };
        // One Helper probe per host and build rather than one per session.
        let mut helpers = std::collections::HashMap::<
            (String, String, u16),
            Option<crate::remote::manager::InstalledHelper>,
        >::new();
        let mut adopted = Vec::new();
        for binding in bindings {
            // Startup restore and user lifecycle operations must reserve the
            // same identity while SSH runs. Re-read the record under that
            // reservation: a prior stop/resume may have replaced the snapshot
            // captured when the bindings directory was enumerated.
            let Ok(_operation) =
                account_handoff::SessionOperation::for_session(self, &binding.session_id)
            else {
                continue;
            };
            let record = {
                let Ok(registry) = self.registry.lock() else {
                    break;
                };
                if registry.get(&binding.session_id).is_some() {
                    continue;
                }
                let Some(record) = registry.record(&binding.session_id) else {
                    continue;
                };
                record
            };
            if record.host.as_deref() != Some(&binding.host_id) {
                continue;
            }
            let Some(host) = hosts.host(&binding.host_id) else {
                continue;
            };
            let helper_key = (
                binding.host_id.clone(),
                binding.helper_build_id.clone(),
                binding.protocol.major,
            );
            let helper = helpers.entry(helper_key).or_insert_with(|| {
                manager
                    .existing_helper(host, &binding.helper_build_id, binding.protocol)
                    .ok()
            });
            let Some(helper) = helper.clone() else {
                diri_telemetry::warn_event!(
                    "remote.restore_skipped",
                    session = diri_telemetry::id(&binding.session_id),
                    host = diri_telemetry::id(&binding.host_id),
                    reason = "helper_unavailable",
                );
                continue;
            };
            let selector = diri_proto::remote_pty::SessionSelector {
                session_id: binding.session_id.clone(),
                session_token: binding.session_token.clone(),
                expected_incarnation: Some(binding.session_incarnation.clone()),
            };
            let inspection = match manager.inspect(&helper, &selector) {
                Ok(inspection) => inspection,
                Err(error) => {
                    diri_telemetry::warn_event!(
                        "remote.restore_skipped",
                        session = diri_telemetry::id(&binding.session_id),
                        host = diri_telemetry::id(&binding.host_id),
                        reason = "inspect_failed",
                        io = diri_telemetry::io_error(&error),
                    );
                    continue;
                }
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
                authority: crate::session::authority_for(&manifest_id, &engine),
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
            // Adoption itself is local (the attach channel opens on the
            // session's own pump thread), so the lock is held only here.
            // `adopt_remote` re-checks the record, which covers a session
            // removed while its inspection was in flight.
            let Ok(mut registry) = self.registry.lock() else {
                break;
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
    pub fn serve(self: &Arc<Self>, stream: UnixStream) -> std::io::Result<()> {
        let _connection = ActiveConnectionGuard::new(Arc::clone(&self.active_connections));
        diri_telemetry::count("engine.connections", 1);
        let mut reader = BufReader::new(stream.try_clone()?);
        let writer = Arc::new(Mutex::new(stream));
        let mut subscription: Option<SubscriptionHandle> = None;

        let mut first = true;
        loop {
            let Some(line) = read_bounded_control_line(&mut reader)? else {
                return Ok(());
            };
            if line.is_empty() {
                continue;
            }
            if first {
                first = false;
                if serde_json::from_slice::<serde_json::Value>(&line)
                    .is_ok_and(|value| value.get("preview_set").is_some())
                {
                    if let Ok(request) =
                        serde_json::from_slice::<diri_proto::preview_set::PreviewSetRequest>(&line)
                        && request.preview_set
                        && request.version == diri_proto::preview_set::PREVIEW_SET_VERSION
                    {
                        let buffered = reader.buffer().to_vec();
                        return self.attach.serve_preview_set(
                            &self.registry,
                            reader.into_inner(),
                            buffered,
                        );
                    }
                    return Ok(());
                }
                // Route by the distinct key before normal attach decoding. Mixed
                // or unsupported requests fail closed without visibility effects.
                if serde_json::from_slice::<serde_json::Value>(&line)
                    .is_ok_and(|value| value.get("preview").is_some())
                {
                    if let Ok(request) =
                        serde_json::from_slice::<diri_proto::preview::PreviewRequest>(&line)
                        && request.version == diri_proto::preview::PREVIEW_VERSION
                    {
                        let buffered = reader.buffer().to_vec();
                        self.attach.serve_preview(
                            &self.registry,
                            &request.preview.0,
                            reader.into_inner(),
                            buffered,
                            writer,
                        );
                    }
                    return Ok(());
                }
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
                        // A refusal is definitive: say why, then close, so the
                        // client stops retrying instead of reconnecting forever.
                        let rejection = if registry.is_note(&attach.attach.0) {
                            Some(diri_proto::frames::AttachRejection::NotTerminal)
                        } else if registry.get(&attach.attach.0).is_some_and(|session| {
                            !session.allows_keyboard_controller(attach.enhanced_keyboard)
                        }) {
                            Some(diri_proto::frames::AttachRejection::KeyboardUnsupported)
                        } else {
                            None
                        };
                        if let Some(rejection) = rejection {
                            drop(registry);
                            self.attach.reject(&writer, &attach.attach.0, rejection);
                            return Ok(());
                        }
                        let _ = registry.ensure_session_awake(&attach.attach.0);
                        let _ = registry.mark_seen(&attach.attach.0);
                        // Every tab switch attaches: leave the write to the
                        // flusher instead of fsyncing on the attach path.
                        registry.persist_deferred();
                        self.publish_updated(&registry, &attach.attach.0);
                    }
                    self.pr_monitor_wake.wake_session(attach.attach.0.clone());
                    // Bytes the line reader buffered past the attach line are
                    // already binary frames; hand them over.
                    let buffered = reader.buffer().to_vec();
                    self.attach.serve_with_keyboard(
                        &self.registry,
                        &attach.attach.0,
                        attach.enhanced_keyboard,
                        reader.into_inner(),
                        buffered,
                        writer,
                    );
                    return Ok(());
                }
            }
            let Some(response) = self.handle_line(&line, &writer, &mut subscription) else {
                continue;
            };
            write_message(&writer, &response)?;
        }
    }

    fn handle_line(
        self: &Arc<Self>,
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
            ControlMessage::Request { id, method, params }
                if matches!(
                    method.as_str(),
                    Method::SESSION_SPAWN
                        | Method::SESSION_SPAWN_TRACKED
                        | Method::SESSION_CONTINUE_ACCOUNT
                        | Method::ACCOUNT_SWITCH_ALL
                        | Method::ACCOUNT_CODEX_LOGIN
                        | Method::ACCOUNT_CLAUDE_LOGIN
                        | Method::HOST_INITIALIZE
                        | Method::HOST_USAGE
                        | Method::HOST_LIST_DIRECTORIES
                        | Method::SESSION_READ_DIFF
                        | Method::WORKTREE_INTEGRATE
                        | Method::TASK_ANSWER
                        | Method::TASK_CANCEL
                        | Method::SESSION_CAPTURE_FIND
                        | Method::SESSION_READ_SCROLLBACK_CELLS
                        | Method::SESSION_KILL
                        | Method::SESSION_REMOVE
                        | Method::SESSION_ARCHIVE
                        | Method::SESSION_RESUME
                        | Method::SESSION_PROCESS_INFO
                        | Method::SESSION_RECONNECT
                        | Method::SESSION_FORK
                        | Method::SESSION_MIGRATE
                        | Method::WORKTREE_OVERVIEW
                        | Method::WORKTREE_CLEANUP
                        | Method::TELEMETRY_UPLOAD_NOW
                ) || ((method == Method::AGENT_READINESS
                    || method == Method::AGENT_CONFIGURE)
                    && params
                        .as_ref()
                        .and_then(|value| value.get("host"))
                        .and_then(Value::as_str)
                        .is_some()) =>
            {
                // These operations can wait on SSH, Git, or the agent's first
                // prompt. Keep this connection readable for input, snapshots
                // and Hello; otherwise even a private client hits its heartbeat
                // timeout, and cannot answer the prompt that spawn is waiting on.
                // The cap bounds cold-path threads; no PTY hot path changes.
                let Some(permit) = BackgroundRequest::acquire(&self.background_requests) else {
                    return Some(ControlMessage::Response {
                        id,
                        result: Err(ControlError::new(
                            "busy",
                            "Too many operations are starting. Wait for one to finish and retry.",
                        )),
                    });
                };
                let server = Arc::clone(self);
                let writer = Arc::clone(writer);
                let spawned = std::thread::Builder::new()
                    .name("dirijord-background-request".into())
                    .spawn(move || {
                        let _permit = permit;
                        let response = ControlMessage::Response {
                            id,
                            result: server.dispatch(&method, params),
                        };
                        let _ = write_message(&writer, &response);
                    });
                match spawned {
                    Ok(_) => None,
                    Err(error) => Some(ControlMessage::Response {
                        id,
                        result: Err(ControlError::internal(format!(
                            "could not start the background request: {error}"
                        ))),
                    }),
                }
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
                        if write_event_frame(&writer, &event).is_err() {
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
            registry.record(&p.session_id.0)
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
        if !diri_telemetry::is_enabled() {
            return self.dispatch_method(method, params);
        }
        let session = crate::telemetry::request_session(params.as_ref());
        let started = Instant::now();
        let result = self.dispatch_method(method, params);
        let elapsed = started.elapsed();
        crate::telemetry::record_rpc(method, elapsed, result.as_ref().err(), session.as_deref());
        if result.is_ok() && crate::telemetry::is_lifecycle_method(method) {
            diri_telemetry::event!(
                "rpc.op",
                method = diri_telemetry::id(method),
                session = session.as_deref().map(diri_telemetry::id),
                ms = elapsed,
            );
        }
        result
    }

    fn dispatch_method(
        &self,
        method: &str,
        params: Option<JsonValue>,
    ) -> Result<JsonValue, ControlError> {
        if TERMINAL_ONLY_METHODS.contains(&method) {
            self.reject_note_terminal(method, params.as_ref())?;
        }
        // Bulk switching excludes concurrent launches/catalog edits without blocking input,
        // output, snapshots, or unrelated read-only requests.
        let _account_switch = if matches!(
            method,
            Method::ACCOUNT_SWITCH_ALL
                | Method::ACCOUNT_CODEX_LOGIN
                | Method::ACCOUNT_CODEX_CAPTURE
                | Method::ACCOUNT_CLAUDE_LOGIN
                | Method::ACCOUNT_CLAUDE_CAPTURE
        ) {
            Some(self.account_operations.try_write().map_err(|_| ControlError::bad_request("An account or session operation is already in progress. Retry when it finishes."))?)
        } else {
            None
        };
        let _account_use = if matches!(
            method,
            Method::SESSION_SPAWN
                | Method::SESSION_RESUME
                | Method::SESSION_FORK
                | Method::SESSION_CONTINUE_ACCOUNT
                | Method::ACCOUNT_PROFILES_SAVE
                | Method::ACCOUNT_PROFILES_REMOVE
                | Method::SESSION_RESUME_FROM_HISTORY
                | Method::SESSION_MIGRATE
                | Method::SESSION_WAKE
                | Method::SESSION_HIBERNATE
        ) {
            Some(self.account_operations.try_read().map_err(|_| {
                ControlError::bad_request(
                    "An account switch is in progress. Retry when it finishes.",
                )
            })?)
        } else {
            None
        };
        let _operation = account_handoff::SessionOperation::acquire(self, method, params.as_ref())?;
        match method {
            Method::ACCOUNT_SWITCH_ALL => self.account_switch_all(params),
            Method::ACCOUNT_CODEX_LOGIN => self.codex_account_login(params),
            Method::ACCOUNT_CODEX_CAPTURE => self.codex_account_capture(params),
            Method::ACCOUNT_CLAUDE_LOGIN => self.claude_account_login(params),
            Method::ACCOUNT_CLAUDE_CAPTURE => self.claude_account_capture(params),
            Method::SESSION_CONTINUE_ACCOUNT => self.session_continue_account(params),
            Method::ACCOUNT_PROFILES_LIST => {
                encode(&self.accounts.lock().map_err(poisoned)?.catalog()?)
            }
            Method::ACCOUNT_PROFILES_SAVE => {
                let profile: diri_proto::AgentAccountProfile = decode(params)?;
                if let Some(host) = &profile.host {
                    self.resolve_host(host)?;
                }
                encode(&self.accounts.lock().map_err(poisoned)?.upsert(profile)?)
            }
            Method::ACCOUNT_PROFILES_REMOVE => {
                let params: diri_proto::AgentAccountId = decode(params)?;
                encode(&self.accounts.lock().map_err(poisoned)?.remove(&params.id)?)
            }
            Method::WORKSPACE_SNAPSHOT => encode(&self.workspaces.snapshot()?),
            Method::WORKSPACE_MUTATE => self.workspace_mutate(params),
            Method::HELLO => self.hello(params),
            Method::SESSION_SPAWN => self.session_spawn(params),
            Method::SESSION_SPAWN_TRACKED => self.session_spawn_tracked(params),
            Method::TASK_SUBMIT => self.task_submit(params),
            Method::TASK_GET => self.task_get(params),
            Method::TASK_REPORT => self.task_report(params),
            Method::TASK_ANSWER => self.task_answer(params),
            Method::TASK_CANCEL => self.task_cancel(params),
            Method::TASK_LIST => self.task_list(params),
            Method::SCHEDULE_CREATE => self.schedule_create(params),
            Method::SCHEDULE_UPDATE => self.schedule_update(params),
            Method::SCHEDULE_DELETE => self.schedule_delete(params),
            Method::SCHEDULE_LIST => self.schedule_list(),
            Method::SCHEDULE_RUN_NOW => self.schedule_run_now(params),
            Method::SESSION_LIST | Method::STATE_SNAPSHOT => self.session_list(),
            Method::SESSION_DELIVER_MESSAGE => self.session_deliver_message(params),
            Method::SESSION_SEND_KEY => self.session_send_key(params),
            Method::SESSION_SEND_TEXT => self.session_send_text(params),
            Method::SESSION_RESIZE => self.session_resize(params),
            Method::SESSION_READ_SCREEN => self.session_read_screen(params),
            Method::SESSION_TERMINAL_TITLE => self.session_terminal_title(params),
            Method::SESSION_RESET_TERMINAL => self.session_reset_terminal(params),
            Method::SESSION_CAPTURE_FIND => self.session_capture_find(params),
            Method::SESSION_READ_SCROLLBACK => self.session_read_scrollback(params),
            Method::SESSION_READ_SCROLLBACK_CELLS => self.session_read_scrollback_cells(params),
            Method::SESSION_KILL => self.session_kill(params),
            Method::SESSION_REMOVE => self.session_remove(params),
            Method::SESSION_RENAME => self.session_rename(params),
            Method::SESSION_MARK_SEEN => self.session_mark_seen(params),
            Method::SESSION_MARK_UNREAD => self.session_mark_unread(params),
            Method::SESSION_ARCHIVE => self.session_archive(params),
            Method::SESSION_UNARCHIVE => self.session_unarchive(params),
            Method::SESSION_HISTORY => self.session_history(),
            Method::ACTIVITY_LIST => self.activity_list(params),
            Method::WORKTREE_CREATE => self.worktree_create(params),
            Method::WORKTREE_LIST => self.worktree_list(params),
            Method::WORKTREE_CLEANUP => self.worktree_cleanup(params),
            Method::WORKTREE_REMOVE => self.worktree_remove(params),
            Method::WORKTREE_SCAN => encode(&self.worktree_scan_page(decode(params)?)?),
            Method::WORKTREE_OVERVIEW => self.worktree_overview(),
            Method::TEST_RUN => self.browser_call("run", params),
            "browser.act" => self.browser_call("browser", params),
            Method::EVENTS_WAIT => self.events_wait(params),
            Method::HOST_SYNC_PREFS => self.host_sync_prefs(params),
            Method::HOST_INITIALIZE => self.host_initialize(params),
            Method::HOST_USAGE => self.host_usage(params),
            Method::HOST_LIST_DIRECTORIES => self.host_list_directories(params),
            Method::HOST_LIST => Ok(
                json!({"hosts": diri_proto::HostsConfig::load(self.hosts_file())
                .hosts.iter().map(|host| json!({
                    "id": host.id, "name": host.display_name(), "defaultCwd": host.default_cwd
                })).collect::<Vec<_>>() }),
            ),
            Method::SESSION_MIGRATE => self.session_migrate(params),
            Method::SESSION_REPARENT_WORKTREE => self.session_reparent_worktree(params),
            Method::HOST_LOCATE_REPO => self.host_locate_repo(params),
            Method::HOOK_REPORT => self.hook_report(params),
            Method::SESSION_RESUME => self.session_resume(params),
            Method::SESSION_PROCESS_INFO => self.session_process_info(params),
            Method::SESSION_RECONNECT => self.session_reconnect(params),
            Method::SESSION_FORK => self.session_fork(params),
            Method::SESSION_RESUME_FROM_HISTORY => self.session_resume_from_history(params),
            Method::SESSION_REOPEN_LAST => self.session_reopen_last(),
            Method::SESSION_REVEAL => self.session_reveal(params),
            Method::AGENT_READINESS => self.agent_readiness(params),
            Method::AGENT_CONFIGURE => self.agent_configure(params),
            Method::PROJECT_ADD => self.project_add(params),
            Method::SESSION_READ_DIFF => self.session_read_diff(params),
            Method::SESSION_READ_TRANSCRIPT => self.session_read_transcript(params),
            Method::WORKTREE_INTEGRATE => self.worktree_integrate(params),
            Method::SESSION_HIBERNATE => self.session_hibernate(params),
            Method::SESSION_WAKE => self.session_wake(params),
            Method::DAEMON_PREPARE_SHUTDOWN => self.daemon_prepare_shutdown(),
            Method::DAEMON_SHUTDOWN_IF_IDLE => self.daemon_shutdown_if_idle(),
            Method::DAEMON_SHUTDOWN => self.daemon_shutdown(),
            Method::TELEMETRY_UPLOAD_NOW => telemetry_upload_now(),
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
        diri_telemetry::event!(
            "client.hello",
            proto = proto,
            build = params
                .as_ref()
                .and_then(|value| value.get("build"))
                .and_then(Value::as_str)
                .map(diri_telemetry::id),
            ok = proto == WIRE_VERSION as u64,
        );
        if proto != WIRE_VERSION as u64 {
            return Err(ControlError::version_mismatch(format!(
                "client speaks protocol {proto}, this engine speaks {WIRE_VERSION}"
            )));
        }
        Ok(json!({
            "proto": WIRE_VERSION,
            "build": BUILD,
            "engineKind": diri_proto::RUST_ENGINE_KIND,
            "engineInstanceId": self.engine_instance_id,
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
        self.session_spawn_identified(params, None, None)
    }

    fn session_spawn_identified(
        &self,
        params: Option<JsonValue>,
        reserved_id: Option<String>,
        scheduled: Option<diri_proto::schedules::ScheduledRunInfo>,
    ) -> Result<JsonValue, ControlError> {
        let raw = params.ok_or_else(|| ControlError::bad_request("params are required"))?;
        // Validate before any account, worktree, or remote side effect. Missing
        // argv keeps manifest launch behavior; an explicit malformed argv must
        // never silently drop arguments or fall back to a login shell.
        let argv = decode_launch_argv(&raw)?;
        let p: diri_proto::SessionSpawnParams = decode(Some(raw))?;
        // A note runs nothing: no account, argv, or holder applies.
        if p.kind.id() == diri_proto::AgentKind::NOTE_ID {
            return self.session_spawn_note(p, reserved_id);
        }
        let mut account_profile = self.accounts.lock().map_err(poisoned)?.resolve(
            p.account_profile_id.as_deref(),
            p.kind.id(),
            p.host.as_deref(),
        )?;
        if p.host.is_some() {
            return self.session_spawn_remote(p, argv, account_profile, reserved_id, scheduled);
        }
        if let Some(profile) = &account_profile
            && profile.agent == "codex"
            && !profile.is_default
            && (profile.config_home == "~/.codex"
                || std::env::var("HOME").is_ok_and(|home| {
                    PathBuf::from(home).join(".codex") == Path::new(&profile.config_home)
                }))
        {
            return Err(ControlError::bad_request(
                "Select this Codex account in the bottom-left menu before starting a conversation. Shared-home profiles use the active login.",
            ));
        }

        let kind = p.kind.id().to_string();
        // A generic kind carries the user's command line inside itself.
        let argv = if argv.is_empty() {
            match p.kind.command() {
                Some(command) if !command.is_empty() => {
                    let shell = std::env::var("SHELL").unwrap_or_else(|_| default_shell().into());
                    vec![shell, "-lc".into(), command.to_string()]
                }
                _ if kind == diri_proto::AgentKind::SHELL_ID => login_shell_argv(),
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
            let info = crate::git::create_worktree(
                Path::new(&p.cwd),
                p.worktree_branch.as_deref(),
                p.worktree_base.as_deref(),
            )
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
        let mut descriptor = manifest.agent.clone().unwrap_or_default();
        if let Some(binary) = descriptor.binary.as_deref() {
            descriptor.binary = Some(self.resolve_local_agent_executable(&kind, binary)?);
        }
        let authority = descriptor.authority();

        let tracked = reserved_id.is_some();
        let id = reserved_id.unwrap_or_else(next_session_id);
        // Build the complete agent argv before `spawn_spec`: agents declaring
        // `returnToLoginShell` need every manifest and injection argument
        // quoted inside the shell's `-c` command.
        let mut launch_args = argv.clone();
        let mut agent_session_id = None;
        if descriptor.binary.is_some() {
            launch_args.extend(descriptor.spawn_args.iter().cloned());
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
            let minted = descriptor
                .mints_conversation_id()
                .then(crate::inject::uuid_v4);
            let provider_dir = registry.recovery_directory(&id).join("provider");
            let plan = descriptor
                .conversation_plan(
                    &launch_args,
                    crate::agent::ConversationLaunch::Fresh {
                        new_id: minted.as_deref(),
                        session_dir: Some(&provider_dir),
                    },
                )
                .ok_or_else(|| {
                    ControlError::bad_request(format!(
                        "agent {kind:?} has an incomplete fresh conversation grammar"
                    ))
                })?;
            launch_args = plan.args;
            agent_session_id = plan.agent_session_id;
        }

        let inherited: Vec<(String, String)> = std::env::vars().collect();
        // A terminal may start where another terminal had `cd`'d to, while
        // `cwd` keeps it in the project it was opened from.
        let start_directory = p
            .start_directory
            .as_deref()
            .filter(|_| kind == diri_proto::AgentKind::SHELL_ID)
            .map(PathBuf::from)
            .filter(|path| path.is_absolute() && path.is_dir());
        let launch_path = start_directory.as_deref().unwrap_or(&cwd_path);
        let mut pty = match descriptor.spawn_spec(launch_path, inherited.clone(), &launch_args) {
            Some(spec) => spec,
            // No binary in the manifest: the caller has to say what to run.
            None if !argv.is_empty() => {
                let mut spec = crate::pty::PtySpec::new(argv.clone(), launch_path);
                spec.env = inherited;
                // GUI apps launched by launchd commonly inherit no terminal
                // environment. A binary-free descriptor is still attached to
                // Diri's colour-capable PTY, so assert the same capabilities
                // as manifest-backed Agents instead of leaving tools such as
                // `clear` unable to operate.
                crate::agent::assert_color_environment(&mut spec.env);
                spec
            }
            None => {
                return Err(ControlError::bad_request(format!(
                    "agent {kind:?} declares no binary, so argv is required"
                )));
            }
        };

        if let Some(profile) = &mut account_profile {
            crate::accounts::bind_pty(profile, &mut pty)?;
        }
        let mut record = new_record(&id, &kind, &cwd);
        record.scheduled_run = scheduled;
        record.terminal_cwd = start_directory.map(|path| path.to_string_lossy().into_owned());
        record.account_profile = account_profile;
        record.kind = p.kind.clone();
        record.originating_prompt = p.initial_prompt.clone();
        // A linked worktree is an execution cwd inside the project selected
        // by the user; it does not become a new first-level sidebar project.
        record.project_id = crate::registry::session_project_id(&p.cwd, None);
        self.ensure_published_project(&mut registry, &p.cwd, None);
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
                pty.env.push((
                    diri_proto::paths::ENV_SESSION_RECOVERY_DIR.into(),
                    registry
                        .recovery_directory(&id)
                        .to_string_lossy()
                        .into_owned(),
                ));
                if let Some(dir) = self.resolved_notes_dir() {
                    pty.env.push((
                        diri_proto::paths::ENV_NOTES_DIR.into(),
                        dir.to_string_lossy().into_owned(),
                    ));
                }
            }
            if let Some(uuid) = &agent_session_id {
                record.agent_session_id = Some(uuid.clone());
                if descriptor.injection.claude_hooks
                    && let Ok(home) = std::env::var("HOME")
                {
                    record.transcript_path = Some(
                        record
                            .account_profile
                            .as_ref()
                            .map_or_else(
                                || {
                                    crate::inject::claude_transcript_path(
                                        Path::new(&home),
                                        &cwd,
                                        uuid,
                                    )
                                },
                                |profile| {
                                    Path::new(&profile.config_home)
                                        .join("projects")
                                        .join(crate::inject::claude_project_slug(&cwd))
                                        .join(format!("{uuid}.jsonl"))
                                },
                            )
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
        if tracked {
            // Persist the launch intent before a Holder can exist. A crash
            // immediately after launch must leave a record for binding adoption.
            registry.insert_record(record.clone());
            registry.persist_for_shutdown().map_err(io_control_error)?;
        }
        let spawn_started = Instant::now();
        registry
            .spawn(spec, record)
            .map_err(|error| ControlError::internal(error.to_string()))?;
        if let Some(record) = registry.record(&id) {
            crate::telemetry::record_session_spawn(
                &record,
                "fresh",
                spawn_started.elapsed(),
                registry.other_live_agent(&id),
            );
        }
        if tracked {
            registry.persist_for_shutdown().map_err(io_control_error)?;
        } else {
            let _ = registry.persist();
        }
        self.publish_updated(&registry, &id);

        // An initial prompt is typed once the TUI can actually receive input,
        // and verified on screen afterward — ported from the Swift
        // `injectInitialPrompt`, which replaced a blind fixed delay that
        // raced Claude Code's boot and lost keystrokes into a composer that
        // did not exist yet.
        let prompt = p.initial_prompt.clone().filter(|prompt| !prompt.is_empty());
        let appearance = p.appearance;
        let accept_claude_workspace = kind == diri_proto::AgentKind::CLAUDE_CODE_ID;
        let record = registry
            .record(&id)
            .ok_or_else(|| ControlError::internal("the new session vanished"))?;
        drop(registry);

        // A supplied prompt is part of the spawn contract: do not acknowledge
        // the request until delivery is confirmed. With no prompt, Claude's
        // workspace-trust convenience remains background work so an ordinary
        // launch still returns as soon as its session exists.
        if let Some(prompt) = prompt {
            prepare_agent_input(
                &self.registry,
                &id,
                accept_claude_workspace,
                appearance,
                Some(&prompt),
            )
            .map_err(|error| initial_prompt_control_error(&id, error))?;
        } else if accept_claude_workspace {
            let registry = Arc::clone(&self.registry);
            let session_id = id.clone();
            std::thread::spawn(move || {
                let _ = prepare_agent_input(&registry, &session_id, true, appearance, None);
            });
        }

        // SessionSpawnResult is the record itself, as the Swift daemon
        // answers — not wrapped.
        serde_json::to_value(&record).map_err(|error| ControlError::internal(error.to_string()))
    }

    /// A note has no terminal: typing into it, reading its screen, or
    /// resuming it can never succeed, so every client learns that at once
    /// instead of waiting on a PTY that will never exist.
    fn reject_note_terminal(
        &self,
        method: &str,
        params: Option<&JsonValue>,
    ) -> Result<(), ControlError> {
        let Some(id) = crate::telemetry::request_session(params) else {
            return Ok(());
        };
        if !self.registry.lock().map_err(poisoned)?.is_note(&id) {
            return Ok(());
        }
        Err(ControlError::new(
            diri_proto::control::SESSION_HAS_NO_TERMINAL,
            format!(
                "{id} is a note, which has no terminal; {method} applies only to agents and terminals"
            ),
        ))
    }

    /// A note Session: a record with no process, backed by a Markdown file
    /// in the notes store. The file is created first so a record can never
    /// point at a note that does not exist. `initial_prompt` seeds the body
    /// (Markdown), which is how a PRD or handoff arrives pre-written. With
    /// `note_id` the Session adopts that existing file instead.
    fn session_spawn_note(
        &self,
        p: diri_proto::SessionSpawnParams,
        reserved_id: Option<String>,
    ) -> Result<JsonValue, ControlError> {
        if p.host.is_some() {
            return Err(ControlError::bad_request(
                "notes are stored on this Mac; they cannot be opened on a remote host",
            ));
        }
        if p.new_worktree.unwrap_or(false) {
            return Err(ControlError::bad_request("a note has no worktree"));
        }
        let store = self.note_store()?;
        let id = reserved_id.unwrap_or_else(next_session_id);
        if let Some(note_id) = p.note_id.as_deref() {
            let requested = Some(p.cwd.trim()).filter(|cwd| !cwd.is_empty());
            let record = self.adopt_note(&store, note_id, requested, p.parent.clone(), id)?;
            return serde_json::to_value(&record)
                .map_err(|error| ControlError::internal(error.to_string()));
        }
        let cwd = p.cwd.trim().to_owned();
        if cwd.is_empty() || !Path::new(&cwd).is_dir() {
            return Err(ControlError::bad_request(format!(
                "cwd {cwd:?} is not a directory"
            )));
        }
        let title = p.title.clone().unwrap_or_default();
        let body = p.initial_prompt.clone().unwrap_or_default();
        let (_, parsed) = diri_notes::markdown::parse(&format!("\n{body}"));
        let doc = diri_notes::doc::Document::new(title.trim(), parsed.blocks);
        let (note_id, _) = store
            .create_for_session(doc, Some(&cwd), Some(&id), &note_author(p.parent.as_ref()))
            .map_err(io_control_error)?;
        let record = note_record(&id, &cwd, title.trim(), note_id, p.parent.clone());
        self.insert_note_record(record.clone())?;
        serde_json::to_value(&record).map_err(|error| ControlError::internal(error.to_string()))
    }

    /// Gives an existing notes file its Session. Idempotent per note id: a
    /// file that already has a Session gets that one back. The file keeps its
    /// id and created date; only its `session` stamp is written.
    fn adopt_note(
        &self,
        store: &diri_notes::store::NoteStore,
        note_id: &str,
        requested_cwd: Option<&str>,
        parent: Option<diri_proto::SessionId>,
        id: String,
    ) -> Result<diri_proto::SessionRecord, ControlError> {
        let meta = store.meta(note_id).map_err(|error| match error.kind() {
            std::io::ErrorKind::NotFound => ControlError::not_found(format!("no note {note_id:?}")),
            std::io::ErrorKind::InvalidInput => {
                ControlError::bad_request(format!("invalid note id {note_id:?}"))
            }
            _ => io_control_error(error),
        })?;
        let record = {
            let registry = self.registry.lock().map_err(poisoned)?;
            match note_session_for(&registry, note_id) {
                Some(existing) => existing,
                None => {
                    drop(registry);
                    let cwd = note_home(meta.project.as_deref(), requested_cwd)?;
                    let mut record =
                        note_record(&id, &cwd, meta.display_title(), note_id.to_owned(), parent);
                    record.created_at = diri_proto::DateMillis(meta.created as f64 * 1000.0);
                    record.updated_at = diri_proto::DateMillis(meta.modified_ms as f64);
                    // A concurrent adoption of the same file wins; return it.
                    let registry = self.registry.lock().map_err(poisoned)?;
                    if let Some(existing) = note_session_for(&registry, note_id) {
                        existing
                    } else {
                        drop(registry);
                        self.insert_note_record(record.clone())?;
                        record
                    }
                }
            }
        };
        // Stamp after the record exists: a crash in between leaves an
        // unstamped file whose record the next startup scan recognises.
        if meta.session.as_deref() != Some(record.id.0.as_str()) {
            let session = record.id.0.clone();
            store
                .update(note_id, &diri_notes::history::Author::User, |note| {
                    note.front
                        .set(diri_notes::store::KEY_SESSION, Some(session));
                    Ok(())
                })
                .map_err(io_control_error)?;
        }
        Ok(record)
    }

    fn insert_note_record(&self, record: diri_proto::SessionRecord) -> Result<(), ControlError> {
        let mut registry = self.registry.lock().map_err(poisoned)?;
        self.ensure_published_project(&mut registry, &record.cwd, None);
        let id = record.id.0.clone();
        registry.insert_record(record);
        registry.persist_for_shutdown().map_err(io_control_error)?;
        self.publish_updated(&registry, &id);
        Ok(())
    }

    /// Gives every orphan note a Session so it shows in the sidebar: notes
    /// written before note Sessions existed, or by `dirijor note` while the
    /// Engine was down. An orphan is an unarchived file with no `session`
    /// stamp and no Session; a note whose Session was removed keeps its stamp
    /// and stays removed. Runs once per Engine start, oldest note first.
    pub fn adopt_orphan_notes(&self) -> Result<usize, ControlError> {
        let store = self.note_store()?;
        self.relink_notes(&store)?;
        let mut notes = store.list().map_err(io_control_error)?;
        notes.sort_by(|a, b| a.created.cmp(&b.created).then(a.id.cmp(&b.id)));
        let known: std::collections::HashSet<String> = self
            .registry
            .lock()
            .map_err(poisoned)?
            .records()
            .into_iter()
            .filter(diri_proto::SessionRecord::is_note)
            .filter_map(|record| record.note_id)
            .collect();
        let mut adopted = 0;
        for note in notes {
            // Stamped: shown, or removed on purpose. Unstamped but listed:
            // created before stamps, so adopt_note only stamps it.
            let listed = known.contains(&note.id);
            if note.session.is_some() || (note.archived && !listed) {
                continue;
            }
            match self.adopt_note(&store, &note.id, None, None, next_session_id()) {
                Ok(_) if !listed => adopted += 1,
                Ok(_) => {}
                Err(error) => diri_telemetry::event!(
                    "notes.adopt_failed",
                    code = diri_telemetry::id(&error.code),
                ),
            }
        }
        Ok(adopted)
    }

    /// Gives back a note Session's file link when its record lost it. An
    /// Engine that predates notes rewrites state.json without `noteId` (it
    /// does not know the field) and reaps the record as exited; the note then
    /// opens as "file is gone" although its file is intact. The file names
    /// its Session in front matter, so the link is recovered from there.
    /// Returns how many records were repaired.
    fn relink_notes(&self, store: &diri_notes::store::NoteStore) -> Result<usize, ControlError> {
        let notes = store.list().map_err(io_control_error)?;
        let mut repaired = 0;
        let mut registry = self.registry.lock().map_err(poisoned)?;
        for note in notes {
            let Some(session) = note.session.as_deref() else {
                continue;
            };
            let Some(mut record) = registry.record(session) else {
                continue;
            };
            if !record.is_note() || record.note_id.is_some() {
                continue;
            }
            record.note_id = Some(note.id.clone());
            if matches!(record.status, diri_proto::SessionStatus::Exited(_)) {
                record.status = diri_proto::SessionStatus::Idle;
            }
            registry.insert_record(record);
            self.publish_updated(&registry, session);
            repaired += 1;
        }
        if repaired > 0 {
            registry.persist_for_shutdown().map_err(io_control_error)?;
            diri_telemetry::event!("notes.relinked", count = repaired);
        }
        Ok(repaired)
    }

    /// Adopts orphan notes on a one-shot thread, off the accept path.
    pub fn spawn_note_adoption(self: &Arc<Self>) {
        let server = Arc::clone(self);
        let _ = std::thread::Builder::new()
            .name("dirijord-note-adoption".into())
            .spawn(move || {
                if let Ok(adopted) = server.adopt_orphan_notes()
                    && adopted > 0
                {
                    diri_telemetry::event!("notes.adopted", count = adopted);
                }
            });
    }

    /// The notes directory this Engine serves: pinned by the daemon, else the
    /// standard location.
    fn resolved_notes_dir(&self) -> Option<PathBuf> {
        self.notes_dir
            .clone()
            .or_else(diri_notes::store::NoteStore::resolve_dir)
    }

    fn note_store(&self) -> Result<diri_notes::store::NoteStore, ControlError> {
        let dir = match &self.notes_dir {
            Some(dir) => dir.clone(),
            None => diri_notes::store::NoteStore::resolve_dir()
                .ok_or_else(|| ControlError::internal("no home directory for notes"))?,
        };
        diri_notes::store::NoteStore::open(dir).map_err(io_control_error)
    }

    fn session_spawn_remote(
        &self,
        p: diri_proto::SessionSpawnParams,
        caller_argv: Vec<String>,
        mut account_profile: Option<diri_proto::AgentAccountProfile>,
        reserved_id: Option<String>,
        scheduled: Option<diri_proto::schedules::ScheduledRunInfo>,
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
        if p.same_repo_as.is_some() {
            return Err(ControlError::bad_request(
                "sameRepoAs requires the structured remote workspace RPC",
            ));
        }

        let helper = manager.ensure_helper(&host).map_err(io_control_error)?;
        let persistence = manager
            .probe_persistence(&host, &helper)
            .map_err(io_control_error)?;
        let mut requested_cwd = if p.cwd.trim().is_empty() {
            host.default_cwd.clone().unwrap_or_else(|| "~".into())
        } else {
            p.cwd.clone()
        };
        let worktree = if p.new_worktree.unwrap_or(false) {
            let info = crate::git::create_worktree_remote(
                &manager,
                &host,
                &requested_cwd,
                p.worktree_branch.as_deref(),
                p.worktree_base.as_deref(),
            )
            .map_err(io_control_error)?;
            requested_cwd.clone_from(&info.path);
            Some(info)
        } else {
            None
        };
        let kind = p.kind.id().to_string();
        let (mut descriptor, engine) = {
            let registry = self.registry.lock().map_err(poisoned)?;
            let engine = registry.engine();
            let manifest = engine.manifest(&kind).ok_or_else(|| {
                ControlError::not_found(format!("no manifest for agent {kind:?}"))
            })?;
            (manifest.agent.clone().unwrap_or_default(), engine)
        };
        drop(engine);
        let captured = if let Some(binary) = descriptor.binary.as_deref() {
            let (executable, captured) = self.discover_remote_agent_for_launch(
                manager.as_ref(),
                &host,
                &kind,
                binary,
                requested_cwd,
            )?;
            descriptor.binary = Some(executable);
            captured
        } else {
            manager
                .capture_environment(
                    &helper,
                    &diri_proto::remote_pty::EnvironmentCaptureRequest {
                        cwd: Some(requested_cwd),
                        timeout_millis: 10_000,
                    },
                )
                .map_err(io_control_error)?
        };
        let cwd = PathBuf::from(&captured.cwd);
        if !cwd.is_absolute() {
            return Err(ControlError::internal(
                "remote Helper returned a non-absolute cwd",
            ));
        }
        let authority = descriptor.authority();
        let inherited = captured
            .environment
            .into_iter()
            .map(|variable| (variable.name, variable.value))
            .collect::<Vec<_>>();

        let tracked = reserved_id.is_some();
        let id = reserved_id.unwrap_or_else(next_session_id);
        let mut agent_session_id = None;
        let mut launch_args = caller_argv.clone();
        if descriptor.binary.is_some() {
            launch_args.extend(descriptor.spawn_args.iter().cloned());
            let minted = descriptor
                .mints_conversation_id()
                .then(crate::inject::uuid_v4);
            let provider_dir = inherited
                .iter()
                .rev()
                .find(|(name, value)| name == "HOME" && !value.is_empty())
                .map(|(_, home)| Path::new(home).join(".diri/session-storage").join(&id));
            let plan = descriptor
                .conversation_plan(
                    &launch_args,
                    crate::agent::ConversationLaunch::Fresh {
                        new_id: minted.as_deref(),
                        session_dir: provider_dir.as_deref(),
                    },
                )
                .ok_or_else(|| {
                    ControlError::bad_request(format!(
                        "agent {kind:?} has an incomplete fresh conversation grammar"
                    ))
                })?;
            launch_args = plan.args;
            agent_session_id = plan.agent_session_id;
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
            crate::agent::assert_color_environment(&mut spec.env);
            spec
        };
        if let (Some(cols), Some(rows)) = (p.initial_cols, p.initial_rows) {
            pty.cols = cols.clamp(2, u16::MAX as i64) as u16;
            pty.rows = rows.clamp(2, u16::MAX as i64) as u16;
        }

        if let Some(profile) = &mut account_profile {
            crate::accounts::bind_pty(profile, &mut pty)?;
        }
        let token = random_session_token()?;
        if let Some(profile) = &account_profile {
            crate::accounts::prepare_remote_directory(profile, &host, &manager)?;
        }
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
        record.scheduled_run = scheduled;
        record.account_profile = account_profile;
        record.kind = p.kind.clone();
        record.originating_prompt = p.initial_prompt.clone();
        record.host = Some(host.id.clone());
        record.project_id = crate::registry::session_project_id(&captured.cwd, Some(&host.id));
        record.remote_persistence = Some(persistence);
        if let Some(info) = worktree {
            record.worktree_path = Some(info.path);
            record.git_branch = info.branch;
        }
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
        let spawn_started = Instant::now();
        self.spawn_session_with_intent(spec, Some(record), tracked)?;
        let mut registry = self.registry.lock().map_err(poisoned)?;
        if let Some(record) = registry.record(&id) {
            crate::telemetry::record_session_spawn(
                &record,
                "fresh",
                spawn_started.elapsed(),
                registry.other_live_agent(&id),
            );
        }
        self.ensure_published_project(&mut registry, &captured.cwd, Some(&host.id));
        if tracked {
            registry.persist_for_shutdown().map_err(io_control_error)?;
        } else {
            let _ = registry.persist();
        }
        self.publish_updated(&registry, &id);

        let prompt = p.initial_prompt.filter(|prompt| !prompt.is_empty());
        let appearance = p.appearance;
        let accept_claude_workspace = kind == diri_proto::AgentKind::CLAUDE_CODE_ID;
        let record = registry
            .record(&id)
            .ok_or_else(|| ControlError::internal("the new remote session vanished"))?;
        drop(registry);

        if let Some(prompt) = prompt {
            prepare_agent_input(
                &self.registry,
                &id,
                accept_claude_workspace,
                appearance,
                Some(&prompt),
            )
            .map_err(|error| initial_prompt_control_error(&id, error))?;
        } else if accept_claude_workspace {
            let registry = Arc::clone(&self.registry);
            let session_id = id.clone();
            std::thread::spawn(move || {
                let _ = prepare_agent_input(&registry, &session_id, true, appearance, None);
            });
        }
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
    fn worktree_scan_page(
        &self,
        p: diri_proto::WorktreeScanParams,
    ) -> Result<diri_proto::WorktreeScanResult, ControlError> {
        let registry = Arc::clone(&self.registry);
        self.worktree_scan.request(p, move |measure_disk, emit| {
            let (records, roots) = {
                let registry = registry.lock().map_err(|e| e.to_string())?;
                (registry.records(), registry.projects_raw().to_vec())
            };
            crate::worktree_health::scan(&roots, &records, measure_disk, emit)
        })
    }

    fn worktree_overview(&self) -> Result<JsonValue, ControlError> {
        // Older clients still receive a complete result, off the connection
        // loop, but join the same worker as incremental clients.
        let mut p = diri_proto::WorktreeScanParams {
            refresh: true,
            ..Default::default()
        };
        let mut entries = std::collections::BTreeMap::new();
        loop {
            let page = self.worktree_scan_page(p.clone())?;
            if p.generation != Some(page.generation) {
                entries.clear();
            }
            for entry in page.entries {
                entries.insert(entry.path.clone(), entry);
            }
            p.refresh = false;
            p.generation = Some(page.generation);
            p.cursor = page.cursor;
            if !page.running && !page.has_more {
                if let Some(error) = page.error {
                    return Err(ControlError::internal(error));
                }
                return encode(&diri_proto::WorktreeOverviewResult {
                    entries: entries.into_values().collect(),
                });
            }
            if !page.has_more {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }

    fn worktree_cleanup(&self, params: Option<JsonValue>) -> Result<JsonValue, ControlError> {
        let p: diri_proto::WorktreeCleanupParams = decode(params)?;
        // Serialize the final local session check and removal against launches.
        // Network and disk scans never run while holding the registry lock.
        let inspection = crate::worktree_health::inspect_cleanup(&p).map_err(io_control_error)?;
        let registry = self.registry.lock().map_err(poisoned)?;
        crate::worktree_health::cleanup(&p, &inspection, &registry.records())
            .map_err(io_control_error)?;
        drop(registry);
        self.events.publish(
            "worktree.removed",
            json!({"repoPath": p.repo_path, "path": p.worktree_path}),
            None,
        );
        Ok(json!({}))
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
                .record(&id)
                .ok_or_else(|| ControlError::not_found(id.clone()))?
        };
        if record.account_profile.is_some() {
            return Err(ControlError::bad_request(
                "Moving an account-bound session requires an explicit destination account; start a new session on the destination host",
            ));
        }
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
        let target_name = target_host
            .as_ref()
            .map(|host| host.display_name())
            .unwrap_or("local");
        let prepared = crate::migrate::prepare(
            &record.cwd,
            source_host.as_ref(),
            target_host.as_ref(),
            &target_repo,
            target_name,
        )
        .map_err(migrate_control_error)?;

        // Stop the source agent. Phase 1 ran while it was still writing, so
        // the target only becomes the truth once everything it changed since
        // has been carried across too. Until then the source holds all the
        // work: a failure leaves the stopped session where it was, resumable.
        let mut warnings: Vec<String> = Vec::new();
        self.terminate_session_unlocked(&id, Duration::from_secs(3))?;
        if let Err(error) = crate::migrate::reconcile(
            &prepared,
            source_host.as_ref(),
            target_host.as_ref(),
            target_name,
        ) {
            // The same bookkeeping as `session.kill`: that is all that has
            // happened to this session.
            let mut registry = self.registry.lock().map_err(poisoned)?;
            let _ = registry.persist();
            if let Some(store) = &self.remote_bindings {
                let _ = store.remove(&id);
            }
            self.publish_updated(&registry, &id);
            return Err(ControlError::new(
                "migrate_reconcile_failed",
                format!(
                    "session {id} was stopped but not moved: changes made while it was moving could not be carried to the target ({error}). All work is still in {}; resume the session there.",
                    prepared.source_repo_root
                ),
            ));
        }
        // Point of no return.
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
            let worktree = prepared.target_is_worktree.then(|| cwd.clone());
            let transcript = shuttle.local_target_path.clone();
            let local = target_host.is_none();
            self.ensure_published_project(&mut registry, &cwd, target_id.as_deref());
            registry.update_record(&id, |record| {
                record.host = target_id;
                record.cwd = cwd;
                record.project_id =
                    crate::registry::session_project_id(&record.cwd, record.host.as_deref());
                record.worktree_path = worktree;
                record.git_branch = Some(branch);
                record.transcript_path = if local { transcript } else { None };
                record.status = diri_proto::SessionStatus::Exited(diri_proto::ExitInfo {
                    reason: diri_proto::ExitReason::Exited,
                    code: Some(0),
                    signal: None,
                    system_restart: false,
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
        diri_telemetry::event!(
            "session.migrate",
            session = diri_telemetry::id(&id),
            agent = diri_telemetry::id(session.kind.id()),
            from_host = record.host.as_deref().map(diri_telemetry::id),
            to_host = session.host.as_deref().map(diri_telemetry::id),
            transcript_migrated = shuttle.migrated,
            warnings = warnings.len(),
        );
        encode(&diri_proto::SessionMigrateResult {
            session,
            transcript_migrated: shuttle.migrated,
            warning: (!warnings.is_empty()).then(|| warnings.join("; ")),
        })
    }

    /// Moves an ended resumable session record to a different checkout of the
    /// same repository. The app proposes this from cached overview data, but
    /// confirmation always lands here for authoritative revalidation.
    fn session_reparent_worktree(
        &self,
        params: Option<JsonValue>,
    ) -> Result<JsonValue, ControlError> {
        let p: diri_proto::SessionReparentWorktreeParams = decode(params)?;
        let project_root = std::fs::canonicalize(&p.project_root).map_err(|error| {
            ControlError::bad_request(format!("project root is unavailable: {error}"))
        })?;
        let worktree_path = std::fs::canonicalize(&p.worktree_path).map_err(|error| {
            ControlError::bad_request(format!("worktree is unavailable: {error}"))
        })?;
        let worktrees = crate::git::list_worktrees(&project_root).map_err(|error| {
            ControlError::bad_request(format!("could not inspect project worktrees: {error}"))
        })?;
        let target = worktrees
            .iter()
            .find(|entry| Path::new(&entry.path) == worktree_path)
            .ok_or_else(|| {
                ControlError::bad_request("the target is not a worktree of this project")
            })?;
        let target_path = worktree_path.to_string_lossy().into_owned();

        let mut registry = self.registry.lock().map_err(poisoned)?;
        let record = registry
            .record(&p.session_id.0)
            .ok_or_else(|| ControlError::not_found(p.session_id.0.clone()))?;
        if record.host.is_some() {
            return Err(ControlError::bad_request(
                "remote sessions cannot move into a local worktree",
            ));
        }
        if !matches!(record.status, diri_proto::SessionStatus::Exited(_)) {
            return Err(ControlError::bad_request(
                "stop or archive the session before moving it to another worktree",
            ));
        }
        if record.resumability != diri_proto::Resumability::Resumable {
            return Err(ControlError::bad_request(
                "this session cannot resume after moving to another worktree",
            ));
        }
        // Project identity is derived from the exact root Diri persisted. The
        // canonical path above is only for authoritative git membership: a
        // symlinked project root must not suddenly hash to a different id.
        let expected_project = crate::registry::session_project_id(&p.project_root, None);
        if record.project_id != expected_project {
            return Err(ControlError::bad_request(
                "the target belongs to a different project",
            ));
        }
        if local_session_uses_worktree(&record, &worktree_path) {
            return Err(ControlError::bad_request(
                "the session is already attached to this worktree",
            ));
        }
        let occupied = registry.records().into_iter().any(|candidate| {
            candidate.id != record.id
                && !matches!(candidate.status, diri_proto::SessionStatus::Exited(_))
                && local_session_uses_worktree(&candidate, &worktree_path)
        });
        if occupied {
            return Err(ControlError::bad_request(
                "another live session already owns this worktree",
            ));
        }
        let updated = registry
            .reparent_worktree(&p.session_id.0, target_path, target.branch.clone())
            .map_err(|error| ControlError::internal(error.to_string()))?;
        self.publish_updated(&registry, &p.session_id.0);
        encode(&updated)
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
        self.agent_catalog
            .lock()
            .map_err(poisoned)?
            .invalidate(Some(&host.id));
        encode(&diri_proto::HostInitializeResult {
            helper_build_id: helper.build_id,
            protocol: helper.protocol,
            persistence,
            cwd: captured.cwd,
            shell: captured.shell,
        })
    }

    fn host_usage(&self, params: Option<JsonValue>) -> Result<JsonValue, ControlError> {
        let p: diri_proto::HostUsageParams = decode(params)?;
        let manager = self
            .remote
            .as_ref()
            .ok_or_else(crate::remote::transport_unavailable)?;
        let host = self.resolve_host(&p.host)?;
        let profiles = self
            .accounts
            .lock()
            .map_err(poisoned)?
            .catalog()?
            .profiles
            .into_iter()
            .filter(|profile| profile.host.as_deref() == Some(host.id.as_str()))
            .filter_map(|profile| {
                Some(diri_proto::remote_pty::TranscriptUsageDirectory {
                    provider: match profile.agent.as_str() {
                        "claude-code" => "claude",
                        "codex" => "codex",
                        _ => return None,
                    }
                    .into(),
                    config_home: profile.config_home,
                })
            })
            .collect();
        let request = diri_proto::remote_pty::TranscriptUsageRequest { profiles };
        let result = manager
            .transcript_usage(&host, &request)
            .map_err(io_control_error)?;
        encode(&result)
    }

    /// `host.list_directories`: one shallow, bounded filesystem read on the
    /// requested execution machine. Remote work stays behind the Engine and
    /// uses the verified Helper over `ssh -T`; the app never executes SSH.
    fn host_list_directories(&self, params: Option<JsonValue>) -> Result<JsonValue, ControlError> {
        let p: diri_proto::HostListDirectoriesParams = decode(params)?;
        let request = diri_proto::remote_pty::DirectoryListRequest {
            path: p.path,
            mode: p.mode,
        };
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
                    .record(&session_id.0)
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

    fn session_deliver_message(
        &self,
        params: Option<JsonValue>,
    ) -> Result<JsonValue, ControlError> {
        let p: diri_proto::DeliverMessageParams = decode(params)?;
        // Reuse the existing input serialization. No new terminal owner, queue,
        // or lock is introduced. Receipt writes precede all PTY effects.
        let mut registry = self.registry.lock().map_err(poisoned)?;
        if registry.get(&p.session_id.0).is_none() {
            return Err(ControlError::not_found(p.session_id.0.clone()));
        }
        let receipt = message_delivery::deliver(
            &self
                .socket_path
                .with_file_name("message-receipts-v1.sqlite"),
            &p,
            || {
                registry
                    .wake_session(&p.session_id.0)
                    .map_err(io_control_error)?;
                self.publish_updated(&registry, &p.session_id.0);
                registry
                    .get(&p.session_id.0)
                    .ok_or_else(|| ControlError::not_found(p.session_id.0.clone()))?
                    .send_text(&p.text, p.submit)
                    .map_err(io_control_error)
            },
        );
        if let Ok(receipt) = &receipt {
            diri_telemetry::event!(
                "message.deliver",
                session = diri_telemetry::id(&p.session_id.0),
                delivery = match receipt.get("delivery").and_then(Value::as_str) {
                    Some("sent") => "sent",
                    _ => "unknown",
                },
                duplicate = receipt
                    .get("duplicate")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                submit = p.submit,
                chars = p.text.chars().count(),
            );
        }
        receipt
    }

    fn session_send_key(&self, params: Option<JsonValue>) -> Result<JsonValue, ControlError> {
        use diri_proto::terminal_input::{KeyEncodingError, encode_action};
        let p: diri_proto::SendKeyParams = decode(params)?;
        let event = p.event().map_err(ControlError::bad_request)?;
        let mut registry = self.registry.lock().map_err(poisoned)?;
        let session = registry
            .get(&p.session_id.0)
            .ok_or_else(|| ControlError::not_found(p.session_id.0.clone()))?;
        if !session.accepts_keyboard_input(true) {
            return Err(ControlError::new(
                "input_modes_unavailable",
                "enhanced keyboard state is unavailable; input was not sent",
            ));
        }
        let bytes = encode_action(&event, p.modifiers, session.keyboard_state(), p.action)
            .map_err(|error| {
                ControlError::new(
                    match error {
                        KeyEncodingError::UnknownModes => "input_modes_unavailable",
                        _ => "unsupported_key_action",
                    },
                    error.to_string(),
                )
            })?;
        registry
            .wake_session(&p.session_id.0)
            .map_err(io_control_error)?;
        let session = registry
            .get(&p.session_id.0)
            .ok_or_else(|| ControlError::not_found(p.session_id.0.clone()))?;
        session.write_input(&bytes).map_err(io_control_error)?;
        self.publish_updated(&registry, &p.session_id.0);
        encode(&diri_proto::SendKeyResult {
            bytes_accepted: bytes.len(),
        })
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
        let reflow = {
            let registry = self.registry.lock().map_err(poisoned)?;
            let session = registry
                .get(&p.session_id.0)
                .ok_or_else(|| ControlError::not_found(p.session_id.0.clone()))?;
            session
                .resize_pty(cols, rows)
                .map_err(|error| ControlError::internal(error.to_string()))?
        };
        // The reflow runs after the Registry lock is released; see
        // `Session::resize_pty`.
        if let Some(reflow) = reflow {
            reflow.apply();
        }
        Ok(json!({}))
    }

    fn session_read_screen(&self, params: Option<JsonValue>) -> Result<JsonValue, ControlError> {
        let p: diri_proto::SessionIdParams = decode(params)?;
        {
            let registry = self.registry.lock().map_err(poisoned)?;
            if let Some(session) = registry.get(&p.session_id.0) {
                let (cols, rows) = session.screen_size();
                return encode(&diri_proto::ReadScreenResult {
                    text: session.screen_lines().join("\n"),
                    cols: cols as i64,
                    rows: rows as i64,
                });
            }
        }
        let screen = self
            .completed_terminal_screen(&p.session_id.0)?
            .ok_or_else(|| ControlError::not_found(p.session_id.0.clone()))?;
        let (cols, rows) = screen.size();
        encode(&diri_proto::ReadScreenResult {
            text: screen.lines().join("\n"),
            cols: cols as i64,
            rows: rows as i64,
        })
    }

    /// The retained terminal of a completed local session that no live
    /// Session backs. Captured under the Registry, loaded outside it, and
    /// revalidated afterwards so a resume or removal that raced the read
    /// cannot hand back another run's screen. Input is never possible here.
    fn completed_terminal_screen(
        &self,
        session_id: &str,
    ) -> Result<Option<diri_terminal_state::HeadlessScreen>, ControlError> {
        Ok(self
            .completed_terminal(session_id)?
            .map(|(screen, _)| screen))
    }

    /// The retained screen and the stable owner identity of its exact run.
    fn completed_terminal(
        &self,
        session_id: &str,
    ) -> Result<Option<(diri_terminal_state::HeadlessScreen, String)>, ControlError> {
        let handle = {
            let registry = self.registry.lock().map_err(poisoned)?;
            registry.completed_run(session_id)
        };
        let Some(handle) = handle else {
            return Ok(None);
        };
        let terminal = handle.load().map_err(|error| {
            ControlError::new("completed_terminal_unavailable", error.to_string())
        })?;
        let Some(terminal) = terminal else {
            return Ok(None);
        };
        let unchanged = {
            let registry = self.registry.lock().map_err(poisoned)?;
            registry.completed_run(session_id).is_some_and(|current| {
                current.record().status == handle.record().status
                    && current.record().created_at == handle.record().created_at
            })
        };
        if !unchanged {
            return Err(ControlError::new(
                "completed_terminal_stale",
                "the session changed while its retained terminal was being read",
            ));
        }
        Ok(terminal
            .screen()
            .map(|screen| (screen, handle.key().owner_id())))
    }

    fn session_terminal_title(&self, params: Option<JsonValue>) -> Result<JsonValue, ControlError> {
        let p: diri_proto::SessionTerminalTitleParams = decode(params)?;
        let registry = self.registry.lock().map_err(poisoned)?;
        let record = registry
            .record(&p.session_id.0)
            .ok_or_else(|| ControlError::not_found(p.session_id.0.clone()))?;
        if record.host.is_some() {
            return Err(ControlError::new(
                "terminal_title_unsupported",
                "Remote Helper snapshots do not provide current terminal titles",
            ));
        }
        let session = registry.get(&p.session_id.0).ok_or_else(|| {
            ControlError::new(
                "terminal_title_unavailable",
                "Session has no Engine-owned terminal state",
            )
        })?;
        let title = session.terminal_title().map_err(|error| {
            let code = if error.kind() == std::io::ErrorKind::Unsupported {
                "terminal_title_unsupported"
            } else {
                "terminal_title_unavailable"
            };
            ControlError::new(code, error.to_string())
        })?;
        encode(&diri_proto::SessionTerminalTitleResult {
            session_id: p.session_id,
            title,
        })
    }

    fn session_reset_terminal(&self, params: Option<JsonValue>) -> Result<JsonValue, ControlError> {
        let p: diri_proto::SessionIdParams = decode(params)?;
        let registry = self.registry.lock().map_err(poisoned)?;
        if registry.get(&p.session_id.0).is_none() {
            return if registry.record(&p.session_id.0).is_some() {
                Err(ControlError::new(
                    "terminal_reset_unavailable",
                    "the session has no live terminal to reset",
                ))
            } else {
                Err(ControlError::not_found(p.session_id.0.clone()))
            };
        }
        let session = registry
            .get(&p.session_id.0)
            .ok_or_else(|| ControlError::not_found(p.session_id.0.clone()))?;
        session.reset_terminal().map_err(|error| {
            let code = match error.kind() {
                std::io::ErrorKind::Unsupported => "terminal_reset_unsupported",
                std::io::ErrorKind::NotConnected => "terminal_reset_unavailable",
                _ => "terminal_reset_failed",
            };
            ControlError::new(code, error.to_string())
        })?;
        Ok(json!({ "sessionID": p.session_id.0 }))
    }

    fn session_capture_find(&self, params: Option<JsonValue>) -> Result<JsonValue, ControlError> {
        let p: diri_proto::SessionIdParams = decode(params)?;
        let reader = {
            let registry = self.registry.lock().map_err(poisoned)?;
            registry
                .get(&p.session_id.0)
                .map(crate::session::Session::scrollback_reader)
        };
        if let Some(reader) = reader {
            return encode(&reader.capture_find()?);
        }
        let (screen, owner) = self
            .completed_terminal(&p.session_id.0)?
            .ok_or_else(|| ControlError::not_found(p.session_id.0.clone()))?;
        let (cols, visible_rows) = screen.size();
        if cols.saturating_mul(visible_rows) > diri_proto::FIND_CAPTURE_MAX_CELLS {
            return Err(ControlError::new(
                "find_capture_too_large",
                "This terminal is too large for a retained search view",
            ));
        }
        let cells = screen
            .find_capture_cells()
            .map_err(|error| ControlError::new("find_capture_too_large", error))?;
        encode(&diri_proto::CaptureFindResult {
            owner,
            // Immutable: a retained terminal never changes under a capture.
            capture_revision: 0,
            session_id: p.session_id,
            is_alt_screen: screen.is_alt_screen(),
            visible_rows,
            partial: cells.first_row > 0,
            cells,
        })
    }

    fn session_read_scrollback(
        &self,
        params: Option<JsonValue>,
    ) -> Result<JsonValue, ControlError> {
        let p: diri_proto::SessionIdParams = decode(params)?;
        {
            let registry = self.registry.lock().map_err(poisoned)?;
            if let Some(session) = registry.get(&p.session_id.0) {
                return encode(&session.read_scrollback());
            }
        }
        let mut screen = self
            .completed_terminal_screen(&p.session_id.0)?
            .ok_or_else(|| ControlError::not_found(p.session_id.0.clone()))?;
        encode(&screen.scrollback())
    }

    fn session_read_scrollback_cells(
        &self,
        params: Option<JsonValue>,
    ) -> Result<JsonValue, ControlError> {
        let p: diri_proto::ReadScrollbackCellsParams = decode(params)?;
        let reader = {
            let registry = self.registry.lock().map_err(poisoned)?;
            registry
                .get(&p.session_id.0)
                .map(crate::session::Session::scrollback_reader)
        };
        // A remote history page takes a network round trip (up to the request
        // timeout). Attach input and grid publication also need the Registry;
        // neither may wait for this reply.
        if let Some(reader) = reader {
            return encode(&reader.read(p.first_row, p.max_rows));
        }
        let (mut screen, _) = self
            .completed_terminal(&p.session_id.0)?
            .ok_or_else(|| ControlError::not_found(p.session_id.0.clone()))?;
        encode(&screen.scrollback_cells(p.first_row, p.max_rows))
    }

    fn spawn_session_unlocked(
        &self,
        spec: crate::session::SessionSpec,
        record: Option<diri_proto::SessionRecord>,
    ) -> Result<(), ControlError> {
        self.spawn_session_with_intent(spec, record, false)
    }

    fn spawn_session_with_intent(
        &self,
        spec: crate::session::SessionSpec,
        record: Option<diri_proto::SessionRecord>,
        persist_intent: bool,
    ) -> Result<(), ControlError> {
        let id = spec.id.clone();
        let engine = {
            let mut registry = self.registry.lock().map_err(poisoned)?;
            registry
                .reserve_launch(&id, record.is_some())
                .map_err(io_control_error)?;
            if persist_intent && let Some(record) = &record {
                registry.insert_record(record.clone());
                if let Err(error) = registry.persist_for_shutdown() {
                    registry.release_launch(&id);
                    return Err(io_control_error(error));
                }
            }
            registry.engine()
        };
        let spawned = crate::session::Session::spawn(spec, engine);
        let mut registry = self.registry.lock().map_err(poisoned)?;
        registry.release_launch(&id);
        let mut session = spawned.map_err(io_control_error)?;
        let installed = registry.install_session(session, record);
        drop(registry);
        if let Err(rejected) = installed {
            session = *rejected;
            let _ = session.terminate(Duration::ZERO);
            return Err(ControlError::bad_request(
                "Session changed while its remote launch was pending",
            ));
        }
        Ok(())
    }

    fn terminate_session_unlocked(
        &self,
        id: &str,
        grace: Duration,
    ) -> Result<Option<crate::pty::Exit>, ControlError> {
        let stop = {
            let registry = self.registry.lock().map_err(poisoned)?;
            registry
                .get(id)
                .and_then(crate::session::Session::remote_stop)
        };
        if let Some(stop) = stop {
            let exit = stop.stop(grace).map_err(io_control_error)?;
            let session = self
                .registry
                .lock()
                .map_err(poisoned)?
                .finish_remote_stop(id, &stop, exit);
            drop(session);
            Ok(Some(exit))
        } else {
            self.registry
                .lock()
                .map_err(poisoned)?
                .terminate(id, grace)
                .map_err(io_control_error)
        }
    }

    fn stop_remote_before_lifecycle(&self, id: &str) -> Result<(), ControlError> {
        let remote = {
            let registry = self.registry.lock().map_err(poisoned)?;
            registry.preflight_lifecycle(id).map_err(io_control_error)?;
            registry
                .get(id)
                .is_some_and(|session| session.remote_stop().is_some())
        };
        if remote {
            self.terminate_session_unlocked(id, Duration::from_millis(500))?;
        }
        Ok(())
    }

    fn session_kill(&self, params: Option<JsonValue>) -> Result<JsonValue, ControlError> {
        let p: diri_proto::SessionIdParams = decode(params)?;
        let exit = self.terminate_session_unlocked(&p.session_id.0, Duration::from_secs(3))?;
        let mut registry = self.registry.lock().map_err(poisoned)?;
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
        self.stop_remote_before_lifecycle(&p.session_id.0)?;
        let mut registry = self.registry.lock().map_err(poisoned)?;
        let removed = registry
            .record(&p.session_id.0)
            .ok_or_else(|| ControlError::not_found(p.session_id.0.clone()))?;
        registry
            .remove(&p.session_id.0, &self.logs_dir)
            .map_err(io_control_error)?;
        if let Some(store) = &self.remote_bindings {
            let _ = store.remove(&p.session_id.0);
        }
        // Closing a note's tab never touches its file: the note stays in
        // Search notes (as a closed note) and opening it there brings its tab
        // back, the way closed chats stay in conversation search.
        self.events.record_removed(&removed);
        self.events.publish(
            diri_proto::EventName::SESSION_REMOVED,
            json!({ "id": p.session_id.0, "reason": "released" }),
            Some(&p.session_id.0),
        );
        Ok(json!({}))
    }

    fn activity_list(&self, params: Option<JsonValue>) -> Result<JsonValue, ControlError> {
        let p: diri_proto::ActivityListParams = decode(params)?;
        let limit = usize::from(p.limit.unwrap_or(100).clamp(1, 300));
        encode(&diri_proto::ActivityListResult {
            entries: self.events.recent_activity(limit),
        })
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
        // Last-seen is a view hint, not an acknowledged edit: the flusher
        // writes it within the debounce window, off this request's thread.
        registry.persist_deferred();
        self.publish_updated(&registry, &p.session_id.0);
        self.pr_monitor_wake.wake_session(p.session_id.0);
        Ok(json!({}))
    }

    fn session_mark_unread(&self, params: Option<JsonValue>) -> Result<JsonValue, ControlError> {
        let p: diri_proto::SessionIdParams = decode(params)?;
        let mut registry = self.registry.lock().map_err(poisoned)?;
        if registry
            .mark_unread(&p.session_id.0)
            .map_err(io_control_error)?
        {
            // An explicit edit, but as small as mark-seen: same deferred write.
            registry.persist_deferred();
            self.publish_updated(&registry, &p.session_id.0);
        }
        Ok(json!({}))
    }

    fn client_set_active(&self, params: Option<JsonValue>) -> Result<JsonValue, ControlError> {
        let p: diri_proto::ClientActiveParams = decode(params)?;
        self.pr_monitor_wake.set_foreground_active(p.active);
        Ok(json!({}))
    }

    fn session_archive(&self, params: Option<JsonValue>) -> Result<JsonValue, ControlError> {
        let p: diri_proto::SessionIdParams = decode(params)?;
        let original = self
            .registry
            .lock()
            .map_err(poisoned)?
            .record(&p.session_id.0)
            .ok_or_else(|| ControlError::not_found(p.session_id.0.clone()))?;
        self.stop_remote_before_lifecycle(&p.session_id.0)?;
        let mut registry = self.registry.lock().map_err(poisoned)?;
        registry
            .archive_record(original)
            .map_err(io_control_error)?;
        self.publish_updated(&registry, &p.session_id.0);
        Ok(json!({}))
    }

    fn session_unarchive(&self, params: Option<JsonValue>) -> Result<JsonValue, ControlError> {
        let p: diri_proto::SessionIdParams = decode(params)?;
        let mut registry = self.registry.lock().map_err(poisoned)?;
        registry
            .unarchive(&p.session_id.0)
            .map_err(io_control_error)?;
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
        diri_telemetry::debug_event!(
            "hook.report",
            session = diri_telemetry::id(&session_id.0),
            kind = diri_telemetry::id(&p.kind),
            event = p.event.as_deref().map(diri_telemetry::id),
            parsed = parsed.is_some(),
        );
        let Some((signal, meta)) = parsed else {
            return Ok(json!({}));
        };
        // Never wait on the Registry here: the Agent is blocked on this reply.
        self.hook_reports
            .submit(
                &self.registry,
                &self.events,
                hook_queue::HookReport {
                    session_id: session_id.0,
                    signal,
                    meta,
                },
            )
            .map_err(poisoned)?;
        Ok(json!({}))
    }

    fn session_process_info(&self, params: Option<JsonValue>) -> Result<JsonValue, ControlError> {
        let p: diri_proto::SessionIdParams = decode(params)?;
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        let lock_registry = || {
            self.registry.try_lock().map_err(|error| match error {
                std::sync::TryLockError::WouldBlock => {
                    ControlError::new("process_facts_busy", "Session registry is busy")
                }
                std::sync::TryLockError::Poisoned(error) => poisoned(error),
            })
        };
        let (reader, host) = {
            let registry = lock_registry()?;
            let record = registry
                .record(&p.session_id.0)
                .ok_or_else(|| ControlError::not_found(p.session_id.0.clone()))?;
            let session = registry.get(&p.session_id.0).ok_or_else(|| {
                ControlError::new("process_unavailable", "Session has no live owner")
            })?;
            (session.process_facts_reader(), record.host)
        };
        let process = reader.read(deadline).map_err(|error| {
            let code = match error.kind() {
                std::io::ErrorKind::Unsupported => "process_facts_unsupported",
                std::io::ErrorKind::TimedOut => "process_facts_timeout",
                std::io::ErrorKind::WouldBlock => "process_facts_busy",
                _ => "process_facts_unavailable",
            };
            ControlError::new(code, error.to_string())
        })?;
        let registry = lock_registry()?;
        if !registry
            .get(&p.session_id.0)
            .is_some_and(|session| reader.matches(session))
            || registry
                .record(&p.session_id.0)
                .is_none_or(|record| record.host != host)
        {
            return Err(ControlError::new(
                "stale_session",
                "Session changed during process inspection",
            ));
        }
        diri_pty::unix_socket::remaining(deadline).map_err(|_| {
            ControlError::new(
                "process_facts_timeout",
                "Process inspection deadline expired",
            )
        })?;
        encode(&diri_proto::process_facts::SessionProcessInfo {
            session_id: p.session_id,
            host,
            observed_at: diri_proto::DateMillis::from(std::time::SystemTime::now()),
            process,
        })
    }

    /// Revives an exited session's conversation under the SAME record id.
    fn session_reconnect(&self, params: Option<JsonValue>) -> Result<JsonValue, ControlError> {
        let p: diri_proto::SessionReconnectParams = decode(params)?;
        let owner = {
            let registry = self.registry.lock().map_err(poisoned)?;
            let record = registry
                .record(&p.session_id.0)
                .ok_or_else(|| ControlError::not_found(p.session_id.0.clone()))?;
            if record.host.is_none() {
                return Err(ControlError::bad_request(
                    "Reconnect requires a remote session",
                ));
            }
            if !matches!(record.status, diri_proto::SessionStatus::Exited(_))
                && registry.get(&p.session_id.0).is_none()
            {
                return Err(ControlError::new(
                    "remote_owner_unavailable",
                    "The remote session has no live Engine binding to reconnect",
                ));
            }
            if !record.remote_connection.is_some_and(|connection| {
                connection.state == diri_proto::RemoteConnectionState::Failed
            }) {
                return encode(&diri_proto::SessionReconnectResult {
                    session: record,
                    started: false,
                    uncertain_input_discarded: false,
                });
            }
            registry
                .get(&p.session_id.0)
                .and_then(|session| session.remote_reconnect_handle())
                .ok_or_else(|| {
                    ControlError::new(
                        "remote_owner_unavailable",
                        "The remote session has no live Engine binding to reconnect",
                    )
                })?
        };
        // Inspect can wait on SSH. The lifecycle reservation pins this identity,
        // while Registry remains available to unrelated sessions and UI reads.
        let inspection = owner
            .inspect()
            .map_err(|error| ControlError::new("remote_reconnect_failed", error.to_string()))?;
        let mut registry = self.registry.lock().map_err(poisoned)?;
        let (started, uncertain_input_discarded) = registry
            .reconnect_remote(&p.session_id.0, &owner, inspection.process_state)
            .map_err(io_control_error)?;
        self.publish_updated(&registry, &p.session_id.0);
        let session = registry
            .record(&p.session_id.0)
            .ok_or_else(|| ControlError::not_found(p.session_id.0.clone()))?;
        encode(&diri_proto::SessionReconnectResult {
            session,
            started,
            uncertain_input_discarded,
        })
    }

    fn session_resume(&self, params: Option<JsonValue>) -> Result<JsonValue, ControlError> {
        let p: diri_proto::SessionIdParams = decode(params)?;
        let record = {
            let mut registry = self.registry.lock().map_err(poisoned)?;
            let record = registry
                .record(&p.session_id.0)
                .ok_or_else(|| ControlError::not_found(p.session_id.0.clone()))?;
            if record.remote_connection.is_some_and(|connection| {
                connection.state == diri_proto::RemoteConnectionState::Failed
            }) {
                return Err(ControlError::new(
                    "remote_transport_failed",
                    "Remote transport failed; the Agent's last state is preserved.",
                ));
            }
            // Presence in the registry is not liveness: only an explicit kill
            // removes a session, so an agent that died on its own is still in
            // the map. Returning here on presence alone would hand back the
            // corpse this call was asked to revive; the exited case falls
            // through to the eviction path below.
            if registry.get(&p.session_id.0).is_some()
                && !matches!(record.status, diri_proto::SessionStatus::Exited(_))
            {
                // Genuinely live: resuming is a no-op, not an error.
                crate::telemetry::record_resume(&record, "already_live", None);
                return self.restored_resume_result(&mut registry, &p.session_id.0);
            }
            record
        };
        // A local terminal has no conversation to re-enter: it restarts as a
        // fresh login shell, back in the directory it had `cd`'d to.
        let restored_directory = restored_terminal_directory(&record);
        let mut spec = if record.host.is_some() {
            crate::telemetry::record_resume(&record, "remote", record.agent_session_id.as_deref());
            self.remote_resume_spec(&record)?
        } else if record.kind == diri_proto::AgentKind::SHELL {
            crate::telemetry::record_resume(&record, "shell", None);
            self.shell_restart_spec(&record, restored_directory.as_deref())?
        } else {
            let registry = self.registry.lock().map_err(poisoned)?;
            match claude_resume_target(&record) {
                // Claude has no transcript for this tab: `--resume` would only
                // print "No conversation found" and leave a bare shell, so
                // start the tab's own id afresh instead.
                Some(None) => {
                    crate::telemetry::record_resume(
                        &record,
                        "fresh_unwritten",
                        record.agent_session_id.as_deref(),
                    );
                    self.fresh_spec(
                        &registry,
                        &record.id.0,
                        record.kind.id(),
                        &record.cwd,
                        record.agent_session_id.as_deref(),
                    )?
                }
                target => {
                    let conversation = target
                        .clone()
                        .flatten()
                        .or_else(|| record.agent_session_id.clone());
                    crate::telemetry::record_resume(
                        &record,
                        match (&target, &conversation) {
                            (Some(Some(_)), _) => "resume_verified",
                            (_, Some(_)) => "resume",
                            (_, None) => "no_conversation",
                        },
                        conversation.as_deref(),
                    );
                    self.resume_spec(
                        &registry,
                        &record.id.0,
                        record.kind.id(),
                        &record.cwd,
                        conversation.as_deref(),
                    )?
                }
            }
        };
        if record.host.is_none()
            && let Some(mut profile) = record.account_profile.clone()
        {
            if profile.host.is_some() || profile.agent != record.kind.id() {
                return Err(ControlError::bad_request(
                    "Session account does not match its Agent or host",
                ));
            }
            crate::accounts::bind_pty(&mut profile, &mut spec.pty)?;
        }
        let remote_persistence = spec.remote.as_ref().map(|remote| remote.launch.persistence);
        if remote_persistence.is_some() {
            self.terminate_session_unlocked(&p.session_id.0, Duration::from_millis(500))?;
            self.spawn_session_unlocked(spec, None)?;
            let mut registry = self.registry.lock().map_err(poisoned)?;
            registry.update_record(&p.session_id.0, |record| {
                record.remote_persistence = remote_persistence
            });
            return self.restored_resume_result(&mut registry, &p.session_id.0);
        }
        let mut registry = self.registry.lock().map_err(poisoned)?;
        let record = registry
            .record(&p.session_id.0)
            .ok_or_else(|| ControlError::not_found(p.session_id.0.clone()))?;
        let exited = matches!(record.status, diri_proto::SessionStatus::Exited(_));
        // The previous run's size is the pane's size: the App kept sizing that
        // PTY. Waiting for the App to propose it again cost the whole launch
        // fallback (an unchanged pane sends no resize) and started the agent
        // at the 80x24 default, to be reflowed once the App caught up.
        if let Some((cols, rows)) = registry
            .get(&p.session_id.0)
            .map(crate::session::Session::screen_size)
            .or_else(|| previous_screen_size(&spec))
            .and_then(|(cols, rows)| Some((u16::try_from(cols).ok()?, u16::try_from(rows).ok()?)))
            .filter(|&(cols, rows)| cols >= 2 && rows >= 2)
        {
            spec.pty.cols = cols;
            spec.pty.rows = rows;
            spec.defer_launch = false;
        }
        if registry.get(&p.session_id.0).is_some() {
            if !exited {
                // Already live: resuming is a no-op, not an error.
                return self.restored_resume_result(&mut registry, &p.session_id.0);
            }
            // An agent that died on its own leaves its session behind: only an
            // explicit kill takes one out of the registry, so presence alone
            // does not mean alive. Evicting the corpse — which also releases
            // the holder still owning this id — is what keeps resume from
            // silently handing back the dead record it was asked to revive.
            let _ = registry.terminate(&p.session_id.0, std::time::Duration::from_millis(500));
        }
        let local_shell = record.kind == diri_proto::AgentKind::SHELL && record.host.is_none();
        registry
            .respawn(spec)
            .map_err(|error| ControlError::internal(error.to_string()))?;
        if local_shell {
            // Report where the new shell actually starts before its first
            // sample: the restored directory, or none when that directory is
            // gone and the shell fell back to `cwd`.
            registry.update_record(&p.session_id.0, |record| {
                record.terminal_cwd = restored_directory
                    .as_deref()
                    .map(|path| path.to_string_lossy().into_owned());
            });
        }
        if let Some(persistence) = remote_persistence {
            registry.update_record(&p.session_id.0, |record| {
                record.remote_persistence = Some(persistence);
            });
        }
        self.restored_resume_result(&mut registry, &p.session_id.0)
    }

    fn restored_resume_result(
        &self,
        registry: &mut Registry,
        id: &str,
    ) -> Result<JsonValue, ControlError> {
        // Resume is also the UI's revive action. Clear the durable archive
        // only after launch succeeds, including an already-live session.
        registry.unarchive(id).map_err(io_control_error)?;
        let _ = registry.persist();
        self.publish_updated(registry, id);
        let record = registry
            .record(id)
            .ok_or_else(|| ControlError::internal("the resumed session vanished"))?;
        serde_json::to_value(&record).map_err(|error| ControlError::internal(error.to_string()))
    }

    /// Starts a new Session whose provider conversation is a native fork of
    /// `source`. The Diri record and provider identity are both new; lineage
    /// points at the source and the source remains untouched.
    fn session_fork(&self, params: Option<JsonValue>) -> Result<JsonValue, ControlError> {
        let p: diri_proto::SessionForkParams = decode(params)?;
        let source = {
            let registry = self.registry.lock().map_err(poisoned)?;
            registry
                .record(&p.session_id.0)
                .ok_or_else(|| ControlError::not_found(p.session_id.0.clone()))?
        };
        // An Agent started by hand in a shell has no conversation Diri
        // knows, so a shell forks as the shell it is.
        let kind = if source.kind == diri_proto::AgentKind::SHELL {
            source.kind.clone()
        } else {
            source.effective_kind().clone()
        };
        let id = next_session_id();
        let mut spec = if source.host.is_some() {
            self.remote_conversation_spec(&source, &id, &kind, ConversationAction::Fork)?
        } else {
            let registry = self.registry.lock().map_err(poisoned)?;
            self.local_conversation_spec(
                &registry,
                &id,
                kind.id(),
                &source.cwd,
                source.agent_session_id.as_deref(),
                ConversationAction::Fork,
            )?
        };
        let remote_persistence = spec.remote.as_ref().map(|remote| remote.launch.persistence);
        if source.host.is_none()
            && let Some(mut profile) = source.account_profile.clone()
        {
            if profile.host.is_some() || profile.agent != kind.id() {
                return Err(ControlError::bad_request(
                    "Session account does not match its Agent or host",
                ));
            }
            crate::accounts::bind_pty(&mut profile, &mut spec.pty)?;
        }
        let mut record = new_record(&id, kind.id(), &source.cwd);
        record.account_profile = source.account_profile.clone();
        record.kind = kind;
        record.project_id = source.project_id.clone();
        record.worktree_path = source.worktree_path.clone();
        record.git_branch = source.git_branch.clone();
        record.parent = Some(p.parent.clone().unwrap_or_else(|| source.id.clone()));
        record.host = source.host.clone();
        record.remote_persistence = remote_persistence;
        record.title = format!("Fork of {}", source.title);
        record.title_source = diri_proto::TitleSource::DirijorAssigned;

        if spec.remote.is_some() {
            self.spawn_session_unlocked(spec, Some(record))?;
        } else {
            self.registry
                .lock()
                .map_err(poisoned)?
                .spawn(spec, record)
                .map_err(io_control_error)?;
        }
        let mut registry = self.registry.lock().map_err(poisoned)?;
        self.ensure_published_project(&mut registry, &source.cwd, source.host.as_deref());
        let _ = registry.persist();
        self.publish_updated(&registry, &id);
        let record = registry
            .record(&id)
            .ok_or_else(|| ControlError::internal("the forked session vanished"))?;
        encode(&record)
    }

    fn remote_resume_spec(
        &self,
        record: &diri_proto::SessionRecord,
    ) -> Result<crate::session::SessionSpec, ControlError> {
        self.remote_conversation_spec(
            record,
            &record.id.0,
            &record.kind,
            ConversationAction::Resume,
        )
    }

    fn remote_conversation_spec(
        &self,
        record: &diri_proto::SessionRecord,
        target_id: &str,
        kind: &diri_proto::AgentKind,
        action: ConversationAction,
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
        let (mut descriptor, authority) = {
            let registry = self.registry.lock().map_err(poisoned)?;
            let engine = registry.engine();
            let manifest = engine.manifest(kind.id()).ok_or_else(|| {
                ControlError::not_found(format!("no manifest for agent {}", kind.id()))
            })?;
            let descriptor = manifest.agent.clone().unwrap_or_default();
            let authority = descriptor.authority();
            (descriptor, authority)
        };
        let captured = if let Some(binary) = descriptor.binary.as_deref() {
            let (executable, captured) = self.discover_remote_agent_for_launch(
                manager.as_ref(),
                &host,
                kind.id(),
                binary,
                record.cwd.clone(),
            )?;
            descriptor.binary = Some(executable);
            captured
        } else {
            manager
                .capture_environment(
                    &helper,
                    &diri_proto::remote_pty::EnvironmentCaptureRequest {
                        cwd: Some(record.cwd.clone()),
                        timeout_millis: 10_000,
                    },
                )
                .map_err(io_control_error)?
        };
        let cwd = PathBuf::from(&captured.cwd);
        if !cwd.is_absolute() {
            return Err(ControlError::internal(
                "remote Helper returned a non-absolute cwd",
            ));
        }
        let mut launch_args = descriptor.spawn_args.clone();
        let provider_dir = captured
            .environment
            .iter()
            .rev()
            .find(|variable| variable.name == "HOME" && !variable.value.is_empty())
            .map(|variable| {
                Path::new(&variable.value)
                    .join(".diri/session-storage")
                    .join(target_id)
            });
        let launch = match action {
            ConversationAction::Resume => crate::agent::ConversationLaunch::Resume {
                source_id: record.agent_session_id.as_deref(),
                session_dir: provider_dir.as_deref(),
            },
            ConversationAction::Fork => crate::agent::ConversationLaunch::Fork {
                source_id: record.agent_session_id.as_deref(),
                session_dir: provider_dir.as_deref(),
            },
            ConversationAction::Fresh => crate::agent::ConversationLaunch::Fresh {
                new_id: record.agent_session_id.as_deref(),
                session_dir: provider_dir.as_deref(),
            },
        };
        launch_args = descriptor
            .conversation_plan(&launch_args, launch)
            .ok_or_else(|| {
                ControlError::bad_request(format!(
                    "agent {} does not support {}",
                    kind.id(),
                    match action {
                        ConversationAction::Resume => "resume",
                        ConversationAction::Fork => "fork",
                        ConversationAction::Fresh => "a fresh launch",
                    }
                ))
            })?
            .args;
        let inherited = captured
            .environment
            .into_iter()
            .map(|variable| (variable.name, variable.value));
        let mut pty = descriptor
            .remote_spawn_spec(&cwd, inherited, &launch_args)
            .ok_or_else(|| {
                ControlError::bad_request(format!("agent {} declares no binary", kind.id()))
            })?;
        if let Some(mut profile) = record.account_profile.clone() {
            if profile.host != record.host || profile.agent != kind.id() {
                return Err(ControlError::bad_request(
                    "Session account binding does not match its execution target",
                ));
            }
            crate::accounts::bind_pty(&mut profile, &mut pty)?;
            crate::accounts::prepare_remote_directory(&profile, &host, &manager)?;
        }
        let launch = diri_proto::remote_pty::LaunchRequest {
            session_id: target_id.to_owned(),
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
            id: target_id.to_owned(),
            pty,
            manifest_id: kind.id().to_string(),
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

    /// A fresh login shell under an existing local terminal's id, started in
    /// `directory` when there is one and in the terminal's `cwd` otherwise.
    /// It is launched exactly as `session.spawn` launches a new terminal.
    fn shell_restart_spec(
        &self,
        record: &diri_proto::SessionRecord,
        directory: Option<&Path>,
    ) -> Result<crate::session::SessionSpec, ControlError> {
        let registry = self.registry.lock().map_err(poisoned)?;
        let kind = diri_proto::AgentKind::SHELL_ID;
        let launch_path = directory.unwrap_or_else(|| Path::new(&record.cwd));
        let mut pty = crate::pty::PtySpec::new(login_shell_argv(), launch_path);
        pty.env = std::env::vars().collect();
        crate::agent::assert_color_environment(&mut pty.env);
        Ok(crate::session::SessionSpec {
            id: record.id.0.clone(),
            pty,
            manifest_id: kind.to_owned(),
            authority: crate::session::authority_for(kind, &registry.engine()),
            logs_dir: self.logs_dir.clone(),
            holder: self.holder.clone(),
            remote: None,
            defer_launch: true,
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
        let prompt = p.initial_prompt.filter(|prompt| !prompt.is_empty());
        let mut record = new_record(&id, &kind, &p.entry.cwd);
        record.agent_session_id = Some(p.entry.id.clone());
        record.transcript_path = Some(p.entry.transcript_path.clone());
        if let Some(title) = &p.entry.title {
            record.title = title.clone();
            record.title_source = diri_proto::TitleSource::FirstPrompt;
        }
        let spec = self.resume_spec(&registry, &id, &kind, &p.entry.cwd, Some(&p.entry.id))?;
        self.ensure_published_project(&mut registry, &p.entry.cwd, None);
        let spawn_started = Instant::now();
        registry
            .spawn(spec, record)
            .map_err(|error| ControlError::internal(error.to_string()))?;
        if let Some(record) = registry.record(&id) {
            crate::telemetry::record_session_spawn(
                &record,
                "history",
                spawn_started.elapsed(),
                registry.other_live_agent(&id),
            );
        }
        let _ = registry.persist();
        self.publish_updated(&registry, &id);
        let record = registry
            .record(&id)
            .ok_or_else(|| ControlError::internal("the resumed session vanished"))?;
        drop(registry);
        // A resumed conversation is past Claude's first run.
        let appearance = None;
        let accept_claude_workspace = kind == diri_proto::AgentKind::CLAUDE_CODE_ID;
        if let Some(prompt) = prompt {
            prepare_agent_input(
                &self.registry,
                &id,
                accept_claude_workspace,
                appearance,
                Some(&prompt),
            )
            .map_err(|error| initial_prompt_control_error(&id, error))?;
        } else if accept_claude_workspace {
            let registry = Arc::clone(&self.registry);
            let session_id = id.clone();
            std::thread::spawn(move || {
                let _ = prepare_agent_input(&registry, &session_id, true, appearance, None);
            });
        }
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
        self.local_conversation_spec(
            registry,
            id,
            kind,
            cwd,
            agent_session_id,
            ConversationAction::Resume,
        )
    }

    /// A new conversation on an existing tab that keeps its minted id.
    pub(super) fn fresh_spec(
        &self,
        registry: &Registry,
        id: &str,
        kind: &str,
        cwd: &str,
        agent_session_id: Option<&str>,
    ) -> Result<crate::session::SessionSpec, ControlError> {
        self.local_conversation_spec(
            registry,
            id,
            kind,
            cwd,
            agent_session_id,
            ConversationAction::Fresh,
        )
    }

    fn local_conversation_spec(
        &self,
        registry: &Registry,
        id: &str,
        kind: &str,
        cwd: &str,
        agent_session_id: Option<&str>,
        action: ConversationAction,
    ) -> Result<crate::session::SessionSpec, ControlError> {
        let engine = registry.engine();
        let manifest = engine
            .manifest(kind)
            .ok_or_else(|| ControlError::not_found(format!("no manifest for agent {kind}")))?;
        let mut descriptor = manifest.agent.clone().unwrap_or_default();
        descriptor
            .binary
            .as_ref()
            .ok_or_else(|| ControlError::bad_request(format!("agent {kind} declares no binary")))?;
        let binary = descriptor.binary.clone().expect("checked above");
        descriptor.binary = Some(self.resolve_local_agent_executable(kind, &binary)?);
        let mut launch_args = descriptor.spawn_args.clone();
        if let Some(injection) = &self.injection {
            launch_args.extend(crate::inject::injection_args_with_cursor(
                &descriptor.injection,
                &injection.inject_dir,
                &injection.cli_path,
                Some(crate::inject::CursorInject {
                    session_id: id,
                    socket_path: &self.socket_path,
                }),
            ));
        }
        let provider_dir = registry.recovery_directory(id).join("provider");
        let launch = match action {
            ConversationAction::Resume => crate::agent::ConversationLaunch::Resume {
                source_id: agent_session_id,
                session_dir: Some(&provider_dir),
            },
            ConversationAction::Fork => crate::agent::ConversationLaunch::Fork {
                source_id: agent_session_id,
                session_dir: Some(&provider_dir),
            },
            ConversationAction::Fresh => crate::agent::ConversationLaunch::Fresh {
                new_id: agent_session_id,
                session_dir: Some(&provider_dir),
            },
        };
        launch_args = descriptor
            .conversation_plan(&launch_args, launch)
            .ok_or_else(|| {
                ControlError::bad_request(format!(
                    "agent {kind} does not support {}",
                    match action {
                        ConversationAction::Resume => "resume",
                        ConversationAction::Fork => "fork",
                        ConversationAction::Fresh => "a fresh launch",
                    }
                ))
            })?
            .args;

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
            pty.env.push((
                diri_proto::paths::ENV_SESSION_RECOVERY_DIR.into(),
                registry
                    .recovery_directory(id)
                    .to_string_lossy()
                    .into_owned(),
            ));
            if let Some(dir) = self.resolved_notes_dir() {
                pty.env.push((
                    diri_proto::paths::ENV_NOTES_DIR.into(),
                    dir.to_string_lossy().into_owned(),
                ));
            }
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

    /// Pops the most recently closed session whose folder still exists,
    /// re-lists it (exited), and relaunches it through the resume path. An
    /// Agent that cannot resume stays listed as exited rather than failing
    /// the reopen.
    fn session_reopen_last(&self) -> Result<JsonValue, ControlError> {
        let record = {
            let mut registry = self.registry.lock().map_err(poisoned)?;
            let record = registry
                .reopen_last_closed()
                .ok_or_else(|| ControlError::bad_request("no recently closed session"))?;
            let _ = registry.persist();
            self.publish_updated(&registry, &record.id.0);
            record
        };
        // A note has nothing to relaunch: bring its file back and show it.
        if record.is_note() {
            if let Some(note_id) = &record.note_id
                && let Ok(store) = self.note_store()
                && store.path_for(note_id).is_ok_and(|path| !path.exists())
            {
                let _ = store.restore(note_id);
            }
            return serde_json::to_value(&record)
                .map_err(|error| ControlError::internal(error.to_string()));
        }
        match self.session_resume(Some(serde_json::json!({ "sessionID": record.id.0 }))) {
            Ok(resumed) => Ok(resumed),
            Err(error) => {
                eprintln!(
                    "diri-engine: reopened session {} could not relaunch: {}",
                    record.id.0, error.message
                );
                serde_json::to_value(&record)
                    .map_err(|error| ControlError::internal(error.to_string()))
            }
        }
    }

    /// Manifest catalog plus executable facts for one execution target. The
    /// scan is batched and target-keyed; a menu render never invokes this RPC.
    fn agent_readiness(&self, params: Option<JsonValue>) -> Result<JsonValue, ControlError> {
        let p: diri_proto::AgentReadinessParams = decode(params).unwrap_or_default();
        encode(&self.agent_catalog(&p, false)?)
    }

    fn resolve_local_agent_executable(
        &self,
        kind: &str,
        binary: &str,
    ) -> Result<String, ControlError> {
        let preference = self
            .agent_catalog
            .lock()
            .map_err(poisoned)?
            .preference(None, kind);
        // Only an explicit user-configured path overrides the manifest. With
        // no override the binary deliberately stays bare: `spawn_spec` either
        // hands it to a fresh interactive login shell (which resolves the
        // nvm/mise/Homebrew PATH the daemon never inherited) or absolutizes
        // it against the spawn environment. Judging availability by the
        // daemon's own PATH here would reject agents the login shell can
        // launch, and pin versions to whatever the daemon saw at startup.
        let Some(configured) = preference.executable_path.as_deref() else {
            return Ok(binary.to_owned());
        };
        let resolution = crate::agent_catalog::resolve_local(binary, Some(configured));
        resolution
            .configured_path
            .ok_or_else(|| agent_unavailable(kind, None, resolution.configured_error.as_deref()))
    }

    fn discover_remote_agent_for_launch(
        &self,
        manager: &crate::remote::manager::RemoteManager,
        host: &diri_proto::HostEntry,
        kind: &str,
        binary: &str,
        cwd: String,
    ) -> Result<(String, diri_proto::remote_pty::EnvironmentCaptureResult), ControlError> {
        let preference = self
            .agent_catalog
            .lock()
            .map_err(poisoned)?
            .preference(Some(&host.id), kind);
        let result = manager
            .discover_executables(
                host,
                &diri_proto::remote_pty::ExecutableDiscoveryRequest {
                    queries: vec![diri_proto::remote_pty::ExecutableQuery {
                        id: kind.to_owned(),
                        binary: binary.to_owned(),
                        configured_path: preference.executable_path,
                    }],
                    cwd: Some(cwd),
                    timeout_millis: 10_000,
                },
            )
            .map_err(io_control_error)?;
        let resolution = result.items.into_iter().next().ok_or_else(|| {
            ControlError::internal("remote Helper omitted the executable discovery result")
        })?;
        let host_name = host.display_name();
        let executable = resolution
            .configured_path
            .or(resolution.detected_path)
            .ok_or_else(|| {
                agent_unavailable(
                    kind,
                    Some(host_name),
                    resolution.configured_error.as_deref(),
                )
            })?;
        Ok((executable, result.environment))
    }

    fn agent_configure(&self, params: Option<JsonValue>) -> Result<JsonValue, ControlError> {
        let p: diri_proto::AgentConfigureParams = decode(params)?;
        let kind = p.kind.id().to_owned();
        {
            let registry = self.registry.lock().map_err(poisoned)?;
            let engine = registry.engine();
            let manifest = engine.manifest(&kind).ok_or_else(|| {
                ControlError::not_found(format!("no manifest for agent {kind:?}"))
            })?;
            if manifest
                .agent
                .as_ref()
                .and_then(|agent| agent.binary.as_ref())
                .is_none()
            {
                return Err(ControlError::bad_request(
                    "terminal and generic manifests cannot be configured as Agents",
                ));
            }
        }
        if let Some(host) = p.host.as_deref() {
            self.resolve_host(host)?;
        }
        let preference = crate::agent_catalog::AgentPreference {
            executable_path: p.executable_path,
            show_in_quick_create: Some(p.show_in_quick_create),
        };
        self.agent_catalog
            .lock()
            .map_err(poisoned)?
            .configure(p.host.as_deref(), &kind, preference)
            .map_err(io_control_error)?;
        let result = self.agent_catalog(
            &diri_proto::AgentReadinessParams {
                host: p.host,
                force_refresh: true,
            },
            true,
        )?;
        encode(&result)
    }

    fn agent_catalog(
        &self,
        params: &diri_proto::AgentReadinessParams,
        _validate_configured: bool,
    ) -> Result<diri_proto::AgentReadinessResult, ControlError> {
        if let Some(host) = params.host.as_deref() {
            self.resolve_host(host)?;
        }
        if !params.force_refresh
            && let Some(cached) = self
                .agent_catalog
                .lock()
                .map_err(poisoned)?
                .cached(params.host.as_deref())
        {
            return Ok(cached);
        }
        let force_baseline = if params.force_refresh {
            self.agent_catalog
                .lock()
                .map_err(poisoned)?
                .cached(params.host.as_deref())
        } else {
            None
        };

        // Single-flight each target. A slow remote never blocks local or a
        // different host, while concurrent menus/settings share one scan.
        let target_key = params.host.as_deref().unwrap_or("local").to_owned();
        let scan_lock = self
            .agent_scans
            .lock()
            .map_err(poisoned)?
            .entry(target_key)
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone();
        let _scan = scan_lock.lock().map_err(poisoned)?;
        if params.force_refresh {
            let current = self
                .agent_catalog
                .lock()
                .map_err(poisoned)?
                .cached(params.host.as_deref());
            if current != force_baseline
                && let Some(current) = current
            {
                return Ok(current);
            }
        }
        if !params.force_refresh
            && let Some(cached) = self
                .agent_catalog
                .lock()
                .map_err(poisoned)?
                .cached(params.host.as_deref())
        {
            return Ok(cached);
        }

        let manifests = {
            let registry = self.registry.lock().map_err(poisoned)?;
            let engine = registry.engine();
            let mut manifests = engine
                .ids()
                .into_iter()
                .filter_map(|id| {
                    let descriptor = engine.manifest(id)?.agent.as_ref()?;
                    let binary = descriptor.binary.clone()?;
                    Some((
                        id.to_owned(),
                        binary,
                        descriptor.catalog_order.unwrap_or(u16::MAX),
                        engine.raw_agent(id).cloned(),
                    ))
                })
                .collect::<Vec<_>>();
            manifests
                .sort_by(|left, right| left.2.cmp(&right.2).then_with(|| left.0.cmp(&right.0)));
            manifests
        };
        let preferences = {
            let catalog = self.agent_catalog.lock().map_err(poisoned)?;
            manifests
                .iter()
                .map(|(id, _, _, _)| catalog.preference(params.host.as_deref(), id))
                .collect::<Vec<_>>()
        };

        let resolutions = if let Some(host_id) = params.host.as_deref() {
            let manager = self
                .remote
                .as_ref()
                .ok_or_else(crate::remote::transport_unavailable)?;
            let host = self.resolve_host(host_id)?;
            let result = manager
                .discover_executables(
                    &host,
                    &diri_proto::remote_pty::ExecutableDiscoveryRequest {
                        queries: manifests
                            .iter()
                            .zip(&preferences)
                            .map(|((id, binary, _, _), preference)| {
                                diri_proto::remote_pty::ExecutableQuery {
                                    id: id.clone(),
                                    binary: binary.clone(),
                                    configured_path: preference.executable_path.clone(),
                                }
                            })
                            .collect(),
                        cwd: None,
                        timeout_millis: 10_000,
                    },
                )
                .map_err(io_control_error)?;
            let by_id = result
                .items
                .into_iter()
                .map(|item| (item.id.clone(), item))
                .collect::<std::collections::HashMap<_, _>>();
            manifests
                .iter()
                .map(|(id, _, _, _)| {
                    let item = by_id.get(id);
                    crate::agent_catalog::ExecutableResolution {
                        detected_path: item.and_then(|item| item.detected_path.clone()),
                        configured_path: item.and_then(|item| item.configured_path.clone()),
                        configured_error: item.and_then(|item| item.configured_error.clone()),
                    }
                })
                .collect::<Vec<_>>()
        } else {
            manifests
                .iter()
                .zip(&preferences)
                .map(|((_, binary, _, _), preference)| {
                    crate::agent_catalog::resolve_local(
                        binary,
                        preference.executable_path.as_deref(),
                    )
                })
                .collect::<Vec<_>>()
        };

        let mut agents = Vec::with_capacity(manifests.len());
        for (((id, binary, _, raw_descriptor), preference), resolution) in
            manifests.into_iter().zip(preferences).zip(resolutions)
        {
            let path = resolution
                .configured_path
                .clone()
                .or_else(|| resolution.detected_path.clone());
            let show = preference.show_in_quick_create.unwrap_or(path.is_some()) && path.is_some();
            let path_source = if resolution.configured_path.is_some() {
                Some(diri_proto::AgentPathSource::Manual)
            } else if resolution.detected_path.is_some() {
                Some(diri_proto::AgentPathSource::SystemPath)
            } else {
                None
            };
            let descriptor = raw_descriptor.and_then(|value| {
                serde_json::from_value::<diri_proto::AgentDescriptor>(value).ok()
            });
            let signed_in = if params.host.is_none() && path.is_some() {
                self.agent_signed_in(&id)
            } else {
                None
            };
            agents.push(diri_proto::AgentReadinessItem {
                signed_in,
                kind: diri_proto::AgentKind::new(id),
                binary,
                path,
                detected_path: resolution.detected_path,
                configured_path: preference.executable_path,
                path_source,
                show_in_quick_create: show,
                error: resolution.configured_error,
                descriptor,
            });
        }
        let result = diri_proto::AgentReadinessResult {
            host: params.host.clone(),
            scanned_at: Some(diri_proto::DateMillis::from(std::time::SystemTime::now())),
            agents,
        };
        self.agent_catalog
            .lock()
            .map_err(poisoned)?
            .cache(params.host.as_deref(), result.clone());
        Ok(result)
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
                .record(&p.session_id.0)
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
        registry.persist_for_shutdown().map_err(io_control_error)?;
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
                registry.persist_for_shutdown().map_err(io_control_error)?;
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
                        release_idle_engine(remote, holder, browser, &socket_path);
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

    /// Lets an Engine nobody can reach any more retire itself. The App asks for
    /// `daemon.shutdown_if_idle` when it quits cleanly; an App that was killed
    /// never asks, and its Engine then outlived it for days with no sessions
    /// and no client. Only an Engine launched with the opt-in runs this: one
    /// kept up by a service manager must stay up while idle.
    ///
    /// The test is the one the request applies, held continuously for `grace`:
    /// a live session or any connection resets it, so nothing that could be
    /// stranded or interrupted ever sees the exit.
    pub fn spawn_orphan_watch(self: &Arc<Self>, grace: Duration, tick: Duration) {
        let server = Arc::clone(self);
        let _ = std::thread::Builder::new()
            .name("dirijord-orphan-watch".into())
            .spawn(move || {
                let mut watch = OrphanWatch::default();
                loop {
                    std::thread::sleep(tick);
                    let connections = server.active_connections.load(Ordering::Acquire);
                    let Ok(mut registry) = server.registry.lock() else {
                        return;
                    };
                    // An enabled schedule is work the Engine must stay up for.
                    let live_sessions = registry.live_count() + server.scheduler.enabled_count();
                    if !watch.observe(live_sessions, connections, Instant::now(), grace) {
                        continue;
                    }
                    if registry.persist_for_shutdown().is_err() {
                        // Losing the final write would cost state; stay up.
                        watch = OrphanWatch::default();
                        continue;
                    }
                    drop(registry);
                    eprintln!("dirijord-rs: no session or client for {grace:?}; exiting");
                    release_idle_engine(
                        server.remote.clone(),
                        server.holder.clone(),
                        server.browser.get().cloned(),
                        &server.socket_path,
                    );
                }
            });
    }

    /// Ack first, then exit: the response has to flush before the process
    /// dies, so the client sees a clean reply followed by a socket drop and
    /// relaunches the fresh binary.
    fn daemon_shutdown(&self) -> Result<JsonValue, ControlError> {
        {
            let mut registry = self.registry.lock().map_err(poisoned)?;
            registry.persist_for_shutdown().map_err(io_control_error)?;
        }
        let browser = self.browser.get().cloned();
        let socket_path = self.socket_path.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(200));
            if let Some(browser) = browser {
                browser.shutdown();
            }
            let _ = std::fs::remove_file(socket_path);
            diri_telemetry::event!("engine.exit", reason = "shutdown");
            diri_telemetry::flush(Duration::from_secs(1));
            std::process::exit(0);
        });
        Ok(json!({}))
    }

    /// Inserts the session root when it is not already a project, and tells
    /// live clients. Hydrate only learns projects from `session.list`, so a
    /// folder that first appears from a spawn stays out of the sidebar order
    /// until this `project.updated`.
    fn ensure_published_project(&self, registry: &mut Registry, root: &str, host: Option<&str>) {
        let id = crate::registry::session_project_id(root, host).0;
        let inserted = !registry
            .projects_raw()
            .iter()
            .any(|project| project.get("id").and_then(|value| value.as_str()) == Some(id.as_str()));
        let project = registry.ensure_session_project(root, host);
        if inserted {
            self.events
                .publish(diri_proto::EventName::PROJECT_UPDATED, project, None);
        }
    }

    /// Publishes `session.updated` with the session's current record.
    /// Relays a request to show a Session to every app window.
    fn session_reveal(&self, params: Option<JsonValue>) -> Result<JsonValue, ControlError> {
        let p: diri_proto::SessionIdParams = decode(params)?;
        let id = p.session_id.0;
        if self
            .registry
            .lock()
            .map_err(poisoned)?
            .record(&id)
            .is_none()
        {
            return Err(ControlError::not_found(format!("no session {id}")));
        }
        self.events.publish_encoded(
            diri_proto::EventName::SESSION_REVEAL,
            &json!({ "sessionID": id }),
            Some(&id),
        );
        Ok(json!({}))
    }

    fn publish_updated(&self, registry: &Registry, id: &str) {
        // One folded record, not a folded copy of the whole table.
        if let Some(record) = registry.record(id) {
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

    /// Open control and data connections.
    pub fn connection_count(&self) -> usize {
        self.active_connections.load(Ordering::SeqCst)
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

/// Which Claude conversation relaunching this local tab should re-enter:
/// `None` leaves the record's id untouched (not Claude, or nothing to check
/// against), `Some(Some(id))` resumes a conversation whose transcript exists,
/// and `Some(None)` means the tab's id was never written and must start fresh.
/// The grid size of this session's last screen checkpoint: the size its
/// previous run was using when the Engine last saved it.
fn previous_screen_size(spec: &crate::session::SessionSpec) -> Option<(usize, usize)> {
    let log = spec.logs_dir.join(format!("{}.bin", spec.id));
    let checkpoint = crate::checkpoint::ScreenCheckpoint::load(
        &crate::checkpoint::ScreenCheckpoint::path_for_log(&log),
    )?;
    Some((
        usize::from(checkpoint.grid.cols),
        usize::from(checkpoint.grid.rows),
    ))
}

fn claude_resume_target(record: &diri_proto::SessionRecord) -> Option<Option<String>> {
    claude_resume_target_in(record, Path::new(&std::env::var_os("HOME")?))
}

fn claude_resume_target_in(
    record: &diri_proto::SessionRecord,
    home: &Path,
) -> Option<Option<String>> {
    if record.host.is_some() || record.kind.id() != diri_proto::AgentKind::CLAUDE_CODE_ID {
        return None;
    }
    let shared = home.join(".claude/projects");
    if !shared.is_dir() {
        return None;
    }
    let mut roots = vec![shared];
    if let Some(profile) = &record.account_profile
        && Path::new(&profile.config_home).is_absolute()
    {
        roots.push(Path::new(&profile.config_home).join("projects"));
    }
    let agent_session_id = record.agent_session_id.as_deref();
    match crate::history::claude_resumable_conversation(
        &roots,
        agent_session_id,
        record.transcript_path.as_deref(),
    ) {
        Some(id) => Some(Some(id)),
        None => agent_session_id.map(|_| None),
    }
}

fn decode_launch_argv(params: &JsonValue) -> Result<Vec<String>, ControlError> {
    let Some(value) = params.get("argv") else {
        return Ok(Vec::new());
    };
    let argv: Vec<String> = serde_json::from_value(value.clone())
        .map_err(|_| ControlError::bad_request("argv must be an array of strings"))?;
    if argv.is_empty() || argv.len() > diri_proto::remote_pty::MAX_ARGUMENTS {
        return Err(ControlError::bad_request(
            "argv must contain 1..=512 entries",
        ));
    }
    if argv[0].is_empty() || argv.iter().any(|argument| argument.contains('\0')) {
        return Err(ControlError::bad_request(
            "argv needs a nonempty executable and NUL-free arguments",
        ));
    }
    if argv.iter().map(String::len).sum::<usize>() > diri_proto::remote_pty::MAX_LAUNCH_BYTES {
        return Err(ControlError::bad_request(
            "argv exceeds the launch byte limit",
        ));
    }
    Ok(argv)
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

/// Who wrote a note the Engine creates: the agent it was created for, else
/// the person at the app.
fn note_author(parent: Option<&diri_proto::SessionId>) -> diri_notes::history::Author {
    parent.map_or(diri_notes::history::Author::User, |parent| {
        diri_notes::history::Author::Session(parent.0.clone())
    })
}

/// The note Session showing `note_id`, if any.
fn note_session_for(registry: &Registry, note_id: &str) -> Option<diri_proto::SessionRecord> {
    registry
        .records()
        .into_iter()
        .find(|record| record.is_note() && record.note_id.as_deref() == Some(note_id))
}

/// Where an adopted note lives in the sidebar: its own project folder when it
/// still exists, else the folder the caller asked for, else the home folder,
/// where a note with no project (an Inbox note) belongs, the same place a new
/// terminal without a project opens.
fn note_home(project: Option<&str>, requested: Option<&str>) -> Result<String, ControlError> {
    project
        .into_iter()
        .chain(requested)
        .find(|path| Path::new(path).is_dir())
        .map(str::to_owned)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|home| Path::new(home).is_dir())
                .map(|home| home.to_string_lossy().into_owned())
        })
        .ok_or_else(|| ControlError::internal("no folder to place the note in"))
}

fn note_record(
    id: &str,
    cwd: &str,
    title: &str,
    note_id: String,
    parent: Option<diri_proto::SessionId>,
) -> diri_proto::SessionRecord {
    let mut record = new_record(id, diri_proto::AgentKind::NOTE_ID, cwd);
    record.kind = diri_proto::AgentKind::NOTE;
    record.project_id = crate::registry::session_project_id(cwd, None);
    if title.trim().is_empty() {
        record.title = "Untitled".into();
    } else {
        record.title = title.trim().to_owned();
        record.title_source = diri_proto::TitleSource::DirijorAssigned;
    }
    record.parent = parent;
    record.git_branch = None;
    record.status = diri_proto::SessionStatus::Idle;
    record.resumability = diri_proto::Resumability::NotResumable;
    record.note_id = Some(note_id);
    record
}

pub(crate) fn new_record(id: &str, kind: &str, cwd: &str) -> diri_proto::SessionRecord {
    use diri_proto::{AgentKind, DateMillis, Resumability, SessionId, TitleSource};
    let now: DateMillis = std::time::SystemTime::now().into();
    diri_proto::SessionRecord {
        attention_state: None,
        id: SessionId(id.to_string()),
        kind: AgentKind::new(kind),
        cwd: cwd.to_string(),
        project_id: crate::registry::session_project_id(cwd, None),
        worktree_path: None,
        git_branch: None,
        title: kind.to_string(),
        title_source: TitleSource::Placeholder,
        account_profile: None,
        originating_prompt: None,
        agent_session_id: None,
        transcript_path: None,
        status: diri_proto::SessionStatus::Starting,
        status_evidence: None,
        needs_input: None,
        resumability: Resumability::Live,
        capabilities: None,
        parent: None,
        created_at: now,
        updated_at: now,
        last_turn_completed_at: None,
        last_seen_at: None,
        pinned: false,
        archived_at: None,
        host: None,
        remote_persistence: None,
        remote_connection: None,
        hibernation: None,
        memory_bytes: None,
        artifacts: None,
        pull_requests: None,
        listening_ports: None,
        foreground_agent: None,
        terminal_cwd: None,
        note_id: None,
        foreground_ports: None,
        terminal_progress: None,
        scheduled_run: None,
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

struct BackgroundRequest(Arc<AtomicUsize>);

impl BackgroundRequest {
    fn acquire(counter: &Arc<AtomicUsize>) -> Option<Self> {
        counter
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |active| {
                (active < 32).then_some(active + 1)
            })
            .ok()?;
        Some(Self(Arc::clone(counter)))
    }
}

impl Drop for BackgroundRequest {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Release);
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

/// The last steps of an Engine that has nothing left to look after. Callers
/// have already persisted the registry and checked that no session is live.
fn release_idle_engine(
    remote: Option<Arc<crate::remote::manager::RemoteManager>>,
    holder: Option<crate::session::HolderConfig>,
    browser: Option<crate::browser::BrowserPool>,
    socket_path: &Path,
) -> ! {
    if let Some(remote) = remote {
        let _ = remote.close_control_masters();
    }
    if let Some(holder) = holder {
        let paths = crate::holder::HolderManagerPaths::new(&holder.holders_dir);
        let _ = crate::holder::HolderManagerClient::new(paths.socket()).shutdown_if_idle();
    }
    if let Some(browser) = browser {
        browser.shutdown();
    }
    let _ = std::fs::remove_file(socket_path);
    diri_telemetry::event!("engine.exit", reason = "idle");
    diri_telemetry::flush(Duration::from_secs(1));
    std::process::exit(0);
}

/// How long an Engine has been unreachable: no live session and no client.
#[derive(Default)]
struct OrphanWatch {
    idle_since: Option<Instant>,
}

impl OrphanWatch {
    /// True once the Engine has been idle for `grace` without interruption.
    fn observe(
        &mut self,
        live_sessions: usize,
        connections: usize,
        now: Instant,
        grace: Duration,
    ) -> bool {
        if live_sessions != 0 || connections != 0 {
            self.idle_since = None;
            return false;
        }
        let since = *self.idle_since.get_or_insert(now);
        now.duration_since(since) >= grace
    }
}

/// Uploads the telemetry spool now at the user's request (Settings, Report a
/// Problem). Runs as a background request: the upload can take seconds.
fn telemetry_upload_now() -> Result<JsonValue, ControlError> {
    use diri_telemetry::upload::UploadNow;
    let result = match diri_telemetry::upload::upload_now(Duration::from_secs(45)) {
        UploadNow::Unavailable => diri_proto::TelemetryUploadNowResult {
            status: "unavailable".into(),
            ..Default::default()
        },
        UploadNow::TimedOut => diri_proto::TelemetryUploadNowResult {
            status: "timeout".into(),
            ..Default::default()
        },
        UploadNow::Done(report) => diri_proto::TelemetryUploadNowResult {
            status: if report.failed {
                "failed"
            } else if report.batches == 0 {
                "up_to_date"
            } else {
                "sent"
            }
            .into(),
            batches: u32::try_from(report.batches).unwrap_or(u32::MAX),
            records: report.lines as u64,
        },
    };
    diri_telemetry::event!(
        "telemetry.upload_now",
        status = diri_telemetry::id(&result.status),
        batches = result.batches,
    );
    encode(&result)
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

fn read_bounded_control_line<R: BufRead>(reader: &mut R) -> std::io::Result<Option<Vec<u8>>> {
    let mut line = Vec::new();
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return if line.is_empty() {
                Ok(None)
            } else {
                Ok(Some(line))
            };
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let consumed = newline.map_or(available.len(), |position| position + 1);
        let payload = newline.unwrap_or(available.len());
        if line.len().saturating_add(payload) > MAX_CONTROL_LINE_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "control line exceeded the protocol maximum",
            ));
        }
        line.extend_from_slice(&available[..payload]);
        reader.consume(consumed);
        if newline.is_some() {
            return Ok(Some(line));
        }
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

/// Writes the same line `ControlMessage::Event` serializes to, around params
/// the bus encoded once, instead of decoding and re-encoding them for every
/// subscriber.
fn write_event_frame(
    writer: &Arc<Mutex<UnixStream>>,
    event: &crate::events::Event,
) -> std::io::Result<()> {
    let mut bytes = Vec::with_capacity(event.encoded.len() + event.name.len() + 48);
    bytes.extend_from_slice(b"{\"event\":");
    serde_json::to_writer(&mut bytes, &event.name)?;
    bytes.extend_from_slice(b",\"seq\":");
    bytes.extend_from_slice(event.seq.to_string().as_bytes());
    bytes.extend_from_slice(b",\"params\":");
    bytes.extend_from_slice(&event.encoded);
    bytes.extend_from_slice(b"}\n");
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

fn migrate_control_error(error: crate::migrate::MigrateError) -> ControlError {
    match error {
        crate::migrate::MigrateError::BadRequest(message) => ControlError::bad_request(message),
        crate::migrate::MigrateError::Internal(message) => ControlError::internal(message),
    }
}

fn io_control_error(error: std::io::Error) -> ControlError {
    if error
        .get_ref()
        .is_some_and(|cause| cause.is::<crate::session::InputModesUnavailable>())
    {
        return ControlError::new("input_modes_unavailable", error.to_string());
    }
    if error
        .get_ref()
        .is_some_and(|cause| cause.is::<crate::remote::client::RemoteTransportFailed>())
    {
        return ControlError::new(
            "remote_transport_failed",
            "Remote transport failed; the Agent's last state is preserved.",
        );
    }
    if let Some(failure) = crate::remote::ssh_error::SshFailure::from_io(&error) {
        return ControlError::new(failure.class.code(), failure.user_message());
    }
    match error.kind() {
        std::io::ErrorKind::NotFound => ControlError::not_found(error.to_string()),
        _ => ControlError::internal(error.to_string()),
    }
}

fn agent_unavailable(kind: &str, host: Option<&str>, detail: Option<&str>) -> ControlError {
    let target = host.map_or_else(|| "this Mac".to_owned(), ToOwned::to_owned);
    let suffix = detail.map_or_else(String::new, |detail| format!(": {detail}"));
    ControlError::new(
        "agent_unavailable",
        format!(
            "{kind} is not available on {target}; detect it or bind an executable in Settings > Agents{suffix}"
        ),
    )
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
    appearance: Option<diri_proto::TerminalAppearance>,
    prompt: Option<&str>,
) -> Result<(), InitialPromptFailure> {
    if accept_claude_workspace {
        accept_claude_workspace_trust(registry, session_id, appearance);
    }
    if let Some(prompt) = prompt {
        let gemini = with_session(registry, session_id, |session| {
            session.manifest_id() == diri_proto::AgentKind::GEMINI_ID
        })
        .unwrap_or(false);
        if gemini {
            accept_gemini_folder_trust(registry, session_id);
        }
        let pi = with_session(registry, session_id, |session| {
            session.manifest_id() == "pi"
        })
        .unwrap_or(false);
        if pi {
            accept_pi_project_trust(registry, session_id);
        }
        let grok = with_session(registry, session_id, |session| {
            session.manifest_id() == "grok"
        })
        .unwrap_or(false);
        if grok {
            wait_for_grok_composer(registry, session_id)?;
        }
        let kimi = with_session(registry, session_id, |session| {
            session.manifest_id() == "kimi"
        })
        .unwrap_or(false);
        if kimi {
            accept_kimi_workspace_trust(registry, session_id);
        }
        let copilot = with_session(registry, session_id, |session| {
            session.manifest_id() == "copilot"
        })
        .unwrap_or(false);
        if copilot {
            accept_copilot_folder_trust(registry, session_id);
        }
        let cursor = with_session(registry, session_id, |session| {
            session.manifest_id() == diri_proto::AgentKind::CURSOR_ID
        })
        .unwrap_or(false);
        if cursor {
            prepare_cursor_input(registry, session_id)?;
        }
        inject_initial_prompt(registry, session_id, prompt)?;
    }
    Ok(())
}

/// Grok paints a composer-shaped placeholder on its unauthenticated welcome
/// screen. Pasting there drops the prompt; Enter starts login, and the fresh
/// NeedsInput evidence can falsely confirm delivery. Only its interactive
/// footer or authenticated home menu proves readiness. Never answer login for a user.
fn wait_for_grok_composer(
    registry: &Arc<Mutex<Registry>>,
    session_id: &str,
) -> Result<(), InitialPromptFailure> {
    for _ in 0..200 {
        let text = screen_text(registry, session_id).ok_or(InitialPromptFailure::SessionEnded)?;
        let bottom = text
            .lines()
            .rev()
            .filter(|line| !line.trim().is_empty())
            .take(8)
            .collect::<Vec<_>>();
        if bottom
            .iter()
            .any(|line| line.split_whitespace().eq(["Login", "with", "Grok", "l"]))
            && bottom
                .iter()
                .any(|line| line.split_whitespace().eq(["Quit", "q"]))
        {
            return Err(InitialPromptFailure::SubmissionUnconfirmed);
        }
        let authenticated_home = text
            .lines()
            .any(|line| line.contains("New worktree") && line.contains("ctrl+w"))
            && text
                .lines()
                .any(|line| line.contains("Resume session") && line.contains("ctrl+r"));
        if authenticated_home
            || bottom.iter().any(|line| {
                let line = line.to_ascii_lowercase();
                line.contains("ctrl+x:shortcuts") || line.contains("ctrl+.:shortcuts")
            })
        {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Err(InitialPromptFailure::SubmissionUnconfirmed)
}

#[derive(Clone, Copy, Debug)]
enum InitialPromptFailure {
    SessionEnded,
    SubmissionUnconfirmed,
    InputFailed,
}

impl std::fmt::Display for InitialPromptFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SessionEnded => formatter.write_str("the session ended before accepting it"),
            Self::InputFailed => formatter.write_str("the session could not accept input"),
            Self::SubmissionUnconfirmed => {
                formatter.write_str("the agent never confirmed that it submitted")
            }
        }
    }
}

fn initial_prompt_control_error(session_id: &str, failure: InitialPromptFailure) -> ControlError {
    ControlError::new(
        "initial_prompt_delivery_failed",
        format!(
            "session {session_id} was created, but initial prompt delivery was not confirmed: {failure}. Input may already have reached the agent; do not resend or spawn a replacement. Inspect this session first."
        ),
    )
}

/// Answers Claude Code's "do you trust this folder?" picker on the user's
/// behalf so a spawn does not stall behind it and swallow the initial prompt.
///
/// This is a deliberate trade: it auto-grants workspace trust for whatever
/// directory the session was pointed at. That is defensible when the user
/// picked the directory in the UI, and weaker when they did not — an
/// orchestrator spawning into a freshly cloned repository gets trust without
/// anyone affirming it. The window is bounded (20s, stretched to at most 30
/// minutes while Claude's own first-run screens are up), but a session whose
/// own output contains the matched phrases inside that window would also
/// receive the keystroke.
///
/// The same watch answers the first run's "Choose the text style" question
/// when the client said whether its window is light or dark: Diri's terminal
/// cannot answer Claude's background-color query, so "Auto" would guess, and
/// a newcomer should not have to pick a palette before they have typed
/// anything. Sign-in and the safety notes are always left to the user.
///
/// The watch ends as soon as a Claude hook reports: Claude runs no hooks
/// until the workspace is trusted, so a hook proves the picker is not coming.
/// Without that exit an already-trusted folder — the common case — held a
/// spawn's initial prompt, and the spawn RPC with it, for the full 20s.
///
/// The answer is navigated, never typed blind. Claude 2.1 lists an
/// unnumbered "No, exit" first and focused; the old answer, "1" and Enter,
/// confirmed that and every first launch in a new folder exited with code 1.
/// Enter is only ever pressed with the focus on "Yes, I trust this folder".
///
/// Claude drops keys that arrive just as the picker mounts, so each key is
/// judged by the screen it leaves: an arrow that did not move the focus, or
/// an Enter that did not close the picker, is pressed again, within a budget.
fn accept_claude_workspace_trust(
    registry: &Arc<Mutex<Registry>>,
    session_id: &str,
    appearance: Option<diri_proto::TerminalAppearance>,
) {
    let started = Instant::now();
    let mut deadline = started + CLAUDE_TRUST_WATCH;
    let mut keys = 0;
    let mut seen = false;
    let mut theme_keys = 0;
    let mut theme_seen = false;
    while Instant::now() < deadline {
        let Some((exited, lines, hooked)) = with_session(registry, session_id, |session| {
            let view = session.view();
            (
                view.exited,
                session.screen_lines(),
                view.status_evidence.is_some_and(|evidence| {
                    evidence.source == diri_proto::StatusEvidenceSource::Hook
                }),
            )
        }) else {
            return;
        };
        if exited {
            return;
        }
        // A first run puts its welcome, theme, sign-in and safety screens
        // ahead of the picker, and signing in takes as long as the browser
        // does. The watch outlasts them, or a newcomer meets the picker
        // after it gave up, with "No, exit" focused.
        if claude_first_run_screen(&lines) {
            deadline = (Instant::now() + CLAUDE_TRUST_WATCH).min(started + CLAUDE_FIRST_RUN_LIMIT);
        }
        if let Some(appearance) = appearance
            && theme_keys < CLAUDE_TRUST_MAX_KEYS
            && let Some(key) = claude_theme_key(&lines, appearance)
        {
            if !theme_seen {
                theme_seen = true;
                std::thread::sleep(CLAUDE_TRUST_SETTLE);
                continue;
            }
            press_claude_key(registry, session_id, key);
            theme_keys += 1;
            let answered = wait_for_claude_screen_change(registry, session_id, 20, |lines| {
                claude_theme_key(lines, appearance) != Some(key)
            });
            if key == ClaudeTrustKey::Confirm && answered {
                diri_telemetry::event!(
                    "prompt.first_run_theme_answered",
                    session = diri_telemetry::id(session_id),
                    keys = theme_keys,
                );
            }
            continue;
        }
        let Some(key) = claude_workspace_trust_key(&lines) else {
            if hooked {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
            continue;
        };
        if !seen {
            seen = true;
            std::thread::sleep(CLAUDE_TRUST_SETTLE);
            continue;
        }
        if keys >= CLAUDE_TRUST_MAX_KEYS {
            // The picker will not take the answer: leave it to the user.
            diri_telemetry::warn_event!(
                "prompt.workspace_trust_unanswered",
                session = diri_telemetry::id(session_id),
                keys = keys,
            );
            return;
        }
        press_claude_key(registry, session_id, key);
        keys += 1;
        // Decide again only once the screen has answered, so a slow repaint
        // never earns a second arrow past "Yes". After Enter this also lets
        // Claude persist trust and replace the picker before a caller's
        // initial prompt starts its own readiness loop.
        let answered = wait_for_claude_screen_change(registry, session_id, 20, |lines| {
            claude_workspace_trust_key(lines) != Some(key)
        });
        if key == ClaudeTrustKey::Confirm && answered {
            diri_telemetry::event!(
                "prompt.workspace_trust_accepted",
                session = diri_telemetry::id(session_id),
                keys = keys,
            );
            return;
        }
    }
}

fn press_claude_key(registry: &Arc<Mutex<Registry>>, session_id: &str, key: ClaudeTrustKey) {
    let bytes: &[u8] = match key {
        ClaudeTrustKey::Confirm => b"\r",
        ClaudeTrustKey::Down => b"\x1b[B",
        ClaudeTrustKey::Up => b"\x1b[A",
    };
    let _ = with_session(registry, session_id, |session| session.write_input(bytes));
}

/// How long the trust watch waits for the picker once nothing else is
/// on screen.
const CLAUDE_TRUST_WATCH: Duration = Duration::from_secs(20);

/// The longest a first run (sign-in included) may hold the trust watch open.
const CLAUDE_FIRST_RUN_LIMIT: Duration = Duration::from_secs(30 * 60);

/// Whether Claude is showing one of the screens of its first run, which come
/// before the workspace-trust picker.
fn claude_first_run_screen(lines: &[String]) -> bool {
    const MARKERS: &[&str] = &[
        "choose the text style",
        "select login method",
        "paste code here",
        "browser didn't open",
        "login successful",
        "security notes:",
        "use claude code's terminal setup",
        "detected a custom api key",
    ];
    crate::detect::bottom_non_empty(lines, 40)
        .iter()
        .any(|line| {
            let line = line.to_ascii_lowercase();
            MARKERS.iter().any(|marker| line.contains(marker))
        })
}

/// The key that moves Claude's first-run text-style picker to the plain
/// light or dark style, or `None` when that picker is not showing.
fn claude_theme_key(
    lines: &[String],
    appearance: diri_proto::TerminalAppearance,
) -> Option<ClaudeTrustKey> {
    let bottom = crate::detect::bottom_non_empty(lines, 30);
    if !bottom
        .iter()
        .any(|line| line.to_ascii_lowercase().contains("choose the text style"))
    {
        return None;
    }
    let wanted = match appearance {
        diri_proto::TerminalAppearance::Light => "light mode",
        diri_proto::TerminalAppearance::Dark => "dark mode",
    };
    // "❯ ✔ Dark mode", "  Light mode", or numbered "2. Light mode".
    let option = |line: &str| {
        let line = line.trim_start();
        let focused = line.starts_with('❯');
        let label = line
            .trim_start_matches('❯')
            .trim_start()
            .trim_start_matches('✔')
            .trim_start();
        let label = label
            .split_once(". ")
            .filter(|(number, _)| number.chars().all(|c| c.is_ascii_digit()))
            .map_or(label, |(_, rest)| rest);
        (focused, label.trim_end().to_ascii_lowercase())
    };
    let target = bottom.iter().position(|line| option(line).1 == wanted)?;
    let focus = bottom.iter().position(|line| option(line).0)?;
    Some(match focus.cmp(&target) {
        std::cmp::Ordering::Equal => ClaudeTrustKey::Confirm,
        std::cmp::Ordering::Less => ClaudeTrustKey::Down,
        std::cmp::Ordering::Greater => ClaudeTrustKey::Up,
    })
}

/// How long the picker stands before the first key, which Claude would
/// otherwise drop while it is still mounting.
const CLAUDE_TRUST_SETTLE: Duration = Duration::from_millis(300);

/// Keys, arrows and Enters together, the trust watch may press before it
/// gives the picker to the user.
const CLAUDE_TRUST_MAX_KEYS: usize = 6;

/// What accepts Claude's workspace-trust picker from where its `❯` focus is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ClaudeTrustKey {
    /// The focus is on "Yes, I trust this folder".
    Confirm,
    Down,
    Up,
}

/// The key that moves Claude's trust picker toward "Yes", or `None` when the
/// picker — both of its options at the bottom of the screen, one focused —
/// is not showing.
fn claude_workspace_trust_key(lines: &[String]) -> Option<ClaudeTrustKey> {
    let bottom = crate::detect::bottom_non_empty(lines, 12);
    let find = |needle: &str| {
        bottom
            .iter()
            .position(|line| line.to_ascii_lowercase().contains(needle))
    };
    let yes = find("yes, i trust this folder")?;
    let no = find("no, exit")?;
    let focused = |index: usize| bottom[index].trim_start().starts_with('❯');
    if focused(yes) {
        Some(ClaudeTrustKey::Confirm)
    } else if focused(no) {
        Some(if yes > no {
            ClaudeTrustKey::Down
        } else {
            ClaudeTrustKey::Up
        })
    } else {
        None
    }
}

/// Whether the screen moved past what `unchanged` describes within
/// `ticks` × 100 ms.
fn wait_for_claude_screen_change(
    registry: &Arc<Mutex<Registry>>,
    session_id: &str,
    ticks: usize,
    changed: impl Fn(&[String]) -> bool,
) -> bool {
    for _ in 0..ticks {
        std::thread::sleep(Duration::from_millis(100));
        let moved = with_session(registry, session_id, |session| {
            changed(&session.screen_lines())
        })
        .unwrap_or(true);
        if moved {
            return true;
        }
    }
    false
}

/// Answers Gemini CLI's "Do you trust the files in this folder?" dialog when
/// a spawn carries an initial prompt, then waits out the restart Gemini does
/// to apply trust.
///
/// Before this the injector's paste-then-Enter answered the dialog by
/// accident: Enter picks the preselected "Trust folder", Gemini restarts, and
/// the prompt died with the old process while the spawn reported success. The
/// trade is the one [`accept_claude_workspace_trust`] documents, and it is no
/// wider than the accidental Enter was. Without a prompt the dialog is left to
/// the user, where the manifest reports it as a question.
///
/// A trusted folder costs about a second: the composer has to stand alone
/// long enough to rule out the dialog Gemini opens just after it. Capped at
/// 20s either way.
fn accept_gemini_folder_trust(registry: &Arc<Mutex<Registry>>, session_id: &str) {
    let mut accepted_at: Option<Instant> = None;
    let mut composer_since: Option<Instant> = None;
    for _ in 0..200 {
        let Some((exited, screen)) = with_session(registry, session_id, |session| {
            (session.view().exited, session.screen_lines())
        }) else {
            return;
        };
        if exited {
            return;
        }
        if is_gemini_folder_trust_screen(&screen) {
            composer_since = None;
            if accepted_at.is_none() {
                diri_telemetry::event!(
                    "prompt.workspace_trust_accepted",
                    session = diri_telemetry::id(session_id),
                );
                // Enter on the preselected "1. Trust folder": a digit would
                // only move Gemini's selection.
                let _ = with_session(registry, session_id, |session| session.submit_input());
                accepted_at = Some(Instant::now());
            }
        } else if is_gemini_composer_screen(&screen) {
            // The outgoing process can repaint its composer before it
            // restarts, and a prompt typed into it dies with it. After an
            // accept, only a composer drawn below the restart notice is the
            // new process's; the time bound covers a Gemini that applies
            // trust without restarting.
            //
            // Before any accept, Gemini paints the composer first and opens
            // the dialog ~100 ms later, so the composer only proves a
            // trusted folder once it has stood alone for a moment.
            let since = *composer_since.get_or_insert_with(Instant::now);
            let ready = match accepted_at {
                None => since.elapsed() >= GEMINI_TRUST_QUIET,
                Some(accepted) => {
                    gemini_restarted_below_notice(&screen)
                        || accepted.elapsed() > Duration::from_secs(5)
                }
            };
            if ready {
                return;
            }
        } else {
            composer_since = None;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// How long Gemini's composer must stand with no trust dialog before a
/// folder counts as already trusted.
const GEMINI_TRUST_QUIET: Duration = Duration::from_secs(1);

/// True when Gemini's composer appears after its "restarting to apply the
/// trust changes" notice, i.e. the relaunched process has drawn its UI.
fn gemini_restarted_below_notice(lines: &[String]) -> bool {
    let notice = lines
        .iter()
        .rposition(|line| line.contains("restarting to apply the trust changes"));
    let composer = lines.iter().rposition(|line| {
        line.to_lowercase()
            .contains("type your message or @path/to/file")
    });
    matches!((notice, composer), (Some(notice), Some(composer)) if composer > notice)
}

/// Gemini's trust dialog, anchored to its option lines at the bottom of the
/// screen: the question itself stays in view above the restarted UI.
fn is_gemini_folder_trust_screen(lines: &[String]) -> bool {
    let bottom = crate::detect::bottom_non_empty(lines, 8)
        .join("\n")
        .to_lowercase();
    bottom.contains("1. trust folder") && bottom.contains("don't trust")
}

fn is_gemini_composer_screen(lines: &[String]) -> bool {
    crate::detect::bottom_non_empty(lines, 8)
        .join("\n")
        .to_lowercase()
        .contains("type your message or @path/to/file")
}

/// Answers Pi's startup "Trust project folder?" selector when a spawn carries
/// an initial prompt, then waits for the composer Pi draws once startup goes
/// on.
///
/// Pi asks before its interactive UI exists, whenever the folder holds
/// project resources (`.pi/settings.json`, `.pi/extensions`, ...). The
/// injector's paste went into the selector, which drops it, and its blind
/// Enter picked the preselected "Trust" and saved it: the folder ended up
/// trusted anyway and the prompt was gone. The trade is the one
/// [`accept_claude_workspace_trust`] documents, and it is no wider than the
/// accidental Enter was. Without a prompt the selector is left to the user,
/// where the manifest reports it as a question.
///
/// The composer only ever follows the selector, so it ends the wait as soon
/// as it shows. Capped at 20s.
fn accept_pi_project_trust(registry: &Arc<Mutex<Registry>>, session_id: &str) {
    let mut accepted = false;
    for _ in 0..200 {
        let Some((exited, screen)) = with_session(registry, session_id, |session| {
            (session.view().exited, session.screen_lines())
        }) else {
            return;
        };
        if exited {
            return;
        }
        if is_pi_project_trust_screen(&screen) {
            if !accepted {
                diri_telemetry::event!(
                    "prompt.workspace_trust_accepted",
                    session = diri_telemetry::id(session_id),
                );
                // Enter on the preselected "→ Trust".
                let _ = with_session(registry, session_id, |session| session.submit_input());
                accepted = true;
            }
        } else if is_pi_composer_screen(&screen) {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Pi's trust selector, anchored to its key hint at the bottom of the screen.
fn is_pi_project_trust_screen(lines: &[String]) -> bool {
    let bottom = crate::detect::bottom_non_empty(lines, 16);
    let hint = bottom
        .iter()
        .rev()
        .take(3)
        .any(|line| line.contains("↑↓ navigate"));
    hint && bottom
        .iter()
        .any(|line| line.to_lowercase().contains("trust project folder?"))
}

/// Pi's composer: two bare full-width rules (above and below the input)
/// directly over its two-line footer.
fn is_pi_composer_screen(lines: &[String]) -> bool {
    let bottom = crate::detect::bottom_non_empty(lines, 6);
    let rules = bottom
        .iter()
        .filter(|line| {
            let line = line.trim();
            line.chars().count() >= 10 && line.chars().all(|c| c == '─')
        })
        .count();
    rules >= 2 && !bottom.iter().any(|line| line.contains("↑↓ navigate"))
}

/// Kimi 2.x gates every new workspace before creating its first session.
/// Pasting into that selector drops the prompt, then the injector's Enter
/// accepts trust anyway. Handle trust explicitly before delivering the prompt,
/// as for Pi/Gemini. Without an initial prompt leave this choice to the user.
fn accept_kimi_workspace_trust(registry: &Arc<Mutex<Registry>>, session_id: &str) {
    let mut accepted = false;
    for _ in 0..200 {
        let Some((exited, screen)) = with_session(registry, session_id, |session| {
            (session.view().exited, session.screen_lines())
        }) else {
            return;
        };
        if exited {
            return;
        }
        if is_kimi_workspace_trust_screen(&screen) {
            if !accepted {
                let _ = with_session(registry, session_id, |session| session.submit_input());
                accepted = true;
            }
        } else if crate::detect::bottom_non_empty(&screen, 5)
            .iter()
            .any(|line| line.trim_start().starts_with("│ >"))
        {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn is_kimi_workspace_trust_screen(lines: &[String]) -> bool {
    let bottom = crate::detect::bottom_non_empty(lines, 16);
    bottom
        .iter()
        .any(|line| line.contains("Trust this folder?"))
        && bottom
            .iter()
            .any(|line| line.contains("↑↓ navigate · Enter select · Esc exit"))
        && !crate::detect::bottom_non_empty(lines, 5)
            .iter()
            .any(|line| line.trim_start().starts_with("│ >"))
}

/// Copilot's folder selector drops pasted text; a blind Enter then accepts
/// trust with the initial prompt lost. As with Gemini/Pi, explicitly accept
/// the one-session "Yes" before injecting, only when a prompt was requested.
/// Do not persist trust or answer any other dialog. Without a prompt the
/// selector remains visible and the manifest reports needs-input.
fn accept_copilot_folder_trust(registry: &Arc<Mutex<Registry>>, session_id: &str) {
    let mut accepted = false;
    let mut composer_since: Option<Instant> = None;
    for _ in 0..200 {
        let Some((exited, screen)) = with_session(registry, session_id, |session| {
            (session.view().exited, session.screen_lines())
        }) else {
            return;
        };
        if exited {
            return;
        }
        if is_copilot_folder_trust_screen(&screen) {
            composer_since = None;
            if !accepted {
                diri_telemetry::event!(
                    "prompt.workspace_trust_accepted",
                    session = diri_telemetry::id(session_id),
                );
                let _ = with_session(registry, session_id, |session| session.submit_input());
                accepted = true;
            }
        } else if crate::detect::bottom_non_empty(&screen, 3)
            .iter()
            .any(|line| line.contains("/ commands") && line.contains("? help"))
        {
            // Folder trust is checked asynchronously after the UI mounts.
            let since = *composer_since.get_or_insert_with(Instant::now);
            if since.elapsed() >= Duration::from_secs(1) {
                return;
            }
        } else {
            composer_since = None;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn is_copilot_folder_trust_screen(lines: &[String]) -> bool {
    let bottom = crate::detect::bottom_non_empty(lines, 20).join("\n");
    bottom.contains("Confirm folder trust")
        && bottom.contains("Do you trust the files in this folder?")
        && bottom.contains("❯ 1. Yes")
        && crate::detect::bottom_non_empty(lines, 3)
            .iter()
            .any(|line| line.contains("enter to select") && line.contains("esc to cancel"))
}

/// Cursor asks for workspace trust before creating its composer. Pasting into
/// that selector drops the prompt, and the injector's later Enter accepts trust.
/// Answer that specific selector first, with the same workspace-trust tradeoff
/// as Claude/Gemini/Pi. Never send an initial prompt to onboarding: any byte there
/// starts browser login. Wait for the user to finish it, or fail unconfirmed.
fn prepare_cursor_input(
    registry: &Arc<Mutex<Registry>>,
    session_id: &str,
) -> Result<(), InitialPromptFailure> {
    let mut accepted = false;
    for _ in 0..200 {
        let (exited, lines) = with_session(registry, session_id, |session| {
            (session.view().exited, session.screen_lines())
        })
        .ok_or(InitialPromptFailure::SessionEnded)?;
        if exited {
            return Err(InitialPromptFailure::SessionEnded);
        }
        if is_cursor_workspace_trust_screen(&lines) {
            if !accepted {
                with_session(registry, session_id, |session| {
                    session.send_text("a", false)
                })
                .ok_or(InitialPromptFailure::SessionEnded)?
                .map_err(|_| InitialPromptFailure::InputFailed)?;
                accepted = true;
            }
        } else if crate::detect::bottom_non_empty(&lines, 8)
            .iter()
            .any(|line| {
                let line = line.trim();
                line.starts_with("→ Plan, search, build anything")
                    || line.starts_with("→ Add a follow-up")
            })
        {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Err(InitialPromptFailure::SubmissionUnconfirmed)
}

fn is_cursor_workspace_trust_screen(lines: &[String]) -> bool {
    let bottom = crate::detect::bottom_non_empty(lines, 8).join("\n");
    bottom.contains("[a] Trust this workspace")
        && bottom.contains("[q] Quit")
        && bottom.contains("Use arrow keys to navigate, Enter to select, or press the key shown")
}

/// Types and submits an initial prompt at most once. Screen observations can
/// confirm acceptance, but an absent echo cannot prove that input was lost.
/// Never clear/retype or send another Enter after an ambiguous outcome.
fn inject_initial_prompt(
    registry: &Arc<Mutex<Registry>>,
    session_id: &str,
    prompt: &str,
) -> Result<(), InitialPromptFailure> {
    let started = Instant::now();
    let mut delivery = "echo_verified";
    let result = deliver_initial_prompt(registry, session_id, prompt, &mut delivery);
    match result {
        Ok(()) => diri_telemetry::event!(
            "prompt.delivered",
            session = diri_telemetry::id(session_id),
            delivery = delivery,
            chars = prompt.chars().count(),
            ms = started.elapsed(),
        ),
        Err(failure) => diri_telemetry::error_event!(
            "prompt.delivery_failed",
            session = diri_telemetry::id(session_id),
            delivery = delivery,
            reason = match failure {
                InitialPromptFailure::SessionEnded => "session_ended",
                InitialPromptFailure::SubmissionUnconfirmed => "submission_unconfirmed",
                InitialPromptFailure::InputFailed => "input_failed",
            },
            chars = prompt.chars().count(),
            ms = started.elapsed(),
        ),
    }
    result
}

/// [`inject_initial_prompt`]'s steps; `delivery` names the path taken.
fn deliver_initial_prompt(
    registry: &Arc<Mutex<Registry>>,
    session_id: &str,
    prompt: &str,
    delivery: &mut &'static str,
) -> Result<(), InitialPromptFailure> {
    if !wait_until_ready(registry, session_id) {
        return Err(InitialPromptFailure::SessionEnded);
    }
    let before = screen_text(registry, session_id).ok_or(InitialPromptFailure::SessionEnded)?;
    let probe = verification_probe(prompt, &before);
    with_session(registry, session_id, |session| session.paste_text(prompt))
        .ok_or(InitialPromptFailure::SessionEnded)?
        .map_err(|_| InitialPromptFailure::InputFailed)?;
    match wait_for_echo(
        registry,
        session_id,
        probe.as_deref(),
        &before,
        ECHO_WINDOW,
        None,
    ) {
        EchoOutcome::Gone => return Err(InitialPromptFailure::SessionEnded),
        EchoOutcome::Visible(shown) => {
            return submit_typed_prompt(registry, session_id, shown.as_deref());
        }
        EchoOutcome::Missing => *delivery = "blind_enter",
    }
    // A line-mode reader may not display anything until Enter. Send it once;
    // a missing response leaves an unknown outcome, never permission to retry.
    let submitted_at = diri_proto::DateMillis::from(std::time::SystemTime::now());
    with_session(registry, session_id, |session| session.submit_input())
        .ok_or(InitialPromptFailure::SessionEnded)?
        .map_err(|_| InitialPromptFailure::InputFailed)?;
    match wait_for_echo(
        registry,
        session_id,
        probe.as_deref(),
        &before,
        LANDED_WINDOW,
        Some(submitted_at),
    ) {
        EchoOutcome::Gone => Err(InitialPromptFailure::SessionEnded),
        EchoOutcome::Visible(_) => Ok(()),
        EchoOutcome::Missing => Err(InitialPromptFailure::SubmissionUnconfirmed),
    }
}

/// What the screen said about a prompt we just typed.
enum EchoOutcome {
    /// The prompt is visibly sitting in the composer: safe to submit. Carries
    /// the text that proved it — the probe, or the placeholder a TUI shows in
    /// place of a long paste — for the submission check to watch.
    Visible(Option<String>),
    /// No echo was observed; acceptance is unknown.
    Missing,
    /// The session exited or vanished — stop touching it.
    Gone,
}

/// How long to watch for the prompt to echo back as it is typed, and how long
/// to watch for it after submitting. The first is short because a TUI that
/// renders its composer does so immediately; the second is longer because it
/// covers a round trip through the agent, which Codex holds back until all
/// of its MCP servers have started. Both end early on confirmation, so the
/// long window only delays reporting an outcome that stays unknown.
const ECHO_WINDOW: Duration = Duration::from_millis(1500);
const LANDED_WINDOW: Duration = Duration::from_secs(10);

/// Polls for the typed prompt to appear on screen. With no usable probe —
/// every word of the prompt was already on screen — any change from `before`
/// is taken as the echo, which is the best signal available in that case. A
/// paste placeholder that was not on screen before also counts: Claude and
/// Codex show one instead of a long paste, so no word of it ever appears.
/// With `submitted_at`, fresh Agent evidence after that Enter counts too.
fn wait_for_echo(
    registry: &Arc<Mutex<Registry>>,
    session_id: &str,
    probe: Option<&str>,
    before: &str,
    window: Duration,
    submitted_at: Option<diri_proto::DateMillis>,
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
            return EchoOutcome::Visible(probe.map(str::to_owned));
        }
        if let Some(placeholder) = new_paste_placeholder(before, &now) {
            return EchoOutcome::Visible(Some(placeholder));
        }
        if submitted_at.is_some_and(|at| agent_started_working(registry, session_id, at)) {
            return EchoOutcome::Visible(None);
        }
    }
    EchoOutcome::Missing
}

/// Claude Code shows `[Pasted text #1 +6 lines]` and Codex `[Pasted Content
/// 1234 chars]` in place of a long paste. The first such marker on `now` that
/// `before` lacked is the composer's echo of our paste.
fn new_paste_placeholder(before: &str, now: &str) -> Option<String> {
    static PLACEHOLDER: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"\[Pasted (?:text #\d+(?: \+\d+ lines)?|Content \d+ chars)\]")
            .expect("paste placeholder pattern compiles")
    });
    PLACEHOLDER
        .find_iter(now)
        .map(|found| found.as_str())
        .find(|placeholder| !before.contains(placeholder))
        .map(str::to_owned)
}

/// Presses Enter on a prompt already verified to be in the composer, and
/// observes whether the composer let go of it. A delayed repaint must never
/// cause an extra Enter: it could submit a queued turn or answer a dialog.
fn submit_typed_prompt(
    registry: &Arc<Mutex<Registry>>,
    session_id: &str,
    probe: Option<&str>,
) -> Result<(), InitialPromptFailure> {
    let submitted_at = diri_proto::DateMillis::from(std::time::SystemTime::now());
    let (before, process_only) = with_session(registry, session_id, |session| {
        let view = session.view();
        (
            session.screen_lines().join("\n"),
            view.status_evidence.is_some_and(|evidence| {
                evidence.fallback_reason == Some(diri_proto::StatusFallbackReason::ProcessOnly)
            }),
        )
    })
    .ok_or(InitialPromptFailure::SessionEnded)?;
    let composer_had_prompt = probe.is_some_and(|probe| {
        composer_text(&before).is_some_and(|composer| composer.contains(probe))
    });
    with_session(registry, session_id, |session| session.submit_input())
        .ok_or(InitialPromptFailure::SessionEnded)?
        .map_err(|_| InitialPromptFailure::InputFailed)?;
    // The prompt may remain in the transcript after submission. In that
    // case require a fresh, authoritative Agent signal; startup output or
    // a status that predates Enter cannot acknowledge these bytes.
    for _ in 0..LANDED_WINDOW.as_millis() / 100 {
        std::thread::sleep(Duration::from_millis(100));
        match screen_text(registry, session_id) {
            None => return Err(InitialPromptFailure::SessionEnded),
            Some(now)
                if probe.is_some_and(|probe| !now.contains(probe))
                    // Codex keeps the submitted text in the transcript.
                    // Require a composer that held our probe before Enter
                    // and is still identifiable but no longer holds it.
                    // An absent composer during a repaint proves nothing.
                    || (composer_had_prompt && probe.is_some_and(|probe| {
                        composer_text(&now).is_some_and(|composer| !composer.contains(probe))
                    }))
                    // Plain CLI tools have no Agent status signals. A new
                    // response after Enter is their available confirmation;
                    // the pasted echo alone must never count as one.
                    || ((process_only || probe.is_none()) && now != before)
                    || agent_started_working(registry, session_id, submitted_at) =>
            {
                return Ok(());
            }
            Some(_) => {}
        }
    }
    Err(InitialPromptFailure::SubmissionUnconfirmed)
}

fn composer_text(screen: &str) -> Option<String> {
    let lines: Vec<String> = screen.lines().map(str::to_owned).collect();
    let body = crate::detect::prompt_box_body(&lines);
    (!body.is_empty()).then(|| body.join("\n"))
}

/// Only fresh Agent evidence can acknowledge submission. A running process
/// or a pre-existing startup/permission status says nothing about this Enter.
fn agent_started_working(
    registry: &Arc<Mutex<Registry>>,
    session_id: &str,
    submitted_at: diri_proto::DateMillis,
) -> bool {
    with_session(registry, session_id, |session| {
        let view = session.view();
        view.status_evidence.is_some_and(|evidence| {
            evidence.status == view.status
                && evidence.signal_at.0 >= submitted_at.0
                && matches!(
                    evidence.source,
                    diri_proto::StatusEvidenceSource::Hook
                        | diri_proto::StatusEvidenceSource::Notify
                        | diri_proto::StatusEvidenceSource::ScreenRule
                )
                && matches!(
                    view.status,
                    diri_proto::SessionStatus::Working | diri_proto::SessionStatus::NeedsInput(_)
                )
        })
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
        if paste && !text.trim().is_empty() {
            // Paste mode says the input line exists; it does NOT say the TUI
            // has stopped repainting over it. Claude Code turns paste mode on
            // while its banner and tips panel are still landing, and anything
            // typed into that window is discarded. Wait for the screen to
            // hold still before treating the composer as real. OpenCode
            // turns paste mode on before its first paint: a blank screen is
            // not a composer, however still it holds.
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
/// split across two screen lines while a single word survives intact. Prefer
/// the earliest line that offers a usable word because Codex collapses long
/// pasted prompts to a leading summary; a probe from the middle can disappear
/// even though the composer accepted the paste. The word also has to be absent
/// from `before`, or a word the banner already displays would read as an echo
/// the instant we looked. `None` when the prompt offers nothing that qualifies
/// — a prompt made entirely of words already on screen, or of words too long
/// to escape wrapping.
fn verification_probe(prompt: &str, before: &str) -> Option<String> {
    prompt.lines().find_map(|line| {
        line.split_whitespace()
            .filter(|word| (MIN_PROBE_CHARS..=MAX_PROBE_CHARS).contains(&word.chars().count()))
            .filter(|word| !before.contains(*word))
            .max_by_key(|word| word.chars().count())
            .map(str::to_owned)
    })
}

/// Short words appear by coincidence; long ones are the ones a narrow
/// composer breaks mid-word.
const MIN_PROBE_CHARS: usize = 4;
const MAX_PROBE_CHARS: usize = 20;

#[cfg(test)]
mod tests {
    use super::*;

    mod agent_relaunch_tests;
    mod find_capture_tests;
    mod reconnect_tests;
    mod send_key_tests;

    #[test]
    fn kimi_trust_requires_the_active_selector() {
        let lines = |text: &str| text.lines().map(str::to_owned).collect::<Vec<_>>();
        let trust = include_str!("../tests/fixtures/kimi_screens/trust.txt");
        let idle = include_str!("../tests/fixtures/kimi_screens/idle.txt");
        assert!(is_kimi_workspace_trust_screen(&lines(trust)));
        assert!(!is_kimi_workspace_trust_screen(&lines(idle)));
        assert!(!is_kimi_workspace_trust_screen(&lines(&format!(
            "{trust}{idle}"
        ))));
    }

    #[test]
    fn telemetry_upload_now_reports_unavailable_without_an_uploader() {
        // Debug builds and tests never start the uploader.
        let value = telemetry_upload_now().unwrap();
        let result: diri_proto::TelemetryUploadNowResult = serde_json::from_value(value).unwrap();
        assert_eq!(result.status, "unavailable");
        assert_eq!(result.batches, 0);
    }

    #[test]
    fn a_shared_event_frame_is_the_line_control_message_writes() {
        let bus = crate::events::EventBus::new();
        let stream = bus.subscribe(None, crate::events::Filter::all());
        let params = json!({ "id": "s_1", "title": "quote \" and \\ and \n 界" });
        bus.publish("session.\"updated\"", params.clone(), Some("s_1"));
        let event = stream.try_recv().expect("event");

        let (mut read, write) = UnixStream::pair().unwrap();
        let writer = Arc::new(Mutex::new(write));
        write_event_frame(&writer, &event).unwrap();
        write_message(
            &writer,
            &ControlMessage::Event {
                name: event.name.clone(),
                seq: event.seq,
                params,
            },
        )
        .unwrap();
        drop(writer);
        let mut output = String::new();
        std::io::Read::read_to_string(&mut read, &mut output).unwrap();
        let lines: Vec<&str> = output.lines().collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], lines[1]);
    }

    #[test]
    fn explicit_launch_argv_is_literal_and_never_silently_repaired() {
        assert!(decode_launch_argv(&json!({})).unwrap().is_empty());
        let arguments = vec!["/bin/echo", "", "a b", "$(touch nope)", "--host", "界"];
        assert_eq!(
            decode_launch_argv(&json!({"argv": arguments})).unwrap(),
            arguments
        );
        for argv in [
            json!(null),
            json!("echo"),
            json!([]),
            json!([""]),
            json!(["echo", 3]),
            json!(["echo", "x\0y"]),
            json!(vec!["x"; diri_proto::remote_pty::MAX_ARGUMENTS + 1]),
            json!(["x".repeat(diri_proto::remote_pty::MAX_LAUNCH_BYTES + 1)]),
        ] {
            assert_eq!(
                decode_launch_argv(&json!({"argv": argv})).unwrap_err().code,
                "bad_request"
            );
        }
    }

    #[test]
    fn failed_remote_state_times_out_exit_wait_and_returns_a_structured_resume_error() {
        let temp = tempfile::tempdir().unwrap();
        let mut registry = Registry::new(engine(), temp.path().join("state.json"));
        let mut record = test_record("failed-remote");
        record.host = Some("fixture".into());
        record.status = diri_proto::SessionStatus::Unknown;
        record.remote_connection = Some(diri_proto::RemoteConnection {
            state: diri_proto::RemoteConnectionState::Failed,
            since: diri_proto::DateMillis(123.0),
        });
        registry.insert_record(record);
        let server = ControlServer::new(Arc::new(Mutex::new(registry)), temp.path().join("socket"));
        let result = server
            .events_wait(Some(serde_json::json!({
                "sessionID":"failed-remote", "until":["exited"], "timeoutMs":0,
            })))
            .unwrap();
        assert_eq!(result["timedOut"], true);
        assert_eq!(result["session"]["remoteConnection"]["state"], "failed");
        let error = server
            .session_resume(Some(serde_json::json!({"sessionID":"failed-remote"})))
            .unwrap_err();
        assert_eq!(error.code, "remote_transport_failed");
        let error = io_control_error(std::io::Error::new(
            std::io::ErrorKind::NotConnected,
            crate::remote::client::RemoteTransportFailed,
        ));
        assert_eq!(error.code, "remote_transport_failed");
    }

    #[test]
    fn process_facts_do_not_wait_for_registry_admission() {
        let temp = tempfile::tempdir().unwrap();
        let server = server(temp.path());
        let _held = server.registry.lock().unwrap();
        assert_eq!(
            server
                .session_process_info(Some(json!({"sessionID":"fixture"})))
                .unwrap_err()
                .code,
            "process_facts_busy"
        );
    }

    #[test]
    fn oversized_control_line_is_rejected_before_unbounded_buffering() {
        let bytes = vec![b'x'; MAX_CONTROL_LINE_BYTES + 1];
        let mut reader = std::io::BufReader::new(bytes.as_slice());
        let error = read_bounded_control_line(&mut reader).expect_err("must reject");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn background_request_capacity_is_bounded_and_released() {
        let counter = Arc::new(AtomicUsize::new(0));
        let permits: Vec<_> = (0..32)
            .map(|_| BackgroundRequest::acquire(&counter).unwrap())
            .collect();
        assert!(BackgroundRequest::acquire(&counter).is_none());
        drop(permits);
        assert_eq!(counter.load(Ordering::Acquire), 0);
        assert!(BackgroundRequest::acquire(&counter).is_some());
    }
    use crate::detect::ManifestEngine;

    fn engine() -> Arc<ManifestEngine> {
        let dir = crate::detect::bundled_manifest_dir()
            .canonicalize()
            .expect("manifests");
        let (engine, _) = ManifestEngine::load_dir(&dir).expect("load");
        Arc::new(engine)
    }

    pub(super) fn server(temp: &Path) -> Arc<ControlServer> {
        let registry = Registry::new(engine(), temp.join("state.json"));
        Arc::new(ControlServer::new(
            Arc::new(Mutex::new(registry)),
            temp.join("daemon.sock"),
        ))
    }

    pub(super) fn test_record(id: &str) -> diri_proto::SessionRecord {
        use diri_proto::*;
        SessionRecord {
            attention_state: None,
            id: SessionId(id.into()),
            kind: AgentKind::SHELL,
            cwd: "/tmp".into(),
            project_id: ProjectId("p".into()),
            worktree_path: None,
            git_branch: None,
            title: "test".into(),
            title_source: TitleSource::Placeholder,
            account_profile: None,
            originating_prompt: None,
            agent_session_id: None,
            transcript_path: None,
            status: SessionStatus::Idle,
            status_evidence: None,
            needs_input: None,
            resumability: Resumability::NotResumable,
            capabilities: None,
            parent: None,
            created_at: DateMillis(0.0),
            updated_at: DateMillis(0.0),
            last_turn_completed_at: None,
            last_seen_at: None,
            pinned: false,
            archived_at: None,
            host: None,
            remote_persistence: None,
            remote_connection: None,
            hibernation: None,
            memory_bytes: None,
            artifacts: None,
            pull_requests: None,
            listening_ports: None,
            foreground_agent: None,
            terminal_cwd: None,
            note_id: None,
            foreground_ports: None,
            terminal_progress: None,
            scheduled_run: None,
        }
    }

    fn repository_with_linked_worktree(temp: &Path) -> (PathBuf, PathBuf) {
        let repo = temp.join("repo");
        let target = temp.join("feature-checkout");
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
        git(&["commit", "--allow-empty", "-q", "-m", "root"]);
        git(&[
            "worktree",
            "add",
            "-q",
            "-b",
            "feature/reparent",
            target.to_str().expect("utf8 target"),
        ]);
        (
            repo.canonicalize().expect("repo"),
            target.canonicalize().expect("target"),
        )
    }

    fn ended_resumable_record(id: &str, repo: &Path) -> diri_proto::SessionRecord {
        let mut record = test_record(id);
        // An Agent's conversation: a terminal typed `exit` is closed for good.
        record.kind = diri_proto::AgentKind::CLAUDE_CODE;
        record.cwd = repo.to_string_lossy().into_owned();
        record.project_id = crate::registry::session_project_id(&record.cwd, None);
        record.status = diri_proto::SessionStatus::Exited(diri_proto::ExitInfo {
            reason: diri_proto::ExitReason::Exited,
            code: Some(0),
            signal: None,
            system_restart: false,
        });
        record.resumability = diri_proto::Resumability::Resumable;
        record
    }

    /// Round-trips one request through the dispatcher the way a client would.
    /// Dispatches one line the way `serve` would, with a throwaway socket
    /// standing in for the connection's write half.
    fn handle(server: &Arc<ControlServer>, line: &[u8]) -> Option<ControlMessage> {
        let (writer, _peer) = UnixStream::pair().expect("socketpair");
        server.handle_line(line, &Arc::new(Mutex::new(writer)), &mut None)
    }

    fn call(
        server: &Arc<ControlServer>,
        method: &str,
        params: Option<JsonValue>,
    ) -> ControlMessage {
        let request = ControlMessage::Request {
            id: 1,
            method: method.into(),
            params,
        };
        let line = serde_json::to_vec(&request).expect("encode");
        let (writer, peer) = UnixStream::pair().expect("socketpair");
        peer.set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        if let Some(response) = server.handle_line(&line, &Arc::new(Mutex::new(writer)), &mut None)
        {
            return response;
        }
        // Slow RPCs now respond on the socket instead of blocking handle_line.
        let mut response = String::new();
        BufReader::new(peer)
            .read_line(&mut response)
            .expect("background response");
        serde_json::from_str(&response).expect("a request gets a response")
    }

    #[test]
    fn note_sessions_have_a_file_and_survive_the_restart_reaper() {
        let temp = tempfile::tempdir().expect("temp");
        let registry = Arc::new(Mutex::new(Registry::new(
            engine(),
            temp.path().join("state.json"),
        )));
        let server = Arc::new(
            ControlServer::new(Arc::clone(&registry), temp.path().join("daemon.sock"))
                .with_notes_dir(temp.path().join("notes")),
        );
        let project = temp.path().join("proj");
        std::fs::create_dir_all(&project).expect("project");
        let record = ok_of(call(
            &server,
            "session.spawn",
            Some(json!({
                "kind": "note",
                "cwd": project.to_string_lossy(),
                "title": "Launch plan",
                "initialPrompt": "- [ ] ship it",
            })),
        ));
        let record: diri_proto::SessionRecord = serde_json::from_value(record).expect("record");
        assert!(record.is_note());
        assert_eq!(record.title, "Launch plan");
        assert!(matches!(record.status, diri_proto::SessionStatus::Idle));
        let note_id = record.note_id.clone().expect("note id");
        let store = diri_notes::store::NoteStore::open(temp.path().join("notes")).expect("store");
        let note = store.load(&note_id).expect("note file");
        assert_eq!(note.doc.title, "Launch plan");
        assert_eq!(note.doc.todo_progress(), (0, 1));
        assert_eq!(note.project(), Some(project.to_string_lossy().as_ref()));

        // Closing the note's tab keeps its file, so search can still find it;
        // reopening brings the tab back.
        ok_of(call(
            &server,
            "session.remove",
            Some(json!({ "sessionID": record.id.0 })),
        ));
        assert!(store.load(&note_id).is_ok(), "a closed note stays findable");
        let reopened = ok_of(call(&server, "session.reopen_last", None));
        assert_eq!(reopened["id"], record.id.0.as_str());
        assert_eq!(store.load(&note_id).unwrap().doc.title, "Launch plan");

        // An Engine that predates notes drops `noteId` from the record and
        // reaps it; the next notes-aware start links it back from the file.
        {
            let mut registry = registry.lock().expect("registry");
            let mut broken = registry.record(&record.id.0).expect("listed");
            broken.note_id = None;
            broken.status = diri_proto::SessionStatus::Exited(diri_proto::ExitInfo {
                reason: diri_proto::ExitReason::DaemonRestart,
                code: None,
                signal: None,
                system_restart: false,
            });
            registry.insert_record(broken);
        }
        server.adopt_orphan_notes().expect("relink");
        let healed = registry
            .lock()
            .expect("registry")
            .record(&record.id.0)
            .expect("listed");
        assert_eq!(
            healed.note_id.as_deref(),
            Some(note_id.as_str()),
            "link restored"
        );
        assert!(matches!(healed.status, diri_proto::SessionStatus::Idle));

        // A daemon restart finds no holder for the note and must not call it lost.
        registry.lock().expect("registry").reap_orphans_for_test();
        let after = registry
            .lock()
            .expect("registry")
            .record(&record.id.0)
            .expect("still listed");
        assert!(matches!(after.status, diri_proto::SessionStatus::Idle));
    }

    /// A note an agent wrote belongs to the user: the agent ending, being
    /// closed, or vanishing in a restart leaves the note and its file alone.
    #[test]
    fn a_note_outlives_the_agent_that_wrote_it() {
        let temp = tempfile::tempdir().expect("temp");
        let (registry, server) = note_server(&temp);
        let project = temp.path().join("proj");
        std::fs::create_dir_all(&project).expect("project");
        let mut agent = new_record("s_agent", "claude-code", &project.to_string_lossy());
        agent.status = diri_proto::SessionStatus::Working;
        registry.lock().expect("registry").insert_record(agent);
        let note = ok_of(call(
            &server,
            "session.spawn",
            Some(json!({
                "kind": "note",
                "cwd": project.to_string_lossy(),
                "title": "Findings",
                "parent": "s_agent",
            })),
        ));
        let note: diri_proto::SessionRecord = serde_json::from_value(note).expect("record");
        let note_id = note.note_id.clone().expect("note id");

        let intact = || {
            let record = registry
                .lock()
                .expect("registry")
                .record(&note.id.0)
                .expect("the note is still listed");
            assert_eq!(record.note_id.as_deref(), Some(note_id.as_str()));
            assert!(matches!(record.status, diri_proto::SessionStatus::Idle));
            let store =
                diri_notes::store::NoteStore::open(temp.path().join("notes")).expect("store");
            assert_eq!(store.load(&note_id).expect("file").doc.title, "Findings");
        };
        // The agent's holder is gone after a restart.
        registry.lock().expect("registry").reap_orphans_for_test();
        intact();
        ok_of(call(
            &server,
            "session.remove",
            Some(json!({ "sessionID": "s_agent" })),
        ));
        intact();
        server.adopt_orphan_notes().expect("adopt");
        intact();
    }

    fn note_server(temp: &tempfile::TempDir) -> (Arc<Mutex<Registry>>, Arc<ControlServer>) {
        let registry = Arc::new(Mutex::new(Registry::new(
            engine(),
            temp.path().join("state.json"),
        )));
        let server = Arc::new(
            ControlServer::new(Arc::clone(&registry), temp.path().join("daemon.sock"))
                .with_notes_dir(temp.path().join("notes")),
        );
        (registry, server)
    }

    /// Attaches over a socket pair and returns everything the Engine wrote
    /// before it closed.
    fn refused_attach(server: &Arc<ControlServer>, id: &str) -> Vec<diri_proto::frames::Frame> {
        let (mut client, engine_end) = UnixStream::pair().expect("pair");
        let serving = {
            let server = Arc::clone(server);
            std::thread::spawn(move || server.serve(engine_end))
        };
        let mut line = serde_json::to_vec(&json!({ "attach": id })).expect("line");
        line.push(b'\n');
        client.write_all(&line).expect("attach line");
        client
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .expect("timeout");
        let mut bytes = Vec::new();
        client.read_to_end(&mut bytes).expect("engine closes");
        serving.join().expect("serve").expect("served");
        diri_proto::frames::FrameCodec::new()
            .feed(&bytes)
            .expect("frames")
    }

    #[test]
    fn a_refused_attach_says_why_before_closing() {
        use diri_proto::frames::AttachRejection;
        let temp = tempfile::tempdir().expect("temp");
        let (_, server) = note_server(&temp);
        let note = ok_of(call(
            &server,
            "session.spawn",
            Some(json!({"kind": "note", "cwd": temp.path().to_string_lossy(), "title": "Plan"})),
        ));
        let note = note["id"].as_str().expect("id").to_owned();

        for (id, reason) in [
            (note.as_str(), AttachRejection::NotTerminal),
            ("s_never_existed", AttachRejection::SessionNotFound),
        ] {
            let frames = refused_attach(&server, id);
            assert_eq!(frames.len(), 1, "{id}: one frame, then EOF");
            assert_eq!(frames[0].attach_rejected_payload(), Some(reason), "{id}");
        }
    }

    #[test]
    fn adopting_a_note_keeps_its_id_and_date_and_is_idempotent() {
        let temp = tempfile::tempdir().expect("temp");
        let (_, server) = note_server(&temp);
        let project = temp.path().join("proj");
        std::fs::create_dir_all(&project).expect("project");
        let store = diri_notes::store::NoteStore::open(temp.path().join("notes")).expect("store");
        let (note_id, _) = store
            .create(
                diri_notes::doc::Document::new("Q3 campaign", Vec::new()),
                Some(&project.to_string_lossy()),
            )
            .expect("note");
        let before = store.meta(&note_id).expect("meta");

        let adopt = || {
            let record = ok_of(call(
                &server,
                "session.spawn",
                Some(json!({"kind": "note", "cwd": "", "noteId": note_id})),
            ));
            serde_json::from_value::<diri_proto::SessionRecord>(record).expect("record")
        };
        let first = adopt();
        assert_eq!(first.note_id.as_deref(), Some(note_id.as_str()));
        assert_eq!(first.title, "Q3 campaign");
        assert_eq!(first.cwd, project.to_string_lossy());
        assert_eq!(first.created_at.0, before.created as f64 * 1000.0);
        let second = adopt();
        assert_eq!(second.id, first.id, "one Session per note");

        let after = store.load(&note_id).expect("note");
        assert_eq!(after.front.get("id"), Some(note_id.as_str()));
        assert_eq!(
            after.front.get("created"),
            before_created(&before).as_deref()
        );
        assert_eq!(after.front.get("session"), Some(first.id.0.as_str()));

        let missing = err_of(call(
            &server,
            "session.spawn",
            Some(json!({"kind": "note", "cwd": "", "noteId": "20000101-000000-dead"})),
        ));
        assert_eq!(missing.code, "not_found");
    }

    fn before_created(meta: &diri_notes::store::NoteMeta) -> Option<String> {
        Some(diri_notes::store::format_timestamp(meta.created))
    }

    #[test]
    fn startup_adopts_only_true_orphans_once() {
        let temp = tempfile::tempdir().expect("temp");
        let (registry, server) = note_server(&temp);
        let project = temp.path().join("proj");
        std::fs::create_dir_all(&project).expect("project");
        let project = project.to_string_lossy().into_owned();
        let store = diri_notes::store::NoteStore::open(temp.path().join("notes")).expect("store");
        let doc = |title: &str| diri_notes::doc::Document::new(title, Vec::new());

        let (offline, _) = store
            .create(doc("Written offline"), Some(&project))
            .unwrap();
        let (inbox, _) = store.create(doc("Loose idea"), None).unwrap();
        let (archived, mut note) = store.create(doc("Old"), Some(&project)).unwrap();
        note.front.set_flag(diri_notes::store::KEY_ARCHIVED, true);
        store.save(&archived, &note).unwrap();
        // Removed on purpose: stamped with a Session that no longer exists.
        let (removed, _) = store
            .create_for_session(
                doc("Removed"),
                Some(&project),
                Some("s_gone"),
                &diri_notes::history::Author::User,
            )
            .unwrap();
        // Already a Session.
        let shown = ok_of(call(
            &server,
            "session.spawn",
            Some(json!({"kind": "note", "cwd": project, "title": "Shown"})),
        ));

        assert_eq!(server.adopt_orphan_notes().expect("scan"), 2);
        assert_eq!(
            server.adopt_orphan_notes().expect("rescan"),
            0,
            "deterministic"
        );

        let notes: Vec<diri_proto::SessionRecord> = registry
            .lock()
            .unwrap()
            .records()
            .into_iter()
            .filter(diri_proto::SessionRecord::is_note)
            .collect();
        let by_note = |id: &str| notes.iter().find(|r| r.note_id.as_deref() == Some(id));
        assert_eq!(notes.len(), 3);
        assert_eq!(by_note(&offline).unwrap().cwd, project);
        let home = std::env::var("HOME").unwrap();
        assert_eq!(
            by_note(&inbox).unwrap().cwd,
            home,
            "Inbox notes live in the home folder"
        );
        assert!(by_note(&archived).is_none());
        assert!(by_note(&removed).is_none());
        assert!(by_note(shown["noteId"].as_str().unwrap()).is_some());
        assert_eq!(
            store.meta(&offline).unwrap().session.as_deref(),
            Some(by_note(&offline).unwrap().id.0.as_str())
        );
    }

    #[test]
    fn tracked_spawns_may_start_from_a_note() {
        let temp = tempfile::tempdir().expect("temp");
        let (_, server) = note_server(&temp);
        let project = temp.path().join("proj");
        std::fs::create_dir_all(&project).expect("project");
        let note = ok_of(call(
            &server,
            "session.spawn",
            Some(json!({"kind": "note", "cwd": project.to_string_lossy(), "title": "PRD"})),
        ));
        let note_session = note["id"].as_str().unwrap().to_owned();
        // A note may not send anything itself, so an agent starts the work.
        let started = ok_of(call(
            &server,
            "session.spawn_tracked",
            Some(json!({
                "senderID": "s_agent",
                "operationID": "op-1",
                "spawn": {"kind": "note", "cwd": project.to_string_lossy(), "title": "Brief", "parent": note_session},
            })),
        ));
        assert_eq!(started["ok"], true);
        assert_eq!(started["parent"], note_session.as_str());

        // Any other parent that is not the sender stays refused.
        let refused = err_of(call(
            &server,
            "session.spawn_tracked",
            Some(json!({
                "senderID": "s_agent",
                "operationID": "op-2",
                "spawn": {"kind": "note", "cwd": project.to_string_lossy(), "parent": "s_someone_else"},
            })),
        ));
        assert_eq!(refused.code, "bad_request");
    }

    #[test]
    fn terminal_requests_on_a_note_fail_fast_for_every_client() {
        let temp = tempfile::tempdir().expect("temp");
        let registry = Arc::new(Mutex::new(Registry::new(
            engine(),
            temp.path().join("state.json"),
        )));
        let server = Arc::new(
            ControlServer::new(Arc::clone(&registry), temp.path().join("daemon.sock"))
                .with_notes_dir(temp.path().join("notes")),
        );
        let project = temp.path().join("proj");
        std::fs::create_dir_all(&project).expect("project");
        let record = ok_of(call(
            &server,
            "session.spawn",
            Some(json!({"kind": "note", "cwd": project.to_string_lossy(), "title": "Plan"})),
        ));
        let id = record["id"].as_str().expect("id").to_owned();
        let started = Instant::now();
        for (method, params) in [
            ("session.send_text", json!({"sessionID": id, "text": "hi"})),
            ("session.send_key", json!({"sessionID": id, "key": "enter"})),
            ("session.read_screen", json!({"sessionID": id})),
            ("session.resume", json!({"sessionID": id})),
            (
                "session.resize",
                json!({"sessionID": id, "cols": 80, "rows": 24}),
            ),
            (
                "session.deliver_message",
                json!({"sessionID": id, "senderID": "s_x", "messageID": "m", "text": "hi"}),
            ),
            (
                "task.submit",
                json!({"caller_id": "s_x", "request_id": "r", "session_id": id, "text": "hi"}),
            ),
        ] {
            let error = err_of(call(&server, method, Some(params)));
            assert_eq!(
                error.code,
                diri_proto::control::SESSION_HAS_NO_TERMINAL,
                "{method}: {error:?}"
            );
        }
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "must not wait on a terminal"
        );
        // Everything else about a note still works.
        ok_of(call(
            &server,
            "session.rename",
            Some(json!({"sessionID": id, "title": "Renamed"})),
        ));
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
    fn account_profiles_list_is_available_to_settings() {
        let temp = tempfile::tempdir().expect("temp");
        let server = server(temp.path());
        let result = ok_of(call(&server, "account.profiles.list", None));
        assert_eq!(result["profiles"], json!([]));
    }

    #[test]
    fn read_screen_serves_a_retained_terminal_for_a_completed_session() {
        use diri_proto::process::{BootId, ProcessBirth, ProcessIdentity};
        let temp = tempfile::tempdir().expect("temp");
        let server = server(temp.path());
        let mut record = test_record("finished");
        let exit = diri_proto::ExitInfo {
            reason: diri_proto::ExitReason::Exited,
            code: Some(0),
            signal: None,
            system_restart: false,
        };
        record.status = diri_proto::SessionStatus::Exited(exit.clone());
        let child = ProcessIdentity::new(
            4321,
            ProcessBirth::Macos {
                boot_session: BootId::parse("0f0e0d0c-0b0a-0908-0706-050403020100").unwrap(),
                start_seconds: 1_700_000_000,
                start_microseconds: 1,
            },
        )
        .unwrap();
        let key = crate::completed_terminal::CompletedRunKey::bind(&record, child, 10).unwrap();
        let mut screen = diri_terminal_state::HeadlessScreen::new(40, 4);
        screen.feed(b"line one\r\nline two\r\nline three\r\nline four\r\nline five");
        let checkpoint = crate::checkpoint::ScreenCheckpoint {
            keyboard_snapshot: screen.keyboard_snapshot(),
            keyboard: Some(screen.keyboard_state()),
            log_offset: 64,
            history: screen.history_snapshot(),
            history_metadata: screen.history_metadata(),
            grid: screen.grid_update(true),
            marker_buffer: Vec::new(),
            alt_screen: false,
            bracketed_paste: false,
            mouse: Default::default(),
        };
        let completed_dir = temp
            .path()
            .join(crate::registry::COMPLETED_TERMINALS_DIR_NAME);
        std::fs::create_dir(&completed_dir).unwrap();
        std::fs::set_permissions(
            &completed_dir,
            std::os::unix::fs::PermissionsExt::from_mode(0o700),
        )
        .unwrap();
        crate::completed_terminal::CompletedTerminalStore::open(&completed_dir)
            .unwrap()
            .publish(&record, &key, &checkpoint, &exit)
            .unwrap();
        {
            let mut registry = server.registry.lock().expect("registry");
            registry.insert_record(record.clone());
            let recovery = registry.recovery_directory("finished");
            std::fs::create_dir_all(&recovery).unwrap();
            diri_proto::recovery::SessionRecoveryStore::new(recovery)
                .write_completed_run(&key)
                .unwrap();
        }

        let result = ok_of(call(
            &server,
            "session.read_screen",
            Some(json!({ "sessionID": "finished" })),
        ));
        assert_eq!(result["cols"], 40);
        assert_eq!(result["rows"], 4);
        assert!(
            result["text"].as_str().unwrap().contains("line five"),
            "{result}"
        );
        let scrollback = ok_of(call(
            &server,
            "session.read_scrollback",
            Some(json!({ "sessionID": "finished" })),
        ));
        let lines: Vec<String> = serde_json::from_value(scrollback["lines"].clone()).unwrap();
        assert!(lines.iter().any(|line| line == "line one"), "{lines:?}");

        let cells = ok_of(call(
            &server,
            "session.read_scrollback_cells",
            Some(json!({ "sessionID": "finished", "firstRow": 0, "maxRows": 10 })),
        ));
        assert!(cells["rowCount"].as_u64().unwrap() > 0, "{cells}");
        let find = ok_of(call(
            &server,
            "session.capture_find",
            Some(json!({ "sessionID": "finished" })),
        ));
        assert!(
            find["owner"].as_str().unwrap().starts_with("completed-"),
            "{find}"
        );
        assert_eq!(find["captureRevision"], 0);
        assert_eq!(find["visibleRows"], 4);

        // Input against a retained terminal is impossible, not silently lost.
        assert!(matches!(
            call(
                &server,
                "session.send_text",
                Some(json!({ "sessionID": "finished", "text": "x", "submit": false })),
            ),
            ControlMessage::Response { result: Err(_), .. }
        ));

        // There is no live emulator behind a retained terminal to reset.
        assert!(matches!(
            call(
                &server,
                "session.reset_terminal",
                Some(json!({ "sessionID": "finished" })),
            ),
            ControlMessage::Response { result: Err(error), .. } if error.code == "terminal_reset_unavailable"
        ));
        assert!(matches!(
            call(
                &server,
                "session.reset_terminal",
                Some(json!({ "sessionID": "never-existed" })),
            ),
            ControlMessage::Response { result: Err(error), .. } if error.code == "not_found"
        ));

        // Another exit than the retained one is a different run: unavailable.
        {
            let mut registry = server.registry.lock().expect("registry");
            let mut other = record.clone();
            other.status = diri_proto::SessionStatus::Exited(diri_proto::ExitInfo {
                reason: diri_proto::ExitReason::DaemonRestart,
                code: None,
                signal: None,
                system_restart: false,
            });
            registry.insert_record(other);
        }
        assert!(matches!(
            call(
                &server,
                "session.read_screen",
                Some(json!({ "sessionID": "finished" })),
            ),
            ControlMessage::Response { result: Err(_), .. }
        ));
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
        assert!(
            result["build"]
                .as_str()
                .is_some_and(|build| build.contains("+catalog.")),
            "manifest changes must alter the daemon identity: {result}"
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
    fn hello_reports_one_engine_instance_identity_per_control_lifetime() {
        let temp = tempfile::tempdir().expect("temp");
        let hello = |server: &Arc<ControlServer>| {
            ok_of(call(
                server,
                "hello",
                Some(json!({ "proto": WIRE_VERSION, "build": "test-client" })),
            ))["engineInstanceId"]
                .as_str()
                .expect("engineInstanceId is a string")
                .to_owned()
        };
        let server = server(temp.path());
        let first = hello(&server);
        assert_eq!(first.len(), 32, "128 random bits as lowercase hex: {first}");
        assert!(first.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_eq!(
            hello(&server),
            first,
            "every Hello from one control server must report the same instance"
        );

        // A replacement Engine with the same executable, build and possibly a
        // reused PID must still look like a different event cursor lifetime.
        let other = tempfile::tempdir().expect("temp");
        assert_ne!(hello(&self::tests::server(other.path())), first);
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
    fn activity_list_reads_the_durable_publication_history() {
        let temp = tempfile::tempdir().expect("temp");
        let server = server(temp.path());
        {
            let mut registry = server.registry.lock().expect("registry");
            let mut record = test_record("s_activity");
            record.kind = diri_proto::AgentKind::CODEX;
            record.status = diri_proto::SessionStatus::Working;
            registry.insert_record(record);
            server.publish_updated(&registry, "s_activity");
            registry.update_record("s_activity", |record| {
                record.status = diri_proto::SessionStatus::Idle;
            });
            server.publish_updated(&registry, "s_activity");
        }

        let result = ok_of(call(
            &server,
            diri_proto::Method::ACTIVITY_LIST,
            Some(json!({"limit": 10})),
        ));
        assert_eq!(result["entries"][0]["kind"], "finished");
        assert_eq!(result["entries"][1]["kind"], "started");
        assert_eq!(result["entries"][0]["sessionID"], "s_activity");
        assert!(
            temp.path()
                .join(diri_proto::paths::ACTIVITY_LOG_FILE_NAME)
                .is_file()
        );
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
        use std::os::unix::fs::PermissionsExt as _;

        let temp = tempfile::tempdir().expect("temp");
        let registry = Registry::new(engine(), temp.path().join("state.json"));
        let server = ControlServer::new(
            Arc::new(Mutex::new(Registry::new(
                engine(),
                temp.path().join("server-state.json"),
            ))),
            temp.path().join("daemon.sock"),
        );
        let executable = temp.path().join("claude");
        std::fs::write(&executable, b"#!/bin/sh\nexit 0\n").expect("fake claude executable");
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700))
            .expect("make fake claude executable");
        server
            .agent_catalog
            .lock()
            .expect("agent catalog lock")
            .configure(
                None,
                "claude-code",
                crate::agent_catalog::AgentPreference {
                    executable_path: Some(executable.to_string_lossy().into_owned()),
                    show_in_quick_create: Some(true),
                },
            )
            .expect("bind fake claude executable");

        let spec = server
            .resume_spec(&registry, "s_resume", "claude-code", "/tmp", Some("uuid-1"))
            .expect("resume spec");
        // Claude declares `returnToLoginShell`, so the agent runs inside the
        // PTY's login shell rather than as its argv[0]; the resume flags still
        // have to reach the agent itself.
        let command = spec.pty.argv.last().expect("argv");
        assert!(
            command.contains(&format!("{}'", executable.display()))
                && command.contains("'--resume' 'uuid-1'"),
            "resume flags must reach the agent: {command:?}"
        );

        let fork = server
            .local_conversation_spec(
                &registry,
                "s_fork",
                "claude-code",
                "/tmp",
                Some("uuid-1"),
                ConversationAction::Fork,
            )
            .expect("fork spec");
        let command = fork.pty.argv.last().expect("fork argv");
        assert!(
            command.contains("'--resume' 'uuid-1' '--fork-session'"),
            "fork grammar must reach the agent: {command:?}"
        );
    }

    #[test]
    fn account_binding_survives_profile_edits_removal_resume_and_fork() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let server = server(temp.path());
        let executable = temp.path().join("codex");
        std::fs::write(
            &executable,
            "#!/bin/sh\nprintf '%s\\n' launch >> \"$CODEX_HOME/launches\"\nexec /bin/sleep 30\n",
        )
        .unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        server
            .agent_catalog
            .lock()
            .unwrap()
            .configure(
                None,
                "codex",
                crate::agent_catalog::AgentPreference {
                    executable_path: Some(executable.to_string_lossy().into_owned()),
                    show_in_quick_create: Some(true),
                },
            )
            .unwrap();
        let profile = diri_proto::AgentAccountProfile {
            id: "work".into(),
            label: "Work".into(),
            agent: "codex".into(),
            host: None,
            config_home: temp
                .path()
                .join("work-account")
                .to_string_lossy()
                .into_owned(),
            is_default: true,
            login_store: None,
        };
        server
            .accounts
            .lock()
            .unwrap()
            .upsert(profile.clone())
            .unwrap();
        let spawned = server.session_spawn(Some(json!({"kind": diri_proto::AgentKind::CODEX, "cwd": temp.path(), "accountProfileId": "work"}))).unwrap();
        let record: diri_proto::SessionRecord = serde_json::from_value(spawned).unwrap();
        assert_eq!(record.account_profile.as_ref(), Some(&profile));
        let wait_for_launches = |count| {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while std::fs::read_to_string(Path::new(&profile.config_home).join("launches"))
                .unwrap_or_default()
                .lines()
                .count()
                < count
            {
                assert!(
                    std::time::Instant::now() < deadline,
                    "fake provider launch timed out"
                );
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        };
        wait_for_launches(1);
        let mut changed = profile.clone();
        changed.config_home = temp
            .path()
            .join("different-account")
            .to_string_lossy()
            .into_owned();
        server.accounts.lock().unwrap().upsert(changed).unwrap();
        server.accounts.lock().unwrap().remove("work").unwrap();
        server
            .registry
            .lock()
            .unwrap()
            .update_record(&record.id.0, |r| {
                r.agent_session_id = Some("thread-test".into())
            });
        server
            .session_kill(Some(json!({"sessionID": record.id})))
            .unwrap();
        let resumed: diri_proto::SessionRecord = serde_json::from_value(
            server
                .session_resume(Some(json!({"sessionID": record.id})))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(resumed.account_profile.as_ref(), Some(&profile));
        wait_for_launches(2);
        let fork: diri_proto::SessionRecord = serde_json::from_value(
            server
                .session_fork(Some(json!({"sessionID": record.id})))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(fork.account_profile.as_ref(), Some(&profile));
        wait_for_launches(3);
        server
            .session_kill(Some(json!({"sessionID": fork.id})))
            .unwrap();
        server
            .session_kill(Some(json!({"sessionID": record.id})))
            .unwrap();
        assert!(!temp.path().join("different-account").exists());
        assert!(server.session_spawn(Some(json!({"kind": diri_proto::AgentKind::CODEX, "cwd": temp.path(), "accountProfileId": "work"}))).is_err());
    }

    /// Without a manual path the manifest's binary stays bare: the interactive
    /// login shell (or `spawn_spec`'s PATH absolutization) resolves it at
    /// launch against nvm/mise/Homebrew PATHs the daemon never inherited.
    /// Pre-judging availability by the daemon's own PATH would hard-fail
    /// spawns the login shell can serve, and pin versions to daemon startup.
    #[test]
    fn local_spawns_keep_the_bare_binary_unless_a_path_is_configured() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().expect("temp");
        let server = server(temp.path());
        assert_eq!(
            server
                .resolve_local_agent_executable("claude-code", "not-on-the-daemon-path")
                .expect("a bare binary is not pre-judged by the daemon's PATH"),
            "not-on-the-daemon-path"
        );

        let configured = temp.path().join("claude");
        std::fs::write(&configured, b"#!/bin/sh\nexit 0\n").expect("fake claude executable");
        std::fs::set_permissions(&configured, std::fs::Permissions::from_mode(0o700))
            .expect("make fake claude executable");
        server
            .agent_catalog
            .lock()
            .expect("agent catalog lock")
            .configure(
                None,
                "claude-code",
                crate::agent_catalog::AgentPreference {
                    executable_path: Some(configured.to_string_lossy().into_owned()),
                    show_in_quick_create: Some(true),
                },
            )
            .expect("bind fake claude executable");
        assert_eq!(
            server
                .resolve_local_agent_executable("claude-code", "claude")
                .expect("configured path wins"),
            configured.to_string_lossy()
        );
    }

    #[test]
    fn history_resume_keeps_the_identity_needed_for_another_resume() {
        let temp = tempfile::tempdir().expect("temp");
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
                    "resume": { "style": "flag", "token": "-c" },
                },
                "rules": [],
            })
            .to_string(),
        )
        .expect("write manifest");
        let (probe, _) = ManifestEngine::load_dir(&manifests).expect("load");
        let registry = Arc::new(Mutex::new(Registry::new(
            Arc::new(probe),
            temp.path().join("state.json"),
        )));
        let server = Arc::new(
            ControlServer::new(Arc::clone(&registry), temp.path().join("daemon.sock"))
                .with_logs_dir(temp.path().join("logs")),
        );
        let transcript = temp.path().join("conversation.jsonl");
        std::fs::write(&transcript, "{}\n").expect("transcript");
        let kind = diri_proto::AgentKind::new("probe");

        let result = ok_of(call(
            &server,
            diri_proto::Method::SESSION_RESUME_FROM_HISTORY,
            Some(json!({
                "entry": {
                    "id": "read line",
                    "kind": serde_json::to_value(&kind).expect("kind"),
                    "cwd": temp.path(),
                    "title": "Recovered conversation",
                    "transcriptPath": transcript,
                    "lastActiveAt": 10.0,
                    "cwdExists": true,
                }
            })),
        ));

        assert_eq!(result["kind"], serde_json::to_value(&kind).expect("kind"));
        assert_eq!(result["agentSessionID"], "read line");
        assert_eq!(
            result["transcriptPath"],
            transcript.to_string_lossy().as_ref()
        );
        assert_eq!(result["resumability"], "live");

        let id = result["id"].as_str().expect("session id").to_owned();
        // End the fixture through its own PTY contract. `kill(-pgid, …)` is a
        // production path covered by process-tree tests; invoking it here made
        // this identity-only regression depend on the CI runner granting
        // process-group signalling (macOS runners intermittently return
        // EPERM). The resumed command is `sh -c 'read line'`, so one submitted
        // line exits it naturally and exercises the same status fold.
        registry
            .lock()
            .expect("registry")
            .get(&id)
            .expect("resumed probe")
            .send_text("done", true)
            .expect("finish resumed probe");
        for _ in 0..100 {
            let exited = registry
                .lock()
                .expect("registry")
                .record(&id)
                .is_some_and(|record| {
                    matches!(record.status, diri_proto::SessionStatus::Exited(_))
                });
            if exited {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(
            registry
                .lock()
                .expect("registry")
                .record(&id)
                .expect("record")
                .resumability,
            diri_proto::Resumability::Resumable,
            "the recovered record must remain resumable after its live process is gone"
        );
    }

    /// An agent that dies on its own — a dropped ssh, a crash — leaves its
    /// session in the registry, because only an explicit kill takes one out.
    /// Resume used to read that presence as "already live", call itself a
    /// no-op and hand the corpse straight back, which left a dead session
    /// with no way at all to restart it.
    #[test]
    fn resume_relaunches_a_session_whose_agent_died_on_its_own() {
        check_resume_relaunches(false);
    }

    #[test]
    fn revive_archived_session_clears_archive_durably() {
        check_resume_relaunches(true);
    }

    #[test]
    fn resume_size_falls_back_to_the_last_screen_checkpoint() {
        // After an Engine restart a dead session is no longer in the
        // registry; its last checkpoint still records the size it ran at.
        let temp = tempfile::tempdir().expect("temp");
        let logs = temp.path().join("logs");
        std::fs::create_dir_all(&logs).expect("logs");
        let spec = crate::session::SessionSpec {
            id: "s_gone".into(),
            pty: crate::pty::PtySpec::new(vec!["/bin/sh".into()], "/tmp"),
            manifest_id: "shell".into(),
            authority: crate::Authority::ProcessOnly,
            logs_dir: logs.clone(),
            holder: None,
            remote: None,
            defer_launch: true,
        };
        assert_eq!(previous_screen_size(&spec), None, "no checkpoint, no size");

        let row = vec![diri_proto::grid::GridCell::BLANK; 3];
        let checkpoint = crate::checkpoint::ScreenCheckpoint {
            keyboard_snapshot: None,
            keyboard: None,
            log_offset: 0,
            history_metadata: Vec::new(),
            history: Vec::new(),
            grid: diri_proto::grid::GridUpdate {
                cols: 3,
                rows: 2,
                cursor_col: 0,
                cursor_row: 0,
                cursor_visible: true,
                is_full_snapshot: true,
                changed_rows: vec![
                    diri_proto::grid::ChangedRow::new(0, row.clone()),
                    diri_proto::grid::ChangedRow::new(1, row),
                ],
            },
            marker_buffer: Vec::new(),
            alt_screen: false,
            bracketed_paste: false,
            mouse: diri_proto::terminal::MouseModes::default(),
        };
        checkpoint
            .write_atomically(&crate::checkpoint::ScreenCheckpoint::path_for_log(
                &logs.join("s_gone.bin"),
            ))
            .expect("write checkpoint");
        assert_eq!(previous_screen_size(&spec), Some((3, 2)));
    }

    #[test]
    fn claude_resume_never_targets_an_unwritten_conversation() {
        let home = tempfile::tempdir().expect("temp");
        let project = home.path().join(".claude/projects/-work-repo");
        let elsewhere = home.path().join(".claude/projects/-work-repo--wt-x");
        std::fs::create_dir_all(&project).expect("project dir");
        std::fs::create_dir_all(&elsewhere).expect("worktree dir");
        let old = project.join("old-conv.jsonl");
        std::fs::write(&old, b"{}\n").expect("old transcript");
        std::fs::write(project.join("empty-conv.jsonl"), b"").expect("empty");
        std::fs::write(elsewhere.join("moved-conv.jsonl"), b"{}\n").expect("moved");

        let mut record = test_record("s_claude");
        record.kind = diri_proto::AgentKind::new("claude-code");
        record.cwd = "/work/repo".into();
        let target =
            |id: Option<&str>, path: Option<&Path>, record: &mut diri_proto::SessionRecord| {
                record.agent_session_id = id.map(str::to_owned);
                record.transcript_path = path.map(|path| path.to_string_lossy().into_owned());
                claude_resume_target_in(record, home.path())
            };

        // Hooks moved the tab to a newer id (`/clear`, a fresh `--resume` id)
        // that Claude never wrote: resume the conversation that exists.
        assert_eq!(
            target(Some("new-conv"), Some(&old), &mut record),
            Some(Some("old-conv".into()))
        );
        assert_eq!(
            target(Some("empty-conv"), Some(&old), &mut record),
            Some(Some("old-conv".into()))
        );
        // A written id wins, wherever Claude filed it.
        assert_eq!(
            target(Some("moved-conv"), Some(&old), &mut record),
            Some(Some("moved-conv".into()))
        );
        // Nothing was ever written: start the tab's id fresh instead of
        // `--resume` into "No conversation found".
        assert_eq!(target(Some("new-conv"), None, &mut record), Some(None));
        assert_eq!(
            target(
                Some("new-conv"),
                Some(&project.join("gone.jsonl")),
                &mut record
            ),
            Some(None)
        );
        // Without an id there is nothing to verify; keep the manifest's path.
        assert_eq!(target(None, None, &mut record), None);

        record.kind = diri_proto::AgentKind::new("codex");
        assert_eq!(target(Some("new-conv"), None, &mut record), None);
    }

    #[test]
    fn failed_revive_keeps_the_archived_conversation() {
        let temp = tempfile::tempdir().expect("temp");
        let registry = Arc::new(Mutex::new(Registry::new(
            engine(),
            temp.path().join("state.json"),
        )));
        let mut record = test_record("s_archived");
        record.kind = diri_proto::AgentKind::new("missing-manifest");
        record.agent_session_id = Some("saved-conversation".into());
        registry.lock().expect("registry").insert_record(record);
        let server = Arc::new(ControlServer::new(
            Arc::clone(&registry),
            temp.path().join("daemon.sock"),
        ));
        ok_of(call(
            &server,
            "session.archive",
            Some(json!({ "sessionID": "s_archived" })),
        ));
        let error = err_of(call(
            &server,
            "session.resume",
            Some(json!({ "sessionID": "s_archived" })),
        ));
        assert_eq!(error.code, "not_found");
        let record = registry
            .lock()
            .expect("registry")
            .record("s_archived")
            .expect("saved record");
        assert!(record.is_archived());
        assert_eq!(
            record.agent_session_id.as_deref(),
            Some("saved-conversation")
        );
    }

    /// A local terminal whose shell died under it (a crash, a reboot that
    /// took its Holder) with `cwd` the project and `terminal_cwd` wherever it
    /// had `cd`'d to. `/usr/bin/false` stands in for the shell that went away;
    /// its non-zero exit is not the user closing the tab.
    fn dead_terminal(
        temp: &Path,
        terminal_cwd: Option<&Path>,
    ) -> (Arc<Mutex<Registry>>, Arc<ControlServer>, PathBuf) {
        let project = temp.join("project").canonicalize().expect("project");
        let registry = Arc::new(Mutex::new(Registry::new(engine(), temp.join("state.json"))));
        {
            let mut guard = registry.lock().expect("registry");
            let mut record = test_record("s_term");
            record.cwd = project.to_string_lossy().into_owned();
            record.terminal_cwd = terminal_cwd.map(|path| path.to_string_lossy().into_owned());
            guard
                .spawn(
                    crate::session::SessionSpec {
                        id: "s_term".into(),
                        pty: crate::pty::PtySpec::new(vec!["/usr/bin/false".into()], &project),
                        manifest_id: diri_proto::AgentKind::SHELL_ID.into(),
                        authority: crate::status::Authority::ProcessOnly,
                        logs_dir: temp.join("logs"),
                        holder: None,
                        remote: None,
                        defer_launch: false,
                    },
                    record,
                )
                .expect("spawn");
        }
        let exited = (0..200).any(|_| {
            let exited = registry
                .lock()
                .expect("registry")
                .record("s_term")
                .is_some_and(|record| {
                    matches!(record.status, diri_proto::SessionStatus::Exited(_))
                });
            if !exited {
                std::thread::sleep(Duration::from_millis(20));
            }
            exited
        });
        assert!(exited, "the stand-in shell never exited");
        let server = Arc::new(ControlServer::new(
            Arc::clone(&registry),
            temp.join("daemon.sock"),
        ));
        (registry, server, project)
    }

    /// Where the resumed shell's own process sits, as the Engine samples it.
    fn wait_for_live_directory(registry: &Arc<Mutex<Registry>>, expected: &Path) {
        let expected = expected.to_string_lossy().into_owned();
        let mut last = None;
        for _ in 0..500 {
            last = registry
                .lock()
                .expect("registry")
                .get("s_term")
                .and_then(|session| session.view().terminal_cwd);
            if last.as_deref() == Some(expected.as_str()) {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("the resumed shell runs in {last:?}, not {expected}");
    }

    fn stop_terminal(registry: &Arc<Mutex<Registry>>) {
        let _ = registry
            .lock()
            .expect("registry")
            .terminate("s_term", Duration::from_secs(2));
    }

    /// A terminal that comes back after its shell died starts in the directory
    /// it had `cd`'d to, keeps its project, and says so before the new shell
    /// has been sampled.
    #[test]
    fn a_resumed_terminal_starts_in_its_last_directory() {
        let temp = tempfile::tempdir().expect("temp");
        let sub = temp.path().join("project/crates/engine");
        std::fs::create_dir_all(&sub).expect("sub");
        let sub = sub.canonicalize().expect("sub");
        let (registry, server, project) = dead_terminal(temp.path(), Some(&sub));
        let record = registry
            .lock()
            .expect("registry")
            .record("s_term")
            .expect("record");
        assert_eq!(
            record.resumability,
            diri_proto::Resumability::Resumable,
            "a terminal whose shell died must offer to come back"
        );

        let result = ok_of(call(
            &server,
            "session.resume",
            Some(json!({ "sessionID": "s_term" })),
        ));
        assert!(
            result["status"].get("exited").is_none(),
            "resume handed back the dead terminal: {}",
            result["status"]
        );
        assert_eq!(result["terminalCwd"], sub.to_string_lossy().as_ref());
        assert_eq!(result["cwd"], project.to_string_lossy().as_ref());
        wait_for_live_directory(&registry, &sub);
        stop_terminal(&registry);
    }

    /// A directory deleted while the terminal was down is not an error: the
    /// shell starts in the project, as it always did, and stops claiming the
    /// vanished directory.
    #[test]
    fn a_resumed_terminal_whose_directory_vanished_starts_in_its_project() {
        let temp = tempfile::tempdir().expect("temp");
        std::fs::create_dir_all(temp.path().join("project")).expect("project");
        let gone = temp.path().join("project/gone");
        let (registry, server, project) = dead_terminal(temp.path(), Some(&gone));

        let result = ok_of(call(
            &server,
            "session.resume",
            Some(json!({ "sessionID": "s_term" })),
        ));
        assert!(result.get("terminalCwd").is_none(), "{result}");
        assert_eq!(result["cwd"], project.to_string_lossy().as_ref());
        wait_for_live_directory(&registry, &project);
        stop_terminal(&registry);
    }

    /// Only a local shell restarts in its last directory. Agents re-enter
    /// their conversation in `cwd`, remote shells are left to the Helper, and
    /// nothing but an existing absolute directory is trusted.
    #[test]
    fn only_a_local_terminal_restores_an_existing_absolute_directory() {
        let temp = tempfile::tempdir().expect("temp");
        let sub = temp.path().canonicalize().expect("temp").join("sub");
        std::fs::create_dir_all(&sub).expect("sub");
        let mut shell = test_record("shell");
        shell.terminal_cwd = Some(sub.to_string_lossy().into_owned());
        assert_eq!(restored_terminal_directory(&shell), Some(sub.clone()));

        let mut agent = shell.clone();
        agent.kind = diri_proto::AgentKind::CLAUDE_CODE;
        assert_eq!(restored_terminal_directory(&agent), None);

        let mut remote = shell.clone();
        remote.host = Some("forge".into());
        assert_eq!(restored_terminal_directory(&remote), None);

        let mut relative = shell.clone();
        relative.terminal_cwd = Some("sub".into());
        assert_eq!(restored_terminal_directory(&relative), None);

        let mut file = shell.clone();
        let path = sub.join("notes.txt");
        std::fs::write(&path, "").expect("file");
        file.terminal_cwd = Some(path.to_string_lossy().into_owned());
        assert_eq!(restored_terminal_directory(&file), None);

        shell.terminal_cwd = None;
        assert_eq!(restored_terminal_directory(&shell), None);
    }

    fn check_resume_relaunches(archived: bool) {
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

        let server = Arc::new(ControlServer::new(
            Arc::clone(&registry),
            temp.path().join("daemon.sock"),
        ));
        if archived {
            ok_of(call(
                &server,
                "session.archive",
                Some(json!({ "sessionID": "s_dead" })),
            ));
        }
        let result = ok_of(call(
            &server,
            "session.resume",
            Some(json!({ "sessionID": "s_dead" })),
        ));
        assert!(
            result.get("archivedAt").is_none(),
            "revive must leave the archive"
        );
        let persisted: JsonValue = serde_json::from_slice(
            &std::fs::read(temp.path().join("state.json")).expect("persisted state"),
        )
        .expect("state JSON");
        let saved = persisted["sessions"]
            .as_array()
            .expect("sessions")
            .iter()
            .find(|record| record["id"] == "s_dead")
            .expect("saved session");
        assert!(
            saved.get("archivedAt").is_none(),
            "revive must survive a resync/restart"
        );

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
        if archived {
            let pid = {
                let mut registry = registry.lock().expect("registry");
                // Older revive attempts could leave a live process archived.
                registry.update_record("s_dead", |record| {
                    record.archived_at = Some(diri_proto::DateMillis(42.0));
                });
                registry.get("s_dead").expect("live session").child_pid()
            };
            let restored = ok_of(call(
                &server,
                "session.resume",
                Some(json!({ "sessionID": "s_dead" })),
            ));
            assert!(restored.get("archivedAt").is_none());
            assert_eq!(restored["agentSessionID"], "conv-1");
            assert_eq!(
                registry
                    .lock()
                    .expect("registry")
                    .get("s_dead")
                    .expect("live session")
                    .child_pid(),
                pid,
                "reviving an already-live archived session must not relaunch it"
            );
        }
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

    #[cfg(unix)]
    #[test]
    fn mark_seen_replies_without_writing_and_the_flush_persists_it() {
        use std::os::unix::fs::MetadataExt;
        let temp = tempfile::tempdir().expect("temp");
        let path = temp.path().join("state.json");
        let registry = Arc::new(Mutex::new(Registry::new(engine(), &path)));
        registry
            .lock()
            .expect("registry")
            .insert_record(test_record("s_seen"));
        registry.lock().expect("registry").persist_now().unwrap();
        let identity = || {
            let metadata = std::fs::metadata(&path).unwrap();
            (metadata.ino(), metadata.mtime_nsec())
        };
        let before = identity();
        // Past the debounce window, where a leading-edge persist would write.
        std::thread::sleep(Duration::from_millis(600));
        let server = Arc::new(ControlServer::new(
            Arc::clone(&registry),
            temp.path().join("daemon.sock"),
        ));

        ok_of(call(
            &server,
            "session.mark_seen",
            Some(json!({ "sessionID": "s_seen" })),
        ));
        assert_eq!(identity(), before, "the request thread must not write");

        registry.lock().expect("registry").flush_dirty().unwrap();
        let state: JsonValue = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert!(state["sessions"][0]["lastSeenAt"].is_number());
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
        let server = Arc::new(ControlServer::new(
            registry,
            temp.path().join("daemon.sock"),
        ));

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
    fn mark_unread_returns_a_seen_completion_to_done_unseen() {
        use diri_proto::{AgentKind, AttentionLevel, DateMillis, SessionRecord};
        let temp = tempfile::tempdir().expect("temp");
        let registry = Arc::new(Mutex::new(Registry::new(
            engine(),
            temp.path().join("state.json"),
        )));
        let mut finished = test_record("s_done");
        finished.kind = AgentKind::CLAUDE_CODE;
        finished.last_turn_completed_at = Some(DateMillis(2_000.0));
        let mut fresh = test_record("s_fresh");
        fresh.kind = AgentKind::CLAUDE_CODE;
        registry.lock().expect("registry").insert_record(finished);
        registry.lock().expect("registry").insert_record(fresh);
        let server = Arc::new(ControlServer::new(
            Arc::clone(&registry),
            temp.path().join("daemon.sock"),
        ));
        let attention = |id: &str| {
            let list = ok_of(call(&server, "session.list", None));
            let record = list["sessions"]
                .as_array()
                .unwrap()
                .iter()
                .find(|record| record["id"] == id)
                .cloned()
                .unwrap();
            serde_json::from_value::<SessionRecord>(record)
                .unwrap()
                .attention()
        };
        let params = |id: &str| Some(json!({ "sessionID": id }));

        ok_of(call(&server, "session.mark_seen", params("s_done")));
        assert_eq!(attention("s_done"), AttentionLevel::IdleSeen);
        ok_of(call(&server, "session.mark_unread", params("s_done")));
        assert_eq!(attention("s_done"), AttentionLevel::DoneUnseen);
        ok_of(call(&server, "session.mark_seen", params("s_done")));
        assert_eq!(attention("s_done"), AttentionLevel::IdleSeen);

        // No completed turn: nothing to be unread, and nothing changes.
        ok_of(call(&server, "session.mark_seen", params("s_fresh")));
        ok_of(call(&server, "session.mark_unread", params("s_fresh")));
        assert_eq!(attention("s_fresh"), AttentionLevel::IdleSeen);

        let error = err_of(call(&server, "session.mark_unread", params("s_missing")));
        assert_eq!(error.code, "not_found");
    }

    #[test]
    fn codex_subagent_completion_does_not_finish_the_parent_turn() {
        let temp = tempfile::tempdir().unwrap();
        let config = temp.path().join("codex");
        let root = config.join("sessions/2026/09/16");
        std::fs::create_dir_all(&root).unwrap();
        for (id, source) in [
            ("parent", json!("cli")),
            ("new-parent", json!("cli")),
            (
                "child",
                json!({"subagent": {"thread_spawn": {"parent_thread_id": "parent", "depth": 1}}}),
            ),
        ] {
            std::fs::write(
                root.join(format!("rollout-now-{id}.jsonl")),
                json!({
                    "type": "session_meta", "payload": {"id": id, "cwd": "/tmp", "source": source}
                })
                .to_string(),
            )
            .unwrap();
        }
        let server = server(temp.path());
        {
            let mut registry = server.registry.lock().unwrap();
            let mut record = test_record("s_codex");
            record.kind = diri_proto::AgentKind::CODEX;
            record.agent_session_id = Some("parent".into());
            record.account_profile = Some(diri_proto::AgentAccountProfile {
                id: "test".into(),
                label: "Test".into(),
                agent: "codex".into(),
                host: None,
                config_home: config.to_string_lossy().into_owned(),
                is_default: false,
                login_store: None,
            });
            registry
                .spawn(
                    crate::session::SessionSpec {
                        id: "s_codex".into(),
                        pty: crate::pty::PtySpec::new(
                            vec!["/bin/sh".into(), "-c".into(), "read line".into()],
                            "/tmp",
                        ),
                        manifest_id: "codex".into(),
                        authority: crate::status::Authority::ScreenPrimary,
                        logs_dir: temp.path().join("logs"),
                        holder: None,
                        remote: None,
                        defer_launch: false,
                    },
                    record,
                )
                .unwrap();
        }
        for thread in [
            "child",
            "unavailable-child",
            "parent",
            "parent",
            "new-parent",
        ] {
            {
                let registry = server.registry.lock().unwrap();
                registry
                    .get("s_codex")
                    .unwrap()
                    .feed_signal(crate::status::StatusSignal::Screen(
                        crate::detect::ScreenObservation {
                            state: crate::detect::ManifestState::Working,
                            matched_rule_id: "working-spinner".into(),
                            priority: 900,
                            content_seq: 1,
                            prompt_excerpt: None,
                            options: None,
                        },
                    ));
            }
            ok_of(call(
                &server,
                "hook.report",
                Some(json!({
                    "kind": "codex-notify", "dirijorSessionID": "s_codex",
                    "payload": {"type": "agent-turn-complete", "thread-id": thread}
                })),
            ));
            let registry = server.registry.lock().unwrap();
            let session = registry.get("s_codex").unwrap();
            session.feed_signal(crate::status::StatusSignal::Tick);
            assert_eq!(
                session.status(),
                if thread.contains("child") {
                    diri_proto::SessionStatus::Working
                } else {
                    diri_proto::SessionStatus::Idle
                },
                "callback from {thread}"
            );
        }
    }

    /// `session.remove` holds the Registry through the Holder's TERM→KILL
    /// escalation while a closing Claude waits on its own SessionEnd hook.
    /// The hook must be answered at once and still land, in order.
    #[test]
    fn a_hook_report_never_waits_for_a_busy_registry_and_keeps_order() {
        let temp = tempfile::tempdir().expect("temp");
        let registry = Arc::new(Mutex::new(Registry::new(
            engine(),
            temp.path().join("state.json"),
        )));
        registry
            .lock()
            .expect("registry")
            .insert_record(test_record("s_hook"));
        let server = Arc::new(ControlServer::new(
            Arc::clone(&registry),
            temp.path().join("daemon.sock"),
        ));
        // Only SessionStart may move the tab to another conversation, so the
        // later reports switch with it; applied out of order, the first
        // prompt would arrive for a foreign conversation and lose its title.
        fn prompt(uuid: &str, prompt: &str) -> Option<JsonValue> {
            let event = if uuid == "uuid-1" {
                "UserPromptSubmit"
            } else {
                "SessionStart"
            };
            Some(json!({
                "kind": "claude-hook", "dirijorSessionID": "s_hook", "event": event,
                "payload": {"session_id": uuid, "hook_event_name": event, "prompt": prompt},
            }))
        }
        let busy = registry.lock().expect("registry");
        let (answered, replies) = std::sync::mpsc::channel();
        let agent = {
            let server = Arc::clone(&server);
            std::thread::spawn(move || {
                for (uuid, text) in [("uuid-1", "first prompt"), ("uuid-2", "second prompt")] {
                    ok_of(call(&server, "hook.report", prompt(uuid, text)));
                }
                answered.send(()).unwrap();
            })
        };
        assert!(
            replies.recv_timeout(Duration::from_secs(2)).is_ok(),
            "hook.report waited for the busy Registry"
        );
        drop(busy);
        agent.join().unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let record = loop {
            let record = registry.lock().unwrap().record("s_hook").unwrap();
            if record.agent_session_id.as_deref() == Some("uuid-2")
                || std::time::Instant::now() > deadline
            {
                break record;
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        // Applied in callback order: the first prompt titled the placeholder,
        // the second report's identity is the latest.
        assert_eq!(record.agent_session_id.as_deref(), Some("uuid-2"));
        assert_eq!(record.title, "first prompt");
        // Drained: the next report applies inline, before its reply.
        ok_of(call(
            &server,
            "hook.report",
            prompt("uuid-3", "third prompt"),
        ));
        assert_eq!(
            registry
                .lock()
                .unwrap()
                .record("s_hook")
                .unwrap()
                .agent_session_id
                .as_deref(),
            Some("uuid-3")
        );
    }

    #[test]
    fn a_hook_report_folds_identity_but_rejects_an_untrusted_transcript_path() {
        let temp = tempfile::tempdir().expect("temp");
        let registry = Arc::new(Mutex::new(Registry::new(
            engine(),
            temp.path().join("state.json"),
        )));
        registry
            .lock()
            .expect("registry")
            .insert_record(test_record("s_hook"));
        let server = Arc::new(ControlServer::new(
            registry,
            temp.path().join("daemon.sock"),
        ));

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
        assert_eq!(record["transcriptPath"], Value::Null);
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
        // Readiness is the whole supported catalog, not just what happens to be
        // installed. Naming manifests keeps that honest when one is retired; a
        // count threshold would only ever be quietly loosened.
        for id in ["claude-code", "codex", "cursor", "gemini", "amp", "pi"] {
            assert!(
                agents.iter().any(|agent| agent["kind"] == id),
                "readiness must expose the supported Agent {id}"
            );
        }
        assert_eq!(
            agents
                .iter()
                .take(5)
                .filter_map(|agent| agent["kind"].as_str())
                .collect::<Vec<_>>(),
            ["claude-code", "codex", "antigravity", "cursor", "gemini"],
            "every first-class Agent needs an explicit catalogOrder, or it falls \
             into the alphabetical tail behind Agents most users never install"
        );
        let claude = agents
            .iter()
            .find(|agent| agent["kind"] == "claude-code")
            .expect("claude in the catalog");
        assert_eq!(claude["binary"], "claude");
        assert!(
            claude["descriptor"]["setup"]["url"]
                .as_str()
                .is_some_and(|url| url.starts_with("https://"))
        );
        assert!(
            claude["descriptor"]["injection"]["claudeHooks"]
                .as_bool()
                .unwrap_or(false),
            "the raw manifest descriptor rides along: {claude}"
        );
        let pi = agents
            .iter()
            .find(|agent| agent["kind"] == "pi")
            .expect("Pi remains in Settings even when its executable is absent");
        assert_eq!(pi["binary"], "pi");
        assert_eq!(pi["descriptor"]["displayName"], "Pi");
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
            .insert_record(diri_proto::SessionRecord {
                // An Agent with no resume grammar; a terminal restarts instead.
                kind: diri_proto::AgentKind::new("amp"),
                ..test_record("s_gone")
            });
        let server = Arc::new(ControlServer::new(
            registry,
            temp.path().join("daemon.sock"),
        ));

        ok_of(call(
            &server,
            "session.remove",
            Some(json!({ "sessionID": "s_gone" })),
        ));
        let list = ok_of(call(&server, "session.list", None));
        assert_eq!(list["sessions"].as_array().map(Vec::len), Some(0));

        let reopened = ok_of(call(&server, "session.reopen_last", None));
        assert_eq!(reopened["id"], "s_gone");
        // This Agent cannot resume; it must come back exited, never still
        // claiming the live status it had when closed.
        assert!(
            reopened["status"].get("exited").is_some(),
            "a reopened session with nothing running must read as exited: {}",
            reopened["status"]
        );
        let list = ok_of(call(&server, "session.list", None));
        assert_eq!(list["sessions"].as_array().map(Vec::len), Some(1));

        // The stack is spent.
        let empty = err_of(call(&server, "session.reopen_last", None));
        assert_eq!(empty.code, "bad_request");
    }

    #[test]
    fn reopening_a_resumable_session_relaunches_it() {
        let temp = tempfile::tempdir().expect("temp");
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
        let registry = Arc::new(Mutex::new(Registry::new(
            Arc::new(probe),
            temp.path().join("state.json"),
        )));
        {
            let mut record = test_record("s_closed");
            record.kind = diri_proto::AgentKind::new("probe");
            record.agent_session_id = Some("conv-1".into());
            record.resumability = diri_proto::Resumability::Resumable;
            registry.lock().expect("registry").insert_record(record);
        }
        let server = Arc::new(ControlServer::new(
            Arc::clone(&registry),
            temp.path().join("daemon.sock"),
        ));

        ok_of(call(
            &server,
            "session.remove",
            Some(json!({ "sessionID": "s_closed" })),
        ));
        let reopened = ok_of(call(&server, "session.reopen_last", None));

        assert_eq!(reopened["id"], "s_closed");
        assert!(
            reopened["status"].get("exited").is_none(),
            "reopen must relaunch, not re-list a record with no PTY: {}",
            reopened["status"]
        );
        assert!(
            registry
                .lock()
                .expect("registry")
                .get("s_closed")
                .is_some_and(|session| !session.view().exited),
            "the reopened session must be a live one"
        );
        let _ = registry
            .lock()
            .expect("registry")
            .terminate("s_closed", Duration::from_millis(500));
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
        let server = Arc::new(ControlServer::new(
            registry,
            temp.path().join("daemon.sock"),
        ));

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
    fn worktree_inventory_does_not_block_hello_on_the_same_connection() {
        let temp = tempfile::tempdir().unwrap();
        let (repo, _) = repository_with_linked_worktree(temp.path());
        for n in 0..48 {
            let status = std::process::Command::new("git")
                .args(["worktree", "add", "--detach", "--quiet"])
                .arg(temp.path().join(format!("tree-{n}")))
                .current_dir(&repo)
                .status()
                .unwrap();
            assert!(status.success());
        }
        let server = server(temp.path());
        server
            .registry
            .lock()
            .unwrap()
            .insert_record(ended_resumable_record("inventory", &repo));
        let (stream, mut peer) = UnixStream::pair().unwrap();
        peer.set_read_timeout(Some(Duration::from_millis(250)))
            .unwrap();
        let worker = std::thread::spawn(move || server.serve(stream));
        let start = std::time::Instant::now();
        peer.write_all(
            b"{\"id\":1,\"method\":\"worktree.overview\"}\n{\"id\":2,\"method\":\"hello\"}\n",
        )
        .unwrap();
        let mut reader = BufReader::new(peer);
        let mut first = String::new();
        let result = reader.read_line(&mut first);
        let latency = start.elapsed();
        // Always drain the outstanding scan before removing its fixture.
        reader
            .get_ref()
            .set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap();
        let mut remaining = String::new();
        for _ in 0..if result.is_ok() { 1 } else { 2 } {
            remaining.clear();
            reader.read_line(&mut remaining).unwrap();
        }
        reader.get_ref().shutdown(std::net::Shutdown::Both).unwrap();
        worker.join().unwrap().unwrap();
        eprintln!(
            "50 worktrees: Hello latency {latency:?}; total {:?}",
            start.elapsed()
        );
        assert!(
            result.is_ok(),
            "Hello timed out behind worktree inventory: {result:?}"
        );
        let first: Value = serde_json::from_str(&first).unwrap();
        assert_eq!(
            first["id"], 2,
            "Hello must arrive before the slow inventory"
        );
    }

    #[test]
    fn settings_worktree_cleanup_over_wire_checks_head_and_keeps_branch() {
        let temp = tempfile::tempdir().unwrap();
        let (repo, target) = repository_with_linked_worktree(temp.path());
        let server = server(temp.path());
        let head = std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&target)
            .output()
            .unwrap();
        let head = String::from_utf8(head.stdout).unwrap().trim().to_owned();
        let response = call(
            &server,
            Method::WORKTREE_CLEANUP,
            Some(json!({
                "repoPath": repo, "worktreePath": target, "expectedHead": "stale"
            })),
        );
        assert!(matches!(
            response,
            ControlMessage::Response { result: Err(_), .. }
        ));
        assert!(target.exists());
        ok_of(call(
            &server,
            Method::WORKTREE_CLEANUP,
            Some(json!({
                "repoPath": repo, "worktreePath": target, "expectedHead": head
            })),
        ));
        assert!(!target.exists());
        assert!(
            std::process::Command::new("git")
                .args(["show-ref", "--verify", "refs/heads/feature/reparent"])
                .current_dir(repo)
                .output()
                .unwrap()
                .status
                .success()
        );
    }

    #[test]
    fn ended_session_reparents_to_a_confirmed_project_worktree() {
        let temp = tempfile::tempdir().expect("temp");
        let (repo, target) = repository_with_linked_worktree(temp.path());
        let registry = Arc::new(Mutex::new(Registry::new(
            engine(),
            temp.path().join("state.json"),
        )));
        let record = ended_resumable_record("s_move", &repo);
        registry.lock().expect("registry").insert_record(record);
        let server = Arc::new(ControlServer::new(
            Arc::clone(&registry),
            temp.path().join("daemon.sock"),
        ));

        let result = ok_of(call(
            &server,
            Method::SESSION_REPARENT_WORKTREE,
            Some(json!({
                "sessionID": "s_move",
                "projectRoot": repo,
                "worktreePath": target,
            })),
        ));
        assert_eq!(result["cwd"], target.to_string_lossy().as_ref());
        assert_eq!(result["worktreePath"], target.to_string_lossy().as_ref());
        assert_eq!(result["gitBranch"], "feature/reparent");
        assert!(temp.path().join("state.json").is_file(), "move is durable");
        let state: JsonValue = serde_json::from_slice(
            &std::fs::read(temp.path().join("state.json")).expect("read durable state"),
        )
        .expect("decode durable state");
        let persisted = state["sessions"]
            .as_array()
            .and_then(|sessions| sessions.iter().find(|session| session["id"] == "s_move"))
            .expect("persisted moved session");
        assert_eq!(persisted["cwd"], target.to_string_lossy().as_ref());
        assert_eq!(persisted["worktreePath"], target.to_string_lossy().as_ref());
    }

    #[test]
    fn worktree_reparent_revalidates_liveness_without_mutating_the_record() {
        let temp = tempfile::tempdir().expect("temp");
        let (repo, target) = repository_with_linked_worktree(temp.path());
        let registry = Arc::new(Mutex::new(Registry::new(
            engine(),
            temp.path().join("state.json"),
        )));
        let mut record = test_record("s_live");
        record.cwd = repo.to_string_lossy().into_owned();
        record.project_id = crate::registry::session_project_id(&record.cwd, None);
        record.resumability = diri_proto::Resumability::Resumable;
        registry.lock().expect("registry").insert_record(record);
        let server = Arc::new(ControlServer::new(
            Arc::clone(&registry),
            temp.path().join("daemon.sock"),
        ));

        let error = err_of(call(
            &server,
            Method::SESSION_REPARENT_WORKTREE,
            Some(json!({
                "sessionID": "s_live",
                "projectRoot": repo,
                "worktreePath": target,
            })),
        ));
        assert_eq!(error.code, "bad_request");
        let record = registry
            .lock()
            .expect("registry")
            .record("s_live")
            .expect("record");
        assert_eq!(record.cwd, repo.to_string_lossy());
        assert_eq!(record.worktree_path, None);
    }

    #[test]
    fn worktree_reparent_refuses_unknown_session_inside_symlinked_subdirectory() {
        let temp = tempfile::tempdir().expect("temp");
        let (repo, target) = repository_with_linked_worktree(temp.path());
        let nested = target.join("nested");
        std::fs::create_dir(&nested).expect("nested");
        let alias = temp.path().join("target-alias");
        std::os::unix::fs::symlink(&target, &alias).expect("symlink");

        let registry = Arc::new(Mutex::new(Registry::new(
            engine(),
            temp.path().join("state.json"),
        )));
        let source = ended_resumable_record("source", &repo);
        let mut occupant = test_record("occupant");
        occupant.cwd = alias.join("nested").to_string_lossy().into_owned();
        occupant.status = diri_proto::SessionStatus::Unknown;
        let original_cwd = source.cwd.clone();
        {
            let mut registry = registry.lock().expect("registry");
            registry.insert_record(source);
            registry.insert_record(occupant);
        }
        let server = Arc::new(ControlServer::new(
            Arc::clone(&registry),
            temp.path().join("daemon.sock"),
        ));

        let error = err_of(call(
            &server,
            Method::SESSION_REPARENT_WORKTREE,
            Some(json!({
                "sessionID": "source",
                "projectRoot": repo,
                "worktreePath": target,
            })),
        ));
        assert_eq!(error.code, "bad_request");
        assert!(error.message.contains("owns this worktree"));
        let source = registry
            .lock()
            .expect("registry")
            .record("source")
            .expect("source");
        assert_eq!(source.cwd, original_cwd);
        assert_eq!(source.worktree_path, None);
    }

    #[test]
    fn remote_session_with_same_path_does_not_occupy_a_local_worktree() {
        let temp = tempfile::tempdir().expect("temp");
        let (repo, target) = repository_with_linked_worktree(temp.path());
        let registry = Arc::new(Mutex::new(Registry::new(
            engine(),
            temp.path().join("state.json"),
        )));
        let source = ended_resumable_record("source", &repo);
        let mut remote = test_record("remote");
        remote.cwd = target.to_string_lossy().into_owned();
        remote.host = Some("forge".into());
        remote.status = diri_proto::SessionStatus::Working;
        {
            let mut registry = registry.lock().expect("registry");
            registry.insert_record(source);
            registry.insert_record(remote);
        }
        let server = Arc::new(ControlServer::new(
            Arc::clone(&registry),
            temp.path().join("daemon.sock"),
        ));

        let result = ok_of(call(
            &server,
            Method::SESSION_REPARENT_WORKTREE,
            Some(json!({
                "sessionID": "source",
                "projectRoot": repo,
                "worktreePath": target,
            })),
        ));
        assert_eq!(result["cwd"], target.to_string_lossy().as_ref());
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
    fn remote_usage_fails_closed_without_transport() {
        let temp = tempfile::tempdir().unwrap();
        let server = server(temp.path());
        let error = err_of(call(
            &server,
            Method::HOST_USAGE,
            Some(json!({"host":"forge"})),
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
    fn pi_trust_and_composer_screens_are_told_apart() {
        let lines = |text: &str| text.lines().map(str::to_owned).collect::<Vec<_>>();
        let rule = "─".repeat(60);
        // Captured from Pi 0.99.2 in a folder holding `.pi/settings.json`.
        let dialog = format!(
            "{rule}\n Trust project folder?\n /tmp/project\n\n\
             This allows pi to load .pi settings and resources, install missing project packages, and execute\n\
             project extensions.\n\n → Trust\n   Trust parent folder (/tmp)\n   Trust (this session only)\n\
             \x20  Do not trust\n   Do not trust (this session only)\n\n\
             \x20↑↓ navigate  enter select  escape/ctrl+c cancel\n\n{rule}"
        );
        assert!(is_pi_project_trust_screen(&lines(&dialog)));
        assert!(!is_pi_composer_screen(&lines(&dialog)));

        let composer = format!(
            " ▀▀█  v0.99.2\n Warning: fd not found. Offline mode enabled, skipping download.\n\
             {rule}\n\n{rule}\n/tmp/project\n0.0%/128k (auto)                fake-model"
        );
        assert!(!is_pi_project_trust_screen(&lines(&composer)));
        assert!(is_pi_composer_screen(&lines(&composer)));
    }

    #[test]
    fn copilot_trust_acceptance_is_limited_to_the_selected_folder_dialog() {
        let lines = |text: &str| text.lines().map(str::to_owned).collect::<Vec<_>>();
        let trust = include_str!("../tests/fixtures/copilot_screens/trust.txt");
        assert!(is_copilot_folder_trust_screen(&lines(trust)));
        assert!(!is_copilot_folder_trust_screen(&lines(
            &trust.replace("❯ 1. Yes", "  1. Yes")
        )));
        for other in [
            include_str!("../tests/fixtures/copilot_screens/permission.txt"),
            include_str!("../tests/fixtures/copilot_screens/login.txt"),
        ] {
            assert!(!is_copilot_folder_trust_screen(&lines(other)));
        }
        let stale = format!(
            "{trust}\n{}",
            include_str!("../tests/fixtures/copilot_screens/idle.txt")
        );
        assert!(!is_copilot_folder_trust_screen(&lines(&stale)));
    }

    #[test]
    fn cursor_trust_requires_the_live_selector() {
        let lines = |screen: &str| screen.lines().map(str::to_owned).collect::<Vec<_>>();
        assert!(is_cursor_workspace_trust_screen(&lines(include_str!(
            "../tests/fixtures/cursor_screens/trust.txt"
        ))));
        assert!(!is_cursor_workspace_trust_screen(&lines(include_str!(
            "../tests/fixtures/cursor_screens/idle.txt"
        ))));
        assert!(!is_cursor_workspace_trust_screen(&lines(include_str!(
            "../tests/fixtures/cursor_screens/login.txt"
        ))));
    }

    #[test]
    fn gemini_trust_is_read_from_the_dialog_at_the_bottom_only() {
        let lines = |text: &str| text.lines().map(str::to_owned).collect::<Vec<_>>();
        let dialog = "│ Do you trust the files in this folder? │\n\
                      │ ● 1. Trust folder (project)          │\n\
                      │   2. Trust parent folder (work)      │\n\
                      │   3. Don't trust                     │";
        assert!(is_gemini_folder_trust_screen(&lines(dialog)));
        assert!(!is_gemini_composer_screen(&lines(dialog)));

        // After accepting, Gemini restarts beneath the old dialog.
        let restarted = format!(
            "{dialog}\n Gemini CLI is restarting to apply the trust changes...\n\
             Gemini CLI v0.62.0\n Tips for getting started:\n\
             1. Create GEMINI.md files\n ▄▄▄▄▄▄\n\
             >   Type your message or @path/to/file\n ▀▀▀▀▀▀\n\
             workspace (/directory)\n ~/project"
        );
        assert!(!is_gemini_folder_trust_screen(&lines(&restarted)));
        assert!(is_gemini_composer_screen(&lines(&restarted)));
        assert!(gemini_restarted_below_notice(&lines(&restarted)));
        // The outgoing process's composer, drawn before the notice.
        let outgoing = " ▄▄▄▄▄▄\n >   Type your message or @path/to/file\n ▀▀▀▀▀▀\n\
                        Gemini CLI is restarting to apply the trust changes...";
        assert!(!gemini_restarted_below_notice(&lines(outgoing)));
    }

    #[test]
    fn workspace_trust_auto_accept_is_narrowly_scoped_to_claudes_exact_picker() {
        let lines = |text: &str| text.lines().map(str::to_owned).collect::<Vec<_>>();
        let key = |screen: &str| claude_workspace_trust_key(&lines(screen));
        // Claude Code 2.1.286, rendered: "No, exit" first, unnumbered, focused.
        let current = " Claude Code'll be able to read, edit, and execute files here.\n\n \
                       Security guide\n\n \
                       ❯ No, exit\n   Yes, I trust this folder\n\n \
                       Enter to confirm · Esc to cancel";
        assert_eq!(key(current), Some(ClaudeTrustKey::Down));
        let moved = current
            .replace("❯ No, exit", "  No, exit")
            .replace("  Yes, I trust", "❯ Yes, I trust");
        assert_eq!(key(&moved), Some(ClaudeTrustKey::Confirm));
        // The older numbered layout: "Yes" first and focused.
        assert_eq!(
            key("❯ 1. Yes, I trust this folder\n  2. No, exit\n\nEnter to confirm"),
            Some(ClaudeTrustKey::Confirm)
        );
        assert_eq!(
            key("  1. Yes, I trust this folder\n❯ 2. No, exit"),
            Some(ClaudeTrustKey::Up)
        );
        // Not the picker: another dialog, the phrase alone, no focus.
        assert_eq!(key("❯ 1. Yes, allow this shell command\n  2. No"), None);
        assert_eq!(key("Yes, I trust this folder"), None);
        assert_eq!(key("  Yes, I trust this folder\n  No, exit"), None);
        // The phrases scrolled up out of the bottom of a busy screen.
        let scrolled = format!("{current}{}", "\noutput".repeat(12));
        assert_eq!(key(&scrolled), None);
    }

    /// Claude Code 2.1.287's first screen, as rendered in a fresh config.
    const CLAUDE_THEME_PICKER: &str = " Let's get started.\n\n \
        Choose the text style that looks best with your terminal\n \
        To change this later, run /theme\n\n     \
        Auto (match terminal)\n \
        ❯ ✔ Dark mode\n     \
        Light mode\n     \
        Dark mode (colorblind-friendly)\n     \
        Light mode (colorblind-friendly)\n     \
        Dark mode (ANSI colors only)\n     \
        Light mode (ANSI colors only)\n \
        ╌╌╌╌╌╌╌╌\n  1  function greet() {\n \
        ╌╌╌╌╌╌╌╌\n  Syntax theme: Monokai Extended (ctrl+t to disable)";

    #[test]
    fn the_first_run_text_style_follows_the_window_and_never_a_variant() {
        use diri_proto::TerminalAppearance::{Dark, Light};
        let lines = |text: &str| text.lines().map(str::to_owned).collect::<Vec<_>>();
        let key = |screen: &str, appearance| claude_theme_key(&lines(screen), appearance);
        assert_eq!(
            key(CLAUDE_THEME_PICKER, Dark),
            Some(ClaudeTrustKey::Confirm)
        );
        assert_eq!(key(CLAUDE_THEME_PICKER, Light), Some(ClaudeTrustKey::Down));
        let on_light = CLAUDE_THEME_PICKER
            .replace("❯ ✔ Dark mode", "  ✔ Dark mode")
            .replace("     Light mode\n", " ❯   Light mode\n");
        assert_eq!(key(&on_light, Light), Some(ClaudeTrustKey::Confirm));
        assert_eq!(key(&on_light, Dark), Some(ClaudeTrustKey::Up));
        // Past the colorblind variant: back up, never confirm it.
        let past = CLAUDE_THEME_PICKER
            .replace("❯ ✔ Dark mode", "  ✔ Dark mode")
            .replace("     Light mode (colorblind", " ❯   Light mode (colorblind");
        assert_eq!(key(&past, Light), Some(ClaudeTrustKey::Up));
        // A numbered layout.
        assert_eq!(
            key(
                "Choose the text style\n❯ 1. Dark mode\n  2. Light mode",
                Light
            ),
            Some(ClaudeTrustKey::Down)
        );
        // Any other list that happens to say "Light mode" is not the picker.
        assert_eq!(key("/theme\n❯ Dark mode\n  Light mode", Light), None);
    }

    #[test]
    fn first_run_screens_hold_the_trust_watch_but_a_composer_does_not() {
        let lines = |text: &str| text.lines().map(str::to_owned).collect::<Vec<_>>();
        assert!(claude_first_run_screen(&lines(CLAUDE_THEME_PICKER)));
        assert!(claude_first_run_screen(&lines(
            " Select login method:\n ❯ 1. Claude account with subscription"
        )));
        assert!(claude_first_run_screen(&lines(
            " Browser didn't open? Use the url below to sign in\n Paste code here if prompted >"
        )));
        assert!(claude_first_run_screen(&lines(
            " Security notes:\n 1. Claude can make mistakes.\n Press Enter to continue…"
        )));
        assert!(!claude_first_run_screen(&lines(
            "╭────╮\n│ > │\n╰────╯\n  ? for shortcuts"
        )));
    }

    #[test]
    fn an_orphaned_engine_retires_only_after_an_unbroken_idle_grace() {
        let grace = Duration::from_secs(600);
        let start = Instant::now();
        let at = |seconds: u64| start + Duration::from_secs(seconds);
        let mut watch = OrphanWatch::default();

        assert!(!watch.observe(0, 0, at(0), grace), "the grace starts now");
        assert!(!watch.observe(0, 0, at(599), grace));
        assert!(watch.observe(0, 0, at(600), grace));

        // A client or a live session at any point starts the grace over.
        let mut watch = OrphanWatch::default();
        assert!(!watch.observe(0, 0, at(0), grace));
        assert!(!watch.observe(0, 1, at(500), grace), "a client is attached");
        assert!(
            !watch.observe(0, 0, at(700), grace),
            "idle again only since 700"
        );
        assert!(!watch.observe(1, 0, at(900), grace), "a session is live");
        assert!(!watch.observe(0, 0, at(1000), grace));
        assert!(!watch.observe(0, 0, at(1599), grace));
        assert!(watch.observe(0, 0, at(1600), grace));
    }

    #[test]
    fn openssh_failures_become_structured_codes_not_internal() {
        use crate::remote::ssh_error::{SshFailure, SshFailureClass};
        let error = io_control_error(
            SshFailure::new(
                SshFailureClass::UnresolvedHost,
                "remote platform probe",
                b"ssh: Could not resolve hostname hogwarts: nodename nor servname provided, or not known\n",
            )
            .into_io_error(),
        );
        assert_eq!(error.code, "ssh_unresolved_host");
        assert!(
            error
                .message
                .starts_with("SSH could not resolve the host name.")
        );
        assert!(
            error
                .message
                .contains("Could not resolve hostname hogwarts")
        );
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
    fn shutdown_handlers_propagate_persistence_failure_before_scheduling_exit() {
        let temp = tempfile::tempdir().expect("temp");
        let blocked_parent = temp.path().join("not-a-directory");
        std::fs::write(&blocked_parent, b"file").expect("blocking file");
        let registry = Registry::new(engine(), blocked_parent.join("state.json"));
        let server = ControlServer::new(
            Arc::new(Mutex::new(registry)),
            temp.path().join("daemon.sock"),
        );

        for error in [
            server
                .daemon_prepare_shutdown()
                .expect_err("prepare must report persistence failure"),
            server
                .daemon_shutdown_if_idle()
                .expect_err("idle shutdown must report persistence failure"),
            server
                .daemon_shutdown()
                .expect_err("forced shutdown must report persistence failure"),
        ] {
            assert_eq!(error.code, "internal");
            assert!(
                error.message.contains("not-a-directory")
                    || error.message.contains("Not a directory")
                    || error.message.contains("File exists"),
                "unexpected persistence error: {error}"
            );
        }
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
