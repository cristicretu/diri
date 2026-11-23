//! Live Surfaces on disk.
//!
//! One JSON file per surface in a flat, owner-only directory beside the
//! notes (`<app support>/surfaces`). Every write takes the directory lock,
//! goes through [`SurfaceDoc::validate`], and lands atomically (temp file,
//! fsync, rename), so the app, the CLI and any number of agents can write
//! concurrently and the app never reads half a surface. Tree changes are kept
//! as numbered versions (`.history/<id>/<revision>.json`, newest
//! [`MAX_VERSIONS`]); what the person does on a surface is appended to its
//! event log (`.events/<id>.jsonl`, newest [`MAX_EVENTS`]) for its agent.
//!
//! The format itself (components, bounds, patches) is `diri_proto::surface`.

use std::fs;
use std::io::{self, BufRead, Write as _};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use diri_proto::surface::patch::{self, Applied, Patch};
use diri_proto::surface::{
    EventKind, Interaction, Node, SCHEMA_VERSION, Scope, SurfaceDoc, SurfaceError, SurfaceEvent,
    identity, limits,
};
use serde::Serialize;

pub use diri_proto::surface;

const LOCK_FILE: &str = ".lock";
const HISTORY_DIR: &str = ".history";
const EVENTS_DIR: &str = ".events";
const TRASH_DIR: &str = ".trash";
const MANIFEST: &str = "manifest.json";
const DATA_FILE: &str = "data.json";
const APPROVALS_FILE: &str = ".approvals.json";
/// Tree versions kept per surface.
pub const MAX_VERSIONS: usize = 50;
/// Events kept per surface; the log is compacted to half when it overflows.
pub const MAX_EVENTS: usize = 400;

/// Who wrote a revision, as the history and the header show it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Author {
    /// The person, in the app.
    User,
    /// An agent session.
    Agent(String),
    /// A scheduled run or a script with no session.
    Tool,
}

impl Author {
    pub fn describe(&self) -> String {
        match self {
            Self::User => "you".into(),
            Self::Agent(session) => format!("agent {session}"),
            Self::Tool => "dirijor".into(),
        }
    }

    /// The caller of a CLI/MCP process: its Diri session, if any.
    pub fn from_env() -> Self {
        std::env::var(diri_proto::paths::ENV_SESSION_ID)
            .ok()
            .filter(|id| !id.trim().is_empty())
            .map_or(Self::Tool, Self::Agent)
    }

    pub fn session(&self) -> Option<&str> {
        match self {
            Self::Agent(session) => Some(session),
            _ => None,
        }
    }
}

/// A failure an agent can act on: a structured surface refusal, or I/O.
#[derive(Debug)]
pub enum StoreError {
    Surface(SurfaceError),
    NotFound(String),
    Io(io::Error),
}

impl StoreError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Surface(error) => error.code,
            Self::NotFound(_) => "not_found",
            Self::Io(_) => "io",
        }
    }
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Surface(error) => error.fmt(f),
            Self::NotFound(id) => write!(f, "not_found: no surface {id:?}"),
            Self::Io(error) => write!(f, "io: {error}"),
        }
    }
}

impl std::error::Error for StoreError {}

impl From<io::Error> for StoreError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<SurfaceError> for StoreError {
    fn from(error: SurfaceError) -> Self {
        Self::Surface(error)
    }
}

pub type Result<T, E = StoreError> = std::result::Result<T, E>;

/// One file of an app's folder.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct AppFile {
    pub path: String,
    pub bytes: u64,
}

/// What lists and the sidebar show without drawing the surface.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SurfaceMeta {
    pub id: String,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subtitle: Option<String>,
    pub scope: Scope,
    pub pinned: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon: Option<surface::Icon>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accent: Option<String>,
    /// A native Lua app (`app.lua`).
    pub app: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    pub revision: u64,
    pub updated_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_by: Option<String>,
    /// `(done, total)` checklist items.
    pub checklist: (usize, usize),
    /// Written by a newer Diri: shown, never rewritten.
    pub read_only: bool,
}

