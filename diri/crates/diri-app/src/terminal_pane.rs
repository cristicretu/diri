//! Terminal pane composition.
//!
//! The daemon remains authoritative: this module only composes
//! `diri-client::SessionAttachment`, `diri-term`, and the T9 session store.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use diri_client::attachment::{SessionAttachment, TerminalChunk};
use diri_proto::grid::GridUpdate;
use diri_proto::{
    AgentKind as ProtoAgentKind, ArtifactKind, ExitReason, PrCheck, PullRequestStatus,
    Resumability, RiskHint, SessionArtifact, SessionId, SessionRecord, SessionStatus,
};
use diri_term::buffer::GridBuffer;
use diri_term::element::{SharedGridBuffer, TerminalElement, TerminalReference};
use diri_term::find::{FindSnapshot, SearchRequest, TerminalFindModel};
use diri_term::keys::{
    Key as TermKey, KeyEvent as TermKeyEvent, Modifiers as TermModifiers, NamedKey, TermInputModes,
    encode_key, paste,
};
use diri_term::metrics::CellMetrics;
use diri_term::scrollback::{WheelDelta, WheelEvent, WheelRoute};
use diri_term::theme::TermTheme;
use diri_ui::{
    AgentKind as UiAgentKind, Fill, FloatingSurface, Ink, Metrics, Radius, SemanticColors,
    StatusGlyph, StatusState, Typo,
};
use gpui::{
    AnyElement, ClickEvent, ClipboardEntry, ClipboardItem, Context, Entity, EventEmitter,
    FocusHandle, KeyDownEvent, KeyUpEvent, ModifiersChangedEvent, MouseButton, Render, ScrollDelta,
    ScrollWheelEvent, SharedString, StatefulInteractiveElement, Task, Window, div, font,
    prelude::*, px, rgba,
};
use tokio::runtime::Handle;
use tokio::sync::mpsc;

