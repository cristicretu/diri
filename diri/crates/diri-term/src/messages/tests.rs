use super::*;
use diri_proto::grid::GridRowCodec;
use diri_terminal_state::HeadlessScreen;

macro_rules! fixture {
    ($name:literal) => {
        include_str!(concat!(
            "../../tests/fixtures/agent_messages/",
            $name,
            ".ansi"
        ))
    };
}

/// Every row a recorded screen leaves in a 100×30 terminal, history first.
fn replay(ansi: &str) -> Vec<Vec<GridCell>> {
    let mut screen = HeadlessScreen::new(100, 30);
    screen.feed(ansi.as_bytes());
    let total = screen.scrollback_cells(0, 0).total_rows;
    let cells = screen.scrollback_cells(0, total);
    GridRowCodec::decode_rows(&cells.payload, cells.row_count as usize).unwrap()
}

fn visible(ansi: &str) -> Vec<Vec<GridCell>> {
    let mut rows = replay(ansi);
    rows.split_off(rows.len() - 30)
}

fn row(text: &str) -> Vec<GridCell> {
    let mut cells: Vec<_> = text
        .chars()
        .map(|ch| GridCell {
            scalar: ch as u32,
            ..GridCell::BLANK
        })
        .collect();
    cells.resize(100, GridCell::BLANK);
    cells
}

fn rows(text: &str) -> Vec<Vec<GridCell>> {
    text.lines().map(row).collect()
}

fn starts(screen: &ScreenMessages) -> Vec<usize> {
    screen
        .messages
        .iter()
        .map(|message| message.start)
        .collect()
}

#[test]
fn recorded_full_screen_agents_show_only_sent_messages() {
    for (gutter, name, screen, expected) in [
        // Live: the replies pushed every message off screen; the composer
        // holds an unsent draft.
        (
            Gutter::Claude,
            "claude live",
            fixture!("claude-code-live"),
            vec![],
        ),
        // Row 0 is the pinned copy of the current turn's message.
        (
            Gutter::Claude,
            "claude up2",
            fixture!("claude-code-up2"),
            vec![],
        ),
        (
            Gutter::Claude,
            "claude up7",
            fixture!("claude-code-up7"),
            vec![5],
        ),
        (
            Gutter::Claude,
            "claude up16",
            fixture!("claude-code-up16"),
            vec![3, 7, 14],
        ),
        (
            Gutter::Claude,
            "claude top",
            fixture!("claude-code-top"),
            vec![11],
        ),
        (Gutter::Codex, "codex live", fixture!("codex-live"), vec![]),
        (Gutter::Codex, "codex up2", fixture!("codex-up2"), vec![1]),
        (
            Gutter::Codex,
            "codex up7",
            fixture!("codex-up7"),
            vec![4, 17],
        ),
        (
            Gutter::Codex,
            "codex up16",
            fixture!("codex-up16"),
            vec![20],
        ),
        (Gutter::Codex, "codex top", fixture!("codex-top"), vec![7]),
        (
            Gutter::OpenCode,
            "opencode live",
            fixture!("opencode-live"),
            vec![],
        ),
        // Only the bottom padding of the latest message is below the title.
        (
            Gutter::OpenCode,
            "opencode up2",
            fixture!("opencode-up2"),
            vec![],
        ),
        (
            Gutter::OpenCode,
            "opencode up7",
            fixture!("opencode-up7"),
            vec![1, 14],
        ),
        // The tool output panel below the message hides its border.
        (
            Gutter::OpenCode,
            "opencode up16",
            fixture!("opencode-up16"),
            vec![14],
        ),
        (
            Gutter::OpenCode,
            "opencode top",
            fixture!("opencode-top"),
            vec![2],
        ),
    ] {
        let screen = gutter.screen(&visible(screen));
        assert_eq!(starts(&screen), expected, "{name}");
        assert_eq!(screen.region.start, 1, "{name}: row 0 is pinned");
    }
}

