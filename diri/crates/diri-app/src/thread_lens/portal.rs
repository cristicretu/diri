//! Interactive screens an Agent draws but never records: Claude's `/model`,
//! `/config`, `/mcp`, `/permissions` and `/login` pickers, Codex's model and
//! approval pickers, a trust dialog, a `fzf` list. None of them reach the
//! transcript, so the GUI Lens would go blank while the CLI waits on a key.
//!
//! [`picker_rows`] reads the live screen's text and returns the rows the
//! picker occupies, so the pane can show exactly that slice of the real grid
//! above the composer and forward keys to it until it closes.

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
];

/// The visible rows of an open picker, top to bottom, or `None` when the
/// screen is an ordinary prompt or output.
pub fn picker_rows(rows: &[String]) -> Option<Range<usize>> {
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
}
