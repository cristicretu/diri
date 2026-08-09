use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use diri_proto::{
    ProviderKind, UsageEvent, UsageQueryParams, UsageQueryResult, UsageSource, UsageTotals,
    UsageValueKind,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::accounts::{AccountStore, now_seconds};
use crate::config::hex_encode;
use crate::error::{NodeError, NodeResult};

pub struct UsageLedger {
    node_id: String,
    connection: Connection,
}

impl UsageLedger {
    pub fn open(path: &Path, node_id: impl Into<String>) -> NodeResult<Self> {
        let connection = Connection::open(path)?;
        connection.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             PRAGMA foreign_keys=ON;
             CREATE TABLE IF NOT EXISTS usage_events (
                id TEXT PRIMARY KEY NOT NULL,
                occurred_at INTEGER NOT NULL,
                node_id TEXT NOT NULL,
                provider TEXT NOT NULL,
                profile_id TEXT,
                session_id TEXT,
                model TEXT,
                input_tokens INTEGER NOT NULL,
                output_tokens INTEGER NOT NULL,
                cache_read_tokens INTEGER NOT NULL,
                cache_write_tokens INTEGER NOT NULL,
                estimated_usd REAL,
                billed_usd REAL,
                value_kind TEXT NOT NULL,
                source TEXT NOT NULL,
                created_at INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS usage_events_time ON usage_events(occurred_at);
             CREATE INDEX IF NOT EXISTS usage_events_profile_time
                ON usage_events(profile_id, occurred_at);
             CREATE INDEX IF NOT EXISTS usage_events_session_time
                ON usage_events(session_id, occurred_at);
             CREATE TABLE IF NOT EXISTS usage_scan_files (
                path TEXT PRIMARY KEY NOT NULL,
                size INTEGER NOT NULL,
                modified_ns INTEGER NOT NULL,
                scanned_at INTEGER NOT NULL
             );",
        )?;
        Ok(Self {
            node_id: node_id.into(),
            connection,
        })
    }

    pub fn record(&mut self, event: &UsageEvent) -> NodeResult<bool> {
        validate_event(event)?;
        let changed = self.connection.execute(
            "INSERT OR IGNORE INTO usage_events (
                id, occurred_at, node_id, provider, profile_id, session_id, model,
                input_tokens, output_tokens, cache_read_tokens, cache_write_tokens,
                estimated_usd, billed_usd, value_kind, source, created_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
            params![
                event.id,
                event.occurred_at,
                self.node_id,
                event.provider.as_str(),
                event.profile_id,
                event.session_id,
                event.model,
                event.input_tokens,
                event.output_tokens,
                event.cache_read_tokens,
                event.cache_write_tokens,
                event.estimated_usd,
                event.billed_usd,
                value_kind(event.value_kind),
                usage_source(event.source),
                now_seconds(),
            ],
        )?;
        Ok(changed != 0)
    }

    pub fn query(&self, query: &UsageQueryParams) -> NodeResult<UsageQueryResult> {
        let provider = query.provider.map(ProviderKind::as_str);
        let totals = self.aggregate(query, None)?;
        let mut by_provider = BTreeMap::new();
        for kind in [ProviderKind::Claude, ProviderKind::Codex] {
            let totals = self.aggregate(query, Some(kind))?;
            if totals.events != 0 {
                by_provider.insert(kind, totals);
            }
        }
        let authoritative_billing_available = self
            .connection
            .query_row(
                "SELECT 1 FROM usage_events
                 WHERE billed_usd IS NOT NULL
                   AND (?1 IS NULL OR occurred_at >= ?1)
                   AND (?2 IS NULL OR occurred_at < ?2)
                   AND (?3 IS NULL OR provider = ?3)
                   AND (?4 IS NULL OR profile_id = ?4)
                   AND (?5 IS NULL OR session_id = ?5)
                 LIMIT 1",
                params![
                    query.from,
                    query.to,
                    provider,
                    query.profile_id,
                    query.session_id
                ],
                |_| Ok(true),
            )
            .optional()?
            .unwrap_or(false);
        Ok(UsageQueryResult {
            node_id: self.node_id.clone(),
            totals,
            by_provider,
            authoritative_billing_available,
            updated_at: now_seconds(),
        })
    }

    pub fn import_transcripts(&mut self, accounts: &AccountStore) -> NodeResult<usize> {
        let mut imported = 0;
        for profile in accounts.profiles() {
            let root = match profile.provider {
                ProviderKind::Claude => accounts.config_home(profile).join("projects"),
                ProviderKind::Codex => accounts.config_home(profile).join("sessions"),
            };
            for path in jsonl_files(&root) {
                if self.file_is_current(&path)? {
                    continue;
                }
                for event in parse_transcript(&path, profile.provider, Some(&profile.id))? {
                    imported += usize::from(self.record(&event)?);
                }
                self.mark_file_current(&path)?;
            }
        }
        Ok(imported)
    }

    fn aggregate(
        &self,
        query: &UsageQueryParams,
        provider_override: Option<ProviderKind>,
    ) -> NodeResult<UsageTotals> {
        let provider = provider_override
            .or(query.provider)
            .map(ProviderKind::as_str);
        self.connection
            .query_row(
                "SELECT
                    COALESCE(SUM(input_tokens), 0),
                    COALESCE(SUM(output_tokens), 0),
                    COALESCE(SUM(cache_read_tokens), 0),
                    COALESCE(SUM(cache_write_tokens), 0),
                    COALESCE(SUM(estimated_usd), 0.0),
                    COALESCE(SUM(billed_usd), 0.0),
                    COUNT(*)
                 FROM usage_events
                 WHERE (?1 IS NULL OR occurred_at >= ?1)
                   AND (?2 IS NULL OR occurred_at < ?2)
                   AND (?3 IS NULL OR provider = ?3)
                   AND (?4 IS NULL OR profile_id = ?4)
                   AND (?5 IS NULL OR session_id = ?5)",
                params![
                    query.from,
                    query.to,
                    provider,
                    query.profile_id,
                    query.session_id
                ],
                |row| {
                    Ok(UsageTotals {
                        input_tokens: row.get(0)?,
                        output_tokens: row.get(1)?,
                        cache_read_tokens: row.get(2)?,
                        cache_write_tokens: row.get(3)?,
                        estimated_usd: row.get(4)?,
                        billed_usd: row.get(5)?,
                        events: row.get(6)?,
                    })
                },
            )
            .map_err(Into::into)
    }

    fn file_is_current(&self, path: &Path) -> NodeResult<bool> {
        let (size, modified_ns) = file_revision(path)?;
        let size = i64::try_from(size).unwrap_or(i64::MAX);
        let modified_ns = i64::try_from(modified_ns).unwrap_or(i64::MAX);
        let current = self
            .connection
            .query_row(
                "SELECT size, modified_ns FROM usage_scan_files WHERE path = ?1",
                [path.to_string_lossy().as_ref()],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()?;
        Ok(current == Some((size, modified_ns)))
    }

    fn mark_file_current(&mut self, path: &Path) -> NodeResult<()> {
        let (size, modified_ns) = file_revision(path)?;
        let size = i64::try_from(size).unwrap_or(i64::MAX);
        let modified_ns = i64::try_from(modified_ns).unwrap_or(i64::MAX);
        self.connection.execute(
            "INSERT INTO usage_scan_files(path, size, modified_ns, scanned_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(path) DO UPDATE SET
               size = excluded.size,
               modified_ns = excluded.modified_ns,
               scanned_at = excluded.scanned_at",
            params![path.to_string_lossy(), size, modified_ns, now_seconds()],
        )?;
        Ok(())
    }
}

fn validate_event(event: &UsageEvent) -> NodeResult<()> {
    if event.id.trim().is_empty() || event.id.len() > 256 {
        return Err(NodeError::BadRequest("usage event id is invalid".into()));
    }
    if [
        event.input_tokens,
        event.output_tokens,
        event.cache_read_tokens,
        event.cache_write_tokens,
    ]
    .iter()
    .any(|value| *value < 0)
    {
        return Err(NodeError::BadRequest(
            "usage token counts cannot be negative".into(),
        ));
    }
    if event
        .estimated_usd
        .into_iter()
        .chain(event.billed_usd)
        .any(|value| !value.is_finite() || value < 0.0)
    {
        return Err(NodeError::BadRequest(
            "usage costs must be finite and non-negative".into(),
        ));
    }
    Ok(())
}

fn value_kind(kind: UsageValueKind) -> &'static str {
    match kind {
        UsageValueKind::SubscriptionQuota => "subscription_quota",
        UsageValueKind::EstimatedApiEquivalent => "estimated_api_equivalent",
        UsageValueKind::AuthoritativeBilled => "authoritative_billed",
    }
}

fn usage_source(source: UsageSource) -> &'static str {
    match source {
        UsageSource::AppServer => "app_server",
        UsageSource::Otel => "otel",
        UsageSource::Transcript => "transcript",
        UsageSource::Manual => "manual",
    }
}

