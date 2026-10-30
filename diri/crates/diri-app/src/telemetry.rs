//! The app's side of the flight recorder (`diri-telemetry`): startup,
//! resource gauges, lifecycle events, a main-thread stall watchdog, frame
//! timing, and the storage behind Settings > General > Privacy and
//! Help > Report a Problem. The event catalog is the App section of
//! `diri/TELEMETRY.md`.
//!
//! Recording is a no-op until [`start`] ran, and `start` never runs in
//! tests, headless previews or screenshot fixtures, so none of them record
//! into the real home.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
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
/// This process is the install's first launch (`activation.first_launch`).
static FIRST_LAUNCH: AtomicBool = AtomicBool::new(false);
/// A local agent catalog arrived in this process before.
static LOCAL_CATALOG_SEEN: AtomicBool = AtomicBool::new(false);

/// A frame longer than this records `ui.slow_frame`.
const SLOW_FRAME: Duration = Duration::from_millis(50);
/// Main-thread unresponsiveness recorded as `ui.stall`.
const STALL: Duration = Duration::from_secs(1);
/// A ping unanswered this long has the main thread's stack sampled once,
/// so the `ui.stall` it becomes names the code it was stuck in.
const STALL_SAMPLE_AFTER: Duration = Duration::from_secs(2);
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
    // Before the first-run privacy notice or anything else writes settings:
    // existing settings mean this Mac used Diri before activation tracking.
    let home = std::env::var_os("HOME").map(PathBuf::from);
    diri_telemetry::activation::init_origin(home.as_deref());
    FIRST_LAUNCH.store(
        diri_telemetry::activation::app_launched(),
        Ordering::Relaxed,
    );
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
    remember_main_thread();
    start_stall_watchdog(cx);
    #[cfg(target_os = "macos")]
    crate::macos::observe_app_lifecycle();
    // Shortcuts resolve to a named action; the keystroke itself is never
    // recorded. Typing into a terminal resolves to no action.
    cx.observe_keystrokes(|event, _, _| {
        if let Some(action) = &event.action {
            action_ran(action.name());
            debug_event!("ui.action", action = action.name(), source = "shortcut");
        }
    })
    .detach();
}

/// `activation.agent_ready`, once per install: the first local agent
/// catalog with a launchable agent. `installing` is the agent the welcome's
/// one-click install is waiting for.
pub(crate) fn agent_catalog_seen(
    installing: Option<&diri_proto::AgentKind>,
    catalog: &diri_proto::AgentReadinessResult,
) {
    let first_catalog = !LOCAL_CATALOG_SEEN.swap(true, Ordering::Relaxed);
    use diri_telemetry::activation::{self, Milestone};
    if activation::is_reached(Milestone::AgentReady) {
        return;
    }
    let Some(ready) = agent_ready(
        installing,
        catalog,
        first_catalog && FIRST_LAUNCH.load(Ordering::Relaxed),
    ) else {
        return;
    };
    activation::reach(
        Milestone::AgentReady,
        vec![
            ("agent", Value::from(id(ready.agent.id()))),
            ("source", Value::from(ready.source)),
            ("agents", Value::from(ready.agents)),
        ],
    );
}

#[derive(Debug, PartialEq, Eq)]
struct AgentReady {
    agent: diri_proto::AgentKind,
    /// `onboarding_install`: the agent Diri's one-click install was waiting
    /// for. `preexisting`: already launchable in the first catalog of the
    /// install's first launch. `manual`: became launchable later, installed
    /// outside Diri.
    source: &'static str,
    /// Launchable agents in the catalog.
    agents: usize,
}

fn agent_ready(
    installing: Option<&diri_proto::AgentKind>,
    catalog: &diri_proto::AgentReadinessResult,
    first_catalog_of_first_launch: bool,
) -> Option<AgentReady> {
    let ready: Vec<_> = crate::agent_catalog::agent_options(catalog)
        .into_iter()
        .filter(|option| option.available && option.kind.id() != diri_proto::AgentKind::NOTE_ID)
        .map(|option| option.kind)
        .collect();
    let agents = ready.len();
    if let Some(kind) = installing.filter(|kind| ready.contains(kind)) {
        return Some(AgentReady {
            agent: kind.clone(),
            source: "onboarding_install",
            agents,
        });
    }
    let agent = ready.into_iter().next()?;
    Some(AgentReady {
        agent,
        source: if first_catalog_of_first_launch {
            "preexisting"
        } else {
            "manual"
        },
        agents,
    })
}

/// A named action run from somewhere other than a key binding.
pub(crate) fn action(name: &'static str, source: &'static str) {
    action_ran(name);
    debug_event!("ui.action", action = name, source = source);
}

/// A Diri Notes feature was used. Counts only: `name` is a fixed event
/// name and `kind` a fixed family ("linear", "bullet"), never note text,
/// titles, URLs or ids. Catalogued under Notes in `diri/TELEMETRY.md`.
pub(crate) fn notes_event(name: &'static str, kind: &'static str) {
    if !diri_telemetry::is_enabled() {
        return;
    }
    let fields = if kind.is_empty() {
        Vec::new()
    } else {
        vec![("kind", Value::from(kind))]
    };
    diri_telemetry::record(name, diri_telemetry::Severity::Info, fields);
}

/// The last action the main thread finished, and when ([`mono_ms`]). An
/// action that ends inside a stall is named on its `ui.stall`.
static LAST_ACTION: Mutex<Option<(&'static str, u64)>> = Mutex::new(None);

fn action_ran(name: &'static str) {
    if let Ok(mut last) = LAST_ACTION.lock() {
        *last = Some((name, mono_ms()));
    }
}

/// The action that finished at or after `since` ([`mono_ms`]), if any.
fn action_since(since: u64) -> Option<&'static str> {
    finished_since(*LAST_ACTION.lock().ok()?, since)
}

