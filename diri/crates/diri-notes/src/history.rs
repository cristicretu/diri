//! Version history: earlier states of every note, kept beside the notes.
//!
//! Each version is the note's full Markdown in
//! `<notes>/.history/<note id>/<unix ms>~<author>~<reason>.md`, so a version
//! is one atomic file and its facts need no index that could disagree with
//! it. History is keyed by note id and lives outside the note file, so it
//! survives trash and restore.
//!
//! Versions compare by body (everything but front matter): pinning, archiving
//! or stamping a note is not a new version, and restoring one puts back its
//! text while the note keeps its current place, pin, and Session.
//!
//! When versions are taken:
//! - edits: at most one per [`EDIT_INTERVAL_MS`] of continuous typing;
//! - always before a write from outside the editor (an agent, the CLI);
//! - always before a restore, and the restored state itself.
//!
//! Retention keeps every version from the last hour, then the newest per ten
//! minutes for a day, per day for a month, per week after that, never more
//! than [`MAX_VERSIONS`] or [`MAX_BYTES`] per note, and always the newest.

use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::markdown;

pub const HISTORY_DIR: &str = ".history";
pub const EDIT_INTERVAL_MS: u64 = 60_000;
pub const MAX_VERSIONS: usize = 200;
pub const MAX_BYTES: u64 = 20 * 1024 * 1024;
/// Not a version: the store's last written text (no `.md`, so never listed).
const LAST_WRITTEN: &str = "last";

const MINUTE: u64 = 60_000;
const HOUR: u64 = 60 * MINUTE;
const DAY: u64 = 24 * HOUR;

/// Who produced a version's content.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Author {
    /// Typed in the note editor.
    User,
    /// An agent, by its Session id.
    Session(String),
    /// `dirijor note` from a terminal outside any Session.
    Cli,
    /// Someone edited the `.md` file directly (another editor, an agent's
    /// own file tools); noticed afterwards, so the editor is unknown.
    File,
}

impl Author {
    fn token(&self) -> &str {
        match self {
            Self::User => "user",
            Self::Cli => "cli",
            Self::File => "file",
            Self::Session(id) => id,
        }
    }

    fn parse(token: &str) -> Option<Self> {
        match token {
            "user" => Some(Self::User),
            "cli" => Some(Self::Cli),
            "file" => Some(Self::File),
            id if crate::store::is_valid_id(id) => Some(Self::Session(id.to_owned())),
            _ => None,
        }
    }

    /// Plain words for people: "you", "the command line", or a session id.
    pub fn describe(&self) -> String {
        match self {
            Self::User => "you".into(),
            Self::Cli => "the command line".into(),
            Self::File => "a direct file edit".into(),
            Self::Session(id) => id.clone(),
        }
    }

    /// The author for a write from this process: the calling Session when
    /// there is one, else the command line.
    pub fn from_env() -> Self {
        std::env::var(diri_proto::paths::ENV_SESSION_ID)
            .ok()
            .filter(|id| crate::store::is_valid_id(id))
            .map_or(Self::Cli, Self::Session)
    }
}

/// Why a version was taken.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    /// An edit in the editor (throttled).
    Edit,
    /// A write from outside the editor: an agent or the command line.
    Write,
    /// The note as it stood before someone else wrote to it.
    BeforeWrite,
    /// The note as it stood before a restore.
    BeforeRestore,
    /// The result of restoring the version with this id.
    Restore(u64),
}

impl Reason {
    fn token(self) -> String {
        match self {
            Self::Edit => "edit".into(),
            Self::Write => "write".into(),
            Self::BeforeWrite => "before-write".into(),
            Self::BeforeRestore => "before-restore".into(),
            Self::Restore(from) => format!("restore-{from}"),
        }
    }

    fn parse(token: &str) -> Option<Self> {
        match token {
            "edit" => Some(Self::Edit),
            "write" => Some(Self::Write),
            "before-write" => Some(Self::BeforeWrite),
            "before-restore" => Some(Self::BeforeRestore),
            other => other
                .strip_prefix("restore-")
                .and_then(|from| from.parse().ok())
                .map(Self::Restore),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Version {
    /// Unix milliseconds when it was taken; unique per note.
    pub id: u64,
    pub author: Author,
    pub reason: Reason,
    pub bytes: u64,
    /// What changed since the version before it, in plain words.
    pub summary: String,
    path: PathBuf,
}

/// The history of every note in one notes directory.
pub struct History {
    dir: PathBuf,
}

impl History {
    pub fn new(notes_dir: &Path) -> Self {
        Self {
            dir: notes_dir.join(HISTORY_DIR),
        }
    }

    fn note_dir(&self, note_id: &str) -> io::Result<PathBuf> {
        if !crate::store::is_valid_id(note_id) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid note id",
            ));
        }
        Ok(self.dir.join(note_id))
    }

