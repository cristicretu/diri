//! A terminal over core v2 that is fed like [`HeadlessScreen`]: every read
//! is indexed once for its string sequences (#922), scanned for progress
//! reports and notifications, and parsed with Kitty and iTerm2 image
//! sequences cut out and handled by the same image code (see
//! [`ImageHost`]). Grid snapshots, diffs and checkpoints are not here yet
//! (migration step 4); this is the full feed path, so benchmarks measure
//! the work the Engine does per read.

use std::sync::{Arc, Mutex};

use alacritty_terminal::event::Event;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::{Config, TermMode};
use alacritty_terminal::vte::ansi::{Handler, Processor};
use alacritty_terminal::vte::scan::{C0Index, c0_positions};
use diri_proto::grid::CursorStyle;
use diri_proto::terminal::MouseModes;
use diri_proto::terminal_input::KeyboardState;

use super::Core;
use super::cell::Style;
use super::page::Link;
use crate::inline_images::{ImageHost, InlineImages, StoredImage};
use crate::notifications::NotificationParser;
use crate::{
    C0_INDEX_CAP, Collector, Inbox, ProgramRecord, Progress, TerminalNotification,
    keyboard_state_of, mouse_modes_of, only_csi_escapes, wire_cursor_style_of,
};

/// Answers `HeadlessScreen` gives that this screen does not give the same
/// way are counted, never silently dropped: a payload that needs them
/// fails a comparison instead.
pub struct Screen {
    pub core: Core,
    parser: Processor,
    events: Arc<Mutex<Inbox>>,
    drained: Vec<Event>,
    keyboard_enhancements_known: bool,
    title: Option<String>,
    notifications: Option<NotificationParser>,
    progress: Progress,
    images: Option<Box<InlineImages>>,
    c0_index: Vec<u32>,
    replies: Vec<u8>,
    /// Host replies are on (`HeadlessScreen::with_host_replies`).
    host_replies: bool,
    /// Events `HeadlessScreen` answers from host state this screen does not
    /// model (colors, color scheme, in-band resize, with host replies on).
    pub unanswered_events: u64,
}

impl Screen {
    /// `lines` x `cols` with history bounded like [`Core::with_budget`], in
    /// the configuration of `HeadlessScreen::new_with_keyboard_enhancements`
    /// (`kitty_keyboard`), answering XTVERSION with `terminal_version` when
    /// given (`with_host_replies`).
    pub fn new(
        lines: usize,
        cols: usize,
        history_rows: usize,
        budget: Option<usize>,
        kitty_keyboard: bool,
        terminal_version: Option<String>,
    ) -> Screen {
        let mut core = Core::with_budget(lines, cols, history_rows, budget);
        let events = Arc::new(Mutex::new(Inbox::default()));
        let config = Config {
            kitty_keyboard,
            terminal_version,
            default_cursor_style: crate::DEFAULT_CURSOR,
            ..Config::default()
        };
        let host_replies = config.terminal_version.is_some();
        core.set_events(config, Box::new(Collector(events.clone())));
        Screen {
            core,
            parser: Processor::new(),
            events,
            drained: Vec::new(),
            keyboard_enhancements_known: kitty_keyboard,
            title: None,
            notifications: None,
            progress: Progress::default(),
            images: None,
            c0_index: Vec::new(),
            replies: Vec::new(),
            host_replies,
            unanswered_events: 0,
        }
    }

    /// As `HeadlessScreen::with_notifications`.
    #[must_use]
    pub fn with_notifications(mut self) -> Self {
        self.notifications = Some(NotificationParser::default());
        self
    }

    /// As `HeadlessScreen::with_inline_images`.
    #[must_use]
    pub fn with_inline_images(mut self) -> Self {
        self.images = Some(Box::new(InlineImages::new()));
        self
    }

    /// Feeds one read, as `HeadlessScreen::feed` does.
    pub fn feed(&mut self, bytes: &[u8]) {
        let csi_only = only_csi_escapes(bytes);
        let mut c0_index = std::mem::take(&mut self.c0_index);
        let indexed = !csi_only && c0_positions(bytes, &mut c0_index, C0_INDEX_CAP);
        let index = indexed.then(|| C0Index::new(&c0_index));
        if !(csi_only && self.progress.carry.is_empty()) {
            self.progress.scan(bytes, index);
        }
        if let Some(parser) = &mut self.notifications {
            if !(csi_only && parser.is_idle()) {
                parser.feed_indexed(bytes, index);
            }
            self.replies.append(&mut parser.replies);
        }
        ImageHost::advance_with_images(self, bytes, 0, csi_only, index);
        self.c0_index = c0_index;
        self.drain_events();
    }

