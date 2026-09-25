//! Terminal pane composition.
//!
//! The daemon remains authoritative: this module only composes
//! `diri-client::SessionAttachment`, `diri-term`, and the T9 session store.

mod controller;
use controller::{AttachmentControl, ControllerLease};
mod find_input;
mod find_overlay;
#[cfg(all(test, target_os = "macos"))]
pub(crate) mod find_workflow_tests;
mod qol;
mod reconnect;
use qol::QolState;

use std::collections::{HashMap, HashSet, VecDeque};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use diri_client::attachment::{SessionAttachment, TerminalChunk};
use diri_proto::grid::GridUpdate;
use diri_proto::terminal::{
    MouseModes, TerminalMouseButton, TerminalMouseEvent, TerminalMouseModifiers, encode_mouse_event,
};
use diri_proto::{
    AgentKind as ProtoAgentKind, ExitReason, Resumability, RiskHint, SessionId, SessionRecord,
    SessionStatus,
};
use diri_term::buffer::GridBuffer;
use diri_term::element::{SharedGridBuffer, TerminalElement, TerminalReference};
use diri_term::find::{
    FindSearchScheduler, FindSnapshot, ReadCompletion, SearchRequest, SearchResult,
    TerminalFindModel,
};
use diri_term::keys::{
    Key as TermKey, KeyEvent as TermKeyEvent, Modifiers as TermModifiers, NamedKey, paste,
};
#[cfg(test)]
use diri_term::keys::{TermInputModes, encode_key};
use diri_term::metrics::CellMetrics;
use diri_term::scrollback::{WheelDelta, WheelEvent, WheelRoute};
use diri_term::theme::TermTheme;
use diri_ui::{
    AgentKind as UiAgentKind, Fill, FloatingSurface, Ink, Metrics, Radius, SemanticColors,
    StatusGlyph, StatusState, Typo,
};
use gpui::{
    AnyElement, ClipboardEntry, ClipboardItem, Context, Entity, EventEmitter, ExternalPaths,
    FocusHandle, KeyDownEvent, KeyUpEvent, ModifiersChangedEvent, MouseButton, Render, Role,
    ScrollDelta, ScrollWheelEvent, SharedString, StatefulInteractiveElement, Task, Window, div,
    font, prelude::*, px,
};
use tokio::runtime::Handle;
use tokio::sync::mpsc;

use crate::clipboard_transfer::{StagedClipboardImage, upload_dropped_file};
use crate::commands::{
    CloseFind, CopySelection, FindNext, FindPrevious, OpenFind, Paste, ResetZoom, TERMINAL_CONTEXT,
    ToggleInspector, ToggleSidebar, ZoomIn, ZoomOut,
};
use crate::external_drop::{TerminalDropAction, plan_terminal_drop, terminal_drop_text};
use crate::haptics::{self, Haptic};
use crate::icons::{SymbolWeight, sf_symbol, sf_symbol_weighted};
use crate::navigation::NavigationOverlay;
use crate::query_editor::{self, ClipboardEdit, Edit, QueryEditor};
use crate::quote::{Quote, QuoteSource};
use crate::session_surfaces::switcher_key;
use crate::store::StoreRuntime;
use crate::surface_shell::UtilitySurfaces;

const GRID_HORIZONTAL_PADDING: f32 = 24.0;
const GRID_VERTICAL_PADDING: f32 = 12.0;
// The outer terminal card has a one-pixel border on both sides and the pane
// adds its own left divider. These pixels are outside TerminalElement's actual
// paint bounds and therefore cannot be offered to the PTY as a text column.
const GRID_LAYOUT_HORIZONTAL_CHROME: f32 = 3.0;
const GRID_LAYOUT_VERTICAL_CHROME: f32 = 2.0;
const REATTACH_DELAY: Duration = Duration::from_millis(500);
const PANE_EVENT_QUEUE_CAPACITY: usize = 256;
/// How often a live drag is allowed to push a new PTY geometry. Matched to the
/// daemon's coalesced grid flush (also 8ms): resizing faster produces frames
/// the client can never see, resizing slower makes the drag look like it snaps
/// at the end instead of reflowing under the cursor.
const RESIZE_CADENCE: Duration = Duration::from_millis(8);
/// Cell motion is redundant above display cadence. This bounds DECSET 1003
/// writes while still allowing one report per rendered frame on high-refresh
/// displays; repeated moves within one cell are suppressed altogether.
const MOUSE_MOTION_CADENCE: Duration = Duration::from_millis(8);
/// Two resizes further apart than this belong to different gestures. A drag
/// steps faster than this and must keep reflowing live; anything slower is a
/// discrete change -- a panel toggle, a window snap, a font-size change --
/// whose reflow is held still by [`REFLOW_HOLD`]. Matched to the window the
/// daemon uses to infer the same thing (`AgentSession.resizeDragWindow`).
const RESIZE_GESTURE_GAP: Duration = Duration::from_millis(200);
/// Ceiling on how long the grid is held still across a column change.
///
/// A cols-only resize comes back in two stages: the daemon re-wraps its
/// emulator and broadcasts that immediately, then the program answers SIGWINCH
/// and repaints. Painting the first stage is what made a sidebar toggle shove
/// the content up and drop it back a frame later -- re-wrapping at a fixed row
/// count spills the top into scrollback, and the grid is painted top-anchored
/// on row index, so every surviving line moves up until the program's repaint
/// puts it back. Holding both stages and applying them as one paint removes
/// the intermediate frame entirely. The hold ends as soon as the program's
/// repaint lands, so this bound only applies to one that is slow or absent.
const REFLOW_HOLD: Duration = Duration::from_millis(140);
/// Slack added to a bottom-anchored grid's height so layout rounding can never
/// shave its last row off. See `TerminalPane::grid_row_overflow`.
const ANCHOR_SLACK: f32 = 1.0;
/// How many evicted sessions keep their last-known grid parked for instant
/// re-selection. Cells only (~100KB each) — elements, channels, and shape
/// caches are rebuilt on promotion — so the ceiling is a memory bound, not a
/// residency one.
const PARKED_GRID_CAP: usize = 12;
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TerminalPaneEvent {
    ContinueAccount(SessionId),
    OpenFileReference {
        reference: String,
        cwd: String,
        session_id: SessionId,
    },
    /// Transient terminal feedback belongs in the window's standard toast.
    Feedback {
        message: String,
    },
    /// Files were dropped on the grid and some (or all) could not be used.
    ExternalDropFeedback {
        message: String,
    },
}

#[path = "session_links.rs"]
mod session_links;
use session_links::SessionLinks;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AttachmentState {
    Attaching,
    Live,
    Reconnecting,
}

/// A reconnect has no trustworthy child modes until its fresh seed arrives.
/// Keeping bracketed paste across that gap can send control framing to a shell
/// that never requested it, so every non-live transition returns to the wire
/// protocol's backward-compatible raw-paste default.
fn bracketed_paste_after_attachment_state(current: bool, state: AttachmentState) -> bool {
    if state == AttachmentState::Live {
        current
    } else {
        false
    }
}

/// Secure Keyboard Entry silences event taps in every app, so it is wanted
/// only while keys typed right now are the secret: the child is reading one,
/// and this pane is where the keyboard is pointed.
fn secure_input_wanted(secret_input: bool, focused: bool, window_active: bool) -> bool {
    secret_input && focused && window_active
}

fn terminal_paste(text: &str, bracketed_paste: bool) -> Vec<u8> {
    paste(text, bracketed_paste)
}