    /// Records `source` as a version unless its body equals the newest
    /// version's, or it is an edit within [`EDIT_INTERVAL_MS`] of the last
    /// version. Returns the version id when one was written.
    pub fn record(
        &self,
        note_id: &str,
        source: &str,
        author: &Author,
        reason: Reason,
        now_ms: u64,
    ) -> io::Result<Option<u64>> {
        let dir = self.note_dir(note_id)?;
        let existing = self.files(note_id)?;
        if let Some(newest) = existing.last() {
            let previous = fs::read_to_string(&newest.path).unwrap_or_default();
            if body(&previous) == body(source) {
                return Ok(None);
            }
            if reason == Reason::Edit && now_ms.saturating_sub(newest.id) < EDIT_INTERVAL_MS {
                return Ok(None);
            }
        }
        create_private_dir(&self.dir)?;
        create_private_dir(&dir)?;
        let mut id = now_ms.max(existing.last().map_or(0, |v| v.id + 1));
        while existing.iter().any(|v| v.id == id) {
            id += 1;
        }
        let name = format!("{id}~{}~{}.md", author.token(), reason.token());
        write_private(&dir.join(name), source)?;
        self.prune(note_id, now_ms)?;
        Ok(Some(id))
    }

    /// Remembers the exact text the store last wrote, so a change made
    /// behind its back can be noticed and the text before it kept, even when
    /// throttling kept no version of it.
    pub fn set_last_written(&self, note_id: &str, source: &str) -> io::Result<()> {
        let dir = self.note_dir(note_id)?;
        create_private_dir(&self.dir)?;
        create_private_dir(&dir)?;
        let path = dir.join(LAST_WRITTEN);
        let _ = fs::remove_file(path.with_extension("tmp"));
        write_private(&path, source)
    }

    /// The text the store last wrote for this note, if it remembers it.
    pub fn last_written(&self, note_id: &str) -> Option<String> {
        fs::read_to_string(self.note_dir(note_id).ok()?.join(LAST_WRITTEN)).ok()
    }

    /// Versions, newest first, each with a summary of what it changed.
    pub fn list(&self, note_id: &str) -> io::Result<Vec<Version>> {
        let files = self.files(note_id)?;
        let mut previous: Option<String> = None;
        let mut versions = Vec::with_capacity(files.len());
        for file in files {
            let source = fs::read_to_string(&file.path).unwrap_or_default();
            let summary = summarize(previous.as_deref(), &source, file.reason);
            previous = Some(source);
            versions.push(Version { summary, ..file });
        }
        versions.reverse();
        Ok(versions)
    }

    /// The full Markdown of one version.
    pub fn read(&self, note_id: &str, version: u64) -> io::Result<String> {
        let file = self
            .files(note_id)?
            .into_iter()
            .find(|file| file.id == version)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("note {note_id} has no version {version}"),
                )
            })?;
        fs::read_to_string(file.path)
    }

    /// Version files in time order (oldest first), without summaries.
    fn files(&self, note_id: &str) -> io::Result<Vec<Version>> {
        let dir = self.note_dir(note_id)?;
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        let mut files = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let Some(stem) = name.strip_suffix(".md") else {
                continue;
            };
            let mut parts = stem.splitn(3, '~');
            let (Some(id), Some(author), Some(reason)) = (parts.next(), parts.next(), parts.next())
            else {
                continue;
            };
            let (Ok(id), Some(author), Some(reason)) =
                (id.parse(), Author::parse(author), Reason::parse(reason))
            else {
                continue;
            };
            let bytes = entry.metadata().map(|m| m.len()).unwrap_or(0);
            files.push(Version {
                id,
                author,
                reason,
                bytes,
                summary: String::new(),
                path,
            });
        }
        files.sort_by_key(|file| file.id);
        Ok(files)
    }

    fn prune(&self, note_id: &str, now_ms: u64) -> io::Result<()> {
        let files = self.files(note_id)?;
        let stamps: Vec<(u64, u64)> = files.iter().map(|f| (f.id, f.bytes)).collect();
        let keep = retained(&stamps, now_ms);
        for (file, keep) in files.iter().zip(keep) {
            if !keep {
                fs::remove_file(&file.path)?;
            }
        }
        Ok(())
    }
}

