mod login;
mod refresh;
mod status;
mod storage;
mod token;
mod types;

#[cfg(test)]
mod tests;

// Re-export all public API symbols so external callers keep working unchanged.
pub use types::{AntigravityAccount, OAuthAccounts, OpenaiAccount};

#[allow(unused_imports)] // used by tests in other modules
pub use storage::{
    default_state_path, load_oauth_accounts, load_oauth_accounts_with_recovery, save_oauth_accounts,
};
pub(crate) use storage::{save_oauth_accounts_locked, with_locked_accounts};

pub(crate) use token::account_exists;
pub use token::{
    get_antigravity_token, get_antigravity_token_from_accounts, get_openai_token,
    get_openai_token_from_accounts, refresh_account_for_provider,
};

pub use status::{
    ProviderAuthStatus, get_provider_auth_status, logout, logout_provider,
    oauth_account_for_provider, status_rows, validate_oauth_on_startup,
};

pub use login::login_provider;

// TUI 自包含 Antigravity 登录入口：生成 PKCE verifier + 授权 URL；
// 用户粘贴 code 后非交互式完成 exchange + 账号写入。
pub(crate) use login::{generate_antigravity_auth_url, login_antigravity_with_code};

pub use refresh::refresh_provider;
