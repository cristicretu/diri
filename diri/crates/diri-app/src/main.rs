mod agent_catalog;
mod app_theme;
mod clipboard_transfer;
mod code_intelligence;
mod code_viewer;
mod commands;
mod composer;
#[cfg(unix)]
mod daemon_launch;
mod delegation;
mod dev_build;
mod diagnostics;
pub mod diff;
mod external_drop;
pub mod fonts;
pub mod fuzzy;
mod git_review;
pub mod history;
mod icons;
mod inspector;
mod launch_recipe;
mod launcher;
pub mod markdown;
mod markdown_view;
#[cfg(any(target_os = "macos", test))]
mod menu_inbox;
pub mod navigation;
pub mod notifications;
pub mod palette;
mod platform;
pub mod query_editor;
pub mod quick_open;
pub mod quote;
mod recovery;
pub mod review_prompt;
pub mod root;
pub mod seam;
mod session_surfaces;
pub mod settings;
pub mod sidebar;
pub mod sounds;
mod status_debug;
mod surface_shell;
pub mod switcher;
pub mod terminal_pane;
pub mod transcript;
pub mod updates;
pub mod usage;
mod workbench;
pub mod worktrees;

#[cfg(target_os = "macos")]
mod macos;

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use dev_build::DevBuildIdentity;
use diri_client::DaemonClient;
#[cfg(target_os = "macos")]
use gpui::SystemMenuType;
use gpui::{
    App, AppContext as _, Bounds, Menu, MenuItem, OsAction, TitlebarOptions, Window,
    WindowBackgroundAppearance, WindowBounds, WindowOptions, point, px, size,
};
use gpui_platform::application;
use root::RootView;
use sidebar::{PreviewScenario, SidebarPreviewFixture};
use tokio::runtime::{Builder as RuntimeBuilder, Runtime};

#[cfg(target_os = "macos")]
use crate::commands::HideApp;
use crate::commands::{
    CloseSession, CloseWindow, CopySelection, OpenLauncher, Paste, Quit, ReopenSession,
};
use crate::store::{StoreRuntime, WindowMode, WindowPlacement};
use crate::updates::UpdateHandle;
use crate::usage::{
    TranscriptInvalidation, TranscriptWatcher, UsageSnapshot, UsageStore, merge_fleet_usage,
};

pub mod store;

const MIN_WINDOW_WIDTH: f32 = 900.0;
const MIN_WINDOW_HEIGHT: f32 = 560.0;
const USAGE_REFRESH_DEBOUNCE: Duration = Duration::from_secs(2);
const USAGE_RECONCILE_INTERVAL: Duration = Duration::from_secs(30 * 60);

/// Install native application menus without exposing actions that the current
/// desktop cannot implement.
fn install_app_menus(cx: &mut App) {
    cx.on_action(|_: &Quit, cx| cx.quit());
    #[cfg(target_os = "macos")]
    cx.on_action(|_: &HideApp, cx| cx.hide());
    cx.on_action(|_: &CloseWindow, cx| {
        if let Some(window) = cx.active_window() {
            let _ = window.update(cx, |_, window, _| window.remove_window());
        }
    });
    // Cmd+W routes through CloseSession: RootView closes the selected
    // session and only propagates here — closing the window — when no
    // session is selected.
    cx.on_action(|_: &CloseSession, cx| {
        if let Some(window) = cx.active_window() {
            let _ = window.update(cx, |_, window, _| window.remove_window());
        }
    });
    #[cfg(target_os = "macos")]
    cx.set_menus([
        Menu::new("diri").items([
            MenuItem::os_submenu("Services", SystemMenuType::Services),
            MenuItem::separator(),
            MenuItem::action("Hide diri", HideApp),
            MenuItem::separator(),
            MenuItem::action("Quit diri", Quit),
        ]),
        Menu::new("File").items([MenuItem::action("New Session", OpenLauncher)]),
        Menu::new("Edit").items([
            MenuItem::os_action("Copy", CopySelection, OsAction::Copy),
            MenuItem::os_action("Paste", Paste, OsAction::Paste),
        ]),
        Menu::new("Window").items([
            MenuItem::action("Close Session", CloseSession),
            MenuItem::action("Reopen Closed Session", ReopenSession),
            MenuItem::action("Close Window", CloseWindow),
        ]),
    ]);
    #[cfg(not(target_os = "macos"))]
    cx.set_menus([
        Menu::new("File").items([
            MenuItem::action("New Session", OpenLauncher),
            MenuItem::separator(),
            MenuItem::action("Quit diri", Quit),
        ]),
        Menu::new("Edit").items([
            MenuItem::os_action("Copy", CopySelection, OsAction::Copy),
            MenuItem::os_action("Paste", Paste, OsAction::Paste),
        ]),
        Menu::new("Window").items([
            MenuItem::action("Close Session", CloseSession),
            MenuItem::action("Reopen Closed Session", ReopenSession),
            MenuItem::action("Close Window", CloseWindow),
        ]),
    ]);
}

