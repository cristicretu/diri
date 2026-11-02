//! What's New end to end in the real window: the footer line after an
//! update, the sheet it opens, and what is remembered.

use gpui::{TestAppContext, VisualTestContext};

use super::tests::test_services;
use super::*;

/// The release the catalog's newest highlights ship in.
fn newest() -> &'static str {
    crate::whats_new::RELEASES[0].version
}

/// The Notes release: three highlights, each with a Try it command.
const NOTES_RELEASE: &str = "0.9.0";

fn open<'a>(
    cx: &'a mut TestAppContext,
    seen: &str,
) -> (Entity<RootView>, &'a mut VisualTestContext) {
    open_running(cx, seen, newest())
}

/// [`open`] in a build running `current`.
fn open_running<'a>(
    cx: &'a mut TestAppContext,
    seen: &str,
    current: &str,
) -> (Entity<RootView>, &'a mut VisualTestContext) {
    crate::whats_new::set_current_version_for_test(Some(current));
    let services = test_services();
    services
        .store
        .store
        .write()
        .unwrap()
        .update_preferences(|prefs| {
            prefs.sidebar_visible = true;
            prefs.whats_new_seen_version = seen.to_owned();
        })
        .unwrap();
    let (root, cx) = cx.add_window_view(move |window, cx| {
        RootView::new(services, false, PreviewScenario::Empty, window, cx)
    });
    cx.run_until_parked();
    (root, cx)
}

fn seen(root: &Entity<RootView>, cx: &mut VisualTestContext) -> String {
    root.read_with(cx, |root, _| {
        root.services
            .store
            .store
            .read()
            .unwrap()
            .preferences()
            .whats_new_seen_version
            .clone()
    })
}

#[gpui::test]
fn an_update_shows_one_line_and_the_sheet_only_when_asked(cx: &mut TestAppContext) {
    let (root, cx) = open(cx, "0.0.1");
    // The footer line, and nothing over the work.
    assert!(cx.debug_bounds("whats-new-pill").is_some());
    root.read_with(cx, |root, _| assert!(root.whats_new.is_none()));

    let pill = cx.debug_bounds("whats-new-pill").unwrap();
    cx.simulate_click(pill.center(), gpui::Modifiers::none());
    cx.run_until_parked();
    let sheet = root.read_with(cx, |root, _| root.whats_new.clone().expect("sheet open"));
    assert_eq!(seen(&root, cx), newest(), "opening marks the release seen");
    assert!(
        cx.debug_bounds("whats-new-pill").is_none(),
        "the line goes away"
    );

    // Arrows page through; Escape closes.
    let pages = sheet.read_with(cx, |sheet, _| sheet.page_count());
    assert!(pages > 1);
    cx.simulate_keystrokes("right");
    assert_eq!(sheet.read_with(cx, |sheet, _| sheet.page()), 1);
    cx.simulate_keystrokes("left");
    assert_eq!(sheet.read_with(cx, |sheet, _| sheet.page()), 0);
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    root.read_with(cx, |root, _| assert!(root.whats_new.is_none()));
    crate::whats_new::set_current_version_for_test(None);
}

#[gpui::test]
fn dismissing_the_line_never_opens_the_sheet(cx: &mut TestAppContext) {
    let (root, cx) = open(cx, "0.0.1");
    let dismiss = cx.debug_bounds("whats-new-dismiss").unwrap();
    cx.simulate_click(dismiss.center(), gpui::Modifiers::none());
    cx.run_until_parked();
    root.read_with(cx, |root, _| assert!(root.whats_new.is_none()));
    assert_eq!(seen(&root, cx), newest());
    assert!(cx.debug_bounds("whats-new-pill").is_none());
    crate::whats_new::set_current_version_for_test(None);
}

#[gpui::test]
fn a_seen_release_and_a_new_install_show_nothing(cx: &mut TestAppContext) {
    let (_, cx) = open(cx, newest());
    assert!(cx.debug_bounds("whats-new-pill").is_none());
    crate::whats_new::set_current_version_for_test(None);
    // A fresh install's preferences start at the running version.
    assert_eq!(
        crate::store::Prefs::default().whats_new_seen_version,
        crate::updates::CURRENT_VERSION
    );
}

#[gpui::test]
fn try_it_closes_the_sheet_and_runs_the_command(cx: &mut TestAppContext) {
    let (root, cx) = open_running(cx, "0.0.1", NOTES_RELEASE);
    root.update_in(cx, |root, window, cx| root.open_whats_new(window, cx));
    cx.run_until_parked();
    let try_it = cx.debug_bounds("whats-new-try").expect("Try it");
    cx.simulate_click(try_it.center(), gpui::Modifiers::none());
    cx.run_until_parked();
    root.read_with(cx, |root, _| assert!(root.whats_new.is_none()));
    crate::whats_new::set_current_version_for_test(None);
}

#[gpui::test]
fn the_command_replays_the_highlights_after_they_were_seen(cx: &mut TestAppContext) {
    let (root, cx) = open(cx, newest());
    assert!(cx.debug_bounds("whats-new-pill").is_none());
    root.update_in(cx, |root, window, cx| {
        root.run_command(CommandId::ShowWhatsNew, window, cx)
    });
    cx.run_until_parked();
    let sheet = root.read_with(cx, |root, _| root.whats_new.clone().expect("sheet open"));
    assert_eq!(sheet.read_with(cx, |sheet, _| sheet.page()), 0);
    crate::whats_new::set_current_version_for_test(None);
}

#[gpui::test]
fn a_settings_thumbnail_opens_the_sheet_on_its_highlight(cx: &mut TestAppContext) {
    let (root, cx) = open_running(cx, NOTES_RELEASE, NOTES_RELEASE);
    let surfaces = root.read_with(cx, |root, _| root.utility_surfaces.clone().unwrap());
    surfaces.update(cx, |_, cx| {
        cx.emit(crate::surface_shell::UtilitySurfacesEvent::ShowWhatsNew(2))
    });
    cx.run_until_parked();
    let sheet = root.read_with(cx, |root, _| root.whats_new.clone().expect("sheet open"));
    assert_eq!(sheet.read_with(cx, |sheet, _| sheet.page()), 2);
    crate::whats_new::set_current_version_for_test(None);
}
