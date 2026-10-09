//! The real Claude Code CLI reporting its status with `OSC 7501` through a
//! real Engine session. Opt-in: it needs a signed-in `claude` (2.1.295 or
//! later) and spends two tiny Haiku turns.
//!
//! ```sh
//! DIRI_REAL_CLAUDE=1 [DIRI_REAL_CLAUDE_CWD=<trusted dir>] [DIRI_REAL_CLAUDE_BIN=<path>] \
//!   cargo test -p diri-engine --test program_status_claude -- --ignored --nocapture
//! ```
//!
//! No hooks are installed, so status can only come from the screen and the
//! program's own reports; the test requires the latter. It leaves one short
//! conversation in the trusted directory's Claude history and removes
//! nothing else.

#![cfg(unix)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use diri_engine::session::{HolderConfig, Session, SessionSpec};
use diri_engine::{Authority, ManifestEngine, PtySpec};
use diri_proto::{NeedsInputKind, SessionStatus, StatusEvidenceSource};

fn engine() -> Arc<ManifestEngine> {
    let dir = diri_engine::detect::bundled_manifest_dir()
        .canonicalize()
        .expect("manifests");
    let (engine, _) = ManifestEngine::load_dir(&dir).expect("load");
    Arc::new(engine)
}

type Point = (SessionStatus, Option<StatusEvidenceSource>, Option<String>);

fn point(session: &Session) -> Point {
    let view = session.view();
    (
        view.status,
        view.status_evidence.map(|evidence| evidence.source),
        view.needs_input.map(|detail| detail.summary),
    )
}

/// Waits for `check`, printing every change on the way.
fn wait_for(
    session: &Session,
    started: Instant,
    last: &mut Option<Point>,
    what: &str,
    seconds: u64,
    check: impl Fn(&Point) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    loop {
        let now = point(session);
        if last.as_ref() != Some(&now) {
            println!("{:>6.2}s  {:?}", started.elapsed().as_secs_f32(), now);
            *last = Some(now.clone());
        }
        if check(&now) {
            return;
        }
        assert!(
            !matches!(now.0, SessionStatus::Exited(_)),
            "claude exited waiting for {what}; screen: {:#?}",
            session.screen_lines()
        );
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what}; screen: {:#?}",
            session.screen_lines()
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn reported(point: &Point) -> bool {
    point.1 == Some(StatusEvidenceSource::ProgramStatus)
}

#[test]
#[ignore = "needs a signed-in claude; set DIRI_REAL_CLAUDE=1"]
fn real_claude_code_reports_its_status_directly() {
    real_claude_code_reports_its_status("direct", false);
}

#[test]
#[ignore = "needs a signed-in claude; set DIRI_REAL_CLAUDE=1"]
fn real_claude_code_reports_its_status_through_a_holder() {
    real_claude_code_reports_its_status("held", true);
}

fn real_claude_code_reports_its_status(tag: &str, held: bool) {
    if std::env::var_os("DIRI_REAL_CLAUDE").is_none() {
        return;
    }
    let cwd = std::env::var_os("DIRI_REAL_CLAUDE_CWD")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap());
    let claude = std::env::var("DIRI_REAL_CLAUDE_BIN").unwrap_or_else(|_| {
        let home = std::env::var("HOME").expect("HOME");
        format!("{home}/.local/bin/claude")
    });
    let logs = std::env::temp_dir().join(format!("diri-real-claude-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&logs);
    let spec = SessionSpec {
        id: format!("real_claude_osc7501_{tag}"),
        // Not a child of whatever Claude runs this test.
        pty: PtySpec::new(
            vec![
                "/usr/bin/env".into(),
                "-u".into(),
                "CLAUDECODE".into(),
                "-u".into(),
                "CLAUDE_CODE_ENTRYPOINT".into(),
                "-u".into(),
                "CLAUDE_CODE_CHILD_SESSION".into(),
                claude,
                "--model".into(),
                "haiku".into(),
                // Ask before running a command, whatever the project sets.
                "--permission-mode".into(),
                "manual".into(),
            ],
            &cwd,
        )
        .env("TERM", "xterm-256color")
        .env("PATH", &std::env::var("PATH").unwrap_or_default())
        // Where the sign-in lives: the config directory and the keychain.
        .env("HOME", &std::env::var("HOME").unwrap_or_default())
        .env("USER", &std::env::var("USER").unwrap_or_default())
        .env("LOGNAME", &std::env::var("LOGNAME").unwrap_or_default())
        .size(120, 40),
        manifest_id: "claude-code".into(),
        authority: Authority::HooksPrimary,
        logs_dir: logs.join("logs"),
        holder: held.then(|| HolderConfig {
            holders_dir: logs.join("holders"),
            executable: PathBuf::from(env!("CARGO_BIN_EXE_diri-holder")),
        }),
        remote: None,
        defer_launch: false,
    };
    let mut session = Session::spawn(spec, engine()).expect("spawn");
    let started = Instant::now();
    let mut last = None;

    wait_for(&session, started, &mut last, "a first report", 30, reported);

    session
        .send_text("Reply with exactly the word: ok", true)
        .unwrap();
    wait_for(&session, started, &mut last, "reported work", 30, |p| {
        reported(p) && p.0 == SessionStatus::Working
    });
    wait_for(
        &session,
        started,
        &mut last,
        "the reported result",
        90,
        |p| reported(p) && p.0 == SessionStatus::Idle,
    );
    assert!(
        session.view().last_turn_completed_at.is_some(),
        "the turn completed"
    );

    let probe = std::env::temp_dir().join(format!("diri-7501-probe-{tag}-{}", std::process::id()));
    session
        .send_text(
            &format!(
                "Use the Bash tool to run exactly: touch {}",
                probe.display()
            ),
            true,
        )
        .unwrap();
    wait_for(
        &session,
        started,
        &mut last,
        "a reported permission",
        90,
        |p| reported(p) && p.0 == SessionStatus::NeedsInput(NeedsInputKind::Permission),
    );
    let detail = session.view().needs_input.expect("detail");
    println!(
        "needs input: {:?} / {:?} / options {:?}",
        detail.source, detail.summary, detail.options
    );

    // Deny it.
    session.write_input(b"\x1b").unwrap();
    wait_for(
        &session,
        started,
        &mut last,
        "back from the request",
        60,
        |p| reported(p) && matches!(p.0, SessionStatus::Idle | SessionStatus::Working),
    );
    wait_for(&session, started, &mut last, "idle again", 60, |p| {
        reported(p) && p.0 == SessionStatus::Idle
    });
    assert!(!probe.exists(), "the denied command did not run");

    let _ = session.terminate(Duration::from_secs(3));

    // Every report the program sent, as it sent them.
    for entry in walk(&logs) {
        let bytes = std::fs::read(&entry).unwrap_or_default();
        let text = String::from_utf8_lossy(&bytes);
        for report in text.split("\x1b]7501;").skip(1) {
            let end = report.find(['\x07', '\x1b']).unwrap_or(report.len());
            println!("OSC 7501;{}", &report[..end]);
        }
    }
    let _ = std::fs::remove_dir_all(&logs);
}

fn walk(dir: &std::path::Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_dir() {
            files.extend(walk(&path));
        } else {
            files.push(path);
        }
    }
    files
}
