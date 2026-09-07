use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use diri_proto::{AgentKind, SessionId, SessionRecord, SessionStatus};
use diri_ui::{FloatingSurface, Ink, Metrics, Radius, SemanticColors, Typo};
use gpui::{
    Animation, AnimationExt, AnyElement, App, BoxShadow, Context, CursorStyle, DragMoveEvent,
    Entity, FocusHandle, Focusable, FontWeight, KeyContext, KeyDownEvent, KeyUpEvent,
    ModifiersChangedEvent, MouseButton, Render, StyleRefinement, Subscription, Task, Window,
    deferred, div, ease_out_quint, prelude::*, px, rgba,
};

use crate::AppServices;
use crate::commands::{
    self, APP_CONTEXT, ArchiveSelectedSession, CheckForUpdates, CloseSession, CommandId,
    DelegateSelectedSession, FocusSidebar, MoveSelectedSessionDown, MoveSelectedSessionUp,
    NewCodexSession, NewDefaultSession, NewTerminal, OpenLauncher, OpenSettings, OpenWorktrees,
    QuoteSelection, QuoteSelectionToSession, RenameSelectedSession, ReopenSession,
    SESSION_NAVIGATION_CONTEXT, SelectLastSession, SelectNextAttentionSession, SelectNextSession,
    SelectPreviousSession, SelectSession1, SelectSession2, SelectSession3, SelectSession4,
    SelectSession5, SelectSession6, SelectSession7, SelectSession8, ToggleAuxiliaryTerminal,
    ToggleCommandPalette, ToggleHistory, ToggleInspector, ToggleOverview, ToggleQuickOpen,
    ToggleSidebar,
};
use crate::external_drop::ExternalDropAction;
use crate::icons::{SymbolWeight, sf_symbol, sf_symbol_weighted};
use crate::inspector::{BrowserAction, InspectorEvent, WorkbenchInspector};
use crate::launcher::{LauncherEvent, LauncherOverlay};
#[cfg(target_os = "macos")]
use crate::macos::browser::NativeBrowser;
use crate::navigation::NavigationOverlay;
use crate::notifications::{InAppBanner, NotificationSound};
use crate::quote::Quote;
use crate::recovery::{RecoveryAction, RecoveryKind, RecoveryNotice};
use crate::seam::{SeamSlide, toggle_has_settled};
use crate::session_surfaces::SessionSurfaces;
use crate::sidebar::{PreviewScenario, Sidebar, SidebarEvent};
use crate::sounds::{self, PlatformPlayer, SoundGate, StatusSound};
use crate::store::SpawnOptions;
use crate::surface_shell::UtilitySurfaces;
use crate::terminal_pane::{TerminalPane, TerminalPaneEvent, TerminalViewport};
use crate::updates::UpdatePhase;
use crate::workbench::WorkbenchLayout;

#[path = "notification_panel.rs"]
mod notification_panel;
#[cfg(test)]
#[path = "notification_panel_tests.rs"]
mod notification_panel_tests;

const WINDOW_BOUNDS_SAVE_DELAY: Duration = Duration::from_millis(150);
const SIDEBAR_PEEK_INSET: f32 = 10.0;
const SIDEBAR_PEEK_DWELL: Duration = Duration::from_millis(20);
const SIDEBAR_PEEK_REVEAL: Duration = Duration::from_millis(140);
const SIDEBAR_PEEK_TRIGGER_WIDTH: f32 = 24.0;

pub(crate) fn cached_window_overlay<T: Render>(view: Entity<T>) -> impl IntoElement {
    view.cached(StyleRefinement::default().absolute().inset_0())
}

/// Joins the two halves of the settings destination: the surface owns the
/// state and paints the page, the sidebar paints the navigation beside it.
///
/// The mirror is one-directional -- the surface's state projects into the
/// sidebar, and the sidebar reports clicks back -- so the list and the page
/// cannot drift into disagreeing about the selected page or the search text.
/// RootView wires this for the window; tests wire the same two entities.
pub(crate) fn wire_settings_navigation<V: 'static>(
    sidebar: Entity<Sidebar>,
    surfaces: Entity<UtilitySurfaces>,
    window: &mut Window,
    cx: &mut Context<V>,
) -> [Subscription; 2] {
    let mirror = {
        let sidebar = sidebar.clone();
        cx.observe(&surfaces, move |_, surfaces, cx| {
            let nav = surfaces.read(cx).settings_nav();
            sidebar.update(cx, |sidebar, cx| sidebar.set_settings_nav(nav, cx));
            cx.notify();
        })
    };
    let clicks = cx.subscribe_in(
        &sidebar,
        window,
        move |_, _, event, window, cx| match event {
            SidebarEvent::SettingsTabSelected(tab) => {
                let tab = *tab;
                surfaces.update(cx, |surfaces, cx| surfaces.open_settings_tab(tab, cx));
            }
            SidebarEvent::SettingsSearchFocused => {
                surfaces.update(cx, |surfaces, cx| {
                    surfaces.focus_settings_search(window, cx)
                });
            }
            SidebarEvent::SettingsSearchCleared => {
                surfaces.update(cx, |surfaces, cx| {
                    surfaces.clear_settings_search(window, cx)
                });
            }
            SidebarEvent::SettingsDismissed => {
                surfaces.update(cx, |surfaces, cx| surfaces.dismiss(cx));
            }
            _ => {}
        },
    );
    [mirror, clicks]
}

#[cfg(target_os = "macos")]
use crate::macos::{menu_bar::NativeMenuBar, notifier::NativeNotifier};

/// Drag payload for the sidebar resize seam. Renders nothing -- it exists so
/// GPUI keeps routing mouse moves to the root while the seam is being dragged.
#[derive(Clone, Copy)]
struct DraggedSidebarEdge;

impl Render for DraggedSidebarEdge {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        gpui::Empty
    }
}

/// Drag payload for the horizontal workbench divider.
#[derive(Clone, Copy)]
struct DraggedTerminalEdge;

impl Render for DraggedTerminalEdge {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        gpui::Empty
    }
}

/// Drag payload for the workbench/inspector seam.
#[derive(Clone, Copy)]
struct DraggedInspectorEdge;

impl Render for DraggedInspectorEdge {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        gpui::Empty
    }
}

