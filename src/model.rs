use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::config::{Config, ModelConfig, Protocol, ProviderBinding};

/// Write a model config while holding the cross-process file lock, matching
/// the server's `save_config()` behavior so CLI model commands cannot lose
/// updates to a concurrently running server.
fn write_model_locked(path: &Path, cfg: &Config, model_id: &str) -> Result<()> {
    let _lock =
        crate::core::ConfigLock::acquire(&crate::service::state_dir(), Duration::from_secs(5))?;
    crate::config_edit::write_model(path, cfg, model_id)
}

pub fn list(cfg: &Config) {
    for (id, model) in &cfg.models {
        let protocols = Protocol::CLIENT_PROTOCOLS
            .into_iter()
            .filter(|protocol| model.exposes_protocol(*protocol))
            .map(|protocol| protocol.route_key())
            .collect::<Vec<_>>()
            .join(",");
        println!(
            "{} context={} max_output={} protocols={}",
            id, model.context_window, model.max_output_tokens, protocols
        );
    }
    if !cfg.models.is_empty() {
        println!();
        println!(
            "Protocols: chat=OpenAI Chat Completions (/v1/chat/completions), responses=OpenAI Responses (/v1/responses), anthropic=Anthropic Messages (/v1/messages)"
        );
    }
}

pub fn info(cfg: &Config, model_id: &str) -> Result<()> {
    let model = cfg
        .models
        .get(model_id)
        .with_context(|| format!("unknown model {model_id:?}"))?;
    println!("model={model_id}");
    println!("context_window={}", model.context_window);
    println!("max_output_tokens={}", model.max_output_tokens);
    for protocol in Protocol::CLIENT_PROTOCOLS {
        let bindings = model.provider_bindings(protocol);
        if bindings.is_empty() {
            continue;
        }
        println!("{}:", protocol.field_name());
        for (index, binding) in bindings.iter().enumerate() {
            println!(
                "  {}. provider={} upstream_model={}",
                index + 1,
                binding.name,
                binding.model
            );
        }
    }
    Ok(())
}

/// 委托模式：格式化 server 返回的 model 列表（与本地 model::list 输出一致）。
pub fn print_model_list_json(result: &serde_json::Value) -> anyhow::Result<()> {
    use anyhow::Context;
    let models = result
        .get("data")
        .and_then(|d| d.get("models"))
        .and_then(serde_json::Value::as_array)
        .context("missing models data")?;
    for m in models {
        let id = m
            .get("id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("?");
        let ctx = m
            .get("context_window")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0);
        let max_out = m
            .get("max_output_tokens")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0);
        let protocols = m
            .get("protocols")
            .and_then(serde_json::Value::as_array)
            .map(|ps| {
                ps.iter()
                    .filter_map(serde_json::Value::as_str)
                    .collect::<Vec<_>>()
                    .join(",")
            })
            .unwrap_or_default();
        println!("{id} context={ctx} max_output={max_out} protocols={protocols}");
    }
    if !models.is_empty() {
        println!();
        println!(
            "Protocols: chat=OpenAI Chat Completions (/v1/chat/completions), responses=OpenAI Responses (/v1/responses), anthropic=Anthropic Messages (/v1/messages)"
        );
    }
    Ok(())
}

/// 委托模式：格式化 server 返回的 model 详情（与本地 model::info 输出一致）。
pub fn print_model_info_json(result: &serde_json::Value) -> anyhow::Result<()> {
    use anyhow::Context;
    let data = result.get("data").context("missing data")?;
    let id = data
        .get("id")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("?");
    println!("model={id}");
    println!(
        "context_window={}",
        data.get("context_window")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0)
    );
    println!(
        "max_output_tokens={}",
        data.get("max_output_tokens")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0)
    );
    if let Some(providers) = data.get("providers").and_then(serde_json::Value::as_object) {
        for (protocol, bindings) in providers {
            println!("{protocol}:");
            if let Some(bindings) = bindings.as_array() {
                for b in bindings {
                    let idx = b
                        .get("index")
                        .and_then(serde_json::Value::as_i64)
                        .unwrap_or(0);
                    let provider = b
                        .get("provider")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("");
                    let upstream = b
                        .get("upstream_model")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("");
                    println!("  {idx}. provider={provider} upstream_model={upstream}");
                }
            }
        }
    }
    Ok(())
}

/// Window flag gate: required unless `--copy-from`/`--from-discovery`; when
/// explicitly provided it must be > 0. Distinguishes "not provided" (None)
/// from "provided but invalid" (Some(<=0)) in the error message.
fn required_window(value: Option<i64>, flag: &str) -> Result<i64> {
    match value {
        None => bail!("model add requires {flag} unless --copy-from or --from-discovery is used"),
        Some(v) if v <= 0 => bail!("{flag} must be > 0"),
        Some(v) => Ok(v),
    }
}

/// Shared validation for `model add` window params — called by the CLI layer
/// (fail fast before delegating/local write) and by the write gate in `add`
/// (final guard for every write path). Single rule source, no drift.
/// Returns the validated window pair, or None in copy-from mode (windows are
/// copied from the source model).
pub fn validate_add_windows(
    context_window: Option<i64>,
    max_output_tokens: Option<i64>,
    copy_from: Option<&str>,
) -> Result<Option<(i64, i64)>> {
    if copy_from.is_some() {
        return Ok(None);
    }
    Ok(Some((
        required_window(context_window, "--context-window")?,
        required_window(max_output_tokens, "--max-output")?,
    )))
}

/// Shared validation for from-discovery window overrides: an explicitly
/// provided override must be > 0 (None means "not overridden, use discovery
/// cache"). Called by the CLI layer and by the add_from_discovery gate.
pub fn validate_window_override(value: Option<i64>, flag: &str) -> Result<()> {
    if let Some(v) = value
        && v <= 0
    {
        bail!("{flag} must be > 0");
    }
    Ok(())
}

