//! Hold ⌘ to see the shortcuts where they apply.
//!
//! Holding Command on its own for a moment fades small shortcut labels onto
//! the controls they operate -- `⌘3` on the third session row, `⌘B` under the
//! sidebar toggle -- and releasing it fades them out. It is the Mac version of
//! the iPadOS hold-⌘ sheet, placed on the controls instead of in a cheat sheet.
//!
//! The behavior is a small pure state machine, [`HeldHints`], fed modifier,
//! key, pointer and activation events with explicit instants so the timing is
//! testable. `RootView` owns one per window and publishes it through the
//! [`HeldHintsState`] global; each view that paints a hint reads it at render
//! time with [`opacity`]. Nothing here runs at rest: the only timer is the
//! one-shot hold delay, and frames are requested only while a fade is moving.

use std::time::{Duration, Instant};

use diri_ui::{Motion, SemanticColors, Typo};
use gpui::{
    AnyElement, App, Global, InteractiveElement, IntoElement, Modifiers, ParentElement, Styled,
    Window, WindowId, div, px,
};

#[cfg(test)]
use crate::commands::ShortcutOverrides;
use crate::commands::{self, CommandId};

/// How long ⌘ must be held alone before the hints appear.
///
/// A ⌘ chord's lead -- Command down to the letter down -- is well under half
/// a second even when the hand has to travel to ⌘, or ⌘9. Anything with a
/// second modifier (⌥⌘T, ⇧⌘D) cancels as soon as that modifier lands, so the
/// delay only has to outlast the slowest single-letter chord. 700 ms clears
/// that with margin while still arriving before a deliberate hold starts to
/// feel like waiting.
pub(crate) const HOLD_DELAY: Duration = Duration::from_millis(700);
/// Appearing uses the shared overlay fade.
pub(crate) const FADE_IN: Duration = Duration::from_millis((Motion::OVERLAY_FADE * 1000.0) as u64);
/// Leaving is quicker than arriving: the hand has already moved on.
pub(crate) const FADE_OUT: Duration =
    Duration::from_millis((Motion::OVERLAY_FADE_OUT * 1000.0) as u64);

#[derive(Clone, Copy, Debug, PartialEq)]
enum Visibility {
    Hidden,
    /// Fading in (or fully shown once `FADE_IN` has passed).
    Shown {
        since: Instant,
    },
    /// Fading out from `from` opacity.
    Hiding {
        since: Instant,
        from: f32,
    },
}

/// The hold-⌘ state machine.
///
/// `idle` -> `armed` when ⌘ goes down alone -> `shown` once the hold delay
/// passes -> `hidden` when ⌘ is released. Any other key, a second modifier or
/// a click while ⌘ is down turns the hold into a chord: the hints never
/// appear (or leave at once) and stay away until ⌘ is released. Losing
/// window activation resets everything instantly.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct HeldHints {
    command_down: bool,
    /// This ⌘ press has been used for a chord or a ⌘-click.
    chorded: bool,
    /// Set while waiting out the hold delay. The generation lets a stale
    /// timer from an earlier press fall on the floor.
    armed: Option<(u64, Instant)>,
    generation: u64,
    visibility: Visibility,
}

impl Default for HeldHints {
    fn default() -> Self {
        Self {
            command_down: false,
            chorded: false,
            armed: None,
            generation: 0,
            visibility: Visibility::Hidden,
        }
    }
}

/// What the owner should do after feeding an event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HintEffect {
    /// Nothing visible changed.
    None,
    /// Start the one-shot hold timer for this generation.
    Arm(u64),
    /// Visibility changed; repaint the views that carry hints.
    Repaint,
}

impl HeldHints {
    /// Feeds the full modifier state after a flags change.
    pub(crate) fn modifiers_changed(&mut self, modifiers: Modifiers, now: Instant) -> HintEffect {
        let others = modifiers.shift || modifiers.alt || modifiers.control || modifiers.function;
        if !modifiers.platform {
            self.command_down = false;
            self.chorded = false;
            self.armed = None;
            return self.hide(now);
        }
        if others {
            // ⌥⌘T, ⇧⌘D, ⌃⌘↑: a chord in progress, not a hold.
            self.command_down = true;
            self.chorded = true;
            self.armed = None;
            return self.hide(now);
        }
        if self.command_down {
            // Back to ⌘ alone after letting go of ⇧: still the same press.
            return HintEffect::None;
        }
        self.command_down = true;
        self.chorded = false;
        self.generation = self.generation.wrapping_add(1);
        self.armed = Some((self.generation, now));
        HintEffect::Arm(self.generation)
    }

