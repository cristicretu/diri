//! Notes on disk: one Markdown file per note in a single flat directory.
//!
//! Organisation lives in front matter (`project`, `pinned`, `archived`), so
//! moving a note between Inbox, a project, and the archive rewrites one file
//! and never renames anything. Writes are atomic (temp file + rename) so a
//! crash, the CLI, and the app can never leave a half-written note.

use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::doc::{BlockKind, Document};
use crate::history::{self, Author, History, Reason};
use crate::markdown::{self, FrontMatter};
use crate::mention::MentionTarget;

/// Notes larger than this are listed but not loaded into the editor.
pub const MAX_NOTE_BYTES: u64 = 4 * 1024 * 1024;
const SNIPPET_CHARS: usize = 140;
const TRASH_DIR: &str = ".trash";
const LOCK_FILE: &str = ".lock";
/// Pictures pasted or dropped into notes: `assets/<note id>/<hash>.<ext>`.
pub const ASSETS_DIR: &str = "assets";
/// The largest picture a note takes in: a screenshot, not a video.
pub const MAX_ASSET_BYTES: usize = 25 * 1024 * 1024;
/// Picture formats a note stores and shows.
pub const IMAGE_EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "webp", "heic", "tiff", "bmp", "svg",
];

pub const KEY_ID: &str = "id";
pub const KEY_CREATED: &str = "created";
pub const KEY_PROJECT: &str = "project";
pub const KEY_PINNED: &str = "pinned";
pub const KEY_ARCHIVED: &str = "archived";
/// The Session that shows this note in the sidebar. Written when the Engine
/// creates or adopts the note and kept afterwards, so a note whose Session
/// was removed on purpose is never adopted again.
pub const KEY_SESSION: &str = "session";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Note {
    pub front: FrontMatter,
    pub doc: Document,
}

impl Note {
    pub fn project(&self) -> Option<&str> {
        self.front.get(KEY_PROJECT).filter(|p| !p.is_empty())
    }

    pub fn to_markdown(&self) -> String {
        markdown::write(&self.front, &self.doc)
    }
}

/// A listing entry: everything the sidebar and note list show without
/// loading the whole note into the editor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NoteMeta {
    pub id: String,
    pub path: PathBuf,
    pub title: String,
    pub snippet: String,
    pub project: Option<String>,
    pub pinned: bool,
    pub archived: bool,
    /// Unix seconds.
    pub created: u64,
    /// Unix milliseconds of the file's last write.
    pub modified_ms: u64,
    pub todos_done: usize,
    pub todos_total: usize,
    /// Open to-dos in document order: (block index, text).
    pub open_todos: Vec<(usize, String)>,
    /// The Session this note was given ([`KEY_SESSION`]); `None` for notes
    /// written before note Sessions or while the Engine was down.
    pub session: Option<String>,
    /// Distinct `diri://` mention targets, in first-mention order.
    pub mentions: Vec<MentionTarget>,
    /// Lower-cased title + body, for search.
    pub haystack: String,
}

impl NoteMeta {
    pub fn display_title(&self) -> &str {
        if self.title.trim().is_empty() {
            "Untitled"
        } else {
            &self.title
        }
    }