/// Claude recognizes dropped image paths in its bracketed-paste handler.
/// A recovered local screen can miss DECSET 2004 when checkpoint recovery
/// falls back to a bounded log tail (Claude usually enables it only at startup).
/// Its direct-launch session kind guarantees the recipient supports framing;
/// shell and unknown sessions must still use only their negotiated mode.
fn terminal_file_paste(
    text: &str,
    bracketed_paste: bool,
    kind: Option<&ProtoAgentKind>,
) -> Vec<u8> {
    terminal_paste(
        text,
        bracketed_paste || kind == Some(&ProtoAgentKind::CLAUDE_CODE),
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PointerOwner {
    LocalSelection,
    LocalReference,
    Terminal,
    Ignored,
}

#[derive(Debug)]
struct PendingMouseMotion {
    cell: (u16, u16),
    bytes: Vec<u8>,
}

#[derive(Debug, Eq, PartialEq)]
enum MotionDispatch {
    SendNow(Vec<u8>),
    Schedule { delay: Duration, generation: u64 },
    None,
}

#[derive(Default)]
struct MouseMotionLimiter {
    last_sent_at: Option<Instant>,
    last_cell: Option<(u16, u16)>,
    pending: Option<PendingMouseMotion>,
    timer_generation: u64,
    timer_armed: bool,
}

impl MouseMotionLimiter {
    fn reset(&mut self) {
        self.last_sent_at = None;
        self.last_cell = None;
        self.cancel_pending();
    }

    fn push(&mut self, now: Instant, cell: (u16, u16), bytes: Vec<u8>) -> MotionDispatch {
        if self.last_cell == Some(cell) {
            // A pending intermediate cell is obsolete if the pointer returned
            // to the last position the child already observed.
            self.cancel_pending();
            return MotionDispatch::None;
        }
        let Some(elapsed) = self.last_sent_at.map(|sent| now.duration_since(sent)) else {
            self.last_sent_at = Some(now);
            self.last_cell = Some(cell);
            return MotionDispatch::SendNow(bytes);
        };
        if elapsed >= MOUSE_MOTION_CADENCE {
            self.cancel_pending();
            self.last_sent_at = Some(now);
            self.last_cell = Some(cell);
            return MotionDispatch::SendNow(bytes);
        }
        self.pending = Some(PendingMouseMotion { cell, bytes });
        if self.timer_armed {
            return MotionDispatch::None;
        }
        self.timer_armed = true;
        self.timer_generation = self.timer_generation.wrapping_add(1);
        MotionDispatch::Schedule {
            delay: MOUSE_MOTION_CADENCE - elapsed,
            generation: self.timer_generation,
        }
    }

    fn flush(&mut self, generation: u64, now: Instant) -> Option<Vec<u8>> {
        if !self.timer_armed || generation != self.timer_generation {
            return None;
        }
        self.timer_armed = false;
        let pending = self.pending.take()?;
        self.last_sent_at = Some(now);
        self.last_cell = Some(pending.cell);
        Some(pending.bytes)
    }

    /// Drains the latest held motion before a release, preserving PTY order.
    fn take_pending(&mut self) -> Option<Vec<u8>> {
        let pending = self.pending.take().map(|pending| pending.bytes);
        self.reset();
        pending
    }

    fn cancel_pending(&mut self) {
        self.pending = None;
        self.timer_armed = false;
        self.timer_generation = self.timer_generation.wrapping_add(1);
    }
}

enum AttachmentCommand {
    Input(Vec<u8>),
    Mouse(Vec<u8>),
    Resize(u16, u16),
    Scroll {
        direction: u8,
        lines: u16,
        col: u16,
        row: u16,
    },
}

#[cfg(test)]
type InputObserver = mpsc::UnboundedSender<(SessionId, Vec<u8>)>;

enum PaneEvent {
    ControllerDamage(SessionId, AttachmentGeneration, bool),
    InputFeedback(SessionId, String),
    AttachmentState(SessionId, AttachmentGeneration, AttachmentState),
    Chunk(SessionId, AttachmentGeneration, TerminalChunk),
    GridBatch(SessionId, AttachmentGeneration, Vec<GridUpdate>),
    FindSnapshot(
        SessionId,
        AttachmentGeneration,
        SearchRequest,
        Option<FindSnapshot>,
    ),
    FindResult(SessionId, AttachmentGeneration, SearchRequest, SearchResult),
    /// A scrollback reply, tagged like grid frames: a session id outlives the
    /// resident that asked, and a late reply must not reach its replacement.
    ScrollbackCells(
        SessionId,
        AttachmentGeneration,
        diri_proto::ReadScrollbackCellsResult,
        usize,
    ),
    ScrollbackFailed(SessionId, AttachmentGeneration),
    /// A history-length probe answered with the row where the live grid
    /// starts, or `None` when the read failed.
    HistoryExtent(SessionId, AttachmentGeneration, Option<i64>),
    /// The scroller knob moved the viewport; fetch whatever it now shows.
    ScrollbackPump(SessionId, usize),
    ClipboardUploadFinished(SessionId, Result<String, String>),
    /// Files dropped on a remote session finished copying; on success the
    /// remote paths are ready to paste in drop order.
    DroppedFilesUploaded(SessionId, Result<Vec<String>, String>),
}

/// Identifies one view residency, not the durable session or shared transport.
/// Replacing a view invalidates its pending search and UI completion events.
type AttachmentGeneration = u64;

/// Bounded, grid-aware handoff from transport tasks to the GPUI thread.
/// Terminal grids are state, not a log: one coalesced final update per session
/// is sufficient, while semantic events retain their order in a fixed queue.
#[derive(Clone)]
struct PaneEventSender {
    state: Arc<Mutex<PaneMailboxState>>,
    wake: mpsc::Sender<()>,
}

struct PaneEventReceiver {
    state: Arc<Mutex<PaneMailboxState>>,
    wake: mpsc::Receiver<()>,
}

/// The one completion a resident's single-flight find pipeline can be waiting
/// to deliver. It has a mailbox class of its own: semantic queue pressure must
/// never strand the scheduler in Reading or Scanning forever.
enum FindCompletion {
    Snapshot {
        generation: AttachmentGeneration,
        request: SearchRequest,
        snapshot: Option<FindSnapshot>,
    },
    Result {
        generation: AttachmentGeneration,
        request: SearchRequest,
        result: SearchResult,
    },
}

impl FindCompletion {
    const fn generation(&self) -> AttachmentGeneration {
        match self {
            Self::Snapshot { generation, .. } | Self::Result { generation, .. } => *generation,
        }
    }

    fn into_event(self, id: SessionId) -> PaneEvent {
        match self {
            Self::Snapshot {
                generation,
                request,
                snapshot,
            } => PaneEvent::FindSnapshot(id, generation, request, snapshot),
            Self::Result {
                generation,
                request,
                result,
            } => PaneEvent::FindResult(id, generation, request, result),
        }
    }
}

#[derive(Default)]
struct PaneMailboxState {
    events: VecDeque<PaneEvent>,
    /// At most one attachment generation's new baseline plus its final trailing
    /// diff. The two-frame boundary is observable by resize reflow holds and
    /// must not be erased.
    grids: HashMap<SessionId, (AttachmentGeneration, Vec<GridUpdate>)>,
    grid_order: VecDeque<SessionId>,
    /// Exactly one completion per session. Terminal residency bounds the
    /// producer set, and replacement generations overwrite detached work.
    find_completions: HashMap<SessionId, FindCompletion>,
    find_order: VecDeque<SessionId>,
}

fn pane_event_channel() -> (PaneEventSender, PaneEventReceiver) {
    let state = Arc::new(Mutex::new(PaneMailboxState::default()));
    let (wake_tx, wake_rx) = mpsc::channel(1);
    (
        PaneEventSender {
            state: Arc::clone(&state),
            wake: wake_tx,
        },
        PaneEventReceiver {
            state,
            wake: wake_rx,
        },
    )
}

impl PaneEventSender {
    fn send(&self, event: PaneEvent) -> Result<(), ()> {
        if self.wake.is_closed() {
            return Err(());
        }
        let mut state = self.state.lock().expect("pane event mailbox");
        match event {
            PaneEvent::Chunk(id, generation, TerminalChunk::Grid(update)) => {
                if let Some((queued_generation, batch)) = state.grids.get_mut(&id) {
                    if generation < *queued_generation {
                        return Ok(());
                    }
                    if generation > *queued_generation {
                        *queued_generation = generation;
                        batch.clear();
                        batch.push(update);
                    } else {
                        let starts_new_baseline = update.is_full_snapshot
                            || batch.last().is_some_and(|last| {
                                last.cols != update.cols || last.rows != update.rows
                            });
                        if starts_new_baseline {
                            batch.clear();
                            batch.push(update);
                        } else if batch.len() == 1 && batch[0].is_full_snapshot {
                            batch.push(update);
                        } else if let Some(pending) = batch.last_mut() {
                            pending.coalesce(update);
                        }
                    }
                } else {
                    state.grid_order.push_back(id.clone());
                    state.grids.insert(id, (generation, vec![update]));
                }
            }
            PaneEvent::FindSnapshot(id, generation, request, snapshot) => {
                state.queue_find_completion(
                    id,
                    FindCompletion::Snapshot {
                        generation,
                        request,
                        snapshot,
                    },
                );
            }
            PaneEvent::FindResult(id, generation, request, result) => {
                state.queue_find_completion(
                    id,
                    FindCompletion::Result {
                        generation,
                        request,
                        result,
                    },
                );
            }
            event => {
                if state.events.len() >= PANE_EVENT_QUEUE_CAPACITY {
                    return Err(());
                }
                state.events.push_back(event);
            }
        }
        drop(state);
        // Capacity one turns any number of producer writes into one GPUI wake.
        let _ = self.wake.try_send(());
        Ok(())
    }
}

impl PaneEventReceiver {
    async fn recv_batch(&mut self, batch: &mut Vec<PaneEvent>) -> bool {
        if self.wake.recv().await.is_none() {
            return false;
        }
        let mut state = self.state.lock().expect("pane event mailbox");
        // Preserve ordinary semantic ordering, then apply every queued grid
        // before find completions. Grid damage increments the find content
        // generation, so a scan of the preceding screen can never flash stale
        // highlights for one rescan interval.
        batch.extend(state.events.drain(..));
        while let Some(id) = state.grid_order.pop_front() {
            if let Some((generation, updates)) = state.grids.remove(&id) {
                batch.push(PaneEvent::GridBatch(id, generation, updates));
            }
        }
        while let Some(id) = state.find_order.pop_front() {
            if let Some(completion) = state.find_completions.remove(&id) {
                batch.push(completion.into_event(id));
            }
        }
        true
    }
}

impl PaneMailboxState {
    fn queue_find_completion(&mut self, id: SessionId, completion: FindCompletion) {
        if let Some(queued) = self.find_completions.get(&id)
            && completion.generation() < queued.generation()
        {
            return;
        }
        if !self.find_completions.contains_key(&id) {
            self.find_order.push_back(id.clone());
        }
        self.find_completions.insert(id, completion);
    }
}

struct ResidentTerminal {
    controller: ControllerLease,
    keyboard: Option<diri_proto::terminal_input::KeyboardState>,
    element: TerminalElement,
    attachment: AttachmentControl,
    /// Rejects events that finished crossing to GPUI after this resident's
    /// predecessor was detached.
    attachment_generation: AttachmentGeneration,
    attachment_state: AttachmentState,
    /// Last mode advertised by this attachment generation. Reset while the
    /// transport is not live so paste never trusts state from a dead child.
    bracketed_paste: bool,
    /// The child is reading a secret (a line prompt with echo off). Reset with
    /// `bracketed_paste`: a dead transport says nothing about the child.
    secret_input: bool,
    find: Option<TerminalFindModel>,
    /// Single flight across both the daemon history read and blocking scan.
    /// One newer request may replace the dirty follow-up; snapshots never
    /// queue behind CPU work.
    find_scheduler: FindSearchScheduler,
    /// The editable text behind `find`'s query, so ⌘F gets the same caret,
    /// selection, and readline keys as the other query fields.
    find_query: QueryEditor,
    find_composition: find_input::Composition,
    last_size: (u16, u16),
    pointer_owner: Option<(MouseButton, PointerOwner)>,
    mouse_motion: MouseMotionLimiter,
    extent_probe: HistoryExtentProbe,
}

/// How often a knob shown over streaming output may re-ask how long the
/// history is.
const HISTORY_EXTENT_PROBE_INTERVAL: Duration = Duration::from_millis(250);

/// Single flight for the one-row reads that size the scroller knob at the
/// live edge, where grid updates say nothing about the history above them.
#[derive(Debug, Default)]
struct HistoryExtentProbe {
    in_flight: bool,
    /// Content generation the last probe was sent against: an unchanged
    /// screen has the same history, so an idle session never asks twice.
    generation: Option<u64>,
    sent_at: Option<Instant>,
}

impl HistoryExtentProbe {
    fn should_send(&self, generation: u64, now: Instant) -> bool {
        !self.in_flight
            && self.generation != Some(generation)
            && self
                .sent_at
                .is_none_or(|at| now.saturating_duration_since(at) >= HISTORY_EXTENT_PROBE_INTERVAL)
    }
}

impl ResidentTerminal {
    /// Delivers input the user aimed at the PTY: typing, line navigation and
    /// paste. A reading view hides the cursor and holds still under output, so
    /// the prompt comes back on screen before the bytes go out. The cursor is
    /// told too: it stays solid while the user acts, and only a move that
    /// follows their input may glide. Printable text reaches the element
    /// through its own input handler; everything encoded by the pane comes
    /// through here. Returns whether the pane owes a repaint, because the view
    /// moved or a dimmed cursor has to come back solid.
    fn send_user_input(&self, bytes: Vec<u8>) -> bool {
        if bytes.is_empty() {
            self.attachment.input(bytes);
            return false;
        }
        let returned = self.element.scroll_to_live(usize::from(self.last_size.1));
        let solid = self.element.note_user_input();
        self.attachment.input(bytes);
        returned || solid
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum SessionSource {
    FollowSelection,
    Fixed(SessionId),
}

/// Grid frames parked while a column change round-trips through the daemon,
/// so the re-wrap and the program's repaint reach the screen as one paint
/// rather than as a jump and a correction. See [`REFLOW_HOLD`].
struct ReflowHold {
    parked: Vec<GridUpdate>,
    /// The daemon's re-wrapped snapshot has landed, so the next frame after it
    /// is the program answering SIGWINCH and completes the pair.
    saw_snapshot: bool,
    /// The ceiling timer. Dropped with the hold, which cancels it.
    _release: Task<()>,
}

impl ReflowHold {
    /// Parks a frame, reporting whether the pair is now complete and the hold
    /// should be released.
    fn park(&mut self, update: GridUpdate) -> bool {
        let snapshot = update.is_full_snapshot;
        self.parked.push(update);
        if snapshot {
            // A later snapshot supersedes the first (a re-seed after
            // backpressure, or the daemon's own settle pass) rather than
            // standing in for the repaint we are waiting on.
            self.saw_snapshot = true;
            return false;
        }
        self.saw_snapshot
    }
}

/// Window-space allocation supplied by the workbench. Terminal input needs
/// the origin while PTY sizing needs the local width and height.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TerminalViewport {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

pub struct TerminalPane {
    #[cfg(test)]
    pub(crate) render_count: usize,
    qol: QolState,
    /// Overlay scroller and rubber band over the grid's scrollback.
    scroller: diri_ui::ScrollerState,
    reconnect: reconnect::ReconnectUi,
    runtime: Arc<StoreRuntime>,
    window_store: Option<crate::store::WindowStore>,
    _tokio_owner: Arc<tokio::runtime::Runtime>,
    tokio: Handle,
    residents: HashMap<SessionId, ResidentTerminal>,
    /// Last painted terminal of recently evicted sessions, most recent last.
    /// Selecting a session mounts that same element, so the row cache and
    /// element identity survive the attachment round-trip instead of flashing
    /// a freshly shaped screen. The attach snapshot then writes into the
    /// same buffer.
    parked_terminals: Vec<(SessionId, TerminalElement)>,
    /// PTY size a session was already using when its resident was dropped.
    /// `(0, 0)` on a new resident means "never sized", so this is what keeps
    /// a switch-back from looking like a first measure.
    known_pty_size: HashMap<SessionId, (u16, u16)>,
    pane_tx: PaneEventSender,
    /// Monotonic within this pane so replaced view residencies cannot receive
    /// stale UI/search completions from their predecessor.
    next_attachment_generation: AttachmentGeneration,
    focus: FocusHandle,
    glyphs: HashMap<SessionId, Entity<StatusGlyph>>,
    session_links: SessionLinks,
    /// The main window's viewport, for content that sizes to it while a
    /// panel paints it elsewhere.
    main_viewport: gpui::Size<gpui::Pixels>,
    /// Paced PTY resizes: window and sidebar drags relayout every frame, but
    /// sustained grid frames leave the daemon at up to 120 Hz, so intermediate
    /// sizes coalesce onto that cadence (see [`RESIZE_CADENCE`]).
    pending_resizes: HashMap<SessionId, ((u16, u16), u64)>,
    resize_flush: Option<Task<()>>,
    /// A cadence tick is already armed; further changes fold into it instead of
    /// rescheduling (which is what used to starve the flush during a drag).
    resize_flush_armed: bool,
    last_resize_sent: Option<Instant>,
    started_at: Instant,
    session_source: SessionSource,
    /// Last selection observed by the primary pane. Spawn responses select the
    /// daemon-created id asynchronously, so this transition is also the
    /// reliable point at which keyboard focus can leave the picker.
    observed_selected_id: Option<SessionId>,
    #[cfg(test)]
    input_observer: Option<InputObserver>,
    viewport: Option<TerminalViewport>,
    sidebar_visible: bool,
    inspector_open: bool,
    /// Space in the title bar reserved for workbench-owned controls painted
    /// above this pane, such as the auxiliary terminal's close button.
    header_trailing_inset: f32,
    /// The workbench hosts this pane's title-bar actions elsewhere (the
    /// horizontal tab strip), so the pane paints no title bar of its own and
    /// the grid takes the reclaimed height.
    header_hidden: bool,
    navigation: Option<Entity<NavigationOverlay>>,
    utility_surfaces: Option<Entity<UtilitySurfaces>>,
    local_clipboard_images: Vec<StagedClipboardImage>,
    /// Secure Keyboard Entry, held only while this pane is where a password
    /// is being typed. See [`Self::reconcile_secure_input`].
    secure_input: crate::secure_input::SecureInputLease,
    /// Whether the files being dragged are over this pane as its drop target.
    external_drag: haptics::Crossing,
    _secure_input_quit: gpui::Subscription,
    _secure_input_close: gpui::Subscription,
    _focus_owner: gpui::Subscription,
    _find_blur: gpui::Subscription,
    _find_focus_change: gpui::Subscription,
    _window_owner: gpui::Subscription,
    _pane_events: Task<()>,
    _store_changes: Task<()>,
}

impl EventEmitter<TerminalPaneEvent> for TerminalPane {}

impl TerminalPane {
    pub fn new(
        runtime: Arc<StoreRuntime>,
        tokio_owner: Arc<tokio::runtime::Runtime>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new_with_source(
            runtime,
            tokio_owner,
            SessionSource::FollowSelection,
            None,
            window,
            cx,
        )
    }

    pub(crate) fn new_for_window(
        runtime: Arc<StoreRuntime>,
        tokio_owner: Arc<tokio::runtime::Runtime>,
        store: crate::store::WindowStore,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new_with_source(
            runtime,
            tokio_owner,
            SessionSource::FollowSelection,
            Some(store),
            window,
            cx,
        )
    }

    pub fn new_fixed(
        runtime: Arc<StoreRuntime>,
        tokio_owner: Arc<tokio::runtime::Runtime>,
        session_id: SessionId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new_with_source(
            runtime,
            tokio_owner,
            SessionSource::Fixed(session_id),
            None,
            window,
            cx,
        )
    }

    /// Point this pane at another shell without discarding its parked grids.
    /// A freshly built pane has nothing to paint until a snapshot arrives.
    pub(crate) fn show_session(
        &mut self,
        id: SessionId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if matches!(&self.session_source, SessionSource::Fixed(current) if current == &id) {
            return;
        }
        if let SessionSource::Fixed(previous) = &self.session_source
            && let Some(resident) = self.residents.get(previous)
        {
            resident.attachment.release();
        }
        self.pending_resizes.clear();
        self.session_source = SessionSource::Fixed(id);
        self.reconcile_residency(cx);
        self.sync_status_glyphs(self.current_colors(), window, cx);
        cx.notify();
    }

    pub(crate) fn set_window_store(&mut self, store: crate::store::WindowStore) {
        self.window_store = Some(store);
    }

    fn new_with_source(
        runtime: Arc<StoreRuntime>,
        tokio_owner: Arc<tokio::runtime::Runtime>,
        session_source: SessionSource,
        window_store: Option<crate::store::WindowStore>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus = cx.focus_handle();
        if matches!(session_source, SessionSource::FollowSelection) {
            window.focus(&focus, cx);
        }
        let focus_owner = cx.on_focus(&focus, window, |this, window, cx| {
            if window.is_window_active() {
                this.claim_selected_control();
            }
            this.reconcile_secure_input(window);
            cx.notify();
        });
        let find_blur = cx.on_blur(&focus, window, |this, window, cx| {
            this.cancel_find_composition(window, cx);
            // A pane that lost focus may never render again, so the release
            // cannot wait for one.
            if this.reconcile_secure_input(window) {
                cx.notify();
            }
        });
        let find_focus_change = cx.observe_pending_input(window, |this, window, cx| {
            if !this.focus.is_focused(window)
                && this
                    .residents
                    .values()
                    .any(|resident| resident.find_composition.is_composing())
            {
                this.cancel_find_composition(window, cx);
            }
        });
        let window_owner = cx.observe_window_activation(window, |this, window, cx| {
            if this.reconcile_secure_input(window) {
                cx.notify();
            }
            if window.is_window_active() && this.focus.is_focused(window) {
                this.claim_selected_control();
                cx.notify();
            }
        });
        // Neither quitting nor closing a window reliably drops this entity, so
        // the lease's own drop is a backstop and these are the release.
        let secure_input_quit = cx.on_app_quit(|this, _| {
            this.secure_input.set(false);
            async {}
        });
        let secure_input_close = {
            let pane = cx.weak_entity();
            let window_id = window.window_handle().window_id();
            cx.on_window_closed(move |cx, closed| {
                if closed == window_id {
                    let _ = pane.update(cx, |this, _| this.secure_input.set(false));
                }
            })
        };
        let (pane_tx, mut pane_rx) = pane_event_channel();
        let pane_events = cx.spawn_in(window, async move |this, cx| {
            let mut batch = Vec::new();
            while pane_rx.recv_batch(&mut batch).await {
                if crate::floating::update_in_owner(&this, cx, |this, window, cx| {
                    for event in batch.drain(..) {
                        this.handle_pane_event(event, window, cx);
                    }
                })
                .is_none()
                {
                    return;
                }
            }
        });

        let mut changes = runtime.changes();
        let store_changes = cx.spawn_in(window, async move |this, cx| {
            loop {
                match changes.recv().await {
                    Ok(()) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        if crate::floating::update_in_owner(&this, cx, |this, window, cx| {
                            this.reconcile_store_change(window, cx);
                        })
                        .is_none()
                        {
                            return;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                }
            }
        });

        let tokio = tokio_owner.handle().clone();
        let observed_selected_id = matches!(session_source, SessionSource::FollowSelection)
            .then(|| {
                window_store.as_ref().map_or_else(
                    || {
                        runtime
                            .store
                            .read()
                            .expect("store")
                            .selected_session_id()
                            .cloned()
                    },
                    |store| store.read().expect("store").selected_session_id().cloned(),
                )
            })
            .flatten();
        let mut pane = Self {
            #[cfg(test)]
            render_count: 0,
            window_store,
            runtime,
            _tokio_owner: tokio_owner,
            tokio,
            residents: HashMap::new(),
            parked_terminals: Vec::new(),
            known_pty_size: HashMap::new(),
            pane_tx,
            next_attachment_generation: 1,
            focus,
            glyphs: HashMap::new(),
            session_links: SessionLinks::new(cx),
            main_viewport: gpui::Size::default(),
            qol: QolState::default(),
            scroller: diri_ui::ScrollerState::new(),
            reconnect: Default::default(),
            pending_resizes: HashMap::new(),
            resize_flush: None,
            resize_flush_armed: false,
            last_resize_sent: None,
            started_at: Instant::now(),
            session_source,
            observed_selected_id,
            #[cfg(test)]
            input_observer: None,
            viewport: None,
            sidebar_visible: true,
            inspector_open: false,
            header_trailing_inset: 0.0,
            header_hidden: false,
            navigation: None,
            utility_surfaces: None,
            local_clipboard_images: Vec::new(),
            secure_input: crate::secure_input::SecureInputLease::system(),
            external_drag: haptics::Crossing::default(),
            _secure_input_quit: secure_input_quit,
            _secure_input_close: secure_input_close,
            _focus_owner: focus_owner,
            _find_blur: find_blur,
            _find_focus_change: find_focus_change,
            _window_owner: window_owner,
            _pane_events: pane_events,
            _store_changes: store_changes,
        };
        pane.reconcile_residency(cx);
        pane.sync_status_glyphs(pane.current_colors(), window, cx);
        pane
    }

    fn reconcile_residency(&mut self, cx: &mut Context<Self>) {
        let window_selected = self
            .window_store
            .as_ref()
            .map(|window| window.read().expect("store").selected_session_id().cloned());
        let store = self
            .runtime
            .store
            .read()
            .expect("session store lock poisoned");
        let resident_ids: HashSet<_> = match &self.session_source {
            SessionSource::FollowSelection => {
                if let Some(selected) = window_selected {
                    selected
                        .filter(|id| {
                            store
                                .sessions()
                                .get(id)
                                .is_some_and(|session| !session.is_archived())
                        })
                        .into_iter()
                        .collect()
                } else {
                    store.terminal_residency().resident().cloned().collect()
                }
            }
            SessionSource::Fixed(id) if store.sessions().contains_key(id) => {
                HashSet::from([id.clone()])
            }
            SessionSource::Fixed(_) => HashSet::new(),
        };
        // A parked terminal for a session the store no longer lists is dead
        // weight; one for a session that just became resident is superseded
        // below by promotion.
        self.parked_terminals
            .retain(|(id, _)| store.sessions().contains_key(id));
        drop(store);
        // Park the painted terminal of every session about to be evicted, so
        // re-selecting it paints the same element instead of flashing a new
        // one while the fresh attachment round-trips.
        for (id, resident) in &self.residents {
            if resident_ids.contains(id) {
                continue;
            }
            self.parked_terminals.retain(|(parked, _)| parked != id);
            self.parked_terminals
                .push((id.clone(), resident.element.clone()));
            if resident.last_size != (0, 0) {
                self.known_pty_size.insert(id.clone(), resident.last_size);
            }
        }
        if self.parked_terminals.len() > PARKED_GRID_CAP {
            let excess = self.parked_terminals.len() - PARKED_GRID_CAP;
            self.parked_terminals.drain(..excess);
        }
        self.residents.retain(|id, _| resident_ids.contains(id));
        let socket = self.runtime.client().socket_path().to_path_buf();
        for id in resident_ids {
            if self.residents.contains_key(&id) {
                continue;
            }
            let mono = crate::fonts::terminal_font();
            let generation = self.next_attachment_generation;
            self.next_attachment_generation = self.next_attachment_generation.wrapping_add(1);
            let parked = self
                .parked_terminals
                .iter()
                .position(|(parked, _)| parked == &id)
                .map(|index| self.parked_terminals.remove(index).1);
            let parked_buffer = parked.as_ref().map(TerminalElement::buffer);
            let last_size = self
                .known_pty_size
                .get(&id)
                .copied()
                .unwrap_or_else(|| parked_grid_size(parked_buffer.as_ref()).unwrap_or((0, 0)));
            let (controller, attachment, buffer) = ControllerLease::mount(
                socket.clone(),
                id.clone(),
                &self.tokio,
                self.pane_tx.clone(),
                generation,
                parked_buffer,
                cx,
            );
            #[cfg(test)]
            let attachment = {
                let mut attachment = attachment;
                attachment.input_observer = self.input_observer.clone().map(|tx| (id.clone(), tx));
                attachment
            };
            let ime_attachment = attachment.clone();
            let reuse_parked = parked
                .as_ref()
                .is_some_and(|element| Arc::ptr_eq(&element.buffer(), &buffer));
            let element = if reuse_parked {
                parked.unwrap()
            } else {
                TerminalElement::new(buffer)
            };
            let element = element
                .font(mono)
                .focus_handle(self.focus.clone())
                .on_text_input(move |text| ime_attachment.input(text.as_bytes().to_vec()));
            controller.observe(&element);
            self.residents.insert(
                id,
                ResidentTerminal {
                    controller,
                    keyboard: None,
                    element,
                    attachment,
                    attachment_generation: generation,
                    attachment_state: AttachmentState::Attaching,
                    bracketed_paste: false,
                    secret_input: false,
                    find: None,
                    find_scheduler: FindSearchScheduler::default(),
                    find_query: QueryEditor::default(),
                    find_composition: find_input::Composition::default(),
                    last_size,
                    pointer_owner: None,
                    mouse_motion: MouseMotionLimiter::default(),
                    extent_probe: HistoryExtentProbe::default(),
                },
            );
        }
    }

    fn reconcile_store_change(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let selected_id = matches!(self.session_source, SessionSource::FollowSelection)
            .then(|| self.selected_id())
            .flatten();
        let selection_changed = selected_id != self.observed_selected_id;
        if selection_changed {
            self.cancel_find_composition(window, cx);
            if let Some(previous) = &self.observed_selected_id
                && let Some(resident) = self.residents.get(previous)
            {
                resident.attachment.release();
            }
            self.pending_resizes.clear();
        }
        self.observed_selected_id = selected_id.clone();

        self.reconcile_residency(cx);
        if selection_changed {
            self.qol.clear_feedback();
            self.session_links.close();
            for resident in self.residents.values_mut() {
                resident.pointer_owner = None;
                resident.mouse_motion.reset();
            }
        }
        self.sync_status_glyphs(self.current_colors(), window, cx);

        // Explicit sidebar clicks already focus through SessionActivated, but
        // successful spawns select their daemon-assigned id on the async store
        // path. Following the selection here covers both RPC/event orderings
        // and avoids trying to focus a terminal before its id exists.
        if selection_changed && selected_id.is_some() {
            self.focus(window, cx);
        }
        self.reconcile_secure_input(window);
        cx.notify();
    }

    /// Holds macOS Secure Keyboard Entry exactly while keystrokes typed here
    /// are a secret on their way to the child: this pane has keyboard focus
    /// in the active window, and its selected, live session reports a line
    /// prompt with echo off.
    ///
    /// Every input to that condition calls this when it changes (modes,
    /// attachment state, focus, blur, window activation, selection and
    /// residency), and so does every render, so a missed edge heals on the
    /// next frame. The lease is idempotent, which is what makes calling this
    /// freely safe: however often it runs, one enable is outstanding at most
    /// and each is released once. Returns whether the lease changed hands,
    /// which the lock badge's wording follows.
    fn reconcile_secure_input(&mut self, window: &Window) -> bool {
        let secret_input = self.selected_id().is_some_and(|id| {
            self.residents.get(&id).is_some_and(|resident| {
                resident.secret_input && resident.attachment_state == AttachmentState::Live
            })
        });
        let was_held = self.secure_input.is_held();
        self.secure_input.set(secure_input_wanted(
            secret_input,
            self.focus.is_focused(window),
            window.is_window_active(),
        ));
        was_held != self.secure_input.is_held()
    }

    /// Paint-only previews must not reconcile residency or acquire a controller.
    pub fn resident_preview_buffers(&self) -> HashMap<SessionId, SharedGridBuffer> {
        self.residents
            .iter()
            .map(|(id, resident)| (id.clone(), resident.element.buffer()))
            .collect()
    }

    pub fn resident_buffers(
        &mut self,
        cx: &mut Context<Self>,
    ) -> HashMap<SessionId, SharedGridBuffer> {
        self.reconcile_residency(cx);
        self.residents
            .iter()
            .map(|(id, resident)| (id.clone(), resident.element.buffer()))
            .collect()
    }

    pub fn set_shell_entities(
        &mut self,
        navigation: Entity<NavigationOverlay>,
        utility_surfaces: Entity<UtilitySurfaces>,
    ) {
        self.navigation = Some(navigation);
        self.utility_surfaces = Some(utility_surfaces);
    }

    pub fn focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.session_source, SessionSource::FollowSelection)
            && self.selected_id() != self.observed_selected_id
        {
            self.reconcile_store_change(window, cx);
            return;
        }
        if window.is_window_active() {
            self.claim_selected_control();
        }
        window.focus(&self.focus, cx);
        cx.notify();
    }

    /// A split workbench explicitly assigns one visible owner per SessionId.
    /// Unfocused sessions still need geometry; duplicate passive views do not.
    pub(crate) fn claim_layout_control(&self, window: &Window) {
        if window.is_window_active() {
            self.claim_selected_control();
        }
    }

    pub(crate) fn release_layout_control(&self) {
        if let Some(id) = self.selected_id()
            && let Some(resident) = self.residents.get(&id)
        {
            resident.attachment.release();
        }
    }

    #[cfg(all(test, target_os = "macos"))]
    pub(crate) fn send_owned_fixture_input(&self) -> Option<(SessionId, u16, u16)> {
        let id = self.selected_id()?;
        let resident = self.residents.get(&id)?;
        if !resident.attachment.is_controller() || resident.last_size == (0, 0) {
            return None;
        }
        resident.attachment.input(b"show\n".to_vec());
        Some((id, resident.last_size.0, resident.last_size.1))
    }
    #[cfg(test)]
    pub(crate) fn layout_owner_for_test(&self) -> bool {
        self.selected_id()
            .and_then(|id| self.residents.get(&id))
            .is_some_and(|resident| resident.attachment.is_controller())
    }

    fn claim_selected_control(&self) {
        if let Some(id) = self.selected_id()
            && let Some(resident) = self.residents.get(&id)
        {
            resident.attachment.claim();
        }
    }

    pub fn set_viewport(&mut self, viewport: TerminalViewport, cx: &mut Context<Self>) {
        if self.viewport == Some(viewport) {
            return;
        }
        self.viewport = Some(viewport);
        cx.notify();
    }

    #[cfg(all(test, target_os = "macos"))]
    pub(crate) fn seed_preview_grid_for_test(&mut self, grid: GridBuffer, cx: &mut Context<Self>) {
        self.reconcile_residency(cx);
        if let Some(id) = self.selected_id()
            && let Some(resident) = self.residents.get_mut(&id)
        {
            *resident.element.buffer().write().unwrap() = grid;
            resident.attachment_state = AttachmentState::Live;
            resident.controller.seed_live_for_test();
            self.seed_reconnect_fixture(&id);
        }
    }

    #[cfg(test)]
    pub(crate) fn geometry_for_test(&self) -> (Option<TerminalViewport>, Option<(u16, u16)>) {
        let grid = self.selected_session().and_then(|session| {
            self.residents
                .get(&session.id)
                .map(|resident| resident.last_size)
        });
        (self.viewport, grid)
    }

    pub fn set_shell_chrome(
        &mut self,
        sidebar_visible: bool,
        inspector_open: bool,
        cx: &mut Context<Self>,
    ) {
        if self.sidebar_visible == sidebar_visible && self.inspector_open == inspector_open {
            return;
        }
        self.sidebar_visible = sidebar_visible;
        self.inspector_open = inspector_open;
        cx.notify();
    }

    pub fn set_header_trailing_inset(&mut self, inset: f32, cx: &mut Context<Self>) {
        if (self.header_trailing_inset - inset).abs() < f32::EPSILON {
            return;
        }
        self.header_trailing_inset = inset.max(0.0);
        cx.notify();
    }

    /// Drop the pane's own title bar because the workbench hosts its actions
    /// in the horizontal tab strip. Grid geometry follows on the next layout.
    pub fn set_header_hidden(&mut self, hidden: bool, cx: &mut Context<Self>) {
        if self.header_hidden == hidden {
            return;
        }
        self.header_hidden = hidden;
        cx.notify();
    }

    pub fn header_hidden(&self) -> bool {
        self.header_hidden
    }

    /// Height of the chrome painted above the terminal surface, which every
    /// grid-space calculation must subtract from the viewport.
    pub(crate) fn header_height(&self) -> f32 {
        if self.header_hidden {
            0.0
        } else {
            Metrics::TITLE_BAR
        }
    }

    pub fn is_focused(&self, window: &Window) -> bool {
        self.focus.is_focused(window)
    }

    #[cfg(all(test, target_os = "macos"))]
    pub(crate) fn capture_input_for_test(
        &mut self,
    ) -> mpsc::UnboundedReceiver<(SessionId, Vec<u8>)> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.input_observer = Some(tx.clone());
        for (id, resident) in &mut self.residents {
            resident.attachment.input_observer = Some((id.clone(), tx.clone()));
            let attachment = resident.attachment.clone();
            resident.element = resident.element.clone().on_text_input(move |text| {
                attachment.input(text.as_bytes().to_vec());
            });
        }
        rx
    }

    #[must_use]
    pub fn quote_focus_handle(&self) -> FocusHandle {
        self.focus.clone()
    }

    fn sync_status_glyphs(
        &mut self,
        colors: SemanticColors,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let fixed_id = match &self.session_source {
            SessionSource::Fixed(id) => Some(id),
            SessionSource::FollowSelection => None,
        };
        let snapshots: Vec<_> = {
            let store = self
                .runtime
                .store
                .read()
                .expect("session store lock poisoned");
            store
                .sessions()
                .iter()
                .filter(|(id, _)| fixed_id.is_none_or(|fixed| fixed == *id))
                .map(|(id, session)| {
                    (
                        id.clone(),
                        ui_agent_kind(session.effective_kind()),
                        status_state(session),
                    )
                })
                .collect()
        };
        self.glyphs
            .retain(|id, _| snapshots.iter().any(|(live, _, _)| live == id));
        for (id, kind, state) in snapshots {
            if let Some(glyph) = self.glyphs.get(&id) {
                glyph.update(cx, |glyph, cx| {
                    glyph.set_kind(kind, cx);
                    glyph.set_state(state, window, cx);
                    glyph.set_colors(colors, cx);
                });
            } else {
                let glyph = StatusGlyph::entity(kind, state, 16.0, colors, cx);
                self.glyphs.insert(id, glyph);
            }
        }
    }

    fn current_colors(&self) -> SemanticColors {
        let store = self
            .runtime
            .store
            .read()
            .expect("session store lock poisoned");
        crate::app_theme::colors_in(&store)
    }

    fn handle_pane_event(&mut self, event: PaneEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event {
            PaneEvent::ControllerDamage(id, generation, changed) => {
                if !self.attachment_is_current(&id, generation) {
                    return;
                }
                let now = self.started_at.elapsed();
                let schedule = self.residents.get_mut(&id).is_some_and(|resident| {
                    let Some(find) = resident.find.as_mut() else {
                        return false;
                    };
                    let scheduled = find.on_output(now);
                    resident.element.sync_find_highlights(find);
                    scheduled
                });
                if schedule {
                    self.schedule_find(id.clone(), self.find_rescan_delay(&id), window, cx);
                }
                if terminal_damage_should_repaint(self.selected_id().as_ref(), &id, changed) {
                    self.request_terminal_repaint(window, cx);
                }
            }
            PaneEvent::InputFeedback(id, message) => {
                if self.selected_id().as_ref() == Some(&id) {
                    self.show_terminal_feedback(message, window, cx);
                }
            }
            PaneEvent::AttachmentState(id, generation, state) => {
                if !self.attachment_is_current(&id, generation) {
                    return;
                }
                if let Some(resident) = self.residents.get_mut(&id) {
                    resident.bracketed_paste =
                        bracketed_paste_after_attachment_state(resident.bracketed_paste, state);
                    resident.secret_input = resident.secret_input && state == AttachmentState::Live;
                    if resident.attachment_state != state {
                        resident.pointer_owner = None;
                        resident.mouse_motion.reset();
                    }
                    if state != AttachmentState::Live {
                        resident.keyboard = None;
                    }
                    resident.attachment_state = state;
                }
                self.reconcile_secure_input(window);
                if self.selected_id().as_ref() == Some(&id) {
                    cx.notify();
                }
            }
            PaneEvent::Chunk(id, generation, TerminalChunk::Grid(update)) => {
                if !self.attachment_is_current(&id, generation) {
                    return;
                }
                self.apply_grid_updates(id, [update], window, cx);
            }
            PaneEvent::GridBatch(id, generation, updates) => {
                if !self.attachment_is_current(&id, generation) {
                    return;
                }
                self.apply_grid_updates(id, updates, window, cx);
            }
            PaneEvent::Chunk(
                id,
                generation,
                TerminalChunk::Modes {
                    keyboard,
                    alt_screen,
                    bracketed_paste,
                    mouse,
                    secret_input,
                },
            ) => {
                if !self.attachment_is_current(&id, generation) {
                    return;
                }
                if let Some(resident) = self.residents.get_mut(&id) {
                    if resident.element.mouse_modes() != mouse {
                        resident.pointer_owner = None;
                        resident.mouse_motion.reset();
                    }
                    resident.keyboard = keyboard;
                    resident.bracketed_paste = bracketed_paste;
                    resident.secret_input = secret_input;
                    resident.element.set_modes(alt_screen, mouse);
                }
                self.reconcile_secure_input(window);
                if self.selected_id().as_ref() == Some(&id) {
                    cx.notify();
                }
            }
            PaneEvent::Chunk(_, _, TerminalChunk::Pong) => {}
            PaneEvent::FindSnapshot(id, generation, request, snapshot) => {
                if !self.attachment_is_current(&id, generation) {
                    return;
                }
                let read_completion = self
                    .residents
                    .get_mut(&id)
                    .map(|resident| {
                        resident
                            .find_scheduler
                            .finish_read(&request, snapshot.is_some())
                    })
                    .unwrap_or(ReadCompletion::Ignore);
                match read_completion {
                    ReadCompletion::Ignore | ReadCompletion::Idle => {}
                    ReadCompletion::Read(next) => {
                        self.launch_find_read(id, generation, next);
                    }
                    ReadCompletion::Scan => {
                        let job = snapshot.and_then(|snapshot| {
                            self.residents.get(&id).and_then(|resident| {
                                resident.find.as_ref().and_then(|find| {
                                    resident
                                        .element
                                        .prepare_find_search(find, &request, snapshot)
                                })
                            })
                        });
                        if let Some(job) = job {
                            let pane_tx = self.pane_tx.clone();
                            self.tokio.spawn_blocking(move || {
                                let result = job.run();
                                let _ = pane_tx
                                    .send(PaneEvent::FindResult(id, generation, request, result));
                            });
                        } else {
                            let next = self
                                .residents
                                .get_mut(&id)
                                .and_then(|resident| resident.find_scheduler.finish_scan(&request))
                                .and_then(|completion| completion.into_next_request());
                            if let Some(next) = next {
                                self.launch_find_read(id, generation, next);
                            }
                        }
                    }
                }
            }
            PaneEvent::FindResult(id, generation, request, result) => {
                if !self.attachment_is_current(&id, generation) {
                    return;
                }
                let visible = self.selected_id().as_ref() == Some(&id);
                let mut next = None;
                if let Some(resident) = self.residents.get_mut(&id)
                    && let Some(completion) = resident.find_scheduler.finish_scan(&request)
                {
                    if completion.should_apply_result()
                        && let Some(find) = resident.find.as_mut()
                        && resident.element.apply_find_result(find, result)
                    {
                        resident.element.sync_find_highlights(find);
                        if visible {
                            cx.notify();
                        }
                    }
                    next = completion.into_next_request();
                }
                if let Some(next) = next {
                    self.launch_find_read(id, generation, next);
                }
            }
            PaneEvent::ScrollbackCells(id, generation, result, visible_rows) => {
                if !self.attachment_is_current(&id, generation) {
                    return;
                }
                if let Some(resident) = self.residents.get_mut(&id) {
                    let _ = resident
                        .element
                        .complete_scrollback_fetch(result, visible_rows);
                }
                self.pump_scrollback_fetch(&id, visible_rows);
                if self.selected_id().as_ref() == Some(&id) {
                    cx.notify();
                }
            }
            PaneEvent::ScrollbackPump(id, visible_rows) => {
                self.pump_scrollback_fetch(&id, visible_rows);
                cx.notify();
            }
            PaneEvent::ScrollbackFailed(id, generation) => {
                if !self.attachment_is_current(&id, generation) {
                    return;
                }
                if let Some(resident) = self.residents.get_mut(&id) {
                    resident.element.fail_scrollback_fetch();
                }
                if self.selected_id().as_ref() == Some(&id) {
                    cx.notify();
                }
            }
            PaneEvent::HistoryExtent(id, generation, live_start_row) => {
                if !self.attachment_is_current(&id, generation) {
                    return;
                }
                if let Some(resident) = self.residents.get_mut(&id) {
                    resident.extent_probe.in_flight = false;
                    match live_start_row {
                        Some(row) => resident.element.note_history_rows(row),
                        // Let the next frame that shows the knob ask again.
                        None => resident.extent_probe.generation = None,
                    }
                }
                if self.selected_id().as_ref() == Some(&id) {
                    cx.notify();
                }
            }
            PaneEvent::ClipboardUploadFinished(id, result) => match result {
                Ok(remote_path) => {
                    if let Some(resident) = self.residents.get(&id)
                        && resident
                            .send_user_input(terminal_paste(&remote_path, resident.bracketed_paste))
                    {
                        cx.notify();
                    }
                }
                Err(error) => {
                    // scp's stderr can name hosts, users and key paths; it
                    // stays in the developer log and out of the app.
                    eprintln!("diri: clipboard image upload failed: {error}");
                    self.show_terminal_feedback(
                        "Couldn't copy the clipboard image to the session's host",
                        window,
                        cx,
                    );
                }
            },
            PaneEvent::DroppedFilesUploaded(id, result) => match result {
                Ok(remote_paths) => {
                    let text = terminal_drop_text(remote_paths.iter().map(String::as_str));
                    if self.paste_into_session(&id, &text) {
                        cx.notify();
                    }
                }
                Err(error) => {
                    eprintln!("diri: dropped file upload failed: {error}");
                    cx.emit(TerminalPaneEvent::ExternalDropFeedback {
                        message: format!(
                            "Couldn't copy the dropped files to the session's host: {error}"
                        ),
                    });
                }
            },
        }
    }

    /// Writes dropped file paths to the target session's composer. Returns
    /// whether that brought a reading view back to live.
    fn paste_into_session(&self, id: &SessionId, text: &str) -> bool {
        let Some(resident) = self.residents.get(id) else {
            return false;
        };
        let store = self
            .runtime
            .store
            .read()
            .expect("session store lock poisoned");
        // Use the declared direct-launch kind, not foreground detection:
        // a detected Claude inside a shell can return to that shell.
        let kind = store.sessions().get(id).map(|session| &session.kind);
        resident.send_user_input(terminal_file_paste(text, resident.bracketed_paste, kind))
    }

    /// Finder released files over the grid. Behaves like a desktop terminal:
    /// the paths are pasted into the foreground program, which is how Claude
    /// Code, Codex and Cursor attach dropped images. Sessions on another host
    /// get the files copied over first so the pasted path exists there.
    fn external_drop(
        &mut self,
        paths: &ExternalPaths,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(id) = self.selected_id() else {
            return;
        };
        if !self.residents.contains_key(&id) {
            return;
        }
        let ssh = self.drop_destination(&id);

        let plan = plan_terminal_drop(paths.paths(), ssh.is_some());
        if let Some(message) = plan.feedback() {
            cx.emit(TerminalPaneEvent::ExternalDropFeedback { message });
        }
        self.external_drag.reset();
        if plan.action.is_some() {
            // The release is the user's moment, for remote sessions too: the
            // upload finishing later is the app's doing and stays silent. A
            // refused drop is answered by the toast alone.
            haptics::perform(Haptic::Accepted, haptics::key("terminal-drop", &id));
            // A Finder drop is an explicit interaction even when macOS has
            // not activated this window. Claim synchronously: focus callbacks
            // run after this handler, too late to admit the dropped paths.
            window.activate_window();
            window.focus(&self.focus, cx);
            self.claim_selected_control();
        }
        match plan.action {
            None => {}
            Some(TerminalDropAction::Paste(text)) => {
                self.paste_into_session(&id, &text);
            }
            Some(TerminalDropAction::Upload(files)) => {
                let Some(ssh) = ssh else {
                    return;
                };
                let pane_tx = self.pane_tx.clone();
                let upload_id = id.clone();
                self.tokio.spawn(async move {
                    let result = tokio::task::spawn_blocking(move || {
                        files
                            .iter()
                            .map(|file| upload_dropped_file(file, &ssh))
                            .collect::<Result<Vec<_>, _>>()
                    })
                    .await
                    .unwrap_or_else(|error| Err(format!("upload task failed: {error}")));
                    let _ = pane_tx.send(PaneEvent::DroppedFilesUploaded(upload_id, result));
                });
            }
        }
        cx.notify();
    }

    /// Where a drop on `id` has to be copied to first, for a session on
    /// another host.
    fn drop_destination(&self, id: &SessionId) -> Option<String> {
        let store = self
            .runtime
            .store
            .read()
            .expect("session store lock poisoned");
        store
            .sessions()
            .get(id)
            .and_then(|session| session.host.as_deref())
            .and_then(|host_id| store.host(host_id))
            .map(|host| host.ssh.clone())
    }

    /// One tick as dragged files arrive over this pane, if releasing them
    /// here would do something: the moment the pane lights as the target.
    /// Files nothing here can take light the same border but stay silent.
    fn track_external_drag(
        &mut self,
        paths: &ExternalPaths,
        over_pane: bool,
        pointer: gpui::Point<gpui::Pixels>,
    ) {
        let target = self
            .selected_id()
            .filter(|id| over_pane && self.residents.contains_key(id))
            .filter(|id| {
                // Staging stats every path, so decide once, on the way in.
                self.external_drag.is_over_target()
                    || plan_terminal_drop(paths.paths(), self.drop_destination(id).is_some())
                        .action
                        .is_some()
            })
            .map(|id| haptics::key("terminal-drop", &id));
        if let Some(target) = self.external_drag.moved_to(target, pointer) {
            haptics::perform(Haptic::Snap, target);
        }
    }

    /// Present only while a drag is in flight, so it never sits between the
    /// pointer and the grid during normal use. Styled only when what is being
    /// dragged is a set of desktop files: other in-app drags pass through it.
    fn external_drop_overlay(&self, cx: &mut Context<Self>) -> AnyElement {
        div()
            .id("diri-terminal-external-drop")
            .absolute()
            .inset_0()
            .rounded(px(Radius::PANEL))
            .drag_over::<ExternalPaths>(|overlay, _, _, _| {
                overlay
                    .bg(Ink::FRESH.alpha(0.07))
                    .border_2()
                    .border_color(Ink::FRESH.alpha(0.5))
            })
            .on_drag_move(
                cx.listener(|this, event: &gpui::DragMoveEvent<ExternalPaths>, _, cx| {
                    let pointer = event.event.position;
                    let paths = event.drag(cx).clone();
                    this.track_external_drag(&paths, event.bounds.contains(&pointer), pointer);
                }),
            )
            .on_drop(cx.listener(|this, paths: &ExternalPaths, window, cx| {
                cx.stop_propagation();
                this.external_drop(paths, window, cx);
            }))
            .into_any_element()
    }

    fn attachment_is_current(&self, id: &SessionId, generation: AttachmentGeneration) -> bool {
        self.residents
            .get(id)
            .is_some_and(|resident| resident.attachment_generation == generation)
    }

    /// Applies grid frames to a resident and repaints if what landed is worth a
    /// frame. Takes a batch because a held reflow releases its parked frames
    /// together: applying them one by one would paint each intermediate.
    fn apply_grid_updates(
        &mut self,
        id: SessionId,
        updates: impl IntoIterator<Item = GridUpdate>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let now = self.started_at.elapsed();
        let selected = self.selected_id();
        let mut schedule_find = false;
        let mut changed = false;
        let mut applied = false;
        if let Some(resident) = self.residents.get_mut(&id) {
            let mut updates = updates.into_iter();
            if let Some(mut update) = updates.next() {
                applied = true;
                for newer in updates {
                    update.coalesce(newer);
                }
                changed = resident.element.apply_damage(update).changed;
            }
            if applied && let Some(find) = resident.find.as_mut() {
                schedule_find = find.on_output(now);
                resident.element.sync_find_highlights(find);
            }
        }
        if !applied {
            return;
        }
        // Visibility/occlusion is GPUI's job (display-link stops when the
        // window is truly hidden). `is_window_active` is only OS focus, so
        // gating on it freezes a still-visible window on another monitor.
        let repaint = terminal_damage_should_repaint(selected.as_ref(), &id, changed);
        if schedule_find {
            let delay = self.find_rescan_delay(&id);
            self.schedule_find(id, delay, window, cx);
        }
        if repaint {
            self.request_terminal_repaint(window, cx);
        }
    }

    /// Holds a session's grid still until its column change has fully
    /// round-tripped. A hold already in flight is extended rather than
    /// released, so a second change landing mid-hold covers its own reflow too;
    /// its frames carry over, because a daemon that never answers the second
    /// resize (a hibernated tree, a session the phone owns) would otherwise
    /// leave the pane painting whatever was on screen before the first one.
    fn hold_reflow(&mut self, id: SessionId, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(resident) = self.residents.get(&id) {
            resident.controller.hold_reflow(cx);
        }
    }

    fn request_terminal_repaint(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        // GPUI coalesces dirty entities and presents them from the platform's
        // CVDisplayLink. A second fixed-rate timer here can only miss the next
        // display deadline (and capped ProMotion at 60 fps), so terminal
        // damage has exactly one pacing authority: the display itself.
        cx.notify();
    }

    /// Schedules the search a query change just armed, after the delay the
    /// model chose: none when it can scan the capture it already holds, a
    /// short pause when the terminal has to be captured again.
    fn schedule_query_search(&self, id: SessionId, window: &mut Window, cx: &mut Context<Self>) {
        let delay = self
            .residents
            .get(&id)
            .and_then(|resident| resident.find.as_ref())
            .map_or(Duration::ZERO, |find| {
                find.search_delay(self.started_at.elapsed())
            });
        self.schedule_find(id, delay, window, cx);
    }

    fn schedule_find(
        &self,
        id: SessionId,
        delay: Duration,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(delay).await;
            let _ = crate::floating::update_in_owner(&this, cx, |this, _window, _cx| {
                this.start_due_find(&id)
            });
        })
        .detach();
    }

    fn start_due_find(&mut self, id: &SessionId) {
        let now = self.started_at.elapsed();
        let Some((generation, request)) = self.residents.get_mut(id).and_then(|resident| {
            let request = resident.find.as_mut()?.take_due_search(now)?;
            let request = resident.find_scheduler.schedule(request)?;
            Some((resident.attachment_generation, request))
        }) else {
            return;
        };
        self.launch_find_read(id.clone(), generation, request);
    }

    fn launch_find_read(
        &mut self,
        id: SessionId,
        generation: AttachmentGeneration,
        request: SearchRequest,
    ) {
        let capture = self
            .residents
            .get_mut(&id)
            .and_then(|resident| resident.find.as_mut())
            .map(|find| {
                (
                    find.uses_retained_capture(),
                    find.reusable_source(),
                    if find.uses_retained_capture() {
                        find.reservation()
                    } else {
                        None
                    },
                )
            });
        let remote = self
            .runtime
            .store
            .read()
            .expect("session store lock poisoned")
            .sessions()
            .get(&id)
            .is_some_and(|session| session.host.is_some());
        let client = Arc::clone(self.runtime.client());
        let pane_tx = self.pane_tx.clone();
        self.tokio.spawn(async move {
            let snapshot = match capture {
                Some((true, Some(source), _)) => Some(FindSnapshot::from(source)),
                Some((true, None, Some(reservation))) => {
                    // Only active searches wait here. A single admission gate
                    // bounds transient RPC/decode allocations across windows.
                    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
                    let permit = loop {
                        if let Some(permit) = diri_term::find::FindCapturePermit::acquire() {
                            break Some(permit);
                        }
                        if tokio::time::Instant::now() >= deadline {
                            break None;
                        }
                        tokio::time::sleep(Duration::from_millis(25)).await;
                    };
                    if let Some(permit) = permit {
                        match client.capture_find(&id).await {
                            Ok(result) if result.session_id == id => Some(
                                tokio::task::spawn_blocking(move || {
                                    let _permit = permit;
                                    match diri_term::find::RetainedFindSnapshot::decode(
                                        result,
                                        reservation,
                                    ) {
                                        Ok(source) => FindSnapshot::from(source),
                                        Err(error) => FindSnapshot::failure(error),
                                    }
                                })
                                .await
                                .unwrap_or_else(|_| FindSnapshot::failure("Search interrupted")),
                            ),
                            Ok(_) => Some(FindSnapshot::failure("Search session changed")),
                            // A remote Helper that cannot serve its history
                            // still has a screen to search; see `apply_result`.
                            Err(_) if remote => {
                                client.read_scrollback(&id).await.ok().map(Into::into)
                            }
                            Err(_) => Some(FindSnapshot::failure(
                                "Search unavailable. Refresh results to retry",
                            )),
                        }
                    } else {
                        Some(FindSnapshot::failure(
                            "Search busy. Refresh results to retry",
                        ))
                    }
                }
                Some((true, None, None)) => Some(FindSnapshot::failure(
                    "Close another Find view to search here",
                )),
                Some((false, _, _)) => client.read_scrollback(&id).await.ok().map(Into::into),
                None => None,
            };
            let _ = pane_tx.send(PaneEvent::FindSnapshot(id, generation, request, snapshot));
        });
    }

    fn find_rescan_delay(&self, id: &SessionId) -> Duration {
        self.residents
            .get(id)
            .and_then(|resident| resident.find.as_ref())
            .map_or(
                Duration::from_millis(100),
                TerminalFindModel::output_rescan_delay,
            )
    }

    #[cfg(all(test, target_os = "macos"))]
    pub(crate) fn session_id_for_test(&self) -> Option<SessionId> {
        self.selected_id()
    }

    /// The resting page reads this Mac's detection facts, never a remote
    /// target's: a newcomer's first session is local, and only a local
    /// installer can be run from here.
    fn render_empty_workbench(&self, colors: SemanticColors) -> impl IntoElement + use<> {
        let state = {
            let store = self
                .runtime
                .store
                .read()
                .expect("session store lock poisoned");
            crate::empty_workbench::EmptyWorkbench {
                has_sessions: !store.sessions().is_empty(),
                agents: crate::agent_setup::AgentSetupState::from_catalog(
                    store.agent_catalog(None),
                ),
                installing: store.installing_agent().cloned(),
                scanning: store.agent_catalog_is_loading(None),
                herdr: store
                    .herdr()
                    .plan
                    .as_ref()
                    .filter(|plan| !plan.is_empty())
                    .map(|plan| plan.headline().into()),
                importing_herdr: store.herdr().importing,
            }
        };
        let canonical = Arc::clone(&self.runtime.store);
        let window_store = self.window_store.clone();
        let install: crate::agent_setup::InstallHandler = Rc::new(move |option, cx| {
            if let Some(window_store) = &window_store {
                window_store
                    .write()
                    .expect("window navigation lock poisoned")
                    .install_agent(option);
            } else {
                canonical
                    .write()
                    .expect("session store lock poisoned")
                    .install_agent(option, None);
            }
            cx.refresh_windows();
        });
        let canonical = Arc::clone(&self.runtime.store);
        let check_again: crate::agent_setup::ActionHandler = Rc::new(move |_, cx| {
            canonical
                .write()
                .expect("session store lock poisoned")
                .request_agent_catalog(None, true);
            cx.refresh_windows();
        });
        // A direct launch, the same one the New Agent shortcut performs: the
        // agent's own prompt takes the task, so there is nothing to compose
        // or inject on the way in.
        let canonical = Arc::clone(&self.runtime.store);
        let window_store = self.window_store.clone();
        let start_in_folder: crate::agent_setup::ActionHandler = Rc::new(move |_, cx| {
            let paths = cx.prompt_for_paths(gpui::PathPromptOptions {
                files: false,
                directories: true,
                multiple: false,
                prompt: Some("Start Here".into()),
            });
            let canonical = Arc::clone(&canonical);
            let window_store = window_store.clone();
            cx.spawn(async move |cx| {
                let Ok(Ok(Some(mut paths))) = paths.await else {
                    return;
                };
                let Some(path) = paths.pop() else {
                    return;
                };
                let options = crate::store::SpawnOptions {
                    cwd: Some(path.to_string_lossy().into_owned()),
                    ..crate::store::SpawnOptions::default()
                };
                if let Some(window_store) = &window_store {
                    window_store
                        .write()
                        .expect("window navigation lock poisoned")
                        .spawn_default(options);
                } else {
                    canonical
                        .write()
                        .expect("session store lock poisoned")
                        .spawn_default(options);
                }
                cx.update(|cx| cx.refresh_windows());
            })
            .detach();
        });
        let canonical = Arc::clone(&self.runtime.store);
        let import_herdr: crate::agent_setup::ActionHandler = Rc::new(move |window, cx| {
            let plan = canonical
                .read()
                .expect("session store lock poisoned")
                .herdr()
                .plan
                .clone();
            let Some(plan) = plan.filter(|plan| !plan.is_empty()) else {
                return;
            };
            let canonical = Arc::clone(&canonical);
            crate::herdr_import::confirm(&plan, window, cx, move |cx| {
                canonical
                    .write()
                    .expect("session store lock poisoned")
                    .import_herdr();
                cx.refresh_windows();
            });
        });
        crate::empty_workbench::render(
            state,
            crate::empty_workbench::EmptyWorkbenchActions {
                install,
                check_again,
                start_in_folder,
                import_herdr,
            },
            colors,
        )
    }

    fn selected_id(&self) -> Option<SessionId> {
        match &self.session_source {
            SessionSource::FollowSelection => self.window_store.as_ref().map_or_else(
                || {
                    self.runtime
                        .store
                        .read()
                        .expect("store")
                        .selected_session_id()
                        .cloned()
                },
                |store| store.read().expect("store").selected_session_id().cloned(),
            ),
            SessionSource::Fixed(id) => Some(id.clone()),
        }
    }

    fn open_account_continuation(&self, cx: &mut Context<Self>) {
        if let Some(session) = self.selected_session()
            && matches!(
                session.kind.id(),
                diri_proto::AgentKind::CLAUDE_CODE_ID | diri_proto::AgentKind::CODEX_ID
            )
        {
            cx.emit(TerminalPaneEvent::ContinueAccount(session.id.clone()));
        }
    }

    /// The sidebar palette the Links popover paints with, for its panel.
    fn panel_colors(&self) -> SemanticColors {
        let store = self
            .runtime
            .store
            .read()
            .expect("session store lock poisoned");
        crate::app_theme::sidebar_colors_in(&store)
    }

    /// Runs `f` against the pane's own window even from a panel handler.
    fn in_main_window(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        f: impl FnOnce(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) {
        crate::floating::in_main_window(self, window, cx, f);
    }

    fn selected_session(&self) -> Option<Arc<SessionRecord>> {
        let id = self.selected_id()?;
        self.runtime
            .store
            .read()
            .expect("session store lock poisoned")
            .sessions()
            .get(&id)
            .map(Arc::clone)
    }

    fn open_find(&mut self, _: &OpenFind, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.selected_id() else {
            return;
        };
        let Some(resident) = self.residents.get_mut(&id) else {
            return;
        };
        if resident.find.is_none() {
            resident.find_composition.cancel(&mut resident.find_query);
            resident.element.set_text_input_enabled(false);
            // Local and remote sessions both search a capture of their
            // history; a remote host that cannot provide one falls back to its
            // screen on the first answer.
            let mut find = TerminalFindModel::retained();
            find.set_query(
                resident.find_query.text().to_owned(),
                self.started_at.elapsed(),
            );
            resident.find = Some(find);
            // Reopening keeps the last query but selects it, so ⌘F then typing
            // starts a new search while ⌘F then ⏎ repeats the old one.
            resident.find_query.select_all();
        }
        self.schedule_query_search(id, window, cx);
        window.focus(&self.focus, cx);
        cx.stop_propagation();
        cx.notify();
    }

    fn close_find(&mut self, _: &CloseFind, window: &mut Window, cx: &mut Context<Self>) {
        if self.close_find_for_selected() {
            find_input::discard_native(window, cx);
            cx.stop_propagation();
            cx.notify();
        } else {
            cx.propagate();
        }
    }

    fn close_find_for_selected(&mut self) -> bool {
        let Some(id) = self.selected_id() else {
            return false;
        };
        let Some(resident) = self.residents.get_mut(&id) else {
            return false;
        };
        if resident.find.take().is_none() {
            return false;
        }
        resident.find_composition.cancel(&mut resident.find_query);
        resident.element.clear_find_source();
        resident.element.set_text_input_enabled(true);
        resident.find_scheduler.cancel();
        resident.element.set_find_highlights(Vec::new());
        true
    }

    fn cancel_find_composition(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut changed = Vec::new();
        let mut owns_input = false;
        for (id, resident) in &mut self.residents {
            if resident.find.is_none() {
                continue;
            }
            owns_input = true;
            resident.find_composition.cancel(&mut resident.find_query);
            if let Some(find) = resident.find.as_mut()
                && find.set_query(
                    resident.find_query.text().to_owned(),
                    self.started_at.elapsed(),
                )
            {
                resident.element.set_find_highlights(Vec::new());
                changed.push(id.clone());
            }
        }
        for id in changed {
            self.schedule_query_search(id, window, cx);
        }
        if owns_input {
            find_input::discard_native(window, cx);
            cx.notify();
        }
    }

    fn find_next(&mut self, _: &FindNext, _window: &mut Window, cx: &mut Context<Self>) {
        self.navigate_find(false, cx);
    }

    fn find_previous(&mut self, _: &FindPrevious, _window: &mut Window, cx: &mut Context<Self>) {
        self.navigate_find(true, cx);
    }

    fn refresh_find(&mut self, cx: &mut Context<Self>) {
        if let Some(id) = self.selected_id() {
            if let Some(resident) = self.residents.get_mut(&id)
                && let Some(find) = resident.find.as_mut()
            {
                resident
                    .element
                    .scroll_to_live(usize::from(resident.last_size.1));
                find.refresh(self.started_at.elapsed());
            }
            self.start_due_find(&id);
            cx.notify();
        }
    }

    fn return_to_live(&mut self, id: &SessionId, cx: &mut Context<Self>) {
        if let Some(resident) = self.residents.get_mut(id) {
            resident
                .element
                .scroll_to_live(usize::from(resident.last_size.1));
            if let Some(find) = resident.find.as_mut()
                && find.is_paused()
            {
                find.refresh(self.started_at.elapsed());
            }
            self.start_due_find(id);
            cx.notify();
        }
    }

    fn navigate_find(&mut self, backwards: bool, cx: &mut Context<Self>) {
        let Some(id) = self.selected_id() else {
            return;
        };
        let Some(resident) = self.residents.get_mut(&id) else {
            return;
        };
        let Some(find) = resident.find.as_mut() else {
            return;
        };
        if backwards {
            resident.element.find_previous(find);
        } else {
            resident.element.find_next(find);
        }
        resident.element.sync_find_highlights(find);
        cx.stop_propagation();
        cx.notify();
    }

    fn zoom_in(&mut self, _: &ZoomIn, window: &mut Window, cx: &mut Context<Self>) {
        self.change_zoom(1.0, false, window, cx);
    }

    fn zoom_out(&mut self, _: &ZoomOut, window: &mut Window, cx: &mut Context<Self>) {
        self.change_zoom(-1.0, false, window, cx);
    }

    fn reset_zoom(&mut self, _: &ResetZoom, window: &mut Window, cx: &mut Context<Self>) {
        self.change_zoom(0.0, true, window, cx);
    }

    fn change_zoom(
        &mut self,
        delta: f32,
        reset: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let result = {
            let mut store = self
                .runtime
                .store
                .write()
                .expect("session store lock poisoned");
            if reset {
                store.reset_terminal_zoom()
            } else {
                store.zoom_terminal(delta)
            }
        };
        if result.is_ok() {
            self.update_selected_geometry(window, cx);
            cx.stop_propagation();
            cx.notify();
        }
    }

    /// Grid cell under a window-space pointer position, using the same
    /// geometry as `handle_scroll`.
    fn grid_cell_at(
        &self,
        position: gpui::Point<gpui::Pixels>,
        window: &mut Window,
    ) -> Option<(usize, usize)> {
        self.selected_session()?;
        let store = self
            .runtime
            .store
            .read()
            .expect("session store lock poisoned");
        let font_size = store.preferences().terminal_font_size;
        drop(store);
        let metrics = CellMetrics::measure(
            window.text_system(),
            &font(crate::fonts::mono_family()),
            px(font_size),
        );
        let viewport = self.viewport.unwrap_or_default();
        let grid_x = viewport.x + GRID_HORIZONTAL_PADDING / 2.0;
        // An overflowing grid is bottom-anchored (see render_grid_and_overlays),
        // so its first row sits above the surface -- selection has to follow it
        // or clicks land on the wrong line while a resize is in flight.
        let grid_rows = self
            .selected_id()
            .and_then(|id| self.residents.get(&id))
            .map_or(0, |resident| resident.element.grid_rows());
        let anchor = self
            .grid_row_overflow(grid_rows, font_size, window)
            .map_or(0.0, |grid_height| self.grid_inner_height() - grid_height);
        let grid_y = viewport.y + self.header_height() + 2.0 + anchor;
        let col = ((f32::from(position.x) - grid_x) / f32::from(metrics.cell_width))
            .floor()
            .max(0.0) as usize;
        let resident = self.selected_id().and_then(|id| self.residents.get(&id))?;
        // A reading view resting between rows is painted this far above its
        // whole-row positions, and shows part of one more row at the bottom.
        let shift = f32::from(
            resident
                .element
                .scroll_shift(metrics.line_height, window.scale_factor()),
        );
        let row = ((f32::from(position.y) - grid_y + shift) / f32::from(metrics.line_height))
            .floor()
            .max(0.0) as usize;
        // The extra row is addressable for selection and links. A program
        // reading the mouse is only ever told about rows of its own screen.
        let extra_row = shift > 0.0 && !resident.element.mouse_modes().is_reporting();
        clamp_grid_cell(
            col,
            row,
            resident.element.grid_cols(),
            resident
                .element
                .grid_rows()
                .saturating_add(u16::from(extra_row)),
        )
        .map(|(col, row)| (usize::from(col), usize::from(row)))
    }

    fn handle_pointer_down(
        &mut self,
        event: &gpui::MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(id) = self.selected_id() else {
            return;
        };
        let Some((col, row)) = self.grid_cell_at(event.position, window) else {
            return;
        };
        self.reset_qol_session(&id);
        self.qol.menu = None;
        if self.qol.paste.is_some() {
            cx.stop_propagation();
            return;
        }
        if event.button == MouseButton::Right
            && (event.modifiers.alt
                || self
                    .residents
                    .get(&id)
                    .is_none_or(|r| !r.element.mouse_modes().is_reporting()))
        {
            self.open_terminal_menu(event.position, col, row, window, cx);
            return;
        }
        let owner = {
            let Some(resident) = self.residents.get(&id) else {
                return;
            };
            pointer_owner(
                resident.element.mouse_modes(),
                event.button,
                &event.modifiers,
            )
        };
        let Some(resident) = self.residents.get_mut(&id) else {
            return;
        };
        resident.pointer_owner = Some((event.button, owner));
        resident.mouse_motion.reset();

        match owner {
            PointerOwner::LocalSelection => {
                match event.click_count {
                    1 if event.modifiers.alt && event.modifiers.shift => {
                        resident.element.begin_rectangle_selection(col, row)
                    }
                    1 => resident.element.begin_selection(col, row),
                    2 => resident.element.select_word(col, row),
                    _ => resident.element.select_line(row),
                }
                cx.notify();
            }
            PointerOwner::LocalReference => {
                self.qol.pressed = resident
                    .element
                    .reference_hit_at(col, row)
                    .map(|hit| (hit, (col, row)));
                cx.stop_propagation();
            }
            PointerOwner::Terminal => {
                let Some(button) = terminal_mouse_button(event.button) else {
                    return;
                };
                if let Some(bytes) = encode_mouse_event(
                    resident.element.mouse_modes(),
                    TerminalMouseEvent::Press(button),
                    terminal_mouse_modifiers(&event.modifiers),
                    col as u16,
                    row as u16,
                ) {
                    resident.attachment.mouse(bytes);
                }
                cx.stop_propagation();
            }
            PointerOwner::Ignored => {}
        }
    }

    fn handle_pointer_up(
        &mut self,
        event: &gpui::MouseUpEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(id) = self.selected_id() else {
            return;
        };
        // Resolve opportunistically, but clear gesture state even if the grid
        // disappeared between press and release (session teardown, a zero-size
        // re-seed). GPUI delivers `on_mouse_up_out` in capture phase, so an
        // ordinary release outside the pane still reaches this path and clamps.
        self.qol.drag = None;
        self.qol.autoscroll = None;
        let cell = self.grid_cell_at(event.position, window);
        let copy_on_select = self
            .runtime
            .store
            .read()
            .expect("store")
            .preferences()
            .terminal_copy_on_select;
        let inside = self.viewport.is_some_and(|viewport| {
            let x = f32::from(event.position.x);
            let y = f32::from(event.position.y);
            x >= viewport.x
                && x < viewport.x + viewport.width
                && y >= viewport.y + self.header_height()
                && y < viewport.y + viewport.height
        });
        let pressed = self.qol.pressed.take();
        let Some(resident) = self.residents.get_mut(&id) else {
            return;
        };
        let (owner, pending) = finish_pointer_state(
            &mut resident.pointer_owner,
            &mut resident.mouse_motion,
            event.button,
            cell.is_some(),
        );
        if owner == Some(PointerOwner::LocalReference) {
            let hit = cell.and_then(|(col, row)| resident.element.reference_hit_at(col, row));
            if let Some((pressed, point)) = pressed
                && inside
                && cell == Some(point)
                && hit.as_ref() == Some(&pressed)
            {
                self.open_reference(pressed.reference, window, cx);
            }
            cx.stop_propagation();
            return;
        }
        if owner == Some(PointerOwner::LocalSelection) {
            // The one place every local selection gesture ends: a released
            // drag and a double or triple click alike. The element ignores an
            // empty selection and stays static under Reduce Motion.
            if event.button == MouseButton::Left && resident.element.complete_selection() {
                cx.notify();
            }
            if copy_on_select {
                self.copy_selection(&CopySelection, window, cx);
            }
            return;
        }
        if owner != Some(PointerOwner::Terminal) {
            return;
        }
        let Some((col, row)) = cell else {
            // With no authoritative coordinate, a guessed release would be
            // worse than dropping this now-cancelled gesture.
            return;
        };
        if let Some(bytes) = pending {
            // A cadence-held drag must precede its release. Letting the timer
            // fire afterward would resurrect a button that is already up.
            resident.attachment.mouse(bytes);
        }
        let Some(button) = terminal_mouse_button(event.button) else {
            return;
        };
        if let Some(bytes) = encode_mouse_event(
            resident.element.mouse_modes(),
            TerminalMouseEvent::Release(button),
            terminal_mouse_modifiers(&event.modifiers),
            col as u16,
            row as u16,
        ) {
            resident.attachment.mouse(bytes);
        }
        cx.stop_propagation();
    }

    fn handle_pointer_move(
        &mut self,
        event: &gpui::MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(id) = self.selected_id() else {
            return;
        };
        let Some((col, row)) = self.grid_cell_at(event.position, window) else {
            return;
        };
        self.reset_qol_session(&id);
        if self
            .qol
            .pressed
            .as_ref()
            .is_some_and(|(_, point)| *point != (col, row))
        {
            self.qol.pressed = None;
        }
        if event.pressed_button.is_none() {
            let next = Some((col, row));
            if self.qol.hover != next {
                self.qol.hover = next;
                cx.notify();
            }
        } else {
            self.qol.hover = None;
        }
        let selecting = self.residents.get(&id).is_some_and(|r| {
            r.pointer_owner == Some((MouseButton::Left, PointerOwner::LocalSelection))
        });
        if selecting {
            self.update_selection_autoscroll(event.position, col, row, window, cx);
        }
        let (dispatch, attachment) = {
            let Some(resident) = self.residents.get_mut(&id) else {
                return;
            };
            let owner = event.pressed_button.and_then(|button| {
                resident
                    .pointer_owner
                    .filter(|(owned, _)| *owned == button)
                    .map(|(_, owner)| owner)
            });
            if owner == Some(PointerOwner::LocalSelection) {
                resident.element.drag_selection(col, row);
                cx.notify();
                return;
            }
            if event.pressed_button.is_some() && owner != Some(PointerOwner::Terminal) {
                return;
            }
            let button = match event.pressed_button {
                Some(button) => terminal_mouse_button(button).map(Some),
                None => Some(None),
            };
            let Some(button) = button else {
                return;
            };
            let Some(bytes) = encode_mouse_event(
                resident.element.mouse_modes(),
                TerminalMouseEvent::Motion(button),
                terminal_mouse_modifiers(&event.modifiers),
                col as u16,
                row as u16,
            ) else {
                return;
            };
            (
                resident
                    .mouse_motion
                    .push(Instant::now(), (col as u16, row as u16), bytes),
                resident.attachment.clone(),
            )
        };
        match dispatch {
            MotionDispatch::SendNow(bytes) => attachment.mouse_motion(bytes),
            MotionDispatch::Schedule { delay, generation } => {
                self.schedule_mouse_motion_flush(id, delay, generation, cx);
            }
            MotionDispatch::None => return,
        }
        cx.stop_propagation();
    }

    fn schedule_mouse_motion_flush(
        &mut self,
        id: SessionId,
        delay: Duration,
        generation: u64,
        cx: &mut Context<Self>,
    ) {
        let timer = cx.background_executor().timer(delay);
        cx.spawn(async move |this, cx| {
            timer.await;
            let _ = this.update(cx, |this, _cx| {
                if this.selected_id().as_ref() != Some(&id) {
                    if let Some(resident) = this.residents.get_mut(&id) {
                        resident.mouse_motion.reset();
                    }
                    return;
                }
                let Some(resident) = this.residents.get_mut(&id) else {
                    return;
                };
                if let Some(bytes) = resident.mouse_motion.flush(generation, Instant::now()) {
                    resident.attachment.mouse_motion(bytes);
                }
            });
        })
        .detach();
    }

    /// The height the mirrored grid needs when the daemon's screen is taller
    /// than the pane can show, or `None` when it fits. Only a resize still in
    /// flight puts the two out of step, so this is `None` on settled frames.
    fn grid_row_overflow(
        &self,
        grid_rows: u16,
        font_size: f32,
        window: &mut Window,
    ) -> Option<f32> {
        if grid_rows == 0 || self.viewport.is_none() {
            return None;
        }
        let metrics = CellMetrics::measure(
            window.text_system(),
            &font(crate::fonts::mono_family()),
            px(font_size),
        );
        // A pixel of slack on top of the exact row height: the element derives
        // its row count back out with `floor(height / line_height)`, and an
        // exactly-sized box loses its last row to float error or to layout
        // rounding -- which is the row this anchoring exists to keep on screen.
        (grid_rows > metrics.rows_for_height(px(self.grid_inner_height())))
            .then(|| f32::from(metrics.line_height).mul_add(f32::from(grid_rows), ANCHOR_SLACK))
    }

    /// Height available to `TerminalElement` inside the terminal surface -- the
    /// same figure [`estimated_grid_size`] turns into a row count.
    fn grid_inner_height(&self) -> f32 {
        let height = self.viewport.map_or(0.0, |viewport| viewport.height);
        (height - self.header_height() - GRID_VERTICAL_PADDING - GRID_LAYOUT_VERTICAL_CHROME)
            .max(1.0)
    }

    fn copy_selection(&mut self, _: &CopySelection, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.selected_id() else {
            return;
        };
        let Some(resident) = self.residents.get(&id) else {
            return;
        };
        let text = resident.element.selected_text();
        if !text.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
            self.show_terminal_feedback("Copied", window, cx);
        }
    }

    /// Captures terminal text together with the stable absolute scrollback
    /// rows that locate it approximately within the source session.
    #[must_use]
    pub fn quote_selection(&self) -> Option<Quote> {
        let id = self.selected_id()?;
        let resident = self.residents.get(&id)?;
        quote_from_terminal_element(id, &resident.element)
    }

    /// Pastes a clipboard image as the path of its staged file, copying it to
    /// the session's host first when that is not this machine.
    fn paste_staged_clipboard_image(
        &mut self,
        id: &SessionId,
        staged: std::io::Result<StagedClipboardImage>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let staged = match staged {
            Ok(staged) => staged,
            Err(error) => {
                eprintln!("diri: could not stage clipboard image: {error}");
                // The kind says what went wrong ("storage full") without the
                // temp path the full error may carry.
                self.show_terminal_feedback(
                    format!("Couldn't paste the clipboard image: {}", error.kind()),
                    window,
                    cx,
                );
                return;
            }
        };
        let ssh = {
            let store = self
                .runtime
                .store
                .read()
                .expect("session store lock poisoned");
            store
                .sessions()
                .get(id)
                .and_then(|session| session.host.as_deref())
                .and_then(|host_id| store.host(host_id))
                .map(|host| host.ssh.clone())
        };

        if let Some(ssh) = ssh {
            let pane_tx = self.pane_tx.clone();
            let upload_id = id.clone();
            self.tokio.spawn(async move {
                let result = tokio::task::spawn_blocking(move || staged.upload(&ssh))
                    .await
                    .unwrap_or_else(|error| Err(format!("upload task failed: {error}")));
                let _ = pane_tx.send(PaneEvent::ClipboardUploadFinished(upload_id, result));
            });
        } else {
            let local_path = staged.path().to_string_lossy().into_owned();
            if let Some(resident) = self.residents.get(id) {
                resident.send_user_input(terminal_paste(&local_path, resident.bracketed_paste));
            }
            self.local_clipboard_images.push(staged);
            if self.local_clipboard_images.len() > 32 {
                self.local_clipboard_images.remove(0);
            }
        }
    }

    fn paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        if self.qol.copy_mode.is_some() {
            self.show_terminal_feedback("Exit copy mode before pasting", window, cx);
            cx.stop_propagation();
            return;
        }
        let Some(item) = cx.read_from_clipboard() else {
            return;
        };
        let Some(id) = self.selected_id() else {
            return;
        };

        if let Some((bytes, extension)) = clipboard_image(&item) {
            let in_find = self
                .residents
                .get(&id)
                .is_some_and(|resident| resident.find.is_some());
            if in_find {
                return;
            }

            let staged = StagedClipboardImage::stage(bytes, extension);
            self.paste_staged_clipboard_image(&id, staged, window, cx);
            cx.stop_propagation();
            cx.notify();
            return;
        }

        let Some(text) = item.text() else {
            return;
        };
        if self
            .residents
            .get(&id)
            .is_some_and(|resident| resident.find.is_none())
            && self.stage_paste_if_needed(&id, &text, cx)
        {
            return;
        }
        let now = self.started_at.elapsed();
        let Some(resident) = self.residents.get_mut(&id) else {
            return;
        };
        if let Some(find) = resident.find.as_mut() {
            resident
                .find_composition
                .commit(&mut resident.find_query, &text);
            find_input::discard_native(window, cx);
            let query = resident.find_query.text().to_owned();
            if find.set_query(query, now) {
                resident.element.set_find_highlights(Vec::new());
            }
            self.schedule_query_search(id, window, cx);
        } else {
            resident.send_user_input(terminal_paste(&text, resident.bracketed_paste));
        }
        cx.stop_propagation();
        cx.notify();
    }

    fn handle_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(navigation) = &self.navigation
            && navigation.read(cx).is_open()
        {
            navigation.update(cx, |navigation, cx| {
                navigation.on_key_down(event, window, cx);
            });
            cx.stop_propagation();
            return;
        }
        if let Some(surfaces) = &self.utility_surfaces
            && surfaces.read(cx).is_open()
        {
            surfaces.update(cx, |surfaces, cx| {
                surfaces.key_down(event, window, cx);
            });
            cx.stop_propagation();
            return;
        }

        if self.handle_qol_key(event, window, cx) {
            cx.stop_propagation();
            return;
        }
        self.qol.hover = None;
        self.qol.hit = None;
        let switcher_key = switcher_key(event);
        let switcher_handled = if let Some(window_store) = &self.window_store {
            let mut store = window_store.write().expect("session store lock poisoned");
            let was_visible = store.switcher_state().is_visible();
            let handled = if was_visible
                || matches!(
                    switcher_key,
                    crate::switcher::SwitcherKey::Tab { control: true, .. }
                ) {
                store.handle_switcher_key(switcher_key)
            } else {
                false
            };
            if handled && !was_visible && store.switcher_state().is_visible() {
                store.dismiss_overview();
            }
            handled
        } else {
            let mut store = self
                .runtime
                .store
                .write()
                .expect("session store lock poisoned");
            let was_visible = store.switcher_state().is_visible();
            let handled = if was_visible
                || matches!(
                    switcher_key,
                    crate::switcher::SwitcherKey::Tab { control: true, .. }
                ) {
                store.handle_switcher_key(switcher_key)
            } else {
                false
            };
            if handled && !was_visible && store.switcher_state().is_visible() {
                store.dismiss_overview();
            }
            handled
        };
        if switcher_handled {
            cx.stop_propagation();
            cx.notify();
            return;
        }

        let Some(id) = self.selected_id() else {
            return;
        };
        let now = self.started_at.elapsed();
        let Some(resident) = self.residents.get_mut(&id) else {
            return;
        };

        if let Some(find) = resident.find.as_mut() {
            match event.keystroke.key.as_str() {
                "escape" => {
                    resident.find = None;
                    resident.element.clear_find_source();
                    resident.find_composition.cancel(&mut resident.find_query);
                    resident.element.set_text_input_enabled(true);
                    find_input::discard_native(window, cx);
                    resident.find_scheduler.cancel();
                    resident.element.set_find_highlights(Vec::new());
                    cx.notify();
                }
                "enter" => {
                    if event.keystroke.modifiers.shift {
                        resident.element.find_previous(find);
                    } else {
                        resident.element.find_next(find);
                    }
                    resident.element.sync_find_highlights(find);
                    cx.notify();
                }
                // Everything else is text editing, through the same key map the
                // command palette and Quick Open use.
                _ => {
                    let Some(edit) = query_editor::edit_for(&event.keystroke) else {
                        cx.propagate();
                        return;
                    };
                    let changed = match edit {
                        Edit::Local(local) => {
                            resident.find_composition.finish();
                            resident.find_query.apply(local)
                        }
                        Edit::Clipboard(ClipboardEdit::Copy) => {
                            query_editor::copy_selection(&resident.find_query, cx);
                            false
                        }
                        Edit::Clipboard(ClipboardEdit::Cut) => {
                            resident.find_composition.finish();
                            query_editor::cut_selection(&mut resident.find_query, cx)
                        }
                        // ⌘V is already an action (it also handles image
                        // pastes); claiming it here too would insert twice.
                        Edit::Clipboard(ClipboardEdit::Paste) => {
                            cx.propagate();
                            return;
                        }
                    };
                    if changed {
                        let query = resident.find_query.text().to_owned();
                        if find.set_query(query, now) {
                            resident.element.set_find_highlights(Vec::new());
                        }
                        self.schedule_query_search(id, window, cx);
                    }
                }
            }
            cx.stop_propagation();
            cx.notify();
            return;
        }

        if event.keystroke.modifiers.platform && event.keystroke.key != "backspace" {
            if let Some(bytes) = terminal_command_navigation(&event.keystroke) {
                if resident.send_user_input(bytes.to_vec()) {
                    cx.notify();
                }
                cx.stop_propagation();
                return;
            }
            cx.propagate();
            return;
        }
        let Some(term_event) = terminal_key_event(event) else {
            cx.propagate();
            return;
        };
        let modifiers = TermModifiers {
            shift: event.keystroke.modifiers.shift,
            ctrl: event.keystroke.modifiers.control,
            alt: event.keystroke.modifiers.alt,
            cmd: event.keystroke.modifiers.platform,
        };
        let bytes = match diri_term::keys::encode_interactive_action(
            &term_event,
            modifiers,
            resident.keyboard,
            if event.is_held {
                diri_term::keys::KeyAction::Repeat
            } else {
                diri_term::keys::KeyAction::Press
            },
        ) {
            Ok(bytes) => bytes,
            Err(error) => {
                self.show_terminal_feedback(error.to_string(), window, cx);
                cx.stop_propagation();
                return;
            }
        };
        if bytes.is_empty() {
            cx.propagate();
        } else {
            if resident.send_user_input(bytes) {
                cx.notify();
            }
            cx.stop_propagation();
        }
    }

    fn finish_switcher_modifiers(&self, control: bool) -> bool {
        if let Some(window_store) = &self.window_store {
            let mut store = window_store.write().expect("session store lock poisoned");
            let was_visible = store.switcher_state().is_visible();
            store.handle_switcher_modifiers_changed(control);
            was_visible != store.switcher_state().is_visible()
        } else {
            let mut store = self
                .runtime
                .store
                .write()
                .expect("session store lock poisoned");
            let was_visible = store.switcher_state().is_visible();
            store.handle_switcher_modifiers_changed(control);
            was_visible != store.switcher_state().is_visible()
        }
    }

    fn handle_key_up(&mut self, event: &KeyUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if matches!(event.keystroke.key.as_str(), "control" | "ctrl")
            && self.finish_switcher_modifiers(false)
        {
            cx.notify();
        }
    }

    fn handle_modifiers_changed(
        &mut self,
        event: &ModifiersChangedEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.finish_switcher_modifiers(event.modifiers.control) {
            cx.notify();
        }
    }

    /// Starts the next queued scrollback fetch for `id`, if the viewport wants
    /// one and none is in flight. Called from wheel events AND from fetch
    /// completion: a fast wheel burst queues the next window while a fetch is
    /// in flight, and nothing else would ever start it — the stranded queue
    /// painted as a transient blank region in deep scrollback.
    fn pump_scrollback_fetch(&mut self, id: &SessionId, visible_rows: usize) {
        let Some(resident) = self.residents.get_mut(id) else {
            return;
        };
        let Some(request) = resident.element.begin_scrollback_fetch(visible_rows) else {
            return;
        };
        let generation = resident.attachment_generation;
        let client = Arc::clone(self.runtime.client());
        let pane_tx = self.pane_tx.clone();
        let fetch_id = id.clone();
        self.tokio.spawn(async move {
            match client
                .read_scrollback_cells(&fetch_id, request.first_row, request.max_rows)
                .await
            {
                Ok(result) => {
                    let _ = pane_tx.send(PaneEvent::ScrollbackCells(
                        fetch_id,
                        generation,
                        result,
                        visible_rows,
                    ));
                }
                Err(_) => {
                    let _ = pane_tx.send(PaneEvent::ScrollbackFailed(fetch_id, generation));
                }
            }
        });
    }

    /// Learns how much history sits above a live view whose scroller knob is
    /// showing. Live grid updates carry no history geometry, so without this
    /// the knob is sized from a one-screen guess (or a figure from the last
    /// time the session was scrolled) and jumps to its real size on the first
    /// scroll. Only a visible knob asks, and only when the screen has changed
    /// since it last did.
    fn probe_history_extent(&mut self, id: &SessionId) {
        if !self.scroller.is_revealed() {
            return;
        }
        let Some(resident) = self.residents.get_mut(id) else {
            return;
        };
        let Some(generation) = resident.element.indicator_probe_generation() else {
            return;
        };
        let now = Instant::now();
        if !resident.extent_probe.should_send(generation, now) {
            return;
        }
        resident.extent_probe = HistoryExtentProbe {
            in_flight: true,
            generation: Some(generation),
            sent_at: Some(now),
        };
        let attachment_generation = resident.attachment_generation;
        let client = Arc::clone(self.runtime.client());
        let pane_tx = self.pane_tx.clone();
        let probe_id = id.clone();
        self.tokio.spawn(async move {
            let live_start_row = client
                .read_scrollback_cells(&probe_id, 0, 1)
                .await
                .ok()
                .map(|result| result.live_start_row);
            let _ = pane_tx.send(PaneEvent::HistoryExtent(
                probe_id,
                attachment_generation,
                live_start_row,
            ));
        });
    }

    fn handle_scroll(
        &mut self,
        event: &ScrollWheelEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(id) = self.selected_id() else {
            return;
        };
        let font_size = self
            .runtime
            .store
            .read()
            .expect("session store lock poisoned")
            .preferences()
            .terminal_font_size;
        let font = font(crate::fonts::mono_family());
        let metrics = CellMetrics::measure(window.text_system(), &font, px(font_size));
        let viewport = self.viewport.unwrap_or_default();
        let grid_x = viewport.x + GRID_HORIZONTAL_PADDING / 2.0;
        let grid_y = viewport.y + self.header_height() + 2.0;
        let col = ((f32::from(event.position.x) - grid_x) / f32::from(metrics.cell_width))
            .floor()
            .max(0.0) as u16;
        let row = ((f32::from(event.position.y) - grid_y) / f32::from(metrics.line_height))
            .floor()
            .max(0.0) as u16;
        let delta = match event.delta {
            ScrollDelta::Pixels(point) => WheelDelta::PrecisePoints(f32::from(point.y)),
            ScrollDelta::Lines(point) => WheelDelta::Lines(point.y),
        };
        let Some(resident) = self.residents.get_mut(&id) else {
            return;
        };
        let visible_rows = resident.last_size.1.max(1);
        let route = resident.element.route_wheel(WheelEvent {
            delta,
            col,
            row,
            visible_rows,
            line_height: f32::from(metrics.line_height),
        });
        match route {
            Some(WheelRoute::Daemon {
                direction,
                lines,
                col,
                row,
            }) => resident.attachment.scroll(direction, lines, col, row),
            Some(WheelRoute::Local { .. }) => {
                self.pump_scrollback_fetch(&id, usize::from(visible_rows));
            }
            None => return,
        }
        cx.stop_propagation();
        cx.notify();
    }

    fn update_selected_geometry(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self.selected_session() else {
            return;
        };
        let font_size = self
            .runtime
            .store
            .read()
            .expect("session store lock poisoned")
            .preferences()
            .terminal_font_size;
        let already_sized = self
            .residents
            .get(&session.id)
            .is_some_and(|resident| resident.last_size != (0, 0));
        // The full-window fallback is a launch guess. An already-sized session
        // waits for the real pane viewport so that guess cannot become a
        // second PTY size.
        let Some(viewport) = self.viewport.or_else(|| {
            if already_sized {
                return None;
            }
            let size = window.viewport_size();
            Some(TerminalViewport {
                x: 0.0,
                y: 0.0,
                width: f32::from(size.width),
                height: f32::from(size.height),
            })
        }) else {
            return;
        };
        let metrics = CellMetrics::measure(
            window.text_system(),
            &font(crate::fonts::mono_family()),
            px(font_size),
        );
        let size = estimated_grid_size(
            viewport.width,
            viewport.height,
            self.header_height(),
            0.0,
            metrics,
        );
        if let Some(resident) = self.residents.get_mut(&session.id)
            && resident.attachment.is_controller()
            && resident.last_size != (0, 0)
            && resident.last_size == size
        {
            // The pane still matches the size this session was already using.
            // An unchanged pane must not send a resize at all. Recording that
            // size would let the next attach replay it.
            return;
        }
        if let Some(resident) = self.residents.get_mut(&session.id)
            && resident.attachment.is_controller()
            && (resident.last_size != size || resident.attachment.needs_resize(size))
        {
            // Leading edge: an isolated change (first measure after attach, a
            // session switch, a window snap, the first frame of a drag) reaches
            // the daemon immediately so the pane feels instant.
            let previous = resident.last_size;
            let first_measure = previous == (0, 0);
            resident.last_size = size;
            let now = Instant::now();
            let since_sent = self.last_resize_sent.map(|at| now.duration_since(at));
            let delay = match plan_resize(first_measure, since_sent, self.resize_flush_armed) {
                ResizePlan::SendNow => {
                    self.last_resize_sent = Some(now);
                    self.pending_resizes.remove(&session.id);
                    resident.attachment.resize(size.0, size.1);
                    if should_hold_reflow(previous, size, since_sent) {
                        self.hold_reflow(session.id.clone(), window, cx);
                    }
                    return;
                }
                // Mid-drag: fold into the tick already armed. It is never
                // rescheduled by a later frame -- it fires on the cadence
                // carrying whatever the newest size is by then -- so a
                // continuous drag keeps the PTY reflowing at ~20Hz instead of
                // waiting for the mouse to stop.
                ResizePlan::Fold => {
                    self.pending_resizes.insert(
                        session.id.clone(),
                        (size, resident.attachment.ownership_revision()),
                    );
                    return;
                }
                ResizePlan::Arm(delay) => delay,
            };
            self.pending_resizes.insert(
                session.id.clone(),
                (size, resident.attachment.ownership_revision()),
            );
            self.resize_flush_armed = true;
            let timer = cx.background_executor().timer(delay);
            self.resize_flush = Some(cx.spawn(async move |this, cx| {
                timer.await;
                let _ = this.update(cx, |this, _cx| {
                    this.resize_flush_armed = false;
                    this.last_resize_sent = Some(Instant::now());
                    let pending = std::mem::take(&mut this.pending_resizes);
                    for (id, (size, revision)) in pending {
                        if let Some(resident) = this.residents.get(&id) {
                            resident.attachment.resize_if_current(size, revision);
                        }
                    }
                });
            }));
        }
    }

    /// The top-left pane owns the native window-button lane when navigation
    /// chrome is hidden, including panes mounted by a saved split layout.
    fn occupies_window_titlebar(&self) -> bool {
        self.viewport
            .is_some_and(|viewport| viewport.x < 0.5 && viewport.y < 0.5)
    }

    fn shows_navigation_control(&self) -> bool {
        (matches!(self.session_source, SessionSource::FollowSelection) && !self.sidebar_visible)
            || self.occupies_window_titlebar()
    }

    fn render_sidebar_reveal_control(
        &self,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let horizontal = self
            .runtime
            .store
            .read()
            .expect("store")
            .preferences()
            .tab_orientation
            == crate::store::TabOrientation::Horizontal;
        div()
            .flex_none()
            .flex()
            .items_center()
            .gap(px(Metrics::TOOLBAR_ITEM_GAP))
            // The visible lights need more breathing room than their native
            // frames imply, so this is an intentional optical safe area.
            .when(self.occupies_window_titlebar(), |control| {
                control.child(div().w(px(Metrics::TOOLBAR_TRAFFIC_LIGHT_LANE)).flex_none())
            })
            .child(
                div()
                    .id("show-sidebar")
                    .debug_selector(|| "show-sidebar".into())
                    .role(gpui::Role::Button)
                    .aria_label(if horizontal {
                        "Toggle top bar"
                    } else {
                        "Show sidebar"
                    })
                    .size(px(Metrics::TOOLBAR_CONTROL_SIZE))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(Radius::BADGE))
                    .cursor_pointer()
                    .hover(move |button| button.bg(Fill::subtle(colors)))
                    .child(sf_symbol(
                        if horizontal {
                            "rectangle.topthird.inset.filled"
                        } else {
                            "sidebar.left"
                        },
                        15.0,
                        colors.secondary,
                    ))
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.focus(window, cx);
                        window.dispatch_action(Box::new(ToggleSidebar), cx);
                        cx.stop_propagation();
                    })),
            )
            .into_any_element()
    }

    fn render_header(
        &self,
        session: &SessionRecord,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let glyph = self.glyphs.get(&session.id).cloned();
        let identity_selector = format!("terminal-session-identity-{}", session.id.0);
        let shell_controls = matches!(self.session_source, SessionSource::FollowSelection);
        let show_sidebar = self.shows_navigation_control();
        let sidebar_reveal = show_sidebar.then(|| self.render_sidebar_reveal_control(colors, cx));
        let header_trailing_inset = self.header_trailing_inset;
        let header_width = self
            .viewport
            .map_or(f32::INFINITY, |viewport| viewport.width);
        div()
            .h(px(Metrics::TITLE_BAR))
            .flex_none()
            .pl(px(Metrics::TOOLBAR_EDGE_INSET))
            .pr(px(Metrics::TOOLBAR_EDGE_INSET + header_trailing_inset))
            .flex()
            .items_center()
            .justify_between()
            .bg(colors.work_surface_nested())
            .child(
                div()
                    .min_w(px(0.0))
                    .flex_1()
                    .flex()
                    .items_center()
                    .gap(px(Metrics::TOOLBAR_ITEM_GAP))
                    .overflow_hidden()
                    .when_some(sidebar_reveal, |title, control| title.child(control))
                    .when_some(glyph.filter(|_| header_width >= 280.0), |title, glyph| {
                        title.child(
                            div()
                                .debug_selector(move || identity_selector.clone())
                                .flex_none()
                                .flex()
                                .items_center()
                                .child(glyph),
                        )
                    })
                    .child(
                        div()
                            .min_w(px(0.0))
                            .flex_1()
                            .text_ellipsis()
                            .text_size(px(Typo::TITLE.size))
                            .font_weight(Typo::TITLE.weight)
                            .text_color(colors.primary)
                            .child(session.title.clone()),
                    )
                    .child(self.render_session_links_trigger(session, colors, cx)),
            )
            .child(
                div()
                    .flex_none()
                    .pl(px(if header_width < 420.0 {
                        4.0
                    } else {
                        Metrics::TOOLBAR_EDGE_INSET
                    }))
                    .flex()
                    .items_center()
                    .gap(px(Metrics::TOOLBAR_ITEM_GAP))
                    .when(shell_controls, |trailing| {
                        trailing
                            .child(self.render_inspector_toggle(colors, cx))
                            .child(self.render_notification_button(colors))
                    }),
            )
            .into_any_element()
    }

    fn render_inspector_toggle(
        &self,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let inspector_open = self.inspector_open;
        div()
            .id("toggle-inspector")
            .debug_selector(|| "toggle-inspector".into())
            .role(gpui::Role::Button)
            .aria_label("Toggle inspector")
            .size(px(Metrics::TOOLBAR_CONTROL_SIZE))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(Radius::BADGE))
            .cursor_pointer()
            .when(inspector_open, |button| button.bg(Fill::subtle(colors)))
            .hover(move |button| button.bg(Fill::subtle(colors)))
            .child(sf_symbol(
                "sidebar.right",
                15.0,
                if inspector_open {
                    colors.primary
                } else {
                    colors.secondary
                },
            ))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(|_, _, window, cx| {
                window.dispatch_action(Box::new(ToggleInspector), cx);
                cx.stop_propagation();
            }))
            .into_any_element()
    }

    fn render_notification_button(&self, colors: SemanticColors) -> AnyElement {
        let unread = self
            .runtime
            .store
            .read()
            .expect("session store lock poisoned")
            .notifications()
            .unread_count();
        div()
            .id("notification-inbox-button")
            .debug_selector(|| "notification-inbox-button".into())
            .role(gpui::Role::Button)
            .aria_label("Notifications")
            .relative()
            .size(px(Metrics::TOOLBAR_CONTROL_SIZE))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(Radius::BADGE))
            .cursor_pointer()
            .hover(move |button| button.bg(Fill::subtle(colors)))
            .child(sf_symbol(
                if unread > 0 { "bell.fill" } else { "bell" },
                14.0,
                if unread > 0 {
                    Ink::FRESH
                } else {
                    colors.secondary
                },
            ))
            .when(unread > 0, |button| {
                button.child(
                    div()
                        .absolute()
                        .top(px(2.0))
                        .right(px(2.0))
                        .size(px(5.0))
                        .rounded_full()
                        .bg(Ink::FRESH),
                )
            })
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(|_, window, cx| {
                window.dispatch_action(Box::new(crate::commands::ToggleNotifications), cx);
                cx.stop_propagation();
            })
            .into_any_element()
    }

    /// The title-bar actions for a workbench that paints them itself, in the
    /// horizontal tab strip beside the new-tab control. Only the pane that
    /// follows the selection owns shell-wide controls; a fixed pane hosts
    /// nothing. The links popover keeps its window-space anchor, so it still
    /// drops from wherever this trigger ends up.
    pub fn render_hosted_header_actions(
        &self,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !matches!(self.session_source, SessionSource::FollowSelection) {
            return None;
        }
        let session = self.selected_session();
        Some(
            div()
                .id("hosted-header-actions")
                .debug_selector(|| "hosted-header-actions".into())
                .flex_none()
                .flex()
                .items_center()
                .gap(px(Metrics::TOOLBAR_COMPACT_GAP))
                .when_some(session, |actions, session| {
                    actions.child(self.render_session_links_trigger(&session, colors, cx))
                })
                .child(self.render_inspector_toggle(colors, cx))
                .child(self.render_notification_button(colors))
                .into_any_element(),
        )
    }

    fn render_grid_and_overlays(
        &mut self,
        session: &SessionRecord,
        theme: TermTheme,
        colors: SemanticColors,
        font_size: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.reset_qol_session(&session.id);
        let hide_pointer = self
            .runtime
            .store
            .read()
            .expect("store")
            .preferences()
            .terminal_hide_pointer;
        if self.focus.is_focused(window) {
            cx.set_cursor_hide_mode(if hide_pointer {
                gpui::CursorHideMode::OnTyping
            } else {
                gpui::CursorHideMode::Never
            });
        }
        self.refresh_link_hover();
        if session.is_archived() {
            return self.render_archived_overlay(session, colors, cx);
        }
        let exited = matches!(session.status, SessionStatus::Exited(_));
        // An exited agent leaves its last screen behind in the daemon, and that
        // output is exactly what people want to read after closing an agent --
        // so only take the pane over when there is no terminal left to show.
        if exited && let Some(takeover) = self.render_exited_takeover(session, colors, cx) {
            return takeover;
        }
        self.probe_history_extent(&session.id);
        let Some(resident) = self.residents.get(&session.id) else {
            return centered_message("Preparing terminal…", "", colors).into_any_element();
        };
        let element = resident
            .element
            .clone()
            .theme(theme)
            // The surface around the grid paints the terminal tint, so
            // the grid only adds its own fill on an opaque window.
            .background_opacity(match colors.material() {
                diri_ui::Material::Opaque => 1.0,
                diri_ui::Material::Glass => 0.0,
            })
            .font_size(px(font_size))
            .focus_handle(self.focus.clone())
            .reduce_motion(cx.reduce_motion())
            .hovered_reference(self.qol.hit.clone());
        let element = if self.qol.copy_mode.is_some()
            || self.qol.paste.is_some()
            || self.qol.menu.is_some()
        {
            element.on_text_input(|_| {})
        } else {
            element
        };
        let view_offset = resident.element.view_offset();
        let attachment_state = resident.attachment_state;
        let show_attaching =
            attachment_state == AttachmentState::Attaching && !resident.element.has_content();
        let secret_input = resident.secret_input && attachment_state == AttachmentState::Live;
        let overflow = self.grid_row_overflow(resident.element.grid_rows(), font_size, window);
        let scroll_target = TerminalScrollTarget {
            element: resident.element.clone(),
            visible_rows: usize::from(resident.last_size.1.max(1)),
            line_height: f32::from(
                CellMetrics::measure(
                    window.text_system(),
                    &font(crate::fonts::mono_family()),
                    px(font_size),
                )
                .line_height,
            ),
            session: session.id.clone(),
            pane_tx: self.pane_tx.clone(),
        };

        let id_for_focus = session.id.clone();
        let follows_selection = matches!(self.session_source, SessionSource::FollowSelection);
        let mut body = div()
            .id("terminal-grid-surface")
            .debug_selector(|| "terminal-grid-surface".into())
            .relative()
            .flex_1()
            .overflow_hidden()
            .pt(px(2.0))
            .pb(px(10.0))
            .px(px(12.0))
            .cursor(if self.qol.hit.is_some() {
                gpui::CursorStyle::PointingHand
            } else {
                gpui::CursorStyle::IBeam
            })
            .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                if !hovered {
                    this.qol.hover = None;
                    this.qol.hit = None;
                    this.qol.hover_key_clear();
                    cx.notify();
                }
            }))
            .track_focus(&self.focus)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &gpui::MouseDownEvent, window, cx| {
                    this.focus(window, cx);
                    if follows_selection {
                        if let Some(store) = &this.window_store {
                            store.write().expect("store").select(id_for_focus.clone());
                        } else {
                            this.runtime
                                .store
                                .write()
                                .expect("store")
                                .select(id_for_focus.clone());
                        }
                    }
                    this.handle_pointer_down(event, window, cx);
                }),
            )
            .on_mouse_down(MouseButton::Middle, cx.listener(Self::handle_pointer_down))
            .on_mouse_down(MouseButton::Right, cx.listener(Self::handle_pointer_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::handle_pointer_up))
            .on_mouse_up(MouseButton::Middle, cx.listener(Self::handle_pointer_up))
            .on_mouse_up(MouseButton::Right, cx.listener(Self::handle_pointer_up))
            // A release outside the pane still belongs to the child that saw
            // the press. `grid_cell_at` clamps it to the nearest grid cell.
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::handle_pointer_up))
            .on_mouse_up_out(MouseButton::Middle, cx.listener(Self::handle_pointer_up))
            .on_mouse_up_out(MouseButton::Right, cx.listener(Self::handle_pointer_up))
            .on_mouse_move(cx.listener(Self::handle_pointer_move))
            .on_scroll_wheel(cx.listener(Self::handle_scroll))
            .child(
                diri_ui::scroll_area(
                    &self.scroller,
                    scroll_target,
                    colors,
                    match overflow {
                        // Settled: the mirrored screen fits, so the grid fills the pane
                        // exactly as before.
                        None => div().size_full().child(element),
                        // The daemon's screen is still taller than the pane -- a shrink
                        // that has not round-tripped yet. Give the grid its natural
                        // height, bottom-anchored: the extra rows clip off the top, the
                        // way a terminal drops scrollback, instead of the prompt and the
                        // agent's input box vanishing off the bottom until the reflow
                        // lands. Collapses back to the branch above on the next frame.
                        Some(grid_height) => div().size_full().relative().overflow_hidden().child(
                            div()
                                .absolute()
                                .bottom(px(0.0))
                                .left(px(0.0))
                                .right(px(0.0))
                                .h(px(grid_height))
                                .child(element),
                        ),
                    },
                )
                .size_full(),
            );

        // The exit pill owns the bottom slot; the transient pills stack above it.
        let pill_bottom = if exited { 52.0 } else { 18.0 };
        if view_offset > 0 {
            let return_id = session.id.clone();
            body = body.child(
                div()
                    .id("scrolled-pill")
                    .absolute()
                    .bottom(px(pill_bottom))
                    .left_1_2()
                    .ml(px(-90.0))
                    .rounded(px(999.0))
                    .px(px(12.0))
                    .py(px(6.0))
                    .bg(colors.floating_surface())
                    .border_1()
                    .border_color(colors.floating_stroke())
                    .text_size(px(11.5))
                    .text_color(colors.secondary)
                    .cursor_pointer()
                    .flex()
                    .items_center()
                    .gap(px(5.0))
                    .child(sf_symbol("arrow.down", 11.5, colors.secondary))
                    .child(format!("{view_offset} lines · Return to live"))
                    .on_click(cx.listener(move |this, _, _window, cx| {
                        this.return_to_live(&return_id, cx);
                    })),
            );
        }
        if show_attaching || attachment_state == AttachmentState::Reconnecting {
            let message = if attachment_state == AttachmentState::Reconnecting {
                "Reconnecting terminal…"
            } else {
                "Attaching…"
            };
            body = body.child(
                div()
                    .absolute()
                    .bottom(px(pill_bottom))
                    .left_1_2()
                    .ml(px(-72.0))
                    .rounded(px(999.0))
                    .px(px(12.0))
                    .py(px(6.0))
                    .bg(colors.floating_surface())
                    .border_1()
                    .border_color(colors.floating_stroke())
                    .text_size(px(11.5))
                    .text_color(colors.secondary)
                    .child(message),
            );
        }
        if exited {
            body = body.child(self.render_exit_pill(session, colors, cx));
        } else if secret_input {
            body = body.child(self.render_secret_input_badge(colors));
        }
        if let Some(status) = self.render_remote_connection(session, colors, cx) {
            body = body.child(status);
        }
        body.child(self.render_qol(colors, cx)).into_any_element()
    }

    /// Quiet lock in the corner away from the prompt line while the child
    /// reads a password. It names Secure Keyboard Entry only when this pane
    /// really holds it, which an unfocused pane, or a platform without the
    /// facility, does not.
    fn render_secret_input_badge(&self, colors: SemanticColors) -> AnyElement {
        let label = if self.secure_input.is_held() {
            "Secure input"
        } else {
            "Password prompt"
        };
        div()
            .debug_selector(|| "terminal-secret-input".into())
            .absolute()
            .top(px(8.0))
            .right(px(18.0))
            .rounded(px(999.0))
            .px(px(9.0))
            .py(px(4.0))
            .bg(colors.floating_surface())
            .border_1()
            .border_color(colors.floating_stroke())
            .flex()
            .items_center()
            .gap(px(5.0))
            .text_size(px(11.5))
            .text_color(colors.secondary)
            .child(sf_symbol("lock.fill", 11.0, colors.tertiary))
            .child(label)
            .into_any_element()
    }

    /// Slim status pill over an exited session's last screen: says what happened
    /// and offers the resume that the pane-filling card used to.
    fn render_exit_pill(
        &self,
        session: &SessionRecord,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = session.id.clone();
        let resumable = session.can_resume();
        let mut pill = div()
            .id("exit-pill")
            .rounded(px(999.0))
            .pl(px(12.0))
            .pr(if resumable { px(4.0) } else { px(12.0) })
            .py(px(4.0))
            .bg(colors.floating_surface())
            .border_1()
            .border_color(colors.floating_stroke())
            .flex()
            .items_center()
            .gap(px(8.0))
            .text_size(px(11.5))
            .text_color(colors.secondary)
            .child(sf_symbol("power", 11.0, colors.tertiary))
            .child(exit_description(session));
        if resumable {
            pill = pill.child(
                div()
                    .id("exit-pill-resume")
                    .rounded(px(999.0))
                    .px(px(9.0))
                    .py(px(3.0))
                    .bg(colors.primary.alpha(0.08))
                    .hover(move |style| style.bg(colors.primary.alpha(0.14)))
                    .cursor_pointer()
                    .text_color(colors.primary)
                    .child("Resume")
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.runtime
                            .store
                            .read()
                            .expect("session store lock poisoned")
                            .resume(id.clone());
                        cx.notify();
                    })),
            );
        } else if session.resumability == Resumability::TranscriptMissing {
            pill = pill.child(div().text_color(colors.tertiary).child("· transcript gone"));
        }
        // Centered by a full-width row rather than a guessed half-width offset,
        // since the description's length varies with the exit reason.
        div()
            .absolute()
            .bottom(px(18.0))
            .left_0()
            .right_0()
            .flex()
            .justify_center()
            .child(pill)
            .into_any_element()
    }

    fn render_find_bar(
        &self,
        session: &SessionRecord,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let resident = self.residents.get(&session.id)?;
        let find = resident.find.as_ref()?;
        let has_matches = !find.matches().is_empty();
        let count = if !has_matches {
            if find.query().is_empty() {
                String::new()
            } else {
                "No matches".to_owned()
            }
        } else {
            format!("{}/{}", find.current_index() + 1, find.matches().len())
        };
        let query = find_input::render(self, &session.id, colors, cx);
        let alt_screen = find.is_alt_screen();
        let search_status = find.error().map(str::to_owned).or_else(|| {
            if find.is_paused() {
                Some(
                    match (find.is_partial(), find.has_newer_output()) {
                        (true, true) => "Paused · Recent output · New output",
                        (true, false) => "Paused · Recent output",
                        (false, true) => "Paused · New output available",
                        (false, false) => "Paused",
                    }
                    .to_owned(),
                )
            } else if find.is_partial() {
                Some("Searching recent output".to_owned())
            } else {
                None
            }
        });
        let retained_search = find.uses_retained_capture();
        Some(find_overlay::render(
            resident.element.clone(),
            div()
                .id("find-bar")
                .debug_selector(|| "find-bar".into())
                .w_full()
                .child(FloatingSurface::new(
                    colors,
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(3.0))
                        .px(px(10.0))
                        .py(px(7.0))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(8.0))
                                .text_size(px(Typo::ROW.size))
                                .text_color(colors.primary)
                                .child(sf_symbol("magnifyingglass", 12.0, colors.tertiary))
                                .child(div().flex_1().min_w(px(0.0)).overflow_hidden().child(query))
                                .child(
                                    div()
                                        .text_size(px(Typo::META.size))
                                        .text_color(colors.tertiary)
                                        .child(count),
                                )
                                .child(div().w(px(1.0)).h(px(16.0)).bg(colors.primary.alpha(0.10)))
                                .child(find_icon_button(
                                    FindButtonSpec {
                                        id: "find-previous",
                                        system_image: "chevron.up",
                                        label: "Previous match",
                                        shortcut: "Shift+Enter",
                                    },
                                    colors,
                                    has_matches,
                                    cx,
                                    |this, _w, cx| {
                                        this.navigate_find(true, cx);
                                    },
                                ))
                                .child(find_icon_button(
                                    FindButtonSpec {
                                        id: "find-next",
                                        system_image: "chevron.down",
                                        label: "Next match",
                                        shortcut: "Enter",
                                    },
                                    colors,
                                    has_matches,
                                    cx,
                                    |this, _w, cx| {
                                        this.navigate_find(false, cx);
                                    },
                                ))
                                .when(retained_search, |row| {
                                    row.child(find_icon_button(
                                        FindButtonSpec {
                                            id: "find-refresh",
                                            system_image: "arrow.clockwise.circle",
                                            label: "Refresh results",
                                            shortcut: "",
                                        },
                                        colors,
                                        true,
                                        cx,
                                        |this, _window, cx| this.refresh_find(cx),
                                    ))
                                })
                                .child(find_icon_button(
                                    FindButtonSpec {
                                        id: "find-close",
                                        system_image: "xmark",
                                        label: "Close find",
                                        shortcut: "Escape",
                                    },
                                    colors,
                                    true,
                                    cx,
                                    |this, window, cx| {
                                        this.close_find_for_selected();
                                        find_input::discard_native(window, cx);
                                        cx.notify();
                                    },
                                )),
                        )
                        .when_some(search_status, |bar, status| {
                            bar.child(
                                div()
                                    .pl(px(20.0))
                                    .text_size(px(Typo::META.size))
                                    .text_color(colors.secondary)
                                    .child(status),
                            )
                        })
                        .when(alt_screen, |bar| {
                            bar.child(
                                div()
                                    .pl(px(20.0))
                                    .text_size(px(Typo::META.size))
                                    .text_color(colors.tertiary)
                                    .child("full-screen app — screen only"),
                            )
                        }),
                ))
                .into_any_element(),
        ))
    }

    /// The pane-filling card for an exited session, or `None` when the terminal
    /// itself should stay on screen (with [`Self::render_exit_pill`] over it).
    fn render_exited_takeover(
        &self,
        session: &SessionRecord,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let (auto_resuming, migrating) = {
            let store = self
                .runtime
                .store
                .read()
                .expect("session store lock poisoned");
            (
                store.auto_resuming().contains(&session.id),
                store.migrating().contains(&session.id),
            )
        };
        // Mid-migration the source agent is briefly down; show the busy state
        // instead of an exit card with a doomed Resume button.
        if migrating {
            return Some(centered_message("◌", "Moving session…", colors).into_any_element());
        }
        if auto_resuming {
            return Some(
                centered_message("◌", "Resuming conversation…", colors).into_any_element(),
            );
        }
        if self
            .residents
            .get(&session.id)
            .is_some_and(|resident| resident.element.has_content())
        {
            return None;
        }
        Some(self.render_exited_card(session, colors, cx))
    }

    fn render_exited_card(
        &self,
        session: &SessionRecord,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = session.id.clone();
        let content = centered_message("", &exit_description(session), colors);
        if session.can_resume() {
            content
                .child(primary_button(
                    "resume-conversation",
                    "Resume Conversation",
                    colors,
                    cx,
                    move |this, cx| {
                        this.runtime
                            .store
                            .read()
                            .expect("session store lock poisoned")
                            .resume(id.clone());
                        cx.notify();
                    },
                ))
                .into_any_element()
        } else if session.resumability == Resumability::TranscriptMissing {
            content
                .child(
                    div()
                        .text_size(px(11.5))
                        .text_color(colors.tertiary)
                        .child("Transcript is gone — start a fresh session in the same folder."),
                )
                .into_any_element()
        } else {
            content.into_any_element()
        }
    }

    fn render_archived_overlay(
        &self,
        session: &SessionRecord,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = session.id.clone();
        let mut content = centered_symbol_message("archivebox", 30.0, &session.title, colors)
            .child(
                div()
                    .text_size(px(13.0))
                    .text_color(colors.secondary)
                    .child("Archived"),
            );
        if session.resumability == Resumability::NotResumable {
            content = content.child(
                div()
                    .max_w(px(320.0))
                    .text_size(px(11.5))
                    .text_color(colors.tertiary)
                    .child(
                        "This session can't resume its conversation; revive restores it as ended.",
                    ),
            );
        }
        content
            .child(primary_button(
                "revive-session",
                "Revive Session",
                colors,
                cx,
                move |this, cx| {
                    this.runtime
                        .store
                        .write()
                        .expect("session store lock poisoned")
                        .revive_sessions(vec![id.clone()]);
                    this.reconcile_residency(cx);
                    cx.notify();
                },
            ))
            .into_any_element()
    }
}

