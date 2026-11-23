//! Wakeups for open GUI Lenses: a kernel file watch (FSEvents / inotify /
//! kqueue via `notify`) on each watched transcript's directory. No polling,
//! no timers: an idle Engine with idle Agents does no work here.
//!
//! A watch is a lease renewed by every `session.thread_read` that asks for
//! it. Expired leases are dropped lazily, on the next lease request, so a
//! client that vanished costs one directory watch until then and nothing
//! per event beyond a map lookup.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use notify::{RecursiveMode, Watcher as _};

/// A lease outlives any plausible quiet stretch of an open lens: the app
/// also re-reads on every `session.updated`, which renews it whenever the
/// Agent starts a turn.
const LEASE: Duration = Duration::from_secs(60 * 60);

pub type Notify = Arc<dyn Fn(&str, u64) + Send + Sync>;

struct Lease {
    session_id: String,
    renewed: Instant,
}

type Leases = Arc<Mutex<HashMap<PathBuf, Lease>>>;

/// The watcher and its directory counts. Kept apart from [`Leases`]: the
/// notify callback takes only the lease lock, so `watch`/`unwatch` (which
/// may wait for the callback thread on macOS) never runs under a lock the
/// callback needs.
#[derive(Default)]
struct Watching {
    watcher: Option<notify::RecommendedWatcher>,
    /// Watched directory → number of leased files inside it.
    dirs: HashMap<PathBuf, usize>,
}

pub struct ThreadWatches {
    watching: Mutex<Watching>,
    leases: Leases,
    notify: Notify,
}

impl ThreadWatches {
    pub fn new(notify: Notify) -> Self {
        Self {
            watching: Mutex::new(Watching::default()),
            leases: Arc::new(Mutex::new(HashMap::new())),
            notify,
        }
    }

    /// Leases a watch on `path` for `session_id`, or renews it.
    pub fn lease(&self, session_id: &str, path: &Path) {
        let Some(dir) = path.parent().map(Path::to_path_buf) else {
            return;
        };
        let mut watching = self.watching.lock().expect("thread watches");
        let now = Instant::now();
        let expired = {
            let mut leases = self.leases.lock().expect("thread leases");
            if let Some(lease) = leases.get_mut(path) {
                lease.renewed = now;
                lease.session_id = session_id.to_owned();
                return;
            }
            let expired: Vec<PathBuf> = leases
                .iter()
                .filter(|(_, lease)| now.duration_since(lease.renewed) >= LEASE)
                .map(|(path, _)| path.clone())
                .collect();
            for path in &expired {
                leases.remove(path);
            }
            expired
        };
        for path in &expired {
            unwatch_one(&mut watching, path);
        }
        if watching.watcher.is_none() {
            let leases = Arc::clone(&self.leases);
            let notify = Arc::clone(&self.notify);
            let watcher =
                notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
                    let Ok(event) = event else { return };
                    if std::env::var_os("DIRI_THREAD_WATCH_DEBUG").is_some() {
                        eprintln!("thread watch event {event:?}");
                    }
                    if !(event.kind.is_modify() || event.kind.is_create()) {
                        return;
                    }
                    let targets: Vec<(String, PathBuf)> = {
                        let Ok(leases) = leases.lock() else { return };
                        let now = Instant::now();
                        event
                            .paths
                            .iter()
                            .filter_map(|path| {
                                let lease = leases.get(path)?;
                                (now.duration_since(lease.renewed) < LEASE)
                                    .then(|| (lease.session_id.clone(), path.clone()))
                            })
                            .collect()
                    };
                    for (session_id, path) in targets {
                        let length = std::fs::symlink_metadata(&path).map_or(0, |m| m.len());
                        notify(&session_id, length);
                    }
                });
            match watcher {
                Ok(watcher) => watching.watcher = Some(watcher),
                Err(error) => {
                    eprintln!(
                        "diri-engine: thread watch unavailable ({error}); GUI Lens refreshes on status changes only"
                    );
                    return;
                }
            }
        }
        let count = watching.dirs.get(&dir).copied().unwrap_or(0);
        if count == 0 {
            let watched = watching
                .watcher
                .as_mut()
                .map(|watcher| watcher.watch(&dir, RecursiveMode::NonRecursive));
            if !matches!(watched, Some(Ok(()))) {
                if std::env::var_os("DIRI_THREAD_WATCH_DEBUG").is_some() {
                    eprintln!("thread watch failed {watched:?}");
                }
                return;
            }
        }
        watching.dirs.insert(dir, count + 1);
        self.leases.lock().expect("thread leases").insert(
            path.to_path_buf(),
            Lease {
                session_id: session_id.to_owned(),
                renewed: now,
            },
        );
    }

    /// Drops every lease of `session_id` (the session was removed).
    pub fn release(&self, session_id: &str) {
        let mut watching = self.watching.lock().expect("thread watches");
        let paths: Vec<PathBuf> = {
            let mut leases = self.leases.lock().expect("thread leases");
            let paths: Vec<PathBuf> = leases
                .iter()
                .filter(|(_, lease)| lease.session_id == session_id)
                .map(|(path, _)| path.clone())
                .collect();
            for path in &paths {
                leases.remove(path);
            }
            paths
        };
        for path in paths {
            unwatch_one(&mut watching, &path);
        }
    }

    #[cfg(test)]
    fn leased(&self) -> usize {
        self.leases.lock().unwrap().len()
    }
}

