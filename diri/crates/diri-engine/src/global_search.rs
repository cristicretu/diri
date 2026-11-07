//! `search.global`: one search over every session's terminal and live
//! conversation, run on demand where the grids already live.
//!
//! **Screens.** Each terminal is scanned newest row first with
//! [`HeadlessScreen::find_lines_before`], holding its screen lock for one run
//! of [`FIND_HISTORY_CHUNK_ROWS`] rows at a time and pausing between runs, the
//! discipline `session.find_history` keeps so output and keystrokes never
//! wait behind a search. Sessions are spread over a few scoped threads; a
//! session stops at its line limit, and the whole scan at the time budget.
//! Nothing is retained between requests.
//!
//! **Conversations.** Claude Code and Codex transcripts are provider JSONL
//! files of many megabytes, so they are read through [`transcript::load`]'s
//! bounded tail into a small cache keyed by path, length and mtime. A query
//! never parses: it matches what is cached (a stale entry still answers
//! while one background reader refreshes it) and reports files not yet read
//! as pending. The reader and the cache exist only while searches happen:
//! both are dropped [`CACHE_IDLE`] after the last one.
//!
//! Remote sessions are searched only on the screen the Engine mirrors; their
//! history lives on the host and reaching it would mean paging it over SSH.
//!
//! No screen or transcript text is ever logged.

use std::collections::{HashMap, HashSet, VecDeque};
use std::ops::DerefMut;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use diri_proto::find::{FIND_HISTORY_CHUNK_ROWS, FindNeedle};
use diri_proto::search::{
    ConversationHit, GlobalSearchParams, GlobalSearchResult, SEARCH_CONTEXT_ROWS,
    SEARCH_MAX_BUDGET_MS, SEARCH_MAX_EXCERPT_CHARS, SEARCH_MAX_LINES_PER_SESSION,
    SEARCH_MAX_SESSIONS, ScreenHit, SearchCursor, SearchLine,
};
use diri_proto::{AgentKind, SessionId, SessionRecord};

use crate::screen::{HeadlessScreen, LineHit, LineSearch};

/// Pause between runs on one screen; see `find_history`.
const RUN_GAP: Duration = Duration::from_micros(20);
/// Scan threads. The scan is CPU-bound per screen; a handful covers a fleet
/// without crowding the Engine's own threads.
const MAX_WORKERS: usize = 6;
/// Conversations considered per request: the most recently active.
const MAX_CONVERSATIONS: usize = 32;
/// Cached text per conversation (original and folded), newest messages kept.
const CONVERSATION_BYTES: usize = 2 << 20;
/// Cached text across all conversations; least recently used goes first.
const CACHE_BYTES: usize = 24 << 20;
/// The cache and its reader thread outlive the last search by this much.
const CACHE_IDLE: Duration = Duration::from_secs(180);

/// A terminal the scan can lock. The Engine's sessions implement it; tests
/// use a bare screen.
pub trait SearchScreen: Sync {
    fn screen(&self) -> std::sync::MutexGuard<'_, HeadlessScreen>;
    /// Only the mirrored visible screen is held here.
    fn screen_only(&self) -> bool;
}

