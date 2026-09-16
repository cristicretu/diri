//! Preserve physical keypad identity only at the terminal adapter boundary.
use diri_term::keys::{KeyEvent, KeypadKey};
use gpui::KeyDownEvent;

pub(crate) fn keypad_event(event: &KeyDownEvent) -> Option<KeyEvent> {
    // Apple SDK HIToolbox/Events.h: kVK_ANSI_Keypad* and kVK_JIS_KeypadComma.
    let key = match gpui_macos::current_native_key_code(event)? {
        0x52 => KeypadKey::Zero,
        0x53 => KeypadKey::One,
        0x54 => KeypadKey::Two,
        0x55 => KeypadKey::Three,
        0x56 => KeypadKey::Four,
        0x57 => KeypadKey::Five,
        0x58 => KeypadKey::Six,
        0x59 => KeypadKey::Seven,
        0x5b => KeypadKey::Eight,
        0x5c => KeypadKey::Nine,
        0x41 => KeypadKey::Decimal,
        0x4b => KeypadKey::Divide,
        0x43 => KeypadKey::Multiply,
        0x4e => KeypadKey::Subtract,
        0x45 => KeypadKey::Add,
        0x51 => KeypadKey::Equal,
        0x5f => KeypadKey::Separator,
        0x4c => KeypadKey::Enter,
        _ => return None,
    };
    Some(KeyEvent::keypad(key))
}

#[cfg(test)]
mod tests {
    use super::*;
    use diri_term::keys::{KeyAction, KeyboardState, Modifiers, encode_action};
    use gpui::Keystroke;

    #[test]
    fn keypad_text_still_inserts_into_ordinary_query_fields() {
        use crate::query_editor::{Edit, QueryEditor, edit_for};
        let mut event = KeyDownEvent {
            keystroke: Keystroke::parse("1").unwrap(),
            is_held: false,
            prefer_character_input: false,
        };
        event.keystroke.key_char = Some("1".into());
        let mut editor = QueryEditor::default();
        gpui_macos::with_native_key_for_test(event.clone(), 0x53, || {
            let Some(Edit::Local(edit)) = edit_for(&event.keystroke) else {
                panic!("normal text edit");
            };
            assert!(editor.apply(edit));
        });
        assert_eq!(editor.text(), "1");
        assert!(keypad_event(&event).is_none());
        event.keystroke.modifiers.platform = true;
        gpui_macos::with_native_key_for_test(event.clone(), 0x53, || {
            assert_eq!(
                event.keystroke,
                Keystroke {
                    key_char: Some("1".into()),
                    ..Keystroke::parse("cmd-1").unwrap()
                }
            );
            assert!(
                edit_for(&event.keystroke).is_none(),
                "Command shortcut does not become text"
            );
        });
    }

    #[test]
    fn native_keypad_uses_shared_encoder_without_changing_text_or_shortcuts() {
        for (logical, code, numeric, application) in [
            ("1", 0x53, b"1".as_slice(), b"\x1bOq".as_slice()),
            ("enter", 0x4c, b"\r", b"\x1bOM"),
            (".", 0x41, b".", b"\x1bOn"),
            ("+", 0x45, b"+", b"\x1bOk"),
        ] {
            let mut event = KeyDownEvent {
                keystroke: Keystroke::parse(logical).unwrap(),
                is_held: false,
                prefer_character_input: false,
            };
            event.keystroke.key_char = Some(if logical == "enter" { "\n" } else { logical }.into());
            let original = event.clone();
            assert!(keypad_event(&event).is_none());
            gpui_macos::with_native_key_for_test(event.clone(), code, || {
                let key = keypad_event(&event).unwrap();
                for (enabled, expected) in [(false, numeric), (true, application)] {
                    let modes = KeyboardState {
                        enhancements: None,
                        application_cursor_keys: false,
                        application_keypad: enabled,
                    };
                    assert_eq!(
                        encode_action(&key, Modifiers::default(), Some(modes), KeyAction::Press)
                            .unwrap(),
                        expected
                    );
                }
                assert_eq!(event, original, "text/IME/shortcut identity is untouched");
            });
            assert!(keypad_event(&event).is_none());
            // Equal logical GPUI events from the top row or ordinary Return
            // must not inherit the previous physical keypad identity.
            gpui_macos::with_native_key_for_test(
                event.clone(),
                if logical == "enter" { 0x24 } else { 0x12 },
                || {
                    assert!(keypad_event(&event).is_none());
                },
            );
        }
    }
}
