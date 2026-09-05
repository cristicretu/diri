//! Incremental, daemon-free usage accounting for Claude Code and Codex transcripts.
//!
//! Costs are computed locally from the transcripts on disk; nothing is sent
//! anywhere. The separate limits reader queries provider-reported subscription
//! windows without deriving quota percentages from these estimates.

mod cache;
pub mod dashboard;
mod fleet;
pub(crate) mod limits;
mod model;
mod parser;
mod pricing;
mod store;
mod timestamp;
mod watcher;

pub(crate) use fleet::merge_fleet_usage;
pub use model::{ProviderUsage, UsageHourAgg, UsageSnapshot, UsageTotals};
pub use pricing::PRICING_ENTRY_COUNT;
pub use store::{
    Clock, ClockReading, RefreshStats, ScanPaths, SystemClock, UsageFormat, UsageProvider,
    UsageStore,
};
pub(crate) use watcher::{TranscriptInvalidation, TranscriptWatcher};

#[cfg(test)]
mod tests;
