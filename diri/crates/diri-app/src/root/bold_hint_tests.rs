//! ⌘B in a note bolds instead of toggling the sidebar: the hint that says
//! how to reach the sidebar from a note, once per launch, until "Don't
//! show again".

use gpui::TestAppContext;

use super::tests::test_services;
use super::*;

fn hint_showing(root: &RootView) -> bool {
    root.toast
        .current()
        .is_some_and(|toast| toast.message == crate::i18n::t("toast.bold_hint"))
}

#[gpui::test]
fn the_bold_hint_shows_once_and_dont_show_again_keeps_it_away(cx: &mut TestAppContext) {
    let services = test_services();
    let (root, cx) = cx.add_window_view(move |window, cx| {
        RootView::new(services, false, PreviewScenario::Empty, window, cx)
    });
    cx.run_until_parked();

    root.update(cx, |root, cx| root.bold_shortcut_used(cx));
    let action = root.read_with(cx, |root, _| {
        assert!(hint_showing(root), "the first ⌘B in a note explains itself");
        let toast = root.toast.current().unwrap();
        assert_eq!(
            toast.detail.as_deref(),
            Some(crate::i18n::t("toast.bold_hint_detail"))
        );
        toast.actions[0].clone()
    });
    assert_eq!(action.command, ToastCommand::DismissBoldHint);

    // Once per launch: the next ⌘B says nothing.
    root.update(cx, |root, _| root.toast.dismiss());
    root.update(cx, |root, cx| root.bold_shortcut_used(cx));
    root.read_with(cx, |root, _| assert!(!hint_showing(root)));

    // "Don't show again" is remembered past this launch.
    root.update(cx, |root, _| root.bold_hint_shown = false);
    root.update(cx, |root, cx| root.bold_shortcut_used(cx));
    root.update_in(cx, |root, window, cx| {
        root.run_toast_command(ToastCommand::DismissBoldHint, window, cx)
    });
    root.read_with(cx, |root, _| {
        assert!(!hint_showing(root), "the button closes the hint");
        assert!(
            root.window_store
                .read()
                .unwrap()
                .preferences()
                .bold_hint_dismissed
        );
    });
    root.update(cx, |root, _| root.bold_hint_shown = false);
    root.update(cx, |root, cx| root.bold_shortcut_used(cx));
    root.read_with(cx, |root, _| assert!(!hint_showing(root)));
}
