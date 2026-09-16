//! Platform-independent keyboard and paste encoding for terminal input.
//!
//! The default behavior follows `TerminalKeyEncoding.swift` and
//! `GridTerminalView+Input.swift`.  UI adapters should put the logical,
//! modifier-free value in [`Key::Character`] and the platform-produced text in
//! [`KeyEvent::text`].  Keeping both values is what lets Option act as Meta while
//! retaining composed characters.

const ESC: u8 = 0x1b;

/// A keyboard event's logical key.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Key {
    /// Characters with layout modifiers removed (AppKit's
    /// `charactersIgnoringModifiers`).
    Character(String),
    Named(NamedKey),
}

/// Non-text keys understood by the terminal encoder.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NamedKey {
    ArrowUp,
    ArrowDown,
    ArrowRight,
    ArrowLeft,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    Delete,
    Tab,
    Enter,
    Escape,
    Backspace,
    F1,
    F2,
    F3,
    F4,
    F5,
    F6,
    F7,
    F8,
    F9,
    F10,
    F11,
    F12,
}

/// A logical key plus the text produced by the platform keyboard layout.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KeyEvent {
    pub key: Key,
    /// AppKit's `characters`, including Option-composed characters. Named-key
    /// constructors fill this with the key's ordinary control character.
    pub text: Option<String>,
}

impl KeyEvent {
    /// Creates a text event whose logical and produced text are identical.
    pub fn character(text: impl Into<String>) -> Self {
        let text = text.into();
        Self {
            key: Key::Character(text.clone()),
            text: Some(text),
        }
    }

    /// Creates a text event with distinct logical and platform-produced text.
    /// This is normally used for an Option-composed character.
    pub fn composed(logical: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            key: Key::Character(logical.into()),
            text: Some(text.into()),
        }
    }

    pub fn named(key: NamedKey) -> Self {
        Self {
            key: Key::Named(key),
            text: named_text(key).map(str::to_owned),
        }
    }
}

/// Keyboard modifiers relevant to terminal input.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Modifiers {
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub cmd: bool,
}

/// Terminal modes and input preferences that change emitted bytes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TermInputModes {
    /// DEC application cursor keys (DECCKM). Only unmodified arrows use SS3;
    /// modified arrows retain xterm's CSI `1;mod` form.
    pub application_cursor_keys: bool,
    /// DEC bracketed-paste mode (DECSET 2004). Pass this to [`paste`].
    pub bracketed_paste: bool,
    /// `true` for DEL (0x7f), matching Dirijor's Swift view; `false` for BS
    /// (0x08), for terminals configured with the alternate backspace mapping.
    /// Configurable BS is an intentional extension; Swift always sends DEL.
    pub backspace_sends_delete: bool,
}

impl Default for TermInputModes {
    fn default() -> Self {
        Self {
            application_cursor_keys: false,
            bracketed_paste: false,
            backspace_sends_delete: true,
        }
    }
}

/// Encodes one key event as bytes to write to the PTY.
///
/// An empty vector means the event had no produced text and was not a known
/// named key. Command shortcuts are otherwise left for the UI adapter, except
/// Command-Backspace, which the Swift terminal explicitly maps to Ctrl-U.
pub fn encode_key(event: &KeyEvent, modifiers: Modifiers, modes: TermInputModes) -> Vec<u8> {
    // `performKeyEquivalent` handles Command-Backspace before `encode(event:)`.
    // Shift is allowed, but Option or Control makes it a different chord.
    if event.key == Key::Named(NamedKey::Backspace)
        && modifiers.cmd
        && !modifiers.alt
        && !modifiers.ctrl
    {
        return vec![0x15];
    }

    if let Key::Named(key) = event.key
        && let Some(bytes) = encode_csi_key(key, modifiers, modes.application_cursor_keys)
    {
        return bytes;
    }

    if modifiers.ctrl
        && let Key::Character(logical) = &event.key
        && let Some(bytes) = encode_control_text(logical)
    {
        return bytes;
    }

    if event.key == Key::Named(NamedKey::Backspace) {
        let byte = backspace_byte(modes);
        return if modifiers.alt {
            vec![ESC, byte]
        } else {
            vec![byte]
        };
    }

    let produced = event.text.as_deref().unwrap_or_default();

    // Swift checks Option before named control characters, making Option-Enter
    // ESC CR, Option-Backspace ESC DEL, and Option-Escape ESC ESC.
    if modifiers.alt && !produced.is_empty() {
        let mut bytes = Vec::with_capacity(1 + produced.len());
        bytes.push(ESC);
        bytes.extend_from_slice(produced.as_bytes());
        return bytes;
    }

    produced.as_bytes().to_vec()
}

