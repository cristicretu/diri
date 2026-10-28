//! Exports the `Term` type which is a high-level API for the Grid.

use std::ops::{Index, IndexMut, Range};
use std::sync::Arc;
use std::{cmp, mem, ptr, slice, str};

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as Base64;
use bitflags::bitflags;
use log::{debug, trace};
use unicode_width::UnicodeWidthChar;

use crate::event::{Event, EventListener};
use crate::grid::{Dimensions, Grid, GridIterator, Scroll};
use crate::index::{self, Boundary, Column, Direction, Line, Point, Side};
use crate::selection::{Selection, SelectionRange, SelectionType};
use crate::term::cell::{Cell, Flags, LineLength};
use crate::term::color::Colors;
use crate::vi_mode::{ViModeCursor, ViMotion};
use crate::vte::ansi::{
    self, Attr, CharsetIndex, Color, CursorShape, CursorStyle, Handler, Hyperlink, KeyboardModes,
    KeyboardModesApplyBehavior, NamedColor, NamedMode, NamedPrivateMode, PrivateMode, Rgb,
    ShellCommandMark, StandardCharset,
};

pub mod cell;
pub mod color;
mod grapheme;
mod grapheme_table;
pub mod keyboard;
pub mod search;

/// Columns `text` takes as this terminal lays it out by default: the grapheme
/// cluster widths of Bun's `stringWidth`, which Claude Code assumes.
pub fn grapheme_width(text: &str) -> usize {
    grapheme::str_width(text, false)
}

/// Minimum number of columns.
///
/// A minimum of 2 is necessary to hold fullwidth unicode characters.
pub const MIN_COLUMNS: usize = 2;

/// Minimum number of visible lines.
pub const MIN_SCREEN_LINES: usize = 1;

/// Max size of the window title stack.
const TITLE_STACK_MAX_DEPTH: usize = 4096;

/// Default semantic escape characters.
pub const SEMANTIC_ESCAPE_CHARS: &str = ",│`|:\"' ()[]{}<>\t";

/// Max size of the keyboard modes.
const KEYBOARD_MODE_STACK_MAX_DEPTH: usize = TITLE_STACK_MAX_DEPTH;

/// Default tab interval, corresponding to terminfo `it` value.
const INITIAL_TABSTOPS: usize = 8;

bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub struct TermMode: u32 {
        const NONE                    = 0;
        const SHOW_CURSOR             = 1;
        const APP_CURSOR              = 1 << 1;
        const APP_KEYPAD              = 1 << 2;
        const MOUSE_REPORT_CLICK      = 1 << 3;
        const BRACKETED_PASTE         = 1 << 4;
        const SGR_MOUSE               = 1 << 5;
        const MOUSE_MOTION            = 1 << 6;
        const LINE_WRAP               = 1 << 7;
        const LINE_FEED_NEW_LINE      = 1 << 8;
        const ORIGIN                  = 1 << 9;
        const INSERT                  = 1 << 10;
        const FOCUS_IN_OUT            = 1 << 11;
        const ALT_SCREEN              = 1 << 12;
        const MOUSE_DRAG              = 1 << 13;
        const UTF8_MOUSE              = 1 << 14;
        const ALTERNATE_SCROLL        = 1 << 15;
        const VI                      = 1 << 16;
        const URGENCY_HINTS           = 1 << 17;
        const DISAMBIGUATE_ESC_CODES  = 1 << 18;
        const REPORT_EVENT_TYPES      = 1 << 19;
        const REPORT_ALTERNATE_KEYS   = 1 << 20;
        const REPORT_ALL_KEYS_AS_ESC  = 1 << 21;
        const REPORT_ASSOCIATED_TEXT  = 1 << 22;
        /// DIRI PATCH: DECSET 2031, color-scheme change reports.
        const COLOR_SCHEME_UPDATES    = 1 << 23;
        /// DIRI PATCH: DECSET 2048, in-band resize reports.
        const IN_BAND_RESIZE          = 1 << 24;
        /// DECSET 2027: grapheme cluster widths by the clustering rule.
        const GRAPHEME_CLUSTERING     = 1 << 25;
        const MOUSE_MODE              = Self::MOUSE_REPORT_CLICK.bits() | Self::MOUSE_MOTION.bits() | Self::MOUSE_DRAG.bits();
        const KITTY_KEYBOARD_PROTOCOL = Self::DISAMBIGUATE_ESC_CODES.bits()
                                      | Self::REPORT_EVENT_TYPES.bits()
                                      | Self::REPORT_ALTERNATE_KEYS.bits()
                                      | Self::REPORT_ALL_KEYS_AS_ESC.bits()
                                      | Self::REPORT_ASSOCIATED_TEXT.bits();
         const ANY                    = u32::MAX;
    }
}

impl From<KeyboardModes> for TermMode {
    fn from(value: KeyboardModes) -> Self {
        let mut mode = Self::empty();

        let disambiguate_esc_codes = value.contains(KeyboardModes::DISAMBIGUATE_ESC_CODES);
        mode.set(TermMode::DISAMBIGUATE_ESC_CODES, disambiguate_esc_codes);

        let report_event_types = value.contains(KeyboardModes::REPORT_EVENT_TYPES);
        mode.set(TermMode::REPORT_EVENT_TYPES, report_event_types);

        let report_alternate_keys = value.contains(KeyboardModes::REPORT_ALTERNATE_KEYS);
        mode.set(TermMode::REPORT_ALTERNATE_KEYS, report_alternate_keys);

        let report_all_keys_as_esc = value.contains(KeyboardModes::REPORT_ALL_KEYS_AS_ESC);
        mode.set(TermMode::REPORT_ALL_KEYS_AS_ESC, report_all_keys_as_esc);

        let report_associated_text = value.contains(KeyboardModes::REPORT_ASSOCIATED_TEXT);
        mode.set(TermMode::REPORT_ASSOCIATED_TEXT, report_associated_text);

        mode
    }
}

impl From<TermMode> for KeyboardModes {
    fn from(value: TermMode) -> Self {
        let mut flags = Self::NO_MODE;
        for (terminal, keyboard) in [
            (
                TermMode::DISAMBIGUATE_ESC_CODES,
                Self::DISAMBIGUATE_ESC_CODES,
            ),
            (TermMode::REPORT_EVENT_TYPES, Self::REPORT_EVENT_TYPES),
            (TermMode::REPORT_ALTERNATE_KEYS, Self::REPORT_ALTERNATE_KEYS),
            (
                TermMode::REPORT_ALL_KEYS_AS_ESC,
                Self::REPORT_ALL_KEYS_AS_ESC,
            ),
            (
                TermMode::REPORT_ASSOCIATED_TEXT,
                Self::REPORT_ASSOCIATED_TEXT,
            ),
        ] {
            flags.set(keyboard, value.contains(terminal));
        }
        flags
    }
}

impl Default for TermMode {
    fn default() -> TermMode {
        TermMode::SHOW_CURSOR
            | TermMode::LINE_WRAP
            | TermMode::ALTERNATE_SCROLL
            | TermMode::URGENCY_HINTS
    }
}

/// Convert a terminal point to a viewport relative point.
#[inline]
pub fn point_to_viewport(display_offset: usize, point: Point) -> Option<Point<usize>> {
    let viewport_line = point.line.0 + display_offset as i32;
    usize::try_from(viewport_line)
        .ok()
        .map(|line| Point::new(line, point.column))
}

/// Convert a viewport relative point to a terminal point.
#[inline]
pub fn viewport_to_point(display_offset: usize, point: Point<usize>) -> Point {
    let line = Line(point.line as i32) - display_offset;
    Point::new(line, point.column)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LineDamageBounds {
    /// Damaged line number.
    pub line: usize,

    /// Leftmost damaged column.
    pub left: usize,

    /// Rightmost damaged column.
    pub right: usize,
}

impl LineDamageBounds {
    #[inline]
    pub fn new(line: usize, left: usize, right: usize) -> Self {
        Self { line, left, right }
    }

    #[inline]
    pub fn undamaged(line: usize, num_cols: usize) -> Self {
        Self {
            line,
            left: num_cols,
            right: 0,
        }
    }

    #[inline]
    pub fn reset(&mut self, num_cols: usize) {
        *self = Self::undamaged(self.line, num_cols);
    }

    #[inline]
    pub fn expand(&mut self, left: usize, right: usize) {
        self.left = cmp::min(self.left, left);
        self.right = cmp::max(self.right, right);
    }

    #[inline]
    pub fn is_damaged(&self) -> bool {
        self.left <= self.right
    }
}

/// Terminal damage information collected since the last [`Term::reset_damage`] call.
#[derive(Debug)]
pub enum TermDamage<'a> {
    /// The entire terminal is damaged.
    Full,

    /// Iterator over damaged lines in the terminal.
    Partial(TermDamageIterator<'a>),
}

/// Iterator over the terminal's viewport damaged lines.
#[derive(Clone, Debug)]
pub struct TermDamageIterator<'a> {
    line_damage: slice::Iter<'a, LineDamageBounds>,
    display_offset: usize,
}

impl<'a> TermDamageIterator<'a> {
    pub fn new(line_damage: &'a [LineDamageBounds], display_offset: usize) -> Self {
        let num_lines = line_damage.len();
        // Filter out invisible damage.
        let line_damage = &line_damage[..num_lines.saturating_sub(display_offset)];
        Self {
            display_offset,
            line_damage: line_damage.iter(),
        }
    }
}

impl Iterator for TermDamageIterator<'_> {
    type Item = LineDamageBounds;

    fn next(&mut self) -> Option<Self::Item> {
        self.line_damage.find_map(|line| {
            line.is_damaged().then_some(LineDamageBounds::new(
                line.line + self.display_offset,
                line.left,
                line.right,
            ))
        })
    }
}

/// State of the terminal damage.
struct TermDamageState {
    /// Hint whether terminal should be damaged entirely regardless of the actual damage changes.
    full: bool,

    /// Information about damage on terminal lines.
    lines: Vec<LineDamageBounds>,

    /// Old terminal cursor point.
    last_cursor: Point,

    /// Diri: for each screen line, the line whose cells it held at the last
    /// reset, when scrolling moved them there and nothing damaged them since;
    /// [`CONTENT_CHANGED`] otherwise. Empty while unknown: after any full
    /// damage other than a scroll, and until the first reset.
    content_sources: Vec<u32>,
}

/// A line whose cells may differ from every line at the last damage reset.
pub const CONTENT_CHANGED: u32 = u32::MAX;

impl TermDamageState {
    fn new(num_cols: usize, num_lines: usize) -> Self {
        let lines = (0..num_lines)
            .map(|line| LineDamageBounds::undamaged(line, num_cols))
            .collect();

        Self {
            full: true,
            lines,
            last_cursor: Default::default(),
            content_sources: Vec::new(),
        }
    }

    /// Full damage whose effect on line content is not tracked.
    #[inline]
    fn mark_fully_damaged(&mut self) {
        self.full = true;
        self.content_sources.clear();
    }

    /// A line whose cells may have changed, without damage bounds.
    #[inline]
    fn content_changed(&mut self, line: Line) {
        if let Some(source) = usize::try_from(line.0)
            .ok()
            .and_then(|line| self.content_sources.get_mut(line))
        {
            *source = CONTENT_CHANGED;
        }
    }

    /// Record `positions` lines of `region` scrolling up (text moving toward
    /// the top) or down, with the uncovered lines reset. Lines outside the
    /// region keep their cells, as in `Grid::scroll_up`/`scroll_down`.
    ///
    /// `Term::input` does not damage the cells it writes: the renderer's
    /// damage covers them through cursor damage at the next cursor movement
    /// or damage query. A scroll is full damage instead, so the cursor line
    /// is marked before its cells move.
    fn scroll_content(&mut self, cursor: Line, region: Range<Line>, positions: usize, up: bool) {
        self.full = true;
        self.content_changed(cursor);
        let (Ok(start), Ok(end)) = (
            usize::try_from(region.start.0),
            usize::try_from(region.end.0),
        ) else {
            self.content_sources.clear();
            return;
        };
        if self.content_sources.is_empty() || start >= end || end > self.content_sources.len() {
            self.content_sources.clear();
            return;
        }
        let positions = positions.min(end - start);
        let sources = &mut self.content_sources[start..end];
        let kept = sources.len() - positions;
        if up {
            sources.copy_within(positions.., 0);
            sources[kept..].fill(CONTENT_CHANGED);
        } else {
            sources.copy_within(..kept, positions);
            sources[..positions].fill(CONTENT_CHANGED);
        }
    }

    #[inline]
    fn resize(&mut self, num_cols: usize, num_lines: usize) {
        // Reset point, so old cursor won't end up outside of the viewport.
        self.last_cursor = Default::default();
        self.mark_fully_damaged();

        self.lines.clear();
        self.lines.reserve(num_lines);
        for line in 0..num_lines {
            self.lines.push(LineDamageBounds::undamaged(line, num_cols));
        }
    }

    /// Damage point inside of the viewport.
    #[inline]
    fn damage_point(&mut self, point: Point<usize>) {
        self.damage_line(point.line, point.column.0, point.column.0);
    }

    /// Expand `line`'s damage to span at least `left` to `right` column.
    #[inline]
    fn damage_line(&mut self, line: usize, left: usize, right: usize) {
        self.lines[line].expand(left, right);
        if let Some(source) = self.content_sources.get_mut(line) {
            *source = CONTENT_CHANGED;
        }
    }

    /// Reset information about terminal damage.
    fn reset(&mut self, num_cols: usize) {
        self.full = false;
        self.lines.iter_mut().for_each(|line| line.reset(num_cols));
        self.content_sources.clear();
        self.content_sources.extend(0..self.lines.len() as u32);
    }
}

pub struct Term<T> {
    /// Terminal focus controlling the cursor shape.
    pub is_focused: bool,

    /// Tests only: write wide characters the way `input` did before
    /// `write_wide_pair`, to check it against.
    #[cfg(test)]
    reference_wide_writes: bool,

    /// Cursor for keyboard selection.
    pub vi_mode_cursor: ViModeCursor,

    pub selection: Option<Selection>,

    /// Currently active grid.
    ///
    /// Tracks the screen buffer currently in use. While the alternate screen buffer is active,
    /// this will be the alternate grid. Otherwise it is the primary screen buffer.
    grid: Grid<Cell>,

    /// Currently inactive grid.
    ///
    /// Opposite of the active grid. While the alternate screen buffer is active, this will be the
    /// primary grid. Otherwise it is the alternate screen buffer.
    /// None is the pristine alternate screen, allocated on first entry.
    inactive_grid: Option<Grid<Cell>>,

    /// Index into `charsets`, pointing to what ASCII is currently being mapped to.
    active_charset: CharsetIndex,

    /// Tabstops.
    tabs: TabStops,

    /// Mode flags.
    mode: TermMode,

    /// Scroll region.
    ///
    /// Range going from top to bottom of the terminal, indexed from the top of the viewport.
    scroll_region: Range<Line>,

    /// Modified terminal colors.
    colors: Colors,

    /// DIRI PATCH: advances whenever a color cells are painted with (0–255
    /// and the default foreground and background) changes, so a recolor that
    /// moves no cell is still noticed.
    colors_generation: u64,

    /// Current style of the cursor.
    cursor_style: Option<CursorStyle>,

    /// Proxy for sending events to the event loop.
    event_proxy: T,

    /// Current title of the window.
    title: Option<String>,

    /// Stack of saved window titles. When a title is popped from this stack, the `title` for the
    /// term is set.
    title_stack: Vec<Option<String>>,

    /// The stack for the keyboard modes.
    keyboard_mode_stack: Vec<KeyboardModes>,

    /// Currently inactive keyboard mode stack.
    inactive_keyboard_mode_stack: Vec<KeyboardModes>,

    /// Information about damaged cells.
    damage: TermDamageState,

    /// Config directly for the terminal.
    config: Config,
}

/// Configuration options for the [`Term`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// The maximum amount of scrolling history.
    pub scrolling_history: usize,

    /// Default cursor style to reset the cursor to.
    pub default_cursor_style: CursorStyle,

    /// Cursor style for Vi mode.
    pub vi_mode_cursor_style: Option<CursorStyle>,

    /// The characters which terminate semantic selection.
    ///
    /// The default value is [`SEMANTIC_ESCAPE_CHARS`].
    pub semantic_escape_chars: String,

    /// Whether to enable kitty keyboard protocol.
    pub kitty_keyboard: bool,

    /// OSC52 support mode.
    pub osc52: Osc52,

    /// DIRI PATCH: the name and version XTVERSION (`CSI > q`) reports. The
    /// query is unanswered when absent.
    pub terminal_version: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            scrolling_history: 10000,
            semantic_escape_chars: SEMANTIC_ESCAPE_CHARS.to_owned(),
            default_cursor_style: Default::default(),
            vi_mode_cursor_style: Default::default(),
            kitty_keyboard: Default::default(),
            osc52: Default::default(),
            terminal_version: None,
        }
    }
}

/// OSC 52 behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(
    feature = "serde",
    derive(Serialize, Deserialize),
    serde(rename_all = "lowercase")
)]
pub enum Osc52 {
    /// The handling of the escape sequence is disabled.
    Disabled,
    /// Only copy sequence is accepted.
    ///
    /// This option is the default as a compromise between entirely
    /// disabling it (the most secure) and allowing `paste` (the less secure).
    #[default]
    OnlyCopy,
    /// Only paste sequence is accepted.
    OnlyPaste,
    /// Both are accepted.
    CopyPaste,
}

impl<T> Term<T> {
    /// Release the primary grid's history caches once output is idle.
    #[cfg(feature = "compact-history")]
    pub fn release_idle_history(&mut self) {
        if self.mode.contains(TermMode::ALT_SCREEN) {
            self.inactive_grid
                .as_mut()
                .expect("primary grid")
                .release_idle_history();
        } else {
            self.grid.release_idle_history();
        }
    }

    /// DIRI PATCH: history rows of the primary screen, also while the
    /// alternate screen is shown.
    pub fn primary_history_size(&self) -> usize {
        match (
            &self.inactive_grid,
            self.mode.contains(TermMode::ALT_SCREEN),
        ) {
            (Some(primary), true) => primary.history_size(),
            _ => self.grid.history_size(),
        }
    }

    /// DIRI PATCH: changes only the history row limit, as `set_options` does
    /// for `scrolling_history`, without its damage and title side effects. A
    /// lower limit drops the oldest rows; the limit survives a reset.
    pub fn set_scrolling_history(&mut self, limit: usize) {
        self.config.scrolling_history = limit;
        if self.mode.contains(TermMode::ALT_SCREEN) {
            self.inactive_grid
                .as_mut()
                .expect("alternate screen retains the primary grid")
                .update_history(limit);
        } else {
            self.grid.update_history(limit);
        }
    }

    /// Bound primary history to `budget` stored bytes. The configured
    /// `scrolling_history` is only a ceiling: see
    /// [`Grid::bound_history_storage`].
    #[cfg(feature = "compact-history")]
    pub fn bound_primary_history_storage(&mut self, budget: usize) {
        let ceiling = self.config.scrolling_history;
        self.primary_grid_mut()
            .bound_history_storage(budget, ceiling);
    }

    /// Keep the primary history's recycled rows across history bounds until
    /// [`Term::release_primary_history_spare_rows`]: see
    /// [`Grid::keep_history_spare_rows`].
    #[cfg(feature = "compact-history")]
    pub fn keep_primary_history_spare_rows(&mut self, keep: bool) {
        self.primary_grid_mut().keep_history_spare_rows(keep);
    }

    /// Drop the primary history's recycled rows.
    #[cfg(feature = "compact-history")]
    pub fn release_primary_history_spare_rows(&mut self) {
        self.primary_grid_mut().release_history_spare_rows();
    }

    #[cfg(feature = "compact-history")]
    fn primary_grid_mut(&mut self) -> &mut Grid<Cell> {
        if self.mode.contains(TermMode::ALT_SCREEN) {
            self.inactive_grid.as_mut().expect("primary grid")
        } else {
            &mut self.grid
        }
    }

    #[inline]
    pub fn scroll_display(&mut self, scroll: Scroll)
    where
        T: EventListener,
    {
        let old_display_offset = self.grid.display_offset();
        self.grid.scroll_display(scroll);
        self.event_proxy.send_event(Event::MouseCursorDirty);

        // Clamp vi mode cursor to the viewport.
        let viewport_start = -(self.grid.display_offset() as i32);
        let viewport_end = viewport_start + self.bottommost_line().0;
        let vi_cursor_line = &mut self.vi_mode_cursor.point.line.0;
        *vi_cursor_line = cmp::min(viewport_end, cmp::max(viewport_start, *vi_cursor_line));
        self.vi_mode_recompute_selection();

        // Damage everything if display offset changed.
        if old_display_offset != self.grid().display_offset() {
            self.mark_fully_damaged();
        }
    }

