//! A live session: one child process on a PTY, watched.
//!
//! This is where the previous layers meet. A session appends everything the
//! child writes to its [`OutputLog`], feeds the same bytes to a
//! [`HeadlessScreen`], evaluates the screen against the agent's manifest, and
//! folds the result through a [`StatusReducer`]. The current status and the
//! output log are what everything else in the product reads.
//!
//! Who owns the PTY is a transport choice. A *direct* session owns it in
//! process — simple, and gone when this process is. A *held* session's PTY
//! belongs to a holder (see [`crate::holder`]): the session is then only a
//! client and a log tail, and the child survives this process dying. Held is
//! what the daemon uses; direct remains for tests and embedded callers.
//!
//! The pump runs on its own thread rather than the async runtime, because the
//! PTY read is a blocking syscall — the same reasoning that moved the test
//! servers off the cooperative pool earlier tonight.

use std::io::Read;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use diri_proto::frames::FrameType;
use diri_proto::remote_pty::{
    FullSnapshot, GridDelta, LaunchRequest, ProcessExit, RemoteCodec, RemoteMessage,
    RemoteProcessState,
};
use diri_proto::terminal::MouseModes;
use diri_proto::{NeedsInputDetail, SessionStatus};
use diri_terminal_state::GridMirror;

use crate::detect::ManifestEngine;
use crate::holder::{
    HolderClient, HolderExitMarker, HolderExitStatus, HolderLaunchSpec, HolderLauncher,
    HolderPaths, HolderStat,
};
use crate::log::OutputLog;
use crate::pty::{Exit, Pty, PtySpec};
use crate::remote::binding::{RemoteBinding, RemoteBindingStore};
use crate::remote::client::RemoteSessionClient;
use crate::remote::manager::{InstalledHelper, RemoteManager};
use crate::screen::HeadlessScreen;
use crate::status::{Authority, ClaudeHook, ReducerOutcome, StatusReducer, StatusSignal};

/// How often the pump ticks when the child is quiet, so debounce timers still
/// advance and staleness is noticed.
const TICK_INTERVAL: Duration = Duration::from_millis(100);

/// Quiet-tick interval for a session that is neither attached, recently
/// touched, nor Working. Reducer ticks are no-ops outside Working, so with 30
/// idle background sessions this is the difference between ~300 wakeups plus
/// ~900 log syscalls a second and ~30.
const IDLE_TICK_INTERVAL: Duration = Duration::from_secs(1);

/// How long an attach poll or input write keeps a session on the fast tick.
const HOT_WINDOW_SECS: u64 = 30;

/// Maximum raw log tail replayed when starting or adopting a held session.
/// The same hard startup-work bound the Swift daemon enforced.
const REPLAY_BUDGET: usize = 256 << 10;
const MAX_REPLAY_BUDGET: usize = 32 << 20;

/// The normal startup bound is intentionally small, but an old checkpoint
/// format can require a one-time raw-log migration to rebuild scrollback.
/// Operators can raise the bound for that restart without changing the
/// steady-state cost; malformed values fall back to the default.
fn replay_budget() -> usize {
    std::env::var("DIRIJOR_REPLAY_BUDGET_BYTES")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .map_or(REPLAY_BUDGET, |value| {
            value.clamp(REPLAY_BUDGET, MAX_REPLAY_BUDGET)
        })
}

/// Quiet time after the last output before a screen checkpoint is written,
/// the Swift daemon's `checkpointSettleDelay`. Bursts coalesce into one
/// write; an idle screen is checkpointed within about a second.
const CHECKPOINT_SETTLE: Duration = Duration::from_secs(1);

/// How long a deferred spawn waits for the first client size before
/// launching at the estimated size anyway — an MCP-spawned agent may never
/// get a view. The Swift daemon's 400ms fallback window.
const LAUNCH_FALLBACK: Duration = Duration::from_millis(400);

/// While unlaunched, each client resize pushes the exec back this far, so
/// the agent starts at the SETTLED viewport rather than a transient
/// first-layout size — otherwise its one-shot banner bakes at the wrong
/// width. The Swift daemon's `scheduleDebouncedLaunch` delay.
const LAUNCH_DEBOUNCE: Duration = Duration::from_millis(120);

/// Quiet time between holder liveness probes: a holder that died markerless
/// (SIGKILL, machine issues) must not leave a forever-live session behind.
/// Elapsed-based so the probe cadence is the same on fast and idle ticks.
const LIVENESS_INTERVAL: Duration = Duration::from_secs(2);

/// How long a half-erased screen waits for the rest of its repaint.
///
/// A TUI repaint is not one write. Ink, Ratatui and friends erase the old
/// frame and draw the new one in separate `write`s, and a PTY hands each one
/// over the instant it lands — so a reader that publishes per read publishes
/// the half-erased screen in between, which is the flash users see as
/// flickering. A terminal emulator never shows that state because its reader
/// drains everything queued before it renders. Draining alone is not enough
/// here: the daemon is usually parked in `poll` and wakes on the *first* write
/// of a repaint, with the rest usually microseconds behind but occasionally a
/// scheduler quantum behind on a loaded machine. This is the quiet window that
/// lets the rest arrive. It is only ever waited out by a screen that just lost
/// content, so typed echo and additive scrolling are not slowed by it. The
/// wait returns as soon as bytes arrive; 16 ms is only its worst-case ceiling.
const OUTPUT_SETTLE: Duration = Duration::from_millis(16);

/// Ceiling on how long one batch may hold back a repaint. Output that never
/// goes quiet (a build log) would otherwise wait on `OUTPUT_SETTLE` forever.
/// One 120 Hz display interval, so continuous streaming cannot be held longer
/// than the renderer's own frame cadence.
const OUTPUT_BATCH_CEILING: Duration = Duration::from_millis(8);

/// A destructive repaint gets one 60 Hz interval to recover from an erase.
/// This is deliberately separate from [`OUTPUT_BATCH_CEILING`], so a build log
/// that continuously adds content still publishes at up to 120 Hz.
const OUTPUT_REPAINT_CEILING: Duration = Duration::from_millis(16);

/// An entirely blank intermediate frame gets a little more grace than a
/// partial repaint. A loaded machine can deschedule a TUI between its clear
/// and redraw writes for longer than one display frame; publishing that state
/// is the most visible possible flash. Keep enough headroom beyond a typical
/// 30 ms scheduler pause for the resumed process to issue its redraw; a
/// program that intentionally clears and stops still appears within the
/// sub-100 ms perceptual-continuity bound.
const OUTPUT_BLANK_REPAINT_CEILING: Duration = Duration::from_millis(80);

/// Byte ceiling on one batch, matching `alacritty_terminal`'s
/// `MAX_LOCKED_READ`. A child that writes faster than it can be parsed must
/// not starve the grid indefinitely.
const OUTPUT_BATCH_BYTES: usize = 1 << 20;

/// How much of a held session's log the follow pump reads per pass. A full
/// read means more is already waiting, which is how that pump recognizes a
/// repaint it has only half consumed.
const LOG_READ_BUDGET: usize = 64 << 10;

/// What a session looks like from the outside.
#[derive(Clone, Debug)]
pub struct SessionView {
    pub id: String,
    pub status: SessionStatus,
    pub status_evidence: Option<diri_proto::StatusEvidence>,
    pub needs_input: Option<NeedsInputDetail>,
    pub title: Option<String>,
    pub title_source: Option<diri_proto::TitleSource>,
    pub tail_offset: u64,
    pub exited: bool,
}

/// Small input-side composer mirror used only until the first real prompt is
/// submitted. It avoids parsing an Agent's rendered screen or reading remote
/// transcript files, and disappears from the hot path after the title exists.
#[derive(Default)]
struct PromptInputState {
    draft: String,
}

impl PromptInputState {
    fn observe(&mut self, bytes: &[u8]) -> Option<String> {
        if matches!(bytes, b"\r" | b"\n") {
            let prompt = std::mem::take(&mut self.draft);
            return (!prompt.trim().is_empty()).then_some(prompt);
        }
        if bytes == [0x7f] || bytes == [0x08] {
            self.draft.pop();
            return None;
        }
        if bytes == [0x15] {
            self.draft.clear();
            return None;
        }
        if bytes == [0x17] {
            while self.draft.ends_with(char::is_whitespace) {
                self.draft.pop();
            }
            while self
                .draft
                .chars()
                .last()
                .is_some_and(|c| !c.is_whitespace())
            {
                self.draft.pop();
            }
            return None;
        }

        let bytes = bytes
            .strip_prefix(b"\x1b[200~")
            .and_then(|bytes| bytes.strip_suffix(b"\x1b[201~"))
            .unwrap_or(bytes);
        if bytes.iter().any(|byte| *byte == 0x1b || *byte < 0x09)
            || bytes.iter().any(|byte| (0x0e..0x20).contains(byte))
        {
            return None;
        }
        if let Ok(text) = std::str::from_utf8(bytes) {
            self.draft.push_str(text);
        }
        None
    }
}

/// The state the pump thread and the outside world share.
struct Shared {
    id: String,
    status: Mutex<SessionStatus>,
    needs_input: Mutex<Option<NeedsInputDetail>>,
    title: Mutex<Option<String>>,
    prompt_title: Mutex<Option<String>>,
    prompt_input: Mutex<PromptInputState>,
    log: Mutex<OutputLog>,
    screen: Mutex<HeadlessScreen>,
    reducer: Mutex<StatusReducer>,
    /// How the child ended, once known (from `wait` or the exit marker).
    exit: Mutex<Option<Exit>>,
    exited: AtomicBool,
    stop: AtomicBool,
    /// Bumped whenever status, needs-input, or title actually change. The
    /// registry watcher compares this instead of cloning and JSON-serializing
    /// every record on every poll.
    state_version: AtomicU64,
    /// Seconds since UNIX_EPOCH of the last attach-pump poll or input write.
    /// Keeps interactive sessions on the fast quiet-tick.
    last_hot: AtomicU64,
    /// Unlike `last_hot`, starts cold: restored/background sessions should not
    /// receive interactive scheduler priority until actually attached or used.
    last_interaction: AtomicU64,
    /// URLs scanned off the visible screen (PRs, previews, links).
    artifacts: Mutex<Vec<diri_proto::SessionArtifact>>,
    /// True while the child tree is SIGSTOPped. Writing into a stopped
    /// tree's PTY wedges (nobody drains the slave; the buffer fills), so
    /// input is queued instead and flushed right after SIGCONT.
    hibernated: AtomicBool,
    /// Input received while hibernated, in arrival order.
    queued_input: Mutex<Vec<u8>>,
    /// The child's pid, for tree enumeration by the resource governor.
    child_pid: std::sync::atomic::AtomicI32,
    /// The remote Holder's grid is display-authoritative. Raw output still
    /// feeds `screen` for local status reduction and artifact detection.
    remote_grid: Mutex<Option<RemoteGridState>>,
    remote_output_offset: AtomicU64,
    grid_wake: GridWake,
}

struct RemoteGridState {
    mirror: GridMirror,
    revision: u64,
    pending: Option<diri_proto::grid::GridUpdate>,
}

impl Shared {
    fn bump_state_version(&self) {
        self.state_version.fetch_add(1, Ordering::SeqCst);
    }

    fn note_hot(&self) {
        let now = unix_secs();
        self.last_hot.store(now, Ordering::Relaxed);
        self.last_interaction.store(now, Ordering::Relaxed);
    }

    /// Fast quiet-tick while the session is attached/touched or Working;
    /// everything else can wait a second.
    fn wants_fast_tick(&self) -> bool {
        if self.was_recently_touched() {
            return true;
        }
        matches!(
            *self.status.lock().expect("status"),
            SessionStatus::Working | SessionStatus::Starting
        )
    }

    fn was_recently_touched(&self) -> bool {
        let last = self.last_interaction.load(Ordering::Relaxed);
        last != 0 && unix_secs().saturating_sub(last) <= HOT_WINDOW_SECS
    }

    fn quiet_tick(&self) -> Duration {
        if self.wants_fast_tick() {
            TICK_INTERVAL
        } else {
            IDLE_TICK_INTERVAL
        }
    }
}

fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// What the grid pump compares between ticks to decide whether anything
/// observable changed. Default is "never seen anything".
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct GridSignature {
    pub content_seq: u64,
    pub size: (usize, usize),
    pub cursor: (u16, u16, bool),
    pub alt_screen: bool,
    pub mouse: MouseModes,
}

/// Event source for the attachment writer. PTY readers advance it only after
/// the authoritative grid changes, so a quiet attached terminal has no
/// frame-rate polling cost.
#[derive(Clone)]
pub(crate) struct GridWake {
    inner: Arc<GridWakeInner>,
}

struct GridWakeInner {
    state: Mutex<GridWakeState>,
    changed: Condvar,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct GridWakeEvent {
    pub generation: u64,
    pub interactive: bool,
}

struct GridWakeState {
    generation: u64,
    interactive_budget: u8,
}

const INTERACTIVE_GRID_BUDGET: u8 = 2;

impl GridWake {
    fn new() -> Self {
        Self {
            inner: Arc::new(GridWakeInner {
                state: Mutex::new(GridWakeState {
                    generation: 0,
                    interactive_budget: 0,
                }),
                changed: Condvar::new(),
            }),
        }
    }

