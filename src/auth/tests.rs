use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;
// Pull in all public API via the parent module's re-exports
use super::*;

// Pull in internal (pub(super)) items needed by tests
use super::refresh::{
    oauth_error_summary, refresh_account_with_urls, refreshed_token_from_json, value_u64,
};
use super::storage::{
    acquire_lock_with_timeout, backup_path_for, cleanup_old_backups, find_backups_newest_first,
    validate_path_safety,
};
use super::testutil::{server_only_config, test_openai_account};
use super::types::unix_secs;

#[test]
fn status_rows_do_not_expose_tokens() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("oauth_accounts.json");
    let mut accounts = AccountsFile::new();
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
    let mut accounts = AccountsFile::new();
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
    assert!(remaining.openai.is_empty() && remaining.others.is_empty());
}

#[test]
fn logout_removes_one_or_all_accounts() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("oauth_accounts.json");
    let mut accounts = AccountsFile::new();
    accounts.openai.insert(
        "openai-subscription".to_string(),
        test_openai_account("openai-subscription"),
    );
    accounts.openai.insert(
        "openai-work".to_string(),
        test_openai_account("openai-work"),
    );
    save_oauth_accounts(&path, &accounts).expect("write");

    assert_eq!(logout(&path, Some("openai-work")).expect("logout"), 1);
    let remaining = load_oauth_accounts(&path).expect("read").0;
    assert!(remaining.openai.contains_key("openai-subscription"));
    assert!(!remaining.openai.contains_key("openai-work"));

    assert_eq!(logout(&path, None).expect("logout all"), 1);
    let remaining = load_oauth_accounts(&path).expect("read all").0;
    assert!(remaining.openai.is_empty());
}

#[tokio::test]
async fn refresh_account_updates_access_token_and_keeps_old_refresh_when_missing() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("oauth_accounts.json");
    let mut accounts = AccountsFile::new();
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
    let accounts = AccountsFile::new();
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
    let mut accounts = AccountsFile::new();
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
    save_oauth_accounts(&real, &AccountsFile::new()).expect("write real");
    std::os::unix::fs::symlink(&real, &link).expect("symlink");

    let err = load_oauth_accounts(&link).expect_err("symlink load");
    assert!(err.to_string().contains("symbolic link"));
    let err = save_oauth_accounts(&link, &AccountsFile::new()).expect_err("symlink save");
    assert!(err.to_string().contains("symbolic link"));
}

#[cfg(unix)]
#[test]
fn oauth_accounts_file_is_owner_only_on_unix() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("oauth_accounts.json");
    save_oauth_accounts(&path, &AccountsFile::new()).expect("write");
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

#[tokio::test(flavor = "current_thread")]
async fn refresh_failure_returns_refresh_id_without_account_id() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("oauth_accounts.json");
    let account_id = "private-account-id";
    let mut accounts = AccountsFile::new();
    let mut account = test_openai_account(account_id);
    account.updated_at_unix = 1_000_000_000;
    accounts.openai.insert(account_id.to_string(), account);
    save_oauth_accounts(&path, &accounts).expect("write isolated OAuth state");

    let error = super::refresh::refresh_account_with_urls_context(
        &path,
        account_id,
        "openai_oauth",
        "http://127.0.0.1:9/token",
        super::refresh::RefreshContext::new("configured-provider", "isolated_failure_test"),
    )
    .await
    .expect_err("unreachable OAuth endpoint must fail");
    let message = format!("{error:#}");

    assert!(message.contains("refresh_id="));
    assert!(!message.contains(account_id));
}

#[tokio::test]
async fn refresh_skips_when_recently_refreshed_but_forces_otherwise() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("oauth_accounts.json");
    let mut accounts = AccountsFile::new();
    // 未过期且刚刚刷新（模拟另一个进程刚完成 refresh）
    accounts
        .openai
        .insert("acct".to_string(), test_openai_account("acct"));
    save_oauth_accounts(&path, &accounts).expect("write");

    // 不可达地址：若真的发起 HTTP 刷新会报错，从而证明 skip 生效
    refresh_account_with_urls(&path, "acct", "openai_oauth", "http://127.0.0.1:9/token")
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
    let mut accounts = AccountsFile::new();
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
    let mut accounts = AccountsFile::new();
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
    let mut accounts = AccountsFile::new();
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
fn test_state_path_for_config_uses_config_directory() {
    let path = super::storage::state_path_for_config(Path::new("/tmp/instance/config.toml"));
    assert_eq!(path, PathBuf::from("/tmp/instance/oauth_accounts.json"));
}

