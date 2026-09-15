//! One mounted transport and live grid per (Engine socket, SessionId).
//! View leases own interaction state elsewhere; changing focus only changes
//! admission authority, never the queue or the durable session.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicU64, Ordering};

use diri_client::attachment::{AttachmentClosed, SessionAttachmentHandle};
use diri_term::element::TerminalDamageObserver;
use gpui::{App, Global};
use tokio::sync::{Notify, oneshot, watch};

use super::*;

static NEXT_VIEW: AtomicU64 = AtomicU64::new(1);

#[derive(Default)]
struct Controllers(HashMap<(PathBuf, SessionId), ControllerEntry>);
struct ControllerEntry {
    session: Weak<RefCell<SessionController>>,
    drained: watch::Receiver<bool>,
}

struct DrainFinished(watch::Sender<bool>);
impl Drop for DrainFinished {
    fn drop(&mut self) {
        let _ = self.0.send(true);
    }
}
impl Global for Controllers {}

struct MountedView {
    events: PaneEventSender,
    generation: AttachmentGeneration,
    damage: Option<TerminalDamageObserver>,
}

struct SessionController {
    id: SessionId,
    buffer: SharedGridBuffer,
    control: Arc<Mutex<ControlState>>,
    views: HashMap<u64, MountedView>,
    state: AttachmentState,
    modes: Option<TerminalChunk>,
    hold: Option<ReflowHold>,
    _events: Option<Task<()>>,
    shutdown: Option<oneshot::Sender<()>>,
}

struct ControlState {
    owner: u64,
    ownership_revision: u64,
    writer: Option<SessionAttachmentHandle>,
    last_resize: Option<(u16, u16)>,
    pending_resize: Option<(u16, u16)>,
    resize_wake: Arc<Notify>,
}

/// Accepted means queued locally. There is no PTY delivery acknowledgement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum InputRejection {
    PassiveView,
    Disconnected,
    Overloaded,
}

impl InputRejection {
    fn message(self) -> &'static str {
        match self {
            Self::PassiveView => "This terminal is active in another view. Focus it to type here.",
            Self::Disconnected => "Terminal input was not accepted while reconnecting.",
            Self::Overloaded => "Terminal input queue is full. The latest input was not accepted.",
        }
    }
}

#[derive(Clone)]
pub(super) struct AttachmentControl {
    state: Arc<Mutex<ControlState>>,
    view: u64,
    events: PaneEventSender,
    id: SessionId,
    #[cfg(test)]
    pub(super) input_observer: Option<(SessionId, InputObserver)>,
}

impl AttachmentControl {
    pub(super) fn claim(&self) {
        let mut state = self.state.lock().unwrap();
        if state.owner != self.view {
            state.owner = self.view;
            state.ownership_revision = state.ownership_revision.wrapping_add(1);
            state.last_resize = None;
            state.pending_resize = None;
            state.resize_wake.notify_one();
        }
    }

    pub(super) fn release(&self) {
        let mut state = self.state.lock().unwrap();
        if state.owner == self.view {
            state.owner = 0;
            state.ownership_revision = state.ownership_revision.wrapping_add(1);
            state.last_resize = None;
            state.pending_resize = None;
            state.resize_wake.notify_one();
        }
    }

    pub(super) fn ownership_revision(&self) -> u64 {
        self.state.lock().unwrap().ownership_revision
    }

    pub(super) fn resize_if_current(&self, size: (u16, u16), revision: u64) {
        let _ = self.submit_at(AttachmentCommand::Resize(size.0, size.1), Some(revision));
    }

    pub(super) fn needs_resize(&self, size: (u16, u16)) -> bool {
        let state = self.state.lock().unwrap();
        state.pending_resize.or(state.last_resize) != Some(size)
    }

    pub(super) fn is_controller(&self) -> bool {
        self.state.lock().unwrap().owner == self.view
    }

    fn submit(&self, command: AttachmentCommand) -> Result<(), InputRejection> {
        self.submit_at(command, None)
    }

