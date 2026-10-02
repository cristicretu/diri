//! Restarts an agent that exited only to be started again.
//!
//! Codex's startup update chooser runs `npm install -g @openai/codex` (or the
//! Homebrew/bun equivalent), prints "Please restart Codex." and exits 0. A
//! clean exit ends the session, so the tab would close; and a bare `codex`
//! started by hand would run without the `-c` overrides Diri injects (the
//! dirijor MCP server and its notify hook). The session pump spots the
//! manifest's `relaunchNotice` as the agent exits and holds that exit back;
//! this relaunches the tab through the same spec builders `session.resume`
//! uses.
use super::*;

impl ControlServer {
    /// Serves relaunch requests published by the Registry watcher.
    pub fn spawn_agent_relaunch(self: &Arc<Self>) {
        let stream = self.events.subscribe(
            None,
            crate::events::Filter::new(
                None,
                Some(vec![crate::events::RELAUNCH_REQUESTED.to_owned()]),
            ),
        );
        let server = Arc::downgrade(self);
        if let Err(error) = std::thread::Builder::new()
            .name("diri-agent-relaunch".into())
            .spawn(move || {
                loop {
                    let Some(event) = stream.recv(Duration::from_secs(3600)) else {
                        if server.strong_count() == 0 {
                            return;
                        }
                        continue;
                    };
                    let Some(server) = server.upgrade() else {
                        return;
                    };
                    if event.name != crate::events::RELAUNCH_REQUESTED {
                        continue;
                    }
                    let Some(id) = event.session_id else {
                        continue;
                    };
                    if let Err(error) = server.relaunch_agent(&id) {
                        diri_telemetry::warn_event!(
                            "session.agent_relaunch_failed",
                            session = diri_telemetry::id(&id),
                            code = diri_telemetry::id(&error.code),
                        );
                    } else {
                        diri_telemetry::event!(
                            "session.agent_relaunched",
                            session = diri_telemetry::id(&id),
                        );
                    }
                }
            })
        {
            eprintln!("diri-engine: could not start agent relaunch: {error}");
        }
    }

    /// Replaces the tab's exited agent with a fresh launch of it.
    ///
    /// The notice is printed before the agent has a conversation (Codex's
    /// update chooser runs ahead of its TUI), so a tab that knows no
    /// conversation starts fresh and one launched to resume resumes the same
    /// id again; never `resume --last`, which could pick another tab's thread.
    pub(super) fn relaunch_agent(&self, id: &str) -> Result<(), ControlError> {
        let _operation = account_handoff::SessionOperation::for_session(self, id)?;
        let (record, mut spec) = {
            let registry = self.registry.lock().map_err(poisoned)?;
            let record = registry
                .record(id)
                .ok_or_else(|| ControlError::not_found(id.to_owned()))?;
            if record.host.is_some()
                || record.is_archived()
                || registry.get(id).is_none()
                || matches!(record.status, diri_proto::SessionStatus::Exited(_))
            {
                return Err(ControlError::bad_request(
                    "Only a live local tab is relaunched",
                ));
            }
            let spec = match record.agent_session_id.as_deref() {
                Some(conversation) => self.resume_spec(
                    &registry,
                    id,
                    record.kind.id(),
                    &record.cwd,
                    Some(conversation),
                )?,
                None => self.fresh_spec(&registry, id, record.kind.id(), &record.cwd, None)?,
            };
            (record, spec)
        };
        if let Some(mut profile) = record.account_profile.clone() {
            if profile.host.is_some() || profile.agent != record.kind.id() {
                return Err(ControlError::bad_request(
                    "Session account does not match its Agent or host",
                ));
            }
            crate::accounts::bind_pty(&mut profile, &mut spec.pty)?;
        }
        // The agent has already exited (that exit is what asked for this),
        // so ending the session waits on nothing. Ending it under the same
        // lock as the respawn keeps its exit from ever being published: the
        // app would close a tab whose agent exited cleanly.
        let mut registry = self.registry.lock().map_err(poisoned)?;
        registry
            .terminate(id, Duration::from_millis(500))
            .map_err(io_control_error)?;
        registry.respawn(spec).map_err(io_control_error)?;
        registry.persist_now().map_err(io_control_error)?;
        self.publish_updated(&registry, id);
        Ok(())
    }
}