impl SurfaceMeta {
    fn from_doc(doc: &SurfaceDoc, read_only: bool) -> Self {
        Self {
            id: doc.id.clone(),
            title: doc.title.clone(),
            subtitle: doc.subtitle.clone(),
            scope: doc.scope.clone(),
            pinned: doc.pinned,
            icon: doc.icon.clone(),
            accent: doc.accent.clone(),
            app: doc.app.is_some(),
            owner: doc.owner.clone(),
            revision: doc.revision,
            updated_ms: doc.updated_ms,
            updated_by: doc.updated_by.clone(),
            checklist: doc.checklist_progress(),
            read_only,
        }
    }
}

/// A surface as read from disk.
#[derive(Clone, Debug)]
pub struct Loaded {
    pub doc: SurfaceDoc,
    /// The file's schema is newer than this build: draw what parses, never
    /// write it.
    pub read_only: bool,
}

/// One kept version of a surface's tree.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Version {
    pub revision: u64,
    pub updated_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_by: Option<String>,
}

pub struct SurfaceStore {
    dir: PathBuf,
}

impl SurfaceStore {
    /// `<app support>/surfaces`.
    pub fn default_dir(home: impl AsRef<Path>) -> PathBuf {
        diri_proto::paths::DirijorPaths::app_support(home).join("surfaces")
    }

    /// `DIRI_SURFACES_DIR` when set (the Engine exports it to agents, tests
    /// set it), else beside the rest of Diri's state, honouring
    /// `DIRIJOR_APP_SUPPORT`.
    pub fn resolve_dir() -> Option<PathBuf> {
        if let Some(dir) =
            std::env::var_os(diri_proto::paths::ENV_SURFACES_DIR).filter(|d| !d.is_empty())
        {
            return Some(PathBuf::from(dir));
        }
        if let Some(support) =
            std::env::var_os(diri_proto::paths::ENV_APP_SUPPORT).filter(|d| !d.is_empty())
        {
            return Some(PathBuf::from(support).join("surfaces"));
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

    pub fn path_for(&self, id: &str) -> Result<PathBuf> {
        if !limits::is_valid_ref(id) {
            return Err(SurfaceError::new(
                "invalid_id",
                format!("invalid surface id {id:?}"),
                "id",
            )
            .into());
        }
        Ok(self.folder(id).join(MANIFEST))
    }

    /// `<dir>/<id>/`: the manifest, `app.lua`, `data.json`, assets.
    fn folder(&self, id: &str) -> PathBuf {
        self.dir.join(id)
    }

    /// The surface folder, for reading its files (`app.lua`, assets).
    pub fn folder_for(&self, id: &str) -> Result<PathBuf> {
        self.path_for(id)?;
        Ok(self.folder(id))
    }

    /// Every readable surface, most recently updated first.
    pub fn list(&self) -> io::Result<Vec<SurfaceMeta>> {
        let mut out = Vec::new();
        for entry in fs::read_dir(&self.dir)? {
            let Ok(entry) = entry else { continue };
            let Some(id) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if id.starts_with('.')
                || !limits::is_valid_ref(&id)
                || !entry.path().join(MANIFEST).is_file()
            {
                continue;
            }
            if let Ok(loaded) = self.load(&id) {
                out.push(SurfaceMeta::from_doc(&loaded.doc, loaded.read_only));
            }
        }
        out.sort_by(|a, b| {
            b.updated_ms
                .cmp(&a.updated_ms)
                .then_with(|| a.id.cmp(&b.id))
        });
        Ok(out)
    }

    pub fn load(&self, id: &str) -> Result<Loaded> {
        let path = self.path_for(id)?;
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(StoreError::NotFound(id.to_owned()));
            }
            Err(error) => return Err(error.into()),
        };
        if bytes.len() > limits::MAX_FILE_BYTES {
            return Err(
                SurfaceError::new("too_large", "surface file is over the size limit", "").into(),
            );
        }
        let doc: SurfaceDoc = serde_json::from_slice(&bytes).map_err(|error| {
            SurfaceError::new("corrupt", format!("cannot read surface {id}: {error}"), "")
        })?;
        if doc.id != id {
            return Err(
                SurfaceError::new("corrupt", "surface id does not match its file", "id").into(),
            );
        }
        let read_only = doc.schema > SCHEMA_VERSION;
        if !read_only {
            // A file edited behind the store's back is drawn only if it is
            // still within bounds.
            doc.validate()?;
        }
        Ok(Loaded { doc, read_only })
    }