/// Runs one request over `screens` (most recently active first) and the
/// conversations of `records`.
pub fn search<S: SearchScreen>(
    params: &GlobalSearchParams,
    screens: &[(SessionId, S)],
    records: &[SessionRecord],
) -> GlobalSearchResult {
    let started = Instant::now();
    let budget = Duration::from_millis(u64::from(params.budget_ms.clamp(1, SEARCH_MAX_BUDGET_MS)));
    let deadline = started + budget;
    let max_sessions = params.max_sessions.clamp(1, SEARCH_MAX_SESSIONS) as usize;
    let max_lines = params
        .max_lines_per_session
        .clamp(1, SEARCH_MAX_LINES_PER_SESSION) as usize;

    let mut result = GlobalSearchResult::default();
    let resuming = !params.resume.is_empty();
    let conversations =
        (params.transcripts && !resuming).then(|| TranscriptCache::global().lookup(records));
    if params.screens && params.query.split_whitespace().next().is_some() {
        // A new search starts every session at its newest row; a
        // continuation only the sessions it names, where they stopped.
        let plan: Vec<(usize, Option<i64>)> = if resuming {
            params
                .resume
                .iter()
                .filter_map(|cursor| {
                    let index = screens
                        .iter()
                        .position(|(id, _)| *id == cursor.session_id)?;
                    Some((index, Some(cursor.before_row)))
                })
                .collect()
        } else {
            (0..screens.len()).map(|index| (index, None)).collect()
        };
        result.sessions_searched = u32::try_from(plan.len()).unwrap_or(u32::MAX);
        let mut scans = scan_screens(screens, plan, &params.query, max_lines, deadline);
        scans.sort_by_key(|scan| scan.index);
        for scan in scans {
            result.rows_scanned += scan.rows;
            let (id, source) = &screens[scan.index];
            if let Some(before_row) = scan.before.filter(|_| !scan.done) {
                result.incomplete.resume.push(SearchCursor {
                    session_id: id.clone(),
                    before_row,
                });
            }
            if !scan.lines.is_empty() && result.screens.len() < max_sessions {
                let more = scan.lines.len() > max_lines;
                let mut lines = scan.lines;
                lines.truncate(max_lines);
                result.screens.push(ScreenHit {
                    session_id: id.clone(),
                    lines,
                    context: scan.context,
                    more,
                    screen_only: source.screen_only(),
                });
            }
        }
    }
    if let Some((conversations, pending)) = conversations {
        result.incomplete.transcripts_pending = pending;
        if let Some(words) = Words::new(&params.query) {
            result.conversations = conversations
                .iter()
                .filter_map(|(id, messages)| match_conversation(id, messages, &words))
                .take(max_sessions)
                .collect();
        }
    }
    result.elapsed_ms = u32::try_from(started.elapsed().as_millis()).unwrap_or(u32::MAX);
    result
}

/// One session's scan so far.
struct ScreenScan {
    index: usize,
    search: Option<LineSearch>,
    lines: Vec<SearchLine>,
    context: Vec<SearchLine>,
    /// Stable row the scan continues below; `None` before it starts.
    before: Option<i64>,
    rows: u64,
    done: bool,
}

/// Scans the planned sessions on a few threads. Each session is scanned a
/// turn of [`ROWS_PER_TURN`] rows at a time and then goes to the back of
/// the queue, so when the budget runs out every session has had its newest
/// rows searched, rather than the first few their whole history.
fn scan_screens<S: SearchScreen>(
    screens: &[(SessionId, S)],
    plan: Vec<(usize, Option<i64>)>,
    query: &str,
    max_lines: usize,
    deadline: Instant,
) -> Vec<ScreenScan> {
    let count = plan.len();
    let queue: Mutex<VecDeque<ScreenScan>> = Mutex::new(
        plan.into_iter()
            .map(|(index, before)| ScreenScan {
                index,
                search: None,
                lines: Vec::new(),
                context: Vec::new(),
                before,
                rows: 0,
                done: false,
            })
            .collect(),
    );
    let finished = Mutex::new(Vec::with_capacity(count));
    let workers = std::thread::available_parallelism()
        .map_or(2, usize::from)
        .min(MAX_WORKERS)
        .min(count);
    let work = || {
        loop {
            let Some(mut scan) = queue.lock().expect("search queue").pop_front() else {
                return;
            };
            // A session not yet started always gets one run, even past the
            // deadline: it has no cursor yet, so skipping it would drop it
            // from the results without a resume cursor to come back for it.
            if scan.before.is_none() || Instant::now() < deadline {
                let source = &screens[scan.index].1;
                scan_turn(&mut scan, || source.screen(), query, max_lines, deadline);
            }
            if scan.done || Instant::now() >= deadline {
                scan.search = None;
                finished.lock().expect("search results").push(scan);
            } else {
                queue.lock().expect("search queue").push_back(scan);
            }
        }
    };
    if workers <= 1 {
        work();
    } else {
        std::thread::scope(|scope| {
            for _ in 0..workers {
                scope.spawn(work);
            }
        });
    }
    finished.into_inner().expect("search results")
}