use crate::clipboard_transfer::StagedClipboardImage;
use crate::commands::{
    CloseFind, CopySelection, FindNext, FindPrevious, OpenFind, Paste, ResetZoom, TERMINAL_CONTEXT,
    ToggleInspector, ToggleSidebar, ZoomIn, ZoomOut,
};
use crate::macos::sf_symbols::{SymbolWeight, sf_symbol, sf_symbol_weighted};
use crate::navigation::{NavigationOverlay, query_label};
use crate::query_editor::{self, ClipboardEdit, Edit, QueryEditor};
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
const TOOLBAR_MAX_VISIBLE_LINKS: usize = 4;
const TOOLBAR_LINK_MAX_WIDTH: f32 = 176.0;
const TOOLBAR_OVERFLOW_WIDTH: f32 = 50.0;
const REATTACH_DELAY: Duration = Duration::from_millis(500);
const PANE_EVENT_QUEUE_CAPACITY: usize = 256;
/// How often a live drag is allowed to push a new PTY geometry. Matched to the
/// daemon's coalesced grid flush (also 8ms): resizing faster produces frames
/// the client can never see, resizing slower makes the drag look like it snaps
/// at the end instead of reflowing under the cursor.
const RESIZE_CADENCE: Duration = Duration::from_millis(8);
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
    OpenFileReference {
        reference: String,
        cwd: String,
        session_id: SessionId,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChipTint {
    Red,
    Orange,
    Yellow,
    Green,
    Purple,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PaneChip {
    pub id: String,
    pub label: String,
    pub system_image: &'static str,
    pub open_url: Option<String>,
    pub copy_string: String,
    pub tint: Option<ChipTint>,
    pub help: String,
    pub checks: Option<PullRequestStatus>,
}

impl PaneChip {
    pub fn for_session(session: &SessionRecord) -> Vec<Self> {
        let mut result = Vec::new();
        let artifacts = session.artifacts.as_deref().unwrap_or_default();
        let statuses = session.pull_requests.as_deref().unwrap_or_default();
        let pull_requests = artifacts
            .iter()
            .filter(|artifact| artifact.kind == ArtifactKind::PullRequest)
            .map(|artifact| {
                (
                    artifact,
                    statuses.iter().find(|status| status.url == artifact.url),
                )
            })
            .collect::<Vec<_>>();

        // Primary PR destinations are the highest-value links, so expose all
        // of them before their supporting checks/comments or generic URLs.
        for (artifact, status) in &pull_requests {
            result.push(Self::from_artifact(artifact, *status));
        }
        for (artifact, status) in pull_requests {
            if let Some(status) = status {
                if let Some(checks) = Self::checks_chip(artifact, status) {
                    result.push(checks);
                }
                if let Some(comments) = Self::comments_chip(artifact, status) {
                    result.push(comments);
                }
            }
        }
        for artifact in artifacts
            .iter()
            .filter(|artifact| artifact.kind != ArtifactKind::PullRequest)
        {
            result.push(Self::from_artifact(artifact, None));
        }
        for port in session.listening_ports.as_deref().unwrap_or_default() {
            let url = format!("http://localhost:{}", port.port);
            result.push(Self {
                id: format!("port-{}", port.port),
                label: format!(":{}", port.port),
                system_image: "network",
                open_url: Some(url.clone()),
                copy_string: url.clone(),
                tint: None,
                help: url,
                checks: None,
            });
        }
        result
    }

    fn from_artifact(artifact: &SessionArtifact, pr: Option<&PullRequestStatus>) -> Self {
        match artifact.kind {
            ArtifactKind::PullRequest => {
                let mut label = pr_number(&artifact.url)
                    .map_or_else(|| "PR".to_owned(), |number| format!("PR #{number}"));
                if let Some(pr) = pr
                    && pr.additions + pr.deletions > 0
                {
                    label.push_str(&format!(" +{} −{}", pr.additions, pr.deletions));
                }
                Self {
                    id: format!("art-{}", artifact.url),
                    label,
                    system_image: pr.map_or("arrow.triangle.pull", |pr| match pr.state.as_str() {
                        "MERGED" => "arrow.triangle.merge",
                        "CLOSED" => "xmark.circle",
                        _ => "arrow.triangle.pull",
                    }),
                    open_url: Some(artifact.url.clone()),
                    copy_string: artifact.url.clone(),
                    tint: pr.and_then(pr_tint),
                    help: pr.map_or_else(|| artifact.url.clone(), pr_help),
                    checks: None,
                }
            }
            ArtifactKind::LinearIssue => Self::quiet_artifact(
                artifact,
                linear_key(&artifact.url).unwrap_or_else(|| "Linear".to_owned()),
                "checklist",
            ),
            ArtifactKind::Preview => Self::quiet_artifact(
                artifact,
                url_port(&artifact.url)
                    .map_or_else(|| url_host(&artifact.url), |port| format!(":{port}")),
                "network",
            ),
            ArtifactKind::Link | ArtifactKind::Unknown => {
                Self::quiet_artifact(artifact, url_host(&artifact.url), "link")
            }
        }
    }

    fn quiet_artifact(
        artifact: &SessionArtifact,
        label: String,
        system_image: &'static str,
    ) -> Self {
        Self {
            id: format!("art-{}", artifact.url),
            label,
            system_image,
            open_url: Some(artifact.url.clone()),
            copy_string: artifact.url.clone(),
            tint: None,
            help: artifact.url.clone(),
            checks: None,
        }
    }

    fn checks_chip(artifact: &SessionArtifact, pr: &PullRequestStatus) -> Option<Self> {
        let total = pr.checks_passed + pr.checks_failed + pr.checks_pending;
        if total <= 0 {
            return None;
        }
        let (system_image, tint) = if pr.checks_failed > 0 {
            ("xmark.circle.fill", ChipTint::Red)
        } else if pr.checks_pending > 0 {
            ("clock.fill", ChipTint::Yellow)
        } else {
            ("checkmark.circle.fill", ChipTint::Green)
        };
        let mut states = vec![format!("{} passed", pr.checks_passed)];
        if pr.checks_failed > 0 {
            states.push(format!("{} failed", pr.checks_failed));
        }
        if pr.checks_pending > 0 {
            states.push(format!("{} running", pr.checks_pending));
        }
        Some(Self {
            id: format!("art-{}-checks", artifact.url),
            label: format!("{}/{total}", pr.checks_passed),
            system_image,
            open_url: Some(format!("{}/checks", artifact.url.trim_end_matches('/'))),
            copy_string: artifact.url.clone(),
            tint: Some(tint),
            help: format!("Checks: {}", states.join(" · ")),
            checks: Some(pr.clone()),
        })
    }

    fn comments_chip(artifact: &SessionArtifact, pr: &PullRequestStatus) -> Option<Self> {
        let count = pr.comment_count + pr.review_count;
        let (label, tint) = if let Some(total) = pr.total_threads.filter(|total| *total > 0) {
            let resolved = pr.resolved_threads.unwrap_or(0);
            (
                format!("{resolved}/{total}"),
                Some(if resolved == total {
                    ChipTint::Green
                } else {
                    ChipTint::Orange
                }),
            )
        } else if count > 0 {
            (count.to_string(), None)
        } else {
            return None;
        };
        Some(Self {
            id: format!("art-{}-comments", artifact.url),
            label,
            system_image: "bubble.left",
            open_url: Some(artifact.url.clone()),
            copy_string: artifact.url.clone(),
            tint,
            help: comments_help(pr),
            checks: None,
        })
    }
}

fn toolbar_chip_width(chip: &PaneChip) -> f32 {
    let label_width = chip.label.chars().count().min(24) as f32 * 6.2;
    (label_width + 34.0).clamp(68.0, TOOLBAR_LINK_MAX_WIDTH)
}

fn toolbar_visible_chip_count(
    chips: &[PaneChip],
    viewport_width: f32,
    sidebar_visible: bool,
) -> usize {
    if chips.is_empty() {
        return 0;
    }

    // Protect a readable session title, branch/host metadata, agent identity,
    // and (when needed) the macOS traffic-light lane + sidebar reveal button.
    let fixed_chrome = if sidebar_visible { 560.0 } else { 673.0 };
    let budget = (viewport_width - fixed_chrome).clamp(TOOLBAR_OVERFLOW_WIDTH, 720.0);
    let limit = chips.len().min(TOOLBAR_MAX_VISIBLE_LINKS);
    let mut used = 0.0;
    let mut visible = 0;

    for (index, chip) in chips.iter().take(limit).enumerate() {
        let gap = if index == 0 {
            0.0
        } else {
            Metrics::TOOLBAR_COMPACT_GAP
        };
        let candidate = used + gap + toolbar_chip_width(chip);
        let overflow = if index + 1 < chips.len() {
            Metrics::TOOLBAR_COMPACT_GAP + TOOLBAR_OVERFLOW_WIDTH
        } else {
            0.0
        };
        if candidate + overflow > budget {
            break;
        }
        used = candidate;
        visible += 1;
    }

    visible
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AttachmentState {
    Attaching,
    Live,
    Reconnecting,
}

enum AttachmentCommand {
    Input(Vec<u8>),
    Resize(u16, u16),
    Scroll {
        direction: u8,
        lines: u16,
        col: u16,
        row: u16,
    },
    Close,
}

#[derive(Clone)]
struct AttachmentControl {
    tx: mpsc::UnboundedSender<AttachmentCommand>,
}

impl AttachmentControl {
    fn input(&self, bytes: Vec<u8>) {
        if bytes.is_empty() {
            return;
        }
        let _ = self.tx.send(AttachmentCommand::Input(bytes));
    }

    fn resize(&self, cols: u16, rows: u16) {
        let _ = self.tx.send(AttachmentCommand::Resize(cols, rows));
    }

    fn scroll(&self, direction: u8, lines: u16, col: u16, row: u16) {
        let _ = self.tx.send(AttachmentCommand::Scroll {
            direction,
            lines,
            col,
            row,
        });
    }

    fn close(&self) {
        let _ = self.tx.send(AttachmentCommand::Close);
    }
}

enum PaneEvent {
    AttachmentState(SessionId, AttachmentState),
    Chunk(SessionId, TerminalChunk),
    GridBatch(SessionId, Vec<GridUpdate>),
    FindSnapshot(SessionId, SearchRequest, FindSnapshot),
    ScrollbackCells(SessionId, diri_proto::ReadScrollbackCellsResult, usize),
    ScrollbackFailed(SessionId),
    ClipboardUploadFinished(SessionId, Result<String, String>),
}

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

#[derive(Default)]
struct PaneMailboxState {
    events: VecDeque<PaneEvent>,
    /// At most a new baseline plus its final trailing diff. The two-frame
    /// boundary is observable by resize reflow holds and must not be erased.
    grids: HashMap<SessionId, Vec<GridUpdate>>,
    grid_order: VecDeque<SessionId>,
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
            PaneEvent::Chunk(id, TerminalChunk::Grid(update)) => {
                if let Some(batch) = state.grids.get_mut(&id) {
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
                } else {
                    state.grid_order.push_back(id.clone());
                    state.grids.insert(id, vec![update]);
                }
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
        batch.extend(state.events.drain(..));
        while let Some(id) = state.grid_order.pop_front() {
            if let Some(updates) = state.grids.remove(&id) {
                batch.push(PaneEvent::GridBatch(id, updates));
            }
        }
        true
    }
}

struct ResidentTerminal {
    element: TerminalElement,
    attachment: AttachmentControl,
    attachment_state: AttachmentState,
    find: Option<TerminalFindModel>,
    /// The editable text behind `find`'s query, so ⌘F gets the same caret,
    /// selection, and readline keys as the other query fields.
    find_query: QueryEditor,
    last_size: (u16, u16),
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

impl Drop for ResidentTerminal {
    fn drop(&mut self) {
        self.attachment.close();
    }
}

pub struct TerminalPane {
    runtime: Arc<StoreRuntime>,
    _tokio_owner: Arc<tokio::runtime::Runtime>,
    tokio: Handle,
    residents: HashMap<SessionId, ResidentTerminal>,
    /// Last-known grids of recently evicted sessions, most recent last.
    /// Selecting a session paints its parked grid on the very first frame
    /// while the fresh attachment round-trips; the attach's full snapshot
    /// then overwrites the same buffer in place. This is what makes session
    /// switching read as instant with a residency of one.
    parked_grids: Vec<(SessionId, SharedGridBuffer)>,
    pane_tx: PaneEventSender,
    focus: FocusHandle,
    glyphs: HashMap<SessionId, Entity<StatusGlyph>>,
    open_checks_for: Option<String>,
    overflow_open: bool,
    /// Paced PTY resizes: window and sidebar drags relayout every frame, but
    /// sustained grid frames leave the daemon at up to 120 Hz, so intermediate
    /// sizes coalesce onto that cadence (see [`RESIZE_CADENCE`]).
    pending_resizes: HashMap<SessionId, (u16, u16)>,
    resize_flush: Option<Task<()>>,
    /// A cadence tick is already armed; further changes fold into it instead of
    /// rescheduling (which is what used to starve the flush during a drag).
    resize_flush_armed: bool,
    last_resize_sent: Option<Instant>,
    /// Grids held still while a column change round-trips. Keyed by session id
    /// so a hold follows the session rather than the pane: selection can move
    /// on mid-hold, and the parked frames still belong to the session that was
    /// resized.
    reflow_holds: HashMap<SessionId, ReflowHold>,
    started_at: Instant,
    session_source: SessionSource,
    /// Last selection observed by the primary pane. Spawn responses select the
    /// daemon-created id asynchronously, so this transition is also the
    /// reliable point at which keyboard focus can leave the picker.
    observed_selected_id: Option<SessionId>,
    viewport: Option<TerminalViewport>,
    sidebar_visible: bool,
    inspector_open: bool,
    navigation: Option<Entity<NavigationOverlay>>,
    utility_surfaces: Option<Entity<UtilitySurfaces>>,
    local_clipboard_images: Vec<StagedClipboardImage>,
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
            window,
            cx,
        )
    }

    fn new_with_source(
        runtime: Arc<StoreRuntime>,
        tokio_owner: Arc<tokio::runtime::Runtime>,
        session_source: SessionSource,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus = cx.focus_handle();
        if matches!(session_source, SessionSource::FollowSelection) {
            window.focus(&focus, cx);
        }
        let (pane_tx, mut pane_rx) = pane_event_channel();
        let pane_events = cx.spawn_in(window, async move |this, cx| {
            let mut batch = Vec::new();
            while pane_rx.recv_batch(&mut batch).await {
                if this
                    .update_in(cx, |this, window, cx| {
                        for event in batch.drain(..) {
                            this.handle_pane_event(event, window, cx);
                        }
                    })
                    .is_err()
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
                        if this
                            .update_in(cx, |this, window, cx| {
                                this.reconcile_store_change(window, cx);
                            })
                            .is_err()
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
                runtime
                    .store
                    .read()
                    .expect("session store lock poisoned")
                    .selected_session_id()
                    .cloned()
            })
            .flatten();
        let mut pane = Self {
            runtime,
            _tokio_owner: tokio_owner,
            tokio,
            residents: HashMap::new(),
            parked_grids: Vec::new(),
            pane_tx,
            focus,
            glyphs: HashMap::new(),
            open_checks_for: None,
            overflow_open: false,
            pending_resizes: HashMap::new(),
            resize_flush: None,
            resize_flush_armed: false,
            last_resize_sent: None,
            reflow_holds: HashMap::new(),
            started_at: Instant::now(),
            session_source,
            observed_selected_id,
            viewport: None,
            sidebar_visible: true,
            inspector_open: false,
            navigation: None,
            utility_surfaces: None,
            local_clipboard_images: Vec::new(),
            _pane_events: pane_events,
            _store_changes: store_changes,
        };
        pane.reconcile_residency();
        pane.sync_status_glyphs(pane.current_colors(), window, cx);
        pane
    }

    fn reconcile_residency(&mut self) {
        let store = self
            .runtime
            .store
            .read()
            .expect("session store lock poisoned");
        let resident_ids: HashSet<_> = match &self.session_source {
            SessionSource::FollowSelection => {
                store.terminal_residency().resident().cloned().collect()
            }
            SessionSource::Fixed(id) if store.sessions().contains_key(id) => {
                HashSet::from([id.clone()])
            }
            SessionSource::Fixed(_) => HashSet::new(),
        };
        // A parked grid for a session the store no longer lists is dead
        // weight; one for a session that just became resident is superseded
        // below by promotion.
        self.parked_grids
            .retain(|(id, _)| store.sessions().contains_key(id));
        drop(store);
        // Park the last-known grid of every session about to be evicted, so
        // re-selecting it paints instantly instead of flashing blank while
        // the fresh attachment round-trips.
        for (id, resident) in &self.residents {
            if resident_ids.contains(id) {
                continue;
            }
            self.parked_grids.retain(|(parked, _)| parked != id);
            self.parked_grids
                .push((id.clone(), resident.element.buffer()));
        }
        if self.parked_grids.len() > PARKED_GRID_CAP {
            let excess = self.parked_grids.len() - PARKED_GRID_CAP;
            self.parked_grids.drain(..excess);
        }
        self.residents.retain(|id, _| resident_ids.contains(id));
        // A hold outliving its resident would park frames belonging to a
        // session id that has been re-attached since, and paint them into a
        // grid that never asked for them.
        let residents = &self.residents;
        self.reflow_holds.retain(|id, _| residents.contains_key(id));

        let socket = self.runtime.client().socket_path().to_path_buf();
        for id in resident_ids {
            if self.residents.contains_key(&id) {
                continue;
            }
            let mut mono = font(crate::fonts::mono_family());
            mono.fallbacks = Some(gpui::FontFallbacks::from_fonts(vec![
                ".SF NS Mono".to_owned(),
                "Menlo".to_owned(),
                "Apple Symbols".to_owned(),
                "STIX Two Math".to_owned(),
                "Apple Color Emoji".to_owned(),
            ]));
            let parked = self
                .parked_grids
                .iter()
                .position(|(parked, _)| parked == &id)
                .map(|index| self.parked_grids.remove(index).1);
            let attachment = spawn_attachment(
                &self.tokio,
                socket.clone(),
                id.clone(),
                self.pane_tx.clone(),
            );
            let ime_attachment = attachment.clone();
            let element = match parked {
                // The parked cells paint on the first frame; the attach's
                // full snapshot overwrites the same shared buffer moments
                // later, so stale content lives for one round-trip at most.
                Some(buffer) => TerminalElement::new(buffer),
                None => TerminalElement::with_buffer(GridBuffer::default()),
            }
            .font(mono)
            .focus_handle(self.focus.clone())
            .on_text_input(move |text| ime_attachment.input(text.as_bytes().to_vec()));
            self.residents.insert(
                id,
                ResidentTerminal {
                    element,
                    attachment,
                    attachment_state: AttachmentState::Attaching,
                    find: None,
                    find_query: QueryEditor::default(),
                    last_size: (0, 0),
                },
            );
        }
    }

    fn reconcile_store_change(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let selected_id = matches!(self.session_source, SessionSource::FollowSelection)
            .then(|| {
                self.runtime
                    .store
                    .read()
                    .expect("session store lock poisoned")
                    .selected_session_id()
                    .cloned()
            })
            .flatten();
        let selection_changed = selected_id != self.observed_selected_id;
        self.observed_selected_id = selected_id.clone();

        self.reconcile_residency();
        self.sync_status_glyphs(self.current_colors(), window, cx);

        // Explicit sidebar clicks already focus through SessionActivated, but
        // successful spawns select their daemon-assigned id on the async store
        // path. Following the selection here covers both RPC/event orderings
        // and avoids trying to focus a terminal before its id exists.
        if selection_changed && selected_id.is_some() {
            window.focus(&self.focus, cx);
        }
        cx.notify();
    }

    pub fn resident_buffers(&mut self) -> HashMap<SessionId, SharedGridBuffer> {
        self.reconcile_residency();
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

    pub fn focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus, cx);
    }

    pub fn set_viewport(&mut self, viewport: TerminalViewport, cx: &mut Context<Self>) {
        if self.viewport == Some(viewport) {
            return;
        }
        self.viewport = Some(viewport);
        cx.notify();
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

    pub fn is_focused(&self, window: &Window) -> bool {
        self.focus.is_focused(window)
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
        crate::app_theme::colors(&store.preferences().terminal_theme)
    }

    fn handle_pane_event(&mut self, event: PaneEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event {
            PaneEvent::AttachmentState(id, state) => {
                if let Some(resident) = self.residents.get_mut(&id) {
                    resident.attachment_state = state;
                }
                if self.selected_id().as_ref() == Some(&id) {
                    cx.notify();
                }
            }
            PaneEvent::Chunk(id, TerminalChunk::Grid(update)) => {
                if let Some(hold) = self.reflow_holds.get_mut(&id) {
                    if hold.park(update) {
                        self.release_reflow_hold(&id, window, cx);
                    }
                    return;
                }
                self.apply_grid_updates(id, [update], window, cx);
            }
            PaneEvent::GridBatch(id, updates) => {
                if self.reflow_holds.contains_key(&id) {
                    for update in updates {
                        let release = self
                            .reflow_holds
                            .get_mut(&id)
                            .is_some_and(|hold| hold.park(update));
                        if release {
                            self.release_reflow_hold(&id, window, cx);
                        }
                    }
                    return;
                }
                self.apply_grid_updates(id, updates, window, cx);
            }
            PaneEvent::Chunk(
                id,
                TerminalChunk::Modes {
                    alt_screen,
                    mouse_reporting,
                },
            ) => {
                if let Some(resident) = self.residents.get_mut(&id) {
                    resident.element.set_modes(alt_screen, mouse_reporting);
                }
                if self.selected_id().as_ref() == Some(&id) {
                    cx.notify();
                }
            }
            PaneEvent::Chunk(_, TerminalChunk::Pong) => {}
            PaneEvent::FindSnapshot(id, request, snapshot) => {
                let visible = self.selected_id().as_ref() == Some(&id);
                if let Some(resident) = self.residents.get_mut(&id)
                    && let Some(find) = resident.find.as_mut()
                    && resident
                        .element
                        .apply_find_snapshot(find, &request, snapshot)
                {
                    resident.element.sync_find_highlights(find);
                    if visible {
                        cx.notify();
                    }
                }
            }
            PaneEvent::ScrollbackCells(id, result, visible_rows) => {
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
            PaneEvent::ScrollbackFailed(id) => {
                if let Some(resident) = self.residents.get_mut(&id) {
                    resident.element.fail_scrollback_fetch();
                }
                if self.selected_id().as_ref() == Some(&id) {
                    cx.notify();
                }
            }
            PaneEvent::ClipboardUploadFinished(id, result) => match result {
                Ok(remote_path) => {
                    if let Some(resident) = self.residents.get(&id) {
                        resident.attachment.input(paste(&remote_path, false));
                    }
                }
                Err(error) => eprintln!("diri: clipboard image upload failed: {error}"),
            },
        }
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
            self.schedule_find(id, Duration::from_millis(100), window, cx);
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
    fn hold_reflow(&mut self, id: SessionId, window: &mut Window, cx: &mut Context<Self>) {
        let held = id.clone();
        let release = cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(REFLOW_HOLD).await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.release_reflow_hold(&held, window, cx);
            });
        });
        let parked = self
            .reflow_holds
            .remove(&id)
            .map_or_else(Vec::new, |hold| hold.parked);
        self.reflow_holds.insert(
            id,
            ReflowHold {
                parked,
                saw_snapshot: false,
                _release: release,
            },
        );
    }

    /// Ends a hold and paints everything it parked as a single frame.
    fn release_reflow_hold(&mut self, id: &SessionId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(hold) = self.reflow_holds.remove(id) else {
            return;
        };
        self.apply_grid_updates(id.clone(), hold.parked, window, cx);
    }

    fn request_terminal_repaint(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        // GPUI coalesces dirty entities and presents them from the platform's
        // CVDisplayLink. A second fixed-rate timer here can only miss the next
        // display deadline (and capped ProMotion at 60 fps), so terminal
        // damage has exactly one pacing authority: the display itself.
        cx.notify();
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
            let _ = this.update_in(cx, |this, _window, _cx| this.start_due_find(&id));
        })
        .detach();
    }

    fn start_due_find(&mut self, id: &SessionId) {
        let now = self.started_at.elapsed();
        let Some(request) = self
            .residents
            .get_mut(id)
            .and_then(|resident| resident.find.as_mut())
            .and_then(|find| find.take_due_search(now))
        else {
            return;
        };
        let client = Arc::clone(self.runtime.client());
        let pane_tx = self.pane_tx.clone();
        let id = id.clone();
        self.tokio.spawn(async move {
            if let Ok(snapshot) = client.read_scrollback(&id).await {
                let _ = pane_tx.send(PaneEvent::FindSnapshot(id, request, snapshot.into()));
            }
        });
    }

    fn selected_id(&self) -> Option<SessionId> {
        match &self.session_source {
            SessionSource::FollowSelection => self
                .runtime
                .store
                .read()
                .expect("session store lock poisoned")
                .selected_session_id()
                .cloned(),
            SessionSource::Fixed(id) => Some(id.clone()),
        }
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
            resident.find = Some(TerminalFindModel::default());
            // Reopening keeps the last query but selects it, so ⌘F then typing
            // starts a new search while ⌘F then ⏎ repeats the old one.
            resident.find_query.select_all();
        }
        window.focus(&self.focus, cx);
        cx.stop_propagation();
        cx.notify();
    }

    fn close_find(&mut self, _: &CloseFind, _window: &mut Window, cx: &mut Context<Self>) {
        if self.close_find_for_selected() {
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
        resident.element.set_find_highlights(Vec::new());
        true
    }

    fn find_next(&mut self, _: &FindNext, _window: &mut Window, cx: &mut Context<Self>) {
        self.navigate_find(false, cx);
    }

    fn find_previous(&mut self, _: &FindPrevious, _window: &mut Window, cx: &mut Context<Self>) {
        self.navigate_find(true, cx);
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
        let grid_y = viewport.y + Metrics::TITLE_BAR + 2.0 + anchor;
        let col = ((f32::from(position.x) - grid_x) / f32::from(metrics.cell_width))
            .floor()
            .max(0.0) as usize;
        let row = ((f32::from(position.y) - grid_y) / f32::from(metrics.line_height))
            .floor()
            .max(0.0) as usize;
        Some((col, row))
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
        (height - Metrics::TITLE_BAR - GRID_VERTICAL_PADDING - GRID_LAYOUT_VERTICAL_CHROME).max(1.0)
    }

    fn copy_selection(&mut self, _: &CopySelection, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.selected_id() else {
            return;
        };
        let Some(resident) = self.residents.get(&id) else {
            return;
        };
        let text = resident.element.selected_text();
        if !text.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    fn paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
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

            let staged = match StagedClipboardImage::stage(bytes, extension) {
                Ok(staged) => staged,
                Err(error) => {
                    eprintln!("diri: could not stage clipboard image: {error}");
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
                    .selected_session()
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
                if let Some(resident) = self.residents.get(&id) {
                    resident.attachment.input(paste(&local_path, false));
                }
                self.local_clipboard_images.push(staged);
                if self.local_clipboard_images.len() > 32 {
                    self.local_clipboard_images.remove(0);
                }
            }
            cx.stop_propagation();
            cx.notify();
            return;
        }

        let Some(text) = item.text() else {
            return;
        };
        let now = self.started_at.elapsed();
        let Some(resident) = self.residents.get_mut(&id) else {
            return;
        };
        if let Some(find) = resident.find.as_mut() {
            resident.find_query.insert(&text);
            let query = resident.find_query.text().to_owned();
            find.set_query(query, now);
            self.schedule_find(id, Duration::from_millis(200), window, cx);
        } else {
            resident.attachment.input(paste(&text, false));
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

        let switcher_key = switcher_key(event);
        let switcher_handled = {
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
                        Edit::Local(local) => resident.find_query.apply(local),
                        Edit::Clipboard(ClipboardEdit::Copy) => {
                            query_editor::copy_selection(&resident.find_query, cx);
                            false
                        }
                        Edit::Clipboard(ClipboardEdit::Cut) => {
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
                        find.set_query(query, now);
                        self.schedule_find(id, Duration::from_millis(200), window, cx);
                    }
                }
            }
            cx.stop_propagation();
            cx.notify();
            return;
        }

        if event.keystroke.modifiers.platform && event.keystroke.key != "backspace" {
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
        let bytes = encode_key(&term_event, modifiers, TermInputModes::default());
        if bytes.is_empty() {
            cx.propagate();
        } else {
            resident.attachment.input(bytes);
            cx.stop_propagation();
        }
    }

    fn handle_key_up(&mut self, event: &KeyUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if matches!(event.keystroke.key.as_str(), "control" | "ctrl") {
            self.runtime
                .store
                .write()
                .expect("session store lock poisoned")
                .handle_switcher_modifiers_changed(false);
            cx.notify();
        }
    }

    fn handle_modifiers_changed(
        &mut self,
        event: &ModifiersChangedEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut store = self
            .runtime
            .store
            .write()
            .expect("session store lock poisoned");
        let was_visible = store.switcher_state().is_visible();
        store.handle_switcher_modifiers_changed(event.modifiers.control);
        if was_visible != store.switcher_state().is_visible() {
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
        let client = Arc::clone(self.runtime.client());
        let pane_tx = self.pane_tx.clone();
        let fetch_id = id.clone();
        self.tokio.spawn(async move {
            match client
                .read_scrollback_cells(&fetch_id, request.first_row, request.max_rows)
                .await
            {
                Ok(result) => {
                    let _ =
                        pane_tx.send(PaneEvent::ScrollbackCells(fetch_id, result, visible_rows));
                }
                Err(_) => {
                    let _ = pane_tx.send(PaneEvent::ScrollbackFailed(fetch_id));
                }
            }
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
        let grid_y = viewport.y + Metrics::TITLE_BAR + 2.0;
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
        let viewport = self.viewport.unwrap_or_else(|| {
            let bounds = window.inner_window_bounds().get_bounds();
            TerminalViewport {
                x: 0.0,
                y: 0.0,
                width: f32::from(bounds.size.width),
                height: f32::from(bounds.size.height),
            }
        });
        let metrics = CellMetrics::measure(
            window.text_system(),
            &font(crate::fonts::mono_family()),
            px(font_size),
        );
        let size = estimated_grid_size(viewport.width, viewport.height, 0.0, metrics);
        if let Some(resident) = self.residents.get_mut(&session.id)
            && resident.last_size != size
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
                    self.pending_resizes.insert(session.id.clone(), size);
                    return;
                }
                ResizePlan::Arm(delay) => delay,
            };
            self.pending_resizes.insert(session.id.clone(), size);
            self.resize_flush_armed = true;
            let timer = cx.background_executor().timer(delay);
            self.resize_flush = Some(cx.spawn(async move |this, cx| {
                timer.await;
                let _ = this.update(cx, |this, _cx| {
                    this.resize_flush_armed = false;
                    this.last_resize_sent = Some(Instant::now());
                    let pending = std::mem::take(&mut this.pending_resizes);
                    for (id, size) in pending {
                        if let Some(resident) = this.residents.get(&id) {
                            resident.attachment.resize(size.0, size.1);
                        }
                    }
                });
            }));
        }
    }

    fn render_sidebar_reveal_control(
        &self,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .flex_none()
            .flex()
            .items_center()
            .gap(px(Metrics::TOOLBAR_ITEM_GAP))
            // The visible lights need more breathing room than their native
            // frames imply, so this is an intentional optical safe area.
            .child(div().w(px(Metrics::TOOLBAR_TRAFFIC_LIGHT_LANE)).flex_none())
            .child(
                div()
                    .id("show-sidebar")
                    .debug_selector(|| "show-sidebar".into())
                    .size(px(Metrics::TOOLBAR_CONTROL_SIZE))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(Radius::BADGE))
                    .cursor_pointer()
                    .hover(move |button| button.bg(Fill::subtle(colors)))
                    .child(sf_symbol("sidebar.left", 15.0, colors.secondary))
                    .on_click(cx.listener(|_, _, window, cx| {
                        window.dispatch_action(Box::new(ToggleSidebar), cx);
                        cx.stop_propagation();
                    })),
            )
            .into_any_element()
    }

    fn render_header(
        &self,
        session: &SessionRecord,
        chips: &[PaneChip],
        visible_chip_count: usize,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let glyph = self.glyphs.get(&session.id).cloned();
        let branch = session.git_branch.clone();
        let host = session.host.as_ref().map(|host| {
            self.runtime
                .store
                .read()
                .expect("session store lock poisoned")
                .host_display_name(host)
        });
        let kind = ui_agent_kind(session.effective_kind());
        let shell_controls = matches!(self.session_source, SessionSource::FollowSelection);
        let show_sidebar = shell_controls && !self.sidebar_visible;
        let sidebar_reveal = show_sidebar.then(|| self.render_sidebar_reveal_control(colors, cx));
        let inspector_open = self.inspector_open;
        let visible_chip_count = visible_chip_count.min(chips.len());
        let overflow_count = chips.len().saturating_sub(visible_chip_count);
        let mut toolbar_links = div()
            .flex_none()
            .flex()
            .items_center()
            .gap(px(Metrics::TOOLBAR_COMPACT_GAP));
        for chip in chips.iter().take(visible_chip_count).cloned() {
            toolbar_links = toolbar_links.child(self.render_chip(chip, colors, cx));
        }
        if overflow_count > 0 {
            toolbar_links = toolbar_links.child(
                div()
                    .id("terminal-chip-overflow")
                    .h(px(Metrics::TOOLBAR_CHIP_HEIGHT))
                    .px(px(6.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(Metrics::TOOLBAR_COMPACT_GAP))
                    .rounded(px(Radius::CHIP))
                    .bg(Fill::subtle(colors))
                    .text_size(px(Typo::META.size))
                    .text_color(colors.secondary)
                    .cursor_pointer()
                    .hover(move |button| button.bg(colors.primary.alpha(0.10)))
                    .child(sf_symbol("ellipsis", 10.0, colors.secondary))
                    .child(format!("+{overflow_count}"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.overflow_open = !this.overflow_open;
                        this.open_checks_for = None;
                        cx.notify();
                        cx.stop_propagation();
                    })),
            );
        }
        div()
            .h(px(Metrics::TITLE_BAR))
            .flex_none()
            .px(px(Metrics::TOOLBAR_EDGE_INSET))
            .flex()
            .items_center()
            .justify_between()
            .bg(colors.sidebar_surface())
            .child(
                div()
                    .min_w(px(0.0))
                    .flex_1()
                    .flex()
                    .items_center()
                    .gap(px(Metrics::TOOLBAR_ITEM_GAP))
                    .overflow_hidden()
                    .when_some(sidebar_reveal, |title, control| title.child(control))
                    .child(sf_symbol("terminal", 15.0, colors.secondary))
                    .child(
                        div()
                            .min_w(px(0.0))
                            .flex_1()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_size(px(Typo::TITLE.size))
                            .font_weight(Typo::TITLE.weight)
                            .text_color(colors.primary)
                            .child(session.title.clone()),
                    )
                    .when_some(branch, |title, branch| {
                        title.child(
                            div()
                                .flex_none()
                                .flex()
                                .items_center()
                                .gap(px(Metrics::TOOLBAR_COMPACT_GAP))
                                .px(px(5.0))
                                .py(px(2.0))
                                .rounded(px(Radius::CHIP))
                                .bg(Fill::subtle(colors))
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_size(px(Typo::META.size))
                                .text_color(colors.tertiary)
                                .child(sf_symbol("arrow.branch", 10.5, colors.tertiary))
                                .child(branch),
                        )
                    })
                    .when_some(host, |title, host| {
                        // Remote-host chip: the agent runs on that configured machine.
                        title.child(
                            div()
                                .flex_none()
                                .flex()
                                .items_center()
                                .gap(px(Metrics::TOOLBAR_COMPACT_GAP))
                                .rounded(px(Radius::CHIP))
                                .px(px(5.0))
                                .py(px(2.0))
                                .bg(Fill::subtle(colors))
                                .text_size(px(Typo::META.size))
                                .text_color(colors.secondary)
                                .child(sf_symbol("network", 9.0, colors.secondary))
                                .child(host),
                        )
                    })
                    .when(!chips.is_empty(), |title| title.child(toolbar_links)),
            )
            .child(
                div()
                    .flex_none()
                    .pl(px(Metrics::TOOLBAR_EDGE_INSET))
                    .flex()
                    .items_center()
                    .gap(px(Metrics::TOOLBAR_ITEM_GAP))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(Metrics::TOOLBAR_COMPACT_GAP))
                            .when_some(glyph, |identity, glyph| identity.child(glyph))
                            .child(
                                div()
                                    .text_size(px(Typo::META.size))
                                    .text_color(colors.tertiary)
                                    .child(kind.label()),
                            ),
                    )
                    .when(shell_controls, |trailing| {
                        trailing.child(
                            div()
                                .id("toggle-inspector")
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
                                .on_click(cx.listener(|_, _, window, cx| {
                                    window.dispatch_action(Box::new(ToggleInspector), cx);
                                    cx.stop_propagation();
                                })),
                        )
                    }),
            )
            .into_any_element()
    }

    fn render_chip(
        &self,
        chip: PaneChip,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let tint = chip.tint.map(chip_tint_color);
        let background = tint.map_or_else(|| Fill::subtle(colors), |color| color.alpha(0.13));
        let hover_background =
            tint.map_or_else(|| colors.primary.alpha(0.10), |color| color.alpha(0.20));
        let activation = chip.clone();
        div()
            .id(SharedString::from(chip.id.clone()))
            .h(px(Metrics::TOOLBAR_CHIP_HEIGHT))
            .max_w(px(TOOLBAR_LINK_MAX_WIDTH))
            .flex_none()
            .flex()
            .items_center()
            .gap(px(Metrics::TOOLBAR_COMPACT_GAP))
            .rounded(px(Radius::CHIP))
            .px(px(6.0))
            .bg(background)
            .hover(move |style| style.bg(hover_background))
            .cursor_pointer()
            .text_size(px(Typo::META.size))
            .text_color(colors.secondary)
            .child(sf_symbol(
                chip.system_image,
                10.0,
                tint.unwrap_or(colors.secondary),
            ))
            .child(
                div()
                    .min_w(px(0.0))
                    .max_w(px(138.0))
                    .truncate()
                    .child(chip.label),
            )
            .on_click(cx.listener(move |this, event: &ClickEvent, _, cx| {
                if event.modifiers().alt {
                    cx.write_to_clipboard(ClipboardItem::new_string(
                        activation.copy_string.clone(),
                    ));
                } else if activation.checks.is_some() {
                    this.open_checks_for = if this.open_checks_for.as_ref() == Some(&activation.id)
                    {
                        None
                    } else {
                        Some(activation.id.clone())
                    };
                    this.overflow_open = false;
                    cx.notify();
                } else if let Some(url) = activation.open_url.as_deref() {
                    cx.open_url(url);
                }
            }))
            .into_any_element()
    }

    fn render_grid_and_overlays(
        &mut self,
        session: &SessionRecord,
        theme: TermTheme,
        font_size: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if session.is_archived() {
            return self.render_archived_overlay(session, cx);
        }
        let exited = matches!(session.status, SessionStatus::Exited(_));
        // An exited agent leaves its last screen behind in the daemon, and that
        // output is exactly what people want to read after closing an agent --
        // so only take the pane over when there is no terminal left to show.
        if exited && let Some(takeover) = self.render_exited_takeover(session, cx) {
            return takeover;
        }
        let Some(resident) = self.residents.get(&session.id) else {
            return centered_message("Preparing terminal…", "").into_any_element();
        };
        let element = resident
            .element
            .clone()
            .theme(theme)
            .font_size(px(font_size))
            .focus_handle(self.focus.clone());
        let view_offset = resident.element.view_offset();
        let attachment_state = resident.attachment_state;
        let overflow = self.grid_row_overflow(resident.element.grid_rows(), font_size, window);

        let id_for_focus = session.id.clone();
        let follows_selection = matches!(self.session_source, SessionSource::FollowSelection);
        let mut body = div()
            .relative()
            .flex_1()
            .overflow_hidden()
            .pt(px(2.0))
            .pb(px(10.0))
            .px(px(12.0))
            .bg(theme.background)
            .track_focus(&self.focus)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &gpui::MouseDownEvent, window, cx| {
                    window.focus(&this.focus, cx);
                    if follows_selection {
                        this.runtime
                            .store
                            .write()
                            .expect("session store lock poisoned")
                            .select(id_for_focus.clone());
                    }
                    let Some(id) = this.selected_id() else {
                        return;
                    };
                    let Some((col, row)) = this.grid_cell_at(event.position, window) else {
                        return;
                    };
                    let Some(resident) = this.residents.get(&id) else {
                        return;
                    };
                    if event.modifiers.platform {
                        match resident.element.reference_at(col, row) {
                            Some(TerminalReference::Url(url)) => cx.open_url(&url),
                            Some(TerminalReference::File(reference)) => {
                                let Some(session) = this.selected_session() else {
                                    return;
                                };
                                cx.emit(TerminalPaneEvent::OpenFileReference {
                                    reference,
                                    cwd: session.cwd.clone(),
                                    session_id: session.id.clone(),
                                });
                            }
                            None => {}
                        }
                        return;
                    }
                    // Mouse-reporting programs (Claude Code, vim) would
                    // normally own the pointer, but clicks are not forwarded
                    // to the PTY yet -- suppressing local selection here
                    // bought nothing and made text un-copyable. Revisit when
                    // click forwarding lands (then: plain drag to the app,
                    // option-drag for local selection, per terminal
                    // convention).
                    match event.click_count {
                        1 => resident.element.begin_selection(col, row),
                        _ => resident.element.select_word(col, row),
                    }
                    // notify, never window.refresh(): refresh() flags the
                    // whole frame as caching-disabled, repainting every cached
                    // view at pointer-event rate.
                    cx.notify();
                }),
            )
            .on_mouse_move(
                cx.listener(|this, event: &gpui::MouseMoveEvent, window, cx| {
                    if event.pressed_button != Some(MouseButton::Left) {
                        return;
                    }
                    let Some(id) = this.selected_id() else {
                        return;
                    };
                    let Some((col, row)) = this.grid_cell_at(event.position, window) else {
                        return;
                    };
                    let Some(resident) = this.residents.get(&id) else {
                        return;
                    };
                    resident.element.drag_selection(col, row);
                    cx.notify();
                }),
            )
            .on_scroll_wheel(cx.listener(Self::handle_scroll))
            .child(match overflow {
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
            });

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
                    .bg(rgba(0x303238e8))
                    .text_size(px(11.5))
                    .text_color(rgba(0xffffff99))
                    .cursor_pointer()
                    .flex()
                    .items_center()
                    .gap(px(5.0))
                    .child(sf_symbol("arrow.down", 11.5, rgba(0xffffff99)))
                    .child(format!("{view_offset} lines · Return to live"))
                    .on_click(cx.listener(move |this, _, _window, cx| {
                        if let Some(resident) = this.residents.get_mut(&return_id) {
                            resident
                                .element
                                .scroll_to_live(usize::from(resident.last_size.1));
                            cx.notify();
                        }
                    })),
            );
        }
        if attachment_state != AttachmentState::Live {
            let message = match attachment_state {
                AttachmentState::Attaching => "Attaching…",
                AttachmentState::Reconnecting => "Reconnecting terminal…",
                AttachmentState::Live => "",
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
                    .bg(rgba(0x303238e8))
                    .text_size(px(11.5))
                    .text_color(rgba(0xffffff99))
                    .child(message),
            );
        }
        if exited {
            body = body.child(self.render_exit_pill(session, cx));
        }
        body.into_any_element()
    }

    /// Slim status pill over an exited session's last screen: says what happened
    /// and offers the resume that the pane-filling card used to.
    fn render_exit_pill(&self, session: &SessionRecord, cx: &mut Context<Self>) -> AnyElement {
        let id = session.id.clone();
        let resumable = session.resumability == Resumability::Resumable;
        let mut pill = div()
            .id("exit-pill")
            .rounded(px(999.0))
            .pl(px(12.0))
            .pr(if resumable { px(4.0) } else { px(12.0) })
            .py(px(4.0))
            .bg(rgba(0x303238e8))
            .flex()
            .items_center()
            .gap(px(8.0))
            .text_size(px(11.5))
            .text_color(rgba(0xffffff99))
            .child(sf_symbol("power", 11.0, rgba(0xffffff66)))
            .child(exit_description(session));
        if resumable {
            pill = pill.child(
                div()
                    .id("exit-pill-resume")
                    .rounded(px(999.0))
                    .px(px(9.0))
                    .py(px(3.0))
                    .bg(rgba(0xffffff1a))
                    .hover(|style| style.bg(rgba(0xffffff2e)))
                    .cursor_pointer()
                    .text_color(rgba(0xffffffe6))
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
            pill = pill.child(
                div()
                    .text_color(rgba(0xffffff4d))
                    .child("· transcript gone"),
            );
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
        let count = if find.matches().is_empty() {
            if find.query().is_empty() {
                String::new()
            } else {
                "No matches".to_owned()
            }
        } else {
            format!("{}/{}", find.current_index() + 1, find.matches().len())
        };
        let query = if resident.find_query.is_empty() {
            div().child("Find").into_any_element()
        } else {
            query_label(&resident.find_query)
        };
        let alt_screen = find.is_alt_screen();
        Some(
            div()
                .id("find-bar")
                .absolute()
                .top(px(Metrics::TITLE_BAR + 6.0))
                .right(px(16.0))
                .w(px(360.0))
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
                                .text_color(rgba(0xffffffd9))
                                .child(sf_symbol("magnifyingglass", 12.0, rgba(0xffffff66)))
                                .child(div().flex_1().child(query))
                                .child(
                                    div()
                                        .text_size(px(Typo::META.size))
                                        .text_color(rgba(0xffffff4d))
                                        .child(count),
                                )
                                .child(div().w(px(1.0)).h(px(16.0)).bg(rgba(0xffffff1a)))
                                .child(find_icon_button(
                                    "find-previous",
                                    "chevron.up",
                                    cx,
                                    |this, _w, cx| {
                                        this.navigate_find(true, cx);
                                    },
                                ))
                                .child(find_icon_button(
                                    "find-next",
                                    "chevron.down",
                                    cx,
                                    |this, _w, cx| {
                                        this.navigate_find(false, cx);
                                    },
                                ))
                                .child(find_icon_button(
                                    "find-close",
                                    "xmark",
                                    cx,
                                    |this, _w, cx| {
                                        this.close_find_for_selected();
                                        cx.notify();
                                    },
                                )),
                        )
                        .when(alt_screen, |bar| {
                            bar.child(
                                div()
                                    .pl(px(20.0))
                                    .text_size(px(Typo::META.size))
                                    .text_color(rgba(0xffffff4d))
                                    .child("full-screen app — screen only"),
                            )
                        }),
                ))
                .into_any_element(),
        )
    }

    /// The pane-filling card for an exited session, or `None` when the terminal
    /// itself should stay on screen (with [`Self::render_exit_pill`] over it).
    fn render_exited_takeover(
        &self,
        session: &SessionRecord,
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
            return Some(centered_message("◌", "Moving session…").into_any_element());
        }
        if auto_resuming {
            return Some(centered_message("◌", "Resuming conversation…").into_any_element());
        }
        if self
            .residents
            .get(&session.id)
            .is_some_and(|resident| resident.element.has_content())
        {
            return None;
        }
        Some(self.render_exited_card(session, cx))
    }

    fn render_exited_card(&self, session: &SessionRecord, cx: &mut Context<Self>) -> AnyElement {
        let id = session.id.clone();
        let content = centered_message("", &exit_description(session));
        if session.resumability == Resumability::Resumable {
            content
                .child(primary_button(
                    "resume-conversation",
                    "Resume Conversation",
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
                        .text_color(rgba(0xffffff4d))
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
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = session.id.clone();
        let mut content = centered_symbol_message("archivebox", 30.0, &session.title).child(
            div()
                .text_size(px(13.0))
                .text_color(rgba(0xffffff99))
                .child("Archived"),
        );
        if session.resumability == Resumability::NotResumable {
            content = content.child(
                div()
                    .max_w(px(320.0))
                    .text_size(px(11.5))
                    .text_color(rgba(0xffffff4d))
                    .child(
                        "This session can't resume its conversation; revive restores it as ended.",
                    ),
            );
        }
        content
            .child(primary_button(
                "revive-session",
                "Revive Session",
                cx,
                move |this, cx| {
                    this.runtime
                        .store
                        .write()
                        .expect("session store lock poisoned")
                        .revive_sessions(vec![id.clone()]);
                    this.reconcile_residency();
                    cx.notify();
                },
            ))
            .into_any_element()
    }

    fn render_checks_popover(
        &self,
        session: &SessionRecord,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let chip_id = self.open_checks_for.as_ref()?;
        let chip = PaneChip::for_session(session)
            .into_iter()
            .find(|chip| &chip.id == chip_id)?;
        let pr = chip.checks?;
        let total = pr.checks_passed + pr.checks_failed + pr.checks_pending;
        let headline = if pr.checks_failed > 0 {
            format!("{} of {total} checks failing", pr.checks_failed)
        } else if pr.checks_pending > 0 {
            format!("{} of {total} checks running", pr.checks_pending)
        } else {
            format!("All {total} checks passed")
        };
        let footer = comments_help(&pr);
        let mut rows = div().flex().flex_col().py(px(4.0)).px(px(6.0));
        for (index, check) in sorted_checks(&pr).into_iter().enumerate() {
            let color = match check.result.as_str() {
                "pass" => Ink::FRESH,
                "fail" => Ink::DANGER,
                _ => Ink::ATTENTION,
            };
            let word = match check.result.as_str() {
                "fail" => "failed",
                "pending" => "running",
                _ => "",
            };
            let url = check.url.clone();
            rows = rows.child(
                div()
                    .id(SharedString::from(format!("pr-check-{index}")))
                    .h(px(24.0))
                    .rounded(px(Radius::ROW))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .px(px(8.0))
                    .hover(|style| style.bg(rgba(0xffffff0f)))
                    .when(url.is_some(), |row| row.cursor_pointer())
                    .child(div().size(px(6.0)).rounded(px(3.0)).bg(color))
                    .child(
                        div()
                            .flex_1()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_size(px(Typo::ROW.size))
                            .text_color(rgba(0xffffffd9))
                            .child(check.name),
                    )
                    .child(
                        div()
                            .text_size(px(Typo::META.size))
                            .text_color(color)
                            .child(word),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(url) = url.as_deref() {
                            cx.open_url(url);
                            this.open_checks_for = None;
                            cx.notify();
                        }
                    })),
            );
        }
        Some(
            div()
                .absolute()
                .inset_0()
                .child(div().absolute().inset_0().occlude().on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| {
                        this.open_checks_for = None;
                        cx.notify();
                        cx.stop_propagation();
                    }),
                ))
                .child(
                    div()
                        .id("checks-popover")
                        .absolute()
                        .top(px(Metrics::TITLE_BAR + 4.0))
                        .right(px(112.0))
                        .w(px(300.0))
                        .occlude()
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                            this.open_checks_for = None;
                            cx.notify();
                        }))
                        .child(FloatingSurface::new(
                            colors,
                            div()
                                .flex()
                                .flex_col()
                                .child(
                                    div()
                                        .px(px(12.0))
                                        .py(px(8.0))
                                        .text_size(px(Typo::ROW_EMPHASIZED.size))
                                        .font_weight(Typo::ROW_EMPHASIZED.weight)
                                        .text_color(rgba(0xffffffff))
                                        .child(headline),
                                )
                                .child(div().h(px(1.0)).bg(rgba(0xffffff14)))
                                .child(div().max_h(px(246.0)).overflow_hidden().child(rows))
                                .child(div().h(px(1.0)).bg(rgba(0xffffff14)))
                                .child(
                                    div()
                                        .px(px(12.0))
                                        .py(px(7.0))
                                        .text_size(px(Typo::META.size))
                                        .text_color(rgba(0xffffff99))
                                        .child(footer),
                                ),
                        )),
                )
                .into_any_element(),
        )
    }

    fn render_overflow(
        &self,
        session: &SessionRecord,
        visible_chip_count: usize,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let chips = PaneChip::for_session(session);
        if !self.overflow_open || visible_chip_count >= chips.len() {
            return None;
        }
        let mut list = div().flex().flex_col().p(px(6.0));
        for (index, chip) in chips.into_iter().skip(visible_chip_count).enumerate() {
            let url = chip.open_url.clone();
            let checks = chip.checks.is_some();
            let chip_id = chip.id.clone();
            let tint = chip.tint.map(chip_tint_color);
            list = list.child(
                div()
                    .id(SharedString::from(format!("overflow-chip-{index}")))
                    .h(px(26.0))
                    .rounded(px(Radius::ROW))
                    .flex()
                    .items_center()
                    .gap(px(7.0))
                    .px(px(8.0))
                    .text_size(px(Typo::ROW.size))
                    .text_color(rgba(0xffffffd9))
                    .hover(|style| style.bg(rgba(0xffffff0f)))
                    .cursor_pointer()
                    .child(sf_symbol(
                        chip.system_image,
                        11.0,
                        tint.unwrap_or(rgba(0xffffff99)),
                    ))
                    .child(div().min_w(px(0.0)).flex_1().truncate().child(chip.label))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if checks {
                            this.open_checks_for = Some(chip_id.clone());
                        } else if let Some(url) = url.as_deref() {
                            cx.open_url(url);
                        }
                        this.overflow_open = false;
                        cx.notify();
                    })),
            );
        }
        Some(
            div()
                .absolute()
                .inset_0()
                .child(div().absolute().inset_0().occlude().on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| {
                        this.overflow_open = false;
                        cx.notify();
                        cx.stop_propagation();
                    }),
                ))
                .child(
                    div()
                        .absolute()
                        .top(px(Metrics::TITLE_BAR + 4.0))
                        .right(px(112.0))
                        .w(px(280.0))
                        .occlude()
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                            this.overflow_open = false;
                            cx.notify();
                        }))
                        .child(FloatingSurface::new(
                            colors,
                            list.id("toolbar-overflow-list")
                                .max_h(px(320.0))
                                .overflow_y_scroll(),
                        )),
                )
                .into_any_element(),
        )
    }
}

