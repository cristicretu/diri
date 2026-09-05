use std::{fs, path::Path};

use serde_json::{Value, json};
use tempfile::TempDir;

use super::{
    Clock, ClockReading, ScanPaths, UsageProvider, UsageStore,
    parser::fnv1a,
    pricing::{match_claude, match_openai},
    timestamp::parse_timestamp,
};

#[derive(Clone, Copy)]
struct FixedClock(ClockReading);

impl Clock for FixedClock {
    fn read(&self) -> ClockReading {
        self.0
    }
}

struct Fixture {
    _temp: TempDir,
    claude: std::path::PathBuf,
    codex: std::path::PathBuf,
    cache: std::path::PathBuf,
    paths: ScanPaths,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let claude = temp.path().join(".claude/projects/project");
        let codex = temp.path().join(".codex/sessions/2026/07/22");
        let cache = temp
            .path()
            .join("Library/Application Support/diri/usage-cache.json");
        fs::create_dir_all(&claude).unwrap();
        fs::create_dir_all(&codex).unwrap();
        let paths = ScanPaths {
            roots: vec![
                (temp.path().join(".claude/projects"), UsageProvider::Claude),
                (temp.path().join(".codex/sessions"), UsageProvider::Codex),
            ],
            cache_file: cache.clone(),
        };
        Self {
            _temp: temp,
            claude,
            codex,
            cache,
            paths,
        }
    }

    fn store(&self, now: &str, today: &str, month: &str) -> UsageStore<FixedClock> {
        UsageStore::with_paths_and_clock(
            self.paths.clone(),
            FixedClock(ClockReading {
                unix_seconds: timestamp(now),
                today_started_at: timestamp(today),
                month_started_at: timestamp(month),
            }),
        )
    }
}