    fn submit_at(
        &self,
        command: AttachmentCommand,
        revision: Option<u64>,
    ) -> Result<(), InputRejection> {
        let mut state = self.state.lock().unwrap();
        if state.owner != self.view
            || revision.is_some_and(|revision| revision != state.ownership_revision)
        {
            return Err(InputRejection::PassiveView);
        }
        if let AttachmentCommand::Resize(cols, rows) = command {
            let size = (cols, rows);
            let result = match &state.writer {
                Some(writer) => writer.resize(cols, rows),
                None => Ok(()), // Desired geometry survives connection setup.
            };
            match result {
                Ok(()) => {
                    state.last_resize = Some(size);
                    state.pending_resize = None;
                }
                Err(_) => {
                    state.pending_resize = Some(size);
                    state.resize_wake.notify_one();
                }
            }
            // Geometry is a coalesced request, not rejected user typing. The
            // worker reserves capacity and admits only the latest owned size.
            return Ok(());
        }
        let writer = state.writer.as_ref().ok_or(InputRejection::Disconnected)?;
        let result = match command {
            AttachmentCommand::Input(bytes) => writer.send_input(bytes),
            AttachmentCommand::Mouse(bytes) => writer.send_mouse(bytes),
            AttachmentCommand::Resize(_, _) => unreachable!("resize handled above"),
            AttachmentCommand::Scroll {
                direction,
                lines,
                col,
                row,
            } => writer.scroll(direction, lines, col, row),
        };
        result.map_err(|error| match error {
            AttachmentClosed::Backpressure => InputRejection::Overloaded,
            AttachmentClosed::Closed => InputRejection::Disconnected,
        })
    }

    fn report(&self, result: Result<(), InputRejection>) {
        if let Err(error) = result {
            let _ = self.events.send(PaneEvent::InputFeedback(
                self.id.clone(),
                error.message().into(),
            ));
        }
    }

    pub(super) fn input(&self, bytes: Vec<u8>) {
        if bytes.is_empty() {
            return;
        }
        #[cfg(test)]
        if let Some((id, observer)) = &self.input_observer
            && self.is_controller()
        {
            let _ = observer.send((id.clone(), bytes.clone()));
        }
        self.report(self.submit(AttachmentCommand::Input(bytes)));
    }

    pub(super) fn resize(&self, cols: u16, rows: u16) {
        // A delayed resize from a view that lost focus is simply obsolete.
        let _ = self.submit(AttachmentCommand::Resize(cols, rows));
    }

    pub(super) fn mouse(&self, bytes: Vec<u8>) {
        if !bytes.is_empty() {
            self.report(self.submit(AttachmentCommand::Mouse(bytes)));
        }
    }

    pub(super) fn scroll(&self, direction: u8, lines: u16, col: u16, row: u16) {
        self.report(self.submit(AttachmentCommand::Scroll {
            direction,
            lines,
            col,
            row,
        }));
    }
}

pub(super) struct ControllerLease {
    session: Rc<RefCell<SessionController>>,
    view: u64,
}

impl ControllerLease {
    pub(super) fn mount(
        socket: PathBuf,
        id: SessionId,
        runtime: &Handle,
        events: PaneEventSender,
        generation: AttachmentGeneration,
        parked: Option<SharedGridBuffer>,
        cx: &mut App,
    ) -> (Self, AttachmentControl, SharedGridBuffer) {
        if !cx.has_global::<Controllers>() {
            cx.set_global(Controllers::default());
        }
        let key = (socket.clone(), id.clone());
        let existing = cx
            .global_mut::<Controllers>()
            .0
            .get(&key)
            .and_then(|entry| entry.session.upgrade());
        let session = existing.unwrap_or_else(|| {
            let (tx, mut rx) = pane_event_channel();
            let (shutdown, shutdown_rx) = oneshot::channel();
            let control = Arc::new(Mutex::new(ControlState {
                owner: 0,
                ownership_revision: 0,
                writer: None,
                last_resize: None,
                pending_resize: None,
                resize_wake: Arc::new(Notify::new()),
            }));
            let session = Rc::new(RefCell::new(SessionController {
                id: id.clone(),
                buffer: parked
                    .unwrap_or_else(|| Arc::new(std::sync::RwLock::new(GridBuffer::default()))),
                control: control.clone(),
                views: HashMap::new(),
                state: AttachmentState::Attaching,
                modes: None,
                hold: None,
                _events: None,
                shutdown: Some(shutdown),
            }));
            let weak = Rc::downgrade(&session);
            session.borrow_mut()._events = Some(cx.spawn(async move |cx| {
                let mut batch = Vec::new();
                while rx.recv_batch(&mut batch).await {
                    let Some(session) = weak.upgrade() else {
                        return;
                    };
                    cx.update(|_| {
                        let mut session = session.borrow_mut();
                        for event in batch.drain(..) {
                            session.receive(event);
                        }
                    });
                }
            }));
            let previous = cx
                .global_mut::<Controllers>()
                .0
                .get(&key)
                .map(|entry| entry.drained.clone());
            let (finished, drained) = watch::channel(false);
            spawn_transport(
                runtime,
                socket,
                id.clone(),
                control,
                tx,
                shutdown_rx,
                (previous, DrainFinished(finished)),
            );
            cx.global_mut::<Controllers>()
                .0
                .retain(|_, entry| entry.session.strong_count() > 0 || !*entry.drained.borrow());
            cx.global_mut::<Controllers>().0.insert(
                key,
                ControllerEntry {
                    session: Rc::downgrade(&session),
                    drained,
                },
            );
            session
        });
        let view = NEXT_VIEW.fetch_add(1, Ordering::Relaxed);
        let mut core = session.borrow_mut();
        // Hydration may mount a session first in an inactive window. Only an
        // explicit active-window/pane claim grants input and resize authority.
        let attachment = AttachmentControl {
            state: core.control.clone(),
            view,
            events: events.clone(),
            id: id.clone(),
            #[cfg(test)]
            input_observer: None,
        };
        let _ = events.send(PaneEvent::AttachmentState(
            id.clone(),
            generation,
            core.state,
        ));
        if let Some(modes) = &core.modes {
            let _ = events.send(PaneEvent::Chunk(id, generation, modes.clone()));
        }
        core.views.insert(
            view,
            MountedView {
                events,
                generation,
                damage: None,
            },
        );
        let buffer = core.buffer.clone();
        drop(core);
        (Self { session, view }, attachment, buffer)
    }

