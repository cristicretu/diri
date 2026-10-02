use super::*;
use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

/// A stand-in for Codex's startup self-update: the first run prints Codex's
/// closing words and exits 0, as `codex` does after `npm install -g`; every
/// later run shows the argv it got and stays up like the real TUI.
fn updating_agent(temp: &Path) -> PathBuf {
    let script = temp.join("fake-codex");
    let marker = temp.join("updated");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\n\
             if [ -e '{marker}' ]; then echo \"RELAUNCHED $*\"; exec sleep 60; fi\n\
             touch '{marker}'\n\
             printf '\\nUpdating Codex via `npm install -g @openai/codex`...\\n'\n\
             printf '\\n\\360\\237\\216\\211 Update ran successfully! Please restart Codex.\\n'\n",
            marker = marker.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    script
}

fn manifest(binary: &Path, home: &Path) -> crate::detect::Manifest {
    serde_json::from_value(json!({
        "schemaVersion": 2,
        "id": "codex",
        "version": "test",
        "statusModel": "full",
        "agent": {
            "binary": binary.to_string_lossy(),
            "returnToLoginShell": true,
            "relaunchNotice": "Please restart Codex.",
            "injection": { "codexMCP": true },
            // A known shell and home: the wrapper must not source the
            // developer's own login configuration.
            "env": { "SHELL": "/bin/sh", "HOME": home.to_string_lossy(), "ENV": "" },
        },
        "rules": [],
    }))
    .expect("manifest")
}

fn screen(registry: &Mutex<Registry>, id: &str) -> String {
    registry
        .lock()
        .unwrap()
        .get(id)
        .map(|session| session.screen_lines().join("\n"))
        .unwrap_or_default()
}