fn jsonl_files(root: &Path) -> Vec<PathBuf> {
    let mut result = Vec::new();
    walk_jsonl(root, &mut result);
    result.sort();
    result
}

fn walk_jsonl(root: &Path, result: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            walk_jsonl(&path, result);
        } else if file_type.is_file() && path.extension().is_some_and(|ext| ext == "jsonl") {
            result.push(path);
        }
    }
}

fn parse_transcript(
    path: &Path,
    provider: ProviderKind,
    profile_id: Option<&str>,
) -> NodeResult<Vec<UsageEvent>> {
    let file = File::open(path)?;
    let mut result = Vec::new();
    let mut offset = 0_u64;
    let mut codex_model = None;
    for line in BufReader::new(file).lines() {
        let line = line?;
        let line_offset = offset;
        offset = offset.saturating_add(u64::try_from(line.len()).unwrap_or(u64::MAX) + 1);
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if provider == ProviderKind::Codex
            && value.pointer("/payload/type").and_then(Value::as_str) == Some("session_meta")
        {
            codex_model = value
                .pointer("/payload/model")
                .and_then(Value::as_str)
                .map(str::to_owned);
        }
        let parsed = match provider {
            ProviderKind::Claude => parse_claude_event(&value),
            ProviderKind::Codex => parse_codex_event(&value, codex_model.as_deref()),
        };
        let Some(mut event) = parsed else {
            continue;
        };
        event.id = transcript_event_id(path, line_offset, &value);
        event.profile_id = profile_id.map(str::to_owned);
        result.push(event);
    }
    Ok(result)
}

