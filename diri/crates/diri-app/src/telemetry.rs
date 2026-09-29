//! The app's side of the flight recorder (`diri-telemetry`): startup,
//! resource gauges, lifecycle events, a main-thread stall watchdog, frame
//! timing, and the storage behind Settings > General > Privacy and
//! Help > Report a Problem. The event catalog is the App section of
//! `diri/TELEMETRY.md`.
//!
//! Recording is a no-op until [`start`] ran, and `start` never runs in
//! tests, headless previews or screenshot fixtures, so none of them record
//! into the real home.

use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use diri_telemetry::{Config, Identity, Process, Value, debug_event, event, id, incident};
use gpui::{App, IntoElement, Styled as _, Window, canvas};

/// Open main windows, floating panels (menus, palette, popovers), terminal
/// panes and mounted session transports: the counts that grow with a leak.
static MAIN_WINDOWS: AtomicU64 = AtomicU64::new(0);
static FLOATING_WINDOWS: AtomicU64 = AtomicU64::new(0);
static TERMINAL_PANES: AtomicU64 = AtomicU64::new(0);
static ATTACHED_SESSIONS: AtomicU64 = AtomicU64::new(0);
/// Windows opened over the process lifetime, to tell churn from a leak.
static WINDOWS_OPENED: AtomicU64 = AtomicU64::new(0);

/// Set when the app is frontmost; the stall watchdog paces itself on it.
static APP_ACTIVE: AtomicBool = AtomicBool::new(true);
static LAUNCH_RECORDED: AtomicBool = AtomicBool::new(false);

/// A frame longer than this records `ui.slow_frame`.
const SLOW_FRAME: Duration = Duration::from_millis(50);
/// Main-thread unresponsiveness recorded as `ui.stall`.
const STALL: Duration = Duration::from_secs(1);
/// A stall this long is something the user noticed: an incident.
const STALL_INCIDENT: Duration = Duration::from_secs(3);
/// A stall still going after this long is recorded (and flushed) before it
/// ends, so a hang that ends in Force Quit still leaves a record.
const STALL_ONGOING: Duration = Duration::from_secs(5);
/// Watchdog ping cadence while diri is frontmost, and while it is not.
const PING_ACTIVE: Duration = Duration::from_secs(1);
const PING_INACTIVE: Duration = Duration::from_secs(5);

fn process_started() -> Instant {
    static STARTED: OnceLock<Instant> = OnceLock::new();
    *STARTED.get_or_init(Instant::now)
}

/// Starts recording for the desktop app. Call first thing in `main`, after
/// deciding whether this launch is a headless preview (which never records).
pub(crate) fn start(preview: bool) {
    let _ = process_started();
    if preview || std::env::var_os("DIRI_SETTINGS_PREVIEW").is_some() {
        return;
    }
    if !diri_telemetry::init_default(Process::App) {
        return;
    }
    diri_telemetry::install_panic_hook();
    diri_telemetry::register_gauge("windows_main", || gauge(&MAIN_WINDOWS));
    diri_telemetry::register_gauge("windows_floating", || gauge(&FLOATING_WINDOWS));
    diri_telemetry::register_gauge("windows_opened", || gauge(&WINDOWS_OPENED));
    diri_telemetry::register_gauge("terminal_panes", || gauge(&TERMINAL_PANES));
    diri_telemetry::register_gauge("attached_sessions", || gauge(&ATTACHED_SESSIONS));
    diri_telemetry::register_gauge("app_active", || {
        Value::from(APP_ACTIVE.load(Ordering::Relaxed))
    });
    diri_telemetry::start_health_sampler(Duration::from_secs(60));
}

fn gauge(counter: &AtomicU64) -> Value {
    Value::from(counter.load(Ordering::Relaxed))
}

