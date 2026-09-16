//! Pure Kitty keyboard encoding. Callers must supply the parser's negotiated
//! flags and filter application shortcuts before invoking this encoder.
//!
//! Specification: <https://sw.kovidgoyal.net/kitty/keyboard-protocol/>.
//! This module alone does not enable negotiation in the terminal parser.

use std::fmt::Write;

use serde::{Deserialize, Serialize};

use super::{Key, KeyAction, KeyEvent, KeypadKey, Modifiers, NamedKey, TermInputModes, encode_key};

/// Validated progressive enhancement flags, in the protocol's bit order.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(try_from = "u8", into = "u8")]
pub struct KeyboardEnhancements(u8);

impl KeyboardEnhancements {
    pub const DISAMBIGUATE: u8 = 1;
    pub const EVENT_TYPES: u8 = 2;
    pub const ALTERNATE_KEYS: u8 = 4;
    pub const ALL_KEYS: u8 = 8;
    pub const ASSOCIATED_TEXT: u8 = 16;

    pub const fn bits(self) -> u8 {
        self.0
    }

    pub const fn contains(self, flags: u8) -> bool {
        self.0 & flags == flags
    }
}

impl TryFrom<u8> for KeyboardEnhancements {
    type Error = &'static str;

    fn try_from(bits: u8) -> Result<Self, Self::Error> {
        if bits & !31 != 0 {
            Err("unknown keyboard enhancement flags")
        } else {
            Ok(Self(bits))
        }
    }
}

impl From<KeyboardEnhancements> for u8 {
    fn from(flags: KeyboardEnhancements) -> Self {
        flags.0
    }
}

/// State after the current event, including both physical sides of modifiers.
/// The platform adapter supplies lock state; the encoder never infers it.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct EnhancedModifiers {
    pub ordinary: Modifiers,
    pub hyper: bool,
    pub meta: bool,
    pub caps_lock: bool,
    pub num_lock: bool,
}