    pub fn new<D: Dimensions>(config: Config, dimensions: &D, event_proxy: T) -> Term<T> {
        let num_cols = dimensions.columns();
        let num_lines = dimensions.screen_lines();

        let history_size = config.scrolling_history;
        let grid = Grid::new(num_lines, num_cols, history_size);
        let inactive_grid = None;

        let tabs = TabStops::new(grid.columns());

        let scroll_region = Line(0)..Line(grid.screen_lines() as i32);

        // Initialize terminal damage, covering the entire terminal upon launch.
        let damage = TermDamageState::new(num_cols, num_lines);

        Term {
            inactive_grid,
            scroll_region,
            event_proxy,
            damage,
            config,
            grid,
            tabs,
            inactive_keyboard_mode_stack: Default::default(),
            keyboard_mode_stack: Default::default(),
            active_charset: Default::default(),
            vi_mode_cursor: Default::default(),
            cursor_style: Default::default(),
            colors: color::Colors::default(),
            colors_generation: 0,
            title_stack: Default::default(),
            is_focused: Default::default(),
            #[cfg(test)]
            reference_wide_writes: false,
            selection: Default::default(),
            title: Default::default(),
            mode: Default::default(),
        }
    }

    /// Collect the information about the changes in the lines, which
    /// could be used to minimize the amount of drawing operations.
    ///
    /// The user controlled elements, like `Vi` mode cursor and `Selection` are **not** part of the
    /// collected damage state. Those could easily be tracked by comparing their old and new
    /// value between adjacent frames.
    ///
    /// After reading damage [`reset_damage`] should be called.
    ///
    /// [`reset_damage`]: Self::reset_damage
    #[must_use]
    pub fn damage(&mut self) -> TermDamage<'_> {
        // Ensure the entire terminal is damaged after entering insert mode.
        // Leaving is handled in the ansi handler.
        if self.mode.contains(TermMode::INSERT) {
            self.mark_fully_damaged();
        }

        let previous_cursor = mem::replace(&mut self.damage.last_cursor, self.grid.cursor.point);

        if self.damage.full {
            // Diri: full damage skips the cursor damage below, which is what
            // covers cells `input` wrote at the cursor since it last moved.
            self.damage.content_changed(previous_cursor.line);
            self.damage.content_changed(self.grid.cursor.point.line);
            return TermDamage::Full;
        }

        // Add information about old cursor position and new one if they are not the same, so we
        // cover everything that was produced by `Term::input`.
        if self.damage.last_cursor != previous_cursor {
            // Cursor coordinates are always inside viewport even if you have `display_offset`.
            let point = Point::new(previous_cursor.line.0 as usize, previous_cursor.column);
            self.damage.damage_point(point);
        }

        // Always damage current cursor.
        self.damage_cursor();