/// Rows a session is scanned for before the next session's turn.
const ROWS_PER_TURN: usize = 16 * FIND_HISTORY_CHUNK_ROWS;

/// One turn of one terminal, newest row first, until `max_lines` matches
/// (plus one, to know there are more), its oldest row, the end of the turn,
/// or the deadline. The screen lock is held for one run at a time.
fn scan_turn<G>(
    scan: &mut ScreenScan,
    mut lock: impl FnMut() -> G,
    query: &str,
    max_lines: usize,
    deadline: Instant,
) where
    G: DerefMut<Target = HeadlessScreen>,
{
    if scan.search.is_none() {
        scan.search = LineSearch::new(query);
    }
    let Some(mut search) = scan.search.take() else {
        scan.done = true;
        return;
    };
    scan_runs(scan, &mut search, &mut lock, max_lines, deadline);
    scan.search = Some(search);
}

fn scan_runs<G>(
    scan: &mut ScreenScan,
    search: &mut LineSearch,
    lock: &mut impl FnMut() -> G,
    max_lines: usize,
    deadline: Instant,
) where
    G: DerefMut<Target = HeadlessScreen>,
{
    let mut hits: Vec<LineHit> = Vec::new();
    let mut turn = 0;
    loop {
        let mut screen = lock();
        let origin = screen.history_origin();
        let total = screen.total_rows();
        let from = scan.before.map_or(total, |stable| {
            usize::try_from((stable - origin).max(0)).map_or(total, |row| row.min(total))
        });
        hits.clear();
        let next = if from > 0 {
            screen.find_lines_before(
                from,
                FIND_HISTORY_CHUNK_ROWS,
                search,
                (max_lines + 1).saturating_sub(scan.lines.len()),
                &mut hits,
            )
        } else {
            0
        };
        scan.rows += (from - next) as u64;
        turn += from - next;
        for hit in hits.drain(..) {
            if scan.context.is_empty() {
                // The newest match is the one a jump lands on: read its
                // neighbours while the rows still mean what they meant.
                let first = hit.row.saturating_sub(SEARCH_CONTEXT_ROWS);
                let around = screen.absolute_rows_text(first..hit.row + SEARCH_CONTEXT_ROWS + 1);
                scan.context = around
                    .into_iter()
                    .enumerate()
                    .map(|(offset, text)| {
                        if first + offset == hit.row {
                            line_from_hit(&hit, None)
                        } else {
                            excerpt(&text.chars().collect::<Vec<_>>(), &[], None)
                        }
                    })
                    .collect();
            }
            scan.lines
                .push(line_from_hit(&hit, Some(origin + hit.row as i64)));
        }
        scan.before = Some(origin + next as i64);
        if next == 0 || scan.lines.len() > max_lines {
            scan.done = true;
            return;
        }
        if turn >= ROWS_PER_TURN || Instant::now() >= deadline {
            return;
        }
        drop(screen);
        std::thread::sleep(RUN_GAP);
    }
}

fn line_from_hit(hit: &LineHit, row: Option<i64>) -> SearchLine {
    excerpt(&hit.text.chars().collect::<Vec<_>>(), &hit.ranges, row)
}

