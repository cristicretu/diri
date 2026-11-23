// Adapted from Ely GPUI Components (MIT OR Apache-2.0), https://github.com/ZacharyZhang-NY/Ely-GPUI-Components
//
//! Word-level emphasis for changed lines.
//!
//! A removed line and the added line that replaced it usually differ in a
//! word or two. The review marks just those words, the way Ely's `DiffViewer`
//! does with `similar`'s inline changes, but without a new dependency: lines
//! are split into word, space, and punctuation tokens, the common head and tail
//! are trimmed, and what is left is aligned by a bounded longest common
//! subsequence. Everything here runs once per snapshot on the loader's
//! background thread, never per frame.

use std::ops::Range;

use crate::diff::{DiffRow, DiffRowKind, RowWords};

/// The changed ranges of the old line and of the new line.
type ChangedWords = (Vec<Range<usize>>, Vec<Range<usize>>);

/// Lines longer than this are emphasized as a whole or not at all: a minified
/// bundle is not worth aligning token by token.
const MAX_LINE_BYTES: usize = 2_000;
/// The alignment table for one pair of lines may hold at most this many cells.
const MAX_PAIR_CELLS: usize = 40_000;
/// The whole snapshot spends at most this many alignment cells. Past it, pairs
/// fall back to trimming the common head and tail, which is linear.
const SNAPSHOT_CELL_BUDGET: usize = 24_000_000;
/// Below this share of shared (non-space) text, a pair is two unrelated lines
/// that happen to sit side by side, and marking every word would be noise.
const MIN_SHARED_SHARE: f32 = 0.20;

/// The changed byte ranges of `old` and `new`, or `None` when the two lines
/// are too different (or too long) for word emphasis to help.
#[must_use]
pub(crate) fn changed_words(old: &str, new: &str) -> Option<ChangedWords> {
    let mut budget = MAX_PAIR_CELLS;
    changed_words_within(old, new, &mut budget)
}

fn changed_words_within(old: &str, new: &str, budget: &mut usize) -> Option<ChangedWords> {
    if old == new || old.len() + new.len() > MAX_LINE_BYTES * 2 {
        return None;
    }
    let old_tokens = tokens(old);
    let new_tokens = tokens(new);
    let head = old_tokens
        .iter()
        .zip(&new_tokens)
        .take_while(|(left, right)| old[(*left).clone()] == new[(*right).clone()])
        .count();
    let tail = old_tokens[head..]
        .iter()
        .rev()
        .zip(new_tokens[head..].iter().rev())
        .take_while(|(left, right)| old[(*left).clone()] == new[(*right).clone()])
        .count();
    let old_middle = &old_tokens[head..old_tokens.len() - tail];
    let new_middle = &new_tokens[head..new_tokens.len() - tail];

    let cells = old_middle.len().saturating_mul(new_middle.len());
    let (old_changed, new_changed) = if cells == 0 {
        (old_middle.to_vec(), new_middle.to_vec())
    } else if cells <= MAX_PAIR_CELLS && cells <= *budget {
        *budget -= cells;
        unmatched(old, old_middle, new, new_middle)
    } else {
        (old_middle.to_vec(), new_middle.to_vec())
    };

    let old_ranges = merge(old, &old_changed);
    let new_ranges = merge(new, &new_changed);
    let shared = |text: &str, changed: &[Range<usize>]| {
        let total = solid_len(text);
        let changed: usize = changed
            .iter()
            .map(|range| solid_len(&text[range.clone()]))
            .sum();
        (total.saturating_sub(changed), total)
    };
    let (old_shared, old_total) = shared(old, &old_ranges);
    let (new_shared, new_total) = shared(new, &new_ranges);
    let longest = old_total.max(new_total);
    if longest > 0 && (old_shared.min(new_shared) as f32) < longest as f32 * MIN_SHARED_SHARE {
        return None;
    }
    if old_ranges.is_empty() && new_ranges.is_empty() {
        return None;
    }
    Some((old_ranges, new_ranges))
}