    /// Any key down. While ⌘ is held this is a shortcut, and hints must never
    /// flash during one.
    pub(crate) fn key_down(&mut self, now: Instant) -> HintEffect {
        self.chord(now)
    }

    /// A pointer press. ⌘-click is its own gesture (open a link, multi-select),
    /// so it cancels the hold the same way a key does.
    pub(crate) fn pointer_down(&mut self, now: Instant) -> HintEffect {
        self.chord(now)
    }

    /// The window stopped being key. Hints vanish without a fade: the window
    /// is no longer where the user is looking.
    pub(crate) fn deactivated(&mut self) -> HintEffect {
        let visible = self.visibility != Visibility::Hidden;
        *self = Self {
            generation: self.generation,
            ..Self::default()
        };
        if visible {
            HintEffect::Repaint
        } else {
            HintEffect::None
        }
    }

    /// The hold timer for `generation` fired. Stale timers are ignored.
    pub(crate) fn delay_elapsed(&mut self, generation: u64, now: Instant) -> HintEffect {
        match self.armed {
            Some((armed, _)) if armed == generation && self.command_down && !self.chorded => {
                self.armed = None;
                self.visibility = Visibility::Shown { since: now };
                HintEffect::Repaint
            }
            _ => HintEffect::None,
        }
    }

    /// The pending hold's generation and start, for fixtures that fire the
    /// timer on a stepped clock.
    #[cfg(all(test, target_os = "macos"))]
    pub(crate) fn armed(&self) -> Option<(u64, Instant)> {
        self.armed
    }

    /// When the pending hold delay ends, if one is pending.
    #[cfg(test)]
    pub(crate) fn deadline(&self) -> Option<Instant> {
        self.armed.map(|(_, since)| since + HOLD_DELAY)
    }

    /// Hint opacity at `now`, 0 when hidden. Under reduce motion the hints are
    /// either fully present or absent.
    pub(crate) fn opacity(&self, now: Instant, reduce_motion: bool) -> f32 {
        match self.visibility {
            Visibility::Hidden => 0.0,
            Visibility::Shown { since } => {
                if reduce_motion {
                    1.0
                } else {
                    diri_ui::motion::settle(progress(now, since, FADE_IN))
                }
            }
            Visibility::Hiding { since, from } => {
                if reduce_motion {
                    0.0
                } else {
                    from * (1.0 - diri_ui::motion::settle(progress(now, since, FADE_OUT)))
                }
            }
        }
    }

    /// Whether a fade is still moving at `now`, i.e. whether a view painting
    /// a hint needs another frame.
    pub(crate) fn animating(&self, now: Instant, reduce_motion: bool) -> bool {
        if reduce_motion {
            return false;
        }
        match self.visibility {
            Visibility::Hidden => false,
            Visibility::Shown { since } => now < since + FADE_IN,
            Visibility::Hiding { since, .. } => now < since + FADE_OUT,
        }
    }

    fn chord(&mut self, now: Instant) -> HintEffect {
        if !self.command_down {
            return HintEffect::None;
        }
        self.chorded = true;
        self.armed = None;
        self.hide(now)
    }

    fn hide(&mut self, now: Instant) -> HintEffect {
        match self.visibility {
            Visibility::Hidden | Visibility::Hiding { .. } => HintEffect::None,
            Visibility::Shown { .. } => {
                let from = self.opacity(now, false);
                self.visibility = if from > 0.0 {
                    Visibility::Hiding { since: now, from }
                } else {
                    Visibility::Hidden
                };
                HintEffect::Repaint
            }
        }
    }
}

fn progress(now: Instant, since: Instant, span: Duration) -> f32 {
    (now.saturating_duration_since(since).as_secs_f32() / span.as_secs_f32()).clamp(0.0, 1.0)
}

