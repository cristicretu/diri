use super::*;

impl ControlServer {
    pub(super) fn workspace_mutate(
        &self,
        params: Option<JsonValue>,
    ) -> Result<JsonValue, ControlError> {
        let params = decode(params)?;
        // Inventory validation does not hold the Registry lock across disk I/O.
        // If removal wins immediately afterward, the saved placement remains an
        // unavailable reference; no layout operation starts or kills a process.
        let sessions = self
            .registry
            .lock()
            .map_err(poisoned)?
            .records()
            .into_iter()
            .map(|session| session.id)
            .collect();
        let snapshot = self.workspaces.apply(params, &sessions)?;
        // A revision invalidation is bounded even for a large catalog. Clients
        // fetch the latest snapshot and ignore older invalidations.
        self.events.publish(
            diri_proto::EventName::WORKSPACE_UPDATED,
            json!({"revision": snapshot.revision}),
            None,
        );
        encode(&snapshot)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_commits_before_invalidating_and_does_not_publish_rejected_edits() {
        let temp = tempfile::tempdir().unwrap();
        let server = crate::control::tests::server(temp.path());
        let events = server.events().subscribe(
            None,
            crate::events::Filter::new(
                None,
                Some(vec![diri_proto::EventName::WORKSPACE_UPDATED.into()]),
            ),
        );
        let params = json!({"expectedRevision": 0, "mutation": {"type": "createWorkspace", "name": "API and deployment"}});
        let committed = server
            .dispatch(Method::WORKSPACE_MUTATE, Some(params.clone()))
            .unwrap();
        let event = events.recv(Duration::from_millis(10)).unwrap();
        assert_eq!(event.params["revision"], committed["revision"]);
        assert_eq!(
            server.dispatch(Method::WORKSPACE_SNAPSHOT, None).unwrap(),
            committed
        );
        let durable: Value =
            serde_json::from_slice(&std::fs::read(temp.path().join("state.json")).unwrap())
                .unwrap();
        assert_eq!(durable["workspaceState"], committed);
        assert_eq!(
            server
                .dispatch(Method::WORKSPACE_MUTATE, Some(params))
                .unwrap_err()
                .code,
            "workspace_revision_conflict"
        );
        assert!(events.recv(Duration::ZERO).is_none());
        let _ = std::fs::remove_file(temp.path().join("state.json"));
        std::fs::create_dir(temp.path().join("state.json")).unwrap();
        assert_eq!(server.dispatch(Method::WORKSPACE_MUTATE, Some(json!({"expectedRevision": 1, "mutation": {"type": "createWorkspace", "name": "Not committed"}}))).unwrap_err().code, "workspace_storage_unavailable");
        assert!(events.recv(Duration::ZERO).is_none());
        assert_eq!(server.registry.lock().unwrap().record_count(), 0);
    }
}
