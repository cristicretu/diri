//! Travel between the messages a person sent to an Agent.
//!
//! Agents draw a sent message with a recognisable gutter: Claude Code a `❯`
//! on a tinted band, Codex a `›`, OpenCode an accent `┃` on a tinted panel.
//! [`Gutter`] recognises those rows and tells them apart from the composer,
//! choosers, wrapped continuations and look-alike tool output.
//!
//! Full-screen Agents keep their whole transcript in their own scrolling view
//! on the alternate screen; the terminal retains nothing above it. There
//! [`Travel`] moves that view with the same wheel input a trackpad sends (and
//! PageUp/PageDown where an Agent scrolls its transcript with them), planned
//! with each Agent's measured response ([`Notches`]) and checked against each
//! redrawn screen until the wanted message sits at the top.
//! Inline Agents leave their transcript in the terminal history, where the
//! app scrolls its own view to rows found by [`Gutter::history_starts`].

use std::ops::Range;

use diri_proto::grid::{GridCell, TermColor, TermStyle};

/// How an Agent draws the first row of a sent message.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Gutter {
    /// Claude Code: `❯` (`>` in older releases) at column 0 on a tinted band.
    /// Its composer uses the same glyph on the default background.
    Claude,
    /// Codex: `›` at column 0. The last `›` on its screen is the composer.
    Codex,
    /// OpenCode: an accent `┃` on a tinted panel. Its composer's `┃` sits on
    /// the plain background, and tool output hides the border in it.
    OpenCode,
    /// A marker at column 0 or 1, followed by a space: Copilot's `❯`,
    /// Gemini's `>`, Kimi's `✨`.
    Marker(&'static str),
    /// Agents that label turns `You:` or `User:`.
    Label,
}

/// How a full-screen Agent's transcript answers wheel notches, as
/// `diri-engine`'s `measure_burst_curve` records it: the lines one burst moves
/// after a pause. A jump plans every step with it and checks each redraw
/// against it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Notches {
    /// Lines every notch moves, for an Agent that never speeds them up.
    per_notch: Option<f32>,
    /// Otherwise (notches, lines) for each burst size a jump may send.
    curve: &'static [(u16, f32)],
    /// The longest burst a jump sends.
    most: u16,
    /// The longest burst that lifts a message into place. Claude Code has
    /// left a message blank after carrying it most of a screen in one burst.
    most_aligning: u16,
    /// The first notch after the view turns around moves nothing.
    ignores_turn: bool,
    /// PageUp and PageDown move the transcript half its height, never sped
    /// up and never typed into the composer: the fastest way through it.
    pub page_keys: bool,
    /// The least time between bursts: sooner ones are sped up by an amount
    /// no single redraw shows.
    pub interval: std::time::Duration,
    /// How long a redraw must stay unchanged before the view counts as at
    /// rest, when it has not yet shown the move predicted.
    pub settle: std::time::Duration,
}

impl Notches {
    /// Codex and OpenCode: three lines a notch, however many and however fast.
    pub const THREE_LINES: Self = Self {
        per_notch: Some(3.0),
        curve: &[],
        most: 40,
        most_aligning: 40,
        ignores_turn: false,
        page_keys: false,
        interval: std::time::Duration::ZERO,
        settle: std::time::Duration::from_millis(14),
    };

    /// Claude Code: a line a notch for short bursts, faster for longer ones,
    /// and faster still for bursts under about 50 ms apart.
    pub const CLAUDE: Self = Self {
        per_notch: None,
        curve: &[
            (1, 1.0),
            (2, 2.0),
            (3, 3.0),
            (4, 4.0),
            (5, 6.0),
            (6, 8.0),
            (7, 10.0),
            (8, 13.0),
            (10, 19.0),
            (12, 26.0),
            (14, 34.0),
            (16, 44.0),
            (20, 67.0),
            (24, 91.0),
        ],
        most: 24,
        most_aligning: 8,
        ignores_turn: true,
        page_keys: true,
        interval: std::time::Duration::from_millis(55),
        settle: std::time::Duration::from_millis(30),
    };

    /// Lines a burst moves; `turned` when it reverses the last one.
    #[must_use]
    pub fn lines(&self, notches: u16, turned: bool) -> f32 {
        let notches = if turned && self.ignores_turn {
            notches.saturating_sub(1)
        } else {
            notches
        };
        if notches == 0 {
            return 0.0;
        }
        if let Some(lines) = self.per_notch {
            return lines * f32::from(notches);
        }
        let below = self.curve.iter().rev().find(|(size, _)| *size <= notches);
        let above = self.curve.iter().find(|(size, _)| *size >= notches);
        match (below, above) {
            (Some(&(low, from)), Some(&(high, to))) if high > low => {
                from + (to - from) * f32::from(notches - low) / f32::from(high - low)
            }
            (Some(&(_, lines)), _) | (None, Some(&(_, lines))) => lines,
            (None, None) => f32::from(notches),
        }
    }