/// Which versions (oldest first, as `(id, bytes)`) to keep at `now_ms`.
pub fn retained(versions: &[(u64, u64)], now_ms: u64) -> Vec<bool> {
    let mut keep = vec![false; versions.len()];
    let mut last_bucket = None;
    // Newest first, so each bucket keeps its newest version.
    for (index, (id, _)) in versions.iter().enumerate().rev() {
        let age = now_ms.saturating_sub(*id);
        let bucket = if age < HOUR {
            None
        } else if age < DAY {
            Some((1, id / (10 * MINUTE)))
        } else if age < 30 * DAY {
            Some((2, id / DAY))
        } else {
            Some((3, id / (7 * DAY)))
        };
        let newest = index + 1 == versions.len();
        match bucket {
            None => keep[index] = true,
            Some(bucket) if newest || last_bucket != Some(bucket) => {
                keep[index] = true;
                last_bucket = Some(bucket);
            }
            Some(_) => {}
        }
    }
    // Caps drop the oldest kept versions, never the newest.
    let mut count = 0;
    let mut bytes = 0;
    for (index, (_, size)) in versions.iter().enumerate().rev() {
        if !keep[index] {
            continue;
        }
        let newest = index + 1 == versions.len();
        if !newest && (count >= MAX_VERSIONS || bytes + size > MAX_BYTES) {
            keep[index] = false;
            continue;
        }
        count += 1;
        bytes += size;
    }
    keep
}

/// The note without its front matter: what a version is compared by.
pub fn body(source: &str) -> &str {
    let Some(rest) = source.strip_prefix("---\n") else {
        return source;
    };
    match rest.find("\n---\n") {
        Some(end) => &rest[end + "\n---\n".len()..],
        None => source,
    }
}

