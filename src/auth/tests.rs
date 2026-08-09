use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serial_test::serial;

// Pull in all public API via the parent module's re-exports
use super::*;

// Pull in internal (pub(super)) items needed by tests
use super::login::{
    antigravity_scopes, build_antigravity_auth_url, exchange_antigravity_code,
    generate_pkce_code_verifier, generate_state, pkce_challenge, poll_openai_device_token,
    random_bytes, request_openai_device_code,
};
use super::refresh::{
    oauth_error_summary, refresh_account_with_urls, refreshed_token_from_json, value_u64,
};
use super::storage::{
    acquire_lock_with_timeout, backup_path_for, cleanup_old_backups, find_backups_newest_first,
    validate_antigravity_account, validate_openai_account, validate_path_safety,
};
use super::types::unix_secs;

fn test_openai_account(account: &str) -> OpenaiAccount {
    let now = unix_secs() as i64;
    OpenaiAccount {
        account_label: format!("{account}@example.com"),
        access_token: format!("access-token-{account}-1234567890"),
        refresh_token: format!("refresh-token-{account}-1234567890"),
        expires_at_unix: now + 3600,
        updated_at_unix: now,
    }
}

fn test_antigravity_account(account: &str) -> AntigravityAccount {
    let now = unix_secs() as i64;
    AntigravityAccount {
        account_label: format!("{account}@example.com"),
        project_id: "test-project-1".to_string(),
        access_token: format!("access-token-{account}-1234567890"),
        refresh_token: format!("refresh-token-{account}-1234567890"),
        expires_at_unix: now + 3600,
        updated_at_unix: now,
    }
}

#[test]
fn status_rows_do_not_expose_tokens() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("oauth_accounts.json");
    let mut accounts = OAuthAccounts::new();
    accounts.openai.insert(
        "openai-subscription".to_string(),
        test_openai_account("openai-subscription"),
    );
    save_oauth_accounts(&path, &accounts).expect("write");

    let (rows, skipped) = status_rows(&path).expect("status");
    assert_eq!(rows.len(), 1);
    assert_eq!(skipped.len(), 0);
    assert_eq!(rows[0].provider, "openai-subscription");
    assert_eq!(rows[0].auth_type, "openai_oauth");
    assert_eq!(rows[0].state, "authenticated");
    assert_eq!(
        rows[0].account_label.as_deref(),
        Some("openai-subscription@example.com")
    );
    let serialized = format!("{rows:?}");
    assert!(!serialized.contains("access-token"));
    assert!(!serialized.contains("refresh-token"));
}

#[test]
fn logout_provider_uses_configured_oauth_account_key() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("oauth_accounts.json");
    let mut accounts = OAuthAccounts::new();
    accounts.openai.insert(
        "shared-account".to_string(),
        test_openai_account("shared-account"),
    );
    save_oauth_accounts(&path, &accounts).expect("write");
    let cfg: crate::config::Config = toml::from_str(
        r#"[server]
listen = "127.0.0.1:8989"

[providers.openai-sub-work]
auth = { type = "openai_oauth", account = "shared-account" }

[providers.openai-sub-work.openai_responses]
url = "https://chatgpt.com/backend-api/codex/responses"
"#,
    )
    .expect("parse config");

    assert_eq!(
        logout_provider(&cfg, &path, "openai-sub-work").expect("logout"),
        1
    );
    let remaining = load_oauth_accounts(&path).expect("read").0;
    assert!(remaining.openai.is_empty());
    assert!(remaining.antigravity.is_empty());
}

#[test]
fn logout_removes_one_or_all_accounts() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("oauth_accounts.json");
    let mut accounts = OAuthAccounts::new();
    accounts.openai.insert(
        "openai-subscription".to_string(),
        test_openai_account("openai-subscription"),
    );
    accounts.antigravity.insert(
        "antigravity".to_string(),
        test_antigravity_account("antigravity"),
    );
    save_oauth_accounts(&path, &accounts).expect("write");

    assert_eq!(logout(&path, Some("antigravity")).expect("logout"), 1);
    let remaining = load_oauth_accounts(&path).expect("read").0;
    assert!(remaining.openai.contains_key("openai-subscription"));
    assert!(!remaining.antigravity.contains_key("antigravity"));

    assert_eq!(logout(&path, None).expect("logout all"), 1);
    let remaining = load_oauth_accounts(&path).expect("read all").0;
    assert!(remaining.openai.is_empty());
    assert!(remaining.antigravity.is_empty());
}

#[test]
fn antigravity_auth_url_contains_pkce_and_scopes() {
    let url = build_antigravity_auth_url("https://accounts.example/auth", "state-1", "challenge-1")
        .expect("url");
    let parsed = url::Url::parse(&url).expect("parse");
    let pairs = parsed
        .query_pairs()
        .collect::<std::collections::BTreeMap<_, _>>();
    assert_eq!(pairs.get("response_type").map(|v| v.as_ref()), Some("code"));
    assert_eq!(pairs.get("state").map(|v| v.as_ref()), Some("state-1"));
    assert_eq!(
        pairs.get("code_challenge_method").map(|v| v.as_ref()),
        Some("S256")
    );
    assert!(
        pairs
            .get("scope")
            .expect("scope")
            .contains("https://www.googleapis.com/auth/cloud-platform")
    );
}

#[test]
fn pkce_challenge_is_deterministic_sha256_base64url() {
    assert_eq!(
        pkce_challenge("verifier"),
        "iMnq5o6zALKXGivsnlom_0F5_WYda32GHkxlV7mq7hQ"
    );
}

#[tokio::test]
async fn antigravity_project_id_uses_load_code_assist_then_onboard_fallback() {
    let app = axum::Router::new()
            .route(
                "/load",
                axum::routing::post(|| async {
                    axum::Json(serde_json::json!({"cloudaicompanionProject": {"id": "project-load"}}))
                }),
            )
            .route(
                "/load-empty",
                axum::routing::post(|| async { axum::Json(serde_json::json!({})) }),
            )
            .route(
                "/onboard",
                axum::routing::post(|| async {
                    axum::Json(serde_json::json!({"response": {"cloudaicompanionProject": {"id": "project-onboard"}}}))
                }),
            );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });

    assert_eq!(
        super::login::fetch_antigravity_project_id(
            &format!("http://{addr}/load"),
            &format!("http://{addr}/onboard"),
            "token",
        )
        .await
        .expect("load"),
        "project-load"
    );
    assert_eq!(
        super::login::fetch_antigravity_project_id(
            &format!("http://{addr}/load-empty"),
            &format!("http://{addr}/onboard"),
            "token",
        )
        .await
        .expect("fallback"),
        "project-onboard"
    );
}

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