#[test]
fn aggregates_costs_dedupes_claude_and_preserves_provider_totals() {
    let fixture = Fixture::new();
    let claude_message = claude_line(
        "2026-07-22T10:12:00.000Z",
        "claude-sonnet-5-20260701",
        "message-1",
        "request-1",
        1_000,
        200,
        300,
        400,
        Some((250, 150)),
    );
    write_lines(
        &fixture.claude.join("one.jsonl"),
        std::slice::from_ref(&claude_message),
    );
    write_lines(
        &fixture.claude.join("resumed.jsonl"),
        std::slice::from_ref(&claude_message),
    );

    let codex_lines = [
        json!({"timestamp":"2026-07-22T11:00:00Z","type":"turn_context","payload":{"model":"gpt-5.4-2026-01-01"}}),
        json!({"timestamp":"2026-07-22T11:10:00Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":1000,"cached_input_tokens":400,"output_tokens":100}}}}),
    ];
    write_lines(&fixture.codex.join("rollout.jsonl"), &codex_lines);

    let mut store = fixture.store(
        "2026-07-22T12:00:00Z",
        "2026-07-22T00:00:00Z",
        "2026-07-01T00:00:00Z",
    );
    let snapshot = store.refresh();

    assert_eq!(snapshot.claude.today.input_tokens, 1_000);
    assert_eq!(snapshot.claude.today.output_tokens, 200);
    assert_eq!(snapshot.claude.today.cache_read_tokens, 300);
    assert_eq!(snapshot.claude.today.cache_write_tokens, 400);
    assert_close(snapshot.claude.today.cost, 0.007_927_5);
    assert_eq!(snapshot.claude.month, snapshot.claude.today);
    assert_eq!(snapshot.claude.session, snapshot.claude.today);

    assert_eq!(snapshot.codex.today.input_tokens, 600);
    assert_eq!(snapshot.codex.today.cache_read_tokens, 400);
    assert_eq!(snapshot.codex.today.output_tokens, 100);
    assert_close(snapshot.codex.today.cost, 0.003_1);
    assert_eq!(snapshot.codex.month, snapshot.codex.today);
    assert_eq!(snapshot.codex.session.total_tokens(), 0);

    assert_eq!(snapshot.today().total_tokens(), 3_000);
    assert_close(snapshot.today().cost, 0.011_027_5);
    assert_close(snapshot.session_cost.unwrap(), 0.007_927_5);
    assert_eq!(
        snapshot.session_started_at,
        Some(timestamp("2026-07-22T10:00:00Z"))
    );
    assert_eq!(
        snapshot.session_ends_at,
        Some(timestamp("2026-07-22T15:00:00Z"))
    );
    assert_eq!(snapshot.session_remaining_seconds, Some(3 * 3_600));

    let cache: Value = serde_json::from_slice(&fs::read(&fixture.cache).unwrap()).unwrap();
    assert_eq!(cache["version"], super::cache::CACHE_VERSION);
    assert_eq!(
        cache["seen"]
            .as_object()
            .unwrap()
            .values()
            .next()
            .unwrap()
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn refresh_reads_only_appended_complete_bytes_and_retains_codex_model() {
    let fixture = Fixture::new();
    let rollout = fixture.codex.join("rollout.jsonl");
    let initial = [
        json!({"timestamp":"2026-07-22T09:00:00Z","type":"turn_context","payload":{"model":"gpt-5.4"}}),
        json!({"timestamp":"2026-07-22T09:05:00Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":100,"cached_input_tokens":20,"output_tokens":10}}}}),
    ];
    write_lines(&rollout, &initial);
    let mut store = fixture.store(
        "2026-07-22T10:00:00Z",
        "2026-07-22T00:00:00Z",
        "2026-07-01T00:00:00Z",
    );

    let first = store.refresh();
    assert_eq!(store.last_stats().files_parsed, 1);
    assert_eq!(
        store.last_stats().bytes_parsed,
        fs::metadata(&rollout).unwrap().len()
    );
    assert_eq!(first.codex.today.total_tokens(), 110);

    let unchanged = store.refresh();
    assert_eq!(store.last_stats().files_unchanged, 1);
    assert_eq!(store.last_stats().files_parsed, 0);
    assert_eq!(store.last_stats().bytes_parsed, 0);
    assert_eq!(unchanged, first);

    let appended = line_bytes(
        &json!({"timestamp":"2026-07-22T09:15:00Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":200,"cached_input_tokens":0,"output_tokens":50}}}}),
    );
    append(&rollout, &appended);
    let after_append = store.refresh();
    assert_eq!(store.last_stats().files_parsed, 1);
    assert_eq!(store.last_stats().bytes_parsed, appended.len() as u64);
    assert_eq!(after_append.codex.today.input_tokens, 280);
    assert_eq!(after_append.codex.today.output_tokens, 60);
    assert_eq!(after_append.codex.today.cache_read_tokens, 20);
    assert_close(after_append.codex.today.cost, 0.001_605);

    let partial = br#"{"timestamp":"2026-07-22T09:20:00Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":9,"cached_input_tokens":0,"output_tokens":1}}}}"#;
    append(&rollout, partial);
    let before_newline = store.refresh();
    assert_eq!(store.last_stats().bytes_parsed, 0);
    assert_eq!(before_newline, after_append);
    append(&rollout, b"\n");
    let completed = store.refresh();
    assert_eq!(store.last_stats().bytes_parsed, partial.len() as u64 + 1);
    assert_eq!(completed.codex.today.input_tokens, 289);
    assert_eq!(completed.codex.today.output_tokens, 61);

    let cache: Value = serde_json::from_slice(&fs::read(&fixture.cache).unwrap()).unwrap();
    let entry = cache["files"].as_object().unwrap().values().next().unwrap();
    assert_eq!(entry["offset"], fs::metadata(&rollout).unwrap().len());
    assert_eq!(entry["model"], "gpt-5.4");
}

