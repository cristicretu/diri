//! Per-session file lifecycle: closing a session deletes every sidecar it
//! wrote, and the startup sweep reclaims orphans without touching referenced,
//! recent, foreign, or symlinked entries.

#![cfg(unix)]

use std::collections::HashSet;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use diri_engine::session::HolderConfig;
use diri_engine::session_files::{LOG_SUFFIXES, SweepOptions, startup_sweep, sweep_orphans};
use diri_engine::{ManifestEngine, Registry};

const LIVE: &str = "s_00000000000a";
const ORPHAN: &str = "s_0000000000bb";
const FRESH_ORPHAN: &str = "s_0000000000cc";

fn engine() -> Arc<ManifestEngine> {
    let dir = diri_engine::detect::bundled_manifest_dir()
        .canonicalize()
        .expect("manifests");
    let (engine, _) = ManifestEngine::load_dir(&dir).expect("load");
    Arc::new(engine)
}

/// Short root so holder sockets stay under `sun_path`.
fn short_root(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("diri-sf-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create dir");
    dir
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

fn write_old(path: &Path, bytes: usize) {
    std::fs::write(path, vec![7u8; bytes]).expect("write");
    let old = SystemTime::now() - Duration::from_secs(24 * 3600);
    File::options()
        .write(true)
        .open(path)
        .and_then(|file| file.set_modified(old))
        .expect("backdate");
}

fn backdate_dir(path: &Path) {
    let old = SystemTime::now() - Duration::from_secs(24 * 3600);
    File::open(path)
        .and_then(|dir| dir.set_modified(old))
        .expect("backdate dir");
}

fn names(dir: &Path) -> HashSet<String> {
    std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default()
}

/// Every kind of orphan goes; referenced, recent, foreign, symlinked, and
/// directory entries stay, and a symlink's target is never touched.
#[test]
fn sweep_removes_every_orphan_kind_and_nothing_else() {
    let temp = tempfile::tempdir().expect("temp");
    let logs = temp.path().join("logs");
    let recovery = temp.path().join("sessions");
    std::fs::create_dir_all(&logs).unwrap();
    std::fs::create_dir_all(&recovery).unwrap();

    for suffix in LOG_SUFFIXES {
        write_old(&logs.join(format!("{LIVE}{suffix}")), 10);
        write_old(&logs.join(format!("{ORPHAN}{suffix}")), 100);
    }
    std::fs::write(logs.join(format!("{FRESH_ORPHAN}.bin")), b"spawning").unwrap();
    for foreign in [
        "dirijord-rs.boot.log",
        "s_0000000000bb.bin.bak",
        "s_short.bin",
        "notes.screen.plist",
    ] {
        write_old(&logs.join(foreign), 5);
    }
    std::fs::create_dir(logs.join("test-artifacts")).unwrap();
    std::fs::create_dir(logs.join("s_0000000000dd.bin")).unwrap();
    // An orphan-named symlink to a file outside the logs directory.
    let outside = temp.path().join("precious.bin");
    write_old(&outside, 42);
    std::os::unix::fs::symlink(&outside, logs.join("s_0000000000ee.bin")).unwrap();

    // Recovery dirs: an orphan with provider storage beside the owned files,
    // an orphan with only owned files, and a referenced one.
    let orphan_dir = recovery.join(ORPHAN);
    std::fs::create_dir_all(orphan_dir.join("provider")).unwrap();
    write_old(&orphan_dir.join("recovery.json"), 3);
    write_old(&orphan_dir.join("last-activity.json"), 3);
    backdate_dir(&orphan_dir);
    let bare_dir = recovery.join("s_0000000000ff");
    std::fs::create_dir_all(&bare_dir).unwrap();
    write_old(&bare_dir.join("completed-run.json"), 3);
    backdate_dir(&bare_dir);
    let live_dir = recovery.join(LIVE);
    std::fs::create_dir_all(&live_dir).unwrap();
    write_old(&live_dir.join("recovery.json"), 3);
    backdate_dir(&live_dir);

    let referenced: HashSet<String> = [LIVE.to_owned()].into();

    let dry = sweep_orphans(
        &logs,
        Some(&recovery),
        &referenced,
        &SweepOptions {
            dry_run: true,
            ..SweepOptions::default()
        },
    );
    assert_eq!(dry.removed_files, LOG_SUFFIXES.len());
    assert_eq!(dry.removed_bytes, 100 * LOG_SUFFIXES.len() as u64);
    assert_eq!(dry.removed_recovery_dirs, 2);
    assert!(
        logs.join(format!("{ORPHAN}.bin")).exists(),
        "a dry run deletes nothing"
    );

    let before = names(&logs);
    let report = sweep_orphans(
        &logs,
        Some(&recovery),
        &referenced,
        &SweepOptions::default(),
    );
    assert_eq!(report.removed_files, LOG_SUFFIXES.len());
    assert_eq!(report.removed_bytes, 100 * LOG_SUFFIXES.len() as u64);
    assert_eq!(report.removed_recovery_dirs, 2);
    assert_eq!(report.kept_recent, 1);
    assert_eq!(report.failed, 0);
    assert!(!report.truncated);

    let after = names(&logs);
    let removed: HashSet<String> = before.difference(&after).cloned().collect();
    let expected: HashSet<String> = LOG_SUFFIXES
        .iter()
        .map(|suffix| format!("{ORPHAN}{suffix}"))
        .collect();
    assert_eq!(removed, expected, "only the orphan's files are removed");
    for suffix in LOG_SUFFIXES {
        assert!(logs.join(format!("{LIVE}{suffix}")).exists());
    }
    assert!(logs.join(format!("{FRESH_ORPHAN}.bin")).exists());
    assert!(logs.join("s_0000000000dd.bin").is_dir());
    assert!(
        logs.join("s_0000000000ee.bin")
            .symlink_metadata()
            .is_ok_and(|metadata| metadata.file_type().is_symlink())
    );
    assert_eq!(std::fs::read(&outside).unwrap().len(), 42);

    assert!(
        orphan_dir.join("provider").is_dir(),
        "provider storage stays"
    );
    assert!(!orphan_dir.join("recovery.json").exists());
    assert!(!orphan_dir.join("last-activity.json").exists());
    assert!(!bare_dir.exists(), "an emptied recovery dir is removed");
    assert!(live_dir.join("recovery.json").exists());
}

#[test]
fn the_sweep_stops_at_its_entry_budget() {
    let temp = tempfile::tempdir().expect("temp");
    for index in 0..10 {
        write_old(&temp.path().join(format!("s_{index:012x}.bin")), 1);
    }
    let report = sweep_orphans(
        temp.path(),
        None,
        &HashSet::new(),
        &SweepOptions {
            max_entries: 4,
            ..SweepOptions::default()
        },
    );
    assert!(report.truncated);
    assert_eq!(report.removed_files, 4);
    assert_eq!(names(temp.path()).len(), 6);
}

/// The startup gate: a failed load or a record-less Registry sweeps nothing,
/// and live holders and remote bindings protect their sessions' files.
#[test]
fn startup_sweep_trusts_only_a_loaded_nonempty_registry() {
    let temp = tempfile::tempdir().expect("temp");
    let logs = temp.path().join("logs");
    let holders = temp.path().join("holders");
    let bindings = temp.path().join("remote-bindings");
    for dir in [&logs, &holders, &bindings] {
        std::fs::create_dir_all(dir).unwrap();
    }
    let held = "s_0000000000a1";
    let bound = "s_0000000000b2";
    for id in [LIVE, ORPHAN, held, bound] {
        write_old(&logs.join(format!("{id}.bin")), 8);
        write_old(&logs.join(format!("{id}.screen.plist")), 8);
    }
    let holders_dir = diri_engine::holder::paths::HolderPaths::new(&holders, held)
        .directory
        .clone();
    std::fs::create_dir_all(&holders_dir).unwrap();
    std::fs::write(holders_dir.join(format!("{held}.sock")), b"").unwrap();
    std::fs::write(bindings.join(format!("{bound}.json")), b"{").unwrap();

    let registry = Mutex::new(Registry::new(engine(), temp.path().join("state.json")));
    let options = SweepOptions::default();
    assert_eq!(
        startup_sweep(&registry, true, &logs, &holders, &bindings, &options),
        None,
        "no records: nothing to judge orphans against"
    );
    registry.lock().unwrap().insert_record(record(LIVE));
    assert_eq!(
        startup_sweep(&registry, false, &logs, &holders, &bindings, &options),
        None,
        "a failed state load must not sweep"
    );
    assert_eq!(names(&logs).len(), 8);

    let report =
        startup_sweep(&registry, true, &logs, &holders, &bindings, &options).expect("swept");
    assert_eq!(report.removed_files, 2);
    let left = names(&logs);
    assert!(!left.contains(&format!("{ORPHAN}.bin")));
    assert!(!left.contains(&format!("{ORPHAN}.screen.plist")));
    for id in [LIVE, held, bound] {
        assert!(left.contains(&format!("{id}.bin")), "{id} kept");
        assert!(left.contains(&format!("{id}.screen.plist")), "{id} kept");
    }
}

fn wait_until(what: &str, timeout: Duration, mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if check() {
            return;
        }
        std::thread::sleep(Duration::from_millis(30));
    }
    panic!("timed out waiting for {what}");
}