fn parse_claude_event(value: &Value) -> Option<UsageEvent> {
    let message = value.get("message")?;
    let usage = message.get("usage")?;
    let occurred_at = event_timestamp(value)?;
    let model = message.get("model").and_then(Value::as_str);
    let input = integer(usage.get("input_tokens"));
    let output = integer(usage.get("output_tokens"));
    let cache_read = integer(usage.get("cache_read_input_tokens"));
    let cache_write = integer(usage.get("cache_creation_input_tokens"));
    let cache_write_5m = integer(usage.pointer("/cache_creation/ephemeral_5m_input_tokens"));
    let cache_write_1h = integer(usage.pointer("/cache_creation/ephemeral_1h_input_tokens"));
    Some(UsageEvent {
        id: String::new(),
        occurred_at,
        provider: ProviderKind::Claude,
        profile_id: None,
        session_id: value
            .get("sessionId")
            .and_then(Value::as_str)
            .map(str::to_owned),
        model: model.map(str::to_owned),
        input_tokens: input,
        output_tokens: output,
        cache_read_tokens: cache_read,
        cache_write_tokens: cache_write,
        estimated_usd: cost(value).or_else(|| {
            diri_usage::claude_estimate(
                model?,
                input,
                output,
                cache_read,
                cache_write_5m,
                cache_write_1h,
            )
        }),
        billed_usd: None,
        value_kind: UsageValueKind::EstimatedApiEquivalent,
        source: UsageSource::Transcript,
    })
}

fn parse_codex_event(value: &Value, model: Option<&str>) -> Option<UsageEvent> {
    if value.pointer("/payload/type").and_then(Value::as_str) != Some("token_count") {
        return None;
    }
    let usage = value.pointer("/payload/info/last_token_usage")?;
    let occurred_at = event_timestamp(value)?;
    let input = integer(usage.get("input_tokens"));
    let output = integer(usage.get("output_tokens"));
    let cache_read = integer(usage.get("cached_input_tokens"));
    Some(UsageEvent {
        id: String::new(),
        occurred_at,
        provider: ProviderKind::Codex,
        profile_id: None,
        session_id: value
            .pointer("/payload/thread_id")
            .and_then(Value::as_str)
            .map(str::to_owned),
        model: model.map(str::to_owned),
        input_tokens: input,
        output_tokens: output,
        cache_read_tokens: cache_read,
        cache_write_tokens: 0,
        estimated_usd: cost(value)
            .or_else(|| diri_usage::openai_estimate(model?, input, output, cache_read)),
        billed_usd: None,
        value_kind: UsageValueKind::EstimatedApiEquivalent,
        source: UsageSource::Transcript,
    })
}

fn transcript_event_id(path: &Path, offset: u64, value: &Value) -> String {
    let provider_id = value
        .pointer("/message/id")
        .or_else(|| value.pointer("/payload/id"))
        .and_then(Value::as_str);
    let mut digest = Sha256::new();
    digest.update(path.to_string_lossy().as_bytes());
    digest.update(offset.to_le_bytes());
    if let Some(provider_id) = provider_id {
        digest.update(provider_id.as_bytes());
    }
    format!("transcript:{}", hex_encode(&digest.finalize()))
}