/// Hooks that need the running GPUI app: the stall watchdog, app
/// activation/sleep/wake, and shortcut-driven actions. Runs once, from
/// `app.run`.
pub(crate) fn install(cx: &mut App) {
    if !diri_telemetry::is_enabled() {
        return;
    }
    start_stall_watchdog(cx);
    #[cfg(target_os = "macos")]
    crate::macos::observe_app_lifecycle();
    // Shortcuts resolve to a named action; the keystroke itself is never
    // recorded. Typing into a terminal resolves to no action.
    cx.observe_keystrokes(|event, _, _| {
        if let Some(action) = &event.action {
            debug_event!("ui.action", action = action.name(), source = "shortcut");
        }
    })
    .detach();
}

/// A named action run from somewhere other than a key binding.
pub(crate) fn action(name: &'static str, source: &'static str) {
    debug_event!("ui.action", action = name, source = source);
}

// Only the macOS app delegate reports these.
#[cfg(target_os = "macos")]
pub(crate) fn app_activation_changed(active: bool) {
    APP_ACTIVE.store(active, Ordering::Relaxed);
    if active {
        event!("app.activate");
    } else {
        event!("app.deactivate");
    }
}

// Only the macOS app delegate reports these.
#[cfg(target_os = "macos")]
pub(crate) fn system_sleep(sleeping: bool) {
    if sleeping {
        event!("app.sleep");
        // The machine may not come back before the battery dies.
        diri_telemetry::flush(Duration::from_millis(200));
    } else {
        event!("app.wake");
    }
}

/// Records the quit and waits (bounded) for it to reach the spool.
pub(crate) fn quitting() {
    if !diri_telemetry::is_enabled() {
        return;
    }
    event!(
        "app.quit",
        uptime_s = process_started().elapsed().as_secs(),
        windows_main = MAIN_WINDOWS.load(Ordering::Relaxed),
        windows_opened = WINDOWS_OPENED.load(Ordering::Relaxed)
    );
    diri_telemetry::flush(Duration::from_millis(150));
}

/// Lives as long as one platform window; records its open and close and
/// keeps the window gauges. `kind` is `main` or `floating`.
pub(crate) struct WindowGuard {
    kind: &'static str,
    window: u64,
    opened: Instant,
}

impl WindowGuard {
    pub(crate) fn new(kind: &'static str, window: &Window) -> Self {
        let window = window.window_handle().window_id().as_u64();
        counter(kind).fetch_add(1, Ordering::Relaxed);
        WINDOWS_OPENED.fetch_add(1, Ordering::Relaxed);
        if kind == "main" {
            event!("window.open", kind = kind, window = window);
        } else {
            debug_event!("window.open", kind = kind, window = window);
        }
        Self {
            kind,
            window,
            opened: Instant::now(),
        }
    }
}

impl Drop for WindowGuard {
    fn drop(&mut self) {
        let open = counter(self.kind)
            .fetch_sub(1, Ordering::Relaxed)
            .saturating_sub(1);
        let lived_s = self.opened.elapsed().as_secs();
        if self.kind == "main" {
            event!(
                "window.close",
                kind = self.kind,
                window = self.window,
                lived_s = lived_s,
                open = open
            );
        } else {
            debug_event!(
                "window.close",
                kind = self.kind,
                window = self.window,
                lived_s = lived_s,
                open = open
            );
        }
    }
}

