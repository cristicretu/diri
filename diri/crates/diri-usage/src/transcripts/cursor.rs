//! Pure billed-usage records and ledger helpers. Provider authentication stays in the desktop app.
use super::{cache::CursorFetchWindow, model::UsageHourAgg, parser::fnv1a};
const OVERLAP_MS: i64 = 5 * 60 * 1000;
#[derive(Clone, Debug, PartialEq)]
pub struct CursorUsageEvent {
    pub id: String,
    pub timestamp_ms: i64,
    pub model: String,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    pub cost: f64,
}

pub fn event_hour(event: &CursorUsageEvent) -> i64 {
    event.timestamp_ms.div_euclid(3_600_000)
}

pub fn event_aggregate(event: &CursorUsageEvent) -> UsageHourAgg {
    UsageHourAgg {
        i: event.input_tokens,
        o: event.output_tokens,
        cr: event.cache_read_tokens,
        cw: event.cache_write_tokens,
        c: event.cost,
    }
}

/// A bounded page batch; only a complete walk may advance the committed watermark.
pub struct CursorBatch {
    pub events: Vec<CursorUsageEvent>,
    pub window: CursorFetchWindow,
    pub complete: bool,
}

pub fn cursor_overlap_ms() -> i64 {
    OVERLAP_MS
}

pub fn event_dedup_hash(id: &str) -> u64 {
    fnv1a(id)
}
