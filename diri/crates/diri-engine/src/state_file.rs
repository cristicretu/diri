//! Guarded access to the Engine's shared JSON state file.
//!
//! Atomic rename prevents torn documents; the adjacent advisory lock prevents
//! interleaved writes when compatible processes update disjoint owned keys at
//! the same time. Callers update only the keys they own, so fields written by
//! a newer Engine or another frontend survive unchanged.
//!
//! The document is held as top-level sections of raw JSON text: a section the
//! caller does not own is carried through byte for byte, never parsed into a
//! value tree and re-serialized. The last image this process read or wrote is
//! cached together with an open handle to its file. While the path still names
//! that exact file (same device, inode, length and modification time) the
//! image *is* the file, so an update under the lock neither re-reads nor
//! re-parses it; any other writer replaces the inode by rename, which misses
//! the cache and reloads exactly as before. Holding the handle keeps the
//! cached inode allocated, so its number cannot be recycled by a newer file
//! while the image is trusted. An update that leaves every section
//! byte-identical does not rewrite or fsync the file.

use std::collections::{BTreeMap, HashMap};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use serde::Serialize;
use serde_json::value::RawValue;
use serde_json::{Map, Value};

type Sections = BTreeMap<String, Box<RawValue>>;

/// One shared handle per state file. Clones share the cached image, so the
/// Registry and the workspace store do not re-parse each other's writes.
#[derive(Clone)]
pub(crate) struct JsonStateFile {
    shared: Arc<Shared>,
}

struct Shared {
    path: PathBuf,
    /// Held only to swap an `Arc`: file I/O happens outside this mutex, so a
    /// lock-free reader never waits behind a writer's fsync.
    image: Mutex<Option<Arc<Image>>>,
    /// Highest snapshot sequence committed per owner. Read and advanced only
    /// while holding the file lock; see [`JsonStateFile::commit_sections`].
    committed: Mutex<HashMap<&'static str, u64>>,
}

struct Image {
    /// Keeps the inode alive, and therefore its number unique, while cached.
    _file: File,
    stamp: Stamp,
    sections: Sections,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Stamp {
    dev: u64,
    ino: u64,
    len: u64,
    mtime: i64,
    mtime_nsec: i64,
}

impl Stamp {
    #[cfg(unix)]
    fn of(metadata: &std::fs::Metadata) -> Option<Self> {
        use std::os::unix::fs::MetadataExt;
        Some(Self {
            dev: metadata.dev(),
            ino: metadata.ino(),
            len: metadata.len(),
            mtime: metadata.mtime(),
            mtime_nsec: metadata.mtime_nsec(),
        })
    }

    /// Without inode identity nothing is cached: every access re-reads.
    #[cfg(not(unix))]
    fn of(_: &std::fs::Metadata) -> Option<Self> {
        None
    }
}

/// The mutable view an update closure receives: one entry per top-level key.
pub(crate) struct Document {
    sections: Sections,
    changed: bool,
}

impl Document {
    pub(crate) fn get(&self, key: &str) -> Option<&RawValue> {
        self.sections.get(key).map(|value| &**value)
    }

    /// Replaces one owned section. Identical bytes are not a change.
    pub(crate) fn insert_raw(&mut self, key: &str, value: Box<RawValue>) {
        if self
            .sections
            .get(key)
            .is_some_and(|current| current.get() == value.get())
        {
            return;
        }
        self.sections.insert(key.to_owned(), value);
        self.changed = true;
    }

