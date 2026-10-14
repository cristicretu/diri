use super::*;

#[test]
fn astra_usage_has_a_standard_cost_estimate() {
    // 60 uncached input + 40 cached input + 20 output tokens.
    let cost = openai_estimate("gpt-6-astra", 60, 20, 40).expect("Astra usage must be priced");
    assert!((cost - 0.001_64).abs() < 1e-12);
    assert_eq!(match_openai("gpt-6-unknown"), None);
}

#[test]
fn sol_uses_its_published_cache_rate() {
    let pricing = match_openai("gpt-6.1-sol").expect("Sol must be priced");
    assert_eq!((pricing.input, pricing.output), (2.0, 10.0));
    // 5% of input, not the 10% default.
    assert!((pricing.cache_read() - 0.1).abs() < 1e-12);
    let cost = openai_estimate("gpt-6.1-sol", 60, 20, 40).expect("Sol usage must be priced");
    assert!((cost - 0.000_324).abs() < 1e-12);
    assert_eq!(match_openai("gpt-6.1-sol-fast"), None);
}

#[test]
fn estimates_keep_cache_rates_distinct() {
    let price = claude_estimate("claude-sonnet", 1_000_000, 0, 0, 0, 0).expect("known model");
    assert_eq!(price, 3.0);
    let cached = openai_estimate("codex", 0, 0, 1_000_000).expect("known model");
    assert!((cached - 0.175).abs() < 1e-12);
}

#[test]
fn provider_spellings_resolve_to_the_first_party_id() {
    let opus = match_claude("claude-opus-4-6").unwrap();
    for spelling in [
        "claude-opus-4-6[1m]",
        "Claude-Opus-4-6",
        "us.anthropic.claude-opus-4-6-v1",
        "anthropic/claude-opus-4-6",
        "claude-opus-4-6@20260205",
        "claude-opus-4-6-20260205",
    ] {
        assert_eq!(match_claude(spelling), Some(opus), "{spelling}");
    }
    let legacy = match_claude("anthropic.claude-sonnet-4-20250514-v1:0").unwrap();
    assert_eq!((legacy.input, legacy.output), (3.0, 15.0));
    let dated = match_openai("gpt-5.4-mini-2026-03-17").unwrap();
    assert_eq!(dated, match_openai("gpt-5.4-mini").unwrap());
    // Providers never cross.
    assert_eq!(lookup("gpt-5.4", true), None);
    assert_eq!(lookup("claude-opus-4-6", false), None);
}

#[test]
fn bundled_tables_are_sane() {
    let generated: HashMap<String, PriceRow> = serde_json::from_str(PRICES).unwrap();
    assert!(generated.len() >= 40, "{} rows", generated.len());
    for (model, pricing) in table() {
        for value in [
            pricing.input,
            pricing.output,
            pricing.cache_read(),
            pricing.cache_write_5m(),
            pricing.cache_write_1h(),
        ] {
            assert!(value.is_finite() && value >= 0.0, "{model}: {value}");
        }
        assert!(pricing.output > 0.0, "{model} has no output price");
    }
}

#[test]
fn every_family_points_at_a_priced_row() {
    for (_, key) in CLAUDE_FAMILIES.iter().chain(OPENAI_FAMILIES) {
        assert!(table().contains_key(*key), "{key} missing from prices.json");
    }
}

/// Ids the bundled agents report today. A release-time sync that drops one
/// of these fails CI on the bump PR instead of silently blanking the cost
/// column.
#[test]
fn current_agent_models_are_priced_exactly() {
    for model in [
        "claude-fable-5-1",
        "claude-mythos-5-1",
        "claude-opus-5-5",
        "claude-opus-4-8",
        "claude-opus-4-6",
        "claude-sonnet-5-5",
        "claude-sonnet-4-6",
        "claude-haiku-5-5",
        "claude-haiku-4-5-20251001",
    ] {
        assert!(lookup(model, true).is_some(), "{model} not in prices.json");
    }
    for model in [
        "gpt-6-astra",
        "gpt-6-sol",
        "gpt-6.1-sol",
        "gpt-6-luna",
        "gpt-5.6",
        "gpt-5.5",
        "gpt-5.4",
        "gpt-5.4-mini",
        "gpt-5.3-codex",
    ] {
        assert!(lookup(model, false).is_some(), "{model} not in prices.json");
    }
}
