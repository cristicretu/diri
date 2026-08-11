//! Platform-neutral notification policy shared by the store and macOS bridge.
//!
//! The behavior and per-agent answers carry over from the retired Swift client.

use diri_proto::remote_pty::PersistenceCapability;
use diri_proto::{
    AgentDescriptor, AgentKind, AttentionLevel, HibernationReason, NeedsInputKind, SessionId,
    SessionRecord,
};

pub const PERMISSION_CATEGORY_ID: &str = "needs-input-permission";
pub const APPROVE_ACTION_ID: &str = "approve";
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NotificationRequest {
    pub identifier: String,
    pub title: String,
    pub body: String,
    pub thread_identifier: Option<String>,
    pub action_data: Option<ActionData>,
    pub use_system_sound: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatusTransition {
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
        sound: None,
        notification: Some(NotificationRequest {
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
        sound: None,
        notification: Some(NotificationRequest {
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

/// Resolve an actionable notification response into a daemon `send_text` call.
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

/// Produce sound/banner work for a single authoritative session update.
///
/// Chimes are emitted even for the focused session. Banners are suppressed only
/// when that same session is selected and the app is active.
///
/// `descriptor` is the manifest descriptor for `current.effective_kind()`, when
/// the daemon shipped one. It carries the agent's display name and its
/// approve/deny keystrokes, so banner copy and quick actions stay
/// manifest-driven instead of needing a client release per agent.
#[must_use]
pub fn transitions_for_update(
    previous: Option<&SessionRecord>,
    current: &SessionRecord,
    selected_session_id: Option<&SessionId>,
    app_is_active: bool,
    status_sounds_enabled: bool,
    descriptor: Option<&AgentDescriptor>,
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
            sound: status_sounds_enabled.then_some(NotificationSound::Frozen),
            notification: Some(memory_pressure_request(current, status_sounds_enabled)),
            in_app_banner: None,
        });
    }

    let previous_attention = previous.map(SessionRecord::attention);
    let current_attention = current.attention();
    let became_blocked = previous_attention != Some(AttentionLevel::NeedsInput)
        && current_attention == AttentionLevel::NeedsInput;
    let became_done = previous_attention != Some(AttentionLevel::DoneUnseen)
        && current_attention == AttentionLevel::DoneUnseen;
    if became_blocked || became_done {
        let is_focused = selected_session_id == Some(&current.id) && app_is_active;
        transitions.push(StatusTransition {
            sound: status_sounds_enabled.then_some(if became_blocked {
                NotificationSound::NeedsInput
            } else {
                NotificationSound::Done
            }),
            notification: (!is_focused)
                .then(|| attention_request(current, status_sounds_enabled, descriptor)),
            in_app_banner: None,
        });
    }

    transitions
}

fn attention_request(
    session: &SessionRecord,
    status_sounds_enabled: bool,
    descriptor: Option<&AgentDescriptor>,
) -> NotificationRequest {
    let (title, body, suffix, action_data) = match session.attention() {
        AttentionLevel::NeedsInput => {
            let detail = session.needs_input.as_ref();
            let action_data = detail
                .filter(|detail| detail.kind == NeedsInputKind::Permission)
                .and_then(|_| approve_answer(session.effective_kind(), descriptor))
                .map(|approve| ActionData {
                    session_id: session.id.clone(),
                    approve,
                    deny: deny_answer(descriptor),
                });
            (
                format!(
                    "{} needs you",
                    display_name(session.effective_kind(), descriptor)
                ),
                detail.map_or_else(|| session.title.clone(), |detail| detail.summary.clone()),
                detail.map_or_else(|| "needs-input".to_owned(), blocker_identity),
                action_data,
            )
        }
        AttentionLevel::DoneUnseen => (
            format!(
                "{} finished",
                display_name(session.effective_kind(), descriptor)
            ),
            session.title.clone(),
            "done".to_owned(),
            None,
        ),
        _ => unreachable!("attention requests are only built for noteworthy states"),
    };

    NotificationRequest {
        identifier: format!("{}-{suffix}", session.id.0),
        title,
        body,
        thread_identifier: Some(session.id.0.clone()),
        action_data,
        use_system_sound: !status_sounds_enabled,
    }
}

fn memory_pressure_request(
    session: &SessionRecord,
    status_sounds_enabled: bool,
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
        identifier: format!("{}-memory-pressure", session.id.0),
        title: "Session frozen — high memory".to_owned(),
        body,
        thread_identifier: Some(session.id.0.clone()),
        action_data: None,
        use_system_sound: !status_sounds_enabled,
    }
}

/// Human-facing agent name for banner copy. Prefers the manifest's own
/// `displayName` so "Amp finished" beats "Agent finished" for every agent the
/// daemon knows about; the table is the pre-descriptor fallback.
fn display_name<'a>(kind: &AgentKind, descriptor: Option<&'a AgentDescriptor>) -> &'a str
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

fn blocker_identity(detail: &diri_proto::NeedsInputDetail) -> String {
    // Swift's `Hasher` is intentionally randomized. A compact deterministic FNV-1a
    // identifier gives the same one-notification-per-distinct-blocker semantics.
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    let identity = format!(
        "{:?}\0{}\0{}",
        detail.kind,
        detail.tool_name.as_deref().unwrap_or_default(),
        detail.summary
    );
    for byte in identity.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_persistent_remote_capability_is_user_visible_once() {
        let mut current = session(AgentKind::CODEX, SessionStatus::Working);
        current.host = Some("forge".to_owned());
        current.remote_persistence = Some(PersistenceCapability::NonPersistent);

        let first = transitions_for_update(None, &current, None, true, false, None);
        let banner = first
            .iter()
            .find_map(|transition| transition.in_app_banner.as_ref())
            .expect("non-persistent warning");
        assert!(banner.title.contains("cannot survive"));
        assert!(banner.body.contains("forge"));

        assert!(
            transitions_for_update(Some(&current), &current, None, true, false, None).is_empty()
        );
    }
    use diri_proto::{
        AgentKeystroke, DateMillis, NeedsInputDetail, NeedsInputSource, ProjectId, Resumability,
        RiskHint, SessionStatus, TitleSource,
    };

    fn session(kind: AgentKind, status: SessionStatus) -> SessionRecord {
        SessionRecord {
            id: SessionId::new("session-1"),
            kind,
            cwd: "/tmp".to_owned(),
            project_id: ProjectId::new("project-1"),
            worktree_path: None,
            git_branch: None,
            title: "Refactor parser".to_owned(),
            title_source: TitleSource::AgentProvided,
            agent_session_id: None,
            transcript_path: None,
            status,
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
            parent: None,
            created_at: DateMillis(1.0),
            updated_at: DateMillis(2.0),
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

    #[test]
    fn approve_and_deny_map_to_each_agents_exact_keystrokes() {
        for (kind, text, submit) in [
            (AgentKind::CLAUDE_CODE, "1", true),
            (AgentKind::CODEX, "", true),
            (AgentKind::CURSOR, "y", false),
            (AgentKind::GEMINI, "", true),
        ] {
            let current = session(kind, SessionStatus::NeedsInput(NeedsInputKind::Permission));
            let transition = &transitions_for_update(None, &current, None, true, true, None)[0];
            let data = transition
                .notification
                .as_ref()
                .unwrap()
                .action_data
                .as_ref()
                .unwrap();
            assert_eq!(
                command_for_action(APPROVE_ACTION_ID, data),
                Some(SendTextCommand {
                    session_id: current.id.clone(),
                    text: text.to_owned(),
                    submit,
                })
            );
            assert_eq!(
                command_for_action(DENY_ACTION_ID, data),
                Some(SendTextCommand {
                    session_id: current.id.clone(),
                    text: "\u{1b}".to_owned(),
                    submit: false,
                })
            );
        }
    }

    #[test]
    fn manifest_descriptors_drive_answers_and_banner_copy_for_new_agents() {
        // An agent this build has never heard of, described entirely by data the
        // daemon read out of its manifest — the whole point of the rework.
        let amp = AgentKind::new("amp");
        let descriptor = AgentDescriptor {
            id: "amp".to_owned(),
            display_name: "Amp".to_owned(),
            short_label: "amp".to_owned(),
            glyph: "\u{23fb}".to_owned(),
            first_class: true,
            approve: Some(AgentKeystroke {
                text: "a".to_owned(),
                submit: true,
            }),
            deny: Some(AgentKeystroke {
                text: "n".to_owned(),
                submit: true,
            }),
            setup: None,
        };
        let current = session(
            amp.clone(),
            SessionStatus::NeedsInput(NeedsInputKind::Permission),
        );
        let transition =
            &transitions_for_update(None, &current, None, true, true, Some(&descriptor))[0];
        let notification = transition.notification.as_ref().unwrap();
        assert_eq!(notification.title, "Amp needs you");
        let data = notification.action_data.as_ref().unwrap();
        assert_eq!(
            command_for_action(APPROVE_ACTION_ID, data),
            Some(SendTextCommand {
                session_id: current.id.clone(),
                text: "a".to_owned(),
                submit: true,
            })
        );
        assert_eq!(
            command_for_action(DENY_ACTION_ID, data),
            Some(SendTextCommand {
                session_id: current.id.clone(),
                text: "n".to_owned(),
                submit: true,
            })
        );

        // No approve keystroke declared — the conservative default for an agent
        // whose permission dialog nobody has verified. The banner still names
        // it; it just offers no one-tap answer.
        let unverified = AgentDescriptor {
            approve: None,
            ..descriptor.clone()
        };
        let transition =
            &transitions_for_update(None, &current, None, true, true, Some(&unverified))[0];
        let notification = transition.notification.as_ref().unwrap();
        assert_eq!(notification.title, "Amp needs you");
        assert!(notification.action_data.is_none());

        // Without a descriptor (daemon too old to send one) an unknown agent
        // falls back to the generic name and no quick actions.
        let transition = &transitions_for_update(None, &current, None, true, true, None)[0];
        assert_eq!(
            transition.notification.as_ref().unwrap().title,
            "Agent needs you"
        );
    }

    #[test]
    fn hidden_needs_input_chimes_and_posts_but_focused_only_chimes() {
        let current = session(
            AgentKind::CODEX,
            SessionStatus::NeedsInput(NeedsInputKind::Permission),
        );
        let hidden = transitions_for_update(
            None,
            &current,
            Some(&SessionId::new("other")),
            true,
            true,
            None,
        );
        assert_eq!(hidden[0].sound, Some(NotificationSound::NeedsInput));
        assert!(hidden[0].notification.is_some());

        let focused = transitions_for_update(None, &current, Some(&current.id), true, true, None);
        assert_eq!(focused[0].sound, Some(NotificationSound::NeedsInput));
        assert!(focused[0].notification.is_none());

        let inactive = transitions_for_update(None, &current, Some(&current.id), false, true, None);
        assert!(inactive[0].notification.is_some());
    }

    #[test]
    fn questions_and_unknown_agents_do_not_offer_unsafe_generic_actions() {
        let mut question = session(
            AgentKind::CLAUDE_CODE,
            SessionStatus::NeedsInput(NeedsInputKind::Question),
        );
        question.needs_input.as_mut().unwrap().kind = NeedsInputKind::Question;
        let transition = &transitions_for_update(None, &question, None, true, true, None)[0];
        assert!(
            transition
                .notification
                .as_ref()
                .unwrap()
                .action_data
                .is_none()
        );

        let shell = session(
            AgentKind::SHELL,
            SessionStatus::NeedsInput(NeedsInputKind::Permission),
        );
        let transition = &transitions_for_update(None, &shell, None, true, true, None)[0];
        assert!(
            transition
                .notification
                .as_ref()
                .unwrap()
                .action_data
                .is_none()
        );
    }

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

    #[test]
    fn disabled_synth_uses_the_system_notification_sound_instead() {
        let current = session(
            AgentKind::CODEX,
            SessionStatus::NeedsInput(NeedsInputKind::Permission),
        );
        let transition = &transitions_for_update(None, &current, None, true, false, None)[0];
        assert_eq!(transition.sound, None);
        assert!(transition.notification.as_ref().unwrap().use_system_sound);
    }
}