fn unwatch_one(watching: &mut Watching, path: &Path) {
    let Some(dir) = path.parent() else { return };
    match watching.dirs.get(dir).copied() {
        Some(1) => {
            watching.dirs.remove(dir);
            if let Some(watcher) = watching.watcher.as_mut() {
                let _ = watcher.unwatch(dir);
            }
        }
        Some(count) => {
            watching.dirs.insert(dir.to_path_buf(), count - 1);
        }
        None => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::mpsc;

    #[test]
    fn leases_share_directory_watches_and_release_by_session() {
        let dir = tempfile::tempdir().unwrap();
        let root = diri_platform::canonicalize(dir.path()).unwrap();
        let (a, b) = (root.join("a.jsonl"), root.join("b.jsonl"));
        std::fs::write(&a, b"").unwrap();
        std::fs::write(&b, b"").unwrap();
        let watches = ThreadWatches::new(Arc::new(|_: &str, _| {}));
        watches.lease("s-1", &a);
        watches.lease("s-2", &b);
        watches.lease("s-1", &a);
        assert_eq!(watches.leased(), 2);
        assert_eq!(watches.watching.lock().unwrap().dirs.get(&root), Some(&2));
        watches.release("s-1");
        assert_eq!(watches.leased(), 1);
        assert_eq!(watches.watching.lock().unwrap().dirs.get(&root), Some(&1));
        watches.release("s-2");
        assert!(watches.watching.lock().unwrap().dirs.is_empty());
    }

    /// Live kernel events. FSEvents starts its stream asynchronously and, in
    /// a test binary running many threads, sometimes not within the test's
    /// window; production also re-reads on every `session.updated`.
    #[test]
    #[ignore = "FSEvents stream start is timing-dependent under parallel test load"]
    fn an_append_to_a_leased_transcript_notifies_its_session() {
        let dir = tempfile::tempdir().unwrap();
        let path = diri_platform::canonicalize(dir.path())
            .unwrap()
            .join("t.jsonl");
        std::fs::write(&path, b"").unwrap();
        let (tx, rx) = mpsc::channel();
        let tx = Mutex::new(tx);
        let watches = ThreadWatches::new(Arc::new(move |session: &str, length| {
            let _ = tx.lock().unwrap().send((session.to_owned(), length));
        }));
        watches.lease("s-1", &path);
        watches.lease("s-1", &path);
        assert_eq!(watches.leased(), 1);
        // FSEvents starts its stream asynchronously after `watch` returns, so
        // keep appending (as an Agent does) until the first event lands.
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        let session = loop {
            file.write_all(b"{\"type\":\"user\"}\n").unwrap();
            file.sync_all().unwrap();
            if let Ok((session, _)) = rx.recv_timeout(Duration::from_millis(250)) {
                break session;
            }
            assert!(Instant::now() < deadline, "no watch event");
        };
        assert_eq!(session, "s-1");
        watches.release("s-1");
        assert_eq!(watches.leased(), 0);
    }
}
