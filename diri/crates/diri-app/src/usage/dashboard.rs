//! Historical projections from the same deduplicated transcript ledger as the
//! account menu. Days use UTC, explicitly labeled in the UI. No transcript text
//! or identifiers cross this boundary.
use super::{UsageHourAgg, UsageProvider, UsageTotals};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub(crate) type ModelHours = BTreeMap<String, BTreeMap<i64, UsageDetail>>;

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct UsageDetail {
    pub tokens: UsageHourAgg,
    pub reasoning: i64,
    pub priced_tokens: i64,
    /// Cache reads at the uncached input rate minus their estimated read cost.
    /// Cache writes are excluded: this is read savings, not net savings.
    pub read_savings: f64,
}

impl UsageDetail {
    pub fn totals(self) -> UsageTotals {
        let mut totals = UsageTotals::default();
        totals += self.tokens;
        totals
    }

    fn merge(&mut self, other: Self) {
        self.tokens.merge(other.tokens);
        self.reasoning += other.reasoning;
        self.priced_tokens += other.priced_tokens;
        self.read_savings += other.read_savings;
    }
}

pub(crate) fn record(
    hours: &mut ModelHours,
    model: &str,
    hour: i64,
    tokens: UsageHourAgg,
    pricing: Option<diri_usage::ModelPricing>,
    reasoning: i64,
) {
    let detail = UsageDetail {
        tokens,
        reasoning,
        priced_tokens: pricing.map_or(0, |_| tokens.i + tokens.o + tokens.cr + tokens.cw),
        read_savings: pricing.map_or(0.0, |price| {
            tokens.cr as f64 * (price.input - price.cache_read()) / 1_000_000.0
        }),
    };
    hours
        .entry(
            if model.is_empty() {
                "Unknown model"
            } else {
                model
            }
            .to_owned(),
        )
        .or_default()
        .entry(hour)
        .or_default()
        .merge(detail);
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct UsageHistory {
    pub(crate) claude: ModelHours,
    pub(crate) codex: ModelHours,
}

#[derive(Clone, Debug)]
pub struct ModelRow {
    pub model: String,
    pub provider: usize,
    pub detail: UsageDetail,
}

#[derive(Clone, Debug, Default)]
pub struct DayRow {
    pub day: i64,
    pub providers: [UsageDetail; 2],
}

impl DayRow {
    pub fn total(&self) -> UsageDetail {
        let mut detail = self.providers[0];
        detail.merge(self.providers[1]);
        detail
    }
}

#[derive(Clone, Debug, Default)]
pub struct UsageReport {
    pub total: UsageDetail,
    pub providers: [UsageDetail; 2],
    pub days: Vec<DayRow>,
    pub models: Vec<ModelRow>,
    pub active_days: usize,
}

impl UsageHistory {
    pub(crate) fn merge(&mut self, provider: UsageProvider, details: &ModelHours) {
        let destination = match provider {
            UsageProvider::Claude => &mut self.claude,
            UsageProvider::Codex => &mut self.codex,
        };
        for (model, hours) in details {
            for (&hour, &detail) in hours {
                destination
                    .entry(model.clone())
                    .or_default()
                    .entry(hour)
                    .or_default()
                    .merge(detail);
            }
        }
    }

    pub fn report(&self, now: i64, days: usize) -> UsageReport {
        let days = days.clamp(1, 90);
        let end = now.div_euclid(86_400);
        let start = end - days as i64 + 1;
        let mut report = UsageReport {
            days: (start..=end)
                .map(|day| DayRow {
                    day,
                    ..DayRow::default()
                })
                .collect(),
            ..UsageReport::default()
        };
        for (provider, models) in [&self.claude, &self.codex].into_iter().enumerate() {
            for (model, hours) in models {
                let mut detail = UsageDetail::default();
                for (&hour, &value) in hours.range(start * 24..=now.div_euclid(3_600)) {
                    detail.merge(value);
                    let index = (hour.div_euclid(24) - start) as usize;
                    report.days[index].providers[provider].merge(value);
                }
                if detail.totals().total_tokens() == 0 {
                    continue;
                }
                report.providers[provider].merge(detail);
                report.total.merge(detail);
                report.models.push(ModelRow {
                    model: model.clone(),
                    provider,
                    detail,
                });
            }
        }
        report.models.sort_by(|a, b| {
            b.detail
                .tokens
                .c
                .total_cmp(&a.detail.tokens.c)
                .then_with(|| {
                    b.detail
                        .totals()
                        .total_tokens()
                        .cmp(&a.detail.totals().total_tokens())
                })
                .then_with(|| a.model.cmp(&b.model))
        });
        report.active_days = report
            .days
            .iter()
            .filter(|day| day.total().totals().total_tokens() > 0)
            .count();
        report
    }
}

/// Gregorian date from epoch day (inverse of days_from_civil).
pub fn date_label(day: i64) -> String {
    let z = day + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = mp + if mp < 10 { 3 } else { -9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}