    /// The longest burst a jump may send that moves at most `lines`; at
    /// least the shortest that moves at all.
    #[must_use]
    pub fn within(&self, lines: f32, turned: bool) -> u16 {
        self.within_most(lines, turned, self.most)
    }

    fn within_most(&self, lines: f32, turned: bool, most: u16) -> u16 {
        let sizes: Vec<u16> = if self.per_notch.is_some() {
            (1..=most).collect()
        } else {
            self.curve
                .iter()
                .map(|(size, _)| *size)
                .filter(|size| *size <= most)
                .collect()
        };
        let moving = sizes
            .iter()
            .copied()
            .filter(|size| self.lines(*size, turned) > 0.0);
        let fitting = moving
            .clone()
            .filter(|size| self.lines(*size, turned) <= lines + 0.01)
            .max();
        fitting.or_else(|| moving.min()).unwrap_or(1)
    }
}

/// One sent message as drawn on a screen.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MessageRows {
    /// First row, including a panel's padding row.
    pub start: usize,
    /// One past the message's last row.
    pub end: usize,
    /// The start of the message's text: the same message again after the
    /// view has moved, or in a pinned header.
    pub text: String,
}

/// What a full-screen Agent's screen shows.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ScreenMessages {
    /// Rows that scroll with the transcript: below any pinned header, above
    /// the composer and footer.
    pub region: Range<usize>,
    pub messages: Vec<MessageRows>,
    /// The message pinned above the transcript: the turn the top row
    /// belongs to.
    pub pinned: Option<String>,
}

impl Gutter {
    /// The Agents whose sent messages Diri can find. Plain shells and notes
    /// have none; they use prompt marks instead.
    #[must_use]
    pub fn for_agent(agent: &str) -> Option<Self> {
        Some(match agent {
            "shell" | "note" => return None,
            "claude-code" | "claude" => Self::Claude,
            "codex" => Self::Codex,
            "opencode" => Self::OpenCode,
            "copilot" => Self::Marker("❯"),
            "gemini" => Self::Marker(">"),
            "kimi" => Self::Marker("✨"),
            _ => Self::Label,
        })
    }

    /// Rows a full-screen Agent pins above its transcript: Claude Code and
    /// Codex overlay the current turn's message on row 0 while scrolled, and
    /// OpenCode draws its session title there. A message brought into view
    /// lands just below them.
    #[must_use]
    pub const fn header_rows(self) -> usize {
        match self {
            Self::Claude | Self::Codex | Self::OpenCode => 1,
            Self::Marker(_) | Self::Label => 0,
        }
    }

    /// How this Agent's transcript answers wheel notches.
    #[must_use]
    pub const fn notches(self) -> Notches {
        match self {
            Self::Claude => Notches::CLAUDE,
            _ => Notches::THREE_LINES,
        }
    }

    /// Whether `row` starts a sent message. `previous` and `next` are its
    /// neighbours when known: a soft-wrapped previous row makes this one its
    /// continuation, and a row below a pinned header has no known previous.
    #[must_use]
    pub fn starts_message(
        self,
        previous: Option<&[GridCell]>,
        row: &[GridCell],
        next: Option<&[GridCell]>,
    ) -> bool {
        if previous.is_some_and(soft_wraps) {
            return false;
        }
        match self {
            Self::Claude => {
                matches!(glyph(row, 0), Some('❯' | '>'))
                    && glyph(row, 1).is_none_or(|ch| ch == ' ')
                    && row.first().is_some_and(|cell| !is_default_bg(cell.bg))
                    && has_text_after(row, 1)
            }
            Self::Codex => {
                glyph(row, 0) == Some('›') && glyph(row, 1) == Some(' ') && has_text_after(row, 1)
            }
            // A panel opens with an empty padding row above its text. The
            // bottom padding of a panel scrolled under the header is not a
            // start, and neither is text whose top padding is out of sight.
            Self::OpenCode => {
                let Some(col) = panel_col(row) else {
                    return false;
                };
                previous.is_none_or(|above| panel_col(above) != Some(col))
                    && panel_text(row, col).is_empty()
                    && next.is_some_and(|below| {
                        panel_col(below) == Some(col) && !panel_text(below, col).is_empty()
                    })
            }
            Self::Marker(marker) => marker_body(row, marker).is_some_and(|body| !body.is_empty()),
            Self::Label => ["You:", "User:"]
                .iter()
                .any(|label| marker_body(row, label).is_some_and(|body| !body.is_empty())),
        }
    }