/// Bracketed prompts are safe unless the text contains framing/control bytes.
#[must_use]
pub fn paste_needs_confirmation(text: &str, bracketed: bool) -> bool {
    (!bracketed && text.contains(['\n', '\r']))
        || text
            .chars()
            .any(|ch| ch.is_control() && !matches!(ch, '\n' | '\r' | '\t'))
}

/// Encodes pasted UTF-8 text, optionally using DEC bracketed-paste framing.
pub fn paste(text: &str, bracketed: bool) -> Vec<u8> {
    let sanitized: String = text
        .chars()
        .map(|ch| {
            if ch.is_control() && !matches!(ch, '\n' | '\r' | '\t') {
                ' '
            } else {
                ch
            }
        })
        .collect();
    let text = sanitized.as_str();
    if !bracketed {
        return text.as_bytes().to_vec();
    }

    // The current Swift GridTerminalView paste action sends raw UTF-8. This
    // framing is an intentional extension required by T6 and matches the
    // daemon's existing submitted-text framing.
    let mut bytes = Vec::with_capacity(6 + text.len() + 6);
    bytes.extend_from_slice(b"\x1b[200~");
    bytes.extend_from_slice(text.as_bytes());
    bytes.extend_from_slice(b"\x1b[201~");
    bytes
}

fn encode_csi_key(
    key: NamedKey,
    modifiers: Modifiers,
    application_cursor_keys: bool,
) -> Option<Vec<u8>> {
    let modifier = xterm_modifier(modifiers);
    let cursor = |final_byte| {
        if modifier == 1 {
            // Application-cursor support is an intentional extension: the
            // Swift encoder currently always emits CSI. Default modes preserve
            // its output exactly.
            if application_cursor_keys {
                ss3(final_byte)
            } else {
                csi_final(final_byte)
            }
        } else {
            csi_modified(modifier, final_byte)
        }
    };
    let csi = |final_byte| {
        if modifier == 1 {
            csi_final(final_byte)
        } else {
            csi_modified(modifier, final_byte)
        }
    };
    let tilde = |number| csi_tilde(number, modifier);
    let fkey = |final_byte| {
        if modifier == 1 {
            ss3(final_byte)
        } else {
            csi_modified(modifier, final_byte)
        }
    };

    // Ordered to match TerminalKeyEncoding.csiKey's Swift switch.
    match key {
        NamedKey::ArrowUp => Some(cursor(b'A')),
        NamedKey::ArrowDown => Some(cursor(b'B')),
        NamedKey::ArrowRight => Some(cursor(b'C')),
        NamedKey::ArrowLeft => Some(cursor(b'D')),
        NamedKey::Home => Some(csi(b'H')),
        NamedKey::End => Some(csi(b'F')),
        NamedKey::PageUp => Some(tilde(5)),
        NamedKey::PageDown => Some(tilde(6)),
        NamedKey::Insert => Some(tilde(2)),
        NamedKey::Delete => Some(tilde(3)),
        NamedKey::Tab if modifiers.shift => Some(b"\x1b[Z".to_vec()),
        NamedKey::Tab => None,
        NamedKey::Enter if modifiers.shift => Some(format!("\x1b[13;{modifier}u").into_bytes()),
        NamedKey::Enter => None,
        NamedKey::F1 => Some(fkey(b'P')),
        NamedKey::F2 => Some(fkey(b'Q')),
        NamedKey::F3 => Some(fkey(b'R')),
        NamedKey::F4 => Some(fkey(b'S')),
        NamedKey::F5 => Some(tilde(15)),
        NamedKey::F6 => Some(tilde(17)),
        NamedKey::F7 => Some(tilde(18)),
        NamedKey::F8 => Some(tilde(19)),
        NamedKey::F9 => Some(tilde(20)),
        NamedKey::F10 => Some(tilde(21)),
        NamedKey::F11 => Some(tilde(23)),
        NamedKey::F12 => Some(tilde(24)),
        NamedKey::Escape | NamedKey::Backspace => None,
    }
}

