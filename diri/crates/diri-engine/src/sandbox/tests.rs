//! Pool rules with a fake backend: capacity, leases, takeover and every
//! reclamation path, deterministic and without a browser. The real-browser
//! tests live in `tests/sandbox_ghost.rs`.

use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;
use diri_proto::sandbox::SandboxStartParams;

#[derive(Default)]
struct Counters {
    launched: AtomicUsize,
    shut_down: AtomicUsize,
    trimmed: AtomicUsize,
    /// Each launched instance's liveness, to simulate a crash.
    alive: Mutex<Vec<Arc<AtomicBool>>>,
    /// Launches block until this many are in flight (a burst).
    barrier: Option<Arc<std::sync::Barrier>>,
}

struct FakeBackend {
    counters: Arc<Counters>,
    fail: bool,
}

struct FakeInstance {
    counters: Arc<Counters>,
    alive: Arc<AtomicBool>,
    down: bool,
    actions: Vec<SandboxAction>,
}

use std::sync::atomic::AtomicBool;

impl SandboxBackend for FakeBackend {
    fn kind(&self) -> SandboxBackendKind {
        SandboxBackendKind::Ghost
    }

    fn availability(&self) -> Result<String, String> {
        Ok("fake".into())
    }

    fn launch(&self, spec: &LaunchSpec) -> Result<Box<dyn SandboxInstance>, SandboxError> {
        assert!(spec.state_dir.is_dir(), "state dir exists before launch");
        if let Some(barrier) = &self.counters.barrier {
            barrier.wait();
        }
        if self.fail {
            return Err(SandboxError::new(
                proto::BACKEND_UNAVAILABLE,
                "fake failure",
            ));
        }
        self.counters.launched.fetch_add(1, Ordering::SeqCst);
        std::fs::write(spec.state_dir.join("profile"), b"state").unwrap();
        let alive = Arc::new(AtomicBool::new(true));
        self.counters.alive.lock().unwrap().push(Arc::clone(&alive));
        Ok(Box::new(FakeInstance {
            counters: Arc::clone(&self.counters),
            alive,
            down: false,
            actions: Vec::new(),
        }))
    }
}

impl SandboxInstance for FakeInstance {
    fn act(&mut self, action: &SandboxAction) -> Result<Value, SandboxError> {
        self.actions.push(action.clone());
        Ok(serde_json::json!({"done": self.actions.len()}))
    }

    fn screenshot_png(&mut self) -> Result<Vec<u8>, SandboxError> {
        let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        png.extend_from_slice(&640_u32.to_be_bytes());
        png.extend_from_slice(&400_u32.to_be_bytes());
        Ok(png)
    }

    fn screenshot_jpeg(&mut self, _max_edge: u32) -> Result<Vec<u8>, SandboxError> {
        Ok(vec![0xFF, 0xD8])
    }

    fn location(&mut self) -> (String, String) {
        ("about:blank".into(), String::new())
    }

    fn trim(&mut self) {
        self.counters.trimmed.fetch_add(1, Ordering::SeqCst);
    }

