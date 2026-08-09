use crate::{admin_client, config};
use anyhow::{Context, Result};

/// 构造 launch 用 Config：
/// - 本地模式（config 有 providers/models）→ 直接读盘（现状）
/// - 远程模式（config 极简）→ 从 server client-config 获取 models/providers，
///   server.listen 覆盖为 CLI 视角地址（base_url 的 host:port）。
pub async fn launch_config(config_path: &std::path::Path, client: &str) -> Result<config::Config> {
    let local = config::Config::load(config_path)?;
    let is_remote = local.providers.is_empty() && local.models.is_empty();
    if !is_remote {
        return Ok(local);
    }
    match admin_client::detect_server(config_path).await {
        Ok(Some(server)) => {
            let data = server.client_config(client).await?;
            config_from_client_data(&data, &server.base_url)
        }
        Ok(None) => {
            anyhow::bail!(
                "server unreachable and no local config for launch (remote mode); \
                 start the server or use a config with models"
            )
        }
        Err(e) => Err(e),
    }
}

/// 从 client-config 响应构造 Config：models/providers 来自 server，
/// server.listen 用 CLI 视角的 base_url host:port（launch 地址推导正确）。
fn config_from_client_data(data: &serde_json::Value, base_url: &str) -> Result<config::Config> {
    let d = data.get("data").context("missing client-config data")?;
    let listen = url::Url::parse(base_url)
        .ok()
        .and_then(|u| {
            u.host_str()
                .map(|host| format!("{host}:{}", u.port().unwrap_or(8989)))
        })
        .unwrap_or_else(|| "127.0.0.1:8989".to_string());
    let cfg = serde_json::from_value(serde_json::json!({
        "server": { "listen": listen },
        "models": d.get("models").cloned().unwrap_or(serde_json::json!({})),
        "providers": d.get("providers").cloned().unwrap_or(serde_json::json!({})),
    }))
    .context("failed to build config from client-config data")?;
    Ok(cfg)
}
