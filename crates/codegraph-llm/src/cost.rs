//! Per-backend pricing and USD cost estimation. Prices are published
//! USD-per-million-token rates for each backend's *default* model; a non-default
//! model (via the backend's `*_MODEL` env override) may mis-estimate, notably for
//! Azure/Bedrock. Estimates, not invoices.

/// Published price for one backend, in USD per 1,000,000 tokens.
#[derive(Debug, Clone, Copy)]
pub struct Pricing {
    pub input_per_mtok: f64,
    pub output_per_mtok: f64,
}

/// `(backend_name, pricing)` for every backend CodeGraph can route to.
/// Local/subscription backends (Ollama, claude-CLI) are zero-rated.
pub static PRICING: &[(&str, Pricing)] = &[
    // OpenAI-compatible backends (see `registry::BACKENDS`).
    (
        "gemini",
        Pricing {
            input_per_mtok: 0.50,
            output_per_mtok: 3.00,
        },
    ),
    (
        "kimi",
        Pricing {
            input_per_mtok: 0.74,
            output_per_mtok: 4.66,
        },
    ),
    (
        "openai",
        Pricing {
            input_per_mtok: 0.40,
            output_per_mtok: 1.60,
        },
    ),
    (
        "deepseek",
        Pricing {
            input_per_mtok: 0.14,
            output_per_mtok: 0.28,
        },
    ),
    (
        "azure",
        Pricing {
            input_per_mtok: 2.50,
            output_per_mtok: 10.00,
        },
    ),
    (
        "ollama",
        Pricing {
            input_per_mtok: 0.0,
            output_per_mtok: 0.0,
        },
    ),
    // Native (non-OpenAI-compat) backends.
    (
        "claude",
        Pricing {
            input_per_mtok: 3.00,
            output_per_mtok: 15.00,
        },
    ),
    (
        "bedrock",
        Pricing {
            input_per_mtok: 3.00,
            output_per_mtok: 15.00,
        },
    ),
    (
        "claude-cli",
        Pricing {
            input_per_mtok: 0.0,
            output_per_mtok: 0.0,
        },
    ),
];

/// Published pricing for `backend`, or `None` if the name is unknown.
pub fn pricing(backend: &str) -> Option<Pricing> {
    PRICING.iter().find(|(n, _)| *n == backend).map(|(_, p)| *p)
}

/// Estimated USD cost for `input_tokens`/`output_tokens` on `backend`. Unknown
/// backends (those without a [`PRICING`] entry) cost `0.0`.
pub fn estimate_cost(backend: &str, input_tokens: u64, output_tokens: u64) -> f64 {
    match pricing(backend) {
        Some(p) => {
            (input_tokens as f64 * p.input_per_mtok + output_tokens as f64 * p.output_per_mtok)
                / 1_000_000.0
        }
        None => 0.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f64, b: f64) {
        assert!((a - b).abs() < 1e-9, "{a} != {b}");
    }

    #[test]
    fn claude_cost_sums_input_and_output_rates() {
        // 1M input + 1M output at $3/$15 -> $18.
        approx(estimate_cost("claude", 1_000_000, 1_000_000), 18.0);
    }

    #[test]
    fn openai_input_only() {
        approx(estimate_cost("openai", 1_000_000, 0), 0.40);
    }

    #[test]
    fn local_backends_are_free() {
        approx(estimate_cost("ollama", 5_000_000, 5_000_000), 0.0);
        approx(estimate_cost("claude-cli", 5_000_000, 5_000_000), 0.0);
    }

    #[test]
    fn unknown_backend_costs_nothing() {
        approx(estimate_cost("does-not-exist", 1_000_000, 1_000_000), 0.0);
    }

    #[test]
    fn every_routable_backend_is_priced() {
        for name in [
            "gemini",
            "kimi",
            "openai",
            "deepseek",
            "azure",
            "ollama",
            "claude",
            "bedrock",
            "claude-cli",
        ] {
            assert!(pricing(name).is_some(), "{name} must have a price entry");
        }
    }
}
