//! Presentation-only tab peek state. It never owns a terminal or issues effects.
use crate::peek_settle::Settle;
use diri_proto::SessionId;
use std::time::Instant;

pub(crate) const PEEK_DISTANCE: f32 = 140.0;
const OVERVIEW_DISTANCE: f32 = 380.0;
const PEEK_CONTENT_OFFSET: f32 = 176.0;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) enum GestureFrame {
    #[default]
    Cancelled,
    #[cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))]
    Tracking(f32),
    #[cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))]
    Released(f32),
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
    let strip_width = 176.0_f32.min((width - 24.0).max(1.0));
    // Keep the keyboard-focused tab on screen in the strip without changing
    // session order. The overview becomes a vertically scrollable collection.
    let strip_x = 12.0 + index as f32 * (strip_width + 12.0)
        - ((peek.focused as f32 * (strip_width + 12.0) + strip_width + 24.0 - width).max(0.0));
    let t = if reduced_motion {
        if peek.overview() > 0.5 { 1.0 } else { 0.0 }
    } else {
        peek.overview()
    };
    let mix = |a, b| a + (b - a) * t;
    CardRect {
        x: mix(strip_x, target_x),
        y: mix(48.0, target_y) + preview_reveal_offset(peek, reduced_motion),
        width: mix(strip_width, target_width),
        height: mix(114.0, target_height),
    }
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
#[cfg(any(target_os = "macos", test))]
#[derive(Default)]
pub(crate) struct ThreeFingerGesture {
    origin: Option<([u64; 3], f32, f32)>,
    last_distance: f32,
    recognized: bool,
    blocked: bool,
}
#[cfg(any(target_os = "macos", test))]
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
