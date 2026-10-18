//! 能力方法消费（`KIND`/`validate_account`）与行为保持断言。
//!
//! 约束：结构重构不破坏任何用户可见功能、不破坏既有功能测试——
//! 本模块钉死两处新消费点的**事件口径**与**非阻断语义**。

use super::Openai;
use super::framework::StorageHandle;
use super::traits::{AuthKind, ProviderAuth};
use super::types::{AccountsFile, OpenaiAccount};
use crate::auth::save_oauth_accounts;

/// 刷新事件 `auth_kind` 字段取值稳定性（改为经 `KIND` 消费后，ops 日志口径逐字不变）。
#[test]
fn auth_kind_event_strings_stable() {
    assert_eq!(AuthKind::OpenaiOauth.as_auth_kind_str(), "openai_oauth");
    assert_eq!(AuthKind::ApiKeyEnv.as_auth_kind_str(), "api_key_env");
    assert_eq!(
        <Openai as ProviderAuth>::KIND.as_auth_kind_str(),
        "openai_oauth"
    );
}

fn seed_openai(path: &std::path::Path, account: &str) {
    let now = chrono::Utc::now().timestamp();
    let mut accounts = AccountsFile::new();
    accounts.openai.insert(
        account.to_string(),
        OpenaiAccount {
            account_label: account.to_string(),
            access_token: "p4-initial-access-token-000001".to_string(),
            refresh_token: "p4-initial-refresh-token-000001".to_string(),
            expires_at_unix: now + 3600,
            updated_at_unix: now,
        },
    );
    save_oauth_accounts(path, &accounts).expect("seed save");
}

/// 写回前校验 = **仅告警不阻断**（行为保持：现状写回无校验，从严报错会改既有行为）。
/// 夹具把 refresh_token 弄成短字符串 → `validate_account` 必然失败 → 写回仍然发生。
/// 断言用**原始字节解析**而非 `load_oauth_accounts`——后者既有语义就是加载时跳过
/// 校验不过的条目（`_skipped`），会把非法条目丢掉；本测试只锁"写回不回退"。
#[test]
fn write_validation_is_nonblocking() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("oauth_accounts.json");
    seed_openai(&path, "prov");
    StorageHandle::new(&path)
        .with_transaction::<Openai, _>(|group| {
            let entry = group.get_mut("prov").expect("seeded entry");
            entry.refresh_token = "short".to_string();
            Ok(())
        })
        .expect("transaction must succeed despite invalid entry");
    let raw = std::fs::read(&path).expect("read back");
    let reloaded: AccountsFile = serde_json::from_slice(&raw).expect("raw snapshot");
    let entry = &reloaded.openai["prov"];
    assert_eq!(
        entry.refresh_token, "short",
        "写回不因校验失败而回退（告警不阻断）"
    );
    assert!(
        <Openai as ProviderAuth>::validate_account("prov", entry).is_err(),
        "夹具必须命中校验失败面（否则本测试失去证明力）"
    );
}
