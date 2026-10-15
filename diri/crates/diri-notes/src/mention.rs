//! Mentions: `@` references from a note to a Diri session or another note.
//!
//! A mention is an ordinary Markdown link whose target uses the `diri:`
//! scheme — `[@Codex: fix resize](diri://session/s_1a2b)` or
//! `[@Release plan](diri://note/20260930-142501-3fa9)` — so a note stays
//! readable anywhere, and agents find related sessions by scanning links.
//! The editor renders these links as chips; nothing else in the model is
//! special.

use std::ops::Range;

use crate::doc::{Block, Document, Style};

const SESSION_PREFIX: &str = "diri://session/";
const NOTE_PREFIX: &str = "diri://note/";
/// Session and note ids are both short ASCII tokens; anything longer or
/// stranger is not a mention we wrote.
const MAX_ID_LEN: usize = 128;

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum MentionTarget {
    Session(String),
    Note(String),
}

impl MentionTarget {
    /// Reads a mention link target. `None` for every other URL.
    pub fn parse(url: &str) -> Option<Self> {
        let target = if let Some(id) = url.strip_prefix(SESSION_PREFIX) {
            Self::Session(id.to_owned())
        } else {
            Self::Note(url.strip_prefix(NOTE_PREFIX)?.to_owned())
        };
        valid_id(target.id()).then_some(target)
    }

    pub fn url(&self) -> String {
        match self {
            Self::Session(id) => format!("{SESSION_PREFIX}{id}"),
            Self::Note(id) => format!("{NOTE_PREFIX}{id}"),
        }
    }

    pub fn id(&self) -> &str {
        match self {
            Self::Session(id) | Self::Note(id) => id,
        }
    }
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_ID_LEN
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && !id.starts_with('.')
}

/// The visible text of a session mention: `@Codex: fix resize`.
pub fn session_label(agent: &str, title: &str) -> String {
    let title = one_line(title);
    match (agent.trim().is_empty(), title.is_empty()) {
        (true, true) => "@Session".to_owned(),
        (true, false) => format!("@{title}"),
        (false, true) => format!("@{}", agent.trim()),
        (false, false) => format!("@{}: {title}", agent.trim()),
    }
}

/// How people know an agent kind: `claude-code` is "Claude Code". The same
/// name the New Agent menu shows, so a chip written by an agent reads like
/// one the app wrote.
pub fn agent_display_name(kind_id: &str) -> String {
    match kind_id.trim().to_ascii_lowercase().as_str() {
        "claude-code" | "claude" => "Claude Code".to_owned(),
        "codex" => "Codex".to_owned(),
        "cursor" => "Cursor".to_owned(),
        "fx" => "fx".to_owned(),
        "gemini" => "Gemini".to_owned(),
        "opencode" => "OpenCode".to_owned(),
        "whipcode" => "WhipCode".to_owned(),
        "shell" => "Terminal".to_owned(),
        other => {
            let mut chars = other.chars();
            chars.next().map_or_else(String::new, |first| {
                first.to_uppercase().collect::<String>() + chars.as_str()
            })
        }
    }
}

/// The visible text of a note mention: `@Release plan`.
pub fn note_label(title: &str) -> String {
    let title = one_line(title);
    if title.is_empty() {
        "@Untitled".to_owned()
    } else {
        format!("@{title}")
    }
}

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// One mention found in a block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mention {
    pub target: MentionTarget,
    /// Byte range of the chip text within the block.
    pub range: Range<usize>,
}

/// Mention chips in a block, in text order.
pub fn in_block(block: &Block) -> Vec<Mention> {
    block
        .marks
        .iter()
        .filter_map(|mark| match &mark.style {
            Style::Link(url) => MentionTarget::parse(url).map(|target| Mention {
                target,
                range: mark.range.clone(),
            }),
            _ => None,
        })
        .collect()
}

/// The mention whose chip covers or ends at `offset`, for atomic deletion
/// and caret stepping. `inclusive_end` counts a caret sitting just after it.
pub fn at(block: &Block, offset: usize, inclusive_end: bool) -> Option<Mention> {
    in_block(block).into_iter().find(|m| {
        m.range.start < offset && (offset < m.range.end || inclusive_end && offset == m.range.end)
    })
}

impl Document {
    /// Every distinct mention target, in first-appearance order. This is
    /// what an agent reads to find the sessions and notes a note relates to.
    pub fn mentions(&self) -> Vec<MentionTarget> {
        let mut seen = Vec::new();
        for block in &self.blocks {
            for mention in in_block(block) {
                if !seen.contains(&mention.target) {
                    seen.push(mention.target);
                }
            }
        }
        seen
    }
}

/// Something the `@` menu can offer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub target: MentionTarget,
    /// Chip text, including the leading `@`.
    pub label: String,
    /// Extra words the query may match (agent name, project, folder).
    pub keywords: String,
}