#[test]
fn path_refresh_touches_only_invalidated_transcripts() {
    let fixture = Fixture::new();
    for index in 0..64 {
        write_lines(
            &fixture.codex.join(format!("rollout-{index}.jsonl")),
            &[
                json!({"timestamp":"2026-07-22T09:00:00Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":1,"cached_input_tokens":0,"output_tokens":0}}}}),
            ],
        );
    }
    let changed = fixture.codex.join("rollout-17.jsonl");
    let mut store = fixture.store(
        "2026-07-22T10:00:00Z",
        "2026-07-22T00:00:00Z",
        "2026-07-01T00:00:00Z",
    );
    assert_eq!(store.refresh().codex.today.input_tokens, 64);

    append(
        &changed,
        &line_bytes(
            &json!({"timestamp":"2026-07-22T09:05:00Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":9,"cached_input_tokens":0,"output_tokens":0}}}}),
        ),
    );
    let refreshed = store.refresh_paths(std::slice::from_ref(&changed));

    assert_eq!(store.last_stats().files_discovered, 1);
    assert_eq!(store.last_stats().files_parsed, 1);
    assert_eq!(store.last_stats().files_unchanged, 0);
    assert_eq!(refreshed.codex.today.input_tokens, 73);
}

#[test]
fn refresh_replaces_totals_when_a_transcript_is_rewritten() {
    let fixture = Fixture::new();
    let rollout = fixture.codex.join("rollout.jsonl");
    let initial = [
        json!({"timestamp":"2026-07-22T09:00:00Z","type":"turn_context","payload":{"model":"gpt-5.4"}}),
        json!({"timestamp":"2026-07-22T09:05:00Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":100,"cached_input_tokens":20,"output_tokens":10}}}}),
    ];
    write_lines(&rollout, &initial);
    let mut store = fixture.store(
        "2026-07-22T10:00:00Z",
        "2026-07-22T00:00:00Z",
        "2026-07-01T00:00:00Z",
    );

    let first = store.refresh();
    assert_eq!(first.codex.today.total_tokens(), 110);

    let replacement = [
        json!({"timestamp":"2026-07-22T09:00:00Z","type":"turn_context","payload":{"model":"gpt-5.4"}}),
        json!({"timestamp":"2026-07-22T09:05:00Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":1000,"cached_input_tokens":400,"output_tokens":100}}}}),
    ];
    write_lines(&rollout, &replacement);

    let rewritten = store.refresh();
    assert_eq!(store.last_stats().files_parsed, 1);
    assert_eq!(
        store.last_stats().bytes_parsed,
        fs::metadata(&rollout).unwrap().len()
    );
    assert_eq!(rewritten.codex.today.input_tokens, 600);
    assert_eq!(rewritten.codex.today.cache_read_tokens, 400);
    assert_eq!(rewritten.codex.today.output_tokens, 100);
}

#[test]
fn refresh_rebuilds_claude_deduplication_when_a_transcript_is_rewritten() {
    let fixture = Fixture::new();
    let transcript = fixture.claude.join("session.jsonl");
    write_lines(
        &transcript,
        &[claude_line(
            "2026-07-22T09:05:00Z",
            "claude-sonnet-5",
            "message",
            "request",
            100,
            10,
            0,
            0,
            None,
        )],
    );
    let mut store = fixture.store(
        "2026-07-22T10:00:00Z",
        "2026-07-22T00:00:00Z",
        "2026-07-01T00:00:00Z",
    );

    assert_eq!(store.refresh().claude.today.total_tokens(), 110);
    write_lines(
        &transcript,
        &[claude_line(
            "2026-07-22T09:05:00Z",
            "claude-sonnet-5",
            "message",
            "request",
            1_000,
            100,
            0,
            0,
            None,
        )],
    );

    let rewritten = store.refresh();
    assert_eq!(store.last_stats().files_parsed, 1);
    assert_eq!(rewritten.claude.today.input_tokens, 1_000);
    assert_eq!(rewritten.claude.today.output_tokens, 100);
}

