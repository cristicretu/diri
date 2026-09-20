//! Session titles that settle. A title an agent or Diri changed crossfades
//! from the text on screen to the new text; one the user typed, a row's first
//! appearance and anything under Reduce Motion are immediate.
//!
//! The model is pure: titles and a clock go in, layers and opacities come
//! out. It schedules nothing. A surface asks [`TitleSettles::is_settling`]
//! whether another frame is owed and stops asking the moment that is false.
use std::collections::HashMap;
use std::time::{Duration, Instant};

use diri_proto::SessionId;
use diri_ui::TypeStyle;
use gpui::{
    App, Div, HighlightStyle, IntoElement, ParentElement, RenderOnce, SharedString, Styled,
    StyledText, TextRun, Window, div, px,
};

pub(super) const SETTLE: Duration = Duration::from_millis(180);
/// Titles on their way out at once. A third rename inside one fade drops the
/// faintest layer instead of stacking another.
const MAX_LEAVING: usize = 2;
/// A layer this faint is dropped on retarget; it is not worth a text element.
const FAINT: f32 = 0.02;
/// Width kept clear for the ellipsis when deciding whether a shared prefix
/// sits wholly inside the truncated box.
const ELLIPSIS_ROOM: f32 = 14.0;

#[derive(Clone, Debug, PartialEq)]
pub(super) struct Layer {
    pub text: SharedString,
    pub opacity: f32,
}

/// What one title paints at one instant.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Frame {
    pub leaving: Vec<Layer>,
    pub arriving: Layer,
}

struct Fade {
    /// Layers and their opacities at the moment this fade started.
    leaving: Vec<Layer>,
    arriving_from: f32,
    started: Instant,
}

struct Entry {
    title: SharedString,
    fade: Option<Fade>,
}

impl Entry {
    fn settling(&self, now: Instant) -> bool {
        self.fade
            .as_ref()
            .is_some_and(|fade| now.saturating_duration_since(fade.started) < SETTLE)
    }

    fn sample(&self, now: Instant) -> Option<Frame> {
        let fade = self.fade.as_ref()?;
        let elapsed = now.saturating_duration_since(fade.started);
        if elapsed >= SETTLE {
            return None;
        }
        let progress = ease_out(elapsed.as_secs_f32() / SETTLE.as_secs_f32());
        Some(Frame {
            leaving: fade
                .leaving
                .iter()
                .map(|layer| Layer {
                    text: layer.text.clone(),
                    opacity: layer.opacity * leaving_opacity(progress),
                })
                .collect(),
            arriving: Layer {
                text: self.title.clone(),
                opacity: fade.arriving_from + (1.0 - fade.arriving_from) * progress,
            },
        })
    }
}

/// Decelerating quadratic. The cubic the theme fade uses spends two thirds of
/// so short a fade above 90%, where opacity no longer reads as motion, and
/// leaves three frames of visible change: a snap with a tail.
fn ease_out(progress: f32) -> f32 {
    let remaining = 1.0 - progress.clamp(0.0, 1.0);
    1.0 - remaining * remaining
}

/// The old text clears out ahead of the new one arriving. A symmetric
/// crossfade holds two different strings at half strength in one box, which
/// reads as a smudge rather than as a change.
fn leaving_opacity(progress: f32) -> f32 {
    let remaining = 1.0 - progress;
    remaining * remaining
}

#[derive(Default)]
pub(super) struct TitleSettles {
    entries: HashMap<SessionId, Entry>,
}

impl TitleSettles {
    /// Records the title `id` has right now. `animate` is false for a change
    /// that must land immediately: the user's own rename, Reduce Motion, or
    /// no surface on screen to show it.
    pub fn observe(&mut self, id: &SessionId, title: &str, animate: bool, now: Instant) {
        let Some(entry) = self.entries.get_mut(id) else {
            // A first appearance has nothing to fade from.
            self.entries.insert(
                id.clone(),
                Entry {
                    title: SharedString::from(title.to_owned()),
                    fade: None,
                },
            );
            return;
        };
        if entry.title.as_ref() == title {
            if !animate || !entry.settling(now) {
                entry.fade = None;
            }
            return;
        }
        let title = SharedString::from(title.to_owned());
        if !animate {
            entry.title = title;
            entry.fade = None;
            return;
        }
        // Head for the new title from what is on screen. A fade in flight is
        // abandoned where it stands, never queued behind.
        let mut leaving = match entry.sample(now) {
            Some(mut frame) => {
                frame.leaving.push(frame.arriving);
                frame.leaving
            }
            None => vec![Layer {
                text: entry.title.clone(),
                opacity: 1.0,
            }],
        };
        // Renamed back to a title still fading out: it turns around from its
        // current opacity instead of being painted twice.
        let arriving_from = leaving
            .iter()
            .position(|layer| layer.text == title)
            .map_or(0.0, |index| leaving.remove(index).opacity);
        leaving.retain(|layer| layer.opacity > FAINT);
        leaving.sort_by(|a, b| b.opacity.total_cmp(&a.opacity));
        leaving.truncate(MAX_LEAVING);
        entry.title = title;
        entry.fade = Some(Fade {
            leaving,
            arriving_from,
            started: now,
        });
    }