    /// The answers `HeadlessScreen::drain_events` gives for a terminal
    /// with images (or without), XTVERSION as configured.
    fn drain_events(&mut self) {
        let mut drained = std::mem::take(&mut self.drained);
        let title = {
            let mut inbox = self.events.lock().expect("terminal events");
            std::mem::swap(&mut inbox.events, &mut drained);
            inbox.title.take()
        };
        for event in title.into_iter().chain(drained.drain(..)) {
            match event {
                Event::Title(title) => self.title = Some(title),
                Event::ResetTitle => self.title = None,
                Event::PtyWrite(text) => {
                    let text = if self.images.is_some() && text == "\x1b[?6c" {
                        "\x1b[?62;22c"
                    } else {
                        text.as_str()
                    };
                    self.replies.extend_from_slice(text.as_bytes());
                }
                Event::TextAreaSizeRequest(format) => {
                    let (cell_width, cell_height) = self
                        .images
                        .as_ref()
                        .map_or((0, 0), |images| images.cell_pixels);
                    let size = alacritty_terminal::event::WindowSize {
                        num_cols: self.core.cols() as u16,
                        num_lines: self.core.lines() as u16,
                        cell_width,
                        cell_height,
                    };
                    self.replies.extend_from_slice(format(size).as_bytes());
                }
                Event::CellSizeRequest(format) => {
                    if let Some(images) = &self.images {
                        let (cell_width, cell_height) = images.cell_pixels;
                        let size = alacritty_terminal::event::WindowSize {
                            num_cols: self.core.cols() as u16,
                            num_lines: self.core.lines() as u16,
                            cell_width,
                            cell_height,
                        };
                        self.replies.extend_from_slice(format(size).as_bytes());
                    }
                }
                Event::ColorRequest(..) | Event::ColorSchemeRequest | Event::InBandResize
                    if self.host_replies =>
                {
                    self.unanswered_events += 1;
                }
                _ => {}
            }
        }
        self.drained = drained;
    }

    pub fn take_replies(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.replies)
    }

    pub fn take_notifications(&mut self) -> Vec<TerminalNotification> {
        self.notifications
            .as_mut()
            .map(|parser| parser.ready.drain(..).collect())
            .unwrap_or_default()
    }

    pub fn take_clipboard(&mut self) -> Option<String> {
        self.notifications.as_mut()?.clipboard.take()
    }

    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    /// (col, row, visible), as `HeadlessScreen::cursor`.
    pub fn cursor(&self) -> (u16, u16, bool) {
        (
            self.core.col as u16,
            self.core.line as u16,
            self.core.mode.contains(TermMode::SHOW_CURSOR),
        )
    }

    pub fn is_alt_screen(&self) -> bool {
        self.core.mode.contains(TermMode::ALT_SCREEN)
    }

    pub fn bracketed_paste(&self) -> bool {
        self.core.mode.contains(TermMode::BRACKETED_PASTE)
    }

    pub fn focus_reporting(&self) -> bool {
        self.core.mode.contains(TermMode::FOCUS_IN_OUT)
    }

    pub fn mouse_modes(&self) -> MouseModes {
        mouse_modes_of(self.core.mode)
    }

    pub fn cursor_style(&self) -> CursorStyle {
        wire_cursor_style_of(self.core.cursor_style())
    }

    pub fn keyboard_state(&self) -> KeyboardState {
        keyboard_state_of(self.core.mode, self.keyboard_enhancements_known)
    }

    pub fn progress(&self) -> Option<(i64, i64)> {
        self.progress.progress()
    }

    pub fn program_status(&self) -> Option<ProgramRecord> {
        self.notifications.as_ref()?.program.summary()
    }

    pub fn synchronized_updates_completed(&self) -> u64 {
        self.parser.synchronized_updates_completed()
    }

    pub fn in_synchronized_update(&self) -> bool {
        self.parser.sync_timeout().sync_timeout().is_some()
    }

    /// Bytes held by the image store.
    pub fn inline_image_bytes(&self) -> usize {
        self.images
            .as_ref()
            .map_or(0, |images| images.stored_bytes())
    }

    /// Every image stored since the last call (feature `bench-verify`).
    #[cfg(feature = "bench-verify")]
    pub fn take_stored_image_log(&mut self) -> Vec<(Option<u32>, StoredImage)> {
        self.images
            .as_mut()
            .map_or_else(Vec::new, |images| std::mem::take(&mut images.store.log))
    }

    /// As `HeadlessScreen::inline_image_store_summary`.
    pub fn inline_image_store_summary(&self) -> Vec<String> {
        self.images
            .as_ref()
            .map_or_else(Vec::new, |images| images.summary())
    }

    /// The stored image an anchor key names.
    pub fn inline_image(&self, key: &str) -> Option<StoredImage> {
        self.images.as_ref()?.get(key)
    }

    /// Runs `f` with the pen set to a default-style blank carrying `link`,
    /// then restores the pen, its link and the character-set gate.
    fn with_pen(&mut self, link: Option<Link>, f: impl FnOnce(&mut Core)) {
        let core = &mut self.core;
        let (pen, saved_link) = (core.pen, core.link.take());
        core.pen = Style::default();
        core.pen_cache.0 = 0;
        core.link = link;
        core.update_slow_text();
        f(core);
        core.pen = pen;
        core.pen_cache.0 = 0;
        core.link = saved_link;
        core.update_slow_text();
    }
}

