//! Find in note: where a query occurs in the open note's blocks.
//!
//! Matching ignores case and runs over each block's stored text, so a match
//! is a block index and a byte range the editor can select as is. Pictures
//! and dividers hold no prose and are skipped.

use std::ops::Range;

use diri_notes::doc::Block;

/// More matches than this are not counted or painted: the count reads "1000+".
pub(crate) const MATCH_LIMIT: usize = 1000;

/// One occurrence: the block it is in and its bytes in that block's text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Match {
    pub(crate) block: usize,
    pub(crate) range: Range<usize>,
}

/// Every non-overlapping occurrence of `query` in `blocks`, in reading
/// order, ignoring case. An empty query matches nothing.
pub(crate) fn find_all(blocks: &[Block], query: &str) -> Vec<Match> {
    let needle: Vec<char> = query.chars().flat_map(char::to_lowercase).collect();
    if needle.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for (index, block) in blocks.iter().enumerate() {
        if block.kind.is_atomic() || block.text.is_empty() {
            continue;
        }
        for range in find_in(&block.text, &needle) {
            out.push(Match {
                block: index,
                range,
            });
            if out.len() >= MATCH_LIMIT {
                return out;
            }
        }
    }
    out
}

/// Byte ranges of `needle` (already lowercased) in `text`, ignoring case.
fn find_in(text: &str, needle: &[char]) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    let mut starts = text.char_indices().map(|(i, _)| i).peekable();
    while let Some(start) = starts.next() {
        let Some(end) = match_at(text, start, needle) else {
            continue;
        };
        out.push(start..end);
        // Matches do not overlap: resume after this one.
        while starts.peek().is_some_and(|next| *next < end) {
            starts.next();
        }
    }
    out
}

/// Where a match of `needle` starting at byte `start` ends, if one does.
fn match_at(text: &str, start: usize, needle: &[char]) -> Option<usize> {
    let mut wanted = needle.iter();
    let mut pending = wanted.next();
    for (offset, ch) in text[start..].char_indices() {
        for lower in ch.to_lowercase() {
            match pending {
                Some(want) if *want == lower => pending = wanted.next(),
                _ => return None,
            }
        }
        if pending.is_none() {
            return Some(start + offset + ch.len_utf8());
        }
    }
    None
}

/// The match to show first: the first one at or after `(block, offset)`,
/// wrapping to the top of the note.
pub(crate) fn first_from(matches: &[Match], block: usize, offset: usize) -> Option<usize> {
    if matches.is_empty() {
        return None;
    }
    Some(
        matches
            .iter()
            .position(|m| (m.block, m.range.start) >= (block, offset))
            .unwrap_or(0),
    )
}

/// The next (or previous) match after `current`, wrapping around.
pub(crate) fn step(current: Option<usize>, len: usize, forward: bool) -> Option<usize> {
    if len == 0 {
        return None;
    }
    Some(match (current, forward) {
        (None, true) => 0,
        (None, false) => len - 1,
        (Some(at), true) => (at + 1) % len,
        (Some(at), false) => (at + len - 1) % len,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use diri_notes::markdown;

    fn blocks(text: &str) -> Vec<Block> {
        markdown::parse(text).1.blocks
    }

    fn found(blocks: &[Block], query: &str) -> Vec<(usize, String)> {
        find_all(blocks, query)
            .into_iter()
            .map(|m| (m.block, blocks[m.block].text[m.range].to_owned()))
            .collect()
    }

    #[test]
    fn ignores_case_and_keeps_reading_order() {
        let doc = blocks("Intro\n\nShip the plan, the PLAN.\n\n- [ ] Plan B\n");
        let hits = found(&doc, "pLaN");
        assert_eq!(hits.len(), 3, "{hits:?}");
        assert_eq!(hits[0].1, "plan");
        assert_eq!(hits[1].1, "PLAN");
        assert_eq!(hits[2].1, "Plan");
        assert!(hits.windows(2).all(|w| w[0].0 <= w[1].0));
    }

    #[test]
    fn matches_do_not_overlap() {
        let doc = blocks("aaaa\n");
        assert_eq!(found(&doc, "aa").len(), 2);
    }

    #[test]
    fn empty_query_matches_nothing() {
        let doc = blocks("anything\n");
        assert!(find_all(&doc, "").is_empty());
    }

    #[test]
    fn byte_ranges_are_char_boundaries() {
        let doc = blocks("Crème brûlée, CRÈME\n");
        let hits = found(&doc, "crème");
        assert_eq!(hits.len(), 2, "{hits:?}");
        assert_eq!(hits[1].1, "CRÈME");
    }

    #[test]
    fn first_from_wraps_to_the_top() {
        let doc = blocks("one\n\ntwo one\n\nthree\n");
        let matches = find_all(&doc, "one");
        assert_eq!(matches.len(), 2);
        let last = doc.len() - 1;
        assert_eq!(first_from(&matches, 0, 0), Some(0));
        assert_eq!(
            first_from(&matches, matches[1].block, matches[1].range.start),
            Some(1)
        );
        assert_eq!(first_from(&matches, last, 0), Some(0));
        assert_eq!(first_from(&[], 0, 0), None);
    }

    #[test]
    fn step_wraps_both_ways() {
        assert_eq!(step(Some(2), 3, true), Some(0));
        assert_eq!(step(Some(0), 3, false), Some(2));
        assert_eq!(step(None, 3, false), Some(2));
        assert_eq!(step(None, 0, true), None);
    }
}
