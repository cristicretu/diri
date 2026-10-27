//! To-dos as tracked agent work.
//!
//! A to-do becomes a work item when a person starts an agent from it. The
//! file keeps only what a person could write by hand:
//!
//! - the to-do's indented children are its **context**;
//! - a trailing session mention in the to-do's text links each attempt,
//!   newest last;
//! - children that begin with the chip of one of those sessions are the
//!   agent's **updates**;
//! - the checkbox is ticked by a person, never by an agent.
//!
//! Everything else (whether the agent is working, waiting on the person, or
//! ready for review) is derived live by [`state`] from the Engine's session
//! record and never written. Functions here take `&[Block]` so the editor,
//! whose block 0 is the title, and a stored [`Document`] share them.

use std::ops::Range;

use diri_proto::{SessionRecord, SessionStatus};

use crate::doc::{Block, BlockKind, Document, MAX_INDENT, floor_boundary};
use crate::markdown::{self, FrontMatter};
use crate::mention::{self, MentionTarget};
use crate::store::Note;

/// The whole prompt an agent receives from a to-do stays under this.
pub const BRIEF_BYTES: usize = 24 * 1024;
const CONTEXT_BYTES: usize = 12 * 1024;
const SECTION_BYTES: usize = 4 * 1024;
const NOTE_EXCERPT_BYTES: usize = 2 * 1024;
const MAX_NOTES: usize = 4;
const UPDATES_BYTES: usize = 2 * 1024;
/// Sessions started from a to-do are titled with its text, this long at most.
const TITLE_CHARS: usize = 60;

/// Block indices of `index`'s children: the list blocks right after it that
/// sit deeper than it. Empty for anything that is not a list block.
pub fn children(blocks: &[Block], index: usize) -> Range<usize> {
    let start = index + 1;
    let Some(parent) = blocks.get(index).filter(|block| block.kind.is_list()) else {
        return start..start;
    };
    let mut end = start;
    while blocks
        .get(end)
        .is_some_and(|block| block.kind.is_list() && block.indent > parent.indent)
    {
        end += 1;
    }
    start..end
}

/// Sessions linked to a to-do, in text order (oldest attempt first).
pub fn sessions(block: &Block) -> Vec<String> {
    if !matches!(block.kind, BlockKind::Todo { .. }) {
        return Vec::new();
    }
    let mut out = Vec::new();
    for mention in mention::in_block(block) {
        if let MentionTarget::Session(id) = mention.target
            && !out.contains(&id)
        {
            out.push(id);
        }
    }
    out
}

/// The session a to-do tracks: its newest attempt.
pub fn current_session(block: &Block) -> Option<String> {
    sessions(block).pop()
}

/// Whether `child` is an update written by one of `sessions`: it begins with
/// that session's chip.
pub fn is_update(child: &Block, sessions: &[String]) -> bool {
    mention::in_block(child).first().is_some_and(|first| {
        first.range.start == 0
            && matches!(&first.target, MentionTarget::Session(id) if sessions.contains(id))
    })
}

/// The block index of the to-do that links `session_id`, preferring the one
/// where it is the newest attempt.
pub fn todo_for_session(blocks: &[Block], session_id: &str) -> Option<usize> {
    let linked = |block: &Block| sessions(block).iter().any(|id| id == session_id);
    blocks
        .iter()
        .position(|block| current_session(block).as_deref() == Some(session_id))
        .or_else(|| blocks.iter().position(linked))
}

/// The to-do's own words, without its session chips, as one line of
/// Markdown.
pub fn task_markdown(block: &Block) -> String {
    markdown::write_inline(&without_session_chips(block), false)
        .trim()
        .to_owned()
}

/// The to-do's plain text without its session chips, trimmed: what
/// identifies it across reloads and in the file.
pub fn task_text(block: &Block) -> String {
    without_session_chips(block).text.trim().to_owned()
}

/// A title for the session started from `block`: its plain text, one line,
/// [`TITLE_CHARS`] at most.
pub fn task_title(block: &Block) -> String {
    let plain = without_session_chips(block).text;
    let one_line = plain.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= TITLE_CHARS {
        return one_line;
    }
    let cut: String = one_line.chars().take(TITLE_CHARS - 1).collect();
    format!("{}…", cut.trim_end())
}

