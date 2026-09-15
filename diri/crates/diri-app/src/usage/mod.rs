//! Incremental, daemon-free usage accounting for Claude Code and Codex transcripts.
//!
//! The shared Rust parser estimates usage from local and remote transcripts.
//! Only aggregates cross SSH. The separate limits reader queries provider-reported subscription
//! windows without deriving quota percentages from these estimates.

pub use diri_usage::PRICING_ENTRY_COUNT;
pub use diri_usage::transcripts::{
    Clock, ClockReading, ProviderUsage, RefreshStats, ScanPaths, SystemClock, UsageFormat,
    UsageHourAgg, UsageProvider, UsageStore, UsageTotals, dashboard,
};
mod fleet;
pub(crate) mod limits;
mod watcher;
pub(crate) use fleet::merge_fleet_usage;
pub(crate) use watcher::{TranscriptInvalidation, TranscriptWatcher};
pub type UsageSnapshot = diri_usage::transcripts::UsageSnapshot<limits::AccountLimits>;

mod remote;
pub use diri_usage::transcripts::{RemoteUsageSnapshot, RemoteUsageStatus};
pub(crate) use remote::watch_remote_usage;

pub(crate) use diri_usage::transcripts::timestamp;

mod cursor;
pub(crate) use cursor::{CursorBatch, CursorRefresh};