    fn notify(&self) {
        let mut state = self.inner.state.lock().expect("grid wake");
        state.generation = state.generation.saturating_add(1);
        self.inner.changed.notify_all();
    }

    fn prioritize_interactive_changes(&self) {
        let mut state = self.inner.state.lock().expect("grid wake");
        state.interactive_budget = INTERACTIVE_GRID_BUDGET;
        self.inner.changed.notify_all();
    }

    pub(crate) fn consume_interactive_priority(&self) {
        let mut state = self.inner.state.lock().expect("grid wake");
        state.interactive_budget = state.interactive_budget.saturating_sub(1);
    }

    pub(crate) fn generation(&self) -> u64 {
        self.inner.state.lock().expect("grid wake").generation
    }

    pub(crate) fn wait_for_change(&self, observed: u64, timeout: Duration) -> GridWakeEvent {
        let state = self.inner.state.lock().expect("grid wake");
        if state.generation != observed {
            return grid_wake_event(&state, observed);
        }
        let (state, _) = self
            .inner
            .changed
            .wait_timeout_while(state, timeout, |state| state.generation == observed)
            .expect("grid wake");
        grid_wake_event(&state, observed)
    }

    pub(crate) fn wait_for_priority_or_timeout(
        &self,
        observed: u64,
        timeout: Duration,
    ) -> GridWakeEvent {
        let state = self.inner.state.lock().expect("grid wake");
        let (state, _) = self
            .inner
            .changed
            .wait_timeout_while(state, timeout, |state| {
                state.interactive_budget == 0 || state.generation == observed
            })
            .expect("grid wake");
        grid_wake_event(&state, observed)
    }

    pub(crate) fn same_source(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }
}

fn grid_wake_event(state: &GridWakeState, observed: u64) -> GridWakeEvent {
    GridWakeEvent {
        generation: state.generation,
        interactive: state.interactive_budget > 0 && state.generation != observed,
    }
}

pub(crate) struct AttachmentSeed {
    pub grid: diri_proto::grid::GridUpdate,
    pub modes: (bool, MouseModes),
    pub signature: GridSignature,
    pub wake: GridWake,
    pub wake_generation: u64,
}

/// Who owns the PTY.
enum Transport {
    /// This process does; dropping the session kills the child.
    Direct(Arc<Mutex<Pty>>),
    /// A holder process does; this session is a socket client and a log
    /// tail, and the child outlives it.
    Held(HolderClient),
    /// A remote Holder owns the PTY. Dropping this transport closes only the
    /// SSH Bridge; explicit termination is the only path that kills Agent.
    Remote(Arc<RemoteSessionClient>),
}

pub struct Session {
    shared: Arc<Shared>,
    transport: Transport,
    pump: Option<JoinHandle<()>>,
    manifest_id: String,
    /// Present while the exec is deferred to the first settled client size.
    deferred: Option<Arc<DeferredLaunch>>,
}

/// Deferred-launch state: the agent is not exec'd until the attaching client
/// reports its real terminal size, so a TUI's one-shot banner renders at the
/// exact width (no post-spawn reflow). Ported from the Swift daemon's
/// `scheduleDebouncedLaunch`.
struct DeferredLaunch {
    state: Mutex<DeferredState>,
    cond: std::sync::Condvar,
}

/// What [`DeferredLaunch::finish_launch`] hands back: the input queued while
/// unlaunched, and a size proposed after the launch size was taken.
struct LaunchHandoff {
    queued_input: Vec<u8>,
    late_size: Option<(u16, u16)>,
}

struct DeferredState {
    /// The latest client-proposed size, if any arrived before launch.
    pending: Option<(u16, u16)>,
    /// When the launch fires: pushed back by each new size proposal.
    deadline: Instant,
    /// Input typed before the child exists, flushed right after exec.
    queued_input: Vec<u8>,
    launched: bool,
    cancelled: bool,
}

impl DeferredLaunch {
    fn new() -> Self {
        Self {
            state: Mutex::new(DeferredState {
                pending: None,
                deadline: Instant::now() + LAUNCH_FALLBACK,
                queued_input: Vec::new(),
                launched: false,
                cancelled: false,
            }),
            cond: std::sync::Condvar::new(),
        }
    }

    /// Records a client size while unlaunched, pushing the exec back so the
    /// viewport can settle. False once launched: resize the PTY instead.
    fn propose_size(&self, cols: u16, rows: u16) -> bool {
        let mut state = self.state.lock().expect("deferred");
        if state.launched {
            return false;
        }
        state.pending = Some((cols, rows));
        state.deadline = Instant::now() + LAUNCH_DEBOUNCE;
        self.cond.notify_all();
        true
    }

    /// Queues input while unlaunched. False once launched: write through.
    fn queue_input(&self, bytes: &[u8]) -> bool {
        let mut state = self.state.lock().expect("deferred");
        if state.launched {
            return false;
        }
        state.queued_input.extend_from_slice(bytes);
        true
    }

    /// Blocks until the debounce window closes and returns the launch size;
    /// `None` when the session was cancelled before ever launching.
    fn wait_for_launch_size(&self, fallback: (u16, u16)) -> Option<(u16, u16)> {
        let mut state = self.state.lock().expect("deferred");
        loop {
            if state.cancelled {
                return None;
            }
            let now = Instant::now();
            if now >= state.deadline {
                return Some(state.pending.unwrap_or(fallback));
            }
            let wait = state.deadline - now;
            state = self.cond.wait_timeout(state, wait).expect("deferred").0;
        }
    }

    /// Marks the launch complete, handing back input queued meanwhile and a
    /// size proposed after `chosen` was taken (to apply as a normal resize).
    /// `None` when a cancel raced the launch: the caller owns the cleanup of
    /// the child it just started.
    fn finish_launch(&self, chosen: (u16, u16)) -> Option<LaunchHandoff> {
        let mut state = self.state.lock().expect("deferred");
        if state.cancelled {
            return None;
        }
        state.launched = true;
        Some(LaunchHandoff {
            queued_input: std::mem::take(&mut state.queued_input),
            late_size: state.pending.filter(|pending| *pending != chosen),
        })
    }

    /// True when cancellation happened before launch — there is no child.
    fn cancel(&self) -> bool {
        let mut state = self.state.lock().expect("deferred");
        if state.launched {
            return false;
        }
        state.cancelled = true;
        self.cond.notify_all();
        true
    }
}

/// Where holders live and what binary hosts them. Present on a spec, it makes
/// the spawn holder-backed.
#[derive(Clone, Debug)]
pub struct HolderConfig {
    pub holders_dir: PathBuf,
    pub executable: PathBuf,
}

/// Everything needed to launch one structured command through an installed
/// remote Helper. Secrets stay in Engine memory and are never written into a
/// public [`diri_proto::SessionRecord`].
#[derive(Clone)]
pub struct RemoteSessionSpec {
    pub manager: Arc<RemoteManager>,
    pub helper: InstalledHelper,
    pub launch: LaunchRequest,
    pub host_id: String,
    pub binding_store: RemoteBindingStore,
}

#[derive(Clone)]
pub struct RemoteAdoptSpec {
    pub manager: Arc<RemoteManager>,
    pub helper: InstalledHelper,
    pub token: diri_proto::remote_pty::SessionToken,
    pub incarnation: String,
    pub binding_store: RemoteBindingStore,
    pub output_offset: u64,
}

struct RemoteLaunchCleanup {
    manager: Arc<RemoteManager>,
    helper: InstalledHelper,
    binding_store: RemoteBindingStore,
    selector: diri_proto::remote_pty::SessionSelector,
    armed: bool,
}

impl RemoteLaunchCleanup {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for RemoteLaunchCleanup {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let _ = self.manager.kill(&self.helper, &self.selector);
        let _ = self.binding_store.remove(&self.selector.session_id);
    }
}

/// How to start a session.
pub struct SessionSpec {
    pub id: String,
    pub pty: PtySpec,
    /// Which manifest drives detection ("claude-code", "codex", …).
    pub manifest_id: String,
    pub authority: Authority,
    pub logs_dir: PathBuf,
    /// `Some` spawns through a holder so the child survives this process.
    pub holder: Option<HolderConfig>,
    /// Present for a remote Holder-backed session. It is mutually exclusive
    /// with the local `holder` transport.
    pub remote: Option<RemoteSessionSpec>,
    /// Defer the exec until the first client size settles (holder spawns
    /// only), so the agent's banner renders at the real viewport width.
    pub defer_launch: bool,
}

impl Session {
    /// Spawns the child and starts watching it — through a holder when the
    /// spec carries a [`HolderConfig`], directly otherwise.
    pub fn spawn(spec: SessionSpec, engine: Arc<ManifestEngine>) -> std::io::Result<Self> {
        if spec.remote.is_some() {
            return Self::spawn_remote(spec, engine);
        }
        match spec.holder.clone() {
            Some(holder) if spec.defer_launch => Self::spawn_held_deferred(spec, &holder, engine),
            Some(holder) => Self::spawn_held(spec, &holder, engine),
            None => Self::spawn_direct(spec, engine),
        }
    }