    /// Whether `row` continues the message that starts at `start`.
    fn continues(self, start: &[GridCell], previous: &[GridCell], row: &[GridCell]) -> bool {
        if soft_wraps(previous) {
            return true;
        }
        match self {
            // A multi-line message keeps the band on every row.
            Self::Claude => {
                row.first()
                    .zip(start.first())
                    .is_some_and(|(cell, first)| cell.bg == first.bg)
                    && !self.starts_message(Some(previous), row, None)
            }
            // Further lines are indented under the text, until a blank row.
            Self::Codex | Self::Marker(_) | Self::Label => {
                let text = row_text(row);
                !text.trim().is_empty()
                    && text.starts_with("  ")
                    && !self.starts_message(Some(previous), row, None)
            }
            Self::OpenCode => panel_col(row).is_some() && panel_col(row) == panel_col(start),
        }
    }

    /// Finds the sent messages on a full-screen Agent's screen.
    #[must_use]
    pub fn screen(self, rows: &[Vec<GridCell>]) -> ScreenMessages {
        let end = self.composer_top(rows).unwrap_or(rows.len());
        let region = self.header_rows().min(end)..end;
        let mut messages = Vec::new();
        let mut row = region.start;
        while row < region.end {
            let previous = (row > region.start).then(|| rows[row - 1].as_slice());
            let next = rows.get(row + 1).map(Vec::as_slice);
            if !self.starts_message(previous, &rows[row], next) {
                row += 1;
                continue;
            }
            let mut last = row + 1;
            while last < region.end && self.continues(&rows[row], &rows[last - 1], &rows[last]) {
                last += 1;
            }
            messages.push(MessageRows {
                start: row,
                end: last,
                text: self.message_text(&rows[row..last]),
            });
            row = last;
        }
        let choices = self.chooser_rows(rows, &region);
        messages.retain(|message| !choices.contains(&message.start));
        let pinned = (self.header_rows() > 0)
            .then(|| rows.first())
            .flatten()
            .filter(|row| self.starts_message(None, row, None))
            .map(|row| self.message_text(std::slice::from_ref(row)));
        ScreenMessages {
            region,
            messages,
            pinned,
        }
    }

    /// The first row of the composer and footer, which never scroll.
    fn composer_top(self, rows: &[Vec<GridCell>]) -> Option<usize> {
        match self {
            // The composer sits between two full-width rules.
            Self::Claude => {
                let rule = |row: &Vec<GridCell>| {
                    let text = row_text(row);
                    text.chars().filter(|ch| *ch == '─').count() * 2 > row.len()
                };
                let lower = rows.iter().rposition(rule)?;
                rows[..lower]
                    .iter()
                    .rposition(rule)
                    .filter(|upper| lower - upper <= 8)
            }
            Self::Codex => rows.iter().rposition(|row| glyph(row, 0) == Some('›')),
            // The composer is the last panel, closed by a `╹` row.
            Self::OpenCode => {
                let bottom = rows.iter().rposition(|row| {
                    let text = row_text(row);
                    text.trim_start().starts_with('╹')
                })?;
                let col = row_text(&rows[bottom]).find('╹');
                let mut top = bottom;
                while top > 0 && {
                    let text = row_text(&rows[top - 1]);
                    text.find('┃').is_some() && text.find('┃') == col
                } {
                    top -= 1;
                }
                Some(top)
            }
            Self::Marker(_) | Self::Label => None,
        }
    }

    /// A numbered chooser ("› 1. Update now", options, then "Press enter to
    /// continue") is not a message, though a message may start with "1. ".
    fn chooser_rows(self, rows: &[Vec<GridCell>], region: &Range<usize>) -> Vec<usize> {
        let mut choices = Vec::new();
        for row in region.clone() {
            let text = row_text(&rows[row]);
            let Some(body) =
                text.get(text.char_indices().nth(2).map_or(text.len(), |(at, _)| at)..)
            else {
                continue;
            };
            if !numbered(body.trim_start()) {
                continue;
            }
            let option = (row + 1..(row + 5).min(rows.len()))
                .find(|&below| numbered(row_text(&rows[below]).trim_start()));
            let footer = option.and_then(|option| {
                (option + 1..(row + 16).min(rows.len())).find(|&below| {
                    let text = row_text(&rows[below]).to_lowercase();
                    ["enter to", "esc to", "press enter"]
                        .iter()
                        .any(|hint| text.contains(hint))
                })
            });
            if footer.is_some() {
                choices.push(row);
            }
        }
        choices
    }

    /// Message starts among history rows read in order, skipping what can
    /// only be a composer draft: the last start on the live grid with no
    /// reply below it. `first_row` is the absolute row of `rows[0]`, and
    /// `live_start` the first row of the live grid.
    #[must_use]
    pub fn history_starts(
        self,
        above: Option<&[GridCell]>,
        rows: &[Vec<GridCell>],
        first_row: i64,
        live_start: i64,
    ) -> Vec<i64> {
        let mut starts = Vec::new();
        let mut reply_after_last = false;
        for (index, row) in rows.iter().enumerate() {
            let previous = index
                .checked_sub(1)
                .map(|above| rows[above].as_slice())
                .or(above);
            if self.starts_message(previous, row, rows.get(index + 1).map(Vec::as_slice)) {
                starts.push(first_row + index as i64);
                reply_after_last = false;
            } else if is_reply(row) {
                reply_after_last = true;
            }
        }
        let region = 0..rows.len();
        let choices = self.chooser_rows(rows, &region);
        starts.retain(|row| !choices.contains(&((row - first_row) as usize)));
        if let Some(&last) = starts.last()
            && last >= live_start
            && !reply_after_last
            && matches!(self, Self::Codex | Self::Marker(_) | Self::Label)
        {
            starts.pop();
        }
        starts
    }

