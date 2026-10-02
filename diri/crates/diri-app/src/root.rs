#[cfg(all(test, target_os = "macos"))]
mod held_hint_frames;
#[cfg(test)]
mod held_hint_tests;
#[cfg(all(test, target_os = "macos"))]
#[path = "root/peek_profile.rs"]
mod peek_profile;
#[cfg(all(test, target_os = "macos"))]
mod progress_frames;
#[cfg(all(test, target_os = "macos"))]
mod project_agent_tests;
#[cfg(all(test, target_os = "macos"))]
mod row_motion_frames;
#[cfg(all(test, target_os = "macos"))]
mod theme_fade_frames;
#[cfg(all(test, target_os = "macos"))]
mod title_settle_frames;
#[cfg(all(test, target_os = "macos"))]
mod whats_new_clips;
#[cfg(test)]
mod whats_new_tests;
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
    NewCodexSession, NewDefaultSession, NewNote, NewTerminal, OpenLauncher, OpenSettings,
    OpenWorktrees, QuoteSelection, QuoteSelectionToSession, RenameSelectedSession, ReopenSession,
    SESSION_NAVIGATION_CONTEXT, SearchNotes, SelectLastSession, SelectNextAttentionSession,
    SelectNextSession, SelectPreviousSession, SelectSession1, SelectSession2, SelectSession3,
    SelectSession4, SelectSession5, SelectSession6, SelectSession7, SelectSession8, ShowTodos,
    ShowWhatsNew, ToggleAuxiliaryTerminal, ToggleCommandPalette, ToggleHistory, ToggleInspector,
    ToggleOverview, ToggleQuickOpen, ToggleSidebar, ToggleTabPeek,
};
use crate::external_drop::ExternalDropAction;
use crate::haptics::{self, Haptic};
use crate::icons::{SymbolWeight, sf_symbol, sf_symbol_weighted};
use crate::inspector::{BrowserAction, InspectorEvent, WorkbenchInspector};
use crate::launcher::{LauncherEvent, LauncherOverlay};
#[cfg(target_os = "macos")]
use crate::macos::browser::NativeBrowser;
use crate::navigation::NavigationOverlay;
use crate::quote::Quote;
use crate::recovery::RecoveryNotice;
use crate::seam::{SeamSlide, toggle_has_settled};
use crate::session_surfaces::SessionSurfaces;
use crate::sidebar::{PreviewScenario, Sidebar, SidebarEvent};
use crate::store::{SpawnOptions, WindowMaterial};
use crate::surface_shell::UtilitySurfaces;
use crate::terminal_pane::{TerminalPane, TerminalPaneEvent, TerminalViewport};
use crate::toast::{Toast, ToastCommand, ToastHandlers, ToastSlot, ToastStyle};
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
                // The press focused the sidebar; the keyboard belongs to the
                // page it opened, so Escape closes settings from there.
                if surfaces.read(cx).is_settings_open() {
                    surfaces.read(cx).focus_handle(cx).focus(window, cx);
                }
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

/// How long a pending Engine connection goes unannounced.
const CONNECTING_NOTICE_GRACE: Duration = Duration::from_millis(1500);

