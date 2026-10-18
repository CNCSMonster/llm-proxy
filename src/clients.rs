use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::{Value, json};

use crate::config::{
    AdapterKind, Config, ModelCapabilityBounds, ModelConfig, Protocol, ReasoningLevelEntry,
    RemoteAuthMode,
};
use crate::json_edit;

const QWEN_MANAGED_ENV_KEY: &str = "LLM_PROXY_API_KEY";

/// qwen-code's reasoning-effort vocabulary (`REASONING_EFFORT_TIERS` in qwen
/// 0.24.7). `capabilities.reasoning.efforts` may only contain these labels;
/// any other level name (e.g. `minimal`, `ultra`, `none`) is rejected by
/// qwen's `validateReasoningDeclaration`. qwen's `openai-effort` profile
/// forwards a tier to the OpenAI-compatible `reasoning_effort` field verbatim,
/// so a model level outside this set is not expressible by qwen and must be
/// dropped rather than advertised.
const QWEN_REASONING_EFFORT_TIERS: &[&str] = &["low", "medium", "high", "xhigh", "max"];

/// 获取远程模式的凭证值。
///
/// - Open 模式：返回占位值 "local"
/// - Token 模式：返回真实 token（从环境变量读取）；环境变量未设置则返回 Err
/// - 本地模式（无 remote）：返回占位值 "local"
fn get_credential_value(cfg: &Config) -> Result<String> {
    if let Some(auth_mode) = cfg.effective_remote_auth_mode() {
        match auth_mode {
            RemoteAuthMode::Open => Ok("local".to_string()),
            RemoteAuthMode::Token => cfg.resolve_remote_auth_token().ok_or_else(|| {
                anyhow::anyhow!(
                    "Token mode requires environment variable '{}' to be set",
                    resolve_env_key(cfg)
                )
            }),
        }
    } else {
        Ok("local".to_string())
    }
}

/// Resolve the environment variable name that clients should use for the
/// proxy API key.
///
/// Returns `remote.auth_token_env` when a `[remote]` section is configured,
/// otherwise falls back to the default `"LLM_PROXY_API_KEY"`.
fn resolve_env_key(cfg: &Config) -> &str {
    cfg.remote
        .as_ref()
        .map(|r| r.auth_token_env.as_str())
        .unwrap_or(QWEN_MANAGED_ENV_KEY)
}

#[derive(Debug, Serialize)]
struct PiProvider {
    #[serde(rename = "baseUrl")]
    base_url: String,
    api: String,
    #[serde(rename = "apiKey")]
    api_key: String,
    models: Vec<PiModel>,
}

#[derive(Debug, Serialize)]
struct PiModel {
    id: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    name: String,
    #[serde(rename = "contextWindow", skip_serializing_if = "is_zero")]
    context_window: i64,
    #[serde(rename = "maxTokens", skip_serializing_if = "is_zero")]
    max_tokens: i64,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    input: Vec<String>,
    reasoning: bool,
    #[serde(rename = "thinkingLevelMap", skip_serializing_if = "Option::is_none")]
    thinking_level_map: Option<PiThinkingLevelMap>,
    #[serde(skip_serializing_if = "Option::is_none")]
    compat: Option<PiCompat>,
}

#[derive(Debug, Serialize)]
struct PiThinkingLevelMap {
    off: Option<String>,
    minimal: Option<String>,
    low: Option<String>,
    medium: Option<String>,
    high: Option<String>,
    xhigh: Option<String>,
    max: Option<String>,
}

#[derive(Debug, Serialize)]
struct PiCompat {
    #[serde(
        rename = "supportsReasoningEffort",
        skip_serializing_if = "Option::is_none"
    )]
    supports_reasoning_effort: Option<bool>,
    #[serde(
        rename = "supportsDeveloperRole",
        skip_serializing_if = "Option::is_none"
    )]
    supports_developer_role: Option<bool>,
    #[serde(
        rename = "forceAdaptiveThinking",
        skip_serializing_if = "Option::is_none"
    )]
    force_adaptive_thinking: Option<bool>,
}

/// Build the apiKey value for Pi's models.json.
///
/// - Remote Token mode: use `$ENV_VAR` syntax to reference the environment variable
///   (Pi supports this natively, avoiding writing literal tokens to disk)
/// - Remote Open mode or Local mode: use `"local"` placeholder
fn pi_api_key_value(cfg: &Config, credential_value: &str) -> String {
    if cfg.effective_remote_auth_mode() == Some(RemoteAuthMode::Token) {
        // Use $ENV_VAR syntax - Pi will resolve this at runtime
        format!("${}", resolve_env_key(cfg))
    } else {
        // Open mode or local mode: use placeholder
        credential_value.to_string()
    }
}

pub fn launch_pi(cfg: &Config, pi_config_dir: Option<PathBuf>, dry_run: bool) -> Result<()> {
    cfg.validate()?;
    let credential_value = get_credential_value(cfg)?;
    let api_key_value = pi_api_key_value(cfg, &credential_value);
    let protocol_specs = [
        (
            Protocol::OpenaiChatCompletions,
            "llm-proxy-openai-chat",
            "openai-completions",
            "/openai/v1",
        ),
        (
            Protocol::OpenaiResponses,
            "llm-proxy-openai-responses",
            "openai-responses",
            "/openai/v1",
        ),
        (
            Protocol::Anthropic,
            "llm-proxy-anthropic",
            "anthropic-messages",
            "/anthropic",
        ),
    ];
    let providers_to_write: Vec<_> = protocol_specs
        .into_iter()
        .filter_map(|(protocol, provider_name, api, prefix)| {
            let mut models = models_for_protocol(cfg, protocol);
            if protocol == Protocol::Anthropic {
                models.retain(|entry| is_eligible_for_pi_anthropic(cfg, entry.id, entry.model));
            }
            if models.is_empty() {
                return None;
            }
            let provider = match json_edit::to_value(
                PiProvider {
                    base_url: crate::config::protocol_base_url(cfg, prefix),
                    api: api.to_string(),
                    api_key: api_key_value.clone(),
                    models: models
                        .iter()
                        .map(|entry| pi_model(cfg, protocol, entry))
                        .collect(),
                },
                "pi provider",
            ) {
                Ok(provider) => provider,
                Err(err) => return Some(Err(err)),
            };
            Some(Ok((provider_name.to_string(), provider, models.len())))
        })
        .collect::<Result<Vec<_>>>()?;
    if providers_to_write.is_empty() {
        bail!(
            "配置中没有 pi 可用模型；模型需要暴露 openai-chat-completions、openai-responses 或 anthropic"
        );
    }

    let config_dir = match pi_config_dir {
        Some(dir) => dir,
        None => crate::cli::launch_spec::resolve_current_default_dir("pi")
            .map_err(|e| anyhow::anyhow!(e))?,
    };
    let path = config_dir.join("models.json");
    let mut pi_cfg = json_edit::load_object(&path)?;
    let providers = json_edit::ensure_object(&mut pi_cfg, "providers")?;

    // Deep-merge models within each managed provider by model id BEFORE provider-level deep merge.
    // This ensures existing client-only fields on managed models (e.g. voiceOnly) are preserved,
    // and obsolete models are pruned before deep_merge overwrites the models array.
    let mut providers_to_write = providers_to_write;
    for (provider_name, provider_value, _) in &mut providers_to_write {
        if let Some(existing_provider) = providers.get(provider_name)
            && let Some(existing_models) = existing_provider.get("models").and_then(Value::as_array)
            && let Some(new_models) = provider_value.get("models").and_then(Value::as_array)
        {
            let merged = merge_models_by_id(existing_models, new_models);
            if let Some(provider_obj) = provider_value.as_object_mut() {
                provider_obj.insert("models".to_string(), Value::Array(merged));
            }
        }
    }

    // Deep-merge managed providers: existing as base, generated as overlay.
    // This preserves client-only fields on managed providers (e.g. defaultModel),
    // and actively prunes managed providers that are no longer generated (e.g. llm-proxy-anthropic).
    json_edit::merge_managed_object_entries(
        providers,
        |name| name.starts_with("llm-proxy-"),
        providers_to_write
            .iter()
            .map(|(name, provider, _)| (name.clone(), provider.clone())),
    );

    println!("pi models.json: {}", path.display());
    for (name, provider, model_count) in &providers_to_write {
        println!("provider: {name}");
        println!("baseUrl: {}", provider["baseUrl"].as_str().unwrap_or(""));
        println!("models: {model_count}");
    }
    for (model_id, model) in &cfg.models {
        if is_eligible_for_pi_anthropic(cfg, model_id, model)
            && let Some(hint) = pi_anthropic_reasoning_hint(cfg, model_id)
        {
            println!("warning: {hint}");
        }
    }

    if dry_run {
        println!("[dry-run] would write {}", path.display());
        return Ok(());
    }

    json_edit::write_object(&path, &pi_cfg)?;
    Ok(())
}

/// : Build `capabilities.reasoning` for a qwen-code model entry.
///
/// Returns `Some(capabilities_object)` when the model supports reasoning and
/// has at least one active (non-disabled) level; `None` otherwise.
///
/// The output schema matches qwen 0.24.7's `validateReasoningDeclaration`:
/// ```json
/// {"reasoning": {"profile": "openai-effort", "efforts": [...], "defaultEffort": "..."}}
/// ```
///
/// Routing models suppress `off`/`none` levels (aligned with
/// `pi_thinking_level_map`'s routing-model off suppression).
fn qwen_capabilities(cfg: &Config, entry: &LaunchModel<'_>) -> Option<Value> {
    let levels = entry.levels.as_deref()?;
    if !model_supports_reasoning(entry.model, Some(levels)) {
        return None;
    }
    let is_routing = entry.model.is_routing_model();
    let efforts: Vec<&str> = levels
        .iter()
        .map(|e| e.level())
        .filter(|level| {
            // qwen's schema only accepts its own tier vocabulary; drop levels
            // other clients may advertise (`minimal`, `ultra`, `none`, …).
            QWEN_REASONING_EFFORT_TIERS.contains(level)
                && if is_routing {
                    // Routing models have no `none`/`off` route — suppress
                    // disabled levels so the client picks a real effort.
                    !is_disabled_reasoning_level(level)
                } else {
                    true
                }
        })
        .collect();
    if efforts.is_empty() {
        return None;
    }
    let default_effort = cfg
        .client_default_reasoning_level(entry.id)
        .filter(|level| {
            QWEN_REASONING_EFFORT_TIERS.contains(&level.as_str())
                && if is_routing {
                    // Don't default to a disabled level for routing models.
                    !is_disabled_reasoning_level(level)
                } else {
                    true
                }
        })
        .unwrap_or_else(|| efforts[0].to_string());
    Some(json!({
        "reasoning": {
            "profile": "openai-effort",
            "efforts": efforts,
            "defaultEffort": default_effort,
        }
    }))
}

pub fn launch_qwen_code(
    cfg: &Config,
    qwen_config_dir: Option<PathBuf>,
    dry_run: bool,
) -> Result<()> {
    cfg.validate()?;
    // : 支持远程模式，不再强制要求 [server]
    let credential_value = get_credential_value(cfg)?;
    let models = models_for_protocol(cfg, Protocol::OpenaiChatCompletions);
    if models.is_empty() {
        bail!("配置中没有 Qwen Code 可用模型；模型需要暴露 openai-chat-completions");
    }

    let config_dir = match qwen_config_dir {
        Some(dir) => dir,
        None => crate::cli::launch_spec::resolve_current_default_dir("qwen-code")
            .map_err(|e| anyhow::anyhow!(e))?,
    };
    let path = config_dir.join("settings.json");
    let mut settings = json_edit::load_object(&path)?;

    let base_url = crate::config::protocol_base_url(cfg, "/openai/v1");
    let new_items = models
        .iter()
        .map(|entry| {
            let id = entry.id;
            let input_modalities = if entry.bounds.features.iter().any(|f| f == "image_input") {
                json!(["text", "image"])
            } else {
                json!(["text"])
            };
            // : emit capabilities.reasoning for models that declare
            // reasoning levels, mirroring pi_model's thinkingLevelMap but in
            // qwen's schema (profile "openai-effort").
            let capabilities = qwen_capabilities(cfg, entry);
            let mut item = json!({
                "id": id,
                "name": id,
                "description": format!("{id} via llm-proxy"),
                "baseUrl": base_url,
                "envKey": resolve_env_key(cfg),
                "generationConfig": {
                    "contextWindowSize": entry.bounds.context_window,
                    "maxOutputTokens": entry.bounds.max_output_tokens,
                },
                "supportedInputs": input_modalities,
            });
            if let Some(caps) = capabilities {
                item.as_object_mut()
                    .unwrap()
                    .insert("capabilities".to_string(), caps);
            }
            item
        })
        .collect();
    let providers = json_edit::ensure_object(&mut settings, "modelProviders")?;
    json_edit::merge_managed_array_items(
        providers,
        "openai",
        |item| is_qwen_managed_model(item, &base_url),
        new_items,
        |item| item.get("id").and_then(Value::as_str),
    );

    let env_key = resolve_env_key(cfg);
    if cfg.effective_remote_auth_mode() == Some(RemoteAuthMode::Token) {
        // 凭证由用户环境变量提供；不把字面 token 写入 settings，
        // 并清除旧版本可能留下的同名字面值。
        if let Some(env) = settings.get_mut("env").and_then(Value::as_object_mut) {
            env.remove(env_key);
        }
    } else {
        json_edit::set_value(&mut settings, &["env", env_key], json!(credential_value))?;
    }
    json_edit::set_value(
        &mut settings,
        &["security", "auth", "selectedType"],
        json!("openai"),
    )?;
    println!("Qwen Code settings.json: {}", path.display());
    println!("baseUrl[openai]: {base_url}");
    println!("modelProviders: {} llm-proxy models", models.len());

    if dry_run {
        println!("[dry-run] would write {}", path.display());
        return Ok(());
    }

    json_edit::write_object(&path, &settings)?;
    Ok(())
}