    /// How many rows the message starting at `start` occupies, at most
    /// `limit`.
    #[must_use]
    pub fn message_len(self, rows: &[Vec<GridCell>], start: usize, limit: usize) -> usize {
        let mut end = start + 1;
        while end < rows.len()
            && end - start < limit
            && self.continues(&rows[start], &rows[end - 1], &rows[end])
        {
            end += 1;
        }
        end - start
    }
}

/// A wheel step toward an Agent, or the end of a jump.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TravelStep {
    /// Send `ticks` wheel notches, toward older output when `up`, then show
    /// [`Travel::observe`] the redrawn screen.
    Scroll { up: bool, ticks: u16 },
    /// Send PageUp (`up`) or PageDown, then show the redrawn screen.
    Page { up: bool },
    /// Look again without sending anything: the last notches may not have
    /// been drawn yet.
    Wait,
    /// The message occupies these screen rows.
    Arrived { start: usize, end: usize },
    /// No sent message further this way: the view reached the end of the
    /// transcript.
    Exhausted,
    /// The screen stopped looking like a transcript this gutter recognises.
    Lost,
}

/// One jump to the previous or next sent message in a full-screen Agent.
///
/// Each step sends the burst of notches [`Notches`] predicts covers most of a
/// screen, then compares the redrawn screen with the one before to measure
/// how far the transcript really moved. Every row passes through the screen
/// on the way, so no message is skipped, and the reading position follows the
/// content however the Agent moved. Once the message is on screen, one burst
/// lifts it to just below the pinned header.
#[derive(Debug)]
pub struct Travel {
    gutter: Gutter,
    notches: Notches,
    next: bool,
    landing: usize,
    /// Where reading started, in current screen rows; it leaves the screen as
    /// the view moves.
    reading: i64,
    keys: Vec<Option<u64>>,
    region: Range<usize>,
    target: Option<Target>,
    sent: Option<(bool, u16)>,
    /// The last step was a page key rather than notches.
    paged: bool,
    /// Page keys are in use: they move this Agent's transcript.
    pages: bool,
    /// Lines a page key moves, once one has been measured.
    page_lines: Option<f32>,
    /// Which way the last notches went: page keys leave it be.
    notch_up: Option<bool>,
    /// The last burst reversed the notches before it.
    turned: bool,
    /// How the Agent's moves compare with the model, should it have changed.
    scale: f32,
    /// The last move was exactly the one predicted: the redraw is complete.
    exact: bool,
    /// The pinned header on the screen before this one.
    pinned: Option<String>,
    /// Messages drawn in the transcript at some point during this jump.
    seen: Vec<String>,
    /// A message that passed under the header without being drawn, being
    /// brought back into view.
    unseen: Option<String>,
    unseen_steps: u8,
    /// Lines the view may move back up looking for it: as far as the move
    /// that carried it past, and a little more.
    unseen_budget: i64,
    /// The last move, in lines.
    last_move: i64,
    /// Whether the view has moved at all.
    moved_any: bool,
    stalled: bool,
    /// The last notches turned the view around; an Agent may have ignored
    /// the first of them.
    may_retry: bool,
    steps: u16,
}

#[derive(Clone, Debug)]
struct Target {
    row: i64,
    /// Its text: a row's other cells change under overlays such as a
    /// "Jump to bottom" hint drawn over it.
    text: String,
    /// In place after a move: one more look must find it unmoved.
    confirming: bool,
    /// Steps spent bringing it back after aligning hid it under the header.
    recovering: u8,
    aligning_steps: u8,
}

impl Target {
    fn new(message: &MessageRows) -> Self {
        Self {
            row: message.start as i64,
            text: message.text.clone(),
            confirming: false,
            recovering: 0,
            aligning_steps: 0,
        }
    }
}

const MAX_STEPS: u16 = 400;

impl Travel {
    /// Starts a jump from the message last arrived at, when it is still on
    /// screen at `from`, or else from the top of the transcript.
    pub fn begin(
        gutter: Gutter,
        next: bool,
        rows: &[Vec<GridCell>],
        from: Option<usize>,
    ) -> (Self, TravelStep) {
        Self::begin_with(gutter, gutter.notches(), next, rows, from)
    }

