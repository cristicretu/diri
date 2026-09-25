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

mod process_facts;

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
use crate::remote::stream::{OutputFrameAction, reconcile_output_frame};
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

/// The probe cadence while the holder's output subscription is open. That
/// socket is itself a liveness signal — a holder that dies closes it, and the
/// pump probes the moment it does — so the probe is only a backstop here, for
/// a holder that is up but has lost its child without writing a marker.
const STREAMING_LIVENESS_INTERVAL: Duration = Duration::from_secs(10);

/// How long after output or input a shell's foreground group is sampled on
/// every tick. The echo of Enter reaches the pump before the shell has forked
/// and handed the terminal to the job, so the sample taken with that output
/// can still name the shell; the ticks that follow catch the job.
const FOREGROUND_SETTLE: Duration = Duration::from_secs(1);

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
///
/// This window is measured from the moment the screen actually went blank —
/// see [`feed_output_batch`] — not from the start of the batch that happened
/// to contain the erase.
const OUTPUT_BLANK_REPAINT_CEILING: Duration = Duration::from_millis(80);

/// Upper bound on a widened blank-repaint grace. Far beyond any perceptual
/// budget, so nothing but a test would ask for it.
const MAX_OUTPUT_BLANK_REPAINT_CEILING: Duration = Duration::from_secs(5);

/// The blank-repaint grace, widened when the environment asks for it.
///
/// A test that drives a real PTY on a contended CI runner cannot assert
/// anything about an 80 ms window: the runner can stall a shell for longer
/// than that between two writes, and the resulting failure says nothing about
/// the coalescing logic under test. Such a test widens the window instead, so
/// scheduler noise is small against it, and the invariant it asserts is the
/// real one.
///
/// Only ever raised, never lowered: a smaller value than the default would
/// mean shipping more flicker, so it is clamped away. Malformed values fall
/// back to the default.
fn blank_repaint_ceiling() -> Duration {
    std::env::var("DIRIJOR_BLANK_REPAINT_CEILING_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map_or(OUTPUT_BLANK_REPAINT_CEILING, |ms| {
            Duration::from_millis(ms).clamp(
                OUTPUT_BLANK_REPAINT_CEILING,
                MAX_OUTPUT_BLANK_REPAINT_CEILING,
            )
        })
}

/// Byte ceiling on one batch, matching `alacritty_terminal`'s
/// `MAX_LOCKED_READ`. A child that writes faster than it can be parsed must
/// not starve the grid indefinitely.
const OUTPUT_BATCH_BYTES: usize = 1 << 20;

/// How much of a held session's log the follow pump reads per pass. A full
/// read means more is already waiting, which is how that pump recognizes a
/// repaint it has only half consumed.
const LOG_READ_BUDGET: usize = 512 << 10;

/// How often status detection may run while output is still backed up. Reading
/// is what keeps the pump close behind the holder, and a screen that changed
/// twice within one frame is not two observations worth having.
const EVAL_INTERVAL: Duration = Duration::from_millis(16);

/// How far ahead of the pump a subscription may start and still be worth
/// taking. Beyond this the file is followed instead, which costs latency for a
/// moment rather than churning subscriptions that cannot be read yet.
const LIVE_HANDOVER_GAP: u64 = 256 << 10;

/// What a session looks like from the outside.
#[derive(Clone, Debug)]
pub struct SessionView {
    pub remote_connection: Option<diri_proto::RemoteConnection>,
    pub attention_state: Option<diri_proto::attention::AttentionState>,
    pub id: String,
    pub status: SessionStatus,
    pub status_evidence: Option<diri_proto::StatusEvidence>,
    pub needs_input: Option<NeedsInputDetail>,
    pub last_turn_completed_at: Option<diri_proto::DateMillis>,
    pub title: Option<String>,
    pub title_source: Option<diri_proto::TitleSource>,
    /// Raw OSC title, kept separate so a captured prompt cannot hide a later name.
    pub terminal_title: Option<String>,
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
    holder_identity: std::sync::OnceLock<(diri_proto::process::ProcessIdentity, u64)>,
    /// A local emulator reset the held pump owes on its next pass. The pump
    /// applies it between chunks and persists the boundary checkpoint.
    reset_requested: AtomicBool,
    /// Advances on every applied local reset so attached clients receive a
    /// full grid instead of a diff against pre-reset cells.
    reset_generation: AtomicU64,
    /// The final retained terminal of a held child that genuinely exited,
    /// handed to the Registry exactly once for durable publication.
    completed: Mutex<Option<CompletedCapture>>,
    keyboard_known: AtomicBool,
    /// The PTY owner's last answer to "is the child reading a secret?",
    /// already vetoed by the alternate screen. See [`record_secret_input`].
    secret_input: AtomicBool,
    id: String,
    find_owner: String,
    find_capture_revision: AtomicU64,
    status: Mutex<SessionStatus>,
    needs_input: Mutex<Option<NeedsInputDetail>>,
    last_turn_completed_at: Mutex<Option<diri_proto::DateMillis>>,
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
    reset_required: bool,
    reset_staged: Option<diri_proto::remote_pty::TerminalResetState>,
    reset_committed: Option<diri_proto::remote_pty::TerminalResetState>,
    keyboard: RemoteKeyboardProjection,
    connection: diri_proto::RemoteConnection,
    mirror: GridMirror,
    revision: u64,
    pending: Option<diri_proto::grid::GridUpdate>,
}

#[derive(Debug)]
pub(crate) struct InputModesUnavailable;
impl std::fmt::Display for InputModesUnavailable {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("enhanced keyboard state is unavailable; input was not sent")
    }
}
impl std::error::Error for InputModesUnavailable {}

/// One bounded staged input-state record, committed with its matching grid.
#[derive(Default)]
struct RemoteKeyboardProjection {
    enhanced: bool,
    required: bool,
    staged: Option<diri_proto::remote_pty::InputModes>,
    committed: Option<diri_proto::terminal_input::KeyboardState>,
}