    pub(crate) fn insert<T: Serialize + ?Sized>(&mut self, key: &str, value: &T) -> io::Result<()> {
        let raw = serde_json::value::to_raw_value(value).map_err(io::Error::other)?;
        self.insert_raw(key, raw);
        Ok(())
    }
}

impl JsonStateFile {
    pub(crate) fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            shared: Arc::new(Shared {
                path: path.into(),
                image: Mutex::new(None),
                committed: Mutex::new(HashMap::new()),
            }),
        }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.shared.path
    }

    /// Reads one complete document. A missing file is a fresh install; a file
    /// that exists but cannot be understood is an error and must not be
    /// replaced with an empty state.
    pub(crate) fn read(&self) -> io::Result<Option<Map<String, Value>>> {
        read_object(self.path())
    }

    /// One top-level section of the current file, served from the cached
    /// image while the path still names the cached file. Takes no file lock,
    /// like [`read`](Self::read): rename-based writers are never seen torn.
    pub(crate) fn read_section(&self, key: &str) -> io::Result<Option<Box<RawValue>>> {
        Ok(self
            .current()?
            .and_then(|image| image.sections.get(key).cloned()))
    }

    /// Verifies that a destructive lifecycle operation can safely begin. This
    /// catches an unreadable/corrupt document before a process is terminated;
    /// the subsequent update still repeats the check under the same lock rules.
    pub(crate) fn verify_editable(&self) -> io::Result<()> {
        self.create_parent()?;
        let _lock = FileLock::exclusive(self.path())?;
        self.current()?;
        Ok(())
    }

    /// Locks, reloads, mutates, and atomically replaces the document. Reloading
    /// after acquiring the lock is what preserves another writer's completed
    /// update instead of applying the mutation to a stale startup snapshot;
    /// the reload costs one `stat` while the cached image is still the file.
    /// The Registry commits through [`commit_sections`](Self::commit_sections).
    #[cfg(test)]
    pub(crate) fn update(
        &self,
        mutate: impl FnOnce(&mut Document) -> io::Result<()>,
    ) -> io::Result<()> {
        self.update_inner(mutate, false)
    }

    /// Also sync the renamed directory entry before acknowledging the edit.
    /// A sync error after rename is indeterminate: callers must reload state.
    pub(crate) fn update_durable(
        &self,
        mutate: impl FnOnce(&mut Document) -> io::Result<()>,
    ) -> io::Result<()> {
        self.update_inner(mutate, true)
    }

    /// Writes an owner's prepared sections unless a later snapshot from the
    /// same owner already landed. The owner assigns `sequence` in the order
    /// its state changed, so it can serialize under its own lock and write
    /// after releasing it without an older snapshot ever replacing a newer one.
    pub(crate) fn commit_sections(
        &self,
        owner: &'static str,
        sequence: u64,
        sections: Vec<(&'static str, Box<RawValue>)>,
    ) -> io::Result<()> {
        self.create_parent()?;
        let _lock = FileLock::exclusive(self.path())?;
        if self
            .lock_committed()?
            .get(owner)
            .is_some_and(|committed| *committed >= sequence)
        {
            return Ok(());
        }
        self.update_locked(
            |document| {
                for (key, value) in sections {
                    document.insert_raw(key, value);
                }
                Ok(())
            },
            false,
        )?;
        self.lock_committed()?.insert(owner, sequence);
        Ok(())
    }

    fn update_inner(
        &self,
        mutate: impl FnOnce(&mut Document) -> io::Result<()>,
        sync_directory: bool,
    ) -> io::Result<()> {
        self.create_parent()?;
        let _lock = FileLock::exclusive(self.path())?;
        self.update_locked(mutate, sync_directory)
    }

    /// Requires the file lock.
    fn update_locked(
        &self,
        mutate: impl FnOnce(&mut Document) -> io::Result<()>,
        sync_directory: bool,
    ) -> io::Result<()> {
        let current = self.current()?;
        let mut document = Document {
            sections: current
                .as_ref()
                .map(|image| image.sections.clone())
                .unwrap_or_default(),
            // A missing file is always written, as before: the first update
            // of a fresh install creates it.
            changed: current.is_none(),
        };
        drop(current);
        mutate(&mut document)?;
        if !document.changed {
            // Every section already has these bytes on disk. A durable caller
            // still gets the directory entry synced before it acknowledges.
            if sync_directory {
                sync_parent(self.path())?;
            }
            return Ok(());
        }
        let written = write_sections(self.path(), &document.sections, sync_directory);
        let mut slot = self.lock_image()?;
        *slot = None;
        let (file, stamp) = written?;
        if let Some(stamp) = stamp {
            *slot = Some(Arc::new(Image {
                _file: file,
                stamp,
                sections: document.sections,
            }));
        }
        Ok(())
    }

    /// The image of the file the path names now, loading it on a miss.
    fn current(&self) -> io::Result<Option<Arc<Image>>> {
        let path = self.path();
        let stamp = match std::fs::metadata(path) {
            Ok(metadata) => Stamp::of(&metadata),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                *self.lock_image()? = None;
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        if let Some(image) = self.lock_image()?.as_ref()
            && stamp == Some(image.stamp)
        {
            return Ok(Some(Arc::clone(image)));
        }
        let mut file = match File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                *self.lock_image()? = None;
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        // Identify the file the bytes come from, not whatever the path names
        // by the time the read finishes.
        let stamp = Stamp::of(&file.metadata()?);
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        let sections: Sections = serde_json::from_slice(&bytes)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let image = Arc::new(Image {
            _file: file,
            stamp: stamp.unwrap_or(Stamp {
                dev: 0,
                ino: 0,
                len: 0,
                mtime: 0,
                mtime_nsec: 0,
            }),
            sections,
        });
        if stamp.is_some() {
            *self.lock_image()? = Some(Arc::clone(&image));
        }
        Ok(Some(image))
    }

    fn lock_image(&self) -> io::Result<MutexGuard<'_, Option<Arc<Image>>>> {
        self.shared
            .image
            .lock()
            .map_err(|_| io::Error::other("state file cache poisoned"))
    }

    fn lock_committed(&self) -> io::Result<MutexGuard<'_, HashMap<&'static str, u64>>> {
        self.shared
            .committed
            .lock()
            .map_err(|_| io::Error::other("state file cache poisoned"))
    }

    fn create_parent(&self) -> io::Result<()> {
        if let Some(parent) = self.path().parent() {
            std::fs::create_dir_all(parent)?;
        }
        Ok(())
    }
}

