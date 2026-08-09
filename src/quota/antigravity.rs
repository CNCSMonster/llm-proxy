//! Antigravity 额度查询客户端。
//!
//! 端点：`POST https://cloudcode-pa.googleapis.com/v1internal:loadCodeAssist`
//! （调研见 `docs/research/quota-api-research.md` §2，实验验证 2026-08-05）。
//! 认证：OAuth Bearer token。
//!
//! 响应返回 tier 信息（`allowedTiers[0].name`）；`paidTier.availableCredits`
//! 仅在 credit-based 账户存在（当前 standard-tier 为 unlimited，无该字段）。
//! `QuotaInfo` 无法表达 credits，故 credit-based 响应也只返回 tier 信息，
//! used_percent 保持 None（详见 CLI 输出 "N/A (unlimited or not reported)"）。

use anyhow::{Context, Result};
use reqwest::Client;
use serde_json::Value;

use super::types::{QuotaInfo, unix_secs};

/// Antigravity loadCodeAssist 端点（生产）。
pub const ANTIGRAVITY_LOAD_CODE_ASSIST_URL: &str =
    "https://cloudcode-pa.googleapis.com/v1internal:loadCodeAssist";

/// 查询 Antigravity 额度/tier 信息。
///
/// - `url`：loadCodeAssist 端点地址（测试可传本地 mock 服务器地址）
pub async fn fetch_antigravity_quota(
    client: &Client,
    url: &str,
    token: &str,
    provider_id: &str,
) -> Result<QuotaInfo> {
    let resp = client
        .post(url)
        .bearer_auth(token)
        .json(&serde_json::json!({"metadata": {"ideType": "ANTIGRAVITY"}}))
        .send()
        .await
        .with_context(|| format!("quota request to {url} failed"))?
        .error_for_status()
        .with_context(|| format!("quota request to {url} returned an error status"))?
        .json::<Value>()
        .await
        .context("quota response is not valid JSON")?;

    // tier 名称作为 plan_type；unlimited 账户无额度字段。
    let plan_type = resp
        .get("allowedTiers")
        .and_then(Value::as_array)
        .and_then(|tiers| tiers.first())
        .and_then(|tier| tier.get("name"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string);

    // credit-based 账户可能返回 paidTier.availableCredits。
    // QuotaInfo 无 credits 字段，仅记录日志（当前 unlimited 账户无此字段）。
    if resp
        .get("paidTier")
        .and_then(|t| t.get("availableCredits"))
        .is_some()
    {
        tracing::info!(
            provider = provider_id,
            "antigravity quota response carries availableCredits (credit-based plan); \
             not representable in QuotaInfo, showing tier only"
        );
    }

    Ok(QuotaInfo {
        provider_id: provider_id.to_string(),
        plan_type,
        used_percent: None,
        limit_window_seconds: None,
        reset_after_seconds: None,
        reset_at_unix: None,
        fetched_at_unix: unix_secs(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fetch_antigravity_quota_parses_tier_without_credits() {
        let app = axum::Router::new().route(
            "/load",
            axum::routing::post(
                |headers: axum::http::HeaderMap,
                 axum::extract::Json(body): axum::extract::Json<serde_json::Value>| async move {
                    assert_eq!(
                        headers
                            .get("authorization")
                            .and_then(|v| v.to_str().ok()),
                        Some("Bearer test-token")
                    );
                    assert_eq!(
                        body,
                        serde_json::json!({"metadata": {"ideType": "ANTIGRAVITY"}})
                    );
                    axum::Json(serde_json::json!({
                        "allowedTiers": [
                            {"id": "standard-tier", "name": "Gemini Code Assist", "isDefault": true}
                        ],
                        "ineligibleTiers": []
                    }))
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });

        let info = fetch_antigravity_quota(
            &reqwest::Client::new(),
            &format!("http://{addr}/load"),
            "test-token",
            "google-antigravity",
        )
        .await
        .expect("fetch");
        assert_eq!(info.provider_id, "google-antigravity");
        assert_eq!(info.plan_type.as_deref(), Some("Gemini Code Assist"));
        assert_eq!(info.used_percent, None);
        assert_eq!(info.limit_window_seconds, None);
        assert_eq!(info.reset_at_unix, None);
    }

    #[tokio::test]
    async fn fetch_antigravity_quota_handles_credit_based_response_gracefully() {
        // credit-based 账户响应：有 paidTier.availableCredits，但 QuotaInfo 无法表达
        // credits，只返回 tier 信息（不报错）。
        let app = axum::Router::new().route(
            "/load",
            axum::routing::post(|| async {
                axum::Json(serde_json::json!({
                    "allowedTiers": [
                        {"id": "tier-1", "name": "Gemini Code Assist Pro", "isDefault": true}
                    ],
                    "paidTier": {
                        "id": "tier-1",
                        "availableCredits": [
                            {"creditType": "GOOGLE_ONE_AI", "creditAmount": "25000", "minimumCreditAmountForUsage": "50"}
                        ]
                    }
                }))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });

        let info = fetch_antigravity_quota(
            &reqwest::Client::new(),
            &format!("http://{addr}/load"),
            "test-token",
            "google-antigravity",
        )
        .await
        .expect("fetch");
        assert_eq!(info.plan_type.as_deref(), Some("Gemini Code Assist Pro"));
        assert_eq!(info.used_percent, None);
    }

    #[tokio::test]
    async fn fetch_antigravity_quota_errors_on_http_error_status() {
        let app = axum::Router::new().route(
            "/load",
            axum::routing::post(|| async { (axum::http::StatusCode::FORBIDDEN, "nope") }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });

        let err = fetch_antigravity_quota(
            &reqwest::Client::new(),
            &format!("http://{addr}/load"),
            "test-token",
            "google-antigravity",
        )
        .await
        .expect_err("should fail");
        assert!(err.to_string().contains("error status"), "{err}");
    }
}
