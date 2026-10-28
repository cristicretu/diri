//! A shell asking for a line itself, with no job in the foreground: fish's
//! `read` builtin. fish marks that prompt with `OSC 133;A` inside the command
//! line's `C`…`D`, which is the only thing that tells it from fish's own idle
//! prompt. An idle prompt, in any shell Diri integrates, is never flagged.
//!
//! Real shells on real PTYs. fish is skipped where it is not installed.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use diri_engine::session::{HolderConfig, Session, SessionSpec};
use diri_engine::shell_integration::ShellIntegration;
use diri_engine::{Authority, ManifestEngine, PtySpec};
use diri_proto::{NeedsInputKind, NeedsInputSource, SessionStatus};

fn engine() -> Arc<ManifestEngine> {
    let dir = diri_engine::detect::bundled_manifest_dir()
        .canonicalize()
        .expect("manifests");
    let (engine, _) = ManifestEngine::load_dir(&dir).expect("load");
    Arc::new(engine)
}

fn root(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("diri-builtin-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("home")).expect("create dir");
    dir.canonicalize().expect("canonical")
}

fn which(program: &str) -> Option<PathBuf> {
    ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin", "/bin"]
        .iter()
        .map(|dir| Path::new(dir).join(program))
        .find(|path| path.is_file())
}

fn spawn(
    id: &str,
    root: &Path,
    argv: Vec<String>,
    holder: Option<HolderConfig>,
    integrate: bool,
) -> Session {
    let home = root.join("home");
    let mut pty = PtySpec::new(argv, root)
        .env("HOME", home.to_str().unwrap())
        .env("XDG_CONFIG_HOME", home.join(".config").to_str().unwrap())
        .env("PATH", "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin")
        .env("TERM", "xterm-256color")
        .env("LANG", "en_US.UTF-8")
        .size(100, 30);
    if integrate {
        ShellIntegration::install(&root.join("inject"))
            .expect("integration")
            .apply(&mut pty);
    }
    let spec = SessionSpec {
        id: id.into(),
        pty,
        manifest_id: "shell".into(),
        authority: Authority::ProcessOnly,
        logs_dir: root.join("logs"),
        holder,
        remote: None,
        defer_launch: false,
    };
    Session::spawn(spec, engine()).expect("spawn")
}

fn wait_until(session: &Session, what: &str, mut check: impl FnMut() -> bool) -> Duration {
    let started = Instant::now();
    while !check() {
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "timed out waiting for {what}: {:?} {:?}",
            session.view().status,
            session.screen_lines()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    started.elapsed()
}

fn needs_input(session: &Session) -> bool {
    matches!(session.status(), SessionStatus::NeedsInput(_))
}

/// Samples the session every 10 ms for `span` and counts the samples that
/// read as a question.
fn flagged_samples(session: &Session, span: Duration) -> (usize, usize) {
    let until = Instant::now() + span;
    let (mut flagged, mut total) = (0, 0);
    while Instant::now() < until {
        total += 1;
        if needs_input(session) {
            flagged += 1;
            eprintln!(
                "flagged: {:?} {:?}",
                session.view().needs_input,
                session.screen_lines().last()
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    (flagged, total)
}

fn idle(session: &Session) -> bool {
    session.status() == SessionStatus::Idle
}

fn fish_read(session: &mut Session, label: &str) {
    wait_until(session, "fish's prompt", || {
        idle(session) && session.screen_lines().iter().any(|line| line.contains('>'))
    });
    std::thread::sleep(Duration::from_millis(300));

    session
        .write_input(b"read -P 'Proceed? [y/N] ' answer\r")
        .unwrap();
    let latency = wait_until(session, "the builtin's question", || needs_input(session));
    eprintln!("{label}: fish read flagged after {latency:?}");
    let view = session.view();
    assert_eq!(
        view.status,
        SessionStatus::NeedsInput(NeedsInputKind::Question)
    );
    let detail = view.needs_input.expect("detail");
    assert_eq!(detail.source, NeedsInputSource::TerminalLine);
    assert_eq!(detail.summary, "Proceed? [y/N]");

    // Answered: fish ends the command line and is at its prompt again.
    session.write_input(b"y\r").unwrap();
    wait_until(session, "the answer to clear it", || idle(session));
    let (flagged, _) = flagged_samples(session, Duration::from_millis(1_500));
    assert_eq!(flagged, 0);

    // ^C at the question ends it too.
    session.write_input(b"read -P 'Again? ' answer\r").unwrap();
    wait_until(session, "the second question", || needs_input(session));
    session.write_input(b"\x03").unwrap();
    wait_until(session, "^C to clear it", || idle(session));

    // A command that runs on after its question was answered is work, not
    // a second question.
    session
        .write_input(b"read -P 'Go? ' answer; sleep 2\r")
        .unwrap();
    wait_until(session, "the third question", || needs_input(session));
    session.write_input(b"y\r").unwrap();
    wait_until(session, "the sleep", || {
        session.status() == SessionStatus::Working
    });
    let (flagged, _) = flagged_samples(session, Duration::from_millis(1_500));
    assert_eq!(flagged, 0, "sleep after an answered read");
    wait_until(session, "the prompt", || idle(session));
}

#[test]
fn fish_read_builtin_needs_the_user_in_a_direct_terminal() {
    let Some(fish) = which("fish") else {
        eprintln!("fish is not installed; skipped");
        return;
    };
    let root = root("fish-direct");
    let mut session = spawn(
        "fish_read_direct",
        &root,
        vec![fish.to_string_lossy().into_owned(), "-i".into()],
        None,
        false,
    );
    fish_read(&mut session, "direct");
    let _ = session.terminate(Duration::from_secs(2));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn fish_read_builtin_needs_the_user_in_a_held_terminal() {
    let Some(fish) = which("fish") else {
        eprintln!("fish is not installed; skipped");
        return;
    };
    let root = root("fish-held");
    let holder = HolderConfig {
        holders_dir: root.join("holders"),
        executable: PathBuf::from(env!("CARGO_BIN_EXE_diri-holder")),
    };
    let mut session = spawn(
        "fish_read_held",
        &root,
        vec![fish.to_string_lossy().into_owned(), "-i".into()],
        Some(holder),
        false,
    );
    fish_read(&mut session, "held");
    let _ = session.terminate(Duration::from_secs(2));
    let _ = std::fs::remove_dir_all(&root);
}

/// The false-positive measurement: each shell idle at its prompt, then
/// through ordinary commands, sampled every 10 ms. Not one sample may read as
/// a question. Prints the sample counts for the record.
#[test]
fn idle_and_busy_shells_are_never_flagged() {
    let mut shells: Vec<(&str, Vec<String>, bool)> = vec![
        ("zsh", vec!["/bin/zsh".into(), "-l".into()], true),
        ("bash", vec!["/bin/bash".into(), "-l".into()], true),
    ];
    if let Some(fish) = which("fish") {
        shells.push((
            "fish",
            vec![fish.to_string_lossy().into_owned(), "-i".into()],
            false,
        ));
    }
    for (name, argv, integrate) in shells {
        if !Path::new(&argv[0]).is_file() {
            continue;
        }
        let root = root(name);
        let mut session = spawn(&format!("fp_{name}"), &root, argv, None, integrate);
        wait_until(&session, "the prompt", || {
            idle(&session)
                && !session
                    .screen_lines()
                    .iter()
                    .all(|line| line.trim().is_empty())
        });
        let mut flagged = 0;
        let mut total = 0;
        let (f, t) = flagged_samples(&session, Duration::from_secs(3));
        flagged += f;
        total += t;
        for command in [
            "true",
            "cd /tmp",
            "echo hello",
            "sleep 1",
            "for i in 1 2 3; echo $i; end",
            "ls >/dev/null",
        ] {
            let command = if name != "fish" && command.starts_with("for") {
                "for i in 1 2 3; do echo $i; done"
            } else {
                command
            };
            session.write_input(command.as_bytes()).unwrap();
            session.write_input(b"\r").unwrap();
            let (f, t) = flagged_samples(&session, Duration::from_millis(1_500));
            flagged += f;
            total += t;
        }
        // A shell that marked the command, replaced by fish.
        if name == "zsh"
            && let Some(fish) = which("fish")
        {
            session
                .write_input(format!("exec {} -i\r", fish.display()).as_bytes())
                .unwrap();
            let (f, t) = flagged_samples(&session, Duration::from_secs(3));
            flagged += f;
            total += t;
        }
        eprintln!("{name}: {flagged} of {total} samples flagged");
        assert_eq!(flagged, 0, "{name} flagged an idle or busy prompt");
        let _ = session.terminate(Duration::from_secs(2));
        let _ = std::fs::remove_dir_all(&root);
    }
}
