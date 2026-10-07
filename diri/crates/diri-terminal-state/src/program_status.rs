//! `OSC 7501` program status: a program says it is idle, working, done,
//! blocked on the user, or failed, instead of the terminal guessing from the
//! screen. Specification:
//! <https://www.superlogical.com/rex/docs/build/program-status>.
//!
//! The terminal keeps one record per `id`; each report replaces its record
//! whole, and `state=clear` removes a record with its descendants. Like every
//! product OSC this is parsed only from live output in the local Engine, and a
//! malformed report is dropped entirely rather than partly applied.
use std::collections::BTreeMap;

/// The OSC number, with its separator, as it opens the payload.
pub(crate) const PREFIX: &str = "7501;";
/// What a program sends to ask whether the terminal speaks the protocol, and
/// what a terminal that does sends back.
pub(crate) const QUERY: &str = "?";
pub(crate) const QUERY_REPLY: &[u8] = b"\x1b]7501;?\x1b\\";

/// The whole sequence, introducer and terminator included.
const SEQUENCE_LIMIT: usize = 4096;
/// `ESC ]` plus the longer terminator, `ESC \`.
const FRAMING: usize = 4;
const RECORD_LIMIT: usize = 256;
const ID_LIMIT: usize = 128;
const ID_SEGMENT_LIMIT: usize = 32;
const ID_DEPTH_LIMIT: usize = 8;
const APP_LIMIT: usize = 32;
const MSG_ENCODED_LIMIT: usize = 2732;
const MSG_DECODED_LIMIT: usize = 2048;
const TITLE_ENCODED_LIMIT: usize = 256;
const TITLE_DECODED_LIMIT: usize = 192;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ProgramState {
    /// Waiting for the user's next instruction.
    Idle,
    /// Finished; the result is ready.
    Done,
    /// Failed or stopped.
    Error,
    /// Running.
    Working,
    /// Waiting on the user to act; see [`BlockedKind`].
    Blocked,
}

impl ProgramState {
    /// Working and blocked describe a turn still under way; the others are
    /// its result.
    #[must_use]
    pub fn is_transient(self) -> bool {
        matches!(self, Self::Working | Self::Blocked)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockedKind {
    Permission,
    Question,
    Auth,
}

/// One record as its last report left it. Text is decoded, free of control
/// characters and bidirectional overrides, and never interpreted as markup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProgramRecord {
    pub state: ProgramState,
    /// Only for [`ProgramState::Blocked`]; an unknown kind reads as none.
    pub kind: Option<BlockedKind>,
    /// Whole percent, for working and blocked records.
    pub progress: Option<u8>,
    pub app: Option<String>,
    pub title: Option<String>,
    pub msg: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct ProgramStatusTable {
    /// Keyed by id; the root record's id is empty.
    records: BTreeMap<String, (ProgramRecord, u64)>,
    clock: u64,
    generation: u64,
}

impl ProgramStatusTable {
    /// Bumps whenever any record changes, so a reader can skip re-deriving
    /// the summary for output that carried no report.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Applies one report, the payload after `7501;`. Returns false, changing
    /// nothing, when it is malformed.
    pub fn apply(&mut self, report: &str) -> bool {
        let Some(report) = Report::parse(report) else {
            return false;
        };
        match report.record {
            None => {
                let before = self.records.len();
                self.records
                    .retain(|id, _| !is_self_or_descendant(id, &report.id));
                if self.records.len() != before {
                    self.generation += 1;
                }
            }
            Some(record) => {
                self.clock += 1;
                if !self.records.contains_key(&report.id) && self.records.len() >= RECORD_LIMIT {
                    let oldest = self
                        .records
                        .iter()
                        .min_by_key(|(_, (_, updated))| *updated)
                        .map(|(id, _)| id.clone());
                    if let Some(oldest) = oldest {
                        self.records.remove(&oldest);
                    }
                }
                self.records.insert(report.id, (record, self.clock));
                self.generation += 1;
            }
        }
        true
    }

    /// Drops every record: the program that reported them has ended.
    /// Returns whether there were any.
    ///
    /// The specification keeps `done`, `error` and `idle` past the program's
    /// exit so a terminal can keep showing the result. Diri reads the records
    /// as the program's status instead, and delivers a result as the turn it
    /// completed when it arrives; kept, they would describe a program that is
    /// gone and hide the status of whatever runs next.
    pub fn end_program(&mut self) -> bool {
        if self.records.is_empty() {
            return false;
        }
        self.records.clear();
        self.generation += 1;
        true
    }

    /// The record that best describes the whole program: one waiting on the
    /// user first, then one at work, then a failure, a result, an idle
    /// program. Ties go to the most recently reported.
    pub fn summary(&self) -> Option<ProgramRecord> {
        self.records
            .values()
            .max_by_key(|(record, updated)| (record.state, *updated))
            .map(|(record, _)| record.clone())
    }
}

/// `id` is `ancestor` or below it. The root, the empty id, holds everything.
fn is_self_or_descendant(id: &str, ancestor: &str) -> bool {
    ancestor.is_empty()
        || id == ancestor
        || id
            .strip_prefix(ancestor)
            .is_some_and(|rest| rest.starts_with('/'))
}

struct Report {
    id: String,
    /// `None` for `state=clear`.
    record: Option<ProgramRecord>,
}

impl Report {
    fn parse(report: &str) -> Option<Self> {
        if report.len() + PREFIX.len() + FRAMING > SEQUENCE_LIMIT
            || report.is_empty()
            || !report
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"_.,+/=-:".contains(&byte))
        {
            return None;
        }
        let mut state = None;
        let mut id = None;
        let mut kind = None;
        let mut progress = None;
        let mut app = None;
        let mut title = None;
        let mut msg = None;
        for pair in report.split(':') {
            let (key, value) = pair.split_once('=')?;
            let slot_taken = match key {
                "state" => state.replace(value).is_some(),
                "id" => id.replace(value).is_some(),
                "kind" => kind.replace(value).is_some(),
                "progress" => progress.replace(value).is_some(),
                "app" => app.replace(value).is_some(),
                "title" => title.replace(value).is_some(),
                "msg" => msg.replace(value).is_some(),
                // A newer revision's key: the rest of the report still holds.
                _ => false,
            };
            if slot_taken {
                return None;
            }
        }
        let id = match id {
            Some(id) => valid_id(id)?.to_owned(),
            None => String::new(),
        };
        let state = match state? {
            "idle" => ProgramState::Idle,
            "working" => ProgramState::Working,
            "done" => ProgramState::Done,
            "blocked" => ProgramState::Blocked,
            "error" => ProgramState::Error,
            "clear" => return Some(Self { id, record: None }),
            _ => return None,
        };
        let progress = match progress {
            Some(value) => {
                let percent = value.parse::<u8>().ok().filter(|percent| *percent <= 100)?;
                state.is_transient().then_some(percent)
            }
            None => None,
        };
        let kind = kind
            .filter(|_| state == ProgramState::Blocked)
            .and_then(|kind| match kind {
                "permission" => Some(BlockedKind::Permission),
                "question" => Some(BlockedKind::Question),
                "auth" => Some(BlockedKind::Auth),
                _ => None,
            });
        let app = match app {
            Some(app)
                if (1..=APP_LIMIT).contains(&app.len())
                    && app
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || b"_.-".contains(&byte)) =>
            {
                Some(app.to_owned())
            }
            Some(_) => return None,
            None => None,
        };
        let title = text(title, TITLE_ENCODED_LIMIT, TITLE_DECODED_LIMIT)?;
        let msg = text(msg, MSG_ENCODED_LIMIT, MSG_DECODED_LIMIT)?;
        Some(Self {
            id,
            record: Some(ProgramRecord {
                state,
                kind,
                progress,
                app,
                title,
                msg,
            }),
        })
    }
}

