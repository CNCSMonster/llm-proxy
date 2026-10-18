//! Token configuration parsing.
//!
//! Supports two storage layouts:
//!
//! ## Legacy: single `tokens.toml` file
//!
//! ```toml
//! [[tokens]]
//! key = "lp-aBcDeFgHiJkLmNoPqRsTuVwX"
//! name = "alice-openai"
//! allowed_models = ["gpt-4o", "gpt-4o-mini"]
//!
//! [[tokens.quota_windows]]
//! window = "5h"
//! quota_mode = "requests"
//! quota_limit = 60
//!
//! expired_at = "2026-12-31T23:59:59Z"
//! ```
//!
//! ## New: `tokens/` directory (one file per token)
//!
//! ```toml
//! # tokens/alice.toml
//! key = "lp-aBcDeFgHiJkLmNoPqRsTuVwX"
//! name = "alice-openai"
//! allowed_models = ["gpt-4o", "gpt-4o-mini"]
//!
//! [[model_quota_groups]]
//! models = ["gpt-4o", "gpt-4o-mini"]
//!
//!   [[model_quota_groups.quota_windows]]
//!   window = "5h"
//!   quota_mode = "requests"
//!   quota_limit = 60
//! ```

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// A single quota window configuration
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[allow(dead_code)] // Used in later stages
pub struct QuotaWindow {
    /// Window duration string (e.g., "5h", "30d", "1h30m")
    pub window: String,
    /// What to count: "tokens" or "requests" (: String, not enum)
    pub quota_mode: String,
    /// Maximum allowed within the window
    pub quota_limit: i64,
}

/// A model-scoped quota group: a set of models sharing the same quota windows.
///
/// When a request's model matches the `models` list (or `models` is empty,
/// meaning all models), the group's quota windows are checked and charged.
/// Multiple groups can match the same model — all matched groups are checked
/// and charged (AND semantics).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[allow(dead_code)] // Used in later stages
pub struct ModelQuotaGroup {
    /// Models this group applies to (empty = all models).
    #[serde(default)]
    pub models: Vec<String>,
    /// Quota windows for this group.
    #[serde(default)]
    pub quota_windows: Vec<QuotaWindow>,
}

impl ModelQuotaGroup {
    /// Returns a canonical identifier for this group based on its model list.
    /// Used as `group_id` in the SQLite schema.
    ///
    /// The models are sorted and joined with commas to ensure two groups with
    /// the same models (in different order) produce the same ID.
    #[allow(dead_code)]
    pub fn canonical_id(&self) -> String {
        let mut sorted = self.models.clone();
        sorted.sort();
        sorted.join(",")
    }

    /// Check if this group applies to the given model.
    /// Empty `models` means all models match.
    #[allow(dead_code)]
    pub fn matches_model(&self, model: &str) -> bool {
        self.models.is_empty() || self.models.iter().any(|m| m == model)
    }
}

/// A single token definition
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[allow(dead_code)] // Used in later stages
pub struct TokenConfig {
    /// The API key (lp- prefix + random string)
    pub key: String,
    /// Human-readable name
    pub name: String,
    /// List of allowed model IDs (empty = reject all)
    #[serde(default)]
    pub allowed_models: Vec<String>,
    /// Quota windows (empty = unlimited). Legacy flat list, applies to all models.
    /// Mutually exclusive with `model_quota_groups` — if both are present,
    /// `model_quota_groups` takes precedence.
    #[serde(default)]
    pub quota_windows: Vec<QuotaWindow>,
    /// Model-scoped quota groups . Each group defines a set of models
    /// and quota windows that apply to those models. Multiple groups can overlap;
    /// all matching groups are checked and charged (AND semantics).
    #[serde(default)]
    pub model_quota_groups: Vec<ModelQuotaGroup>,
    /// Optional expiration time (ISO 8601)
    #[serde(default)]
    pub expired_at: Option<String>,
    /// Per-account override for bad-request budget daily limit.
    /// When `None`, falls back to the global `[protection.bad_request_budget]` default.
    #[serde(default)]
    pub bad_request_budget_daily_limit: Option<u32>,
    /// Per-account override for bad-request budget lifetime limit.
    /// When `None`, falls back to the global `[protection.bad_request_budget]` default.
    #[serde(default)]
    pub bad_request_budget_lifetime_limit: Option<u32>,
    /// Whether this token has administrative privileges (default false).
    #[serde(default)]
    pub admin: bool,
}

impl TokenConfig {
    /// Returns the effective quota windows for a given model.
    ///
    /// If `model_quota_groups` is non-empty, returns windows from all groups
    /// that match the model (AND semantics). Otherwise, falls back to the
    /// legacy flat `quota_windows` list.
    #[allow(dead_code)]
    pub fn effective_windows_for_model(&self, model: &str) -> Vec<&QuotaWindow> {
        if !self.model_quota_groups.is_empty() {
            self.model_quota_groups
                .iter()
                .filter(|g| g.matches_model(model))
                .flat_map(|g| &g.quota_windows)
                .collect()
        } else {
            self.quota_windows.iter().collect()
        }
    }

    /// Returns all quota windows across all groups (for initialization/migration).
    /// If `model_quota_groups` is non-empty, returns windows from all groups.
    /// Otherwise, returns the legacy flat `quota_windows` list.
    #[allow(dead_code)]
    pub fn all_quota_windows(&self) -> Vec<&QuotaWindow> {
        if !self.model_quota_groups.is_empty() {
            self.model_quota_groups
                .iter()
                .flat_map(|g| &g.quota_windows)
                .collect()
        } else {
            self.quota_windows.iter().collect()
        }
    }
}