#[derive(Clone, Debug)]
struct QuoteTargetPicker {
    quote: Quote,
    targets: Vec<SessionRecord>,
    highlighted: usize,
    return_surface: QuoteSurface,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum QuoteSurface {
    #[default]
    PrimaryTerminal,
    AuxiliaryTerminal,
    Inspector,
}

/// Advances one panel's seam by a frame and returns the width to paint,
/// clearing the slide once it lands. An unfinished slide asks for the next
/// frame itself: the seam is a plain animated width rather than a GPUI
/// animation element, so nothing else will tick the window.
///
/// Takes the slide by `&mut Option<_>` rather than hanging off `RootView` so
/// both seams can be advanced in one pass without borrowing all of `self`.
fn advance_seam(slide: &mut Option<SeamSlide>, settled: f32, now: Instant, window: &Window) -> f32 {
    match *slide {
        Some(active) if !active.is_done(now) => {
            window.request_animation_frame();
            active.seam_at(settled, now)
        }
        Some(_) => {
            *slide = None;
            settled
        }
        None => settled,
    }
}

pub struct RootView {
    sidebar: Entity<Sidebar>,
    terminal: Option<Entity<TerminalPane>>,
    navigation: Option<Entity<NavigationOverlay>>,
    session_surfaces: Option<Entity<SessionSurfaces>>,
    utility_surfaces: Option<Entity<UtilitySurfaces>>,
    launcher: Entity<LauncherOverlay>,
    inspector: Option<Entity<WorkbenchInspector>>,
    #[cfg(target_os = "macos")]
    browser: std::rc::Rc<std::cell::RefCell<NativeBrowser>>,
    services: Arc<AppServices>,
    focus: FocusHandle,
    resize_origin: Option<(f32, f32)>,
    /// The sidebar open/close currently being painted, if any.
    sidebar_slide: Option<SeamSlide>,
    /// The sidebar seam width painted on the last frame. A new slide starts
    /// from this rather than from the settled width so it picks up wherever the
    /// previous frame left the panel.
    sidebar_seam: f32,
    /// The panel is always mounted in one absolute slot. Only this exposure
    /// and its floating treatment change; the layout seam independently makes room.
    sidebar_panel_slide: Option<SeamSlide>,
    sidebar_panel_width: f32,
    sidebar_float_slide: Option<SeamSlide>,
    sidebar_float: f32,
    sidebar_floating: bool,
    sidebar_peek_dwell: Option<Task<()>>,
    auxiliary_terminal: Option<Entity<TerminalPane>>,
    auxiliary_id: Option<SessionId>,
    auxiliary_parent: Option<SessionId>,
    auxiliary_spawn_parent: Option<SessionId>,
    collapsed_auxiliary_parents: HashSet<SessionId>,
    workbench_layout: WorkbenchLayout,
    terminal_resize_origin: Option<(f32, f32)>,
    terminal_available_height: f32,
    inspector_open: bool,
    inspector_width: f32,
    inspector_max_width: f32,
    inspector_resize_origin: Option<(f32, f32)>,
    /// The inspector's mirror of `sidebar_slide` / `sidebar_seam`.
    inspector_slide: Option<SeamSlide>,
    inspector_seam: f32,
    /// When the inspector last opened or closed, so a held ⌘⇧D cannot outrun
    /// its slide. The sidebar's equivalent lives on the sidebar itself, which
    /// owns its own visibility; the inspector's lives here because RootView is
    /// what owns that flag.
    inspector_toggled_at: Option<Instant>,
    /// Debounces move/resize persistence while retaining the newest placement
    /// in memory immediately (the quit hook flushes that value synchronously).
    window_bounds_save: Option<Task<()>>,
    status_banner: Option<InAppBanner>,
    status_banner_generation: u64,
    quote_target_picker: Option<QuoteTargetPicker>,
    notification_panel_open: bool,
    notification_filter_unread: bool,
    notification_selected: usize,
    notification_scroll: gpui::UniformListScrollHandle,
    notification_options_open: bool,
    notification_focus: FocusHandle,
    notification_health: String,
    pending_notification_open: Option<(SessionId, Option<String>)>,
    #[cfg(target_os = "macos")]
    _notification_events: Task<()>,
    last_quote_surface: QuoteSurface,
    sound_gate: SoundGate,
    /// Set when opening settings had to reveal a hidden sidebar to put its
    /// navigation somewhere, so closing settings can hide it again.
    sidebar_revealed_for_settings: bool,
    preview: bool,
    preview_scenario: PreviewScenario,
    #[cfg(target_os = "macos")]
    menu_bar: Option<NativeMenuBar>,
    #[cfg(target_os = "macos")]
    notifier: NativeNotifier,
    _subscriptions: Vec<Subscription>,
    _service_events: Task<()>,
    _surface_sync: Option<Task<()>>,
    _workbench_sync: Task<()>,
    #[cfg(target_os = "macos")]
    _browser_state_sync: Task<()>,
}

impl Focusable for RootView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl RootView {
    pub(crate) fn new(
        services: Arc<AppServices>,
        preview: bool,
        preview_scenario: PreviewScenario,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let sidebar_runtime = (!preview).then(|| Arc::clone(&services.store));
        let sidebar = cx.new(|cx| {
            let mut sidebar = Sidebar::new(sidebar_runtime, preview, preview_scenario, cx);
            sidebar.set_surface_in_parent();
            sidebar
        });
        let terminal = (!preview || preview_scenario == PreviewScenario::Empty).then(|| {
            let runtime = Arc::clone(&services.store);
            let tokio = Arc::clone(&services.tokio);
            cx.new(|cx| TerminalPane::new(runtime, tokio, window, cx))
        });
        let navigation = (!preview).then(|| {
            let runtime = Arc::clone(&services.store);
            cx.new(|cx| NavigationOverlay::new(runtime, Arc::clone(&services.tokio), window, cx))
        });
        let session_surfaces = (!preview).then(|| {
            let runtime = Arc::clone(&services.store);
            cx.new(|cx| SessionSurfaces::new(runtime, cx))
        });
        let utility_surfaces = (!preview).then(|| {
            let runtime = Arc::clone(&services.store);
            let tokio = Arc::clone(&services.tokio);
            let updates = services.updates.clone();
            cx.new(|cx| UtilitySurfaces::new(runtime, tokio, updates, window, cx))
        });
        let launcher = cx.new(|cx| LauncherOverlay::new(Arc::clone(&services), preview, cx));
        let inspector = (!preview || preview_scenario == PreviewScenario::Artifacts).then(|| {
            let runtime = Arc::clone(&services.store);
            let tokio = Arc::clone(&services.tokio);
            cx.new(|cx| WorkbenchInspector::new(runtime, tokio, cx))
        });
        if let (Some(terminal), Some(navigation), Some(utility_surfaces)) =
            (&terminal, &navigation, &utility_surfaces)
        {
            let navigation = navigation.clone();
            let utility_surfaces = utility_surfaces.clone();
            terminal.update(cx, |terminal, _| {
                terminal.set_shell_entities(navigation, utility_surfaces);
            });
        }
        if let Some(terminal) = &terminal {
            let terminal = terminal.clone();
            cx.defer_in(window, move |_, window, cx| {
                terminal.update(cx, |terminal, cx| terminal.focus(window, cx));
            });
        }
        if let Some(terminal) = &terminal {
            cx.subscribe_in(terminal, window, |this, _, event, window, cx| match event {
                TerminalPaneEvent::ContinueAccount(id) => {
                    if let Some(surfaces) = &this.utility_surfaces {
                        surfaces.update(cx, |surfaces, cx| {
                            surfaces.open_account_continuation(id.clone(), window, cx)
                        });
                    }
                }
                TerminalPaneEvent::OpenFileReference { reference, cwd, .. } => {
                    let inspector = this.inspector.clone();
                    this.reveal_inspector(cx);
                    if let Some(inspector) = inspector {
                        inspector.update(cx, |inspector, cx| {
                            inspector.open_file_reference(cwd.clone(), reference.clone(), cx);
                        });
                    }
                }
                TerminalPaneEvent::ExternalDropFeedback { message } => {
                    this.show_quote_feedback("Dropped files", message.clone(), cx);
                }
            })
            .detach();
        }
        cx.subscribe_in(&sidebar, window, |this, _, event, window, cx| {
            if matches!(event, SidebarEvent::RefreshUsageLimits) {
                let _ = this.services.usage_limits_refresh.try_send(());
            }
            if let SidebarEvent::ContinueAccount(id) = event
                && let Some(surfaces) = &this.utility_surfaces
            {
                surfaces.update(cx, |surfaces, cx| {
                    surfaces.open_account_continuation(id.clone(), window, cx)
                });
            }
            if let SidebarEvent::HandoffProposed(proposal) = event {
                this.launcher.update(cx, |launcher, cx| {
                    launcher.open_handoff(proposal.clone(), window, cx);
                });
            }
            if let SidebarEvent::ExternalDrop(plan) = event
                && let Some(action) = &plan.action
            {
                let notice = plan.feedback();
                match action {
                    ExternalDropAction::OpenLauncher { root } => {
                        this.launcher.update(cx, |launcher, cx| {
                            launcher.open_at_directory(root.clone(), notice, window, cx);
                        });
                    }
                    ExternalDropAction::OpenSessionComposer {
                        session_id,
                        insertion,
                    } => {
                        this.launcher.update(cx, |launcher, cx| {
                            launcher.open_local_paths_for_session(
                                session_id.clone(),
                                insertion,
                                notice,
                                window,
                                cx,
                            );
                        });
                    }
                }
                // Like Command-N, a drop swaps the main-pane branch. Focus
                // once more after GPUI mounts the composer so the insertion
                // caret is ready without a click.
                let launcher = this.launcher.clone();
                cx.defer_in(window, move |_, window, cx| {
                    launcher.update(cx, |launcher, cx| launcher.focus(window, cx));
                });
            }
            if matches!(event, SidebarEvent::SessionActivated) {
                if this.launcher.read(cx).is_open() {
                    this.launcher
                        .update(cx, |launcher, cx| launcher.dismiss(cx));
                }
                if let Some(terminal) = &this.terminal {
                    terminal.update(cx, |terminal, cx| terminal.focus(window, cx));
                    this.sync_auxiliary_terminal(window, cx);
                }
            }
            if matches!(event, SidebarEvent::FocusTerminal) {
                if let Some(terminal) = &this.terminal {
                    terminal.update(cx, |terminal, cx| terminal.focus(window, cx));
                    this.sync_auxiliary_terminal(window, cx);
                } else {
                    window.focus(&this.focus, cx);
                }
            }
            if let SidebarEvent::Update(command) = event {
                this.services.updates.send(command.clone());
            }
            if let SidebarEvent::OpenAgentSettings(host) = event
                && let Some(surfaces) = &this.utility_surfaces
            {
                surfaces.update(cx, |surfaces, cx| {
                    surfaces.open_agent_settings(host.clone(), cx);
                });
            }
            if matches!(event, SidebarEvent::AddRemoteHost)
                && let Some(surfaces) = &this.utility_surfaces
            {
                surfaces.update(cx, |surfaces, cx| {
                    surfaces.open_add_remote_host(window, cx);
                });
            }
            if matches!(event, SidebarEvent::VisibilityChanged) {
                this.sidebar_peek_dwell = None;
                this.sidebar_floating = false;
                this.begin_sidebar_slide(cx);
                // Settings navigation lives in the sidebar, so hiding the
                // sidebar is also the way out of settings.
                if !this.sidebar.read(cx).is_visible()
                    && let Some(surfaces) = &this.utility_surfaces
                    && surfaces.read(cx).is_settings_open()
                {
                    this.sidebar_revealed_for_settings = false;
                    surfaces.update(cx, |surfaces, cx| surfaces.dismiss(cx));
                }
            }
            if matches!(event, SidebarEvent::PeekChanged) {
                this.sidebar_floating = true;
                // Establish the floating shape offscreen; exits retain it.
                if this.sidebar_panel_width == 0.0 {
                    this.sidebar_float = 1.0;
                }
                this.begin_sidebar_panel_slide(Instant::now(), cx);
            }
            cx.notify();
        })
        .detach();
        if let Some(surfaces) = &utility_surfaces {
            // Settings navigation is painted by the sidebar, so a settings
            // that opened onto a hidden sidebar has to bring it back -- and
            // give it up again on the way out.
            cx.observe(surfaces, |this, surfaces, cx| {
                let open = surfaces.read(cx).is_settings_open();
                if open && !this.sidebar.read(cx).is_visible() {
                    this.sidebar_revealed_for_settings = true;
                    this.sidebar.update(cx, |sidebar, cx| sidebar.reveal(cx));
                } else if !open && std::mem::take(&mut this.sidebar_revealed_for_settings) {
                    this.sidebar.update(cx, |sidebar, cx| sidebar.conceal(cx));
                }
            })
            .detach();
            for subscription in
                wire_settings_navigation(sidebar.clone(), surfaces.clone(), window, cx)
            {
                subscription.detach();
            }
        }
        cx.subscribe_in(
            &launcher,
            window,
            |this, _, event: &LauncherEvent, window, cx| {
                if matches!(event, LauncherEvent::ManageAccounts)
                    && let Some(surfaces) = &this.utility_surfaces
                {
                    surfaces.update(cx, |surfaces, cx| {
                        surfaces.open_settings(cx);
                        surfaces.open_settings_tab(crate::settings::SettingsTab::Accounts, cx);
                        surfaces.focus_handle(cx).focus(window, cx);
                    });
                    cx.notify();
                    return;
                }
                if let LauncherEvent::ManageAgents(host) = event
                    && let Some(surfaces) = &this.utility_surfaces
                {
                    surfaces.update(cx, |surfaces, cx| {
                        surfaces.open_agent_settings(host.clone(), cx);
                    });
                }
                if let Some(terminal) = &this.terminal {
                    terminal.update(cx, |terminal, cx| terminal.focus(window, cx));
                } else {
                    window.focus(&this.focus, cx);
                }
                // The launcher is a main-pane destination, so closing it must
                // make RootView swap the terminal branch back into the row.
                cx.notify();
            },
        )
        .detach();
        if let Some(inspector) = &inspector {
            cx.subscribe_in(
                inspector,
                window,
                |this, _, event, window, cx| match event {
                    InspectorEvent::Close => {
                        // A removed focus path cannot route workbench shortcuts.
                        window.focus(&this.focus, cx);
                        this.inspector_toggled_at = None;
                        this.set_inspector_open(false, cx);
                        this.inspector_toggled_at = None;
                    }
                    InspectorEvent::SessionChanged => {
                        this.inspector_open = this
                            .inspector
                            .as_ref()
                            .is_some_and(|inspector| inspector.read(cx).is_visible());
                        this.inspector_toggled_at = None;
                        this.begin_inspector_slide(cx);
                        cx.notify();
                    }
                    InspectorEvent::WorkspaceChanged(surface) => {
                        window.focus(&this.focus, cx);
                        #[cfg(target_os = "macos")]
                        if *surface == crate::inspector::WorkspaceSurface::Browser
                            && let Some(inspector) = &this.inspector
                            && let Some(id) = inspector.read(cx).active_workspace_id()
                        {
                            this.browser.borrow_mut().select_tab(id);
                            let state = this.browser.borrow().state();
                            let blank = state.url.is_none();
                            inspector.update(cx, |inspector, cx| {
                                inspector.set_browser_state(state, cx);
                                if blank {
                                    inspector.focus_browser_address(window, cx);
                                }
                            });
                        }
                        if let Some(terminal) = &this.auxiliary_terminal
                            && *surface == crate::inspector::WorkspaceSurface::Terminal
                        {
                            terminal.update(cx, |terminal, cx| terminal.focus(window, cx));
                        }
                        cx.notify();
                    }
                    InspectorEvent::RequestTerminal => {
                        this.ensure_auxiliary_terminal(window, cx);
                    }
                    InspectorEvent::WorkspaceClosed { surface, id } => {
                        #[cfg(not(target_os = "macos"))]
                        let _ = id;
                        #[cfg(target_os = "macos")]
                        if *surface == crate::inspector::WorkspaceSurface::Browser {
                            this.browser.borrow_mut().close_tab(*id);
                        }
                        if *surface == crate::inspector::WorkspaceSurface::Terminal
                            && !this.inspector.as_ref().is_some_and(|inspector| {
                                inspector.read(cx).workspace_needs_terminal()
                            })
                        {
                            this.hide_auxiliary_terminal(window, cx);
                        }
                        window.focus(&this.focus, cx);
                        cx.notify();
                    }
                    InspectorEvent::Browser(action) => {
                        #[cfg(target_os = "macos")]
                        match action {
                            BrowserAction::Navigate(url) => {
                                this.browser.borrow_mut().load(url.clone())
                            }
                            BrowserAction::Back => this.browser.borrow().go_back(),
                            BrowserAction::Forward => this.browser.borrow().go_forward(),
                            BrowserAction::Reload => this.browser.borrow().reload(),
                            BrowserAction::OpenExternal(url) => cx.open_url(url),
                        }
                        #[cfg(not(target_os = "macos"))]
                        if let BrowserAction::Navigate(url) | BrowserAction::OpenExternal(url) =
                            action
                        {
                            cx.open_url(url);
                        }
                        cx.notify();
                    }
                },
            )
            .detach();
        }

        let mut status_events = services.store.status_events();
        let mut snapshots = services.store.snapshots();
        let mut usage = services.usage_tx.subscribe();
        let mut updates = services.updates.subscribe();
        sidebar.update(cx, |sidebar, cx| {
            sidebar.set_usage(usage.borrow().clone(), cx)
        });
        if let Some(surfaces) = &utility_surfaces {
            surfaces.update(cx, |surfaces, cx| {
                surfaces.set_usage(usage.borrow().clone(), cx)
            });
        }
        // Seed the current state: `watch` only wakes on changes, and an
        // unsupported build settles before this view exists.
        let initial_update = services.updates.state();
        sidebar.update(cx, |sidebar, cx| sidebar.set_update(initial_update, cx));

        #[cfg(target_os = "macos")]
        let mut menu_bar = objc2_foundation::MainThreadMarker::new()
            .and_then(|mtm| NativeMenuBar::new(mtm, Arc::clone(&services.store.store)));
        #[cfg(target_os = "macos")]
        if let Some(menu_bar) = &mut menu_bar {
            menu_bar.refresh();
        }
        #[cfg(target_os = "macos")]
        let (notification_tx, mut notification_rx) = tokio::sync::mpsc::unbounded_channel();
        #[cfg(target_os = "macos")]
        let notifier = NativeNotifier::new(notification_tx);
        #[cfg(target_os = "macos")]
        let notification_events = cx.spawn_in(window, async move |this, cx| {
            while let Some(event) = notification_rx.recv().await {
                if this
                    .update_in(cx, |this, window, cx| {
                        use crate::macos::notifier::NativeNotificationEvent;
                        match event {
                            NativeNotificationEvent::Open {
                                session_id,
                                notification_id,
                            } => {
                                this.open_notification(
                                    SessionId::new(session_id),
                                    Some(notification_id),
                                    window,
                                    cx,
                                );
                            }
                            NativeNotificationEvent::Read(id) => {
                                this.services
                                    .store
                                    .store
                                    .write()
                                    .expect("store")
                                    .set_notification_read(&id, true);
                            }
                            NativeNotificationEvent::Health(message) => {
                                this.notification_health = message
                            }
                        }
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        });

        let activation_services = Arc::clone(&services);
        let activation = cx.observe_window_activation(window, move |_this, window, _cx| {
            activation_services
                .store
                .store
                .write()
                .expect("session store lock poisoned")
                .set_active(window.is_window_active());
        });
        let bounds_observer = (!preview).then(|| {
            cx.observe_window_bounds(window, |this, window, cx| {
                this.window_bounds_changed(window, cx);
            })
        });

        let service_sidebar = sidebar.clone();
        let service_events = cx.spawn(async move |this, cx| {
            loop {
                tokio::select! {
                    status = status_events.recv() => {
                        let status = match status {
                            Ok(status) => status,
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        };
                        let _ = this.update(cx, |this, cx| {
                            #[cfg(target_os = "macos")]
                            let app_is_active = this
                                .services
                                .store
                                .store
                                .read()
                                .expect("session store lock poisoned")
                                .app_is_active();
                            #[cfg(target_os = "macos")]
                            this.notifier.dismiss(&status.dismiss);
                            let deliver = status.notification.as_ref().is_none_or(|request| this.services.store.store.read().expect("store").should_deliver_notification(request));
                            if let Some(sound) = status.sound && deliver {
                                let sound = match sound {
                                    NotificationSound::NeedsInput => StatusSound::NeedsInput,
                                    NotificationSound::Done => StatusSound::Done,
                                    NotificationSound::Frozen => StatusSound::Frozen,
                                };
                                if this.sound_gate.should_play(sound, Instant::now()) {
                                    let _ = sounds::play(&PlatformPlayer, sound);
                                }
                            }
                            #[cfg(target_os = "macos")]
                            if let Some(notification) = &status.notification
                                && deliver && (!app_is_active || status.in_app_banner.is_none())
                            {
                                this.notifier.post(notification);
                            }
                            if let Some(banner) = status.in_app_banner {
                                this.status_banner_generation =
                                    this.status_banner_generation.wrapping_add(1);
                                let generation = this.status_banner_generation;
                                this.status_banner = Some(banner);
                                cx.notify();
                                cx.spawn(async move |this, cx| {
                                    cx.background_executor()
                                        .timer(Duration::from_secs(7))
                                        .await;
                                    let _ = this.update(cx, |this, cx| {
                                        if this.status_banner_generation == generation {
                                            this.status_banner = None;
                                            cx.notify();
                                        }
                                    });
                                })
                                .detach();
                            }
                        });
                    }
                    changed = snapshots.changed() => {
                        if changed.is_err() { break; }
                        let _ = snapshots.borrow_and_update();
                        let _ = this.update(cx, |_this, _cx| {
                            #[cfg(target_os = "macos")]
                            if let Some(menu_bar) = &mut _this.menu_bar {
                                menu_bar.refresh();
                            }
                        });
                    }
                    changed = usage.changed() => {
                        if changed.is_err() { break; }
                        let snapshot = usage.borrow_and_update().clone();
                        service_sidebar.update(cx, |sidebar, cx| {
                            sidebar.set_usage(snapshot.clone(), cx);
                        });
                        let _ = this.update(cx, |this, cx| {
                            if let Some(surfaces) = &this.utility_surfaces {
                                surfaces.update(cx, |surfaces, cx| surfaces.set_usage(snapshot, cx));
                            }
                        });
                    }
                    changed = updates.changed() => {
                        if changed.is_err() { break; }
                        let state = updates.borrow_and_update().clone();
                        let installing = state.phase == UpdatePhase::Installing;
                        service_sidebar.update(cx, |sidebar, cx| {
                            sidebar.set_update(state, cx);
                        });
                        // The swap helper is already polling for this process
                        // to exit; quitting is what lets the install proceed.
                        if installing {
                            cx.update(|cx| cx.quit());
                        }
                    }
                }
            }
        });
        let surface_sync =
            terminal
                .as_ref()
                .zip(session_surfaces.as_ref())
                .map(|(terminal, surfaces)| {
                    let terminal = terminal.clone();
                    let surfaces = surfaces.clone();
                    let mut changes = services.store.changes();
                    cx.spawn(async move |_this, cx| {
                        loop {
                            match changes.recv().await {
                                Ok(())
                                | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                                    let buffers = terminal
                                        .update(cx, |terminal, _| terminal.resident_buffers());
                                    surfaces.update(cx, |surfaces, _| {
                                        surfaces.sync_resident_buffers(buffers);
                                    });
                                }
                                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                            }
                        }
                    })
                });
        let mut workbench_changes = services.store.changes();
        let workbench_sync = cx.spawn_in(window, async move |this, cx| {
            loop {
                match workbench_changes.recv().await {
                    Ok(()) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        if this
                            .update_in(cx, |this, window, cx| {
                                // This loop runs on every store change; probe under
                                // a read lock so only the rare menu-bar request
                                // pays for exclusive access.
                                let pending = this
                                    .services
                                    .store
                                    .store
                                    .read()
                                    .expect("session store lock poisoned")
                                    .has_pending_ui_request();
                                let (open_launcher, open_settings) = if pending {
                                    let mut store = this
                                        .services
                                        .store
                                        .store
                                        .write()
                                        .expect("session store lock poisoned");
                                    (
                                        store.take_open_launcher_request(),
                                        store.take_open_settings_request(),
                                    )
                                } else {
                                    (false, false)
                                };
                                if open_launcher {
                                    this.open_launcher(&OpenLauncher, window, cx);
                                }
                                if open_settings && let Some(surfaces) = &this.utility_surfaces {
                                    surfaces.update(cx, |surfaces, cx| surfaces.open_settings(cx));
                                }
                                if let Some(inspector) = &this.inspector {
                                    inspector.update(cx, |inspector, cx| {
                                        inspector.sync_workspace_session(cx)
                                    });
                                }
                                this.sync_auxiliary_terminal(window, cx);
                                cx.notify();
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
        let (workbench_layout, inspector_open, inspector_width) = {
            let store = services
                .store
                .store
                .read()
                .expect("session store lock poisoned");
            let prefs = store.preferences();
            (
                WorkbenchLayout::from_fraction(prefs.workbench_primary_fraction),
                prefs.inspector_open,
                prefs.inspector_width,
            )
        };
        if inspector_open && let Some(inspector) = &inspector {
            inspector.update(cx, |inspector, cx| inspector.set_visible(true, cx));
        }
        // Seed both seams from the restored layout so the first frame paints
        // the settled panels instead of sliding them open at launch.
        let sidebar_seam = if sidebar.read(cx).is_visible() {
            sidebar.read(cx).width()
        } else {
            0.0
        };
        let inspector_seam = if inspector_open { inspector_width } else { 0.0 };
        #[cfg(target_os = "macos")]
        let (browser, mut browser_events) = NativeBrowser::new();
        #[cfg(target_os = "macos")]
        let browser = std::rc::Rc::new(std::cell::RefCell::new(browser));
        #[cfg(target_os = "macos")]
        if let Some(inspector) = &inspector {
            inspector.update(cx, |inspector, _| {
                inspector.set_native_browser(browser.clone())
            });
        }
        #[cfg(target_os = "macos")]
        let browser_state_sync = cx.spawn_in(window, async move |this, cx| {
            while browser_events.recv().await.is_some() {
                if this
                    .update_in(cx, |this, _window, cx| {
                        if let Some(inspector) = this.inspector.clone() {
                            let states = this.browser.borrow().tab_states();
                            inspector.update(cx, |inspector, cx| {
                                for (id, state) in states {
                                    inspector.set_browser_tab_state(id, state, cx);
                                }
                            });
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    return;
                }
            }
        });
        let mut root = Self {
            sidebar,
            terminal,
            navigation,
            session_surfaces,
            utility_surfaces,
            launcher,
            inspector,
            #[cfg(target_os = "macos")]
            browser,
            services,
            focus: cx.focus_handle(),
            resize_origin: None,
            sidebar_slide: None,
            sidebar_panel_slide: None,
            sidebar_panel_width: sidebar_seam,
            sidebar_float_slide: None,
            sidebar_float: 0.0,
            sidebar_floating: false,
            sidebar_peek_dwell: None,
            sidebar_seam,
            auxiliary_terminal: None,
            auxiliary_id: None,
            auxiliary_parent: None,
            auxiliary_spawn_parent: None,
            collapsed_auxiliary_parents: HashSet::new(),
            workbench_layout,
            terminal_resize_origin: None,
            terminal_available_height: 0.0,
            inspector_open,
            inspector_width,
            inspector_max_width: 720.0,
            inspector_slide: None,
            inspector_seam,
            inspector_toggled_at: None,
            inspector_resize_origin: None,
            window_bounds_save: None,
            status_banner: None,
            status_banner_generation: 0,
            quote_target_picker: None,
            notification_panel_open: false,
            notification_filter_unread: true,
            notification_selected: 0,
            notification_scroll: gpui::UniformListScrollHandle::new(),
            notification_options_open: false,
            notification_focus: cx.focus_handle(),
            pending_notification_open: None,
            notification_health:
                "Use Test alert to check macOS delivery. Notifications remain available here."
                    .into(),
            #[cfg(target_os = "macos")]
            _notification_events: notification_events,
            last_quote_surface: QuoteSurface::default(),
            sound_gate: SoundGate::default(),
            sidebar_revealed_for_settings: false,
            preview,
            preview_scenario,
            #[cfg(target_os = "macos")]
            menu_bar,
            #[cfg(target_os = "macos")]
            notifier,
            _subscriptions: std::iter::once(activation).chain(bounds_observer).collect(),
            _service_events: service_events,
            _surface_sync: surface_sync,
            _workbench_sync: workbench_sync,
            #[cfg(target_os = "macos")]
            _browser_state_sync: browser_state_sync,
        };
        root.sync_auxiliary_terminal(window, cx);
        if !preview {
            // Do not rely on AppKit emitting a move/resize after the observer
            // is installed: even an untouched first launch should become the
            // placement restored by the next launch.
            root.window_bounds_changed(window, cx);
        }
        root
    }

    fn window_bounds_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let placement = crate::current_window_placement(window, cx);
        self.services
            .store
            .store
            .write()
            .expect("session store lock poisoned")
            .remember_window_placement(placement);

        if self.window_bounds_save.is_some() {
            return;
        }
        self.window_bounds_save = Some(cx.spawn_in(window, async move |this, cx| {
            cx.background_executor()
                .timer(WINDOW_BOUNDS_SAVE_DELAY)
                .await;
            let _ = this.update_in(cx, |this, _window, _cx| {
                this.window_bounds_save.take();
                if let Err(error) = this
                    .services
                    .store
                    .store
                    .write()
                    .expect("session store lock poisoned")
                    .persist_preferences()
                {
                    eprintln!("diri: could not remember window placement: {error}");
                }
            });
        }));
    }

    fn colors(&self) -> SemanticColors {
        let store = self
            .services
            .store
            .store
            .read()
            .expect("session store lock poisoned");
        crate::app_theme::colors(store.theme_id())
    }

    fn show_quote_feedback(
        &mut self,
        title: impl Into<String>,
        body: impl Into<String>,
        cx: &mut Context<Self>,
    ) {
        self.status_banner_generation = self.status_banner_generation.wrapping_add(1);
        let generation = self.status_banner_generation;
        self.status_banner = Some(InAppBanner {
            title: title.into(),
            body: body.into(),
        });
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_secs(4)).await;
            let _ = this.update(cx, |this, cx| {
                if this.status_banner_generation == generation {
                    this.status_banner = None;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn focused_quote_surface(&self, window: &Window, cx: &App) -> Option<QuoteSurface> {
        if let Some(auxiliary) = &self.auxiliary_terminal
            && auxiliary.read(cx).is_focused(window)
        {
            return Some(QuoteSurface::AuxiliaryTerminal);
        }
        if let Some(inspector) = &self.inspector
            && inspector.read(cx).is_focused(window)
        {
            return Some(QuoteSurface::Inspector);
        }
        if let Some(terminal) = &self.terminal
            && terminal.read(cx).is_focused(window)
        {
            return Some(QuoteSurface::PrimaryTerminal);
        }
        None
    }

    fn quote_from_surface(&self, surface: QuoteSurface, cx: &App) -> Option<Quote> {
        match surface {
            QuoteSurface::PrimaryTerminal => self
                .terminal
                .as_ref()
                .and_then(|terminal| terminal.read(cx).quote_selection()),
            QuoteSurface::AuxiliaryTerminal => self
                .auxiliary_terminal
                .as_ref()
                .and_then(|terminal| terminal.read(cx).quote_selection()),
            QuoteSurface::Inspector => self
                .inspector
                .as_ref()
                .and_then(|inspector| inspector.read(cx).quote_selection()),
        }
    }

    fn remember_quote_surface(&mut self, window: &Window, cx: &App) {
        if let Some(surface) = self.focused_quote_surface(window, cx) {
            self.last_quote_surface = surface;
        }
    }

    fn restore_quote_focus(
        &self,
        surface: QuoteSurface,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let handle = match surface {
            QuoteSurface::PrimaryTerminal => self
                .terminal
                .as_ref()
                .map(|terminal| terminal.read(cx).quote_focus_handle()),
            QuoteSurface::AuxiliaryTerminal => self
                .auxiliary_terminal
                .as_ref()
                .map(|terminal| terminal.read(cx).quote_focus_handle()),
            QuoteSurface::Inspector => self
                .inspector
                .as_ref()
                .map(|inspector| inspector.read(cx).focus_handle(cx)),
        };
        if let Some(handle) = handle {
            window.focus(&handle, cx);
        }
    }

    fn selected_quote(&self, window: &Window, cx: &App) -> Option<Quote> {
        if let Some(surface) = self.focused_quote_surface(window, cx) {
            return self.quote_from_surface(surface, cx);
        }
        // Palette execution temporarily owns focus. Preserve the visible
        // source surface rather than making Quote Selection palette-only fail.
        self.quote_from_surface(self.last_quote_surface, cx)
            .or_else(|| {
                self.terminal
                    .as_ref()
                    .and_then(|terminal| terminal.read(cx).quote_selection())
            })
    }

    fn quote_targets(&self) -> Vec<SessionRecord> {
        self.services
            .store
            .store
            .write()
            .expect("session store lock poisoned")
            .ordered_sessions()
            .into_iter()
            .filter(is_quote_target)
            .collect()
    }

    fn quote_selection(&mut self, pick_target: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(quote) = self.selected_quote(window, cx) else {
            self.show_quote_feedback(
                "Nothing selected",
                "Select terminal text, a diff hunk or line range, or a Markdown turn first.",
                cx,
            );
            return;
        };
        if pick_target {
            let return_surface = self
                .focused_quote_surface(window, cx)
                .unwrap_or(self.last_quote_surface);
            let targets = self.quote_targets();
            if targets.is_empty() {
                self.show_quote_feedback(
                    "No target session",
                    "Start an agent to stage this quote.",
                    cx,
                );
                return;
            }
            let active = self
                .services
                .store
                .store
                .read()
                .expect("session store lock poisoned")
                .selected_session_id()
                .cloned();
            let highlighted = active
                .as_ref()
                .and_then(|id| targets.iter().position(|session| &session.id == id))
                .unwrap_or(0);
            self.sidebar.update(cx, |sidebar, cx| sidebar.reveal(cx));
            self.quote_target_picker = Some(QuoteTargetPicker {
                quote,
                targets,
                highlighted,
                return_surface,
            });
            window.focus(&self.focus, cx);
            cx.notify();
            return;
        }
        let target = self
            .services
            .store
            .store
            .read()
            .expect("session store lock poisoned")
            .selected_session_id()
            .cloned();
        let Some(target) = target else {
            self.show_quote_feedback(
                "No active session",
                "Select an agent to receive the quote.",
                cx,
            );
            return;
        };
        if !self
            .quote_targets()
            .iter()
            .any(|session| session.id == target)
        {
            self.show_quote_feedback(
                "Active session unavailable",
                "Choose a running or sleeping agent—not a shell—as the quote target.",
                cx,
            );
            return;
        }
        self.open_quote_draft(target, quote, window, cx);
    }

    fn open_quote_draft(
        &mut self,
        target: SessionId,
        quote: Quote,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let target_record = self
            .services
            .store
            .store
            .read()
            .expect("session store lock poisoned")
            .sessions()
            .get(&target)
            .cloned();
        let Some(target_record) = target_record else {
            self.show_quote_feedback("Target unavailable", "That session no longer exists.", cx);
            return;
        };
        if !is_quote_target(&target_record) {
            self.show_quote_feedback(
                "Target unavailable",
                "Quotes can be staged only in an agent draft, not a shell.",
                cx,
            );
            return;
        }
        let text = quote.framed();
        self.launcher.update(cx, |launcher, cx| {
            launcher.open_for_session(target, &text, None, window, cx);
        });
        // Mount the app-owned composer before focusing its insertion caret.
        // This changes no sidebar/session selection and does not touch the PTY.
        let launcher = self.launcher.clone();
        cx.defer_in(window, move |_, window, cx| {
            launcher.update(cx, |launcher, cx| launcher.focus(window, cx));
        });
    }

    fn activate_quote_target(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(picker) = self.quote_target_picker.take() else {
            return;
        };
        // Resolve against the snapshot shown to the user. A concurrent store
        // reorder must never redirect a click to a different session.
        let Some(target) = quote_target_id(&picker.targets, index) else {
            self.show_quote_feedback("Target unavailable", "Choose another session.", cx);
            return;
        };
        self.open_quote_draft(target, picker.quote, window, cx);
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.notification_panel_open && self.notification_key(event, window, cx) {
            return;
        }
        // A sidebar drag rarely has sidebar focus (the press left it in the
        // terminal), so Escape is caught here, on the window's capture path.
        if event.keystroke.key == "escape"
            && self
                .sidebar
                .update(cx, |sidebar, cx| sidebar.cancel_active_drag(cx))
        {
            cx.stop_propagation();
            return;
        }
        if self.quote_target_picker.is_some() {
            let target_count = self
                .quote_target_picker
                .as_ref()
                .map_or(0, |picker| picker.targets.len());
            match event.keystroke.key.as_str() {
                "escape" => {
                    if let Some(picker) = self.quote_target_picker.take() {
                        self.restore_quote_focus(picker.return_surface, window, cx);
                    }
                    cx.notify();
                }
                "up" if target_count > 0 => {
                    let picker = self.quote_target_picker.as_mut().expect("picker exists");
                    picker.highlighted = picker
                        .highlighted
                        .checked_sub(1)
                        .unwrap_or(target_count - 1);
                    cx.notify();
                }
                "down" if target_count > 0 => {
                    let picker = self.quote_target_picker.as_mut().expect("picker exists");
                    picker.highlighted = (picker.highlighted + 1) % target_count;
                    cx.notify();
                }
                "enter" if target_count > 0 => {
                    let highlighted = self
                        .quote_target_picker
                        .as_ref()
                        .expect("picker exists")
                        .highlighted;
                    self.activate_quote_target(highlighted, window, cx);
                }
                _ => {}
            }
            cx.stop_propagation();
            return;
        }
        if self.inspector_open
            && !self.launcher.read(cx).is_open()
            && !self
                .navigation
                .as_ref()
                .is_some_and(|view| view.read(cx).is_open())
            && !self
                .utility_surfaces
                .as_ref()
                .is_some_and(|view| view.read(cx).is_open())
            && let Some(inspector) = &self.inspector
            && inspector.update(cx, |inspector, cx| {
                inspector.browser_shortcut(event, window, cx)
            })
        {
            cx.stop_propagation();
            return;
        }
        // The sidebar is a real keyboard surface. Let its bubble handler own
        // navigation and rename input instead of mirroring the same keystroke
        // into the live terminal during root capture.
        if self.sidebar.read(cx).is_focused(window) {
            return;
        }
        if self
            .navigation
            .as_ref()
            .is_some_and(|navigation| navigation.read(cx).is_open())
        {
            // The focused palette owns input even over the composer or Settings.
            return;
        }
        if self.launcher.read(cx).is_open() {
            let reopen = commands::matches_keystroke(CommandId::OpenLauncher, &event.keystroke);
            let focus_sidebar =
                commands::matches_keystroke(CommandId::FocusSidebar, &event.keystroke);
            if !focus_sidebar {
                self.launcher.update(cx, |launcher, cx| {
                    launcher.handle_key_down(event, window, cx);
                });
            }
            if !reopen && !focus_sidebar {
                cx.stop_propagation();
            }
            return;
        }
        if let Some(surfaces) = &self.utility_surfaces
            && surfaces.read(cx).is_open()
        {
            let global_overlay_command = [
                CommandId::ToggleHistory,
                CommandId::OpenSettings,
                CommandId::ToggleCommandPalette,
                CommandId::ToggleQuickOpen,
                CommandId::FocusSidebar,
            ]
            .into_iter()
            .any(|command| commands::matches_keystroke(command, &event.keystroke));
            if !global_overlay_command {
                surfaces.update(cx, |surfaces, cx| {
                    surfaces.key_down(event, window, cx);
                });
                cx.stop_propagation();
                return;
            }
        }
        if let Some(surfaces) = &self.session_surfaces {
            surfaces.update(cx, |surfaces, cx| {
                surfaces.handle_key_down(event, window, cx);
            });
        }
    }

    /// Executes application commands after GPUI has resolved the active key
    /// context. This is the only place that translates static commands into
    /// mutations of RootView's child modules.
    fn run_command(&mut self, command: CommandId, window: &mut Window, cx: &mut Context<Self>) {
        match command {
            // A spawn the catalog vetoes falls back to the launcher, where the
            // unavailability is visible and another Agent is one keystroke
            // away, instead of a shortcut that silently does nothing.
            CommandId::NewDefaultSession => {
                if !self.spawn_default() {
                    self.open_launcher(&OpenLauncher, window, cx);
                }
            }
            CommandId::NewTerminal => {
                self.spawn(None);
            }
            CommandId::NewCodexSession => {
                if !self.spawn(Some(AgentKind::CODEX)) {
                    self.open_launcher(&OpenLauncher, window, cx);
                }
            }
            CommandId::ToggleCommandPalette => {
                self.remember_quote_surface(window, cx);
                if let Some(navigation) = &self.navigation {
                    navigation.update(cx, |navigation, cx| {
                        navigation.toggle_command_palette(&ToggleCommandPalette, window, cx);
                    });
                }
            }
            CommandId::ToggleQuickOpen => {
                if let Some(navigation) = &self.navigation {
                    navigation.update(cx, |navigation, cx| {
                        navigation.toggle_quick_open(&ToggleQuickOpen, window, cx);
                    });
                }
            }
            CommandId::ToggleHistory => {
                if let Some(navigation) = &self.navigation {
                    navigation.update(cx, |navigation, cx| {
                        navigation.toggle_history(&ToggleHistory, window, cx)
                    });
                }
            }
            CommandId::ToggleOverview => {
                if let Some(surfaces) = &self.session_surfaces {
                    surfaces.update(cx, |surfaces, cx| surfaces.toggle_overview(cx));
                }
            }
            CommandId::OpenWorktrees => {
                if let Some(navigation) = &self.navigation {
                    navigation.update(cx, |navigation, cx| navigation.dismiss(cx));
                }
                if let Some(surfaces) = &self.utility_surfaces {
                    surfaces.update(cx, |surfaces, cx| surfaces.open_worktrees(cx));
                }
            }
            CommandId::OpenSettings => {
                if let Some(navigation) = &self.navigation {
                    navigation.update(cx, |navigation, cx| navigation.dismiss(cx));
                }
                if let Some(surfaces) = &self.utility_surfaces {
                    surfaces.update(cx, |surfaces, cx| surfaces.toggle_settings(cx));
                }
            }
            CommandId::ToggleSidebar => {
                self.sidebar.update(cx, |sidebar, cx| sidebar.toggle(cx));
            }
            CommandId::FocusSidebar => {
                if self.launcher.read(cx).is_open() {
                    self.launcher
                        .update(cx, |launcher, cx| launcher.dismiss(cx));
                }
                if let Some(navigation) = &self.navigation {
                    navigation.update(cx, |navigation, cx| navigation.dismiss(cx));
                }
                if let Some(surfaces) = &self.utility_surfaces {
                    surfaces.update(cx, |surfaces, cx| surfaces.dismiss(cx));
                }
                if let Some(surfaces) = &self.session_surfaces {
                    surfaces.update(cx, |surfaces, cx| surfaces.dismiss(cx));
                }
                self.sidebar
                    .update(cx, |sidebar, cx| sidebar.focus(window, cx));
            }
            CommandId::ToggleInspector => {
                self.toggle_inspector(cx);
                if !self.inspector_open {
                    window.focus(&self.focus, cx);
                }
            }
            CommandId::ToggleAuxiliaryTerminal => {
                self.open_auxiliary_terminal(window, cx);
            }
            CommandId::QuoteSelection => self.quote_selection(false, window, cx),
            CommandId::QuoteSelectionToSession => self.quote_selection(true, window, cx),
            CommandId::ArchiveSelectedSession => {
                self.sidebar
                    .update(cx, |sidebar, cx| sidebar.archive_selected(cx));
            }
            CommandId::RenameSelectedSession => {
                self.sidebar
                    .update(cx, |sidebar, cx| sidebar.rename_selected(window, cx));
            }
            CommandId::DelegateSelectedSession => {
                let handled = self
                    .sidebar
                    .update(cx, |sidebar, cx| sidebar.mark_or_delegate_selected(cx));
                if !handled {
                    cx.propagate();
                }
            }
            CommandId::ToggleNotifications => self.toggle_notifications(window, cx),
            CommandId::SelectNextAttentionSession => {
                self.sidebar
                    .update(cx, |sidebar, cx| sidebar.select_next_needing_input(cx));
            }
            CommandId::CheckForUpdates => self.services.updates.check(true),
            CommandId::SelectPreviousSession if !self.arrow_surface_visible() => {
                self.sidebar
                    .update(cx, |sidebar, cx| sidebar.select_relative(-1, cx));
            }
            CommandId::SelectNextSession if !self.arrow_surface_visible() => {
                self.sidebar
                    .update(cx, |sidebar, cx| sidebar.select_relative(1, cx));
            }
            CommandId::MoveSelectedSessionUp if !self.arrow_surface_visible() => {
                self.sidebar
                    .update(cx, |sidebar, cx| sidebar.reorder_selected(-1, cx));
            }
            CommandId::MoveSelectedSessionDown if !self.arrow_surface_visible() => {
                self.sidebar
                    .update(cx, |sidebar, cx| sidebar.reorder_selected(1, cx));
            }
            CommandId::SelectSession1 => self.select_session_shortcut(0, cx),
            CommandId::SelectSession2 => self.select_session_shortcut(1, cx),
            CommandId::SelectSession3 => self.select_session_shortcut(2, cx),
            CommandId::SelectSession4 => self.select_session_shortcut(3, cx),
            CommandId::SelectSession5 => self.select_session_shortcut(4, cx),
            CommandId::SelectSession6 => self.select_session_shortcut(5, cx),
            CommandId::SelectSession7 => self.select_session_shortcut(6, cx),
            CommandId::SelectSession8 => self.select_session_shortcut(7, cx),
            CommandId::SelectLastSession => {
                self.sidebar
                    .update(cx, |sidebar, cx| sidebar.select_last(cx));
            }
            _ => cx.propagate(),
        }
    }

    fn select_session_shortcut(&mut self, index: usize, cx: &mut Context<Self>) {
        self.sidebar
            .update(cx, |sidebar, cx| sidebar.select_shortcut(index, cx));
    }

    /// Spawns a shell (`None`) or a specific agent straight from a shortcut,
    /// bypassing the sidebar's picker. No-ops in preview, which has no daemon
    /// to spawn into. Reports whether the spawn was dispatched.
    fn spawn(&self, agent: Option<AgentKind>) -> bool {
        if self.preview {
            return false;
        }
        let mut store = self
            .services
            .store
            .store
            .write()
            .expect("session store lock poisoned");
        match agent {
            Some(agent) => {
                let host = store.default_spawn_host();
                if !crate::agent_catalog::kind_spawnable(
                    &agent,
                    store.agent_catalog(host.as_deref()),
                ) {
                    store.request_agent_catalog(host, false);
                    return false;
                }
                store.spawn_kind(
                    agent,
                    SpawnOptions {
                        host,
                        ..SpawnOptions::default()
                    },
                );
            }
            None => store.spawn_shell(SpawnOptions::default()),
        }
        true
    }

    fn spawn_default(&self) -> bool {
        if self.preview {
            return false;
        }
        let mut store = self
            .services
            .store
            .store
            .write()
            .expect("session store lock poisoned");
        let host = store.default_spawn_host();
        store.spawn_default(SpawnOptions {
            host,
            ..SpawnOptions::default()
        })
    }

    fn open_auxiliary_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.preview {
            return false;
        }
        if let Some(inspector) = &self.inspector
            && inspector.read(cx).workspace_needs_terminal()
        {
            inspector.update(cx, |inspector, cx| {
                inspector.select_workspace(crate::inspector::WorkspaceSurface::Terminal, cx);
            });
            self.reveal_inspector(cx);
            return self.ensure_auxiliary_terminal(window, cx);
        }
        self.sync_auxiliary_terminal(window, cx);
        if self.auxiliary_terminal.is_some() {
            self.hide_auxiliary_terminal(window, cx);
            true
        } else {
            self.ensure_auxiliary_terminal(window, cx)
        }
    }

    /// Tab activation is idempotent: it never toggles an already-open shell.
    fn ensure_auxiliary_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.preview {
            return false;
        }
        let selected = self
            .services
            .store
            .store
            .read()
            .expect("session store lock poisoned")
            .selected_session_id()
            .cloned();
        let Some(parent) = selected else {
            return false;
        };
        self.collapsed_auxiliary_parents.remove(&parent);
        self.sync_auxiliary_terminal(window, cx);
        if let Some(terminal) = &self.auxiliary_terminal {
            terminal.update(cx, |terminal, cx| terminal.focus(window, cx));
            return true;
        }
        let slot = self
            .inspector
            .as_ref()
            .map_or(0, |inspector| inspector.read(cx).terminal_slot());
        let spawned = {
            let mut store = self
                .services
                .store
                .store
                .write()
                .expect("session store lock poisoned");
            if slot == 0 {
                store.spawn_auxiliary_terminal(parent.clone())
            } else {
                store.spawn_auxiliary_terminal_slot(parent.clone(), slot)
            }
        };
        if spawned {
            self.auxiliary_spawn_parent = Some(parent);
            cx.notify();
        }
        spawned
    }

    /// Hide the pane without starting or stopping the Engine-owned child shell.
    fn hide_auxiliary_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(parent) = self
            .services
            .store
            .store
            .read()
            .expect("session store lock poisoned")
            .selected_session_id()
            .cloned()
        {
            self.collapsed_auxiliary_parents.insert(parent);
        }
        self.auxiliary_terminal = None;
        self.auxiliary_id = None;
        self.auxiliary_parent = None;
        self.auxiliary_spawn_parent = None;
        if let Some(inspector) = &self.inspector {
            inspector.update(cx, |inspector, cx| inspector.set_terminal_surface(None, cx));
        }
        if let Some(primary) = &self.terminal {
            primary.update(cx, |terminal, cx| terminal.focus(window, cx));
        }
        cx.notify();
    }

    /// Reconciles the UI-owned pane entity with the daemon-owned child shell.
    /// The relationship survives app restarts because it lives in the session
    /// record; the GPUI entity remains disposable rendering state.
    fn sync_auxiliary_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.preview {
            return;
        }
        let slot = self
            .inspector
            .as_ref()
            .map_or(0, |inspector| inspector.read(cx).terminal_slot());
        let (selected, auxiliary, spawn_pending) = {
            let mut store = self
                .services
                .store
                .store
                .write()
                .expect("session store lock poisoned");
            let selected = store.selected_session_id().cloned();
            let auxiliary = selected
                .as_ref()
                .and_then(|parent| store.auxiliary_terminal_for_slot(parent, slot));
            let pending = selected
                .as_ref()
                .is_some_and(|parent| store.auxiliary_spawn_pending(parent, slot));
            (selected, auxiliary, pending)
        };

        if selected
            .as_ref()
            .is_some_and(|parent| self.collapsed_auxiliary_parents.contains(parent))
        {
            // Collapsing a pane is UI-only: keep its daemon shell alive so
            // the next ⌘J restores the same scrollback and process state.
            self.auxiliary_terminal = None;
            self.auxiliary_id = None;
            self.auxiliary_parent = None;
            if let Some(inspector) = &self.inspector {
                inspector.update(cx, |inspector, cx| inspector.set_terminal_surface(None, cx));
            }
            return;
        }

        if let Some(session) = auxiliary {
            let parent = session
                .parent
                .clone()
                .expect("auxiliary terminal has an owning session");
            if self.auxiliary_id.as_ref() == Some(&session.id)
                && self.auxiliary_parent.as_ref() == Some(&parent)
            {
                self.auxiliary_spawn_parent = None;
                return;
            }

            let runtime = Arc::clone(&self.services.store);
            let tokio = Arc::clone(&self.services.tokio);
            let id = session.id.clone();
            let terminal =
                cx.new(|cx| TerminalPane::new_fixed(runtime, tokio, id.clone(), window, cx));
            if let (Some(navigation), Some(utility_surfaces)) =
                (&self.navigation, &self.utility_surfaces)
            {
                terminal.update(cx, |terminal, _| {
                    terminal.set_shell_entities(navigation.clone(), utility_surfaces.clone());
                });
            }
            let should_focus = self.auxiliary_spawn_parent.as_ref() == Some(&parent);
            self.auxiliary_id = Some(session.id.clone());
            self.auxiliary_parent = Some(parent);
            self.auxiliary_terminal = Some(terminal.clone());
            self.auxiliary_spawn_parent = None;
            if let Some(inspector) = &self.inspector {
                let terminal = terminal.clone();
                inspector.update(cx, |inspector, cx| {
                    inspector.set_terminal_surface(Some(terminal), cx)
                });
            }
            if should_focus {
                terminal.update(cx, |terminal, cx| terminal.focus(window, cx));
            }
            cx.notify();
            return;
        }

        let had_auxiliary_state = self.auxiliary_terminal.is_some()
            || self.auxiliary_id.is_some()
            || self.auxiliary_parent.is_some()
            || self.auxiliary_spawn_parent.is_some();
        self.auxiliary_terminal = None;
        self.auxiliary_id = None;
        self.auxiliary_parent = None;
        self.auxiliary_spawn_parent = None;
        if let Some(inspector) = &self.inspector {
            inspector.update(cx, |inspector, cx| inspector.set_terminal_surface(None, cx));
        }
        if spawn_pending {
            self.auxiliary_spawn_parent = selected;
        }
        if had_auxiliary_state {
            cx.notify();
        }
    }

    /// True while the ⌃Tab switcher or the overview is up: both drive their
    /// own arrow-key navigation, so ⌘↑/⌘↓ stays out of their way.
    fn arrow_surface_visible(&self) -> bool {
        let store = self
            .services
            .store
            .store
            .read()
            .expect("session store lock poisoned");
        store.switcher_state().is_visible() || store.overview_state().is_visible()
    }

    /// Cmd+W: close the selected session with the sidebar ✕ semantics.
    /// With no session selected the action propagates to the global
    /// handler in main.rs, which closes the window instead.
    fn close_selected_session(
        &mut self,
        _: &CloseSession,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .auxiliary_terminal
            .as_ref()
            .is_some_and(|terminal| terminal.read(cx).is_focused(window))
            && let Some(id) = self.auxiliary_id.clone()
        {
            self.services
                .store
                .store
                .write()
                .expect("session store lock poisoned")
                .remove_sessions(vec![id]);
            if let Some(terminal) = &self.terminal {
                terminal.update(cx, |terminal, cx| terminal.focus(window, cx));
            }
            return;
        }
        let closed = self
            .sidebar
            .update(cx, |sidebar, cx| sidebar.close_selected_now(cx));
        if !closed {
            cx.propagate();
        }
    }

    /// Cmd+Shift+T: reopen the most recently closed session (daemon-backed,
    /// survives restarts).
    fn reopen_last_session(
        &mut self,
        _: &ReopenSession,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sidebar
            .update(cx, |sidebar, cx| sidebar.reopen_last(cx));
    }

    fn open_launcher(&mut self, _: &OpenLauncher, window: &mut Window, cx: &mut Context<Self>) {
        self.launcher
            .update(cx, |launcher, cx| launcher.open(window, cx));
        // Opening changes which main-pane branch RootView renders.
        cx.notify();
        // The launcher was not mounted while the terminal branch was active.
        // Focus it on the next frame, after GPUI has installed its focus node.
        let launcher = self.launcher.clone();
        cx.defer_in(window, move |_, window, cx| {
            launcher.update(cx, |launcher, cx| launcher.focus(window, cx));
        });
    }

    fn toggle_launcher(&mut self, _: &OpenLauncher, window: &mut Window, cx: &mut Context<Self>) {
        let opens = self
            .launcher
            .update(cx, |launcher, cx| launcher.toggle(window, cx));
        cx.notify();
        if !opens {
            return;
        }
        let launcher = self.launcher.clone();
        cx.defer_in(window, move |_, window, cx| {
            launcher.update(cx, |launcher, cx| launcher.focus(window, cx));
        });
    }

    fn on_key_up(&mut self, event: &KeyUpEvent, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(surfaces) = &self.session_surfaces {
            surfaces.update(cx, |surfaces, cx| {
                surfaces.handle_key_up(event, window, cx);
            });
        }
    }

    fn on_modifiers_changed(
        &mut self,
        event: &ModifiersChangedEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(surfaces) = &self.session_surfaces {
            surfaces.update(cx, |surfaces, cx| {
                surfaces.handle_modifiers_changed(event, window, cx);
            });
        }
    }

    /// The settled seam width: what the sidebar wrapper is worth once nothing
    /// is animating. This -- not the painted seam -- is what the terminal is
    /// told about, so the PTY hears one resize per toggle rather than one per
    /// animation frame.
    fn settled_sidebar_seam(&self, cx: &App) -> f32 {
        let sidebar = self.sidebar.read(cx);
        if sidebar.is_visible() {
            sidebar.width()
        } else {
            0.0
        }
    }

    /// Starts sliding the seam toward the visibility the sidebar just adopted.
    /// Reduced-motion users get the settled width immediately.
    fn begin_sidebar_slide(&mut self, cx: &mut Context<Self>) {
        let to = self.settled_sidebar_seam(cx);
        let now = Instant::now();
        self.sidebar_slide = (!cx.reduce_motion())
            .then(|| SeamSlide::begin_at(self.sidebar_seam, to, now))
            .flatten();
        if self.sidebar_slide.is_none() {
            self.sidebar_seam = to;
        }
        self.begin_sidebar_panel_slide(now, cx);
    }

    fn begin_sidebar_panel_slide(&mut self, now: Instant, cx: &mut Context<Self>) {
        let sidebar = self.sidebar.read(cx);
        let to = if sidebar.is_visible() || sidebar.is_peeking() {
            sidebar.width()
        } else {
            0.0
        };
        let float_to = if self.sidebar_floating { 1.0 } else { 0.0 };
        self.sidebar_panel_slide = (!cx.reduce_motion())
            .then(|| SeamSlide::begin_at(self.sidebar_panel_width, to, now))
            .flatten()
            .map(|slide| {
                if sidebar.is_peeking() {
                    slide.with_duration(SIDEBAR_PEEK_REVEAL)
                } else {
                    slide
                }
            });
        self.sidebar_float_slide = (!cx.reduce_motion())
            .then(|| SeamSlide::begin_at(self.sidebar_float, float_to, now))
            .flatten();
        if self.sidebar_panel_slide.is_none() {
            self.sidebar_panel_width = to;
        }
        if self.sidebar_float_slide.is_none() {
            self.sidebar_float = float_to;
        }
    }

    /// The inspector's settled seam. Like the sidebar's, this is what the
    /// terminal is told about, so a slide costs no PTY resizes.
    fn settled_inspector_seam(&self) -> f32 {
        if self.inspector_open {
            self.inspector_width.min(self.inspector_max_width)
        } else {
            0.0
        }
    }

    fn begin_inspector_slide(&mut self, cx: &mut Context<Self>) {
        let to = self.settled_inspector_seam();
        self.inspector_slide = (!cx.reduce_motion())
            .then(|| SeamSlide::begin(self.inspector_seam, to))
            .flatten();
        if self.inspector_slide.is_none() {
            self.inspector_seam = to;
        }
    }

    /// The grab strip that straddles the sidebar/terminal seam.
    ///
    /// Two things make this reliable, and both are easy to lose:
    ///  - `deferred` + `occlude` put the strip above the terminal card, which
    ///    is a later sibling and would otherwise win the hit test on the half
    ///    of the strip that overhangs it.
    ///  - the drag is tracked with `on_drag`/`on_drag_move` (see `RootView::
    ///    render`) rather than `on_mouse_move`, because plain move listeners
    ///    only fire while the hitbox is hovered -- so any pointer motion that
    ///    outran the 9px strip silently dropped the resize.
    fn resize_handle(&self, cx: &mut Context<Self>) -> AnyElement {
        div()
            .relative()
            .flex_none()
            .w(px(0.0))
            .h_full()
            .child(deferred(
                div()
                    .id("sidebar-resize-handle")
                    .absolute()
                    .left(px(-4.5))
                    .top(px(0.0))
                    .w(px(9.0))
                    .h_full()
                    .cursor(CursorStyle::ResizeLeftRight)
                    .group("sidebar-resize")
                    .child(
                        div()
                            .absolute()
                            .left(px(3.5))
                            .top_0()
                            .w(px(2.0))
                            .h_full()
                            .bg(if self.resize_origin.is_some() {
                                rgba(0x4f83f1ff)
                            } else {
                                rgba(0x4f83f100)
                            })
                            .group_hover("sidebar-resize", |line| line.bg(rgba(0x4f83f1ff))),
                    )
                    .occlude()
                    .on_drag(DraggedSidebarEdge, |edge, _, _, cx| {
                        cx.stop_propagation();
                        cx.new(|_| *edge)
                    })
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, event: &gpui::MouseDownEvent, _, cx| {
                            let width = this.sidebar.read(cx).width();
                            this.resize_origin = Some((f32::from(event.position.x), width));
                            cx.notify();
                            cx.stop_propagation();
                        }),
                    )
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(|this, event: &gpui::MouseUpEvent, _, cx| {
                            if event.click_count == 2 {
                                this.sidebar
                                    .update(cx, |sidebar, cx| sidebar.reset_width(cx));
                                cx.stop_propagation();
                            }
                            this.finish_resize(cx);
                        }),
                    )
                    .on_mouse_up_out(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| this.finish_resize(cx)),
                    ),
            ))
            .into_any_element()
    }

    fn terminal_resize_handle(&self, cx: &mut Context<Self>) -> AnyElement {
        let line = rgba(0xffffff18);
        div()
            .relative()
            .flex_none()
            .h(px(1.0))
            .w_full()
            .bg(line)
            .child(deferred(
                div()
                    .id("terminal-resize-handle")
                    .absolute()
                    .top(px(-4.0))
                    .left(px(0.0))
                    .h(px(9.0))
                    .w_full()
                    .cursor(CursorStyle::ResizeUpDown)
                    .group("terminal-resize")
                    .child(
                        div()
                            .absolute()
                            .top(px(3.5))
                            .left_0()
                            .h(px(2.0))
                            .w_full()
                            .bg(if self.terminal_resize_origin.is_some() {
                                rgba(0x4f83f1ff)
                            } else {
                                rgba(0x4f83f100)
                            })
                            .group_hover("terminal-resize", |line| line.bg(rgba(0x4f83f1ff))),
                    )
                    .occlude()
                    .on_drag(DraggedTerminalEdge, |edge, _, _, cx| {
                        cx.stop_propagation();
                        cx.new(|_| *edge)
                    })
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, event: &gpui::MouseDownEvent, _, cx| {
                            let primary = this
                                .workbench_layout
                                .pane_heights(this.terminal_available_height)
                                .primary;
                            this.terminal_resize_origin =
                                Some((f32::from(event.position.y), primary));
                            cx.notify();
                            cx.stop_propagation();
                        }),
                    )
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(|this, event: &gpui::MouseUpEvent, _, cx| {
                            if event.click_count == 2 {
                                this.workbench_layout.reset();
                            }
                            this.finish_terminal_resize(cx);
                        }),
                    )
                    .on_mouse_up_out(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| this.finish_terminal_resize(cx)),
                    ),
            ))
            .into_any_element()
    }