fn without_session_chips(block: &Block) -> Block {
    let mut out = block.clone();
    let mut chips: Vec<Range<usize>> = mention::in_block(block)
        .into_iter()
        .filter(|m| matches!(m.target, MentionTarget::Session(_)))
        .map(|m| m.range)
        .collect();
    chips.sort_by_key(|range| std::cmp::Reverse(range.start));
    for range in chips {
        out.replace(range, "", &[]);
    }
    let trimmed = out.text.trim_end().len();
    let len = out.text.len();
    out.replace(trimmed..len, "", &[]);
    out
}

// ---------------------------------------------------------------------------
// Live state

/// What the Engine reports about one session, reduced to what a work item
/// shows. Built from a [`SessionRecord`] by the app and the MCP bridge alike.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionFacts {
    pub activity: Activity,
    /// The agent finished at least one turn since it started.
    pub turn_completed: bool,
    pub archived: bool,
    /// The question the agent is waiting on, when it is waiting.
    pub question: Option<Question>,
    /// An open pull request the session made.
    pub pull_request: Option<PullRequest>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Activity {
    #[default]
    Starting,
    Working,
    Idle,
    NeedsInput,
    /// The process ended; `clean` for a normal exit with status 0 (or none).
    Exited {
        clean: bool,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Question {
    pub summary: String,
    pub excerpt: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PullRequest {
    pub number: i64,
    pub url: String,
}

impl SessionFacts {
    pub fn from_record(record: &SessionRecord) -> Self {
        let activity = match &record.status {
            SessionStatus::Starting | SessionStatus::Unknown => Activity::Starting,
            SessionStatus::Working => Activity::Working,
            SessionStatus::Idle => Activity::Idle,
            SessionStatus::NeedsInput(_) => Activity::NeedsInput,
            SessionStatus::Exited(exit) => Activity::Exited {
                clean: exit.reason == diri_proto::ExitReason::Exited
                    && exit.code.is_none_or(|code| code == 0),
            },
        };
        let question = (activity == Activity::NeedsInput).then(|| {
            let detail = record.needs_input.as_ref();
            Question {
                summary: detail
                    .map(|d| d.summary.trim().to_owned())
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| "The agent is waiting for you.".to_owned()),
                excerpt: detail
                    .and_then(|d| d.prompt_excerpt.as_deref())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned),
            }
        });
        let pull_request = record.pull_requests.as_ref().and_then(|prs| {
            prs.iter()
                .rev()
                .find(|pr| pr.state.eq_ignore_ascii_case("open"))
                .map(|pr| PullRequest {
                    number: pr.number,
                    url: pr.url.clone(),
                })
        });
        Self {
            activity,
            turn_completed: record.last_turn_completed_at.is_some(),
            archived: record.is_archived(),
            question,
            pull_request,
        }
    }
}

/// Where a to-do's work stands, derived live and never stored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkState {
    /// Nothing started yet.
    Ready,
    Starting,
    Working,
    NeedsYou(Question),
    /// The agent finished; a person looks before ticking.
    Review(Option<PullRequest>),
    /// The agent ended before finishing anything.
    Stopped,
    Archived,
    /// The linked session is not known to the Engine.
    Missing,
    Done,
}

impl WorkState {
    /// Plain words, for everyone who uses Diri and not only developers.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Ready => "Start",
            Self::Starting => "Starting…",
            Self::Working => "Working on it",
            Self::NeedsYou(_) => "Needs you",
            Self::Review(_) => "Ready to review",
            Self::Stopped => "Stopped",
            Self::Archived => "Session archived",
            Self::Missing => "Session not found",
            Self::Done => "Done",
        }
    }

    /// The agent may still change things: ticking should ask first.
    pub fn is_running(&self) -> bool {
        matches!(self, Self::Starting | Self::Working | Self::NeedsYou(_))
    }

    /// Nothing is running and another attempt makes sense.
    pub fn can_restart(&self) -> bool {
        matches!(
            self,
            Self::Ready | Self::Stopped | Self::Archived | Self::Missing
        )
    }
}