pub fn add(
    path: &Path,
    model_id: &str,
    context_window: Option<i64>,
    max_output_tokens: Option<i64>,
    copy_from: Option<&str>,
) -> Result<()> {
    // Gate: validate params before any state read/write. This function is the
    // shared entry of every write path (CLI direct, UDS-delegated, direct
    // admin API), so the check must live here, not in the CLI layer.
    let windows = validate_add_windows(context_window, max_output_tokens, copy_from)?;
    let mut cfg = Config::load(path)?;
    if cfg.models.contains_key(model_id) {
        bail!("model {model_id:?} already exists");
    }
    let model = if let Some(source) = copy_from {
        cfg.models
            .get(source)
            .with_context(|| format!("unknown source model {source:?}"))?
            .clone()
    } else {
        let (context_window, max_output_tokens) = windows.expect("validated in gate above");
        ModelConfig {
            description: None,
            context_window,
            max_output_tokens,
            features: Vec::new(),
            supported_reasoning_levels: Vec::new(),
            default_reasoning_level: None,
            enable_thinking: None,
            openai_chat_providers: Vec::new(),
            openai_responses_providers: Vec::new(),
            anthropic_providers: Vec::new(),
            reasoning_level_map: None,
        }
    };
    cfg.models.insert(model_id.to_string(), model);
    write_model_locked(path, &cfg, model_id)?;
    println!("added model: {model_id}");
    Ok(())
}

#[derive(Debug, Clone, Default)]
pub struct SetModelOptions {
    pub context_window: Option<i64>,
    pub max_output_tokens: Option<i64>,
    pub supported_reasoning_levels: Option<Vec<String>>,
    pub thinking_level: Option<String>,
    pub enable_thinking: Option<bool>,
    pub enable_features: Vec<String>,
    pub disable_features: Vec<String>,
}

pub fn set(path: &Path, model_id: &str, options: SetModelOptions) -> Result<()> {
    let mut cfg = Config::load(path)?;
    {
        let model = cfg
            .models
            .get_mut(model_id)
            .with_context(|| format!("unknown model {model_id:?}"))?;
        if let Some(context_window) = options.context_window {
            if context_window <= 0 {
                bail!("--context-window must be > 0");
            }
            model.context_window = context_window;
        }
        if let Some(max_output_tokens) = options.max_output_tokens {
            if max_output_tokens <= 0 {
                bail!("--max-output must be > 0");
            }
            model.max_output_tokens = max_output_tokens;
        }
        if let Some(levels) = options.supported_reasoning_levels {
            validate_reasoning_levels(&levels)?;
            model.supported_reasoning_levels = levels;
            if let Some(default) = &model.default_reasoning_level
                && !model.supported_reasoning_levels.is_empty()
                && !model.supported_reasoning_levels.contains(default)
            {
                bail!(
                    "existing default_reasoning_level {default:?} is not in new supported_reasoning_levels"
                );
            }
        }
        if let Some(thinking_level) = options.thinking_level {
            if !model.supported_reasoning_levels.is_empty()
                && !model.supported_reasoning_levels.contains(&thinking_level)
            {
                bail!(
                    "thinking level {thinking_level:?} is not supported by model {model_id:?}; supported values: {}",
                    model.supported_reasoning_levels.join(", ")
                );
            }
            model.default_reasoning_level = Some(thinking_level);
        }
        if let Some(enable_thinking) = options.enable_thinking {
            model.enable_thinking = Some(enable_thinking);
        }
        for feature in options.enable_features {
            validate_feature_name(&feature)?;
            if !model.features.contains(&feature) {
                model.features.push(feature);
            }
        }
        for feature in options.disable_features {
            validate_feature_name(&feature)?;
            model.features.retain(|existing| existing != &feature);
        }
        model.features.sort();
    }
    cfg.validate()?;
    write_model_locked(path, &cfg, model_id)?;
    println!("updated model: {model_id}");
    Ok(())
}

fn validate_feature_name(feature: &str) -> Result<()> {
    match feature {
        "image_input" | "document_input" | "tools" | "tool_call_reasoning" => Ok(()),
        _ => bail!(
            "unknown model feature {feature:?}; expected image_input, document_input, tools, or tool_call_reasoning"
        ),
    }
}

fn validate_reasoning_levels(levels: &[String]) -> Result<()> {
    let mut seen = std::collections::BTreeSet::new();
    for level in levels {
        if level.trim().is_empty() {
            bail!("supported reasoning levels must not contain empty values");
        }
        if !seen.insert(level) {
            bail!("supported reasoning levels must not contain duplicates: {level:?}");
        }
    }
    Ok(())
}

pub fn add_from_discovery(
    path: &Path,
    model_id: &str,
    provider_id: &str,
    upstream_model: Option<&str>,
    context_window_override: Option<i64>,
    max_output_override: Option<i64>,
) -> Result<()> {
    add_from_discovery_with_cache_path(
        path,
        &crate::status::cache_path(),
        model_id,
        provider_id,
        upstream_model,
        context_window_override,
        max_output_override,
    )
}