fn read_object(path: &Path) -> io::Result<Option<Map<String, Value>>> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    match value {
        Value::Object(object) => Ok(Some(object)),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "state file root is not a JSON object",
        )),
    }
}

/// The same compact object `serde_json` writes for a `Map`: keys in sorted
/// order, each section's text verbatim.
fn encode_sections(sections: &Sections) -> io::Result<Vec<u8>> {
    let capacity = sections
        .iter()
        .map(|(key, value)| key.len() + value.get().len() + 4)
        .sum::<usize>()
        + 2;
    let mut body = Vec::with_capacity(capacity);
    body.push(b'{');
    for (index, (key, value)) in sections.iter().enumerate() {
        if index > 0 {
            body.push(b',');
        }
        serde_json::to_writer(&mut body, key)?;
        body.push(b':');
        body.extend_from_slice(value.get().as_bytes());
    }
    body.push(b'}');
    Ok(body)
}

/// Writes, fsyncs, and renames a complete document into place, returning the
/// still-open new file and its identity for the cache.
fn write_sections(
    path: &Path,
    sections: &Sections,
    sync_directory: bool,
) -> io::Result<(File, Option<Stamp>)> {
    static NEXT_TEMPORARY: AtomicU64 = AtomicU64::new(0);

    let body = encode_sections(sections)?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("state.json");
    let temporary = path.with_file_name(format!(
        ".{file_name}.tmp-{}-{}",
        std::process::id(),
        NEXT_TEMPORARY.fetch_add(1, Ordering::Relaxed)
    ));
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary)?;
    if let Err(error) = file.write_all(&body).and_then(|()| file.sync_all()) {
        drop(file);
        let _ = std::fs::remove_file(&temporary);
        return Err(error);
    }
    if let Err(error) = std::fs::rename(&temporary, path) {
        drop(file);
        let _ = std::fs::remove_file(temporary);
        return Err(error);
    }
    // Persist the rename itself before acknowledging an organization edit.
    // A file fsync alone does not guarantee its directory entry after a crash.
    if sync_directory {
        sync_parent(path)?;
    }
    let stamp = Stamp::of(&file.metadata()?);
    Ok((file, stamp))
}

fn sync_parent(path: &Path) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    #[cfg(unix)]
    {
        File::open(parent)?.sync_all()
    }
    // Windows has no unprivileged directory fsync: FlushFileBuffers on a
    // directory handle opened for reading fails with access denied. The file
    // was flushed before the rename; no stronger guarantee is claimed.
    #[cfg(windows)]
    {
        let _ = parent;
        Ok(())
    }
}

