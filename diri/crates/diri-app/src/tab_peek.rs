//! Presentation-only tab peek state. It never owns a terminal or issues effects.
use crate::peek_settle::Settle;
use diri_proto::SessionId;
use std::time::Instant;

pub(crate) const PEEK_DISTANCE: f32 = 140.0;
const OVERVIEW_DISTANCE: f32 = 380.0;
const PEEK_CONTENT_OFFSET: f32 = 176.0;
// A normal stroke should comfortably land in the strip. Keep gain independent
// of event timing so a quick pinch is no stronger than the same slow stroke.
const PINCH_GAIN: f32 = 500.0;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) enum GestureFrame {
    #[default]
    Cancelled,
    #[cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))]
    Tracking(f32),
    #[cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))]
    Released(f32),
}

/// Direct manipulation across all three poses; detents are chosen on release.
/// Haptics mark the strip boundary without consuming gesture movement.
#[derive(Default)]
pub(crate) struct TabPinch {
    origin: Option<f32>,
    position: f32,
    tracking: bool,
    blocked: bool,
    boundary_feedback_sent: bool,
    feedback_pending: bool,
}

impl TabPinch {
    pub(crate) fn cancel(&mut self) {
        *self = Self {
            blocked: true,
            ..Self::default()
        };
    }

    pub(crate) fn take_feedback(&mut self) -> bool {
        std::mem::take(&mut self.feedback_pending)
    }

    pub(crate) fn sample(
        &mut self,
        event: &gpui::PinchEvent,
        current_position: f32,
        _now: Instant,
    ) -> Option<GestureFrame> {
        if event.phase == gpui::TouchPhase::Started {
            *self = Self::default();
        }
        if event.phase == gpui::TouchPhase::Cancelled || !event.delta.is_finite() {
            let frame = self.tracking.then_some(GestureFrame::Cancelled);
            self.cancel();
            return frame;
        }
        if self.blocked {
            return None;
        }
        let origin = *self.origin.get_or_insert_with(|| {
            self.position = current_position;
            current_position
        });
        let previous = self.position;
        self.position = (self.position - event.delta * PINCH_GAIN).clamp(0.0, OVERVIEW_DISTANCE);
        let crossed_strip = (previous < PEEK_DISTANCE && self.position >= PEEK_DISTANCE)
            || (previous > PEEK_DISTANCE && self.position <= PEEK_DISTANCE);
        if crossed_strip && !self.boundary_feedback_sent {
            self.feedback_pending = true;
            self.boundary_feedback_sent = true;
        }
        let distance = self.position - origin;
        if event.phase == gpui::TouchPhase::Ended {
            let frame = self.tracking.then_some(GestureFrame::Released(distance));
            *self = Self {
                feedback_pending: self.feedback_pending,
                ..Self::default()
            };
            return frame;
        }
        if !self.tracking {
            // Grab an existing presentation immediately, including a settle in flight.
            // The threshold only protects the closed state from incidental pinches.
            if origin == 0.0 && distance.abs() < 10.0 {
                return None;
            }
            self.tracking = true;
        }
        Some(GestureFrame::Tracking(distance))
    }
}

pub(crate) struct TabPeek<T = SessionId> {
    pub(crate) sessions: Vec<T>,
    pub(crate) focused: usize,
    distance: f32,
    settle: Option<Settle>,
    gesture_origin: f32,
    closing: bool,
    pub(crate) tracking: bool,
}

impl<T> Default for TabPeek<T> {
    fn default() -> Self {
        Self {
            sessions: Vec::new(),
            focused: 0,
            distance: 0.0,
            settle: None,
            gesture_origin: 0.0,
            closing: false,
            tracking: false,
        }
    }
}

