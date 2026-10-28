//! Physical identity scoped to synchronous native key dispatch.
//!
//! GPUI's logical Keystroke remains unchanged for shortcuts and text/IME.
//! Consumers must query during their matching callback; saved/replayed events
//! outside that scope deliberately have no physical identity.

use gpui::{KeyDownEvent, PlatformInput};
use std::cell::RefCell;

struct PhysicalKey {
    code: u16,
    modifier_flags: u64,
}

thread_local! {
    static CURRENT: RefCell<Option<(KeyDownEvent, u16)>> = const { RefCell::new(None) };
    static PHYSICAL: RefCell<Option<PhysicalKey>> = const { RefCell::new(None) };
}

pub(crate) struct NativeKeyDispatch {
    previous_event: Option<(KeyDownEvent, u16)>,
    previous_physical: Option<PhysicalKey>,
}

impl NativeKeyDispatch {
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn enter(event: &PlatformInput, key_code: u16) -> Self {
        Self::enter_with_flags(event, key_code, 0)
    }

    /// `modifier_flags` is the raw `NSEvent.modifierFlags` bitfield, including
    /// the device-dependent left/right bits GPUI's logical modifiers omit.
    pub(crate) fn enter_with_flags(
        event: &PlatformInput,
        key_code: u16,
        modifier_flags: u64,
    ) -> Self {
        let current = match event {
            PlatformInput::KeyDown(event) => Some((event.clone(), key_code)),
            _ => None,
        };
        Self {
            previous_event: CURRENT.with(|state| state.replace(current)),
            previous_physical: PHYSICAL.with(|state| {
                state.replace(Some(PhysicalKey {
                    code: key_code,
                    modifier_flags,
                }))
            }),
        }
    }
}

impl Drop for NativeKeyDispatch {
    fn drop(&mut self) {
        CURRENT.with(|state| state.replace(self.previous_event.take()));
        PHYSICAL.with(|state| state.replace(self.previous_physical.take()));
    }
}

/// The hardware key code for the matching, currently dispatched native event.
///
/// This is callback-scoped metadata, not an identity attached to a queued GPUI
/// event. It must not be retained for later input or queried from another thread.
pub fn current_native_key_code(event: &KeyDownEvent) -> Option<u16> {
    CURRENT.with(|state| {
        state
            .borrow()
            .as_ref()
            .and_then(|(current, code)| (current == event).then_some(*code))
    })
}

/// Hardware key code and raw modifier flags for whichever native event is
/// dispatching on this thread right now.
///
/// Valid for key-down and flags-changed. Cleared when that dispatch returns,
/// so a replayed GPUI event outside the callback sees nothing.
pub fn current_physical_key() -> Option<(u16, u64)> {
    PHYSICAL.with(|state| {
        state
            .borrow()
            .as_ref()
            .map(|key| (key.code, key.modifier_flags))
    })
}

#[cfg(any(test, feature = "test-support"))]
thread_local! {
    static LAYOUT_FOR_TEST: RefCell<Option<(String, String)>> = const { RefCell::new(None) };
}

/// What the current keyboard layout types on the matching, currently
/// dispatched key with no modifiers and with Shift alone. GPUI resolves Shift
/// into the logical key for non-letters (`@` for Shift-2); the kitty keyboard
/// protocol needs the unshifted key back. Scoped like
/// [`current_native_key_code`].
pub fn current_key_layout_chars(event: &KeyDownEvent) -> Option<(String, String)> {
    let code = current_native_key_code(event)?;
    #[cfg(any(test, feature = "test-support"))]
    if let Some(chars) = LAYOUT_FOR_TEST.with(|state| state.borrow().clone()) {
        return Some(chars);
    }
    Some(crate::events::layout_chars_for_key_code(code))
}

/// Like [`with_native_key_for_test`], with the layout characters a fixed
/// keyboard layout would report, independent of the host's layout.
#[cfg(any(test, feature = "test-support"))]
pub fn with_native_key_layout_for_test<R>(
    event: KeyDownEvent,
    code: u16,
    layout: (&str, &str),
    callback: impl FnOnce() -> R,
) -> R {
    let previous = LAYOUT_FOR_TEST.with(|state| {
        state.replace(Some((layout.0.to_owned(), layout.1.to_owned())))
    });
    struct Restore(Option<(String, String)>);
    impl Drop for Restore {
        fn drop(&mut self) {
            LAYOUT_FOR_TEST.with(|state| state.replace(self.0.take()));
        }
    }
    let _restore = Restore(previous);
    with_native_key_for_test(event, code, callback)
}

