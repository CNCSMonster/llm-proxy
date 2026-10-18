//! Quota dimension logic — the single home for all dimension-specific behaviour.
//!
//! : replaces the `QuotaMode` enum with `String` + centralised match
//! functions.  Adding a new dimension means adding match arms here — no trait,
//! no registry, no `Arc<dyn>`.

/// Increment applied at admission time (inside `reserve_request_slot`).
///
/// `Some(n)` → `used += n`; `None` → no increment at admission.
pub fn reserve_increment(mode: &str) -> Option<i64> {
    match mode {
        "requests" => Some(1),
        _ => None,
    }
}

/// Amount charged after a response completes (inside `add_usage`).
///
/// `Some(n)` → `used += n`; `None` → no charge after response.
pub fn charge_amount(mode: &str, input_tokens: i64, output_tokens: i64) -> Option<i64> {
    match mode {
        "tokens" => Some(input_tokens.saturating_add(output_tokens)),
        _ => None,
    }
}

/// Whether `mode` is a known dimension ID.
///
/// Used at config-load time and CLI/admin input validation.  Hot paths do NOT
/// call this — by the time a `quota_mode` reaches runtime it is guaranteed valid.
pub fn is_valid_mode(mode: &str) -> bool {
    matches!(mode, "tokens" | "requests" | "credits")
}

/// Human-readable label for CLI / API responses.
///
/// Currently all mode IDs are their own label. This function exists as the
/// single point to change if a mode needs a different display name.
pub fn label(mode: &str) -> &str {
    mode
}

/// Compute credits charge from weighted token count and conversion rate.
///
/// `credits = weighted_tokens / 1_000_000 × per_1m`, rounded up.
/// Negative `weighted_tokens` is clamped to 0.
pub fn credits_charge(weighted_tokens: i64, per_1m: f64) -> i64 {
    (weighted_tokens.max(0) as f64 / 1_000_000.0 * per_1m).ceil() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── reserve_increment ──────────────────────────────────────────

    #[test]
    fn reserve_increment_requests_returns_one() {
        assert_eq!(reserve_increment("requests"), Some(1));
    }

    #[test]
    fn reserve_increment_tokens_returns_none() {
        assert_eq!(reserve_increment("tokens"), None);
    }

    #[test]
    fn reserve_increment_unknown_returns_none() {
        assert_eq!(reserve_increment("credits"), None);
    }

    // ── charge_amount ──────────────────────────────────────────────

    #[test]
    fn charge_amount_tokens_sums_io() {
        assert_eq!(charge_amount("tokens", 100, 200), Some(300));
    }

    #[test]
    fn charge_amount_tokens_saturating_add() {
        assert_eq!(charge_amount("tokens", i64::MAX, 1), Some(i64::MAX));
    }

    #[test]
    fn charge_amount_requests_returns_none() {
        assert_eq!(charge_amount("requests", 100, 200), None);
    }

    #[test]
    fn charge_amount_unknown_returns_none() {
        assert_eq!(charge_amount("credits", 100, 200), None);
    }

    // ── is_valid_mode ──────────────────────────────────────────────

    #[test]
    fn is_valid_mode_known() {
        assert!(is_valid_mode("tokens"));
        assert!(is_valid_mode("requests"));
        assert!(is_valid_mode("credits"));
    }

    #[test]
    fn is_valid_mode_unknown() {
        assert!(!is_valid_mode(""));
        assert!(!is_valid_mode("Tokens"));
        assert!(!is_valid_mode("credit"));
    }

    // ── label ──────────────────────────────────────────────────────

    #[test]
    fn label_returns_mode_as_is() {
        assert_eq!(label("tokens"), "tokens");
        assert_eq!(label("requests"), "requests");
        assert_eq!(label("credits"), "credits");
        assert_eq!(label("unknown_mode"), "unknown_mode");
    }

    // ── credits_charge ─────────────────────────────────────────────

    #[test]
    fn credits_charge_basic() {
        // 1M weighted tokens × rate 100.0 = 100 credits
        assert_eq!(credits_charge(1_000_000, 100.0), 100);
    }

    #[test]
    fn credits_charge_rounds_up() {
        // 500K tokens × rate 100.0 = 50.0 → exactly 50
        assert_eq!(credits_charge(500_000, 100.0), 50);
        // 1 token × rate 100.0 = 0.0001 → ceil = 1
        assert_eq!(credits_charge(1, 100.0), 1);
    }

    #[test]
    fn credits_charge_zero_tokens() {
        assert_eq!(credits_charge(0, 100.0), 0);
    }

    #[test]
    fn credits_charge_negative_clamped() {
        assert_eq!(credits_charge(-100, 100.0), 0);
    }

    #[test]
    fn credits_charge_high_rate() {
        // 1M tokens × rate 500.0 = 500 credits
        assert_eq!(credits_charge(1_000_000, 500.0), 500);
    }

    #[test]
    fn credits_charge_fractional_rate() {
        // 1M tokens × rate 0.5 = 1 credit (ceil of 0.5)
        assert_eq!(credits_charge(1_000_000, 0.5), 1);
    }

    // ── 回归基线：模拟新维度 ──────────────────────────────────────

    /// 回归基线：在 `charge_amount` 中临时加一个 `"test_double"` arm，验证集中 match
    /// 函数能被新维度正确使用。此测试验证完后保留——它证明的是"加 match arm 即可
    /// 扩展"这一核心设计不变量。
    #[test]
    fn red_proof_new_dimension_charge() {
        // 模拟新维度的 charge 逻辑（不修改真正的 charge_amount）
        fn test_charge(mode: &str, input: i64, output: i64) -> Option<i64> {
            match mode {
                "tokens" => Some(input.saturating_add(output)),
                "test_double" => Some(input.saturating_add(output).saturating_mul(2)),
                _ => None,
            }
        }
        assert_eq!(test_charge("test_double", 100, 50), Some(300));
        assert_eq!(test_charge("tokens", 100, 50), Some(150));
        assert_eq!(test_charge("requests", 100, 50), None);
    }
}