fn counter(kind: &'static str) -> &'static AtomicU64 {
    if kind == "main" {
        &MAIN_WINDOWS
    } else {
        &FLOATING_WINDOWS
    }
}

/// Keeps a live-instance gauge for as long as it is held.
pub(crate) struct Live(&'static AtomicU64);

impl Live {
    pub(crate) fn pane() -> Self {
        TERMINAL_PANES.fetch_add(1, Ordering::Relaxed);
        Self(&TERMINAL_PANES)
    }

    pub(crate) fn attached_session() -> Self {
        ATTACHED_SESSIONS.fetch_add(1, Ordering::Relaxed);
        Self(&ATTACHED_SESSIONS)
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

/// What a main window was showing, for `ui.slow_frame`.
#[derive(Clone, Copy)]
pub(crate) struct FrameContext {
    pub(crate) surface: &'static str,
    pub(crate) workspace: bool,
}

/// A zero-size element painted last in a main window: the time from the
/// start of the root view's render to here is the frame's CPU cost (render,
/// layout, prepaint, paint of everything before it; not GPU present).
pub(crate) fn frame_probe(started: Instant, context: FrameContext) -> impl IntoElement {
    canvas(
        |_, _, _| {},
        move |_, _, window, _| {
            if !diri_telemetry::is_enabled() {
                return;
            }
            let cost = started.elapsed();
            diri_telemetry::observe("ui.frame", cost);
            if !LAUNCH_RECORDED.swap(true, Ordering::Relaxed) {
                event!(
                    "app.launch",
                    ms = process_started().elapsed(),
                    version = crate::updates::CURRENT_VERSION,
                    windows = MAIN_WINDOWS.load(Ordering::Relaxed)
                );
            }
            if cost >= SLOW_FRAME {
                diri_telemetry::warn_event!(
                    "ui.slow_frame",
                    ms = cost,
                    window = window.window_handle().window_id().as_u64(),
                    surface = context.surface,
                    workspace = context.workspace
                );
            }
        },
    )
    .absolute()
    .size_0()
}

/// Watches main-thread responsiveness without polling it. A background
/// thread posts a ping once a second (once every five while diri is in the
/// background) and the main thread answers when it next runs. The answer's
/// latency is the stall; a ping still unanswered after five seconds is
/// recorded and flushed on the spot, since the user may be about to Force
/// Quit. Idle cost: one wakeup per interval on each side, no timers on the
/// main thread, and the measured stall is a lower bound (±1 interval).
fn start_stall_watchdog(cx: &mut App) {
    let base = Instant::now();
    let elapsed_ms = move || u64::try_from(base.elapsed().as_millis()).unwrap_or(u64::MAX);
    // 0 means no ping is outstanding; otherwise the ms (since `base`, plus 1)
    // it was sent at.
    let outstanding = std::sync::Arc::new(AtomicU64::new(0));
    let (ping_tx, mut ping_rx) = tokio::sync::mpsc::channel::<()>(1);

    let answered = std::sync::Arc::clone(&outstanding);
    cx.spawn(async move |_| {
        while ping_rx.recv().await.is_some() {
            let sent = answered.swap(0, Ordering::AcqRel);
            if sent == 0 {
                continue;
            }
            let latency = Duration::from_millis(elapsed_ms().saturating_sub(sent - 1));
            if latency >= STALL {
                record_stall(latency, false);
            }
        }
    })
    .detach();

    let _ = std::thread::Builder::new()
        .name("diri-stall-watchdog".into())
        .spawn(move || {
            let mut reported_ongoing = false;
            loop {
                let interval = if APP_ACTIVE.load(Ordering::Relaxed) {
                    PING_ACTIVE
                } else {
                    PING_INACTIVE
                };
                std::thread::sleep(interval);
                let sent = outstanding.load(Ordering::Acquire);
                if sent == 0 {
                    reported_ongoing = false;
                    outstanding.store(elapsed_ms() + 1, Ordering::Release);
                    if ping_tx.try_send(()).is_err() && ping_tx.is_closed() {
                        return;
                    }
                    continue;
                }
                let stalled = Duration::from_millis(elapsed_ms().saturating_sub(sent - 1));
                if stalled >= STALL_ONGOING && !reported_ongoing {
                    reported_ongoing = true;
                    record_stall(stalled, true);
                    diri_telemetry::flush(Duration::from_secs(1));
                }
            }
        });
}

fn record_stall(duration: Duration, ongoing: bool) {
    let active = APP_ACTIVE.load(Ordering::Relaxed);
    if duration >= STALL_INCIDENT {
        incident!(
            "ui.stall",
            ms = duration,
            ongoing = ongoing,
            active = active
        );
    } else {
        diri_telemetry::warn_event!(
            "ui.stall",
            ms = duration,
            ongoing = ongoing,
            active = active
        );
    }
}

/// Milliseconds since the process started, plus one, so zero can mean
/// "nothing pending" in an atomic.
fn mono_ms() -> u64 {
    u64::try_from(process_started().elapsed().as_millis())
        .unwrap_or(u64::MAX - 1)
        .saturating_add(1)
}

/// Input → next screen change, per session: how long typing takes to show.
/// Only the first input of a burst is timed, and only with atomics, so the
/// input and grid paths take no lock for it. Output that happens to arrive
/// after a keystroke counts as its echo, so this is an upper bound on
/// responsiveness, not an exact echo time.
#[derive(Default)]
pub(crate) struct EchoProbe(AtomicU64);

/// Longer than this is an agent thinking, not a terminal being slow.
const ECHO_MAX: Duration = Duration::from_secs(2);

impl EchoProbe {
    pub(crate) fn sent(&self) {
        if diri_telemetry::is_enabled() {
            let _ = self
                .0
                .compare_exchange(0, mono_ms(), Ordering::AcqRel, Ordering::Relaxed);
        }
    }

    pub(crate) fn screen_changed(&self) {
        let sent = self.0.swap(0, Ordering::AcqRel);
        if sent == 0 {
            return;
        }
        let latency = Duration::from_millis(mono_ms().saturating_sub(sent));
        if latency <= ECHO_MAX {
            diri_telemetry::observe("input.echo", latency);
        }
    }
}

/// PTY sizes that keep flipping back (A→B→A→B…) are two layouts fighting,
/// which shows as a jittering, reflowing terminal. A drag only ever moves
/// forward, so it never counts.
#[derive(Default)]
pub(crate) struct ResizeStorm {
    previous: Option<(u16, u16)>,
    current: Option<(u16, u16)>,
    flips: u32,
    since: Option<Instant>,
    reported: Option<Instant>,
}

const STORM_FLIPS: u32 = 10;
const STORM_WINDOW: Duration = Duration::from_secs(3);

impl ResizeStorm {
    pub(crate) fn note(&mut self, session: &diri_proto::SessionId, size: (u16, u16)) {
        if self.current == Some(size) {
            return;
        }
        let now = Instant::now();
        if self
            .since
            .is_none_or(|since| now.duration_since(since) > STORM_WINDOW)
        {
            self.since = Some(now);
            self.flips = 0;
        }
        if self.previous == Some(size) {
            self.flips += 1;
        }
        self.previous = self.current;
        self.current = Some(size);
        if self.flips >= STORM_FLIPS
            && self
                .reported
                .is_none_or(|at| now.duration_since(at) > Duration::from_secs(60))
        {
            self.reported = Some(now);
            diri_telemetry::warn_event!(
                "pane.resize_storm",
                session = id(&session.0),
                flips = self.flips,
                cols = size.0,
                rows = size.1
            );
        }
    }
}

/// One session transport's attach history, kept on its own task so the
/// chunk path only bumps local counters: attach latency, first frame, how
/// often the Engine re-seeded the grid, and why it dropped.
pub(crate) struct TransportTrace {
    session: diri_telemetry::Id,
    mounted_at: Instant,
    attempt_at: Instant,
    failures: u32,
    attaches: u64,
    live_at: Option<Instant>,
    grids: u64,
    snapshots: u64,
}

/// Failed attaches in a row before the pane is recorded as failing.
const ATTACH_FAILING_AFTER: u32 = 3;

impl TransportTrace {
    pub(crate) fn new(session: &diri_proto::SessionId) -> Self {
        let now = Instant::now();
        Self {
            session: id(&session.0),
            mounted_at: now,
            attempt_at: now,
            failures: 0,
            attaches: 0,
            live_at: None,
            grids: 0,
            snapshots: 0,
        }
    }

    pub(crate) fn connecting(&mut self) {
        self.attempt_at = Instant::now();
    }

    pub(crate) fn connect_failed(&mut self, reason: &'static str, error: Option<&str>) {
        self.failures += 1;
        diri_telemetry::count("pane.attach_retries", 1);
        if self.failures == ATTACH_FAILING_AFTER {
            diri_telemetry::error_event!(
                "pane.attach_failing",
                session = self.session.clone(),
                attempts = self.failures,
                reason = reason,
                error = error.map(diri_telemetry::text),
                since_mount_ms = self.mounted_at.elapsed()
            );
        }
    }

    pub(crate) fn live(&mut self) {
        let connect = self.attempt_at.elapsed();
        diri_telemetry::observe("pane.attach", connect);
        event!(
            "pane.attached",
            session = self.session.clone(),
            reconnect = self.attaches > 0,
            attempts = self.failures + 1,
            connect_ms = connect,
            since_mount_ms = self.mounted_at.elapsed()
        );
        self.attaches += 1;
        self.failures = 0;
        self.live_at = Some(Instant::now());
        self.grids = 0;
        self.snapshots = 0;
    }

    pub(crate) fn chunk(&mut self, chunk: &diri_client::TerminalChunk) {
        let diri_client::TerminalChunk::Grid(update) = chunk else {
            return;
        };
        self.grids += 1;
        if update.is_full_snapshot {
            self.snapshots += 1;
            if self.snapshots > 1 {
                // The Engine replaced the grid wholesale: a resize re-wrap or
                // a slow reader that fell behind and was re-seeded.
                diri_telemetry::count("pane.reseed", 1);
            }
        }
        if self.grids == 1 {
            let since_live = self.live_at.map(|at| at.elapsed());
            if let Some(since_live) = since_live {
                diri_telemetry::observe("pane.first_grid", since_live);
            }
            if update.is_full_snapshot {
                debug_event!(
                    "pane.first_grid",
                    session = self.session.clone(),
                    ms = since_live
                );
            } else {
                // A diff before any snapshot paints onto a blank grid.
                diri_telemetry::warn_event!(
                    "pane.first_grid",
                    session = self.session.clone(),
                    ms = since_live,
                    snapshot = false
                );
            }
        }
    }

    pub(crate) fn drain_interrupted(&self) {
        diri_telemetry::warn_event!("pane.drain_interrupted", session = self.session.clone());
    }

    pub(crate) fn detached(&mut self) {
        diri_telemetry::warn_event!(
            "pane.detached",
            session = self.session.clone(),
            live_ms = self.live_at.take().map(|at| at.elapsed()),
            grids = self.grids,
            reseeds = self.snapshots.saturating_sub(1)
        );
    }
}

/// Size class of a paste or copy, never its content.
pub(crate) fn size_bucket(bytes: usize) -> &'static str {
    match bytes {
        0 => "0",
        1..=63 => "<64",
        64..=1023 => "<1k",
        1024..=16383 => "<16k",
        16384..=262_143 => "<256k",
        262_144..=1_048_575 => "<1m",
        _ => ">=1m",
    }
}

/// Where the telemetry files live, or `None` in tests (which must never
/// touch the real home) and when `$HOME` is unset.
pub(crate) fn state_dir() -> Option<PathBuf> {
    if cfg!(test) {
        return None;
    }
    diri_telemetry::default_state_dir()
}

/// What Settings > General > Privacy shows and edits.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct PrivacySettings {
    pub(crate) config: Config,
    pub(crate) support_id: Option<String>,
    pub(crate) login_name: Option<String>,
    pub(crate) folder: Option<PathBuf>,
}

impl PrivacySettings {
    pub(crate) fn load() -> Self {
        let Some(dir) = state_dir() else {
            return Self::default();
        };
        Self {
            config: Config::load(&dir),
            support_id: Identity::load_or_create(&dir)
                .ok()
                .map(|identity| identity.support_id()),
            login_name: diri_telemetry::login_name(),
            folder: Some(diri_telemetry::telemetry_dir(&dir)),
        }
    }

    /// The name field's text: what was typed, or the login name it defaults to.
    pub(crate) fn name_text(&self) -> String {
        match &self.config.name {
            Some(name) => name.clone(),
            None => self.login_name.clone().unwrap_or_default(),
        }
    }

    pub(crate) fn save(&self) {
        if let Some(dir) = state_dir()
            && let Err(error) = self.config.save(&dir)
        {
            eprintln!("diri: could not save diagnostics settings: {error}");
            diri_telemetry::error_event!(
                "settings.privacy_save_failed",
                error = diri_telemetry::io_error(&error)
            );
        }
    }
}

/// True the first time a build that records runs: no one has seen, or
/// changed, the diagnostics settings yet. Marks the notice as shown by
/// writing the (default) settings file the Engine's uploader reads.
pub(crate) fn take_first_run_notice() -> bool {
    if !diri_telemetry::is_enabled() {
        return false;
    }
    let Some(dir) = state_dir() else {
        return false;
    };
    let path = diri_telemetry::telemetry_dir(&dir).join("config.json");
    if path.exists() {
        return false;
    }
    if Config::load(&dir).save(&dir).is_err() {
        return false;
    }
    event!("privacy.notice_shown");
    true
}

pub(crate) const REPORT_ISSUE_URL: &str = "https://github.com/cristicretu/diri/issues/new";

/// Help > Report a Problem: marks the moment in the timeline (an incident,
/// so it uploads within a minute), copies the Support ID, and opens a new
/// GitHub issue that already names this install and build.
pub(crate) fn report_problem(cx: &mut App) {
    let support_id = state_dir()
        .and_then(|dir| Identity::load_or_create(&dir).ok())
        .map(|identity| identity.support_id());
    incident!(
        "user.report",
        version = crate::updates::CURRENT_VERSION,
        support_id = support_id.as_deref().map(id)
    );
    diri_telemetry::flush(Duration::from_millis(200));
    if let Some(support_id) = &support_id {
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(support_id.clone()));
    }
    cx.open_url(&report_url(support_id.as_deref()));
}

fn report_url(support_id: Option<&str>) -> String {
    let platform = crate::diagnostics::PlatformMetadata::current();
    let body = format!(
        "What happened?\n\n\nWhat did you expect?\n\n\n---\nSupport ID: {}\ndiri {} ({} {}, {})\n",
        support_id.unwrap_or("unavailable"),
        crate::updates::CURRENT_VERSION,
        platform.os_name,
        platform.os_version,
        platform.architecture,
    );
    let encoded: String = url::form_urlencoded::byte_serialize(body.as_bytes()).collect();
    format!("{REPORT_ISSUE_URL}?body={encoded}")
}

/// With `DIRI_LATENCY_TRACE=1`, follows each keystroke's echo through GPUI's
/// draw, the Metal commit and the compositor as well as the transport; see
/// [`diri_client::latency_trace`]. A no-op otherwise.
pub(crate) fn install_latency_trace() {
    if !diri_client::latency_trace::enabled() {
        return;
    }
    gpui::set_frame_observer(|stage, at| {
        use diri_client::latency_trace::{Hop, mark_at};
        mark_at(
            match stage {
                gpui::FrameStage::DrawStart => Hop::DrawStart,
                gpui::FrameStage::DrawEnd => Hop::DrawEnd,
                gpui::FrameStage::Committed => Hop::Committed,
                gpui::FrameStage::GpuCompleted => Hop::GpuCompleted,
                gpui::FrameStage::Presented => Hop::Presented,
            },
            at,
        );
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_are_bucketed_not_recorded() {
        assert_eq!(size_bucket(0), "0");
        assert_eq!(size_bucket(12), "<64");
        assert_eq!(size_bucket(4096), "<16k");
        assert_eq!(size_bucket(5 << 20), ">=1m");
    }

    #[test]
    fn report_url_prefills_support_id_and_version() {
        let url = report_url(Some("D-7K3MQ9XA"));
        assert!(url.starts_with("https://github.com/cristicretu/diri/issues/new?body="));
        assert!(url.contains("D-7K3MQ9XA"));
        assert!(url.contains(crate::updates::CURRENT_VERSION));
        assert!(!url.contains(' '));
    }

    #[test]
    fn tests_never_touch_the_real_telemetry_dir() {
        assert_eq!(state_dir(), None);
        assert!(!take_first_run_notice());
        assert_eq!(PrivacySettings::load(), PrivacySettings::default());
    }
}
