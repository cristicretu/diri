//! One non-modal recovery state system for daemon connectivity and failed
//! actions. Rendering stays in `RootView`; this module owns the calm copy,
//! priority, and safe-action policy as a pure decision.

use crate::store::{ActionFailure, DaemonState};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecoveryKind {
    Connecting,
    Reconnecting,
    ManualAttention,
    ActionFailed,
    RetryingAction,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecoveryAction {
    RetryConnection,
    RetryAction,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecoveryNotice {
    pub kind: RecoveryKind,
    pub title: String,
    pub body: String,
    pub primary_action: Option<(RecoveryAction, &'static str)>,
    pub dismissible: bool,
}

impl RecoveryNotice {
    #[must_use]
    pub fn resolve(daemon: &DaemonState, failure: Option<&ActionFailure>) -> Option<Self> {
        if let Some(failure) = failure {
            let retry = failure
                .can_retry()
                .then_some((RecoveryAction::RetryAction, "Retry"));
            return Some(Self {
                kind: if failure.retrying {
                    RecoveryKind::RetryingAction
                } else {
                    RecoveryKind::ActionFailed
                },
                title: if failure.retrying {
                    format!("Retrying: {}", failure.title.trim_end_matches(" failed"))
                } else {
                    failure.title.clone()
                },
                body: if failure.retrying {
                    "Waiting for the daemon to confirm the operation.".to_owned()
                } else {
                    failure.detail.clone()
                },
                primary_action: if failure.retrying { None } else { retry },
                dismissible: !failure.retrying,
            });
        }

        match daemon {
            DaemonState::Connected => None,
            DaemonState::Connecting => Some(Self {
                kind: RecoveryKind::Connecting,
                title: "Connecting to the Diri daemon".to_owned(),
                body: "Sessions stay visible while the connection is established.".to_owned(),
                primary_action: None,
                dismissible: false,
            }),
            DaemonState::Unreachable(error) if needs_manual_attention(error) => Some(Self {
                kind: RecoveryKind::ManualAttention,
                title: "The daemon needs attention".to_owned(),
                body: "Retry the connection. If it still fails, relaunch Diri to replace the bundled daemon.".to_owned(),
                primary_action: Some((RecoveryAction::RetryConnection, "Retry now")),
                dismissible: false,
            }),
            DaemonState::Unreachable(_) => Some(Self {
                kind: RecoveryKind::Reconnecting,
                title: "Daemon unavailable".to_owned(),
                body: "Reconnecting automatically; existing sessions remain readable.".to_owned(),
                primary_action: Some((RecoveryAction::RetryConnection, "Retry now")),
                dismissible: false,
            }),
        }
    }
}

fn needs_manual_attention(error: &str) -> bool {
    let error = error.to_ascii_lowercase();
    error.contains("protocol error")
        || error.contains("authoritative rust engine")
        || error.contains("wrong protocol")
        || error.contains("identity")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn failure(retry: bool, retrying: bool) -> ActionFailure {
        ActionFailure::fixture(
            "Rename session failed",
            "request timed out",
            retry,
            retrying,
        )
    }

    #[test]
    fn state_gallery_covers_connecting_unreachable_failure_retry_and_recovery() {
        let connecting = RecoveryNotice::resolve(&DaemonState::Connecting, None).unwrap();
        assert_eq!(connecting.kind, RecoveryKind::Connecting);
        assert!(connecting.primary_action.is_none());

        let reconnecting = RecoveryNotice::resolve(
            &DaemonState::Unreachable("socket temporarily absent".to_owned()),
            None,
        )
        .unwrap();
        assert_eq!(reconnecting.kind, RecoveryKind::Reconnecting);
        assert!(reconnecting.body.contains("automatically"));

        let manual = RecoveryNotice::resolve(
            &DaemonState::Unreachable("protocol error: wrong identity".to_owned()),
            None,
        )
        .unwrap();
        assert_eq!(manual.kind, RecoveryKind::ManualAttention);
        assert!(manual.body.contains("relaunch"));

        let failed = failure(true, false);
        let failed = RecoveryNotice::resolve(&DaemonState::Connected, Some(&failed)).unwrap();
        assert_eq!(failed.kind, RecoveryKind::ActionFailed);
        assert_eq!(
            failed.primary_action,
            Some((RecoveryAction::RetryAction, "Retry"))
        );

        let retrying = failure(true, true);
        let retrying = RecoveryNotice::resolve(&DaemonState::Connected, Some(&retrying)).unwrap();
        assert_eq!(retrying.kind, RecoveryKind::RetryingAction);
        assert!(retrying.primary_action.is_none());

        assert!(RecoveryNotice::resolve(&DaemonState::Connected, None).is_none());
    }

    #[test]
    fn unsafe_failed_actions_never_offer_retry() {
        let destructive = failure(false, false);
        let notice = RecoveryNotice::resolve(&DaemonState::Connected, Some(&destructive)).unwrap();
        assert!(notice.primary_action.is_none());
    }
}