impl Render for TerminalPane {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.reconcile_residency();
        let (theme, colors, sidebar_colors, font_size) = {
            let store = self
                .runtime
                .store
                .read()
                .expect("session store lock poisoned");
            let theme_id = &store.preferences().terminal_theme;
            (
                crate::app_theme::terminal_theme(theme_id),
                crate::app_theme::colors(theme_id),
                crate::app_theme::sidebar_colors(theme_id),
                store.preferences().terminal_font_size,
            )
        };
        self.sync_status_glyphs(colors, window, cx);
        self.update_selected_geometry(window, cx);

        let selected = self.selected_session();

        let content = if let Some(session) = selected {
            let chips = PaneChip::for_session(&session);
            let visible_chip_count = toolbar_visible_chip_count(
                &chips,
                self.viewport.map_or(900.0, |viewport| viewport.width),
                self.sidebar_visible,
            );
            if visible_chip_count >= chips.len() {
                self.overflow_open = false;
            }
            let mut pane = div()
                .relative()
                .flex()
                .flex_col()
                .flex_1()
                .h_full()
                .overflow_hidden()
                .border_l_1()
                .border_color(sidebar_colors.primary.alpha(0.08))
                .bg(sidebar_colors.sidebar_surface())
                .child(self.render_header(
                    &session,
                    &chips,
                    visible_chip_count,
                    sidebar_colors,
                    cx,
                ));
            let terminal_surface = div()
                .relative()
                .min_h(px(0.0))
                .flex_1()
                .flex()
                .flex_col()
                .rounded_tl(px(Radius::CARD))
                .rounded_tr(px(Radius::CARD))
                .overflow_hidden()
                .bg(theme.background)
                .child(self.render_grid_and_overlays(&session, theme, font_size, window, cx));
            pane = pane.child(terminal_surface);
            if let Some(find) = self.render_find_bar(&session, colors, cx) {
                pane = pane.child(find);
            }
            if let Some(popover) = self.render_checks_popover(&session, colors, cx) {
                pane = pane.child(popover);
            }
            if let Some(overflow) = self.render_overflow(&session, visible_chip_count, colors, cx) {
                pane = pane.child(overflow);
            }
            pane.into_any_element()
        } else {
            let show_sidebar = matches!(self.session_source, SessionSource::FollowSelection)
                && !self.sidebar_visible;
            let sidebar_reveal =
                show_sidebar.then(|| self.render_sidebar_reveal_control(sidebar_colors, cx));
            div()
                .flex_1()
                .h_full()
                .flex()
                .flex_col()
                .bg(theme.background)
                .when_some(sidebar_reveal, |pane, control| {
                    pane.child(
                        div()
                            .h(px(Metrics::TITLE_BAR))
                            .flex_none()
                            .px(px(Metrics::TOOLBAR_EDGE_INSET))
                            .flex()
                            .items_center()
                            .bg(sidebar_colors.sidebar_surface())
                            .child(control),
                    )
                })
                .child(
                    div()
                        .flex_1()
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_size(px(13.0))
                        .text_color(sidebar_colors.tertiary)
                        .child("Start a terminal from the sidebar"),
                )
                .into_any_element()
        };

