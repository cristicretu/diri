//! Shared API-equivalent pricing used by local and node transcript fallbacks.
//!
//! These values are estimates, never authoritative provider billing. Callers
//! must label them accordingly and prefer billed spend when available.

pub const PRICING_ENTRY_COUNT: usize = 17;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ModelPricing {
    pub input: f64,
    pub output: f64,
    /// Published cached-input price as a fraction of `input`. Most bundled
    /// models use 10%; gpt-6.1-sol's published rate is 5%.
    cache_read_ratio: f64,
}

impl ModelPricing {
    const fn new(input: f64, output: f64) -> Self {
        Self {
            input,
            output,
            cache_read_ratio: 0.1,
        }
    }

    const fn with_cache_read_ratio(self, cache_read_ratio: f64) -> Self {
        Self {
            cache_read_ratio,
            ..self
        }
    }

    pub const fn cache_read(self) -> f64 {
        self.input * self.cache_read_ratio
    }

    pub const fn cache_write_5m(self) -> f64 {
        self.input * 1.25
    }

    pub const fn cache_write_1h(self) -> f64 {
        self.input * 2.0
    }
}

/// Match Claude model names in specific-to-generic order.
pub fn match_claude(model: &str) -> Option<ModelPricing> {
    if model.contains("fable") || model.contains("mythos") {
        Some(ModelPricing::new(10.0, 50.0))
    } else if model.contains("opus-4-1") || model.contains("opus-4-2025") {
        Some(ModelPricing::new(15.0, 75.0))
    } else if model.contains("opus") {
        Some(ModelPricing::new(5.0, 25.0))
    } else if model.contains("sonnet") {
        Some(ModelPricing::new(3.0, 15.0))
    } else if model.contains("haiku-4") {
        Some(ModelPricing::new(1.0, 5.0))
    } else if model.contains("3-5-haiku") {
        Some(ModelPricing::new(0.8, 4.0))
    } else if model.contains("haiku") {
        Some(ModelPricing::new(0.25, 1.25))
    } else {
        None
    }
}

/// Match OpenAI model names in specific-to-generic order.
pub fn match_openai(model: &str) -> Option<ModelPricing> {
    if model == "gpt-6-astra" {
        // Standard API-equivalent rates, verified 2026-09-13:
        // https://developers.openai.com/api/docs/models/gpt-6-astra
        // These base estimates exclude service-tier and long-context premiums.
        Some(ModelPricing::new(10.0, 50.0))
    } else if model == "gpt-6.1-sol" {
        // Standard short-context rates, verified 2026-10-13:
        // https://developers.openai.com/api/docs/models/gpt-6.1-sol
        // Cached input is $0.10/M (5% of input), not the 10% factor used above.
        // These base estimates exclude service-tier and long-context premiums.
        Some(ModelPricing::new(2.0, 10.0).with_cache_read_ratio(0.05))
    } else if model.contains("gpt-5.4-mini") {
        Some(ModelPricing::new(0.75, 4.5))
    } else if model.contains("gpt-5.4") {
        Some(ModelPricing::new(2.5, 15.0))
    } else if model.contains("gpt-5.5") {
        Some(ModelPricing::new(5.0, 30.0))
    } else if model.contains("codex-mini") {
        Some(ModelPricing::new(1.5, 6.0))
    } else if model.contains("codex") {
        Some(ModelPricing::new(1.75, 14.0))
    } else if model.contains("mini") {
        Some(ModelPricing::new(0.25, 2.0))
    } else if model.contains("nano") {
        Some(ModelPricing::new(0.05, 0.4))
    } else if model.contains("gpt-5") {
        Some(ModelPricing::new(1.25, 10.0))
    } else {
        None
    }
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
mod tests {
    use super::*;

    #[test]
    fn astra_usage_has_a_standard_cost_estimate() {
        // 60 uncached input + 40 cached input + 20 output tokens.
        let cost = openai_estimate("gpt-6-astra", 60, 20, 40).expect("Astra usage must be priced");
        assert!((cost - 0.001_64).abs() < 1e-12);
        assert_eq!(match_openai("gpt-6-unknown"), None);
    }

    #[test]
    fn sol_usage_uses_the_published_standard_rate() {
        let pricing = match_openai("gpt-6.1-sol").expect("Sol must be priced");
        assert_eq!((pricing.input, pricing.output), (2.0, 10.0));
        assert!((pricing.cache_read() - 0.1).abs() < 1e-12);
        // 60 uncached input + 40 cached input + 20 output tokens.
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
}

pub mod transcripts;