impl RemoteKeyboardProjection {
    fn state_for(
        &self,
        sequence: u64,
    ) -> std::io::Result<Option<diri_proto::terminal_input::KeyboardState>> {
        if !self.required {
            return Ok(None);
        }
        self.staged
            .filter(|state| {
                state.sequence == sequence
                    && (self.enhanced
                        || state
                            .keyboard
                            .is_some_and(|keyboard| keyboard.enhancements.is_none()))
            })
            .map(|state| state.keyboard)
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "grid publication has no matching keyboard state",
                )
            })
    }

    fn commit(&mut self, state: Option<diri_proto::terminal_input::KeyboardState>) {
        self.committed = state;
        self.staged = None;
    }
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
    pub reset_generation: u64,
    pub keyboard: Option<diri_proto::terminal_input::KeyboardState>,
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

    pub(crate) fn notify(&self) {
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

pub(crate) struct TerminalPublication {
    pub grid: Option<diri_proto::grid::GridUpdate>,
    pub modes: (bool, bool, MouseModes),
    pub keyboard: Option<diri_proto::terminal_input::KeyboardState>,
    pub secret_input: bool,
}

pub(crate) struct AttachmentSeed {
    pub grid: diri_proto::grid::GridUpdate,
    pub modes: (bool, bool, MouseModes),
    pub secret_input: bool,
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

/// A history read pinned to this Session's state and remote incarnation.
/// Acquire it under the Registry lock, then release that lock before waiting
/// for SSH. Keeping these handles alive does not keep a removed Session's
/// pump running or retarget a request to its replacement.
pub(crate) struct ScrollbackReader {
    shared: Arc<Shared>,
    remote: Option<Arc<RemoteSessionClient>>,
}

impl ScrollbackReader {
    pub(crate) fn capture_find(
        self,
    ) -> Result<diri_proto::CaptureFindResult, diri_proto::ControlError> {
        if let Some(client) = &self.remote {
            let is_alt_screen = self.shared.screen.lock().expect("screen").is_alt_screen();
            let cells = capture_remote_find_cells(|first_row, max_rows| {
                client.read_scrollback_cells(first_row, max_rows).ok()
            })
            .map_err(|error| diri_proto::ControlError::new("find_capture_unavailable", error))?;
            return Ok(diri_proto::CaptureFindResult {
                owner: self.shared.find_owner.clone(),
                capture_revision: self
                    .shared
                    .find_capture_revision
                    .fetch_add(1, Ordering::Relaxed),
                session_id: diri_proto::SessionId(self.shared.id.clone()),
                is_alt_screen,
                visible_rows: usize::try_from(cells.total_rows - cells.live_start_row)
                    .unwrap_or_default(),
                partial: cells.first_row > 0,
                cells,
            });
        }
        let screen = self.shared.screen.lock().expect("screen");
        let (cols, visible_rows) = screen.size();
        if cols.saturating_mul(visible_rows) > diri_proto::FIND_CAPTURE_MAX_CELLS {
            return Err(diri_proto::ControlError::new(
                "find_capture_too_large",
                "This terminal is too large for a retained search view",
            ));
        }
        let cells = screen
            .find_capture_cells()
            .map_err(|error| diri_proto::ControlError::new("find_capture_too_large", error))?;
        let first = cells.first_row;
        Ok(diri_proto::CaptureFindResult {
            owner: self.shared.find_owner.clone(),
            capture_revision: self
                .shared
                .find_capture_revision
                .fetch_add(1, Ordering::Relaxed),
            session_id: diri_proto::SessionId(self.shared.id.clone()),
            is_alt_screen: screen.is_alt_screen(),
            visible_rows,
            partial: first > 0,
            cells,
        })
    }

    pub(crate) fn read(
        self,
        first_row: i64,
        max_rows: i64,
    ) -> diri_proto::ReadScrollbackCellsResult {
        if let Some(client) = self.remote
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
}

/// Rows one remote scrollback request may carry (`ScrollbackRequest::validate`).
const REMOTE_FIND_CHUNK_ROWS: i64 = 1024;
/// Chunk reads allowed per capture: enough for the largest capture plus a few
/// catch-up reads, and a bound on how long a streaming terminal is chased.
const REMOTE_FIND_MAX_READS: usize = 16;

/// Builds a find capture for a remote session from the history rows its Holder
/// already serves on demand, so Find searches what scrolling can reach instead
/// of only the mirrored screen. No Helper protocol is involved beyond the
/// existing scrollback request.
///
/// The client requires one coherent tail: rows `first..total` with the visible
/// grid last. Chunks are read upward; absolute rows never renumber, so output
/// that arrives in between only adds rows at the end, which a further read
/// picks up. Rows a full-screen program repaints between two reads can be a
/// frame apart, as they can in any capture of a live terminal; the client
/// already checks live-grid matches against the grid it is painting.
fn capture_remote_find_cells(
    mut read: impl FnMut(i64, i64) -> Option<diri_proto::ReadScrollbackCellsResult>,
) -> Result<diri_proto::ReadScrollbackCellsResult, &'static str> {
    const UNAVAILABLE: &str = "History search is unavailable on this host right now";
    let probe = read(0, 1).ok_or(UNAVAILABLE)?;
    let cols = usize::try_from(probe.cols)
        .ok()
        .filter(|cols| *cols > 0)
        .ok_or("Invalid capture width")?;
    let budget = (diri_proto::FIND_CAPTURE_MAX_CELLS / cols).min(diri_proto::FIND_CAPTURE_MAX_ROWS);
    let visible = usize::try_from(probe.total_rows - probe.live_start_row).unwrap_or(usize::MAX);
    if budget < visible {
        return Err("This terminal is too large for a retained search view");
    }

    let first_wanted = (probe.total_rows - budget as i64).max(0);
    let mut rows: std::collections::VecDeque<Vec<diri_proto::grid::GridCell>> =
        std::collections::VecDeque::new();
    let mut metadata = std::collections::VecDeque::new();
    let mut annotated = true;
    let mut next = first_wanted;
    let mut last = probe;
    let mut reads = 0;
    while next < last.total_rows {
        reads += 1;
        if reads > REMOTE_FIND_MAX_READS {
            return Err("The terminal is printing too fast to search. Try again in a moment");
        }
        let chunk =
            read(next, (last.total_rows - next).min(REMOTE_FIND_CHUNK_ROWS)).ok_or(UNAVAILABLE)?;
        let count = usize::try_from(chunk.row_count).unwrap_or_default();
        if chunk.first_row != next || count == 0 || chunk.cols != last.cols {
            // Trimmed history, a reset, or a resize: the rows read so far no
            // longer belong to one terminal.
            return Err("The terminal changed while it was being searched. Try again");
        }
        let decoded = diri_proto::grid::GridRowCodec::decode_rows(&chunk.payload, count)
            .map_err(|_| "Invalid capture cells")?;
        if decoded.iter().any(|row| row.len() != cols) {
            return Err("Invalid capture row width");
        }
        // Older Helpers send no row metadata; the client accepts none at all
        // but not a partial set.
        annotated &= chunk.metadata.len() == count;
        if annotated {
            metadata.extend(chunk.metadata.iter().cloned());
        }
        rows.extend(decoded);
        next += count as i64;
        last = chunk;
    }

    // Catching up may have read past the budget; the newest rows are the ones
    // that include the screen.
    while rows.len() > budget {
        rows.pop_front();
        metadata.pop_front();
    }
    let rows = Vec::from(rows);
    Ok(diri_proto::ReadScrollbackCellsResult {
        metadata: if annotated {
            Vec::from(metadata)
        } else {
            Vec::new()
        },
        payload: diri_proto::grid::GridRowCodec::encode_rows(&rows)
            .map_err(|_| "Invalid capture cells")?,
        first_row: last.total_rows - rows.len() as i64,
        row_count: rows.len() as i64,
        total_rows: last.total_rows,
        live_start_row: last.live_start_row,
        cols: last.cols,
        content_seq: last.content_seq,
    })
}

#[cfg(test)]
mod remote_find_capture_tests {
    use super::capture_remote_find_cells;
    use diri_proto::ReadScrollbackCellsResult;
    use diri_proto::grid::{GridCell, GridRowCodec, RowMetadata, TermColor, TermStyle};

    const COLS: usize = 200;
    const VISIBLE: i64 = 40;

    /// A Holder whose terminal has `total` rows; row `n` starts with the
    /// character for `n % 10`, so a capture can be checked row by row.
    struct Holder {
        total: i64,
        cols: usize,
        annotated: bool,
        reads: Vec<(i64, i64)>,
    }

    impl Holder {
        fn new(total: i64) -> Self {
            Self {
                total,
                cols: COLS,
                annotated: true,
                reads: Vec::new(),
            }
        }

        fn read(&mut self, first: i64, max: i64) -> Option<ReadScrollbackCellsResult> {
            assert!((0..=1024).contains(&max), "the Holder rejects larger reads");
            self.reads.push((first, max));
            let end = (first + max).min(self.total);
            let rows: Vec<Vec<GridCell>> = (first..end)
                .map(|row| {
                    let mut cells = vec![GridCell::BLANK; self.cols];
                    cells[0] = GridCell::new(
                        u32::from(b'0') + (row % 10) as u32,
                        TermColor::Default,
                        TermColor::Default,
                        TermStyle::empty(),
                    );
                    cells
                })
                .collect();
            Some(ReadScrollbackCellsResult {
                metadata: if self.annotated {
                    vec![RowMetadata::default(); rows.len()]
                } else {
                    Vec::new()
                },
                payload: GridRowCodec::encode_rows(&rows).unwrap(),
                first_row: first,
                row_count: rows.len() as i64,
                total_rows: self.total,
                live_start_row: self.total - VISIBLE,
                cols: self.cols as i64,
                content_seq: self.total as u64,
            })
        }
    }

    fn first_scalars(cells: &ReadScrollbackCellsResult) -> Vec<u32> {
        GridRowCodec::decode_rows(&cells.payload, cells.row_count as usize)
            .unwrap()
            .iter()
            .map(|row| row[0].scalar)
            .collect()
    }

    #[test]
    fn a_remote_capture_is_the_newest_rows_the_budget_allows_ending_at_the_screen() {
        let mut holder = Holder::new(5_000);
        let cells = capture_remote_find_cells(|first, max| holder.read(first, max)).unwrap();
        // 160_000 cells at 200 columns.
        assert_eq!(cells.row_count, 800);
        assert_eq!(cells.first_row, 4_200);
        assert_eq!(cells.first_row + cells.row_count, cells.total_rows);
        assert_eq!(cells.total_rows - cells.live_start_row, VISIBLE);
        assert_eq!(cells.metadata.len(), 800);
        let scalars = first_scalars(&cells);
        assert_eq!(scalars[0], u32::from(b'0'), "row 4200");
        assert_eq!(scalars[799], u32::from(b'9'), "row 4999");
        assert_eq!(holder.reads, [(0, 1), (4_200, 800)]);
    }

    #[test]
    fn a_narrow_terminal_is_read_in_chunks_the_holder_accepts() {
        let mut holder = Holder::new(9_000);
        holder.cols = 80;
        let cells = capture_remote_find_cells(|first, max| holder.read(first, max)).unwrap();
        assert_eq!(cells.row_count, 2_000);
        assert_eq!(holder.reads, [(0, 1), (7_000, 1_024), (8_024, 976)]);
    }

    #[test]
    fn a_short_history_is_captured_whole() {
        let mut holder = Holder::new(120);
        let cells = capture_remote_find_cells(|first, max| holder.read(first, max)).unwrap();
        assert_eq!((cells.first_row, cells.row_count), (0, 120));
    }

    #[test]
    fn output_that_arrives_between_reads_is_caught_up_and_the_oldest_rows_give_way() {
        let mut holder = Holder::new(5_000);
        let mut calls = 0;
        let cells = capture_remote_find_cells(|first, max| {
            calls += 1;
            if calls == 2 {
                holder.total += 30;
            }
            holder.read(first, max)
        })
        .unwrap();
        assert_eq!(cells.total_rows, 5_030);
        assert_eq!(cells.row_count, 800, "still within the budget");
        assert_eq!(cells.first_row, 4_230);
        assert_eq!(cells.first_row + cells.row_count, cells.total_rows);
        assert_eq!(first_scalars(&cells)[0], u32::from(b'0'), "row 4230");
    }

    #[test]
    fn a_terminal_that_outruns_the_capture_is_reported_instead_of_chased_forever() {
        let mut holder = Holder::new(5_000);
        let error = capture_remote_find_cells(|first, max| {
            holder.total += 2_000;
            holder.read(first, max)
        })
        .unwrap_err();
        assert!(error.contains("too fast"), "{error}");
        assert!(holder.reads.len() <= 17);
    }

    #[test]
    fn a_resize_in_the_middle_of_a_capture_fails_it() {
        let mut holder = Holder::new(9_000);
        holder.cols = 80;
        let mut calls = 0;
        let error = capture_remote_find_cells(|first, max| {
            calls += 1;
            if calls == 3 {
                holder.cols = 100;
            }
            holder.read(first, max)
        })
        .unwrap_err();
        assert!(error.contains("changed"), "{error}");
    }

    #[test]
    fn an_older_helper_without_row_metadata_still_captures() {
        let mut holder = Holder::new(500);
        holder.annotated = false;
        let cells = capture_remote_find_cells(|first, max| holder.read(first, max)).unwrap();
        assert_eq!(cells.row_count, 500);
        assert!(
            cells.metadata.is_empty(),
            "none at all, never a partial set"
        );
    }

    #[test]
    fn an_unreachable_holder_is_an_error_not_an_empty_capture() {
        assert!(capture_remote_find_cells(|_, _| None).is_err());
    }
}

/// Pins the failed remote owner while inspection runs outside Registry.
pub(crate) struct RemoteReconnect {
    shared: Arc<Shared>,
    client: Arc<RemoteSessionClient>,
}
impl RemoteReconnect {
    pub(crate) fn inspect(&self) -> std::io::Result<diri_proto::remote_pty::SessionInspection> {
        self.client
            .inspect_for_reconnect(self.shared.child_pid.load(Ordering::SeqCst))
    }
    pub(crate) fn matches(&self, session: &Session) -> bool {
        Arc::ptr_eq(&self.shared, &session.shared)
    }
}

/// A remote stop pins the original incarnation while the Registry stays usable.
pub(crate) struct RemoteStop {
    shared: Arc<Shared>,
    client: Arc<RemoteSessionClient>,
}

impl RemoteStop {
    pub(crate) fn stop(&self, grace: Duration) -> std::io::Result<Exit> {
        if !self.shared.exited.load(Ordering::SeqCst) {
            let _ = self.client.signal(libc::SIGTERM);
            let deadline = Instant::now() + grace;
            while Instant::now() < deadline && !self.shared.exited.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        // Preserve terminate's treatment of an already-ended session (its
        // Holder may also be gone). A failed stop of a live session must keep
        // the original tracked owner so another Agent cannot replace it.
        let exit = accept_remote_stop_result(&self.shared, self.client.kill())?;
        self.shared.stop.store(true, Ordering::SeqCst);
        self.client.close();
        Ok(exit)
    }

    pub(crate) fn matches(&self, session: &Session) -> bool {
        Arc::ptr_eq(&self.shared, &session.shared)
    }
}

/// The destructive stop channel revokes the prior controller. Its observed
/// exit must reach the projection even when that controller never saw ProcessExit.
fn accept_remote_stop_result(
    shared: &Shared,
    result: std::io::Result<ProcessExit>,
) -> std::io::Result<Exit> {
    match result {
        Ok(exit) => {
            let local = match (exit.code, exit.signal) {
                (Some(code), None) => Exit::Code(code),
                (None, Some(signal)) if signal > 0 => Exit::Signal(signal),
                _ => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "stop returned ambiguous exit facts",
                    ));
                }
            };
            record_remote_exit(shared, exit);
            Ok(local)
        }
        Err(error) => shared.exit.lock().expect("exit").ok_or(error),
    }
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
        )?);
        let log = OutputLog::writer(&spec.logs_dir, &spec.id)?;
        let shared = new_shared(&spec, log, &engine, true);
        shared.keyboard_known.store(false, Ordering::SeqCst);
        *shared.remote_grid.lock().expect("remote grid") = Some(RemoteGridState {
            reset_required: false,
            reset_staged: None,
            reset_committed: None,
            keyboard: RemoteKeyboardProjection {
                enhanced: client.enhanced_keyboard_protocol(),
                ..Default::default()
            },
            connection: diri_proto::RemoteConnection {
                state: diri_proto::RemoteConnectionState::Connecting,
                since: diri_proto::DateMillis::from(SystemTime::now()),
            },
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

    pub(crate) fn remote_reconnect_handle(&self) -> Option<RemoteReconnect> {
        let Transport::Remote(client) = &self.transport else {
            return None;
        };
        Some(RemoteReconnect {
            shared: Arc::clone(&self.shared),
            client: Arc::clone(client),
        })
    }

    pub(crate) fn restart_failed_remote(
        &mut self,
        engine: Arc<ManifestEngine>,
        inspected: diri_proto::remote_pty::RemoteProcessState,
    ) -> std::io::Result<(bool, bool)> {
        let Transport::Remote(client) = &self.transport else {
            return Err(std::io::Error::other("session has no remote transport"));
        };
        if self.shared.exited.load(Ordering::SeqCst) {
            return Ok((false, false));
        }
        if let RemoteProcessState::Exited { code, signal } = inspected {
            record_remote_exit(&self.shared, ProcessExit { code, signal });
            return Ok((false, false));
        }
        if self
            .view()
            .remote_connection
            .is_none_or(|connection| connection.state != diri_proto::RemoteConnectionState::Failed)
        {
            return Ok((false, false));
        }
        let RemoteProcessState::Running { pid } = inspected else {
            unreachable!()
        };
        let uncertain = client.uncertain_effect();
        let previous = self.pump.take();
        let shared = Arc::clone(&self.shared);
        let client = Arc::clone(client);
        let manifest_id = self.manifest_id.clone();
        set_remote_connection(
            &self.shared,
            diri_proto::RemoteConnectionState::Reconnecting,
        );
        let worker = std::thread::Builder::new()
            .name(format!("diri-remote-session-{}", self.shared.id))
            .spawn(move || {
                // Joining the old failed pump happens outside Registry; a log
                // flush cannot block other sessions or overlap another owner.
                if let Some(previous) = previous {
                    let _ = previous.join();
                }
                if shared.stop.load(Ordering::SeqCst) {
                    return;
                }
                if client.restart_failed(pid).is_err() {
                    mark_remote_transport_failed(&shared);
                    return;
                }
                pump_remote(shared, engine, client, manifest_id);
            });
        match worker {
            Ok(worker) => {
                self.pump = Some(worker);
                Ok((true, uncertain))
            }
            Err(error) => {
                mark_remote_transport_failed(&self.shared);
                Err(error)
            }
        }
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
        )?);
        let log = OutputLog::writer(&spec.logs_dir, &spec.id)?;
        let shared = new_shared(&spec, log, &engine, false);
        shared
            .remote_output_offset
            .store(remote.output_offset, Ordering::SeqCst);
        shared.keyboard_known.store(false, Ordering::SeqCst);
        *shared.remote_grid.lock().expect("remote grid") = Some(RemoteGridState {
            reset_required: false,
            reset_staged: None,
            reset_committed: None,
            keyboard: RemoteKeyboardProjection {
                enhanced: client.enhanced_keyboard_protocol(),
                ..Default::default()
            },
            connection: diri_proto::RemoteConnection {
                state: diri_proto::RemoteConnectionState::Connecting,
                since: diri_proto::DateMillis::from(SystemTime::now()),
            },
            mirror: GridMirror::new(),
            revision: 0,
            pending: None,
        });
        if let Some((status, needs_input)) = initial_status
            && shared
                .reducer
                .lock()
                .expect("reducer")
                .attention_state()
                .is_none_or(|state| state.sequence == 0)
        {
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
        let shared = new_shared(&spec, log, &engine, true);
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
            let mut log = OutputLog::reader_before_launch(&spec.logs_dir, &spec.id)?;
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
        let (floor, stat) = wait_for_holder(&client, &spec.logs_dir, &spec.id, pre_spawn_tail)
            .map_err(holder_io_error)?;
        Self::attach(spec, client, floor, engine, true, stat.as_ref())
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
        let log = OutputLog::reader_before_launch(&spec.logs_dir, &spec.id)?;
        let shared = new_shared(&spec, log, &engine, true);
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
                    let Ok((floor, stat)) =
                        wait_for_holder(&client, &logs_dir, &id, pre_spawn_tail)
                    else {
                        mark_launch_failed(&shared);
                        return;
                    };
                    let stat = stat.or_else(|| {
                        client
                            .stat()
                            .ok()
                            .filter(|stat| stat.epoch_offset == Some(floor))
                    });
                    if let Some(stat) = stat {
                        process_facts::capture_holder(&shared, &stat);
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
                    pump_held(shared, engine, client, floor, manifest_id, true)
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
        let session = Self::attach(spec, client, floor, engine, false, Some(stat))?;
        if let Some((status, needs_input)) = initial_status
            && session
                .shared
                .reducer
                .lock()
                .expect("reducer")
                .attention_state()
                .is_none_or(|state| state.sequence == 0)
        {
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
        fresh: bool,
        stat: Option<&HolderStat>,
    ) -> std::io::Result<Self> {
        let log = OutputLog::reader(&spec.logs_dir, &spec.id)?;
        let shared = new_shared(&spec, log, &engine, fresh);
        // Log progress can establish short-lived launch readiness before the
        // first stat succeeds. Preserve the existing one-time launch probe,
        // but never bind a later Holder epoch to that log boundary.
        let fallback = if stat.is_none() {
            client
                .stat()
                .ok()
                .filter(|stat| stat.epoch_offset == Some(exit_marker_floor))
        } else {
            None
        };
        if let Some(stat) = stat.or(fallback.as_ref()) {
            process_facts::capture_holder(&shared, stat);
        }

        let pump = {
            let shared = Arc::clone(&shared);
            let engine = Arc::clone(&engine);
            let client = client.clone();
            let manifest_id = spec.manifest_id.clone();
            std::thread::Builder::new()
                .name(format!("diri-session-{}", spec.id))
                .spawn(move || {
                    pump_held(
                        shared,
                        engine,
                        client,
                        exit_marker_floor,
                        manifest_id,
                        fresh,
                    )
                })?
        };

        Ok(Self {
            shared,
            transport: Transport::Held(client),
            pump: Some(pump),
            manifest_id: spec.manifest_id,
            deferred: None,
        })
    }

    pub(crate) fn remote_stop(&self) -> Option<RemoteStop> {
        match &self.transport {
            Transport::Remote(client) => Some(RemoteStop {
                shared: Arc::clone(&self.shared),
                client: Arc::clone(client),
            }),
            _ => None,
        }
    }

    pub fn id(&self) -> &str {
        &self.shared.id
    }

    pub fn manifest_id(&self) -> &str {
        &self.manifest_id
    }

    pub fn view(&self) -> SessionView {
        let terminal_title = self.shared.title.lock().expect("title").clone();
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
                terminal_title.clone(),
                Some(diri_proto::TitleSource::TerminalTitle),
            )
        };
        let (attention_state, status_evidence) = {
            let reducer = self.shared.reducer.lock().expect("reducer");
            (
                reducer.attention_state().cloned(),
                reducer.evidence().cloned(),
            )
        };
        SessionView {
            remote_connection: self
                .shared
                .remote_grid
                .lock()
                .expect("remote grid")
                .as_ref()
                .map(|remote| remote.connection),
            attention_state,
            id: self.shared.id.clone(),
            terminal_title,
            status: self.shared.status.lock().expect("status").clone(),
            status_evidence,
            needs_input: self.shared.needs_input.lock().expect("needs input").clone(),
            last_turn_completed_at: *self
                .shared
                .last_turn_completed_at
                .lock()
                .expect("last turn completed"),
            title,
            title_source,
            tail_offset: self.shared.log.lock().expect("log").tail_offset(),
            exited: self.shared.exited.load(Ordering::SeqCst),
        }
    }

    /// Monotonic counter that moves exactly when status, needs-input, or
    /// title change. Poll this before paying for [`Self::view`].
    pub fn take_notifications(&self) -> Vec<diri_terminal_state::TerminalNotification> {
        self.shared
            .screen
            .lock()
            .expect("screen")
            .take_notifications()
    }

    pub fn state_version(&self) -> u64 {
        self.shared.state_version.load(Ordering::SeqCst)
    }

    pub fn status(&self) -> SessionStatus {
        self.shared.status.lock().expect("status").clone()
    }

    /// Total bytes the child has ever written. The governor compares this
    /// across sweeps: a growing tail is a working session, whatever the
    /// status heuristics currently believe.
    pub fn output_tail(&self) -> u64 {
        self.shared.log.lock().expect("log").tail_offset()
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

    /// The verified child birth and Holder epoch this Session was bound to
    /// at launch or adoption, if the Holder provided them. Local held
    /// sessions only; never refreshed from a later inspection.
    pub fn holder_run(&self) -> Option<(diri_proto::process::ProcessIdentity, u64)> {
        matches!(self.transport, Transport::Held(_))
            .then(|| self.shared.holder_identity.get().copied())
            .flatten()
    }

    /// Takes the final retained terminal of a genuinely exited held child.
    /// Returns it once; storage work must happen outside the Registry lock.
    pub(crate) fn take_completed_capture(&self) -> Option<CompletedCapture> {
        self.shared
            .completed
            .lock()
            .expect("completed capture")
            .take()
    }

    /// Reads the local emulator's title without using the conversation name.
    pub fn terminal_title(&self) -> std::io::Result<Option<String>> {
        if matches!(&self.transport, Transport::Remote(_)) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "Remote Helper snapshots do not provide current terminal titles",
            ));
        }
        Ok(self
            .shared
            .screen
            .lock()
            .expect("screen")
            .title()
            .map(str::to_owned))
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
        self.preview_seed()
    }

    /// Observe the current mirror without changing activity or process state.
    pub(crate) fn preview_seed(&self) -> AttachmentSeed {
        let wake = self.shared.grid_wake.clone();
        loop {
            let wake_generation = wake.generation();
            let sampled = {
                let remote = self.shared.remote_grid.lock().expect("remote grid");
                remote.as_ref().and_then(|remote| {
                    let grid = remote.mirror.full_update()?;
                    let (cols, rows) = remote.mirror.size();
                    let (cursor_col, cursor_row, cursor_visible) = remote.mirror.cursor();
                    let (alt_screen, bracketed_paste, mouse) = remote.mirror.modes();
                    Some((
                        grid,
                        (alt_screen, bracketed_paste, mouse),
                        GridSignature {
                            reset_generation: remote
                                .reset_committed
                                .as_ref()
                                .map_or(0, |state| state.generation),
                            keyboard: remote.keyboard.committed,
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
                    (
                        screen.is_alt_screen(),
                        screen.bracketed_paste(),
                        screen.mouse_modes(),
                    ),
                    GridSignature {
                        reset_generation: self.shared.reset_generation.load(Ordering::SeqCst),
                        keyboard: self
                            .shared
                            .keyboard_known
                            .load(Ordering::SeqCst)
                            .then(|| screen.input_keyboard_state())
                            .flatten(),
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
                    secret_input: self.secret_input(),
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
        self.terminal_publication(signature).grid
    }

    /// Capture grid and input modes under the same terminal/mirror lock.
    pub(crate) fn terminal_publication(
        &self,
        signature: &mut GridSignature,
    ) -> TerminalPublication {
        if let Some(remote) = self
            .shared
            .remote_grid
            .lock()
            .expect("remote grid")
            .as_mut()
            && remote.mirror.sequence().is_some()
        {
            let (cols, rows) = remote.mirror.size();
            let modes = remote.mirror.modes();
            let current = GridSignature {
                reset_generation: remote
                    .reset_committed
                    .as_ref()
                    .map_or(0, |state| state.generation),
                keyboard: remote.keyboard.committed,
                content_seq: remote.revision,
                size: (usize::from(cols), usize::from(rows)),
                cursor: remote.mirror.cursor(),
                alt_screen: modes.0,
                mouse: modes.2,
            };
            let grid = if current == *signature {
                None
            } else {
                let reset_changed = signature.reset_generation != current.reset_generation;
                *signature = current;
                if reset_changed {
                    remote.pending = None;
                    remote.mirror.full_update()
                } else {
                    remote
                        .pending
                        .take()
                        .or_else(|| remote.mirror.full_update())
                }
            };
            return TerminalPublication {
                grid,
                modes,
                keyboard: current.keyboard,
                // A remote Holder does not report termios yet.
                secret_input: false,
            };
        }
        let mut screen = self.shared.screen.lock().expect("screen");
        let modes = (
            screen.is_alt_screen(),
            screen.bracketed_paste(),
            screen.mouse_modes(),
        );
        let current = GridSignature {
            reset_generation: self.shared.reset_generation.load(Ordering::SeqCst),
            keyboard: self
                .shared
                .keyboard_known
                .load(Ordering::SeqCst)
                .then(|| screen.input_keyboard_state())
                .flatten(),
            content_seq: screen.content_seq(),
            size: screen.size(),
            cursor: screen.cursor(),
            alt_screen: modes.0,
            mouse: modes.2,
        };
        let grid = if current == *signature {
            None
        } else {
            *signature = current;
            Some(screen.grid_update(false))
        };
        drop(screen);
        TerminalPublication {
            grid,
            modes,
            keyboard: current.keyboard,
            secret_input: self.secret_input(),
        }
    }

    /// Whether the child is reading a secret from a line prompt with echo
    /// off, as last sampled from the PTY owner. Always `false` for a remote
    /// session and for a child that has exited.
    pub fn secret_input(&self) -> bool {
        self.shared.secret_input.load(Ordering::SeqCst)
            && !self.shared.exited.load(Ordering::SeqCst)
    }

    /// Asks the PTY owner now, for a caller about to record typed input. The
    /// pump's samples trail the child by up to a tick, which is fine for a
    /// lock icon and not for deciding whether a keystroke may be remembered.
    fn refresh_secret_input(&self) -> bool {
        match &self.transport {
            Transport::Direct(pty) => {
                let reading = pty.lock().expect("pty").secret_input();
                record_secret_input(&self.shared, reading)
            }
            Transport::Held(client) => {
                if !secret_input_plausible(&self.shared) {
                    return record_secret_input(&self.shared, false);
                }
                match client.stat() {
                    Ok(stat) => record_secret_input(&self.shared, stat.secret_input == Some(true)),
                    Err(_) => self.secret_input(),
                }
            }
            Transport::Remote(_) => false,
        }
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

    /// Unknown for old remote Holders until a negotiated, matching publication.
    pub fn keyboard_state(&self) -> Option<diri_proto::terminal_input::KeyboardState> {
        if let Some(remote) = self
            .shared
            .remote_grid
            .lock()
            .expect("remote grid")
            .as_ref()
        {
            if remote.keyboard.enhanced
                && remote.connection.state != diri_proto::RemoteConnectionState::Connected
            {
                return None;
            }
            return remote.keyboard.committed;
        }
        self.shared
            .keyboard_known
            .load(Ordering::SeqCst)
            .then(|| {
                self.shared
                    .screen
                    .lock()
                    .expect("screen")
                    .input_keyboard_state()
            })
            .flatten()
    }

    fn enhanced_keyboard_owner(&self) -> bool {
        if let Some(remote) = self
            .shared
            .remote_grid
            .lock()
            .expect("remote grid")
            .as_ref()
        {
            return remote.keyboard.enhanced;
        }
        self.shared
            .screen
            .lock()
            .expect("screen")
            .keyboard_enhancements_enabled()
    }

    pub(crate) fn allows_keyboard_controller(&self, capable: bool) -> bool {
        capable
            || match self.keyboard_state() {
                Some(state) => !state.requires_enhanced_controller(),
                None => !self.enhanced_keyboard_owner(),
            }
    }

    pub(crate) fn accepts_keyboard_input(&self, capable: bool) -> bool {
        match self.keyboard_state() {
            Some(state) => capable || !state.requires_enhanced_controller(),
            None => !self.enhanced_keyboard_owner(),
        }
    }

    /// Current alternate-screen, bracketed-paste, and granular mouse modes.
    pub fn modes(&self) -> (bool, bool, MouseModes) {
        if let Some(remote) = self
            .shared
            .remote_grid
            .lock()
            .expect("remote grid")
            .as_ref()
            && remote.mirror.sequence().is_some()
        {
            return remote.mirror.modes();
        }
        let screen = self.shared.screen.lock().expect("screen");
        (
            screen.is_alt_screen(),
            screen.bracketed_paste(),
            screen.mouse_modes(),
        )
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
        self.scrollback_reader().read(first_row, max_rows)
    }

    pub(crate) fn scrollback_reader(&self) -> ScrollbackReader {
        ScrollbackReader {
            shared: Arc::clone(&self.shared),
            remote: match &self.transport {
                Transport::Remote(client) => Some(Arc::clone(client)),
                _ => None,
            },
        }
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
    /// idempotent; delivery itself must never be replayed based on screen echo.
    ///
    /// Bracketed text is sanitized first so it cannot embed its own
    /// end-of-paste marker (#275).
    pub fn paste_text(&self, text: &str) -> std::io::Result<()> {
        if !self.accepts_keyboard_input(true) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                InputModesUnavailable,
            ));
        }
        // Text answering a password prompt must not become the session's name.
        if self.manifest_id != "shell" && !self.refresh_secret_input() {
            self.capture_prompt_title(text);
        }
        let framed = if self.bracketed_paste() {
            format!("\x1b[200~{}\x1b[201~", sanitize_paste_text(text))
        } else {
            text.to_owned()
        };
        self.write_input(framed.as_bytes())
    }

    /// The Enter that submits whatever is in the composer.
    pub fn submit_input(&self) -> std::io::Result<()> {
        self.write_input(b"\r")
    }

    /// Sends bytes to the child, as if typed.
    pub fn write_input(&self, bytes: &[u8]) -> std::io::Result<()> {
        if !self.accepts_keyboard_input(true) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                InputModesUnavailable,
            ));
        }
        if self.shared.exited.load(Ordering::SeqCst) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "session has exited",
            ));
        }
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
        // Match complete key packets: an arrow key or bracketed paste also
        // contains ESC/newlines, but neither proves a submitted response.
        let submits = matches!(
            bytes,
            b"\r" | b"\n" | b"\r\n" | b"\x03" | b"\x1b" | b"y" | b"n"
        );
        self.feed_signal(if submits {
            StatusSignal::UserSubmission
        } else {
            StatusSignal::UserKeystroke
        });
        self.sample_pty_facts();
        Ok(())
    }

    fn sample_pty_facts(&self) {
        match &self.transport {
            Transport::Direct(pty) => {
                let (child_pid, pgid, reading_secret) = {
                    let pty = pty.lock().expect("pty");
                    (pty.pid() as i32, pty.foreground_pgid(), pty.secret_input())
                };
                apply_foreground_sample(&self.shared, &self.manifest_id, child_pid, pgid);
                record_secret_input(&self.shared, reading_secret);
            }
            Transport::Held(client) => {
                sample_held_pty_facts(&self.shared, client, &self.manifest_id);
            }
            Transport::Remote(_) => {}
        }
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
        if self.refresh_secret_input() {
            // A password is not a conversation name either, and nothing typed
            // before the prompt may be joined to what is typed after it.
            self.shared
                .prompt_input
                .lock()
                .expect("prompt input")
                .draft
                .clear();
            return;
        }
        if !matches!(
            *self.shared.status.lock().expect("status"),
            SessionStatus::Starting | SessionStatus::Idle | SessionStatus::Working
        ) {
            // Dialog responses are not conversation names. Drop any partial
            // composer draft too, so it cannot leak across a permission flow.
            self.shared
                .prompt_input
                .lock()
                .expect("prompt input")
                .draft
                .clear();
            return;
        }
        // Screen classification trails input: Codex can accept its first
        // prompt while we still report Starting, or repaint Working before
        // the submit arrives. Neither state should discard composer text.
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

    /// Resets the emulator without touching the child: the PTY, process and
    /// session identity stay, the screen, history, modes and title go. Remote
    /// sessions ask their Holder; held local sessions queue the reset for
    /// their pump, which applies it between log chunks and persists a
    /// checkpoint at that exact offset so an Engine restart replays only
    /// bytes after the boundary. Acceptance means queued, not applied.
    pub fn reset_terminal(&self) -> std::io::Result<()> {
        if self.shared.exited.load(Ordering::SeqCst) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                "session has exited",
            ));
        }
        match &self.transport {
            Transport::Remote(client) => client.reset_terminal(),
            Transport::Held(_) => {
                if self.pump.is_none() {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::NotConnected,
                        "session has no terminal owner to apply a reset",
                    ));
                }
                self.shared.reset_requested.store(true, Ordering::SeqCst);
                // A reset is a user touch: keep the pump on its fast tick so
                // the request is applied within it even for an idle session.
                self.shared.note_hot();
                Ok(())
            }
            Transport::Direct(_) => Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "direct PTY sessions do not support an emulator reset",
            )),
        }
    }

    /// How many local resets this Session has applied; tests and callers
    /// observing the boundary use it, clients see it through a full grid.
    pub fn reset_generation(&self) -> u64 {
        self.shared.reset_generation.load(Ordering::SeqCst)
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
        self.feed_identified_signal(signal, Default::default())
    }

    pub fn feed_identified_signal(
        &self,
        signal: StatusSignal,
        identity: crate::attention::SignalIdentity,
    ) -> ReducerOutcome {
        let outcome = self
            .shared
            .reducer
            .lock()
            .expect("reducer")
            .reduce_identified(signal, identity, SystemTime::now());
        apply(&self.shared, &outcome);
        outcome
    }

    pub fn claude_hook(&self, hook: ClaudeHook, is_subagent: bool) -> ReducerOutcome {
        self.feed_signal(StatusSignal::ClaudeHook {
            hook,
            is_subagent,
            pending_work: None,
        })
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
            Transport::Direct(pty) => terminate_direct(pty, grace)?,
            Transport::Held(client) => {
                // The holder escalates TERM → KILL itself; wait for the exit
                // marker to land in the log so the recorded exit is the real
                // one.
                if let Err(error) = client.kill_tree()
                    && !self.shared.exited.load(Ordering::SeqCst)
                {
                    return Err(holder_io_error(error));
                }
                let deadline = std::time::Instant::now() + grace + Duration::from_secs(1);
                while std::time::Instant::now() < deadline {
                    if self.shared.exited.load(Ordering::SeqCst) {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                self.shared.exit.lock().expect("exit").ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "Holder did not confirm Agent exit; the session remains tracked",
                    )
                })?
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
                accept_remote_stop_result(&self.shared, client.kill())?
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

fn new_shared(
    spec: &SessionSpec,
    log: OutputLog,
    engine: &ManifestEngine,
    fresh: bool,
) -> Arc<Shared> {
    let manifest_version = engine
        .manifest(&spec.manifest_id)
        .map(|manifest| manifest.version.clone());
    let reducer = StatusReducer::new(spec.authority, SystemTime::now())
        .with_manifest(spec.manifest_id.clone(), manifest_version)
        .with_attention_storage(
            &spec.logs_dir.join(format!("{}.attention.sqlite", spec.id)),
            fresh,
        );
    let initial_status = reducer.status().clone();
    let initial_detail = reducer
        .attention_state()
        .and_then(|state| state.active_requests().find(|event| event.blocking))
        .and_then(|event| event.detail.clone());
    let initial_completion = reducer
        .attention_state()
        .and_then(|state| {
            state
                .events
                .iter()
                .rev()
                .find(|event| event.kind == diri_proto::attention::AttentionKind::Completion)
        })
        .map(|event| event.occurred_at);
    Arc::new(Shared {
        holder_identity: std::sync::OnceLock::new(),
        reset_requested: AtomicBool::new(false),
        reset_generation: AtomicU64::new(0),
        completed: Mutex::new(None),
        keyboard_known: AtomicBool::new(true),
        secret_input: AtomicBool::new(false),
        id: spec.id.clone(),
        find_owner: {
            static NEXT: AtomicU64 = AtomicU64::new(1);
            let stamp = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let serial = NEXT.fetch_add(1, Ordering::Relaxed);
            format!("{}-{stamp}-{serial}", std::process::id())
        },
        find_capture_revision: AtomicU64::new(0),
        status: Mutex::new(initial_status),
        needs_input: Mutex::new(initial_detail),
        last_turn_completed_at: Mutex::new(initial_completion),
        title: Mutex::new(None),
        prompt_title: Mutex::new(None),
        prompt_input: Mutex::new(PromptInputState::default()),
        log: Mutex::new(log),
        screen: Mutex::new(
            HeadlessScreen::new(spec.pty.cols as usize, spec.pty.rows as usize)
                .with_notifications(),
        ),
        reducer: Mutex::new(reducer),
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
) -> Result<(u64, Option<HolderStat>), crate::holder::HolderError> {
    for delay in crate::holder::readiness_delays().take(300) {
        if let Ok(stat) = client.stat() {
            return Ok((stat.epoch_offset.unwrap_or(pre_spawn_tail), Some(stat)));
        }
        if let Ok(mut log) = OutputLog::reader(logs_dir, session_id) {
            log.refresh_from_disk();
            if log.tail_offset() > pre_spawn_tail {
                return Ok((pre_spawn_tail, None));
            }
        }
        std::thread::sleep(delay);
    }
    Err(crate::holder::HolderError::Launch(
        "holder did not become ready".into(),
    ))
}

fn holder_io_error(error: crate::holder::HolderError) -> std::io::Error {
    std::io::Error::other(error.to_string())
}

/// Replaces control characters other than `\n`, `\r` and `\t` with a space,
/// so text framed in `ESC[200~ ... ESC[201~` cannot close the envelope early
/// (#275). Replacing one-for-one never deletes, so no marker can reassemble.
/// Mirrors `diri_term::keys::paste`; kept local since this crate does not
/// depend on `diri-term`.
fn sanitize_paste_text(text: &str) -> String {
    text.chars()
        .map(|ch| {
            if ch.is_control() && !matches!(ch, '\n' | '\r' | '\t') {
                ' '
            } else {
                ch
            }
        })
        .collect()
}

#[cfg(test)]
mod paste_text_sanitize_tests {
    use super::sanitize_paste_text;

    /// Issue #275, audited for `Session::paste_text`: an injected prompt
    /// that opens with the paste end-marker must not be able to close the
    /// envelope early and let the rest submit ahead of the real, separate
    /// Enter that `send_text` sends afterwards.
    #[test]
    fn embedded_end_marker_cannot_survive_sanitizing() {
        let framed = format!(
            "\x1b[200~{}\x1b[201~",
            sanitize_paste_text("\x1b[201~printf x\r")
        );
        let bytes = framed.as_bytes();
        let end_marker_positions: Vec<usize> = bytes
            .windows(6)
            .enumerate()
            .filter(|(_, window)| *window == b"\x1b[201~")
            .map(|(index, _)| index)
            .collect();
        assert_eq!(end_marker_positions, vec![bytes.len() - 6]);
        assert!(!bytes[6..bytes.len() - 6].contains(&0x1b));
    }

    /// Split-marker bytes must not reassemble once the embedded ESC is gone.
    #[test]
    fn nested_partial_markers_cannot_reassemble_after_sanitizing() {
        let sanitized = sanitize_paste_text("\x1b[20\x1b[201~1~");
        assert!(!sanitized.contains('\x1b'));
    }

    #[test]
    fn ordinary_text_and_newlines_are_untouched() {
        assert_eq!(
            sanitize_paste_text("one\ntwo\r\nthree\tfour"),
            "one\ntwo\r\nthree\tfour"
        );
        assert_eq!(sanitize_paste_text("café 🎉"), "café 🎉");
    }
}

/// Applies a reducer outcome to the shared state, bumping the state version
/// only when something observable actually changed — that version is what the
/// registry watcher polls instead of deep-diffing records.
fn apply(shared: &Shared, outcome: &ReducerOutcome) {
    let mut changed = outcome.attention_changed;
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
    if outcome.turn_completed {
        *shared
            .last_turn_completed_at
            .lock()
            .expect("last turn completed") = Some(diri_proto::DateMillis::from(SystemTime::now()));
        changed = true;
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

fn apply_foreground_sample(
    shared: &Shared,
    manifest_id: &str,
    child_pid: i32,
    foreground_pgid: Option<i32>,
) {
    if manifest_id != "shell" {
        return;
    }
    let Some(running) = crate::status::foreground_job_running(child_pid, foreground_pgid) else {
        return;
    };
    let outcome = shared
        .reducer
        .lock()
        .expect("reducer")
        .reduce(StatusSignal::ForegroundJob { running }, SystemTime::now());
    apply(shared, &outcome);
}

/// One Holder stat answers both questions the pump has about the PTY itself:
/// which job is in the foreground, and whether the child is reading a secret.
///
/// A shell is asked whenever it is due, as it already was for its foreground
/// job. Other sessions are asked only while a password prompt is possible at
/// all, so an agent's composer never pays for the round trip.
///
/// Returns what the holder said about its own liveness, when it was asked: a
/// stat is the same request the liveness probe makes, so a caller that just
/// sampled need not connect a second time to learn it.
fn sample_held_pty_facts(
    shared: &Shared,
    client: &HolderClient,
    manifest_id: &str,
) -> Option<bool> {
    let shell = manifest_id == "shell";
    if !shell && !secret_input_plausible(shared) {
        record_secret_input(shared, false);
        return None;
    }
    let stat = client.stat().ok()?;
    if shell {
        shared.child_pid.store(stat.child_pid, Ordering::SeqCst);
        apply_foreground_sample(shared, manifest_id, stat.child_pid, stat.foreground_pid);
    }
    // A Holder that predates the field omits it: not known to be secret.
    record_secret_input(shared, stat.secret_input == Some(true));
    Some(stat.alive)
}

/// A line-mode password prompt cannot coexist with the alternate screen or
/// with bracketed paste: the first is a full-screen program, the second a
/// composer that reads raw. Both are already known to the emulator, so
/// neither is worth asking the Holder about.
fn secret_input_plausible(shared: &Shared) -> bool {
    let screen = shared.screen.lock().expect("screen");
    !screen.is_alt_screen() && !screen.bracketed_paste()
}

/// Stores the PTY owner's termios sample, and wakes the attach pump when it
/// changed so the mode reaches the client although no cell did.
///
/// Editors and agent TUIs silence echo too, but in raw mode, which the owner
/// already excludes. The alternate screen is vetoed here as well, where the
/// emulator lives: whatever a full-screen program does to its line
/// discipline, it is not a password prompt.
fn record_secret_input(shared: &Shared, reading_secret: bool) -> bool {
    let secret = reading_secret && !shared.screen.lock().expect("screen").is_alt_screen();
    if shared.secret_input.swap(secret, Ordering::SeqCst) != secret {
        shared.grid_wake.notify();
    }
    secret
}

/// Whether a quiet tick should ask the holder for the shell's foreground
/// group. Each ask is a connection the holder has to wake for, and ten a
/// second for as long as a job runs bought nothing: a job starting or ending
/// moves bytes (the echoed newline, the next prompt), and those are sampled as
/// they arrive. What is left is the settle after output or input, and a slow
/// backstop for a change that moved no bytes at all (a job started with echo
/// off), which is late by at most [`LIVENESS_INTERVAL`].
///
/// Secret input rides the same samples and has the same shape: a password
/// prompt is printed as echo goes off, and the newline that answers it is
/// echoed just before echo comes back, which the settle then catches.
fn held_foreground_sample_due(
    since_activity: Option<Duration>,
    since_sample: Option<Duration>,
) -> bool {
    since_activity.is_some_and(|since| since <= FOREGROUND_SETTLE)
        || since_sample.is_none_or(|since| since >= LIVENESS_INTERVAL)
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
            set_remote_connection(&shared, diri_proto::RemoteConnectionState::Reconnecting);
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
        if client.uncertain_effect()
            && !shared.stop.load(Ordering::SeqCst)
            && !shared.exited.load(Ordering::SeqCst)
        {
            client.fail_closed();
            mark_remote_transport_failed(&shared);
            break;
        }
        match disposition {
            RemoteConnectionDisposition::Continue => continue,
            RemoteConnectionDisposition::Reconnect => {
                set_remote_connection(&shared, diri_proto::RemoteConnectionState::Reconnecting);
                reconnects = reconnects.saturating_add(1);
                if reconnects.is_multiple_of(3) && remote_inspection_exited(&shared, &client) {
                    break;
                }
                wait_for_remote_retry(&shared, reconnect_delay);
                reconnect_delay = (reconnect_delay * 2).min(Duration::from_secs(2));
            }
            RemoteConnectionDisposition::Exited | RemoteConnectionDisposition::Stopped => break,
            RemoteConnectionDisposition::Fatal => {
                client.fail_closed();
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
    let Ok(mut write_wakeup) = client.take_write_wakeup(generation) else {
        return RemoteConnectionDisposition::Reconnect;
    };

    loop {
        if shared.stop.load(Ordering::SeqCst) {
            return RemoteConnectionDisposition::Stopped;
        }
        if shared.exited.load(Ordering::SeqCst) {
            return RemoteConnectionDisposition::Exited;
        }
        scan_artifacts_if_due(shared, &mut last_scan_at, &mut last_scan_seq);

        // The timeout below only paces reducer timers: Helper output and
        // queued input each have a descriptor in the poll and wake it at once,
        // and a stop closes the Bridge, which does too. So an idle, untouched
        // session takes the same stretched tick a held one does, instead of
        // ten wakeups a second per Forge tab for as long as the Engine runs.
        let tick = shared.quiet_tick();
        if last_tick.elapsed().unwrap_or_default() >= tick {
            last_tick = SystemTime::now();
            let outcome = shared
                .reducer
                .lock()
                .expect("reducer")
                .reduce(StatusSignal::Tick, last_tick);
            apply(shared, &outcome);
        }

        let pending_fd = match client.pending_write_fd(generation) {
            Ok(fd) => fd,
            Err(_) => return RemoteConnectionDisposition::Reconnect,
        };
        let mut descriptors = [
            libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: write_wakeup.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: pending_fd.as_ref().map_or(-1, AsRawFd::as_raw_fd),
                events: libc::POLLOUT,
                revents: 0,
            },
        ];
        // SAFETY: the owned output, wakeup and cloned pending descriptors stay
        // alive throughout poll; generation checks precede every write.
        let ready = unsafe {
            libc::poll(
                descriptors.as_mut_ptr(),
                descriptors.len() as _,
                tick.as_millis() as i32,
            )
        };
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

        if descriptors[1].revents != 0 {
            let mut wake_bytes = [0; 256];
            while write_wakeup
                .read(&mut wake_bytes)
                .is_ok_and(|count| count > 0)
            {}
        }
        if (descriptors[1].revents != 0 || descriptors[2].revents != 0)
            && client.flush_pending(generation).is_err()
        {
            return RemoteConnectionDisposition::Reconnect;
        }
        if descriptors[0].revents == 0 {
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
    if shared
        .remote_grid
        .lock()
        .expect("remote grid")
        .as_ref()
        .is_some_and(|remote| remote.reset_staged.is_some())
        && !matches!(
            &message,
            RemoteMessage::InputModes(_) | RemoteMessage::FullSnapshot(_)
        )
    {
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
            if let RemoteProcessState::Running { pid } = acknowledgement.process_state {
                shared.child_pid.store(pid as i32, Ordering::SeqCst);
                apply_foreground_sample(
                    shared,
                    manifest_id,
                    pid as i32,
                    acknowledgement.foreground_pid,
                );
            }
            if let Some(remote) = shared.remote_grid.lock().expect("remote grid").as_mut() {
                remote.reset_required = client.supports_terminal_reset();
                remote.reset_staged = None;
                remote.keyboard = RemoteKeyboardProjection {
                    enhanced: acknowledgement.protocol.minor
                        >= diri_proto::remote_pty::ENHANCED_KEYBOARD_PROTOCOL_MINOR
                        && acknowledgement
                            .capabilities
                            .contains(&diri_proto::remote_pty::RemoteCapability::EnhancedKeyboard),
                    required: acknowledgement.protocol.minor
                        >= diri_proto::remote_pty::INPUT_MODES_PROTOCOL_MINOR
                        && acknowledgement
                            .capabilities
                            .contains(&diri_proto::remote_pty::RemoteCapability::InputModes),
                    ..RemoteKeyboardProjection::default()
                };
            }
            *hello_accepted = true;
            RemoteConnectionDisposition::Continue
        }
        RemoteMessage::Terminal(frame) => match frame.frame_type {
            FrameType::ReplayBegin => {
                shared
                    .screen
                    .lock()
                    .expect("screen")
                    .reset_notification_sequence();
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
                match apply_remote_output(
                    shared,
                    engine,
                    manifest_id,
                    last_eval_seq,
                    offset,
                    bytes,
                    *replaying,
                ) {
                    Some(next_offset) => {
                        client.observe_output_offset(next_offset);
                        RemoteConnectionDisposition::Continue
                    }
                    None => RemoteConnectionDisposition::Reconnect,
                }
            }
            _ => RemoteConnectionDisposition::Fatal,
        },
        RemoteMessage::TerminalResetState(reset) => {
            let mut remote = shared.remote_grid.lock().expect("remote grid");
            let Some(remote) = remote.as_mut() else {
                return RemoteConnectionDisposition::Fatal;
            };
            if !remote.reset_required
                || remote.reset_staged.is_some()
                || reset.incarnation != client.incarnation()
                || remote.reset_committed.as_ref().is_some_and(|old| {
                    reset.generation < old.generation
                        || (reset.generation == old.generation
                            && reset.output_offset != old.output_offset)
                })
            {
                return RemoteConnectionDisposition::Fatal;
            }
            remote.reset_staged = Some(reset);
            RemoteConnectionDisposition::Continue
        }
        RemoteMessage::FullSnapshot(snapshot) => {
            if apply_remote_snapshot(shared, engine, client, manifest_id, last_eval_seq, snapshot)
                .is_err()
            {
                RemoteConnectionDisposition::Fatal
            } else {
                set_remote_connection(shared, diri_proto::RemoteConnectionState::Connected);
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
        RemoteMessage::InputModes(modes) => {
            let mut remote = shared.remote_grid.lock().expect("remote grid");
            let Some(remote) = remote.as_mut() else {
                return RemoteConnectionDisposition::Fatal;
            };
            if !remote.keyboard.required
                || remote
                    .keyboard
                    .staged
                    .is_some_and(|old| old.sequence >= modes.sequence)
            {
                return RemoteConnectionDisposition::Fatal;
            }
            remote.keyboard.staged = Some(modes);
            RemoteConnectionDisposition::Continue
        }
        RemoteMessage::ForegroundProcess(foreground) => {
            apply_foreground_sample(
                shared,
                manifest_id,
                shared.child_pid.load(Ordering::SeqCst),
                foreground.pid,
            );
            RemoteConnectionDisposition::Continue
        }
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
) -> Option<u64> {
    let expected = shared.remote_output_offset.load(Ordering::SeqCst);
    let end = offset.saturating_add(bytes.len() as u64);
    let bytes = match reconcile_output_frame(expected, offset, bytes.len()) {
        OutputFrameAction::Feed => bytes,
        OutputFrameAction::FeedSuffix { drop_leading } => &bytes[drop_leading..],
        OutputFrameAction::Skip => return Some(expected),
        // Replay is bounded by the Holder. If older bytes have already fallen
        // out of that bound, the FullSnapshot queued after ReplayEnd is the
        // authoritative recovery. A gap in the live stream instead means the
        // connection must be reseeded before more output is consumed.
        OutputFrameAction::Gap { .. } if replaying => bytes,
        OutputFrameAction::Gap { .. } => return None,
    };
    if bytes.is_empty() {
        return Some(expected);
    }
    shared.remote_output_offset.store(end, Ordering::SeqCst);
    let _ = shared.log.lock().expect("log").append(bytes);
    let observation = (!replaying)
        .then(|| {
            let mut screen = shared.screen.lock().expect("screen");
            screen.feed(bytes);
            if screen.has_notifications() {
                shared.bump_state_version();
            }
            evaluate_if_screen_changed(shared, &mut screen, engine, manifest_id, last_eval_seq)
        })
        .flatten();
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
    Some(end)
}

fn apply_remote_snapshot(
    shared: &Shared,
    engine: &ManifestEngine,
    client: &RemoteSessionClient,
    manifest_id: &str,
    last_eval_seq: &mut u64,
    snapshot: FullSnapshot,
) -> std::io::Result<()> {
    {
        let mut remote = shared.remote_grid.lock().expect("remote grid");
        let remote = remote
            .as_mut()
            .ok_or_else(|| std::io::Error::other("remote grid state is unavailable"))?;
        let reset = remote.reset_staged.take();
        if remote.reset_required
            && reset
                .as_ref()
                .is_none_or(|reset| reset.sequence != snapshot.sequence)
        {
            return Err(std::io::Error::other(
                "full snapshot reset boundary is missing or mismatched",
            ));
        }
        let reset_changed = reset.as_ref().is_some_and(|reset| {
            reset.generation != 0
                && remote
                    .reset_committed
                    .as_ref()
                    .is_none_or(|old| old.generation != reset.generation)
        });
        let keyboard = remote.keyboard.state_for(snapshot.sequence)?;
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
        remote.keyboard.commit(keyboard);
        if reset_changed {
            shared.screen.lock().expect("screen").reset();
            client.observe_terminal_reset(reset.as_ref().expect("changed reset").generation);
            shared.bump_state_version();
        }
        remote.reset_committed = reset.or_else(|| remote.reset_committed.clone());
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
        let keyboard = remote.keyboard.state_for(delta.sequence)?;
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
        remote.keyboard.commit(keyboard);
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

fn set_remote_connection(shared: &Shared, state: diri_proto::RemoteConnectionState) {
    // Reuse the existing mirror lock only at lifecycle transitions. Silent
    // sessions create no timer, new SSH operation, or repeated status event.
    let (changed, keyboard_changed) = {
        let mut remote = shared.remote_grid.lock().expect("remote grid");
        if let Some(remote) = remote.as_mut()
            && remote.connection.state != state
        {
            let keyboard_changed = state != diri_proto::RemoteConnectionState::Connected
                && remote.keyboard.committed.is_some();
            if state != diri_proto::RemoteConnectionState::Connected {
                // Retain the last image, but never encode new input from modes
                // observed before transport loss. A new validated seed restores them.
                remote.keyboard.committed = None;
                remote.keyboard.staged = None;
            }
            remote.connection = diri_proto::RemoteConnection {
                state,
                since: diri_proto::DateMillis::from(SystemTime::now()),
            };
            (true, keyboard_changed)
        } else {
            (false, false)
        }
    };
    if changed {
        shared.bump_state_version();
    }
    if keyboard_changed {
        shared.grid_wake.notify();
    }
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
    set_remote_connection(shared, diri_proto::RemoteConnectionState::Exited);
}

fn mark_remote_transport_failed(shared: &Shared) {
    let outcome = shared
        .reducer
        .lock()
        .expect("reducer")
        .reduce(StatusSignal::TransportUnavailable, SystemTime::now());
    apply(shared, &outcome);
    *shared.needs_input.lock().expect("needs input") = None;
    set_remote_connection(shared, diri_proto::RemoteConnectionState::Failed);
    // The last PID/grid remain observable. Neither transport failure nor an
    // uncertain write supplies an Agent exit code or permission to replay input.
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
                let (observation, replies) = {
                    let mut screen = shared.screen.lock().expect("screen");
                    let replies = screen.take_replies();
                    (
                        evaluate_if_screen_changed(
                            &shared,
                            &mut screen,
                            &engine,
                            &manifest_id,
                            &mut last_eval_seq,
                        ),
                        replies,
                    )
                };
                // Answers to the child's queries go back before anything else
                // is published: it is blocked reading them.
                if !replies.is_empty() {
                    use std::io::Write;
                    if let Ok(mut writer) = pty.lock().expect("pty").writer() {
                        let _ = writer.write_all(&replies);
                        let _ = writer.flush();
                    }
                }
                // A password prompt is printed beside the termios change that
                // hides its answer, so output is when the answer can differ.
                // Sampled before the wake so one publication carries both.
                let reading_secret = pty.lock().is_ok_and(|pty| pty.secret_input());
                record_secret_input(&shared, reading_secret);
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
            // Sample from the PTY owner, not `reader`: `tcgetpgrp` on the
            // live read fd can swallow canonical-mode input.
            if manifest_id == "shell" {
                let pgid = pty.lock().ok().and_then(|pty| pty.foreground_pgid());
                apply_foreground_sample(
                    &shared,
                    &manifest_id,
                    shared.child_pid.load(Ordering::SeqCst),
                    pgid,
                );
            }
            // Echo is usually restored just after the newline that ends a
            // password, with no output to mark it; the tick this loop already
            // takes is what notices.
            let reading_secret = pty.lock().is_ok_and(|pty| pty.secret_input());
            record_secret_input(&shared, reading_secret);
        }
    }

    // The stream ended: reap the child and record how it died.
    let exit = reap_direct(&shared, &pty, &mut reader, &mut buffer);
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

/// Stops a directly owned child: SIGTERM, then SIGKILL after `grace`.
///
/// The PTY lock is taken only for each signal and each reap attempt, never
/// across the wait. The pump needs that lock after every batch of output, and
/// on macOS a dying session leader is not reapable until the pump has read
/// what it left in the terminal: holding the lock while waiting for the exit
/// made the two wait on each other forever, under the Registry lock (#461).
fn terminate_direct(pty: &Mutex<Pty>, grace: Duration) -> std::io::Result<Exit> {
    let wait = |timeout: Duration| -> std::io::Result<Option<Exit>> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(exit) = pty.lock().expect("pty").try_wait()? {
                return Ok(Some(exit));
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
            std::thread::sleep(crate::pty::REAP_POLL_INTERVAL);
        }
    };
    pty.lock().expect("pty").kill_group(libc::SIGTERM)?;
    if let Some(exit) = wait(grace)? {
        return Ok(exit);
    }
    pty.lock().expect("pty").kill_group(libc::SIGKILL)?;
    wait(crate::pty::KILL_REAP_TIMEOUT)?.ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "the Agent did not exit after SIGKILL; the session remains tracked",
        )
    })
}

/// Reaps the direct child once the pump has left its loop.
///
/// The pump is the terminal's only reader, so it keeps reading (and
/// discarding) here: a child killed mid-output cannot finish exiting on macOS
/// until its output has been read. The PTY lock is held only per attempt, so
/// `terminate` can still signal a child that closed its terminal and lived on.
/// A stopped session gives up after [`crate::pty::KILL_REAP_TIMEOUT`] instead
/// of pinning the thread that joins this pump.
fn reap_direct(
    shared: &Shared,
    pty: &Mutex<Pty>,
    reader: &mut crate::pty::PtyStream,
    scratch: &mut [u8],
) -> Option<Exit> {
    let started = Instant::now();
    let mut stopped_at = None;
    let mut open = true;
    loop {
        if let Some(exit) = pty.lock().expect("pty").try_wait().ok()? {
            return Some(exit);
        }
        if shared.stop.load(Ordering::SeqCst)
            && stopped_at.get_or_insert_with(Instant::now).elapsed()
                >= crate::pty::KILL_REAP_TIMEOUT
        {
            return None;
        }
        // Prompt while an exit is imminent, then no more than a slow tick for
        // a child that outlives its terminal.
        let step = if started.elapsed() < Duration::from_secs(1) {
            crate::pty::REAP_POLL_INTERVAL
        } else {
            TICK_INTERVAL
        };
        if !open {
            std::thread::sleep(step);
            continue;
        }
        open = match reader.wait_readable(step) {
            Ok(true) => {
                use std::io::Read;
                match reader.read(scratch) {
                    Ok(0) => false,
                    Ok(_) => true,
                    Err(error) => matches!(
                        error.kind(),
                        std::io::ErrorKind::Interrupted | std::io::ErrorKind::WouldBlock
                    ),
                }
            }
            Ok(false) => true,
            Err(error) => error.kind() == std::io::ErrorKind::Interrupted,
        };
    }
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
    let blank_ceiling = blank_repaint_ceiling();
    let mut count = first;
    let mut total = 0usize;
    let mut filled_before = None;
    // When the screen first went blank, which is when its redraw budget
    // starts. Anchoring this to `started_at` instead would spend part of the
    // budget on whatever the batch did before the erase arrived, so a busy
    // session gets a shorter grace than an idle one and publishes the flash
    // this batching exists to prevent.
    let mut blank_since: Option<Instant> = None;
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
            if screen.has_notifications() {
                shared.bump_state_version();
            }
            let after = screen.filled_cells();
            (after < before, after == 0 && before != 0)
        };
        total += count;

        // A screen that recovered content is no longer mid-erase; a later
        // erase in the same batch starts its own budget.
        if !blank_repaint {
            blank_since = None;
        }

        let deadline = if blank_repaint {
            *blank_since.get_or_insert_with(Instant::now) + blank_ceiling
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
    fresh: bool,
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
                let mut screen = shared.screen.lock().expect("screen");
                if checkpoint
                    .keyboard_snapshot
                    .as_ref()
                    .is_some_and(|snapshot| !screen.can_restore_keyboard_snapshot(snapshot))
                {
                    return false;
                }
                let restored = screen.restore(
                    &checkpoint.history,
                    &checkpoint.grid,
                    checkpoint.alt_screen,
                    checkpoint.bracketed_paste,
                    checkpoint.mouse,
                );
                if restored {
                    shared
                        .keyboard_known
                        .store(checkpoint.keyboard.is_some(), Ordering::SeqCst);
                    if let Some(keyboard) = checkpoint.keyboard {
                        screen.restore_keyboard_state(keyboard);
                    }
                    if let Some(snapshot) = &checkpoint.keyboard_snapshot {
                        let restored_keyboard = screen.restore_keyboard_snapshot(snapshot);
                        debug_assert!(restored_keyboard);
                    }
                    screen.restore_history_metadata(&checkpoint.history_metadata);
                }
                restored
            });
        match restored {
            Some(checkpoint) => (
                checkpoint_path,
                checkpoint.log_offset,
                watcher,
                checkpoint.marker_buffer,
            ),
            None => {
                let start = log.preferred_replay_start(replay_budget);
                shared.keyboard_known.store(start == 0, Ordering::SeqCst);
                if start != 0 {
                    shared
                        .screen
                        .lock()
                        .expect("screen")
                        .invalidate_keyboard_enhancements();
                }
                (checkpoint_path, start, watcher, Vec::new())
            }
        }
    };
    // Adoption can restore a checkpoint concurrently with a freshly attached
    // App. One event is cheap and guarantees a seed that raced the restore is
    // corrected without bringing back periodic grid polling.
    shared.grid_wake.notify();
    let mut last_checkpoint_key: Option<CheckpointKey> = None;
    let mut checkpoint_dirty_at: Option<Instant> = None;
    let mut last_liveness = Instant::now();
    // When this session's foreground group was last sampled, and when bytes
    // last moved in either direction; see `held_foreground_sample_due`.
    let mut last_foreground_sample: Option<Instant> = None;
    let mut last_activity: Option<Instant> = None;
    let mut last_interaction_seen = shared.last_interaction.load(Ordering::Relaxed);
    let mut last_eval_seq = 0u64;
    let mut last_eval_at: Option<Instant> = None;
    // Set when a chunk was fed without detection running, so the settle below
    // knows the screen still needs judging.
    let mut eval_dirty = false;
    let mut last_scan_at = None;
    let mut last_scan_seq = 0u64;
    let mut exit_status: Option<HolderExitStatus> = None;
    let mut interactive_qos = false;
    // Set while a repaint is being assembled across more than one log read.
    let mut publish_pending: Option<Instant> = None;
    // Until the tail is first caught up, bytes are history, not activity:
    // they must render, but not flip a quiet adopted session to Working.
    let mut replaying = true;
    // Everything already in the log when this pump attached. A query below it
    // was asked before we were here and has either been answered or outlived
    // its asker; a query above it came from the running child and is owed an
    // answer, even if the pump has not finished draining the tail yet.
    let replay_until = if fresh {
        // Output from this newly launched child is live even if it reached
        // disk before the Engine pump started. Startup queries need answers.
        exit_marker_floor
    } else {
        let mut log = shared.log.lock().expect("log");
        // Refreshed first: the handle may predate output the holder has
        // already written, and a stale tail here would answer queries that
        // belong to a previous incarnation.
        log.refresh_from_disk();
        log.tail_offset()
    };

    // Output arrives one of two ways. The log always holds every byte, and
    // tailing it is all an older holder supports — but the file is written
    // asynchronously, so a screen fed from it waits on the filesystem. A
    // subscription delivers the same bytes as the holder reads them, with the
    // log still underneath: the stream can end or be dropped at any moment,
    // and the loop just goes back to reading the file from the offset it had
    // reached.
    let mut live: Option<crate::holder::HolderOutputStream> = None;
    // An adopted Holder keeps the same executable for this session's life.
    // Remember a completed negative negotiation; otherwise every log wakeup
    // reconnects just to receive the same unsupported-operation response.
    // Transport errors remain retryable, as do interrupted supported streams.
    let mut output_stream_supported = true;
    // Reused across passes: a fresh allocation per pass would charge every
    // byte of output an allocation it does not need.
    let mut live_run: Vec<u8> = Vec::new();
    // Where the subscription's first frame sits. The holder is ahead of the
    // log by whatever it has read but not yet written, so the file has to be
    // followed up to this point before a frame can be consumed.
    let mut live_from: u64 = 0;
    // Set once the log has been read to its end, which is the only safe moment
    // to subscribe.
    let mut drained = false;

    while !shared.stop.load(Ordering::SeqCst) && exit_status.is_none() {
        if shared.reset_requested.swap(false, Ordering::SeqCst) {
            // Between chunks every consumed byte has been fed, so the reset
            // and the checkpoint below describe the same point in the log.
            // The checkpoint is the durable replay boundary: a restart seeds
            // from it and replays only bytes written after this offset,
            // never the pre-reset output. Partial marker bytes travel with it.
            {
                let mut screen = shared.screen.lock().expect("screen");
                screen.reset();
                shared.keyboard_known.store(true, Ordering::SeqCst);
            }
            shared.reset_generation.fetch_add(1, Ordering::SeqCst);
            shared.bump_state_version();
            persist_checkpoint(
                &shared,
                &checkpoint_path,
                offset,
                &marker_buffer,
                &mut last_checkpoint_key,
            );
            checkpoint_dirty_at = None;
            shared.grid_wake.notify();
        }
        scan_artifacts_if_due(&shared, &mut last_scan_at, &mut last_scan_seq);
        // Subscribe only from a standing start, with the log drained.
        //
        // A subscription taken while still catching up is worse than none: the
        // holder begins filling a queue this loop is not yet reading, and once
        // that queue is full the pump waits on it for every chunk. Waiting for
        // a drained log keeps the handover to a few frames.
        if output_stream_supported && live.is_none() && drained {
            match client.open_output_stream() {
                Ok(Some(stream)) => {
                    // A drained log does not mean the holder is where the log
                    // ends: writes are queued, so it can be megabytes further on.
                    // Subscribing across that distance is the worst case — the
                    // holder fills a queue this loop cannot read until the file
                    // catches up, then drops it for being full, and both ends
                    // repeat. Take the subscription only when the gap is small
                    // enough to close immediately, and otherwise keep tailing and
                    // try again the next time the log runs dry.
                    live_from = stream.start_offset();
                    if live_from.saturating_sub(offset) <= LIVE_HANDOVER_GAP {
                        live = Some(stream);
                    }
                }
                Ok(None) => output_stream_supported = false,
                Err(_) => {}
            }
            drained = false;
        }
        // Frames only once the file has been followed up to where they begin.
        let streaming = live.is_some() && offset >= live_from;
        let from_log;
        let (start, chunk): (u64, &[u8]) = match live.as_mut().filter(|_| streaming) {
            Some(stream) => {
                // Once a redraw is pending, the next empty read is what
                // publishes it. Waiting the ordinary idle tick here held an
                // otherwise complete mouse-driven TUI frame for 100 ms. Keep
                // draining within the batch deadline, then publish even if
                // the child produces no more output. Truly idle sessions
                // retain their existing blocking wait.
                let timeout = publish_pending.map_or_else(
                    || shared.quiet_tick(),
                    |started| OUTPUT_BATCH_CEILING.saturating_sub(started.elapsed()),
                );
                match stream.next_run_into(timeout, LOG_READ_BUDGET, &mut live_run) {
                    // Contiguous by construction, and checked anyway: a run
                    // that does not start where the last one ended means
                    // something raced, and the log is the authority to fall
                    // back on rather than feed the emulator a gap it cannot
                    // detect.
                    Ok(Some(at)) if at == offset => (offset, &live_run[..]),
                    Ok(Some(_)) => {
                        live = None;
                        continue;
                    }
                    // Quiet: fall through to the timers with nothing to feed.
                    Ok(None) => (offset, &[][..]),
                    Err(_) => {
                        // Dropped for falling behind, or the child is gone.
                        // The log has every byte; resume from it. The open
                        // socket was standing in for the liveness probe, so
                        // the next quiet pass probes rather than waiting out
                        // an interval.
                        live = None;
                        last_liveness = Instant::now()
                            .checked_sub(LIVENESS_INTERVAL)
                            .unwrap_or(last_liveness);
                        continue;
                    }
                }
            }
            None => {
                let mut log = shared.log.lock().expect("log");
                log.refresh_from_disk();
                from_log = log.read(offset, LOG_READ_BUDGET);
                (from_log.0, &from_log.1[..])
            }
        };
        // A full read means the tail is not caught up, so this pass may hold
        // half a repaint. Publishing it would flicker; fold the rest into the
        // same frame instead — bounded, so a holder streaming faster than we
        // parse still updates.
        // "Nothing more is waiting." A short log read means the file is
        // drained; a live frame says nothing either way, since the holder may
        // already have more, so only an empty poll settles the stream.
        let caught_up = if streaming {
            chunk.is_empty()
        } else {
            chunk.len() < LOG_READ_BUDGET
        };

        if chunk.is_empty() {
            if publish_pending.take().is_some() {
                shared.grid_wake.notify();
            }
            // Output stopped with a screen detection has not judged yet: this
            // is the settle, and it is what makes throttling safe.
            if eval_dirty {
                eval_dirty = false;
                last_eval_at = Some(Instant::now());
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
                if let Some(observation) = observation {
                    let outcome = shared
                        .reducer
                        .lock()
                        .expect("reducer")
                        .reduce(StatusSignal::Screen(observation), SystemTime::now());
                    apply(&shared, &outcome);
                }
            }
            release_stalled_sync(&shared);
            if live.is_none() && !replaying {
                drained = true;
            }
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
                // A subscription already waited a tick for its frame; waiting
                // again here would add one to every quiet pass.
                _ if streaming => false,
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
            let interaction = shared.last_interaction.load(Ordering::Relaxed);
            if interaction != last_interaction_seen {
                last_interaction_seen = interaction;
                last_activity = Some(Instant::now());
            }
            if held_foreground_sample_due(
                last_activity.map(|at| at.elapsed()),
                last_foreground_sample.map(|at| at.elapsed()),
            ) {
                last_foreground_sample = Some(Instant::now());
                if sample_held_pty_facts(&shared, &client, &manifest_id) == Some(true) {
                    last_liveness = Instant::now();
                }
            }

            let liveness_interval = if streaming {
                STREAMING_LIVENESS_INTERVAL
            } else {
                LIVENESS_INTERVAL
            };
            if last_liveness.elapsed() >= liveness_interval {
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
        // Ordinary output carries no exit marker, and accumulating it only to
        // be handed the same bytes back costs several copies of everything a
        // session ever prints. When there is nothing buffered and nothing
        // marker-shaped in the chunk, it is fed where it lies.
        let staged;
        let output: &[u8] = if honored_from == 0
            && marker_buffer.is_empty()
            && HolderExitMarker::absent_from(chunk)
        {
            chunk
        } else {
            let mut assembled = Vec::new();
            if honored_from > 0 {
                marker_buffer.extend_from_slice(&chunk[..honored_from]);
                let (replayed, _stale_exit) = HolderExitMarker::drain(&mut marker_buffer);
                assembled.extend_from_slice(&replayed);
                if start + honored_from as u64 >= exit_marker_floor {
                    marker_buffer.clear(); // an unfinished stale marker ends here
                }
            }
            if honored_from < chunk.len() {
                marker_buffer.extend_from_slice(&chunk[honored_from..]);
                let (live, exit) = HolderExitMarker::drain(&mut marker_buffer);
                assembled.extend_from_slice(&live);
                if exit.is_some() {
                    exit_status = exit;
                }
            }
            staged = assembled;
            &staged
        };

        if !output.is_empty() {
            checkpoint_dirty_at = Some(Instant::now());
            // Detection snapshots the whole screen and walks it with the
            // manifest's patterns. That is cheap per screen and ruinous per
            // read: a session streaming output produces thousands of reads a
            // second, and paying for it on each one puts the pump behind the
            // holder — which shows up as latency, because a query the child
            // makes cannot be answered until the pump reaches it. While output
            // is still backed up it runs on a timer, and the moment the pump
            // catches up it runs again, so a settled screen is never stale.
            let evaluate_now = last_eval_at.is_none_or(|at: Instant| at.elapsed() >= EVAL_INTERVAL);
            eval_dirty = !evaluate_now;
            let (observation, replies) = {
                let mut screen = shared.screen.lock().expect("screen");
                let historical_bytes =
                    replay_until.saturating_sub(start).min(output.len() as u64) as usize;
                screen.feed_with_history(output, historical_bytes);
                if screen.has_notifications() {
                    shared.bump_state_version();
                }
                let replies = screen.take_replies();
                let observation = if evaluate_now {
                    last_eval_at = Some(Instant::now());
                    evaluate_if_screen_changed(
                        &shared,
                        &mut screen,
                        &engine,
                        &manifest_id,
                        &mut last_eval_seq,
                    )
                } else {
                    None
                };
                (observation, replies)
            };
            // The child is blocked reading the answer to its query, so send it
            // through the holder's input path before publishing anything.
            //
            // Not for replayed history: those queries were asked by a program
            // that has already moved on — often one that has exited — and the
            // answers would arrive as unsolicited keystrokes. They are taken
            // and dropped so a replayed query cannot leak into the live
            // stream.
            //
            // The boundary is the log tail at attach, not whether the tail has
            // been drained: a child that asks the moment it starts — which a
            // shell setting up its prompt does — would otherwise be answered
            // only if the pump happened to see an empty read first.
            let historical = offset <= replay_until;
            if !replies.is_empty() && !historical {
                let _ = client.write(&replies);
            }
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
                apply(&shared, &outcome);
            }
            drop(reducer);
            if !replaying {
                last_activity = Some(Instant::now());
                last_foreground_sample = Some(Instant::now());
                sample_held_pty_facts(&shared, &client, &manifest_id);
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
    // The marker is the last thing the Holder writes after draining the PTY,
    // so reaching it with nothing buffered means the retained screen is the
    // whole run. A partial marker means the drain is not proven; retain
    // nothing rather than something that may be missing its tail.
    if let Some(exit) = exit
        && marker_buffer.is_empty()
    {
        let (_, checkpoint) = capture_checkpoint(&shared, offset, &marker_buffer);
        let exit = match exit {
            Exit::Code(code) => diri_proto::ExitInfo {
                reason: diri_proto::ExitReason::Exited,
                code: Some(code),
                signal: None,
            },
            Exit::Signal(signal) => diri_proto::ExitInfo {
                reason: diri_proto::ExitReason::Signaled,
                code: None,
                signal: Some(signal),
            },
        };
        *shared.completed.lock().expect("completed capture") =
            Some(CompletedCapture { checkpoint, exit });
    }
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
#[derive(Clone, PartialEq)]
struct CheckpointKey {
    keyboard_snapshot: Option<diri_terminal_state::KeyboardSnapshot>,
    keyboard: Option<diri_proto::terminal_input::KeyboardState>,
    offset: u64,
    content_seq: u64,
    marker_bytes: usize,
    alt_screen: bool,
    bracketed_paste: bool,
    mouse: MouseModes,
}

/// The final terminal of a held child that genuinely exited, with the exit
/// facts the same marker carried. Built only after the log was drained to it.
pub(crate) struct CompletedCapture {
    pub checkpoint: crate::checkpoint::ScreenCheckpoint,
    pub exit: diri_proto::ExitInfo,
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
    let (key, checkpoint) = capture_checkpoint(shared, offset, marker_buffer);
    if last_key.as_ref() == Some(&key) {
        return;
    }
    // A failed write must not stop the session; the checkpoint is a cache.
    if checkpoint.write_atomically(path).is_ok() {
        *last_key = Some(key);
    }
}

/// Samples the emulator into a checkpoint and the key that identifies it.
fn capture_checkpoint(
    shared: &Shared,
    offset: u64,
    marker_buffer: &[u8],
) -> (CheckpointKey, crate::checkpoint::ScreenCheckpoint) {
    let (
        history,
        history_metadata,
        grid,
        alt_screen,
        bracketed_paste,
        mouse,
        content_seq,
        keyboard,
        keyboard_snapshot,
    ) = {
        let mut screen = shared.screen.lock().expect("screen");
        (
            screen.history_snapshot(),
            screen.history_metadata(),
            screen.full_snapshot(),
            screen.is_alt_screen(),
            screen.bracketed_paste(),
            screen.mouse_modes(),
            screen.content_seq(),
            shared
                .keyboard_known
                .load(Ordering::SeqCst)
                .then(|| screen.input_keyboard_state())
                .flatten(),
            shared
                .keyboard_known
                .load(Ordering::SeqCst)
                .then(|| screen.keyboard_snapshot())
                .flatten(),
        )
    };
    let key = CheckpointKey {
        keyboard_snapshot: keyboard_snapshot.clone(),
        keyboard,
        offset,
        content_seq,
        marker_bytes: marker_buffer.len(),
        alt_screen,
        bracketed_paste,
        mouse,
    };
    let checkpoint = crate::checkpoint::ScreenCheckpoint {
        keyboard_snapshot,
        keyboard,
        log_offset: offset,
        history,
        history_metadata,
        grid,
        marker_buffer: marker_buffer.to_vec(),
        alt_screen,
        bracketed_paste,
        mouse,
    };
    (key, checkpoint)
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
mod quiet_tick_tests {
    use super::*;

    #[test]
    fn only_an_idle_untouched_session_takes_the_stretched_tick() {
        let temp = tempfile::tempdir().unwrap();
        let (engine, _) = ManifestEngine::load_dir(&crate::detect::bundled_manifest_dir()).unwrap();
        let spec = SessionSpec {
            id: "tick".into(),
            pty: PtySpec::new(vec!["/bin/sh".into()], "/tmp"),
            manifest_id: "generic".into(),
            authority: Authority::ProcessOnly,
            logs_dir: temp.path().to_path_buf(),
            holder: None,
            remote: None,
            defer_launch: false,
        };
        let log = OutputLog::writer(temp.path(), &spec.id).unwrap();
        let shared = new_shared(&spec, log, &engine, true);
        let wakeups_per_minute = |tick: Duration| 60_000 / tick.as_millis();

        // Starting and Working run debounce timers: fast.
        assert_eq!(shared.quiet_tick(), TICK_INTERVAL);
        *shared.status.lock().unwrap() = SessionStatus::Working;
        assert_eq!(shared.quiet_tick(), TICK_INTERVAL);

        // Idle and never touched: what a background remote tab is, and what
        // the remote pump polled at the fast interval regardless.
        *shared.status.lock().unwrap() = SessionStatus::Idle;
        assert_eq!(shared.quiet_tick(), IDLE_TICK_INTERVAL);
        assert_eq!(wakeups_per_minute(TICK_INTERVAL), 600, "before");
        assert_eq!(wakeups_per_minute(shared.quiet_tick()), 60, "after");

        // Input or an attach makes it interactive again at once.
        shared.note_hot();
        assert_eq!(shared.quiet_tick(), TICK_INTERVAL);
    }
}

#[cfg(test)]
mod held_foreground_tests {
    use super::*;

    #[test]
    fn a_quiet_shell_is_asked_for_its_foreground_group_on_a_slow_backstop() {
        // Ten minutes of 100 ms ticks while a silent job runs: the old pump
        // connected to the holder on every one of them.
        let tick = Duration::from_millis(100);
        let mut since_sample: Option<Duration> = None;
        let mut samples = 0;
        let ticks = 6_000;
        for _ in 0..ticks {
            since_sample = since_sample.map(|since| since + tick);
            if held_foreground_sample_due(None, since_sample) {
                samples += 1;
                since_sample = Some(Duration::ZERO);
            }
        }
        assert_eq!(samples, 300, "one per liveness interval, down from {ticks}");

        // Output or input keeps every tick sampling until the job the echoed
        // newline announced has had time to take the terminal.
        assert!(held_foreground_sample_due(
            Some(Duration::from_millis(900)),
            Some(tick)
        ));
        assert!(!held_foreground_sample_due(
            Some(Duration::from_millis(1100)),
            Some(tick)
        ));
        // Never sampled yet: ask.
        assert!(held_foreground_sample_due(None, None));
    }
}

#[cfg(test)]
mod prompt_title_tests {
    use super::*;

    #[test]
    fn terminal_prompt_capture_survives_screen_timing_but_ignores_dialog_answers() {
        let temp = tempfile::tempdir().unwrap();
        let (engine, _) = ManifestEngine::load_dir(&crate::detect::bundled_manifest_dir()).unwrap();
        let engine = Arc::new(engine);
        for (index, initial) in [
            SessionStatus::Starting,
            SessionStatus::Idle,
            SessionStatus::Working,
            SessionStatus::NeedsInput(diri_proto::NeedsInputKind::Permission),
            SessionStatus::NeedsInput(diri_proto::NeedsInputKind::Question),
        ]
        .into_iter()
        .enumerate()
        {
            let spec = SessionSpec {
                id: format!("prompt-{index}"),
                pty: PtySpec::new(vec!["/bin/sh".into()], "/tmp"),
                manifest_id: "codex".into(),
                authority: Authority::ScreenPrimary,
                logs_dir: temp.path().to_path_buf(),
                holder: None,
                remote: None,
                defer_launch: true,
            };
            // Keep the real Session input/reducer path, but hold input in the
            // pre-launch queue so a PTY pump cannot race our status timeline.
            let session = Session {
                shared: new_shared(
                    &spec,
                    OutputLog::writer(temp.path(), &spec.id).unwrap(),
                    &engine,
                    true,
                ),
                transport: Transport::Held(HolderClient::new(temp.path().join("unused.sock"))),
                pump: None,
                manifest_id: spec.manifest_id.clone(),
                deferred: Some(Arc::new(DeferredLaunch::new())),
            };
            *session.shared.status.lock().unwrap() = initial.clone();
            for byte in b"Fix chat naming" {
                session.write_input(&[*byte]).unwrap();
            }
            // A screen repaint can change the status between typing and Enter.
            *session.shared.status.lock().unwrap() = SessionStatus::Working;
            session.write_input(b"\r").unwrap();
            if matches!(initial, SessionStatus::NeedsInput(_)) {
                assert_eq!(session.view().title, None);
                session.write_input(b"Real conversation prompt").unwrap();
                session.write_input(b"\r").unwrap();
                assert_eq!(
                    session.view().title.as_deref(),
                    Some("Real conversation prompt")
                );
            } else {
                assert_eq!(session.view().title.as_deref(), Some("Fix chat naming"));
            }
        }
    }

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
    #[test]
    fn negotiated_unknown_keyboard_is_not_legacy_and_reseed_does_not_reuse_flags() {
        use super::RemoteKeyboardProjection;
        use diri_proto::remote_pty::InputModes;
        use diri_proto::terminal_input::KeyboardState;
        let enhanced = KeyboardState {
            enhancements: Some(5.try_into().unwrap()),
            ..Default::default()
        };
        let mut old = RemoteKeyboardProjection {
            required: true,
            ..Default::default()
        };
        old.staged = Some(InputModes {
            sequence: 2,
            keyboard: Some(enhanced),
        });
        assert!(old.state_for(2).is_err());
        old.staged = Some(InputModes {
            sequence: 2,
            keyboard: None,
        });
        assert!(old.state_for(2).is_err());
        old.staged = Some(InputModes {
            sequence: 2,
            keyboard: Some(enhanced.legacy_projection()),
        });
        assert_eq!(old.state_for(2).unwrap().unwrap().enhancements, None);
        let mut new = RemoteKeyboardProjection {
            required: true,
            enhanced: true,
            ..Default::default()
        };
        new.staged = Some(InputModes {
            sequence: 2,
            keyboard: Some(enhanced),
        });
        new.commit(new.state_for(2).unwrap());
        new.staged = Some(InputModes {
            sequence: 3,
            keyboard: None,
        });
        assert_eq!(new.committed, Some(enhanced));
        assert!(new.state_for(4).is_err());
        new.commit(new.state_for(3).unwrap());
        assert_eq!(new.committed, None);
        assert!(
            new.state_for(3).is_err(),
            "committed prefixes cannot be reused"
        );
    }

    #[test]
    fn remote_keyboard_state_is_unknown_until_matching_grid_commit() {
        use super::RemoteKeyboardProjection;
        use diri_proto::remote_pty::InputModes;
        use diri_proto::terminal_input::KeyboardState;
        let state = KeyboardState {
            enhancements: None,
            application_cursor_keys: true,
            application_keypad: false,
        };
        let mut projection = RemoteKeyboardProjection {
            required: true,
            ..Default::default()
        };
        assert!(projection.state_for(8).is_err());
        projection.staged = Some(InputModes {
            sequence: 8,
            keyboard: Some(state),
        });
        assert_eq!(projection.committed, None);
        assert!(projection.state_for(7).is_err());
        let matched = projection.state_for(8).unwrap();
        assert_eq!(
            projection.committed, None,
            "validation alone must not publish"
        );
        projection.commit(matched);
        assert_eq!(projection.committed, Some(state));
        assert!(projection.staged.is_none());
        projection = RemoteKeyboardProjection {
            required: true,
            ..Default::default()
        };
        assert!(
            projection.state_for(8).is_err(),
            "reconnect does not reuse old state"
        );
        assert_eq!(
            RemoteKeyboardProjection::default().state_for(8).unwrap(),
            None
        );
    }

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

        let unchanged = wake.wait_for_change(observed, Duration::from_millis(1));
        assert_eq!(unchanged.generation, observed);
        assert!(!unchanged.interactive);

        wake.notify();
        let changed = wake.wait_for_change(observed, Duration::from_secs(1));
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

#[cfg(test)]
mod notification_tests {
    use super::*;

    #[test]
    fn remote_notifications_ignore_replay_and_duplicate_offsets_without_changing_status() {
        let temp = tempfile::tempdir().unwrap();
        let (engine, _) = ManifestEngine::load_dir(&crate::detect::bundled_manifest_dir()).unwrap();
        let spec = SessionSpec {
            id: "notification-replay".into(),
            pty: PtySpec::new(vec!["/bin/sh".into()], "/tmp"),
            manifest_id: "shell".into(),
            authority: Authority::ProcessOnly,
            logs_dir: temp.path().to_path_buf(),
            holder: None,
            remote: None,
            defer_launch: false,
        };
        let log = OutputLog::open(temp.path(), &spec.id, 4096, 8192, false).unwrap();
        let shared = new_shared(&spec, log, &engine, true);
        let mut seq = 0;
        let bytes = b"\x1b]777;notify;Build;Passed\x07";
        let end = apply_remote_output(&shared, &engine, "shell", &mut seq, 0, bytes, true).unwrap();
        assert!(!shared.screen.lock().unwrap().has_notifications());
        apply_remote_output(&shared, &engine, "shell", &mut seq, end, bytes, false).unwrap();
        let events = shared.screen.lock().unwrap().take_notifications();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].body, "Passed");
        assert!(shared.state_version.load(Ordering::SeqCst) > 0);
        apply_remote_output(&shared, &engine, "shell", &mut seq, end, bytes, false).unwrap();
        assert!(!shared.screen.lock().unwrap().has_notifications());
        assert_eq!(*shared.status.lock().unwrap(), SessionStatus::Idle);
    }
}

#[cfg(test)]
mod remote_stop_tests {
    use super::*;
    #[test]
    fn remote_stop_failure_preserves_only_already_observed_exit() {
        let temp = tempfile::tempdir().unwrap();
        let spec = SessionSpec {
            id: "stop-facts".into(),
            pty: PtySpec::new(vec!["/bin/sh".into()], "/"),
            manifest_id: "shell".into(),
            authority: Authority::ProcessOnly,
            logs_dir: temp.path().to_path_buf(),
            holder: None,
            remote: None,
            defer_launch: false,
        };
        let shared = new_shared(
            &spec,
            OutputLog::writer(temp.path(), &spec.id).unwrap(),
            &ManifestEngine::new(Vec::new()),
            true,
        );
        let pending = || std::io::Error::new(std::io::ErrorKind::TimedOut, "stop pending");
        assert_eq!(
            accept_remote_stop_result(&shared, Err(pending()))
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::TimedOut
        );
        assert!(!shared.exited.load(Ordering::SeqCst));
        assert!(shared.exit.lock().unwrap().is_none());
        assert!(
            accept_remote_stop_result(
                &shared,
                Ok(ProcessExit {
                    code: None,
                    signal: None
                })
            )
            .is_err()
        );
        assert!(!shared.exited.load(Ordering::SeqCst));
        assert_eq!(
            accept_remote_stop_result(
                &shared,
                Ok(ProcessExit {
                    code: Some(42),
                    signal: None
                })
            )
            .unwrap(),
            Exit::Code(42)
        );
        assert!(shared.exited.load(Ordering::SeqCst));
        assert_eq!(
            accept_remote_stop_result(&shared, Err(pending())).unwrap(),
            Exit::Code(42)
        );
    }
}

#[cfg(test)]
mod preview_tests {
    use super::*;

    #[test]
    fn legacy_unknown_input_remains_usable_but_enhanced_cache_loss_rejects_every_input_path() {
        let temp = tempfile::tempdir().unwrap();
        let (engine, _) = ManifestEngine::load_dir(&crate::detect::bundled_manifest_dir()).unwrap();
        let spec = SessionSpec {
            id: "preview-cold".into(),
            pty: PtySpec::new(vec!["/bin/sh".into()], "/tmp"),
            manifest_id: "generic".into(),
            authority: Authority::ProcessOnly,
            logs_dir: temp.path().to_path_buf(),
            holder: None,
            remote: None,
            defer_launch: true,
        };
        let session = Session {
            shared: new_shared(
                &spec,
                OutputLog::writer(temp.path(), &spec.id).unwrap(),
                &engine,
                true,
            ),
            transport: Transport::Held(HolderClient::new(temp.path().join("unused.sock"))),
            pump: None,
            manifest_id: spec.manifest_id.clone(),
            deferred: Some(Arc::new(DeferredLaunch::new())),
        };
        session.shared.keyboard_known.store(false, Ordering::SeqCst);
        assert_eq!(session.keyboard_state(), None);
        assert!(session.allows_keyboard_controller(false));
        assert!(session.accepts_keyboard_input(false));
        session.write_input(b"legacy").unwrap();
        session.shared.keyboard_known.store(true, Ordering::SeqCst);
        session
            .shared
            .screen
            .lock()
            .unwrap()
            .restore_keyboard_state(Default::default());
        assert_eq!(session.keyboard_state().unwrap().enhancements, None);
        assert!(session.accepts_keyboard_input(false));
        session.shared.last_hot.store(0, Ordering::Relaxed);
        session.shared.last_interaction.store(0, Ordering::Relaxed);
        let mut enabled = HeadlessScreen::new_with_keyboard_enhancements(80, 24);
        enabled.feed(b"\x1b[>5u");
        let complete = enabled.keyboard_snapshot().unwrap();
        enabled.restore_keyboard_state(Default::default());
        *session.shared.screen.lock().unwrap() = enabled;
        assert_eq!(session.keyboard_state(), None);
        assert!(!session.allows_keyboard_controller(false));
        assert!(session.allows_keyboard_controller(true));
        for capable in [false, true] {
            assert!(!session.accepts_keyboard_input(capable));
        }
        let queued = session
            .deferred
            .as_ref()
            .unwrap()
            .state
            .lock()
            .unwrap()
            .queued_input
            .clone();
        assert!(session.write_input(b"raw").is_err());
        assert!(session.send_text("paste", false).is_err());
        assert!(session.submit_input().is_err());
        assert_eq!(
            session
                .deferred
                .as_ref()
                .unwrap()
                .state
                .lock()
                .unwrap()
                .queued_input,
            queued
        );
        assert_eq!(session.shared.last_hot.load(Ordering::Relaxed), 0);
        assert_eq!(session.shared.last_interaction.load(Ordering::Relaxed), 0);
        assert!(
            session
                .shared
                .screen
                .lock()
                .unwrap()
                .restore_keyboard_snapshot(&complete)
        );
        assert_eq!(
            session
                .keyboard_state()
                .unwrap()
                .enhancements
                .unwrap()
                .bits(),
            5
        );
        assert!(!session.accepts_keyboard_input(false));
        assert!(session.accepts_keyboard_input(true));
    }

    #[test]
    fn observing_a_deferred_grid_does_not_refresh_activity_or_launch() {
        let temp = tempfile::tempdir().unwrap();
        let (engine, _) = ManifestEngine::load_dir(&crate::detect::bundled_manifest_dir()).unwrap();
        let spec = SessionSpec {
            id: "preview-cold".into(),
            pty: PtySpec::new(vec!["/bin/sh".into()], "/tmp"),
            manifest_id: "generic".into(),
            authority: Authority::ProcessOnly,
            logs_dir: temp.path().to_path_buf(),
            holder: None,
            remote: None,
            defer_launch: true,
        };
        let session = Session {
            shared: new_shared(
                &spec,
                OutputLog::writer(temp.path(), &spec.id).unwrap(),
                &engine,
                true,
            ),
            transport: Transport::Held(HolderClient::new(temp.path().join("unused.sock"))),
            pump: None,
            manifest_id: spec.manifest_id.clone(),
            deferred: Some(Arc::new(DeferredLaunch::new())),
        };
        session.shared.last_hot.store(0, Ordering::Relaxed);
        session.shared.screen.lock().unwrap().feed(b"last received");
        let seed = session.preview_seed();
        assert!(seed.grid.is_full_snapshot);
        assert_eq!(session.shared.last_hot.load(Ordering::Relaxed), 0);
        assert_eq!(session.shared.last_interaction.load(Ordering::Relaxed), 0);
        assert!(session.deferred.is_some());
        assert!(session.pump.is_none());
    }
}

#[cfg(test)]
mod remote_connection_tests {
    use super::*;
    use crate::remote::{
        binding::RemoteBindingStore,
        bootstrap::RemoteTarget,
        executor::ProcessExecutor,
        manager::{ArtifactCatalog, InstalledHelper, RemoteManager},
        ssh::SshTransport,
    };
    use diri_proto::{HostEntry, RemoteConnectionState as State};
    use std::os::unix::fs::PermissionsExt;

    fn wait_for(label: &str, mut predicate: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !predicate() {
            assert!(Instant::now() < deadline, "timed out: {label}");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    struct ChildGuard(std::process::Child);
    impl Drop for ChildGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[test]
    fn bridge_loss_preserves_process_and_grid_until_a_validated_reconnect() {
        for fatal in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            // The fixture process outlives both synthetic SSH bridges. It exits126
            // only after the test requests and observes that actual process exit.
            let mut process = ChildGuard(
                std::process::Command::new("/bin/sh")
                    .args([
                        "-c",
                        "while [ ! -f process-exit ]; do sleep 0.01; done; exit 126",
                    ])
                    .current_dir(temp.path())
                    .spawn()
                    .unwrap(),
            );
            let pid = process.0.id();
            let hello = RemoteMessage::HelloAck(diri_proto::remote_pty::HelloAck {
                protocol: diri_proto::remote_pty::ProtocolVersion::CURRENT,
                holder_build_id: "fixture".into(),
                session_incarnation: "same-incarnation".into(),
                capabilities: diri_proto::remote_pty::ANNOTATED_HOLDER_CAPABILITIES.to_vec(),
                controller_epoch: 1,
                process_state: RemoteProcessState::Running { pid },
                output_offset: 0,
                snapshot_sequence: 1,
                foreground_pid: Some(pid as i32),
                child_identity: None,
            });
            let mut screen = crate::screen::HeadlessScreen::new(80, 24);
            screen.feed(b"stable remote image");
            let snapshot = RemoteMessage::FullSnapshot(FullSnapshot {
                sequence: 1,
                alt_screen: false,
                bracketed_paste: false,
                mouse: Default::default(),
                grid: screen.full_snapshot(),
            });
            // A capable controller receives the reset boundary before every
            // full snapshot, even when no reset has ever happened.
            let boundary =
                RemoteMessage::TerminalResetState(diri_proto::remote_pty::TerminalResetState {
                    incarnation: "same-incarnation".into(),
                    generation: 0,
                    sequence: 1,
                    output_offset: 0,
                });
            for (name, message) in [
                ("hello.bin", hello),
                ("reset.bin", boundary),
                ("snapshot.bin", snapshot),
                (
                    "modes.bin",
                    RemoteMessage::InputModes(diri_proto::remote_pty::InputModes {
                        sequence: 1,
                        keyboard: Some(diri_proto::terminal_input::KeyboardState {
                            enhancements: None,
                            application_cursor_keys: true,
                            application_keypad: true,
                        }),
                    }),
                ),
                (
                    "fatal.bin",
                    RemoteMessage::Terminal(diri_proto::frames::Frame::input(
                        b"invalid direction".to_vec(),
                    )),
                ),
                (
                    "exit.bin",
                    RemoteMessage::ProcessExit(ProcessExit {
                        code: Some(126),
                        signal: None,
                    }),
                ),
            ] {
                std::fs::write(
                    temp.path().join(name),
                    RemoteCodec::encode(&message).unwrap(),
                )
                .unwrap();
            }
            let fake = temp.path().join("ssh");
            std::fs::write(
                &fake,
                r#"#!/bin/sh
cd "$(dirname "$0")" || exit 1
printf x >> attaches
if mkdir first 2>/dev/null; then
  cat hello.bin modes.bin
  while [ ! -f seed ]; do sleep 0.01; done
  cat reset.bin snapshot.bin
  while [ ! -f disconnect ]; do sleep 0.01; done
else
  while [ ! -f reconnect ]; do sleep 0.01; done
  cat hello.bin modes.bin reset.bin snapshot.bin
  while [ ! -f finish ]; do sleep 0.01; done
  if [ -f fatal ]; then cat fatal.bin; else cat exit.bin; fi
fi
"#,
            )
            .unwrap();
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o700)).unwrap();
            let manager = Arc::new(
                RemoteManager::new(
                    ProcessExecutor::new(&fake),
                    ArtifactCatalog::without_artifacts_for_test(),
                    temp.path().join("control"),
                )
                .unwrap(),
            );
            let host = HostEntry {
                id: "fixture".into(),
                name: None,
                ssh: "fixture".into(),
                default_cwd: None,
                node: None,
            };
            let helper = InstalledHelper {
                target: RemoteTarget::MacosAarch64,
                build_id: "fixture".into(),
                protocol: diri_proto::remote_pty::ProtocolVersion::CURRENT,
                transport: SshTransport::new(&host, temp.path().join("control/socket"))
                    .with_executable(&fake),
            };
            let (engine, _) =
                ManifestEngine::load_dir(&crate::detect::bundled_manifest_dir()).unwrap();
            let session = Session::adopt_remote(
                SessionSpec {
                    id: "remote-fixture".into(),
                    pty: PtySpec::new(vec!["/bin/sh".into()], temp.path()),
                    manifest_id: "generic".into(),
                    authority: Authority::ProcessOnly,
                    logs_dir: temp.path().join("logs"),
                    holder: None,
                    remote: None,
                    defer_launch: false,
                },
                RemoteAdoptSpec {
                    manager,
                    helper,
                    token: diri_proto::remote_pty::SessionToken::new("remote-fixture-token")
                        .unwrap(),
                    incarnation: "same-incarnation".into(),
                    binding_store: RemoteBindingStore::new(temp.path().join("bindings")).unwrap(),
                    output_offset: 0,
                },
                Arc::new(engine),
            )
            .unwrap();
            assert!(
                !session.accepts_keyboard_input(true),
                "new enhanced owner requires a validated seed before queueing input"
            );
            assert!(session.write_input(b"must-not-queue-before-seed").is_err());
            wait_for("HelloAck", || session.child_pid() == pid as i32);
            assert_eq!(
                session.view().remote_connection.unwrap().state,
                State::Connecting
            );
            assert!(
                session
                    .shared
                    .remote_grid
                    .lock()
                    .unwrap()
                    .as_ref()
                    .unwrap()
                    .mirror
                    .sequence()
                    .is_none()
            );
            wait_for("staged input modes", || {
                session
                    .shared
                    .remote_grid
                    .lock()
                    .unwrap()
                    .as_ref()
                    .unwrap()
                    .keyboard
                    .staged
                    .is_some()
            });
            assert_eq!(
                session.keyboard_state(),
                None,
                "modes wait for their validated grid"
            );
            std::fs::write(temp.path().join("seed"), "").unwrap();
            wait_for("validated snapshot", || {
                session.view().remote_connection.unwrap().state == State::Connected
            });
            let keyboard = Some(diri_proto::terminal_input::KeyboardState {
                enhancements: None,
                application_cursor_keys: true,
                application_keypad: true,
            });
            assert_eq!(session.keyboard_state(), keyboard);
            let seed = session.preview_seed().grid;
            let connected = session.view().remote_connection.unwrap();
            let version = session.state_version();
            set_remote_connection(&session.shared, State::Connected);
            assert_eq!(
                session.state_version(),
                version,
                "silent/repeated state has no new event"
            );
            assert_eq!(session.view().remote_connection.unwrap(), connected);
            std::fs::write(temp.path().join("disconnect"), "").unwrap();
            wait_for("bridge EOF", || {
                session.view().remote_connection.unwrap().state == State::Reconnecting
            });
            assert_eq!(
                session.keyboard_state(),
                None,
                "EOF clears stale input modes immediately"
            );
            assert_eq!(session.child_pid(), pid as i32);
            assert!(process.0.try_wait().unwrap().is_none());
            assert_eq!(session.preview_seed().grid, seed);
            assert!(!session.view().exited);
            std::fs::write(temp.path().join("reconnect"), "").unwrap();
            wait_for("reconnected snapshot", || {
                session.view().remote_connection.unwrap().state == State::Connected
            });
            assert_eq!(session.child_pid(), pid as i32);
            assert_eq!(session.preview_seed().grid, seed);
            assert_eq!(session.keyboard_state(), keyboard);
            if fatal {
                std::fs::write(temp.path().join("fatal"), "").unwrap();
                std::fs::write(temp.path().join("finish"), "").unwrap();
                wait_for("fatal transport", || {
                    session.view().remote_connection.unwrap().state == State::Failed
                });
                assert_eq!(session.keyboard_state(), None);
                assert_eq!(session.view().status, SessionStatus::Unknown);
                assert!(!session.view().exited);
                assert!(!crate::events::satisfies_wait_target(
                    &session.view().status,
                    "exited"
                ));
                assert!(session.shared.exit.lock().unwrap().is_none());
                assert_eq!(session.child_pid(), pid as i32);
                assert_eq!(session.preview_seed().grid, seed);
                assert!(process.0.try_wait().unwrap().is_none());
                assert!(session.write_input(b"never replay").is_err());
                assert!(session.resize(132, 42).is_err());
                assert_eq!(std::fs::read(temp.path().join("attaches")).unwrap(), b"xx");
                continue;
            }
            std::fs::write(temp.path().join("process-exit"), "").unwrap();
            assert_eq!(process.0.wait().unwrap().code(), Some(126));
            std::fs::write(temp.path().join("finish"), "").unwrap();
            wait_for("real exit126 fact", || session.view().exited);
            wait_for("exit clears keyboard modes", || {
                session.keyboard_state().is_none()
            });
            assert_eq!(
                session.view().remote_connection.unwrap().state,
                State::Exited
            );
            assert!(
                matches!(session.view().status, SessionStatus::Exited(info) if info.code == Some(126))
            );
        }
    }
}
