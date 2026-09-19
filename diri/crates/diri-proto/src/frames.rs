//! Binary data-channel frames.
//!
//! This is the Rust counterpart of `Sources/DirijorProtocol/Frames.swift`.
//! Each frame is `[type u8][payload length u32 BE][payload]`.

use std::error::Error;
use std::fmt;

use crate::grid::{GridCodecError, GridUpdate};
use crate::terminal::MouseModes;

/// Bit of a Modes frame's first byte that reports secret input.
const MODES_SECRET_INPUT: u8 = 1 << 6;

/// A single frame larger than this indicates a corrupt stream.
pub const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum FrameType {
    Output = 1,
    Input = 2,
    Resize = 3,
    ReplayBegin = 4,
    ReplayEnd = 5,
    Ping = 6,
    Pong = 7,
    Grid = 8,
    Scroll = 9,
    Modes = 10,
    /// Pre-encoded terminal mouse report. Kept distinct from `Input` so
    /// status/prompt reducers never mistake escape sequences for typed text.
    Mouse = 11,
}

impl TryFrom<u8> for FrameType {
    type Error = FrameCodecError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::Output),
            2 => Ok(Self::Input),
            3 => Ok(Self::Resize),
            4 => Ok(Self::ReplayBegin),
            5 => Ok(Self::ReplayEnd),
            6 => Ok(Self::Ping),
            7 => Ok(Self::Pong),
            8 => Ok(Self::Grid),
            9 => Ok(Self::Scroll),
            10 => Ok(Self::Modes),
            11 => Ok(Self::Mouse),
            other => Err(FrameCodecError::UnknownFrameType(other)),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Frame {
    pub frame_type: FrameType,
    pub payload: Vec<u8>,
}

impl Frame {
    #[must_use]
    pub fn new(frame_type: FrameType, payload: Vec<u8>) -> Self {
        Self {
            frame_type,
            payload,
        }
    }

    #[must_use]
    pub fn output(offset: u64, bytes: &[u8]) -> Self {
        let mut payload = Vec::with_capacity(8 + bytes.len());
        payload.extend_from_slice(&offset.to_be_bytes());
        payload.extend_from_slice(bytes);
        Self::new(FrameType::Output, payload)
    }

    #[must_use]
    pub fn input(bytes: impl Into<Vec<u8>>) -> Self {
        Self::new(FrameType::Input, bytes.into())
    }

    #[must_use]
    pub fn mouse(bytes: impl Into<Vec<u8>>) -> Self {
        Self::new(FrameType::Mouse, bytes.into())
    }

    #[must_use]
    pub fn resize(cols: u16, rows: u16) -> Self {
        let mut payload = Vec::with_capacity(4);
        payload.extend_from_slice(&cols.to_be_bytes());
        payload.extend_from_slice(&rows.to_be_bytes());
        Self::new(FrameType::Resize, payload)
    }

    #[must_use]
    pub fn replay_begin(offset: u64) -> Self {
        Self::offset_frame(FrameType::ReplayBegin, offset)
    }

    #[must_use]
    pub fn replay_end(offset: u64) -> Self {
        Self::offset_frame(FrameType::ReplayEnd, offset)
    }

    #[must_use]
    pub fn ping() -> Self {
        Self::new(FrameType::Ping, Vec::new())
    }

    #[must_use]
    pub fn pong() -> Self {
        Self::new(FrameType::Pong, Vec::new())
    }

    pub fn grid(update: &GridUpdate) -> Result<Self, GridCodecError> {
        Ok(Self::new(FrameType::Grid, update.encode()?))
    }

    /// `direction` is `0` for up and `1` for down.
    #[must_use]
    pub fn scroll(direction: u8, lines: u16, col: u16, row: u16) -> Self {
        let mut payload = Vec::with_capacity(7);
        payload.push(direction);
        payload.extend_from_slice(&lines.to_be_bytes());
        payload.extend_from_slice(&col.to_be_bytes());
        payload.extend_from_slice(&row.to_be_bytes());
        Self::new(FrameType::Scroll, payload)
    }

    #[must_use]
    pub fn modes(alt_screen: bool, mouse: MouseModes) -> Self {
        Self::modes_with_bracketed_paste(alt_screen, false, mouse)
    }