        // NOTE: damage which changes all the content when the display offset is non-zero (e.g.
        // scrolling) is handled via full damage.
        let display_offset = self.grid().display_offset();
        TermDamage::Partial(TermDamageIterator::new(&self.damage.lines, display_offset))
    }

    /// Resets the terminal damage information.
    pub fn reset_damage(&mut self) {
        self.damage.reset(self.columns());
    }

    #[inline]
    fn mark_fully_damaged(&mut self) {
        self.damage.mark_fully_damaged();
    }

    /// Diri: where each screen line's cells were at the last
    /// [`reset_damage`], for damage collected since then; `None` when that
    /// is unknown. A line holds the cells line `sources[line]` held then,
    /// unless it is [`CONTENT_CHANGED`]. Scrolling moves lines without
    /// forgetting them, so a consumer can reuse per-line work after full
    /// damage. This is only as complete as line damage itself: every cell
    /// write that is not covered by full damage must damage its line.
    ///
    /// [`reset_damage`]: Self::reset_damage
    #[must_use]
    pub fn damage_content_sources(&self) -> Option<&[u32]> {
        (self.damage.content_sources.len() == self.damage.lines.len())
            .then_some(&self.damage.content_sources[..])
    }

    /// Set new options for the [`Term`].
    /// DIRI PATCH: the options in effect, so a caller changing one keeps
    /// the rest.
    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn set_options(&mut self, options: Config)
    where
        T: EventListener,
    {
        let old_config = mem::replace(&mut self.config, options);

        let title_event = match &self.title {
            Some(title) => Event::Title(title.clone()),
            None => Event::ResetTitle,
        };

        self.event_proxy.send_event(title_event);

        if self.mode.contains(TermMode::ALT_SCREEN) {
            self.inactive_grid
                .as_mut()
                .expect("alternate screen retains the primary grid")
                .update_history(self.config.scrolling_history);
        } else {
            self.grid.update_history(self.config.scrolling_history);
        }

        if self.config.kitty_keyboard != old_config.kitty_keyboard {
            self.keyboard_mode_stack = Vec::new();
            self.inactive_keyboard_mode_stack = Vec::new();
            self.mode.remove(TermMode::KITTY_KEYBOARD_PROTOCOL);
        }

        // Damage everything on config updates.
        self.mark_fully_damaged();
    }

    /// Convert the active selection to a String.
    pub fn selection_to_string(&self) -> Option<String> {
        let selection_range = self.selection.as_ref().and_then(|s| s.to_range(self))?;
        let SelectionRange { start, end, .. } = selection_range;

        let mut res = String::new();

        match self.selection.as_ref() {
            Some(Selection {
                ty: SelectionType::Block,
                ..
            }) => {
                for line in (start.line.0..end.line.0).map(Line::from) {
                    res += self
                        .line_to_string(line, start.column..end.column, start.column.0 != 0)
                        .trim_end();
                    res += "\n";
                }

                res += self
                    .line_to_string(end.line, start.column..end.column, true)
                    .trim_end();
            }
            Some(Selection {
                ty: SelectionType::Lines,
                ..
            }) => {
                res = self.bounds_to_string(start, end) + "\n";
            }
            _ => {
                res = self.bounds_to_string(start, end);
            }
        }

        Some(res)
    }

    /// Convert range between two points to a String.
    pub fn bounds_to_string(&self, start: Point, end: Point) -> String {
        let mut res = String::new();

        for line in (start.line.0..=end.line.0).map(Line::from) {
            let start_col = if line == start.line {
                start.column
            } else {
                Column(0)
            };
            let end_col = if line == end.line {
                end.column
            } else {
                self.last_column()
            };

            res += &self.line_to_string(line, start_col..end_col, line == end.line);
        }

        res.strip_suffix('\n').map(str::to_owned).unwrap_or(res)
    }

    /// Convert a single line in the grid to a String.
    fn line_to_string(
        &self,
        line: Line,
        mut cols: Range<Column>,
        include_wrapped_wide: bool,
    ) -> String {
        let mut text = String::new();

        let grid_line = &self.grid[line];
        let line_length = cmp::min(grid_line.line_length(), cols.end + 1);

        // Include wide char when trailing spacer is selected.
        if grid_line[cols.start]
            .flags
            .contains(Flags::WIDE_CHAR_SPACER)
        {
            cols.start -= 1;
        }

        let mut tab_mode = false;
        for column in (cols.start.0..line_length.0).map(Column::from) {
            let cell = &grid_line[column];

            // Skip over cells until next tab-stop once a tab was found.
            if tab_mode {
                if self.tabs[column] || cell.c != ' ' {
                    tab_mode = false;
                } else {
                    continue;
                }
            }

            if cell.c == '\t' {
                tab_mode = true;
            }

            if !cell
                .flags
                .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
            {
                // Push cells primary character.
                text.push(cell.c);

                // Push zero-width characters.
                for c in cell.zerowidth().into_iter().flatten() {
                    text.push(*c);
                }
            }
        }

        if cols.end >= self.columns() - 1
            && (line_length.0 == 0
                || !self.grid[line][line_length - 1]
                    .flags
                    .contains(Flags::WRAPLINE))
        {
            text.push('\n');
        }

        // If wide char is not part of the selection, but leading spacer is, include it.
        if line_length == self.columns()
            && line_length.0 >= 2
            && grid_line[line_length - 1]
                .flags
                .contains(Flags::LEADING_WIDE_CHAR_SPACER)
            && include_wrapped_wide
        {
            text.push(self.grid[line - 1i32][Column(0)].c);
        }

        text
    }

    /// Terminal content required for rendering.
    #[inline]
    pub fn renderable_content(&self) -> RenderableContent<'_>
    where
        T: EventListener,
    {
        RenderableContent::new(self)
    }

    /// Access to the raw grid data structure.
    pub fn grid(&self) -> &Grid<Cell> {
        &self.grid
    }

    /// Mutable access to the raw grid data structure.
    pub fn grid_mut(&mut self) -> &mut Grid<Cell> {
        &mut self.grid
    }

    /// Resize terminal to new dimensions.
    pub fn resize<S: Dimensions>(&mut self, size: S) {
        let old_cols = self.columns();
        let old_lines = self.screen_lines();

        let num_cols = size.columns();
        let num_lines = size.screen_lines();

        if old_cols == num_cols && old_lines == num_lines {
            debug!("Term::resize dimensions unchanged");
            return;
        }

        debug!("New num_cols is {num_cols} and num_lines is {num_lines}");

        // Move vi mode cursor with the content.
        let history_size = self.history_size();
        let mut delta = num_lines as i32 - old_lines as i32;
        let min_delta = cmp::min(0, num_lines as i32 - self.grid.cursor.point.line.0 - 1);
        delta = cmp::min(cmp::max(delta, min_delta), history_size as i32);
        self.vi_mode_cursor.point.line += delta;

        let is_alt = self.mode.contains(TermMode::ALT_SCREEN);
        // Reflow may change the physical row count; only the configured
        // ceiling limits it by rows. The caller's byte bound trims afterwards.
        #[cfg(feature = "compact-history")]
        {
            let ceiling = self.config.scrolling_history;
            self.primary_grid_mut().restore_history_limit(ceiling);
        }
        self.grid.resize(!is_alt, num_lines, num_cols);
        if let Some(inactive_grid) = self.inactive_grid.as_mut() {
            inactive_grid.resize(is_alt, num_lines, num_cols);
        }

        // Invalidate selection and tabs only when necessary.
        if old_cols != num_cols {
            self.selection = None;

            // Recreate tabs list.
            self.tabs.resize(num_cols);
        } else if let Some(selection) = self.selection.take() {
            let max_lines = cmp::max(num_lines, old_lines) as i32;
            let range = Line(0)..Line(max_lines);
            self.selection = selection.rotate(self, &range, -delta);
        }

        // Clamp vi cursor to viewport.
        let vi_point = self.vi_mode_cursor.point;
        let viewport_top = Line(-(self.grid.display_offset() as i32));
        let viewport_bottom = viewport_top + self.bottommost_line();
        self.vi_mode_cursor.point.line =
            cmp::max(cmp::min(vi_point.line, viewport_bottom), viewport_top);
        self.vi_mode_cursor.point.column = cmp::min(vi_point.column, self.last_column());

        // Reset scrolling region.
        self.scroll_region = Line(0)..Line(self.screen_lines() as i32);

        // Resize damage information.
        self.damage.resize(num_cols, num_lines);
    }

    /// Active terminal modes.
    #[inline]
    pub fn mode(&self) -> &TermMode {
        &self.mode
    }

    /// Swap primary and alternate screen buffer.
    pub fn swap_alt(&mut self) {
        self.swap_alt_screen(AltScreen::SaveCursorAndClear);
    }

    /// DIRI PATCH: the three alternate-screen modes differ only in what
    /// happens to the cursor and the alternate screen's contents.
    fn swap_alt_screen(&mut self, kind: AltScreen) {
        let num_lines = self.grid.screen_lines();
        let num_cols = self.grid.columns();
        let entering = !self.mode.contains(TermMode::ALT_SCREEN);
        // xterm's 47 and 1047 share one cursor between the two screens.
        let shared_cursor =
            (kind != AltScreen::SaveCursorAndClear).then(|| self.grid.cursor.clone());
        if !entering && kind == AltScreen::ClearOnExit {
            self.grid.reset_region(..);
        }
        let inactive_grid = self
            .inactive_grid
            .get_or_insert_with(|| Grid::new(num_lines, num_cols, 0));
        if entering && kind == AltScreen::SaveCursorAndClear {
            // Set alt screen cursor to the current primary screen cursor.
            inactive_grid.cursor = self.grid.cursor.clone();

            // Drop information about the primary screens saved cursor.
            self.grid.saved_cursor = self.grid.cursor.clone();

            // Reset alternate screen contents.
            inactive_grid.reset_region(..);
        }

        mem::swap(
            &mut self.keyboard_mode_stack,
            &mut self.inactive_keyboard_mode_stack,
        );
        let keyboard_mode = self
            .keyboard_mode_stack
            .last()
            .copied()
            .unwrap_or(KeyboardModes::NO_MODE)
            .into();
        self.set_keyboard_mode(keyboard_mode, KeyboardModesApplyBehavior::Replace);

        mem::swap(
            &mut self.grid,
            self.inactive_grid
                .as_mut()
                .expect("alternate screen initialized before swap"),
        );
        self.mode ^= TermMode::ALT_SCREEN;
        if let Some(cursor) = shared_cursor {
            self.grid.cursor = cursor;
        }
        self.selection = None;
        self.mark_fully_damaged();
    }

    /// DIRI PATCH: the SGR state of new text, as DECRQSS `m` reports it.
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

        let template = &self.grid.cursor.template;
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
            if template.flags.contains(flag) {
                out.push(';');
                out.push_str(code);
            }
        }
        color(&mut out, template.fg, 30, 90, 38);
        color(&mut out, template.bg, 40, 100, 48);
        if let Some(underline) = template.underline_color() {
            color(&mut out, underline, 58, 58, 58);
        }
        out.push('m');
        out
    }

    /// Scroll screen down.
    ///
    /// Text moves down; clear at bottom
    /// Expects origin to be in scroll range.
    #[inline]
    fn scroll_down_relative(&mut self, origin: Line, mut lines: usize) {
        trace!("Scrolling down relative: origin={origin}, lines={lines}");

        lines = cmp::min(
            lines,
            (self.scroll_region.end - self.scroll_region.start).0 as usize,
        );
        lines = cmp::min(lines, (self.scroll_region.end - origin).0 as usize);

        let region = origin..self.scroll_region.end;

        // Scroll selection.
        self.selection = self
            .selection
            .take()
            .and_then(|s| s.rotate(self, &region, -(lines as i32)));

        // Scroll vi mode cursor.
        let line = &mut self.vi_mode_cursor.point.line;
        if region.start <= *line && region.end > *line {
            *line = cmp::min(*line + lines, region.end - 1);
        }

        // Scroll between origin and bottom
        self.grid.scroll_down(&region, lines);
        self.damage
            .scroll_content(self.grid.cursor.point.line, region, lines, false);
    }

    /// Scroll screen up
    ///
    /// Text moves up; clear at top
    /// Expects origin to be in scroll range.
    #[inline]
    fn scroll_up_relative(&mut self, origin: Line, mut lines: usize) {
        trace!("Scrolling up relative: origin={origin}, lines={lines}");

        lines = cmp::min(
            lines,
            (self.scroll_region.end - self.scroll_region.start).0 as usize,
        );

        let region = origin..self.scroll_region.end;

        // Scroll selection.
        self.selection = self
            .selection
            .take()
            .and_then(|s| s.rotate(self, &region, lines as i32));

        self.grid.scroll_up(&region, lines);
        self.damage
            .scroll_content(self.grid.cursor.point.line, region.clone(), lines, true);

        // Scroll vi mode cursor.
        let viewport_top = Line(-(self.grid.display_offset() as i32));
        let top = if region.start == 0 {
            viewport_top
        } else {
            region.start
        };
        let line = &mut self.vi_mode_cursor.point.line;
        if (top <= *line) && region.end > *line {
            *line = cmp::max(*line - lines, top);
        }
    }

    fn deccolm(&mut self)
    where
        T: EventListener,
    {
        // Setting 132 column font makes no sense, but run the other side effects.
        // Clear scrolling region.
        self.set_scrolling_region(1, None);

        // Clear grid.
        self.grid.reset_region(..);
        self.mark_fully_damaged();
    }

    #[inline]
    pub fn exit(&mut self)
    where
        T: EventListener,
    {
        self.event_proxy.send_event(Event::Exit);
    }

    /// Toggle the vi mode.
    #[inline]
    pub fn toggle_vi_mode(&mut self)
    where
        T: EventListener,
    {
        self.mode ^= TermMode::VI;

        if self.mode.contains(TermMode::VI) {
            let display_offset = self.grid.display_offset() as i32;
            if self.grid.cursor.point.line > self.bottommost_line() - display_offset {
                // Move cursor to top-left if terminal cursor is not visible.
                let point = Point::new(Line(-display_offset), Column(0));
                self.vi_mode_cursor = ViModeCursor::new(point);
            } else {
                // Reset vi mode cursor position to match primary cursor.
                self.vi_mode_cursor = ViModeCursor::new(self.grid.cursor.point);
            }
        }

        // Update UI about cursor blinking state changes.
        self.event_proxy.send_event(Event::CursorBlinkingChange);
    }

    /// Move vi mode cursor.
    #[inline]
    pub fn vi_motion(&mut self, motion: ViMotion)
    where
        T: EventListener,
    {
        // Require vi mode to be active.
        if !self.mode.contains(TermMode::VI) {
            return;
        }

        // Move cursor.
        self.vi_mode_cursor = self.vi_mode_cursor.motion(self, motion);
        self.vi_mode_recompute_selection();
    }

    /// Move vi cursor to a point in the grid.
    #[inline]
    pub fn vi_goto_point(&mut self, point: Point)
    where
        T: EventListener,
    {
        // Move viewport to make point visible.
        self.scroll_to_point(point);

        // Move vi cursor to the point.
        self.vi_mode_cursor.point = point;

        self.vi_mode_recompute_selection();
    }

    /// Update the active selection to match the vi mode cursor position.
    #[inline]
    fn vi_mode_recompute_selection(&mut self) {
        // Require vi mode to be active.
        if !self.mode.contains(TermMode::VI) {
            return;
        }

        // Update only if non-empty selection is present.
        if let Some(selection) = self.selection.as_mut().filter(|s| !s.is_empty()) {
            selection.update(self.vi_mode_cursor.point, Side::Left);
            selection.include_all();
        }
    }

    /// Scroll display to point if it is outside of viewport.
    pub fn scroll_to_point(&mut self, point: Point)
    where
        T: EventListener,
    {
        let display_offset = self.grid.display_offset() as i32;
        let screen_lines = self.grid.screen_lines() as i32;

        if point.line < -display_offset {
            let lines = point.line + display_offset;
            self.scroll_display(Scroll::Delta(-lines.0));
        } else if point.line >= (screen_lines - display_offset) {
            let lines = point.line + display_offset - screen_lines + 1i32;
            self.scroll_display(Scroll::Delta(-lines.0));
        }
    }

    /// Jump to the end of a wide cell.
    pub fn expand_wide(&self, mut point: Point, direction: Direction) -> Point {
        let flags = self.grid[point.line][point.column].flags;

        match direction {
            Direction::Right if flags.contains(Flags::LEADING_WIDE_CHAR_SPACER) => {
                point.column = Column(1);
                point.line += 1;
            }
            Direction::Right if flags.contains(Flags::WIDE_CHAR) => {
                point.column = cmp::min(point.column + 1, self.last_column());
            }
            Direction::Left if flags.intersects(Flags::WIDE_CHAR | Flags::WIDE_CHAR_SPACER) => {
                if flags.contains(Flags::WIDE_CHAR_SPACER) {
                    point.column -= 1;
                }

                let prev = point.sub(self, Boundary::Grid, 1);
                if self.grid[prev]
                    .flags
                    .contains(Flags::LEADING_WIDE_CHAR_SPACER)
                {
                    point = prev;
                }
            }
            _ => (),
        }

        point
    }

    #[inline]
    pub fn semantic_escape_chars(&self) -> &str {
        &self.config.semantic_escape_chars
    }

    #[cfg(test)]
    pub(crate) fn set_semantic_escape_chars(&mut self, semantic_escape_chars: &str) {
        self.config.semantic_escape_chars = semantic_escape_chars.into();
    }

    /// Active terminal cursor style.
    ///
    /// While vi mode is active, this will automatically return the vi mode cursor style.
    #[inline]
    pub fn cursor_style(&self) -> CursorStyle {
        let cursor_style = self
            .cursor_style
            .unwrap_or(self.config.default_cursor_style);

        if self.mode.contains(TermMode::VI) {
            self.config.vi_mode_cursor_style.unwrap_or(cursor_style)
        } else {
            cursor_style
        }
    }

    pub fn colors(&self) -> &Colors {
        &self.colors
    }

    /// DIRI PATCH: see `colors_generation`. The cursor color does not count;
    /// no cell is painted with it.
    pub fn colors_generation(&self) -> u64 {
        self.colors_generation
    }

    /// Insert a linebreak at the current cursor position.
    #[inline]
    fn wrapline(&mut self)
    where
        T: EventListener,
    {
        if !self.mode.contains(TermMode::LINE_WRAP) {
            return;
        }

        trace!("Wrapping input");

        self.grid.cursor_cell().flags.insert(Flags::WRAPLINE);

        if self.grid.cursor.point.line + 1 >= self.scroll_region.end {
            self.linefeed();
        } else {
            self.damage_cursor();
            self.grid.cursor.point.line += 1;
        }

        self.grid.cursor.point.column = Column(0);
        self.grid.cursor.input_needs_wrap = false;
        self.damage_cursor();
    }

    /// DIRI PATCH: `input` for a character that is not a control, with its
    /// `info` already looked up.
    #[inline(never)]
    fn input_measured(&mut self, c: char, info: grapheme::Info)
    where
        T: EventListener,
    {
        if grapheme::may_join(c, info) && self.continue_cluster(c, info) {
            return;
        }
        let width = info.width();

        // Handle zero-width characters.
        if width == 0 {
            // Get previous column.
            let mut column = self.grid.cursor.point.column;
            if !self.grid.cursor.input_needs_wrap {
                column.0 = column.saturating_sub(1);
            }

            // Put zerowidth characters over first fullwidth character cell.
            let line = self.grid.cursor.point.line;
            if self.grid[line][column]
                .flags
                .contains(Flags::WIDE_CHAR_SPACER)
            {
                column.0 = column.saturating_sub(1);
            }

            self.grid[line][column].push_zerowidth(c);
            return;
        }

        // Move cursor to next line.
        if self.grid.cursor.input_needs_wrap {
            self.wrapline();
        }

        // If in insert mode, first shift cells to the right.
        let columns = self.columns();
        if self.mode.contains(TermMode::INSERT) && self.grid.cursor.point.column + width < columns {
            let line = self.grid.cursor.point.line;
            let col = self.grid.cursor.point.column;
            let row = &mut self.grid[line][..];

            for col in (col.0..(columns - width)).rev() {
                row.swap(col + width, col);
            }
        }

        if width == 1 {
            self.write_at_cursor(c);
        } else {
            if self.grid.cursor.point.column + 1 >= columns {
                if self.mode.contains(TermMode::LINE_WRAP) {
                    // Insert placeholder before wide char if glyph does not fit in this row.
                    self.grid
                        .cursor
                        .template
                        .flags
                        .insert(Flags::LEADING_WIDE_CHAR_SPACER);
                    self.write_at_cursor(' ');
                    self.grid
                        .cursor
                        .template
                        .flags
                        .remove(Flags::LEADING_WIDE_CHAR_SPACER);
                    self.wrapline();
                } else {
                    // Prevent out of bounds crash when linewrapping is disabled.
                    self.grid.cursor.input_needs_wrap = true;
                    return;
                }
            }

            if !self.write_wide_pair(c) {
                // Write full width glyph to current cursor cell.
                self.grid.cursor.template.flags.insert(Flags::WIDE_CHAR);
                self.write_at_cursor(c);
                self.grid.cursor.template.flags.remove(Flags::WIDE_CHAR);

                // Write spacer to cell following the wide glyph.
                self.grid.cursor.point.column += 1;
                self.grid
                    .cursor
                    .template
                    .flags
                    .insert(Flags::WIDE_CHAR_SPACER);
                self.write_at_cursor(' ');
                self.grid
                    .cursor
                    .template
                    .flags
                    .remove(Flags::WIDE_CHAR_SPACER);
            }
        }

        if self.grid.cursor.point.column + 1 < columns {
            self.grid.cursor.point.column += 1;
        } else {
            self.grid.cursor.input_needs_wrap = true;
        }
    }

    /// DIRI PATCH: writes the characters of `text` from byte `start` that
    /// start a cluster of their own and fit in the cursor row, as `input`
    /// would, and returns where it stopped. Writes one character through
    /// `input` when none can take this path.
    #[inline]
    fn input_row_segment(&mut self, text: &str, start: usize) -> usize
    where
        T: EventListener,
    {
        // Columns of a character that starts a cluster of its own and takes
        // one or two columns, as `input` measures it; `None` for anything
        // that may join the cluster before it or is zero-width.
        #[inline(always)]
        fn own_width(c: char, info: grapheme::Info) -> Option<usize> {
            if grapheme::may_join(c, info) {
                return None;
            }
            match info.width() {
                width @ (1 | 2) => Some(width),
                _ => None,
            }
        }
        let first = text[start..]
            .chars()
            .next()
            .expect("start is on a char boundary");
        if first < '\u{A0}' {
            // ASCII or a C1 control, from a caller other than the parser.
            self.input(first);
            return start + first.len_utf8();
        }
        let first_info = grapheme::Info::of(first);
        if own_width(first, first_info).is_none() {
            // Marks and joiners, one at a time, as `input` takes them.
            self.input_measured(first, first_info);
            return start + first.len_utf8();
        }
        let columns = self.columns();
        let point = self.grid.cursor.point;
        let mut column = point.column.0;
        let mut index = start;
        let mut wrap = false;
        #[cfg(test)]
        let fast = !self.reference_wide_writes;
        #[cfg(not(test))]
        let fast = true;
        if fast && !self.grid.cursor.input_needs_wrap && !self.mode.contains(TermMode::INSERT) {
            let charset = self.grid.cursor.charsets[self.active_charset];
            let template = &self.grid.cursor.template;
            let (fg, bg, flags) = (template.fg, template.bg, template.flags);
            let extra = template.extra.clone();
            let spacer = charset.map(' ');
            let row = &mut self.grid[point.line];
            for c in text[start..].chars() {
                if c < '\u{A0}' {
                    break;
                }
                let Some(width) = own_width(c, grapheme::Info::of(c)) else {
                    break;
                };
                if column + width > columns {
                    break;
                }
                // Overwriting a wide character or spacer also clears its
                // partner cells; leave that to `input`.
                let wide = Flags::WIDE_CHAR | Flags::WIDE_CHAR_SPACER;
                if row[Column(column)].flags.intersects(wide)
                    || (width == 2 && row[Column(column + 1)].flags.intersects(wide))
                {
                    break;
                }
                let cells = &mut row[Column(column)..Column(column + width)];
                let cell = &mut cells[0];
                cell.c = charset.map(c);
                cell.fg = fg;
                cell.bg = bg;
                cell.extra = extra.clone();
                if width == 1 {
                    cell.flags = flags | (cell.flags & Flags::SHELL_MARKS);
                } else {
                    cell.flags = flags | Flags::WIDE_CHAR | (cell.flags & Flags::SHELL_MARKS);
                    let cell = &mut cells[1];
                    cell.c = spacer;
                    cell.fg = fg;
                    cell.bg = bg;
                    cell.flags =
                        flags | Flags::WIDE_CHAR_SPACER | (cell.flags & Flags::SHELL_MARKS);
                    cell.extra = extra.clone();
                }
                index += c.len_utf8();
                column += width;
                if column == columns {
                    wrap = true;
                    break;
                }
            }
        }
        if index == start {
            self.input_measured(first, first_info);
            return start + first.len_utf8();
        }
        if wrap {
            self.grid.cursor.point.column = Column(columns - 1);
            self.grid.cursor.input_needs_wrap = true;
        } else {
            self.grid.cursor.point.column = Column(column);
        }
        index
    }

    /// DIRI PATCH: writes the wide character `c` and its spacer at the
    /// cursor, leaving the cursor on the spacer, when neither cell is part of
    /// an earlier wide character. Exactly the two `write_at_cursor` calls of
    /// `input`, with the row looked up once; returns false, writing nothing,
    /// when they would also have to clear an old wide character.
    #[inline]
    fn write_wide_pair(&mut self, c: char) -> bool {
        #[cfg(test)]
        if self.reference_wide_writes {
            return false;
        }
        let point = self.grid.cursor.point;
        if point.column.0 + 1 >= self.columns() {
            return false;
        }
        let row = &self.grid[point.line];
        let wide = Flags::WIDE_CHAR | Flags::WIDE_CHAR_SPACER;
        if row[point.column].flags.intersects(wide) || row[point.column + 1].flags.intersects(wide)
        {
            return false;
        }
        // Non-ASCII characters and the spacer's blank map to themselves in
        // every charset, but keep the mapping `write_at_cursor` applies.
        let charset = self.grid.cursor.charsets[self.active_charset];
        let (c, blank) = (charset.map(c), charset.map(' '));
        let template = &self.grid.cursor.template;
        let (fg, bg, flags, extra) = (
            template.fg,
            template.bg,
            template.flags,
            template.extra.clone(),
        );
        let row = &mut self.grid[point.line];
        let [glyph, spacer] = &mut row[point.column..point.column + 2] else {
            unreachable!("two cells")
        };
        glyph.c = c;
        glyph.fg = fg;
        glyph.bg = bg;
        glyph.flags = flags | Flags::WIDE_CHAR | (glyph.flags & Flags::SHELL_MARKS);
        glyph.extra = extra.clone();
        spacer.c = blank;
        spacer.fg = fg;
        spacer.bg = bg;
        spacer.flags = flags | Flags::WIDE_CHAR_SPACER | (spacer.flags & Flags::SHELL_MARKS);
        spacer.extra = extra;
        self.grid.cursor.point.column += 1;
        true
    }

    /// Write `c` to the cell at the cursor position.
    #[inline(always)]
    fn write_at_cursor(&mut self, c: char) {
        let c = self.grid.cursor.charsets[self.active_charset].map(c);
        let fg = self.grid.cursor.template.fg;
        let bg = self.grid.cursor.template.bg;
        let flags = self.grid.cursor.template.flags
            | (self.grid[self.grid.cursor.point].flags & Flags::SHELL_MARKS);
        let extra = self.grid.cursor.template.extra.clone();

        let mut cursor_cell = self.grid.cursor_cell();

        // Clear all related cells when overwriting a fullwidth cell.
        if cursor_cell
            .flags
            .intersects(Flags::WIDE_CHAR | Flags::WIDE_CHAR_SPACER)
        {
            // Remove wide char and spacer.
            let wide = cursor_cell.flags.contains(Flags::WIDE_CHAR);
            let point = self.grid.cursor.point;
            if wide && point.column < self.last_column() {
                self.grid[point.line][point.column + 1]
                    .flags
                    .remove(Flags::WIDE_CHAR_SPACER);
            } else if point.column > 0 {
                self.grid[point.line][point.column - 1].clear_wide();
            }

            // Remove leading spacers.
            if point.column <= 1 && point.line != self.topmost_line() {
                let column = self.last_column();
                self.grid[point.line - 1i32][column]
                    .flags
                    .remove(Flags::LEADING_WIDE_CHAR_SPACER);
                // Diri: this cell is outside the cursor damage that covers
                // `input`; the renderer never draws the spacer flag.
                self.damage.content_changed(point.line - 1i32);
            }

            cursor_cell = self.grid.cursor_cell();
        }

        cursor_cell.c = c;
        cursor_cell.fg = fg;
        cursor_cell.bg = bg;
        cursor_cell.flags = flags;
        cursor_cell.extra = extra;
    }

    /// Adds `c` to the grapheme cluster in the cell before the cursor when no
    /// cluster boundary separates them, resizing that cell when the cluster's
    /// width changes (`❤` + VS16 becomes two columns, a second regional
    /// indicator makes a two-column flag). Returns false when `c` starts a
    /// new cluster or takes columns of its own.
    ///
    /// The cluster is read back from the cell, so no state outlives a cursor
    /// move: a cell holds one cluster's codepoints in order.
    fn continue_cluster(&mut self, c: char, info: grapheme::Info) -> bool
    where
        T: EventListener,
    {
        // Real clusters hold a handful of codepoints; past this a run of
        // joiners stops growing one cell without bound.
        const MAX_CLUSTER_CHARS: usize = 32;

        let point = self.grid.cursor.point;
        let mut column = point.column;
        if !self.grid.cursor.input_needs_wrap {
            if column.0 == 0 {
                return false;
            }
            column -= 1;
        }
        let line = point.line;
        if self.grid[line][column]
            .flags
            .contains(Flags::WIDE_CHAR_SPACER)
        {
            if column.0 == 0 {
                return false;
            }
            column -= 1;
        }
        let cell = &self.grid[line][column];
        if cell
            .flags
            .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
        {
            return false;
        }
        let zerowidth = cell.zerowidth().unwrap_or(&[]);
        if zerowidth.len() >= MAX_CLUSTER_CHARS {
            return false;
        }
        // Cheap rejections before replaying the cell, for runs of marks and
        // emoji. A codepoint rejected here lands exactly as it would after a
        // cluster boundary: a zero-width mark on this cell, anything else in
        // cells of its own.
        let clustering = self.mode.contains(TermMode::GRAPHEME_CLUSTERING);
        let first = grapheme::Info::of(cell.c);
        let base = first.class();
        if !grapheme::is_presentation_mark(c) {
            let alone_presents = grapheme::Cluster::new(cell.c, first).presentation();
            if info.width() == 0 {
                // A mark changes no width, except that a second codepoint
                // makes a lone regional indicator a two-column cluster.
                if !(zerowidth.is_empty() && alone_presents) {
                    return false;
                }
            } else if !clustering
                && !matches!(
                    info.class(),
                    grapheme::Class::RegionalIndicator
                        | grapheme::Class::EmojiModifier
                        | grapheme::Class::ExtendedPictographic
                        | grapheme::Class::EmojiModifierBase
                )
                && !alone_presents
                && !zerowidth.iter().any(|&z| grapheme::sets_presentation(z))
            {
                // By default a codepoint with columns keeps them unless an
                // emoji rule decides the cluster's width.
                return false;
            }
        }
        match info.class() {
            grapheme::Class::ExtendedPictographic
            | grapheme::Class::EmojiModifierBase
            | grapheme::Class::IndicConsonant
                if zerowidth.is_empty() =>
            {
                return false;
            }
            grapheme::Class::RegionalIndicator
                if !zerowidth.is_empty() || base != grapheme::Class::RegionalIndicator =>
            {
                return false;
            }
            _ => (),
        }

        // Replay the cell's cluster to recover the break state.
        let mut cluster = grapheme::Cluster::new(cell.c, first);
        let mut previous = base;
        let mut state = grapheme::BreakState::Default;
        for &z in zerowidth {
            let z_info = grapheme::Info::of(z);
            if grapheme::breaks(previous, z_info.class(), &mut state) {
                // The cell holds a zero-width cluster after its own (a stray
                // mark); leave it alone.
                return false;
            }
            cluster.add(z, z_info);
            previous = z_info.class();
        }
        if grapheme::breaks(previous, info.class(), &mut state) {
            return false;
        }

        let before = cluster.width(clustering);
        cluster.add(c, info);
        let after = cluster.width(clustering);
        // By default a joined codepoint with columns of its own (a spacing
        // mark, a second Hangul lead) keeps its own cells: the cluster is as
        // wide as the sum of its codepoints, which per-cell layout gives.
        if !clustering && info.width() > 0 && !cluster.presentation() {
            return false;
        }

        let cell_width = if cell.flags.contains(Flags::WIDE_CHAR) {
            2
        } else {
            1
        };
        self.grid[line][column].push_zerowidth(c);
        if after != before && before == cell_width {
            match after {
                2 => self.widen_cluster(line, column),
                1 => self.narrow_cluster(line, column),
                _ => (),
            }
        }
        true
    }

    /// Turns the one-column cluster at `column`, just before the cursor (or
    /// under a cursor waiting to wrap), into a two-column one.
    fn widen_cluster(&mut self, line: Line, column: Column)
    where
        T: EventListener,
    {
        let columns = self.columns();
        if columns < 2 {
            return;
        }
        if column.0 + 1 >= columns {
            // No room on this row: the cluster moves to the next one, as a
            // wide character that does not fit would have.
            if !self.mode.contains(TermMode::LINE_WRAP) {
                return;
            }
            let cluster = self.grid[line][column].clone();
            self.grid.cursor.input_needs_wrap = false;
            self.grid.cursor.point.column = column;
            self.grid
                .cursor
                .template
                .flags
                .insert(Flags::LEADING_WIDE_CHAR_SPACER);
            self.write_at_cursor(' ');
            self.grid
                .cursor
                .template
                .flags
                .remove(Flags::LEADING_WIDE_CHAR_SPACER);
            self.damage.content_changed(line);
            self.wrapline();
            self.grid.cursor.template.flags.insert(Flags::WIDE_CHAR);
            self.write_at_cursor(cluster.c);
            self.grid.cursor.template.flags.remove(Flags::WIDE_CHAR);
            let shell_marks = self.grid.cursor_cell().flags & Flags::SHELL_MARKS;
            let target = self.grid.cursor_cell();
            *target = cluster;
            target.flags.remove(Flags::SHELL_MARKS | Flags::WRAPLINE);
            target.flags.insert(Flags::WIDE_CHAR | shell_marks);
        } else {
            self.grid[line][column].flags.insert(Flags::WIDE_CHAR);
            self.grid.cursor.input_needs_wrap = false;
            self.grid.cursor.point.column = column;
        }

        // The spacer takes the next column, shifting the row in insert mode.
        self.grid.cursor.point.column += 1;
        let spacer = self.grid.cursor.point.column;
        if self.mode.contains(TermMode::INSERT) && spacer.0 + 1 < columns {
            let cursor_line = self.grid.cursor.point.line;
            let row = &mut self.grid[cursor_line][..];
            for col in (spacer.0..columns - 1).rev() {
                row.swap(col + 1, col);
            }
        }
        self.grid
            .cursor
            .template
            .flags
            .insert(Flags::WIDE_CHAR_SPACER);
        self.write_at_cursor(' ');
        self.grid
            .cursor
            .template
            .flags
            .remove(Flags::WIDE_CHAR_SPACER);
        if self.grid.cursor.point.column + 1 < columns {
            self.grid.cursor.point.column += 1;
        } else {
            self.grid.cursor.input_needs_wrap = true;
        }
    }

    /// Turns the two-column cluster at `column` into a one-column one (text
    /// presentation under mode 2027), freeing its spacer.
    fn narrow_cluster(&mut self, line: Line, column: Column) {
        let spacer_column = column + 1;
        if spacer_column.0 >= self.columns()
            || !self.grid[line][spacer_column]
                .flags
                .contains(Flags::WIDE_CHAR_SPACER)
        {
            return;
        }
        self.grid[line][column].flags.remove(Flags::WIDE_CHAR);
        let spacer = &mut self.grid[line][spacer_column];
        spacer.flags.remove(Flags::WIDE_CHAR_SPACER);
        spacer.c = ' ';
        if self.grid.cursor.input_needs_wrap && self.grid.cursor.point.column == spacer_column {
            self.grid.cursor.input_needs_wrap = false;
        } else {
            self.grid.cursor.point.column = spacer_column;
        }
    }

    #[inline]
    fn damage_cursor(&mut self) {
        // The normal cursor coordinates are always in viewport.
        let point = Point::new(
            self.grid.cursor.point.line.0 as usize,
            self.grid.cursor.point.column,
        );
        self.damage.damage_point(point);
    }

    #[inline]
    fn set_keyboard_mode(&mut self, mode: TermMode, apply: KeyboardModesApplyBehavior) {
        let active_mode = self.mode & TermMode::KITTY_KEYBOARD_PROTOCOL;
        self.mode &= !TermMode::KITTY_KEYBOARD_PROTOCOL;
        let new_mode = match apply {
            KeyboardModesApplyBehavior::Replace => mode,
            KeyboardModesApplyBehavior::Union => active_mode.union(mode),
            KeyboardModesApplyBehavior::Difference => active_mode.difference(mode),
        };
        trace!("Setting keyboard mode to {new_mode:?}");
        self.mode |= new_mode;
    }
}

impl<T> Dimensions for Term<T> {
    #[inline]
    fn columns(&self) -> usize {
        self.grid.columns()
    }

    #[inline]
    fn screen_lines(&self) -> usize {
        self.grid.screen_lines()
    }

    #[inline]
    fn total_lines(&self) -> usize {
        self.grid.total_lines()
    }
}

