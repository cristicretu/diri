//! Bounded, main-thread delivery of native gesture samples. Motion may coalesce
//! within a stroke; release and cancellation remain ordered barriers.
use crate::tab_peek::GestureFrame;
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    time::Instant,
};

const CAPACITY: usize = 16;

#[derive(Clone, Copy, Debug)]
pub(crate) struct GestureSample {
    pub(crate) frame: GestureFrame,
    pub(crate) observed_at: Instant,
    starts_stroke: bool,
}

#[derive(Default)]
pub(crate) struct GestureBatch {
    samples: [Option<GestureSample>; CAPACITY],
    len: usize,
}
impl GestureBatch {
    pub(crate) fn iter(&self) -> impl Iterator<Item = GestureSample> + '_ {
        self.samples[..self.len].iter().flatten().copied()
    }
}

#[derive(Default)]
struct Pending {
    batch: GestureBatch,
    overflowed: bool,
}

pub(crate) struct GestureSender {
    pending: Rc<RefCell<Pending>>,
    wake: tokio::sync::watch::Sender<()>,
    in_stroke: Cell<bool>,
}
pub(crate) struct GestureReceiver {
    pending: Rc<RefCell<Pending>>,
    wake: tokio::sync::watch::Receiver<()>,
}

pub(crate) fn channel() -> (GestureSender, GestureReceiver) {
    let pending = Rc::new(RefCell::new(Pending::default()));
    let (tx, rx) = tokio::sync::watch::channel(());
    (
        GestureSender {
            pending: pending.clone(),
            wake: tx,
            in_stroke: Cell::new(false),
        },
        GestureReceiver { pending, wake: rx },
    )
}
impl GestureSender {
    /// False means delivery overflowed. The producer must discard the current
    /// contact sequence until all fingers lift. The consumer receives Cancelled.
    pub(crate) fn send(&self, frame: GestureFrame, observed_at: Instant) -> bool {
        let mut pending = self.pending.borrow_mut();
        if pending.overflowed {
            return false;
        }
        let tracking = matches!(frame, GestureFrame::Tracking(_));
        let starts_stroke = tracking && !self.in_stroke.get();
        self.in_stroke.set(tracking);
        let sample = GestureSample {
            frame,
            observed_at,
            starts_stroke,
        };
        let len = pending.batch.len;
        if len > 0
            && matches!(sample.frame, GestureFrame::Tracking(_))
            && matches!(
                pending.batch.samples[len - 1].unwrap().frame,
                GestureFrame::Tracking(_)
            )
            && !pending.batch.samples[len - 1].unwrap().starts_stroke
        {
            pending.batch.samples[len - 1] = Some(sample);
            return true;
        }
        if len == CAPACITY {
            *pending = Pending::default();
            pending.batch.samples[0] = Some(GestureSample {
                frame: GestureFrame::Cancelled,
                observed_at: sample.observed_at,
                starts_stroke: false,
            });
            pending.batch.len = 1;
            pending.overflowed = true;
            self.in_stroke.set(false);
            return false;
        }
        pending.batch.samples[len] = Some(sample);
        pending.batch.len += 1;
        drop(pending);
        if len == 0 {
            self.wake.send_replace(());
        }
        true
    }

    /// Explicit window cancellation invalidates all previously queued motion.
    pub(crate) fn cancel(&self, observed_at: Instant) {
        *self.pending.borrow_mut() = Pending::default();
        self.in_stroke.set(false);
        self.send(GestureFrame::Cancelled, observed_at);
    }
}
impl GestureReceiver {
    pub(crate) async fn recv(&mut self) -> Option<GestureBatch> {
        while self.wake.changed().await.is_ok() {
            if let Some(batch) = self.take_pending() {
                return Some(batch);
            }
        }
        None
    }