    /// Builds the additive terminal-mode payload. [`Self::modes`] remains the
    /// source-compatible constructor for peers that only know screen and
    /// mouse state.
    #[must_use]
    pub fn modes_with_bracketed_paste(
        alt_screen: bool,
        bracketed_paste: bool,
        mouse: MouseModes,
    ) -> Self {
        // Bit 1 stays the historical "any mouse reporting" flag so older
        // clients continue to route the pointer to the terminal. Granular
        // tracking, encoding, and bracketed paste live in previously unused
        // bits. Old clients ignore bit 5; new clients decode it as false from
        // every frame emitted before this extension.
        let bits = u8::from(alt_screen)
            | (u8::from(mouse.is_reporting()) << 1)
            | (mouse.detail_bits() << 2)
            | (u8::from(bracketed_paste) << 5);
        Self::new(FrameType::Modes, vec![bits])
    }

    /// Marks a Modes frame as sent while the child reads a secret: a line
    /// prompt with echo off, as `sudo` and `ssh` use. Bit 6 was unused, so a
    /// client that predates it ignores the bit and one that knows it reads
    /// every older frame as "not secret".
    #[must_use]
    pub fn with_secret_input(mut self, secret_input: bool) -> Self {
        if self.frame_type == FrameType::Modes
            && let Some(bits) = self.payload.first_mut()
        {
            *bits = (*bits & !MODES_SECRET_INPUT) | (u8::from(secret_input) << 6);
        }
        self
    }

    /// Whether this Modes frame reports secret input. `None` for any other
    /// frame; an absent bit is `false`, which is also the fail-safe reading.
    #[must_use]
    pub fn secret_input_payload(&self) -> Option<bool> {
        if self.frame_type != FrameType::Modes {
            return None;
        }
        self.payload
            .first()
            .map(|bits| bits & MODES_SECRET_INPUT != 0)
    }

    pub fn grid_payload(&self) -> Result<Option<GridUpdate>, GridCodecError> {
        if self.frame_type != FrameType::Grid {
            return Ok(None);
        }
        GridUpdate::decode(&self.payload).map(Some)
    }

    #[must_use]
    pub fn output_payload(&self) -> Option<(u64, &[u8])> {
        if self.frame_type != FrameType::Output || self.payload.len() < 8 {
            return None;
        }
        let offset = u64::from_be_bytes(self.payload[..8].try_into().expect("length checked"));
        Some((offset, &self.payload[8..]))
    }

    #[must_use]
    pub fn resize_payload(&self) -> Option<(u16, u16)> {
        if self.frame_type != FrameType::Resize || self.payload.len() < 4 {
            return None;
        }
        Some((read_u16(&self.payload, 0), read_u16(&self.payload, 2)))
    }

    #[must_use]
    pub fn offset_payload(&self) -> Option<u64> {
        if !matches!(
            self.frame_type,
            FrameType::ReplayBegin | FrameType::ReplayEnd
        ) || self.payload.len() < 8
        {
            return None;
        }
        Some(u64::from_be_bytes(
            self.payload[..8].try_into().expect("length checked"),
        ))
    }

    #[must_use]
    pub fn scroll_payload(&self) -> Option<(u8, u16, u16, u16)> {
        if self.frame_type != FrameType::Scroll || self.payload.len() < 7 {
            return None;
        }
        Some((
            self.payload[0],
            read_u16(&self.payload, 1),
            read_u16(&self.payload, 3),
            read_u16(&self.payload, 5),
        ))
    }

    /// Add keyboard state to the historical Modes payload. Older readers use
    /// only byte zero; absence of the versioned tail means unknown state.
    #[must_use]
    pub fn modes_with_keyboard(
        alt_screen: bool,
        bracketed_paste: bool,
        mouse: MouseModes,
        keyboard: Option<crate::terminal_input::KeyboardState>,
    ) -> Self {
        let mut frame = Self::modes_with_bracketed_paste(alt_screen, bracketed_paste, mouse);
        if let Some(keyboard) = keyboard {
            frame.payload.extend_from_slice(&[
                1, // keyboard-state extension version
                u8::from(keyboard.application_cursor_keys)
                    | (u8::from(keyboard.application_keypad) << 1),
            ]);
        }
        frame
    }

