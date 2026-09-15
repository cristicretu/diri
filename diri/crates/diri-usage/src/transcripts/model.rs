use std::ops::AddAssign;

use serde::{Deserialize, Serialize};

/// Token and USD totals for one display window.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct UsageTotals {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    pub cost: f64,
}

impl UsageTotals {
    #[must_use]
    pub const fn total_tokens(self) -> i64 {
        self.input_tokens + self.output_tokens + self.cache_read_tokens + self.cache_write_tokens
    }
}

impl AddAssign<UsageHourAgg> for UsageTotals {
    fn add_assign(&mut self, rhs: UsageHourAgg) {
        self.input_tokens += rhs.i;
        self.output_tokens += rhs.o;
        self.cache_read_tokens += rhs.cr;
        self.cache_write_tokens += rhs.cw;
        self.cost += rhs.c;
    }
}

impl AddAssign for UsageTotals {
    fn add_assign(&mut self, rhs: Self) {
        self.input_tokens += rhs.input_tokens;
        self.output_tokens += rhs.output_tokens;
        self.cache_read_tokens += rhs.cache_read_tokens;
        self.cache_write_tokens += rhs.cache_write_tokens;
        self.cost += rhs.cost;
    }
}

/// One provider's display windows.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ProviderUsage {
    /// The active Claude five-hour block. Codex remains zero because its rate
    /// window is monthly and the Swift implementation deliberately excludes it.
    pub session: UsageTotals,
    pub today: UsageTotals,
    pub month: UsageTotals,
}

/// The UI-facing usage projection. Dates are Unix seconds.
#[derive(Clone, Debug, PartialEq)]
pub struct UsageSnapshot<L = ()> {
    pub claude: ProviderUsage,
    pub codex: ProviderUsage,
    pub cursor: ProviderUsage,
    pub session_cost: Option<f64>,
    pub session_started_at: Option<i64>,
    pub session_ends_at: Option<i64>,
    pub session_remaining_seconds: Option<i64>,
    pub updated_at: i64,
    /// Default local account quotas; never inferred from transcript costs.
    pub limits: Vec<L>,
    pub remote: Vec<RemoteUsageSnapshot>,
    /// Local transcript history; fleet summaries do not provide this detail.
    pub history: std::sync::Arc<super::dashboard::UsageHistory>,
}

impl<L> UsageSnapshot<L> {
    pub fn with_limits<T>(self, limits: Vec<T>) -> UsageSnapshot<T> {
        UsageSnapshot {
            claude: self.claude,
            codex: self.codex,
            cursor: self.cursor,
            session_cost: self.session_cost,
            session_started_at: self.session_started_at,
            session_ends_at: self.session_ends_at,
            session_remaining_seconds: self.session_remaining_seconds,
            updated_at: self.updated_at,
            history: self.history,
            limits,
            remote: self.remote,
        }
    }

    #[must_use]
    pub fn today(&self) -> UsageTotals {
        let mut totals = self.claude.today;
        totals += self.codex.today;
        totals += self.cursor.today;
        totals
    }

    #[must_use]
    pub fn month(&self) -> UsageTotals {
        let mut totals = self.claude.month;
        totals += self.codex.month;
        totals += self.cursor.month;
        totals
    }
}

/// One epoch-hour aggregate. Short serialized keys match the Swift cache.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct UsageHourAgg {
    pub i: i64,
    pub o: i64,
    pub cr: i64,
    pub cw: i64,
    pub c: f64,
}

impl UsageHourAgg {
    pub(crate) fn merge(&mut self, other: Self) {
        self.i += other.i;
        self.o += other.o;
        self.cr += other.cr;
        self.cw += other.cw;
        self.c += other.c;
    }
}

impl<L> Default for UsageSnapshot<L> {
    fn default() -> Self {
        Self {
            claude: ProviderUsage::default(),
            codex: ProviderUsage::default(),
            cursor: ProviderUsage::default(),
            session_cost: None,
            session_started_at: None,
            session_ends_at: None,
            session_remaining_seconds: None,
            updated_at: 0,
            limits: Vec::new(),
            history: Default::default(),
            remote: Vec::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum RemoteUsageStatus {
    Loading,
    Ready,
    Unavailable,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RemoteUsageSnapshot {
    pub host: String,
    pub name: String,
    pub status: RemoteUsageStatus,
    pub data: Option<std::sync::Arc<diri_proto::remote_pty::TranscriptUsageResult>>,
}

impl<L> UsageSnapshot<L> {
    /// None selects all machines; an empty string selects only this machine.
    pub fn history_for_source(&self, host: Option<&str>) -> super::dashboard::UsageHistory {
        let mut history = if host.is_none_or(str::is_empty) {
            (*self.history).clone()
        } else {
            super::dashboard::UsageHistory::default()
        };
        let mut sources = std::collections::BTreeMap::new();
        for remote in &self.remote {
            if host.is_none_or(|id| id == remote.host)
                && let Some(data) = &remote.data
            {
                let saved = sources.entry(&data.source_id).or_insert(data);
                if data.collected_at >= saved.collected_at {
                    *saved = data;
                }
            }
        }
        for data in sources.values() {
            let _ = history.merge_remote(data);
        }
        history
    }
}
