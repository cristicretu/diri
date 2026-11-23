use std::sync::Arc;

use diri_proto::surface::{Interaction, Scope, SurfaceDoc, template};
use serde_json::json;

use super::*;

fn store() -> (tempfile::TempDir, SurfaceStore) {
    let dir = tempfile::tempdir().unwrap();
    let store = SurfaceStore::open(dir.path().join("surfaces")).unwrap();
    (dir, store)
}

fn today(store: &SurfaceStore) -> SurfaceDoc {
    let doc = template::whats_for_today("", Scope::Global, &[]);
    store.create(doc, &Author::Agent("owner-1".into())).unwrap()
}

fn patch(ops: serde_json::Value, expected: Option<u64>) -> Patch {
    serde_json::from_value(json!({"ops": ops, "expected_revision": expected})).unwrap()
}

#[test]
fn create_assigns_id_owner_and_revision() {
    let (_dir, store) = store();
    let doc = today(&store);
    assert!(doc.id.starts_with("what-s-for-today-"), "{}", doc.id);
    assert_eq!(doc.revision, 1);
    assert_eq!(doc.owner.as_deref(), Some("owner-1"));
    assert_eq!(doc.updated_by.as_deref(), Some("agent owner-1"));
    let listed = store.list().unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].checklist, (1, 5));
    assert_eq!(store.resolve("today").unwrap(), doc.id);
    assert_eq!(store.load(&doc.id).unwrap().doc, doc);
}

#[cfg(unix)]
#[test]
fn files_are_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let (_dir, store) = store();
    let doc = today(&store);
    let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(store.dir()), 0o700);
    assert_eq!(mode(&store.path_for(&doc.id).unwrap()), 0o600);
}

#[test]
fn ticks_survive_agent_patches_and_are_logged() {
    let (_dir, store) = store();
    let doc = today(&store);
    let (_, event, notify) = store
        .interact(
            &doc.id,
            &Interaction::Check {
                checklist: "today".into(),
                item: "spec".into(),
                checked: true,
            },
        )
        .unwrap();
    assert!(notify);
    assert_eq!(event.seq, 1);
    assert_eq!(event.delivered_to.as_deref(), Some("owner-1"));
    // The tick did not move the revision, so the agent's guard still holds.
    let (next, applied) = store
        .patch(
            &doc.id,
            &patch(
                json!([{"op": "set", "id": "m-shipped", "fields": {"value": "9"}}]),
                Some(1),
            ),
            &Author::Agent("owner-1".into()),
        )
        .unwrap();
    assert_eq!(applied.revision, 2);
    assert_eq!(next.state.checks.get("today/spec"), Some(&true));
    let events = store.events(&doc.id, 0, 10).unwrap();
    assert_eq!(events.len(), 1);
    assert!(store.events(&doc.id, 1, 10).unwrap().is_empty());
}

#[test]
fn refused_patch_writes_nothing() {
    let (_dir, store) = store();
    let doc = today(&store);
    let path = store.path_for(&doc.id).unwrap();
    let before = fs::read(&path).unwrap();
    let error = store
        .patch(
            &doc.id,
            &patch(
                json!([{"op": "insert", "node": {"type": "link", "url": "file:///etc"}}]),
                None,
            ),
            &Author::Tool,
        )
        .unwrap_err();
    assert_eq!(error.code(), "invalid_url");
    assert_eq!(fs::read(&path).unwrap(), before);
    let error = store
        .patch(
            &doc.id,
            &patch(json!([{"op": "remove", "id": "summary"}]), Some(7)),
            &Author::Tool,
        )
        .unwrap_err();
    assert_eq!(error.code(), "revision_conflict");
}

#[test]
fn history_and_restore() {
    let (_dir, store) = store();
    let doc = today(&store);
    store
        .replace(&doc.id, Some("Tomorrow"), vec![], None, &Author::Tool)
        .unwrap();
    let versions = store.history(&doc.id).unwrap();
    assert_eq!(
        versions.iter().map(|v| v.revision).collect::<Vec<_>>(),
        vec![2, 1]
    );
    let restored = store.restore(&doc.id, 1, &Author::User).unwrap();
    assert_eq!(restored.revision, 3);
    assert_eq!(restored.title, "What's for Today");
    assert_eq!(restored.root, doc.root);
    assert_eq!(store.history(&doc.id).unwrap().len(), 3);
}

#[test]
fn history_is_bounded() {
    let (_dir, store) = store();
    let doc = today(&store);
    for n in 0..(MAX_VERSIONS + 5) {
        store
            .patch(
                &doc.id,
                &patch(
                    json!([{"op": "set", "id": "m-shipped", "fields": {"value": n.to_string()}}]),
                    None,
                ),
                &Author::Tool,
            )
            .unwrap();
    }
    assert_eq!(store.history(&doc.id).unwrap().len(), MAX_VERSIONS);
}

