//! Pure scheduling rules: five-field cron in local time, and the catch-up
//! decision for occurrences the Engine could not fire on time.
//!
//! Every occurrence is claimed at most once. Occurrences that pile up while
//! the Mac sleeps or diri is not running collapse into a single run of the
//! newest one, which fires late if it is still inside the catch-up window and
//! is recorded as missed otherwise. Nothing is skipped silently.

use diri_proto::schedules::{ScheduleOutcome, ScheduleWhen};

/// A run this close to its due time counts as on time.
pub const ON_TIME_SLACK_MS: i64 = 90_000;
/// Bound on occurrences counted while collapsing a backlog.
const MAX_COLLAPSED: u32 = 100_000;
/// Bound on the day/hour/minute steps searched for the next occurrence;
/// enough for any satisfiable expression (Feb 29 recurs within 8 years).
const MAX_SEARCH_STEPS: usize = 200_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cron {
    minutes: u64,
    hours: u32,
    days_of_month: u32,
    months: u16,
    days_of_week: u8,
    /// Cron's rule: when both day fields are restricted, either may match.
    dom_restricted: bool,
    dow_restricted: bool,
}

impl Cron {
    pub fn parse(expr: &str) -> Result<Self, String> {
        let fields: Vec<&str> = expr.split_whitespace().collect();
        if fields.len() != 5 {
            return Err(format!(
                "cron needs five fields (minute hour day month weekday), got {}",
                fields.len()
            ));
        }
        let minutes = parse_field(fields[0], 0, 59, &[])?;
        let hours = parse_field(fields[1], 0, 23, &[])?;
        let days_of_month = parse_field(fields[2], 1, 31, &[])?;
        let months = parse_field(
            fields[3],
            1,
            12,
            &[
                "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
            ],
        )?;
        let mut days_of_week = parse_field(
            fields[4],
            0,
            7,
            &["sun", "mon", "tue", "wed", "thu", "fri", "sat"],
        )?;
        // 7 is Sunday too.
        if days_of_week & (1 << 7) != 0 {
            days_of_week |= 1;
        }
        let cron = Self {
            minutes,
            hours: hours as u32,
            days_of_month: days_of_month as u32,
            months: months as u16,
            days_of_week: (days_of_week & 0x7f) as u8,
            dom_restricted: !fields[2].starts_with('*'),
            dow_restricted: !fields[4].starts_with('*'),
        };
        Ok(cron)
    }

    fn day_matches(&self, mday: u32, wday: u32) -> bool {
        let dom = self.days_of_month & (1 << mday) != 0;
        let dow = self.days_of_week & (1 << wday) != 0;
        match (self.dom_restricted, self.dow_restricted) {
            (true, true) => dom || dow,
            _ => dom && dow,
        }
    }

    /// First occurrence strictly after `after_ms`, in local time.
    pub fn next_after(&self, after_ms: i64) -> Option<i64> {
        let mut t = after_ms.div_euclid(60_000) * 60 + 60;
        for _ in 0..MAX_SEARCH_STEPS {
            let tm = local::break_down(t)?;
            let next = if self.months & (1 << tm.month) == 0 {
                local::compose(tm.year, tm.month + 1, 1, 0, 0)?
            } else if !self.day_matches(tm.mday, tm.wday) {
                local::compose(tm.year, tm.month, tm.mday + 1, 0, 0)?
            } else if self.hours & (1 << tm.hour) == 0 || self.minutes & (1 << tm.minute) == 0 {
                t + 60
            } else {
                return Some(t * 1000);
            };
            // A DST fold can map a later wall time to an earlier instant.
            t = if next > t { next } else { t + 60 };
        }
        None
    }
    /// Latest occurrence at or before `now_ms`; used after the history count
    /// saturates so a long outage still catches up the newest occurrence.
    fn latest_at(&self, now_ms: i64) -> Option<i64> {
        let mut t = now_ms.div_euclid(60_000) * 60;
        for _ in 0..MAX_SEARCH_STEPS {
            let tm = local::break_down(t)?;
            if self.months & (1 << tm.month) == 0 || !self.day_matches(tm.mday, tm.wday) {
                t = (local::compose(tm.year, tm.month, tm.mday, 0, 0)? - 60).min(t - 60);
            } else if self.hours & (1 << tm.hour) != 0 && self.minutes & (1 << tm.minute) != 0 {
                return Some(t * 1000);
            } else {
                t -= 60;
            }
        }
        None
    }
}

