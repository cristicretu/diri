//! Hold ⌘T for a ring of everything ⌘T could start.
//!
//! A tap of the New Session shortcut still starts the default Agent. Holding
//! it past [`HOLD_DELAY`] opens a ring under the pointer (or in the middle of
//! the window when the pointer is elsewhere) with every quick-create Agent,
//! Terminal and Note around it. Moving toward a slice selects it -- the
//! slices reach outward without limit, so a flick in the right direction is
//! enough -- and letting go of the shortcut starts what is selected. Letting
//! go without choosing leaves the ring open to click, the way a pie menu
//! does; Escape, a click in the hub or a click away closes it.
//!
//! The key repeat the shortcut used to turn into a burst of spawns is
//! swallowed here: one press is one action, whatever the repeat rate.
//!
//! [`RadialPress`] is a pure state machine fed the shortcut's down/up edges,
//! the hold timer and the pointer, so the gesture is testable without a
//! window. Geometry ([`slot_at`], [`arrange`]) is pure as well. Rendering is a
//! function of the open ring and the instant being drawn.

use std::f32::consts::TAU;
use std::time::{Duration, Instant};

use diri_proto::AgentKind as ProtoAgentKind;
use diri_ui::{AgentLogo, Glass, SemanticColors, Typo};
use gpui::{
    AnyElement, BoxShadow, Bounds, Hsla, IntoElement, ParentElement, PathBuilder,
    Pixels, Point, Rgba, Size, Styled, Window, canvas, div, point, px,
};

/// How long the shortcut must stay down before the ring replaces the tap.
///
/// A deliberate tap is released in well under 150 ms; the first key repeat
/// arrives at the system's initial repeat delay (225 ms at the fastest
/// setting). Opening just past that keeps the tap instant-feeling while a
/// hold answers before it starts to feel like waiting.
pub(crate) const HOLD_DELAY: Duration = Duration::from_millis(230);
/// Pointer travel from the centre before a slice is chosen.
pub(crate) const DEAD_ZONE: f32 = 18.0;
/// Outer radius of the ring.
pub(crate) const OUTER_RADIUS: f32 = 112.0;
/// Radius of the hub that holds the label.
pub(crate) const HUB_RADIUS: f32 = 46.0;
/// Where the marks sit, midway across the band.
const MARK_RADIUS: f32 = (OUTER_RADIUS + HUB_RADIUS) / 2.0 + 1.0;
const MARK_SIZE: f32 = 26.0;
/// The round highlight behind a chosen mark.
const HIGHLIGHT_RADIUS: f32 = 24.0;
/// Clearance kept between the ring and the window edge.
const EDGE_MARGIN: f32 = 12.0;
/// Once the shortcut is released, a click this far past the ring is a click
/// away rather than a choice.
const STICKY_REACH: f32 = 36.0;

const APPEAR: Duration = Duration::from_millis(150);
const LEAVE: Duration = Duration::from_millis(120);
const HIGHLIGHT_GLIDE: Duration = Duration::from_millis(110);
const HIGHLIGHT_FADE: Duration = Duration::from_millis(90);

/// What a slice starts.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RingItem {
    pub kind: ProtoAgentKind,
    pub title: String,
    /// The Agent a tap of the shortcut would start.
    pub is_default: bool,
}

/// Lays the items around the ring so the hand can learn it: the default
/// Agent at twelve o'clock, the other Agents clockwise from it, and Terminal
/// and Note together at six o'clock, wherever the Agent count leaves them.
pub(crate) fn arrange(agents: Vec<RingItem>, extras: Vec<RingItem>) -> Vec<RingItem> {
    let count = agents.len() + extras.len();
    if count == 0 {
        return Vec::new();
    }
    let mut ring: Vec<Option<RingItem>> = vec![None; count];
    if agents.is_empty() {
        return extras;
    }
    let bottom = count / 2;
    for (offset, extra) in extras.into_iter().enumerate() {
        ring[(bottom + offset) % count] = Some(extra);
    }
    let mut agents = agents.into_iter();
    for slot in ring.iter_mut() {
        if slot.is_none() {
            *slot = agents.next();
        }
    }
    ring.into_iter().flatten().collect()
}