/// Closing a held session deletes its log, screen checkpoint, and attention
/// store — before this change only the `.bin` went, and the other two leaked
/// for every closed tab.
#[test]
fn removing_a_held_session_deletes_every_sidecar() {
    let root = short_root("rm");
    let logs = root.join("logs");
    let holder = HolderConfig {
        holders_dir: root.join("holders"),
        executable: PathBuf::from(env!("CARGO_BIN_EXE_diri-holder")),
    };
    let id = "s_0000000000c1";
    let mut registry = Registry::new(engine(), root.join("state.json"));
    registry
        .spawn(
            diri_engine::session::SessionSpec {
                id: id.into(),
                pty: diri_engine::PtySpec::new(
                    vec!["/bin/sh".into(), "-c".into(), "cat".into()],
                    "/tmp",
                )
                .env("PATH", "/usr/bin:/bin")
                .env("TERM", "xterm-256color")
                .size(80, 24),
                manifest_id: "shell".into(),
                authority: diri_engine::Authority::ProcessOnly,
                logs_dir: logs.clone(),
                holder: Some(holder),
                remote: None,
                defer_launch: false,
            },
            record(id),
        )
        .expect("spawn");
    registry
        .get(id)
        .expect("session")
        .write_input(b"sidecars\n")
        .expect("write");
    for suffix in [".bin", ".screen.plist", ".attention.sqlite"] {
        let path = logs.join(format!("{id}{suffix}"));
        wait_until(
            &format!("{suffix} to exist"),
            Duration::from_secs(10),
            || path.exists(),
        );
    }

    registry.remove(id, &logs).expect("remove");
    // A late checkpoint write from the stopping pump would recreate a file.
    std::thread::sleep(Duration::from_millis(800));
    let left: Vec<String> = names(&logs)
        .into_iter()
        .filter(|name| name.starts_with(id))
        .collect();
    assert!(left.is_empty(), "sidecars left behind: {left:?}");
    let _ = std::fs::remove_dir_all(&root);
}
