//! Screen checkpoints across a daemon restart: the held pump writes
//! `<id>.screen.plist` once output settles, and an adopting daemon seeds its
//! emulator from that file instead of replaying the raw log tail.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use diri_engine::checkpoint::ScreenCheckpoint;
use diri_engine::session::HolderConfig;
use diri_engine::{ManifestEngine, OutputLog, Registry};
use diri_proto::grid::{ChangedRow, GridCell, GridUpdate, TermColor, TermStyle};

fn engine() -> Arc<ManifestEngine> {
    let dir = diri_engine::detect::bundled_manifest_dir()
        .canonicalize()
        .expect("manifests");
    let (engine, _) = ManifestEngine::load_dir(&dir).expect("load");
    Arc::new(engine)
}

fn holders_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("diri-ckpt-{tag}-{}", std::process::id()));
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

fn shell_spec(
    id: &str,
    script: &str,
    logs: &Path,
    holder: Option<HolderConfig>,
) -> diri_engine::session::SessionSpec {
    diri_engine::session::SessionSpec {
        id: id.into(),
        pty: diri_engine::PtySpec::new(vec!["/bin/sh".into(), "-c".into(), script.into()], "/tmp")
            .env("PATH", "/usr/bin:/bin")
            .env("TERM", "xterm-256color")
            .size(80, 24),
        manifest_id: "shell".into(),
        authority: diri_engine::Authority::ProcessOnly,
        logs_dir: logs.to_path_buf(),
        holder,
        remote: None,
        defer_launch: false,
    }
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
        agent_session_id: None,
        transcript_path: None,
        status: SessionStatus::Starting,
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

fn checkpoint_path(logs: &Path, id: &str) -> PathBuf {
    logs.join(format!("{id}.screen.plist"))
}

fn log_tail(logs: &Path, id: &str) -> u64 {
    let mut log = OutputLog::reader(logs, id).expect("log");
    log.refresh_from_disk();
    log.tail_offset()
}

/// A full-snapshot 80×24 grid whose first row spells `text` — content that
/// exists nowhere in any log, so seeing it on screen proves the checkpoint
/// (not a raw-tail replay) painted the emulator.
fn synthetic_grid(text: &str) -> GridUpdate {
    let mut rows = Vec::with_capacity(24);
    for y in 0..24u16 {
        let mut cells = vec![GridCell::BLANK; 80];
        if y == 0 {
            for (x, ch) in text.chars().take(80).enumerate() {
                cells[x] = GridCell::new(
                    ch as u32,
                    TermColor::Default,
                    TermColor::DefaultInverted,
                    TermStyle::empty(),
                );
            }
        }
        rows.push(ChangedRow::new(y, cells));
    }
    GridUpdate {
        cols: 80,
        rows: 24,
        cursor_col: 0,
        cursor_row: 1,
        cursor_visible: true,
        is_full_snapshot: true,
        changed_rows: rows,
    }
}

/// The write side: a held session's pump checkpoints the settled screen, and
/// the file both parses and agrees with the log tail.
#[test]
fn the_held_pump_writes_a_checkpoint_once_output_settles() {
    let root = holders_dir("write");
    let logs = root.join("logs");
    let holder = holder_config(&root);

    let mut registry = Registry::new(engine(), root.join("state.json"));
    registry
        .spawn(
            shell_spec("s_ck", "cat", &logs, Some(holder.clone())),
            record("s_ck"),
        )
        .expect("spawn");
    registry
        .get("s_ck")
        .expect("session")
        .write_input(b"checkpoint-me\n")
        .expect("write");

    let path = checkpoint_path(&logs, "s_ck");
    wait_until("a checkpoint to appear", Duration::from_secs(10), || {
        ScreenCheckpoint::load(&path).is_some()
    });
    // The settle delay means the write happens after output went quiet, so
    // by load time the offset covers everything the child said.
    wait_until(
        "the checkpoint to cover the tail",
        Duration::from_secs(10),
        || {
            ScreenCheckpoint::load(&path)
                .is_some_and(|checkpoint| checkpoint.log_offset == log_tail(&logs, "s_ck"))
        },
    );
    let checkpoint = ScreenCheckpoint::load(&path).expect("checkpoint");
    let text = checkpoint
        .grid
        .changed_rows
        .iter()
        .map(|row| {
            row.cells
                .iter()
                .map(|cell| char::from_u32(cell.scalar).unwrap_or(' '))
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        text.contains("checkpoint-me"),
        "the settled screen is in the checkpoint: {text:?}"
    );

    registry
        .terminate("s_ck", Duration::from_secs(2))
        .expect("terminate");
}

/// The read side, proven by absence: an adopted session whose checkpoint
/// carries content the log never contained must show that content — a raw
/// tail replay could not have painted it.
#[test]
fn adoption_seeds_from_the_checkpoint_not_the_raw_tail() {
    let root = holders_dir("adopt");
    let logs = root.join("logs");
    let holder = holder_config(&root);
    let state_file = root.join("state.json");

    // Life #1: a held cat says something, then the daemon "crashes".
    {
        let mut registry = Registry::new(engine(), &state_file);
        registry
            .spawn(
                shell_spec("s_ad", "cat", &logs, Some(holder.clone())),
                record("s_ad"),
            )
            .expect("spawn");
        registry
            .get("s_ad")
            .expect("session")
            .write_input(b"hello-from-the-log\n")
            .expect("write");
        wait_until("the echo to land", Duration::from_secs(5), || {
            log_tail(&logs, "s_ad") > 0
        });
        registry.persist().expect("persist");
    }

    // Between lives: plant a checkpoint at the exact tail whose content the
    // log has never seen.
    ScreenCheckpoint {
        log_offset: log_tail(&logs, "s_ad"),
        history: Vec::new(),
        grid: synthetic_grid("PAINTED-FROM-CHECKPOINT"),
        marker_buffer: Vec::new(),
        alt_screen: false,
        bracketed_paste: false,
        mouse_reporting: false,
    }
    .write_atomically(&checkpoint_path(&logs, "s_ad"))
    .expect("plant checkpoint");

    // Life #2 adopts; the screen must be the checkpoint's, not the log's.
    let mut registry = Registry::new(engine(), &state_file);
    registry.load().expect("load");
    assert_eq!(registry.restore(&holder, &logs), vec!["s_ad".to_string()]);
    wait_until("the checkpoint to paint", Duration::from_secs(5), || {
        registry
            .get("s_ad")
            .expect("adopted")
            .screen_lines()
            .join("\n")
            .contains("PAINTED-FROM-CHECKPOINT")
    });
    let screen = registry
        .get("s_ad")
        .expect("adopted")
        .screen_lines()
        .join("\n");
    assert!(
        !screen.contains("hello-from-the-log"),
        "replay from the checkpoint offset must not re-feed old bytes: {screen:?}"
    );

    registry
        .terminate("s_ad", Duration::from_secs(2))
        .expect("terminate");
}

/// An unusable checkpoint (wrong geometry here) is a cache miss: adoption
/// falls back to the bounded raw-tail replay and still shows the log.
#[test]
fn a_stale_checkpoint_falls_back_to_tail_replay() {
    let root = holders_dir("fallback");
    let logs = root.join("logs");
    let holder = holder_config(&root);
    let state_file = root.join("state.json");

    {
        let mut registry = Registry::new(engine(), &state_file);
        registry
            .spawn(
                shell_spec("s_fb", "cat", &logs, Some(holder.clone())),
                record("s_fb"),
            )
            .expect("spawn");
        registry
            .get("s_fb")
            .expect("session")
            .write_input(b"the-log-is-truth\n")
            .expect("write");
        wait_until("the echo to land", Duration::from_secs(5), || {
            log_tail(&logs, "s_fb") > 0
        });
        registry.persist().expect("persist");
    }

    // A checkpoint from some other geometry: restore must refuse it.
    let mut grid = synthetic_grid("GEOMETRY-MISMATCH");
    grid.cols = 40;
    for row in &mut grid.changed_rows {
        row.cells.truncate(40);
    }
    ScreenCheckpoint {
        log_offset: log_tail(&logs, "s_fb"),
        history: Vec::new(),
        grid,
        marker_buffer: Vec::new(),
        alt_screen: false,
        bracketed_paste: false,
        mouse_reporting: false,
    }
    .write_atomically(&checkpoint_path(&logs, "s_fb"))
    .expect("plant checkpoint");

    let mut registry = Registry::new(engine(), &state_file);
    registry.load().expect("load");
    assert_eq!(registry.restore(&holder, &logs), vec!["s_fb".to_string()]);
    wait_until("the tail replay to paint", Duration::from_secs(5), || {
        registry
            .get("s_fb")
            .expect("adopted")
            .screen_lines()
            .join("\n")
            .contains("the-log-is-truth")
    });
    let screen = registry
        .get("s_fb")
        .expect("adopted")
        .screen_lines()
        .join("\n");
    assert!(
        !screen.contains("GEOMETRY-MISMATCH"),
        "a refused checkpoint must not leak onto the screen: {screen:?}"
    );

    registry
        .terminate("s_fb", Duration::from_secs(2))
        .expect("terminate");
}
