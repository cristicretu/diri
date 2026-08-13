//! Menu-bar list model built from the same sidebar projection as the main app.
//!
//! Project collapse, spawn depth, and row order stay in lockstep with the
//! sidebar so the menubar is a compact mirror rather than a second ranking.

use diri_proto::{AttentionLevel, SessionRecord};

use crate::store::{SidebarProject, SidebarProjection};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TrailingStatus {
    NeedsYou,
    Done,
    Zzz,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InboxSessionRow {
    pub session_id: String,
    pub title: String,
    pub agent_id: String,
    pub depth: u16,
    pub trailing: Option<TrailingStatus>,
    pub working: bool,
    pub destructive: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InboxRow {
    Project {
        id: String,
        name: String,
        collapsed: bool,
    },
    Session(InboxSessionRow),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InboxModel {
    pub rows: Vec<InboxRow>,
}

pub fn build_inbox(
    projection: &SidebarProjection,
    collapsed_projects: &std::collections::HashSet<String>,
) -> InboxModel {
    let mut rows = Vec::new();
    for group in &projection.projects {
        if group.active.is_empty() {
            continue;
        }
        rows.extend(project_rows(group, collapsed_projects));
    }
    InboxModel { rows }
}

fn project_rows(
    group: &SidebarProject,
    collapsed_projects: &std::collections::HashSet<String>,
) -> Vec<InboxRow> {
    let collapsed = collapsed_projects.contains(group.project.id.0.as_str());
    let mut rows = vec![InboxRow::Project {
        id: group.project.id.0.clone(),
        name: group.project.name.clone(),
        collapsed,
    }];
    if collapsed {
        return rows;
    }
    for row in &group.sessions {
        rows.push(InboxRow::Session(session_row(
            row.session.as_ref(),
            row.depth,
        )));
    }
    rows
}

fn session_row(session: &SessionRecord, depth: u16) -> InboxSessionRow {
    let hibernated = session.hibernation.is_some();
    let attention = session.attention();
    let trailing = if hibernated {
        Some(TrailingStatus::Zzz)
    } else {
        match attention {
            AttentionLevel::NeedsInput => Some(TrailingStatus::NeedsYou),
            AttentionLevel::DoneUnseen => Some(TrailingStatus::Done),
            _ => None,
        }
    };
    let destructive = session
        .needs_input
        .as_ref()
        .is_some_and(|detail| detail.risk_hint == diri_proto::RiskHint::Destructive);
    InboxSessionRow {
        session_id: session.id.0.clone(),
        title: display_title(session).to_owned(),
        agent_id: session.effective_kind().id().to_owned(),
        depth,
        trailing,
        working: !hibernated
            && !session.effective_kind().is_terminal()
            && attention == AttentionLevel::Working,
        destructive,
    }
}

fn display_title(session: &SessionRecord) -> &str {
    if session.title.is_empty() {
        "Untitled"
    } else {
        &session.title
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use diri_proto::{
        AgentKind, DateMillis, NeedsInputDetail, NeedsInputKind, NeedsInputSource, Project,
        ProjectId, RiskHint, SessionId, SessionStatus,
    };

    use crate::store::SidebarRow;

    fn session(
        id: &str,
        project: &str,
        title: &str,
        kind: AgentKind,
        status: SessionStatus,
    ) -> Arc<SessionRecord> {
        Arc::new(SessionRecord {
            id: SessionId::new(id),
            kind,
            cwd: "/tmp".into(),
            project_id: ProjectId::new(project),
            worktree_path: None,
            git_branch: None,
            title: title.into(),
            title_source: diri_proto::TitleSource::Placeholder,
            originating_prompt: None,
            agent_session_id: None,
            transcript_path: None,
            status,
            status_evidence: None,
            needs_input: None,
            resumability: diri_proto::Resumability::Live,
            parent: None,
            created_at: DateMillis(1.0),
            updated_at: DateMillis(1.0),
            last_turn_completed_at: None,
            last_seen_at: Some(DateMillis(0.0)),
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
        })
    }

    fn row(session: Arc<SessionRecord>, depth: u16) -> SidebarRow {
        SidebarRow {
            session,
            depth,
            has_children: false,
            collapsed: false,
            pinned: false,
            rails: 0,
        }
    }

    fn group(
        name: &str,
        id: &str,
        visible: Vec<SidebarRow>,
        active: Vec<Arc<SessionRecord>>,
    ) -> SidebarProject {
        SidebarProject {
            project: Project {
                id: ProjectId::new(id),
                root: format!("/{id}"),
                name: name.into(),
                pinned_order: None,
                host: None,
            },
            host: None,
            sessions: visible,
            active,
            archived: Vec::new(),
            pinned: false,
        }
    }

    #[test]
    fn projects_with_no_active_sessions_are_dropped_entirely() {
        let projection = SidebarProjection {
            projects: vec![group("robite-landing", "robite", Vec::new(), Vec::new())],
            ordered_sessions: Vec::new(),
            display_order: Vec::new(),
            fleet_pulse: Default::default(),
        };
        let model = build_inbox(&projection, &std::collections::HashSet::new());
        assert!(model.rows.is_empty());
    }

    #[test]
    fn menubar_collapse_hides_sessions_without_requiring_sidebar_fold() {
        let child = session(
            "a",
            "robite",
            "Cursor Agent",
            AgentKind::CURSOR,
            SessionStatus::Idle,
        );
        let projection = SidebarProjection {
            projects: vec![group(
                "robite-landing",
                "robite",
                vec![row(Arc::clone(&child), 0)],
                vec![child],
            )],
            ordered_sessions: Vec::new(),
            display_order: Vec::new(),
            fleet_pulse: Default::default(),
        };
        let collapsed = std::collections::HashSet::from(["robite".to_owned()]);
        let model = build_inbox(&projection, &collapsed);
        assert_eq!(model.rows.len(), 1);
        assert!(matches!(
            &model.rows[0],
            InboxRow::Project {
                collapsed: true,
                ..
            }
        ));
    }

    #[test]
    fn preserves_spawn_depth_and_zzz_trailing() {
        let mut asleep = session(
            "child",
            "robite",
            "Nested",
            AgentKind::CLAUDE_CODE,
            SessionStatus::Idle,
        );
        {
            let session = Arc::make_mut(&mut asleep);
            session.hibernation = Some(diri_proto::HibernationInfo {
                since: DateMillis(1.0),
                reason: diri_proto::HibernationReason::Idle,
                tree_pids: vec![1],
                tree_start_times: None,
            });
        }
        let parent = session(
            "parent",
            "robite",
            "Root",
            AgentKind::CURSOR,
            SessionStatus::Working,
        );
        let projection = SidebarProjection {
            projects: vec![group(
                "robite-landing",
                "robite",
                vec![row(Arc::clone(&parent), 0), row(Arc::clone(&asleep), 1)],
                vec![parent, asleep],
            )],
            ordered_sessions: Vec::new(),
            display_order: Vec::new(),
            fleet_pulse: Default::default(),
        };
        let model = build_inbox(&projection, &std::collections::HashSet::new());
        let InboxRow::Session(nested) = &model.rows[2] else {
            panic!("expected nested session");
        };
        assert_eq!(nested.depth, 1);
        assert_eq!(nested.trailing, Some(TrailingStatus::Zzz));
        let InboxRow::Session(root) = &model.rows[1] else {
            panic!("expected root session");
        };
        assert!(root.working);
    }

    #[test]
    fn terminal_sessions_are_not_marked_working() {
        let shell = session(
            "sh",
            "alex",
            "shell",
            AgentKind::SHELL,
            SessionStatus::Working,
        );
        let projection = SidebarProjection {
            projects: vec![group(
                "alex",
                "alex",
                vec![row(Arc::clone(&shell), 0)],
                vec![shell],
            )],
            ordered_sessions: Vec::new(),
            display_order: Vec::new(),
            fleet_pulse: Default::default(),
        };
        let model = build_inbox(&projection, &std::collections::HashSet::new());
        let InboxRow::Session(row) = &model.rows[1] else {
            panic!("expected shell session");
        };
        assert!(!row.working);
    }

    #[test]
    fn permission_rows_keep_needs_you_trailing() {
        let mut blocked = session(
            "a",
            "robite",
            "Blocked",
            AgentKind::CLAUDE_CODE,
            SessionStatus::Working,
        );
        {
            let session = Arc::make_mut(&mut blocked);
            session.status = SessionStatus::NeedsInput(NeedsInputKind::Permission);
            session.needs_input = Some(NeedsInputDetail {
                kind: NeedsInputKind::Permission,
                source: NeedsInputSource::ClaudePermissionHook,
                tool_name: None,
                summary: "git push".into(),
                prompt_excerpt: None,
                options: None,
                risk_hint: RiskHint::Neutral,
                occurred_at: DateMillis(1.0),
            });
        }
        let projection = SidebarProjection {
            projects: vec![group(
                "robite-landing",
                "robite",
                vec![row(Arc::clone(&blocked), 0)],
                vec![blocked],
            )],
            ordered_sessions: Vec::new(),
            display_order: Vec::new(),
            fleet_pulse: Default::default(),
        };
        let model = build_inbox(&projection, &std::collections::HashSet::new());
        let InboxRow::Session(row) = &model.rows[1] else {
            panic!("expected session");
        };
        assert_eq!(row.trailing, Some(TrailingStatus::NeedsYou));
    }
}