/// Build the ANTHROPIC_AUTH_TOKEN value for Claude Code's settings.json.
///
/// - Remote Token mode: use `apiKeyHelper` with `echo $ENV_VAR` to avoid
///   writing literal token values to disk (Claude Code executes this command
///   at runtime to obtain the credential)
/// - Remote Open mode or Local mode: use the placeholder value directly
fn claude_code_auth_token(cfg: &Config, credential_value: &str) -> (String, Option<String>) {
    if cfg.effective_remote_auth_mode() == Some(RemoteAuthMode::Token) {
        // Use apiKeyHelper to avoid writing literal token to disk
        let helper_cmd = format!("echo ${}", resolve_env_key(cfg));
        (credential_value.to_string(), Some(helper_cmd))
    } else {
        // Open mode or local mode: use placeholder directly
        (credential_value.to_string(), None)
    }
}

pub fn launch_claude_code(
    cfg: &Config,
    claude_config_dir: Option<PathBuf>,
    dry_run: bool,
) -> Result<()> {
    cfg.validate()?;
    let credential_value = get_credential_value(cfg)?;
    let (auth_token, api_key_helper) = claude_code_auth_token(cfg, &credential_value);
    let models = models_for_protocol(cfg, Protocol::Anthropic);
    if models.is_empty() {
        bail!("配置中没有 Claude Code 可用模型；需要模型暴露 anthropic provider binding");
    }

    // : four-tier model mapping — Claude Code internally resolves
    // Sonnet/Opus/Haiku/Fable roles to these env vars.  Map every tier to the
    // first available model so sub-Agent and Compact never 404 on a missing
    // tier name.  Users with multiple models can customise afterwards.
    let primary_model = models[0].id;

    // : 一次解析同时产出配置目录与侧车 onboarding 文件，语义由「是否
    // 显式覆盖 CLAUDE_CONFIG_DIR」决定，不再从 config_dir 的路径形状反推
    // （隐式契约红线）。
    let layout = crate::cli::launch_spec::resolve_current_claude_layout(claude_config_dir)
        .map_err(|e| anyhow::anyhow!(e))?;
    let path = layout.config_dir.join("settings.json");
    let mut settings = json_edit::load_object(&path)?;
    let base_url = crate::config::protocol_base_url(cfg, "/anthropic/v1")
        .trim_end_matches("/v1")
        .to_string();

    json_edit::set_value(
        &mut settings,
        &["env", "ANTHROPIC_BASE_URL"],
        json!(base_url),
    )?;
    if let Some(helper) = api_key_helper {
        // Remote Token mode: `apiKeyHelper` 是 Claude Code 的顶层设置项，
        // 由客户端在执行时调用，避免把字面 token 写入 settings.json。
        json_edit::set_value(&mut settings, &["apiKeyHelper"], json!(helper))?;
        if let Some(env) = settings.get_mut("env").and_then(Value::as_object_mut) {
            env.remove("ANTHROPIC_AUTH_TOKEN");
        }
    } else {
        // Open mode or local mode: use placeholder directly; 同时清掉旧的
        // apiKeyHelper，避免它覆盖 ANTHROPIC_AUTH_TOKEN。
        settings.remove("apiKeyHelper");
        json_edit::set_value(
            &mut settings,
            &["env", "ANTHROPIC_AUTH_TOKEN"],
            json!(auth_token),
        )?;
    }
    json_edit::set_value(
        &mut settings,
        &["env", "ANTHROPIC_MODEL"],
        json!(primary_model),
    )?;
    json_edit::set_value(
        &mut settings,
        &["env", "ANTHROPIC_DEFAULT_SONNET_MODEL"],
        json!(primary_model),
    )?;
    json_edit::set_value(
        &mut settings,
        &["env", "ANTHROPIC_DEFAULT_OPUS_MODEL"],
        json!(primary_model),
    )?;
    json_edit::set_value(
        &mut settings,
        &["env", "ANTHROPIC_DEFAULT_HAIKU_MODEL"],
        json!(primary_model),
    )?;
    println!("Claude Code settings.json: {}", path.display());
    println!("ANTHROPIC_BASE_URL: {base_url}");
    println!("ANTHROPIC_MODEL: {primary_model} (all tiers)");

    // : onboarding bypass — write hasCompletedOnboarding=true into the
    // side-car onboarding file to skip the browser OAuth popup on first launch.
    // : 位置由 ClaudeLayout 显式给出，与官方 CLAUDE_CONFIG_DIR 语义一致
    // （给定 --claude-config-dir X 或已设 env → X/.claude.json；全默认 → $HOME/.claude.json）。
    let onboarding_path = layout.onboarding_file;
    let mut onboarding = json_edit::load_object(&onboarding_path)?;
    json_edit::set_value(&mut onboarding, &["hasCompletedOnboarding"], json!(true))?;

    if dry_run {
        println!("[dry-run] would write {}", path.display());
        println!("[dry-run] would write {}", onboarding_path.display());
        return Ok(());
    }

    json_edit::write_object(&path, &settings)?;
    json_edit::write_object(&onboarding_path, &onboarding)?;
    Ok(())
}

pub fn launch_claude_desktop(
    cfg: &Config,
    claude_desktop_config_dir: Option<PathBuf>,
    profile: String,
    dry_run: bool,
) -> Result<()> {
    cfg.validate()?;
    let credential_value = get_credential_value(cfg)?;
    let models = models_for_protocol(cfg, Protocol::Anthropic);
    if models.is_empty() {
        bail!("配置中没有 Claude Desktop 可用模型；需要模型暴露 anthropic provider binding");
    }

    let config_dir = match claude_desktop_config_dir {
        Some(dir) => dir,
        None => crate::cli::launch_spec::resolve_current_default_dir("claude-desktop")
            .map_err(|e| anyhow::anyhow!(e))?,
    };
    let path = config_dir.join("claude_desktop_config.json");
    let mut settings = json_edit::load_object(&path)?;
    let base_url = crate::config::protocol_base_url(cfg, "/anthropic/v1")
        .trim_end_matches("/v1")
        .to_string();
    let model_ids: Vec<_> = models.iter().map(|entry| entry.id.to_string()).collect();

    json_edit::set_value(
        &mut settings,
        &["llmProxy", "providers", "llm-proxy", "type"],
        json!("anthropic"),
    )?;
    json_edit::set_value(
        &mut settings,
        &["llmProxy", "providers", "llm-proxy", "baseUrl"],
        json!(base_url),
    )?;
    json_edit::set_value(
        &mut settings,
        &["llmProxy", "providers", "llm-proxy", "apiKey"],
        json!(credential_value),
    )?;
    json_edit::set_value(
        &mut settings,
        &["llmProxy", "providers", "llm-proxy", "models"],
        json!(model_ids),
    )?;
    json_edit::set_value(
        &mut settings,
        &["llmProxy", "profiles", &profile, "provider"],
        json!("llm-proxy"),
    )?;

    println!("Claude Desktop config: {}", path.display());
    println!(
        "profile: {}",
        settings["llmProxy"]["activeProfile"]
            .as_str()
            .unwrap_or("llm-proxy")
    );
    println!(
        "provider baseUrl: {}",
        settings["llmProxy"]["providers"]["llm-proxy"]["baseUrl"]
            .as_str()
            .unwrap_or("")
    );
    println!("models: {}", models.len());

    if dry_run {
        println!("[dry-run] would write {}", path.display());
        return Ok(());
    }

    json_edit::write_object(&path, &settings)?;
    Ok(())
}