    /// Forgets sessions that are gone, so the state is bounded by the store.
    pub fn retain(&mut self, mut live: impl FnMut(&SessionId) -> bool) {
        self.entries.retain(|id, _| live(id));
    }

    /// The layers to paint for `id`, or `None` when its title is at rest and
    /// the ordinary label is exactly right.
    pub fn frame(&self, id: &SessionId, now: Instant) -> Option<Frame> {
        self.entries.get(id)?.sample(now)
    }

    /// Whether any title still owes a frame at `now`.
    pub fn is_settling(&self, now: Instant) -> bool {
        self.entries.values().any(|entry| entry.settling(now))
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

/// Byte length of the prefix `from` and `to` share, cut back to a word
/// boundary so a word never changes halfway through. Zero when they share
/// no whole word.
pub(super) fn shared_prefix(from: &str, to: &str) -> usize {
    if from == to {
        return 0;
    }
    let mut end = from
        .char_indices()
        .zip(to.chars())
        .find(|((_, a), b)| a != b)
        .map_or(from.len().min(to.len()), |((index, _), _)| index);
    let splits_word = |text: &str, at: usize| {
        let before = text[..at].chars().next_back();
        let after = text[at..].chars().next();
        matches!((before, after), (Some(a), Some(b)) if !a.is_whitespace() && !b.is_whitespace())
    };
    while end > 0 && (splits_word(from, end) || splits_word(to, end)) {
        end = from[..end]
            .char_indices()
            .next_back()
            .map_or(0, |(index, _)| index);
    }
    end
}

/// The label for a title that is settling. Every layer is laid out in the
/// same box with the same truncation, so nothing shifts; only opacity moves.
/// A rise of a pixel or two on the arriving text was tried and dropped: two
/// strings on different baselines read as one blurred string, not as motion.
///
/// The caller's container supplies font, size and clipping exactly as it does
/// for the label at rest. `available_width` is that container's width.
#[derive(IntoElement)]
pub(super) struct SettlingLabel {
    pub frame: Frame,
    pub font: TypeStyle,
    pub available_width: f32,
}

impl RenderOnce for SettlingLabel {
    fn render(self, window: &mut Window, _: &mut App) -> impl IntoElement {
        let Self {
            frame,
            font,
            available_width,
        } = self;
        settling_label(&frame, font, available_width, window)
    }
}

fn settling_label(
    frame: &Frame,
    font: TypeStyle,
    available_width: f32,
    window: &mut Window,
) -> Div {
    let layer = || {
        div()
            .w_full()
            .overflow_hidden()
            .text_ellipsis()
            .whitespace_nowrap()
    };
    let root = div().relative().w_full();
    if let [leaving] = frame.leaving.as_slice()
        && let Some(split) = stable_prefix(
            &leaving.text,
            &frame.arriving.text,
            font,
            available_width,
            window,
        )
    {
        // A refinement ("Fix login" to "Fix login redirect loop") keeps the
        // words it shares painted once at full strength. Two translucent
        // copies of the same glyphs composite to less than one opaque copy,
        // so crossfading them dims words that never changed: a blink.
        let Some(split) = split else {
            // The whole visible text is shared: nothing on screen changes.
            return root.child(layer().child(frame.arriving.text.clone()));
        };
        let tail = |text: &SharedString, head_alpha: f32, tail_alpha: f32| {
            // A highlight color is blended over the inherited one, so a
            // translucent color would leave the text opaque. Fading scales
            // the inherited color's own alpha.
            let shade = |alpha: f32| HighlightStyle {
                fade_out: Some(1.0 - alpha),
                ..Default::default()
            };
            StyledText::new(text.clone()).with_highlights(
                [
                    (0..split, shade(head_alpha)),
                    (split..text.len(), shade(tail_alpha)),
                ]
                .into_iter()
                .filter(|(range, _)| !range.is_empty()),
            )
        };
        return root
            .child(layer().child(tail(&frame.arriving.text, 1.0, frame.arriving.opacity)))
            .child(
                layer()
                    .absolute()
                    .inset_0()
                    .child(tail(&leaving.text, 0.0, leaving.opacity)),
            );
    }
    root.child(
        layer()
            .opacity(frame.arriving.opacity)
            .child(frame.arriving.text.clone()),
    )
    .children(frame.leaving.iter().map(|leaving| {
        layer()
            .absolute()
            .inset_0()
            .opacity(leaving.opacity)
            .child(leaving.text.clone())
    }))
}

/// Where the shared prefix of two titles ends, if both lay it out
/// identically. `Some(None)` means the prefix fills the visible box, so the
/// two titles look the same; `None` means crossfade the whole string.
fn stable_prefix(
    from: &SharedString,
    to: &SharedString,
    font: TypeStyle,
    available_width: f32,
    window: &mut Window,
) -> Option<Option<usize>> {
    let split = shared_prefix(from, to);
    if split == 0 {
        return None;
    }
    let mut face = gpui::font(crate::fonts::ui_family());
    face.weight = font.weight;
    let edge = |text: &SharedString| {
        let run = TextRun {
            len: text.len(),
            font: face.clone(),
            ..TextRun::default()
        };
        window
            .text_system()
            .shape_line(text.clone(), px(font.size), &[run], None)
            .x_for_index(split)
    };
    let (from_edge, to_edge) = (edge(from), edge(to));
    if (f32::from(from_edge) - f32::from(to_edge)).abs() > 0.01 {
        return None;
    }
    Some((f32::from(to_edge) + ELLIPSIS_ROOM <= available_width).then_some(split))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(name: &str) -> SessionId {
        SessionId::new(name)
    }

    fn at(start: Instant, millis: u64) -> Instant {
        start + Duration::from_millis(millis)
    }

    #[test]
    fn a_first_appearance_and_an_unchanged_title_are_at_rest() {
        let now = Instant::now();
        let mut titles = TitleSettles::default();
        titles.observe(&id("a"), "Untitled", true, now);
        titles.observe(&id("a"), "Untitled", true, at(now, 16));
        assert_eq!(titles.frame(&id("a"), at(now, 16)), None);
        assert!(!titles.is_settling(at(now, 16)));
    }

    #[test]
    fn a_change_starts_from_the_old_text_and_lands_exactly_on_the_new() {
        let now = Instant::now();
        let mut titles = TitleSettles::default();
        titles.observe(&id("a"), "Untitled", true, now);
        titles.observe(&id("a"), "Fix login", true, now);
        let first = titles.frame(&id("a"), now).unwrap();
        assert_eq!(first.leaving[0].text.as_ref(), "Untitled");
        assert_eq!(first.leaving[0].opacity, 1.0);
        assert_eq!(first.arriving.text.as_ref(), "Fix login");
        assert_eq!(first.arriving.opacity, 0.0);

        let mut last = first;
        for millis in (16..180).step_by(16) {
            let frame = titles.frame(&id("a"), at(now, millis)).unwrap();
            assert!(frame.arriving.opacity > last.arriving.opacity);
            assert!(frame.leaving[0].opacity < last.leaving[0].opacity);
            last = frame;
        }
        // Ease-out: most of the way there by the halfway mark, and the old
        // text is nearly gone by then.
        let halfway = titles.frame(&id("a"), now + SETTLE / 2).unwrap();
        assert!(halfway.arriving.opacity > 0.7);
        assert!(halfway.leaving[0].opacity < 0.1);

        assert!(titles.is_settling(at(now, 179)));
        assert_eq!(titles.frame(&id("a"), now + SETTLE), None);
        assert!(!titles.is_settling(now + SETTLE));
        // Landing clears the fade; a later pass cannot replay it.
        titles.observe(&id("a"), "Fix login", true, at(now, 200));
        assert_eq!(titles.frame(&id("a"), now), None);
    }

    #[test]
    fn a_rename_mid_fade_retargets_from_what_is_on_screen() {
        let now = Instant::now();
        let mut titles = TitleSettles::default();
        titles.observe(&id("a"), "Untitled", true, now);
        titles.observe(&id("a"), "Fix login", true, now);
        let midway = at(now, 48);
        let before = titles.frame(&id("a"), midway).unwrap();
        titles.observe(&id("a"), "Fix login redirect loop", true, midway);
        let after = titles.frame(&id("a"), midway).unwrap();

        // Both strings that were on screen still are, at the same strength.
        assert_eq!(after.arriving.text.as_ref(), "Fix login redirect loop");
        assert_eq!(after.arriving.opacity, 0.0);
        let mut on_screen = before.leaving.clone();
        on_screen.push(before.arriving.clone());
        on_screen.sort_by(|a, b| b.opacity.total_cmp(&a.opacity));
        assert_eq!(after.leaving, on_screen);

        // One fade to finish, timed from the retarget.
        assert!(titles.is_settling(at(midway, 179)));
        assert_eq!(titles.frame(&id("a"), midway + SETTLE), None);
    }

    #[test]
    fn a_burst_of_renames_never_piles_up_layers() {
        let now = Instant::now();
        let mut titles = TitleSettles::default();
        titles.observe(&id("a"), "0", true, now);
        for step in 1..40_u64 {
            let when = at(now, step * 8);
            titles.observe(&id("a"), &step.to_string(), true, when);
            let frame = titles.frame(&id("a"), when).unwrap();
            assert!(frame.leaving.len() <= MAX_LEAVING);
        }
        assert_eq!(titles.frame(&id("a"), at(now, 39 * 8) + SETTLE), None);
    }

    #[test]
    fn renaming_back_turns_the_fading_title_around() {
        let now = Instant::now();
        let mut titles = TitleSettles::default();
        titles.observe(&id("a"), "Fix login", true, now);
        titles.observe(&id("a"), "Renaming", true, now);
        let midway = at(now, 32);
        let before = titles.frame(&id("a"), midway).unwrap();
        titles.observe(&id("a"), "Fix login", true, midway);
        let after = titles.frame(&id("a"), midway).unwrap();
        assert_eq!(after.arriving.text.as_ref(), "Fix login");
        assert_eq!(after.arriving.opacity, before.leaving[0].opacity);
        assert_eq!(after.leaving, vec![before.arriving]);
    }

    #[test]
    fn a_user_rename_and_reduce_motion_commit_instantly() {
        let now = Instant::now();
        let mut titles = TitleSettles::default();
        titles.observe(&id("a"), "Untitled", true, now);
        titles.observe(&id("a"), "My name for it", false, now);
        assert_eq!(titles.frame(&id("a"), now), None);
        assert!(!titles.is_settling(now));

        // Also when it interrupts a fade already running.
        titles.observe(&id("a"), "Agent title", true, now);
        assert!(titles.is_settling(at(now, 32)));
        titles.observe(&id("a"), "Typed over it", false, at(now, 32));
        assert_eq!(titles.frame(&id("a"), at(now, 32)), None);

        // Reduce Motion switched on mid-fade stops it on the same title.
        titles.observe(&id("a"), "Agent again", true, at(now, 300));
        titles.observe(&id("a"), "Agent again", false, at(now, 316));
        assert!(!titles.is_settling(at(now, 316)));
    }

    #[test]
    fn state_is_bounded_by_the_live_sessions() {
        let now = Instant::now();
        let mut titles = TitleSettles::default();
        for index in 0..100 {
            titles.observe(&id(&format!("s{index}")), "Untitled", true, now);
        }
        assert_eq!(titles.len(), 100);
        titles.retain(|session| session.0 == "s7");
        assert_eq!(titles.len(), 1);
        // A session that returns is a first appearance again.
        titles.observe(&id("s8"), "Came back renamed", true, now);
        assert_eq!(titles.frame(&id("s8"), now), None);
    }

    #[test]
    fn shared_prefixes_end_on_a_word_boundary() {
        assert_eq!(shared_prefix("Fix login", "Fix login redirect loop"), 9);
        assert_eq!(shared_prefix("Fix login redirect loop", "Fix login"), 9);
        // "login" to "logins" changes a word, so the word crossfades whole.
        assert_eq!(shared_prefix("Fix login", "Fix logins"), 4);
        assert_eq!(shared_prefix("Fix login", "Fix logout flow"), 4);
        assert_eq!(shared_prefix("Untitled", "Fix login"), 0);
        assert_eq!(shared_prefix("Fixture", "Fix login"), 0);
        assert_eq!(shared_prefix("Same", "Same"), 0);
        assert_eq!(
            shared_prefix("Résumé parser", "Résumé upload"),
            "Résumé ".len()
        );
    }
}
