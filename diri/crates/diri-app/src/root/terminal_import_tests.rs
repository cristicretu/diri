//! Importing another terminal's setup in the real window: the sheet the
//! command and the first-run welcome open, Import applying everything at
//! once, Undo putting it all back, and Not Now changing nothing.

use gpui::{TestAppContext, VisualTestContext};

use super::tests::test_services;
use super::*;
use crate::store::TerminalCursorShape;

struct Opened<'a> {
    root: Entity<RootView>,
    cx: &'a mut VisualTestContext,
    _themes: tempfile::TempDir,
}

fn open<'a>(cx: &'a mut TestAppContext, fixture: &str) -> Opened<'a> {
    let themes = tempfile::tempdir().unwrap();
    crate::imported_themes::set_test_dir(Some(themes.path().join("themes")));
    let services = test_services();
    services
        .store
        .store
        .write()
        .unwrap()
        .set_terminal_setup_scan(crate::terminal_import::fixture_scan(fixture));
    let (root, cx) = cx.add_window_view(move |window, cx| {
        RootView::new(services, false, PreviewScenario::Empty, window, cx)
    });
    cx.run_until_parked();
    Opened {
        root,
        cx,
        _themes: themes,
    }
}

fn prefs(root: &Entity<RootView>, cx: &mut VisualTestContext) -> crate::store::Prefs {
    root.read_with(cx, |root, _| {
        root.services.store.store.read().unwrap().preferences().clone()
    })
}

fn sheet_open(root: &Entity<RootView>, cx: &mut VisualTestContext) -> bool {
    root.read_with(cx, |root, _| root.terminal_import.is_some())
}

#[gpui::test]
fn import_applies_every_change_at_once_and_undo_restores_them(cx: &mut TestAppContext) {
    let Opened { root, cx, _themes } = open(cx, "ghostty");
    let before = prefs(&root, cx);
    root.update_in(cx, |root, window, cx| {
        root.run_command(CommandId::ImportTerminalSetup, window, cx);
    });
    cx.run_until_parked();
    assert!(sheet_open(&root, cx));
    assert!(cx.debug_bounds("terminal-import-sheet").is_some());

    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert!(!sheet_open(&root, cx), "Import closes the sheet");
    let after = prefs(&root, cx);
    assert_eq!(after.terminal_font_size, 15.0);
    assert_eq!(after.terminal_line_height, 1.2);
    assert_eq!(after.terminal_cursor_shape, TerminalCursorShape::Bar);
    assert!(!after.terminal_cursor_blink);
    assert!(after.terminal_copy_on_select);
    assert_eq!(after.terminal_theme, "imported-night-owl-custom");
    let toast = root.read_with(cx, |root, _| root.toast.current().cloned());
    let toast = toast.expect("a confirmation");
    assert!(
        toast
            .actions
            .iter()
            .any(|action| action.command == ToastCommand::UndoTerminalImport)
    );

    root.update_in(cx, |root, window, cx| {
        root.run_toast_command(ToastCommand::UndoTerminalImport, window, cx);
    });
    cx.run_until_parked();
    assert_eq!(prefs(&root, cx), before);
    assert!(diri_term::themes::find("imported-night-owl-custom").is_none());
    crate::imported_themes::set_test_dir(None);
}

#[gpui::test]
fn not_now_changes_nothing(cx: &mut TestAppContext) {
    let Opened { root, cx, _themes } = open(cx, "kitty");
    let before = prefs(&root, cx);
    root.update_in(cx, |root, window, cx| {
        root.run_command(CommandId::ImportTerminalSetup, window, cx);
    });
    cx.run_until_parked();
    assert!(sheet_open(&root, cx));
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert!(!sheet_open(&root, cx));
    assert_eq!(prefs(&root, cx), before);
    crate::imported_themes::set_test_dir(None);
}

#[gpui::test]
fn the_first_run_welcome_offers_the_import(cx: &mut TestAppContext) {
    let Opened { root, cx, _themes } = open(cx, "ghostty");
    let link = cx
        .debug_bounds("empty-import-terminal-setup")
        .expect("the welcome names Ghostty");
    cx.simulate_click(link.center(), gpui::Modifiers::none());
    cx.run_until_parked();
    assert!(sheet_open(&root, cx));
    crate::imported_themes::set_test_dir(None);
}

#[gpui::test]
fn with_nothing_to_import_the_command_says_so(cx: &mut TestAppContext) {
    let Opened { root, cx, _themes } = open(cx, "terminal-stock");
    root.update_in(cx, |root, window, cx| {
        root.run_command(CommandId::ImportTerminalSetup, window, cx);
    });
    cx.run_until_parked();
    assert!(!sheet_open(&root, cx));
    assert!(root.read_with(cx, |root, _| root.toast.current().is_some()));
    assert!(cx.debug_bounds("empty-import-terminal-setup").is_none());
    crate::imported_themes::set_test_dir(None);
}

/// The sheet over the real window, for the PR and design review.
/// `DIRI_VISUAL_TERMINAL_SETUP=ghostty|kitty|iterm|...` picks the fixture,
/// `DIRI_VISUAL_LIGHT=1` the light theme.
#[cfg(target_os = "macos")]
#[test]
#[ignore = "writes the import sheet to DIRI_VISUAL_OUTPUT"]
fn render_terminal_import_sheet_screenshot() {
    use gpui::HeadlessAppContext;
    let output = std::env::var("DIRI_VISUAL_OUTPUT").expect("output path");
    let fixture = std::env::var("DIRI_VISUAL_TERMINAL_SETUP").unwrap_or_else(|_| "ghostty".into());
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
    let window_height: f32 = std::env::var("DIRI_VISUAL_HEIGHT")
        .ok()
        .and_then(|height| height.parse().ok())
        .unwrap_or(700.0);
    let themes = tempfile::tempdir().unwrap();
    crate::imported_themes::set_test_dir(Some(themes.path().join("themes")));
    let services = test_services();
    {
        let mut store = services.store.store.write().unwrap();
        // Headless capture has no backdrop to blur, so glass would show the
        // window's content through the sheet; render the solid style.
        store
            .update_preferences(|prefs| {
                prefs.set_interface_style(crate::store::InterfaceStyle::System);
                if std::env::var_os("DIRI_VISUAL_LIGHT").is_some() {
                    prefs.terminal_theme = "dirijor-light".into();
                }
            })
            .unwrap();
        let mut found = Vec::new();
        for name in fixture.split(',') {
            found.extend(crate::terminal_import::fixture_scan(name));
        }
        store.set_terminal_setup_scan(found);
    }
    let window = cx
        .open_window(gpui::size(px(1000.0), px(window_height)), |window, cx| {
            cx.new(|cx| {
                let mut root = RootView::new(services, false, PreviewScenario::Empty, window, cx);
                root.preview = true;
                root.open_terminal_import(window, cx);
                root
            })
        })
        .unwrap();
    cx.run_until_parked();
    cx.capture_screenshot(window.into())
        .unwrap()
        .save(output)
        .unwrap();
    cx.update_window(window.into(), |_, window, _| window.remove_window())
        .unwrap();
    cx.run_until_parked();
    crate::imported_themes::set_test_dir(None);
}