impl ImageHost for Screen {
    fn images_ref(&self) -> Option<&InlineImages> {
        self.images.as_deref()
    }

    fn images_mut(&mut self) -> Option<&mut InlineImages> {
        self.images.as_deref_mut()
    }

    fn advance_parser(&mut self, bytes: &[u8], index: Option<C0Index<'_>>) {
        match index {
            Some(index) => self.parser.advance_indexed(&mut self.core, bytes, index),
            None => self.parser.advance(&mut self.core, bytes),
        }
    }

    fn reply(&mut self, historical: bool, reply: &[u8]) {
        if historical {
            return;
        }
        self.drain_events();
        self.replies.extend_from_slice(reply);
    }

    fn release_held_update(&mut self) {
        if self.parser.sync_timeout().sync_timeout().is_some() {
            self.parser.stop_sync(&mut self.core);
        }
    }

    fn screen_size(&self) -> (usize, usize) {
        (self.core.cols(), self.core.lines())
    }

    fn image_cursor(&self) -> (Line, Column) {
        (Line(self.core.line as i32), Column(self.core.col))
    }

    fn write_anchor_rows(
        &mut self,
        key: &str,
        col0: usize,
        (cols, rows): (u16, u16),
        written: usize,
        move_cursor: bool,
    ) {
        let screen_lines = self.core.lines();
        let (line0, col, wrap) = (self.core.line, self.core.col, self.core.wrap_pending);
        // The whole box is written under the anchor pen, linefeeds included:
        // a row a linefeed scrolls in is erased in that (default) style, as
        // the current core's cursor template has it.
        self.with_pen(None, |core| {
            let mut line = line0;
            for row in 0..rows {
                if row > 0 {
                    if move_cursor {
                        Handler::linefeed(core);
                        line = core.line;
                    } else {
                        line += 1;
                        if line >= screen_lines {
                            break;
                        }
                    }
                }
                let anchor = diri_proto::inline_image::ImageAnchor {
                    key: key.to_owned(),
                    row,
                    col: col0 as u16,
                    cols,
                    rows,
                };
                core.link = Some(Link {
                    id: None,
                    uri: anchor.to_uri(),
                });
                core.update_slow_text();
                core.line = line;
                core.col = col0;
                core.wrap_pending = false;
                for _ in 0..written {
                    Handler::input(core, ' ');
                }
            }
        });
        if !move_cursor {
            self.core.line = line0;
            self.core.col = col;
            self.core.wrap_pending = wrap;
        }
    }

    fn link_uri_at(&self, line: Line, column: Column) -> Option<String> {
        Some(self.core.link_at(line.0, column.0)?.uri)
    }

    fn blank_cells(&mut self, positions: &[(Line, Column)]) {
        if positions.is_empty() {
            return;
        }
        let (line, col, wrap) = (self.core.line, self.core.col, self.core.wrap_pending);
        self.with_pen(None, |core| {
            for &(line, column) in positions {
                core.line = line.0 as usize;
                core.col = column.0;
                core.wrap_pending = false;
                Handler::input(core, ' ');
            }
        });
        self.core.line = line;
        self.core.col = col;
        self.core.wrap_pending = wrap;
    }
}