/// Candidates matching `query`, best first. Prefix matches of any word beat
/// substring matches; ties keep the caller's order (most recent first).
pub fn rank<'a>(query: &str, candidates: &'a [Candidate], limit: usize) -> Vec<&'a Candidate> {
    let query = query.trim().to_lowercase();
    let mut scored: Vec<(u8, usize, &Candidate)> = candidates
        .iter()
        .enumerate()
        .filter_map(|(order, candidate)| {
            if query.is_empty() {
                return Some((0, order, candidate));
            }
            let label = candidate.label.trim_start_matches('@').to_lowercase();
            let keywords = candidate.keywords.to_lowercase();
            let words = || {
                label
                    .split(|c: char| !c.is_alphanumeric())
                    .chain(keywords.split(|c: char| !c.is_alphanumeric()))
            };
            let score = if label.starts_with(&query) {
                0
            } else if words().any(|w| w.starts_with(&query)) {
                1
            } else if label.contains(&query) || keywords.contains(&query) {
                2
            } else {
                return None;
            };
            Some((score, order, candidate))
        })
        .collect();
    scored.sort_by_key(|(score, order, _)| (*score, *order));
    scored.into_iter().take(limit).map(|(_, _, c)| c).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::BlockKind;
    use crate::markdown;

    #[test]
    fn parses_only_well_formed_diri_links() {
        assert_eq!(
            MentionTarget::parse("diri://session/s_4e97a43bd495"),
            Some(MentionTarget::Session("s_4e97a43bd495".into()))
        );
        assert_eq!(
            MentionTarget::parse("diri://note/20260930-142501-3fa9"),
            Some(MentionTarget::Note("20260930-142501-3fa9".into()))
        );
        for bad in [
            "https://diri.sh",
            "diri://session/",
            "diri://session/a b",
            "diri://session/../x",
            "diri://session/a/b",
            "diri://note/.hidden",
            "diri://window/x",
        ] {
            assert_eq!(MentionTarget::parse(bad), None, "{bad}");
        }
        let target = MentionTarget::Session("s_1".into());
        assert_eq!(MentionTarget::parse(&target.url()), Some(target));
    }

    #[test]
    fn labels_are_single_line() {
        assert_eq!(session_label("Codex", "fix\n resize"), "@Codex: fix resize");
        assert_eq!(session_label("", "t"), "@t");
        assert_eq!(session_label("Codex", " "), "@Codex");
        assert_eq!(note_label(""), "@Untitled");
    }

    #[test]
    fn a_mention_is_a_plain_markdown_link_that_round_trips() {
        let label = "@Codex: fix [resize] *now*";
        let mut block = Block::new(0, BlockKind::Paragraph, format!("see {label} and"));
        block.add_mark(
            4..4 + label.len(),
            Style::Link("diri://session/s_4e97a43bd495".into()),
        );
        let mut other = Block::new(0, BlockKind::Todo { checked: false }, "@Plan");
        other.add_mark(0..5, Style::Link("diri://note/20260930-142501-3fa9".into()));
        let doc = Document::new("T", vec![block, other]);
        let text = markdown::write(&markdown::FrontMatter::default(), &doc);
        assert!(
            text.contains("](diri://session/s_4e97a43bd495)"),
            "stored as a link:\n{text}"
        );
        let (_, parsed) = markdown::parse(&text);
        assert!(parsed.same_content(&doc), "{text}\n{parsed:#?}");
        assert_eq!(
            parsed.mentions(),
            vec![
                MentionTarget::Session("s_4e97a43bd495".into()),
                MentionTarget::Note("20260930-142501-3fa9".into()),
            ]
        );
    }

    #[test]
    fn hand_written_markdown_mentions_are_found() {
        let (_, doc) = markdown::parse(
            "# Plan\n\nWait for [@Codex: fix resize](diri://session/s_1) and [docs](https://x.dev).\n\n- [ ] [again](diri://session/s_1) [@n](diri://note/n-1)\n",
        );
        assert_eq!(
            doc.mentions(),
            vec![
                MentionTarget::Session("s_1".into()),
                MentionTarget::Note("n-1".into())
            ]
        );
    }

    #[test]
    fn ranking_prefers_prefixes_and_keeps_order() {
        let c = |id: &str, label: &str, keywords: &str| Candidate {
            target: MentionTarget::Session(id.into()),
            label: label.into(),
            keywords: keywords.into(),
        };
        let all = vec![
            c("1", "@Claude Code: tidy sidebar", "claude diri"),
            c("2", "@Codex: fix resize", "codex diri"),
            c("3", "@Codex: resize tests", "codex"),
            c("4", "@Terminal", "shell"),
        ];
        let ids = |q: &str| -> Vec<String> {
            rank(q, &all, 10)
                .iter()
                .map(|c| c.target.id().to_owned())
                .collect()
        };
        assert_eq!(ids(""), ["1", "2", "3", "4"]);
        assert_eq!(ids("codex"), ["2", "3"]);
        assert_eq!(ids("resize"), ["2", "3"]);
        assert_eq!(ids("res"), ["2", "3"]);
        assert_eq!(ids("shell"), ["4"]);
        assert_eq!(ids("izE"), ["2", "3"]);
        assert!(ids("zzz").is_empty());
        assert_eq!(rank("", &all, 2).len(), 2);
    }
}