fn quote_from_terminal_element(session_id: SessionId, element: &TerminalElement) -> Option<Quote> {
    let range = element.selection_range()?;
    Quote::new(
        QuoteSource::Terminal {
            session_id,
            start_row: range.start.row,
            end_row: range.end.row,
        },
        element.selected_text(),
    )
}

impl Render for TerminalPane {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        #[cfg(test)]
        {
            self.render_count += 1;
        }
        self.reconcile_residency(cx);
        if window.is_window_active() && self.focus.is_focused(window) {
            self.claim_selected_control();
        }
        self.reconcile_secure_input(window);
        if crate::alerts::enabled(cx) {
            self.sync_paste_prompt(window, cx);
        }
        let (theme, colors, sidebar_colors, font_size) = {
            let store = self
                .runtime
                .store
                .read()
                .expect("session store lock poisoned");
            (
                crate::app_theme::terminal_theme_in(&store),
                crate::app_theme::colors_in(&store),
                crate::app_theme::sidebar_colors_in(&store),
                store.preferences().terminal_font_size,
            )
        };
        self.sync_status_glyphs(colors, window, cx);
        self.update_selected_geometry(window, cx);
        self.main_viewport = window.viewport_size();

        let selected = self.selected_session();

        let content = if let Some(session) = selected {
            let mut pane = div()
                .relative()
                .flex()
                .flex_col()
                .flex_1()
                .h_full()
                .overflow_hidden()
                .border_l_1()
                .border_color(sidebar_colors.primary.alpha(0.08))
                .bg(colors.terminal_surface())
                .when(!self.header_hidden, |pane| {
                    pane.child(self.render_header(&session, sidebar_colors, cx))
                });
            let mut terminal_surface = div()
                .relative()
                .min_h(px(0.0))
                .flex_1()
                .flex()
                .flex_col()
                .when(!self.header_hidden, |surface| {
                    surface
                        .rounded_tl(px(Radius::CARD))
                        .rounded_tr(px(Radius::CARD))
                })
                .overflow_hidden()
                .bg(colors.work_surface_nested())
                .child(
                    self.render_grid_and_overlays(&session, theme, colors, font_size, window, cx),
                );
            if let Some(find) = self.render_find_bar(&session, colors, cx) {
                terminal_surface = terminal_surface.child(find);
            }
            pane = pane.child(terminal_surface);
            if let Some(summary) = self.render_session_links(&session, sidebar_colors, window, cx) {
                pane = pane.child(summary);
            }
            pane.into_any_element()
        } else {
            let show_sidebar = self.shows_navigation_control() && !self.header_hidden;
            let sidebar_reveal =
                show_sidebar.then(|| self.render_sidebar_reveal_control(sidebar_colors, cx));
            div()
                .flex_1()
                .h_full()
                .flex()
                .flex_col()
                .bg(colors.terminal_surface())
                .when_some(sidebar_reveal, |pane, control| {
                    pane.child(
                        div()
                            .h(px(Metrics::TITLE_BAR))
                            .flex_none()
                            .px(px(Metrics::TOOLBAR_EDGE_INSET))
                            .flex()
                            .items_center()
                            .bg(colors.work_surface_nested())
                            .child(control),
                    )
                })
                .child(self.render_empty_workbench(colors))
                .into_any_element()
        };