#[test]
fn recorded_messages_span_their_wrapped_rows_and_panels() {
    let claude = Gutter::Claude.screen(&visible(fixture!("claude-code-up16")));
    assert_eq!(claude.messages[1].start..claude.messages[1].end, 7..9);
    assert_eq!(claude.messages[0].start..claude.messages[0].end, 3..4);
    let codex = Gutter::Codex.screen(&visible(fixture!("codex-up7")));
    assert_eq!(codex.messages[0].start..codex.messages[0].end, 4..6);
    let opencode = Gutter::OpenCode.screen(&visible(fixture!("opencode-up7")));
    assert_eq!(opencode.messages[0].start..opencode.messages[0].end, 1..5);
    assert_eq!(opencode.messages[1].start..opencode.messages[1].end, 14..17);
}

#[test]
fn composers_end_the_scrolling_region() {
    for (gutter, screen, end) in [
        (Gutter::Claude, fixture!("claude-code-live"), 26),
        (Gutter::Codex, fixture!("codex-up16"), 26),
        (Gutter::OpenCode, fixture!("opencode-top"), 23),
    ] {
        assert_eq!(
            gutter.screen(&visible(screen)).region.end,
            end,
            "{gutter:?}"
        );
    }
}

#[test]
fn inline_codex_history_skips_the_draft() {
    let history = replay(fixture!("codex-inline-live"));
    let live_start = history.len() as i64 - 30;
    let found = Gutter::Codex.history_starts(None, &history, 0, live_start);
    let texts: Vec<_> = found
        .iter()
        .map(|&start| row_text(&history[start as usize]))
        .collect();
    assert_eq!(found.len(), 4, "{texts:?}");
    assert!(texts[0].starts_with("› First question"));
    assert!(texts[1].starts_with("› RUNCMD"));
    assert!(texts[2].starts_with("› Third question"));
    assert!(texts[3].starts_with("› 1. Fix the search"));
    // Paged reads see the same starts.
    let mut paged = Vec::new();
    for first in (0..history.len()).step_by(37) {
        let end = (first + 37).min(history.len());
        let above = first.checked_sub(1).map(|row| history[row].as_slice());
        paged.extend(Gutter::Codex.history_starts(
            above,
            &history[first..end],
            first as i64,
            live_start,
        ));
    }
    assert_eq!(paged, found);
}

#[test]
fn gutters_tell_messages_from_replies_quotes_and_options() {
    for (gutter, text, want) in [
        (Gutter::Codex, "› Fix the search", true),
        (Gutter::Codex, "›", false),
        (Gutter::Codex, "• I will fix the search", false),
        (Gutter::Codex, "  › quoted text", false),
        (Gutter::Marker("❯"), " ❯ RUNCMD for me   11:05 ┃", true),
        (Gutter::Marker("❯"), "  ❯ 1. Yes", false),
        (Gutter::Marker("✨"), " ✨ SLOW stream something", true),
        (Gutter::Marker(">"), "> explain this", true),
        (Gutter::Label, "You: what changed?", true),
        (Gutter::Label, "    You: quoted", false),
        // Claude Code's composer and dialogs draw `❯` on the plain background.
        (Gutter::Claude, "❯ Review this change", false),
    ] {
        assert_eq!(
            gutter.starts_message(None, &row(text), None),
            want,
            "{gutter:?}: {text}"
        );
    }
    let mut wrapped = row("› genuine message that wraps");
    wrapped.last_mut().unwrap().style |= TermStyle::SOFT_WRAP;
    assert!(!Gutter::Codex.starts_message(Some(&wrapped), &row("› continuation"), None));
    assert_eq!(Gutter::for_agent("shell"), None);
    assert_eq!(Gutter::for_agent("claude-code"), Some(Gutter::Claude));
    assert_eq!(Gutter::for_agent("some-new-agent"), Some(Gutter::Label));
}

#[test]
fn numbered_choosers_are_not_messages_but_numbered_messages_are() {
    let chooser = rows(
        "› 1. Update now (runs `npm install -g @openai/codex`)\n  2. Skip\n  3. Skip until next version\n\n  Press enter to continue\n› 1. Fix the search, then 2. run the checks\n• I will do that",
    );
    assert_eq!(
        Gutter::Codex.history_starts(None, &chooser, 0, i64::MAX),
        vec![5]
    );
    let numbered =
        rows("› 1. Fix the search\n  2. Add a regression\n• My plan\n  1. Inspect it\n  2. Fix it");
    assert_eq!(
        Gutter::Codex.history_starts(None, &numbered, 0, i64::MAX),
        vec![0]
    );
}