#[cfg(test)]
mod tests {
    use super::Screen;
    use crate::HeadlessScreen;

    const LINES: usize = 12;
    const COLS: usize = 30;

    /// Image anchors name a per-store nonce; only the rest must match.
    fn without_store_nonce(text: String) -> String {
        const ANCHOR: &str = "diri-image:";
        let mut out = String::with_capacity(text.len());
        let mut rest = text.as_str();
        while let Some(at) = rest.find(ANCHOR) {
            out.push_str(&rest[..at + ANCHOR.len()]);
            rest = &rest[at + ANCHOR.len()..];
            let nonce = rest.find('.').unwrap_or(0);
            out.push('#');
            rest = &rest[nonce..];
        }
        out.push_str(rest);
        out
    }

    /// Everything both screens expose after a run, as text.
    fn state_v2(
        screen: &mut Screen,
        replies: &[u8],
        notes: &[String],
        clips: &[String],
    ) -> Vec<String> {
        let mut bridged = HeadlessScreen::from_core_v2(&screen.core);
        vec![
            format!("replies {:?}", String::from_utf8_lossy(replies)),
            format!("notifications {notes:?}"),
            // Two OSC 52 writes in one read reach the pump as one, the
            // newer, by design: compare the last.
            format!("clipboard {:?}", clips.last()),
            format!("title {:?}", screen.title()),
            format!("cursor {:?}", screen.cursor()),
            format!(
                "modes {} {} {} {:?} {:?}",
                screen.is_alt_screen(),
                screen.bracketed_paste(),
                screen.focus_reporting(),
                screen.mouse_modes(),
                screen.cursor_style()
            ),
            format!("keyboard {:?}", screen.keyboard_state()),
            format!("progress {:?}", screen.progress()),
            format!("program {:?}", screen.program_status()),
            format!(
                "sync {} {}",
                // How many complete depends on where reads split (an update
                // spanning reads is held whole); whether any did does not.
                screen.synchronized_updates_completed() > 0,
                screen.in_synchronized_update()
            ),
            format!("images {:?}", screen.inline_image_store_summary()),
            format!("image bytes {}", screen.inline_image_bytes()),
            without_store_nonce(format!("grid {:?}", bridged.full_snapshot())),
            without_store_nonce(format!("links {:?}", bridged.link_source(0).hyperlinks)),
        ]
    }

    fn state_current(
        screen: &mut HeadlessScreen,
        replies: &[u8],
        notes: &[String],
        clips: &[String],
    ) -> Vec<String> {
        vec![
            format!("replies {:?}", String::from_utf8_lossy(replies)),
            format!("notifications {notes:?}"),
            // Two OSC 52 writes in one read reach the pump as one, the
            // newer, by design: compare the last.
            format!("clipboard {:?}", clips.last()),
            format!("title {:?}", screen.title()),
            format!("cursor {:?}", screen.cursor()),
            format!(
                "modes {} {} {} {:?} {:?}",
                screen.is_alt_screen(),
                screen.bracketed_paste(),
                screen.focus_reporting(),
                screen.mouse_modes(),
                screen.cursor_style()
            ),
            format!("keyboard {:?}", screen.keyboard_state()),
            format!("progress {:?}", screen.progress()),
            format!("program {:?}", screen.program_status()),
            format!(
                "sync {} {}",
                // How many complete depends on where reads split (an update
                // spanning reads is held whole); whether any did does not.
                screen.synchronized_updates_completed() > 0,
                screen.in_synchronized_update()
            ),
            format!("images {:?}", screen.inline_image_store_summary()),
            format!("image bytes {}", screen.inline_image_bytes()),
            without_store_nonce(format!("grid {:?}", screen.full_snapshot())),
            without_store_nonce(format!("links {:?}", screen.link_source(0).hyperlinks)),
        ]
    }

    fn run_v2(input: &[u8], reads: &[usize]) -> (Screen, Vec<String>) {
        let mut screen = Screen::new(LINES, COLS, 0, None, true, None)
            .with_notifications()
            .with_inline_images();
        let (mut replies, mut notes, mut clips) = (Vec::new(), Vec::new(), Vec::new());
        let mut rest = input;
        let mut k = 0;
        while !rest.is_empty() {
            let n = reads[k % reads.len()].min(rest.len());
            screen.feed(&rest[..n]);
            rest = &rest[n..];
            k += 1;
            replies.extend(screen.take_replies());
            notes.extend(screen.take_notifications().iter().map(|n| format!("{n:?}")));
            clips.extend(screen.take_clipboard());
        }
        let state = state_v2(&mut screen, &replies, &notes, &clips);
        (screen, state)
    }

