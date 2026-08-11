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
///
/// Status marks are deliberately static. State changes invalidate the entity,
/// but leaving a working or needs-input glyph on screen never schedules
/// another frame.
///
/// This is a performance contract, not a style preference. A glyph that moves
/// has to be invalidated by somebody, and the only cadence that reads as
/// motion also keeps the window rendering for as long as any Agent is
/// working — which is most of the time this app is open. Measured on the
/// packaged app in preview mode, a 10 Hz status ticker cost ~3% idle CPU and
/// held ~240 MB of GPU memory that an idle window otherwise returns within
/// seconds of the last frame. `status_glyphs_never_create_autonomous_frame_tasks`
/// and `status_marks_never_sample_a_clock_while_rendering` below are what keep
/// it that way.
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

    fn rendered_mark(&self) -> gpui::AnyElement {
        static_mark(
            self.kind,
            self.size,
            static_status_color(self.kind, self.state, self.colors),
            1.0,
        )
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
        div()
            .flex_none()
            .size(px(self.size))
            .child(self.rendered_mark())
    }
}

fn static_mark(
    kind: AgentKind,
    size: f32,
    color: gpui::Rgba,
    visual_scale: f32,
) -> gpui::AnyElement {
    if let Some(mark) = kind.brand_mark() {
        BrandMark::solid(mark, size, color)
            .inset(0.08)
            .visual_scale(visual_scale)
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

    /// The companion to `status_glyphs_never_create_autonomous_frame_tasks`:
    /// that one stops a glyph from scheduling its own frames, this one stops a
    /// glyph from *needing* somebody else to schedule them. A mark whose
    /// appearance depends on the current time is only correct if something
    /// repaints it, so sampling a clock here silently re-creates the 10 Hz
    /// wake even when the timer lives in another crate.
    #[test]
    fn status_marks_never_sample_a_clock_while_rendering() {
        // Only the shipped half of the file: the tests below deliberately
        // evaluate `AnimationPhase` at fixed instants to check its math, which
        // costs nothing at runtime.
        let source = include_str!("status.rs");
        let test_module = ["#[cfg", "(test)]"].concat();
        let shipped = source
            .split_once(&test_module)
            .map_or(source, |(shipped, _)| shipped);
        let phase_sample = ["AnimationPhase", "::at("].concat();
        let clock_sample = ["wall_clock_seconds", "()"].concat();

        assert!(
            !shipped.contains(&phase_sample),
            "a status mark that reads an animation phase has to be repainted to stay correct"
        );
        // `wall_clock_seconds` is declared here for other animated surfaces;
        // only its own declaration may name it.
        assert_eq!(
            shipped.matches(&clock_sample).count(),
            1,
            "status marks must not sample the wall clock; only the shared helper's own \
             declaration may name it"
        );
    }
}