#[test]
fn buckets_local_day_month_and_five_hour_blocks_like_swift() {
    let fixture = Fixture::new();
    let messages = [
        claude_line(
            "2026-06-30T23:00:00Z",
            "claude-haiku-4",
            "a",
            "a",
            10,
            0,
            0,
            0,
            None,
        ),
        claude_line(
            "2026-07-21T20:00:00Z",
            "claude-haiku-4",
            "b",
            "b",
            20,
            0,
            0,
            0,
            None,
        ),
        claude_line(
            "2026-07-22T00:00:00Z",
            "claude-haiku-4",
            "c",
            "c",
            30,
            0,
            0,
            0,
            None,
        ),
        claude_line(
            "2026-07-22T01:00:00Z",
            "claude-haiku-4",
            "d",
            "d",
            40,
            0,
            0,
            0,
            None,
        ),
        claude_line(
            "2026-07-22T04:00:00Z",
            "claude-haiku-4",
            "e",
            "e",
            50,
            0,
            0,
            0,
            None,
        ),
    ];
    write_lines(&fixture.claude.join("blocks.jsonl"), &messages);
    let mut store = fixture.store(
        "2026-07-22T04:30:00Z",
        "2026-07-22T00:00:00Z",
        "2026-07-01T00:00:00Z",
    );
    let snapshot = store.refresh();

    assert_eq!(snapshot.claude.today.input_tokens, 120);
    assert_eq!(snapshot.claude.month.input_tokens, 140);
    assert_eq!(snapshot.claude.session.input_tokens, 90);
    assert_eq!(
        snapshot.session_started_at,
        Some(timestamp("2026-07-22T01:00:00Z"))
    );
    assert_eq!(
        snapshot.session_ends_at,
        Some(timestamp("2026-07-22T06:00:00Z"))
    );
    assert_eq!(snapshot.session_remaining_seconds, Some(5_400));

    let mut after_window = fixture.store(
        "2026-07-22T06:00:00Z",
        "2026-07-22T00:00:00Z",
        "2026-07-01T00:00:00Z",
    );
    let expired = after_window.refresh();
    assert_eq!(expired.session_cost, None);
    assert_eq!(expired.claude.session.total_tokens(), 0);
}

#[test]
fn ordered_pricing_table_and_fnv_hash_match_the_swift_constants() {
    assert_eq!(super::PRICING_ENTRY_COUNT, 15);
    for (model, input, output) in [
        ("claude-fable", 10.0, 50.0),
        ("claude-mythos", 10.0, 50.0),
        ("claude-opus-4-1", 15.0, 75.0),
        ("claude-opus-4-20250514", 15.0, 75.0),
        ("claude-opus-4-5", 5.0, 25.0),
        ("claude-sonnet-5", 3.0, 15.0),
        ("claude-haiku-4", 1.0, 5.0),
        ("claude-3-5-haiku", 0.8, 4.0),
        ("claude-haiku-3", 0.25, 1.25),
    ] {
        let pricing = match_claude(model).unwrap();
        assert_eq!((pricing.input, pricing.output), (input, output));
    }
    for (model, input, output) in [
        ("gpt-5.4-mini", 0.75, 4.5),
        ("gpt-5.4", 2.5, 15.0),
        ("gpt-5.5", 5.0, 30.0),
        ("codex-mini", 1.5, 6.0),
        ("gpt-5.3-codex", 1.75, 14.0),
        ("gpt-5-mini", 0.25, 2.0),
        ("gpt-5-nano", 0.05, 0.4),
        ("gpt-5.2", 1.25, 10.0),
    ] {
        let pricing = match_openai(model).unwrap();
        assert_eq!((pricing.input, pricing.output), (input, output));
    }
    assert_eq!(fnv1a("hello"), 0xa430_d846_80aa_bd0b);
}

#[test]
fn timestamps_accept_fractional_plain_and_offset_forms() {
    assert_eq!(
        parse_timestamp("2026-07-22T12:34:56.123456Z"),
        parse_timestamp("2026-07-22T12:34:56Z")
    );
    assert_eq!(
        parse_timestamp("2026-07-22T15:34:56+03:00"),
        parse_timestamp("2026-07-22T12:34:56Z")
    );
    assert_eq!(parse_timestamp("2026-02-30T12:00:00Z"), None);
}

fn timestamp(value: &str) -> i64 {
    parse_timestamp(value).unwrap()
}