        let root_id = match &self.session_source {
            SessionSource::FollowSelection => SharedString::from("diri-terminal-root"),
            SessionSource::Fixed(id) => SharedString::from(format!("diri-terminal-root-{}", id.0)),
        };
        div()
            .id(root_id)
            .key_context(TERMINAL_CONTEXT)
            .track_focus(&self.focus)
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
            .on_key_down(cx.listener(Self::handle_key_down))
            .on_key_up(cx.listener(Self::handle_key_up))
            .on_modifiers_changed(cx.listener(Self::handle_modifiers_changed))
            .child(content)
    }
}

fn find_icon_button(
    id: &'static str,
    system_image: &'static str,
    cx: &mut Context<TerminalPane>,
    handler: impl Fn(&mut TerminalPane, &mut Window, &mut Context<TerminalPane>) + 'static,
) -> AnyElement {
    div()
        .id(id)
        .size(px(20.0))
        .rounded(px(4.0))
        .flex()
        .items_center()
        .justify_center()
        .text_size(px(11.0))
        .text_color(rgba(0xffffff99))
        .hover(|style| style.bg(rgba(0xffffff0f)))
        .cursor_pointer()
        .child(sf_symbol_weighted(
            system_image,
            11.0,
            SymbolWeight::Semibold,
            rgba(0xffffff99),
        ))
        .on_click(cx.listener(move |this, _, window, cx| handler(this, window, cx)))
        .into_any_element()
}

