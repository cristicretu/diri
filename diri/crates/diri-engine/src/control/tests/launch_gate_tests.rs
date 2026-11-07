//! Every Agent launch holds the account gate, and relaunches evict only dead runs.
use super::*;

/// Calls that start or replace an Agent process.
const LAUNCHES: &[&str] = &[
    "Session::spawn(",
    ".spawn(spec",
    ".spawn(crate::session::SessionSpec",
    ".spawn(SessionSpec",
    ".respawn(",
];

/// Non-test source of `control.rs` and `control/*.rs`, as (file, fn, body).
fn control_functions() -> Vec<(String, String, String)> {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = vec![src.join("control.rs")];
    let mut nested: Vec<_> = std::fs::read_dir(src.join("control"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "rs"))
        .collect();
    nested.sort();
    files.extend(nested);
    let mut functions = Vec::new();
    for file in files {
        let text = std::fs::read_to_string(&file).unwrap();
        let text = text.split("#[cfg(test)]\nmod tests").next().unwrap();
        let name = file.file_name().unwrap().to_string_lossy().into_owned();
        let mut current: Option<(String, String)> = None;
        for line in text.lines() {
            let trimmed = line.trim_start();
            let indent = line.len() - trimmed.len();
            let starts_fn = indent <= 4
                && ["fn ", "pub fn ", "pub(super) fn ", "pub(crate) fn "]
                    .iter()
                    .any(|prefix| trimmed.starts_with(prefix));
            if starts_fn {
                if let Some((signature, body)) = current.take() {
                    functions.push((name.clone(), signature, body));
                }
                current = Some((trimmed.to_owned(), String::new()));
            } else if let Some((_, body)) = current.as_mut() {
                body.push_str(line);
                body.push('\n');
            }
        }
        if let Some((signature, body)) = current {
            functions.push((name, signature, body));
        }
    }
    functions
}

#[test]
fn every_launch_site_holds_the_account_gate() {
    let mut sites = 0;
    let mut unguarded = Vec::new();
    for (file, signature, body) in control_functions() {
        if !LAUNCHES.iter().any(|launch| body.contains(launch)) {
            continue;
        }
        sites += 1;
        if !body.contains("self.launch_permit()?") {
            unguarded.push(format!("{file}: {signature}"));
        }
    }
    // A scanner that stops seeing launches would pass vacuously.
    assert!(sites >= 8, "found only {sites} launch sites");
    assert!(
        unguarded.is_empty(),
        "launches without the account gate:\n{}",
        unguarded.join("\n")
    );
}

fn busy(error: ControlError) -> bool {
    error.message.contains("account switch is in progress")
}

#[test]
fn launches_and_wakes_wait_out_an_account_switch() {
    let temp = tempfile::tempdir().unwrap();
    let server = server(temp.path());
    let mut asleep = test_record("asleep");
    asleep.hibernation = Some(diri_proto::HibernationInfo {
        since: diri_proto::DateMillis(0.0),
        reason: diri_proto::HibernationReason::Manual,
        tree_pids: Vec::new(),
        tree_start_times: None,
    });
    server.registry.lock().unwrap().insert_record(asleep);

    let switch = server.account_gate.exclusive().expect("switch");
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                let id = json!({"sessionID": "asleep"});
                for method in [
                    Method::SESSION_SPAWN,
                    Method::SESSION_SPAWN_TRACKED,
                    Method::TASK_SUBMIT,
                    Method::SESSION_RESUME,
                    Method::SESSION_RECONNECT,
                    Method::SESSION_REOPEN_LAST,
                    Method::AGENT_CONFIGURE,
                    Method::SESSION_WAKE,
                ] {
                    let error = server.dispatch(method, Some(id.clone())).unwrap_err();
                    assert!(busy(error.clone()), "{method}: {}", error.message);
                }
                // The primitives refuse on their own, whatever the caller.
                let shell = json!({"kind": diri_proto::AgentKind::SHELL, "cwd": temp.path()});
                assert!(busy(server.session_spawn(Some(shell)).unwrap_err()));
                assert!(busy(server.session_resume(Some(id.clone())).unwrap_err()));
                assert!(busy(server.relaunch_agent("asleep").unwrap_err()));
                // Delivering to a sleeping session wakes it: same gate.
                let deliver = json!({"sessionID": "asleep", "text": "hi", "submit": true,
                    "messageID": "m1", "senderID": "s"});
                let error = server
                    .dispatch(Method::SESSION_DELIVER_MESSAGE, Some(deliver))
                    .unwrap_err();
                assert!(busy(error.clone()), "deliver: {}", error.message);
            })
            .join()
            .unwrap();
    });
    drop(switch);
    assert!(server.account_gate.exclusive().is_some(), "gate released");
}

#[test]
fn resume_runs_only_under_its_lifecycle_reservation() {
    let temp = tempfile::tempdir().unwrap();
    let server = server(temp.path());
    server
        .registry
        .lock()
        .unwrap()
        .insert_record(test_record("closed"));
    let error = server
        .session_resume(Some(json!({"sessionID": "closed"})))
        .unwrap_err();
    assert!(
        error.message.contains("lifecycle reservation"),
        "{}",
        error.message
    );
    // Held by another operation: the resume request is refused, not interleaved.
    let held = account_handoff::SessionOperation::for_session(&server, "closed").unwrap();
    let error = server
        .dispatch(Method::SESSION_RESUME, Some(json!({"sessionID": "closed"})))
        .unwrap_err();
    assert!(
        error.message.contains("already changing"),
        "{}",
        error.message
    );
    drop(held);
}

#[test]
fn relaunch_eviction_keeps_a_run_that_came_up_meanwhile() {
    let temp = tempfile::tempdir().unwrap();
    let server = server(temp.path());
    server
        .registry
        .lock()
        .unwrap()
        .spawn(
            crate::session::SessionSpec {
                id: "live".into(),
                pty: crate::pty::PtySpec::new(
                    vec!["/bin/sh".into(), "-c".into(), "read -r line".into()],
                    temp.path(),
                )
                .size(80, 24),
                manifest_id: "shell".into(),
                authority: crate::status::Authority::ProcessOnly,
                logs_dir: temp.path().join("logs"),
                holder: None,
                remote: None,
                defer_launch: false,
            },
            test_record("live"),
        )
        .unwrap();
    // Live (the remote-resume race: SSH ran, then the run came back).
    assert!(
        server
            .evict_dead_unlocked("live", Duration::ZERO)
            .unwrap()
            .is_none()
    );
    assert!(server.registry.lock().unwrap().get("live").is_some());
    // Dead: evicted for the relaunch.
    server
        .registry
        .lock()
        .unwrap()
        .get("live")
        .unwrap()
        .send_text("done", true)
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !server
        .registry
        .lock()
        .unwrap()
        .record("live")
        .is_some_and(|record| matches!(record.status, diri_proto::SessionStatus::Exited(_)))
    {
        assert!(std::time::Instant::now() < deadline, "shell did not exit");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        server
            .evict_dead_unlocked("live", Duration::ZERO)
            .unwrap()
            .is_some()
    );
    assert!(server.registry.lock().unwrap().get("live").is_none());
}