fn encode_control_text(logical: &str) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    let mut mapped_any = false;

    for scalar in logical.chars() {
        match scalar {
            'a'..='z' => bytes.push(scalar as u8 - b'a' + 1),
            'A'..='Z' => bytes.push(scalar as u8 - b'A' + 1),
            ' ' | '@' => bytes.push(0x00),
            '[' => bytes.push(0x1b),
            '\\' => bytes.push(0x1c),
            ']' => bytes.push(0x1d),
            // Swift's comment promises Ctrl-A...Ctrl-_ but its implementation
            // only spells out Space, `[`, `\\`, and `]`. T6 asks for the full
            // conventional punctuation set, so Ctrl-@/^/_ and Ctrl-? are
            // intentional extensions (NUL/RS/US/DEL respectively).
            '^' => bytes.push(0x1e),
            '_' => bytes.push(0x1f),
            '?' => bytes.push(0x7f),
            _ => {
                let mut encoded = [0; 4];
                bytes.extend_from_slice(scalar.encode_utf8(&mut encoded).as_bytes());
                continue;
            }
        }
        mapped_any = true;
    }

    mapped_any.then_some(bytes)
}

fn named_text(key: NamedKey) -> Option<&'static str> {
    match key {
        NamedKey::Tab => Some("\t"),
        NamedKey::Enter => Some("\r"),
        NamedKey::Escape => Some("\x1b"),
        NamedKey::Backspace => Some("\x7f"),
        _ => None,
    }
}

fn backspace_byte(modes: TermInputModes) -> u8 {
    if modes.backspace_sends_delete {
        0x7f
    } else {
        0x08
    }
}

fn xterm_modifier(modifiers: Modifiers) -> u8 {
    1 + u8::from(modifiers.shift) + 2 * u8::from(modifiers.alt) + 4 * u8::from(modifiers.ctrl)
}

fn ss3(final_byte: u8) -> Vec<u8> {
    vec![ESC, b'O', final_byte]
}

fn csi_final(final_byte: u8) -> Vec<u8> {
    vec![ESC, b'[', final_byte]
}

fn csi_modified(modifier: u8, final_byte: u8) -> Vec<u8> {
    format!("\x1b[1;{modifier}{}", char::from(final_byte)).into_bytes()
}