fn finished_since(last: Option<(&'static str, u64)>, since: u64) -> Option<&'static str> {
    last.filter(|(_, at)| *at >= since).map(|(name, _)| name)
}

/// The main thread's Mach port, so any thread can read its CPU time; 0
/// until [`install`] ran on the main thread.
#[cfg(target_os = "macos")]
static MAIN_THREAD: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// The main thread, for the stall watchdog's stack sample.
static MAIN_STACK: OnceLock<crate::stall_stack::SampledThread> = OnceLock::new();
/// The main thread's stack sampled during the outstanding stall, keyed by
/// that stall's ping ([`Sent::at`]).
static STALL_STACK: Mutex<Option<(u64, Vec<String>)>> = Mutex::new(None);
/// The surface the last main-window frame showed (`ui.slow_frame`'s
/// `surface`), for `ui.stall`.
static LAST_SURFACE: Mutex<&'static str> = Mutex::new("none");

fn remember_main_thread() {
    if let Some(thread) = crate::stall_stack::SampledThread::current() {
        let _ = MAIN_STACK.set(thread);
    }
    #[cfg(target_os = "macos")]
    {
        // SAFETY: returns the calling thread's port without taking a
        // reference, so there is nothing to release; the main thread lives
        // as long as the process.
        let port = unsafe { libc::pthread_mach_thread_np(libc::pthread_self()) };
        MAIN_THREAD.store(port, Ordering::Relaxed);
    }
}

/// CPU time the main thread has used so far, read from any thread.
fn main_thread_cpu() -> Option<Duration> {
    #[cfg(target_os = "macos")]
    {
        let port = MAIN_THREAD.load(Ordering::Relaxed);
        if port == 0 {
            return None;
        }
        // SAFETY: thread_info fills a caller-owned struct of the flavor's
        // size, given in integer_t units.
        let mut info: libc::thread_basic_info = unsafe { std::mem::zeroed() };
        let mut count = (std::mem::size_of::<libc::thread_basic_info>()
            / std::mem::size_of::<libc::integer_t>())
            as libc::mach_msg_type_number_t;
        let status = unsafe {
            libc::thread_info(
                port,
                libc::THREAD_BASIC_INFO as libc::thread_flavor_t,
                (&raw mut info).cast(),
                &mut count,
            )
        };
        if status != libc::KERN_SUCCESS {
            return None;
        }
        let time = |value: libc::time_value_t| {
            Duration::from_secs(u64::try_from(value.seconds).unwrap_or(0))
                + Duration::from_micros(u64::try_from(value.microseconds).unwrap_or(0))
        };
        Some(time(info.user_time) + time(info.system_time))
    }
    #[cfg(not(target_os = "macos"))]
    None
}

/// Page faults the whole process has taken so far: a slow interval full of
/// them is memory the system compressed or swapped being paged back in.
#[cfg(unix)]
fn process_faults() -> u64 {
    // SAFETY: getrusage fills a caller-owned struct.
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) } != 0 {
        return 0;
    }
    u64::try_from(usage.ru_minflt)
        .unwrap_or(0)
        .saturating_add(u64::try_from(usage.ru_majflt).unwrap_or(0))
}

#[cfg(windows)]
fn process_faults() -> u64 {
    diri_platform::process::page_faults().unwrap_or(0)
}

/// Main-thread CPU and process page faults at one moment. Across a slow
/// frame or a stall they tell apart work done on the main thread (CPU),
/// memory paged back in (faults) and waiting on something else (neither:
/// a lock, a synchronous call, or a thread the system did not schedule).
#[derive(Clone, Copy)]
struct Usage {
    cpu: Option<Duration>,
    faults: u64,
}

impl Usage {
    fn now() -> Self {
        Self {
            cpu: main_thread_cpu(),
            faults: process_faults(),
        }
    }

    /// CPU used and faults taken since this sample.
    fn since(self) -> (Option<Duration>, u64) {
        let now = Self::now();
        let cpu = self
            .cpu
            .zip(now.cpu)
            .map(|(then, now)| now.saturating_sub(then));
        (cpu, now.faults.saturating_sub(self.faults))
    }
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
            LAST_FRAME_END.with(|ends| ends.borrow_mut().remove(&self.window));
            crate::perf_overlay::forget_window(self.window);
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

/// When a main window's frame began: taken first thing in the root view's
/// render and handed to [`frame_probe`], with the terminal paints counted so
/// far so the probe can attribute the frame's own share.
#[derive(Clone, Copy)]
pub(crate) struct FrameStart {
    pub(crate) at: Instant,
    paints: diri_term::element::PaintTotals,
    /// Main-thread CPU and process faults, when telemetry is recording.
    usage: Option<Usage>,
}

pub(crate) fn frame_start() -> FrameStart {
    FrameStart {
        at: Instant::now(),
        paints: diri_term::element::PaintTotals::now(),
        usage: diri_telemetry::is_enabled().then(Usage::now),
    }
}

thread_local! {
    /// When each main window last finished a frame, for `idle_ms`: the
    /// first frame after a long quiet spell is the one that finds its
    /// memory compressed.
    static LAST_FRAME_END: RefCell<HashMap<u64, Instant>> = RefCell::new(HashMap::new());
}

/// A 120 Hz frame budget: frames over it are candidates for a sampled
/// `ui.slow_frame` breakdown even below [`SLOW_FRAME`].
const OVER_BUDGET_FRAME: Duration = Duration::from_micros(8_333);
/// At most one sampled over-budget breakdown per this interval.
const OVER_BUDGET_SAMPLE_EVERY: Duration = Duration::from_secs(30);
static OVER_BUDGET_SAMPLED_MS: AtomicU64 = AtomicU64::new(0);

/// Where one frame's CPU time went, as the probe sees it from the end of the
/// root's paint: GPUI's phases so far, the terminals it painted and how many
/// views rendered or replayed. Deferred overlays, tooltips and the
/// accessibility update come after the probe; the accessibility cost is
/// taken from the previous frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct FrameBreakdown {
    pub(crate) total: Duration,
    /// The main thread's CPU time over `total`.
    pub(crate) cpu: Option<Duration>,
    /// Page faults the process took over `total`.
    pub(crate) faults: Option<u64>,
    /// How long the window was quiet before this frame began.
    pub(crate) idle: Option<Duration>,
    /// Whether this frame's window was the active one.
    pub(crate) window_active: bool,
    pub(crate) gpui: gpui::FrameStats,
    pub(crate) previous_a11y: Duration,
    pub(crate) terminals: diri_term::element::PaintTotals,
    pub(crate) windows: usize,
}