#[test]
fn recorded_inline_agents_keep_sent_messages_and_drop_drafts() {
    for (gutter, text, expected) in [
        (
            Gutter::Marker("❯"),
            include_str!("../../../diri-engine/tests/fixtures/copilot_screens/idle.txt"),
            vec![8, 12, 17],
        ),
        (
            Gutter::Marker("✨"),
            include_str!("../../../diri-engine/tests/fixtures/kimi_screens/working.txt"),
            vec![12, 14],
        ),
        (
            Gutter::Marker("❯"),
            include_str!("../../../diri-engine/tests/fixtures/copilot_screens/permission.txt"),
            vec![2, 6, 11],
        ),
    ] {
        let found = gutter.history_starts(None, &rows(text), 0, 0);
        assert_eq!(found, expected, "{gutter:?}");
    }
    let draft = rows("› First message\n• reply\n› unfinished draft");
    assert_eq!(Gutter::Codex.history_starts(None, &draft, 0, 0), vec![0]);
}

/// A full-screen Agent in the style of Codex: a pinned copy of the current
/// turn's message on row 0, the transcript below it, and a composer that
/// never scrolls.
struct FakeAgent {
    transcript: Vec<String>,
    top: usize,
    lines: fn(u16) -> usize,
    /// How the planner expects it to move.
    notches: Notches,
    /// PageUp/PageDown move half the transcript; otherwise they do nothing.
    pages: bool,
    swallow_reversal: bool,
    /// Like Claude Code, leave this message undrawn when it scrolls into
    /// view from below, until the view next moves up.
    undrawn_going_down: Option<String>,
    undrawn: bool,
    last_up: Option<bool>,
    spinner: bool,
    /// Screens drawn, each a round trip to the Agent.
    redraws: usize,
}

const SCREEN: usize = 30;
const REGION: usize = 26;

impl FakeAgent {
    fn new(replies: &[usize], lines: fn(u16) -> usize, notches: Notches) -> Self {
        let mut transcript = vec!["  >_ Agent".to_owned(), String::new()];
        for (turn, reply) in replies.iter().enumerate() {
            transcript.push(format!("› Message {turn}"));
            transcript.push(String::new());
            transcript.push(format!("• Reply {turn} begins"));
            for line in 1..*reply {
                transcript.push(String::new());
                transcript.push(format!("  reply {turn} line {line}"));
            }
            transcript.push(String::new());
        }
        let mut agent = Self {
            transcript,
            top: 0,
            lines,
            notches,
            pages: notches.page_keys,
            swallow_reversal: false,
            undrawn_going_down: None,
            undrawn: false,
            last_up: None,
            spinner: false,
            redraws: 0,
        };
        agent.top = agent.bottom();
        agent
    }

    fn bottom(&self) -> usize {
        self.transcript.len().saturating_sub(REGION)
    }

    fn screen(&self) -> Vec<Vec<GridCell>> {
        let mut screen: Vec<_> = (0..REGION)
            .map(|offset| {
                self.transcript
                    .get(self.top + offset)
                    .filter(|text| {
                        !self.undrawn || self.undrawn_going_down.as_deref() != Some(text.as_str())
                    })
                    .map_or_else(|| row(""), |text| row(text))
            })
            .collect();
        let turn = self.transcript[..=self.top]
            .iter()
            .rposition(|line| line.starts_with('›'));
        if let Some(turn) = turn.filter(|turn| *turn < self.top) {
            screen[0] = row(&self.transcript[turn]);
        }
        screen.push(row("› draft"));
        screen.push(row(""));
        screen.push(row(if self.spinner {
            "  ⠋ working"
        } else {
            "  ⠙ working"
        }));
        screen.push(row("  ? for shortcuts"));
        assert_eq!(screen.len(), SCREEN);
        screen
    }

