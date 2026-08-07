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
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use diri_proto::{NeedsInputDetail, SessionStatus};

use crate::detect::ManifestEngine;
use crate::holder::{
    HolderClient, HolderExitMarker, HolderExitStatus, HolderLaunchSpec, HolderLauncher,
    HolderPaths, HolderStat,
};
use crate::log::OutputLog;
use crate::pty::{Exit, Pty, PtySpec};
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

/// What a session looks like from the outside.
#[derive(Clone, Debug)]
pub struct SessionView {
    pub id: String,
    pub status: SessionStatus,
    pub needs_input: Option<NeedsInputDetail>,
    pub title: Option<String>,
    pub tail_offset: u64,
    pub exited: bool,
}

/// The state the pump thread and the outside world share.
struct Shared {
    id: String,
    status: Mutex<SessionStatus>,
    needs_input: Mutex<Option<NeedsInputDetail>>,
    title: Mutex<Option<String>>,
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
}

impl Shared {
    fn bump_state_version(&self) {
        self.state_version.fetch_add(1, Ordering::SeqCst);
    }

    fn note_hot(&self) {
        self.last_hot.store(unix_secs(), Ordering::Relaxed);
    }

    /// Fast quiet-tick while the session is attached/touched or Working;
    /// everything else can wait a second.
    fn wants_fast_tick(&self) -> bool {
        if unix_secs().saturating_sub(self.last_hot.load(Ordering::Relaxed)) <= HOT_WINDOW_SECS {
            return true;
        }
        matches!(
            *self.status.lock().expect("status"),
            SessionStatus::Working | SessionStatus::Starting
        )
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
    pub mouse_reporting: bool,
}

/// Who owns the PTY.
enum Transport {
    /// This process does; dropping the session kills the child.
    Direct(Arc<Mutex<Pty>>),
    /// A holder process does; this session is a socket client and a log
    /// tail, and the child outlives it.
    Held(HolderClient),
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
    /// Defer the exec until the first client size settles (holder spawns
    /// only), so the agent's banner renders at the real viewport width.
    pub defer_launch: bool,
}

impl Session {
    /// Spawns the child and starts watching it — through a holder when the
    /// spec carries a [`HolderConfig`], directly otherwise.
    pub fn spawn(spec: SessionSpec, engine: Arc<ManifestEngine>) -> std::io::Result<Self> {
        match spec.holder.clone() {
            Some(holder) if spec.defer_launch => Self::spawn_held_deferred(spec, &holder, engine),
            Some(holder) => Self::spawn_held(spec, &holder, engine),
            None => Self::spawn_direct(spec, engine),
        }
    }

    fn spawn_direct(spec: SessionSpec, engine: Arc<ManifestEngine>) -> std::io::Result<Self> {
        let pty = Pty::spawn(&spec.pty)?;
        let log = OutputLog::writer(&spec.logs_dir, &spec.id)?;
        let shared = new_shared(&spec, log);
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
        let shared = new_shared(&spec, log);
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
        let shared = new_shared(&spec, log);
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
        SessionView {
            id: self.shared.id.clone(),
            status: self.shared.status.lock().expect("status").clone(),
            needs_input: self.shared.needs_input.lock().expect("needs input").clone(),
            title: self.shared.title.lock().expect("title").clone(),
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
        self.shared.screen.lock().expect("screen").size()
    }

    /// A full grid snapshot for a freshly attached sink, plus current modes.
    /// Does not disturb the shared diff baseline.
    pub fn full_grid(&self) -> (diri_proto::grid::GridUpdate, (bool, bool)) {
        self.shared.note_hot();
        let screen = self.shared.screen.lock().expect("screen");
        let modes = (screen.is_alt_screen(), screen.mouse_reporting());
        (screen.full_snapshot(), modes)
    }

    /// The next grid diff, if anything observable changed since `signature`.
    /// The signature compare makes an idle 16ms pump tick cost one mutex lock
    /// and a tuple compare — the grid walk only happens on change.
    ///
    /// Doubles as the attachment heartbeat: the attach hub polls this while
    /// any sink is connected, which keeps the session pump on its fast tick
    /// without the hub having to know about pump cadence.
    pub fn grid_update_if_changed(
        &self,
        signature: &mut GridSignature,
    ) -> Option<diri_proto::grid::GridUpdate> {
        self.shared.note_hot();
        let mut screen = self.shared.screen.lock().expect("screen");
        let current = GridSignature {
            content_seq: screen.content_seq(),
            size: screen.size(),
            cursor: screen.cursor(),
            alt_screen: screen.is_alt_screen(),
            mouse_reporting: screen.mouse_reporting(),
        };
        if current == *signature {
            return None;
        }
        *signature = current;
        Some(screen.grid_update(false))
    }

    /// Whether the child has bracketed-paste mode on — the "composer is
    /// alive" tell that gates initial-prompt injection.
    pub fn bracketed_paste(&self) -> bool {
        self.shared.screen.lock().expect("screen").bracketed_paste()
    }

    /// Current (alt_screen, mouse_reporting).
    pub fn modes(&self) -> (bool, bool) {
        let screen = self.shared.screen.lock().expect("screen");
        (screen.is_alt_screen(), screen.mouse_reporting())
    }

    /// A wheel event from an attached client: forwarded to the child when it
    /// asked for mouse reporting, otherwise ignored (the client scrolls its
    /// own scrollback).
    pub fn scroll(&self, up: bool, lines: usize, col: usize, row: usize) -> std::io::Result<()> {
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
        }
    }