    /// [`Self::begin`] for an Agent that answers notches as `notches` says.
    fn begin_with(
        gutter: Gutter,
        notches: Notches,
        next: bool,
        rows: &[Vec<GridCell>],
        from: Option<usize>,
    ) -> (Self, TravelStep) {
        let screen = gutter.screen(rows);
        let landing = screen.region.start;
        let mut travel = Self {
            gutter,
            notches,
            next,
            landing,
            reading: from.unwrap_or(landing) as i64,
            keys: rows.iter().map(|row| row_key(row)).collect(),
            region: screen.region.clone(),
            target: None,
            sent: None,
            paged: false,
            pages: notches.page_keys,
            page_lines: None,
            notch_up: None,
            turned: false,
            scale: 1.0,
            exact: false,
            pinned: screen.pinned.clone(),
            seen: screen
                .messages
                .iter()
                .map(|message| message.text.clone())
                .collect(),
            unseen: None,
            unseen_steps: 0,
            unseen_budget: 0,
            last_move: 0,
            moved_any: false,
            stalled: false,
            may_retry: false,
            steps: 0,
        };
        if screen.region.is_empty() {
            return (travel, TravelStep::Lost);
        }
        let step = travel.decide(&screen, None);
        (travel, step)
    }

    /// How the Agent answers notches: the pacing and settling to wait for.
    #[must_use]
    pub const fn notches(&self) -> Notches {
        self.notches
    }

    /// Whether the jump moved the view, for telling a person who reached
    /// the end of the transcript where the view went.
    #[must_use]
    pub const fn moved(&self) -> bool {
        self.moved_any
    }

    /// The row wheel notches point at: inside the transcript, where an Agent
    /// scrolls it rather than its composer.
    #[must_use]
    pub fn pointer_row(&self) -> usize {
        (self.region.start + self.region.end) / 2
    }

    /// Whether `rows` already show the whole move the last burst predicts,
    /// so there is nothing more to wait for.
    #[must_use]
    pub fn shows_predicted_move(&self, rows: &[Vec<GridCell>]) -> bool {
        let expected = self.expected();
        if expected == 0 {
            return false;
        }
        let keys: Vec<_> = rows.iter().map(|row| row_key(row)).collect();
        estimate_shift(
            &self.keys,
            &keys,
            &self.region,
            expected,
            expected..=expected,
        ) == Some(expected)
    }

    /// The redrawn screen after the last [`TravelStep::Scroll`] or
    /// [`TravelStep::Wait`].
    pub fn observe(&mut self, rows: &[Vec<GridCell>]) -> TravelStep {
        self.steps += 1;
        if self.steps > MAX_STEPS {
            return TravelStep::Lost;
        }
        let screen = self.gutter.screen(rows);
        if screen.region.is_empty() {
            return TravelStep::Lost;
        }
        let keys: Vec<_> = rows.iter().map(|row| row_key(row)).collect();
        self.region = screen.region.clone();
        let expected = self.expected();
        let measured = self.measure(&keys, &screen.region);
        self.keys = keys;
        let moved = measured.map_or(expected, |(lines, _)| lines);
        self.exact = measured.is_some_and(|(lines, _)| lines == expected);
        let confirming = self.target.as_ref().is_some_and(|target| target.confirming);
        if moved == 0 && self.sent.is_some() && !confirming {
            // A spinner may redraw before the notches are drawn: look once
            // more before taking an unmoved view for the end of the transcript.
            if !self.stalled {
                self.stalled = true;
                return TravelStep::Wait;
            }
        }
        self.stalled = false;
        if self.paged
            && let Some((lines, true)) = measured
            && lines != 0
        {
            self.page_lines = Some(lines.unsigned_abs() as f32);
        } else if let (Some((lines, true)), Some((_, ticks))) = (measured, self.sent)
            && lines != 0
            && !self.exact
        {
            // The Agent no longer moves as measured: follow what it does.
            let model = self.notches.lines(ticks, self.turned);
            if model > 0.0 {
                let ratio = lines.unsigned_abs() as f32 / model;
                self.scale = ((self.scale + ratio) / 2.0).clamp(0.5, 2.0);
            }
        }
        self.reading += moved;
        self.last_move = moved;
        self.moved_any |= moved != 0;
        if let Some(target) = &mut self.target {
            target.row += moved;
        }
        let pinned = std::mem::replace(&mut self.pinned, screen.pinned.clone());
        if let Some(step) = self.passed_unseen(&screen, pinned) {
            return step;
        }
        for message in &screen.messages {
            if !self.seen.contains(&message.text) {
                self.seen.push(message.text.clone());
            }
        }
        self.decide(&screen, Some(moved))
    }

    /// How far the last burst should move content down.
    fn expected(&self) -> i64 {
        self.sent.map_or(0, |(up, ticks)| {
            let lines = if self.paged {
                self.page_estimate().round() as i64
            } else {
                (self.notches.lines(ticks, self.turned) * self.scale).round() as i64
            };
            if up { lines } else { -lines }
        })
    }