pub(crate) struct AppServices {
    // StoreRuntime drops/aborts its client tasks before the executor is dropped.
    pub(crate) store: Arc<StoreRuntime>,
    pub(crate) usage_tx: tokio::sync::watch::Sender<UsageSnapshot>,
    pub(crate) updates: UpdateHandle,
    pub(crate) dev_build: Option<DevBuildIdentity>,
    #[cfg(unix)]
    daemon_startup: Option<daemon_launch::DeferredDaemonStartup>,
    // Declared last so every service and its owned startup handle drops before
    // the executor during early unwinding as well as ordinary app shutdown.
    pub(crate) tokio: Arc<Runtime>,
}

fn main() {
    if std::env::var_os("DIRI_PROBE_SYMBOLS").is_some() {
        icons::probe();
        return;
    }

    let smoke_test = std::env::var_os("DIRI_UI_SMOKE_TEST").is_some();
    let preview_value = std::env::var("DIRIJOR_SIDEBAR_PREVIEW").ok();
    let preview = smoke_test || preview_value.as_deref().is_some_and(|value| value != "0");
    let scenario_value = std::env::var("DIRIJOR_SIDEBAR_SCENARIO")
        .ok()
        .or_else(|| preview_value.filter(|value| value != "1"));
    let scenario = PreviewScenario::from_env(scenario_value.as_deref());
    #[cfg(target_os = "macos")]
    let bundle_id = macos::bundle_identifier();
    #[cfg(target_os = "macos")]
    let dev_build = DevBuildIdentity::from_process_environment(bundle_id.as_deref());
    #[cfg(not(target_os = "macos"))]
    let dev_build = DevBuildIdentity::from_process_environment(None);

    // The client runtime multiplexes one daemon socket plus a handful of
    // event-driven housekeeping tasks. The default Tokio constructor creates
    // one worker per CPU core (14 on current Apple Silicon), which is needless
    // scheduler/thread-stack overhead for this I/O-bound desktop client.
    let tokio = Arc::new(
        RuntimeBuilder::new_multi_thread()
            .worker_threads(2)
            .thread_name("diri-async")
            .enable_all()
            .build()
            .expect("failed to start diri async runtime"),
    );

    // Plan app-owned Engine supervision now, but do not probe the socket or
    // hash the bundled executable on GPUI's first-paint path. The one-shot plan
    // is consumed only after the first window has been opened below.
    #[cfg(unix)]
    let daemon_startup = (!preview)
        .then(daemon_launch::DeferredDaemonStartup::for_process)
        .flatten();
    #[cfg(unix)]
    let defer_client_start = daemon_startup.is_some();
    #[cfg(not(unix))]
    let defer_client_start = false;

    let client = Arc::new(DaemonClient::new());
    let store_runtime = {
        let _guard = tokio.enter();
        Arc::new(if preview {
            StoreRuntime::inert()
        } else if defer_client_start {
            StoreRuntime::start_default_deferred(Arc::clone(&client))
                .expect("failed to load diri state")
        } else {
            StoreRuntime::start_default(Arc::clone(&client)).expect("failed to load diri state")
        })
    };
    if preview {
        let fixture = SidebarPreviewFixture::make(scenario);
        let selected = fixture.selected_session_id.clone();
        let mut store = store_runtime
            .store
            .write()
            .expect("preview session store lock poisoned");
        store.hydrate(fixture.list);
        if let Some(selected) = selected {
            store.select(selected);
        }
        if scenario == PreviewScenario::Artifacts {
            store
                .update_preferences(|prefs| {
                    prefs.inspector_open = true;
                    prefs.inspector_width = 480.0;
                    prefs.inspector_tab = store::InspectorTab::Artifacts;
                })
                .expect("headless preview preferences");
        }
    }
    let (usage_tx, _) = tokio::sync::watch::channel(UsageSnapshot::default());
    if !preview {
        let usage_tx = usage_tx.clone();
        let usage_home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/nonexistent"));
        tokio.spawn(async move {
            let mut store = UsageStore::new();
            let roots = store.watch_roots();
            let Some(returned_store) =
                publish_usage_refresh(store, &usage_tx, None, &usage_home).await
            else {
                return;
            };
            store = returned_store;
            let mut watcher = TranscriptWatcher::new(&roots).ok();
            let mut invalidated = HashSet::<PathBuf>::new();
            let mut reconcile = false;
            let mut refresh_due: Option<tokio::time::Instant> = None;
            let mut reconciliation = tokio::time::interval(USAGE_RECONCILE_INTERVAL);
            reconciliation.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            reconciliation.tick().await; // initial full refresh already completed above
            loop {
                tokio::select! {
                    event = async {
                        match watcher.as_mut() {
                            Some(watcher) => watcher.recv().await,
                            None => std::future::pending().await,
                        }
                    } => {
                        match event {
                            Some(TranscriptInvalidation::Paths(paths)) => {
                                invalidated.extend(paths);
                            }
                            Some(TranscriptInvalidation::Reconcile) | None => {
                                reconcile = true;
                            }
                        }
                        refresh_due = Some(tokio::time::Instant::now() + USAGE_REFRESH_DEBOUNCE);
                    }
                    _ = async {
                        if let Some(deadline) = refresh_due {
                            tokio::time::sleep_until(deadline).await;
                        } else {
                            std::future::pending().await
                        }
                    } => {
                        refresh_due = None;
                        let paths = (!reconcile).then(|| invalidated.drain().collect::<Vec<_>>());
                        invalidated.clear();
                        reconcile = false;
                        let Some(returned_store) =
                            publish_usage_refresh(store, &usage_tx, paths, &usage_home).await
                        else {
                            return;
                        };
                        store = returned_store;
                    }
                    _ = reconciliation.tick() => {
                        // FSEvents can coalesce/drop events. A rare reconciliation
                        // preserves correctness without tying a recursive walk to
                        // every session status/resource update.
                        let Some(returned_store) =
                            publish_usage_refresh(store, &usage_tx, None, &usage_home).await
                        else {
                            return;
                        };
                        store = returned_store;
                        invalidated.clear();
                        reconcile = false;
                        refresh_due = None;
                    }
                }
            }
        });
    }
    let updates = if preview {
        updates::inert()
    } else {
        let (automatic_updates, skipped_update) = {
            let store = store_runtime
                .store
                .read()
                .expect("session store lock poisoned");
            let prefs = store.preferences();
            (
                prefs.automatic_updates,
                Some(prefs.skipped_update_version.clone()),
            )
        };
        updates::spawn(&tokio, automatic_updates, skipped_update)
    };
    let services = Arc::new(AppServices {
        store: store_runtime,
        usage_tx,
        updates,
        dev_build,
        #[cfg(unix)]
        daemon_startup,
        tokio,
    });

    let app = application().with_assets(diri_ui::IconAssets);
    // Clicking the Dock icon with no windows must bring the app back
    // (AppKit reopen). Without this a closed window strands a live process
    // that "opens" to nothing.
    let reopen_services = Arc::clone(&services);
    app.on_reopen(move |cx| {
        if cx.windows().is_empty() {
            open_main_window(cx, Arc::clone(&reopen_services), preview, scenario);
        }
        cx.activate(true);
    });
    app.run(move |cx: &mut App| {
        load_system_fonts(cx);
        #[cfg(target_os = "macos")]
        diri_ui::set_mark_rasterizer(macos::brand_raster::raster_mark);
        commands::bind_default_keys(cx);
        install_app_menus(cx);
        let quit_services = Arc::clone(&services);
        let quit_updates = services.updates.clone();
        let release_owned_daemon =
            !preview && std::env::var_os(diri_proto::paths::ENV_SOCKET).is_none();
        cx.on_app_quit(move |_| {
            let quit_services = Arc::clone(&quit_services);
            let quit_updates = quit_updates.clone();
            // This runs while GPUI is constructing the quit future, before its
            // 200 ms grace period begins. The coordinator never transfers its
            // sole task handle into that cancellable future: pending startup
            // and idle release remain owned by runtime blocking workers.
            #[cfg(unix)]
            let startup_owns_release =
                quit_services
                    .daemon_startup
                    .as_ref()
                    .is_some_and(|startup| {
                        startup
                            .request_shutdown(&quit_services.tokio, quit_services.store.client());
                        true
                    });
            #[cfg(not(unix))]
            let startup_owns_release = false;
            async move {
                if let Err(error) = quit_services
                    .store
                    .store
                    .write()
                    .expect("session store lock poisoned")
                    .persist_preferences()
                {
                    eprintln!("diri: could not flush preferences while quitting: {error}");
                }
                // Automatic updates are already downloaded and verified. Start
                // the detached swap helper now; it waits for this process to
                // exit and deliberately does not reopen an app the user quit.
                quit_updates.install_on_quit();
                if release_owned_daemon
                    && !startup_owns_release
                    && let Err(error) = quit_services.store.client().shutdown_daemon_if_idle().await
                {
                    eprintln!("diri: could not release the idle Engine while quitting: {error}");
                }
                quit_services.store.shutdown().await;
            }
        })
        .detach();
        open_main_window(cx, Arc::clone(&services), preview, scenario);
        cx.activate(true);
        // `open_main_window` must stay before this call. The supervisor can
        // spend up to five seconds probing and retiring an outdated Engine;
        // it runs on Tokio's blocking pool and releases the reconnect loop
        // only after replacement is complete, so the UI cannot race a daemon
        // that is about to shut down.
        #[cfg(unix)]
        if let Some(startup) = services.daemon_startup.as_ref() {
            startup.after_window_open(&services.tokio, Arc::clone(services.store.client()));
        }
        if smoke_test {
            cx.spawn(async move |cx| {
                cx.background_executor()
                    .timer(Duration::from_millis(750))
                    .await;
                eprintln!("diri: UI smoke window opened successfully");
                cx.update(|cx| cx.quit());
            })
            .detach();
        }
    });
}

