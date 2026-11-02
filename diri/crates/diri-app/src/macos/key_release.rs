//! Tells when a held ⌘-chord is let go.
//!
//! AppKit does not send the key window a key-up for a key released while ⌘
//! is down, so GPUI never sees the end of a held ⌘T. A local monitor sees
//! every event the app dequeues, key-ups included. It exists only while a
//! press is being watched and passes every event through untouched.

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_app_kit::{NSEvent, NSEventMask, NSEventModifierFlags, NSEventType};
use tokio::sync::mpsc::{self, Receiver};

/// Watches one physical key; dropping it removes the monitor.
pub(crate) struct ReleaseWatch {
    monitor: Option<Retained<AnyObject>>,
}

/// Starts watching `key_code` (or ⌘) for release. Each release (the key up,
/// or ⌘ no longer held) is one message on the receiver.
pub(crate) fn watch(key_code: Option<u16>) -> Option<(ReleaseWatch, Receiver<()>)> {
    let (sender, receiver) = mpsc::channel(4);
    let handler = block2::RcBlock::new(move |event: std::ptr::NonNull<NSEvent>| {
        let native = unsafe { event.as_ref() };
        let released = match native.r#type() {
            NSEventType::KeyUp => key_code.is_none_or(|code| native.keyCode() == code),
            NSEventType::FlagsChanged => !native
                .modifierFlags()
                .contains(NSEventModifierFlags::Command),
            _ => false,
        };
        if released {
            let _ = sender.try_send(());
        }
        event.as_ptr()
    });
    // SAFETY: AppKit calls the block on the main thread; it only reads the
    // event and sends on a bounded channel.
    let monitor = unsafe {
        NSEvent::addLocalMonitorForEventsMatchingMask_handler(
            NSEventMask::KeyUp | NSEventMask::FlagsChanged,
            &handler,
        )
    }?;
    Some((
        ReleaseWatch {
            monitor: Some(monitor),
        },
        receiver,
    ))
}

impl Drop for ReleaseWatch {
    fn drop(&mut self) {
        if let Some(monitor) = self.monitor.take() {
            // SAFETY: this token came from addLocalMonitor on this thread.
            unsafe { NSEvent::removeMonitor(&monitor) };
        }
    }
}
