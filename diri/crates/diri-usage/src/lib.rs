//! Shared API-equivalent pricing used by local and node transcript fallbacks.
//!
//! These values are estimates, never authoritative provider billing. Callers
//! must label them accordingly and prefer billed spend when available.

pub const PRICING_ENTRY_COUNT: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ModelPricing {
    pub input: f64,
    pub output: f64,
}

impl ModelPricing {
    pub const fn cache_read(self) -> f64 {
        self.input * 0.1
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
        Some(ModelPricing {
            input: 10.0,
            output: 50.0,
        })
    } else if model.contains("opus-4-1") || model.contains("opus-4-2025") {
        Some(ModelPricing {
            input: 15.0,
            output: 75.0,
        })
    } else if model.contains("opus") {
        Some(ModelPricing {
            input: 5.0,
            output: 25.0,
        })
    } else if model.contains("sonnet") {
        Some(ModelPricing {
            input: 3.0,
            output: 15.0,
        })
    } else if model.contains("haiku-4") {
        Some(ModelPricing {
            input: 1.0,
            output: 5.0,
        })
    } else if model.contains("3-5-haiku") {
        Some(ModelPricing {
            input: 0.8,
            output: 4.0,
        })
    } else if model.contains("haiku") {
        Some(ModelPricing {
            input: 0.25,
            output: 1.25,
        })
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
        Some(ModelPricing {
            input: 10.0,
            output: 50.0,
        })
    } else if model.contains("gpt-5.4-mini") {
        Some(ModelPricing {
            input: 0.75,
            output: 4.5,
        })
    } else if model.contains("gpt-5.4") {
        Some(ModelPricing {
            input: 2.5,
            output: 15.0,
        })
    } else if model.contains("gpt-5.5") {
        Some(ModelPricing {
            input: 5.0,
            output: 30.0,
        })
    } else if model.contains("codex-mini") {
        Some(ModelPricing {
            input: 1.5,
            output: 6.0,
        })
    } else if model.contains("codex") {
        Some(ModelPricing {
            input: 1.75,
            output: 14.0,
        })
    } else if model.contains("mini") {
        Some(ModelPricing {
            input: 0.25,
            output: 2.0,
        })
    } else if model.contains("nano") {
        Some(ModelPricing {
            input: 0.05,
            output: 0.4,
        })
    } else if model.contains("gpt-5") {
        Some(ModelPricing {
            input: 1.25,
            output: 10.0,
        })
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
    fn estimates_keep_cache_rates_distinct() {
        let price = claude_estimate("claude-sonnet", 1_000_000, 0, 0, 0, 0).expect("known model");
        assert_eq!(price, 3.0);
        let cached = openai_estimate("codex", 0, 0, 1_000_000).expect("known model");
        assert!((cached - 0.175).abs() < 1e-12);
    }
}
