//! Pure session-lifecycle planning.
//!
//! The control server asks the Registry for one lifecycle operation; this
//! module owns the record invariants for that operation. Process termination,
//! persistence, and log cleanup stay in the Registry, while tests exercise the
//! same small planning interface production uses.

use std::io;

use diri_proto::{DateMillis, ExitInfo, ExitReason, SessionRecord, SessionStatus};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LifecycleAction {
    Archive,
    Restore,
    Remove,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct LifecyclePlan {
    pub(crate) replacement: Option<SessionRecord>,
    pub(crate) terminate_live_session: bool,
    pub(crate) retain_for_reopen: bool,
    pub(crate) delete_output_log: bool,
}

impl LifecyclePlan {
    pub(crate) fn for_record(
        record: &SessionRecord,
        action: LifecycleAction,
        now: DateMillis,
    ) -> io::Result<Self> {
        match action {
            LifecycleAction::Archive => {
                let mut replacement = record.clone();
                replacement.archived_at = Some(now);
                replacement.updated_at = now;
                if !matches!(replacement.status, SessionStatus::Exited(_)) {
                    replacement.status = SessionStatus::Exited(ExitInfo {
                        reason: ExitReason::Archived,
                        code: None,
                        signal: None,
                    });
                }
                replacement.needs_input = None;
                Ok(Self {
                    replacement: Some(replacement),
                    terminate_live_session: true,
                    retain_for_reopen: false,
                    delete_output_log: false,
                })
            }
            LifecycleAction::Restore => {
                let mut replacement = record.clone();
                replacement.archived_at = None;
                replacement.updated_at = now;
                Ok(Self {
                    replacement: Some(replacement),
                    terminate_live_session: false,
                    retain_for_reopen: false,
                    delete_output_log: false,
                })
            }
            LifecycleAction::Remove => Ok(Self {
                replacement: None,
                terminate_live_session: true,
                retain_for_reopen: true,
                delete_output_log: true,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use diri_proto::{AgentKind, ProjectId, Resumability, SessionId, SessionStatus, TitleSource};

    use super::*;

    fn record() -> SessionRecord {
        SessionRecord {
            id: SessionId::new("session"),
            kind: AgentKind::CODEX,
            cwd: "/repo".into(),
            project_id: ProjectId::new("project"),
            worktree_path: None,
            git_branch: None,
            title: "Work".into(),
            title_source: TitleSource::AgentProvided,
            originating_prompt: None,
            agent_session_id: Some("conversation".into()),
            transcript_path: None,
            status: SessionStatus::Working,
            status_evidence: None,
            needs_input: None,
            resumability: Resumability::Live,
            capabilities: None,
            parent: None,
            created_at: DateMillis(1.0),
            updated_at: DateMillis(1.0),
            last_turn_completed_at: None,
            last_seen_at: None,
            pinned: true,
            archived_at: None,
            host: None,
            remote_persistence: None,
            hibernation: None,
            memory_bytes: None,
            artifacts: None,
            pull_requests: None,
            listening_ports: None,
            foreground_agent: None,
        }
    }

    #[test]
    fn archive_preserves_identity_and_clears_attention() {
        let original = record();
        let plan = LifecyclePlan::for_record(&original, LifecycleAction::Archive, DateMillis(42.0))
            .expect("plan");
        let archived = plan.replacement.expect("replacement");

        assert_eq!(archived.id, original.id);
        assert_eq!(archived.agent_session_id, original.agent_session_id);
        assert_eq!(archived.archived_at, Some(DateMillis(42.0)));
        assert!(matches!(
            archived.status,
            SessionStatus::Exited(ExitInfo {
                reason: ExitReason::Archived,
                ..
            })
        ));
        assert!(plan.terminate_live_session);
        assert!(!plan.delete_output_log);
    }

    #[test]
    fn remove_is_the_only_plan_that_drops_identity_and_output() {
        let plan = LifecyclePlan::for_record(&record(), LifecycleAction::Remove, DateMillis(42.0))
            .expect("plan");

        assert!(plan.replacement.is_none());
        assert!(plan.terminate_live_session);
        assert!(plan.retain_for_reopen);
        assert!(plan.delete_output_log);
    }

    #[test]
    fn archiving_an_ended_session_preserves_its_exit_evidence() {
        let mut original = record();
        original.status = SessionStatus::Exited(ExitInfo {
            reason: ExitReason::Signaled,
            code: None,
            signal: Some(9),
        });

        let archived =
            LifecyclePlan::for_record(&original, LifecycleAction::Archive, DateMillis(42.0))
                .expect("plan")
                .replacement
                .expect("replacement");

        assert_eq!(archived.status, original.status);
    }
}