/// The state of a to-do given what is known about its newest session:
/// `None` when nothing is linked, `Some(None)` when the Engine does not
/// know the linked session.
pub fn state(checked: bool, session: Option<Option<&SessionFacts>>) -> WorkState {
    if checked {
        return WorkState::Done;
    }
    let Some(facts) = session else {
        return WorkState::Ready;
    };
    let Some(facts) = facts else {
        return WorkState::Missing;
    };
    if facts.archived {
        return WorkState::Archived;
    }
    match facts.activity {
        Activity::Starting => WorkState::Starting,
        Activity::Working => WorkState::Working,
        Activity::NeedsInput => WorkState::NeedsYou(facts.question.clone().unwrap_or(Question {
            summary: "The agent is waiting for you.".to_owned(),
            excerpt: None,
        })),
        // An agent at its prompt before its first turn has not started.
        Activity::Idle if !facts.turn_completed => WorkState::Starting,
        Activity::Idle => WorkState::Review(facts.pull_request.clone()),
        Activity::Exited { clean: true } if facts.turn_completed => {
            WorkState::Review(facts.pull_request.clone())
        }
        Activity::Exited { .. } => WorkState::Stopped,
    }
}

// ---------------------------------------------------------------------------
// Writing back

/// Adds `text` from `session_id` as a child bullet under the to-do at
/// `todo`, after its other children, so an agent's progress folds with the
/// work it belongs to. Returns the new block's index.
pub fn append_todo_update(
    note: &mut Note,
    todo: usize,
    date: &str,
    label: &str,
    session_id: &str,
    text: &str,
) -> Option<usize> {
    let doc = &mut note.doc;
    let mut blocks = std::mem::take(&mut doc.blocks);
    let at = insert_todo_update(
        &mut blocks,
        todo,
        date,
        label,
        session_id,
        text,
        &mut || doc.fresh_id(),
    );
    doc.blocks = blocks;
    at
}

/// [`append_todo_update`] on any run of blocks (the editor's starts with the
/// title), with `fresh` handing out block ids.
pub fn insert_todo_update(
    blocks: &mut Vec<Block>,
    todo: usize,
    date: &str,
    label: &str,
    session_id: &str,
    text: &str,
    fresh: &mut dyn FnMut() -> u64,
) -> Option<usize> {
    let block = blocks.get(todo)?;
    if !matches!(block.kind, BlockKind::Todo { .. }) {
        return None;
    }
    // Under its own to-do the agent's name is enough; the to-do is its title.
    let label = label.split(':').next().unwrap_or(label).trim();
    let indent = (block.indent + 1).min(MAX_INDENT);
    let at = children(blocks, todo).end;
    let (head, details) = update_lines(text);
    let mut line = Block::new(0, BlockKind::Bullet, "");
    let target = MentionTarget::Session(session_id.to_owned());
    line.replace(0..0, label, &[]);
    line.add_mark(0..label.len(), crate::doc::Style::Link(target.url()));
    let start = line.text.len();
    let stamp = if date.is_empty() {
        String::new()
    } else {
        format!(" · {date}")
    };
    line.replace(start..start, &format!("{stamp} — {head}"), &[]);
    line.indent = indent;
    line.id = fresh();
    blocks.insert(at, line);
    for (offset, detail) in details.iter().enumerate() {
        let (text, marks) = crate::markdown::parse_inline(detail);
        let mut child = Block::new(0, BlockKind::Bullet, text);
        child.marks = marks;
        child.normalize();
        child.indent = (indent + 1).min(MAX_INDENT);
        child.id = fresh();
        blocks.insert(at + 1 + offset, child);
    }
    Some(at)
}

/// Whether the to-do at `todo` already holds an update from `session_id`:
/// a child line that starts with that session's chip.
pub fn has_update_from(blocks: &[Block], todo: usize, session_id: &str) -> bool {
    let own = [session_id.to_owned()];
    blocks[children(blocks, todo)]
        .iter()
        .any(|child| is_update(child, &own))
}

#[cfg(test)]
mod update_lines_tests {
    #[test]
    fn a_report_with_a_list_becomes_a_headline_and_points() {
        let (head, details) = super::update_lines(
            "Done: there are 3 docs:\n\n- faq.md: how to cancel\n- pricing.md: $12/mo\n2. roadmap.md: a stub",
        );
        assert_eq!(head, "Done: there are 3 docs:");
        assert_eq!(
            details,
            [
                "faq.md: how to cancel",
                "pricing.md: $12/mo",
                "roadmap.md: a stub"
            ]
        );
    }
}