    /// Lines a page key moves: as measured, or half the transcript area.
    fn page_estimate(&self) -> f32 {
        self.page_lines
            .unwrap_or_else(|| (self.region.len() as f32 / 2.0).floor().max(1.0))
    }

    /// How far content moved since the last screen, and whether that was
    /// close to the move predicted. Agents repeat whole lines (every reply
    /// may end alike, numbered lists line up), and a screen of them lines up
    /// at many offsets; moves near the prediction decide first. Only when
    /// none lines up is any move the notches' way considered: the view
    /// reaching the end of the transcript part way.
    fn measure(&self, keys: &[Option<u64>], region: &Range<usize>) -> Option<(i64, bool)> {
        let (up, _) = self.sent?;
        let expected = self.expected().abs();
        let reach = region.len() as i64;
        let near = ((expected as f32 * 0.6).floor() as i64 - 1).max(1)
            ..=((expected as f32 * 1.5).ceil() as i64 + 2).min(reach);
        let signed = |range: std::ops::RangeInclusive<i64>| {
            if up {
                range
            } else {
                -*range.end()..=-*range.start()
            }
        };
        if !near.is_empty()
            && let Some(lines) = estimate_shift(
                &self.keys,
                keys,
                region,
                self.expected(),
                signed(near.clone()),
            )
        {
            return Some((lines, true));
        }
        estimate_shift(&self.keys, keys, region, self.expected(), signed(0..=reach))
            .map(|lines| (lines, false))
    }

    /// Claude Code may leave a message's rows undrawn while its view moves
    /// down, until the view moves back up. The pinned header still names
    /// the turn under the top row: a turn never seen drawn means its message
    /// went by. Step back up until it shows, and take it as the target.
    fn passed_unseen(
        &mut self,
        screen: &ScreenMessages,
        pinned: Option<String>,
    ) -> Option<TravelStep> {
        if !self.next || self.target.is_some() {
            return None;
        }
        if let Some(text) = self.unseen.clone() {
            if let Some(message) = screen
                .messages
                .iter()
                .filter(|message| message.start >= self.landing)
                .find(|message| same_message(&message.text, &text))
            {
                self.unseen = None;
                self.target = Some(Target::new(message));
                return None;
            }
            self.unseen_budget -= self.last_move.max(0);
            if self.unseen_budget <= 0 || self.unseen_steps >= 16 {
                return Some(TravelStep::Lost);
            }
            return Some(self.step_back(screen));
        }
        let now = screen.pinned.as_ref()?;
        let changed = pinned.is_none_or(|before| !same_message(&before, now));
        let drawn = self.seen.iter().any(|seen| same_message(seen, now));
        if changed && !drawn && self.unseen_steps == 0 {
            self.unseen = Some(now.clone());
            self.unseen_budget = self.last_move.abs() + screen.region.len() as i64 / 2;
            return Some(self.step_back(screen));
        }
        None
    }

    /// Back up by a quarter of the screen, so the message, just above it,
    /// comes into view without being carried past again.
    fn step_back(&mut self, screen: &ScreenMessages) -> TravelStep {
        self.unseen_steps += 1;
        if self.pages {
            return self.page(true);
        }
        let lines = (screen.region.len() as f32 / 4.0).max(2.0) / self.scale;
        let turned = self.notch_up == Some(false);
        self.send(true, self.notches.within(lines, turned))
    }

    fn decide(&mut self, screen: &ScreenMessages, moved: Option<i64>) -> TravelStep {
        if self.target.is_none() {
            let visible = screen
                .messages
                .iter()
                .filter(|message| message.start >= self.landing);
            let found = if self.next {
                visible
                    .filter(|message| (message.start as i64) > self.reading)
                    .min_by_key(|message| message.start)
            } else {
                visible
                    .filter(|message| (message.start as i64) < self.reading)
                    .max_by_key(|message| message.start)
            };
            if let Some(message) = found {
                self.target = Some(Target::new(message));
            } else if moved == Some(0) && self.paged && self.page_lines.is_none() {
                // A page key that never moved anything may be bound to
                // something else here: go on with notches.
                self.pages = false;
                return self.seek(screen);
            } else if moved == Some(0) {
                return self.unmoved(TravelStep::Exhausted);
            } else {
                return self.seek(screen);
            }
        }
        self.align(screen, moved)
    }

    /// An unmoved view right after the view turned around may be an Agent
    /// ignoring the first notch the other way: send the notches once more
    /// before taking it for the end of the transcript.
    fn unmoved(&mut self, otherwise: TravelStep) -> TravelStep {
        match self.sent {
            Some((up, ticks)) if self.may_retry && !self.paged => {
                self.may_retry = false;
                self.turned = false;
                TravelStep::Scroll { up, ticks }
            }
            _ => otherwise,
        }
    }