/// Word emphasis for every replaced line of a snapshot. Inside a hunk, each
/// run of removed lines is paired, line for line, with the run of added lines
/// directly after it, the way a split view sets them side by side.
#[must_use]
pub(crate) fn snapshot_words(rows: &[DiffRow]) -> Vec<RowWords> {
    let mut words = Vec::new();
    let mut budget = SNAPSHOT_CELL_BUDGET;
    let mut index = 0;
    while index < rows.len() {
        if rows[index].kind != DiffRowKind::Deletion {
            index += 1;
            continue;
        }
        let removed_start = index;
        while index < rows.len() && rows[index].kind == DiffRowKind::Deletion {
            index += 1;
        }
        let added_start = index;
        while index < rows.len() && rows[index].kind == DiffRowKind::Addition {
            index += 1;
        }
        let pairs = (added_start - removed_start).min(index - added_start);
        for offset in 0..pairs {
            let (old_row, new_row) = (removed_start + offset, added_start + offset);
            let Some((old, new)) =
                changed_words_within(&rows[old_row].text, &rows[new_row].text, &mut budget)
            else {
                continue;
            };
            if !old.is_empty() {
                words.push(RowWords {
                    row: old_row,
                    ranges: old,
                });
            }
            if !new.is_empty() {
                words.push(RowWords {
                    row: new_row,
                    ranges: new,
                });
            }
        }
    }
    words.sort_by_key(|words| words.row);
    words
}

/// Splits a line into identifier runs, whitespace runs, and single other
/// characters, as byte ranges on character boundaries.
fn tokens(text: &str) -> Vec<Range<usize>> {
    #[derive(PartialEq)]
    enum Class {
        Word,
        Space,
        Other,
    }
    let class = |character: char| {
        if character.is_alphanumeric() || character == '_' {
            Class::Word
        } else if character.is_whitespace() {
            Class::Space
        } else {
            Class::Other
        }
    };
    let mut tokens: Vec<Range<usize>> = Vec::new();
    let mut previous = None;
    for (start, character) in text.char_indices() {
        let end = start + character.len_utf8();
        let current = class(character);
        match (tokens.last_mut(), previous.as_ref()) {
            (Some(last), Some(previous)) if *previous == current && current != Class::Other => {
                last.end = end;
            }
            _ => tokens.push(start..end),
        }
        previous = Some(current);
    }
    tokens
}

/// The tokens of each side left out of a longest common subsequence.
fn unmatched(
    old: &str,
    old_tokens: &[Range<usize>],
    new: &str,
    new_tokens: &[Range<usize>],
) -> (Vec<Range<usize>>, Vec<Range<usize>>) {
    let (rows, columns) = (old_tokens.len(), new_tokens.len());
    // lengths[i][j] is the LCS of old[i..] and new[j..].
    let mut lengths = vec![0_u16; (rows + 1) * (columns + 1)];
    let at = |row: usize, column: usize| row * (columns + 1) + column;
    for row in (0..rows).rev() {
        for column in (0..columns).rev() {
            lengths[at(row, column)] =
                if old[old_tokens[row].clone()] == new[new_tokens[column].clone()] {
                    lengths[at(row + 1, column + 1)].saturating_add(1)
                } else {
                    lengths[at(row + 1, column)].max(lengths[at(row, column + 1)])
                };
        }
    }
    let (mut old_changed, mut new_changed) = (Vec::new(), Vec::new());
    let (mut row, mut column) = (0, 0);
    while row < rows && column < columns {
        if old[old_tokens[row].clone()] == new[new_tokens[column].clone()] {
            row += 1;
            column += 1;
        } else if lengths[at(row + 1, column)] >= lengths[at(row, column + 1)] {
            old_changed.push(old_tokens[row].clone());
            row += 1;
        } else {
            new_changed.push(new_tokens[column].clone());
            column += 1;
        }
    }
    old_changed.extend(old_tokens[row..].iter().cloned());
    new_changed.extend(new_tokens[column..].iter().cloned());
    (old_changed, new_changed)
}

