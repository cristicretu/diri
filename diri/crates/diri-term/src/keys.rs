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
