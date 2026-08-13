use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use diri_proto::control::{ControlError, ControlMessage, JsonValue, encode_line};
use diri_proto::methods::*;
use diri_proto::model::{Project, SessionId, SessionRecord, WorktreeInfo};
use diri_proto::paths::DirijorPaths;
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::sync::{Mutex, RwLock, broadcast, mpsc, oneshot, watch};
use tokio::task::JoinHandle;

use crate::connection::ActiveConnection;
use crate::state::{ConnectionState, EventEnvelope};

pub const CLIENT_BUILD: &str = "diri-0.1.0";

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(25);
const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(10);
const INITIAL_BACKOFF: Duration = Duration::from_millis(500);
const MAX_BACKOFF: Duration = Duration::from_secs(8);
const EVENT_CHANNEL_CAPACITY: usize = 4096;
// A local catalog scan is filesystem metadata over the manifest list; nothing
// in it blocks. A remote one crosses ssh and may bootstrap the Helper before it
// can answer, which the Engine bounds far more generously than a user will wait
// staring at a spinner. Timing out does not waste the scan: the Engine finishes
// it and caches the result, so the retry this failure re-enables is usually
// instant.
const AGENT_CATALOG_TIMEOUT: Duration = Duration::from_secs(30);
const REMOTE_AGENT_CATALOG_TIMEOUT: Duration = Duration::from_secs(240);

/// Errors surfaced by daemon requests and the reconnecting transport.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClientError {
    Control(ControlError),
    Disconnected(String),
    Timeout(String),
    Io(String),
    Json(String),
    Protocol(String),
}

impl ClientError {
    pub(crate) fn disconnected(message: impl Into<String>) -> Self {
        Self::Disconnected(message.into())
    }

    pub(crate) fn io(error: impl fmt::Display) -> Self {
        Self::Io(error.to_string())
    }

    pub(crate) fn json(error: impl fmt::Display) -> Self {
        Self::Json(error.to_string())
    }

    pub(crate) fn protocol(message: impl Into<String>) -> Self {
        Self::Protocol(message.into())
    }
}

impl fmt::Display for ClientError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Control(error) => error.fmt(formatter),
            Self::Disconnected(message) => write!(formatter, "disconnected: {message}"),
            Self::Timeout(message) => write!(formatter, "timeout: {message}"),
            Self::Io(message) => write!(formatter, "I/O error: {message}"),
            Self::Json(message) => write!(formatter, "JSON error: {message}"),
            Self::Protocol(message) => write!(formatter, "protocol error: {message}"),
        }
    }
}

impl std::error::Error for ClientError {}

impl From<ControlError> for ClientError {
    fn from(error: ControlError) -> Self {
        Self::Control(error)
    }
}

type PendingResult = Result<JsonValue, ClientError>;

pub(crate) struct ClientCore {
    socket_path: PathBuf,
    build: String,
    token: Option<String>,
    next_request_id: AtomicU64,
    pending: Mutex<HashMap<u64, oneshot::Sender<PendingResult>>>,
    writer: RwLock<Option<mpsc::Sender<Vec<u8>>>>,
    state_tx: watch::Sender<ConnectionState>,
    event_tx: broadcast::Sender<EventEnvelope>,
    want_events: AtomicBool,
    events_subscribed: AtomicBool,
    last_seq: AtomicU64,
    shutdown_tx: watch::Sender<bool>,
    retry_tx: watch::Sender<u64>,
}

impl ClientCore {
    /// The only event kinds diri asks the daemon for.
    ///
    /// This list is EXPLICIT, not "everything", because the daemon publishes a
    /// second tier of narrow automation events (`session.status`,
    /// `session.needs_input`, `session.output`, `session.artifact`, worktree
    /// lifecycle, …) that exist for scripts and agents. Every one of them is
    /// derived from a record change diri already receives in full via
    /// `session.updated`, so subscribing to them would cost the client wakeups
    /// and decode work for information it already has — `session.output` alone
    /// fires roughly once per second per busy session. The idle cost of this
    /// client has been tuned deliberately; an unfiltered subscription quietly
    /// gives that back.
    ///
    /// IF YOU ADD AN EVENT KIND THAT DIRI NEEDS, ADD IT HERE TOO. Server-side
    /// filtering means an unlisted kind never reaches `route_message`, and the
    /// symptom is silence, not an error.
    const EVENT_KINDS: [&'static str; 4] = [
        EventName::SESSION_UPDATED,
        EventName::SESSION_RESOURCES,
        EventName::SESSION_REMOVED,
        EventName::PROJECT_UPDATED,
    ];