    pub fn from_note(id: &str, path: PathBuf, note: &Note, modified_ms: u64) -> Self {
        let (todos_done, todos_total) = note.doc.todo_progress();
        let open_todos = note
            .doc
            .blocks
            .iter()
            .enumerate()
            .filter(|(_, b)| b.kind == BlockKind::Todo { checked: false } && !b.text.is_empty())
            .map(|(i, b)| (i, b.text.clone()))
            .collect();
        let body = note.doc.plain_text();
        let snippet: String = body
            .split('\n')
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .collect::<Vec<_>>()
            .join(" · ")
            .chars()
            .take(SNIPPET_CHARS)
            .collect();
        let created = note
            .front
            .get(KEY_CREATED)
            .and_then(parse_timestamp)
            .unwrap_or(modified_ms / 1000);
        Self {
            id: id.to_owned(),
            path,
            title: note.doc.title.clone(),
            snippet,
            project: note.project().map(str::to_owned),
            pinned: note.front.flag(KEY_PINNED),
            archived: note.front.flag(KEY_ARCHIVED),
            created,
            modified_ms,
            todos_done,
            todos_total,
            open_todos,
            session: note
                .front
                .get(KEY_SESSION)
                .filter(|s| !s.is_empty())
                .map(str::to_owned),
            mentions: note.doc.mentions(),
            haystack: format!("{}\n{}", note.doc.title, body).to_lowercase(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SaveOutcome {
    /// Written; `source` is what the file now holds.
    Saved { source: String },
    /// Someone else changed (or removed) the file since it was loaded.
    Conflict { current: Option<String> },
}

pub struct NoteStore {
    dir: PathBuf,
}

impl NoteStore {
    /// `<app support>/notes`, beside every other piece of Diri state.
    pub fn default_dir(home: impl AsRef<Path>) -> PathBuf {
        diri_proto::paths::DirijorPaths::app_support(home).join("notes")
    }

    /// Where this process should keep notes: `DIRI_NOTES_DIR` when set
    /// (tests, fixtures), else beside the rest of Diri's state, honouring the
    /// same `DIRIJOR_APP_SUPPORT` override every other component uses.
    pub fn resolve_dir() -> Option<PathBuf> {
        if let Some(dir) =
            std::env::var_os(diri_proto::paths::ENV_NOTES_DIR).filter(|d| !d.is_empty())
        {
            return Some(PathBuf::from(dir));
        }
        if let Some(support) =
            std::env::var_os(diri_proto::paths::ENV_APP_SUPPORT).filter(|d| !d.is_empty())
        {
            return Some(PathBuf::from(support).join("notes"));
        }
        let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
        Some(Self::default_dir(home))
    }

    pub fn open(dir: impl Into<PathBuf>) -> io::Result<Self> {
        let dir = dir.into();
        diri_platform::private::create_dir_all(&dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
        }
        Ok(Self { dir })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn path_for(&self, id: &str) -> io::Result<PathBuf> {
        validate_id(id)?;
        Ok(self.dir.join(format!("{id}.md")))
    }

    /// Every note, newest modification first. Unreadable files are skipped.
    pub fn list(&self) -> io::Result<Vec<NoteMeta>> {
        let mut notes = Vec::new();
        for entry in fs::read_dir(&self.dir)? {
            let entry = entry?;
            let path = entry.path();
            let Some(id) = note_id(&path) else { continue };
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            if !metadata.is_file() || metadata.len() > MAX_NOTE_BYTES {
                continue;
            }
            let Ok(source) = fs::read_to_string(&path) else {
                continue;
            };
            let note = parse_note(&source);
            notes.push(NoteMeta::from_note(
                &id,
                path,
                &note,
                millis(metadata.modified().ok()),
            ));
        }
        notes.sort_by(|a, b| b.modified_ms.cmp(&a.modified_ms).then(b.id.cmp(&a.id)));
        Ok(notes)
    }

    pub fn load(&self, id: &str) -> io::Result<Note> {
        let path = self.path_for(id)?;
        let metadata = fs::metadata(&path)?;
        if metadata.len() > MAX_NOTE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "note is too large to open",
            ));
        }
        Ok(parse_note(&fs::read_to_string(path)?))
    }

    pub fn meta(&self, id: &str) -> io::Result<NoteMeta> {
        let path = self.path_for(id)?;
        let note = self.load(id)?;
        let modified = fs::metadata(&path)?.modified().ok();
        Ok(NoteMeta::from_note(id, path, &note, millis(modified)))
    }

    /// Atomically replaces the note's file.
    pub fn save(&self, id: &str, note: &Note) -> io::Result<()> {
        let _lock = self.lock()?;
        self.write(id, &note.to_markdown())
    }

    /// Saves only when the file still holds `expected` (the source the caller
    /// loaded), so an editor with unsaved typing never overwrites what the
    /// CLI or an agent wrote meanwhile. A missing file is a conflict too.
    pub fn save_if_unchanged(
        &self,
        id: &str,
        note: &Note,
        expected: &str,
    ) -> io::Result<SaveOutcome> {
        let _lock = self.lock()?;
        let current = match fs::read_to_string(self.path_for(id)?) {
            Ok(current) => Some(current),
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => return Err(e),
        };
        if current.as_deref() != Some(expected) {
            return Ok(SaveOutcome::Conflict { current });
        }
        let source = note.to_markdown();
        self.write(id, &source)?;
        let _ = self
            .history()
            .record(id, &source, &Author::User, Reason::Edit, history::now_ms());
        Ok(SaveOutcome::Saved { source })
    }

    /// Read-modify-write under the store lock: `edit` sees the note as it is
    /// on disk now, and the file is rewritten only if `edit` changed it.
    /// Every writer outside the editor (CLI, agents) goes through here, so
    /// history keeps the note as it stood before the write and the result,
    /// attributed to `author`.
    pub fn update<T>(
        &self,
        id: &str,
        author: &Author,
        edit: impl FnOnce(&mut Note) -> io::Result<T>,
    ) -> io::Result<(Note, T)> {
        let _lock = self.lock()?;
        // A direct file edit since the last store write is kept first.
        let before = parse_note(&self.notice_locked(id)?);
        let mut note = before.clone();
        let out = edit(&mut note)?;
        if note != before {
            let history = self.history();
            let now = history::now_ms();
            // History is a safety net; a full disk must not block the write.
            let _ = history.record(
                id,
                &before.to_markdown(),
                &Author::User,
                Reason::BeforeWrite,
                now,
            );
            let source = note.to_markdown();
            self.write(id, &source)?;
            let _ = history.record(id, &source, author, Reason::Write, now);
        }
        Ok((note, out))
    }

    /// Puts back the text of `version`, keeping the note's current front
    /// matter (project, pin, Session). The current text is kept as a version
    /// first, so a restore can itself be undone.
    pub fn restore_version(&self, id: &str, version: u64, author: &Author) -> io::Result<Note> {
        let _lock = self.lock()?;
        let history = self.history();
        let old = parse_note(&history.read(id, version)?);
        let current = self.load(id)?;
        let now = history::now_ms();
        let _ = history.record(
            id,
            &current.to_markdown(),
            &Author::User,
            Reason::BeforeRestore,
            now,
        );
        let restored = Note {
            front: current.front,
            doc: old.doc,
        };
        let source = restored.to_markdown();
        self.write(id, &source)?;
        let _ = history.record(id, &source, author, Reason::Restore(version), now);
        Ok(restored)
    }

    /// Every note's version history, beside the notes.
    pub fn history(&self) -> History {
        History::new(&self.dir)
    }

    /// An exclusive advisory lock over the whole store, held for one
    /// read-modify-write and released when the returned file drops. Not
    /// reentrant: never take it twice on one thread.
    fn lock(&self) -> io::Result<fs::File> {
        let file = diri_platform::private::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.dir.join(LOCK_FILE))?;
        diri_platform::private::restrict(&file)?;
        file.lock()?;
        Ok(file)
    }

    fn write(&self, id: &str, contents: &str) -> io::Result<()> {
        let path = self.path_for(id)?;
        let tmp = self.dir.join(format!(".{id}.{}.tmp", nonce()));
        let result = (|| {
            let mut file = diri_platform::private::options()
                .write(true)
                .create_new(true)
                .open(&tmp)?;
            file.write_all(contents.as_bytes())?;
            file.sync_data()?;
            fs::rename(&tmp, &path)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&tmp);
        } else {
            let _ = self.history().set_last_written(id, contents);
        }
        result
    }

    /// Takes in a change made to the file behind the store's back (another
    /// editor, an agent's own file tools): history keeps the text as the
    /// store last wrote it and then the new text, by [`Author::File`], and a
    /// front matter the edit broke is repaired from the last known one so
    /// the note keeps its identity. Returns the file's text, repaired.
    pub fn notice_outside_change(&self, id: &str) -> io::Result<String> {
        let _lock = self.lock()?;
        self.notice_locked(id)
    }

    fn notice_locked(&self, id: &str) -> io::Result<String> {
        let current = fs::read_to_string(self.path_for(id)?)?;
        let history = self.history();
        let Some(known) = history.last_written(id) else {
            // Nothing written through the store yet: nothing to compare.
            return Ok(current);
        };
        if known == current {
            return Ok(current);
        }
        let now = history::now_ms();
        let _ = history.record(id, &known, &Author::User, Reason::BeforeWrite, now);
        let mut note = parse_note(&current);
        let repaired = repair_front(&mut note.front, id, &parse_note(&known).front);
        let source = if repaired {
            note.to_markdown()
        } else {
            current
        };
        if repaired {
            self.write(id, &source)?;
        } else {
            let _ = history.set_last_written(id, &source);
        }
        let _ = history.record(id, &source, &Author::File, Reason::Write, now);
        Ok(source)
    }

    /// Stores a picture for note `id` and returns the path a note links it
    /// by, relative to the notes folder. Content-addressed, so pasting the
    /// same screenshot twice keeps one file; written atomically.
    pub fn save_asset(&self, id: &str, bytes: &[u8], extension: &str) -> io::Result<String> {
        validate_id(id)?;
        let extension = extension.to_ascii_lowercase();
        if !IMAGE_EXTENSIONS.contains(&extension.as_str()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "not a picture format notes keep",
            ));
        }
        if bytes.is_empty() || bytes.len() > MAX_ASSET_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "picture is empty or too large",
            ));
        }
        let folder = self.dir.join(ASSETS_DIR).join(id);
        diri_platform::private::create_dir_all(&folder)?;
        let name = format!("{:016x}.{extension}", fnv1a(bytes));
        let path = folder.join(&name);
        if !path.exists() {
            let tmp = folder.join(format!(".{name}.{}.tmp", nonce()));
            let result = (|| {
                let mut file = diri_platform::private::options()
                    .write(true)
                    .create_new(true)
                    .open(&tmp)?;
                file.write_all(bytes)?;
                file.sync_data()?;
                fs::rename(&tmp, &path)
            })();
            if result.is_err() {
                let _ = fs::remove_file(&tmp);
            }
            result?;
        }
        Ok(format!("{ASSETS_DIR}/{id}/{name}"))
    }

    /// Copies a picture file into note `id`'s assets (a Finder drop).
    pub fn import_asset(&self, id: &str, source: &Path) -> io::Result<String> {
        let extension = source
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or_default()
            .to_owned();
        let size = fs::metadata(source)?.len();
        if size > MAX_ASSET_BYTES as u64 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "picture is too large",
            ));
        }
        self.save_asset(id, &fs::read(source)?, &extension)
    }

    /// The file a note's image `src` names, when it is a relative path that
    /// stays inside the notes folder. URLs and escapes (`..`, absolute
    /// paths) resolve to nothing.
    pub fn resolve_asset(&self, src: &str) -> Option<PathBuf> {
        resolve_asset(&self.dir, src)
    }

    /// Creates a note and returns its id. `project` is a project root path.
    pub fn create(&self, doc: Document, project: Option<&str>) -> io::Result<(String, Note)> {
        self.create_for_session(doc, project, None, &Author::User)
    }

    /// Creates a note already stamped with the Session that will show it.
    pub fn create_for_session(
        &self,
        doc: Document,
        project: Option<&str>,
        session: Option<&str>,
        author: &Author,
    ) -> io::Result<(String, Note)> {
        let now = SystemTime::now();
        let id = new_id(now);
        let mut front = FrontMatter::default();
        front.set(KEY_ID, Some(id.clone()));
        front.set(KEY_CREATED, Some(format_timestamp(secs(now))));
        front.set(KEY_PROJECT, project.map(str::to_owned));
        front.set(KEY_SESSION, session.map(str::to_owned));
        let note = Note { front, doc };
        self.save(&id, &note)?;
        let _ = self.history().record(
            &id,
            &note.to_markdown(),
            author,
            Reason::Write,
            history::now_ms(),
        );
        Ok((id, note))
    }

    /// Appends Markdown to a note's body (quick capture from the CLI).
    pub fn append(&self, id: &str, markdown_body: &str, author: &Author) -> io::Result<Note> {
        self.update(id, author, |note| {
            append_markdown(note, markdown_body);
            Ok(())
        })
        .map(|(note, ())| note)
    }

    /// Moves the note into `.trash/`, from where it can be restored by hand.
    pub fn trash(&self, id: &str) -> io::Result<PathBuf> {
        let path = self.path_for(id)?;
        let trash = self.dir.join(TRASH_DIR);
        diri_platform::private::create_dir_all(&trash)?;
        let target = trash.join(format!("{id}.md"));
        fs::rename(&path, &target)?;
        Ok(target)
    }

    /// Restores a note trashed by [`Self::trash`].
    pub fn restore(&self, id: &str) -> io::Result<()> {
        let path = self.path_for(id)?;
        fs::rename(self.dir.join(TRASH_DIR).join(format!("{id}.md")), path)
    }
}