/// The key window's hint state, published by its `RootView`. Only one window
/// is key at a time, so one slot is enough; views in other windows read 0.
#[derive(Default)]
pub(crate) struct HeldHintsState {
    window: Option<WindowId>,
    hints: HeldHints,
    /// Frozen time for screenshot fixtures and frame strips.
    clock: Option<Instant>,
}

impl Global for HeldHintsState {}

impl HeldHintsState {
    /// Replaces the published state for `window`.
    pub(crate) fn publish(window: WindowId, hints: HeldHints, cx: &mut App) {
        let clock = cx.try_global::<Self>().and_then(|state| state.clock);
        cx.set_global(Self {
            window: Some(window),
            hints,
            clock,
        });
    }

    /// Pins the hint clock, so a capture shows one exact moment of a fade.
    /// Setting it republishes, so every view carrying hints repaints at the
    /// new moment.
    #[cfg(all(test, target_os = "macos"))]
    pub(crate) fn freeze_clock(now: Option<Instant>, cx: &mut App) {
        let (window, hints) = cx
            .try_global::<Self>()
            .map_or((None, HeldHints::default()), |state| {
                (state.window, state.hints)
            });
        cx.set_global(Self {
            window,
            hints,
            clock: now,
        });
    }
}

/// The clock hints are measured against: real time, unless a fixture froze it.
pub(crate) fn now(cx: &App) -> Instant {
    cx.try_global::<HeldHintsState>()
        .and_then(|state| state.clock)
        .unwrap_or_else(Instant::now)
}

/// This frame's hint opacity for `window`, 0 at rest. Requests the next frame
/// while a fade is moving, so the calling view keeps painting until it lands.
pub(crate) fn opacity(window: &mut Window, cx: &App) -> f32 {
    let Some(state) = cx.try_global::<HeldHintsState>() else {
        return 0.0;
    };
    if state.window != Some(window.window_handle().window_id()) {
        return 0.0;
    }
    let now = state.clock.unwrap_or_else(Instant::now);
    let reduce_motion = cx.reduce_motion();
    if state.clock.is_none() && state.hints.animating(now, reduce_motion) {
        window.request_animation_frame();
    }
    state.hints.opacity(now, reduce_motion)
}

/// The label for a command's current binding, honoring user overrides; `None`
/// when the command is unbound.
pub(crate) fn label(id: CommandId) -> Option<String> {
    command_chord(commands::command(id).shortcut_label())
}

/// Holding ⌘ asks "what does ⌘ do here", so only ⌘ chords answer. A control
/// rebound to ⌃⇧Space keeps its binding but gets no hint, which also keeps
/// long non-⌘ labels from crowding their neighbours.
fn command_chord(label: Option<String>) -> Option<String> {
    if cfg!(target_os = "macos") {
        label.filter(|label| label.contains('⌘'))
    } else {
        label
    }
}

/// The command a sidebar/tab position triggers: ranks 1–8 are the numbered
/// selections, rank 9 is always the last session.
pub(crate) fn session_command(rank: usize) -> Option<CommandId> {
    Some(match rank {
        1 => CommandId::SelectSession1,
        2 => CommandId::SelectSession2,
        3 => CommandId::SelectSession3,
        4 => CommandId::SelectSession4,
        5 => CommandId::SelectSession5,
        6 => CommandId::SelectSession6,
        7 => CommandId::SelectSession7,
        8 => CommandId::SelectSession8,
        9 => CommandId::SelectLastSession,
        _ => return None,
    })
}

/// The label for the session at `rank`, from the active bindings.
pub(crate) fn session_label(rank: usize) -> Option<String> {
    session_command(rank).and_then(label)
}

/// The label for the session at `rank` under explicit `overrides`.
#[cfg(test)]
pub(crate) fn session_label_for(rank: usize, overrides: &ShortcutOverrides) -> Option<String> {
    session_command(rank)
        .and_then(|id| command_chord(commands::command(id).shortcut_label_for(overrides)))
}

/// Hint text: menu-shortcut weight, secondary ink, no box.
pub(crate) fn text(
    selector: String,
    label: String,
    opacity: f32,
    colors: SemanticColors,
) -> gpui::Div {
    #[cfg(test)]
    if let Some(styled) = fixture_style(&selector, &label, opacity, colors) {
        return styled;
    }
    div()
        .debug_selector(move || selector)
        .flex_none()
        .whitespace_nowrap()
        .text_size(px(Typo::META.size))
        .font_weight(Typo::META.weight)
        .text_color(colors.secondary)
        .opacity(opacity)
        .child(label)
}

