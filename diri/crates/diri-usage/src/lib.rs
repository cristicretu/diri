//! Shared API-equivalent pricing used by local and node transcript fallbacks.
//!
//! These values are estimates, never authoritative provider billing. Callers
//! must label them accordingly and prefer billed spend when available.

use std::{collections::HashMap, sync::OnceLock};

/// USD per million tokens, regenerated from LiteLLM by
/// `diri/scripts/sync-model-prices.py` (the "Model prices" workflow).
const PRICES: &str = include_str!("../prices.json");
/// Hand-kept rates for ids the generated table lacks. These win.
const OVERRIDES: &str = include_str!("../price-overrides.json");

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ModelPricing {
    pub input: f64,
    pub output: f64,
    cache_read: f64,
    cache_write_5m: f64,
    cache_write_1h: f64,
}

impl ModelPricing {
    pub const fn cache_read(self) -> f64 {
        self.cache_read
    }

    pub const fn cache_write_5m(self) -> f64 {
        self.cache_write_5m
    }

    pub const fn cache_write_1h(self) -> f64 {
        self.cache_write_1h
    }
}

#[derive(serde::Deserialize)]
struct PriceRow {
    input: f64,
    output: f64,
    cache_read: Option<f64>,
    cache_write_5m: Option<f64>,
    cache_write_1h: Option<f64>,
}

impl From<PriceRow> for ModelPricing {
    /// Unpublished cache rates use Anthropic's standard multipliers.
    fn from(row: PriceRow) -> Self {
        Self {
            input: row.input,
            output: row.output,
            cache_read: row.cache_read.unwrap_or(row.input * 0.1),
            cache_write_5m: row.cache_write_5m.unwrap_or(row.input * 1.25),
            cache_write_1h: row.cache_write_1h.unwrap_or(row.input * 2.0),
        }
    }
}

#[derive(serde::Deserialize)]
struct Overrides {
    models: HashMap<String, PriceRow>,
}

fn table() -> &'static HashMap<String, ModelPricing> {
    static TABLE: OnceLock<HashMap<String, ModelPricing>> = OnceLock::new();
    TABLE.get_or_init(|| {
        let generated: HashMap<String, PriceRow> =
            serde_json::from_str(PRICES).expect("bundled prices.json");
        let overrides: Overrides =
            serde_json::from_str(OVERRIDES).expect("bundled price-overrides.json");
        generated
            .into_iter()
            .chain(overrides.models)
            .map(|(model, row)| (model, row.into()))
            .collect()
    })
}

/// Reduce provider spellings to the first-party id: `[1m]` context tags,
/// `vertex_ai/` or `us.anthropic.` prefixes, Bedrock `-v1:0` and Vertex
/// `@date` suffixes. Dated ids fall back to their undated alias.
fn lookup(model: &str, claude: bool) -> Option<ModelPricing> {
    let mut id = model.trim().to_ascii_lowercase();
    if let Some(open) = id.find('[') {
        id.truncate(open);
    }
    if let Some(slash) = id.rfind('/') {
        id.drain(..=slash);
    }
    if let Some(at) = id.find("anthropic.") {
        id.drain(..at + "anthropic.".len());
    }
    if let Some(at) = id.find('@') {
        id.truncate(at);
    }
    if let Some(version) = id.rfind("-v")
        && id[version + 2..]
            .split(':')
            .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
    {
        id.truncate(version);
    }
    if id.starts_with("claude-") != claude {
        return None;
    }
    let table = table();
    table
        .get(&id)
        .or_else(|| table.get(strip_date(&id)?))
        .copied()
}

fn strip_date(id: &str) -> Option<&str> {
    let digits = |s: &str| s.bytes().all(|b| b.is_ascii_digit());
    let (head, tail) = id.rsplit_once('-')?;
    if tail.len() == 8 && digits(tail) {
        return Some(head);
    }
    // OpenAI dates: gpt-5.4-2026-03-05.
    let (head, day) = head
        .rsplit_once('-')
        .filter(|_| tail.len() == 2 && digits(tail))?;
    let (head, year) = head
        .rsplit_once('-')
        .filter(|_| day.len() == 2 && digits(day))?;
    (year.len() == 4 && digits(year)).then_some(head)
}

/// Families for ids the table does not name, specific to generic. Each maps
/// to a table row, so the guess still follows synced prices.
const CLAUDE_FAMILIES: &[(&[&str], &str)] = &[
    (&["fable"], "claude-fable-5-1"),
    (&["mythos"], "claude-mythos-5-1"),
    (&["opus-4-1", "opus-4-2025"], "claude-opus-4-1"),
    (&["opus"], "claude-opus-4-8"),
    (&["sonnet"], "claude-sonnet-4-6"),
    (&["haiku-4"], "claude-haiku-4-5"),
    (&["3-5-haiku"], "claude-3-5-haiku"),
    (&["haiku"], "claude-3-haiku"),
];

const OPENAI_FAMILIES: &[(&[&str], &str)] = &[
    (&["gpt-5.4-mini"], "gpt-5.4-mini"),
    (&["gpt-5.4"], "gpt-5.4"),
    (&["gpt-5.5"], "gpt-5.5"),
    (&["codex-mini"], "codex-mini-latest"),
    (&["codex"], "gpt-5.2-codex"),
    (&["mini"], "gpt-5-mini"),
    (&["nano"], "gpt-5-nano"),
    (&["gpt-5"], "gpt-5"),
];

fn family(model: &str, families: &[(&[&str], &str)]) -> Option<ModelPricing> {
    families
        .iter()
        .find(|(needles, _)| needles.iter().any(|needle| model.contains(needle)))
        .and_then(|(_, key)| table().get(*key).copied())
}

/// Exact (normalized) Claude id first, then family.
pub fn match_claude(model: &str) -> Option<ModelPricing> {
    lookup(model, true).or_else(|| family(model, CLAUDE_FAMILIES))
}

/// Exact (normalized) OpenAI id first, then family. gpt-6 ids have no family
/// fallback, so `gpt-6.1-sol-fast` stays unpriced rather than borrowing Sol.
pub fn match_openai(model: &str) -> Option<ModelPricing> {
    lookup(model, false).or_else(|| family(model, OPENAI_FAMILIES))
}

pub fn openai_estimate(
    model: &str,
    input_tokens: i64,
    output_tokens: i64,
    cache_read_tokens: i64,
) -> Option<f64> {
    let pricing = match_openai(model)?;
    Some(
        (input_tokens.max(0) as f64 * pricing.input
            + output_tokens.max(0) as f64 * pricing.output
            + cache_read_tokens.max(0) as f64 * pricing.cache_read())
            / 1_000_000.0,
    )
}

pub fn claude_estimate(
    model: &str,
    input_tokens: i64,
    output_tokens: i64,
    cache_read_tokens: i64,
    cache_write_5m_tokens: i64,
    cache_write_1h_tokens: i64,
) -> Option<f64> {
    let pricing = match_claude(model)?;
    Some(
        (input_tokens.max(0) as f64 * pricing.input
            + output_tokens.max(0) as f64 * pricing.output
            + cache_read_tokens.max(0) as f64 * pricing.cache_read()
            + cache_write_5m_tokens.max(0) as f64 * pricing.cache_write_5m()
            + cache_write_1h_tokens.max(0) as f64 * pricing.cache_write_1h())
            / 1_000_000.0,
    )
}

#[cfg(test)]
mod tests;

pub mod transcripts;
