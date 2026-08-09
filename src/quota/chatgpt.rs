//! ChatGPT Subscription / Codex 额度查询客户端。
//!
//! 端点：`GET {backend}/wham/usage`（调研见 `docs/research/quota-api-research.md` §1，
//! 实验验证 2026-08-05，plan_type: plus，used_percent: 50，7 天窗口）。
//! 认证：OAuth Bearer token + `ChatGPT-Account-ID` header。
//! 账号 ID 从 JWT payload 的 `https://api.openai.com/auth` → `chatgpt_account_id` 提取
//! （参考 `docs/research/providers/provider-chatgpt-subscription.md` §3.2）。

use anyhow::{Context, Result};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use reqwest::Client;
use serde_json::Value;

use super::types::{QuotaInfo, unix_secs};

/// ChatGPT 后端根地址（生产）。`/wham/usage` 挂在其下。
pub const CHATGPT_BACKEND_URL: &str = "https://chatgpt.com/backend-api";

/// 从 OAuth access token（JWT）payload 中提取 ChatGPT 账号 ID
/// （`https://api.openai.com/auth` → `chatgpt_account_id`），用作
/// `ChatGPT-Account-ID` header 值。
///
/// JWT 格式严格校验：必须是 3 段（header.payload.signature），用 `.` 分隔。
/// payload 段使用 base64url 编码（RFC 4648 §5），解码后必须是有效 JSON。
pub fn chatgpt_account_id_from_jwt(token: &str) -> Result<String> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        anyhow::bail!(
            "invalid JWT format: expected 3 parts (header.payload.signature), got {}",
            parts.len()
        );
    }

    // JWT 使用 base64url 编码（RFC 4648 §5），URL_SAFE_NO_PAD 已处理
    let payload_bytes = URL_SAFE_NO_PAD
        .decode(parts[1].trim_end_matches('='))
        .context("JWT payload is not valid base64url encoding")?;

    let value: Value =
        serde_json::from_slice(&payload_bytes).context("JWT payload is not valid JSON")?;

    let account_id = value
        .get("https://api.openai.com/auth")
        .and_then(|auth| auth.get("chatgpt_account_id"))
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .context("JWT payload has no chatgpt_account_id under https://api.openai.com/auth")?;

    Ok(account_id.to_string())
}

