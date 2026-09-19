mod agent_catalog;
mod app_theme;
mod application_notifications;
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
mod editor;
mod empty_workbench;
mod external_drop;
mod floating;
pub mod fonts;
pub mod fuzzy;
#[cfg(test)]
mod gesture_delivery;
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
mod notification_feed;
pub mod notifications;
mod number_flow;
pub mod palette;
mod palette_chrome;
mod palette_workspace;
mod peek_settle;
mod phone_access;
mod platform;
pub mod query_editor;
pub mod quick_open;
pub mod quote;
mod recovery;
pub mod review_prompt;
pub mod root;
pub mod seam;
mod session_presentation;
mod session_surfaces;
pub mod settings;
pub mod sidebar;
mod skills_catalog;
mod skills_page;
pub mod sounds;
mod status_debug;
mod surface_shell;
pub mod switcher;
mod tab_navigation;
mod tab_peek;
mod tab_preview;
pub mod terminal_pane;
pub mod transcript;
pub mod updates;
pub mod usage;
mod window_restore;
mod workbench;
#[cfg(all(test, target_os = "macos"))]
mod workspace_fixture;
#[cfg_attr(not(test), allow(dead_code))]
mod workspace_geometry;
#[cfg_attr(not(test), allow(dead_code))]
mod workspace_preview;
mod workspace_preview_source;
mod workspace_workbench;
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
    App, AppContext as _, Bounds, Menu, MenuItem, OsAction, TitlebarOptions, Window, WindowBounds,
    WindowOptions, point, px, size,
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
    CursorBatch, CursorRefresh, TranscriptInvalidation, TranscriptWatcher, UsageSnapshot,
    UsageStore, merge_fleet_usage,
};
use crate::window_restore::{DisplayFrame, RestorePolicy};

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
    refresh_app_menus(cx);
}

