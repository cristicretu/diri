//! Guarded access to the Engine's shared JSON state file.
//!
//! Atomic rename prevents torn documents; the adjacent advisory lock prevents
//! interleaved writes when compatible processes update disjoint owned keys at
//! the same time. Callers update only the keys they own, so fields written by
//! a newer Engine or another frontend survive unchanged.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Map, Value};

pub(crate) struct JsonStateFile {
    path: PathBuf,
}

impl JsonStateFile {
    pub(crate) fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Reads one complete document. A missing file is a fresh install; a file
    /// that exists but cannot be understood is an error and must not be
    /// replaced with an empty state.
    pub(crate) fn read(&self) -> io::Result<Option<Map<String, Value>>> {
        read_object(&self.path)
    }

    /// Verifies that a destructive lifecycle operation can safely begin. This
    /// catches an unreadable/corrupt document before a process is terminated;
    /// the subsequent update still repeats the check under the same lock rules.
    pub(crate) fn verify_editable(&self) -> io::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let _lock = FileLock::exclusive(&self.path)?;
        let _ = read_object(&self.path)?;
        Ok(())
    }

    /// Locks, reloads, mutates, and atomically replaces the document. Reloading
    /// after acquiring the lock is what preserves another writer's completed
    /// update instead of applying the mutation to a stale startup snapshot.
    pub(crate) fn update(
        &self,
        mutate: impl FnOnce(&mut Map<String, Value>) -> io::Result<()>,
    ) -> io::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let _lock = FileLock::exclusive(&self.path)?;
        let mut document = read_object(&self.path)?.unwrap_or_default();
        mutate(&mut document)?;
        write_object(&self.path, &document)
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
    value.as_object().cloned().map(Some).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "state file root is not a JSON object",
        )
    })
}

fn write_object(path: &Path, document: &Map<String, Value>) -> io::Result<()> {
    static NEXT_TEMPORARY: AtomicU64 = AtomicU64::new(0);

    let body = serde_json::to_vec(&Value::Object(document.clone()))?;
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
    drop(file);
    if let Err(error) = std::fs::rename(&temporary, path) {
        let _ = std::fs::remove_file(temporary);
        return Err(error);
    }
    Ok(())
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
        Ok(Self(file))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Barrier};

    use super::*;

    #[test]
    fn update_refuses_to_clobber_an_unparseable_document() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("state.json");
        std::fs::write(&path, b"{ not json").expect("broken fixture");
        let state = JsonStateFile::new(&path);

        let error = state
            .update(|document| {
                document.insert("sessions".into(), Value::Array(Vec::new()));
                Ok(())
            })
            .expect_err("broken state must be preserved");

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(
            std::fs::read(path).expect("original remains"),
            b"{ not json"
        );
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
            .update(|document| {
                document.insert("sessions".into(), serde_json::json!([{"id":"new"}]));
                Ok(())
            })
            .expect("update");

        let written = state.read().expect("read").expect("document");
        assert_eq!(written["future"], serde_json::json!({"theme":"plum"}));
        assert_eq!(written["sessions"][0]["id"], "new");
    }

    #[test]
    fn concurrent_disjoint_updates_do_not_lose_each_other() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let state = Arc::new(JsonStateFile::new(directory.path().join("state.json")));
        let barrier = Arc::new(Barrier::new(3));
        let mut writers = Vec::new();
        for (key, value) in [("desktop", 1), ("cli", 2)] {
            let state = Arc::clone(&state);
            let barrier = Arc::clone(&barrier);
            writers.push(std::thread::spawn(move || {
                barrier.wait();
                state
                    .update(|document| {
                        document.insert(key.into(), Value::from(value));
                        Ok(())
                    })
                    .expect("concurrent update");
            }));
        }
        barrier.wait();
        for writer in writers {
            writer.join().expect("writer");
        }

        let written = state.read().expect("read").expect("document");
        assert_eq!(written["desktop"], 1);
        assert_eq!(written["cli"], 2);
    }
}