fn wait_for_screen(registry: &Mutex<Registry>, id: &str, needle: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let text = screen(registry, id);
        if text.contains(needle) {
            return text;
        }
        assert!(
            Instant::now() < deadline,
            "screen never showed {needle:?}:\n{text}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn an_agent_that_updated_itself_is_relaunched_with_its_injection() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let agent = updating_agent(temp.path());
    let engine = Arc::new(crate::detect::ManifestEngine::new(vec![manifest(
        &agent, &home,
    )]));
    let registry = Arc::new(Mutex::new(Registry::new(
        engine,
        temp.path().join("state.json"),
    )));
    let server = Arc::new(
        ControlServer::new(Arc::clone(&registry), temp.path().join("daemon.sock")).with_injection(
            InjectionConfig {
                inject_dir: temp.path().join("inject"),
                cli_path: temp.path().join("dirijor"),
            },
        ),
    );
    server.spawn_agent_relaunch();
    let stop = Arc::new(AtomicBool::new(false));
    let watcher = crate::events::spawn_registry_watcher(
        Arc::clone(&registry),
        server.events(),
        Arc::clone(&stop),
    );

    let mut record = test_record("s_update");
    record.kind = diri_proto::AgentKind::CODEX;
    record.cwd = temp.path().to_string_lossy().into_owned();
    {
        let mut guard = registry.lock().unwrap();
        let spec = server
            .fresh_spec(&guard, "s_update", "codex", &record.cwd, None)
            .unwrap();
        guard.spawn(spec, record).unwrap();
    }

    let relaunched = wait_for_screen(&registry, "s_update", "RELAUNCHED");
    assert!(
        relaunched.contains("mcp_servers.dirijor.command="),
        "the relaunch carries the injected MCP server:\n{relaunched}"
    );
    assert!(
        !relaunched.contains("Please restart Codex."),
        "the relaunch is a new terminal, not the run that exited:\n{relaunched}"
    );

    stop.store(true, Ordering::SeqCst);
    let _ = watcher.join();
    let _ = registry
        .lock()
        .unwrap()
        .terminate("s_update", Duration::ZERO);
}

#[test]
fn a_clean_exit_without_the_notice_ends_the_session() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let agent = temp.path().join("fake-codex");
    std::fs::write(&agent, "#!/bin/sh\necho 'Goodbye from codex'\n").unwrap();
    std::fs::set_permissions(&agent, std::fs::Permissions::from_mode(0o755)).unwrap();
    let engine = Arc::new(crate::detect::ManifestEngine::new(vec![manifest(
        &agent, &home,
    )]));
    let registry = Arc::new(Mutex::new(Registry::new(
        engine,
        temp.path().join("state.json"),
    )));
    let server = Arc::new(ControlServer::new(
        Arc::clone(&registry),
        temp.path().join("daemon.sock"),
    ));
    let mut record = test_record("s_quit");
    record.kind = diri_proto::AgentKind::CODEX;
    record.cwd = temp.path().to_string_lossy().into_owned();
    {
        let mut guard = registry.lock().unwrap();
        let spec = server
            .fresh_spec(&guard, "s_quit", "codex", &record.cwd, None)
            .unwrap();
        guard.spawn(spec, record).unwrap();
    }
    wait_for_screen(&registry, "s_quit", "Goodbye from codex");
    // The agent's own exit ends the session, published as a clean exit.
    let deadline = Instant::now() + Duration::from_secs(5);
    while !registry
        .lock()
        .unwrap()
        .get("s_quit")
        .is_some_and(|session| {
            matches!(
                session.status(),
                diri_proto::SessionStatus::Exited(diri_proto::ExitInfo { code: Some(0), .. })
            )
        })
    {
        assert!(Instant::now() < deadline, "the session did not end");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(registry.lock().unwrap().take_relaunch_requests().is_empty());
    let _ = registry.lock().unwrap().terminate("s_quit", Duration::ZERO);
}

#[test]
fn a_relaunch_that_fails_still_ends_the_session() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let agent = updating_agent(temp.path());
    let engine = Arc::new(crate::detect::ManifestEngine::new(vec![manifest(
        &agent, &home,
    )]));
    let registry = Arc::new(Mutex::new(Registry::new(
        engine,
        temp.path().join("state.json"),
    )));
    let server = Arc::new(ControlServer::new(
        Arc::clone(&registry),
        temp.path().join("daemon.sock"),
    ));
    server.spawn_agent_relaunch();
    let stop = Arc::new(AtomicBool::new(false));
    let watcher = crate::events::spawn_registry_watcher(
        Arc::clone(&registry),
        server.events(),
        Arc::clone(&stop),
    );

    let mut record = test_record("s_stuck");
    record.kind = diri_proto::AgentKind::CODEX;
    record.cwd = temp.path().to_string_lossy().into_owned();
    // An account bound to another agent: the relaunch refuses to start it.
    record.account_profile = Some(diri_proto::AgentAccountProfile {
        id: "other".into(),
        label: "Other".into(),
        agent: "claude-code".into(),
        host: None,
        config_home: temp.path().join("other").to_string_lossy().into_owned(),
        is_default: false,
        login_store: None,
    });
    {
        let mut guard = registry.lock().unwrap();
        let spec = server
            .fresh_spec(&guard, "s_stuck", "codex", &record.cwd, None)
            .unwrap();
        guard.spawn(spec, record).unwrap();
    }

    // The exit held back for the relaunch is published once it fails, so
    // the tab never looks alive with no process behind it.
    let deadline = Instant::now() + Duration::from_secs(15);
    while !registry
        .lock()
        .unwrap()
        .get("s_stuck")
        .is_some_and(|session| {
            matches!(
                session.status(),
                diri_proto::SessionStatus::Exited(diri_proto::ExitInfo { code: Some(0), .. })
            )
        })
    {
        assert!(
            Instant::now() < deadline,
            "the failed relaunch left the session looking alive:\n{}",
            screen(&registry, "s_stuck")
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!screen(&registry, "s_stuck").contains("RELAUNCHED"));

    stop.store(true, Ordering::SeqCst);
    let _ = watcher.join();
    let _ = registry
        .lock()
        .unwrap()
        .terminate("s_stuck", Duration::ZERO);
}
