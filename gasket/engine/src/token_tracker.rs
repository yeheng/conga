//! Token usage tracking and cost calculation
//!
//! Provides token counting using tiktoken-rs and cost estimation based on provider pricing.
//!
//! Note: TokenTracker and TokenUsage are now defined in gasket_types for cross-crate sharing.
//! This module re-exports the types and adds engine-specific utilities.

pub use gasket_types::{
    calculate_cost, format_cost, format_token_usage, ModelPricing, SessionTokenStats, TokenTracker,
    TokenUsage,
};

use std::sync::OnceLock;
use tiktoken_rs::CoreBPE;
use tracing::warn;

/// Global cached BPE encoder (cl100k_base, covers GPT-4/GPT-3.5).
static ENCODER: OnceLock<Option<CoreBPE>> = OnceLock::new();

fn get_encoder() -> Option<&'static CoreBPE> {
    ENCODER
        .get_or_init(|| match tiktoken_rs::cl100k_base() {
            Ok(enc) => Some(enc),
            Err(e) => {
                warn!(
                    "Failed to init tiktoken cl100k_base encoder: {}. Falling back to len/4.",
                    e
                );
                None
            }
        })
        .as_ref()
}

/// Estimate tokens in text using tiktoken-rs.
///
/// Falls back to `text.len() / 4` if the encoder fails to initialize.
pub fn estimate_tokens(text: &str) -> usize {
    match get_encoder() {
        Some(enc) => enc.encode_with_special_tokens(text).len(),
        None => text.len() / 4,
    }
}

/// Format token/cost info for a single request
pub fn format_request_stats(
    usage: &TokenUsage,
    cost: f64,
    currency: &str,
    pricing: Option<&ModelPricing>,
) -> String {
    let token_line = format!("[Token Usage] {}", format_token_usage(usage));

    let cost_line = if cost > 0.0 {
        let pricing_info = if let Some(p) = pricing {
            format!(
                " (at ${:.2}/M input, ${:.2}/M output)",
                p.price_input_per_million, p.price_output_per_million
            )
        } else {
            "".to_string()
        };
        format!("[Cost] {}{}", format_cost(cost, currency), pricing_info)
    } else {
        "[Cost] N/A (pricing not configured)".to_string()
    };

    format!("{}\n{}", token_line, cost_line)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_token_usage_creation() {
        let usage = TokenUsage::new(100, 50);
        assert_eq!(usage.input_tokens, 100);
        assert_eq!(usage.output_tokens, 50);
        assert_eq!(usage.total_tokens, 150);
    }

    #[test]
    fn test_token_usage_from_api() {
        let usage = TokenUsage::from_api_fields(200, 100);
        assert_eq!(usage.input_tokens, 200);
        assert_eq!(usage.output_tokens, 100);
        assert_eq!(usage.total_tokens, 300);
    }

    #[test]
    fn test_model_pricing_calculation() {
        let pricing = ModelPricing::new(2.5, 10.0, "USD");
        let cost = pricing.calculate_cost(1000, 500);
        // (1000 * 2.5 / 1_000_000) + (500 * 10.0 / 1_000_000) = 0.0025 + 0.005 = 0.0075
        assert!((cost - 0.0075).abs() < 0.0001);
    }

    #[test]
    fn test_model_pricing_zero_tokens() {
        let pricing = ModelPricing::new(2.5, 10.0, "USD");
        let cost = pricing.calculate_cost(0, 0);
        assert_eq!(cost, 0.0);
    }

    #[test]
    fn test_session_stats_accumulation() {
        let mut stats = SessionTokenStats::new("USD");

        let usage1 = TokenUsage::new(100, 50);
        stats.add_usage(&usage1, 0.001);

        let usage2 = TokenUsage::new(200, 100);
        stats.add_usage(&usage2, 0.002);

        assert_eq!(stats.request_count, 2);
        assert_eq!(stats.total_input_tokens, 300);
        assert_eq!(stats.total_output_tokens, 150);
        assert_eq!(stats.total_tokens(), 450);
        assert!((stats.total_cost - 0.003).abs() < 0.0001);
    }

    #[test]
    fn test_session_stats_avg_tokens() {
        let mut stats = SessionTokenStats::new("USD");

        stats.add_usage(&TokenUsage::new(100, 50), 0.001);
        stats.add_usage(&TokenUsage::new(200, 100), 0.002);

        assert!((stats.avg_tokens_per_request() - 225.0).abs() < 0.01);
    }

    #[test]
    fn test_estimate_tokens() {
        // "hello world" should be a small number of tokens
        let tokens = estimate_tokens("hello world");
        assert!(tokens > 0);
        assert!(tokens < 10);

        // Empty string should be 0
        assert_eq!(estimate_tokens(""), 0);
    }

    #[test]
    fn test_calculate_cost_with_pricing() {
        let usage = TokenUsage::new(1000, 500);
        let pricing = ModelPricing::new(2.5, 10.0, "USD");

        let cost = calculate_cost(&usage, Some(&pricing));
        assert!((cost - 0.0075).abs() < 0.0001);
    }

    #[test]
    fn test_calculate_cost_without_pricing() {
        let usage = TokenUsage::new(1000, 500);
        let cost = calculate_cost(&usage, None);
        assert_eq!(cost, 0.0);
    }

    #[test]
    fn test_format_token_usage() {
        let usage = TokenUsage::new(1234, 567);
        let formatted = format_token_usage(&usage);
        assert!(formatted.contains("Input: 1234"));
        assert!(formatted.contains("Output: 567"));
        assert!(formatted.contains("Total: 1801"));
    }

    #[test]
    fn test_format_cost() {
        assert_eq!(format_cost(0.0123, "USD"), "$0.0123");
        assert_eq!(format_cost(0.0123, "CNY"), "¥0.0123");
    }

    #[test]
    fn test_session_stats_format_summary() {
        let mut stats = SessionTokenStats::new("USD");
        stats.add_usage(&TokenUsage::new(100, 50), 0.001);

        let summary = stats.format_summary();
        assert!(summary.contains("Requests: 1"));
        assert!(summary.contains("Total Tokens: 150"));
        assert!(summary.contains("Total Cost: $0.0010"));
    }
}