    fn drag_resize(&mut self, pointer_x: f32, cx: &mut Context<Self>) {
        let Some((origin_x, base_width)) = self.resize_origin else {
            return;
        };
        let width = base_width + pointer_x - origin_x;
        self.sidebar
            .update(cx, |sidebar, cx| sidebar.set_width(width, cx));
    }

    fn drag_terminal_resize(&mut self, pointer_y: f32, cx: &mut Context<Self>) {
        let Some((origin_y, base_height)) = self.terminal_resize_origin else {
            return;
        };
        let previous = self.workbench_layout;
        self.workbench_layout.resize_primary(
            base_height + pointer_y - origin_y,
            self.terminal_available_height,
        );
        if self.workbench_layout != previous {
            cx.notify();
        }
    }

    fn finish_terminal_resize(&mut self, cx: &mut Context<Self>) {
        if self.terminal_resize_origin.take().is_none() {
            return;
        }
        let fraction = self.workbench_layout.primary_fraction();
        if let Err(error) = self
            .services
            .store
            .store
            .write()
            .expect("session store lock poisoned")
            .update_preferences(|prefs| prefs.workbench_primary_fraction = fraction)
        {
            eprintln!("diri: could not remember workbench split: {error}");
        }
        cx.notify();
    }

    /// End of a resize drag: the live width only lived in the sidebar's UI
    /// state, so write it through to preferences now.
    fn finish_resize(&mut self, cx: &mut Context<Self>) {
        if self.resize_origin.take().is_some() {
            self.sidebar
                .update(cx, |sidebar, cx| sidebar.commit_width(cx));
            // Width persistence does not notify the sidebar. Retire the drag
            // shield now, even when the last motion was beyond the clamp.
            cx.notify();
        }
    }

