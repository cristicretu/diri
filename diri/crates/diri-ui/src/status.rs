use std::time::{SystemTime, UNIX_EPOCH};

use gpui::{
    App, AppContext as _, Context, Entity, IntoElement, Render, RenderOnce, Window, div,
    prelude::*, px,
};

use crate::{AgentKind, BrandMark, Ink, Motion, SemanticColors};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatusState {
    Working,
    NeedsInput { destructive: bool },
    DoneUnseen,
    IdleSeen,
    None,
    Hibernated,
}

impl StatusState {
    pub const ALL: [Self; 7] = [
        Self::Working,
        Self::NeedsInput { destructive: false },
        Self::NeedsInput { destructive: true },
        Self::DoneUnseen,
        Self::IdleSeen,
        Self::None,
        Self::Hibernated,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Working => "Working",
            Self::NeedsInput { destructive: false } => "Needs input",
            Self::NeedsInput { destructive: true } => "Needs input · destructive",
            Self::DoneUnseen => "Done · unseen",
            Self::IdleSeen => "Idle · seen",
            Self::None => "Ended",
            Self::Hibernated => "Hibernated",
        }
    }
}

/// All periodic values evaluated from one absolute clock sample.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AnimationPhase {
    pub breathe_scale: f32,
    pub waiting_scale: f32,
    pub needs_input_pulse: f32,
    pub needs_input_opacity: f32,
    pub sweep_turns: f32,
    pub shell_on: bool,
}

impl AnimationPhase {
    pub fn at(seconds: f64) -> Self {
        let wave = |period: f64| (seconds * std::f64::consts::TAU / period).sin() as f32;
        let needs_input_pulse = 0.5 + 0.5 * wave(Motion::PING_PERIOD);
        Self {
            breathe_scale: 1.0 + 0.055 * wave(Motion::BREATHE),
            waiting_scale: 1.0 + 0.05 * wave(Motion::BREATHE),
            needs_input_pulse,
            // Swift's PulsingMark applies the pulse through a second 0.5→1 map.
            needs_input_opacity: 0.5 + 0.5 * needs_input_pulse,
            sweep_turns: (seconds / Motion::SWEEP_REV).rem_euclid(1.0) as f32,
            shell_on: wave(Motion::SHELL_BLINK) > 0.0,
        }
    }
}

/// Absolute Unix-wall-clock sample. Every independently mounted glyph uses
/// this same epoch, keeping animation phases synchronized without shared state.
pub fn wall_clock_seconds() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

/// Stateful status mark. Construct with `cx.new(|cx| StatusGlyph::new(...))`.
/// Motion is derived from an absolute clock; the host supplies one shared
/// invalidation cadence instead of creating one animation task per glyph.
pub struct StatusGlyph {
    kind: AgentKind,
    state: StatusState,
    size: f32,
    colors: SemanticColors,
}

impl StatusGlyph {
    pub fn new(kind: AgentKind, state: StatusState, size: f32, colors: SemanticColors) -> Self {
        Self {
            kind,
            state,
            size,
            colors,
        }
    }

    pub fn set_state(&mut self, state: StatusState, _window: &mut Window, cx: &mut Context<Self>) {
        if self.state != state {
            self.state = state;
            cx.notify();
        }
    }

    pub fn set_kind(&mut self, kind: AgentKind, cx: &mut Context<Self>) {
        if self.kind != kind {
            self.kind = kind;
            cx.notify();
        }
    }

    pub fn set_colors(&mut self, colors: SemanticColors, cx: &mut Context<Self>) {
        if self.colors != colors {
            self.colors = colors;
            cx.notify();
        }
    }

    /// Whether a shared host clock should invalidate this glyph. Keeping this
    /// decision on the glyph prevents a single animated Agent from repainting
    /// every static status mark in a large sidebar.
    pub const fn needs_animation(&self) -> bool {
        matches!(
            self.state,
            StatusState::Working | StatusState::NeedsInput { .. }
        )
    }

    fn rendered_mark(&self, phase: AnimationPhase) -> gpui::AnyElement {
        let mut color = static_status_color(self.kind, self.state, self.colors);
        let mut visual_scale = 1.0;
        let mut rotation_turns = 0.0;
        match self.state {
            StatusState::Working => {
                visual_scale = phase.breathe_scale;
                let direction = if self.kind == AgentKind::Codex {
                    -1.0
                } else {
                    1.0
                };
                rotation_turns = direction * phase.sweep_turns;
            }
            StatusState::NeedsInput { .. } => {
                visual_scale = phase.waiting_scale;
                color = color.opacity(phase.needs_input_opacity);
            }
            StatusState::DoneUnseen
            | StatusState::IdleSeen
            | StatusState::None
            | StatusState::Hibernated => {}
        }
        static_mark(self.kind, self.size, color, visual_scale, rotation_turns)
    }

    pub fn entity(
        kind: AgentKind,
        state: StatusState,
        size: f32,
        colors: SemanticColors,
        cx: &mut App,
    ) -> Entity<Self> {
        cx.new(|_| Self::new(kind, state, size, colors))
    }
}

fn static_status_color(kind: AgentKind, state: StatusState, colors: SemanticColors) -> gpui::Rgba {
    match state {
        StatusState::Working => Ink::working(kind, colors).opacity(0.96),
        StatusState::NeedsInput { destructive: false } => Ink::ATTENTION,
        StatusState::NeedsInput { destructive: true } => Ink::DANGER,
        StatusState::DoneUnseen => Ink::FRESH,
        StatusState::IdleSeen => colors.primary.alpha(0.42),
        StatusState::None => colors.primary.alpha(0.28),
        StatusState::Hibernated => colors.primary.alpha(0.36),
    }
}

