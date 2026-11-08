//! USD cost of a provider call from token counts. Static list prices (standard tier, short
//! context); a missing price or token count yields `None` rather than a misleading zero.

use super::AiProviderKind;

use AiProviderKind::{Anthropic, Gemini, OpenAi};

/// (provider, model prefix, USD per 1M input tokens, USD per 1M output tokens).
/// Sources: ai.google.dev/gemini-api/docs/pricing, openai.com API pricing,
/// platform.claude.com/docs/en/about-claude/pricing.
// Static table; move to an env override if list prices change often.
const PRICES: &[(AiProviderKind, &str, f64, f64)] = &[
    (Gemini, "gemini-3.8-flash", 0.75, 3.75),
    (Gemini, "gemini-3.6-flash", 0.75, 3.75),
    (Gemini, "gemini-3.5-flash-lite", 0.30, 2.50),
    (Gemini, "gemini-2.5-flash", 0.30, 2.50),
    (Gemini, "gemini-2.5-flash-lite", 0.10, 0.40),
    (OpenAi, "gpt-5.6-terra", 2.00, 12.00),
    (OpenAi, "gpt-5.6-luna", 0.20, 1.20),
    (OpenAi, "gpt-5.6-sol", 4.00, 20.00),
    (Anthropic, "claude-opus-5-5", 4.00, 20.00),
    (Anthropic, "claude-sonnet-5-5", 2.00, 10.00),
    (Anthropic, "claude-sonnet-4-6", 3.00, 15.00),
    (Anthropic, "claude-sonnet-4-5", 3.00, 15.00),
    (Anthropic, "claude-haiku-4-5", 1.00, 5.00),
];

#[must_use]
#[allow(
    clippy::cast_precision_loss,
    reason = "token counts are far below 2^52; cost is an f64 estimate"
)]
pub fn cost_usd(
    kind: AiProviderKind,
    model: &str,
    tokens_in: Option<i64>,
    tokens_out: Option<i64>,
) -> Option<f64> {
    let (tin, tout) = (tokens_in?, tokens_out?);
    let (_, _, pin, pout) = PRICES
        .iter()
        .filter(|(k, prefix, ..)| *k == kind && model.starts_with(prefix))
        .max_by_key(|(_, prefix, ..)| prefix.len())?;
    Some((tin as f64 * pin + tout as f64 * pout) / 1_000_000.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_match_and_arithmetic() {
        let c = cost_usd(Gemini, "gemini-3.8-flash", Some(1_000_000), Some(1_000_000)).unwrap();
        assert!((c - 4.50).abs() < 1e-9);
    }

    #[test]
    fn dated_suffix_matches_by_prefix() {
        assert!(cost_usd(Anthropic, "claude-sonnet-5-5-20261001", Some(10), Some(10)).is_some());
    }

    #[test]
    fn longest_prefix_wins() {
        let c = cost_usd(Gemini, "gemini-2.5-flash-lite", Some(1_000_000), Some(0)).unwrap();
        assert!((c - 0.10).abs() < 1e-9);
        let c = cost_usd(Gemini, "gemini-2.5-flash", Some(1_000_000), Some(0)).unwrap();
        assert!((c - 0.30).abs() < 1e-9);
    }

    #[test]
    fn unknown_model_or_wrong_provider_is_none() {
        assert_eq!(cost_usd(Gemini, "mystery", Some(1), Some(1)), None);
        assert_eq!(cost_usd(OpenAi, "gemini-3.8-flash", Some(1), Some(1)), None);
    }

    #[test]
    fn missing_either_count_is_none() {
        assert_eq!(cost_usd(Gemini, "gemini-3.8-flash", None, Some(1)), None);
        assert_eq!(cost_usd(Gemini, "gemini-3.8-flash", Some(1), None), None);
    }
}
