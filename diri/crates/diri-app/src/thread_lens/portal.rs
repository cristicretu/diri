//! Interactive screens an Agent draws but never records: Claude's `/model`,
//! `/config`, `/mcp`, `/permissions` and `/login` pickers, Codex's model and
//! approval pickers, a trust dialog, a `fzf` list. None of them reach the
//! transcript, so the GUI Lens would go blank while the CLI waits on a key.
//!
//! [`interactive`] reads the live screen's text and returns the rows the
//! screen occupies, so the pane can show exactly that slice of the real grid
//! above the composer and forward keys to it until it closes. It recognises
//! three shapes, each checked against real Claude Code 2.1 frames
//! (`screens/`):
//!
//! - a panel under a `▔` rule (`/status`, `/config`, `/usage`, `/help`,
//!   `/model`'s settings search) while Claude's own input box is gone;
//! - any screen with a picker footer (`Esc to cancel`, `Enter to confirm`,
//!   `←/→ to switch`, …) or a highlighted numbered list;
//! - the alternate screen (`fzf`, an editor, a full-screen TUI).

use std::ops::Range;

/// A picker taller than this is shown from its bottom: the hint and the
/// highlighted choice are what the user needs.
const MAX_ROWS: usize = 24;

/// Footer hints every picker family prints under its choices.
const HINTS: &[&str] = &[
    "esc to cancel",
    "esc to go back",
    "esc to exit",
    "esc to close",
    "esc to dismiss",
    "esc to interrupt and",
    "enter to confirm",
    "enter to select",
    "enter to save",
    "enter to continue",
    "press enter to",
    "↑/↓ to",
    "↑↓ to",
    "arrow keys to",
    "to navigate",
    "space to toggle",
    "←/→ to switch",
    "to switch tabs",
    "to search",
];

/// An interactive screen on the live grid: its rows, and whether the top
/// (a panel's tab bar) or the bottom (a picker's hint and highlighted
/// choice) is what to keep when it is taller than the portal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Screen {
    pub rows: Range<usize>,
    pub anchor_top: bool,
}

/// The interactive screen the Agent shows, if any.
pub fn interactive(rows: &[String], alt_screen: bool) -> Option<Screen> {
    let end = rows.iter().rposition(|row| !is_blank(row)).map(|r| r + 1)?;
    if alt_screen {
        return Some(Screen { rows: 0..end, anchor_top: true });
    }
    // A Settings or Help panel replaces Claude's input box; the box coming
    // back is what closing it looks like.
    let panel = rows.iter().rposition(|row| is_panel_rule(row));
    if let Some(top) = panel
        && !prompt_box_after(rows, top)
    {
        let start = (top + 1..end).find(|&r| !is_blank(&rows[r])).unwrap_or(top + 1);
        return Some(Screen { rows: start..end, anchor_top: true });
    }
    if let Some(range) = picker_rows(rows) {
        return Some(Screen { rows: range, anchor_top: false });
    }
    // A screen that scrolls (`↓ 22 more`, a lone `↓`) with no input box.
    let scrolls = rows[..end].last().is_some_and(|row| {
        let row = row.trim();
        row == "↓" || row == "↑" || (row.starts_with("↓ ") && row.ends_with("more"))
    });
    (scrolls && !prompt_box_after(rows, 0)).then_some(Screen { rows: 0..end, anchor_top: true })
}

/// Claude's input box: a `❯` row with a `─` rule directly above it.
fn prompt_box_after(rows: &[String], from: usize) -> bool {
    (from.max(1)..rows.len()).any(|r| {
        let row = rows[r].trim_start();
        (row.starts_with('❯') || row.starts_with('›')) && is_rule(&rows[r - 1])
    })
}

/// The heavy top border Claude draws over its Settings and Help panels.
fn is_panel_rule(row: &str) -> bool {
    let trimmed = row.trim();
    trimmed.chars().count() >= 12 && trimmed.chars().all(|c| c == '▔')
}

