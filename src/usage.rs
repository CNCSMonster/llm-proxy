use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::path::Path;

use crate::auth;
use crate::config::{AuthConfig, Config};

const CHATGPT_BACKEND_BASE: &str = "https://chatgpt.com/backend-api";

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct UsageStatus {
    pub plan_type: String,
    pub rate_limit: Option<RateLimitInfo>,
    pub reset_credits_available: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct RateLimitInfo {
    pub allowed: Option<bool>,
    pub limit_reached: bool,
    pub primary_window: Option<WindowSnapshot>,
    pub secondary_window: Option<WindowSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct WindowSnapshot {
    pub used_percent: i32,
    pub reset_after_seconds: Option<i32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetConfirmLevel {
    None,
    Confirm,
    ConfirmWarn,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsumeResult {
    Reset,
    NothingToReset,
    NoCredit,
    AlreadyRedeemed,
    Unknown,
}

impl UsageStatus {
    pub fn from_json(value: Value) -> Self {
        let rate_limit = value.get("rate_limit").and_then(RateLimitInfo::from_json);
        let reset_credits_available = value
            .get("rate_limit_reset_credits")
            .and_then(|v| v.get("available_count"))
            .and_then(Value::as_i64);
        Self {
            plan_type: value
                .get("plan_type")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string(),
            rate_limit,
            reset_credits_available,
        }
    }

    pub fn reset_confirm_level(&self) -> ResetConfirmLevel {
        if self
            .rate_limit
            .as_ref()
            .is_some_and(|rate| rate.limit_reached)
        {
            return ResetConfirmLevel::None;
        }
        let any_low_usage = self.rate_limit.as_ref().is_some_and(|rate| {
            rate.primary_window
                .as_ref()
                .is_some_and(|w| w.used_percent < 50)
                || rate
                    .secondary_window
                    .as_ref()
                    .is_some_and(|w| w.used_percent < 50)
        });
        if any_low_usage {
            ResetConfirmLevel::ConfirmWarn
        } else {
            ResetConfirmLevel::Confirm
        }
    }
}

impl RateLimitInfo {
    fn from_json(value: &Value) -> Option<Self> {
        Some(Self {
            allowed: value.get("allowed").and_then(Value::as_bool),
            limit_reached: value
                .get("limit_reached")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            primary_window: value
                .get("primary_window")
                .and_then(WindowSnapshot::from_json),
            secondary_window: value
                .get("secondary_window")
                .and_then(WindowSnapshot::from_json),
        })
    }
}

impl WindowSnapshot {
    fn from_json(value: &Value) -> Option<Self> {
        Some(Self {
            used_percent: value.get("used_percent")?.as_i64()? as i32,
            reset_after_seconds: value
                .get("reset_after_seconds")
                .and_then(Value::as_i64)
                .map(|v| v as i32),
        })
    }
}

impl ConsumeResult {
    pub fn from_json(value: Value) -> Self {
        match value
            .get("code")
            .and_then(Value::as_str)
            .unwrap_or_default()
        {
            "reset" => Self::Reset,
            "nothing_to_reset" => Self::NothingToReset,
            "no_credit" => Self::NoCredit,
            "already_redeemed" => Self::AlreadyRedeemed,
            _ => Self::Unknown,
        }
    }
}

pub fn resolve_openai_token(cfg: &Config, auth_path: &Path, provider_id: &str) -> Result<String> {
    let provider = cfg
        .providers
        .get(provider_id)
        .with_context(|| format!("unknown provider {provider_id:?}"))?;
    let account = match provider.auth_config(provider_id)? {
        AuthConfig::OpenaiOauth { account } => account.unwrap_or_else(|| provider_id.to_string()),
        AuthConfig::ApiKeyEnv { .. } | AuthConfig::None | AuthConfig::AntigravityOauth { .. } => {
            bail!("Provider {provider_id:?} does not support usage reset.")
        }
    };

    auth::get_openai_token(auth_path, &account, provider_id)
}

pub async fn query_usage(token: &str) -> Result<UsageStatus> {
    let resp = reqwest::Client::new()
        .get(format!("{CHATGPT_BACKEND_BASE}/wham/usage"))
        .bearer_auth(token)
        .send()
        .await?
        .error_for_status()?;
    let payload: Value = resp.json().await?;
    Ok(UsageStatus::from_json(payload))
}

pub async fn consume_reset(token: &str) -> Result<ConsumeResult> {
    let resp = reqwest::Client::new()
        .post(format!(
            "{CHATGPT_BACKEND_BASE}/wham/rate-limit-reset-credits/consume"
        ))
        .bearer_auth(token)
        .json(&serde_json::json!({ "redeem_request_id": redeem_request_id() }))
        .send()
        .await?
        .error_for_status()?;
    let payload: Value = resp.json().await?;
    Ok(ConsumeResult::from_json(payload))
}

fn redeem_request_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{EndpointConfig, ProviderConfig, ServerConfig};

    #[test]
    fn parses_usage_and_confirmation_levels() {
        let used_up = UsageStatus::from_json(serde_json::json!({
            "plan_type": "pro",
            "rate_limit": {
                "allowed": false,
                "limit_reached": true,
                "primary_window": {"used_percent": 100, "reset_after_seconds": 100}
            },
            "rate_limit_reset_credits": {"available_count": 2}
        }));
        assert_eq!(used_up.plan_type, "pro");
        assert_eq!(used_up.reset_credits_available, Some(2));
        assert_eq!(used_up.reset_confirm_level(), ResetConfirmLevel::None);

        let low = UsageStatus::from_json(serde_json::json!({
            "rate_limit": {"primary_window": {"used_percent": 20}}
        }));
        assert_eq!(low.reset_confirm_level(), ResetConfirmLevel::ConfirmWarn);

        let normal = UsageStatus::from_json(serde_json::json!({
            "rate_limit": {"primary_window": {"used_percent": 70}}
        }));
        assert_eq!(normal.reset_confirm_level(), ResetConfirmLevel::Confirm);
    }

    #[test]
    fn parses_consume_results() {
        assert_eq!(
            ConsumeResult::from_json(serde_json::json!({"code":"reset"})),
            ConsumeResult::Reset
        );
        assert_eq!(
            ConsumeResult::from_json(serde_json::json!({"code":"nothing_to_reset"})),
            ConsumeResult::NothingToReset
        );
        assert_eq!(
            ConsumeResult::from_json(serde_json::json!({"code":"no_credit"})),
            ConsumeResult::NoCredit
        );
        assert_eq!(
            ConsumeResult::from_json(serde_json::json!({"code":"already_redeemed"})),
            ConsumeResult::AlreadyRedeemed
        );
    }

    #[test]
    fn resolves_openai_oauth_credential_and_rejects_unsupported_provider() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("oauth_accounts.json");
        let cfg = Config {
            server: ServerConfig {
                listen: "127.0.0.1:8989".to_string(),
                usage: Default::default(),
                max_sse_buffer_bytes: crate::config::default_max_sse_buffer_bytes(),
                max_output_items: crate::config::default_max_output_items(),
            },
            fallback: Default::default(),
            protection: Default::default(),
            status: Default::default(),
            providers: [
                (
                    "openai-subscription".to_string(),
                    ProviderConfig {
                        auth: Some(AuthConfig::OpenaiOauth {
                            account: Some("acct".to_string()),
                        }),
                        openai_responses: Some(EndpointConfig::native(
                            "https://chatgpt.com/backend-api/codex/responses",
                        )),
                        ..ProviderConfig::default()
                    },
                ),
                (
                    "deepseek".to_string(),
                    ProviderConfig {
                        api_key_env: Some("DEEPSEEK_API_KEY".to_string()),
                        openai_chat: Some(EndpointConfig::native(
                            "https://api.deepseek.com/chat/completions",
                        )),
                        ..ProviderConfig::default()
                    },
                ),
            ]
            .into_iter()
            .collect(),
            models: Default::default(),
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time")
            .as_secs() as i64;
        let mut accounts = auth::OAuthAccounts::new();
        accounts.openai.insert(
            "acct".to_string(),
            auth::OpenaiAccount {
                account_label: "user@example.com".to_string(),
                access_token: "secret-access-token-1234567890".to_string(),
                refresh_token: "secret-refresh-token-1234567890".to_string(),
                expires_at_unix: now + 3600,
                updated_at_unix: now,
            },
        );
        auth::save_oauth_accounts(&path, &accounts).expect("write");
        assert_eq!(
            resolve_openai_token(&cfg, &path, "openai-subscription").expect("token"),
            "secret-access-token-1234567890"
        );
        let err = resolve_openai_token(&cfg, &path, "deepseek").expect_err("unsupported");
        assert!(err.to_string().contains("does not support usage reset"));
    }
}
