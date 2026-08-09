use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use serde::Serialize;
use sha2::{Digest, Sha256};
use toml_edit::{DocumentMut, value};
use url::Url;

use crate::config::{Config, ModelConfig, Protocol, local_protocol_base_url};

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

pub fn launch_codex(cfg: &Config, codex_home: Option<PathBuf>, dry_run: bool) -> Result<()> {
    cfg.validate()?;

    let (default_model_id, default_model) = cfg
        .default_model_for(Protocol::OpenaiResponses)
        .context("no model exposes openai-responses; Codex requires Responses API")?;

    let codex_dir = codex_home.unwrap_or_else(default_codex_home);
    let raw_proxy_url = default_proxy_url(&cfg.server.listen);
    let proxy_url = ensure_responses_v1_url(&raw_proxy_url)?;
    let provider_id = provider_id_for_proxy_url(&proxy_url)?;
    let catalog_filename = catalog_filename_for_proxy_url(&proxy_url)?;
    let catalog_path = codex_dir.join(&catalog_filename);

    let catalog = generate_catalog(cfg);
    let catalog_json =
        serde_json::to_string_pretty(&catalog).context("failed to serialize Codex catalog")? + "\n";

    println!("Codex home: {}", codex_dir.display());
    println!("Proxy URL: {proxy_url}");
    println!("Default model: {default_model_id}");

    if dry_run {
        println!("[dry-run] would write {}", catalog_path.display());
        println!(
            "[dry-run] would update {}",
            codex_dir.join("config.toml").display()
        );
        return Ok(());
    }

    fs::create_dir_all(&codex_dir)
        .with_context(|| format!("failed to create Codex home {}", codex_dir.display()))?;
    fs::write(&catalog_path, catalog_json)
        .with_context(|| format!("failed to write catalog {}", catalog_path.display()))?;

    update_codex_config(
        &codex_dir,
        &provider_id,
        &proxy_url,
        &catalog_filename,
        default_model_id,
        default_model.context_window,
        default_model.max_output_tokens,
    )?;

    println!("Codex catalog written: {}", catalog_path.display());
    println!(
        "Codex config updated: {}",
        codex_dir.join("config.toml").display()
    );
    Ok(())
}

fn generate_catalog(cfg: &Config) -> Catalog {
    let mut models = Vec::new();
    for (idx, (model_id, model)) in cfg
        .models
        .iter()
        .filter(|(_, model)| model.exposes_protocol(Protocol::OpenaiResponses))
        .enumerate()
    {
        models.push(model_info(model_id, model, idx as i64));
    }
    Catalog { models }
}

fn model_info(model_id: &str, model: &ModelConfig, index: i64) -> ModelInfo {
    let ctx = model.context_window;
    let supported_reasoning_levels = model
        .supported_reasoning_levels
        .iter()
        .map(|level| ReasoningLevel {
            effort: level.clone(),
            description: format!("{level} reasoning effort"),
        })
        .collect::<Vec<_>>();
    let default_reasoning_level = model
        .default_reasoning_level
        .clone()
        .or_else(|| model.supported_reasoning_levels.first().cloned())
        .unwrap_or_else(|| "none".to_string());
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
        supports_parallel_tool_calls: model_supports_tools(model),
        supports_image_detail_original: false,
        context_window: ctx,
        max_context_window: ctx,
        auto_compact_token_limit: (ctx as f64 * 0.9) as i64,
        effective_context_window_percent: 95,
        experimental_supported_tools: Vec::new(),
        input_modalities: if model.features.iter().any(|f| f == "image_input") {
            vec!["text".to_string(), "image".to_string()]
        } else {
            vec!["text".to_string()]
        },
        supports_search_tool: false,
    }
}

fn model_supports_tools(model: &ModelConfig) -> bool {
    model
        .features
        .iter()
        .any(|feature| matches!(feature.as_str(), "tools" | "tool_call_reasoning"))
}

fn update_codex_config(
    codex_dir: &std::path::Path,
    provider_id: &str,
    proxy_url: &str,
    catalog_filename: &str,
    default_model: &str,
    context_window: i64,
    max_output_tokens: i64,
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

    if provider_id != "proxy"
        && let Some(table) = doc["model_providers"].as_table_mut()
    {
        table.remove("proxy");
    }

    doc["model"] = value(default_model);
    doc["model_provider"] = value(provider_id);
    doc["model_catalog_json"] = value(catalog_filename);
    doc["model_context_window"] = value(context_window);
    doc["model_max_output_tokens"] = value(max_output_tokens);
    doc["features"]["remote_compaction_v2"] = value(false);
    doc["features"]["auto_compaction"] = value(false);
    doc["model_providers"][provider_id]["name"] = value("LLM Proxy");
    doc["model_providers"][provider_id]["base_url"] = value(proxy_url);
    doc["model_providers"][provider_id]["wire_api"] = value("responses");
    doc["model_providers"][provider_id]["supports_websockets"] = value(false);

    fs::write(&path, doc.to_string()).with_context(|| format!("failed to write {}", path.display()))
}

fn default_codex_home() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".codex")
}

fn default_proxy_url(listen: &str) -> String {
    local_protocol_base_url(listen, "/openai/v1")
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

    #[test]
    fn launch_codex_writes_responses_provider_config_and_catalog() {
        let temp = tempfile::tempdir().expect("tempdir");
        let codex_home = temp.path().join("codex-home");
        let cfg = config::default_deepseek_config();

        launch_codex(&cfg, Some(codex_home.clone()), false).expect("launch codex");

        let config_text =
            fs::read_to_string(codex_home.join("config.toml")).expect("read codex config");
        // No routes section in v2: the launch default is the first model (in
        // sorted config order) that exposes Responses.
        assert!(config_text.contains("model = \"deepseek-v4-flash-lp\""));
        assert!(config_text.contains("wire_api = \"responses\""));
        assert!(config_text.contains("supports_websockets = false"));
        assert!(config_text.contains("remote_compaction_v2 = false"));
        assert!(config_text.contains("auto_compaction = false"));
        assert!(config_text.contains("base_url = \"http://127.0.0.1:8989/openai/v1\""));

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
        assert_eq!(flash["supported_reasoning_levels"][3]["effort"], "xhigh");
        assert_eq!(flash["supports_parallel_tool_calls"], true);
    }

    #[test]
    fn codex_catalog_does_not_fabricate_missing_reasoning_levels() {
        let mut cfg = config::default_deepseek_config();
        let model = cfg.models.get_mut("deepseek-v4-flash-lp").unwrap();
        model.supported_reasoning_levels.clear();
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
}
