//! Callers supply a shared animation frame and own its repaint cadence.
//! No task, clock, entity invalidation, or frame request belongs to this mark.

use diri_proto::{
    AgentKind as ProtoAgentKind, AttentionLevel as ProtoAttentionLevel, SessionRecord,
};
use diri_ui::{AgentKind, Icon, IconName, Ink, SemanticColors, StatusState};
use gpui::{AnyElement, IntoElement, Role, div, prelude::*, px, svg};

const FRAMES: [&str; 8] = [
    "icons/working-0.svg",
    "icons/working-1.svg",
    "icons/working-2.svg",
    "icons/working-3.svg",
    "icons/working-4.svg",
    "icons/working-5.svg",
    "icons/working-6.svg",
    "icons/working-7.svg",
];

pub(crate) fn frame_at(millis: f64, reduce_motion: bool) -> usize {
    if reduce_motion {
        0
    } else {
        (millis.max(0.0) / 125.0) as usize % FRAMES.len()
    }
}

pub(crate) fn activity_mark(
    state: StatusState,
    frame: usize,
    colors: SemanticColors,
) -> AnyElement {
    // Match the project badge column; the mark itself stays optically smaller.
    let slot = div()
        .size(px(18.0))
        .flex_none()
        .flex()
        .items_center()
        .justify_center();
    match state {
        StatusState::Working => slot
            .child(
                svg()
                    .path(FRAMES[frame % FRAMES.len()])
                    .size(px(14.0))
                    .text_color(colors.secondary),
            )
            .into_any_element(),
        StatusState::NeedsInput { destructive } => slot
            .child(Icon::new(
                IconName::Warning,
                14.0,
                if destructive {
                    Ink::DANGER
                } else {
                    Ink::ATTENTION
                },
            ))
            .into_any_element(),
        StatusState::DoneUnseen => slot
            .child(Icon::new(IconName::Check, 14.0, Ink::FRESH))
            .into_any_element(),
        StatusState::Hibernated => slot
            .id("sleeping-status")
            .role(Role::Image)
            .aria_label("Sleeping")
            .child(Icon::new(IconName::Moon, 13.0, colors.tertiary))
            .into_any_element(),
        StatusState::IdleSeen | StatusState::None => slot.into_any_element(),
    }
}

pub(crate) fn status_state(session: &SessionRecord, migrating: bool) -> StatusState {
    if migrating {
        return StatusState::Working;
    }
    if session.hibernation.is_some() {
        return StatusState::Hibernated;
    }
    match session.attention() {
        ProtoAttentionLevel::NeedsInput => StatusState::NeedsInput {
            destructive: session
                .needs_input
                .as_ref()
                .is_some_and(|detail| detail.risk_hint == diri_proto::RiskHint::Destructive),
        },
        ProtoAttentionLevel::DoneUnseen => StatusState::DoneUnseen,
        ProtoAttentionLevel::Working => StatusState::Working,
        ProtoAttentionLevel::IdleSeen => StatusState::IdleSeen,
        ProtoAttentionLevel::None | ProtoAttentionLevel::Unknown => StatusState::None,
    }
}

pub(crate) fn is_loading(session: &SessionRecord, migrating: bool) -> bool {
    !migrating
        && session.hibernation.is_none()
        && session.effective_kind() != &ProtoAgentKind::SHELL
        && matches!(session.status, diri_proto::SessionStatus::Starting)
}

pub(crate) fn ui_agent_kind(kind: &ProtoAgentKind) -> AgentKind {
    // Brand vocabulary, not a protocol type: a manifest agent the client has
    // no hand-drawn mark for falls back to the generic terminal treatment.
    match kind.id() {
        ProtoAgentKind::CLAUDE_CODE_ID => AgentKind::ClaudeCode,
        ProtoAgentKind::CODEX_ID => AgentKind::Codex,
        ProtoAgentKind::CURSOR_ID => AgentKind::Cursor,
        ProtoAgentKind::GEMINI_ID => AgentKind::Gemini,
        ProtoAgentKind::SHELL_ID => AgentKind::Shell,
        _ => AgentKind::Generic,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_terminal_has_no_loading_or_working_indicator() {
        use crate::sidebar::{PreviewScenario, SidebarPreviewFixture};
        use diri_proto::SessionStatus;

        let mut session = SidebarPreviewFixture::make(PreviewScenario::Typical)
            .list
            .sessions
            .into_iter()
            .find(|session| session.kind == ProtoAgentKind::SHELL)
            .expect("shell fixture");
        for status in [
            SessionStatus::Starting,
            SessionStatus::Working,
            SessionStatus::Idle,
        ] {
            session.status = status;
            assert!(!is_loading(&session, false));
            assert_eq!(status_state(&session, false), StatusState::None);
        }

        session.foreground_agent = Some(ProtoAgentKind::CLAUDE_CODE);
        session.status = SessionStatus::Starting;
        assert!(is_loading(&session, false));
        assert_eq!(status_state(&session, false), StatusState::Working);
        assert!(!is_loading(&session, true));
    }

    #[test]
    fn every_activity_frame_is_embedded_in_the_app() {
        use gpui::AssetSource;
        for path in FRAMES {
            let bytes = diri_ui::IconAssets
                .load(path)
                .expect("load activity frame")
                .expect(path);
            assert!(bytes.starts_with(b"<svg"), "{path}");
        }
    }

    #[test]
    fn fleet_shares_eight_bounded_frames_and_reduce_motion_is_static() {
        for millis in [0.0, 124.0, 125.0, 999.0, 1000.0, 1750000000000.0] {
            let frames = [frame_at(millis, false); 30];
            assert!(frames.iter().all(|frame| *frame == frames[0] && *frame < 8));
            assert_eq!(frame_at(millis, true), 0);
        }
        assert_eq!(frame_at(124.0, false), 0);
        assert_eq!(frame_at(125.0, false), 1);
        assert_eq!(frame_at(1000.0, false), 0);
    }
}