fn add_from_discovery_with_cache_path(
    path: &Path,
    cache_path: &Path,
    model_id: &str,
    provider_id: &str,
    upstream_model: Option<&str>,
    context_window_override: Option<i64>,
    max_output_override: Option<i64>,
) -> Result<()> {
    // Gate: validate overrides before any state read/write.
    validate_window_override(context_window_override, "--context-window")?;
    validate_window_override(max_output_override, "--max-output")?;
    let mut cfg = Config::load(path)?;
    if cfg.models.contains_key(model_id) {
        bail!("model {model_id:?} already exists");
    }
    let provider = cfg
        .providers
        .get(provider_id)
        .with_context(|| format!("unknown provider {provider_id:?}"))?;
    let upstream_model = upstream_model.unwrap_or(model_id);
    let cache = crate::status::read_cache(cache_path).unwrap_or_default();
    let row = cache
        .dynamic_models
        .get(provider_id)
        .into_iter()
        .flatten()
        .find(|row| row.model_id == upstream_model)
        .with_context(|| {
            format!(
                "no discovered model {upstream_model:?} for provider {provider_id:?}; run `llm-proxy status --refresh` first"
            )
        })?;
    let context_window = match context_window_override {
        Some(v) => v,
        None => row.context_window.with_context(|| {
            format!(
                "discovered model {upstream_model:?} has no context_window; pass --context-window"
            )
        })?,
    };
    let max_output_tokens = match max_output_override {
        Some(v) => v,
        None => row.max_output_tokens.unwrap_or(context_window),
    };
    let mut model = ModelConfig {
        description: row.display_name.clone(),
        context_window,
        max_output_tokens,
        features: row.features.clone(),
        supported_reasoning_levels: row.supported_reasoning_levels.clone(),
        default_reasoning_level: row.default_reasoning_level.clone(),
        enable_thinking: None,
        openai_chat_providers: Vec::new(),
        openai_responses_providers: Vec::new(),
        anthropic_providers: Vec::new(),
        reasoning_level_map: None,
    };
    for protocol in Protocol::CLIENT_PROTOCOLS {
        if provider.endpoint(protocol).is_some() {
            provider_bindings_mut(&mut model, protocol).push(ProviderBinding {
                name: provider_id.to_string(),
                model: upstream_model.to_string(),
            });
        }
    }
    if !model.exposes_protocol(Protocol::OpenaiChatCompletions)
        && !model.exposes_protocol(Protocol::OpenaiResponses)
        && !model.exposes_protocol(Protocol::Anthropic)
    {
        bail!("provider {provider_id:?} has no client-protocol endpoints");
    }
    cfg.models.insert(model_id.to_string(), model);
    write_model_locked(path, &cfg, model_id)?;
    println!(
        "added model from discovery: {model_id} provider={provider_id} upstream_model={upstream_model}"
    );
    Ok(())
}

pub fn remove(path: &Path, model_id: &str, force: bool) -> Result<()> {
    if !force {
        bail!("model remove requires --force until interactive confirmation is implemented");
    }
    let mut cfg = Config::load(path)?;
    if cfg.models.remove(model_id).is_none() {
        bail!("unknown model {model_id:?}");
    }
    let _lock =
        crate::core::ConfigLock::acquire(&crate::service::state_dir(), Duration::from_secs(5))?;
    crate::config_edit::remove_model(path, &cfg, model_id)?;
    println!("removed model: {model_id}");
    Ok(())
}

pub fn provider_add(
    path: &Path,
    model_id: &str,
    protocol: Protocol,
    provider_id: &str,
    upstream_model: Option<String>,
) -> Result<()> {
    let mut cfg = Config::load(path)?;
    let provider = cfg
        .providers
        .get(provider_id)
        .with_context(|| format!("unknown provider {provider_id:?}"))?;
    if provider.endpoint(protocol).is_none() {
        bail!(
            "provider {provider_id:?} does not declare {} endpoint",
            protocol.field_name()
        );
    }
    let model = cfg
        .models
        .get_mut(model_id)
        .with_context(|| format!("unknown model {model_id:?}"))?;
    let bindings = provider_bindings_mut(model, protocol);
    if bindings.iter().any(|binding| binding.name == provider_id) {
        bail!("model {model_id:?} already has provider {provider_id:?} for {protocol:?}");
    }
    let resolved_model = match upstream_model {
        Some(m) => m,
        None => bindings
            .first()
            .map(|head| head.model.clone())
            .with_context(|| {
                format!(
                    "no existing binding to copy upstream model from; \
                     --upstream-model is required when adding the first provider \
                     for {protocol:?}",
                )
            })?,
    };
    bindings.push(ProviderBinding {
        name: provider_id.to_string(),
        model: resolved_model,
    });
    write_model_locked(path, &cfg, model_id)?;
    println!(
        "added provider {provider_id} to model {model_id} {}",
        protocol.field_name()
    );
    Ok(())
}

pub fn provider_remove(
    path: &Path,
    model_id: &str,
    protocol: Protocol,
    provider_id: &str,
) -> Result<()> {
    let mut cfg = Config::load(path)?;
    let model = cfg
        .models
        .get_mut(model_id)
        .with_context(|| format!("unknown model {model_id:?}"))?;
    let bindings = provider_bindings_mut(model, protocol);
    let before = bindings.len();
    bindings.retain(|binding| binding.name != provider_id);
    if bindings.len() == before {
        bail!("model {model_id:?} has no provider {provider_id:?} for {protocol:?}");
    }
    write_model_locked(path, &cfg, model_id)?;
    println!(
        "removed provider {provider_id} from model {model_id} {}",
        protocol.field_name()
    );
    Ok(())
}

pub fn provider_move(
    path: &Path,
    model_id: &str,
    protocol: Protocol,
    provider_id: &str,
    to_index: usize,
) -> Result<()> {
    if to_index == 0 {
        bail!("--to is 1-based and must be >= 1");
    }
    let mut cfg = Config::load(path)?;
    let model = cfg
        .models
        .get_mut(model_id)
        .with_context(|| format!("unknown model {model_id:?}"))?;
    let bindings = provider_bindings_mut(model, protocol);
    let from = bindings
        .iter()
        .position(|binding| binding.name == provider_id)
        .with_context(|| format!("model {model_id:?} has no provider {provider_id:?}"))?;
    let binding = bindings.remove(from);
    let to = (to_index - 1).min(bindings.len());
    if to_index - 1 > bindings.len() {
        eprintln!(
            "warning: --to {to_index} exceeds binding count ({}), clamped to position {}",
            bindings.len(),
            bindings.len() + 1
        );
    }
    bindings.insert(to, binding);
    write_model_locked(path, &cfg, model_id)?;
    println!(
        "moved provider {provider_id} in model {model_id} {} to {}",
        protocol.field_name(),
        to + 1
    );
    Ok(())
}