    fn wheel(&mut self, up: bool, ticks: u16) {
        let mut ticks = ticks;
        if self.swallow_reversal && self.last_up.is_some_and(|last| last != up) {
            ticks -= 1;
        }
        self.last_up = Some(up);
        let lines = (self.lines)(ticks);
        let visible = |agent: &Self| {
            agent.undrawn_going_down.as_ref().is_some_and(|hidden| {
                agent.transcript[agent.top..(agent.top + REGION).min(agent.transcript.len())]
                    .contains(hidden)
            })
        };
        let before = visible(self);
        self.top = if up {
            self.top.saturating_sub(lines)
        } else {
            (self.top + lines).min(self.bottom())
        };
        if up {
            self.undrawn = false;
        } else if !before && visible(self) {
            self.undrawn = true;
        }
        self.spinner = !self.spinner;
        self.redraws += 1;
    }

    fn page(&mut self, up: bool) {
        if !self.pages {
            return;
        }
        let lines = (REGION - 1) / 2;
        let visible = |agent: &Self| {
            agent.undrawn_going_down.as_ref().is_some_and(|hidden| {
                agent.transcript[agent.top..(agent.top + REGION).min(agent.transcript.len())]
                    .contains(hidden)
            })
        };
        let before = visible(self);
        self.top = if up {
            self.top.saturating_sub(lines)
        } else {
            (self.top + lines).min(self.bottom())
        };
        if up {
            self.undrawn = false;
        } else if !before && visible(self) {
            self.undrawn = true;
        }
        self.spinner = !self.spinner;
        self.redraws += 1;
    }

    /// Runs one jump; returns the transcript line the arrived message is.
    fn jump(&mut self, next: bool, from: Option<usize>) -> Option<(usize, String)> {
        let (mut travel, mut step) =
            Travel::begin_with(Gutter::Codex, self.notches, next, &self.screen(), from);
        for _ in 0..1000 {
            step = match step {
                TravelStep::Scroll { up, ticks } => {
                    self.wheel(up, ticks);
                    travel.observe(&self.screen())
                }
                TravelStep::Page { up } => {
                    self.page(up);
                    travel.observe(&self.screen())
                }
                TravelStep::Wait => travel.observe(&self.screen()),
                TravelStep::Arrived { start, end } => {
                    let screen = self.screen();
                    assert!(end > start);
                    return Some((start, row_text(&screen[start]).trim_end().to_owned()));
                }
                TravelStep::Exhausted => return None,
                TravelStep::Lost => panic!("lost at top {}", self.top),
            };
        }
        panic!("travel did not finish");
    }
}

/// From live output with the latest message scrolled away, Previous visits
/// every message back to the first, and Next returns to live.
fn walk(agent: &mut FakeAgent, tolerance: usize) {
    let turns = agent
        .transcript
        .iter()
        .filter(|line| line.starts_with('›'))
        .count();
    let mut from = None;
    for turn in (0..turns).rev() {
        let (row, text) = agent.jump(false, from).expect("an earlier message");
        assert_eq!(text, format!("› Message {turn}"));
        assert!(
            row >= 1 && row <= tolerance,
            "message {turn} landed on row {row}"
        );
        from = Some(row);
    }
    assert_eq!(agent.jump(false, from), None, "nothing before the first");
    assert_eq!(agent.top, 0, "the search ended at the top");
    let mut from = Some(
        agent
            .screen()
            .iter()
            .position(|row| row_text(row).starts_with("› Message 0"))
            .unwrap(),
    );
    for turn in 1..turns {
        let (row, text) = agent.jump(true, from).expect("a later message");
        assert_eq!(text, format!("› Message {turn}"));
        from = Some(row);
    }
    assert_eq!(agent.jump(true, from), None, "nothing after the latest");
    assert_eq!(
        agent.top,
        agent.bottom(),
        "the view returned to the latest output"
    );
}

#[test]
fn travel_visits_every_message_with_three_line_notches() {
    let mut agent = FakeAgent::new(
        &[3, 40, 1, 120, 8, 2, 60],
        |ticks| usize::from(ticks) * 3,
        Notches::THREE_LINES,
    );
    walk(&mut agent, 3);
    // About 450 transcript rows each way: most of a screen a step, and one
    // more to place each of the fourteen jumps.
    assert!(agent.redraws < 80, "{} redraws", agent.redraws);
}