    fn spawn_remote(mut spec: SessionSpec, engine: Arc<ManifestEngine>) -> std::io::Result<Self> {
        if spec.holder.is_some() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "a session cannot use local and remote Holders together",
            ));
        }
        let remote = spec.remote.take().expect("checked");
        remote.launch.validate().map_err(std::io::Error::other)?;
        if remote.launch.session_id != spec.id {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "remote launch session id does not match SessionSpec",
            ));
        }
        let token = remote.launch.session_token.clone();
        let launched = remote.manager.launch(&remote.helper, &remote.launch)?;
        let mut cleanup = RemoteLaunchCleanup {
            manager: Arc::clone(&remote.manager),
            helper: remote.helper.clone(),
            binding_store: remote.binding_store.clone(),
            selector: diri_proto::remote_pty::SessionSelector {
                session_id: spec.id.clone(),
                session_token: token.clone(),
                expected_incarnation: Some(launched.session_incarnation.clone()),
            },
            armed: true,
        };
        if launched.session_id != spec.id {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "remote Helper launched the wrong session",
            ));
        }
        let binding = RemoteBinding {
            session_id: spec.id.clone(),
            host_id: remote.host_id,
            helper_build_id: remote.helper.build_id.clone(),
            protocol: remote.helper.protocol,
            session_token: token.clone(),
            session_incarnation: launched.session_incarnation.clone(),
            last_output_offset: 0,
        };
        remote.binding_store.save(&binding)?;
        let client = Arc::new(RemoteSessionClient::new(
            Arc::clone(&remote.manager),
            remote.helper,
            spec.id.clone(),
            token,
            launched.session_incarnation,
            remote.binding_store,
            0,
        ));
        let log = OutputLog::writer(&spec.logs_dir, &spec.id)?;
        let shared = new_shared(&spec, log, &engine);
        *shared.remote_grid.lock().expect("remote grid") = Some(RemoteGridState {
            mirror: GridMirror::new(),
            revision: 0,
            pending: None,
        });

        let pump = {
            let shared = Arc::clone(&shared);
            let engine = Arc::clone(&engine);
            let client = Arc::clone(&client);
            let manifest_id = spec.manifest_id.clone();
            std::thread::Builder::new()
                .name(format!("diri-remote-session-{}", spec.id))
                .spawn(move || pump_remote(shared, engine, client, manifest_id))?
        };

        let session = Self {
            shared,
            transport: Transport::Remote(client),
            pump: Some(pump),
            manifest_id: spec.manifest_id,
            deferred: None,
        };
        cleanup.disarm();
        Ok(session)
    }

    /// Reattaches an Engine restarted after the Holder was launched. The
    /// owner-only binding provides the bearer and exact Helper build.
    pub fn adopt_remote(
        spec: SessionSpec,
        remote: RemoteAdoptSpec,
        engine: Arc<ManifestEngine>,
    ) -> std::io::Result<Self> {
        Self::adopt_remote_with_status(spec, remote, engine, None)
    }

    /// Reattaches a remote Holder while retaining the last canonical status.
    /// The incoming Full Snapshot still updates the reducer; seeding prevents
    /// an Engine/App restart from presenting an already-idle Agent as a fresh
    /// launch during startup grace.
    pub fn adopt_remote_with_status(
        spec: SessionSpec,
        remote: RemoteAdoptSpec,
        engine: Arc<ManifestEngine>,
        initial_status: Option<(SessionStatus, Option<NeedsInputDetail>)>,
    ) -> std::io::Result<Self> {
        let client = Arc::new(RemoteSessionClient::new(
            remote.manager,
            remote.helper,
            spec.id.clone(),
            remote.token,
            remote.incarnation,
            remote.binding_store,
            remote.output_offset,
        ));
        let log = OutputLog::writer(&spec.logs_dir, &spec.id)?;
        let shared = new_shared(&spec, log, &engine);
        shared
            .remote_output_offset
            .store(remote.output_offset, Ordering::SeqCst);
        *shared.remote_grid.lock().expect("remote grid") = Some(RemoteGridState {
            mirror: GridMirror::new(),
            revision: 0,
            pending: None,
        });
        if let Some((status, needs_input)) = initial_status {
            *shared.status.lock().expect("status") = status;
            *shared.needs_input.lock().expect("needs input") = needs_input;
        }
        shared
            .reducer
            .lock()
            .expect("reducer")
            .finish_startup_grace(SystemTime::now());
        let pump = {
            let shared = Arc::clone(&shared);
            let engine = Arc::clone(&engine);
            let client = Arc::clone(&client);
            let manifest_id = spec.manifest_id.clone();
            std::thread::Builder::new()
                .name(format!("diri-remote-session-{}", spec.id))
                .spawn(move || pump_remote(shared, engine, client, manifest_id))?
        };
        Ok(Self {
            shared,
            transport: Transport::Remote(client),
            pump: Some(pump),
            manifest_id: spec.manifest_id,
            deferred: None,
        })
    }

    fn spawn_direct(spec: SessionSpec, engine: Arc<ManifestEngine>) -> std::io::Result<Self> {
        let pty = Pty::spawn(&spec.pty)?;
        let log = OutputLog::writer(&spec.logs_dir, &spec.id)?;
        let shared = new_shared(&spec, log, &engine);
        shared.child_pid.store(pty.pid() as i32, Ordering::SeqCst);

        let reader = pty.reader()?;
        let pty = Arc::new(Mutex::new(pty));

        let pump = {
            let shared = Arc::clone(&shared);
            let engine = Arc::clone(&engine);
            let pty = Arc::clone(&pty);
            let manifest_id = spec.manifest_id.clone();
            std::thread::Builder::new()
                .name(format!("diri-session-{}", spec.id))
                .spawn(move || pump(shared, engine, pty, reader, manifest_id))?
        };

        Ok(Self {
            shared,
            transport: Transport::Direct(pty),
            pump: Some(pump),
            manifest_id: spec.manifest_id,
            deferred: None,
        })
    }

    /// Spawns through the holder manager, so the child outlives this process.
    fn spawn_held(
        spec: SessionSpec,
        holder: &HolderConfig,
        engine: Arc<ManifestEngine>,
    ) -> std::io::Result<Self> {
        let paths = HolderPaths::new(&holder.holders_dir, &spec.id);
        // Incarnation-boundary fallback for pre-epoch holders: everything
        // already in the log predates the child about to spawn.
        let pre_spawn_tail = {
            let mut log = OutputLog::reader(&spec.logs_dir, &spec.id)?;
            log.refresh_from_disk();
            log.tail_offset()
        };
        let launch = HolderLaunchSpec {
            session_id: spec.id.clone(),
            socket_path: paths.socket().to_string_lossy().into_owned(),
            pid_file_path: paths.pid_file().to_string_lossy().into_owned(),
            log_file_path: spec
                .logs_dir
                .join(format!("{}.bin", spec.id))
                .to_string_lossy()
                .into_owned(),
            argv: spec.pty.argv.clone(),
            cwd: spec.pty.cwd.to_string_lossy().into_owned(),
            environment: spec.pty.env.iter().cloned().collect(),
            cols: spec.pty.cols.max(2),
            rows: spec.pty.rows.max(2),
            disk_capacity: crate::holder::protocol::DEFAULT_DISK_CAPACITY,
        };
        HolderLauncher::launch(&holder.executable, &paths, &launch).map_err(holder_io_error)?;

        let client = HolderClient::new(paths.socket());
        let floor = wait_for_holder(&client, &spec.logs_dir, &spec.id, pre_spawn_tail)
            .map_err(holder_io_error)?;
        Self::attach(spec, client, floor, engine)
    }

    /// Spawns through a holder, but not yet: the exec waits for the first
    /// client size to settle ([`LAUNCH_DEBOUNCE`] after each proposal, at
    /// most [`LAUNCH_FALLBACK`] total without one), so the agent's one-shot
    /// banner renders at the real viewport width. Until then input queues
    /// and the session presents an empty screen at the estimated size.
    fn spawn_held_deferred(
        spec: SessionSpec,
        holder: &HolderConfig,
        engine: Arc<ManifestEngine>,
    ) -> std::io::Result<Self> {
        let paths = HolderPaths::new(&holder.holders_dir, &spec.id);
        let client = HolderClient::new(paths.socket());
        let log = OutputLog::reader(&spec.logs_dir, &spec.id)?;
        let shared = new_shared(&spec, log, &engine);
        let deferred = Arc::new(DeferredLaunch::new());

        let pump = {
            let shared = Arc::clone(&shared);
            let engine = Arc::clone(&engine);
            let client = client.clone();
            let deferred = Arc::clone(&deferred);
            let holder = holder.clone();
            let manifest_id = spec.manifest_id.clone();
            let logs_dir = spec.logs_dir.clone();
            let id = spec.id.clone();
            let mut pty = spec.pty.clone();
            std::thread::Builder::new()
                .name(format!("diri-session-{}", spec.id))
                .spawn(move || {
                    let Some((cols, rows)) = deferred.wait_for_launch_size((pty.cols, pty.rows))
                    else {
                        return; // cancelled before ever launching
                    };
                    pty.cols = cols.max(2);
                    pty.rows = rows.max(2);
                    shared
                        .screen
                        .lock()
                        .expect("screen")
                        .resize(pty.cols as usize, pty.rows as usize);

                    let pre_spawn_tail = {
                        let mut log = shared.log.lock().expect("log");
                        log.refresh_from_disk();
                        log.tail_offset()
                    };
                    let launch = HolderLaunchSpec {
                        session_id: id.clone(),
                        socket_path: paths.socket().to_string_lossy().into_owned(),
                        pid_file_path: paths.pid_file().to_string_lossy().into_owned(),
                        log_file_path: logs_dir
                            .join(format!("{id}.bin"))
                            .to_string_lossy()
                            .into_owned(),
                        argv: pty.argv.clone(),
                        cwd: pty.cwd.to_string_lossy().into_owned(),
                        environment: pty.env.iter().cloned().collect(),
                        cols: pty.cols,
                        rows: pty.rows,
                        disk_capacity: crate::holder::protocol::DEFAULT_DISK_CAPACITY,
                    };
                    if HolderLauncher::launch(&holder.executable, &paths, &launch).is_err() {
                        mark_launch_failed(&shared);
                        return;
                    }
                    let Ok(floor) = wait_for_holder(&client, &logs_dir, &id, pre_spawn_tail) else {
                        mark_launch_failed(&shared);
                        return;
                    };
                    if let Ok(stat) = client.stat() {
                        shared.child_pid.store(stat.child_pid, Ordering::SeqCst);
                    }
                    let Some(handoff) = deferred.finish_launch((cols, rows)) else {
                        // A terminate raced the launch and believes there is
                        // no child; there is one now, so it goes with us.
                        let _ = client.kill_tree();
                        return;
                    };
                    if !handoff.queued_input.is_empty() {
                        let _ = client.write(&handoff.queued_input);
                    }
                    if let Some((cols, rows)) = handoff.late_size {
                        // A size proposed while the exec was in flight: apply
                        // as an ordinary resize now that the PTY exists.
                        let _ = client.resize(cols.max(2), rows.max(2));
                        shared
                            .screen
                            .lock()
                            .expect("screen")
                            .resize(cols.max(2) as usize, rows.max(2) as usize);
                    }
                    pump_held(shared, engine, client, floor, manifest_id)
                })?
        };

        Ok(Self {
            shared,
            transport: Transport::Held(client),
            pump: Some(pump),
            manifest_id: spec.manifest_id,
            deferred: Some(deferred),
        })
    }

    /// Reconstitutes a live session owned by a holder a previous daemon
    /// spawned. The holder must already be alive; `stat` is its current view.
    pub fn adopt(
        spec: SessionSpec,
        holder: &HolderConfig,
        stat: &HolderStat,
        engine: Arc<ManifestEngine>,
    ) -> std::io::Result<Self> {
        Self::adopt_with_status(spec, holder, stat, engine, None)
    }

    /// Adopt, seeding the visible status from the persisted record: a fresh
    /// reducer starts at Starting, and without evidence (a hook, a screen
    /// change) an adopted idle Claude would sit "starting" forever — the
    /// restart would rewrite history the record already knows.
    pub fn adopt_with_status(
        spec: SessionSpec,
        holder: &HolderConfig,
        stat: &HolderStat,
        engine: Arc<ManifestEngine>,
        initial_status: Option<(SessionStatus, Option<NeedsInputDetail>)>,
    ) -> std::io::Result<Self> {
        let paths = HolderPaths::new(&holder.holders_dir, &spec.id);
        let client = HolderClient::new(paths.socket());
        // Exit markers below the adopted holder's epoch were written by prior
        // incarnations of this session id — never by this child. Markers at
        // or above it (including one written while no daemon ran) apply.
        let floor = stat.epoch_offset.unwrap_or(0);
        let mut spec = spec;
        if let (Some(cols), Some(rows)) = (stat.cols, stat.rows) {
            spec.pty.cols = cols;
            spec.pty.rows = rows;
        }
        let session = Self::attach(spec, client, floor, engine)?;
        if let Some((status, needs_input)) = initial_status {
            *session.shared.status.lock().expect("status") = status;
            *session.shared.needs_input.lock().expect("needs input") = needs_input;
        }
        Ok(session)
    }

    /// The held-transport core: a read-only log tail drives the screen and
    /// reducer; the holder socket carries input, resize, and kill.
    fn attach(
        spec: SessionSpec,
        client: HolderClient,
        exit_marker_floor: u64,
        engine: Arc<ManifestEngine>,
    ) -> std::io::Result<Self> {
        let log = OutputLog::reader(&spec.logs_dir, &spec.id)?;
        let shared = new_shared(&spec, log, &engine);
        if let Ok(stat) = client.stat() {
            shared.child_pid.store(stat.child_pid, Ordering::SeqCst);
        }

        let pump = {
            let shared = Arc::clone(&shared);
            let engine = Arc::clone(&engine);
            let client = client.clone();
            let manifest_id = spec.manifest_id.clone();
            std::thread::Builder::new()
                .name(format!("diri-session-{}", spec.id))
                .spawn(move || pump_held(shared, engine, client, exit_marker_floor, manifest_id))?
        };

        Ok(Self {
            shared,
            transport: Transport::Held(client),
            pump: Some(pump),
            manifest_id: spec.manifest_id,
            deferred: None,
        })
    }

    pub fn id(&self) -> &str {
        &self.shared.id
    }

    pub fn manifest_id(&self) -> &str {
        &self.manifest_id
    }

    pub fn view(&self) -> SessionView {
        let prompt_title = self
            .shared
            .prompt_title
            .lock()
            .expect("prompt title")
            .clone();
        let (title, title_source) = if let Some(title) = prompt_title {
            (Some(title), Some(diri_proto::TitleSource::FirstPrompt))
        } else {
            (
                self.shared.title.lock().expect("title").clone(),
                Some(diri_proto::TitleSource::AgentProvided),
            )
        };
        SessionView {
            id: self.shared.id.clone(),
            status: self.shared.status.lock().expect("status").clone(),
            status_evidence: self
                .shared
                .reducer
                .lock()
                .expect("reducer")
                .evidence()
                .cloned(),
            needs_input: self.shared.needs_input.lock().expect("needs input").clone(),
            title,
            title_source,
            tail_offset: self.shared.log.lock().expect("log").tail_offset(),
            exited: self.shared.exited.load(Ordering::SeqCst),
        }
    }

    /// Monotonic counter that moves exactly when status, needs-input, or
    /// title change. Poll this before paying for [`Self::view`].
    pub fn state_version(&self) -> u64 {
        self.shared.state_version.load(Ordering::SeqCst)
    }

    pub fn status(&self) -> SessionStatus {
        self.shared.status.lock().expect("status").clone()
    }

    /// Reads recorded output by absolute stream offset, for attach and replay.
    pub fn read_output(&self, from_offset: u64, max_bytes: usize) -> (u64, Vec<u8>) {
        self.shared
            .log
            .lock()
            .expect("log")
            .read(from_offset, max_bytes)
    }

    /// The visible screen, as detection sees it.
    pub fn screen_lines(&self) -> Vec<String> {
        self.shared.screen.lock().expect("screen").lines()
    }

    /// The emulator's current geometry.
    /// URLs the screen has shown, for the artifacts inspector.
    pub fn artifacts(&self) -> Vec<diri_proto::SessionArtifact> {
        self.shared.artifacts.lock().expect("artifacts").clone()
    }

    /// The child's pid (0 before it is known), for tree enumeration.
    pub fn child_pid(&self) -> i32 {
        self.shared.child_pid.load(Ordering::SeqCst)
    }

    pub fn screen_size(&self) -> (usize, usize) {
        if let Some(remote) = self
            .shared
            .remote_grid
            .lock()
            .expect("remote grid")
            .as_ref()
            && remote.mirror.sequence().is_some()
        {
            let (cols, rows) = remote.mirror.size();
            return (usize::from(cols), usize::from(rows));
        }
        self.shared.screen.lock().expect("screen").size()
    }

    /// A coherent full snapshot and change-generation baseline for a freshly
    /// attached sink. Sampling the generation on both sides closes the race
    /// where output lands between the seed and pump registration.
    pub(crate) fn attachment_seed(&self) -> AttachmentSeed {
        self.shared.note_hot();
        let wake = self.shared.grid_wake.clone();
        loop {
            let wake_generation = wake.generation();
            let sampled = {
                let remote = self.shared.remote_grid.lock().expect("remote grid");
                remote.as_ref().and_then(|remote| {
                    let grid = remote.mirror.full_update()?;
                    let (cols, rows) = remote.mirror.size();
                    let (cursor_col, cursor_row, cursor_visible) = remote.mirror.cursor();
                    let (alt_screen, _, mouse) = remote.mirror.modes();
                    Some((
                        grid,
                        (alt_screen, mouse),
                        GridSignature {
                            content_seq: remote.revision,
                            size: (usize::from(cols), usize::from(rows)),
                            cursor: (cursor_col, cursor_row, cursor_visible),
                            alt_screen,
                            mouse,
                        },
                    ))
                })
            }
            .unwrap_or_else(|| {
                let screen = self.shared.screen.lock().expect("screen");
                (
                    screen.full_snapshot(),
                    (screen.is_alt_screen(), screen.mouse_modes()),
                    GridSignature {
                        content_seq: screen.content_seq(),
                        size: screen.size(),
                        cursor: screen.cursor(),
                        alt_screen: screen.is_alt_screen(),
                        mouse: screen.mouse_modes(),
                    },
                )
            });
            if wake.generation() == wake_generation {
                return AttachmentSeed {
                    grid: sampled.0,
                    modes: sampled.1,
                    signature: sampled.2,
                    wake,
                    wake_generation,
                };
            }
        }
    }

    /// The next grid diff after a [`GridWake`] notification, if anything
    /// observable changed since `signature`.
    pub fn grid_update_if_changed(
        &self,
        signature: &mut GridSignature,
    ) -> Option<diri_proto::grid::GridUpdate> {
        if let Some(remote) = self
            .shared
            .remote_grid
            .lock()
            .expect("remote grid")
            .as_mut()
            && remote.mirror.sequence().is_some()
        {
            let (cols, rows) = remote.mirror.size();
            let (cursor_col, cursor_row, cursor_visible) = remote.mirror.cursor();
            let (alt_screen, _, mouse) = remote.mirror.modes();
            let current = GridSignature {
                content_seq: remote.revision,
                size: (usize::from(cols), usize::from(rows)),
                cursor: (cursor_col, cursor_row, cursor_visible),
                alt_screen,
                mouse,
            };
            if current == *signature {
                return None;
            }
            *signature = current;
            return remote
                .pending
                .take()
                .or_else(|| remote.mirror.full_update());
        }
        let mut screen = self.shared.screen.lock().expect("screen");
        let current = GridSignature {
            content_seq: screen.content_seq(),
            size: screen.size(),
            cursor: screen.cursor(),
            alt_screen: screen.is_alt_screen(),
            mouse: screen.mouse_modes(),
        };
        if current == *signature {
            return None;
        }
        *signature = current;
        Some(screen.grid_update(false))
    }

    pub(crate) fn grid_wake(&self) -> GridWake {
        self.shared.grid_wake.clone()
    }

    /// Whether the child has bracketed-paste mode on — the "composer is
    /// alive" tell that gates initial-prompt injection.
    pub fn bracketed_paste(&self) -> bool {
        if let Some(remote) = self
            .shared
            .remote_grid
            .lock()
            .expect("remote grid")
            .as_ref()
            && remote.mirror.sequence().is_some()
        {
            return remote.mirror.modes().1;
        }
        self.shared.screen.lock().expect("screen").bracketed_paste()
    }

    /// Current alternate-screen and granular mouse modes.
    pub fn modes(&self) -> (bool, MouseModes) {
        if let Some(remote) = self
            .shared
            .remote_grid
            .lock()
            .expect("remote grid")
            .as_ref()
            && remote.mirror.sequence().is_some()
        {
            let (alt_screen, _, mouse) = remote.mirror.modes();
            return (alt_screen, mouse);
        }
        let screen = self.shared.screen.lock().expect("screen");
        (screen.is_alt_screen(), screen.mouse_modes())
    }

    /// A wheel event from an attached client: forwarded to the child when it
    /// asked for mouse reporting, otherwise ignored (the client scrolls its
    /// own scrollback).
    pub fn scroll(&self, up: bool, lines: usize, col: usize, row: usize) -> std::io::Result<()> {
        if let Transport::Remote(client) = &self.transport {
            return client.scroll(
                u8::from(!up),
                u16::try_from(lines).unwrap_or(u16::MAX),
                u16::try_from(col).unwrap_or(u16::MAX),
                u16::try_from(row).unwrap_or(u16::MAX),
            );
        }
        let bytes = self
            .shared
            .screen
            .lock()
            .expect("screen")
            .mouse_wheel(up, lines, col, row);
        if bytes.is_empty() {
            return Ok(());
        }
        // Raw: a wheel is not a keystroke, and must not look like user typing
        // to the status reducer.
        self.write_raw(&bytes)
    }

    pub fn read_scrollback(&self) -> diri_proto::ReadScrollbackResult {
        self.shared.screen.lock().expect("screen").scrollback()
    }

    pub fn read_scrollback_cells(
        &self,
        first_row: i64,
        max_rows: i64,
    ) -> diri_proto::ReadScrollbackCellsResult {
        if let Transport::Remote(client) = &self.transport
            && let Ok(result) = client.read_scrollback_cells(first_row, max_rows)
        {
            return result;
        }
        self.shared
            .screen
            .lock()
            .expect("screen")
            .scrollback_cells(first_row, max_rows)
    }

    /// Marks the session hibernated (input queues) or awake. On wake, the
    /// queued input flushes in order — right after the caller's SIGCONT, as
    /// the Swift daemon's wake() did.
    pub fn set_hibernated(&self, hibernated: bool) -> std::io::Result<()> {
        self.shared.hibernated.store(hibernated, Ordering::SeqCst);
        if hibernated {
            return Ok(());
        }
        let queued = std::mem::take(&mut *self.shared.queued_input.lock().expect("queued input"));
        if queued.is_empty() {
            return Ok(());
        }
        self.write_raw(&queued)
    }

    pub fn is_hibernated(&self) -> bool {
        self.shared.hibernated.load(Ordering::SeqCst)
    }

    /// Signals the whole child tree. For held sessions the holder walks the
    /// tree with pid-identity checks; a direct session signals its group.
    /// Returns the (pid, start-time) samples the holder observed, when held.
    pub fn signal_tree(&self, signal: i32) -> std::io::Result<Vec<(i32, i64)>> {
        match &self.transport {
            Transport::Direct(pty) => {
                pty.lock().expect("pty").kill_group(signal)?;
                Ok(Vec::new())
            }
            Transport::Held(client) => Ok(client
                .signal(signal)
                .map_err(holder_io_error)?
                .into_iter()
                .map(|sample| (sample.pid, sample.start_sec))
                .collect()),
            Transport::Remote(client) => {
                client.signal(signal)?;
                Ok(Vec::new())
            }
        }
    }

    fn write_raw(&self, bytes: &[u8]) -> std::io::Result<()> {
        self.write_raw_kind(bytes, false)
    }

    fn write_raw_kind(&self, bytes: &[u8], mouse: bool) -> std::io::Result<()> {
        // Before the deferred exec there is no PTY: queue for the launch
        // flush, exactly like the Swift daemon's `queuedLaunchInput`.
        if let Some(deferred) = &self.deferred
            && deferred.queue_input(bytes)
        {
            return Ok(());
        }
        if self.shared.hibernated.load(Ordering::SeqCst) {
            self.shared
                .queued_input
                .lock()
                .expect("queued input")
                .extend_from_slice(bytes);
            return Ok(());
        }
        match &self.transport {
            Transport::Direct(pty) => {
                use std::io::Write;
                let mut writer = pty.lock().expect("pty").writer()?;
                writer.write_all(bytes)?;
                writer.flush()
            }
            Transport::Held(client) => client.write(bytes).map_err(holder_io_error),
            Transport::Remote(client) if mouse => client.write_mouse(bytes),
            Transport::Remote(client) => client.write(bytes),
        }
    }

    /// Sends a pre-encoded terminal mouse report without presenting it to the
    /// prompt/status reducers as typed text.
    pub fn write_mouse(&self, bytes: &[u8]) -> std::io::Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        self.shared.note_hot();
        self.shared.grid_wake.prioritize_interactive_changes();
        self.write_raw_kind(bytes, true)
    }

    /// Sends text the way a user would.
    ///
    /// Non-submitting input goes through raw — pickers and permission dialogs
    /// read the literal keypress. A submitted prompt is framed as a bracketed
    /// paste when the child has that mode on (so embedded newlines don't
    /// submit the composer early), and the Enter is a SEPARATE write after a
    /// short settle — never riding the same buffer, where a truncated paste
    /// also loses or misfires it. Ported from `AgentSession.sendText`.
    pub fn send_text(&self, text: &str, submit: bool) -> std::io::Result<()> {
        if !submit {
            return self.write_input(text.as_bytes());
        }
        self.paste_text(text)?;
        std::thread::sleep(Duration::from_millis(30));
        self.submit_input()
    }

    /// Types `text` into the composer WITHOUT submitting it, framed as a
    /// bracketed paste when the child has that mode on. Separated from
    /// [`Self::send_text`] so a caller that cannot see the composer — the
    /// initial-prompt injector — can watch the text echo back before it
    /// commits to an Enter it can never take back.
    ///
    /// Titling happens here rather than at submit, so a prompt the injector
    /// types names its session the same way one the user types does. It is
    /// idempotent, which matters because the injector may retype.
    pub fn paste_text(&self, text: &str) -> std::io::Result<()> {
        self.capture_prompt_title(text);
        let framed = if self.bracketed_paste() {
            format!("\x1b[200~{text}\x1b[201~")
        } else {
            text.to_owned()
        };
        self.write_input(framed.as_bytes())
    }

    /// The Enter that submits whatever is in the composer.
    pub fn submit_input(&self) -> std::io::Result<()> {
        self.write_input(b"\r")
    }

    /// Kill-line (⌃U): what every one of these TUIs uses to empty its
    /// composer. Sent before a retyped prompt so a half-landed first attempt
    /// cannot concatenate with the second.
    pub fn clear_input_line(&self) -> std::io::Result<()> {
        self.write_input(b"\x15")
    }

    /// Sends bytes to the child, as if typed.
    pub fn write_input(&self, bytes: &[u8]) -> std::io::Result<()> {
        // Input means someone is interacting: keep the pump on its fast tick
        // so the echo renders promptly.
        self.shared.note_hot();
        if !bytes.is_empty() {
            // Let the attachment pump interrupt a background coalescing wait
            // instead of making typed input cross an 8 ms frame boundary.
            self.shared.grid_wake.prioritize_interactive_changes();
        }
        self.observe_prompt_input(bytes);
        // Typed before the deferred exec: queue for the launch flush, and
        // still count as a keystroke for the reducer.
        if let Some(deferred) = &self.deferred
            && deferred.queue_input(bytes)
        {
            self.feed_signal(StatusSignal::UserKeystroke);
            return Ok(());
        }
        if self.shared.hibernated.load(Ordering::SeqCst) {
            // Never write into a stopped tree's PTY (nobody drains the slave;
            // the buffer fills and writes wedge) — queue for the wake flush.
            self.shared
                .queued_input
                .lock()
                .expect("queued input")
                .extend_from_slice(bytes);
            self.feed_signal(StatusSignal::UserKeystroke);
            return Ok(());
        }
        match &self.transport {
            Transport::Direct(pty) => {
                use std::io::Write;
                let mut writer = pty.lock().expect("pty").writer()?;
                writer.write_all(bytes)?;
                writer.flush()?;
            }
            Transport::Held(client) => client.write(bytes).map_err(holder_io_error)?,
            Transport::Remote(client) => client.write(bytes)?,
        }
        self.feed_signal(StatusSignal::UserKeystroke);
        Ok(())
    }

    fn observe_prompt_input(&self, bytes: &[u8]) {
        if self.manifest_id == "shell"
            || self
                .shared
                .prompt_title
                .lock()
                .expect("prompt title")
                .is_some()
        {
            return;
        }
        if !matches!(
            *self.shared.status.lock().expect("status"),
            SessionStatus::Idle
        ) {
            if matches!(bytes, b"\r" | b"\n") {
                self.shared
                    .prompt_input
                    .lock()
                    .expect("prompt input")
                    .draft
                    .clear();
            }
            return;
        }
        let prompt = self
            .shared
            .prompt_input
            .lock()
            .expect("prompt input")
            .observe(bytes);
        if let Some(prompt) = prompt {
            self.capture_prompt_title(&prompt);
        }
    }

    fn capture_prompt_title(&self, prompt: &str) {
        if self.manifest_id == "shell" {
            return;
        }
        let title = crate::hooks::title_from_prompt(prompt);
        if title.is_empty() {
            return;
        }
        let mut current = self.shared.prompt_title.lock().expect("prompt title");
        if current.is_none() {
            *current = Some(title);
            drop(current);
            self.shared.bump_state_version();
        }
    }

    pub fn resize(&self, cols: u16, rows: u16) -> std::io::Result<()> {
        // Before the deferred exec, the FIRST client size decides the launch
        // geometry — record it and push the exec back so the viewport can
        // settle; the emulator is resized at launch, not per proposal.
        if let Some(deferred) = &self.deferred
            && deferred.propose_size(cols, rows)
        {
            return Ok(());
        }
        match &self.transport {
            Transport::Direct(pty) => pty.lock().expect("pty").resize(cols, rows)?,
            Transport::Held(client) => client.resize(cols, rows).map_err(holder_io_error)?,
            Transport::Remote(client) => client.resize(cols, rows)?,
        }
        self.shared
            .screen
            .lock()
            .expect("screen")
            .resize(cols as usize, rows as usize);
        self.shared.grid_wake.notify();
        Ok(())
    }

    /// Feeds an out-of-band signal — a hook callback, a notify — into the
    /// reducer.
    pub fn feed_signal(&self, signal: StatusSignal) -> ReducerOutcome {
        let outcome = self
            .shared
            .reducer
            .lock()
            .expect("reducer")
            .reduce(signal, SystemTime::now());
        apply(&self.shared, &outcome);
        outcome
    }

    pub fn claude_hook(&self, hook: ClaudeHook, is_subagent: bool) -> ReducerOutcome {
        self.feed_signal(StatusSignal::ClaudeHook { hook, is_subagent })
    }

    /// Ends the session, killing the child's whole tree.
    pub fn terminate(&mut self, grace: Duration) -> std::io::Result<Exit> {
        // Killed before the deferred exec: there is no child. Cancel wakes
        // the launcher (which double-checks under the same lock, killing a
        // child it raced into existence), and the session records a kill.
        if let Some(deferred) = &self.deferred
            && deferred.cancel()
        {
            self.shared.stop.store(true, Ordering::SeqCst);
            if let Some(pump) = self.pump.take() {
                let _ = pump.join();
            }
            self.shared.exited.store(true, Ordering::SeqCst);
            return Ok(Exit::Signal(libc::SIGKILL));
        }
        let exit = match &self.transport {
            Transport::Direct(pty) => pty.lock().expect("pty").terminate(grace)?,
            Transport::Held(client) => {
                // The holder escalates TERM → KILL itself; wait for the exit
                // marker to land in the log so the recorded exit is the real
                // one.
                let _ = client.kill_tree();
                let deadline = std::time::Instant::now() + grace + Duration::from_secs(1);
                while std::time::Instant::now() < deadline {
                    if self.shared.exited.load(Ordering::SeqCst) {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                self.shared
                    .exit
                    .lock()
                    .expect("exit")
                    .unwrap_or(Exit::Signal(libc::SIGKILL))
            }
            Transport::Remote(client) => {
                if !self.shared.exited.load(Ordering::SeqCst) {
                    let _ = client.signal(libc::SIGTERM);
                    let deadline = std::time::Instant::now() + grace;
                    while std::time::Instant::now() < deadline
                        && !self.shared.exited.load(Ordering::SeqCst)
                    {
                        std::thread::sleep(Duration::from_millis(20));
                    }
                }
                // `kill` also stops the per-session Holder. Do this even when
                // the Agent already exited naturally; an explicit lifecycle
                // termination must not leave an idle remote owner behind.
                if let Err(error) = client.kill()
                    && !self.shared.exited.load(Ordering::SeqCst)
                {
                    return Err(error);
                }
                self.shared
                    .exit
                    .lock()
                    .expect("exit")
                    .unwrap_or(Exit::Signal(libc::SIGKILL))
            }
        };
        self.shared.stop.store(true, Ordering::SeqCst);
        if let Transport::Remote(client) = &self.transport {
            client.close();
        }
        if let Some(pump) = self.pump.take() {
            let _ = pump.join();
        }
        Ok(exit)
    }
}

impl Drop for Session {
    /// Dropping a session ends the *watch*; what happens to the child depends
    /// on who owns the PTY.
    ///
    /// Direct: the child has to go, not merely be forgotten — the pump thread
    /// cannot be reclaimed while the terminal has a writer, and a forgotten
    /// child would keep running with nothing watching or reaping it.
    ///
    /// Held: the child is deliberately left running. Surviving the owner is
    /// the holder's whole purpose; a restarted daemon adopts it via
    /// [`Session::adopt`].
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        // A drop while the exec is still deferred wakes the launcher so the
        // join below is prompt; the child was never spawned.
        if let Some(deferred) = &self.deferred {
            let _ = deferred.cancel();
        }
        if let Transport::Direct(pty) = &self.transport
            && !self.shared.exited.load(Ordering::SeqCst)
            && let Ok(pty) = pty.lock()
        {
            let _ = pty.kill_group(libc::SIGKILL);
        }
        if let Transport::Remote(client) = &self.transport {
            client.close();
        }
        if let Some(pump) = self.pump.take() {
            let _ = pump.join();
        }
    }
}