impl FrameBreakdown {
    fn observe(&self) {
        diri_telemetry::observe("ui.frame", self.total);
        if let Some(cpu) = self.cpu {
            diri_telemetry::observe("ui.frame.cpu", cpu);
        }
        diri_telemetry::observe("ui.frame.layout", self.gpui.layout);
        diri_telemetry::observe("ui.frame.prepaint", self.gpui.prepaint);
        diri_telemetry::observe("ui.frame.paint", self.gpui.paint);
        diri_telemetry::observe(
            "ui.frame.terminals",
            Duration::from_micros(self.terminals.micros),
        );
        diri_telemetry::count(
            "ui.frame.views_rendered",
            u64::from(self.gpui.views_rendered),
        );
        diri_telemetry::count("ui.frame.views_reused", u64::from(self.gpui.views_reused));
        diri_telemetry::count("ui.frame.terminal_paints", self.terminals.paints);
        diri_telemetry::count("ui.frame.shape_misses", self.terminals.shape_misses);
        if self.gpui.a11y_active {
            diri_telemetry::count("ui.frame.a11y_frames", 1);
            diri_telemetry::observe("ui.frame.a11y", self.previous_a11y);
        }
    }

    fn fields(&self, window: u64, context: FrameContext) -> Vec<(&'static str, Value)> {
        vec![
            ("ms", Value::from(self.total)),
            ("cpu_ms", Value::from(self.cpu)),
            ("faults", Value::from(self.faults)),
            ("idle_ms", Value::from(self.idle)),
            ("active", Value::from(self.window_active)),
            (
                "app_active",
                Value::from(APP_ACTIVE.load(Ordering::Relaxed)),
            ),
            ("window", Value::from(window)),
            ("surface", Value::from(context.surface)),
            ("workspace", Value::from(context.workspace)),
            ("layout_ms", Value::from(self.gpui.layout)),
            ("prepaint_ms", Value::from(self.gpui.prepaint)),
            ("paint_ms", Value::from(self.gpui.paint)),
            ("views", Value::from(u64::from(self.gpui.views_rendered))),
            ("reused", Value::from(u64::from(self.gpui.views_reused))),
            ("terminals", Value::from(self.terminals.paints)),
            (
                "terminal_ms",
                Value::from(Duration::from_micros(self.terminals.micros)),
            ),
            ("shape_misses", Value::from(self.terminals.shape_misses)),
            ("windows", Value::from(self.windows)),
            ("a11y", Value::from(self.gpui.a11y_active)),
        ]
    }
}

/// A zero-size element painted last in a main window: the time from the
/// start of the root view's render to here is the frame's CPU cost (render,
/// layout, prepaint, paint of everything before it; not GPU present).
pub(crate) fn frame_probe(started: FrameStart, context: FrameContext) -> impl IntoElement {
    canvas(
        |_, _, _| {},
        move |_, _, window, cx| {
            if crate::perf_overlay::overlay_enabled() {
                let gpui = window.frame_stats_so_far();
                crate::perf_overlay::record_frame(
                    window.window_handle().window_id().as_u64(),
                    crate::perf_overlay::FrameSample {
                        end: cx.background_executor().now(),
                        cost: started.at.elapsed(),
                        views_rendered: gpui.views_rendered,
                        views_reused: gpui.views_reused,
                        terminal_paints: diri_term::element::PaintTotals::now()
                            .since(started.paints)
                            .paints,
                    },
                );
            }
            if !diri_telemetry::is_enabled() {
                return;
            }
            if let Ok(mut surface) = LAST_SURFACE.lock() {
                *surface = context.surface;
            }
            let window_id = window.window_handle().window_id().as_u64();
            let previous_end =
                LAST_FRAME_END.with(|ends| ends.borrow_mut().insert(window_id, Instant::now()));
            let (cpu, faults) = started.usage.map_or((None, None), |usage| {
                let (cpu, faults) = usage.since();
                (cpu, Some(faults))
            });
            let breakdown = FrameBreakdown {
                total: started.at.elapsed(),
                cpu,
                faults,
                idle: previous_end.map(|end| started.at.saturating_duration_since(end)),
                window_active: window.is_window_active(),
                gpui: window.frame_stats_so_far(),
                previous_a11y: window.last_frame_stats().a11y,
                terminals: diri_term::element::PaintTotals::now().since(started.paints),
                windows: cx.windows().len(),
            };
            breakdown.observe();
            if !LAUNCH_RECORDED.swap(true, Ordering::Relaxed) {
                let platform = LaunchPlatform::current(cx.compositor_name(), window.gpu_specs());
                event!(
                    "app.launch",
                    ms = process_started().elapsed(),
                    version = crate::updates::CURRENT_VERSION,
                    windows = MAIN_WINDOWS.load(Ordering::Relaxed),
                    display = platform.display,
                    desktop = platform.desktop,
                    package = platform.package,
                    gpu = platform.gpu,
                    gpu_software = platform.gpu_software
                );
            }
            if breakdown.total >= SLOW_FRAME {
                diri_telemetry::record(
                    "ui.slow_frame",
                    diri_telemetry::Severity::Warn,
                    breakdown.fields(window_id, context),
                );
            } else if breakdown.total >= OVER_BUDGET_FRAME && over_budget_sample_due() {
                diri_telemetry::record(
                    "ui.slow_frame",
                    diri_telemetry::Severity::Debug,
                    breakdown.fields(window_id, context),
                );
            }
        },
    )
    .absolute()
    .size_0()
}

/// Linux display facts for `app.launch`, as closed classes: which display
/// server, desktop, package and GPU driver a launch or rendering report came
/// from. All `None` on macOS. Never a device name, driver version or path.
#[derive(Debug, Default, PartialEq, Eq)]
struct LaunchPlatform {
    display: Option<&'static str>,
    desktop: Option<&'static str>,
    package: Option<&'static str>,
    gpu: Option<&'static str>,
    gpu_software: Option<bool>,
}