    /// The single gate every inspector open and close passes through -- ⌘⇧D,
    /// the terminal chrome button, and the panel's own ✕ -- so the debounce
    /// only has to hold here.
    fn set_inspector_open(&mut self, open: bool, cx: &mut Context<Self>) {
        if self.preview || self.inspector_open == open {
            return;
        }
        let now = Instant::now();
        if !toggle_has_settled(self.inspector_toggled_at.map(|at| now.duration_since(at))) {
            return;
        }
        self.inspector_toggled_at = Some(now);
        self.inspector_open = open;
        if let Some(inspector) = &self.inspector {
            inspector.update(cx, |inspector, cx| inspector.set_visible(open, cx));
        }
        if let Err(error) = self
            .services
            .store
            .store
            .write()
            .expect("session store lock poisoned")
            .update_preferences(|prefs| prefs.inspector_open = open)
        {
            eprintln!("diri: could not remember inspector visibility: {error}");
        }
        self.begin_inspector_slide(cx);
        cx.notify();
    }

    fn toggle_inspector(&mut self, cx: &mut Context<Self>) {
        self.set_inspector_open(!self.inspector_open, cx);
    }

    /// Source navigation is an explicit destination, so it must not be lost
    /// behind the short debounce that protects repeated panel toggles.
    fn reveal_inspector(&mut self, cx: &mut Context<Self>) {
        self.inspector_toggled_at = None;
        self.set_inspector_open(true, cx);
    }

