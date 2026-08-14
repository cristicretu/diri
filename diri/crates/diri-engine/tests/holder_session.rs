//! Holder-backed sessions: the engine-level guarantee that a session
//! survives its daemon.
//!
//! These tests play the daemon's role twice: spawn a held session, throw the
//! session object away (a daemon crash in miniature), then adopt what the
//! holder kept alive from a brand-new registry — the exact move a Rust
//! daemon makes when it replaces a dead one, Swift or Rust.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use diri_engine::holder::{HolderClient, HolderPaths};
use diri_engine::session::{HolderConfig, Session, SessionSpec};
use diri_engine::{Authority, ManifestEngine, OutputLog, PtySpec, Registry};
use diri_proto::SessionStatus;

fn engine() -> Arc<ManifestEngine> {
    let dir = diri_engine::detect::bundled_manifest_dir()
        .canonicalize()
        .expect("manifests");
    let (engine, _) = ManifestEngine::load_dir(&dir).expect("load");
    Arc::new(engine)
}

fn holders_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("diri-hsess-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create dir");
    dir
}

fn holder_config(root: &Path) -> HolderConfig {
    HolderConfig {
        holders_dir: root.join("holders"),
        executable: PathBuf::from(env!("CARGO_BIN_EXE_diri-holder")),
    }
}

fn shell_spec(id: &str, script: &str, logs: &Path, holder: Option<HolderConfig>) -> SessionSpec {
    SessionSpec {
        id: id.into(),
        pty: PtySpec::new(vec!["/bin/sh".into(), "-c".into(), script.into()], "/tmp")
            .env("PATH", "/usr/bin:/bin")
            .env("TERM", "xterm-256color")
            .size(80, 24),
        manifest_id: "shell".into(),
        authority: Authority::ProcessOnly,
        logs_dir: logs.to_path_buf(),
        holder,
        remote: None,
        defer_launch: false,
    }
}

fn wait_until(what: &str, timeout: Duration, mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if check() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("timed out waiting for {what}");
}

fn log_contains(logs: &Path, id: &str, needle: &[u8]) -> bool {
    let Ok(mut log) = OutputLog::reader(logs, id) else {
        return false;
    };
    log.refresh_from_disk();
    let tail = log.tail_offset();
    let (_, bytes) = log.read(0, tail as usize);
    bytes.windows(needle.len()).any(|window| window == needle)
}

#[test]
fn a_held_session_survives_its_session_object_and_is_adoptable() {
    let root = holders_dir("survive");
    let logs = root.join("logs");
    let holder = holder_config(&root);
    let engine = engine();

    // Daemon #1 spawns the session and immediately "crashes" (drop).
    let session = Session::spawn(
        shell_spec("s_sur", "cat", &logs, Some(holder.clone())),
        Arc::clone(&engine),
    )
    .expect("spawn held");
    session.write_input(b"before the crash\n").expect("write");
    wait_until("first write in log", Duration::from_secs(5), || {
        log_contains(&logs, "s_sur", b"before the crash")
    });
    drop(session);

    // The holder — a separate process — still owns a live child.
    let paths = HolderPaths::new(&holder.holders_dir, "s_sur");
    let client = HolderClient::new(paths.socket());
    let stat = client.stat().expect("holder survives the session object");
    assert!(stat.alive, "the child survived the daemon");

    // Daemon #2 adopts and carries on.
    let mut adopted = Session::adopt(
        shell_spec("s_sur", "", &logs, Some(holder.clone())),
        &holder,
        &stat,
        engine,
    )
    .expect("adopt");
    adopted.write_input(b"after the restart\n").expect("write");
    wait_until("second write in log", Duration::from_secs(5), || {
        log_contains(&logs, "s_sur", b"after the restart")
    });

    let exit = adopted
        .terminate(Duration::from_secs(2))
        .expect("terminate");
    assert!(
        matches!(exit, diri_engine::Exit::Signal(_)),
        "kill-tree death is a signal: {exit:?}"
    );
    assert!(!client.is_alive(), "terminate really ends the child");
}