impl<T: EventListener> Handler for Term<T> {
    /// Printable ASCII, written a row segment at a time.
    ///
    /// Equivalent to `input` for each character: every character is one
    /// column wide, so a segment that neither wraps, inserts, nor touches a
    /// wide character or spacer is exactly the per-character writes followed
    /// by the per-character cursor advance. Every other case takes `input`.
    #[inline(never)]
    fn input_ascii(&mut self, text: &str) {
        let bytes = text.as_bytes();
        let mut index = 0;
        while index < bytes.len() {
            let point = self.grid.cursor.point;
            let columns = self.columns();
            if self.grid.cursor.input_needs_wrap || self.mode.contains(TermMode::INSERT) {
                self.input(char::from(bytes[index]));
                index += 1;
                continue;
            }
            let count = (columns - point.column.0).min(bytes.len() - index);
            let charset = self.grid.cursor.charsets[self.active_charset];
            let template = &self.grid.cursor.template;
            let (fg, bg, flags) = (template.fg, template.bg, template.flags);
            let extra = template.extra.clone();
            let text = &bytes[index..index + count];
            // One pass over the cells: overwriting a wide character or spacer
            // also clears its partner cells, so the segment stops there and
            // `input` does that. Occupancy rises over the written cells, as
            // per-cell writes raise it.
            let count = self.grid[point.line].write_from(point.column.0, |cells| {
                let mut written = 0;
                for (cell, &byte) in cells.iter_mut().zip(text) {
                    if cell
                        .flags
                        .intersects(Flags::WIDE_CHAR | Flags::WIDE_CHAR_SPACER)
                    {
                        break;
                    }
                    cell.c = match charset {
                        StandardCharset::Ascii => char::from(byte),
                        charset => charset.map(char::from(byte)),
                    };
                    cell.fg = fg;
                    cell.bg = bg;
                    cell.flags = flags | (cell.flags & Flags::SHELL_MARKS);
                    // Most cells and templates have no extra storage; skip
                    // the reference count when both are empty.
                    if extra.is_some() || cell.extra.is_some() {
                        cell.extra = extra.clone();
                    }
                    written += 1;
                }
                written
            });
            if count == 0 {
                self.input(char::from(bytes[index]));
                index += 1;
                continue;
            }
            let end = point.column + count;
            index += count;
            if end.0 < columns {
                self.grid.cursor.point.column = end;
            } else {
                self.grid.cursor.point.column = Column(columns - 1);
                self.grid.cursor.input_needs_wrap = true;
            }
        }
    }

    /// DIRI PATCH: characters beyond ASCII, none a C1 control.
    ///
    /// Equivalent to `input` for each character. Characters that start a
    /// grapheme cluster of their own and take one or two columns are written
    /// straight into the cursor row, a row segment at a time, while they fit
    /// and land on no wide character or spacer, with no wrap pending and
    /// insert mode off. Anything else (marks, joiners, variation selectors,
    /// emoji modifiers, regional indicators, a wide character at the margin,
    /// a pending wrap) takes `input`, cluster rules included.
    #[inline(never)]
    fn input_text(&mut self, text: &str) {
        let mut index = 0;
        while index < text.len() {
            index = self.input_row_segment(text, index);
        }
    }

    /// A character to be displayed.
    #[inline(never)]
    fn input(&mut self, c: char) {
        // Controls never reach the grid.
        if c < ' ' || ('\u{7F}'..'\u{A0}').contains(&c) {
            return;
        }
        // Number of cells the char will occupy, as Claude Code (Bun's
        // `stringWidth`) measures it; see `grapheme`.
        self.input_measured(c, grapheme::Info::of(c));
    }

    #[inline]
    fn decaln(&mut self) {
        trace!("Decalnning");

        for line in (0..self.screen_lines()).map(Line::from) {
            for column in 0..self.columns() {
                let cell = &mut self.grid[line][Column(column)];
                *cell = Cell::default();
                cell.c = 'E';
            }
        }

        self.mark_fully_damaged();
    }

    #[inline]
    fn goto(&mut self, line: i32, col: usize) {
        let line = Line(line);
        let col = Column(col);

        trace!("Going to: line={line}, col={col}");
        let (y_offset, max_y) = if self.mode.contains(TermMode::ORIGIN) {
            (self.scroll_region.start, self.scroll_region.end - 1)
        } else {
            (Line(0), self.bottommost_line())
        };

        self.damage_cursor();
        self.grid.cursor.point.line = cmp::max(cmp::min(line + y_offset, max_y), Line(0));
        self.grid.cursor.point.column = cmp::min(col, self.last_column());
        self.damage_cursor();
        self.grid.cursor.input_needs_wrap = false;
    }

    #[inline]
    fn goto_line(&mut self, line: i32) {
        trace!("Going to line: {line}");
        self.goto(line, self.grid.cursor.point.column.0)
    }

    #[inline]
    fn goto_col(&mut self, col: usize) {
        trace!("Going to column: {col}");
        self.goto(self.grid.cursor.point.line.0, col)
    }

    #[inline]
    fn insert_blank(&mut self, count: usize) {
        let cursor = &self.grid.cursor;
        let bg = cursor.template.bg;

        // Ensure inserting within terminal bounds
        let count = cmp::min(count, self.columns() - cursor.point.column.0);

        let source = cursor.point.column;
        let destination = cursor.point.column.0 + count;
        let num_cells = self.columns() - destination;

        let line = cursor.point.line;
        self.damage
            .damage_line(line.0 as usize, 0, self.columns() - 1);

        let row = &mut self.grid[line][..];

        for offset in (0..num_cells).rev() {
            row.swap(destination + offset, source.0 + offset);
        }

        // Cells were just moved out toward the end of the line;
        // fill in between source and dest with blanks.
        for cell in &mut row[source.0..destination] {
            *cell = bg.into();
        }
    }

    #[inline]
    fn move_up(&mut self, lines: usize) {
        trace!("Moving up: {lines}");

        let line = self.grid.cursor.point.line - lines;
        let column = self.grid.cursor.point.column;
        self.goto(line.0, column.0)
    }

    #[inline]
    fn move_down(&mut self, lines: usize) {
        trace!("Moving down: {lines}");

        let line = self.grid.cursor.point.line + lines;
        let column = self.grid.cursor.point.column;
        self.goto(line.0, column.0)
    }

    #[inline]
    fn move_forward(&mut self, cols: usize) {
        trace!("Moving forward: {cols}");
        let last_column = cmp::min(self.grid.cursor.point.column + cols, self.last_column());

        let cursor_line = self.grid.cursor.point.line.0 as usize;
        self.damage
            .damage_line(cursor_line, self.grid.cursor.point.column.0, last_column.0);

        self.grid.cursor.point.column = last_column;
        self.grid.cursor.input_needs_wrap = false;
    }

    #[inline]
    fn move_backward(&mut self, cols: usize) {
        trace!("Moving backward: {cols}");
        let column = self.grid.cursor.point.column.saturating_sub(cols);

        let cursor_line = self.grid.cursor.point.line.0 as usize;
        self.damage
            .damage_line(cursor_line, column, self.grid.cursor.point.column.0);

        self.grid.cursor.point.column = Column(column);
        self.grid.cursor.input_needs_wrap = false;
    }

    #[inline]
    fn identify_terminal(&mut self, intermediate: Option<char>) {
        match intermediate {
            None => {
                trace!("Reporting primary device attributes");
                let text = String::from("\x1b[?6c");
                self.event_proxy.send_event(Event::PtyWrite(text));
            }
            Some('>') => {
                trace!("Reporting secondary device attributes");
                let version = version_number(env!("CARGO_PKG_VERSION"));
                let text = format!("\x1b[>0;{version};1c");
                self.event_proxy.send_event(Event::PtyWrite(text));
            }
            _ => debug!("Unsupported device attributes intermediate"),
        }
    }

    #[inline]
    fn report_keyboard_mode(&mut self) {
        if !self.config.kitty_keyboard {
            return;
        }

        trace!("Reporting active keyboard mode");
        let current_mode = KeyboardModes::from(self.mode).bits();
        let text = format!("\x1b[?{current_mode}u");
        self.event_proxy.send_event(Event::PtyWrite(text));
    }

    #[inline]
    fn push_keyboard_mode(&mut self, mode: KeyboardModes) {
        if !self.config.kitty_keyboard {
            return;
        }

        trace!("Pushing `{mode:?}` keyboard mode into the stack");

        if self.keyboard_mode_stack.len() >= KEYBOARD_MODE_STACK_MAX_DEPTH {
            let removed = self.keyboard_mode_stack.remove(0);
            trace!(
                "Removing '{removed:?}' from bottom of keyboard mode stack that exceeds its \
                 maximum depth"
            );
        }

        self.keyboard_mode_stack.push(mode);
        self.set_keyboard_mode(mode.into(), KeyboardModesApplyBehavior::Replace);
    }

    #[inline]
    fn pop_keyboard_modes(&mut self, to_pop: u16) {
        if !self.config.kitty_keyboard {
            return;
        }

        trace!("Attempting to pop {to_pop} keyboard modes from the stack");
        let new_len = self
            .keyboard_mode_stack
            .len()
            .saturating_sub(to_pop as usize);
        self.keyboard_mode_stack.truncate(new_len);

        // Reload active mode.
        let mode = self
            .keyboard_mode_stack
            .last()
            .copied()
            .unwrap_or(KeyboardModes::NO_MODE);
        self.set_keyboard_mode(mode.into(), KeyboardModesApplyBehavior::Replace);
    }

    #[inline]
    fn set_keyboard_mode(&mut self, mode: KeyboardModes, apply: KeyboardModesApplyBehavior) {
        if !self.config.kitty_keyboard {
            return;
        }

        self.set_keyboard_mode(mode.into(), apply);
        // CSI = changes the current stack entry. Keep the saved screen state
        // consistent with the mode used to encode input and answer CSI ? u.
        let current = KeyboardModes::from(self.mode);
        if let Some(top) = self.keyboard_mode_stack.last_mut() {
            *top = current;
        } else {
            self.keyboard_mode_stack.push(current);
        }
    }

    #[inline]
    fn device_status(&mut self, arg: usize) {
        trace!("Reporting device status: {arg}");
        match arg {
            5 => {
                let text = String::from("\x1b[0n");
                self.event_proxy.send_event(Event::PtyWrite(text));
            }
            6 => {
                let pos = self.grid.cursor.point;
                let text = format!("\x1b[{};{}R", pos.line + 1, pos.column + 1);
                self.event_proxy.send_event(Event::PtyWrite(text));
            }
            _ => debug!("unknown device status query: {arg}"),
        };
    }

    #[inline]
    fn move_down_and_cr(&mut self, lines: usize) {
        trace!("Moving down and cr: {lines}");

        let line = self.grid.cursor.point.line + lines;
        self.goto(line.0, 0)
    }

    #[inline]
    fn move_up_and_cr(&mut self, lines: usize) {
        trace!("Moving up and cr: {lines}");

        let line = self.grid.cursor.point.line - lines;
        self.goto(line.0, 0)
    }

    /// Insert tab at cursor position.
    #[inline]
    fn put_tab(&mut self, mut count: u16) {
        // A tab after the last column is the same as a linebreak.
        if self.grid.cursor.input_needs_wrap {
            self.wrapline();
            return;
        }

        while self.grid.cursor.point.column < self.columns() && count != 0 {
            count -= 1;

            let c = self.grid.cursor.charsets[self.active_charset].map('\t');
            let cell = self.grid.cursor_cell();
            if cell.c == ' ' {
                cell.c = c;
            }

            loop {
                if (self.grid.cursor.point.column + 1) == self.columns() {
                    break;
                }

                self.grid.cursor.point.column += 1;

                if self.tabs[self.grid.cursor.point.column] {
                    break;
                }
            }
        }
    }

    /// Backspace.
    #[inline]
    fn backspace(&mut self) {
        trace!("Backspace");

        if self.grid.cursor.point.column > Column(0) {
            let line = self.grid.cursor.point.line.0 as usize;
            let column = self.grid.cursor.point.column.0;
            self.grid.cursor.point.column -= 1;
            self.grid.cursor.input_needs_wrap = false;
            self.damage.damage_line(line, column - 1, column);
        }
    }

    /// Carriage return.
    #[inline]
    fn carriage_return(&mut self) {
        trace!("Carriage return");
        let new_col = 0;
        let line = self.grid.cursor.point.line.0 as usize;
        self.damage
            .damage_line(line, new_col, self.grid.cursor.point.column.0);
        self.grid.cursor.point.column = Column(new_col);
        self.grid.cursor.input_needs_wrap = false;
    }

    /// Linefeed.
    #[inline]
    fn linefeed(&mut self) {
        trace!("Linefeed");
        let next = self.grid.cursor.point.line + 1;
        if next == self.scroll_region.end {
            self.scroll_up(1);
        } else if next < self.screen_lines() {
            self.damage_cursor();
            self.grid.cursor.point.line += 1;
            self.damage_cursor();
        }
    }

    /// Set current position as a tabstop.
    #[inline]
    fn bell(&mut self) {
        trace!("Bell");
        self.event_proxy.send_event(Event::Bell);
    }

    #[inline]
    fn substitute(&mut self) {
        trace!("[unimplemented] Substitute");
    }

    /// Run LF/NL.
    ///
    /// LF/NL mode has some interesting history. According to ECMA-48 4th
    /// edition, in LINE FEED mode,
    ///
    /// > The execution of the formatter functions LINE FEED (LF), FORM FEED
    /// > (FF), LINE TABULATION (VT) cause only movement of the active position in
    /// > the direction of the line progression.
    ///
    /// In NEW LINE mode,
    ///
    /// > The execution of the formatter functions LINE FEED (LF), FORM FEED
    /// > (FF), LINE TABULATION (VT) cause movement to the line home position on
    /// > the following line, the following form, etc. In the case of LF this is
    /// > referred to as the New Line (NL) option.
    ///
    /// Additionally, ECMA-48 4th edition says that this option is deprecated.
    /// ECMA-48 5th edition only mentions this option (without explanation)
    /// saying that it's been removed.
    ///
    /// As an emulator, we need to support it since applications may still rely
    /// on it.
    #[inline]
    fn newline(&mut self) {
        self.linefeed();

        if self.mode.contains(TermMode::LINE_FEED_NEW_LINE) {
            self.carriage_return();
        }
    }

    #[inline]
    fn set_horizontal_tabstop(&mut self) {
        trace!("Setting horizontal tabstop");
        self.tabs[self.grid.cursor.point.column] = true;
    }

    #[inline]
    fn scroll_up(&mut self, lines: usize) {
        let origin = self.scroll_region.start;
        self.scroll_up_relative(origin, lines);
    }

    #[inline]
    fn scroll_down(&mut self, lines: usize) {
        let origin = self.scroll_region.start;
        self.scroll_down_relative(origin, lines);
    }

    #[inline]
    fn insert_blank_lines(&mut self, lines: usize) {
        trace!("Inserting blank {lines} lines");

        let origin = self.grid.cursor.point.line;
        if self.scroll_region.contains(&origin) {
            self.scroll_down_relative(origin, lines);
        }
    }

    #[inline]
    fn delete_lines(&mut self, lines: usize) {
        let origin = self.grid.cursor.point.line;
        let lines = cmp::min(self.screen_lines() - origin.0 as usize, lines);

        trace!("Deleting {lines} lines");

        if lines > 0 && self.scroll_region.contains(&origin) {
            self.scroll_up_relative(origin, lines);
        }
    }

    #[inline]
    fn erase_chars(&mut self, count: usize) {
        let cursor = &self.grid.cursor;

        trace!(
            "Erasing chars: count={}, col={}",
            count, cursor.point.column
        );

        let start = cursor.point.column;
        let end = cmp::min(start + count, Column(self.columns()));

        // Cleared cells have current background color set.
        let bg = self.grid.cursor.template.bg;
        let line = cursor.point.line;
        self.damage.damage_line(line.0 as usize, start.0, end.0);
        let row = &mut self.grid[line];
        for cell in &mut row[start..end] {
            *cell = bg.into();
        }
    }

    #[inline]
    fn delete_chars(&mut self, count: usize) {
        let columns = self.columns();
        let cursor = &self.grid.cursor;
        let bg = cursor.template.bg;

        // Ensure deleting within terminal bounds.
        let count = cmp::min(count, columns);

        let start = cursor.point.column.0;
        let end = cmp::min(start + count, columns - 1);
        let num_cells = columns - end;

        let line = cursor.point.line;
        self.damage
            .damage_line(line.0 as usize, 0, self.columns() - 1);
        let row = &mut self.grid[line][..];

        for offset in 0..num_cells {
            row.swap(start + offset, end + offset);
        }

        // Clear last `count` cells in the row. If deleting 1 char, need to delete
        // 1 cell.
        let end = columns - count;
        for cell in &mut row[end..] {
            *cell = bg.into();
        }
    }

    #[inline]
    fn move_backward_tabs(&mut self, count: u16) {
        trace!("Moving backward {count} tabs");

        let old_col = self.grid.cursor.point.column.0;
        for _ in 0..count {
            let mut col = self.grid.cursor.point.column;

            if col == 0 {
                break;
            }

            for i in (0..(col.0)).rev() {
                if self.tabs[index::Column(i)] {
                    col = index::Column(i);
                    break;
                }
            }
            self.grid.cursor.point.column = col;
        }

        let line = self.grid.cursor.point.line.0 as usize;
        self.damage
            .damage_line(line, self.grid.cursor.point.column.0, old_col);
    }

    #[inline]
    fn move_forward_tabs(&mut self, count: u16) {
        trace!("Moving forward {count} tabs");

        let num_cols = self.columns();
        let old_col = self.grid.cursor.point.column.0;
        for _ in 0..count {
            let mut col = self.grid.cursor.point.column;

            if col == num_cols - 1 {
                break;
            }

            for i in col.0 + 1..num_cols {
                col = index::Column(i);
                if self.tabs[col] {
                    break;
                }
            }

            self.grid.cursor.point.column = col;
        }

        let line = self.grid.cursor.point.line.0 as usize;
        self.damage
            .damage_line(line, old_col, self.grid.cursor.point.column.0);
    }

    #[inline]
    fn save_cursor_position(&mut self) {
        trace!("Saving cursor position");

        self.grid.saved_cursor = self.grid.cursor.clone();
    }

    #[inline]
    fn restore_cursor_position(&mut self) {
        trace!("Restoring cursor position");

        self.damage_cursor();
        self.grid.cursor = self.grid.saved_cursor.clone();
        self.damage_cursor();
    }

    #[inline]
    fn clear_line(&mut self, mode: ansi::LineClearMode) {
        trace!("Clearing line: {mode:?}");

        let cursor = &self.grid.cursor;
        let bg = cursor.template.bg;
        let point = cursor.point;

        // DIRI PATCH: like xterm, erasing in line cancels a pending wrap, so
        // the next character overwrites the last column instead of wrapping.
        let (left, right) = match mode {
            ansi::LineClearMode::Right => (point.column, Column(self.columns())),
            ansi::LineClearMode::Left => (Column(0), point.column + 1),
            ansi::LineClearMode::All => (Column(0), Column(self.columns())),
        };

        self.damage
            .damage_line(point.line.0 as usize, left.0, right.0 - 1);

        let row = &mut self.grid[point.line];
        for cell in &mut row[left..right] {
            *cell = bg.into();
        }

        self.grid.cursor.input_needs_wrap = false;
        let range = self.grid.cursor.point.line..=self.grid.cursor.point.line;
        self.selection = self.selection.take().filter(|s| !s.intersects_range(range));
    }

    /// Set the indexed color value.
    #[inline]
    fn set_color(&mut self, index: usize, color: Rgb) {
        trace!("Setting color[{index}] = {color:?}");

        // Damage terminal if the color changed and it's not the cursor.
        if index != NamedColor::Cursor as usize && self.colors[index] != Some(color) {
            self.mark_fully_damaged();
            self.colors_generation += 1;
        }

        self.colors[index] = Some(color);
    }

    /// Respond to a color query escape sequence.
    #[inline]
    fn dynamic_color_sequence(&mut self, prefix: String, index: usize, terminator: &str) {
        trace!("Requested write of escape sequence for color code {prefix}: color[{index}]");

        let terminator = terminator.to_owned();
        self.event_proxy.send_event(Event::ColorRequest(
            index,
            Arc::new(move |color| {
                format!(
                    "\x1b]{};rgb:{1:02x}{1:02x}/{2:02x}{2:02x}/{3:02x}{3:02x}{4}",
                    prefix, color.r, color.g, color.b, terminator
                )
            }),
        ));
    }

    /// Reset the indexed color to original value.
    #[inline]
    fn reset_color(&mut self, index: usize) {
        trace!("Resetting color[{index}]");

        // Damage terminal if the color changed and it's not the cursor.
        if index != NamedColor::Cursor as usize && self.colors[index].is_some() {
            self.mark_fully_damaged();
            self.colors_generation += 1;
        }

        self.colors[index] = None;
    }