async fn publish_usage_refresh(
    mut store: UsageStore,
    usage_tx: &tokio::sync::watch::Sender<UsageSnapshot>,
    invalidated: Option<Vec<PathBuf>>,
    home: &std::path::Path,
) -> Option<UsageStore> {
    let (store, snapshot) = tokio::task::spawn_blocking(move || {
        let snapshot = match invalidated {
            Some(paths) => store.refresh_paths(&paths),
            None => store.refresh(),
        };
        (store, snapshot)
    })
    .await
    .ok()?;
    let snapshot = merge_fleet_usage(snapshot, home).await;
    usage_tx.send_replace(snapshot);
    Some(store)
}

fn open_main_window(
    cx: &mut App,
    services: Arc<AppServices>,
    preview: bool,
    scenario: PreviewScenario,
) {
    let perf_large_window = std::env::var_os("DIRI_PERF_LARGE_WINDOW").is_some();
    let initial_size = if perf_large_window {
        size(px(1800.0), px(1100.0))
    } else {
        size(px(1100.0), px(700.0))
    };
    let saved_placement = (!preview && !perf_large_window)
        .then(|| {
            services
                .store
                .store
                .read()
                .expect("session store lock poisoned")
                .preferences()
                .window_placement
                .clone()
        })
        .flatten();
    let (window_bounds, display_id) = saved_placement
        .map(|placement| restore_window_bounds(placement, cx))
        .unwrap_or_else(|| {
            (
                WindowBounds::Windowed(Bounds::centered(None, initial_size, cx)),
                None,
            )
        });
    let app_id = services.dev_build.as_ref().map_or_else(
        || "com.dirijor.diri".to_owned(),
        |build| build.bundle_id().to_owned(),
    );
    let title = services
        .dev_build
        .as_ref()
        .map(|build| build.window_title().into());
    cx.open_window(
        WindowOptions {
            window_bounds: Some(window_bounds),
            display_id,
            window_min_size: Some(size(px(MIN_WINDOW_WIDTH), px(MIN_WINDOW_HEIGHT))),
            // The terminal is an opaque work surface. Marking the whole window
            // blurred forces WindowServer/Metal to retain full-size backdrop
            // surfaces even though only the sidebar used that material.
            window_background: WindowBackgroundAppearance::Opaque,
            app_id: Some(app_id),
            titlebar: Some(TitlebarOptions {
                title,
                appears_transparent: cfg!(target_os = "macos"),
                // GPUI uses top/left insets here: AppKit's native 8 pt origin plus the
                // spec's +12 x / -6 frame-origin nudge maps to 20 pt left and 14 pt top.
                traffic_light_position: cfg!(target_os = "macos")
                    .then_some(point(px(20.0), px(14.0))),
            }),
            ..Default::default()
        },
        move |window, cx| cx.new(|cx| RootView::new(services, preview, scenario, window, cx)),
    )
    .expect("failed to open the diri window");
}

