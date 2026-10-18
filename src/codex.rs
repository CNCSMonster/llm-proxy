use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Serialize;
use sha2::{Digest, Sha256};
use toml_edit::{DocumentMut, table, value};
use url::Url;

use crate::config::{Config, ModelCapabilityBounds, Protocol};

#[derive(Debug, Serialize)]
struct Catalog {
    models: Vec<ModelInfo>,
}

#[derive(Debug, Serialize)]
struct ModelInfo {
    slug: String,
    display_name: String,
    description: String,
    default_reasoning_level: String,
    supported_reasoning_levels: Vec<ReasoningLevel>,
    shell_type: String,
    visibility: String,
    supported_in_api: bool,
    priority: i64,
    additional_speed_tiers: Vec<String>,
    service_tiers: Vec<ServiceTier>,
    default_service_tier: Option<String>,
    availability_nux: Option<serde_json::Value>,
    upgrade: Option<serde_json::Value>,
    base_instructions: String,
    model_messages: Option<serde_json::Value>,
    supports_reasoning_summaries: bool,
    default_reasoning_summary: String,
    support_verbosity: bool,
    default_verbosity: Option<serde_json::Value>,
    apply_patch_tool_type: String,
    web_search_tool_type: String,
    truncation_policy: TruncationPolicy,
    supports_parallel_tool_calls: bool,
    supports_image_detail_original: bool,
    context_window: i64,
    max_context_window: i64,
    auto_compact_token_limit: i64,
    effective_context_window_percent: i64,
    experimental_supported_tools: Vec<String>,
    input_modalities: Vec<String>,
    supports_search_tool: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    auto_review_model_override: Option<String>,
}

#[derive(Debug, Serialize)]
struct ServiceTier {
    id: String,
    name: String,
    description: String,
}

#[derive(Debug, Serialize)]
struct ReasoningLevel {
    effort: String,
    description: String,
}

#[derive(Debug, Serialize)]
struct TruncationPolicy {
    mode: String,
    limit: i64,
}

pub fn launch_codex(
    cfg: &Config,
    codex_config_dir: Option<PathBuf>,
    dry_run: bool,
    config_dir: Option<&Path>,
) -> Result<()> {
    cfg.validate()?;
    // : 支持远程模式，不再强制要求 [server]

    let (proxy_url, provider_id, catalog_filename) = {
        let raw_proxy_url = crate::config::protocol_base_url(cfg, "/openai/v1");
        let proxy_url = ensure_responses_v1_url(&raw_proxy_url)?;
        let provider_id = provider_id_for_proxy_url(&proxy_url)?;
        let catalog_filename = catalog_filename_for_proxy_url(&proxy_url)?;
        (proxy_url, provider_id, catalog_filename)
    };
    let codex_dir = match codex_config_dir {
        Some(dir) => dir,
        None => crate::cli::launch_spec::resolve_current_default_dir("codex")
            .map_err(|e| anyhow::anyhow!(e))?,
    };
    let catalog_path = codex_dir.join(&catalog_filename);

    let catalog = generate_catalog(cfg);
    let catalog_json =
        serde_json::to_string_pretty(&catalog).context("failed to serialize Codex catalog")? + "\n";

    println!("Codex config dir: {}", codex_dir.display());
    println!("Proxy URL: {proxy_url}");
    println!("Models in catalog: {}", catalog.models.len());

    // 凭证与 env_key 处理：
    // 1. 远端模式（Client-Only 模式）：
    //    - Token 模式：env_key 直接采用 remote.auth_token_env 配置（按照 client 的 llm-proxy 配置来），
    //      绝不写入 .env 文件，不落盘明文 key；
    //    - Open 模式：不需要凭证，config.toml 中移除 env_key，不写 .env 文件。
    // 2. 本地模式（Serve in same board 模式）：
    //    - Local Token 模式（本地配置了 tokens.toml 或 server auth token）：写入 env_key = "LLM_PROXY_API_KEY"，
    //      此时才允许写入包含本地/mock token 的 .env 文件，避免本机找不到环境变量造成影响；
    //    - Local Open 模式（未配置 tokens.toml）：不需要凭证，config.toml 中移除 env_key，不写 .env 文件。
    let auth_mode = cfg.effective_remote_auth_mode();
    let (env_key_name, local_env_file_token): (Option<String>, Option<String>) = match auth_mode {
        Some(crate::config::RemoteAuthMode::Token) => {
            let env_var_name = cfg
                .remote
                .as_ref()
                .map(|r| r.auth_token_env.clone())
                .unwrap_or_else(|| "LLM_PROXY_API_KEY".to_string());
            // P1（评审整改）：凭证不落盘，运行期由客户端环境注入——launch 时明示用户注入点，
            // 避免 codex 启动时才看到晦涩的 Missing environment variable。
            println!(
                "Remote token mode: token is NOT written to disk; set env var {env_var_name} in the Codex client runtime before starting codex."
            );
            (Some(env_var_name), None)
        }
        Some(crate::config::RemoteAuthMode::Open) => (None, None),
        None => {
            if let Some(token) = resolve_local_codex_token(cfg, config_dir) {
                (Some("LLM_PROXY_API_KEY".to_string()), Some(token))
            } else {
                (None, None)
            }
        }
    };

    let env_path = codex_dir.join(".env");
    if dry_run {
        if local_env_file_token.is_some() {
            println!("[dry-run] would write {}", env_path.display());
            println!("[dry-run] LLM_PROXY_API_KEY=<token>");
        }
        println!("[dry-run] would write {}", catalog_path.display());
        println!(
            "[dry-run] would update {}",
            codex_dir.join("config.toml").display()
        );
        return Ok(());
    }

    fs::create_dir_all(&codex_dir)
        .with_context(|| format!("failed to create Codex home {}", codex_dir.display()))?;

    // 复核整改：非 Local Token 模式下清除旧 .env，防模式切换后残留凭证
    // （旧 token 可能被 codex 读作同名 env_key 的 fallback，或其它变量引入意外鉴权）。
    if local_env_file_token.is_none() && env_path.exists() {
        fs::remove_file(&env_path)
            .with_context(|| format!("failed to remove stale {}", env_path.display()))?;
        println!("Removed stale Codex .env: {}", env_path.display());
    }

    let previous_catalog = fs::read(&catalog_path).ok();
    crate::config_edit::atomic_write(&catalog_path, catalog_json.as_bytes())?;

    let config_path = codex_dir.join("config.toml");
    let previous_config = fs::read(&config_path).ok();
    if let Err(err) = update_codex_config(
        &codex_dir,
        &provider_id,
        &proxy_url,
        &catalog_filename,
        env_key_name.as_deref(),
    ) {
        match previous_catalog {
            Some(previous) => crate::config_edit::atomic_write(&catalog_path, &previous),
            None => fs::remove_file(&catalog_path)
                .with_context(|| format!("failed to remove {}", catalog_path.display())),
        }
        .with_context(|| format!("failed to restore catalog after config update error: {err}"))?;
        return Err(err);
    }

    // 复核整改：config 更新成功后才写 .env；.env 写入（含 0600 权限）失败时回滚
    // config 与 catalog，避免半完成态（凭证落盘但无声明 / 有声明但无凭证）。
    // 0600 失败从严报错：凭证文件不容许权限不安全时静默继续。
    if let Some(ref token) = local_env_file_token {
        let env_content = format!("LLM_PROXY_API_KEY={}\n", token);
        let write_env = crate::config_edit::atomic_write(&env_path, env_content.as_bytes());
        #[cfg(unix)]
        let write_env = write_env.and_then(|_| {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&env_path, fs::Permissions::from_mode(0o600))
                .with_context(|| format!("failed to set 0600 on {}", env_path.display()))
        });
        if let Err(err) = write_env {
            // 复核复验补强：回滚时同时清理已落盘但权限不安全的 .env
            let _ = fs::remove_file(&env_path);
            let restore_config = match &previous_config {
                Some(previous) => crate::config_edit::atomic_write(&config_path, previous),
                None => fs::remove_file(&config_path)
                    .with_context(|| format!("failed to remove {}", config_path.display())),
            };
            restore_config
                .and_then(|_| match previous_catalog {
                    Some(previous) => crate::config_edit::atomic_write(&catalog_path, &previous),
                    None => fs::remove_file(&catalog_path)
                        .with_context(|| format!("failed to remove {}", catalog_path.display())),
                })
                .with_context(|| {
                    format!("failed to restore state after .env write error: {err}")
                })?;
            return Err(err);
        }
        println!("Codex .env written: {}", env_path.display());
    }

    println!("Codex catalog written: {}", catalog_path.display());
    println!(
        "Codex config updated: {}",
        codex_dir.join("config.toml").display()
    );
    Ok(())
}