#[test]
fn a_registry_restore_adopts_live_holders_from_a_previous_life() {
    let root = holders_dir("registry");
    let logs = root.join("logs");
    let holder = holder_config(&root);
    let state_file = root.join("state.json");

    // Life #1: spawn one held session and one that exits immediately, then
    // drop the whole registry mid-flight.
    {
        let mut registry = Registry::new(engine(), &state_file);
        registry
            .spawn(
                shell_spec("s_live", "cat", &logs, Some(holder.clone())),
                record("s_live"),
            )
            .expect("spawn live");
        registry
            .spawn(
                shell_spec("s_done", "exit 0", &logs, Some(holder.clone())),
                record("s_done"),
            )
            .expect("spawn done");
        wait_until("short-lived session exits", Duration::from_secs(5), || {
            registry
                .views()
                .iter()
                .any(|view| view.id == "s_done" && view.exited)
        });
        registry.persist().expect("persist");
        // Dropping held sessions detaches; nothing is killed.
    }

    // Life #2: a fresh registry finds the records and adopts what is still
    // alive — and only that.
    let mut registry = Registry::new(engine(), &state_file);
    registry.load().expect("load state");
    let adopted = registry.restore(&holder, &logs);
    assert_eq!(adopted, vec!["s_live".to_string()], "only the live one");

    let session = registry.get("s_live").expect("adopted session");
    session.write_input(b"hello second life\n").expect("write");
    wait_until("write lands", Duration::from_secs(5), || {
        log_contains(&logs, "s_live", b"hello second life")
    });

    registry
        .terminate("s_live", Duration::from_secs(2))
        .expect("terminate");
}

#[test]
fn a_held_child_exit_is_observed_from_the_marker() {
    let root = holders_dir("exit");
    let logs = root.join("logs");
    let holder = holder_config(&root);

    let session = Session::spawn(
        shell_spec("s_exit", "exit 7", &logs, Some(holder)),
        engine(),
    )
    .expect("spawn");
    wait_until("exit observed", Duration::from_secs(5), || {
        session.view().exited
    });
    assert!(
        matches!(session.status(), SessionStatus::Exited(_)),
        "status: {:?}",
        session.status()
    );
}

fn record(id: &str) -> diri_proto::SessionRecord {
    use diri_proto::*;
    SessionRecord {
        id: SessionId(id.into()),
        kind: AgentKind::SHELL,
        cwd: "/tmp".into(),
        project_id: ProjectId("p".into()),
        worktree_path: None,
        git_branch: None,
        title: "test".into(),
        title_source: TitleSource::Placeholder,
        originating_prompt: None,
        agent_session_id: None,
        transcript_path: None,
        status: SessionStatus::Starting,
        status_evidence: None,
        needs_input: None,
        resumability: Resumability::NotResumable,
        parent: None,
        created_at: DateMillis(0.0),
        updated_at: DateMillis(0.0),
        last_turn_completed_at: None,
        last_seen_at: None,
        pinned: false,
        archived_at: None,
        host: None,
        remote_persistence: None,
        hibernation: None,
        memory_bytes: None,
        artifacts: None,
        pull_requests: None,
        listening_ports: None,
        foreground_agent: None,
    }
}

/// The wake-on-input contract: typing into a SIGSTOPped session queues the
/// bytes (never wedging the PTY), and waking flushes them in order — no
/// keystroke lost, the reply arrives after SIGCONT.
#[test]
fn input_to_a_hibernated_session_queues_and_flushes_on_wake() {
    let root = holders_dir("hib");
    let logs = root.join("logs");
    let holder = holder_config(&root);
    let state_file = root.join("state.json");

    let mut registry = Registry::new(engine(), &state_file);
    registry
        .spawn(
            shell_spec("s_hib", "cat", &logs, Some(holder.clone())),
            record("s_hib"),
        )
        .expect("spawn");
    wait_until("cat is up", Duration::from_secs(5), || {
        registry.get("s_hib").is_some_and(|s| s.child_pid() > 1)
    });

    registry
        .hibernate("s_hib", diri_proto::HibernationReason::Manual)
        .expect("hibernate");

    // Typed while frozen: queued, not written — cat can't echo while stopped.
    registry
        .get("s_hib")
        .expect("session")
        .write_input(b"typed-while-frozen\n")
        .expect("queued write");
    std::thread::sleep(Duration::from_millis(600));
    assert!(
        !log_contains(&logs, "s_hib", b"typed-while-frozen"),
        "a stopped tree must not echo"
    );

    // Wake: SIGCONT + flush; the echo lands and the record clears.
    registry.wake_session("s_hib").expect("wake");
    wait_until("queued input flushed", Duration::from_secs(5), || {
        log_contains(&logs, "s_hib", b"typed-while-frozen")
    });
    assert!(
        registry
            .records()
            .iter()
            .find(|r| r.id.0 == "s_hib")
            .expect("record")
            .hibernation
            .is_none()
    );

    registry
        .terminate("s_hib", Duration::from_secs(2))
        .expect("terminate");
}
