//! Lossless coalescing for daemon-model publication.
//!
//! The Store mutates synchronously as events arrive, but rebuilding GPUI and
//! menu projections is intentionally limited to one publication interval. A
//! bounded channel is the wrong abstraction here: saturation may discard the
//! final event in a burst and leave subscribers stale forever. This mailbox
//! retains one merged semantic value (`Model` dominates `Resources`) and one
//! wake permit, regardless of burst size.

use std::sync::atomic::{AtomicU8, Ordering};

use diri_proto::EventName;
use tokio::sync::Notify;

use super::{StoreEventChange, UI_PUBLISH_INTERVAL};

#[derive(Debug, Default)]
pub(super) struct PublicationMailbox {
    pending: AtomicU8,
    wake: Notify,
}

impl PublicationMailbox {
    pub(super) fn submit(&self, change: StoreEventChange) {
        let value = match change {
            StoreEventChange::None => return,
            StoreEventChange::Resources => 1,
            StoreEventChange::Model => 2,
        };
        self.pending.fetch_max(value, Ordering::AcqRel);
        self.wake.notify_one();
    }

    pub(super) async fn next(&self) -> StoreEventChange {
        loop {
            // Register before checking so a submit between the check and await
            // leaves a permit instead of becoming a lost wake.
            let notified = self.wake.notified();
            if self.pending.load(Ordering::Acquire) == 0 {
                notified.await;
            }
            tokio::time::sleep(UI_PUBLISH_INTERVAL).await;
            match self.pending.swap(0, Ordering::AcqRel) {
                2 => return StoreEventChange::Model,
                1 => return StoreEventChange::Resources,
                _ => continue,
            }
        }
    }
}

/// Filters replay already covered by an authoritative `session.list`
/// snapshot. Events beyond the watermark remain ordered and are applied.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct ResyncWatermark {
    generation: u64,
    sequence: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum EventAdmission {
    Apply,
    SkipCovered,
    ResyncGap,
    ResyncConnection,
}

impl ResyncWatermark {
    pub(super) fn covers_generation(self, generation: u64) -> bool {
        self.generation >= generation
    }

    pub(super) fn install(&mut self, generation: u64, sequence: u64) {
        self.generation = generation;
        self.sequence = sequence;
    }

    pub(super) fn admit(self, generation: u64, name: &str, sequence: u64) -> EventAdmission {
        if generation < self.generation {
            EventAdmission::SkipCovered
        } else if generation > self.generation {
            EventAdmission::ResyncConnection
        } else if name == EventName::EVENTS_DROPPED {
            EventAdmission::ResyncGap
        } else if sequence <= self.sequence {
            EventAdmission::SkipCovered
        } else {
            EventAdmission::Apply
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use super::*;

    #[tokio::test]
    async fn a_burst_keeps_its_strongest_final_publication_without_capacity() {
        let mailbox = Arc::new(PublicationMailbox::default());
        for _ in 0..10_000 {
            mailbox.submit(StoreEventChange::Resources);
        }
        mailbox.submit(StoreEventChange::Model);
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), mailbox.next())
                .await
                .expect("publication"),
            StoreEventChange::Model
        );

        // A change arriving after the first value was taken owns the next
        // publication; the previous wake cannot consume or erase it.
        mailbox.submit(StoreEventChange::Resources);
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), mailbox.next())
                .await
                .expect("next publication"),
            StoreEventChange::Resources
        );
    }

    #[test]
    fn a_resync_watermark_drops_only_snapshot_covered_replay() {
        let mut watermark = ResyncWatermark::default();
        watermark.install(3, 41);
        assert_eq!(
            watermark.admit(3, EventName::SESSION_UPDATED, 40),
            EventAdmission::SkipCovered
        );
        assert_eq!(
            watermark.admit(3, EventName::SESSION_UPDATED, 41),
            EventAdmission::SkipCovered
        );
        assert_eq!(
            watermark.admit(3, EventName::SESSION_UPDATED, 42),
            EventAdmission::Apply
        );
        watermark.install(4, 10);
        assert_eq!(
            watermark.admit(4, EventName::SESSION_UPDATED, 11),
            EventAdmission::Apply,
            "a new connection generation installs its own lower sequence"
        );
        assert_eq!(
            watermark.admit(3, EventName::SESSION_UPDATED, 500),
            EventAdmission::SkipCovered,
            "queued events from the retired connection stay retired"
        );
    }

    #[test]
    fn daemon_drop_markers_force_resync_even_with_the_out_of_band_zero_sequence() {
        let mut watermark = ResyncWatermark::default();
        watermark.install(1, 500);
        assert_eq!(
            watermark.admit(1, EventName::EVENTS_DROPPED, 0),
            EventAdmission::ResyncGap
        );
    }
}
