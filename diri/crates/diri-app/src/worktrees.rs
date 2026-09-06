//! State and safe cleanup flow for the worktrees sheet.

use diri_proto::{SessionReparentWorktreeParams, WorktreeCleanupParams, WorktreeOverviewEntry};

use crate::delegation::WorktreeMoveProposal;

#[derive(Clone, Debug, Default)]
pub struct WorktreesSheet {
    pub entries: Vec<WorktreeOverviewEntry>,
    pub loading: bool,
    pub cleanup_only: bool,
    pub old_only: bool,
    pub pending_cleanup: Option<WorktreeOverviewEntry>,
    pub pending_move: Option<WorktreeMoveProposal>,
    pub move_refusal: Option<String>,
    pub error: Option<String>,
}

impl WorktreesSheet {
    pub fn begin_refresh(&mut self) {
        self.pending_cleanup = None;
        self.loading = true;
        self.error = None;
    }

    pub fn finish_refresh(&mut self, result: Result<Vec<WorktreeOverviewEntry>, String>) {
        self.loading = false;
        match result {
            Ok(mut entries) => {
                entries.sort_by(|left, right| {
                    left.project_root
                        .cmp(&right.project_root)
                        .then_with(|| left.branch.cmp(&right.branch))
                        .then_with(|| left.path.cmp(&right.path))
                });
                for entry in &mut entries {
                    if entry.health.head.is_none() {
                        entry.stale_suggestion = false;
                        entry.health.protection =
                            Some("Update the engine to inspect cleanup safety".into());
                        entry.health.pr_state = "Unavailable".into();
                    }
                }
                self.entries = entries;
                self.error = None;
            }
            Err(error) => self.error = Some(error),
        }
    }

    /// Only an engine-verified candidate can reach confirmation.
    pub fn request_cleanup(&mut self, path: &str) -> bool {
        if self.loading {
            return false;
        }
        let Some(entry) = self
            .entries
            .iter()
            .find(|entry| {
                entry.path == path && entry.stale_suggestion && entry.health.head.is_some()
            })
            .cloned()
        else {
            return false;
        };
        self.pending_cleanup = Some(entry);
        true
    }

    pub fn cancel_cleanup(&mut self) {
        self.pending_cleanup = None;
    }

    pub fn confirm_cleanup(&mut self) -> Option<WorktreeCleanupParams> {
        let entry = self.pending_cleanup.take()?;
        Some(WorktreeCleanupParams {
            repo_path: entry.project_root,
            worktree_path: entry.path,
            expected_head: entry.health.head?,
        })
    }

    pub fn propose_move(&mut self, result: Result<WorktreeMoveProposal, String>) {
        match result {
            Ok(proposal) => {
                self.pending_move = Some(proposal);
                self.move_refusal = None;
            }
            Err(reason) => {
                self.pending_move = None;
                self.move_refusal = Some(reason);
            }
        }
    }

    pub fn cancel_move(&mut self) {
        self.pending_move = None;
        self.move_refusal = None;
    }

    pub fn confirm_move(&mut self) -> Option<SessionReparentWorktreeParams> {
        let proposal = self.pending_move.take()?;
        self.move_refusal = None;
        Some(SessionReparentWorktreeParams {
            session_id: proposal.source_id,
            project_root: proposal.project_root,
            worktree_path: proposal.worktree_path,
        })
    }
}

#[cfg(test)]
mod tests {
    use diri_proto::SessionStatus;

    use super::*;

    fn entry(path: &str, stale: bool) -> WorktreeOverviewEntry {
        WorktreeOverviewEntry {
            path: path.to_owned(),
            branch: Some("feature".to_owned()),
            project_root: "/repo".to_owned(),
            session_id: None,
            session_status: None::<SessionStatus>,
            dirty: false,
            merged: true,
            age_days: 20,
            stale_suggestion: stale,
            health: diri_proto::WorktreeHealth {
                head: Some("abc".into()),
                ..Default::default()
            },
        }
    }

    #[test]
    fn cleanup_requires_stale_suggestion_and_confirmation() {
        let mut sheet = WorktreesSheet {
            entries: vec![entry("/repo/main", false), entry("/repo/feature", true)],
            ..WorktreesSheet::default()
        };
        assert!(!sheet.request_cleanup("/repo/main"));
        assert!(sheet.request_cleanup("/repo/feature"));
        let params = sheet.confirm_cleanup().unwrap();
        assert_eq!(params.repo_path, "/repo");
        assert_eq!(params.worktree_path, "/repo/feature");
        assert_eq!(params.expected_head, "abc");
    }

    #[test]
    fn cancelling_cleanup_never_builds_a_removal() {
        let mut sheet = WorktreesSheet {
            entries: vec![entry("/repo/feature", true)],
            ..WorktreesSheet::default()
        };
        assert!(sheet.request_cleanup("/repo/feature"));
        sheet.cancel_cleanup();
        assert!(sheet.confirm_cleanup().is_none());
    }

    #[test]
    fn refresh_invalidates_confirmation_and_old_engines_cannot_cleanup() {
        let mut sheet = WorktreesSheet {
            entries: vec![entry("/repo/feature", true)],
            ..Default::default()
        };
        assert!(sheet.request_cleanup("/repo/feature"));
        sheet.begin_refresh();
        assert!(sheet.confirm_cleanup().is_none());
        assert!(!sheet.request_cleanup("/repo/feature"));
        let mut old = entry("/repo/feature", true);
        old.health = Default::default();
        sheet.finish_refresh(Ok(vec![old]));
        assert!(!sheet.request_cleanup("/repo/feature"));
    }

    #[test]
    fn worktree_moves_require_a_separate_confirmation() {
        let mut sheet = WorktreesSheet::default();
        sheet.propose_move(Ok(WorktreeMoveProposal {
            source_id: diri_proto::SessionId::new("source"),
            source_title: "Source".to_owned(),
            project_root: "/repo".to_owned(),
            worktree_path: "/repo-feature".to_owned(),
            branch: Some("feature".to_owned()),
        }));
        assert!(sheet.pending_move.is_some());
        assert_eq!(
            sheet.confirm_move(),
            Some(SessionReparentWorktreeParams {
                session_id: diri_proto::SessionId::new("source"),
                project_root: "/repo".to_owned(),
                worktree_path: "/repo-feature".to_owned(),
            })
        );
        assert!(sheet.confirm_move().is_none());
    }

    #[test]
    fn cancelling_a_worktree_proposal_leaves_no_mutation() {
        let mut sheet = WorktreesSheet::default();
        sheet.propose_move(Err("session is still running".to_owned()));
        assert_eq!(
            sheet.move_refusal.as_deref(),
            Some("session is still running")
        );
        sheet.cancel_move();
        assert!(sheet.pending_move.is_none());
        assert!(sheet.move_refusal.is_none());
        assert!(sheet.confirm_move().is_none());
    }
}