fn primary_button(
    id: &'static str,
    label: &'static str,
    cx: &mut Context<TerminalPane>,
    handler: impl Fn(&mut TerminalPane, &mut Context<TerminalPane>) + 'static,
) -> AnyElement {
    div()
        .id(id)
        .mt(px(2.0))
        .rounded(px(7.0))
        .px(px(14.0))
        .py(px(7.0))
        .bg(rgba(0xffffffeb))
        .text_size(px(13.0))
        .font_weight(Typo::ROW_EMPHASIZED.weight)
        .text_color(rgba(0x121318ff))
        .hover(|style| style.bg(rgba(0xffffffff)))
        .cursor_pointer()
        .child(label)
        .on_click(cx.listener(move |this, _, _, cx| handler(this, cx)))
        .into_any_element()
}

fn centered_message(icon: &str, message: &str) -> gpui::Div {
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
                    .text_color(rgba(0xffffff4d))
                    .child(icon.to_owned()),
            )
        })
        .when(!message.is_empty(), |content| {
            content.child(
                div()
                    .text_size(px(13.0))
                    .text_color(rgba(0xffffff99))
                    .child(message.to_owned()),
            )
        })
}

fn centered_symbol_message(system_image: &str, size: f32, message: &str) -> gpui::Div {
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
            rgba(0xffffff4d),
        ))
        .when(!message.is_empty(), |content| {
            content.child(
                div()
                    .text_size(px(13.0))
                    .text_color(rgba(0xffffff99))
                    .child(message.to_owned()),
            )
        })
}

