//! Authorization for MCP writes initiated by an agent session.
//!
//! Reads intentionally remain fleet-wide. Writes are narrower: a root agent
//! may coordinate its project and message direct children on any host, while a
//! delegated agent may only write to its parent or direct children. Every write
//! is re-evaluated from the Engine's latest session snapshot, so a stale or
//! unhosted MCP process fails closed.

use std::path::Path;

use diri_proto::{Project, SessionRecord, SessionStatus};

use super::{Lineage, Relation};

pub(super) const WRITE_POLICY: &str = "Reads are open across all sessions. Root agents may write within their project and message direct children on any host; delegated agents may write only to their parent and direct children. Every write requires a live Diri session identity. Cross-lineage messages are attributed. Agents cannot target themselves. Root agents may release, hibernate, wake, resume, fork, or integrate only sessions in their project; delegated agents only their direct children. Split-pane layout follows the same reach, plus the caller's own pane and splitting beside its parent. Delegation depth and live children per session are capped (DIRIJOR_MAX_SPAWN_DEPTH, default 3; DIRIJOR_MAX_LIVE_CHILDREN, default 16).";

const DEFAULT_MAX_SPAWN_DEPTH: usize = 3;
const DEFAULT_MAX_LIVE_CHILDREN: usize = 16;

fn limit(variable: &str, default: usize) -> usize {
    std::env::var(variable)
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(default)
}