impl LaunchPlatform {
    fn current(compositor: &'static str, gpu: Option<gpui::GpuSpecs>) -> Self {
        if !cfg!(target_os = "linux") {
            return Self::default();
        }
        let executable = std::env::current_exe().ok();
        Self {
            display: display_class(compositor),
            desktop: std::env::var("XDG_CURRENT_DESKTOP")
                .ok()
                .map(|desktop| desktop_class(&desktop)),
            package: Some(package_class(
                std::env::var_os("APPIMAGE").is_some(),
                executable.as_deref(),
            )),
            gpu: gpu
                .as_ref()
                .map(|gpu| gpu_class(&gpu.driver_name, &gpu.device_name)),
            gpu_software: gpu.map(|gpu| gpu.is_software_emulated),
        }
    }
}

fn display_class(compositor: &str) -> Option<&'static str> {
    match compositor {
        "Wayland" => Some("wayland"),
        "X11" => Some("x11"),
        "" => None,
        _ => Some("other"),
    }
}

/// `XDG_CURRENT_DESKTOP` is a colon-separated list, most specific first
/// (`ubuntu:GNOME`, `Budgie:GNOME`).
fn desktop_class(value: &str) -> &'static str {
    const KNOWN: [&str; 21] = [
        "gnome",
        "kde",
        "xfce",
        "cinnamon",
        "mate",
        "lxqt",
        "lxde",
        "budgie",
        "pantheon",
        "cosmic",
        "unity",
        "deepin",
        "sway",
        "hyprland",
        "niri",
        "river",
        "wayfire",
        "i3",
        "labwc",
        "enlightenment",
        "gamescope",
    ];
    for part in value.split(':') {
        let part = part.trim().to_ascii_lowercase();
        let part = part.strip_prefix("x-").unwrap_or(&part);
        if part == "ubuntu" || part == "pop" || part == "zorin" {
            continue;
        }
        if let Some(known) = KNOWN.iter().find(|known| part.starts_with(*known)) {
            return known;
        }
    }
    if value.trim().is_empty() {
        "none"
    } else {
        "other"
    }
}

/// `appimage`, `system` (a distro package: `<prefix>/bin` beside
/// `<prefix>/lib/diri`) or `source` (a cargo build).
fn package_class(appimage: bool, executable: Option<&std::path::Path>) -> &'static str {
    if appimage {
        return "appimage";
    }
    let packaged = executable
        .and_then(std::path::Path::parent)
        .filter(|dir| dir.file_name().is_some_and(|name| name == "bin"))
        .and_then(std::path::Path::parent)
        .is_some_and(|prefix| prefix.join("lib/diri").is_dir());
    if packaged { "system" } else { "source" }
}

/// The Vulkan driver family from wgpu's adapter info.
fn gpu_class(driver: &str, device: &str) -> &'static str {
    let text = format!("{driver} {device}").to_ascii_lowercase();
    const CLASSES: [(&str, &[&str]); 9] = [
        (
            "software",
            &["llvmpipe", "lavapipe", "softpipe", "swiftshader"],
        ),
        ("nouveau", &["nvk", "nouveau"]),
        ("nvidia", &["nvidia"]),
        ("amd", &["radv", "amd", "radeon"]),
        ("intel", &["intel", "anv"]),
        ("asahi", &["honeykrisp", "asahi", "apple"]),
        ("adreno", &["turnip", "freedreno", "adreno"]),
        ("mali", &["panvk", "panfrost", "mali"]),
        ("virtual", &["venus", "virtio", "vmware", "virgl"]),
    ];
    CLASSES
        .iter()
        .find(|(_, needles)| needles.iter().any(|needle| text.contains(needle)))
        .map_or("other", |(class, _)| class)
}

/// One sampled over-budget frame per [`OVER_BUDGET_SAMPLE_EVERY`].
fn over_budget_sample_due() -> bool {
    let now = mono_ms();
    let last = OVER_BUDGET_SAMPLED_MS.load(Ordering::Relaxed);
    let every = u64::try_from(OVER_BUDGET_SAMPLE_EVERY.as_millis()).unwrap_or(u64::MAX);
    (last == 0 || now.saturating_sub(last) >= every)
        && OVER_BUDGET_SAMPLED_MS
            .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
}

