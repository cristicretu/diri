//! Authorization for MCP writes initiated by an agent session.
//!
//! Reads intentionally remain fleet-wide. Writes are narrower: a root agent
//! may coordinate its project, while a delegated agent may only write to its
//! parent or direct children. Every write is re-evaluated from the Engine's
//! latest session snapshot, so a stale or unhosted MCP process fails closed.

use std::path::Path;

use diri_proto::{Project, SessionRecord, SessionStatus};

use super::{Lineage, Relation};

pub(super) const WRITE_POLICY: &str = "Reads are open across all sessions. Root agents may write within their project; delegated agents may write only to their parent and direct children. Every write requires a live Diri session identity. Cross-lineage messages are attributed. Agents cannot target themselves, and only roots may release non-child sessions in their project.";

#[derive(Clone, Copy, Debug)]
pub(super) enum WriteAction<'a> {
    Spawn,
    SendPrompt { target: &'a str },
    Release { target: &'a str },
    Worktree { repo: &'a str },
    Browser,
    TestRun,
    ReportToParent { target: &'a str },
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Authorization<'a> {
    caller: &'a SessionRecord,
    relation: Relation,
}

impl Authorization<'_> {
    pub(super) fn relation(self) -> Relation {
        self.relation
    }

    pub(super) fn frame(self, text: &str) -> String {
        if self.relation.delivers_verbatim() {
            return text.to_owned();
        }
        format!(
            "[message from id:{} ({}), channel: dirijor — reply with send_prompt to that id]\n\n{text}",
            self.caller.id.0, self.caller.title
        )
    }
}

pub(super) struct McpPolicy<'a> {
    projects: &'a [Project],
    lineage: Lineage<'a>,
    caller: &'a SessionRecord,
}

impl<'a> McpPolicy<'a> {
    pub(super) fn new(
        records: &'a [SessionRecord],
        projects: &'a [Project],
        caller: Option<&'a str>,
    ) -> Result<Self, String> {
        let caller = caller.ok_or_else(|| {
            "MCP writes require DIRIJOR_SESSION_ID and must run inside a live Diri session"
                .to_owned()
        })?;
        let lineage = Lineage::new(records, Some(caller));
        let caller = lineage.record(caller).ok_or_else(|| {
            format!("MCP writes are disabled because calling session {caller} is not live")
        })?;
        if caller.is_archived() || matches!(caller.status, SessionStatus::Exited(_)) {
            return Err(format!(
                "MCP writes are disabled because calling session {} is no longer live",
                caller.id.0
            ));
        }
        Ok(Self {
            projects,
            lineage,
            caller,
        })
    }

    /// The module's single write interface. Callers name the semantic action;
    /// lineage traversal, project reach, and self/ancestor protection remain
    /// implementation details here.
    pub(super) fn authorize(&self, action: WriteAction<'_>) -> Result<Authorization<'a>, String> {
        let relation = match action {
            WriteAction::Spawn | WriteAction::Browser | WriteAction::TestRun => Relation::Unrelated,
            WriteAction::Worktree { repo } => {
                let project = self
                    .projects
                    .iter()
                    .find(|project| project.id == self.caller.project_id)
                    .ok_or_else(|| {
                        format!(
                            "MCP writes are disabled because project {} is not live",
                            self.caller.project_id
                        )
                    })?;
                if !same_path(&project.root, repo) {
                    return Err(format!(
                        "worktree write denied: {repo} is outside calling project {}",
                        project.root
                    ));
                }
                Relation::Unrelated
            }
            WriteAction::SendPrompt { target } => {
                let (target_record, relation) = self.target(target)?;
                if relation == Relation::Caller {
                    return Err(format!(
                        "send_prompt cannot target the calling session ({target}); answer normally instead"
                    ));
                }
                if !self.can_message(target_record, relation) {
                    return if self.is_root() {
                        Err(format!(
                            "send_prompt denied: {target} is outside calling project {}",
                            self.caller.project_id
                        ))
                    } else {
                        Err(format!(
                            "send_prompt denied: delegated sessions may message only their parent or direct children; {target} is {}",
                            relation.as_str()
                        ))
                    };
                }
                relation
            }
            WriteAction::Release { target } => {
                let (target_record, relation) = self.target(target)?;
                if relation == Relation::Caller {
                    return Err("release_agent cannot terminate its caller".into());
                }
                if !self.can_release(target_record, relation) {
                    let reason = if matches!(relation, Relation::Parent | Relation::Ancestor) {
                        "the session waiting on this result"
                    } else {
                        "a session outside its direct children"
                    };
                    return Err(format!("release_agent cannot terminate {reason}"));
                }
                relation
            }
            WriteAction::ReportToParent { target } => {
                let (_, relation) = self.target(target)?;
                if relation != Relation::Parent {
                    return Err(format!(
                        "report_to_parent denied: {target} is {}, not the direct parent",
                        relation.as_str()
                    ));
                }
                relation
            }
        };
        Ok(Authorization {
            caller: self.caller,
            relation,
        })
    }

    fn target(&self, target: &str) -> Result<(&'a SessionRecord, Relation), String> {
        let record = self
            .lineage
            .record(target)
            .ok_or_else(|| format!("no such session: {target}"))?;
        Ok((record, self.lineage.relation(target)))
    }

    fn is_root(&self) -> bool {
        self.caller.parent.is_none()
    }

    fn same_project(&self, target: &SessionRecord) -> bool {
        self.caller.project_id == target.project_id
    }

    fn can_message(&self, target: &SessionRecord, relation: Relation) -> bool {
        if self.is_root() {
            self.same_project(target)
        } else {
            matches!(relation, Relation::Parent | Relation::Child)
        }
    }

    fn can_release(&self, target: &SessionRecord, relation: Relation) -> bool {
        if self.is_root() {
            self.same_project(target)
        } else {
            relation == Relation::Child
        }
    }
}

fn same_path(left: &str, right: &str) -> bool {
    let left = Path::new(left);
    let right = Path::new(right);
    left == right
        || left
            .canonicalize()
            .ok()
            .zip(right.canonicalize().ok())
            .is_some_and(|(left, right)| left == right)
}

#[cfg(test)]
mod tests {
    use super::*;
    use diri_proto::{
        AgentKind, DateMillis, ExitInfo, ExitReason, ProjectId, Resumability, SessionId,
        SessionStatus, TitleSource,
    };

    fn record(id: &str, project: &str, parent: Option<&str>) -> SessionRecord {
        SessionRecord {
            id: SessionId::new(id),
            kind: AgentKind::CODEX,
            cwd: format!("/tmp/{project}"),
            project_id: ProjectId::new(project),
            worktree_path: None,
            git_branch: None,
            title: id.into(),
            title_source: TitleSource::Placeholder,
            originating_prompt: None,
            agent_session_id: None,
            transcript_path: None,
            status: SessionStatus::Idle,
            status_evidence: None,
            needs_input: None,
            resumability: Resumability::Live,
            capabilities: None,
            parent: parent.map(SessionId::new),
            created_at: DateMillis(0.0),
            updated_at: DateMillis(0.0),
            last_turn_completed_at: None,
            last_seen_at: None,
            pinned: false,
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

    fn project(id: &str) -> Project {
        Project {
            id: ProjectId::new(id),
            root: format!("/tmp/{id}"),
            name: id.into(),
            pinned_order: None,
            host: None,
        }
    }

    #[test]
    fn writes_fail_closed_without_a_live_caller() {
        let mut exited = record("exited", "p", None);
        exited.status = SessionStatus::Exited(ExitInfo {
            reason: ExitReason::Exited,
            code: Some(0),
            signal: None,
        });
        let records = vec![record("root", "p", None), exited];
        let projects = vec![project("p")];
        assert!(McpPolicy::new(&records, &projects, None).is_err());
        assert!(McpPolicy::new(&records, &projects, Some("gone")).is_err());
        assert!(McpPolicy::new(&records, &projects, Some("exited")).is_err());
    }

    #[test]
    fn root_agents_control_only_their_project() {
        let records = vec![
            record("root", "p", None),
            record("peer", "p", None),
            record("foreign", "q", None),
        ];
        let projects = vec![project("p"), project("q")];
        let policy = McpPolicy::new(&records, &projects, Some("root")).expect("policy");

        assert!(
            policy
                .authorize(WriteAction::SendPrompt { target: "peer" })
                .is_ok()
        );
        assert!(
            policy
                .authorize(WriteAction::Release { target: "peer" })
                .is_ok()
        );
        assert!(
            policy
                .authorize(WriteAction::SendPrompt { target: "foreign" })
                .is_err()
        );
    }

    #[test]
    fn delegates_are_limited_to_direct_lineage() {
        let records = vec![
            record("root", "p", None),
            record("caller", "p", Some("root")),
            record("child", "p", Some("caller")),
            record("grandchild", "p", Some("child")),
            record("sibling", "p", Some("root")),
        ];
        let projects = vec![project("p")];
        let policy = McpPolicy::new(&records, &projects, Some("caller")).expect("policy");

        let parent_write = policy
            .authorize(WriteAction::SendPrompt { target: "root" })
            .expect("parent should be writable");
        assert_eq!(parent_write.frame("hello"), "hello");
        let child_write = policy
            .authorize(WriteAction::SendPrompt { target: "child" })
            .expect("child should be writable");
        assert_eq!(child_write.frame("hello"), "hello");
        assert!(
            policy
                .authorize(WriteAction::ReportToParent { target: "root" })
                .is_ok()
        );
        assert!(
            policy
                .authorize(WriteAction::ReportToParent { target: "child" })
                .is_err()
        );
        for target in ["grandchild", "sibling"] {
            assert!(
                policy
                    .authorize(WriteAction::SendPrompt { target })
                    .is_err(),
                "{target} should be outside direct lineage"
            );
        }
        assert!(
            policy
                .authorize(WriteAction::Release { target: "child" })
                .is_ok()
        );
        assert!(
            policy
                .authorize(WriteAction::Release { target: "root" })
                .is_err()
        );
    }

    #[test]
    fn cross_lineage_messages_are_attributed() {
        let records = vec![record("root", "p", None), record("peer", "p", None)];
        let projects = vec![project("p")];
        let policy = McpPolicy::new(&records, &projects, Some("root")).expect("policy");
        let authorized = policy
            .authorize(WriteAction::SendPrompt { target: "peer" })
            .expect("same-project root write");
        assert!(
            authorized
                .frame("hello")
                .starts_with("[message from id:root")
        );
    }

    #[test]
    fn project_writes_stay_in_the_callers_project() {
        let records = vec![record("root", "p", None)];
        let projects = vec![project("p")];
        let policy = McpPolicy::new(&records, &projects, Some("root")).expect("policy");

        assert!(policy.authorize(WriteAction::Spawn).is_ok());
        assert!(policy.authorize(WriteAction::Browser).is_ok());
        assert!(policy.authorize(WriteAction::TestRun).is_ok());
        assert!(
            policy
                .authorize(WriteAction::Worktree { repo: "/tmp/p" })
                .is_ok()
        );
        assert!(
            policy
                .authorize(WriteAction::Worktree { repo: "/tmp/q" })
                .is_err()
        );
    }
}
