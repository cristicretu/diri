//! Diff Cards for an Agent's file edits, computed once per call.
//!
//! An Edit's `old_string`/`new_string` (or a Write's content, or a Codex
//! patch) becomes unified rows with word-level emphasis on replaced lines,
//! reusing the review's bounded word aligner (`git_ui::words`).

use std::ops::Range;

use diri_proto::thread::ToolDetail;

/// Lines aligned by LCS per hunk; past this the hunk shows as remove + add.
const MAX_ALIGN_CELLS: usize = 250_000;
/// Rows a card keeps; a longer change says how many more there are.
pub const MAX_CARD_ROWS: usize = 400;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Context,
    Removed,
    Added,
    /// `@@` separator between hunks of one file.
    Gap,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CardRow {
    pub side: Side,
    pub text: String,
    /// Byte ranges of `text` that changed within a replaced line.
    pub words: Vec<Range<usize>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffCard {
    pub path: String,
    pub added: usize,
    pub removed: usize,
    pub rows: Vec<CardRow>,
    pub hidden_rows: usize,
}

/// One card per file the call touched.
pub fn cards(detail: &ToolDetail) -> Vec<DiffCard> {
    match detail {
        ToolDetail::Edit { path, hunks } => {
            let mut rows = Vec::new();
            for (index, hunk) in hunks.iter().enumerate() {
                if index > 0 {
                    rows.push(CardRow {
                        side: Side::Gap,
                        text: String::new(),
                        words: Vec::new(),
                    });
                }
                rows.extend(line_diff(&hunk.old, &hunk.new));
            }
            vec![finish(path.clone(), rows)]
        }
        ToolDetail::Write { path, content } => {
            let rows = content
                .lines()
                .map(|line| CardRow {
                    side: Side::Added,
                    text: line.to_owned(),
                    words: Vec::new(),
                })
                .collect();
            vec![finish(path.clone(), rows)]
        }
        ToolDetail::Patch { patch } => patch_cards(patch),
        _ => Vec::new(),
    }
}

fn finish(path: String, mut rows: Vec<CardRow>) -> DiffCard {
    let added = rows.iter().filter(|r| r.side == Side::Added).count();
    let removed = rows.iter().filter(|r| r.side == Side::Removed).count();
    let hidden_rows = rows.len().saturating_sub(MAX_CARD_ROWS);
    rows.truncate(MAX_CARD_ROWS);
    DiffCard {
        path,
        added,
        removed,
        rows,
        hidden_rows,
    }
}

/// Codex `apply_patch` bodies and unified diffs, split per file.
fn patch_cards(patch: &str) -> Vec<DiffCard> {
    let mut cards = Vec::new();
    let mut path: Option<String> = None;
    let mut rows: Vec<CardRow> = Vec::new();
    let mut flush = |path: &mut Option<String>, rows: &mut Vec<CardRow>| {
        if let Some(path) = path.take() {
            emphasize(rows);
            cards.push(finish(path, std::mem::take(rows)));
        }
        rows.clear();
    };
    for line in patch.lines() {
        let header = line
            .strip_prefix("*** Update File: ")
            .or_else(|| line.strip_prefix("*** Add File: "))
            .or_else(|| line.strip_prefix("*** Delete File: "))
            .or_else(|| line.strip_prefix("+++ b/"));
        if let Some(name) = header {
            flush(&mut path, &mut rows);
            path = Some(name.trim().to_owned());
            continue;
        }
        if line.starts_with("*** ")
            || line.starts_with("--- ")
            || line.starts_with("diff --git")
            || line.starts_with("index ")
        {
            continue;
        }
        let (side, text) = if let Some(rest) = line.strip_prefix("@@") {
            let _ = rest;
            if rows.is_empty() {
                continue;
            }
            (Side::Gap, "")
        } else if let Some(rest) = line.strip_prefix('+') {
            (Side::Added, rest)
        } else if let Some(rest) = line.strip_prefix('-') {
            (Side::Removed, rest)
        } else {
            (Side::Context, line.strip_prefix(' ').unwrap_or(line))
        };
        if path.is_some() {
            rows.push(CardRow {
                side,
                text: text.to_owned(),
                words: Vec::new(),
            });
        }
    }
    flush(&mut path, &mut rows);
    cards
}

/// Unified rows for `old` → `new`: common lines as context, the rest as
/// removed then added, with word emphasis on paired replacements.
pub fn line_diff(old: &str, new: &str) -> Vec<CardRow> {
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();
    let head = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let tail = a[head..]
        .iter()
        .rev()
        .zip(b[head..].iter().rev())
        .take_while(|(x, y)| x == y)
        .count();
    let (am, bm) = (&a[head..a.len() - tail], &b[head..b.len() - tail]);
    let mut rows: Vec<CardRow> = a[..head].iter().map(|l| context(l)).collect();
    if am.len().saturating_mul(bm.len()) <= MAX_ALIGN_CELLS {
        // LCS table over the middle.
        let (n, m) = (am.len(), bm.len());
        let mut table = vec![0u32; (n + 1) * (m + 1)];
        for i in (0..n).rev() {
            for j in (0..m).rev() {
                table[i * (m + 1) + j] = if am[i] == bm[j] {
                    table[(i + 1) * (m + 1) + j + 1] + 1
                } else {
                    table[(i + 1) * (m + 1) + j].max(table[i * (m + 1) + j + 1])
                };
            }
        }
        let (mut i, mut j) = (0, 0);
        while i < n || j < m {
            if i < n && j < m && am[i] == bm[j] {
                rows.push(context(am[i]));
                i += 1;
                j += 1;
            } else if j < m && (i == n || table[i * (m + 1) + j + 1] >= table[(i + 1) * (m + 1) + j]) {
                rows.push(changed(Side::Added, bm[j]));
                j += 1;
            } else {
                rows.push(changed(Side::Removed, am[i]));
                i += 1;
            }
        }
    } else {
        rows.extend(am.iter().map(|l| changed(Side::Removed, l)));
        rows.extend(bm.iter().map(|l| changed(Side::Added, l)));
    }
    rows.extend(a[a.len() - tail..].iter().map(|l| context(l)));
    reorder_runs(&mut rows);
    emphasize(&mut rows);
    rows
}

fn context(line: &str) -> CardRow {
    CardRow {
        side: Side::Context,
        text: line.to_owned(),
        words: Vec::new(),
    }
}

fn changed(side: Side, line: &str) -> CardRow {
    CardRow {
        side,
        text: line.to_owned(),
        words: Vec::new(),
    }
}

/// Within each run of changed rows, removals first, then additions, as a
/// reviewer reads a replacement.
fn reorder_runs(rows: &mut [CardRow]) {
    let mut start = 0;
    while start < rows.len() {
        if rows[start].side == Side::Context || rows[start].side == Side::Gap {
            start += 1;
            continue;
        }
        let mut end = start;
        while end < rows.len() && matches!(rows[end].side, Side::Added | Side::Removed) {
            end += 1;
        }
        rows[start..end].sort_by_key(|row| row.side == Side::Added);
        start = end;
    }
}

/// Pairs the k-th removed line of a run with its k-th added line and marks
/// the words that differ.
fn emphasize(rows: &mut [CardRow]) {
    let mut start = 0;
    while start < rows.len() {
        if !matches!(rows[start].side, Side::Removed) {
            start += 1;
            continue;
        }
        let removed_end = (start..rows.len())
            .find(|&i| rows[i].side != Side::Removed)
            .unwrap_or(rows.len());
        let added_end = (removed_end..rows.len())
            .find(|&i| rows[i].side != Side::Added)
            .unwrap_or(rows.len());
        let pairs = (removed_end - start).min(added_end - removed_end);
        for k in 0..pairs {
            let (old, new) = (rows[start + k].text.clone(), rows[removed_end + k].text.clone());
            if let Some((old_words, new_words)) = crate::git_ui::words::changed_words(&old, &new) {
                rows[start + k].words = old_words;
                rows[removed_end + k].words = new_words;
            }
        }
        start = added_end.max(start + 1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use diri_proto::thread::EditHunk;

    fn sides(rows: &[CardRow]) -> String {
        rows.iter()
            .map(|r| match r.side {
                Side::Context => ' ',
                Side::Removed => '-',
                Side::Added => '+',
                Side::Gap => '@',
            })
            .collect()
    }

    #[test]
    fn replaced_lines_pair_up_with_word_emphasis() {
        let rows = line_diff("fn a() {\n    let x = 1;\n}\n", "fn a() {\n    let x = 2;\n    y();\n}\n");
        assert_eq!(sides(&rows), " -++ ");
        assert_eq!(&rows[1].text[rows[1].words[0].clone()], "1");
        assert_eq!(&rows[2].text[rows[2].words[0].clone()], "2");
    }

    #[test]
    fn edit_cards_count_and_separate_hunks() {
        let card = &cards(&ToolDetail::Edit {
            path: "src/a.rs".into(),
            hunks: vec![
                EditHunk { old: "a".into(), new: "b".into() },
                EditHunk { old: "c".into(), new: "c\nd".into() },
            ],
        })[0];
        assert_eq!((card.added, card.removed), (2, 1));
        assert_eq!(sides(&card.rows), "-+@ +");
    }

    #[test]
    fn codex_patches_split_per_file() {
        let cards = cards(&ToolDetail::Patch {
            patch: "*** Begin Patch\n*** Update File: a.rs\n@@\n-old\n+new\n*** Add File: b.md\n+hello\n*** End Patch".into(),
        });
        assert_eq!(cards.len(), 2);
        assert_eq!(cards[0].path, "a.rs");
        assert_eq!(sides(&cards[0].rows), "-+");
        assert_eq!(cards[1].added, 1);
    }
}
