//! One non-modal recovery state system for daemon connectivity and failed
//! actions. Rendering stays in `RootView`; this module owns the calm copy,
//! priority, and safe-action policy as a pure decision.

use crate::i18n::{t, tf};
use crate::store::{ActionFailure, DaemonState};
use crate::toast::{Toast, ToastCommand, ToastTone};

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
    pub detail: Option<String>,
    pub primary_action: Option<(RecoveryAction, &'static str)>,
    pub dismissible: bool,
}

impl RecoveryNotice {
    #[must_use]
    pub fn resolve(daemon: &DaemonState, failure: Option<&ActionFailure>) -> Option<Self> {
        if let Some(failure) = failure {
            let retry = failure
                .can_retry()
                .then_some((RecoveryAction::RetryAction, t("toast.action_retry")));
            return Some(Self {
                kind: if failure.retrying {
                    RecoveryKind::RetryingAction
                } else {
                    RecoveryKind::ActionFailed
                },
                title: if failure.retrying {
                    tf(
                        "toast.retrying",
                        &[("action", &failure.title.trim_end_matches(" failed"))],
                    )
                } else {
                    failure.title.clone()
                },
                body: if failure.retrying {
                    t("toast.retry_waiting").to_owned()
                } else {
                    failure.detail.clone()
                },
                detail: Some(failure.detail.clone()),
                primary_action: if failure.retrying { None } else { retry },
                dismissible: !failure.retrying,
            });
        }

        match daemon {
            DaemonState::Connected => None,
            DaemonState::Connecting => Some(Self {
                kind: RecoveryKind::Connecting,
                title: t("toast.connecting").to_owned(),
                body: t("toast.connecting_body").to_owned(),
                detail: None,
                primary_action: None,
                dismissible: false,
            }),
            DaemonState::Unreachable(error) if needs_manual_attention(error) => Some(Self {
                kind: RecoveryKind::ManualAttention,
                title: t("toast.engine_unreachable").to_owned(),
                body: t("toast.engine_unreachable_body").to_owned(),
                detail: None,
                primary_action: Some((
                    RecoveryAction::RetryConnection,
                    t("toast.action_retry_now"),
                )),
                dismissible: false,
            }),
            DaemonState::Unreachable(_) => Some(Self {
                kind: RecoveryKind::Reconnecting,
                title: t("toast.reconnecting").to_owned(),
                body: t("toast.reconnecting_body").to_owned(),
                detail: None,
                primary_action: Some((
                    RecoveryAction::RetryConnection,
                    t("toast.action_retry_now"),
                )),
                dismissible: false,
            }),
        }
    }
}

impl RecoveryNotice {
    /// The notice as the window's standard toast: persistent while its state
    /// lasts, with Retry and Copy details as inline text actions.
    #[must_use]
    pub fn toast(&self) -> Toast {
        let tone = match self.kind {
            RecoveryKind::Connecting
            | RecoveryKind::Reconnecting
            | RecoveryKind::RetryingAction => ToastTone::Progress,
            RecoveryKind::ManualAttention => ToastTone::Warning,
            RecoveryKind::ActionFailed => ToastTone::Error,
        };
        let mut toast = Toast::new(tone, self.title.clone())
            .detail(self.body.clone())
            .persistent(self.dismissible);
        if let Some((action, label)) = self.primary_action {
            toast = toast.action(
                label,
                match action {
                    RecoveryAction::RetryConnection => ToastCommand::RetryConnection,
                    RecoveryAction::RetryAction => ToastCommand::RetryAction,
                },
            );
        }
        if let Some(detail) = &self.detail {
            toast = toast.action(
                t("toast.action_copy_details"),
                ToastCommand::CopyDetails(detail.clone()),
            );
        }
        toast
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