/// At most [`SEARCH_MAX_EXCERPT_CHARS`] of `chars`, around the first match
/// when the text is longer, with `…` where it was cut.
fn excerpt(chars: &[char], ranges: &[[u32; 2]], row: Option<i64>) -> SearchLine {
    let max = SEARCH_MAX_EXCERPT_CHARS;
    if chars.len() <= max {
        return SearchLine {
            row,
            text: chars.iter().collect(),
            ranges: ranges.to_vec(),
        };
    }
    let first = ranges.first().map_or(0, |range| range[0] as usize);
    let start = first.saturating_sub(max / 4).min(chars.len() - max);
    let end = start + max;
    let mut text = String::with_capacity(max + 8);
    let mut shift = -(start as i64);
    if start > 0 {
        text.push('…');
        shift += 1;
    }
    text.extend(&chars[start..end]);
    if end < chars.len() {
        text.push('…');
    }
    let ranges = ranges
        .iter()
        .filter(|range| (range[0] as usize) < end && (range[1] as usize) > start)
        .map(|range| {
            let clamp = |at: u32| (i64::from(at).clamp(start as i64, end as i64) + shift) as u32;
            [clamp(range[0]), clamp(range[1])]
        })
        .collect();
    SearchLine { row, text, ranges }
}

/// The query's words for message matching: lowercase words fold case
/// (matched against the message's folded text), others are exact.
struct Words {
    words: Vec<(String, bool)>,
    needles: Vec<FindNeedle>,
}

impl Words {
    fn new(query: &str) -> Option<Self> {
        let words: Vec<(String, bool)> = query
            .split_whitespace()
            .map(|word| (word.to_owned(), word.chars().any(char::is_uppercase)))
            .collect();
        let needles = words
            .iter()
            .filter_map(|(word, _)| FindNeedle::new(word))
            .collect();
        (!words.is_empty()).then_some(Self { words, needles })
    }

    fn matches(&self, message: &Message) -> bool {
        self.words.iter().all(|(word, exact)| {
            if *exact {
                message.text.contains(word.as_str())
            } else {
                message.folded.contains(word.as_str())
            }
        })
    }
}

/// The newest message of a conversation that holds every word.
fn match_conversation(
    id: &SessionId,
    messages: &[Message],
    words: &Words,
) -> Option<ConversationHit> {
    let mut found = messages
        .iter()
        .rev()
        .enumerate()
        .filter(|(_, message)| words.matches(message));
    let (messages_ago, message) = found.next()?;
    let more = found.count();
    // One line: the excerpt shows prose, not the message's layout.
    let mut chars: Vec<char> = Vec::with_capacity(message.text.len());
    for ch in message.text.chars() {
        let ch = if ch.is_whitespace() { ' ' } else { ch };
        if ch == ' ' && chars.last().is_none_or(|last| *last == ' ') {
            continue;
        }
        chars.push(ch);
    }
    let mut ranges = Vec::new();
    for needle in &words.needles {
        needle.for_each_match(&chars, |start, end| ranges.push([start as u32, end as u32]));
    }
    ranges.sort_unstable();
    ranges.dedup_by(|next, kept| {
        let overlaps = next[0] <= kept[1];
        if overlaps {
            kept[1] = kept[1].max(next[1]);
        }
        overlaps
    });
    Some(ConversationHit {
        session_id: id.clone(),
        role: if message.user { "user" } else { "agent" }.into(),
        excerpt: excerpt(&chars, &ranges, None),
        messages_ago: u32::try_from(messages_ago).unwrap_or(u32::MAX),
        more: u32::try_from(more).unwrap_or(u32::MAX),
    })
}

struct Message {
    user: bool,
    text: Box<str>,
    /// `text` lowercased, for case-folding words.
    folded: Box<str>,
}

impl Message {
    fn bytes(&self) -> usize {
        self.text.len() + self.folded.len()
    }
}

/// What identifies a file's content well enough to reuse its parse.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Version {
    len: u64,
    modified: Option<SystemTime>,
}

struct Conversation {
    version: Version,
    messages: Arc<[Message]>,
    bytes: usize,
    used: Instant,
}