/// Appends Markdown blocks to the end of a note, dropping blank paragraphs.
pub fn append_markdown(note: &mut Note, markdown_body: &str) {
    let (_, extra) = markdown::parse(&format!("\n{markdown_body}"));
    let mut blocks: Vec<_> = note
        .doc
        .blocks
        .iter()
        .filter(|b| !(b.kind == BlockKind::Paragraph && b.text.is_empty()))
        .cloned()
        .collect();
    blocks.extend(
        extra
            .blocks
            .into_iter()
            .filter(|b| !b.text.is_empty() || b.kind == BlockKind::Divider),
    );
    note.doc = Document::new(note.doc.title.clone(), blocks);
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resolve {
    Found(Box<NoteMeta>),
    NotFound,
    /// More than one note matches; each is listed so the caller can be exact.
    Ambiguous(Vec<NoteMeta>),
}

/// Finds a note by exact id, else exact title (case-insensitive), else the
/// single unarchived note whose title contains `query`.
pub fn resolve(notes: &[NoteMeta], query: &str) -> Resolve {
    if let Some(exact) = notes.iter().find(|n| n.id == query) {
        return Resolve::Found(Box::new(exact.clone()));
    }
    let needle = query.to_lowercase();
    let exact_title: Vec<&NoteMeta> = notes
        .iter()
        .filter(|n| n.title.to_lowercase() == needle)
        .collect();
    if let [one] = exact_title.as_slice() {
        return Resolve::Found(Box::new((*one).clone()));
    }
    let matches: Vec<NoteMeta> = notes
        .iter()
        .filter(|n| !n.archived && n.title.to_lowercase().contains(&needle))
        .cloned()
        .collect();
    match matches.len() {
        0 => Resolve::NotFound,
        1 => Resolve::Found(Box::new(matches.into_iter().next().expect("one match"))),
        _ => Resolve::Ambiguous(matches),
    }
}

/// Puts back the identity keys a direct edit dropped or changed (`id`,
/// `created`, `project`, `session`, pin and archive state), taking them from
/// `known`. The id always matches the file name. Returns whether anything
/// changed. Other keys the edit added are kept.
pub fn repair_front(front: &mut FrontMatter, id: &str, known: &FrontMatter) -> bool {
    let mut changed = false;
    if front.get(KEY_ID) != Some(id) {
        front.set(KEY_ID, Some(id.to_owned()));
        changed = true;
    }
    for key in [
        KEY_CREATED,
        KEY_PROJECT,
        KEY_SESSION,
        KEY_PINNED,
        KEY_ARCHIVED,
    ] {
        if front.get(key).is_none()
            && let Some(value) = known.get(key)
        {
            front.set(key, Some(value.to_owned()));
            changed = true;
        }
    }
    changed
}

pub fn parse_note(source: &str) -> Note {
    let (front, doc) = markdown::parse(source);
    Note { front, doc }
}

/// The id of a note file, or `None` for anything else in the directory.
pub fn note_id(path: &Path) -> Option<String> {
    if path.extension()? != "md" {
        return None;
    }
    let stem = path.file_stem()?.to_str()?;
    validate_id(stem).ok()?;
    Some(stem.to_owned())
}

/// Note ids (and the session ids mentions carry) are single path-safe
/// components: ASCII alphanumerics, `-` and `_`, never hidden.
pub fn is_valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && !id.starts_with('.')
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn validate_id(id: &str) -> io::Result<()> {
    if is_valid_id(id) {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid note id",
        ))
    }
}

