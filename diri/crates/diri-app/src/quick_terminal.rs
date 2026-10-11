//! The quick terminal: a shell that drops from the top of the active screen
//! over whatever app is in front, on a global shortcut, and slides away on
//! the next press.
//!
//! The shell is an ordinary Engine Session (it is listed in the sidebar and
//! survives relaunches); `Prefs::quick_terminal_session` only remembers which
//! one. The panel is a non-activating popup window that exists only while it
//! is on screen: hidden, the quick terminal costs no window, no attachment
//! and no timer, and before the first press it costs nothing at all.
//!
//! macOS only for now: the panel leans on AppKit's non-activating panels.

use std::sync::Arc;
use std::time::Duration;

use diri_proto::SessionId;
use gpui::{
    App, AppContext, Bounds, Context, DisplayId, Entity, FocusHandle, Global, InteractiveElement,
    IntoElement, KeyDownEvent, ParentElement, Render, Styled, Subscription, Task, Window,
    WindowBounds, WindowHandle, WindowKind, WindowOptions, div, point, px, size,
};

use crate::AppServices;
use crate::i18n::t;
use crate::terminal_pane::{TerminalPane, TerminalViewport};

/// Share of the screen's visible height the panel takes.
const HEIGHT_FRACTION: f64 = 0.4;
/// Shortest panel worth showing, in points, on screens that allow it.
const MIN_HEIGHT: f64 = 240.0;
/// Radius of the panel's two bottom corners.
const RADIUS: f32 = 12.0;
/// Breathing room around the grid, inside the panel edges.
const INSET_X: f32 = 12.0;
const INSET_TOP: f32 = 8.0;
const INSET_BOTTOM: f32 = 10.0;
const SHOW_SECONDS: f64 = 0.2;
const HIDE_SECONDS: f64 = 0.16;

/// A frame in AppKit screen coordinates: points, origin at the bottom left.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct PanelRect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// Where the panel rests on screen and where it waits off it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct PanelFrames {
    pub shown: PanelRect,
    /// The same frame lifted by its own height: its bottom edge on the top
    /// of the visible area, tucked under the menu bar.
    pub hidden: PanelRect,
}

/// Full width of the visible area, hanging from its top edge.
pub(crate) fn panel_frames(visible: PanelRect) -> PanelFrames {
    let height = (visible.height * HEIGHT_FRACTION)
        .round()
        .max(MIN_HEIGHT)
        .min(visible.height);
    let top = visible.y + visible.height;
    let shown = PanelRect {
        x: visible.x,
        y: top - height,
        width: visible.width,
        height,
    };
    PanelFrames {
        shown,
        hidden: PanelRect { y: top, ..shown },
    }
}

/// One slide for the platform to run.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Slide {
    pub to: PanelRect,
    /// A panel not yet on screen is placed here first, then ordered in.
    pub order_in_from: Option<PanelRect>,
    /// Make the panel key: sliding in, not out.
    pub take_key: bool,
    /// Seconds; zero places the panel at once.
    pub duration: f64,
}