/// The visible rows of an open picker, top to bottom, or `None` when the
/// screen is an ordinary prompt or output.
fn picker_rows(rows: &[String]) -> Option<Range<usize>> {
    let lowered: Vec<String> = rows.iter().map(|row| row.to_lowercase()).collect();
    let hint = lowered
        .iter()
        .rposition(|row| HINTS.iter().any(|hint| row.contains(hint)))
        .or_else(|| numbered_choices(rows))?;
    // The picker ends at its last non-blank row at or below the hint.
    let mut bottom = hint + 1;
    while bottom < rows.len() && !is_blank(&rows[bottom]) && !is_rule(&rows[bottom]) {
        bottom += 1;
    }
    // It starts below the nearest rule above the hint (Claude frames its
    // pickers with one), or after two blank rows, or at the screen's top.
    let mut top = hint;
    let mut blanks = 0;
    while top > 0 {
        let above = &rows[top - 1];
        if is_rule(above) || (is_blank(above) && top >= 2 && is_prompt_echo(&rows[top - 2])) {
            break;
        }
        if is_blank(above) {
            blanks += 1;
            if blanks == 2 {
                break;
            }
        } else {
            blanks = 0;
        }
        top -= 1;
    }
    while top < hint && is_blank(&rows[top]) {
        top += 1;
    }
    if bottom - top > MAX_ROWS {
        top = bottom - MAX_ROWS;
    }
    Some(top..bottom)
}

/// A selection list with no footer: a highlighted `❯ 2.` row among at least
/// two numbered choices. Returns the last choice's row.
fn numbered_choices(rows: &[String]) -> Option<usize> {
    let choice = |row: &str| {
        let row = row.trim_start().trim_start_matches(['❯', '›', '>', '▸', '→']).trim_start();
        let digits = row.chars().take_while(char::is_ascii_digit).count();
        digits > 0 && row[digits..].starts_with(". ")
    };
    let marked = rows.iter().position(|row| {
        let trimmed = row.trim_start();
        trimmed.starts_with(['❯', '›', '▸', '→']) && choice(row)
    })?;
    let last = (marked..rows.len()).take_while(|&i| choice(&rows[i]) || is_blank(&rows[i])).filter(|&i| choice(&rows[i])).last()?;
    let first = (0..=marked).rev().take_while(|&i| choice(&rows[i])).last()?;
    (last > first).then_some(last)
}

/// The command line that opened the picker (`› /model`), not part of it.
fn is_prompt_echo(row: &str) -> bool {
    let trimmed = row.trim_start();
    ["› /", "❯ /", "> /"].iter().any(|prefix| trimmed.starts_with(prefix))
}

fn is_blank(row: &str) -> bool {
    row.trim().is_empty()
}

