use diri_proto::{AttentionLevel, NeedsInputKind, RiskHint, SessionId, SessionRecord};

/// One revision-cached answer to “what deserves the human next?”.
///
/// The pulse deliberately reads only canonical [`SessionRecord`] state and is
/// built from the sidebar's active tree order. That keeps folded projects in
/// the fleet while excluding archived, closing, and workbench-owned terminal
/// rows before this module sees them. Destructive requests lead the queue, but
/// otherwise the user's project/session order is preserved.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FleetPulse {
    needs_you: usize,
    destructive: usize,
    done_unseen: usize,
    working: usize,
    next_actionable: Option<SessionId>,
    next_is_destructive: bool,
    summary: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FleetPulseState {
    Urgent { destructive: bool },
    Ready,
    Quiet,
}

impl FleetPulse {
    pub(super) fn derive<'a>(
        sessions: impl IntoIterator<Item = &'a SessionRecord>,
        selected: Option<&SessionId>,
    ) -> Self {
        let mut pulse = Self::default();
        let mut actionable = Vec::new();
        let mut ready = Vec::new();

        for session in sessions {
            match session.attention() {
                AttentionLevel::NeedsInput => {
                    pulse.needs_you += 1;
                    if needs_destructive_confirmation(session) {
                        pulse.destructive += 1;
                    }
                    actionable.push(session);
                }
                AttentionLevel::DoneUnseen => {
                    pulse.done_unseen += 1;
                    ready.push(session);
                }
                AttentionLevel::Working
                    if session.hibernation.is_none() && !session.effective_kind().is_terminal() =>
                {
                    pulse.working += 1;
                }
                AttentionLevel::None
                | AttentionLevel::IdleSeen
                | AttentionLevel::Working
                | AttentionLevel::Unknown => {}
            }
        }

        // `sort_by_key` is stable, so risk changes urgency without destroying
        // the project/session ordering a human deliberately arranged.
        actionable.sort_by_key(|session| !needs_destructive_confirmation(session));
        let queue = if actionable.is_empty() {
            &ready
        } else {
            &actionable
        };
        let next = queue
            .iter()
            .position(|session| selected == Some(&session.id))
            .map_or_else(
                || queue.first().copied(),
                |index| queue.get((index + 1) % queue.len()).copied(),
            );
        if let Some(next) = next {
            pulse.next_actionable = Some(next.id.clone());
            pulse.next_is_destructive = needs_destructive_confirmation(next);
            pulse.summary = Some(if pulse.needs_you > 0 {
                attention_summary(next)
            } else {
                concise(display_title(next), 96)
            });
        }
        pulse
    }

    pub const fn needs_you(&self) -> usize {
        self.needs_you
    }

    pub const fn destructive(&self) -> usize {
        self.destructive
    }

    pub const fn done_unseen(&self) -> usize {
        self.done_unseen
    }

    pub const fn working(&self) -> usize {
        self.working
    }

    pub const fn next_actionable(&self) -> Option<&SessionId> {
        self.next_actionable.as_ref()
    }

    pub const fn next_is_destructive(&self) -> bool {
        self.next_is_destructive
    }

    pub fn summary(&self) -> Option<&str> {
        self.summary.as_deref()
    }

    /// `None` is intentional: a fleet with only seen/ended/asleep sessions has
    /// no headline worth stealing vertical space from the session tree.
    pub const fn state(&self) -> Option<FleetPulseState> {
        if self.needs_you > 0 {
            Some(FleetPulseState::Urgent {
                destructive: self.next_is_destructive,
            })
        } else if self.done_unseen > 0 {
            Some(FleetPulseState::Ready)
        } else if self.working > 0 {
            Some(FleetPulseState::Quiet)
        } else {
            None
        }
    }

    pub fn headline(&self) -> Option<String> {
        match self.state()? {
            FleetPulseState::Urgent { .. } => Some(if self.needs_you == 1 {
                "1 needs you".to_owned()
            } else {
                format!("{} need you", self.needs_you)
            }),
            FleetPulseState::Ready => Some(if self.done_unseen == 1 {
                "1 agent finished".to_owned()
            } else {
                format!("{} agents finished", self.done_unseen)
            }),
            FleetPulseState::Quiet => Some(if self.working == 1 {
                "1 agent working".to_owned()
            } else {
                format!("{} agents working", self.working)
            }),
        }
    }

    pub fn accessibility_label(&self) -> Option<String> {
        let headline = self.headline()?;
        let mut parts = vec![headline];
        if self.destructive > 0 {
            parts.push(if self.destructive == 1 {
                "1 destructive request".to_owned()
            } else {
                format!("{} destructive requests", self.destructive)
            });
        }
        if let Some(summary) = &self.summary {
            parts.push(summary.clone());
        }
        Some(parts.join(". "))
    }
}

fn needs_destructive_confirmation(session: &SessionRecord) -> bool {
    session
        .needs_input
        .as_ref()
        .is_some_and(|detail| detail.risk_hint == RiskHint::Destructive)
}

fn attention_summary(session: &SessionRecord) -> String {
    let detail = session.needs_input.as_ref();
    let summary = detail
        .map(|detail| detail.summary.trim())
        .unwrap_or_default();
    if !summary.is_empty() {
        return concise(summary, 96);
    }
    match detail.map(|detail| &detail.kind).or({
        if let diri_proto::SessionStatus::NeedsInput(kind) = &session.status {
            Some(kind)
        } else {
            None
        }
    }) {
        Some(NeedsInputKind::Permission) => "Approval requested".to_owned(),
        Some(NeedsInputKind::Question | NeedsInputKind::Unknown) | None => {
            "Question waiting".to_owned()
        }
    }
}