/// Rebuild native menu shortcut labels after the user edits the live keymap.
/// Action handlers are installed only once by `install_app_menus`.
pub(crate) fn refresh_app_menus(cx: &mut App) {
    #[cfg(target_os = "macos")]
    cx.set_menus([
        Menu::new("diri").items([
            MenuItem::os_submenu("Services", SystemMenuType::Services),
            MenuItem::separator(),
            MenuItem::action("Hide diri", HideApp),
            MenuItem::separator(),
            MenuItem::action("Quit diri", Quit),
        ]),
        Menu::new("File").items([
            MenuItem::action("New Session", OpenLauncher),
            MenuItem::action("New Window", commands::NewWindow),
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
    #[cfg(not(target_os = "macos"))]
    cx.set_menus([
        Menu::new("File").items([
            MenuItem::action("New Session", OpenLauncher),
            MenuItem::action("New Window", commands::NewWindow),
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
    pub(crate) usage_limits_refresh: tokio::sync::mpsc::Sender<()>,
    pub(crate) updates: UpdateHandle,
    pub(crate) dev_build: Option<DevBuildIdentity>,
    #[cfg(unix)]
    daemon_startup: Option<daemon_launch::DeferredDaemonStartup>,
    // Declared last so every service and its owned startup handle drops before
    // the executor during early unwinding as well as ordinary app shutdown.
    pub(crate) tokio: Arc<Runtime>,
}

fn main() {
    if cfg!(test) {
        #[cfg(all(test, target_os = "macos"))]
        if std::env::var_os("DIRI_TEST_NATIVE_FIND").is_some() {
            terminal_pane::find_workflow_tests::run_native();
        }
        return;
    }
    #[cfg(all(target_os = "macos", debug_assertions))]
    if std::env::var_os("DIRI_NATIVE_MENU_SMOKE").is_some() {
        macos::menu_bar::smoke_test();
        return;
    }
    #[cfg(all(target_os = "macos", debug_assertions))]
    if std::env::var_os("DIRI_NATIVE_BROWSER_SMOKE").is_some() {
        macos::browser::smoke_test();
        return;
    }
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
            run_usage_updates(
                UsageStore::new(),
                usage_tx,
                usage_home,
                CursorRefresh::default(),
            )
            .await;
        });
    }
    if !preview && std::env::var_os("DIRI_SETTINGS_PREVIEW").is_none() {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/nonexistent"));
        tokio.spawn(usage::watch_remote_usage(
            Arc::clone(&client),
            home,
            usage_tx.clone(),
        ));
    }
    let (usage_limits_refresh, mut limits_requests) = tokio::sync::mpsc::channel(1);
    if !preview && std::env::var_os("DIRI_SETTINGS_PREVIEW").is_none() {
        let usage_tx = usage_tx.clone();
        let client = Arc::clone(&client);
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/nonexistent"));
        tokio.spawn(async move {
            let mut last_request: Option<std::time::Instant> = None;
            while limits_requests.recv().await.is_some() {
                if last_request.is_some_and(|last| last.elapsed() < Duration::from_secs(10)) {
                    continue;
                }
                last_request = Some(std::time::Instant::now());
                // Read credentials only after opening the account menu or
                // explicitly refreshing it. Never during startup/preview.
                if client
                    .wait_until_connected(Duration::from_secs(5))
                    .await
                    .is_err()
                {
                    continue;
                }
                let Ok(accounts) = client.account_profiles().await else {
                    continue;
                };
                let limits = usage::limits::refresh(&home, &accounts).await;
                usage_tx.send_modify(|snapshot| snapshot.limits = limits);
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
        usage_limits_refresh,
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
            // A window the user closed comes back fresh, at its last frame
            // but never straight into full screen.
            open_main_window(
                cx,
                Arc::clone(&reopen_services),
                preview,
                scenario,
                RestorePolicy::FRAME_ONLY,
            );
        }
        cx.activate(true);
    });
    app.run(move |cx: &mut App| {
        load_system_fonts(cx);
        // Menus and popovers open as blurred panels under glass; DIRI_FLOATING_PANELS=0
        // keeps them inside the window for comparison or when a panel misbehaves.
        if std::env::var_os("DIRI_FLOATING_PANELS").is_none_or(|value| value != "0") {
            floating::enable(cx);
        }
        #[cfg(target_os = "macos")]
        diri_ui::set_mark_rasterizer(macos::brand_raster::raster_mark);
        #[cfg(target_os = "macos")]
        macos::observe_scroller_style(cx);
        let shortcut_overrides = services
            .store
            .store
            .read()
            .expect("session store lock poisoned")
            .preferences()
            .shortcut_overrides
            .clone();
        commands::bind_keys(cx, &shortcut_overrides);
        install_app_menus(cx);
        let window_services = services.clone();
        cx.on_action(move |_: &commands::NewWindow, cx| {
            let context = cx.active_window().and_then(|handle| {
                handle
                    .update(cx, |root, window, cx| {
                        root.downcast::<RootView>()
                            .ok()
                            .map(|root| root.read(cx).native_window_context(window, cx))
                    })
                    .ok()
                    .flatten()
            });
            match context {
                Some(context) => open_main_window_with_context(
                    cx,
                    window_services.clone(),
                    preview,
                    scenario,
                    context,
                ),
                None => open_main_window(
                    cx,
                    window_services.clone(),
                    preview,
                    scenario,
                    RestorePolicy::FRAME_ONLY,
                ),
            };
        });
        let quit_services = Arc::clone(&services);
        let quit_updates = services.updates.clone();
        let release_owned_daemon =
            !preview && std::env::var_os(diri_proto::paths::ENV_SOCKET).is_none();
        cx.on_app_quit(move |cx| {
            let quit_services = Arc::clone(&quit_services);
            let quit_updates = quit_updates.clone();
            // Windows are still open here; this is the one moment the full
            // set is known, so record it before preferences are flushed.
            if !preview {
                remember_open_windows(cx, &quit_services.store);
            }
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
        let policy = if preview {
            RestorePolicy::FRAME_ONLY
        } else {
            RestorePolicy::for_system(window_restore::system_keeps_windows_on_quit())
        };
        let key_window = open_main_window(cx, Arc::clone(&services), preview, scenario, policy);
        if policy.extra_windows {
            let additional = services
                .store
                .store
                .read()
                .expect("session store lock poisoned")
                .preferences()
                .additional_windows
                .clone();
            for saved in additional {
                open_main_window_with_context(
                    cx,
                    Arc::clone(&services),
                    preview,
                    scenario,
                    NativeWindowContext {
                        workspace: saved.workspace,
                        selected: saved.selected_session,
                        placement: saved.placement,
                        policy,
                    },
                );
            }
            // The window that was key at quit is key again.
            let _ = key_window.update(cx, |_, window, _| window.activate_window());
        }
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

async fn run_usage_updates(
    mut store: UsageStore,
    usage_tx: tokio::sync::watch::Sender<UsageSnapshot>,
    usage_home: PathBuf,
    mut cursor: CursorRefresh,
) {
    cursor.start(&usage_home, store.cursor_fetch_window());
    let mut cursor_interval = tokio::time::interval(Duration::from_secs(60));
    cursor_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    cursor_interval.tick().await;
    let mut reconciliation = tokio::time::interval(USAGE_RECONCILE_INTERVAL);
    reconciliation.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    reconciliation.tick().await; // initial full refresh follows below
    let roots = store.watch_roots();
    let Some(returned_store) = publish_usage_refresh(store, &usage_tx, None, &usage_home).await
    else {
        return;
    };
    store = returned_store;
    let mut watcher = TranscriptWatcher::new(&roots).ok();
    let mut invalidated = HashSet::<PathBuf>::new();
    let mut reconcile = false;
    let mut refresh_due: Option<tokio::time::Instant> = None;
    loop {
        tokio::select! {
            _ = cursor_interval.tick() => {
                cursor.start(&usage_home, store.cursor_fetch_window());
            }
            result = cursor.next() => {
                if let Ok(batch) = result {
                    let Some(returned_store) = publish_cursor_batch(store, batch, &usage_tx).await else {
                        return;
                    };
                    store = returned_store;
                }
            }
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
}

async fn publish_cursor_batch(
    mut store: UsageStore,
    batch: CursorBatch,
    usage_tx: &tokio::sync::watch::Sender<UsageSnapshot>,
) -> Option<UsageStore> {
    let (store, cursor, history) = tokio::task::spawn_blocking(move || {
        let cursor = store.ingest_cursor_batch(batch);
        let history = store.cursor_history();
        (store, cursor, history)
    })
    .await
    .ok()?;
    usage_tx.send_modify(|snapshot| {
        snapshot.cursor = cursor;
        snapshot.updated_at = usage::Clock::read(&usage::SystemClock).unix_seconds;
        Arc::make_mut(&mut snapshot.history).cursor = history;
    });
    Some(store)
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
    let snapshot = merge_fleet_usage(snapshot.with_limits(Vec::new()), home).await;
    usage_tx.send_modify(|current| {
        let limits = std::mem::take(&mut current.limits);
        let remote = std::mem::take(&mut current.remote);
        *current = snapshot;
        current.limits = limits;
        current.remote = remote;
    });
    Some(store)
}

struct NativeWindowContext {
    workspace: Option<diri_proto::workspace::WorkspaceId>,
    selected: Option<diri_proto::SessionId>,
    placement: WindowPlacement,
    policy: RestorePolicy,
}

/// Where a main window's frame and view state come from.
enum WindowOpen {
    /// The key window's saved placement, under the given policy.
    Restore(RestorePolicy),
    /// A live window's state (⌘N) or a window brought back beside the key one.
    Context(NativeWindowContext),
}

fn open_main_window(
    cx: &mut App,
    services: Arc<AppServices>,
    preview: bool,
    scenario: PreviewScenario,
    policy: RestorePolicy,
) -> gpui::WindowHandle<RootView> {
    open_window(cx, services, preview, scenario, WindowOpen::Restore(policy))
}

fn open_main_window_with_context(
    cx: &mut App,
    services: Arc<AppServices>,
    preview: bool,
    scenario: PreviewScenario,
    context: NativeWindowContext,
) -> gpui::WindowHandle<RootView> {
    open_window(
        cx,
        services,
        preview,
        scenario,
        WindowOpen::Context(context),
    )
}

fn open_window(
    cx: &mut App,
    services: Arc<AppServices>,
    preview: bool,
    scenario: PreviewScenario,
    open: WindowOpen,
) -> gpui::WindowHandle<RootView> {
    let perf_large_window = std::env::var_os("DIRI_PERF_LARGE_WINDOW").is_some();
    let initial_size = if perf_large_window {
        size(px(1800.0), px(1100.0))
    } else {
        size(px(1100.0), px(700.0))
    };
    let (saved_placement, policy, context) = match open {
        WindowOpen::Restore(policy) => {
            let saved = (!preview && !perf_large_window)
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
            (saved, policy, None)
        }
        WindowOpen::Context(context) => (
            Some(context.placement.clone()),
            context.policy,
            Some(context),
        ),
    };
    let selected_override = context.as_ref().map(|context| context.selected.clone());
    let workspace_override = context.map(|context| context.workspace);
    let (window_bounds, display_id) = saved_placement
        .map(|placement| restore_window_bounds(&placement, policy, cx))
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
    let window_material = services
        .store
        .store
        .read()
        .expect("session store lock poisoned")
        .preferences()
        .window_material;
    cx.open_window(
        WindowOptions {
            window_bounds: Some(window_bounds),
            display_id,
            window_min_size: Some(size(px(MIN_WINDOW_WIDTH), px(MIN_WINDOW_HEIGHT))),
            // Glass blurs the desktop behind the whole window, which makes
            // WindowServer retain a backdrop for it; the Appearance settings
            // offer an opaque window for anyone who would rather not pay that.
            window_background: crate::root::window_background(window_material),
            app_id: Some(app_id),
            // Diri paints the whole titlebar. Keep AppKit from turning presses on
            // its controls into window drags; RootView explicitly moves the
            // window from unhandled titlebar presses instead.
            app_owns_titlebar_drag: cfg!(target_os = "macos"),
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
        move |window, cx| {
            cx.new(|cx| {
                if workspace_override.is_some() {
                    RootView::new_with_selection(
                        services,
                        preview,
                        scenario,
                        workspace_override,
                        selected_override,
                        window,
                        cx,
                    )
                } else {
                    RootView::new(services, preview, scenario, window, cx)
                }
            })
        },
    )
    .expect("failed to open the diri window")
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

/// Record every open main window so the next launch can bring them back. The
/// key window becomes the placement every launch restores; the rest ride
/// along for launches that keep windows.
fn remember_open_windows(cx: &mut App, runtime: &StoreRuntime) {
    let active = cx.active_window();
    let mut windows = Vec::new();
    for handle in cx.windows() {
        let saved = handle
            .update(cx, |root, window, cx| {
                root.downcast::<RootView>()
                    .ok()
                    .map(|root| root.read(cx).saved_window(window, cx))
            })
            .ok()
            .flatten();
        if let Some(saved) = saved {
            windows.push((Some(handle) == active, saved));
        }
    }
    let mut store = runtime.store.write().expect("session store lock poisoned");
    if windows.is_empty() {
        // Every window was closed before quitting. The bounds observer's last
        // placement is all that should come back.
        store.remember_additional_windows(Vec::new());
        return;
    }
    let key = windows
        .iter()
        .position(|(active, _)| *active)
        .unwrap_or_default();
    let (_, key_window) = windows.remove(key);
    store.remember_window_placement(key_window.placement);
    store.remember_additional_windows(windows.into_iter().map(|(_, saved)| saved).collect());
}

/// Place a saved window on the displays attached right now.
fn restore_window_bounds(
    placement: &WindowPlacement,
    policy: RestorePolicy,
    cx: &App,
) -> (WindowBounds, Option<gpui::DisplayId>) {
    let displays = cx.displays();
    let primary = cx.primary_display().map(|display| display.id());
    let frames: Vec<DisplayFrame> = displays
        .iter()
        .map(|display| DisplayFrame {
            uuid: display.uuid().ok().map(|uuid| uuid.to_string()),
            visible: display.visible_bounds(),
            primary: Some(display.id()) == primary,
        })
        .collect();
    let restored = window_restore::resolve(placement, &frames, policy);
    (
        restored.bounds,
        restored.display.map(|index| displays[index].id()),
    )
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

#[cfg(test)]
mod usage_refresh_tests {
    use super::*;

    #[tokio::test]
    async fn cursor_regression_slow_fetch_does_not_block_local_updates() {
        let home = tempfile::tempdir().unwrap();
        let paths = usage::ScanPaths::for_home(home.path());
        std::fs::create_dir_all(&paths.roots[0].0).unwrap();
        // An empty local store still has a published timestamp and must finish
        // startup even while Cursor is waiting indefinitely on a response.
        let store = UsageStore::with_paths_and_clock(paths, usage::SystemClock);
        let (usage_tx, mut usage_rx) = tokio::sync::watch::channel(UsageSnapshot::default());
        let cursor = CursorRefresh::with_task(tokio::spawn(std::future::pending()));
        let worker = tokio::spawn(run_usage_updates(
            store,
            usage_tx,
            home.path().to_owned(),
            cursor,
        ));
        tokio::time::timeout(Duration::from_secs(5), usage_rx.changed())
            .await
            .unwrap()
            .unwrap();
        assert_ne!(usage_rx.borrow_and_update().updated_at, 0);
        // Advance the real reconciliation loop deterministically; this must
        // publish again while the same Cursor fetch remains pending.
        tokio::time::pause();
        tokio::time::advance(USAGE_RECONCILE_INTERVAL).await;
        tokio::time::resume();
        tokio::time::timeout(Duration::from_secs(5), usage_rx.changed())
            .await
            .unwrap()
            .unwrap();
        worker.abort();
        let _ = worker.await;
    }
}
