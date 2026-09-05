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
#[derive(Clone, Debug, Default, PartialEq)]
pub struct UsageSnapshot {
    pub claude: ProviderUsage,
    pub codex: ProviderUsage,
    pub session_cost: Option<f64>,
    pub session_started_at: Option<i64>,
    pub session_ends_at: Option<i64>,
    pub session_remaining_seconds: Option<i64>,
    pub updated_at: i64,
    /// Default local account quotas; never inferred from transcript costs.
    pub limits: Vec<super::limits::AccountLimits>,
    /// Local transcript history; fleet summaries do not provide this detail.
    pub history: std::sync::Arc<super::dashboard::UsageHistory>,
}

impl UsageSnapshot {
    #[must_use]
    pub fn today(&self) -> UsageTotals {
        let mut totals = self.claude.today;
        totals += self.codex.today;
        totals
    }

    #[must_use]
    pub fn month(&self) -> UsageTotals {
        let mut totals = self.claude.month;
        totals += self.codex.month;
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