/// Splits an agent's report into its headline and its detail lines, so a
/// multi-line result reads as a bullet with nested points instead of one
/// run-on line. List markers on detail lines are dropped (they become
/// bullets); blank lines are skipped.
pub fn update_lines(text: &str) -> (String, Vec<String>) {
    let mut head: Vec<String> = Vec::new();
    let mut details = Vec::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        let item = ["- ", "* ", "+ "]
            .iter()
            .find_map(|marker| line.strip_prefix(marker))
            .or_else(|| {
                let digits = line.bytes().take_while(u8::is_ascii_digit).count();
                (digits > 0)
                    .then(|| line[digits..].strip_prefix(". "))
                    .flatten()
            });
        match item {
            Some(item) => details.push(item.trim().to_owned()),
            None if details.is_empty() => head.push(line.to_owned()),
            None => details.push(line.to_owned()),
        }
    }
    if head.is_empty() && !details.is_empty() {
        head.push(details.remove(0));
    }
    (head.join(" "), details)
}

// ---------------------------------------------------------------------------
// The brief: what an agent started from a to-do receives

/// A note mentioned in the to-do's context, loaded by the caller.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedNote {
    pub id: String,
    pub title: String,
    /// The note's Markdown body without front matter.
    pub body: String,
}

/// A session mentioned in the to-do's context, as the caller knows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedSession {
    pub id: String,
    pub kind: String,
    pub title: String,
    pub status: String,
}

/// Exactly what an agent receives, plus the counts the Start panel shows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Brief {
    pub prompt: String,
    pub context_lines: usize,
    pub links: usize,
    pub notes: usize,
    pub sessions: usize,
    /// Some part was cut to stay under [`BRIEF_BYTES`].
    pub truncated: bool,
}

/// Notes and sessions the to-do and its context mention, excluding the
/// to-do's own attempts; the caller resolves these for [`brief`].
pub fn mentions(blocks: &[Block], todo: usize) -> Vec<MentionTarget> {
    let Some(block) = blocks.get(todo) else {
        return Vec::new();
    };
    let own = sessions(block);
    let mut out = Vec::new();
    let context = context_blocks(blocks, todo);
    for block in std::iter::once(block).chain(context.iter().copied()) {
        for mention in mention::in_block(block) {
            let own_attempt =
                matches!(&mention.target, MentionTarget::Session(id) if own.contains(id));
            if !own_attempt && !out.contains(&mention.target) {
                out.push(mention.target);
            }
        }
    }
    out
}

/// Splits a to-do's children into the person's context and its agents'
/// updates. An update's nested lines (details from a multi-line report)
/// belong to the update, not to the context.
fn partition(blocks: &[Block], todo: usize) -> (Vec<&Block>, Vec<&Block>) {
    let own = blocks.get(todo).map(sessions).unwrap_or_default();
    let mut context = Vec::new();
    let mut updates = Vec::new();
    let mut inside_update: Option<u8> = None;
    for child in &blocks[children(blocks, todo)] {
        if let Some(depth) = inside_update {
            if child.indent > depth {
                updates.push(child);
                continue;
            }
            inside_update = None;
        }
        if is_update(child, &own) {
            inside_update = Some(child.indent);
            updates.push(child);
        } else {
            context.push(child);
        }
    }
    (context, updates)
}

fn context_blocks(blocks: &[Block], todo: usize) -> Vec<&Block> {
    partition(blocks, todo).0
}

fn update_blocks(blocks: &[Block], todo: usize) -> Vec<&Block> {
    partition(blocks, todo).1
}

