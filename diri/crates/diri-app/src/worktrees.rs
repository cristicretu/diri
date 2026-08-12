//! State and safe cleanup flow for the worktrees sheet.

use diri_proto::{SessionReparentWorktreeParams, WorktreeOverviewEntry, WorktreeRemoveParams};

use crate::delegation::WorktreeMoveProposal;

#[derive(Clone, Debug, Default)]
pub struct WorktreesSheet {
    pub entries: Vec<WorktreeOverviewEntry>,
    pub loading: bool,
    pub pending_cleanup: Option<WorktreeOverviewEntry>,
    pub pending_move: Option<WorktreeMoveProposal>,
    pub move_refusal: Option<String>,
    pub error: Option<String>,
}

impl WorktreesSheet {
    pub fn begin_refresh(&mut self) {
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
                self.entries = entries;
                self.error = None;
            }
            Err(error) => self.error = Some(error),
        }
    }

    /// Cleanup is suggest-only, matching the Swift sheet. A dirty, unmerged,
    /// main, or otherwise non-stale worktree cannot reach confirmation.
    pub fn request_cleanup(&mut self, path: &str) -> bool {
        let Some(entry) = self
            .entries
            .iter()
            .find(|entry| entry.path == path && entry.stale_suggestion)
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

    pub fn confirm_cleanup(&mut self) -> Option<WorktreeRemoveParams> {
        let entry = self.pending_cleanup.take()?;
        Some(WorktreeRemoveParams {
            repo_path: entry.project_root,
            worktree_path: entry.path,
            force: false,
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
        assert!(!params.force);
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
