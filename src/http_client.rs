//! Centralized HTTP client factory with proxy support.
//!
//! All `reqwest::Client` construction goes through this module so that
//! proxy configuration (config file or environment variables) is applied
//! consistently across the entire application.

use std::time::Duration;

use crate::config::Config;

/// Build an HTTP client with proxy support and default settings.
///
/// Proxy resolution order:
/// 1. `[server.proxy]` in config.toml
/// 2. `HTTPS_PROXY` / `https_proxy` environment variable
/// 3. `HTTP_PROXY` / `http_proxy` environment variable
pub fn build(cfg: &Config) -> reqwest::Client {
    apply_proxy(reqwest::Client::builder(), cfg)
        .user_agent("llm-proxy/0.2.0")
        .build()
        .expect("reqwest client")
}

/// Build an HTTP client with a custom timeout, returning an error instead of panicking.
pub fn try_build_with_timeout(
    cfg: &Config,
    timeout: Duration,
) -> Result<reqwest::Client, reqwest::Error> {
    apply_proxy(reqwest::Client::builder(), cfg)
        .user_agent("llm-proxy/0.2.0")
        .timeout(timeout)
        .build()
}

/// Build an HTTP client from environment variables with a custom timeout.
///
/// Use this in contexts where `Config` is not available (e.g. OAuth flows).
/// Reads `HTTPS_PROXY` / `HTTP_PROXY` environment variables.
#[allow(dead_code)]
pub fn build_from_env_with_timeout(timeout: Duration) -> reqwest::Client {
    apply_proxy_from_env(reqwest::Client::builder())
        .user_agent("llm-proxy/0.2.0")
        .timeout(timeout)
        .build()
        .expect("reqwest client")
}

/// Build an HTTP client from environment variables, returning an error instead of panicking.
pub fn try_build_from_env_with_timeout(
    timeout: Duration,
) -> Result<reqwest::Client, reqwest::Error> {
    apply_proxy_from_env(reqwest::Client::builder())
        .user_agent("llm-proxy/0.2.0")
        .timeout(timeout)
        .build()
}

/// Apply proxy configuration to a `ClientBuilder`.
///
/// Reads proxy URL from config or environment, and applies `no_proxy`
/// exclusions. Logs a warning if the proxy URL is invalid but does not
/// fail the build.
fn apply_proxy(builder: reqwest::ClientBuilder, cfg: &Config) -> reqwest::ClientBuilder {
    let proxy_url = cfg
        .server
        .as_ref()
        .and_then(|s| s.proxy.as_ref())
        .map(|p| p.url.clone())
        .or_else(|| std::env::var("HTTPS_PROXY").ok())
        .or_else(|| std::env::var("https_proxy").ok())
        .or_else(|| std::env::var("HTTP_PROXY").ok())
        .or_else(|| std::env::var("http_proxy").ok());

    let Some(url) = proxy_url else {
        return builder;
    };

    match reqwest::Proxy::all(&url) {
        Ok(proxy) => {
            let no_proxy = cfg
                .server
                .as_ref()
                .and_then(|s| s.proxy.as_ref())
                .and_then(|p| p.no_proxy.clone())
                .or_else(|| std::env::var("NO_PROXY").ok())
                .or_else(|| std::env::var("no_proxy").ok());

            let proxy = if let Some(no_proxy_str) = no_proxy {
                proxy.no_proxy(reqwest::NoProxy::from_string(&no_proxy_str))
            } else {
                proxy
            };

            tracing::info!(proxy_url = %url, "HTTP client configured with proxy");
            builder.proxy(proxy)
        }
        Err(e) => {
            tracing::warn!(proxy_url = %url, error = %e, "failed to configure proxy, ignoring");
            builder
        }
    }
}

/// Apply proxy configuration from environment variables only.
fn apply_proxy_from_env(builder: reqwest::ClientBuilder) -> reqwest::ClientBuilder {
    let proxy_url = std::env::var("HTTPS_PROXY")
        .ok()
        .or_else(|| std::env::var("https_proxy").ok())
        .or_else(|| std::env::var("HTTP_PROXY").ok())
        .or_else(|| std::env::var("http_proxy").ok());

    let Some(url) = proxy_url else {
        return builder;
    };

    match reqwest::Proxy::all(&url) {
        Ok(proxy) => {
            let no_proxy = std::env::var("NO_PROXY")
                .ok()
                .or_else(|| std::env::var("no_proxy").ok());

            let proxy = if let Some(no_proxy_str) = no_proxy {
                proxy.no_proxy(reqwest::NoProxy::from_string(&no_proxy_str))
            } else {
                proxy
            };

            builder.proxy(proxy)
        }
        Err(_) => builder,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ProxyConfig, ServerConfig};

    fn minimal_config() -> Config {
        Config {
            server: Some(ServerConfig {
                listen: "127.0.0.1:0".to_string(),
                usage: Default::default(),
                max_sse_buffer_bytes: crate::config::default_max_sse_buffer_bytes(),
                max_output_items: crate::config::default_max_output_items(),
                proxy: None,
                require_auth: None,
                auth_token: None,
                auth_token_env: None,
                management_port: None,
            }),
            remote: None,
            fallback: Default::default(),
            protection: Default::default(),
            diagnostics: Default::default(),
            status: Default::default(),
            codex: Default::default(),
            launch: Default::default(),
            providers: Default::default(),
            models: Default::default(),
        }
    }

    #[test]
    fn build_without_proxy_succeeds() {
        let cfg = minimal_config();
        let _client = build(&cfg);
    }

    #[test]
    fn build_with_proxy_config() {
        let mut cfg = minimal_config();
        cfg.server.as_mut().unwrap().proxy = Some(ProxyConfig {
            url: "http://127.0.0.1:7890".to_string(),
            no_proxy: Some("localhost,127.0.0.1".to_string()),
        });
        let _client = build(&cfg);
    }

    #[test]
    fn build_with_invalid_proxy_does_not_panic() {
        let mut cfg = minimal_config();
        cfg.server.as_mut().unwrap().proxy = Some(ProxyConfig {
            url: "not-a-valid-url".to_string(),
            no_proxy: None,
        });
        let _client = build(&cfg);
    }

    #[test]
    fn build_with_timeout_succeeds() {
        let cfg = minimal_config();
        let _client = try_build_with_timeout(&cfg, Duration::from_secs(30));
    }
}