/// Slide length under the current motion preference.
pub(crate) fn slide_seconds(showing: bool, reduce_motion: bool) -> f64 {
    match (reduce_motion, showing) {
        (true, _) => 0.0,
        (false, true) => SHOW_SECONDS,
        (false, false) => HIDE_SECONDS,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Phase {
    Hidden,
    Showing,
    Shown,
    Hiding,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Event {
    /// The global shortcut, or the command palette.
    Toggle,
    /// Escape pressed in the panel.
    Escape,
    /// Another window or app took key focus from the panel.
    FocusLost,
    /// The panel's shell exited.
    ShellEnded,
    /// A slide of this generation finished.
    SlideFinished(u64),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Effect {
    /// Open the panel if it is closed and slide it in.
    Show { generation: u64 },
    /// Slide the panel out.
    Hide { generation: u64 },
    /// The panel is out of sight: close its window.
    Close,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct Options {
    pub hide_on_escape: bool,
    pub autohide: bool,
}

/// What the quick terminal does next, apart from any window. Every slide
/// carries a generation, so the finish of one a later press reversed is
/// recognised as stale instead of closing a panel that is sliding back in.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Machine {
    phase: Phase,
    generation: u64,
}

impl Default for Machine {
    fn default() -> Self {
        Self {
            phase: Phase::Hidden,
            generation: 0,
        }
    }
}

impl Machine {
    #[cfg(test)]
    fn phase(&self) -> Phase {
        self.phase
    }

    pub(crate) fn handle(&mut self, event: Event, options: Options) -> Option<Effect> {
        let visible = matches!(self.phase, Phase::Showing | Phase::Shown);
        match event {
            Event::Toggle if visible => Some(self.hide()),
            Event::Toggle => Some(self.show()),
            Event::Escape if visible && options.hide_on_escape => Some(self.hide()),
            // Only a settled panel: making it key while it slides in can
            // briefly hand focus around, and that is not the user leaving.
            Event::FocusLost if self.phase == Phase::Shown && options.autohide => Some(self.hide()),
            Event::ShellEnded if visible => Some(self.hide()),
            Event::SlideFinished(generation) if generation == self.generation => match self.phase {
                Phase::Showing => {
                    self.phase = Phase::Shown;
                    None
                }
                Phase::Hiding => {
                    self.phase = Phase::Hidden;
                    Some(Effect::Close)
                }
                Phase::Hidden | Phase::Shown => None,
            },
            Event::Escape | Event::FocusLost | Event::ShellEnded | Event::SlideFinished(_) => None,
        }
    }

    fn show(&mut self) -> Effect {
        self.generation += 1;
        self.phase = Phase::Showing;
        Effect::Show {
            generation: self.generation,
        }
    }

    fn hide(&mut self) -> Effect {
        self.generation += 1;
        self.phase = Phase::Hiding;
        Effect::Hide {
            generation: self.generation,
        }
    }
}

/// App-level owner of the panel. Installed at launch as an empty shell (no
/// window, no task); everything else is created on the first press.
struct QuickTerminal {
    services: Arc<AppServices>,
    machine: Machine,
    window: Option<WindowHandle<QuickTerminalView>>,
    frames: Option<PanelFrames>,
    slide_finish: Option<Task<()>>,
}

impl Global for QuickTerminal {}

pub(crate) fn init(cx: &mut App, services: Arc<AppServices>) {
    cx.set_global(QuickTerminal {
        services,
        machine: Machine::default(),
        window: None,
        frames: None,
        slide_finish: None,
    });
    cx.on_action(|_: &crate::commands::ToggleQuickTerminal, cx| handle(Event::Toggle, cx));
}

fn options(services: &AppServices) -> Options {
    let store = services
        .store
        .store
        .read()
        .expect("session store lock poisoned");
    let prefs = store.preferences();
    Options {
        hide_on_escape: prefs.quick_terminal_hide_on_escape,
        autohide: prefs.quick_terminal_autohide,
    }
}

/// Feeds the machine and runs what it decides. Call at App level, never
/// from inside the panel's own update: opening, sliding and closing the
/// window all update it.
pub(crate) fn handle(event: Event, cx: &mut App) {
    if !cx.has_global::<QuickTerminal>() {
        return;
    }
    let effect = {
        let quick = cx.global_mut::<QuickTerminal>();
        let options = options(&quick.services);
        quick.machine.handle(event, options)
    };
    match effect {
        Some(Effect::Show { generation }) => show(generation, cx),
        Some(Effect::Hide { generation }) => hide(generation, cx),
        Some(Effect::Close) => close(cx),
        None => {}
    }
}

fn show(generation: u64, cx: &mut App) {
    let duration = slide_seconds(true, cx.reduce_motion());
    let quick = cx.global::<QuickTerminal>();
    let services = Arc::clone(&quick.services);
    // A press during the slide out reverses it: same window, same screen.
    if let (Some(handle), Some(frames)) = (quick.window, quick.frames) {
        let slid = handle.update(cx, |view, window, cx| {
            crate::macos::quick_terminal_panel::slide(
                window,
                Slide {
                    to: frames.shown,
                    order_in_from: None,
                    take_key: true,
                    duration,
                },
                cx.foreground_executor(),
            );
            view.focus_terminal(window, cx);
        });
        if slid.is_ok() {
            finish_after(generation, duration, cx);
            return;
        }
    }
    let Some((visible, display)) = crate::macos::quick_terminal_panel::active_screen() else {
        return;
    };
    let frames = panel_frames(visible);
    let material = services
        .store
        .store
        .read()
        .expect("session store lock poisoned")
        .preferences()
        .window_material;
    let opened = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds {
                origin: point(px(0.0), px(0.0)),
                size: size(
                    px(frames.shown.width as f32),
                    px(frames.shown.height as f32),
                ),
            })),
            titlebar: None,
            focus: false,
            show: false,
            kind: WindowKind::PopUp,
            is_movable: false,
            is_resizable: false,
            is_minimizable: false,
            window_background: crate::root::window_background(material),
            display_id: Some(DisplayId::new(u64::from(display))),
            ..Default::default()
        },
        move |window, cx| {
            crate::macos::quick_terminal_panel::prepare(window, RADIUS);
            cx.new(|cx| QuickTerminalView::new(services, window, cx))
        },
    );
    let Ok(handle) = opened else {
        // Nothing opened, so nothing will finish: settle back to hidden.
        let quick = cx.global_mut::<QuickTerminal>();
        quick.machine = Machine::default();
        return;
    };
    let _ = handle.update(cx, |_, window, cx| {
        crate::macos::quick_terminal_panel::slide(
            window,
            Slide {
                to: frames.shown,
                order_in_from: Some(frames.hidden),
                take_key: true,
                duration,
            },
            cx.foreground_executor(),
        );
    });
    let quick = cx.global_mut::<QuickTerminal>();
    quick.window = Some(handle);
    quick.frames = Some(frames);
    finish_after(generation, duration, cx);
}