#[derive(Clone, Copy, Debug)]
pub(super) enum WriteAction<'a> {
    /// `count` sessions about to become direct children of the caller.
    Spawn {
        count: usize,
    },
    /// Lifecycle control over an existing session: hibernate, wake, resume,
    /// fork its conversation, or integrate its branch.
    Manage {
        target: &'a str,
    },
    QuickOpenInclude,
    /// Rearrange split panes: move/close/zoom `target`'s pane, or (with
    /// `anchor`) dock another pane beside it. Panes are views; no process
    /// is started or stopped.
    Layout {
        target: &'a str,
        anchor: bool,
    },
    SendPrompt {
        target: &'a str,
    },
    Release {
        target: &'a str,
    },
    Worktree {
        repo: &'a str,
    },
    Browser,
    /// Drive the Mac's apps through the shared Cua Driver, under the
    /// caller's own cursor and app leases.
    Computer,
    /// Open a request in the caller's own API tab (nothing is sent for it).
    ApiRequest,
    /// Run a non-destructive app window action (split, focus, sidebar...).
    AppAction,
    TestRun,
    ReportToParent {
        target: &'a str,
    },
    /// A new note under the caller (it runs nothing, so nothing is capped).
    CreateNote,
    /// Start an agent whose parent is this note Session.
    StartFromNote {
        note_session: &'a str,
    },
    /// An additive edit to a Diri note that mentions these session ids.
    WriteNote {
        mentions: &'a [String],
        /// The note's own Session, when it has one.
        note_session: Option<&'a str>,
    },
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
            "[message from id:{}, channel: dirijor — reply with send_prompt to that id]\n\n{text}",
            self.caller.id.0
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
            WriteAction::Spawn { count } => {
                self.check_fan_out(count)?;
                Relation::Unrelated
            }
            WriteAction::Manage { target } => {
                let (target_record, relation) = self.agent_target(target)?;
                if relation == Relation::Caller {
                    return Err("an agent cannot manage its own session".into());
                }
                if !self.can_release(target_record, relation) {
                    return Err(format!(
                        "manage denied: {target} is {}; root agents may manage sessions in their project, delegated agents only direct children",
                        relation.as_str()
                    ));
                }
                relation
            }
            WriteAction::Layout { target, anchor } => {
                let (target_record, relation) = self.target(target)?;
                let allowed = matches!(relation, Relation::Caller | Relation::Child)
                    || self.can_release(target_record, relation)
                    || (anchor && relation == Relation::Parent);
                if !allowed {
                    return Err(format!(
                        "layout denied: {target} is {}; arrange your own pane, your children's, or (root agents) sessions in your project{}",
                        relation.as_str(),
                        if anchor {
                            ", or split beside your parent"
                        } else {
                            ""
                        }
                    ));
                }
                relation
            }
            WriteAction::QuickOpenInclude
            | WriteAction::Browser
            | WriteAction::Computer
            | WriteAction::ApiRequest
            | WriteAction::AppAction
            | WriteAction::TestRun => Relation::Unrelated,
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
                let (target_record, relation) = self.agent_target(target)?;
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
                let (target_record, relation) = self.agent_target(target)?;
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
            WriteAction::WriteNote {
                mentions,
                note_session,
            } => {
                // Notes are the user's. Root agents act for the user; a
                // delegated agent may add only to notes about its own line:
                // the note it descends from, or one that mentions it or an
                // ancestor.
                let line: Vec<&SessionRecord> = std::iter::once(self.caller)
                    .chain(self.lineage.ancestors(&self.caller.id.0))
                    .collect();
                let reachable = line.iter().any(|record| {
                    mentions.contains(&record.id.0) || note_session == Some(record.id.0.as_str())
                });
                if !self.is_root() && !reachable {
                    return Err(
                        "write_note denied: delegated sessions may write only to the note they were started from or notes that mention them or one of their ancestors"
                            .into(),
                    );
                }
                Relation::Unrelated
            }
            WriteAction::CreateNote => Relation::Unrelated,
            WriteAction::StartFromNote { note_session } => {
                let (note, _) = self.target(note_session)?;
                if !note.is_note() {
                    return Err(format!("{note_session} is not a note"));
                }
                let from_own_line = self
                    .lineage
                    .ancestors(&self.caller.id.0)
                    .iter()
                    .any(|record| record.id.0 == note_session);
                if !self.is_root() && !from_own_line {
                    return Err(
                        "start_from_note denied: a delegated session may start work only from the note it was started from"
                            .into(),
                    );
                }
                self.check_depth()?;
                self.check_live_children(note_session, 1)?;
                Relation::Unrelated
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

    /// Recursive delegation must terminate and a single orchestrator must not
    /// flood the machine. Both limits count live (unexited, unarchived) state.
    fn check_fan_out(&self, count: usize) -> Result<(), String> {
        self.check_depth()?;
        self.check_live_children(&self.caller.id.0, count)
    }

    fn check_depth(&self) -> Result<(), String> {
        // Notes are where the user starts work, not delegation levels.
        let depth = self
            .lineage
            .ancestors(&self.caller.id.0)
            .into_iter()
            .filter(|record| !record.is_note())
            .count();
        let max_depth = limit("DIRIJOR_MAX_SPAWN_DEPTH", DEFAULT_MAX_SPAWN_DEPTH);
        if depth >= max_depth {
            return Err(format!(
                "spawn denied: this session is at delegation depth {depth} (limit {max_depth}); do the work here or report back to your parent"
            ));
        }
        Ok(())
    }

    /// Notes run nothing, so they never count against the limit.
    fn check_live_children(&self, parent: &str, count: usize) -> Result<(), String> {
        let live = self
            .lineage
            .children(parent)
            .into_iter()
            .filter(|child| {
                !child.is_note()
                    && !child.is_archived()
                    && !matches!(child.status, SessionStatus::Exited(_))
            })
            .count();
        let max_live = limit("DIRIJOR_MAX_LIVE_CHILDREN", DEFAULT_MAX_LIVE_CHILDREN);
        if live + count > max_live {
            return Err(format!(
                "spawn denied: {live} live children plus {count} new would exceed the limit of {max_live}; release or wait for finished children first"
            ));
        }
        Ok(())
    }

    fn target(&self, target: &str) -> Result<(&'a SessionRecord, Relation), String> {
        let record = self
            .lineage
            .record(target)
            .ok_or_else(|| format!("no such session: {target}"))?;
        Ok((record, self.lineage.relation(target)))
    }

    /// A note has no terminal: nothing can be typed into, managed, or
    /// released there. Reports to a parent note are handled by the caller.
    fn agent_target(&self, target: &str) -> Result<(&'a SessionRecord, Relation), String> {
        let (record, relation) = self.target(target)?;
        if record.is_note() {
            return Err(format!(
                "{target} is a note, not an agent: read it with read_note and add to it with write_note"
            ));
        }
        Ok((record, relation))
    }

    /// Started by the user: no parent, or a note the user started it from.
    fn is_root(&self) -> bool {
        match &self.caller.parent {
            None => true,
            Some(parent) => self
                .lineage
                .record(&parent.0)
                .is_some_and(SessionRecord::is_note),
        }
    }

    fn same_project(&self, target: &SessionRecord) -> bool {
        self.caller.project_id == target.project_id
    }

    fn can_message(&self, target: &SessionRecord, relation: Relation) -> bool {
        if self.is_root() {
            relation == Relation::Child || self.same_project(target)
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

pub(super) fn same_path(left: &str, right: &str) -> bool {
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
            attention_state: None,
            id: SessionId::new(id),
            kind: AgentKind::CODEX,
            cwd: format!("/tmp/{project}"),
            project_id: ProjectId::new(project),
            worktree_path: None,
            git_branch: None,
            title: id.into(),
            title_source: TitleSource::Placeholder,
            account_profile: None,
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
            remote_connection: None,
            hibernation: None,
            memory_bytes: None,
            artifacts: None,
            pull_requests: None,
            listening_ports: None,
            foreground_agent: None,
            terminal_cwd: None,
            agent_workspace: None,
            note_id: None,
            foreground_ports: None,
            terminal_progress: None,
            scheduled_run: None,
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
            system_restart: false,
            interrupted: false,
        });
        let records = vec![record("root", "p", None), exited];
        let projects = vec![project("p")];
        assert!(McpPolicy::new(&records, &projects, None).is_err());
        assert!(McpPolicy::new(&records, &projects, Some("gone")).is_err());
        assert!(McpPolicy::new(&records, &projects, Some("exited")).is_err());
    }

    #[test]
    fn layout_reaches_own_children_and_parent_anchor_only() {
        let records = vec![
            record("root", "p", None),
            record("child", "p", Some("root")),
            record("sibling", "p", Some("root")),
            record("grandchild", "p", Some("child")),
            record("foreign", "q", None),
        ];
        let projects = vec![project("p"), project("q")];
        let layout = |caller: &str, target: &str, anchor: bool| {
            McpPolicy::new(&records, &projects, Some(caller))
                .unwrap()
                .authorize(WriteAction::Layout { target, anchor })
                .is_ok()
        };
        // A root agent arranges itself and its project, never a foreign one.
        assert!(layout("root", "root", false));
        assert!(layout("root", "sibling", false));
        assert!(!layout("root", "foreign", false));
        assert!(!layout("root", "foreign", true));
        // A delegated agent: itself and its children; its parent only as
        // the pane it splits beside.
        assert!(layout("child", "child", false));
        assert!(layout("child", "grandchild", false));
        assert!(layout("child", "root", true));
        assert!(!layout("child", "root", false));
        assert!(!layout("child", "sibling", true));
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
    fn roots_can_coordinate_direct_children_across_hosts_but_not_foreign_sessions() {
        let mut child = record("child", "remote-project", Some("root"));
        child.host = Some("forge".into());
        let records = vec![
            record("root", "local-project", None),
            child,
            record("grandchild", "remote-project", Some("child")),
            record("foreign", "remote-project", None),
        ];
        let projects = vec![project("local-project"), project("remote-project")];
        let policy = McpPolicy::new(&records, &projects, Some("root")).expect("policy");
        let message = policy
            .authorize(WriteAction::SendPrompt { target: "child" })
            .expect("direct child is writable across projects");
        assert_eq!(message.frame("follow up"), "follow up");
        assert!(
            policy
                .authorize(WriteAction::Release { target: "child" })
                .is_err()
        );
        for target in ["root", "foreign", "grandchild"] {
            assert!(
                policy
                    .authorize(WriteAction::SendPrompt { target })
                    .is_err(),
                "{target}"
            );
            assert!(
                policy.authorize(WriteAction::Release { target }).is_err(),
                "{target}"
            );
        }
        let child_policy =
            McpPolicy::new(&records, &projects, Some("child")).expect("child policy");
        assert!(
            child_policy
                .authorize(WriteAction::SendPrompt { target: "root" })
                .is_ok()
        );
        assert!(
            child_policy
                .authorize(WriteAction::ReportToParent { target: "root" })
                .is_ok()
        );
        assert!(
            child_policy
                .authorize(WriteAction::Release { target: "root" })
                .is_err()
        );
        assert!(
            child_policy
                .authorize(WriteAction::SendPrompt { target: "foreign" })
                .is_err()
        );
        let grandchild_policy =
            McpPolicy::new(&records, &projects, Some("grandchild")).expect("grandchild policy");
        assert!(
            grandchild_policy
                .authorize(WriteAction::SendPrompt { target: "root" })
                .is_err()
        );
        assert!(
            grandchild_policy
                .authorize(WriteAction::Release { target: "root" })
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

        assert!(policy.authorize(WriteAction::Spawn { count: 1 }).is_ok());
        assert!(policy.authorize(WriteAction::QuickOpenInclude).is_ok());
        assert!(policy.authorize(WriteAction::Browser).is_ok());
        assert!(policy.authorize(WriteAction::Computer).is_ok());
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

    #[test]
    fn delegation_depth_and_live_fan_out_are_bounded() {
        let mut records = vec![
            record("root", "p", None),
            record("a", "p", Some("root")),
            record("b", "p", Some("a")),
            record("c", "p", Some("b")),
        ];
        let projects = vec![project("p")];
        let deep = McpPolicy::new(&records, &projects, Some("c")).expect("policy");
        assert!(deep.authorize(WriteAction::Spawn { count: 1 }).is_err());
        let middle = McpPolicy::new(&records, &projects, Some("b")).expect("policy");
        assert!(middle.authorize(WriteAction::Spawn { count: 1 }).is_ok());

        for index in 0..DEFAULT_MAX_LIVE_CHILDREN - 1 {
            records.push(record(&format!("kid{index}"), "p", Some("root")));
        }
        let root = McpPolicy::new(&records, &projects, Some("root")).expect("policy");
        assert!(root.authorize(WriteAction::Spawn { count: 1 }).is_err());
        assert!(root.authorize(WriteAction::Manage { target: "c" }).is_ok());
        assert!(
            root.authorize(WriteAction::Manage { target: "root" })
                .is_err()
        );
        let delegated = McpPolicy::new(&records, &projects, Some("a")).expect("policy");
        assert!(
            delegated
                .authorize(WriteAction::Manage { target: "b" })
                .is_ok()
        );
        assert!(
            delegated
                .authorize(WriteAction::Manage { target: "c" })
                .is_err()
        );
    }
}