#[tokio::test]
async fn refresh_account_updates_access_token_and_keeps_old_refresh_when_missing() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("oauth_accounts.json");
    let mut accounts = OAuthAccounts::new();
    let mut acct = test_openai_account("acct");
    acct.expires_at_unix = 1000000001;
    acct.updated_at_unix = 1000000000;
    accounts.openai.insert("acct".to_string(), acct);
    save_oauth_accounts(&path, &accounts).expect("write");

    let app = axum::Router::new().route(
        "/token",
        axum::routing::post(|| async {
            axum::Json(serde_json::json!({
                "access_token": "new-access-token-1234567890",
                "expires_in": 3600
            }))
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });

    refresh_account_with_urls(
        &path,
        "acct",
        "openai_oauth",
        &format!("http://{addr}/token"),
        "http://127.0.0.1:9/token",
    )
    .await
    .expect("refresh");

    let refreshed = load_oauth_accounts(&path)
        .expect("read")
        .0
        .openai
        .get("acct")
        .expect("acct")
        .clone();
    assert_eq!(refreshed.access_token, "new-access-token-1234567890");
    assert_eq!(refreshed.refresh_token, "refresh-token-acct-1234567890");
    assert!(refreshed.expires_at_unix > unix_secs() as i64);
}

#[test]
fn get_openai_token_returns_token_and_rejects_missing_or_expired() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("oauth_accounts.json");
    let mut accounts = OAuthAccounts::new();
    accounts
        .openai
        .insert("work".to_string(), test_openai_account("work"));
    let mut expired = test_openai_account("expired");
    expired.expires_at_unix = 1000000001;
    expired.updated_at_unix = 1000000000;
    accounts.openai.insert("expired".to_string(), expired);
    save_oauth_accounts(&path, &accounts).expect("write");

    assert_eq!(
        get_openai_token(&path, "work", "openai-subscription").expect("token"),
        "access-token-work-1234567890"
    );
    let err =
        get_openai_token(&path, "missing", "openai-subscription").expect_err("missing account");
    assert!(err.to_string().contains("not found"));
    let err =
        get_openai_token(&path, "expired", "openai-subscription").expect_err("expired account");
    assert!(err.to_string().contains("expired"));
    // 账号存在于错误的分组时同样视为未登录
    let err = get_antigravity_token(&path, "work", "antigravity").expect_err("wrong group");
    assert!(err.to_string().contains("not found"));
}

#[test]
fn get_token_requires_login_when_file_missing() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("oauth_accounts.json");
    let err = get_openai_token(&path, "work", "openai-subscription").expect_err("missing file");
    assert!(err.to_string().contains("requires OAuth login"));
}

#[test]
fn save_creates_backup_and_keeps_only_three() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("oauth_accounts.json");
    let accounts = OAuthAccounts::new();
    save_oauth_accounts(&path, &accounts).expect("initial write");
    // 首次写入不产生备份
    assert!(find_backups_newest_first(&path).is_empty());

    for i in 1..=5 {
        save_oauth_accounts(&path, &accounts).unwrap_or_else(|err| panic!("write {i}: {err}"));
        // 保证备份文件 mtime 递增
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let backups: Vec<_> = fs::read_dir(temp.path())
        .expect("read dir")
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with("oauth_accounts.json.bak."))
        })
        .collect();
    assert_eq!(backups.len(), 3);
}

#[test]
fn acquire_lock_times_out_when_held() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("oauth_accounts.json");
    let _lock = acquire_lock_with_timeout(&path, Duration::from_secs(1)).expect("first lock");
    let result = acquire_lock_with_timeout(&path, Duration::from_millis(100));
    assert!(result.is_err(), "second acquisition must time out");
    assert!(result.unwrap_err().to_string().contains("timed out"));
}

#[test]
fn load_recovers_from_backup_when_file_corrupted() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("oauth_accounts.json");
    let mut accounts = OAuthAccounts::new();
    accounts
        .openai
        .insert("work".to_string(), test_openai_account("work"));
    save_oauth_accounts(&path, &accounts).expect("write");
    save_oauth_accounts(&path, &accounts).expect("write with backup");

    // 损坏主文件
    fs::write(&path, "{ not valid json").expect("corrupt");
    assert!(load_oauth_accounts(&path).is_err());

    let recovered = load_oauth_accounts_with_recovery(&path).expect("recovery");
    assert!(recovered.openai.contains_key("work"));
}

#[cfg(unix)]
#[test]
fn load_and_save_reject_symlink_path() {
    let temp = tempfile::tempdir().expect("tempdir");
    let real = temp.path().join("real.json");
    let link = temp.path().join("oauth_accounts.json");
    save_oauth_accounts(&real, &OAuthAccounts::new()).expect("write real");
    std::os::unix::fs::symlink(&real, &link).expect("symlink");

    let err = load_oauth_accounts(&link).expect_err("symlink load");
    assert!(err.to_string().contains("symbolic link"));
    let err = save_oauth_accounts(&link, &OAuthAccounts::new()).expect_err("symlink save");
    assert!(err.to_string().contains("symbolic link"));
}

#[cfg(unix)]
#[test]
fn oauth_accounts_file_is_owner_only_on_unix() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("oauth_accounts.json");
    save_oauth_accounts(&path, &OAuthAccounts::new()).expect("write");
    let mode = fs::metadata(&path).expect("metadata").permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
}

#[test]
fn locked_updates_merge_changes_from_interleaved_writers() {
    // 模拟两个进程的交错写：各自在锁内 load→modify→save，双方变更都保留
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("oauth_accounts.json");

    with_locked_accounts(&path, |accounts| {
        accounts
            .openai
            .insert("process-a".to_string(), test_openai_account("process-a"));
        save_oauth_accounts_locked(&path, accounts)
    })
    .expect("process A write");
    with_locked_accounts(&path, |accounts| {
        accounts
            .openai
            .insert("process-b".to_string(), test_openai_account("process-b"));
        save_oauth_accounts_locked(&path, accounts)
    })
    .expect("process B write");

    let accounts = load_oauth_accounts(&path).expect("read").0;
    assert!(accounts.openai.contains_key("process-a"));
    assert!(accounts.openai.contains_key("process-b"));
}

