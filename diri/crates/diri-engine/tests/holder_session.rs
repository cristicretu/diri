//! Holder-backed sessions: the engine-level guarantee that a session
//! survives its daemon.
//!
//! These tests play the daemon's role twice: spawn a held session, throw the
//! session object away (a daemon crash in miniature), then adopt what the
//! holder kept alive from a brand-new registry — the exact move a Rust
//! daemon makes when it replaces a dead one, Swift or Rust.

#![cfg(unix)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use diri_engine::holder::{HolderClient, HolderLaunchSpec, HolderPaths, HolderServer};
use diri_engine::session::{HolderConfig, Session, SessionSpec};
use diri_engine::{Authority, ManifestEngine, OutputLog, PtySpec, Registry};
use diri_proto::{AgentKind, SessionStatus};

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
fn a_capsule_and_hook_seed_recover_a_holder_when_global_state_is_gone() {
    let root = holders_dir("capsule");
    let logs = root.join("logs");
    let holder = holder_config(&root);
    let state_file = root.join("state.json");

    let paths = HolderPaths::new(&holder.holders_dir, "s_recover");
    let launch = HolderLaunchSpec {
        session_id: "s_recover".into(),
        socket_path: paths.socket().to_string_lossy().into_owned(),
        pid_file_path: paths.pid_file().to_string_lossy().into_owned(),
        log_file_path: logs.join("s_recover.bin").to_string_lossy().into_owned(),
        argv: vec!["/bin/cat".into()],
        cwd: "/tmp".into(),
        environment: HashMap::from([
            ("PATH".into(), "/usr/bin:/bin".into()),
            ("TERM".into(), "xterm-256color".into()),
        ]),
        cols: 80,
        rows: 24,
        disk_capacity: diri_engine::holder::protocol::DEFAULT_DISK_CAPACITY,
    };
    let holder_thread = std::thread::spawn(move || HolderServer::run(launch));
    let client = HolderClient::new(paths.socket());
    wait_until("holder ready", Duration::from_secs(5), || client.is_alive());

    let recovery_dir = root.join("sessions/s_recover");
    let store = diri_proto::recovery::SessionRecoveryStore::new(&recovery_dir);
    let profile = diri_proto::AgentAccountProfile {
        id: "work".into(),
        label: "Work".into(),
        agent: "claude-code".into(),
        host: None,
        config_home: "/tmp/work-claude".into(),
        is_default: false,
        login_store: None,
    };
    store
        .write_capsule(&diri_proto::recovery::SessionRecoveryCapsule {
            account_profile: Some(profile.clone()),
            version: diri_proto::recovery::SessionRecoveryCapsule::VERSION,
            session_id: diri_proto::SessionId::new("s_recover"),
            manifest_id: AgentKind::CLAUDE_CODE_ID.into(),
            cwd: "/tmp/recovered-project".into(),
            created_at: diri_proto::DateMillis(10.0),
            agent_session_id: None,
            transcript_path: None,
        })
        .expect("capsule");
    store
        .write_activity(&diri_proto::recovery::HookActivitySeed {
            native_request_id: None,
            native_turn_id: None,
            claude_pending_work: None,
            version: diri_proto::recovery::HookActivitySeed::VERSION,
            kind: "claude-hook".into(),
            event: Some("Stop".into()),
            occurred_at_ms: 100,
            agent_session_id: Some("conversation-recovered".into()),
            transcript_path: None,
            notification_type: None,
            tool_name: None,
        })
        .expect("seed");

    let mut registry = Registry::new(engine(), &state_file);
    let adopted = registry.restore(&holder, &logs);
    assert_eq!(adopted, ["s_recover"]);
    let recovered = registry.record("s_recover").expect("recovered record");
    assert_eq!(recovered.kind, AgentKind::CLAUDE_CODE);
    assert_eq!(recovered.account_profile, Some(profile));
    assert_eq!(recovered.cwd, "/tmp/recovered-project");
    assert_eq!(
        recovered.agent_session_id.as_deref(),
        Some("conversation-recovered")
    );
    wait_until("hook seed restores idle", Duration::from_secs(3), || {
        matches!(
            registry.record("s_recover").map(|record| record.status),
            Some(SessionStatus::Idle)
        )
    });
    assert!(
        state_file.is_file(),
        "the reconstructed record is persisted"
    );

    registry
        .terminate("s_recover", Duration::from_secs(2))
        .expect("terminate");
    holder_thread
        .join()
        .expect("holder thread")
        .expect("holder exits cleanly");
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
        attention_state: None,
        id: SessionId(id.into()),
        kind: AgentKind::SHELL,
        cwd: "/tmp".into(),
        project_id: ProjectId("p".into()),
        worktree_path: None,
        git_branch: None,
        title: "test".into(),
        title_source: TitleSource::Placeholder,
        account_profile: None,
        originating_prompt: None,
        agent_session_id: None,
        transcript_path: None,
        status: SessionStatus::Starting,
        status_evidence: None,
        needs_input: None,
        resumability: Resumability::NotResumable,
        capabilities: None,
        parent: None,
        created_at: DateMillis(0.0),
        updated_at: DateMillis(0.0),
        last_turn_completed_at: None,
        last_seen_at: None,
        pinned: false,
        archived_at: None,
        host: None,
        remote_persistence: None,
        remote_connection: None,
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

#[test]
fn failed_holder_stop_keeps_the_live_session_tracked_until_retry() {
    let root = holders_dir("failed-stop");
    let logs = root.join("logs");
    let holder = holder_config(&root);
    let mut registry = Registry::new(engine(), root.join("state.json"));
    registry
        .spawn(
            shell_spec("s_stop_retry", "cat", &logs, Some(holder.clone())),
            record("s_stop_retry"),
        )
        .unwrap();
    let pid = registry.get("s_stop_retry").unwrap().child_pid();
    let paths = HolderPaths::new(&holder.holders_dir, "s_stop_retry");
    let socket = paths.socket();
    let parked = socket.with_extension("unavailable");
    std::fs::rename(&socket, &parked).unwrap();
    let stopped = registry.terminate("s_stop_retry", Duration::from_millis(100));
    std::fs::rename(&parked, &socket).unwrap();
    assert!(
        stopped.is_err(),
        "missing Holder acknowledgement must fail closed"
    );
    assert_eq!(registry.get("s_stop_retry").unwrap().child_pid(), pid);
    assert_eq!(unsafe { libc::kill(pid, 0) }, 0);
    registry
        .terminate("s_stop_retry", Duration::from_secs(3))
        .unwrap();
    assert!(registry.get("s_stop_retry").is_none());
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn a_held_shell_is_working_while_a_foreground_job_runs() {
    let root = holders_dir("fgwork");
    let logs = root.join("logs");
    let holder = holder_config(&root);
    let spec = SessionSpec {
        id: "s_fgwork".into(),
        pty: PtySpec::new(
            vec![
                "/bin/bash".into(),
                "--norc".into(),
                "--noprofile".into(),
                "-i".into(),
            ],
            "/tmp",
        )
        .env("PATH", "/usr/bin:/bin")
        .env("TERM", "xterm-256color")
        .env("HOME", "/tmp")
        .env("PS1", "$ "),
        manifest_id: "shell".into(),
        authority: Authority::ProcessOnly,
        logs_dir: logs.to_path_buf(),
        holder: Some(holder),
        remote: None,
        defer_launch: false,
    };
    let mut session = Session::spawn(spec, engine()).expect("spawn");
    wait_until("idle shell prompt", Duration::from_secs(5), || {
        matches!(session.status(), SessionStatus::Idle)
    });

    session.write_input(b"sleep 30\n").expect("write sleep");
    wait_until("working foreground job", Duration::from_secs(3), || {
        matches!(session.status(), SessionStatus::Working)
    });

    session
        .terminate(Duration::from_secs(2))
        .expect("terminate");
    let _ = std::fs::remove_dir_all(root);
}

/// The confirmed recovery gap: a session that completed while the Engine was
/// alive must keep its final screen after the Engine is replaced, even though
/// its Holder is gone and no live Session can be adopted.
#[test]
fn completed_terminal_survives_engine_replacement() {
    let root = holders_dir("completed");
    let logs = root.join("logs");
    std::fs::create_dir_all(&logs).unwrap();
    let state = root.join("state.json");
    let holder = holder_config(&root);
    let id = "completed-run";
    let mut record = record(id);

    let mut registry = Registry::new(engine(), &state);
    registry
        .spawn(
            shell_spec(
                id,
                "printf 'retained line\\n'; printf 'final prompt'; exit 3",
                &logs,
                Some(holder.clone()),
            ),
            record.clone(),
        )
        .expect("spawn held");
    let mut published = HashMap::new();
    let mut publications = Vec::new();
    // The Holder drains its log and takes the final capture before it exposes
    // the exit; on a loaded CI runner that has taken longer than ten seconds
    // (two releases' bump PRs failed here and passed on rerun). The wait is a
    // ceiling, not the expected time, so give it real headroom.
    wait_until(
        "the exit and its retained terminal",
        Duration::from_secs(45),
        || {
            registry.changed_since(&mut published);
            publications.extend(registry.take_completed_publications());
            !publications.is_empty()
        },
    );
    assert_eq!(publications.len(), 1);
    assert!(
        registry.completed_run(id).is_none(),
        "a live Session object still owns the record; no fallback yet"
    );
    publications
        .pop()
        .unwrap()
        .publish()
        .expect("publish outside the lock");
    let artifacts = std::fs::read_dir(root.join("completed-terminals"))
        .unwrap()
        .count();
    assert_eq!(artifacts, 1);
    registry.persist_for_shutdown().unwrap();
    drop(registry);

    // The replacement Engine finds no Holder socket to adopt, only the record.
    let mut restored = Registry::new(engine(), &state);
    restored.load().unwrap();
    assert!(restored.restore(&holder, &logs).is_empty());
    assert!(restored.get(id).is_none());
    let handle = restored
        .completed_run(id)
        .expect("a bound, exited local record has a retained run");
    record.status = handle.record().status.clone();
    assert!(matches!(
        &record.status,
        SessionStatus::Exited(exit) if exit.code == Some(3)
    ));
    let terminal = handle.load().unwrap().expect("the exact run was retained");
    assert_eq!(terminal.exit.code, Some(3));
    let screen = terminal.screen().unwrap();
    let text = screen.lines().join("\n");
    assert!(
        text.contains("retained line") && text.contains("final prompt"),
        "{text}"
    );

    // Removing the record removes its retained terminal with it.
    restored.remove(id, &logs).unwrap();
    assert!(restored.completed_run(id).is_none());
    assert_eq!(
        std::fs::read_dir(root.join("completed-terminals"))
            .unwrap()
            .count(),
        0
    );
}

/// An explicit Stop is a completion too: the Session object leaves the
/// Registry immediately, so its final screen must travel with it.
#[test]
fn stopped_session_terminal_is_retained() {
    let root = holders_dir("stopped");
    let logs = root.join("logs");
    std::fs::create_dir_all(&logs).unwrap();
    let state = root.join("state.json");
    let holder = holder_config(&root);
    let id = "stopped-run";

    let mut registry = Registry::new(engine(), &state);
    registry
        .spawn(
            shell_spec(
                id,
                "printf 'still running'; IFS= read -r never",
                &logs,
                Some(holder.clone()),
            ),
            record(id),
        )
        .expect("spawn held");
    let mut published = HashMap::new();
    wait_until("the prompt on screen", Duration::from_secs(10), || {
        registry.changed_since(&mut published);
        registry
            .get(id)
            .is_some_and(|session| session.screen_lines().join("\n").contains("still running"))
    });
    let exit = registry
        .terminate(id, Duration::from_millis(500))
        .expect("stop")
        .expect("a live child was stopped");
    assert!(registry.get(id).is_none());
    let mut publications = registry.take_completed_publications();
    assert_eq!(publications.len(), 1, "the stop carried the capture out");
    publications.pop().unwrap().publish().expect("publish");
    registry.persist_for_shutdown().unwrap();
    drop(registry);

    let mut restored = Registry::new(engine(), &state);
    restored.load().unwrap();
    assert!(restored.restore(&holder, &logs).is_empty());
    let handle = restored.completed_run(id).expect("bound stopped run");
    let terminal = handle.load().unwrap().expect("retained");
    match exit {
        diri_engine::Exit::Signal(signal) => assert_eq!(terminal.exit.signal, Some(signal)),
        diri_engine::Exit::Code(code) => assert_eq!(terminal.exit.code, Some(code)),
    }
    assert!(
        terminal
            .screen()
            .unwrap()
            .lines()
            .join("\n")
            .contains("still running")
    );
}

/// A local emulator reset survives Engine replacement: the pump persists a
/// checkpoint at the reset offset, so the replacement Engine seeds from it
/// instead of replaying the pre-reset output through a fresh emulator.
#[test]
fn local_reset_survives_engine_replacement() {
    let root = holders_dir("reset");
    let logs = root.join("logs");
    std::fs::create_dir_all(&logs).unwrap();
    let state = root.join("state.json");
    let holder = holder_config(&root);
    let id = "reset-run";

    let mut registry = Registry::new(engine(), &state);
    registry
        .spawn(
            shell_spec(
                id,
                "printf 'before reset\\n'; IFS= read -r value; printf 'after:%s\\n' \"$value\"; IFS= read -r done",
                &logs,
                Some(holder.clone()),
            ),
            record(id),
        )
        .expect("spawn held");
    let mut published = HashMap::new();
    let screen = |registry: &Registry| registry.get(id).unwrap().screen_lines().join("\n");
    wait_until("pre-reset output", Duration::from_secs(10), || {
        registry.changed_since(&mut published);
        screen(&registry).contains("before reset")
    });
    assert_eq!(registry.get(id).unwrap().reset_generation(), 0);
    registry.get(id).unwrap().reset_terminal().expect("queued");
    wait_until("the reset to apply", Duration::from_secs(5), || {
        registry.get(id).unwrap().reset_generation() == 1
    });
    assert!(
        screen(&registry).trim().is_empty(),
        "the grid is blank after a reset: {:?}",
        screen(&registry)
    );
    let pid = registry.get(id).unwrap().child_pid();
    assert!(pid > 0);

    // The replacement Engine adopts the same Holder and child.
    registry.persist_for_shutdown().unwrap();
    drop(registry);
    let mut restored = Registry::new(engine(), &state);
    restored.load().unwrap();
    assert_eq!(restored.restore(&holder, &logs), vec![id.to_string()]);
    assert_eq!(restored.get(id).unwrap().child_pid(), pid);
    // Give a replaying pump every chance to be wrong before asserting.
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        !screen(&restored).contains("before reset"),
        "pre-reset output came back through replay: {:?}",
        screen(&restored)
    );
    restored.get(id).unwrap().write_input(b"x\n").unwrap();
    wait_until(
        "post-reset output after adoption",
        Duration::from_secs(10),
        || screen(&restored).contains("after:x"),
    );
    assert!(!screen(&restored).contains("before reset"));
    restored.terminate(id, Duration::from_millis(500)).unwrap();
}