fn provider_bindings_mut(model: &mut ModelConfig, protocol: Protocol) -> &mut Vec<ProviderBinding> {
    match protocol {
        Protocol::OpenaiChatCompletions => &mut model.openai_chat_providers,
        Protocol::OpenaiResponses => &mut model.openai_responses_providers,
        Protocol::Anthropic => &mut model.anthropic_providers,
        Protocol::Antigravity => unreachable!("antigravity is not a client protocol"),
    }
}

pub fn parse_client_protocol(value: &str) -> Result<Protocol> {
    match value {
        "openai-chat" | "openai-chat-completions" | "chat" | "chat-completions" => {
            Ok(Protocol::OpenaiChatCompletions)
        }
        "openai-responses" | "responses" => Ok(Protocol::OpenaiResponses),
        "anthropic" | "anthropic-messages" => Ok(Protocol::Anthropic),
        _ => bail!(
            "unsupported --type {value:?}; expected openai-chat, openai-responses, or anthropic"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct StateDirGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        old: Option<std::ffi::OsString>,
        _temp: tempfile::TempDir,
    }

    impl StateDirGuard {
        fn new() -> Self {
            let lock = ENV_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let temp = tempfile::tempdir().expect("state tempdir");
            let old = std::env::var_os("LLM_PROXY_STATE_DIR");
            unsafe { std::env::set_var("LLM_PROXY_STATE_DIR", temp.path()) };
            Self {
                _lock: lock,
                old,
                _temp: temp,
            }
        }
    }

    impl Drop for StateDirGuard {
        fn drop(&mut self) {
            match &self.old {
                Some(v) => unsafe { std::env::set_var("LLM_PROXY_STATE_DIR", v) },
                None => unsafe { std::env::remove_var("LLM_PROXY_STATE_DIR") },
            }
        }
    }

    fn isolated_state() -> StateDirGuard {
        StateDirGuard::new()
    }

    fn write_config(text: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("config.toml");
        std::fs::write(&path, text).expect("write config");
        (temp, path)
    }

    #[test]
    #[serial]
    fn parse_client_protocol_accepts_aliases() {
        assert_eq!(
            parse_client_protocol("openai-chat").unwrap(),
            Protocol::OpenaiChatCompletions
        );
        assert_eq!(
            parse_client_protocol("openai-chat-completions").unwrap(),
            Protocol::OpenaiChatCompletions
        );
        assert_eq!(
            parse_client_protocol("chat").unwrap(),
            Protocol::OpenaiChatCompletions
        );
        assert_eq!(
            parse_client_protocol("chat-completions").unwrap(),
            Protocol::OpenaiChatCompletions
        );
        assert_eq!(
            parse_client_protocol("openai-responses").unwrap(),
            Protocol::OpenaiResponses
        );
        assert_eq!(
            parse_client_protocol("responses").unwrap(),
            Protocol::OpenaiResponses
        );
        assert_eq!(
            parse_client_protocol("anthropic").unwrap(),
            Protocol::Anthropic
        );
        assert_eq!(
            parse_client_protocol("anthropic-messages").unwrap(),
            Protocol::Anthropic
        );
    }

    #[test]
    fn parse_client_protocol_rejects_unknown_type() {
        let err = parse_client_protocol("antigravity")
            .unwrap_err()
            .to_string();
        assert!(err.contains("unsupported --type"));
    }

    #[test]
    fn print_json_helpers_reject_missing_data() {
        assert!(print_model_list_json(&serde_json::json!({})).is_err());
        assert!(print_model_info_json(&serde_json::json!({})).is_err());
    }

    #[test]
    fn print_json_helpers_accept_sparse_server_payloads() {
        print_model_list_json(&serde_json::json!({"data":{"models":[{"id":"m"}]}})).unwrap();
        print_model_info_json(&serde_json::json!({"data":{"id":"m","providers":{"openai_chat":[{"index":1,"provider":"p","upstream_model":"u"}]}}})).unwrap();
    }

    #[test]
    fn info_rejects_unknown_model() {
        let (_temp, path) = write_config("[server]\nlisten='127.0.0.1:8989'\n");
        let cfg = Config::load(&path).unwrap();
        let err = info(&cfg, "missing").unwrap_err().to_string();
        assert!(err.contains("unknown model"));
    }

    #[test]
    fn add_rejects_duplicate_model_and_unknown_copy_source() {
        let _state = isolated_state();
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        crate::config::init_config(&path).unwrap();
        assert!(
            add(&path, "deepseek-v4-flash-lp", Some(1), Some(1), None)
                .unwrap_err()
                .to_string()
                .contains("already exists")
        );
        assert!(
            add(&path, "copy", Some(1), Some(1), Some("missing"))
                .unwrap_err()
                .to_string()
                .contains("unknown source model")
        );
    }

    #[test]
    fn validate_add_windows_shared_by_cli_and_gate() {
        // 缺窗口 → requires 报错
        let err = validate_add_windows(None, None, None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("requires --context-window"));
        let err = validate_add_windows(Some(10), None, None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("requires --max-output"));
        // 显式 0 → must be > 0
        let err = validate_add_windows(Some(0), Some(10), None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("--context-window must be > 0"));
        // copy-from 模式跳过，返回 None
        assert_eq!(validate_add_windows(None, None, Some("src")).unwrap(), None);
        // 合法值返回校验后的窗口对
        assert_eq!(
            validate_add_windows(Some(100), Some(20), None).unwrap(),
            Some((100, 20))
        );
    }

    #[test]
    fn validate_window_override_rejects_non_positive() {
        assert!(validate_window_override(None, "--context-window").is_ok());
        assert!(validate_window_override(Some(5), "--context-window").is_ok());
        let err = validate_window_override(Some(0), "--context-window")
            .unwrap_err()
            .to_string();
        assert!(err.contains("--context-window must be > 0"));
        let err = validate_window_override(Some(-1), "--max-output")
            .unwrap_err()
            .to_string();
        assert!(err.contains("--max-output must be > 0"));
    }

    #[test]
    fn add_gate_distinguishes_missing_windows_from_zero() {
        let _state = isolated_state();
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        crate::config::init_config(&path).unwrap();
        // 未提供 → requires 报错
        let err = add(&path, "m", None, None, None).unwrap_err().to_string();
        assert!(err.contains("requires --context-window"));
        let err = add(&path, "m", Some(10), None, None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("requires --max-output"));
        // 显式提供 0 → must be > 0 报错（与"未提供"区分）
        let err = add(&path, "m", Some(0), Some(10), None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("--context-window must be > 0"));
        let err = add(&path, "m", Some(10), Some(0), None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("--max-output must be > 0"));
        // copy-from 模式跳过窗口 gate（不报 requires，报 unknown source）
        let err = add(&path, "m", None, None, Some("missing"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown source model"));
    }

    #[test]
    fn add_from_discovery_rejects_zero_override_instead_of_treating_as_unset() {
        let _state = isolated_state();
        let (_temp, path) = write_config(
            "[server]\nlisten='127.0.0.1:8989'\n[providers.p]\n[providers.p.openai_chat]\nurl='https://example.test/v1'\n",
        );
        let temp2 = tempfile::tempdir().unwrap();
        let cache_path = temp2.path().join("cache.json");
        let mut cache = crate::status::StatusCache::default();
        cache.dynamic_models.insert(
            "p".into(),
            vec![crate::status::DynamicModelCacheEntry {
                provider_id: "p".into(),
                source_url: "u".into(),
                probed_at_unix: 1,
                stale_after_unix: 2,
                model_id: "up".into(),
                display_name: None,
                context_window: Some(100),
                max_output_tokens: Some(10),
                features: vec![],
                supported_parameters: vec![],
                supported_reasoning_levels: vec![],
                default_reasoning_level: None,
            }],
        );
        crate::status::write_cache(&cache_path, &cache).unwrap();
        let err = add_from_discovery_with_cache_path(
            &path,
            &cache_path,
            "m",
            "p",
            Some("up"),
            Some(0),
            None,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("--context-window must be > 0"));
        let err = add_from_discovery_with_cache_path(
            &path,
            &cache_path,
            "m",
            "p",
            Some("up"),
            None,
            Some(0),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("--max-output must be > 0"));
    }

    #[test]
    fn add_can_copy_existing_model_config() {
        let _state = isolated_state();
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        crate::config::init_config(&path).unwrap();
        add(
            &path,
            "copied",
            Some(1),
            Some(1),
            Some("deepseek-v4-flash-lp"),
        )
        .unwrap();
        let cfg = Config::load(&path).unwrap();
        assert_eq!(
            cfg.models["copied"].context_window,
            cfg.models["deepseek-v4-flash-lp"].context_window
        );
        assert_eq!(
            cfg.models["copied"].openai_chat_providers.len(),
            cfg.models["deepseek-v4-flash-lp"]
                .openai_chat_providers
                .len()
        );
    }

    #[test]
    fn set_rejects_non_positive_token_limits() {
        let _state = isolated_state();
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        crate::config::init_config(&path).unwrap();
        assert!(
            set(
                &path,
                "deepseek-v4-flash-lp",
                SetModelOptions {
                    context_window: Some(0),
                    ..Default::default()
                }
            )
            .unwrap_err()
            .to_string()
            .contains("--context-window")
        );
        assert!(
            set(
                &path,
                "deepseek-v4-flash-lp",
                SetModelOptions {
                    max_output_tokens: Some(0),
                    ..Default::default()
                }
            )
            .unwrap_err()
            .to_string()
            .contains("--max-output")
        );
    }

    #[test]
    fn set_rejects_bad_reasoning_levels() {
        let _state = isolated_state();
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        crate::config::init_config(&path).unwrap();
        assert!(
            set(
                &path,
                "deepseek-v4-flash-lp",
                SetModelOptions {
                    supported_reasoning_levels: Some(vec!["low".into(), "low".into()]),
                    ..Default::default()
                }
            )
            .unwrap_err()
            .to_string()
            .contains("duplicates")
        );
        assert!(
            set(
                &path,
                "deepseek-v4-flash-lp",
                SetModelOptions {
                    supported_reasoning_levels: Some(vec![" ".into()]),
                    ..Default::default()
                }
            )
            .unwrap_err()
            .to_string()
            .contains("empty")
        );
    }

    #[test]
    fn set_rejects_new_levels_that_exclude_existing_default() {
        let _state = isolated_state();
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        crate::config::init_config(&path).unwrap();
        set(
            &path,
            "deepseek-v4-flash-lp",
            SetModelOptions {
                supported_reasoning_levels: Some(vec![
                    "low".into(),
                    "medium".into(),
                    "high".into(),
                ]),
                thinking_level: Some("medium".into()),
                ..Default::default()
            },
        )
        .unwrap();
        let err = set(
            &path,
            "deepseek-v4-flash-lp",
            SetModelOptions {
                supported_reasoning_levels: Some(vec!["low".into()]),
                ..Default::default()
            },
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("existing default_reasoning_level"));
    }

    #[test]
    fn set_feature_toggles_are_idempotent_and_sorted() {
        let _state = isolated_state();
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        crate::config::init_config(&path).unwrap();
        add(&path, "feature-model", Some(100), Some(10), None).unwrap();
        set(
            &path,
            "feature-model",
            SetModelOptions {
                enable_features: vec!["tools".into(), "image_input".into(), "tools".into()],
                disable_features: vec!["document_input".into()],
                ..Default::default()
            },
        )
        .unwrap();
        let cfg = Config::load(&path).unwrap();
        assert_eq!(
            cfg.models["feature-model"].features,
            vec!["image_input", "tools"]
        );
    }

    #[test]
    fn provider_add_rejects_unknown_or_incompatible_provider() {
        let _state = isolated_state();
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        crate::config::init_config(&path).unwrap();
        add(&path, "m", Some(100), Some(10), None).unwrap();
        assert!(
            provider_add(&path, "m", Protocol::OpenaiChatCompletions, "missing", None)
                .unwrap_err()
                .to_string()
                .contains("unknown provider")
        );
        assert!(
            provider_add(&path, "m", Protocol::Antigravity, "deepseek", None)
                .unwrap_err()
                .to_string()
                .contains("does not declare")
        );
    }

    #[test]
    fn provider_add_rejects_unknown_model_and_duplicate_binding() {
        let _state = isolated_state();
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        crate::config::init_config(&path).unwrap();
        assert!(
            provider_add(
                &path,
                "missing",
                Protocol::OpenaiChatCompletions,
                "deepseek",
                None
            )
            .unwrap_err()
            .to_string()
            .contains("unknown model")
        );
        add(&path, "m", Some(100), Some(10), None).unwrap();
        provider_add(
            &path,
            "m",
            Protocol::OpenaiChatCompletions,
            "deepseek",
            Some("deepseek-chat".to_string()),
        )
        .unwrap();
        // duplicate rejected before upstream-model resolution
        assert!(
            provider_add(
                &path,
                "m",
                Protocol::OpenaiChatCompletions,
                "deepseek",
                None
            )
            .unwrap_err()
            .to_string()
            .contains("already has provider")
        );
    }

    #[test]
    fn provider_add_copies_upstream_model_from_chain_head_when_omitted() {
        let _state = isolated_state();
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        // Write config with two providers that both expose openai_chat.
        std::fs::write(
            &path,
            r#"[server]
listen = "127.0.0.1:8989"

[providers.deepseek]
api_key_env = "DEEPSEEK_API_KEY"

[providers.deepseek.openai_chat]
url = "https://api.deepseek.com/chat/completions"

[providers.ollama]

[providers.ollama.openai_chat]
url = "http://127.0.0.1:11434/v1/chat/completions"

[models.m]
context_window = 100
max_output_tokens = 10
"#,
        )
        .unwrap();

        // First binding without --upstream-model must error (no existing bindings).
        let err = provider_add(
            &path,
            "m",
            Protocol::OpenaiChatCompletions,
            "deepseek",
            None,
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("--upstream-model is required"),
            "unexpected error: {err}"
        );

        // Add first binding with explicit upstream model.
        provider_add(
            &path,
            "m",
            Protocol::OpenaiChatCompletions,
            "deepseek",
            Some("deepseek-v3".to_string()),
        )
        .unwrap();

        // Second binding without --upstream-model copies from chain head ("deepseek-v3").
        provider_add(&path, "m", Protocol::OpenaiChatCompletions, "ollama", None).unwrap();

        let cfg = Config::load(&path).unwrap();
        let bindings = &cfg.models["m"].openai_chat_providers;
        assert_eq!(bindings.len(), 2);
        assert_eq!(bindings[0].model, "deepseek-v3");
        assert_eq!(bindings[1].name, "ollama");
        assert_eq!(
            bindings[1].model, "deepseek-v3",
            "should copy from chain head"
        );
    }

    #[test]
    fn provider_remove_rejects_missing_binding_and_model() {
        let _state = isolated_state();
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        crate::config::init_config(&path).unwrap();
        assert!(
            provider_remove(
                &path,
                "missing",
                Protocol::OpenaiChatCompletions,
                "deepseek"
            )
            .unwrap_err()
            .to_string()
            .contains("unknown model")
        );
        add(&path, "m", Some(100), Some(10), None).unwrap();
        assert!(
            provider_remove(&path, "m", Protocol::OpenaiChatCompletions, "deepseek")
                .unwrap_err()
                .to_string()
                .contains("has no provider")
        );
    }

    #[test]
    fn provider_move_rejects_zero_index_missing_model_and_provider() {
        let _state = isolated_state();
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        crate::config::init_config(&path).unwrap();
        assert!(
            provider_move(&path, "m", Protocol::OpenaiChatCompletions, "deepseek", 0)
                .unwrap_err()
                .to_string()
                .contains("1-based")
        );
        assert!(
            provider_move(
                &path,
                "missing",
                Protocol::OpenaiChatCompletions,
                "deepseek",
                1
            )
            .unwrap_err()
            .to_string()
            .contains("unknown model")
        );
        add(&path, "m", Some(100), Some(10), None).unwrap();
        assert!(
            provider_move(&path, "m", Protocol::OpenaiChatCompletions, "deepseek", 1)
                .unwrap_err()
                .to_string()
                .contains("has no provider")
        );
    }

    #[test]
    fn provider_move_clamps_large_target_index() {
        let _state = isolated_state();
        let (_temp, path) = write_config(
            r#"[server]
listen = "127.0.0.1:8989"
[providers.a]
[providers.a.openai_chat]
url = "https://a.example/v1/chat/completions"
[providers.b]
[providers.b.openai_chat]
url = "https://b.example/v1/chat/completions"
[models.m]
context_window = 100
max_output_tokens = 10
openai_chat_providers = [{ name = "a", model = "ua" }, { name = "b", model = "ub" }]
"#,
        );
        provider_move(&path, "m", Protocol::OpenaiChatCompletions, "a", 99).unwrap();
        let cfg = Config::load(&path).unwrap();
        assert_eq!(cfg.models["m"].openai_chat_providers[0].name, "b");
        assert_eq!(cfg.models["m"].openai_chat_providers[1].name, "a");
    }

    #[test]
    fn remove_requires_force_and_rejects_unknown_model() {
        let _state = isolated_state();
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        crate::config::init_config(&path).unwrap();
        assert!(
            remove(&path, "deepseek-v4-flash-lp", false)
                .unwrap_err()
                .to_string()
                .contains("--force")
        );
        assert!(
            remove(&path, "missing", true)
                .unwrap_err()
                .to_string()
                .contains("unknown model")
        );
    }

    #[test]
    fn remove_deletes_existing_model_when_forced() {
        let _state = isolated_state();
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        crate::config::init_config(&path).unwrap();
        add(&path, "delete-me", Some(100), Some(10), None).unwrap();
        remove(&path, "delete-me", true).unwrap();
        assert!(
            !Config::load(&path)
                .unwrap()
                .models
                .contains_key("delete-me")
        );
    }

    #[test]
    fn add_from_discovery_rejects_duplicate_unknown_provider_and_missing_cache_row() {
        let _state = isolated_state();
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        crate::config::init_config(&path).unwrap();
        let cache_path = temp.path().join("cache.json");
        crate::status::write_cache(&cache_path, &crate::status::StatusCache::default()).unwrap();
        assert!(
            add_from_discovery_with_cache_path(
                &path,
                &cache_path,
                "deepseek-v4-flash-lp",
                "deepseek",
                None,
                Some(1),
                Some(1)
            )
            .unwrap_err()
            .to_string()
            .contains("already exists")
        );
        assert!(
            add_from_discovery_with_cache_path(
                &path,
                &cache_path,
                "x",
                "missing",
                None,
                Some(1),
                Some(1)
            )
            .unwrap_err()
            .to_string()
            .contains("unknown provider")
        );
        assert!(
            add_from_discovery_with_cache_path(
                &path,
                &cache_path,
                "x",
                "deepseek",
                None,
                Some(1),
                Some(1)
            )
            .unwrap_err()
            .to_string()
            .contains("no discovered model")
        );
    }

    #[test]
    fn add_from_discovery_requires_context_when_cache_lacks_it() {
        let _state = isolated_state();
        let (_temp, path) = write_config(
            "[server]\nlisten='127.0.0.1:8989'\n[providers.p]\n[providers.p.openai_chat]\nurl='https://example.test/v1/chat/completions'\n",
        );
        let temp2 = tempfile::tempdir().unwrap();
        let cache_path = temp2.path().join("cache.json");
        let mut cache = crate::status::StatusCache::default();
        cache.dynamic_models.insert(
            "p".into(),
            vec![crate::status::DynamicModelCacheEntry {
                provider_id: "p".into(),
                source_url: "u".into(),
                probed_at_unix: 1,
                stale_after_unix: 2,
                model_id: "up".into(),
                display_name: None,
                context_window: None,
                max_output_tokens: None,
                features: vec![],
                supported_parameters: vec![],
                supported_reasoning_levels: vec![],
                default_reasoning_level: None,
            }],
        );
        crate::status::write_cache(&cache_path, &cache).unwrap();
        assert!(
            add_from_discovery_with_cache_path(
                &path,
                &cache_path,
                "m",
                "p",
                Some("up"),
                None,
                None
            )
            .unwrap_err()
            .to_string()
            .contains("has no context_window")
        );
    }

    #[test]
    fn add_from_discovery_rejects_provider_without_client_endpoints() {
        let _state = isolated_state();
        let (_temp, path) = write_config(
            "[server]\nlisten='127.0.0.1:8989'\n[providers.p]\n[providers.p.antigravity]\nurl='https://example.test/v1'\n",
        );
        let temp2 = tempfile::tempdir().unwrap();
        let cache_path = temp2.path().join("cache.json");
        let mut cache = crate::status::StatusCache::default();
        cache.dynamic_models.insert(
            "p".into(),
            vec![crate::status::DynamicModelCacheEntry {
                provider_id: "p".into(),
                source_url: "u".into(),
                probed_at_unix: 1,
                stale_after_unix: 2,
                model_id: "up".into(),
                display_name: None,
                context_window: Some(100),
                max_output_tokens: Some(10),
                features: vec![],
                supported_parameters: vec![],
                supported_reasoning_levels: vec![],
                default_reasoning_level: None,
            }],
        );
        crate::status::write_cache(&cache_path, &cache).unwrap();
        assert!(
            add_from_discovery_with_cache_path(
                &path,
                &cache_path,
                "m",
                "p",
                Some("up"),
                None,
                None
            )
            .unwrap_err()
            .to_string()
            .contains("no client-protocol endpoints")
        );
    }

    #[test]
    fn model_edits_preserve_unmanaged_comments() {
        let _state = isolated_state();
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("config.toml");
        std::fs::write(
            &path,
            r#"# root comment
[server]
listen = "127.0.0.1:8989"

# provider comment
[providers.deepseek]
api_key_env = "DEEPSEEK_API_KEY"

[providers.deepseek.openai_chat]
url = "https://api.deepseek.com/chat/completions"

# model comment
[models.existing]
context_window = 1000
max_output_tokens = 100
"#,
        )
        .expect("write");

        provider_add(
            &path,
            "existing",
            Protocol::OpenaiChatCompletions,
            "deepseek",
            Some("deepseek-v4-flash".to_string()),
        )
        .expect("provider add");

        let text = std::fs::read_to_string(&path).expect("read");
        assert!(text.contains("# root comment"));
        assert!(text.contains("# provider comment"));
        assert!(text.contains("# model comment"));
        assert!(text.contains("openai_chat_providers"));
    }

    #[test]
    fn model_add_from_discovery_uses_cached_metadata_and_provider_endpoints() {
        let _state = isolated_state();
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("config.toml");
        std::fs::write(
            &path,
            r#"[server]
listen = "127.0.0.1:8989"

[providers.ollama]

[providers.ollama.openai_chat]
url = "http://127.0.0.1:11434/v1/chat/completions"

[providers.ollama.openai_responses]
derive_from = "openai_chat"

[providers.ollama.anthropic]
derive_from = "openai_chat"
"#,
        )
        .expect("write config");
        let mut cache = crate::status::StatusCache::default();
        cache.dynamic_models.insert(
            "ollama".to_string(),
            vec![crate::status::DynamicModelCacheEntry {
                provider_id: "ollama".to_string(),
                source_url: "http://127.0.0.1:11434/api/tags".to_string(),
                probed_at_unix: 1,
                stale_after_unix: u64::MAX,
                model_id: "qwen3:27b".to_string(),
                display_name: Some("Qwen 3 27B".to_string()),
                context_window: Some(32768),
                max_output_tokens: None,
                features: vec!["tools".to_string(), "image_input".to_string()],
                supported_parameters: Vec::new(),
                supported_reasoning_levels: vec!["low".to_string(), "medium".to_string()],
                default_reasoning_level: Some("medium".to_string()),
            }],
        );
        let cache_path = temp.path().join("status-cache.json");
        crate::status::write_cache(&cache_path, &cache).expect("write cache");

        add_from_discovery_with_cache_path(
            &path,
            &cache_path,
            "qwen3-27b-local",
            "ollama",
            Some("qwen3:27b"),
            None,
            Some(4096),
        )
        .expect("add from discovery");
        let cfg = Config::load(&path).expect("load");
        let model = &cfg.models["qwen3-27b-local"];
        assert_eq!(model.context_window, 32768);
        assert_eq!(model.max_output_tokens, 4096);
        assert!(model.features.contains(&"tools".to_string()));
        assert_eq!(model.supported_reasoning_levels, vec!["low", "medium"]);
        assert_eq!(model.default_reasoning_level.as_deref(), Some("medium"));
        assert_eq!(model.openai_chat_providers[0].name, "ollama");
        assert_eq!(model.openai_chat_providers[0].model, "qwen3:27b");
        assert_eq!(model.openai_responses_providers[0].name, "ollama");
        assert_eq!(model.anthropic_providers[0].name, "ollama");
    }

    #[test]
    fn model_provider_add_remove_and_move_update_bindings() {
        let _state = isolated_state();
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("config.toml");
        crate::config::init_config(&path).expect("init");
        add(&path, "test-model", Some(1000), Some(100), None).expect("add model");

        provider_add(
            &path,
            "test-model",
            Protocol::OpenaiChatCompletions,
            "deepseek",
            Some("deepseek-v4-flash".to_string()),
        )
        .expect("provider add");
        let cfg = Config::load(&path).expect("load");
        assert_eq!(
            cfg.models["test-model"].openai_chat_providers[0].name,
            "deepseek"
        );

        provider_move(
            &path,
            "test-model",
            Protocol::OpenaiChatCompletions,
            "deepseek",
            1,
        )
        .expect("provider move");
        provider_remove(
            &path,
            "test-model",
            Protocol::OpenaiChatCompletions,
            "deepseek",
        )
        .expect("provider remove");
        let cfg = Config::load(&path).expect("reload");
        assert!(cfg.models["test-model"].openai_chat_providers.is_empty());
    }

    #[test]
    fn model_set_updates_parameters_thinking_and_features_atomically() {
        let _state = isolated_state();
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("config.toml");
        crate::config::init_config(&path).expect("init");

        set(
            &path,
            "deepseek-v4-flash-lp",
            SetModelOptions {
                context_window: Some(200_000),
                max_output_tokens: Some(8_192),
                supported_reasoning_levels: Some(vec![
                    "low".to_string(),
                    "medium".to_string(),
                    "high".to_string(),
                ]),
                thinking_level: Some("medium".to_string()),
                enable_thinking: Some(false),
                enable_features: vec!["image_input".to_string(), "tools".to_string()],
                disable_features: vec!["tool_call_reasoning".to_string()],
            },
        )
        .expect("set model");

        let cfg = Config::load(&path).expect("load");
        let model = &cfg.models["deepseek-v4-flash-lp"];
        assert_eq!(model.context_window, 200_000);
        assert_eq!(model.max_output_tokens, 8_192);
        assert_eq!(
            model.supported_reasoning_levels,
            vec!["low", "medium", "high"]
        );
        assert_eq!(model.default_reasoning_level.as_deref(), Some("medium"));
        assert_eq!(model.enable_thinking, Some(false));
        assert!(model.features.contains(&"image_input".to_string()));
        assert!(model.features.contains(&"tools".to_string()));
        assert!(!model.features.contains(&"tool_call_reasoning".to_string()));
    }

    #[test]
    fn model_set_rejects_unknown_feature_and_unsupported_thinking_level() {
        let _state = isolated_state();
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("config.toml");
        crate::config::init_config(&path).expect("init");

        assert!(
            set(
                &path,
                "deepseek-v4-flash-lp",
                SetModelOptions {
                    enable_features: vec!["unknown_feature".to_string()],
                    ..SetModelOptions::default()
                },
            )
            .is_err()
        );
        assert!(
            set(
                &path,
                "deepseek-v4-flash-lp",
                SetModelOptions {
                    thinking_level: Some("unsupported".to_string()),
                    ..SetModelOptions::default()
                },
            )
            .is_err()
        );
    }
}
