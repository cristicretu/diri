use std::{
    collections::{BTreeMap, HashSet},
    fs::File,
    io::{self, Read, Seek, SeekFrom},
    path::Path,
};

use serde_json::Value;

use super::{
    model::UsageHourAgg,
    pricing::{match_claude, match_openai},
    timestamp::parse_timestamp,
};

pub(crate) fn parse_claude(
    path: &Path,
    offset: u64,
    cutoff_hour: i64,
    hours: &mut BTreeMap<i64, UsageHourAgg>,
    details: &mut super::dashboard::ModelHours,
    seen_all: &mut HashSet<u64>,
    seen_by_hour: &mut BTreeMap<i64, Vec<u64>>,
) -> io::Result<u64> {
    let Some((data, consumed)) = read_complete_lines(path, offset)? else {
        return Ok(0);
    };

    for line in data.split(|byte| *byte == b'\n') {
        if line.is_empty() || !contains_bytes(line, b"\"usage\"") {
            continue;
        }
        let Ok(object) = serde_json::from_slice::<Value>(line) else {
            continue;
        };
        if object.get("type").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let Some(timestamp) = object.get("timestamp").and_then(Value::as_str) else {
            continue;
        };
        let Some(message) = object.get("message").and_then(Value::as_object) else {
            continue;
        };
        let Some(usage) = message.get("usage").and_then(Value::as_object) else {
            continue;
        };
        let Some(timestamp) = parse_timestamp(timestamp) else {
            continue;
        };
        let model = message.get("model").and_then(Value::as_str).unwrap_or("");
        if model == "<synthetic>" {
            continue;
        }

        let hour = timestamp / 3_600;
        if hour < cutoff_hour {
            continue;
        }

        if let (Some(id), Some(request_id)) = (
            message.get("id").and_then(Value::as_str),
            object.get("requestId").and_then(Value::as_str),
        ) {
            let hash = fnv1a(&format!("{id}:{request_id}"));
            if !seen_all.insert(hash) {
                continue;
            }
            seen_by_hour.entry(hour).or_default().push(hash);
        }

        let input = integer(usage.get("input_tokens"));
        let output = integer(usage.get("output_tokens"));
        let cache_read = integer(usage.get("cache_read_input_tokens"));
        let cache_write = integer(usage.get("cache_creation_input_tokens"));
        let (write_5m, write_1h) = usage
            .get("cache_creation")
            .and_then(Value::as_object)
            .map_or((cache_write, 0), |creation| {
                (
                    integer(creation.get("ephemeral_5m_input_tokens")),
                    integer(creation.get("ephemeral_1h_input_tokens")),
                )
            });

        let mut aggregate = UsageHourAgg {
            i: input,
            o: output,
            cr: cache_read,
            cw: cache_write,
            c: 0.0,
        };
        if let Some(pricing) = match_claude(model) {
            aggregate.c = (input as f64 * pricing.input
                + output as f64 * pricing.output
                + cache_read as f64 * pricing.cache_read()
                + write_5m as f64 * pricing.cache_write_5m()
                + write_1h as f64 * pricing.cache_write_1h())
                / 1_000_000.0;
        }
        super::dashboard::record(details, model, hour, aggregate, match_claude(model), 0);
        hours.entry(hour).or_default().merge(aggregate);
    }
    Ok(consumed)
}

/// Cumulative counters identify a re-emitted usage event without confusing it
/// with a separate request that happens to have the same token counts.
#[derive(Clone, Copy, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub(crate) struct CodexTotal {
    input_tokens: i64,
    cached_input_tokens: i64,
    output_tokens: i64,
}