/// Plain-language change summary, e.g. "added 3 lines, removed 1, 2 to-dos done".
fn summarize(previous: Option<&str>, source: &str, reason: Reason) -> String {
    let mut parts = Vec::new();
    match reason {
        Reason::Restore(from) => {
            parts.push(format!("restored the version from {}", describe_time(from)))
        }
        Reason::BeforeWrite => parts.push("before an outside change".into()),
        Reason::BeforeRestore => parts.push("before a restore".into()),
        Reason::Edit | Reason::Write => {}
    }
    let Some(previous) = previous else {
        parts.push("first version".into());
        return parts.join("; ");
    };
    let (_, old) = markdown::parse(previous);
    let (_, new) = markdown::parse(source);
    if old.title != new.title {
        parts.push(format!("renamed to \"{}\"", new.title));
    }
    let old_lines: Vec<&str> = body(previous)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .collect();
    let new_lines: Vec<&str> = body(source)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .collect();
    let added = new_lines.iter().filter(|l| !old_lines.contains(l)).count();
    let removed = old_lines.iter().filter(|l| !new_lines.contains(l)).count();
    let (old_done, old_total) = old.todo_progress();
    let (new_done, new_total) = new.todo_progress();
    let checked = new_done.saturating_sub(old_done);
    let new_todos = new_total.saturating_sub(old_total);
    if new_todos > 0 {
        parts.push(plural(new_todos, "new to-do", "new to-dos"));
    }
    if checked > 0 {
        parts.push(format!("{} done", plural(checked, "to-do", "to-dos")));
    }
    // Checking a to-do rewrites its line; don't count it twice.
    let edited = added.min(removed);
    let added = added - edited;
    let removed = removed - edited;
    let changed = edited.saturating_sub(checked);
    if changed > 0 {
        parts.push(format!("changed {}", plural(changed, "line", "lines")));
    }
    if added > new_todos {
        parts.push(format!(
            "added {}",
            plural(added - new_todos, "line", "lines")
        ));
    }
    if removed > 0 {
        parts.push(format!("removed {}", plural(removed, "line", "lines")));
    }
    if parts.is_empty() {
        parts.push("formatting".into());
    }
    parts.join(", ")
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// `2026-09-30 14:05` (UTC), for summaries and listings.
pub fn describe_time(ms: u64) -> String {
    let (y, mo, d, h, mi, _) = crate::store::civil(ms / 1000);
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}")
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

fn create_private_dir(dir: &Path) -> io::Result<()> {
    diri_platform::private::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn write_private(path: &Path, contents: &str) -> io::Result<()> {
    let tmp = path.with_extension("tmp");
    let result = (|| {
        let mut file = diri_platform::private::options()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        file.write_all(contents.as_bytes())?;
        file.sync_data()?;
        fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: u64 = 1_790_000_000_000;

    fn note(body: &str) -> String {
        format!("---\nid: n1\n---\n# Plan\n\n{body}\n")
    }

    #[test]
    fn edits_are_throttled_and_deduped_but_outside_writes_are_not() {
        let temp = tempfile::tempdir().unwrap();
        let history = History::new(temp.path());
        let user = Author::User;
        assert!(
            history
                .record("n1", &note("a"), &user, Reason::Edit, T0)
                .unwrap()
                .is_some()
        );
        // Typing 10 s later: throttled.
        assert!(
            history
                .record("n1", &note("ab"), &user, Reason::Edit, T0 + 10_000)
                .unwrap()
                .is_none()
        );
        // An agent is about to write: always kept.
        assert!(
            history
                .record("n1", &note("ab"), &user, Reason::BeforeWrite, T0 + 11_000)
                .unwrap()
                .is_some()
        );
        // Same body with other front matter: not a version.
        let restamped = note("ab").replace("id: n1", "id: n1\nsession: s_1");
        assert!(
            history
                .record("n1", &restamped, &user, Reason::BeforeWrite, T0 + 12_000)
                .unwrap()
                .is_none()
        );
        // A minute after the last version typing is kept again.
        assert!(
            history
                .record("n1", &note("abc"), &user, Reason::Edit, T0 + 72_000)
                .unwrap()
                .is_some()
        );

        let versions = history.list("n1").unwrap();
        assert_eq!(versions.len(), 3);
        assert_eq!(versions[0].author, Author::User);
        assert_eq!(versions[2].summary, "first version");
        assert_eq!(history.read("n1", versions[1].id).unwrap(), note("ab"));
    }

    #[test]
    fn summaries_speak_plainly() {
        let temp = tempfile::tempdir().unwrap();
        let history = History::new(temp.path());
        let agent = Author::Session("s_agent".into());
        history
            .record(
                "n1",
                &note("- [ ] call the venue\n- [ ] book flights"),
                &Author::User,
                Reason::Edit,
                T0,
            )
            .unwrap();
        history
            .record(
                "n1",
                &note("- [x] call the venue\n- [ ] book flights\n- [ ] print badges"),
                &agent,
                Reason::Edit,
                T0 + HOUR,
            )
            .unwrap();
        let versions = history.list("n1").unwrap();
        assert_eq!(versions[0].author, agent);
        assert_eq!(versions[0].summary, "1 new to-do, 1 to-do done");
    }

    #[test]
    fn versions_are_owner_only_and_ids_never_collide() {
        let temp = tempfile::tempdir().unwrap();
        let history = History::new(temp.path());
        for n in 0..3 {
            history
                .record(
                    "n1",
                    &note(&n.to_string()),
                    &Author::Cli,
                    Reason::BeforeWrite,
                    T0,
                )
                .unwrap()
                .unwrap();
        }
        let versions = history.list("n1").unwrap();
        assert_eq!(
            versions.iter().map(|v| v.id).collect::<Vec<_>>(),
            vec![T0 + 2, T0 + 1, T0]
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let dir = temp.path().join(HISTORY_DIR).join("n1");
            assert_eq!(
                fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
                0o700
            );
            let file = fs::read_dir(&dir).unwrap().next().unwrap().unwrap().path();
            assert_eq!(
                fs::metadata(file).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        assert!(history.list("../etc").is_err());
    }

    #[test]
    fn retention_is_dense_recently_and_sparse_later() {
        let now = T0 + 90 * DAY;
        // One version a minute for the last two hours, hourly for 3 days
        // before that, and daily for 80 days before that.
        let mut stamps = Vec::new();
        for day in (0..80).rev() {
            stamps.push((now - 10 * DAY - day * DAY, 1));
        }
        for hour in (0..72).rev() {
            stamps.push((now - 2 * HOUR - hour * HOUR - 1, 1));
        }
        for minute in (0..120).rev() {
            stamps.push((now - minute * MINUTE, 1));
        }
        stamps.sort();
        let keep = retained(&stamps, now);
        let kept: Vec<u64> = stamps
            .iter()
            .zip(&keep)
            .filter(|(_, k)| **k)
            .map(|(s, _)| s.0)
            .collect();
        let within = |lo: u64, hi: u64| {
            kept.iter()
                .filter(|t| now - **t >= lo && now - **t < hi)
                .count()
        };
        assert_eq!(within(0, HOUR), 60, "every version from the last hour");
        assert!(
            within(HOUR, DAY) <= 6 * 23 + 1,
            "at most one per ten minutes"
        );
        assert!(within(DAY, 30 * DAY) <= 30, "at most one per day");
        assert!(within(30 * DAY, 400 * DAY) <= 10, "about one per week");
        assert!(*keep.last().unwrap(), "the newest is always kept");
        assert!(kept.len() <= MAX_VERSIONS);
    }

    #[test]
    fn caps_drop_the_oldest_first() {
        let now = T0;
        let stamps: Vec<(u64, u64)> = (0..300).map(|n| (now - 300 + n, 1)).collect();
        let keep = retained(&stamps, now);
        assert_eq!(keep.iter().filter(|k| **k).count(), MAX_VERSIONS);
        assert!(!keep[0] && keep[299]);
        let big: Vec<(u64, u64)> = (0..5).map(|n| (now - 5 + n, MAX_BYTES / 2)).collect();
        let keep = retained(&big, now);
        assert_eq!(keep, vec![false, false, false, true, true]);
    }
}