fn new_shared(spec: &SessionSpec, log: OutputLog, engine: &ManifestEngine) -> Arc<Shared> {
    let manifest_version = engine
        .manifest(&spec.manifest_id)
        .map(|manifest| manifest.version.clone());
    Arc::new(Shared {
        id: spec.id.clone(),
        status: Mutex::new(SessionStatus::Starting),
        needs_input: Mutex::new(None),
        title: Mutex::new(None),
        prompt_title: Mutex::new(None),
        prompt_input: Mutex::new(PromptInputState::default()),
        log: Mutex::new(log),
        screen: Mutex::new(HeadlessScreen::new(
            spec.pty.cols as usize,
            spec.pty.rows as usize,
        )),
        reducer: Mutex::new(
            StatusReducer::new(spec.authority, SystemTime::now())
                .with_manifest(spec.manifest_id.clone(), manifest_version),
        ),
        exit: Mutex::new(None),
        exited: AtomicBool::new(false),
        stop: AtomicBool::new(false),
        state_version: AtomicU64::new(0),
        last_hot: AtomicU64::new(unix_secs()),
        last_interaction: AtomicU64::new(0),
        artifacts: Mutex::new(Vec::new()),
        hibernated: AtomicBool::new(false),
        queued_input: Mutex::new(Vec::new()),
        child_pid: std::sync::atomic::AtomicI32::new(0),
        remote_grid: Mutex::new(None),
        remote_output_offset: AtomicU64::new(0),
        grid_wake: GridWake::new(),
    })
}