    /// Store data into clipboard.
    #[inline]
    fn clipboard_store(&mut self, clipboard: u8, base64: &[u8]) {
        if !matches!(self.config.osc52, Osc52::OnlyCopy | Osc52::CopyPaste) {
            debug!("Denied osc52 store");
            return;
        }

        let clipboard_type = match clipboard {
            b'c' => ClipboardType::Clipboard,
            b'p' | b's' => ClipboardType::Selection,
            _ => return,
        };

        if let Ok(bytes) = Base64.decode(base64) {
            if let Ok(text) = String::from_utf8(bytes) {
                self.event_proxy
                    .send_event(Event::ClipboardStore(clipboard_type, text));
            }
        }
    }

    /// Load data from clipboard.
    #[inline]
    fn clipboard_load(&mut self, clipboard: u8, terminator: &str) {
        if !matches!(self.config.osc52, Osc52::OnlyPaste | Osc52::CopyPaste) {
            debug!("Denied osc52 load");
            return;
        }

        let clipboard_type = match clipboard {
            b'c' => ClipboardType::Clipboard,
            b'p' | b's' => ClipboardType::Selection,
            _ => return,
        };

        let terminator = terminator.to_owned();

        self.event_proxy.send_event(Event::ClipboardLoad(
            clipboard_type,
            Arc::new(move |text| {
                let base64 = Base64.encode(text);
                format!("\x1b]52;{};{}{}", clipboard as char, base64, terminator)
            }),
        ));
    }

    #[inline]
    fn clear_screen(&mut self, mode: ansi::ClearMode) {
        trace!("Clearing screen: {mode:?}");
        let bg = self.grid.cursor.template.bg;

        let screen_lines = self.screen_lines();

        match mode {
            ansi::ClearMode::Above => {
                let cursor = self.grid.cursor.point;

                // If clearing more than one line.
                if cursor.line > 1 {
                    // Fully clear all lines before the current line.
                    self.grid.reset_region(..cursor.line);
                }

                // Clear up to the current column in the current line.
                let end = cmp::min(cursor.column + 1, Column(self.columns()));
                for cell in &mut self.grid[cursor.line][..end] {
                    *cell = bg.into();
                }

                let range = Line(0)..=cursor.line;
                self.selection = self.selection.take().filter(|s| !s.intersects_range(range));
            }
            ansi::ClearMode::Below => {
                let cursor = self.grid.cursor.point;
                for cell in &mut self.grid[cursor.line][cursor.column..] {
                    *cell = bg.into();
                }

                if (cursor.line.0 as usize) < screen_lines - 1 {
                    self.grid.reset_region((cursor.line + 1)..);
                }

                let range = cursor.line..Line(screen_lines as i32);
                self.selection = self.selection.take().filter(|s| !s.intersects_range(range));
            }
            ansi::ClearMode::All => {
                if self.mode.contains(TermMode::ALT_SCREEN) {
                    self.grid.reset_region(..);
                } else {
                    let old_offset = self.grid.display_offset();

                    self.grid.clear_viewport();

                    // Compute number of lines scrolled by clearing the viewport.
                    let lines = self.grid.display_offset().saturating_sub(old_offset);

                    self.vi_mode_cursor.point.line =
                        (self.vi_mode_cursor.point.line - lines).grid_clamp(self, Boundary::Grid);
                }

                self.selection = None;
            }
            ansi::ClearMode::Saved if self.history_size() > 0 => {
                self.grid.clear_history();

                self.vi_mode_cursor.point.line = self
                    .vi_mode_cursor
                    .point
                    .line
                    .grid_clamp(self, Boundary::Cursor);

                self.selection = self
                    .selection
                    .take()
                    .filter(|s| !s.intersects_range(..Line(0)));
            }
            // We have no history to clear.
            ansi::ClearMode::Saved => (),
        }

        self.mark_fully_damaged();
    }

    #[inline]
    fn clear_tabs(&mut self, mode: ansi::TabulationClearMode) {
        trace!("Clearing tabs: {mode:?}");
        match mode {
            ansi::TabulationClearMode::Current => {
                self.tabs[self.grid.cursor.point.column] = false;
            }
            ansi::TabulationClearMode::All => {
                self.tabs.clear_all();
            }
        }
    }

    /// Reset all important fields in the term struct.
    #[inline]
    fn reset_state(&mut self) {
        if self.mode.contains(TermMode::ALT_SCREEN) {
            mem::swap(
                &mut self.grid,
                self.inactive_grid
                    .as_mut()
                    .expect("alternate screen initialized before swap"),
            );
        }
        self.active_charset = Default::default();
        self.cursor_style = None;
        self.grid.reset();
        if let Some(inactive_grid) = self.inactive_grid.as_mut() {
            inactive_grid.reset();
        }
        self.scroll_region = Line(0)..Line(self.screen_lines() as i32);
        self.tabs = TabStops::new(self.columns());
        self.title_stack = Vec::new();
        // DIRI PATCH: RIS resets the title too; listeners track it by event.
        if self.title.take().is_some() {
            self.event_proxy.send_event(Event::ResetTitle);
        }
        // DIRI PATCH: and the colors a program set with OSC 4/10/11/12, as
        // xterm and libghostty do: `reset` restores the configured palette.
        if (0..color::COUNT).any(|index| self.colors[index].is_some()) {
            self.colors = color::Colors::default();
            self.colors_generation += 1;
        }
        self.selection = None;
        self.vi_mode_cursor = Default::default();
        self.keyboard_mode_stack = Default::default();
        self.inactive_keyboard_mode_stack = Default::default();

        // Preserve vi mode across resets.
        self.mode &= TermMode::VI;
        self.mode.insert(TermMode::default());

        self.event_proxy.send_event(Event::CursorBlinkingChange);
        self.mark_fully_damaged();
    }

    #[inline]
    fn reverse_index(&mut self) {
        trace!("Reversing index");
        // If cursor is at the top.
        if self.grid.cursor.point.line == self.scroll_region.start {
            self.scroll_down(1);
        } else {
            self.damage_cursor();
            self.grid.cursor.point.line = cmp::max(self.grid.cursor.point.line - 1, Line(0));
            self.damage_cursor();
        }
    }

    #[inline]
    fn mark_prompt(&mut self) {
        // Store on the grid so erase, scrolling and reflow own its lifetime.
        // Alternate-screen TUIs do not define shell navigation boundaries.
        if !self.mode.contains(TermMode::ALT_SCREEN) {
            let point = self.grid.cursor.point;
            self.grid[point].flags.insert(Flags::PROMPT_START);
            self.damage_cursor();
        }
    }

    #[inline]
    fn mark_shell_command(&mut self, mark: ShellCommandMark) {
        // Same lifetime and alternate-screen rule as `mark_prompt`. A command
        // end replaces any earlier end on the same cell.
        if !self.mode.contains(TermMode::ALT_SCREEN) {
            let point = self.grid.cursor.point;
            let flags = &mut self.grid[point].flags;
            match mark {
                ShellCommandMark::InputStart => flags.insert(Flags::INPUT_START),
                ShellCommandMark::OutputStart => flags.insert(Flags::OUTPUT_START),
                ShellCommandMark::End(status) => {
                    flags.remove(Flags::COMMAND_END);
                    flags.insert(match status {
                        Some(0) => Flags::COMMAND_SUCCEEDED,
                        Some(_) => Flags::COMMAND_FAILED,
                        None => Flags::COMMAND_END,
                    });
                }
            }
            self.damage_cursor();
        }
    }

    fn set_hyperlink(&mut self, hyperlink: Option<Hyperlink>) {
        trace!("Setting hyperlink: {hyperlink:?}");
        self.grid
            .cursor
            .template
            .set_hyperlink(hyperlink.map(|e| e.into()));
    }

    /// Set a terminal attribute.
    #[inline]
    fn terminal_attribute(&mut self, attr: Attr) {
        trace!("Setting attribute: {attr:?}");
        let cursor = &mut self.grid.cursor;
        match attr {
            Attr::Foreground(color) => cursor.template.fg = color,
            Attr::Background(color) => cursor.template.bg = color,
            Attr::UnderlineColor(color) => cursor.template.set_underline_color(color),
            Attr::Reset => {
                cursor.template.fg = Color::Named(NamedColor::Foreground);
                cursor.template.bg = Color::Named(NamedColor::Background);
                cursor.template.flags = Flags::empty();
                cursor.template.set_underline_color(None);
            }
            Attr::Reverse => cursor.template.flags.insert(Flags::INVERSE),
            Attr::CancelReverse => cursor.template.flags.remove(Flags::INVERSE),
            Attr::Bold => cursor.template.flags.insert(Flags::BOLD),
            Attr::CancelBold => cursor.template.flags.remove(Flags::BOLD),
            Attr::Dim => cursor.template.flags.insert(Flags::DIM),
            Attr::CancelBoldDim => cursor.template.flags.remove(Flags::BOLD | Flags::DIM),
            Attr::Italic => cursor.template.flags.insert(Flags::ITALIC),
            Attr::CancelItalic => cursor.template.flags.remove(Flags::ITALIC),
            Attr::Underline => {
                cursor.template.flags.remove(Flags::ALL_UNDERLINES);
                cursor.template.flags.insert(Flags::UNDERLINE);
            }
            Attr::DoubleUnderline => {
                cursor.template.flags.remove(Flags::ALL_UNDERLINES);
                cursor.template.flags.insert(Flags::DOUBLE_UNDERLINE);
            }
            Attr::Undercurl => {
                cursor.template.flags.remove(Flags::ALL_UNDERLINES);
                cursor.template.flags.insert(Flags::UNDERCURL);
            }
            Attr::DottedUnderline => {
                cursor.template.flags.remove(Flags::ALL_UNDERLINES);
                cursor.template.flags.insert(Flags::DOTTED_UNDERLINE);
            }
            Attr::DashedUnderline => {
                cursor.template.flags.remove(Flags::ALL_UNDERLINES);
                cursor.template.flags.insert(Flags::DASHED_UNDERLINE);
            }
            Attr::CancelUnderline => cursor.template.flags.remove(Flags::ALL_UNDERLINES),
            Attr::Hidden => cursor.template.flags.insert(Flags::HIDDEN),
            Attr::CancelHidden => cursor.template.flags.remove(Flags::HIDDEN),
            Attr::Strike => cursor.template.flags.insert(Flags::STRIKEOUT),
            Attr::CancelStrike => cursor.template.flags.remove(Flags::STRIKEOUT),
            Attr::BlinkSlow | Attr::BlinkFast => cursor.template.flags.insert(Flags::BLINK),
            Attr::CancelBlink => cursor.template.flags.remove(Flags::BLINK),
            Attr::Overline => cursor.template.flags.insert(Flags::OVERLINE),
            Attr::CancelOverline => cursor.template.flags.remove(Flags::OVERLINE),
        }
    }

    #[inline]
    fn set_private_mode(&mut self, mode: PrivateMode) {
        let mode = match mode {
            PrivateMode::Named(mode) => mode,
            PrivateMode::Unknown(mode) => {
                debug!("Ignoring unknown mode {mode} in set_private_mode");
                return;
            }
        };

        trace!("Setting private mode: {mode:?}");
        match mode {
            NamedPrivateMode::UrgencyHints => self.mode.insert(TermMode::URGENCY_HINTS),
            NamedPrivateMode::SwapScreenAndSetRestoreCursor => {
                if !self.mode.contains(TermMode::ALT_SCREEN) {
                    self.swap_alt();
                }
            }
            NamedPrivateMode::ShowCursor => self.mode.insert(TermMode::SHOW_CURSOR),
            NamedPrivateMode::CursorKeys => self.mode.insert(TermMode::APP_CURSOR),
            // Mouse protocols are mutually exclusive.
            NamedPrivateMode::ReportMouseClicks => {
                self.mode.remove(TermMode::MOUSE_MODE);
                self.mode.insert(TermMode::MOUSE_REPORT_CLICK);
                self.event_proxy.send_event(Event::MouseCursorDirty);
            }
            NamedPrivateMode::ReportCellMouseMotion => {
                self.mode.remove(TermMode::MOUSE_MODE);
                self.mode.insert(TermMode::MOUSE_DRAG);
                self.event_proxy.send_event(Event::MouseCursorDirty);
            }
            NamedPrivateMode::ReportAllMouseMotion => {
                self.mode.remove(TermMode::MOUSE_MODE);
                self.mode.insert(TermMode::MOUSE_MOTION);
                self.event_proxy.send_event(Event::MouseCursorDirty);
            }
            NamedPrivateMode::ReportFocusInOut => self.mode.insert(TermMode::FOCUS_IN_OUT),
            NamedPrivateMode::BracketedPaste => self.mode.insert(TermMode::BRACKETED_PASTE),
            // Mouse encodings are mutually exclusive.
            NamedPrivateMode::SgrMouse => {
                self.mode.remove(TermMode::UTF8_MOUSE);
                self.mode.insert(TermMode::SGR_MOUSE);
            }
            NamedPrivateMode::Utf8Mouse => {
                self.mode.remove(TermMode::SGR_MOUSE);
                self.mode.insert(TermMode::UTF8_MOUSE);
            }
            NamedPrivateMode::AlternateScroll => self.mode.insert(TermMode::ALTERNATE_SCROLL),
            NamedPrivateMode::LineWrap => self.mode.insert(TermMode::LINE_WRAP),
            NamedPrivateMode::Origin => {
                self.mode.insert(TermMode::ORIGIN);
                self.goto(0, 0);
            }
            NamedPrivateMode::ColumnMode => self.deccolm(),
            NamedPrivateMode::BlinkingCursor => {
                let style = self
                    .cursor_style
                    .get_or_insert(self.config.default_cursor_style);
                style.blinking = true;
                self.event_proxy.send_event(Event::CursorBlinkingChange);
            }
            NamedPrivateMode::SyncUpdate => (),
            NamedPrivateMode::GraphemeClustering => self.mode.insert(TermMode::GRAPHEME_CLUSTERING),
            NamedPrivateMode::LegacyAltScreen => {
                if !self.mode.contains(TermMode::ALT_SCREEN) {
                    self.swap_alt_screen(AltScreen::Shared);
                }
            }
            NamedPrivateMode::ClearingAltScreen => {
                if !self.mode.contains(TermMode::ALT_SCREEN) {
                    self.swap_alt_screen(AltScreen::ClearOnExit);
                }
            }
            NamedPrivateMode::LeftRightMargins | NamedPrivateMode::SgrPixelMouse => (),
            NamedPrivateMode::ColorSchemeUpdates => {
                self.mode.insert(TermMode::COLOR_SCHEME_UPDATES)
            }
            NamedPrivateMode::InBandResize => {
                self.mode.insert(TermMode::IN_BAND_RESIZE);
                // The program learns the current size as soon as it asks.
                self.event_proxy.send_event(Event::InBandResize);
            }
        }
    }

    #[inline]
    fn unset_private_mode(&mut self, mode: PrivateMode) {
        let mode = match mode {
            PrivateMode::Named(mode) => mode,
            PrivateMode::Unknown(mode) => {
                debug!("Ignoring unknown mode {mode} in unset_private_mode");
                return;
            }
        };

        trace!("Unsetting private mode: {mode:?}");
        match mode {
            NamedPrivateMode::UrgencyHints => self.mode.remove(TermMode::URGENCY_HINTS),
            NamedPrivateMode::SwapScreenAndSetRestoreCursor => {
                if self.mode.contains(TermMode::ALT_SCREEN) {
                    self.swap_alt();
                }
            }
            NamedPrivateMode::ShowCursor => self.mode.remove(TermMode::SHOW_CURSOR),
            NamedPrivateMode::CursorKeys => self.mode.remove(TermMode::APP_CURSOR),
            NamedPrivateMode::ReportMouseClicks => {
                self.mode.remove(TermMode::MOUSE_REPORT_CLICK);
                self.event_proxy.send_event(Event::MouseCursorDirty);
            }
            NamedPrivateMode::ReportCellMouseMotion => {
                self.mode.remove(TermMode::MOUSE_DRAG);
                self.event_proxy.send_event(Event::MouseCursorDirty);
            }
            NamedPrivateMode::ReportAllMouseMotion => {
                self.mode.remove(TermMode::MOUSE_MOTION);
                self.event_proxy.send_event(Event::MouseCursorDirty);
            }
            NamedPrivateMode::ReportFocusInOut => self.mode.remove(TermMode::FOCUS_IN_OUT),
            NamedPrivateMode::BracketedPaste => self.mode.remove(TermMode::BRACKETED_PASTE),
            NamedPrivateMode::SgrMouse => self.mode.remove(TermMode::SGR_MOUSE),
            NamedPrivateMode::Utf8Mouse => self.mode.remove(TermMode::UTF8_MOUSE),
            NamedPrivateMode::AlternateScroll => self.mode.remove(TermMode::ALTERNATE_SCROLL),
            NamedPrivateMode::LineWrap => self.mode.remove(TermMode::LINE_WRAP),
            NamedPrivateMode::Origin => self.mode.remove(TermMode::ORIGIN),
            NamedPrivateMode::ColumnMode => self.deccolm(),
            NamedPrivateMode::BlinkingCursor => {
                let style = self
                    .cursor_style
                    .get_or_insert(self.config.default_cursor_style);
                style.blinking = false;
                self.event_proxy.send_event(Event::CursorBlinkingChange);
            }
            NamedPrivateMode::SyncUpdate => (),
            NamedPrivateMode::GraphemeClustering => self.mode.remove(TermMode::GRAPHEME_CLUSTERING),
            NamedPrivateMode::LegacyAltScreen => {
                if self.mode.contains(TermMode::ALT_SCREEN) {
                    self.swap_alt_screen(AltScreen::Shared);
                }
            }
            NamedPrivateMode::ClearingAltScreen => {
                if self.mode.contains(TermMode::ALT_SCREEN) {
                    self.swap_alt_screen(AltScreen::ClearOnExit);
                }
            }
            NamedPrivateMode::LeftRightMargins | NamedPrivateMode::SgrPixelMouse => (),
            NamedPrivateMode::ColorSchemeUpdates => {
                self.mode.remove(TermMode::COLOR_SCHEME_UPDATES)
            }
            NamedPrivateMode::InBandResize => self.mode.remove(TermMode::IN_BAND_RESIZE),
        }
    }

    #[inline]
    fn report_private_mode(&mut self, mode: PrivateMode) {
        trace!("Reporting private mode {mode:?}");
        let state = match mode {
            PrivateMode::Named(mode) => match mode {
                NamedPrivateMode::CursorKeys => self.mode.contains(TermMode::APP_CURSOR).into(),
                NamedPrivateMode::Origin => self.mode.contains(TermMode::ORIGIN).into(),
                NamedPrivateMode::LineWrap => self.mode.contains(TermMode::LINE_WRAP).into(),
                NamedPrivateMode::BlinkingCursor => {
                    let style = self
                        .cursor_style
                        .get_or_insert(self.config.default_cursor_style);
                    style.blinking.into()
                }
                NamedPrivateMode::ShowCursor => self.mode.contains(TermMode::SHOW_CURSOR).into(),
                NamedPrivateMode::ReportMouseClicks => {
                    self.mode.contains(TermMode::MOUSE_REPORT_CLICK).into()
                }
                NamedPrivateMode::ReportCellMouseMotion => {
                    self.mode.contains(TermMode::MOUSE_DRAG).into()
                }
                NamedPrivateMode::ReportAllMouseMotion => {
                    self.mode.contains(TermMode::MOUSE_MOTION).into()
                }
                NamedPrivateMode::ReportFocusInOut => {
                    self.mode.contains(TermMode::FOCUS_IN_OUT).into()
                }
                NamedPrivateMode::Utf8Mouse => self.mode.contains(TermMode::UTF8_MOUSE).into(),
                NamedPrivateMode::SgrMouse => self.mode.contains(TermMode::SGR_MOUSE).into(),
                NamedPrivateMode::AlternateScroll => {
                    self.mode.contains(TermMode::ALTERNATE_SCROLL).into()
                }
                NamedPrivateMode::UrgencyHints => {
                    self.mode.contains(TermMode::URGENCY_HINTS).into()
                }
                NamedPrivateMode::SwapScreenAndSetRestoreCursor => {
                    self.mode.contains(TermMode::ALT_SCREEN).into()
                }
                NamedPrivateMode::BracketedPaste => {
                    self.mode.contains(TermMode::BRACKETED_PASTE).into()
                }
                NamedPrivateMode::SyncUpdate => ModeState::Reset,
                NamedPrivateMode::GraphemeClustering => {
                    self.mode.contains(TermMode::GRAPHEME_CLUSTERING).into()
                }
                NamedPrivateMode::ColumnMode => ModeState::NotSupported,
                NamedPrivateMode::LegacyAltScreen | NamedPrivateMode::ClearingAltScreen => {
                    self.mode.contains(TermMode::ALT_SCREEN).into()
                }
                NamedPrivateMode::LeftRightMargins | NamedPrivateMode::SgrPixelMouse => {
                    ModeState::PermanentlyReset
                }
                NamedPrivateMode::ColorSchemeUpdates => {
                    self.mode.contains(TermMode::COLOR_SCHEME_UPDATES).into()
                }
                NamedPrivateMode::InBandResize => {
                    self.mode.contains(TermMode::IN_BAND_RESIZE).into()
                }
            },
            PrivateMode::Unknown(_) => ModeState::NotSupported,
        };

        self.event_proxy.send_event(Event::PtyWrite(format!(
            "\x1b[?{};{}$y",
            mode.raw(),
            state as u8,
        )));
    }

