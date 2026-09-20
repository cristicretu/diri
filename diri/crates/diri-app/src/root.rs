#[cfg(all(test, target_os = "macos"))]
#[path = "root/peek_profile.rs"]
mod peek_profile;
#[cfg(all(test, target_os = "macos"))]
mod project_agent_tests;
#[cfg(all(test, target_os = "macos"))]
mod theme_fade_frames;
#[cfg(all(test, target_os = "macos"))]
mod window_navigation_tests;
mod workspace_launches;
#[cfg(all(test, target_os = "macos"))]
mod workspace_palette_tests;

#[cfg(all(test, target_os = "macos"))]
mod gesture_acceptance_tests;
#[cfg(all(test, target_os = "macos"))]
mod gesture_schedule_profile;

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use diri_proto::{AgentKind, SessionId, SessionRecord, SessionStatus};
use diri_ui::{FloatingSurface, Ink, Metrics, Radius, SemanticColors, Typo};
use gpui::{
    Animation, AnimationExt, AnyElement, App, BoxShadow, Context, CursorStyle, DragMoveEvent,
    Entity, FocusHandle, Focusable, FontWeight, KeyContext, KeyDownEvent, KeyUpEvent,
    ModifiersChangedEvent, MouseButton, Render, StyleRefinement, Subscription, Task, Window,
    WindowBackgroundAppearance, deferred, div, ease_out_quint, prelude::*, px, rgba,
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
    ToggleSidebar, ToggleTabPeek,
};
use crate::external_drop::ExternalDropAction;
use crate::icons::{SymbolWeight, sf_symbol, sf_symbol_weighted};
use crate::inspector::{BrowserAction, InspectorEvent, WorkbenchInspector};
use crate::launcher::{LauncherEvent, LauncherOverlay};
#[cfg(target_os = "macos")]
use crate::macos::browser::NativeBrowser;
use crate::navigation::NavigationOverlay;
use crate::notifications::InAppBanner;
use crate::quote::Quote;
use crate::recovery::{RecoveryAction, RecoveryKind, RecoveryNotice};
use crate::seam::{SeamSlide, toggle_has_settled};
use crate::session_surfaces::SessionSurfaces;
use crate::sidebar::{PreviewScenario, Sidebar, SidebarEvent};
use crate::store::{SpawnOptions, WindowMaterial};
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