        let root_id = match &self.session_source {
            SessionSource::FollowSelection => SharedString::from("diri-terminal-root"),
            SessionSource::Fixed(id) => SharedString::from(format!("diri-terminal-root-{}", id.0)),
        };
        let has_resident = self
            .selected_id()
            .is_some_and(|id| self.residents.contains_key(&id));
        if !cx.has_active_drag() {
            self.external_drag.reset();
        }
        let drop_overlay =
            (has_resident && cx.has_active_drag()).then(|| self.external_drop_overlay(cx));
        div()
            .id(root_id)
            .key_context(TERMINAL_CONTEXT)
            .track_focus(&self.focus)
            .relative()
            .flex()
            .size_full()
            .text_color(colors.primary)
            .on_action(cx.listener(Self::open_find))
            .on_action(cx.listener(Self::find_next))
            .on_action(cx.listener(Self::find_previous))
            .on_action(cx.listener(Self::close_find))
            .on_action(cx.listener(Self::zoom_in))
            .on_action(cx.listener(Self::zoom_out))
            .on_action(cx.listener(Self::reset_zoom))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::copy_selection))
            .on_action(
                cx.listener(|this, _: &crate::commands::EnterCopyMode, window, cx| {
                    this.enter_copy_mode(window, cx);
                    cx.stop_propagation();
                }),
            )
            .on_action(
                cx.listener(|this, _: &crate::commands::FindSelection, window, cx| {
                    this.find_selection(window, cx);
                    cx.stop_propagation();
                }),
            )
            .on_action(
                cx.listener(|this, _: &crate::commands::ExportScrollback, window, cx| {
                    this.read_terminal_history(None, window, cx);
                    cx.stop_propagation();
                }),
            )
            .on_action(
                cx.listener(|this, _: &crate::commands::PreviousPrompt, window, cx| {
                    this.read_terminal_history(Some(false), window, cx);
                    cx.stop_propagation();
                }),
            )
            .on_action(
                cx.listener(|this, _: &crate::commands::NextPrompt, window, cx| {
                    this.read_terminal_history(Some(true), window, cx);
                    cx.stop_propagation();
                }),
            )
            .on_key_down(cx.listener(Self::handle_key_down))
            .on_key_up(cx.listener(Self::handle_key_up))
            .on_modifiers_changed(cx.listener(Self::handle_modifiers_changed))
            .child(content)
            .children(drop_overlay)
    }
}

