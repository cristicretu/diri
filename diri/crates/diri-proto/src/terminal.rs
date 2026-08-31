//! Shared terminal mode and input-wire types.
//!
//! These values cross the Holder, Engine, client, and renderer boundaries, so
//! keeping one representation prevents a terminal mode from being silently
//! weakened as it moves through the stack.

use serde::{Deserialize, Serialize};

/// The DEC private mouse tracking mode selected by the foreground program.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
#[repr(u8)]
pub enum MouseTrackingMode {
    #[default]
    Off = 0,
    /// DECSET 1000: button press/release events only.
    ButtonEvents = 1,
    /// DECSET 1002: button events plus motion while a button is held.
    ButtonMotion = 2,
    /// DECSET 1003: button events plus all pointer motion.
    AnyMotion = 3,
    /// A pre-1.4 peer reported only that some mouse mode was active.
    ///
    /// This is deliberately not assigned a wire-detail value: guessing one
    /// of 1000/1002/1003 would either drop requested motion or send motion a
    /// program never requested.
    Unknown = 4,
}

impl MouseTrackingMode {
    #[must_use]
    pub const fn from_wire(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Off),
            1 => Some(Self::ButtonEvents),
            2 => Some(Self::ButtonMotion),
            3 => Some(Self::AnyMotion),
            _ => None,
        }
    }

    #[must_use]
    pub const fn dec_private_mode(self) -> Option<u16> {
        match self {
            Self::Off => None,
            Self::ButtonEvents => Some(1000),
            Self::ButtonMotion => Some(1002),
            Self::AnyMotion => Some(1003),
            Self::Unknown => None,
        }
    }
}

/// Coordinate/button encoding selected independently with DECSET 1006.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
#[repr(u8)]
pub enum MouseEncoding {
    #[default]
    Legacy = 0,
    Sgr = 1,
    /// A pre-1.4 peer did not expose whether DECSET 1006 was active.
    Unknown = 2,
}

impl MouseEncoding {
    #[must_use]
    pub const fn from_wire(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Legacy),
            1 => Some(Self::Sgr),
            _ => None,
        }
    }
}

/// Complete mouse state: tracking and encoding are independent DEC modes.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MouseModes {
    pub tracking: MouseTrackingMode,
    pub encoding: MouseEncoding,
}

impl MouseModes {
    pub const OFF: Self = Self {
        tracking: MouseTrackingMode::Off,
        encoding: MouseEncoding::Legacy,
    };

    /// Some reporting mode is active, but a legacy peer did not preserve its
    /// tracking granularity or coordinate encoding.
    pub const UNKNOWN: Self = Self {
        tracking: MouseTrackingMode::Unknown,
        encoding: MouseEncoding::Unknown,
    };

    #[must_use]
    pub const fn new(tracking: MouseTrackingMode, encoding: MouseEncoding) -> Self {
        Self { tracking, encoding }
    }

    #[must_use]
    pub const fn is_reporting(self) -> bool {
        !matches!(self.tracking, MouseTrackingMode::Off)
    }

    /// Whether a desktop can safely synthesize button/motion reports.
    #[must_use]
    pub const fn has_known_details(self) -> bool {
        !matches!(self.tracking, MouseTrackingMode::Unknown)
            && !matches!(self.encoding, MouseEncoding::Unknown)
    }

    /// Packs the granular fields without assigning their position in a wider
    /// protocol mode byte.
    #[must_use]
    pub const fn detail_bits(self) -> u8 {
        match (self.tracking, self.encoding) {
            (MouseTrackingMode::Unknown, _) | (_, MouseEncoding::Unknown) => 0,
            _ => (self.tracking as u8) | ((self.encoding as u8) << 2),
        }
    }

