//! Handler effects that are events and terminal state rather than grid
//! cells: titles, colors, clipboard, replies, the keyboard protocol stack and
//! the cursor style. Each is a port of the current core's `Term` method of
//! the same name, sending the same `Event`s to the same kind of listener.

use std::sync::Arc;

use alacritty_terminal::event::{Event, EventListener, VoidListener};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::color::Colors;
use alacritty_terminal::term::{ClipboardType, Config, Osc52, TermMode};
use alacritty_terminal::vte::ansi::{
    Color, CursorShape, CursorStyle, KeyboardModes, KeyboardModesApplyBehavior, Mode, NamedColor,
    NamedMode, NamedPrivateMode, PrivateMode, Rgb,
};

use super::Core;

/// `TITLE_STACK_MAX_DEPTH` / `KEYBOARD_MODE_STACK_MAX_DEPTH` of the current core.
const STACK_MAX_DEPTH: usize = 4096;

/// What the current core keeps beside its grid: the options it was built
/// with, where its events go, and the state those events describe.
pub struct Events {
    pub config: Config,
    pub listener: Box<dyn EventListener + Send>,
    pub title: Option<String>,
    pub title_stack: Vec<Option<String>>,
    pub colors: Colors,
    pub colors_generation: u64,
    pub cursor_style: Option<CursorStyle>,
    pub keyboard_stack: Vec<KeyboardModes>,
    pub inactive_keyboard_stack: Vec<KeyboardModes>,
}

impl Default for Events {
    fn default() -> Self {
        Events {
            config: Config::default(),
            listener: Box::new(VoidListener),
            title: None,
            title_stack: Vec::new(),
            colors: Colors::default(),
            colors_generation: 0,
            cursor_style: None,
            keyboard_stack: Vec::new(),
            inactive_keyboard_stack: Vec::new(),
        }
    }
}

/// `ModeState` of the current core.
#[derive(Clone, Copy)]
enum ModeState {
    NotSupported = 0,
    Set = 1,
    Reset = 2,
    PermanentlyReset = 4,
}

impl From<bool> for ModeState {
    fn from(set: bool) -> Self {
        if set {
            ModeState::Set
        } else {
            ModeState::Reset
        }
    }
}

impl Core {
    /// Uses `config` and sends events to `listener`, as `Term::new` does.
    pub fn set_events(&mut self, config: Config, listener: Box<dyn EventListener + Send>) {
        self.events.config = config;
        self.events.listener = listener;
    }

    pub fn title(&self) -> Option<&str> {
        self.events.title.as_deref()
    }

    pub fn colors(&self) -> &Colors {
        &self.events.colors
    }

    pub fn colors_generation(&self) -> u64 {
        self.events.colors_generation
    }

    pub fn cursor_style(&self) -> CursorStyle {
        self.events
            .cursor_style
            .unwrap_or(self.events.config.default_cursor_style)
    }

    pub(super) fn send(&self, event: Event) {
        self.events.listener.send_event(event);
    }

    fn reply(&self, text: String) {
        self.send(Event::PtyWrite(text));
    }

    /// `Term::set_keyboard_mode` (the inherent one): the protocol bits of
    /// the mode.
    pub(super) fn apply_keyboard_mode(
        &mut self,
        mode: TermMode,
        apply: KeyboardModesApplyBehavior,
    ) {
        let active = self.mode & TermMode::KITTY_KEYBOARD_PROTOCOL;
        self.mode &= !TermMode::KITTY_KEYBOARD_PROTOCOL;
        let new = match apply {
            KeyboardModesApplyBehavior::Replace => mode,
            KeyboardModesApplyBehavior::Union => active.union(mode),
            KeyboardModesApplyBehavior::Difference => active.difference(mode),
        };
        self.mode |= new;
    }

    /// The keyboard part of `Term::swap_alt_screen`.
    pub(super) fn swap_keyboard_stacks(&mut self) {
        let events = &mut self.events;
        std::mem::swap(
            &mut events.keyboard_stack,
            &mut events.inactive_keyboard_stack,
        );
        let mode = events
            .keyboard_stack
            .last()
            .copied()
            .unwrap_or(KeyboardModes::NO_MODE)
            .into();
        self.apply_keyboard_mode(mode, KeyboardModesApplyBehavior::Replace);
    }

    /// The non-grid part of `Term::reset_state`, in its order.
    pub(super) fn reset_events(&mut self) {
        self.events.cursor_style = None;
        self.events.title_stack = Vec::new();
        if self.events.title.take().is_some() {
            self.send(Event::ResetTitle);
        }
        if (0..alacritty_terminal::term::color::COUNT).any(|i| self.events.colors[i].is_some()) {
            self.events.colors = Colors::default();
            self.events.colors_generation += 1;
        }
        self.events.keyboard_stack = Vec::new();
        self.events.inactive_keyboard_stack = Vec::new();
    }