#[allow(clippy::too_many_arguments)]
fn claude_line(
    timestamp: &str,
    model: &str,
    id: &str,
    request_id: &str,
    input: i64,
    output: i64,
    cache_read: i64,
    cache_write: i64,
    cache_creation: Option<(i64, i64)>,
) -> Value {
    let mut usage = json!({
        "input_tokens": input,
        "output_tokens": output,
        "cache_read_input_tokens": cache_read,
        "cache_creation_input_tokens": cache_write,
    });
    if let Some((five_minutes, one_hour)) = cache_creation {
        usage["cache_creation"] = json!({
            "ephemeral_5m_input_tokens": five_minutes,
            "ephemeral_1h_input_tokens": one_hour,
        });
    }
    json!({
        "type": "assistant",
        "timestamp": timestamp,
        "requestId": request_id,
        "message": {"id": id, "model": model, "usage": usage},
    })
}

fn write_lines(path: &Path, lines: &[Value]) {
    let bytes = lines.iter().flat_map(line_bytes).collect::<Vec<_>>();
    fs::write(path, bytes).unwrap();
}

fn line_bytes(value: &Value) -> Vec<u8> {
    let mut bytes = serde_json::to_vec(value).unwrap();
    bytes.push(b'\n');
    bytes
}

fn append(path: &Path, bytes: &[u8]) {
    use std::io::Write;

    let mut file = fs::OpenOptions::new().append(true).open(path).unwrap();
    file.write_all(bytes).unwrap();
}

fn assert_close(actual: f64, expected: f64) {
    assert!((actual - expected).abs() < 1e-12, "{actual} != {expected}");
}

#[test]
fn dashboard_preserves_deduplication_model_changes_and_cached_history() {
    let fixture = Fixture::new();
    let message = claude_line(
        "2026-07-22T10:00:00Z",
        "claude-sonnet-5",
        "m",
        "r",
        100,
        20,
        1_000,
        50,
        None,
    );
    write_lines(
        &fixture.claude.join("a.jsonl"),
        std::slice::from_ref(&message),
    );
    write_lines(&fixture.claude.join("b.jsonl"), &[message]);
    let codex = fixture.codex.join("rollout.jsonl");
    write_lines(
        &codex,
        &[
            json!({"type":"turn_context","payload":{"model":"gpt-5.4"}}),
            json!({"timestamp":"2026-07-22T10:00:00Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":100,"cached_input_tokens":50,"output_tokens":20,"reasoning_output_tokens":8}}}}),
            json!({"type":"turn_context","payload":{"model":"unrecognized-model"}}),
            json!({"timestamp":"2026-07-22T11:00:00Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":200,"output_tokens":30}}}}),
        ],
    );
    let mut store = fixture.store(
        "2026-07-22T12:00:00Z",
        "2026-07-22T00:00:00Z",
        "2026-07-01T00:00:00Z",
    );
    let snapshot = store.refresh();
    let report = snapshot.history.report(snapshot.updated_at, 30);
    assert_eq!(report.total.totals(), snapshot.today());
    assert_eq!(report.models.len(), 3);
    assert_eq!(report.active_days, 1);
    assert_eq!(report.total.reasoning, 8); // Already a subset of output.
    assert_eq!(report.total.priced_tokens, 1_290);
    assert_eq!(
        report.total.totals().total_tokens() - report.total.priced_tokens,
        230
    );
    assert_close(report.total.read_savings, 0.002_812_5);
    assert_eq!(report.days.last().unwrap().total(), report.total);
    let warm = store.refresh();
    assert_eq!(warm.history, snapshot.history);
    assert_eq!(store.last_stats().bytes_parsed, 0);
    let mut restarted = fixture.store(
        "2026-07-22T12:00:00Z",
        "2026-07-22T00:00:00Z",
        "2026-07-01T00:00:00Z",
    );
    assert_eq!(restarted.refresh().history, snapshot.history);
    assert_eq!(restarted.last_stats().bytes_parsed, 0);
    append(
        &codex,
        &line_bytes(
            &json!({"timestamp":"2026-07-22T12:00:00Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":10,"output_tokens":1}}}}),
        ),
    );
    let appended = restarted.refresh_paths(&[codex]);
    let report = appended.history.report(appended.updated_at, 7);
    assert_eq!(report.total.totals(), appended.today());
    assert_eq!(report.total.totals().total_tokens(), 1_531);
}