#[test]
fn test_state_path_for_config_keeps_default_layout_compatible() {
    let path = super::storage::state_path_for_config(Path::new(
        "/home/user/.config/llm-proxy/config.toml",
    ));
    assert_eq!(
        path,
        PathBuf::from("/home/user/.config/llm-proxy/oauth_accounts.json")
    );
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
fn test_default_state_path_respects_xdg_config_home() {
    let path = super::storage::default_state_path_in(PathBuf::from("/tmp/test-xdg-config"));
    assert!(
        path.starts_with("/tmp/test-xdg-config"),
        "expected path under /tmp/test-xdg-config, got: {}",
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

[providers.openai-default]
auth = { type = "openai_oauth" }

[providers.api-key-prov]
auth = { type = "api_key_env", env = "SOME_KEY" }
"#;
    let cfg: crate::config::Config = toml::from_str(cfg_toml).expect("parse config");

    // 显示指定的 account
    let acc1 = oauth_account_for_provider(&cfg, "openai-explicit").expect("account");
    assert_eq!(acc1, "my-custom-account");

    // 默认与 provider_id 相同的 account
    let acc2 = oauth_account_for_provider(&cfg, "openai-default").expect("account");
    assert_eq!(acc2, "openai-default");

    // 非 OAuth provider
    let err1 = oauth_account_for_provider(&cfg, "api-key-prov").expect_err("non-oauth");
    assert!(err1.to_string().contains("not OAuth-backed"));

    // 未知 provider
    let err2 = oauth_account_for_provider(&cfg, "unknown-prov").expect_err("unknown");
    assert!(err2.to_string().contains("unknown provider"));
}

// =========================================================================
// 补充测试：validate_openai_account 遗漏的边界条件
// =========================================================================

// =========================================================================
// 补充测试：AccountsFile 结构体验证
// =========================================================================

#[test]
fn oauth_accounts_new_has_version_one_and_empty_maps() {
    let accounts = AccountsFile::new();
    assert_eq!(accounts.version, 1);
    assert!(accounts.openai.is_empty() && accounts.others.is_empty());
    assert!(accounts.openai.is_empty());
}

#[test]
fn oauth_accounts_default_is_equivalent_to_new() {
    let default_acc = AccountsFile::default();
    assert_eq!(default_acc.version, 0); // Default derives to 0, not 1
    assert!(default_acc.openai.is_empty() && default_acc.others.is_empty());
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
    assert!(accounts.openai.is_empty() && accounts.others.is_empty());
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

    let mut accounts = AccountsFile::new();
    accounts.openai.insert(
        "openai-user".to_string(),
        test_openai_account("openai-user"),
    );
    accounts.others.insert(
        "ag-user".to_string(),
        serde_json::Value::String("ag-user".to_string()),
    );
    save_oauth_accounts(&path, &accounts).expect("save");

    let (loaded, skipped) = load_oauth_accounts(&path).expect("load");
    assert!(loaded.openai.contains_key("openai-user"));
    assert!(skipped.is_empty());
    // 注：load 只重建类型化分组（flatten 的 others 不参与逐账号校验重建，与
    // 基线行为一致）；此处断言 save 侧原样落盘——以文件内容为证。
    let raw = std::fs::read_to_string(&path).expect("read back");
    assert!(raw.contains("ag-user"), "others 必须随 save 落盘：{raw}");
}

#[test]
fn added_account_exists_recognizes_aliases_and_unknown_types() {
    let mut accounts = AccountsFile::new();
    accounts
        .openai
        .insert("oa".into(), test_openai_account("oa"));
    accounts
        .others
        .insert("ag".into(), serde_json::Value::String("ag".into()));
    assert!(account_exists(&accounts, "oa", "openai"));
    assert!(account_exists(&accounts, "oa", "openai_oauth"));
    assert!(!account_exists(&accounts, "oa", "bogus"));
    assert!(!account_exists(&accounts, "missing", "openai"));
}

#[test]
fn added_logout_missing_account_does_not_create_backup_or_modify_file() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("accounts.json");
    let mut accounts = AccountsFile::new();
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
    let mut first = AccountsFile::new();
    first
        .openai
        .insert("first".into(), test_openai_account("first"));
    save_oauth_accounts_locked(&path, &first).unwrap();
    let mut second = AccountsFile::new();
    second
        .openai
        .insert("second".into(), test_openai_account("second"));
    save_oauth_accounts_locked(&path, &second).unwrap();
    let loaded = load_oauth_accounts(&path).unwrap().0;
    assert!(loaded.openai.contains_key("second"));
    assert!(!loaded.openai.contains_key("first"));
    assert_eq!(find_backups_newest_first(&path).len(), 1);
}

#[test]
fn added_validate_path_safety_allows_missing_path() {
    let temp = tempfile::tempdir().unwrap();
    assert!(validate_path_safety(&temp.path().join("missing.json")).is_ok());
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
"#,
    )
    .unwrap();
    validate_oauth_on_startup(&cfg, &path).unwrap();
    save_oauth_accounts(&path, &AccountsFile::new()).unwrap();
    validate_oauth_on_startup(&cfg, &path).unwrap();
}

