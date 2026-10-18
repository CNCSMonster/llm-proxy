//! OpenAI **主题**测试（自 `auth/tests.rs` 整函数迁入）。
//!
//! 判据同 anti 主题测试：测试主题属于 openai（device 流 / token 获取 / 账号校验与过期 /
//! 目录恢复 openai 映射）才迁入；共享主题仅拿 openai 当载体的留 `auth/tests.rs`。
//! 本目录是公开能力、不属认证能力裁剪范围。

use crate::auth::testutil::{server_only_config, test_openai_account};
use crate::auth::types::unix_secs;
use crate::auth::*;

use super::flow::{poll_openai_device_token, request_openai_device_code};
use super::schema::validate_openai_account;

#[tokio::test]
async fn openai_device_code_accepts_alias_fields_and_string_numbers() {
    let app = axum::Router::new().route(
        "/device",
        axum::routing::post(|| async {
            axum::Json(serde_json::json!({
                "deviceAuthID": "dev-123",
                "usercode": "ABCD-1234",
                "expires_in": "900",
                "interval": "1"
            }))
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });

    let code = request_openai_device_code(&format!("http://{addr}/device"))
        .await
        .expect("device code");
    assert_eq!(code.device_auth_id, "dev-123");
    assert_eq!(code.user_code, "ABCD-1234");
    assert_eq!(code.expires_in, 900);
    assert_eq!(code.interval, 1);
}

#[tokio::test]
async fn openai_device_poll_pending_on_403_or_404() {
    for status in [
        reqwest::StatusCode::FORBIDDEN,
        reqwest::StatusCode::NOT_FOUND,
    ] {
        let app = axum::Router::new().route(
            "/poll",
            axum::routing::post(move || async move { (status, "pending") }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });

        let poll = poll_openai_device_token(&format!("http://{addr}/poll"), "dev", "code")
            .await
            .expect("poll");
        assert!(poll.is_none());
    }
}

#[test]
fn test_validate_openai_account_success_and_failures() {
    let valid_acc = test_openai_account("valid_account");
    assert!(validate_openai_account("valid_account", &valid_acc).is_ok());

    // 空 ID
    assert!(validate_openai_account("", &valid_acc).is_err());

    // 非法字符
    assert!(validate_openai_account("invalid#id", &valid_acc).is_err());

    // ID 过长 (>64)
    let long_id = "b".repeat(65);
    assert!(validate_openai_account(&long_id, &valid_acc).is_err());

    // token 相同
    let mut bad = valid_acc.clone();
    bad.refresh_token = bad.access_token.clone();
    assert!(validate_openai_account("valid_account", &bad).is_err());

    // access_token 过短 (<20)
    let mut bad = valid_acc.clone();
    bad.access_token = "short".to_string();
    assert!(validate_openai_account("valid_account", &bad).is_err());

    // refresh_token 过短 (<20)
    let mut bad = valid_acc.clone();
    bad.refresh_token = "short".to_string();
    assert!(validate_openai_account("valid_account", &bad).is_err());

    // expires_at_unix 非法
    let mut bad = valid_acc.clone();
    bad.expires_at_unix = 500;
    assert!(validate_openai_account("valid_account", &bad).is_err());

    // updated_at_unix > expires_at_unix
    let mut bad = valid_acc.clone();
    bad.updated_at_unix = bad.expires_at_unix + 10;
    assert!(validate_openai_account("valid_account", &bad).is_err());
}

#[test]
fn openai_account_is_expired_checks_unix_time() {
    let now = unix_secs() as i64;
    let mut acc = test_openai_account("expiry");

    acc.expires_at_unix = now + 3600;
    assert!(!acc.is_expired(), "future expiry should not be expired");

    acc.expires_at_unix = now - 1;
    assert!(acc.is_expired(), "past expiry should be expired");

    acc.expires_at_unix = now;
    assert!(acc.is_expired(), "current unix second should be expired");
}

#[test]
fn validate_openai_account_id_boundary_at_64_chars_is_ok() {
    let acc = test_openai_account("boundary");
    let id_64 = "z".repeat(64);
    assert!(
        validate_openai_account(&id_64, &acc).is_ok(),
        "64-char ID should be accepted"
    );
}

#[test]
fn validate_openai_token_boundary_at_exactly_20_chars_is_ok() {
    let mut acc = test_openai_account("tok");
    acc.access_token = "a".repeat(20);
    acc.refresh_token = "b".repeat(20);
    assert!(
        validate_openai_account("tok", &acc).is_ok(),
        "20-char tokens should be accepted"
    );
}

#[test]
fn validate_openai_updated_at_unix_too_small_is_rejected() {
    // 现有测试没有覆盖 openai 的 updated_at_unix < 1000000000 分支
    let mut acc = test_openai_account("small-updated");
    acc.updated_at_unix = 999_999_999;
    assert!(
        validate_openai_account("small-updated", &acc).is_err(),
        "updated_at_unix < 1_000_000_000 should be rejected"
    );
}

#[test]
fn validate_openai_updated_at_equal_to_expires_at_is_ok() {
    let mut acc = test_openai_account("eq-times");
    acc.updated_at_unix = acc.expires_at_unix;
    assert!(
        validate_openai_account("eq-times", &acc).is_ok(),
        "updated_at == expires_at should be ok"
    );
}

#[test]
fn validate_openai_account_id_allows_hyphen_and_underscore() {
    let acc = test_openai_account("hyphens");
    assert!(validate_openai_account("my-account_id", &acc).is_ok());
    assert!(validate_openai_account("my-account", &acc).is_ok());
    assert!(validate_openai_account("my_account", &acc).is_ok());
}

#[test]
fn added_get_openai_token_from_accounts_success_missing_and_expired() {
    let mut accounts = AccountsFile::new();
    let acc = test_openai_account("oa");
    let token = acc.access_token.clone();
    accounts.openai.insert("oa".into(), acc);
    assert_eq!(
        get_openai_token_from_accounts(&accounts, "oa", "prov").unwrap(),
        token
    );
    assert!(
        get_openai_token_from_accounts(&accounts, "missing", "prov")
            .unwrap_err()
            .to_string()
            .contains("not found")
    );
    accounts.openai.get_mut("oa").unwrap().expires_at_unix = unix_secs() as i64 - 1;
    assert!(
        get_openai_token_from_accounts(&accounts, "oa", "prov")
            .unwrap_err()
            .to_string()
            .contains("expired")
    );
}

#[test]
fn added_get_openai_token_file_success_and_missing_account() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("accounts.json");
    let mut accounts = AccountsFile::new();
    let acc = test_openai_account("oa");
    let token = acc.access_token.clone();
    accounts.openai.insert("oa".into(), acc);
    save_oauth_accounts(&path, &accounts).unwrap();
    assert_eq!(get_openai_token(&path, "oa", "prov").unwrap(), token);
    assert!(
        get_openai_token(&path, "missing", "prov")
            .unwrap_err()
            .to_string()
            .contains("not found")
    );
}

#[test]
fn orphan_openai_subscription_credential_rebuilds_its_catalog_provider() {
    let mut accounts = AccountsFile::new();
    accounts.openai.insert(
        "openai-subscription".into(),
        test_openai_account("openai-subscription"),
    );
    let mut cfg = server_only_config();

    let report = recover_orphan_oauth_accounts(&mut cfg, &accounts);

    assert_eq!(report.recovered.len(), 1, "{report:?}");
    assert_eq!(report.recovered[0].provider, "openai-sub");
    cfg.validate().expect("rebuilt config stays valid");
}

#[test]
fn added_validate_openai_account_rejects_identical_tokens_and_bad_id_chars() {
    let mut acc = test_openai_account("oa");
    acc.refresh_token = acc.access_token.clone();
    assert!(
        validate_openai_account("oa", &acc)
            .unwrap_err()
            .to_string()
            .contains("identical")
    );
    assert!(
        validate_openai_account("bad.id", &test_openai_account("oa"))
            .unwrap_err()
            .to_string()
            .contains("invalid account ID format")
    );
}