/// Watches main-thread responsiveness without polling it. A background
/// thread posts a ping once a second (once every five while diri is in the
/// background) and the main thread answers when it next runs. The answer's
/// latency is the stall; a ping still unanswered after five seconds is
/// recorded and flushed on the spot, since the user may be about to Force
/// Quit. Idle cost: one wakeup per interval on each side, no timers on the
/// main thread, and the measured stall is a lower bound (±1 interval).
fn start_stall_watchdog(cx: &mut App) {
    let ping = std::sync::Arc::new(Ping::default());
    let (ping_tx, mut ping_rx) = tokio::sync::mpsc::channel::<()>(1);

    let answered = std::sync::Arc::clone(&ping);
    cx.spawn(async move |_| {
        while ping_rx.recv().await.is_some() {
            let Some(sent) = answered.take() else {
                continue;
            };
            let latency = Duration::from_millis(mono_ms().saturating_sub(sent.at));
            if latency >= STALL {
                record_stall(latency, false, &sent);
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
                let Some(sent) = ping.peek() else {
                    reported_ongoing = false;
                    ping.send();
                    if ping_tx.try_send(()).is_err() && ping_tx.is_closed() {
                        return;
                    }
                    continue;
                };
                let stalled = Duration::from_millis(mono_ms().saturating_sub(sent.at));
                if stalled >= STALL_SAMPLE_AFTER {
                    sample_stall_stack(sent.at);
                }
                if stalled >= STALL_ONGOING && !reported_ongoing {
                    reported_ongoing = true;
                    record_stall(stalled, true, &sent);
                    diri_telemetry::flush(Duration::from_secs(1));
                }
            }
        });
}

/// The outstanding watchdog ping. `at` is 0 when none is; the other fields
/// are written before `at` is published and read after it is taken.
#[derive(Default)]
struct Ping {
    at: AtomicU64,
    active: AtomicBool,
    /// Main-thread CPU µs when sent, `u64::MAX` when unknown.
    cpu_us: AtomicU64,
    faults: AtomicU64,
}

/// What the watchdog knew when it sent a ping.
struct Sent {
    at: u64,
    active: bool,
    usage: Usage,
}

impl Ping {
    fn send(&self) {
        let usage = Usage::now();
        self.active
            .store(APP_ACTIVE.load(Ordering::Relaxed), Ordering::Relaxed);
        self.cpu_us.store(
            usage.cpu.map_or(u64::MAX, |cpu| {
                u64::try_from(cpu.as_micros()).unwrap_or(u64::MAX)
            }),
            Ordering::Relaxed,
        );
        self.faults.store(usage.faults, Ordering::Relaxed);
        self.at.store(mono_ms(), Ordering::Release);
    }

    fn peek(&self) -> Option<Sent> {
        let at = self.at.load(Ordering::Acquire);
        self.sent(at)
    }

    fn take(&self) -> Option<Sent> {
        let at = self.at.swap(0, Ordering::AcqRel);
        self.sent(at)
    }

    fn sent(&self, at: u64) -> Option<Sent> {
        if at == 0 {
            return None;
        }
        let cpu_us = self.cpu_us.load(Ordering::Relaxed);
        Some(Sent {
            at,
            active: self.active.load(Ordering::Relaxed),
            usage: Usage {
                cpu: (cpu_us != u64::MAX).then(|| Duration::from_micros(cpu_us)),
                faults: self.faults.load(Ordering::Relaxed),
            },
        })
    }
}

/// Samples the main thread's stack once per stall, from the watchdog
/// thread while the main thread is still stuck.
fn sample_stall_stack(ping_at: u64) {
    let Some(thread) = MAIN_STACK.get().copied() else {
        return;
    };
    if STALL_STACK
        .lock()
        .is_ok_and(|stack| stack.as_ref().is_some_and(|(at, _)| *at == ping_at))
    {
        return;
    }
    let frames = thread.sample();
    if let Ok(mut stack) = STALL_STACK.lock() {
        *stack = Some((ping_at, frames));
    }
}

/// The stack sampled during the stall that began with `ping_at`; an ended
/// stall takes it, an ongoing one leaves it for the final record.
fn stall_stack(ping_at: u64, ongoing: bool) -> Option<Value> {
    let mut stack = STALL_STACK.lock().ok()?;
    if stack.as_ref().is_none_or(|(at, _)| *at != ping_at) {
        return None;
    }
    let frames = if ongoing {
        stack.as_ref().map(|(_, frames)| frames.clone())?
    } else {
        stack.take().map(|(_, frames)| frames)?
    };
    Some(Value::List(
        frames
            .iter()
            .map(|frame| Value::from(diri_telemetry::text(frame)))
            .collect(),
    ))
}

fn last_surface() -> &'static str {
    LAST_SURFACE.lock().map_or("none", |surface| *surface)
}