/// Fraction of a clockwise lap from twelve o'clock to `offset`.
fn turn_of(offset: Point<f32>) -> f32 {
    let turn = offset.x.atan2(-offset.y) / TAU;
    if turn < 0.0 { turn + 1.0 } else { turn }
}

/// The slice the pointer at `offset` from the centre selects, if it has left
/// the dead zone. `reach` bounds how far out a slice extends.
pub(crate) fn slot_at(offset: Point<f32>, count: usize, reach: Option<f32>) -> Option<usize> {
    if count == 0 {
        return None;
    }
    let distance = offset.x.hypot(offset.y);
    if distance < DEAD_ZONE || reach.is_some_and(|reach| distance > reach) {
        return None;
    }
    Some(((turn_of(offset) * count as f32).round() as usize) % count)
}

/// Centre for a ring opened at `pointer`, kept whole inside the window.
pub(crate) fn place(pointer: Option<Point<Pixels>>, window: Size<Pixels>) -> Point<Pixels> {
    let width = f32::from(window.width);
    let height = f32::from(window.height);
    let middle = point(width / 2.0, height / 2.0);
    let wanted = pointer
        .map(|p| point(f32::from(p.x), f32::from(p.y)))
        .filter(|p| p.x > 0.0 && p.y > 0.0 && p.x < width && p.y < height)
        .unwrap_or(middle);
    let clamp = |value: f32, extent: f32| {
        let low = OUTER_RADIUS + EDGE_MARGIN;
        let high = extent - OUTER_RADIUS - EDGE_MARGIN;
        if low > high {
            extent / 2.0
        } else {
            value.clamp(low, high)
        }
    };
    point(px(clamp(wanted.x, width)), px(clamp(wanted.y, height)))
}

/// An open ring and how it is moving.
#[derive(Clone, Debug)]
pub(crate) struct Ring {
    pub center: Point<Pixels>,
    pub items: Vec<RingItem>,
    /// Folder the new session lands in, shown while nothing is selected.
    pub place: String,
    pub hovered: Option<usize>,
    pub pointer: Option<Point<Pixels>>,
    /// The shortcut was released with nothing chosen: the ring waits for a
    /// click instead of following the hand.
    pub sticky: bool,
    pub opened_at: Instant,
    /// Where the highlight is gliding from, and since when.
    glide_from: Option<f32>,
    glide_since: Instant,
}

impl Ring {
    pub(crate) fn new(
        center: Point<Pixels>,
        items: Vec<RingItem>,
        place: String,
        now: Instant,
    ) -> Self {
        Self {
            center,
            items,
            place,
            hovered: None,
            pointer: None,
            sticky: false,
            opened_at: now,
            glide_from: None,
            glide_since: now,
        }
    }

    fn offset(&self, position: Point<Pixels>) -> Point<f32> {
        point(
            f32::from(position.x - self.center.x),
            f32::from(position.y - self.center.y),
        )
    }

    fn reach(&self) -> Option<f32> {
        self.sticky.then_some(OUTER_RADIUS + STICKY_REACH)
    }

    /// Follows the pointer; true when the selected slice changed.
    fn track(&mut self, position: Point<Pixels>, now: Instant) -> bool {
        self.pointer = Some(position);
        let next = slot_at(self.offset(position), self.items.len(), self.reach());
        if next == self.hovered {
            return false;
        }
        self.glide_from = self.highlight_turn(now);
        self.glide_since = now;
        self.hovered = next;
        true
    }

