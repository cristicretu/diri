//! What the `dirijor` CLI borrows from the MCP bridge: the same session to
//! pane resolution and the same write policy, so a shell script and an
//! agent's tool call see one set of layout rules and one release gate.

pub use super::layout::{Placement, edge, locate, placements, split_mutation, tab_view};
use super::*;
use diri_proto::workspace::{WorkspaceMutation, WorkspaceSnapshot};

/// Who is running the CLI, as far as the destructive-op gate is concerned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CliCaller {
    /// No `DIRIJOR_SESSION_ID`: a terminal outside Diri, so the owner.
    Outside,
    /// A Diri terminal with no Agent in the foreground: a person typing.
    Person,
    /// An Agent, or an Agent typed into a Diri terminal, or an identity that
    /// is no longer live (which fails closed like MCP).
    Agent,
}

/// Pure classification, so the gate is testable without an Engine.
pub fn classify_caller(caller: Option<&str>, sessions: &[SessionRecord]) -> CliCaller {
    let Some(caller) = caller else {
        return CliCaller::Outside;
    };
    match sessions.iter().find(|record| record.id.0 == caller) {
        Some(record) if record.kind.is_terminal() && record.foreground_agent.is_none() => {
            CliCaller::Person
        }
        _ => CliCaller::Agent,
    }
}

impl Bridge {
    /// The session this CLI runs in (`DIRIJOR_SESSION_ID`), if any.
    pub fn caller_id(&self) -> Option<&str> {
        self.caller.as_deref()
    }

    pub fn cli_caller(&self) -> Result<CliCaller, String> {
        if self.caller.is_none() {
            return Ok(CliCaller::Outside);
        }
        Ok(classify_caller(self.caller.as_deref(), &self.sessions()?))
    }

    /// Stopping or removing a session from the CLI follows MCP
    /// `release_agent` when an Agent runs it: root Agents reach their
    /// project, delegated Agents their direct children, and nobody their
    /// own session. A person (outside Diri, or at a Diri shell prompt) is
    /// not gated.
    pub fn authorize_cli_release(&self, target: &str) -> Result<CliCaller, String> {
        if self.caller.is_none() {
            return Ok(CliCaller::Outside);
        }
        let snapshot = self.snapshot()?;
        let caller = classify_caller(self.caller.as_deref(), &snapshot.sessions);
        if caller != CliCaller::Agent {
            return Ok(caller);
        }
        McpPolicy::new(
            &snapshot.sessions,
            &snapshot.projects,
            self.caller.as_deref(),
        )
        .and_then(|policy| policy.authorize(WriteAction::Release { target }))
        .map(|_| CliCaller::Agent)
        .map_err(|error| {
            format!(
                "{error}. Agents get the same reach as MCP release_agent: root agents their project, delegated agents their direct children"
            )
        })
    }

    pub fn workspace(&self) -> Result<WorkspaceSnapshot, String> {
        self.workspace_snapshot()
    }

    /// Applies the mutation `build` derives from a fresh snapshot, retrying
    /// on a revision conflict, exactly as `manage_layout` does.
    pub fn mutate_workspace(
        &self,
        build: impl FnMut(&WorkspaceSnapshot) -> Result<WorkspaceMutation, String>,
    ) -> Result<WorkspaceSnapshot, String> {
        self.mutate_layout(build)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use diri_proto::{AgentKind, SessionId};

    fn record(id: &str, kind: AgentKind, foreground: Option<AgentKind>) -> SessionRecord {
        let mut record: SessionRecord = serde_json::from_value(json!({
            "id": id,
            "kind": kind,
            "cwd": "/tmp",
            "projectID": "p",
            "title": id,
            "titleSource": diri_proto::TitleSource::Placeholder,
            "status": diri_proto::SessionStatus::Idle,
            "resumability": diri_proto::Resumability::Live,
            "createdAt": 0.0,
            "updatedAt": 0.0,
            "pinned": false,
        }))
        .expect("record fixture");
        record.id = SessionId::new(id);
        record.foreground_agent = foreground;
        record
    }

    #[test]
    fn people_at_a_prompt_are_not_gated_but_agents_and_stale_ids_are() {
        let sessions = vec![
            record("s_shell", AgentKind::SHELL, None),
            record("s_typed", AgentKind::SHELL, Some(AgentKind::CLAUDE_CODE)),
            record("s_agent", AgentKind::CODEX, None),
        ];
        assert_eq!(classify_caller(None, &sessions), CliCaller::Outside);
        assert_eq!(
            classify_caller(Some("s_shell"), &sessions),
            CliCaller::Person
        );
        assert_eq!(
            classify_caller(Some("s_typed"), &sessions),
            CliCaller::Agent
        );
        assert_eq!(
            classify_caller(Some("s_agent"), &sessions),
            CliCaller::Agent
        );
        assert_eq!(classify_caller(Some("s_gone"), &sessions), CliCaller::Agent);
    }
}