    #[cfg(all(test, target_os = "macos"))]
    pub(super) fn seed_live_for_test(&self) {
        self.session
            .borrow_mut()
            .receive(PaneEvent::AttachmentState(
                SessionId("fixture".into()),
                0,
                AttachmentState::Live,
            ));
    }

    pub(super) fn observe(&self, element: &TerminalElement) {
        if let Some(view) = self.session.borrow_mut().views.get_mut(&self.view) {
            view.damage = Some(element.damage_observer());
        }
    }

    pub(super) fn hold_reflow(&self, cx: &mut App) {
        if self.session.borrow().control.lock().unwrap().owner != self.view {
            return;
        }
        let weak = Rc::downgrade(&self.session);
        let release = cx.spawn(async move |cx| {
            cx.background_executor().timer(REFLOW_HOLD).await;
            if let Some(session) = weak.upgrade() {
                cx.update(|_| session.borrow_mut().release_hold());
            }
        });
        let mut session = self.session.borrow_mut();
        let parked = session
            .hold
            .take()
            .map_or_else(Vec::new, |hold| hold.parked);
        session.hold = Some(ReflowHold {
            parked,
            saw_snapshot: false,
            _release: release,
        });
    }
}

impl Drop for ControllerLease {
    fn drop(&mut self) {
        let mut session = self.session.borrow_mut();
        session.views.remove(&self.view);
        let mut control = session.control.lock().unwrap();
        if control.owner == self.view {
            // No hidden passive view gets authority without an explicit focus.
            control.owner = 0;
            control.ownership_revision = control.ownership_revision.wrapping_add(1);
            control.last_resize = None;
            control.pending_resize = None;
            control.resize_wake.notify_one();
        }
    }
}

impl Drop for SessionController {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
    }
}

impl SessionController {
    fn receive(&mut self, event: PaneEvent) {
        match event {
            PaneEvent::AttachmentState(_, _, state) => {
                self.state = state;
                if state != AttachmentState::Live {
                    self.modes = None;
                }
                for view in self.views.values() {
                    let _ = view.events.send(PaneEvent::AttachmentState(
                        self.id.clone(),
                        view.generation,
                        state,
                    ));
                }
            }
            PaneEvent::Chunk(_, _, TerminalChunk::Grid(update)) => self.grid(update),
            PaneEvent::GridBatch(_, _, updates) => {
                for update in updates {
                    self.grid(update);
                }
            }
            PaneEvent::Chunk(_, _, modes @ TerminalChunk::Modes { .. }) => {
                self.modes = Some(modes.clone());
                for view in self.views.values() {
                    let _ = view.events.send(PaneEvent::Chunk(
                        self.id.clone(),
                        view.generation,
                        modes.clone(),
                    ));
                }
            }
            PaneEvent::InputFeedback(_, message) => {
                for view in self.views.values() {
                    let _ = view
                        .events
                        .send(PaneEvent::InputFeedback(self.id.clone(), message.clone()));
                }
            }
            _ => {}
        }
    }