    fn run_current(input: &[u8]) -> Vec<String> {
        let mut screen = HeadlessScreen::new_with_keyboard_enhancements(COLS, LINES)
            .with_notifications()
            .with_inline_images();
        let (mut replies, mut notes, mut clips) = (Vec::new(), Vec::new(), Vec::new());
        for byte in input.chunks(1) {
            screen.feed(byte);
            replies.extend(screen.take_replies());
            notes.extend(screen.take_notifications().iter().map(|n| format!("{n:?}")));
            clips.extend(screen.take_clipboard());
        }
        state_current(&mut screen, &replies, &notes, &clips)
    }

    /// The v2 screen ends where the current screen, fed a byte at a time,
    /// ends: grid, replies, notifications, clipboard, title, modes,
    /// keyboard, progress, program status, synchronized updates and the
    /// image store, whatever the reads.
    fn check(name: &str, input: &[u8]) {
        let want = run_current(input);
        for reads in [
            &[input.len().max(1)][..],
            &[1],
            &[7],
            &[64 << 10],
            &[3, 97, 1, 4096],
        ] {
            let (screen, got) = run_v2(input, reads);
            assert_eq!(screen.core.unsupported, 0, "{name}");
            assert_eq!(screen.unanswered_events, 0, "{name}");
            for (got, want) in got.iter().zip(&want) {
                if got != want {
                    let at = got
                        .bytes()
                        .zip(want.bytes())
                        .position(|(a, b)| a != b)
                        .unwrap_or(got.len().min(want.len()));
                    let from = at.saturating_sub(300);
                    panic!(
                        "{name} reads {reads:?}: differs at byte {at}\n got  ...{}\n want ...{}",
                        &got[from..(at + 200).min(got.len())],
                        &want[from..(at + 200).min(want.len())]
                    );
                }
            }
        }
    }

    #[test]
    fn adversarial_fixtures_match_the_current_screen() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/adversarial");
        let mut names: Vec<_> = std::fs::read_dir(dir)
            .expect("fixtures")
            .map(|entry| entry.expect("entry").path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "bin"))
            .collect();
        names.sort();
        assert!(names.len() >= 40, "fixtures present");
        for path in names {
            let input = std::fs::read(&path).expect("fixture");
            check(&path.display().to_string(), &input);
        }
    }

    #[test]
    fn kitty_and_iterm_placements_deletes_and_replies() {
        // A 2x1 RGBA image (8 bytes), placed with and without moving the
        // cursor, at the right edge, at the bottom (scrolling), replaced,
        // put again, and deleted by id, by number, at the cursor and all;
        // an iTerm2 inline PNG; queries the image layer answers.
        let rgba = "AAAA//8AAP8=";
        let png = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";
        let input = format!(
            "\x1b[1;41mhello\x1b_Ga=T,f=32,s=2,v=1,i=7,c=4,r=2;{rgba}\x1b\\after\r\n\
             \x1b[5;27H\x1b_Ga=T,f=32,s=2,v=1,i=8,c=6,r=3,C=1;{rgba}\x1b\\x\
             \x1b[12;1H\x1b_Ga=T,f=32,s=2,v=1,I=3,c=2,r=4;{rgba}\x1b\\tail\
             \x1b_Ga=q,f=32,s=2,v=1,i=9;{rgba}\x1b\\\x1b_Ga=p,i=7,c=3,r=1\x1b\\\
             \x1b[2;1H\x1b_Ga=d,d=c\x1b\\\x1b_Ga=d,d=I,i=8\x1b\\\x1b_Ga=d,d=n,I=3\x1b\\\
             \x1b]1337;File=inline=1;width=3;height=2:{png}\x07\x1b]1337;ReportCellSize\x07\
             \x1b[c\x1b[14t\x1b[16t\x1b_Ga=t,f=32,s=2,v=1,i=10,m=1;AAAA\x1b\\\x1b_Gm=0;//8AAP8=\x1b\\\
             \x1b_Ga=p,i=10\x1b\\more\x1b[1;20H\x1b_Ga=p,i=9,c=2,r=2\x1b\\end"
        );
        check("placements", input.as_bytes());
    }
}
