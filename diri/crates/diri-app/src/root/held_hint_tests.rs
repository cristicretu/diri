//! Hold-⌘ hints through RootView's real event path: platform modifier
//! changes, the keystroke interceptor, the executor's hold timer and window
//! activation.

use gpui::{Modifiers, TestAppContext, VisualTestContext};

use super::tests::test_services;
use super::*;
use crate::SidebarPreviewFixture;
use crate::held_hints::HOLD_DELAY;

fn command() -> Modifiers {
    Modifiers {
        platform: true,
        ..Modifiers::default()
    }
}

fn hint(cx: &mut VisualTestContext, name: String) -> bool {
    cx.debug_bounds(Box::leak(format!("held-hint:{name}").into_boxed_str()))
        .is_some()
}

/// A window on the Typical fixture with the sidebar showing, plus the id of
/// the session ⌘3 selects.
fn window(cx: &mut TestAppContext) -> (Entity<RootView>, &mut VisualTestContext, SessionId) {
    cx.update(|cx| {
        commands::bind_keys(cx, &Default::default());
        cx.set_reduce_motion(true);
    });
    let services = test_services();
    let fixture = SidebarPreviewFixture::make(PreviewScenario::Typical);
    let selected = fixture.selected_session_id.clone().unwrap();
    {
        let mut store = services.store.store.write().unwrap();
        store.hydrate(fixture.list);
        store.select(selected.clone());
        store
            .update_preferences(|prefs| prefs.sidebar_visible = true)
            .unwrap();
    }
    let (root, cx) = cx.add_window_view(move |window, cx| {
        RootView::new(services, false, PreviewScenario::Typical, window, cx)
    });
    cx.run_until_parked();
    // Which row ⌘3 lands on, read from the sidebar's own numbering.
    let third = root.update(cx, |root, cx| {
        root.sidebar
            .update(cx, |sidebar, cx| assert!(sidebar.select_shortcut(2, cx)));
        let mut store = root.window_store.write().unwrap();
        let third = store.selected_session_id().cloned().unwrap();
        store.select(selected.clone());
        third
    });
    cx.run_until_parked();
    (root, cx, third)
}

fn hold(cx: &mut VisualTestContext) {
    cx.simulate_modifiers_change(command());
    cx.executor().advance_clock(HOLD_DELAY);
    cx.run_until_parked();
}

#[gpui::test]
fn holding_command_labels_the_visible_controls_and_three_selects(cx: &mut TestAppContext) {
    let (root, cx, third) = window(cx);
    assert!(!hint(cx, format!("session:{}", third.0)), "nothing at rest");

    cx.simulate_modifiers_change(command());
    cx.executor().advance_clock(HOLD_DELAY / 2);
    cx.run_until_parked();
    assert!(
        !hint(cx, format!("session:{}", third.0)),
        "not before the delay"
    );
    cx.executor().advance_clock(HOLD_DELAY / 2);
    cx.run_until_parked();
    assert!(hint(cx, format!("session:{}", third.0)));
    assert!(hint(cx, "sidebar-toggle".into()));
    assert!(hint(cx, "new-agent".into()));
    assert!(hint(cx, "toggle-inspector".into()));
    // The sidebar is open, so its reveal control in the header is not drawn
    // and neither is its hint.
    assert!(!hint(cx, "show-sidebar".into()));

    cx.simulate_keystrokes(&commands::test_chords("cmd-3"));
    cx.run_until_parked();
    let selected = root.read_with(cx, |root, _| {
        root.window_store
            .read()
            .unwrap()
            .selected_session_id()
            .cloned()
    });
    assert_eq!(selected, Some(third.clone()));
    assert!(
        !hint(cx, format!("session:{}", third.0)),
        "the chord dismissed them"
    );
    // Still holding ⌘ after the chord does not bring them back.
    cx.executor().advance_clock(HOLD_DELAY * 2);
    cx.run_until_parked();
    assert!(!hint(cx, "sidebar-toggle".into()));
}

#[gpui::test]
fn a_quick_chord_never_shows_hints(cx: &mut TestAppContext) {
    let (_root, cx, third) = window(cx);
    cx.simulate_modifiers_change(command());
    cx.executor().advance_clock(HOLD_DELAY / 5);
    cx.simulate_keystrokes(&commands::test_chords("cmd-1"));
    cx.executor().advance_clock(HOLD_DELAY * 2);
    cx.run_until_parked();
    assert!(!hint(cx, format!("session:{}", third.0)));
    assert!(!hint(cx, "sidebar-toggle".into()));
}

#[gpui::test]
fn release_and_deactivation_hide_the_hints(cx: &mut TestAppContext) {
    let (_root, cx, _third) = window(cx);
    cx.update(|window, _| window.activate_window());
    cx.run_until_parked();
    assert!(cx.update(|window, _| window.is_window_active()));
    hold(cx);
    assert!(hint(cx, "sidebar-toggle".into()));
    cx.simulate_modifiers_change(Modifiers::default());
    cx.run_until_parked();
    assert!(!hint(cx, "sidebar-toggle".into()), "release hides them");

    hold(cx);
    assert!(hint(cx, "sidebar-toggle".into()));
    cx.deactivate_window();
    cx.run_until_parked();
    assert!(
        !hint(cx, "sidebar-toggle".into()),
        "losing key status hides them"
    );
}

#[gpui::test]
fn hints_follow_what_is_on_screen(cx: &mut TestAppContext) {
    let (_root, cx, third) = window(cx);
    cx.simulate_keystrokes(&commands::test_chords("cmd-b"));
    cx.run_until_parked();
    // Keys pressed while ⌘ was not yet down never count against a hold.
    hold(cx);
    assert!(
        !hint(cx, format!("session:{}", third.0)),
        "hidden rows stay quiet"
    );
    assert!(!hint(cx, "sidebar-toggle".into()));
    assert!(
        hint(cx, "show-sidebar".into()),
        "the header's reveal control speaks up"
    );
}