/// Builds the prompt for an agent started from the to-do at `todo` (a
/// document block index) in `note`. The prompt is the whole contract: the
/// Start panel shows it verbatim.
pub fn brief(
    note_id: &str,
    note: &Note,
    todo: usize,
    notes: &[ResolvedNote],
    sessions: &[ResolvedSession],
) -> Brief {
    let blocks = &note.doc.blocks;
    let Some(block) = blocks.get(todo) else {
        return Brief::default();
    };
    let title = note_title(&note.doc);
    let mut brief = Brief::default();
    let mut out = format!(
        "Your task is this to-do from the Diri note \"{title}\":\n\n{}\n",
        task_markdown(block)
    );

    let context = context_blocks(blocks, todo);
    if !context.is_empty() {
        brief.context_lines = context.len();
        brief.links = context
            .iter()
            .flat_map(|b| b.marks.iter())
            .filter(|m| {
                matches!(&m.style, crate::doc::Style::Link(url) if MentionTarget::parse(url).is_none())
            })
            .count();
        let text = outline(&context);
        out.push_str("\nContext the person gave with it:\n");
        out.push_str(&capped(&text, CONTEXT_BYTES, note_id, &mut brief.truncated));
    }

    let wanted = mentions(blocks, todo);
    let linked_notes: Vec<&ResolvedNote> = wanted
        .iter()
        .filter_map(|target| match target {
            MentionTarget::Note(id) => notes.iter().find(|n| &n.id == id),
            MentionTarget::Session(_) => None,
        })
        .take(MAX_NOTES)
        .collect();
    if !linked_notes.is_empty() {
        brief.notes = linked_notes.len();
        out.push_str("\nNotes it mentions:\n");
        for linked in linked_notes {
            out.push_str(&format!(
                "\n--- note \"{}\" ({}) ---\n",
                one_line(&linked.title),
                linked.id
            ));
            out.push_str(&capped(
                linked.body.trim(),
                NOTE_EXCERPT_BYTES,
                &linked.id,
                &mut brief.truncated,
            ));
        }
        out.push_str("--- end of notes ---\n");
    }
    let linked_sessions: Vec<&ResolvedSession> = wanted
        .iter()
        .filter_map(|target| match target {
            MentionTarget::Session(id) => sessions.iter().find(|s| &s.id == id),
            MentionTarget::Note(_) => None,
        })
        .collect();
    if !linked_sessions.is_empty() {
        brief.sessions = linked_sessions.len();
        out.push_str(
            "\nDiri sessions it mentions. Look at them with read_output, get_diff or \
             get_status before you repeat their work:\n",
        );
        for session in linked_sessions {
            out.push_str(&format!(
                "- {} ({}, {}): {}\n",
                session.id,
                session.kind,
                session.status,
                one_line(&session.title)
            ));
        }
    }

    let section = section_markdown(blocks, todo);
    if !section.trim().is_empty() {
        out.push_str("\nWhere it sits in the note:\n");
        out.push_str(&capped(
            &section,
            SECTION_BYTES,
            note_id,
            &mut brief.truncated,
        ));
    }

    let updates = update_blocks(blocks, todo);
    if !updates.is_empty() {
        out.push_str("\nAn earlier attempt reported:\n");
        out.push_str(&capped(
            &outline(&updates),
            UPDATES_BYTES,
            note_id,
            &mut brief.truncated,
        ));
    }

    out.push_str(&format!(
        "\nThis note is your parent in Diri (note \"{note_id}\"; read_note \"origin\" \
         shows all of it). Post progress and your result with report_to_parent: each \
         report appears under this to-do. Do not tick the to-do; the person reviews your \
         work and ticks it. If the result is text, put it in your final report so it \
         can be read from the note.\n"
    ));
    if out.len() > BRIEF_BYTES {
        let cut = floor_boundary(&out, BRIEF_BYTES - 128);
        out.truncate(cut);
        out.push_str(&format!(
            "\n[… cut to stay short; read_note \"{note_id}\" has the rest]\n"
        ));
        brief.truncated = true;
    }
    brief.prompt = out;
    brief
}

fn note_title(doc: &Document) -> String {
    let title = one_line(&doc.title);
    if title.is_empty() {
        "Untitled".to_owned()
    } else {
        title
    }
}

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Blocks as Markdown list lines, re-indented so the shallowest is flush.
fn outline(blocks: &[&Block]) -> String {
    let base = blocks.iter().map(|b| b.indent).min().unwrap_or(0);
    let owned: Vec<Block> = blocks
        .iter()
        .map(|block| {
            let mut block = (*block).clone();
            block.indent -= base;
            block
        })
        .collect();
    markdown::write(&FrontMatter::default(), &Document::new("", owned))
}

