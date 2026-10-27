//! Shared terminal key and paste encoder, retained at the desktop import path.

pub use diri_proto::terminal_input::*;

/// Encode a desktop key, retaining navigation for surviving legacy sessions.
///
/// An old Holder or checkpoint may supply no keyboard metadata. In that case
/// arrows and Home/End retain the desktop's historical normal-cursor (CSI)
/// encoding. This is a compatibility policy, not an observed terminal mode:
/// do not persist it or use it for the strict automation key API. Keypad keys
/// still require authoritative state, and known application modes always win.
/// The Engine continues to enforce controller and enhanced-keyboard admission.
pub fn encode_interactive_action(
    event: &KeyEvent,
    modifiers: Modifiers,
    keyboard: Option<KeyboardState>,
    action: KeyAction,
) -> Result<Vec<u8>, KeyEncodingError> {
    match encode_action(event, modifiers, keyboard, action) {
        Err(KeyEncodingError::UnknownModes)
            if !modifiers.cmd && matches!(event.key, Key::Named(_)) =>
        {
            Ok(encode_key(event, modifiers, TermInputModes::default()))
        }
        result => result,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The desktop key path: once a program negotiates kitty flags, keys it
    /// asked to disambiguate arrive in that protocol, and releases are sent
    /// only when it asked for event types.
    #[test]
    fn interactive_keys_follow_negotiated_kitty_flags() {
        let keyboard = |bits: u8| {
            Some(KeyboardState {
                enhancements: Some(bits.try_into().unwrap()),
                ..Default::default()
            })
        };
        let enter = KeyEvent::named(NamedKey::Enter);
        let shift = Modifiers {
            shift: true,
            ..Default::default()
        };
        let encode = |bits, modifiers, action| {
            encode_interactive_action(&enter, modifiers, keyboard(bits), action).unwrap()
        };
        // Flags 0 keep the legacy bytes (Shift-Enter was already CSI u).
        assert_eq!(
            encode(0, Modifiers::default(), KeyAction::Press),
            encode_key(&enter, Modifiers::default(), TermInputModes::default())
        );
        assert_eq!(encode(1, shift, KeyAction::Press), b"\x1b[13;2u");
        assert_eq!(encode(1, Modifiers::default(), KeyAction::Press), b"\r");
        assert_eq!(encode(1, shift, KeyAction::Release), b"");
        // Enter, Tab and Backspace releases need all-keys mode (spec).
        assert_eq!(encode(3, shift, KeyAction::Release), b"");
        assert_eq!(encode(11, shift, KeyAction::Release), b"\x1b[13;2:3u");
        let escape = KeyEvent::named(NamedKey::Escape);
        assert_eq!(
            encode_interactive_action(&escape, Modifiers::default(), keyboard(1), KeyAction::Press)
                .unwrap(),
            b"\x1b[27u"
        );
        // A modifier key alone is not a key the protocol reports without
        // all-keys mode; its release sends nothing rather than an error.
        let shift_key = KeyEvent::character("shift");
        assert_eq!(
            encode_interactive_action(&shift_key, shift, keyboard(3), KeyAction::Release).unwrap(),
            b""
        );
    }

    #[test]
    fn interactive_compatibility_preserves_authoritative_modes_and_strict_errors() {
        let up = KeyEvent::named(NamedKey::ArrowUp);
        for action in [KeyAction::Press, KeyAction::Repeat] {
            for (keyboard, expected) in [
                (None, b"\x1b[A".as_slice()),
                (Some(KeyboardState::default()), b"\x1b[A"),
                (
                    Some(KeyboardState {
                        application_cursor_keys: true,
                        ..Default::default()
                    }),
                    b"\x1bOA",
                ),
            ] {
                assert_eq!(
                    encode_interactive_action(&up, Modifiers::default(), keyboard, action).unwrap(),
                    expected
                );
            }
        }
        assert_eq!(
            encode_action(&up, Modifiers::default(), None, KeyAction::Press),
            Err(KeyEncodingError::UnknownModes)
        );
        assert_eq!(
            encode_interactive_action(&up, Modifiers::default(), None, KeyAction::Release),
            Err(KeyEncodingError::UnsupportedRelease)
        );
        assert_eq!(
            encode_interactive_action(
                &KeyEvent::keypad(KeypadKey::One),
                Modifiers::default(),
                None,
                KeyAction::Press
            ),
            Err(KeyEncodingError::UnknownModes)
        );
        assert!(
            encode_interactive_action(
                &up,
                Modifiers {
                    cmd: true,
                    ..Default::default()
                },
                None,
                KeyAction::Press
            )
            .is_err()
        );
    }
}
