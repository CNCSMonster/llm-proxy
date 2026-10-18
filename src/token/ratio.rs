//! Model ratio calculation for unified quota computation
//!
//! Implements the quota formula from the design spec §7.2:
//! ```text
//! quota = ceil((input_tokens + output_tokens × completion_ratio) × model_ratio)
//! ```
//!
//! Resolution order (per §7.3):
//! 1. Exact match in `model_ratios`
//! 2. Longest prefix match in `model_prefix_ratios`
//! 3. Default value from `defaults.model_ratio` / `defaults.completion_ratio`

#![allow(dead_code)] // Used in later stages

use super::config::TokensConfig;

/// Resolved ratios for a specific model
#[derive(Debug, Clone, Copy)]
pub struct ModelRatios {
    pub model_ratio: f64,
    pub completion_ratio: f64,
}

/// Stateless ratio calculator.
///
/// All methods borrow the current `TokensConfig` rather than holding a clone,
/// so a hot-reloaded `tokens.toml` (via `/admin/config/reload`) takes effect
/// immediately — there is no cached snapshot to go stale.
pub struct RatioCalculator;

impl RatioCalculator {
    /// Resolve ratios for a given model name.
    ///
    /// Resolution order:
    /// 1. Exact match in `model_ratios`
    /// 2. Longest prefix match in `model_prefix_ratios`
    /// 3. Default from `defaults`
    pub fn resolve(config: &TokensConfig, model: &str) -> ModelRatios {
        let model_ratio = Self::resolve_model_ratio(config, model);
        let completion_ratio = config.defaults.completion_ratio;

        ModelRatios {
            model_ratio,
            completion_ratio,
        }
    }

    /// Resolve the model ratio for a given model name
    fn resolve_model_ratio(config: &TokensConfig, model: &str) -> f64 {
        // 1. Exact match
        if let Some(&ratio) = config.model_ratios.get(model) {
            return ratio;
        }

        // 2. Longest prefix match
        let mut best_match: Option<(&str, f64)> = None;
        for (prefix, &ratio) in &config.model_prefix_ratios {
            if model.starts_with(prefix.as_str()) {
                let match_len = prefix.len();
                match best_match {
                    Some((best_prefix, _)) if best_prefix.len() >= match_len => {}
                    _ => best_match = Some((prefix, ratio)),
                }
            }
        }

        if let Some((_, ratio)) = best_match {
            return ratio;
        }

        // 3. Default
        config.defaults.model_ratio
    }

    /// Calculate quota consumption for a request.
    ///
    /// Formula: `ceil((input + output × completion_ratio) × model_ratio)`
    pub fn calculate_quota(
        config: &TokensConfig,
        model: &str,
        input_tokens: i64,
        output_tokens: i64,
    ) -> i64 {
        let ratios = Self::resolve(config, model);

        let weighted_tokens = input_tokens as f64 + output_tokens as f64 * ratios.completion_ratio;

        let quota = weighted_tokens * ratios.model_ratio;

        // Round up to ensure we don't under-count
        quota.ceil() as i64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn test_config() -> TokensConfig {
        let mut model_ratios = HashMap::new();
        model_ratios.insert("gpt-4o".to_string(), 2.5);
        model_ratios.insert("gpt-4o-mini".to_string(), 0.075);

        let mut prefix_ratios = HashMap::new();
        prefix_ratios.insert("gpt-4o-".to_string(), 2.5);
        prefix_ratios.insert("claude-".to_string(), 3.0);

        TokensConfig {
            tokens: vec![],
            defaults: super::super::config::RatioDefaults {
                model_ratio: 1.0,
                completion_ratio: 1.0,
            },
            model_ratios,
            model_prefix_ratios: prefix_ratios,
            credits_per_1m: None,
        }
    }

    #[test]
    fn test_exact_match() {
        let config = test_config();
        let ratios = RatioCalculator::resolve(&config, "gpt-4o");
        assert_eq!(ratios.model_ratio, 2.5);
    }

    #[test]
    fn test_prefix_match() {
        let config = test_config();
        // "gpt-4o-2024-08-06" doesn't have exact match, but matches "gpt-4o-" prefix
        let ratios = RatioCalculator::resolve(&config, "gpt-4o-2024-08-06");
        assert_eq!(ratios.model_ratio, 2.5);
    }

    #[test]
    fn test_longest_prefix_wins() {
        let mut config = test_config();
        config
            .model_prefix_ratios
            .insert("gpt-4o-2024-".to_string(), 1.25);

        // "gpt-4o-2024-08-06" should match "gpt-4o-2024-" (longer) over "gpt-4o-"
        let ratios = RatioCalculator::resolve(&config, "gpt-4o-2024-08-06");
        assert_eq!(ratios.model_ratio, 1.25);
    }

    #[test]
    fn test_default_fallback() {
        let config = test_config();
        // "unknown-model" has no match, uses default
        let ratios = RatioCalculator::resolve(&config, "unknown-model");
        assert_eq!(ratios.model_ratio, 1.0);
    }

    #[test]
    fn test_calculate_quota_basic() {
        let config = test_config();
        // No ratio match, default 1.0
        // quota = ceil((100 + 50 × 1.0) × 1.0) = 150
        let quota = RatioCalculator::calculate_quota(&config, "unknown-model", 100, 50);
        assert_eq!(quota, 150);
    }

    #[test]
    fn test_calculate_quota_with_model_ratio() {
        let config = test_config();
        // gpt-4o: ratio = 2.5
        // quota = ceil((100 + 50 × 1.0) × 2.5) = ceil(375) = 375
        let quota = RatioCalculator::calculate_quota(&config, "gpt-4o", 100, 50);
        assert_eq!(quota, 375);
    }

    #[test]
    fn test_calculate_quota_with_completion_ratio() {
        let mut config = test_config();
        config.defaults.completion_ratio = 2.0;

        // gpt-4o: model_ratio = 2.5, completion_ratio = 2.0
        // quota = ceil((100 + 50 × 2.0) × 2.5) = ceil(500) = 500
        let quota = RatioCalculator::calculate_quota(&config, "gpt-4o", 100, 50);
        assert_eq!(quota, 500);
    }

    #[test]
    fn test_calculate_quota_mini_model() {
        let config = test_config();
        // gpt-4o-mini: ratio = 0.075
        // quota = ceil((100 + 50 × 1.0) × 0.075) = ceil(11.25) = 12
        let quota = RatioCalculator::calculate_quota(&config, "gpt-4o-mini", 100, 50);
        assert_eq!(quota, 12);
    }

    #[test]
    fn test_calculate_quota_rounds_up() {
        let mut config = test_config();
        config.model_ratios.insert("test".to_string(), 0.1);

        // test: ratio = 0.1
        // quota = ceil((1 + 0 × 1.0) × 0.1) = ceil(0.1) = 1
        let quota = RatioCalculator::calculate_quota(&config, "test", 1, 0);
        assert_eq!(quota, 1);
    }
}
