//! Physical identity scoped to synchronous native key dispatch.
//!
//! GPUI's logical Keystroke remains unchanged for shortcuts and text/IME.
//! Consumers must query during their matching callback; saved/replayed events
//! outside that scope deliberately have no physical identity.

use gpui::{KeyDownEvent, PlatformInput};
use std::cell::RefCell;

thread_local! {
    static CURRENT: RefCell<Option<(KeyDownEvent, u16)>> = const { RefCell::new(None) };
}

pub(crate) struct NativeKeyDispatch(Option<(KeyDownEvent, u16)>);

impl NativeKeyDispatch {
    pub(crate) fn enter(event: &PlatformInput, key_code: u16) -> Self {
        let current = match event {
            PlatformInput::KeyDown(event) => Some((event.clone(), key_code)),
            _ => None,
        };
        Self(CURRENT.with(|state| state.replace(current)))
    }
}

impl Drop for NativeKeyDispatch {
    fn drop(&mut self) {
        CURRENT.with(|state| state.replace(self.0.take()));
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
}