fn display_title(session: &SessionRecord) -> &str {
    if session.title.trim().is_empty() {
        "Untitled session"
    } else {
        session.title.trim()
    }
}

fn concise(value: &str, max_chars: usize) -> String {
    let normalized = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.chars().count() <= max_chars {
        return normalized;
    }
    let mut result: String = normalized
        .chars()
        .take(max_chars.saturating_sub(1))
        .collect();
    result.push('…');
    result
}

#[cfg(test)]
mod tests {
    use diri_proto::{
        AgentKind, DateMillis, HibernationInfo, HibernationReason, NeedsInputDetail,
        NeedsInputSource, ProjectId, Resumability, SessionStatus, TitleSource,
    };

    use super::*;

    fn session(id: &str, status: SessionStatus) -> SessionRecord {
        SessionRecord {
            id: SessionId::new(id),
            kind: AgentKind::CLAUDE_CODE,
            cwd: "/work/diri".into(),
            project_id: ProjectId::new("diri"),
            worktree_path: None,
            git_branch: None,
            title: id.into(),
            title_source: TitleSource::Placeholder,
            originating_prompt: None,
            agent_session_id: None,
            transcript_path: None,
            status,
            status_evidence: None,
            needs_input: None,
            resumability: Resumability::Live,
            parent: None,
            created_at: DateMillis(1.0),
            updated_at: DateMillis(1.0),
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

    fn needs_input(id: &str, summary: &str, risk_hint: RiskHint) -> SessionRecord {
        let mut session = session(id, SessionStatus::NeedsInput(NeedsInputKind::Permission));
        session.needs_input = Some(NeedsInputDetail {
            kind: NeedsInputKind::Permission,
            source: NeedsInputSource::ClaudePermissionHook,
            tool_name: Some("Bash".into()),
            summary: summary.into(),
            prompt_excerpt: None,
            options: None,
            risk_hint,
            occurred_at: DateMillis(2.0),
        });
        session
    }

    #[test]
    fn pulse_counts_canonical_attention_and_ignores_sleeping_or_terminal_work() {
        let mut done = session("done", SessionStatus::Idle);
        done.last_turn_completed_at = Some(DateMillis(3.0));
        done.last_seen_at = Some(DateMillis(2.0));
        let working = session("working", SessionStatus::Working);
        let mut shell = session("shell", SessionStatus::Working);
        shell.kind = AgentKind::SHELL;
        let mut asleep = session("asleep", SessionStatus::Working);
        asleep.hibernation = Some(HibernationInfo {
            since: DateMillis(3.0),
            reason: HibernationReason::Idle,
            tree_pids: vec![10],
            tree_start_times: None,
        });
        let destructive = needs_input("delete", "Delete the old worktree?", RiskHint::Destructive);
        let neutral = needs_input("publish", "Publish the preview?", RiskHint::Network);

        let pulse = FleetPulse::derive(
            [&done, &working, &shell, &asleep, &neutral, &destructive],
            None,
        );

        assert_eq!(pulse.needs_you(), 2);
        assert_eq!(pulse.destructive(), 1);
        assert_eq!(pulse.done_unseen(), 1);
        assert_eq!(pulse.working(), 1);
        assert_eq!(pulse.next_actionable(), Some(&SessionId::new("delete")));
        assert!(pulse.next_is_destructive());
        assert_eq!(pulse.summary(), Some("Delete the old worktree?"));
        assert_eq!(
            pulse.state(),
            Some(FleetPulseState::Urgent { destructive: true })
        );
    }

    #[test]
    fn attention_queue_cycles_after_selection_without_losing_risk_priority() {
        let destructive = needs_input("delete", "Delete it?", RiskHint::Destructive);
        let first = needs_input("first", "First question", RiskHint::Neutral);
        let second = needs_input("second", "Second question", RiskHint::Neutral);

        let after_destructive =
            FleetPulse::derive([&first, &destructive, &second], Some(&destructive.id));
        assert_eq!(after_destructive.next_actionable(), Some(&first.id));

        let after_first = FleetPulse::derive([&first, &destructive, &second], Some(&first.id));
        assert_eq!(after_first.next_actionable(), Some(&second.id));

        let wrapped = FleetPulse::derive([&first, &destructive, &second], Some(&second.id));
        assert_eq!(wrapped.next_actionable(), Some(&destructive.id));
    }

    #[test]
    fn pulse_reveals_ready_then_quiet_and_omits_resting_fleets() {
        let mut done = session("done", SessionStatus::Idle);
        done.last_turn_completed_at = Some(DateMillis(3.0));
        let working = session("working", SessionStatus::Starting);
        let seen = session("seen", SessionStatus::Idle);

        assert_eq!(
            FleetPulse::derive([&done, &working], None).state(),
            Some(FleetPulseState::Ready)
        );
        assert_eq!(
            FleetPulse::derive([&working], None).state(),
            Some(FleetPulseState::Quiet)
        );
        assert_eq!(FleetPulse::derive([&seen], None).state(), None);
        assert_eq!(FleetPulse::derive([], None).state(), None);
    }

    #[test]
    fn summaries_are_normalized_bounded_and_have_a_semantic_fallback() {
        let long = needs_input(
            "long",
            "  Approve   this request because it contains a very long explanation that should not own the entire sidebar width forever and ever  ",
            RiskHint::Neutral,
        );
        let pulse = FleetPulse::derive([&long], None);
        let summary = pulse.summary().expect("summary");
        assert!(!summary.contains("  "));
        assert!(summary.chars().count() <= 96);
        assert!(summary.ends_with('…'));

        let fallback = needs_input("fallback", " ", RiskHint::Neutral);
        assert_eq!(
            FleetPulse::derive([&fallback], None).summary(),
            Some("Approval requested")
        );
    }
}
