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

/// The pixel size of one cell in the coordinates a program is told about:
/// the PTY's `ws_xpixel`/`ws_ypixel`, the `CSI 14 t`/`16 t` replies and
/// SGR-Pixels mouse reports (DECSET 1016) all use it, so a program that
/// divides one by another lands on the cell the pointer is over whatever
/// the renderer's real font size is.
pub const NOMINAL_CELL_PIXELS: (u16, u16) = (16, 34);

/// Coordinate/button encoding selected with DECSET 1005/1006/1016.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
#[repr(u8)]
pub enum MouseEncoding {
    #[default]
    Legacy = 0,
    Sgr = 1,
    /// A pre-1.4 peer did not expose whether DECSET 1006 was active.
    Unknown = 2,
    /// DECSET 1016: SGR reports whose coordinates are zero-based pixels in
    /// [`NOMINAL_CELL_PIXELS`] cells. On wires that predate it, it travels
    /// as [`Self::Sgr`] plus a separate flag that older peers ignore.
    SgrPixels = 3,
}

impl MouseEncoding {
    #[must_use]
    pub const fn from_wire(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Legacy),
            1 => Some(Self::Sgr),
            3 => Some(Self::SgrPixels),
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
            // SGR-Pixels shares the SGR bit: a peer that predates it then
            // still sends SGR reports, as it did before DECSET 1016 was
            // recognized. [`Self::pixel_reports`] carries the difference.
            _ => {
                (self.tracking as u8)
                    | ((matches!(self.encoding, MouseEncoding::Sgr | MouseEncoding::SgrPixels)
                        as u8)
                        << 2)
            }
        }
    }

    /// Whether reports carry pixel coordinates (DECSET 1016). Wires carry
    /// this beside [`Self::detail_bits`] in a bit older peers ignore.
    #[must_use]
    pub const fn pixel_reports(self) -> bool {
        matches!(self.encoding, MouseEncoding::SgrPixels)
    }

    /// Applies the separately carried [`Self::pixel_reports`] flag to modes
    /// decoded from [`Self::detail_bits`]. Only an SGR encoding can be one.
    #[must_use]
    pub const fn with_pixel_reports(mut self, pixels: bool) -> Self {
        if pixels && matches!(self.encoding, MouseEncoding::Sgr) {
            self.encoding = MouseEncoding::SgrPixels;
        }
        self
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
    /// Horizontal wheel (xterm buttons 6 and 7, reported as 66 and 67).
    WheelLeft,
    WheelRight,
}