/// 解析本地模式下 codex 需要的 token 值。
///
/// 尝试从 config_dir 中的 tokens.toml（或 tokens/ 目录）读取第一个 token 的 key；
/// 其次尝试从 server 配置中解析 token。
/// 若开启了 require_auth 且无具体 token，返回本机 mock 占位值。
/// （此处只检查显式 `require_auth = true`；自动推断的鉴权要求由 `validate()` 前置拦住，
/// 不会到达本函数的 mock 分支。）
/// 未配置 token 且未强制要求认证时，返回 None（进入 Local Open 模式）。
fn resolve_local_codex_token(cfg: &Config, config_dir: Option<&Path>) -> Option<String> {
    if let Some(dir) = config_dir
        && let Ok((tokens_config, _source)) = crate::token::config::TokensConfig::load_auto(dir)
        && let Some(first_token) = tokens_config.tokens.first()
    {
        return Some(first_token.key.clone());
    }
    if let Some(token) = cfg.resolve_server_auth_token() {
        return Some(token);
    }
    if cfg.server.as_ref().and_then(|s| s.require_auth) == Some(true) {
        return Some("lp-mock-local-token".to_string());
    }
    None
}

fn generate_catalog(cfg: &Config) -> Catalog {
    let mut model_ids = cfg
        .models
        .iter()
        .filter(|(model_id, _)| cfg.model_serves_protocol(model_id, Protocol::OpenaiResponses))
        .map(|(model_id, _)| model_id.clone())
        .collect::<Vec<_>>();
    // : filter review targets to models actually declared in config;
    // a review target referencing an unknown model would panic in model_info.
    let review_targets = model_ids
        .iter()
        .filter_map(|model_id| cfg.codex_review_target(model_id))
        .filter(|target| cfg.models.contains_key(*target))
        .map(str::to_string)
        .collect::<std::collections::BTreeSet<_>>();
    if review_targets.is_empty() {
        eprintln!(
            "warning: no Codex auto-review model configured; Codex will use provider-default/fallback review behavior"
        );
    }
    model_ids.extend(review_targets.iter().cloned());
    model_ids.sort();
    model_ids.dedup();

    Catalog {
        models: model_ids
            .iter()
            .enumerate()
            .map(|(idx, model_id)| {
                let review_override = if review_targets.contains(model_id) {
                    None
                } else {
                    cfg.codex_review_target(model_id).map(str::to_string)
                };
                model_info(cfg, model_id, idx as i64, review_override)
            })
            .collect(),
    }
}