    /// Where the highlight is drawn at `now`, as a lap fraction.
    fn highlight_turn(&self, now: Instant) -> Option<f32> {
        let target = self.hovered? as f32 / self.items.len() as f32;
        let Some(from) = self.glide_from else {
            return Some(target);
        };
        let t = ease_out(progress(self.glide_since, now, HIGHLIGHT_GLIDE));
        // The short way round.
        let mut delta = target - from;
        if delta > 0.5 {
            delta -= 1.0;
        } else if delta < -0.5 {
            delta += 1.0;
        }
        Some((from + delta * t).rem_euclid(1.0))
    }

    /// Highlight strength at `now`: fades in from nothing, holds while a
    /// slice stays selected.
    fn highlight_alpha(&self, now: Instant) -> f32 {
        if self.hovered.is_none() {
            return 0.0;
        }
        if self.glide_from.is_some() {
            return 1.0;
        }
        ease_out(progress(self.glide_since, now, HIGHLIGHT_FADE))
    }

    fn animating(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.opened_at) < APPEAR
            || (self.hovered.is_some()
                && now.saturating_duration_since(self.glide_since) < HIGHLIGHT_GLIDE)
    }
}

/// What the caller must do after feeding the gesture an event.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum PressEffect {
    /// Nothing to do, but the event belonged to the gesture.
    None,
    /// Start the hold timer for this press.
    Arm(u64),
    /// A tap: start the default Agent, as ⌘T always has.
    Tap,
    /// Start this item and close the ring.
    Launch(RingItem),
    /// The ring closed without starting anything.
    Closed,
    /// The ring changed and must be drawn again.
    Redraw,
}

#[derive(Clone, Debug, Default)]
enum Phase {
    #[default]
    Idle,
    /// The shortcut is down and the hold timer is running.
    Held { generation: u64 },
    Open(Ring),
    /// The gesture ended while the shortcut is still down: its repeats and
    /// its release are swallowed, so they start nothing.
    Spent,
}

/// A ring on its way out, drawn until [`LEAVE`] has passed.
#[derive(Clone, Debug)]
pub(crate) struct Leaving {
    pub ring: Ring,
    pub chosen: Option<usize>,
    pub since: Instant,
}

/// The ⌘T press, from key down to whatever it turns into.
#[derive(Debug, Default)]
pub(crate) struct RadialPress {
    phase: Phase,
    generation: u64,
    leaving: Option<Leaving>,
}

impl RadialPress {
    /// The shortcut went down. Repeats of a held key land here too and are
    /// swallowed; a press while the ring waits for a click closes it.
    pub(crate) fn shortcut_down(&mut self, now: Instant) -> PressEffect {
        match &self.phase {
            Phase::Idle => {
                self.generation += 1;
                self.phase = Phase::Held {
                    generation: self.generation,
                };
                PressEffect::Arm(self.generation)
            }
            Phase::Held { .. } | Phase::Spent => PressEffect::None,
            Phase::Open(ring) if ring.sticky => self.close(None, now),
            Phase::Open(_) => PressEffect::None,
        }
    }

    /// The shortcut came back up (its key, or ⌘).
    pub(crate) fn released(&mut self, now: Instant) -> PressEffect {
        match &mut self.phase {
            Phase::Idle => PressEffect::None,
            Phase::Spent => {
                self.phase = Phase::Idle;
                PressEffect::None
            }
            Phase::Held { .. } => {
                self.phase = Phase::Idle;
                PressEffect::Tap
            }
            Phase::Open(ring) if ring.sticky => PressEffect::None,
            Phase::Open(ring) => match ring.hovered {
                Some(index) => {
                    ring.sticky = true;
                    self.close(Some(index), now)
                }
                None => {
                    ring.sticky = true;
                    PressEffect::Redraw
                }
            },
        }
    }

    /// The hold timer fired. True when this press is still held and the
    /// caller should [`open`](Self::open) the ring.
    pub(crate) fn hold_elapsed(&self, generation: u64) -> bool {
        matches!(self.phase, Phase::Held { generation: held } if held == generation)
    }