    /// Version 2 is emitted only for a consumer that explicitly negotiated it.
    /// Older peers retain the exact version-1 bytes and unknown enhancements.
    #[must_use]
    pub fn modes_with_keyboard_capability(
        alt_screen: bool,
        bracketed_paste: bool,
        mouse: MouseModes,
        keyboard: Option<crate::terminal_input::KeyboardState>,
        enhanced_keyboard: bool,
    ) -> Self {
        let mut frame = Self::modes_with_keyboard(alt_screen, bracketed_paste, mouse, keyboard);
        if enhanced_keyboard && let Some(flags) = keyboard.and_then(|state| state.enhancements) {
            frame.payload[1] = 2;
            frame.payload.push(flags.bits());
        }
        frame
    }

    pub fn keyboard_state_payload(
        &self,
    ) -> Result<Option<crate::terminal_input::KeyboardState>, &'static str> {
        if self.frame_type != FrameType::Modes || self.payload.is_empty() {
            return Err("not a valid Modes frame");
        }
        match self.payload.as_slice() {
            [_] => Ok(None),
            [_, 1, bits] if bits & !3 == 0 => Ok(Some(crate::terminal_input::KeyboardState {
                enhancements: None,
                application_cursor_keys: bits & 1 != 0,
                application_keypad: bits & 2 != 0,
            })),
            [_, 2, bits, flags] if bits & !3 == 0 => {
                Ok(Some(crate::terminal_input::KeyboardState {
                    application_cursor_keys: bits & 1 != 0,
                    application_keypad: bits & 2 != 0,
                    enhancements: Some((*flags).try_into()?),
                }))
            }
            _ => Err("unsupported keyboard-state extension"),
        }
    }

    #[must_use]
    pub fn modes_payload(&self) -> Option<(bool, MouseModes)> {
        self.terminal_modes_payload()
            .map(|(alt_screen, _, mouse)| (alt_screen, mouse))
    }

    /// Decodes all currently defined mode bits. [`Self::modes_payload`]
    /// retains its original two-field API for existing consumers.
    #[must_use]
    pub fn terminal_modes_payload(&self) -> Option<(bool, bool, MouseModes)> {
        if self.frame_type != FrameType::Modes {
            return None;
        }
        self.payload.first().map(|bits| {
            (
                bits & 1 != 0,
                bits & (1 << 5) != 0,
                MouseModes::from_detail_bits(bits >> 2, bits & 2 != 0),
            )
        })
    }

    fn offset_frame(frame_type: FrameType, offset: u64) -> Self {
        Self::new(frame_type, offset.to_be_bytes().to_vec())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FrameCodecError {
    UnknownFrameType(u8),
    FrameTooLarge { length: usize, max: usize },
    PayloadLengthOverflow(usize),
}

impl fmt::Display for FrameCodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownFrameType(frame_type) => {
                write!(f, "unknown frame type {frame_type}")
            }
            Self::FrameTooLarge { length, max } => {
                write!(f, "frame payload is {length} bytes; maximum is {max}")
            }
            Self::PayloadLengthOverflow(length) => {
                write!(f, "frame payload length {length} does not fit in u32")
            }
        }
    }
}

impl Error for FrameCodecError {}

/// Incremental decoder for arbitrary data-channel chunks.
#[derive(Clone, Debug, Default)]
pub struct FrameCodec {
    buffer: Vec<u8>,
    start: usize,
}

impl FrameCodec {
    pub const MAX_FRAME_BYTES: usize = MAX_FRAME_BYTES;

    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn encode(frame: &Frame) -> Result<Vec<u8>, FrameCodecError> {
        let length = frame.payload.len();
        if length > MAX_FRAME_BYTES {
            return Err(FrameCodecError::FrameTooLarge {
                length,
                max: MAX_FRAME_BYTES,
            });
        }
        let length = u32::try_from(length)
            .map_err(|_| FrameCodecError::PayloadLengthOverflow(frame.payload.len()))?;
        let mut encoded = Vec::with_capacity(5 + frame.payload.len());
        encoded.push(frame.frame_type as u8);
        encoded.extend_from_slice(&length.to_be_bytes());
        encoded.extend_from_slice(&frame.payload);
        Ok(encoded)
    }