/// Joins changed tokens into ranges. Two changes separated only by unchanged
/// whitespace read as one edit, so the gap joins them; whitespace at the edge
/// of a range is let go unless the range is nothing but whitespace, since an
/// indentation change is still a change.
fn merge(text: &str, changed: &[Range<usize>]) -> Vec<Range<usize>> {
    let mut ranges: Vec<Range<usize>> = Vec::new();
    for token in changed {
        if let Some(last) = ranges.last_mut()
            && text[last.end.min(token.start)..token.start]
                .trim()
                .is_empty()
            && last.end <= token.start
        {
            last.end = token.end;
            continue;
        }
        ranges.push(token.clone());
    }
    for range in &mut ranges {
        let slice = &text[range.clone()];
        let trimmed = slice.trim();
        if trimmed.is_empty() {
            continue;
        }
        let leading = slice.len() - slice.trim_start().len();
        let trailing = slice.len() - slice.trim_end().len();
        *range = range.start + leading..range.end - trailing;
    }
    ranges
}

fn solid_len(text: &str) -> usize {
    text.chars()
        .filter(|character| !character.is_whitespace())
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn marked<'a>(text: &'a str, ranges: &[Range<usize>]) -> Vec<&'a str> {
        ranges.iter().map(|range| &text[range.clone()]).collect()
    }

    #[test]
    fn a_changed_literal_marks_only_that_literal() {
        let (old, new) = changed_words("let x = 1;", "let x = 2;").expect("words");
        assert_eq!(marked("let x = 1;", &old), ["1"]);
        assert_eq!(marked("let x = 2;", &new), ["2"]);
    }

    #[test]
    fn an_inserted_argument_marks_only_the_insertion() {
        let before = "call(alpha, gamma)";
        let after = "call(alpha, beta, gamma)";
        let (old, new) = changed_words(before, after).expect("words");
        assert!(old.is_empty());
        assert_eq!(marked(after, &new), ["beta,"]);
    }

    #[test]
    fn neighbouring_changes_across_a_space_join() {
        let (old, new) = changed_words("a quick brown fox", "a slow red fox").expect("words");
        assert_eq!(marked("a quick brown fox", &old), ["quick brown"]);
        assert_eq!(marked("a slow red fox", &new), ["slow red"]);
    }

    #[test]
    fn unrelated_lines_are_not_emphasized() {
        assert!(changed_words("fn render(&self) {", "import os").is_none());
        assert!(changed_words("same", "same").is_none());
    }

    #[test]
    fn ranges_stay_on_character_boundaries() {
        let before = "let naïve = \"café\";";
        let after = "let naïve = \"caffè\";";
        let (old, new) = changed_words(before, after).expect("words");
        for range in old.iter().chain(&new) {
            assert!(before.is_char_boundary(range.start) || after.is_char_boundary(range.start));
        }
        assert_eq!(marked(before, &old), ["café"]);
        assert_eq!(marked(after, &new), ["caffè"]);
    }

    #[test]
    fn very_long_lines_are_left_alone() {
        let long = "x".repeat(MAX_LINE_BYTES * 2);
        assert!(changed_words(&long, "x").is_none());
    }

    #[test]
    fn snapshot_pairs_removals_with_the_additions_after_them() {
        let rows = |items: &[(DiffRowKind, &str)]| -> Vec<DiffRow> {
            items
                .iter()
                .map(|(kind, text)| DiffRow {
                    kind: *kind,
                    old_line: None,
                    new_line: None,
                    text: (*text).to_owned(),
                })
                .collect()
        };
        use DiffRowKind::*;
        let rows = rows(&[
            (Hunk, "@@ -1,3 +1,3 @@"),
            (Context, "fn main() {"),
            (Deletion, "    let total = 1;"),
            (Deletion, "    let count = 3;"),
            (Addition, "    let total = 2;"),
            (Context, "}"),
            (Addition, "// trailing"),
        ]);
        let words = snapshot_words(&rows);
        let rows_with_words: Vec<usize> = words.iter().map(|words| words.row).collect();
        assert_eq!(
            rows_with_words,
            [2, 4],
            "the unpaired removal and addition stay plain"
        );
        assert_eq!(marked(&rows[2].text, &words[0].ranges), ["1"]);
        assert_eq!(marked(&rows[4].text, &words[1].ranges), ["2"]);
    }
}