fn parse_field(field: &str, min: u32, max: u32, names: &[&str]) -> Result<u64, String> {
    let mut bits = 0u64;
    for part in field.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((range, step)) => {
                let step: u32 = step
                    .parse()
                    .ok()
                    .filter(|step| *step > 0)
                    .ok_or_else(|| format!("bad step in `{part}`"))?;
                (range, step)
            }
            None => (part, 1),
        };
        let (lo, hi) = if range == "*" {
            (min, max)
        } else if let Some((lo, hi)) = range.split_once('-') {
            (value(lo, min, names)?, value(hi, min, names)?)
        } else {
            let single = value(range, min, names)?;
            // `5/15` means from 5 to the end, stepping.
            (single, if part.contains('/') { max } else { single })
        };
        if lo < min || hi > max || lo > hi {
            return Err(format!("`{part}` is outside {min}-{max}"));
        }
        let mut v = lo;
        while v <= hi {
            bits |= 1 << v;
            let Some(next) = v.checked_add(step) else {
                break;
            };
            v = next;
        }
    }
    if bits == 0 {
        return Err(format!("`{field}` matches nothing"));
    }
    Ok(bits)
}

fn value(text: &str, min: u32, names: &[&str]) -> Result<u32, String> {
    if let Ok(number) = text.parse() {
        return Ok(number);
    }
    let lower = text.to_ascii_lowercase();
    names
        .iter()
        .position(|name| *name == lower)
        .map(|index| index as u32 + min)
        .ok_or_else(|| format!("`{text}` is not a number or name"))
}

