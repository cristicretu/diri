//! The working mark loops through pre-rasterized frames that the window
//! advances on its own (`diri_ui::FrameLoop`). No task, entity invalidation,
//! or frame request belongs to this mark, and no caller re-renders for it.

use diri_proto::{
    AgentKind as ProtoAgentKind, AttentionLevel as ProtoAttentionLevel, SessionRecord,
};
use diri_ui::{AgentKind, FrameLoop, Icon, IconName, Ink, SemanticColors, StatusState};
use gpui::{AnyElement, IntoElement, Role, div, prelude::*, px};
use std::time::Duration;

static FRAMES: [&str; 8] = [
    "icons/working-0.svg",
    "icons/working-1.svg",
    "icons/working-2.svg",
    "icons/working-3.svg",
    "icons/working-4.svg",
    "icons/working-5.svg",
    "icons/working-6.svg",
    "icons/working-7.svg",
];

/// One working-mark frame lasts this long: eight frames, one lap a second.
const FRAME_INTERVAL: Duration = Duration::from_millis(125);

pub(crate) fn activity_mark(state: StatusState, colors: SemanticColors) -> AnyElement {
    // Match the project badge column; the mark itself stays optically smaller.
    let slot = div()
        .size(px(18.0))
        .flex_none()
        .flex()
        .items_center()
        .justify_center();
    match state {
        StatusState::Working => slot
            .child(FrameLoop::new(
                &FRAMES,
                FRAME_INTERVAL,
                14.0,
                colors.primary,
            ))
            .into_any_element(),
        StatusState::NeedsInput { destructive } => slot
            .child(Icon::new(
                IconName::Warning,
                14.0,
                Ink::on_surface(
                    if destructive {
                        Ink::DANGER
                    } else {
                        Ink::ATTENTION
                    },
                    colors,
                ),
            ))
            .into_any_element(),
        StatusState::DoneUnseen => slot
            .child(Icon::new(
                IconName::Check,
                14.0,
                Ink::on_surface(Ink::FRESH, colors),
            ))
            .into_any_element(),
        StatusState::Hibernated => slot
            .id("sleeping-status")
            .role(Role::Image)
            .aria_label(crate::i18n::t("session.sleeping"))
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
    // Brand vocabulary, not a protocol type: any other Agent takes its mark
    // from the catalog, and one without a mark the generic terminal treatment.
    match kind.id() {
        ProtoAgentKind::CLAUDE_CODE_ID => AgentKind::ClaudeCode,
        ProtoAgentKind::CODEX_ID => AgentKind::Codex,
        ProtoAgentKind::CURSOR_ID => AgentKind::Cursor,
        ProtoAgentKind::GEMINI_ID => AgentKind::Gemini,
        ProtoAgentKind::SHELL_ID => AgentKind::Shell,
        ProtoAgentKind::NOTE_ID => AgentKind::Note,
        id => AgentKind::from_agent_id(id),
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

    /// Every Agent Diri can launch or recognise has its own mark, and every
    /// bundled mark belongs to one: no Agent is left on the generic terminal
    /// treatment, and no mark is dead weight.
    #[test]
    fn every_agent_has_its_own_mark() {
        let (manifests, _) =
            diri_engine::ManifestEngine::load_dir(&diri_engine::detect::bundled_manifest_dir())
                .unwrap();
        let mut ids: std::collections::BTreeSet<&str> = manifests.ids().into_iter().collect();
        ids.extend(
            diri_engine::recognition::CATALOG
                .iter()
                .map(|agent| agent.id),
        );
        for id in &ids {
            let kind = ui_agent_kind(&ProtoAgentKind::new(*id));
            match *id {
                ProtoAgentKind::GENERIC_ID => assert_eq!(kind, AgentKind::Generic),
                ProtoAgentKind::SHELL_ID => assert_eq!(kind, AgentKind::Shell),
                _ => assert!(kind.brand_mark().is_some(), "{id} has no mark"),
            }
        }
        for mark in &diri_ui::AGENT_MARKS {
            assert!(ids.contains(mark.id()), "{} is no Agent's mark", mark.id());
            assert_eq!(
                ui_agent_kind(&ProtoAgentKind::new(mark.id())),
                AgentKind::Agent(mark)
            );
        }
        // An id from a newer Engine falls back to the terminal treatment.
        assert_eq!(
            ui_agent_kind(&ProtoAgentKind::new("some-future-agent")),
            AgentKind::Generic
        );
    }

    /// Every Agent mark next to the four brand marks, as the sidebar draws
    /// them (a 16 px status glyph, rasterized by CoreGraphics) and at 32 px
    /// to judge their shapes, on a dark, a light and two themed palettes.
    ///
    /// `DIRI_VISUAL_OUTPUT=/tmp/marks.png cargo test -p diri-app
    /// render_agent_marks_screenshot -- --ignored`
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "writes the Agent mark grid screenshot"]
    fn render_agent_marks_screenshot() {
        use diri_term::theme::TermTheme;
        use diri_ui::{SemanticColors, StatusGlyph, Typo};
        use gpui::{HeadlessAppContext, Render, Window, size};
        use std::sync::Arc;

        struct Grid {
            panels: Vec<SemanticColors>,
            marks: Vec<(AgentKind, String)>,
        }

        impl Render for Grid {
            fn render(&mut self, _: &mut Window, _: &mut gpui::Context<Self>) -> impl IntoElement {
                let panel = |colors: SemanticColors, marks: &[(AgentKind, String)]| {
                    let cell = |(kind, name): &(AgentKind, String)| {
                        div()
                            .w(px(150.0))
                            .h(px(36.0))
                            .flex()
                            .items_center()
                            .gap(px(8.0))
                            .child(
                                StatusGlyph::new(*kind, StatusState::IdleSeen, 16.0, colors)
                                    .rendered_mark(),
                            )
                            .child(
                                StatusGlyph::new(*kind, StatusState::IdleSeen, 32.0, colors)
                                    .rendered_mark(),
                            )
                            .child(
                                div()
                                    .text_size(px(Typo::META.size))
                                    .text_color(colors.primary)
                                    .child(name.clone()),
                            )
                    };
                    div()
                        .w(px(620.0))
                        .p(px(14.0))
                        .flex()
                        .flex_wrap()
                        .bg(colors.background)
                        .children(marks.iter().map(cell))
                };
                div()
                    .flex()
                    .flex_wrap()
                    .size_full()
                    .children(self.panels.iter().map(|colors| panel(*colors, &self.marks)))
            }
        }

        let output = std::env::var("DIRI_VISUAL_OUTPUT").expect("set DIRI_VISUAL_OUTPUT");
        diri_ui::set_mark_rasterizer(crate::macos::brand_raster::raster_mark_for_fixture);
        let (manifests, _) =
            diri_engine::ManifestEngine::load_dir(&diri_engine::detect::bundled_manifest_dir())
                .unwrap();
        let mut marks: Vec<(AgentKind, String)> = [
            ProtoAgentKind::CLAUDE_CODE,
            ProtoAgentKind::CODEX,
            ProtoAgentKind::CURSOR,
            ProtoAgentKind::GEMINI,
        ]
        .iter()
        .map(|kind| (ui_agent_kind(kind), ui_agent_kind(kind).label().to_owned()))
        .collect();
        marks.extend(diri_ui::AGENT_MARKS.iter().map(|mark| {
            let name = diri_proto::agent_names::recognized_agent_name(mark.id())
                .map(str::to_owned)
                .or_else(|| {
                    let raw = manifests.raw_agent(mark.id())?.clone();
                    serde_json::from_value::<diri_proto::AgentDescriptor>(raw)
                        .ok()
                        .map(|descriptor| descriptor.display_name)
                })
                .unwrap_or_else(|| mark.id().to_owned());
            (AgentKind::Agent(mark), name)
        }));
        let panels = [
            TermTheme::DIRIJOR_DARK.id,
            TermTheme::DIRIJOR_LIGHT.id,
            TermTheme::TOKYO_NIGHT.id,
            TermTheme::SOLARIZED_LIGHT.id,
        ]
        .into_iter()
        .map(crate::app_theme::sidebar_colors)
        .collect();

        let platform = gpui_platform::current_platform(true);
        let mut cx = HeadlessAppContext::with_platform(
            platform.text_system(),
            Arc::new(diri_ui::IconAssets),
            gpui_platform::current_headless_renderer,
        );
        cx.update(|cx| crate::fonts::init(cx));
        let window = cx
            .open_window(size(px(1240.0), px(1120.0)), |_, cx| {
                cx.new(|_| Grid { panels, marks })
            })
            .unwrap();
        cx.run_until_parked();
        cx.capture_screenshot(window.into())
            .unwrap()
            .save(output)
            .unwrap();
    }

    #[test]
    fn every_activity_frame_is_embedded_in_the_app() {
        use gpui::AssetSource;
        for path in FRAMES {
            let bytes = diri_ui::IconAssets
                .load(path)
                .expect("load activity frame")
                .expect(path);
            let svg = std::str::from_utf8(bytes.as_ref()).expect(path);
            assert!(svg.starts_with("<svg"), "{path}");
            assert!(
                svg.contains("currentColor"),
                "{path} must follow the theme color"
            );
        }
    }
}