impl EnhancedModifiers {
    fn bits(self) -> u8 {
        u8::from(self.ordinary.shift)
            | (u8::from(self.ordinary.alt) << 1)
            | (u8::from(self.ordinary.ctrl) << 2)
            | (u8::from(self.ordinary.cmd) << 3)
            | (u8::from(self.hyper) << 4)
            | (u8::from(self.meta) << 5)
            | (u8::from(self.caps_lock) << 6)
            | (u8::from(self.num_lock) << 7)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModifierKey {
    LeftShift,
    LeftControl,
    LeftAlt,
    LeftSuper,
    LeftHyper,
    LeftMeta,
    RightShift,
    RightControl,
    RightAlt,
    RightSuper,
    RightHyper,
    RightMeta,
    IsoLevel3Shift,
    IsoLevel5Shift,
}

impl ModifierKey {
    fn code(self) -> u32 {
        match self {
            Self::LeftShift => 57441,
            Self::LeftControl => 57442,
            Self::LeftAlt => 57443,
            Self::LeftSuper => 57444,
            Self::LeftHyper => 57445,
            Self::LeftMeta => 57446,
            Self::RightShift => 57447,
            Self::RightControl => 57448,
            Self::RightAlt => 57449,
            Self::RightSuper => 57450,
            Self::RightHyper => 57451,
            Self::RightMeta => 57452,
            Self::IsoLevel3Shift => 57453,
            Self::IsoLevel5Shift => 57454,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EnhancedKey {
    Ordinary(Key),
    Modifier(ModifierKey),
    /// F13 through F35; F1 through F12 use the existing named keys.
    ExtendedFunction(u8),
    /// An IME commit with no physical key identity. Never invent a key code.
    Text,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EnhancedKeyEvent {
    pub key: EnhancedKey,
    pub text: Option<String>,
    /// Actual shifted layout character, not a guessed uppercase mapping.
    pub shifted_key: Option<char>,
    /// Actual PC-101 physical layout character, when the platform exposes it.
    pub base_layout_key: Option<char>,
}

impl From<KeyEvent> for EnhancedKeyEvent {
    fn from(event: KeyEvent) -> Self {
        Self {
            key: EnhancedKey::Ordinary(event.key),
            text: event.text,
            shifted_key: None,
            base_layout_key: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EnhancedEncodingError {
    /// A logical key must be one Unicode scalar; composed text belongs in text.
    InvalidLogicalKey,
    InvalidFunctionKey,
}

impl std::fmt::Display for EnhancedEncodingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::InvalidLogicalKey => "enhanced logical key must be one Unicode scalar",
            Self::InvalidFunctionKey => "extended function key must be F13 through F35",
        })
    }
}
impl std::error::Error for EnhancedEncodingError {}

/// Encode a terminal-owned event. Empty output means the application did not
/// request that event, for example a release without event reporting. Shortcut
/// ownership and delivery receipts remain the caller's responsibility.
pub fn encode(
    event: &EnhancedKeyEvent,
    modifiers: EnhancedModifiers,
    modes: TermInputModes,
    flags: KeyboardEnhancements,
    action: KeyAction,
) -> Result<Vec<u8>, EnhancedEncodingError> {
    let all = flags.contains(KeyboardEnhancements::ALL_KEYS);
    let events = flags.contains(KeyboardEnhancements::EVENT_TYPES);
    let disambiguate = flags.contains(KeyboardEnhancements::DISAMBIGUATE) || all;
    let active = disambiguate || events;
    let reset_key = matches!(
        event.key,
        EnhancedKey::Ordinary(Key::Named(
            NamedKey::Enter | NamedKey::Tab | NamedKey::Backspace
        ))
    );
    if action == KeyAction::Release && (!events || (!all && reset_key)) {
        return Ok(Vec::new());
    }
    if matches!(event.key, EnhancedKey::Modifier(_)) && !all {
        return Ok(Vec::new());
    }

    // IMEs can commit multiple scalars without a physical key. In all-keys
    // mode only associated-text reporting permits delivery of that text.
    if event.key == EnhancedKey::Text {
        return Ok(if action == KeyAction::Release {
            Vec::new()
        } else if !all {
            event
                .text
                .as_deref()
                .unwrap_or_default()
                .as_bytes()
                .to_vec()
        } else if flags.contains(KeyboardEnhancements::ASSOCIATED_TEXT) {
            printable_text(event).map_or_else(Vec::new, |text| {
                sequence(0, 'u', None, None, 0, KeyAction::Press, false, Some(text))
            })
        } else {
            Vec::new()
        });
    }

    let chord = modifiers.bits() & 0b11_1110 != 0;
    let character = matches!(event.key, EnhancedKey::Ordinary(Key::Character(_)));
    let raw_text = character && !all && !chord;
    let csi = if all {
        true
    } else if reset_key {
        disambiguate && modifiers.bits() & 0b11_1111 != 0
    } else if character {
        active && chord
    } else {
        active
    };
    if !csi || raw_text {
        if action == KeyAction::Release {
            return Ok(Vec::new());
        }
        return Ok(match &event.key {
            EnhancedKey::Ordinary(key) => encode_key(
                &KeyEvent {
                    key: key.clone(),
                    text: event.text.clone(),
                },
                modifiers.ordinary,
                modes,
            ),
            EnhancedKey::ExtendedFunction(_) | EnhancedKey::Modifier(_) => Vec::new(),
            EnhancedKey::Text => unreachable!(),
        });
    }

    let (number, suffix) = match &event.key {
        EnhancedKey::Ordinary(Key::Character(logical)) => {
            let mut scalars = logical.chars();
            let scalar = scalars
                .next()
                .ok_or(EnhancedEncodingError::InvalidLogicalKey)?;
            if scalars.next().is_some() || scalar.is_control() {
                return Err(EnhancedEncodingError::InvalidLogicalKey);
            }
            (u32::from(scalar), 'u')
        }
        EnhancedKey::Ordinary(Key::Named(key)) => named_code(*key),
        EnhancedKey::Ordinary(Key::Keypad(key)) => (keypad_code(*key), 'u'),
        EnhancedKey::Modifier(key) => (key.code(), 'u'),
        EnhancedKey::ExtendedFunction(number @ 13..=35) => (57376 + u32::from(*number - 13), 'u'),
        EnhancedKey::ExtendedFunction(_) => return Err(EnhancedEncodingError::InvalidFunctionKey),
        EnhancedKey::Text => unreachable!(),
    };
    let alternates = character && flags.contains(KeyboardEnhancements::ALTERNATE_KEYS);
    let shifted = (alternates && modifiers.ordinary.shift)
        .then_some(event.shifted_key)
        .flatten();
    let base = alternates.then_some(event.base_layout_key).flatten();
    let text = if all
        && flags.contains(KeyboardEnhancements::ASSOCIATED_TEXT)
        && action != KeyAction::Release
    {
        printable_text(event)
    } else {
        None
    };
    Ok(sequence(
        number,
        suffix,
        shifted,
        base,
        modifiers.bits(),
        action,
        events,
        text,
    ))
}

fn printable_text(event: &EnhancedKeyEvent) -> Option<&str> {
    event
        .text
        .as_deref()
        .filter(|text| !text.is_empty() && !text.chars().any(char::is_control))
}

#[allow(clippy::too_many_arguments)]
fn sequence(
    number: u32,
    suffix: char,
    shifted: Option<char>,
    base: Option<char>,
    modifier_bits: u8,
    action: KeyAction,
    events: bool,
    text: Option<&str>,
) -> Vec<u8> {
    let event_type = match (events, action) {
        (true, KeyAction::Repeat) => Some(2),
        (true, KeyAction::Release) => Some(3),
        _ => None,
    };
    let parameters = modifier_bits != 0 || event_type.is_some() || text.is_some();
    let mut result = String::from("\x1b[");
    if number != 1 || !matches!(suffix, 'A'..='H' | 'P' | 'Q' | 'S') || parameters {
        write!(result, "{number}").expect("String write");
    }
    if shifted.is_some() || base.is_some() {
        result.push(':');
        if let Some(shifted) = shifted {
            write!(result, "{}", u32::from(shifted)).expect("String write");
        }
        if let Some(base) = base {
            write!(result, ":{}", u32::from(base)).expect("String write");
        }
    }
    if parameters {
        write!(result, ";{}", u16::from(modifier_bits) + 1).expect("String write");
    }
    if let Some(event_type) = event_type {
        write!(result, ":{event_type}").expect("String write");
    }
    if let Some(text) = text {
        for (index, scalar) in text.chars().enumerate() {
            let separator = if index == 0 { ';' } else { ':' };
            write!(result, "{separator}{}", u32::from(scalar)).expect("String write");
        }
    }
    result.push(suffix);
    result.into_bytes()
}

fn named_code(key: NamedKey) -> (u32, char) {
    match key {
        NamedKey::Escape => (27, 'u'),
        NamedKey::Enter => (13, 'u'),
        NamedKey::Tab => (9, 'u'),
        NamedKey::Backspace => (127, 'u'),
        NamedKey::ArrowUp => (1, 'A'),
        NamedKey::ArrowDown => (1, 'B'),
        NamedKey::ArrowRight => (1, 'C'),
        NamedKey::ArrowLeft => (1, 'D'),
        NamedKey::Home => (1, 'H'),
        NamedKey::End => (1, 'F'),
        NamedKey::PageUp => (5, '~'),
        NamedKey::PageDown => (6, '~'),
        NamedKey::Insert => (2, '~'),
        NamedKey::Delete => (3, '~'),
        NamedKey::F1 => (1, 'P'),
        NamedKey::F2 => (1, 'Q'),
        // CSI R collides with cursor position reports.
        NamedKey::F3 => (13, '~'),
        NamedKey::F4 => (1, 'S'),
        NamedKey::F5 => (15, '~'),
        NamedKey::F6 => (17, '~'),
        NamedKey::F7 => (18, '~'),
        NamedKey::F8 => (19, '~'),
        NamedKey::F9 => (20, '~'),
        NamedKey::F10 => (21, '~'),
        NamedKey::F11 => (23, '~'),
        NamedKey::F12 => (24, '~'),
    }
}

fn keypad_code(key: KeypadKey) -> u32 {
    match key {
        KeypadKey::Zero => 57399,
        KeypadKey::One => 57400,
        KeypadKey::Two => 57401,
        KeypadKey::Three => 57402,
        KeypadKey::Four => 57403,
        KeypadKey::Five => 57404,
        KeypadKey::Six => 57405,
        KeypadKey::Seven => 57406,
        KeypadKey::Eight => 57407,
        KeypadKey::Nine => 57408,
        KeypadKey::Decimal => 57409,
        KeypadKey::Divide => 57410,
        KeypadKey::Multiply => 57411,
        KeypadKey::Subtract => 57412,
        KeypadKey::Add => 57413,
        KeypadKey::Enter => 57414,
        KeypadKey::Equal => 57415,
        KeypadKey::Separator => 57416,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes(
        event: impl Into<EnhancedKeyEvent>,
        modifiers: Modifiers,
        flags: u8,
        action: KeyAction,
    ) -> Vec<u8> {
        encode(
            &event.into(),
            EnhancedModifiers {
                ordinary: modifiers,
                ..Default::default()
            },
            TermInputModes::default(),
            flags.try_into().unwrap(),
            action,
        )
        .unwrap()
    }

    #[test]
    fn enhancement_flags_round_trip_and_reject_unknown_wire_bits() {
        for bits in 0..=31 {
            let flags = KeyboardEnhancements::try_from(bits).unwrap();
            assert_eq!(serde_json::to_string(&flags).unwrap(), bits.to_string());
            assert_eq!(
                serde_json::from_str::<KeyboardEnhancements>(&bits.to_string()).unwrap(),
                flags
            );
        }
        for invalid in ["32", "255", "256", "-1", "null", "{}"] {
            assert!(serde_json::from_str::<KeyboardEnhancements>(invalid).is_err());
        }
    }

    #[test]
    fn ordinary_text_and_legacy_modes_survive_non_activating_flags() {
        for flags in [0, 4, 16, 20] {
            for event in [
                KeyEvent::character("a"),
                KeyEvent::composed("a", "å"),
                KeyEvent::named(NamedKey::ArrowUp),
                KeyEvent::keypad(KeypadKey::One),
            ] {
                for modifiers in [
                    Modifiers::default(),
                    Modifiers {
                        alt: true,
                        ..Default::default()
                    },
                ] {
                    let modes = TermInputModes {
                        application_cursor_keys: true,
                        application_keypad: true,
                        ..Default::default()
                    };
                    for action in [KeyAction::Press, KeyAction::Repeat] {
                        assert_eq!(
                            encode(
                                &event.clone().into(),
                                EnhancedModifiers {
                                    ordinary: modifiers,
                                    ..Default::default()
                                },
                                modes,
                                flags.try_into().unwrap(),
                                action
                            )
                            .unwrap(),
                            encode_key(&event, modifiers, modes)
                        );
                    }
                }
            }
        }
        for flags in [1, 2, 3, 7] {
            assert_eq!(
                bytes(
                    KeyEvent::composed("a", "A"),
                    Modifiers {
                        shift: true,
                        ..Default::default()
                    },
                    flags,
                    KeyAction::Repeat
                ),
                b"A"
            );
            assert!(
                bytes(
                    KeyEvent::character("a"),
                    Modifiers::default(),
                    flags,
                    KeyAction::Release
                )
                .is_empty()
            );
        }
    }

    #[test]
    fn ctrl_shift_and_alt_keys_are_distinct_from_control_bytes() {
        for (modifiers, expected) in [
            (
                Modifiers {
                    ctrl: true,
                    ..Default::default()
                },
                "\x1b[105;5u",
            ),
            (
                Modifiers {
                    ctrl: true,
                    shift: true,
                    ..Default::default()
                },
                "\x1b[105;6u",
            ),
            (
                Modifiers {
                    ctrl: true,
                    alt: true,
                    ..Default::default()
                },
                "\x1b[105;7u",
            ),
            (
                Modifiers {
                    alt: true,
                    ..Default::default()
                },
                "\x1b[105;3u",
            ),
        ] {
            assert_eq!(
                bytes(KeyEvent::character("i"), modifiers, 1, KeyAction::Press),
                expected.as_bytes()
            );
        }
        assert_eq!(
            bytes(
                KeyEvent::named(NamedKey::Escape),
                Modifiers::default(),
                1,
                KeyAction::Press
            ),
            b"\x1b[27u"
        );
        assert_eq!(
            bytes(
                KeyEvent::character("["),
                Modifiers {
                    alt: true,
                    ..Default::default()
                },
                1,
                KeyAction::Press
            ),
            b"\x1b[91;3u"
        );
    }

    #[test]
    fn reset_keys_keep_unmodified_bytes_and_suppress_release_until_all_keys() {
        for (key, raw, code) in [
            (NamedKey::Enter, b"\r".as_slice(), 13),
            (NamedKey::Tab, b"\t", 9),
            (NamedKey::Backspace, b"\x7f", 127),
        ] {
            for flags in [1, 2, 3, 7, 23] {
                for action in [KeyAction::Press, KeyAction::Repeat] {
                    assert_eq!(
                        bytes(KeyEvent::named(key), Modifiers::default(), flags, action),
                        raw
                    );
                }
                assert!(
                    bytes(
                        KeyEvent::named(key),
                        Modifiers {
                            ctrl: true,
                            ..Default::default()
                        },
                        flags,
                        KeyAction::Release
                    )
                    .is_empty()
                );
            }
            assert_eq!(
                bytes(
                    KeyEvent::named(key),
                    Modifiers::default(),
                    11,
                    KeyAction::Release
                ),
                format!("\x1b[{code};1:3u").as_bytes()
            );
        }
        assert_eq!(
            bytes(
                KeyEvent::named(NamedKey::Tab),
                Modifiers {
                    shift: true,
                    ctrl: true,
                    ..Default::default()
                },
                1,
                KeyAction::Press
            ),
            b"\x1b[9;6u"
        );
    }

    #[test]
    fn canonical_functional_keys_do_not_use_ss3_or_collide_with_cursor_reports() {
        for (key, expected) in [
            (NamedKey::ArrowUp, "\x1b[A"),
            (NamedKey::Home, "\x1b[H"),
            (NamedKey::F1, "\x1b[P"),
            (NamedKey::F2, "\x1b[Q"),
            (NamedKey::F3, "\x1b[13~"),
            (NamedKey::F4, "\x1b[S"),
            (NamedKey::F12, "\x1b[24~"),
        ] {
            assert_eq!(
                bytes(
                    KeyEvent::named(key),
                    Modifiers::default(),
                    1,
                    KeyAction::Press
                ),
                expected.as_bytes()
            );
        }
        assert_eq!(
            bytes(
                KeyEvent::named(NamedKey::ArrowUp),
                Modifiers::default(),
                3,
                KeyAction::Repeat
            ),
            b"\x1b[1;1:2A"
        );
        assert_eq!(
            bytes(
                KeyEvent::named(NamedKey::F3),
                Modifiers::default(),
                3,
                KeyAction::Release
            ),
            b"\x1b[13;1:3~"
        );
        assert_eq!(
            bytes(
                KeyEvent::keypad(KeypadKey::Enter),
                Modifiers::default(),
                3,
                KeyAction::Release
            ),
            b"\x1b[57414;1:3u"
        );
        assert_eq!(
            bytes(
                KeyEvent::keypad(KeypadKey::One),
                Modifiers::default(),
                1,
                KeyAction::Press
            ),
            b"\x1b[57400u"
        );
    }

    #[test]
    fn alternates_come_from_layout_and_shifted_field_requires_shift() {
        let mut event: EnhancedKeyEvent = KeyEvent::composed("с", "С").into();
        event.shifted_key = Some('С');
        event.base_layout_key = Some('c');
        assert_eq!(
            bytes(
                event.clone(),
                Modifiers {
                    ctrl: true,
                    ..Default::default()
                },
                5,
                KeyAction::Press
            ),
            b"\x1b[1089::99;5u"
        );
        assert_eq!(
            bytes(
                event.clone(),
                Modifiers {
                    ctrl: true,
                    shift: true,
                    ..Default::default()
                },
                5,
                KeyAction::Press
            ),
            b"\x1b[1089:1057:99;6u"
        );
        assert_eq!(
            bytes(
                event,
                Modifiers {
                    ctrl: true,
                    shift: true,
                    ..Default::default()
                },
                1,
                KeyAction::Press
            ),
            b"\x1b[1089;6u"
        );
        let mut punctuation: EnhancedKeyEvent = KeyEvent::composed("=", "+").into();
        punctuation.shifted_key = Some('+');
        assert_eq!(
            bytes(
                punctuation,
                Modifiers {
                    shift: true,
                    ctrl: true,
                    ..Default::default()
                },
                5,
                KeyAction::Press
            ),
            b"\x1b[61:43;6u"
        );
    }

    #[test]
    fn associated_text_is_unicode_preserves_combining_scalars_and_never_repeats_on_release() {
        let event = KeyEvent::composed("e", "e\u{301}");
        assert_eq!(
            bytes(event.clone(), Modifiers::default(), 31, KeyAction::Press),
            b"\x1b[101;1;101:769u"
        );
        assert_eq!(
            bytes(event.clone(), Modifiers::default(), 31, KeyAction::Repeat),
            b"\x1b[101;1:2;101:769u"
        );
        assert_eq!(
            bytes(event, Modifiers::default(), 31, KeyAction::Release),
            b"\x1b[101;1:3u"
        );
        for text in ["\0", "a\x1bb", "\u{0085}", "\u{007f}"] {
            assert_eq!(
                bytes(
                    KeyEvent::composed("a", text),
                    Modifiers::default(),
                    24,
                    KeyAction::Press
                ),
                b"\x1b[97u"
            );
        }
    }

    #[test]
    fn ime_commits_use_unknown_key_and_do_not_invent_modifiers_or_releases() {
        let event = EnhancedKeyEvent {
            key: EnhancedKey::Text,
            text: Some("日本".into()),
            shifted_key: None,
            base_layout_key: None,
        };
        let modifiers = Modifiers {
            alt: true,
            ..Default::default()
        };
        assert_eq!(
            bytes(event.clone(), modifiers, 31, KeyAction::Press),
            b"\x1b[0;1;26085:26412u"
        );
        assert_eq!(
            bytes(event.clone(), modifiers, 1, KeyAction::Press),
            "日本".as_bytes()
        );
        assert!(bytes(event.clone(), modifiers, 8, KeyAction::Press).is_empty());
        assert!(bytes(event, modifiers, 31, KeyAction::Release).is_empty());
    }

    #[test]
    fn modifier_events_require_all_keys_and_use_post_event_state() {
        let event = EnhancedKeyEvent {
            key: EnhancedKey::Modifier(ModifierKey::LeftControl),
            text: None,
            shifted_key: None,
            base_layout_key: None,
        };
        assert!(
            bytes(
                event.clone(),
                Modifiers {
                    ctrl: true,
                    ..Default::default()
                },
                3,
                KeyAction::Press
            )
            .is_empty()
        );
        assert_eq!(
            bytes(
                event.clone(),
                Modifiers {
                    ctrl: true,
                    ..Default::default()
                },
                11,
                KeyAction::Press
            ),
            b"\x1b[57442;5u"
        );
        assert_eq!(
            bytes(event.clone(), Modifiers::default(), 11, KeyAction::Release),
            b"\x1b[57442;1:3u"
        );
        // Right Control is still held while Left Control releases.
        assert_eq!(
            bytes(
                event,
                Modifiers {
                    ctrl: true,
                    ..Default::default()
                },
                11,
                KeyAction::Release
            ),
            b"\x1b[57442;5:3u"
        );
    }

    #[test]
    fn all_modifier_bits_fit_without_overflow_and_locks_do_not_escape_plain_text() {
        let event: EnhancedKeyEvent = KeyEvent::character("a").into();
        let modifiers = EnhancedModifiers {
            ordinary: Modifiers {
                shift: true,
                alt: true,
                ctrl: true,
                cmd: true,
            },
            hyper: true,
            meta: true,
            caps_lock: true,
            num_lock: true,
        };
        assert_eq!(
            encode(
                &event,
                modifiers,
                TermInputModes::default(),
                8.try_into().unwrap(),
                KeyAction::Press
            )
            .unwrap(),
            b"\x1b[97;256u"
        );
        assert_eq!(
            encode(
                &event,
                EnhancedModifiers {
                    caps_lock: true,
                    num_lock: true,
                    ..Default::default()
                },
                TermInputModes::default(),
                1.try_into().unwrap(),
                KeyAction::Press
            )
            .unwrap(),
            b"a"
        );
    }

    #[test]
    fn invalid_logical_identity_is_not_silently_truncated() {
        for text in ["", "ab", "e\u{301}", "\x1b"] {
            assert_eq!(
                encode(
                    &KeyEvent::character(text).into(),
                    EnhancedModifiers::default(),
                    TermInputModes::default(),
                    8.try_into().unwrap(),
                    KeyAction::Press
                ),
                Err(EnhancedEncodingError::InvalidLogicalKey)
            );
        }
        for number in [0, 12, 36, 255] {
            let event = EnhancedKeyEvent {
                key: EnhancedKey::ExtendedFunction(number),
                text: None,
                shifted_key: None,
                base_layout_key: None,
            };
            assert_eq!(
                encode(
                    &event,
                    EnhancedModifiers::default(),
                    TermInputModes::default(),
                    8.try_into().unwrap(),
                    KeyAction::Press
                ),
                Err(EnhancedEncodingError::InvalidFunctionKey)
            );
        }
    }
}