    pub(crate) fn open(&mut self, mut ring: Ring, now: Instant) {
        if let Some(pointer) = ring.pointer {
            ring.track(pointer, now);
            ring.glide_from = None;
        }
        self.leaving = None;
        self.phase = Phase::Open(ring);
    }

    pub(crate) fn pointer_moved(&mut self, position: Point<Pixels>, now: Instant) -> bool {
        match &mut self.phase {
            Phase::Open(ring) => ring.track(position, now),
            _ => false,
        }
    }

    /// A click while the ring is open: the slice under it, or away.
    pub(crate) fn click(&mut self, position: Point<Pixels>, now: Instant) -> PressEffect {
        let Phase::Open(ring) = &mut self.phase else {
            return PressEffect::None;
        };
        ring.sticky = true;
        ring.track(position, now);
        let chosen = ring.hovered;
        self.close(chosen, now)
    }

    /// Escape, a lost window, anything that ends the gesture without a
    /// choice. A held press is forgotten without its tap.
    pub(crate) fn cancel(&mut self, now: Instant) -> PressEffect {
        match self.phase {
            Phase::Idle | Phase::Spent => PressEffect::None,
            Phase::Held { .. } => {
                self.phase = Phase::Spent;
                PressEffect::Closed
            }
            Phase::Open(_) => self.close(None, now),
        }
    }

    /// The window lost key: whatever the keys do next goes to another app,
    /// so nothing is left waiting for a release.
    pub(crate) fn reset(&mut self, now: Instant) -> PressEffect {
        let effect = self.cancel(now);
        self.phase = Phase::Idle;
        effect
    }

    fn close(&mut self, chosen: Option<usize>, now: Instant) -> PressEffect {
        let Phase::Open(ring) = std::mem::take(&mut self.phase) else {
            return PressEffect::None;
        };
        if !ring.sticky {
            self.phase = Phase::Spent;
        }
        let item = chosen.and_then(|index| ring.items.get(index).cloned());
        self.leaving = Some(Leaving {
            ring,
            chosen,
            since: now,
        });
        match item {
            Some(item) => PressEffect::Launch(item),
            None => PressEffect::Closed,
        }
    }

    /// The shortcut is down or the ring is up: its key repeats and releases
    /// belong to this gesture.
    pub(crate) fn active(&self) -> bool {
        !matches!(self.phase, Phase::Idle)
    }

    #[cfg(test)]
    pub(crate) fn pressing(&self) -> bool {
        match &self.phase {
            Phase::Held { .. } | Phase::Spent => true,
            Phase::Open(ring) => !ring.sticky,
            Phase::Idle => false,
        }
    }

    pub(crate) fn ring(&self) -> Option<&Ring> {
        match &self.phase {
            Phase::Open(ring) => Some(ring),
            _ => None,
        }
    }

    /// The ring fading out, until its fade is over.
    pub(crate) fn leaving(&mut self, now: Instant) -> Option<&Leaving> {
        if self
            .leaving
            .as_ref()
            .is_some_and(|leaving| now.saturating_duration_since(leaving.since) >= LEAVE)
        {
            self.leaving = None;
        }
        self.leaving.as_ref()
    }

    #[cfg(test)]
    pub(crate) fn hovered_title(&self) -> Option<&str> {
        let ring = self.ring()?;
        ring.items.get(ring.hovered?).map(|item| item.title.as_str())
    }
}

fn progress(since: Instant, now: Instant, over: Duration) -> f32 {
    (now.saturating_duration_since(since).as_secs_f32() / over.as_secs_f32()).clamp(0.0, 1.0)
}

fn ease_out(t: f32) -> f32 {
    1.0 - (1.0 - t).powi(3)
}

/// A point `turn` of a clockwise lap from twelve o'clock.
fn on_circle(center: Point<Pixels>, radius: f32, turn: f32) -> Point<Pixels> {
    let angle = turn * TAU;
    point(
        center.x + px(radius * angle.sin()),
        center.y - px(radius * angle.cos()),
    )
}

