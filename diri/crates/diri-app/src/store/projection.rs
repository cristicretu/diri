use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use diri_proto::{Project, ProjectId, SessionId, SessionRecord};

use super::{Prefs, is_auxiliary_terminal};

#[derive(Clone, Debug, PartialEq)]
pub struct SidebarProject {
    pub project: Project,
    /// Execution location shared by every Session in this project node.
    /// `None` means local; a host id means the remote filesystem owns root.
    pub host: Option<String>,
    pub sessions: Vec<Arc<SessionRecord>>,
    pub archived: Vec<Arc<SessionRecord>>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct SidebarProjection {
    pub projects: Vec<SidebarProject>,
    /// Flat command-1…9 order. Archived rows are omitted unless selected.
    pub ordered_sessions: Vec<Arc<SessionRecord>>,
    /// Active then archived rows per project, regardless of archive expansion.
    pub display_order: Vec<SessionId>,
}

pub(super) fn build_projection(
    sessions: &HashMap<SessionId, Arc<SessionRecord>>,
    projects: &HashMap<ProjectId, Project>,
    prefs: &Prefs,
    selected: Option<&SessionId>,
    closing: &HashSet<SessionId>,
) -> SidebarProjection {
    let mut grouped: HashMap<ProjectId, Vec<Arc<SessionRecord>>> = HashMap::new();
    for session in sessions.values() {
        // Closing rows leave the sidebar as soon as the request is dispatched.
        // Workbench-owned terminal shells live under their primary agent and
        // are reopened there; exposing them as top-level rows would split one
        // workspace into two unrelated-looking sessions.
        if closing.contains(&session.id) || is_auxiliary_terminal(session) {
            continue;
        }
        grouped
            .entry(session.project_id.clone())
            .or_default()
            .push(Arc::clone(session));
    }

    let session_rank: HashMap<&SessionId, usize> = prefs
        .sidebar_session_order
        .iter()
        .enumerate()
        .map(|(rank, id)| (id, rank))
        .collect();
    let project_rank: HashMap<&ProjectId, usize> = prefs
        .sidebar_project_order
        .iter()
        .enumerate()
        .map(|(rank, id)| (id, rank))
        .collect();

    let mut result = Vec::with_capacity(grouped.len());
    for (project_id, group) in grouped {
        let project = projects
            .get(&project_id)
            .cloned()
            .unwrap_or_else(|| synthetic_project(&project_id, &group));
        let host = group.first().and_then(|session| session.host.clone());
        let (mut archived, mut active): (Vec<_>, Vec<_>) =
            group.into_iter().partition(|session| session.is_archived());
        active.sort_by(|left, right| {
            manual_then(&session_rank, &left.id, &right.id).unwrap_or_else(|| {
                right
                    .created_at
                    .partial_cmp(&left.created_at)
                    .unwrap_or(Ordering::Equal)
                    .then_with(|| left.id.0.cmp(&right.id.0))
            })
        });
        archived.sort_by(|left, right| {
            right
                .archived_at
                .partial_cmp(&left.archived_at)
                .unwrap_or(Ordering::Equal)
                .then_with(|| left.id.0.cmp(&right.id.0))
        });
        result.push(SidebarProject {
            project,
            host,
            sessions: active,
            archived,
        });
    }

    result.sort_by(|left, right| {
        manual_then(&project_rank, &left.project.id, &right.project.id).unwrap_or_else(|| {
            left.project
                .name
                .to_lowercase()
                .cmp(&right.project.name.to_lowercase())
                .then_with(|| left.project.id.0.cmp(&right.project.id.0))
        })
    });

    let display_order = result
        .iter()
        .flat_map(|group| group.sessions.iter().chain(&group.archived))
        .map(|session| session.id.clone())
        .collect();
    let ordered_sessions = result
        .iter()
        .flat_map(|group| {
            group.sessions.iter().chain(
                group
                    .archived
                    .iter()
                    .filter(|session| selected == Some(&session.id)),
            )
        })
        .cloned()
        .collect();

    SidebarProjection {
        projects: result,
        ordered_sessions,
        display_order,
    }
}

fn manual_then<K: Eq + std::hash::Hash>(
    ranks: &HashMap<&K, usize>,
    left: &K,
    right: &K,
) -> Option<Ordering> {
    match (ranks.get(left), ranks.get(right)) {
        (Some(left), Some(right)) => Some(left.cmp(right)),
        (Some(_), None) => Some(Ordering::Less),
        (None, Some(_)) => Some(Ordering::Greater),
        (None, None) => None,
    }
}

fn synthetic_project(id: &ProjectId, sessions: &[Arc<SessionRecord>]) -> Project {
    let sample = sessions.iter().max_by(|left, right| {
        left.created_at
            .partial_cmp(&right.created_at)
            .unwrap_or(Ordering::Equal)
    });
    let root = sample
        .and_then(|session| session.worktree_path.as_deref().or(Some(&session.cwd)))
        .unwrap_or(&id.0)
        .to_owned();
    let name = Path::new(&root)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or(&root)
        .to_owned();
    Project {
        id: id.clone(),
        root,
        name,
        pinned_order: None,
    }
}