struct FileLock(#[allow(dead_code)] File);

impl FileLock {
    fn exclusive(target: &Path) -> io::Result<Self> {
        let lock_path = target.with_extension("lock");
        let mut options = OpenOptions::new();
        options.create(true).truncate(false).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(lock_path)?;
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        // LockFileEx; released when the handle closes, like flock.
        #[cfg(windows)]
        file.lock()?;
        Ok(Self(file))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Barrier};

    use super::*;

    fn raw(text: &str) -> Box<RawValue> {
        RawValue::from_string(text.to_owned()).unwrap()
    }

    #[cfg(unix)]
    fn identity(path: &Path) -> (u64, i64, i64) {
        use std::os::unix::fs::MetadataExt;
        let metadata = std::fs::metadata(path).unwrap();
        (metadata.ino(), metadata.mtime(), metadata.mtime_nsec())
    }

    #[test]
    fn durable_updates_sync_successfully_and_measure_directory_sync_cost() {
        let directory = tempfile::tempdir().unwrap();
        let file = JsonStateFile::new(directory.path().join("state.json"));
        let mut samples = [Vec::new(), Vec::new()];
        for i in 0..32 {
            for durable in [false, true] {
                let start = std::time::Instant::now();
                file.update_inner(
                    |document| document.insert(if durable { "workspace" } else { "sessions" }, &i),
                    durable,
                )
                .unwrap();
                samples[usize::from(durable)].push(start.elapsed());
            }
        }
        for sample in &mut samples {
            sample.sort();
        }
        eprintln!(
            "state writes: file-sync median {:?}, p95 {:?}; file+directory-sync median {:?}, p95 {:?}",
            samples[0][16], samples[0][30], samples[1][16], samples[1][30]
        );
        let document = file.read().unwrap().unwrap();
        assert_eq!(document["workspace"], 31);
        assert_eq!(document["sessions"], 31);
    }

    #[test]
    fn update_refuses_to_clobber_an_unparseable_document() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("state.json");
        std::fs::write(&path, b"{ not json").expect("broken fixture");
        let state = JsonStateFile::new(&path);

        let error = state
            .update(|document| document.insert("sessions", &[0u8; 0]))
            .expect_err("broken state must be preserved");

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(
            std::fs::read(path).expect("original remains"),
            b"{ not json"
        );
    }

    #[test]
    fn a_non_object_root_is_invalid_data() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.json");
        std::fs::write(&path, b"[1,2]").unwrap();
        let state = JsonStateFile::new(&path);
        let error = state
            .update(|document| document.insert("sessions", &1))
            .expect_err("array root must be preserved");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(std::fs::read(path).unwrap(), b"[1,2]");
    }

    #[test]
    fn a_cached_image_never_hides_a_later_corrupt_replacement() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.json");
        let state = JsonStateFile::new(&path);
        state
            .update(|document| document.insert("sessions", &1))
            .unwrap();
        // Another process replaces the file with something unreadable.
        std::fs::write(directory.path().join("other"), b"{ torn").unwrap();
        std::fs::rename(directory.path().join("other"), &path).unwrap();

        let error = state
            .update(|document| document.insert("sessions", &2))
            .expect_err("the cache must not paper over a corrupt file");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(std::fs::read(&path).unwrap(), b"{ torn");
        assert!(state.verify_editable().is_err());
    }

    #[test]
    fn update_preserves_keys_the_caller_does_not_own() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("state.json");
        std::fs::write(
            &path,
            br#"{"version":1,"sessions":[],"future":{"theme":"plum"}}"#,
        )
        .expect("fixture");
        let state = JsonStateFile::new(&path);

        state
            .update(|document| document.insert("sessions", &serde_json::json!([{"id":"new"}])))
            .expect("update");

        let written = state.read().expect("read").expect("document");
        assert_eq!(written["future"], serde_json::json!({"theme":"plum"}));
        assert_eq!(written["sessions"][0]["id"], "new");
    }

    #[cfg(unix)]
    #[test]
    fn an_unchanged_update_does_not_rewrite_the_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.json");
        let state = JsonStateFile::new(&path);
        state
            .update(|document| document.insert("sessions", &serde_json::json!([{"id":"a"}])))
            .unwrap();
        let before = identity(&path);
        std::thread::sleep(std::time::Duration::from_millis(5));

        for durable in [false, true] {
            state
                .update_inner(
                    |document| document.insert("sessions", &serde_json::json!([{"id":"a"}])),
                    durable,
                )
                .unwrap();
        }
        assert_eq!(
            identity(&path),
            before,
            "identical bytes must not be rewritten"
        );

        state
            .update(|document| document.insert("sessions", &serde_json::json!([{"id":"b"}])))
            .unwrap();
        assert_ne!(identity(&path), before, "a real change is written");
    }

    #[test]
    fn an_external_writers_section_is_not_clobbered_by_a_cached_image() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.json");
        let registry = JsonStateFile::new(&path);
        registry
            .update(|document| document.insert("sessions", &1))
            .unwrap();

        // Another process (its own handle and cache) writes a key it owns.
        let other = JsonStateFile::new(&path);
        other
            .update(|document| {
                document.insert("workspaceState", &serde_json::json!({"revision": 9}))
            })
            .unwrap();
        // An in-place writer that keeps the inode is caught by length/mtime.
        let text = std::fs::read_to_string(&path).unwrap();
        let text = format!("{},\"future\":\"plum\"}}", &text[..text.len() - 1]);
        std::fs::write(&path, text).unwrap();

        registry
            .update(|document| document.insert("sessions", &2))
            .unwrap();
        let written = registry.read().unwrap().unwrap();
        assert_eq!(written["sessions"], 2);
        assert_eq!(written["workspaceState"]["revision"], 9);
        assert_eq!(written["future"], "plum");
        assert_eq!(
            registry
                .read_section("workspaceState")
                .unwrap()
                .unwrap()
                .get(),
            r#"{"revision":9}"#
        );
    }

    #[test]
    fn clones_share_one_image_and_see_each_others_writes() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.json");
        let registry = JsonStateFile::new(&path);
        let workspace = registry.clone();
        registry
            .update(|document| document.insert("sessions", &1))
            .unwrap();
        workspace
            .update_durable(|document| {
                assert_eq!(document.get("sessions").unwrap().get(), "1");
                document.insert("workspaceState", &2)
            })
            .unwrap();
        assert_eq!(
            registry
                .read_section("workspaceState")
                .unwrap()
                .unwrap()
                .get(),
            "2"
        );
    }

    #[test]
    fn an_older_owner_snapshot_never_replaces_a_newer_one() {
        let directory = tempfile::tempdir().unwrap();
        let state = JsonStateFile::new(directory.path().join("state.json"));
        state
            .commit_sections("registry", 2, vec![("sessions", raw("\"newer\""))])
            .unwrap();
        state
            .commit_sections("registry", 1, vec![("sessions", raw("\"older\""))])
            .unwrap();
        assert_eq!(state.read().unwrap().unwrap()["sessions"], "newer");
        state
            .commit_sections("registry", 3, vec![("sessions", raw("\"latest\""))])
            .unwrap();
        assert_eq!(state.read().unwrap().unwrap()["sessions"], "latest");
    }

    #[test]
    fn concurrent_disjoint_updates_do_not_lose_each_other() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("state.json");
        // Separate handles model separate processes; they share only the lock.
        let barrier = Arc::new(Barrier::new(3));
        let mut writers = Vec::new();
        for (key, value) in [("desktop", 1), ("cli", 2)] {
            let state = JsonStateFile::new(&path);
            let barrier = Arc::clone(&barrier);
            writers.push(std::thread::spawn(move || {
                barrier.wait();
                state
                    .update(|document| document.insert(key, &value))
                    .expect("concurrent update");
            }));
        }
        barrier.wait();
        for writer in writers {
            writer.join().expect("writer");
        }

        let written = JsonStateFile::new(&path)
            .read()
            .expect("read")
            .expect("document");
        assert_eq!(written["desktop"], 1);
        assert_eq!(written["cli"], 2);
    }
}