/// Convert GPUI's runtime window state into the JSON-friendly preference
/// representation. This is shared by the bounds observer in `RootView`.
pub(crate) fn current_window_placement(window: &Window, cx: &App) -> WindowPlacement {
    let window_bounds = window.window_bounds();
    let bounds = window_bounds.get_bounds();
    let mode = if window.is_fullscreen() {
        WindowMode::Fullscreen
    } else if window.is_maximized() {
        WindowMode::Maximized
    } else {
        match window_bounds {
            WindowBounds::Windowed(_) => WindowMode::Windowed,
            WindowBounds::Maximized(_) => WindowMode::Maximized,
            WindowBounds::Fullscreen(_) => WindowMode::Fullscreen,
        }
    };
    WindowPlacement {
        display_uuid: window
            .display(cx)
            .and_then(|display| display.uuid().ok())
            .map(|uuid| uuid.to_string()),
        mode,
        x: f32::from(bounds.origin.x),
        y: f32::from(bounds.origin.y),
        width: f32::from(bounds.size.width),
        height: f32::from(bounds.size.height),
    }
}

fn restore_window_bounds(
    placement: WindowPlacement,
    cx: &App,
) -> (WindowBounds, Option<gpui::DisplayId>) {
    let display = placement.display_uuid.as_deref().and_then(|uuid| {
        cx.displays().into_iter().find(|display| {
            display
                .uuid()
                .is_ok_and(|candidate| candidate.to_string() == uuid)
        })
    });
    let display_id = display.as_ref().map(|display| display.id());
    let size = size(px(placement.width), px(placement.height));
    let saved = Bounds::new(point(px(placement.x), px(placement.y)), size);
    // Display arrangements change. Preserve the saved size, but center it on
    // the current primary display if the old origin is now wholly off-screen.
    let visible = display.as_ref().map_or_else(
        || {
            cx.displays()
                .iter()
                .any(|display| saved.intersects(&display.visible_bounds()))
        },
        |display| saved.intersects(&display.visible_bounds()),
    );
    let bounds = if visible {
        saved
    } else {
        Bounds::centered(display_id, size, cx)
    };
    let bounds = match placement.mode {
        WindowMode::Windowed => WindowBounds::Windowed(bounds),
        WindowMode::Maximized => WindowBounds::Maximized(bounds),
        WindowMode::Fullscreen => WindowBounds::Fullscreen(bounds),
    };
    (bounds, display_id)
}

#[cfg(target_os = "macos")]
fn load_system_fonts(cx: &mut App) {
    // GPUI resolves its virtual system family through CoreText. Registering
    // system font file bytes in its in-memory source duplicates tens of
    // megabytes for the process lifetime.
    fonts::init(cx);
}

#[cfg(not(target_os = "macos"))]
fn load_system_fonts(cx: &mut App) {
    fonts::init(cx);
}