/// Exercise the same scoped dispatch boundary without sending OS input.
#[cfg(any(test, feature = "test-support"))]
pub fn with_native_key_for_test<R>(
    event: KeyDownEvent,
    code: u16,
    callback: impl FnOnce() -> R,
) -> R {
    let _scope = NativeKeyDispatch::enter(&PlatformInput::KeyDown(event), code);
    callback()
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::Keystroke;

    fn key(name: &str) -> KeyDownEvent {
        KeyDownEvent {
            keystroke: Keystroke::parse(name).unwrap(),
            is_held: false,
            prefer_character_input: false,
        }
    }

    #[test]
    fn appkit_events_keep_logical_text_but_report_distinct_physical_keys() {
        use cocoa::{
            appkit::{NSEvent, NSEventModifierFlags, NSEventType},
            base::{id, nil},
            foundation::{NSAutoreleasePool, NSPoint, NSString},
        };
        use objc::runtime::NO;
        // These are synthetic NSEvents, never posted to the OS or another app.
        unsafe {
            let pool = NSAutoreleasePool::new(nil);
            for (characters, code, expected) in [
                ("1", 0x53, "1"),
                ("1", 0x12, "1"),
                ("\u{3}", 0x4c, "enter"),
                ("\r", 0x24, "enter"),
            ] {
                let text = NSString::alloc(nil).init_str(characters);
                let native = <id as NSEvent>::keyEventWithType_location_modifierFlags_timestamp_windowNumber_context_characters_charactersIgnoringModifiers_isARepeat_keyCode_(nil, NSEventType::NSKeyDown, NSPoint::new(0.0, 0.0), NSEventModifierFlags::empty(), 0.0, 0, nil, text, text, NO, code);
                let event = crate::events::platform_input_from_native(native, None).unwrap();
                let PlatformInput::KeyDown(key) = &event else {
                    panic!("key event");
                };
                assert_eq!(key.keystroke.key, expected);
                {
                    let _scope = NativeKeyDispatch::enter(&event, native.keyCode());
                    assert_eq!(current_native_key_code(key), Some(code));
                }
                assert_eq!(current_native_key_code(key), None);
            }
            pool.drain();
        }
    }

    #[test]
    fn matching_scope_clears_and_nested_duplicate_keys_restore_their_own_codes() {
        let event = key("1");
        assert_eq!(current_native_key_code(&event), None);
        with_native_key_for_test(event.clone(), 83, || {
            assert_eq!(current_native_key_code(&event), Some(83));
            assert_eq!(current_native_key_code(&key("2")), None);
            // Same logical key, physically distinct top-row event.
            with_native_key_for_test(event.clone(), 18, || {
                assert_eq!(current_native_key_code(&event), Some(18));
            });
            assert_eq!(current_native_key_code(&event), Some(83));
            let mut repeat = event.clone();
            repeat.is_held = true;
            assert_eq!(current_native_key_code(&repeat), None);
        });
        assert_eq!(current_native_key_code(&event), None);
        with_native_key_for_test(event.clone(), 18, || {
            assert_eq!(current_native_key_code(&event), Some(18));
        });
        assert_eq!(current_native_key_code(&event), None);
    }

    #[test]
    fn unwind_and_other_threads_cannot_leak_a_native_code() {
        let event = key("enter");
        with_native_key_for_test(event.clone(), 76, || {
            let other = event.clone();
            assert_eq!(
                std::thread::spawn(move || current_native_key_code(&other))
                    .join()
                    .unwrap(),
                None
            );
            let result = std::panic::catch_unwind(|| {
                with_native_key_for_test(event.clone(), 36, || panic!("fixture unwind"));
            });
            assert!(result.is_err());
            assert_eq!(current_native_key_code(&event), Some(76));
        });
        assert_eq!(current_native_key_code(&event), None);
    }

    #[test]
    fn flags_changed_keeps_a_physical_key_until_dispatch_ends() {
        let event = PlatformInput::ModifiersChanged(gpui::ModifiersChangedEvent::default());
        assert_eq!(current_physical_key(), None);
        let outer = NativeKeyDispatch::enter_with_flags(&event, 61, 0x40);
        assert_eq!(current_physical_key(), Some((61, 0x40)));
        {
            let inner = NativeKeyDispatch::enter_with_flags(&event, 58, 0x20);
            assert_eq!(current_physical_key(), Some((58, 0x20)));
            drop(inner);
        }
        assert_eq!(current_physical_key(), Some((61, 0x40)));
        drop(outer);
        assert_eq!(current_physical_key(), None);
    }
}