/// The section around the to-do: from the nearest heading above it to the
/// next heading of the same or a higher level, without the to-do and its
/// children (they are already the task and its context).
fn section_markdown(blocks: &[Block], todo: usize) -> String {
    let heading = blocks[..todo]
        .iter()
        .rposition(|b| matches!(b.kind, BlockKind::Heading(_)));
    let (start, level) = match heading {
        Some(index) => match blocks[index].kind {
            BlockKind::Heading(level) => (index, level),
            _ => unreachable!(),
        },
        None => (0, 0),
    };
    let end = blocks[todo + 1..]
        .iter()
        .position(|b| matches!(b.kind, BlockKind::Heading(l) if level == 0 || l <= level))
        .map_or(blocks.len(), |offset| todo + 1 + offset);
    let skip = todo..children(blocks, todo).end;
    let kept: Vec<Block> = (start..end)
        .filter(|index| !skip.contains(index))
        .map(|index| blocks[index].clone())
        .filter(|b| !(b.kind == BlockKind::Paragraph && b.text.is_empty()))
        .collect();
    if kept.iter().all(|b| matches!(b.kind, BlockKind::Heading(_))) {
        return String::new();
    }
    markdown::write(&FrontMatter::default(), &Document::new("", kept))
}

fn capped(text: &str, limit: usize, note_id: &str, truncated: &mut bool) -> String {
    let mut out = if text.len() > limit {
        *truncated = true;
        let cut = floor_boundary(text, limit);
        format!(
            "{}\n[… cut; read_note \"{note_id}\" for the rest]",
            text[..cut].trim_end()
        )
    } else {
        text.trim_end().to_owned()
    };
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::parse_note;

    const LAUNCH: &str = "---\nid: n1\n---\n# Launch plan\n\n\
        ## Marketing\n\n\
        We ship on Thursday.\n\n\
        - [ ] Draft 3 LinkedIn posts for the launch\n\
        \x20 - Tone: plain, confident, no emojis\n\
        \x20 - [Launch brief](https://example.com/brief)\n\
        \x20 - Voice like [@Brand voice](diri://note/n-voice)\n\
        - [ ] Book the venue\n\n\
        ## Engineering\n\n\
        - [ ] Fix resize flicker\n";

    fn launch() -> Note {
        parse_note(LAUNCH)
    }

    fn index_of(note: &Note, text: &str) -> usize {
        note.doc
            .blocks
            .iter()
            .position(|b| b.text.starts_with(text))
            .expect(text)
    }

    #[test]
    fn children_are_the_deeper_list_blocks_right_after() {
        let note = launch();
        let posts = index_of(&note, "Draft 3");
        assert_eq!(children(&note.doc.blocks, posts), posts + 1..posts + 4);
        let venue = index_of(&note, "Book");
        assert!(children(&note.doc.blocks, venue).is_empty());
        let para = index_of(&note, "We ship");
        assert!(children(&note.doc.blocks, para).is_empty());
    }

    #[test]
    fn states_come_from_the_newest_session() {
        let facts = |activity, turn_completed| SessionFacts {
            activity,
            turn_completed,
            ..SessionFacts::default()
        };
        assert_eq!(state(false, None), WorkState::Ready);
        assert_eq!(state(true, None), WorkState::Done);
        assert_eq!(state(false, Some(None)), WorkState::Missing);
        assert_eq!(
            state(false, Some(Some(&facts(Activity::Idle, false)))),
            WorkState::Starting,
            "an agent at its prompt before its first turn has not finished"
        );
        assert_eq!(
            state(false, Some(Some(&facts(Activity::Working, false)))),
            WorkState::Working
        );
        assert_eq!(
            state(false, Some(Some(&facts(Activity::Idle, true)))),
            WorkState::Review(None)
        );
        assert_eq!(
            state(
                false,
                Some(Some(&facts(Activity::Exited { clean: true }, true)))
            ),
            WorkState::Review(None)
        );
        assert_eq!(
            state(
                false,
                Some(Some(&facts(Activity::Exited { clean: false }, true)))
            ),
            WorkState::Stopped
        );
        assert_eq!(
            state(
                false,
                Some(Some(&facts(Activity::Exited { clean: true }, false)))
            ),
            WorkState::Stopped
        );
        let mut archived = facts(Activity::Idle, true);
        archived.archived = true;
        assert_eq!(state(false, Some(Some(&archived))), WorkState::Archived);
        let mut waiting = facts(Activity::NeedsInput, false);
        waiting.question = Some(Question {
            summary: "Allow Bash: rm -rf build?".into(),
            excerpt: None,
        });
        let needs = state(false, Some(Some(&waiting)));
        assert!(matches!(&needs, WorkState::NeedsYou(q) if q.summary.contains("rm -rf")));
        assert!(needs.is_running());
        assert_eq!(needs.label(), "Needs you");
        // Ticked wins over everything.
        assert_eq!(state(true, Some(Some(&waiting))), WorkState::Done);
    }

    #[test]
    fn facts_from_a_record() {
        let mut record = test_record();
        record.status = SessionStatus::Idle;
        record.last_turn_completed_at = Some(diri_proto::DateMillis(5.0));
        record.pull_requests = Some(vec![test_pr(7, "OPEN"), test_pr(8, "MERGED")]);
        let facts = SessionFacts::from_record(&record);
        assert_eq!(facts.activity, Activity::Idle);
        assert!(facts.turn_completed);
        assert_eq!(facts.pull_request.as_ref().map(|pr| pr.number), Some(7));
        assert_eq!(
            state(false, Some(Some(&facts))),
            WorkState::Review(facts.pull_request.clone())
        );
        record.status = SessionStatus::Exited(diri_proto::ExitInfo {
            reason: diri_proto::ExitReason::Signaled,
            code: None,
            signal: Some(9),
            system_restart: false,
            interrupted: false,
        });
        assert_eq!(
            SessionFacts::from_record(&record).activity,
            Activity::Exited { clean: false }
        );
    }

    fn test_record() -> SessionRecord {
        use diri_proto::{AgentKind, DateMillis, ProjectId, Resumability, SessionId, TitleSource};
        SessionRecord {
            command_run: None,
            attention_state: None,
            id: SessionId::new("s_1"),
            kind: AgentKind::CLAUDE_CODE,
            cwd: "/tmp".into(),
            project_id: ProjectId::new("p"),
            worktree_path: None,
            git_branch: None,
            title: "t".into(),
            title_source: TitleSource::Placeholder,
            account_profile: None,
            originating_prompt: None,
            agent_session_id: None,
            transcript_path: None,
            status: SessionStatus::Working,
            status_evidence: None,
            needs_input: None,
            resumability: Resumability::Live,
            capabilities: None,
            parent: None,
            created_at: DateMillis(0.0),
            updated_at: DateMillis(0.0),
            last_turn_completed_at: None,
            last_seen_at: None,
            pinned: false,
            archived_at: None,
            host: None,
            remote_persistence: None,
            remote_connection: None,
            hibernation: None,
            memory_bytes: None,
            artifacts: None,
            pull_requests: None,
            listening_ports: None,
            foreground_agent: None,
            terminal_cwd: None,
            agent_workspace: None,
            foreground_ports: None,
            note_id: None,
            terminal_progress: None,
            scheduled_run: None,
        }
    }

    fn test_pr(number: i64, state: &str) -> diri_proto::PullRequestStatus {
        serde_json::from_value(serde_json::json!({
            "url": format!("https://github.com/o/r/pull/{number}"),
            "number": number, "state": state, "isDraft": false,
            "additions": 1, "deletions": 1, "changedFiles": 1, "commentCount": 0,
            "reviewCount": 0, "checksPassed": 0, "checksFailed": 0, "checksPending": 0,
            "fetchedAt": 0.0
        }))
        .unwrap()
    }

    #[test]
    fn brief_carries_the_task_context_mentions_and_section() {
        let note = launch();
        let posts = index_of(&note, "Draft 3");
        let voice = ResolvedNote {
            id: "n-voice".into(),
            title: "Brand voice".into(),
            body: "# Brand voice\n\nShort sentences. Say what it does.".into(),
        };
        let brief = brief("n1", &note, posts, &[voice], &[]);
        let p = &brief.prompt;
        assert!(
            p.starts_with(
                "Your task is this to-do from the Diri note \"Launch plan\":\n\n\
                 Draft 3 LinkedIn posts for the launch\n"
            ),
            "{p}"
        );
        assert!(p.contains("- Tone: plain, confident, no emojis\n"), "{p}");
        assert!(
            p.contains("- [Launch brief](https://example.com/brief)"),
            "{p}"
        );
        assert!(p.contains("Short sentences. Say what it does."), "{p}");
        assert!(p.contains("We ship on Thursday."), "section context: {p}");
        assert!(
            p.contains("Book the venue"),
            "sibling to-dos are context: {p}"
        );
        assert!(
            !p.contains("Fix resize flicker"),
            "other sections stay out: {p}"
        );
        assert!(p.contains("report_to_parent"));
        assert!(p.contains("Do not tick the to-do"));
        assert_eq!(brief.context_lines, 3);
        assert_eq!(brief.links, 1);
        assert_eq!(brief.notes, 1);
        assert!(!brief.truncated);
    }

    #[test]
    fn a_retry_sees_earlier_updates_apart_from_context() {
        let mut note = launch();
        let posts = index_of(&note, "Draft 3");
        crate::handoff::link_session(&mut note, posts, "@Claude", "s_a");
        append_todo_update(
            &mut note,
            posts,
            "2026-09-30",
            "@Claude",
            "s_a",
            "Drafted 1 of 3",
        );
        let blocks = &note.doc.blocks;
        assert_eq!(children(blocks, posts).len(), 4);
        assert_eq!(context_blocks(blocks, posts).len(), 3);
        let brief = brief("n1", &note, posts, &[], &[]);
        assert!(
            !brief
                .prompt
                .contains("[@Claude](diri://session/s_a)\n\nContext"),
            "{}",
            brief.prompt
        );
        assert!(brief.prompt.contains("An earlier attempt reported:\n- [@Claude](diri://session/s_a) · 2026-09-30 — Drafted 1 of 3"), "{}", brief.prompt);
        assert!(
            brief
                .prompt
                .contains("\n\nDraft 3 LinkedIn posts for the launch\n"),
            "task text drops the attempt chip: {}",
            brief.prompt
        );
        assert_eq!(brief.context_lines, 3);
        assert!(
            !mentions(blocks, posts).contains(&MentionTarget::Session("s_a".into())),
            "the to-do's own attempt is not related work"
        );
    }

    #[test]
    fn brief_is_bounded_and_says_so() {
        let mut source = String::from("# Big\n\n- [ ] Summarise\n");
        for i in 0..2000 {
            source.push_str(&format!("  - context line number {i} with some words\n"));
        }
        let note = parse_note(&source);
        let brief = brief("big", &note, 0, &[], &[]);
        assert!(brief.truncated);
        assert!(brief.prompt.len() <= BRIEF_BYTES, "{}", brief.prompt.len());
        assert!(brief.prompt.contains("read_note \"big\" for the rest"));
        assert!(
            brief.prompt.contains("report_to_parent"),
            "instructions survive the cut"
        );
    }

    #[test]
    fn updates_fold_under_their_todo_and_round_trip() {
        let mut note = launch();
        let posts = index_of(&note, "Draft 3");
        crate::handoff::link_session(&mut note, posts, "@Claude: Draft", "s_a");
        let at = append_todo_update(
            &mut note,
            posts,
            "14:02",
            "@Claude: Draft",
            "s_a",
            "Drafted\npost 1",
        )
        .unwrap();
        assert_eq!(at, posts + 4);
        append_todo_update(
            &mut note,
            posts,
            "14:09",
            "@Claude: Draft",
            "s_a",
            "All three done",
        )
        .unwrap();
        let source = note.to_markdown();
        assert!(
            source.contains(
                "  - Voice like [@Brand voice](diri://note/n-voice)\n  - [@Claude](diri://session/s_a) · 14:02 — Drafted post 1\n  - [@Claude](diri://session/s_a) · 14:09 — All three done\n- [ ] Book the venue"
            ),
            "{source}"
        );
        let back = parse_note(&source);
        let posts = index_of(&back, "Draft 3");
        assert_eq!(update_blocks(&back.doc.blocks, posts).len(), 2);
        assert_eq!(todo_for_session(&back.doc.blocks, "s_a"), Some(posts));
        assert_eq!(
            current_session(&back.doc.blocks[posts]).as_deref(),
            Some("s_a")
        );
    }

    #[test]
    fn titles_are_one_short_line() {
        let block = Block::new(0, BlockKind::Todo { checked: false }, "Draft\n 3   posts");
        assert_eq!(task_title(&block), "Draft 3 posts");
        let long = Block::new(0, BlockKind::Todo { checked: false }, "word ".repeat(40));
        let title = task_title(&long);
        assert!(
            title.chars().count() <= TITLE_CHARS && title.ends_with('…'),
            "{title}"
        );
    }
}