fn sync_system_theme(runtime: &crate::store::StoreRuntime, appearance: gpui::WindowAppearance) {
    let dark = matches!(
        appearance,
        gpui::WindowAppearance::Dark | gpui::WindowAppearance::VibrantDark
    );
    let mut store = runtime.store.write().expect("store lock");
    let mut candidate = store.preferences().clone();
    if !candidate.apply_system_theme(dark) {
        return;
    }
    let changed = store.update_preferences(|prefs| *prefs = candidate).is_ok();
    drop(store);
    if changed {
        runtime.publish_local_change();
    }
}

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
/// The platform window background that realizes a preferred material.
pub(crate) fn window_background(material: WindowMaterial) -> WindowBackgroundAppearance {
    match material {
        WindowMaterial::Glass => WindowBackgroundAppearance::Blurred,
        WindowMaterial::Opaque => WindowBackgroundAppearance::Opaque,
    }
}

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
    spawn_owner: crate::store::SpawnOwner,
    window_store: crate::store::WindowStore,
    launches_expanded: bool,
    launches_focus: FocusHandle,
    launch_cursor: Option<u64>,
    launch_scroll: gpui::ScrollHandle,
    active_workspace: Option<diri_proto::workspace::WorkspaceId>,
    workspace_error: Option<String>,
    workspace_workbench: Option<Entity<crate::workspace_workbench::WorkspaceWorkbench>>,
    sidebar: Entity<Sidebar>,
    terminal: Option<Entity<TerminalPane>>,
    navigation: Option<Entity<NavigationOverlay>>,
    session_surfaces: Option<Entity<SessionSurfaces>>,
    tab_pinch: crate::tab_peek::TabPinch,
    utility_surfaces: Option<Entity<UtilitySurfaces>>,
    usage_share_overlay_open: bool,
    launcher: Entity<LauncherOverlay>,
    inspector: Option<Entity<WorkbenchInspector>>,
    #[cfg(target_os = "macos")]
    browser: std::rc::Rc<std::cell::RefCell<NativeBrowser>>,
    services: Arc<AppServices>,
    focus: FocusHandle,
    /// A press on otherwise-unhandled titlebar chrome. Button presses stop the
    /// mouse-down before it bubbles here, so even a one-pixel move remains a
    /// button click rather than becoming a window drag.
    titlebar_drag_armed: bool,
    resize_origin: Option<(f32, f32)>,
    /// The sidebar open/close currently being painted, if any.
    sidebar_slide: Option<SeamSlide>,
    /// The sidebar seam width painted on the last frame. A new slide starts
    /// from this rather than from the settled width so it picks up wherever the
    /// previous frame left the panel.
    sidebar_seam: f32,
    tabs_slide: Option<SeamSlide>,
    tabs_seam: f32,
    tabs_target: f32,
    /// The panel is always mounted in one absolute slot. Only this exposure
    /// and its floating treatment change; the layout seam independently makes room.
    sidebar_panel_slide: Option<SeamSlide>,
    sidebar_panel_width: f32,
    sidebar_float_slide: Option<SeamSlide>,
    sidebar_float: f32,
    sidebar_floating: bool,
    sidebar_peek_dwell: Option<Task<()>>,
    /// The window material last pushed to the platform window, so a
    /// preference change re-applies it exactly once.
    applied_material: Option<WindowMaterial>,
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
    /// The system alert asking whether to close sessions, while it is up.
    close_prompt: Option<Task<()>>,
    /// The main window's viewport, for content that sizes to it while a
    /// panel paints it elsewhere.
    main_viewport: gpui::Size<gpui::Pixels>,
    notification_filter_unread: bool,
    notification_selected: usize,
    notification_scroll: gpui::UniformListScrollHandle,
    notification_scroller: diri_ui::ScrollerState,
    notification_options_open: bool,
    notification_focus: FocusHandle,
    notification_health: String,
    pending_notification_open: Option<(SessionId, Option<String>)>,
    last_quote_surface: QuoteSurface,
    /// Set when opening settings had to reveal a hidden sidebar to put its
    /// navigation somewhere, so closing settings can hide it again.
    sidebar_revealed_for_settings: bool,
    preview: bool,
    preview_scenario: PreviewScenario,
    #[cfg(target_os = "macos")]
    menu_bar: Option<NativeMenuBar>,
    #[cfg(target_os = "macos")]
    notifier: std::rc::Rc<NativeNotifier>,
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
        Self::new_with_workspace(services, preview, preview_scenario, None, window, cx)
    }

    pub(crate) fn native_window_context(
        &self,
        window: &Window,
        cx: &App,
    ) -> crate::NativeWindowContext {
        let mut placement = crate::current_window_placement(window, cx);
        placement.x += 28.0;
        placement.y += 28.0;
        placement.mode = crate::store::WindowMode::Windowed;
        crate::NativeWindowContext {
            workspace: self.window_workspace(),
            selected: self.window_session(),
            placement,
            policy: crate::window_restore::RestorePolicy::FRAME_ONLY,
        }
    }

    /// This window as the next launch should bring it back.
    pub(crate) fn saved_window(&self, window: &Window, cx: &App) -> crate::store::SavedWindow {
        crate::store::SavedWindow {
            placement: crate::current_window_placement(window, cx),
            workspace: self.window_workspace(),
            selected_session: self.window_session(),
        }
    }

    pub(crate) fn window_workspace(&self) -> Option<diri_proto::workspace::WorkspaceId> {
        self.active_workspace.clone()
    }

    pub(crate) fn new_with_workspace(
        services: Arc<AppServices>,
        preview: bool,
        preview_scenario: PreviewScenario,
        workspace_override: Option<Option<diri_proto::workspace::WorkspaceId>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new_with_selection(
            services,
            preview,
            preview_scenario,
            workspace_override,
            None,
            window,
            cx,
        )
    }

    pub(crate) fn window_session(&self) -> Option<diri_proto::SessionId> {
        self.window_store
            .read()
            .expect("store")
            .selected_session_id()
            .cloned()
    }

    pub(crate) fn new_with_selection(
        services: Arc<AppServices>,
        preview: bool,
        preview_scenario: PreviewScenario,
        workspace_override: Option<Option<diri_proto::workspace::WorkspaceId>>,
        selected_override: Option<Option<diri_proto::SessionId>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        if !preview {
            sync_system_theme(&services.store, window.appearance());
        }
        let appearance_observer = (!preview).then(|| {
            let runtime = Arc::clone(&services.store);
            window.observe_window_appearance(move |window, _| {
                sync_system_theme(&runtime, window.appearance())
            })
        });
        let sidebar_runtime = (!preview).then(|| Arc::clone(&services.store));
        let sidebar = cx.new(|cx| {
            let mut sidebar = Sidebar::new(sidebar_runtime, preview, preview_scenario, cx);
            if let Some(workspace) = &workspace_override {
                sidebar.set_initial_workspace(workspace.clone());
            }
            if let Some(selected) = &selected_override {
                sidebar.set_initial_session(selected.clone());
            }
            sidebar.set_surface_in_parent();
            sidebar
        });
        let window_store = if preview {
            crate::store::WindowStore::from_canonical(services.store.store.clone())
        } else {
            sidebar.read(cx).window_store()
        };
        cx.on_release(|this, _| this.window_store.close_context())
            .detach();
        let terminal = (!preview || preview_scenario == PreviewScenario::Empty).then(|| {
            let runtime = Arc::clone(&services.store);
            let tokio = Arc::clone(&services.tokio);
            cx.new(|cx| {
                TerminalPane::new_for_window(runtime, tokio, window_store.clone(), window, cx)
            })
        });
        let navigation = (!preview).then(|| {
            let runtime = Arc::clone(&services.store);
            cx.new(|cx| {
                let mut navigation =
                    NavigationOverlay::new(runtime, Arc::clone(&services.tokio), window, cx);
                navigation.set_window_store(window_store.clone());
                navigation
            })
        });
        let session_surfaces = (!preview).then(|| {
            let runtime = Arc::clone(&services.store);
            cx.new(|cx| {
                let mut surfaces =
                    SessionSurfaces::new(runtime, Some(services.tokio.handle().clone()), cx);
                surfaces.set_window_store(window_store.clone());
                surfaces
            })
        });
        let utility_surfaces = (!preview).then(|| {
            let runtime = Arc::clone(&services.store);
            let tokio = Arc::clone(&services.tokio);
            let updates = services.updates.clone();
            cx.new(|cx| {
                let mut surfaces = UtilitySurfaces::new(runtime, tokio, updates, window, cx);
                surfaces.set_window_store(window_store.clone(), cx);
                surfaces
            })
        });
        if let Some(surfaces) = &utility_surfaces {
            cx.subscribe(
                surfaces,
                |this, _, _: &crate::surface_shell::UtilitySurfacesEvent, cx| {
                    this.sidebar
                        .update(cx, |_, cx| cx.emit(SidebarEvent::SessionActivated));
                },
            )
            .detach();
        }
        let launcher = cx.new(|cx| {
            let mut launcher = LauncherOverlay::new(Arc::clone(&services), preview, cx);
            launcher.set_window_store(window_store.clone());
            launcher
        });
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
                TerminalPaneEvent::Feedback { message } => {
                    this.show_quote_feedback("Terminal", message.clone(), cx);
                }
                TerminalPaneEvent::ExternalDropFeedback { message } => {
                    this.show_quote_feedback("Dropped files", message.clone(), cx);
                }
            })
            .detach();
        }
        if let Some(navigation) = &navigation {
            cx.subscribe_in(
                navigation,
                window,
                |this, _, command: &crate::palette_workspace::WorkspaceCommand, window, cx| {
                    let handled = this.sidebar.update(cx, |sidebar, cx| {
                        sidebar.run_workspace_palette(command.clone(), window, cx)
                    });
                    if !handled {
                        this.show_quote_feedback(
                            "Workspace changed",
                            "The selected target is no longer available. Open the command palette to choose again.",
                            cx,
                        );
                    }
                },
            ).detach();
        }
        cx.observe(&sidebar, |_, sidebar, cx| {
            if sidebar.read(cx).project_picker_active() {
                cx.notify();
            }
        })
        .detach();
        cx.subscribe_in(&sidebar, window, |this, _, event, window, cx| {
            if let SidebarEvent::WorkspaceActivated(id) = event {
                this.sidebar
                    .update(cx, |sidebar, _| sidebar.sync_focused_agent_selection());
                this.activate_saved_workspace(id.clone(), window, cx);
            }
            if matches!(event, SidebarEvent::WorkspaceTabActivated) {
                this.sidebar
                    .update(cx, |sidebar, _| sidebar.sync_focused_agent_selection());
                if let Some(workbench) = &this.workspace_workbench {
                    workbench.update(cx, |workbench, cx| workbench.focus(window, cx));
                }
                cx.notify();
            }
            if let SidebarEvent::AccountAction(profile) = event {
                let services = this.services.clone();
                this.sidebar.update(cx, |sidebar, cx| {
                    sidebar.account_menu_action(profile.clone(), services, cx);
                });
            }
            if matches!(event, SidebarEvent::ManageAccounts)
                && let Some(surfaces) = &this.utility_surfaces
            {
                surfaces.update(cx, |surfaces, cx| {
                    surfaces.open_settings(cx);
                    surfaces.open_settings_tab(crate::settings::SettingsTab::Accounts, cx);
                    surfaces.focus_handle(cx).focus(window, cx);
                });
            }
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
            if matches!(
                event,
                SidebarEvent::SessionActivated | SidebarEvent::ProjectLayoutUnavailable
            ) {
                let opening_project = matches!(event, SidebarEvent::SessionActivated)
                    && this
                        .sidebar
                        .update(cx, |sidebar, cx| sidebar.open_selected_project_agent(cx));
                if !opening_project && this.active_workspace.is_some() {
                    this.sidebar
                        .update(cx, |sidebar, cx| sidebar.activate_workspace(None, cx));
                    this.activate_saved_workspace(None, window, cx);
                }
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
                if let Some(terminal) = this.active_terminal(cx) {
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
            if matches!(
                event,
                SidebarEvent::VisibilityChanged | SidebarEvent::TabOrientationChanged
            ) {
                this.sidebar_peek_dwell = None;
                this.sidebar_floating = false;
                if matches!(event, SidebarEvent::TabOrientationChanged) {
                    // A terminal receives its final viewport immediately. Switching
                    // axes must commit the chrome in the same frame, otherwise a
                    // full-width workspace is clipped by the old sidebar width.
                    this.sidebar_slide = None;
                    this.sidebar_panel_slide = None;
                    this.sidebar_float_slide = None;
                    this.sidebar_seam = this.settled_sidebar_seam(cx);
                    this.sidebar_panel_width = this.sidebar_seam;
                    this.sidebar_float = 0.0;
                    this.tabs_slide = None;
                    this.tabs_target = if this.sidebar.read(cx).horizontal_tabs_visible() {
                        crate::tab_navigation::TAB_STRIP_HEIGHT
                    } else {
                        0.0
                    };
                    this.tabs_seam = this.tabs_target;
                } else {
                    this.begin_sidebar_slide(cx);
                }
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
                let share = surfaces.read(cx).is_usage_share_open();
                if this.usage_share_overlay_open != share {
                    this.usage_share_overlay_open = share;
                    cx.notify();
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
                    InspectorEvent::WorkspaceChanged(surface)
                    | InspectorEvent::WorkspaceRestored(surface) => {
                        let focus_workspace = matches!(event, InspectorEvent::WorkspaceChanged(_));
                        if focus_workspace {
                            window.focus(&this.focus, cx);
                        }
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
                                if blank && focus_workspace {
                                    inspector.focus_browser_address(window, cx);
                                }
                            });
                        }
                        if let Some(terminal) = &this.auxiliary_terminal
                            && *surface == crate::inspector::WorkspaceSurface::Terminal
                            && focus_workspace
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
        crate::application_notifications::install(services.clone(), preview, preview_scenario, cx);
        #[cfg(target_os = "macos")]
        let notifier = crate::application_notifications::notifier(cx);

        let activation = cx.observe_window_activation(window, move |this, window, cx| {
            if !window.is_window_active() {
                this.tab_pinch.cancel();
                if let Some(surfaces) = &this.session_surfaces {
                    surfaces.update(cx, |s, cx| s.cancel_tab_peek_immediately(cx));
                }
            }
            this.window_store
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
                    cx.spawn(async move |this, cx| {
                        loop {
                            match changes.recv().await {
                                Ok(())
                                | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                                    terminal.update(cx, |terminal, cx| {
                                        terminal.resident_buffers(cx);
                                    });
                                    if this
                                        .update(cx, |this, cx| {
                                            let buffers = this.preview_buffers(cx);
                                            surfaces.update(cx, |surfaces, _| {
                                                surfaces.sync_resident_buffers(buffers)
                                            });
                                        })
                                        .is_err()
                                    {
                                        return;
                                    }
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
                        if crate::floating::update_in_owner(&this, cx, |this, window, cx| {
                            let launched = {
                                let mut store = this.window_store.write().expect("store");
                                let before = store.selected_session_id().cloned();
                                store.accept_completed_launches(true);
                                (store.selected_session_id() != before.as_ref())
                                    .then(|| store.selected_session_id().cloned())
                                    .flatten()
                            };
                            if let Some(id) = launched {
                                this.open_workspace_launch_session(id, window, cx);
                            }
                            let actions = this
                                .window_store
                                .write()
                                .expect("store")
                                .take_window_actions();
                            for action in actions {
                                window.activate_window();
                                match action {
                                    crate::store::WindowAction::Focus => {}
                                    crate::store::WindowAction::OpenNotification {
                                        session,
                                        notification,
                                    } => this.open_notification(
                                        session,
                                        Some(notification),
                                        window,
                                        cx,
                                    ),
                                    crate::store::WindowAction::Select(id) => {
                                        this.open_workspace_launch_session(id, window, cx);
                                    }
                                    crate::store::WindowAction::Close(id) => this
                                        .window_store
                                        .write()
                                        .expect("store")
                                        .request_close(vec![id]),
                                    crate::store::WindowAction::OpenLauncher => {
                                        this.open_launcher(&OpenLauncher, window, cx)
                                    }
                                    crate::store::WindowAction::OpenSettings => {
                                        this.run_command(CommandId::OpenSettings, window, cx)
                                    }
                                    crate::store::WindowAction::Spawn(kind) => {
                                        this.spawn(kind);
                                    }
                                }
                            }
                            // This loop runs on every store change; probe under
                            // a read lock so only the rare menu-bar request
                            // pays for exclusive access.
                            let pending = this
                                .window_store
                                .read()
                                .expect("session store lock poisoned")
                                .has_pending_ui_request();
                            let (open_launcher, open_settings) = if pending {
                                let mut store = this
                                    .window_store
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
                            this.sync_workspace_spawn_context(cx);
                            this.sync_inspector_context(cx);
                            this.sync_auxiliary_terminal(window, cx);
                            let error = this
                                .window_store
                                .read()
                                .expect("store")
                                .workspace_catalog()
                                .error
                                .clone();
                            if error != this.workspace_error {
                                this.workspace_error = error.clone();
                                if let Some(error) = error {
                                    this.show_quote_feedback(
                                        "Workspace change was not saved",
                                        error,
                                        cx,
                                    );
                                }
                            }
                            cx.notify();
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
        let tabs_seam = if sidebar.read(cx).horizontal_tabs_visible() {
            crate::tab_navigation::TAB_STRIP_HEIGHT
        } else {
            0.0
        };
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
                if crate::floating::update_in_owner(&this, cx, |this, _window, cx| {
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
                .is_none()
                {
                    return;
                }
            }
        });
        if let Some(surfaces) = &session_surfaces {
            cx.subscribe_in(
                surfaces,
                window,
                |this, _, _: &crate::session_surfaces::TabPeekActivated, window, cx| {
                    if let Some(terminal) = this.active_terminal(cx) {
                        terminal.update(cx, |terminal, cx| terminal.focus(window, cx));
                    }
                    this.sync_auxiliary_terminal(window, cx);
                },
            )
            .detach();
        }
        let peek_observer = session_surfaces.as_ref().map(|surfaces| {
            let mut was_visible = false;
            let mut previous_offset = 0.0;
            cx.observe(surfaces, move |_this, surfaces, cx| {
                let visible = surfaces.read(cx).tab_peek_visible();
                let offset = surfaces.read(cx).tab_peek_offset(cx);
                if was_visible && !visible {
                    _this.tab_pinch.cancel();
                }
                // Output only repaints the preview entity. Root layout needs
                // invalidation solely when terminal placement changes.
                if was_visible != visible || previous_offset != offset {
                    cx.notify();
                }
                was_visible = visible;
                previous_offset = offset;
            })
        });
        let peek_output =
            terminal
                .as_ref()
                .zip(session_surfaces.as_ref())
                .map(|(terminal, surfaces)| {
                    let surfaces = surfaces.clone();
                    cx.observe(terminal, move |this, _, cx| {
                        if surfaces.read(cx).tab_peek_visible() {
                            let buffers = this.preview_buffers(cx);
                            surfaces.update(cx, |surface, cx| {
                                surface.sync_resident_buffers(buffers);
                                cx.notify();
                            });
                        }
                    })
                });
        let mut root = Self {
            spawn_owner: window_store.owner(),
            window_store,
            launches_expanded: false,
            launches_focus: cx.focus_handle(),
            launch_cursor: None,
            launch_scroll: gpui::ScrollHandle::new(),
            active_workspace: None,
            workspace_error: None,
            workspace_workbench: None,
            sidebar,
            terminal,
            navigation,
            session_surfaces,
            tab_pinch: Default::default(),
            utility_surfaces,
            usage_share_overlay_open: false,
            launcher,
            inspector,
            #[cfg(target_os = "macos")]
            browser,
            services,
            focus: cx.focus_handle(),
            titlebar_drag_armed: false,
            resize_origin: None,
            sidebar_slide: None,
            sidebar_panel_slide: None,
            sidebar_panel_width: sidebar_seam,
            sidebar_float_slide: None,
            sidebar_float: 0.0,
            sidebar_floating: false,
            sidebar_peek_dwell: None,
            sidebar_seam,
            applied_material: None,
            tabs_slide: None,
            tabs_seam,
            tabs_target: tabs_seam,
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
            close_prompt: None,
            main_viewport: gpui::Size::default(),
            notification_filter_unread: true,
            notification_selected: 0,
            notification_scroll: gpui::UniformListScrollHandle::new(),
            notification_scroller: diri_ui::ScrollerState::new(),
            notification_options_open: false,
            notification_focus: cx.focus_handle(),
            pending_notification_open: None,
            notification_health:
                "Use Test alert to check macOS delivery. Notifications remain available here."
                    .into(),
            last_quote_surface: QuoteSurface::default(),
            sidebar_revealed_for_settings: false,
            preview,
            preview_scenario,
            #[cfg(target_os = "macos")]
            menu_bar,
            #[cfg(target_os = "macos")]
            notifier,
            _subscriptions: std::iter::once(activation)
                .chain(bounds_observer)
                .chain(appearance_observer)
                .chain(peek_observer)
                .chain(peek_output)
                .collect(),
            _service_events: service_events,
            _surface_sync: surface_sync,
            _workbench_sync: workbench_sync,
            #[cfg(target_os = "macos")]
            _browser_state_sync: browser_state_sync,
        };
        root.sync_auxiliary_terminal(window, cx);
        let saved_workspace = workspace_override.unwrap_or_else(|| {
            root.services
                .store
                .store
                .read()
                .expect("store")
                .preferences()
                .active_workspace
                .clone()
        });
        if saved_workspace.is_some() && !preview {
            root.activate_saved_workspace(saved_workspace, window, cx);
        }
        if !preview {
            // Do not rely on AppKit emitting a move/resize after the observer
            // is installed: even an untouched first launch should become the
            // placement restored by the next launch.
            root.window_bounds_changed(window, cx);
        }
        root.window_store
            .write()
            .expect("store")
            .set_active(window.is_window_active());
        root.sync_inspector_context(cx);
        root
    }

    fn window_bounds_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let placement = crate::current_window_placement(window, cx);
        self.window_store
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
            let _ = crate::floating::update_in_owner(&this, cx, |this, _window, _cx| {
                this.window_bounds_save.take();
                if let Err(error) = this
                    .window_store
                    .write()
                    .expect("session store lock poisoned")
                    .persist_preferences()
                {
                    eprintln!("diri: could not remember window placement: {error}");
                }
            });
        }));
    }

    fn activate_saved_workspace(
        &mut self,
        id: Option<diri_proto::workspace::WorkspaceId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.active_workspace != id {
            self.window_store
                .write()
                .expect("store")
                .bump_navigation_context();
        }
        self.active_workspace = id;
        self.sync_workspace_spawn_context(cx);
        if let Some(workbench) = &self.workspace_workbench {
            workbench.update(cx, |workbench, cx| workbench.deactivate(cx));
        }
        if self.active_workspace.is_some() {
            if let Some(auxiliary) = &self.auxiliary_terminal {
                auxiliary.update(cx, |terminal, _| terminal.release_layout_control());
            }
            if let Some(terminal) = &self.terminal {
                terminal.update(cx, |terminal, _| terminal.release_layout_control());
            }
            if self.workspace_workbench.is_none() {
                let runtime = self.services.store.clone();
                let tokio = self.services.tokio.clone();
                let workbench = cx.new(|cx| {
                    crate::workspace_workbench::WorkspaceWorkbench::new(runtime, tokio, window, cx)
                });
                workbench.update(cx, |workbench, cx| {
                    workbench.set_window_store(self.window_store.clone(), cx);
                });
                cx.subscribe_in(
                    &workbench,
                    window,
                    |this, _, event, window, cx| match event {
                        crate::workspace_workbench::WorkspaceWorkbenchEvent::Notice(message) => {
                            this.show_quote_feedback("Workspace", message.clone(), cx)
                        }
                        crate::workspace_workbench::WorkspaceWorkbenchEvent::RequestSplit {
                            tab,
                            pane,
                            edge,
                        } => this.sidebar.update(cx, |sidebar, cx| {
                            sidebar.choose_split_session(
                                tab.clone(),
                                pane.clone(),
                                *edge,
                                window,
                                cx,
                            )
                        }),
                        crate::workspace_workbench::WorkspaceWorkbenchEvent::Terminal(
                            TerminalPaneEvent::OpenFileReference { reference, cwd, .. },
                        ) => {
                            this.reveal_inspector(cx);
                            if let Some(inspector) = &this.inspector {
                                inspector.update(cx, |inspector, cx| {
                                    inspector.open_file_reference(
                                        cwd.clone(),
                                        reference.clone(),
                                        cx,
                                    )
                                });
                            }
                        }
                        crate::workspace_workbench::WorkspaceWorkbenchEvent::Terminal(
                            TerminalPaneEvent::Feedback { message },
                        ) => this.show_quote_feedback("Terminal", message.clone(), cx),
                        crate::workspace_workbench::WorkspaceWorkbenchEvent::Terminal(
                            TerminalPaneEvent::ExternalDropFeedback { message },
                        ) => this.show_quote_feedback("Dropped files", message.clone(), cx),
                        crate::workspace_workbench::WorkspaceWorkbenchEvent::Terminal(
                            TerminalPaneEvent::ContinueAccount(id),
                        ) => {
                            if let Some(surfaces) = &this.utility_surfaces {
                                surfaces.update(cx, |surfaces, cx| {
                                    surfaces.open_account_continuation(id.clone(), window, cx)
                                });
                            }
                        }
                    },
                )
                .detach();
                cx.observe_in(&workbench, window, |this, _, window, cx| {
                    this.sidebar
                        .update(cx, |sidebar, _| sidebar.sync_focused_agent_selection());
                    this.sync_inspector_context(cx);
                    this.sync_auxiliary_terminal(window, cx);
                    if let Some(surfaces) = &this.session_surfaces
                        && surfaces.read(cx).tab_peek_visible()
                    {
                        let buffers = this.preview_buffers(cx);
                        surfaces.update(cx, |surfaces, cx| {
                            surfaces.sync_resident_buffers(buffers);
                            cx.notify();
                        });
                    }
                })
                .detach();
                self.workspace_workbench = Some(workbench);
            }
        } else {
            if let Some(workbench) = &self.workspace_workbench {
                workbench.update(cx, |workbench, cx| workbench.deactivate(cx));
            }
            if let Some(terminal) = &self.terminal {
                terminal.update(cx, |terminal, cx| terminal.focus(window, cx));
            }
        }
        self.sync_inspector_context(cx);
        self.sync_auxiliary_terminal(window, cx);
        cx.notify();
    }

    /// Runs `f` against the main window even from a panel handler.
    fn in_main_window(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        f: impl FnOnce(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) {
        crate::floating::in_main_window(self, window, cx, f);
    }

    fn colors(&self) -> SemanticColors {
        let store = self
            .window_store
            .read()
            .expect("session store lock poisoned");
        crate::app_theme::colors_in(&store)
    }

    /// Pushes the preferred window material to the platform window when it
    /// changes. The window opens with the right material already; this only
    /// follows the settings toggle afterwards.
    fn sync_window_material(&mut self, window: &Window) {
        let material = self
            .services
            .store
            .store
            .read()
            .expect("session store lock poisoned")
            .preferences()
            .window_material;
        if self.applied_material == Some(material) {
            return;
        }
        window.set_background_appearance(window_background(material));
        self.applied_material = Some(material);
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

    fn preview_buffers(
        &self,
        cx: &App,
    ) -> std::collections::HashMap<SessionId, diri_term::element::SharedGridBuffer> {
        let mut buffers = self
            .terminal
            .as_ref()
            .map(|terminal| terminal.read(cx).resident_preview_buffers())
            .unwrap_or_default();
        if let Some(workbench) = &self.workspace_workbench {
            buffers.extend(workbench.read(cx).resident_preview_buffers(cx));
        }
        buffers
    }
    fn active_session_id(&self, cx: &App) -> Option<SessionId> {
        if self.active_workspace.is_some() {
            self.workspace_workbench
                .as_ref()
                .and_then(|workbench| workbench.read(cx).focused_session_id())
        } else {
            self.window_store
                .read()
                .expect("store")
                .selected_session_id()
                .cloned()
        }
    }

    fn sync_inspector_context(&mut self, cx: &mut Context<Self>) {
        let selected = self.active_session_id(cx);
        self.window_store
            .write()
            .expect("store")
            .set_visible_session(selected.clone());
        let context = Some(selected);
        if let Some(inspector) = &self.inspector {
            inspector.update(cx, |inspector, cx| {
                inspector.set_session_context(context, cx)
            });
        }
    }

    fn active_terminal(&self, cx: &App) -> Option<Entity<TerminalPane>> {
        if self.active_workspace.is_some() {
            self.workspace_workbench
                .as_ref()
                .and_then(|workbench| workbench.read(cx).focused_terminal())
        } else {
            self.terminal.clone()
        }
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
        if let Some(terminal) = self.active_terminal(cx)
            && terminal.read(cx).is_focused(window)
        {
            return Some(QuoteSurface::PrimaryTerminal);
        }
        None
    }

    fn quote_from_surface(&self, surface: QuoteSurface, cx: &App) -> Option<Quote> {
        match surface {
            QuoteSurface::PrimaryTerminal => self
                .active_terminal(cx)
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
                .active_terminal(cx)
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
                self.active_terminal(cx)
                    .as_ref()
                    .and_then(|terminal| terminal.read(cx).quote_selection())
            })
    }

    fn quote_targets(&self) -> Vec<SessionRecord> {
        self.window_store
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
            let active = self.active_session_id(cx);
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
        let target = self.active_session_id(cx);
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
            .window_store
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

    pub(crate) fn toggle_tab_peek(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.launcher.read(cx).is_open()
            || self
                .utility_surfaces
                .as_ref()
                .is_some_and(|view| view.read(cx).is_open())
            || self.notification_panel_open
            || self.sidebar.read(cx).pending_close_copy().is_some()
            || self.quote_target_picker.is_some()
        {
            return;
        }
        if let Some(surfaces) = &self.session_surfaces {
            let buffers = self.preview_buffers(cx);
            surfaces.update(cx, |surfaces, cx| {
                surfaces.sync_resident_buffers(buffers);
                surfaces.toggle_tab_peek(cx);
                surfaces.sync_tab_peek_focus(window, cx);
            });
        }
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(surfaces) = &self.session_surfaces
            && surfaces.read(cx).tab_peek_visible()
        {
            if commands::matches_keystroke(CommandId::ToggleTabOrientation, &event.keystroke) {
                self.run_command(CommandId::ToggleTabOrientation, window, cx);
            } else {
                surfaces.update(cx, |surfaces, cx| {
                    surfaces.handle_key_down(event, window, cx)
                });
            }
            cx.stop_propagation();
            return;
        }
        // The close dialog overlays the focused surface without taking its
        // focus. Handle its keys first so they cannot reach the terminal or
        // sidebar beneath it, and focus is preserved when closing is canceled.
        if self.sidebar.read(cx).pending_close_copy().is_some() {
            self.sidebar
                .update(cx, |sidebar, cx| match event.keystroke.key.as_str() {
                    "enter" => sidebar.confirm_close(cx),
                    "escape" => sidebar.cancel_close(cx),
                    _ => {}
                });
            cx.stop_propagation();
            return;
        }
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
        if let Some(command) = crate::workspace_workbench::PaneCommand::from_id(command) {
            if (self.sidebar.read(cx).workspace_menu_is_open()
                || self.sidebar.read(cx).project_picker_active())
                || self.launcher.read(cx).is_open()
                || self
                    .navigation
                    .as_ref()
                    .is_some_and(|view| view.read(cx).is_open())
                || self
                    .utility_surfaces
                    .as_ref()
                    .is_some_and(|view| view.read(cx).is_open())
                || self
                    .session_surfaces
                    .as_ref()
                    .is_some_and(|view| view.read(cx).tab_peek_visible())
                || (self.launches_expanded && self.launches_focus.contains_focused(window, cx))
                || self.quote_target_picker.is_some()
            {
                return;
            }
            if self.active_workspace.is_some()
                && let Some(workbench) = &self.workspace_workbench
            {
                workbench.update(cx, |workbench, cx| {
                    workbench.execute_command(command, window, cx)
                });
            } else {
                self.show_quote_feedback(
                    "Workspace panes",
                    "Choose a workspace to arrange its panes.",
                    cx,
                );
            }
            return;
        }
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
                self.sync_workspace_spawn_context(cx);
                self.remember_quote_surface(window, cx);
                if let Some(navigation) = &self.navigation {
                    navigation.update(cx, |navigation, cx| {
                        navigation.toggle_command_palette(&ToggleCommandPalette, window, cx);
                    });
                }
            }
            CommandId::ToggleQuickOpen => {
                self.sync_workspace_spawn_context(cx);
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
            CommandId::ReviewLaunches => {
                self.launches_expanded = true;
                window.focus(&self.launches_focus, cx);
                cx.notify();
            }
            CommandId::ToggleTabPeek => self.toggle_tab_peek(window, cx),
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
            CommandId::ToggleTabOrientation
            | CommandId::HorizontalTabs
            | CommandId::VerticalTabs => {
                let orientation = match command {
                    CommandId::HorizontalTabs => crate::store::TabOrientation::Horizontal,
                    CommandId::VerticalTabs => crate::store::TabOrientation::Vertical,
                    _ => self.sidebar.read(cx).tab_orientation().toggled(),
                };
                let navigation_focused = self.sidebar.read(cx).is_focused(window);
                if let Err(error) = self.sidebar.update(cx, |sidebar, cx| {
                    sidebar.set_tab_orientation(orientation, cx)
                }) {
                    self.show_quote_feedback(
                        "Tab orientation",
                        format!("Could not save tab orientation: {error}"),
                        cx,
                    );
                    return;
                }
                if orientation == crate::store::TabOrientation::Horizontal
                    && navigation_focused
                    && !self.sidebar.read(cx).is_visible()
                    && let Some(terminal) = &self.terminal
                {
                    terminal.update(cx, |terminal, cx| terminal.focus(window, cx));
                }
                cx.notify();
            }
            CommandId::ToggleSidebar => {
                if self.sidebar.read(cx).tab_orientation()
                    == crate::store::TabOrientation::Horizontal
                {
                    if let Err(error) = self
                        .sidebar
                        .update(cx, |sidebar, cx| sidebar.toggle_horizontal_tabs(window, cx))
                    {
                        self.show_quote_feedback(
                            "Tab bar",
                            format!("Could not save tab bar visibility: {error}"),
                            cx,
                        );
                    }
                    cx.notify();
                } else {
                    self.sidebar.update(cx, |sidebar, cx| sidebar.toggle(cx));
                }
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
        let workspace_target = self.workspace_spawn_target();
        if self.preview {
            return false;
        }
        let mut store = self
            .window_store
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
                        workspace_target,
                        host,
                        ..SpawnOptions::default()
                    },
                );
            }
            None => store.spawn_shell(SpawnOptions {
                workspace_target,
                ..SpawnOptions::default()
            }),
        }
        true
    }

    fn spawn_default(&self) -> bool {
        let workspace_target = self.workspace_spawn_target();
        if self.preview {
            return false;
        }
        let mut store = self
            .window_store
            .write()
            .expect("session store lock poisoned");
        let host = store.default_spawn_host();
        store.spawn_default(SpawnOptions {
            workspace_target,
            host,
            ..SpawnOptions::default()
        })
    }

    fn open_auxiliary_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.preview {
            return false;
        }
        self.sync_inspector_context(cx);
        if let Some(inspector) = &self.inspector
            && (self.active_workspace.is_some() || inspector.read(cx).workspace_needs_terminal())
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
        let selected = self.active_session_id(cx);
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
                .window_store
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
        if let Some(parent) = self.active_session_id(cx) {
            self.collapsed_auxiliary_parents.insert(parent);
        }
        self.auxiliary_terminal = None;
        self.auxiliary_id = None;
        self.auxiliary_parent = None;
        self.auxiliary_spawn_parent = None;
        if let Some(inspector) = &self.inspector {
            inspector.update(cx, |inspector, cx| inspector.set_terminal_surface(None, cx));
        }
        if let Some(primary) = self.active_terminal(cx) {
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
        let selected = self.active_session_id(cx);
        let (selected, auxiliary, spawn_pending) = {
            let mut store = self
                .window_store
                .write()
                .expect("session store lock poisoned");

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
            terminal.update(cx, |terminal, _| {
                terminal.set_window_store(self.window_store.clone())
            });
            if let (Some(navigation), Some(utility_surfaces)) =
                (&self.navigation, &self.utility_surfaces)
            {
                terminal.update(cx, |terminal, _| {
                    terminal.set_shell_entities(navigation.clone(), utility_surfaces.clone());
                });
            }
            cx.observe(&terminal, |_, _, cx| cx.notify()).detach();
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
            .window_store
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
            self.window_store
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
        self.sync_workspace_spawn_context(cx);
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
        self.sync_workspace_spawn_context(cx);
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
        if self
            .session_surfaces
            .as_ref()
            .is_some_and(|s| s.read(cx).tab_peek_visible())
        {
            cx.stop_propagation();
            return;
        }
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
            .window_store
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
            .window_store
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
            .window_store
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
        // On macOS fullscreen, window bounds retain the windowed restore
        // rectangle. Terminal layout must follow the live drawable viewport.
        let viewport_size = window.viewport_size();
        let sidebar_width = if visible_sidebar {
            self.sidebar.read(cx).width()
        } else {
            0.0
        };
        let card_width =
            (f32::from(viewport_size.width) - sidebar_width - inspector_width).max(0.0);
        let tabs_height = if self.sidebar.read(cx).horizontal_tabs_visible() {
            crate::tab_navigation::TAB_STRIP_HEIGHT
        } else {
            0.0
        };
        let card_height = (f32::from(viewport_size.height) - tabs_height).max(0.0);
        if self.tabs_target != tabs_height {
            self.tabs_target = tabs_height;
            self.tabs_slide = (!cx.reduce_motion())
                .then(|| SeamSlide::begin(self.tabs_seam, tabs_height))
                .flatten();
        }
        self.tabs_seam = advance_seam(&mut self.tabs_slide, tabs_height, Instant::now(), window);
        let selected = self
            .window_store
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
        let peek_offset = self
            .session_surfaces
            .as_ref()
            .map_or(0.0, |surfaces| surfaces.read(cx).tab_peek_offset(cx));
        if let Some(surfaces) = &self.session_surfaces {
            let buffers = self.preview_buffers(cx);
            surfaces.update(cx, |surfaces, cx| {
                surfaces.sync_resident_buffers(buffers);
                surfaces.set_tab_peek_region(sidebar_width, tabs_height, card_width, cx);
                surfaces.set_workspace_peek(
                    self.active_workspace.clone(),
                    crate::workspace_geometry::Rect {
                        width: card_width,
                        height: card_height,
                        ..Default::default()
                    },
                    cx,
                );
            });
        }
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
            .bg(terminal.work_surface_nested())
            .overflow_hidden()
            .text_color(terminal.primary);

        // Moving the terminal exposes the parent, whose glass fill is clear.
        // Give that strip the same single surface tint as the terminal instead
        // of exposing the much lighter window/desktop backdrop beneath it.
        if peek_offset > 0.0 {
            card = card.child(
                div()
                    .id("tab-peek-terminal-backdrop")
                    .absolute()
                    .top(px(self.tabs_seam))
                    .left(px(0.0))
                    .w_full()
                    .h(px(peek_offset))
                    .bg(terminal.terminal_surface()),
            );
        }

        // Paint the frame independently from layout. A normal border shrinks
        // the content box, putting this title bar one pixel below the
        // borderless sidebar title bar even though both are 42 points tall.
        // The sidebar owns the shared divider; only draw our left edge when
        // it is hidden. Bottom corners stay square against the window edge.
        let card_outline = div()
            .absolute()
            .inset_0()
            .when(seam <= 0.0, |outline| outline.rounded_tl(px(Radius::CARD)))
            .when(inspector_seam <= 0.0, |outline| {
                outline.rounded_tr(px(Radius::CARD))
            })
            .border_t_1()
            .border_r_1()
            .border_b_1()
            .when(seam <= 0.0, |outline| outline.border_l_1())
            .border_color(terminal.primary.alpha(0.10));

        if let Some(workbench) = &self.workspace_workbench {
            let external_owner = if terminal_in_workspace_panel
                && self
                    .auxiliary_terminal
                    .as_ref()
                    .is_some_and(|terminal| terminal.read(cx).is_focused(window))
            {
                self.auxiliary_id.clone()
            } else {
                None
            };
            workbench.update(cx, |workbench, _| {
                workbench.set_external_owner(external_owner)
            });
        }
        if terminal_in_workspace_panel && let Some(auxiliary) = &self.auxiliary_terminal {
            auxiliary.update(cx, |terminal, cx| {
                terminal.set_shell_chrome(visible_sidebar, true, cx);
                terminal.set_header_trailing_inset(0.0, cx);
                terminal.set_viewport(
                    TerminalViewport {
                        x: sidebar_width + card_width,
                        y: Metrics::TITLE_BAR,
                        width: inspector_width,
                        height: f32::from(viewport_size.height).max(0.0),
                    },
                    cx,
                );
            });
        }

        // The strip hosts the primary pane's title-bar actions whenever it is
        // the settled chrome, so the pane hides its own title bar in step with
        // `set_header_hidden` below rather than with the slide.
        let hosts_pane_actions =
            tabs_height > 0.0 && self.active_workspace.is_none() && !self.preview;
        if self.tabs_seam > 0.0 {
            let sidebar_colors = {
                let store = self
                    .window_store
                    .read()
                    .expect("session store lock poisoned");
                crate::app_theme::sidebar_colors_in(&store)
            };
            let trailing = hosts_pane_actions
                .then_some(self.terminal.as_ref())
                .flatten()
                .and_then(|primary| {
                    primary.update(cx, |terminal, cx| {
                        terminal.render_hosted_header_actions(sidebar_colors, cx)
                    })
                });
            let strip = self.sidebar.update(cx, |sidebar, cx| {
                sidebar.render_horizontal_tabs(card_width, trailing, cx)
            });
            card = card.child(
                div()
                    .id("animated-top-bar")
                    .flex_none()
                    .h(px(self.tabs_seam))
                    .w_full()
                    .overflow_hidden()
                    .child(
                        div()
                            .relative()
                            .top(px(self.tabs_seam - crate::tab_navigation::TAB_STRIP_HEIGHT))
                            .h(px(crate::tab_navigation::TAB_STRIP_HEIGHT))
                            .child(strip),
                    ),
            );
        }
        // Translation changes only paint placement. The stationary tab strip
        // and settled PTY viewport never participate in the gesture layout.
        let mut body = div()
            .id("terminal-card-body")
            .debug_selector(|| "terminal-card-body".into())
            .relative()
            .top(px(peek_offset))
            .flex_none()
            .flex()
            .flex_col()
            .w_full()
            .h(px(card_height))
            .min_h(px(0.0))
            .bg(terminal.work_surface_nested());
        if self.active_workspace.is_some() {
            let tab = {
                let store = self.window_store.read().expect("store");
                store
                    .workspace_catalog()
                    .snapshot()
                    .and_then(|snapshot| {
                        snapshot
                            .workspaces
                            .iter()
                            .find(|workspace| Some(&workspace.id) == self.active_workspace.as_ref())
                    })
                    .and_then(|workspace| {
                        workspace
                            .tabs
                            .iter()
                            .find(|tab| Some(&tab.id) == workspace.selected_tab.as_ref())
                    })
                    .cloned()
            };
            if let (Some(tab), Some(workbench)) = (tab, &self.workspace_workbench) {
                workbench.update(cx, |workbench, cx| {
                    workbench.set_tab(
                        tab,
                        TerminalViewport {
                            x: sidebar_width,
                            y: tabs_height,
                            width: card_width,
                            height: card_height,
                        },
                        window,
                        cx,
                    )
                });
                body = body.child(workbench.clone());
                self.sync_inspector_context(cx);
            } else {
                if let Some(workbench) = &self.workspace_workbench {
                    workbench.update(cx, |workbench, cx| workbench.deactivate(cx));
                }
                body = body.child(
                    div()
                        .p(px(28.0))
                        .text_color(terminal.secondary)
                        .child("Choose an agent from the sidebar, or start a New Agent"),
                );
            }
        } else if self.preview && self.preview_scenario != PreviewScenario::Empty {
            body = body.child(self.preview_workbench(terminal));
        } else if split_open {
            let available_height = (card_height - 1.0).max(0.0);
            self.terminal_available_height = available_height;
            let heights = self.workbench_layout.pane_heights(available_height);
            if let Some(primary) = &self.terminal {
                primary.update(cx, |terminal, cx| {
                    terminal.set_shell_chrome(visible_sidebar, self.inspector_open, cx);
                    terminal.set_header_hidden(hosts_pane_actions, cx);
                    terminal.set_viewport(
                        TerminalViewport {
                            x: sidebar_width,
                            y: tabs_height,
                            width: card_width,
                            height: heights.primary,
                        },
                        cx,
                    );
                });
                body = body.child(
                    div()
                        .flex_none()
                        .w_full()
                        .h(px(heights.primary))
                        .min_h(px(0.0))
                        .overflow_hidden()
                        .child(primary.clone()),
                );
            }
            body = body.child(self.terminal_resize_handle(cx));

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
                            y: tabs_height + heights.primary + 1.0,
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
                        .bg(terminal.terminal_surface())
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
            body = body.child(auxiliary);
        } else if let Some(primary) = &self.terminal {
            self.terminal_available_height = card_height;
            primary.update(cx, |terminal, cx| {
                terminal.set_shell_chrome(visible_sidebar, self.inspector_open, cx);
                terminal.set_header_hidden(hosts_pane_actions, cx);
                terminal.set_viewport(
                    TerminalViewport {
                        x: sidebar_width,
                        y: tabs_height,
                        width: card_width,
                        height: card_height,
                    },
                    cx,
                );
            });
            body = body.child(primary.clone());
        }

        if let Some(auxiliary) = &self.auxiliary_terminal {
            let duplicate = self.active_workspace.is_some()
                && self.auxiliary_id.as_ref().is_some_and(|id| {
                    self.workspace_workbench
                        .as_ref()
                        .is_some_and(|workbench| workbench.read(cx).visible_session(id))
                });
            let visible =
                terminal_in_workspace_panel || (self.active_workspace.is_none() && split_open);
            auxiliary.update(cx, |terminal, _| {
                if visible && (!duplicate || terminal.is_focused(window)) {
                    terminal.claim_layout_control(window);
                } else {
                    terminal.release_layout_control();
                }
            });
        }
        card.child(body).child(card_outline).into_any_element()
    }

    fn preview_workbench(&self, colors: SemanticColors) -> AnyElement {
        let scenario = match self.preview_scenario {
            PreviewScenario::Typical => "Typical",
            PreviewScenario::Stress => "Stress",
            PreviewScenario::Empty => "Empty",
            PreviewScenario::Artifacts => "Artifacts",
            PreviewScenario::Fleet => "30 working sessions",
            PreviewScenario::Projects => "Six projects",
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

    /// Raises the system's alert sheet for a pending close and applies its
    /// answer; a no-op while one is already up or nothing is pending.
    fn sync_close_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((title, message)) = self.sidebar.read(cx).pending_close_copy() else {
            self.close_prompt = None;
            return;
        };
        if self.close_prompt.is_some() {
            return;
        }
        let answer = window.prompt(
            gpui::PromptLevel::Warning,
            &title,
            Some(&message),
            &[
                gpui::PromptButton::ok("Close"),
                gpui::PromptButton::cancel("Cancel"),
            ],
            cx,
        );
        let sidebar = self.sidebar.clone();
        self.close_prompt = Some(cx.spawn(async move |this, cx| {
            let choice = answer.await.ok();
            let _ = this.update(cx, |root, cx| {
                root.close_prompt = None;
                sidebar.update(cx, |sidebar, cx| match choice {
                    Some(0) => sidebar.confirm_close(cx),
                    _ => sidebar.cancel_close(cx),
                });
            });
        }));
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
            .window_store
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
            .bottom(px(16.0))
            .right(px(16.0))
            .w(px(380.0))
            .max_w(gpui::relative(0.9))
            .p(px(14.0))
            .flex()
            .flex_col()
            .gap(px(8.0))
            .rounded(px(Radius::PANEL))
            .bg(colors.floating_surface())
            .border_1()
            .border_color(colors.floating_stroke())
            .shadow_lg()
            .occlude()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .text_color(colors.primary)
            .child(
                div()
                    .pr(px(24.0))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .child(sf_symbol(
                        if matches!(
                            notice.kind,
                            RecoveryKind::ActionFailed | RecoveryKind::ManualAttention
                        ) {
                            "exclamationmark.triangle"
                        } else {
                            "arrow.triangle.2.circlepath"
                        },
                        13.0,
                        accent,
                    ))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .text_size(px(Typo::ROW_EMPHASIZED.size))
                            .font_weight(Typo::ROW_EMPHASIZED.weight)
                            .child(notice.title),
                    ),
            )
            .child(
                div()
                    .text_size(px(Typo::META.size))
                    .line_height(px(18.0))
                    .text_color(colors.secondary)
                    .child(bounded_notice_body(&notice.body)),
            );
        let mut actions = div().flex().items_center().gap(px(8.0));
        if let Some((action, label)) = notice.primary_action {
            let store = self.window_store.clone();
            actions = actions.child(
                div()
                    .id("recovery-primary-action")
                    .self_start()
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
        let detail = notice.detail;
        let has_actions = notice.primary_action.is_some() || detail.is_some();
        if let Some(detail) = detail {
            actions = actions.child(
                div()
                    .id("copy-recovery-details")
                    .debug_selector(|| "copy-recovery-details".into())
                    .h(px(28.0))
                    .px(px(7.0))
                    .flex()
                    .items_center()
                    .rounded(px(Radius::ROW))
                    .cursor_pointer()
                    .text_size(px(Typo::META.size))
                    .text_color(colors.secondary)
                    .hover(move |button| button.bg(colors.primary.alpha(0.06)))
                    .child("Copy details")
                    .on_click(move |_, _, cx| {
                        cx.write_to_clipboard(gpui::ClipboardItem::new_string(detail.clone()));
                        cx.stop_propagation();
                    }),
            );
        }
        if has_actions {
            bar = bar.child(actions);
        }
        if notice.dismissible {
            let store = self.window_store.clone();
            bar = bar.child(
                div()
                    .id("dismiss-recovery-notice")
                    .debug_selector(|| "dismiss-recovery-notice".into())
                    .absolute()
                    .top(px(9.0))
                    .right(px(9.0))
                    .size(px(28.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(Radius::CHIP))
                    .cursor_pointer()
                    .tooltip(move |_, cx| {
                        cx.new(|_| crate::palette_chrome::PaletteTooltip("Dismiss".into(), colors))
                            .into()
                    })
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

impl RootView {
    fn handle_tab_pinch(
        &mut self,
        event: &gpui::PinchEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let allowed = window.is_window_active()
            && !self.launcher.read(cx).is_open()
            && !self
                .navigation
                .as_ref()
                .is_some_and(|v| v.read(cx).is_open())
            && !self
                .utility_surfaces
                .as_ref()
                .is_some_and(|v| v.read(cx).is_open())
            && !self.notification_panel_open
            && self.sidebar.read(cx).pending_close_copy().is_none()
            && self.quote_target_picker.is_none()
            && self.resize_origin.is_none()
            && self.terminal_resize_origin.is_none()
            && self.inspector_resize_origin.is_none();
        let Some(surfaces) = self.session_surfaces.clone() else {
            return;
        };
        if !allowed {
            self.tab_pinch.cancel();
            return;
        }
        let now = cx.background_executor().now();
        let position = surfaces.update(cx, |surfaces, _| surfaces.tab_peek_position(now));
        let frame = self.tab_pinch.sample(event, position, now);
        if self.tab_pinch.take_feedback() {
            #[cfg(target_os = "macos")]
            crate::macos::pinch_feedback();
        }
        if let Some(frame) = frame {
            surfaces.update(cx, |surfaces, cx| {
                surfaces.tab_gesture_at(frame, now, cx);
                surfaces.sync_tab_peek_focus(window, cx);
            });
            cx.stop_propagation();
        }
    }
}

impl Render for RootView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.main_viewport = window.viewport_size();
        if self.pending_notification_open.is_some()
            && self
                .window_store
                .read()
                .expect("store")
                .has_hydrated_sessions()
            && let Some((session, event)) = self.pending_notification_open.take()
        {
            self.open_notification(session, event, window, cx);
        }
        // Before anything reads a color: this frame's sample of a theme fade.
        crate::app_theme::follow(&self.window_store.read().expect("store"), window, cx);
        let colors = self.colors();
        self.sync_window_material(window);
        let launcher_open = self.launcher.read(cx).is_open();
        let recovery_notice = if self.preview {
            None
        } else {
            let store = self
                .window_store
                .read()
                .expect("session store lock poisoned");
            // The composer owns its inline failure while open. Once closed,
            // the same failure remains available here without duplicate copy.
            RecoveryNotice::resolve(
                store.daemon_state(),
                store.action_failure().filter(|failure| {
                    !launcher_open || failure.title != crate::store::PROMPT_DELIVERY_FAILURE_TITLE
                }),
            )
        };
        #[cfg(target_os = "macos")]
        {
            self.notification_health = crate::application_notifications::health(cx);
        }
        let notification_surface_visible = !launcher_open
            && !self.notification_panel_open
            && !self
                .utility_surfaces
                .as_ref()
                .is_some_and(|surfaces| surfaces.read(cx).is_open());
        self.window_store
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
        let window_width = f32::from(window.viewport_size().width);
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
                gpui::point(px(0.0), px(0.0)),
                gpui::size(px(sidebar_width + inset), window.viewport_size().height),
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
            .top_0()
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
            .capture_pinch(cx.listener(Self::handle_tab_pinch))
            .relative()
            .size_full()
            // Real SF Pro (registered from SFNS.ttf at startup) for every UI
            // surface; the terminal grid sets its own mono font.
            .font_family(crate::fonts::ui_family())
            .flex()
            // The window's base tint. Opaque matches the solid platform
            // window; glass leaves the blurred desktop showing through it.
            // Every panel keeps its own surface treatment above this base.
            .bg(colors.window_fill())
            .track_focus(&self.focus)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &gpui::MouseDownEvent, _, _| {
                    let pointer_y = f32::from(event.position.y);
                    this.titlebar_drag_armed =
                        cfg!(target_os = "macos") && (0.0..Metrics::TITLE_BAR).contains(&pointer_y);
                }),
            )
            .on_mouse_move(
                cx.listener(|this, event: &gpui::MouseMoveEvent, window, _| {
                    if this.titlebar_drag_armed && event.pressed_button == Some(MouseButton::Left) {
                        this.titlebar_drag_armed = false;
                        window.start_window_move();
                    }
                }),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, event: &gpui::MouseUpEvent, window, _| {
                    if this.titlebar_drag_armed && event.click_count == 2 {
                        window.titlebar_double_click();
                    }
                    this.titlebar_drag_armed = false;
                }),
            )
            .capture_key_down(cx.listener(Self::on_key_down))
            .capture_key_up(cx.listener(Self::on_key_up))
            .on_action(cx.listener(Self::close_selected_session))
            .on_action(
                cx.listener(|this, _: &crate::commands::NewWindow, window, cx| {
                    let context = this.native_window_context(window, cx);
                    crate::open_main_window_with_context(
                        cx,
                        this.services.clone(),
                        this.preview,
                        this.preview_scenario,
                        context,
                    );
                }),
            )
            .on_action(
                cx.listener(|_, _: &crate::commands::CloseWindow, window, _| {
                    // Dispatch already identifies the originating window. Closing
                    // it must not consult whichever platform window is active later.
                    window.remove_window();
                }),
            )
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
            .on_action(
                cx.listener(|this, _: &crate::commands::FocusPaneLeft, window, cx| {
                    this.run_command(CommandId::FocusPaneLeft, window, cx);
                }),
            )
            .on_action(
                cx.listener(|this, _: &crate::commands::FocusPaneRight, window, cx| {
                    this.run_command(CommandId::FocusPaneRight, window, cx);
                }),
            )
            .on_action(
                cx.listener(|this, _: &crate::commands::FocusPaneUp, window, cx| {
                    this.run_command(CommandId::FocusPaneUp, window, cx);
                }),
            )
            .on_action(
                cx.listener(|this, _: &crate::commands::FocusPaneDown, window, cx| {
                    this.run_command(CommandId::FocusPaneDown, window, cx);
                }),
            )
            .on_action(
                cx.listener(|this, _: &crate::commands::SplitPaneRight, window, cx| {
                    this.run_command(CommandId::SplitPaneRight, window, cx);
                }),
            )
            .on_action(
                cx.listener(|this, _: &crate::commands::SplitPaneBelow, window, cx| {
                    this.run_command(CommandId::SplitPaneBelow, window, cx);
                }),
            )
            .on_action(
                cx.listener(|this, _: &crate::commands::TogglePaneZoom, window, cx| {
                    this.run_command(CommandId::TogglePaneZoom, window, cx);
                }),
            )
            .on_action(
                cx.listener(|this, _: &crate::commands::RemoveFocusedPane, window, cx| {
                    this.run_command(CommandId::RemoveFocusedPane, window, cx);
                }),
            )
            .on_action(
                cx.listener(|this, _: &crate::commands::PaneGrowWidth, window, cx| {
                    this.run_command(CommandId::PaneGrowWidth, window, cx);
                }),
            )
            .on_action(
                cx.listener(|this, _: &crate::commands::PaneShrinkWidth, window, cx| {
                    this.run_command(CommandId::PaneShrinkWidth, window, cx);
                }),
            )
            .on_action(
                cx.listener(|this, _: &crate::commands::PaneGrowHeight, window, cx| {
                    this.run_command(CommandId::PaneGrowHeight, window, cx);
                }),
            )
            .on_action(
                cx.listener(|this, _: &crate::commands::PaneShrinkHeight, window, cx| {
                    this.run_command(CommandId::PaneShrinkHeight, window, cx);
                }),
            )
            .on_action(
                cx.listener(|this, _: &crate::commands::SwapPaneLeft, window, cx| {
                    this.run_command(CommandId::SwapPaneLeft, window, cx);
                }),
            )
            .on_action(
                cx.listener(|this, _: &crate::commands::SwapPaneRight, window, cx| {
                    this.run_command(CommandId::SwapPaneRight, window, cx);
                }),
            )
            .on_action(
                cx.listener(|this, _: &crate::commands::SwapPaneUp, window, cx| {
                    this.run_command(CommandId::SwapPaneUp, window, cx);
                }),
            )
            .on_action(
                cx.listener(|this, _: &crate::commands::SwapPaneDown, window, cx| {
                    this.run_command(CommandId::SwapPaneDown, window, cx);
                }),
            )
            .on_action(
                cx.listener(|this, _: &crate::commands::MovePaneLeft, window, cx| {
                    this.run_command(CommandId::MovePaneLeft, window, cx);
                }),
            )
            .on_action(
                cx.listener(|this, _: &crate::commands::MovePaneRight, window, cx| {
                    this.run_command(CommandId::MovePaneRight, window, cx);
                }),
            )
            .on_action(
                cx.listener(|this, _: &crate::commands::MovePaneUp, window, cx| {
                    this.run_command(CommandId::MovePaneUp, window, cx);
                }),
            )
            .on_action(
                cx.listener(|this, _: &crate::commands::MovePaneDown, window, cx| {
                    this.run_command(CommandId::MovePaneDown, window, cx);
                }),
            )
            .on_action(
                cx.listener(|this, _: &crate::commands::ReviewLaunches, window, cx| {
                    this.run_command(CommandId::ReviewLaunches, window, cx);
                }),
            )
            .on_action(cx.listener(|this, _: &ToggleTabPeek, window, cx| {
                this.toggle_tab_peek(window, cx);
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
            .on_action(cx.listener(|this, _: &commands::ShowAgentSettings, _, cx| {
                if let Some(surfaces) = &this.utility_surfaces {
                    surfaces.update(cx, |surfaces, cx| surfaces.open_agent_settings(None, cx));
                }
            }))
            .on_action(cx.listener(|this, _: &ToggleSidebar, window, cx| {
                this.run_command(CommandId::ToggleSidebar, window, cx);
            }))
            .on_action(
                cx.listener(|this, _: &commands::ToggleTabOrientation, window, cx| {
                    this.run_command(CommandId::ToggleTabOrientation, window, cx);
                }),
            )
            .on_action(
                cx.listener(|this, _: &commands::HorizontalTabs, window, cx| {
                    this.run_command(CommandId::HorizontalTabs, window, cx);
                }),
            )
            .on_action(cx.listener(|this, _: &commands::VerticalTabs, window, cx| {
                this.run_command(CommandId::VerticalTabs, window, cx);
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
        root = root.children(self.sidebar.update(cx, |sidebar, cx| {
            sidebar.render_project_picker_overlay(window, cx)
        }));
        root = root.children(self.sidebar.update(cx, |sidebar, cx| {
            sidebar.render_strip_menu_overlay(exposed > 0.0, window, cx)
        }));
        if !sidebar_visible
            && seam == 0.0
            && exposed == 0.0
            && panel_width == 0.0
            && !self.sidebar.read(cx).project_picker_active()
        {
            root = root.child(
                div()
                    .id("sidebar-peek-edge")
                    .debug_selector(|| "sidebar-peek-edge".into())
                    .absolute()
                    .left_0()
                    .top(px(36.0))
                    .bottom_0()
                    .w(px(SIDEBAR_PEEK_TRIGGER_WIDTH))
                    .on_hover(cx.listener(|this, hovered: &bool, window, cx| {
                        this.sidebar_peek_dwell = None;
                        if *hovered {
                            this.sidebar_peek_dwell =
                                Some(cx.spawn_in(window, async move |this, cx| {
                                    cx.background_executor().timer(SIDEBAR_PEEK_DWELL).await;
                                    let _ = crate::floating::update_in_owner(
                                        &this,
                                        cx,
                                        |this, window, cx| {
                                            this.sidebar_peek_dwell = None;
                                            this.sidebar.update(cx, |sidebar, cx| {
                                                sidebar.hover_peek_region(true, window, cx);
                                                sidebar.peek(window, cx);
                                            });
                                        },
                                    );
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
        if let Some(launches) = self.workspace_launches(colors, cx) {
            root = root.child(launches);
        }
        if crate::alerts::enabled(cx) {
            self.sync_close_prompt(window, cx);
        } else if let Some(confirmation) = self.close_confirmation(colors, cx) {
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
            if surfaces.read(cx).is_settings_open() && !surfaces.read(cx).is_usage_share_open() {
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
            root = root.child(dev_build_marker(build.marker_label(), colors, 10.0));
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
    use gpui::{Modifiers, point, size};

    fn workspace_cpu_fixture() -> (Arc<AppServices>, diri_proto::workspace::WorkspaceId) {
        use diri_proto::workspace::*;
        let services = test_services();
        let workspace = WorkspaceId::new("cpu-workspace");
        let tab = TabId::new("cpu-tab");
        let pane = PaneId::new("cpu-pane");
        {
            let mut store = services.store.store.write().unwrap();
            store.hydrate(SidebarPreviewFixture::make(PreviewScenario::Typical).list);
            store.seed_workspace_snapshot_for_test(WorkspaceSnapshot {
                revision: 1,
                workspaces: vec![WorkspaceRecord {
                    project_id: None,
                    id: workspace.clone(),
                    name: "CPU fixture".into(),
                    selected_tab: Some(tab.clone()),
                    tabs: vec![WorkspaceTab {
                        id: tab,
                        title: None,
                        focused_pane: pane.clone(),
                        zoomed_pane: None,
                        layout: LayoutNode::Pane {
                            id: pane,
                            session_id: SessionId::new("preview-claude"),
                        },
                    }],
                }],
                ..Default::default()
            });
        }
        (services, workspace)
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "headless Metal CPU comparison; run explicitly on macOS"]
    fn workspace_sidebar_redraw_cpu() {
        use gpui::HeadlessAppContext;
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
        let (services, workspace) = workspace_cpu_fixture();
        let window = cx
            .open_window(size(px(1600.0), px(1000.0)), |window, cx| {
                cx.new(|cx| {
                    let root = RootView::new(services, false, PreviewScenario::Empty, window, cx);
                    root.sidebar.update(cx, |sidebar, cx| {
                        sidebar.activate_workspace(Some(workspace), cx)
                    });
                    root
                })
            })
            .unwrap();
        cx.run_until_parked();
        let (sidebar, terminal) = cx
            .update_window(window.into(), |root, _, cx| {
                let root = root.downcast::<RootView>().unwrap();
                let root = root.read(cx);
                (
                    root.sidebar.clone(),
                    root.workspace_workbench
                        .as_ref()
                        .unwrap()
                        .read(cx)
                        .focused_terminal()
                        .unwrap(),
                )
            })
            .unwrap();
        cx.update(|cx| {
            terminal.update(cx, |terminal, cx| {
                let mut grid = diri_term::buffer::GridBuffer::new(160, 50);
                for (index, cell) in grid.cells.iter_mut().enumerate() {
                    cell.scalar = u32::from(b'a' + (index % 26) as u8);
                }
                terminal.seed_preview_grid_for_test(grid, cx);
                cx.notify();
            })
        });
        cx.run_until_parked();
        cx.capture_screenshot(window.into()).unwrap();
        for _ in 0..20 {
            cx.update(|cx| sidebar.update(cx, |_, cx| cx.notify()));
            cx.run_until_parked();
        }
        let before = cx.update(|cx| terminal.read(cx).render_count);
        fn cpu_seconds() -> f64 {
            let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
            assert_eq!(
                unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) },
                0
            );
            let usage = unsafe { usage.assume_init() };
            usage.ru_utime.tv_sec as f64
                + usage.ru_utime.tv_usec as f64 / 1e6
                + usage.ru_stime.tv_sec as f64
                + usage.ru_stime.tv_usec as f64 / 1e6
        }
        let start_cpu = cpu_seconds();
        let start = Instant::now();
        for _ in 0..200 {
            cx.update(|cx| sidebar.update(cx, |_, cx| cx.notify()));
            cx.run_until_parked();
        }
        let cpu = cpu_seconds() - start_cpu;
        let wall = start.elapsed();
        let renders = cx.update(|cx| terminal.read(cx).render_count) - before;
        eprintln!(
            "workspace-sidebar-cpu: updates=200 terminal_renders={renders} cpu_ms_per_update={:.3} wall_ms_per_update={:.3}",
            cpu * 1000.0 / 200.0,
            wall.as_secs_f64() * 1000.0 / 200.0
        );
        if let Ok(output) = std::env::var("DIRI_REDRAW_SCREENSHOT") {
            cx.capture_screenshot(window.into())
                .unwrap()
                .save(output)
                .unwrap();
        }
        cx.update_window(window.into(), |_, window, _| window.remove_window())
            .unwrap();
        cx.run_until_parked();
    }

    #[gpui::test]
    fn sidebar_updates_do_not_render_unchanged_workspace_terminal(cx: &mut gpui::TestAppContext) {
        let (services, workspace) = workspace_cpu_fixture();
        let (root, cx) = cx.add_window_view(move |window, cx| {
            let root = RootView::new(services, false, PreviewScenario::Empty, window, cx);
            root.sidebar.update(cx, |sidebar, cx| {
                sidebar.activate_workspace(Some(workspace), cx);
            });
            root
        });
        cx.run_until_parked();
        let terminal = root.read_with(cx, |root, cx| {
            root.workspace_workbench
                .as_ref()
                .unwrap()
                .read(cx)
                .focused_terminal()
                .unwrap()
        });
        // Settle initial geometry and focus. Sidebar activity and store updates
        // both invalidate this entity; neither changes the terminal grid.
        cx.executor().advance_clock(Duration::from_millis(500));
        cx.run_until_parked();
        let before = terminal.read_with(cx, |terminal, _| terminal.render_count);
        assert!(before > 0, "fixture must mount and render its terminal");
        let sidebar = root.read_with(cx, |root, _| root.sidebar.clone());
        for _ in 0..8 {
            sidebar.update(cx, |_, cx| cx.notify());
            cx.executor().advance_clock(Duration::from_millis(125));
            cx.run_until_parked();
        }
        let after = terminal.read_with(cx, |terminal, _| terminal.render_count);
        eprintln!(
            "unchanged terminal renders during eight sidebar ticks: {}",
            after - before
        );
        assert_eq!(
            after, before,
            "sidebar animation repainted an unchanged terminal"
        );
        terminal.update(cx, |_, cx| cx.notify());
        cx.run_until_parked();
        assert!(
            terminal.read_with(cx, |terminal, _| terminal.render_count) > after,
            "terminal notifications must still redraw through the cached parent"
        );
        let before_resize = terminal.read_with(cx, |terminal, _| terminal.geometry_for_test());
        cx.simulate_resize(size(px(1100.0), px(750.0)));
        cx.run_until_parked();
        assert_ne!(
            terminal.read_with(cx, |terminal, _| terminal.geometry_for_test()),
            before_resize,
            "cached workspace must propagate new terminal dimensions"
        );
    }

    #[gpui::test]
    fn confirmed_close_restores_previous_session_from_saved_pane(cx: &mut gpui::TestAppContext) {
        check_close_restores_previous_session_from_saved_pane(cx, true);
    }

    #[gpui::test]
    fn immediate_close_restores_previous_session_from_saved_pane(cx: &mut gpui::TestAppContext) {
        check_close_restores_previous_session_from_saved_pane(cx, false);
    }

    fn check_close_restores_previous_session_from_saved_pane(
        cx: &mut gpui::TestAppContext,
        confirm: bool,
    ) {
        cx.update(|cx| commands::bind_keys(cx, &Default::default()));
        let (services, workspace) = workspace_cpu_fixture();
        services
            .store
            .store
            .write()
            .unwrap()
            .update_preferences(|prefs| {
                prefs.confirm_before_closing_session = confirm;
            })
            .unwrap();
        let runtime = services.store.clone();
        let closed = SessionId::new("preview-claude");
        let previous = services
            .store
            .store
            .write()
            .unwrap()
            .ordered_sessions()
            .into_iter()
            .find(|session| session.id != closed && !session.is_archived())
            .unwrap()
            .id;
        let expected = previous.clone();
        let removed = closed.clone();
        let (root, cx) = cx.add_window_view(move |window, cx| {
            let root = RootView::new(services, false, PreviewScenario::Empty, window, cx);
            root.window_store.write().unwrap().select(previous.clone());
            root.window_store.write().unwrap().select(closed.clone());
            root.sidebar.update(cx, |sidebar, cx| {
                sidebar.activate_workspace(Some(workspace), cx)
            });
            root
        });
        cx.run_until_parked();
        cx.simulate_keystrokes(&commands::test_chords("cmd-w"));
        if confirm {
            assert!(root.read_with(cx, |root, _| {
                root.window_store.read().unwrap().pending_close().is_some()
            }));
            cx.simulate_keystrokes("escape");
            root.update_in(cx, |root, window, cx| {
                assert_eq!(root.active_session_id(cx), Some(removed.clone()));
                assert!(
                    root.active_terminal(cx)
                        .unwrap()
                        .read(cx)
                        .is_focused(window)
                );
            });
            cx.simulate_keystrokes(&commands::test_chords("cmd-w"));
            cx.simulate_keystrokes("enter");
        }
        // Settle the daemon's removal while its saved layout still references
        // the closed session: this is when the unavailable placeholder appears.
        runtime
            .store
            .write()
            .unwrap()
            .remove_session_record(&removed);
        cx.run_until_parked();
        root.update_in(cx, |root, window, cx| {
            assert_eq!(
                root.active_session_id(cx),
                Some(expected),
                "closing must leave the saved pane and restore the previous session"
            );
            assert!(
                root.active_terminal(cx)
                    .unwrap()
                    .read(cx)
                    .is_focused(window)
            );
        });
    }

    #[gpui::test]
    fn close_confirmation_is_the_system_alert_when_native(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            commands::bind_keys(cx, &Default::default());
            crate::alerts::enable(cx);
        });
        let services = test_services();
        let runtime = services.store.clone();
        let fixture = SidebarPreviewFixture::make(PreviewScenario::Typical);
        let selected = fixture.selected_session_id.expect("selected session");
        {
            let mut store = runtime.store.write().unwrap();
            store.hydrate(fixture.list);
            store.select(selected.clone());
        }
        let (root, cx) = cx.add_window_view(move |window, cx| {
            RootView::new(services, false, PreviewScenario::Typical, window, cx)
        });
        let window_store = root.read_with(cx, |root, _| root.window_store.clone());

        cx.simulate_keystrokes(&commands::test_chords("cmd-w"));
        assert!(window_store.read().unwrap().pending_close().is_some());
        assert!(cx.has_pending_prompt(), "a system alert asks first");
        assert!(
            cx.debug_bounds("cancel-close").is_none(),
            "no in-window dialog beside the alert"
        );
        cx.simulate_prompt_answer("Cancel");
        cx.run_until_parked();
        {
            let store = window_store.read().unwrap();
            assert!(store.pending_close().is_none(), "Cancel keeps the session");
            assert_eq!(store.selected_session_id(), Some(&selected));
        }

        cx.simulate_keystrokes(&commands::test_chords("cmd-w"));
        assert!(cx.has_pending_prompt());
        cx.simulate_prompt_answer("Close");
        cx.run_until_parked();
        let store = window_store.read().unwrap();
        assert!(store.pending_close().is_none());
        assert_ne!(
            store.selected_session_id(),
            Some(&selected),
            "Close removes it"
        );
    }

    #[gpui::test]
    fn close_confirmation_keyboard_from_terminal(cx: &mut gpui::TestAppContext) {
        check_close_confirmation_keyboard(cx, false);
    }

    #[gpui::test]
    fn close_confirmation_keyboard_from_sidebar(cx: &mut gpui::TestAppContext) {
        check_close_confirmation_keyboard(cx, true);
    }

    fn check_close_confirmation_keyboard(cx: &mut gpui::TestAppContext, sidebar_focused: bool) {
        cx.update(|cx| commands::bind_keys(cx, &Default::default()));
        let services = test_services();
        let runtime = services.store.clone();
        let fixture = SidebarPreviewFixture::make(PreviewScenario::Typical);
        let selected = fixture.selected_session_id.expect("selected session");
        {
            let mut store = runtime.store.write().unwrap();
            store.hydrate(fixture.list);
            store.select(selected.clone());
        }
        let (root, cx) = cx.add_window_view(move |window, cx| {
            RootView::new(services, false, PreviewScenario::Typical, window, cx)
        });
        let window_store = root.read_with(cx, |root, _| root.window_store.clone());
        if sidebar_focused {
            root.update_in(cx, |root, window, cx| {
                root.sidebar
                    .update(cx, |sidebar, cx| sidebar.focus(window, cx));
            });
        }

        cx.simulate_keystrokes(&commands::test_chords("cmd-w"));
        assert!(window_store.read().unwrap().pending_close().is_some());
        cx.simulate_keystrokes("escape");
        {
            let store = window_store.read().unwrap();
            assert!(
                store.pending_close().is_none(),
                "Escape must cancel closing"
            );
            assert_eq!(store.selected_session_id(), Some(&selected));
        }

        cx.simulate_keystrokes(&commands::test_chords("cmd-w"));
        assert!(window_store.read().unwrap().pending_close().is_some());
        cx.simulate_keystrokes("enter");
        let store = window_store.read().unwrap();
        assert!(
            store.pending_close().is_none(),
            "Enter must confirm closing"
        );
        assert_ne!(store.selected_session_id(), Some(&selected));
    }

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
    fn terminal_feedback_reaches_the_standard_toast_in_both_layouts(cx: &mut gpui::TestAppContext) {
        let services = test_services();
        let (root, cx) = cx.add_window_view(move |window, cx| {
            RootView::new(services, false, PreviewScenario::Empty, window, cx)
        });
        for workspace in [false, true] {
            root.update_in(cx, |root, window, cx| {
                let event = TerminalPaneEvent::Feedback {
                    message: format!("Input rejected in workspace={workspace}"),
                };
                if workspace {
                    root.activate_saved_workspace(
                        Some(diri_proto::workspace::WorkspaceId::new("toast-test")),
                        window,
                        cx,
                    );
                    root.workspace_workbench
                        .as_ref()
                        .unwrap()
                        .update(cx, |_, cx| {
                            cx.emit(
                                crate::workspace_workbench::WorkspaceWorkbenchEvent::Terminal(
                                    event,
                                ),
                            );
                        });
                } else {
                    root.terminal
                        .as_ref()
                        .unwrap()
                        .update(cx, |_, cx| cx.emit(event));
                }
            });
            cx.run_until_parked();
            root.read_with(cx, |root, _| {
                let banner = root
                    .status_banner
                    .as_ref()
                    .expect("standard right-side toast");
                assert_eq!(banner.title, "Terminal");
                assert_eq!(
                    banner.body,
                    format!("Input rejected in workspace={workspace}")
                );
            });
        }
        cx.executor().advance_clock(Duration::from_secs(4));
        cx.run_until_parked();
        root.read_with(cx, |root, _| assert!(root.status_banner.is_none()));
    }

    #[gpui::test]
    fn horizontal_tabs_fill_window_on_orientation_switch(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            cx.set_reduce_motion(false);
            commands::bind_keys(cx, &Default::default());
        });
        let services = test_services();
        let fixture = SidebarPreviewFixture::make(PreviewScenario::Typical);
        {
            let mut store = services.store.store.write().unwrap();
            store.hydrate(fixture.list);
            let selected = fixture.selected_session_id.unwrap();
            store.select(selected.clone());
            use diri_proto::workspace::*;
            let workspace = WorkspaceId::new("test-workspace");
            let tab = TabId::new("test-tab");
            let pane = PaneId::new("test-pane");
            store.seed_workspace_snapshot_for_test(WorkspaceSnapshot {
                revision: 1,
                workspaces: vec![WorkspaceRecord {
                    project_id: None,
                    id: workspace.clone(),
                    name: "Test".into(),
                    selected_tab: Some(tab.clone()),
                    tabs: vec![WorkspaceTab {
                        id: tab,
                        title: None,
                        focused_pane: pane.clone(),
                        zoomed_pane: None,
                        layout: LayoutNode::Pane {
                            id: pane,
                            session_id: selected,
                        },
                    }],
                }],
                ..Default::default()
            });
            store
                .update_preferences(|prefs| {
                    prefs.active_workspace = Some(workspace);
                    prefs.sidebar_visible = true;
                })
                .unwrap();
        }
        let (root, cx) = cx.add_window_view(move |window, cx| {
            RootView::new(services, false, PreviewScenario::Empty, window, cx)
        });
        let terminal = root.read_with(cx, |root, cx| root.active_terminal(cx).unwrap());
        for reduced_motion in [false, true] {
            cx.update(|_, cx| cx.set_reduce_motion(reduced_motion));
            for width in [640.0, 1000.0, 1942.0] {
                cx.simulate_resize(size(px(width), px(700.0)));
                cx.run_until_parked();
                let vertical = cx.debug_bounds("terminal-card-body").unwrap();
                assert!(vertical.left() > px(0.0));
                for horizontal in [true, false, true, false] {
                    root.update_in(cx, |root, window, cx| window.focus(&root.focus, cx));
                    cx.simulate_keystrokes(&commands::test_chords("cmd-shift-s"));
                    cx.run_until_parked();
                    // Check the first layout, without a resize, frame tick or a
                    // terminal-output notification to repair stale geometry.
                    let body = cx.debug_bounds("terminal-card-body").unwrap();
                    let grid = cx.debug_bounds("terminal-grid-surface").unwrap();
                    assert_eq!(body.right(), px(width));
                    assert_eq!(
                        grid.right(),
                        body.right(),
                        "terminal exceeds its card after switching tabs"
                    );
                    assert_eq!(
                        body.left(),
                        if horizontal { px(0.0) } else { vertical.left() }
                    );
                    assert_eq!(
                        body.top(),
                        px(if horizontal {
                            crate::tab_navigation::TAB_STRIP_HEIGHT
                        } else {
                            0.0
                        })
                    );
                    assert_eq!(body.bottom(), px(700.0));
                    root.read_with(cx, |root, cx| {
                        assert_eq!(root.active_terminal(cx), Some(terminal.clone()));
                        let viewport = terminal.read(cx).geometry_for_test().0.unwrap();
                        assert_eq!(px(viewport.x), body.left());
                        assert_eq!(px(viewport.width), body.size.width);
                    });
                    if horizontal {
                        let project = cx.debug_bounds("horizontal-tab-project").unwrap();
                        if cfg!(target_os = "macos") {
                            assert!(project.left() >= px(92.0), "tabs overlap traffic lights");
                        }
                    }
                }
            }
        }
    }

    #[gpui::test]
    fn horizontal_tabs_reveal_selection_after_first_layout_and_resize(
        cx: &mut gpui::TestAppContext,
    ) {
        let services = test_services();
        let fixture = SidebarPreviewFixture::make(PreviewScenario::Typical);
        let selected = fixture.selected_session_id.clone().unwrap();
        {
            let mut store = services.store.store.write().unwrap();
            store.hydrate(fixture.list);
            store.select(selected.clone());
        }
        let (root, cx) = cx.add_window_view(move |window, cx| {
            RootView::new(services, false, PreviewScenario::Empty, window, cx)
        });
        root.update_in(cx, |root, window, cx| {
            root.run_command(CommandId::HorizontalTabs, window, cx)
        });
        for width in [1000.0, 640.0] {
            cx.simulate_resize(size(px(width), px(700.0)));
            cx.run_until_parked();
            let tab = cx.debug_bounds("horizontal-tab-preview-codex").unwrap();
            let project = cx.debug_bounds("horizontal-tab-project").unwrap();
            assert!(
                tab.left() >= project.right(),
                "selected tab hidden to the left"
            );
            assert!(
                tab.right() <= px(width - 40.0),
                "selected tab hidden to the right at {width}: {tab:?}"
            );
        }
    }

    #[gpui::test]
    fn tab_orientation_preserves_terminal_identity_selection_and_restores_geometry(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|cx| {
            cx.set_reduce_motion(true);
            commands::bind_keys(cx, &Default::default());
        });
        let services = test_services();
        let fixture = SidebarPreviewFixture::make(PreviewScenario::Typical);
        let selected = fixture.selected_session_id.unwrap();
        {
            let mut store = services.store.store.write().unwrap();
            store.hydrate(fixture.list);
            store.select(selected.clone());
        }
        let store = services.store.clone();
        let (root, cx) = cx.add_window_view(move |window, cx| {
            RootView::new(services, false, PreviewScenario::Empty, window, cx)
        });
        cx.simulate_resize(size(px(1000.0), px(700.0)));
        root.update_in(cx, |root, window, cx| {
            root.run_command(CommandId::VerticalTabs, window, cx)
        });
        cx.run_until_parked();
        let (entity, before, records) = root.read_with(cx, |root, cx| {
            let terminal = root.terminal.as_ref().unwrap();
            (
                terminal.clone(),
                terminal.read(cx).geometry_for_test().0.unwrap(),
                store.store.read().unwrap().sessions().clone(),
            )
        });
        cx.simulate_keystrokes(&commands::test_chords("cmd-k"));
        cx.run_until_parked();
        assert!(
            root.read_with(cx, |root, cx| root
                .navigation
                .as_ref()
                .unwrap()
                .read(cx)
                .is_open()),
            "palette opens from terminal"
        );
        cx.simulate_keystrokes("h o r i z o n t a l");
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("palette-row-0").is_some(),
            "orientation is searchable"
        );
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        assert_eq!(
            store.store.read().unwrap().preferences().tab_orientation,
            crate::store::TabOrientation::Horizontal
        );
        assert!(cx.debug_bounds("horizontal-tabs").is_some());
        let horizontal = root.read_with(cx, |root, cx| {
            assert_eq!(root.terminal.as_ref(), Some(&entity));
            entity.read(cx).geometry_for_test().0.unwrap()
        });
        assert_eq!(
            horizontal.height,
            before.height - crate::tab_navigation::TAB_STRIP_HEIGHT
        );
        assert!(horizontal.width > before.width);
        assert_eq!(horizontal.y, crate::tab_navigation::TAB_STRIP_HEIGHT);
        let picker = cx.debug_bounds("horizontal-tab-project").unwrap();
        cx.simulate_click(picker.center(), Modifiers::default());
        assert!(cx.debug_bounds("project-picker-popup").is_some());
        cx.executor().advance_clock(Duration::from_millis(300));
        cx.run_until_parked();
        root.read_with(cx, |root, cx| {
            assert!(
                !root.sidebar.read(cx).is_peeking(),
                "project dropdown never reveals the sidebar"
            );
            assert_eq!(
                entity.read(cx).geometry_for_test().0.unwrap(),
                horizontal,
                "project picker overlays work without resizing"
            );
        });
        cx.simulate_keystrokes("escape");
        cx.executor().advance_clock(Duration::from_millis(300));
        cx.run_until_parked();
        cx.simulate_keystrokes(&commands::test_chords("cmd-shift-s"));
        cx.run_until_parked();
        root.read_with(cx, |root, cx| {
            assert_eq!(root.terminal.as_ref(), Some(&entity));
            assert_eq!(entity.read(cx).geometry_for_test().0.unwrap(), before);
        });
        let store = store.store.read().unwrap();
        assert_eq!(store.selected_session_id(), Some(&selected));
        assert_eq!(
            store.sessions(),
            &records,
            "presentation must not mutate workload records"
        );
    }

    #[gpui::test]
    fn tab_peek_commit_focuses_same_terminal_while_cancel_restores_prior_focus(
        cx: &mut gpui::TestAppContext,
    ) {
        let services = test_services();
        let fixture = SidebarPreviewFixture::make(PreviewScenario::Typical);
        {
            let mut store = services.store.store.write().unwrap();
            store.hydrate(fixture.list);
            store.select(fixture.selected_session_id.unwrap());
        }
        let (root, cx) = cx.add_window_view(move |window, cx| {
            RootView::new(services, false, PreviewScenario::Empty, window, cx)
        });
        cx.simulate_resize(size(px(1000.0), px(700.0)));
        cx.run_until_parked();
        let (original, terminal_focus) = root.read_with(cx, |root, cx| {
            (
                root.terminal.as_ref().unwrap().clone(),
                root.terminal
                    .as_ref()
                    .unwrap()
                    .read(cx)
                    .quote_focus_handle(),
            )
        });
        for key in ["escape", "enter"] {
            root.update_in(cx, |root, window, cx| {
                window.focus(&root.focus, cx);
                root.toggle_tab_peek(window, cx);
            });
            cx.simulate_keystrokes(key);
            cx.run_until_parked();
            root.update_in(cx, |root, window, cx| {
                assert_eq!(root.terminal.as_ref(), Some(&original));
                assert!(if key == "escape" {
                    root.focus.is_focused(window)
                } else {
                    terminal_focus.is_focused(window)
                });
                assert!(
                    !root
                        .session_surfaces
                        .as_ref()
                        .unwrap()
                        .read(cx)
                        .tab_peek_visible()
                );
            });
        }
    }

    #[gpui::test]
    fn tab_peek_translation_does_not_resize_the_terminal(cx: &mut gpui::TestAppContext) {
        use crate::tab_peek::GestureFrame;
        let services = test_services();
        let fixture = SidebarPreviewFixture::make(PreviewScenario::Typical);
        {
            let mut store = services.store.store.write().unwrap();
            store.hydrate(fixture.list);
            store.select(fixture.selected_session_id.unwrap());
        }
        let (root, cx) = cx.add_window_view(move |window, cx| {
            RootView::new(services, false, PreviewScenario::Empty, window, cx)
        });
        cx.simulate_resize(size(px(1000.0), px(700.0)));
        cx.run_until_parked();
        let original = root.read_with(cx, |root, cx| {
            root.terminal.as_ref().unwrap().read(cx).geometry_for_test()
        });
        let selected = root.read_with(cx, |root, _| {
            root.services
                .store
                .store
                .read()
                .unwrap()
                .selected_session_id()
                .cloned()
        });
        for distance in [10.0, 50.0, 140.0, 240.0, 380.0, 180.0, 40.0] {
            root.update(cx, |root, cx| {
                root.session_surfaces.as_ref().unwrap().update(cx, |s, cx| {
                    s.tab_gesture(GestureFrame::Tracking(distance), cx)
                })
            });
            cx.run_until_parked();
            let current = root.read_with(cx, |root, cx| {
                root.terminal.as_ref().unwrap().read(cx).geometry_for_test()
            });
            assert_eq!(
                current, original,
                "peek at {distance} changed terminal geometry"
            );
            assert_eq!(
                root.read_with(cx, |root, _| root
                    .services
                    .store
                    .store
                    .read()
                    .unwrap()
                    .selected_session_id()
                    .cloned()),
                selected
            );
        }
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        assert_eq!(
            root.read_with(cx, |root, cx| root
                .terminal
                .as_ref()
                .unwrap()
                .read(cx)
                .geometry_for_test()),
            original
        );
    }

    #[gpui::test]
    fn horizontal_strip_hosts_the_pane_title_bar_actions(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| cx.set_reduce_motion(true));
        let services = test_services();
        let fixture = SidebarPreviewFixture::make(PreviewScenario::Typical);
        {
            let mut store = services.store.store.write().unwrap();
            store.hydrate(fixture.list);
            store.select(fixture.selected_session_id.unwrap());
            store
                .update_preferences(|prefs| {
                    prefs.tab_orientation = crate::store::TabOrientation::Horizontal;
                    prefs.sidebar_visible = false;
                })
                .unwrap();
        }
        let (root, cx) = cx.add_window_view(move |window, cx| {
            RootView::new(services, false, PreviewScenario::Empty, window, cx)
        });
        cx.simulate_resize(size(px(1000.0), px(700.0)));
        cx.run_until_parked();

        let strip = cx
            .debug_bounds("horizontal-tabs")
            .expect("horizontal strip");
        let new_tab = cx
            .debug_bounds("horizontal-new-tab")
            .expect("new tab control");
        let actions = cx
            .debug_bounds("hosted-header-actions")
            .expect("the strip hosts the pane's title-bar actions");
        assert!(
            actions.left() >= new_tab.right() && actions.right() <= strip.right(),
            "actions sit after the new-tab control inside the strip: {actions:?} vs {new_tab:?}"
        );
        for selector in [
            "session-links-trigger",
            "toggle-inspector",
            "notification-inbox-button",
        ] {
            let control = cx
                .debug_bounds(selector)
                .unwrap_or_else(|| panic!("missing hosted control {selector}"));
            assert!(
                control.top() >= strip.top() && control.bottom() <= strip.bottom(),
                "{selector} must live in the strip: {control:?} vs {strip:?}"
            );
        }
        assert!(
            cx.debug_bounds("show-sidebar").is_none(),
            "no pane title bar remains to carry the top-bar toggle"
        );
        let grid = cx
            .debug_bounds("terminal-grid-surface")
            .expect("grid surface");
        assert!(
            grid.top() < strip.bottom() + px(2.0),
            "the grid reclaims the title bar height directly under the strip: {grid:?}"
        );
        assert!(root.read_with(cx, |root, cx| {
            root.terminal.as_ref().unwrap().read(cx).header_hidden()
        }));

        let trigger = cx.debug_bounds("session-links-trigger").unwrap().center();
        cx.simulate_click(trigger, Modifiers::default());
        cx.run_until_parked();
        let panel = cx
            .debug_bounds("session-links-panel")
            .expect("the hosted trigger still opens the pane's links popover");
        assert!(
            panel.top() >= strip.bottom(),
            "the popover drops from the strip-hosted trigger: {panel:?}"
        );
        cx.simulate_click(trigger, Modifiers::default());
        cx.run_until_parked();

        // Hiding the strip hands the actions back to the pane's own title bar.
        root.update_in(cx, |root, window, cx| {
            root.run_command(CommandId::ToggleSidebar, window, cx)
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("hosted-header-actions").is_none());
        assert!(
            cx.debug_bounds("show-sidebar").is_some(),
            "the pane title bar returns with its top-bar toggle"
        );
        let bell = cx.debug_bounds("notification-inbox-button").unwrap();
        assert!(bell.center().y < px(Metrics::TITLE_BAR));
        assert!(!root.read_with(cx, |root, cx| {
            root.terminal.as_ref().unwrap().read(cx).header_hidden()
        }));

        // Vertical tabs never host actions in a strip.
        root.update_in(cx, |root, window, cx| {
            root.run_command(CommandId::VerticalTabs, window, cx)
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("horizontal-tabs").is_none());
        assert!(cx.debug_bounds("hosted-header-actions").is_none());
        assert!(cx.debug_bounds("toggle-inspector").is_some());
    }

    #[gpui::test]
    fn horizontal_peek_keeps_tabs_stationary_and_terminal_identity(cx: &mut gpui::TestAppContext) {
        use crate::tab_peek::GestureFrame;
        let services = test_services();
        let fixture = SidebarPreviewFixture::make(PreviewScenario::Typical);
        {
            let mut store = services.store.store.write().unwrap();
            store.hydrate(fixture.list);
            store.select(fixture.selected_session_id.unwrap());
            // Start with settled horizontal chrome; this test exercises peek motion.
            store
                .update_preferences(|prefs| {
                    prefs.tab_orientation = crate::store::TabOrientation::Horizontal;
                    prefs.sidebar_visible = false;
                })
                .unwrap();
        }
        let (root, cx) = cx.add_window_view(move |window, cx| {
            RootView::new(services, false, PreviewScenario::Empty, window, cx)
        });
        cx.simulate_resize(size(px(1000.0), px(700.0)));
        cx.run_until_parked();
        let heading = cx.debug_bounds("horizontal-tabs").unwrap();
        let body = cx.debug_bounds("terminal-card-body").unwrap();
        let entity = root.read_with(cx, |root, _| root.terminal.clone().unwrap());
        let original = root.read_with(cx, |root, cx| {
            root.terminal.as_ref().unwrap().read(cx).geometry_for_test()
        });
        let selected = root.read_with(cx, |root, _| {
            root.services
                .store
                .store
                .read()
                .unwrap()
                .selected_session_id()
                .cloned()
        });
        for distance in [10.0, 50.0, 140.0, 240.0, 380.0, 180.0, 40.0] {
            root.update(cx, |root, cx| {
                root.session_surfaces.as_ref().unwrap().update(cx, |s, cx| {
                    s.tab_gesture(GestureFrame::Tracking(distance), cx)
                })
            });
            cx.run_until_parked();
            assert_eq!(cx.debug_bounds("horizontal-tabs").unwrap(), heading);
            let moved = cx.debug_bounds("terminal-card-body").unwrap();
            assert_eq!(moved.size, body.size);
            let overlay = cx.debug_bounds("TAB_PEEK").unwrap();
            assert_eq!(overlay.top(), heading.bottom());
            let current = root.read_with(cx, |root, cx| {
                assert_eq!(root.terminal.as_ref(), Some(&entity));
                root.terminal.as_ref().unwrap().read(cx).geometry_for_test()
            });
            assert_eq!(
                current, original,
                "peek at {distance} changed terminal geometry"
            );
            assert_eq!(
                root.read_with(cx, |root, _| root
                    .services
                    .store
                    .store
                    .read()
                    .unwrap()
                    .selected_session_id()
                    .cloned()),
                selected
            );
        }
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        assert_eq!(cx.debug_bounds("horizontal-tabs").unwrap(), heading);
        assert!(cx.debug_bounds("terminal-card-body").unwrap().top() > body.top());
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(250));
        root.update_in(cx, |_, window, cx| window.simulate_next_frame(cx));
        cx.run_until_parked();
        assert_eq!(cx.debug_bounds("terminal-card-body").unwrap(), body);
        let trigger = cx.debug_bounds("horizontal-peek-tabs").unwrap();
        cx.simulate_click(trigger.center(), Modifiers::default());
        cx.run_until_parked();
        assert!(cx.debug_bounds("TAB_PEEK").is_some());
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(250));
        root.update_in(cx, |_, window, cx| window.simulate_next_frame(cx));
        cx.run_until_parked();
        assert!(cx.debug_bounds("TAB_PEEK").is_none());
        assert_eq!(
            root.read_with(cx, |root, cx| root
                .terminal
                .as_ref()
                .unwrap()
                .read(cx)
                .geometry_for_test()),
            original
        );
    }

    #[gpui::test]
    fn fullscreen_terminal_tracks_drawable_size_with_windowed_restore_bounds(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|cx| cx.set_reduce_motion(true));
        let services = test_services();
        let fixture = SidebarPreviewFixture::make(PreviewScenario::Typical);
        {
            let mut store = services.store.store.write().unwrap();
            store.hydrate(fixture.list);
            store.select(fixture.selected_session_id.unwrap());
        }
        let (root, cx) = cx.add_window_view(move |window, cx| {
            RootView::new(services, false, PreviewScenario::Empty, window, cx)
        });
        root.update_in(cx, |_, window, _| window.activate_window());
        cx.run_until_parked();
        let windowed = size(px(1000.0), px(700.0));
        cx.simulate_resize(windowed);
        cx.run_until_parked();
        let original = root.read_with(cx, |root, cx| {
            root.terminal.as_ref().unwrap().read(cx).geometry_for_test()
        });

        let fullscreen = size(px(1600.0), px(1000.0));
        cx.simulate_resize(fullscreen);
        root.update_in(cx, |_, window, cx| {
            window.toggle_fullscreen();
            // TestWindow::resize changes platform bounds without delivering a
            // resize callback. Preserve the fullscreen drawable size while
            // emulating macOS's saved windowed bounds for window restoration.
            window.resize(windowed);
            assert_eq!(window.viewport_size(), fullscreen);
            assert_eq!(window.inner_window_bounds().get_bounds().size, windowed);
            cx.notify();
        });
        cx.run_until_parked();
        let expanded = root.read_with(cx, |root, cx| {
            root.terminal.as_ref().unwrap().read(cx).geometry_for_test()
        });
        let before = original.0.unwrap();
        let after = expanded.0.unwrap();
        assert_eq!(
            after.width - before.width,
            600.0,
            "terminal must fill fullscreen width"
        );
        assert_eq!(
            after.height - before.height,
            300.0,
            "terminal must fill fullscreen height"
        );
        let before_grid = original.1.unwrap();
        let after_grid = expanded.1.unwrap();
        assert!(after_grid.0 > before_grid.0 && after_grid.1 > before_grid.1);

        root.update_in(cx, |_, window, _| window.toggle_fullscreen());
        cx.simulate_resize(windowed);
        cx.run_until_parked();
        let restored = root.read_with(cx, |root, cx| {
            root.terminal.as_ref().unwrap().read(cx).geometry_for_test()
        });
        assert_eq!(
            restored, original,
            "leaving fullscreen must restore terminal geometry"
        );
    }

    #[gpui::test]
    fn fullscreen_notification_tray_tracks_drawable_size_with_windowed_restore_bounds(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|cx| cx.set_reduce_motion(true));
        let services = test_services();
        let fixture = SidebarPreviewFixture::make(PreviewScenario::Typical);
        {
            let mut store = services.store.store.write().unwrap();
            store.hydrate(fixture.list.clone());
            store.select(fixture.selected_session_id.unwrap());
            let session = fixture
                .list
                .sessions
                .iter()
                .find(|session| Some(&session.id) != store.selected_session_id())
                .unwrap();
            // More than the seven-row cap, so only the viewport can shorten it.
            for index in 0..9 {
                assert!(
                    store.handle_event(diri_client::EventEnvelope {
                        name: diri_proto::EventName::SESSION_NOTIFICATION.into(),
                        params: serde_json::to_value(diri_proto::SessionNotificationEvent {
                            id: format!("tray-{index}"),
                            session_id: session.id.clone(),
                            session_created_at: session.created_at,
                            occurred_at: diri_proto::DateMillis(10_000.0),
                            title: "Build finished".into(),
                            body: format!("Run {index}"),
                        })
                        .unwrap(),
                        seq: index + 1,
                    })
                );
            }
        }
        let (root, cx) = cx.add_window_view(move |window, cx| {
            RootView::new(services, false, PreviewScenario::Empty, window, cx)
        });
        root.update_in(cx, |_, window, _| window.activate_window());
        let tray = |cx: &mut gpui::VisualTestContext| {
            cx.run_until_parked();
            (
                cx.debug_bounds("notification-panel").unwrap().size.width,
                cx.debug_bounds("notification-list").unwrap().size.height,
            )
        };
        // Saved restore bounds and the drawable deliberately differ; see the
        // terminal test above for why `resize` follows `simulate_resize`.
        let enter_fullscreen = |cx: &mut gpui::VisualTestContext,
                                drawable: gpui::Size<gpui::Pixels>,
                                saved: gpui::Size<gpui::Pixels>| {
            cx.simulate_resize(drawable);
            root.update_in(cx, |_, window, cx| {
                window.toggle_fullscreen();
                window.resize(saved);
                assert_eq!(window.viewport_size(), drawable);
                assert_eq!(window.inner_window_bounds().get_bounds().size, saved);
                cx.notify();
            });
        };
        let leave_fullscreen = |cx: &mut gpui::VisualTestContext,
                                saved: gpui::Size<gpui::Pixels>| {
            root.update_in(cx, |_, window, _| window.toggle_fullscreen());
            cx.simulate_resize(saved);
        };

        let short = size(px(420.0), px(320.0));
        let large = size(px(1600.0), px(1000.0));
        let chrome = Metrics::TITLE_BAR + 6.0 + 74.0;
        let constrained = (px(420.0 - 28.0), px(320.0 - chrome));
        let capped = (px(440.0), px(52.0 * 7.0));

        cx.simulate_resize(short);
        root.update_in(cx, |root, window, cx| root.toggle_notifications(window, cx));
        assert_eq!(tray(cx), constrained);

        enter_fullscreen(cx, large, short);
        assert_eq!(
            tray(cx),
            capped,
            "a short saved window must not constrain the fullscreen tray"
        );
        leave_fullscreen(cx, short);
        assert_eq!(tray(cx), constrained, "restoring must constrain it again");

        // Fullscreen on a display smaller than the saved window.
        cx.simulate_resize(large);
        assert_eq!(tray(cx), capped);
        enter_fullscreen(cx, short, large);
        assert_eq!(
            tray(cx),
            constrained,
            "the tray must fit the smaller fullscreen drawable"
        );
        leave_fullscreen(cx, large);
        assert_eq!(tray(cx), capped);
    }

    #[cfg(target_os = "macos")]
    #[gpui::test]
    fn switching_sidebar_conversations_keeps_terminal_focused(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| cx.set_reduce_motion(true));
        let services = test_services();
        let fixture = SidebarPreviewFixture::make(PreviewScenario::Typical);
        let runtime = services.store.clone();
        {
            let mut store = services.store.store.write().unwrap();
            store
                .update_preferences(|prefs| *prefs = fixture.prefs)
                .unwrap();
            store.hydrate(fixture.list);
            store.select(SessionId::new("preview-claude"));
            store.select(SessionId::new("preview-codex"));
        }
        let (root, cx) = cx.add_window_view(move |window, cx| {
            RootView::new(services, false, PreviewScenario::Empty, window, cx)
        });
        root.update_in(cx, |_, window, _| window.activate_window());
        cx.run_until_parked();
        let mut input = root.update(cx, |root, cx| {
            root.terminal
                .as_ref()
                .unwrap()
                .update(cx, |terminal, _| terminal.capture_input_for_test())
        });
        // Explicit inspector navigation may take focus; restoring this same
        // blank browser tab after a conversation switch must not.
        root.update(cx, |root, cx| {
            root.inspector
                .as_ref()
                .unwrap()
                .update(cx, |inspector, cx| {
                    inspector.select_workspace(crate::inspector::WorkspaceSurface::Browser, cx);
                });
        });
        cx.simulate_resize(size(px(1000.0), px(700.0)));
        cx.run_until_parked();
        root.update_in(cx, |root, window, cx| {
            assert!(
                !root.terminal.as_ref().unwrap().read(cx).is_focused(window),
                "explicit browser tab activation still takes focus"
            );
        });
        for peeking in [false, true] {
            if peeking {
                root.update(cx, |root, cx| {
                    root.sidebar.update(cx, |sidebar, cx| sidebar.conceal(cx));
                });
                cx.run_until_parked();
                let edge = cx.debug_bounds("sidebar-peek-edge").unwrap();
                cx.simulate_mouse_move(edge.center(), None, Modifiers::default());
                cx.executor().advance_clock(Duration::from_millis(25));
                cx.run_until_parked();
            }
            for id in ["preview-claude", "preview-codex", "preview-claude"] {
                // Debug selectors are paint-local; refresh the cached sidebar
                // before locating a row, never after the click under test.
                root.update(cx, |root, cx| root.sidebar.update(cx, |_, cx| cx.notify()));
                cx.run_until_parked();
                let session = cx
                    .debug_bounds(if id == "preview-claude" {
                        "SESSION_preview-claude"
                    } else {
                        "SESSION_preview-codex"
                    })
                    .unwrap_or_else(|| panic!("visible row {id} (peek={peeking})"));
                cx.simulate_click(session.center(), Modifiers::default());
                // The inert runtime has no effect worker. Deliver the real local
                // change broadcast so inspector restoration runs after the click.
                runtime.publish_local_change();
                cx.run_until_parked();
                root.update_in(cx, |root, window, cx| {
                    assert!(
                        root.terminal.as_ref().unwrap().read(cx).is_focused(window),
                        "terminal must accept typing after sidebar selection (peek={peeking})"
                    );
                });
                cx.simulate_input("a");
                cx.simulate_keystrokes("enter");
                let mut bytes = Vec::new();
                while let Ok((target, chunk)) = input.try_recv() {
                    assert_eq!(
                        target,
                        SessionId::new(id),
                        "typing must reach the selected conversation"
                    );
                    bytes.extend(chunk);
                }
                assert_eq!(
                    bytes, b"a\r",
                    "typing must work without clicking the terminal"
                );
            }
            if peeking {
                cx.simulate_mouse_move(
                    gpui::point(px(600.0), px(300.0)),
                    None,
                    Modifiers::default(),
                );
                cx.executor().advance_clock(Duration::from_millis(300));
                cx.run_until_parked();
                root.update_in(cx, |root, window, cx| {
                    assert!(!root.sidebar.read(cx).is_peeking());
                    assert!(root.terminal.as_ref().unwrap().read(cx).is_focused(window));
                });
            }
        }
    }

    #[gpui::test]
    fn a_theme_preview_fades_the_whole_window_then_stops_painting(cx: &mut gpui::TestAppContext) {
        use std::time::Duration;

        use diri_term::theme::TermTheme;

        let _fades = crate::app_theme::live::testing::enable_with_manual_clock();
        let services = test_services();
        let store = services.store.clone();
        let (root, cx) = cx.add_window_view(move |window, cx| {
            RootView::new(services, false, PreviewScenario::Empty, window, cx)
        });
        cx.simulate_resize(size(px(1000.0), px(700.0)));
        cx.run_until_parked();
        let saved = TermTheme::CATALOG
            .into_iter()
            .find(|theme| theme.id == store.store.read().unwrap().theme_id())
            .unwrap();
        let shown = |cx: &mut gpui::VisualTestContext| {
            let store = store.store.read().unwrap();
            let _ = cx;
            (
                crate::app_theme::terminal_theme_in(&store),
                crate::app_theme::colors_in(&store),
                crate::app_theme::sidebar_colors_in(&store),
            )
        };
        assert_eq!(shown(cx).0, saved);

        store
            .store
            .write()
            .unwrap()
            .preview_theme(Some(TermTheme::GITHUB_LIGHT.id.into()));
        root.update_in(cx, |_, window, _| window.refresh());
        cx.run_until_parked();

        crate::app_theme::live::testing::advance(Duration::from_millis(70));
        root.update_in(cx, |_, window, cx| {
            assert_eq!(window.simulate_next_frame(cx), 1);
        });
        cx.run_until_parked();
        let (terminal, chrome, sidebar) = shown(cx);
        assert_ne!(terminal.background, saved.background);
        assert_ne!(terminal.background, TermTheme::GITHUB_LIGHT.background);
        // One source: the terminal, the chrome and the sidebar are the same
        // frame of the fade, never one theme each.
        assert_eq!(chrome.background, terminal.background);
        assert_eq!(chrome.primary, terminal.foreground);
        assert_eq!(sidebar.primary, terminal.foreground);

        crate::app_theme::live::testing::advance(Duration::from_millis(400));
        root.update_in(cx, |_, window, cx| {
            assert_eq!(window.simulate_next_frame(cx), 1);
        });
        cx.run_until_parked();
        assert_eq!(shown(cx).0, TermTheme::GITHUB_LIGHT);
        root.update_in(cx, |_, window, cx| {
            assert_eq!(
                window.simulate_next_frame(cx),
                0,
                "a landed fade paints no more"
            );
        });
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
        cx.simulate_keystrokes(&commands::test_chords("cmd-k s e t t i n g s"));
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
        cx.simulate_keystrokes(&commands::test_chords("cmd-k s e t t i n g s enter"));
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
        cx.simulate_keystrokes(&commands::test_chords(
            "cmd-k s e t t i n g s enter down enter",
        ));
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
    #[gpui::test]
    fn recovery_notice_keeps_titlebar_clear_and_supports_copy_and_dismiss(
        cx: &mut gpui::TestAppContext,
    ) {
        let services = test_services();
        let store = services.store.clone();
        let (root, cx) = cx.add_window_view(move |window, cx| {
            let mut root = RootView::new(services, true, PreviewScenario::Typical, window, cx);
            root.preview = false;
            root.services
                .store
                .store
                .write()
                .unwrap()
                .report_prompt_delivery_failure("diagnostic detail".into());
            root
        });
        for width in [640.0, 1000.0] {
            cx.simulate_resize(size(px(width), px(700.0)));
            cx.run_until_parked();
            let card = cx.debug_bounds("RECOVERY_NOTICE").unwrap();
            assert!(card.top() > px(Metrics::TITLE_BAR));
            assert!(card.right() <= px(width - 16.0));
            assert!(card.bottom() <= px(684.0));
            let button = cx.debug_bounds("copy-recovery-details").unwrap();
            assert!(card.contains(&button.center()));
            cx.simulate_click(button.center(), Modifiers::default());
            assert_eq!(
                cx.read_from_clipboard().unwrap().text().as_deref(),
                Some("diagnostic detail")
            );
        }
        let dismiss = cx.debug_bounds("dismiss-recovery-notice").unwrap().center();
        cx.simulate_click(dismiss, Modifiers::default());
        assert!(store.store.read().unwrap().action_failure().is_none());
        root.update(cx, |_, cx| cx.notify());
        cx.run_until_parked();
        assert!(cx.debug_bounds("copy-recovery-details").is_none());
    }

    #[gpui::test]
    fn launch_review_keyboard_opens_exact_session_retries_only_placement_and_dismisses(
        cx: &mut gpui::TestAppContext,
    ) {
        use crate::store::{SpawnOwner, WorkspaceSpawnState, WorkspaceSpawnTarget};
        use diri_proto::workspace::WorkspaceId;
        let services = test_services();
        let runtime = services.store.clone();
        runtime
            .store
            .write()
            .unwrap()
            .hydrate(SidebarPreviewFixture::make(PreviewScenario::Typical).list);
        let (root, cx) = cx.add_window_view(move |window, cx| {
            RootView::new(services, true, PreviewScenario::Typical, window, cx)
        });
        let target = WorkspaceSpawnTarget {
            owner: SpawnOwner::default(),
            workspace: WorkspaceId::new("removed"),
            selected_tab: None,
        };
        let retry_id = runtime
            .store
            .write()
            .unwrap()
            .seed_workspace_spawn_for_test(
                target.clone(),
                WorkspaceSpawnState::Unplaced {
                    session: SessionId::new("preview-codex"),
                    detail: "The workspace was removed. Your session remains available.".into(),
                },
            );
        let dismiss_id = runtime
            .store
            .write()
            .unwrap()
            .seed_workspace_spawn_for_test(
                target,
                WorkspaceSpawnState::Unconfirmed(
                    "Check All sessions before creating another session.".into(),
                ),
            );
        root.update(cx, |root, cx| {
            root.launches_expanded = true;
            cx.notify();
        });
        cx.run_until_parked();
        let button = cx
            .debug_bounds("workspace-launches-toggle")
            .unwrap()
            .center();
        cx.simulate_click(button, Modifiers::default());
        cx.simulate_click(button, Modifiers::default());
        cx.simulate_keystrokes("backspace");
        assert!(
            !runtime
                .store
                .read()
                .unwrap()
                .workspace_spawn_receipts()
                .any(|r| r.id == dismiss_id)
        );
        cx.simulate_keystrokes("enter");
        assert_eq!(
            root.read_with(cx, |root, _| root
                .window_store
                .read()
                .unwrap()
                .selected_session_id()
                .cloned())
                .as_ref(),
            Some(&SessionId::new("preview-codex"))
        );
        assert!(!root.read_with(cx, |root, _| root.launches_expanded));
        cx.simulate_click(button, Modifiers::default());
        cx.simulate_keystrokes("r");
        assert_eq!(
            runtime
                .store
                .read()
                .unwrap()
                .workspace_spawn_receipts()
                .find(|r| r.id == retry_id)
                .unwrap()
                .state,
            WorkspaceSpawnState::Placing(SessionId::new("preview-codex"))
        );
        cx.simulate_keystrokes("backspace");
        assert!(
            runtime
                .store
                .read()
                .unwrap()
                .workspace_spawn_receipts()
                .any(|r| r.id == retry_id),
            "pending placement cannot be cancelled by dismiss"
        );
        cx.simulate_keystrokes("escape");
        assert!(!root.read_with(cx, |root, _| root.launches_expanded));
    }

    #[gpui::test]
    fn project_filter_and_group_collapse_preserve_workspace_terminal_and_restore_agents(
        cx: &mut gpui::TestAppContext,
    ) {
        use diri_proto::workspace::*;
        cx.update(|cx| cx.set_reduce_motion(true));
        let services = test_services();
        let mut fixture = SidebarPreviewFixture::make(PreviewScenario::Typical);
        for session in &mut fixture.list.sessions {
            if session.id.0 == "preview-claude" || session.id.0 == "preview-codex" {
                session.title = if session.id.0 == "preview-claude" {
                    "Build frontend"
                } else {
                    "Review API"
                }
                .into();
                session.title_source = diri_proto::TitleSource::UserRename;
            }
        }
        let tab = |id: &str, title: &str, session: &str| WorkspaceTab {
            id: TabId::new(id),
            title: Some(title.into()),
            focused_pane: PaneId::new(id),
            zoomed_pane: None,
            layout: LayoutNode::Pane {
                id: PaneId::new(id),
                session_id: SessionId::new(session),
            },
        };
        {
            let mut store = services.store.store.write().unwrap();
            store.hydrate(fixture.list);
            store.seed_workspace_snapshot_for_test(WorkspaceSnapshot {
                revision: 7,
                workspaces: vec![
                    WorkspaceRecord {
                        project_id: None,
                        id: WorkspaceId::new("release"),
                        name: "Release".into(),
                        selected_tab: Some(TabId::new("build")),
                        tabs: vec![
                            tab("build", "Build frontend", "preview-claude"),
                            tab("review", "Review notes", "preview-codex"),
                        ],
                    },
                    WorkspaceRecord {
                        project_id: None,
                        id: WorkspaceId::new("remote"),
                        name: "Remote".into(),
                        selected_tab: Some(TabId::new("logs")),
                        tabs: vec![tab("logs", "Server logs", "preview-claude")],
                    },
                ],
                ..Default::default()
            });
            store
                .update_preferences(|prefs| {
                    prefs.active_workspace = Some(WorkspaceId::new("release"));
                    prefs.sidebar_visible = true;
                })
                .unwrap();
        }
        let runtime = services.store.clone();
        let (root, cx) = cx.add_window_view(move |window, cx| {
            RootView::new(services, false, PreviewScenario::Empty, window, cx)
        });
        cx.simulate_resize(size(px(1100.0), px(700.0)));
        cx.run_until_parked();
        let terminal = root.read_with(cx, |root, cx| root.active_terminal(cx).unwrap());
        let before = runtime
            .store
            .read()
            .unwrap()
            .workspace_catalog()
            .snapshot()
            .unwrap()
            .clone();
        assert!(cx.debug_bounds("new-agent").is_some());
        let fold = cx.debug_bounds("PROJECT_preview-dirijor").unwrap().center();
        cx.simulate_click(fold, Modifiers::default());
        cx.run_until_parked();
        assert!(cx.debug_bounds("SESSION_preview-claude").is_none());
        assert_eq!(
            root.read_with(cx, |root, cx| root.active_terminal(cx).unwrap()),
            terminal
        );
        let filter = cx.debug_bounds("sidebar-filter").unwrap().center();
        cx.simulate_click(filter, Modifiers::default());
        cx.simulate_keystrokes("r e v i e w");
        cx.run_until_parked();
        assert!(cx.debug_bounds("SESSION_preview-codex").is_some());
        assert!(cx.debug_bounds("SESSION_preview-claude").is_none());
        assert_eq!(
            root.read_with(cx, |root, cx| root.active_terminal(cx).unwrap()),
            terminal
        );
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("SESSION_preview-codex").is_none(),
            "clear restores the saved collapsed project"
        );
        cx.simulate_click(fold, Modifiers::default());
        cx.run_until_parked();
        assert!(cx.debug_bounds("SESSION_preview-claude").is_some());
        assert!(cx.debug_bounds("SESSION_preview-codex").is_some());
        assert_eq!(
            runtime.store.read().unwrap().workspace_catalog().snapshot(),
            Some(&before)
        );
        assert_eq!(
            root.read_with(cx, |root, cx| root.active_terminal(cx).unwrap()),
            terminal
        );
        cx.simulate_click(filter, Modifiers::default());
        cx.simulate_keystrokes("r e v i e w down");
        cx.run_until_parked();
        assert_eq!(
            root.read_with(cx, |root, _| root.active_workspace.clone()),
            Some(WorkspaceId::new("release")),
            "filter navigation does not activate an agent or replace its layout"
        );
        root.update_in(cx, |root, window, cx| {
            root.run_command(CommandId::HorizontalTabs, window, cx)
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("horizontal-tab-preview-claude").is_some());
        assert!(cx.debug_bounds("horizontal-tab-preview-codex").is_some());
        assert_eq!(
            root.read_with(cx, |root, cx| root.active_terminal(cx).unwrap()),
            terminal
        );
        root.update_in(cx, |root, window, cx| {
            root.run_command(CommandId::VerticalTabs, window, cx)
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("SESSION_preview-codex").is_some());
        assert!(cx.debug_bounds("horizontal-tab-preview-claude").is_none());
        assert_eq!(
            root.read_with(cx, |root, cx| root.active_terminal(cx).unwrap()),
            terminal
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "real local PTYs and native keyboard routing"]
    fn keyboard_workspace_operations_commit_through_engine_without_restarting_ptys() {
        use gpui::HeadlessAppContext;
        let fixture = crate::workspace_fixture::LiveWorkspace::start();
        let platform = gpui_platform::current_platform(true);
        let mut cx = HeadlessAppContext::with_platform(
            platform.text_system(),
            Arc::new(diri_ui::IconAssets),
            gpui_platform::current_headless_renderer,
        );
        cx.update(|cx| {
            crate::fonts::init(cx);
            cx.set_reduce_motion(true);
            commands::bind_keys(cx, &Default::default());
        });
        let services = fixture.services.clone();
        let store = services.store.clone();
        let window = cx
            .open_window(size(px(1100.0), px(700.0)), move |window, cx| {
                cx.new(|cx| RootView::new(services, false, PreviewScenario::Empty, window, cx))
            })
            .unwrap();
        macro_rules! root_read {
            ($f:expr) => {
                cx.update_window(window.into(), |root, _, cx| {
                    let root = root.downcast::<RootView>().unwrap();
                    ($f)(root.read(cx), cx)
                })
                .unwrap()
            };
        }
        macro_rules! root_update {
            ($f:expr) => {
                cx.update_window(window.into(), |root, window, cx| {
                    root.downcast::<RootView>()
                        .unwrap()
                        .update(cx, |root, cx| ($f)(root, window, cx))
                })
                .unwrap()
            };
        }
        macro_rules! keys {
            ($text:expr) => {
                for key in $text.split_whitespace() {
                    cx.update_window(window.into(), |_, window, cx| {
                        window.dispatch_keystroke(gpui::Keystroke::parse(key).unwrap(), cx);
                    })
                    .unwrap();
                    cx.run_until_parked();
                }
            };
        }
        cx.update_window(window.into(), |root, window, cx| {
            window.activate_window();
            root.downcast::<RootView>().unwrap().update(cx, |root, cx| {
                root.workspace_workbench
                    .as_ref()
                    .unwrap()
                    .update(cx, |workbench, cx| workbench.focus(window, cx))
            });
        })
        .unwrap();
        let snapshot = || {
            store
                .store
                .read()
                .unwrap()
                .workspace_catalog()
                .snapshot()
                .unwrap()
                .clone()
        };
        for _ in 0..40 {
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(5));
        }
        let mut revision = snapshot().revision;
        macro_rules! committed {
            () => {{
                let deadline = std::time::Instant::now() + Duration::from_secs(5);
                loop {
                    cx.run_until_parked();
                    let current = snapshot();
                    if current.revision > revision
                        && store.store.read().unwrap().workspace_catalog().can_edit()
                    {
                        assert_eq!(
                            current.revision,
                            revision + 1,
                            "one durable mutation per command"
                        );
                        revision = current.revision;
                        break current;
                    }
                    assert!(
                        std::time::Instant::now() < deadline,
                        "command did not commit revision {}, error={:?}",
                        revision + 1,
                        store.store.read().unwrap().workspace_catalog().error
                    );
                    std::thread::sleep(Duration::from_millis(5));
                }
            }};
        }
        cx.run_until_parked();
        let initial = snapshot().workspaces[0].tabs[0].clone();
        let initial_controllers = root_read!(|_, cx| TerminalPane::controller_counts_for_test(cx));
        keys!("ctrl-alt-left");
        let focused = committed!().workspaces[0].tabs[0].focused_pane.clone();
        assert_ne!(focused, initial.focused_pane);
        keys!("cmd-shift-enter");
        assert_eq!(
            committed!().workspaces[0].tabs[0].zoomed_pane,
            Some(focused)
        );
        keys!("ctrl-alt-right");
        let zoomed = committed!().workspaces[0].tabs[0].clone();
        assert_eq!(zoomed.zoomed_pane.as_ref(), Some(&zoomed.focused_pane));
        keys!("cmd-shift-enter");
        assert!(committed!().workspaces[0].tabs[0].zoomed_pane.is_none());
        let before = snapshot().workspaces[0].tabs[0].layout.clone();
        keys!("cmd-alt-shift-right");
        assert_ne!(committed!().workspaces[0].tabs[0].layout, before);
        // The split action opens a real keyboard-operated session picker.
        keys!("cmd-alt-shift-d");
        cx.run_until_parked();
        assert!(root_read!(|root: &RootView, cx| root
            .sidebar
            .read(cx)
            .workspace_menu_is_open()));
        keys!("down enter");
        let split = committed!().workspaces[0].tabs[0].clone();
        for _ in 0..20 {
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(5));
        }
        let split_controllers = root_read!(|_, cx| TerminalPane::controller_counts_for_test(cx));
        assert_eq!(
            split_controllers.0, initial_controllers.0,
            "a duplicate pane reuses the existing controller"
        );
        assert_eq!(
            split_controllers.1,
            initial_controllers.1 + 1,
            "only one view was mounted"
        );
        fixture.verify_process_identity();
        assert_ne!(split.layout, before);
        for command in [
            CommandId::SwapPaneUp,
            CommandId::MovePaneDown,
            CommandId::RemoveFocusedPane,
        ] {
            root_update!(|root: &mut RootView, window, cx| root.run_command(command, window, cx));
            committed!();
        }
        let persisted = diri_engine::workspace::WorkspaceStore::new(
            fixture.directory.path().join("state.json"),
        )
        .snapshot()
        .unwrap();
        assert_eq!(
            persisted,
            snapshot(),
            "GUI sees the same durable catalog as a new reader"
        );
        assert_eq!(persisted.revision, revision);
        fixture.verify_process_identity();
        if let Ok(output) = std::env::var("DIRI_KEYBOARD_WORKSPACE_SCREENSHOT") {
            cx.capture_screenshot(window.into())
                .unwrap()
                .save(output)
                .unwrap();
        }
        cx.update_window(window.into(), |_, window, _| window.remove_window())
            .unwrap();
        cx.run_until_parked();
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "real disposable PTYs and native screenshot to DIRI_WORKSPACE_LIVE_SCREENSHOT"]
    fn render_workspace_real_pty_geometry_screenshot() {
        use gpui::HeadlessAppContext;
        let output = std::env::var("DIRI_WORKSPACE_LIVE_SCREENSHOT").unwrap();
        let fixture = crate::workspace_fixture::LiveWorkspace::start();
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
        let mut previous_cols = None;
        for width in [1100.0, 720.0] {
            let window = cx
                .open_window(size(px(width), px(700.0)), |window, cx| {
                    cx.new(|cx| {
                        RootView::new(
                            fixture.services.clone(),
                            false,
                            PreviewScenario::Empty,
                            window,
                            cx,
                        )
                    })
                })
                .unwrap();
            cx.update_window(window.into(), |_, window, _| window.activate_window())
                .unwrap();
            cx.run_until_parked();
            let deadline = std::time::Instant::now() + Duration::from_secs(6);
            let expected = loop {
                cx.run_until_parked();
                let expected = cx
                    .update_window(window.into(), |root, window, cx| {
                        let root = root.downcast::<RootView>().unwrap();
                        assert_eq!(
                            root.read(cx).active_workspace.as_ref(),
                            Some(&fixture.workspace)
                        );
                        assert!(window.is_window_active());
                        root.read(cx)
                            .workspace_workbench
                            .as_ref()
                            .unwrap()
                            .read(cx)
                            .send_owned_fixture_input(cx)
                    })
                    .unwrap();
                if expected.len() == 2 {
                    break expected;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "both visible session owners resize"
                );
                std::thread::sleep(Duration::from_millis(5));
            };
            assert!(
                expected
                    .iter()
                    .all(|(_, cols, rows)| *cols < 120 && *rows > 0)
            );
            loop {
                cx.run_until_parked();
                let complete = cx
                    .update_window(window.into(), |root, _, cx| {
                        let root = root.downcast::<RootView>().unwrap();
                        let buffers = root
                            .read(cx)
                            .workspace_workbench
                            .as_ref()
                            .unwrap()
                            .read(cx)
                            .resident_preview_buffers(cx);
                        expected.iter().all(|(id, cols, rows)| {
                            let grid = buffers[id].read().unwrap();
                            let text = (0..grid.rows)
                                .filter_map(|row| grid.row_text_with_columns(row as usize))
                                .map(|(text, _)| text)
                                .collect::<String>();
                            let compact = text
                                .chars()
                                .filter(|ch| !ch.is_whitespace())
                                .collect::<String>();
                            grid.cols == *cols
                                && grid.rows == *rows
                                && compact.contains("Nofixedscreenshotgridisusedhere.")
                                && compact.contains(&format!("columns:{rows}{cols}"))
                        })
                    })
                    .unwrap();
                if complete {
                    break;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "PTY output wraps completely at actual owned geometry"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
            fixture.verify_geometry(&expected);
            cx.update_window(window.into(), |root, window, cx| {
                root.downcast::<RootView>()
                    .unwrap()
                    .update(cx, |root, cx| root.toggle_tab_peek(window, cx));
            })
            .unwrap();
            for _ in 0..10 {
                cx.run_until_parked();
                std::thread::sleep(Duration::from_millis(5));
            }
            fixture.verify_geometry(&expected);
            cx.update_window(window.into(), |root, window, cx| {
                root.downcast::<RootView>()
                    .unwrap()
                    .update(cx, |root, cx| root.toggle_tab_peek(window, cx));
            })
            .unwrap();
            cx.run_until_parked();
            fixture.verify_geometry(&expected);
            eprintln!("window={width}, actual PTY geometry={expected:?}");
            let cols = expected.iter().map(|(_, cols, _)| *cols).sum::<u16>();
            if let Some(previous) = previous_cols {
                assert!(cols < previous, "narrower window changes actual PTY widths");
            }
            previous_cols = Some(cols);
            cx.capture_screenshot(window.into())
                .unwrap()
                .save(&output)
                .unwrap();
            cx.update_window(window.into(), |_, window, _| window.remove_window())
                .unwrap();
            cx.run_until_parked();
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "native multiwindow views backed by disposable Engine PTYs"]
    fn new_window_copies_workspace_and_reuses_controllers_without_spawning_or_closing_sessions() {
        use gpui::HeadlessAppContext;
        let fixture = crate::workspace_fixture::LiveWorkspace::start();
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
        let services = fixture.services.clone();
        let first = cx
            .open_window(size(px(1000.0), px(700.0)), |window, cx| {
                cx.new(|cx| RootView::new(services, false, PreviewScenario::Empty, window, cx))
            })
            .unwrap();
        cx.run_until_parked();
        let controllers_before = cx.update(|cx| TerminalPane::controller_counts_for_test(cx));
        assert_eq!(controllers_before.0, 2);
        let sessions_before = fixture
            .services
            .tokio
            .block_on(fixture.services.store.client().sessions())
            .unwrap();
        // Preferences can reflect another window. Copy this window's context,
        // including explicit All sessions, instead of consulting them again.
        fixture
            .services
            .store
            .store
            .write()
            .unwrap()
            .update_preferences(|prefs| {
                prefs.active_workspace =
                    Some(diri_proto::workspace::WorkspaceId::new("another-window"))
            })
            .unwrap();
        let captured = cx
            .update_window(first.into(), |root, _, cx| {
                root.downcast::<RootView>()
                    .unwrap()
                    .read(cx)
                    .window_workspace()
            })
            .unwrap();
        let services = fixture.services.clone();
        let second = cx
            .open_window(size(px(1000.0), px(700.0)), |window, cx| {
                cx.new(|cx| {
                    RootView::new_with_workspace(
                        services,
                        false,
                        PreviewScenario::Empty,
                        Some(captured.clone()),
                        window,
                        cx,
                    )
                })
            })
            .unwrap();
        cx.run_until_parked();
        assert_eq!(
            cx.update_window(second.into(), |root, _, cx| root
                .downcast::<RootView>()
                .unwrap()
                .read(cx)
                .window_workspace())
                .unwrap(),
            captured
        );
        let controllers_after = cx.update(|cx| TerminalPane::controller_counts_for_test(cx));
        assert_eq!(
            controllers_after.0, controllers_before.0,
            "one shared attachment/controller per SessionId"
        );
        assert!(
            controllers_after.1 > controllers_before.1,
            "second window adds views only"
        );
        let services = fixture.services.clone();
        let all_sessions = cx
            .open_window(size(px(1000.0), px(700.0)), |window, cx| {
                cx.new(|cx| {
                    RootView::new_with_workspace(
                        services,
                        false,
                        PreviewScenario::Empty,
                        Some(None),
                        window,
                        cx,
                    )
                })
            })
            .unwrap();
        cx.run_until_parked();
        assert_eq!(
            cx.update_window(all_sessions.into(), |root, _, cx| root
                .downcast::<RootView>()
                .unwrap()
                .read(cx)
                .window_workspace())
                .unwrap(),
            None
        );
        assert_eq!(
            cx.update_window(first.into(), |root, _, cx| root
                .downcast::<RootView>()
                .unwrap()
                .read(cx)
                .window_workspace())
                .unwrap(),
            captured
        );
        for handle in [all_sessions, second] {
            cx.update_window(handle.into(), |_, window, _| window.remove_window())
                .unwrap();
        }
        cx.run_until_parked();
        assert_eq!(
            cx.update(|cx| TerminalPane::controller_counts_for_test(cx))
                .0,
            controllers_before.0
        );
        let sessions_after = fixture
            .services
            .tokio
            .block_on(fixture.services.store.client().sessions())
            .unwrap();
        assert_eq!(
            sessions_before
                .sessions
                .iter()
                .map(|r| &r.id)
                .collect::<HashSet<_>>(),
            sessions_after
                .sessions
                .iter()
                .map(|r| &r.id)
                .collect::<HashSet<_>>()
        );
        fixture.verify_process_identity();
        cx.update_window(first.into(), |_, window, _| window.remove_window())
            .unwrap();
        cx.run_until_parked();
        fixture.verify_process_identity();
    }

    /// A tab click in the strip opens the project-agent workspace for that
    /// session, so every later "new tab" is placed into that workspace and
    /// must still become the selected session in either orientation.
    #[cfg(target_os = "macos")]
    fn new_session_selected_in_orientation(orientation: crate::store::TabOrientation) {
        use gpui::HeadlessAppContext;
        let fixture = crate::workspace_fixture::LiveWorkspace::start();
        fixture
            .services
            .store
            .store
            .write()
            .unwrap()
            .update_preferences(|prefs| {
                prefs.tab_orientation = orientation;
                prefs.sidebar_visible = orientation == crate::store::TabOrientation::Vertical;
            })
            .unwrap();
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
        let services = fixture.services.clone();
        let window = cx
            .open_window(size(px(1000.0), px(700.0)), |window, cx| {
                cx.new(|cx| RootView::new(services, false, PreviewScenario::Empty, window, cx))
            })
            .unwrap();
        let settle = |cx: &mut HeadlessAppContext| {
            for _ in 0..10 {
                cx.update_window(window.into(), |_, window, cx| {
                    window.simulate_next_frame(cx)
                })
                .unwrap();
                cx.run_until_parked();
                std::thread::sleep(Duration::from_millis(5));
            }
        };
        settle(&mut cx);
        let before: Vec<SessionId> = cx
            .update_window(window.into(), |root, _, cx| {
                let root = root.downcast::<RootView>().unwrap();
                let root = root.read(cx);
                let store = root.window_store.read().unwrap();
                store.sessions().keys().cloned().collect()
            })
            .unwrap();
        assert_eq!(before.len(), 2, "{before:?}");
        cx.update_window(window.into(), |root, _, cx| {
            let root = root.downcast::<RootView>().unwrap();
            root.update(cx, |root, cx| {
                root.sidebar.update(cx, |sidebar, cx| {
                    sidebar
                        .window_store()
                        .write()
                        .unwrap()
                        .select(SessionId::new("build"));
                    cx.emit(crate::sidebar::SidebarEvent::SessionActivated);
                    cx.notify();
                });
            });
        })
        .unwrap();
        for _ in 0..5 {
            settle(&mut cx);
        }
        let active = cx
            .update_window(window.into(), |root, _, cx| {
                let root = root.downcast::<RootView>().unwrap();
                root.read(cx).active_workspace.clone()
            })
            .unwrap();
        assert!(active.is_some(), "project agent workspace did not open");
        cx.update_window(window.into(), |root, window, cx| {
            let root = root.downcast::<RootView>().unwrap();
            root.update(cx, |root, cx| {
                root.run_command(CommandId::NewTerminal, window, cx);
            });
        })
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let created = loop {
            settle(&mut cx);
            let created = cx
                .update_window(window.into(), |root, _, cx| {
                    let root = root.downcast::<RootView>().unwrap();
                    let root = root.read(cx);
                    let store = root.window_store.read().unwrap();
                    store
                        .sessions()
                        .keys()
                        .find(|id| !before.contains(id))
                        .cloned()
                })
                .unwrap();
            if let Some(created) = created {
                break created;
            }
            assert!(Instant::now() < deadline, "session never arrived");
        };
        // Give the launch receipt, the placement, and the window a chance to meet.
        for _ in 0..5 {
            settle(&mut cx);
        }
        let selected = cx
            .update_window(window.into(), |root, _, cx| {
                let root = root.downcast::<RootView>().unwrap();
                let root = root.read(cx);
                let store = root.window_store.read().unwrap();
                store.selected_session_id().cloned()
            })
            .unwrap();
        assert_eq!(selected.as_ref(), Some(&created), "{orientation:?}");
        cx.update_window(window.into(), |_, window, _| window.remove_window())
            .unwrap();
        cx.run_until_parked();
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "disposable real Engine PTYs"]
    fn new_session_is_selected_with_vertical_tabs() {
        new_session_selected_in_orientation(crate::store::TabOrientation::Vertical);
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "disposable real Engine PTYs"]
    fn new_session_is_selected_with_horizontal_tabs() {
        new_session_selected_in_orientation(crate::store::TabOrientation::Horizontal);
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "native window lifetime and disposable real Engine PTYs"]
    fn workspace_launch_finishes_after_initiating_window_closes() {
        use crate::store::WorkspaceSpawnState;
        use gpui::HeadlessAppContext;
        let fixture = crate::workspace_fixture::LiveWorkspace::start();
        let held = fixture.held_spawn();
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
        let services = fixture.services.clone();
        let window = cx
            .open_window(size(px(1000.0), px(700.0)), |window, cx| {
                cx.new(|cx| RootView::new(services, false, PreviewScenario::Empty, window, cx))
            })
            .unwrap();
        cx.run_until_parked();
        let id = cx
            .update_window(window.into(), |root, _, cx| {
                let root = root.downcast::<RootView>().unwrap();
                let root = root.read(cx);
                let target = root
                    .workspace_spawn_target()
                    .expect("window captures its workspace");
                root.services
                    .store
                    .store
                    .write()
                    .unwrap()
                    .request_workspace_spawn(target, held.params.clone())
                    .unwrap()
            })
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !held.entered.exists() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        cx.update_window(window.into(), |_, window, _| window.remove_window())
            .unwrap();
        cx.run_until_parked();
        held.release();
        let state = loop {
            let state = fixture
                .services
                .store
                .store
                .read()
                .unwrap()
                .workspace_spawn_receipts()
                .find(|r| r.id == id)
                .unwrap()
                .state
                .clone();
            if !state.pending() {
                break state;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        };
        let WorkspaceSpawnState::Placed { session, tab } = state else {
            panic!("{state:?}");
        };
        let snapshot = fixture
            .services
            .tokio
            .block_on(fixture.services.store.client().workspaces())
            .unwrap();
        assert!(
            snapshot
                .workspaces
                .iter()
                .find(|w| w.id == fixture.workspace)
                .unwrap()
                .tabs
                .iter()
                .any(|t| t.id == tab)
        );
        let sessions = fixture
            .services
            .tokio
            .block_on(fixture.services.store.client().sessions())
            .unwrap();
        assert!(sessions.sessions.iter().any(|s| s.id == session));
        fixture.verify_process_identity();
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "writes synthetic saved workspace UI to DIRI_WORKSPACE_SCREENSHOT"]
    fn render_workspace_workbench_screenshot() {
        use diri_proto::workspace::*;
        use gpui::HeadlessAppContext;
        let output = std::env::var("DIRI_WORKSPACE_SCREENSHOT").unwrap();
        let platform = gpui_platform::current_platform(true);
        let mut cx = HeadlessAppContext::with_platform(
            platform.text_system(),
            Arc::new(diri_ui::IconAssets),
            gpui_platform::current_headless_renderer,
        );
        cx.update(|cx| {
            crate::fonts::init(cx);
            cx.set_reduce_motion(std::env::var_os("DIRI_WORKSPACE_PEEK").is_none());
        });
        let services = test_services();
        let mut fixture = SidebarPreviewFixture::make(PreviewScenario::Typical);
        if std::env::var_os("DIRI_WORKSPACE_REMOTE_FAILURE").is_some() {
            fixture.list.sessions[0].host = Some("dev-box".into());
            fixture.list.sessions[0].remote_connection = Some(diri_proto::RemoteConnection {
                state: diri_proto::RemoteConnectionState::Failed,
                since: diri_proto::DateMillis(0.0),
            });
        }
        let workspace = WorkspaceId::new("release-workspace");
        let tab = TabId::new("release-tab");
        let first = PaneId::new("coding");
        {
            let mut store = services.store.store.write().unwrap();
            store.hydrate(fixture.list);
            store
                .update_preferences(|prefs| {
                    prefs.terminal_theme = if std::env::var_os("DIRI_VISUAL_LIGHT").is_some() {
                        "dirijor-light"
                    } else {
                        "dirijor-dark"
                    }
                    .into();
                })
                .unwrap();
            store.seed_workspace_snapshot_for_test(WorkspaceSnapshot {
                revision: 7,
                workspaces: vec![WorkspaceRecord {
                    project_id: None,
                    id: workspace.clone(),
                    name: "Release room".into(),
                    selected_tab: Some(tab.clone()),
                    tabs: vec![
                        WorkspaceTab {
                            id: tab,
                            title: Some("Implement and verify".into()),
                            focused_pane: first.clone(),
                            zoomed_pane: None,
                            layout: LayoutNode::Split {
                                id: SplitId::new("split-main"),
                                axis: LayoutAxis::Horizontal,
                                fraction: 0.58,
                                first: Box::new(LayoutNode::Pane {
                                    id: first,
                                    session_id: SessionId::new("preview-claude"),
                                }),
                                second: Box::new(LayoutNode::Pane {
                                    id: PaneId::new("verification"),
                                    session_id: SessionId::new("preview-codex"),
                                }),
                            },
                        },
                        WorkspaceTab {
                            id: TabId::new("notes-tab"),
                            title: Some("Review notes".into()),
                            focused_pane: PaneId::new("notes"),
                            zoomed_pane: None,
                            layout: LayoutNode::Pane {
                                id: PaneId::new("notes"),
                                session_id: SessionId::new("preview-claude"),
                            },
                        },
                    ],
                }],
                ..Default::default()
            });
        }
        {
            let mut store = services.store.store.write().unwrap();
            let mut snapshot = store.workspace_catalog().snapshot().unwrap().clone();
            let tab = &mut snapshot.workspaces[0].tabs[0];
            if std::env::var_os("DIRI_WORKSPACE_NESTED").is_some()
                && let LayoutNode::Split { second, .. } = &mut tab.layout
            {
                **second = LayoutNode::Split {
                    id: SplitId::new("split-detail"),
                    axis: LayoutAxis::Vertical,
                    fraction: 0.6,
                    first: second.clone(),
                    second: Box::new(LayoutNode::Pane {
                        id: PaneId::new("notes-duplicate"),
                        session_id: SessionId::new("preview-claude"),
                    }),
                };
            }
            if std::env::var_os("DIRI_WORKSPACE_ZOOM").is_some() {
                tab.focused_pane = PaneId::new("verification");
                tab.zoomed_pane = Some(tab.focused_pane.clone());
            }
            if let Ok(mode) = std::env::var("DIRI_WORKSPACE_GROUPS") {
                snapshot.workspaces.push(WorkspaceRecord {
                    project_id: None,
                    id: WorkspaceId::new("operations-workspace"),
                    name: "Operations".into(),
                    selected_tab: Some(TabId::new("deployment-tab")),
                    tabs: vec![WorkspaceTab {
                        id: TabId::new("deployment-tab"),
                        title: Some("Watch deployment logs".into()),
                        focused_pane: PaneId::new("deployment-pane"),
                        zoomed_pane: None,
                        layout: LayoutNode::Pane {
                            id: PaneId::new("deployment-pane"),
                            session_id: SessionId::new("preview-codex"),
                        },
                    }],
                });
                if mode == "collapsed" || mode == "filter" {
                    store
                        .update_preferences(|prefs| {
                            prefs.sidebar_collapsed_workspaces.push(workspace.clone())
                        })
                        .unwrap();
                }
            }
            store.seed_workspace_snapshot_for_test(snapshot);
        }
        let width = if std::env::var_os("DIRI_WORKSPACE_NARROW").is_some() {
            720.0
        } else {
            1200.0
        };
        let window = cx
            .open_window(size(px(width), px(800.0)), |window, cx| {
                cx.new(|cx| {
                    let mut root = RootView::new(services, false, PreviewScenario::Empty, window, cx);
                    if let Ok(mode) = std::env::var("DIRI_WORKSPACE_LAUNCHES") {
                        let target = crate::store::WorkspaceSpawnTarget { owner: root.spawn_owner, workspace: workspace.clone(), selected_tab: Some(TabId::new("release-tab")) };
                        let state = if mode == "pending" { crate::store::WorkspaceSpawnState::Creating } else { crate::store::WorkspaceSpawnState::Unplaced { session: SessionId::new("preview-codex"), detail: "The workspace changed while this session was starting. Your session is ready in All sessions. Retry placement to use the current layout.".into() } };
                        root.services.store.store.write().unwrap().seed_workspace_spawn_for_test(target, state);
                        root.launches_expanded = mode != "pending";
                    }
                    root.sidebar.update(cx, |sidebar, cx| {
                        if std::env::var("DIRI_WORKSPACE_GROUPS").as_deref() == Ok("filter") {
                            sidebar.seed_workspace_filter_for_test("review", cx);
                        }
                        sidebar
                            .set_tab_orientation(
                                if std::env::var_os("DIRI_WORKSPACE_HORIZONTAL").is_some() {
                                    crate::store::TabOrientation::Horizontal
                                } else {
                                    crate::store::TabOrientation::Vertical
                                },
                                cx,
                            )
                            .unwrap();
                        sidebar.activate_workspace(Some(workspace), cx);
                    });
                    root
                })
            })
            .unwrap();
        cx.run_until_parked();
        cx.update_window(window.into(), |root, _, cx| {
            let root = root.downcast::<RootView>().unwrap();
            let workbench = root.read(cx).workspace_workbench.clone().unwrap();
            workbench.update(cx, |workbench, cx| workbench.seed_panes_for_test(cx));
        })
        .unwrap();
        cx.run_until_parked();
        if let Ok(mode) = std::env::var("DIRI_WORKSPACE_PEEK") {
            cx.update_window(window.into(), |root, window, cx| {
                let root = root.downcast::<RootView>().unwrap();
                root.update(cx, |root, cx| {
                    root.toggle_tab_peek(window, cx);
                    root.session_surfaces
                        .as_ref()
                        .unwrap()
                        .update(cx, |surface, cx| {
                            let distance = if mode == "overview" { 380.0 } else { 140.0 };
                            surface
                                .tab_gesture(crate::tab_peek::GestureFrame::Tracking(distance), cx);
                            surface
                                .tab_gesture(crate::tab_peek::GestureFrame::Released(distance), cx);
                        });
                });
            })
            .unwrap();
            cx.run_until_parked();
        }
        if std::env::var_os("DIRI_WORKSPACE_SPLIT_PICKER").is_some() {
            cx.update_window(window.into(), |root, window, cx| {
                root.downcast::<RootView>().unwrap().update(cx, |root, cx| {
                    root.run_command(CommandId::SplitPaneBelow, window, cx)
                });
            })
            .unwrap();
            cx.run_until_parked();
        }
        let image = cx.capture_screenshot(window.into()).unwrap();
        image.save(&output).unwrap();
        cx.update_window(window.into(), |_, window, _| window.remove_window())
            .unwrap();
        cx.run_until_parked();
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "writes the complete tab peek workbench to DIRI_PEEK_ROOT_SCREENSHOT"]
    fn render_tab_peek_workbench_screenshot() {
        use diri_term::buffer::GridBuffer;
        use gpui::HeadlessAppContext;
        let output = std::env::var("DIRI_PEEK_ROOT_SCREENSHOT").expect("output path");
        let distance = std::env::var("DIRI_PEEK_DISTANCE")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(140.0);
        let platform = gpui_platform::current_platform(true);
        let mut cx = HeadlessAppContext::with_platform(
            platform.text_system(),
            Arc::new(diri_ui::IconAssets),
            gpui_platform::current_headless_renderer,
        );
        cx.update(|cx| crate::fonts::init(cx));
        let services = test_services();
        let live_source = std::env::var_os("DIRI_PEEK_LIVE")
            .map(|_| crate::tab_preview::screenshot_fixture::Source::new());
        let mut fixture = SidebarPreviewFixture::make(PreviewScenario::Typical);
        let selected = fixture.selected_session_id.clone().unwrap();
        let project = fixture
            .list
            .sessions
            .iter()
            .find(|s| s.id == selected)
            .unwrap()
            .project_id
            .clone();
        fixture.list.sessions.retain(|s| s.project_id == project);
        if std::env::var_os("DIRI_PEEK_REMOTE_STATES").is_some() {
            use diri_proto::{DateMillis, RemoteConnection, RemoteConnectionState};
            let states = [
                RemoteConnectionState::Connected,
                RemoteConnectionState::Connecting,
                RemoteConnectionState::Reconnecting,
                RemoteConnectionState::Failed,
                RemoteConnectionState::Unknown,
                RemoteConnectionState::Exited,
            ];
            for (index, session) in fixture.list.sessions.iter_mut().enumerate() {
                session.host = Some("fixture-host".into());
                session.remote_connection = Some(RemoteConnection {
                    state: states[index % states.len()],
                    since: DateMillis(1.0),
                });
            }
        }
        {
            let mut store = services.store.store.write().unwrap();
            store.hydrate(fixture.list);
            store.select(selected.clone());
            store
                .update_preferences(|p| {
                    p.terminal_theme = if std::env::var_os("DIRI_VISUAL_LIGHT").is_some() {
                        "dirijor-light"
                    } else {
                        "dirijor-dark"
                    }
                    .into()
                })
                .unwrap();
        }
        let width = if std::env::var_os("DIRI_PEEK_NARROW").is_some() {
            640.0
        } else {
            1200.0
        };
        let window = cx.open_window(size(px(width), px(800.0)), |window, cx| {
            cx.new(|cx| {
                let root = RootView::new(services, false, PreviewScenario::Empty, window, cx);
                if std::env::var_os("DIRI_PEEK_HORIZONTAL").is_some() {
                    root.sidebar.update(cx, |sidebar, cx| {
                        sidebar.set_tab_orientation(crate::store::TabOrientation::Horizontal, cx)
                    }).unwrap();
                }
                let mut grid = GridBuffer::new(100, 36);
            let sample = "$ pwd\n/Users/you/work/diri\n\n$ cargo test --workspace\n\nrunning 4 tests\ntest session_identity_survives ... ok\ntest preview_does_not_resize ... ok\ntest controller_stays_attached ... ok\ntest input_returns_to_terminal ... ok\n\ntest result: ok. 4 passed; 0 failed\n\n$ git status --short\n M crates/diri-app/src/tab_peek.rs\n M crates/diri-app/src/root.rs\n\n$ ";
                for (y, line) in sample.lines().enumerate() {
                    for (x, ch) in line.chars().enumerate() {
                        grid.cells[y * 100 + x].scalar = ch as u32;
                    }
                }
                let terminal = root.terminal.as_ref().unwrap();
                terminal.update(cx, |terminal, cx| {
                    terminal.seed_preview_grid_for_test(grid, cx);
                    cx.notify();
                });
                let buffers = terminal.read(cx).resident_preview_buffers();
                root.session_surfaces.as_ref().unwrap().update(cx, |surface, cx| {
                    if let Some(source) = &live_source {
                        surface.configure_preview_fixture(source);
                    }
                    surface.sync_resident_buffers(buffers);
                    surface.tab_gesture(crate::tab_peek::GestureFrame::Tracking(distance), cx);
                });
                root
            })
        }).unwrap();
        cx.run_until_parked();
        if let Some(source) = &live_source {
            let states = cx
                .update_window(window.into(), |root, _, cx| {
                    root.downcast::<RootView>()
                        .unwrap()
                        .read(cx)
                        .session_surfaces
                        .as_ref()
                        .unwrap()
                        .read(cx)
                        .preview_fixture_states()
                })
                .unwrap();
            source.settle(states);
            cx.run_until_parked();
        }
        if let Some(profile) = std::env::var_os("DIRI_PEEK_PROFILE") {
            super::peek_profile::run(&mut cx, window, std::path::Path::new(&profile));
        }
        cx.capture_screenshot(window.into())
            .unwrap()
            .save(output)
            .unwrap();
        cx.update_window(window.into(), |_, window, _| window.remove_window())
            .unwrap();
        cx.run_until_parked();
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "writes the terminal toast preview to DIRI_TERMINAL_TOAST_SCREENSHOT"]
    fn render_terminal_toast_screenshot() {
        use gpui::HeadlessAppContext;
        let output = std::env::var("DIRI_TERMINAL_TOAST_SCREENSHOT").expect("output path");
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
        let mut fixture = SidebarPreviewFixture::make(PreviewScenario::Typical);
        let selected = fixture.selected_session_id.clone().unwrap();
        let session = fixture
            .list
            .sessions
            .iter_mut()
            .find(|s| s.id == selected)
            .unwrap();
        session.kind = diri_proto::AgentKind::CODEX;
        session.host = None;
        session.title = "Image drop regression".into();
        {
            let mut store = services.store.store.write().unwrap();
            store.hydrate(fixture.list);
            store.select(selected);
            store
                .update_preferences(|p| p.terminal_theme = "dirijor-dark".into())
                .unwrap();
        }
        let window = cx.open_window(size(px(1000.0), px(650.0)), |window, cx| {
            cx.new(|cx| {
                let mut root = RootView::new(services, false, PreviewScenario::Empty, window, cx);
                // This fixture has no live Engine; omit its unrelated connecting notice.
                root.preview = true;
                let mut grid = diri_term::buffer::GridBuffer::new(100, 36);
                let sample = "  OpenAI Codex\n\n  /Users/you/work/diri\n\n  Ready to work on your project.\n\n› Ask Codex to do anything";
                for (y, line) in sample.lines().enumerate() {
                    for (x, ch) in line.chars().enumerate() {
                        grid.cells[y * 100 + x].scalar = ch as u32;
                    }
                }
                root.terminal.as_ref().unwrap().update(cx, |terminal, cx| {
                    terminal.seed_preview_grid_for_test(grid, cx);
                    cx.emit(TerminalPaneEvent::Feedback {
                        message: "This terminal is active in another view. Focus it to type here.".into(),
                    });
                });
                root
            })
        }).unwrap();
        cx.run_until_parked();
        cx.capture_screenshot(window.into())
            .unwrap()
            .save(output)
            .unwrap();
        cx.update_window(window.into(), |_, window, _| window.remove_window())
            .unwrap();
        cx.run_until_parked();
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "writes a visual preview to DIRI_RECOVERY_SCREENSHOT"]
    fn render_recovery_notice_screenshot() {
        use gpui::{AppContext as _, HeadlessAppContext};
        let output = std::env::var("DIRI_RECOVERY_SCREENSHOT").expect("output path");
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
        let width = if std::env::var_os("DIRI_RECOVERY_NARROW").is_some() {
            640.0
        } else {
            1000.0
        };
        let window = cx.open_window(size(px(width), px(700.0)), |window, cx| {
            cx.new(|cx| {
                let mut root = RootView::new(services.clone(), true, PreviewScenario::Typical, window, cx);
                root.preview = false;
                let mut store = services.store.store.write().unwrap();
                store.update_preferences(|prefs| {
                    prefs.terminal_theme = if std::env::var_os("DIRI_RECOVERY_LIGHT").is_some() {
                        "dirijor-light".into()
                    } else {
                        "dirijor-dark".into()
                    };
                }).unwrap();
                store.report_prompt_delivery_failure("initial_prompt_delivery_failed: session s_123 was created, but initial prompt delivery was not confirmed: the agent never confirmed that it submitted".into());
                drop(store);
                services.store.publish_local_change();
                root
            })
        }).expect("preview window");
        cx.run_until_parked();
        cx.capture_screenshot(window.into())
            .expect("screenshot")
            .save(output)
            .expect("save");
        cx.update_window(window.into(), |_, window, _| window.remove_window())
            .unwrap();
        cx.run_until_parked();
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "writes deterministic tab orientation screenshots to DIRI_TABS_SCREENSHOTS"]
    fn render_tab_orientation_screenshots() {
        use gpui::{AppContext as _, HeadlessAppContext};
        let output = std::env::var("DIRI_TABS_SCREENSHOTS").expect("DIRI_TABS_SCREENSHOTS");
        std::fs::create_dir_all(&output).unwrap();
        let platform = gpui_platform::current_platform(true);
        let mut cx = HeadlessAppContext::with_platform(
            platform.text_system(),
            Arc::new(diri_ui::IconAssets),
            gpui_platform::current_headless_renderer,
        );
        cx.update(|cx| {
            crate::fonts::init(cx);
            cx.set_reduce_motion(false);
        });
        for (name, orientation, light, width) in [
            (
                "horizontal-dark",
                crate::store::TabOrientation::Horizontal,
                false,
                1000.0,
            ),
            (
                "horizontal-light",
                crate::store::TabOrientation::Horizontal,
                true,
                1000.0,
            ),
            (
                "horizontal-narrow",
                crate::store::TabOrientation::Horizontal,
                false,
                640.0,
            ),
            (
                "vertical-light",
                crate::store::TabOrientation::Vertical,
                true,
                1000.0,
            ),
        ] {
            let services = test_services();
            // `DIRI_VISUAL_SCENARIO`, `DIRI_VISUAL_SELECT=<session id>` and
            // `DIRI_VISUAL_THEME=<theme id>` choose what the strip shows.
            let fixture = SidebarPreviewFixture::make(PreviewScenario::from_env(
                std::env::var("DIRI_VISUAL_SCENARIO").ok().as_deref(),
            ));
            {
                let mut store = services.store.store.write().unwrap();
                store.hydrate(fixture.list);
                store.select(
                    std::env::var("DIRI_VISUAL_SELECT")
                        .map(diri_proto::SessionId::new)
                        .unwrap_or_else(|_| fixture.selected_session_id.unwrap()),
                );
                store
                    .update_preferences(|prefs| {
                        prefs.sidebar_visible = true;
                        prefs.terminal_theme = if light {
                            "dirijor-light"
                        } else {
                            "dirijor-dark"
                        }
                        .into();
                        if let Ok(theme) = std::env::var("DIRI_VISUAL_THEME") {
                            prefs.terminal_theme = theme;
                        }
                    })
                    .unwrap();
            }
            let window = cx
                .open_window(size(px(width), px(700.0)), |window, cx| {
                    cx.new(|cx| RootView::new(services, false, PreviewScenario::Empty, window, cx))
                })
                .unwrap();
            cx.run_until_parked();
            cx.update_window(window.into(), |view, window, cx| {
                view.downcast::<RootView>().unwrap().update(cx, |root, cx| {
                    root.run_command(
                        if orientation == crate::store::TabOrientation::Horizontal {
                            CommandId::HorizontalTabs
                        } else {
                            CommandId::VerticalTabs
                        },
                        window,
                        cx,
                    );
                });
            })
            .unwrap();
            cx.run_until_parked();
            // `DIRI_TABS_PICKER=1` captures the header's project dropdown open
            // over the workbench instead of the bare strip.
            if orientation == crate::store::TabOrientation::Horizontal
                && std::env::var_os("DIRI_TABS_PICKER").is_some()
            {
                cx.update_window(window.into(), |view, window, cx| {
                    view.downcast::<RootView>().unwrap().update(cx, |root, cx| {
                        root.sidebar.update(cx, |sidebar, cx| {
                            sidebar.open_project_picker_for_test(window, cx);
                        });
                    });
                })
                .unwrap();
                cx.run_until_parked();
                // Let the surface's wall-clock entry fade finish so the capture
                // shows the settled material rather than its first frame.
                std::thread::sleep(Duration::from_millis(220));
                cx.update_window(window.into(), |_, window, _| window.refresh())
                    .unwrap();
                cx.run_until_parked();
            }
            cx.capture_screenshot(window.into())
                .unwrap()
                .save(std::path::Path::new(&output).join(format!("{name}.png")))
                .unwrap();
            cx.update_window(window.into(), |_, window, _| window.remove_window())
                .unwrap();
            cx.run_until_parked();
        }
    }

    /// The first-run and resting pages inside the real window, so they are
    /// judged beside the sidebar and title bar they ship with.
    /// `DIRI_FIRST_RUN_SCREENSHOTS=<dir>`; `DIRI_VISUAL_BACKDROP=62616e` paints
    /// a fake desktop behind the window's translucent surfaces.
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "writes first-run window previews to DIRI_FIRST_RUN_SCREENSHOTS"]
    fn render_first_run_window_screenshots() {
        use gpui::{AppContext as _, HeadlessAppContext};
        let output =
            std::env::var("DIRI_FIRST_RUN_SCREENSHOTS").expect("DIRI_FIRST_RUN_SCREENSHOTS");
        std::fs::create_dir_all(&output).unwrap();
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
        let ready: &[&str] = &["claude-code", "codex"];
        for (name, installed, sessions, light) in [
            ("no-agents-dark", &[][..], false, false),
            ("ready-dark", ready, false, false),
            ("ready-light", ready, false, true),
            ("resting-dark", ready, true, false),
        ] {
            let services = test_services();
            {
                let mut store = services.store.store.write().unwrap();
                if sessions {
                    store.hydrate(SidebarPreviewFixture::make(PreviewScenario::Typical).list);
                }
                store.set_agent_catalog(crate::agent_setup::bundled_catalog(installed));
                store
                    .update_preferences(|prefs| {
                        prefs.sidebar_visible = true;
                        prefs.terminal_theme = if light {
                            "dirijor-light"
                        } else {
                            "dirijor-dark"
                        }
                        .into()
                    })
                    .unwrap();
            }
            let window = cx
                .open_window(size(px(1100.0), px(720.0)), |window, cx| {
                    cx.new(|cx| RootView::new(services, false, PreviewScenario::Empty, window, cx))
                })
                .unwrap();
            cx.run_until_parked();
            cx.capture_screenshot(window.into())
                .unwrap()
                .save(std::path::Path::new(&output).join(format!("{name}.png")))
                .unwrap();
            cx.update_window(window.into(), |_, window, _| window.remove_window())
                .unwrap();
            cx.run_until_parked();
        }
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
    fn inspector_close_preserves_new_focus(cx: &mut gpui::TestAppContext) {
        for command in [
            None,
            Some(CommandId::FocusSidebar),
            Some(CommandId::ToggleCommandPalette),
        ] {
            let services = test_services();
            let fixture = SidebarPreviewFixture::make(PreviewScenario::Typical);
            services.store.store.write().unwrap().hydrate(fixture.list);
            let (root, cx) = cx.add_window_view(move |window, cx| {
                RootView::new(services, false, PreviewScenario::Empty, window, cx)
            });
            cx.simulate_resize(size(px(1200.0), px(800.0)));
            root.update(cx, |root, cx| {
                root.preview = false;
                root.inspector_open = true;
                root.inspector_seam = 440.0;
                cx.notify();
            });
            cx.run_until_parked();
            // Move to another keyboard surface while the inspector closes.
            let focused = root.update_in(cx, |root, window, cx| {
                root.run_command(CommandId::ToggleInspector, window, cx);
                if let Some(command) = command {
                    root.run_command(command, window, cx);
                } else {
                    root.terminal
                        .as_ref()
                        .expect("terminal")
                        .update(cx, |terminal, cx| {
                            terminal.focus(window, cx);
                        });
                }
                window.focused(cx).expect("new surface has focus")
            });
            root.update(cx, |root, cx| {
                root.inspector_slide = None;
                root.inspector_seam = 0.0;
                cx.notify();
            });
            cx.run_until_parked();
            root.update_in(cx, |_, window, cx| {
                assert_eq!(
                    window.focused(cx),
                    Some(focused),
                    "inspector unmount must preserve the newly focused {command:?}"
                );
            });
        }
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
        cx.simulate_keystrokes(&commands::test_chords("cmd-shift-d"));
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
    fn titlebar_controls_do_not_arm_window_drag_but_empty_chrome_does(
        cx: &mut gpui::TestAppContext,
    ) {
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
        for selector in [
            "show-sidebar",
            "session-links-trigger",
            "notification-inbox-button",
        ] {
            let control = cx
                .debug_bounds(selector)
                .unwrap_or_else(|| panic!("missing titlebar control {selector}"));
            assert!(
                control.center().y < px(Metrics::TITLE_BAR),
                "fixture must place {selector} in the titlebar: {control:?}"
            );
            cx.simulate_event(gpui::MouseDownEvent {
                position: control.center(),
                modifiers: Modifiers::default(),
                button: MouseButton::Left,
                click_count: 1,
                first_mouse: false,
            });
            assert!(
                !root.read_with(cx, |root, _| root.titlebar_drag_armed),
                "{selector} must remain a click even if the pointer moves by a pixel"
            );
            cx.simulate_event(gpui::MouseUpEvent {
                position: point(px(500.0), px(100.0)),
                modifiers: Modifiers::default(),
                button: MouseButton::Left,
                click_count: 1,
            });
        }

        let trigger = cx.debug_bounds("session-links-trigger").unwrap().center();
        cx.simulate_click(trigger, Modifiers::default());
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("session-links-panel").is_some(),
            "the protected dropdown trigger must still activate normally"
        );

        cx.simulate_click(trigger, Modifiers::default());
        cx.run_until_parked();
        let empty_titlebar = point(px(520.0), px(20.0));
        cx.simulate_event(gpui::MouseDownEvent {
            position: empty_titlebar,
            modifiers: Modifiers::default(),
            button: MouseButton::Left,
            click_count: 1,
            first_mouse: false,
        });
        assert_eq!(
            root.read_with(cx, |root, _| root.titlebar_drag_armed),
            cfg!(target_os = "macos"),
            "macOS arms window move on empty chrome; Linux leaves it to the compositor"
        );
    }

    #[gpui::test]
    fn saved_workspace_auxiliary_uses_focused_parent_without_global_selection(
        cx: &mut gpui::TestAppContext,
    ) {
        use diri_proto::workspace::*;
        let services = test_services();
        let fixture = SidebarPreviewFixture::make(PreviewScenario::Typical);
        let parent = fixture.list.sessions[0].id.clone();
        let other = fixture.list.sessions[1].id.clone();
        let mut child = fixture.list.sessions[0].clone();
        child.id = SessionId::new("saved-pane-shell");
        child.parent = Some(parent.clone());
        child.kind = AgentKind::SHELL;
        child.title = crate::store::AUXILIARY_TERMINAL_TITLE.into();
        let child_id = child.id.clone();
        let workspace = WorkspaceId::new("focused-context");
        let tab = TabId::new("focused-context-tab");
        let pane = PaneId::new("focused-context-pane");
        {
            let mut store = services.store.store.write().unwrap();
            store.hydrate(fixture.list);
            store.upsert_session(child);
            store.select(other.clone());
            store.seed_workspace_snapshot_for_test(WorkspaceSnapshot {
                revision: 1,
                workspaces: vec![WorkspaceRecord {
                    project_id: None,
                    id: workspace.clone(),
                    name: "Window context".into(),
                    selected_tab: Some(tab.clone()),
                    tabs: vec![WorkspaceTab {
                        id: tab,
                        title: None,
                        focused_pane: pane.clone(),
                        zoomed_pane: None,
                        layout: LayoutNode::Pane {
                            id: pane,
                            session_id: parent.clone(),
                        },
                    }],
                }],
                ..Default::default()
            });
        }
        let runtime = services.store.clone();
        let (root, cx) = cx.add_window_view(move |window, cx| {
            RootView::new(services, false, PreviewScenario::Empty, window, cx)
        });
        root.update_in(cx, |root, window, cx| {
            root.activate_saved_workspace(Some(workspace), window, cx)
        });
        cx.simulate_resize(size(px(1100.0), px(800.0)));
        cx.run_until_parked();
        root.update_in(cx, |root, window, cx| {
            assert_eq!(root.active_session_id(cx), Some(parent.clone()));
            assert!(root.open_auxiliary_terminal(window, cx));
            assert_eq!(root.auxiliary_parent, Some(parent.clone()));
            assert_eq!(root.auxiliary_id, Some(child_id));
            assert!(root.inspector_open);
            assert!(root.inspector.as_ref().unwrap().read(cx).is_terminal_tab());
        });
        assert_eq!(
            runtime.store.read().unwrap().selected_session_id(),
            Some(&other)
        );
        cx.run_until_parked();
        root.update_in(cx, |root, window, cx| {
            root.hide_auxiliary_terminal(window, cx);
            assert!(root.auxiliary_terminal.is_none());
            assert!(
                runtime
                    .store
                    .read()
                    .unwrap()
                    .auxiliary_terminal_for(&parent)
                    .is_some()
            );
        });
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