/// 查询 ChatGPT Subscription 额度。
///
/// - `backend`：后端根地址（测试可传本地 mock 服务器地址）
/// - `account_id`：`ChatGPT-Account-ID` header 值（从 JWT 提取）
///
/// 响应结构（字段均为可选，私有 API 可能变更，防御式解析）：
/// ```json
/// {
///   "plan_type": "plus",
///   "rate_limit": {
///     "primary_window": {
///       "used_percent": 50,
///       "limit_window_seconds": 604800,
///       "reset_after_seconds": 316632,
///       "reset_at": 1786184630
///     }
///   }
/// }
/// ```
pub async fn fetch_chatgpt_quota(
    client: &Client,
    backend: &str,
    token: &str,
    account_id: &str,
    provider_id: &str,
) -> Result<QuotaInfo> {
    let url = format!("{backend}/wham/usage");
    let resp = client
        .get(&url)
        .bearer_auth(token)
        .header("ChatGPT-Account-ID", account_id)
        .send()
        .await
        .with_context(|| format!("quota request to {url} failed"))?
        .error_for_status()
        .with_context(|| format!("quota request to {url} returned an error status"))?
        .json::<Value>()
        .await
        .context("quota response is not valid JSON")?;

    let plan_type = resp
        .get("plan_type")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string);
    let primary = resp
        .get("rate_limit")
        .and_then(|rl| rl.get("primary_window"));
    Ok(QuotaInfo {
        provider_id: provider_id.to_string(),
        plan_type,
        used_percent: primary
            .and_then(|w| w.get("used_percent"))
            .and_then(Value::as_f64),
        limit_window_seconds: primary
            .and_then(|w| w.get("limit_window_seconds"))
            .and_then(Value::as_i64),
        reset_after_seconds: primary
            .and_then(|w| w.get("reset_after_seconds"))
            .and_then(Value::as_i64),
        reset_at_unix: primary
            .and_then(|w| w.get("reset_at"))
            .and_then(Value::as_i64),
        fetched_at_unix: unix_secs(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造带 `chatgpt_account_id` 的假 JWT（与 codex 测试同构，见
    /// `3rdparty/codex/codex-rs/login/src/auth/auth_tests.rs` fake_jwt）。
    fn fake_jwt(account_id: &str) -> String {
        let header = serde_json::json!({"alg": "none", "typ": "JWT"});
        let payload = serde_json::json!({
            "email": "user@example.com",
            "email_verified": true,
            "https://api.openai.com/auth": {
                "chatgpt_plan_type": "plus",
                "chatgpt_account_id": account_id,
                "chatgpt_user_id": "user-12345",
            }
        });
        let b64 = |v: &serde_json::Value| URL_SAFE_NO_PAD.encode(serde_json::to_vec(v).unwrap());
        format!("{}.{}.sig", b64(&header), b64(&payload))
    }

    #[test]
    fn jwt_account_id_extracted_from_payload() {
        let token = fake_jwt("4ac60e19-abc");
        assert_eq!(
            chatgpt_account_id_from_jwt(&token).expect("decode"),
            "4ac60e19-abc"
        );
    }

    #[test]
    fn jwt_missing_account_id_claim_is_error() {
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#);
        let payload = URL_SAFE_NO_PAD.encode(br#"{"email":"user@example.com"}"#);
        let token = format!("{header}.{payload}.sig");
        assert!(chatgpt_account_id_from_jwt(&token).is_err());
    }

    #[test]
    fn jwt_malformed_is_error() {
        // 不是 3 段
        assert!(chatgpt_account_id_from_jwt("not-a-jwt").is_err());
        assert!(chatgpt_account_id_from_jwt("only.two").is_err());
        assert!(chatgpt_account_id_from_jwt("one.two.three.four").is_err());
        assert!(chatgpt_account_id_from_jwt("").is_err());

        // payload 段存在但不是合法 base64/JSON
        assert!(chatgpt_account_id_from_jwt("a.%%%.sig").is_err());

        // 空段
        assert!(chatgpt_account_id_from_jwt("..").is_err());
    }

    #[test]
    fn jwt_wrong_part_count_returns_clear_error() {
        let err = chatgpt_account_id_from_jwt("single-segment")
            .expect_err("should fail with clear message");
        assert!(
            err.to_string().contains("expected 3 parts"),
            "error should mention expected parts: {err}"
        );
    }

    #[tokio::test]
    async fn fetch_chatgpt_quota_parses_fields_and_sends_headers() {
        let app = axum::Router::new().route(
            "/wham/usage",
            axum::routing::get(|headers: axum::http::HeaderMap| async move {
                assert_eq!(
                    headers.get("authorization").and_then(|v| v.to_str().ok()),
                    Some("Bearer test-token")
                );
                assert_eq!(
                    headers
                        .get("chatgpt-account-id")
                        .and_then(|v| v.to_str().ok()),
                    Some("acct-123")
                );
                axum::Json(serde_json::json!({
                    "plan_type": "plus",
                    "rate_limit": {
                        "allowed": true,
                        "limit_reached": false,
                        "primary_window": {
                            "used_percent": 50,
                            "limit_window_seconds": 604800,
                            "reset_after_seconds": 316632,
                            "reset_at": 1786184630
                        },
                        "secondary_window": null
                    }
                }))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });

        let info = fetch_chatgpt_quota(
            &reqwest::Client::new(),
            &format!("http://{addr}"),
            "test-token",
            "acct-123",
            "openai-sub",
        )
        .await
        .expect("fetch");
        assert_eq!(info.provider_id, "openai-sub");
        assert_eq!(info.plan_type.as_deref(), Some("plus"));
        assert_eq!(info.used_percent, Some(50.0));
        assert_eq!(info.limit_window_seconds, Some(604800));
        assert_eq!(info.reset_after_seconds, Some(316632));
        assert_eq!(info.reset_at_unix, Some(1786184630));
    }

    #[tokio::test]
    async fn fetch_chatgpt_quota_handles_missing_rate_limit() {
        let app = axum::Router::new().route(
            "/wham/usage",
            axum::routing::get(|| async { axum::Json(serde_json::json!({"plan_type": "free"})) }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });

        let info = fetch_chatgpt_quota(
            &reqwest::Client::new(),
            &format!("http://{addr}"),
            "test-token",
            "acct-123",
            "openai-sub",
        )
        .await
        .expect("fetch");
        assert_eq!(info.plan_type.as_deref(), Some("free"));
        assert_eq!(info.used_percent, None);
        assert_eq!(info.limit_window_seconds, None);
        assert_eq!(info.reset_at_unix, None);
    }

    #[tokio::test]
    async fn fetch_chatgpt_quota_errors_on_http_error_status() {
        let app = axum::Router::new().route(
            "/wham/usage",
            axum::routing::get(|| async { (axum::http::StatusCode::UNAUTHORIZED, "nope") }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });

        let err = fetch_chatgpt_quota(
            &reqwest::Client::new(),
            &format!("http://{addr}"),
            "test-token",
            "acct-123",
            "openai-sub",
        )
        .await
        .expect_err("should fail");
        assert!(err.to_string().contains("error status"), "{err}");
    }
}
