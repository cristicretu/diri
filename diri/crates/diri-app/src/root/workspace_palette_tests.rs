use super::*;
use crate::palette::PaletteCommand;
use crate::palette_workspace::WorkspaceCommand;
use gpui::{HeadlessAppContext, size};

#[test]
#[ignore = "native palette workflow with disposable Engine, PTYs, durable catalog, and screenshots"]
fn workspace_palette_targets_its_window_and_restores_durable_names() {
    let fixture = crate::workspace_fixture::LiveWorkspace::start();
    fixture
        .services
        .store
        .store
        .write()
        .unwrap()
        .update_preferences(|prefs| {
            prefs.terminal_theme = if std::env::var_os("DIRI_VISUAL_LIGHT").is_some() {
                "dirijor-light"
            } else {
                "dirijor"
            }
            .into()
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
        commands::bind_keys(cx, &Default::default());
    });
    let services = fixture.services.clone();
    let first = cx
        .open_window(size(px(1100.0), px(720.0)), |window, cx| {
            cx.new(|cx| RootView::new(services, false, PreviewScenario::Empty, window, cx))
        })
        .unwrap();
    let services = fixture.services.clone();
    let second = cx
        .open_window(size(px(820.0), px(720.0)), |window, cx| {
            cx.new(|cx| {
                RootView::new_with_selection(
                    services,
                    false,
                    PreviewScenario::Empty,
                    Some(None),
                    Some(Some(SessionId::new("review"))),
                    window,
                    cx,
                )
            })
        })
        .unwrap();
    macro_rules! update {
        ($handle:expr,$body:expr) => {
            cx.update_window($handle.into(), |root, window, cx| {
                root.downcast::<RootView>()
                    .unwrap()
                    .update(cx, |root, cx| ($body)(root, window, cx))
            })
            .unwrap()
        };
    }
    macro_rules! settle {
        () => {
            for _ in 0..30 {
                cx.run_until_parked();
                std::thread::sleep(Duration::from_millis(5));
            }
        };
    }
    macro_rules! rows {
        ($handle:expr,$query:expr) => {
            update!($handle, |root: &mut RootView,
                              window: &mut Window,
                              cx: &mut Context<RootView>| {
                root.navigation.as_ref().unwrap().update(cx, |nav, cx| {
                    nav.workspace_palette_for_test($query, window, cx)
                })
            })
        };
    }
    macro_rules! invoke {
        ($handle:expr,$command:expr) => {
            update!($handle, |root: &mut RootView,
                              window: &mut Window,
                              cx: &mut Context<RootView>| root
                .navigation
                .as_ref()
                .unwrap()
                .update(cx, |nav, cx| nav
                    .invoke_workspace_palette_for_test($command, window, cx)));
            settle!();
        };
    }
    macro_rules! keys {
        ($handle:expr,$keys:expr) => {
            for key in $keys.split_whitespace() {
                cx.update_window($handle.into(), |_, window, cx| {
                    window.dispatch_keystroke(gpui::Keystroke::parse(key).unwrap(), cx);
                })
                .unwrap();
                cx.run_until_parked();
            }
        };
    }
    macro_rules! wait {
        ($condition:expr) => {{
            let deadline = Instant::now() + Duration::from_secs(10);
            while !$condition {
                assert!(Instant::now() < deadline, "workspace mutation deadline");
                settle!();
            }
        }};
    }
    let snapshot = || {
        fixture
            .services
            .tokio
            .block_on(fixture.services.store.client().workspaces())
            .unwrap()
    };
    settle!();
    let original_tab = snapshot().workspaces[0].tabs[0].id.clone();
    let original_panes = snapshot().workspaces[0].tabs[0].layout.clone();
    let rename = rows!(first, "rename")
        .into_iter()
        .find(|row| row.id == "rename-selected-tab")
        .unwrap();
    assert_eq!(rename.title, "Rename Tab");
    assert_eq!(
        rename.command,
        PaletteCommand::Workspace(WorkspaceCommand::RenameTab(original_tab.clone()))
    );
    // Capture the command in A, then focus B before invoking it in A.
    cx.update_window(second.into(), |_, window, _| window.activate_window())
        .unwrap();
    invoke!(first, rename.command);
    keys!(first, "cmd-a r e n a m e d enter");
    wait!(snapshot().workspaces[0].tabs[0].title.as_deref() == Some("renamed"));
    update!(second, |root: &mut RootView,
                     _,
                     _: &mut Context<RootView>| {
        assert!(root.active_workspace.is_none());
        assert_eq!(root.window_session(), Some(SessionId::new("review")));
    });
    let rename = rows!(first, "workspace")
        .into_iter()
        .find(|row| row.id == "rename-workspace")
        .unwrap();
    invoke!(first, rename.command);
    keys!(first, "cmd-a d e l i v e r y enter");
    wait!(snapshot().workspaces[0].name == "delivery");
    assert_eq!(snapshot().workspaces[0].tabs[0].layout, original_panes);
    assert_eq!(
        diri_engine::workspace::WorkspaceStore::new(fixture.directory.path().join("state.json"))
            .snapshot()
            .unwrap(),
        snapshot()
    );
    fixture.verify_process_identity();
    cx.update_window(first.into(), |_, window, _| window.remove_window())
        .unwrap();
    settle!();
    let services = fixture.services.clone();
    let reopened = cx
        .open_window(size(px(1100.0), px(720.0)), |window, cx| {
            cx.new(|cx| RootView::new(services, false, PreviewScenario::Empty, window, cx))
        })
        .unwrap();
    settle!();
    update!(reopened, |root: &mut RootView,
                       _,
                       _: &mut Context<RootView>| assert_eq!(
        root.active_workspace,
        Some(fixture.workspace.clone())
    ));
    let workspace_rows = rows!(reopened, "workspace");
    assert!(
        workspace_rows.iter().any(
            |row| row.title == "Switch to delivery" && row.detail.as_deref() == Some("Current")
        )
    );
    let save = |cx: &mut HeadlessAppContext, window: gpui::AnyWindowHandle, name: &str| {
        if let Some(directory) = std::env::var_os("DIRI_WORKSPACE_PALETTE_SCREENSHOTS") {
            let directory = std::path::PathBuf::from(directory);
            std::fs::create_dir_all(&directory).unwrap();
            cx.capture_screenshot(window)
                .unwrap()
                .save(directory.join(name))
                .unwrap();
        }
    };
    cx.run_until_parked();
    save(&mut cx, reopened.into(), "workspace.png");
    let switch_rows = rows!(reopened, "switch");
    assert!(
        switch_rows.iter().any(
            |row| row.title == "Switch to delivery" && row.detail.as_deref() == Some("Current")
        )
    );
    cx.run_until_parked();
    save(&mut cx, reopened.into(), "switch.png");
    let tab_rows = rows!(reopened, "rename tab");
    assert!(tab_rows.iter().any(|row| row.title == "Rename Tab"
        && row.detail.as_deref() == Some("renamed")
        && row.shortcut.is_some()));
    cx.run_until_parked();
    save(&mut cx, reopened.into(), "tab.png");
    let session_rows = rows!(second, "rename session");
    assert!(session_rows.iter().any(|row| row.command
        == PaletteCommand::Workspace(WorkspaceCommand::RenameSession(SessionId::new("review")))));
    cx.run_until_parked();
    save(&mut cx, second.into(), "session.png");
    let create = rows!(reopened, "new workspace")
        .into_iter()
        .find(|row| row.id == "new-workspace")
        .unwrap();
    invoke!(reopened, create.command);
    keys!(reopened, "r e s e a r c h enter");
    wait!(snapshot().workspaces.len() == 2);
    let created = snapshot().workspaces[1].id.clone();
    wait!(update!(
        reopened,
        |root: &mut RootView, _, _: &mut Context<RootView>| root.active_workspace
            == Some(created.clone())
    ));
    update!(second, |root: &mut RootView,
                     _,
                     _: &mut Context<RootView>| assert!(
        root.active_workspace.is_none()
    ));
    invoke!(
        reopened,
        PaletteCommand::Workspace(WorkspaceCommand::Switch(Some(fixture.workspace.clone())))
    );
    update!(reopened, |root: &mut RootView,
                       _,
                       _: &mut Context<RootView>| assert_eq!(
        root.active_workspace,
        Some(fixture.workspace.clone())
    ));
    // The chooser exposed by the palette supports filtering and Return.
    invoke!(
        reopened,
        PaletteCommand::Workspace(WorkspaceCommand::Browse)
    );
    keys!(reopened, "r e s e a r c h enter");
    wait!(update!(
        reopened,
        |root: &mut RootView, _, _: &mut Context<RootView>| root.active_workspace
            == Some(created.clone())
    ));
    fixture.verify_process_identity();
    cx.update_window(reopened.into(), |_, window, _| window.remove_window())
        .unwrap();
    cx.update_window(second.into(), |_, window, _| window.remove_window())
        .unwrap();
    cx.run_until_parked();
    fixture.verify_process_identity();
}