fn event_timestamp(value: &Value) -> Option<i64> {
    if let Some(timestamp) = value.get("timestamp").and_then(Value::as_i64) {
        return Some(timestamp);
    }
    let timestamp = value.get("timestamp").and_then(Value::as_str)?;
    parse_rfc3339_seconds(timestamp)
}

fn parse_rfc3339_seconds(value: &str) -> Option<i64> {
    if value.len() < 20 || &value[4..5] != "-" || &value[7..8] != "-" || &value[10..11] != "T" {
        return None;
    }
    let year = value[0..4].parse::<i64>().ok()?;
    let month = value[5..7].parse::<i64>().ok()?;
    let day = value[8..10].parse::<i64>().ok()?;
    let hour = value[11..13].parse::<i64>().ok()?;
    let minute = value[14..16].parse::<i64>().ok()?;
    let second = value[17..19].parse::<i64>().ok()?;
    let days = days_from_civil(year, month, day);
    let mut result = days * 86_400 + hour * 3_600 + minute * 60 + second;
    let suffix = &value[19..];
    if let Some(position) = suffix.rfind(['+', '-']) {
        let sign = if suffix.as_bytes()[position] == b'+' {
            1
        } else {
            -1
        };
        let timezone = &suffix[position + 1..];
        if timezone.len() >= 5 {
            let hours = timezone[0..2].parse::<i64>().ok()?;
            let minutes = timezone[3..5].parse::<i64>().ok()?;
            result -= sign * (hours * 3_600 + minutes * 60);
        }
    }
    Some(result)
}

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let shifted_month = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * shifted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn integer(value: Option<&Value>) -> i64 {
    value.and_then(Value::as_i64).unwrap_or_default().max(0)
}

fn cost(value: &Value) -> Option<f64> {
    value
        .get("costUSD")
        .or_else(|| value.get("cost_usd"))
        .or_else(|| value.pointer("/message/costUSD"))
        .and_then(Value::as_f64)
        .filter(|cost| cost.is_finite() && *cost >= 0.0)
}

fn file_revision(path: &Path) -> NodeResult<(u64, u64)> {
    let metadata = fs::metadata(path)?;
    let modified_ns = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |duration| {
            u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
        });
    Ok((metadata.len(), modified_ns))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ledger_deduplicates_and_preserves_billing_semantics() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let mut ledger =
            UsageLedger::open(&directory.path().join("usage.db"), "forge").expect("ledger");
        let event = UsageEvent {
            id: "request-1".into(),
            occurred_at: 1_800_000_000,
            provider: ProviderKind::Codex,
            profile_id: Some("work".into()),
            session_id: Some("thread-1".into()),
            model: Some("codex".into()),
            input_tokens: 100,
            output_tokens: 20,
            cache_read_tokens: 10,
            cache_write_tokens: 0,
            estimated_usd: Some(0.12),
            billed_usd: None,
            value_kind: UsageValueKind::EstimatedApiEquivalent,
            source: UsageSource::AppServer,
        };
        assert!(ledger.record(&event).expect("first insert"));
        assert!(!ledger.record(&event).expect("duplicate"));
        let result = ledger
            .query(&UsageQueryParams {
                profile_id: Some("work".into()),
                ..UsageQueryParams::default()
            })
            .expect("query");
        assert_eq!(result.totals.events, 1);
        assert_eq!(result.totals.input_tokens, 100);
        assert_eq!(result.totals.estimated_usd, 0.12);
        assert!(!result.authoritative_billing_available);
    }

    #[test]
    fn parses_utc_and_offset_timestamps() {
        assert_eq!(parse_rfc3339_seconds("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_rfc3339_seconds("1970-01-01T02:30:00+02:30"), Some(0));
    }

    #[test]
    fn transcript_fallback_computes_labeled_api_equivalent_cost() {
        let event = parse_claude_event(&serde_json::json!({
            "timestamp": "2026-08-02T12:00:00Z",
            "message": {
                "model": "claude-sonnet",
                "usage": {
                    "input_tokens": 1_000_000,
                    "output_tokens": 0,
                    "cache_read_input_tokens": 0,
                    "cache_creation_input_tokens": 0
                }
            }
        }))
        .expect("usage event");
        assert_eq!(event.estimated_usd, Some(3.0));
        assert_eq!(event.value_kind, UsageValueKind::EstimatedApiEquivalent);
        assert_eq!(event.source, UsageSource::Transcript);
    }
}