    fn inspector_resize_handle(&self, cx: &mut Context<Self>) -> AnyElement {
        div()
            .relative()
            .flex_none()
            .w(px(0.0))
            .h_full()
            .child(deferred(
                div()
                    .id("inspector-resize-handle")
                    .absolute()
                    .left(px(-4.5))
                    .top(px(0.0))
                    .w(px(9.0))
                    .h_full()
                    .cursor(CursorStyle::ResizeLeftRight)
                    .group("inspector-resize")
                    .child(
                        div()
                            .absolute()
                            .left(px(3.5))
                            .top_0()
                            .w(px(2.0))
                            .h_full()
                            .bg(if self.inspector_resize_origin.is_some() {
                                rgba(0x4f83f1ff)
                            } else {
                                rgba(0x4f83f100)
                            })
                            .group_hover("inspector-resize", |line| line.bg(rgba(0x4f83f1ff))),
                    )
                    .occlude()
                    .on_drag(DraggedInspectorEdge, |edge, _, _, cx| {
                        cx.stop_propagation();
                        cx.new(|_| *edge)
                    })
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, event: &gpui::MouseDownEvent, _, cx| {
                            this.inspector_resize_origin =
                                Some((f32::from(event.position.x), this.inspector_width));
                            cx.notify();
                            cx.stop_propagation();
                        }),
                    )
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(|this, event: &gpui::MouseUpEvent, _, cx| {
                            if event.click_count == 2 {
                                this.inspector_width = 440.0_f32.min(this.inspector_max_width);
                            }
                            this.finish_inspector_resize(cx);
                        }),
                    )
                    .on_mouse_up_out(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| this.finish_inspector_resize(cx)),
                    ),
            ))
            .into_any_element()
    }

    fn drag_inspector_resize(&mut self, pointer_x: f32, cx: &mut Context<Self>) {
        let Some((origin_x, base_width)) = self.inspector_resize_origin else {
            return;
        };
        let width = (base_width - pointer_x + origin_x).clamp(
            300.0_f32.min(self.inspector_max_width),
            self.inspector_max_width,
        );
        if self.inspector_width == width {
            return;
        }
        self.inspector_width = width;
        cx.notify();
    }

    fn finish_inspector_resize(&mut self, cx: &mut Context<Self>) {
        if self.inspector_resize_origin.take().is_none() {
            return;
        }
        let width = self.inspector_width;
        if let Err(error) = self
            .services
            .store
            .store
            .write()
            .expect("session store lock poisoned")
            .update_preferences(|prefs| prefs.inspector_width = width)
        {
            eprintln!("diri: could not remember inspector width: {error}");
        }
        cx.notify();
    }

    /// While a resize drag is active, keep pointer motion from reaching the
    /// terminal's selection layer. The drag payload still routes to RootView,
    /// while this transparent hitbox owns everything underneath it.
    fn resize_shield(&self, cx: &mut Context<Self>) -> AnyElement {
        let vertical = self.terminal_resize_origin.is_some();
        deferred(
            div()
                .id("active-resize-shield")
                .absolute()
                .inset_0()
                .cursor(if vertical {
                    CursorStyle::ResizeUpDown
                } else {
                    CursorStyle::ResizeLeftRight
                })
                .occlude()
                .on_mouse_up(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| {
                        this.finish_resize(cx);
                        this.finish_terminal_resize(cx);
                        this.finish_inspector_resize(cx);
                        cx.stop_propagation();
                    }),
                )
                .on_mouse_up_out(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| {
                        this.finish_resize(cx);
                        this.finish_terminal_resize(cx);
                        this.finish_inspector_resize(cx);
                    }),
                ),
        )
        .into_any_element()
    }

    #[cfg(target_os = "macos")]
    fn browser_visible(&self, launcher_open: bool, panel_width: f32, cx: &App) -> bool {
        self.browser.borrow().has_page()
            && panel_width > 1.0
            && !launcher_open
            && !self
                .utility_surfaces
                .as_ref()
                .is_some_and(|view| view.read(cx).is_open())
            && !self
                .navigation
                .as_ref()
                .is_some_and(|view| view.read(cx).is_open())
            && !self.arrow_surface_visible()
            && self.quote_target_picker.is_none()
            && self.sidebar.read(cx).pending_close_copy().is_none()
            && self.inspector_open
            && self.inspector_seam >= panel_width - 0.5
            && self.inspector.as_ref().is_some_and(|view| {
                let inspector = view.read(cx);
                inspector.is_browser_tab() && !inspector.blocks_native_browser()
            })
    }

    /// `visible_sidebar` and `inspector_width` are the settled layout and drive
    /// everything the terminal is *told* -- viewport geometry, and whether its
    /// chrome offers a "show sidebar" button. The two `*_seam` widths are what
    /// is being painted this frame and drive only the card's own top corners,
    /// so each radius appears the moment its panel finishes clearing rather
    /// than at the start of the slide. Keeping the two apart is what stops a
    /// 260ms slide from firing a PTY resize on every frame of it.
    fn terminal_card(
        &mut self,
        visible_sidebar: bool,
        seam: f32,
        inspector_width: f32,
        inspector_seam: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let terminal = self.colors();
        let bounds = window.inner_window_bounds().get_bounds();
        let sidebar_width = if visible_sidebar {
            self.sidebar.read(cx).width()
        } else {
            0.0
        };
        let card_width = (f32::from(bounds.size.width) - sidebar_width - inspector_width).max(0.0);
        let card_height = f32::from(bounds.size.height).max(0.0);
        let selected = self
            .services
            .store
            .store
            .read()
            .expect("session store lock poisoned")
            .selected_session_id()
            .cloned();
        let terminal_in_workspace_panel = inspector_seam > 0.5
            && self
                .inspector
                .as_ref()
                .is_some_and(|inspector| inspector.read(cx).is_terminal_tab());
        let workspace_owns_terminal = self
            .inspector
            .as_ref()
            .is_some_and(|inspector| inspector.read(cx).workspace_needs_terminal());
        let split_open = !workspace_owns_terminal
            && (self.auxiliary_terminal.is_some()
                || selected
                    .as_ref()
                    .is_some_and(|id| self.auxiliary_spawn_parent.as_ref() == Some(id)));
        let mut card = div()
            .relative()
            .flex_1()
            .flex()
            .flex_col()
            .h_full()
            .min_w(px(0.0))
            .when(seam <= 0.0, |card| card.rounded_tl(px(Radius::CARD)))
            .when(inspector_seam <= 0.0, |card| {
                card.rounded_tr(px(Radius::CARD))
            })
            .rounded_bl(px(Radius::CARD))
            .bg(terminal.background)
            .overflow_hidden()
            .text_color(terminal.primary);

        // Paint the frame independently from layout. A normal border shrinks
        // the content box, putting this title bar one pixel below the
        // borderless sidebar title bar even though both are 42 points tall.
        let card_outline = div()
            .absolute()
            .inset_0()
            .when(seam <= 0.0, |outline| outline.rounded_tl(px(Radius::CARD)))
            .when(inspector_seam <= 0.0, |outline| {
                outline.rounded_tr(px(Radius::CARD))
            })
            .rounded_bl(px(Radius::CARD))
            .border_1()
            .border_color(terminal.primary.alpha(0.10));

        if terminal_in_workspace_panel && let Some(auxiliary) = &self.auxiliary_terminal {
            auxiliary.update(cx, |terminal, cx| {
                terminal.set_shell_chrome(visible_sidebar, true, cx);
                terminal.set_header_trailing_inset(0.0, cx);
                terminal.set_viewport(
                    TerminalViewport {
                        x: sidebar_width + card_width,
                        y: Metrics::TITLE_BAR,
                        width: inspector_width,
                        height: card_height.max(0.0),
                    },
                    cx,
                );
            });
        }

        if self.preview && self.preview_scenario != PreviewScenario::Empty {
            card = card.child(self.preview_workbench(terminal));
        } else if split_open {
            let available_height = (card_height - 1.0).max(0.0);
            self.terminal_available_height = available_height;
            let heights = self.workbench_layout.pane_heights(available_height);
            if let Some(primary) = &self.terminal {
                primary.update(cx, |terminal, cx| {
                    terminal.set_shell_chrome(visible_sidebar, self.inspector_open, cx);
                    terminal.set_viewport(
                        TerminalViewport {
                            x: sidebar_width,
                            y: 0.0,
                            width: card_width,
                            height: heights.primary,
                        },
                        cx,
                    );
                });
                card = card.child(
                    div()
                        .flex_none()
                        .w_full()
                        .h(px(heights.primary))
                        .min_h(px(0.0))
                        .overflow_hidden()
                        .child(primary.clone()),
                );
            }
            card = card.child(self.terminal_resize_handle(cx));

            let mut auxiliary = div()
                .relative()
                .flex_none()
                .w_full()
                .h(px(heights.auxiliary))
                .min_h(px(0.0))
                .overflow_hidden();
            if let Some(terminal) = &self.auxiliary_terminal {
                terminal.update(cx, |terminal, cx| {
                    terminal.set_header_trailing_inset(48.0, cx);
                    terminal.set_viewport(
                        TerminalViewport {
                            x: sidebar_width,
                            y: heights.primary + 1.0,
                            width: card_width,
                            height: heights.auxiliary,
                        },
                        cx,
                    );
                });
                auxiliary = auxiliary.child(terminal.clone());
            } else {
                auxiliary = auxiliary.child(
                    div()
                        .size_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .bg(terminal.background)
                        .text_size(px(12.0))
                        .text_color(terminal.secondary)
                        .child("Opening terminal…"),
                );
            }
            if let Some(id) = self.auxiliary_id.clone() {
                let store = Arc::clone(&self.services.store);
                let primary = self.terminal.clone();
                auxiliary = auxiliary.child(
                    div()
                        .id("close-auxiliary-terminal")
                        .absolute()
                        .top(px(9.0))
                        .right(px(12.0))
                        .size(px(24.0))
                        .debug_selector(|| "close-auxiliary-terminal".into())
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(Radius::BADGE))
                        .cursor_pointer()
                        .text_color(terminal.secondary)
                        .hover(move |button| button.bg(terminal.primary.alpha(0.08)))
                        .child(sf_symbol("xmark", 10.5, terminal.secondary))
                        .on_click(move |_, window, cx| {
                            store
                                .store
                                .write()
                                .expect("session store lock poisoned")
                                .remove_sessions(vec![id.clone()]);
                            if let Some(primary) = &primary {
                                primary.update(cx, |terminal, cx| terminal.focus(window, cx));
                            }
                            cx.stop_propagation();
                        }),
                );
            }
            card = card.child(auxiliary);
        } else if let Some(primary) = &self.terminal {
            self.terminal_available_height = card_height;
            primary.update(cx, |terminal, cx| {
                terminal.set_shell_chrome(visible_sidebar, self.inspector_open, cx);
                terminal.set_viewport(
                    TerminalViewport {
                        x: sidebar_width,
                        y: 0.0,
                        width: card_width,
                        height: card_height,
                    },
                    cx,
                );
            });
            card = card.child(primary.clone());
        }

        card.child(card_outline).into_any_element()
    }

    fn preview_workbench(&self, colors: SemanticColors) -> AnyElement {
        let scenario = match self.preview_scenario {
            PreviewScenario::Typical => "Typical",
            PreviewScenario::Stress => "Stress",
            PreviewScenario::Empty => "Empty",
            PreviewScenario::Artifacts => "Artifacts",
        };
        div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .child(
                div()
                    .w(px(360.0))
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(px(22.0))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .items_center()
                            .gap(px(7.0))
                            .child(
                                div()
                                    .text_size(px(25.0))
                                    .font_weight(FontWeight::THIN)
                                    .text_color(colors.secondary)
                                    .child(sf_symbol_weighted(
                                        "sidebar.left",
                                        25.0,
                                        SymbolWeight::Regular,
                                        colors.secondary,
                                    )),
                            )
                            .child(
                                div()
                                    .text_size(px(17.0))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child("Sidebar design preview"),
                            )
                            .child(
                                div()
                                    .text_size(px(Typo::META.size))
                                    .text_color(colors.secondary)
                                    .child("Mock data only · no daemon connection"),
                            ),
                    )
                    .child(preview_control("Content", scenario, colors))
                    .child(preview_control("Appearance", "Dark", colors))
                    .child(
                        div()
                            .w_full()
                            .p(px(14.0))
                            .flex()
                            .flex_col()
                            .gap(px(9.0))
                            .rounded(px(Radius::PANEL))
                            .bg(colors.primary.alpha(0.045))
                            .border_1()
                            .border_color(colors.primary.alpha(0.07))
                            .child(preview_hint(
                                "cursorarrow.rays",
                                "Hover rows and project headers",
                                colors,
                            ))
                            .child(preview_hint(
                                "cursorarrow.click.2",
                                "Select, collapse, rename, and drag mock sessions",
                                colors,
                            ))
                            .child(preview_hint(
                                "arrow.left.and.right",
                                "Resize the sidebar from its trailing edge",
                                colors,
                            )),
                    ),
            )
            .into_any_element()
    }

    fn close_confirmation(
        &self,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let (title, message) = self.sidebar.read(cx).pending_close_copy()?;
        Some(
            div()
                .absolute()
                .inset_0()
                .occlude()
                .flex()
                .items_center()
                .justify_center()
                .bg(gpui::rgba(0x00000055))
                .on_mouse_down(MouseButton::Left, {
                    let sidebar = self.sidebar.clone();
                    move |_, _, cx| {
                        sidebar.update(cx, |sidebar, cx| sidebar.cancel_close(cx));
                        cx.stop_propagation();
                    }
                })
                .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
                .child(FloatingSurface::new(
                    colors,
                    div()
                        .w(px(320.0))
                        .p(px(18.0))
                        .flex()
                        .flex_col()
                        .gap(px(10.0))
                        .occlude()
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .child(
                            div()
                                .text_size(px(Typo::DISPLAY_TITLE.size))
                                .font_weight(Typo::DISPLAY_TITLE.weight)
                                .text_color(colors.primary)
                                .child(title),
                        )
                        .child(
                            div()
                                .text_size(px(Typo::ROW.size))
                                .text_color(colors.secondary)
                                .child(message),
                        )
                        .child(
                            div()
                                .mt(px(6.0))
                                .flex()
                                .justify_end()
                                .gap(px(8.0))
                                .child(
                                    div()
                                        .id("cancel-close")
                                        .px(px(12.0))
                                        .h(px(30.0))
                                        .flex()
                                        .items_center()
                                        .rounded(px(Radius::ROW))
                                        .cursor_pointer()
                                        .text_size(px(Typo::ROW.size))
                                        .text_color(colors.secondary)
                                        .hover(move |button| button.bg(colors.primary.alpha(0.06)))
                                        .child("Cancel")
                                        .on_click({
                                            let sidebar = self.sidebar.clone();
                                            move |_, _, cx| {
                                                sidebar.update(cx, |sidebar, cx| {
                                                    sidebar.cancel_close(cx)
                                                });
                                            }
                                        }),
                                )
                                .child(
                                    div()
                                        .id("confirm-close")
                                        .px(px(12.0))
                                        .h(px(30.0))
                                        .flex()
                                        .items_center()
                                        .rounded(px(Radius::ROW))
                                        .cursor_pointer()
                                        .bg(diri_ui::Ink::DANGER.alpha(0.16))
                                        .text_size(px(Typo::ROW.size))
                                        .font_weight(FontWeight::MEDIUM)
                                        .text_color(diri_ui::Ink::DANGER)
                                        .child("Close")
                                        .on_click({
                                            let sidebar = self.sidebar.clone();
                                            move |_, _, cx| {
                                                sidebar.update(cx, |sidebar, cx| {
                                                    sidebar.confirm_close(cx)
                                                });
                                            }
                                        }),
                                ),
                        ),
                ))
                .into_any_element(),
        )
    }

    fn quote_target_picker(
        &self,
        colors: SemanticColors,
        sidebar_width: f32,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let picker = self.quote_target_picker.as_ref()?;
        let targets = picker.targets.clone();
        let active = self
            .services
            .store
            .store
            .read()
            .expect("session store lock poisoned")
            .selected_session_id()
            .cloned();
        let mut rows = div().py(px(4.0)).flex().flex_col().gap(px(1.0));
        for (index, session) in targets.into_iter().enumerate() {
            let highlighted = index == picker.highlighted;
            let is_active = active.as_ref() == Some(&session.id);
            let detail = if session.hibernation.is_some() {
                "Sleeping · stages without waking"
            } else if is_active {
                "Active session"
            } else {
                "Keeps current session active"
            };
            rows = rows.child(
                div()
                    .id(("quote-target", index))
                    .debug_selector(move || format!("QUOTE_TARGET_{index}"))
                    .min_h(px(46.0))
                    .mx(px(4.0))
                    .px(px(8.0))
                    .py(px(6.0))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .rounded(px(Radius::ROW))
                    .bg(if highlighted {
                        rgba(0x5b8fd12f)
                    } else {
                        colors.primary.alpha(0.0)
                    })
                    .border_1()
                    .border_color(if highlighted {
                        rgba(0x8bb9e878)
                    } else {
                        colors.primary.alpha(0.0)
                    })
                    .cursor_pointer()
                    .hover(move |row| row.bg(colors.primary.alpha(0.075)))
                    .child(
                        div()
                            .size(px(26.0))
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_full()
                            .bg(colors.primary.alpha(0.055))
                            .child(sf_symbol("terminal", 11.0, colors.secondary)),
                    )
                    .child(
                        div()
                            .min_w(px(0.0))
                            .flex_1()
                            .flex()
                            .flex_col()
                            .gap(px(2.0))
                            .child(
                                div()
                                    .truncate()
                                    .text_size(px(Typo::ROW.size))
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(colors.primary)
                                    .child(session.title),
                            )
                            .child(
                                div()
                                    .truncate()
                                    .text_size(px(Typo::META.size))
                                    .text_color(colors.tertiary)
                                    .child(detail),
                            ),
                    )
                    .when(highlighted, |row| {
                        row.child(sf_symbol("return", 9.5, colors.secondary))
                    })
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.activate_quote_target(index, window, cx);
                        cx.stop_propagation();
                    })),
            );
        }

        let panel = div()
            .id("quote-target-picker")
            .debug_selector(|| "QUOTE_TARGET_PICKER".to_owned())
            .absolute()
            .top(px(Metrics::TITLE_BAR + 6.0))
            .left(px(7.0))
            .w(px((sidebar_width - 14.0).max(220.0)))
            .max_h(px(460.0))
            .flex()
            .flex_col()
            .rounded(px(Radius::PANEL))
            .overflow_hidden()
            .bg(colors.floating_surface())
            .border_1()
            .border_color(colors.floating_stroke())
            .shadow_lg()
            .occlude()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .h(px(43.0))
                    .px(px(11.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(7.0))
                    .border_b_1()
                    .border_color(colors.primary.alpha(0.07))
                    .child(sf_symbol("text.quote", 11.5, rgba(0x8bb9e8ff)))
                    .child(
                        div()
                            .min_w(px(0.0))
                            .flex_1()
                            .flex()
                            .flex_col()
                            .gap(px(1.0))
                            .child(
                                div()
                                    .text_size(px(Typo::ROW.size))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(colors.primary)
                                    .child("Quote into…"),
                            )
                            .child(
                                div()
                                    .text_size(px(Typo::META.size))
                                    .text_color(colors.tertiary)
                                    .child("↑↓ choose · Return stage · Esc cancel"),
                            ),
                    )
                    .child(
                        div()
                            .id("quote-target-close")
                            .size(px(21.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(Radius::CHIP))
                            .cursor_pointer()
                            .hover(move |button| button.bg(colors.primary.alpha(0.07)))
                            .child(sf_symbol("xmark", 9.0, colors.tertiary))
                            .on_click(cx.listener(|this, _, window, cx| {
                                if let Some(picker) = this.quote_target_picker.take() {
                                    this.restore_quote_focus(picker.return_surface, window, cx);
                                }
                                cx.notify();
                                cx.stop_propagation();
                            })),
                    ),
            )
            .child(
                div()
                    .id("quote-target-scroll")
                    .min_h(px(0.0))
                    .overflow_y_scroll()
                    .child(rows),
            );
        let panel = if cx.reduce_motion() {
            panel.into_any_element()
        } else {
            panel
                .with_animation(
                    "quote-target-picker-enter",
                    Animation::new(Duration::from_millis(150)).with_easing(ease_out_quint()),
                    |panel, delta| {
                        panel
                            .top(px(Metrics::TITLE_BAR + (1.0 - delta) * 6.0 + 6.0))
                            .opacity(0.72 + delta * 0.28)
                    },
                )
                .into_any_element()
        };

        Some(
            div()
                .absolute()
                .top_0()
                .bottom_0()
                .left_0()
                .w(px(sidebar_width.max(234.0)))
                .bg(colors.background.alpha(0.20))
                .occlude()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, window, cx| {
                        if let Some(picker) = this.quote_target_picker.take() {
                            this.restore_quote_focus(picker.return_surface, window, cx);
                        }
                        cx.notify();
                        cx.stop_propagation();
                    }),
                )
                .child(panel)
                .into_any_element(),
        )
    }

    fn status_banner(&self, colors: SemanticColors, cx: &mut Context<Self>) -> Option<AnyElement> {
        let banner = self.status_banner.as_ref()?;
        Some(
            deferred(
                div()
                    .absolute()
                    .right(px(16.0))
                    .bottom(px(16.0))
                    .w(px(360.0))
                    .p(px(13.0))
                    .flex()
                    .items_start()
                    .gap(px(10.0))
                    .rounded(px(Radius::PANEL))
                    .bg(colors.background)
                    .border_1()
                    .border_color(colors.floating_stroke())
                    .shadow_lg()
                    .occlude()
                    .child(
                        div()
                            .min_w(px(0.0))
                            .flex_1()
                            .flex()
                            .flex_col()
                            .gap(px(3.0))
                            .child(
                                div()
                                    .text_size(px(Typo::ROW_EMPHASIZED.size))
                                    .font_weight(Typo::ROW_EMPHASIZED.weight)
                                    .text_color(colors.primary)
                                    .child(banner.title.clone()),
                            )
                            .child(
                                div()
                                    .text_size(px(Typo::META.size))
                                    .text_color(colors.secondary)
                                    .child(banner.body.clone()),
                            ),
                    )
                    .child(
                        div()
                            .id("dismiss-status-banner")
                            .size(px(22.0))
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(Radius::CHIP))
                            .cursor_pointer()
                            .text_color(colors.tertiary)
                            .hover(move |button| button.bg(colors.primary.alpha(0.06)))
                            .child(sf_symbol_weighted(
                                "xmark",
                                8.5,
                                SymbolWeight::Bold,
                                colors.tertiary,
                            ))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.status_banner_generation =
                                    this.status_banner_generation.wrapping_add(1);
                                this.status_banner = None;
                                cx.notify();
                            })),
                    ),
            )
            .into_any_element(),
        )
    }

    fn recovery_notice(&self, notice: RecoveryNotice, colors: SemanticColors) -> AnyElement {
        let accent = match notice.kind {
            RecoveryKind::Connecting
            | RecoveryKind::Reconnecting
            | RecoveryKind::RetryingAction => colors.secondary,
            RecoveryKind::ManualAttention | RecoveryKind::ActionFailed => Ink::ATTENTION,
        };
        let mut bar = div()
            .id("recovery-notice")
            .debug_selector(|| "RECOVERY_NOTICE".to_owned())
            .absolute()
            .top_0()
            .left_0()
            .right_0()
            .h(px(42.0))
            .px(px(14.0))
            .flex()
            .items_center()
            .gap(px(9.0))
            .bg(colors.sidebar_surface())
            .border_b_1()
            .border_color(colors.primary.alpha(0.08))
            .text_color(colors.primary)
            .child(div().size(px(7.0)).flex_none().rounded_full().bg(accent))
            .child(
                div()
                    .min_w(px(0.0))
                    .flex_1()
                    .flex()
                    .items_baseline()
                    .gap(px(7.0))
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(Typo::ROW_EMPHASIZED.size))
                            .font_weight(Typo::ROW_EMPHASIZED.weight)
                            .child(notice.title),
                    )
                    .child(
                        div()
                            .min_w(px(0.0))
                            .text_ellipsis()
                            .text_size(px(Typo::META.size))
                            .text_color(colors.secondary)
                            .child(bounded_notice_body(&notice.body)),
                    ),
            );
        if let Some((action, label)) = notice.primary_action {
            let store = Arc::clone(&self.services.store.store);
            bar = bar.child(
                div()
                    .id("recovery-primary-action")
                    .h(px(27.0))
                    .px(px(9.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .rounded(px(Radius::ROW))
                    .cursor_pointer()
                    .bg(colors.primary.alpha(0.075))
                    .hover(move |button| button.bg(colors.primary.alpha(0.12)))
                    .text_size(px(Typo::META.size))
                    .font_weight(FontWeight::MEDIUM)
                    .child(label)
                    .on_click(move |_, _, cx| {
                        let mut store = store.write().expect("session store lock poisoned");
                        match action {
                            RecoveryAction::RetryConnection => store.retry_connection(),
                            RecoveryAction::RetryAction => store.retry_last_action(),
                        }
                        cx.stop_propagation();
                    }),
            );
        }
        if notice.dismissible {
            let store = Arc::clone(&self.services.store.store);
            bar = bar.child(
                div()
                    .id("dismiss-recovery-notice")
                    .size(px(24.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(Radius::CHIP))
                    .cursor_pointer()
                    .hover(move |button| button.bg(colors.primary.alpha(0.06)))
                    .child(sf_symbol_weighted(
                        "xmark",
                        8.5,
                        SymbolWeight::Bold,
                        colors.tertiary,
                    ))
                    .on_click(move |_, _, cx| {
                        store
                            .write()
                            .expect("session store lock poisoned")
                            .dismiss_action_failure();
                        cx.stop_propagation();
                    }),
            );
        }
        bar.into_any_element()
    }
}

impl Render for RootView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.pending_notification_open.is_some()
            && self
                .services
                .store
                .store
                .read()
                .expect("store")
                .has_hydrated_sessions()
            && let Some((session, event)) = self.pending_notification_open.take()
        {
            self.open_notification(session, event, window, cx);
        }
        let colors = self.colors();
        let recovery_notice = if self.preview {
            None
        } else {
            let store = self
                .services
                .store
                .store
                .read()
                .expect("session store lock poisoned");
            RecoveryNotice::resolve(store.daemon_state(), store.action_failure())
        };
        let recovery_height = if recovery_notice.is_some() { 42.0 } else { 0.0 };
        #[cfg(target_os = "macos")]
        self.notifier.set_badge(
            self.services
                .store
                .store
                .read()
                .expect("store")
                .notifications()
                .unread_count(),
        );
        let launcher_open = self.launcher.read(cx).is_open();
        let notification_surface_visible = !launcher_open
            && !self.notification_panel_open
            && !self
                .utility_surfaces
                .as_ref()
                .is_some_and(|surfaces| surfaces.read(cx).is_open());
        self.services
            .store
            .store
            .write()
            .expect("store")
            .set_notification_surface_visible(notification_surface_visible);
        let sidebar_visible = self.sidebar.read(cx).is_visible();
        let sidebar_width = self.sidebar.read(cx).width();
        let panel_width = if sidebar_visible || self.sidebar.read(cx).is_peeking() {
            sidebar_width
        } else {
            0.0
        };
        let window_width = f32::from(window.inner_window_bounds().get_bounds().size.width);
        let occupied_sidebar_width = if sidebar_visible { sidebar_width } else { 0.0 };
        self.inspector_max_width =
            (window_width - occupied_sidebar_width - 320.0).clamp(0.0, 720.0);
        // The inspector's own width, whether or not it is currently shown --
        // the panel keeps painting at full width while it slides away.
        let inspector_panel_width = self.inspector_width.min(self.inspector_max_width);
        let inspector_width = if self.inspector_open && !launcher_open {
            inspector_panel_width
        } else {
            0.0
        };
        let now = Instant::now();
        self.sidebar_panel_width =
            advance_seam(&mut self.sidebar_panel_slide, panel_width, now, window);
        self.sidebar_float = advance_seam(
            &mut self.sidebar_float_slide,
            if self.sidebar_floating { 1.0 } else { 0.0 },
            now,
            window,
        );
        self.sidebar_seam =
            advance_seam(&mut self.sidebar_slide, occupied_sidebar_width, now, window);
        self.inspector_seam = advance_seam(&mut self.inspector_slide, inspector_width, now, window);
        let seam = self.sidebar_seam;
        let inspector_seam = self.inspector_seam;
        #[cfg(target_os = "macos")]
        {
            let browser_active = self.browser_visible(launcher_open, inspector_panel_width, cx);
            self.browser.borrow_mut().set_visible(browser_active);
            self.browser.borrow_mut().set_pointer_passthrough(
                self.resize_origin.is_some()
                    || self.inspector_resize_origin.is_some()
                    || self.terminal_resize_origin.is_some(),
            );
        }
        let mut key_context = KeyContext::new_with_defaults();
        key_context.add(APP_CONTEXT);
        key_context.add(SESSION_NAVIGATION_CONTEXT);
        let inset = SIDEBAR_PEEK_INSET * self.sidebar_float;
        let radius = Radius::PANEL * self.sidebar_float;
        let exposed = self.sidebar_panel_width;
        // Keep workbench text from bleeding through the floating panel, then
        // ease its material back to the docked theme alongside the geometry.
        let mut sidebar_surface = colors.sidebar_surface();
        sidebar_surface.a += (1.0 - sidebar_surface.a) * self.sidebar_float;
        let peek_pointer_tracking = self.sidebar.read(cx).is_peeking().then(|| {
            let region = gpui::Bounds::new(
                gpui::point(px(0.0), px(recovery_height)),
                gpui::size(
                    px(sidebar_width + inset),
                    window.viewport_size().height - px(recovery_height),
                ),
            );
            // Capture moves even when a terminal or menu handles the bubble
            // phase. The gap and panel are one hover target, including the
            // first stationary frame after the edge dwell opens it.
            let sidebar = self.sidebar.downgrade();
            gpui::canvas(
                |_, _, _| (),
                move |_, _, window, _| {
                    let moving_sidebar = sidebar.clone();
                    window.on_mouse_event(
                        move |event: &gpui::MouseMoveEvent, phase, window, cx| {
                            if phase == gpui::DispatchPhase::Capture {
                                let _ = moving_sidebar.update(cx, |sidebar, cx| {
                                    sidebar.hover_peek_region(
                                        region.contains(&event.position),
                                        window,
                                        cx,
                                    );
                                });
                            }
                        },
                    );
                    let sidebar = sidebar.clone();
                    window.on_mouse_event(move |_: &gpui::MouseExitEvent, phase, window, cx| {
                        if phase == gpui::DispatchPhase::Capture {
                            let _ = sidebar.update(cx, |sidebar, cx| {
                                sidebar.hover_peek_region(false, window, cx)
                            });
                        }
                    });
                },
            )
            .absolute()
            .size_full()
        });
        let sidebar_wrapper = div()
            .id("sidebar-frame")
            .absolute()
            .left_0()
            .top(px(recovery_height))
            .bottom_0()
            // Include the inset in the hover region so the edge and card
            // are one continuous target, including during docking.
            .w(px(exposed + inset * exposed / sidebar_width))
            .when(exposed > 0.0, |wrapper| {
                wrapper.occlude().child(
                    div()
                        .id("sidebar-surface")
                        .debug_selector(|| "sidebar-surface".into())
                        .absolute()
                        .top(px(inset))
                        .bottom(px(inset))
                        .right(px(0.0))
                        .w(px(sidebar_width))
                        .rounded(px(radius))
                        .bg(sidebar_surface)
                        .occlude()
                        .shadow(vec![BoxShadow {
                            color: gpui::black().opacity(0.32 * self.sidebar_float),
                            offset: gpui::point(px(0.0), px(8.0 * self.sidebar_float)),
                            blur_radius: px(24.0 * self.sidebar_float),
                            spread_radius: px(0.0),
                            inset: false,
                        }])
                        // A reactive boundary: the sidebar re-renders on its
                        // own notifies, not on the terminal's 60fps repaints.
                        .child(
                            self.sidebar
                                .clone()
                                .cached(StyleRefinement::default().size_full()),
                        )
                        .child(
                            div()
                                .absolute()
                                .inset_0()
                                .rounded(px(radius))
                                .border_1()
                                .border_color(colors.floating_stroke().opacity(self.sidebar_float)),
                        )
                        .child(
                            div()
                                .absolute()
                                .right_0()
                                .top_0()
                                .bottom_0()
                                .w(px(1.0))
                                .bg(colors.sidebar_stroke().opacity(1.0 - self.sidebar_float)),
                        ),
                )
            });

        let mut root = div()
            .id("root")
            .key_context(key_context)
            .relative()
            .size_full()
            .pt(px(recovery_height))
            // Real SF Pro (registered from SFNS.ttf at startup) for every UI
            // surface; the terminal grid sets its own mono font.
            .font_family(crate::fonts::ui_family())
            .flex()
            // Match the opaque platform window so content behind diri never
            // participates in compositing. The sidebar keeps its own surface
            // treatment above this base.
            .bg(colors.background)
            .track_focus(&self.focus)
            .capture_key_down(cx.listener(Self::on_key_down))
            .capture_key_up(cx.listener(Self::on_key_up))
            .on_action(cx.listener(Self::close_selected_session))
            .on_action(cx.listener(Self::reopen_last_session))
            .on_action(cx.listener(Self::toggle_launcher))
            .on_action(cx.listener(|this, _: &NewDefaultSession, window, cx| {
                this.run_command(CommandId::NewDefaultSession, window, cx);
            }))
            .on_action(cx.listener(|this, _: &NewTerminal, window, cx| {
                this.run_command(CommandId::NewTerminal, window, cx);
            }))
            .on_action(cx.listener(|this, _: &NewCodexSession, window, cx| {
                this.run_command(CommandId::NewCodexSession, window, cx);
            }))
            .on_action(cx.listener(|this, _: &ToggleCommandPalette, window, cx| {
                this.run_command(CommandId::ToggleCommandPalette, window, cx);
            }))
            .on_action(cx.listener(|this, _: &ToggleQuickOpen, window, cx| {
                this.run_command(CommandId::ToggleQuickOpen, window, cx);
            }))
            .on_action(cx.listener(
                |this, _: &crate::commands::ToggleNotifications, window, cx| {
                    this.toggle_notifications(window, cx);
                },
            ))
            .on_action(cx.listener(|this, _: &ToggleHistory, window, cx| {
                this.run_command(CommandId::ToggleHistory, window, cx);
            }))
            .on_action(cx.listener(|this, _: &ToggleOverview, window, cx| {
                this.run_command(CommandId::ToggleOverview, window, cx);
            }))
            .on_action(cx.listener(|this, _: &OpenWorktrees, window, cx| {
                this.run_command(CommandId::OpenWorktrees, window, cx);
            }))
            .on_action(cx.listener(|this, _: &OpenSettings, window, cx| {
                this.run_command(CommandId::OpenSettings, window, cx);
            }))
            .on_action(cx.listener(|this, _: &commands::ShowSettings, _, cx| {
                if let Some(surfaces) = &this.utility_surfaces {
                    surfaces.update(cx, |surfaces, cx| {
                        if !surfaces.is_settings_open() {
                            surfaces.open_settings(cx);
                        }
                    });
                }
            }))
            .on_action(cx.listener(|this, _: &ToggleSidebar, window, cx| {
                this.run_command(CommandId::ToggleSidebar, window, cx);
            }))
            .on_action(cx.listener(|this, _: &FocusSidebar, window, cx| {
                this.run_command(CommandId::FocusSidebar, window, cx);
            }))
            .on_action(cx.listener(|this, _: &ToggleInspector, window, cx| {
                this.run_command(CommandId::ToggleInspector, window, cx);
            }))
            .on_action(
                cx.listener(|this, _: &ToggleAuxiliaryTerminal, window, cx| {
                    this.run_command(CommandId::ToggleAuxiliaryTerminal, window, cx);
                }),
            )
            .on_action(cx.listener(|this, _: &QuoteSelection, window, cx| {
                this.run_command(CommandId::QuoteSelection, window, cx);
            }))
            .on_action(
                cx.listener(|this, _: &QuoteSelectionToSession, window, cx| {
                    this.run_command(CommandId::QuoteSelectionToSession, window, cx);
                }),
            )
            .on_action(cx.listener(|this, _: &ArchiveSelectedSession, window, cx| {
                this.run_command(CommandId::ArchiveSelectedSession, window, cx);
            }))
            .on_action(cx.listener(|this, _: &RenameSelectedSession, window, cx| {
                this.run_command(CommandId::RenameSelectedSession, window, cx);
            }))
            .on_action(
                cx.listener(|this, _: &DelegateSelectedSession, window, cx| {
                    this.run_command(CommandId::DelegateSelectedSession, window, cx);
                }),
            )
            .on_action(
                cx.listener(|this, _: &SelectNextAttentionSession, window, cx| {
                    this.run_command(CommandId::SelectNextAttentionSession, window, cx);
                }),
            )
            .on_action(cx.listener(|this, _: &CheckForUpdates, window, cx| {
                this.run_command(CommandId::CheckForUpdates, window, cx);
            }))
            .on_action(cx.listener(|this, _: &SelectPreviousSession, window, cx| {
                this.run_command(CommandId::SelectPreviousSession, window, cx);
            }))
            .on_action(cx.listener(|this, _: &SelectNextSession, window, cx| {
                this.run_command(CommandId::SelectNextSession, window, cx);
            }))
            .on_action(cx.listener(|this, _: &MoveSelectedSessionUp, window, cx| {
                this.run_command(CommandId::MoveSelectedSessionUp, window, cx);
            }))
            .on_action(
                cx.listener(|this, _: &MoveSelectedSessionDown, window, cx| {
                    this.run_command(CommandId::MoveSelectedSessionDown, window, cx);
                }),
            )
            .on_action(cx.listener(|this, _: &SelectSession1, window, cx| {
                this.run_command(CommandId::SelectSession1, window, cx);
            }))
            .on_action(cx.listener(|this, _: &SelectSession2, window, cx| {
                this.run_command(CommandId::SelectSession2, window, cx);
            }))
            .on_action(cx.listener(|this, _: &SelectSession3, window, cx| {
                this.run_command(CommandId::SelectSession3, window, cx);
            }))
            .on_action(cx.listener(|this, _: &SelectSession4, window, cx| {
                this.run_command(CommandId::SelectSession4, window, cx);
            }))
            .on_action(cx.listener(|this, _: &SelectSession5, window, cx| {
                this.run_command(CommandId::SelectSession5, window, cx);
            }))
            .on_action(cx.listener(|this, _: &SelectSession6, window, cx| {
                this.run_command(CommandId::SelectSession6, window, cx);
            }))
            .on_action(cx.listener(|this, _: &SelectSession7, window, cx| {
                this.run_command(CommandId::SelectSession7, window, cx);
            }))
            .on_action(cx.listener(|this, _: &SelectSession8, window, cx| {
                this.run_command(CommandId::SelectSession8, window, cx);
            }))
            .on_action(cx.listener(|this, _: &SelectLastSession, window, cx| {
                this.run_command(CommandId::SelectLastSession, window, cx);
            }))
            .on_modifiers_changed(cx.listener(Self::on_modifiers_changed))
            // Fires for every move once the seam drag starts, wherever the
            // pointer wanders -- unlike hover-gated move listeners.
            .on_drag_move(
                cx.listener(|this, event: &DragMoveEvent<DraggedSidebarEdge>, _, cx| {
                    this.drag_resize(f32::from(event.event.position.x), cx);
                }),
            )
            .on_drag_move(
                cx.listener(|this, event: &DragMoveEvent<DraggedTerminalEdge>, _, cx| {
                    this.drag_terminal_resize(f32::from(event.event.position.y), cx);
                }),
            )
            .on_drag_move(cx.listener(
                |this, event: &DragMoveEvent<DraggedInspectorEdge>, _, cx| {
                    this.drag_inspector_resize(f32::from(event.event.position.x), cx);
                },
            ))
            .child(div().flex_none().h_full().w(px(seam)))
            .when(seam > 0.0, |root| root.child(self.resize_handle(cx)));
        if launcher_open {
            // Command-N behaves like an unsaved new tab: preserve the app
            // shell, but replace the live session pane instead of floating a
            // dialog above it or manufacturing another session/tab up front.
            root = root.child(
                div()
                    .relative()
                    .flex_1()
                    .h_full()
                    .min_w(px(0.0))
                    .overflow_hidden()
                    .child(
                        self.launcher
                            .clone()
                            .cached(StyleRefinement::default().size_full()),
                    ),
            );
        } else {
            root = root.child(self.terminal_card(
                sidebar_visible,
                seam,
                inspector_width,
                inspector_seam,
                window,
                cx,
            ));
        }
        if inspector_seam > 0.0 {
            root = root.child(self.inspector_resize_handle(cx));
            if let Some(inspector) = &self.inspector {
                root = root.child(
                    div()
                        .relative()
                        .flex_none()
                        .h_full()
                        .w(px(inspector_seam))
                        .overflow_hidden()
                        .border_l_1()
                        .border_color(colors.primary.alpha(0.08))
                        .child(
                            div()
                                .absolute()
                                .top(px(0.0))
                                .left(px(0.0))
                                .h_full()
                                .w(px(inspector_panel_width))
                                .child(
                                    inspector
                                        .clone()
                                        .cached(StyleRefinement::default().size_full()),
                                ),
                        ),
                );
            }
        }
        // This overlay never participates in the terminal's flex layout or
        // viewport sizing. Keep it below dialogs and above workbench content.
        root = root.child(sidebar_wrapper).children(peek_pointer_tracking);
        if !sidebar_visible && seam == 0.0 && exposed == 0.0 && panel_width == 0.0 {
            root = root.child(
                div()
                    .id("sidebar-peek-edge")
                    .debug_selector(|| "sidebar-peek-edge".into())
                    .absolute()
                    .left_0()
                    .top(px(recovery_height + 36.0))
                    .bottom_0()
                    .w(px(SIDEBAR_PEEK_TRIGGER_WIDTH))
                    .on_hover(cx.listener(|this, hovered: &bool, window, cx| {
                        this.sidebar_peek_dwell = None;
                        if *hovered {
                            this.sidebar_peek_dwell =
                                Some(cx.spawn_in(window, async move |this, cx| {
                                    cx.background_executor().timer(SIDEBAR_PEEK_DWELL).await;
                                    let _ = this.update_in(cx, |this, window, cx| {
                                        this.sidebar_peek_dwell = None;
                                        this.sidebar.update(cx, |sidebar, cx| {
                                            sidebar.hover_peek_region(true, window, cx);
                                            sidebar.peek(window, cx);
                                        });
                                    });
                                }));
                        }
                    })),
            );
        }
        if self.resize_origin.is_some()
            || self.terminal_resize_origin.is_some()
            || self.inspector_resize_origin.is_some()
        {
            root = root.child(self.resize_shield(cx));
        }
        if let Some(confirmation) = self.close_confirmation(colors, cx) {
            root = root.child(confirmation);
        }
        // Overlay views are cached reactive boundaries too: each subscribes to
        // store changes itself, so the only thing these wrappers must do is
        // stay out of the root flex row (absolute, zero-size at rest).
        if let Some(surfaces) = &self.session_surfaces {
            root = root.child(cached_window_overlay(surfaces.clone()));
        }
        if let Some(surfaces) = &self.utility_surfaces {
            // Settings is not a sheet floating over the workbench: it replaces
            // it, and the sidebar it navigates from stays put beside it. Laying
            // it out against the live seam -- rather than a width settings
            // snapshotted for itself -- keeps the two edges together while the
            // sidebar slides or is dragged wider.
            let mut placement = StyleRefinement::default().absolute().inset_0();
            if surfaces.read(cx).is_settings_open() {
                placement = placement.left(px(seam));
            }
            root = root.child(surfaces.clone().cached(placement));
        }
        if let Some(navigation) = &self.navigation {
            root = root.child(cached_window_overlay(navigation.clone()));
        }
        if let Some(picker) = self.quote_target_picker(colors, sidebar_width, cx) {
            root = root.child(deferred(picker));
        }
        if let Some(panel) = self.notification_panel(window, cx) {
            root = root.child(deferred(panel));
        }
        if let Some(status) = self.status_banner(colors, cx) {
            root = root.child(status);
        }
        if let Some(notice) = recovery_notice {
            root = root.child(self.recovery_notice(notice, colors));
        }
        if let Some(build) = &self.services.dev_build {
            root = root.child(dev_build_marker(
                build.marker_label(),
                colors,
                recovery_height + 10.0,
            ));
        }
        root
    }
}