    /// Decodes additive wire details. An enabled-only legacy value remains
    /// explicitly unknown: unlike the old checkpoint restore path, a live
    /// peer may really be using any tracking mode and either encoding.
    #[must_use]
    pub const fn from_detail_bits(bits: u8, historical_enabled: bool) -> Self {
        let tracking = match MouseTrackingMode::from_wire(bits & 0b11) {
            Some(MouseTrackingMode::Off) if historical_enabled => return Self::UNKNOWN,
            Some(mode) => mode,
            None => MouseTrackingMode::Off,
        };
        let encoding = if bits & 0b100 != 0 {
            MouseEncoding::Sgr
        } else {
            MouseEncoding::Legacy
        };
        Self { tracking, encoding }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TerminalMouseButton {
    Left,
    Middle,
    Right,
}

impl TerminalMouseButton {
    const fn code(self) -> u8 {
        match self {
            Self::Left => 0,
            Self::Middle => 1,
            Self::Right => 2,
        }
    }
}

/// Only modifiers defined by xterm mouse reporting. Command/Platform is
/// intentionally absent: the desktop reserves that gesture for local links.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TerminalMouseModifiers {
    pub shift: bool,
    pub alt: bool,
    pub control: bool,
}

impl TerminalMouseModifiers {
    fn bits(self) -> u8 {
        u8::from(self.shift) * 4 + u8::from(self.alt) * 8 + u8::from(self.control) * 16
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TerminalMouseEvent {
    Press(TerminalMouseButton),
    Release(TerminalMouseButton),
    Motion(Option<TerminalMouseButton>),
    WheelUp,
    WheelDown,
}

impl TerminalMouseEvent {
    const fn button_code(self) -> u8 {
        match self {
            Self::Press(button) | Self::Release(button) => button.code(),
            Self::Motion(Some(button)) => 32 + button.code(),
            Self::Motion(None) => 35,
            Self::WheelUp => 64,
            Self::WheelDown => 65,
        }
    }

    const fn is_release(self) -> bool {
        matches!(self, Self::Release(_))
    }
}

/// Encodes one xterm mouse report at a zero-based grid coordinate.
///
/// The caller clamps coordinates to its authoritative grid. Legacy X10
/// coordinates have no representation beyond the 223rd row/column, so those
/// reports are omitted instead of lying about which cell was targeted.
#[must_use]
pub fn encode_mouse_event(
    modes: MouseModes,
    event: TerminalMouseEvent,
    modifiers: TerminalMouseModifiers,
    col: u16,
    row: u16,
) -> Option<Vec<u8>> {
    match (modes.tracking, event) {
        (MouseTrackingMode::Off, _) => return None,
        (MouseTrackingMode::Unknown, _) => return None,
        (MouseTrackingMode::ButtonEvents, TerminalMouseEvent::Motion(_)) => return None,
        (MouseTrackingMode::ButtonMotion, TerminalMouseEvent::Motion(None)) => return None,
        _ => {}
    }

    let modifiers = modifiers.bits();
    match modes.encoding {
        MouseEncoding::Sgr => {
            let final_byte = if event.is_release() { 'm' } else { 'M' };
            Some(
                format!(
                    "\x1b[<{};{};{}{final_byte}",
                    event.button_code() + modifiers,
                    u32::from(col) + 1,
                    u32::from(row) + 1,
                )
                .into_bytes(),
            )
        }
        MouseEncoding::Legacy => {
            if col >= 223 || row >= 223 {
                return None;
            }
            let button = if event.is_release() {
                3 + modifiers
            } else {
                event.button_code() + modifiers
            };
            Some(vec![
                0x1b,
                b'[',
                b'M',
                32 + button,
                33 + u8::try_from(col).expect("legacy coordinate checked"),
                33 + u8::try_from(row).expect("legacy coordinate checked"),
            ])
        }
        MouseEncoding::Unknown => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LEGACY_BUTTONS: MouseModes =
        MouseModes::new(MouseTrackingMode::ButtonEvents, MouseEncoding::Legacy);
    const SGR_MOTION: MouseModes =
        MouseModes::new(MouseTrackingMode::AnyMotion, MouseEncoding::Sgr);

    #[test]
    fn tracking_modes_filter_motion_at_the_requested_granularity() {
        let held = TerminalMouseEvent::Motion(Some(TerminalMouseButton::Left));
        let free = TerminalMouseEvent::Motion(None);
        let modifiers = TerminalMouseModifiers::default();

        assert!(encode_mouse_event(MouseModes::OFF, held, modifiers, 0, 0).is_none());
        assert!(encode_mouse_event(LEGACY_BUTTONS, held, modifiers, 0, 0).is_none());
        let drag = MouseModes::new(MouseTrackingMode::ButtonMotion, MouseEncoding::Legacy);
        assert!(encode_mouse_event(drag, held, modifiers, 0, 0).is_some());
        assert!(encode_mouse_event(drag, free, modifiers, 0, 0).is_none());
        assert!(encode_mouse_event(SGR_MOTION, held, modifiers, 0, 0).is_some());
        assert!(encode_mouse_event(SGR_MOTION, free, modifiers, 0, 0).is_some());
    }

    #[test]
    fn legacy_encodes_each_button_release_and_modifier_bit() {
        let modifiers = TerminalMouseModifiers {
            shift: true,
            alt: true,
            control: true,
        };
        for (button, code) in [
            (TerminalMouseButton::Left, 0),
            (TerminalMouseButton::Middle, 1),
            (TerminalMouseButton::Right, 2),
        ] {
            assert_eq!(
                encode_mouse_event(
                    LEGACY_BUTTONS,
                    TerminalMouseEvent::Press(button),
                    modifiers,
                    4,
                    7
                ),
                Some(vec![0x1b, b'[', b'M', 32 + code + 28, 37, 40])
            );
            assert_eq!(
                encode_mouse_event(
                    LEGACY_BUTTONS,
                    TerminalMouseEvent::Release(button),
                    modifiers,
                    4,
                    7
                ),
                Some(vec![0x1b, b'[', b'M', 32 + 3 + 28, 37, 40])
            );
        }
    }

    #[test]
    fn sgr_preserves_large_coordinates_and_release_button_identity() {
        assert_eq!(
            encode_mouse_event(
                SGR_MOTION,
                TerminalMouseEvent::Press(TerminalMouseButton::Right),
                TerminalMouseModifiers::default(),
                500,
                300,
            ),
            Some(b"\x1b[<2;501;301M".to_vec())
        );
        assert_eq!(
            encode_mouse_event(
                SGR_MOTION,
                TerminalMouseEvent::Release(TerminalMouseButton::Middle),
                TerminalMouseModifiers::default(),
                500,
                300,
            ),
            Some(b"\x1b[<1;501;301m".to_vec())
        );
        assert!(
            encode_mouse_event(
                LEGACY_BUTTONS,
                TerminalMouseEvent::Press(TerminalMouseButton::Left),
                TerminalMouseModifiers::default(),
                223,
                0,
            )
            .is_none()
        );
    }

    #[test]
    fn motion_reports_encode_held_button_no_button_and_sgr_modifiers() {
        let modifiers = TerminalMouseModifiers {
            shift: true,
            alt: false,
            control: true,
        };
        assert_eq!(
            encode_mouse_event(
                SGR_MOTION,
                TerminalMouseEvent::Motion(Some(TerminalMouseButton::Left)),
                modifiers,
                9,
                4,
            ),
            Some(b"\x1b[<52;10;5M".to_vec())
        );
        assert_eq!(
            encode_mouse_event(
                SGR_MOTION,
                TerminalMouseEvent::Motion(None),
                TerminalMouseModifiers {
                    alt: true,
                    ..TerminalMouseModifiers::default()
                },
                9,
                4,
            ),
            Some(b"\x1b[<43;10;5M".to_vec())
        );
    }

    #[test]
    fn legacy_enabled_only_wire_state_keeps_its_details_unknown() {
        assert_eq!(MouseModes::from_detail_bits(0, true), MouseModes::UNKNOWN);
        assert!(
            encode_mouse_event(
                MouseModes::UNKNOWN,
                TerminalMouseEvent::Press(TerminalMouseButton::Left),
                TerminalMouseModifiers::default(),
                0,
                0,
            )
            .is_none(),
            "unknown legacy modes must not be guessed for app-generated input"
        );
        assert_eq!(MouseModes::from_detail_bits(0, false), MouseModes::OFF);
    }
}
