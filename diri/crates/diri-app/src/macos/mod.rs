pub mod brand_raster;
pub mod browser;
pub(crate) mod floating_panel;
pub mod menu_bar;
pub mod notifier;
pub(crate) mod terminal_keys;

use objc2_foundation::NSBundle;

pub(crate) fn bundle_identifier() -> Option<String> {
    NSBundle::mainBundle()
        .bundleIdentifier()
        .map(|identifier| identifier.to_string())
}

#[cfg(test)]
pub(crate) mod tab_gesture;

/// Mirrors System Settings > Appearance > "Show scroll bars" into every
/// scroll area, now and whenever the user changes it.
pub(crate) fn observe_scroller_style(cx: &mut gpui::App) {
    use diri_ui::ScrollerStyle;
    use objc2::MainThreadMarker;
    use objc2_app_kit::{
        NSPreferredScrollerStyleDidChangeNotification, NSScroller, NSScrollerStyle,
    };
    use objc2_foundation::{NSNotification, NSNotificationCenter, NSOperationQueue};

    fn read(mtm: MainThreadMarker) -> ScrollerStyle {
        if NSScroller::preferredScrollerStyle(mtm) == NSScrollerStyle::Legacy {
            ScrollerStyle::Legacy
        } else {
            ScrollerStyle::Overlay
        }
    }

    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    diri_ui::set_scroller_style(cx, read(mtm));

    let (changed, mut changes) = tokio::sync::mpsc::channel::<()>(1);
    let handler = block2::RcBlock::new(move |_: std::ptr::NonNull<NSNotification>| {
        let _ = changed.try_send(());
    });
    // SAFETY: the block captures only a channel sender, and the main queue
    // delivers it on the thread AppKit already runs the app on.
    let observer = unsafe {
        NSNotificationCenter::defaultCenter().addObserverForName_object_queue_usingBlock(
            Some(NSPreferredScrollerStyleDidChangeNotification),
            None,
            Some(&NSOperationQueue::mainQueue()),
            &handler,
        )
    };
    // The observation lasts as long as the process.
    std::mem::forget(observer);

    cx.spawn(async move |cx| {
        while changes.recv().await.is_some() {
            let style = read(mtm);
            cx.update(|cx| {
                diri_ui::set_scroller_style(cx, style);
            });
        }
    })
    .detach();
}

/// Whether the user asked macOS to reduce motion.
fn system_reduce_motion() -> bool {
    use objc2_app_kit::NSWorkspace;
    let workspace = NSWorkspace::sharedWorkspace();
    // SAFETY: a BOOL property getter on NSWorkspace, available since
    // macOS 10.12. Sent by hand because the typed accessor sits behind a
    // crate feature nothing else here needs.
    unsafe { objc2::msg_send![&*workspace, accessibilityDisplayShouldReduceMotion] }
}

/// Follows System Settings → Accessibility → Reduce Motion, now and whenever
/// it changes. Every animation in the app already asks `cx.reduce_motion()`,
/// but nothing ever set it outside the tests, so the system setting did
/// nothing here.
pub(crate) fn observe_reduce_motion(cx: &mut gpui::App) {
    use objc2::MainThreadMarker;
    use objc2_app_kit::NSWorkspace;
    use objc2_foundation::{NSNotification, NSOperationQueue, NSString};

    if MainThreadMarker::new().is_none() {
        return;
    }
    cx.set_reduce_motion(system_reduce_motion());

    let (changed, mut changes) = tokio::sync::mpsc::channel::<()>(1);
    let handler = block2::RcBlock::new(move |_: std::ptr::NonNull<NSNotification>| {
        let _ = changed.try_send(());
    });
    // The name is the constant's own spelling; AppKit posts it on the
    // workspace's centre, not the default one.
    let name = NSString::from_str("NSWorkspaceAccessibilityDisplayOptionsDidChangeNotification");
    // SAFETY: the block captures only a channel sender, and the main queue
    // delivers it on the thread AppKit already runs the app on.
    let observer = unsafe {
        NSWorkspace::sharedWorkspace()
            .notificationCenter()
            .addObserverForName_object_queue_usingBlock(
                Some(&name),
                None,
                Some(&NSOperationQueue::mainQueue()),
                &handler,
            )
    };
    // The observation lasts as long as the process.
    std::mem::forget(observer);

    cx.spawn(async move |cx| {
        while changes.recv().await.is_some() {
            let reduce = system_reduce_motion();
            cx.update(|cx| cx.set_reduce_motion(reduce));
        }
    })
    .detach();
}

/// Plays one haptic pattern on the trackpad, now. AppKit respects the
/// current trackpad and the user's haptic preferences, and does nothing when
/// the user is not touching a Force Touch trackpad. Go through
/// `crate::haptics`, which decides when a moment deserves one.
#[cfg_attr(test, allow(dead_code))]
pub(crate) fn perform_haptic(pattern: crate::haptics::Pattern) {
    use crate::haptics::Pattern;
    use objc2_app_kit::{
        NSHapticFeedbackManager, NSHapticFeedbackPattern, NSHapticFeedbackPerformanceTime,
        NSHapticFeedbackPerformer,
    };
    let pattern = match pattern {
        Pattern::Alignment => NSHapticFeedbackPattern::Alignment,
        Pattern::LevelChange => NSHapticFeedbackPattern::LevelChange,
        Pattern::Generic => NSHapticFeedbackPattern::Generic,
    };
    NSHapticFeedbackManager::defaultPerformer()
        .performFeedbackPattern_performanceTime(pattern, NSHapticFeedbackPerformanceTime::Now);
}

#[cfg(test)]
mod reduce_motion_tests {
    /// The getter is sent by hand, so a misspelt selector would only show up
    /// as an abort at launch. This machine's setting is whatever it is; the
    /// point is that the message is understood and answers a BOOL.
    #[test]
    fn the_system_setting_is_readable() {
        let first = super::system_reduce_motion();
        assert_eq!(first, super::system_reduce_motion());
    }
}
