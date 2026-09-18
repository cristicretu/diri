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

/// AppKit respects the current trackpad and the user's haptic preferences.
pub(crate) fn pinch_feedback() {
    use objc2_app_kit::{
        NSHapticFeedbackManager, NSHapticFeedbackPattern, NSHapticFeedbackPerformanceTime,
        NSHapticFeedbackPerformer,
    };
    NSHapticFeedbackManager::defaultPerformer().performFeedbackPattern_performanceTime(
        NSHapticFeedbackPattern::Alignment,
        NSHapticFeedbackPerformanceTime::Now,
    );
}
