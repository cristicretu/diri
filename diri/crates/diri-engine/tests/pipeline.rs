//! The whole sensing pipeline against a real child process.
//!
//! A shell on a real PTY paints a Claude-style permission prompt; the engine
//! emulates the terminal, evaluates the actual shipped manifest, folds the
//! observation through the reducer, and must conclude the session needs the
//! user. Then the prompt is answered and the session must return to idle.
//!
//! This is the test that says the Rust engine can do the daemon's job. It
//! touches nothing outside its own child process.

#![cfg(unix)]

use std::io::Read;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

use diri_engine::detect::ManifestEngine;
use diri_engine::status::{Authority, StatusReducer, StatusSignal};
use diri_engine::{HeadlessScreen, OutputLog, Pty, PtySpec};
use diri_proto::{NeedsInputKind, SessionStatus};

fn manifest_dir() -> PathBuf {
    diri_engine::detect::bundled_manifest_dir()
        .canonicalize()
        .expect("manifests directory")
}

/// Pumps the PTY into the screen (and the log) until `predicate` holds.
fn pump_until(
    reader: &mut diri_engine::pty::PtyStream,
    screen: &mut HeadlessScreen,
    log: &mut OutputLog,
    within: Duration,
    mut predicate: impl FnMut(&HeadlessScreen) -> bool,
) -> bool {
    let deadline = Instant::now() + within;
    let mut buffer = [0u8; 8192];
    while Instant::now() < deadline {
        // Never block in read: a child that goes quiet must not wedge the test.
        match reader.wait_readable(Duration::from_millis(100)) {
            Ok(false) => continue,
            Ok(true) => {}
            Err(_) => break,
        }
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => {
                log.append(&buffer[..n]).expect("append");
                screen.feed(&buffer[..n]);
                if predicate(screen) {
                    return true;
                }
            }
            Err(_) => break,
        }
    }
    predicate(screen)
}