impl Render for StatusGlyph {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let phase = AnimationPhase::at(wall_clock_seconds());
        div()
            .flex_none()
            .size(px(self.size))
            .child(self.rendered_mark(phase))
    }
}

fn static_mark(
    kind: AgentKind,
    size: f32,
    color: gpui::Rgba,
    visual_scale: f32,
    rotation_turns: f32,
) -> gpui::AnyElement {
    if let Some(mark) = kind.brand_mark() {
        BrandMark::solid(mark, size, color)
            .inset(0.08)
            .visual_scale(visual_scale)
            .rotation_turns(rotation_turns)
            .into_any_element()
    } else {
        shell_caret(size, color, visual_scale)
    }
}

fn shell_caret(size: f32, color: gpui::Rgba, visual_scale: f32) -> gpui::AnyElement {
    div()
        .flex_none()
        .flex()
        .size(px(size))
        .items_center()
        .justify_center()
        .child(
            div()
                .w(px(size * 0.42 * visual_scale))
                .h(px(size * 0.62 * visual_scale))
                .rounded(px(1.0))
                .bg(color),
        )
        .into_any_element()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttentionLevel {
    NeedsInput { destructive: bool },
    DoneUnseen,
    Working,
    IdleSeen,
    None,
    Hibernated,
}

/// Compact project/menu-bar rollup. Quiet states intentionally render nothing.
#[derive(IntoElement)]
pub struct AttentionDot {
    level: AttentionLevel,
    colors: SemanticColors,
}

impl AttentionDot {
    pub const fn new(level: AttentionLevel, colors: SemanticColors) -> Self {
        Self { level, colors }
    }
}

impl RenderOnce for AttentionDot {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let (size, color) = match self.level {
            AttentionLevel::NeedsInput { destructive: false } => (6.0, Some(Ink::ATTENTION)),
            AttentionLevel::NeedsInput { destructive: true } => (6.0, Some(Ink::DANGER)),
            AttentionLevel::DoneUnseen => (6.0, Some(Ink::FRESH)),
            AttentionLevel::Working => (5.0, Some(self.colors.primary.alpha(0.54))),
            AttentionLevel::IdleSeen | AttentionLevel::None | AttentionLevel::Hibernated => {
                (0.0, None)
            }
        };
        div()
            .flex_none()
            .size(px(size))
            .rounded_full()
            .when_some(color, |element, color| element.bg(color))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn animation_phase_matches_swift_math() {
        let phase = AnimationPhase::at(0.0);
        assert_eq!(phase.breathe_scale, 1.0);
        assert_eq!(phase.needs_input_pulse, 0.5);
        assert_eq!(phase.needs_input_opacity, 0.75);
        assert_eq!(phase.sweep_turns, 0.0);
        assert!(!phase.shell_on);

        let peak = AnimationPhase::at(Motion::PING_PERIOD / 4.0);
        assert!((peak.needs_input_pulse - 1.0).abs() < 0.0001);
        assert!((peak.needs_input_opacity - 1.0).abs() < 0.0001);
    }

    #[test]
    fn codex_is_the_only_reversed_sweep() {
        for kind in AgentKind::ALL {
            let direction = if kind == AgentKind::Codex { -1.0 } else { 1.0 };
            assert_eq!(direction < 0.0, kind == AgentKind::Codex);
        }
    }

    #[test]
    fn static_status_colors_preserve_state_transitions() {
        let colors = SemanticColors::dark();
        assert_eq!(
            static_status_color(
                AgentKind::Codex,
                StatusState::NeedsInput { destructive: false },
                colors,
            ),
            Ink::ATTENTION
        );
        assert_eq!(
            static_status_color(
                AgentKind::Codex,
                StatusState::NeedsInput { destructive: true },
                colors,
            ),
            Ink::DANGER
        );
        assert_eq!(
            static_status_color(AgentKind::Codex, StatusState::DoneUnseen, colors),
            Ink::FRESH
        );
        assert_ne!(
            static_status_color(AgentKind::Codex, StatusState::Working, colors),
            static_status_color(AgentKind::Codex, StatusState::IdleSeen, colors)
        );
        assert_ne!(
            static_status_color(AgentKind::Codex, StatusState::IdleSeen, colors),
            static_status_color(AgentKind::Codex, StatusState::None, colors)
        );
    }

    #[test]
    fn only_motion_states_request_shared_clock_invalidations() {
        let colors = SemanticColors::dark();
        for state in StatusState::ALL {
            let glyph = StatusGlyph::new(AgentKind::Codex, state, 16.0, colors);
            assert_eq!(
                glyph.needs_animation(),
                matches!(state, StatusState::Working | StatusState::NeedsInput { .. })
            );
        }
    }

    #[test]
    fn status_glyphs_never_create_autonomous_frame_tasks() {
        let source = include_str!("status.rs");
        let window_task = ["spawn", "_in(window"].concat();
        let periodic_timer = ["background_executor()", ".timer("].concat();

        assert!(
            !source.contains(&window_task),
            "status glyph rendering must stay event-driven"
        );
        assert!(
            !source.contains(&periodic_timer),
            "status glyphs must not own periodic frame timers"
        );
    }

    #[test]
    fn working_status_render_uses_the_absolute_shared_phase() {
        let source = include_str!("status.rs");
        let shared_phase = ["AnimationPhase::at", "(wall_clock_seconds())"].concat();
        assert!(
            source.contains(&shared_phase),
            "status motion must resume at the current phase after inactive-window throttling"
        );
    }
}