/// Waits for a freshly launched holder and returns the exit-marker floor:
/// 250 × 20ms.
///
/// Any stat answer attaches — `alive: false` just means the child already
/// exited, and the pump will find its marker. A child so short-lived that the
/// holder has *already cleaned up* is attached by evidence instead: the log
/// advancing past the pre-spawn tail proves the holder ran and wrote a
/// marker.
fn wait_for_holder(
    client: &HolderClient,
    logs_dir: &Path,
    session_id: &str,
    pre_spawn_tail: u64,
) -> Result<u64, crate::holder::HolderError> {
    for _ in 0..250 {
        if let Ok(stat) = client.stat() {
            return Ok(stat.epoch_offset.unwrap_or(pre_spawn_tail));
        }
        if let Ok(mut log) = OutputLog::reader(logs_dir, session_id) {
            log.refresh_from_disk();
            if log.tail_offset() > pre_spawn_tail {
                return Ok(pre_spawn_tail);
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    Err(crate::holder::HolderError::Launch(
        "holder did not become ready".into(),
    ))
}

fn holder_io_error(error: crate::holder::HolderError) -> std::io::Error {
    std::io::Error::other(error.to_string())
}

/// Applies a reducer outcome to the shared state, bumping the state version
/// only when something observable actually changed — that version is what the
/// registry watcher polls instead of deep-diffing records.
fn apply(shared: &Shared, outcome: &ReducerOutcome) {
    let mut changed = false;
    if let Some(status) = &outcome.status_change {
        {
            let mut current = shared.status.lock().expect("status");
            if *current != *status {
                *current = status.clone();
                changed = true;
            }
        }
        if matches!(status, SessionStatus::Exited(_)) {
            shared.exited.store(true, Ordering::SeqCst);
        }
    }
    if let Some(detail) = &outcome.needs_input {
        let mut current = shared.needs_input.lock().expect("needs input");
        if current.as_ref() != Some(detail) {
            *current = Some(detail.clone());
            changed = true;
        }
    }
    // The reducer already suppresses evidence with unchanged structured
    // meaning, so an emitted value always warrants a record/UI refresh.
    changed |= outcome.status_evidence.is_some();
    // Leaving a needs-input state clears the pending detail, so the UI does not
    // keep showing a prompt that has been answered.
    if matches!(
        outcome.status_change,
        Some(SessionStatus::Working) | Some(SessionStatus::Idle)
    ) {
        let mut current = shared.needs_input.lock().expect("needs input");
        if current.is_some() {
            *current = None;
            changed = true;
        }
    }
    if changed {
        shared.bump_state_version();
    }
}

/// Rescans the visible screen for artifact URLs every ~2s, only when the
/// content actually changed and only when it plausibly contains a URL —
/// most screens never pay more than a substring check.
fn scan_artifacts_if_due(
    shared: &Shared,
    last_scan_at: &mut Option<std::time::Instant>,
    last_scan_seq: &mut u64,
) {
    if last_scan_at.is_some_and(|at| at.elapsed() < Duration::from_secs(2)) {
        return;
    }
    *last_scan_at = Some(std::time::Instant::now());
    let (seq, text) = {
        let screen = shared.screen.lock().expect("screen");
        let seq = screen.content_seq();
        if seq == *last_scan_seq {
            return;
        }
        (seq, screen.lines().join("\n"))
    };
    *last_scan_seq = seq;
    if !(text.contains("http") || text.contains("github.com") || text.contains("linear.app")) {
        return;
    }
    let now = diri_proto::DateMillis::from(SystemTime::now());
    let mut artifacts = shared.artifacts.lock().expect("artifacts");
    *artifacts = crate::artifacts::scan(&text, &artifacts, now);
}

/// Follows one remote Holder through any number of short-lived SSH Bridges.
/// The Holder remains the PTY owner; a broken Bridge only advances this
/// reconnect loop. Offsets and grid sequences make every retry idempotent.
fn pump_remote(
    shared: Arc<Shared>,
    engine: Arc<ManifestEngine>,
    client: Arc<RemoteSessionClient>,
    manifest_id: String,
) {
    let mut reconnect_delay = Duration::from_millis(50);
    let mut reconnects = 0_u32;
    while !shared.stop.load(Ordering::SeqCst) && !shared.exited.load(Ordering::SeqCst) {
        let output_offset = shared.remote_output_offset.load(Ordering::SeqCst);
        let grid_sequence = shared
            .remote_grid
            .lock()
            .expect("remote grid")
            .as_ref()
            .and_then(|state| state.mirror.sequence());
        let Ok((generation, mut output)) = client.connect(output_offset, grid_sequence) else {
            reconnects = reconnects.saturating_add(1);
            if reconnects.is_multiple_of(3) && remote_inspection_exited(&shared, &client) {
                break;
            }
            wait_for_remote_retry(&shared, reconnect_delay);
            reconnect_delay = (reconnect_delay * 2).min(Duration::from_secs(2));
            continue;
        };
        reconnect_delay = Duration::from_millis(50);
        let disposition = pump_remote_connection(
            &shared,
            &engine,
            &client,
            generation,
            &mut output,
            &manifest_id,
        );
        client.disconnect(generation);
        match disposition {
            RemoteConnectionDisposition::Continue => continue,
            RemoteConnectionDisposition::Reconnect => {
                reconnects = reconnects.saturating_add(1);
                if reconnects.is_multiple_of(3) && remote_inspection_exited(&shared, &client) {
                    break;
                }
                wait_for_remote_retry(&shared, reconnect_delay);
                reconnect_delay = (reconnect_delay * 2).min(Duration::from_secs(2));
            }
            RemoteConnectionDisposition::Exited | RemoteConnectionDisposition::Stopped => break,
            RemoteConnectionDisposition::Fatal => {
                mark_remote_transport_failed(&shared);
                break;
            }
        }
    }
    let _ = shared.log.lock().expect("log").flush();
}

fn remote_inspection_exited(shared: &Shared, client: &RemoteSessionClient) -> bool {
    let Ok(inspection) = client.inspect() else {
        return false;
    };
    let RemoteProcessState::Exited { code, signal } = inspection.process_state else {
        return false;
    };
    record_remote_exit(shared, ProcessExit { code, signal });
    true
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RemoteConnectionDisposition {
    Continue,
    Reconnect,
    Exited,
    Stopped,
    Fatal,
}

fn pump_remote_connection(
    shared: &Shared,
    engine: &ManifestEngine,
    client: &RemoteSessionClient,
    generation: u64,
    output: &mut std::process::ChildStdout,
    manifest_id: &str,
) -> RemoteConnectionDisposition {
    let mut codec = RemoteCodec::new();
    let mut buffer = [0_u8; 64 << 10];
    let mut replaying = false;
    let mut hello_accepted = false;
    let mut last_tick = SystemTime::now();
    let mut last_eval_seq = 0_u64;
    let mut last_scan_at = None;
    let mut last_scan_seq = 0_u64;
    let fd = output.as_raw_fd();

    loop {
        if shared.stop.load(Ordering::SeqCst) {
            return RemoteConnectionDisposition::Stopped;
        }
        if shared.exited.load(Ordering::SeqCst) {
            return RemoteConnectionDisposition::Exited;
        }
        scan_artifacts_if_due(shared, &mut last_scan_at, &mut last_scan_seq);

        let mut poll_fd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: `poll_fd` points to one initialized pollfd and remains valid
        // for the duration of this call.
        let ready = unsafe { libc::poll(&mut poll_fd, 1, TICK_INTERVAL.as_millis() as i32) };
        if ready < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return RemoteConnectionDisposition::Reconnect;
        }

        if ready == 0 {
            let now = SystemTime::now();
            let outcome = shared
                .reducer
                .lock()
                .expect("reducer")
                .reduce(StatusSignal::Tick, now);
            apply(shared, &outcome);
            last_tick = now;
            continue;
        }

        match output.read(&mut buffer) {
            Ok(0) => return RemoteConnectionDisposition::Reconnect,
            Ok(count) => {
                let messages = match codec.feed(&buffer[..count]) {
                    Ok(messages) => messages,
                    Err(_) => return RemoteConnectionDisposition::Fatal,
                };
                for message in messages {
                    let disposition = handle_remote_message(
                        shared,
                        engine,
                        client,
                        generation,
                        manifest_id,
                        &mut last_eval_seq,
                        &mut replaying,
                        &mut hello_accepted,
                        message,
                    );
                    if disposition != RemoteConnectionDisposition::Continue {
                        return disposition;
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return RemoteConnectionDisposition::Reconnect,
        }

        if last_tick.elapsed().unwrap_or_default() >= TICK_INTERVAL {
            last_tick = SystemTime::now();
            let outcome = shared
                .reducer
                .lock()
                .expect("reducer")
                .reduce(StatusSignal::Tick, last_tick);
            apply(shared, &outcome);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_remote_message(
    shared: &Shared,
    engine: &ManifestEngine,
    client: &RemoteSessionClient,
    generation: u64,
    manifest_id: &str,
    last_eval_seq: &mut u64,
    replaying: &mut bool,
    hello_accepted: &mut bool,
    message: RemoteMessage,
) -> RemoteConnectionDisposition {
    if !*hello_accepted && !matches!(message, RemoteMessage::HelloAck(_)) {
        return RemoteConnectionDisposition::Fatal;
    }
    match message {
        RemoteMessage::HelloAck(acknowledgement) => {
            if *hello_accepted
                || client.validate_hello(&acknowledgement).is_err()
                || client
                    .accept_hello(generation, acknowledgement.controller_epoch)
                    .is_err()
            {
                return RemoteConnectionDisposition::Fatal;
            }
            if let RemoteProcessState::Exited { code, signal } = acknowledgement.process_state {
                record_remote_exit(shared, ProcessExit { code, signal });
                return RemoteConnectionDisposition::Exited;
            }
            *hello_accepted = true;
            RemoteConnectionDisposition::Continue
        }
        RemoteMessage::Terminal(frame) => match frame.frame_type {
            FrameType::ReplayBegin => {
                *replaying = true;
                RemoteConnectionDisposition::Continue
            }
            FrameType::ReplayEnd => {
                *replaying = false;
                RemoteConnectionDisposition::Continue
            }
            FrameType::Output => {
                let Some((offset, bytes)) = frame.output_payload() else {
                    return RemoteConnectionDisposition::Fatal;
                };
                client.observe_output_offset(offset.saturating_add(bytes.len() as u64));
                apply_remote_output(
                    shared,
                    engine,
                    manifest_id,
                    last_eval_seq,
                    offset,
                    bytes,
                    *replaying,
                );
                RemoteConnectionDisposition::Continue
            }
            _ => RemoteConnectionDisposition::Fatal,
        },
        RemoteMessage::FullSnapshot(snapshot) => {
            if apply_remote_snapshot(shared, engine, manifest_id, last_eval_seq, snapshot).is_err()
            {
                RemoteConnectionDisposition::Fatal
            } else {
                RemoteConnectionDisposition::Continue
            }
        }
        RemoteMessage::GridDelta(delta) => {
            if apply_remote_delta(shared, delta).is_err() {
                // A gap is recoverable: the next Hello always reseeds with a
                // full authoritative snapshot.
                RemoteConnectionDisposition::Reconnect
            } else {
                RemoteConnectionDisposition::Continue
            }
        }
        RemoteMessage::ControlGranted(granted) => {
            if client
                .grant_control(generation, granted.controller_epoch)
                .is_err()
            {
                RemoteConnectionDisposition::Reconnect
            } else {
                RemoteConnectionDisposition::Continue
            }
        }
        RemoteMessage::ControlRevoked(_) => RemoteConnectionDisposition::Reconnect,
        RemoteMessage::ProcessExit(exit) => {
            record_remote_exit(shared, exit);
            RemoteConnectionDisposition::Exited
        }
        RemoteMessage::ScrollbackResponse(response) => {
            client.complete_scrollback(response);
            RemoteConnectionDisposition::Continue
        }
        RemoteMessage::Error(error) if error.fatal => RemoteConnectionDisposition::Fatal,
        RemoteMessage::Error(_) => RemoteConnectionDisposition::Continue,
        _ => RemoteConnectionDisposition::Fatal,
    }
}

fn apply_remote_output(
    shared: &Shared,
    engine: &ManifestEngine,
    manifest_id: &str,
    last_eval_seq: &mut u64,
    offset: u64,
    bytes: &[u8],
    replaying: bool,
) {
    let expected = shared.remote_output_offset.load(Ordering::SeqCst);
    let end = offset.saturating_add(bytes.len() as u64);
    if end <= expected {
        return;
    }
    let skip = expected.saturating_sub(offset).min(bytes.len() as u64) as usize;
    let bytes = &bytes[skip..];
    if bytes.is_empty() {
        return;
    }
    shared.remote_output_offset.store(end, Ordering::SeqCst);
    let _ = shared.log.lock().expect("log").append(bytes);
    let observation = {
        let mut screen = shared.screen.lock().expect("screen");
        screen.feed(bytes);
        evaluate_if_screen_changed(shared, &mut screen, engine, manifest_id, last_eval_seq)
    };
    let now = SystemTime::now();
    let mut reducer = shared.reducer.lock().expect("reducer");
    if !replaying {
        let outcome = reducer.reduce(StatusSignal::PtyOutputActivity, now);
        apply(shared, &outcome);
    }
    if let Some(observation) = observation {
        let outcome = reducer.reduce(StatusSignal::Screen(observation), now);
        drop(reducer);
        apply(shared, &outcome);
    }
}

fn apply_remote_snapshot(
    shared: &Shared,
    engine: &ManifestEngine,
    manifest_id: &str,
    last_eval_seq: &mut u64,
    snapshot: FullSnapshot,
) -> std::io::Result<()> {
    {
        let mut remote = shared.remote_grid.lock().expect("remote grid");
        let remote = remote
            .as_mut()
            .ok_or_else(|| std::io::Error::other("remote grid state is unavailable"))?;
        remote
            .mirror
            .apply_snapshot(
                snapshot.sequence,
                &snapshot.grid,
                snapshot.alt_screen,
                snapshot.bracketed_paste,
                snapshot.mouse,
            )
            .map_err(std::io::Error::other)?;
        remote.revision = remote.revision.saturating_add(1);
        remote.pending = Some(snapshot.grid.clone());
    }
    shared.grid_wake.notify();
    let observation = {
        let mut screen = shared.screen.lock().expect("screen");
        screen.resize(
            usize::from(snapshot.grid.cols),
            usize::from(snapshot.grid.rows),
        );
        if !screen.restore(
            // A remote Full Snapshot carries only the visible grid; scrollback
            // is fetched on demand through `Scroll`, never replayed here.
            &[],
            &snapshot.grid,
            snapshot.alt_screen,
            snapshot.bracketed_paste,
            snapshot.mouse,
        ) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "remote terminal snapshot could not be restored",
            ));
        }
        evaluate_if_screen_changed(shared, &mut screen, engine, manifest_id, last_eval_seq)
    };
    if let Some(observation) = observation {
        let outcome = shared
            .reducer
            .lock()
            .expect("reducer")
            .reduce(StatusSignal::Screen(observation), SystemTime::now());
        apply(shared, &outcome);
    }
    Ok(())
}

fn apply_remote_delta(shared: &Shared, delta: GridDelta) -> std::io::Result<()> {
    {
        let mut remote = shared.remote_grid.lock().expect("remote grid");
        let remote = remote
            .as_mut()
            .ok_or_else(|| std::io::Error::other("remote grid state is unavailable"))?;
        remote
            .mirror
            .apply_delta(
                delta.sequence,
                &delta.grid,
                delta.alt_screen,
                delta.bracketed_paste,
                delta.mouse,
            )
            .map_err(std::io::Error::other)?;
        remote.revision = remote.revision.saturating_add(1);
        remote.pending = if remote.pending.is_some() {
            remote.mirror.full_update()
        } else {
            Some(delta.grid)
        };
    }
    shared.grid_wake.notify();
    Ok(())
}

fn record_remote_exit(shared: &Shared, exit: ProcessExit) {
    let local = match (exit.code, exit.signal) {
        (_, Some(signal)) => Exit::Signal(signal),
        (Some(code), None) => Exit::Code(code),
        (None, None) => Exit::Code(-1),
    };
    *shared.exit.lock().expect("exit") = Some(local);
    let outcome = shared.reducer.lock().expect("reducer").reduce(
        StatusSignal::ProcessExit {
            code: exit.code,
            signal: exit.signal,
        },
        SystemTime::now(),
    );
    apply(shared, &outcome);
    shared.exited.store(true, Ordering::SeqCst);
}

fn mark_remote_transport_failed(shared: &Shared) {
    *shared.exit.lock().expect("exit") = Some(Exit::Code(126));
    let outcome = shared.reducer.lock().expect("reducer").reduce(
        StatusSignal::ProcessExit {
            code: Some(126),
            signal: None,
        },
        SystemTime::now(),
    );
    apply(shared, &outcome);
    shared.exited.store(true, Ordering::SeqCst);
}

fn wait_for_remote_retry(shared: &Shared, duration: Duration) {
    let deadline = Instant::now() + duration;
    while !shared.stop.load(Ordering::SeqCst) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The read/evaluate/reduce loop.
///
/// Waits on the terminal with a timeout rather than blocking in `read`. Two
/// reasons, both of which a blocking read got wrong: the debounce timers must
/// keep advancing while the child is *quiet* — that is exactly when staleness
/// and idle confirmation matter — and a blocking read cannot be interrupted, so
/// stopping a session would hang until the child happened to say something.
fn pump(
    shared: Arc<Shared>,
    engine: Arc<ManifestEngine>,
    pty: Arc<Mutex<Pty>>,
    mut reader: crate::pty::PtyStream,
    manifest_id: String,
) {
    // 64 KiB, matching the held pump: every read may trigger an evaluation,
    // so a small buffer multiplies per-chunk costs on burst output.
    let mut buffer = [0u8; 64 << 10];
    let mut last_tick = SystemTime::now();
    let mut last_eval_seq = 0u64;
    let mut last_scan_at = None;
    let mut last_scan_seq = 0u64;
    let fd = reader.as_raw_fd();

    loop {
        if shared.stop.load(Ordering::SeqCst) {
            break;
        }
        scan_artifacts_if_due(&shared, &mut last_scan_at, &mut last_scan_seq);

        // Wait for output, but never longer than a tick. Output interrupts the
        // wait immediately, so the idle tick only slows reducer timers — which
        // are no-ops outside Working anyway.
        let mut poll_fd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one initialized pollfd, a millisecond timeout.
        let ready = unsafe { libc::poll(&mut poll_fd, 1, shared.quiet_tick().as_millis() as i32) };
        if ready < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            break;
        }
        let hung_up = poll_fd.revents & (libc::POLLHUP | libc::POLLERR) != 0;
        let readable = poll_fd.revents & libc::POLLIN != 0;

        let read_result = if readable || hung_up {
            reader.read(&mut buffer)
        } else {
            Ok(usize::MAX) // nothing to read; fall through to the tick
        };

        match read_result {
            Ok(usize::MAX) => {}
            Ok(0) => break, // the child closed the terminal
            Ok(n) => {
                let closed = feed_output_batch(&shared, &mut reader, &mut buffer, n);
                // One detection pass per batch, not per read: the reducer
                // discards observations it has already judged anyway.
                let observation = {
                    let mut screen = shared.screen.lock().expect("screen");
                    evaluate_if_screen_changed(
                        &shared,
                        &mut screen,
                        &engine,
                        &manifest_id,
                        &mut last_eval_seq,
                    )
                };
                shared.grid_wake.notify();

                let now = SystemTime::now();
                let mut reducer = shared.reducer.lock().expect("reducer");
                let outcome = reducer.reduce(StatusSignal::PtyOutputActivity, now);
                apply(&shared, &outcome);
                if let Some(observation) = observation {
                    let outcome = reducer.reduce(StatusSignal::Screen(observation), now);
                    drop(reducer);
                    apply(&shared, &outcome);
                }
                if closed {
                    break;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }

        release_stalled_sync(&shared);

        // Ticks drive the debounce timers even when the child is quiet.
        if last_tick.elapsed().unwrap_or_default() >= TICK_INTERVAL {
            last_tick = SystemTime::now();
            let outcome = shared
                .reducer
                .lock()
                .expect("reducer")
                .reduce(StatusSignal::Tick, last_tick);
            apply(&shared, &outcome);
        }
    }

    // The stream ended: reap the child and record how it died.
    let exit = pty.lock().expect("pty").wait().ok();
    *shared.exit.lock().expect("exit") = exit;
    let (code, signal) = match exit {
        Some(Exit::Code(code)) => (Some(code), None),
        Some(Exit::Signal(signal)) => (None, Some(signal)),
        None => (None, None),
    };
    let outcome = shared.reducer.lock().expect("reducer").reduce(
        StatusSignal::ProcessExit { code, signal },
        SystemTime::now(),
    );
    apply(&shared, &outcome);
    shared.exited.store(true, Ordering::SeqCst);
    let _ = shared.log.lock().expect("log").flush();
}

/// Feeds one batch of PTY output: the read the caller already made, plus every
/// continuation that arrives before the batch settles.
///
/// The point of the batch is that the caller publishes the grid *once*, after
/// it returns. A repaint the child split across several writes therefore
/// reaches attached clients as one frame instead of as an erase followed by a
/// redraw, which is what a pane paints as a flicker. See [`OUTPUT_SETTLE`].
///
/// The screen lock is released between chunks so the grid can still be read
/// mid-batch, which is safe because nothing wakes a reader until the caller
/// publishes.
///
/// Returns true when the child closed the terminal.
fn feed_output_batch(
    shared: &Shared,
    reader: &mut crate::pty::PtyStream,
    buffer: &mut [u8],
    first: usize,
) -> bool {
    let started_at = Instant::now();
    let batch_deadline = started_at + OUTPUT_BATCH_CEILING;
    let repaint_deadline = started_at + OUTPUT_REPAINT_CEILING;
    let mut count = first;
    let mut total = 0usize;
    let mut filled_before = None;
    loop {
        {
            let mut log = shared.log.lock().expect("log");
            // A failed disk write must not stop the session: the child is
            // still running and its status still matters.
            let _ = log.append(&buffer[..count]);
        }
        let (mid_repaint, blank_repaint) = {
            let mut screen = shared.screen.lock().expect("screen");
            let before = *filled_before.get_or_insert_with(|| screen.filled_cells());
            screen.feed(&buffer[..count]);
            let after = screen.filled_cells();
            (after < before, after == 0 && before != 0)
        };
        total += count;

        let deadline = if blank_repaint {
            started_at + OUTPUT_BLANK_REPAINT_CEILING
        } else if mid_repaint {
            repaint_deadline
        } else {
            batch_deadline
        };
        let remaining = deadline.saturating_duration_since(Instant::now());
        if total >= OUTPUT_BATCH_BYTES || remaining.is_zero() {
            return false;
        }
        // Bytes already queued are always folded in — that costs nothing and
        // is what a terminal emulator's reader does. Waiting for bytes that
        // have not arrived is reserved for a screen that currently holds less
        // than it did when the batch opened, which is a repaint caught between
        // its erase and its redraw. An echo, which only adds, never waits.
        let settle = if blank_repaint {
            // A full erase is the most destructive intermediate frame. Give
            // its redraw the entire bounded grace period instead of falling
            // through the ordinary 16 ms partial-repaint settle window.
            remaining
        } else if mid_repaint {
            OUTPUT_SETTLE
        } else {
            Duration::ZERO
        };
        if !matches!(reader.wait_readable(remaining.min(settle)), Ok(true)) {
            return false;
        }
        count = match reader.read(buffer) {
            Ok(0) => return true, // the child closed the terminal
            Ok(next) => next,
            // Ending the batch here is safe: the caller polls again straight
            // away and picks the rest up.
            Err(_) => return false,
        };
    }
}

/// Publishes a synchronized update the child opened and never closed.
///
/// Cheap enough to run every loop iteration: it is an `Option<Instant>` test
/// unless an update is actually overdue.
fn release_stalled_sync(shared: &Shared) {
    if shared.screen.lock().expect("screen").flush_expired_sync() {
        shared.grid_wake.notify();
    }
}

/// Runs manifest detection only when the visible screen actually changed.
///
/// `feed` is called per PTY chunk, but the reducer discards observations whose
/// `content_seq` it has already judged — previously *after* paying for a full
/// snapshot, two region clones, and the regex walk. `content_seq` also covers
/// the title (an OSC title change bumps it), so the title store rides the same
/// gate and only allocates when it moved.
fn evaluate_if_screen_changed(
    shared: &Shared,
    screen: &mut HeadlessScreen,
    engine: &ManifestEngine,
    manifest_id: &str,
    last_eval_seq: &mut u64,
) -> Option<crate::detect::ScreenObservation> {
    let seq = screen.content_seq();
    if seq == *last_eval_seq {
        return None;
    }
    *last_eval_seq = seq;
    {
        let title = screen.title();
        let mut stored = shared.title.lock().expect("title");
        if stored.as_deref() != title {
            *stored = title.map(str::to_string);
            drop(stored);
            shared.bump_state_version();
        }
    }
    engine.evaluate(&screen.snapshot(), manifest_id)
}

/// The held-transport pump: tails the holder-owned output log.
///
/// The holder writes the log; this loop replays a bounded tail, then follows
/// new bytes — stripping exit markers before the emulator sees them, and
/// honoring only markers at or beyond `exit_marker_floor` (bytes below it
/// belong to prior incarnations of the session id). A holder that dies
/// *without* a marker is caught by a periodic liveness probe.
fn pump_held(
    shared: Arc<Shared>,
    engine: Arc<ManifestEngine>,
    client: HolderClient,
    exit_marker_floor: u64,
    manifest_id: String,
) {
    let replay_budget = replay_budget();
    let (checkpoint_path, mut offset, mut watcher, mut marker_buffer) = {
        let mut log = shared.log.lock().expect("log");
        log.refresh_from_disk();
        let checkpoint_path = crate::checkpoint::ScreenCheckpoint::path_for_log(log.path());
        let watcher = log_watch::LogWatcher::new(log.path());
        let tail = log.tail_offset();
        // A fresh-enough checkpoint seeds the emulator from a few KiB and
        // replay resumes at its offset. "Fresh enough" preserves the hard
        // startup-work bound: the remaining tail must fit the same budget a
        // cold replay would use, even if a checkpoint went stale during a
        // sustained output flood. Anything unusable is a cache miss.
        let restored = crate::checkpoint::ScreenCheckpoint::load(&checkpoint_path)
            .filter(|checkpoint| {
                checkpoint.log_offset <= tail
                    && tail - checkpoint.log_offset <= replay_budget as u64
            })
            .filter(|checkpoint| {
                shared.screen.lock().expect("screen").restore(
                    &checkpoint.history,
                    &checkpoint.grid,
                    checkpoint.alt_screen,
                    checkpoint.bracketed_paste,
                    checkpoint.mouse,
                )
            });
        match restored {
            Some(checkpoint) => (
                checkpoint_path,
                checkpoint.log_offset,
                watcher,
                checkpoint.marker_buffer,
            ),
            None => (
                checkpoint_path,
                log.preferred_replay_start(replay_budget),
                watcher,
                Vec::new(),
            ),
        }
    };
    // Adoption can restore a checkpoint concurrently with a freshly attached
    // App. One event is cheap and guarantees a seed that raced the restore is
    // corrected without bringing back periodic grid polling.
    shared.grid_wake.notify();
    let mut last_checkpoint_key: Option<CheckpointKey> = None;
    let mut checkpoint_dirty_at: Option<Instant> = None;
    let mut last_liveness = Instant::now();
    let mut last_eval_seq = 0u64;
    let mut last_scan_at = None;
    let mut last_scan_seq = 0u64;
    let mut exit_status: Option<HolderExitStatus> = None;
    let mut interactive_qos = false;
    // Set while a repaint is being assembled across more than one log read.
    let mut publish_pending: Option<Instant> = None;
    // Until the tail is first caught up, bytes are history, not activity:
    // they must render, but not flip a quiet adopted session to Working.
    let mut replaying = true;

    while !shared.stop.load(Ordering::SeqCst) && exit_status.is_none() {
        scan_artifacts_if_due(&shared, &mut last_scan_at, &mut last_scan_seq);
        let (start, chunk) = {
            let mut log = shared.log.lock().expect("log");
            log.refresh_from_disk();
            log.read(offset, LOG_READ_BUDGET)
        };
        // A full read means the tail is not caught up, so this pass may hold
        // half a repaint. Publishing it would flicker; fold the rest into the
        // same frame instead — bounded, so a holder streaming faster than we
        // parse still updates.
        let caught_up = chunk.len() < LOG_READ_BUDGET;

        if chunk.is_empty() {
            if publish_pending.take().is_some() {
                shared.grid_wake.notify();
            }
            release_stalled_sync(&shared);
            if replaying {
                replaying = false;
                // The replay tail is drained: checkpoint immediately, as the
                // Swift daemon does right after `replayExistingLog`.
                if checkpoint_dirty_at.take().is_some() {
                    persist_checkpoint(
                        &shared,
                        &checkpoint_path,
                        offset,
                        &marker_buffer,
                        &mut last_checkpoint_key,
                    );
                }
            } else if checkpoint_dirty_at.is_some_and(|at| at.elapsed() >= CHECKPOINT_SETTLE) {
                checkpoint_dirty_at = None;
                persist_checkpoint(
                    &shared,
                    &checkpoint_path,
                    offset,
                    &marker_buffer,
                    &mut last_checkpoint_key,
                );
            }
            let should_be_interactive = shared.was_recently_touched();
            if should_be_interactive != interactive_qos {
                set_current_thread_interactive(should_be_interactive);
                interactive_qos = should_be_interactive;
            }
            // Quiet: block on the log watcher, which wakes the instant the
            // holder appends — the tick interval is only the ceiling for
            // reducer timers and the liveness probe. Attached or Working
            // sessions keep the fast ceiling; idle background ones stretch it.
            let log_replaced = match watcher.as_mut() {
                Some(watcher) => watcher.wait(shared.quiet_tick()),
                None => {
                    std::thread::sleep(shared.quiet_tick());
                    false
                }
            };
            if log_replaced {
                // The watcher's descriptor followed the retired inode through
                // rotation. Make the cached payload reader reopen the path as
                // well, matching the Swift daemon's logDidChange(rearm:).
                shared.log.lock().expect("log").invalidate_read_handle();
            }
            let outcome = shared
                .reducer
                .lock()
                .expect("reducer")
                .reduce(StatusSignal::Tick, SystemTime::now());
            apply(&shared, &outcome);

            if last_liveness.elapsed() >= LIVENESS_INTERVAL {
                last_liveness = Instant::now();
                if !client.is_alive() {
                    // One last look for a marker that raced the probe.
                    let (_, tail) = {
                        let mut log = shared.log.lock().expect("log");
                        log.refresh_from_disk();
                        log.read(offset, 64 << 10)
                    };
                    if tail.is_empty() {
                        // Markerless death: the child is gone but how is
                        // unknowable.
                        break;
                    }
                }
            }
            continue;
        }

        // A rotation can move the readable floor past us; resynchronize.
        if start > offset && !marker_buffer.is_empty() {
            marker_buffer.clear();
        }
        offset = start + chunk.len() as u64;
        last_liveness = Instant::now();

        // The floor is an incarnation boundary, so no marker straddles it:
        // markers wholly below are stripped but their statuses ignored.
        let honored_from = exit_marker_floor
            .saturating_sub(start)
            .min(chunk.len() as u64) as usize;
        let mut output = Vec::new();
        if honored_from > 0 {
            marker_buffer.extend_from_slice(&chunk[..honored_from]);
            let (replayed, _stale_exit) = HolderExitMarker::drain(&mut marker_buffer);
            output.extend_from_slice(&replayed);
            if start + honored_from as u64 >= exit_marker_floor {
                marker_buffer.clear(); // an unfinished stale marker ends here
            }
        }
        if honored_from < chunk.len() {
            marker_buffer.extend_from_slice(&chunk[honored_from..]);
            let (live, exit) = HolderExitMarker::drain(&mut marker_buffer);
            output.extend_from_slice(&live);
            if exit.is_some() {
                exit_status = exit;
            }
        }

        if !output.is_empty() {
            checkpoint_dirty_at = Some(Instant::now());
            let observation = {
                let mut screen = shared.screen.lock().expect("screen");
                screen.feed(&output);
                evaluate_if_screen_changed(
                    &shared,
                    &mut screen,
                    &engine,
                    &manifest_id,
                    &mut last_eval_seq,
                )
            };
            let batch_started = *publish_pending.get_or_insert_with(Instant::now);
            if caught_up || batch_started.elapsed() >= OUTPUT_BATCH_CEILING {
                publish_pending = None;
                shared.grid_wake.notify();
            }
            let now = SystemTime::now();
            let mut reducer = shared.reducer.lock().expect("reducer");
            if !replaying {
                let outcome = reducer.reduce(StatusSignal::PtyOutputActivity, now);
                apply(&shared, &outcome);
            }
            if let Some(observation) = observation {
                let outcome = reducer.reduce(StatusSignal::Screen(observation), now);
                drop(reducer);
                apply(&shared, &outcome);
            }
        }
    }

    if interactive_qos {
        set_current_thread_interactive(false);
    }

    // A repaint still being assembled when the holder exited is the last thing
    // clients will ever see of this session: publish it.
    if publish_pending.take().is_some() {
        shared.grid_wake.notify();
    }

    // Detaching or exiting: capture the final screen, so the next daemon
    // seeds from a checkpoint instead of pushing a raw tail through a fresh
    // emulator — the Swift daemon's teardown persist.
    if checkpoint_dirty_at.is_some() {
        persist_checkpoint(
            &shared,
            &checkpoint_path,
            offset,
            &marker_buffer,
            &mut last_checkpoint_key,
        );
    }

    if shared.stop.load(Ordering::SeqCst) && exit_status.is_none() {
        return; // detaching, not exiting: the held child lives on
    }

    let exit = exit_status.map(|status| match (status.code, status.signal) {
        (_, Some(signal)) => Exit::Signal(signal),
        (code, None) => Exit::Code(code.unwrap_or(-1)),
    });
    *shared.exit.lock().expect("exit") = exit;
    let (code, signal) = match exit {
        Some(Exit::Code(code)) => (Some(code), None),
        Some(Exit::Signal(signal)) => (None, Some(signal)),
        None => (None, None),
    };
    let outcome = shared.reducer.lock().expect("reducer").reduce(
        StatusSignal::ProcessExit { code, signal },
        SystemTime::now(),
    );
    apply(&shared, &outcome);
    shared.exited.store(true, Ordering::SeqCst);
}

/// The held-output follower is on the input-to-pixel path while a terminal is
/// attached, but the same thread may later follow background work. Raise its
/// Apple QoS only for the existing hot window and restore the default class
/// when it cools; other platforms keep their native scheduler behavior.
#[cfg(target_vendor = "apple")]
fn set_current_thread_interactive(interactive: bool) {
    let qos = if interactive {
        libc::qos_class_t::QOS_CLASS_USER_INTERACTIVE
    } else {
        libc::qos_class_t::QOS_CLASS_DEFAULT
    };
    // SAFETY: this changes only the calling thread's QoS class. Priority zero
    // is the documented relative priority for both selected classes.
    let _ = unsafe { libc::pthread_set_qos_class_self_np(qos, 0) };
}

#[cfg(not(target_vendor = "apple"))]
fn set_current_thread_interactive(_interactive: bool) {}

/// Records a deferred launch that never produced a child: the session
/// reports exit 127, the spawn-failure convention the app already knows.
fn mark_launch_failed(shared: &Shared) {
    *shared.exit.lock().expect("exit") = Some(Exit::Code(127));
    let outcome = shared.reducer.lock().expect("reducer").reduce(
        StatusSignal::ProcessExit {
            code: Some(127),
            signal: None,
        },
        SystemTime::now(),
    );
    apply(shared, &outcome);
    shared.exited.store(true, Ordering::SeqCst);
}

/// Everything a checkpoint's content is a function of, mirroring the Swift
/// `CheckpointKey`: grid and cursor state derive from fed log bytes (tracked
/// by the offset and the screen's `content_seq`), so equal keys mean a
/// byte-identical checkpoint that need not be rewritten.
#[derive(Clone, Copy, PartialEq)]
struct CheckpointKey {
    offset: u64,
    content_seq: u64,
    marker_bytes: usize,
    alt_screen: bool,
    bracketed_paste: bool,
    mouse: MouseModes,
}

/// Writes the current screen as a durable checkpoint, skipping the write when
/// nothing observable changed since the last one.
fn persist_checkpoint(
    shared: &Shared,
    path: &Path,
    offset: u64,
    marker_buffer: &[u8],
    last_key: &mut Option<CheckpointKey>,
) {
    let (history, grid, alt_screen, bracketed_paste, mouse, content_seq) = {
        let screen = shared.screen.lock().expect("screen");
        (
            screen.history_snapshot(),
            screen.full_snapshot(),
            screen.is_alt_screen(),
            screen.bracketed_paste(),
            screen.mouse_modes(),
            screen.content_seq(),
        )
    };
    let key = CheckpointKey {
        offset,
        content_seq,
        marker_bytes: marker_buffer.len(),
        alt_screen,
        bracketed_paste,
        mouse,
    };
    if *last_key == Some(key) {
        return;
    }
    let checkpoint = crate::checkpoint::ScreenCheckpoint {
        log_offset: offset,
        history,
        grid,
        marker_buffer: marker_buffer.to_vec(),
        alt_screen,
        bracketed_paste,
        mouse,
    };
    // A failed write must not stop the session; the checkpoint is a cache.
    if checkpoint.write_atomically(path).is_ok() {
        *last_key = Some(key);
    }
}

/// Wakes the held pump the moment the holder appends to the log, instead of
/// sleep-polling between reads. The Swift daemon used a DispatchSource for
/// exactly this; without it every byte of held-session output arrives up to a
/// quiet-tick late, which reads as ~10fps scrolling in a TUI.
#[cfg(target_os = "macos")]
mod log_watch {
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    pub struct LogWatcher {
        kq: i32,
        fd: i32,
        path: PathBuf,
    }

    impl LogWatcher {
        pub fn new(path: &Path) -> Option<Self> {
            // SAFETY: plain kqueue creation; failure is handled.
            let kq = unsafe { libc::kqueue() };
            if kq < 0 {
                return None;
            }
            let mut watcher = Self {
                kq,
                fd: -1,
                path: path.to_path_buf(),
            };
            watcher.arm();
            Some(watcher)
        }

        fn arm(&mut self) {
            if self.fd >= 0 {
                // SAFETY: closing a descriptor this struct owns.
                unsafe { libc::close(self.fd) };
                self.fd = -1;
            }
            let Ok(cpath) = std::ffi::CString::new(self.path.as_os_str().as_encoded_bytes()) else {
                return;
            };
            // SAFETY: O_EVTONLY opens for watching without inhibiting unmount.
            let fd = unsafe { libc::open(cpath.as_ptr(), libc::O_EVTONLY) };
            if fd < 0 {
                return; // not created yet: wait() degrades to a plain sleep
            }
            self.fd = fd;
            let event = libc::kevent {
                ident: fd as usize,
                filter: libc::EVFILT_VNODE,
                flags: libc::EV_ADD | libc::EV_CLEAR,
                fflags: libc::NOTE_WRITE
                    | libc::NOTE_EXTEND
                    | libc::NOTE_DELETE
                    | libc::NOTE_RENAME,
                data: 0,
                udata: std::ptr::null_mut(),
            };
            // SAFETY: registering one initialized event; no output requested.
            unsafe {
                libc::kevent(
                    self.kq,
                    &event,
                    1,
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null(),
                )
            };
        }

        /// Blocks until the log changes or `timeout` passes. EV_CLEAR keeps
        /// writes that land between waits queued, so wakeups are never lost.
        /// Returns true when rotation replaced the watched file, so the
        /// caller can invalidate any other descriptors for the old inode.
        pub fn wait(&mut self, timeout: Duration) -> bool {
            if self.fd < 0 {
                self.arm();
                if self.fd < 0 {
                    std::thread::sleep(timeout);
                    return false;
                }
            }
            let spec = libc::timespec {
                tv_sec: timeout.as_secs() as libc::time_t,
                tv_nsec: libc::c_long::from(timeout.subsec_nanos()),
            };
            // SAFETY: zeroed kevent output slot, valid timeout.
            let mut out = unsafe { std::mem::zeroed::<libc::kevent>() };
            let woke = unsafe { libc::kevent(self.kq, std::ptr::null(), 0, &mut out, 1, &spec) };
            if woke > 0 && out.fflags & (libc::NOTE_DELETE | libc::NOTE_RENAME) != 0 {
                // Rotation replaced the file: track the new incarnation.
                self.arm();
                return true;
            }
            false
        }
    }

    impl Drop for LogWatcher {
        fn drop(&mut self) {
            if self.fd >= 0 {
                // SAFETY: descriptors this struct owns.
                unsafe { libc::close(self.fd) };
            }
            unsafe { libc::close(self.kq) };
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::log::OutputLog;

        #[test]
        fn replacement_tells_the_log_reader_to_reopen() {
            let root = tempfile::tempdir().expect("temp dir");
            let mut writer = OutputLog::open(root.path(), "s", 1 << 20, 64, false).expect("writer");
            writer.append(&[b'a'; 32]).expect("initial append");
            let mut reader = OutputLog::reader(root.path(), "s").expect("reader");
            let mut watcher = LogWatcher::new(reader.path()).expect("watcher");

            writer.append(&[b'b'; 40]).expect("rotating append");
            writer.append(b"after").expect("post-rotation append");
            writer.flush().expect("flush");

            assert!(
                watcher.wait(Duration::from_secs(1)),
                "rename/delete notification identifies the replacement"
            );
            reader.invalidate_read_handle();
            assert!(reader.refresh_from_disk());
            assert_eq!(reader.tail_offset(), 77);
            let (_, data) = reader.read(72, 16);
            assert_eq!(data, b"after");
        }
    }
}

/// Platform gap, named: non-macOS builds sleep-poll at the tick interval.
/// Linux wants an inotify equivalent here.
#[cfg(not(target_os = "macos"))]
mod log_watch {
    use std::path::Path;
    use std::time::Duration;

    pub struct LogWatcher;

    impl LogWatcher {
        pub fn new(_path: &Path) -> Option<Self> {
            None
        }

        pub fn wait(&mut self, timeout: Duration) -> bool {
            std::thread::sleep(timeout);
            false
        }
    }
}

/// Convenience for tests and callers that just want the shipped rules.
pub fn load_engine(manifests: &Path) -> std::io::Result<(Arc<ManifestEngine>, Vec<String>)> {
    let (engine, failed) = ManifestEngine::load_dir(manifests)?;
    Ok((Arc::new(engine), failed))
}

/// The reducer authority for an agent, as its manifest declares it.
///
/// This used to special-case "claude-code" in code. It is data: each manifest
/// carries `agent.statusAuthority`, so a new agent gets the right behavior by
/// existing as a file.
pub fn authority_for(manifest_id: &str, engine: &ManifestEngine) -> Authority {
    engine
        .manifest(manifest_id)
        .and_then(|manifest| manifest.agent.as_ref())
        .map_or(Authority::ProcessOnly, |agent| agent.authority())
}

#[cfg(test)]
mod prompt_title_tests {
    use super::PromptInputState;

    #[test]
    fn committed_utf8_prompt_becomes_a_title_candidate() {
        let mut input = PromptInputState::default();
        assert!(input.observe("修".as_bytes()).is_none());
        assert!(input.observe("复 remote attach".as_bytes()).is_none());
        assert_eq!(input.observe(b"\r").as_deref(), Some("修复 remote attach"));
    }

    #[test]
    fn bracketed_paste_and_edits_are_normalized_before_submit() {
        let mut input = PromptInputState::default();
        input.observe(b"wrong");
        input.observe(&[0x15]);
        input.observe(b"\x1b[200~repair remote titles\x1b[201~");
        input.observe(&[0x7f]);
        input.observe(b"e");
        assert_eq!(
            input.observe(b"\r").as_deref(),
            Some("repair remote titlee")
        );
    }
}

#[cfg(test)]
mod grid_wake_tests {
    use std::time::Duration;

    use super::GridWake;

    #[test]
    fn grid_waiter_sleeps_until_a_real_change_and_coalesces_generations() {
        let wake = GridWake::new();
        let observed = wake.generation();
        let notifier = wake.clone();
        let thread = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            notifier.notify();
            notifier.notify();
        });

        let changed = wake.wait_for_change(observed, Duration::from_secs(1));
        thread.join().expect("notifier");
        assert!(changed.generation > observed);
        assert!(!changed.interactive);

        // The waiter may wake after the first notification while the notifier
        // advances the generation again. Catch up to the latest coalesced
        // generation before asserting that a quiet source stays asleep.
        let latest = wake.wait_for_change(changed.generation, Duration::ZERO);
        assert_eq!(latest.generation, wake.generation());
        assert!(!latest.interactive);
        assert_eq!(
            wake.wait_for_change(latest.generation, Duration::from_millis(5)),
            latest
        );
    }

    #[test]
    fn interactive_priority_covers_two_grid_changes_then_expires() {
        let wake = GridWake::new();
        let observed = wake.generation();
        wake.prioritize_interactive_changes();

        let unchanged = wake.wait_for_priority_or_timeout(observed, Duration::from_millis(1));
        assert_eq!(unchanged.generation, observed);
        assert!(!unchanged.interactive);

        wake.notify();
        let changed = wake.wait_for_priority_or_timeout(observed, Duration::from_secs(1));
        assert!(changed.generation > observed);
        assert!(changed.interactive);

        wake.consume_interactive_priority();
        wake.notify();
        let trailing = wake.wait_for_change(changed.generation, Duration::from_secs(1));
        assert!(trailing.interactive);

        wake.consume_interactive_priority();
        wake.notify();
        let background = wake.wait_for_change(trailing.generation, Duration::from_secs(1));
        assert!(!background.interactive);
    }
}