pub struct RootView {
    spawn_owner: crate::store::SpawnOwner,
    window_store: crate::store::WindowStore,
    launches_expanded: bool,
    launches_focus: FocusHandle,
    launch_cursor: Option<u64>,
    launch_scroll: gpui::ScrollHandle,
    active_workspace: Option<diri_proto::workspace::WorkspaceId>,
    /// The last workspace failure shown, and the layout revision it failed at.
    workspace_error: Option<(u64, String)>,
    workspace_workbench: Option<Entity<crate::workspace_workbench::WorkspaceWorkbench>>,
    sidebar: Entity<Sidebar>,
    terminal: Option<Entity<TerminalPane>>,
    navigation: Option<Entity<NavigationOverlay>>,
    session_surfaces: Option<Entity<SessionSurfaces>>,
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
    /// The To-dos page covers the workbench while open; selecting any
    /// session closes it.
    todos_open: bool,
    todos_page: Option<Entity<crate::notes::todos::TodosPage>>,
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
    /// Which end of its travel the seam being dragged is held against.
    seam_limit: haptics::Crossing,
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
    /// The one transient toast; connection recovery renders beside it.
    toast: ToastSlot,
    toast_style: ToastStyle,
    /// Records this window's open and close for the flight recorder.
    _telemetry_window: crate::telemetry::WindowGuard,
    quote_target_picker: Option<QuoteTargetPicker>,
    notification_panel_open: bool,
    /// The system alert asking whether to close sessions, while it is up.
    close_prompt: Option<Task<()>>,
    /// The What's New sheet, while open.
    whats_new: Option<Entity<crate::whats_new::WhatsNewSheet>>,
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
    /// When this window first saw the Engine connection pending. A launch
    /// connects in well under a second, so the notice waits out
    /// [`CONNECTING_NOTICE_GRACE`] instead of greeting every launch.
    connecting_since: Option<Instant>,
    pending_notification_open: Option<(SessionId, Option<String>)>,
    last_quote_surface: QuoteSurface,
    /// Set when opening settings had to reveal a hidden sidebar to put its
    /// navigation somewhere, so closing settings can hide it again.
    sidebar_revealed_for_settings: bool,
    /// The session that held keyboard focus when Settings opened, so closing
    /// Settings can return there instead of leaving the hidden page focused.
    settings_return_terminal: Option<Entity<TerminalPane>>,
    settings_was_open: bool,
    preview: bool,
    preview_scenario: PreviewScenario,
    #[cfg(target_os = "macos")]
    menu_bar: Option<NativeMenuBar>,
    #[cfg(target_os = "macos")]
    notifier: std::rc::Rc<NativeNotifier>,
    /// Hold-⌘ shortcut hints for this window; published while it is key.
    held_hints: crate::held_hints::HeldHints,
    _held_hint_timer: Option<Task<()>>,
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
    /// The connecting notice, once the connection has been pending past the
    /// grace. Until then the window simply shows its sessions, and a timer
    /// brings the notice in if the Engine is genuinely slow.
    fn hold_connecting_notice(
        &mut self,
        notice: Option<RecoveryNotice>,
        connecting: bool,
        cx: &mut Context<Self>,
    ) -> Option<RecoveryNotice> {
        if !connecting {
            self.connecting_since = None;
            return notice;
        }
        let first = self.connecting_since.is_none();
        let since = *self.connecting_since.get_or_insert_with(Instant::now);
        let held = notice
            .as_ref()
            .is_some_and(|notice| notice.kind == crate::recovery::RecoveryKind::Connecting);
        let remaining = CONNECTING_NOTICE_GRACE.saturating_sub(since.elapsed());
        if !held || remaining.is_zero() {
            return notice;
        }
        if first {
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(remaining).await;
                let _ = this.update(cx, |_, cx| cx.notify());
            })
            .detach();
        }
        None
    }

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
        if !preview {
            let todos = crate::notes::todos::TodosModel::global(&services.store, cx);
            sidebar.update(cx, |sidebar, cx| sidebar.set_todos(todos, cx));
        }
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
                TerminalPaneEvent::Feedback { message } => {
                    this.show_feedback("terminal", Toast::info(message.clone()), cx);
                }
                TerminalPaneEvent::ExternalDropFeedback { message } => {
                    this.show_feedback("dropped_files", Toast::warning(message.clone()), cx);
                }
                TerminalPaneEvent::RevealSession(id) => {
                    this.open_workspace_launch_session(id.clone(), window, cx);
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
                        this.show_feedback(
                            "workspace",
                            Toast::info("That target is gone. Open the palette to pick again."),
                            cx,
                        );
                    }
                },
            )
            .detach();
            cx.subscribe_in(
                navigation,
                window,
                |this, _, opened: &crate::navigation::NoteOpened, window, cx| {
                    this.show_opened_note(opened, window, cx);
                },
            )
            .detach();
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
            if matches!(event, SidebarEvent::OpenTodos) {
                this.open_todos(window, cx);
            }
            if matches!(event, SidebarEvent::SessionActivated) {
                this.close_todos(cx);
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
                this.focus_active_terminal(window, cx);
            }
            if let SidebarEvent::Update(command) = event {
                this.services.updates.send(command.clone());
            }
            if matches!(event, SidebarEvent::OpenWhatsNew) {
                this.open_whats_new(window, cx);
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
            cx.observe_in(surfaces, window, |this, surfaces, window, cx| {
                let open = surfaces.read(cx).is_settings_open();
                if open && !this.settings_was_open {
                    this.settings_was_open = true;
                    // Keyboard focus follows the page, so paste and dictation
                    // land here instead of in the session underneath.
                    this.settings_return_terminal = this.terminal_holding_focus(window, cx);
                    surfaces.read(cx).focus_handle(cx).focus(window, cx);
                } else if !open && this.settings_was_open {
                    this.settings_was_open = false;
                    let restore = this.settings_return_terminal.take();
                    // The sidebar was settings navigation until now, so focus
                    // on it is still focus inside settings.
                    if (surfaces
                        .read(cx)
                        .focus_handle(cx)
                        .contains_focused(window, cx)
                        || this.sidebar.read(cx).is_focused(window))
                        && let Some(terminal) = restore.or_else(|| this.active_terminal(cx))
                    {
                        terminal.update(cx, |terminal, cx| terminal.focus(window, cx));
                    }
                }
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
            cx.subscribe_in(
                surfaces,
                window,
                |this, _, event: &crate::surface_shell::UtilitySurfacesEvent, window, cx| {
                    if let crate::surface_shell::UtilitySurfacesEvent::ShowWhatsNew(page) = event {
                        this.open_whats_new_at(*page, window, cx);
                    }
                },
            )
            .detach();
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
                            // Terminal keeps the root focus until its pane exists.
                            // Every other surface owns ⌘W, so focus lands here
                            // instead of on the agent session behind it.
                            if *surface == crate::inspector::WorkspaceSurface::Terminal {
                                window.focus(&this.focus, cx);
                            } else if let Some(inspector) = &this.inspector {
                                let handle = inspector.read(cx).focus_handle(cx);
                                window.focus(&handle, cx);
                            }
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
                    InspectorEvent::WorkspaceClosed {
                        surface,
                        id,
                        terminal_slot,
                    } => {
                        #[cfg(not(target_os = "macos"))]
                        let _ = id;
                        #[cfg(target_os = "macos")]
                        if *surface == crate::inspector::WorkspaceSurface::Browser {
                            this.browser.borrow_mut().close_tab(*id);
                        }
                        if *surface == crate::inspector::WorkspaceSurface::Terminal {
                            this.close_workspace_terminal(*terminal_slot, window, cx);
                        }
                        if !this.focus_remaining_workspace(window, cx) {
                            window.focus(&this.focus, cx);
                        }
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
        // Once, the first time a build that records runs: say so, and where
        // to turn it off. The standard toast, held a little longer.
        if !preview && crate::telemetry::take_first_run_notice() {
            cx.spawn(async move |this, cx| {
                let _ = this.update(cx, |this, cx| {
                    this.show_toast(
                        Toast::info("diri sends crash and error reports")
                            .detail("Terminal contents never leave your Mac.")
                            .action("Settings", ToastCommand::OpenPrivacySettings)
                            .hold(Duration::from_secs(20)),
                        cx,
                    );
                });
            })
            .detach();
        }
        #[cfg(target_os = "macos")]
        let notifier = crate::application_notifications::notifier(cx);

        let activation = cx.observe_window_activation(window, move |this, window, cx| {
            if !window.is_window_active() {
                let effect = this.held_hints.deactivated();
                this.apply_held_hint_effect(effect, window, cx);
                if let Some(surfaces) = &this.session_surfaces {
                    surfaces.update(cx, |s, cx| {
                        s.cancel_overview_pinch(cx);
                        s.cancel_tab_peek_immediately(cx);
                    });
                }
            }
            this.window_store
                .write()
                .expect("session store lock poisoned")
                .set_active(window.is_window_active());
        });
        // Every key in this window, before any binding runs: a key while ⌘ is
        // held is a shortcut, so hold-⌘ hints must stand down for it.
        let held_hint_root = cx.weak_entity();
        let held_hint_window = window.window_handle();
        let held_hint_keys = cx.intercept_keystrokes(move |event, window, cx| {
            if window.window_handle() != held_hint_window
                || matches!(
                    event.keystroke.key.as_str(),
                    "platform" | "shift" | "control" | "alt" | "function"
                )
            {
                return;
            }
            let now = crate::held_hints::now(cx);
            let _ = held_hint_root.update(cx, |this, cx| {
                let effect = this.held_hints.key_down(now);
                this.apply_held_hint_effect(effect, window, cx);
            });
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
                            if let Some(toast) = status.in_app_banner {
                                this.show_toast(toast, cx);
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
                            let error = {
                                let store = this.window_store.read().expect("store");
                                let catalog = store.workspace_catalog();
                                catalog.error.clone().map(|error| {
                                    (catalog.snapshot().map_or(0, |s| s.revision), error)
                                })
                            };
                            // Each edit clears the error, so one rejection
                            // repeated per click would toast per click. Say it
                            // once until the layout moves on.
                            if let Some(error) = error
                                && this.workspace_error.as_ref() != Some(&error)
                            {
                                this.workspace_error = Some(error.clone());
                                this.show_feedback(
                                    "workspace_rejected",
                                    Toast::error("Workspace change wasn’t saved").detail(error.1),
                                    cx,
                                );
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
            todos_open: false,
            todos_page: None,
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
            seam_limit: haptics::Crossing::default(),
            window_bounds_save: None,
            toast: ToastSlot::default(),
            toast_style: ToastStyle::from_env(),
            _telemetry_window: crate::telemetry::WindowGuard::new("main", window),
            quote_target_picker: None,
            notification_panel_open: false,
            close_prompt: None,
            whats_new: None,
            main_viewport: gpui::Size::default(),
            notification_filter_unread: true,
            notification_selected: 0,
            notification_scroll: gpui::UniformListScrollHandle::new(),
            notification_scroller: diri_ui::ScrollerState::new(),
            notification_options_open: false,
            notification_focus: cx.focus_handle(),
            pending_notification_open: None,
            connecting_since: None,
            notification_health:
                "Use Test alert to check macOS delivery. Notifications remain available here."
                    .into(),
            last_quote_surface: QuoteSurface::default(),
            sidebar_revealed_for_settings: false,
            settings_return_terminal: None,
            settings_was_open: false,
            preview,
            preview_scenario,
            #[cfg(target_os = "macos")]
            menu_bar,
            #[cfg(target_os = "macos")]
            notifier,
            held_hints: crate::held_hints::HeldHints::default(),
            _held_hint_timer: None,
            _subscriptions: std::iter::once(activation)
                .chain(std::iter::once(held_hint_keys))
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
                            this.show_feedback("workspace", Toast::info(message.clone()), cx)
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
                            TerminalPaneEvent::RevealSession(id),
                        ) => this.open_workspace_launch_session(id.clone(), window, cx),
                        crate::workspace_workbench::WorkspaceWorkbenchEvent::Terminal(
                            TerminalPaneEvent::Feedback { message },
                        ) => this.show_feedback("terminal", Toast::info(message.clone()), cx),
                        crate::workspace_workbench::WorkspaceWorkbenchEvent::Terminal(
                            TerminalPaneEvent::ExternalDropFeedback { message },
                        ) => {
                            this.show_feedback("dropped_files", Toast::warning(message.clone()), cx)
                        }
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

    /// The window's standard toast. `kind` is a fixed label for the flight
    /// recorder; the toast's copy may carry runtime detail and is never
    /// recorded.
    fn show_feedback(&mut self, kind: &'static str, toast: Toast, cx: &mut Context<Self>) {
        diri_telemetry::event!("ui.toast", kind = kind);
        self.show_toast(toast, cx);
    }

    fn show_toast(&mut self, toast: Toast, cx: &mut Context<Self>) {
        let timer = self.toast.show(toast);
        self.arm_toast_timer(timer, cx);
        cx.notify();
    }

    fn arm_toast_timer(&mut self, timer: Option<crate::toast::ToastTimer>, cx: &mut Context<Self>) {
        let Some(timer) = timer else {
            return;
        };
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(timer.after).await;
            let _ = this.update(cx, |this, cx| {
                if this.toast.expire(timer.token) {
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn run_toast_command(
        &mut self,
        command: ToastCommand,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match command {
            ToastCommand::OpenPrivacySettings => {
                self.toast.dismiss();
                if let Some(surfaces) = &self.utility_surfaces {
                    surfaces.update(cx, |surfaces, cx| {
                        surfaces.open_settings(cx);
                        surfaces.open_settings_tab(crate::settings::SettingsTab::General, cx);
                        surfaces.focus_handle(cx).focus(window, cx);
                    });
                }
            }
            ToastCommand::RetryConnection => {
                self.window_store
                    .write()
                    .expect("session store lock poisoned")
                    .retry_connection();
            }
            ToastCommand::RetryAction => {
                self.window_store
                    .write()
                    .expect("session store lock poisoned")
                    .retry_last_action();
            }
            ToastCommand::CopyDetails(detail) => {
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(detail));
            }
        }
        cx.notify();
    }

    fn frame_context(&self, cx: &App) -> crate::telemetry::FrameContext {
        let surface = if self
            .utility_surfaces
            .as_ref()
            .is_some_and(|surfaces| surfaces.read(cx).is_open())
        {
            "settings"
        } else if self
            .navigation
            .as_ref()
            .is_some_and(|navigation| navigation.read(cx).is_open())
        {
            "palette"
        } else if self.launcher.read(cx).is_open() {
            "launcher"
        } else {
            "workbench"
        };
        crate::telemetry::FrameContext {
            surface,
            workspace: self.active_workspace.is_some(),
        }
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

    /// The terminal that currently owns keyboard focus, including the shell
    /// docked in the right sidebar. `None` when focus is already elsewhere.
    fn terminal_holding_focus(&self, window: &Window, cx: &App) -> Option<Entity<TerminalPane>> {
        if let Some(terminal) = &self.auxiliary_terminal
            && terminal.read(cx).is_focused(window)
        {
            return Some(terminal.clone());
        }
        self.active_terminal(cx)
            .filter(|terminal| terminal.read(cx).is_focused(window))
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
            self.show_feedback(
                "quote",
                Toast::info("Select text, a diff hunk or a Markdown turn to quote"),
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
                self.show_feedback("quote", Toast::info("Start an agent to quote into"), cx);
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
            self.show_feedback("quote", Toast::info("Select an agent to quote into"), cx);
            return;
        };
        if !self
            .quote_targets()
            .iter()
            .any(|session| session.id == target)
        {
            self.show_feedback(
                "quote",
                Toast::info("Quotes go to an agent, not a shell"),
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
            self.show_feedback("quote", Toast::info("That session no longer exists"), cx);
            return;
        };
        if !is_quote_target(&target_record) {
            self.show_feedback(
                "quote",
                Toast::info("Quotes go to an agent, not a shell"),
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
            self.show_feedback(
                "quote",
                Toast::info("That session is gone. Pick another."),
                cx,
            );
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
                CommandId::SearchNotes,
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
                self.show_feedback(
                    "workspace",
                    Toast::info("Open a workspace to arrange its panes"),
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
                if self.spawn_default() {
                    self.focus_spawned_session(window, cx);
                } else {
                    self.open_launcher(&OpenLauncher, window, cx);
                }
            }
            CommandId::NewTerminal => {
                if self.spawn(None) {
                    self.focus_spawned_session(window, cx);
                }
            }
            CommandId::NewCodexSession => {
                if self.spawn(Some(AgentKind::CODEX)) {
                    self.focus_spawned_session(window, cx);
                } else {
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
            CommandId::SearchNotes => {
                if let Some(navigation) = &self.navigation {
                    navigation.update(cx, |navigation, cx| {
                        navigation.toggle_search_notes(&SearchNotes, window, cx)
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
            CommandId::ShowTodos => {
                if let Some(navigation) = &self.navigation {
                    navigation.update(cx, |navigation, cx| navigation.dismiss(cx));
                }
                if self.todos_open {
                    self.close_todos(cx);
                    self.focus_active_terminal(window, cx);
                } else {
                    self.open_todos(window, cx);
                }
            }
            CommandId::NewNote => {
                if let Some(navigation) = &self.navigation {
                    navigation.update(cx, |navigation, cx| navigation.dismiss(cx));
                }
                if self.spawn_note() {
                    self.focus_spawned_session(window, cx);
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
                    let opening = !surfaces.read(cx).is_settings_open();
                    if opening {
                        self.settings_return_terminal = self.terminal_holding_focus(window, cx);
                        self.settings_was_open = true;
                    }
                    surfaces.update(cx, |surfaces, cx| surfaces.toggle_settings(cx));
                    if surfaces.read(cx).is_settings_open() {
                        surfaces.read(cx).focus_handle(cx).focus(window, cx);
                    } else {
                        self.settings_was_open = false;
                        let restore = self.settings_return_terminal.take();
                        if let Some(terminal) = restore.or_else(|| self.active_terminal(cx)) {
                            terminal.update(cx, |terminal, cx| terminal.focus(window, cx));
                        }
                    }
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
                    self.show_feedback(
                        "prefs",
                        Toast::error("Couldn’t save the tab layout").detail(error.to_string()),
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
                        self.show_feedback(
                            "prefs",
                            Toast::error("Couldn’t save the tab bar setting")
                                .detail(error.to_string()),
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
            CommandId::ShowWhatsNew => self.open_whats_new_at(0, window, cx),
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

    /// ⌥⌘N: a note Session in the current project, created like ⌘T creates
    /// a terminal. The Engine writes the note's file and the spawn reply
    /// selects it.
    fn spawn_note(&self) -> bool {
        if self.preview {
            return false;
        }
        self.window_store
            .write()
            .expect("session store lock poisoned")
            .spawn_kind(AgentKind::NOTE, SpawnOptions::default());
        true
    }

    /// Opens the What's New sheet on the releases not seen yet, or on the
    /// newest one when opened with nothing new, and marks them seen.
    pub(crate) fn open_whats_new(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_whats_new_at(0, window, cx);
    }

    /// [`Self::open_whats_new`] on highlight `page` (Settings' thumbnails).
    pub(crate) fn open_whats_new_at(
        &mut self,
        page: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use crate::whats_new::{WhatsNewEvent, WhatsNewSheet, current_version, latest, unseen};
        if self.whats_new.is_some() {
            return;
        }
        let current = current_version();
        let runtime = Arc::clone(&self.services.store);
        let releases = {
            let store = runtime.store.read().expect("session store lock poisoned");
            let unseen = unseen(&store.preferences().whats_new_seen_version, &current);
            if unseen.is_empty() {
                latest(&current)
            } else {
                unseen
            }
        };
        self.sidebar.read(cx).mark_whats_new_seen();
        if releases.is_empty() {
            return;
        }
        let sheet = cx.new(|cx| WhatsNewSheet::new(&releases, runtime, cx));
        if page > 0 {
            sheet.update(cx, |sheet, cx| sheet.go(page, window, cx));
        }
        cx.subscribe_in(
            &sheet,
            window,
            |this, _, event: &WhatsNewEvent, window, cx| {
                this.close_whats_new(window, cx);
                match event {
                    WhatsNewEvent::Close => {}
                    WhatsNewEvent::Run(command) => this.run_command(*command, window, cx),
                    WhatsNewEvent::ReleaseNotes => {
                        if let Some(surfaces) = &this.utility_surfaces {
                            surfaces.update(cx, |surfaces, cx| {
                                surfaces.open_settings(cx);
                                surfaces
                                    .open_settings_tab(crate::settings::SettingsTab::WhatsNew, cx);
                                surfaces.focus_handle(cx).focus(window, cx);
                            });
                        }
                    }
                }
            },
        )
        .detach();
        sheet.read(cx).focus_handle(cx).focus(window, cx);
        self.whats_new = Some(sheet);
        self.sidebar.update(cx, |_, cx| cx.notify());
        cx.notify();
    }

    fn close_whats_new(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(sheet) = self.whats_new.take() {
            sheet.update(cx, |sheet, cx| sheet.release(window, cx));
            self.focus_active_terminal(window, cx);
            cx.notify();
        }
    }

    fn open_todos(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.todos_page.is_none() {
            let runtime = Arc::clone(&self.services.store);
            let model = crate::notes::todos::TodosModel::global(&runtime, cx);
            let page = cx.new(|cx| crate::notes::todos::TodosPage::new(runtime, model, cx));
            cx.subscribe_in(&page, window, |this, _, event, window, cx| {
                use crate::notes::todos::TodosEvent;
                let (session, block) = match event {
                    TodosEvent::OpenNote { session, block } => (session.clone(), Some(*block)),
                    TodosEvent::OpenSession(session) => (session.clone(), None),
                };
                this.close_todos(cx);
                this.window_store
                    .write()
                    .expect("session store lock poisoned")
                    .select(session);
                if let (Some(block), Some(terminal)) = (block, &this.terminal) {
                    terminal.update(cx, |terminal, _| terminal.reveal_note_block(block));
                }
                this.focus_active_terminal(window, cx);
                cx.notify();
            })
            .detach();
            self.todos_page = Some(page);
        }
        self.todos_open = true;
        self.sidebar
            .update(cx, |sidebar, cx| sidebar.set_todos_active(true, cx));
        window.focus(&self.focus, cx);
        cx.notify();
    }

    /// A note the palette just opened (selected, unarchived or adopted):
    /// leave the To-dos page, put the caret on the block that matched and
    /// focus the editor.
    fn show_opened_note(
        &mut self,
        opened: &crate::navigation::NoteOpened,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_todos(cx);
        if let (Some(block), Some(terminal)) = (opened.block, &self.terminal) {
            terminal.update(cx, |terminal, _| {
                terminal.reveal_block_in_note(opened.note_id.clone(), block)
            });
        }
        self.focus_active_terminal(window, cx);
        cx.notify();
    }

    fn close_todos(&mut self, cx: &mut Context<Self>) {
        if !self.todos_open {
            return;
        }
        self.todos_open = false;
        self.sidebar
            .update(cx, |sidebar, cx| sidebar.set_todos_active(false, cx));
        cx.notify();
    }

    fn focus_active_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(terminal) = self.active_terminal(cx) {
            terminal.update(cx, |terminal, cx| terminal.focus(window, cx));
            self.sync_auxiliary_terminal(window, cx);
        } else {
            window.focus(&self.focus, cx);
        }
    }

    /// A session the user just spawned owns the keyboard. The pane also
    /// refocuses when the spawn reply selects the new id, but until then
    /// whatever held focus (sidebar, ⌘J pane, inspector) kept swallowing
    /// keys, and an open launcher kept covering the pane.
    fn focus_spawned_session(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.launcher.read(cx).is_open() {
            self.launcher
                .update(cx, |launcher, cx| launcher.dismiss(cx));
        }
        self.focus_active_terminal(window, cx);
        cx.notify();
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
        if let Some(inspector) = self.inspector.clone()
            && (self.active_workspace.is_some() || inspector.read(cx).workspace_needs_terminal())
        {
            // ⌘J closes only the focused inspector terminal. A just-opened panel
            // still has its toggle stamp, so clear it the way the Close control does.
            if self.inspector_open
                && inspector.read(cx).is_terminal_tab()
                && self
                    .auxiliary_terminal
                    .as_ref()
                    .is_some_and(|terminal| terminal.read(cx).is_focused(window))
            {
                self.inspector_toggled_at = None;
                self.set_inspector_open(false, cx);
                self.inspector_toggled_at = None;
                if !self.inspector_open {
                    window.focus(&self.focus, cx);
                }
                return true;
            }
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

    /// The workspace-tab X closes that slot's shell. Collapse stays on
    /// `hide_auxiliary_terminal` and keeps the process.
    fn close_workspace_terminal(
        &mut self,
        slot: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(slot) = slot else {
            return;
        };
        let Some(parent) = self.active_session_id(cx) else {
            return;
        };
        let removed = {
            let mut store = self
                .window_store
                .write()
                .expect("session store lock poisoned");
            let Some(id) = store
                .auxiliary_terminal_for_slot(&parent, slot)
                .map(|session| session.id.clone())
            else {
                return;
            };
            store.remove_sessions(vec![id.clone()]);
            id
        };
        // The next terminal tab is already selected. Point the pane at its
        // shell instead of tearing it down and attaching again.
        if self
            .inspector
            .as_ref()
            .is_some_and(|inspector| inspector.read(cx).is_terminal_tab())
        {
            self.sync_auxiliary_terminal(window, cx);
            return;
        }
        if self.auxiliary_id.as_ref() != Some(&removed) {
            return;
        }
        self.auxiliary_terminal = None;
        self.auxiliary_id = None;
        self.auxiliary_parent = None;
        self.auxiliary_spawn_parent = None;
        if let Some(inspector) = &self.inspector {
            inspector.update(cx, |inspector, cx| inspector.set_terminal_surface(None, cx));
        }
        let workspace_remains = self
            .inspector
            .as_ref()
            .is_some_and(|inspector| inspector.read(cx).has_active_workspace());
        if !workspace_remains && let Some(primary) = self.active_terminal(cx) {
            primary.update(cx, |terminal, cx| terminal.focus(window, cx));
        }
        cx.notify();
    }

    /// After ⌘W, stay on the inspector tab that replaced the closed one.
    fn focus_remaining_workspace(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(inspector) = &self.inspector else {
            return false;
        };
        if !inspector.read(cx).has_active_workspace() {
            return false;
        }
        if inspector.read(cx).is_terminal_tab() {
            if let Some(terminal) = &self.auxiliary_terminal {
                terminal.update(cx, |terminal, cx| terminal.focus(window, cx));
            }
            return true;
        }
        inspector.update(cx, |inspector, cx| {
            inspector.focus_active_surface(window, cx)
        });
        true
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

            if let Some(terminal) = &self.auxiliary_terminal {
                let id = session.id.clone();
                terminal.update(cx, |terminal, cx| terminal.show_session(id, window, cx));
                let should_focus = self.auxiliary_spawn_parent.as_ref() == Some(&parent);
                self.auxiliary_id = Some(session.id.clone());
                self.auxiliary_parent = Some(parent);
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
            && self.auxiliary_id.is_some()
        {
            let closed_tab = self.inspector.as_ref().is_some_and(|inspector| {
                inspector.update(cx, |inspector, cx| inspector.close_active_terminal(cx))
            });
            if !closed_tab && let Some(id) = self.auxiliary_id.clone() {
                self.window_store
                    .write()
                    .expect("session store lock poisoned")
                    .remove_sessions(vec![id]);
                if let Some(terminal) = &self.terminal {
                    terminal.update(cx, |terminal, cx| terminal.focus(window, cx));
                }
            }
            return;
        }
        let closed_workspace = self.inspector.as_ref().is_some_and(|inspector| {
            inspector.update(cx, |inspector, cx| {
                inspector.close_focused_workspace(window, cx)
            })
        });
        if closed_workspace {
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
        let now = crate::held_hints::now(cx);
        let effect = self.held_hints.modifiers_changed(event.modifiers, now);
        self.apply_held_hint_effect(effect, window, cx);
        if let Some(surfaces) = &self.session_surfaces {
            surfaces.update(cx, |surfaces, cx| {
                surfaces.handle_modifiers_changed(event, window, cx);
            });
        }
    }

    /// Carries out what the hold-⌘ state machine asked for: start the one-shot
    /// hold timer, or publish the new visibility to the views that paint hints.
    fn apply_held_hint_effect(
        &mut self,
        effect: crate::held_hints::HintEffect,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use crate::held_hints::{HOLD_DELAY, HeldHintsState, HintEffect};
        match effect {
            HintEffect::None => {}
            HintEffect::Arm(generation) => {
                self._held_hint_timer = Some(cx.spawn_in(window, async move |this, cx| {
                    cx.background_executor().timer(HOLD_DELAY).await;
                    let _ = crate::floating::update_in_owner(&this, cx, |this, window, cx| {
                        this._held_hint_timer = None;
                        let now = crate::held_hints::now(cx);
                        let effect = this.held_hints.delay_elapsed(generation, now);
                        this.apply_held_hint_effect(effect, window, cx);
                    });
                }));
            }
            HintEffect::Repaint => {
                HeldHintsState::publish(window.window_handle().window_id(), self.held_hints, cx);
                cx.notify();
            }
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
        let applied = self.sidebar.read(cx).width();
        self.seam_met_limit("sidebar-seam", width, applied, pointer_x);
    }

    /// One tick when a dragged seam stops following the pointer because it
    /// reached the end of its travel. The seams have no snap points, so the
    /// ends are the only thresholds a resize crosses.
    fn seam_met_limit(&mut self, seam: &'static str, requested: f32, applied: f32, pointer: f32) {
        let limit = (requested != applied).then(|| haptics::key(seam, requested > applied));
        if let Some(target) = self
            .seam_limit
            .moved_to(limit, gpui::point(px(pointer), px(0.0)))
        {
            haptics::perform(Haptic::Limit, target);
        }
    }

    fn drag_terminal_resize(&mut self, pointer_y: f32, cx: &mut Context<Self>) {
        let Some((origin_y, base_height)) = self.terminal_resize_origin else {
            return;
        };
        let previous = self.workbench_layout;
        let height = base_height + pointer_y - origin_y;
        self.workbench_layout
            .resize_primary(height, self.terminal_available_height);
        let applied = WorkbenchLayout::clamped_primary(height, self.terminal_available_height);
        self.seam_met_limit("terminal-seam", height, applied, pointer_y);
        if self.workbench_layout != previous {
            cx.notify();
        }
    }

    fn finish_terminal_resize(&mut self, cx: &mut Context<Self>) {
        if self.terminal_resize_origin.take().is_none() {
            return;
        }
        self.seam_limit.reset();
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
            self.seam_limit.reset();
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
                    .debug_selector(|| "inspector-resize-handle".into())
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
        let requested = base_width - pointer_x + origin_x;
        let width = requested.clamp(
            300.0_f32.min(self.inspector_max_width),
            self.inspector_max_width,
        );
        self.seam_met_limit("inspector-seam", requested, width, pointer_x);
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
        self.seam_limit.reset();
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
            let page_geometry = self
                .terminal
                .as_ref()
                .filter(|_| self.active_workspace.is_none())
                .and_then(|terminal| terminal.read(cx).page_geometry());
            surfaces.update(cx, |surfaces, cx| {
                surfaces.sync_resident_buffers(buffers);
                surfaces.set_tab_peek_region(sidebar_width, tabs_height, card_width, cx);
                // Last frame's primary pane, which is the page a pinch picks
                // up; the whole card when no pane has been laid out yet.
                let page = page_geometry.unwrap_or((
                    crate::terminal_pane::TerminalViewport {
                        x: sidebar_width,
                        y: tabs_height,
                        width: card_width,
                        height: card_height,
                    },
                    0.0,
                ));
                surfaces.set_page_region(page.0, page.1);
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
                        // The pane sits under the inspector header. Counting
                        // that header in the height sizes the PTY past the
                        // painted grid, so a wrapped line parks the prompt on
                        // a row scrollback cannot reach.
                        y: Metrics::TITLE_BAR,
                        width: inspector_width,
                        height: (f32::from(viewport_size.height) - Metrics::TITLE_BAR).max(0.0),
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
        // Sampled here because RootView paints the strip inline; this also
        // keeps RootView drawing frames while a hint fade is moving.
        let held_hint = crate::held_hints::opacity(window, cx);
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
                        terminal.render_hosted_header_actions(sidebar_colors, held_hint, cx)
                    })
                });
            let strip = self.sidebar.update(cx, |sidebar, cx| {
                sidebar.strip_held_hint = held_hint;
                sidebar.render_horizontal_tabs(card_width, trailing, window, cx)
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
        if let Some(page) = self.todos_page.clone().filter(|_| self.todos_open) {
            body = body.child(page);
        } else if self.active_workspace.is_some() {
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

    /// The toast stack: connection recovery (persistent) nearest the edge,
    /// the transient toast beside it. Both are the same primitive.
    fn toast_stack(
        &self,
        recovery: Option<RecoveryNotice>,
        colors: SemanticColors,
        insets: (f32, f32),
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let transient = self.toast.current();
        if recovery.is_none() && transient.is_none() {
            return None;
        }
        let style = self.toast_style;
        let reduce_motion = cx.reduce_motion();
        let hover = |cx: &mut Context<Self>| {
            let view = cx.entity().downgrade();
            Box::new(move |hovered: bool, _: &mut Window, cx: &mut App| {
                let _ = view.update(cx, |this, cx| {
                    let timer = this.toast.set_hovered(hovered);
                    this.arm_toast_timer(timer, cx);
                });
            }) as crate::toast::HoverHandler
        };
        let action = |cx: &mut Context<Self>| {
            let view = cx.entity().downgrade();
            Box::new(
                move |command: ToastCommand, window: &mut Window, cx: &mut App| {
                    let _ = view.update(cx, |this, cx| this.run_toast_command(command, window, cx));
                },
            ) as crate::toast::ActionHandler
        };
        let mut items: Vec<AnyElement> = Vec::with_capacity(2);
        if let Some(toast) = transient {
            let view = cx.entity().downgrade();
            items.push(crate::toast::toast_element(
                toast,
                style,
                colors,
                "toast",
                self.toast.generation(),
                reduce_motion,
                ToastHandlers {
                    on_action: action(cx),
                    on_dismiss: Box::new(move |_, cx| {
                        let _ = view.update(cx, |this, cx| {
                            this.toast.dismiss();
                            cx.notify();
                        });
                    }),
                    on_hover: hover(cx),
                },
            ));
        }
        if let Some(notice) = recovery {
            let store = self.window_store.clone();
            items.push(crate::toast::toast_element(
                &notice.toast(),
                style,
                colors,
                "recovery",
                0,
                reduce_motion,
                ToastHandlers {
                    on_action: action(cx),
                    on_dismiss: Box::new(move |_, _| {
                        store
                            .write()
                            .expect("session store lock poisoned")
                            .dismiss_action_failure();
                    }),
                    on_hover: Box::new(|_, _, _| {}),
                },
            ));
        }
        let (left, right) = insets;
        let mut stack = div()
            .absolute()
            .left(px(left + 16.0))
            .right(px(right + 16.0))
            .flex()
            .flex_col()
            .gap(px(8.0));
        stack = match style.anchor() {
            // The persistent notice sits nearest the edge; a transient toast
            // stacks beyond it and leaves without moving it.
            crate::toast::ToastAnchor::BottomCenter => stack.bottom(px(18.0)).items_center(),
            crate::toast::ToastAnchor::BottomLeft => stack.bottom(px(16.0)).items_start(),
            crate::toast::ToastAnchor::TopRight => {
                items.reverse();
                stack.top(px(Metrics::TITLE_BAR + 8.0)).items_end()
            }
        };
        Some(
            deferred(stack.children(items))
                .with_priority(1)
                .into_any_element(),
        )
    }
}

impl RootView {
    /// Trackpad pinch: Safari's pinch between the page and the overview.
    fn handle_pinch(
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
            surfaces.update(cx, |surfaces, cx| surfaces.cancel_overview_pinch(cx));
            return;
        }
        let (taken, crossed) = surfaces.update(cx, |surfaces, cx| {
            let taken = surfaces.overview_pinch(event, window, cx);
            (taken, surfaces.take_zoom_feedback())
        });
        if crossed {
            // The pinch crossed the point where releasing commits.
            haptics::perform(Haptic::LevelChange, haptics::key("overview-pinch", ()));
        }
        if taken {
            cx.stop_propagation();
        }
    }
}

impl Render for RootView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let frame_started = crate::telemetry::frame_start();
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
        // Also while the sidebar and strip are both hidden, so a title that
        // changed out of sight does not crossfade when they come back, and a
        // session that came or went does not grow in or collapse out.
        self.sidebar.update(cx, |sidebar, cx| {
            sidebar.observe_titles(cx);
            sidebar.observe_rows(cx);
        });
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
            let notice = RecoveryNotice::resolve(
                store.daemon_state(),
                store.action_failure().filter(|failure| {
                    !launcher_open || failure.title != crate::store::PROMPT_DELIVERY_FAILURE_TITLE
                }),
            );
            let connecting = matches!(store.daemon_state(), crate::store::DaemonState::Connecting);
            drop(store);
            self.hold_connecting_notice(notice, connecting, cx)
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
            .capture_pinch(cx.listener(Self::handle_pinch))
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
            .on_action(cx.listener(|this, _: &NewNote, window, cx| {
                this.run_command(CommandId::NewNote, window, cx);
            }))
            .on_action(cx.listener(|this, _: &ShowTodos, window, cx| {
                this.run_command(CommandId::ShowTodos, window, cx);
            }))
            .on_action(cx.listener(|this, _: &SearchNotes, window, cx| {
                this.run_command(CommandId::SearchNotes, window, cx);
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
            .on_action(cx.listener(|this, _: &ShowWhatsNew, window, cx| {
                this.run_command(CommandId::ShowWhatsNew, window, cx);
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
            // ⌘-click is its own gesture; it must not leave hints behind.
            .capture_any_mouse_down(cx.listener(|this, _, window, cx| {
                let now = crate::held_hints::now(cx);
                let effect = this.held_hints.pointer_down(now);
                this.apply_held_hint_effect(effect, window, cx);
            }))
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
            // The handle is deferred so it wins hit tests against the terminal.
            // That also puts it above Settings, which covers this sidebar, so
            // the page would resize the panel hidden behind it.
            let settings_open = self
                .utility_surfaces
                .as_ref()
                .is_some_and(|surfaces| surfaces.read(cx).is_settings_open());
            if !settings_open {
                root = root.child(self.inspector_resize_handle(cx));
            }
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
        // Above Settings too: Settings › What's New opens it.
        if let Some(sheet) = &self.whats_new {
            root = root.child(div().absolute().inset_0().child(sheet.clone()));
        }
        if let Some(picker) = self.quote_target_picker(colors, sidebar_width, cx) {
            root = root.child(deferred(picker));
        }
        if let Some(panel) = self.notification_panel(window, cx) {
            root = root.child(deferred(panel));
        }
        if let Some(stack) = self.toast_stack(recovery_notice, colors, (seam, inspector_seam), cx) {
            root = root.child(stack);
        }
        if let Some(build) = &self.services.dev_build {
            root = root.child(dev_build_marker(build.marker_label(), colors, 10.0));
        }
        root.child(crate::telemetry::frame_probe(
            frame_started,
            self.frame_context(cx),
        ))
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

    /// What drawing one keystroke's echo costs in a whole workspace window:
    /// the pane is a cached view, so the sidebar and strip should replay.
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "headless Metal CPU measurement; run explicitly on macOS"]
    fn workspace_terminal_echo_redraw_cpu() {
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
        let terminal = cx
            .update_window(window.into(), |root, _, cx| {
                let root = root.downcast::<RootView>().unwrap();
                root.read(cx)
                    .workspace_workbench
                    .as_ref()
                    .unwrap()
                    .read(cx)
                    .focused_terminal()
                    .unwrap()
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
        let present = |cx: &mut HeadlessAppContext| {
            cx.update_window(window.into(), |_, window, _| window.present_if_needed())
                .unwrap();
        };
        let echo = |cx: &mut HeadlessAppContext, col: u16| {
            cx.update_window(window.into(), |_, window, cx| {
                terminal.update(cx, |terminal, cx| {
                    terminal.land_echo_for_test(col, window, cx)
                })
            })
            .unwrap();
            cx.run_until_parked();
        };
        for col in 0..20 {
            echo(&mut cx, col);
            present(&mut cx);
        }
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
        const ECHOES: u16 = 300;
        let before = cx.update(|cx| terminal.read(cx).render_count);
        let mut draw = Vec::new();
        let mut submit = Vec::new();
        let start_cpu = cpu_seconds();
        for index in 0..ECHOES {
            let started = Instant::now();
            echo(&mut cx, index % 150);
            let drawn = Instant::now();
            present(&mut cx);
            draw.push(drawn - started);
            submit.push(drawn.elapsed());
        }
        let cpu = cpu_seconds() - start_cpu;
        let renders = cx.update(|cx| terminal.read(cx).render_count) - before;
        draw.sort();
        submit.sort();
        let pick = |samples: &[Duration], q: f64| {
            samples[((samples.len() - 1) as f64 * q).round() as usize].as_secs_f64() * 1000.0
        };
        eprintln!(
            "workspace-echo: echoes={ECHOES} terminal_renders={renders} cpu_ms_per_echo={:.3} \
             apply+draw p50={:.3}ms p95={:.3}ms metal-submit p50={:.3}ms p95={:.3}ms",
            cpu * 1000.0 / f64::from(ECHOES),
            pick(&draw, 0.5),
            pick(&draw, 0.95),
            pick(&submit, 0.5),
            pick(&submit, 0.95),
        );
        cx.update_window(window.into(), |_, window, _| window.remove_window())
            .unwrap();
        cx.run_until_parked();
    }

    /// A screen of agent-like TUI content: coloured prose, a boxed composer,
    /// a status line with a spinner. `phase` scrolls the prose.
    #[cfg(target_os = "macos")]
    fn real_use_screen_rows(
        cols: u16,
        rows: u16,
        phase: usize,
    ) -> Vec<Vec<diri_proto::grid::GridCell>> {
        use diri_proto::grid::{GridCell, TermColor, TermStyle};
        const WORDS: [&str; 24] = [
            "Reading",
            "crates/diri-app/src/root.rs",
            "fn",
            "render(&mut",
            "self)",
            "->",
            "impl",
            "IntoElement",
            "the",
            "terminal",
            "pane",
            "is",
            "cached;",
            "updated",
            "3",
            "files",
            "(+42",
            "-7)",
            "cargo",
            "test",
            "--release",
            "passed",
            "Ok(())",
            "✓",
        ];
        let colors = [
            TermColor::Default,
            TermColor::Ansi(2),
            TermColor::Ansi(4),
            TermColor::Rgb(215, 119, 87),
            TermColor::Ansi(8),
            TermColor::Rgb(177, 185, 249),
        ];
        let cols = usize::from(cols);
        let mut screen = Vec::with_capacity(usize::from(rows));
        let text_row = |seed: usize| {
            let mut cells = Vec::with_capacity(cols);
            let mut word = seed;
            while cells.len() + 1 < cols.saturating_sub(4) {
                let text = WORDS[word % WORDS.len()];
                let color = colors[(word / 3) % colors.len()];
                let style = if word.is_multiple_of(11) {
                    TermStyle::BOLD
                } else {
                    TermStyle::empty()
                };
                for character in text.chars() {
                    cells.push(GridCell::new(
                        character as u32,
                        color,
                        TermColor::DefaultInverted,
                        style,
                    ));
                }
                cells.push(GridCell::BLANK);
                word = word.wrapping_mul(31).wrapping_add(7) % 997;
                if word.is_multiple_of(9) {
                    break;
                }
            }
            cells.truncate(cols);
            cells
        };
        let body = usize::from(rows).saturating_sub(6);
        for row in 0..body {
            screen.push(text_row(row + phase));
        }
        let border = |left: char, fill: char, right: char| {
            let mut cells = vec![GridCell::new(
                left as u32,
                TermColor::Ansi(8),
                TermColor::DefaultInverted,
                TermStyle::empty(),
            )];
            cells.extend((2..cols).map(|_| {
                GridCell::new(
                    fill as u32,
                    TermColor::Ansi(8),
                    TermColor::DefaultInverted,
                    TermStyle::empty(),
                )
            }));
            cells.push(GridCell::new(
                right as u32,
                TermColor::Ansi(8),
                TermColor::DefaultInverted,
                TermStyle::empty(),
            ));
            cells
        };
        screen.push(vec![GridCell::BLANK; cols]);
        screen.push(border('╭', '─', '╮'));
        let mut prompt = border('│', ' ', '│');
        for (index, character) in "> ".chars().enumerate() {
            prompt[index + 2].scalar = character as u32;
        }
        screen.push(prompt);
        screen.push(border('╰', '─', '╯'));
        screen.push(real_use_status_row(cols, phase));
        screen.push(vec![GridCell::BLANK; cols]);
        screen.truncate(usize::from(rows));
        screen
    }

    #[cfg(target_os = "macos")]
    fn real_use_status_row(cols: usize, phase: usize) -> Vec<diri_proto::grid::GridCell> {
        use diri_proto::grid::{GridCell, TermColor, TermStyle};
        const SPINNER: [char; 6] = ['·', '✢', '✳', '✶', '✻', '✽'];
        let text = format!(
            "{} Thinking… ({}s · ↑ {} tokens · esc to interrupt)",
            SPINNER[phase % SPINNER.len()],
            phase / 10,
            1_200 + phase * 7
        );
        let mut cells: Vec<GridCell> = text
            .chars()
            .map(|character| {
                GridCell::new(
                    character as u32,
                    TermColor::Rgb(215, 119, 87),
                    TermColor::DefaultInverted,
                    TermStyle::empty(),
                )
            })
            .collect();
        cells.resize(cols, GridCell::BLANK);
        cells.truncate(cols);
        cells
    }

    /// Real sessions carry their pull requests: a description, CI checks and
    /// the review discussion. Every third session gets one to three. The
    /// sizes are illustrative, not measured from a real fleet: here row
    /// comparison is under 1% of `Window::draw`, where a sample of the
    /// installed app put it at 8%.
    #[cfg(target_os = "macos")]
    fn real_use_pull_requests(sessions: &mut [diri_proto::SessionRecord]) {
        for (index, session) in sessions.iter_mut().enumerate() {
            if !index.is_multiple_of(3) {
                continue;
            }
            let prs = (0..1 + index % 3)
                .map(|pr| {
                    serde_json::from_value::<diri_proto::model::PullRequestStatus>(serde_json::json!({
                        "url": format!("https://github.com/example/repo/pull/{}", 500 + index * 2 + pr),
                        "number": 500 + index * 2 + pr,
                        "title": format!("Make the thing faster, part {pr}"),
                        "author": "someone",
                        "body": "Summary of the change. ".repeat(700),
                        "baseRefName": "main",
                        "headRefName": format!("perf/branch-{index}-{pr}"),
                        "state": "OPEN",
                        "isDraft": false,
                        "reviewDecision": "REVIEW_REQUIRED",
                        "additions": 420,
                        "deletions": 73,
                        "changedFiles": 12,
                        "commentCount": 18,
                        "reviewCount": 3,
                        "checksPassed": 9,
                        "checksFailed": 0,
                        "checksPending": 1,
                        "checks": (0..10).map(|check| serde_json::json!({
                            "name": format!("CI / job {check}"),
                            "result": "success",
                            "url": format!("https://github.com/example/repo/actions/runs/{check}"),
                        })).collect::<Vec<_>>(),
                        "discussion": (0..40).map(|comment| serde_json::json!({
                            "kind": "comment",
                            "author": "reviewer",
                            "body": format!("Comment {comment}: ").repeat(50),
                        })).collect::<Vec<_>>(),
                        "fetchedAt": 1_750_000_000_000.0,
                    }))
                    .unwrap()
                })
                .collect();
            session.pull_requests = Some(prs);
        }
    }

    /// Frame cost under heavy real use, the fixture behind the 2026-09-30
    /// telemetry investigation: 51 sessions (four working) in the sidebar or
    /// the horizontal strip, a workspace tab split into three busy agent
    /// terminals, all driven at their real cadences on a 120 Hz tick: one
    /// pane streaming (whole screen scrolls, 30 Hz), two spinners (10 and 8
    /// Hz), the sidebar's 125 ms activity tick and a store publication every
    /// 200 ms. Every tick with something due draws one frame, the way a
    /// display link coalesces them. Prints the frame distribution and GPUI's
    /// per-phase breakdown.
    ///
    /// `DIRI_BENCH_A11Y=1` attaches pretend assistive technology (what a Mac
    /// running Rectangle, Raycast, Wispr Flow and the like does to every
    /// app). `DIRI_BENCH_TABS=horizontal` swaps the sidebar for the strip.
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "headless Metal frame-distribution bench; run explicitly on macOS"]
    fn real_use_frame_distribution() {
        use diri_proto::workspace::*;
        use gpui::HeadlessAppContext;
        let platform = gpui_platform::current_platform(true);
        let mut cx = HeadlessAppContext::with_platform(
            platform.text_system(),
            Arc::new(diri_ui::IconAssets),
            gpui_platform::current_headless_renderer,
        );
        cx.update(|cx| crate::fonts::init(cx));
        diri_ui::set_mark_rasterizer(bench_stand_in_raster);
        let a11y = std::env::var("DIRI_BENCH_A11Y").is_ok_and(|value| value == "1");
        let horizontal = std::env::var("DIRI_BENCH_TABS").is_ok_and(|value| value == "horizontal");
        let ticks: usize = std::env::var("DIRI_BENCH_TICKS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(2_400);
        let services = test_services();
        let workspace = WorkspaceId::new("real-use");
        {
            let mut store = services.store.store.write().unwrap();
            let mut fleet = SidebarPreviewFixture::bench_fleet(51, 4).list;
            if std::env::var("DIRI_BENCH_PRS").map_or(true, |value| value != "0") {
                real_use_pull_requests(&mut fleet.sessions);
            }
            store.hydrate(fleet);
            store
                .update_preferences(|prefs| {
                    if horizontal {
                        prefs.tab_orientation = crate::store::TabOrientation::Horizontal;
                        prefs.horizontal_tabs_visible = true;
                        prefs.sidebar_visible = false;
                    } else {
                        prefs.sidebar_visible = true;
                    }
                })
                .unwrap();
            let pane = |name: &str, session: &str| LayoutNode::Pane {
                id: PaneId::new(name),
                session_id: SessionId::new(session),
            };
            store.seed_workspace_snapshot_for_test(WorkspaceSnapshot {
                revision: 1,
                workspaces: vec![WorkspaceRecord {
                    project_id: None,
                    id: workspace.clone(),
                    name: "Real use".into(),
                    selected_tab: Some(TabId::new("real-tab")),
                    tabs: vec![WorkspaceTab {
                        id: TabId::new("real-tab"),
                        title: None,
                        focused_pane: PaneId::new("a-stream"),
                        zoomed_pane: None,
                        layout: LayoutNode::Split {
                            id: SplitId::new("outer"),
                            axis: LayoutAxis::Horizontal,
                            fraction: 0.55,
                            first: Box::new(pane("a-stream", "bench-0")),
                            second: Box::new(LayoutNode::Split {
                                id: SplitId::new("inner"),
                                axis: LayoutAxis::Vertical,
                                fraction: 0.5,
                                first: Box::new(pane("b-spinner", "bench-1")),
                                second: Box::new(pane("c-spinner", "bench-2")),
                            }),
                        },
                    }],
                }],
                ..Default::default()
            });
        }
        let window: gpui::AnyWindowHandle = cx
            .open_window(size(px(1728.0), px(1080.0)), |window, cx| {
                cx.new(|cx| {
                    let root = RootView::new(services, false, PreviewScenario::Empty, window, cx);
                    root.sidebar.update(cx, |sidebar, cx| {
                        sidebar.activate_workspace(Some(workspace), cx)
                    });
                    root
                })
            })
            .unwrap()
            .into();
        cx.run_until_parked();
        let root = cx
            .update_window(window, |root, _, _| root.downcast::<RootView>().unwrap())
            .unwrap();
        let (sidebar, terminals) = cx.update(|cx| {
            let root = root.read(cx);
            (
                root.sidebar.clone(),
                root.workspace_workbench
                    .as_ref()
                    .unwrap()
                    .read(cx)
                    .terminals_for_test(),
            )
        });
        assert_eq!(terminals.len(), 3, "three mounted panes");
        // Seed every pane with a full screen at its own size.
        for terminal in &terminals {
            cx.update(|cx| {
                terminal.update(cx, |terminal, cx| {
                    let (cols, rows) = terminal.selected_grid_size_for_test().unwrap_or((0, 0));
                    let (cols, rows) = if cols < 20 || rows < 10 {
                        // Not attached, so never sized: fill the pane at
                        // Menlo 13's cell size.
                        terminal
                            .geometry_for_test()
                            .0
                            .map_or((120, 40), |viewport| {
                                (
                                    (viewport.width / 7.83) as u16,
                                    ((viewport.height - 44.0) / 16.0) as u16,
                                )
                            })
                    } else {
                        (cols, rows)
                    };
                    let mut grid = diri_term::buffer::GridBuffer::new(cols, rows);
                    for (row, cells) in real_use_screen_rows(cols, rows, 0).into_iter().enumerate()
                    {
                        let start = row * usize::from(cols);
                        for (col, cell) in cells.into_iter().enumerate() {
                            grid.cells[start + col] = cell;
                        }
                    }
                    terminal.seed_preview_grid_for_test(grid, cx);
                    cx.notify();
                })
            });
        }
        cx.run_until_parked();
        if a11y {
            cx.update_window(window, |_, window, _| {
                window.set_accessibility_active_for_test(true)
            })
            .unwrap();
            cx.run_until_parked();
        }
        let sizes: Vec<(u16, u16)> = cx.update(|cx| {
            terminals
                .iter()
                .map(|terminal| {
                    terminal
                        .read(cx)
                        .selected_grid_size_for_test()
                        .unwrap_or((120, 40))
                })
                .collect()
        });
        // (period in 120 Hz ticks, what happens)
        let step = |cx: &mut HeadlessAppContext, tick: usize| -> bool {
            let stream = tick.is_multiple_of(4);
            let spinner_b = tick.is_multiple_of(12);
            let spinner_c = tick.is_multiple_of(15);
            let activity = tick.is_multiple_of(15);
            let publication = tick.is_multiple_of(24);
            if !(stream || spinner_b || spinner_c || activity || publication) {
                return false;
            }
            cx.update_window(window, |_, window, cx| {
                if stream {
                    let (cols, rows) = sizes[0];
                    let screen = real_use_screen_rows(cols, rows, tick / 4);
                    terminals[0].update(cx, |terminal, cx| {
                        terminal.land_rows_for_test(
                            screen
                                .into_iter()
                                .enumerate()
                                .map(|(row, cells)| (row as u16, cells))
                                .collect(),
                            (4, rows.saturating_sub(4)),
                            window,
                            cx,
                        )
                    });
                }
                for (index, due) in [(1, spinner_b), (2, spinner_c)] {
                    if due {
                        let (cols, rows) = sizes[index];
                        let status = real_use_status_row(usize::from(cols), tick / 12);
                        terminals[index].update(cx, |terminal, cx| {
                            terminal.land_rows_for_test(
                                vec![(rows.saturating_sub(2), status)],
                                (4, rows.saturating_sub(4)),
                                window,
                                cx,
                            )
                        });
                    }
                }
                if activity {
                    sidebar.update(cx, |sidebar, cx| {
                        sidebar.advance_activity_frame_for_test(cx)
                    });
                }
                if publication {
                    sidebar.update(cx, |sidebar, cx| sidebar.store_changed(cx));
                    root.update(cx, |_, cx| cx.notify());
                }
            })
            .unwrap();
            cx.run_until_parked();
            true
        };
        for tick in 0..240 {
            step(&mut cx, tick);
        }
        let paints_before = diri_term::element::PaintTotals::now();
        let mut frames: Vec<(Duration, gpui::FrameStats)> = Vec::new();
        let start_cpu = bench_cpu_seconds();
        for tick in 240..240 + ticks {
            let started = Instant::now();
            if step(&mut cx, tick) {
                let elapsed = started.elapsed();
                let stats = cx
                    .update_window(window, |_, window, _| window.last_frame_stats())
                    .unwrap();
                frames.push((elapsed, stats));
            }
        }
        let cpu = bench_cpu_seconds() - start_cpu;
        let paints = diri_term::element::PaintTotals::now().since(paints_before);
        let n = frames.len();
        let mut walls: Vec<Duration> = frames.iter().map(|(wall, _)| *wall).collect();
        let mut draws: Vec<Duration> = frames.iter().map(|(_, stats)| stats.total()).collect();
        walls.sort();
        draws.sort();
        let ms = |duration: Duration| duration.as_secs_f64() * 1000.0;
        let pick = |samples: &[Duration], q: f64| {
            ms(samples[((samples.len() - 1) as f64 * q).round() as usize])
        };
        let mean = |f: &dyn Fn(&gpui::FrameStats) -> f64| {
            frames.iter().map(|(_, stats)| f(stats)).sum::<f64>() / n as f64
        };
        eprintln!(
            "real-use a11y={a11y} tabs={} frames={n} step_ms p50={:.3} p90={:.3} p99={:.3} max={:.3} \
             draw_ms p50={:.3} p90={:.3} p99={:.3} cpu_ms_per_frame={:.3} \
             layout={:.3} prepaint={:.3} paint={:.3} a11y={:.3} finish={:.3} views_rendered={:.1} views_reused={:.1} \
             terminal_paints_per_frame={:.2} terminal_ms_per_frame={:.3} shape_misses={}",
            if horizontal { "horizontal" } else { "sidebar" },
            pick(&walls, 0.5),
            pick(&walls, 0.9),
            pick(&walls, 0.99),
            pick(&walls, 1.0),
            pick(&draws, 0.5),
            pick(&draws, 0.9),
            pick(&draws, 0.99),
            cpu * 1000.0 / n as f64,
            mean(&|stats| ms(stats.layout)),
            mean(&|stats| ms(stats.prepaint)),
            mean(&|stats| ms(stats.paint)),
            mean(&|stats| ms(stats.a11y)),
            mean(&|stats| ms(stats.finish)),
            mean(&|stats| f64::from(stats.views_rendered)),
            mean(&|stats| f64::from(stats.views_reused)),
            paints.paints as f64 / n as f64,
            paints.micros as f64 / 1000.0 / n as f64,
            paints.shape_misses,
        );
        if let Ok(output) = std::env::var("DIRI_BENCH_SCREENSHOT") {
            cx.capture_screenshot(window).unwrap().save(output).unwrap();
        }
        drop(root);
        drop(sidebar);
        drop(terminals);
        cx.update_window(window, |_, window, _| window.remove_window())
            .unwrap();
        cx.run_until_parked();
    }

    /// Rows reused across activity ticks, no-op store publications and
    /// root-only frames paint exactly what a full re-render paints: after the
    /// ticks, a `window.refresh()` that rebuilds everything at the same mark
    /// frame must match pixel for pixel. Eleven ticks leave the marks mid-cycle,
    /// so a mark that failed to advance would differ too.
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "headless Metal pixel comparison; run explicitly on macOS"]
    fn reused_sidebar_rows_paint_like_a_full_render() {
        use gpui::HeadlessAppContext;
        let platform = gpui_platform::current_platform(true);
        let mut cx = HeadlessAppContext::with_platform(
            platform.text_system(),
            Arc::new(diri_ui::IconAssets),
            gpui_platform::current_headless_renderer,
        );
        cx.update(|cx| crate::fonts::init(cx));
        let services = test_services();
        services
            .store
            .store
            .write()
            .unwrap()
            .hydrate(SidebarPreviewFixture::bench_fleet(51, 4).list);
        services
            .store
            .store
            .write()
            .unwrap()
            .update_preferences(|prefs| prefs.sidebar_visible = true)
            .unwrap();
        let window = cx
            .open_window(size(px(1600.0), px(1000.0)), |window, cx| {
                cx.new(|cx| RootView::new(services, false, PreviewScenario::Empty, window, cx))
            })
            .unwrap();
        cx.run_until_parked();
        let root = cx
            .update_window(window.into(), |root, _, _| {
                root.downcast::<RootView>().unwrap()
            })
            .unwrap();
        let sidebar = cx.update(|cx| root.read(cx).sidebar.clone());
        cx.capture_screenshot(window.into()).unwrap();
        for tick in 0..11 {
            cx.update(|cx| {
                sidebar.update(cx, |sidebar, cx| {
                    sidebar.advance_activity_frame_for_test(cx)
                })
            });
            cx.run_until_parked();
            cx.update_window(window.into(), |_, window, cx| window.draw(cx).clear())
                .unwrap();
            let step = if tick % 2 == 0 {
                |sidebar: &mut crate::sidebar::Sidebar,
                 cx: &mut Context<crate::sidebar::Sidebar>| {
                    sidebar.store_changed(cx)
                }
            } else {
                |_: &mut crate::sidebar::Sidebar, cx: &mut Context<crate::sidebar::Sidebar>| {
                    cx.notify()
                }
            };
            cx.update(|cx| sidebar.update(cx, step));
            cx.run_until_parked();
            cx.update_window(window.into(), |_, window, cx| window.draw(cx).clear())
                .unwrap();
            cx.update(|cx| root.update(cx, |_, cx| cx.notify()));
            cx.run_until_parked();
        }
        let reused = cx.capture_screenshot(window.into()).unwrap();
        cx.update_window(window.into(), |_, window, _| window.refresh())
            .unwrap();
        cx.run_until_parked();
        let fresh = cx.capture_screenshot(window.into()).unwrap();
        if let Ok(dir) = std::env::var("DIRI_VISUAL_OUTPUT_DIR") {
            let dir = std::path::PathBuf::from(dir);
            reused.save(dir.join("sidebar-reused.png")).unwrap();
            fresh.save(dir.join("sidebar-fresh.png")).unwrap();
        }
        assert_eq!(reused.dimensions(), fresh.dimensions());
        let differing = reused
            .pixels()
            .zip(fresh.pixels())
            .filter(|(left, right)| left != right)
            .count();
        assert_eq!(differing, 0, "reused rows painted differently");
        drop(root);
        drop(sidebar);
        cx.update_window(window.into(), |_, window, _| window.remove_window())
            .unwrap();
        cx.run_until_parked();
    }

    /// Process CPU time (user + system) so far, for render-cost benches.
    #[cfg(target_os = "macos")]
    fn bench_cpu_seconds() -> f64 {
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

    // Production draws solid brand marks as cached CoreGraphics rasters
    // (an `img` per row), not tessellated paths. AppKit drawing needs the
    // main thread, which a test does not own, so stand in with a cached
    // blank raster of the same size to keep the element shape identical.
    #[cfg(target_os = "macos")]
    fn bench_stand_in_raster(
        _: diri_ui::BrandMarkKind,
        size: f32,
        _: f32,
        _: gpui::Rgba,
    ) -> Option<AnyElement> {
        use std::sync::{LazyLock, Mutex};
        static CACHE: LazyLock<Mutex<std::collections::HashMap<u32, Arc<gpui::RenderImage>>>> =
            LazyLock::new(Default::default);
        let image = CACHE
            .lock()
            .unwrap()
            .entry(size.to_bits())
            .or_insert_with(|| {
                let pixels = (size * 2.0).ceil() as u32;
                Arc::new(gpui::RenderImage::new(smallvec::smallvec![
                    image::Frame::new(image::RgbaImage::new(pixels, pixels))
                ]))
            })
            .clone();
        Some(
            gpui::img(image)
                .flex_none()
                .size(px(size))
                .into_any_element(),
        )
    }

    /// Render cost of the sidebar under a busy fleet: 51 sessions over five
    /// projects, four of them working, mounted in the real RootView and
    /// painted by headless Metal. Measures one activity-mark tick and one
    /// store publication that changes nothing the sidebar shows.
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "headless Metal render-cost bench; run explicitly on macOS"]
    fn sidebar_fleet_render_cost() {
        use crate::sidebar::render_probe;
        use gpui::HeadlessAppContext;
        let platform = gpui_platform::current_platform(true);
        let mut cx = HeadlessAppContext::with_platform(
            platform.text_system(),
            Arc::new(diri_ui::IconAssets),
            gpui_platform::current_headless_renderer,
        );
        cx.update(|cx| crate::fonts::init(cx));
        diri_ui::set_mark_rasterizer(bench_stand_in_raster);
        let services = test_services();
        services.store.store.write().unwrap().hydrate(
            SidebarPreviewFixture::bench_fleet(
                std::env::var("DIRI_BENCH_SESSIONS")
                    .ok()
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(51),
                4,
            )
            .list,
        );
        services
            .store
            .store
            .write()
            .unwrap()
            .update_preferences(|prefs| prefs.sidebar_visible = true)
            .unwrap();
        let window = cx
            .open_window(size(px(1600.0), px(1000.0)), |window, cx| {
                cx.new(|cx| RootView::new(services, false, PreviewScenario::Empty, window, cx))
            })
            .unwrap();
        cx.run_until_parked();
        let sidebar = cx
            .update_window(window.into(), |root, _, cx| {
                root.downcast::<RootView>()
                    .unwrap()
                    .read(cx)
                    .sidebar
                    .clone()
            })
            .unwrap();
        cx.capture_screenshot(window.into()).unwrap();
        let iterations: usize = std::env::var("DIRI_BENCH_ITERATIONS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(300);
        let root = cx
            .update_window(window.into(), |root, _, _| {
                root.downcast::<RootView>().unwrap()
            })
            .unwrap();
        let cases: [&str; 3] = ["activity-tick", "noop-store-change", "root-only-frame"];
        for name in cases {
            let step = |cx: &mut HeadlessAppContext| {
                let start = Instant::now();
                cx.update(|cx| match name {
                    "activity-tick" => sidebar.update(cx, |sidebar, cx| {
                        sidebar.advance_activity_frame_for_test(cx)
                    }),
                    "noop-store-change" => {
                        sidebar.update(cx, |sidebar, cx| sidebar.store_changed(cx))
                    }
                    _ => root.update(cx, |_, cx| cx.notify()),
                });
                cx.run_until_parked();
                cx.update_window(window.into(), |_, window, cx| window.draw(cx).clear())
                    .unwrap();
                start.elapsed()
            };
            for _ in 0..20 {
                step(&mut cx);
            }
            render_probe::take();
            let mut draws = Vec::with_capacity(iterations);
            let start_cpu = bench_cpu_seconds();
            for _ in 0..iterations {
                draws.push(step(&mut cx));
            }
            let cpu = bench_cpu_seconds() - start_cpu;
            let (rows, renders, render_time) = render_probe::take();
            draws.sort();
            let ms = |duration: Duration| duration.as_secs_f64() * 1000.0;
            eprintln!(
                "sidebar-fleet {name}: steps={iterations} sidebar_renders={renders} \
                 rows_built_per_step={:.1} sidebar_render_fn_ms={:.3} \
                 step_ms_median={:.3} step_ms_p90={:.3} cpu_ms_per_step={:.3}",
                rows as f64 / iterations as f64,
                ms(render_time) / iterations as f64,
                ms(draws[iterations / 2]),
                ms(draws[iterations * 9 / 10]),
                cpu * 1000.0 / iterations as f64,
            );
        }
        drop(root);
        drop(sidebar);
        if let Ok(output) = std::env::var("DIRI_SIDEBAR_BENCH_SCREENSHOT") {
            cx.capture_screenshot(window.into())
                .unwrap()
                .save(output)
                .unwrap();
        }
        cx.update_window(window.into(), |_, window, _| window.remove_window())
            .unwrap();
        cx.run_until_parked();
    }

    /// Strip tabs reused across terminal frames, activity ticks, no-op
    /// store publications and a selection pill glide paint exactly what a
    /// full re-render paints: after
    /// the steps, a `window.refresh()` that rebuilds everything at the same
    /// mark frame must match pixel for pixel. Eleven ticks leave the marks
    /// mid-cycle, so a mark that failed to advance would differ too.
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "headless Metal pixel comparison; run explicitly on macOS"]
    fn reused_strip_tabs_paint_like_a_full_render() {
        use gpui::HeadlessAppContext;
        let platform = gpui_platform::current_platform(true);
        let mut cx = HeadlessAppContext::with_platform(
            platform.text_system(),
            Arc::new(diri_ui::IconAssets),
            gpui_platform::current_headless_renderer,
        );
        cx.update(|cx| crate::fonts::init(cx));
        let window = strip_bench_window(&mut cx, 51);
        let root = cx
            .update_window(window, |root, _, _| root.downcast::<RootView>().unwrap())
            .unwrap();
        let (sidebar, terminal) = cx.update(|cx| {
            let root = root.read(cx);
            (root.sidebar.clone(), root.terminal.clone())
        });
        // Let the strip's entry slide finish.
        std::thread::sleep(Duration::from_millis(600));
        cx.run_until_parked();
        cx.capture_screenshot(window).unwrap();
        let draw = |cx: &mut HeadlessAppContext| {
            cx.run_until_parked();
            cx.update_window(window, |_, window, cx| window.draw(cx).clear())
                .unwrap();
        };
        for tick in 0..11 {
            if tick == 3 {
                // Select the next tab and let the pill glide there over
                // reused tabs, drawing frames the way a display link would.
                cx.update(|cx| {
                    root.read(cx)
                        .window_store
                        .write()
                        .unwrap()
                        .select(SessionId::new("bench-5"));
                    sidebar.update(cx, |sidebar, cx| sidebar.store_changed(cx));
                });
                draw(&mut cx);
                let glide = Instant::now();
                while glide.elapsed() < diri_ui::Motion::ROW_SELECT_TIME + Duration::from_millis(60)
                {
                    std::thread::sleep(Duration::from_millis(16));
                    if let Some(terminal) = &terminal {
                        cx.update(|cx| terminal.update(cx, |_, cx| cx.notify()));
                    }
                    draw(&mut cx);
                }
            }
            cx.update(|cx| {
                sidebar.update(cx, |sidebar, cx| {
                    sidebar.advance_activity_frame_for_test(cx)
                })
            });
            draw(&mut cx);
            if tick % 2 == 0 {
                cx.update(|cx| sidebar.update(cx, |sidebar, cx| sidebar.store_changed(cx)));
            } else {
                cx.update(|cx| root.update(cx, |_, cx| cx.notify()));
            }
            draw(&mut cx);
            if let Some(terminal) = &terminal {
                cx.update(|cx| terminal.update(cx, |_, cx| cx.notify()));
                draw(&mut cx);
            }
        }
        let reused = cx.capture_screenshot(window).unwrap();
        cx.update_window(window, |_, window, _| window.refresh())
            .unwrap();
        cx.run_until_parked();
        let fresh = cx.capture_screenshot(window).unwrap();
        if let Ok(dir) = std::env::var("DIRI_VISUAL_OUTPUT_DIR") {
            let dir = std::path::PathBuf::from(dir);
            reused.save(dir.join("strip-reused.png")).unwrap();
            fresh.save(dir.join("strip-fresh.png")).unwrap();
        }
        assert_eq!(reused.dimensions(), fresh.dimensions());
        let differing = reused
            .pixels()
            .zip(fresh.pixels())
            .filter(|(left, right)| left != right)
            .count();
        assert_eq!(differing, 0, "reused strip tabs painted differently");
        drop(root);
        drop(sidebar);
        drop(terminal);
        cx.update_window(window, |_, window, _| window.remove_window())
            .unwrap();
        cx.run_until_parked();
    }

    /// Horizontal tabs for the fleet `sidebar_fleet_render_cost` uses: the
    /// sidebar hidden, sessions as tabs across the top, the selected project
    /// holding a working session.
    #[cfg(target_os = "macos")]
    fn strip_bench_window(
        cx: &mut gpui::HeadlessAppContext,
        sessions: usize,
    ) -> gpui::AnyWindowHandle {
        let services = test_services();
        {
            let mut store = services.store.store.write().unwrap();
            store.hydrate(SidebarPreviewFixture::bench_fleet(sessions, 4).list);
            store
                .update_preferences(|prefs| {
                    prefs.tab_orientation = crate::store::TabOrientation::Horizontal;
                    prefs.horizontal_tabs_visible = true;
                    prefs.sidebar_visible = false;
                })
                .unwrap();
            // Project 0 of five: bench-0 (working), bench-5, bench-10, ...
            store.select(SessionId::new("bench-0"));
        }
        let window = cx
            .open_window(size(px(1600.0), px(1000.0)), |window, cx| {
                cx.new(|cx| RootView::new(services, false, PreviewScenario::Empty, window, cx))
            })
            .unwrap();
        cx.run_until_parked();
        window.into()
    }

    /// Render cost of the horizontal tab strip under a busy fleet. RootView
    /// paints the strip inline, so every window frame renders it: a terminal
    /// output frame, a working mark's tick, a store publication that changes
    /// nothing, and a bare root frame. Counts tabs built per frame and the
    /// time spent building the strip (its tabs included).
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "headless Metal render-cost bench; run explicitly on macOS"]
    fn strip_fleet_render_cost() {
        use crate::sidebar::render_probe;
        use gpui::HeadlessAppContext;
        let platform = gpui_platform::current_platform(true);
        let mut cx = HeadlessAppContext::with_platform(
            platform.text_system(),
            Arc::new(diri_ui::IconAssets),
            gpui_platform::current_headless_renderer,
        );
        cx.update(|cx| crate::fonts::init(cx));
        diri_ui::set_mark_rasterizer(bench_stand_in_raster);
        let sessions = std::env::var("DIRI_BENCH_SESSIONS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(51);
        let window = strip_bench_window(&mut cx, sessions);
        let root = cx
            .update_window(window, |root, _, _| root.downcast::<RootView>().unwrap())
            .unwrap();
        let (sidebar, terminal) = cx.update(|cx| {
            let root = root.read(cx);
            (root.sidebar.clone(), root.terminal.clone())
        });
        // Let the strip's entry slide finish before measuring.
        std::thread::sleep(Duration::from_millis(600));
        cx.run_until_parked();
        cx.capture_screenshot(window).unwrap();
        let tabs = cx.update(|cx| sidebar.read(cx).strip_tab_count_for_test());
        let iterations: usize = std::env::var("DIRI_BENCH_ITERATIONS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(300);
        let cases: [&str; 4] = [
            "terminal-output-frame",
            "activity-tick",
            "noop-store-change",
            "root-only-frame",
        ];
        for name in cases {
            let step = |cx: &mut HeadlessAppContext| {
                let start = Instant::now();
                cx.update(|cx| match name {
                    "terminal-output-frame" => match &terminal {
                        Some(terminal) => terminal.update(cx, |_, cx| cx.notify()),
                        None => root.update(cx, |_, cx| cx.notify()),
                    },
                    "activity-tick" => sidebar.update(cx, |sidebar, cx| {
                        sidebar.advance_activity_frame_for_test(cx)
                    }),
                    "noop-store-change" => {
                        sidebar.update(cx, |sidebar, cx| sidebar.store_changed(cx))
                    }
                    _ => root.update(cx, |_, cx| cx.notify()),
                });
                cx.run_until_parked();
                cx.update_window(window, |_, window, cx| window.draw(cx).clear())
                    .unwrap();
                start.elapsed()
            };
            for _ in 0..20 {
                step(&mut cx);
            }
            render_probe::take_strip();
            let mut draws = Vec::with_capacity(iterations);
            let start_cpu = bench_cpu_seconds();
            for _ in 0..iterations {
                draws.push(step(&mut cx));
            }
            let cpu = bench_cpu_seconds() - start_cpu;
            let (built, renders, strip_time) = render_probe::take_strip();
            draws.sort();
            let ms = |duration: Duration| duration.as_secs_f64() * 1000.0;
            eprintln!(
                "strip-fleet {name}: tabs={tabs} terminal={} steps={iterations} strip_renders={renders} \
                 tabs_built_per_step={:.2} strip_render_ms={:.3} \
                 step_ms_median={:.3} step_ms_p90={:.3} cpu_ms_per_step={:.3}",
                terminal.is_some(),
                built as f64 / iterations as f64,
                ms(strip_time) / iterations as f64,
                ms(draws[iterations / 2]),
                ms(draws[iterations * 9 / 10]),
                cpu * 1000.0 / iterations as f64,
            );
        }
        drop(root);
        drop(sidebar);
        drop(terminal);
        if let Ok(output) = std::env::var("DIRI_STRIP_BENCH_SCREENSHOT") {
            cx.capture_screenshot(window).unwrap().save(output).unwrap();
        }
        cx.update_window(window, |_, window, _| window.remove_window())
            .unwrap();
        cx.run_until_parked();
    }

    #[gpui::test]
    fn shortcut_spawn_hands_focus_to_the_active_terminal(cx: &mut gpui::TestAppContext) {
        let services = test_services();
        let (root, cx) = cx.add_window_view(move |window, cx| {
            RootView::new(services, false, PreviewScenario::Empty, window, cx)
        });
        cx.simulate_resize(size(px(1_000.0), px(700.0)));
        cx.run_until_parked();

        root.update_in(cx, |root, window, cx| {
            root.sidebar
                .update(cx, |sidebar, cx| sidebar.focus(window, cx));
            let terminal = root.active_terminal(cx).expect("active terminal");
            assert!(!terminal.read(cx).is_focused(window));

            root.run_command(CommandId::NewTerminal, window, cx);
            assert!(
                terminal.read(cx).is_focused(window),
                "a new session must own the keyboard without waiting for the spawn reply"
            );
        });
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
                let toast = root.toast.current().expect("standard toast");
                assert_eq!(
                    toast.message,
                    format!("Input rejected in workspace={workspace}")
                );
                assert_eq!(toast.detail, None, "no category title like “Terminal”");
            });
        }
        cx.executor().advance_clock(Duration::from_secs(4));
        cx.run_until_parked();
        root.read_with(cx, |root, _| assert!(root.toast.current().is_none()));
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
        // Measure the resting position, not the entrance rise.
        cx.update(|_, cx| cx.set_reduce_motion(true));
        for width in [640.0, 1000.0] {
            cx.simulate_resize(size(px(width), px(700.0)));
            cx.run_until_parked();
            let card = cx.debug_bounds("recovery").unwrap();
            assert!(card.top() > px(Metrics::TITLE_BAR));
            assert!(card.right() <= px(width - 16.0));
            assert!(card.bottom() <= px(684.0));
            let button = cx.debug_bounds("recovery-action-0").unwrap();
            assert!(card.contains(&button.center()));
            cx.simulate_click(button.center(), Modifiers::default());
            assert_eq!(
                cx.read_from_clipboard().unwrap().text().as_deref(),
                Some("diagnostic detail")
            );
        }
        let dismiss = cx.debug_bounds("recovery-dismiss").unwrap().center();
        cx.simulate_click(dismiss, Modifiers::default());
        assert!(store.store.read().unwrap().action_failure().is_none());
        root.update(cx, |_, cx| cx.notify());
        cx.run_until_parked();
        assert!(cx.debug_bounds("recovery-action-0").is_none());
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

    /// Every toast direction × theme × state, in the real window, for the
    /// redesign comparison sheets (docs/screenshots/toast-redesign).
    /// `DIRI_TOAST_STYLES=capsule,card,ink` narrows the set.
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "writes toast previews to DIRI_TOAST_SCREENSHOTS"]
    fn render_toast_redesign_screenshots() {
        use gpui::HeadlessAppContext;
        let dir =
            std::path::PathBuf::from(std::env::var("DIRI_TOAST_SCREENSHOTS").expect("output dir"));
        std::fs::create_dir_all(&dir).unwrap();
        let styles =
            std::env::var("DIRI_TOAST_STYLES").unwrap_or_else(|_| "capsule,card,ink".into());
        type State = (&'static str, fn() -> Toast);
        let states: [State; 3] = [
            ("privacy", || {
                Toast::info("diri sends crash and error reports")
                    .detail("Terminal contents never leave your Mac.")
                    .action("Settings", ToastCommand::OpenPrivacySettings)
            }),
            ("error", || Toast::error("Workspace change wasn’t saved")),
            ("reconnect", || {
                Toast::warning("Reconnected. Your last keystrokes may not have arrived.")
            }),
        ];
        for style_name in styles.split(',') {
            let style = ToastStyle::parse(style_name).expect("style");
            for theme in ["dark", "light"] {
                for (state, make) in states.iter().map(|(n, f)| (*n, *f)).chain([
                    ("stack", (|| Toast::success("Copied")) as fn() -> Toast),
                    ("hover", states[0].1),
                ]) {
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
                            .update_preferences(|p| {
                                p.terminal_theme = format!("dirijor-{theme}");
                                p.sidebar_visible = true;
                            })
                            .unwrap();
                        if state == "stack" {
                            store.report_prompt_delivery_failure("diagnostic detail".into());
                        }
                    }
                    let window = cx
                        .open_window(size(px(1000.0), px(650.0)), |window, cx| {
                            cx.new(|cx| {
                                let mut root = RootView::new(
                                    services.clone(),
                                    false,
                                    PreviewScenario::Empty,
                                    window,
                                    cx,
                                );
                                root.preview = state != "stack";
                                root.toast_style = style;
                                let mut grid = diri_term::buffer::GridBuffer::new(100, 36);
                                let sample = "  OpenAI Codex\n\n  /Users/you/work/diri\n\n  Ready to work on your project.\n\n› Ask Codex to do anything";
                                for (y, line) in sample.lines().enumerate() {
                                    for (x, ch) in line.chars().enumerate() {
                                        grid.cells[y * 100 + x].scalar = ch as u32;
                                    }
                                }
                                root.terminal.as_ref().unwrap().update(cx, |terminal, cx| {
                                    terminal.seed_preview_grid_for_test(grid, cx);
                                });
                                root.show_toast(make(), cx);
                                root
                            })
                        })
                        .unwrap();
                    cx.run_until_parked();
                    if state == "hover" {
                        // A point inside the toast for each anchor, so the
                        // corner ✕ and action hover show.
                        let at = match style.anchor() {
                            crate::toast::ToastAnchor::BottomCenter => point(px(560.0), px(612.0)),
                            crate::toast::ToastAnchor::TopRight => point(px(840.0), px(66.0)),
                            crate::toast::ToastAnchor::BottomLeft => point(px(300.0), px(612.0)),
                        };
                        cx.update_window(window.into(), |_, window, cx| {
                            window.simulate_mouse_move(at, cx);
                        })
                        .unwrap();
                        cx.run_until_parked();
                    }
                    cx.capture_screenshot(window.into())
                        .unwrap()
                        .save(dir.join(format!("{style_name}-{theme}-{state}.png")))
                        .unwrap();
                    cx.update_window(window.into(), |_, window, _| window.remove_window())
                        .unwrap();
                    cx.run_until_parked();
                }
            }
        }
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

    /// The card over a session a reboot ended: it blames the computer, not
    /// Diri, and offers to bring back every such session at once.
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "writes restart-ended card screenshots to DIRI_RESTART_SCREENSHOTS"]
    fn render_restart_ended_screenshots() {
        use gpui::{AppContext as _, HeadlessAppContext};
        let output = std::env::var("DIRI_RESTART_SCREENSHOTS").expect("DIRI_RESTART_SCREENSHOTS");
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
        for (name, light) in [("restart-ended-dark", false), ("restart-ended-light", true)] {
            let services = test_services();
            let fixture = SidebarPreviewFixture::make(PreviewScenario::Typical);
            let selected = fixture.selected_session_id.clone().unwrap();
            {
                let mut store = services.store.store.write().unwrap();
                let mut list = fixture.list;
                for session in list.sessions.iter_mut().filter(|session| {
                    !session.is_archived() && session.kind != diri_proto::AgentKind::SHELL
                }) {
                    session.status =
                        diri_proto::SessionStatus::Exited(diri_proto::ExitInfo::restart(true));
                    session.resumability = diri_proto::Resumability::Resumable;
                    session.capabilities = None;
                    session.needs_input = None;
                }
                store.hydrate(list);
                store.mark_connected_for_test();
                store.select(selected.clone());
                store
                    .update_preferences(|prefs| {
                        prefs.sidebar_visible = true;
                        prefs.terminal_theme = if light {
                            "dirijor-light"
                        } else {
                            "dirijor-dark"
                        }
                        .into();
                    })
                    .unwrap();
            }
            let window = cx
                .open_window(size(px(1000.0), px(700.0)), |window, cx| {
                    cx.new(|cx| {
                        RootView::new(services.clone(), false, PreviewScenario::Empty, window, cx)
                    })
                })
                .unwrap();
            cx.run_until_parked();
            // Selecting it auto-resumed it once; with no Engine behind this
            // store that never lands, so settle it back to the card.
            services
                .store
                .store
                .write()
                .unwrap()
                .finish_auto_resume(&selected);
            services.store.publish_local_change();
            cx.update_window(window.into(), |_, window, _| window.refresh())
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

    /// A note Session selected in the real window: the sidebar lists it among
    /// agents and terminals (one agent is its child), and the main area shows
    /// the note editor. `DIRI_VISUAL_OUTPUT=<png>`, `DIRI_VISUAL_THEME=<id>`.
    /// To-dos as agent work inside a note, in the real window: a
    /// marketing launch plan whose to-dos are being worked on by agents.
    /// `DIRI_VISUAL_OUTPUT=<png>`, `DIRI_VISUAL_THEME=<id>`,
    /// `DIRI_WORK_SCENE=tracking|start|tick`.
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "writes the note work-item screenshot artifact"]
    fn render_note_work_screenshot() {
        use diri_notes::edit::Pos;
        use gpui::{AppContext as _, HeadlessAppContext};
        let output = std::env::var("DIRI_VISUAL_OUTPUT").expect("DIRI_VISUAL_OUTPUT");
        let scene = std::env::var("DIRI_WORK_SCENE").unwrap_or_else(|_| "tracking".into());
        let platform = gpui_platform::current_platform(true);
        let mut cx = HeadlessAppContext::with_platform(
            platform.text_system(),
            Arc::new(diri_ui::IconAssets),
            gpui_platform::current_headless_renderer,
        );
        cx.update(|cx| {
            crate::fonts::init(cx);
            cx.set_reduce_motion(true);
            cx.bind_keys(crate::notes::key_bindings());
        });
        let notes_dir = tempfile::tempdir().unwrap();
        let note_store =
            Arc::new(diri_notes::store::NoteStore::open(notes_dir.path().join("notes")).unwrap());
        // `long`: a to-do that wraps, to show Start following its last word.
        let markdown = if scene == "long" {
            crate::notes::work_item_tests::TRACKING.replace(
                "Book the venue for the meetup",
                "Book the venue for the meetup: somewhere near Union Square that holds sixty \
                 people on a Thursday evening, has a projector, and stays under budget.",
            )
        } else {
            crate::notes::work_item_tests::TRACKING.to_owned()
        };
        let (_, doc) = diri_notes::markdown::parse(&markdown);
        let (note_id, _) = note_store.create(doc, None).unwrap();
        let work_width: f32 = std::env::var("DIRI_VISUAL_WIDTH")
            .ok()
            .and_then(|w| w.parse().ok())
            .unwrap_or(1240.0);

        let services = test_services();
        let mut fixture = SidebarPreviewFixture::make(PreviewScenario::from_env(None));
        let template = fixture.list.sessions[0].clone();
        let mut note = template.clone();
        note.id = SessionId::new("s_note_launch");
        note.kind = AgentKind::NOTE;
        note.title = "Launch plan".into();
        note.title_source = diri_proto::TitleSource::DirijorAssigned;
        note.status = diri_proto::SessionStatus::Idle;
        note.needs_input = None;
        note.resumability = diri_proto::Resumability::NotResumable;
        note.note_id = Some(note_id);
        note.parent = None;
        note.pinned = false;
        note.archived_at = None;
        note.git_branch = None;
        note.foreground_agent = None;
        note.pull_requests = None;
        note.worktree_path = None;
        let child = |id: &str, kind: AgentKind, title: &str| {
            let mut s = template.clone();
            s.id = SessionId::new(id);
            s.kind = kind;
            s.title = title.into();
            s.parent = Some(note.id.clone());
            s.archived_at = None;
            s.pinned = false;
            s.needs_input = None;
            s.pull_requests = None;
            s.foreground_agent = None;
            s.last_turn_completed_at = None;
            s
        };
        let mut posts = child("s_posts", AgentKind::CLAUDE_CODE, "Draft 3 LinkedIn posts");
        posts.status = diri_proto::SessionStatus::Working;
        let mut pricing = child("s_pricing", AgentKind::CODEX, "Pick the pricing headline");
        pricing.status =
            diri_proto::SessionStatus::NeedsInput(diri_proto::NeedsInputKind::Question);
        pricing.needs_input = Some(diri_proto::NeedsInputDetail {
            kind: diri_proto::NeedsInputKind::Question,
            source: diri_proto::NeedsInputSource::CodexNotify,
            tool_name: None,
            summary: "Should the headline lead with price or with time saved?".into(),
            prompt_excerpt: None,
            options: None,
            risk_hint: diri_proto::RiskHint::Neutral,
            occurred_at: diri_proto::DateMillis(1.0),
            secret: false,
        });
        let mut faq = child("s_faq", AgentKind::CLAUDE_CODE, "Write the launch FAQ");
        faq.status = diri_proto::SessionStatus::Idle;
        faq.last_turn_completed_at = Some(diri_proto::DateMillis(2.0));
        let mut redirect = child(
            "s_redirect",
            AgentKind::CODEX,
            "Fix the signup redirect loop",
        );
        redirect.status = diri_proto::SessionStatus::Idle;
        redirect.last_turn_completed_at = Some(diri_proto::DateMillis(2.0));
        redirect.pull_requests = Some(vec![
            serde_json::from_value(serde_json::json!({
                "url": "https://github.com/acme/app/pull/612", "number": 612,
                "state": "OPEN", "isDraft": false, "additions": 18, "deletions": 4,
                "changedFiles": 2, "commentCount": 0, "reviewCount": 0,
                "checksPassed": 3, "checksFailed": 0, "checksPending": 0,
                "fetchedAt": 0.0
            }))
            .unwrap(),
        ]);
        fixture.list.sessions.insert(1, note);
        for session in [posts, pricing, faq, redirect] {
            fixture.list.sessions.insert(2, session);
        }
        {
            let mut store = services.store.store.write().unwrap();
            store.hydrate(fixture.list);
            store.set_agent_catalog(crate::agent_setup::bundled_catalog(&[
                "claude-code",
                "codex",
                "gemini",
            ]));
            store.select(SessionId::new("s_note_launch"));
            store
                .update_preferences(|prefs| {
                    prefs.sidebar_visible = true;
                    prefs.terminal_theme = std::env::var("DIRI_VISUAL_THEME")
                        .unwrap_or_else(|_| "dirijor-light".into());
                })
                .unwrap();
        }
        let runtime = Arc::clone(&services.store);
        let note_pane = std::rc::Rc::new(std::cell::RefCell::new(None));
        let window = cx
            .open_window(size(px(work_width), px(780.0)), {
                let note_pane = note_pane.clone();
                move |window, cx| {
                    cx.new(|cx| {
                        let root =
                            RootView::new(services, false, PreviewScenario::Empty, window, cx);
                        let pane = cx.new(|cx| {
                            crate::notes::NotePane::with_store(runtime, Some(note_store), false, cx)
                        });
                        *note_pane.borrow_mut() = Some(pane.clone());
                        if let Some(terminal) = &root.terminal {
                            terminal
                                .update(cx, |terminal, _| terminal.set_note_pane_for_test(pane));
                        }
                        root
                    })
                }
            })
            .unwrap();
        cx.run_until_parked();
        let pane = note_pane.borrow().clone().expect("note pane");
        let editor = cx
            .update(|cx| pane.read(cx).editor_for_test())
            .expect("open note editor");
        let find = |cx: &mut HeadlessAppContext, text: &'static str| {
            cx.update(|cx| {
                editor
                    .read(cx)
                    .editor
                    .blocks()
                    .iter()
                    .position(|b| b.text.starts_with(text))
                    .expect(text)
            })
        };
        let posts = find(&mut cx, "Draft 3 LinkedIn");
        let venue = find(&mut cx, "Book the venue");
        cx.update(|cx| pane.update(cx, |pane, cx| pane.push_work(cx)));
        cx.update_window(window.into(), |_, window, cx| {
            editor.update(cx, |view, cx| {
                // Show one started to-do open, with its context and report.
                view.set_folded(posts, false, cx);
                let end = view.editor.block(venue).text.len();
                view.editor.set_caret(Pos::new(venue, end));
                let focus = gpui::Focusable::focus_handle(view, cx);
                window.focus(&focus, cx);
            });
        })
        .unwrap();
        cx.run_until_parked();
        for _ in 0..2 {
            cx.update_window(window.into(), |_, window, _| window.refresh())
                .unwrap();
            cx.run_until_parked();
        }
        match scene.as_str() {
            "start" => {
                let block = cx.update(|cx| editor.read(cx).editor.block(venue).id);
                cx.update(|cx| {
                    pane.update(cx, |pane, cx| {
                        pane.on_work(&crate::notes::work_item::WorkRequest::Prepare { block }, cx)
                    })
                });
            }
            "tick" => {
                cx.update(|cx| {
                    editor.update(cx, |view, cx| {
                        view.editor.set_caret(Pos::new(posts, 0));
                        view.guard_tick(posts, cx);
                    })
                });
            }
            _ => {}
        }
        cx.run_until_parked();
        for _ in 0..3 {
            cx.update_window(window.into(), |_, window, _| window.refresh())
                .unwrap();
            cx.run_until_parked();
        }
        cx.capture_screenshot(window.into())
            .unwrap()
            .save(&output)
            .unwrap();
        cx.update_window(window.into(), |_, window, _| window.remove_window())
            .unwrap();
        cx.run_until_parked();
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "writes the notes-in-window screenshot artifact"]
    fn render_notes_in_window_screenshot() {
        use gpui::{AppContext as _, HeadlessAppContext};
        let output = std::env::var("DIRI_VISUAL_OUTPUT").expect("DIRI_VISUAL_OUTPUT");
        let platform = gpui_platform::current_platform(true);
        let mut cx = HeadlessAppContext::with_platform(
            platform.text_system(),
            Arc::new(diri_ui::IconAssets),
            gpui_platform::current_headless_renderer,
        );
        cx.update(|cx| {
            crate::fonts::init(cx);
            cx.set_reduce_motion(true);
            cx.bind_keys(crate::notes::key_bindings());
        });
        let notes_dir = tempfile::tempdir().unwrap();
        let note_store =
            Arc::new(diri_notes::store::NoteStore::open(notes_dir.path().join("notes")).unwrap());
        let (_, doc) = diri_notes::markdown::parse(crate::notes::tests::PLAN);
        let (note_id, _) = note_store.create(doc, None).unwrap();
        let history_note_id = note_id.clone();

        let services = test_services();
        let mut fixture = SidebarPreviewFixture::make(PreviewScenario::from_env(None));
        let template = fixture.list.sessions[0].clone();
        let mut note = template.clone();
        note.id = SessionId::new("s_note_plan");
        note.kind = AgentKind::NOTE;
        note.title = "Notes launch plan".into();
        note.title_source = diri_proto::TitleSource::DirijorAssigned;
        note.status = diri_proto::SessionStatus::Idle;
        note.needs_input = None;
        note.resumability = diri_proto::Resumability::NotResumable;
        note.note_id = Some(note_id);
        note.parent = None;
        note.pinned = false;
        note.archived_at = None;
        note.agent_session_id = None;
        note.transcript_path = None;
        note.git_branch = None;
        note.foreground_agent = None;
        note.pull_requests = None;
        note.listening_ports = None;
        note.artifacts = None;
        note.worktree_path = None;
        note.created_at = template.created_at;
        // One agent in the same project works for the note.
        if let Some(child) =
            fixture.list.sessions.iter_mut().find(|s| {
                s.project_id == note.project_id && s.id != template.id && !s.is_archived()
            })
        {
            child.parent = Some(note.id.clone());
        }
        let history_agent = fixture
            .list
            .sessions
            .iter()
            .find(|s| s.parent.as_ref() == Some(&note.id))
            .map_or_else(|| template.id.0.clone(), |s| s.id.0.clone());
        fixture.list.sessions.insert(1, note);
        {
            let mut store = services.store.store.write().unwrap();
            store.hydrate(fixture.list);
            store.select(SessionId::new("s_note_plan"));
            store
                .update_preferences(|prefs| {
                    prefs.sidebar_visible = true;
                    prefs.terminal_theme = std::env::var("DIRI_VISUAL_THEME")
                        .unwrap_or_else(|_| "dirijor-light".into());
                })
                .unwrap();
        }
        let runtime = Arc::clone(&services.store);
        cx.update(|cx| {
            let model = cx.new(|cx| {
                crate::notes::todos::TodosModel::with_store(
                    Arc::clone(&runtime),
                    Some(Arc::clone(&note_store)),
                    false,
                    cx,
                )
            });
            crate::notes::todos::TodosModel::install(model, cx);
        });
        let note_pane = std::rc::Rc::new(std::cell::RefCell::new(None));
        let window = cx
            .open_window(size(px(1240.0), px(780.0)), {
                let note_pane = note_pane.clone();
                move |window, cx| {
                    cx.new(|cx| {
                        let root =
                            RootView::new(services, false, PreviewScenario::Empty, window, cx);
                        let pane = cx.new(|cx| {
                            crate::notes::NotePane::with_store(runtime, Some(note_store), false, cx)
                        });
                        *note_pane.borrow_mut() = Some(pane.clone());
                        if let Some(terminal) = &root.terminal {
                            terminal
                                .update(cx, |terminal, _| terminal.set_note_pane_for_test(pane));
                        }
                        root
                    })
                }
            })
            .unwrap();
        cx.run_until_parked();
        // `DIRI_VISUAL_TODOS=1` opens the To-dos page from the sidebar row.
        if std::env::var_os("DIRI_VISUAL_TODOS").is_some() {
            cx.update_window(window.into(), |root, window, cx| {
                let root = root.downcast::<RootView>().unwrap();
                root.update(cx, |root, cx| root.open_todos(window, cx));
            })
            .unwrap();
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(50));
            cx.run_until_parked();
        }
        // `DIRI_VISUAL_NOTE_MENU=slash|mention|chips|fold|links|link-editor|media|big|select|empty|table|table-wide|table-menu` types
        // into the note:
        // mention chips beside a to-do, then the `/` or `@` menu open at the
        // caret, to judge the menus beside the rest of diri's chrome.
        if let Ok(scene) = std::env::var("DIRI_VISUAL_NOTE_MENU") {
            let pane = note_pane.borrow().clone().expect("note pane");
            let editor = cx
                .update(|cx| pane.read(cx).editor_for_test())
                .expect("open note editor");
            cx.update_window(window.into(), |_, window, cx| {
                use diri_notes::edit::Pos;
                use diri_notes::mention::MentionTarget;
                use gpui::EntityInputHandler as _;
                editor.update(cx, |view, cx| {
                    view.set_mentions(
                        crate::notes::editor_view::MentionDirectory {
                            entries: crate::notes::tests::fixture_mentions(),
                        },
                        cx,
                    );
                    let row = view
                        .editor
                        .blocks()
                        .iter()
                        .position(|b| b.text.starts_with("Agents can"))
                        .expect("fixture to-do");
                    let end = view.editor.block(row).text.len();
                    view.editor.set_caret(Pos::new(row, end));
                    view.replace_text_in_range(None, " — waiting on ", window, cx);
                    let at = view.editor.selection.head.offset;
                    view.editor.insert_mention(
                        at..at,
                        &MentionTarget::Session("s_codex".into()),
                        "@Codex: fix resize flicker",
                        0,
                    );
                    view.replace_text_in_range(None, "and ", window, cx);
                    let at = view.editor.selection.head.offset;
                    view.editor.insert_mention(
                        at..at,
                        &MentionTarget::Note("n-q4".into()),
                        "@Q4 campaign brief",
                        0,
                    );
                    let quick = view
                        .editor
                        .blocks()
                        .iter()
                        .position(|b| b.text.starts_with("Quick capture"))
                        .expect("fixture to-do");
                    let end = view.editor.block(quick).text.len();
                    view.editor.set_caret(Pos::new(quick, end));
                    match scene.as_str() {
                        "select" => {
                            // A selection across blocks, over bold, a link
                            // and chips.
                            let intro = view
                                .editor
                                .blocks()
                                .iter()
                                .position(|b| b.text.starts_with("A rich"))
                                .expect("intro");
                            view.editor.set_selection(diri_notes::edit::Selection {
                                anchor: Pos::new(intro, 2),
                                head: Pos::new(row, 30),
                            });
                            window.focus(&view.focus_handle(cx), cx);
                        }
                        "empty" => {
                            view.reload(
                                diri_notes::edit::Editor::new(&diri_notes::doc::Document::new(
                                    "",
                                    Vec::new(),
                                )),
                                cx,
                            );
                            view.editor.set_caret(Pos::new(1, 0));
                            window.focus(&view.focus_handle(cx), cx);
                        }
                        "table" | "table-wide" | "table-menu" => {
                            // The user's screenshot: an agent's gap analysis,
                            // once raw pipes, now a table.
                            let source = match scene.as_str() {
                                "table-wide" => crate::notes::tests::WIDE_TABLE,
                                _ => crate::notes::tests::AGENT_GAPS,
                            };
                            let (_, doc) = diri_notes::markdown::parse(source);
                            view.reload(diri_notes::edit::Editor::new(&doc), cx);
                            let first = view
                                .editor
                                .blocks()
                                .iter()
                                .position(|b| b.kind.is_cell())
                                .expect("a table");
                            if scene == "table-menu" {
                                // Editing: caret in a body cell, menu open.
                                let cell = first + 3 * 2 + 1;
                                let len = view.editor.block(cell).text.len();
                                view.editor.set_caret(Pos::new(cell, len));
                                window.focus(&view.focus_handle(cx), cx);
                                view.open_table_menu(cx);
                            } else {
                                view.editor.set_caret(Pos::new(0, 0));
                            }
                        }
                        "big" => {
                            // A long note, scrolled deep: only nearby blocks
                            // are laid out.
                            let (_, doc) = diri_notes::markdown::parse(
                                &crate::notes::tests::big_note_markdown(2000),
                            );
                            view.reload(diri_notes::edit::Editor::new(&doc), cx);
                            view.editor.set_caret(Pos::new(0, 0));
                        }
                        "media" => {
                            // A picture and every callout tone.
                            let picture = notes_dir.path().join("funnel.png");
                            std::fs::write(&picture, crate::notes::tests::chart_png(1200, 520))
                                .expect("fixture picture");
                            view.editor.enter(0);
                            view.editor.turn_into(
                                diri_notes::edit::Turn::Kind(diri_notes::doc::BlockKind::Paragraph),
                                0,
                            );
                            view.insert_image_files(&[picture], "drop", cx);
                            for (tone, text) in [
                                (diri_notes::doc::Tone::Tip, "Paste a screenshot straight into a note."),
                                (diri_notes::doc::Tone::Warning, "Q4 budget is capped at $5k."),
                            ] {
                                view.editor.insert_text(text, 0);
                                view.editor.turn_into(
                                    diri_notes::edit::Turn::Kind(diri_notes::doc::BlockKind::Callout(tone)),
                                    0,
                                );
                                view.editor.enter(0);
                            }
                            let first = view
                                .editor
                                .blocks()
                                .iter()
                                .position(|b| b.kind == diri_notes::doc::BlockKind::Image)
                                .expect("image");
                            view.editor.set_caret(Pos::new(first, 0));
                        }
                        "link-editor" => {
                            // ⌘K on "calm" with a Notion URL typed in.
                            let intro = view
                                .editor
                                .blocks()
                                .iter()
                                .position(|b| b.text.starts_with("A rich"))
                                .expect("intro");
                            let calm = view.editor.block(intro).text.find("calm").unwrap_or(0);
                            view.editor.set_selection(diri_notes::edit::Selection {
                                anchor: Pos::new(intro, calm),
                                head: Pos::new(intro, calm + 4),
                            });
                            view.link(&crate::notes::editor_view::Link, window, cx);
                            view.replace_text_in_range(
                                None,
                                "notion.so/acme/Calm-writing-1f2e3d4c5b6a79881f2e3d4c5b6a7988",
                                window,
                                cx,
                            );
                        }
                        "links" => {
                            // A research line a PM would write: tool links
                            // pasted bare become titled chips.
                            view.editor.enter(0);
                            view.editor.turn_into(diri_notes::edit::Turn::Kind(diri_notes::doc::BlockKind::Paragraph), 0);
                            view.editor.insert_text("Sources: ", 0);
                            for url in [
                                "https://www.notion.so/acme/Q4-campaign-brief-1f2e3d4c5b6a79881f2e3d4c5b6a7988",
                                "https://docs.google.com/spreadsheets/d/1AbC/edit",
                                "https://linear.app/acme/issue/GRO-42/launch-email",
                                "https://www.figma.com/design/AbC123/Onboarding-v2",
                                "https://app.hubspot.com/contacts/1/record/0-3/2",
                                "https://acme.slack.com/archives/C024BE91L/p1700000000000100",
                                "https://app.amplitude.com/analytics/acme/chart/abc",
                                "https://github.com/cristicretu/diri/pull/600",
                            ] {
                                view.editor.paste_url(url, 0);
                                view.editor.insert_text(" ", 0);
                            }
                        }
                        "fold" => {
                            // Two nested items under "Quick capture", folded
                            // under "Agents can append".
                            view.editor.enter(0);
                            view.editor.indent(false, 0);
                            view.editor.insert_text("Global hotkey", 0);
                            view.editor.enter(0);
                            view.editor.insert_text("Capture panel", 0);
                            let agents = view
                                .editor
                                .blocks()
                                .iter()
                                .position(|b| b.text.starts_with("Agents can"))
                                .expect("fixture to-do");
                            view.editor.set_caret(Pos::new(agents, 0));
                            let end = view.editor.block(agents).text.len();
                            view.editor.set_caret(Pos::new(agents, end));
                            view.editor.enter(0);
                            view.editor.indent(false, 0);
                            view.editor.insert_text("Hidden while folded", 0);
                            view.set_folded(agents, true, cx);
                        }
                        "slash" => {
                            view.editor.enter(0);
                            view.editor
                                .backspace(diri_notes::edit::Granularity::Grapheme, 0);
                            view.replace_text_in_range(None, "/", window, cx);
                        }
                        "mention" => {
                            view.replace_text_in_range(None, " — ask ", window, cx);
                            view.replace_text_in_range(None, "@", window, cx);
                        }
                        _ => {}
                    }
                    cx.notify();
                });
            })
            .unwrap();
            cx.run_until_parked();
        }
        // `DIRI_VISUAL_NOTE_HISTORY=1` opens Version History with earlier
        // versions a person, an agent, and the person again wrote.
        if std::env::var_os("DIRI_VISUAL_NOTE_HISTORY").is_some() {
            use diri_notes::history::{Author, History, Reason, now_ms};
            let pane = note_pane.borrow().clone().expect("note pane");
            let history = History::new(&notes_dir.path().join("notes"));
            let note_id = history_note_id.clone();
            // Replace the creation version with a believable past.
            let _ = std::fs::remove_dir_all(notes_dir.path().join("notes/.history").join(&note_id));
            let base = crate::notes::tests::PLAN;
            let now = now_ms();
            let agent = Author::Session(history_agent.clone());
            for (age_ms, author, text) in [
                (
                    3 * 86_400_000,
                    Author::User,
                    base.replace("\n## Open questions", "\n## Open questions\n\n> Draft"),
                ),
                (
                    2 * 3_600_000,
                    agent,
                    format!("{base}\n- Finding: quick capture needs a global hotkey\n"),
                ),
                (
                    20 * 60_000,
                    Author::User,
                    format!(
                        "{base}\n- Finding: quick capture needs a global hotkey\n- [ ] Pick the hotkey\n"
                    ),
                ),
            ] {
                history
                    .record(&note_id, &text, &author, Reason::Write, now - age_ms)
                    .unwrap();
            }
            cx.update_window(window.into(), |_, window, cx| {
                pane.update(cx, |pane, cx| {
                    pane.open_versions(&crate::commands::NoteVersionHistory, window, cx);
                    pane.select_version(1, cx);
                });
            })
            .unwrap();
            cx.run_until_parked();
        }
        if std::env::var("DIRI_VISUAL_NOTE_MENU").as_deref() == Ok("big") {
            let pane = note_pane.borrow().clone().expect("note pane");
            let editor = cx
                .update(|cx| pane.read(cx).editor_for_test())
                .expect("open note editor");
            // Scroll like a trackpad: many steps, a frame each, so heights
            // are measured as blocks come into view.
            for _ in 0..150 {
                cx.update_window(window.into(), |_, _, cx| {
                    editor.update(cx, |view, cx| view.scroll_by_for_test(px(-120.0), cx));
                })
                .unwrap();
                cx.run_until_parked();
                cx.update_window(window.into(), |_, window, cx| window.draw(cx).clear())
                    .unwrap();
            }
        }
        for _ in 0..3 {
            cx.update_window(window.into(), |_, window, _| window.refresh())
                .unwrap();
            cx.run_until_parked();
        }
        cx.capture_screenshot(window.into())
            .unwrap()
            .save(&output)
            .unwrap();
        cx.update_window(window.into(), |_, window, _| window.remove_window())
            .unwrap();
        cx.run_until_parked();
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
        for (name, installed, sessions, light, signed_in) in [
            ("no-agents-dark", &[][..], false, false, None),
            ("ready-dark", ready, false, false, Some(true)),
            ("ready-light", ready, false, true, Some(true)),
            ("signed-out-dark", ready, false, false, Some(false)),
            ("signed-out-light", ready, false, true, Some(false)),
            ("resting-dark", ready, true, false, None),
        ] {
            let services = test_services();
            {
                let mut store = services.store.store.write().unwrap();
                if sessions {
                    store.hydrate(SidebarPreviewFixture::make(PreviewScenario::Typical).list);
                }
                let mut catalog = crate::agent_setup::bundled_catalog(installed);
                for agent in &mut catalog.agents {
                    if agent.path.is_some() {
                        agent.signed_in = signed_in;
                    }
                }
                store.set_agent_catalog(catalog);
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
    fn settings_hides_the_inspector_seam_and_takes_terminal_focus(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| cx.set_reduce_motion(true));
        let services = test_services();
        let fixture = SidebarPreviewFixture::make(PreviewScenario::Typical);
        services.store.store.write().unwrap().hydrate(fixture.list);
        let (root, cx) = cx.add_window_view(move |window, cx| {
            RootView::new(services, false, PreviewScenario::Empty, window, cx)
        });
        cx.simulate_resize(size(px(1200.0), px(800.0)));
        root.update(cx, |root, cx| {
            root.inspector_open = true;
            root.inspector_seam = 440.0;
            cx.notify();
        });
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("inspector-resize-handle").is_some(),
            "the open sidebar exposes its resize handle"
        );

        root.update_in(cx, |root, window, cx| {
            let terminal = root.terminal.as_ref().expect("terminal").clone();
            terminal.update(cx, |terminal, cx| terminal.focus(window, cx));
            assert!(terminal.read(cx).is_focused(window));
            root.run_command(CommandId::OpenSettings, window, cx);
            assert!(
                !terminal.read(cx).is_focused(window),
                "settings must take focus off the session behind it"
            );
            assert!(
                root.utility_surfaces
                    .as_ref()
                    .expect("settings")
                    .read(cx)
                    .focus_handle(cx)
                    .contains_focused(window, cx)
            );
        });
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("inspector-resize-handle").is_none(),
            "settings covers the sidebar, so its seam must not stay hittable"
        );

        root.update_in(cx, |root, window, cx| {
            root.run_command(CommandId::OpenSettings, window, cx);
            assert!(
                root.terminal
                    .as_ref()
                    .expect("terminal")
                    .read(cx)
                    .is_focused(window),
                "closing settings returns focus to the session"
            );
        });
    }

    #[gpui::test]
    fn one_escape_closes_settings_and_returns_to_the_session(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| cx.set_reduce_motion(true));
        // Each way of arriving on a page: straight from the shortcut, a click
        // on a tab in the sidebar's settings navigation, a click into the
        // empty search field, and keyboard focus left on the sidebar. None of
        // them leaves a nested action open.
        for arrival in [
            None,
            Some("SETTINGS_TAB_What's New"),
            Some("SETTINGS_TAB_Appearance"),
            Some("settings-search"),
            Some("sidebar"),
        ] {
            let services = test_services();
            let fixture = SidebarPreviewFixture::make(PreviewScenario::Typical);
            services.store.store.write().unwrap().hydrate(fixture.list);
            let (root, cx) = cx.add_window_view(move |window, cx| {
                RootView::new(services, false, PreviewScenario::Empty, window, cx)
            });
            cx.simulate_resize(size(px(1200.0), px(800.0)));
            root.update_in(cx, |root, window, cx| {
                root.utility_surfaces
                    .as_ref()
                    .expect("settings")
                    .update(cx, |surfaces, _| {
                        surfaces.seed_release_notes(diri_updater::ReleaseNotes {
                            tag_name: "v0.6.0".into(),
                            name: Some("diri 0.6.0".into()),
                            body: "## Highlights\n\n- Faster sessions".into(),
                            published_at: Some("2026-09-05T12:17:55Z".into()),
                        })
                    });
                let terminal = root.terminal.as_ref().expect("terminal").clone();
                terminal.update(cx, |terminal, cx| terminal.focus(window, cx));
                root.run_command(CommandId::OpenSettings, window, cx);
            });
            cx.run_until_parked();
            if arrival == Some("sidebar") {
                root.update_in(cx, |root, window, cx| {
                    root.sidebar
                        .update(cx, |sidebar, cx| sidebar.focus(window, cx));
                    assert!(root.sidebar.read(cx).is_focused(window));
                });
            } else if let Some(selector) = arrival {
                let target = cx
                    .debug_bounds(selector)
                    .unwrap_or_else(|| panic!("{selector} is painted"));
                cx.simulate_click(target.center(), Modifiers::default());
            }
            cx.run_until_parked();

            cx.simulate_keystrokes("escape");
            cx.run_until_parked();
            root.update_in(cx, |root, window, cx| {
                assert!(
                    !root
                        .utility_surfaces
                        .as_ref()
                        .expect("settings")
                        .read(cx)
                        .is_settings_open(),
                    "one Escape must close settings after {arrival:?}"
                );
                assert!(
                    root.terminal
                        .as_ref()
                        .expect("terminal")
                        .read(cx)
                        .is_focused(window),
                    "closing settings after {arrival:?} returns focus to the session"
                );
            });
        }
    }

    #[gpui::test]
    fn escape_clears_a_settings_search_before_closing_settings(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| cx.set_reduce_motion(true));
        let services = test_services();
        let (root, cx) = cx.add_window_view(move |window, cx| {
            RootView::new(services, false, PreviewScenario::Empty, window, cx)
        });
        cx.simulate_resize(size(px(1200.0), px(800.0)));
        root.update_in(cx, |root, window, cx| {
            root.run_command(CommandId::OpenSettings, window, cx);
        });
        cx.run_until_parked();
        let search = cx.debug_bounds("settings-search").expect("settings search");
        cx.simulate_click(search.center(), Modifiers::default());
        cx.simulate_keystrokes("a g");
        cx.run_until_parked();
        let settings_open = |root: &Entity<RootView>, cx: &mut gpui::VisualTestContext| {
            root.read_with(cx, |root, cx| {
                root.utility_surfaces
                    .as_ref()
                    .expect("settings")
                    .read(cx)
                    .is_settings_open()
            })
        };

        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        assert!(
            settings_open(&root, cx),
            "a search with text spends Escape on clearing it"
        );
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        assert!(
            !settings_open(&root, cx),
            "the emptied search lets the next Escape close settings"
        );
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
    fn auxiliary_terminal_shortcut_closes_focused_inspector_terminal(
        cx: &mut gpui::TestAppContext,
    ) {
        let services = test_services();
        let mut parent = SidebarPreviewFixture::make(PreviewScenario::Typical)
            .list
            .sessions[0]
            .clone();
        parent.parent = None;
        let parent_id = parent.id.clone();
        let mut auxiliary = parent.clone();
        auxiliary.id = SessionId::new("auxiliary-terminal");
        auxiliary.kind = AgentKind::SHELL;
        auxiliary.parent = Some(parent_id.clone());
        auxiliary.title = crate::store::AUXILIARY_TERMINAL_TITLE.to_owned();
        {
            let mut store = services.store.store.write().expect("store");
            store.upsert_session(parent);
            store.upsert_session(auxiliary);
            store.select(parent_id);
        }
        let (root, cx) = cx.add_window_view(move |window, cx| {
            RootView::new(services, false, PreviewScenario::Empty, window, cx)
        });
        cx.simulate_resize(size(px(1_000.0), px(700.0)));
        cx.run_until_parked();

        root.update_in(cx, |root, window, cx| {
            root.inspector
                .as_ref()
                .unwrap()
                .update(cx, |inspector, cx| {
                    inspector.select_workspace(crate::inspector::WorkspaceSurface::Terminal, cx);
                });
            root.run_command(CommandId::ToggleAuxiliaryTerminal, window, cx);
            assert!(root.inspector_open);
            assert!(
                root.auxiliary_terminal
                    .as_ref()
                    .unwrap()
                    .read(cx)
                    .is_focused(window)
            );
            let shell = root.auxiliary_id.clone();

            root.run_command(CommandId::ToggleAuxiliaryTerminal, window, cx);
            assert!(!root.inspector_open);
            assert!(root.focus.is_focused(window));
            assert_eq!(root.auxiliary_id, shell);
            assert!(root.auxiliary_terminal.is_some());

            root.run_command(CommandId::ToggleAuxiliaryTerminal, window, cx);
            assert!(root.inspector_open);
            window.focus(&root.focus, cx);
            root.run_command(CommandId::ToggleAuxiliaryTerminal, window, cx);
            assert!(root.inspector_open);
            assert!(
                root.auxiliary_terminal
                    .as_ref()
                    .unwrap()
                    .read(cx)
                    .is_focused(window)
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

    #[gpui::test]
    fn a_seam_ticks_once_as_it_meets_the_end_of_its_travel(cx: &mut gpui::TestAppContext) {
        // The sidebar's own clamp.
        const MIN_SIDEBAR_WIDTH: f32 = 200.0;
        const MAX_SIDEBAR_WIDTH: f32 = 400.0;
        let services = test_services();
        let (root, cx) = cx.add_window_view(move |window, cx| {
            RootView::new(services, true, PreviewScenario::Typical, window, cx)
        });
        root.update(cx, |root, cx| {
            let _ = haptics::testing::take();
            root.resize_origin = Some((300.0, 300.0));
            // Free travel, then well past the minimum, then resting there.
            for x in ((MIN_SIDEBAR_WIDTH as i32 - 60)..300).rev() {
                root.drag_resize(x as f32, cx);
            }
            root.drag_resize(MIN_SIDEBAR_WIDTH - 60.0, cx);
            assert_eq!(
                haptics::testing::take(),
                [(Haptic::Limit, haptics::key("sidebar-seam", false))]
            );
            for x in (MIN_SIDEBAR_WIDTH as i32 - 60)..(MAX_SIDEBAR_WIDTH as i32 + 60) {
                root.drag_resize(x as f32, cx);
            }
            assert_eq!(
                haptics::testing::take(),
                [(Haptic::Limit, haptics::key("sidebar-seam", true))]
            );
            root.finish_resize(cx);

            // The inspector grows leftwards and shares the same rule.
            root.inspector_max_width = 600.0;
            root.inspector_width = 440.0;
            root.inspector_resize_origin = Some((700.0, 440.0));
            for x in (400..700).rev() {
                root.drag_inspector_resize(x as f32, cx);
            }
            assert_eq!(
                haptics::testing::take(),
                [(Haptic::Limit, haptics::key("inspector-seam", true))]
            );
            root.finish_inspector_resize(cx);

            // A width set by the app, with no drag behind it, is silent.
            root.sidebar
                .update(cx, |sidebar, cx| sidebar.set_width(10.0, cx));
            assert_eq!(haptics::testing::take(), []);
        });
    }

    #[gpui::test]
    fn the_terminal_split_ticks_at_the_end_of_its_travel(cx: &mut gpui::TestAppContext) {
        let services = test_services();
        let (root, cx) = cx.add_window_view(move |window, cx| {
            RootView::new(services, true, PreviewScenario::Typical, window, cx)
        });
        root.update(cx, |root, cx| {
            let _ = haptics::testing::take();
            root.terminal_available_height = 800.0;
            root.terminal_resize_origin = Some((400.0, 400.0));
            for y in (0..400).rev() {
                root.drag_terminal_resize(y as f32, cx);
            }
            assert_eq!(
                haptics::testing::take(),
                [(Haptic::Limit, haptics::key("terminal-seam", false))]
            );
            root.finish_terminal_resize(cx);
        });
    }

    #[gpui::test]
    fn the_overview_pinch_ticks_once_at_the_commit_threshold(cx: &mut gpui::TestAppContext) {
        let services = test_services();
        {
            // The pinch picks up the selected session's page.
            let mut store = services.store.store.write().unwrap();
            store.hydrate(SidebarPreviewFixture::make(PreviewScenario::Typical).list);
            store.select(SessionId::new("preview-claude"));
        }
        let (root, cx) = cx.add_window_view(move |window, cx| {
            RootView::new(services, false, PreviewScenario::Typical, window, cx)
        });
        cx.update(|window, _| window.activate_window());
        cx.run_until_parked();
        let _ = haptics::testing::take();
        root.update_in(cx, |root, window, cx| {
            assert!(root.session_surfaces.is_some() && window.is_window_active());
            let mut pinch = |delta: f32, phase| {
                let event = gpui::PinchEvent {
                    position: gpui::point(px(400.0), px(300.0)),
                    delta,
                    modifiers: Modifiers::default(),
                    phase,
                };
                root.handle_pinch(&event, window, cx);
            };
            pinch(0.0, gpui::TouchPhase::Started);
            assert_eq!(
                haptics::testing::take(),
                [],
                "starting a pinch is not a threshold"
            );
            // In past the commit point, back out across it, and in again.
            for _ in 0..3 {
                for _ in 0..8 {
                    pinch(-0.1, gpui::TouchPhase::Moved);
                }
                pinch(1.5, gpui::TouchPhase::Moved);
            }
        });
        assert_eq!(
            haptics::testing::take(),
            [(Haptic::LevelChange, haptics::key("overview-pinch", ()))],
            "one threshold, one tick per gesture"
        );
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
            system_restart: false,
        });
        assert!(!is_quote_target(&exited));
    }
}