#[test]
fn dashboard_ranges_zero_fill_and_rebuild_old_caches_for_ninety_days() {
    let fixture = Fixture::new();
    write_lines(
        &fixture.claude.join("history.jsonl"),
        &[
            claude_line(
                "2026-04-24T00:00:00Z",
                "claude-sonnet",
                "a",
                "a",
                10,
                0,
                0,
                0,
                None,
            ),
            claude_line(
                "2026-06-23T00:00:00Z",
                "claude-sonnet",
                "b",
                "b",
                20,
                0,
                0,
                0,
                None,
            ),
            claude_line(
                "2026-07-16T00:00:00Z",
                "claude-sonnet",
                "c",
                "c",
                30,
                0,
                0,
                0,
                None,
            ),
            claude_line(
                "2026-07-23T00:00:00Z",
                "claude-sonnet",
                "future",
                "future",
                99,
                0,
                0,
                0,
                None,
            ),
        ],
    );
    let mut store = fixture.store(
        "2026-07-22T12:00:00Z",
        "2026-07-22T00:00:00Z",
        "2026-07-01T00:00:00Z",
    );
    let first = store.refresh();
    for (days, tokens, active) in [(7, 30, 1), (30, 50, 2), (90, 60, 3)] {
        let report = first.history.report(first.updated_at, days);
        assert_eq!(report.days.len(), days);
        assert_eq!(report.total.totals().total_tokens(), tokens);
        assert_eq!(report.active_days, active);
        assert_eq!(
            report.days.last().unwrap().total().totals().total_tokens(),
            0
        );
    }
    let mut cache: Value = serde_json::from_slice(&fs::read(&fixture.cache).unwrap()).unwrap();
    cache["version"] = json!(2);
    for entry in cache["files"].as_object_mut().unwrap().values_mut() {
        entry.as_object_mut().unwrap().remove("details");
    }
    fs::write(&fixture.cache, serde_json::to_vec(&cache).unwrap()).unwrap();
    let mut restarted = fixture.store(
        "2026-07-22T12:00:00Z",
        "2026-07-22T00:00:00Z",
        "2026-07-01T00:00:00Z",
    );
    assert_eq!(restarted.refresh().history, first.history);
    assert!(restarted.last_stats().bytes_parsed > 0);
    assert_eq!(
        super::dashboard::date_label(timestamp("2024-02-29T00:00:00Z") / 86400),
        "2024-02-29"
    );
}

#[test]
fn codex_repeated_usage_is_not_billed_again_after_cache_reload() {
    let fixture = Fixture::new();
    let path = fixture.codex.join("rollout.jsonl");
    let event = |total: i64| json!({"timestamp":"2026-07-22T09:05:00Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":total,"cached_input_tokens":0,"output_tokens":total / 10},"last_token_usage":{"input_tokens":100,"cached_input_tokens":0,"output_tokens":10}}}});
    write_lines(
        &path,
        &[
            json!({"type":"turn_context","payload":{"model":"gpt-5.4"}}),
            event(100),
        ],
    );
    let make_store = || {
        fixture.store(
            "2026-07-22T10:00:00Z",
            "2026-07-22T00:00:00Z",
            "2026-07-01T00:00:00Z",
        )
    };
    let first = make_store().refresh();
    append(&path, &line_bytes(&event(100)));
    let mut reloaded = make_store();
    let repeated = reloaded.refresh();
    assert_eq!(
        repeated.codex.today, first.codex.today,
        "re-emitted cumulative counters are not a second request"
    );
    append(&path, &line_bytes(&event(200)));
    let second_request = reloaded.refresh();
    assert_eq!(
        second_request.codex.today.total_tokens(),
        220,
        "equal-sized real requests must both count when cumulative usage advances"
    );
}