    fn write_raw(&self, bytes: &[u8]) -> std::io::Result<()> {
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
        }
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
        let framed = if self.shared.screen.lock().expect("screen").bracketed_paste() {
            format!("\x1b[200~{text}\x1b[201~")
        } else {
            text.to_string()
        };
        self.write_input(framed.as_bytes())?;
        std::thread::sleep(Duration::from_millis(30));
        self.write_input(b"\r")
    }

    /// Sends bytes to the child, as if typed.
    pub fn write_input(&self, bytes: &[u8]) -> std::io::Result<()> {
        // Input means someone is interacting: keep the pump on its fast tick
        // so the echo renders promptly.
        self.shared.note_hot();
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
        }
        self.feed_signal(StatusSignal::UserKeystroke);
        Ok(())
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
        }
        self.shared
            .screen
            .lock()
            .expect("screen")
            .resize(cols as usize, rows as usize);
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
        };
        self.shared.stop.store(true, Ordering::SeqCst);
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
        if let Some(pump) = self.pump.take() {
            let _ = pump.join();
        }
    }
}

fn new_shared(spec: &SessionSpec, log: OutputLog) -> Arc<Shared> {
    Arc::new(Shared {
        id: spec.id.clone(),
        status: Mutex::new(SessionStatus::Starting),
        needs_input: Mutex::new(None),
        title: Mutex::new(None),
        log: Mutex::new(log),
        screen: Mutex::new(HeadlessScreen::new(
            spec.pty.cols as usize,
            spec.pty.rows as usize,
        )),
        reducer: Mutex::new(StatusReducer::new(spec.authority, SystemTime::now())),
        exit: Mutex::new(None),
        exited: AtomicBool::new(false),
        stop: AtomicBool::new(false),
        state_version: AtomicU64::new(0),
        last_hot: AtomicU64::new(unix_secs()),
        artifacts: Mutex::new(Vec::new()),
        hibernated: AtomicBool::new(false),
        queued_input: Mutex::new(Vec::new()),
        child_pid: std::sync::atomic::AtomicI32::new(0),
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
                let chunk = &buffer[..n];
                {
                    let mut log = shared.log.lock().expect("log");
                    // A failed disk write must not stop the session: the child
                    // is still running and its status still matters.
                    let _ = log.append(chunk);
                }
                let observation = {
                    let mut screen = shared.screen.lock().expect("screen");
                    screen.feed(chunk);
                    evaluate_if_screen_changed(
                        &shared,
                        &mut screen,
                        &engine,
                        &manifest_id,
                        &mut last_eval_seq,
                    )
                };

                let now = SystemTime::now();
                let mut reducer = shared.reducer.lock().expect("reducer");
                let outcome = reducer.reduce(StatusSignal::PtyOutputActivity, now);
                apply(&shared, &outcome);
                if let Some(observation) = observation {
                    let outcome = reducer.reduce(StatusSignal::Screen(observation), now);
                    drop(reducer);
                    apply(&shared, &outcome);
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }

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
                    && tail - checkpoint.log_offset <= REPLAY_BUDGET as u64
            })
            .filter(|checkpoint| {
                shared.screen.lock().expect("screen").restore(
                    &checkpoint.grid,
                    checkpoint.alt_screen,
                    checkpoint.bracketed_paste,
                    checkpoint.mouse_reporting,
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
                log.preferred_replay_start(REPLAY_BUDGET),
                watcher,
                Vec::new(),
            ),
        }
    };
    let mut last_checkpoint_key: Option<CheckpointKey> = None;
    let mut checkpoint_dirty_at: Option<Instant> = None;
    let mut last_liveness = Instant::now();
    let mut last_eval_seq = 0u64;
    let mut last_scan_at = None;
    let mut last_scan_seq = 0u64;
    let mut exit_status: Option<HolderExitStatus> = None;
    // Until the tail is first caught up, bytes are history, not activity:
    // they must render, but not flip a quiet adopted session to Working.
    let mut replaying = true;

    while !shared.stop.load(Ordering::SeqCst) && exit_status.is_none() {
        scan_artifacts_if_due(&shared, &mut last_scan_at, &mut last_scan_seq);
        let (start, chunk) = {
            let mut log = shared.log.lock().expect("log");
            log.refresh_from_disk();
            log.read(offset, 64 << 10)
        };

        if chunk.is_empty() {
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
            // Quiet: block on the log watcher, which wakes the instant the
            // holder appends — the tick interval is only the ceiling for
            // reducer timers and the liveness probe. Attached or Working
            // sessions keep the fast ceiling; idle background ones stretch it.
            match watcher.as_mut() {
                Some(watcher) => watcher.wait(shared.quiet_tick()),
                None => std::thread::sleep(shared.quiet_tick()),
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
    mouse_reporting: bool,
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
    let (grid, alt_screen, bracketed_paste, mouse_reporting, content_seq) = {
        let screen = shared.screen.lock().expect("screen");
        (
            screen.full_snapshot(),
            screen.is_alt_screen(),
            screen.bracketed_paste(),
            screen.mouse_reporting(),
            screen.content_seq(),
        )
    };
    let key = CheckpointKey {
        offset,
        content_seq,
        marker_bytes: marker_buffer.len(),
        alt_screen,
        bracketed_paste,
        mouse_reporting,
    };
    if *last_key == Some(key) {
        return;
    }
    let checkpoint = crate::checkpoint::ScreenCheckpoint {
        log_offset: offset,
        grid,
        marker_buffer: marker_buffer.to_vec(),
        alt_screen,
        bracketed_paste,
        mouse_reporting,
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
        pub fn wait(&mut self, timeout: Duration) {
            if self.fd < 0 {
                self.arm();
                if self.fd < 0 {
                    std::thread::sleep(timeout);
                    return;
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
            }
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

        pub fn wait(&mut self, timeout: Duration) {
            std::thread::sleep(timeout);
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