fn clamp_grid_cell(col: usize, row: usize, cols: u16, rows: u16) -> Option<(u16, u16)> {
    if cols == 0 || rows == 0 {
        return None;
    }
    Some((
        u16::try_from(col).unwrap_or(u16::MAX).min(cols - 1),
        u16::try_from(row).unwrap_or(u16::MAX).min(rows - 1),
    ))
}

fn pointer_owner(
    mouse: MouseModes,
    button: MouseButton,
    modifiers: &gpui::Modifiers,
) -> PointerOwner {
    // `platform` is Command on the supported macOS desktop. Preserve local
    // reference resolution for that entire gesture, including its release.
    if modifiers.platform {
        return if button == MouseButton::Left {
            PointerOwner::LocalReference
        } else {
            PointerOwner::Ignored
        };
    }
    // Control-click opens references too. Only the left button is claimed so
    // Control with other buttons still reaches a mouse-reporting child.
    if modifiers.control && button == MouseButton::Left {
        return PointerOwner::LocalReference;
    }
    // Option must claim the press, not merely the first move; otherwise the
    // child would receive an unmatched press before a local selection began.
    if modifiers.alt {
        return if button == MouseButton::Left {
            PointerOwner::LocalSelection
        } else {
            PointerOwner::Ignored
        };
    }
    if mouse.is_reporting() && mouse.has_known_details() && terminal_mouse_button(button).is_some()
    {
        PointerOwner::Terminal
    } else if button == MouseButton::Left {
        PointerOwner::LocalSelection
    } else {
        PointerOwner::Ignored
    }
}

fn terminal_mouse_button(button: MouseButton) -> Option<TerminalMouseButton> {
    match button {
        MouseButton::Left => Some(TerminalMouseButton::Left),
        MouseButton::Middle => Some(TerminalMouseButton::Middle),
        MouseButton::Right => Some(TerminalMouseButton::Right),
        MouseButton::Navigate(_) => None,
    }
}

fn terminal_mouse_modifiers(modifiers: &gpui::Modifiers) -> TerminalMouseModifiers {
    TerminalMouseModifiers {
        shift: modifiers.shift,
        alt: modifiers.alt,
        control: modifiers.control,
    }
}

fn finish_pointer_state(
    pointer_owner: &mut Option<(MouseButton, PointerOwner)>,
    mouse_motion: &mut MouseMotionLimiter,
    button: MouseButton,
    cell_available: bool,
) -> (Option<PointerOwner>, Option<Vec<u8>>) {
    let owner = pointer_owner
        .take()
        .filter(|(owned, _)| *owned == button)
        .map(|(_, owner)| owner);
    let pending = if owner == Some(PointerOwner::Terminal) && cell_available {
        mouse_motion.take_pending()
    } else {
        mouse_motion.reset();
        None
    };
    (owner, pending)
}

#[derive(Clone, Copy)]
struct FindButtonSpec {
    id: &'static str,
    system_image: &'static str,
    label: &'static str,
    shortcut: &'static str,
}

fn find_icon_button(
    spec: FindButtonSpec,
    colors: SemanticColors,
    enabled: bool,
    cx: &mut Context<TerminalPane>,
    handler: impl Fn(&mut TerminalPane, &mut Window, &mut Context<TerminalPane>) + 'static,
) -> AnyElement {
    div()
        .id(spec.id)
        .debug_selector(move || spec.id.into())
        .size(px(28.0))
        .rounded(px(Radius::CHIP))
        .flex()
        .items_center()
        .justify_center()
        .role(Role::Button)
        .aria_label(spec.label)
        .aria_keyshortcuts(spec.shortcut)
        .aria_description(if enabled {
            "Activate this terminal find action"
        } else {
            "Unavailable because there are no matches"
        })
        .text_size(px(11.0))
        .text_color(if enabled {
            colors.secondary
        } else {
            colors.tertiary
        })
        .when(enabled, |button| {
            button
                .hover(move |style| style.bg(colors.primary.alpha(0.08)))
                .active(move |style| style.bg(colors.primary.alpha(0.12)))
                .cursor_pointer()
                .on_click(cx.listener(move |this, _, window, cx| handler(this, window, cx)))
        })
        .child(sf_symbol_weighted(
            spec.system_image,
            11.0,
            SymbolWeight::Semibold,
            if enabled {
                colors.secondary
            } else {
                colors.tertiary
            },
        ))
        .into_any_element()
}

fn primary_button(
    id: &'static str,
    label: &'static str,
    colors: SemanticColors,
    cx: &mut Context<TerminalPane>,
    handler: impl Fn(&mut TerminalPane, &mut Context<TerminalPane>) + 'static,
) -> AnyElement {
    div()
        .id(id)
        .mt(px(2.0))
        .rounded(px(7.0))
        .px(px(14.0))
        .py(px(7.0))
        .bg(colors.primary)
        .text_size(px(13.0))
        .font_weight(Typo::ROW_EMPHASIZED.weight)
        .text_color(colors.background)
        .hover(move |style| style.opacity(0.86))
        .active(move |style| style.opacity(0.72))
        .cursor_pointer()
        .child(label)
        .on_click(cx.listener(move |this, _, _, cx| handler(this, cx)))
        .into_any_element()
}

fn centered_message(icon: &str, message: &str, colors: SemanticColors) -> gpui::Div {
    div()
        .flex_1()
        .size_full()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(px(12.0))
        .when(!icon.is_empty(), |content| {
            content.child(
                div()
                    .text_size(px(30.0))
                    .text_color(colors.tertiary)
                    .child(icon.to_owned()),
            )
        })
        .when(!message.is_empty(), |content| {
            content.child(
                div()
                    .text_size(px(13.0))
                    .text_color(colors.secondary)
                    .child(message.to_owned()),
            )
        })
}

fn centered_symbol_message(
    system_image: &str,
    size: f32,
    message: &str,
    colors: SemanticColors,
) -> gpui::Div {
    div()
        .flex_1()
        .size_full()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(px(12.0))
        .child(sf_symbol_weighted(
            system_image,
            size,
            SymbolWeight::Regular,
            colors.tertiary,
        ))
        .when(!message.is_empty(), |content| {
            content.child(
                div()
                    .text_size(px(13.0))
                    .text_color(colors.secondary)
                    .child(message.to_owned()),
            )
        })
}

/// Readline-compatible line navigation. Other Command chords continue to the
/// app keymap; in particular, Ctrl-A is not a substitute for Select All.
fn terminal_command_navigation(key: &gpui::Keystroke) -> Option<&'static [u8]> {
    let modifiers = key.modifiers;
    if !modifiers.platform || modifiers.control || modifiers.alt || modifiers.shift {
        return None;
    }
    match key.key.as_str() {
        "left" => Some(b"\x01"),
        "right" => Some(b"\x05"),
        _ => None,
    }
}

fn terminal_key_event(event: &KeyDownEvent) -> Option<TermKeyEvent> {
    #[cfg(target_os = "macos")]
    if let Some(keypad) = crate::macos::terminal_keys::keypad_event(event) {
        return Some(keypad);
    }
    let named = match event.keystroke.key.as_str() {
        "up" => Some(NamedKey::ArrowUp),
        "down" => Some(NamedKey::ArrowDown),
        "right" => Some(NamedKey::ArrowRight),
        "left" => Some(NamedKey::ArrowLeft),
        "home" => Some(NamedKey::Home),
        "end" => Some(NamedKey::End),
        "pageup" => Some(NamedKey::PageUp),
        "pagedown" => Some(NamedKey::PageDown),
        "insert" => Some(NamedKey::Insert),
        "delete" => Some(NamedKey::Delete),
        "tab" => Some(NamedKey::Tab),
        "enter" => Some(NamedKey::Enter),
        "escape" => Some(NamedKey::Escape),
        "backspace" => Some(NamedKey::Backspace),
        "f1" => Some(NamedKey::F1),
        "f2" => Some(NamedKey::F2),
        "f3" => Some(NamedKey::F3),
        "f4" => Some(NamedKey::F4),
        "f5" => Some(NamedKey::F5),
        "f6" => Some(NamedKey::F6),
        "f7" => Some(NamedKey::F7),
        "f8" => Some(NamedKey::F8),
        "f9" => Some(NamedKey::F9),
        "f10" => Some(NamedKey::F10),
        "f11" => Some(NamedKey::F11),
        "f12" => Some(NamedKey::F12),
        _ => None,
    };
    if let Some(named) = named {
        return Some(TermKeyEvent::named(named));
    }
    let logical = event.keystroke.key.clone();
    let text = event
        .keystroke
        .key_char
        .clone()
        .unwrap_or_else(|| logical.clone());
    (!logical.is_empty()).then_some(TermKeyEvent {
        key: TermKey::Character(logical),
        text: Some(text),
    })
}

fn ui_agent_kind(kind: &ProtoAgentKind) -> UiAgentKind {
    // Brand vocabulary, not a protocol type: a manifest agent the client has
    // no hand-drawn mark for falls back to the generic terminal treatment.
    match kind.id() {
        ProtoAgentKind::CLAUDE_CODE_ID => UiAgentKind::ClaudeCode,
        ProtoAgentKind::CODEX_ID => UiAgentKind::Codex,
        ProtoAgentKind::CURSOR_ID => UiAgentKind::Cursor,
        ProtoAgentKind::GEMINI_ID => UiAgentKind::Gemini,
        ProtoAgentKind::SHELL_ID => UiAgentKind::Shell,
        _ => UiAgentKind::Generic,
    }
}

fn status_state(session: &SessionRecord) -> StatusState {
    if session.hibernation.is_some() {
        return StatusState::Hibernated;
    }
    match session.attention() {
        diri_proto::AttentionLevel::Working => StatusState::Working,
        diri_proto::AttentionLevel::NeedsInput => StatusState::NeedsInput {
            destructive: session
                .needs_input
                .as_ref()
                .is_some_and(|detail| detail.risk_hint == RiskHint::Destructive),
        },
        diri_proto::AttentionLevel::DoneUnseen => StatusState::DoneUnseen,
        diri_proto::AttentionLevel::IdleSeen => StatusState::IdleSeen,
        diri_proto::AttentionLevel::None | diri_proto::AttentionLevel::Unknown => StatusState::None,
    }
}

fn terminal_damage_should_repaint(
    selected: Option<&SessionId>,
    updated: &SessionId,
    changed: bool,
) -> bool {
    changed && selected == Some(updated)
}

/// What to do with a geometry change that just landed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResizePlan {
    /// Push it to the daemon now.
    SendNow,
    /// Hold it and arm a tick to send in this long.
    Arm(Duration),
    /// Hold it; a tick is already armed and will carry it.
    Fold,
}

/// Decides whether a geometry change goes out now or rides the next cadence
/// tick. Pure, and deliberately named: the version this replaced looked correct
/// but rescheduled its timer on every frame, so a smooth drag cancelled its own
/// flush forever and the PTY only ever heard the size the mouse stopped at.
fn plan_resize(first_measure: bool, since_sent: Option<Duration>, armed: bool) -> ResizePlan {
    // The first measure after attach is what a deferred agent launch waits for,
    // and an isolated change (session switch, window snap, the opening frame of
    // a drag) should feel instant -- neither may wait on the cadence.
    if first_measure || since_sent.is_none_or(|since| since >= RESIZE_CADENCE) {
        return ResizePlan::SendNow;
    }
    if armed {
        return ResizePlan::Fold;
    }
    ResizePlan::Arm(RESIZE_CADENCE.saturating_sub(since_sent.unwrap_or_default()))
}

/// Columns and rows of a parked grid, when the engine has already published a size.
fn parked_grid_size(parked: Option<&SharedGridBuffer>) -> Option<(u16, u16)> {
    let grid = parked?.read().ok()?;
    (grid.cols > 0 && grid.rows > 0).then_some((grid.cols, grid.rows))
}

/// Whether a geometry change should hold the grid still while it round-trips.
/// Pure so the three conditions stay stated rather than implied:
///
/// - a first measure has nothing on screen to hold;
/// - only a column change reflows, and it is the reflow that moves content
///   vertically -- a rows-only change crops or extends the grid, which the
///   bottom-anchor path already covers;
/// - a drag steps faster than [`RESIZE_GESTURE_GAP`] and has to keep reflowing
///   under the cursor, so only a discrete change holds.
fn should_hold_reflow(
    previous: (u16, u16),
    next: (u16, u16),
    since_sent: Option<Duration>,
) -> bool {
    previous != (0, 0)
        && previous.0 != next.0
        && since_sent.is_none_or(|since| since >= RESIZE_GESTURE_GAP)
}

/// The current window-space estimate used for PTY sizing. Keeping this
/// calculation named makes the protocol-vs-painted-width invariant directly
/// testable: the daemon must never receive more columns than the grid element
/// can actually paint after layout chrome is applied.
fn estimated_grid_size(
    window_width: f32,
    window_height: f32,
    header_height: f32,
    chrome_inset: f32,
    metrics: CellMetrics,
) -> (u16, u16) {
    let width = px((window_width
        - chrome_inset
        - GRID_HORIZONTAL_PADDING
        - GRID_LAYOUT_HORIZONTAL_CHROME)
        .max(1.0));
    let height = px((window_height
        - header_height
        - GRID_VERTICAL_PADDING
        - GRID_LAYOUT_VERTICAL_CHROME)
        .max(1.0));
    (
        metrics.cols_for_width(width).max(2),
        metrics.rows_for_height(height).max(2),
    )
}

fn clipboard_image(item: &ClipboardItem) -> Option<(&[u8], &'static str)> {
    item.entries().iter().find_map(|entry| match entry {
        ClipboardEntry::Image(image) => Some((image.bytes.as_slice(), image.format.extension())),
        ClipboardEntry::String(_) | ClipboardEntry::ExternalPaths(_) => None,
    })
}

fn exit_description(session: &SessionRecord) -> String {
    let SessionStatus::Exited(info) = &session.status else {
        return "Session ended".to_owned();
    };
    match info.reason {
        ExitReason::DaemonRestart => "Session ended when the daemon restarted".to_owned(),
        ExitReason::Signaled => "Agent was stopped".to_owned(),
        ExitReason::Exited if info.code == Some(0) => "Agent exited".to_owned(),
        ExitReason::Exited => format!("Agent exited (code {})", info.code.unwrap_or(-1)),
        ExitReason::External => "Imported session — not started yet".to_owned(),
        ExitReason::Archived => "Archived".to_owned(),
        ExitReason::Unknown => "Session ended".to_owned(),
    }
}

/// Lets the shared scroller drive the terminal's scrollback viewport, which
/// counts lines from the live edge rather than pixels from the top.
struct TerminalScrollTarget {
    element: TerminalElement,
    visible_rows: usize,
    line_height: f32,
    session: SessionId,
    pane_tx: PaneEventSender,
}

impl TerminalScrollTarget {
    fn max_lines(&self) -> i64 {
        if self.element.alt_screen() {
            0
        } else {
            // The indicator's range, not the navigable one: following live,
            // the viewport can only navigate a guessed screen of history.
            self.element.indicator_max_view_offset(self.visible_rows)
        }
    }
}

impl diri_ui::ScrollTarget for TerminalScrollTarget {
    fn offset(&self) -> gpui::Point<gpui::Pixels> {
        // Fractional, so the knob glides with a trackpad gesture and the top
        // only reports itself reached once the last partial row is.
        let max = self.max_lines() as f64;
        let scrolled_up = self.element.scroll_position().min(max);
        let from_top = (max - scrolled_up) as f32 * self.line_height;
        gpui::point(px(0.0), px(-from_top))
    }

    fn max_offset(&self) -> gpui::Point<gpui::Pixels> {
        gpui::point(px(0.0), px(self.max_lines() as f32 * self.line_height))
    }

    fn set_offset(&self, offset: gpui::Point<gpui::Pixels>, _: &mut Window, _: &mut gpui::App) {
        let from_top = f64::from(-f32::from(offset.y) / self.line_height);
        let target = (self.max_lines() as f64 - from_top).max(0.0);
        if self.element.set_scroll_position(target, self.visible_rows) {
            let _ = self.pane_tx.send(PaneEvent::ScrollbackPump(
                self.session.clone(),
                self.visible_rows,
            ));
        }
    }

    /// The top of history gives; the live edge is where output lands and
    /// must stay put.
    fn bounce_edges(&self) -> (bool, bool) {
        (true, false)
    }
}

#[cfg(test)]
mod tests {
    use diri_proto::grid::{ChangedRow, GridCell, TermColor, TermStyle};
    use diri_proto::{
        DateMillis, ExitInfo, NeedsInputDetail, NeedsInputKind, NeedsInputSource, SessionListResult,
    };
    use diri_proto::{PrCheck, PullRequestStatus};
    use gpui::{Image, ImageFormat, KeyDownEvent, Keystroke, Modifiers, TestAppContext};

    use super::*;

    fn due_find_request(
        model: &mut TerminalFindModel,
        query: &str,
        now: Duration,
    ) -> SearchRequest {
        model.set_query(query, now);
        model
            .take_due_search(now + diri_term::find::SEARCH_DEBOUNCE)
            .expect("find request should be due")
    }

    fn find_snapshot(content_seq: u64) -> FindSnapshot {
        FindSnapshot {
            error: None,
            retained: None,
            text_cells: Default::default(),
            lines: Vec::new(),
            first_row: 0,
            visible_start_row: 0,
            cols: 8,
            rows: 1,
            content_seq,
            is_alt_screen: false,
        }
    }

    fn find_result(model: &TerminalFindModel, request: &SearchRequest) -> SearchResult {
        let mut live = GridBuffer::new(8, 1);
        for (index, ch) in "needle".chars().enumerate() {
            live.cells[index] = GridCell::new(
                u32::from(ch),
                TermColor::Default,
                TermColor::DefaultInverted,
                TermStyle::empty(),
            );
        }
        model
            .prepare_search(request, find_snapshot(1), &live)
            .expect("search job")
            .run()
    }

    fn fill_semantic_mailbox(sender: &PaneEventSender) {
        for _ in 0..PANE_EVENT_QUEUE_CAPACITY {
            assert!(
                sender
                    .send(PaneEvent::ScrollbackFailed(SessionId::new("pressure"), 0))
                    .is_ok()
            );
        }
    }

    #[tokio::test]
    async fn pane_mailbox_retains_one_exact_final_grid_per_session() {
        let (sender, mut receiver) = pane_event_channel();
        let id = SessionId::new("mailbox");
        let generation = 7;
        let mut first_cells = vec![GridCell::BLANK; 2];
        first_cells[0].scalar = u32::from('a');
        let first = GridUpdate {
            cols: 2,
            rows: 1,
            cursor_col: 1,
            cursor_row: 0,
            cursor_visible: true,
            is_full_snapshot: true,
            changed_rows: vec![ChangedRow::new(0, first_cells)],
        };
        let mut final_cells = vec![GridCell::BLANK; 2];
        final_cells[1].scalar = u32::from('b');
        let second = GridUpdate {
            cols: 2,
            rows: 1,
            cursor_col: 0,
            cursor_row: 0,
            cursor_visible: false,
            is_full_snapshot: false,
            changed_rows: vec![ChangedRow::new(0, final_cells.clone())],
        };

        assert!(
            sender
                .send(PaneEvent::Chunk(
                    id.clone(),
                    generation,
                    TerminalChunk::Grid(first),
                ))
                .is_ok()
        );
        assert!(
            sender
                .send(PaneEvent::Chunk(
                    id.clone(),
                    generation,
                    TerminalChunk::Grid(second),
                ))
                .is_ok()
        );

        let mut batch = Vec::new();
        assert!(receiver.recv_batch(&mut batch).await);
        assert_eq!(batch.len(), 1);
        let PaneEvent::GridBatch(batch_id, batch_generation, updates) =
            batch.pop().expect("grid batch")
        else {
            panic!("mailbox did not return a grid batch");
        };
        assert_eq!(batch_id, id);
        assert_eq!(batch_generation, generation);
        assert_eq!(updates.len(), 2, "the post-snapshot boundary is retained");
        let mut applied = Vec::new();
        for update in &updates {
            update.apply(&mut applied);
        }
        assert_eq!(applied, final_cells);
        assert_eq!(updates.last().expect("final update").cursor_col, 0);
        assert!(!updates.last().expect("final update").cursor_visible);
    }