#[tokio::test]
async fn refresh_skips_when_recently_refreshed_but_forces_otherwise() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("oauth_accounts.json");
    let mut accounts = OAuthAccounts::new();
    // 未过期且刚刚刷新（模拟另一个进程刚完成 refresh）
    accounts
        .openai
        .insert("acct".to_string(), test_openai_account("acct"));
    save_oauth_accounts(&path, &accounts).expect("write");

    // 不可达地址：若真的发起 HTTP 刷新会报错，从而证明 skip 生效
    refresh_account_with_urls(
        &path,
        "acct",
        "openai_oauth",
        "http://127.0.0.1:9/token",
        "http://127.0.0.1:9/token",
    )
    .await
    .expect("skip refresh");

    // updated_at 较旧的未过期 token：手动 refresh 仍强制执行
    let mut accounts = load_oauth_accounts(&path).expect("read").0;
    let entry = accounts.openai.get_mut("acct").expect("acct");
    entry.updated_at_unix = 1000000000;
    save_oauth_accounts(&path, &accounts).expect("rewrite");

    let app = axum::Router::new().route(
        "/token",
        axum::routing::post(|| async {
            axum::Json(serde_json::json!({
                "access_token": "forced-access-token-1234567890",
                "expires_in": 3600
            }))
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });

    refresh_account_with_urls(
        &path,
        "acct",
        "openai_oauth",
        &format!("http://{addr}/token"),
        "http://127.0.0.1:9/token",
    )
    .await
    .expect("forced refresh");

    let accounts = load_oauth_accounts(&path).expect("read").0;
    assert_eq!(
        accounts.openai.get("acct").expect("acct").access_token,
        "forced-access-token-1234567890"
    );
}

#[test]
fn recovery_writes_back_to_main_file() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("oauth_accounts.json");
    let mut accounts = OAuthAccounts::new();
    accounts
        .openai
        .insert("work".to_string(), test_openai_account("work"));
    save_oauth_accounts(&path, &accounts).expect("write");
    save_oauth_accounts(&path, &accounts).expect("write with backup");

    fs::write(&path, "{ corrupted").expect("corrupt");
    let recovered = load_oauth_accounts_with_recovery(&path).expect("recovery");
    assert!(recovered.openai.contains_key("work"));

    // 主文件已被写回，不再需要恢复路径
    let reloaded = load_oauth_accounts(&path).expect("main file repaired").0;
    assert!(reloaded.openai.contains_key("work"));
}

#[test]
fn recovery_tries_older_backups_when_newest_is_invalid() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("oauth_accounts.json");
    let mut accounts = OAuthAccounts::new();
    accounts
        .openai
        .insert("work".to_string(), test_openai_account("work"));

    // 手工构造：较早的有效备份 + 较新的损坏备份（按 mtime 排序，损坏的排在前面）
    let older = temp.path().join("oauth_accounts.json.bak.1");
    fs::write(
        &older,
        serde_json::to_string_pretty(&accounts).expect("json"),
    )
    .expect("write older backup");
    std::thread::sleep(std::time::Duration::from_millis(10));
    let newer = temp.path().join("oauth_accounts.json.bak.2");
    fs::write(&newer, "{ also broken").expect("write corrupt newer backup");

    fs::write(&path, "{ corrupted").expect("corrupt main");
    let recovered = load_oauth_accounts_with_recovery(&path).expect("recovery from older");
    assert!(recovered.openai.contains_key("work"));
}

#[test]
fn version_mismatch_skips_recovery() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("oauth_accounts.json");

    // 写入 version: 2 的配置（当前 binary 只支持 version: 1）
    let newer_config = serde_json::json!({
        "version": 2,
        "openai": {}
    });
    fs::write(
        &path,
        serde_json::to_string_pretty(&newer_config).expect("json"),
    )
    .expect("write newer version");

    // 同时写入一个有效的 v1 备份
    let mut accounts = OAuthAccounts::new();
    accounts
        .openai
        .insert("work".to_string(), test_openai_account("work"));
    let backup = temp.path().join("oauth_accounts.json.bak.1");
    fs::write(
        &backup,
        serde_json::to_string_pretty(&accounts).expect("json"),
    )
    .expect("write v1 backup");

    // 版本不匹配应该直接报错，不尝试从备份恢复
    let err = load_oauth_accounts_with_recovery(&path).expect_err("version mismatch");
    assert!(
        err.to_string().contains("newer than supported"),
        "error should mention version mismatch, got: {}",
        err
    );

    // 主文件应该保持原样（未被备份覆盖）
    let content = fs::read_to_string(&path).expect("read main");
    assert!(
        content.contains("\"version\": 2"),
        "main file should not be overwritten by backup"
    );
}

#[test]
fn test_validate_antigravity_account_success_and_failures() {
    let valid_acc = test_antigravity_account("valid_activity");
    assert!(validate_antigravity_account("valid_activity", &valid_acc).is_ok());

    // 空 ID
    assert!(validate_antigravity_account("", &valid_acc).is_err());

    // 非法字符
    assert!(validate_antigravity_account("invalid@id!", &valid_acc).is_err());

    // ID 过长 (>64)
    let long_id = "a".repeat(65);
    assert!(validate_antigravity_account(&long_id, &valid_acc).is_err());

    // token 相同
    let mut bad = valid_acc.clone();
    bad.refresh_token = bad.access_token.clone();
    assert!(validate_antigravity_account("valid_activity", &bad).is_err());

    // access_token 过短 (<20)
    let mut bad = valid_acc.clone();
    bad.access_token = "short".to_string();
    assert!(validate_antigravity_account("valid_activity", &bad).is_err());

    // refresh_token 过短 (<20)
    let mut bad = valid_acc.clone();
    bad.refresh_token = "short".to_string();
    assert!(validate_antigravity_account("valid_activity", &bad).is_err());

    // expires_at_unix 非法 (<1000000000)
    let mut bad = valid_acc.clone();
    bad.expires_at_unix = 999_999_999;
    assert!(validate_antigravity_account("valid_activity", &bad).is_err());

    // updated_at_unix 非法 (<1000000000)
    let mut bad = valid_acc.clone();
    bad.updated_at_unix = 999_999_999;
    assert!(validate_antigravity_account("valid_activity", &bad).is_err());

    // updated_at_unix > expires_at_unix
    let mut bad = valid_acc.clone();
    bad.updated_at_unix = bad.expires_at_unix + 100;
    assert!(validate_antigravity_account("valid_activity", &bad).is_err());

    // project_id 格式非法
    let mut bad = valid_acc.clone();
    bad.project_id = "INVALID_PROJ!".to_string();
    assert!(validate_antigravity_account("valid_activity", &bad).is_err());
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
fn test_validate_path_safety_valid_and_symlink() {
    let temp = tempfile::tempdir().expect("tempdir");
    let normal_path = temp.path().join("oauth_accounts.json");
    assert!(validate_path_safety(&normal_path).is_ok());

    let target = temp.path().join("real.json");
    fs::write(&target, "{}").expect("write");
    let symlink = temp.path().join("symlink.json");
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&target, &symlink).expect("symlink");
        let res = validate_path_safety(&symlink);
        assert!(res.is_err());
        assert!(res.unwrap_err().to_string().contains("symbolic link"));
    }
}