/// `was_active` is whether diri was frontmost when the stall began, `active`
/// whether it is now; `cpu_ms` is the main thread's own CPU time over the
/// stall (≈ `ms`: busy; ≈ 0: blocked or not scheduled), `faults` the
/// process's page faults, `action` a named action that finished inside it,
/// `surface` what the last frame before it showed, and `stack` the main
/// thread's call stack sampled 2 s in (macOS; innermost first).
fn record_stall(duration: Duration, ongoing: bool, sent: &Sent) {
    let active = APP_ACTIVE.load(Ordering::Relaxed);
    let (cpu, faults) = sent.usage.since();
    let action = action_since(sent.at);
    let surface = last_surface();
    let stack = stall_stack(sent.at, ongoing);
    if duration >= STALL_INCIDENT {
        incident!(
            "ui.stall",
            ms = duration,
            ongoing = ongoing,
            active = active,
            was_active = sent.active,
            cpu_ms = cpu,
            faults = faults,
            action = action,
            surface = surface,
            stack = stack
        );
    } else {
        diri_telemetry::warn_event!(
            "ui.stall",
            ms = duration,
            ongoing = ongoing,
            active = active,
            was_active = sent.active,
            cpu_ms = cpu,
            faults = faults,
            action = action,
            surface = surface,
            stack = stack
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
///
/// The keystroke is followed hop by hop, each stamped by the thread that
/// performs it:
///
/// - `input.echo.transport`: input queued → the first grid frame after it
///   reached the pane's transport task. Everything outside this process: the
///   socket, the Engine, the Holder, the PTY and the agent's own reaction
///   (the Engine records its own share as `input.echo.engine` and
///   `input.echo.publish`).
/// - `input.echo.apply`: that frame → applied to the pane's grid on the main
///   thread (queueing behind other main-thread work, such as a frame).
/// - `input.echo`: input queued → applied, as before.
/// - `input.echo.paint`: applied → the terminal painted it.
/// - `input.echo.<agent>`: input queued → painted, by agent class.
#[derive(Default)]
pub(crate) struct EchoProbe {
    /// When the pending input was queued, in µs since process start + 1; 0
    /// when nothing is pending.
    sent: AtomicU64,
    /// When the first grid frame after `sent` reached the transport task.
    received: AtomicU64,
    /// When the echo was applied, awaiting its paint; and when its input was
    /// queued. Main thread only.
    applied: AtomicU64,
    applied_sent: AtomicU64,
}

/// Longer than this is an agent thinking, not a terminal being slow.
const ECHO_MAX: Duration = Duration::from_secs(2);

/// Microseconds since the process started, plus one, so zero can mean
/// "nothing pending" in an atomic.
fn mono_us() -> u64 {
    u64::try_from(process_started().elapsed().as_micros())
        .unwrap_or(u64::MAX - 1)
        .saturating_add(1)
}

fn span_us(from: u64, to: u64) -> Duration {
    Duration::from_micros(to.saturating_sub(from))
}

impl EchoProbe {
    pub(crate) fn sent(&self) {
        if diri_telemetry::is_enabled()
            && self
                .sent
                .compare_exchange(0, mono_us(), Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
        {
            self.received.store(0, Ordering::Release);
        }
    }

    /// A grid frame reached the transport task. One atomic load unless a
    /// keystroke is waiting for its first frame.
    pub(crate) fn frame_received(&self) {
        if self.sent.load(Ordering::Acquire) != 0 {
            let _ =
                self.received
                    .compare_exchange(0, mono_us(), Ordering::AcqRel, Ordering::Relaxed);
        }
    }

    pub(crate) fn screen_changed(&self) {
        let sent = self.sent.swap(0, Ordering::AcqRel);
        if sent == 0 {
            return;
        }
        let received = self.received.swap(0, Ordering::AcqRel);
        let now = mono_us();
        let latency = span_us(sent, now);
        if latency > ECHO_MAX {
            return;
        }
        diri_telemetry::observe("input.echo", latency);
        if received >= sent && received <= now {
            diri_telemetry::observe("input.echo.transport", span_us(sent, received));
            diri_telemetry::observe("input.echo.apply", span_us(received, now));
        }
        self.applied_sent.store(sent, Ordering::Relaxed);
        self.applied.store(now, Ordering::Release);
    }

    /// The pane painted the session's grid; closes an applied echo.
    pub(crate) fn painted(&self, agent: &str) {
        let applied = self.applied.swap(0, Ordering::AcqRel);
        if applied == 0 {
            return;
        }
        let sent = self.applied_sent.load(Ordering::Relaxed);
        let now = mono_us();
        diri_telemetry::observe("input.echo.paint", span_us(applied, now));
        diri_telemetry::observe(echo_metric(agent), span_us(sent, now));
    }
}

/// `input.echo.<class>`: a closed set of names, so agent ids never become
/// metric names.
fn echo_metric(agent: &str) -> &'static str {
    match diri_telemetry::agent_class(agent) {
        diri_telemetry::AgentClass::Claude => "input.echo.claude",
        diri_telemetry::AgentClass::Codex => "input.echo.codex",
        diri_telemetry::AgentClass::Cursor => "input.echo.cursor",
        diri_telemetry::AgentClass::Gemini => "input.echo.gemini",
        diri_telemetry::AgentClass::Shell => "input.echo.shell",
        diri_telemetry::AgentClass::Other => "input.echo.other",
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
    /// Attaches in a row that ended before their first grid.
    empty: u32,
}

/// Failed attaches in a row before the pane is recorded as failing.
const ATTACH_FAILING_AFTER: u32 = 3;
/// Empty attaches in a row after which one `pane.attach_flapping` stands in
/// for the per-attempt attach/detach events until a grid arrives.
const ATTACH_FLAPPING_AFTER: u32 = 3;

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
            empty: 0,
        }
    }

    /// Per-attempt events are muted while the pane is flapping.
    fn flapping(&self) -> bool {
        self.empty >= ATTACH_FLAPPING_AFTER
    }

    pub(crate) fn connecting(&mut self) {
        self.attempt_at = Instant::now();
    }

    pub(crate) fn connect_failed(&mut self, reason: &'static str) {
        self.failures += 1;
        diri_telemetry::count("pane.attach_retries", 1);
        if self.failures == ATTACH_FAILING_AFTER {
            diri_telemetry::error_event!(
                "pane.attach_failing",
                session = self.session.clone(),
                attempts = self.failures,
                reason = reason,
                since_mount_ms = self.mounted_at.elapsed()
            );
        }
    }

    pub(crate) fn live(&mut self) {
        let connect = self.attempt_at.elapsed();
        diri_telemetry::observe("pane.attach", connect);
        if !self.flapping() {
            event!(
                "pane.attached",
                session = self.session.clone(),
                reconnect = self.attaches > 0,
                attempts = self.failures + 1,
                connect_ms = connect,
                since_mount_ms = self.mounted_at.elapsed()
            );
        }
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
        self.empty = 0;
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
        if !self.flapping() {
            diri_telemetry::warn_event!("pane.drain_interrupted", session = self.session.clone());
        }
    }

    /// `input_at_risk`: typed input may have been lost, so the pane says so;
    /// otherwise the reconnect is silent and only counted.
    pub(crate) fn detached(&mut self, input_at_risk: bool) {
        let live_ms = self.live_at.take().map(|at| at.elapsed());
        if !input_at_risk {
            diri_telemetry::count("pane.reconnect_silent", 1);
        }
        if !self.flapping() {
            diri_telemetry::warn_event!(
                "pane.detached",
                session = self.session.clone(),
                live_ms = live_ms,
                grids = self.grids,
                reseeds = self.snapshots.saturating_sub(1),
                input_at_risk = input_at_risk
            );
        }
        if self.grids == 0 {
            self.empty += 1;
            if self.empty == ATTACH_FLAPPING_AFTER {
                diri_telemetry::warn_event!(
                    "pane.attach_flapping",
                    session = self.session.clone(),
                    attaches = self.empty,
                    since_mount_ms = self.mounted_at.elapsed()
                );
            }
        }
    }

    /// The Engine refused the attach; the pane waits for the session to
    /// change, so this is recorded once per refusal, not per retry.
    pub(crate) fn rejected(&mut self, reason: diri_proto::frames::AttachRejection) {
        self.live_at = None;
        diri_telemetry::warn_event!(
            "pane.attach_rejected",
            session = self.session.clone(),
            reason = reason.as_str(),
            since_mount_ms = self.mounted_at.elapsed()
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

    pub(crate) fn save_config(&mut self, config: Config) -> std::io::Result<()> {
        let state = self
            .folder
            .as_deref()
            .and_then(std::path::Path::parent)
            .ok_or_else(|| std::io::Error::other("diagnostics settings folder unavailable"))?;
        config.save(state)?;
        self.config = config;
        Ok(())
    }
}

/// True once, on a new install: no diagnostics settings exist yet. Saves
/// sharing off, so ignoring the question keeps it off and it is not asked
/// again. Installs from before opt-in already have settings and keep them.
pub(crate) fn take_consent_prompt() -> bool {
    if !diri_telemetry::is_enabled() {
        return false;
    }
    let Some(dir) = state_dir() else {
        return false;
    };
    take_consent_prompt_in(&dir)
}

fn take_consent_prompt_in(dir: &std::path::Path) -> bool {
    if Config::exists(dir) || Config::default().save(dir).is_err() {
        return false;
    }
    event!("privacy.prompt_shown");
    true
}

/// The person answered the prompt or flipped the Settings toggle.
pub(crate) fn choose_sharing(share: bool) -> std::io::Result<Config> {
    let dir = state_dir().ok_or_else(|| std::io::Error::other("no state directory"))?;
    let config = Config::load(&dir).choose(share, diri_telemetry::now_ms());
    config.save(&dir)?;
    event!("privacy.upload_changed", upload = share);
    Ok(config)
}

/// Asks the Engine to upload everything recorded so far, now, even with
/// sharing off (the user asked). Blocks up to a minute: call it off the main
/// thread.
pub(crate) fn upload_now_blocking() -> Result<diri_proto::TelemetryUploadNowResult, String> {
    // Tests never reach the real Engine.
    let home = diri_platform::home_dir()
        .map(|p| p.into_os_string())
        .filter(|_| !cfg!(test))
        .ok_or_else(|| "no home directory".to_owned())?;
    // What this process recorded a moment ago goes in the same upload.
    diri_telemetry::flush(Duration::from_millis(200));
    let socket = diri_proto::paths::DirijorPaths::socket(home);
    let value = crate::daemon_launch::control_request_with_timeout(
        &socket,
        1,
        diri_proto::Method::TELEMETRY_UPLOAD_NOW,
        None,
        Duration::from_secs(60),
    )
    .map_err(|error| error.to_string())?;
    serde_json::from_value(value).map_err(|error| error.to_string())
}

/// One line for the user about an [`upload_now_blocking`] outcome.
pub(crate) fn upload_now_summary(
    result: &Result<diri_proto::TelemetryUploadNowResult, String>,
) -> &'static str {
    match result.as_ref().map(|result| result.status.as_str()) {
        Ok("sent") => crate::i18n::t("settings.privacy.sent"),
        Ok("up_to_date") => crate::i18n::t("settings.privacy.up_to_date"),
        Ok("failed") => crate::i18n::t("settings.privacy.send_failed"),
        Ok("timeout") => crate::i18n::t("settings.privacy.send_timeout"),
        Ok("unavailable") => crate::i18n::t("settings.privacy.send_unavailable"),
        Ok(_) => crate::i18n::t("settings.privacy.sent_short"),
        Err(_) => crate::i18n::t("settings.privacy.engine_down"),
    }
}

pub(crate) const REPORT_ISSUE_URL: &str = "https://github.com/cristicretu/diri/issues/new";

/// Help > Report a Problem: marks the moment in the timeline, uploads it
/// right away (even with sharing off), copies the Support ID, and opens a
/// new GitHub issue that already names this install and build.
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
    // Reporting is explicit consent to send what led up to it, now.
    if state_dir().is_some() {
        let _ = std::thread::Builder::new()
            .name("diri-report-upload".into())
            .spawn(|| {
                let _ = upload_now_blocking();
            });
    }
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
    fn linux_launch_facts_are_closed_classes() {
        assert_eq!(display_class("Wayland"), Some("wayland"));
        assert_eq!(display_class("X11"), Some("x11"));
        assert_eq!(display_class(""), None);
        assert_eq!(desktop_class("ubuntu:GNOME"), "gnome");
        assert_eq!(desktop_class("KDE"), "kde");
        assert_eq!(desktop_class("X-Cinnamon"), "cinnamon");
        assert_eq!(desktop_class("Hyprland"), "hyprland");
        assert_eq!(desktop_class("Budgie:GNOME"), "budgie");
        assert_eq!(desktop_class("my-own-wm"), "other");
        assert_eq!(desktop_class(""), "none");
        assert_eq!(gpu_class("NVIDIA", "NVIDIA GeForce RTX 4070"), "nvidia");
        assert_eq!(gpu_class("radv", "AMD Radeon 780M (RADV PHOENIX)"), "amd");
        assert_eq!(
            gpu_class("Intel open-source Mesa driver", "Intel(R) Graphics (RPL-P)"),
            "intel"
        );
        assert_eq!(
            gpu_class("llvmpipe", "llvmpipe (LLVM 19.1.7, 256 bits)"),
            "software"
        );
        assert_eq!(gpu_class("NVK", "NVIDIA GeForce GTX 1060"), "nouveau");
        assert_eq!(gpu_class("Honeykrisp", "Apple M2 Pro (G14S B1)"), "asahi");
        assert_eq!(gpu_class("", ""), "other");
    }

    #[test]
    fn linux_package_kind_follows_the_install_layout() {
        let temp = tempfile::tempdir().unwrap();
        let bin = temp.path().join("usr/bin");
        std::fs::create_dir_all(&bin).unwrap();
        let exe = bin.join("diri");
        assert_eq!(package_class(true, Some(&exe)), "appimage");
        assert_eq!(package_class(false, Some(&exe)), "source");
        std::fs::create_dir_all(temp.path().join("usr/lib/diri")).unwrap();
        assert_eq!(package_class(false, Some(&exe)), "system");
        assert_eq!(package_class(false, None), "source");
    }

    #[test]
    fn macos_launches_carry_no_linux_facts() {
        if cfg!(target_os = "macos") {
            assert_eq!(LaunchPlatform::current("", None), LaunchPlatform::default());
        }
    }

    #[test]
    fn privacy_changes_are_committed_only_after_a_successful_save() {
        let dir = tempfile::tempdir().unwrap();
        let mut settings = PrivacySettings {
            folder: Some(dir.path().join("telemetry")),
            ..PrivacySettings::default()
        };
        Config::default().choose(true, 1).save(dir.path()).unwrap();
        settings.config = Config::load(dir.path());
        let disabled = settings.config.choose(false, 2);
        let config_path = dir.path().join("telemetry/config.json");
        std::fs::remove_file(&config_path).unwrap();
        std::fs::create_dir(&config_path).unwrap();
        assert!(settings.save_config(disabled.clone()).is_err());
        assert!(
            settings.config.sharing_allowed(),
            "UI must keep its last confirmed setting"
        );
        std::fs::remove_dir(&config_path).unwrap();
        settings.save_config(disabled.clone()).unwrap();
        assert_eq!(settings.config, disabled);
        assert_eq!(Config::load(dir.path()), disabled);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn stall_usage_tells_a_busy_main_thread_from_a_blocked_one() {
        // This test thread stands in for the main thread; the watchdog reads
        // it from another thread, as here.
        remember_main_thread();
        let read_elsewhere = || std::thread::spawn(Usage::now).join().unwrap();

        let before = read_elsewhere();
        let spin = Instant::now();
        let mut work = 0u64;
        while spin.elapsed() < Duration::from_millis(120) {
            work = std::hint::black_box(work.wrapping_add(1));
        }
        let (busy, _) = before.since();
        let busy = busy.expect("main-thread CPU is readable on macOS");
        assert!(busy >= Duration::from_millis(60), "busy: {busy:?}");

        let before = read_elsewhere();
        std::thread::sleep(Duration::from_millis(120));
        let (blocked, _) = before.since();
        assert!(
            blocked.unwrap() < Duration::from_millis(40),
            "blocked: {blocked:?}"
        );
    }

    #[test]
    fn only_an_action_that_finished_inside_the_stall_is_named() {
        let stall_began = 1_000;
        assert_eq!(finished_since(None, stall_began), None);
        assert_eq!(
            finished_since(Some(("diri::Earlier", 999)), stall_began),
            None
        );
        assert_eq!(
            finished_since(Some(("diri::Paste", 1_000)), stall_began),
            Some("diri::Paste")
        );
    }

    #[test]
    fn a_stall_carries_only_the_stack_sampled_during_it() {
        let frames = |value: Option<Value>| match value {
            Some(Value::List(frames)) => frames.len(),
            _ => 0,
        };
        *STALL_STACK.lock().unwrap() = Some((7, vec!["diri a".to_owned(), "diri b".to_owned()]));
        // Another stall's record never borrows this one's sample.
        assert_eq!(frames(stall_stack(6, false)), 0);
        // The 5 s ongoing record shows it and leaves it for the final one.
        assert_eq!(frames(stall_stack(7, true)), 2);
        assert_eq!(frames(stall_stack(7, false)), 2);
        assert_eq!(frames(stall_stack(7, false)), 0);
    }

    #[test]
    fn page_faults_only_grow() {
        let before = process_faults();
        let touched = vec![1u8; 8 << 20];
        std::hint::black_box(&touched);
        assert!(process_faults() > before);
    }

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
    fn upload_now_summaries_cover_every_status() {
        let status = |status: &str| {
            Ok(diri_proto::TelemetryUploadNowResult {
                status: status.into(),
                ..Default::default()
            })
        };
        assert!(upload_now_summary(&status("sent")).starts_with("Sent"));
        assert!(upload_now_summary(&status("failed")).contains("try again"));
        assert!(upload_now_summary(&status("unavailable")).contains("isn't set up"));
        assert!(upload_now_summary(&Err("refused".into())).contains("engine"));
        assert!(
            upload_now_blocking().is_err(),
            "tests never reach the real Engine"
        );
    }

    #[test]
    fn agent_ready_names_where_the_first_agent_came_from() {
        use diri_proto::AgentKind;
        let none = crate::agent_setup::bundled_catalog(&[]);
        assert_eq!(agent_ready(None, &none, true), None);
        assert_eq!(
            agent_ready(Some(&AgentKind::CLAUDE_CODE), &none, false),
            None
        );

        let claude = crate::agent_setup::bundled_catalog(&["claude-code"]);
        let ready = |installing: Option<&AgentKind>, first| {
            agent_ready(installing, &claude, first).unwrap()
        };
        let (claude_code, codex) = (AgentKind::CLAUDE_CODE, AgentKind::CODEX);
        let installed = ready(Some(&claude_code), false);
        assert_eq!(installed.source, "onboarding_install");
        assert_eq!(installed.agent, AgentKind::CLAUDE_CODE);
        assert_eq!(installed.agents, 1);
        assert_eq!(ready(None, true).source, "preexisting");
        assert_eq!(ready(None, false).source, "manual");
        // Waiting for Codex while Claude Code turns up is not the install.
        assert_eq!(ready(Some(&codex), false).source, "manual");

        let two = crate::agent_setup::bundled_catalog(&["codex", "claude-code"]);
        let both = agent_ready(Some(&AgentKind::CODEX), &two, false).unwrap();
        assert_eq!((both.agent, both.agents), (AgentKind::CODEX, 2));
    }

    #[test]
    fn consent_prompt_asks_new_installs_once_and_leaves_existing_ones_alone() {
        let fresh = tempfile::tempdir().unwrap();
        assert!(take_consent_prompt_in(fresh.path()));
        assert!(!take_consent_prompt_in(fresh.path()), "only once");
        assert!(!Config::load(fresh.path()).sharing_allowed(), "off until yes");

        // Installed before opt-in, sharing on by default: unchanged, no prompt.
        let existing = tempfile::tempdir().unwrap();
        let path = diri_telemetry::telemetry_dir(existing.path()).join("config.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let before = r#"{"upload": true, "name": null}"#;
        std::fs::write(&path, before).unwrap();
        assert!(!take_consent_prompt_in(existing.path()));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        assert!(Config::load(existing.path()).sharing_allowed());
    }

    #[test]
    fn tests_never_touch_the_real_telemetry_dir() {
        assert_eq!(state_dir(), None);
        assert!(!take_consent_prompt());
        assert!(choose_sharing(true).is_err());
        assert_eq!(PrivacySettings::load(), PrivacySettings::default());
    }
}
