//! Platform-neutral notification policy shared by the store and macOS bridge.
//!
//! The behavior and per-agent answers carry over from the retired Swift client.

use diri_proto::remote_pty::PersistenceCapability;
use diri_proto::{
    AgentDescriptor, AgentKind, HibernationReason, NeedsInputKind, SessionId, SessionRecord,
};

#[cfg(target_os = "macos")]
pub const OPEN_ACTION_ID: &str = "open-session";
#[cfg(test)]
pub const APPROVE_ACTION_ID: &str = "approve";
#[cfg(test)]
pub const DENY_ACTION_ID: &str = "deny";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotificationSound {
    NeedsInput,
    Done,
    Frozen,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct Answer {
    pub text: String,
    pub submit: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct ActionData {
    pub session_id: SessionId,
    pub approve: Answer,
    pub deny: Answer,
}

#[derive(Clone, Debug)]
pub struct DeliveryGuard(pub std::sync::Arc<std::sync::atomic::AtomicBool>);
impl PartialEq for DeliveryGuard {
    fn eq(&self, other: &Self) -> bool {
        std::sync::Arc::ptr_eq(&self.0, &other.0)
    }
}
impl Eq for DeliveryGuard {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NotificationRequest {
    /// Session lifecycle/custom events must retain an admission receipt and a live guard.
    pub session_event: bool,
    pub guard: Option<DeliveryGuard>,
    pub identifier: String,
    pub title: String,
    pub body: String,
    pub thread_identifier: Option<String>,
    pub action_data: Option<ActionData>,
    pub use_system_sound: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatusTransition {
    /// Native alerts withdrawn after read, resolution, or session removal.
    pub dismiss: Vec<String>,
    pub sound: Option<NotificationSound>,
    pub notification: Option<NotificationRequest>,
    /// Foreground feedback for user-initiated operations. System
    /// notifications are not a reliable visible surface while the app is
    /// active or when notification permission is disabled.
    pub in_app_banner: Option<InAppBanner>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InAppBanner {
    pub title: String,
    pub body: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SendTextCommand {
    pub session_id: SessionId,
    pub text: String,
    pub submit: bool,
}

fn one_shot_identifier(prefix: &str) -> String {
    format!(
        "{prefix}-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |duration| duration.as_nanos())
    )
}

fn plain_banner(prefix: &str, title: String, body: String) -> StatusTransition {
    StatusTransition {
        dismiss: Vec::new(),
        sound: None,
        notification: Some(NotificationRequest {
            session_event: false,
            guard: None,
            identifier: one_shot_identifier(prefix),
            title: title.clone(),
            body: body.clone(),
            thread_identifier: None,
            action_data: None,
            use_system_sound: true,
        }),
        in_app_banner: Some(InAppBanner { title, body }),
    }
}

fn foreground_banner(title: String, body: String) -> StatusTransition {
    StatusTransition {
        dismiss: Vec::new(),
        sound: None,
        notification: None,
        in_app_banner: Some(InAppBanner { title, body }),
    }
}

/// Transient feedback for `host.sync_prefs`: one banner summarizing per-tool
/// outcomes, or the failure detail.
#[must_use]
pub fn prefs_sync_transition(
    host_name: &str,
    result: Result<&diri_proto::HostSyncPrefsResult, &str>,
) -> StatusTransition {
    match result {
        Ok(report) => {
            let failed: Vec<_> = report.tools.iter().filter(|tool| !tool.ok).collect();
            if failed.is_empty() {
                let summary = report
                    .tools
                    .iter()
                    .map(|tool| {
                        if tool.synced.is_empty() {
                            format!("{}: nothing to sync", tool.tool)
                        } else {
                            format!("{}: {} items", tool.tool, tool.synced.len())
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(" · ");
                plain_banner(
                    "prefs-sync",
                    format!("Prefs synced to {host_name}"),
                    summary,
                )
            } else {
                let detail = failed
                    .iter()
                    .map(|tool| {
                        format!(
                            "{}: {}",
                            tool.tool,
                            tool.error.as_deref().unwrap_or("failed")
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(" · ");
                plain_banner(
                    "prefs-sync",
                    format!("Prefs sync to {host_name} failed"),
                    detail,
                )
            }
        }
        Err(error) => plain_banner(
            "prefs-sync",
            format!("Prefs sync to {host_name} failed"),
            error.to_owned(),
        ),
    }
}

/// Transient feedback for `session.migrate`. A clean success is confirmed
/// inside the app; warnings and failures additionally use a system banner.
#[must_use]
pub fn migration_transition(
    session_title: &str,
    destination: &str,
    result: Result<Option<&str>, &str>,
) -> Option<StatusTransition> {
    match result {
        Ok(None) => Some(foreground_banner(
            format!("Moved “{session_title}” to {destination}"),
            format!("The conversation is now running on {destination}."),
        )),
        Ok(Some(warning)) => Some(plain_banner(
            "migrate",
            format!("Moved to {destination} with warnings"),
            warning.to_owned(),
        )),
        Err(error) => Some(plain_banner(
            "migrate",
            if session_title.is_empty() {
                format!("Move to {destination} failed")
            } else {
                format!("Move “{session_title}” to {destination} failed")
            },
            error.to_owned(),
        )),
    }
}

#[must_use]
pub fn reach_failure_transition() -> StatusTransition {
    StatusTransition {
        dismiss: Vec::new(),
        sound: None,
        notification: Some(NotificationRequest {
            session_event: false,
            guard: None,
            identifier: format!(
                "reach-failure-{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |duration| duration.as_nanos())
            ),
            title: "Couldn't reach session".to_owned(),
            body: "diri couldn't deliver your answer. Open the session to respond.".to_owned(),
            thread_identifier: None,
            action_data: None,
            use_system_sound: true,
        }),
        in_app_banner: None,
    }
}

/// The keystroke meaning "yes" at a CLI's permission prompt.
///
/// Declared by the agent's manifest and shipped to us in `agent.readiness`, so
/// an agent added on the daemon as a file drop gets a working Approve button
/// here without a client release. `None` — the correct default for any agent
/// whose dialog nobody has verified — simply omits the quick-approve action.
///
/// The built-in table below is the fallback for a daemon too old to send
/// descriptors; it deliberately covers only the four agents that predate them.
#[must_use]
pub fn approve_answer(kind: &AgentKind, descriptor: Option<&AgentDescriptor>) -> Option<Answer> {
    if let Some(descriptor) = descriptor {
        return descriptor.approve.as_ref().map(|approve| Answer {
            text: approve.text.clone(),
            submit: approve.submit,
        });
    }
    match kind.id() {
        AgentKind::CLAUDE_CODE_ID => Some(Answer {
            text: "1".to_owned(),
            submit: true,
        }),
        AgentKind::CODEX_ID | AgentKind::GEMINI_ID => Some(Answer {
            text: String::new(),
            submit: true,
        }),
        AgentKind::CURSOR_ID => Some(Answer {
            text: "y".to_owned(),
            submit: false,
        }),
        _ => None,
    }
}

/// The keystroke meaning "no". Escape everywhere we have seen, but the manifest
/// can override it for a CLI that dismisses differently.
fn deny_answer(descriptor: Option<&AgentDescriptor>) -> Answer {
    descriptor
        .and_then(|descriptor| descriptor.deny.as_ref())
        .map_or(
            Answer {
                text: "\u{1b}".to_owned(),
                submit: false,
            },
            |deny| Answer {
                text: deny.text.clone(),
                submit: deny.submit,
            },
        )
}

/// Legacy key mapping. Native actions now open the session: captured terminal
/// keystrokes cannot safely approve a prompt after it changes.
#[cfg(test)]
#[must_use]
pub fn command_for_action(action_id: &str, data: &ActionData) -> Option<SendTextCommand> {
    let answer = match action_id {
        APPROVE_ACTION_ID => &data.approve,
        DENY_ACTION_ID => &data.deny,
        _ => return None,
    };
    Some(SendTextCommand {
        session_id: data.session_id.clone(),
        text: answer.text.clone(),
        submit: answer.submit,
    })
}

/// Quick Approve/Deny payload for a permission prompt — shared by notifications
/// and the menu-bar attention inbox.
#[must_use]
pub fn permission_action_data(
    session: &SessionRecord,
    descriptor: Option<&AgentDescriptor>,
) -> Option<ActionData> {
    session
        .needs_input
        .as_ref()
        .filter(|detail| detail.kind == NeedsInputKind::Permission)
        .and_then(|_| approve_answer(session.effective_kind(), descriptor))
        .map(|approve| ActionData {
            session_id: session.id.clone(),
            approve,
            deny: deny_answer(descriptor),
        })
}

/// Produce the sound/banner work that a session update earns immediately.
///
/// These are one-shot facts — the host cannot keep processes alive, the
/// governor froze a session — so there is nothing to wait out. Attention
/// events are admitted and settled by `NotificationFeed`.
#[must_use]
pub fn immediate_transitions_for_update(
    previous: Option<&SessionRecord>,
    current: &SessionRecord,
    status_sounds_enabled: bool,
) -> Vec<StatusTransition> {
    let mut transitions = Vec::with_capacity(2);

    let became_non_persistent = previous.and_then(|session| session.remote_persistence)
        != Some(PersistenceCapability::NonPersistent)
        && current.remote_persistence == Some(PersistenceCapability::NonPersistent);
    if became_non_persistent {
        let host = current.host.as_deref().unwrap_or("the remote host");
        transitions.push(plain_banner(
            "remote-non-persistent",
            "Remote session cannot survive disconnects".to_owned(),
            format!(
                "{host} does not preserve detached user processes. Keep SSH connected or the Agent may exit."
            ),
        ));
    }

    let was_memory_frozen = previous.is_some_and(|session| {
        session
            .hibernation
            .as_ref()
            .is_some_and(|info| info.reason == HibernationReason::MemoryPressure)
    });
    let is_memory_frozen = current
        .hibernation
        .as_ref()
        .is_some_and(|info| info.reason == HibernationReason::MemoryPressure);
    if !was_memory_frozen && is_memory_frozen {
        transitions.push(StatusTransition {
            dismiss: Vec::new(),
            sound: status_sounds_enabled.then_some(NotificationSound::Frozen),
            notification: Some(memory_pressure_request(current, status_sounds_enabled)),
            in_app_banner: None,
        });
    }

    transitions
}

fn memory_pressure_request(
    session: &SessionRecord,
    _status_sounds_enabled: bool,
) -> NotificationRequest {
    let body = session.memory_bytes.map_or_else(
        || {
            format!(
                "{} was frozen to reclaim memory. Select it to wake.",
                session.title
            )
        },
        |bytes| {
            format!(
                "{} — {:.1} GB. Select it to wake.",
                session.title,
                bytes as f64 / 1_000_000_000.0
            )
        },
    );
    NotificationRequest {
        session_event: false,
        guard: None,
        identifier: format!("{}-memory-pressure", session.id.0),
        title: "Session frozen — high memory".to_owned(),
        body,
        thread_identifier: Some(session.id.0.clone()),
        action_data: None,
        use_system_sound: false,
    }
}

/// Human-facing agent name for banner copy. Prefers the manifest's own
/// `displayName` so "Amp finished" beats "Agent finished" for every agent the
/// daemon knows about; the table is the pre-descriptor fallback.
pub fn display_name<'a>(kind: &AgentKind, descriptor: Option<&'a AgentDescriptor>) -> &'a str
where
    'static: 'a,
{
    if let Some(descriptor) = descriptor
        && !descriptor.display_name.is_empty()
    {
        return &descriptor.display_name;
    }
    match kind.id() {
        AgentKind::CLAUDE_CODE_ID => "Claude Code",
        AgentKind::CODEX_ID => "Codex",
        AgentKind::CURSOR_ID => "Cursor",
        AgentKind::GEMINI_ID => "Gemini",
        AgentKind::SHELL_ID => "Terminal",
        _ => "Agent",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_persistent_remote_capability_is_user_visible_once() {
        let mut current = session(AgentKind::CODEX, SessionStatus::Working);
        current.host = Some("forge".to_owned());
        current.remote_persistence = Some(PersistenceCapability::NonPersistent);

        let first = immediate_transitions_for_update(None, &current, false);
        let banner = first
            .iter()
            .find_map(|transition| transition.in_app_banner.as_ref())
            .expect("non-persistent warning");
        assert!(banner.title.contains("cannot survive"));
        assert!(banner.body.contains("forge"));

        assert!(immediate_transitions_for_update(Some(&current), &current, false).is_empty());
    }
    use diri_proto::{
        DateMillis, NeedsInputDetail, NeedsInputSource, ProjectId, Resumability, RiskHint,
        SessionStatus, TitleSource,
    };

    fn session(kind: AgentKind, status: SessionStatus) -> SessionRecord {
        SessionRecord {
            attention_state: None,
            id: SessionId::new("session-1"),
            kind,
            cwd: "/tmp".to_owned(),
            project_id: ProjectId::new("project-1"),
            worktree_path: None,
            git_branch: None,
            title: "Refactor parser".to_owned(),
            title_source: TitleSource::AgentProvided,
            account_profile: None,
            originating_prompt: None,
            agent_session_id: None,
            transcript_path: None,
            status,
            status_evidence: None,
            needs_input: Some(NeedsInputDetail {
                kind: NeedsInputKind::Permission,
                source: NeedsInputSource::ScreenScrape,
                tool_name: Some("Bash".to_owned()),
                summary: "Run the test suite?".to_owned(),
                prompt_excerpt: None,
                options: None,
                risk_hint: RiskHint::Neutral,
                occurred_at: DateMillis(1.0),
            }),
            resumability: Resumability::NotResumable,
            capabilities: None,
            parent: None,
            created_at: DateMillis(1.0),
            updated_at: DateMillis(2.0),
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
        }
    }

    /// What the settle task does at the deadline for a session that is still in
    /// the state that armed it.
    #[test]
    fn prefs_sync_and_migration_banners_summarize_outcomes() {
        let report = diri_proto::HostSyncPrefsResult {
            tools: vec![
                diri_proto::PrefsSyncToolReport {
                    tool: "claude".into(),
                    ok: true,
                    synced: vec!["CLAUDE.md".into(), "commands".into()],
                    error: None,
                },
                diri_proto::PrefsSyncToolReport {
                    tool: "codex".into(),
                    ok: true,
                    synced: vec![],
                    error: None,
                },
            ],
        };
        let ok = prefs_sync_transition("Forge", Ok(&report));
        let banner = ok.notification.expect("banner");
        assert_eq!(banner.title, "Prefs synced to Forge");
        assert_eq!(banner.body, "claude: 2 items · codex: nothing to sync");

        let mut failed = report.clone();
        failed.tools[0].ok = false;
        failed.tools[0].error = Some("rsync is not installed on Forge".into());
        let banner = prefs_sync_transition("Forge", Ok(&failed))
            .notification
            .expect("banner");
        assert_eq!(banner.title, "Prefs sync to Forge failed");
        assert!(banner.body.contains("rsync is not installed"));

        // Migration: clean success confirms in-app; warnings and failures also
        // carry the detail on both foreground and system surfaces.
        let moved = migration_transition("Refactor", "Forge", Ok(None))
            .expect("success banner")
            .in_app_banner
            .expect("foreground success");
        assert_eq!(moved.title, "Moved “Refactor” to Forge");
        let warned = migration_transition("Refactor", "Forge", Ok(Some("transcript not found")))
            .expect("warning banner")
            .notification
            .expect("banner");
        assert_eq!(warned.title, "Moved to Forge with warnings");
        let failed = migration_transition("Refactor", "local", Err("repo not cloned locally"))
            .expect("failure banner")
            .notification
            .expect("banner");
        assert_eq!(failed.title, "Move “Refactor” to local failed");
        assert_eq!(failed.body, "repo not cloned locally");

        let failed = migration_transition("Refactor", "local", Err("repo not cloned locally"))
            .expect("failure banner")
            .in_app_banner
            .expect("migration failures must be visible inside the active app");
        assert_eq!(failed.title, "Move “Refactor” to local failed");
        assert_eq!(failed.body, "repo not cloned locally");
    }
}