    async fn request<P: Serialize + ?Sized>(
        &self,
        method: &str,
        params: Option<&P>,
        timeout: Option<Duration>,
    ) -> Result<JsonValue, ClientError> {
        let id = self.next_request_id.fetch_add(1, Ordering::Relaxed) + 1;
        let params = params
            .map(serde_json::to_value)
            .transpose()
            .map_err(ClientError::json)?;
        let message = ControlMessage::Request {
            id,
            method: method.to_owned(),
            params,
        };
        let line = encode_line(&message).map_err(ClientError::json)?;
        let writer = self
            .writer
            .read()
            .await
            .clone()
            .ok_or_else(|| ClientError::disconnected("not connected to daemon"))?;
        let (response_tx, response_rx) = oneshot::channel();
        self.pending.lock().await.insert(id, response_tx);

        if writer.send(line).await.is_err() {
            self.pending.lock().await.remove(&id);
            return Err(ClientError::disconnected(
                "control connection writer stopped",
            ));
        }

        let response = if let Some(timeout) = timeout {
            match tokio::time::timeout(timeout, response_rx).await {
                Ok(response) => response,
                Err(_) => {
                    self.pending.lock().await.remove(&id);
                    return Err(ClientError::Timeout(format!(
                        "request {id} ({method}) timed out"
                    )));
                }
            }
        } else {
            response_rx.await
        };

        response.unwrap_or_else(|_| {
            Err(ClientError::disconnected(
                "response channel closed before the daemon replied",
            ))
        })
    }

    async fn request_typed<P, R>(
        &self,
        method: &str,
        params: Option<&P>,
        timeout: Option<Duration>,
    ) -> Result<R, ClientError>
    where
        P: Serialize + ?Sized,
        R: DeserializeOwned,
    {
        let value = self.request(method, params, timeout).await?;
        serde_json::from_value(value).map_err(ClientError::json)
    }

    async fn hello(&self, timeout: Option<Duration>) -> Result<HelloResult, ClientError> {
        let params = HelloParams {
            proto: diri_proto::control::WIRE_VERSION,
            build: self.build.clone(),
            token: self.token.clone(),
        };
        self.request_typed(Method::HELLO, Some(&params), timeout)
            .await
    }

    async fn subscribe_to_events(&self) -> Result<EventsSubscribeResult, ClientError> {
        if self
            .events_subscribed
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Ok(EventsSubscribeResult { subscribed: true });
        }
        let seq = self.last_seq.load(Ordering::Acquire);
        let params = EventsSubscribeParams {
            since_seq: (seq != 0).then_some(seq),
            sessions: None,
            kinds: Some(
                Self::EVENT_KINDS
                    .iter()
                    .map(|name| name.to_string())
                    .collect(),
            ),
        };
        let result = self
            .request_typed(
                Method::EVENTS_SUBSCRIBE,
                Some(&params),
                HEARTBEAT_TIMEOUT.into(),
            )
            .await;
        if result.is_err() {
            self.events_subscribed.store(false, Ordering::Release);
        }
        result
    }

    pub(crate) async fn route_message(&self, message: ControlMessage) {
        match message {
            ControlMessage::Response { id, result } => {
                let Some(sender) = self.pending.lock().await.remove(&id) else {
                    return;
                };
                let result = result.map_err(ClientError::Control);
                let _ = sender.send(result);
            }
            ControlMessage::Event { name, seq, params } => {
                self.last_seq.fetch_max(seq, Ordering::AcqRel);
                let _ = self.event_tx.send(EventEnvelope { name, seq, params });
            }
            ControlMessage::Request { .. } => {}
        }
    }

    async fn fail_pending(&self, error: ClientError) {
        let pending = std::mem::take(&mut *self.pending.lock().await);
        for (_, sender) in pending {
            let _ = sender.send(Err(error.clone()));
        }
    }

    fn set_state(&self, state: ConnectionState) {
        self.state_tx.send_replace(state);
    }
}

/// A reconnecting, request-correlated control client for the existing Dirijor daemon.
pub struct DaemonClient {
    core: Arc<ClientCore>,
    lifecycle: StdMutex<Option<JoinHandle<()>>>,
}

impl Default for DaemonClient {
    fn default() -> Self {
        Self::new()
    }
}

impl DaemonClient {
    /// Uses the platform path provider's default control socket.
    pub fn new() -> Self {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/nonexistent"));
        Self::with_socket_path(DirijorPaths::socket(home))
    }

