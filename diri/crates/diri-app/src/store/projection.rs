use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use diri_proto::{Project, ProjectId, SessionId, SessionRecord};

use super::{Prefs, is_auxiliary_terminal};

/// Rank given to an item the manual order has never seen. Reconciliation
/// normally appends every live id (see [`super::SessionStore::reconcile_sidebar_order`]),
/// so this only applies to stores built straight from a fixture — and the
/// tie-breaks below then reproduce exactly the order reconciliation would have
/// written, which is what lets both paths agree.
const UNRANKED: usize = usize::MAX;

/// One rendered session line inside a project group.
#[derive(Clone, Debug, PartialEq)]
pub struct SidebarRow {
    pub session: Arc<SessionRecord>,
    /// Nesting level inside the group. Zero is a session a human started;
    /// deeper rows were spawned by an ancestor through the MCP tools.
    pub depth: u16,
    /// This row has children, whether or not they are currently shown.
    pub has_children: bool,
    /// This row's subtree is folded away.
    pub collapsed: bool,
    pub pinned: bool,
    /// One bit per indent column: set when that column's rail continues past
    /// this row, i.e. the ancestor owning it still has siblings below. The
    /// column directly parenting this row is bit `depth - 1`, so a last child
    /// leaves it clear and the rail stops on its elbow.
    pub rails: u32,
}