fn paint_dot(window: &mut Window, center: Point<Pixels>, radius: f32, color: Rgba) {
    let mut path = PathBuilder::fill();
    path.move_to(on_circle(center, radius, 0.0));
    for quarter in 1..=4 {
        path.arc_to(
            point(px(radius), px(radius)),
            px(0.0),
            false,
            true,
            on_circle(center, radius, quarter as f32 / 4.0),
        );
    }
    path.close();
    if let Ok(path) = path.build() {
        window.paint_path(path, color);
    }
}

/// The disk the ring is drawn on. Opaque enough that terminal text under it
/// never reads through, since an in-window surface gets no backdrop blur.
fn disk_fill(colors: SemanticColors) -> Rgba {
    let base = Glass::panel_fill(colors);
    let dark = colors.appearance == diri_ui::Appearance::Dark;
    let lift = Rgba {
        r: 1.0,
        g: 1.0,
        b: 1.0,
        a: if dark { 0.05 } else { 0.0 },
    };
    let solid = composite(base, colors.background.alpha(1.0));
    let lifted = composite(lift, solid);
    lifted.alpha(0.97)
}

fn composite(over: Rgba, under: Rgba) -> Rgba {
    let a = over.a + under.a * (1.0 - over.a);
    if a <= 0.0 {
        return Rgba {
            r: 0.0,
            g: 0.0,
            b: 0.0,
            a: 0.0,
        };
    }
    let mix = |o: f32, u: f32| (o * over.a + u * under.a * (1.0 - over.a)) / a;
    Rgba {
        r: mix(over.r, under.r),
        g: mix(over.g, under.g),
        b: mix(over.b, under.b),
        a,
    }
}