pub(crate) fn parse_codex(
    path: &Path,
    offset: u64,
    cutoff_hour: i64,
    hours: &mut BTreeMap<i64, UsageHourAgg>,
    details: &mut super::dashboard::ModelHours,
    model: &mut Option<String>,
    previous_total: &mut Option<CodexTotal>,
) -> io::Result<u64> {
    let Some((data, consumed)) = read_complete_lines(path, offset)? else {
        return Ok(0);
    };

    for line in data.split(|byte| *byte == b'\n') {
        if line.is_empty() {
            continue;
        }

        if contains_bytes(line, b"\"turn_context\"") {
            if let Ok(object) = serde_json::from_slice::<Value>(line)
                && object.get("type").and_then(Value::as_str) == Some("turn_context")
                && let Some(current) = object
                    .get("payload")
                    .and_then(|payload| payload.get("model"))
                    .and_then(Value::as_str)
            {
                *model = Some(current.to_owned());
            }
            continue;
        }

        if !contains_bytes(line, b"\"token_count\"") {
            continue;
        }
        let Ok(object) = serde_json::from_slice::<Value>(line) else {
            continue;
        };
        if object.get("type").and_then(Value::as_str) != Some("event_msg") {
            continue;
        }
        let Some(timestamp) = object.get("timestamp").and_then(Value::as_str) else {
            continue;
        };
        let Some(payload) = object.get("payload") else {
            continue;
        };
        if payload.get("type").and_then(Value::as_str) != Some("token_count") {
            continue;
        }
        let Some(last) = payload
            .get("info")
            .and_then(|info| info.get("last_token_usage"))
        else {
            continue;
        };
        let Some(timestamp) = parse_timestamp(timestamp) else {
            continue;
        };
        // Update even outside retention, so a recent re-emission of old usage
        // cannot become new spend. Persist across incremental scans.
        if let Some(total) = payload
            .pointer("/info/total_token_usage")
            .filter(|total| total.is_object())
        {
            let current = CodexTotal {
                input_tokens: integer(total.get("input_tokens")),
                cached_input_tokens: integer(total.get("cached_input_tokens")),
                output_tokens: integer(total.get("output_tokens")),
            };
            if previous_total.as_ref() == Some(&current) {
                continue;
            }
            *previous_total = Some(current);
        } else {
            // Old rollouts have only per-request usage. Equal-sized requests
            // are legitimate; don't deduplicate by token counts alone.
            *previous_total = None;
        }
        let hour = timestamp / 3_600;
        if hour < cutoff_hour {
            continue;
        }

        let input = integer(last.get("input_tokens"));
        let cached = integer(last.get("cached_input_tokens")).min(input);
        let output = integer(last.get("output_tokens"));
        if input + output <= 0 {
            continue;
        }

        let mut aggregate = UsageHourAgg {
            i: input - cached,
            o: output,
            cr: cached,
            cw: 0,
            c: 0.0,
        };
        if let Some(pricing) = model.as_deref().and_then(match_openai) {
            aggregate.c = ((input - cached) as f64 * pricing.input
                + cached as f64 * pricing.cache_read()
                + output as f64 * pricing.output)
                / 1_000_000.0;
        }
        super::dashboard::record(
            details,
            model.as_deref().unwrap_or("Unknown model"),
            hour,
            aggregate,
            model.as_deref().and_then(match_openai),
            integer(last.get("reasoning_output_tokens")).min(output),
        );
        hours.entry(hour).or_default().merge(aggregate);
    }
    Ok(consumed)
}

fn read_complete_lines(path: &Path, offset: u64) -> io::Result<Option<(Vec<u8>, u64)>> {
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(offset))?;
    let mut data = Vec::new();
    file.read_to_end(&mut data)?;
    let Some(last_newline) = data.iter().rposition(|byte| *byte == b'\n') else {
        return Ok(None);
    };
    data.truncate(last_newline + 1);
    let consumed = u64::try_from(data.len()).expect("transcript tails fit in u64");
    Ok(Some((data, consumed)))
}

pub(crate) fn tail_hash(path: &Path, offset: u64) -> io::Result<u64> {
    const WINDOW: u64 = 4 * 1_024;

    if offset == 0 {
        return Ok(0);
    }
    let start = offset.saturating_sub(WINDOW);
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = vec![0; usize::try_from(offset - start).expect("hash window fits usize")];
    file.read_exact(&mut bytes)?;
    Ok(fnv1a_bytes(&bytes))
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

fn integer(value: Option<&Value>) -> i64 {
    value.and_then(Value::as_i64).unwrap_or(0).max(0)
}

pub(crate) fn fnv1a(value: &str) -> u64 {
    fnv1a_bytes(value.as_bytes())
}

fn fnv1a_bytes(value: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for &byte in value {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}
