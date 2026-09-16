use super::*;

impl ControlServer {
    pub(super) fn workspace_mutate(
        &self,
        params: Option<JsonValue>,
    ) -> Result<JsonValue, ControlError> {
        let params: diri_proto::workspace::WorkspaceMutationParams = decode(params)?;
        // Inventory validation does not hold the Registry lock across disk I/O.
        // If removal wins immediately afterward, the saved placement remains an
        // unavailable reference; no layout operation starts or kills a process.
        let (sessions, project_agent) = {
            let registry = self.registry.lock().map_err(poisoned)?;
            let records = registry.records();
            let project_agent =
                if let diri_proto::workspace::WorkspaceMutation::OpenProjectAgent {
                    session_id,
                    ..
                } = &params.mutation
                {
                    let record = records
                        .iter()
                        .find(|record| &record.id == session_id)
                        .ok_or_else(|| {
                            ControlError::new(
                                "workspace_session_unavailable",
                                "the session is absent from the Engine inventory",
                            )
                        })?;
                    let project = registry
                        .projects_raw()
                        .iter()
                        .filter_map(|project| {
                            serde_json::from_value::<diri_proto::Project>(project.clone()).ok()
                        })
                        .find(|project| project.id == record.project_id)
                        .ok_or_else(|| {
                            ControlError::new(
                                "workspace_project_unavailable",
                                "the agent project is absent from the Engine inventory",
                            )
                        })?;
                    Some(crate::workspace::ProjectAgentInventory {
                        session_id: session_id.clone(),
                        project,
                        session_projects: records
                            .iter()
                            .map(|record| (record.id.clone(), record.project_id.clone()))
                            .collect(),
                    })
                } else {
                    None
                };
            (
                records.into_iter().map(|record| record.id).collect(),
                project_agent,
            )
        };
        let snapshot =
            self.workspaces
                .apply_with_project_agent(params, &sessions, project_agent.as_ref())?;
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
    fn opening_project_agent_uses_registry_identity_without_spawning_or_changing_records() {
        let temp = tempfile::tempdir().unwrap();
        let server = crate::control::tests::server(temp.path());
        let (local, remote) = {
            let mut registry = server.registry.lock().unwrap();
            let local: diri_proto::Project =
                serde_json::from_value(registry.ensure_session_project("/repos/app", None))
                    .unwrap();
            let remote: diri_proto::Project = serde_json::from_value(
                registry.ensure_session_project("/repos/app", Some("production")),
            )
            .unwrap();
            for (id, project) in [("local_agent", &local), ("remote_agent", &remote)] {
                let mut record = crate::control::tests::test_record(id);
                record.project_id = project.id.clone();
                record.host = project.host.clone();
                registry.insert_record(record);
            }
            (local, remote)
        };
        assert_eq!(local.name, remote.name);
        assert_ne!(local.id, remote.id);
        let before = server.registry.lock().unwrap().records();
        for (revision, session) in [(0, "local_agent"), (1, "remote_agent")] {
            server
                .dispatch(
                    Method::WORKSPACE_MUTATE,
                    Some(json!({
                        "expectedRevision": revision,
                        "mutation": {"type":"openProjectAgent", "sessionId":session}
                    })),
                )
                .unwrap();
        }
        let snapshot = server.workspaces.snapshot().unwrap();
        assert_eq!(snapshot.workspaces.len(), 2);
        assert_eq!(snapshot.workspaces[0].project_id, Some(local.id));
        assert_eq!(snapshot.workspaces[1].project_id, Some(remote.id));
        let registry = server.registry.lock().unwrap();
        let after = registry.records();
        for record in before {
            assert_eq!(
                after.iter().find(|candidate| candidate.id == record.id),
                Some(&record)
            );
        }
        assert_eq!(after.len(), 2);
        assert!(registry.get("local_agent").is_none());
        assert!(registry.get("remote_agent").is_none());
    }

    #[test]
    fn missing_project_or_agent_rejects_without_saving_workspace_state() {
        let temp = tempfile::tempdir().unwrap();
        let server = crate::control::tests::server(temp.path());
        server
            .registry
            .lock()
            .unwrap()
            .insert_record(crate::control::tests::test_record("orphan"));
        for (session, code) in [
            ("orphan", "workspace_project_unavailable"),
            ("absent", "workspace_session_unavailable"),
        ] {
            let error = server.dispatch(Method::WORKSPACE_MUTATE, Some(json!({
                "expectedRevision":0, "mutation":{"type":"openProjectAgent", "sessionId":session}
            }))).unwrap_err();
            assert_eq!(error.code, code);
        }
        assert!(server.workspaces.snapshot().unwrap().workspaces.is_empty());
    }

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