/// Time-sortable, collision-resistant ids: `20260930-142501-3fa9`.
pub fn new_id(now: SystemTime) -> String {
    let (y, mo, d, h, mi, s) = civil(secs(now));
    format!("{y:04}{mo:02}{d:02}-{h:02}{mi:02}{s:02}-{}", &nonce()[..4])
}

fn nonce() -> String {
    let mut bytes = [0u8; 4];
    if getrandom::fill(&mut bytes).is_err() {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos());
        bytes = nanos.to_le_bytes();
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn secs(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn millis(time: Option<SystemTime>) -> u64 {
    time.and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_millis() as u64)
}

/// RFC 3339 in UTC, seconds precision.
pub fn format_timestamp(unix: u64) -> String {
    let (y, mo, d, h, mi, s) = civil(unix);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

pub fn parse_timestamp(text: &str) -> Option<u64> {
    let text = text.trim().strip_suffix('Z')?;
    let (date, time) = text.split_once('T')?;
    let mut date = date.split('-').map(|p| p.parse::<i64>());
    let (y, mo, d) = (date.next()?.ok()?, date.next()?.ok()?, date.next()?.ok()?);
    let mut time = time.split(':').map(|p| p.parse::<i64>());
    let (h, mi, s) = (time.next()?.ok()?, time.next()?.ok()?, time.next()?.ok()?);
    let days = days_from_civil(y, mo, d);
    u64::try_from(days * 86_400 + h * 3600 + mi * 60 + s).ok()
}

/// Unix seconds → (year, month, day, hour, minute, second) in UTC.
pub fn civil(unix: u64) -> (i64, u32, u32, u32, u32, u32) {
    let days = (unix / 86_400) as i64;
    let rem = unix % 86_400;
    // Howard Hinnant's days-to-civil.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (
        y,
        m,
        d,
        (rem / 3600) as u32,
        (rem % 3600 / 60) as u32,
        (rem % 60) as u32,
    )
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::Block;

    #[test]
    fn timestamps_round_trip() {
        for unix in [0, 951_782_400, 1_790_000_000, 4_102_444_800] {
            assert_eq!(parse_timestamp(&format_timestamp(unix)), Some(unix));
        }
        assert_eq!(format_timestamp(1_790_769_600), "2026-09-30T12:00:00Z");
    }

    #[test]
    fn create_list_append_trash() {
        let dir = tempfile::tempdir().unwrap();
        let store = NoteStore::open(dir.path().join("notes")).unwrap();
        let doc = Document::new(
            "Groceries",
            vec![
                Block::new(0, BlockKind::Todo { checked: false }, "milk"),
                Block::new(0, BlockKind::Todo { checked: true }, "eggs"),
            ],
        );
        let (id, _) = store.create(doc, Some("/tmp/proj")).unwrap();
        let listed = store.list().unwrap();
        assert_eq!(listed.len(), 1);
        let meta = &listed[0];
        assert_eq!(meta.title, "Groceries");
        assert_eq!(meta.project.as_deref(), Some("/tmp/proj"));
        assert_eq!((meta.todos_done, meta.todos_total), (1, 2));
        assert_eq!(meta.open_todos, vec![(0, "milk".to_owned())]);

        let note = store.append(&id, "- [ ] bread", &Author::Cli).unwrap();
        assert_eq!(note.doc.todo_progress(), (1, 3));
        assert!(store.load(&id).unwrap().doc.same_content(&note.doc));

        store.trash(&id).unwrap();
        assert!(store.list().unwrap().is_empty());
        store.restore(&id).unwrap();
        assert_eq!(store.list().unwrap().len(), 1);
    }

    #[test]
    fn concurrent_appends_are_never_lost() {
        let dir = tempfile::tempdir().unwrap();
        let store = NoteStore::open(dir.path().join("notes")).unwrap();
        let (id, _) = store
            .create(Document::new("Log", Vec::new()), None)
            .unwrap();
        std::thread::scope(|scope| {
            for writer in 0..8 {
                let (store, id) = (&store, &id);
                scope.spawn(move || {
                    // A store per writer, as separate processes would have.
                    let store = NoteStore::open(store.dir()).unwrap();
                    for n in 0..10 {
                        store
                            .append(id, &format!("- w{writer} n{n}"), &Author::Cli)
                            .unwrap();
                    }
                });
            }
        });
        assert_eq!(store.load(&id).unwrap().doc.blocks.len(), 80);
    }

    #[test]
    fn save_if_unchanged_refuses_to_overwrite_outside_writes() {
        let dir = tempfile::tempdir().unwrap();
        let store = NoteStore::open(dir.path().join("notes")).unwrap();
        let (id, note) = store
            .create(Document::new("PRD", Vec::new()), None)
            .unwrap();
        let loaded = note.to_markdown();

        // An agent appends while the editor has unsaved typing.
        store
            .append(
                &id,
                "- [ ] from the agent",
                &Author::Session("s_agent".into()),
            )
            .unwrap();
        let mut typed = note.clone();
        append_markdown(&mut typed, "typed in the editor");
        let SaveOutcome::Conflict {
            current: Some(current),
        } = store.save_if_unchanged(&id, &typed, &loaded).unwrap()
        else {
            panic!("expected a conflict");
        };
        assert!(current.contains("from the agent"));
        assert!(
            store
                .load(&id)
                .unwrap()
                .to_markdown()
                .contains("from the agent")
        );

        // Against the current source the save goes through.
        let SaveOutcome::Saved { source } = store.save_if_unchanged(&id, &typed, &current).unwrap()
        else {
            panic!("expected a save");
        };
        assert_eq!(
            fs::read_to_string(store.path_for(&id).unwrap()).unwrap(),
            source
        );

        store.trash(&id).unwrap();
        assert_eq!(
            store.save_if_unchanged(&id, &typed, &source).unwrap(),
            SaveOutcome::Conflict { current: None }
        );
    }

    #[test]
    fn outside_writes_and_restores_are_versioned_and_undoable() {
        let dir = tempfile::tempdir().unwrap();
        let store = NoteStore::open(dir.path().join("notes")).unwrap();
        let (id, _) = store
            .create(Document::new("Launch", Vec::new()), None)
            .unwrap();
        // The person types (the editor saves, throttled).
        let mut typed = store.load(&id).unwrap();
        append_markdown(&mut typed, "Draft the announcement.");
        let loaded = fs::read_to_string(store.path_for(&id).unwrap()).unwrap();
        store.save_if_unchanged(&id, &typed, &loaded).unwrap();
        // An agent adds a to-do.
        let agent = Author::Session("s_agent".into());
        store.append(&id, "- [ ] book the venue", &agent).unwrap();

        let versions = store.history().list(&id).unwrap();
        let newest = &versions[0];
        assert_eq!(newest.author, agent);
        assert_eq!(newest.reason, Reason::Write);
        assert!(versions.iter().any(|v| {
            v.reason == Reason::BeforeWrite
                && store
                    .history()
                    .read(&id, v.id)
                    .unwrap()
                    .contains("Draft the announcement")
        }));

        // Restore the version before the agent wrote, then undo that restore.
        let mut pinned = store.load(&id).unwrap();
        pinned.front.set_flag(KEY_PINNED, true);
        store.save(&id, &pinned).unwrap();
        let before_agent = versions
            .iter()
            .find(|v| v.reason == Reason::BeforeWrite)
            .unwrap()
            .id;
        let restored = store
            .restore_version(&id, before_agent, &Author::User)
            .unwrap();
        assert!(!restored.to_markdown().contains("book the venue"));
        assert!(
            restored.front.flag(KEY_PINNED),
            "a restore keeps the note's place and pin"
        );
        let latest = store.history().list(&id).unwrap();
        assert_eq!(latest[0].reason, Reason::Restore(before_agent));
        // The text before the restore is always in history: here it already
        // was (the agent's write), so no duplicate was taken.
        let undo = latest[1].id;
        assert!(
            store
                .history()
                .read(&id, undo)
                .unwrap()
                .contains("book the venue")
        );
        store.restore_version(&id, undo, &Author::User).unwrap();
        assert!(
            store
                .load(&id)
                .unwrap()
                .to_markdown()
                .contains("book the venue")
        );

        // History outlives the trash.
        let count = store.history().list(&id).unwrap().len();
        store.trash(&id).unwrap();
        assert_eq!(store.history().list(&id).unwrap().len(), count);
        store.restore(&id).unwrap();
        assert!(
            store.list().unwrap().iter().all(|n| n.id == id),
            "history is not a note"
        );
    }

    #[test]
    fn update_skips_unchanged_writes() {
        let dir = tempfile::tempdir().unwrap();
        let store = NoteStore::open(dir.path().join("notes")).unwrap();
        let (id, _) = store
            .create(Document::new("Same", Vec::new()), None)
            .unwrap();
        let path = store.path_for(&id).unwrap();
        let before = fs::metadata(&path).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        let (_, answer) = store.update(&id, &Author::Cli, |_| Ok(42)).unwrap();
        assert_eq!(answer, 42);
        assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), before);
    }

    #[test]
    fn meta_lists_mentions() {
        let dir = tempfile::tempdir().unwrap();
        let store = NoteStore::open(dir.path().join("notes")).unwrap();
        let (id, _) = store.create(Document::new("M", Vec::new()), None).unwrap();
        store
            .append(
                &id,
                "Ask [@Codex](diri://session/s_1) about [@PRD](diri://note/n1)",
                &Author::Cli,
            )
            .unwrap();
        assert_eq!(
            store.meta(&id).unwrap().mentions,
            vec![
                MentionTarget::Session("s_1".into()),
                MentionTarget::Note("n1".into())
            ]
        );
    }

    #[test]
    fn rejects_path_like_ids() {
        let dir = tempfile::tempdir().unwrap();
        let store = NoteStore::open(dir.path()).unwrap();
        for id in ["../x", "a/b", ".hidden", ""] {
            assert!(store.path_for(id).is_err(), "{id}");
        }
    }
}