#[test]
fn test_acquire_lock_with_timeout_success_and_timeout() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("data.json");

    // 成功获取锁
    let lock1 = acquire_lock_with_timeout(&path, Duration::from_secs(1));
    assert!(lock1.is_ok());

    // 锁被持有时重试超时
    let lock2 = acquire_lock_with_timeout(&path, Duration::from_millis(50));
    assert!(lock2.is_err());
    assert!(lock2.unwrap_err().to_string().contains("timed out"));
}

#[test]
fn test_backup_path_for_generates_expected_path() {
    let path = Path::new("/tmp/test_dir/oauth_accounts.json");
    let backup = backup_path_for(path, 123456789);
    assert_eq!(
        backup,
        PathBuf::from("/tmp/test_dir/oauth_accounts.json.bak.123456789")
    );
}

#[test]
fn test_find_backups_newest_first_sorts_by_mtime() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("oauth.json");

    let b1 = temp.path().join("oauth.json.bak.1");
    let b2 = temp.path().join("oauth.json.bak.2");
    let b3 = temp.path().join("oauth.json.bak.3");

    fs::write(&b1, "1").expect("write b1");
    std::thread::sleep(Duration::from_millis(15));
    fs::write(&b2, "2").expect("write b2");
    std::thread::sleep(Duration::from_millis(15));
    fs::write(&b3, "3").expect("write b3");

    let backups = find_backups_newest_first(&path);
    assert_eq!(backups.len(), 3);
    assert_eq!(backups[0], b3);
    assert_eq!(backups[1], b2);
    assert_eq!(backups[2], b1);
}

#[test]
fn test_cleanup_old_backups_removes_oldest_files() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("oauth.json");

    for i in 1..=5 {
        let b = temp.path().join(format!("oauth.json.bak.{i}"));
        fs::write(&b, format!("{i}")).expect("write backup");
        std::thread::sleep(Duration::from_millis(10));
    }

    assert_eq!(find_backups_newest_first(&path).len(), 5);
    cleanup_old_backups(&path, 2).expect("cleanup");
    let remaining = find_backups_newest_first(&path);
    assert_eq!(remaining.len(), 2);
    assert!(remaining[0].to_str().unwrap().contains("bak.5"));
    assert!(remaining[1].to_str().unwrap().contains("bak.4"));
}

#[test]
fn test_unix_secs_returns_valid_timestamp() {
    let secs = unix_secs();
    assert!(secs > 1_700_000_000);
}

#[test]
fn test_default_state_path_returns_expected_path() {
    let path = default_state_path();
    assert!(
        path.ends_with("oauth_accounts.json"),
        "unexpected path: {}",
        path.display()
    );
}

#[test]
fn test_default_state_path_respects_state_dir_env() {
    let key = "LLM_PROXY_STATE_DIR";
    let old = std::env::var(key).ok();
    unsafe {
        std::env::set_var(key, "/tmp/test-state-dir");
    }
    let path = default_state_path();
    match old {
        Some(v) => unsafe { std::env::set_var(key, v) },
        None => unsafe { std::env::remove_var(key) },
    }
    assert!(
        path.starts_with("/tmp/test-state-dir"),
        "expected path under /tmp/test-state-dir, got: {}",
        path.display()
    );
    assert!(path.ends_with("oauth_accounts.json"));
}

#[test]
fn test_oauth_account_for_provider_resolves_configured_or_default_and_rejects_non_oauth() {
    let cfg_toml = r#"
[server]
listen = "127.0.0.1:8989"

[providers.openai-explicit]
auth = { type = "openai_oauth", account = "my-custom-account" }

[providers.antigravity-default]
auth = { type = "antigravity_oauth" }

[providers.api-key-prov]
auth = { type = "api_key_env", env = "SOME_KEY" }
"#;
    let cfg: crate::config::Config = toml::from_str(cfg_toml).expect("parse config");

    // 显示指定的 account
    let acc1 = oauth_account_for_provider(&cfg, "openai-explicit").expect("account");
    assert_eq!(acc1, "my-custom-account");

    // 默认与 provider_id 相同的 account
    let acc2 = oauth_account_for_provider(&cfg, "antigravity-default").expect("account");
    assert_eq!(acc2, "antigravity-default");

    // 非 OAuth provider
    let err1 = oauth_account_for_provider(&cfg, "api-key-prov").expect_err("non-oauth");
    assert!(err1.to_string().contains("not OAuth-backed"));

    // 未知 provider
    let err2 = oauth_account_for_provider(&cfg, "unknown-prov").expect_err("unknown");
    assert!(err2.to_string().contains("unknown provider"));
}

#[test]
fn test_build_antigravity_auth_url_valid_and_invalid() {
    let url = build_antigravity_auth_url(
        "https://accounts.google.com/o/oauth2/v2/auth",
        "state_123",
        "challenge_456",
    )
    .expect("build url");
    let parsed = url::Url::parse(&url).expect("parse url");
    let query: std::collections::HashMap<_, _> = parsed.query_pairs().into_owned().collect();

    assert_eq!(query.get("state").map(|s| s.as_str()), Some("state_123"));
    assert_eq!(
        query.get("code_challenge").map(|s| s.as_str()),
        Some("challenge_456")
    );
    assert_eq!(
        query.get("code_challenge_method").map(|s| s.as_str()),
        Some("S256")
    );
    assert_eq!(query.get("response_type").map(|s| s.as_str()), Some("code"));
    assert_eq!(
        query.get("access_type").map(|s| s.as_str()),
        Some("offline")
    );
    assert_eq!(query.get("prompt").map(|s| s.as_str()), Some("consent"));

    let invalid = build_antigravity_auth_url("invalid-url-str", "s", "c");
    assert!(invalid.is_err());
}

#[test]
fn test_antigravity_scopes_returns_expected_list() {
    let scopes = antigravity_scopes();
    assert!(!scopes.is_empty());
    assert!(scopes.contains(&"https://www.googleapis.com/auth/cloud-platform"));
    assert!(scopes.contains(&"https://www.googleapis.com/auth/userinfo.email"));
    assert!(scopes.contains(&"https://www.googleapis.com/auth/userinfo.profile"));
    assert!(scopes.contains(&"https://www.googleapis.com/auth/cclog"));
    assert!(scopes.contains(&"https://www.googleapis.com/auth/experimentsandconfigs"));
}

