//! facade/registry/source 接线测试。

use super::traits::{AuthCtx, UpstreamUrls};
use super::{Registry, get_token_valid, get_token_valid_inject, peek_token};
use crate::auth::traits::{AuthKind, TokenQuery};
use crate::auth::{AccountsFile, AuthRegistry, OpenaiAccount, save_oauth_accounts};
use crate::config::ProviderConfig;
use serde_json::json;
use std::path::Path;

fn provider(raw: serde_json::Value) -> ProviderConfig {
    serde_json::from_value(raw).expect("provider config")
}

pub(super) fn seed_openai(path: &Path, account: &str, expired: bool) {
    let now = chrono::Utc::now().timestamp();
    let mut accounts = AccountsFile::new();
    accounts.openai.insert(
        account.to_string(),
        OpenaiAccount {
            account_label: "p3@example.com".to_string(),
            access_token: "p3-initial-access-token-000001".to_string(),
            refresh_token: "p3-initial-refresh-token-000001".to_string(),
            expires_at_unix: if expired { now - 3600 } else { now + 3600 },
            updated_at_unix: if expired { now - 7200 } else { now },
        },
    );
    save_oauth_accounts(path, &accounts).expect("seed save");
}

/// registry 构造 + 3→3 kind 路由 + apikey facade 端到端。
#[test]
#[serial_test::serial]
fn registry_routes_and_apikey_facade_e2e() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("oauth_accounts.json");
    let registry = Registry::new(&path);
    // kind 路由：无存储文件时 source peek → login 引导错误
    let openai_cfg = provider(json!({"auth": {"type": "openai_oauth"}}));
    let err = registry
        .source(AuthKind::OpenaiOauth)
        .peek_token(&TokenQuery::resolve("prov", &openai_cfg).expect("resolve"))
        .expect_err("no storage file");
    assert!(err.to_string().contains("requires OAuth login"), "{err}");
    // apikey 经 facade（env 注入语义）
    unsafe {
        std::env::set_var("LLM_PROXY_FACADE_KEY", "p3-key-value");
    }
    let api_cfg = provider(json!({"auth": {"type": "api_key_env", "env": "LLM_PROXY_FACADE_KEY"}}));
    assert_eq!(
        get_token_valid("prov", &api_cfg, &path).expect("apikey facade"),
        "p3-key-value"
    );
}

/// 生产 facade：新鲜 token 只读（文件字节不变、无备份产生）；peek 同源。
#[test]
fn facade_fresh_token_is_read_only_and_peek_agrees() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("oauth_accounts.json");
    seed_openai(&path, "prov", false);
    let before = std::fs::read(&path).expect("read seed");
    let cfg = provider(json!({"auth": {"type": "openai_oauth"}}));
    let token = get_token_valid("prov", &cfg, &path).expect("fresh token");
    assert_eq!(token, "p3-initial-access-token-000001");
    assert_eq!(
        std::fs::read(&path).expect("read after"),
        before,
        "fresh path must not rewrite the file"
    );
    let entries: Vec<_> = std::fs::read_dir(dir.path())
        .expect("dir")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        entries
            .iter()
            .all(|n| !n.contains(".bak") && !n.ends_with(".tmp")),
        "no backup/tmp on read-only path: {entries:?}"
    );
    assert_eq!(peek_token("prov", &cfg, &path).expect("peek"), token);
}

/// 注入入口：过期条目 → 经 testserver 刷新（上游地址显式注入）→ 写回。
#[test]
fn inject_expired_refreshes_via_testserver_and_writes_back() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let (addr, hits) = rt.block_on(super::refresh_concurrency_tests::spawn_refresh_server(0));
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("oauth_accounts.json");
    seed_openai(&path, "prov", true);
    let cfg = provider(json!({"auth": {"type": "openai_oauth"}}));
    let client = reqwest::Client::new();
    let ctx = AuthCtx {
        now: chrono::Utc::now(),
        http: &client,
        upstream: UpstreamUrls {
            token_url: Some(format!("http://{addr}/token")),
            userinfo_url: Some(format!("http://{addr}/token")),
        },
    };
    let token = rt
        .block_on(async {
            // 同步 facade 在 async 上下文内执行（block_in_place + block_on 路径）
            get_token_valid_inject(&ctx, "prov", &cfg, &path)
        })
        .expect("refresh via testserver");
    assert!(token.starts_with("upstream-serial-"), "got {token}");
    assert_eq!(
        hits.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "one upstream hit"
    );
    let after = std::fs::read(&path).expect("read after refresh");
    let parsed: AccountsFile = serde_json::from_slice(&after).expect("parseable snapshot");
    assert_eq!(
        parsed.openai["prov"].access_token, token,
        "write-back persisted"
    );
}
