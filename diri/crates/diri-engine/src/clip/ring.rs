//! Bounded eviction for the Context Ring. Pure: the store applies the plan.
//!
//! The ring is held newest first. A clip is kept when it is younger than the
//! TTL and it lies in the longest prefix of the unexpired clips that fits the
//! count and byte budgets. `formal/ContextRing.lean` proves that what is
//! kept always fits both budgets, is a prefix (the newest clips survive), and
//! that eviction is idempotent.

use diri_proto::clip::{ClipRecord, DEFAULT_MAX_BYTES, DEFAULT_MAX_CLIPS, DEFAULT_TTL_MS};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub max_clips: usize,
    pub max_bytes: u64,
    pub ttl_ms: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_clips: DEFAULT_MAX_CLIPS,
            max_bytes: DEFAULT_MAX_BYTES,
            ttl_ms: DEFAULT_TTL_MS,
        }
    }
}

/// How many of the newest clips to keep. `ring` must be newest first.
/// Expired clips end the kept prefix too: everything older than an expired
/// clip is older still.
pub fn keep_count(ring: &[ClipRecord], now_ms: u64, limits: Limits) -> usize {
    let mut bytes = 0u64;
    for (index, clip) in ring.iter().enumerate() {
        let expired = now_ms.saturating_sub(clip.created_at_ms) >= limits.ttl_ms;
        let next = bytes.saturating_add(clip.charged_bytes());
        if expired || index >= limits.max_clips || next > limits.max_bytes {
            return index;
        }
        bytes = next;
    }
    ring.len()
}

/// Sorts newest first (ties by id, so the order is total) and splits the
/// ring into what stays and what goes.
pub fn evict(
    mut ring: Vec<ClipRecord>,
    now_ms: u64,
    limits: Limits,
) -> (Vec<ClipRecord>, Vec<ClipRecord>) {
    ring.sort_by(|a, b| {
        b.created_at_ms
            .cmp(&a.created_at_ms)
            .then_with(|| b.id.cmp(&a.id))
    });
    let keep = keep_count(&ring, now_ms, limits);
    let evicted = ring.split_off(keep);
    (ring, evicted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use diri_proto::clip::{ClipSource, ClipTextOrigin};

    fn clip(id: &str, at: u64, bytes: u64) -> ClipRecord {
        ClipRecord {
            id: id.into(),
            created_at_ms: at,
            source: ClipSource::default(),
            tab_key: None,
            text: None,
            text_origin: ClipTextOrigin::None,
            text_truncated: false,
            png_path: None,
            png_bytes: bytes,
            width: 0,
            height: 0,
            sent_to: Vec::new(),
        }
    }

    fn ids(ring: &[ClipRecord]) -> Vec<&str> {
        ring.iter().map(|c| c.id.as_str()).collect()
    }

    const LIMITS: Limits = Limits {
        max_clips: 3,
        max_bytes: 100,
        ttl_ms: 1_000,
    };

    #[test]
    fn keeps_the_newest_within_the_count() {
        let ring = (0..5).map(|i| clip(&format!("c{i}"), 100 + i, 1)).collect();
        let (kept, evicted) = evict(ring, 200, LIMITS);
        assert_eq!(ids(&kept), ["c4", "c3", "c2"]);
        assert_eq!(ids(&evicted), ["c1", "c0"]);
    }

    #[test]
    fn keeps_within_the_byte_budget() {
        let ring = vec![clip("a", 3, 60), clip("b", 2, 40), clip("c", 1, 1)];
        let (kept, evicted) = evict(ring, 10, LIMITS);
        assert_eq!(ids(&kept), ["a", "b"]);
        assert_eq!(ids(&evicted), ["c"]);
        let total: u64 = kept.iter().map(ClipRecord::charged_bytes).sum();
        assert!(total <= LIMITS.max_bytes);
    }

    #[test]
    fn text_counts_against_the_budget() {
        let mut big = clip("t", 5, 0);
        big.text = Some("x".repeat(101));
        let (kept, _) = evict(vec![big], 5, LIMITS);
        assert!(kept.is_empty());
    }

    #[test]
    fn expired_clips_go() {
        let ring = vec![clip("new", 1_500, 1), clip("old", 400, 1)];
        let (kept, evicted) = evict(ring, 1_500, LIMITS);
        assert_eq!(ids(&kept), ["new"]);
        assert_eq!(ids(&evicted), ["old"]);
    }

    #[test]
    fn eviction_is_idempotent_and_order_independent() {
        let ring: Vec<_> = [("a", 5, 30), ("b", 9, 30), ("c", 7, 30), ("d", 8, 30)]
            .into_iter()
            .map(|(id, at, bytes)| clip(id, at, bytes))
            .collect();
        let mut reversed = ring.clone();
        reversed.reverse();
        let (once, _) = evict(ring, 10, LIMITS);
        let (twice, gone) = evict(once.clone(), 10, LIMITS);
        assert_eq!(once, twice);
        assert!(gone.is_empty());
        assert_eq!(evict(reversed, 10, LIMITS).0, once);
        assert_eq!(ids(&once), ["b", "d", "c"]);
    }
}
