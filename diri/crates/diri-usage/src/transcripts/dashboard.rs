//! Historical projections from the same deduplicated transcript ledger as the
//! account menu. Days use UTC, explicitly labeled in the UI. No transcript text
//! or identifiers cross this boundary.
use super::{UsageHourAgg, UsageProvider, UsageTotals};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub type ModelHours = BTreeMap<String, BTreeMap<i64, UsageDetail>>;

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

pub(crate) fn record_billed(hours: &mut ModelHours, model: &str, hour: i64, tokens: UsageHourAgg) {
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
        .merge(UsageDetail {
            tokens,
            reasoning: 0,
            priced_tokens: tokens.i + tokens.o + tokens.cr + tokens.cw,
            read_savings: 0.0,
        });
}

pub fn record(
    hours: &mut ModelHours,
    model: &str,
    hour: i64,
    tokens: UsageHourAgg,
    pricing: Option<crate::ModelPricing>,
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
    pub claude: ModelHours,
    pub codex: ModelHours,
    pub cursor: ModelHours,
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
    pub providers: [UsageDetail; 3],
}

impl DayRow {
    pub fn total(&self) -> UsageDetail {
        let mut detail = self.providers[0];
        detail.merge(self.providers[1]);
        detail.merge(self.providers[2]);
        detail
    }
}

#[derive(Clone, Debug, Default)]
pub struct UsageReport {
    pub total: UsageDetail,
    pub providers: [UsageDetail; 3],
    pub days: Vec<DayRow>,
    pub models: Vec<ModelRow>,
    pub active_days: usize,
}

impl UsageHistory {
    pub fn merge(&mut self, provider: UsageProvider, details: &ModelHours) {
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
        for (provider, models) in [&self.claude, &self.codex, &self.cursor]
            .into_iter()
            .enumerate()
        {
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

impl UsageHistory {
    pub fn remote_summary(
        &self,
        collected_at: i64,
        source_id: String,
    ) -> Result<diri_proto::remote_pty::TranscriptUsageResult, &'static str> {
        use diri_proto::remote_pty::{
            MAX_USAGE_BUCKETS, TranscriptUsageBucket, TranscriptUsageResult,
        };
        let mut result = TranscriptUsageResult {
            source_id,
            collected_at,
            buckets: Vec::new(),
        };
        let today = collected_at.div_euclid(86_400);
        for (provider, models) in [("claude", &self.claude), ("codex", &self.codex)] {
            for (model, hours) in models {
                let mut days = BTreeMap::<i64, UsageDetail>::new();
                for (&hour, &detail) in hours {
                    let day = hour.div_euclid(24);
                    if (today - 91..=today).contains(&day) {
                        days.entry(day).or_default().merge(detail);
                    }
                }
                for (day, detail) in days {
                    if result.buckets.len() == MAX_USAGE_BUCKETS {
                        return Err("remote usage bucket limit exceeded");
                    }
                    result.buckets.push(TranscriptUsageBucket {
                        provider: provider.into(),
                        model: model.clone(),
                        day,
                        input: detail.tokens.i,
                        output: detail.tokens.o,
                        cache_read: detail.tokens.cr,
                        cache_write: detail.tokens.cw,
                        reasoning: detail.reasoning,
                        priced_tokens: detail.priced_tokens,
                        estimated_usd: detail.tokens.c,
                        read_savings_usd: detail.read_savings,
                    });
                }
            }
        }
        result.validate()?;
        Ok(result)
    }

    /// The caller replaces the per-host snapshot before building this projection.
    pub fn merge_remote(
        &mut self,
        result: &diri_proto::remote_pty::TranscriptUsageResult,
    ) -> Result<(), &'static str> {
        result.validate()?;
        for row in &result.buckets {
            let provider = if row.provider == "claude" {
                UsageProvider::Claude
            } else {
                UsageProvider::Codex
            };
            let detail = UsageDetail {
                tokens: UsageHourAgg {
                    i: row.input,
                    o: row.output,
                    cr: row.cache_read,
                    cw: row.cache_write,
                    c: row.estimated_usd,
                },
                reasoning: row.reasoning,
                priced_tokens: row.priced_tokens,
                read_savings: row.read_savings_usd,
            };
            self.merge(
                provider,
                &BTreeMap::from([(row.model.clone(), BTreeMap::from([(row.day * 24, detail)]))]),
            );
        }
        Ok(())
    }
}