fn valid_id(id: &str) -> Option<&str> {
    let segments = id.split('/');
    let valid = !id.is_empty()
        && id.len() <= ID_LIMIT
        && segments.clone().count() <= ID_DEPTH_LIMIT
        && segments.into_iter().all(|segment| {
            (1..=ID_SEGMENT_LIMIT).contains(&segment.len()) && !segment.contains('=')
        });
    valid.then_some(id)
}

/// A base64 text field. The outer `Option` is validity: `None` rejects the
/// whole report. Empty text reads as absent.
#[allow(clippy::option_option)]
fn text(value: Option<&str>, encoded_limit: usize, decoded_limit: usize) -> Option<Option<String>> {
    let Some(value) = value else {
        return Some(None);
    };
    if value.len() > encoded_limit {
        return None;
    }
    let bytes = decode_base64(value)?;
    if bytes.len() > decoded_limit {
        return None;
    }
    let text = String::from_utf8(bytes).ok()?;
    // C0, DEL and C1 are forbidden outright; a report carrying one is not
    // displayed in part.
    if text.chars().any(char::is_control) {
        return None;
    }
    // Direction overrides and isolates would let a program reorder the text
    // around where it is shown.
    let text: String = text
        .chars()
        .filter(|ch| !matches!(ch, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'))
        .collect();
    Some((!text.is_empty()).then_some(text))
}

/// Standard base64; padding may be left off. Small enough not to give the
/// Remote Helper, which links this crate, a dependency it would never use.
fn decode_base64(value: &str) -> Option<Vec<u8>> {
    let padded;
    let bytes = match value.len() % 4 {
        0 => value.as_bytes(),
        1 => return None,
        missing => {
            padded = format!("{value}{}", &"=="[..4 - missing]);
            padded.as_bytes()
        }
    };
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for (index, chunk) in bytes.chunks(4).enumerate() {
        let last = index == bytes.len() / 4 - 1;
        let padding = chunk.iter().rev().take_while(|byte| **byte == b'=').count();
        if padding > 2 || (padding > 0 && !last) {
            return None;
        }
        let mut word = 0u32;
        for &byte in &chunk[..4 - padding] {
            let sextet = match byte {
                b'A'..=b'Z' => byte - b'A',
                b'a'..=b'z' => byte - b'a' + 26,
                b'0'..=b'9' => byte - b'0' + 52,
                b'+' => 62,
                b'/' => 63,
                _ => return None,
            };
            word = word << 6 | u32::from(sextet);
        }
        word <<= 6 * padding as u32;
        let decoded = word.to_be_bytes();
        out.extend_from_slice(&decoded[1..4 - padding]);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(reports: &[&str]) -> ProgramStatusTable {
        let mut table = ProgramStatusTable::default();
        for report in reports {
            table.apply(report);
        }
        table
    }

    #[test]
    fn the_terraform_example_from_the_specification() {
        let table = table(&[
            "state=blocked:kind=permission:app=terraform:msg=QXBwbHkgMyB0byBhZGQsIDEgdG8gY2hhbmdlLCAwIHRvIGRlc3Ryb3k/",
        ]);
        assert_eq!(
            table.summary(),
            Some(ProgramRecord {
                state: ProgramState::Blocked,
                kind: Some(BlockedKind::Permission),
                progress: None,
                app: Some("terraform".into()),
                title: None,
                msg: Some("Apply 3 to add, 1 to change, 0 to destroy?".into()),
            })
        );
    }

    #[test]
    fn a_report_replaces_its_record_whole() {
        let table = table(&[
            "state=working:progress=40:msg=YnVpbGRpbmc=",
            "state=working",
        ]);
        let summary = table.summary().unwrap();
        assert_eq!(summary.progress, None);
        assert_eq!(summary.msg, None);
    }

    #[test]
    fn a_blocked_child_outranks_its_working_parent_and_clear_takes_descendants() {
        let mut table = table(&[
            "state=working:id=build",
            "state=blocked:id=build/deploy:kind=question",
            "state=done:id=lint",
        ]);
        assert_eq!(table.summary().unwrap().state, ProgramState::Blocked);
        assert!(table.apply("state=clear:id=build"));
        assert_eq!(table.summary().unwrap().state, ProgramState::Done);
        assert!(table.apply("state=clear"));
        assert_eq!(table.summary(), None);
    }

    #[test]
    fn clearing_a_sibling_prefix_leaves_the_other_record() {
        let mut table = table(&["state=working:id=build2"]);
        table.apply("state=clear:id=build");
        assert!(table.summary().is_some());
    }

    #[test]
    fn ending_the_program_drops_every_record() {
        let mut table = table(&["state=working:id=a", "state=error:id=b"]);
        assert!(table.end_program());
        assert_eq!(table.summary(), None);
        assert!(!table.end_program());
    }

    #[test]
    fn malformed_reports_change_nothing() {
        let mut table = table(&["state=idle"]);
        let generation = table.generation();
        for report in [
            "",
            "state=sleeping",
            "kind=permission",
            "state=working:state=idle",
            "state=working:progress=101",
            "state=working:progress=-1",
            "state=working:id=a//b",
            "state=working:id=a/b/c/d/e/f/g/h/i",
            "state=working:app=has space",
            "state=working:msg=not base64!",
            "state=working:msg=YQ===",
            // "a\nb": a control character inside decoded text.
            "state=working:msg=YQpi",
            // Invalid UTF-8.
            "state=working:msg=/w==",
        ] {
            assert!(!table.apply(report), "{report:?} should be rejected");
        }
        assert_eq!(table.generation(), generation);
        assert_eq!(table.summary().unwrap().state, ProgramState::Idle);
    }

    #[test]
    fn unknown_keys_and_kinds_are_tolerated() {
        let table = table(&["state=blocked:kind=telepathy:future=1"]);
        let summary = table.summary().unwrap();
        assert_eq!(summary.state, ProgramState::Blocked);
        assert_eq!(summary.kind, None);
    }

    #[test]
    fn records_are_bounded_by_least_recent_update() {
        let mut table = ProgramStatusTable::default();
        for index in 0..=RECORD_LIMIT {
            table.apply(&format!("state=idle:id=r{index}"));
        }
        assert_eq!(table.records.len(), RECORD_LIMIT);
        assert!(!table.records.contains_key("r0"));
        assert!(table.records.contains_key(&format!("r{RECORD_LIMIT}")));
    }

    #[test]
    fn direction_overrides_are_disarmed() {
        // "a\u{202e}b"
        let table = table(&["state=done:title=YeKArmI="]);
        assert_eq!(table.summary().unwrap().title.as_deref(), Some("ab"));
    }

    #[test]
    fn base64_round_trips_every_padding() {
        assert_eq!(decode_base64("").unwrap(), b"");
        assert_eq!(decode_base64("YQ==").unwrap(), b"a");
        assert_eq!(decode_base64("YWI=").unwrap(), b"ab");
        assert_eq!(decode_base64("YWJj").unwrap(), b"abc");
        assert!(decode_base64("YQ==YWJj").is_none());
        assert!(decode_base64("Y===").is_none());
        assert_eq!(decode_base64("YQ").unwrap(), b"a");
        assert!(decode_base64("YWJjZ").is_none());
    }
}