    pub(crate) fn take_pending(&mut self) -> Option<GestureBatch> {
        self.wake.borrow_and_update();
        let mut pending = self.pending.borrow_mut();
        (pending.batch.len > 0).then(|| std::mem::take(&mut *pending).batch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tab_peek::TabPeek;
    use std::time::Duration;

    #[test]
    fn delayed_delivery_matches_ordered_strokes_and_elapsed_settles() {
        for reduced in [false, true] {
            let (sender, mut receiver) = channel();
            let start = Instant::now();
            let mut ordered = TabPeek::default();
            ordered.begin(vec!["one"], None);
            ordered.animate_to(380.0, start, true);
            let mut delayed = TabPeek::default();
            delayed.begin(vec!["one"], None);
            delayed.animate_to(380.0, start, true);
            let samples = [
                (0, GestureFrame::Tracking(-200.0)),
                (10, GestureFrame::Released(-200.0)),
                (300, GestureFrame::Tracking(-20.0)),
                (310, GestureFrame::Tracking(-40.0)),
                (320, GestureFrame::Tracking(-60.0)),
                (330, GestureFrame::Released(-60.0)),
                (600, GestureFrame::Cancelled),
                (900, GestureFrame::Tracking(50.0)),
                (910, GestureFrame::Tracking(120.0)),
                (920, GestureFrame::Released(120.0)),
            ];
            for (millis, frame) in samples {
                let now = start + Duration::from_millis(millis);
                if ordered.sessions.is_empty() {
                    ordered.begin(vec!["one"], None);
                }
                ordered.update_animated(frame, now, reduced);
                assert!(sender.send(frame, now));
            }
            let batch = receiver.take_pending().unwrap();
            assert!(batch.len < samples.len(), "motion should coalesce");
            for sample in batch.iter() {
                if delayed.sessions.is_empty() {
                    delayed.begin(vec!["one"], None);
                }
                delayed.update_animated(sample.frame, sample.observed_at, reduced);
            }
            let final_time = start + Duration::from_secs(2);
            ordered.advance_motion(final_time);
            delayed.advance_motion(final_time);
            assert_eq!(ordered.reveal(), delayed.reveal());
            assert_eq!(ordered.overview(), delayed.overview());
            assert_eq!(ordered.tracking, delayed.tracking);
            assert_eq!(ordered.selected(), delayed.selected());
            assert_eq!(delayed.reveal(), 1.0);
            assert_eq!(delayed.overview(), 0.0);
            assert!(receiver.take_pending().is_none());
        }
    }

    #[test]
    fn coalescing_keeps_stroke_origin_during_an_active_settle() {
        let start = Instant::now();
        let (sender, mut receiver) = channel();
        let mut peek = TabPeek::default();
        peek.begin(vec!["one"], None);
        peek.update_animated(GestureFrame::Tracking(200.0), start, false);
        peek.update_animated(GestureFrame::Released(200.0), start, false);
        for (millis, distance) in [(50, 20.0), (100, 40.0), (150, 60.0)] {
            assert!(sender.send(
                GestureFrame::Tracking(distance),
                start + Duration::from_millis(millis)
            ));
        }
        for sample in receiver.take_pending().unwrap().iter() {
            peek.update_animated(sample.frame, sample.observed_at, false);
        }
        // 200→140 settle is at165.3125 when the stroke starts at50ms;
        // its latest60pt displacement must keep that first origin.
        assert!((peek.overview() - (225.3125 - 140.0) / 240.0).abs() < 0.001);
    }

    #[tokio::test]
    async fn overflow_is_bounded_and_recovers_after_cancel_is_drained() {
        let now = Instant::now();
        let (sender, mut receiver) = channel();
        for _ in 0..CAPACITY / 2 {
            assert!(sender.send(GestureFrame::Tracking(100.0), now));
            assert!(sender.send(GestureFrame::Released(100.0), now));
        }
        assert!(!sender.send(GestureFrame::Tracking(200.0), now));
        for _ in 0..1000 {
            assert!(!sender.send(GestureFrame::Released(200.0), now));
        }
        let frames: Vec<_> = receiver
            .recv()
            .await
            .unwrap()
            .iter()
            .map(|s| s.frame)
            .collect();
        assert_eq!(frames, [GestureFrame::Cancelled]);
        assert!(sender.send(GestureFrame::Tracking(80.0), now));
        assert!(sender.send(GestureFrame::Released(80.0), now));
        drop(sender);
        assert_eq!(receiver.recv().await.unwrap().iter().count(), 2);
        assert!(receiver.recv().await.is_none());
    }

    #[test]
    fn explicit_window_cancellation_discards_stale_motion() {
        let (sender, mut receiver) = channel();
        let now = Instant::now();
        sender.send(GestureFrame::Tracking(380.0), now);
        sender.send(GestureFrame::Released(380.0), now);
        sender.cancel(now);
        assert_eq!(
            receiver
                .take_pending()
                .unwrap()
                .iter()
                .map(|s| s.frame)
                .collect::<Vec<_>>(),
            [GestureFrame::Cancelled]
        );
    }

    #[test]
    fn every_consumer_partition_preserves_relative_stroke_state() {
        let start = Instant::now();
        let samples = [
            (0, GestureFrame::Tracking(-100.0)),
            (10, GestureFrame::Tracking(-200.0)),
            (20, GestureFrame::Released(-200.0)),
            (100, GestureFrame::Tracking(-20.0)),
            (150, GestureFrame::Tracking(-40.0)),
            (200, GestureFrame::Tracking(-60.0)),
            (210, GestureFrame::Released(-60.0)),
            (450, GestureFrame::Cancelled),
        ];
        for reduced in [false, true] {
            for drain_mask in 0..(1 << samples.len()) {
                let (sender, mut receiver) = channel();
                let mut ordered = TabPeek::default();
                ordered.begin(vec!["one"], None);
                ordered.animate_to(380.0, start, true);
                let mut delayed = TabPeek::default();
                delayed.begin(vec!["one"], None);
                delayed.animate_to(380.0, start, true);
                for (index, (millis, frame)) in samples.iter().enumerate() {
                    let now = start + Duration::from_millis(*millis);
                    ordered.update_animated(*frame, now, reduced);
                    assert!(sender.send(*frame, now));
                    if drain_mask & (1 << index) != 0 || index == samples.len() - 1 {
                        for sample in receiver.take_pending().unwrap().iter() {
                            delayed.update_animated(sample.frame, sample.observed_at, reduced);
                        }
                        assert!((ordered.reveal() - delayed.reveal()).abs() < 0.001);
                        assert!((ordered.overview() - delayed.overview()).abs() < 0.001);
                        assert_eq!(ordered.tracking, delayed.tracking);
                        assert_eq!(ordered.selected(), delayed.selected());
                    }
                }
            }
        }
    }

    #[tokio::test]
    async fn motion_flood_keeps_only_origin_latest_and_one_pending_wake() {
        let (sender, mut receiver) = channel();
        let now = Instant::now();
        for value in 0..100_000 {
            assert!(sender.send(GestureFrame::Tracking(value as f32), now));
        }
        let batch = receiver.recv().await.unwrap();
        assert_eq!(
            batch.iter().map(|sample| sample.frame).collect::<Vec<_>>(),
            [
                GestureFrame::Tracking(0.0),
                GestureFrame::Tracking(99_999.0)
            ]
        );
        assert!(!receiver.wake.has_changed().unwrap());
    }
}
