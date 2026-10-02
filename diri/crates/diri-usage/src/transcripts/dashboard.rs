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
        let (start_hour, end_hour) = hour_window(now, days);
        let start_day = start_hour.div_euclid(24);
        let end_day = end_hour.div_euclid(24);
        let mut report = UsageReport {
            days: (start_day..=end_day)
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
                for (&hour, &value) in hours.range(start_hour..=end_hour) {
                    detail.merge(value);
                    let index = (hour.div_euclid(24) - start_day) as usize;
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

    pub fn hourly_provider_totals(&self, now: i64, days: usize) -> Vec<(i64, [UsageDetail; 3])> {
        let (start, end) = hour_window(now, days);
        let len = (end - start + 1) as usize;
        let mut buckets = vec![[UsageDetail::default(); 3]; len];
        for (provider, models) in [&self.claude, &self.codex, &self.cursor]
            .into_iter()
            .enumerate()
        {
            for hours in models.values() {
                for (&hour, &detail) in hours.range(start..=end) {
                    buckets[(hour - start) as usize][provider].merge(detail);
                }
            }
        }
        (0..len)
            .map(|index| (start + index as i64, buckets[index]))
            .collect()
    }

    pub fn hourly_totals(&self, now: i64, days: usize) -> Vec<(i64, UsageDetail)> {
        self.hourly_provider_totals(now, days)
            .into_iter()
            .map(|(hour, providers)| {
                let mut total = UsageDetail::default();
                for detail in providers {
                    total.merge(detail);
                }
                (hour, total)
            })
            .collect()
    }

    fn earliest_hour(&self) -> Option<i64> {
        [&self.claude, &self.codex, &self.cursor]
            .into_iter()
            .flat_map(|models| models.values())
            .filter_map(|hours| hours.keys().next().copied())
            .min()
    }

    /// Current window vs a same-length window `shift_days` earlier, both ending
    /// at the same time of day. Missing or truncated prior history is not
    /// comparable: callers should omit the delta rather than invent a change.
    pub fn compare(&self, now: i64, days: usize) -> UsageCompare {
        self.compare_shifted(now, days, days as i64)
    }

    pub fn compare_shifted(&self, now: i64, days: usize, shift_days: i64) -> UsageCompare {
        let days = days.clamp(1, 90);
        let shift_days = shift_days.max(1);
        let previous_now = now.saturating_sub(shift_days.saturating_mul(86_400));
        let current = self.report(now, days);
        let previous = self.report(previous_now, days);
        let used = previous.total.totals().total_tokens() > 0 || previous.tokens_cost() > 0.0;
        let comparable = used
            && (days == 1 || {
                let (start, _) = hour_window(previous_now, days);
                self.earliest_hour().is_some_and(|hour| hour <= start)
            });
        UsageCompare {
            current,
            previous,
            comparable,
        }
    }
}

#[derive(Clone, Debug)]
pub struct UsageCompare {
    pub current: UsageReport,
    pub previous: UsageReport,
    comparable: bool,
}

impl UsageCompare {
    pub fn comparable(&self) -> bool {
        self.comparable
    }

    pub fn cost_change(&self) -> Option<f64> {
        self.relative(self.current.tokens_cost(), self.previous.tokens_cost())
    }

    pub fn provider_cost_change(&self, provider: usize) -> Option<f64> {
        self.relative(
            self.current.providers[provider].tokens.c,
            self.previous.providers[provider].tokens.c,
        )
    }

    pub fn provider_tokens_change(&self, provider: usize) -> Option<f64> {
        self.relative(
            self.current.providers[provider].totals().total_tokens() as f64,
            self.previous.providers[provider].totals().total_tokens() as f64,
        )
    }

    pub fn processed_tokens_change(&self) -> Option<f64> {
        self.relative(
            self.current.total.totals().total_tokens() as f64,
            self.previous.total.totals().total_tokens() as f64,
        )
    }

    pub fn cached_input_change(&self) -> Option<f64> {
        self.relative(
            self.current.total.totals().cache_read_tokens as f64,
            self.previous.total.totals().cache_read_tokens as f64,
        )
    }

    pub fn uncached_input_change(&self) -> Option<f64> {
        self.relative(
            self.current.total.totals().input_tokens as f64,
            self.previous.total.totals().input_tokens as f64,
        )
    }

    pub fn output_change(&self) -> Option<f64> {
        self.relative(
            self.current.total.totals().output_tokens as f64,
            self.previous.total.totals().output_tokens as f64,
        )
    }

    pub fn read_savings_change(&self) -> Option<f64> {
        self.relative(
            self.current.total.read_savings,
            self.previous.total.read_savings,
        )
    }

    fn relative(&self, current: f64, previous: f64) -> Option<f64> {
        if self.comparable && previous > 0.0 {
            Some((current - previous) / previous)
        } else {
            None
        }
    }
}

impl UsageReport {
    fn tokens_cost(&self) -> f64 {
        self.total.tokens.c
    }
}

fn hour_window(now: i64, days: usize) -> (i64, i64) {
    let days = days.clamp(1, 90) as i64;
    let end = now.div_euclid(3_600);
    (end - days * 24 + 1, end)
}

/// Gregorian date plus hour-of-day from a Unix hour.
pub fn hour_label(hour: i64) -> String {
    format!(
        "{} {:02}:00",
        date_label(hour.div_euclid(24)),
        hour.rem_euclid(24)
    )
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

/// Unix epoch day 0 is Thursday.
pub fn weekday_label(day: i64) -> &'static str {
    const NAMES: [&str; 7] = [
        "Thursday",
        "Friday",
        "Saturday",
        "Sunday",
        "Monday",
        "Tuesday",
        "Wednesday",
    ];
    NAMES[day.rem_euclid(7) as usize]
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

#[cfg(test)]
mod tests {
    use super::*;

    fn seed(history: &mut UsageHistory, hour: i64, input: i64, cost: f64) {
        record(
            &mut history.claude,
            "claude-sonnet",
            hour,
            UsageHourAgg {
                i: input,
                o: 0,
                cr: 0,
                cw: 0,
                c: cost,
            },
            None,
            0,
        );
    }

    #[test]
    fn compare_aligns_to_the_same_hour_of_day() {
        let mut history = UsageHistory::default();
        let now: i64 = 1_700_000_000;
        let now_hour = now.div_euclid(3_600);
        seed(&mut history, now_hour, 200, 2.0);
        seed(&mut history, now_hour - 24, 100, 1.0);
        seed(&mut history, now_hour - 23, 50, 0.5);
        let compare = history.compare(now, 1);
        assert!(compare.comparable());
        assert_eq!(compare.current.total.totals().input_tokens, 250);
        assert_eq!(compare.previous.total.totals().input_tokens, 100);
        assert_eq!(compare.cost_change(), Some(1.5));
        assert_eq!(compare.provider_cost_change(0), Some(1.5));
        assert_eq!(compare.provider_tokens_change(0), Some(1.5));
        assert_eq!(compare.provider_cost_change(1), None);
    }

    #[test]
    fn snapshot_last_24h_cost_matches_the_rolling_day() {
        let mut history = UsageHistory::default();
        let now: i64 = 1_700_000_000;
        let now_hour = now.div_euclid(3_600);
        seed(&mut history, now_hour, 10, 2.0);
        seed(&mut history, now_hour - 23, 10, 0.5);
        seed(&mut history, now_hour - 24, 10, 7.0);
        let mut remote = UsageHistory::default();
        seed(&mut remote, now_hour, 10, 4.0);
        let snapshot = super::super::UsageSnapshot::<()> {
            updated_at: now,
            history: std::sync::Arc::new(history),
            remote: vec![super::super::RemoteUsageSnapshot {
                host: "forge".into(),
                name: "Forge".into(),
                status: super::super::RemoteUsageStatus::Ready,
                data: Some(std::sync::Arc::new(
                    remote.remote_summary(now, "a".repeat(32)).unwrap(),
                )),
            }],
            ..Default::default()
        };
        assert_eq!(snapshot.last_24h_cost(), 6.5);
        assert_eq!(
            snapshot.last_24h_cost(),
            snapshot
                .history_for_source(None)
                .compare(now, 1)
                .current
                .total
                .tokens
                .c
        );
    }

    #[test]
    fn compare_omits_deltas_without_prior_usage() {
        let mut history = UsageHistory::default();
        let now: i64 = 1_700_000_000;
        seed(&mut history, now.div_euclid(3_600), 200, 2.0);
        let compare = history.compare(now, 1);
        assert!(!compare.comparable());
        assert_eq!(compare.cost_change(), None);
        assert_eq!(compare.processed_tokens_change(), None);
    }

    #[test]
    fn compare_hides_truncated_multi_day_windows() {
        let mut history = UsageHistory::default();
        let now: i64 = 1_700_000_000;
        let now_hour = now.div_euclid(3_600);
        seed(&mut history, now_hour, 300, 3.0);
        seed(&mut history, now_hour - 40 * 24, 10, 0.1);
        let compare = history.compare(now, 30);
        assert!(!compare.comparable());
        assert_eq!(compare.cost_change(), None);
    }

    #[test]
    fn compare_seven_days_when_history_covers_the_prior_window() {
        let mut history = UsageHistory::default();
        let now: i64 = 1_700_000_000;
        let now_hour = now.div_euclid(3_600);
        seed(&mut history, now_hour - 14 * 24 + 1, 20, 0.2);
        seed(&mut history, now_hour - 10 * 24, 80, 0.8);
        seed(&mut history, now_hour - 2 * 24, 40, 0.4);
        let compare = history.compare(now, 7);
        assert!(compare.comparable());
        assert_eq!(compare.previous.total.totals().input_tokens, 100);
        assert_eq!(compare.current.total.totals().input_tokens, 40);
        assert_eq!(compare.processed_tokens_change(), Some(-0.6));
    }

    #[test]
    fn weekday_label_follows_unix_epoch() {
        assert_eq!(weekday_label(0), "Thursday");
        assert_eq!(weekday_label(2), "Saturday");
    }

    #[test]
    fn hourly_totals_keeps_same_day_hours_apart() {
        let mut history = UsageHistory::default();
        let now: i64 = 1_700_000_000;
        let now_hour = now.div_euclid(3_600);
        seed(&mut history, now_hour, 100, 1.0);
        seed(&mut history, now_hour - 1, 50, 0.5);
        let hours = history.hourly_totals(now, 1);
        assert_eq!(hours.len(), 24);
        assert_eq!(hours[22].0, now_hour - 1);
        assert_eq!(hours[22].1.tokens.i, 50);
        assert_eq!(hours[23].0, now_hour);
        assert_eq!(hours[23].1.tokens.i, 100);
    }

    #[test]
    fn hourly_provider_totals_stay_on_their_own_line() {
        let mut history = UsageHistory::default();
        let now: i64 = 1_700_000_000;
        let now_hour = now.div_euclid(3_600);
        seed(&mut history, now_hour, 100, 1.0);
        record(
            &mut history.cursor,
            "cursor-model",
            now_hour,
            UsageHourAgg {
                i: 40,
                o: 0,
                cr: 0,
                cw: 0,
                c: 2.0,
            },
            None,
            0,
        );
        let hours = history.hourly_provider_totals(now, 1);
        assert_eq!(hours[23].1[0].tokens.i, 100);
        assert_eq!(hours[23].1[1].tokens.i, 0);
        assert_eq!(hours[23].1[2].tokens.i, 40);
        let combined = history.hourly_totals(now, 1);
        assert_eq!(combined[23].1.tokens.i, 140);
    }

    #[test]
    fn report_totals_match_the_rolling_hourly_window() {
        let mut history = UsageHistory::default();
        let now: i64 = 1_700_000_000;
        let now_hour = now.div_euclid(3_600);
        seed(&mut history, now_hour, 40, 0.4);
        seed(&mut history, now_hour - 5, 10, 0.1);
        seed(&mut history, now_hour - 30, 80, 0.8);
        let report = history.report(now, 1);
        let hourly: f64 = history
            .hourly_totals(now, 1)
            .iter()
            .map(|(_, detail)| detail.tokens.c)
            .sum();
        assert_eq!(report.total.totals().input_tokens, 50);
        assert_eq!(report.total.tokens.c, hourly);
        assert_eq!(history.report(now, 7).total.totals().input_tokens, 130);
    }
}