/// Everything inside the ring at `now`, positioned in window coordinates.
/// `fade` is the opacity of a ring on its way in or out, `scale` its size.
pub(crate) fn render_ring(
    ring: &Ring,
    chosen: Option<usize>,
    leaving_since: Option<Instant>,
    now: Instant,
    colors: SemanticColors,
    window: &mut Window,
) -> AnyElement {
    let appear = ease_out(progress(ring.opened_at, now, APPEAR));
    let (opacity, scale) = match leaving_since {
        Some(since) => {
            let out = progress(since, now, LEAVE);
            let out = out * out;
            let grow = if chosen.is_some() { 0.04 } else { -0.06 };
            (appear * (1.0 - out), 1.0 + grow * out)
        }
        None => (appear, 0.88 + 0.12 * appear),
    };
    if opacity < 1.0 || ring.animating(now) {
        window.request_animation_frame();
    }
    let outer = OUTER_RADIUS * scale;
    let hub = HUB_RADIUS * scale;
    let mark_radius = MARK_RADIUS * scale;
    let diameter = outer * 2.0;
    let origin = point(ring.center.x - px(outer), ring.center.y - px(outer));
    let count = ring.items.len().max(1);
    let lit = chosen.or(ring.hovered);
    let highlight = if leaving_since.is_some() {
        chosen.map(|index| (index as f32 / count as f32, 1.0))
    } else {
        ring.highlight_turn(now)
            .map(|turn| (turn, ring.highlight_alpha(now)))
    };
    let pointer_turn = ring
        .pointer
        .map(|pointer| ring.offset(pointer))
        .filter(|offset| offset.x.hypot(offset.y) >= 4.0)
        .map(turn_of);
    let band_color = colors.primary.alpha(0.11);
    let hub_fill = colors.primary.alpha(0.045);
    let tick_color = colors.secondary.alpha(0.9);
    let paint_center_local = point(px(outer), px(outer));

    let mut root = div()
        .absolute()
        .left(origin.x)
        .top(origin.y)
        .size(px(diameter))
        .opacity(opacity)
        // The disk itself: a lifted, nearly opaque circle with the same
        // hairline and light catch as every floating menu.
        .child(
            div()
                .absolute()
                .inset_0()
                .rounded_full()
                .bg(disk_fill(colors))
                .border_1()
                .border_color(colors.floating_stroke())
                .shadow(vec![
                    BoxShadow {
                        color: Hsla::from(Rgba {
                            r: 0.0,
                            g: 0.0,
                            b: 0.0,
                            a: if colors.appearance == diri_ui::Appearance::Dark {
                                0.42
                            } else {
                                0.16
                            },
                        }),
                        offset: point(px(0.0), px(14.0)),
                        blur_radius: px(36.0),
                        spread_radius: px(0.0),
                        inset: false,
                    },
                    BoxShadow {
                        color: Glass::rim(colors).into(),
                        offset: point(px(0.0), px(1.0)),
                        blur_radius: px(0.0),
                        spread_radius: px(0.0),
                        inset: true,
                    },
                ]),
        )
        .child(
            canvas(
                |_, _, _| {},
                move |bounds: Bounds<Pixels>, _, window, _| {
                    let center = bounds.origin + paint_center_local;
                    // The hub: a quieter well that holds the label.
                    paint_dot(window, center, hub, hub_fill);
                    if let Some((turn, alpha)) = highlight {
                        // A round well under the chosen mark. It travels
                        // along the ring between slots, so moving the hand
                        // reads as one thing sliding, not cells blinking.
                        paint_dot(
                            window,
                            on_circle(center, mark_radius, turn),
                            HIGHLIGHT_RADIUS * scale,
                            band_color.alpha(band_color.a * alpha),
                        );
                    }
                    // The pointer's own direction, on the hub's rim: the
                    // ring answers the hand before a slice is reached.
                    if let Some(turn) = pointer_turn {
                        paint_dot(
                            window,
                            on_circle(center, hub - 7.0 * scale, turn),
                            2.5 * scale,
                            tick_color,
                        );
                    }
                },
            )
            .absolute()
            .inset_0(),
        );

    for (index, item) in ring.items.iter().enumerate() {
        let turn = index as f32 / count as f32;
        let at = on_circle(paint_center_local, mark_radius, turn);
        let on = lit == Some(index);
        let size = MARK_SIZE * scale * if on { 1.12 } else { 1.0 };
        root = root.child(
            div()
                .absolute()
                .left(at.x - px(size / 2.0))
                .top(at.y - px(size / 2.0))
                .size(px(size))
                .flex()
                .items_center()
                .justify_center()
                .opacity(if on || lit.is_none() { 1.0 } else { 0.72 })
                .child(
                    AgentLogo::new(
                        crate::session_presentation::ui_agent_kind(&item.kind),
                        size,
                        colors,
                    )
                    .badged(false)
                    .inset(0.1)
                    // Brand colour only on the slice the hand is on.
                    .monochrome(!on),
                ),
        );
    }

    let (title, detail) = match lit.and_then(|index| ring.items.get(index)) {
        Some(item) => (
            item.title.clone(),
            item.is_default
                .then(|| crate::i18n::t("sidebar.new_agent.radial_default").to_owned()),
        ),
        None => (ring.place.clone(), Some(crate::i18n::t("sidebar.new_agent.radial_new_in").to_owned())),
    };
    let label_width = (hub * 2.0 - 10.0).max(0.0);
    root = root.child(
        div()
            .absolute()
            .left(px(outer - label_width / 2.0))
            .top(px(outer - 22.0))
            .w(px(label_width))
            .h(px(44.0))
            .gap(px(1.0))
            .line_height(px(16.0))
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .text_center()
            .when_some(detail.clone().filter(|_| lit.is_none()), |label, detail| {
                label.child(
                    div()
                        .text_size(px(Typo::META.size))
                        .text_color(colors.tertiary)
                        .child(detail),
                )
            })
            .child(
                div()
                    .max_w_full()
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .text_ellipsis()
                    .text_size(px(Typo::ROW.size))
                    .font_weight(Typo::ROW_EMPHASIZED.weight)
                    .text_color(if lit.is_some() {
                        colors.primary
                    } else {
                        colors.secondary
                    })
                    .child(title),
            )
            .when_some(detail.filter(|_| lit.is_some()), |label, detail| {
                label.child(
                    div()
                        .text_size(px(Typo::META.size))
                        .text_color(colors.tertiary)
                        .child(detail),
                )
            }),
    );
    root.into_any_element()
}

