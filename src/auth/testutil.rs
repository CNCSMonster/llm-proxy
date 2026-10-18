//! 共享测试 fixture（tests 共享 fixture 抽 `#[cfg(test)] testutil`）。
//! `test_openai_account` 留 `tests.rs`，openai 归位时并入。

use crate::auth::types::{OpenaiAccount, unix_secs};

pub(super) fn server_only_config() -> crate::config::Config {
    toml::from_str(
        r#"
[server]
listen = "127.0.0.1:8989"
"#,
    )
    .unwrap()
}

pub(super) fn test_openai_account(account: &str) -> OpenaiAccount {
    let now = unix_secs() as i64;
    OpenaiAccount {
        account_label: format!("{account}@example.com"),
        access_token: format!("access-token-{account}-1234567890"),
        refresh_token: format!("refresh-token-{account}-1234567890"),
        expires_at_unix: now + 3600,
        updated_at_unix: now,
    }
}
