use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

pub(super) const OPENAI_DEVICE_AUTH_URL: &str =
    "https://auth.openai.com/api/accounts/deviceauth/usercode";
pub(super) const OPENAI_DEVICE_TOKEN_URL: &str =
    "https://auth.openai.com/api/accounts/deviceauth/token";
pub(super) const OPENAI_OAUTH_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
pub(super) const OPENAI_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub(super) const ANTIGRAVITY_AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
pub(super) const ANTIGRAVITY_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
pub(super) const ANTIGRAVITY_CLIENT_ID: &str =
    "1071006060591-tmhssin2h21lcre235vtolojh4g403ep.apps.googleusercontent.com";
pub(super) const ANTIGRAVITY_CLIENT_SECRET: &str = "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";
pub(super) const ANTIGRAVITY_REDIRECT_URI: &str = "https://antigravity.google/oauth-callback";
pub(super) const ANTIGRAVITY_USERINFO_URL: &str =
    "https://www.googleapis.com/oauth2/v2/userinfo?alt=json";
pub(super) const ANTIGRAVITY_LOAD_CODE_ASSIST_URL: &str =
    "https://cloudcode-pa.googleapis.com/v1internal:loadCodeAssist";
pub(super) const ANTIGRAVITY_ONBOARD_USER_URL: &str =
    "https://daily-cloudcode-pa.googleapis.com/v1internal:onboardUser";

// ============================================================================
// 新 OAuth 账号存储结构（按类型分组）
// ============================================================================

/// 顶层 OAuth 账号存储结构
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct OAuthAccounts {
    pub version: u32,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub antigravity: HashMap<String, AntigravityAccount>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub openai: HashMap<String, OpenaiAccount>,
}

/// Antigravity OAuth 账号
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AntigravityAccount {
    pub account_label: String,
    pub project_id: String,
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at_unix: i64,
    pub updated_at_unix: i64,
}

/// OpenAI OAuth 账号
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenaiAccount {
    pub account_label: String,
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at_unix: i64,
    pub updated_at_unix: i64,
}

impl AntigravityAccount {
    pub fn is_expired(&self) -> bool {
        let now = unix_secs() as i64;
        self.expires_at_unix <= now
    }
}

impl OpenaiAccount {
    pub fn is_expired(&self) -> bool {
        let now = unix_secs() as i64;
        self.expires_at_unix <= now
    }
}

impl OAuthAccounts {
    pub fn new() -> Self {
        Self {
            version: 1,
            antigravity: HashMap::new(),
            openai: HashMap::new(),
        }
    }
}

/// 被跳过的无效账号信息
#[derive(Debug, Clone)]
pub struct SkippedAccount {
    pub account_type: String, // "antigravity" or "openai"
    pub account_id: String,
    pub reason: String,
}

pub(super) fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}