#[test]
fn events_are_bounded_and_sequenced() {
    let (_dir, store) = store();
    let doc = today(&store);
    for n in 0..(MAX_EVENTS + 10) {
        store
            .interact(
                &doc.id,
                &Interaction::Toggle {
                    id: "dnd".into(),
                    on: n % 2 == 0,
                },
            )
            .unwrap();
    }
    let events = store.events(&doc.id, 0, usize::MAX).unwrap();
    assert!(events.len() <= MAX_EVENTS);
    assert_eq!(events.last().unwrap().seq, (MAX_EVENTS + 10) as u64);
    assert!(events.windows(2).all(|w| w[0].seq + 1 == w[1].seq));
}

#[test]
fn promote_copies_a_session_surface() {
    let (_dir, store) = store();
    let mut doc = template::whats_for_today(
        "",
        Scope::Session {
            session: "s1".into(),
        },
        &[],
    );
    doc.pinned = false;
    let session = store.create(doc, &Author::Agent("s1".into())).unwrap();
    store
        .interact(
            &session.id,
            &Interaction::Check {
                checklist: "today".into(),
                item: "spec".into(),
                checked: true,
            },
        )
        .unwrap();
    let saved = store
        .promote(&session.id, Scope::Global, &Author::User)
        .unwrap();
    assert_ne!(saved.id, session.id);
    assert!(saved.pinned && saved.scope.is_saved());
    assert_eq!(saved.state.checks.get("today/spec"), Some(&true));
    assert_eq!(
        saved.owner.as_deref(),
        Some("s1"),
        "the agent that built it keeps receiving events"
    );
    assert!(store.load(&session.id).is_ok(), "the session surface stays");
}

#[test]
fn newer_schema_is_read_only() {
    let (_dir, store) = store();
    let doc = today(&store);
    let path = store.path_for(&doc.id).unwrap();
    let mut value: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    value["schema"] = json!(SCHEMA_VERSION + 1);
    value["root"]
        .as_array_mut()
        .unwrap()
        .push(json!({"type": "hologram", "id": "h"}));
    fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    let loaded = store.load(&doc.id).unwrap();
    assert!(loaded.read_only);
    let error = store
        .patch(
            &doc.id,
            &patch(json!([{"op": "remove", "id": "summary"}]), None),
            &Author::Tool,
        )
        .unwrap_err();
    assert_eq!(error.code(), "unsupported_schema");
    let error = store
        .interact(
            &doc.id,
            &Interaction::Toggle {
                id: "dnd".into(),
                on: true,
            },
        )
        .unwrap_err();
    assert_eq!(error.code(), "unsupported_schema");
    // Unchanged on disk: the newer component is still there.
    let after: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(after, value);
}

#[test]
fn hand_edited_files_out_of_bounds_are_refused() {
    let (_dir, store) = store();
    let doc = today(&store);
    let path = store.path_for(&doc.id).unwrap();
    let mut value: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    value["root"] = json!([{"type": "link", "url": "javascript:alert(1)"}]);
    fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    assert_eq!(store.load(&doc.id).unwrap_err().code(), "invalid_url");
    assert!(
        store.list().unwrap().is_empty(),
        "never listed, never drawn"
    );
}

#[test]
fn ids_never_escape_the_directory() {
    let (_dir, store) = store();
    for id in ["../x", "a/b", ".hidden", ""] {
        assert_eq!(store.load(id).unwrap_err().code(), "invalid_id", "{id:?}");
    }
}

#[test]
fn delete_moves_to_trash() {
    let (_dir, store) = store();
    let doc = today(&store);
    let trashed = store.delete(&doc.id).unwrap();
    assert!(trashed.exists());
    assert!(matches!(store.load(&doc.id), Err(StoreError::NotFound(_))));
    assert!(store.list().unwrap().is_empty());
}

/// Agents and the app racing on one surface: every write is serialized by
/// the lock, every revision lands once, and every tick survives.
#[test]
fn concurrent_writers_never_lose_updates() {
    let (_dir, store) = store();
    let doc = today(&store);
    let store = Arc::new(store);
    let id = Arc::new(doc.id.clone());
    let mut threads = Vec::new();
    for writer in 0..4 {
        let store = Arc::clone(&store);
        let id = Arc::clone(&id);
        threads.push(std::thread::spawn(move || {
            for n in 0..10 {
                store
                    .patch(
                        &id,
                        &patch(
                            json!([{"op": "insert", "parent": "agents", "node": {"type": "text", "id": format!("w{writer}-{n}"), "text": "x"}}]),
                            None,
                        ),
                        &Author::Agent(format!("w{writer}")),
                    )
                    .unwrap();
            }
        }));
    }
    let ticker = {
        let store = Arc::clone(&store);
        let id = Arc::clone(&id);
        std::thread::spawn(move || {
            for item in ["review-812", "eng-142", "spec", "release"] {
                store
                    .interact(
                        &id,
                        &Interaction::Check {
                            checklist: "today".into(),
                            item: item.into(),
                            checked: true,
                        },
                    )
                    .unwrap();
            }
        })
    };
    for thread in threads {
        thread.join().unwrap();
    }
    ticker.join().unwrap();
    let doc = store.load(&id).unwrap().doc;
    assert_eq!(doc.revision, 41);
    assert_eq!(doc.checklist_progress(), (5, 5));
}