    /// Uses an injectable socket path, primarily for deterministic tests.
    pub fn with_socket_path(socket_path: impl Into<PathBuf>) -> Self {
        Self::with_config(socket_path, CLIENT_BUILD, None)
    }

    pub fn with_config(
        socket_path: impl Into<PathBuf>,
        build: impl Into<String>,
        token: Option<String>,
    ) -> Self {
        let (state_tx, _) = watch::channel(ConnectionState::Disconnected(
            "not connected to daemon".to_owned(),
        ));
        let (event_tx, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        let (shutdown_tx, _) = watch::channel(false);
        let (retry_tx, _) = watch::channel(0);
        Self {
            core: Arc::new(ClientCore {
                socket_path: socket_path.into(),
                build: build.into(),
                token,
                next_request_id: AtomicU64::new(0),
                pending: Mutex::new(HashMap::new()),
                writer: RwLock::new(None),
                state_tx,
                event_tx,
                want_events: AtomicBool::new(false),
                events_subscribed: AtomicBool::new(false),
                last_seq: AtomicU64::new(0),
                shutdown_tx,
                retry_tx,
            }),
            lifecycle: StdMutex::new(None),
        }
    }

    pub fn socket_path(&self) -> &Path {
        &self.core.socket_path
    }

    /// Starts the connect/reconnect loop. Repeated calls are idempotent.
    pub fn connect(&self) {
        let mut lifecycle = self.lifecycle.lock().expect("lifecycle mutex poisoned");
        if lifecycle.as_ref().is_some_and(|task| !task.is_finished()) {
            return;
        }
        let core = Arc::clone(&self.core);
        *lifecycle = Some(tokio::spawn(async move { run_lifecycle(core).await }));
    }

    /// Synchronously asks the connect/reconnect loop to close its control
    /// connection. This is the non-waiting half of [`Self::shutdown`] for
    /// lifecycle callbacks whose futures may be cancelled immediately.
    ///
    /// The signal is idempotent and never sends a daemon shutdown request.
    pub fn begin_shutdown(&self) {
        self.core.shutdown_tx.send_replace(true);
    }

    /// Stops this client only. This never sends a shutdown request to the daemon.
    pub async fn shutdown(&self) {
        self.begin_shutdown();
        let task = self
            .lifecycle
            .lock()
            .expect("lifecycle mutex poisoned")
            .take();
        if let Some(task) = task {
            let _ = task.await;
        }
        *self.core.writer.write().await = None;
        self.core
            .fail_pending(ClientError::disconnected("client shut down"))
            .await;
        self.core
            .set_state(ConnectionState::Disconnected("client shut down".to_owned()));
    }

    pub fn connection_state(&self) -> watch::Receiver<ConnectionState> {
        self.core.state_tx.subscribe()
    }

    /// Wakes the reconnect loop out of its bounded backoff. The operation is
    /// idempotent and never launches, kills, or replaces a daemon by itself.
    pub fn retry_now(&self) {
        let next = self.core.retry_tx.borrow().wrapping_add(1);
        self.core.retry_tx.send_replace(next);
    }

    pub fn events(&self) -> broadcast::Receiver<EventEnvelope> {
        self.core.want_events.store(true, Ordering::Release);
        let receiver = self.core.event_tx.subscribe();
        if self.core.state_tx.borrow().is_connected() {
            let core = Arc::clone(&self.core);
            tokio::spawn(async move {
                let _ = core.subscribe_to_events().await;
            });
        }
        receiver
    }

    /// Subscribes and waits for the daemon acknowledgement when already connected.
    pub async fn subscribe_events(
        &self,
    ) -> Result<broadcast::Receiver<EventEnvelope>, ClientError> {
        self.core.want_events.store(true, Ordering::Release);
        let receiver = self.core.event_tx.subscribe();
        if self.core.state_tx.borrow().is_connected() {
            self.core.subscribe_to_events().await?;
        }
        Ok(receiver)
    }

    pub fn last_seq(&self) -> u64 {
        self.core.last_seq.load(Ordering::Acquire)
    }

    pub async fn wait_until_connected(
        &self,
        timeout: Duration,
    ) -> Result<HelloResult, ClientError> {
        let mut states = self.connection_state();
        let wait = async {
            loop {
                if let ConnectionState::Connected(hello) = &*states.borrow_and_update() {
                    return Ok(hello.clone());
                }
                states
                    .changed()
                    .await
                    .map_err(|_| ClientError::disconnected("connection state channel closed"))?;
            }
        };
        match tokio::time::timeout(timeout, wait).await {
            Ok(result) => result,
            Err(_) => Err(ClientError::Timeout(format!(
                "waiting for daemon connection; latest state: {:?}",
                *self.core.state_tx.borrow()
            ))),
        }
    }

    pub async fn request<P: Serialize + ?Sized>(
        &self,
        method: &str,
        params: Option<&P>,
        timeout: Option<Duration>,
    ) -> Result<JsonValue, ClientError> {
        self.core.request(method, params, timeout).await
    }

    pub async fn hello(&self) -> Result<HelloResult, ClientError> {
        self.core.hello(HEARTBEAT_TIMEOUT.into()).await
    }

    pub async fn sessions(&self) -> Result<SessionListResult, ClientError> {
        self.no_params(Method::SESSION_LIST).await
    }

    pub async fn spawn(&self, params: SessionSpawnParams) -> Result<SessionId, ClientError> {
        let record: SessionSpawnResult = self.typed(Method::SESSION_SPAWN, &params).await?;
        Ok(record.id)
    }

    pub async fn kill(&self, session_id: &SessionId) -> Result<(), ClientError> {
        self.empty(Method::SESSION_KILL, &session_params(session_id))
            .await
    }

    pub async fn remove(&self, session_id: &SessionId) -> Result<(), ClientError> {
        self.empty(Method::SESSION_REMOVE, &session_params(session_id))
            .await
    }

    pub async fn rename(&self, session_id: &SessionId, title: String) -> Result<(), ClientError> {
        self.empty(
            Method::SESSION_RENAME,
            &SessionRenameParams {
                session_id: session_id.clone(),
                title,
            },
        )
        .await
    }

    pub async fn resume(&self, session_id: &SessionId) -> Result<SessionId, ClientError> {
        let record: SessionResumeResult = self
            .typed(Method::SESSION_RESUME, &session_params(session_id))
            .await?;
        Ok(record.id)
    }

    /// `session.migrate`: git shuttling + respawn can take a while, so this
    /// carries its own generous timeout instead of waiting forever.
    pub async fn migrate(
        &self,
        session_id: &SessionId,
        target_host: Option<String>,
    ) -> Result<SessionMigrateResult, ClientError> {
        self.core
            .request_typed(
                Method::SESSION_MIGRATE,
                Some(&SessionMigrateParams {
                    session_id: session_id.clone(),
                    target_host,
                }),
                Some(Duration::from_secs(600)),
            )
            .await
    }

    pub async fn reparent_worktree(
        &self,
        params: SessionReparentWorktreeParams,
    ) -> Result<SessionReparentWorktreeResult, ClientError> {
        self.typed(Method::SESSION_REPARENT_WORKTREE, &params).await
    }

    /// `host.sync_prefs`: rsync over ssh — bounded, but slower than a local
    /// round trip.
    pub async fn sync_prefs(&self, host: &str) -> Result<HostSyncPrefsResult, ClientError> {
        self.core
            .request_typed(
                Method::HOST_SYNC_PREFS,
                Some(&HostSyncPrefsParams {
                    host: host.to_owned(),
                }),
                Some(Duration::from_secs(300)),
            )
            .await
    }

    /// Bootstraps and validates one configured SSH host. This remains an
    /// Engine RPC so the desktop app never executes SSH or handles protocol
    /// credentials itself.
    pub async fn initialize_host(&self, host: &str) -> Result<HostInitializeResult, ClientError> {
        self.prepare_host(host, false).await
    }

    /// Re-runs the complete verified upload/bootstrap path for a configured
    /// host. Existing Holder sessions keep their creation-time Helper.
    pub async fn reinstall_host(&self, host: &str) -> Result<HostInitializeResult, ClientError> {
        self.prepare_host(host, true).await
    }

    async fn prepare_host(
        &self,
        host: &str,
        force_reinstall: bool,
    ) -> Result<HostInitializeResult, ClientError> {
        self.core
            .request_typed(
                Method::HOST_INITIALIZE,
                Some(&HostInitializeParams {
                    host: host.to_owned(),
                    force_reinstall,
                }),
                Some(Duration::from_secs(600)),
            )
            .await
    }

    /// Lists exactly one directory level on the Engine-selected machine.
    /// Remote requests stay behind the Engine's authenticated SSH transport.
    pub async fn list_directories(
        &self,
        host: Option<String>,
        path: String,
    ) -> Result<HostListDirectoriesResult, ClientError> {
        self.core
            .request_typed(
                Method::HOST_LIST_DIRECTORIES,
                Some(&HostListDirectoriesParams {
                    host,
                    path,
                    mode: diri_proto::remote_pty::DirectoryListMode::Directories,
                }),
                Some(Duration::from_secs(30)),
            )
            .await
    }

    /// `host.locate_repo`: cached daemon-side; the first remote lookup still
    /// crosses ssh, hence the explicit timeout.
    pub async fn locate_repo(
        &self,
        params: HostLocateRepoParams,
    ) -> Result<HostLocateRepoResult, ClientError> {
        self.core
            .request_typed(
                Method::HOST_LOCATE_REPO,
                Some(&params),
                Some(Duration::from_secs(60)),
            )
            .await
    }

    pub async fn send_text(
        &self,
        session_id: &SessionId,
        text: String,
        submit: bool,
    ) -> Result<(), ClientError> {
        self.empty(
            Method::SESSION_SEND_TEXT,
            &SendTextParams {
                session_id: session_id.clone(),
                text,
                submit,
            },
        )
        .await
    }

    pub async fn resize(
        &self,
        session_id: &SessionId,
        cols: i64,
        rows: i64,
    ) -> Result<(), ClientError> {
        self.empty(
            Method::SESSION_RESIZE,
            &ResizeParams {
                session_id: session_id.clone(),
                cols,
                rows,
            },
        )
        .await
    }

    pub async fn read_screen(
        &self,
        session_id: &SessionId,
    ) -> Result<ReadScreenResult, ClientError> {
        self.typed(Method::SESSION_READ_SCREEN, &session_params(session_id))
            .await
    }

    pub async fn read_scrollback(
        &self,
        session_id: &SessionId,
    ) -> Result<ReadScrollbackResult, ClientError> {
        self.typed(Method::SESSION_READ_SCROLLBACK, &session_params(session_id))
            .await
    }

    pub async fn read_scrollback_cells(
        &self,
        session_id: &SessionId,
        first_row: i64,
        max_rows: i64,
    ) -> Result<ReadScrollbackCellsResult, ClientError> {
        self.typed(
            Method::SESSION_READ_SCROLLBACK_CELLS,
            &ReadScrollbackCellsParams {
                session_id: session_id.clone(),
                first_row,
                max_rows,
            },
        )
        .await
    }

    pub async fn mark_seen(&self, session_id: &SessionId) -> Result<(), ClientError> {
        self.empty(Method::SESSION_MARK_SEEN, &session_params(session_id))
            .await
    }

    pub async fn read_diff(
        &self,
        session_id: &SessionId,
        base: SessionDiffBase,
    ) -> Result<SessionReadDiffResult, ClientError> {
        self.typed(
            Method::SESSION_READ_DIFF,
            &SessionReadDiffParams {
                session_id: session_id.clone(),
                base: Some(base),
            },
        )
        .await
    }

    pub async fn set_active(&self, active: bool) -> Result<(), ClientError> {
        self.empty(Method::CLIENT_SET_ACTIVE, &ClientActiveParams { active })
            .await
    }

    pub async fn configure_governor(
        &self,
        params: GovernorConfigureParams,
    ) -> Result<(), ClientError> {
        self.empty(Method::GOVERNOR_CONFIGURE, &params).await
    }

    /// `agent.readiness`: PATH metadata locally, but a remote target crosses
    /// ssh and may install the Helper first, so the two carry very different
    /// bounds. Both are explicit: the settings page shows a spinner for the
    /// whole call, and an unbounded one has no way back to the user.
    pub async fn agent_readiness(
        &self,
        params: diri_proto::AgentReadinessParams,
    ) -> Result<AgentReadinessResult, ClientError> {
        let timeout = agent_catalog_timeout(params.host.is_some());
        self.core
            .request_typed(Method::AGENT_READINESS, Some(&params), Some(timeout))
            .await
    }

    /// `agent.configure`: writes the preference, then answers with the same
    /// catalog `agent.readiness` builds — and can therefore scan.
    pub async fn configure_agent(
        &self,
        params: diri_proto::AgentConfigureParams,
    ) -> Result<diri_proto::AgentConfigureResult, ClientError> {
        let timeout = agent_catalog_timeout(params.host.is_some());
        self.core
            .request_typed(Method::AGENT_CONFIGURE, Some(&params), Some(timeout))
            .await
    }

    pub async fn hibernate(&self, session_id: &SessionId) -> Result<(), ClientError> {
        self.empty(Method::SESSION_HIBERNATE, &session_params(session_id))
            .await
    }

    pub async fn wake(&self, session_id: &SessionId) -> Result<(), ClientError> {
        self.empty(Method::SESSION_WAKE, &session_params(session_id))
            .await
    }

    pub async fn archive(&self, session_id: &SessionId) -> Result<(), ClientError> {
        self.empty(Method::SESSION_ARCHIVE, &session_params(session_id))
            .await
    }

    pub async fn unarchive(&self, session_id: &SessionId) -> Result<(), ClientError> {
        self.empty(Method::SESSION_UNARCHIVE, &session_params(session_id))
            .await
    }

    pub async fn worktree_create(
        &self,
        params: WorktreeCreateParams,
    ) -> Result<WorktreeInfo, ClientError> {
        self.typed(Method::WORKTREE_CREATE, &params).await
    }

    pub async fn worktree_list(
        &self,
        params: WorktreeListParams,
    ) -> Result<Vec<WorktreeInfo>, ClientError> {
        self.typed(Method::WORKTREE_LIST, &params).await
    }

    pub async fn worktree_remove(&self, params: WorktreeRemoveParams) -> Result<(), ClientError> {
        self.empty(Method::WORKTREE_REMOVE, &params).await
    }

    pub async fn worktree_overview(&self) -> Result<Vec<WorktreeOverviewEntry>, ClientError> {
        let result: WorktreeOverviewResult = self.no_params(Method::WORKTREE_OVERVIEW).await?;
        Ok(result.entries)
    }

    pub async fn history(&self) -> Result<SessionHistoryResult, ClientError> {
        self.no_params(Method::SESSION_HISTORY).await
    }

    pub async fn resume_from_history(
        &self,
        entry: HistoryEntry,
    ) -> Result<SessionRecord, ClientError> {
        self.resume_from_history_with_prompt(entry, None).await
    }

    pub async fn resume_from_history_with_prompt(
        &self,
        entry: HistoryEntry,
        initial_prompt: Option<String>,
    ) -> Result<SessionRecord, ClientError> {
        self.typed(
            Method::SESSION_RESUME_FROM_HISTORY,
            &ResumeFromHistoryParams {
                entry,
                initial_prompt,
            },
        )
        .await
    }

    pub async fn reopen_last(&self) -> Result<SessionRecord, ClientError> {
        self.no_params(Method::SESSION_REOPEN_LAST).await
    }

    pub async fn events_wait(
        &self,
        params: EventsWaitParams,
    ) -> Result<EventsWaitResult, ClientError> {
        let timeout_ms = params.timeout_ms.max(0) as u64;
        self.core
            .request_typed(
                Method::EVENTS_WAIT,
                Some(&params),
                Some(Duration::from_millis(timeout_ms.saturating_add(10_000))),
            )
            .await
    }

    pub async fn project_add(&self, root: String) -> Result<Project, ClientError> {
        self.typed(Method::PROJECT_ADD, &ProjectAddParams { root })
            .await
    }

    pub async fn hook_report(&self, params: HookReportParams) -> Result<(), ClientError> {
        self.empty(Method::HOOK_REPORT, &params).await
    }

    pub async fn test_run(&self, params: TestRunParams) -> Result<JsonValue, ClientError> {
        self.typed(Method::TEST_RUN, &params).await
    }

    pub async fn state_snapshot(&self) -> Result<StateSnapshotResult, ClientError> {
        self.no_params(Method::STATE_SNAPSHOT).await
    }

    /// Releases an App-owned Engine only when the Engine itself confirms that
    /// no live session and no other control client still needs it.
    pub async fn shutdown_daemon_if_idle(&self) -> Result<DaemonShutdownIfIdleResult, ClientError> {
        self.no_params(Method::DAEMON_SHUTDOWN_IF_IDLE).await
    }

    async fn typed<P, R>(&self, method: &str, params: &P) -> Result<R, ClientError>
    where
        P: Serialize + ?Sized,
        R: DeserializeOwned,
    {
        self.core.request_typed(method, Some(params), None).await
    }

    async fn no_params<R: DeserializeOwned>(&self, method: &str) -> Result<R, ClientError> {
        self.core
            .request_typed::<EmptyParams, R>(method, None, None)
            .await
    }

    async fn empty<P: Serialize + ?Sized>(
        &self,
        method: &str,
        params: &P,
    ) -> Result<(), ClientError> {
        let _: EmptyResult = self.typed(method, params).await?;
        Ok(())
    }
}

impl Drop for DaemonClient {
    fn drop(&mut self) {
        self.begin_shutdown();
        if let Ok(slot) = self.lifecycle.get_mut()
            && let Some(task) = slot.take()
        {
            task.abort();
        }
    }
}

impl ConnectionState {
    fn is_connected(&self) -> bool {
        matches!(self, Self::Connected(_))
    }
}

const fn agent_catalog_timeout(remote: bool) -> Duration {
    if remote {
        REMOTE_AGENT_CATALOG_TIMEOUT
    } else {
        AGENT_CATALOG_TIMEOUT
    }
}

fn session_params(session_id: &SessionId) -> SessionIdParams {
    SessionIdParams {
        session_id: session_id.clone(),
    }
}

struct AttemptOutcome {
    error: ClientError,
    established: bool,
}

async fn run_lifecycle(core: Arc<ClientCore>) {
    let mut backoff = INITIAL_BACKOFF;
    let mut shutdown = core.shutdown_tx.subscribe();
    let mut retries = core.retry_tx.subscribe();
    while !*shutdown.borrow() {
        core.set_state(ConnectionState::Connecting);
        let outcome = run_once(Arc::clone(&core), &mut shutdown).await;
        *core.writer.write().await = None;
        core.events_subscribed.store(false, Ordering::Release);
        core.fail_pending(outcome.error.clone()).await;
        core.set_state(ConnectionState::Disconnected(outcome.error.to_string()));
        if *shutdown.borrow() {
            break;
        }
        if outcome.established {
            backoff = INITIAL_BACKOFF;
        }
        tokio::select! {
            () = tokio::time::sleep(backoff) => {}
            result = retries.changed() => {
                if result.is_err() {
                    break;
                }
            }
            result = shutdown.changed() => {
                if result.is_err() || *shutdown.borrow() {
                    break;
                }
            }
        }
        backoff = backoff.saturating_mul(2).min(MAX_BACKOFF);
    }
}

async fn run_once(core: Arc<ClientCore>, shutdown: &mut watch::Receiver<bool>) -> AttemptOutcome {
    let mut connection = match ActiveConnection::open(&core.socket_path, Arc::clone(&core)).await {
        Ok(connection) => connection,
        Err(error) => {
            return AttemptOutcome {
                error,
                established: false,
            };
        }
    };
    *core.writer.write().await = Some(connection.sender());

    let hello = tokio::select! {
        result = core.hello(Some(HEARTBEAT_TIMEOUT)) => result,
        error = connection.closed() => Err(error),
        _ = shutdown.changed() => Err(ClientError::disconnected("client shut down")),
    };
    let hello = match hello {
        Ok(hello) => hello,
        Err(error) => {
            return AttemptOutcome {
                error,
                established: false,
            };
        }
    };
    if hello.engine_kind.as_deref() != Some(diri_proto::RUST_ENGINE_KIND) {
        return AttemptOutcome {
            error: ClientError::protocol(
                "the daemon socket is not owned by the authoritative Rust Engine",
            ),
            established: false,
        };
    }

    if core.want_events.load(Ordering::Acquire) {
        let subscribed = tokio::select! {
            result = core.subscribe_to_events() => result.map(|_| ()),
            error = connection.closed() => Err(error),
            _ = shutdown.changed() => Err(ClientError::disconnected("client shut down")),
        };
        if let Err(error) = subscribed {
            return AttemptOutcome {
                error,
                established: true,
            };
        }
    }
    core.set_state(ConnectionState::Connected(hello));

    loop {
        tokio::select! {
            () = tokio::time::sleep(HEARTBEAT_INTERVAL) => {}
            error = connection.closed() => {
                return AttemptOutcome { error, established: true };
            }
            _ = shutdown.changed() => {
                return AttemptOutcome {
                    error: ClientError::disconnected("client shut down"),
                    established: true,
                };
            }
        }

        let heartbeat = tokio::select! {
            result = core.hello(Some(HEARTBEAT_TIMEOUT)) => result.map(|_| ()),
            error = connection.closed() => Err(error),
            _ = shutdown.changed() => Err(ClientError::disconnected("client shut down")),
        };
        if let Err(error) = heartbeat {
            return AttemptOutcome {
                error,
                established: true,
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use diri_proto::methods::EventName;
    use diri_proto::model::AgentKind;
    use std::error::Error;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn begin_shutdown_publishes_without_an_async_runtime_turn() {
        let client = DaemonClient::with_socket_path("/nonexistent/diri-test.sock");
        let shutdown = client.core.shutdown_tx.subscribe();

        client.begin_shutdown();

        assert!(*shutdown.borrow());
    }

    #[tokio::test]
    async fn retry_now_wakes_the_lifecycle_signal_without_spawning_a_daemon() {
        let client = DaemonClient::with_socket_path("/nonexistent/diri-test.sock");
        let mut retries = client.core.retry_tx.subscribe();

        client.retry_now();

        retries.changed().await.expect("retry sender remains alive");
        assert_eq!(*retries.borrow(), 1);
        assert!(
            client
                .lifecycle
                .lock()
                .expect("lifecycle mutex poisoned")
                .is_none()
        );
    }

    #[tokio::test]
    async fn live_daemon_control_round_trip() -> Result<(), Box<dyn Error>> {
        if std::env::var_os("DIRI_RUN_MUTATING_DAEMON_TESTS").is_none() {
            eprintln!(
                "skipping mutating live daemon test; set DIRI_RUN_MUTATING_DAEMON_TESTS=1 to opt in"
            );
            return Ok(());
        }
        let Some(home) = std::env::var_os("HOME") else {
            eprintln!("skipping live daemon test: HOME is unset");
            return Ok(());
        };
        let socket = DirijorPaths::socket(home);
        if !socket.exists() {
            eprintln!(
                "skipping live daemon test: {} does not exist",
                socket.display()
            );
            return Ok(());
        }

        let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let scratch = std::env::temp_dir().join(format!(
            "diri-client-live-test-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir(&scratch)?;

        let client = DaemonClient::with_socket_path(socket);
        client.connect();
        let test_result = async {
            client.wait_until_connected(Duration::from_secs(10)).await?;
            let hello = client.hello().await?;
            if hello.proto != diri_proto::control::WIRE_VERSION {
                return Err(ClientError::protocol("hello returned the wrong protocol"));
            }
            client.sessions().await?;
            let mut events = client.subscribe_events().await?;

            let spawned_id = client
                .spawn(SessionSpawnParams {
                    kind: AgentKind::SHELL,
                    cwd: scratch.to_string_lossy().into_owned(),
                    new_worktree: None,
                    worktree_branch: None,
                    title: Some("diri-client integration scratch".to_owned()),
                    initial_prompt: None,
                    parent: None,
                    initial_cols: Some(80),
                    initial_rows: Some(24),
                    host: None,
                    same_repo_as: None,
                })
                .await?;

            let verification = verify_spawn(&client, &mut events, &spawned_id).await;

            // HARD SAFETY RULE: these are the only destructive calls in this test,
            // and both use the exact ID returned by this run's spawn response.
            let kill_result = client.kill(&spawned_id).await;
            let remove_result = client.remove(&spawned_id).await;
            let removal_result = verify_removed(&client, &spawned_id).await;

            verification?;
            kill_result?;
            remove_result?;
            removal_result?;
            Ok::<(), ClientError>(())
        }
        .await;

        client.shutdown().await;
        fs::remove_dir_all(&scratch)?;
        test_result.map_err(Into::into)
    }

    async fn verify_spawn(
        client: &DaemonClient,
        events: &mut broadcast::Receiver<EventEnvelope>,
        spawned_id: &SessionId,
    ) -> Result<(), ClientError> {
        let listed = client
            .sessions()
            .await?
            .sessions
            .iter()
            .any(|session| session.id == *spawned_id);
        if !listed {
            return Err(ClientError::protocol(
                "spawned scratch session was absent from session.list",
            ));
        }

        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let event = events.recv().await.map_err(|error| {
                    ClientError::protocol(format!("event stream receive failed: {error}"))
                })?;
                if event.name != EventName::SESSION_UPDATED {
                    continue;
                }
                let record: SessionRecord =
                    serde_json::from_value(event.params.clone()).map_err(ClientError::json)?;
                if record.id == *spawned_id {
                    return Ok::<(), ClientError>(());
                }
            }
        })
        .await
        .map_err(|_| ClientError::Timeout("waiting for spawned session event".to_owned()))?
    }

    async fn verify_removed(
        client: &DaemonClient,
        spawned_id: &SessionId,
    ) -> Result<(), ClientError> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let present = client
                .sessions()
                .await?
                .sessions
                .iter()
                .any(|session| session.id == *spawned_id);
            if !present {
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(ClientError::Timeout(
                    "waiting for scratch session removal".to_owned(),
                ));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}