#[test]
fn test_crypto_utils_generate_verifier_state_random_and_challenge() {
    let rnd = random_bytes(16).expect("random bytes");
    assert_eq!(rnd.len(), 16);

    let verifier = generate_pkce_code_verifier().expect("verifier");
    assert!(!verifier.is_empty());

    let state = generate_state().expect("state");
    assert!(!state.is_empty());

    let challenge = pkce_challenge("verifier");
    assert_eq!(challenge, "iMnq5o6zALKXGivsnlom_0F5_WYda32GHkxlV7mq7hQ");
}

// =========================================================================
// 补充测试：validate_antigravity_account 边界条件
// =========================================================================

#[test]
fn validate_antigravity_id_boundary_at_64_chars_is_ok() {
    // 恰好 64 字符的 ID 应通过
    let valid_acc = test_antigravity_account("boundary");
    let id_64 = "a".repeat(64);
    assert!(
        validate_antigravity_account(&id_64, &valid_acc).is_ok(),
        "64-char ID should be accepted"
    );
}

#[test]
fn validate_antigravity_token_boundary_at_exactly_20_chars_is_ok() {
    // 恰好 20 个字符的 token 应通过（≥ 20 有效）
    let mut acc = test_antigravity_account("tok");
    acc.access_token = "a".repeat(20);
    acc.refresh_token = "b".repeat(20);
    assert!(
        validate_antigravity_account("tok", &acc).is_ok(),
        "20-char token should be accepted"
    );
}

#[test]
fn validate_antigravity_token_at_19_chars_is_rejected() {
    // 19 个字符的 token 不通过（< 20）
    let mut acc = test_antigravity_account("tok");
    acc.access_token = "a".repeat(19);
    assert!(
        validate_antigravity_account("tok", &acc).is_err(),
        "19-char access_token should be rejected"
    );
    let mut acc2 = test_antigravity_account("tok2");
    acc2.refresh_token = "b".repeat(19);
    assert!(
        validate_antigravity_account("tok2", &acc2).is_err(),
        "19-char refresh_token should be rejected"
    );
}

#[test]
fn validate_antigravity_project_id_boundary_cases() {
    let base = test_antigravity_account("proj-test");

    // 最短合法 project_id (6 chars: a + 4 middle + z0)
    let mut acc = base.clone();
    acc.project_id = "abc123".to_string(); // 6 chars, valid
    assert!(
        validate_antigravity_account("proj-test", &acc).is_ok(),
        "6-char project_id should be ok"
    );

    // 太短 (5 chars: 不满足 4,28 中间)
    let mut acc = base.clone();
    acc.project_id = "ab12z".to_string(); // 5 chars total
    assert!(
        validate_antigravity_account("proj-test", &acc).is_err(),
        "5-char project_id should fail"
    );

    // 以数字开头（不合法）
    let mut acc = base.clone();
    acc.project_id = "1invalid-start".to_string();
    assert!(
        validate_antigravity_account("proj-test", &acc).is_err(),
        "project_id starting with digit should fail"
    );

    // 以连字符结尾（不合法）
    let mut acc = base.clone();
    acc.project_id = "invalid-end-".to_string();
    assert!(
        validate_antigravity_account("proj-test", &acc).is_err(),
        "project_id ending with hyphen should fail"
    );

    // 含大写字母（不合法）
    let mut acc = base.clone();
    acc.project_id = "Invalid-Project".to_string();
    assert!(
        validate_antigravity_account("proj-test", &acc).is_err(),
        "project_id with uppercase should fail"
    );
}

#[test]
fn validate_antigravity_account_id_allows_hyphen_and_underscore() {
    let acc = test_antigravity_account("hyphens");
    // 连字符和下划线应被允许
    assert!(validate_antigravity_account("my-account_id", &acc).is_ok());
    assert!(validate_antigravity_account("my-account", &acc).is_ok());
    assert!(validate_antigravity_account("my_account", &acc).is_ok());
}

#[test]
fn validate_antigravity_updated_at_equal_to_expires_at_is_ok() {
    // updated_at == expires_at 是合法的（边界值：相等时不违反 >）
    let mut acc = test_antigravity_account("eq-times");
    acc.updated_at_unix = acc.expires_at_unix;
    assert!(
        validate_antigravity_account("eq-times", &acc).is_ok(),
        "updated_at == expires_at should be ok"
    );
}

#[test]
fn antigravity_account_is_expired_checks_unix_time() {
    let now = unix_secs() as i64;
    let mut acc = test_antigravity_account("expiry");

    // 未过期
    acc.expires_at_unix = now + 3600;
    assert!(!acc.is_expired(), "future expiry should not be expired");

    // 已过期
    acc.expires_at_unix = now - 1;
    assert!(acc.is_expired(), "past expiry should be expired");

    // 恰好到期（expires_at <= now 时算过期）
    acc.expires_at_unix = now;
    assert!(acc.is_expired(), "current unix second should be expired");
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

// =========================================================================
// 补充测试：validate_openai_account 遗漏的边界条件
// =========================================================================

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

// =========================================================================
// 补充测试：OAuthAccounts 结构体验证
// =========================================================================

#[test]
fn oauth_accounts_new_has_version_one_and_empty_maps() {
    let accounts = OAuthAccounts::new();
    assert_eq!(accounts.version, 1);
    assert!(accounts.antigravity.is_empty());
    assert!(accounts.openai.is_empty());
}

#[test]
fn oauth_accounts_default_is_equivalent_to_new() {
    let default_acc = OAuthAccounts::default();
    assert_eq!(default_acc.version, 0); // Default derives to 0, not 1
    assert!(default_acc.antigravity.is_empty());
    assert!(default_acc.openai.is_empty());
}

#[test]
fn load_oauth_accounts_skips_invalid_and_loads_valid() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("oauth_accounts.json");

    // 包含一个有效和一个无效的账号
    let mixed = serde_json::json!({
        "version": 1,
        "openai": {
            "valid-account": {
                "account_label": "user@example.com",
                "access_token": "valid-access-token-1234567890",
                "refresh_token": "valid-refresh-token-1234567890",
                "expires_at_unix": 2000000000,
                "updated_at_unix": 1000000000
            },
            "invalid-account": {
                "account_label": "bad@example.com",
                "access_token": "short",
                "refresh_token": "also-short",
                "expires_at_unix": 2000000000,
                "updated_at_unix": 1000000000
            }
        }
    });
    fs::write(&path, serde_json::to_string(&mixed).unwrap()).expect("write");

    let (accounts, skipped) = load_oauth_accounts(&path).expect("load");
    assert_eq!(accounts.openai.len(), 1);
    assert!(accounts.openai.contains_key("valid-account"));
    assert_eq!(skipped.len(), 1);
    assert_eq!(skipped[0].account_id, "invalid-account");
    assert_eq!(skipped[0].account_type, "openai");
}