fn chip_tint_color(tint: ChipTint) -> gpui::Rgba {
    match tint {
        ChipTint::Red => Ink::DANGER,
        ChipTint::Orange => rgba(0xf59e42ff),
        ChipTint::Yellow => Ink::ATTENTION,
        ChipTint::Green => Ink::FRESH,
        ChipTint::Purple => rgba(0xa879f7ff),
    }
}

fn terminal_key_event(event: &KeyDownEvent) -> Option<TermKeyEvent> {
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

fn spawn_attachment(
    runtime: &Handle,
    socket: std::path::PathBuf,
    id: SessionId,
    pane_tx: PaneEventSender,
) -> AttachmentControl {
    let (command_tx, mut commands) = mpsc::unbounded_channel();
    let control = AttachmentControl { tx: command_tx };
    runtime.spawn(async move {
        // The first resize must be the measured pane geometry: deferred agent
        // launch waits for it. Do not seed an arbitrary 80×24 size.
        let mut last_resize = None;
        loop {
            let _ = pane_tx.send(PaneEvent::AttachmentState(
                id.clone(),
                AttachmentState::Attaching,
            ));
            let mut attachment = match SessionAttachment::connect(&socket, id.clone()).await {
                Ok(attachment) => attachment,
                Err(_) => {
                    let _ = pane_tx.send(PaneEvent::AttachmentState(
                        id.clone(),
                        AttachmentState::Reconnecting,
                    ));
                    if wait_for_retry(&mut commands, &mut last_resize).await {
                        return;
                    }
                    continue;
                }
            };
            let writer = attachment.handle();
            if let Some((cols, rows)) = last_resize {
                let _ = writer.resize(cols, rows);
            }
            let _ = pane_tx.send(PaneEvent::AttachmentState(
                id.clone(),
                AttachmentState::Live,
            ));

            let should_close = loop {
                tokio::select! {
                    chunk = attachment.chunks.recv() => {
                        let Some(chunk) = chunk else { break false };
                        if pane_tx.send(PaneEvent::Chunk(id.clone(), chunk)).is_err() {
                            break true;
                        }
                    }
                    command = commands.recv() => {
                        match command {
                            Some(AttachmentCommand::Input(bytes)) => {
                                let _ = writer.send_input(bytes);
                            }
                            Some(AttachmentCommand::Resize(cols, rows)) => {
                                last_resize = Some((cols, rows));
                                let _ = writer.resize(cols, rows);
                            }
                            Some(AttachmentCommand::Scroll { direction, lines, col, row }) => {
                                let _ = writer.scroll(direction, lines, col, row);
                            }
                            Some(AttachmentCommand::Close) | None => break true,
                        }
                    }
                }
            };
            attachment.close().await;
            if should_close {
                return;
            }
            let _ = pane_tx.send(PaneEvent::AttachmentState(
                id.clone(),
                AttachmentState::Reconnecting,
            ));
            if wait_for_retry(&mut commands, &mut last_resize).await {
                return;
            }
        }
    });
    control
}

async fn wait_for_retry(
    commands: &mut mpsc::UnboundedReceiver<AttachmentCommand>,
    last_resize: &mut Option<(u16, u16)>,
) -> bool {
    let delay = tokio::time::sleep(REATTACH_DELAY);
    tokio::pin!(delay);
    loop {
        tokio::select! {
            () = &mut delay => return false,
            command = commands.recv() => match command {
                Some(AttachmentCommand::Resize(cols, rows)) => *last_resize = Some((cols, rows)),
                Some(AttachmentCommand::Close) | None => return true,
                Some(AttachmentCommand::Input(_)) | Some(AttachmentCommand::Scroll { .. }) => {}
            }
        }
    }
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

fn pr_number(url: &str) -> Option<String> {
    let parts: Vec<_> = url.split('/').filter(|part| !part.is_empty()).collect();
    if let Some(index) = parts.iter().position(|part| *part == "pull") {
        return parts
            .get(index + 1)
            .map(|part| part.chars().take_while(char::is_ascii_digit).collect())
            .filter(|part: &String| !part.is_empty());
    }
    parts
        .last()
        .filter(|part| part.chars().all(|character| character.is_ascii_digit()))
        .map(|part| (*part).to_owned())
}

fn linear_key(url: &str) -> Option<String> {
    let parts: Vec<_> = url.split('/').collect();
    let index = parts.iter().position(|part| *part == "issue")?;
    parts.get(index + 1).map(|part| (*part).to_owned())
}

fn url_host(url: &str) -> String {
    url.split_once("://")
        .map_or(url, |(_, remainder)| remainder)
        .split('/')
        .next()
        .unwrap_or(url)
        .split(':')
        .next()
        .unwrap_or(url)
        .to_owned()
}

fn url_port(url: &str) -> Option<u16> {
    let authority = url
        .split_once("://")
        .map_or(url, |(_, remainder)| remainder)
        .split('/')
        .next()?;
    authority.rsplit_once(':')?.1.parse().ok()
}

fn pr_tint(pr: &PullRequestStatus) -> Option<ChipTint> {
    if pr.state == "MERGED" {
        return Some(ChipTint::Purple);
    }
    if pr.state == "CLOSED" || pr.mergeable.as_deref() == Some("CONFLICTING") {
        return Some(ChipTint::Red);
    }
    if pr.is_draft {
        return None;
    }
    match pr.review_decision.as_deref() {
        Some("CHANGES_REQUESTED") => Some(ChipTint::Orange),
        Some("REVIEW_REQUIRED") => Some(ChipTint::Yellow),
        Some("APPROVED") => Some(ChipTint::Green),
        _ => None,
    }
}

fn pr_help(pr: &PullRequestStatus) -> String {
    let overall = if pr.state == "MERGED" {
        "merged"
    } else if pr.state == "CLOSED" {
        "closed"
    } else if pr.is_draft {
        "draft"
    } else {
        "open"
    };
    let title = pr.title.as_deref().map_or_else(
        || overall.to_owned(),
        |title| format!("{title} — {overall}"),
    );
    format!(
        "{title} · +{} −{} · {} file{}",
        pr.additions,
        pr.deletions,
        pr.changed_files,
        if pr.changed_files == 1 { "" } else { "s" }
    )
}

fn comments_help(pr: &PullRequestStatus) -> String {
    let mut parts = Vec::new();
    if let Some(total) = pr.total_threads.filter(|total| *total > 0) {
        parts.push(format!(
            "{} of {total} threads resolved",
            pr.resolved_threads.unwrap_or(0)
        ));
    }
    parts.push(format!(
        "{} comment{}",
        pr.comment_count,
        if pr.comment_count == 1 { "" } else { "s" }
    ));
    parts.push(format!(
        "{} review{}",
        pr.review_count,
        if pr.review_count == 1 { "" } else { "s" }
    ));
    parts.join(" · ")
}

fn sorted_checks(pr: &PullRequestStatus) -> Vec<PrCheck> {
    let mut checks = pr.checks.clone().unwrap_or_default();
    checks.sort_by_key(|check| match check.result.as_str() {
        "fail" => 0,
        "pending" => 1,
        "pass" => 2,
        _ => 3,
    });
    checks
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
    chrome_inset: f32,
    metrics: CellMetrics,
) -> (u16, u16) {
    let width = px((window_width
        - chrome_inset
        - GRID_HORIZONTAL_PADDING
        - GRID_LAYOUT_HORIZONTAL_CHROME)
        .max(1.0));
    let height = px((window_height
        - Metrics::TITLE_BAR
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

#[cfg(test)]
mod tests {
    use diri_proto::grid::{ChangedRow, GridCell};
    use diri_proto::{
        DateMillis, ExitInfo, NeedsInputDetail, NeedsInputKind, NeedsInputSource, SessionListResult,
    };
    use gpui::{Image, ImageFormat, KeyDownEvent, Keystroke, Modifiers, TestAppContext, point};

    use super::*;

    #[tokio::test]
    async fn pane_mailbox_retains_one_exact_final_grid_per_session() {
        let (sender, mut receiver) = pane_event_channel();
        let id = SessionId::new("mailbox");
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
                .send(PaneEvent::Chunk(id.clone(), TerminalChunk::Grid(first)))
                .is_ok()
        );
        assert!(
            sender
                .send(PaneEvent::Chunk(id.clone(), TerminalChunk::Grid(second)))
                .is_ok()
        );

        let mut batch = Vec::new();
        assert!(receiver.recv_batch(&mut batch).await);
        assert_eq!(batch.len(), 1);
        let PaneEvent::GridBatch(batch_id, updates) = batch.pop().expect("grid batch") else {
            panic!("mailbox did not return a grid batch");
        };
        assert_eq!(batch_id, id);
        assert_eq!(updates.len(), 2, "the post-snapshot boundary is retained");
        let mut applied = Vec::new();
        for update in &updates {
            update.apply(&mut applied);
        }
        assert_eq!(applied, final_cells);
        assert_eq!(updates.last().expect("final update").cursor_col, 0);
        assert!(!updates.last().expect("final update").cursor_visible);
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

    fn fixture_session() -> SessionRecord {
        let envelope: serde_json::Value = serde_json::from_str(include_str!(
            "../../diri-proto/tests/fixtures/session_list_response.json"
        ))
        .unwrap();
        let list: SessionListResult = serde_json::from_value(envelope["ok"].clone()).unwrap();
        list.sessions[0].clone()
    }

    fn pull_request(url: &str) -> PullRequestStatus {
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
    fn chips_follow_swift_artifact_pr_family_then_ports_order() {
        let mut session = fixture_session();
        let url = "https://github.com/dirijor/dirijor/pull/42";
        session.artifacts = Some(vec![SessionArtifact {
            kind: ArtifactKind::PullRequest,
            url: url.to_owned(),
            first_seen_at: DateMillis(1.0),
        }]);
        session.pull_requests = Some(vec![pull_request(url)]);
        session.listening_ports = Some(vec![diri_proto::PortInfo {
            port: 3000,
            process_name: "vite".to_owned(),
        }]);

        let chips = PaneChip::for_session(&session);
        assert_eq!(chips.len(), 4);
        assert_eq!(chips[0].label, "PR #42 +45 −12");
        assert_eq!(chips[0].tint, Some(ChipTint::Green));
        assert_eq!(chips[1].label, "3/5");
        assert_eq!(chips[1].tint, Some(ChipTint::Red));
        assert!(chips[1].checks.is_some());
        assert_eq!(chips[2].label, "3/5");
        assert_eq!(chips[2].tint, Some(ChipTint::Orange));
        assert_eq!(chips[3].label, ":3000");
        assert_eq!(chips[3].open_url.as_deref(), Some("http://localhost:3000"));
    }

    #[test]
    fn toolbar_prioritizes_pr_destinations_and_collapses_low_priority_links() {
        let mut session = fixture_session();
        let first_pr = "https://github.com/dirijor/dirijor/pull/7";
        let second_pr = "https://github.com/dirijor/dirijor/pull/8";
        session.artifacts = Some(vec![
            SessionArtifact {
                kind: ArtifactKind::Link,
                url: "https://docs.example.com/reference".to_owned(),
                first_seen_at: DateMillis(1.0),
            },
            SessionArtifact {
                kind: ArtifactKind::PullRequest,
                url: first_pr.to_owned(),
                first_seen_at: DateMillis(2.0),
            },
            SessionArtifact {
                kind: ArtifactKind::Preview,
                url: "https://preview.example.com".to_owned(),
                first_seen_at: DateMillis(3.0),
            },
            SessionArtifact {
                kind: ArtifactKind::PullRequest,
                url: second_pr.to_owned(),
                first_seen_at: DateMillis(4.0),
            },
        ]);
        session.pull_requests = Some(vec![pull_request(first_pr), pull_request(second_pr)]);

        let chips = PaneChip::for_session(&session);
        assert!(chips[0].label.starts_with("PR #7"));
        assert!(chips[1].label.starts_with("PR #8"));
        assert!(
            chips
                .iter()
                .position(|chip| chip.label == "docs.example.com")
                .is_some_and(|index| index > 1)
        );
        assert_eq!(
            toolbar_visible_chip_count(&chips, 5_000.0, true),
            TOOLBAR_MAX_VISIBLE_LINKS
        );
        assert_eq!(toolbar_visible_chip_count(&chips, 700.0, false), 0);
    }

    #[test]
    fn check_popover_prioritizes_failure_then_running() {
        let checks = sorted_checks(&pull_request("https://example.com/pull/42"));
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
            cx.debug_bounds("show-sidebar").is_some(),
            "collapsing the sidebar must leave a way to reveal it"
        );
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

    #[gpui::test]
    fn terminal_popovers_dismiss_on_an_outside_click(cx: &mut TestAppContext) {
        let runtime = Arc::new(StoreRuntime::inert());
        let tokio = Arc::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("test runtime"),
        );
        let mut session = fixture_session();
        let url = "https://github.com/dirijor/dirijor/pull/42";
        session.artifacts = Some(vec![SessionArtifact {
            kind: ArtifactKind::PullRequest,
            url: url.to_owned(),
            first_seen_at: DateMillis(1.0),
        }]);
        session.pull_requests = Some(vec![pull_request(url)]);
        let checks_id = PaneChip::for_session(&session)
            .into_iter()
            .find(|chip| chip.checks.is_some())
            .expect("fixture should expose a checks chip")
            .id;
        {
            let mut store = runtime.store.write().expect("session store lock poisoned");
            store.upsert_session(session.clone());
            store.select(session.id.clone());
        }

        let (pane, cx) = cx.add_window_view(move |window, cx| {
            let mut pane = TerminalPane::new(runtime, tokio, window, cx);
            pane.open_checks_for = Some(checks_id);
            pane
        });
        let outside_panel = point(px(500.0), px(320.0));

        cx.simulate_click(outside_panel, Modifiers::default());
        assert_eq!(
            pane.read_with(cx, |pane, _| pane.open_checks_for.clone()),
            None
        );

        pane.update(cx, |pane, cx| {
            pane.overflow_open = true;
            cx.notify();
        });
        cx.simulate_click(outside_panel, Modifiers::default());
        assert!(!pane.read_with(cx, |pane, _| pane.overflow_open));
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
        let reported = estimated_grid_size(101.5, 100.0, 0.0, metrics);
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