#[derive(Clone, PartialEq, Eq)]
struct Job {
    path: PathBuf,
    kind: AgentKind,
    agent_id: String,
    cwd: String,
}

#[derive(Default)]
struct CacheState {
    files: HashMap<PathBuf, Conversation>,
    queue: VecDeque<Job>,
    queued: HashSet<PathBuf>,
    reader: bool,
    last_search: Option<Instant>,
}

/// Lets a reader that dies mid-read (a panicking parse) go: without this,
/// `reader` stays set and its file stays `queued`, so no later search ever
/// reads anything again. The queue is dropped too; the next search queues
/// what it still needs and starts a new reader.
struct ReaderExit<'a> {
    cache: &'a TranscriptCache,
    finished: bool,
}

impl Drop for ReaderExit<'_> {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        let mut state = self.cache.state.lock().unwrap_or_else(|poisoned| {
            self.cache.state.clear_poison();
            poisoned.into_inner()
        });
        state.reader = false;
        state.queue.clear();
        state.queued.clear();
    }
}

/// Parsed conversations, shared by every request of this Engine.
pub(crate) struct TranscriptCache {
    state: Mutex<CacheState>,
    wake: Condvar,
    home: Option<PathBuf>,
}

type Found = (Vec<(SessionId, Arc<[Message]>)>, u32);

impl TranscriptCache {
    fn global() -> &'static Self {
        static CACHE: OnceLock<TranscriptCache> = OnceLock::new();
        CACHE.get_or_init(|| Self {
            state: Mutex::default(),
            wake: Condvar::new(),
            home: diri_platform::home_dir()
                .map(|path| path.into_os_string())
                .map(PathBuf::from),
        })
    }

    /// The cached messages of `records`' conversations (most recently
    /// active first), queueing a read of any missing or changed file.
    /// Returns them with the number not cached at all.
    fn lookup(&'static self, records: &[SessionRecord]) -> Found {
        let mut candidates: Vec<(&SessionRecord, Job)> = records
            .iter()
            .filter(|record| record.host.is_none() && !record.is_archived())
            .filter_map(|record| {
                let kind = record.effective_kind();
                if !matches!(kind.id(), AgentKind::CLAUDE_CODE_ID | AgentKind::CODEX_ID) {
                    return None;
                }
                Some((
                    record,
                    Job {
                        path: PathBuf::from(record.transcript_path.as_deref()?),
                        kind: kind.clone(),
                        agent_id: record.agent_session_id.clone()?,
                        cwd: record.cwd.clone(),
                    },
                ))
            })
            .collect();
        candidates.sort_by(|left, right| right.0.updated_at.0.total_cmp(&left.0.updated_at.0));
        candidates.truncate(MAX_CONVERSATIONS);
        let versions: Vec<Option<Version>> = candidates
            .iter()
            .map(|(_, job)| version_of(&job.path))
            .collect();

        let now = Instant::now();
        let mut state = self.state.lock().expect("transcript cache");
        state.last_search = Some(now);
        let mut found = Vec::new();
        let mut pending = 0;
        let mut queued = false;
        for ((record, job), version) in candidates.into_iter().zip(versions) {
            let Some(version) = version else { continue };
            let fresh = match state.files.get_mut(&job.path) {
                Some(cached) => {
                    cached.used = now;
                    found.push((record.id.clone(), Arc::clone(&cached.messages)));
                    cached.version == version
                }
                None => {
                    pending += 1;
                    false
                }
            };
            if !fresh && state.queued.insert(job.path.clone()) {
                state.queue.push_back(job);
                queued = true;
            }
        }
        if queued {
            if state.reader {
                self.wake.notify_one();
            } else {
                state.reader = true;
                let spawned = std::thread::Builder::new()
                    .name("diri-search-transcripts".into())
                    .spawn(move || self.read_queue());
                if spawned.is_err() {
                    state.reader = false;
                }
            }
        }
        (found, pending)
    }

    /// The reader thread: parses queued files, then waits for more until
    /// the cache has been idle for [`CACHE_IDLE`], and drops it.
    fn read_queue(&self) {
        let mut exit = ReaderExit {
            cache: self,
            finished: false,
        };
        loop {
            let job = {
                let mut state = self.state.lock().expect("transcript cache");
                loop {
                    if let Some(job) = state.queue.pop_front() {
                        break job;
                    }
                    let idle_for = state.last_search.map_or(CACHE_IDLE, |at| at.elapsed());
                    if idle_for >= CACHE_IDLE {
                        state.files = HashMap::new();
                        state.queued = HashSet::new();
                        state.reader = false;
                        exit.finished = true;
                        return;
                    }
                    state = self
                        .wake
                        .wait_timeout(state, CACHE_IDLE - idle_for)
                        .expect("transcript cache")
                        .0;
                }
            };
            let read = version_of(&job.path).and_then(|version| {
                let snapshot = crate::transcript::load(
                    self.home.as_deref()?,
                    &job.path,
                    &job.kind,
                    &job.agent_id,
                    &job.cwd,
                    None,
                )
                .ok()??;
                Some((version, messages_of(snapshot.document.turns)))
            });
            let mut state = self.state.lock().expect("transcript cache");
            state.queued.remove(&job.path);
            match read {
                Some((version, (messages, bytes))) => {
                    state.files.insert(
                        job.path,
                        Conversation {
                            version,
                            messages,
                            bytes,
                            used: Instant::now(),
                        },
                    );
                    evict(&mut state.files);
                }
                None => {
                    state.files.remove(&job.path);
                }
            }
        }
    }
}