#[test]
fn load_oauth_accounts_returns_not_found_as_empty() {
    let path = Path::new("/tmp/nonexistent_oauth_test_file_xyz.json");
    let (accounts, skipped) = load_oauth_accounts(path).expect("not found should be empty");
    assert!(accounts.openai.is_empty());
    assert!(accounts.antigravity.is_empty());
    assert!(skipped.is_empty());
}

#[test]
fn load_oauth_accounts_rejects_version_2() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("v2.json");
    let v2 = serde_json::json!({ "version": 2, "openai": {} });
    fs::write(&path, serde_json::to_string(&v2).unwrap()).expect("write");

    let err = load_oauth_accounts(&path).expect_err("version 2 should fail");
    assert!(
        err.to_string()
            .contains("unsupported OAuth accounts version"),
        "expected version error, got: {err}"
    );
}

#[test]
fn load_oauth_accounts_rejects_malformed_json() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("bad.json");
    fs::write(&path, "{ not valid json").expect("write");

    let err = load_oauth_accounts(&path).expect_err("malformed json should fail");
    assert!(
        err.to_string().contains("failed to parse"),
        "expected parse error, got: {err}"
    );
}

#[test]
fn save_and_reload_both_account_types_roundtrip() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("accounts.json");

    let mut accounts = OAuthAccounts::new();
    accounts.openai.insert(
        "openai-user".to_string(),
        test_openai_account("openai-user"),
    );
    accounts
        .antigravity
        .insert("ag-user".to_string(), test_antigravity_account("ag-user"));
    save_oauth_accounts(&path, &accounts).expect("save");

    let (loaded, skipped) = load_oauth_accounts(&path).expect("load");
    assert!(loaded.openai.contains_key("openai-user"));
    assert!(loaded.antigravity.contains_key("ag-user"));
    assert!(skipped.is_empty());
}

#[test]
fn added_account_exists_recognizes_aliases_and_unknown_types() {
    let mut accounts = OAuthAccounts::new();
    accounts
        .openai
        .insert("oa".into(), test_openai_account("oa"));
    accounts
        .antigravity
        .insert("ag".into(), test_antigravity_account("ag"));
    assert!(account_exists(&accounts, "oa", "openai"));
    assert!(account_exists(&accounts, "oa", "openai_oauth"));
    assert!(account_exists(&accounts, "ag", "antigravity"));
    assert!(account_exists(&accounts, "ag", "antigravity_oauth"));
    assert!(!account_exists(&accounts, "oa", "bogus"));
    assert!(!account_exists(&accounts, "missing", "openai"));
}

#[test]
fn added_get_openai_token_from_accounts_success_missing_and_expired() {
    let mut accounts = OAuthAccounts::new();
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
fn added_get_antigravity_token_from_accounts_success_missing_and_expired() {
    let mut accounts = OAuthAccounts::new();
    let acc = test_antigravity_account("ag");
    let token = acc.access_token.clone();
    accounts.antigravity.insert("ag".into(), acc);
    assert_eq!(
        get_antigravity_token_from_accounts(&accounts, "ag", "prov").unwrap(),
        token
    );
    assert!(
        get_antigravity_token_from_accounts(&accounts, "missing", "prov")
            .unwrap_err()
            .to_string()
            .contains("not found")
    );
    accounts.antigravity.get_mut("ag").unwrap().expires_at_unix = unix_secs() as i64 - 1;
    assert!(
        get_antigravity_token_from_accounts(&accounts, "ag", "prov")
            .unwrap_err()
            .to_string()
            .contains("expired")
    );
}

#[test]
#[serial]
fn added_get_provider_auth_status_all_auth_variants() {
    let cfg: crate::config::Config = toml::from_str(
        r#"
[server]
listen = "127.0.0.1:8989"
[providers.none]
auth = { type = "none" }
[providers.key]
auth = { type = "api_key_env", env = "LLM_PROXY_AUTH_TEST_MISSING_KEY" }
[providers.oa]
auth = { type = "openai_oauth" }
[providers.ag]
auth = { type = "antigravity_oauth", account = "shared-ag" }
"#,
    )
    .unwrap();
    unsafe {
        std::env::remove_var("LLM_PROXY_AUTH_TEST_MISSING_KEY");
    }
    let mut accounts = OAuthAccounts::new();
    accounts
        .openai
        .insert("oa".into(), test_openai_account("oa"));
    accounts
        .antigravity
        .insert("shared-ag".into(), test_antigravity_account("shared-ag"));
    assert_eq!(
        get_provider_auth_status(&cfg.providers["none"], "none", &accounts),
        ProviderAuthStatus::Ready
    );
    assert_eq!(
        get_provider_auth_status(&cfg.providers["key"], "key", &accounts),
        ProviderAuthStatus::MissingKey("LLM_PROXY_AUTH_TEST_MISSING_KEY".into())
    );
    assert_eq!(
        get_provider_auth_status(&cfg.providers["oa"], "oa", &accounts),
        ProviderAuthStatus::Ready
    );
    assert_eq!(
        get_provider_auth_status(&cfg.providers["ag"], "ag", &accounts),
        ProviderAuthStatus::Ready
    );
    accounts.openai.get_mut("oa").unwrap().expires_at_unix = unix_secs() as i64 - 1;
    assert_eq!(
        get_provider_auth_status(&cfg.providers["oa"], "oa", &accounts),
        ProviderAuthStatus::Expired
    );
    accounts.antigravity.clear();
    assert_eq!(
        get_provider_auth_status(&cfg.providers["ag"], "ag", &accounts),
        ProviderAuthStatus::NotLoggedIn
    );
}

#[test]
fn added_status_rows_reports_expired_antigravity_and_skipped_invalid() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("accounts.json");
    let expired = unix_secs() as i64 - 1;
    let data = serde_json::json!({"version":1,"antigravity":{
        "ag": {"account_label":"ag@example.com","project_id":"test-project-1","access_token":"access-token-ag-1234567890","refresh_token":"refresh-token-ag-1234567890","expires_at_unix": expired,"updated_at_unix":1000000000},
        "bad!": {"account_label":"bad","project_id":"test-project-1","access_token":"access-token-bad-1234567890","refresh_token":"refresh-token-bad-1234567890","expires_at_unix":2000000000,"updated_at_unix":1000000000}
    }});
    fs::write(&path, data.to_string()).unwrap();
    let (rows, skipped) = status_rows(&path).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].state, "expired");
    assert_eq!(rows[0].auth_type, "antigravity_oauth");
    assert_eq!(skipped.len(), 1);
    assert_eq!(skipped[0].account_id, "bad!");
}