impl<T: Clone + PartialEq> TabPeek<T> {
    pub(crate) fn visible(&self) -> bool {
        !self.closing && self.paint_visible()
    }
    pub(crate) fn paint_visible(&self) -> bool {
        !self.sessions.is_empty() && (self.distance > 0.0 || self.settle.is_some())
    }
    pub(crate) fn is_closing(&self) -> bool {
        self.closing
    }
    pub(crate) fn is_settling(&self) -> bool {
        self.settle.is_some()
    }
    pub(crate) fn advance_motion(&mut self, now: Instant) {
        if let Some(settle) = self.settle {
            let (distance, done) = settle.sample(now);
            self.distance = distance;
            if done {
                self.settle = None;
                if settle.to == 0.0 {
                    self.dismiss();
                }
            }
        }
    }
    pub(crate) fn animate_to(&mut self, target: f32, now: Instant, reduced_motion: bool) {
        self.advance_motion(now);
        self.tracking = false;
        self.closing = target == 0.0;
        if reduced_motion {
            self.settle = None;
            self.distance = target;
            if target == 0.0 {
                self.dismiss();
            }
        } else {
            self.settle = Settle::new(self.distance, target, now);
            if self.settle.is_none() && target == 0.0 {
                self.dismiss();
            }
        }
    }
    pub(crate) fn update_animated(
        &mut self,
        frame: GestureFrame,
        now: Instant,
        reduced_motion: bool,
    ) {
        self.advance_motion(now);
        match frame {
            GestureFrame::Tracking(distance) if distance.is_finite() => {
                if !self.tracking {
                    self.gesture_origin = self.distance;
                }
                self.distance = (self.gesture_origin + distance).clamp(0.0, OVERVIEW_DISTANCE);
                self.tracking = true;
                self.closing = false;
                self.settle = None;
            }
            GestureFrame::Released(distance) if distance.is_finite() => {
                let final_distance = if self.tracking {
                    self.gesture_origin + distance
                } else {
                    self.distance + distance
                };
                if self.tracking {
                    self.distance = final_distance.clamp(0.0, OVERVIEW_DISTANCE);
                }
                let target = if final_distance < 45.0 {
                    0.0
                } else if final_distance < 260.0 {
                    PEEK_DISTANCE
                } else {
                    OVERVIEW_DISTANCE
                };
                self.animate_to(target, now, reduced_motion);
            }
            _ => {
                if !self.closing {
                    self.animate_to(0.0, now, reduced_motion);
                }
            }
        }
    }
    pub(crate) fn begin(&mut self, sessions: Vec<T>, selected: Option<&T>) {
        self.focused = selected
            .and_then(|id| sessions.iter().position(|item| item == id))
            .unwrap_or(0);
        self.sessions = sessions;
    }
    #[cfg(test)]
    pub(crate) fn update(&mut self, frame: GestureFrame) {
        self.settle = None;
        self.closing = false;
        match frame {
            GestureFrame::Cancelled => self.dismiss(),
            GestureFrame::Tracking(distance) => {
                self.tracking = true;
                self.distance = if distance.is_finite() {
                    distance.clamp(0.0, OVERVIEW_DISTANCE)
                } else {
                    0.0
                };
            }
            GestureFrame::Released(distance) => {
                self.tracking = false;
                if !distance.is_finite() || distance < 45.0 {
                    self.dismiss();
                } else {
                    self.distance = if distance < 260.0 {
                        PEEK_DISTANCE
                    } else {
                        OVERVIEW_DISTANCE
                    };
                }
            }
        }
    }
    pub(crate) fn dismiss(&mut self) {
        self.sessions.clear();
        self.settle = None;
        self.closing = false;
        self.gesture_origin = 0.0;
        self.distance = 0.0;
        self.tracking = false;
    }
    pub(crate) fn reveal(&self) -> f32 {
        (self.distance / PEEK_DISTANCE).clamp(0.0, 1.0)
    }
    pub(crate) fn position(&self) -> f32 {
        self.distance
    }
    pub(crate) fn overview(&self) -> f32 {
        ((self.distance - PEEK_DISTANCE) / (OVERVIEW_DISTANCE - PEEK_DISTANCE)).clamp(0.0, 1.0)
    }
    pub(crate) fn advance(&mut self, by: isize) {
        if !self.sessions.is_empty() {
            self.focused =
                (self.focused as isize + by).rem_euclid(self.sessions.len() as isize) as usize;
        }
    }
    pub(crate) fn selected(&self) -> Option<T> {
        self.sessions.get(self.focused).cloned()
    }
}

/// A fixed source viewport with a changing paint origin. The terminal keeps
/// receiving the original width/height; only this presentation offset changes.
pub(crate) fn terminal_offset<T: Clone + PartialEq>(
    peek: &TabPeek<T>,
    reduced_motion: bool,
) -> f32 {
    if reduced_motion {
        0.0
    } else {
        PEEK_CONTENT_OFFSET * peek.reveal()
    }
}