/// See [`NoteStore::resolve_asset`].
pub fn resolve_asset(dir: &Path, src: &str) -> Option<PathBuf> {
    if src.is_empty() || src.contains("://") || src.starts_with('/') || src.starts_with('~') {
        return None;
    }
    let relative = Path::new(src);
    let safe = relative
        .components()
        .all(|c| matches!(c, std::path::Component::Normal(_)));
    safe.then(|| dir.join(relative))
}

/// Whether `path` names a picture a note can hold.
pub fn is_image_path(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| IMAGE_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
}

/// A picture's pixel size from its file header: PNG, GIF, JPEG and WebP.
/// Reads at most 64 KiB and decodes nothing, so a view can size an image
/// before (or without) loading it.
pub fn image_size(path: &Path) -> Option<(u32, u32)> {
    use std::io::Read as _;
    let mut head = Vec::with_capacity(64 * 1024);
    fs::File::open(path)
        .ok()?
        .take(64 * 1024)
        .read_to_end(&mut head)
        .ok()?;
    image_size_of(&head)
}

fn image_size_of(b: &[u8]) -> Option<(u32, u32)> {
    let be16 = |i: usize| Some(u32::from(u16::from_be_bytes([*b.get(i)?, *b.get(i + 1)?])));
    let le16 = |i: usize| Some(u32::from(u16::from_le_bytes([*b.get(i)?, *b.get(i + 1)?])));
    let be32 = |i: usize| Some(u32::from_be_bytes(b.get(i..i + 4)?.try_into().ok()?));
    let le24 = |i: usize| {
        Some(
            u32::from(*b.get(i)?)
                | u32::from(*b.get(i + 1)?) << 8
                | u32::from(*b.get(i + 2)?) << 16,
        )
    };
    if b.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some((be32(16)?, be32(20)?));
    }
    if b.starts_with(b"GIF8") {
        return Some((le16(6)?, le16(8)?));
    }
    if b.starts_with(b"RIFF") && b.get(8..12) == Some(b"WEBP") {
        return match b.get(12..16)? {
            b"VP8X" => Some((le24(24)? + 1, le24(27)? + 1)),
            b"VP8 " => Some((le16(26)? & 0x3fff, le16(28)? & 0x3fff)),
            b"VP8L" => {
                let bits = u32::from_le_bytes(b.get(21..25)?.try_into().ok()?);
                Some(((bits & 0x3fff) + 1, ((bits >> 14) & 0x3fff) + 1))
            }
            _ => None,
        };
    }
    if b.starts_with(&[0xff, 0xd8]) {
        let mut i = 2;
        while i + 9 < b.len() {
            if b[i] != 0xff {
                i += 1;
                continue;
            }
            let marker = b[i + 1];
            let length = be16(i + 2)? as usize;
            // Start-of-frame markers carry the size; C4, C8 and CC do not.
            if (0xc0..=0xcf).contains(&marker) && !matches!(marker, 0xc4 | 0xc8 | 0xcc) {
                return Some((be16(i + 7)?, be16(i + 5)?));
            }
            i += 2 + length;
        }
    }
    None
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod asset_tests {
    use super::*;

    #[test]
    fn pictures_are_stored_once_beside_the_notes() {
        let dir = tempfile::tempdir().unwrap();
        let store = NoteStore::open(dir.path().join("notes")).unwrap();
        let (id, _) = store.create(Document::new("T", Vec::new()), None).unwrap();
        let png = b"\x89PNG fake";
        let a = store.save_asset(&id, png, "PNG").unwrap();
        let b = store.save_asset(&id, png, "png").unwrap();
        assert_eq!(a, b, "same bytes, same file");
        assert!(a.starts_with(&format!("assets/{id}/")) && a.ends_with(".png"));
        let path = store.resolve_asset(&a).unwrap();
        assert_eq!(fs::read(path).unwrap(), png);
        // Assets never show up as notes.
        assert_eq!(store.list().unwrap().len(), 1);
        assert!(store.save_asset(&id, png, "exe").is_err());
        assert!(store.save_asset("../x", png, "png").is_err());
        assert!(store.save_asset(&id, b"", "png").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn notes_pictures_and_trash_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
        let dir = tempfile::tempdir().unwrap();
        let notes = dir.path().join("notes");
        let store = NoteStore::open(&notes).unwrap();
        let (id, _) = store.create(Document::new("T", Vec::new()), None).unwrap();
        let asset = store.save_asset(&id, b"\x89PNG fake", "png").unwrap();
        let asset = store.resolve_asset(&asset).unwrap();
        assert_eq!(mode(&asset), 0o600);
        assert_eq!(mode(asset.parent().unwrap()), 0o700);
        assert_eq!(mode(&notes.join(ASSETS_DIR)), 0o700);
        let trashed = store.trash(&id).unwrap();
        assert_eq!(mode(trashed.parent().unwrap()), 0o700);
        assert_eq!(mode(&trashed), 0o600);
    }

    #[test]
    fn picture_sizes_come_from_headers() {
        let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        png.extend_from_slice(&1200u32.to_be_bytes());
        png.extend_from_slice(&520u32.to_be_bytes());
        assert_eq!(image_size_of(&png), Some((1200, 520)));
        let gif = b"GIF89a\x40\x01\xf0\x00";
        assert_eq!(image_size_of(gif), Some((320, 240)));
        let jpeg = [
            0xff, 0xd8, 0xff, 0xe0, 0x00, 0x04, 0x00, 0x00, 0xff, 0xc0, 0x00, 0x11, 0x08, 0x01,
            0xe0, 0x02, 0x80, 0x03, 0, 0, 0, 0,
        ];
        assert_eq!(image_size_of(&jpeg), Some((640, 480)));
        assert_eq!(image_size_of(b"not a picture"), None);
    }

    #[test]
    fn only_relative_paths_inside_the_folder_resolve() {
        let dir = Path::new("/notes");
        assert_eq!(
            resolve_asset(dir, "assets/n/a.png"),
            Some(PathBuf::from("/notes/assets/n/a.png"))
        );
        for bad in [
            "../secret.png",
            "/etc/x.png",
            "~/x.png",
            "https://a.b/c.png",
            "a/../../b.png",
            "",
        ] {
            assert_eq!(resolve_asset(dir, bad), None, "{bad}");
        }
        assert!(is_image_path(Path::new("Shot.JPEG")));
        assert!(!is_image_path(Path::new("notes.md")));
    }

    #[test]
    fn a_direct_file_edit_is_versioned_and_cannot_break_identity() {
        let dir = tempfile::tempdir().unwrap();
        let store = NoteStore::open(dir.path().join("notes")).unwrap();
        let (id, _) = store
            .create_for_session(
                Document::new("Plan", Vec::new()),
                Some("/work/p"),
                Some("s_note"),
                &Author::User,
            )
            .unwrap();
        // The person types; this save is throttled out of history but is
        // the last text the store wrote.
        let mut typed = store.load(&id).unwrap();
        append_markdown(&mut typed, "typed just now");
        let loaded = fs::read_to_string(store.path_for(&id).unwrap()).unwrap();
        store.save_if_unchanged(&id, &typed, &loaded).unwrap();
        // Someone rewrites the file by hand and drops the front matter.
        fs::write(
            store.path_for(&id).unwrap(),
            "# Plan\n\nrewritten by hand\n",
        )
        .unwrap();

        let source = store.notice_outside_change(&id).unwrap();
        let note = parse_note(&source);
        assert_eq!(note.front.get(KEY_ID), Some(id.as_str()));
        assert_eq!(note.front.get(KEY_SESSION), Some("s_note"));
        assert_eq!(note.front.get(KEY_PROJECT), Some("/work/p"));
        assert!(note.to_markdown().contains("rewritten by hand"));
        assert_eq!(
            fs::read_to_string(store.path_for(&id).unwrap()).unwrap(),
            source,
            "repaired on disk"
        );

        let versions = store.history().list(&id).unwrap();
        assert_eq!(versions[0].author, Author::File);
        assert!(
            versions.iter().any(|v| store
                .history()
                .read(&id, v.id)
                .unwrap()
                .contains("typed just now")),
            "the text before the edit is kept"
        );
        // Noticing again changes nothing.
        let count = versions.len();
        store.notice_outside_change(&id).unwrap();
        assert_eq!(store.history().list(&id).unwrap().len(), count);
    }
}