impl SidebarRow {
    pub fn id(&self) -> &SessionId {
        &self.session.id
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct SidebarProject {
    pub project: Project,
    /// Execution location shared by every Session in this project node.
    /// `None` means local; a host id means the remote filesystem owns root.
    pub host: Option<String>,
    /// Visible rows in tree order. Subtrees under a collapsed row are omitted,
    /// and the list is empty while the project itself is collapsed.
    pub sessions: Vec<SidebarRow>,
    /// Every active session in the group, in the same tree order, regardless of
    /// what is folded away. Rollups and selection ranges read this.
    pub active: Vec<Arc<SessionRecord>>,
    pub archived: Vec<Arc<SessionRecord>>,
    pub pinned: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct SidebarProjection {
    pub projects: Vec<SidebarProject>,
    /// Flat ⌘1…⌘9 order over rows the user can actually see. Archived rows are
    /// omitted unless selected.
    pub ordered_sessions: Vec<Arc<SessionRecord>>,
    /// Active then archived rows per project, regardless of what is collapsed.
    pub display_order: Vec<SessionId>,
}

impl SidebarProjection {
    /// The row an empty selection should fall back to: the first active
    /// session in sidebar order, whether or not its project is collapsed.
    /// Selecting it unfolds whatever is hiding it (`SessionStore::reveal`).
    pub fn first_active(&self) -> Option<&Arc<SessionRecord>> {
        self.projects.iter().find_map(|group| group.active.first())
    }
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

    let session_rank = rank_map(&prefs.sidebar_session_order);
    let project_rank = rank_map(&prefs.sidebar_project_order);
    let pinned_sessions: HashSet<&SessionId> = prefs.sidebar_pinned_sessions.iter().collect();
    let pinned_projects: HashSet<&ProjectId> = prefs.sidebar_pinned_projects.iter().collect();
    let collapsed_sessions: HashSet<&SessionId> = prefs.sidebar_collapsed_sessions.iter().collect();
    let collapsed_projects: HashSet<&ProjectId> = prefs.sidebar_collapsed_projects.iter().collect();

    let mut ranked = Vec::with_capacity(grouped.len());
    for (project_id, group) in grouped {
        let project = projects
            .get(&project_id)
            .cloned()
            .unwrap_or_else(|| synthetic_project(&project_id, &group));
        let host = group.first().and_then(|session| session.host.clone());
        // A project is as old as its oldest session. That is the arrival order
        // a first-time user perceives, and it keeps a project from jumping
        // around as its sessions come and go.
        let arrival = group
            .iter()
            .map(|session| session.created_at.0)
            .fold(f64::INFINITY, f64::min);
        let (mut archived, active): (Vec<_>, Vec<_>) =
            group.into_iter().partition(|session| session.is_archived());
        // Most recently archived first: the bucket is a recovery surface, and
        // the thing you just put away is the thing you are most likely after.
        archived.sort_by(|left, right| {
            right
                .archived_at
                .partial_cmp(&left.archived_at)
                .unwrap_or(Ordering::Equal)
                .then_with(|| left.id.0.cmp(&right.id.0))
        });
        let expanded = !collapsed_projects.contains(&project_id);
        let (sessions, active) = build_tree(
            active,
            &session_rank,
            &pinned_sessions,
            &collapsed_sessions,
            expanded,
        );
        ranked.push((
            arrival,
            SidebarProject {
                pinned: pinned_projects.contains(&project.id),
                project,
                host,
                sessions,
                active,
                archived,
            },
        ));
    }

    ranked.sort_by(|(left_arrival, left), (right_arrival, right)| {
        // Pinned projects lead, then the manual order, then arrival. The last
        // two agree by construction, so a project keeps its place whether or
        // not the manual order has been materialised yet.
        right
            .pinned
            .cmp(&left.pinned)
            .then_with(|| {
                rank_of(&project_rank, &left.project.id)
                    .cmp(&rank_of(&project_rank, &right.project.id))
            })
            .then_with(|| left_arrival.total_cmp(right_arrival))
            .then_with(|| left.project.id.0.cmp(&right.project.id.0))
    });
    let result: Vec<SidebarProject> = ranked.into_iter().map(|(_, group)| group).collect();

    let display_order = result
        .iter()
        .flat_map(|group| {
            group
                .active
                .iter()
                .chain(&group.archived)
                .map(|session| session.id.clone())
        })
        .collect();
    let ordered_sessions = result
        .iter()
        .flat_map(|group| {
            group
                .sessions
                .iter()
                .map(|row| &row.session)
                .chain(
                    group
                        .archived
                        .iter()
                        .filter(|session| selected == Some(&session.id)),
                )
                .cloned()
        })
        .collect();

    SidebarProjection {
        projects: result,
        ordered_sessions,
        display_order,
    }
}

/// Arranges one project's active sessions into the lineage forest the sidebar
/// draws, and returns `(visible rows, every session in tree order)`.
fn build_tree(
    active: Vec<Arc<SessionRecord>>,
    session_rank: &HashMap<&SessionId, usize>,
    pinned: &HashSet<&SessionId>,
    collapsed: &HashSet<&SessionId>,
    project_expanded: bool,
) -> (Vec<SidebarRow>, Vec<Arc<SessionRecord>>) {
    let parents = resolve_parents(&active);
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); active.len()];
    let mut roots = Vec::new();
    for (index, parent) in parents.iter().enumerate() {
        match parent {
            Some(parent) => children[*parent].push(index),
            None => roots.push(index),
        }
    }

    let order = |left: &usize, right: &usize| {
        sibling_cmp(&active[*left], &active[*right], session_rank, pinned)
    };
    roots.sort_by(order);
    for list in &mut children {
        list.sort_by(order);
    }

    let mut rows = Vec::with_capacity(active.len());
    let mut tree_order = Vec::with_capacity(active.len());
    // Explicit stack rather than recursion: a spawn chain is attacker-shaped
    // input in the sense that nothing in the daemon bounds its depth.
    let mut stack: Vec<(usize, u16, bool, u32)> = Vec::with_capacity(active.len());
    for index in roots.iter().rev() {
        stack.push((*index, 0, project_expanded, 0));
    }
    while let Some((index, depth, visible, rails)) = stack.pop() {
        let session = &active[index];
        tree_order.push(Arc::clone(session));
        let has_children = !children[index].is_empty();
        let is_collapsed = has_children && collapsed.contains(&session.id);
        if visible {
            rows.push(SidebarRow {
                session: Arc::clone(session),
                depth,
                has_children,
                collapsed: is_collapsed,
                pinned: pinned.contains(&session.id),
                rails,
            });
        }
        let children_visible = visible && !is_collapsed;
        let count = children[index].len();
        // Children inherit this row's rails and light the column beside it,
        // except for the last one — whose rail stops on its own elbow.
        let inherited = rails | (1u32 << depth.min(31));
        for (position, child) in children[index].iter().enumerate().rev() {
            let child_rails = if position + 1 == count {
                rails
            } else {
                inherited
            };
            stack.push((*child, depth + 1, children_visible, child_rails));
        }
    }
    (rows, tree_order)
}

/// Maps each session to the index of its parent *within this project group*.
///
/// A parent that is archived, closing, in another project, or simply gone
/// leaves the child at the root — a session must never vanish because the
/// agent that spawned it did. Cycles cannot come from a well-behaved daemon,
/// but one would hang the flatten below, so they are broken here.
fn resolve_parents(active: &[Arc<SessionRecord>]) -> Vec<Option<usize>> {
    let index: HashMap<&SessionId, usize> = active
        .iter()
        .enumerate()
        .map(|(index, session)| (&session.id, index))
        .collect();
    let mut parents: Vec<Option<usize>> = active
        .iter()
        .enumerate()
        .map(|(position, session)| {
            session
                .parent
                .as_ref()
                .and_then(|parent| index.get(parent).copied())
                .filter(|parent| *parent != position)
        })
        .collect();
    for start in 0..parents.len() {
        let mut seen = HashSet::from([start]);
        let mut cursor = parents[start];
        while let Some(node) = cursor {
            if !seen.insert(node) {
                parents[start] = None;
                break;
            }
            cursor = parents[node];
        }
    }
    parents
}

/// Orders one run of siblings: pinned first, then the manual order, then
/// arrival. New sessions carry the newest `created_at`, so they land last.
fn sibling_cmp(
    left: &SessionRecord,
    right: &SessionRecord,
    ranks: &HashMap<&SessionId, usize>,
    pinned: &HashSet<&SessionId>,
) -> Ordering {
    pinned
        .contains(&right.id)
        .cmp(&pinned.contains(&left.id))
        .then_with(|| rank_of(ranks, &left.id).cmp(&rank_of(ranks, &right.id)))
        .then_with(|| left.created_at.0.total_cmp(&right.created_at.0))
        .then_with(|| left.id.0.cmp(&right.id.0))
}

fn rank_map<T: Eq + std::hash::Hash>(order: &[T]) -> HashMap<&T, usize> {
    order
        .iter()
        .enumerate()
        .map(|(rank, id)| (id, rank))
        .collect()
}

fn rank_of<T: Eq + std::hash::Hash>(ranks: &HashMap<&T, usize>, id: &T) -> usize {
    ranks.get(id).copied().unwrap_or(UNRANKED)
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