fn bounded_notice_body(body: &str) -> String {
    body.trim()
        .chars()
        .filter(|character| !character.is_control())
        .take(240)
        .collect()
}

fn quote_target_id(targets: &[SessionRecord], index: usize) -> Option<SessionId> {
    targets.get(index).map(|session| session.id.clone())
}

fn is_quote_target(session: &SessionRecord) -> bool {
    !session.is_archived()
        && !matches!(session.status, SessionStatus::Exited(_))
        // Shell and generic sessions are raw terminals. A local agent draft
        // is safe precisely because its eventual send is an explicit prompt;
        // offering that affordance for a shell would turn prompt-shaped quote
        // data into an executable command when the user confirms it.
        && !session.effective_kind().is_terminal()
}

fn dev_build_marker(label: &str, colors: SemanticColors, top: f32) -> AnyElement {
    div()
        .absolute()
        .top(px(top))
        .left_0()
        .right_0()
        .flex()
        .justify_center()
        .child(
            div()
                .h(px(22.0))
                .px(px(7.0))
                .flex()
                .items_center()
                .gap(px(5.0))
                .rounded(px(Radius::CHIP))
                .border_1()
                .border_color(Ink::ATTENTION.alpha(0.22))
                .bg(colors.floating_surface())
                .text_size(px(Typo::META.size))
                .font_weight(Typo::META.weight)
                .text_color(colors.secondary)
                .child(
                    div()
                        .size(px(5.0))
                        .rounded_full()
                        .bg(Ink::ATTENTION.alpha(0.88)),
                )
                .child(div().text_color(Ink::ATTENTION.alpha(0.88)).child("DEV"))
                .child("·")
                .child(label.to_owned()),
        )
        .into_any_element()
}