#[test]
fn travel_survives_accelerated_notches_and_a_swallowed_reversal() {
    // Claude Code: page keys, then notches that speed up in longer bursts,
    // and the first notch after the wheel turns around ignored.
    let mut agent = FakeAgent::new(
        &[5, 70, 2, 150, 40],
        |ticks| Notches::CLAUDE.lines(ticks, false) as usize,
        Notches::CLAUDE,
    );
    agent.swallow_reversal = true;
    walk(&mut agent, 2);
    assert!(agent.redraws < 120, "{} redraws", agent.redraws);
}

#[test]
fn travel_falls_back_to_notches_when_page_keys_do_nothing() {
    let mut agent = FakeAgent::new(
        &[5, 70, 2, 150, 40],
        |ticks| Notches::CLAUDE.lines(ticks, false) as usize,
        Notches::CLAUDE,
    );
    agent.pages = false;
    agent.swallow_reversal = true;
    walk(&mut agent, 2);
}

#[test]
fn travel_finds_a_message_claude_leaves_undrawn_while_paging_down() {
    let mut agent = FakeAgent::new(
        &[3, 40, 30, 50],
        |ticks| Notches::CLAUDE.lines(ticks, false) as usize,
        Notches::CLAUDE,
    );
    agent.swallow_reversal = true;
    agent.undrawn_going_down = Some("› Message 2".into());
    walk(&mut agent, 2);
}

#[test]
fn travel_finds_a_message_left_undrawn_while_moving_down() {
    let mut agent = FakeAgent::new(
        &[3, 40, 30, 50],
        |ticks| usize::from(ticks) * 3,
        Notches::THREE_LINES,
    );
    agent.undrawn_going_down = Some("› Message 2".into());
    walk(&mut agent, 3);
}

#[test]
fn travel_from_live_skips_the_message_already_on_screen() {
    let mut agent = FakeAgent::new(
        &[3, 30, 4],
        |ticks| usize::from(ticks) * 3,
        Notches::THREE_LINES,
    );
    // The latest message and its short reply are visible at the bottom.
    assert!(
        agent
            .screen()
            .iter()
            .any(|row| row_text(row).starts_with("› Message 2"))
    );
    let (_, text) = agent.jump(false, None).unwrap();
    assert_eq!(text, "› Message 1");
}

#[test]
fn notch_models_match_the_recorded_curves() {
    let claude = Notches::CLAUDE;
    assert_eq!(claude.lines(4, false), 4.0);
    assert_eq!(claude.lines(10, false), 19.0);
    assert_eq!(claude.lines(9, false), 16.0, "between recorded sizes");
    assert_eq!(claude.lines(8, true), 10.0, "a turn loses one notch");
    assert_eq!(claude.lines(1, true), 0.0);
    assert_eq!(claude.within(20.0, false), 10);
    assert_eq!(claude.within(5.0, false), 4);
    assert_eq!(claude.within(1.0, true), 2, "the shortest burst that moves");
    assert_eq!(claude.within_most(40.0, false, claude.most_aligning), 8);
    let three = Notches::THREE_LINES;
    assert_eq!(three.lines(7, true), 21.0);
    assert_eq!(three.within(40.0, false), 13);
    assert_eq!(three.within(2.0, false), 1);
}

#[test]
fn shift_measurement_prefers_lined_up_text() {
    let keys = |lines: &[&str]| {
        lines
            .iter()
            .map(|line| row_key(&row(line)))
            .collect::<Vec<_>>()
    };
    let before = keys(&["a", "", "b", "", "c", "", "d", "", "e", ""]);
    let after = keys(&["x", "", "y", "", "a", "", "b", "", "c", ""]);
    assert_eq!(
        estimate_shift(&before, &after, &(0..10), 0, 0..=10),
        Some(4)
    );
    let blank = keys(&["", "", "", ""]);
    assert_eq!(estimate_shift(&blank, &blank, &(0..4), 3, 0..=4), None);
    // Two replies that end alike line up both ways; only the notches' way
    // is a move.
    let before = keys(&["r 1", "r 2", "r 3", "r 4", "", "", "", ""]);
    let after = keys(&["", "", "", "", "r 1", "r 2", "r 3", "r 4"]);
    assert_eq!(
        estimate_shift(&after, &before, &(0..8), -4, -8..=0),
        Some(-4)
    );
    assert_eq!(estimate_shift(&after, &before, &(0..8), 4, 0..=8), None);
}