fn hide(generation: u64, cx: &mut App) {
    let duration = slide_seconds(false, cx.reduce_motion());
    let quick = cx.global::<QuickTerminal>();
    let (Some(handle), Some(frames)) = (quick.window, quick.frames) else {
        // A hide with nothing on screen still has to settle the machine.
        finish_after(generation, 0.0, cx);
        return;
    };
    let _ = handle.update(cx, |_, window, cx| {
        crate::macos::quick_terminal_panel::slide(
            window,
            Slide {
                to: frames.hidden,
                order_in_from: None,
                take_key: false,
                duration,
            },
            cx.foreground_executor(),
        );
    });
    finish_after(generation, duration, cx);
}

fn close(cx: &mut App) {
    let quick = cx.global_mut::<QuickTerminal>();
    quick.frames = None;
    quick.slide_finish = None;
    if let Some(handle) = quick.window.take() {
        let _ = handle.update(cx, |_, window, _| window.remove_window());
    }
}

/// One timer per slide, replaced by the next; it never outlives the slide.
fn finish_after(generation: u64, seconds: f64, cx: &mut App) {
    let task = cx.spawn(async move |cx| {
        if seconds > 0.0 {
            cx.background_executor()
                .timer(Duration::from_secs_f64(seconds))
                .await;
        }
        cx.update(|cx| handle(Event::SlideFinished(generation), cx));
    });
    cx.global_mut::<QuickTerminal>().slide_finish = Some(task);
}

/// The panel's content: the quick terminal's shell, edge to edge, on the
/// window material.
pub(crate) struct QuickTerminalView {
    services: Arc<AppServices>,
    terminal: Option<Entity<TerminalPane>>,
    session: Option<SessionId>,
    /// This panel already asked for a shell. A failed spawn is reported like
    /// any other and not retried until the next press opens a new panel.
    spawn_requested: bool,
    focus: FocusHandle,
    /// Frames painted, and frames painted once AppKit had the panel on
    /// screen. The content stays transparent until the first of those, as
    /// floating panels do; earlier frames never reach the display.
    frames: u32,
    settled_frames: u32,
    revealed: bool,
    _changes: Task<()>,
    _activation: Subscription,
    _telemetry: crate::telemetry::WindowGuard,
}