impl TokenConfig {
    #[allow(dead_code)]
    pub fn key_hash(&self) -> String {
        super::keygen::token_key_hash(&self.key)
    }
}

/// Root configuration structure for tokens.toml
#[derive(Debug, Clone, PartialEq, Deserialize, Default)]
#[allow(dead_code)] // Used in later stages
pub struct TokensConfig {
    #[serde(default)]
    pub tokens: Vec<TokenConfig>,

    /// Default ratios (optional, default 1.0)
    #[serde(default)]
    pub defaults: RatioDefaults,

    /// Model-specific ratios (exact match, highest priority)
    #[serde(default)]
    pub model_ratios: std::collections::HashMap<String, f64>,

    /// Prefix-based ratios (exact match fails, longest prefix wins)
    #[serde(default)]
    pub model_prefix_ratios: std::collections::HashMap<String, f64>,

    /// Credits conversion rate: credits per 1M weighted tokens.
    /// `None` → fallback to 100.0 at charge time.
    #[serde(default)]
    pub credits_per_1m: Option<f64>,
}

/// Default ratio configuration
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[allow(dead_code)] // Used in later stages
pub struct RatioDefaults {
    #[serde(default = "default_ratio")]
    pub model_ratio: f64,
    #[serde(default = "default_ratio")]
    pub completion_ratio: f64,
}

impl Default for RatioDefaults {
    fn default() -> Self {
        Self {
            model_ratio: 1.0,
            completion_ratio: 1.0,
        }
    }
}

fn default_ratio() -> f64 {
    1.0
}