fn preview_control(label: &str, value: &str, colors: SemanticColors) -> AnyElement {
    div()
        .w(px(330.0))
        .flex()
        .items_center()
        .child(
            div()
                .w(px(82.0))
                .text_size(px(Typo::ROW_EMPHASIZED.size))
                .font_weight(Typo::ROW_EMPHASIZED.weight)
                .text_color(colors.secondary)
                .child(label.to_owned()),
        )
        .child(
            div()
                .flex_1()
                .h(px(26.0))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(Radius::BADGE))
                .bg(colors.primary.alpha(0.08))
                .text_size(px(Typo::META.size))
                .text_color(colors.primary)
                .child(value.to_owned()),
        )
        .into_any_element()
}

fn preview_hint(system_image: &str, label: &str, colors: SemanticColors) -> AnyElement {
    div()
        .flex()
        .items_center()
        .gap(px(9.0))
        .child(
            div()
                .w(px(15.0))
                .flex()
                .items_center()
                .justify_center()
                .child(sf_symbol(system_image, 11.0, colors.secondary)),
        )
        .child(
            div()
                .text_size(px(Typo::ROW.size))
                .text_color(colors.primary.alpha(0.82))
                .child(label.to_owned()),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sidebar::{PreviewScenario, SidebarPreviewFixture};
    use gpui::{Modifiers, size};

    pub(super) fn test_services() -> Arc<AppServices> {
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

    #[gpui::test]
    fn palette_settings_clicks_reach_themes_and_full_settings(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            cx.set_reduce_motion(true);
            crate::commands::bind_keys(cx, &Default::default());
        });
        let services = test_services();
        let store = services.store.clone();
        let original_theme = store.store.read().unwrap().theme_id().to_owned();
        let (root, cx) = cx.add_window_view(move |window, cx| {
            RootView::new(services, false, PreviewScenario::Empty, window, cx)
        });
        cx.simulate_resize(size(px(1000.0), px(700.0)));
        cx.run_until_parked();
        cx.simulate_keystrokes("cmd-k s e t t i n g s");
        cx.run_until_parked();
        let position = cx.debug_bounds("palette-row-0").unwrap().center();
        cx.simulate_click(position, Modifiers::default());
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("palette-back").is_some(),
            "Settings opens a palette page"
        );
        let position = cx.debug_bounds("palette-row-0").unwrap().center();
        cx.simulate_click(position, Modifiers::default());
        cx.run_until_parked();
        cx.simulate_keystrokes("down");
        assert_ne!(store.store.read().unwrap().theme_id(), original_theme);
        cx.simulate_keystrokes("escape");
        assert_eq!(store.store.read().unwrap().theme_id(), original_theme);
        cx.simulate_keystrokes("cmd-k s e t t i n g s enter");
        cx.run_until_parked();
        let position = cx.debug_bounds("palette-row-1").unwrap().center();
        cx.simulate_click(position, Modifiers::default());
        cx.run_until_parked();
        root.read_with(cx, |root, cx| {
            assert!(
                root.utility_surfaces
                    .as_ref()
                    .unwrap()
                    .read(cx)
                    .is_settings_open()
            );
            assert!(!root.navigation.as_ref().unwrap().read(cx).is_open());
        });
        cx.simulate_keystrokes("cmd-k s e t t i n g s enter down enter");
        cx.run_until_parked();
        root.read_with(cx, |root, cx| {
            assert!(
                root.utility_surfaces
                    .as_ref()
                    .unwrap()
                    .read(cx)
                    .is_settings_open(),
                "All settings keeps an already open Settings canvas visible"
            );
        });
    }

    #[gpui::test]
    fn sidebar_peek_reveals_on_the_edge_and_leaves_the_layout_collapsed(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|cx| cx.set_reduce_motion(true));
        let services = test_services();
        let (root, cx) = cx.add_window_view(move |window, cx| {
            RootView::new(services, true, PreviewScenario::Typical, window, cx)
        });
        cx.simulate_resize(size(px(1000.0), px(700.0)));
        root.update(cx, |root, cx| {
            root.sidebar.update(cx, |sidebar, cx| sidebar.conceal(cx))
        });
        cx.run_until_parked();
        let edge = cx
            .debug_bounds("sidebar-peek-edge")
            .expect("collapsed edge");
        cx.simulate_mouse_move(edge.center(), None, Modifiers::default());
        cx.executor().advance_clock(Duration::from_millis(5));
        cx.simulate_mouse_move(
            gpui::point(px(600.0), px(300.0)),
            None,
            Modifiers::default(),
        );
        cx.executor().advance_clock(Duration::from_millis(120));
        cx.run_until_parked();
        assert!(!root.read_with(cx, |root, cx| root.sidebar.read(cx).is_peeking()));
        cx.simulate_mouse_move(edge.center(), None, Modifiers::default());
        cx.executor().advance_clock(Duration::from_millis(25));
        cx.run_until_parked();
        root.read_with(cx, |root, cx| {
            assert!(root.sidebar.read(cx).is_peeking());
            assert!(!root.sidebar.read(cx).is_visible());
            assert_eq!(
                root.sidebar_seam, 0.0,
                "peeking must not resize the terminal"
            );
            assert_eq!(root.sidebar_panel_width, root.sidebar.read(cx).width());
        });
        // Resting in the new inset must not start a close/reopen loop.
        cx.simulate_mouse_move(
            gpui::point(px(SIDEBAR_PEEK_INSET / 2.0), edge.center().y),
            None,
            Modifiers::default(),
        );
        cx.executor().advance_clock(Duration::from_secs(1));
        cx.run_until_parked();
        assert!(root.read_with(cx, |root, cx| root.sidebar.read(cx).is_peeking()));
        let floating = cx
            .debug_bounds("sidebar-surface")
            .expect("floating sidebar");
        assert_eq!(
            floating.origin,
            gpui::point(px(SIDEBAR_PEEK_INSET), px(SIDEBAR_PEEK_INSET))
        );
        assert_eq!(floating.size.height, px(700.0 - 2.0 * SIDEBAR_PEEK_INSET));
        let session = cx
            .debug_bounds("SESSION_preview-claude")
            .expect("peek session row");
        cx.simulate_click(session.center(), Modifiers::default());
        root.read_with(cx, |root, cx| {
            assert_eq!(
                root.sidebar.read(cx).selected_session().unwrap().id,
                SessionId::new("preview-claude")
            );
            assert!(
                root.sidebar.read(cx).is_peeking(),
                "selecting a session keeps the peek interactive"
            );
        });
        cx.simulate_mouse_move(
            gpui::point(px(150.0), px(300.0)),
            None,
            Modifiers::default(),
        );
        cx.executor().advance_clock(Duration::from_secs(1));
        cx.run_until_parked();
        assert!(root.read_with(cx, |root, cx| root.sidebar.read(cx).is_peeking()));
        cx.simulate_mouse_move(
            gpui::point(px(600.0), px(300.0)),
            None,
            Modifiers::default(),
        );
        cx.executor().advance_clock(Duration::from_millis(100));
        cx.run_until_parked();
        assert!(root.read_with(cx, |root, cx| root.sidebar.read(cx).is_peeking()));
        // Returning during the grace period cancels the pending dismissal.
        cx.simulate_mouse_move(
            gpui::point(px(150.0), px(300.0)),
            None,
            Modifiers::default(),
        );
        cx.executor().advance_clock(Duration::from_millis(300));
        cx.run_until_parked();
        assert!(root.read_with(cx, |root, cx| root.sidebar.read(cx).is_peeking()));
        cx.simulate_mouse_move(
            gpui::point(px(600.0), px(300.0)),
            None,
            Modifiers::default(),
        );
        cx.executor().advance_clock(Duration::from_millis(300));
        cx.run_until_parked();
        assert!(!root.read_with(cx, |root, cx| root.sidebar.read(cx).is_peeking()));
        assert!(cx.debug_bounds("sidebar-peek-edge").is_some());

        cx.simulate_mouse_move(edge.center(), None, Modifiers::default());
        cx.executor().advance_clock(Duration::from_millis(120));
        cx.run_until_parked();
        let pin = cx.debug_bounds("sidebar-toggle").expect("peek pin control");
        cx.simulate_click(pin.center(), Modifiers::default());
        cx.simulate_mouse_move(
            gpui::point(px(600.0), px(300.0)),
            None,
            Modifiers::default(),
        );
        cx.executor().advance_clock(Duration::from_secs(1));
        cx.run_until_parked();
        root.read_with(cx, |root, cx| {
            assert!(root.sidebar.read(cx).is_visible());
            assert!(!root.sidebar.read(cx).is_peeking());
            assert_eq!(root.sidebar_float, 0.0);
            assert_eq!(root.sidebar_seam, root.sidebar.read(cx).width());
        });
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "writes a visual preview to DIRI_PEEK_SCREENSHOT"]
    fn render_sidebar_peek_screenshot() {
        use gpui::{AppContext as _, HeadlessAppContext};
        let output = std::env::var("DIRI_PEEK_SCREENSHOT").expect("DIRI_PEEK_SCREENSHOT");
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
        let services = test_services();
        let window = cx
            .open_window(size(px(1000.0), px(700.0)), |window, cx| {
                cx.new(|cx| {
                    let root = RootView::new(services, true, PreviewScenario::Typical, window, cx);
                    root.sidebar.update(cx, |sidebar, cx| {
                        sidebar.conceal(cx);
                        sidebar.peek(window, cx);
                        if std::env::var_os("DIRI_PEEK_PINNED").is_some() {
                            sidebar.toggle(cx);
                        }
                    });
                    root
                })
            })
            .expect("preview window");
        cx.run_until_parked();
        cx.capture_screenshot(window.into())
            .expect("peek screenshot")
            .save(output)
            .expect("save peek screenshot");
        cx.update_window(window.into(), |_, window, _| window.remove_window())
            .expect("close preview window");
        cx.run_until_parked();
    }

    #[gpui::test]
    fn sidebar_peek_docks_the_same_panel_on_one_motion_curve(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| cx.set_reduce_motion(true));
        let services = test_services();
        let (root, cx) = cx.add_window_view(move |window, cx| {
            RootView::new(services, true, PreviewScenario::Typical, window, cx)
        });
        root.update_in(cx, |root, window, cx| {
            root.sidebar.update(cx, |sidebar, cx| {
                sidebar.conceal(cx);
                sidebar.peek(window, cx);
            });
        });
        cx.run_until_parked();
        let sidebar = root.read_with(cx, |root, _| root.sidebar.clone());
        root.update(cx, |root, cx| {
            cx.set_reduce_motion(false);
            root.sidebar.update(cx, |sidebar, cx| sidebar.toggle(cx));
        });
        root.read_with(cx, |root, cx| {
            assert_eq!(root.sidebar, sidebar);
            let width = sidebar.read(cx).width();
            assert_eq!(
                root.sidebar_panel_width, width,
                "the peek stays fully exposed when pinned"
            );
            assert!(
                root.sidebar_panel_slide.is_none(),
                "the panel must not replay its reveal"
            );
            let seam = root.sidebar_slide.expect("content makes room gradually");
            let float = root
                .sidebar_float_slide
                .expect("inset and corners ease into the dock");
            let halfway = Instant::now() + crate::seam::SEAM_SLIDE / 2;
            let occupied = seam.seam_at(width, halfway);
            let floating = float.seam_at(0.0, halfway);
            assert!(occupied > 0.0 && occupied < width);
            assert!(floating > 0.0 && floating < 1.0);
            assert!(
                (occupied / width + floating - 1.0).abs() < 0.001,
                "layout, inset, radius and shadow must share the same curve and clock"
            );
        });
    }

    #[gpui::test]
    fn inspector_shortcut_reopens_after_close_button(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| crate::commands::bind_keys(cx, &Default::default()));
        let services = test_services();
        let (root, cx) = cx.add_window_view(move |window, cx| {
            RootView::new(services, true, PreviewScenario::Artifacts, window, cx)
        });
        cx.simulate_resize(size(px(1200.0), px(800.0)));
        root.update_in(cx, |root, window, cx| {
            root.preview = false;
            root.inspector_open = true;
            root.inspector_seam = 440.0;
            let inspector = root.inspector.as_ref().unwrap();
            window.focus(&inspector.read(cx).focus_handle(cx), cx);
            cx.notify();
        });
        cx.run_until_parked();
        let close = cx.debug_bounds("INSPECTOR_CLOSE").expect("close button");
        cx.simulate_click(close.center(), Modifiers::default());
        cx.run_until_parked();
        assert!(!root.read_with(cx, |root, _| root.inspector_open));
        root.update(cx, |root, cx| {
            root.inspector_slide = None;
            root.inspector_seam = 0.0;
            cx.notify();
        });
        cx.run_until_parked();
        cx.simulate_keystrokes("cmd-shift-d");
        cx.run_until_parked();
        assert!(
            root.read_with(cx, |root, _| root.inspector_open),
            "shortcut must reopen after X removes the focused panel"
        );
    }

    #[gpui::test]
    fn notification_trigger_closes_an_open_panel_in_one_click(cx: &mut gpui::TestAppContext) {
        let services = test_services();
        let session = SidebarPreviewFixture::make(PreviewScenario::Typical)
            .list
            .sessions[0]
            .clone();
        {
            let mut store = services.store.store.write().expect("store");
            store.upsert_session(session.clone());
            store.select(session.id);
        }
        let (root, cx) = cx.add_window_view(move |window, cx| {
            RootView::new(services, true, PreviewScenario::Empty, window, cx)
        });
        cx.simulate_resize(size(px(1_000.0), px(700.0)));
        cx.run_until_parked();
        let trigger = cx
            .debug_bounds("notification-inbox-button")
            .expect("notification trigger");

        root.update_in(cx, |root, window, cx| {
            root.toggle_notifications(window, cx);
            assert!(root.notification_panel_open);
        });
        cx.simulate_click(trigger.center(), Modifiers::default());
        cx.run_until_parked();

        assert!(
            !root.read_with(cx, |root, _| root.notification_panel_open),
            "clicking the notification trigger again must close the panel without reopening it"
        );
    }

    #[gpui::test]
    fn auxiliary_close_control_does_not_cover_terminal_identity(cx: &mut gpui::TestAppContext) {
        let services = test_services();
        let mut parent = SidebarPreviewFixture::make(PreviewScenario::Typical)
            .list
            .sessions[0]
            .clone();
        parent.parent = None;
        let mut auxiliary = parent.clone();
        auxiliary.id = SessionId::new("auxiliary-terminal");
        auxiliary.kind = AgentKind::SHELL;
        auxiliary.parent = Some(parent.id.clone());
        auxiliary.title = crate::store::AUXILIARY_TERMINAL_TITLE.to_owned();
        {
            let mut store = services.store.store.write().expect("store");
            store.upsert_session(parent.clone());
            store.upsert_session(auxiliary);
            store.select(parent.id.clone());
            assert!(store.auxiliary_terminal_for(&parent.id).is_some());
            assert_eq!(store.selected_session_id(), Some(&parent.id));
        }
        let (root, cx) = cx.add_window_view(move |window, cx| {
            RootView::new(services, false, PreviewScenario::Empty, window, cx)
        });
        assert_eq!(
            root.read_with(cx, |root, _| root.auxiliary_id.clone()),
            Some(SessionId::new("auxiliary-terminal")),
            "fixture must mount the auxiliary terminal before the first async refresh"
        );
        cx.simulate_resize(size(px(1_000.0), px(700.0)));
        cx.run_until_parked();

        assert_eq!(
            root.read_with(cx, |root, _| root.auxiliary_id.clone()),
            Some(SessionId::new("auxiliary-terminal")),
            "fixture must mount the auxiliary terminal"
        );

        let identity = cx
            .debug_bounds("terminal-session-identity-auxiliary-terminal")
            .expect("auxiliary terminal identity");
        let close = cx
            .debug_bounds("close-auxiliary-terminal")
            .expect("auxiliary terminal close control");
        assert!(
            identity.right() <= close.left(),
            "the close control must occupy reserved title-bar space instead of covering {identity:?}"
        );
    }

    #[gpui::test]
    fn notification_panel_uses_the_settings_canvas_top_edge(cx: &mut gpui::TestAppContext) {
        let services = test_services();
        let (root, cx) = cx.add_window_view(move |window, cx| {
            RootView::new(services, false, PreviewScenario::Empty, window, cx)
        });
        cx.simulate_resize(size(px(1_000.0), px(700.0)));
        root.update_in(cx, |root, window, cx| {
            root.run_command(CommandId::OpenSettings, window, cx);
            root.toggle_notifications(window, cx);
        });
        cx.run_until_parked();
        cx.executor().advance_clock(Duration::from_millis(200));
        cx.run_until_parked();

        let settings = cx.debug_bounds("settings-shell").expect("settings shell");
        let trigger = cx
            .debug_bounds("notification-inbox-button")
            .expect("settings notification trigger");
        let panel = cx
            .debug_bounds("notification-panel")
            .expect("notification panel");
        assert_eq!(trigger.top(), settings.top() + px(7.0));
        assert!(
            panel.top() <= settings.top() + px(14.0)
                && panel.top() < settings.top() + px(Metrics::TITLE_BAR),
            "Settings has no workbench navbar, so the panel must enter from its canvas edge: {panel:?}"
        );

        cx.simulate_click(trigger.center(), Modifiers::default());
        cx.run_until_parked();
        assert!(
            !root.read_with(cx, |root, _| root.notification_panel_open),
            "the Settings notification trigger must close its open panel in one click"
        );
    }

    #[cfg(target_os = "macos")]
    #[gpui::test]
    fn browser_stays_visible_through_every_resize(cx: &mut gpui::TestAppContext) {
        let services = test_services();
        let (root, cx) = cx.add_window_view(move |window, cx| {
            RootView::new(services, true, PreviewScenario::Artifacts, window, cx)
        });
        root.update(cx, |root, cx| {
            root.inspector_open = true;
            root.inspector_seam = 440.0;
            root.browser
                .borrow_mut()
                .load("http://127.0.0.1:9/resize-fixture".into());
            root.inspector
                .as_ref()
                .unwrap()
                .update(cx, |inspector, cx| {
                    inspector.select_workspace(crate::inspector::WorkspaceSurface::Browser, cx);
                });
            assert!(root.browser_visible(false, 440.0, cx));
            root.resize_origin = Some((250.0, 250.0));
            assert!(
                root.browser_visible(false, 440.0, cx),
                "left sidebar resize hid the website"
            );
            root.resize_origin = None;
            root.inspector_resize_origin = Some((700.0, 440.0));
            assert!(
                root.browser_visible(false, 440.0, cx),
                "right sidebar resize hid the website"
            );
            root.inspector_resize_origin = None;
            root.terminal_resize_origin = Some((300.0, 300.0));
            assert!(root.browser_visible(false, 440.0, cx));
            root.terminal_resize_origin = None;
            assert!(
                !root.browser_visible(true, 440.0, cx),
                "launcher must cover native content"
            );
            assert!(
                !root.browser_visible(false, 0.0, cx),
                "a collapsed panel must not leave a native overlay"
            );
            // TestWindow has no AppKit handle; the separate native fixture
            // exercises painting and hit testing with an actual WKWebView.
            root.browser.borrow_mut().clear();
        });
    }

    #[test]
    fn picker_resolves_the_chosen_target_without_changing_the_active_id() {
        let fixture = SidebarPreviewFixture::make(PreviewScenario::Typical);
        let sessions = fixture.list.sessions;
        assert!(sessions.len() >= 2);
        let active = sessions[0].id.clone();
        let chosen = quote_target_id(&sessions, 1).expect("second target");
        assert_eq!(chosen, sessions[1].id);
        assert_eq!(
            active, sessions[0].id,
            "target lookup has no navigation side effect"
        );
    }

    #[test]
    fn quote_targets_exclude_shells_generic_terminals_archived_and_exited_sessions() {
        let fixture = SidebarPreviewFixture::make(PreviewScenario::Typical);
        let template = fixture.list.sessions[0].clone();
        let mut agent = template.clone();
        agent.kind = AgentKind::CODEX;
        agent.foreground_agent = None;
        agent.archived_at = None;
        agent.status = SessionStatus::Idle;
        assert!(is_quote_target(&agent));

        let mut shell = agent.clone();
        shell.kind = AgentKind::SHELL;
        assert!(!is_quote_target(&shell));

        let mut generic = agent.clone();
        generic.kind = AgentKind::generic("custom-command");
        assert!(!is_quote_target(&generic));

        let mut archived = agent.clone();
        archived.archived_at = Some(diri_proto::DateMillis(1.0));
        assert!(!is_quote_target(&archived));

        let mut exited = agent;
        exited.status = SessionStatus::Exited(diri_proto::ExitInfo {
            reason: diri_proto::ExitReason::Exited,
            code: Some(0),
            signal: None,
        });
        assert!(!is_quote_target(&exited));
    }
}