    /// Finds a surface by exact id, else by a unique case-insensitive title
    /// match (exact title first, then substring).
    pub fn resolve(&self, query: &str) -> Result<String> {
        if limits::is_valid_ref(query) && self.path_for(query)?.exists() {
            return Ok(query.to_owned());
        }
        let needle = query.to_lowercase();
        let all = self.list()?;
        let exact: Vec<_> = all
            .iter()
            .filter(|m| m.title.to_lowercase() == needle)
            .collect();
        if let [one] = exact.as_slice() {
            return Ok(one.id.clone());
        }
        let partial: Vec<_> = all
            .iter()
            .filter(|m| m.title.to_lowercase().contains(&needle))
            .collect();
        match partial.as_slice() {
            [one] => Ok(one.id.clone()),
            [] => Err(StoreError::NotFound(query.to_owned())),
            many => Err(SurfaceError::new(
                "ambiguous",
                format!(
                    "{query:?} matches {} surfaces: {}",
                    many.len(),
                    many.iter()
                        .map(|m| m.id.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                "",
            )
            .into()),
        }
    }

    /// Stores a new surface. `doc.id` empty picks one from the title.
    pub fn create(&self, mut doc: SurfaceDoc, author: &Author) -> Result<SurfaceDoc> {
        let _lock = self.lock()?;
        if doc.id.is_empty() {
            doc.id = new_id(&doc.title);
        }
        let path = self.path_for(&doc.id)?;
        if path.exists() {
            return Err(SurfaceError::new(
                "exists",
                format!("surface {} already exists", doc.id),
                "id",
            )
            .into());
        }
        let now = now_ms();
        doc.schema = SCHEMA_VERSION;
        doc.revision = 1;
        doc.created_ms = now;
        doc.updated_ms = now;
        doc.updated_by = Some(author.describe());
        if doc.owner.is_none() {
            doc.owner = author.session().map(str::to_owned);
        }
        doc.state = doc.state.pruned_to(&doc.root);
        doc.validate()?;
        self.write(&doc)?;
        self.record_version(&doc);
        Ok(doc)
    }

    /// Applies a patch under the lock. A refused patch writes nothing.
    pub fn patch(&self, id: &str, patch: &Patch, author: &Author) -> Result<(SurfaceDoc, Applied)> {
        self.write_tree(id, author, |doc| patch::apply(doc, patch))
    }

    /// Replaces the whole tree, keeping the state of surviving components.
    pub fn replace(
        &self,
        id: &str,
        title: Option<&str>,
        root: Vec<Node>,
        expected_revision: Option<u64>,
        author: &Author,
    ) -> Result<(SurfaceDoc, Applied)> {
        self.write_tree(id, author, |doc| {
            patch::replace_all(doc, title, root.clone(), expected_revision)
        })
    }

    /// Metadata changes: pin, owner, agents, scope. Revision bumps.
    pub fn update_meta(
        &self,
        id: &str,
        author: &Author,
        change: impl FnOnce(&mut SurfaceDoc),
    ) -> Result<SurfaceDoc> {
        let (doc, _) = self.write_tree(id, author, |doc| {
            let mut next = doc.clone();
            change(&mut next);
            next.id = doc.id.clone();
            next.revision = doc.revision.saturating_add(1);
            let nodes = next.validate()?;
            let revision = next.revision;
            Ok((
                next,
                Applied {
                    ops: 0,
                    nodes,
                    revision,
                },
            ))
        })?;
        Ok(doc)
    }

    fn write_tree(
        &self,
        id: &str,
        author: &Author,
        change: impl FnOnce(&SurfaceDoc) -> std::result::Result<(SurfaceDoc, Applied), SurfaceError>,
    ) -> Result<(SurfaceDoc, Applied)> {
        let _lock = self.lock()?;
        let loaded = self.load(id)?;
        if loaded.read_only {
            return Err(read_only_error(&loaded.doc).into());
        }
        let (mut next, applied) = change(&loaded.doc)?;
        next.updated_ms = now_ms();
        next.updated_by = Some(author.describe());
        self.write(&next)?;
        self.record_version(&next);
        Ok((next, applied))
    }

    /// The person changed a tick, toggle or choice: keep it and log it.
    /// Returns the surface, the logged event, and whether its agent asked to
    /// hear about it. The revision does not move: state is not a tree change,
    /// so an agent's `expected_revision` is not invalidated by a tick.
    pub fn interact(
        &self,
        id: &str,
        interaction: &Interaction,
    ) -> Result<(SurfaceDoc, SurfaceEvent, bool)> {
        let _lock = self.lock()?;
        let loaded = self.load(id)?;
        if loaded.read_only {
            return Err(read_only_error(&loaded.doc).into());
        }
        let mut doc = loaded.doc;
        let notify = doc.interact(interaction)?;
        doc.validate()?;
        doc.updated_ms = now_ms();
        self.write(&doc)?;
        let event = self.append_event(
            id,
            EventKind::Interaction(interaction.clone()),
            notify.then(|| doc.owner.clone()).flatten(),
        )?;
        Ok((doc, event, notify))
    }

    /// Logs a button the person pressed, and who was told.
    pub fn record_action(
        &self,
        id: &str,
        what: EventKind,
        delivered_to: Option<String>,
    ) -> Result<SurfaceEvent> {
        let _lock = self.lock()?;
        self.path_for(id)?;
        self.append_event(id, what, delivered_to)
    }

    /// Events after `after` (a `seq`), oldest first, at most `limit`.
    pub fn events(&self, id: &str, after: u64, limit: usize) -> Result<Vec<SurfaceEvent>> {
        self.path_for(id)?;
        let mut events = self.read_events(id)?;
        events.retain(|event| event.seq > after);
        events.truncate(limit);
        Ok(events)
    }

    pub fn history(&self, id: &str) -> Result<Vec<Version>> {
        self.path_for(id)?;
        let dir = self.history_dir(id);
        let mut versions = Vec::new();
        let Ok(entries) = fs::read_dir(&dir) else {
            return Ok(versions);
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|ext| ext != "json") {
                continue;
            }
            if let Ok(doc) = fs::read(&path).map_err(StoreError::from).and_then(|bytes| {
                serde_json::from_slice::<SurfaceDoc>(&bytes).map_err(|e| {
                    StoreError::Surface(SurfaceError::new("corrupt", e.to_string(), ""))
                })
            }) {
                versions.push(Version {
                    revision: doc.revision,
                    updated_ms: doc.updated_ms,
                    updated_by: doc.updated_by,
                });
            }
        }
        versions.sort_by_key(|v| std::cmp::Reverse(v.revision));
        Ok(versions)
    }

    pub fn read_version(&self, id: &str, revision: u64) -> Result<SurfaceDoc> {
        self.path_for(id)?;
        let path = self.history_dir(id).join(format!("{revision}.json"));
        let bytes = fs::read(&path).map_err(|_| {
            StoreError::Surface(SurfaceError::new(
                "not_found",
                format!("no kept version {revision} of {id}"),
                "",
            ))
        })?;
        serde_json::from_slice(&bytes)
            .map_err(|e| SurfaceError::new("corrupt", e.to_string(), "").into())
    }

    /// Brings back an earlier tree as a new revision; the current one stays
    /// in history, so a restore can itself be undone.
    pub fn restore(&self, id: &str, revision: u64, author: &Author) -> Result<SurfaceDoc> {
        let old = self.read_version(id, revision)?;
        let (doc, _) = self.write_tree(id, author, |doc| {
            let mut next = doc.clone();
            next.title = old.title.clone();
            next.subtitle = old.subtitle.clone();
            next.root = old.root.clone();
            next.state = doc.state.pruned_to(&next.root);
            next.revision = doc.revision.saturating_add(1);
            let nodes = next.validate()?;
            let revision = next.revision;
            Ok((
                next,
                Applied {
                    ops: 1,
                    nodes,
                    revision,
                },
            ))
        })?;
        Ok(doc)
    }

    /// Saves a copy of a session surface (or any surface) as a saved,
    /// pinned surface. The original is left in place.
    pub fn promote(&self, id: &str, scope: Scope, author: &Author) -> Result<SurfaceDoc> {
        if !scope.is_saved() {
            return Err(SurfaceError::new(
                "invalid_value",
                "promote to a global or project scope",
                "scope",
            )
            .into());
        }
        let source = self.load(id)?;
        if source.read_only {
            return Err(read_only_error(&source.doc).into());
        }
        let mut saved = source.doc.promoted(String::new(), scope, now_ms());
        saved.id = String::new();
        let saved = self.create(saved, author)?;
        // The app comes along: its code, data and assets.
        for file in self.list_files(id)? {
            if file.path == DATA_FILE {
                self.write_data(&saved.id, &self.read_data(id)?)?;
            } else if let Ok(bytes) = fs::read(self.folder(id).join(&file.path)) {
                self.write_file(&saved.id, &file.path, &bytes)?;
            }
        }
        Ok(saved)
    }

    /// Moves a surface to `.trash/` (with its history and events), so a
    /// delete can be recovered by hand.
    pub fn delete(&self, id: &str) -> Result<PathBuf> {
        let _lock = self.lock()?;
        let path = self.path_for(id)?;
        if !path.exists() {
            return Err(StoreError::NotFound(id.to_owned()));
        }
        let trash = self.dir.join(TRASH_DIR);
        diri_platform::private::create_dir_all(&trash)?;
        let target = trash.join(format!("{id}.{}", now_ms()));
        fs::rename(self.folder(id), &target)?;
        let _ = self.set_approval(id, None);
        let _ = fs::remove_dir_all(self.history_dir(id));
        let _ = fs::remove_file(self.events_path(id));
        Ok(target)
    }

    // -- App files --------------------------------------------------------

    /// Writes one file of a surface's app (`app.lua`, an asset), atomically.
    /// Paths stay inside the folder, types and sizes are bounded, and the
    /// manifest and data have their own calls.
    pub fn write_file(&self, id: &str, path: &str, bytes: &[u8]) -> Result<()> {
        let _lock = self.lock()?;
        self.path_for(id)?;
        if !identity::is_valid_app_path(path) || path == DATA_FILE {
            return Err(SurfaceError::new(
                "invalid_path",
                format!(
                    "{path:?} is not an app file (lua, json, images, txt, md; at most 3 levels)"
                ),
                "path",
            )
            .into());
        }
        if bytes.len() > identity::MAX_APP_FILE_BYTES {
            return Err(
                SurfaceError::new("too_large", "an app file is at most 2 MiB", path).into(),
            );
        }
        if path.ends_with(".lua") && std::str::from_utf8(bytes).is_err() {
            return Err(
                SurfaceError::new("invalid_value", "Lua source must be UTF-8 text", path).into(),
            );
        }
        let files = self.list_files(id)?;
        let replaced = files.iter().find(|f| f.path == path).map_or(0, |f| f.bytes);
        let total: u64 = files.iter().map(|f| f.bytes).sum::<u64>() - replaced + bytes.len() as u64;
        if total as usize > identity::MAX_APP_BYTES
            || (replaced == 0 && files.len() >= identity::MAX_APP_FILES)
        {
            return Err(SurfaceError::new(
                "too_large",
                "an app holds at most 128 files and 16 MiB",
                path,
            )
            .into());
        }
        let target = self.folder(id).join(path);
        if let Some(parent) = target.parent() {
            diri_platform::private::create_dir_all(parent)?;
        }
        let tmp = self.folder(id).join(format!(".file.{}.tmp", nonce()));
        diri_platform::private::replace_via(&tmp, &target, bytes)?;
        Ok(())
    }

    pub fn read_file(&self, id: &str, path: &str) -> Result<Vec<u8>> {
        self.path_for(id)?;
        if !identity::is_valid_app_path(path) {
            return Err(SurfaceError::new(
                "invalid_path",
                format!("{path:?} is not an app file"),
                "path",
            )
            .into());
        }
        fs::read(self.folder(id).join(path)).map_err(|error| match error.kind() {
            io::ErrorKind::NotFound => StoreError::NotFound(format!("{id}/{path}")),
            _ => StoreError::Io(error),
        })
    }

    pub fn remove_file(&self, id: &str, path: &str) -> Result<()> {
        let _lock = self.lock()?;
        self.path_for(id)?;
        if !identity::is_valid_app_path(path) {
            return Err(SurfaceError::new(
                "invalid_path",
                format!("{path:?} is not an app file"),
                "path",
            )
            .into());
        }
        fs::remove_file(self.folder(id).join(path)).map_err(StoreError::Io)
    }

    /// The app's files (not the manifest), sorted.
    pub fn list_files(&self, id: &str) -> Result<Vec<AppFile>> {
        let root = self.folder_for(id)?;
        let mut out = Vec::new();
        fn walk(root: &Path, dir: &Path, depth: usize, out: &mut Vec<AppFile>) {
            let Ok(entries) = fs::read_dir(dir) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let Ok(meta) = entry.metadata() else { continue };
                let Ok(relative) = path.strip_prefix(root) else {
                    continue;
                };
                let Some(relative) = relative.to_str() else {
                    continue;
                };
                if meta.is_dir() && depth < 3 {
                    walk(root, &path, depth + 1, out);
                } else if meta.is_file() && identity::is_valid_app_path(relative) {
                    out.push(AppFile {
                        path: relative.to_owned(),
                        bytes: meta.len(),
                    });
                }
            }
        }
        walk(&root, &root, 1, &mut out);
        out.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(out)
    }