#[allow(dead_code)] // Used in later stages
impl TokensConfig {
    /// Load tokens configuration from a file path.
    /// Returns Ok(default) if file doesn't exist.
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read tokens config: {}", path.display()))?;
        Self::parse(&content)
    }

    /// Parse tokens configuration from TOML content.
    pub fn parse(content: &str) -> Result<Self> {
        if content.trim().is_empty() {
            return Ok(Self::default());
        }
        let config: TokensConfig =
            toml::from_str(content).context("failed to parse tokens.toml")?;
        config.validate()?;
        Ok(config)
    }

    /// Validate the configuration:
    /// - All keys must be unique
    /// - All keys must start with "lp-"
    /// - All names must be unique
    /// - Quota windows must have valid duration strings
    /// - expired_at must be valid ISO 8601 if present
    /// - model_quota_groups windows must also be valid
    fn validate(&self) -> Result<()> {
        let mut keys = HashSet::new();
        let mut names = HashSet::new();

        for (i, token) in self.tokens.iter().enumerate() {
            // Validate key format
            if !token.key.starts_with("lp-") {
                bail!(
                    "tokens[{}].key must start with 'lp-', got: {}",
                    i,
                    token.key
                );
            }
            if token.key.len() < 10 {
                bail!(
                    "tokens[{}].key too short (minimum 10 chars): {}",
                    i,
                    token.key
                );
            }

            // Check key uniqueness
            if !keys.insert(&token.key) {
                bail!("duplicate token key: {}", token.key);
            }

            // Check name uniqueness
            if !names.insert(&token.name) {
                bail!("duplicate token name: {}", token.name);
            }

            // Validate name is not empty
            if token.name.is_empty() {
                bail!("tokens[{}].name cannot be empty", i);
            }

            // Validate legacy quota windows
            for (j, window) in token.quota_windows.iter().enumerate() {
                parse_duration(&window.window).with_context(|| {
                    format!(
                        "tokens[{}].quota_windows[{}].window '{}' is invalid",
                        i, j, window.window
                    )
                })?;
                if window.quota_limit <= 0 {
                    bail!(
                        "tokens[{}].quota_windows[{}].quota_limit must be positive, got: {}",
                        i,
                        j,
                        window.quota_limit
                    );
                }
                if !super::dimension::is_valid_mode(&window.quota_mode) {
                    bail!(
                        "tokens[{}].quota_windows[{}].quota_mode '{}' is unknown; available: tokens, requests, credits",
                        i,
                        j,
                        window.quota_mode
                    );
                }
            }

            // Validate model_quota_groups windows
            for (g, group) in token.model_quota_groups.iter().enumerate() {
                for (j, window) in group.quota_windows.iter().enumerate() {
                    parse_duration(&window.window).with_context(|| {
                        format!(
                            "tokens[{}].model_quota_groups[{}].quota_windows[{}].window '{}' is invalid",
                            i, g, j, window.window
                        )
                    })?;
                    if window.quota_limit <= 0 {
                        bail!(
                            "tokens[{}].model_quota_groups[{}].quota_windows[{}].quota_limit must be positive, got: {}",
                            i,
                            g,
                            j,
                            window.quota_limit
                        );
                    }
                    if !super::dimension::is_valid_mode(&window.quota_mode) {
                        bail!(
                            "tokens[{}].model_quota_groups[{}].quota_windows[{}].quota_mode '{}' is unknown; available: tokens, requests, credits",
                            i,
                            g,
                            j,
                            window.quota_mode
                        );
                    }
                }
            }

            // Validate expired_at if present
            if let Some(ref expired_at) = token.expired_at {
                chrono::DateTime::parse_from_rfc3339(expired_at).with_context(|| {
                    format!(
                        "tokens[{}].expired_at '{}' is not valid ISO 8601",
                        i, expired_at
                    )
                })?;
            }
        }

        Ok(())
    }

    /// Check if authentication is enabled (has at least one token)
    pub fn is_enabled(&self) -> bool {
        !self.tokens.is_empty()
    }

    /// Find a token by key
    pub fn find_by_key(&self, key: &str) -> Option<&TokenConfig> {
        self.tokens.iter().find(|t| t.key == key)
    }

    /// Load tokens from a `tokens/` directory (one `.toml` file per token).
    ///
    /// # Loading strategy
    ///
    /// - Directory does not exist → `Ok(None)` (caller treats as open mode)
    /// - Directory exists but empty (no `.toml` files) → `Ok(Some(default))` with empty tokens
    /// - All files parse successfully → `Ok(Some(config))` with all tokens
    /// - Any file fails to parse → `Err` with all errors collected
    ///
    /// Cross-file uniqueness of `key` and `name` is enforced.
    pub fn load_from_directory(dir: &Path) -> Result<Option<Self>> {
        if !dir.exists() {
            return Ok(None);
        }
        if !dir.is_dir() {
            bail!(
                "tokens path exists but is not a directory: {}",
                dir.display()
            );
        }

        // Collect all .toml files
        let mut toml_files: Vec<PathBuf> = std::fs::read_dir(dir)
            .with_context(|| format!("failed to read tokens directory: {}", dir.display()))?
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "toml"))
            .collect();

        // Sort for deterministic loading order
        toml_files.sort();

        if toml_files.is_empty() {
            tracing::warn!(
                directory = %dir.display(),
                "tokens/ directory exists but contains no .toml files; running in open mode"
            );
            return Ok(Some(Self::default()));
        }

        // Parse each file, collecting errors
        let mut tokens = Vec::new();
        let mut errors: Vec<String> = Vec::new();

        for file_path in &toml_files {
            match Self::parse_single_token_file(file_path) {
                Ok(token) => tokens.push(token),
                Err(err) => {
                    let file_name = file_path
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| file_path.display().to_string());
                    errors.push(format!("{}: {:#}", file_name, err));
                }
            }
        }

        if !errors.is_empty() {
            let success_count = tokens.len();
            let error_report = errors.join("\n  ✗ ");
            bail!(
                "token configuration has errors, refusing to load.\n\n\
                 ✗ {}\n\n\
                 ✓ {} other token file(s) loaded successfully\n\n\
                 Fix the errors above and retry.",
                error_report,
                success_count
            );
        }

        // Build config and validate cross-file uniqueness
        let config = Self {
            tokens,
            ..Default::default()
        };
        config.validate()?;
        Ok(Some(config))
    }

    /// Parse a single token file from the `tokens/` directory.
    /// Each file contains exactly one token (not wrapped in `[[tokens]]`).
    fn parse_single_token_file(path: &Path) -> Result<TokenConfig> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;

        if content.trim().is_empty() {
            bail!("file is empty");
        }

        let token: TokenConfig = toml::from_str(&content)
            .with_context(|| format!("failed to parse {}", path.display()))?;

        // Basic per-token validation (key/name format)
        if !token.key.starts_with("lp-") {
            bail!("key must start with 'lp-', got: {}", token.key);
        }
        if token.key.len() < 10 {
            bail!("key too short (minimum 10 chars): {}", token.key);
        }
        if token.name.is_empty() {
            bail!("name cannot be empty");
        }

        // Validate quota windows (both legacy and groups)
        for (j, window) in token.quota_windows.iter().enumerate() {
            parse_duration(&window.window).with_context(|| {
                format!("quota_windows[{}].window '{}' is invalid", j, window.window)
            })?;
            if window.quota_limit <= 0 {
                bail!(
                    "quota_windows[{}].quota_limit must be positive, got: {}",
                    j,
                    window.quota_limit
                );
            }
            if !super::dimension::is_valid_mode(&window.quota_mode) {
                bail!(
                    "quota_windows[{}].quota_mode '{}' is unknown; available: tokens, requests, credits",
                    j,
                    window.quota_mode
                );
            }
        }
        for (g, group) in token.model_quota_groups.iter().enumerate() {
            for (j, window) in group.quota_windows.iter().enumerate() {
                parse_duration(&window.window).with_context(|| {
                    format!(
                        "model_quota_groups[{}].quota_windows[{}].window '{}' is invalid",
                        g, j, window.window
                    )
                })?;
                if window.quota_limit <= 0 {
                    bail!(
                        "model_quota_groups[{}].quota_windows[{}].quota_limit must be positive, got: {}",
                        g,
                        j,
                        window.quota_limit
                    );
                }
                if !super::dimension::is_valid_mode(&window.quota_mode) {
                    bail!(
                        "model_quota_groups[{}].quota_windows[{}].quota_mode '{}' is unknown; available: tokens, requests, credits",
                        g,
                        j,
                        window.quota_mode
                    );
                }
            }
        }

        // Validate expired_at if present
        if let Some(ref expired_at) = token.expired_at {
            chrono::DateTime::parse_from_rfc3339(expired_at)
                .with_context(|| format!("expired_at '{}' is not valid ISO 8601", expired_at))?;
        }

        Ok(token)
    }

    /// Load tokens from either a `tokens/` directory or a legacy `tokens.toml` file.
    ///
    /// Preference order:
    /// 1. If `tokens/` directory exists → load from directory
    /// 2. Else if `tokens.toml` file exists → load from file (legacy)
    /// 3. Else → open mode (no tokens)
    ///
    /// Returns `(config, source_description)` where source_description indicates
    /// where the tokens were loaded from (for logging).
    pub fn load_auto(config_dir: &Path) -> Result<(Self, String)> {
        let tokens_dir = config_dir.join("tokens");
        let tokens_file = config_dir.join("tokens.toml");

        // Conflict detection: both exist → error
        if tokens_dir.is_dir() && tokens_file.exists() {
            bail!(
                "conflicting token configuration sources: both tokens/ directory \
                 and tokens.toml file exist. Remove one or run: llm-proxy token migrate"
            );
        }

        // Prefer directory over file
        if tokens_dir.is_dir() {
            match Self::load_from_directory(&tokens_dir)? {
                Some(config) => {
                    let desc = format!("tokens/ directory ({} tokens)", config.tokens.len());
                    return Ok((config, desc));
                }
                None => {
                    // Directory doesn't exist (shouldn't happen since we checked is_dir)
                    unreachable!()
                }
            }
        }

        // Fall back to legacy file
        if tokens_file.exists() {
            let config = Self::load(&tokens_file)?;
            let desc = format!("tokens.toml ({} tokens)", config.tokens.len());
            return Ok((config, desc));
        }

        // Neither exists → open mode
        Ok((
            Self::default(),
            "open mode (no tokens configured)".to_string(),
        ))
    }
}