    /// The events and cursor style of `Term::set_private_mode` /
    /// `unset_private_mode`, after its mode bits.
    pub(super) fn private_mode_events(&mut self, mode: NamedPrivateMode, on: bool) {
        match mode {
            NamedPrivateMode::ReportMouseClicks
            | NamedPrivateMode::ReportCellMouseMotion
            | NamedPrivateMode::ReportAllMouseMotion => self.send(Event::MouseCursorDirty),
            NamedPrivateMode::BlinkingCursor => {
                let default = self.events.config.default_cursor_style;
                self.events.cursor_style.get_or_insert(default).blinking = on;
                self.send(Event::CursorBlinkingChange);
            }
            NamedPrivateMode::InBandResize if on => self.send(Event::InBandResize),
            _ => {}
        }
    }

    // ---- the Handler methods, ported ----

    pub(super) fn identify_terminal_inner(&mut self, intermediate: Option<char>) {
        match intermediate {
            None => self.reply(String::from("\x1b[?6c")),
            Some('>') => {
                let version = alacritty_terminal::term::da2_version();
                self.reply(format!("\x1b[>0;{version};1c"));
            }
            _ => {}
        }
    }

    pub(super) fn report_keyboard_mode_inner(&mut self) {
        if !self.events.config.kitty_keyboard {
            return;
        }
        let current = KeyboardModes::from(self.mode).bits();
        self.reply(format!("\x1b[?{current}u"));
    }

    pub(super) fn push_keyboard_mode_inner(&mut self, mode: KeyboardModes) {
        if !self.events.config.kitty_keyboard {
            return;
        }
        if self.events.keyboard_stack.len() >= STACK_MAX_DEPTH {
            self.events.keyboard_stack.remove(0);
        }
        self.events.keyboard_stack.push(mode);
        self.apply_keyboard_mode(mode.into(), KeyboardModesApplyBehavior::Replace);
    }

    pub(super) fn pop_keyboard_modes_inner(&mut self, to_pop: u16) {
        if !self.events.config.kitty_keyboard {
            return;
        }
        let stack = &mut self.events.keyboard_stack;
        stack.truncate(stack.len().saturating_sub(to_pop as usize));
        let mode = stack.last().copied().unwrap_or(KeyboardModes::NO_MODE);
        self.apply_keyboard_mode(mode.into(), KeyboardModesApplyBehavior::Replace);
    }

    pub(super) fn set_keyboard_mode_inner(
        &mut self,
        mode: KeyboardModes,
        apply: KeyboardModesApplyBehavior,
    ) {
        if !self.events.config.kitty_keyboard {
            return;
        }
        self.apply_keyboard_mode(mode.into(), apply);
        let current = KeyboardModes::from(self.mode);
        match self.events.keyboard_stack.last_mut() {
            Some(top) => *top = current,
            None => self.events.keyboard_stack.push(current),
        }
    }

    pub(super) fn device_status_inner(&mut self, arg: usize) {
        match arg {
            5 => self.reply(String::from("\x1b[0n")),
            6 => self.reply(format!("\x1b[{};{}R", self.line + 1, self.col + 1)),
            _ => {}
        }
    }

    pub(super) fn private_device_status_inner(&mut self, arg: usize) {
        match arg {
            6 => self.reply(format!("\x1b[?{};{};1R", self.line + 1, self.col + 1)),
            996 => self.send(Event::ColorSchemeRequest),
            _ => {}
        }
    }

    pub(super) fn set_color_inner(&mut self, index: usize, color: Rgb) {
        if index != NamedColor::Cursor as usize && self.events.colors[index] != Some(color) {
            self.events.colors_generation += 1;
        }
        self.events.colors[index] = Some(color);
    }

    pub(super) fn reset_color_inner(&mut self, index: usize) {
        if index != NamedColor::Cursor as usize && self.events.colors[index].is_some() {
            self.events.colors_generation += 1;
        }
        self.events.colors[index] = None;
    }

    pub(super) fn dynamic_color_sequence_inner(&mut self, prefix: String, index: usize, end: &str) {
        let end = end.to_owned();
        self.send(Event::ColorRequest(
            index,
            Arc::new(move |color| {
                format!(
                    "\x1b]{};rgb:{1:02x}{1:02x}/{2:02x}{2:02x}/{3:02x}{3:02x}{4}",
                    prefix, color.r, color.g, color.b, end
                )
            }),
        ));
    }

    fn clipboard_type(clipboard: u8) -> Option<ClipboardType> {
        match clipboard {
            b'c' => Some(ClipboardType::Clipboard),
            b'p' | b's' => Some(ClipboardType::Selection),
            _ => None,
        }
    }