    /// Appends bytes and returns every complete frame now available.
    pub fn feed(&mut self, bytes: &[u8]) -> Result<Vec<Frame>, FrameCodecError> {
        self.buffer.extend_from_slice(bytes);
        let mut frames = Vec::new();
        let mut consumed = self.start;

        while self.buffer.len().saturating_sub(consumed) >= 5 {
            let frame_type = FrameType::try_from(self.buffer[consumed])?;
            let length = u32::from_be_bytes(
                self.buffer[consumed + 1..consumed + 5]
                    .try_into()
                    .expect("header length checked"),
            ) as usize;
            if length > MAX_FRAME_BYTES {
                return Err(FrameCodecError::FrameTooLarge {
                    length,
                    max: MAX_FRAME_BYTES,
                });
            }
            let frame_end = consumed + 5 + length;
            if self.buffer.len() < frame_end {
                break;
            }
            frames.push(Frame::new(
                frame_type,
                self.buffer[consumed + 5..frame_end].to_vec(),
            ));
            consumed = frame_end;
        }

        self.start = consumed;
        if self.start == self.buffer.len() {
            self.buffer.clear();
            self.start = 0;
        } else if self.start >= 64 * 1024 && self.start >= self.buffer.len() / 2 {
            self.buffer.copy_within(self.start.., 0);
            self.buffer.truncate(self.buffer.len() - self.start);
            self.start = 0;
        }
        Ok(frames)
    }

    /// Swift calls this operation `append`; keep the same spelling available.
    pub fn append(&mut self, bytes: &[u8]) -> Result<Vec<Frame>, FrameCodecError> {
        self.feed(bytes)
    }

    #[must_use]
    pub fn buffered_len(&self) -> usize {
        self.buffer.len() - self.start
    }
}