    #[inline]
    fn set_mode(&mut self, mode: ansi::Mode) {
        let mode = match mode {
            ansi::Mode::Named(mode) => mode,
            ansi::Mode::Unknown(mode) => {
                debug!("Ignoring unknown mode {mode} in set_mode");
                return;
            }
        };

        trace!("Setting public mode: {mode:?}");
        match mode {
            NamedMode::Insert => self.mode.insert(TermMode::INSERT),
            NamedMode::LineFeedNewLine => self.mode.insert(TermMode::LINE_FEED_NEW_LINE),
        }
    }

    #[inline]
    fn unset_mode(&mut self, mode: ansi::Mode) {
        let mode = match mode {
            ansi::Mode::Named(mode) => mode,
            ansi::Mode::Unknown(mode) => {
                debug!("Ignoring unknown mode {mode} in unset_mode");
                return;
            }
        };

        trace!("Setting public mode: {mode:?}");
        match mode {
            NamedMode::Insert => {
                self.mode.remove(TermMode::INSERT);
                self.mark_fully_damaged();
            }
            NamedMode::LineFeedNewLine => self.mode.remove(TermMode::LINE_FEED_NEW_LINE),
        }
    }

    #[inline]
    fn report_mode(&mut self, mode: ansi::Mode) {
        trace!("Reporting mode {mode:?}");
        let state = match mode {
            ansi::Mode::Named(mode) => match mode {
                NamedMode::Insert => self.mode.contains(TermMode::INSERT).into(),
                NamedMode::LineFeedNewLine => {
                    self.mode.contains(TermMode::LINE_FEED_NEW_LINE).into()
                }
            },
            ansi::Mode::Unknown(_) => ModeState::NotSupported,
        };

        self.event_proxy.send_event(Event::PtyWrite(format!(
            "\x1b[{};{}$y",
            mode.raw(),
            state as u8,
        )));
    }

    #[inline]
    fn set_scrolling_region(&mut self, top: usize, bottom: Option<usize>) {
        // Fallback to the last line as default.
        let bottom = bottom.unwrap_or_else(|| self.screen_lines());

        if top >= bottom {
            debug!("Invalid scrolling region: ({top};{bottom})");
            return;
        }

        // Bottom should be included in the range, but range end is not
        // usually included. One option would be to use an inclusive
        // range, but instead we just let the open range end be 1
        // higher.
        let start = Line(top as i32 - 1);
        let end = Line(bottom as i32);

        trace!("Setting scrolling region: ({start};{end})");

        let screen_lines = Line(self.screen_lines() as i32);
        self.scroll_region.start = cmp::min(start, screen_lines);
        self.scroll_region.end = cmp::min(end, screen_lines);
        self.goto(0, 0);
    }

    #[inline]
    fn set_keypad_application_mode(&mut self) {
        trace!("Setting keypad application mode");
        self.mode.insert(TermMode::APP_KEYPAD);
    }

    #[inline]
    fn unset_keypad_application_mode(&mut self) {
        trace!("Unsetting keypad application mode");
        self.mode.remove(TermMode::APP_KEYPAD);
    }

    #[inline]
    fn configure_charset(&mut self, index: CharsetIndex, charset: StandardCharset) {
        trace!("Configuring charset {index:?} as {charset:?}");
        self.grid.cursor.charsets[index] = charset;
    }

    #[inline]
    fn set_active_charset(&mut self, index: CharsetIndex) {
        trace!("Setting active charset {index:?}");
        self.active_charset = index;
    }

    #[inline]
    fn set_cursor_style(&mut self, style: Option<CursorStyle>) {
        trace!("Setting cursor style {style:?}");
        self.cursor_style = style;

        // Notify UI about blinking changes.
        self.event_proxy.send_event(Event::CursorBlinkingChange);
    }

    #[inline]
    fn set_cursor_shape(&mut self, shape: CursorShape) {
        trace!("Setting cursor shape {shape:?}");

        let style = self
            .cursor_style
            .get_or_insert(self.config.default_cursor_style);
        style.shape = shape;
    }

    #[inline]
    fn set_title(&mut self, title: Option<String>) {
        trace!("Setting title to '{title:?}'");

        self.title.clone_from(&title);

        let title_event = match title {
            Some(title) => Event::Title(title),
            None => Event::ResetTitle,
        };

        self.event_proxy.send_event(title_event);
    }

    #[inline]
    fn push_title(&mut self) {
        trace!("Pushing '{:?}' onto title stack", self.title);

        if self.title_stack.len() >= TITLE_STACK_MAX_DEPTH {
            let removed = self.title_stack.remove(0);
            trace!(
                "Removing '{removed:?}' from bottom of title stack that exceeds its maximum depth"
            );
        }

        self.title_stack.push(self.title.clone());
    }

    #[inline]
    fn pop_title(&mut self) {
        trace!("Attempting to pop title from stack...");

        if let Some(popped) = self.title_stack.pop() {
            trace!("Title '{popped:?}' popped from stack");
            self.set_title(popped);
        }
    }

    #[inline]
    fn text_area_size_pixels(&mut self) {
        self.event_proxy
            .send_event(Event::TextAreaSizeRequest(Arc::new(move |window_size| {
                let height = window_size.num_lines * window_size.cell_height;
                let width = window_size.num_cols * window_size.cell_width;
                format!("\x1b[4;{height};{width}t")
            })));
    }

    #[inline]
    fn cell_size_pixels(&mut self) {
        self.event_proxy
            .send_event(Event::CellSizeRequest(Arc::new(move |window_size| {
                format!(
                    "\x1b[6;{};{}t",
                    window_size.cell_height, window_size.cell_width
                )
            })));
    }

    #[inline]
    fn text_area_size_chars(&mut self) {
        let text = format!("\x1b[8;{};{}t", self.screen_lines(), self.columns());
        self.event_proxy.send_event(Event::PtyWrite(text));
    }

    /// DIRI PATCH: XTWINOPS 19; the screen is the text area here.
    #[inline]
    fn screen_size_chars(&mut self) {
        let text = format!("\x1b[9;{};{}t", self.screen_lines(), self.columns());
        self.event_proxy.send_event(Event::PtyWrite(text));
    }

    /// DIRI PATCH: XTVERSION.
    #[inline]
    fn report_version(&mut self) {
        if let Some(version) = &self.config.terminal_version {
            let text = format!("\x1bP>|{version}\x1b\\");
            self.event_proxy.send_event(Event::PtyWrite(text));
        }
    }

    /// DIRI PATCH: DECSTR. Resets the modes and attributes a crashed program
    /// may have left behind and keeps the screen, the cursor position and
    /// bracketed paste, as xterm does.
    fn soft_reset(&mut self) {
        trace!("Soft reset");
        self.mode
            .insert(TermMode::SHOW_CURSOR | TermMode::LINE_WRAP);
        self.mode.remove(
            TermMode::INSERT | TermMode::ORIGIN | TermMode::APP_CURSOR | TermMode::APP_KEYPAD,
        );
        self.scroll_region = Line(0)..Line(self.screen_lines() as i32);
        self.active_charset = Default::default();
        let cursor = &mut self.grid.cursor;
        cursor.template = Default::default();
        cursor.charsets = Default::default();
        let mut saved = cursor.clone();
        saved.point = Point::default();
        saved.input_needs_wrap = false;
        self.grid.saved_cursor = saved;
        if self.cursor_style.take().is_some() {
            self.event_proxy.send_event(Event::CursorBlinkingChange);
        }
    }

    /// DIRI PATCH: DEC-private DSR: extended cursor position and the
    /// color-scheme query.
    fn private_device_status(&mut self, arg: usize) {
        match arg {
            6 => {
                let pos = self.grid.cursor.point;
                let text = format!("\x1b[?{};{};1R", pos.line + 1, pos.column + 1);
                self.event_proxy.send_event(Event::PtyWrite(text));
            }
            996 => self.event_proxy.send_event(Event::ColorSchemeRequest),
            _ => debug!("unknown private device status query: {arg}"),
        }
    }

    /// DIRI PATCH: XTGETTCAP. One reply per requested name, in order.
    fn request_termcap(&mut self, names: &[u8]) {
        for hex in names.split(|&byte| byte == b';') {
            let name = decode_hex(hex);
            let text = match name.as_deref().and_then(termcap) {
                Some(Some(value)) => format!(
                    "\x1bP1+r{}={}\x1b\\",
                    hex_name(hex),
                    encode_hex(value.as_bytes())
                ),
                Some(None) => format!("\x1bP1+r{}\x1b\\", hex_name(hex)),
                None => format!("\x1bP0+r{}\x1b\\", hex_name(hex)),
            };
            self.event_proxy.send_event(Event::PtyWrite(text));
        }
    }

    /// DIRI PATCH: DECRQSS for SGR, the scrolling region and the cursor style.
    fn request_setting(&mut self, setting: &[u8]) {
        let value = match setting {
            b"m" => Some(self.sgr_report()),
            b"r" => Some(format!(
                "{};{}r",
                self.scroll_region.start.0 + 1,
                self.scroll_region.end.0
            )),
            b" q" => {
                let style = self
                    .cursor_style
                    .unwrap_or(self.config.default_cursor_style);
                let steady = match style.shape {
                    CursorShape::Underline => 4,
                    CursorShape::Beam => 6,
                    CursorShape::Block | CursorShape::HollowBlock | CursorShape::Hidden => 2,
                };
                Some(format!("{} q", steady - u8::from(style.blinking)))
            }
            _ => None,
        };
        let text = match value {
            Some(value) => format!("\x1bP1$r{value}\x1b\\"),
            None => String::from("\x1bP0$r\x1b\\"),
        };
        self.event_proxy.send_event(Event::PtyWrite(text));
    }
}

/// DIRI PATCH: XTGETTCAP answers. `Some(None)` is a boolean capability; the
/// values are terminfo strings, escapes spelled the way terminfo spells them.
fn termcap(name: &[u8]) -> Option<Option<&'static str>> {
    Some(match name {
        b"TN" | b"name" => Some("xterm-256color"),
        b"Co" | b"colors" => Some("256"),
        b"RGB" => Some("8"),
        b"Tc" => None,
        b"setrgbf" => Some("\\E[38;2;%p1%d;%p2%d;%p3%dm"),
        b"setrgbb" => Some("\\E[48;2;%p1%d;%p2%d;%p3%dm"),
        b"Smulx" => Some("\\E[4:%p1%dm"),
        b"Ss" => Some("\\E[%p1%d q"),
        b"Se" => Some("\\E[2 q"),
        b"kbs" => Some("\x7f"),
        _ => return None,
    })
}

/// The name as sent, upper-cased the way replies spell hex.
fn hex_name(hex: &[u8]) -> String {
    String::from_utf8_lossy(hex).to_ascii_uppercase()
}

fn decode_hex(hex: &[u8]) -> Option<Vec<u8>> {
    if hex.is_empty() || hex.len() % 2 != 0 {
        return None;
    }
    hex.chunks(2)
        .map(|pair| {
            let digit = |byte: u8| (byte as char).to_digit(16);
            Some((digit(pair[0])? * 16 + digit(pair[1])?) as u8)
        })
        .collect()
}

fn encode_hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, byte| {
            let _ = write!(out, "{byte:02X}");
            out
        })
}

/// The state of the [`Mode`] and [`PrivateMode`].
#[repr(u8)]
#[derive(Debug, Clone, Copy)]
enum ModeState {
    /// The mode is not supported.
    NotSupported = 0,
    /// The mode is currently set.
    Set = 1,
    /// The mode is currently not set.
    Reset = 2,
    /// DIRI PATCH: the mode is recognized but can never be set.
    PermanentlyReset = 4,
}

/// DIRI PATCH: how an alternate-screen mode treats the cursor and contents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AltScreen {
    /// 1049: save the cursor, clear the alternate screen on entry, restore
    /// the cursor on exit.
    SaveCursorAndClear,
    /// 47: one cursor, nothing cleared.
    Shared,
    /// 1047: one cursor, the alternate screen cleared on exit.
    ClearOnExit,
}

impl From<bool> for ModeState {
    fn from(value: bool) -> Self {
        if value { Self::Set } else { Self::Reset }
    }
}

/// Terminal version for escape sequence reports.
///
/// This returns the current terminal version as a unique number based on alacritty_terminal's
/// semver version. The different versions are padded to ensure that a higher semver version will
/// always report a higher version number.
fn version_number(mut version: &str) -> usize {
    if let Some(separator) = version.rfind('-') {
        version = &version[..separator];
    }

    let mut version_number = 0;

    let semver_versions = version.split('.');
    for (i, semver_version) in semver_versions.rev().enumerate() {
        let semver_number = semver_version.parse::<usize>().unwrap_or(0);
        version_number += usize::pow(100, i as u32) * semver_number;
    }

    version_number
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardType {
    Clipboard,
    Selection,
}

struct TabStops {
    tabs: Vec<bool>,
}

impl TabStops {
    #[inline]
    fn new(columns: usize) -> TabStops {
        TabStops {
            tabs: (0..columns).map(|i| i % INITIAL_TABSTOPS == 0).collect(),
        }
    }

    /// Remove all tabstops.
    #[inline]
    fn clear_all(&mut self) {
        unsafe {
            ptr::write_bytes(self.tabs.as_mut_ptr(), 0, self.tabs.len());
        }
    }

    /// Increase tabstop capacity.
    #[inline]
    fn resize(&mut self, columns: usize) {
        let mut index = self.tabs.len();
        self.tabs.resize_with(columns, || {
            let is_tabstop = index % INITIAL_TABSTOPS == 0;
            index += 1;
            is_tabstop
        });
    }
}

impl Index<Column> for TabStops {
    type Output = bool;

    fn index(&self, index: Column) -> &bool {
        &self.tabs[index.0]
    }
}

impl IndexMut<Column> for TabStops {
    fn index_mut(&mut self, index: Column) -> &mut bool {
        self.tabs.index_mut(index.0)
    }
}

/// Terminal cursor rendering information.
#[derive(Copy, Clone, PartialEq, Eq)]
pub struct RenderableCursor {
    pub shape: CursorShape,
    pub point: Point,
}

impl RenderableCursor {
    fn new<T>(term: &Term<T>) -> Self {
        // Cursor position.
        let vi_mode = term.mode().contains(TermMode::VI);
        let mut point = if vi_mode {
            term.vi_mode_cursor.point
        } else {
            term.grid.cursor.point
        };
        if term.grid[point].flags.contains(Flags::WIDE_CHAR_SPACER) {
            point.column -= 1;
        }

        // Cursor shape.
        let shape = if !vi_mode && !term.mode().contains(TermMode::SHOW_CURSOR) {
            CursorShape::Hidden
        } else {
            term.cursor_style().shape
        };

        Self { shape, point }
    }
}

/// Visible terminal content.
///
/// This contains all content required to render the current terminal view.
pub struct RenderableContent<'a> {
    pub display_iter: GridIterator<'a, Cell>,
    pub selection: Option<SelectionRange>,
    pub cursor: RenderableCursor,
    pub display_offset: usize,
    pub colors: &'a color::Colors,
    pub mode: TermMode,
}

impl<'a> RenderableContent<'a> {
    fn new<T>(term: &'a Term<T>) -> Self {
        Self {
            display_iter: term.grid().display_iter(),
            display_offset: term.grid().display_offset(),
            cursor: RenderableCursor::new(term),
            selection: term.selection.as_ref().and_then(|s| s.to_range(term)),
            colors: &term.colors,
            mode: *term.mode(),
        }
    }
}

/// Terminal test helpers.
pub mod test {
    use super::*;

    #[cfg(feature = "serde")]
    use serde::{Deserialize, Serialize};

    use crate::event::VoidListener;

    #[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
    pub struct TermSize {
        pub columns: usize,
        pub screen_lines: usize,
    }

    impl TermSize {
        pub fn new(columns: usize, screen_lines: usize) -> Self {
            Self {
                columns,
                screen_lines,
            }
        }
    }

    impl Dimensions for TermSize {
        fn total_lines(&self) -> usize {
            self.screen_lines()
        }

        fn screen_lines(&self) -> usize {
            self.screen_lines
        }

        fn columns(&self) -> usize {
            self.columns
        }
    }