    fn send(&mut self, up: bool, ticks: u16) -> TravelStep {
        self.turned = self.notch_up.is_some_and(|was_up| was_up != up);
        self.may_retry = self.notch_up.is_none_or(|was_up| was_up != up);
        self.notch_up = Some(up);
        self.paged = false;
        self.sent = Some((up, ticks));
        TravelStep::Scroll { up, ticks }
    }

    fn page(&mut self, up: bool) -> TravelStep {
        self.may_retry = false;
        self.paged = true;
        self.sent = Some((up, 0));
        TravelStep::Page { up }
    }

    fn seek(&mut self, screen: &ScreenMessages) -> TravelStep {
        // Most of a screen per step: every row is still seen on the way, and
        // enough of each screen stays on the next to tell a real move from
        // repeated lines that happen to line up.
        let up = !self.next;
        if self.pages {
            return self.page(up);
        }
        let rows = screen.region.len() as f32;
        let span = (rows * 0.75).min(rows - 6.0).max(2.0) / self.scale;
        let turned = self.notch_up.is_some_and(|was_up| was_up != up);
        self.send(up, self.notches.within(span, turned))
    }

    fn align(&mut self, screen: &ScreenMessages, moved: Option<i64>) -> TravelStep {
        let target = self.target.clone().expect("aligning a target");
        // Where it should be now, or the nearest copy of its text: a move
        // too far to measure still leaves the message itself recognisable.
        let found = screen
            .messages
            .iter()
            .filter(|message| same_message(&message.text, &target.text))
            .min_by_key(|message| (message.start as i64 - target.row).abs());
        let Some(message) = found else {
            // Aligning only ever lifts the message, so one that left the
            // screen went up under the header: bring it back down.
            if target.recovering >= 4 {
                return TravelStep::Lost;
            }
            self.target = Some(Target {
                recovering: target.recovering + 1,
                ..target
            });
            let hidden = (self.landing as i64 - target.row).max(1) as f32 / self.scale;
            let turned = self.notch_up == Some(false);
            return self.send(true, self.notches.within(hidden, turned));
        };
        let arrived = TravelStep::Arrived {
            start: message.start,
            end: message.end,
        };
        let distance = message.start as i64 - self.landing as i64;
        self.target = Some(Target {
            row: message.start as i64,
            aligning_steps: target.aligning_steps.saturating_add(1),
            ..target
        });
        // Within a notch of the top reads as the top: the finest move there
        // is would carry it under the header.
        let finest = self.notches.lines(1, false).max(2.0);
        let placed = distance <= 0
            || target.aligning_steps >= 8
            || (distance as f32) < finest
            || target.recovering > 0;
        if placed || (moved == Some(0) && self.sent.is_some_and(|(up, _)| !up)) {
            // An Agent may still be easing a move in that did not land as
            // predicted: arrive once a look finds the view at rest.
            if moved.is_some_and(|lines| lines != 0) && !self.exact && !target.confirming {
                self.target = Some(Target {
                    confirming: true,
                    ..self.target.clone().expect("aligning a target")
                });
                return TravelStep::Wait;
            }
            if placed {
                return arrived;
            }
            // The transcript ends below: this is as high as it goes.
            return self.unmoved(arrived);
        }
        self.target = Some(Target {
            confirming: false,
            ..self.target.clone().expect("aligning a target")
        });
        if self.pages && distance as f32 >= self.page_estimate() {
            return self.page(false);
        }
        let turned = self.notch_up == Some(true);
        let ticks = self.notches.within_most(
            distance as f32 / self.scale,
            turned,
            self.notches.most_aligning,
        );
        self.send(false, ticks)
    }
}

/// How far content moved down between two screens, measured on the rows
/// that scroll, among the moves `range` allows. A move scores the text rows
/// it lines up, less those it would put over different text; ties prefer the
/// move the notches were expected to make. Agents repeat whole lines (every
/// reply may end alike), so a move against the notches is never considered.
fn estimate_shift(
    before: &[Option<u64>],
    after: &[Option<u64>],
    region: &Range<usize>,
    expected: i64,
    range: std::ops::RangeInclusive<i64>,
) -> Option<i64> {
    let mut best: Option<(i64, i64, usize)> = None;
    for shift in range {
        let (mut matches, mut conflicts) = (0_usize, 0_usize);
        for row in region.clone() {
            let moved = row as i64 + shift;
            if moved < region.start as i64 || moved >= region.end as i64 {
                continue;
            }
            match (
                before.get(row).copied().flatten(),
                after.get(moved as usize).copied().flatten(),
            ) {
                (Some(old), Some(new)) if old == new => matches += 1,
                (Some(_), Some(_)) => conflicts += 1,
                _ => {}
            }
        }
        let score = matches as i64 * 2 - conflicts as i64;
        let better = best.is_none_or(|(best_shift, best_score, _)| {
            score > best_score
                || (score == best_score && (shift - expected).abs() < (best_shift - expected).abs())
        });
        if matches > 0 && better {
            best = Some((shift, score, matches));
        }
    }
    best.filter(|(_, score, matches)| *matches >= 2 && *score > 0)
        .map(|(shift, ..)| shift)
}