use gpui::prelude::FluentBuilder as _;

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str) -> RingItem {
        RingItem {
            kind: ProtoAgentKind::new(id),
            title: id.to_owned(),
            is_default: false,
        }
    }

    fn titles(ring: &[RingItem]) -> Vec<&str> {
        ring.iter().map(|item| item.title.as_str()).collect()
    }

    #[test]
    fn default_leads_at_twelve_and_terminal_note_sit_at_six() {
        let agents = ["claude", "codex", "agy", "cursor", "gemini", "opencode"]
            .map(item)
            .to_vec();
        let ring = arrange(agents, vec![item("shell"), item("note")]);
        // 8 slots: slot 4 is six o'clock.
        assert_eq!(
            titles(&ring),
            ["claude", "codex", "agy", "cursor", "shell", "note", "gemini", "opencode"]
        );
        let ring = arrange(
            ["claude", "codex", "agy"].map(item).to_vec(),
            vec![item("shell"), item("note")],
        );
        // 5 slots: 2 and 3 straddle six o'clock.
        assert_eq!(titles(&ring), ["claude", "codex", "shell", "note", "agy"]);
    }

    #[test]
    fn no_agents_leaves_terminal_and_note() {
        let ring = arrange(Vec::new(), vec![item("shell"), item("note")]);
        assert_eq!(titles(&ring), ["shell", "note"]);
    }

    #[test]
    fn slices_reach_outward_and_ignore_the_dead_zone() {
        let up = point(0.0, -60.0);
        assert_eq!(slot_at(up, 8, None), Some(0));
        assert_eq!(slot_at(point(60.0, 0.0), 8, None), Some(2));
        assert_eq!(slot_at(point(0.0, 60.0), 8, None), Some(4));
        assert_eq!(slot_at(point(-60.0, 0.0), 8, None), Some(6));
        // Just left of twelve still rounds to the top slice.
        assert_eq!(slot_at(point(-10.0, -60.0), 8, None), Some(0));
        // Far beyond the ring a flick still counts.
        assert_eq!(slot_at(point(0.0, 900.0), 8, None), Some(4));
        assert_eq!(slot_at(point(0.0, 900.0), 8, Some(150.0)), None);
        assert_eq!(slot_at(point(5.0, 5.0), 8, None), None);
    }

    #[test]
    fn place_keeps_the_ring_inside_the_window() {
        let window = gpui::size(px(1200.0), px(800.0));
        let center = place(Some(point(px(4.0), px(790.0))), window);
        assert_eq!(f32::from(center.x), OUTER_RADIUS + EDGE_MARGIN);
        assert_eq!(f32::from(center.y), 800.0 - OUTER_RADIUS - EDGE_MARGIN);
        let center = place(None, window);
        assert_eq!(center, point(px(600.0), px(400.0)));
    }

    fn ring(now: Instant) -> Ring {
        Ring::new(
            point(px(300.0), px(300.0)),
            ["claude", "codex", "shell", "note"].map(item).to_vec(),
            "dirijor".into(),
            now,
        )
    }

    #[test]
    fn a_tap_starts_the_default_and_repeats_are_swallowed() {
        let now = Instant::now();
        let mut press = RadialPress::default();
        let PressEffect::Arm(generation) = press.shortcut_down(now) else {
            panic!("first press arms the hold timer");
        };
        // Key repeat while held: no second spawn, no second timer.
        assert_eq!(press.shortcut_down(now), PressEffect::None);
        assert_eq!(press.shortcut_down(now), PressEffect::None);
        assert_eq!(press.released(now), PressEffect::Tap);
        assert!(!press.hold_elapsed(generation), "a stale timer opens nothing");
        assert!(!press.active());
    }

    #[test]
    fn hold_then_flick_then_release_launches_the_slice() {
        let now = Instant::now();
        let mut press = RadialPress::default();
        let PressEffect::Arm(generation) = press.shortcut_down(now) else {
            panic!();
        };
        assert!(press.hold_elapsed(generation));
        press.open(ring(now), now);
        assert!(press.pressing());
        // Straight down from the centre: slot 2 of 4 (six o'clock).
        assert!(press.pointer_moved(point(px(300.0), px(380.0)), now));
        assert_eq!(press.hovered_title(), Some("shell"));
        // Repeats while the ring is up do nothing.
        assert_eq!(press.shortcut_down(now), PressEffect::None);
        assert_eq!(
            press.released(now),
            PressEffect::Launch(item("shell"))
        );
        assert!(!press.active());
        assert!(press.leaving(now).is_some());
        assert!(press.leaving(now + LEAVE).is_none());
    }

    #[test]
    fn release_without_a_choice_keeps_the_ring_for_a_click() {
        let now = Instant::now();
        let mut press = RadialPress::default();
        press.shortcut_down(now);
        press.open(ring(now), now);
        assert_eq!(press.released(now), PressEffect::Redraw);
        assert!(press.active() && !press.pressing());
        // A click far outside the ring is a click away.
        assert_eq!(
            press.click(point(px(900.0), px(300.0)), now),
            PressEffect::Closed
        );
        press.shortcut_down(now);
        press.open(ring(now), now);
        press.released(now);
        // To the right: codex (slot 1 of 4).
        assert_eq!(
            press.click(point(px(380.0), px(300.0)), now),
            PressEffect::Launch(item("codex"))
        );
    }

    #[test]
    fn hub_click_and_escape_close_without_launching() {
        let now = Instant::now();
        let mut press = RadialPress::default();
        press.shortcut_down(now);
        press.open(ring(now), now);
        press.released(now);
        assert_eq!(press.click(point(px(302.0), px(300.0)), now), PressEffect::Closed);
        press.shortcut_down(now);
        assert_eq!(press.cancel(now), PressEffect::Closed);
        // Still held: its repeats and its release start nothing.
        assert_eq!(press.shortcut_down(now), PressEffect::None);
        assert_eq!(press.released(now), PressEffect::None, "a cancelled hold never taps");
        assert!(!press.active());
        assert!(matches!(press.shortcut_down(now), PressEffect::Arm(_)));
    }

    #[test]
    fn pressing_again_while_waiting_for_a_click_closes() {
        let now = Instant::now();
        let mut press = RadialPress::default();
        press.shortcut_down(now);
        press.open(ring(now), now);
        press.released(now);
        assert_eq!(press.shortcut_down(now), PressEffect::Closed);
        assert!(!press.active());
    }

    #[test]
    fn the_highlight_glides_the_short_way_round() {
        let now = Instant::now();
        let mut open = ring(now);
        open.track(point(px(300.0), px(200.0)), now); // slot 0
        let later = now + Duration::from_millis(500);
        open.track(point(px(200.0), px(300.0)), later); // slot 3 (nine o'clock)
        let halfway = open
            .highlight_turn(later + HIGHLIGHT_GLIDE / 2)
            .expect("highlighted");
        // From 0.0 to 0.75 the short way is backwards through 0.875.
        assert!(halfway > 0.75 && halfway < 1.0, "{halfway}");
        assert_eq!(open.highlight_turn(later + HIGHLIGHT_GLIDE), Some(0.75));
    }
}