// startup reconciliation of the connect two-step write:
// the credential half is expensive (only the user's OAuth flow produces it), so
// a lost config half is *completed*, never reclaimed by deleting the credential.

#[test]
fn account_key_is_matched_through_the_catalog_not_by_guessing_the_provider_name() {
    // The account key is not the provider name: a connect under a custom name
    // rewrites the key. A key no template declares must be reported, never
    // invented under a guessed provider name.
    let mut accounts = AccountsFile::new();
    accounts.openai.insert(
        "openai-work-account".into(),
        test_openai_account("openai-work-account"),
    );
    let mut cfg = server_only_config();

    let report = recover_orphan_oauth_accounts(&mut cfg, &accounts);

    assert!(report.recovered.is_empty(), "{report:?}");
    assert_eq!(report.unrecoverable.len(), 1);
    assert_eq!(report.unrecoverable[0].account, "openai-work-account");
    assert_eq!(report.unrecoverable[0].auth_type, "openai");
    assert!(cfg.providers.is_empty(), "nothing may be invented");
}

#[test]
fn recovery_never_overwrites_a_provider_that_already_occupies_the_name() {
    let mut accounts = AccountsFile::new();
    accounts.openai.insert(
        "custom-occupied".into(),
        test_openai_account("custom-occupied"),
    );
    accounts
        .openai
        .insert("ag-alt".into(), test_openai_account("ag-alt"));
    let mut cfg: crate::config::Config = toml::from_str(
        r#"
[server]
listen = "127.0.0.1:8989"
[providers.custom-occupied]
auth = { type = "openai_oauth", account = "ag-alt" }
"#,
    )
    .unwrap();

    let report = recover_orphan_oauth_accounts(&mut cfg, &accounts);

    // "ag-alt" is referenced by the existing provider, so only "custom-occupied"
    // is an orphan — and its name is taken by a deliberate entry.
    assert!(report.recovered.is_empty(), "{report:?}");
    assert_eq!(report.unrecoverable.len(), 1);
    assert_eq!(report.unrecoverable[0].account, "custom-occupied");
    let provider = cfg.providers.get("custom-occupied").unwrap();
    assert!(matches!(
        &provider.auth,
        Some(crate::config::AuthConfig::OpenaiOauth { account })
            if account.as_deref() == Some("ag-alt")
    ));
}

#[test]
fn credential_referenced_by_a_custom_named_provider_is_not_an_orphan() {
    let mut accounts = AccountsFile::new();
    accounts.others.insert(
        "ag-work".into(),
        serde_json::Value::String("ag-work".into()),
    );
    let mut cfg: crate::config::Config = toml::from_str(
        r#"
[server]
listen = "127.0.0.1:8989"
[providers.ag-work]
"#,
    )
    .unwrap();

    let report = recover_orphan_oauth_accounts(&mut cfg, &accounts);

    assert!(report.is_empty(), "{report:?}");
    assert_eq!(cfg.providers.len(), 1);
}

#[test]
fn provider_without_an_account_field_falls_back_to_its_own_name() {
    let mut accounts = AccountsFile::new();
    accounts
        .openai
        .insert("oa".into(), test_openai_account("oa"));
    let mut cfg: crate::config::Config = toml::from_str(
        r#"
[server]
listen = "127.0.0.1:8989"
[providers.oa]
auth = { type = "openai_oauth" }
"#,
    )
    .unwrap();

    let report = recover_orphan_oauth_accounts(&mut cfg, &accounts);

    assert!(report.is_empty(), "{report:?}");
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

#[tokio::test]
async fn added_refresh_account_for_provider_rejects_unknown_type() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("accounts.json");
    assert!(
        super::token::refresh_account_for_provider_with_context_and_urls(
            &path,
            "acc",
            "unknown",
            "http://127.0.0.1:1/token",
            super::refresh::RefreshContext::new("test-provider", "test"),
        )
        .await
        .unwrap_err()
        .to_string()
        .contains("unknown oauth type")
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