    pub(super) fn clipboard_store_inner(&mut self, clipboard: u8, base64: &[u8]) {
        if !matches!(self.events.config.osc52, Osc52::OnlyCopy | Osc52::CopyPaste) {
            return;
        }
        let Some(kind) = Self::clipboard_type(clipboard) else {
            return;
        };
        if let Some(text) = alacritty_terminal::term::osc52_store_text(base64) {
            self.send(Event::ClipboardStore(kind, text));
        }
    }

    pub(super) fn clipboard_load_inner(&mut self, clipboard: u8, end: &str) {
        if !matches!(
            self.events.config.osc52,
            Osc52::OnlyPaste | Osc52::CopyPaste
        ) {
            return;
        }
        let Some(kind) = Self::clipboard_type(clipboard) else {
            return;
        };
        let end = end.to_owned();
        self.send(Event::ClipboardLoad(
            kind,
            Arc::new(move |text| alacritty_terminal::term::osc52_load_reply(clipboard, text, &end)),
        ));
    }

    pub(super) fn report_mode_inner(&mut self, mode: Mode) {
        let state = match mode {
            Mode::Named(NamedMode::Insert) => self.mode.contains(TermMode::INSERT).into(),
            Mode::Named(NamedMode::LineFeedNewLine) => {
                self.mode.contains(TermMode::LINE_FEED_NEW_LINE).into()
            }
            Mode::Unknown(_) => ModeState::NotSupported,
        };
        self.reply(format!("\x1b[{};{}$y", mode.raw(), state as u8));
    }

    pub(super) fn report_private_mode_inner(&mut self, mode: PrivateMode) {
        let on = |bit| ModeState::from(self.mode.contains(bit));
        let state = match mode {
            PrivateMode::Named(named) => match named {
                NamedPrivateMode::CursorKeys => on(TermMode::APP_CURSOR),
                NamedPrivateMode::Origin => on(TermMode::ORIGIN),
                NamedPrivateMode::LineWrap => on(TermMode::LINE_WRAP),
                NamedPrivateMode::BlinkingCursor => {
                    let default = self.events.config.default_cursor_style;
                    self.events
                        .cursor_style
                        .get_or_insert(default)
                        .blinking
                        .into()
                }
                NamedPrivateMode::ShowCursor => on(TermMode::SHOW_CURSOR),
                NamedPrivateMode::ReportMouseClicks => on(TermMode::MOUSE_REPORT_CLICK),
                NamedPrivateMode::ReportCellMouseMotion => on(TermMode::MOUSE_DRAG),
                NamedPrivateMode::ReportAllMouseMotion => on(TermMode::MOUSE_MOTION),
                NamedPrivateMode::ReportFocusInOut => on(TermMode::FOCUS_IN_OUT),
                NamedPrivateMode::Utf8Mouse => on(TermMode::UTF8_MOUSE),
                NamedPrivateMode::SgrMouse => on(TermMode::SGR_MOUSE),
                NamedPrivateMode::AlternateScroll => on(TermMode::ALTERNATE_SCROLL),
                NamedPrivateMode::UrgencyHints => on(TermMode::URGENCY_HINTS),
                NamedPrivateMode::SwapScreenAndSetRestoreCursor
                | NamedPrivateMode::LegacyAltScreen
                | NamedPrivateMode::ClearingAltScreen => on(TermMode::ALT_SCREEN),
                NamedPrivateMode::BracketedPaste => on(TermMode::BRACKETED_PASTE),
                NamedPrivateMode::SyncUpdate => ModeState::Reset,
                NamedPrivateMode::GraphemeClustering => on(TermMode::GRAPHEME_CLUSTERING),
                NamedPrivateMode::ColumnMode => ModeState::NotSupported,
                NamedPrivateMode::LeftRightMargins | NamedPrivateMode::SgrPixelMouse => {
                    ModeState::PermanentlyReset
                }
                NamedPrivateMode::ColorSchemeUpdates => on(TermMode::COLOR_SCHEME_UPDATES),
                NamedPrivateMode::InBandResize => on(TermMode::IN_BAND_RESIZE),
            },
            PrivateMode::Unknown(_) => ModeState::NotSupported,
        };
        self.reply(format!("\x1b[?{};{}$y", mode.raw(), state as u8));
    }

    pub(super) fn set_cursor_style_inner(&mut self, style: Option<CursorStyle>) {
        self.events.cursor_style = style;
        self.send(Event::CursorBlinkingChange);
    }

    pub(super) fn set_cursor_shape_inner(&mut self, shape: CursorShape) {
        let default = self.events.config.default_cursor_style;
        self.events.cursor_style.get_or_insert(default).shape = shape;
    }