fn model_info(
    cfg: &Config,
    model_id: &str,
    index: i64,
    auto_review_model_override: Option<String>,
) -> ModelInfo {
    let model = &cfg.models[model_id];
    // a routing model has no execution metadata of its own; what the
    // client should see is the conservative bound over every reachable target.
    let bounds = cfg
        .model_capability_bounds(model_id)
        .unwrap_or_else(|| ModelCapabilityBounds {
            context_window: model.context_window,
            max_output_tokens: model.max_output_tokens,
            features: model.features.clone(),
        });
    let levels = cfg.client_reasoning_levels(model_id);
    let levels = levels.as_deref();
    let ctx = bounds.context_window;
    let supported_reasoning_levels = levels
        .map(|table| {
            table
                .iter()
                .map(|entry| ReasoningLevel {
                    effort: entry.level().to_string(),
                    description: format!("{} reasoning effort", entry.level()),
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let default_reasoning_level = model
        .default_reasoning_level
        .clone()
        .or_else(|| {
            levels
                .and_then(|table| table.first())
                .map(|entry| entry.level().to_string())
        })
        .unwrap_or_else(|| "none".to_string());
    let service_tier_config = cfg.client_codex_service_tier(model_id);
    let service_tiers = service_tier_config
        .as_ref()
        .map(|config| {
            config
                .tiers
                .iter()
                .map(|tier| ServiceTier {
                    id: tier.clone(),
                    name: if tier == "priority" {
                        "fast".to_string()
                    } else {
                        tier.clone()
                    },
                    description: format!("{tier} service tier"),
                })
                .collect()
        })
        .unwrap_or_default();
    let default_service_tier = service_tier_config
        .as_ref()
        .and_then(|config| config.default_tier.clone());
    ModelInfo {
        slug: model_id.to_string(),
        display_name: model_id.to_string(),
        description: format!("{model_id} via llm-proxy Responses API"),
        default_reasoning_level,
        supported_reasoning_levels,
        shell_type: "shell_command".to_string(),
        visibility: "list".to_string(),
        supported_in_api: true,
        priority: 5 + index * 10,
        additional_speed_tiers: Vec::new(),
        service_tiers,
        default_service_tier,
        availability_nux: None,
        upgrade: None,
        base_instructions: format!(
            "You are Codex, a coding agent powered by {model_id}. You and the user share the same workspace and collaborate to achieve the user's goals."
        ),
        model_messages: None,
        supports_reasoning_summaries: false,
        default_reasoning_summary: "none".to_string(),
        support_verbosity: false,
        default_verbosity: None,
        apply_patch_tool_type: "freeform".to_string(),
        web_search_tool_type: "text".to_string(),
        truncation_policy: TruncationPolicy {
            mode: "tokens".to_string(),
            limit: (ctx / 8).min(8192),
        },
        supports_parallel_tool_calls: model_supports_tools(&bounds.features),
        supports_image_detail_original: false,
        context_window: ctx,
        max_context_window: ctx,
        auto_compact_token_limit: (ctx as f64 * 0.9) as i64,
        effective_context_window_percent: 95,
        experimental_supported_tools: Vec::new(),
        input_modalities: if bounds.features.iter().any(|f| f == "image_input") {
            vec!["text".to_string(), "image".to_string()]
        } else {
            vec!["text".to_string()]
        },
        supports_search_tool: false,
        auto_review_model_override,
    }
}

fn model_supports_tools(features: &[String]) -> bool {
    features
        .iter()
        .any(|feature| matches!(feature.as_str(), "tools" | "tool_call_reasoning"))
}

fn update_codex_config(
    codex_dir: &std::path::Path,
    provider_id: &str,
    proxy_url: &str,
    catalog_filename: &str,
    env_key: Option<&str>,
) -> Result<()> {
    let path = codex_dir.join("config.toml");
    let mut doc = if path.exists() {
        fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?
            .parse::<DocumentMut>()
            .with_context(|| format!("failed to parse {}", path.display()))?
    } else {
        DocumentMut::new()
    };

    doc["model_provider"] = value(provider_id);
    doc["model_catalog_json"] = value(catalog_filename);
    doc["features"]["remote_compaction_v2"] = value(false);
    doc["features"]["auto_compaction"] = value(false);
    if doc.get("model_providers").is_none() {
        doc["model_providers"] = table();
    }
    doc["model_providers"][provider_id]["name"] = value("LLM Proxy");
    doc["model_providers"][provider_id]["base_url"] = value(proxy_url);
    doc["model_providers"][provider_id]["wire_api"] = value("responses");
    doc["model_providers"][provider_id]["supports_websockets"] = value(false);
    // 注入或清理 env_key：
    // 当 env_key 为 Some 时注入指定的环境变量名；
    // 当 env_key 为 None 时，显式从 model_providers 表中移除旧的 env_key，防止旧配置残留引发 Missing environment variable 报错
    if let Some(env_key_name) = env_key {
        doc["model_providers"][provider_id]["env_key"] = value(env_key_name);
    } else if let Some(provider_table) = doc["model_providers"][provider_id].as_table_like_mut() {
        provider_table.remove("env_key");
    }

    crate::config_edit::atomic_write(&path, doc.to_string().as_bytes())
        .with_context(|| format!("failed to write {}", path.display()))
}

fn ensure_responses_v1_url(raw: &str) -> Result<String> {
    let trimmed = raw.trim_end_matches('/');
    let parsed = Url::parse(trimmed).with_context(|| format!("invalid proxy URL {raw:?}"))?;
    if parsed.scheme().is_empty() || parsed.host_str().is_none() {
        bail!("proxy URL must include scheme and host: {raw:?}");
    }
    if trimmed.ends_with("/openai/v1") || trimmed.ends_with("/responses/v1") {
        return Ok(trimmed.to_string());
    }
    Ok(format!("{trimmed}/openai/v1"))
}

fn server_base_for_hash(proxy_url: &str) -> Result<String> {
    let parsed =
        Url::parse(proxy_url).with_context(|| format!("invalid proxy URL {proxy_url:?}"))?;
    let host = parsed
        .host_str()
        .with_context(|| format!("proxy URL must include host: {proxy_url:?}"))?;
    let port = parsed.port().map(|p| format!(":{p}")).unwrap_or_default();
    Ok(format!(
        "{}://{}{}",
        parsed.scheme().to_lowercase(),
        host.to_lowercase(),
        port
    ))
}

fn provider_id_for_proxy_url(proxy_url: &str) -> Result<String> {
    Ok(format!(
        "llm-proxy-{}",
        short_hash(&server_base_for_hash(proxy_url)?)
    ))
}

fn catalog_filename_for_proxy_url(proxy_url: &str) -> Result<String> {
    Ok(format!(
        "llm-proxy-{}-model-catalog.json",
        short_hash(&server_base_for_hash(proxy_url)?)
    ))
}

fn short_hash(input: &str) -> String {
    let digest = Sha256::digest(input.as_bytes());
    hex::encode(digest)[..8].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config;
    use serde_json::json;

    #[test]
    fn codex_catalog_injects_review_targets_and_overrides() {
        let mut cfg = config::default_deepseek_config();
        let review_model = cfg.models.get("deepseek-v4-flash-lp").unwrap().clone();
        cfg.models.insert("review-model".to_string(), review_model);
        cfg.codex.auto_review_model = Some("review-model".to_string());
        cfg.codex.model_overrides.insert(
            "deepseek-v4-pro-lp".to_string(),
            config::CodexModelOverride {
                auto_review_model: "deepseek-v4-flash-lp".to_string(),
            },
        );

        let catalog = serde_json::to_value(generate_catalog(&cfg)).expect("catalog json");
        let models = catalog["models"].as_array().expect("models");
        let pro = models
            .iter()
            .find(|model| model["slug"] == "deepseek-v4-pro-lp")
            .expect("pro model");
        assert_eq!(pro["auto_review_model_override"], "deepseek-v4-flash-lp");
        let flash = models
            .iter()
            .find(|model| model["slug"] == "deepseek-v4-flash-lp")
            .expect("flash model");
        assert!(flash.get("auto_review_model_override").is_none());
        let injected = models
            .iter()
            .find(|model| model["slug"] == "review-model")
            .expect("injected review model");
        assert!(injected.get("auto_review_model_override").is_none());
    }

    /// : 验证通过 catalog 模板添加百炼 Qwen 3.7 Plus 后，
    /// launch_codex 生成的 Codex catalog 正确暴露 reasoning_levels 与 default_reasoning_level。
    #[test]
    fn launch_codex_with_catalog_bailian_qwen_preserves_reasoning_levels() {
        use crate::catalog::{apply_catalog_model_defaults, bailian_coding_plan_cn};

        let coding = bailian_coding_plan_cn();
        let mut cfg: Config =
            toml::from_str("[server]\nlisten = \"127.0.0.1:8998\"\n").expect("minimal config");
        cfg.providers.insert(coding.id.to_string(), coding.provider);
        apply_catalog_model_defaults(
            &mut cfg,
            "bailian-coding-plan-cn",
            Some(&["qwen3.7-plus-bailian-lp".to_string()]),
        )
        .expect("apply coding plan defaults");

        let tmp = tempfile::tempdir().expect("tempdir");
        launch_codex(&cfg, Some(tmp.path().to_path_buf()), false, None).expect("launch codex");

        let mut found = false;
        for entry in std::fs::read_dir(tmp.path()).expect("read dir") {
            let entry = entry.expect("entry");
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("json") {
                let content = std::fs::read_to_string(&path).expect("read content");
                let parsed: serde_json::Value = serde_json::from_str(&content).expect("parse json");
                let models = parsed["models"].as_array().expect("models array");
                let qwen = models
                    .iter()
                    .find(|m| m["slug"] == "qwen3.7-plus-bailian-lp")
                    .expect("qwen model present");

                assert_eq!(qwen["default_reasoning_level"], "medium");
                let levels = qwen["supported_reasoning_levels"]
                    .as_array()
                    .expect("supported levels array");
                let efforts: Vec<&str> = levels
                    .iter()
                    .map(|l| l["effort"].as_str().expect("effort str"))
                    .collect();
                assert_eq!(efforts, vec!["low", "medium", "high"]);
                found = true;
            }
        }
        assert!(found, "model catalog json was generated and checked");
    }

    /// : review targets referencing models not in config must be
    /// silently skipped rather than panicking in model_info.
    #[test]
    fn codex_catalog_skips_review_targets_not_in_config() {
        let mut cfg = config::default_deepseek_config();
        // Global review target points to a model that does not exist in config.
        cfg.codex.auto_review_model = Some("ghost-review-model".to_string());

        let catalog = serde_json::to_value(generate_catalog(&cfg)).expect("catalog json");
        let models = catalog["models"].as_array().expect("models");
        // Only the two config models should appear; the ghost must be absent.
        assert_eq!(models.len(), 2);
        assert!(models.iter().all(|m| m["slug"] != "ghost-review-model"));
    }

    /// : per-model review target referencing a model not in config
    /// must be silently skipped.
    #[test]
    fn codex_catalog_skips_per_model_review_target_not_in_config() {
        let mut cfg = config::default_deepseek_config();
        cfg.codex.model_overrides.insert(
            "deepseek-v4-pro-lp".to_string(),
            config::CodexModelOverride {
                auto_review_model: "nonexistent-review-model".to_string(),
            },
        );

        // Must not panic.
        let catalog = serde_json::to_value(generate_catalog(&cfg)).expect("catalog json");
        let models = catalog["models"].as_array().expect("models");
        assert_eq!(models.len(), 2);
        assert!(
            models
                .iter()
                .all(|m| m["slug"] != "nonexistent-review-model")
        );
    }

    #[test]
    fn launch_codex_writes_responses_provider_config_and_catalog() {
        let temp = tempfile::tempdir().expect("tempdir");
        let codex_home = temp.path().join("codex-home");
        let cfg = config::default_deepseek_config();

        launch_codex(&cfg, Some(codex_home.clone()), false, None).expect("launch codex");

        let config_text =
            fs::read_to_string(codex_home.join("config.toml")).expect("read codex config");
        assert!(config_text.contains("model_provider = \"llm-proxy-972f2898\""));
        assert!(!config_text.contains("model = "));
        assert!(!config_text.contains("model_context_window = "));
        assert!(!config_text.contains("model_max_output_tokens = "));
        assert!(config_text.contains("model_catalog_json = "));
        assert!(config_text.contains("supports_websockets = false"));
        assert!(config_text.contains("remote_compaction_v2 = false"));
        assert!(config_text.contains("auto_compaction = false"));
        assert!(config_text.contains("base_url = \"http://127.0.0.1:8989/openai/v1\""));
        // 本地 Open 模式（未配置 tokens.toml）：不生成 env_key，不写 .env
        assert!(!config_text.contains("env_key"));
        assert!(!codex_home.join(".env").exists());

        let catalog_path = codex_home.join("llm-proxy-972f2898-model-catalog.json");
        let catalog_text = fs::read_to_string(catalog_path).expect("read catalog");
        assert!(catalog_text.contains("\"slug\": \"deepseek-v4-pro-lp\""));
        let catalog_json: serde_json::Value =
            serde_json::from_str(&catalog_text).expect("parse catalog");
        let flash = catalog_json["models"]
            .as_array()
            .expect("models")
            .iter()
            .find(|model| model["slug"] == "deepseek-v4-flash-lp")
            .expect("flash model");
        assert_eq!(flash["default_reasoning_level"], "high");
        assert_eq!(flash["supported_reasoning_levels"][4]["effort"], "xhigh");
        assert_eq!(flash["supports_parallel_tool_calls"], true);
    }

    #[test]
    fn launch_codex_preserves_existing_model_selection() {
        let temp = tempfile::tempdir().expect("tempdir");
        let codex_home = temp.path().join("codex-home");
        fs::create_dir_all(&codex_home).expect("create codex home");
        fs::write(
            codex_home.join("config.toml"),
            r#"model = "user-model"
model_provider = "user-provider"
model_context_window = 12345
model_max_output_tokens = 678

[model_providers.user-provider]
base_url = "https://user.example/v1"
wire_api = "responses"
"#,
        )
        .expect("write existing codex config");
        let cfg = config::default_deepseek_config();

        launch_codex(&cfg, Some(codex_home.clone()), false, None).expect("launch codex");

        let config_text =
            fs::read_to_string(codex_home.join("config.toml")).expect("read codex config");
        assert!(config_text.contains("model = \"user-model\""));
        assert!(config_text.contains("model_provider = \"llm-proxy-972f2898\""));
        assert!(
            config_text.contains("model_catalog_json = \"llm-proxy-972f2898-model-catalog.json\"")
        );
        assert!(config_text.contains("model_context_window = 12345"));
        assert!(config_text.contains("model_max_output_tokens = 678"));
        assert!(config_text.contains("base_url = \"https://user.example/v1\""));
        assert!(config_text.contains("wire_api = \"responses\""));
    }

    #[test]
    fn codex_catalog_declares_priority_as_fast_only_when_configured() {
        let provider = crate::catalog::openai_sub();
        let mut cfg = config::default_deepseek_config();
        cfg.providers.clear();
        cfg.models.clear();
        cfg.providers
            .insert(provider.id.to_string(), provider.provider.clone());
        for template in provider.model_templates {
            cfg.models.insert(
                template.frontend_id.to_string(),
                crate::catalog::model_with_supported_protocols(
                    provider.id,
                    &provider.provider,
                    template.upstream_model,
                    crate::catalog::ModelDefaults::from(&template),
                ),
            );
        }
        let catalog = serde_json::to_value(generate_catalog(&cfg)).expect("catalog json");
        let models = catalog["models"].as_array().expect("models");
        let priority_model = models
            .iter()
            .find(|model| model["slug"] == "gpt-5.5-sub-lp")
            .expect("priority model");
        assert_eq!(priority_model["service_tiers"][0]["id"], "priority");
        assert_eq!(priority_model["service_tiers"][0]["name"], "fast");
        assert_eq!(priority_model["default_service_tier"], "priority");
        assert_eq!(priority_model["default_reasoning_level"], "medium");
        let supported_levels: Vec<&str> = priority_model["supported_reasoning_levels"]
            .as_array()
            .expect("reasoning levels")
            .iter()
            .map(|l| l["effort"].as_str().unwrap())
            .collect();
        assert_eq!(supported_levels, vec!["low", "medium", "high", "xhigh"]);

        let mut forced_model = cfg
            .models
            .get("gpt-5.5-sub-lp")
            .expect("configured model")
            .clone();
        forced_model
            .codex_service_tier
            .as_mut()
            .unwrap()
            .default_tier = None;
        forced_model
            .codex_service_tier
            .as_mut()
            .unwrap()
            .forced_tier = Some("priority".to_string());
        cfg.models.insert("gpt-5.5-fast".to_string(), forced_model);
        let forced_catalog = serde_json::to_value(generate_catalog(&cfg)).expect("catalog json");
        let forced_model = forced_catalog["models"]
            .as_array()
            .expect("models")
            .iter()
            .find(|model| model["slug"] == "gpt-5.5-fast")
            .expect("forced model");
        assert_eq!(
            forced_model["service_tiers"],
            json!([{
                "id": "priority",
                "name": "fast",
                "description": "priority service tier"
            }])
        );
        assert!(forced_model["default_service_tier"].is_null());

        let unconfigured_model = models
            .iter()
            .find(|model| model["slug"] == "gpt-5.6-sol-sub-lp")
            .expect("unconfigured model");
        assert!(
            unconfigured_model["service_tiers"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert!(unconfigured_model["default_service_tier"].is_null());
    }

    #[test]
    fn codex_catalog_does_not_fabricate_missing_reasoning_levels() {
        let mut cfg = config::default_deepseek_config();
        let model = cfg.models.get_mut("deepseek-v4-flash-lp").unwrap();
        model.reasoning_levels = None;
        model.default_reasoning_level = None;
        model.features.clear();

        let catalog = serde_json::to_value(generate_catalog(&cfg)).expect("catalog json");
        let flash = catalog["models"]
            .as_array()
            .expect("models")
            .iter()
            .find(|model| model["slug"] == "deepseek-v4-flash-lp")
            .expect("flash model");

        assert_eq!(flash["default_reasoning_level"], "none");
        assert_eq!(
            flash["supported_reasoning_levels"]
                .as_array()
                .unwrap()
                .len(),
            0
        );
        assert_eq!(flash["supports_parallel_tool_calls"], false);
    }

    /// A routing model must appear in the Codex catalog with the metadata a
    /// client should expect: the capabilities every target declares identically
    /// (strict target consistency) and its route-level vocabulary.
    #[test]
    fn codex_catalog_reports_a_routing_models_derived_metadata() {
        let toml_str = r#"
[server]
listen = "127.0.0.1:0"

[providers.p]
openai_responses = { url = "https://example.test/responses" }

[models."target-high"]
context_window = 200_000
max_output_tokens = 64_000
features = ["image_input"]
openai_responses_providers = [{ name = "p", model = "upstream-high" }]

[models."target-low"]
context_window = 200_000
max_output_tokens = 64_000
features = ["image_input"]
openai_responses_providers = [{ name = "p", model = "upstream-low" }]

[models."router-lp"]
reasoning_routes = [
    { level = "low", target = "target-low" },
    { level = "high", target = "target-high" },
]
"#;
        let cfg: Config = toml::from_str(toml_str).expect("parse");
        cfg.validate().expect("validate");
        let catalog = serde_json::to_value(generate_catalog(&cfg)).expect("catalog json");
        let router = catalog["models"]
            .as_array()
            .expect("models")
            .iter()
            .find(|model| model["slug"] == "router-lp")
            .expect("a routing model must be offered to Codex");

        assert_eq!(router["context_window"], 200_000);
        assert_eq!(router["max_context_window"], 200_000);
        assert_eq!(
            router["input_modalities"],
            json!(["text", "image"]),
            "image_input survives because every target declares it"
        );
        assert_eq!(
            router["supports_parallel_tool_calls"], false,
            "no target declares tool_call_reasoning, so it must not be advertised"
        );
        let levels = router["supported_reasoning_levels"]
            .as_array()
            .expect("levels");
        let names = levels
            .iter()
            .map(|level| level["effort"].as_str().expect("effort"))
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["low", "high"]);
    }

    /// A routing model cannot declare a service tier of its own, so the catalog
    /// may only advertise one that every target declares identically.
    #[test]
    fn codex_catalog_derives_a_routing_models_service_tier_from_targets() {
        let toml_str = r#"
[server]
listen = "127.0.0.1:0"

[providers.p]
openai_responses = { url = "https://example.test/responses" }

[models."low"]
context_window = 1000
max_output_tokens = 100
codex_service_tier = { tiers = ["priority"], default_tier = "priority" }
openai_responses_providers = [{ name = "p", model = "upstream-low" }]

[models."high"]
context_window = 1000
max_output_tokens = 100
codex_service_tier = { tiers = ["priority"], default_tier = "priority" }
openai_responses_providers = [{ name = "p", model = "upstream-high" }]

[models."router-lp"]
reasoning_routes = [
    { level = "low", target = "low" },
    { level = "high", target = "high" },
]
"#;
        let cfg: Config = toml::from_str(toml_str).expect("parse");
        cfg.validate().expect("validate");
        let catalog = serde_json::to_value(generate_catalog(&cfg)).expect("catalog json");
        let router = catalog["models"]
            .as_array()
            .unwrap()
            .iter()
            .find(|model| model["slug"] == "router-lp")
            .expect("router");
        assert_eq!(
            router["default_service_tier"], "priority",
            "every target declares the same tier, so the hop can promise it"
        );

        // When the targets disagree, the hop has no single promise to make.
        let mut mixed = cfg.clone();
        mixed
            .models
            .get_mut("high")
            .expect("high")
            .codex_service_tier = Some(config::CodexServiceTierConfig {
            tiers: vec!["priority".to_string()],
            default_tier: None,
            forced_tier: Some("priority".to_string()),
        });
        let catalog = serde_json::to_value(generate_catalog(&mixed)).expect("catalog json");
        let router = catalog["models"]
            .as_array()
            .unwrap()
            .iter()
            .find(|model| model["slug"] == "router-lp")
            .expect("router");
        assert!(
            router["default_service_tier"].is_null(),
            "differing target declarations must not be flattened into one promise"
        );
    }

    /// 远端 Token 模式（Client-Only 模式）：
    /// env_key 字段严格采用配置声明的 auth_token_env 名称，
    /// 且绝不落盘写入 .env 文件，杜绝明文密钥扩散。
    #[test]
    fn codex_remote_token_mode_injects_env_key_and_skips_env_file() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let codex_home = temp_dir.path().to_path_buf();

        let mut cfg = crate::config::default_deepseek_config();
        cfg.server = None;
        cfg.remote = Some(config::RemoteConfig {
            url: "https://example.com".to_string(),
            auth_mode: Some(config::RemoteAuthMode::Token),
            auth_token_env: "CUSTOM_REMOTE_AI_TOKEN_ENV".into(),
        });

        launch_codex(&cfg, Some(codex_home.clone()), false, None).expect("launch should succeed");

        // 验证绝不落盘 .env 文件（保护密钥机密性）
        let env_path = codex_home.join(".env");
        assert!(
            !env_path.exists(),
            ".env file must NOT be written in remote token mode"
        );

        // 验证 config.toml 中 env_key 精确指向配置声明的环境变量名
        let config_text =
            std::fs::read_to_string(codex_home.join("config.toml")).expect("read config");
        assert!(
            config_text.contains("env_key = \"CUSTOM_REMOTE_AI_TOKEN_ENV\""),
            "config.toml must specify the configured auth_token_env as env_key, got: {config_text}"
        );
    }

    /// 远端显式 Open 模式（Client-Only 模式）：
    /// 不需要任何认证凭据，显式清除 config.toml 中残留的 env_key，且不写 .env 文件。
    #[test]
    fn codex_remote_open_mode_cleans_env_key_and_skips_env_file() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let codex_home = temp_dir.path().to_path_buf();

        // 模拟旧配置残留有 env_key
        std::fs::create_dir_all(&codex_home).expect("create dir");
        let stale_config = r#"
model_provider = "llm-proxy-100680ad"

[model_providers.llm-proxy-100680ad]
name = "LLM Proxy"
base_url = "https://example.com/openai/v1"
env_key = "STALE_LEAKED_KEY"
"#;
        std::fs::write(codex_home.join("config.toml"), stale_config).expect("write stale config");

        let mut cfg = crate::config::default_deepseek_config();
        cfg.server = None;
        cfg.remote = Some(config::RemoteConfig {
            url: "https://example.com".to_string(),
            auth_mode: Some(config::RemoteAuthMode::Open),
            auth_token_env: "REMOTE_CODEX_NOEXIST_VAR".into(),
        });

        launch_codex(&cfg, Some(codex_home.clone()), false, None).expect("launch should succeed");

        // 验证未生成 .env 文件
        let env_path = codex_home.join(".env");
        assert!(
            !env_path.exists(),
            ".env file must NOT be created in remote open mode"
        );

        // 验证旧残留的 env_key 已被清除且兄弟字段保全
        let config_text =
            std::fs::read_to_string(codex_home.join("config.toml")).expect("read config");
        assert!(
            !config_text.contains("env_key"),
            "config.toml must NOT contain env_key in open mode (stale key must be removed), got: {config_text}"
        );
        assert!(config_text.contains("name = \"LLM Proxy\""));
        assert!(config_text.contains("base_url = \"https://example.com/openai/v1\""));
    }

    /// 远端隐式推断 Open 模式：
    /// 未显式声明 auth_mode 且凭据环境变量不存在时，推断为 Open 模式，清除 env_key 且不写 .env。
    #[test]
    fn codex_remote_inferred_open_mode_cleans_env_key_and_skips_env_file() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let codex_home = temp_dir.path().to_path_buf();

        // 模拟旧配置残留有 env_key
        std::fs::create_dir_all(&codex_home).expect("create dir");
        let stale_config = r#"
model_provider = "llm-proxy-100680ad"

[model_providers.llm-proxy-100680ad]
name = "LLM Proxy"
base_url = "https://example.com/openai/v1"
env_key = "STALE_INFERRED_KEY"
"#;
        std::fs::write(codex_home.join("config.toml"), stale_config).expect("write stale config");

        let mut cfg = crate::config::default_deepseek_config();
        cfg.server = None;
        cfg.remote = Some(config::RemoteConfig {
            url: "https://example.com".to_string(),
            auth_mode: None, // 隐式推断
            auth_token_env: "REMOTE_CODEX_INFERRED_NOEXIST_VAR".into(),
        });

        launch_codex(&cfg, Some(codex_home.clone()), false, None).expect("launch should succeed");

        let env_path = codex_home.join(".env");
        assert!(
            !env_path.exists(),
            ".env file must NOT be created in inferred remote open mode"
        );

        let config_text =
            std::fs::read_to_string(codex_home.join("config.toml")).expect("read config");
        assert!(
            !config_text.contains("env_key"),
            "config.toml must NOT contain env_key in inferred open mode, got: {config_text}"
        );
        assert!(config_text.contains("name = \"LLM Proxy\""));
    }

    /// 本地 Open 模式（Serve in same board 模式，未配置 tokens.toml）：
    /// 本机代理不需要鉴权，清除 config.toml 中的 env_key，且不写 .env 文件。
    #[test]
    fn codex_local_open_mode_cleans_env_key_and_skips_env_file() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let codex_home = temp_dir.path().join("codex-home");

        // 模拟旧配置残留有 env_key
        std::fs::create_dir_all(&codex_home).expect("create dir");
        let stale_config = r#"
model_provider = "llm-proxy-972f2898"

[model_providers.llm-proxy-972f2898]
name = "LLM Proxy"
base_url = "http://127.0.0.1:8989/openai/v1"
env_key = "STALE_LOCAL_KEY"
"#;
        std::fs::write(codex_home.join("config.toml"), stale_config).expect("write stale config");

        let cfg = crate::config::default_deepseek_config();

        // 本地模式（有 server，无 remote），不传 config_dir（无 tokens.toml）
        launch_codex(&cfg, Some(codex_home.clone()), false, None)
            .expect("launch should succeed in local open mode");

        // 验证未生成 .env 文件
        let env_path = codex_home.join(".env");
        assert!(
            !env_path.exists(),
            ".env file must NOT be created in local open mode"
        );

        // 验证 config.toml 中移除了 env_key
        let config_text =
            std::fs::read_to_string(codex_home.join("config.toml")).expect("read config");
        assert!(
            !config_text.contains("env_key"),
            "config.toml must NOT contain env_key in local open mode, got: {config_text}"
        );
    }

    /// 本地 Token 模式（Serve in same board 模式，配置了 tokens.toml）：
    /// 此时才允许写入包含本机 mock/本地 token 明文的 .env 文件，
    /// 并在 config.toml 中配置 env_key = "LLM_PROXY_API_KEY"，避免影响本机开箱即用。
    #[test]
    fn codex_local_token_mode_with_tokens_toml_writes_env_file() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let config_dir = temp_dir.path().join("config");
        std::fs::create_dir_all(&config_dir).expect("create config dir");

        // 写入 tokens.toml
        let tokens_content = r#"
[[tokens]]
key = "lp-test-token-from-tokens-toml"
name = "test-user"
allowed_models = []
"#;
        std::fs::write(config_dir.join("tokens.toml"), tokens_content).expect("write tokens.toml");

        let codex_home = temp_dir.path().join("codex-home");
        let cfg = crate::config::default_deepseek_config();

        launch_codex(
            &cfg,
            Some(codex_home.clone()),
            false,
            Some(config_dir.as_path()),
        )
        .expect("launch should succeed with tokens.toml");

        // 验证在 local token 模式下才写入 .env 文件
        let env_path = codex_home.join(".env");
        assert!(
            env_path.exists(),
            ".env file must be created in local token mode"
        );
        let env_content = std::fs::read_to_string(&env_path).expect("read .env");
        assert!(
            env_content.contains("LLM_PROXY_API_KEY=lp-test-token-from-tokens-toml"),
            ".env must contain the token from tokens.toml, got: {env_content}"
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let metadata = std::fs::metadata(&env_path).expect("metadata");
            assert_eq!(
                metadata.permissions().mode() & 0o777,
                0o600,
                ".env file must have 0600 permissions"
            );
        }

        // 验证 config.toml 中配置了 env_key
        let config_text =
            std::fs::read_to_string(codex_home.join("config.toml")).expect("read config");
        assert!(
            config_text.contains("env_key = \"LLM_PROXY_API_KEY\""),
            "config.toml must specify LLM_PROXY_API_KEY, got: {config_text}"
        );
    }

    /// 本地 Token 模式（server.require_auth = true，且配置了 auth_token）：
    /// 此时写入本机 token 明文到 .env，配置 env_key = "LLM_PROXY_API_KEY"。
    #[test]
    fn codex_local_token_mode_with_server_require_auth_writes_mock_env_file() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let codex_home = temp_dir.path().join("codex-home");

        let mut cfg = crate::config::default_deepseek_config();
        if let Some(ref mut server) = cfg.server {
            server.require_auth = Some(true);
            server.auth_token = Some("lp-mock-local-token".to_string());
        }

        launch_codex(&cfg, Some(codex_home.clone()), false, None)
            .expect("launch should succeed with server require_auth");

        let env_path = codex_home.join(".env");
        assert!(
            env_path.exists(),
            ".env file must be created with mock token when require_auth = true"
        );
        let env_content = std::fs::read_to_string(&env_path).expect("read .env");
        assert!(
            env_content.contains("LLM_PROXY_API_KEY=lp-mock-local-token"),
            "mock token must be written, got: {env_content}"
        );

        let config_text =
            std::fs::read_to_string(codex_home.join("config.toml")).expect("read config");
        assert!(
            config_text.contains("env_key = \"LLM_PROXY_API_KEY\""),
            "config.toml must contain env_key, got: {config_text}"
        );
    }

    /// 模式切换残留清理（复核整改）：Local Token 写入 .env 后切到 Remote Open，
    /// launch 必须清除旧 .env，防旧 token 被 codex 读作同名 env_key 的 fallback
    /// / 其它变量引入意外鉴权。
    #[test]
    fn codex_mode_switch_from_local_token_to_open_removes_stale_env_file() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let config_dir = temp_dir.path().join("config");
        std::fs::create_dir_all(&config_dir).expect("create config dir");
        std::fs::write(
            config_dir.join("tokens.toml"),
            "[[tokens]]\nkey = \"lp-stale-token\"\nname = \"u\"\nallowed_models = []\n",
        )
        .expect("write tokens.toml");
        let codex_home = temp_dir.path().join("codex-home");

        // 第一次：本地 token 模式（无 remote）→ 写 .env
        let mut cfg = crate::config::default_deepseek_config();
        launch_codex(
            &cfg,
            Some(codex_home.clone()),
            false,
            Some(config_dir.as_path()),
        )
        .expect("launch local token mode");
        assert!(
            codex_home.join(".env").exists(),
            "local token mode writes .env"
        );

        // 切换到 Remote Open（[server] 与 [remote] 互斥，先清 server）→ 旧 .env 必须被清除
        cfg.server = None;
        cfg.remote = Some(crate::config::RemoteConfig {
            url: "http://127.0.0.1:1".to_string(),
            auth_token_env: "LLM_PROXY_API_KEY".to_string(),
            auth_mode: Some(crate::config::RemoteAuthMode::Open),
        });
        launch_codex(
            &cfg,
            Some(codex_home.clone()),
            false,
            Some(config_dir.as_path()),
        )
        .expect("launch remote open mode");
        assert!(
            !codex_home.join(".env").exists(),
            "stale .env must be removed when leaving local token mode"
        );
    }

    /// 多行表格式锁行为（复核裁定）：provider 以 `[model_providers.x]` 多行表
    /// （非单行 inline table）声明 env_key 时，Open 模式清除同样生效。
    #[test]
    fn codex_open_mode_cleans_env_key_in_multiline_provider_table() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let codex_home = temp_dir.path().join("codex-home");
        std::fs::create_dir_all(&codex_home).expect("create dir");
        let stale = r#"
model_provider = "llm-proxy-stale-multiline"

[model_providers.llm-proxy-stale-multiline]
name = "LLM Proxy"
base_url = "http://127.0.0.1:8989/openai/v1"
wire_api = "responses"
env_key = "STALE_KEY"
"#;
        std::fs::write(codex_home.join("config.toml"), stale).expect("write stale config");

        update_codex_config(
            &codex_home,
            "llm-proxy-stale-multiline",
            "http://127.0.0.1:8989/openai/v1",
            "catalog.json",
            None,
        )
        .expect("update config");

        let config_text =
            std::fs::read_to_string(codex_home.join("config.toml")).expect("read config");
        assert!(
            !config_text.contains("env_key"),
            "multiline provider table env_key must be removed, got: {config_text}"
        );
    }
}
