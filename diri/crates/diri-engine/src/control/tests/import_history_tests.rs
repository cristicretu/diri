//! First-launch import through the dispatcher: candidates come back from a
//! fixture home, importing creates resumable records with no process behind
//! them, and importing again changes nothing.
use super::*;
use crate::onboarding_import::tests::{Fixture, uuid};

fn server(fixture: &Fixture, state: &Path) -> (Arc<Mutex<Registry>>, Arc<ControlServer>) {
    let registry = Arc::new(Mutex::new(Registry::new(
        engine(),
        state.join("state.json"),
    )));
    let server = Arc::new(
        ControlServer::new(Arc::clone(&registry), state.join("daemon.sock"))
            .with_history_home(&fixture.home),
    );
    (registry, server)
}

fn candidates(server: &Arc<ControlServer>) -> diri_proto::HistoryImportCandidatesResult {
    serde_json::from_value(ok_of(call(server, "history.import_candidates", None))).unwrap()
}

fn import(
    server: &Arc<ControlServer>,
    entries: &[diri_proto::HistoryEntry],
) -> diri_proto::SessionImportHistoryResult {
    serde_json::from_value(ok_of(call(
        server,
        "session.import_history",
        Some(json!({ "entries": entries })),
    )))
    .unwrap()
}

#[test]
fn import_creates_resumable_records_without_starting_anything_and_is_idempotent() {
    let fixture = Fixture::standard();
    let state = tempfile::tempdir().unwrap();
    let (registry, server) = server(&fixture, state.path());

    let found = candidates(&server);
    let suggested: Vec<_> = found.suggested.iter().map(|c| c.entry.id.clone()).collect();
    assert_eq!(suggested, (1..=5).map(uuid).collect::<Vec<_>>());
    let more: Vec<_> = found.more.iter().map(|c| c.entry.id.clone()).collect();
    assert_eq!(more, vec![uuid(6), uuid(7)]);

    // The five suggestions, one of them twice, plus a junk entry a client
    // made up: a scratch folder.
    let mut entries: Vec<_> = found.suggested.iter().map(|c| c.entry.clone()).collect();
    entries.push(entries[0].clone());
    let mut junk = entries[0].clone();
    junk.id = uuid(22);
    junk.cwd = "/private/tmp/experiment".into();
    entries.push(junk);
    let result = import(&server, &entries);
    assert_eq!(result.sessions.len(), 5);
    assert_eq!(result.skipped, vec![uuid(1), uuid(22)]);

    {
        let registry = registry.lock().unwrap();
        // Zero spawn: no live session, launching or otherwise.
        assert_eq!(registry.live_count(), 0);
        for record in &result.sessions {
            assert!(registry.get(&record.id.0).is_none());
            assert!(!registry.is_live(&record.id.0));
            assert_eq!(record.resumability, diri_proto::Resumability::Resumable);
            assert!(record.can_resume(), "{:?}", record.capabilities);
            assert!(
                matches!(&record.status, diri_proto::SessionStatus::Exited(info) if info.is_imported())
            );
        }
        // Each conversation's folder is a sidebar project.
        let projects: Vec<_> = registry
            .projects_raw()
            .iter()
            .filter_map(|project| {
                project
                    .get("root")
                    .and_then(JsonValue::as_str)
                    .map(str::to_owned)
            })
            .collect();
        for folder in [&fixture.anara, &fixture.diri] {
            assert!(
                projects
                    .iter()
                    .any(|root| Path::new(root) == folder.as_path()),
                "{folder:?} missing from {projects:?}"
            );
        }
    }

    // Again: everything is skipped, nothing is added.
    let again = import(&server, &entries);
    assert!(again.sessions.is_empty());
    assert_eq!(registry.lock().unwrap().records().len(), 5);
    // And the imported ones are no longer offered; the browse list moves up.
    let after = candidates(&server);
    let left: Vec<_> = after.suggested.iter().map(|c| c.entry.id.clone()).collect();
    assert_eq!(left, vec![uuid(6), uuid(7)]);

    // The records survive an Engine restart as imported, still not running.
    let reloaded = Registry::new(engine(), state.path().join("state.json"));
    let mut reloaded = reloaded;
    reloaded.load().unwrap();
    let records = reloaded.records();
    assert_eq!(records.len(), 5);
    assert!(records.iter().all(|record| {
        matches!(&record.status, diri_proto::SessionStatus::Exited(info) if info.is_imported())
            && record.agent_session_id.is_some()
    }));
}

#[test]
fn import_refuses_more_than_one_scan_can_offer() {
    let fixture = Fixture::standard();
    let state = tempfile::tempdir().unwrap();
    let (_registry, server) = server(&fixture, state.path());
    let entry = candidates(&server).suggested[0].entry.clone();
    let entries =
        vec![entry; crate::onboarding_import::SUGGESTED + crate::onboarding_import::MAX_MORE + 1];
    let response = call(
        &server,
        "session.import_history",
        Some(json!({ "entries": entries })),
    );
    assert_eq!(err_of(response).code, "bad_request");
}