fn is_rule(row: &str) -> bool {
    let trimmed = row.trim();
    trimmed.chars().count() >= 12 && trimmed.chars().all(|c| matches!(c, '─' | '━' | '═' | '-' | '╌' | '┄'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen(lines: &[&str]) -> Vec<String> {
        lines.iter().map(|line| (*line).to_owned()).collect()
    }

    /// Claude Code 2.1's `/model` picker, as in the user's screenshot.
    #[test]
    fn claudes_model_picker_is_found_below_its_rule() {
        let rows = screen(&[
            "● `/model` only works when it's on a line by itself.",
            "",
            "✻ Cogitated for 1s · done 11:32 PM",
            "────────────────────────────────────────────────────────────",
            "",
            "  Select model",
            "  Your pick is the default for new sessions. For other models, use --model.",
            "",
            "    1. Default (recommended)   Opus 5.5 · Best for everyday, complex tasks",
            "  ❯ 2. Opus 5.5 ✔              For complex work and everyday tasks",
            "    3. Fable 5.2               For your toughest challenges",
            "    4. Sonnet 5.5              Most efficient for simpler tasks",
            "    5. Haiku 5.5               Fastest for quick answers",
            "    ▸ More models",
            "",
            "  ◐ Medium effort (default) ←/→ to adjust",
            "",
            "  Use /fast to turn on fast mode (Opus 5.5).",
            "",
            "  s to use this session only · Enter to save · Esc to cancel",
            "",
        ]);
        assert_eq!(picker_rows(&rows), Some(5..20));
    }

    #[test]
    fn an_ordinary_prompt_box_is_not_a_picker() {
        let rows = screen(&[
            "● Hi! What can I help you with today?",
            "",
            "────────────────────────────────────────────────────────────",
            "❯ 1. ship it",
            "────────────────────────────────────────────────────────────",
            "  ⏵⏵ auto mode on (shift+tab to cycle)",
        ]);
        assert_eq!(picker_rows(&rows), None);
    }

    #[test]
    fn codex_pickers_and_footerless_lists_are_found() {
        let codex = screen(&[
            "› /model",
            "",
            "  Select Model and Effort",
            "  › 1. gpt-5.5 (current)  Latest frontier model",
            "    2. gpt-5.5-mini       Faster, cheaper",
            "",
            "  Press enter to confirm or esc to go back",
        ]);
        assert_eq!(picker_rows(&codex), Some(2..7));
        let trust = screen(&[
            "Do you trust the files in this folder?",
            "",
            " ❯ 1. Yes, proceed",
            "   2. No, exit",
            "",
        ]);
        assert_eq!(picker_rows(&trust), Some(0..4), "the question stays with its choices");
    }

    #[test]
    fn a_tall_picker_keeps_its_bottom() {
        let mut lines = vec!["  Select an MCP server".to_owned()];
        lines.extend((0..40).map(|n| format!("    {n}. server-{n}")));
        lines.push("  ↑/↓ to navigate · Enter to select · Esc to cancel".to_owned());
        let range = picker_rows(&lines).unwrap();
        assert_eq!(range.end, lines.len());
        assert_eq!(range.len(), MAX_ROWS);
    }

    fn real(name: &str) -> Vec<String> {
        let text = match name {
            "idle" => include_str!("screens/claude-idle.txt"),
            "usage" => include_str!("screens/claude-usage.txt"),
            "usage-closed" => include_str!("screens/claude-usage-closed.txt"),
            "status" => include_str!("screens/claude-status.txt"),
            "config" => include_str!("screens/claude-config.txt"),
            "help" => include_str!("screens/claude-help.txt"),
            "model-settings" => include_str!("screens/claude-model-settings.txt"),
            "context" => include_str!("screens/claude-context.txt"),
            _ => unreachable!(),
        };
        text.lines().map(str::to_owned).collect()
    }

    /// The user was trapped in the GUI by `/usage`: a panel taller than the
    /// screen, no footer hint, only a `↓`. Every Settings/Help panel from a
    /// real Claude Code 2.1 run now opens the portal from its tab bar down.
    #[test]
    fn real_settings_panels_open_the_portal_from_their_tab_bar() {
        for (name, rows) in [("usage", 3..40), ("status", 3..22), ("config", 3..40), ("model-settings", 3..12), ("help", 23..40)] {
            let screen = interactive(&real(name), false).unwrap_or_else(|| panic!("{name} must open the portal"));
            assert_eq!(screen, Screen { rows, anchor_top: true }, "{name}");
        }
    }

    /// Esc brings the input box back: the portal closes. `/context` prints
    /// inline and never takes the keyboard.
    #[test]
    fn the_idle_prompt_and_inline_output_are_not_interactive() {
        for name in ["idle", "usage-closed", "context"] {
            assert_eq!(interactive(&real(name), false), None, "{name}");
        }
    }

    #[test]
    fn the_alternate_screen_is_always_interactive() {
        let rows = screen(&["> fzf query", "  a.rs", "  b.rs", ""]);
        assert_eq!(interactive(&rows, true), Some(Screen { rows: 0..3, anchor_top: true }));
    }
}