    /// Construct a terminal from its content as string.
    ///
    /// A `\n` will break line and `\r\n` will break line without wrapping.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use alacritty_terminal::term::test::mock_term;
    ///
    /// // Create a terminal with the following cells:
    /// //
    /// // [h][e][l][l][o] <- WRAPLINE flag set
    /// // [:][)][ ][ ][ ]
    /// // [t][e][s][t][ ]
    /// mock_term(
    ///     "\
    ///     hello\n:)\r\ntest",
    /// );
    /// ```
    pub fn mock_term(content: &str) -> Term<VoidListener> {
        let lines: Vec<&str> = content.split('\n').collect();
        let num_cols = lines
            .iter()
            .map(|line| {
                line.chars()
                    .filter(|c| *c != '\r')
                    .map(|c| c.width().unwrap())
                    .sum()
            })
            .max()
            .unwrap_or(0);

        // Create terminal with the appropriate dimensions.
        let size = TermSize::new(num_cols, lines.len());
        let mut term = Term::new(Config::default(), &size, VoidListener);

        // Fill terminal with content.
        for (line, text) in lines.iter().enumerate() {
            let line = Line(line as i32);
            if !text.ends_with('\r') && line + 1 != lines.len() {
                term.grid[line][Column(num_cols - 1)]
                    .flags
                    .insert(Flags::WRAPLINE);
            }

            let mut index = 0;
            for c in text.chars().take_while(|c| *c != '\r') {
                term.grid[line][Column(index)].c = c;

                // Handle fullwidth characters.
                let width = c.width().unwrap();
                if width == 2 {
                    term.grid[line][Column(index)]
                        .flags
                        .insert(Flags::WIDE_CHAR);
                    term.grid[line][Column(index + 1)]
                        .flags
                        .insert(Flags::WIDE_CHAR_SPACER);
                }

                index += width;
            }
        }

        term
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::mem;

    use crate::event::VoidListener;
    use crate::grid::{Grid, Scroll};
    use crate::index::{Column, Point, Side};
    use crate::selection::{Selection, SelectionType};
    use crate::term::cell::{Cell, Flags};
    use crate::term::test::TermSize;
    use crate::vte::ansi::{self, CharsetIndex, Handler, StandardCharset};

    #[test]
    fn keyboard_query_and_screen_switch_preserve_directly_set_flags() {
        #[derive(Clone)]
        struct Replies(std::sync::Arc<std::sync::Mutex<Vec<String>>>);
        impl EventListener for Replies {
            fn send_event(&self, event: Event) {
                if let Event::PtyWrite(reply) = event {
                    self.0.lock().unwrap().push(reply);
                }
            }
        }
        let replies = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut term = Term::new(
            Config {
                kitty_keyboard: true,
                ..Config::default()
            },
            &TermSize::new(5, 10),
            Replies(replies.clone()),
        );
        let mut parser: ansi::Processor = ansi::Processor::new();
        parser.advance(&mut term, b"\x1b[=5u\x1b[?u");
        assert_eq!(*replies.lock().unwrap(), vec!["\x1b[?5u"]);
        parser.advance(&mut term, b"\x1b[?1049h\x1b[=3u\x1b[?u\x1b[?1049l\x1b[?u");
        assert_eq!(
            *replies.lock().unwrap(),
            vec!["\x1b[?5u", "\x1b[?3u", "\x1b[?5u"]
        );
        parser.advance(&mut term, b"\x1b[>1u\x1b[=2;2u\x1b[>8u\x1b[<u\x1b[?u");
        assert_eq!(replies.lock().unwrap().last().unwrap(), "\x1b[?3u");
        parser.advance(&mut term, b"\x1b[=1;3u\x1b[?u");
        assert_eq!(replies.lock().unwrap().last().unwrap(), "\x1b[?2u");
        parser.advance(&mut term, b"\x1b[<u\x1b[?u");
        assert_eq!(replies.lock().unwrap().last().unwrap(), "\x1b[?5u");
        parser.advance(&mut term, b"\x1bc\x1b[?u");
        assert_eq!(replies.lock().unwrap().last().unwrap(), "\x1b[?0u");
    }

    #[test]
    fn a_coloured_line_widened_and_then_recycled_clears_its_old_columns() {
        // A line scrolled in under a red background is red blanks above an
        // occupancy of zero. Widening appends default cells; recycling the
        // row for a new line must still clear the red ones, or they show up
        // on an unrelated line.
        for compact in [false, true] {
            let mut term = Term::new(
                Config {
                    scrolling_history: 4,
                    ..Config::default()
                },
                &TermSize::new(10, 2),
                VoidListener,
            );
            #[cfg(feature = "compact-history")]
            if compact {
                term.grid_mut().enable_compact_history();
            }
            let mut parser: crate::vte::ansi::Processor = crate::vte::ansi::Processor::new();
            parser.advance(&mut term, b"\x1b[41m\r\n\r\n\x1b[0m");
            term.resize(TermSize::new(20, 2));
            parser.advance(&mut term, b"\r\n\r\n\r\n\r\n\r\n\r\n\r\n\r\nx");
            let grid = term.grid();
            for line in -(grid.history_size() as i32)..2 {
                for cell in &grid[Line(line)][..] {
                    assert_eq!(
                        cell.bg,
                        Color::Named(NamedColor::Background),
                        "compact {compact} line {line}"
                    );
                }
            }
            #[cfg(not(feature = "compact-history"))]
            let _ = compact;
        }
    }

    #[test]
    fn scroll_display_page_up() {
        let size = TermSize::new(5, 10);
        let mut term = Term::new(Config::default(), &size, VoidListener);

        // Create 11 lines of scrollback.
        for _ in 0..20 {
            term.newline();
        }

        // Scrollable amount to top is 11.
        term.scroll_display(Scroll::PageUp);
        assert_eq!(term.vi_mode_cursor.point, Point::new(Line(-1), Column(0)));
        assert_eq!(term.grid.display_offset(), 10);

        // Scrollable amount to top is 1.
        term.scroll_display(Scroll::PageUp);
        assert_eq!(term.vi_mode_cursor.point, Point::new(Line(-2), Column(0)));
        assert_eq!(term.grid.display_offset(), 11);

        // Scrollable amount to top is 0.
        term.scroll_display(Scroll::PageUp);
        assert_eq!(term.vi_mode_cursor.point, Point::new(Line(-2), Column(0)));
        assert_eq!(term.grid.display_offset(), 11);
    }

    #[test]
    fn scroll_display_page_down() {
        let size = TermSize::new(5, 10);
        let mut term = Term::new(Config::default(), &size, VoidListener);

        // Create 11 lines of scrollback.
        for _ in 0..20 {
            term.newline();
        }

        // Change display_offset to topmost.
        term.grid_mut().scroll_display(Scroll::Top);
        term.vi_mode_cursor = ViModeCursor::new(Point::new(Line(-11), Column(0)));

        // Scrollable amount to bottom is 11.
        term.scroll_display(Scroll::PageDown);
        assert_eq!(term.vi_mode_cursor.point, Point::new(Line(-1), Column(0)));
        assert_eq!(term.grid.display_offset(), 1);

        // Scrollable amount to bottom is 1.
        term.scroll_display(Scroll::PageDown);
        assert_eq!(term.vi_mode_cursor.point, Point::new(Line(0), Column(0)));
        assert_eq!(term.grid.display_offset(), 0);

        // Scrollable amount to bottom is 0.
        term.scroll_display(Scroll::PageDown);
        assert_eq!(term.vi_mode_cursor.point, Point::new(Line(0), Column(0)));
        assert_eq!(term.grid.display_offset(), 0);
    }

    #[test]
    fn simple_selection_works() {
        let size = TermSize::new(5, 5);
        let mut term = Term::new(Config::default(), &size, VoidListener);
        let grid = term.grid_mut();
        for i in 0..4 {
            if i == 1 {
                continue;
            }

            grid[Line(i)][Column(0)].c = '"';

            for j in 1..4 {
                grid[Line(i)][Column(j)].c = 'a';
            }

            grid[Line(i)][Column(4)].c = '"';
        }
        grid[Line(2)][Column(0)].c = ' ';
        grid[Line(2)][Column(4)].c = ' ';
        grid[Line(2)][Column(4)].flags.insert(Flags::WRAPLINE);
        grid[Line(3)][Column(0)].c = ' ';

        // Multiple lines contain an empty line.
        term.selection = Some(Selection::new(
            SelectionType::Simple,
            Point {
                line: Line(0),
                column: Column(0),
            },
            Side::Left,
        ));
        if let Some(s) = term.selection.as_mut() {
            s.update(
                Point {
                    line: Line(2),
                    column: Column(4),
                },
                Side::Right,
            );
        }
        assert_eq!(
            term.selection_to_string(),
            Some(String::from("\"aaa\"\n\n aaa "))
        );

        // A wrapline.
        term.selection = Some(Selection::new(
            SelectionType::Simple,
            Point {
                line: Line(2),
                column: Column(0),
            },
            Side::Left,
        ));
        if let Some(s) = term.selection.as_mut() {
            s.update(
                Point {
                    line: Line(3),
                    column: Column(4),
                },
                Side::Right,
            );
        }
        assert_eq!(
            term.selection_to_string(),
            Some(String::from(" aaa  aaa\""))
        );
    }

    #[test]
    fn semantic_selection_works() {
        let size = TermSize::new(5, 3);
        let mut term = Term::new(Config::default(), &size, VoidListener);
        let mut grid: Grid<Cell> = Grid::new(3, 5, 0);
        for i in 0..5 {
            for j in 0..2 {
                grid[Line(j)][Column(i)].c = 'a';
            }
        }
        grid[Line(0)][Column(0)].c = '"';
        grid[Line(0)][Column(3)].c = '"';
        grid[Line(1)][Column(2)].c = '"';
        grid[Line(0)][Column(4)].flags.insert(Flags::WRAPLINE);

        let mut escape_chars = String::from("\"");

        mem::swap(&mut term.grid, &mut grid);
        mem::swap(&mut term.config.semantic_escape_chars, &mut escape_chars);

        {
            term.selection = Some(Selection::new(
                SelectionType::Semantic,
                Point {
                    line: Line(0),
                    column: Column(1),
                },
                Side::Left,
            ));
            assert_eq!(term.selection_to_string(), Some(String::from("aa")));
        }

        {
            term.selection = Some(Selection::new(
                SelectionType::Semantic,
                Point {
                    line: Line(0),
                    column: Column(4),
                },
                Side::Left,
            ));
            assert_eq!(term.selection_to_string(), Some(String::from("aaa")));
        }

        {
            term.selection = Some(Selection::new(
                SelectionType::Semantic,
                Point {
                    line: Line(1),
                    column: Column(1),
                },
                Side::Left,
            ));
            assert_eq!(term.selection_to_string(), Some(String::from("aaa")));
        }
    }

    #[test]
    fn line_selection_works() {
        let size = TermSize::new(5, 1);
        let mut term = Term::new(Config::default(), &size, VoidListener);
        let mut grid: Grid<Cell> = Grid::new(1, 5, 0);
        for i in 0..5 {
            grid[Line(0)][Column(i)].c = 'a';
        }
        grid[Line(0)][Column(0)].c = '"';
        grid[Line(0)][Column(3)].c = '"';

        mem::swap(&mut term.grid, &mut grid);

        term.selection = Some(Selection::new(
            SelectionType::Lines,
            Point {
                line: Line(0),
                column: Column(3),
            },
            Side::Left,
        ));
        assert_eq!(term.selection_to_string(), Some(String::from("\"aa\"a\n")));
    }

    #[test]
    fn block_selection_works() {
        let size = TermSize::new(5, 5);
        let mut term = Term::new(Config::default(), &size, VoidListener);
        let grid = term.grid_mut();
        for i in 1..4 {
            grid[Line(i)][Column(0)].c = '"';

            for j in 1..4 {
                grid[Line(i)][Column(j)].c = 'a';
            }

            grid[Line(i)][Column(4)].c = '"';
        }
        grid[Line(2)][Column(2)].c = ' ';
        grid[Line(2)][Column(4)].flags.insert(Flags::WRAPLINE);
        grid[Line(3)][Column(4)].c = ' ';

        term.selection = Some(Selection::new(
            SelectionType::Block,
            Point {
                line: Line(0),
                column: Column(3),
            },
            Side::Left,
        ));

        // The same column.
        if let Some(s) = term.selection.as_mut() {
            s.update(
                Point {
                    line: Line(3),
                    column: Column(3),
                },
                Side::Right,
            );
        }
        assert_eq!(term.selection_to_string(), Some(String::from("\na\na\na")));

        // The first column.
        if let Some(s) = term.selection.as_mut() {
            s.update(
                Point {
                    line: Line(3),
                    column: Column(0),
                },
                Side::Left,
            );
        }
        assert_eq!(
            term.selection_to_string(),
            Some(String::from("\n\"aa\n\"a\n\"aa"))
        );

        // The last column.
        if let Some(s) = term.selection.as_mut() {
            s.update(
                Point {
                    line: Line(3),
                    column: Column(4),
                },
                Side::Right,
            );
        }
        assert_eq!(
            term.selection_to_string(),
            Some(String::from("\na\"\na\"\na"))
        );
    }

    /// Check that the grid can be serialized back and forth losslessly.
    ///
    /// This test is in the term module as opposed to the grid since we want to
    /// test this property with a T=Cell.
    #[test]
    #[cfg(feature = "serde")]
    fn grid_serde() {
        let grid: Grid<Cell> = Grid::new(24, 80, 0);
        let serialized = serde_json::to_string(&grid).expect("ser");
        let deserialized = serde_json::from_str::<Grid<Cell>>(&serialized).expect("de");

        assert_eq!(deserialized, grid);
    }

    #[test]
    fn input_line_drawing_character() {
        let size = TermSize::new(7, 17);
        let mut term = Term::new(Config::default(), &size, VoidListener);
        let cursor = Point::new(Line(0), Column(0));
        term.configure_charset(
            CharsetIndex::G0,
            StandardCharset::SpecialCharacterAndLineDrawing,
        );
        term.input('a');

        assert_eq!(term.grid()[cursor].c, '▒');
    }

    #[test]
    fn clearing_viewport_keeps_history_position() {
        let size = TermSize::new(10, 20);
        let mut term = Term::new(Config::default(), &size, VoidListener);

        // Create 10 lines of scrollback.
        for _ in 0..29 {
            term.newline();
        }

        // Change the display area.
        term.scroll_display(Scroll::Top);

        assert_eq!(term.grid.display_offset(), 10);

        // Clear the viewport.
        term.clear_screen(ansi::ClearMode::All);

        assert_eq!(term.grid.display_offset(), 10);
    }

    #[test]
    fn clearing_viewport_with_vi_mode_keeps_history_position() {
        let size = TermSize::new(10, 20);
        let mut term = Term::new(Config::default(), &size, VoidListener);

        // Create 10 lines of scrollback.
        for _ in 0..29 {
            term.newline();
        }

        // Enable vi mode.
        term.toggle_vi_mode();

        // Change the display area and the vi cursor position.
        term.scroll_display(Scroll::Top);
        term.vi_mode_cursor.point = Point::new(Line(-5), Column(3));

        assert_eq!(term.grid.display_offset(), 10);

        // Clear the viewport.
        term.clear_screen(ansi::ClearMode::All);

        assert_eq!(term.grid.display_offset(), 10);
        assert_eq!(term.vi_mode_cursor.point, Point::new(Line(-5), Column(3)));
    }

    #[test]
    fn clearing_scrollback_resets_display_offset() {
        let size = TermSize::new(10, 20);
        let mut term = Term::new(Config::default(), &size, VoidListener);

        // Create 10 lines of scrollback.
        for _ in 0..29 {
            term.newline();
        }

        // Change the display area.
        term.scroll_display(Scroll::Top);

        assert_eq!(term.grid.display_offset(), 10);

        // Clear the scrollback buffer.
        term.clear_screen(ansi::ClearMode::Saved);

        assert_eq!(term.grid.display_offset(), 0);
    }

    #[test]
    fn clearing_scrollback_sets_vi_cursor_into_viewport() {
        let size = TermSize::new(10, 20);
        let mut term = Term::new(Config::default(), &size, VoidListener);

        // Create 10 lines of scrollback.
        for _ in 0..29 {
            term.newline();
        }

        // Enable vi mode.
        term.toggle_vi_mode();

        // Change the display area and the vi cursor position.
        term.scroll_display(Scroll::Top);
        term.vi_mode_cursor.point = Point::new(Line(-5), Column(3));

        assert_eq!(term.grid.display_offset(), 10);

        // Clear the scrollback buffer.
        term.clear_screen(ansi::ClearMode::Saved);

        assert_eq!(term.grid.display_offset(), 0);
        assert_eq!(term.vi_mode_cursor.point, Point::new(Line(0), Column(3)));
    }

    #[test]
    fn clear_saved_lines() {
        let size = TermSize::new(7, 17);
        let mut term = Term::new(Config::default(), &size, VoidListener);

        // Add one line of scrollback.
        term.grid.scroll_up(&(Line(0)..Line(1)), 1);

        // Clear the history.
        term.clear_screen(ansi::ClearMode::Saved);

        // Make sure that scrolling does not change the grid.
        let mut scrolled_grid = term.grid.clone();
        scrolled_grid.scroll_display(Scroll::Top);

        // Truncate grids for comparison.
        scrolled_grid.truncate();
        term.grid.truncate();

        assert_eq!(term.grid, scrolled_grid);
    }

    #[test]
    fn vi_cursor_keep_pos_on_scrollback_buffer() {
        let size = TermSize::new(5, 10);
        let mut term = Term::new(Config::default(), &size, VoidListener);

        // Create 11 lines of scrollback.
        for _ in 0..20 {
            term.newline();
        }

        // Enable vi mode.
        term.toggle_vi_mode();

        term.scroll_display(Scroll::Top);
        term.vi_mode_cursor.point.line = Line(-11);

        term.linefeed();
        assert_eq!(term.vi_mode_cursor.point.line, Line(-12));
    }

    #[test]
    fn grow_lines_updates_active_cursor_pos() {
        let mut size = TermSize::new(100, 10);
        let mut term = Term::new(Config::default(), &size, VoidListener);

        // Create 10 lines of scrollback.
        for _ in 0..19 {
            term.newline();
        }
        assert_eq!(term.history_size(), 10);
        assert_eq!(term.grid.cursor.point, Point::new(Line(9), Column(0)));

        // Increase visible lines.
        size.screen_lines = 30;
        term.resize(size);

        assert_eq!(term.history_size(), 0);
        assert_eq!(term.grid.cursor.point, Point::new(Line(19), Column(0)));
    }

    #[test]
    fn grow_lines_updates_inactive_cursor_pos() {
        let mut size = TermSize::new(100, 10);
        let mut term = Term::new(Config::default(), &size, VoidListener);

        // Create 10 lines of scrollback.
        for _ in 0..19 {
            term.newline();
        }
        assert_eq!(term.history_size(), 10);
        assert_eq!(term.grid.cursor.point, Point::new(Line(9), Column(0)));

        // Enter alt screen.
        term.set_private_mode(NamedPrivateMode::SwapScreenAndSetRestoreCursor.into());

        // Increase visible lines.
        size.screen_lines = 30;
        term.resize(size);

        // Leave alt screen.
        term.unset_private_mode(NamedPrivateMode::SwapScreenAndSetRestoreCursor.into());

        assert_eq!(term.history_size(), 0);
        assert_eq!(term.grid.cursor.point, Point::new(Line(19), Column(0)));
    }

    #[test]
    fn shrink_lines_updates_active_cursor_pos() {
        let mut size = TermSize::new(100, 10);
        let mut term = Term::new(Config::default(), &size, VoidListener);

        // Create 10 lines of scrollback.
        for _ in 0..19 {
            term.newline();
        }
        assert_eq!(term.history_size(), 10);
        assert_eq!(term.grid.cursor.point, Point::new(Line(9), Column(0)));

        // Increase visible lines.
        size.screen_lines = 5;
        term.resize(size);

        assert_eq!(term.history_size(), 15);
        assert_eq!(term.grid.cursor.point, Point::new(Line(4), Column(0)));
    }

    #[test]
    fn shrink_lines_updates_inactive_cursor_pos() {
        let mut size = TermSize::new(100, 10);
        let mut term = Term::new(Config::default(), &size, VoidListener);

        // Create 10 lines of scrollback.
        for _ in 0..19 {
            term.newline();
        }
        assert_eq!(term.history_size(), 10);
        assert_eq!(term.grid.cursor.point, Point::new(Line(9), Column(0)));

        // Enter alt screen.
        term.set_private_mode(NamedPrivateMode::SwapScreenAndSetRestoreCursor.into());

        // Increase visible lines.
        size.screen_lines = 5;
        term.resize(size);

        // Leave alt screen.
        term.unset_private_mode(NamedPrivateMode::SwapScreenAndSetRestoreCursor.into());

        assert_eq!(term.history_size(), 15);
        assert_eq!(term.grid.cursor.point, Point::new(Line(4), Column(0)));
    }

    #[test]
    fn damage_public_usage() {
        let size = TermSize::new(10, 10);
        let mut term = Term::new(Config::default(), &size, VoidListener);
        // Reset terminal for partial damage tests since it's initialized as fully damaged.
        term.reset_damage();

        // Test that we damage input form [`Term::input`].

        let left = term.grid.cursor.point.column.0;
        term.input('d');
        term.input('a');
        term.input('m');
        term.input('a');
        term.input('g');
        term.input('e');
        let right = term.grid.cursor.point.column.0;

        let mut damaged_lines = match term.damage() {
            TermDamage::Full => panic!("Expected partial damage, however got Full"),
            TermDamage::Partial(damaged_lines) => damaged_lines,
        };
        assert_eq!(
            damaged_lines.next(),
            Some(LineDamageBounds {
                line: 0,
                left,
                right
            })
        );
        assert_eq!(damaged_lines.next(), None);
        term.reset_damage();

        // Create scrollback.
        for _ in 0..20 {
            term.newline();
        }

        match term.damage() {
            TermDamage::Full => (),
            TermDamage::Partial(_) => panic!("Expected Full damage, however got Partial "),
        };
        term.reset_damage();

        term.scroll_display(Scroll::Delta(10));
        term.reset_damage();

        // No damage when scrolled into viewport.
        for idx in 0..term.columns() {
            term.goto(idx as i32, idx);
        }
        let mut damaged_lines = match term.damage() {
            TermDamage::Full => panic!("Expected partial damage, however got Full"),
            TermDamage::Partial(damaged_lines) => damaged_lines,
        };
        assert_eq!(damaged_lines.next(), None);

        // Scroll back into the viewport, so we have 2 visible lines which terminal can write
        // to.
        term.scroll_display(Scroll::Delta(-2));
        term.reset_damage();

        term.goto(0, 0);
        term.goto(1, 0);
        term.goto(2, 0);
        let display_offset = term.grid().display_offset();
        let mut damaged_lines = match term.damage() {
            TermDamage::Full => panic!("Expected partial damage, however got Full"),
            TermDamage::Partial(damaged_lines) => damaged_lines,
        };
        assert_eq!(
            damaged_lines.next(),
            Some(LineDamageBounds {
                line: display_offset,
                left: 0,
                right: 0
            })
        );
        assert_eq!(
            damaged_lines.next(),
            Some(LineDamageBounds {
                line: display_offset + 1,
                left: 0,
                right: 0
            })
        );
        assert_eq!(damaged_lines.next(), None);
    }

    #[test]
    fn damage_cursor_movements() {
        let size = TermSize::new(10, 10);
        let mut term = Term::new(Config::default(), &size, VoidListener);
        let num_cols = term.columns();
        // Reset terminal for partial damage tests since it's initialized as fully damaged.
        term.reset_damage();

        term.goto(1, 1);

        // NOTE While we can use `[Term::damage]` to access terminal damage information, in the
        // following tests we will be accessing `term.damage.lines` directly to avoid adding extra
        // damage information (like cursor and Vi cursor), which we're not testing.

        assert_eq!(
            term.damage.lines[0],
            LineDamageBounds {
                line: 0,
                left: 0,
                right: 0
            }
        );
        assert_eq!(
            term.damage.lines[1],
            LineDamageBounds {
                line: 1,
                left: 1,
                right: 1
            }
        );
        term.damage.reset(num_cols);

        term.move_forward(3);
        assert_eq!(
            term.damage.lines[1],
            LineDamageBounds {
                line: 1,
                left: 1,
                right: 4
            }
        );
        term.damage.reset(num_cols);

        term.move_backward(8);
        assert_eq!(
            term.damage.lines[1],
            LineDamageBounds {
                line: 1,
                left: 0,
                right: 4
            }
        );
        term.goto(5, 5);
        term.damage.reset(num_cols);

        term.backspace();
        term.backspace();
        assert_eq!(
            term.damage.lines[5],
            LineDamageBounds {
                line: 5,
                left: 3,
                right: 5
            }
        );
        term.damage.reset(num_cols);

        term.move_up(1);
        assert_eq!(
            term.damage.lines[5],
            LineDamageBounds {
                line: 5,
                left: 3,
                right: 3
            }
        );
        assert_eq!(
            term.damage.lines[4],
            LineDamageBounds {
                line: 4,
                left: 3,
                right: 3
            }
        );
        term.damage.reset(num_cols);

        term.move_down(1);
        term.move_down(1);
        assert_eq!(
            term.damage.lines[4],
            LineDamageBounds {
                line: 4,
                left: 3,
                right: 3
            }
        );
        assert_eq!(
            term.damage.lines[5],
            LineDamageBounds {
                line: 5,
                left: 3,
                right: 3
            }
        );
        assert_eq!(
            term.damage.lines[6],
            LineDamageBounds {
                line: 6,
                left: 3,
                right: 3
            }
        );
        term.damage.reset(num_cols);

        term.wrapline();
        assert_eq!(
            term.damage.lines[6],
            LineDamageBounds {
                line: 6,
                left: 3,
                right: 3
            }
        );
        assert_eq!(
            term.damage.lines[7],
            LineDamageBounds {
                line: 7,
                left: 0,
                right: 0
            }
        );
        term.move_forward(3);
        term.move_up(1);
        term.damage.reset(num_cols);

        term.linefeed();
        assert_eq!(
            term.damage.lines[6],
            LineDamageBounds {
                line: 6,
                left: 3,
                right: 3
            }
        );
        assert_eq!(
            term.damage.lines[7],
            LineDamageBounds {
                line: 7,
                left: 3,
                right: 3
            }
        );
        term.damage.reset(num_cols);

        term.carriage_return();
        assert_eq!(
            term.damage.lines[7],
            LineDamageBounds {
                line: 7,
                left: 0,
                right: 3
            }
        );
        term.damage.reset(num_cols);

        term.erase_chars(5);
        assert_eq!(
            term.damage.lines[7],
            LineDamageBounds {
                line: 7,
                left: 0,
                right: 5
            }
        );
        term.damage.reset(num_cols);

        term.delete_chars(3);
        let right = term.columns() - 1;
        assert_eq!(
            term.damage.lines[7],
            LineDamageBounds {
                line: 7,
                left: 0,
                right
            }
        );
        term.move_forward(term.columns());
        term.damage.reset(num_cols);

        term.move_backward_tabs(1);
        assert_eq!(
            term.damage.lines[7],
            LineDamageBounds {
                line: 7,
                left: 8,
                right
            }
        );
        term.save_cursor_position();
        term.goto(1, 1);
        term.damage.reset(num_cols);

        term.restore_cursor_position();
        assert_eq!(
            term.damage.lines[1],
            LineDamageBounds {
                line: 1,
                left: 1,
                right: 1
            }
        );
        assert_eq!(
            term.damage.lines[7],
            LineDamageBounds {
                line: 7,
                left: 8,
                right: 8
            }
        );
        term.damage.reset(num_cols);

        term.clear_line(ansi::LineClearMode::All);
        assert_eq!(
            term.damage.lines[7],
            LineDamageBounds {
                line: 7,
                left: 0,
                right
            }
        );
        term.damage.reset(num_cols);

        term.clear_line(ansi::LineClearMode::Left);
        assert_eq!(
            term.damage.lines[7],
            LineDamageBounds {
                line: 7,
                left: 0,
                right: 8
            }
        );
        term.damage.reset(num_cols);

        term.clear_line(ansi::LineClearMode::Right);
        assert_eq!(
            term.damage.lines[7],
            LineDamageBounds {
                line: 7,
                left: 8,
                right
            }
        );
        term.damage.reset(num_cols);

        term.reverse_index();
        assert_eq!(
            term.damage.lines[7],
            LineDamageBounds {
                line: 7,
                left: 8,
                right: 8
            }
        );
        assert_eq!(
            term.damage.lines[6],
            LineDamageBounds {
                line: 6,
                left: 8,
                right: 8
            }
        );
    }

    #[test]
    fn full_damage() {
        let size = TermSize::new(100, 10);
        let mut term = Term::new(Config::default(), &size, VoidListener);

        assert!(term.damage.full);
        for _ in 0..20 {
            term.newline();
        }
        term.reset_damage();

        term.clear_screen(ansi::ClearMode::Above);
        assert!(term.damage.full);
        term.reset_damage();

        term.scroll_display(Scroll::Top);
        assert!(term.damage.full);
        term.reset_damage();

        // Sequential call to scroll display without doing anything shouldn't damage.
        term.scroll_display(Scroll::Top);
        assert!(!term.damage.full);
        term.reset_damage();

        term.set_options(Config::default());
        assert!(term.damage.full);
        term.reset_damage();

        term.scroll_down_relative(Line(5), 2);
        assert!(term.damage.full);
        term.reset_damage();

        term.scroll_up_relative(Line(3), 2);
        assert!(term.damage.full);
        term.reset_damage();

        term.deccolm();
        assert!(term.damage.full);
        term.reset_damage();

        term.decaln();
        assert!(term.damage.full);
        term.reset_damage();

        term.set_mode(NamedMode::Insert.into());
        // Just setting `Insert` mode shouldn't mark terminal as damaged.
        assert!(!term.damage.full);
        term.reset_damage();

        let color_index = 257;
        term.set_color(color_index, Rgb::default());
        assert!(term.damage.full);
        term.reset_damage();

        // Setting the same color once again shouldn't trigger full damage.
        term.set_color(color_index, Rgb::default());
        assert!(!term.damage.full);

        term.reset_color(color_index);
        assert!(term.damage.full);
        term.reset_damage();

        // We shouldn't trigger fully damage when cursor gets update.
        term.set_color(NamedColor::Cursor as usize, Rgb::default());
        assert!(!term.damage.full);

        // However requesting terminal damage should mark terminal as fully damaged in `Insert`
        // mode.
        let _ = term.damage();
        assert!(term.damage.full);
        term.reset_damage();

        term.unset_mode(NamedMode::Insert.into());
        assert!(term.damage.full);
        term.reset_damage();

        // Keep this as a last check, so we don't have to deal with restoring from alt-screen.
        term.swap_alt();
        assert!(term.damage.full);
        term.reset_damage();

        let size = TermSize::new(10, 10);
        term.resize(size);
        assert!(term.damage.full);
    }

    #[test]
    fn window_title() {
        let size = TermSize::new(7, 17);
        let mut term = Term::new(Config::default(), &size, VoidListener);

        // Title None by default.
        assert_eq!(term.title, None);

        // Title can be set.
        term.set_title(Some("Test".into()));
        assert_eq!(term.title, Some("Test".into()));

        // Title can be pushed onto stack.
        term.push_title();
        term.set_title(Some("Next".into()));
        assert_eq!(term.title, Some("Next".into()));
        assert_eq!(term.title_stack.first().unwrap(), &Some("Test".into()));

        // Title can be popped from stack and set as the window title.
        term.pop_title();
        assert_eq!(term.title, Some("Test".into()));
        assert!(term.title_stack.is_empty());

        // Title stack doesn't grow infinitely.
        for _ in 0..4097 {
            term.push_title();
        }
        assert_eq!(term.title_stack.len(), 4096);

        // Title and title stack reset when terminal state is reset.
        term.push_title();
        term.reset_state();
        assert_eq!(term.title, None);
        assert!(term.title_stack.is_empty());

        // Title stack pops back to default.
        term.title = None;
        term.push_title();
        term.set_title(Some("Test".into()));
        term.pop_title();
        assert_eq!(term.title, None);

        // Title can be reset to default.
        term.title = Some("Test".into());
        term.set_title(None);
        assert_eq!(term.title, None);
    }

    #[test]
    fn keyboard_stack_overflow_preserves_titles_and_evicts_only_oldest_modes() {
        let size = TermSize::new(7, 17);
        let mut term = Term::new(
            Config {
                kitty_keyboard: true,
                ..Config::default()
            },
            &size,
            VoidListener,
        );
        term.push_keyboard_mode(KeyboardModes::DISAMBIGUATE_ESC_CODES);
        // An empty title stack must not make the 4,097th keyboard push panic.
        for _ in 0..KEYBOARD_MODE_STACK_MAX_DEPTH {
            term.push_keyboard_mode(KeyboardModes::REPORT_EVENT_TYPES);
        }
        assert_eq!(
            term.keyboard_mode_stack.len(),
            KEYBOARD_MODE_STACK_MAX_DEPTH
        );
        assert!(term.title_stack.is_empty());
        assert!(
            term.keyboard_mode_stack
                .iter()
                .all(|mode| *mode == KeyboardModes::REPORT_EVENT_TYPES)
        );
        term.set_title(Some("saved window title".into()));
        term.push_title();
        term.push_keyboard_mode(KeyboardModes::REPORT_ALL_KEYS_AS_ESC);
        assert_eq!(term.title_stack, vec![Some("saved window title".into())]);
        assert_eq!(
            term.keyboard_mode_stack.len(),
            KEYBOARD_MODE_STACK_MAX_DEPTH
        );
        term.pop_keyboard_modes(1);
        assert!(term.mode.contains(TermMode::REPORT_EVENT_TYPES));
        assert!(!term.mode.contains(TermMode::REPORT_ALL_KEYS_AS_ESC));
    }

    #[test]
    fn parse_cargo_version() {
        assert!(version_number(env!("CARGO_PKG_VERSION")) >= 10_01);
        assert_eq!(version_number("0.0.1-dev"), 1);
        assert_eq!(version_number("0.1.2-dev"), 1_02);
        assert_eq!(version_number("1.2.3-dev"), 1_02_03);
        assert_eq!(version_number("999.99.99"), 9_99_99_99);
    }
}

#[cfg(test)]
mod ascii_run_tests {
    use super::*;
    use crate::event::VoidListener;
    use crate::term::test::TermSize;
    use crate::vte::ansi::{Handler, Processor};