    /// `app.lua`, when the surface is a Lua app and has one.
    pub fn app_source(&self, id: &str) -> Result<Option<String>> {
        match self.read_file(id, identity::APP_ENTRY) {
            Ok(bytes) => Ok(Some(String::from_utf8_lossy(&bytes).into_owned())),
            Err(StoreError::NotFound(_)) => Ok(None),
            Err(error) => Err(error),
        }
    }

    // -- Data ------------------------------------------------------------

    /// `data.json`: what `sync` or an agent last wrote; `null` before.
    pub fn read_data(&self, id: &str) -> Result<serde_json::Value> {
        let path = self.folder_for(id)?.join(DATA_FILE);
        match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|e| SurfaceError::new("corrupt", format!("data.json: {e}"), "").into()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(serde_json::Value::Null),
            Err(error) => Err(error.into()),
        }
    }

    /// Replaces `data.json` atomically. Bounded; the manifest is untouched,
    /// so a sync never races an agent's edit of the tree.
    pub fn write_data(&self, id: &str, data: &serde_json::Value) -> Result<()> {
        let _lock = self.lock()?;
        let folder = self.folder_for(id)?;
        if !folder.join(MANIFEST).is_file() {
            return Err(StoreError::NotFound(id.to_owned()));
        }
        let bytes = serde_json::to_vec(data)
            .map_err(|e| SurfaceError::new("invalid_value", e.to_string(), ""))?;
        if bytes.len() > identity::MAX_DATA_BYTES {
            return Err(SurfaceError::new(
                "too_large",
                format!(
                    "data is {} bytes; the limit is {}",
                    bytes.len(),
                    identity::MAX_DATA_BYTES
                ),
                "data",
            )
            .into());
        }
        let tmp = folder.join(format!(".data.{}.tmp", nonce()));
        diri_platform::private::replace_via(&tmp, &folder.join(DATA_FILE), bytes)?;
        Ok(())
    }

    /// Keeps a Lua app's own state (what `on_event` returned). Like a
    /// tick, it is not a tree change: the revision does not move.
    pub fn set_app_state(&self, id: &str, state: &serde_json::Value) -> Result<()> {
        let _lock = self.lock()?;
        let loaded = self.load(id)?;
        if loaded.read_only {
            return Err(read_only_error(&loaded.doc).into());
        }
        let mut doc = loaded.doc;
        doc.state.app = Some(state.clone());
        doc.validate()?;
        self.write(&doc)
    }

    // -- Approvals -------------------------------------------------------

    /// Records that the person approved `ctx.exec`/`ctx.fetch` for the app
    /// whose `app.lua` has `digest`. Only the app UI calls this; agents
    /// have no tool for it. An edit changes the digest, which then no
    /// longer matches.
    pub fn approve(&self, id: &str, digest: &str) -> Result<()> {
        self.path_for(id)?;
        self.set_approval(id, Some(digest))
    }

    pub fn revoke(&self, id: &str) -> Result<()> {
        self.path_for(id)?;
        self.set_approval(id, None)
    }

    /// Whether the person approved exactly this source.
    pub fn is_approved(&self, id: &str, digest: &str) -> bool {
        self.approvals()
            .get(id)
            .is_some_and(|approved| approved == digest)
    }

    fn approvals(&self) -> std::collections::BTreeMap<String, String> {
        fs::read(self.dir.join(APPROVALS_FILE))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    fn set_approval(&self, id: &str, digest: Option<&str>) -> Result<()> {
        let mut approvals = self.approvals();
        match digest {
            Some(digest) => {
                approvals.insert(id.to_owned(), digest.to_owned());
            }
            None => {
                approvals.remove(id);
            }
        }
        let bytes = serde_json::to_vec_pretty(&approvals)
            .map_err(|e| SurfaceError::new("invalid_value", e.to_string(), ""))?;
        let tmp = self.dir.join(format!(".approvals.{}.tmp", nonce()));
        diri_platform::private::replace_via(&tmp, &self.dir.join(APPROVALS_FILE), bytes)?;
        Ok(())
    }

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

    fn write(&self, doc: &SurfaceDoc) -> Result<()> {
        let path = self.path_for(&doc.id)?;
        diri_platform::private::create_dir_all(&self.folder(&doc.id))?;
        let bytes = serde_json::to_vec_pretty(doc)
            .map_err(|e| SurfaceError::new("invalid_value", e.to_string(), ""))?;
        let tmp = self
            .folder(&doc.id)
            .join(format!(".manifest.{}.tmp", nonce()));
        diri_platform::private::replace_via(&tmp, &path, bytes)?;
        Ok(())
    }

    fn history_dir(&self, id: &str) -> PathBuf {
        self.dir.join(HISTORY_DIR).join(id)
    }

    fn events_path(&self, id: &str) -> PathBuf {
        self.dir.join(EVENTS_DIR).join(format!("{id}.jsonl"))
    }

    /// Best effort: history never blocks a write.
    fn record_version(&self, doc: &SurfaceDoc) {
        let dir = self.history_dir(&doc.id);
        if diri_platform::private::create_dir_all(&dir).is_err() {
            return;
        }
        let mut kept = doc.clone();
        kept.state = Default::default();
        if let Ok(bytes) = serde_json::to_vec(&kept) {
            let tmp = dir.join(format!(".{}.tmp", nonce()));
            let _ = diri_platform::private::replace_via(
                &tmp,
                &dir.join(format!("{}.json", doc.revision)),
                bytes,
            );
        }
        let Ok(entries) = fs::read_dir(&dir) else {
            return;
        };
        let mut revisions: Vec<u64> = entries
            .flatten()
            .filter_map(|e| e.path().file_stem()?.to_str()?.parse().ok())
            .collect();
        if revisions.len() > MAX_VERSIONS {
            revisions.sort_unstable();
            for revision in &revisions[..revisions.len() - MAX_VERSIONS] {
                let _ = fs::remove_file(dir.join(format!("{revision}.json")));
            }
        }
    }

    fn read_events(&self, id: &str) -> io::Result<Vec<SurfaceEvent>> {
        let file = match fs::File::open(self.events_path(id)) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        Ok(io::BufReader::new(file)
            .lines()
            .map_while(|line| line.ok())
            .filter_map(|line| serde_json::from_str(&line).ok())
            .collect())
    }

    fn append_event(
        &self,
        id: &str,
        what: EventKind,
        delivered_to: Option<String>,
    ) -> Result<SurfaceEvent> {
        let path = self.events_path(id);
        if let Some(parent) = path.parent() {
            diri_platform::private::create_dir_all(parent)?;
        }
        let mut events = self.read_events(id)?;
        let event = SurfaceEvent {
            seq: events.last().map_or(1, |last| last.seq + 1),
            at_ms: now_ms(),
            what,
            delivered_to,
        };
        let line = serde_json::to_string(&event)
            .map_err(|e| SurfaceError::new("invalid_value", e.to_string(), ""))?;
        if line.len() > limits::MAX_PAYLOAD_BYTES * 2 {
            return Err(SurfaceError::new("too_large", "event is too large", "").into());
        }
        if events.len() + 1 > MAX_EVENTS {
            events.drain(..events.len() + 1 - MAX_EVENTS / 2);
            events.push(event.clone());
            let mut text = String::new();
            for event in &events {
                if let Ok(line) = serde_json::to_string(event) {
                    text.push_str(&line);
                    text.push('\n');
                }
            }
            let tmp = path.with_extension(format!("{}.tmp", nonce()));
            diri_platform::private::replace_via(&tmp, &path, text)?;
        } else {
            let mut file = diri_platform::private::append(&path)?;
            file.write_all(format!("{line}\n").as_bytes())?;
        }
        Ok(event)
    }
}