/// Parse a duration string like "30m", "5h", "1d", "7d", "30d", "1h30m"
/// Returns duration in seconds.
#[allow(dead_code)] // Used in later stages
pub fn parse_duration(s: &str) -> Result<i64> {
    let s = s.trim();
    if s.is_empty() {
        bail!("empty duration string");
    }

    let mut total_seconds: i64 = 0;
    let mut current_number: Option<i64> = None;

    for c in s.chars() {
        if c.is_ascii_digit() {
            current_number = Some(current_number.unwrap_or(0) * 10 + (c as i64 - '0' as i64));
        } else {
            let number = current_number.ok_or_else(|| {
                anyhow::anyhow!("expected number before unit '{}' in duration '{}'", c, s)
            })?;
            let multiplier = match c {
                's' => 1,
                'm' => 60,
                'h' => 3600,
                'd' => 86400,
                'w' => 604800,
                _ => bail!("unknown duration unit '{}' in '{}'", c, s),
            };
            total_seconds += number * multiplier;
            current_number = None;
        }
    }

    // Handle trailing number without unit (assume seconds)
    if let Some(number) = current_number {
        total_seconds += number;
    }

    if total_seconds <= 0 {
        bail!("duration must be positive, got: {}", s);
    }

    Ok(total_seconds)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_duration_basic() {
        assert_eq!(parse_duration("30s").unwrap(), 30);
        assert_eq!(parse_duration("5m").unwrap(), 300);
        assert_eq!(parse_duration("1h").unwrap(), 3600);
        assert_eq!(parse_duration("1d").unwrap(), 86400);
        assert_eq!(parse_duration("7d").unwrap(), 604800);
        assert_eq!(parse_duration("30d").unwrap(), 2592000);
    }

    #[test]
    fn parse_duration_compound() {
        assert_eq!(parse_duration("1h30m").unwrap(), 5400);
        assert_eq!(parse_duration("1d12h").unwrap(), 129600);
        assert_eq!(parse_duration("2h30m15s").unwrap(), 9015);
    }

    #[test]
    fn parse_duration_trailing_number() {
        // Trailing number without unit is treated as seconds
        assert_eq!(parse_duration("30").unwrap(), 30);
        assert_eq!(parse_duration("1h30").unwrap(), 3630);
    }

    #[test]
    fn parse_duration_invalid() {
        assert!(parse_duration("").is_err());
        assert!(parse_duration("m").is_err());
        assert!(parse_duration("5x").is_err());
        assert!(parse_duration("0s").is_err());
    }

    #[test]
    fn parse_empty_config() {
        let config = TokensConfig::parse("").unwrap();
        assert!(!config.is_enabled());
        assert!(config.tokens.is_empty());
    }

    #[test]
    fn parse_whitespace_only_config() {
        let config = TokensConfig::parse("   \n\n  ").unwrap();
        assert!(!config.is_enabled());
    }

    #[test]
    fn parse_minimal_token() {
        let content = r#"
[[tokens]]
key = "lp-abcdefghijklmnopqrstuvwxyz123456"
name = "test-token"
allowed_models = ["gpt-4o"]
"#;
        let config = TokensConfig::parse(content).unwrap();
        assert!(config.is_enabled());
        assert_eq!(config.tokens.len(), 1);
        assert_eq!(config.tokens[0].key, "lp-abcdefghijklmnopqrstuvwxyz123456");
        assert_eq!(config.tokens[0].name, "test-token");
        assert_eq!(config.tokens[0].allowed_models, vec!["gpt-4o"]);
        assert!(config.tokens[0].quota_windows.is_empty());
        assert!(config.tokens[0].expired_at.is_none());
    }

    #[test]
    fn parse_full_token() {
        let content = r#"
[[tokens]]
key = "lp-aBcDeFgHiJkLmNoPqRsTuVwX"
name = "alice-openai"
allowed_models = ["gpt-4o", "gpt-4o-mini"]
expired_at = "2026-12-31T23:59:59Z"

[[tokens.quota_windows]]
window = "5h"
quota_mode = "requests"
quota_limit = 60

[[tokens.quota_windows]]
window = "30d"
quota_mode = "tokens"
quota_limit = 5000000
"#;
        let config = TokensConfig::parse(content).unwrap();
        assert_eq!(config.tokens.len(), 1);
        let token = &config.tokens[0];
        assert_eq!(token.name, "alice-openai");
        assert_eq!(token.allowed_models, vec!["gpt-4o", "gpt-4o-mini"]);
        assert_eq!(token.quota_windows.len(), 2);
        assert_eq!(token.quota_windows[0].window, "5h");
        assert_eq!(token.quota_windows[0].quota_mode, "requests");
        assert_eq!(token.quota_windows[0].quota_limit, 60);
        assert_eq!(token.quota_windows[1].window, "30d");
        assert_eq!(token.quota_windows[1].quota_mode, "tokens");
        assert_eq!(token.quota_windows[1].quota_limit, 5000000);
        assert_eq!(token.expired_at, Some("2026-12-31T23:59:59Z".to_string()));
    }

    #[test]
    fn parse_multiple_tokens() {
        let content = r#"
[[tokens]]
key = "lp-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa11"
name = "token-a"
allowed_models = ["gpt-4o"]

[[tokens]]
key = "lp-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbb22"
name = "token-b"
allowed_models = ["claude-3-5-sonnet-20241022"]
"#;
        let config = TokensConfig::parse(content).unwrap();
        assert_eq!(config.tokens.len(), 2);
        assert_eq!(config.tokens[0].name, "token-a");
        assert_eq!(config.tokens[1].name, "token-b");
    }

    #[test]
    fn reject_invalid_key_prefix() {
        let content = r#"
[[tokens]]
key = "invalid-key"
name = "test"
allowed_models = []
"#;
        let err = TokensConfig::parse(content).unwrap_err();
        assert!(err.to_string().contains("must start with 'lp-'"));
    }

    #[test]
    fn reject_short_key() {
        let content = r#"
[[tokens]]
key = "lp-short"
name = "test"
allowed_models = []
"#;
        let err = TokensConfig::parse(content).unwrap_err();
        assert!(err.to_string().contains("too short"));
    }

    #[test]
    fn reject_duplicate_key() {
        let content = r#"
[[tokens]]
key = "lp-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa11"
name = "token-a"
allowed_models = []

[[tokens]]
key = "lp-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa11"
name = "token-b"
allowed_models = []
"#;
        let err = TokensConfig::parse(content).unwrap_err();
        assert!(err.to_string().contains("duplicate token key"));
    }

    #[test]
    fn reject_duplicate_name() {
        let content = r#"
[[tokens]]
key = "lp-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa11"
name = "same-name"
allowed_models = []

[[tokens]]
key = "lp-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbb22"
name = "same-name"
allowed_models = []
"#;
        let err = TokensConfig::parse(content).unwrap_err();
        assert!(err.to_string().contains("duplicate token name"));
    }

    #[test]
    fn reject_empty_name() {
        let content = r#"
[[tokens]]
key = "lp-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa11"
name = ""
allowed_models = []
"#;
        let err = TokensConfig::parse(content).unwrap_err();
        assert!(err.to_string().contains("name cannot be empty"));
    }

    #[test]
    fn reject_invalid_window_duration() {
        let content = r#"
[[tokens]]
key = "lp-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa11"
name = "test"
allowed_models = []

[[tokens.quota_windows]]
window = "invalid"
quota_mode = "requests"
quota_limit = 100
"#;
        let err = TokensConfig::parse(content).unwrap_err();
        assert!(err.to_string().contains("invalid"));
    }

    #[test]
    fn reject_non_positive_quota_limit() {
        let content = r#"
[[tokens]]
key = "lp-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa11"
name = "test"
allowed_models = []

[[tokens.quota_windows]]
window = "1h"
quota_mode = "requests"
quota_limit = 0
"#;
        let err = TokensConfig::parse(content).unwrap_err();
        assert!(err.to_string().contains("must be positive"));
    }

    #[test]
    fn reject_invalid_expired_at() {
        let content = r#"
[[tokens]]
key = "lp-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa11"
name = "test"
allowed_models = []
expired_at = "not-a-date"
"#;
        let err = TokensConfig::parse(content).unwrap_err();
        assert!(err.to_string().contains("not valid ISO 8601"));
    }

    #[test]
    fn find_by_key_works() {
        let content = r#"
[[tokens]]
key = "lp-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa11"
name = "token-a"
allowed_models = []

[[tokens]]
key = "lp-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbb22"
name = "token-b"
allowed_models = []
"#;
        let config = TokensConfig::parse(content).unwrap();
        assert!(
            config
                .find_by_key("lp-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa11")
                .is_some()
        );
        assert!(
            config
                .find_by_key("lp-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbb22")
                .is_some()
        );
        assert!(config.find_by_key("nonexistent").is_none());
    }

    // ── : model_quota_groups tests ──────────────────────────────

    #[test]
    fn parse_token_with_model_quota_groups() {
        let content = r#"
[[tokens]]
key = "lp-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa11"
name = "lyc"
allowed_models = ["deepseek-v3", "mimo", "gpt-4o"]

[[tokens.model_quota_groups]]
models = ["deepseek-v3", "mimo"]

  [[tokens.model_quota_groups.quota_windows]]
  window = "5h"
  quota_mode = "tokens"
  quota_limit = 10000000

  [[tokens.model_quota_groups.quota_windows]]
  window = "30d"
  quota_mode = "tokens"
  quota_limit = 200000000

[[tokens.model_quota_groups]]
models = ["deepseek-v3"]

  [[tokens.model_quota_groups.quota_windows]]
  window = "5h"
  quota_mode = "tokens"
  quota_limit = 5000000

[[tokens.model_quota_groups]]
models = ["gpt-4o"]

  [[tokens.model_quota_groups.quota_windows]]
  window = "5h"
  quota_mode = "requests"
  quota_limit = 800
"#;
        let config = TokensConfig::parse(content).unwrap();
        assert_eq!(config.tokens.len(), 1);
        let token = &config.tokens[0];
        assert_eq!(token.model_quota_groups.len(), 3);

        // Group 0: deepseek-v3 + mimo
        assert_eq!(
            token.model_quota_groups[0].models,
            vec!["deepseek-v3", "mimo"]
        );
        assert_eq!(token.model_quota_groups[0].quota_windows.len(), 2);
        assert_eq!(
            token.model_quota_groups[0].quota_windows[0].quota_limit,
            10000000
        );

        // Group 1: deepseek-v3 only
        assert_eq!(token.model_quota_groups[1].models, vec!["deepseek-v3"]);
        assert_eq!(
            token.model_quota_groups[1].quota_windows[0].quota_limit,
            5000000
        );

        // Group 2: gpt-4o only
        assert_eq!(token.model_quota_groups[2].models, vec!["gpt-4o"]);
        assert_eq!(
            token.model_quota_groups[2].quota_windows[0].quota_mode,
            "requests"
        );
    }

    #[test]
    fn parse_token_with_global_quota_group() {
        // A group with empty `models` applies to all models
        let content = r#"
[[tokens]]
key = "lp-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa11"
name = "test"
allowed_models = ["gpt-4o", "claude-sonnet"]

[[tokens.model_quota_groups]]
# no models = all models

  [[tokens.model_quota_groups.quota_windows]]
  window = "1d"
  quota_mode = "requests"
  quota_limit = 5000
"#;
        let config = TokensConfig::parse(content).unwrap();
        let token = &config.tokens[0];
        assert_eq!(token.model_quota_groups.len(), 1);
        assert!(token.model_quota_groups[0].models.is_empty());
        assert!(token.model_quota_groups[0].matches_model("gpt-4o"));
        assert!(token.model_quota_groups[0].matches_model("anything"));
    }

    #[test]
    fn effective_windows_for_model_with_groups() {
        let content = r#"
[[tokens]]
key = "lp-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa11"
name = "lyc"
allowed_models = ["deepseek-v3", "mimo", "gpt-4o"]

[[tokens.model_quota_groups]]
models = ["deepseek-v3", "mimo"]

  [[tokens.model_quota_groups.quota_windows]]
  window = "5h"
  quota_mode = "tokens"
  quota_limit = 10000000

[[tokens.model_quota_groups]]
models = ["deepseek-v3"]

  [[tokens.model_quota_groups.quota_windows]]
  window = "5h"
  quota_mode = "tokens"
  quota_limit = 5000000

[[tokens.model_quota_groups]]
# global ceiling
  [[tokens.model_quota_groups.quota_windows]]
  window = "1d"
  quota_mode = "requests"
  quota_limit = 5000
"#;
        let config = TokensConfig::parse(content).unwrap();
        let token = &config.tokens[0];

        // deepseek-v3 matches all 3 groups
        let ds_windows = token.effective_windows_for_model("deepseek-v3");
        assert_eq!(ds_windows.len(), 3, "deepseek-v3 should match all 3 groups");

        // mimo matches group 0 and the global group
        let mimo_windows = token.effective_windows_for_model("mimo");
        assert_eq!(mimo_windows.len(), 2, "mimo should match 2 groups");

        // gpt-4o matches only the global group
        let gpt_windows = token.effective_windows_for_model("gpt-4o");
        assert_eq!(
            gpt_windows.len(),
            1,
            "gpt-4o should match only the global group"
        );

        // unknown model matches only the global group
        let unknown_windows = token.effective_windows_for_model("unknown-model");
        assert_eq!(
            unknown_windows.len(),
            1,
            "unknown model should match only the global group"
        );
    }

    #[test]
    fn effective_windows_falls_back_to_legacy_when_no_groups() {
        let content = r#"
[[tokens]]
key = "lp-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa11"
name = "legacy"
allowed_models = ["gpt-4o"]

[[tokens.quota_windows]]
window = "5h"
quota_mode = "requests"
quota_limit = 60

[[tokens.quota_windows]]
window = "30d"
quota_mode = "tokens"
quota_limit = 5000000
"#;
        let config = TokensConfig::parse(content).unwrap();
        let token = &config.tokens[0];

        // With no model_quota_groups, effective_windows_for_model returns legacy quota_windows
        let windows = token.effective_windows_for_model("gpt-4o");
        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0].quota_limit, 60);
        assert_eq!(windows[1].quota_limit, 5000000);
    }

    #[test]
    fn all_quota_windows_returns_all_groups() {
        let content = r#"
[[tokens]]
key = "lp-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa11"
name = "test"
allowed_models = []

[[tokens.model_quota_groups]]
models = ["a"]
  [[tokens.model_quota_groups.quota_windows]]
  window = "1h"
  quota_mode = "requests"
  quota_limit = 10

[[tokens.model_quota_groups]]
models = ["b"]
  [[tokens.model_quota_groups.quota_windows]]
  window = "1d"
  quota_mode = "tokens"
  quota_limit = 1000
  [[tokens.model_quota_groups.quota_windows]]
  window = "1h"
  quota_mode = "tokens"
  quota_limit = 500
"#;
        let config = TokensConfig::parse(content).unwrap();
        let token = &config.tokens[0];

        // all_quota_windows returns all windows across all groups
        let all = token.all_quota_windows();
        assert_eq!(all.len(), 3);
    }

    #[test]
    fn model_quota_group_canonical_id() {
        let group1 = ModelQuotaGroup {
            models: vec!["b".to_string(), "a".to_string()],
            quota_windows: vec![],
        };
        let group2 = ModelQuotaGroup {
            models: vec!["a".to_string(), "b".to_string()],
            quota_windows: vec![],
        };
        // Same models in different order → same canonical ID
        assert_eq!(group1.canonical_id(), group2.canonical_id());
        assert_eq!(group1.canonical_id(), "a,b");

        // Empty models → empty string
        let global = ModelQuotaGroup {
            models: vec![],
            quota_windows: vec![],
        };
        assert_eq!(global.canonical_id(), "");
    }

    // ── : directory loading tests ───────────────────────────────

    #[test]
    fn load_from_directory_not_found_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let tokens_dir = dir.path().join("tokens");
        // Directory doesn't exist
        let result = TokensConfig::load_from_directory(&tokens_dir).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn load_from_directory_empty_returns_default() {
        let dir = tempfile::tempdir().unwrap();
        let tokens_dir = dir.path().join("tokens");
        std::fs::create_dir(&tokens_dir).unwrap();
        // Directory exists but is empty
        let result = TokensConfig::load_from_directory(&tokens_dir).unwrap();
        assert!(result.is_some());
        assert!(result.unwrap().tokens.is_empty());
    }

    #[test]
    fn load_from_directory_single_file() {
        let dir = tempfile::tempdir().unwrap();
        let tokens_dir = dir.path().join("tokens");
        std::fs::create_dir(&tokens_dir).unwrap();

        let token_content = r#"
key = "lp-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa11"
name = "alice"
allowed_models = ["gpt-4o"]

[[model_quota_groups]]
models = ["gpt-4o"]

  [[model_quota_groups.quota_windows]]
  window = "5h"
  quota_mode = "requests"
  quota_limit = 60
"#;
        std::fs::write(tokens_dir.join("alice.toml"), token_content).unwrap();

        let result = TokensConfig::load_from_directory(&tokens_dir).unwrap();
        assert!(result.is_some());
        let config = result.unwrap();
        assert_eq!(config.tokens.len(), 1);
        assert_eq!(config.tokens[0].name, "alice");
        assert_eq!(config.tokens[0].model_quota_groups.len(), 1);
    }

    #[test]
    fn load_from_directory_multiple_files() {
        let dir = tempfile::tempdir().unwrap();
        let tokens_dir = dir.path().join("tokens");
        std::fs::create_dir(&tokens_dir).unwrap();

        let alice = r#"
key = "lp-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa11"
name = "alice"
allowed_models = ["gpt-4o"]
"#;
        let bob = r#"
key = "lp-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbb22"
name = "bob"
allowed_models = ["claude-sonnet"]
"#;
        std::fs::write(tokens_dir.join("alice.toml"), alice).unwrap();
        std::fs::write(tokens_dir.join("bob.toml"), bob).unwrap();

        let result = TokensConfig::load_from_directory(&tokens_dir).unwrap();
        let config = result.unwrap();
        assert_eq!(config.tokens.len(), 2);
        // Files are loaded in sorted order
        assert_eq!(config.tokens[0].name, "alice");
        assert_eq!(config.tokens[1].name, "bob");
    }

    #[test]
    fn load_from_directory_duplicate_key_across_files() {
        let dir = tempfile::tempdir().unwrap();
        let tokens_dir = dir.path().join("tokens");
        std::fs::create_dir(&tokens_dir).unwrap();

        let alice = r#"
key = "lp-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa11"
name = "alice"
allowed_models = []
"#;
        let alice_dup = r#"
key = "lp-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa11"
name = "alice-dup"
allowed_models = []
"#;
        std::fs::write(tokens_dir.join("alice.toml"), alice).unwrap();
        std::fs::write(tokens_dir.join("alice-dup.toml"), alice_dup).unwrap();

        let result = TokensConfig::load_from_directory(&tokens_dir);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("duplicate token key")
        );
    }

    #[test]
    fn load_from_directory_duplicate_name_across_files() {
        let dir = tempfile::tempdir().unwrap();
        let tokens_dir = dir.path().join("tokens");
        std::fs::create_dir(&tokens_dir).unwrap();

        let alice1 = r#"
key = "lp-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa11"
name = "same-name"
allowed_models = []
"#;
        let alice2 = r#"
key = "lp-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbb22"
name = "same-name"
allowed_models = []
"#;
        std::fs::write(tokens_dir.join("a.toml"), alice1).unwrap();
        std::fs::write(tokens_dir.join("b.toml"), alice2).unwrap();

        let result = TokensConfig::load_from_directory(&tokens_dir);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("duplicate token name")
        );
    }

    #[test]
    fn load_from_directory_parse_error_collected() {
        let dir = tempfile::tempdir().unwrap();
        let tokens_dir = dir.path().join("tokens");
        std::fs::create_dir(&tokens_dir).unwrap();

        let good = r#"
key = "lp-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa11"
name = "good"
allowed_models = []
"#;
        let bad = r#"
key = "invalid-no-lp-prefix"
name = "bad"
allowed_models = []
"#;
        std::fs::write(tokens_dir.join("good.toml"), good).unwrap();
        std::fs::write(tokens_dir.join("bad.toml"), bad).unwrap();

        let result = TokensConfig::load_from_directory(&tokens_dir);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("bad.toml"),
            "error should mention the bad file: {err}"
        );
        assert!(
            err.contains("lp-"),
            "error should mention the validation failure: {err}"
        );
        assert!(
            err.contains("1 other token file(s) loaded successfully"),
            "should report success count: {err}"
        );
    }

    #[test]
    fn load_from_directory_ignores_non_toml_files() {
        let dir = tempfile::tempdir().unwrap();
        let tokens_dir = dir.path().join("tokens");
        std::fs::create_dir(&tokens_dir).unwrap();

        let alice = r#"
key = "lp-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa11"
name = "alice"
allowed_models = []
"#;
        std::fs::write(tokens_dir.join("alice.toml"), alice).unwrap();
        std::fs::write(tokens_dir.join("README.md"), "# tokens").unwrap();
        std::fs::write(tokens_dir.join(".hidden"), "ignored").unwrap();

        let result = TokensConfig::load_from_directory(&tokens_dir).unwrap();
        let config = result.unwrap();
        assert_eq!(config.tokens.len(), 1, "only .toml files should be loaded");
    }

    #[test]
    fn load_auto_loads_from_directory() {
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path();

        // Create tokens/ directory only (no tokens.toml)
        let tokens_dir = config_dir.join("tokens");
        std::fs::create_dir(&tokens_dir).unwrap();

        let dir_token = r#"
key = "lp-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa11"
name = "from-dir"
allowed_models = []
"#;
        std::fs::write(tokens_dir.join("alice.toml"), dir_token).unwrap();

        let (config, desc) = TokensConfig::load_auto(config_dir).unwrap();
        assert_eq!(config.tokens.len(), 1);
        assert_eq!(config.tokens[0].name, "from-dir");
        assert!(
            desc.contains("directory"),
            "should indicate directory source: {desc}"
        );
    }

    #[test]
    fn load_auto_falls_back_to_legacy_file() {
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path();

        // Only tokens.toml, no tokens/ directory
        let file_content = r#"
[[tokens]]
key = "lp-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbb22"
name = "from-file"
allowed_models = []
"#;
        std::fs::write(config_dir.join("tokens.toml"), file_content).unwrap();

        let (config, desc) = TokensConfig::load_auto(config_dir).unwrap();
        assert_eq!(config.tokens.len(), 1);
        assert_eq!(config.tokens[0].name, "from-file");
        assert!(
            desc.contains("tokens.toml"),
            "should indicate file source: {desc}"
        );
    }

    #[test]
    fn load_auto_open_mode_when_neither_exists() {
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path();

        let (config, desc) = TokensConfig::load_auto(config_dir).unwrap();
        assert!(!config.is_enabled());
        assert!(
            desc.contains("open mode"),
            "should indicate open mode: {desc}"
        );
    }

    #[test]
    fn load_auto_errors_when_both_directory_and_file_exist() {
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path();

        // Create tokens/ directory with a valid token file
        let tokens_dir = config_dir.join("tokens");
        std::fs::create_dir(&tokens_dir).unwrap();
        let token_content = r#"
key = "lp-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa11"
name = "test"
allowed_models = []
"#;
        std::fs::write(tokens_dir.join("test.toml"), token_content).unwrap();

        // Create tokens.toml file
        let legacy_content = r#"
[[tokens]]
key = "lp-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbb22"
name = "legacy"
allowed_models = []
"#;
        std::fs::write(config_dir.join("tokens.toml"), legacy_content).unwrap();

        // Should error with conflict message
        let result = TokensConfig::load_auto(config_dir);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("conflicting token configuration sources"),
            "error should mention conflict: {err_msg}"
        );
        assert!(
            err_msg.contains("tokens/ directory") && err_msg.contains("tokens.toml file"),
            "error should mention both sources: {err_msg}"
        );
    }

    #[test]
    fn validate_rejects_invalid_group_window() {
        let content = r#"
[[tokens]]
key = "lp-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa11"
name = "test"
allowed_models = []

[[tokens.model_quota_groups]]
models = ["gpt-4o"]

  [[tokens.model_quota_groups.quota_windows]]
  window = "invalid"
  quota_mode = "requests"
  quota_limit = 100
"#;
        let err = TokensConfig::parse(content).unwrap_err();
        assert!(err.to_string().contains("invalid"));
    }

    #[test]
    fn validate_rejects_non_positive_group_quota_limit() {
        let content = r#"
[[tokens]]
key = "lp-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa11"
name = "test"
allowed_models = []

[[tokens.model_quota_groups]]
models = ["gpt-4o"]

  [[tokens.model_quota_groups.quota_windows]]
  window = "1h"
  quota_mode = "requests"
  quota_limit = 0
"#;
        let err = TokensConfig::parse(content).unwrap_err();
        assert!(err.to_string().contains("must be positive"));
    }

    // ── : unknown quota_mode rejected ─────────────────────

    #[test]
    fn validate_rejects_unknown_quota_mode_in_legacy_windows() {
        let content = r#"
[[tokens]]
key = "lp-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa11"
name = "test"
allowed_models = []

[[tokens.quota_windows]]
window = "1h"
quota_mode = "unknown"
quota_limit = 100
"#;
        let err = TokensConfig::parse(content).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("unknown"),
            "error should mention 'unknown': {msg}"
        );
        assert!(
            msg.contains("tokens") && msg.contains("requests") && msg.contains("credits"),
            "error should list available modes: {msg}"
        );
    }

    #[test]
    fn validate_rejects_unknown_quota_mode_in_group_windows() {
        let content = r#"
[[tokens]]
key = "lp-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa11"
name = "test"
allowed_models = []

[[tokens.model_quota_groups]]
models = ["gpt-4o"]

  [[tokens.model_quota_groups.quota_windows]]
  window = "1h"
  quota_mode = "bananas"
  quota_limit = 100
"#;
        let err = TokensConfig::parse(content).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("unknown"),
            "error should mention 'unknown': {msg}"
        );
        assert!(
            msg.contains("tokens") && msg.contains("requests") && msg.contains("credits"),
            "error should list available modes: {msg}"
        );
    }
}
