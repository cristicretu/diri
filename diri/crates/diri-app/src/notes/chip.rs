//! The mention chip: how a `diri://session/…` or `diri://note/…` link looks.
//!
//! The editor paints chips inline behind text it lays out itself; anything
//! else in a note (a to-do's live work state, a row's trailing status) mounts
//! [`SessionChip`] as an element. Both read the same tokens here, so a chip
//! is one look wherever it appears, and both take live status from the
//! [`MentionDirectory`] the host keeps current.

use diri_notes::mention::{MentionTarget, session_label};
use diri_ui::{AgentKind as UiAgentKind, Ink, SemanticColors, StatusState};
use gpui::{App, FontWeight, IntoElement, RenderOnce, SharedString, Window, div, prelude::*, px};

use super::editor_view::{MentionDirectory, MentionEntry};

/// A chip reaches this far past its text on each side.
pub(crate) const PAD_X: f32 = 3.0;
pub(crate) const RADIUS: f32 = 5.0;
pub(crate) const DOT: f32 = 7.0;
pub(crate) const BORDER: f32 = 0.5;

pub(crate) fn fill(colors: SemanticColors) -> gpui::Rgba {
    diri_ui::rgba_f32(
        colors.primary.r,
        colors.primary.g,
        colors.primary.b,
        colors.primary.a * 0.07,
    )
}

pub(crate) fn border(colors: SemanticColors) -> gpui::Rgba {
    diri_ui::rgba_f32(
        colors.primary.r,
        colors.primary.g,
        colors.primary.b,
        colors.primary.a * 0.1,
    )
}

/// A session's live status as one ink, for the dot at the head of its chip.
pub(crate) fn status_ink(
    agent: UiAgentKind,
    state: StatusState,
    colors: SemanticColors,
) -> gpui::Rgba {
    match state {
        StatusState::Working => Ink::working(agent, colors),
        StatusState::NeedsInput { destructive: false } => Ink::on_surface(Ink::ATTENTION, colors),
        StatusState::NeedsInput { destructive: true } => Ink::on_surface(Ink::DANGER, colors),
        StatusState::DoneUnseen => Ink::on_surface(Ink::FRESH, colors),
        StatusState::IdleSeen | StatusState::ShellsRunning => colors.secondary,
        StatusState::None | StatusState::Hibernated => colors.tertiary,
    }
}

/// What sits at the head of a chip where its `@` would be.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum ChipDot {
    /// A live session, filled with its status ink.
    Status(gpui::Rgba),
    /// A session the directory no longer knows: archived or removed.
    Gone,
    /// A note: its `@` stays, faded.
    None,
}

impl ChipDot {
    /// The dot for `target`, read live from `directory`.
    pub(crate) fn for_target(
        target: &MentionTarget,
        directory: &MentionDirectory,
        colors: SemanticColors,
    ) -> Self {
        match target {
            MentionTarget::Note(_) => Self::None,
            MentionTarget::Session(_) => directory
                .find(target)
                .and_then(|entry| entry.agent.zip(entry.status))
                .map_or(Self::Gone, |(agent, state)| {
                    Self::Status(status_ink(agent, state, colors))
                }),
        }
    }
}

/// A mention chip as a standalone element: `● Codex: fix resize`.
#[cfg_attr(
    not(test),
    allow(dead_code, reason = "API for the to-do handoff (notes/todo-handoff)")
)]
#[derive(IntoElement)]
pub(crate) struct SessionChip {
    label: SharedString,
    dot: ChipDot,
    colors: SemanticColors,
}

#[cfg_attr(
    not(test),
    allow(dead_code, reason = "API for the to-do handoff (notes/todo-handoff)")
)]
impl SessionChip {
    /// The live chip for a directory entry.
    pub(crate) fn for_entry(entry: &MentionEntry, colors: SemanticColors) -> Self {
        Self {
            label: entry
                .candidate
                .label
                .trim_start_matches('@')
                .to_owned()
                .into(),
            dot: ChipDot::for_target(
                &entry.candidate.target,
                &MentionDirectory {
                    entries: vec![entry.clone()],
                },
                colors,
            ),
            colors,
        }
    }

    /// The live chip for session `id`, or a hollow "gone" chip labelled
    /// `fallback` when the directory no longer has it.
    pub(crate) fn for_session(
        id: &str,
        fallback: &str,
        directory: &MentionDirectory,
        colors: SemanticColors,
    ) -> Self {
        let target = MentionTarget::Session(id.to_owned());
        match directory.find(&target) {
            Some(entry) => Self::for_entry(entry, colors),
            None => Self {
                label: session_label("", fallback)
                    .trim_start_matches('@')
                    .to_owned()
                    .into(),
                dot: ChipDot::Gone,
                colors,
            },
        }
    }
}

impl RenderOnce for SessionChip {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let colors = self.colors;
        let dot = match self.dot {
            ChipDot::Status(ink) => Some(div().size(px(DOT)).rounded_full().bg(ink)),
            ChipDot::Gone => Some(
                div()
                    .size(px(DOT))
                    .rounded_full()
                    .border(px(1.25))
                    .border_color(colors.tertiary),
            ),
            ChipDot::None => None,
        };
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(5.0))
            .h(px(22.0))
            .px(px(PAD_X + 3.0))
            .rounded(px(RADIUS))
            .bg(fill(colors))
            .border(px(BORDER))
            .border_color(border(colors))
            .text_size(px(13.0))
            .font_weight(FontWeight::MEDIUM)
            .text_color(colors.primary)
            .children(dot)
            .when(self.dot == ChipDot::None, |chip| {
                chip.child(div().text_color(colors.tertiary).child("@"))
            })
            .child(div().whitespace_nowrap().child(self.label))
    }
}