fn read_only_error(doc: &SurfaceDoc) -> SurfaceError {
    SurfaceError::new(
        "unsupported_schema",
        format!(
            "surface {} was written by a newer Diri (schema {}); update Diri to change it",
            doc.id, doc.schema
        ),
        "schema",
    )
}

/// The surface a changed path belongs to: the folder under `dir` it sits
/// in. Temp files, history, events and approvals belong to none.
pub fn surface_id(dir: &Path, path: &Path) -> Option<String> {
    let relative = path.strip_prefix(dir).ok()?;
    let mut parts = relative.components();
    let id = parts.next()?.as_os_str().to_str()?;
    let file = path.file_name()?.to_str()?;
    let ok = limits::is_valid_ref(id) && !id.starts_with('.') && !file.starts_with('.');
    ok.then(|| id.to_owned())
}

/// `whats-for-today-3f2a`: a readable slug of the title plus a nonce.
pub fn new_id(title: &str) -> String {
    let mut slug = String::new();
    for c in title.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
        } else if !slug.ends_with('-') && !slug.is_empty() {
            slug.push('-');
        }
        if slug.len() >= 40 {
            break;
        }
    }
    let slug = slug.trim_end_matches('-');
    let nonce = &nonce()[..4];
    if slug.is_empty() {
        format!("surface-{nonce}")
    } else {
        format!("{slug}-{nonce}")
    }
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
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

#[cfg(test)]
mod tests;