/// : Generate OpenCode provider configuration with JSONC round-trip.
///
/// OpenCode uses JSON/JSONC config at `~/.config/opencode/opencode.json{c}`.
/// We manage only `providers["llm-proxy"]`, preserving all other providers,
/// top-level settings, and comments (via jsonc-parser CST).
pub fn launch_opencode(
    cfg: &Config,
    opencode_config_dir: Option<PathBuf>,
    dry_run: bool,
) -> Result<()> {
    use jsonc_parser::ParseOptions;
    use jsonc_parser::cst::CstRootNode;

    cfg.validate()?;
    // Validate credential availability (errors in token mode if env var unset).
    let _credential_value = get_credential_value(cfg)?;

    let chat_models = models_for_protocol(cfg, Protocol::OpenaiChatCompletions);
    let responses_models = models_for_protocol(cfg, Protocol::OpenaiResponses);

    if chat_models.is_empty() && responses_models.is_empty() {
        bail!(
            "配置中没有 OpenCode 可用模型；模型需要暴露 openai-chat-completions 或 openai-responses"
        );
    }

    let config_dir = match opencode_config_dir {
        Some(dir) => dir,
        None => crate::cli::launch_spec::resolve_current_default_dir("opencode")
            .map_err(|e| anyhow::anyhow!(e))?,
    };
    let path = opencode_config_path(&config_dir);
    let base_url = crate::config::protocol_base_url(cfg, "/openai/v1");

    // Read existing content (empty object if file doesn't exist).
    let existing_text = if path.exists() {
        std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?
    } else {
        String::from("{}")
    };

    let root = CstRootNode::parse(&existing_text, &ParseOptions::default())
        .with_context(|| format!("failed to parse {}", path.display()))?;
    let root_obj = root.object_value_or_set();

    // Ensure "provider" object exists.
    let provider_prop = root_obj.get("provider").unwrap_or_else(|| {
        root_obj.append("provider", jsonc_parser::cst::CstInputValue::Object(vec![]))
    });
    let provider_obj = provider_prop.object_value_or_set();

    // Build the llm-proxy provider value.
    let env_key = resolve_env_key(cfg);
    let provider_value =
        build_opencode_provider_value(&base_url, &chat_models, &responses_models, env_key);

    // Set or append the "llm-proxy" key within provider.
    if let Some(existing) = provider_obj.get("llm-proxy") {
        existing.set_value(provider_value);
    } else {
        provider_obj.append("llm-proxy", provider_value);
    }

    let output = root.to_string();

    // Count models for display.
    let mut seen = std::collections::HashSet::new();
    for m in chat_models.iter().chain(responses_models.iter()) {
        seen.insert(m.id);
    }
    let model_count = seen.len();

    println!("OpenCode config: {}", path.display());
    println!("baseUrl: {base_url}");
    println!("provider.llm-proxy: {model_count} models");

    if dry_run {
        println!("[dry-run] would write {}", path.display());
        return Ok(());
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    std::fs::write(&path, output).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

/// Determine the OpenCode config file path.
///
/// Prefers an existing file (`.jsonc` or `.json`); defaults to `.jsonc` when
/// neither exists.
fn opencode_config_path(home: &std::path::Path) -> PathBuf {
    let jsonc = home.join("opencode.jsonc");
    let json = home.join("opencode.json");
    if jsonc.exists() {
        jsonc
    } else if json.exists() {
        json
    } else {
        jsonc
    }
}

/// Build the `CstInputValue` for the llm-proxy provider object.
fn build_opencode_provider_value(
    base_url: &str,
    chat_models: &[LaunchModel<'_>],
    responses_models: &[LaunchModel<'_>],
    env_key: &str,
) -> jsonc_parser::cst::CstInputValue {
    use jsonc_parser::cst::CstInputValue;

    // Build per-model entries. Models appearing in both protocols get "responses"
    // (newer API); chat-only models get "chat".
    let mut model_entries: Vec<(String, &str)> = Vec::new();
    let mut seen_ids = std::collections::HashSet::new();

    for entry in responses_models {
        if seen_ids.insert(entry.id) {
            model_entries.push((entry.id.to_string(), "responses"));
        }
    }
    for entry in chat_models {
        if seen_ids.insert(entry.id) {
            model_entries.push((entry.id.to_string(), "chat"));
        }
    }
    model_entries.sort_by(|a, b| a.0.cmp(&b.0));

    let models_obj: Vec<(String, CstInputValue)> = model_entries
        .iter()
        .map(|(id, _api)| {
            (
                id.clone(),
                CstInputValue::Object(vec![
                    ("id".to_string(), CstInputValue::String(id.clone())),
                    ("name".to_string(), CstInputValue::String(id.clone())),
                ]),
            )
        })
        .collect();

    CstInputValue::Object(vec![
        (
            "npm".to_string(),
            CstInputValue::String("@ai-sdk/openai-compatible".to_string()),
        ),
        (
            "name".to_string(),
            CstInputValue::String("llm-proxy".to_string()),
        ),
        (
            "env".to_string(),
            CstInputValue::Array(vec![CstInputValue::String(env_key.to_string())]),
        ),
        (
            "options".to_string(),
            CstInputValue::Object(vec![
                (
                    "baseURL".to_string(),
                    CstInputValue::String(base_url.to_string()),
                ),
                (
                    "apiKey".to_string(),
                    CstInputValue::String(format!("${{{env_key}}}")),
                ),
            ]),
        ),
        ("models".to_string(), CstInputValue::Object(models_obj)),
    ])
}

/// Merge model arrays by id, deep-merging matching entries.
///
/// For each new model, if there's an existing model with the same id, deep-merge
/// them (existing as base, new as overlay). New models without a match are added.
/// Existing models without a match are dropped (model was removed from proxy).
fn merge_models_by_id(existing: &[Value], new_models: &[Value]) -> Vec<Value> {
    use std::collections::HashMap;

    // Build a map of existing models by id
    let mut existing_by_id: HashMap<String, Value> = HashMap::new();
    for model in existing {
        if let Some(id) = model.get("id").and_then(Value::as_str) {
            existing_by_id.insert(id.to_string(), model.clone());
        }
    }

    let mut result = Vec::new();
    for new_model in new_models {
        let final_model = match new_model.get("id").and_then(Value::as_str) {
            Some(id) => match existing_by_id.remove(id) {
                Some(old_model) => json_edit::deep_merge(old_model, new_model.clone()),
                None => new_model.clone(),
            },
            None => new_model.clone(),
        };
        result.push(final_model);
    }
    result
}

/// A model a client may select for a protocol, together with the metadata the
/// client should see. A routing model is selectable when every one of
/// its targets serves the protocol, and reports the conservative capability
/// bound plus the route level vocabulary over those targets.
struct LaunchModel<'a> {
    id: &'a str,
    model: &'a ModelConfig,
    bounds: ModelCapabilityBounds,
    levels: Option<Vec<ReasoningLevelEntry>>,
}

fn models_for_protocol(cfg: &Config, protocol: Protocol) -> Vec<LaunchModel<'_>> {
    let mut models: Vec<_> = cfg
        .models
        .iter()
        .filter(|(id, _)| cfg.model_serves_protocol(id, protocol))
        .map(|(id, model)| {
            let bounds = cfg
                .model_capability_bounds(id)
                .unwrap_or_else(|| ModelCapabilityBounds {
                    context_window: model.context_window,
                    max_output_tokens: model.max_output_tokens,
                    features: model.features.clone(),
                });
            LaunchModel {
                id: id.as_str(),
                model,
                bounds,
                levels: cfg.client_reasoning_levels(id),
            }
        })
        .collect();
    models.sort_by(|a, b| a.id.cmp(b.id));
    models
}

fn pi_model(cfg: &Config, protocol: Protocol, entry: &LaunchModel<'_>) -> PiModel {
    // Launch metadata uses the *effective* vocabulary: the model's own table, the
    // one inherited from its bound provider (ADR-034 §2 / spec.md §5.2.6), or — for
    // a routing model — its route levels.
    let id = entry.id;
    let model = entry.model;
    let levels = entry.levels.as_deref();
    let reasoning = model_supports_reasoning(model, levels);
    let thinking_level_map = reasoning
        .then(|| pi_thinking_level_map(protocol, model, levels))
        .filter(PiThinkingLevelMap::has_active_level)
        .filter(|_| protocol != Protocol::Anthropic || anthropic_reasoning_path_is_safe(cfg, id));
    // `reasoning` is true when the model supports thinking AND either:
    // - the protocol can express at least one level (thinking_level_map is Some), or
    // - the model forces thinking without level control (e.g. Claude force_think).
    // When the protocol cannot express any level (e.g. Anthropic + xhigh-only model),
    // reasoning is false to prevent the client from sending unrepresentable effort.
    let reasoning = thinking_level_map.is_some() || (reasoning && model.force_think == Some(true));
    let supports_reasoning_effort = (thinking_level_map.is_some()
        && protocol == Protocol::OpenaiChatCompletions)
        .then_some(true);
    let supports_developer_role = match protocol {
        Protocol::OpenaiChatCompletions | Protocol::OpenaiResponses => {
            (!model_supports_developer_role(cfg, protocol, id)).then_some(false)
        }
        _ => None,
    };
    let force_adaptive_thinking = (reasoning && protocol == Protocol::Anthropic).then_some(true);
    let compat = if supports_reasoning_effort.is_some()
        || supports_developer_role.is_some()
        || force_adaptive_thinking.is_some()
    {
        Some(PiCompat {
            supports_reasoning_effort,
            supports_developer_role,
            force_adaptive_thinking,
        })
    } else {
        None
    };
    PiModel {
        id: id.to_string(),
        name: id.to_string(),
        context_window: entry.bounds.context_window,
        max_tokens: entry.bounds.max_output_tokens,
        input: if entry
            .bounds
            .features
            .iter()
            .any(|feature| feature == "image_input")
        {
            vec!["text".to_string(), "image".to_string()]
        } else {
            Vec::new()
        },
        reasoning,
        thinking_level_map,
        compat,
    }
}

fn model_supports_reasoning(model: &ModelConfig, levels: Option<&[ReasoningLevelEntry]>) -> bool {
    if let Some(table) = levels {
        return table
            .iter()
            .any(|entry| !is_disabled_reasoning_level(entry.level()));
    }
    // Capability/strategy evidence for launch metadata (§5.1).
    model.supports_thinking == Some(true)
        || model.force_think == Some(true)
        || model.default_think == Some(true)
        || model
            .default_reasoning_level
            .as_deref()
            .is_some_and(|level| !is_disabled_reasoning_level(level))
}

fn is_disabled_reasoning_level(level: &str) -> bool {
    matches!(level, "none" | "off")
}

fn pi_thinking_level_map(
    protocol: Protocol,
    model: &ModelConfig,
    levels: Option<&[ReasoningLevelEntry]>,
) -> PiThinkingLevelMap {
    let supports = |level: &str| {
        if let Some(table) = levels {
            // Explicit table: only declared levels are supported.
            table
                .iter()
                .any(|entry| entry.level() == level)
                .then(|| level.to_string())
        } else if model.force_think == Some(true) {
            // force_think without levels table = no level control (e.g. Claude).
            None
        } else {
            // Flexible model: accepts any standard level.
            Some(level.to_string())
        }
    };
    // Close intent is not a table entry (ADR-034 §1: `none`/`disabled` do not go through
    // the activity-level gate), so Pi `off` is derived from policy: it is available
    // unless the deployment forces thinking on.
    // a routing model has no `none` route — advertising `off = "none"`
    // would make Pi send `reasoning_effort = "none"` which the router cannot
    // resolve. Suppress `off` for routing models so the client picks a real level.
    let off = if model.is_routing_model() {
        None
    } else {
        (model.force_think != Some(true)).then(|| "none".to_string())
    };
    PiThinkingLevelMap {
        off,
        minimal: supports("minimal"),
        low: supports("low"),
        medium: supports("medium"),
        high: supports("high"),
        xhigh: if protocol == Protocol::Anthropic {
            None
        } else {
            supports("xhigh")
        },
        max: supports("max"),
    }
}

impl PiThinkingLevelMap {
    fn has_active_level(&self) -> bool {
        [
            &self.minimal,
            &self.low,
            &self.medium,
            &self.high,
            &self.xhigh,
            &self.max,
        ]
        .into_iter()
        .any(Option::is_some)
    }
}

fn model_supports_developer_role(cfg: &Config, protocol: Protocol, model_id: &str) -> bool {
    let candidates = cfg.client_candidate_plans(protocol, model_id);
    if candidates.is_empty() {
        return true;
    }
    candidates
        .iter()
        .all(|(_, plan)| plan.compat.effective_supports_developer_role())
}

fn is_eligible_for_pi_anthropic(cfg: &Config, id: &str, model: &ModelConfig) -> bool {
    // Direct model id contains "claude"
    if id.to_ascii_lowercase().contains("claude") {
        return true;
    }
    // Candidate upstream model or provider is Claude / Anthropic
    let candidates = cfg.client_candidate_plans(Protocol::Anthropic, id);
    let has_claude_candidate = candidates.iter().any(|(_, plan)| {
        plan.upstream_model.to_ascii_lowercase().contains("claude")
            || plan.provider_id.to_ascii_lowercase().contains("claude")
            || plan.provider_id == "anthropic"
            || crate::config::anthropic_family_glob_set(&plan.anthropic_family_models)
                .is_some_and(|set| set.is_match(&plan.upstream_model))
    });
    if has_claude_candidate {
        return true;
    }
    // Catalog models with force_think are Claude bindings (e.g. adapter-backed Claude catalog entries).
    // Require at least one valid Anthropic candidate plan to prevent misconfigured models from
    // leaking into Pi's Anthropic provider.
    if model.force_think == Some(true) && !candidates.is_empty() {
        return true;
    }
    false
}

fn anthropic_reasoning_path_is_safe(cfg: &Config, model_id: &str) -> bool {
    let candidates = cfg.client_candidate_plans(Protocol::Anthropic, model_id);
    !candidates.is_empty()
        && candidates
            .iter()
            .all(|(_, plan)| matches!(plan.adapter(), AdapterKind::Passthrough))
}

fn pi_anthropic_reasoning_hint(cfg: &Config, model_id: &str) -> Option<String> {
    let model = cfg.models.get(model_id)?;
    let levels = cfg.client_reasoning_levels(model_id);
    let levels = levels.as_deref();
    if !model_supports_reasoning(model, levels) {
        return None;
    }
    let openai_has_active =
        pi_thinking_level_map(Protocol::OpenaiResponses, model, levels).has_active_level();
    let anthropic_has_active =
        pi_thinking_level_map(Protocol::Anthropic, model, levels).has_active_level();
    // force_think without levels exports reasoning=true with no thinkingLevelMap
    // on both protocols (pi_model decoupling) — these diagnostics must mirror
    // pi_model() or they contradict the generated config.
    let reasoning_without_levels = model.force_think == Some(true);
    if !openai_has_active && !anthropic_has_active && !reasoning_without_levels {
        return Some(format!(
            "model {model_id:?} Pi reasoning metadata is disabled because its declared reasoning levels contain no Pi reasoning level that this proxy can express"
        ));
    }
    if cfg.model_serves_protocol(model_id, Protocol::Anthropic)
        && !anthropic_has_active
        && !reasoning_without_levels
    {
        return Some(format!(
            "model {model_id:?} Pi Anthropic reasoning metadata is disabled because none of its active levels is expressible in Pi's Anthropic budget mode"
        ));
    }
    (cfg.model_serves_protocol(model_id, Protocol::Anthropic)
        && !anthropic_reasoning_path_is_safe(cfg, model_id))
    .then(|| {
        format!("model {model_id:?} has Anthropic fallback but Pi reasoning metadata is disabled")
    })
}

fn is_qwen_managed_model(item: &Value, base_url: &str) -> bool {
    let Some(obj) = item.as_object() else {
        return false;
    };
    obj.get("envKey").and_then(Value::as_str) == Some(QWEN_MANAGED_ENV_KEY)
        || obj.get("baseUrl").and_then(Value::as_str) == Some(base_url)
}

fn is_zero(value: &i64) -> bool {
    *value == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        self, EndpointConfig, ProviderBinding, ProviderConfig, ReasoningLevelEntry,
    };
    use serial_test::serial;
    use std::fs;

    /// Build a `reasoning_levels` table from level names (no api_value mapping).
    fn reasoning_table(levels: &[&str]) -> Option<Vec<ReasoningLevelEntry>> {
        if levels.is_empty() {
            None
        } else {
            Some(
                levels
                    .iter()
                    .map(|level| ReasoningLevelEntry::new((*level).to_string(), None))
                    .collect(),
            )
        }
    }

    fn launch_pi_config(cfg: &Config) -> Value {
        let temp = tempfile::tempdir().expect("tempdir");
        launch_pi(cfg, Some(temp.path().to_path_buf()), false).expect("launch pi");
        let text = fs::read_to_string(temp.path().join("models.json")).expect("read pi");
        serde_json::from_str(&text).expect("parse pi config")
    }

    fn pi_provider_model<'a>(value: &'a Value, provider: &str, model_id: &str) -> &'a Value {
        value["providers"][provider]["models"]
            .as_array()
            .expect("provider models")
            .iter()
            .find(|model| model["id"] == model_id)
            .expect("pi model")
    }

    fn add_claude_model(cfg: &mut Config) -> &'static str {
        let model_id = "claude-sonnet-4-6-lp";
        let mut model = cfg.models["deepseek-v4-flash-lp"].clone();
        model.anthropic_providers = vec![ProviderBinding {
            name: "deepseek".to_string(),
            model: "claude-sonnet-4-6".to_string(),
        }];
        model.openai_chat_providers = vec![ProviderBinding {
            name: "deepseek".to_string(),
            model: "claude-sonnet-4-6".to_string(),
        }];
        model.openai_responses_providers = vec![ProviderBinding {
            name: "deepseek".to_string(),
            model: "claude-sonnet-4-6".to_string(),
        }];
        cfg.models.insert(model_id.to_string(), model);
        model_id
    }

    #[test]
    fn launch_pi_writes_all_supported_protocol_providers() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut cfg = config::default_deepseek_config();
        add_claude_model(&mut cfg);

        launch_pi(&cfg, Some(temp.path().to_path_buf()), false).expect("launch pi");

        let text = fs::read_to_string(temp.path().join("models.json")).expect("read pi");
        let value: Value = serde_json::from_str(&text).expect("parse pi config");
        let providers = value["providers"].as_object().expect("providers");
        assert_eq!(
            providers["llm-proxy-openai-chat"]["api"],
            "openai-completions"
        );
        assert_eq!(
            providers["llm-proxy-openai-responses"]["api"],
            "openai-responses"
        );
        assert_eq!(
            providers["llm-proxy-anthropic"]["api"],
            "anthropic-messages"
        );
        assert_eq!(
            providers["llm-proxy-openai-responses"]["baseUrl"],
            "http://127.0.0.1:8989/openai/v1"
        );
        assert_eq!(
            providers["llm-proxy-anthropic"]["baseUrl"],
            "http://127.0.0.1:8989/anthropic"
        );
        let anthropic_base_url = providers["llm-proxy-anthropic"]["baseUrl"]
            .as_str()
            .expect("anthropic baseUrl");
        assert_eq!(
            format!("{anthropic_base_url}/v1/messages"),
            "http://127.0.0.1:8989/anthropic/v1/messages"
        );
        assert!(
            providers["llm-proxy-openai-responses"]["models"]
                .as_array()
                .unwrap()
                .iter()
                .any(|model| model["id"] == "deepseek-v4-pro-lp")
        );
        assert!(
            providers["llm-proxy-anthropic"]["models"]
                .as_array()
                .unwrap()
                .iter()
                .any(|model| model["id"] == "claude-sonnet-4-6-lp")
        );
        assert!(
            !providers["llm-proxy-anthropic"]["models"]
                .as_array()
                .unwrap()
                .iter()
                .any(|model| model["id"] == "deepseek-v4-flash-lp")
        );
    }

    // follow-up: launch_pi should deep-merge managed providers and models
    #[test]
    fn launch_pi_merges_managed_provider_preserving_client_fields() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("models.json");
        // A managed provider (llm-proxy-openai-chat) with client-only fields
        // at provider level (defaultModel).
        fs::write(
            &path,
            r#"{
  "providers": {
    "llm-proxy-openai-chat": {
      "baseUrl": "http://stale.invalid/v1",
      "api": "openai-completions",
      "apiKey": "local",
      "defaultModel": "deepseek-v4-pro-lp",
      "models": []
    },
    "custom-provider": {
      "baseUrl": "http://custom.example.com",
      "api": "custom",
      "apiKey": "custom-key",
      "models": []
    }
  }
}
"#,
        )
        .expect("write existing pi config");
        let cfg = config::default_deepseek_config();

        launch_pi(&cfg, Some(temp.path().to_path_buf()), false).expect("launch pi");

        let value: Value =
            serde_json::from_str(&fs::read_to_string(path).expect("read pi")).expect("parse");
        let managed = &value["providers"]["llm-proxy-openai-chat"];
        // proxy-owned fields are overwritten
        assert_eq!(managed["baseUrl"], json!("http://127.0.0.1:8989/openai/v1"));
        // client-only fields survive the merge
        assert_eq!(managed["defaultModel"], json!("deepseek-v4-pro-lp"));
        // non-managed provider is kept verbatim
        assert_eq!(
            value["providers"]["custom-provider"]["baseUrl"],
            json!("http://custom.example.com")
        );
    }

    #[test]
    fn launch_pi_omits_protocols_without_model_bindings() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut cfg = config::default_deepseek_config();
        add_claude_model(&mut cfg);
        for model in cfg.models.values_mut() {
            model.openai_chat_providers.clear();
        }

        launch_pi(&cfg, Some(temp.path().to_path_buf()), false).expect("launch pi");

        let text = fs::read_to_string(temp.path().join("models.json")).expect("read pi");
        let value: Value = serde_json::from_str(&text).expect("parse pi config");
        let providers = value["providers"].as_object().expect("providers");
        assert!(!providers.contains_key("llm-proxy-openai-chat"));
        assert!(providers.contains_key("llm-proxy-openai-responses"));
        assert!(providers.contains_key("llm-proxy-anthropic"));
    }

    /// : Claude models with force_think + image_input produce correct
    /// pi config: reasoning=true, no thinkingLevelMap, input includes image.
    /// : openai-chat projects supportsDeveloperRole: false (from deepseek provider);
    /// anthropic projects forceAdaptiveThinking: true.
    #[test]
    fn launch_pi_claude_force_think_produces_reasoning_without_levels() {
        let mut cfg = config::default_deepseek_config();
        let model = cfg.models.get_mut("deepseek-v4-flash-lp").unwrap();
        // Simulate Claude-like config: force_think + image_input, no reasoning levels.
        model.force_think = Some(true);
        model.features = vec!["image_input".to_string()];
        model.reasoning_levels = None;
        model.default_reasoning_level = None;
        model.supports_thinking = None;

        let value = launch_pi_config(&cfg);

        // OpenAI Chat: reasoning=true, no thinkingLevelMap, supportsDeveloperRole=false, input has image.
        let chat = pi_provider_model(&value, "llm-proxy-openai-chat", "deepseek-v4-flash-lp");
        assert_eq!(chat["reasoning"], true, "force_think → reasoning=true");
        assert!(
            chat.get("thinkingLevelMap").is_none(),
            "no levels → no thinkingLevelMap"
        );
        assert_eq!(chat["compat"]["supportsDeveloperRole"], false);
        assert!(chat["compat"].get("supportsReasoningEffort").is_none());
        assert_eq!(chat["input"], json!(["text", "image"]));

        // Anthropic: reasoning=true, no thinkingLevelMap, forceAdaptiveThinking=true.
        let anthropic = pi_provider_model(&value, "llm-proxy-anthropic", "deepseek-v4-flash-lp");
        assert_eq!(anthropic["reasoning"], true);
        assert!(anthropic.get("thinkingLevelMap").is_none());
        assert_eq!(anthropic["compat"]["forceAdaptiveThinking"], true);

        // The hint must mirror pi_model(): reasoning is exported, so no
        // "reasoning metadata is disabled" diagnostic may fire (regression for
        // the decoupled force_think-without-levels path).
        assert!(
            pi_anthropic_reasoning_hint(&cfg, "deepseek-v4-flash-lp").is_none(),
            "force_think without levels must not produce a disabled-reasoning warning"
        );
    }

    #[test]
    fn launch_pi_derives_reasoning_capability_from_explicit_model_metadata() {
        let cases = [
            (
                "declared active level survives default-off policy",
                vec!["none", "low"],
                None,
                Some(false),
                true,
            ),
            (
                "only a closed level is not capability evidence",
                vec!["none"],
                None,
                Some(true),
                false,
            ),
            (
                "supports_thinking true is evidence when levels are absent",
                vec![],
                None,
                Some(true),
                true,
            ),
            (
                "active default is evidence when levels are absent",
                vec![],
                Some("medium"),
                Some(false),
                true,
            ),
            (
                "fully absent metadata is not capability evidence",
                vec![],
                None,
                None,
                false,
            ),
        ];

        for (name, levels, default_level, thinking_evidence, expected) in cases {
            let mut cfg = config::default_deepseek_config();
            add_claude_model(&mut cfg);
            let model = cfg.models.get_mut("claude-sonnet-4-6-lp").unwrap();
            model.reasoning_levels = reasoning_table(&levels);
            model.default_reasoning_level = default_level.map(ToOwned::to_owned);
            model.supports_thinking = None;
            model.force_think = None;
            model.force_no_think = None;
            model.default_think = None;
            // Retired enable_thinking semantics: Some(true) was capability
            // evidence (→ supports_thinking); Some(false) was a default-off
            // policy (→ default_think).
            match thinking_evidence {
                Some(true) => model.supports_thinking = Some(true),
                Some(false) => model.default_think = Some(false),
                None => {}
            }

            let value = launch_pi_config(&cfg);
            for provider in [
                "llm-proxy-openai-chat",
                "llm-proxy-openai-responses",
                "llm-proxy-anthropic",
            ] {
                assert_eq!(
                    pi_provider_model(&value, provider, "claude-sonnet-4-6-lp")["reasoning"],
                    expected,
                    "{name}: {provider}"
                );
            }
        }
    }

    #[test]
    fn launch_pi_projects_protocol_specific_reasoning_maps_without_upstream_values() {
        let mut cfg = config::default_deepseek_config();
        add_claude_model(&mut cfg);
        let model = cfg.models.get_mut("deepseek-v4-flash-lp").unwrap();
        model.reasoning_levels = Some(vec![
            ReasoningLevelEntry::new("none", None),
            ReasoningLevelEntry::new("low", Some("provider-low".to_string())),
            ReasoningLevelEntry::new("high", Some("provider-max".to_string())),
        ]);
        model.default_reasoning_level = Some("high".to_string());

        let claude = cfg.models.get_mut("claude-sonnet-4-6-lp").unwrap();
        claude.reasoning_levels = Some(vec![
            ReasoningLevelEntry::new("none", None),
            ReasoningLevelEntry::new("low", Some("provider-low".to_string())),
            ReasoningLevelEntry::new("high", Some("provider-max".to_string())),
        ]);
        claude.default_reasoning_level = Some("high".to_string());

        let value = launch_pi_config(&cfg);
        let expected_openai = json!({
            "off": "none",
            "minimal": null,
            "low": "low",
            "medium": null,
            "high": "high",
            "xhigh": null,
            "max": null
        });
        let chat = pi_provider_model(&value, "llm-proxy-openai-chat", "deepseek-v4-flash-lp");
        let responses =
            pi_provider_model(&value, "llm-proxy-openai-responses", "deepseek-v4-flash-lp");
        assert_eq!(chat["thinkingLevelMap"], expected_openai);
        assert_eq!(responses["thinkingLevelMap"], expected_openai);
        assert_eq!(chat["compat"]["supportsReasoningEffort"], true);
        assert_eq!(chat["compat"]["supportsDeveloperRole"], false);
        assert!(responses.get("compat").is_none());

        let anthropic = pi_provider_model(&value, "llm-proxy-anthropic", "claude-sonnet-4-6-lp");
        assert_eq!(
            anthropic["thinkingLevelMap"],
            json!({
                "off": "none",
                "minimal": null,
                "low": "low",
                "medium": null,
                "high": "high",
                "xhigh": null,
                "max": null
            })
        );
        assert_eq!(anthropic["compat"]["forceAdaptiveThinking"], true);
        assert!(!value.to_string().contains("provider-low"));
        assert!(!value.to_string().contains("provider-max"));
    }

    #[test]
    fn launch_pi_uses_real_pi_levels_when_capability_has_no_declared_gate() {
        let mut cfg = config::default_deepseek_config();
        add_claude_model(&mut cfg);
        let model = cfg.models.get_mut("deepseek-v4-flash-lp").unwrap();
        model.reasoning_levels = None;
        model.default_reasoning_level = None;
        model.supports_thinking = Some(true);

        let claude = cfg.models.get_mut("claude-sonnet-4-6-lp").unwrap();
        claude.reasoning_levels = None;
        claude.default_reasoning_level = None;
        claude.supports_thinking = Some(true);

        let value = launch_pi_config(&cfg);
        let openai =
            pi_provider_model(&value, "llm-proxy-openai-responses", "deepseek-v4-flash-lp");
        assert_eq!(
            openai["thinkingLevelMap"],
            json!({
                "off": "none",
                "minimal": "minimal",
                "low": "low",
                "medium": "medium",
                "high": "high",
                "xhigh": "xhigh",
                "max": "max"
            })
        );
        let anthropic = pi_provider_model(&value, "llm-proxy-anthropic", "claude-sonnet-4-6-lp");
        assert_eq!(
            anthropic["thinkingLevelMap"],
            json!({
                "off": "none",
                "minimal": "minimal",
                "low": "low",
                "medium": "medium",
                "high": "high",
                "xhigh": null,
                "max": "max"
            })
        );
        assert_eq!(anthropic["compat"]["forceAdaptiveThinking"], true);
    }

    #[test]
    fn launch_pi_requires_a_protocol_expressible_active_reasoning_level() {
        let cases = [
            (
                "unknown-only",
                vec!["custom"],
                [false, false, false],
                "no Pi reasoning level",
            ),
            (
                "xhigh-only",
                vec!["xhigh"],
                [true, true, false],
                "Anthropic",
            ),
            (
                "closed-only",
                vec!["none", "off"],
                [false, false, false],
                "",
            ),
        ];

        for (name, levels, expected, hint_fragment) in cases {
            let mut cfg = config::default_deepseek_config();
            add_claude_model(&mut cfg);
            let model = cfg.models.get_mut("claude-sonnet-4-6-lp").unwrap();
            model.reasoning_levels = reasoning_table(&levels);
            model.default_reasoning_level = None;
            model.supports_thinking = Some(true);

            let value = launch_pi_config(&cfg);
            for (provider, expected_reasoning) in [
                ("llm-proxy-openai-chat", expected[0]),
                ("llm-proxy-openai-responses", expected[1]),
                ("llm-proxy-anthropic", expected[2]),
            ] {
                let model = pi_provider_model(&value, provider, "claude-sonnet-4-6-lp");
                assert_eq!(model["reasoning"], expected_reasoning, "{name}: {provider}");
                if expected_reasoning {
                    assert!(
                        model.get("thinkingLevelMap").is_some(),
                        "{name}: {provider}"
                    );
                } else {
                    assert!(
                        model.get("thinkingLevelMap").is_none(),
                        "{name}: {provider}"
                    );
                }
            }
            let hint = pi_anthropic_reasoning_hint(&cfg, "claude-sonnet-4-6-lp");
            if hint_fragment.is_empty() {
                assert!(hint.is_none(), "{name}");
            } else {
                assert!(hint.expect("launch hint").contains(hint_fragment), "{name}");
            }
        }
    }

    #[test]
    fn launch_pi_disables_anthropic_reasoning_for_mixed_fallback_without_dropping_model() {
        let mut cfg = config::default_deepseek_config();
        add_claude_model(&mut cfg);
        cfg.providers.insert(
            "unsafe-anthropic".to_string(),
            ProviderConfig {
                openai_chat: Some(EndpointConfig::native(
                    "https://example.test/v1/chat/completions",
                )),
                anthropic: Some(EndpointConfig::derived(Protocol::OpenaiChatCompletions)),
                ..ProviderConfig::default()
            },
        );
        cfg.models
            .get_mut("claude-sonnet-4-6-lp")
            .unwrap()
            .anthropic_providers
            .push(ProviderBinding {
                name: "unsafe-anthropic".to_string(),
                model: "upstream-model".to_string(),
            });

        let value = launch_pi_config(&cfg);
        let model = pi_provider_model(&value, "llm-proxy-anthropic", "claude-sonnet-4-6-lp");
        assert_eq!(model["reasoning"], false);
        assert_eq!(model["id"], "claude-sonnet-4-6-lp");
        assert!(
            pi_anthropic_reasoning_hint(&cfg, "claude-sonnet-4-6-lp")
                .expect("mixed fallback hint")
                .contains("Anthropic fallback")
        );
    }

    /// : launch_pi fixes all three issues:
    /// 1. projects `supportsDeveloperRole: false` for providers/models that do not support it
    /// 2. filters third-party models (Qwen, DeepSeek, etc.) out of `llm-proxy-anthropic`
    /// 3. actively prunes stale/dirty models and preserves client-only model fields (e.g. voiceOnly)
    #[test]
    fn launch_pi_fixes_all_three_issues_compat_filter_and_prune() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("models.json");

        // 1. Simulate legacy dirty configuration:
        // - old version illegally injected qwen into llm-proxy-anthropic
        // - client-only field voiceOnly: true on chat model
        // - stale model that no longer exists in proxy config
        fs::write(
            &path,
            r#"{
            "providers": {
                "llm-proxy-anthropic": {
                    "baseUrl": "http://127.0.0.1:8989/anthropic",
                    "models": [
                        {"id": "qwen3.8-flash-lp", "reasoning": true}
                    ]
                },
                "llm-proxy-openai-chat": {
                    "baseUrl": "http://stale.invalid/v1",
                    "api": "openai-completions",
                    "apiKey": "local",
                    "defaultModel": "custom-default",
                    "models": [
                        {"id": "qwen3.8-flash-lp", "voiceOnly": true, "customTag": "preserve-me"},
                        {"id": "stale-deleted-model", "someField": "dropped"}
                    ]
                }
            }
        }"#,
        )
        .unwrap();

        let mut cfg = config::default_deepseek_config();
        use crate::config::CompatConfig;
        // Add qwen3.8-flash-lp with supports_developer_role: false on provider
        cfg.providers.insert(
            "ubt-bailian".to_string(),
            ProviderConfig {
                openai_chat: Some(EndpointConfig {
                    compat: Some(CompatConfig {
                        supports_developer_role: Some(false),
                        ..CompatConfig::default()
                    }),
                    ..EndpointConfig::native("https://dashscope.aliyuncs.com/compatible-mode/v1")
                }),
                ..ProviderConfig::default()
            },
        );
        let mut qwen_model = cfg.models["deepseek-v4-flash-lp"].clone();
        qwen_model.openai_chat_providers = vec![ProviderBinding {
            name: "ubt-bailian".to_string(),
            model: "qwen3.8-flash".to_string(),
        }];
        qwen_model.openai_responses_providers.clear();
        qwen_model.anthropic_providers.clear();
        cfg.models
            .insert("qwen3.8-flash-lp".to_string(), qwen_model);

        // Add a Claude model
        let mut claude_model = cfg.models["deepseek-v4-flash-lp"].clone();
        claude_model.anthropic_providers = vec![ProviderBinding {
            name: "deepseek".to_string(),
            model: "claude-sonnet-4-6".to_string(),
        }];
        claude_model.force_think = Some(true);
        cfg.models
            .insert("claude-sonnet-4-6-lp".to_string(), claude_model);

        launch_pi(&cfg, Some(temp.path().to_path_buf()), false).unwrap();

        let result: Value = serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
        let providers = result["providers"].as_object().unwrap();

        // Verification 1: OpenAI Chat must project supportsDeveloperRole: false
        let chat_models = providers["llm-proxy-openai-chat"]["models"]
            .as_array()
            .unwrap();
        let qwen_chat = chat_models
            .iter()
            .find(|m| m["id"] == "qwen3.8-flash-lp")
            .unwrap();
        assert_eq!(
            qwen_chat["compat"]["supportsDeveloperRole"], false,
            "必须注入 supportsDeveloperRole: false"
        );
        // Verification 3 (part a): Client-only fields on models (voiceOnly, customTag) are preserved across launch_pi
        assert_eq!(qwen_chat["voiceOnly"], true);
        assert_eq!(qwen_chat["customTag"], "preserve-me");
        // Provider-level client fields preserved
        assert_eq!(
            providers["llm-proxy-openai-chat"]["defaultModel"],
            "custom-default"
        );
        // Stale model pruned
        assert!(!chat_models.iter().any(|m| m["id"] == "stale-deleted-model"));

        // Verification 2 & 3 (part b): Third-party model Qwen must NEVER appear in Anthropic provider,
        // and legacy dirty Qwen in Anthropic must be actively pruned!
        let anthropic = providers.get("llm-proxy-anthropic").unwrap();
        let anthropic_models = anthropic["models"].as_array().unwrap();
        assert!(
            !anthropic_models
                .iter()
                .any(|m| m["id"] == "qwen3.8-flash-lp"),
            "Anthropic provider 中不得残留或包含 qwen3.8-flash-lp"
        );
        let claude = anthropic_models
            .iter()
            .find(|m| m["id"] == "claude-sonnet-4-6-lp")
            .unwrap();
        assert_eq!(claude["compat"]["forceAdaptiveThinking"], true);
    }

    /// : When no eligible Claude models exist in config,
    /// `llm-proxy-anthropic` must be completely omitted and pruned from `models.json`.
    #[test]
    fn launch_pi_prunes_empty_anthropic_provider_when_no_claude_models() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("models.json");

        fs::write(
            &path,
            r#"{
            "providers": {
                "llm-proxy-anthropic": {
                    "baseUrl": "http://127.0.0.1:8989/anthropic",
                    "models": [
                        {"id": "qwen3.8-flash-lp", "reasoning": true}
                    ]
                }
            }
        }"#,
        )
        .unwrap();

        let cfg = config::default_deepseek_config();
        launch_pi(&cfg, Some(temp.path().to_path_buf()), false).unwrap();

        let result: Value = serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
        let providers = result["providers"].as_object().unwrap();
        assert!(
            !providers.contains_key("llm-proxy-anthropic"),
            "When no eligible Claude models exist, llm-proxy-anthropic must be completely omitted and pruned"
        );
    }

    #[test]
    fn launch_pi_preserves_existing_incomplete_custom_provider() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("models.json");
        fs::write(
            &path,
            r#"{
  "version": 1,
  "providers": {
    "custom": {
      "baseUrl": "https://example.test/v1",
      "api": "openai-completions",
      "models": [
        { "id": "custom-model" }
      ]
    },
    "llm-proxy-old": {
      "baseUrl": "http://old/openai/v1",
      "api": "openai-completions",
      "models": []
    }
  }
}
"#,
        )
        .expect("write existing pi config");
        let cfg = config::default_deepseek_config();

        launch_pi(&cfg, Some(temp.path().to_path_buf()), false).expect("launch pi");

        let text = fs::read_to_string(path).expect("read pi");
        assert!(text.contains("\"version\": 1"));
        assert!(text.contains("\"custom\""));
        assert!(text.contains("\"custom-model\""));
        assert!(!text.contains("\"llm-proxy-old\""));
        assert!(text.contains("\"llm-proxy-openai-chat\""));
    }

    #[test]
    fn launch_qwen_code_writes_openai_provider_without_selecting_model() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("settings.json");
        fs::write(
            &path,
            r#"{"model":{"name":"existing-model","baseUrl":"https://existing.example/v1"}}"#,
        )
        .expect("write existing qwen settings");
        let cfg = config::default_deepseek_config();

        launch_qwen_code(&cfg, Some(temp.path().to_path_buf()), false).expect("launch qwen");

        let value: Value = serde_json::from_str(&fs::read_to_string(path).expect("read qwen"))
            .expect("parse qwen settings");
        assert_eq!(value["model"]["name"], "existing-model");
        assert_eq!(value["model"]["baseUrl"], "https://existing.example/v1");
        assert_eq!(value["security"]["auth"]["selectedType"], "openai");
        assert!(
            value["modelProviders"]["openai"]
                .as_array()
                .unwrap()
                .iter()
                .any(|model| model["id"] == "deepseek-v4-pro-lp")
        );
        assert_eq!(
            value["modelProviders"]["openai"][0]["generationConfig"]["contextWindowSize"],
            1_000_000
        );
        assert!(value["modelProviders"]["openai"][0]["contextWindow"].is_null());
        // Verify supportedInputs is written from model.features
        assert_eq!(
            value["modelProviders"]["openai"][0]["supportedInputs"],
            json!(["text"])
        );
    }

    #[test]
    fn launch_qwen_code_writes_supported_inputs_from_model_features() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("settings.json");
        fs::write(&path, r#"{}"#).expect("write empty qwen settings");

        // Create a config with a model that has image_input feature
        let mut cfg = config::default_deepseek_config();
        cfg.models
            .get_mut("deepseek-v4-flash-lp")
            .unwrap()
            .features
            .push("image_input".to_string());

        launch_qwen_code(&cfg, Some(temp.path().to_path_buf()), false).expect("launch qwen");

        let value: Value = serde_json::from_str(&fs::read_to_string(path).expect("read qwen"))
            .expect("parse qwen settings");

        // Verify supportedInputs includes image when model has image_input feature
        let model = value["modelProviders"]["openai"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["id"] == "deepseek-v4-flash-lp")
            .expect("find model");
        assert_eq!(model["supportedInputs"], json!(["text", "image"]));
    }

    #[test]
    fn launch_qwen_code_preserves_existing_model_without_explicit_selection() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("settings.json");
        fs::write(
            &path,
            r#"{
  "model": {
    "name": "existing-model",
    "baseUrl": "https://existing.example/v1"
  },
  "modelProviders": {
    "openai": []
  }
}
"#,
        )
        .expect("write existing qwen settings");
        let cfg = config::default_deepseek_config();

        launch_qwen_code(&cfg, Some(temp.path().to_path_buf()), false).expect("launch qwen");

        let value: Value = serde_json::from_str(&fs::read_to_string(path).expect("read qwen"))
            .expect("parse qwen settings");
        assert_eq!(value["model"]["name"], "existing-model");
        assert_eq!(value["model"]["baseUrl"], "https://existing.example/v1");
    }

    #[test]
    fn launch_qwen_code_preserves_unmanaged_settings_and_models() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("settings.json");
        fs::write(
            &path,
            r#"{
  "theme": "dark",
  "modelProviders": {
    "openai": [
      {
        "id": "custom-openai",
        "baseUrl": "https://example.test/v1",
        "envKey": "CUSTOM_KEY"
      },
      {
        "id": "old-managed",
        "baseUrl": "http://127.0.0.1:8989/openai/v1",
        "envKey": "LLM_PROXY_API_KEY"
      }
    ]
  }
}
"#,
        )
        .expect("write existing qwen settings");
        let cfg = config::default_deepseek_config();

        launch_qwen_code(&cfg, Some(temp.path().to_path_buf()), false).expect("launch qwen");

        let text = fs::read_to_string(path).expect("read qwen");
        assert!(text.contains("\"theme\": \"dark\""));
        assert!(text.contains("\"custom-openai\""));
        assert!(!text.contains("\"old-managed\""));
        assert!(text.contains("\"deepseek-v4-pro-lp\""));
    }

    #[test]
    fn launch_qwen_code_merges_managed_model_preserving_client_fields() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("settings.json");
        // A managed entry (envKey == LLM_PROXY_API_KEY) that llm-proxy also
        // generates (deepseek-v4-pro-lp). It carries client-only fields
        // (fastOnly) and a nested client addition (generationConfig.reasoning)
        // plus stale values for proxy-owned fields (baseUrl, description,
        // contextWindowSize) that the launch must overwrite.
        fs::write(
            &path,
            r#"{
  "modelProviders": {
    "openai": [
      {
        "id": "deepseek-v4-pro-lp",
        "name": "deepseek-v4-pro-lp",
        "description": "stale custom description",
        "baseUrl": "http://stale.invalid/v1",
        "envKey": "LLM_PROXY_API_KEY",
        "fastOnly": true,
        "generationConfig": {
          "contextWindowSize": -12345,
          "reasoning": { "effort": "max" }
        }
      }
    ]
  }
}
"#,
        )
        .expect("write existing qwen settings");
        let cfg = config::default_deepseek_config();

        launch_qwen_code(&cfg, Some(temp.path().to_path_buf()), false).expect("launch qwen");

        let value: Value =
            serde_json::from_str(&fs::read_to_string(path).expect("read qwen")).expect("parse");
        let item = value["modelProviders"]["openai"]
            .as_array()
            .expect("openai array")
            .iter()
            .find(|m| m.get("id").and_then(Value::as_str) == Some("deepseek-v4-pro-lp"))
            .expect("managed model present");

        // proxy-owned fields are overwritten by the freshly generated entry
        assert_eq!(item["baseUrl"], json!("http://127.0.0.1:8989/openai/v1"));
        assert_eq!(
            item["description"],
            json!("deepseek-v4-pro-lp via llm-proxy")
        );
        assert_ne!(item["generationConfig"]["contextWindowSize"], json!(-12345));
        // client-only fields survive the merge
        assert_eq!(item["fastOnly"], json!(true));
        assert_eq!(
            item["generationConfig"]["reasoning"]["effort"],
            json!("max")
        );
    }

    #[test]
    fn launch_pi_uses_server_listen_base_url() {
        let temp = tempfile::tempdir().expect("tempdir");
        let cfg = config::default_deepseek_config();

        launch_pi(&cfg, Some(temp.path().to_path_buf()), false).expect("launch pi");

        let text = fs::read_to_string(temp.path().join("models.json")).expect("read pi");
        assert!(text.contains("\"baseUrl\": \"http://127.0.0.1:8989/openai/v1\""));
    }

    /// launch pi in Remote Token mode must use `$ENV_VAR` syntax
    /// for apiKey instead of writing literal token values to disk.
    #[test]
    fn launch_pi_remote_token_mode_uses_env_var_reference() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut cfg = config::default_deepseek_config();
        cfg.server = None;
        cfg.remote = Some(config::RemoteConfig {
            url: "https://cncsmonster.com/lp".to_string(),
            auth_mode: Some(config::RemoteAuthMode::Token),
            auth_token_env: "CNCSMONSTER_API_KEY".into(),
        });

        // SAFETY: unique env var name for test isolation
        unsafe { std::env::set_var("CNCSMONSTER_API_KEY", "test-token-123") };

        launch_pi(&cfg, Some(temp.path().to_path_buf()), false).expect("launch pi");

        let text = fs::read_to_string(temp.path().join("models.json")).expect("read pi");
        let value: Value = serde_json::from_str(&text).expect("parse pi config");

        // apiKey must use $ENV_VAR syntax, not the literal token value
        let api_key = value["providers"]["llm-proxy-openai-chat"]["apiKey"]
            .as_str()
            .expect("apiKey must be a string");
        assert_eq!(
            api_key, "$CNCSMONSTER_API_KEY",
            "apiKey must reference env var, not contain literal token"
        );

        // Verify the literal token is NOT written to disk
        assert!(
            !text.contains("test-token-123"),
            "literal token must not appear in config file"
        );

        unsafe { std::env::remove_var("CNCSMONSTER_API_KEY") };
    }

    #[test]
    fn launch_claude_desktop_writes_profile_and_provider() {
        let temp = tempfile::tempdir().expect("tempdir");
        let cfg = config::default_deepseek_config();

        launch_claude_desktop(
            &cfg,
            Some(temp.path().to_path_buf()),
            "coding".to_string(),
            false,
        )
        .expect("launch claude desktop");

        let text = fs::read_to_string(temp.path().join("claude_desktop_config.json"))
            .expect("read desktop config");
        assert!(!text.contains("\"activeProfile\""));
        assert!(!text.contains("\"defaultModel\""));
        assert!(text.contains("\"baseUrl\": \"http://127.0.0.1:8989/anthropic\""));
        assert!(text.contains("deepseek-v4-pro-lp"));
    }

    #[test]
    fn launch_claude_desktop_preserves_unmanaged_fields() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("claude_desktop_config.json");
        fs::write(
            &path,
            r#"{"theme":"dark","llmProxy":{"profiles":{"custom":{"provider":"other"}}}}"#,
        )
        .expect("write existing desktop config");
        let cfg = config::default_deepseek_config();

        launch_claude_desktop(
            &cfg,
            Some(temp.path().to_path_buf()),
            "llm-proxy".to_string(),
            false,
        )
        .expect("launch claude desktop");

        let text = fs::read_to_string(path).expect("read desktop config");
        assert!(text.contains("\"theme\": \"dark\""));
        assert!(text.contains("\"custom\""));
        assert!(text.contains("http://127.0.0.1:8989/anthropic"));
    }

    #[test]
    fn launch_claude_code_writes_anthropic_settings() {
        let temp = tempfile::tempdir().expect("tempdir");
        let cfg = config::default_deepseek_config();
        let claude_home = temp.path().join(".claude");
        fs::create_dir_all(&claude_home).expect("create claude home");

        launch_claude_code(&cfg, Some(claude_home.clone()), false).expect("launch claude code");

        let text = fs::read_to_string(claude_home.join("settings.json")).expect("read settings");
        assert!(text.contains("\"ANTHROPIC_BASE_URL\": \"http://127.0.0.1:8989/anthropic\""));
        assert!(text.contains("\"ANTHROPIC_AUTH_TOKEN\": \"local\""));
        // : four-tier model mapping must be present
        assert!(text.contains("\"ANTHROPIC_MODEL\""));
        assert!(text.contains("\"ANTHROPIC_DEFAULT_SONNET_MODEL\""));
        assert!(text.contains("\"ANTHROPIC_DEFAULT_OPUS_MODEL\""));
        assert!(text.contains("\"ANTHROPIC_DEFAULT_HAIKU_MODEL\""));
    }

    /// : all four Claude Code model tiers resolve to the first
    /// available anthropic-protocol model, preventing 404s when Claude Code's
    /// internal sub-Agent or Compact looks up a tier-specific model name.
    #[test]
    fn launch_claude_code_maps_all_tiers_to_primary_model() {
        let temp = tempfile::tempdir().expect("tempdir");
        let cfg = config::default_deepseek_config();
        let claude_home = temp.path().join(".claude");
        fs::create_dir_all(&claude_home).expect("create claude home");

        launch_claude_code(&cfg, Some(claude_home.clone()), false).expect("launch claude code");

        let settings_path = claude_home.join("settings.json");
        let content: Value =
            serde_json::from_str(&fs::read_to_string(&settings_path).expect("read settings"))
                .expect("parse settings");

        let env = content["env"].as_object().expect("env object");
        let primary = env["ANTHROPIC_MODEL"].as_str().expect("ANTHROPIC_MODEL");
        assert_eq!(
            env["ANTHROPIC_DEFAULT_SONNET_MODEL"].as_str(),
            Some(primary),
            "Sonnet tier must map to the primary model"
        );
        assert_eq!(
            env["ANTHROPIC_DEFAULT_OPUS_MODEL"].as_str(),
            Some(primary),
            "Opus tier must map to the primary model"
        );
        assert_eq!(
            env["ANTHROPIC_DEFAULT_HAIKU_MODEL"].as_str(),
            Some(primary),
            "Haiku tier must map to the primary model"
        );
        // The primary model must be one of the configured anthropic models
        assert!(
            primary.starts_with("deepseek-v4"),
            "primary model should be from the available anthropic-protocol models, got: {primary}"
        );
    }

    /// : onboarding bypass — `~/.claude.json` must contain
    /// `hasCompletedOnboarding: true` after launch, skipping the OAuth popup.
    #[test]
    fn launch_claude_code_injects_onboarding_bypass() {
        let temp = tempfile::tempdir().expect("tempdir");
        let cfg = config::default_deepseek_config();

        // : --claude-config-dir X ⟺ 客户端带 CLAUDE_CONFIG_DIR=X 启动，
        // 官方语义下 onboarding 文件在 X **内部**（不再逃到 X 的父目录）。
        let claude_home = temp.path().join("isolated").join(".claude");
        fs::create_dir_all(&claude_home).expect("create claude home");

        // Pre-create .claude.json with other fields to verify merge behaviour
        let onboarding_path = claude_home.join(".claude.json");
        fs::write(&onboarding_path, r#"{"existingField": 42}"#).expect("write initial onboarding");

        launch_claude_code(&cfg, Some(claude_home.clone()), false).expect("launch claude code");

        let content: Value =
            serde_json::from_str(&fs::read_to_string(&onboarding_path).expect("read onboarding"))
                .expect("parse onboarding");

        assert_eq!(
            content["hasCompletedOnboarding"],
            json!(true),
            "hasCompletedOnboarding must be true to skip OAuth popup"
        );
        assert_eq!(
            content["existingField"],
            json!(42),
            "existing fields must be preserved (idempotent merge)"
        );
    }

    /// : onboarding bypass creates the file if it doesn't exist.
    #[test]
    fn launch_claude_code_creates_onboarding_file_from_scratch() {
        let temp = tempfile::tempdir().expect("tempdir");
        let cfg = config::default_deepseek_config();

        let claude_home = temp.path().join("isolated").join(".claude");
        fs::create_dir_all(&claude_home).expect("create claude home");

        launch_claude_code(&cfg, Some(claude_home.clone()), false).expect("launch claude code");

        // : onboarding 文件在 override 目录**内部**
        let onboarding_path = claude_home.join(".claude.json");
        assert!(onboarding_path.exists(), ".claude.json must be created");

        let content: Value =
            serde_json::from_str(&fs::read_to_string(&onboarding_path).expect("read onboarding"))
                .expect("parse onboarding");
        assert_eq!(content["hasCompletedOnboarding"], json!(true));
        // : 逃逸防护——override 目录的**直接父级**不得出现 .claude.json。
        // 2026-10-13 复核修正：原断言误查祖父级 temp/ 根，旧实现写的是
        // temp/isolated/.claude.json，原断言恒绿、形同虚设。
        let direct_parent = claude_home.parent().expect("claude_home has parent");
        assert!(
            !direct_parent.join(".claude.json").exists(),
            "旧实现会把 .claude.json 写到 override 目录的直接父级（逃逸），必须为零残留"
        );
    }

    // ---- routing models in launch output ----

    /// Two targets with identical client-visible capabilities (strict target
    /// consistency) — only the upstream model ids differ — plus a
    /// routing model over them.
    fn routing_launch_config() -> Config {
        let toml_str = r#"
[server]
listen = "127.0.0.1:0"

[providers.p]
openai_chat = { url = "https://example.test/chat/completions" }
openai_responses = { url = "https://example.test/responses" }

[models."target-high"]
context_window = 200_000
max_output_tokens = 64_000
features = ["image_input"]
openai_chat_providers = [{ name = "p", model = "upstream-high" }]
openai_responses_providers = [{ name = "p", model = "upstream-high" }]

[models."target-low"]
context_window = 200_000
max_output_tokens = 64_000
features = ["image_input"]
openai_chat_providers = [{ name = "p", model = "upstream-low" }]
openai_responses_providers = [{ name = "p", model = "upstream-low" }]

[models."router-lp"]
reasoning_routes = [
    { level = "low", target = "target-low" },
    { level = "high", target = "target-high" },
]
"#;
        let cfg: Config = toml::from_str(toml_str).expect("parse");
        cfg.validate().expect("validate");
        cfg
    }

    #[test]
    fn launch_pi_reports_a_routing_models_derived_metadata() {
        let value = launch_pi_config(&routing_launch_config());
        let router = pi_provider_model(&value, "llm-proxy-openai-responses", "router-lp");

        assert_eq!(
            router["contextWindow"], 200_000,
            "every target declares 200_000 (strict target consistency), and the hop reports it"
        );
        assert_eq!(router["maxTokens"], 64_000, "same bound as the targets");
        assert_eq!(
            router["input"],
            json!(["text", "image"]),
            "image_input survives because every target declares it"
        );
        assert_eq!(router["reasoning"], true);
        assert_eq!(router["thinkingLevelMap"]["low"], "low");
        assert_eq!(router["thinkingLevelMap"]["high"], "high");
        assert!(
            router["thinkingLevelMap"]["medium"].is_null(),
            "a level with no route must stay unadvertised"
        );
        assert!(
            router["thinkingLevelMap"]["off"].is_null(),
            "a routing hop has no `none` route: advertising `off` would make Pi send \
             reasoning_effort=none, which the hop cannot resolve"
        );
    }

    #[test]
    fn launch_qwen_code_reports_a_routing_models_derived_bounds() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("settings.json");
        fs::write(&path, r#"{}"#).expect("write empty qwen settings");

        launch_qwen_code(
            &routing_launch_config(),
            Some(temp.path().to_path_buf()),
            false,
        )
        .expect("launch qwen");

        let value: Value =
            serde_json::from_str(&fs::read_to_string(&path).expect("read qwen")).expect("parse");
        let router = value["modelProviders"]["openai"]
            .as_array()
            .expect("models")
            .iter()
            .find(|model| model["id"] == "router-lp")
            .expect("a routing model must be offered to qwen code");
        assert_eq!(router["generationConfig"]["contextWindowSize"], 200_000);
        assert_eq!(router["generationConfig"]["maxOutputTokens"], 64_000);
        assert_eq!(router["supportedInputs"], json!(["text", "image"]));
    }

    // ── : qwen capabilities.reasoning ──────────────────────

    /// Helper: launch qwen-code and return the parsed settings.json.
    fn launch_qwen_settings(cfg: &Config) -> Value {
        let temp = tempfile::tempdir().expect("tempdir");
        launch_qwen_code(cfg, Some(temp.path().to_path_buf()), false).expect("launch qwen");
        let text = fs::read_to_string(temp.path().join("settings.json")).expect("read qwen");
        serde_json::from_str(&text).expect("parse qwen settings")
    }

    /// Find a model entry in the qwen openai modelProviders array.
    fn qwen_model<'a>(value: &'a Value, model_id: &str) -> &'a Value {
        value["modelProviders"]["openai"]
            .as_array()
            .expect("openai models")
            .iter()
            .find(|m| m["id"] == model_id)
            .expect("model not found")
    }

    #[test]
    fn qwen_regular_model_emits_capabilities_reasoning() {
        let cfg = config::default_deepseek_config();
        let value = launch_qwen_settings(&cfg);
        let model = qwen_model(&value, "deepseek-v4-pro-lp");

        let reasoning = &model["capabilities"]["reasoning"];
        assert!(
            !reasoning.is_null(),
            "a model with reasoning_levels must emit capabilities.reasoning"
        );
        assert_eq!(reasoning["profile"], "openai-effort");
        // default_deepseek_config declares minimal/low/medium/high/xhigh, but
        // qwen's schema only accepts low/medium/high/xhigh/max — `minimal`
        // must be dropped rather than advertised (qwen rejects it as invalid).
        let efforts = reasoning["efforts"].as_array().expect("efforts array");
        let effort_strs: Vec<&str> = efforts
            .iter()
            .map(|v| v.as_str().expect("effort is string"))
            .collect();
        assert_eq!(effort_strs, vec!["low", "medium", "high", "xhigh"]);
        assert_eq!(reasoning["defaultEffort"], "high");
    }

    #[test]
    fn qwen_routing_model_suppresses_off_and_none() {
        let cfg = routing_launch_config();
        let value = launch_qwen_settings(&cfg);
        let router = qwen_model(&value, "router-lp");

        let reasoning = &router["capabilities"]["reasoning"];
        assert!(
            !reasoning.is_null(),
            "a routing model with routes must emit capabilities.reasoning"
        );
        assert_eq!(reasoning["profile"], "openai-effort");
        let efforts = reasoning["efforts"].as_array().expect("efforts array");
        let effort_strs: Vec<&str> = efforts
            .iter()
            .map(|v| v.as_str().expect("effort is string"))
            .collect();
        // routing_launch_config defines routes: low, high
        assert_eq!(effort_strs, vec!["low", "high"]);
        assert!(
            !effort_strs.contains(&"off") && !effort_strs.contains(&"none"),
            "routing model must not advertise off/none; got: {effort_strs:?}"
        );
        // defaultEffort should be the first route level
        assert_eq!(reasoning["defaultEffort"], "low");
    }

    #[test]
    fn qwen_model_without_reasoning_omits_capabilities() {
        let mut cfg = config::default_deepseek_config();
        // Strip reasoning metadata from both models.
        for model in cfg.models.values_mut() {
            model.reasoning_levels = None;
            model.default_reasoning_level = None;
            model.supports_thinking = None;
            model.force_think = None;
            model.default_think = None;
        }
        let value = launch_qwen_settings(&cfg);
        for model in value["modelProviders"]["openai"]
            .as_array()
            .expect("openai models")
        {
            assert!(
                model["capabilities"]["reasoning"].is_null(),
                "model {} has no reasoning metadata; capabilities.reasoning must be absent",
                model["id"].as_str().unwrap_or("?")
            );
        }
    }

    #[test]
    fn qwen_all_tiers_outside_wordlist_suppresses_reasoning() {
        let mut cfg = config::default_deepseek_config();
        // Set reasoning levels to only values outside QWEN_REASONING_EFFORT_TIERS
        // ("minimal" and "ultra" are not in the wordlist ["low","medium","high","xhigh","max"]).
        for model in cfg.models.values_mut() {
            model.reasoning_levels = reasoning_table(&["minimal", "ultra"]);
            model.default_reasoning_level = Some("minimal".to_string());
        }
        let value = launch_qwen_settings(&cfg);
        for model in value["modelProviders"]["openai"]
            .as_array()
            .expect("openai models")
        {
            assert!(
                model["capabilities"]["reasoning"].is_null(),
                "model {} has only out-of-wordlist tiers; capabilities.reasoning must be absent",
                model["id"].as_str().unwrap_or("?")
            );
        }
    }

    #[test]
    #[serial]
    fn qwen_code_explicit_envkey_with_token() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let qwen_home = temp_dir.path().to_path_buf();

        // SAFETY: unique env var name
        unsafe { std::env::set_var("LP_TEST_QWEN_TOKEN", "lp-qwen-token-172") };

        let mut cfg = crate::config::default_deepseek_config();
        cfg.server = None;
        cfg.remote = Some(config::RemoteConfig {
            url: "https://example.com".to_string(),
            auth_mode: Some(config::RemoteAuthMode::Token),
            auth_token_env: "LP_TEST_QWEN_TOKEN".into(),
        });

        // 预置上一轮留下的字面 token，launch 必须清除它。
        let settings_path = qwen_home.join("settings.json");
        fs::write(
            &settings_path,
            r#"{"env":{"LP_TEST_QWEN_TOKEN":"stale-token"}}"#,
        )
        .expect("seed settings");

        launch_qwen_code(&cfg, Some(qwen_home.clone()), false).expect("launch should succeed");

        assert!(settings_path.exists(), "settings.json must be created");

        let content: Value =
            serde_json::from_str(&fs::read_to_string(&settings_path).expect("read settings"))
                .expect("parse settings");

        // 验证 envKey 显式存在于每个模型中
        let providers = content["modelProviders"].as_object().expect("providers");
        let openai_models = providers
            .get("openai")
            .and_then(|v| v.as_array())
            .expect("openai models");
        assert!(!openai_models.is_empty(), "should have at least one model");

        for model in openai_models {
            assert_eq!(
                model["envKey"].as_str(),
                Some("LP_TEST_QWEN_TOKEN"),
                "envKey must match config auth_token_env"
            );
        }

        // Remote Token 模式：凭证由用户环境变量提供，不得把字面 token 写入 settings。
        assert!(
            content["env"].get("LP_TEST_QWEN_TOKEN").is_none(),
            "token mode must not write env.LP_TEST_QWEN_TOKEN"
        );
        let text = fs::read_to_string(&settings_path).expect("read settings");
        assert!(
            !text.contains("lp-qwen-token-172"),
            "literal token must not appear in settings.json"
        );
        assert!(
            !text.contains("stale-token"),
            "stale literal token must be removed"
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let metadata = fs::metadata(&settings_path).expect("metadata");
            assert_eq!(
                metadata.permissions().mode() & 0o777,
                0o600,
                "settings.json must have 0600 permissions"
            );
        }

        // 变异测试：开放模式应使用占位值 "local"
        let mut cfg_open = cfg.clone();
        if let Some(ref mut remote) = cfg_open.remote {
            remote.auth_mode = Some(config::RemoteAuthMode::Open);
        }
        launch_qwen_code(&cfg_open, Some(qwen_home.clone()), false).expect("open mode launch");

        let content_open: Value =
            serde_json::from_str(&fs::read_to_string(&settings_path).expect("read settings"))
                .expect("parse settings");
        let openai_models_open = content_open["modelProviders"]
            .get("openai")
            .and_then(|v| v.as_array())
            .expect("openai models");
        assert!(
            !openai_models_open.is_empty(),
            "should have at least one model"
        );

        // 验证开放模式下 env 值为占位值 "local"
        let env_value_open = content_open["env"]["LP_TEST_QWEN_TOKEN"].as_str();
        assert_eq!(
            env_value_open,
            Some("local"),
            "open mode must use placeholder value"
        );

        unsafe { std::env::remove_var("LP_TEST_QWEN_TOKEN") };
    }

    #[test]
    #[serial]
    fn claude_code_remote_token_mode() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let claude_home = temp_dir.path().join(".claude");
        fs::create_dir_all(&claude_home).expect("create claude home");

        // SAFETY: unique env var name
        unsafe { std::env::set_var("LP_TEST_CLAUDE_TOKEN", "lp-claude-token-172") };

        let mut cfg = crate::config::default_deepseek_config();
        cfg.server = None;
        cfg.remote = Some(config::RemoteConfig {
            url: "https://remote.example.com".to_string(),
            auth_mode: Some(config::RemoteAuthMode::Token),
            auth_token_env: "LP_TEST_CLAUDE_TOKEN".into(),
        });

        launch_claude_code(&cfg, Some(claude_home.clone()), false).expect("launch should succeed");

        let settings_path = claude_home.join("settings.json");
        assert!(settings_path.exists(), "settings.json must be created");

        let content: Value =
            serde_json::from_str(&fs::read_to_string(&settings_path).expect("read settings"))
                .expect("parse settings");

        // 验证 base URL 指向远端
        let base_url = content["env"]["ANTHROPIC_BASE_URL"].as_str();
        assert_eq!(
            base_url,
            Some("https://remote.example.com/anthropic"),
            "ANTHROPIC_BASE_URL must point to remote server"
        );

        // 验证使用顶层 apiKeyHelper 而非字面 token
        let api_key_helper = content["apiKeyHelper"].as_str();
        assert_eq!(
            api_key_helper,
            Some("echo $LP_TEST_CLAUDE_TOKEN"),
            "apiKeyHelper must be top-level and reference env var"
        );
        assert!(
            content["env"].get("ANTHROPIC_AUTH_TOKEN").is_none(),
            "token mode must not write ANTHROPIC_AUTH_TOKEN"
        );

        // 验证字面 token 未写入文件
        let text = fs::read_to_string(&settings_path).expect("read settings");
        assert!(
            !text.contains("lp-claude-token-172"),
            "literal token must not appear in settings.json"
        );

        unsafe { std::env::remove_var("LP_TEST_CLAUDE_TOKEN") };
    }

    #[test]
    fn claude_code_remote_open_mode() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let claude_home = temp_dir.path().join(".claude");
        fs::create_dir_all(&claude_home).expect("create claude home");
        // 预置上一轮 token 模式留下的 apiKeyHelper，open 模式必须清掉它。
        fs::write(
            claude_home.join("settings.json"),
            r#"{"apiKeyHelper":"echo $STALE_KEY","env":{"ANTHROPIC_AUTH_TOKEN":"stale"}}"#,
        )
        .expect("seed settings");

        let mut cfg = crate::config::default_deepseek_config();
        cfg.server = None;
        cfg.remote = Some(config::RemoteConfig {
            url: "https://remote.example.com".to_string(),
            auth_mode: Some(config::RemoteAuthMode::Open),
            auth_token_env: "LP_TEST_CLAUDE_NOEXIST".into(),
        });

        launch_claude_code(&cfg, Some(claude_home.clone()), false).expect("launch should succeed");

        let settings_path = claude_home.join("settings.json");
        let content: Value =
            serde_json::from_str(&fs::read_to_string(&settings_path).expect("read settings"))
                .expect("parse settings");

        // 验证 base URL 指向远端
        let base_url = content["env"]["ANTHROPIC_BASE_URL"].as_str();
        assert_eq!(
            base_url,
            Some("https://remote.example.com/anthropic"),
            "ANTHROPIC_BASE_URL must point to remote server in open mode"
        );

        // 验证凭证为占位值
        let auth_token = content["env"]["ANTHROPIC_AUTH_TOKEN"].as_str();
        assert_eq!(
            auth_token,
            Some("local"),
            "ANTHROPIC_AUTH_TOKEN must use placeholder in open mode"
        );
        assert!(
            content.get("apiKeyHelper").is_none(),
            "open mode must clear stale apiKeyHelper"
        );
    }

    #[test]
    #[serial]
    fn qwen_code_remote_auth_token_env() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let qwen_home = temp_dir.path().to_path_buf();

        let env_key = "LP_TEST_QWEN_LAUNCH_TEST";
        // SAFETY: unique env var name, no concurrent test reads it
        unsafe { std::env::set_var(env_key, "lp-from-env-token") };

        let mut cfg = crate::config::default_deepseek_config();
        cfg.server = None;
        cfg.remote = Some(config::RemoteConfig {
            url: "https://remote.example.com".to_string(),
            auth_mode: Some(config::RemoteAuthMode::Token),
            auth_token_env: env_key.to_string(),
        });

        launch_qwen_code(&cfg, Some(qwen_home.clone()), false).expect("launch should succeed");

        let settings_path = qwen_home.join("settings.json");
        let content: Value =
            serde_json::from_str(&fs::read_to_string(&settings_path).expect("read settings"))
                .expect("parse settings");

        // Remote Token 模式：只保留 envKey 引用，不把字面 token 写入 env。
        assert!(
            content["env"].get(env_key).is_none(),
            "token mode must not write env.{env_key}"
        );
        let text = fs::read_to_string(&settings_path).expect("read settings");
        assert!(
            !text.contains("lp-from-env-token"),
            "literal token must not appear in settings.json"
        );

        // 验证 base URL 指向远端
        let providers = content["modelProviders"].as_object().expect("providers");
        let openai_models = providers
            .get("openai")
            .and_then(|v| v.as_array())
            .expect("openai models");
        assert!(!openai_models.is_empty());
        let model_base_url = openai_models[0]["baseUrl"].as_str();
        assert_eq!(
            model_base_url,
            Some("https://remote.example.com/openai/v1"),
            "baseUrl must point to remote server"
        );

        unsafe { std::env::remove_var(env_key) };
    }

    /// : when config has models [A, B] but client settings has a stale
    /// managed model C (previously launched, since removed from config), launch
    /// must only write [A, B] — C is dropped because it is managed but no longer
    /// in config.  Unmanaged (user-added) models are always preserved.
    #[test]
    fn qwen_code_drops_stale_managed_models_not_in_config() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("settings.json");
        // Pre-existing settings with:
        //  - a user-added model (custom-key → unmanaged, must survive)
        //  - a stale managed model "removed-model" (managed by envKey, NOT in config)
        fs::write(
            &path,
            r#"{
  "modelProviders": {
    "openai": [
      {
        "id": "removed-model",
        "baseUrl": "http://127.0.0.1:8989/openai/v1",
        "envKey": "LLM_PROXY_API_KEY"
      },
      {
        "id": "user-model",
        "baseUrl": "https://user.example/v1",
        "envKey": "CUSTOM_KEY"
      }
    ]
  }
}
"#,
        )
        .expect("write existing qwen settings");
        let cfg = config::default_deepseek_config();
        // config has deepseek-v4-flash-lp and deepseek-v4-pro-lp; NOT "removed-model"

        launch_qwen_code(&cfg, Some(temp.path().to_path_buf()), false).expect("launch qwen");

        let value: Value =
            serde_json::from_str(&fs::read_to_string(path).expect("read qwen")).expect("parse");
        let models = value["modelProviders"]["openai"]
            .as_array()
            .expect("openai models");
        let ids: Vec<&str> = models.iter().filter_map(|m| m["id"].as_str()).collect();
        // Stale managed model must be gone
        assert!(
            !ids.contains(&"removed-model"),
            "stale managed model should be dropped; got: {ids:?}"
        );
        // User-added model must survive
        assert!(
            ids.contains(&"user-model"),
            "unmanaged user model must be preserved; got: {ids:?}"
        );
        // Config models must be present
        assert!(ids.contains(&"deepseek-v4-pro-lp"));
        assert!(ids.contains(&"deepseek-v4-flash-lp"));
    }

    /// : Pi launch must drop stale managed models that were removed
    /// from config, while preserving only models currently declared.
    #[test]
    fn pi_drops_stale_managed_models_not_in_config() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("models.json");
        // Pre-existing Pi config with a stale managed model "ghost-model"
        fs::write(
            &path,
            r#"{
  "providers": {
    "llm-proxy-openai-chat": {
      "baseUrl": "http://127.0.0.1:8989/openai/v1",
      "api": "openai-completions",
      "apiKey": "local",
      "models": [
        { "id": "ghost-model", "name": "ghost-model" },
        { "id": "deepseek-v4-pro-lp", "name": "deepseek-v4-pro-lp" }
      ]
    }
  }
}
"#,
        )
        .expect("write existing pi config");
        let cfg = config::default_deepseek_config();
        // config has deepseek-v4-flash-lp and deepseek-v4-pro-lp; NOT "ghost-model"

        launch_pi(&cfg, Some(temp.path().to_path_buf()), false).expect("launch pi");

        let value: Value =
            serde_json::from_str(&fs::read_to_string(&path).expect("read pi")).expect("parse");
        let providers = value["providers"].as_object().expect("providers");
        let openai_provider = providers
            .get("llm-proxy-openai-chat")
            .expect("openai-chat provider");
        let models = openai_provider["models"].as_array().expect("models");
        let ids: Vec<&str> = models.iter().filter_map(|m| m["id"].as_str()).collect();
        // Stale managed model must be gone
        assert!(
            !ids.contains(&"ghost-model"),
            "stale managed model should be dropped; got: {ids:?}"
        );
        // Config models must be present
        assert!(ids.contains(&"deepseek-v4-pro-lp"));
        assert!(ids.contains(&"deepseek-v4-flash-lp"));
    }

    /// : Claude Desktop launch overwrites the model list from config,
    /// so a stale model in the existing config must not survive a re-launch.
    #[test]
    fn claude_desktop_drops_stale_models_not_in_config() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("claude_desktop_config.json");
        // Pre-existing config with a stale model "ghost-model" not in config.toml
        fs::write(
            &path,
            r#"{
  "llmProxy": {
    "providers": {
      "llm-proxy": {
        "type": "anthropic",
        "baseUrl": "http://127.0.0.1:8989/anthropic",
        "apiKey": "local",
        "models": ["ghost-model", "deepseek-v4-pro-lp"]
      }
    },
    "profiles": {
      "llm-proxy": { "provider": "llm-proxy" }
    }
  }
}
"#,
        )
        .expect("write existing claude desktop config");
        let cfg = config::default_deepseek_config();
        // config has deepseek-v4-flash-lp and deepseek-v4-pro-lp; NOT "ghost-model"

        launch_claude_desktop(
            &cfg,
            Some(temp.path().to_path_buf()),
            "llm-proxy".to_string(),
            false,
        )
        .expect("launch claude desktop");

        let value: Value =
            serde_json::from_str(&fs::read_to_string(&path).expect("read")).expect("parse");
        let models = value["llmProxy"]["providers"]["llm-proxy"]["models"]
            .as_array()
            .expect("models");
        let ids: Vec<&str> = models.iter().filter_map(|m| m.as_str()).collect();
        // Stale model must be gone
        assert!(
            !ids.contains(&"ghost-model"),
            "stale model should be dropped; got: {ids:?}"
        );
        // Config models must be present
        assert!(ids.contains(&"deepseek-v4-pro-lp"));
        assert!(ids.contains(&"deepseek-v4-flash-lp"));
    }

    // ── : launch_opencode tests ──────────────────────────────

    /// Helper: parse the written JSONC file back to a serde_json::Value for assertions.
    fn opencode_config_value(path: &std::path::Path) -> Value {
        let text = fs::read_to_string(path).expect("read opencode config");
        // jsonc-parser's serde integration strips comments and returns serde_json::Value.
        jsonc_parser::parse_to_serde_value(&text, &jsonc_parser::ParseOptions::default())
            .expect("parse opencode config")
    }

    #[test]
    fn launch_opencode_writes_provider_config() {
        let temp = tempfile::tempdir().expect("tempdir");
        let cfg = config::default_deepseek_config();

        launch_opencode(&cfg, Some(temp.path().to_path_buf()), false).expect("launch opencode");

        let path = temp.path().join("opencode.jsonc");
        assert!(path.exists(), "opencode.jsonc should be created");

        let value = opencode_config_value(&path);
        let provider = &value["provider"]["llm-proxy"];
        assert_eq!(provider["npm"], "@ai-sdk/openai-compatible");
        assert_eq!(provider["name"], "llm-proxy");
        assert_eq!(provider["env"][0], "LLM_PROXY_API_KEY");
        assert_eq!(
            provider["options"]["baseURL"],
            "http://127.0.0.1:8989/openai/v1"
        );
        assert_eq!(provider["options"]["apiKey"], "${LLM_PROXY_API_KEY}");

        // Models should be present.
        let models = provider["models"].as_object().expect("models object");
        assert!(
            models.contains_key("deepseek-v4-pro-lp"),
            "should contain deepseek-v4-pro-lp"
        );
        assert!(
            models.contains_key("deepseek-v4-flash-lp"),
            "should contain deepseek-v4-flash-lp"
        );

        // Each model should have id and name fields.
        for (key, model) in models {
            assert_eq!(model["id"], json!(key));
            assert_eq!(model["name"], json!(key));
        }
    }

    #[test]
    fn launch_opencode_preserves_existing_providers() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("opencode.jsonc");
        fs::write(
            &path,
            r#"{
  "provider": {
    "my-custom-provider": {
      "name": "custom",
      "options": { "baseURL": "https://custom.example.com" }
    }
  },
  "defaultModel": "my-custom-provider/some-model"
}"#,
        )
        .expect("write existing config");
        let cfg = config::default_deepseek_config();

        launch_opencode(&cfg, Some(temp.path().to_path_buf()), false).expect("launch opencode");

        let value = opencode_config_value(&path);
        // User's custom provider preserved.
        assert_eq!(value["provider"]["my-custom-provider"]["name"], "custom");
        assert_eq!(
            value["provider"]["my-custom-provider"]["options"]["baseURL"],
            "https://custom.example.com"
        );
        // User's default model preserved.
        assert_eq!(value["defaultModel"], "my-custom-provider/some-model");
        // llm-proxy provider added.
        assert_eq!(value["provider"]["llm-proxy"]["name"], "llm-proxy");
    }

    #[test]
    fn launch_opencode_preserves_comments_in_jsonc() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("opencode.jsonc");
        let existing = r#"{
  // User's custom provider
  "provider": {
    // Do not remove this
    "my-provider": {
      "name": "test"
    }
  },
  // My default model
  "defaultModel": "my-provider/model"
}"#;
        fs::write(&path, existing).expect("write existing jsonc");
        let cfg = config::default_deepseek_config();

        launch_opencode(&cfg, Some(temp.path().to_path_buf()), false).expect("launch opencode");

        let text = fs::read_to_string(&path).expect("read back");
        // All original comments must survive the round-trip.
        assert!(
            text.contains("// User's custom provider"),
            "comment should be preserved"
        );
        assert!(
            text.contains("// Do not remove this"),
            "nested comment should be preserved"
        );
        assert!(
            text.contains("// My default model"),
            "top-level comment should be preserved"
        );
        // And the llm-proxy provider should be injected.
        assert!(
            text.contains("\"llm-proxy\""),
            "llm-proxy provider should be present"
        );
    }

    #[test]
    fn launch_opencode_idempotent() {
        let temp = tempfile::tempdir().expect("tempdir");
        let cfg = config::default_deepseek_config();

        launch_opencode(&cfg, Some(temp.path().to_path_buf()), false).expect("first launch");
        let path = temp.path().join("opencode.jsonc");
        let first_text = fs::read_to_string(&path).expect("read first");

        launch_opencode(&cfg, Some(temp.path().to_path_buf()), false).expect("second launch");
        let second_text = fs::read_to_string(&path).expect("read second");

        // Both runs should produce parseable config with the same structure.
        let first_value = opencode_config_value(&path);
        // Re-parse from first text for comparison.
        let first_parsed: Value =
            jsonc_parser::parse_to_serde_value(&first_text, &jsonc_parser::ParseOptions::default())
                .unwrap();
        let second_parsed: Value = jsonc_parser::parse_to_serde_value(
            &second_text,
            &jsonc_parser::ParseOptions::default(),
        )
        .unwrap();
        assert_eq!(
            first_parsed, second_parsed,
            "idempotent: values should match"
        );
        assert_eq!(first_value["provider"]["llm-proxy"]["name"], "llm-proxy");
    }

    #[test]
    fn launch_opencode_uses_server_listen_base_url() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut cfg = config::default_deepseek_config();
        // Setting a custom port; 0.0.0.0 is normalized to 127.0.0.1 by local_server_base_url.
        cfg.server.as_mut().unwrap().listen = "0.0.0.0:9999".to_string();

        launch_opencode(&cfg, Some(temp.path().to_path_buf()), false).expect("launch opencode");

        let path = temp.path().join("opencode.jsonc");
        let value = opencode_config_value(&path);
        assert_eq!(
            value["provider"]["llm-proxy"]["options"]["baseURL"],
            "http://127.0.0.1:9999/openai/v1"
        );
    }

    /// launch opencode must respect config.toml auth_token_env,
    /// not hardcode LLM_PROXY_API_KEY.
    #[test]
    fn launch_opencode_respects_auth_token_env() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut cfg = config::default_deepseek_config();
        cfg.server = None;
        cfg.remote = Some(config::RemoteConfig {
            url: "https://cncsmonster.com/lp".to_string(),
            auth_mode: Some(config::RemoteAuthMode::Token),
            auth_token_env: "CNCSMONSTER_API_KEY".into(),
        });

        launch_opencode(&cfg, Some(temp.path().to_path_buf()), false).expect("launch opencode");

        let path = temp.path().join("opencode.jsonc");
        let value = opencode_config_value(&path);
        let provider = &value["provider"]["llm-proxy"];

        // env array must reference the configured env var, not the default.
        assert_eq!(
            provider["env"][0], "CNCSMONSTER_API_KEY",
            "env must use auth_token_env from config"
        );
        assert_eq!(
            provider["options"]["apiKey"], "${CNCSMONSTER_API_KEY}",
            "apiKey template must use auth_token_env from config"
        );
        // base URL must point to remote.
        assert_eq!(
            provider["options"]["baseURL"],
            "https://cncsmonster.com/lp/openai/v1"
        );
    }

    #[test]
    fn launch_pi_writes_directly_to_config_dir_without_pi_agent_nesting() {
        let temp = tempfile::tempdir().expect("tempdir");
        let cfg = config::default_deepseek_config();

        launch_pi(&cfg, Some(temp.path().to_path_buf()), false).expect("launch pi");

        let flat_path = temp.path().join("models.json");
        let nested_dir = temp.path().join(".pi");
        assert!(
            flat_path.exists(),
            "models.json must exist directly under target dir"
        );
        assert!(
            !nested_dir.exists(),
            "must not derive .pi/agent subdirectories"
        );
    }

    #[test]
    fn launch_claude_desktop_linux_dual_branch() {
        let cfg = config::default_deepseek_config();

        if cfg!(target_os = "linux") {
            // Default target (None) on Linux must error
            let err = launch_claude_desktop(&cfg, None, "llm-proxy".to_string(), false)
                .expect_err("default target on Linux must fail");
            assert!(
                err.to_string()
                    .contains("Linux has no official Claude Desktop configuration directory"),
                "error message must guide user: {err}"
            );

            // Explicit target (Some) on Linux must succeed
            let temp = tempfile::tempdir().expect("tempdir");
            launch_claude_desktop(
                &cfg,
                Some(temp.path().to_path_buf()),
                "llm-proxy".to_string(),
                false,
            )
            .expect("explicit target on Linux must succeed");
            assert!(temp.path().join("claude_desktop_config.json").exists());
        }
    }

    /// : Concurrent execution of `launch_claude_code` with isolated
    /// directory trees succeeds without file collision or atomic rename races on
    /// the shared `/tmp/.claude.json`.
    #[test]
    fn claude_code_concurrent_launch_isolation_stress() {
        let handles: Vec<_> = (0..8)
            .map(|_| {
                std::thread::spawn(|| {
                    let temp = tempfile::tempdir().expect("tempdir");
                    let claude_home = temp.path().join(".claude");
                    fs::create_dir_all(&claude_home).expect("create claude home");
                    let cfg = config::default_deepseek_config();
                    launch_claude_code(&cfg, Some(claude_home), false)
                })
            })
            .collect();

        for h in handles {
            let res = h.join().expect("thread should not panic");
            res.expect("launch should succeed");
        }
    }
}