fn csi_tilde(number: u8, modifier: u8) -> Vec<u8> {
    if modifier == 1 {
        format!("\x1b[{number}~").into_bytes()
    } else {
        format!("\x1b[{number};{modifier}~").into_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MODIFIER_CASES: [(&str, Modifiers, u8); 8] = [
        ("plain", modifiers(false, false, false, false), 1),
        ("shift", modifiers(true, false, false, false), 2),
        ("alt", modifiers(false, false, true, false), 3),
        ("shift-alt", modifiers(true, false, true, false), 4),
        ("ctrl", modifiers(false, true, false, false), 5),
        ("shift-ctrl", modifiers(true, true, false, false), 6),
        ("alt-ctrl", modifiers(false, true, true, false), 7),
        ("shift-alt-ctrl", modifiers(true, true, true, false), 8),
    ];

    const fn modifiers(shift: bool, ctrl: bool, alt: bool, cmd: bool) -> Modifiers {
        Modifiers {
            shift,
            ctrl,
            alt,
            cmd,
        }
    }

    fn event(key: NamedKey) -> KeyEvent {
        KeyEvent::named(key)
    }

    fn expected_csi(final_byte: u8, modifier: u8) -> Vec<u8> {
        if modifier == 1 {
            vec![ESC, b'[', final_byte]
        } else {
            format!("\x1b[1;{modifier}{}", char::from(final_byte)).into_bytes()
        }
    }

    fn expected_fkey(final_byte: u8, modifier: u8) -> Vec<u8> {
        if modifier == 1 {
            vec![ESC, b'O', final_byte]
        } else {
            format!("\x1b[1;{modifier}{}", char::from(final_byte)).into_bytes()
        }
    }

    fn expected_tilde(number: u8, modifier: u8) -> Vec<u8> {
        if modifier == 1 {
            format!("\x1b[{number}~").into_bytes()
        } else {
            format!("\x1b[{number};{modifier}~").into_bytes()
        }
    }

    #[test]
    fn xterm_modifier_matches_swift_table() {
        for (name, modifiers, expected) in MODIFIER_CASES {
            assert_eq!(xterm_modifier(modifiers), expected, "{name}");
        }
    }

    #[test]
    fn navigation_and_function_switch_matches_swift_top_to_bottom() {
        #[derive(Clone, Copy)]
        enum Sequence {
            Csi(u8),
            Tilde(u8),
            FKey(u8),
        }

        // This order is TerminalKeyEncoding.csiKey's switch order. Each row is
        // exercised with all eight xterm modifier values and with Command both
        // absent and present (Command is deliberately not an xterm modifier).
        let cases = [
            ("up", NamedKey::ArrowUp, Sequence::Csi(b'A')),
            ("down", NamedKey::ArrowDown, Sequence::Csi(b'B')),
            ("right", NamedKey::ArrowRight, Sequence::Csi(b'C')),
            ("left", NamedKey::ArrowLeft, Sequence::Csi(b'D')),
            ("home", NamedKey::Home, Sequence::Csi(b'H')),
            ("end", NamedKey::End, Sequence::Csi(b'F')),
            ("page-up", NamedKey::PageUp, Sequence::Tilde(5)),
            ("page-down", NamedKey::PageDown, Sequence::Tilde(6)),
            ("insert/help", NamedKey::Insert, Sequence::Tilde(2)),
            ("forward-delete", NamedKey::Delete, Sequence::Tilde(3)),
            ("f1", NamedKey::F1, Sequence::FKey(b'P')),
            ("f2", NamedKey::F2, Sequence::FKey(b'Q')),
            ("f3", NamedKey::F3, Sequence::FKey(b'R')),
            ("f4", NamedKey::F4, Sequence::FKey(b'S')),
            ("f5", NamedKey::F5, Sequence::Tilde(15)),
            ("f6", NamedKey::F6, Sequence::Tilde(17)),
            ("f7", NamedKey::F7, Sequence::Tilde(18)),
            ("f8", NamedKey::F8, Sequence::Tilde(19)),
            ("f9", NamedKey::F9, Sequence::Tilde(20)),
            ("f10", NamedKey::F10, Sequence::Tilde(21)),
            ("f11", NamedKey::F11, Sequence::Tilde(23)),
            ("f12", NamedKey::F12, Sequence::Tilde(24)),
        ];

        for (key_name, key, sequence) in cases {
            for (modifier_name, base_modifiers, modifier) in MODIFIER_CASES {
                for cmd in [false, true] {
                    let modifiers = Modifiers {
                        cmd,
                        ..base_modifiers
                    };
                    let expected = match sequence {
                        Sequence::Csi(final_byte) => expected_csi(final_byte, modifier),
                        Sequence::Tilde(number) => expected_tilde(number, modifier),
                        Sequence::FKey(final_byte) => expected_fkey(final_byte, modifier),
                    };
                    assert_eq!(
                        encode_key(&event(key), modifiers, TermInputModes::default()),
                        expected,
                        "{key_name}, {modifier_name}, cmd={cmd}"
                    );
                }
            }
        }
    }

    #[test]
    fn tab_and_enter_switch_cases_match_swift_for_every_modifier() {
        for (name, base_modifiers, modifier) in MODIFIER_CASES {
            for cmd in [false, true] {
                let modifiers = Modifiers {
                    cmd,
                    ..base_modifiers
                };

                let tab = if modifiers.shift {
                    b"\x1b[Z".to_vec()
                } else if modifiers.alt {
                    b"\x1b\t".to_vec()
                } else {
                    b"\t".to_vec()
                };
                assert_eq!(
                    encode_key(&event(NamedKey::Tab), modifiers, TermInputModes::default()),
                    tab,
                    "tab, {name}, cmd={cmd}"
                );

                let enter = if modifiers.shift {
                    format!("\x1b[13;{modifier}u").into_bytes()
                } else if modifiers.alt {
                    b"\x1b\r".to_vec()
                } else {
                    b"\r".to_vec()
                };
                assert_eq!(
                    encode_key(
                        &event(NamedKey::Enter),
                        modifiers,
                        TermInputModes::default()
                    ),
                    enter,
                    "enter, {name}, cmd={cmd}"
                );
            }
        }
    }

    #[test]
    fn application_cursor_mode_uses_ss3_only_for_unmodified_arrows() {
        let cases = [
            (NamedKey::ArrowUp, b'A'),
            (NamedKey::ArrowDown, b'B'),
            (NamedKey::ArrowRight, b'C'),
            (NamedKey::ArrowLeft, b'D'),
        ];

        for (key, final_byte) in cases {
            for (name, base_modifiers, modifier) in MODIFIER_CASES {
                for cmd in [false, true] {
                    let modifiers = Modifiers {
                        cmd,
                        ..base_modifiers
                    };
                    let modes = TermInputModes {
                        application_cursor_keys: true,
                        ..TermInputModes::default()
                    };
                    let expected = if modifier == 1 {
                        vec![ESC, b'O', final_byte]
                    } else {
                        expected_csi(final_byte, modifier)
                    };
                    assert_eq!(
                        encode_key(&event(key), modifiers, modes),
                        expected,
                        "{key:?}, {name}, cmd={cmd}"
                    );
                }
            }
        }
    }

    #[test]
    fn option_arrows_are_xterm_word_jump_sequences() {
        let alt = modifiers(false, false, true, false);
        assert_eq!(
            encode_key(&event(NamedKey::ArrowLeft), alt, TermInputModes::default()),
            b"\x1b[1;3D"
        );
        assert_eq!(
            encode_key(&event(NamedKey::ArrowRight), alt, TermInputModes::default()),
            b"\x1b[1;3C"
        );
    }

    #[test]
    fn named_control_keys_match_swift_for_every_modifier() {
        for (name, base_modifiers, _) in MODIFIER_CASES {
            for cmd in [false, true] {
                let modifiers = Modifiers {
                    cmd,
                    ..base_modifiers
                };
                let escape = if modifiers.alt {
                    b"\x1b\x1b".to_vec()
                } else {
                    b"\x1b".to_vec()
                };
                assert_eq!(
                    encode_key(
                        &event(NamedKey::Escape),
                        modifiers,
                        TermInputModes::default()
                    ),
                    escape,
                    "escape, {name}, cmd={cmd}"
                );

                for backspace_sends_delete in [true, false] {
                    let modes = TermInputModes {
                        backspace_sends_delete,
                        ..TermInputModes::default()
                    };
                    let backspace = if modifiers.cmd && !modifiers.alt && !modifiers.ctrl {
                        vec![0x15]
                    } else {
                        let byte = if backspace_sends_delete { 0x7f } else { 0x08 };
                        if modifiers.alt {
                            vec![ESC, byte]
                        } else {
                            vec![byte]
                        }
                    };
                    assert_eq!(
                        encode_key(&event(NamedKey::Backspace), modifiers, modes),
                        backspace,
                        "backspace, {name}, cmd={cmd}, del={backspace_sends_delete}"
                    );
                }
            }
        }
    }

    #[test]
    fn ctrl_letters_match_swift_a_through_z() {
        let ctrl = modifiers(false, true, false, false);
        let ctrl_alt = modifiers(false, true, true, false);

        for offset in 0_u8..26 {
            let lower = char::from(b'a' + offset).to_string();
            let upper = char::from(b'A' + offset).to_string();
            let expected = vec![offset + 1];
            assert_eq!(
                encode_key(&KeyEvent::character(lower), ctrl, TermInputModes::default()),
                expected,
                "lowercase offset {offset}"
            );
            assert_eq!(
                encode_key(&KeyEvent::character(upper), ctrl, TermInputModes::default()),
                expected,
                "uppercase offset {offset}"
            );
            assert_eq!(
                encode_key(
                    &KeyEvent::character(char::from(b'a' + offset).to_string()),
                    ctrl_alt,
                    TermInputModes::default()
                ),
                expected,
                "control takes precedence over Option at offset {offset}"
            );
        }
    }

    #[test]
    fn ctrl_symbol_table_includes_all_c0_punctuation() {
        let ctrl = modifiers(false, true, false, false);
        let cases = [
            ("space", " ", 0x00),
            ("at", "@", 0x00),
            ("left-bracket", "[", 0x1b),
            ("backslash", "\\", 0x1c),
            ("right-bracket", "]", 0x1d),
            ("caret", "^", 0x1e),
            ("underscore", "_", 0x1f),
            ("question", "?", 0x7f),
        ];

        for (name, input, expected) in cases {
            assert_eq!(
                encode_key(&KeyEvent::character(input), ctrl, TermInputModes::default()),
                vec![expected],
                "{name}"
            );
        }
    }

    #[test]
    fn option_prefixes_the_composed_text_not_the_logical_key() {
        let alt = modifiers(false, false, true, false);
        let cases = [
            (KeyEvent::character("x"), b"\x1bx".as_slice()),
            (KeyEvent::composed("e", "é"), b"\x1b\xc3\xa9".as_slice()),
            (
                KeyEvent::composed("dead-key", "´e"),
                b"\x1b\xc2\xb4e".as_slice(),
            ),
        ];

        for (event, expected) in cases {
            assert_eq!(encode_key(&event, alt, TermInputModes::default()), expected);
        }
    }

    #[test]
    fn printable_and_unmapped_control_text_pass_through_utf8() {
        assert_eq!(
            encode_key(
                &KeyEvent::character("λ🙂"),
                Modifiers::default(),
                TermInputModes::default()
            ),
            "λ🙂".as_bytes()
        );
        assert_eq!(
            encode_key(
                &KeyEvent::character("1"),
                modifiers(false, true, false, false),
                TermInputModes::default()
            ),
            b"1"
        );
        assert_eq!(
            encode_key(
                &KeyEvent::character("1"),
                modifiers(false, true, true, false),
                TermInputModes::default()
            ),
            b"\x1b1"
        );
        assert_eq!(
            encode_key(
                &KeyEvent::character("a1"),
                modifiers(false, true, false, false),
                TermInputModes::default()
            ),
            b"\x011"
        );
        assert_eq!(
            encode_key(
                &KeyEvent {
                    key: Key::Character("x".into()),
                    text: None,
                },
                Modifiers::default(),
                TermInputModes::default()
            ),
            Vec::<u8>::new()
        );
    }

    #[test]
    fn paste_matches_raw_swift_path_and_bracketed_extension() {
        let text = "one\ntwo λ";
        assert_eq!(paste(text, false), text.as_bytes());
        assert_eq!(paste(text, true), b"\x1b[200~one\ntwo \xce\xbb\x1b[201~");
        assert_eq!(paste("", false), b"");
        assert_eq!(paste("", true), b"\x1b[200~\x1b[201~");
    }
}

#[cfg(test)]
mod paste_protection_tests {
    use super::*;
    #[test]
    fn review_is_mode_aware_and_controls_cannot_escape_bracketing() {
        assert!(paste_needs_confirmation("one\ntwo", false));
        assert!(!paste_needs_confirmation("one\ntwo", true));
        assert!(paste_needs_confirmation("\x1b[201~run", true));
        assert!(paste_needs_confirmation("\x03", false));
        let bytes = paste("hello\x1b[201~\x03\0world", true);
        assert_eq!(
            bytes
                .windows(6)
                .filter(|part| *part == b"\x1b[201~")
                .count(),
            1
        );
        assert!(!bytes.contains(&3));
        assert!(!bytes.contains(&0));
    }

    /// Issue #275: a clipboard payload that opens with the paste end-marker
    /// (followed by a bare CR) must not be able to close the bracketed
    /// envelope early and let the trailing bytes submit as if typed.
    #[test]
    fn embedded_end_marker_cannot_submit_without_enter() {
        let bytes = paste("\x1b[201~printf x\r", true);

        // The only end marker present is the real, outer one — at the very
        // end of the buffer.
        let end_marker_positions: Vec<usize> = bytes
            .windows(6)
            .enumerate()
            .filter(|(_, window)| *window == b"\x1b[201~")
            .map(|(index, _)| index)
            .collect();
        assert_eq!(end_marker_positions, vec![bytes.len() - 6]);
        assert!(bytes.starts_with(b"\x1b[200~"));
        assert!(bytes.ends_with(b"\x1b[201~"));

        let interior = &bytes[6..bytes.len() - 6];
        assert!(!interior.contains(&ESC));
        assert_eq!(interior.iter().filter(|byte| **byte == b'\r').count(), 1);
    }
}