fn row_key(row: &[GridCell]) -> Option<u64> {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    let mut blank = true;
    for cell in row {
        let scalar = if cell.scalar == 0 { 32 } else { cell.scalar };
        blank &= scalar == 32;
        scalar.hash(&mut hasher);
    }
    (!blank).then(|| hasher.finish())
}

impl Gutter {
    /// The first text of a message's rows, without its gutter or a
    /// truncation mark, as a pinned header and the message itself both
    /// begin. An OpenCode panel's text ends where its tint does, before any
    /// sidebar drawn beside the transcript.
    fn message_text(self, rows: &[Vec<GridCell>]) -> String {
        rows.iter()
            .map(|row| {
                let text = match (self, panel_col(row)) {
                    (Self::OpenCode, Some(col)) => panel_text(row, col),
                    _ => row_text(row),
                };
                text.trim_start()
                    .trim_start_matches(['❯', '>', '›', '┃', '✨'])
                    .trim()
                    .trim_end_matches('…')
                    .trim_end()
                    .to_owned()
            })
            .find(|text| !text.is_empty())
            .unwrap_or_default()
    }
}

/// The text inside an OpenCode panel: the cells after its border that share
/// the border's tint.
fn panel_text(row: &[GridCell], col: usize) -> String {
    let tint = row[col].bg;
    row_text(
        &row[col + 1..]
            .iter()
            .take_while(|cell| cell.bg == tint)
            .copied()
            .collect::<Vec<_>>(),
    )
    .trim()
    .to_owned()
}

/// Whether two message texts are the same message, one possibly cut short.
fn same_message(a: &str, b: &str) -> bool {
    let shorter = a.chars().count().min(b.chars().count());
    shorter >= 4 && (a.starts_with(b) || b.starts_with(a))
}

fn soft_wraps(row: &[GridCell]) -> bool {
    row.last()
        .is_some_and(|cell| cell.style.contains(TermStyle::SOFT_WRAP))
}

fn is_default_bg(color: TermColor) -> bool {
    matches!(color, TermColor::Default | TermColor::DefaultInverted)
}

fn glyph(row: &[GridCell], col: usize) -> Option<char> {
    row.get(col).and_then(|cell| match cell.scalar {
        0 => Some(' '),
        scalar => char::from_u32(scalar),
    })
}

fn has_text_after(row: &[GridCell], col: usize) -> bool {
    row.iter()
        .skip(col + 1)
        .any(|cell| !matches!(cell.scalar, 0 | 32))
}

fn row_text(row: &[GridCell]) -> String {
    row.iter()
        .filter(|cell| !cell.style.contains(TermStyle::WIDE_SPACER) && cell.scalar != 0)
        .map(|cell| char::from_u32(cell.scalar).unwrap_or(' '))
        .collect()
}

/// The column of an OpenCode message panel's border: an accent `┃` on a
/// background tinted away from the row's own.
fn panel_col(row: &[GridCell]) -> Option<usize> {
    let base = row.first()?.bg;
    let col = row.iter().position(|cell| !matches!(cell.scalar, 0 | 32))?;
    let border = row[col];
    (col <= 8
        && border.scalar == u32::from('┃')
        && border.bg != base
        && !is_default_bg(border.bg)
        && border.fg != base
        && border.fg != border.bg)
        .then_some(col)
}

/// The text after `marker`, which must sit in column 0 or 1: deeper
/// indentation is a quote, code or a selected option.
fn marker_body(row: &[GridCell], marker: &str) -> Option<String> {
    let text = row_text(row);
    let trimmed = text.trim_start_matches(' ');
    if text.len() - trimmed.len() > 1 {
        return None;
    }
    let body = trimmed.strip_prefix(marker)?;
    body.starts_with(char::is_whitespace)
        .then(|| body.trim().to_owned())
}

/// An assistant bullet, or a permission prompt that answers a message.
fn is_reply(row: &[GridCell]) -> bool {
    let text = row_text(row);
    let text = text.trim_start();
    text.starts_with(['•', '●', '⏺', '✦'])
        || (text.starts_with(['│', '┃'])
            && [
                "Do you want to",
                "Allow execution",
                "Approve tool",
                "Allow this",
            ]
            .iter()
            .any(|prompt| text.contains(prompt)))
}

fn numbered(text: &str) -> bool {
    let rest = text.trim_start_matches(|ch: char| ch.is_ascii_digit());
    rest.len() != text.len() && rest.starts_with(". ")
}

#[cfg(test)]
mod tests;
