//! OpenAI 账号 schema（上游常量 + 账号结构体 + 校验）。
//! 原位：`auth/types.rs`（常量与结构体/impl）、`auth/storage.rs`（校验）。
//! openai/ 无独立 validate 模块，校验随 schema 就位；
//! 可见性机械适配：`pub(super)`（原 parent=auth）→ `pub(in crate::auth)`。

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::auth::types::unix_secs;

pub(in crate::auth) const OPENAI_DEVICE_AUTH_URL: &str =
    "https://auth.openai.com/api/accounts/deviceauth/usercode";
pub(in crate::auth) const OPENAI_DEVICE_TOKEN_URL: &str =
    "https://auth.openai.com/api/accounts/deviceauth/token";
pub(crate) const OPENAI_OAUTH_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
pub(in crate::auth) const OPENAI_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";

/// OpenAI OAuth 账号
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenaiAccount {
    pub account_label: String,
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at_unix: i64,
    pub updated_at_unix: i64,
}

impl OpenaiAccount {
    pub fn is_expired(&self) -> bool {
        let now = unix_secs() as i64;
        self.expires_at_unix <= now
    }
}

/// 验证单个 OpenAI 账号
pub(in crate::auth) fn validate_openai_account(id: &str, acc: &OpenaiAccount) -> Result<()> {
    // Account ID 格式
    if id.is_empty() {
        bail!("account ID is empty");
    }
    if !id
        .chars()
        .all(|c| c.is_alphanumeric() || c == '_' || c == '-')
    {
        bail!("invalid account ID format");
    }
    if id.len() > 64 {
        bail!("account ID too long");
    }

    // Token 不能相同
    if acc.access_token == acc.refresh_token {
        bail!("access_token and refresh_token are identical");
    }

    // Token 长度
    if acc.access_token.len() < 20 {
        bail!("access_token too short");
    }
    if acc.refresh_token.len() < 20 {
        bail!("refresh_token too short");
    }

    // 时间戳合理性
    if acc.expires_at_unix < 1000000000 {
        bail!("invalid expires_at_unix");
    }
    if acc.updated_at_unix < 1000000000 {
        bail!("invalid updated_at_unix");
    }
    if acc.updated_at_unix > acc.expires_at_unix {
        bail!("updated_at_unix > expires_at_unix");
    }

    Ok(())
}