fn version_of(path: &Path) -> Option<Version> {
    let metadata = std::fs::symlink_metadata(path).ok()?;
    metadata.is_file().then(|| Version {
        len: metadata.len(),
        modified: metadata.modified().ok(),
    })
}

/// The newest messages that fit [`CONVERSATION_BYTES`], oldest first.
fn messages_of(turns: Vec<crate::transcript::TranscriptTurn>) -> (Arc<[Message]>, usize) {
    let mut kept = Vec::new();
    let mut bytes = 0;
    for turn in turns.into_iter().rev() {
        let message = Message {
            user: turn.role == "You",
            folded: turn.text.to_lowercase().into_boxed_str(),
            text: turn.text.into_boxed_str(),
        };
        if bytes + message.bytes() > CONVERSATION_BYTES {
            break;
        }
        bytes += message.bytes();
        kept.push(message);
    }
    kept.reverse();
    (kept.into(), bytes)
}

fn evict(files: &mut HashMap<PathBuf, Conversation>) {
    let mut total: usize = files.values().map(|file| file.bytes).sum();
    while total > CACHE_BYTES {
        let Some(oldest) = files
            .iter()
            .min_by_key(|(_, file)| file.used)
            .map(|(path, _)| path.clone())
        else {
            return;
        };
        if let Some(file) = files.remove(&oldest) {
            total -= file.bytes;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Bare(Mutex<HeadlessScreen>, bool);

    impl SearchScreen for Bare {
        fn screen(&self) -> std::sync::MutexGuard<'_, HeadlessScreen> {
            self.0.lock().unwrap()
        }
        fn screen_only(&self) -> bool {
            self.1
        }
    }

    fn terminal(lines: &[&str]) -> Bare {
        let mut screen = HeadlessScreen::new(80, 6);
        for line in lines {
            screen.feed(format!("{line}\r\n").as_bytes());
        }
        Bare(Mutex::new(screen), false)
    }

    fn params(query: &str) -> GlobalSearchParams {
        GlobalSearchParams {
            query: query.into(),
            max_sessions: 10,
            max_lines_per_session: 2,
            budget_ms: 200,
            screens: true,
            transcripts: false,
            resume: Vec::new(),
        }
    }

    #[test]
    fn finds_the_newest_lines_holding_every_word_in_each_session() {
        let screens = vec![
            (
                SessionId("a".into()),
                terminal(&[
                    "Traceback (most recent call last):",
                    "ValueError: bad thing",
                    "ok",
                    "ValueError: worse thing",
                    "ValueError: bad again",
                    "tail",
                ]),
            ),
            (SessionId("b".into()), terminal(&["nothing here"])),
        ];
        let result = search(&params("valueerror bad"), &screens, &[]);
        assert_eq!(result.sessions_searched, 2);
        assert_eq!(result.screens.len(), 1);
        let hit = &result.screens[0];
        assert_eq!(hit.session_id.0, "a");
        let texts: Vec<_> = hit.lines.iter().map(|line| line.text.as_str()).collect();
        assert_eq!(texts, ["ValueError: bad again", "ValueError: bad thing"]);
        assert!(!hit.more);
        assert_eq!(hit.lines[0].ranges, [[0, 10], [12, 15]]);
        // Context: two rows each side of the newest match, match ranged.
        let context: Vec<_> = hit.context.iter().map(|line| line.text.as_str()).collect();
        assert_eq!(
            context,
            [
                "ok",
                "ValueError: worse thing",
                "ValueError: bad again",
                "tail",
                ""
            ]
        );
        assert!(hit.context[2].ranges.len() == 2);
        assert!(result.incomplete.resume.is_empty());
    }

    #[test]
    fn caps_lines_and_reports_more_and_covers_deep_history() {
        // Two hundred screens deep, yet small enough that a loaded CI runner
        // finishes an unoptimized scan well inside the maximum budget (12k
        // rows timed out at 200 ms and dropped the oldest match).
        let mut lines: Vec<String> = (0..1_200).map(|i| format!("row {i}")).collect();
        lines[3] = "needle early".into();
        lines[600] = "needle middle".into();
        lines[1_100] = "needle late".into();
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let screens = vec![(SessionId("deep".into()), terminal(&refs))];
        let deep = |max_lines_per_session| GlobalSearchParams {
            max_lines_per_session,
            budget_ms: SEARCH_MAX_BUDGET_MS,
            ..params("needle")
        };
        let result = search(&deep(2), &screens, &[]);
        let hit = &result.screens[0];
        assert_eq!(hit.lines.len(), 2);
        assert!(hit.more);
        assert_eq!(hit.lines[0].text, "needle late");
        let all = search(&deep(8), &screens, &[]);
        assert_eq!(all.screens[0].lines.len(), 3);
        assert_eq!(all.screens[0].lines[2].text, "needle early");
        assert!(!all.screens[0].more);
        assert!(all.rows_scanned >= 1_200);
        assert!(all.incomplete.resume.is_empty());
    }

    #[test]
    fn a_spent_budget_returns_cursors_that_resume_without_gaps() {
        let mut lines: Vec<String> = (0..40_000).map(|i| format!("plain output {i}")).collect();
        lines[10] = "rare needle".into();
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let screens = vec![
            (SessionId("big".into()), terminal(&refs)),
            (SessionId("small".into()), terminal(&["rare needle too"])),
        ];
        let mut request = GlobalSearchParams {
            budget_ms: 1,
            ..params("needle")
        };
        let first = search(&request, &screens, &[]);
        // Round-robin turns: the small session finished in its first turn.
        assert_eq!(first.incomplete.resume.len(), 1, "{:?}", first.incomplete);
        assert_eq!(first.incomplete.resume[0].session_id.0, "big");
        assert!(first.screens.iter().any(|hit| hit.session_id.0 == "small"));
        request.resume = first.incomplete.resume.clone();
        let mut found = Vec::new();
        let mut pages = 1;
        while !request.resume.is_empty() {
            let page = search(&request, &screens, &[]);
            pages += 1;
            found.extend(page.screens.into_iter().flat_map(|hit| hit.lines));
            request.resume = page.incomplete.resume;
        }
        assert!(pages > 2);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].text, "rare needle");
    }

    #[test]
    fn a_deadline_already_past_still_gives_every_session_its_first_run() {
        // A loaded machine can spend the whole budget before a worker pops
        // the second session; it must still be searched or get a cursor.
        let mut lines: Vec<String> = (0..40_000).map(|i| format!("plain output {i}")).collect();
        lines[10] = "rare needle".into();
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let screens = vec![
            (SessionId("big".into()), terminal(&refs)),
            (SessionId("small".into()), terminal(&["rare needle too"])),
        ];
        let past = Instant::now() - Duration::from_secs(1);
        let scans = scan_screens(&screens, vec![(0, None), (1, None)], "needle", 2, past);
        let small = scans
            .iter()
            .find(|scan| scan.index == 1)
            .expect("small scanned");
        assert!(small.done);
        assert_eq!(small.lines.len(), 1);
        let big = scans
            .iter()
            .find(|scan| scan.index == 0)
            .expect("big scanned");
        assert!(!big.done);
        assert!(
            big.before.is_some(),
            "an unfinished session keeps a resume cursor"
        );
    }

    #[test]
    fn remote_hits_say_only_the_screen_was_searched() {
        let mut remote = terminal(&["deploy failed"]);
        remote.1 = true;
        let screens = vec![(SessionId("r".into()), remote)];
        let result = search(&params("deploy"), &screens, &[]);
        assert!(result.screens[0].screen_only);
    }

    #[test]
    fn long_lines_are_cut_around_the_first_match() {
        let text: Vec<char> = format!("{}needle{}", "a".repeat(500), "b".repeat(500))
            .chars()
            .collect();
        let line = excerpt(&text, &[[500, 506]], Some(9));
        assert!(line.text.starts_with('…') && line.text.ends_with('…'));
        let chars: Vec<char> = line.text.chars().collect();
        let [start, end] = line.ranges[0];
        let matched: String = chars[start as usize..end as usize].iter().collect();
        assert_eq!(matched, "needle");
        assert_eq!(line.row, Some(9));
    }

    fn message(user: bool, text: &str) -> Message {
        Message {
            user,
            folded: text.to_lowercase().into(),
            text: text.into(),
        }
    }

    #[test]
    fn conversations_match_the_newest_message_with_every_word() {
        let messages = [
            message(true, "Why does the parser panic?"),
            message(false, "The Parser panics because\nthe buffer is empty."),
            message(true, "thanks"),
        ];
        let words = Words::new("parser panic").unwrap();
        let hit = match_conversation(&SessionId("c".into()), &messages, &words).unwrap();
        assert_eq!(hit.role, "agent");
        assert_eq!(hit.messages_ago, 1);
        assert_eq!(hit.more, 1);
        assert_eq!(
            hit.excerpt.text,
            "The Parser panics because the buffer is empty."
        );
        assert_eq!(hit.excerpt.ranges, [[4, 10], [11, 16]]);
        // An uppercase word is exact.
        assert!(
            match_conversation(
                &SessionId("c".into()),
                &messages,
                &Words::new("PANIC").unwrap()
            )
            .is_none()
        );
    }

    #[test]
    fn the_cache_keeps_the_newest_messages_within_its_bound() {
        let big = "x".repeat(CONVERSATION_BYTES / 3);
        let turns = (0..5)
            .map(|index| crate::transcript::TranscriptTurn {
                role: if index % 2 == 0 { "You" } else { "Claude" },
                text: format!("{index}{big}"),
                line: index,
            })
            .collect();
        let (messages, bytes) = messages_of(turns);
        assert!(bytes <= CONVERSATION_BYTES);
        assert_eq!(messages.len(), 1);
        assert!(messages[0].text.starts_with('4'));
        assert!(messages[0].user);
    }
}