/// The label treatments weighed against each other in the PR's comparison
/// strip. `DIRI_HINTS_STYLE=tertiary|mono|keycap` renders one of the rejected
/// alternatives instead of the shipped secondary text.
#[cfg(test)]
fn fixture_style(
    selector: &str,
    label: &str,
    opacity: f32,
    colors: SemanticColors,
) -> Option<gpui::Div> {
    let style = std::env::var("DIRI_HINTS_STYLE").ok()?;
    let selector = selector.to_owned();
    let base = div()
        .debug_selector(move || selector)
        .flex_none()
        .whitespace_nowrap()
        .opacity(opacity)
        .child(label.to_owned());
    Some(match style.as_str() {
        "tertiary" => base
            .text_size(px(Typo::META.size))
            .font_weight(Typo::META.weight)
            .text_color(colors.tertiary),
        "mono" => base
            .font_family(crate::fonts::mono_family())
            .text_size(px(Typo::META_MONO.size))
            .text_color(colors.tertiary),
        "keycap" => crate::palette_chrome::keycap(colors)
            .w_auto()
            .min_w(px(28.0))
            .px(px(4.0))
            .bg(colors.floating_surface())
            .opacity(opacity)
            .child(label.to_owned()),
        _ => return None,
    })
}

/// Wraps an icon control so its hint sits centered just below it, outside
/// layout. The label is deferred so it paints above neighbouring content and
/// escapes the title bar's clip. Returns the control untouched when there is
/// nothing to show.
pub(crate) fn below(
    control: AnyElement,
    selector: &str,
    label: Option<String>,
    opacity: f32,
    colors: SemanticColors,
) -> AnyElement {
    let Some(label) = label.filter(|_| opacity > 0.0) else {
        return control;
    };
    div()
        .relative()
        .flex_none()
        .child(control)
        .child(
            gpui::deferred(
                div()
                    .absolute()
                    // Tucked up into the control's own bottom padding, under
                    // its glyph, so the label clears the content below.
                    .top_full()
                    .mt(px(-4.0))
                    .left(px(-24.0))
                    .right(px(-24.0))
                    .flex()
                    .justify_center()
                    .child(text(
                        format!("held-hint:{selector}"),
                        label,
                        opacity,
                        colors,
                    )),
            )
            .with_priority(1),
        )
        .into_any_element()
}

/// Crossfades a small trailing mark (an agent logo) into its hint, right
/// aligned in the same slot, so a row's title never moves.
pub(crate) fn in_slot(
    mark: AnyElement,
    size: f32,
    selector: String,
    label: Option<String>,
    opacity: f32,
    colors: SemanticColors,
) -> AnyElement {
    slot(mark, size, selector, label, opacity, colors, true)
}

/// `in_slot` for a leading mark: the label starts at the slot's left edge.
pub(crate) fn in_leading_slot(
    mark: AnyElement,
    size: f32,
    selector: String,
    label: Option<String>,
    opacity: f32,
    colors: SemanticColors,
) -> AnyElement {
    slot(mark, size, selector, label, opacity, colors, false)
}