/// Keep the preview strip above the translated terminal during reveal/return.
/// Clipping at the overlay top removes cards as they leave the viewport.
pub(crate) fn preview_reveal_offset<T: Clone + PartialEq>(
    peek: &TabPeek<T>,
    reduced_motion: bool,
) -> f32 {
    if reduced_motion {
        0.0
    } else {
        -PEEK_CONTENT_OFFSET * (1.0 - peek.reveal())
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct CardRect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

fn overview_card_rect(index: usize, count: usize, width: f32, height: f32) -> CardRect {
    let width = width.max(1.0);
    let columns = if width < 620.0 { 1 } else { 2 };
    let rows = count.div_ceil(columns).max(1);
    let gap = 16.0;
    let target_width = ((width - gap * (columns + 1) as f32) / columns as f32).clamp(1.0, 460.0);
    let target_height = (target_width * 0.6).min((height - 92.0).max(64.0));
    let total_width = columns as f32 * target_width + (columns - 1) as f32 * gap;
    let total_height = rows as f32 * (target_height + gap) - gap;
    let target_x = (width - total_width) / 2.0 + (index % columns) as f32 * (target_width + gap);
    let target_y = 60.0
        + ((height - 80.0 - total_height) / 2.0).max(0.0)
        + (index / columns) as f32 * (target_height + gap);
    CardRect {
        x: target_x,
        y: target_y,
        width: target_width,
        height: target_height,
    }
}

pub(crate) fn card_rect<T: Clone + PartialEq>(
    index: usize,
    count: usize,
    width: f32,
    height: f32,
    peek: &TabPeek<T>,
    reduced_motion: bool,
) -> CardRect {
    let width = width.max(1.0);
    let columns = if width < 620.0 { 1 } else { 2 };
    let gap = 16.0;
    let target = overview_card_rect(index, count, width, height);
    let target_top = overview_card_rect(0, count, width, height).y;
    let strip_width = 176.0_f32.min((width - 24.0).max(1.0));
    let t = if reduced_motion {
        if peek.overview() > 0.5 { 1.0 } else { 0.0 }
    } else {
        peek.overview()
    };
    let smooth = |v: f32| {
        let v = v.clamp(0.0, 1.0);
        v * v * (3.0 - 2.0 * v)
    };
    let growth = smooth(t);
    let mix = |a, b| a + (b - a) * growth;
    let card_width = mix(strip_width, target.width);
    let card_height = mix(114.0, target.height);
    // Grow from the first movement. Until rows separate, use the growing card
    // width for strip spacing; then fold into columns without crossing cards.
    let separate = smooth(t / 0.25);
    let fold = smooth((t - 0.25) / 0.75);
    let strip_x = 12.0 + index as f32 * (card_width + 12.0)
        - ((peek.focused as f32 * (card_width + 12.0) + card_width + 24.0 - width).max(0.0));
    CardRect {
        x: strip_x + (target.x - strip_x) * fold,
        y: mix(48.0, target_top)
            + (index / columns) as f32 * (card_height + gap) * separate
            + preview_reveal_offset(peek, reduced_motion),
        width: card_width,
        height: card_height,
    }
}

/// Scroll with the morph so the selected strip card stays in view even when
/// it belongs to a late overview row. This uses the painted geometry (including
/// row separation), rather than waiting until expansion finishes to jump rows.
pub(crate) fn preview_scroll_anchor<T: Clone + PartialEq>(
    peek: &TabPeek<T>,
    width: f32,
    height: f32,
    reduced_motion: bool,
) -> f32 {
    if peek.sessions.is_empty() {
        return 0.0;
    }
    let count = peek.sessions.len();
    let target = overview_card_rect(peek.focused, count, width, height);
    // Keep the whole initial strip in view when its focused card already fits
    // in the overview. Only late rows need the camera to follow the morph.
    if target.y + target.height + 24.0 <= height {
        return 0.0;
    }
    let first = card_rect(0, count, width, height, peek, reduced_motion);
    let focused = card_rect(peek.focused, count, width, height, peek, reduced_motion);
    let t = if reduced_motion {
        if peek.overview() > 0.5 { 1.0 } else { 0.0 }
    } else {
        peek.overview()
    };
    let growth = t * t * (3.0 - 2.0 * t);
    let bottom = (height - focused.height - 24.0).max(first.y);
    let viewport_y = first.y + (bottom - first.y) * growth;
    (focused.y - viewport_y).max(0.0)
}

/// Intersect the same interpolated geometry used by the painter. Offscreen
/// terminal streams are closed even while cards continue to exist in the tree.
pub(crate) fn visible_card_indices<T: Clone + PartialEq>(
    peek: &TabPeek<T>,
    width: f32,
    height: f32,
    scroll_y: f32,
    reduced_motion: bool,
) -> Vec<usize> {
    if !peek.paint_visible() || width <= 0.0 || height <= 0.0 {
        return Vec::new();
    }
    (0..peek.sessions.len())
        .filter(|index| {
            let rect = card_rect(
                *index,
                peek.sessions.len(),
                width,
                height,
                peek,
                reduced_motion,
            );
            rect.x < width
                && rect.x + rect.width > 0.0
                && rect.y + scroll_y < height
                && rect.y + rect.height + scroll_y > 0.0
        })
        .collect()
}

/// Recognizes only a stable set of three contacts. Coordinates are normalized
/// trackpad coordinates with Y up; changing finger count cancels until lifted.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct ThreeFingerGesture {
    origin: Option<([u64; 3], f32, f32)>,
    last_distance: f32,
    recognized: bool,
    blocked: bool,
}
#[cfg(test)]
impl ThreeFingerGesture {
    pub(crate) fn sample(
        &mut self,
        touches: Vec<(u64, f32, f32)>,
        cancelled: bool,
    ) -> Option<GestureFrame> {
        self.sample_with_reverse(touches, cancelled, false)
    }

    pub(crate) fn sample_with_reverse(
        &mut self,
        mut touches: Vec<(u64, f32, f32)>,
        cancelled: bool,
        revealed: bool,
    ) -> Option<GestureFrame> {
        if cancelled {
            *self = Self {
                blocked: true,
                ..Self::default()
            };
            return Some(GestureFrame::Cancelled);
        }
        if touches.is_empty() {
            let result = self
                .recognized
                .then_some(GestureFrame::Released(self.last_distance));
            *self = Self::default();
            return result;
        }
        if self.blocked {
            return None;
        }
        if touches.len() > 3 {
            let result = self.recognized.then_some(GestureFrame::Cancelled);
            self.recognized = false;
            self.blocked = true;
            return result;
        }
        if touches.len() != 3 {
            if self.origin.is_some() {
                let result = self
                    .recognized
                    .then_some(GestureFrame::Released(self.last_distance));
                self.recognized = false;
                self.blocked = true;
                return result;
            }
            return None;
        }
        touches.sort_unstable_by_key(|touch| touch.0);
        let ids = [touches[0].0, touches[1].0, touches[2].0];
        let x = touches.iter().map(|t| t.1).sum::<f32>() / 3.0;
        let y = touches.iter().map(|t| t.2).sum::<f32>() / 3.0;
        let Some((original_ids, origin_x, origin_y)) = self.origin else {
            self.origin = Some((ids, x, y));
            return None;
        };
        if ids != original_ids {
            self.blocked = true;
            self.recognized = false;
            return Some(GestureFrame::Cancelled);
        }
        let down = (origin_y - y) * 1200.0;
        if !self.recognized {
            if (x - origin_x).abs() * 1200.0 > 18.0 && (x - origin_x).abs() * 1200.0 > down.abs() {
                self.blocked = true;
                return None;
            }
            if !revealed && down < -18.0 {
                self.blocked = true;
                return None;
            }
            if (if revealed { down.abs() } else { down }) < 8.0 {
                return None;
            }
            self.recognized = true;
        }
        self.last_distance = if revealed { down } else { down.max(0.0) };
        Some(GestureFrame::Tracking(self.last_distance))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn touches(x: f32, y: f32) -> Vec<(u64, f32, f32)> {
        (1..=3).map(|id| (id, x, y)).collect()
    }
    #[test]
    fn reduced_motion_preserves_relative_strokes_and_release_thresholds() {
        let now = Instant::now();
        let mut peek = TabPeek::default();
        let sessions = vec![SessionId::new("a"), SessionId::new("b")];
        peek.begin(sessions.clone(), sessions.first());
        peek.animate_to(OVERVIEW_DISTANCE, now, true);
        peek.update_animated(GestureFrame::Tracking(-100.0), now, true);
        assert_eq!(peek.distance, 280.0);
        assert!(peek.visible());
        peek.update_animated(GestureFrame::Tracking(-200.0), now, true);
        assert_eq!(peek.distance, 180.0);
        peek.update_animated(GestureFrame::Released(-200.0), now, true);
        assert_eq!(peek.distance, PEEK_DISTANCE);
        assert!(!peek.is_settling());
        assert_eq!(peek.sessions, sessions);
        assert_eq!(peek.selected(), sessions.first().cloned());
        peek.update_animated(GestureFrame::Tracking(-120.0), now, true);
        assert_eq!(peek.distance, 20.0);
        peek.update_animated(GestureFrame::Released(-120.0), now, true);
        assert!(!peek.paint_visible());
        assert!(peek.sessions.is_empty());
    }

    #[test]
    fn ordinary_scroll_and_upward_or_horizontal_swipes_do_not_reveal() {
        let mut gesture = ThreeFingerGesture::default();
        assert_eq!(
            gesture.sample(vec![(1, 0.0, 0.0), (2, 0.0, 0.0)], false),
            None
        );
        gesture.sample(touches(0.5, 0.5), false);
        assert_eq!(gesture.sample(touches(0.6, 0.5), false), None);
        assert_eq!(gesture.sample(touches(0.6, 0.1), false), None);
        gesture.sample(vec![], false);
        gesture.sample(touches(0.5, 0.5), false);
        assert_eq!(gesture.sample(touches(0.5, 0.6), false), None);
    }
    #[test]
    fn reverse_then_release_cancels_and_replacement_contacts_cancel() {
        let mut gesture = ThreeFingerGesture::default();
        gesture.sample(touches(0.5, 0.5), false);
        assert!(matches!(
            gesture.sample(touches(0.5, 0.3), false),
            Some(GestureFrame::Tracking(_))
        ));
        assert_eq!(
            gesture.sample(touches(0.5, 0.5), false),
            Some(GestureFrame::Tracking(0.0))
        );
        assert_eq!(
            gesture.sample(vec![], false),
            Some(GestureFrame::Released(0.0))
        );
        gesture.sample(touches(0.5, 0.5), false);
        let mut replacement = touches(0.5, 0.2);
        replacement[0].0 = 4;
        assert_eq!(
            gesture.sample(replacement, false),
            Some(GestureFrame::Cancelled)
        );
    }
    #[test]
    fn revealed_three_finger_swipe_can_fold_overview_and_cancel_without_changing_identity() {
        let now = Instant::now();
        let id = SessionId("same-work".into());
        let mut peek = TabPeek::default();
        peek.begin(vec![id.clone()], Some(&id));
        peek.update(GestureFrame::Released(380.0));
        let mut gesture = ThreeFingerGesture::default();
        assert_eq!(
            gesture.sample_with_reverse(touches(0.5, 0.5), false, true),
            None
        );
        let frame = gesture
            .sample_with_reverse(touches(0.5, 0.7), false, true)
            .unwrap();
        assert!(matches!(frame, GestureFrame::Tracking(distance) if distance < -239.0));
        peek.update_animated(frame, now, false);
        assert_eq!(peek.selected(), Some(id.clone()));
        assert!((peek.distance - 140.0).abs() < 0.01);
        let released = gesture.sample_with_reverse(vec![], false, true).unwrap();
        peek.update_animated(released, now, false);
        peek.advance_motion(now + Settle::DURATION);
        assert_eq!(peek.distance, 140.0);
        assert_eq!(peek.selected(), Some(id));
        gesture.sample_with_reverse(touches(0.5, 0.5), false, true);
        let reverse = gesture
            .sample_with_reverse(touches(0.5, 0.7), false, true)
            .unwrap();
        peek.update_animated(reverse, now + Settle::DURATION, false);
        let release = gesture.sample_with_reverse(vec![], false, true).unwrap();
        peek.update_animated(release, now + Settle::DURATION, false);
        assert!(!peek.paint_visible());
    }

    #[test]
    fn revealed_gesture_still_rejects_horizontal_motion_and_cancels_contact_changes() {
        let mut gesture = ThreeFingerGesture::default();
        gesture.sample_with_reverse(touches(0.5, 0.5), false, true);
        assert_eq!(
            gesture.sample_with_reverse(touches(0.7, 0.5), false, true),
            None
        );
        gesture.sample_with_reverse(vec![], false, true);
        gesture.sample_with_reverse(touches(0.5, 0.5), false, true);
        assert!(
            gesture
                .sample_with_reverse(touches(0.5, 0.7), false, true)
                .is_some()
        );
        let mut extra = touches(0.5, 0.7);
        extra.push((4, 0.5, 0.7));
        assert_eq!(
            gesture.sample_with_reverse(extra, false, true),
            Some(GestureFrame::Cancelled)
        );
    }

    #[test]
    fn strip_cards_keep_clear_of_terminal_during_reveal_and_return() {
        let mut peek = TabPeek::default();
        peek.begin(vec![SessionId("same-work".into())], None);
        for distance in [1.0, 20.0, 70.0, 140.0] {
            peek.update(GestureFrame::Tracking(distance));
            let card = card_rect(0, 1, 1000.0, 700.0, &peek, false);
            assert!((terminal_offset(&peek, false) - (card.y + card.height) - 14.0).abs() < 0.001);
        }
    }

    #[test]
    fn release_and_cancel_never_select_or_change_work_identity() {
        let ids = vec![
            SessionId::new("one"),
            SessionId::new("two"),
            SessionId::new("three"),
        ];
        let mut peek = TabPeek::default();
        peek.begin(ids.clone(), Some(&ids[0]));
        peek.update(GestureFrame::Released(300.0));
        peek.advance(-1);
        assert_eq!(peek.selected(), Some(ids[2].clone()));
        assert_eq!(peek.overview(), 1.0);
        peek.update(GestureFrame::Cancelled);
        assert!(!peek.visible());
        assert_eq!(peek.selected(), None);
    }
    #[test]
    fn added_finger_cancels_and_staggered_lifts_release_only_once() {
        let mut gesture = ThreeFingerGesture::default();
        gesture.sample(touches(0.5, 0.5), false);
        gesture.sample(touches(0.5, 0.3), false);
        let mut four = touches(0.5, 0.3);
        four.push((4, 0.5, 0.3));
        assert_eq!(gesture.sample(four, false), Some(GestureFrame::Cancelled));
        assert_eq!(gesture.sample(vec![], false), None);
        gesture.sample(touches(0.5, 0.5), false);
        gesture.sample(touches(0.5, 0.3), false);
        let mut two = touches(0.5, 0.3);
        two.pop();
        assert!(matches!(
            gesture.sample(two, false),
            Some(GestureFrame::Released(_))
        ));
        assert_eq!(gesture.sample(vec![], false), None);
    }
    #[test]
    fn coalesced_final_frame_contains_complete_release_or_cancel_state() {
        let (tx, mut rx) = tokio::sync::watch::channel(GestureFrame::Cancelled);
        tx.send_replace(GestureFrame::Tracking(10.0));
        tx.send_replace(GestureFrame::Tracking(300.0));
        tx.send_replace(GestureFrame::Released(300.0));
        let mut peek = TabPeek::default();
        peek.begin(vec![SessionId::new("one")], None);
        peek.update(*rx.borrow_and_update());
        assert!(peek.visible());
        assert_eq!(peek.overview(), 1.0);
        assert!(!peek.tracking);
        tx.send_replace(GestureFrame::Tracking(240.0));
        tx.send_replace(GestureFrame::Cancelled);
        peek.update(*rx.borrow_and_update());
        assert!(!peek.visible());
    }

    #[test]
    fn cards_do_not_cross_over_each_other_between_strip_and_overview() {
        let mut peek = TabPeek::default();
        peek.begin(
            (0..12).map(|i| SessionId::new(i.to_string())).collect(),
            None,
        );
        for width in [400.0, 800.0, 1100.0] {
            for focused in [0, 5, 11] {
                peek.focused = focused;
                for step in 0..=240 {
                    peek.update(GestureFrame::Tracking(140.0 + step as f32));
                    let cards: Vec<_> = (0..12)
                        .map(|i| card_rect(i, 12, width, 700.0, &peek, false))
                        .collect();
                    for (i, a) in cards.iter().enumerate() {
                        for b in &cards[i + 1..] {
                            let overlap_x = (a.x + a.width).min(b.x + b.width) - a.x.max(b.x);
                            let overlap_y = (a.y + a.height).min(b.y + b.height) - a.y.max(b.y);
                            assert!(
                                overlap_x <= 0.01 || overlap_y <= 0.01,
                                "cards overlap at width {width}, focus {focused}, step {step}: {a:?} {b:?}"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn geometry_is_continuous_and_reduced_motion_has_no_terminal_travel() {
        let mut peek = TabPeek::default();
        peek.begin(vec![SessionId::new("one")], None);
        peek.update(GestureFrame::Tracking(140.0));
        let first = card_rect(0, 4, 1000.0, 700.0, &peek, false);
        peek.update(GestureFrame::Tracking(141.0));
        let second = card_rect(0, 4, 1000.0, 700.0, &peek, false);
        assert!((first.width - second.width).abs() < 2.0);
        assert_eq!(terminal_offset(&peek, true), 0.0);
        assert_eq!(terminal_offset(&peek, false), PEEK_CONTENT_OFFSET);
    }
}

#[cfg(test)]
mod visibility_tests {
    use super::*;
    #[test]
    fn only_cards_intersecting_the_actual_strip_or_scrolled_overview_are_live() {
        let mut peek = TabPeek::default();
        peek.begin(
            (0..100).map(|i| SessionId::new(i.to_string())).collect(),
            None,
        );
        peek.update(GestureFrame::Released(140.0));
        assert_eq!(
            visible_card_indices(&peek, 800.0, 600.0, 0.0, false),
            vec![0, 1, 2, 3, 4]
        );
        peek.advance(20);
        let moved = visible_card_indices(&peek, 800.0, 600.0, 0.0, false);
        assert!(moved.contains(&20));
        assert!(!moved.contains(&0));
        peek.update(GestureFrame::Released(380.0));
        let top = visible_card_indices(&peek, 800.0, 600.0, 0.0, false);
        let scrolled = visible_card_indices(&peek, 800.0, 600.0, -1000.0, false);
        assert!(top.contains(&0));
        assert!(!scrolled.contains(&0));
        assert!(scrolled.iter().all(|index| *index > 3));
        peek.dismiss();
        assert!(visible_card_indices(&peek, 800.0, 600.0, 0.0, false).is_empty());
    }
}

#[cfg(test)]
mod settling_tests {
    use super::*;
    use std::time::Duration;
    fn peek() -> TabPeek {
        let mut peek = TabPeek::default();
        peek.begin(vec![SessionId::new("same-work")], None);
        peek
    }
    #[test]
    fn release_has_no_pose_jump_and_a_new_gesture_interrupts_at_the_current_pose() {
        let now = Instant::now();
        let mut peek = peek();
        peek.update_animated(GestureFrame::Tracking(285.0), now, false);
        peek.update_animated(GestureFrame::Released(285.0), now, false);
        assert_eq!(peek.distance, 285.0);
        assert!(peek.is_settling());
        let halfway = now + Duration::from_millis(100);
        peek.advance_motion(halfway);
        let pose = peek.distance;
        assert!(pose > 285.0 && pose < 380.0);
        peek.update_animated(GestureFrame::Tracking(0.0), halfway, false);
        assert_eq!(peek.distance, pose);
        assert!(!peek.is_settling());
        peek.update_animated(GestureFrame::Tracking(-25.0), halfway, false);
        assert_eq!(peek.distance, pose - 25.0);
        assert_eq!(peek.selected(), Some(SessionId::new("same-work")));
    }
    #[test]
    fn cancel_keeps_a_finite_paint_return_but_releases_interaction_immediately() {
        let now = Instant::now();
        let mut peek = peek();
        peek.update_animated(GestureFrame::Tracking(140.0), now, false);
        peek.update_animated(GestureFrame::Cancelled, now, false);
        assert_eq!(peek.distance, 140.0);
        assert!(!peek.visible());
        assert!(peek.paint_visible());
        peek.advance_motion(now + Settle::DURATION);
        assert!(!peek.paint_visible());
        assert!(!peek.is_settling());
        assert!(peek.sessions.is_empty());
    }
    #[test]
    fn reduced_motion_and_coalesced_release_finish_without_animation() {
        let now = Instant::now();
        let mut peek = peek();
        peek.update_animated(GestureFrame::Released(300.0), now, true);
        assert_eq!(peek.distance, 380.0);
        assert!(!peek.is_settling());
        peek.update_animated(GestureFrame::Cancelled, now, true);
        assert!(!peek.paint_visible());
    }
}

#[cfg(test)]
mod pinch_tests {
    use super::*;
    use std::time::Duration;

    fn event(delta: f32, phase: gpui::TouchPhase) -> gpui::PinchEvent {
        gpui::PinchEvent {
            delta,
            phase,
            ..Default::default()
        }
    }

    #[test]
    fn ordinary_pinch_settles_at_small_preview_even_when_delivered_quickly() {
        let now = Instant::now();
        for interval in [8, 100] {
            let mut pinch = TabPinch::default();
            let mut peek = TabPeek::default();
            peek.begin(vec![SessionId::new("work")], None);
            for (i, phase, delta) in [
                (0, gpui::TouchPhase::Started, 0.0),
                (1, gpui::TouchPhase::Moved, -0.16),
                (2, gpui::TouchPhase::Moved, -0.16),
                (3, gpui::TouchPhase::Ended, 0.0),
            ] {
                let time = now + Duration::from_millis(i * interval);
                if let Some(frame) = pinch.sample(&event(delta, phase), peek.position(), time) {
                    peek.update_animated(frame, time, true);
                }
            }
            assert_eq!(peek.position(), PEEK_DISTANCE);
        }
    }

    #[test]
    fn cards_start_growing_when_leaving_the_strip() {
        let mut peek = TabPeek::default();
        peek.begin(vec![SessionId::new("work")], None);
        peek.update(GestureFrame::Tracking(PEEK_DISTANCE));
        let small = card_rect(0, 1, 1000.0, 700.0, &peek, false);
        peek.update(GestureFrame::Tracking(PEEK_DISTANCE + 24.0));
        let growing = card_rect(0, 1, 1000.0, 700.0, &peek, false);
        assert!(growing.width > small.width && growing.height > small.height);
    }

    #[test]
    fn early_cards_do_not_scroll_out_before_the_overview_needs_scroll() {
        let mut peek = TabPeek::default();
        peek.begin(
            (0..6).map(|i| SessionId::new(i.to_string())).collect(),
            None,
        );
        peek.focused = 2;
        for step in 0..=240 {
            peek.update(GestureFrame::Tracking(PEEK_DISTANCE + step as f32));
            assert_eq!(preview_scroll_anchor(&peek, 1000.0, 700.0, false), 0.0);
        }
    }

    #[test]
    fn focused_card_stays_visible_while_small_preview_expands() {
        let mut peek = TabPeek::default();
        peek.begin(
            (0..24).map(|i| SessionId::new(i.to_string())).collect(),
            None,
        );
        for width in [400.0, 800.0, 1100.0] {
            for focused in [5, 11, 23] {
                peek.focused = focused;
                for step in 0..=240 {
                    peek.update(GestureFrame::Tracking(PEEK_DISTANCE + step as f32));
                    assert!(
                        visible_card_indices(
                            &peek,
                            width,
                            700.0,
                            -preview_scroll_anchor(&peek, width, 700.0, false),
                            false
                        )
                        .contains(&focused),
                        "focused card disappeared at width {width}, focus {focused}, step {step}"
                    );
                }
            }
        }
    }

    #[test]
    fn identical_strokes_have_identical_positions_regardless_of_timing() {
        let now = Instant::now();
        for interval in [8, 16, 100, 500] {
            let mut pinch = TabPinch::default();
            pinch.sample(&event(0.0, gpui::TouchPhase::Started), 0.0, now);
            let mut frame = None;
            for i in 1..=8 {
                frame = pinch.sample(
                    &event(-0.08, gpui::TouchPhase::Moved),
                    0.0,
                    now + Duration::from_millis(i * interval),
                );
            }
            assert_eq!(frame, Some(GestureFrame::Tracking(320.0)));
        }
    }

    #[test]
    fn reversing_across_strip_has_no_dead_zone_and_feedback_is_bounded() {
        let now = Instant::now();
        let mut pinch = TabPinch::default();
        pinch.sample(&event(0.0, gpui::TouchPhase::Started), 120.0, now);
        assert_eq!(
            pinch.sample(&event(-0.08, gpui::TouchPhase::Moved), 120.0, now),
            Some(GestureFrame::Tracking(40.0))
        );
        assert!(pinch.take_feedback());
        assert_eq!(
            pinch.sample(&event(0.08, gpui::TouchPhase::Moved), 160.0, now),
            Some(GestureFrame::Tracking(0.0))
        );
        assert!(!pinch.take_feedback());
        assert_eq!(
            pinch.sample(&event(-0.08, gpui::TouchPhase::Moved), 120.0, now),
            Some(GestureFrame::Tracking(40.0))
        );
        assert!(!pinch.take_feedback());
    }

    #[test]
    fn bounds_do_not_accumulate_hidden_movement_and_cancel_blocks_until_start() {
        let now = Instant::now();
        let mut pinch = TabPinch::default();
        pinch.sample(&event(0.0, gpui::TouchPhase::Started), 380.0, now);
        pinch.sample(&event(-0.5, gpui::TouchPhase::Moved), 380.0, now);
        assert_eq!(
            pinch.sample(&event(0.04, gpui::TouchPhase::Moved), 380.0, now),
            Some(GestureFrame::Tracking(-20.0))
        );
        assert_eq!(
            pinch.sample(&event(0.0, gpui::TouchPhase::Cancelled), 360.0, now),
            Some(GestureFrame::Cancelled)
        );
        assert_eq!(
            pinch.sample(&event(-0.1, gpui::TouchPhase::Moved), 0.0, now),
            None
        );
        pinch.sample(&event(0.0, gpui::TouchPhase::Started), 0.0, now);
        assert_eq!(
            pinch.sample(&event(-0.08, gpui::TouchPhase::Moved), 0.0, now),
            Some(GestureFrame::Tracking(40.0))
        );
    }

    #[test]
    fn existing_pose_is_grabbed_before_first_movement() {
        let now = Instant::now();
        let mut pinch = TabPinch::default();
        assert_eq!(
            pinch.sample(&event(0.0, gpui::TouchPhase::Started), 225.0, now),
            Some(GestureFrame::Tracking(0.0))
        );
        assert_eq!(
            pinch.sample(&event(0.01, gpui::TouchPhase::Moved), 225.0, now),
            Some(GestureFrame::Tracking(-5.0))
        );
        assert_eq!(
            pinch.sample(&event(0.0, gpui::TouchPhase::Ended), 220.0, now),
            Some(GestureFrame::Released(-5.0))
        );
    }

    #[test]
    fn pinch_reversal_interrupts_settle_without_a_pose_jump() {
        let now = Instant::now();
        let mut peek = TabPeek::default();
        peek.begin(vec![SessionId::new("same-work")], None);
        peek.update_animated(GestureFrame::Tracking(280.0), now, false);
        peek.update_animated(GestureFrame::Released(280.0), now, false);
        let grabbed_at = now + Duration::from_millis(80);
        peek.advance_motion(grabbed_at);
        let pose = peek.position();
        let mut pinch = TabPinch::default();
        let frame = pinch
            .sample(&event(0.0, gpui::TouchPhase::Started), pose, grabbed_at)
            .unwrap();
        peek.update_animated(frame, grabbed_at, false);
        assert_eq!(peek.position(), pose);
        assert!(!peek.is_settling());
        let moved_at = grabbed_at + Duration::from_millis(16);
        let frame = pinch
            .sample(&event(0.08, gpui::TouchPhase::Moved), pose, moved_at)
            .unwrap();
        peek.update_animated(frame, moved_at, false);
        assert_eq!(peek.position(), pose - 40.0);
        assert_eq!(peek.selected(), Some(SessionId::new("same-work")));
    }

    #[test]
    fn pinch_from_small_preview_moves_immediately() {
        let now = Instant::now();
        let mut pinch = TabPinch::default();
        pinch.sample(&event(0.0, gpui::TouchPhase::Started), PEEK_DISTANCE, now);
        assert_eq!(
            pinch.sample(
                &event(-0.08, gpui::TouchPhase::Moved),
                PEEK_DISTANCE,
                now + Duration::from_millis(16)
            ),
            Some(GestureFrame::Tracking(40.0)),
        );
    }

    #[test]
    fn continuous_pinch_preserves_movement_across_preview_boundary() {
        let now = Instant::now();
        let mut pinch = TabPinch::default();
        pinch.sample(&event(0.0, gpui::TouchPhase::Started), 0.0, now);
        pinch.sample(&event(-0.24, gpui::TouchPhase::Moved), 0.0, now);
        assert_eq!(
            pinch.sample(
                &event(-0.08, gpui::TouchPhase::Moved),
                120.0,
                now + Duration::from_millis(16)
            ),
            Some(GestureFrame::Tracking(160.0)),
        );
    }
}