    fn grid(&mut self, update: GridUpdate) {
        if let Some(hold) = &mut self.hold {
            if hold.park(update) {
                self.release_hold();
            }
        } else {
            self.apply(update);
        }
    }

    fn release_hold(&mut self) {
        if let Some(hold) = self.hold.take() {
            let mut updates = hold.parked.into_iter();
            if let Some(mut update) = updates.next() {
                for next in updates {
                    update.coalesce(next);
                }
                self.apply(update);
            }
        }
    }

    fn apply(&mut self, update: GridUpdate) {
        for view in self.views.values() {
            if let Some(damage) = &view.damage {
                damage.prepare(&update);
            }
        }
        let changed = self.buffer.write().unwrap().apply(update).changed;
        for view in self.views.values() {
            let _ = view.events.send(PaneEvent::ControllerDamage(
                self.id.clone(),
                view.generation,
                changed,
            ));
        }
    }
}

fn spawn_transport(
    runtime: &Handle,
    socket: PathBuf,
    id: SessionId,
    control: Arc<Mutex<ControlState>>,
    events: PaneEventSender,
    mut shutdown: oneshot::Receiver<()>,
    drain: (Option<watch::Receiver<bool>>, DrainFinished),
) {
    let (previous, finished) = drain;
    runtime.spawn(async move {
        let _finished = finished;
        // A rapid unmount/remount cannot open a replacement attachment ahead
        // of the previous controller's accepted-input drain.
        if let Some(mut previous) = previous {
            let mut cancelled = false;
            while !*previous.borrow_and_update() {
                tokio::select! {
                    _ = &mut shutdown, if !cancelled => cancelled = true,
                    result = previous.changed() => if result.is_err() { break; }
                }
            }
            // A cancelled intermediate mount still carries its predecessor's
            // drain barrier. Otherwise A→B→C could let C attach ahead of A.
            if cancelled { return; }
        }
        loop {
            let connect = SessionAttachment::connect(&socket, id.clone());
            let connected = tokio::select! {
                _ = &mut shutdown => return,
                result = tokio::time::timeout(Duration::from_secs(2), connect) => result,
            };
            if let Ok(Ok(mut attachment)) = connected {
                let writer = attachment.handle();
                let resize_wake = {
                    let mut state = control.lock().unwrap();
                    if let Some(size) = state.pending_resize.take().or(state.last_resize) {
                        let _ = writer.resize(size.0, size.1);
                        state.last_resize = Some(size);
                    }
                    state.writer = Some(writer.clone());
                    state.resize_wake.clone()
                };
                let _ = events.send(PaneEvent::AttachmentState(id.clone(), 0, AttachmentState::Live));
                let mut resize_wait = None;
                let stopping = loop {
                    let pending_resize = {
                        let state = control.lock().unwrap();
                        state.owner != 0 && state.pending_resize.is_some()
                    };
                    if pending_resize && resize_wait.is_none() {
                        let writer = writer.clone();
                        resize_wait = Some(Box::pin(async move { writer.reserve_resize().await }));
                    } else if !pending_resize { resize_wait = None; }
                    tokio::select! {
                        _ = resize_wake.notified() => {},
                        reservation = async {
                            match &mut resize_wait {
                                Some(wait) => wait.await,
                                None => std::future::pending().await,
                            }
                        } => {
                            resize_wait = None;
                            match reservation {
                                Ok(reservation) => {
                                    let mut state = control.lock().unwrap();
                                    if state.owner != 0 && let Some(size) = state.pending_resize.take() {
                                        reservation.send(size.0, size.1);
                                        state.last_resize = Some(size);
                                    }
                                }
                                Err(_) => break false,
                            }
                        }
                        _ = &mut shutdown => break true,
                        chunk = attachment.chunks.recv() => match chunk {
                            Some(chunk) => { let _ = events.send(PaneEvent::Chunk(id.clone(), 0, chunk)); }
                            None => break false,
                        }
                    }
                };
                drop(resize_wait);
                // Linearize closed admission before draining. The existing
                // single queue keeps all commands accepted before this point.
                control.lock().unwrap().writer = None;
                if !matches!(tokio::time::timeout(Duration::from_secs(2), attachment.close_checked()).await, Ok(Ok(()))) {
                    // Payload-free diagnostic also covers EOF/write failure
                    // during last-view close, when no view remains to notify.
                    eprintln!("diri: terminal attachment drain interrupted; queued input may not have reached the session");
                }
                if stopping { return; }
                let _ = events.send(PaneEvent::InputFeedback(id.clone(),
                    "Terminal connection interrupted. Recent input may not have reached the session; it will not be replayed.".into()));
            }
            let _ = events.send(PaneEvent::AttachmentState(id.clone(), 0, AttachmentState::Reconnecting));
            tokio::select! {
                _ = &mut shutdown => return,
                _ = tokio::time::sleep(REATTACH_DELAY) => {}
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use diri_proto::frames::{Frame, FrameCodec, FrameType};
    use diri_proto::grid::{ChangedRow, GridCell};
    use gpui::TestAppContext;
    use std::sync::atomic::AtomicUsize;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixListener;

    use super::*;

    struct FakeEngine {
        path: PathBuf,
        connects: Arc<AtomicUsize>,
        frames: mpsc::UnboundedReceiver<Frame>,
        output: tokio::sync::broadcast::Sender<GridUpdate>,
        _task: tokio::task::JoinHandle<()>,
    }

    impl FakeEngine {
        fn start(runtime: &tokio::runtime::Runtime) -> Self {
            let path = std::env::temp_dir().join(format!(
                "diri-controller-{}-{}.sock",
                std::process::id(),
                NEXT_VIEW.fetch_add(1, Ordering::Relaxed)
            ));
            let _enter = runtime.enter();
            let listener = UnixListener::bind(&path).unwrap();
            let connects = Arc::new(AtomicUsize::new(0));
            let count = connects.clone();
            let (frames_tx, frames) = mpsc::unbounded_channel();
            let (output, _) = tokio::sync::broadcast::channel::<GridUpdate>(8);
            let grids = output.clone();
            let task = runtime.spawn(async move {
                while let Ok((stream, _)) = listener.accept().await {
                    let mut updates = grids.subscribe();
                    let frames = frames_tx.clone();
                    let count = count.clone();
                    tokio::spawn(async move {
                        let mut stream = BufReader::new(stream);
                        let mut hello = String::new();
                        stream.read_line(&mut hello).await.unwrap();
                        let request: diri_proto::methods::AttachRequest = serde_json::from_str(&hello).unwrap();
                        assert_eq!(request.attach, SessionId("shared-session".into()));
                        count.fetch_add(1, Ordering::SeqCst);
                        let mut codec = FrameCodec::new();
                        let mut bytes = [0; 8192];
                        loop {
                            tokio::select! {
                                read = stream.read(&mut bytes) => {
                                    let Ok(length) = read else { return; };
                                    if length == 0 { return; }
                                    for frame in codec.feed(&bytes[..length]).unwrap() {
                                        frames.send(frame).unwrap();
                                    }
                                }
                                Ok(update) = updates.recv() => {
                                    let frame = Frame::grid(&update).unwrap();
                                    if stream.get_mut().write_all(&FrameCodec::encode(&frame).unwrap()).await.is_err() { return; }
                                }
                            }
                        }
                    });
                }
            });
            Self {
                path,
                connects,
                frames,
                output,
                _task: task,
            }
        }
    }

    impl Drop for FakeEngine {
        fn drop(&mut self) {
            self._task.abort();
            let _ = std::fs::remove_file(&self.path);
        }
    }

    fn wait_for(
        cx: &mut TestAppContext,
        runtime: &tokio::runtime::Runtime,
        mut condition: impl FnMut() -> bool,
    ) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while !condition() {
            assert!(Instant::now() < deadline, "controller condition timed out");
            cx.run_until_parked();
            runtime.block_on(async {
                tokio::time::sleep(Duration::from_millis(1)).await;
            });
        }
        cx.run_until_parked();
    }

    fn frame(sequence: u64, scalar: char) -> GridUpdate {
        GridUpdate {
            cols: 2,
            rows: 1,
            cursor_col: 0,
            cursor_row: 0,
            cursor_visible: true,
            is_full_snapshot: sequence == 1,
            changed_rows: vec![ChangedRow::new(
                0,
                vec![
                    GridCell {
                        scalar: scalar as u32,
                        ..GridCell::BLANK
                    };
                    2
                ],
            )],
        }
    }

    #[test]
    fn cancelled_intermediate_mount_cannot_skip_an_earlier_drain() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let engine = FakeEngine::start(&runtime);
        let (events, _rx) = pane_event_channel();
        let state = || {
            Arc::new(Mutex::new(ControlState {
                owner: 1,
                ownership_revision: 0,
                writer: None,
                last_resize: None,
                pending_resize: None,
                resize_wake: Arc::new(Notify::new()),
            }))
        };
        let (prior_done, prior) = watch::channel(false);
        let (middle_done, middle) = watch::channel(false);
        let (middle_stop, middle_shutdown) = oneshot::channel();
        spawn_transport(
            runtime.handle(),
            engine.path.clone(),
            SessionId("shared-session".into()),
            state(),
            events.clone(),
            middle_shutdown,
            (Some(prior), DrainFinished(middle_done)),
        );
        middle_stop.send(()).unwrap();
        let (last_done, _last) = watch::channel(false);
        let (last_stop, last_shutdown) = oneshot::channel();
        let last_state = state();
        spawn_transport(
            runtime.handle(),
            engine.path.clone(),
            SessionId("shared-session".into()),
            last_state.clone(),
            events,
            last_shutdown,
            (Some(middle), DrainFinished(last_done)),
        );
        runtime.block_on(async {
            tokio::time::sleep(Duration::from_millis(10)).await;
        });
        assert_eq!(engine.connects.load(Ordering::SeqCst), 0);
        prior_done.send(true).unwrap();
        runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(2), async {
                while last_state.lock().unwrap().writer.is_none()
                    || engine.connects.load(Ordering::SeqCst) == 0
                {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            })
            .await
            .unwrap();
        });
        assert_eq!(engine.connects.load(Ordering::SeqCst), 1);
        last_stop.send(()).unwrap();
    }

    #[test]
    fn measure_control_gate_cost_without_terminal_locking() {
        let (events, _rx) = pane_event_channel();
        let control = AttachmentControl {
            state: Arc::new(Mutex::new(ControlState {
                owner: 1,
                ownership_revision: 0,
                writer: None,
                last_resize: None,
                pending_resize: None,
                resize_wake: Arc::new(Notify::new()),
            })),
            view: 1,
            events,
            id: SessionId("benchmark".into()),
            input_observer: None,
        };
        let mut samples = Vec::new();
        for _ in 0..1000 {
            let started = Instant::now();
            for _ in 0..100 {
                control.claim();
                std::hint::black_box(control.is_controller());
            }
            samples.push(started.elapsed().as_nanos() / 100);
        }
        samples.sort_unstable();
        eprintln!(
            "controller claim + admission-owner check: median {} ns, p95 {} ns (no terminal/grid mutex)",
            samples[500], samples[950]
        );
        assert!(control.is_controller());
    }

    #[gpui::test]
    fn stalled_writer_coalesces_resizes_without_feedback_or_stale_owner_geometry(
        cx: &mut TestAppContext,
    ) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let mut engine = FakeEngine::start(&runtime);
        let (events, rx) = pane_event_channel();
        let (lease, control, _) = cx.update(|cx| {
            ControllerLease::mount(
                engine.path.clone(),
                SessionId("shared-session".into()),
                runtime.handle(),
                events,
                1,
                None,
                cx,
            )
        });
        wait_for(cx, &runtime, || {
            control.state.lock().unwrap().writer.is_some()
                && engine.connects.load(Ordering::SeqCst) == 1
        });
        assert!(!control.is_controller(), "first mount starts passive");
        control.claim();
        // Park the socket writer with its entire byte budget occupied while
        // GPUI keeps processing layout. Geometry must create no feedback loop.
        control
            .submit(AttachmentCommand::Input(vec![b'x'; 1024 * 1024]))
            .unwrap();
        for cols in 40..140 {
            control.resize(cols, 30);
            cx.run_until_parked();
        }
        assert_eq!(
            control.state.lock().unwrap().pending_resize,
            Some((139, 30))
        );
        assert!(
            !control.needs_resize((139, 30)),
            "pending geometry does not request another render retry"
        );
        assert!(
            !rx.state
                .lock()
                .unwrap()
                .events
                .iter()
                .any(|event| matches!(event, PaneEvent::InputFeedback(..)))
        );
        control.input(b"rejected typing".to_vec());
        assert_eq!(
            rx.state
                .lock()
                .unwrap()
                .events
                .iter()
                .filter(|event| matches!(event, PaneEvent::InputFeedback(..)))
                .count(),
            1,
            "actual rejected input remains visible"
        );
        wait_for(cx, &runtime, || {
            control.state.lock().unwrap().pending_resize.is_none()
        });
        let next_resize = |engine: &mut FakeEngine| {
            runtime.block_on(async {
                loop {
                    let frame = tokio::time::timeout(Duration::from_secs(2), engine.frames.recv())
                        .await
                        .unwrap()
                        .unwrap();
                    if let Some(size) = frame.resize_payload() {
                        break size;
                    }
                }
            })
        };
        assert_eq!(
            next_resize(&mut engine),
            (139, 30),
            "latest geometry drains without any new layout or feedback event"
        );

        control
            .submit(AttachmentCommand::Input(vec![b'y'; 1024 * 1024]))
            .unwrap();
        control.resize(200, 40);
        let (next_events, _next_rx) = pane_event_channel();
        let (next_lease, next, _) = cx.update(|cx| {
            ControllerLease::mount(
                engine.path.clone(),
                SessionId("shared-session".into()),
                runtime.handle(),
                next_events,
                2,
                None,
                cx,
            )
        });
        next.claim();
        assert!(
            next.state.lock().unwrap().pending_resize.is_none(),
            "transfer cancels unadmitted old geometry"
        );
        next.resize(77, 22);
        control.resize(250, 50); // stale view cannot replace the new request
        wait_for(cx, &runtime, || {
            next.state.lock().unwrap().pending_resize.is_none()
        });
        assert_eq!(next_resize(&mut engine), (77, 22));
        let old_revision = next.ownership_revision();
        control.claim();
        next.claim();
        next.resize_if_current((250, 50), old_revision);
        assert_eq!(
            next.state.lock().unwrap().last_resize,
            None,
            "an old cadence tick stays obsolete after same-view reacquisition"
        );
        next.resize_if_current((80, 24), next.ownership_revision());
        assert_eq!(next_resize(&mut engine), (80, 24));
        next.release();
        assert!(!next.is_controller());
        assert_eq!(
            next.state.lock().unwrap().last_resize,
            None,
            "released geometry cannot seed a reconnect"
        );
        assert_eq!(engine.connects.load(Ordering::SeqCst), 1);
        drop(lease);
        drop(next_lease);
    }

    #[gpui::test]
    fn two_views_share_one_transport_and_grid_but_keep_selection_local(cx: &mut TestAppContext) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let mut engine = FakeEngine::start(&runtime);
        let (events_a, mut rx_a) = pane_event_channel();
        let (events_b, mut rx_b) = pane_event_channel();
        let (first, a, grid_a) = cx.update(|cx| {
            ControllerLease::mount(
                engine.path.clone(),
                SessionId("shared-session".into()),
                runtime.handle(),
                events_a,
                1,
                None,
                cx,
            )
        });
        let (second, b, grid_b) = cx.update(|cx| {
            ControllerLease::mount(
                engine.path.clone(),
                SessionId("shared-session".into()),
                runtime.handle(),
                events_b,
                2,
                None,
                cx,
            )
        });
        assert!(Arc::ptr_eq(&grid_a, &grid_b));
        let view_a = TerminalElement::new(grid_a.clone());
        let view_b = TerminalElement::new(grid_b.clone());
        first.observe(&view_a);
        second.observe(&view_b);
        wait_for(cx, &runtime, || {
            a.state.lock().unwrap().writer.is_some() && engine.connects.load(Ordering::SeqCst) == 1
        });
        assert_eq!(engine.connects.load(Ordering::SeqCst), 1);
        assert_eq!(
            b.submit(AttachmentCommand::Resize(90, 30)),
            Err(InputRejection::PassiveView)
        );
        assert_eq!(
            b.submit(AttachmentCommand::Input(b"no".to_vec())),
            Err(InputRejection::PassiveView)
        );
        assert!(!a.is_controller(), "mount order does not confer ownership");
        a.claim();
        a.submit(AttachmentCommand::Input(b"before".to_vec()))
            .unwrap();
        b.claim();
        b.submit(AttachmentCommand::Input(b"after".to_vec()))
            .unwrap();
        assert_eq!(
            a.submit(AttachmentCommand::Input(b"stale".to_vec())),
            Err(InputRejection::PassiveView)
        );
        engine.output.send(frame(1, 'A')).unwrap();
        wait_for(cx, &runtime, || {
            grid_b
                .read()
                .unwrap()
                .cells
                .first()
                .is_some_and(|cell| cell.scalar == 'A' as u32)
        });
        let mut batch = Vec::new();
        runtime.block_on(rx_a.recv_batch(&mut batch));
        assert_eq!(
            batch
                .iter()
                .filter(|event| matches!(event, PaneEvent::ControllerDamage(_, 1, true)))
                .count(),
            1
        );
        batch.clear();
        runtime.block_on(rx_b.recv_batch(&mut batch));
        assert_eq!(
            batch
                .iter()
                .filter(|event| matches!(event, PaneEvent::ControllerDamage(_, 2, true)))
                .count(),
            1
        );
        drop(first);
        drop(a);
        // The second view and queued commands survive the original owner.
        let inputs = runtime.block_on(async {
            let mut inputs = Vec::new();
            while inputs.len() < 2 {
                let frame = tokio::time::timeout(Duration::from_secs(2), engine.frames.recv())
                    .await
                    .unwrap()
                    .unwrap();
                if frame.frame_type == FrameType::Input {
                    inputs.push(frame.payload);
                }
            }
            inputs
        });
        assert_eq!(inputs, [b"before".to_vec(), b"after".to_vec()]);
        assert_eq!(engine.connects.load(Ordering::SeqCst), 1);
        view_b.begin_selection(0, 0);
        view_b.drag_selection(1, 0);
        assert!(!view_b.selected_text().is_empty());
        assert!(view_a.selected_text().is_empty());
        engine.output.send(frame(2, 'B')).unwrap();
        wait_for(cx, &runtime, || {
            grid_b.read().unwrap().cells[0].scalar == 'B' as u32
        });
        assert!(
            view_b.selected_text().is_empty(),
            "view-local selection is invalidated before shared damage"
        );
        batch.clear();
        runtime.block_on(rx_b.recv_batch(&mut batch));
        assert_eq!(
            batch
                .iter()
                .filter(|event| matches!(event, PaneEvent::ControllerDamage(_, 2, true)))
                .count(),
            1
        );
        assert!(
            rx_a.wake.try_recv().is_err(),
            "unmounted view receives no new frame"
        );
        let (events_c, _rx_c) = pane_event_channel();
        let (third, c, grid_c) = cx.update(|cx| {
            ControllerLease::mount(
                engine.path.clone(),
                SessionId("shared-session".into()),
                runtime.handle(),
                events_c,
                3,
                None,
                cx,
            )
        });
        let view_c = TerminalElement::new(grid_c.clone());
        third.observe(&view_c);
        assert!(!c.is_controller());
        drop(second);
        drop(b);
        assert!(
            !c.is_controller(),
            "closing the owner does not silently grant passive resize"
        );
        assert_eq!(
            c.submit(AttachmentCommand::Resize(20, 10)),
            Err(InputRejection::PassiveView)
        );
        engine.output.send(frame(3, 'C')).unwrap();
        wait_for(cx, &runtime, || {
            grid_c.read().unwrap().cells[0].scalar == 'C' as u32
        });
        assert_eq!(engine.connects.load(Ordering::SeqCst), 1);
        c.claim();
        assert!(c.is_controller());
        drop(third);
    }

    #[gpui::test]
    fn owner_drop_does_not_grant_passive_resize_and_remount_waits_for_drain(
        cx: &mut TestAppContext,
    ) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let engine = FakeEngine::start(&runtime);
        let (tx, _rx) = pane_event_channel();
        let (lease, control, _) = cx.update(|cx| {
            ControllerLease::mount(
                engine.path.clone(),
                SessionId("shared-session".into()),
                runtime.handle(),
                tx.clone(),
                1,
                None,
                cx,
            )
        });
        wait_for(cx, &runtime, || {
            control.state.lock().unwrap().writer.is_some()
                && engine.connects.load(Ordering::SeqCst) == 1
        });
        control.claim();
        control
            .submit(AttachmentCommand::Input(b"queued before close".to_vec()))
            .unwrap();
        let previous = cx.update(|cx| {
            cx.global::<Controllers>()
                .0
                .values()
                .next()
                .unwrap()
                .drained
                .clone()
        });
        drop(lease);
        drop(control);
        let (replacement, next, _) = cx.update(|cx| {
            ControllerLease::mount(
                engine.path.clone(),
                SessionId("shared-session".into()),
                runtime.handle(),
                tx,
                2,
                None,
                cx,
            )
        });
        wait_for(cx, &runtime, || next.state.lock().unwrap().writer.is_some());
        assert!(
            *previous.borrow(),
            "previous writer drains before replacement attaches"
        );
        assert_eq!(engine.connects.load(Ordering::SeqCst), 2);
        drop(replacement);
    }
}