fn slot(
    mark: AnyElement,
    size: f32,
    selector: String,
    label: Option<String>,
    opacity: f32,
    colors: SemanticColors,
    trailing: bool,
) -> AnyElement {
    let Some(label) = label.filter(|_| opacity > 0.0) else {
        return mark;
    };
    div()
        .relative()
        .size(px(size))
        .flex_none()
        .child(div().size_full().opacity(1.0 - opacity).child(mark))
        .child({
            let anchor = div().absolute().top_0().bottom_0().flex().items_center();
            let anchor = if trailing {
                anchor.right_0().justify_end()
            } else {
                anchor.left_0()
            };
            anchor.child(text(selector, label, opacity, colors))
        })
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command() -> Modifiers {
        Modifiers {
            platform: true,
            ..Modifiers::default()
        }
    }

    fn held(t0: Instant) -> (HeldHints, u64) {
        let mut hints = HeldHints::default();
        let HintEffect::Arm(generation) = hints.modifiers_changed(command(), t0) else {
            panic!("⌘ alone arms the hold");
        };
        (hints, generation)
    }

    #[test]
    fn a_held_command_shows_hints_only_after_the_delay_and_fades_in() {
        let t0 = Instant::now();
        let (mut hints, generation) = held(t0);
        assert_eq!(hints.deadline(), Some(t0 + HOLD_DELAY));
        assert_eq!(hints.opacity(t0 + HOLD_DELAY / 2, false), 0.0);

        let shown = t0 + HOLD_DELAY;
        assert_eq!(hints.delay_elapsed(generation, shown), HintEffect::Repaint);
        assert_eq!(hints.opacity(shown, false), 0.0, "the fade starts at zero");
        let midway = hints.opacity(shown + FADE_IN / 2, false);
        assert!(midway > 0.5 && midway < 1.0, "{midway}");
        assert!(hints.animating(shown + FADE_IN / 2, false));
        assert_eq!(hints.opacity(shown + FADE_IN, false), 1.0);
        assert!(
            !hints.animating(shown + FADE_IN, false),
            "nothing moves at rest"
        );
    }

    #[test]
    fn releasing_command_fades_the_hints_out_quicker_than_they_came() {
        let t0 = Instant::now();
        let (mut hints, generation) = held(t0);
        let shown = t0 + HOLD_DELAY;
        hints.delay_elapsed(generation, shown);
        let release = shown + Duration::from_secs(2);
        assert_eq!(
            hints.modifiers_changed(Modifiers::default(), release),
            HintEffect::Repaint
        );
        assert_eq!(hints.opacity(release, false), 1.0);
        assert!(hints.opacity(release + FADE_OUT / 2, false) < 0.5);
        assert_eq!(hints.opacity(release + FADE_OUT, false), 0.0);
        assert!(!hints.animating(release + FADE_OUT, false));
        assert!(FADE_OUT < FADE_IN);
    }

    #[test]
    fn a_release_before_the_delay_never_shows_anything() {
        let t0 = Instant::now();
        let (mut hints, generation) = held(t0);
        hints.modifiers_changed(Modifiers::default(), t0 + Duration::from_millis(300));
        assert_eq!(
            hints.delay_elapsed(generation, t0 + HOLD_DELAY),
            HintEffect::None,
            "the stale timer lands on nothing"
        );
        assert_eq!(hints.opacity(t0 + HOLD_DELAY + FADE_IN, false), 0.0);
    }

    #[test]
    fn a_command_chord_never_flashes_hints() {
        // ⌘T: Command down, T 150 ms later, still holding ⌘ past the delay.
        let t0 = Instant::now();
        let (mut hints, generation) = held(t0);
        hints.key_down(t0 + Duration::from_millis(150));
        assert_eq!(
            hints.delay_elapsed(generation, t0 + HOLD_DELAY),
            HintEffect::None
        );
        for ms in (0..2_000).step_by(50) {
            assert_eq!(hints.opacity(t0 + Duration::from_millis(ms), false), 0.0);
        }
        // Holding on after the chord does not re-arm: it is the same press.
        assert_eq!(
            hints.modifiers_changed(command(), t0 + Duration::from_secs(1)),
            HintEffect::None
        );
        assert_eq!(hints.deadline(), None);
    }

    #[test]
    fn a_second_modifier_or_a_click_cancels_the_hold() {
        let t0 = Instant::now();
        for cancel in [
            |hints: &mut HeldHints, at| {
                hints.modifiers_changed(
                    Modifiers {
                        platform: true,
                        alt: true,
                        ..Modifiers::default()
                    },
                    at,
                )
            },
            |hints: &mut HeldHints, at| hints.pointer_down(at),
        ] {
            let (mut hints, generation) = held(t0);
            cancel(&mut hints, t0 + Duration::from_millis(100));
            assert_eq!(
                hints.delay_elapsed(generation, t0 + HOLD_DELAY),
                HintEffect::None
            );
            assert_eq!(hints.opacity(t0 + Duration::from_secs(2), false), 0.0);
        }
    }

    #[test]
    fn a_key_while_shown_dismisses_the_hints_until_the_next_press() {
        // Hold ⌘, read the hints, press 3: session 3 is selected and the
        // hints leave even though ⌘ is still down.
        let t0 = Instant::now();
        let (mut hints, generation) = held(t0);
        let shown = t0 + HOLD_DELAY;
        hints.delay_elapsed(generation, shown);
        let press = shown + Duration::from_millis(400);
        assert_eq!(hints.key_down(press), HintEffect::Repaint);
        assert_eq!(hints.opacity(press + FADE_OUT, false), 0.0);
        // A fresh press starts over.
        hints.modifiers_changed(Modifiers::default(), press + Duration::from_millis(50));
        let again = press + Duration::from_millis(100);
        assert!(matches!(
            hints.modifiers_changed(command(), again),
            HintEffect::Arm(next) if next != generation
        ));
    }

    #[test]
    fn deactivation_hides_at_once_and_forgets_the_press() {
        let t0 = Instant::now();
        let (mut hints, generation) = held(t0);
        hints.delay_elapsed(generation, t0 + HOLD_DELAY);
        assert_eq!(hints.deactivated(), HintEffect::Repaint);
        assert_eq!(hints.opacity(t0 + HOLD_DELAY, false), 0.0, "no fade");
        assert_eq!(
            hints.delay_elapsed(generation, t0 + HOLD_DELAY),
            HintEffect::None
        );
        // Coming back with ⌘ still down arms a new hold from scratch.
        assert!(matches!(
            hints.modifiers_changed(command(), t0 + Duration::from_secs(3)),
            HintEffect::Arm(_)
        ));
        // Deactivating while hidden changes nothing visible.
        let mut idle = HeldHints::default();
        assert_eq!(idle.deactivated(), HintEffect::None);
    }

    #[test]
    fn reduce_motion_shows_and_hides_without_a_fade() {
        let t0 = Instant::now();
        let (mut hints, generation) = held(t0);
        let shown = t0 + HOLD_DELAY;
        hints.delay_elapsed(generation, shown);
        assert_eq!(hints.opacity(shown, true), 1.0);
        assert!(!hints.animating(shown, true));
        hints.modifiers_changed(Modifiers::default(), shown + Duration::from_millis(10));
        assert_eq!(hints.opacity(shown + Duration::from_millis(10), true), 0.0);
        assert!(!hints.animating(shown + Duration::from_millis(10), true));
    }

    #[test]
    fn keys_without_command_are_ignored() {
        let t0 = Instant::now();
        let mut hints = HeldHints::default();
        assert_eq!(hints.key_down(t0), HintEffect::None);
        assert_eq!(hints.pointer_down(t0), HintEffect::None);
        assert_eq!(hints, HeldHints::default());
    }

    #[test]
    fn session_hints_read_the_real_bindings_including_overrides() {
        let defaults = ShortcutOverrides::new();
        let expected = |key: &str| commands::primary_shortcut_label(key);
        assert_eq!(session_label_for(1, &defaults), Some(expected("1")));
        assert_eq!(session_label_for(8, &defaults), Some(expected("8")));
        // ⌘9 is the last session, not the ninth.
        assert_eq!(session_command(9), Some(CommandId::SelectLastSession));
        assert_eq!(session_label_for(9, &defaults), Some(expected("9")));
        assert_eq!(session_label_for(10, &defaults), None);

        let mut custom = ShortcutOverrides::new();
        custom.insert("select-session-3".to_owned(), Some("ctrl-3".to_owned()));
        custom.insert("select-session-4".to_owned(), None);
        // Rebound off ⌘ entirely: the binding stands, but holding ⌘ is not
        // how you reach it, so the row stays quiet.
        #[cfg(target_os = "macos")]
        assert_eq!(session_label_for(3, &custom), None);
        assert_eq!(
            session_label_for(4, &custom),
            None,
            "an unbound row has no hint"
        );
        let mut rebound = ShortcutOverrides::new();
        rebound.insert("select-session-2".to_owned(), Some("cmd-alt-2".to_owned()));
        assert_eq!(
            session_label_for(2, &rebound),
            commands::command(CommandId::SelectSession2).shortcut_label_for(&rebound)
        );
    }
}