impl TerminalMouseEvent {
    const fn button_code(self) -> u8 {
        match self {
            Self::Press(button) | Self::Release(button) => button.code(),
            Self::Motion(Some(button)) => 32 + button.code(),
            Self::Motion(None) => 35,
            Self::WheelUp => 64,
            Self::WheelDown => 65,
            Self::WheelLeft => 66,
            Self::WheelRight => 67,
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
/// SGR-Pixels reports give the cell's top-left pixel; a caller that knows
/// where in the cell the pointer is uses [`encode_mouse_event_at`].
#[must_use]
pub fn encode_mouse_event(
    modes: MouseModes,
    event: TerminalMouseEvent,
    modifiers: TerminalMouseModifiers,
    col: u16,
    row: u16,
) -> Option<Vec<u8>> {
    encode_mouse_event_at(
        modes,
        event,
        modifiers,
        col,
        row,
        cell_origin_pixels(col, row),
    )
}

/// The zero-based pixel of a cell's top-left corner in
/// [`NOMINAL_CELL_PIXELS`] cells.
#[must_use]
pub const fn cell_origin_pixels(col: u16, row: u16) -> (u32, u32) {
    (
        col as u32 * NOMINAL_CELL_PIXELS.0 as u32,
        row as u32 * NOMINAL_CELL_PIXELS.1 as u32,
    )
}

/// The zero-based pixel under a pointer at fractional grid position
/// (`x` columns, `y` rows from the grid's top-left), clamped into a
/// `cols`×`rows` grid, in [`NOMINAL_CELL_PIXELS`] cells.
#[must_use]
pub fn grid_position_pixels(x: f32, y: f32, cols: u16, rows: u16) -> (u32, u32) {
    let axis = |position: f32, cells: u16, cell: u16| {
        let limit = u32::from(cells.max(1)) * u32::from(cell) - 1;
        let pixel = (position.max(0.0) * f32::from(cell)).floor();
        // `as` saturates a float beyond u32; the clamp keeps it in the grid.
        (pixel as u32).min(limit)
    };
    (
        axis(x, cols, NOMINAL_CELL_PIXELS.0),
        axis(y, rows, NOMINAL_CELL_PIXELS.1),
    )
}

/// [`encode_mouse_event`] with the pointer's pixel inside the grid, which an
/// SGR-Pixels report (DECSET 1016) carries instead of the cell.
#[must_use]
pub fn encode_mouse_event_at(
    modes: MouseModes,
    event: TerminalMouseEvent,
    modifiers: TerminalMouseModifiers,
    col: u16,
    row: u16,
    pixel: (u32, u32),
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
        MouseEncoding::SgrPixels => {
            let final_byte = if event.is_release() { 'm' } else { 'M' };
            Some(
                format!(
                    "\x1b[<{};{};{}{final_byte}",
                    event.button_code() + modifiers,
                    pixel.0,
                    pixel.1,
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

/// Encodes `count` identical wheel reports (one per scrolled line), the way
/// xterm reports each wheel step as its own button press. Empty when the
/// program has not asked for reports or the coordinate cannot be encoded.
#[must_use]
pub fn encode_mouse_wheel(
    modes: MouseModes,
    event: TerminalMouseEvent,
    modifiers: TerminalMouseModifiers,
    col: u16,
    row: u16,
    count: usize,
) -> Vec<u8> {
    encode_mouse_wheel_at(
        modes,
        event,
        modifiers,
        col,
        row,
        cell_origin_pixels(col, row),
        count,
    )
}

/// [`encode_mouse_wheel`] with the pointer's pixel for SGR-Pixels reports.
#[must_use]
pub fn encode_mouse_wheel_at(
    modes: MouseModes,
    event: TerminalMouseEvent,
    modifiers: TerminalMouseModifiers,
    col: u16,
    row: u16,
    pixel: (u32, u32),
    count: usize,
) -> Vec<u8> {
    let Some(report) = encode_mouse_event_at(modes, event, modifiers, col, row, pixel) else {
        return Vec::new();
    };
    report.repeat(count)
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

    /// Bytes a reference xterm-compatible emulator sent a program reading
    /// its raw input, for the same events, in a 320x90 grid.
    #[test]
    fn reports_match_the_reference_bytes_for_each_supported_mode() {
        use TerminalMouseButton::{Left, Middle, Right};
        use TerminalMouseEvent::{
            Motion, Press, Release, WheelDown, WheelLeft, WheelRight, WheelUp,
        };
        let none = TerminalMouseModifiers::default();
        let ctrl = TerminalMouseModifiers {
            control: true,
            ..none
        };
        let alt = TerminalMouseModifiers { alt: true, ..none };
        let shift = TerminalMouseModifiers {
            shift: true,
            ..none
        };
        let all = TerminalMouseModifiers {
            shift: true,
            alt: true,
            control: true,
        };
        let legacy = MouseModes::new(MouseTrackingMode::ButtonEvents, MouseEncoding::Legacy);
        let sgr = MouseModes::new(MouseTrackingMode::ButtonEvents, MouseEncoding::Sgr);
        let drag_sgr = MouseModes::new(MouseTrackingMode::ButtonMotion, MouseEncoding::Sgr);
        let any_sgr = MouseModes::new(MouseTrackingMode::AnyMotion, MouseEncoding::Sgr);
        let any_legacy = MouseModes::new(MouseTrackingMode::AnyMotion, MouseEncoding::Legacy);
        type Case = (
            MouseModes,
            TerminalMouseEvent,
            TerminalMouseModifiers,
            u16,
            u16,
            &'static [u8],
        );
        let cases: &[Case] = &[
            (sgr, Press(Left), none, 3, 4, b"\x1b[<0;4;5M"),
            (sgr, Release(Left), none, 3, 4, b"\x1b[<0;4;5m"),
            (sgr, Press(Middle), none, 3, 4, b"\x1b[<1;4;5M"),
            (sgr, Release(Right), none, 3, 4, b"\x1b[<2;4;5m"),
            (sgr, Press(Middle), ctrl, 3, 4, b"\x1b[<17;4;5M"),
            (sgr, Release(Middle), ctrl, 3, 4, b"\x1b[<17;4;5m"),
            (sgr, Press(Middle), alt, 3, 4, b"\x1b[<9;4;5M"),
            (sgr, Press(Middle), all, 3, 4, b"\x1b[<29;4;5M"),
            (sgr, Press(Right), shift, 3, 4, b"\x1b[<6;4;5M"),
            (sgr, WheelUp, none, 3, 4, b"\x1b[<64;4;5M"),
            (sgr, WheelDown, none, 3, 4, b"\x1b[<65;4;5M"),
            (sgr, WheelLeft, none, 3, 4, b"\x1b[<66;4;5M"),
            (sgr, WheelRight, none, 3, 4, b"\x1b[<67;4;5M"),
            (sgr, WheelDown, ctrl, 3, 4, b"\x1b[<81;4;5M"),
            (sgr, WheelUp, shift, 3, 4, b"\x1b[<68;4;5M"),
            (sgr, WheelDown, alt, 3, 4, b"\x1b[<73;4;5M"),
            (sgr, Press(Middle), none, 0, 0, b"\x1b[<1;1;1M"),
            (sgr, Press(Middle), none, 223, 0, b"\x1b[<1;224;1M"),
            (sgr, Release(Middle), none, 300, 89, b"\x1b[<1;301;90m"),
            (sgr, WheelDown, none, 300, 89, b"\x1b[<65;301;90M"),
            (drag_sgr, Motion(Some(Left)), none, 8, 8, b"\x1b[<32;9;9M"),
            (drag_sgr, Motion(Some(Left)), alt, 9, 8, b"\x1b[<40;10;9M"),
            (any_sgr, Motion(None), none, 7, 8, b"\x1b[<35;8;9M"),
            (any_sgr, Motion(Some(Right)), none, 9, 8, b"\x1b[<34;10;9M"),
            (legacy, Press(Left), none, 3, 4, b"\x1b[M $%"),
            (legacy, Release(Left), none, 3, 4, b"\x1b[M#$%"),
            (legacy, Press(Middle), ctrl, 3, 4, b"\x1b[M1$%"),
            (legacy, Release(Middle), ctrl, 3, 4, b"\x1b[M3$%"),
            (legacy, Press(Middle), all, 3, 4, b"\x1b[M=$%"),
            (legacy, Release(Middle), all, 3, 4, b"\x1b[M?$%"),
            (legacy, Release(Right), shift, 3, 4, b"\x1b[M'$%"),
            (legacy, WheelUp, none, 3, 4, b"\x1b[M`$%"),
            (legacy, WheelLeft, none, 3, 4, b"\x1b[Mb$%"),
            (legacy, WheelRight, none, 3, 4, b"\x1b[Mc$%"),
            (legacy, WheelDown, ctrl, 3, 4, b"\x1b[Mq$%"),
            (legacy, WheelUp, shift, 3, 4, b"\x1b[Md$%"),
            (legacy, WheelDown, alt, 3, 4, b"\x1b[Mi$%"),
            (legacy, Press(Middle), none, 94, 0, b"\x1b[M!\x7f!"),
            (legacy, Press(Middle), none, 95, 0, b"\x1b[M!\x80!"),
            (legacy, Press(Middle), none, 222, 0, b"\x1b[M!\xff!"),
            (any_legacy, Motion(None), none, 7, 8, b"\x1b[MC()"),
            (any_legacy, Motion(Some(Left)), alt, 9, 8, b"\x1b[MH*)"),
        ];
        for &(modes, event, modifiers, col, row, expected) in cases {
            assert_eq!(
                encode_mouse_event(modes, event, modifiers, col, row).as_deref(),
                Some(expected),
                "{modes:?} {event:?} {modifiers:?} at {col},{row}"
            );
        }
        // Neither emulator can name a column past 223 in the legacy encoding.
        for (col, row) in [(223, 0), (300, 89), (0, 223)] {
            assert_eq!(
                encode_mouse_event(legacy, Press(Middle), none, col, row),
                None
            );
        }
        assert_eq!(
            encode_mouse_wheel(sgr, WheelLeft, ctrl, 3, 4, 2),
            b"\x1b[<82;4;5M\x1b[<82;4;5M".to_vec()
        );
        assert!(encode_mouse_wheel(MouseModes::OFF, WheelUp, none, 3, 4, 2).is_empty());
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

    #[test]
    fn sgr_pixels_reports_the_pointer_pixel_and_release_final() {
        let pixels = MouseModes::new(MouseTrackingMode::AnyMotion, MouseEncoding::SgrPixels);
        let modifiers = TerminalMouseModifiers {
            shift: true,
            ..Default::default()
        };
        let left = TerminalMouseButton::Left;
        assert_eq!(
            encode_mouse_event_at(
                pixels,
                TerminalMouseEvent::Press(left),
                modifiers,
                2,
                1,
                (41, 50)
            ),
            Some(b"\x1b[<4;41;50M".to_vec())
        );
        assert_eq!(
            encode_mouse_event_at(
                pixels,
                TerminalMouseEvent::Release(left),
                TerminalMouseModifiers::default(),
                2,
                1,
                (41, 50)
            ),
            Some(b"\x1b[<0;41;50m".to_vec())
        );
        // Without a pointer pixel, a report names the cell's top-left pixel.
        assert_eq!(
            encode_mouse_event(
                pixels,
                TerminalMouseEvent::WheelUp,
                TerminalMouseModifiers::default(),
                3,
                2
            ),
            Some(b"\x1b[<64;48;68M".to_vec())
        );
        // A cell report ignores the pixel.
        let sgr = MouseModes::new(MouseTrackingMode::AnyMotion, MouseEncoding::Sgr);
        assert_eq!(
            encode_mouse_event_at(
                sgr,
                TerminalMouseEvent::Press(left),
                TerminalMouseModifiers::default(),
                2,
                1,
                (41, 50)
            ),
            Some(b"\x1b[<0;3;2M".to_vec())
        );
    }

    #[test]
    fn grid_positions_become_nominal_pixels_clamped_into_the_grid() {
        assert_eq!(grid_position_pixels(0.0, 0.0, 80, 24), (0, 0));
        assert_eq!(grid_position_pixels(2.5, 1.5, 80, 24), (40, 51));
        assert_eq!(grid_position_pixels(-3.0, -1.0, 80, 24), (0, 0));
        assert_eq!(
            grid_position_pixels(500.0, 99.0, 80, 24),
            (80 * 16 - 1, 24 * 34 - 1)
        );
    }

    #[test]
    fn sgr_pixels_travels_as_sgr_plus_a_flag_older_peers_ignore() {
        let pixels = MouseModes::new(MouseTrackingMode::ButtonMotion, MouseEncoding::SgrPixels);
        let sgr = MouseModes::new(MouseTrackingMode::ButtonMotion, MouseEncoding::Sgr);
        assert_eq!(pixels.detail_bits(), sgr.detail_bits());
        assert!(pixels.pixel_reports());
        assert!(!sgr.pixel_reports());
        let decoded = MouseModes::from_detail_bits(pixels.detail_bits(), true);
        assert_eq!(decoded, sgr);
        assert_eq!(decoded.with_pixel_reports(true), pixels);
        let legacy = MouseModes::new(MouseTrackingMode::ButtonMotion, MouseEncoding::Legacy);
        assert_eq!(legacy.with_pixel_reports(true), legacy);
    }
}