#[test]
fn added_logout_missing_account_does_not_create_backup_or_modify_file() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("accounts.json");
    let mut accounts = OAuthAccounts::new();
    accounts
        .openai
        .insert("oa".into(), test_openai_account("oa"));
    save_oauth_accounts(&path, &accounts).unwrap();
    assert_eq!(logout(&path, Some("missing")).unwrap(), 0);
    assert_eq!(find_backups_newest_first(&path).len(), 0);
    assert!(
        load_oauth_accounts(&path)
            .unwrap()
            .0
            .openai
            .contains_key("oa")
    );
}

#[test]
fn added_with_locked_accounts_can_create_parent_and_save_inside_transaction() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("nested").join("accounts.json");
    with_locked_accounts(&path, |accounts| {
        accounts
            .openai
            .insert("oa".into(), test_openai_account("oa"));
        save_oauth_accounts_locked(&path, accounts)
    })
    .unwrap();
    assert!(
        load_oauth_accounts(&path)
            .unwrap()
            .0
            .openai
            .contains_key("oa")
    );
}

#[test]
fn added_backup_path_for_appends_timestamp_to_full_path() {
    let path = Path::new("/tmp/oauth_accounts.json");
    let backup = backup_path_for(path, 42);
    assert_eq!(backup, PathBuf::from("/tmp/oauth_accounts.json.bak.42"));
}

#[test]
fn added_find_backups_returns_empty_without_parent_or_prefix_matches() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("accounts.json");
    fs::write(temp.path().join("other.json.bak.1"), "{}").unwrap();
    assert!(find_backups_newest_first(&path).is_empty());
    assert!(find_backups_newest_first(Path::new("accounts.json")).is_empty());
}

#[test]
fn added_cleanup_old_backups_with_keep_larger_than_len_is_noop() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("accounts.json");
    for i in 0..2 {
        fs::write(backup_path_for(&path, i), "{}").unwrap();
    }
    cleanup_old_backups(&path, 5).unwrap();
    assert_eq!(find_backups_newest_first(&path).len(), 2);
}

#[test]
fn added_save_locked_replaces_existing_file_and_creates_backup() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("accounts.json");
    let mut first = OAuthAccounts::new();
    first
        .openai
        .insert("first".into(), test_openai_account("first"));
    save_oauth_accounts_locked(&path, &first).unwrap();
    let mut second = OAuthAccounts::new();
    second
        .antigravity
        .insert("second".into(), test_antigravity_account("second"));
    save_oauth_accounts_locked(&path, &second).unwrap();
    let loaded = load_oauth_accounts(&path).unwrap().0;
    assert!(loaded.openai.is_empty());
    assert!(loaded.antigravity.contains_key("second"));
    assert_eq!(find_backups_newest_first(&path).len(), 1);
}

#[test]
fn added_validate_path_safety_allows_missing_path() {
    let temp = tempfile::tempdir().unwrap();
    assert!(validate_path_safety(&temp.path().join("missing.json")).is_ok());
}

#[test]
fn added_load_oauth_accounts_skips_invalid_antigravity_reasons() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("accounts.json");
    let data = serde_json::json!({"version":1,"antigravity":{
        "good": {"account_label":"good","project_id":"test-project-1","access_token":"access-token-good-1234567890","refresh_token":"refresh-token-good-1234567890","expires_at_unix":2000000000,"updated_at_unix":1000000000},
        "bad": {"account_label":"bad","project_id":"BadProject","access_token":"access-token-bad-1234567890","refresh_token":"refresh-token-bad-1234567890","expires_at_unix":2000000000,"updated_at_unix":1000000000}
    }});
    fs::write(&path, data.to_string()).unwrap();
    let (accounts, skipped) = load_oauth_accounts(&path).unwrap();
    assert!(accounts.antigravity.contains_key("good"));
    assert_eq!(skipped.len(), 1);
    assert!(
        skipped[0]
            .reason
            .contains("invalid Google Cloud project ID")
    );
}

#[test]
fn added_load_oauth_accounts_empty_json_defaults_to_version_zero_and_errors() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("accounts.json");
    fs::write(&path, "{}").unwrap();
    let err = load_oauth_accounts(&path).unwrap_err().to_string();
    assert!(err.contains("failed to parse"), "unexpected error: {err}");
}

#[test]
fn added_recovery_without_backups_returns_original_parse_error() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("accounts.json");
    fs::write(&path, "not json").unwrap();
    assert!(
        load_oauth_accounts_with_recovery(&path)
            .unwrap_err()
            .to_string()
            .contains("failed to parse")
    );
}

#[test]
fn added_recovery_all_backups_invalid_reports_manual_intervention() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("accounts.json");
    fs::write(&path, "not json").unwrap();
    fs::write(backup_path_for(&path, 1), "also not json").unwrap();
    let err = load_oauth_accounts_with_recovery(&path)
        .unwrap_err()
        .to_string();
    assert!(err.contains("all 1 backup"));
    assert!(err.contains("Manual intervention required"));
}

#[test]
fn added_get_openai_token_file_success_and_missing_account() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("accounts.json");
    let mut accounts = OAuthAccounts::new();
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
fn added_get_antigravity_token_file_success_and_missing_account() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("accounts.json");
    let mut accounts = OAuthAccounts::new();
    let acc = test_antigravity_account("ag");
    let token = acc.access_token.clone();
    accounts.antigravity.insert("ag".into(), acc);
    save_oauth_accounts(&path, &accounts).unwrap();
    assert_eq!(get_antigravity_token(&path, "ag", "prov").unwrap(), token);
    assert!(
        get_antigravity_token(&path, "missing", "prov")
            .unwrap_err()
            .to_string()
            .contains("not found")
    );
}

#[test]
fn added_get_antigravity_token_expired_without_runtime_errors() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("accounts.json");
    let mut accounts = OAuthAccounts::new();
    let mut acc = test_antigravity_account("ag");
    acc.expires_at_unix = unix_secs() as i64 - 1;
    accounts.antigravity.insert("ag".into(), acc);
    save_oauth_accounts(&path, &accounts).unwrap();
    let err = get_antigravity_token(&path, "ag", "prov")
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("OAuth refresh request failed")
            || err.contains("no runtime available")
            || err.contains("not found"),
        "unexpected error: {err}"
    );
}

#[test]
fn added_validate_oauth_on_startup_ok_for_missing_file_and_missing_refs() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("accounts.json");
    let cfg: crate::config::Config = toml::from_str(
        r#"
[server]
listen = "127.0.0.1:8989"
[providers.oa]
auth = { type = "openai_oauth" }
[providers.ag]
auth = { type = "antigravity_oauth", account = "missing-ag" }
"#,
    )
    .unwrap();
    validate_oauth_on_startup(&cfg, &path).unwrap();
    save_oauth_accounts(&path, &OAuthAccounts::new()).unwrap();
    validate_oauth_on_startup(&cfg, &path).unwrap();
}