    fn assert_same(bulk: &Term<VoidListener>, single: &Term<VoidListener>, step: usize) {
        let (a, b) = (bulk.grid(), single.grid());
        assert_eq!(a.cursor, b.cursor, "cursor after step {step}");
        assert_eq!(
            a.saved_cursor, b.saved_cursor,
            "saved cursor after step {step}"
        );
        assert_eq!(
            a.history_size(),
            b.history_size(),
            "history after step {step}"
        );
        assert_eq!(bulk.mode(), single.mode(), "mode after step {step}");
        for line in -(b.history_size() as i32)..b.screen_lines() as i32 {
            let line = Line(line);
            assert_eq!(a[line], b[line], "line {line} after step {step}");
            assert_eq!(
                a[line].occ, b[line].occ,
                "occupancy of {line} after step {step}"
            );
        }
    }

    /// `input_ascii` must equal `input` per character across wrapping,
    /// insert mode, charsets, wide characters, hyperlinks, prompt marks,
    /// scroll regions and the alternate screen.
    #[test]
    fn ascii_runs_match_per_character_input() {
        for (compact, columns, lines) in [(false, 13, 5), (true, 17, 7), (false, 2, 3)] {
            let size = TermSize::new(columns, lines);
            let config = Config {
                scrolling_history: 40,
                ..Config::default()
            };
            let mut bulk = Term::new(config.clone(), &size, VoidListener);
            let mut single = Term::new(config, &size, VoidListener);
            #[cfg(feature = "compact-history")]
            if compact {
                bulk.grid_mut().enable_compact_history();
                single.grid_mut().enable_compact_history();
            }
            #[cfg(not(feature = "compact-history"))]
            let _ = compact;
            let mut bulk_parser: Processor = Processor::new();
            let mut single_parser: Processor = Processor::new();
            let sequences: &[&str] = &[
                "\r\n",
                "\r",
                "\n",
                "\t",
                "\x08",
                "\x1b[4h",
                "\x1b[4l",
                "\x1b[?7l",
                "\x1b[?7h",
                "\x1b(0",
                "\x1b(B",
                "\x1b)0\x0e",
                "\x0f",
                "\x1b[31;1m",
                "\x1b[38;2;1;2;3;48;5;17m",
                "\x1b[0m",
                "\x1b[4:3m\x1b[58;5;9m",
                "\x1b]8;id=x;https://example.invalid\x07",
                "\x1b]8;;\x07",
                "\x1b]133;A\x07",
                "\x1b]133;B\x07",
                "\x1b]133;C\x07",
                "\x1b]133;D;1\x07",
                "界",
                "界界",
                "\u{301}",
                "é",
                "\x1b[2K",
                "\x1b[3@",
                "\x1b[2P",
                "\x1b[3X",
                "\x1b[H",
                "\x1b[3;4H",
                "\x1b[99;99H",
                "\x1b[1;1H\x1b[2;3r",
                "\x1b[r",
                "\x1b[2S",
                "\x1b[1T",
                "\x1b7",
                "\x1b8",
                "\x1b[?1049h",
                "\x1b[?1049l",
                "\x1b[?6h",
                "\x1b[?6l",
                "\x1b#8",
                "\x1bc",
            ];
            let mut seed = 0x2545_f491_4f6c_dd1du64;
            for step in 0..6000 {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                if seed % 3 == 0 {
                    let sequence = sequences[(seed >> 8) as usize % sequences.len()];
                    bulk_parser.advance(&mut bulk, sequence.as_bytes());
                    single_parser.advance(&mut single, sequence.as_bytes());
                } else {
                    let length = 1 + (seed >> 12) as usize % (3 * columns);
                    let text: String = (0..length)
                        .map(|index| {
                            let value = (seed >> (index % 40)) as u8 ^ index as u8;
                            char::from(0x20 + value % 95)
                        })
                        .collect();
                    bulk.input_ascii(&text);
                    for c in text.chars() {
                        single.input(c);
                    }
                }
                assert_same(&bulk, &single, step);
                #[cfg(feature = "compact-history")]
                {
                    bulk.grid_mut().release_history_read_cache();
                    single.grid_mut().release_history_read_cache();
                }
            }
        }
    }

    /// `input_text` must equal `input` per character across wrapping,
    /// insert mode, charsets, wide characters, hyperlinks, prompt marks,
    /// scroll regions and the alternate screen.
    #[test]
    fn non_ascii_runs_match_per_character_input() {
        for (compact, columns, lines) in [(false, 13, 5), (true, 17, 7), (false, 2, 3)] {
            let size = TermSize::new(columns, lines);
            let config = Config {
                scrolling_history: 40,
                ..Config::default()
            };
            let mut bulk = Term::new(config.clone(), &size, VoidListener);
            let mut single = Term::new(config, &size, VoidListener);
            single.reference_wide_writes = true;
            #[cfg(feature = "compact-history")]
            if compact {
                bulk.grid_mut().enable_compact_history();
                single.grid_mut().enable_compact_history();
            }
            #[cfg(not(feature = "compact-history"))]
            let _ = compact;
            let mut bulk_parser: Processor = Processor::new();
            let mut single_parser: Processor = Processor::new();
            let sequences: &[&str] = &[
                "\r\n",
                "\r",
                "\n",
                "\t",
                "\x08",
                "\x1b[4h",
                "\x1b[4l",
                "\x1b[?7l",
                "\x1b[?7h",
                "\x1b(0",
                "\x1b(B",
                "\x1b)0\x0e",
                "\x0f",
                "\x1b[31;1m",
                "\x1b[38;2;1;2;3;48;5;17m",
                "\x1b[0m",
                "\x1b[4:3m\x1b[58;5;9m",
                "\x1b]8;id=x;https://example.invalid\x07",
                "\x1b]8;;\x07",
                "\x1b]133;A\x07",
                "\x1b]133;B\x07",
                "\x1b]133;C\x07",
                "\x1b]133;D;1\x07",
                "界",
                "界界",
                "\u{301}",
                "é",
                "\x1b[2K",
                "\x1b[3@",
                "\x1b[2P",
                "\x1b[3X",
                "\x1b[H",
                "\x1b[3;4H",
                "\x1b[99;99H",
                "\x1b[1;1H\x1b[2;3r",
                "\x1b[r",
                "\x1b[2S",
                "\x1b[1T",
                "\x1b7",
                "\x1b8",
                "\x1b[?1049h",
                "\x1b[?1049l",
                "\x1b[?6h",
                "\x1b[?6l",
                "\x1b#8",
                "\x1bc",
                "\x1b]133;A\x07",
                "\x1b]133;B\x07",
                "\x1b]133;D;0\x07",
                "\x1b[?2027h",
                "\x1b[?2027l",
            ];
            // Wide characters (CJK, emoji, a fullwidth form), joiners and
            // marks that continue a cluster, and narrow characters beyond
            // ASCII.
            let pool: Vec<char> =
                "界日本語한국어Ａ😀👍🏽❤\u{FE0F}\u{200D}🧑\u{1F4BB}🇷🇴\u{301}\u{20E3}कि\u{AD}é✓—★"
                    .chars()
                    .collect();
            let mut seed = 0x2545_f491_4f6c_dd1du64;
            for step in 0..6000 {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                if seed % 3 == 0 {
                    let sequence = sequences[(seed >> 8) as usize % sequences.len()];
                    bulk_parser.advance(&mut bulk, sequence.as_bytes());
                    single_parser.advance(&mut single, sequence.as_bytes());
                } else {
                    let length = 1 + (seed >> 12) as usize % (3 * columns);
                    let text: String = (0..length)
                        .map(|index| {
                            let value = (seed >> (index % 40)) as usize ^ index;
                            // Mostly wide characters, as CJK output is, with
                            // narrow ones between so wide characters land on
                            // every column and over cells of every kind.
                            match value % 4 {
                                0 => pool[(value >> 2) % pool.len()],
                                1 => char::from(b'a' + (value >> 2) as u8 % 26),
                                _ => pool[(value >> 2) % 7],
                            }
                        })
                        .collect();
                    bulk.input_text(&text);
                    for c in text.chars() {
                        single.input(c);
                    }
                }
                assert_same(&bulk, &single, step);
                #[cfg(feature = "compact-history")]
                {
                    bulk.grid_mut().release_history_read_cache();
                    single.grid_mut().release_history_read_cache();
                }
            }
        }
    }
}

#[cfg(test)]
mod content_source_tests {
    use super::*;
    use crate::event::VoidListener;
    use crate::term::test::TermSize;
    use crate::vte::ansi::Processor;

    const C: u32 = CONTENT_CHANGED;

    fn sources(term: &mut Term<VoidListener>) -> Option<Vec<u32>> {
        let _ = term.damage();
        let sources = term.damage_content_sources().map(<[u32]>::to_vec);
        term.reset_damage();
        sources
    }

    #[test]
    fn scrolling_moves_content_sources() {
        let mut term = Term::new(Config::default(), &TermSize::new(5, 6), VoidListener);
        let mut parser: Processor = Processor::new();
        for line in 0..6 {
            parser.advance(&mut term, format!("\x1b[{};1H{line}", line + 1).as_bytes());
        }
        assert_eq!(sources(&mut term), None, "sources start unknown");

        // `input` leaves its cells to cursor damage. A linefeed at the
        // bottom scrolls them up before any cursor damage covers them.
        parser.advance(&mut term, b"ab\n");
        assert_eq!(sources(&mut term), Some(vec![1, 2, 3, 4, C, C]));

        // A region scroll moves only region lines. The cursor lines are
        // damaged by the home move.
        parser.advance(&mut term, b"\x1b[2;4r\x1b[1T");
        assert_eq!(sources(&mut term), Some(vec![C, C, 1, 2, 4, C]));

        // Insert and delete lines scroll from the cursor line.
        parser.advance(&mut term, b"\x1b[r\x1b[3;1H");
        let _ = sources(&mut term);
        parser.advance(&mut term, b"\x1b[2M");
        assert_eq!(sources(&mut term), Some(vec![0, 1, C, 5, C, C]));
        parser.advance(&mut term, b"\x1b[1L");
        assert_eq!(sources(&mut term), Some(vec![0, 1, C, C, 3, 4]));

        // Scrolling an entire region resets every region line.
        parser.advance(&mut term, b"\x1b[9S");
        assert_eq!(sources(&mut term), Some(vec![C; 6]));

        // Other full damage forgets where lines came from.
        parser.advance(&mut term, b"\x1b[2J");
        assert_eq!(sources(&mut term), None);
        assert!(sources(&mut term).is_some());
        parser.advance(&mut term, b"\x1b[?1049h");
        assert_eq!(sources(&mut term), None);
    }
}