fn read_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_be_bytes(
        bytes[offset..offset + 2]
            .try_into()
            .expect("caller checked payload length"),
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn enhanced_keyboard_tail_requires_opt_in_and_preserves_legacy_bytes() {
        use crate::terminal_input::KeyboardState;
        for flags in 0..=31 {
            let state = KeyboardState {
                application_cursor_keys: true,
                application_keypad: true,
                enhancements: Some(flags.try_into().unwrap()),
            };
            let legacy = Frame::modes_with_keyboard_capability(
                false,
                false,
                MouseModes::OFF,
                Some(state),
                false,
            );
            assert_eq!(legacy.payload, vec![0, 1, 3]);
            assert_eq!(
                legacy.keyboard_state_payload().unwrap(),
                Some(state.legacy_projection())
            );
            let capable = Frame::modes_with_keyboard_capability(
                false,
                false,
                MouseModes::OFF,
                Some(state),
                true,
            );
            assert_eq!(capable.payload, vec![0, 2, 3, flags]);
            assert_eq!(capable.keyboard_state_payload().unwrap(), Some(state));
        }
        let mut frame = Frame::modes_with_keyboard_capability(
            false,
            false,
            MouseModes::OFF,
            Some(KeyboardState {
                enhancements: Some(Default::default()),
                ..Default::default()
            }),
            true,
        );
        frame.payload[3] = 32;
        assert!(frame.keyboard_state_payload().is_err());
        frame.payload = vec![0, 2, 0];
        assert!(frame.keyboard_state_payload().is_err());
        let old = Frame::modes_with_keyboard_capability(
            false,
            false,
            MouseModes::OFF,
            Some(KeyboardState::default()),
            true,
        );
        assert_eq!(
            old.keyboard_state_payload().unwrap().unwrap().enhancements,
            None
        );
    }

    #[test]
    fn keyboard_modes_tail_preserves_old_decoder_and_distinguishes_unknown() {
        use crate::terminal_input::KeyboardState;
        let legacy =
            super::Frame::modes_with_bracketed_paste(true, true, crate::terminal::MouseModes::OFF);
        assert_eq!(legacy.keyboard_state_payload().unwrap(), None);
        let known = super::Frame::modes_with_keyboard(
            true,
            true,
            crate::terminal::MouseModes::OFF,
            Some(KeyboardState::default()),
        );
        assert_eq!(
            known.terminal_modes_payload(),
            legacy.terminal_modes_payload()
        );
        assert_eq!(
            known.keyboard_state_payload().unwrap(),
            Some(KeyboardState::default())
        );
        for payload in [vec![0, 1], vec![0, 2, 0], vec![0, 1, 4], vec![]] {
            assert!(
                super::Frame::new(super::FrameType::Modes, payload)
                    .keyboard_state_payload()
                    .is_err()
            );
        }
    }

    use super::*;

    #[test]
    fn frame_type_values_are_wire_stable() {
        let types = [
            FrameType::Output,
            FrameType::Input,
            FrameType::Resize,
            FrameType::ReplayBegin,
            FrameType::ReplayEnd,
            FrameType::Ping,
            FrameType::Pong,
            FrameType::Grid,
            FrameType::Scroll,
            FrameType::Modes,
            FrameType::Mouse,
        ];
        for (index, frame_type) in types.into_iter().enumerate() {
            assert_eq!(frame_type as u8, index as u8 + 1);
            assert_eq!(FrameType::try_from(index as u8 + 1), Ok(frame_type));
        }
        assert_eq!(
            FrameType::try_from(0),
            Err(FrameCodecError::UnknownFrameType(0))
        );
        assert_eq!(
            FrameType::try_from(12),
            Err(FrameCodecError::UnknownFrameType(12))
        );
    }

    #[test]
    fn typed_frames_match_swift_bytes_and_accessors() {
        let output = Frame::output(0x0102_0304_0506_0708, b"pty");
        assert_eq!(
            FrameCodec::encode(&output).unwrap(),
            vec![1, 0, 0, 0, 11, 1, 2, 3, 4, 5, 6, 7, 8, b'p', b't', b'y']
        );
        assert_eq!(
            output.output_payload(),
            Some((0x0102_0304_0506_0708, &b"pty"[..]))
        );

        let resize = Frame::resize(0x1234, 0xabcd);
        assert_eq!(resize.payload, [0x12, 0x34, 0xab, 0xcd]);
        assert_eq!(resize.resize_payload(), Some((0x1234, 0xabcd)));

        let begin = Frame::replay_begin(42);
        let end = Frame::replay_end(u64::MAX);
        assert_eq!(begin.offset_payload(), Some(42));
        assert_eq!(end.offset_payload(), Some(u64::MAX));

        let scroll = Frame::scroll(1, 0x0203, 0x0405, 0x0607);
        assert_eq!(scroll.payload, [1, 2, 3, 4, 5, 6, 7]);
        assert_eq!(scroll.scroll_payload(), Some((1, 0x0203, 0x0405, 0x0607)));

        for alt_screen in [false, true] {
            for bracketed_paste in [false, true] {
                for mouse in [
                    MouseModes::OFF,
                    MouseModes::UNKNOWN,
                    MouseModes::new(
                        crate::terminal::MouseTrackingMode::Off,
                        crate::terminal::MouseEncoding::Sgr,
                    ),
                    MouseModes::new(
                        crate::terminal::MouseTrackingMode::ButtonEvents,
                        crate::terminal::MouseEncoding::Legacy,
                    ),
                    MouseModes::new(
                        crate::terminal::MouseTrackingMode::ButtonMotion,
                        crate::terminal::MouseEncoding::Sgr,
                    ),
                    MouseModes::new(
                        crate::terminal::MouseTrackingMode::AnyMotion,
                        crate::terminal::MouseEncoding::Sgr,
                    ),
                ] {
                    let modes =
                        Frame::modes_with_bracketed_paste(alt_screen, bracketed_paste, mouse);
                    assert_eq!(
                        modes.terminal_modes_payload(),
                        Some((alt_screen, bracketed_paste, mouse))
                    );
                    assert_eq!(modes.modes_payload(), Some((alt_screen, mouse)));
                    assert_eq!(modes.payload[0] & 0b10 != 0, mouse.is_reporting());
                    assert_eq!(modes.payload[0] & 0b10_0000 != 0, bracketed_paste);
                }
            }
        }
        assert_eq!(
            Frame::modes(true, MouseModes::OFF).terminal_modes_payload(),
            Some((true, false, MouseModes::OFF))
        );
        assert_eq!(Frame::ping().payload, Vec::<u8>::new());
        assert_eq!(Frame::pong().payload, Vec::<u8>::new());
        assert_eq!(Frame::input(b"input".to_vec()).payload, b"input");
        assert_eq!(Frame::mouse(b"mouse".to_vec()).payload, b"mouse");
    }

    #[test]
    fn modes_decode_the_historical_boolean_wire_format() {
        assert_eq!(
            Frame::new(FrameType::Modes, vec![0b11]).terminal_modes_payload(),
            Some((true, false, MouseModes::UNKNOWN))
        );
    }

    #[test]
    fn secret_input_rides_an_unused_modes_bit_without_disturbing_the_rest() {
        let mouse = MouseModes::new(
            crate::terminal::MouseTrackingMode::AnyMotion,
            crate::terminal::MouseEncoding::Sgr,
        );
        let keyboard = Some(crate::terminal_input::KeyboardState {
            enhancements: None,
            application_cursor_keys: true,
            application_keypad: false,
        });
        let plain = Frame::modes_with_keyboard(false, true, mouse, keyboard);
        let secret = plain.clone().with_secret_input(true);

        assert_eq!(plain.secret_input_payload(), Some(false));
        assert_eq!(secret.secret_input_payload(), Some(true));
        assert_eq!(secret.payload[0], plain.payload[0] | 0b100_0000);
        // Everything a client that predates the bit decodes is unchanged.
        assert_eq!(
            secret.terminal_modes_payload(),
            plain.terminal_modes_payload()
        );
        assert_eq!(
            secret.keyboard_state_payload(),
            plain.keyboard_state_payload()
        );
        assert_eq!(secret.with_secret_input(false), plain);

        // Frames from an engine that predates the bit read as not secret.
        assert_eq!(
            Frame::new(FrameType::Modes, vec![0b11]).secret_input_payload(),
            Some(false)
        );
        assert_eq!(Frame::ping().with_secret_input(true), Frame::ping());
        assert_eq!(Frame::ping().secret_input_payload(), None);
    }

    #[test]
    fn incremental_decoder_reassembles_every_partial_read_boundary() {
        let expected = vec![
            Frame::input(b"abc".to_vec()),
            Frame::resize(120, 40),
            Frame::ping(),
            Frame::scroll(0, 3, 17, 9),
        ];
        let stream: Vec<u8> = expected
            .iter()
            .flat_map(|frame| FrameCodec::encode(frame).unwrap())
            .collect();

        for split in 0..=stream.len() {
            let mut codec = FrameCodec::new();
            let mut actual = codec.feed(&stream[..split]).unwrap();
            actual.extend(codec.feed(&stream[split..]).unwrap());
            assert_eq!(actual, expected, "split at byte {split}");
            assert_eq!(codec.buffered_len(), 0);
        }

        let mut bytewise = FrameCodec::new();
        let mut actual = Vec::new();
        for byte in stream {
            actual.extend(bytewise.feed(&[byte]).unwrap());
        }
        assert_eq!(actual, expected);
    }

    #[test]
    fn decoder_rejects_unknown_types_and_oversized_headers_immediately() {
        let mut codec = FrameCodec::new();
        assert_eq!(
            codec.feed(&[99, 0, 0, 0, 0]),
            Err(FrameCodecError::UnknownFrameType(99))
        );

        let mut codec = FrameCodec::new();
        let oversized = (MAX_FRAME_BYTES as u32 + 1).to_be_bytes();
        let header = [
            FrameType::Grid as u8,
            oversized[0],
            oversized[1],
            oversized[2],
            oversized[3],
        ];
        assert_eq!(
            codec.feed(&header),
            Err(FrameCodecError::FrameTooLarge {
                length: MAX_FRAME_BYTES + 1,
                max: MAX_FRAME_BYTES,
            })
        );
    }

    #[test]
    fn encoder_rejects_oversized_payload() {
        let frame = Frame::new(FrameType::Input, vec![0; MAX_FRAME_BYTES + 1]);
        assert_eq!(
            FrameCodec::encode(&frame),
            Err(FrameCodecError::FrameTooLarge {
                length: MAX_FRAME_BYTES + 1,
                max: MAX_FRAME_BYTES,
            })
        );
    }

    #[test]
    fn typed_accessors_reject_wrong_type_or_short_payload() {
        assert_eq!(
            Frame::new(FrameType::Output, vec![0; 7]).output_payload(),
            None
        );
        assert_eq!(Frame::input(vec![0; 8]).output_payload(), None);
        assert_eq!(
            Frame::new(FrameType::Resize, vec![0; 3]).resize_payload(),
            None
        );
        assert_eq!(
            Frame::new(FrameType::Modes, Vec::new()).modes_payload(),
            None
        );
        assert_eq!(
            Frame::new(FrameType::Modes, Vec::new()).terminal_modes_payload(),
            None
        );
        assert_eq!(
            Frame::new(FrameType::Scroll, vec![0; 6]).scroll_payload(),
            None
        );
    }
}