/// Validates `when` and returns its first occurrence strictly after `now_ms`.
/// A one-shot in the past is still returned so catch-up can decide its fate.
pub fn first_due(when: &ScheduleWhen, now_ms: i64) -> Result<Option<i64>, String> {
    match when {
        ScheduleWhen::Once { at } => Ok(Some(at.0 as i64)),
        ScheduleWhen::Cron { expr } => {
            let cron = Cron::parse(expr)?;
            cron.next_after(now_ms)
                .map(Some)
                .ok_or_else(|| format!("`{expr}` never occurs"))
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Evaluation {
    /// The occurrence being claimed now.
    pub due_ms: i64,
    pub outcome: ScheduleOutcome,
    /// Older occurrences folded into this one.
    pub collapsed: u32,
    /// Where the schedule goes next; `None` ends it.
    pub next_due_ms: Option<i64>,
}

/// Decides what happens to a schedule whose `next_due_ms` has arrived.
/// Returns `None` while it is not yet due.
pub fn evaluate(
    when: &ScheduleWhen,
    next_due_ms: i64,
    now_ms: i64,
    catch_up_window_ms: u64,
) -> Option<Evaluation> {
    if next_due_ms > now_ms {
        return None;
    }
    let (latest, collapsed, next_due_ms) = match when {
        ScheduleWhen::Once { .. } => (next_due_ms, 0, None),
        ScheduleWhen::Cron { expr } => {
            let Ok(cron) = Cron::parse(expr) else {
                return Some(Evaluation {
                    due_ms: next_due_ms,
                    outcome: ScheduleOutcome::Failed,
                    collapsed: 0,
                    next_due_ms: None,
                });
            };
            let mut latest = next_due_ms;
            let mut collapsed = 0u32;
            let next = loop {
                match cron.next_after(latest) {
                    Some(next) if next <= now_ms && collapsed < MAX_COLLAPSED => {
                        latest = next;
                        collapsed += 1;
                    }
                    Some(next) if next <= now_ms => {
                        // Count is saturated, but catch-up still uses the newest occurrence.
                        latest = cron.latest_at(now_ms).unwrap_or(latest);
                        break cron.next_after(now_ms);
                    }
                    other => break other,
                }
            };
            (latest, collapsed, next)
        }
    };
    let lateness = now_ms - latest;
    let outcome = if lateness <= ON_TIME_SLACK_MS {
        ScheduleOutcome::OnTime
    } else if lateness <= i64::try_from(catch_up_window_ms).unwrap_or(i64::MAX) {
        ScheduleOutcome::Late
    } else {
        ScheduleOutcome::Missed
    };
    Some(Evaluation {
        due_ms: latest,
        outcome,
        collapsed,
        next_due_ms,
    })
}

mod local {
    #[derive(Debug)]
    pub struct Tm {
        pub year: i32,
        /// 1-12
        pub month: u32,
        pub mday: u32,
        /// 0 = Sunday
        pub wday: u32,
        pub hour: u32,
        pub minute: u32,
    }

    #[cfg(unix)]
    pub fn break_down(secs: i64) -> Option<Tm> {
        let time = libc::time_t::try_from(secs).ok()?;
        // SAFETY: `localtime_r` only writes the provided, zeroed `tm`.
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        if unsafe { libc::localtime_r(&time, &mut tm) }.is_null() {
            return None;
        }
        Some(Tm {
            year: tm.tm_year + 1900,
            month: (tm.tm_mon + 1) as u32,
            mday: tm.tm_mday as u32,
            wday: tm.tm_wday as u32,
            hour: tm.tm_hour as u32,
            minute: tm.tm_min as u32,
        })
    }

    /// Local wall time to epoch seconds; out-of-range fields roll over.
    #[cfg(unix)]
    pub fn compose(year: i32, month: u32, mday: u32, hour: u32, minute: u32) -> Option<i64> {
        // SAFETY: a zeroed `tm` is valid; mktime only normalizes it in place.
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        tm.tm_year = year - 1900;
        tm.tm_mon = month as i32 - 1;
        tm.tm_mday = mday as i32;
        tm.tm_hour = hour as i32;
        tm.tm_min = minute as i32;
        tm.tm_isdst = -1;
        let secs = unsafe { libc::mktime(&mut tm) };
        (secs != -1).then_some(secs as i64)
    }

    /// The C runtime's `struct tm`, which is the same nine ints on Windows.
    #[cfg(windows)]
    #[repr(C)]
    #[derive(Default)]
    struct CrtTm {
        tm_sec: i32,
        tm_min: i32,
        tm_hour: i32,
        tm_mday: i32,
        tm_mon: i32,
        tm_year: i32,
        tm_wday: i32,
        tm_yday: i32,
        tm_isdst: i32,
    }

    #[cfg(windows)]
    unsafe extern "C" {
        fn _localtime64_s(tm: *mut CrtTm, time: *const i64) -> i32;
        fn _mktime64(tm: *mut CrtTm) -> i64;
    }

    #[cfg(windows)]
    pub fn break_down(secs: i64) -> Option<Tm> {
        let mut tm = CrtTm::default();
        // SAFETY: both pointers are live for the call, which only writes `tm`.
        if unsafe { _localtime64_s(&mut tm, &secs) } != 0 {
            return None;
        }
        Some(Tm {
            year: tm.tm_year + 1900,
            month: (tm.tm_mon + 1) as u32,
            mday: tm.tm_mday as u32,
            wday: tm.tm_wday as u32,
            hour: tm.tm_hour as u32,
            minute: tm.tm_min as u32,
        })
    }

    /// Local wall time to epoch seconds; out-of-range fields roll over.
    #[cfg(windows)]
    pub fn compose(year: i32, month: u32, mday: u32, hour: u32, minute: u32) -> Option<i64> {
        let mut tm = CrtTm {
            tm_year: year - 1900,
            tm_mon: month as i32 - 1,
            tm_mday: mday as i32,
            tm_hour: hour as i32,
            tm_min: minute as i32,
            tm_isdst: -1,
            ..CrtTm::default()
        };
        // SAFETY: `tm` is initialized; mktime only normalizes it in place.
        let secs = unsafe { _mktime64(&mut tm) };
        (secs != -1).then_some(secs)
    }

    #[cfg(not(any(unix, windows)))]
    pub fn break_down(_secs: i64) -> Option<Tm> {
        None
    }

    #[cfg(not(any(unix, windows)))]
    pub fn compose(_: i32, _: u32, _: u32, _: u32, _: u32) -> Option<i64> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use diri_proto::DateMillis;

    const HOUR: i64 = 3_600_000;
    const MINUTE: i64 = 60_000;

    fn at(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> i64 {
        local::compose(year, month, day, hour, minute).unwrap() * 1000
    }

    fn cron(expr: &str) -> ScheduleWhen {
        ScheduleWhen::Cron { expr: expr.into() }
    }

    #[test]
    fn a_backlog_beyond_the_count_cap_still_fires_the_latest_occurrence() {
        let due = at(2026, 1, 1, 0, 0);
        let now = due + (i64::from(MAX_COLLAPSED) + 10) * MINUTE;
        let result = evaluate(&cron("* * * * *"), due, now, 60_000).unwrap();
        assert_eq!(result.due_ms, now);
        assert_eq!(result.outcome, ScheduleOutcome::OnTime);
        assert_eq!(result.collapsed, MAX_COLLAPSED);
        assert_eq!(result.next_due_ms, Some(now + MINUTE));
    }

    #[test]
    fn wildcard_day_steps_are_not_ignored() {
        assert_eq!(
            Cron::parse("0 0 */2 * *")
                .unwrap()
                .next_after(at(2026, 10, 1, 0, 0)),
            Some(at(2026, 10, 3, 0, 0))
        );
        assert_eq!(
            Cron::parse("0 0 * * */2")
                .unwrap()
                .next_after(at(2026, 10, 1, 0, 0)),
            Some(at(2026, 10, 3, 0, 0))
        );
    }

    #[test]
    fn huge_cron_steps_do_not_overflow() {
        assert!(Cron::parse("1/4294967295 * * * *").is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn dst_fold_and_gap_in_an_isolated_timezone() {
        if std::env::var_os("DIRI_CRON_DST_CHILD").is_none() {
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "schedule::tests::dst_fold_and_gap_in_an_isolated_timezone",
                ])
                .env("TZ", "America/New_York")
                .env("DIRI_CRON_DST_CHILD", "1")
                .status()
                .unwrap();
            assert!(status.success());
            return;
        }
        // First 01:45 EDT, followed by 01:30 EST during the repeated hour.
        assert_eq!(
            Cron::parse("30 1 * * *")
                .unwrap()
                .next_after(1_793_511_900_000),
            Some(1_793_514_600_000)
        );
        // Nonexistent 02:30 on spring-forward day is skipped, not shifted to 03:30.
        assert_eq!(
            Cron::parse("30 2 * * *")
                .unwrap()
                .next_after(at(2026, 3, 8, 0, 0)),
            Some(at(2026, 3, 9, 2, 30))
        );
    }

    #[test]
    fn parses_common_expressions() {
        for expr in [
            "* * * * *",
            "0 9 * * 1-5",
            "*/15 * * * *",
            "30 8,12,18 * * mon-fri",
            "0 0 1 jan *",
            "5/10 * * * 7",
        ] {
            Cron::parse(expr).unwrap_or_else(|error| panic!("{expr}: {error}"));
        }
        for expr in [
            "",
            "* * * *",
            "60 * * * *",
            "* 24 * * *",
            "*/0 * * * *",
            "0 9 * * xyz",
        ] {
            assert!(Cron::parse(expr).is_err(), "{expr} should be rejected");
        }
    }

    #[test]
    fn weekday_nine_am_skips_the_weekend() {
        // 2026-10-02 is a Friday.
        let cron = Cron::parse("0 9 * * 1-5").unwrap();
        let friday_ten = at(2026, 10, 2, 10, 0);
        assert_eq!(cron.next_after(friday_ten), Some(at(2026, 10, 5, 9, 0)));
        let friday_eight = at(2026, 10, 2, 8, 0);
        assert_eq!(cron.next_after(friday_eight), Some(at(2026, 10, 2, 9, 0)));
    }

    #[test]
    fn next_after_is_strictly_later() {
        let cron = Cron::parse("*/15 * * * *").unwrap();
        let quarter = at(2026, 10, 2, 9, 15);
        assert_eq!(cron.next_after(quarter), Some(at(2026, 10, 2, 9, 30)));
        assert_eq!(cron.next_after(quarter - 1), Some(quarter));
    }

    #[test]
    fn restricted_day_fields_match_either() {
        // The 1st of the month OR any Monday.
        let cron = Cron::parse("0 0 1 * 1").unwrap();
        // 2026-10-01 is a Thursday; the next match after it is Monday 5th.
        assert_eq!(
            cron.next_after(at(2026, 10, 1, 0, 0)),
            Some(at(2026, 10, 5, 0, 0))
        );
    }

    #[test]
    fn leap_day_is_found() {
        let cron = Cron::parse("0 0 29 2 *").unwrap();
        assert_eq!(
            cron.next_after(at(2026, 10, 1, 0, 0)),
            Some(at(2028, 2, 29, 0, 0))
        );
    }

    #[test]
    fn not_yet_due_does_nothing() {
        let due = at(2026, 10, 2, 9, 0);
        assert_eq!(
            evaluate(&cron("0 9 * * *"), due, due - 1, 12 * HOUR as u64),
            None
        );
    }

    #[test]
    fn woken_ten_minutes_late_fires_once_late() {
        // The user's example: due at 9:00, the Mac wakes at 9:10.
        let due = at(2026, 10, 2, 9, 0);
        let evaluation =
            evaluate(&cron("0 9 * * *"), due, due + 10 * MINUTE, 12 * HOUR as u64).unwrap();
        assert_eq!(evaluation.due_ms, due);
        assert_eq!(evaluation.outcome, ScheduleOutcome::Late);
        assert_eq!(evaluation.collapsed, 0);
        assert_eq!(evaluation.next_due_ms, Some(at(2026, 10, 3, 9, 0)));
    }

    #[test]
    fn a_closed_weekend_collapses_into_one_run() {
        // Due Saturday 9:00, lid opened Monday 9:10: one late run for Monday,
        // Saturday and Sunday folded into it.
        let saturday = at(2026, 10, 3, 9, 0);
        let monday_late = at(2026, 10, 5, 9, 10);
        let evaluation =
            evaluate(&cron("0 9 * * *"), saturday, monday_late, 12 * HOUR as u64).unwrap();
        assert_eq!(evaluation.due_ms, at(2026, 10, 5, 9, 0));
        assert_eq!(evaluation.outcome, ScheduleOutcome::Late);
        assert_eq!(evaluation.collapsed, 2);
        assert_eq!(evaluation.next_due_ms, Some(at(2026, 10, 6, 9, 0)));
    }

    #[test]
    fn past_the_window_is_recorded_missed() {
        let due = at(2026, 10, 2, 9, 0);
        let evaluation =
            evaluate(&cron("0 9 * * *"), due, due + 13 * HOUR, 12 * HOUR as u64).unwrap();
        assert_eq!(evaluation.outcome, ScheduleOutcome::Missed);
        assert_eq!(evaluation.next_due_ms, Some(at(2026, 10, 3, 9, 0)));
    }

    #[test]
    fn a_zero_window_never_catches_up_but_keeps_on_time_slack() {
        let due = at(2026, 10, 2, 9, 0);
        let on_time = evaluate(&cron("0 9 * * *"), due, due + 30_000, 0).unwrap();
        assert_eq!(on_time.outcome, ScheduleOutcome::OnTime);
        let late = evaluate(&cron("0 9 * * *"), due, due + 5 * MINUTE, 0).unwrap();
        assert_eq!(late.outcome, ScheduleOutcome::Missed);
    }

    #[test]
    fn one_shot_ends_after_its_run() {
        let due = at(2026, 10, 2, 9, 0);
        let once = ScheduleWhen::Once {
            at: DateMillis(due as f64),
        };
        let evaluation = evaluate(&once, due, due + 20 * MINUTE, 12 * HOUR as u64).unwrap();
        assert_eq!(evaluation.outcome, ScheduleOutcome::Late);
        assert_eq!(evaluation.next_due_ms, None);
    }
}