#[test]
fn a_real_process_painting_a_prompt_is_detected_as_needing_input() {
    let engine = {
        let (engine, failed) = ManifestEngine::load_dir(&manifest_dir()).expect("load manifests");
        assert!(failed.is_empty(), "manifests failed to load: {failed:?}");
        engine
    };

    let temp = tempfile::tempdir().expect("temp dir");
    let mut log = OutputLog::writer(temp.path(), "pipeline").expect("log");
    let mut screen = HeadlessScreen::new(80, 24);
    let started = SystemTime::now();
    let mut reducer = StatusReducer::new(Authority::HooksPrimary, started);

    // A shell that paints a permission prompt, waits for a line, then clears
    // the screen and shows an ordinary prompt again.
    let script = r#"
printf '\033[2J\033[H'
printf 'Bash command\n'
printf 'rm -rf build\n'
printf 'Do you want to proceed?\n'
printf '\342\235\257 1. Yes\n'
printf '  2. No, and tell Claude what to do differently\n'
printf 'esc to cancel\n'
read answer
printf '\033[2J\033[H'
printf 'done: %s\n' "$answer"
sleep 30
"#;
    let spec = PtySpec::new(
        vec!["/bin/sh".into(), "-c".into(), script.into()],
        temp.path(),
    )
    .env("PATH", "/usr/bin:/bin")
    .env("TERM", "xterm-256color")
    .size(80, 24);

    let mut pty = Pty::spawn(&spec).expect("spawn");
    let mut reader = pty.reader().expect("reader");
    let mut writer = pty.writer().expect("writer");

    // 1. The prompt appears and detection reads it as a blocker.
    let painted = pump_until(
        &mut reader,
        &mut screen,
        &mut log,
        Duration::from_secs(15),
        |screen| {
            screen
                .lines()
                .iter()
                .any(|line| line.contains("esc to cancel"))
        },
    );
    assert!(painted, "the child never painted the prompt");

    let observation = engine
        .evaluate(&screen.snapshot(), "claude-code")
        .expect("the shipped claude manifest should match this screen");

    let now = started + Duration::from_secs(5); // past the startup grace
    let outcome = reducer.reduce(StatusSignal::Screen(observation), now);
    assert_eq!(
        outcome.status_change,
        Some(SessionStatus::NeedsInput(NeedsInputKind::Permission)),
        "a painted permission prompt must reach the reducer as needs-input"
    );
    let detail = outcome.needs_input.expect("a needs-input detail");
    assert!(
        detail.options.is_some_and(|options| options.len() >= 2),
        "the numbered options should have been scraped off the screen"
    );

    // 2. Answering it clears the prompt, and the session settles back to idle.
    use std::io::Write;
    writer.write_all(b"1\n").expect("answer the prompt");
    writer.flush().expect("flush");

    let cleared = pump_until(
        &mut reader,
        &mut screen,
        &mut log,
        Duration::from_secs(15),
        |screen| screen.lines().iter().any(|line| line.contains("done:")),
    );
    assert!(cleared, "the child never acknowledged the answer");

    // The blocker needs two consecutive non-blocker frames to release.
    let mut status = reducer.status().clone();
    for step in 1..=4 {
        let snapshot = screen.snapshot();
        let observation = engine.evaluate(&snapshot, "claude-code");
        let signal = match observation {
            Some(observation) => StatusSignal::Screen(observation),
            // Nothing matched: an ordinary screen. Feed an idle observation the
            // way the daemon's scanner would.
            None => StatusSignal::Screen(diri_engine::detect::ScreenObservation {
                state: diri_engine::detect::ManifestState::Idle,
                matched_rule_id: "no-match".into(),
                priority: 0,
                content_seq: snapshot.content_seq + step,
                prompt_excerpt: None,
                options: None,
            }),
        };
        if let Some(change) = reducer
            .reduce(signal, now + Duration::from_millis(100 * step))
            .status_change
        {
            status = change;
        }
    }
    assert_eq!(
        status,
        SessionStatus::Idle,
        "the session should return to idle once the prompt is gone"
    );

    // 3. The log captured the whole session, and the erase is a replay point.
    assert!(log.tail_offset() > 0, "the output log recorded the session");
    assert!(
        !log.sync_points().is_empty(),
        "the screen clears should have registered as replay sync points"
    );

    pty.terminate(Duration::from_secs(3)).expect("terminate");
}

#[test]
fn a_process_only_agent_reports_working_then_its_exit_code() {
    let temp = tempfile::tempdir().expect("temp dir");
    let mut log = OutputLog::writer(temp.path(), "generic").expect("log");
    let mut screen = HeadlessScreen::new(80, 24);
    let started = SystemTime::now();
    let mut reducer = StatusReducer::new(Authority::ProcessOnly, started);

    let spec = PtySpec::new(
        vec![
            "/bin/sh".into(),
            "-c".into(),
            "printf 'building\\n'; exit 3".into(),
        ],
        temp.path(),
    )
    .env("PATH", "/usr/bin:/bin");

    let mut pty = Pty::spawn(&spec).expect("spawn");
    let mut reader = pty.reader().expect("reader");

    let painted = pump_until(
        &mut reader,
        &mut screen,
        &mut log,
        Duration::from_secs(10),
        |screen| screen.lines().iter().any(|line| line.contains("building")),
    );
    assert!(painted, "no output from the child");

    let outcome = reducer.reduce(StatusSignal::PtyOutputActivity, SystemTime::now());
    assert_eq!(outcome.status_change, Some(SessionStatus::Working));

    let exit = pty.wait().expect("wait");
    let outcome = reducer.reduce(
        StatusSignal::ProcessExit {
            code: match exit {
                diri_engine::Exit::Code(code) => Some(code),
                diri_engine::Exit::Signal(_) => None,
            },
            signal: match exit {
                diri_engine::Exit::Signal(signal) => Some(signal),
                diri_engine::Exit::Code(_) => None,
            },
        },
        SystemTime::now(),
    );

    match outcome.status_change {
        Some(SessionStatus::Exited(info)) => assert_eq!(info.code, Some(3)),
        other => panic!("expected exit 3, got {other:?}"),
    }
}