    fn alive(&mut self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    fn shutdown(&mut self) {
        assert!(!self.down, "shut down twice");
        self.down = true;
        self.counters.shut_down.fetch_add(1, Ordering::SeqCst);
    }
}

struct Harness {
    pool: SandboxPool,
    counters: Arc<Counters>,
    live: Arc<Mutex<HashSet<String>>>,
    root: tempfile::TempDir,
}

fn harness_with(capacity: usize, counters: Counters, fail: bool) -> Harness {
    let root = tempfile::tempdir().unwrap();
    let counters = Arc::new(counters);
    let pool = SandboxPool::new(
        PoolConfig {
            capacity,
            state_root: root.path().join("state"),
            artifact_root: root.path().join("artifacts"),
            sleep_after: Duration::from_secs(3600),
            reclaim_after: Duration::from_secs(7200),
            // Tests drive the reaper by hand through `reap_once`.
            reap_interval: Duration::from_secs(3600),
        },
        vec![
            Box::new(FakeBackend {
                counters: Arc::clone(&counters),
                fail,
            }),
            Box::new(MicroVmBackend),
        ],
    );
    let live: Arc<Mutex<HashSet<String>>> =
        Arc::new(Mutex::new(["a", "b", "c"].map(String::from).into()));
    let check = Arc::clone(&live);
    pool.set_liveness(Arc::new(move |id: &str| check.lock().unwrap().contains(id)));
    Harness {
        pool,
        counters,
        live,
        root,
    }
}

fn harness(capacity: usize) -> Harness {
    harness_with(capacity, Counters::default(), false)
}

fn start(h: &Harness, session: &str) -> Result<SandboxInfo, SandboxError> {
    h.pool
        .start(Caller::Session(session), &SandboxStartParams::default())
}

fn state_dirs(h: &Harness) -> usize {
    std::fs::read_dir(h.root.path().join("state"))
        .map(|entries| entries.count())
        .unwrap_or(0)
}

#[test]
fn the_pool_never_exceeds_capacity() {
    let h = harness(2);
    start(&h, "a").unwrap();
    start(&h, "b").unwrap();
    let error = start(&h, "c").unwrap_err();
    assert_eq!(error.code, proto::POOL_FULL);
    assert_eq!(h.pool.occupied(), 2);
    assert_eq!(h.counters.launched.load(Ordering::SeqCst), 2);
}

#[test]
fn a_burst_of_starts_reserves_before_launching() {
    // Four threads race for three slots while launches are in flight.
    let counters = Counters {
        barrier: Some(Arc::new(std::sync::Barrier::new(3))),
        ..Counters::default()
    };
    let h = Arc::new(harness_with(3, counters, false));
    let results: Vec<_> = (0..4)
        .map(|_| {
            let h = Arc::clone(&h);
            std::thread::spawn(move || start(&h, "a").map(|info| info.id))
        })
        .collect::<Vec<_>>()
        .into_iter()
        .map(|thread| thread.join().unwrap())
        .collect();
    let started = results.iter().filter(|result| result.is_ok()).count();
    let refused = results
        .iter()
        .filter(|result| matches!(result, Err(error) if error.code == proto::POOL_FULL))
        .count();
    assert_eq!((started, refused), (3, 1));
    assert_eq!(h.pool.occupied(), 3);
}

#[test]
fn a_failed_launch_releases_its_slot_and_files() {
    let h = harness_with(1, Counters::default(), true);
    assert_eq!(start(&h, "a").unwrap_err().code, proto::BACKEND_UNAVAILABLE);
    assert_eq!(h.pool.occupied(), 0);
    assert_eq!(state_dirs(&h), 0);
}

#[test]
fn only_the_owner_acts_snapshots_or_stops() {
    let h = harness(4);
    let id = start(&h, "a").unwrap().id;
    let click = SandboxAction::Click {
        x: Some(1.0),
        y: Some(2.0),
        selector: None,
    };
    assert!(h.pool.act(Caller::Session("a"), &id, &click).is_ok());
    for refused in [
        h.pool.act(Caller::Session("b"), &id, &click).unwrap_err(),
        h.pool
            .snapshot(Caller::Session("b"), &id, None, false)
            .unwrap_err(),
        h.pool.stop(Caller::Session("b"), &id).unwrap_err(),
    ] {
        assert_eq!(refused.code, proto::NOT_OWNER);
    }
    assert_eq!(h.pool.occupied(), 1);
    h.pool.stop(Caller::Session("a"), &id).unwrap();
    assert_eq!(h.pool.occupied(), 0);
    assert_eq!(
        h.pool
            .act(Caller::Session("a"), &id, &click)
            .unwrap_err()
            .code,
        proto::NOT_FOUND
    );
}

#[test]
fn a_dead_session_cannot_start_one() {
    let h = harness(4);
    assert_eq!(start(&h, "ghost").unwrap_err().code, proto::NOT_OWNER);
    assert_eq!(h.pool.occupied(), 0);
}

#[test]
fn the_person_takes_over_and_hands_back() {
    let h = harness(4);
    let id = start(&h, "a").unwrap().id;
    let press = SandboxAction::Press { key: "a".into() };
    h.pool.act(Caller::Person, &id, &press).unwrap();
    let info = h.pool.info(&id).unwrap();
    assert!(info.human_until.is_some());
    assert_eq!(
        h.pool
            .act(Caller::Session("a"), &id, &press)
            .unwrap_err()
            .code,
        proto::HUMAN_CONTROL
    );
    // Hand back: the takeover lapses.
    h.pool.state().slots.get_mut(&id).unwrap().human_until =
        Some(Instant::now() - Duration::from_millis(1));
    h.pool.act(Caller::Session("a"), &id, &press).unwrap();
}

#[test]
fn an_ended_session_leaks_nothing() {
    let h = harness(4);
    let first = start(&h, "a").unwrap().id;
    start(&h, "a").unwrap();
    let kept = start(&h, "b").unwrap().id;
    assert_eq!(state_dirs(&h), 3);
    h.live.lock().unwrap().remove("a");
    assert!(h.pool.reap_once());
    assert_eq!(h.counters.shut_down.load(Ordering::SeqCst), 2);
    assert_eq!(h.pool.occupied(), 1);
    assert_eq!(state_dirs(&h), 1);
    assert!(h.pool.info(&first).is_err());
    assert!(h.pool.info(&kept).is_ok());
}

#[test]
fn release_owner_stops_exactly_that_sessions_sandboxes() {
    let h = harness(4);
    start(&h, "a").unwrap();
    start(&h, "b").unwrap();
    assert_eq!(h.pool.release_owner("a"), 1);
    assert_eq!(h.pool.release_owner("a"), 0);
    assert_eq!(h.pool.occupied(), 1);
}

#[test]
fn a_crashed_backend_is_reaped() {
    let h = harness(4);
    start(&h, "a").unwrap();
    h.counters.alive.lock().unwrap()[0].store(false, Ordering::SeqCst);
    assert!(h.pool.reap_once());
    assert_eq!(h.pool.occupied(), 0);
    assert_eq!(h.counters.shut_down.load(Ordering::SeqCst), 1);
    assert_eq!(state_dirs(&h), 0);
}

#[test]
fn idle_sandboxes_sleep_then_are_reclaimed() {
    let h = harness(4);
    let id = start(&h, "a").unwrap().id;
    let age = |by: Duration| {
        h.pool.state().slots.get_mut(&id).unwrap().last_active = Instant::now() - by;
    };
    age(Duration::from_secs(3601));
    h.pool.reap_once();
    assert_eq!(h.counters.trimmed.load(Ordering::SeqCst), 1);
    assert!(h.pool.info(&id).unwrap().sleeping);
    h.pool.reap_once();
    assert_eq!(h.counters.trimmed.load(Ordering::SeqCst), 1, "trim once");
    // An action wakes it.
    h.pool
        .act(Caller::Session("a"), &id, &SandboxAction::Wait { ms: 0 })
        .unwrap();
    assert!(!h.pool.info(&id).unwrap().sleeping);
    age(Duration::from_secs(7201));
    h.pool.reap_once();
    assert_eq!(h.pool.occupied(), 0);
}

#[test]
fn the_reaper_stops_when_the_pool_empties() {
    let h = harness(4);
    let id = start(&h, "a").unwrap().id;
    assert!(h.pool.state().reaper_running);
    h.pool.stop(Caller::Person, &id).unwrap();
    assert!(!h.pool.reap_once());
    assert!(!h.pool.state().reaper_running);
}

#[test]
fn dropping_the_pool_reclaims_everything() {
    let h = harness(4);
    start(&h, "a").unwrap();
    start(&h, "b").unwrap();
    let counters = Arc::clone(&h.counters);
    let root = h.root;
    // The reaper thread holds a clone; shutdown is the Engine's path.
    h.pool.shutdown();
    drop(h.pool);
    assert_eq!(counters.shut_down.load(Ordering::SeqCst), 2);
    assert_eq!(
        std::fs::read_dir(root.path().join("state"))
            .unwrap()
            .count(),
        0
    );
}

#[test]
fn snapshots_are_private_files_with_a_content_hash() {
    let h = harness(4);
    let id = start(&h, "a").unwrap().id;
    let snap = h
        .pool
        .snapshot(Caller::Session("a"), &id, Some("before/../x"), true)
        .unwrap();
    assert!(snap.path.ends_with("001-beforex.png"), "{}", snap.path);
    assert_eq!((snap.width, snap.height), (640, 400));
    assert_eq!(snap.sha256.len(), 64);
    assert!(snap.jpeg.is_some());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(&snap.path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}

#[test]
fn micro_vm_fails_closed() {
    let h = harness(4);
    let error = h
        .pool
        .start(
            Caller::Session("a"),
            &SandboxStartParams {
                backend: SandboxBackendKind::MicroVm,
                ..SandboxStartParams::default()
            },
        )
        .unwrap_err();
    assert_eq!(error.code, proto::BACKEND_UNAVAILABLE);
    let listed = h.pool.list();
    assert_eq!(listed.capacity, 4);
    assert!(
        listed
            .backends
            .iter()
            .any(|b| b.kind == SandboxBackendKind::MicroVm && !b.available)
    );
}

#[test]
fn stale_state_from_a_dead_engine_is_swept() {
    let root = tempfile::tempdir().unwrap();
    // A pid that is certainly not running: spawn and reap a child.
    let mut child = std::process::Command::new("true").spawn().unwrap();
    let dead = child.id();
    child.wait().unwrap();
    std::fs::create_dir(root.path().join(format!("{dead}-g1"))).unwrap();
    std::fs::create_dir(root.path().join(format!("{}-g2", std::process::id()))).unwrap();
    std::fs::create_dir(root.path().join("unrelated")).unwrap();
    assert_eq!(sweep_stale_state(root.path()), 1);
    let mut left: Vec<String> = std::fs::read_dir(root.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    left.sort();
    assert_eq!(left.len(), 2);
    assert!(left.contains(&"unrelated".to_owned()));
}

#[cfg(unix)]
#[test]
fn a_symlinked_state_root_is_refused() {
    let root = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    let link = root.path().join("state");
    std::os::unix::fs::symlink(elsewhere.path(), &link).unwrap();
    assert!(create_state_dir(&link, "g1").is_err());
}