    #[tokio::test]
    async fn pane_mailbox_replaces_a_stale_attachment_grid_with_the_new_generation() {
        let (sender, mut receiver) = pane_event_channel();
        let id = SessionId::new("reselected");

        for (generation, character) in [(3, 'o'), (4, 'n'), (3, 's')] {
            assert!(
                sender
                    .send(PaneEvent::Chunk(
                        id.clone(),
                        generation,
                        TerminalChunk::Grid(filled_grid(character)),
                    ))
                    .is_ok()
            );
        }

        let mut batch = Vec::new();
        assert!(receiver.recv_batch(&mut batch).await);
        assert_eq!(batch.len(), 1);
        let PaneEvent::GridBatch(batch_id, generation, updates) = batch.pop().expect("grid batch")
        else {
            panic!("mailbox did not return a grid batch");
        };
        assert_eq!(batch_id, id);
        assert_eq!(generation, 4);
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].changed_rows[0].cells[0].scalar, u32::from('n'));
    }

    #[tokio::test]
    async fn full_semantic_mailbox_cannot_strand_a_find_read() {
        let (sender, mut receiver) = pane_event_channel();
        let id = SessionId::new("find-read-pressure");
        let generation = 7;
        let mut model = TerminalFindModel::default();
        let first = due_find_request(&mut model, "first", Duration::ZERO);
        let latest = due_find_request(&mut model, "latest", Duration::from_secs(1));
        let mut scheduler = FindSearchScheduler::default();
        assert_eq!(scheduler.schedule(first.clone()), Some(first.clone()));
        assert_eq!(scheduler.schedule(latest.clone()), None);

        fill_semantic_mailbox(&sender);
        assert!(
            sender
                .send(PaneEvent::FindSnapshot(
                    id,
                    generation,
                    first.clone(),
                    Some(find_snapshot(1)),
                ))
                .is_ok(),
            "find completion must not share rejectable semantic capacity"
        );

        let mut batch = Vec::new();
        assert!(receiver.recv_batch(&mut batch).await);
        let delivered = batch.into_iter().find_map(|event| match event {
            PaneEvent::FindSnapshot(_, _, request, snapshot) => Some((request, snapshot)),
            _ => None,
        });
        let (delivered_request, snapshot) = delivered.expect("guaranteed read completion");
        assert!(snapshot.is_some());
        assert_eq!(delivered_request, first);
        assert_eq!(
            scheduler.finish_read(&delivered_request, true),
            ReadCompletion::Read(latest),
            "the latest search must run after the pressured read completes"
        );
    }

    #[tokio::test]
    async fn full_semantic_mailbox_cannot_strand_a_find_scan() {
        let (sender, mut receiver) = pane_event_channel();
        let id = SessionId::new("find-scan-pressure");
        let generation = 9;
        let mut model = TerminalFindModel::default();
        let first = due_find_request(&mut model, "needle", Duration::ZERO);
        let result = find_result(&model, &first);
        let latest = due_find_request(&mut model, "latest", Duration::from_secs(1));
        let mut scheduler = FindSearchScheduler::default();
        assert_eq!(scheduler.schedule(first.clone()), Some(first.clone()));
        assert_eq!(scheduler.finish_read(&first, true), ReadCompletion::Scan);
        assert_eq!(scheduler.schedule(latest.clone()), None);

        fill_semantic_mailbox(&sender);
        assert!(
            sender
                .send(PaneEvent::FindResult(id, generation, first.clone(), result))
                .is_ok(),
            "find result must not share rejectable semantic capacity"
        );

        let mut batch = Vec::new();
        assert!(receiver.recv_batch(&mut batch).await);
        let delivered_request = batch.into_iter().find_map(|event| match event {
            PaneEvent::FindResult(_, _, request, _) => Some(request),
            _ => None,
        });
        let delivered_request = delivered_request.expect("guaranteed scan completion");
        let completion = scheduler
            .finish_scan(&delivered_request)
            .expect("active scan completion");
        assert_eq!(
            completion.into_next_request(),
            Some(latest),
            "the latest search must run after the pressured scan completes"
        );
    }

    #[tokio::test]
    async fn pane_mailbox_delivers_grid_damage_before_an_older_find_result() {
        let (sender, mut receiver) = pane_event_channel();
        let id = SessionId::new("grid-before-find");
        let generation = 11;
        let mut model = TerminalFindModel::default();
        let request = due_find_request(&mut model, "needle", Duration::ZERO);
        let result = find_result(&model, &request);
        let mut scheduler = FindSearchScheduler::default();
        assert_eq!(scheduler.schedule(request.clone()), Some(request.clone()));
        assert_eq!(scheduler.finish_read(&request, true), ReadCompletion::Scan);

        // The result reaches the mailbox first, but the grid is newer content
        // already waiting in the same GPUI wake.
        assert!(
            sender
                .send(PaneEvent::FindResult(
                    id.clone(),
                    generation,
                    request.clone(),
                    result,
                ))
                .is_ok()
        );
        assert!(
            sender
                .send(PaneEvent::Chunk(
                    id,
                    generation,
                    TerminalChunk::Grid(filled_grid('n')),
                ))
                .is_ok()
        );

        let mut batch = Vec::new();
        assert!(receiver.recv_batch(&mut batch).await);
        assert!(matches!(batch.first(), Some(PaneEvent::GridBatch(..))));
        assert!(matches!(batch.last(), Some(PaneEvent::FindResult(..))));

        let mut viewport = diri_term::scrollback::ScrollbackViewport::default();
        for event in batch {
            match event {
                PaneEvent::GridBatch(..) => {
                    assert!(model.on_output(Duration::from_secs(1)));
                }
                PaneEvent::FindResult(_, _, delivered, result) => {
                    assert!(scheduler.finish_scan(&delivered).is_some());
                    assert!(
                        !model.apply_result(result, &mut viewport),
                        "queued newer grid must invalidate the older result before apply"
                    );
                }
                _ => {}
            }
        }
        assert!(model.matches().is_empty());
    }

    /// Replays a drag as the render loop sees it -- a geometry change every
    /// `frame`, for `frames` frames -- and returns when each size reached the
    /// daemon. Mirrors `update_selected_geometry`: `Arm`/`Fold` hold the size,
    /// and an armed tick fires on the cadence carrying the newest one.
    fn simulate_drag(frames: u32, frame: Duration) -> Vec<Duration> {
        let mut sent = Vec::new();
        let mut last_sent: Option<Duration> = None;
        let mut armed_at: Option<Duration> = None;
        let mut now = Duration::ZERO;
        for tick in 0..frames {
            now += frame;
            // The armed tick fires on its own, independent of the frame.
            if let Some(at) = armed_at
                && now >= at
            {
                sent.push(at);
                last_sent = Some(at);
                armed_at = None;
            }
            let since = last_sent.map(|at| now.saturating_sub(at));
            match plan_resize(tick == 0, since, armed_at.is_some()) {
                ResizePlan::SendNow => {
                    sent.push(now);
                    last_sent = Some(now);
                }
                ResizePlan::Arm(delay) => armed_at = Some(now + delay),
                ResizePlan::Fold => {}
            }
        }
        if let Some(at) = armed_at {
            sent.push(at);
        }
        sent
    }

    #[test]
    fn pointer_ownership_preserves_local_escape_hatches_and_reporting_off() {
        let plain = Modifiers::default();
        let option = Modifiers {
            alt: true,
            ..Modifiers::default()
        };
        let command = Modifiers {
            platform: true,
            ..Modifiers::default()
        };
        let control = Modifiers {
            control: true,
            ..Modifiers::default()
        };
        let reporting = MouseModes::new(
            diri_proto::terminal::MouseTrackingMode::AnyMotion,
            diri_proto::terminal::MouseEncoding::Sgr,
        );

        assert_eq!(
            pointer_owner(MouseModes::OFF, MouseButton::Left, &plain),
            PointerOwner::LocalSelection
        );
        assert_eq!(
            pointer_owner(MouseModes::OFF, MouseButton::Right, &plain),
            PointerOwner::Ignored,
            "reporting-off right-click behavior stays unchanged"
        );
        assert_eq!(
            pointer_owner(reporting, MouseButton::Left, &plain),
            PointerOwner::Terminal
        );
        assert_eq!(
            pointer_owner(reporting, MouseButton::Left, &option),
            PointerOwner::LocalSelection,
            "Option claims the whole drag before a press can reach the PTY"
        );
        assert_eq!(
            pointer_owner(reporting, MouseButton::Left, &command),
            PointerOwner::LocalReference,
            "Command-click remains local"
        );
        assert_eq!(
            pointer_owner(reporting, MouseButton::Right, &command),
            PointerOwner::Ignored,
            "no Command-modified button is forwarded"
        );
        assert_eq!(
            pointer_owner(reporting, MouseButton::Left, &control),
            PointerOwner::LocalReference,
            "Control-click opens references like Command-click"
        );
        assert_eq!(
            pointer_owner(MouseModes::OFF, MouseButton::Left, &control),
            PointerOwner::LocalReference
        );
        assert_eq!(
            pointer_owner(reporting, MouseButton::Right, &control),
            PointerOwner::Terminal,
            "Control with other buttons still reaches the child"
        );
        assert_eq!(
            pointer_owner(MouseModes::UNKNOWN, MouseButton::Left, &plain),
            PointerOwner::LocalSelection,
            "an old remote Holder must not receive a guessed click encoding"
        );
    }

    #[test]
    fn option_drag_still_produces_copyable_terminal_text() {
        let element = TerminalElement::with_buffer(GridBuffer::default());
        let mut cells: Vec<_> = "copy me"
            .chars()
            .map(|character| diri_proto::grid::GridCell {
                scalar: u32::from(character),
                ..diri_proto::grid::GridCell::BLANK
            })
            .collect();
        cells.push(diri_proto::grid::GridCell::BLANK);
        element.apply_damage(GridUpdate {
            cols: 8,
            rows: 1,
            cursor_col: 0,
            cursor_row: 0,
            cursor_visible: true,
            is_full_snapshot: true,
            changed_rows: vec![diri_proto::grid::ChangedRow::new(0, cells)],
        });
        let option = Modifiers {
            alt: true,
            ..Modifiers::default()
        };
        let reporting = MouseModes::new(
            diri_proto::terminal::MouseTrackingMode::ButtonMotion,
            diri_proto::terminal::MouseEncoding::Sgr,
        );
        assert_eq!(
            pointer_owner(reporting, MouseButton::Left, &option),
            PointerOwner::LocalSelection
        );
        element.begin_selection(0, 0);
        element.drag_selection(7, 0);
        assert_eq!(element.selected_text(), "copy me");
    }

    #[test]
    fn pointer_coordinates_clamp_to_every_grid_edge() {
        assert_eq!(clamp_grid_cell(5, 7, 80, 24), Some((5, 7)));
        assert_eq!(clamp_grid_cell(usize::MAX, 100, 80, 24), Some((79, 23)));
        assert_eq!(clamp_grid_cell(0, 0, 0, 24), None);
        assert_eq!(clamp_grid_cell(0, 0, 80, 0), None);
    }

    #[test]
    fn unrestricted_motion_coalesces_to_the_latest_cell_at_the_trailing_edge() {
        let started = Instant::now();
        let mut limiter = MouseMotionLimiter::default();
        assert_eq!(
            limiter.push(started, (1, 1), b"one".to_vec()),
            MotionDispatch::SendNow(b"one".to_vec())
        );
        let MotionDispatch::Schedule { generation, .. } =
            limiter.push(started + Duration::from_millis(1), (2, 1), b"two".to_vec())
        else {
            panic!("second cell should arm the trailing edge");
        };
        assert_eq!(
            limiter.push(
                started + Duration::from_millis(2),
                (3, 1),
                b"three".to_vec(),
            ),
            MotionDispatch::None,
            "the armed timer folds newer cells"
        );
        assert_eq!(
            limiter.flush(generation, started + MOUSE_MOTION_CADENCE),
            Some(b"three".to_vec()),
            "the destination is not dropped when pointer events stop"
        );
        assert_eq!(limiter.flush(generation, started), None);
        assert_eq!(
            limiter.push(
                started + MOUSE_MOTION_CADENCE,
                (3, 1),
                b"duplicate".to_vec(),
            ),
            MotionDispatch::None
        );
    }

    #[test]
    fn a_pending_drag_is_drained_before_release_and_its_timer_is_cancelled() {
        let started = Instant::now();
        let mut limiter = MouseMotionLimiter::default();
        assert!(matches!(
            limiter.push(started, (1, 1), b"first".to_vec()),
            MotionDispatch::SendNow(_)
        ));
        let MotionDispatch::Schedule { generation, .. } = limiter.push(
            started + Duration::from_millis(1),
            (2, 1),
            b"pending-before-release".to_vec(),
        ) else {
            panic!("pending motion");
        };
        assert_eq!(
            limiter.take_pending(),
            Some(b"pending-before-release".to_vec())
        );
        assert_eq!(
            limiter.flush(generation, started + MOUSE_MOTION_CADENCE),
            None,
            "no motion may be emitted after the release"
        );
    }

    #[test]
    fn release_without_a_grid_cell_cancels_the_gesture_and_pending_timer() {
        let started = Instant::now();
        let mut limiter = MouseMotionLimiter::default();
        assert!(matches!(
            limiter.push(started, (1, 1), b"first".to_vec()),
            MotionDispatch::SendNow(_)
        ));
        let MotionDispatch::Schedule { generation, .. } = limiter.push(
            started + Duration::from_millis(1),
            (2, 1),
            b"pending".to_vec(),
        ) else {
            panic!("pending motion");
        };
        let mut owner = Some((MouseButton::Left, PointerOwner::Terminal));
        assert_eq!(
            finish_pointer_state(&mut owner, &mut limiter, MouseButton::Left, false),
            (Some(PointerOwner::Terminal), None)
        );
        assert_eq!(owner, None);
        assert_eq!(
            limiter.flush(generation, started + MOUSE_MOTION_CADENCE),
            None,
            "a stale timer cannot send motion after the physical release"
        );
    }

    #[test]
    fn a_live_drag_keeps_resizing_the_pty_at_the_cadence() {
        // Roughly one second of dragging at 120Hz. The trailing-edge debounce this
        // replaced sent exactly one resize here -- after the mouse stopped --
        // which is why the terminal appeared to reflow only on drop. The
        // expected count derives from the cadence so it moves with it.
        let sent = simulate_drag(120, Duration::from_millis(8));
        let expected =
            (120 * Duration::from_millis(8).as_millis() / RESIZE_CADENCE.as_millis()) as usize;
        assert!(
            sent.len().abs_diff(expected) <= 3,
            "expected ~{expected} resizes in a second of dragging, got {}",
            sent.len()
        );
        // Leading edge: the drag's first frame is not made to wait.
        assert_eq!(sent[0], Duration::from_millis(8));
        // And no two land closer together than the cadence.
        for pair in sent.windows(2) {
            assert!(
                pair[1].saturating_sub(pair[0]) >= RESIZE_CADENCE,
                "{pair:?} are closer than the cadence"
            );
        }
    }

    #[test]
    fn the_size_a_drag_ends_on_always_reaches_the_daemon() {
        // Three frames then release: the last size must still go out, or the
        // pane keeps painting a grid the daemon has never been told about.
        let sent = simulate_drag(3, Duration::from_millis(8));
        assert!(sent.len() >= 2, "the release size must be sent: {sent:?}");
        let release = Duration::from_millis(3 * 8);
        assert!(
            *sent.last().expect("sent") <= release + RESIZE_CADENCE,
            "the final size lands within one cadence of release: {sent:?}"
        );
    }

    #[test]
    fn an_isolated_resize_never_waits() {
        // A window snap or a session switch is one change after a long idle.
        assert_eq!(
            plan_resize(false, Some(Duration::from_secs(3)), false),
            ResizePlan::SendNow
        );
        assert_eq!(plan_resize(false, None, false), ResizePlan::SendNow);
        // The first measure after attach is what a deferred launch waits for.
        assert_eq!(
            plan_resize(true, Some(Duration::ZERO), true),
            ResizePlan::SendNow
        );
    }

    #[test]
    fn terminal_element_selection_becomes_a_session_provenance_quote() {
        let mut buffer = GridBuffer::new(8, 1);
        for (index, character) in "hello".chars().enumerate() {
            buffer.cells[index] = GridCell::new(
                u32::from(character),
                TermColor::Default,
                TermColor::DefaultInverted,
                TermStyle::empty(),
            );
        }
        let element = TerminalElement::with_buffer(buffer);
        element.begin_selection(0, 0);
        element.drag_selection(5, 0);
        let quote = quote_from_terminal_element(SessionId::new("source-terminal"), &element)
            .expect("terminal quote");
        assert_eq!(quote.content, "hello");
        assert_eq!(
            quote.source,
            QuoteSource::Terminal {
                session_id: SessionId::new("source-terminal"),
                start_row: 0,
                end_row: 0,
            }
        );
    }

    fn grid_frame(cols: u16, full: bool) -> GridUpdate {
        GridUpdate {
            cols,
            rows: 40,
            cursor_col: 0,
            cursor_row: 0,
            cursor_visible: true,
            is_full_snapshot: full,
            changed_rows: Vec::new(),
        }
    }

    fn filled_grid(character: char) -> GridUpdate {
        const COLS: u16 = 8;
        const ROWS: u16 = 4;
        let mut cell = GridCell::BLANK;
        cell.scalar = u32::from(character);
        GridUpdate {
            cols: COLS,
            rows: ROWS,
            cursor_col: 0,
            cursor_row: ROWS - 1,
            cursor_visible: true,
            is_full_snapshot: true,
            changed_rows: (0..ROWS)
                .map(|row| ChangedRow::new(row, vec![cell; usize::from(COLS)]))
                .collect(),
        }
    }

    fn reflow_hold() -> ReflowHold {
        ReflowHold {
            parked: Vec::new(),
            saw_snapshot: false,
            _release: Task::ready(()),
        }
    }

    #[test]
    fn a_panel_toggle_holds_the_grid_but_a_drag_keeps_reflowing() {
        // ⌘B after any pause: one column change, held so the re-wrap and the
        // program's repaint land together.
        assert!(should_hold_reflow(
            (120, 40),
            (100, 40),
            Some(Duration::from_secs(3))
        ));
        // A drag steps every few frames; freezing it would stop the grid from
        // reflowing under the cursor, which is the whole point of the cadence.
        assert!(!should_hold_reflow(
            (120, 40),
            (119, 40),
            Some(Duration::from_millis(16))
        ));
    }

    #[test]
    fn a_change_with_no_reflow_in_it_is_never_held() {
        // Rows-only: the daemon crops or extends, nothing re-wraps.
        assert!(!should_hold_reflow((120, 40), (120, 30), None));
        // The first measure after attach has nothing on screen to hold.
        assert!(!should_hold_reflow((0, 0), (120, 40), None));
    }

    #[test]
    fn a_hold_ends_on_the_repaint_that_follows_the_re_wrap() {
        let mut hold = reflow_hold();
        // The daemon's re-wrapped snapshot: on its own this is the frame that
        // used to shove the content up, so it must not release the hold.
        assert!(!hold.park(grid_frame(100, true)));
        // The program answering SIGWINCH completes the pair.
        assert!(hold.park(grid_frame(100, false)));
        assert_eq!(hold.parked.len(), 2);
    }

    #[test]
    fn a_re_seed_mid_hold_does_not_stand_in_for_the_repaint() {
        let mut hold = reflow_hold();
        assert!(!hold.park(grid_frame(100, true)));
        assert!(!hold.park(grid_frame(100, true)));
        assert!(hold.park(grid_frame(100, false)));
    }

    #[test]
    fn a_repaint_arriving_before_any_snapshot_keeps_waiting() {
        // Output already in flight when the resize went out is not the answer
        // to it; releasing on it would paint the pre-reflow grid.
        let mut hold = reflow_hold();
        assert!(!hold.park(grid_frame(120, false)));
    }

    pub(super) fn fixture_session() -> SessionRecord {
        let envelope: serde_json::Value = serde_json::from_str(include_str!(
            "../../diri-proto/tests/fixtures/session_list_response.json"
        ))
        .unwrap();
        let list: SessionListResult = serde_json::from_value(envelope["ok"].clone()).unwrap();
        list.sessions[0].clone()
    }

    pub(super) fn pull_request(url: &str) -> PullRequestStatus {
        PullRequestStatus {
            url: url.to_owned(),
            number: 42,
            title: Some("Keep terminal resident".to_owned()),
            author: None,
            body: None,
            base_ref_name: None,
            head_ref_name: None,
            state: "OPEN".to_owned(),
            is_draft: false,
            review_decision: Some("APPROVED".to_owned()),
            mergeable: Some("MERGEABLE".to_owned()),
            merge_state_status: Some("CLEAN".to_owned()),
            additions: 45,
            deletions: 12,
            changed_files: 3,
            comment_count: 2,
            review_count: 1,
            resolved_threads: Some(3),
            total_threads: Some(5),
            checks_passed: 3,
            checks_failed: 1,
            checks_pending: 1,
            checks: Some(vec![
                PrCheck {
                    name: "build".to_owned(),
                    result: "pending".to_owned(),
                    detail: None,
                    url: None,
                },
                PrCheck {
                    name: "lint".to_owned(),
                    result: "fail".to_owned(),
                    detail: None,
                    url: Some("https://example.com/lint".to_owned()),
                },
                PrCheck {
                    name: "test".to_owned(),
                    result: "pass".to_owned(),
                    detail: None,
                    url: None,
                },
            ]),
            discussion: None,
            fetched_at: DateMillis(1.0),
        }
    }

    #[test]
    fn check_popover_prioritizes_failure_then_running() {
        let checks = session_links::sorted_checks(&pull_request("https://example.com/pull/42"));
        assert_eq!(
            checks
                .iter()
                .map(|check| check.result.as_str())
                .collect::<Vec<_>>(),
            ["fail", "pending", "pass"]
        );
    }

    #[gpui::test]
    fn an_empty_terminal_pane_keeps_the_sidebar_reveal_control(cx: &mut TestAppContext) {
        let runtime = Arc::new(StoreRuntime::inert());
        let tokio = Arc::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("test runtime"),
        );

        let (pane, cx) = cx.add_window_view(move |window, cx| {
            let mut pane = TerminalPane::new(runtime, tokio, window, cx);
            pane.set_shell_chrome(false, false, cx);
            pane
        });

        assert!(
            pane.read_with(cx, |pane, _| pane.selected_session().is_none()),
            "fixture must exercise the empty terminal state"
        );
        assert!(
            cx.debug_bounds("empty-start-session").is_some(),
            "the empty pane offers a direct next action"
        );
        assert!(
            cx.debug_bounds("show-sidebar").is_some(),
            "collapsing the sidebar must leave a way to reveal it"
        );
    }

    #[gpui::test]
    fn a_first_launch_without_agents_installs_one_from_the_empty_pane(cx: &mut TestAppContext) {
        let runtime = Arc::new(StoreRuntime::inert());
        runtime
            .store
            .write()
            .unwrap()
            .set_agent_catalog(crate::agent_setup::bundled_catalog(&[]));
        let tokio = Arc::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("test runtime"),
        );
        let store = Arc::clone(&runtime.store);
        let (pane, cx) =
            cx.add_window_view(move |window, cx| TerminalPane::new(runtime, tokio, window, cx));

        assert!(
            cx.debug_bounds("empty-start-session").is_none(),
            "with no agent, starting a session is not the next step"
        );
        let install = cx
            .debug_bounds("welcome-install-claude-code")
            .expect("the shortest path to a first session is one button");
        cx.simulate_click(install.center(), gpui::Modifiers::default());
        cx.run_until_parked();

        // Nothing runs on the click: the system sheet shows the exact command
        // first, and Cancel leaves the Mac untouched.
        let (_, detail) = cx.pending_prompt().expect("install asks before running");
        assert!(
            detail.contains("curl -fsSL https://claude.ai/install.sh | bash"),
            "the sheet must show the command it is about to type: {detail}"
        );
        cx.simulate_prompt_answer("Cancel");
        cx.run_until_parked();
        assert_eq!(store.read().unwrap().installing_agent(), None);

        cx.simulate_click(install.center(), gpui::Modifiers::default());
        cx.run_until_parked();
        cx.simulate_prompt_answer("Install");
        cx.run_until_parked();

        assert_eq!(
            store.read().unwrap().installing_agent(),
            Some(&diri_proto::AgentKind::CLAUDE_CODE)
        );
        assert!(
            cx.debug_bounds("welcome-install-claude-code").is_none(),
            "a running install cannot be started twice"
        );
        assert!(cx.debug_bounds("welcome-install-codex").is_some());

        // Detection finding any agent ends setup: the page now leads with
        // the session it was blocking.
        store
            .write()
            .unwrap()
            .set_agent_catalog(crate::agent_setup::bundled_catalog(&["claude-code"]));
        // The inert runtime has no change broadcast to repaint the pane.
        pane.update(cx, |_, cx| cx.notify());
        cx.run_until_parked();
        assert!(cx.debug_bounds("empty-start-session").is_some());
        assert!(cx.debug_bounds("welcome-install-codex").is_none());
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "writes a terminal interaction preview to DIRI_QOL_SCREENSHOT"]
    fn render_terminal_qol_screenshot() {
        use diri_proto::grid::{ChangedRow, GridCell, LinkSpan};
        use gpui::{AppContext as _, HeadlessAppContext};
        let output = std::env::var("DIRI_QOL_SCREENSHOT").expect("output path");
        let scene = std::env::var("DIRI_QOL_SCENE").unwrap_or_default();
        let width: f32 = std::env::var("DIRI_QOL_WIDTH")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(800.0);
        let height: f32 = std::env::var("DIRI_QOL_HEIGHT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(600.0);
        let platform = gpui_platform::current_platform(true);
        let mut cx = HeadlessAppContext::with_platform(
            platform.text_system(),
            Arc::new(diri_ui::IconAssets),
            gpui_platform::current_headless_renderer,
        );
        cx.update(|cx| {
            crate::fonts::init(cx);
            cx.set_reduce_motion(true);
        });
        let runtime = Arc::new(StoreRuntime::inert());
        let tokio = Arc::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap(),
        );
        let session = fixture_session();
        let id = session.id.clone();
        {
            let mut store = runtime.store.write().unwrap();
            store
                .update_preferences(|prefs| {
                    prefs.terminal_paste_protection = true;
                    if let Ok(theme) = std::env::var("DIRI_QOL_THEME") {
                        prefs.terminal_theme = theme;
                    }
                })
                .unwrap();
            store.upsert_session(session);
            store.select(id.clone());
        }
        let window = cx
            .open_window(gpui::size(px(width), px(height)), |window, cx| {
                cx.new(|cx| {
                    let mut pane = TerminalPane::new(runtime, tokio, window, cx);
                    pane.set_viewport(
                        TerminalViewport {
                            x: 0.0,
                            y: 0.0,
                            width,
                            height,
                        },
                        cx,
                    );
                    let mut grid = grid_frame(80, true);
                    grid.rows = 28;
                    for (y, text) in [
                        "https://example.com/pull/42",
                        "View pull request",
                        "Build completed. Select output or right-click for terminal actions.",
                    ]
                    .iter()
                    .enumerate()
                    {
                        let mut cells = vec![GridCell::BLANK; 80];
                        for (cell, ch) in cells.iter_mut().zip(text.chars()) {
                            cell.scalar = ch as u32;
                        }
                        let mut row = ChangedRow::new(y as u16, cells);
                        if y == 1 {
                            row.metadata.links.push(LinkSpan {
                                start: 0,
                                end: 17,
                                uri: "https://example.com/pull/42".into(),
                            });
                        }
                        grid.changed_rows.push(row);
                    }
                    let find_fixture = if scene == "find-unicode" {
                        let mut screen = diri_engine::HeadlessScreen::new(80, 28);
                        screen.feed("$ printf 'Unicode terminal output'\r\n\r\n1  <界> cafe\u{301}\r\n2  A🙂B  cafe\u{301}\r\n\r\nSearch keeps wide glyphs and combining marks aligned with their cells.".as_bytes());
                        grid = screen.full_snapshot();
                        let query = std::env::var("DIRI_QOL_QUERY").unwrap_or_else(|_| "e\u{301}".into());
                        Some((query, FindSnapshot::from(screen.scrollback())))
                    } else if scene.starts_with("find") {
                        for row in [0, 15] {
                            let mut cells = vec![GridCell::BLANK; 80];
                            for (cell, ch) in cells[(width as usize / 12).clamp(4, 60)..]
                                .iter_mut()
                                .zip("needle".chars())
                            {
                                cell.scalar = ch as u32;
                            }
                            grid.changed_rows.retain(|changed| changed.y != row);
                            grid.changed_rows.push(ChangedRow::new(row, cells));
                        }
                        Some(("needle".into(), FindSnapshot {
                            cols: 80,
                            rows: 28,
                            is_alt_screen: scene == "find-alt",
                            ..find_snapshot(1)
                        }))
                    } else {
                        None
                    };
                    let resident = pane.residents.get_mut(&id).unwrap();
                    resident.element.apply_damage(grid);
                    if let Some((query, snapshot)) = find_fixture {
                        let mut find = TerminalFindModel::default();
                        let request = due_find_request(&mut find, &query, Duration::ZERO);
                        let result = resident.element.prepare_find_search(&find, &request, snapshot).unwrap().run();
                        resident.element.apply_find_result(&mut find, result);
                        assert!(!find.matches().is_empty());
                        if scene == "find-clear" {
                            resident.element.find_next(&mut find);
                        }
                        resident.find_query.insert(&query);
                        resident.element.sync_find_highlights(&find);
                        resident.find = Some(find);
                    }
                    resident.last_size = (80, 28);
                    resident.attachment_state = AttachmentState::Live;
                    resident.controller.seed_live_for_test();
                    // "secret-paste": pasted while the child reads a password.
                    resident.secret_input = scene == "secret-paste";
                    if scene == "scrolled" {
                        // Six hundred rows of history, read a third of the
                        // way up: enough for the scroller to show a knob.
                        resident.element.adopt_history_geometry(600, 628, 1, 28);
                        resident.element.set_view_offset(180, 28);
                    }
                    if scene == "returned-live" {
                        // The same history, read and then followed back to
                        // the live edge: the knob rests at the bottom of its
                        // track at the size it had on the way down.
                        resident.element.adopt_history_geometry(600, 628, 1, 28);
                        resident.element.set_view_offset(180, 28);
                        resident.element.scroll_to_live(28);
                    }
                    pane.focus(window, cx);
                    pane.reset_qol_session(&id);
                    pane.qol.hover = Some((2, 1));
                    match scene.as_str() {
                        "menu" => pane.open_terminal_menu(
                            gpui::point(px(260.0), px(180.0)),
                            2,
                            1,
                            window,
                            cx,
                        ),
                        "paste" | "secret-paste" => {
                            pane.stage_paste_if_needed(
                                &id,
                                &std::env::var("DIRI_QOL_PASTE").unwrap_or_else(|_| {
                                    "echo first command\necho second command".into()
                                }),
                                cx,
                            );
                        }
                        "controller-feedback" => pane.handle_pane_event(
                            PaneEvent::InputFeedback(id.clone(), "Terminal input queue is full. The latest input was not accepted.".into()), window, cx),
                        "copy" => pane.enter_copy_mode(window, cx),
                        _ => (),
                    }
                    pane
                })
            })
            .expect("preview window");
        cx.run_until_parked();
        if scene == "secret" {
            // The fixture's own attachment has reported Live by now; a mode
            // set any earlier is dropped with the state that preceded it.
            window
                .update(&mut cx, |pane, _, cx| {
                    let id = pane.selected_id().expect("selected session");
                    pane.residents.get_mut(&id).unwrap().secret_input = true;
                    cx.notify();
                })
                .unwrap();
            cx.run_until_parked();
        }
        cx.capture_screenshot(window.into())
            .expect("screenshot")
            .save(output)
            .expect("save");
        cx.update_window(window.into(), |_, window, _| window.remove_window())
            .unwrap();
        cx.run_until_parked();
    }

    #[gpui::test]
    fn split_terminal_switcher_events_keep_the_originating_window(cx: &mut TestAppContext) {
        use crate::store::WindowStore;
        use crate::workspace_workbench::WorkspaceWorkbench;
        use diri_proto::workspace::{LayoutAxis, LayoutNode, PaneId, SplitId, TabId, WorkspaceTab};
        let runtime = Arc::new(StoreRuntime::inert());
        let tokio = Arc::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap(),
        );
        let first = fixture_session();
        let first_id = first.id.clone();
        let mut second = first.clone();
        second.id = SessionId::new("switcher-second");
        let second_id = second.id.clone();
        {
            let mut store = runtime.store.write().unwrap();
            store.upsert_session(first);
            store.upsert_session(second);
            store.select(first_id.clone());
        }
        let origin = WindowStore::new(runtime.store.clone(), Some(first_id.clone()));
        let other = WindowStore::new(runtime.store.clone(), Some(second_id.clone()));
        let tab = WorkspaceTab {
            id: TabId::new("saved-tab"),
            title: Some("Saved split".into()),
            focused_pane: PaneId::new("first"),
            zoomed_pane: None,
            layout: LayoutNode::Split {
                id: SplitId::new("divider"),
                axis: LayoutAxis::Horizontal,
                fraction: 0.5,
                first: Box::new(LayoutNode::Pane {
                    id: PaneId::new("first"),
                    session_id: first_id.clone(),
                }),
                second: Box::new(LayoutNode::Pane {
                    id: PaneId::new("second"),
                    session_id: second_id.clone(),
                }),
            },
        };
        let workbench = cx.add_window(|window, cx| {
            let mut workbench = WorkspaceWorkbench::new(runtime.clone(), tokio.clone(), window, cx);
            workbench.set_window_store(origin.clone(), cx);
            workbench.set_tab(
                tab,
                TerminalViewport {
                    width: 900.0,
                    height: 600.0,
                    ..Default::default()
                },
                window,
                cx,
            );
            workbench
        });
        let other_window = cx.add_window(|window, cx| {
            TerminalPane::new_for_window(runtime.clone(), tokio.clone(), other.clone(), window, cx)
        });
        // Another window is focused after mounting; the fixed split must still
        // route terminal-local events to the window that owns that saved tab.
        other.write().unwrap().set_active(true);
        for release_with_key_up in [true, false] {
            origin.write().unwrap().select(first_id.clone());
            other.write().unwrap().select(second_id.clone());
            workbench
                .update(cx, |workbench, window, cx| {
                    let terminal = workbench.focused_terminal().unwrap();
                    terminal.update(cx, |pane, cx| {
                        assert_eq!(pane.window_store.as_ref().unwrap().owner(), origin.owner());
                        pane.handle_key_down(
                            &KeyDownEvent {
                                keystroke: Keystroke::parse("ctrl-tab").unwrap(),
                                is_held: false,
                                prefer_character_input: false,
                            },
                            window,
                            cx,
                        );
                        assert!(origin.read().unwrap().switcher_state().is_visible());
                        assert!(!other.read().unwrap().switcher_state().is_visible());
                        assert!(!runtime.store.read().unwrap().switcher_state().is_visible());
                        assert_eq!(
                            origin.read().unwrap().switcher_state().highlighted(),
                            Some(&second_id)
                        );
                        if release_with_key_up {
                            pane.handle_key_up(
                                &KeyUpEvent {
                                    keystroke: Keystroke::parse("control").unwrap(),
                                },
                                window,
                                cx,
                            );
                        } else {
                            pane.handle_modifiers_changed(
                                &ModifiersChangedEvent::default(),
                                window,
                                cx,
                            );
                        }
                    });
                })
                .unwrap();
            assert_eq!(
                origin.read().unwrap().selected_session_id(),
                Some(&second_id)
            );
            assert_eq!(
                other.read().unwrap().selected_session_id(),
                Some(&second_id)
            );
            assert_eq!(
                runtime.store.read().unwrap().selected_session_id(),
                Some(&first_id)
            );
            assert!(!origin.read().unwrap().switcher_state().is_visible());
        }
        other_window
            .update(cx, |_, window, _| window.remove_window())
            .unwrap();
        workbench
            .update(cx, |_, window, _| window.remove_window())
            .unwrap();
    }

    #[gpui::test]
    fn two_windows_transfer_control_without_replacing_grid_or_passive_resize(
        cx: &mut TestAppContext,
    ) {
        let runtime = Arc::new(StoreRuntime::inert());
        let tokio = Arc::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap(),
        );
        let session = fixture_session();
        let id = session.id.clone();
        {
            let mut store = runtime.store.write().unwrap();
            store.upsert_session(session);
            store.select(id.clone());
        }
        let first = cx
            .add_window(|window, cx| TerminalPane::new(runtime.clone(), tokio.clone(), window, cx));
        let second = cx.add_window(|window, cx| {
            TerminalPane::new_fixed(runtime.clone(), tokio.clone(), id.clone(), window, cx)
        });
        // Both windows hydrate before either is active. In particular the
        // first session reference must not acquire control just by mounting.
        first
            .update(cx, |pane, window, cx| {
                assert!(!pane.residents[&id].attachment.is_controller());
                pane.focus(window, cx);
                pane.set_viewport(
                    TerminalViewport {
                        width: 400.0,
                        height: 300.0,
                        ..Default::default()
                    },
                    cx,
                );
                pane.update_selected_geometry(window, cx);
                assert_eq!(pane.residents[&id].last_size, (0, 0));
                assert!(!pane.residents[&id].attachment.is_controller());
                window.activate_window();
            })
            .unwrap();
        cx.run_until_parked();
        let first_grid = first
            .update(cx, |pane, window, cx| {
                pane.focus(window, cx);
                pane.set_viewport(
                    TerminalViewport {
                        width: 900.0,
                        height: 600.0,
                        ..Default::default()
                    },
                    cx,
                );
                pane.update_selected_geometry(window, cx);
                pane.residents[&id].element.buffer()
            })
            .unwrap();
        second
            .update(cx, |pane, window, cx| {
                assert!(Arc::ptr_eq(
                    &first_grid,
                    &pane.residents[&id].element.buffer()
                ));
                assert!(!pane.residents[&id].attachment.is_controller());
                pane.set_viewport(
                    TerminalViewport {
                        width: 300.0,
                        height: 200.0,
                        ..Default::default()
                    },
                    cx,
                );
                pane.update_selected_geometry(window, cx);
                assert_eq!(
                    pane.residents[&id].last_size,
                    (0, 0),
                    "passive layout cannot resize"
                );
                pane.focus(window, cx);
                assert!(
                    !pane.residents[&id].attachment.is_controller(),
                    "inactive hydration/focus cannot steal control"
                );
            })
            .unwrap();
        second
            .update(cx, |_, window, _| window.activate_window())
            .unwrap();
        cx.run_until_parked();
        second
            .update(cx, |pane, window, cx| {
                assert!(
                    pane.residents[&id].attachment.is_controller(),
                    "activation claims the already-focused pane without another click"
                );
                pane.update_selected_geometry(window, cx);
                assert_ne!(pane.residents[&id].last_size, (0, 0));
                pane.handle_pane_event(
                    PaneEvent::InputFeedback(id.clone(), "Input rejected".into()),
                    window,
                    cx,
                );
                assert_eq!(pane.qol.feedback.as_deref(), Some("Input rejected"));
                let generation = pane.qol.feedback_generation;
                for _ in 0..100 {
                    pane.handle_pane_event(
                        PaneEvent::InputFeedback(id.clone(), "Input rejected".into()),
                        window,
                        cx,
                    );
                }
                assert_eq!(
                    pane.qol.feedback_generation, generation,
                    "identical rejection does not create another timer or repaint"
                );
            })
            .unwrap();
        first
            .update(cx, |pane, window, cx| {
                assert!(!pane.residents[&id].attachment.is_controller());
                pane.observed_selected_id = None;
                pane.reconcile_store_change(window, cx);
                assert!(
                    !pane.residents[&id].attachment.is_controller(),
                    "inactive selection hydration cannot take ownership"
                );
                let size = pane.residents[&id].last_size;
                pane.set_viewport(
                    TerminalViewport {
                        width: 700.0,
                        height: 500.0,
                        ..Default::default()
                    },
                    cx,
                );
                pane.update_selected_geometry(window, cx);
                assert_eq!(pane.residents[&id].last_size, size);
                window.remove_window();
            })
            .unwrap();
        second
            .update(cx, |pane, window, _| {
                assert!(pane.residents[&id].attachment.is_controller());
                assert!(Arc::ptr_eq(
                    &first_grid,
                    &pane.residents[&id].element.buffer()
                ));
                window.remove_window();
            })
            .unwrap();
    }

    /// The links panel is its own window and renders the pane while it draws,
    /// so GPUI comes to regard that window as the pane's. Closing it leaves
    /// the pane with no window until the main one draws again, and only
    /// terminal output asks for that draw: output landing in the gap must
    /// still repaint, or the session stays frozen until something else does.
    #[gpui::test]
    fn output_keeps_repainting_after_a_floating_window_closes(cx: &mut TestAppContext) {
        struct Panel {
            pane: Entity<TerminalPane>,
        }
        impl Render for Panel {
            fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
                let _ = self.pane.read(cx).selected_id();
                div()
            }
        }
        let runtime = Arc::new(StoreRuntime::inert());
        let tokio = Arc::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap(),
        );
        let mut session = fixture_session();
        session.host = None;
        let id = session.id.clone();
        {
            let mut store = runtime.store.write().unwrap();
            store.upsert_session(session);
            store.select(id.clone());
        }
        let (pane, cx) = cx.add_window_view({
            let runtime = runtime.clone();
            move |window, cx| TerminalPane::new(runtime, tokio, window, cx)
        });
        cx.run_until_parked();
        let (sender, generation) = pane.read_with(cx, |pane, _| {
            (
                pane.pane_tx.clone(),
                pane.residents[&id].attachment_generation,
            )
        });

        let panel = cx.update(|_, cx| {
            let pane = pane.clone();
            cx.open_window(Default::default(), |_, cx| cx.new(|_| Panel { pane }))
                .unwrap()
        });
        cx.run_until_parked();
        panel
            .update(cx, |_, window, _| window.remove_window())
            .unwrap();
        cx.run_until_parked();

        for _ in 0..3 {
            let before = pane.read_with(cx, |pane, _| pane.render_count);
            sender
                .send(PaneEvent::ControllerDamage(id.clone(), generation, true))
                .expect("the pane still listens for output");
            cx.run_until_parked();
            assert!(
                pane.read_with(cx, |pane, _| pane.render_count) > before,
                "terminal output must repaint the pane"
            );
        }

        // Selection changes reach the pane through the same kind of loop.
        let mut other = fixture_session();
        other.host = None;
        other.id = SessionId::new("after-the-panel");
        let other_id = other.id.clone();
        {
            let mut store = runtime.store.write().unwrap();
            store.upsert_session(other);
            store.select(other_id.clone());
        }
        runtime.publish_local_change();
        cx.run_until_parked();
        assert_eq!(
            pane.read_with(cx, |pane, _| pane.observed_selected_id.clone()),
            Some(other_id)
        );
    }

    #[gpui::test]
    fn image_drop_claims_input_before_pasting(cx: &mut TestAppContext) {
        let runtime = Arc::new(StoreRuntime::inert());
        let tokio = Arc::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap(),
        );
        let mut session = fixture_session();
        session.host = None;
        session.kind = diri_proto::AgentKind::CODEX;
        let id = session.id.clone();
        {
            let mut store = runtime.store.write().unwrap();
            store.upsert_session(session);
            store.select(id.clone());
        }
        let image = tempfile::Builder::new().suffix(".png").tempfile().unwrap();
        let paths = ExternalPaths(smallvec::smallvec![image.path().to_path_buf()]);
        let (pane, cx) =
            cx.add_window_view(move |window, cx| TerminalPane::new(runtime, tokio, window, cx));
        pane.update_in(cx, |pane, window, cx| {
            pane.reconcile_store_change(window, cx);
            let other = cx.new(|cx| {
                TerminalPane::new_fixed(
                    pane.runtime.clone(),
                    pane._tokio_owner.clone(),
                    id.clone(),
                    window,
                    cx,
                )
            });
            other.update(cx, |other, cx| {
                other.reconcile_residency(cx);
                other.claim_selected_control();
            });
            let (tx, mut input) = mpsc::unbounded_channel();
            let resident = pane.residents.get_mut(&id).unwrap();
            resident.attachment.input_observer = Some((id.clone(), tx));
            resident.bracketed_paste = true;
            assert!(!resident.attachment.is_controller());
            pane.external_drop(&paths, window, cx);
            let expected = terminal_file_paste(
                &terminal_drop_text([image.path().to_str().unwrap()]),
                true,
                Some(&diri_proto::AgentKind::CODEX),
            );
            assert_eq!(
                input.try_recv().ok(),
                Some((id.clone(), expected)),
                "dropping an image must claim the target before admitting its path"
            );
            assert!(input.try_recv().is_err(), "drop must paste exactly once");
            assert!(pane.focus.is_focused(window));
            assert!(
                !other.read(cx).residents[&id].attachment.is_controller(),
                "the previous view must lose input authority"
            );
        });
    }

    /// A pane on a local session, filling a window, ready to take a drop.
    fn drop_target_pane(
        cx: &mut TestAppContext,
    ) -> (
        Entity<TerminalPane>,
        SessionId,
        &mut gpui::VisualTestContext,
    ) {
        let runtime = Arc::new(StoreRuntime::inert());
        let tokio = Arc::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap(),
        );
        let mut session = fixture_session();
        session.host = None;
        let id = session.id.clone();
        {
            let mut store = runtime.store.write().unwrap();
            store.upsert_session(session);
            store.select(id.clone());
        }
        let (pane, cx) =
            cx.add_window_view(move |window, cx| TerminalPane::new(runtime, tokio, window, cx));
        pane.update_in(cx, |pane, window, cx| {
            pane.reconcile_store_change(window, cx);
            assert!(pane.residents.contains_key(&id));
        });
        cx.run_until_parked();
        (pane, id, cx)
    }

    #[gpui::test]
    fn files_dragged_over_a_pane_tick_on_arrival_and_again_when_taken(cx: &mut TestAppContext) {
        use gpui::FileDropEvent;
        let (_pane, id, cx) = drop_target_pane(cx);
        let image = tempfile::Builder::new().suffix(".png").tempfile().unwrap();
        let paths = ExternalPaths(smallvec::smallvec![image.path().to_path_buf()]);
        let target = haptics::key("terminal-drop", &id);
        let inside = gpui::point(px(200.0), px(150.0));
        let outside = gpui::point(px(-20.0), px(150.0));
        let _ = haptics::testing::take();

        cx.simulate_event(FileDropEvent::Entered {
            position: outside,
            paths: paths.clone(),
        });
        cx.simulate_event(FileDropEvent::Pending { position: outside });
        assert_eq!(
            haptics::testing::take(),
            [],
            "in the window is not yet over the pane"
        );

        cx.simulate_event(FileDropEvent::Pending { position: inside });
        assert_eq!(haptics::testing::take(), [(Haptic::Snap, target)]);

        // Moving on inside the pane is hover, and macOS keeps reporting a
        // pointer that is holding still.
        cx.simulate_event(FileDropEvent::Pending {
            position: inside + gpui::point(px(30.0), px(10.0)),
        });
        for _ in 0..3 {
            cx.simulate_event(FileDropEvent::Pending { position: inside });
        }
        assert_eq!(haptics::testing::take(), []);

        // Out and straight back in is one boundary crossed twice in a
        // hurry; coming back later is a new arrival.
        cx.simulate_event(FileDropEvent::Pending { position: outside });
        cx.simulate_event(FileDropEvent::Pending { position: inside });
        assert_eq!(haptics::testing::take(), []);
        cx.simulate_event(FileDropEvent::Pending { position: outside });
        haptics::testing::advance(haptics::REPEAT_WINDOW * 2);
        cx.simulate_event(FileDropEvent::Pending { position: inside });
        assert_eq!(haptics::testing::take(), [(Haptic::Snap, target)]);

        cx.simulate_event(FileDropEvent::Submit { position: inside });
        assert_eq!(
            haptics::testing::take(),
            [(Haptic::Accepted, target)],
            "the release is confirmed with its own pattern"
        );
    }

    #[gpui::test]
    fn files_a_pane_cannot_take_are_met_with_silence(cx: &mut TestAppContext) {
        use gpui::FileDropEvent;
        let (_pane, _, cx) = drop_target_pane(cx);
        let folder = tempfile::tempdir().unwrap();
        let paths = ExternalPaths(smallvec::smallvec![folder.path().join("gone.png")]);
        let inside = gpui::point(px(200.0), px(150.0));
        let _ = haptics::testing::take();

        cx.simulate_event(FileDropEvent::Entered {
            position: gpui::point(px(-20.0), px(150.0)),
            paths,
        });
        cx.simulate_event(FileDropEvent::Pending { position: inside });
        cx.simulate_event(FileDropEvent::Pending {
            position: inside + gpui::point(px(5.0), px(0.0)),
        });
        cx.simulate_event(FileDropEvent::Submit { position: inside });
        assert_eq!(haptics::testing::take(), []);
    }

    #[gpui::test]
    fn terminal_feedback_uses_shell_toast(cx: &mut TestAppContext) {
        let runtime = Arc::new(StoreRuntime::inert());
        let tokio = Arc::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap(),
        );
        let session = fixture_session();
        let id = session.id.clone();
        {
            let mut store = runtime.store.write().unwrap();
            store.upsert_session(session);
            store.select(id.clone());
        }
        let (pane, cx) =
            cx.add_window_view(move |window, cx| TerminalPane::new(runtime, tokio, window, cx));
        let mut events = cx.events(&pane);
        pane.update_in(cx, |pane, window, cx| {
            pane.handle_pane_event(
                PaneEvent::InputFeedback(id.clone(), "Input rejected".into()),
                window,
                cx,
            );
        });
        assert_eq!(
            events.try_recv().ok(),
            Some(TerminalPaneEvent::Feedback {
                message: "Input rejected".into(),
            }),
            "input feedback must reach the shell toast"
        );
        pane.update_in(cx, |pane, window, cx| {
            pane.handle_pane_event(
                PaneEvent::InputFeedback(id, "Input rejected".into()),
                window,
                cx,
            );
        });
        assert!(
            events.try_recv().is_err(),
            "repeated rejection must not flood toasts"
        );
    }

    #[gpui::test]
    fn terminal_local_file_links_open_in_the_default_app(cx: &mut TestAppContext) {
        let runtime = Arc::new(StoreRuntime::inert());
        let tokio = Arc::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap(),
        );
        let mut session = fixture_session();
        session.host = None;
        session.cwd = "/tmp/workspace".into();
        {
            let mut store = runtime.store.write().unwrap();
            store.upsert_session(session.clone());
            store.select(session.id);
        }
        let (pane, cx) =
            cx.add_window_view(move |window, cx| TerminalPane::new(runtime, tokio, window, cx));
        let mut events = cx.events(&pane);
        for (reference, expected) in [
            (
                "/Users/giga/Desktop/pr6037-current-tool-ui.png",
                "file:///Users/giga/Desktop/pr6037-current-tool-ui.png",
            ),
            ("./preview.html", "file:///tmp/workspace/preview.html"),
            (
                "file:///tmp/my%20preview.html",
                "file:///tmp/my%20preview.html",
            ),
            ("src/main.rs:42:7", "file:///tmp/workspace/src/main.rs"),
        ] {
            pane.update_in(cx, |pane, window, cx| {
                pane.open_reference(TerminalReference::File(reference.into()), window, cx);
            });
            assert_eq!(cx.opened_url().as_deref(), Some(expected), "{reference}");
            assert!(
                events.try_recv().is_err(),
                "local files must not reveal the inspector"
            );
        }
        pane.update_in(cx, |pane, window, cx| {
            pane.open_reference(
                TerminalReference::Url("https://example.com".into()),
                window,
                cx,
            );
        });
        assert_eq!(cx.opened_url().as_deref(), Some("https://example.com"));
        pane.update_in(cx, |pane, window, cx| {
            pane.open_reference(
                TerminalReference::File("file://remote/tmp/preview.html".into()),
                window,
                cx,
            );
            assert_eq!(
                pane.qol.feedback.as_deref(),
                Some("Could not open this local file link")
            );
        });
        assert_eq!(cx.opened_url().as_deref(), Some("https://example.com"));
        assert_eq!(
            events.try_recv().ok(),
            Some(TerminalPaneEvent::Feedback {
                message: "Could not open this local file link".into(),
            })
        );

        pane.update_in(cx, |pane, window, cx| {
            let mut session = (*pane.selected_session().unwrap()).clone();
            session.host = Some("remote-host".into());
            pane.runtime
                .store
                .write()
                .unwrap()
                .upsert_session(session.clone());
            pane.open_reference(
                TerminalReference::File("/tmp/preview.html".into()),
                window,
                cx,
            );
        });
        assert!(matches!(
            events.try_recv(),
            Ok(TerminalPaneEvent::OpenFileReference { .. })
        ));
        assert_eq!(
            cx.opened_url().as_deref(),
            Some("https://example.com"),
            "remote paths must not open local files"
        );
    }

    #[gpui::test]
    fn terminal_navigation_survives_missing_keyboard_metadata(cx: &mut TestAppContext) {
        let runtime = Arc::new(StoreRuntime::inert());
        let tokio = Arc::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap(),
        );
        let session = fixture_session();
        let id = session.id.clone();
        {
            let mut store = runtime.store.write().unwrap();
            store.upsert_session(session);
            store.select(id.clone());
        }
        let (pane, cx) =
            cx.add_window_view(move |window, cx| TerminalPane::new(runtime, tokio, window, cx));
        pane.update_in(cx, |pane, window, cx| {
            pane.reconcile_store_change(window, cx);
            let (tx, mut input) = mpsc::unbounded_channel();
            let resident = pane.residents.get_mut(&id).unwrap();
            resident.attachment.claim();
            resident.attachment.input_observer = Some((id.clone(), tx));
            resident.keyboard = None;
            for (key, expected) in [
                ("up", b"\x1b[A".as_slice()),
                ("down", b"\x1b[B"),
                ("right", b"\x1b[C"),
                ("left", b"\x1b[D"),
                ("home", b"\x1b[H"),
                ("end", b"\x1b[F"),
                ("alt-left", b"\x1b[1;3D"),
                ("shift-right", b"\x1b[1;2C"),
                ("cmd-left", b"\x01"),
                ("cmd-right", b"\x05"),
                ("cmd-backspace", b"\x15"),
            ] {
                pane.handle_key_down(
                    &KeyDownEvent {
                        keystroke: Keystroke::parse(key).unwrap(),
                        is_held: false,
                        prefer_character_input: false,
                    },
                    window,
                    cx,
                );
                assert_eq!(
                    pane.qol.feedback, None,
                    "{key} must not be blocked by missing metadata"
                );
                assert_eq!(input.try_recv().unwrap(), (id.clone(), expected.to_vec()));
                assert_eq!(
                    pane.residents[&id].keyboard, None,
                    "compatibility must not invent observed state"
                );
            }
        });
    }

    #[gpui::test]
    fn explicit_terminal_input_returns_a_reading_view_to_live(cx: &mut TestAppContext) {
        const ROWS: usize = 10;
        const READING: i64 = 5;
        let runtime = Arc::new(StoreRuntime::inert());
        let tokio = Arc::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap(),
        );
        let session = fixture_session();
        let id = session.id.clone();
        {
            let mut store = runtime.store.write().unwrap();
            store.upsert_session(session);
            store.select(id.clone());
        }
        let (pane, cx) =
            cx.add_window_view(move |window, cx| TerminalPane::new(runtime, tokio, window, cx));
        let key = |key: &str| KeyDownEvent {
            keystroke: Keystroke::parse(key).unwrap(),
            is_held: false,
            prefer_character_input: false,
        };
        pane.update_in(cx, |pane, window, cx| {
            pane.reconcile_store_change(window, cx);
            let (tx, mut input) = mpsc::unbounded_channel();
            let resident = pane.residents.get_mut(&id).unwrap();
            resident.attachment.claim();
            resident.attachment.input_observer = Some((id.clone(), tx));
            let generation = resident.attachment_generation;
            resident.element.adopt_history_geometry(100, 110, 1, ROWS);
            let read_history = |pane: &TerminalPane| {
                let element = &pane.residents[&id].element;
                element.set_view_offset(READING, ROWS);
                assert_eq!(element.view_offset(), READING);
            };
            let offset = |pane: &TerminalPane| pane.residents[&id].element.view_offset();

            // Typed text, Enter and the command-key line navigation all reach
            // the PTY, so each has to put the prompt back on screen.
            for (keystroke, expected) in [
                ("a", b"a".as_slice()),
                ("enter", b"\r"),
                ("cmd-left", b"\x01"),
            ] {
                read_history(pane);
                pane.handle_key_down(&key(keystroke), window, cx);
                assert_eq!(input.try_recv().unwrap(), (id.clone(), expected.to_vec()));
                assert_eq!(offset(pane), 0, "{keystroke} left the terminal in history");
            }

            read_history(pane);
            cx.write_to_clipboard(ClipboardItem::new_string("echo pasted".to_owned()));
            pane.paste(&Paste, window, cx);
            assert_eq!(
                input.try_recv().unwrap(),
                (id.clone(), b"echo pasted".to_vec())
            );
            assert_eq!(offset(pane), 0, "paste left the terminal in history");

            // Nothing below is input aimed at the PTY: the reader keeps their
            // place, including while output keeps streaming underneath.
            read_history(pane);
            pane.handle_pane_event(
                PaneEvent::Chunk(
                    id.clone(),
                    generation,
                    TerminalChunk::Grid(filled_grid('o')),
                ),
                window,
                cx,
            );
            assert_eq!(offset(pane), READING, "background output moved the reader");

            pane.handle_modifiers_changed(
                &ModifiersChangedEvent {
                    modifiers: Modifiers {
                        shift: true,
                        ..Modifiers::default()
                    },
                    capslock: Default::default(),
                },
                window,
                cx,
            );
            assert_eq!(offset(pane), READING, "a pure modifier moved the reader");

            pane.handle_key_down(&key("cmd-c"), window, cx);
            pane.copy_selection(&CopySelection, window, cx);
            assert_eq!(offset(pane), READING, "copy moved the reader");

            pane.open_find(&OpenFind, window, cx);
            let mut typed = key("n");
            typed.keystroke = typed.keystroke.with_simulated_ime();
            pane.handle_key_down(&typed, window, cx);
            cx.write_to_clipboard(ClipboardItem::new_string("eedle".to_owned()));
            pane.paste(&Paste, window, cx);
            assert_eq!(pane.residents[&id].find_query.text(), "needle");
            assert_eq!(
                offset(pane),
                READING,
                "editing the Find query moved the reader"
            );

            assert!(input.try_recv().is_err(), "a local gesture reached the PTY");
        });
    }

    #[gpui::test]
    fn clipboard_image_failures_are_shown_and_paste_nothing(cx: &mut TestAppContext) {
        let runtime = Arc::new(StoreRuntime::inert());
        let tokio = Arc::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap(),
        );
        let session = fixture_session();
        let id = session.id.clone();
        {
            let mut store = runtime.store.write().unwrap();
            store.upsert_session(session);
            store.select(id.clone());
        }
        let (pane, cx) =
            cx.add_window_view(move |window, cx| TerminalPane::new(runtime, tokio, window, cx));
        let shown = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink = Arc::clone(&shown);
        cx.update(|_, cx| {
            cx.subscribe(&pane, move |_, event: &TerminalPaneEvent, _| {
                if let TerminalPaneEvent::Feedback { message } = event {
                    sink.lock().unwrap().push(message.clone());
                }
            })
            .detach();
        });
        let mut input = pane.update_in(cx, |pane, window, cx| {
            pane.reconcile_store_change(window, cx);
            let (tx, mut input) = mpsc::unbounded_channel();
            let resident = pane.residents.get_mut(&id).unwrap();
            resident.attachment.claim();
            resident.attachment.input_observer = Some((id.clone(), tx));

            // The staging seam fails with a detail only a developer can use.
            pane.paste_staged_clipboard_image(
                &id,
                Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "open /private/var/folders/secret/T/dirijor-clipboard-x.png",
                )),
                window,
                cx,
            );
            assert!(input.try_recv().is_err(), "a failed staging pasted a path");

            // The uploader fails with whatever scp wrote to stderr.
            pane.handle_pane_event(
                PaneEvent::ClipboardUploadFinished(
                    id.clone(),
                    Err("scp failed: deploy@forge.internal: Permission denied (publickey)".into()),
                ),
                window,
                cx,
            );
            assert!(input.try_recv().is_err(), "a failed upload pasted a path");
            input
        });
        assert_eq!(
            &*shown.lock().unwrap(),
            &[
                "Couldn't paste the clipboard image: permission denied",
                "Couldn't copy the clipboard image to the session's host",
            ],
            "each failure is shown once, without paths, hosts or subprocess output"
        );

        pane.update_in(cx, |pane, window, cx| {
            let staged = StagedClipboardImage::stage(b"png bytes", "png").unwrap();
            let local_path = staged.path().to_string_lossy().into_owned();
            pane.paste_staged_clipboard_image(&id, Ok(staged), window, cx);
            assert_eq!(
                input.try_recv().unwrap(),
                (id.clone(), local_path.into_bytes())
            );
            assert!(input.try_recv().is_err(), "the local path was pasted once");

            pane.handle_pane_event(
                PaneEvent::ClipboardUploadFinished(
                    id.clone(),
                    Ok("/tmp/dirijor-clipboard-x.png".into()),
                ),
                window,
                cx,
            );
            assert_eq!(
                input.try_recv().unwrap(),
                (id.clone(), b"/tmp/dirijor-clipboard-x.png".to_vec())
            );
            assert!(input.try_recv().is_err(), "the remote path was pasted once");
        });
        assert_eq!(shown.lock().unwrap().len(), 2, "success shows no failure");
    }

    #[gpui::test]
    fn arrow_keys_follow_the_attachment_application_cursor_mode(cx: &mut TestAppContext) {
        let runtime = Arc::new(StoreRuntime::inert());
        let tokio = Arc::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap(),
        );
        let session = fixture_session();
        let id = session.id.clone();
        {
            let mut store = runtime.store.write().unwrap();
            store.upsert_session(session);
            store.select(id.clone());
        }
        let (pane, cx) =
            cx.add_window_view(move |window, cx| TerminalPane::new(runtime, tokio, window, cx));
        pane.update_in(cx, |pane, window, cx| {
            pane.reconcile_store_change(window, cx);
            let (tx, mut input) = mpsc::unbounded_channel();
            let resident = pane.residents.get_mut(&id).unwrap();
            resident.attachment.claim();
            resident.attachment.input_observer = Some((id.clone(), tx));
            let generation = resident.attachment_generation;
            // DECCKM reaches the pane as the daemon's Modes frame, the same
            // event an attach seed and every later ESC[?1h / ESC[?1l produce.
            for (application_cursor_keys, up, shift_up) in [
                (true, b"\x1bOA".as_slice(), b"\x1b[1;2A".as_slice()),
                (false, b"\x1b[A", b"\x1b[1;2A"),
            ] {
                pane.handle_pane_event(
                    PaneEvent::Chunk(
                        id.clone(),
                        generation,
                        TerminalChunk::Modes {
                            keyboard: Some(diri_proto::terminal_input::KeyboardState {
                                application_cursor_keys,
                                ..Default::default()
                            }),
                            alt_screen: false,
                            bracketed_paste: false,
                            mouse: Default::default(),
                            secret_input: false,
                        },
                    ),
                    window,
                    cx,
                );
                for (key, expected) in [("up", up), ("shift-up", shift_up)] {
                    pane.handle_key_down(
                        &KeyDownEvent {
                            keystroke: Keystroke::parse(key).unwrap(),
                            is_held: false,
                            prefer_character_input: false,
                        },
                        window,
                        cx,
                    );
                    assert_eq!(
                        input.try_recv().unwrap(),
                        (id.clone(), expected.to_vec()),
                        "{key} with application cursor keys = {application_cursor_keys}"
                    );
                }
            }
        });
    }

    #[gpui::test]
    fn terminal_copy_mode_and_paste_review_keep_input_local(cx: &mut TestAppContext) {
        let runtime = Arc::new(StoreRuntime::inert());
        let tokio = Arc::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap(),
        );
        let session = fixture_session();
        let id = session.id.clone();
        {
            let mut store = runtime.store.write().unwrap();
            store.upsert_session(session);
            store.select(id.clone());
        }
        let (pane, cx) =
            cx.add_window_view(move |window, cx| TerminalPane::new(runtime, tokio, window, cx));
        pane.update_in(cx, |pane, window, cx| {
            pane.reconcile_store_change(window, cx);
            assert!(pane.residents.contains_key(&id));
            pane.enter_copy_mode(window, cx);
            assert!(pane.qol.copy_mode.is_some());
            for key in ["v", "right", "pageup", "a"] {
                let event = KeyDownEvent {
                    keystroke: Keystroke::parse(key).unwrap(),
                    is_held: false,
                    prefer_character_input: false,
                };
                assert!(pane.handle_qol_key(&event, window, cx));
                assert!(pane.qol.copy_mode.is_some());
            }
            let escape = KeyDownEvent {
                keystroke: Keystroke::parse("escape").unwrap(),
                is_held: false,
                prefer_character_input: false,
            };
            assert!(pane.handle_qol_key(&escape, window, cx));
            assert!(pane.qol.copy_mode.is_none());
            assert!(!pane.stage_paste_if_needed(&id, "echo one\necho two", cx));
            assert!(pane.qol.paste.is_none());
            pane.runtime
                .store
                .write()
                .unwrap()
                .update_preferences(|prefs| prefs.terminal_paste_protection = true)
                .unwrap();
            assert!(!pane.stage_paste_if_needed(&id, "ordinary text", cx));
            assert!(pane.stage_paste_if_needed(&id, "echo one\necho two", cx));
            assert!(pane.qol.paste.is_some());
            assert!(pane.handle_qol_key(&escape, window, cx));
            assert!(pane.qol.paste.is_none());
            assert!(pane.stage_paste_if_needed(&id, "echo one\necho two", cx));
            for key in ["tab", "enter"] {
                let event = KeyDownEvent {
                    keystroke: Keystroke::parse(key).unwrap(),
                    is_held: false,
                    prefer_character_input: false,
                };
                assert!(pane.handle_qol_key(&event, window, cx));
            }
            assert!(pane.qol.paste.is_none(), "Tab then Enter cancels the paste");
            assert!(pane.stage_paste_if_needed(&id, "echo one\necho two", cx));
            pane.residents.get_mut(&id).unwrap().bracketed_paste = true;
            let enter = KeyDownEvent {
                keystroke: Keystroke::parse("enter").unwrap(),
                is_held: false,
                prefer_character_input: false,
            };
            assert!(pane.handle_qol_key(&enter, window, cx));
            assert!(pane.qol.paste.is_none());
            assert_eq!(
                pane.qol.feedback.as_deref(),
                Some("Terminal changed. Paste again to review.")
            );
        });
    }

    #[gpui::test]
    fn find_overlay_tracks_painted_match_without_resizing_terminal(cx: &mut TestAppContext) {
        let runtime = Arc::new(StoreRuntime::inert());
        let tokio = Arc::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap(),
        );
        let session = fixture_session();
        let id = session.id.clone();
        {
            let mut store = runtime.store.write().unwrap();
            store.upsert_session(session);
            store.select(id.clone());
        }
        let (pane, cx) =
            cx.add_window_view(move |window, cx| TerminalPane::new(runtime, tokio, window, cx));
        pane.update_in(cx, |pane, window, cx| {
            pane.reconcile_store_change(window, cx);
            let resident = pane.residents.get_mut(&id).unwrap();
            resident.attachment_state = AttachmentState::Live;
            resident.element.apply_damage(grid_frame(200, true));
            cx.notify();
        });
        let surface = cx.debug_bounds("terminal-grid-surface").unwrap();
        let original_size = pane.read_with(cx, |pane, _| pane.residents[&id].last_size);
        pane.update_in(cx, |pane, window, cx| pane.open_find(&OpenFind, window, cx));
        let anchor = cx
            .debug_bounds("find-bar")
            .expect("find opens over terminal");
        let span = |row| diri_term::find::FindSpan {
            row,
            start_col: 0,
            end_col_exclusive: 200,
            is_current: true,
        };
        pane.update_in(cx, |pane, _, cx| {
            pane.residents[&id]
                .element
                .set_find_highlights(vec![span(0)]);
            cx.notify();
        });
        let moved = cx.debug_bounds("find-bar").unwrap();
        let current = pane.read_with(cx, |pane, _| {
            pane.residents[&id]
                .element
                .current_find_match_bounds()
                .unwrap()
        });
        assert!(
            moved.top() > anchor.top(),
            "bar must move in the frame with the match"
        );
        assert!(
            !moved.intersects(&current),
            "active result must remain readable"
        );
        assert_eq!(cx.debug_bounds("terminal-grid-surface").unwrap(), surface);
        pane.read_with(cx, |pane, _| {
            assert_eq!(pane.residents[&id].last_size, original_size)
        });
        pane.update_in(cx, |pane, _, cx| {
            pane.residents[&id]
                .element
                .set_find_highlights(vec![span(15)]);
            cx.notify();
        });
        assert_eq!(
            cx.debug_bounds("find-bar").unwrap(),
            anchor,
            "bar returns when result moves clear"
        );
        pane.update_in(cx, |pane, _, cx| {
            pane.residents[&id]
                .element
                .set_find_highlights(vec![span(0)]);
            cx.notify();
        });
        let relocated = cx.debug_bounds("find-bar").unwrap();
        assert_eq!(relocated, moved);
        pane.update_in(cx, |pane, _, cx| {
            pane.residents[&id]
                .element
                .set_view_offset(3, usize::from(original_size.1));
            cx.notify();
        });
        let refresh = cx
            .debug_bounds("find-refresh")
            .expect("reachable refresh control")
            .center();
        cx.simulate_mouse_down(refresh, MouseButton::Left, Modifiers::default());
        cx.simulate_mouse_up(refresh, MouseButton::Left, Modifiers::default());
        pane.read_with(cx, |pane, _| {
            assert!(
                pane.residents[&id].find.is_some(),
                "Refresh must not close Find"
            );
            assert_eq!(pane.residents[&id].element.view_offset(), 0);
            assert_eq!(pane.residents[&id].last_size, original_size);
        });
        let relocated = cx.debug_bounds("find-bar").unwrap();
        let close = gpui::point(relocated.right() - px(22.0), relocated.center().y);
        cx.simulate_mouse_down(close, MouseButton::Left, Modifiers::default());
        cx.simulate_mouse_up(close, MouseButton::Left, Modifiers::default());
        assert!(cx.debug_bounds("find-bar").is_none());
        pane.read_with(cx, |pane, _| {
            assert!(pane.residents[&id].find.is_none());
            assert!(
                pane.residents[&id]
                    .element
                    .current_find_match_bounds()
                    .is_none()
            );
            assert_eq!(pane.residents[&id].last_size, original_size);
        });
    }

    #[gpui::test]
    fn terminal_selection_drag_reaches_outside_and_context_menu_dismisses(cx: &mut TestAppContext) {
        let runtime = Arc::new(StoreRuntime::inert());
        let tokio = Arc::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap(),
        );
        let session = fixture_session();
        let id = session.id.clone();
        {
            let mut store = runtime.store.write().unwrap();
            store.upsert_session(session);
            store.select(id.clone());
        }
        let (pane, cx) =
            cx.add_window_view(move |window, cx| TerminalPane::new(runtime, tokio, window, cx));
        pane.update_in(cx, |pane, window, cx| {
            pane.reconcile_store_change(window, cx);
            pane.set_viewport(
                TerminalViewport {
                    x: 0.0,
                    y: 0.0,
                    width: 800.0,
                    height: 600.0,
                },
                cx,
            );
            let resident = pane.residents.get_mut(&id).unwrap();
            resident.attachment_state = AttachmentState::Live;
            resident.last_size = (80, 40);
            resident.element.apply_damage(grid_frame(80, true));
            cx.notify();
        });
        let bounds = cx
            .debug_bounds("terminal-grid-surface")
            .expect("terminal surface");
        cx.simulate_mouse_down(bounds.center(), MouseButton::Left, Modifiers::default());
        let outside = gpui::point(bounds.center().x, bounds.top() - px(8.0));
        cx.simulate_mouse_move(outside, MouseButton::Left, Modifiers::default());
        pane.read_with(cx, |pane, _| {
            assert!(pane.qol.drag.is_some(), "outside move must arm autoscroll")
        });
        cx.simulate_mouse_up(outside, MouseButton::Left, Modifiers::default());
        pane.read_with(cx, |pane, _| assert!(pane.qol.drag.is_none()));
        cx.simulate_mouse_down(bounds.center(), MouseButton::Right, Modifiers::default());
        cx.simulate_mouse_up(bounds.center(), MouseButton::Right, Modifiers::default());
        assert!(cx.debug_bounds("terminal-context-menu").is_some());
        cx.simulate_mouse_down(outside, MouseButton::Left, Modifiers::default());
        pane.read_with(cx, |pane, _| assert!(pane.qol.menu.is_none()));
    }

    fn selection_shimmer_pane(
        cx: &mut TestAppContext,
    ) -> (
        Entity<TerminalPane>,
        &mut gpui::VisualTestContext,
        SessionId,
    ) {
        let runtime = Arc::new(StoreRuntime::inert());
        let tokio = Arc::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap(),
        );
        let session = fixture_session();
        let id = session.id.clone();
        {
            let mut store = runtime.store.write().unwrap();
            store.upsert_session(session);
            store.select(id.clone());
        }
        let (pane, cx) =
            cx.add_window_view(move |window, cx| TerminalPane::new(runtime, tokio, window, cx));
        pane.update_in(cx, |pane, window, cx| {
            pane.reconcile_store_change(window, cx);
            pane.set_viewport(
                TerminalViewport {
                    x: 0.0,
                    y: 0.0,
                    width: 800.0,
                    height: 600.0,
                },
                cx,
            );
            let resident = pane.residents.get_mut(&id).unwrap();
            resident.attachment_state = AttachmentState::Live;
            resident.last_size = (80, 40);
            resident.element.apply_damage(grid_frame(80, true));
            cx.notify();
        });
        (pane, cx, id)
    }

    #[gpui::test]
    fn releasing_a_selection_starts_one_shimmer_that_ends_on_its_own(cx: &mut TestAppContext) {
        let (pane, cx, id) = selection_shimmer_pane(cx);
        let element = pane.read_with(cx, |pane, _| pane.residents[&id].element.clone());
        let started = std::time::Instant::now();
        element.pin_selection_shimmer_clock(Some(started));
        let bounds = cx
            .debug_bounds("terminal-grid-surface")
            .expect("terminal surface");
        let release = bounds.center() + gpui::point(px(120.0), px(45.0));

        // A bare click selects nothing, so there is nothing to confirm.
        // A bare click selects nothing, so there is nothing to confirm.
        cx.simulate_mouse_down(bounds.center(), MouseButton::Left, Modifiers::default());
        cx.simulate_mouse_up(bounds.center(), MouseButton::Left, Modifiers::default());
        assert!(!element.selection_shimmer_running());

        cx.simulate_mouse_down(bounds.center(), MouseButton::Left, Modifiers::default());
        cx.simulate_mouse_move(release, MouseButton::Left, Modifiers::default());
        assert!(element.selection_range().is_some());
        assert!(
            !element.selection_shimmer_running(),
            "the sheen waits for the release"
        );
        cx.simulate_mouse_up(release, MouseButton::Left, Modifiers::default());
        assert!(element.selection_shimmer_running());
        // Frame requests are counted against a bare element in diri-term; in
        // this window the scrollbar's own fade also asks for frames.
        for elapsed in [150, 300] {
            element.pin_selection_shimmer_clock(Some(
                started + std::time::Duration::from_millis(elapsed),
            ));
            pane.update(cx, |_, cx| cx.notify());
            cx.run_until_parked();
            assert!(
                element.selection_shimmer_running(),
                "running at {elapsed} ms"
            );
        }
        element.pin_selection_shimmer_clock(Some(started + diri_term::selection_shimmer::DURATION));
        pane.update(cx, |_, cx| cx.notify());
        cx.run_until_parked();
        assert!(!element.selection_shimmer_running());
        assert!(element.selection_range().is_some());
    }

    #[gpui::test]
    fn reduce_motion_keeps_a_finished_selection_static(cx: &mut TestAppContext) {
        let (pane, cx, id) = selection_shimmer_pane(cx);
        cx.update(|_, cx| cx.set_reduce_motion(true));
        let element = pane.read_with(cx, |pane, _| pane.residents[&id].element.clone());
        let bounds = cx
            .debug_bounds("terminal-grid-surface")
            .expect("terminal surface");
        let release = bounds.center() + gpui::point(px(120.0), px(45.0));
        cx.simulate_mouse_down(bounds.center(), MouseButton::Left, Modifiers::default());
        cx.simulate_mouse_move(release, MouseButton::Left, Modifiers::default());
        cx.simulate_mouse_up(release, MouseButton::Left, Modifiers::default());
        cx.run_until_parked();
        assert!(element.selection_range().is_some());
        assert!(!element.selection_shimmer_running());
    }

    #[gpui::test]
    fn changing_sessions_clears_feedback_and_rearms_identical_later_errors(
        cx: &mut TestAppContext,
    ) {
        let runtime = Arc::new(StoreRuntime::inert());
        let tokio = Arc::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap(),
        );
        let first = fixture_session();
        let mut second = first.clone();
        second.id = SessionId::new("feedback-next");
        {
            let mut store = runtime.store.write().unwrap();
            store.upsert_session(first.clone());
            store.upsert_session(second.clone());
            store.select(first.id);
        }
        let runtime_for_view = runtime.clone();
        let (pane, cx) = cx.add_window_view(move |window, cx| {
            TerminalPane::new(runtime_for_view, tokio, window, cx)
        });
        pane.update_in(cx, |pane, window, cx| {
            pane.show_terminal_feedback("Input rejected", window, cx)
        });
        runtime.store.write().unwrap().select(second.id);
        pane.update_in(cx, |pane, window, cx| {
            pane.reconcile_store_change(window, cx);
            assert!(
                pane.qol.feedback.is_none(),
                "previous session's feedback must not linger"
            );
            pane.show_terminal_feedback("Input rejected", window, cx);
            assert_eq!(pane.qol.feedback.as_deref(), Some("Input rejected"));
        });
        cx.run_until_parked();
        cx.executor().advance_clock(Duration::from_secs(3));
        cx.run_until_parked();
        pane.read_with(cx, |pane, _| {
            assert!(
                pane.qol.feedback.is_none(),
                "identical later error has its own expiry"
            )
        });
    }

    #[gpui::test]
    fn selecting_a_newly_spawned_session_focuses_its_terminal(cx: &mut TestAppContext) {
        let runtime = Arc::new(StoreRuntime::inert());
        let tokio = Arc::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("test runtime"),
        );
        let existing = fixture_session();
        {
            let mut store = runtime.store.write().expect("session store lock poisoned");
            store.upsert_session(existing.clone());
            store.select(existing.id.clone());
        }

        let runtime_for_view = Arc::clone(&runtime);
        let (pane, cx) = cx.add_window_view(move |window, cx| {
            TerminalPane::new(runtime_for_view, tokio, window, cx)
        });
        let _picker_focus = pane.update_in(cx, |pane, window, cx| {
            let picker_focus = cx.focus_handle();
            window.focus(&picker_focus, cx);
            assert!(!pane.is_focused(window));
            picker_focus
        });
        pane.update_in(cx, |pane, window, cx| {
            pane.reconcile_store_change(window, cx);
            assert!(
                !pane.is_focused(window),
                "an unrelated store update must not steal focus from the picker"
            );
        });

        let mut spawned = fixture_session();
        spawned.id = SessionId::new("spawned");
        {
            let mut store = runtime.store.write().expect("session store lock poisoned");
            store.upsert_session(spawned.clone());
            store.select(spawned.id);
        }

        // A successful spawn selects the daemon's new id asynchronously,
        // after the picker owned focus; the follow-selection pane must take
        // focus with that production store-change reconciliation.
        pane.update_in(cx, |pane, window, cx| {
            pane.reconcile_store_change(window, cx);
            assert!(pane.is_focused(window));
        });
    }

    #[test]
    fn secure_input_needs_a_secret_a_focused_pane_and_an_active_window() {
        assert!(secure_input_wanted(true, true, true));
        assert!(!secure_input_wanted(false, true, true));
        assert!(!secure_input_wanted(true, false, true));
        assert!(!secure_input_wanted(true, true, false));
    }

    #[gpui::test]
    fn secure_input_is_released_on_every_way_out_of_a_password_prompt(cx: &mut TestAppContext) {
        let runtime = Arc::new(StoreRuntime::inert());
        let tokio = Arc::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("test runtime"),
        );
        let prompting = fixture_session();
        let mut other = fixture_session();
        other.id = SessionId::new("other");
        {
            let mut store = runtime.store.write().expect("session store lock poisoned");
            store.upsert_session(prompting.clone());
            store.upsert_session(other.clone());
            store.select(prompting.id.clone());
        }
        let runtime_for_view = Arc::clone(&runtime);
        let (pane, cx) = cx.add_window_view(move |window, cx| {
            TerminalPane::new(runtime_for_view, tokio, window, cx)
        });
        let recorder = crate::secure_input::testing::Recorder::default();
        pane.update_in(cx, |pane, window, cx| {
            pane.secure_input = crate::secure_input::SecureInputLease::new(recorder.clone());
            window.activate_window();
            pane.focus(window, cx);
        });
        cx.run_until_parked();

        // What the engine sends when `sudo` silences echo, and when it is done.
        let modes = |pane: &mut TerminalPane,
                     secret_input: bool,
                     window: &mut Window,
                     cx: &mut Context<TerminalPane>| {
            let id = pane.selected_id().expect("selected session");
            let resident = pane.residents.get_mut(&id).expect("resident");
            resident.attachment_state = AttachmentState::Live;
            let generation = resident.attachment_generation;
            pane.handle_pane_event(
                PaneEvent::Chunk(
                    id,
                    generation,
                    TerminalChunk::Modes {
                        keyboard: None,
                        alt_screen: false,
                        bracketed_paste: false,
                        mouse: MouseModes::OFF,
                        secret_input,
                    },
                ),
                window,
                cx,
            );
        };

        pane.update_in(cx, |pane, window, cx| {
            assert!(pane.is_focused(window) && window.is_window_active());
            modes(pane, true, window, cx);
            // Rendering and further mode frames reconcile again; none of
            // that may take a second reference.
            modes(pane, true, window, cx);
        });
        cx.run_until_parked();
        assert_eq!(
            recorder.outstanding(),
            1,
            "held at a focused password prompt"
        );

        pane.update_in(cx, |pane, window, cx| modes(pane, false, window, cx));
        assert_eq!(recorder.outstanding(), 0, "the child restored echo");

        // Focus leaves the pane while the prompt is still up.
        pane.update_in(cx, |pane, window, cx| modes(pane, true, window, cx));
        assert_eq!(recorder.outstanding(), 1);
        let elsewhere = pane.update_in(cx, |_, window, cx| {
            let elsewhere = cx.focus_handle();
            window.focus(&elsewhere, cx);
            elsewhere
        });
        cx.run_until_parked();
        assert_eq!(recorder.outstanding(), 0, "the pane lost focus");
        pane.update_in(cx, |pane, window, cx| pane.focus(window, cx));
        cx.run_until_parked();
        assert_eq!(recorder.outstanding(), 1, "focus came back to the prompt");
        drop(elsewhere);

        // The window stops being the one the keyboard is pointed at.
        cx.deactivate_window();
        cx.run_until_parked();
        assert_eq!(recorder.outstanding(), 0, "the window deactivated");
        pane.update_in(cx, |_, window, _| window.activate_window());
        cx.run_until_parked();
        assert_eq!(recorder.outstanding(), 1, "the window is active again");

        // The transport drops: nothing is known about the child any more.
        pane.update_in(cx, |pane, window, cx| {
            let id = pane.selected_id().expect("selected session");
            let generation = pane.residents[&id].attachment_generation;
            pane.handle_pane_event(
                PaneEvent::AttachmentState(id, generation, AttachmentState::Reconnecting),
                window,
                cx,
            );
        });
        assert_eq!(recorder.outstanding(), 0, "the attachment is not live");
        pane.update_in(cx, |pane, window, cx| modes(pane, true, window, cx));
        assert_eq!(recorder.outstanding(), 1);

        // Another session is selected; the prompting one is no longer typed into.
        runtime
            .store
            .write()
            .expect("session store lock poisoned")
            .select(other.id.clone());
        pane.update_in(cx, |pane, window, cx| {
            pane.reconcile_store_change(window, cx);
        });
        cx.run_until_parked();
        assert_eq!(recorder.outstanding(), 0, "the session was deselected");

        // The window closes with a prompt still up. The pane outlives it
        // here, as a leaked or still-referenced entity would in the app.
        runtime
            .store
            .write()
            .expect("session store lock poisoned")
            .select(prompting.id.clone());
        pane.update_in(cx, |pane, window, cx| {
            pane.reconcile_store_change(window, cx);
        });
        // Let the fresh attachment report its own state before the prompt.
        cx.run_until_parked();
        pane.update_in(cx, |pane, window, cx| modes(pane, true, window, cx));
        assert_eq!(recorder.outstanding(), 1);
        pane.update_in(cx, |_, window, _| window.remove_window());
        cx.run_until_parked();
        assert_eq!(recorder.outstanding(), 0, "the window closed");
        assert_eq!(recorder.enables(), 6, "one reference per entry, never two");
    }

    #[gpui::test]
    fn the_running_app_reviews_a_paste_with_the_system_alert(cx: &mut TestAppContext) {
        let runtime = Arc::new(StoreRuntime::inert());
        let tokio = Arc::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap(),
        );
        let session = fixture_session();
        let id = session.id.clone();
        {
            let mut store = runtime.store.write().unwrap();
            store.upsert_session(session);
            store.select(id.clone());
        }
        cx.update(crate::alerts::enable);
        let (pane, cx) =
            cx.add_window_view(move |window, cx| TerminalPane::new(runtime, tokio, window, cx));
        let stage = |pane: &mut TerminalPane, cx: &mut Context<TerminalPane>| {
            assert!(pane.stage_paste_if_needed(&id, "make build\nmake install\n", cx));
        };
        pane.update_in(cx, |pane, window, cx| {
            pane.reconcile_store_change(window, cx);
            pane.residents.get_mut(&id).unwrap().attachment_state = AttachmentState::Live;
            pane.runtime
                .store
                .write()
                .unwrap()
                .update_preferences(|prefs| prefs.terminal_paste_protection = true)
                .unwrap();
            stage(pane, cx);
        });
        cx.run_until_parked();
        assert!(cx.has_pending_prompt(), "the system alert asks");
        assert!(
            cx.debug_bounds("terminal-paste-review").is_none(),
            "and the in-window panel stays out of its way"
        );

        cx.simulate_prompt_answer("Cancel");
        cx.run_until_parked();
        assert!(pane.read_with(cx, |pane, _| pane.qol.paste.is_none()));
        assert!(!cx.has_pending_prompt());

        pane.update_in(cx, |pane, _, cx| stage(pane, cx));
        cx.run_until_parked();
        assert!(cx.has_pending_prompt(), "a second paste asks again");
        cx.simulate_prompt_answer("Paste");
        cx.run_until_parked();
        assert!(pane.read_with(cx, |pane, _| pane.qol.paste.is_none()));
        assert!(!cx.has_pending_prompt());
    }

    #[test]
    fn paste_review_never_shows_text_staged_at_a_password_prompt() {
        let text = "hunter2\n";
        assert_eq!(
            qol::paste_review_preview(&mut text.chars(), false),
            "hunter2\n"
        );
        let hidden = qol::paste_review_preview(&mut text.chars(), true);
        assert!(!hidden.contains("hunter2"), "{hidden}");
        assert!(hidden.starts_with("8 characters"), "{hidden}");
    }

    #[gpui::test]
    fn switching_back_keeps_an_unchanged_pty_size_and_holds_a_real_column_change(
        cx: &mut TestAppContext,
    ) {
        let runtime = Arc::new(StoreRuntime::inert());
        let tokio = Arc::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("test runtime"),
        );
        let mut cursor = fixture_session();
        cursor.id = SessionId::new("cursor");
        let mut other = fixture_session();
        other.id = SessionId::new("other");
        let cursor_id = cursor.id.clone();
        let other_id = other.id.clone();
        {
            let mut store = runtime.store.write().expect("session store lock poisoned");
            store.upsert_session(cursor);
            store.upsert_session(other);
            store.select(cursor_id.clone());
        }
        let runtime_for_view = Arc::clone(&runtime);
        let (pane, cx) = cx.add_window_view(move |window, cx| {
            TerminalPane::new(runtime_for_view, tokio, window, cx)
        });
        let viewport = TerminalViewport {
            width: 900.0,
            height: 600.0,
            ..Default::default()
        };
        pane.update_in(cx, |pane, window, cx| {
            pane.set_viewport(viewport, cx);
            window.activate_window();
            pane.focus(window, cx);
            pane.update_selected_geometry(window, cx);
        });
        cx.run_until_parked();
        let sized = pane.read_with(cx, |pane, _| {
            let resident = &pane.residents[&cursor_id];
            assert_ne!(resident.last_size, (0, 0), "first show still measures");
            assert!(
                resident.attachment.resize_sends_for_test() >= 1,
                "a session with no size yet still resizes once"
            );
            resident.last_size
        });

        runtime
            .store
            .write()
            .expect("session store lock poisoned")
            .select(other_id);
        pane.update_in(cx, |pane, window, cx| {
            pane.reconcile_store_change(window, cx);
        });
        runtime
            .store
            .write()
            .expect("session store lock poisoned")
            .select(cursor_id.clone());
        pane.update_in(cx, |pane, window, cx| {
            pane.reconcile_store_change(window, cx);
            let resident = pane.residents.get(&cursor_id).expect("remounted");
            assert_eq!(
                resident.last_size, sized,
                "the new resident keeps the pty size"
            );
            assert_eq!(resident.attachment.resize_sends_for_test(), 0);
            // No pane viewport yet: the full-window guess must not be sent.
            pane.viewport = None;
            pane.update_selected_geometry(window, cx);
            let resident = &pane.residents[&cursor_id];
            assert_eq!(resident.attachment.resize_sends_for_test(), 0);
            assert_eq!(resident.last_size, sized);
            pane.set_viewport(viewport, cx);
            pane.focus(window, cx);
            pane.update_selected_geometry(window, cx);
            let resident = &pane.residents[&cursor_id];
            assert_eq!(
                resident.attachment.resize_sends_for_test(),
                0,
                "an unchanged pane must not resize"
            );
            assert_eq!(resident.last_size, sized);
            assert!(
                resident.attachment.needs_resize(sized),
                "not sending must not record a size the next attach would replay"
            );
            pane.last_resize_sent = Some(Instant::now() - Duration::from_secs(3));
            pane.set_viewport(
                TerminalViewport {
                    width: 1400.0,
                    height: 600.0,
                    ..Default::default()
                },
                cx,
            );
            pane.update_selected_geometry(window, cx);
            let resident = &pane.residents[&cursor_id];
            assert_ne!(
                resident.last_size.0, sized.0,
                "the hidden window change is real"
            );
            assert_eq!(resident.attachment.resize_sends_for_test(), 1);
            assert!(
                resident.controller.reflow_held_for_test(),
                "previous is the last real size, so the column change is held"
            );
            pane.update_selected_geometry(window, cx);
            assert_eq!(
                pane.residents[&cursor_id]
                    .attachment
                    .resize_sends_for_test(),
                1,
                "the catch-up resize is sent once"
            );
        });
    }

    #[gpui::test]
    fn stale_detached_attachment_events_cannot_overwrite_a_reselected_session(
        cx: &mut TestAppContext,
    ) {
        let runtime = Arc::new(StoreRuntime::inert());
        let tokio = Arc::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("test runtime"),
        );
        let mut reselected = fixture_session();
        reselected.id = SessionId::new("reselected");
        let mut other = fixture_session();
        other.id = SessionId::new("other");
        {
            let mut store = runtime.store.write().expect("session store lock poisoned");
            store.upsert_session(reselected.clone());
            store.upsert_session(other.clone());
            store.select(reselected.id.clone());
        }

        let runtime_for_view = Arc::clone(&runtime);
        let (pane, cx) = cx.add_window_view(move |window, cx| {
            TerminalPane::new(runtime_for_view, tokio, window, cx)
        });
        let old_generation = pane.read_with(cx, |pane, _| {
            pane.residents
                .get(&reselected.id)
                .expect("initial resident")
                .attachment_generation
        });
        let stale = PaneEvent::Chunk(
            reselected.id.clone(),
            old_generation,
            TerminalChunk::Grid(filled_grid('s')),
        );

        // Replace A's resident attachment, exactly as an A -> B -> A switch
        // does with the default residency of one.
        {
            runtime
                .store
                .write()
                .expect("session store lock poisoned")
                .select(other.id.clone());
        }
        pane.update_in(cx, |pane, window, cx| {
            pane.reconcile_store_change(window, cx);
        });
        {
            runtime
                .store
                .write()
                .expect("session store lock poisoned")
                .select(reselected.id.clone());
        }
        pane.update_in(cx, |pane, window, cx| {
            pane.reconcile_store_change(window, cx);
            let new_generation = pane
                .residents
                .get(&reselected.id)
                .expect("reselected resident")
                .attachment_generation;
            assert_ne!(new_generation, old_generation);
            pane.handle_pane_event(
                PaneEvent::Chunk(
                    reselected.id.clone(),
                    new_generation,
                    TerminalChunk::Grid(filled_grid('n')),
                ),
                window,
                cx,
            );
        });

        // The old attachment can finish a read after its control was dropped.
        // That event was already queued before the replacement existed and
        // must not repaint the new resident's buffer.
        pane.update_in(cx, |pane, window, cx| {
            pane.handle_pane_event(stale, window, cx);
            let resident = pane
                .residents
                .get(&reselected.id)
                .expect("reselected resident");
            let buffer = resident.element.buffer();
            let buffer = buffer.read().expect("grid buffer lock poisoned");
            assert_eq!(
                buffer.cells[0].scalar,
                u32::from('n'),
                "a detached attachment repainted the newly selected terminal"
            );
        });

        // Find crosses two additional async handoffs. Exercise both with a
        // request that would otherwise be valid for the new resident: only the
        // attachment generation distinguishes the old producer.
        let mut find = TerminalFindModel::default();
        find.set_query("needle", Duration::ZERO);
        let request = find
            .take_due_search(Duration::from_millis(200))
            .expect("find request");
        let snapshot = FindSnapshot {
            error: None,
            retained: None,
            text_cells: Default::default(),
            lines: Vec::new(),
            first_row: 0,
            visible_start_row: 0,
            cols: 8,
            rows: 1,
            content_seq: 1,
            is_alt_screen: false,
        };
        let mut live = GridBuffer::new(8, 1);
        for (index, ch) in "needle".chars().enumerate() {
            live.cells[index] = GridCell::new(
                u32::from(ch),
                TermColor::Default,
                TermColor::DefaultInverted,
                TermStyle::empty(),
            );
        }
        let result = find
            .prepare_search(&request, snapshot.clone(), &live)
            .expect("search job")
            .run();

        pane.update_in(cx, move |pane, window, cx| {
            let resident = pane
                .residents
                .get_mut(&reselected.id)
                .expect("reselected resident");
            resident.find = Some(find);
            assert_eq!(
                resident.find_scheduler.schedule(request.clone()),
                Some(request.clone())
            );

            pane.handle_pane_event(
                PaneEvent::FindSnapshot(
                    reselected.id.clone(),
                    old_generation,
                    request.clone(),
                    Some(snapshot.clone()),
                ),
                window,
                cx,
            );
            let resident = pane
                .residents
                .get_mut(&reselected.id)
                .expect("reselected resident");
            assert_eq!(
                resident.find_scheduler.finish_read(&request, true),
                ReadCompletion::Scan,
                "stale snapshot advanced the new resident's scheduler"
            );

            pane.handle_pane_event(
                PaneEvent::FindResult(
                    reselected.id.clone(),
                    old_generation,
                    request.clone(),
                    result,
                ),
                window,
                cx,
            );
            let resident = pane
                .residents
                .get_mut(&reselected.id)
                .expect("reselected resident");
            assert!(resident.find.as_ref().unwrap().matches().is_empty());
            assert!(
                resident.find_scheduler.finish_scan(&request).is_some(),
                "stale result completed the new resident's active scan"
            );
        });
    }

    #[test]
    fn history_extent_probes_are_single_flight_and_need_new_content() {
        let start = Instant::now();
        let mut probe = HistoryExtentProbe::default();
        assert!(probe.should_send(7, start));
        probe = HistoryExtentProbe {
            in_flight: true,
            generation: Some(7),
            sent_at: Some(start),
        };
        let later = start + HISTORY_EXTENT_PROBE_INTERVAL;
        assert!(!probe.should_send(8, later), "one read at a time");
        probe.in_flight = false;
        assert!(
            !probe.should_send(7, later),
            "an unchanged screen never re-asks"
        );
        assert!(
            !probe.should_send(8, start),
            "streaming output is rate limited"
        );
        assert!(probe.should_send(8, later));
    }

    #[gpui::test]
    fn the_scroller_knob_keeps_its_size_at_the_live_edge(cx: &mut TestAppContext) {
        const ROWS: usize = 10;
        let runtime = Arc::new(StoreRuntime::inert());
        let tokio = Arc::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("test runtime"),
        );
        let session = fixture_session();
        let id = session.id.clone();
        {
            let mut store = runtime.store.write().expect("session store lock poisoned");
            store.upsert_session(session);
            store.select(id.clone());
        }
        let (pane, cx) =
            cx.add_window_view(move |window, cx| TerminalPane::new(runtime, tokio, window, cx));

        pane.update_in(cx, |pane, window, cx| {
            let resident = &pane.residents[&id];
            let generation = resident.attachment_generation;
            let target = TerminalScrollTarget {
                element: resident.element.clone(),
                visible_rows: ROWS,
                line_height: 10.0,
                session: id.clone(),
                pane_tx: pane.pane_tx.clone(),
            };
            let range = |target: &TerminalScrollTarget| {
                f32::from(diri_ui::ScrollTarget::max_offset(target).y)
            };

            // Read 500 rows of history, then follow output back to the live
            // edge, where the viewport forgets its geometry.
            resident.element.adopt_history_geometry(500, 510, 1, ROWS);
            assert!(resident.element.set_view_offset(40, ROWS));
            assert_eq!(range(&target), 5_000.0);
            assert!(resident.element.scroll_to_live(ROWS));
            assert_eq!(
                range(&target),
                5_000.0,
                "the knob was sized for one screen of history at the bottom"
            );

            // A probe sent while the knob shows tracks output that has
            // scrolled more rows into history since.
            pane.handle_pane_event(
                PaneEvent::HistoryExtent(id.clone(), generation, Some(800)),
                window,
                cx,
            );
            assert_eq!(range(&target), 8_000.0);

            // A reply addressed to a predecessor is dropped.
            pane.handle_pane_event(
                PaneEvent::HistoryExtent(id.clone(), generation.wrapping_add(1), Some(5)),
                window,
                cx,
            );
            assert_eq!(range(&target), 8_000.0);
        });
    }

    fn scrollback_reply(first: i64, live: i64, seq: u64) -> diri_proto::ReadScrollbackCellsResult {
        let rows: Vec<_> = (first..live)
            .map(|_| vec![GridCell::default(); 8])
            .collect();
        diri_proto::ReadScrollbackCellsResult {
            metadata: Vec::new(),
            payload: diri_proto::grid::GridRowCodec::encode_rows(&rows).expect("encoded rows"),
            first_row: first,
            row_count: live - first,
            live_start_row: live,
            total_rows: live + 10,
            cols: 8,
            content_seq: seq,
        }
    }

    #[gpui::test]
    fn late_scrollback_replies_cannot_mutate_a_reselected_session(cx: &mut TestAppContext) {
        const ROWS: usize = 10;
        let runtime = Arc::new(StoreRuntime::inert());
        // Never driven: every fetch this pane starts stays paused in flight,
        // and the test delivers the replies by hand.
        let tokio = Arc::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("test runtime"),
        );
        let mut reselected = fixture_session();
        reselected.id = SessionId::new("reselected");
        let mut other = fixture_session();
        other.id = SessionId::new("other");
        {
            let mut store = runtime.store.write().expect("session store lock poisoned");
            store.upsert_session(reselected.clone());
            store.upsert_session(other.clone());
            store.select(reselected.id.clone());
        }

        let runtime_for_view = Arc::clone(&runtime);
        let (pane, cx) = cx.add_window_view(move |window, cx| {
            TerminalPane::new(runtime_for_view, tokio, window, cx)
        });

        // R1: the first resident scrolls into history and asks for rows.
        let old_generation = pane.update(cx, |pane, _| {
            let resident = &pane.residents[&reselected.id];
            resident.element.adopt_history_geometry(100, 110, 1, ROWS);
            assert!(resident.element.set_view_offset(10, ROWS));
            let generation = resident.attachment_generation;
            pane.pump_scrollback_fetch(&reselected.id, ROWS);
            assert!(
                pane.residents[&reselected.id]
                    .element
                    .begin_scrollback_fetch(ROWS)
                    .is_none(),
                "R1 is in flight"
            );
            generation
        });

        for id in [other.id.clone(), reselected.id.clone()] {
            runtime
                .store
                .write()
                .expect("session store lock poisoned")
                .select(id);
            pane.update_in(cx, |pane, window, cx| {
                pane.reconcile_store_change(window, cx);
            });
        }

        pane.update_in(cx, |pane, window, cx| {
            // R2: the replacement resident reads a different stretch of a
            // history that has since grown.
            let resident = &pane.residents[&reselected.id];
            let new_generation = resident.attachment_generation;
            assert_ne!(new_generation, old_generation);
            resident.element.adopt_history_geometry(200, 210, 2, ROWS);
            assert!(resident.element.set_view_offset(10, ROWS));
            pane.pump_scrollback_fetch(&reselected.id, ROWS);

            // R1 fails late. Requeueing it here would let a second request
            // start while R2 is still in flight.
            pane.handle_pane_event(
                PaneEvent::ScrollbackFailed(reselected.id.clone(), old_generation),
                window,
                cx,
            );
            let element = &pane.residents[&reselected.id].element;
            assert!(
                element.begin_scrollback_fetch(ROWS).is_none(),
                "a stale failure cleared the replacement's in-flight request"
            );

            // R1 succeeds late, carrying the old geometry and rows.
            pane.handle_pane_event(
                PaneEvent::ScrollbackCells(
                    reselected.id.clone(),
                    old_generation,
                    scrollback_reply(80, 100, 1),
                    ROWS,
                ),
                window,
                cx,
            );
            let element = &pane.residents[&reselected.id].element;
            let viewport = element.viewport();
            assert_eq!(
                viewport.cached_row_count(),
                0,
                "a stale reply seeded the replacement's row cache"
            );
            assert_eq!(viewport.live_start_row(), 200);
            assert_eq!(viewport.view_offset(), 10);
            assert!(
                element.begin_scrollback_fetch(ROWS).is_none(),
                "a stale reply cleared the replacement's in-flight request"
            );

            // R2 still completes normally.
            pane.handle_pane_event(
                PaneEvent::ScrollbackCells(
                    reselected.id.clone(),
                    new_generation,
                    scrollback_reply(180, 200, 2),
                    ROWS,
                ),
                window,
                cx,
            );
            let viewport = pane.residents[&reselected.id].element.viewport();
            assert_eq!(viewport.cached_row_count(), 20);
            assert_eq!(viewport.live_start_row(), 200);
            assert_eq!(viewport.view_offset(), 10);
        });
    }

    #[test]
    fn needs_input_glyph_preserves_destructive_risk() {
        let mut session = fixture_session();
        session.status = SessionStatus::NeedsInput(NeedsInputKind::Permission);
        session.needs_input = Some(NeedsInputDetail {
            kind: NeedsInputKind::Permission,
            source: NeedsInputSource::ClaudePermissionHook,
            tool_name: Some("Bash".to_owned()),
            summary: "Approve command".to_owned(),
            prompt_excerpt: None,
            options: None,
            risk_hint: RiskHint::Destructive,
            occurred_at: DateMillis(2.0),
        });
        assert_eq!(
            status_state(&session),
            StatusState::NeedsInput { destructive: true }
        );
    }

    #[test]
    fn daemon_restart_exit_copy_matches_reference() {
        let mut session = fixture_session();
        session.status = SessionStatus::Exited(ExitInfo {
            reason: ExitReason::DaemonRestart,
            code: None,
            signal: None,
        });
        assert_eq!(
            exit_description(&session),
            "Session ended when the daemon restarted"
        );
    }

    #[test]
    fn terminal_command_navigation_preserves_app_and_modified_shortcuts() {
        for key in [
            "cmd-a",
            "cmd-c",
            "cmd-v",
            "cmd-f",
            "cmd-alt-left",
            "cmd-shift-left",
            "cmd-ctrl-right",
            "left",
        ] {
            assert_eq!(
                terminal_command_navigation(&Keystroke::parse(key).unwrap()),
                None,
                "{key}"
            );
        }
    }

    #[test]
    fn gpui_key_adapter_feeds_existing_terminal_encoder() {
        let event = KeyDownEvent {
            keystroke: Keystroke::parse("up").unwrap(),
            is_held: false,
            prefer_character_input: false,
        };
        let mapped = terminal_key_event(&event).unwrap();
        assert_eq!(
            encode_key(&mapped, TermModifiers::default(), TermInputModes::default()),
            b"\x1b[A"
        );

        let command_backspace = KeyDownEvent {
            keystroke: Keystroke {
                modifiers: Modifiers {
                    platform: true,
                    ..Modifiers::default()
                },
                key: "backspace".to_owned(),
                key_char: None,
            },
            is_held: false,
            prefer_character_input: false,
        };
        let mapped = terminal_key_event(&command_backspace).unwrap();
        assert_eq!(
            encode_key(
                &mapped,
                TermModifiers {
                    cmd: true,
                    ..TermModifiers::default()
                },
                TermInputModes::default()
            ),
            [0x15]
        );
    }

    #[test]
    fn clipboard_image_entries_are_detected_before_text_paste() {
        let item = ClipboardItem::new_image(&Image {
            format: ImageFormat::Png,
            bytes: b"clipboard png".to_vec(),
            id: 7,
        });

        let (bytes, extension) = clipboard_image(&item).expect("image payload");
        assert_eq!(bytes, b"clipboard png");
        assert_eq!(extension, "png");
        assert_eq!(item.text(), None);
    }

    #[test]
    fn clipboard_paste_respects_the_childs_bracketed_paste_mode() {
        let text = "first command\nsecond command";

        assert_eq!(terminal_paste(text, false), text.as_bytes());
        assert_eq!(
            terminal_paste(text, true),
            b"\x1b[200~first command\nsecond command\x1b[201~"
        );
    }

    #[test]
    fn claude_image_drop_is_a_paste_even_when_recovered_modes_are_missing() {
        let directory = tempfile::tempdir().expect("drop fixture");
        let path = directory
            .path()
            .join("CleanShot 2026-09-05 at 9\u{202f}.40.35@2x.png");
        std::fs::write(&path, b"image fixture").expect("write fixture");
        let plan = plan_terminal_drop(std::slice::from_ref(&path), false);
        let Some(TerminalDropAction::Paste(text)) = plan.action else {
            panic!("a readable local image must reach the terminal");
        };
        let expected = paste(&terminal_drop_text([path.to_str().unwrap()]), true);
        for reported_mode in [false, true] {
            assert_eq!(
                terminal_file_paste(&text, reported_mode, Some(&ProtoAgentKind::CLAUDE_CODE)),
                expected,
                "Claude attaches images only on its paste path, including after bounded log recovery"
            );
        }
    }

    #[test]
    fn other_file_drop_targets_keep_their_negotiated_paste_mode() {
        let text = terminal_drop_text(["/tmp/Screen Shot.png", "/tmp/notes.txt"]);
        for kind in [
            None,
            Some(&ProtoAgentKind::SHELL),
            Some(&ProtoAgentKind::CODEX),
        ] {
            for reported_mode in [false, true] {
                assert_eq!(
                    terminal_file_paste(&text, reported_mode, kind),
                    paste(&text, reported_mode),
                );
            }
        }
    }

    #[test]
    fn reattachment_drops_stale_bracketed_paste_until_fresh_modes_arrive() {
        assert!(bracketed_paste_after_attachment_state(
            true,
            AttachmentState::Live
        ));
        assert!(!bracketed_paste_after_attachment_state(
            true,
            AttachmentState::Reconnecting
        ));
        assert!(!bracketed_paste_after_attachment_state(
            true,
            AttachmentState::Attaching
        ));
    }

    #[test]
    fn unselected_terminal_damage_updates_its_buffer_without_repainting_the_window() {
        let selected = SessionId::new("selected");
        let background = SessionId::new("background");

        // Selected session damage always paints, including when the window is
        // unfocused-but-visible on another monitor. GPUI occlusion handles
        // truly hidden windows.
        assert!(terminal_damage_should_repaint(
            Some(&selected),
            &selected,
            true
        ));
        assert!(!terminal_damage_should_repaint(
            Some(&selected),
            &background,
            true
        ));
        assert!(!terminal_damage_should_repaint(
            Some(&selected),
            &selected,
            false
        ));
    }

    #[test]
    fn protocol_grid_never_exceeds_the_columns_that_can_be_painted() {
        let metrics =
            CellMetrics::from_measurements(px(7.75), px(10.0), px(3.0), px(1.0), gpui::FontId(7));
        // A fractional-width boundary where the window estimate reports ten
        // columns, but the actual grid content box is three border pixels
        // narrower and can paint only nine.
        let reported = estimated_grid_size(101.5, 100.0, Metrics::TITLE_BAR, 0.0, metrics);
        let painted = metrics.cols_for_width(px(101.5
            - GRID_HORIZONTAL_PADDING
            - GRID_LAYOUT_HORIZONTAL_CHROME));

        assert!(
            reported.0 <= painted,
            "reported {} columns but only {painted} fit",
            reported.0
        );
    }
}