/// Give up waiting for AppKit's occlusion report after this many frames.
const REVEAL_DEADLINE_FRAMES: u32 = 120;

impl QuickTerminalView {
    fn new(services: Arc<AppServices>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        let mut changes = services.store.changes();
        let watch = cx.spawn_in(window, async move |this, cx| {
            loop {
                match changes.recv().await {
                    Ok(()) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        if crate::floating::update_in_owner(&this, cx, |this, window, cx| {
                            this.reconcile(window, cx)
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
        let activation = cx.observe_window_activation(window, |this, window, cx| {
            let active = window.is_window_active();
            // The panel is Diri in front, as far as attention and
            // notifications are concerned, while Diri itself may be inactive.
            this.services
                .store
                .store
                .write()
                .expect("session store lock poisoned")
                .set_active(active);
            if !active {
                cx.defer(|cx| handle(Event::FocusLost, cx));
            }
        });
        let mut view = Self {
            services,
            terminal: None,
            session: None,
            spawn_requested: false,
            focus,
            frames: 0,
            settled_frames: 0,
            revealed: false,
            _changes: watch,
            _activation: activation,
            _telemetry: crate::telemetry::WindowGuard::new("quick_terminal", window),
        };
        view.reconcile(window, cx);
        view
    }

    /// Shows the remembered shell, starts one when there is none, and slides
    /// the panel away when the one it shows exits.
    fn reconcile(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (live, pending) = {
            let store = self
                .services
                .store
                .store
                .read()
                .expect("session store lock poisoned");
            (
                store
                    .quick_terminal_session()
                    .map(|session| session.id.clone()),
                store.quick_terminal_spawn_pending(),
            )
        };
        match live {
            Some(id) if self.session.as_ref() == Some(&id) => {}
            Some(id) => {
                if let Some(terminal) = &self.terminal {
                    let shown = id.clone();
                    terminal.update(cx, |terminal, cx| terminal.show_session(shown, window, cx));
                } else {
                    let runtime = Arc::clone(&self.services.store);
                    let tokio = Arc::clone(&self.services.tokio);
                    let shown = id.clone();
                    let terminal =
                        cx.new(|cx| TerminalPane::new_fixed(runtime, tokio, shown, window, cx));
                    cx.observe(&terminal, |_, _, cx| cx.notify()).detach();
                    self.terminal = Some(terminal);
                }
                self.session = Some(id);
                self.focus_terminal(window, cx);
            }
            None if self.session.is_some() => {
                // Its shell exited. The screen stays as it was while the
                // panel slides out; the next press starts a new shell.
                self.session = None;
                cx.defer(|cx| handle(Event::ShellEnded, cx));
            }
            None if !pending && !self.spawn_requested => {
                self.spawn_requested = true;
                self.services
                    .store
                    .store
                    .write()
                    .expect("session store lock poisoned")
                    .spawn_quick_terminal(t("quick_terminal.session_title").to_owned());
            }
            None => {}
        }
        cx.notify();
    }

    fn focus_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match &self.terminal {
            Some(terminal) => terminal.update(cx, |terminal, cx| terminal.focus(window, cx)),
            None => window.focus(&self.focus, cx),
        }
    }

    fn hide_on_escape(&self) -> bool {
        options(&self.services).hide_on_escape
    }
}

/// Plain Escape, the one key the panel can be told to take for itself.
fn is_plain_escape(event: &KeyDownEvent) -> bool {
    event.keystroke.key == "escape" && !event.keystroke.modifiers.modified()
}

/// The terminal's own surface around the grid. Painted as a ring rather
/// than under the pane, which paints that surface itself: stacked glass
/// tints compound toward opaque.
fn inset_ring(colors: diri_ui::SemanticColors) -> [gpui::Div; 4] {
    let strip = || div().absolute().bg(colors.terminal_surface());
    [
        strip().left_0().right_0().top_0().h(px(INSET_TOP)),
        strip().left_0().right_0().bottom_0().h(px(INSET_BOTTOM)),
        strip()
            .left_0()
            .top(px(INSET_TOP))
            .bottom(px(INSET_BOTTOM))
            .w(px(INSET_X)),
        strip()
            .right_0()
            .top(px(INSET_TOP))
            .bottom(px(INSET_BOTTOM))
            .w(px(INSET_X)),
    ]
}

impl Render for QuickTerminalView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = crate::app_theme::colors_in(
            &self
                .services
                .store
                .store
                .read()
                .expect("session store lock poisoned"),
        );
        self.frames += 1;
        // Test windows have no AppKit window behind them to ask.
        let on_screen = cfg!(test) || crate::macos::floating_panel::is_on_screen(window);
        if on_screen {
            self.settled_frames += 1;
        }
        if !self.revealed {
            if self.settled_frames >= 1 || self.frames >= REVEAL_DEADLINE_FRAMES {
                self.revealed = true;
                if !cfg!(test) {
                    window.on_next_frame(|window, _| crate::macos::floating_panel::reveal(window));
                }
            } else {
                window.request_animation_frame();
            }
        }
        let viewport = window.viewport_size();
        if let Some(terminal) = &self.terminal {
            terminal.update(cx, |terminal, cx| {
                terminal.set_shell_chrome(false, false, cx);
                terminal.set_header_hidden(true, cx);
                terminal.set_leading_hairline(false, cx);
                terminal.set_viewport(
                    TerminalViewport {
                        x: INSET_X,
                        y: INSET_TOP,
                        width: (f32::from(viewport.width) - 2.0 * INSET_X).max(0.0),
                        height: (f32::from(viewport.height) - INSET_TOP - INSET_BOTTOM).max(0.0),
                    },
                    cx,
                );
            });
        }
        div()
            .id("quick-terminal")
            .key_context("QuickTerminal")
            .track_focus(&self.focus)
            .capture_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                if is_plain_escape(event) && this.hide_on_escape() {
                    cx.stop_propagation();
                    cx.defer(|cx| handle(Event::Escape, cx));
                }
            }))
            .relative()
            .size_full()
            .overflow_hidden()
            .rounded_b(px(RADIUS))
            .text_color(colors.primary)
            .opacity(if self.revealed { 1.0 } else { 0.0 })
            .children(inset_ring(colors))
            .child(
                div()
                    .absolute()
                    .left(px(INSET_X))
                    .right(px(INSET_X))
                    .top(px(INSET_TOP))
                    .bottom(px(INSET_BOTTOM))
                    .children(self.terminal.clone()),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ON: Options = Options {
        hide_on_escape: true,
        autohide: true,
    };
    const OFF: Options = Options {
        hide_on_escape: false,
        autohide: false,
    };

    fn shown(machine: &mut Machine) -> u64 {
        let Some(Effect::Show { generation }) = machine.handle(Event::Toggle, OFF) else {
            panic!("a hidden panel shows on toggle");
        };
        assert_eq!(machine.handle(Event::SlideFinished(generation), OFF), None);
        assert_eq!(machine.phase(), Phase::Shown);
        generation
    }

    #[test]
    fn toggle_shows_then_hides_then_closes_the_window() {
        let mut machine = Machine::default();
        assert_eq!(machine.phase(), Phase::Hidden);
        shown(&mut machine);
        let Some(Effect::Hide { generation }) = machine.handle(Event::Toggle, OFF) else {
            panic!("a shown panel hides on toggle");
        };
        assert_eq!(machine.phase(), Phase::Hiding);
        assert_eq!(
            machine.handle(Event::SlideFinished(generation), OFF),
            Some(Effect::Close)
        );
        assert_eq!(machine.phase(), Phase::Hidden);
    }

    #[test]
    fn a_press_mid_slide_reverses_and_the_old_slide_cannot_close_it() {
        let mut machine = Machine::default();
        let Some(Effect::Show { generation: first }) = machine.handle(Event::Toggle, OFF) else {
            panic!("shows");
        };
        // Pressed again before the slide in finished: slide back out.
        let Some(Effect::Hide { generation: second }) = machine.handle(Event::Toggle, OFF) else {
            panic!("reverses while showing");
        };
        // And again: back in, over the same window.
        let Some(Effect::Show { generation: third }) = machine.handle(Event::Toggle, OFF) else {
            panic!("reverses while hiding");
        };
        assert!(first < second && second < third);
        // The superseded slides finish late; neither may close the panel.
        assert_eq!(machine.handle(Event::SlideFinished(first), OFF), None);
        assert_eq!(machine.handle(Event::SlideFinished(second), OFF), None);
        assert_eq!(machine.phase(), Phase::Showing);
        assert_eq!(machine.handle(Event::SlideFinished(third), OFF), None);
        assert_eq!(machine.phase(), Phase::Shown);
    }

    #[test]
    fn escape_hides_only_when_configured() {
        let mut machine = Machine::default();
        shown(&mut machine);
        assert_eq!(machine.handle(Event::Escape, OFF), None);
        assert_eq!(machine.phase(), Phase::Shown);
        assert!(matches!(
            machine.handle(Event::Escape, ON),
            Some(Effect::Hide { .. })
        ));
        // Hidden, Escape belongs to whatever app is in front.
        let mut hidden = Machine::default();
        assert_eq!(hidden.handle(Event::Escape, ON), None);
    }

    #[test]
    fn losing_focus_hides_a_settled_panel_when_autohide_is_on() {
        let mut machine = Machine::default();
        let Some(Effect::Show { generation }) = machine.handle(Event::Toggle, ON) else {
            panic!("shows");
        };
        // Focus shuffles while the panel takes key on its way in.
        assert_eq!(machine.handle(Event::FocusLost, ON), None);
        machine.handle(Event::SlideFinished(generation), ON);
        assert_eq!(machine.handle(Event::FocusLost, OFF), None);
        assert_eq!(machine.phase(), Phase::Shown);
        assert!(matches!(
            machine.handle(Event::FocusLost, ON),
            Some(Effect::Hide { .. })
        ));
    }

    #[test]
    fn an_exited_shell_slides_the_panel_away() {
        let mut machine = Machine::default();
        assert_eq!(machine.handle(Event::ShellEnded, OFF), None);
        shown(&mut machine);
        assert!(matches!(
            machine.handle(Event::ShellEnded, OFF),
            Some(Effect::Hide { .. })
        ));
        // Already on its way out: nothing more to do.
        assert_eq!(machine.handle(Event::ShellEnded, OFF), None);
    }

    #[test]
    fn panel_hangs_from_the_visible_top_and_hides_just_above_it() {
        // A 1512x982 laptop screen with a 33 pt menu bar and the Dock hidden.
        let visible = PanelRect {
            x: 0.0,
            y: 0.0,
            width: 1512.0,
            height: 949.0,
        };
        let frames = panel_frames(visible);
        assert_eq!(frames.shown.width, 1512.0);
        assert_eq!(frames.shown.height, 380.0);
        assert_eq!(frames.shown.y + frames.shown.height, 949.0);
        assert_eq!(frames.hidden.y, 949.0);
        assert_eq!(frames.hidden.x, frames.shown.x);
        assert_eq!(frames.hidden.height, frames.shown.height);
    }

    #[test]
    fn panel_follows_a_secondary_screen_and_keeps_a_usable_height() {
        // A display to the left of and above the primary one.
        let visible = PanelRect {
            x: -1920.0,
            y: 1080.0,
            width: 1920.0,
            height: 400.0,
        };
        let frames = panel_frames(visible);
        assert_eq!(frames.shown.x, -1920.0);
        assert_eq!(frames.shown.height, MIN_HEIGHT);
        assert_eq!(frames.shown.y, 1480.0 - MIN_HEIGHT);
        assert_eq!(frames.hidden.y, 1480.0);
        // Never taller than the screen itself.
        let tiny = panel_frames(PanelRect {
            height: 200.0,
            ..visible
        });
        assert_eq!(tiny.shown.height, 200.0);
        assert_eq!(tiny.shown.y, visible.y);
    }

    #[test]
    fn reduce_motion_places_the_panel_at_once() {
        assert_eq!(slide_seconds(true, true), 0.0);
        assert_eq!(slide_seconds(false, true), 0.0);
        assert!(slide_seconds(true, false) > slide_seconds(false, false));
        assert!(slide_seconds(false, false) > 0.0);
    }

    #[test]
    fn only_bare_escape_counts() {
        let key = |text: &str| KeyDownEvent {
            keystroke: gpui::Keystroke::parse(text).unwrap(),
            is_held: false,
            prefer_character_input: false,
        };
        assert!(is_plain_escape(&key("escape")));
        assert!(!is_plain_escape(&key("shift-escape")));
        assert!(!is_plain_escape(&key("cmd-escape")));
        assert!(!is_plain_escape(&key("a")));
    }

    fn services() -> Arc<AppServices> {
        Arc::new(AppServices {
            store: Arc::new(crate::store::StoreRuntime::inert()),
            usage_tx: tokio::sync::watch::channel(crate::usage::UsageSnapshot::default()).0,
            usage_limits_refresh: tokio::sync::mpsc::channel(1).0,
            updates: crate::updates::inert(),
            dev_build: None,
            daemon_startup: None,
            tokio: Arc::new(
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap(),
            ),
        })
    }

    fn shell(id: &str) -> diri_proto::SessionRecord {
        let mut record = crate::store::tests::session(id, "home", 1.0);
        record.kind = diri_proto::AgentKind::SHELL;
        record
    }

    #[gpui::test]
    fn the_panel_asks_for_one_shell_then_shows_it_and_leaves_when_it_exits(
        cx: &mut gpui::TestAppContext,
    ) {
        let services = services();
        let store = Arc::clone(&services.store);
        let (view, cx) =
            cx.add_window_view(|window, cx| QuickTerminalView::new(services, window, cx));
        // No remembered shell: the panel requested one, exactly once.
        assert!(store.store.read().unwrap().quick_terminal_spawn_pending());
        view.update_in(cx, |view, window, cx| {
            assert!(view.spawn_requested && view.terminal.is_none());
            view.reconcile(window, cx);
        });

        // The Engine answers and its record arrives.
        {
            let mut store = store.store.write().unwrap();
            store.finish_quick_terminal_spawn(Some(SessionId::new("quick")));
            store.upsert_session(shell("quick"));
        }
        view.update_in(cx, |view, window, cx| {
            view.reconcile(window, cx);
            assert_eq!(view.session, Some(SessionId::new("quick")));
            assert!(view.terminal.is_some(), "the shell is mounted");
        });

        // The shell exits: the panel lets go of it and asks for nothing new.
        {
            let mut ended = shell("quick");
            ended.status = diri_proto::SessionStatus::Exited(diri_proto::ExitInfo {
                reason: diri_proto::ExitReason::Exited,
                code: Some(0),
                signal: None,
                system_restart: false,
                interrupted: false,
            });
            store.store.write().unwrap().upsert_session(ended);
        }
        view.update_in(cx, |view, window, cx| {
            view.reconcile(window, cx);
            assert_eq!(view.session, None);
        });
        assert!(!store.store.read().unwrap().quick_terminal_spawn_pending());
    }

    #[gpui::test]
    fn a_remembered_shell_is_shown_without_spawning(cx: &mut gpui::TestAppContext) {
        let services = services();
        {
            let mut store = services.store.store.write().unwrap();
            store.upsert_session(shell("kept"));
            store
                .update_preferences(|prefs| {
                    prefs.quick_terminal_session = Some(SessionId::new("kept"))
                })
                .unwrap();
        }
        let store = Arc::clone(&services.store);
        let (view, cx) =
            cx.add_window_view(|window, cx| QuickTerminalView::new(services, window, cx));
        view.update_in(cx, |view, _, _| {
            assert_eq!(view.session, Some(SessionId::new("kept")));
            assert!(view.terminal.is_some());
            assert!(!view.spawn_requested);
        });
        assert!(!store.store.read().unwrap().quick_terminal_spawn_pending());
    }

    /// Renders the panel's own window headlessly, edge to edge, with a seeded
    /// shell transcript. `DIRI_VISUAL_LIGHT=1` uses a light theme.
    ///
    /// ```sh
    /// DIRI_VISUAL_OUTPUT=/tmp/quick.png \
    ///   cargo test -p diri-app --bin diri -- --ignored render_quick_terminal_screenshot
    /// ```
    #[test]
    #[ignore = "writes the quick terminal panel to DIRI_VISUAL_OUTPUT"]
    fn render_quick_terminal_screenshot() {
        use diri_proto::grid::{TermColor, TermStyle};
        use diri_term::buffer::GridBuffer;
        use gpui::HeadlessAppContext;

        let output = std::env::var_os("DIRI_VISUAL_OUTPUT")
            .map(std::path::PathBuf::from)
            .expect("set DIRI_VISUAL_OUTPUT to the target PNG path");
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
        let services = services();
        {
            let mut store = services.store.store.write().unwrap();
            store.upsert_session(shell("quick"));
            store
                .update_preferences(|prefs| {
                    prefs.quick_terminal_session = Some(SessionId::new("quick"));
                    if std::env::var_os("DIRI_VISUAL_LIGHT").is_some() {
                        prefs.terminal_theme = "github-light".into();
                    }
                })
                .unwrap();
        }
        // A 1512 pt wide laptop screen: the panel is 40 % of its visible
        // height, as `panel_frames` sizes it.
        let frames = panel_frames(PanelRect {
            x: 0.0,
            y: 0.0,
            width: 1512.0,
            height: 949.0,
        });
        let window = cx
            .open_window(
                size(
                    px(frames.shown.width as f32),
                    px(frames.shown.height as f32),
                ),
                move |window, cx| cx.new(|cx| QuickTerminalView::new(services, window, cx)),
            )
            .expect("open headless quick terminal");
        cx.run_until_parked();

        type Span = (&'static str, TermColor, TermStyle);
        let plain = |text| -> Span { (text, TermColor::Default, TermStyle::empty()) };
        let ansi = |text, index| -> Span { (text, TermColor::Ansi(index), TermStyle::empty()) };
        let bold = |text, index| -> Span { (text, TermColor::Ansi(index), TermStyle::BOLD) };
        let prompt = |command| -> Vec<Span> {
            vec![
                bold("~", 4),
                plain(" "),
                ansi("❯", 2),
                plain(" "),
                plain(command),
            ]
        };
        let lines: Vec<Vec<Span>> = vec![
            prompt("brew upgrade --quiet"),
            vec![bold("==>", 4), bold(" Upgrading 3 outdated packages:", 15)],
            vec![plain(
                "ripgrep 14.1.0 -> 14.1.1, gh 2.62.0 -> 2.63.0, jq 1.7.1 -> 1.8.0",
            )],
            vec![
                ansi("🍺", 3),
                plain("  /opt/homebrew/Cellar/gh/2.63.0: 214 files, 46.1MB"),
            ],
            vec![],
            prompt("dig +short diri.dev"),
            vec![plain("76.76.21.21")],
            vec![],
            prompt("du -sh ~/Downloads"),
            vec![plain(" 12G\t/Users/you/Downloads")],
            vec![],
            prompt(""),
        ];
        let (cols, rows) = (180u16, 20u16);
        let mut grid = GridBuffer::new(cols, rows);
        for (row, spans) in lines.into_iter().enumerate() {
            let mut col = 0;
            for (text, fg, style) in spans {
                for ch in text.chars() {
                    let cell = &mut grid.cells[row * usize::from(cols) + col];
                    cell.scalar = u32::from(ch);
                    cell.fg = fg;
                    cell.style = style;
                    col += 1;
                }
            }
        }
        cx.update_window(window.into(), |root, _, cx| {
            let view = root
                .downcast::<QuickTerminalView>()
                .expect("quick terminal");
            let terminal = view.read(cx).terminal.clone().expect("mounted shell");
            terminal.update(cx, |terminal, cx| {
                terminal.seed_preview_grid_for_test(grid, cx);
                cx.notify();
            });
            view.update(cx, |_, cx| cx.notify());
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(window.into(), |_, window, _| window.refresh())
            .unwrap();
        cx.run_until_parked();
        let image = cx.capture_screenshot(window.into()).expect("capture");
        image.save(&output).expect("write PNG");
    }
}