    pub(super) fn set_title_inner(&mut self, title: Option<String>) {
        self.events.title.clone_from(&title);
        self.send(match title {
            Some(title) => Event::Title(title),
            None => Event::ResetTitle,
        });
    }

    pub(super) fn push_title_inner(&mut self) {
        if self.events.title_stack.len() >= STACK_MAX_DEPTH {
            self.events.title_stack.remove(0);
        }
        let title = self.events.title.clone();
        self.events.title_stack.push(title);
    }

    pub(super) fn pop_title_inner(&mut self) {
        if let Some(popped) = self.events.title_stack.pop() {
            self.set_title_inner(popped);
        }
    }

    pub(super) fn text_area_size_pixels_inner(&mut self) {
        self.send(Event::TextAreaSizeRequest(Arc::new(|size| {
            let height = size.num_lines * size.cell_height;
            let width = size.num_cols * size.cell_width;
            format!("\x1b[4;{height};{width}t")
        })));
    }

    pub(super) fn cell_size_pixels_inner(&mut self) {
        self.send(Event::CellSizeRequest(Arc::new(|size| {
            format!("\x1b[6;{};{}t", size.cell_height, size.cell_width)
        })));
    }

    pub(super) fn text_area_size_chars_inner(&mut self) {
        self.reply(format!("\x1b[8;{};{}t", self.lines, self.cols));
    }

    pub(super) fn screen_size_chars_inner(&mut self) {
        self.reply(format!("\x1b[9;{};{}t", self.lines, self.cols));
    }

    pub(super) fn report_version_inner(&mut self) {
        if let Some(version) = &self.events.config.terminal_version {
            self.reply(format!("\x1bP>|{version}\x1b\\"));
        }
    }

    pub(super) fn request_termcap_inner(&mut self, names: &[u8]) {
        for hex in names.split(|&byte| byte == b';') {
            self.reply(alacritty_terminal::term::xtgettcap_reply(hex));
        }
    }

    pub(super) fn request_setting_inner(&mut self, setting: &[u8]) {
        let value = match setting {
            b"m" => Some(self.sgr_report()),
            b"r" => Some(format!("{};{}r", self.region.0 + 1, self.region.1)),
            b" q" => {
                let style = self.cursor_style();
                let steady = match style.shape {
                    CursorShape::Underline => 4,
                    CursorShape::Beam => 6,
                    CursorShape::Block | CursorShape::HollowBlock | CursorShape::Hidden => 2,
                };
                Some(format!("{} q", steady - u8::from(style.blinking)))
            }
            _ => None,
        };
        self.reply(match value {
            Some(value) => format!("\x1bP1$r{value}\x1b\\"),
            None => String::from("\x1bP0$r\x1b\\"),
        });
    }

    /// `Term::sgr_report`: the SGR state of new text.
    fn sgr_report(&self) -> String {
        use std::fmt::Write;
        fn color(out: &mut String, color: Color, base: u8, bright: u8, extended: u8) {
            match color {
                Color::Named(named) if (named as usize) < 8 => {
                    let _ = write!(out, ";{}", base + named as u8);
                }
                Color::Named(named) if (named as usize) < 16 => {
                    let _ = write!(out, ";{}", bright + named as u8 - 8);
                }
                Color::Named(_) => {}
                Color::Indexed(index) => {
                    let _ = write!(out, ";{extended}:5:{index}");
                }
                Color::Spec(rgb) => {
                    let _ = write!(out, ";{extended}:2::{}:{}:{}", rgb.r, rgb.g, rgb.b);
                }
            }
        }
        let flags = Flags::from_bits_retain(self.pen.flags);
        let mut out = String::from("0");
        for (flag, code) in [
            (Flags::BOLD, "1"),
            (Flags::DIM, "2"),
            (Flags::ITALIC, "3"),
            (Flags::UNDERLINE, "4"),
            (Flags::DOUBLE_UNDERLINE, "4:2"),
            (Flags::UNDERCURL, "4:3"),
            (Flags::DOTTED_UNDERLINE, "4:4"),
            (Flags::DASHED_UNDERLINE, "4:5"),
            (Flags::INVERSE, "7"),
            (Flags::HIDDEN, "8"),
            (Flags::STRIKEOUT, "9"),
        ] {
            if flags.contains(flag) {
                out.push(';');
                out.push_str(code);
            }
        }
        color(&mut out, self.pen.fg, 30, 90, 38);
        color(&mut out, self.pen.bg, 40, 100, 48);
        if let Some(underline) = self.pen.underline {
            color(&mut out, underline, 58, 58, 58);
        }
        out.push('m');
        out
    }
}