#[test]
fn added_refreshed_token_from_json_accepts_optional_fields() {
    let tok = refreshed_token_from_json(serde_json::json!({"access_token":"new-access"})).unwrap();
    assert_eq!(tok.access_token, "new-access");
    assert!(tok.refresh_token.is_none());
    assert!(tok.expires_in.is_none());
    let err = refreshed_token_from_json(serde_json::json!({"refresh_token":"r"})).unwrap_err();
    assert!(err.to_string().contains("no access_token"));
}

#[test]
fn added_value_u64_parses_numbers_strings_and_rejects_invalid() {
    let payload = serde_json::json!({"n": 7, "s":"8", "bad":"x", "neg":-1});
    assert_eq!(value_u64(&payload, "n"), Some(7));
    assert_eq!(value_u64(&payload, "s"), Some(8));
    assert_eq!(value_u64(&payload, "bad"), None);
    assert_eq!(value_u64(&payload, "neg"), None);
    assert_eq!(value_u64(&payload, "missing"), None);
}

#[test]
fn added_build_antigravity_auth_url_rejects_empty_base_url() {
    assert!(build_antigravity_auth_url("", "state", "challenge").is_err());
}

#[test]
fn added_generate_pkce_verifier_and_state_are_base64url_without_padding() {
    let verifier = generate_pkce_code_verifier().unwrap();
    let state = generate_state().unwrap();
    assert!(!verifier.contains('='));
    assert!(!state.contains('='));
    assert!(verifier.len() >= 80);
    assert!(state.len() >= 40);
}

#[test]
fn added_random_bytes_zero_len_is_ok() {
    assert_eq!(random_bytes(0).unwrap().len(), 0);
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

#[test]
fn added_validate_antigravity_rejects_bad_timestamps() {
    let mut acc = test_antigravity_account("ag");
    acc.expires_at_unix = 999_999_999;
    assert!(
        validate_antigravity_account("ag", &acc)
            .unwrap_err()
            .to_string()
            .contains("invalid expires")
    );
    let mut acc = test_antigravity_account("ag");
    acc.updated_at_unix = acc.expires_at_unix + 1;
    assert!(
        validate_antigravity_account("ag", &acc)
            .unwrap_err()
            .to_string()
            .contains("updated_at_unix")
    );
}

#[tokio::test]
async fn added_refresh_account_for_provider_rejects_unknown_type() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("accounts.json");
    assert!(
        refresh_account_for_provider(&path, "acc", "unknown")
            .await
            .unwrap_err()
            .to_string()
            .contains("unknown oauth type")
    );
}

#[tokio::test]
async fn added_exchange_antigravity_code_rejects_empty_code_before_http() {
    assert!(
        exchange_antigravity_code("http://127.0.0.1:1/token", "   ", "verifier")
            .await
            .unwrap_err()
            .to_string()
            .contains("must not be empty")
    );
}

#[tokio::test]
async fn antigravity_project_id_load_error_does_not_fallback_to_onboard() {
    let app = axum::Router::new()
        .route(
            "/load-401",
            axum::routing::post(|| async {
                (axum::http::StatusCode::UNAUTHORIZED, "Unauthorized")
            }),
        )
        .route(
            "/onboard",
            axum::routing::post(|| async {
                axum::Json(serde_json::json!({"response": {"cloudaicompanionProject": {"id": "should-not-reach"}}}))
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });

    let err = super::login::fetch_antigravity_project_id(
        &format!("http://{addr}/load-401"),
        &format!("http://{addr}/onboard"),
        "token",
    )
    .await
    .expect_err("should fail on load 401");

    let msg = format!("{err:#}");
    assert!(
        msg.contains("not falling back to onboardUser"),
        "got message: {msg}"
    );
}

#[test]
fn oauth_error_summary_extracts_json_fields_and_sanitizes() {
    let json_both = r#"{"error":"invalid_grant","error_description":"refresh token expired"}"#;
    assert_eq!(
        oauth_error_summary(json_both),
        "error=invalid_grant; error_description=refresh token expired"
    );

    let json_error_only = r#"{"error":"unauthorized_client"}"#;
    assert_eq!(
        oauth_error_summary(json_error_only),
        "error=unauthorized_client"
    );

    let json_desc_only = r#"{"error_description":"something bad"}"#;
    assert_eq!(
        oauth_error_summary(json_desc_only),
        "error_description=something bad"
    );

    let json_neither = r#"{"other_field":"foo"}"#;
    assert_eq!(oauth_error_summary(json_neither), "response body omitted");

    // Non-JSON long input should return fixed message (not output content)
    let long_raw = "x ".repeat(300);
    let summary = oauth_error_summary(&long_raw);
    assert_eq!(summary, "non-JSON response body omitted");
}

#[test]
fn oauth_error_summary_non_json_body_omitted() {
    // Non-JSON body should return fixed text, not output content
    let html_error = "<html><body>Internal Server Error</body></html>";
    assert_eq!(
        oauth_error_summary(html_error),
        "non-JSON response body omitted"
    );

    let plain_text = "Something went wrong";
    assert_eq!(
        oauth_error_summary(plain_text),
        "non-JSON response body omitted"
    );
}

#[test]
fn oauth_error_summary_redacts_sensitive_fields_in_description() {
    // JSON with sensitive fields in error_description should be redacted
    let json_with_token = r#"{
        "error": "invalid_grant",
        "error_description": "failed for refresh_token=secret-refresh-token-12345"
    }"#;
    let summary = oauth_error_summary(json_with_token);
    assert!(summary.contains("invalid_grant"));
    assert!(summary.contains("[REDACTED]"));
    assert!(!summary.contains("secret-refresh-token-12345"));

    // Multiple sensitive fields
    let json_multi = r#"{
        "error": "unauthorized",
        "error_description": "client_secret=abc123 and access_token=xyz789 are invalid"
    }"#;
    let summary = oauth_error_summary(json_multi);
    assert!(summary.contains("[REDACTED]"));
    assert!(!summary.contains("abc123"));
    assert!(!summary.contains("xyz789"));
}

#[test]
fn oauth_error_summary_preserves_non_sensitive_description() {
    // JSON with non-sensitive error_description should be preserved
    let json_safe = r#"{
        "error": "invalid_request",
        "error_description": "missing required parameter: grant_type"
    }"#;
    let summary = oauth_error_summary(json_safe);
    assert!(summary.contains("invalid_request"));
    assert!(summary.contains("missing required parameter: grant_type"));
}
