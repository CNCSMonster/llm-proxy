use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::config::{Config, ModelConfig, Protocol, ProviderBinding};

/// Write a model config while holding the cross-process file lock, matching
/// the server's `save_config()` behavior so CLI model commands cannot lose
/// updates to a concurrently running server.
fn write_model_locked(path: &Path, cfg: &Config, model_id: &str) -> Result<()> {
    write_model_locked_in(path, cfg, model_id, &model_runtime_dir(path))
}

fn model_runtime_dir(config_path: &Path) -> std::path::PathBuf {
    #[cfg(test)]
    {
        if let Some(path) = tests::runtime_dir() {
            return path;
        }
    }
    let config_dir = config_path.parent().unwrap_or(std::path::Path::new("."));
    crate::service::StateDirs::resolve(config_dir).runtime_dir
}

fn write_model_locked_in(
    path: &Path,
    cfg: &Config,
    model_id: &str,
    runtime_dir: &Path,
) -> Result<()> {
    let _lock = crate::core::ConfigLock::acquire(runtime_dir, Duration::from_secs(5))?;
    crate::config_edit::write_model(path, cfg, model_id)
}

pub fn list(cfg: &Config) {
    for (id, model) in &cfg.models {
        // a routing model serves a protocol when every target does, and
        // its client-visible window is derived from those targets.
        let protocols = Protocol::CLIENT_PROTOCOLS
            .into_iter()
            .filter(|protocol| cfg.model_serves_protocol(id, *protocol))
            .map(|protocol| protocol.route_key())
            .collect::<Vec<_>>()
            .join(",");
        let bounds = cfg.model_capability_bounds(id);
        let (context_window, max_output_tokens) = bounds
            .map(|bounds| (bounds.context_window, bounds.max_output_tokens))
            .unwrap_or((model.context_window, model.max_output_tokens));
        let thinking_tag =
            if model.reasoning_levels.is_none() && model.references_provider("openrouter") {
                " [thinking: client-driven]"
            } else {
                ""
            };
        println!(
            "{} context={} max_output={} protocols={}{}",
            id, context_window, max_output_tokens, protocols, thinking_tag
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
    let bounds = cfg.model_capability_bounds(model_id);
    let (context_window, max_output_tokens) = bounds
        .as_ref()
        .map(|bounds| (bounds.context_window, bounds.max_output_tokens))
        .unwrap_or((model.context_window, model.max_output_tokens));
    let derived_features = bounds
        .map(|bounds| bounds.features.join(","))
        .unwrap_or_default();
    println!("model={model_id}");
    println!("context_window={context_window}");
    println!("max_output_tokens={max_output_tokens}");
    // a routing model has routes instead of provider bindings, and its
    // protocols / features are derived from its targets.
    if model.is_routing_model() {
        println!("routing_model=true");
        let derived_protocols: Vec<_> = Protocol::CLIENT_PROTOCOLS
            .into_iter()
            .filter(|protocol| cfg.model_serves_protocol(model_id, *protocol))
            .map(|protocol| protocol.field_name().to_string())
            .collect();
        println!("protocols={}", derived_protocols.join(","));
        println!("features={derived_features}");
        println!("routes:");
        for (index, entry) in model.routes().iter().enumerate() {
            println!(
                "  {}. level={} target={}",
                index + 1,
                entry.level,
                entry.target
            );
        }
        return Ok(());
    }
    let cooldown_entries =
        crate::cooldown::read_entries(&crate::cooldown::default_state_path()).unwrap_or_default();
    let now = crate::cooldown::unix_secs();
    for protocol in Protocol::CLIENT_PROTOCOLS {
        let bindings = model.provider_bindings(protocol);
        if bindings.is_empty() {
            continue;
        }
        println!("{}:", protocol.field_name());
        for (index, binding) in bindings.iter().enumerate() {
            let cooldown = crate::cooldown::find_binding_cooldown(
                &cooldown_entries,
                model_id,
                &binding.name,
                protocol,
                now,
            )
            .map(|e| crate::cooldown::format_timestamp_local(e.expires_at_unix));
            let badge = crate::cooldown::cooldown_badge(cooldown.as_deref());
            println!(
                "  {}. provider={} upstream_model={} {}",
                index + 1,
                binding.name,
                binding.model,
                badge
            );
        }
    }
    if let Some(ref levels) = model.reasoning_levels {
        let level_strs: Vec<_> = levels.iter().map(|e| e.level()).collect();
        println!("reasoning_levels={}", level_strs.join(","));
    } else if model.references_provider("openrouter") {
        println!("reasoning=[thinking: client-driven]");
    }
    if let Some(ref default) = model.default_reasoning_level {
        println!("default_reasoning_level={default}");
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
                    let cooldown = b.get("cooldown").and_then(serde_json::Value::as_str);
                    let badge = crate::cooldown::cooldown_badge(cooldown);
                    println!("  {idx}. provider={provider} upstream_model={upstream} {badge}");
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
/// Returns the validated window pair, or None in copy-from / route mode
/// (windows are copied from the source model or not needed for routing models).
pub fn validate_add_windows(
    context_window: Option<i64>,
    max_output_tokens: Option<i64>,
    copy_from: Option<&str>,
    has_routes: bool,
) -> Result<Option<(i64, i64)>> {
    if copy_from.is_some() {
        if context_window.is_some() || max_output_tokens.is_some() {
            bail!(
                "--copy-from cannot be combined with --context-window or --max-output; \
                 use 'model set' after adding to adjust limits"
            );
        }
        if has_routes {
            bail!("--copy-from cannot be combined with --route");
        }
        return Ok(None);
    }
    if has_routes {
        if context_window.is_some() || max_output_tokens.is_some() {
            bail!(
                "--route cannot be combined with --context-window or --max-output; \
                 routing models derive these from their targets"
            );
        }
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

/// Parse `--route LEVEL=TARGET` strings into `RouteEntry` values.
/// Rejects malformed entries (no `=`, empty level/target) and duplicate levels.
pub fn parse_route_entries(raw: &[String]) -> Result<Vec<crate::config::RouteEntry>> {
    let mut entries = Vec::with_capacity(raw.len());
    let mut seen_levels = std::collections::HashSet::new();
    for s in raw {
        let (level, target) = s
            .split_once('=')
            .with_context(|| format!("--route {s:?} must be LEVEL=TARGET"))?;
        if level.is_empty() || target.is_empty() {
            bail!("--route {s:?}: level and target must not be empty");
        }
        if !seen_levels.insert(level.to_string()) {
            bail!("--route: duplicate level {level:?}");
        }
        entries.push(crate::config::RouteEntry {
            level: level.to_string(),
            target: target.to_string(),
        });
    }
    Ok(entries)
}

#[derive(Debug, Clone)]
pub(crate) enum ConfigMutation {
    Add {
        model_id: String,
        context_window: Option<i64>,
        max_output_tokens: Option<i64>,
        copy_from: Option<String>,
        routes: Option<Vec<crate::config::RouteEntry>>,
    },
    Set {
        model_id: String,
        options: Box<SetModelOptions>,
    },
    Remove {
        model_id: String,
        force: bool,
    },
    Rename {
        old_model: String,
        new_model: String,
    },
    ProviderAdd {
        model_id: String,
        protocol: Protocol,
        provider_id: String,
        upstream_model: Option<String>,
    },
    ProviderSet {
        model_id: String,
        protocol: Protocol,
        provider_id: String,
        upstream_model: String,
    },
    ProviderRemove {
        model_id: String,
        protocol: Protocol,
        provider_id: String,
    },
    ProviderMove {
        model_id: String,
        protocol: Protocol,
        provider_id: String,
        to_index: usize,
    },
}

pub(crate) fn apply_mutation(
    mut cfg: Config,
    mutation: ConfigMutation,
) -> Result<(Config, String)> {
    let message = match mutation {
        ConfigMutation::Add {
            model_id,
            context_window,
            max_output_tokens,
            copy_from,
            routes,
        } => {
            let has_routes = routes.is_some();
            let windows = validate_add_windows(
                context_window,
                max_output_tokens,
                copy_from.as_deref(),
                has_routes,
            )?;
            if cfg.models.contains_key(&model_id) {
                bail!("model {model_id:?} already exists");
            }
            let model = if let Some(source) = copy_from {
                cfg.models
                    .get(&source)
                    .with_context(|| format!("unknown source model {source:?}"))?
                    .clone()
            } else if has_routes {
                // Routing model: no execution-layer fields needed
                ModelConfig {
                    description: None,
                    context_window: 0,
                    max_output_tokens: 0,
                    features: Vec::new(),
                    reasoning_levels: None,
                    default_reasoning_level: None,
                    supports_thinking: None,
                    force_think: None,
                    force_no_think: None,
                    default_think: None,
                    enable_thinking: None,
                    chat_reasoning_off_format: None,
                    codex_service_tier: None,
                    openai_chat_providers: Vec::new(),
                    openai_responses_providers: Vec::new(),
                    anthropic_providers: Vec::new(),
                    reasoning_level_map: None,
                    supported_reasoning_levels: Vec::new(),
                    cooldown: None,
                    reasoning_routes: routes,
                }
            } else {
                let (context_window, max_output_tokens) = windows.expect("validated above");
                ModelConfig {
                    description: None,
                    context_window,
                    max_output_tokens,
                    features: Vec::new(),
                    reasoning_levels: None,
                    default_reasoning_level: None,
                    supports_thinking: None,
                    force_think: None,
                    force_no_think: None,
                    default_think: None,
                    enable_thinking: None,
                    chat_reasoning_off_format: None,
                    codex_service_tier: None,
                    openai_chat_providers: Vec::new(),
                    openai_responses_providers: Vec::new(),
                    anthropic_providers: Vec::new(),
                    reasoning_level_map: None,
                    supported_reasoning_levels: Vec::new(),
                    cooldown: None,
                    reasoning_routes: None,
                }
            };
            // If --route was provided with --copy-from, override the routes
            // (unreachable: validate_add_windows rejects this combination)
            cfg.models.insert(model_id.clone(), model);
            format!("added model: {model_id}")
        }
        ConfigMutation::Set { model_id, options } => {
            let model = cfg
                .models
                .get_mut(&model_id)
                .with_context(|| format!("unknown model {model_id:?}"))?;
            apply_model_set_fields(model, &model_id, *options)?;
            format!("updated model: {model_id}")
        }
        ConfigMutation::Remove { model_id, force } => {
            if !force {
                bail!(
                    "model remove requires --force until interactive confirmation is implemented"
                );
            }
            let references = cfg.codex_model_references(&model_id);
            if !references.is_empty() {
                bail!(
                    "cannot remove model {model_id:?}; it is referenced by Codex auto-review: {}",
                    references.join(", ")
                );
            }
            // refuse to remove a model that is a routing target; the
            // parent routers would be left with dangling references that fail
            // the next load.
            let routing_references: Vec<String> = cfg
                .models
                .iter()
                .filter(|(_, m)| m.is_routing_model())
                .filter(|(_, m)| m.routes().iter().any(|entry| entry.target == model_id))
                .map(|(id, _)| id.clone())
                .collect();
            if !routing_references.is_empty() {
                bail!(
                    "cannot remove model {model_id:?}; it is referenced as a routing target by: {}",
                    routing_references.join(", ")
                );
            }
            if cfg.models.remove(&model_id).is_none() {
                bail!("unknown model {model_id:?}");
            }
            format!("removed model: {model_id}")
        }
        ConfigMutation::Rename {
            old_model,
            new_model,
        } => {
            rename_model_in_config(&mut cfg, &old_model, &new_model)?;
            format!("renamed model: {old_model} -> {new_model}")
        }
        ConfigMutation::ProviderAdd {
            model_id,
            protocol,
            provider_id,
            upstream_model,
        } => {
            let provider = cfg
                .providers
                .get(&provider_id)
                .with_context(|| format!("unknown provider {provider_id:?}"))?;
            if provider.endpoint(protocol).is_none() {
                bail!(
                    "provider {provider_id:?} does not declare {} endpoint",
                    protocol.field_name()
                );
            }
            reject_cross_product_copy(
                &cfg,
                &model_id,
                protocol,
                &provider_id,
                &provider.product,
                upstream_model.as_deref(),
            )?;
            let model = cfg
                .models
                .get_mut(&model_id)
                .with_context(|| format!("unknown model {model_id:?}"))?;
            let bindings = provider_bindings_mut(model, protocol);
            if bindings.iter().any(|binding| binding.name == provider_id) {
                bail!("model {model_id:?} already has provider {provider_id:?} for {protocol:?}");
            }
            let resolved = upstream_model.or_else(|| bindings.first().map(|binding| binding.model.clone())).with_context(|| format!("no existing binding to copy upstream model from; --upstream-model is required when adding the first provider for {protocol:?}"))?;
            bindings.push(ProviderBinding {
                name: provider_id.clone(),
                model: resolved.clone(),
            });
            format!(
                "added provider {provider_id} (upstream: {resolved}) to model {model_id} {}",
                protocol.field_name()
            )
        }
        ConfigMutation::ProviderSet {
            model_id,
            protocol,
            provider_id,
            upstream_model,
        } => {
            let model = cfg
                .models
                .get_mut(&model_id)
                .with_context(|| format!("unknown model {model_id:?}"))?;
            let bindings = provider_bindings_mut(model, protocol);
            let binding = bindings
                .iter_mut()
                .find(|b| b.name == provider_id)
                .with_context(|| {
                    format!("model {model_id:?} has no provider {provider_id:?} for {protocol:?}")
                })?;
            binding.model = upstream_model.clone();
            format!(
                "updated provider {provider_id} upstream model to {upstream_model} for model {model_id} {}",
                protocol.field_name()
            )
        }
        ConfigMutation::ProviderRemove {
            model_id,
            protocol,
            provider_id,
        } => {
            let model = cfg
                .models
                .get_mut(&model_id)
                .with_context(|| format!("unknown model {model_id:?}"))?;
            let bindings = provider_bindings_mut(model, protocol);
            let before = bindings.len();
            bindings.retain(|binding| binding.name != provider_id);
            if bindings.len() == before {
                bail!("model {model_id:?} has no provider {provider_id:?} for {protocol:?}");
            }
            format!(
                "removed provider {provider_id} from model {model_id} {}",
                protocol.field_name()
            )
        }
        ConfigMutation::ProviderMove {
            model_id,
            protocol,
            provider_id,
            to_index,
        } => {
            if to_index == 0 {
                bail!("--to is 1-based and must be >= 1");
            }
            let model = cfg
                .models
                .get_mut(&model_id)
                .with_context(|| format!("unknown model {model_id:?}"))?;
            let bindings = provider_bindings_mut(model, protocol);
            let from = bindings
                .iter()
                .position(|binding| binding.name == provider_id)
                .with_context(|| format!("model {model_id:?} has no provider {provider_id:?}"))?;
            let binding = bindings.remove(from);
            let to = (to_index - 1).min(bindings.len());
            bindings.insert(to, binding);
            format!("moved provider {provider_id} in model {model_id}")
        }
    };
    cfg.validate()?;
    Ok((cfg, message))
}
pub fn add(
    path: &Path,
    model_id: &str,
    context_window: Option<i64>,
    max_output_tokens: Option<i64>,
    copy_from: Option<&str>,
    routes: Option<Vec<crate::config::RouteEntry>>,
) -> Result<()> {
    // Gate: validate params before any state read/write. This function is the
    // shared entry of every write path (CLI direct, UDS-delegated, direct
    // admin API), so the check must live here, not in the CLI layer.
    let has_routes = routes.is_some();
    let windows = validate_add_windows(context_window, max_output_tokens, copy_from, has_routes)?;
    let mut cfg = Config::load(path)?;
    if cfg.models.contains_key(model_id) {
        bail!("model {model_id:?} already exists");
    }
    let model = if let Some(source) = copy_from {
        cfg.models
            .get(source)
            .with_context(|| format!("unknown source model {source:?}"))?
            .clone()
    } else if has_routes {
        ModelConfig {
            description: None,
            context_window: 0,
            max_output_tokens: 0,
            features: Vec::new(),
            reasoning_levels: None,
            default_reasoning_level: None,
            supports_thinking: None,
            force_think: None,
            force_no_think: None,
            default_think: None,
            enable_thinking: None,
            chat_reasoning_off_format: None,
            codex_service_tier: None,
            openai_chat_providers: Vec::new(),
            openai_responses_providers: Vec::new(),
            anthropic_providers: Vec::new(),
            reasoning_level_map: None,
            supported_reasoning_levels: Vec::new(),
            cooldown: None,
            reasoning_routes: routes,
        }
    } else {
        let (context_window, max_output_tokens) = windows.expect("validated in gate above");
        ModelConfig {
            description: None,
            context_window,
            max_output_tokens,
            features: Vec::new(),
            reasoning_levels: None,
            default_reasoning_level: None,
            supports_thinking: None,
            force_think: None,
            force_no_think: None,
            default_think: None,
            enable_thinking: None,
            chat_reasoning_off_format: None,
            codex_service_tier: None,
            openai_chat_providers: Vec::new(),
            openai_responses_providers: Vec::new(),
            anthropic_providers: Vec::new(),
            reasoning_level_map: None,
            supported_reasoning_levels: Vec::new(),
            cooldown: None,
            reasoning_routes: None,
        }
    };
    cfg.models.insert(model_id.to_string(), model);
    write_model_locked(path, &cfg, model_id)?;
    println!("added model: {model_id}");
    Ok(())
}

/// How a caller wants the reasoning table updated.
#[derive(Debug, Clone)]
pub enum ReasoningLevelsUpdate {
    /// Level names only — what the CLI `--supported-thinking-levels` flag and the TUI
    /// field editor can express. The `api_value` of every level that survives the edit
    /// is preserved; newly added levels are identity.
    Names(Vec<String>),
    /// Full entries (admin API): replace the table verbatim.
    Entries(Vec<crate::config::ReasoningLevelEntry>),
}

impl ReasoningLevelsUpdate {
    /// Stable, explicit shape for the mutation-identity digest. The two variants carry
    /// different information, so they must not collapse to the same value.
    pub fn digest(&self) -> serde_json::Value {
        match self {
            ReasoningLevelsUpdate::Names(levels) => {
                serde_json::json!({"kind": "names", "levels": levels})
            }
            ReasoningLevelsUpdate::Entries(entries) => {
                serde_json::json!({"kind": "entries", "entries": entries})
            }
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct SetModelOptions {
    pub context_window: Option<i64>,
    pub max_output_tokens: Option<i64>,
    pub reasoning_levels: Option<ReasoningLevelsUpdate>,
    pub thinking_level: Option<String>,
    pub supports_thinking: Option<bool>,
    pub force_think: Option<bool>,
    pub force_no_think: Option<bool>,
    pub default_think: Option<bool>,
    pub enable_features: Vec<String>,
    pub disable_features: Vec<String>,
    pub description: Option<String>,
    pub clear_description: bool,
    pub chat_reasoning_off_format: Option<String>,
    pub clear_chat_reasoning_off_format: bool,
    pub codex_service_tier: Option<crate::config::CodexServiceTierConfig>,
    pub clear_codex_service_tier: bool,
    pub cooldown: Option<crate::config::CooldownOverrideConfig>,
    pub clear_cooldown: bool,
}

pub(crate) fn apply_model_set_fields(
    model: &mut ModelConfig,
    model_id: &str,
    options: SetModelOptions,
) -> Result<()> {
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
    if let Some(update) = options.reasoning_levels {
        let table = apply_reasoning_levels_update(model.reasoning_levels.as_deref(), update)?;
        let level_names: Vec<String> = table
            .iter()
            .map(|entry| entry.level().to_string())
            .collect();
        model.reasoning_levels = Some(table);
        if let Some(default) = &model.default_reasoning_level
            && !level_names.is_empty()
            && !level_names.contains(default)
        {
            bail!("existing default_reasoning_level {default:?} is not in new reasoning_levels");
        }
    }
    if let Some(thinking_level) = options.thinking_level {
        let level_names: Vec<&str> = model
            .reasoning_levels
            .as_ref()
            .map(|table| table.iter().map(|e| e.level()).collect())
            .unwrap_or_default();
        if !level_names.is_empty() && !level_names.contains(&thinking_level.as_str()) {
            bail!(
                "thinking level {thinking_level:?} is not supported by model {model_id:?}; supported values: {}",
                level_names.join(", ")
            );
        }
        model.default_reasoning_level = Some(thinking_level);
    }
    if let Some(supports_thinking) = options.supports_thinking {
        model.supports_thinking = Some(supports_thinking);
    }
    if let Some(force_think) = options.force_think {
        model.force_think = Some(force_think);
    }
    if let Some(force_no_think) = options.force_no_think {
        model.force_no_think = Some(force_no_think);
    }
    if let Some(default_think) = options.default_think {
        model.default_think = Some(default_think);
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

    if options.clear_description {
        model.description = None;
    } else if let Some(desc) = options.description {
        model.description = if desc.trim().is_empty() {
            None
        } else {
            Some(desc)
        };
    }

    if options.clear_chat_reasoning_off_format {
        model.chat_reasoning_off_format = None;
    } else if let Some(format) = options.chat_reasoning_off_format {
        if format.trim().is_empty() || format == "none" {
            model.chat_reasoning_off_format = None;
        } else {
            if format != "enable-thinking" {
                bail!(
                    "model {model_id} chat_reasoning_off_format {format:?} is not a known dialect; known values: \"enable-thinking\""
                );
            }
            model.chat_reasoning_off_format = Some(format);
        }
    }

    if options.clear_codex_service_tier {
        model.codex_service_tier = None;
    } else if let Some(tier_cfg) = options.codex_service_tier {
        model.codex_service_tier = Some(tier_cfg);
    }

    if options.clear_cooldown {
        model.cooldown = None;
    } else if let Some(cd) = options.cooldown {
        let existing = model.cooldown.get_or_insert_with(Default::default);
        if cd.network_seconds.is_some() {
            existing.network_seconds = cd.network_seconds;
        }
        if cd.server_error_seconds.is_some() {
            existing.server_error_seconds = cd.server_error_seconds;
        }
        if cd.rate_limit_seconds.is_some() {
            existing.rate_limit_seconds = cd.rate_limit_seconds;
        }
        if cd.model_unavailable_seconds.is_some() {
            existing.model_unavailable_seconds = cd.model_unavailable_seconds;
        }
        if cd.client_error_seconds.is_some() {
            existing.client_error_seconds = cd.client_error_seconds;
        }
        if cd.auth_error_seconds.is_some() {
            existing.auth_error_seconds = cd.auth_error_seconds;
        }
    }

    Ok(())
}

pub fn set(path: &Path, model_id: &str, options: SetModelOptions) -> Result<()> {
    let mut cfg = Config::load(path)?;
    {
        let model = cfg
            .models
            .get_mut(model_id)
            .with_context(|| format!("unknown model {model_id:?}"))?;
        apply_model_set_fields(model, model_id, options)?;
    }
    cfg.validate()?;
    write_model_locked(path, &cfg, model_id)?;
    println!("updated model: {model_id}");
    Ok(())
}

fn validate_feature_name(feature: &str) -> Result<()> {
    match feature {
        "image_input"
        | "audio_input"
        | "video_input"
        | "document_input"
        | "tools"
        | "tool_call_reasoning"
        | "effort_ignored" => Ok(()),
        _ => bail!(
            "unknown model feature {feature:?}; expected image_input, audio_input, video_input, document_input, tools, tool_call_reasoning, or effort_ignored"
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

/// Resolve a requested reasoning-table update into the table to store.
///
/// `Names` keeps the mapping of every level that survives the edit — the CLI/TUI only
/// name levels, so rebuilding from scratch would silently reset mappings to identity.
/// `Entries` is taken verbatim so an admin caller's `api_value` is honoured rather than
/// dropped.
fn apply_reasoning_levels_update(
    previous: Option<&[crate::config::ReasoningLevelEntry]>,
    update: ReasoningLevelsUpdate,
) -> Result<Vec<crate::config::ReasoningLevelEntry>> {
    match update {
        ReasoningLevelsUpdate::Names(levels) => {
            validate_reasoning_levels(&levels)?;
            Ok(levels
                .iter()
                .map(|level| {
                    previous
                        .and_then(|table| table.iter().find(|entry| entry.level() == *level))
                        .cloned()
                        .unwrap_or_else(|| {
                            crate::config::ReasoningLevelEntry::new(level.clone(), None)
                        })
                })
                .collect())
        }
        ReasoningLevelsUpdate::Entries(entries) => Ok(entries),
    }
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
        &{
            let config_dir = path.parent().unwrap_or(std::path::Path::new("."));
            crate::service::StateDirs::resolve(config_dir).state_dir
        }
        .join("status-cache.json"),
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
        reasoning_levels: row.reasoning_levels.clone(),
        default_reasoning_level: row.default_reasoning_level.clone(),
        supports_thinking: None,
        force_think: None,
        force_no_think: None,
        default_think: None,
        enable_thinking: None,
        chat_reasoning_off_format: None,
        codex_service_tier: None,
        openai_chat_providers: Vec::new(),
        openai_responses_providers: Vec::new(),
        anthropic_providers: Vec::new(),
        reasoning_level_map: None,
        supported_reasoning_levels: Vec::new(),
        cooldown: None,
        reasoning_routes: None,
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
    let references = cfg.codex_model_references(model_id);
    if !references.is_empty() {
        bail!(
            "cannot remove model {model_id:?}; it is referenced by Codex auto-review: {}",
            references.join(", ")
        );
    }
    if cfg.models.remove(model_id).is_none() {
        bail!("unknown model {model_id:?}");
    }
    let _lock = crate::core::ConfigLock::acquire(&model_runtime_dir(path), Duration::from_secs(5))?;
    crate::config_edit::remove_model(path, &cfg, model_id)?;
    println!("removed model: {model_id}");
    Ok(())
}

pub fn rename(path: &Path, old_model: &str, new_model: &str) -> Result<()> {
    let mut cfg = Config::load(path)?;
    rename_model_in_config(&mut cfg, old_model, new_model)?;
    cfg.validate()?;
    let _lock = crate::core::ConfigLock::acquire(&model_runtime_dir(path), Duration::from_secs(5))?;
    crate::config_edit::write_full_config(path, &cfg)?;
    println!("renamed model: {old_model} -> {new_model}");
    Ok(())
}

/// Move `[models.OLD]` to `[models.NEW]`, update bindings that still name the
/// old frontend ID, and keep Codex auto-review references consistent.
pub fn rename_model_in_config(cfg: &mut Config, old_model: &str, new_model: &str) -> Result<()> {
    validate_model_id(new_model)?;
    if old_model == new_model {
        bail!("new model {new_model:?} is the same as the current id");
    }
    let model_config = cfg
        .models
        .remove(old_model)
        .ok_or_else(|| anyhow::anyhow!("unknown model {old_model:?}"))?;
    if cfg.models.contains_key(new_model) {
        cfg.models.insert(old_model.to_string(), model_config);
        bail!("model {new_model:?} already exists");
    }
    cfg.models.insert(new_model.to_string(), model_config);

    for model in cfg.models.values_mut() {
        update_binding_model_ids(&mut model.openai_chat_providers, old_model, new_model);
        update_binding_model_ids(&mut model.openai_responses_providers, old_model, new_model);
        update_binding_model_ids(&mut model.anthropic_providers, old_model, new_model);
        // rename targets in routing routes so a renamed leaf does not
        // leave its parent routers with dangling references.
        if let Some(routes) = &mut model.reasoning_routes {
            for entry in routes.iter_mut() {
                if entry.target == old_model {
                    entry.target = new_model.to_string();
                }
            }
        }
    }

    if cfg.codex.auto_review_model.as_deref() == Some(old_model) {
        cfg.codex.auto_review_model = Some(new_model.to_string());
    }
    if let Some(override_config) = cfg.codex.model_overrides.remove(old_model) {
        cfg.codex
            .model_overrides
            .insert(new_model.to_string(), override_config);
    }
    for override_config in cfg.codex.model_overrides.values_mut() {
        if override_config.auto_review_model == old_model {
            override_config.auto_review_model = new_model.to_string();
        }
    }
    Ok(())
}

fn update_binding_model_ids(bindings: &mut [ProviderBinding], old_model: &str, new_model: &str) {
    for binding in bindings.iter_mut() {
        if binding.model == old_model {
            binding.model = new_model.to_string();
        }
    }
}

fn validate_model_id(model_id: &str) -> Result<()> {
    if model_id.is_empty() {
        bail!("model id must not be empty");
    }
    if !model_id
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '.')
    {
        bail!("invalid model id {model_id:?}; expected letters, digits, '-', '_', or '.'");
    }
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
    reject_cross_product_copy(
        &cfg,
        model_id,
        protocol,
        provider_id,
        &provider.product,
        upstream_model.as_deref(),
    )?;
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
        model: resolved_model.clone(),
    });
    write_model_locked(path, &cfg, model_id)?;
    println!(
        "added provider {provider_id} (upstream: {resolved_model}) to model {model_id} {}",
        protocol.field_name()
    );
    Ok(())
}

pub fn provider_set(
    path: &Path,
    model_id: &str,
    protocol: Protocol,
    provider_id: &str,
    upstream_model: &str,
) -> Result<()> {
    let mut cfg = Config::load(path)?;
    let model = cfg
        .models
        .get_mut(model_id)
        .with_context(|| format!("unknown model {model_id:?}"))?;
    let bindings = provider_bindings_mut(model, protocol);
    let binding = bindings
        .iter_mut()
        .find(|b| b.name == provider_id)
        .with_context(|| {
            format!("model {model_id:?} has no provider {provider_id:?} for {protocol:?}")
        })?;
    binding.model = upstream_model.to_string();
    write_model_locked(path, &cfg, model_id)?;
    println!(
        "updated provider {provider_id} upstream model to {upstream_model} for model {model_id} {}",
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

/// Reject `model provider add` when `--upstream-model` is omitted and the
/// target provider belongs to a different product than the first existing
/// binding. Same-product copies are allowed (multi-account sharing); cross-
/// product copies silently produce a binding with the wrong upstream model
/// name .
fn reject_cross_product_copy(
    cfg: &Config,
    model_id: &str,
    protocol: Protocol,
    provider_id: &str,
    target_product: &str,
    upstream_model: Option<&str>,
) -> Result<()> {
    if upstream_model.is_some() {
        return Ok(());
    }
    let model = match cfg.models.get(model_id) {
        Some(m) => m,
        None => return Ok(()),
    };
    let first = match model.provider_bindings(protocol).first() {
        Some(b) => b,
        None => return Ok(()),
    };
    let first_product = match cfg.providers.get(&first.name) {
        Some(p) => &p.product,
        None => return Ok(()),
    };
    if first_product != target_product {
        bail!(
            "provider {provider_id:?} (product={target_product}) belongs to a different \
             product than existing binding {first_name:?} (product={first_product}); \
             --upstream-model is required when adding a cross-product binding",
            first_name = first.name,
        );
    }
    Ok(())
}

fn provider_bindings_mut(model: &mut ModelConfig, protocol: Protocol) -> &mut Vec<ProviderBinding> {
    match protocol {
        Protocol::OpenaiChatCompletions => &mut model.openai_chat_providers,
        Protocol::OpenaiResponses => &mut model.openai_responses_providers,
        Protocol::Anthropic => &mut model.anthropic_providers,
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

    thread_local! {
        static TEST_RUNTIME_DIR: std::cell::RefCell<Option<std::path::PathBuf>> = const { std::cell::RefCell::new(None) };
    }

    struct RuntimeDirGuard {
        previous: Option<std::path::PathBuf>,
        _temp: tempfile::TempDir,
    }

    impl RuntimeDirGuard {
        fn new() -> Self {
            let temp = tempfile::tempdir().expect("runtime tempdir");
            let previous = TEST_RUNTIME_DIR
                .with(|runtime_dir| runtime_dir.borrow_mut().replace(temp.path().to_path_buf()));
            Self {
                previous,
                _temp: temp,
            }
        }
    }

    impl Drop for RuntimeDirGuard {
        fn drop(&mut self) {
            TEST_RUNTIME_DIR.with(|runtime_dir| {
                runtime_dir.borrow_mut().clone_from(&self.previous);
            });
        }
    }

    pub(super) fn runtime_dir() -> Option<std::path::PathBuf> {
        TEST_RUNTIME_DIR.with(|runtime_dir| runtime_dir.borrow().clone())
    }

    fn isolated_state() -> RuntimeDirGuard {
        RuntimeDirGuard::new()
    }

    fn write_config(text: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("config.toml");
        std::fs::write(&path, text).expect("write config");
        (temp, path)
    }

    #[test]
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
        let err = parse_client_protocol("some-unknown-protocol")
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
    fn print_model_info_json_renders_cooldown_badge() {
        let payload = serde_json::json!({
            "data": {
                "id": "m",
                "providers": {
                    "openai_chat": [
                        {
                            "index": 1,
                            "provider": "p1",
                            "upstream_model": "u1",
                            "cooldown": "2026-10-01 12:00"
                        },
                        {
                            "index": 2,
                            "provider": "p2",
                            "upstream_model": "u2",
                            "cooldown": null
                        }
                    ]
                }
            }
        });
        assert!(print_model_info_json(&payload).is_ok());
    }

    #[test]
    fn info_rejects_unknown_model() {
        let (_temp, path) = write_config("[server]\nlisten='127.0.0.1:8989'\n");
        let cfg = Config::load(&path).unwrap();
        let err = info(&cfg, "missing").unwrap_err().to_string();
        assert!(err.contains("unknown model"));
    }

    #[test]
    fn rename_moves_model_and_updates_referencing_bindings() {
        let _state = isolated_state();
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        crate::config::init_config(&path).unwrap();
        add(&path, "my-model", Some(100), Some(10), None, None).unwrap();
        add(&path, "referencing-model", Some(100), Some(10), None, None).unwrap();
        {
            let mut cfg = Config::load(&path).unwrap();
            cfg.models
                .get_mut("referencing-model")
                .unwrap()
                .openai_chat_providers
                .push(ProviderBinding {
                    name: "deepseek".into(),
                    model: "my-model".into(),
                });
            // auto_review_model must expose Responses or Config::validate rejects it.
            cfg.models
                .get_mut("my-model")
                .unwrap()
                .openai_responses_providers
                .push(ProviderBinding {
                    name: "deepseek".into(),
                    model: "deepseek-v4-flash".into(),
                });
            cfg.codex.auto_review_model = Some("my-model".into());
            // Override lives on a different coding model so the review target
            // itself is not nested (validate rejects that).
            cfg.codex.model_overrides.insert(
                "deepseek-v4-flash-lp".into(),
                crate::config::CodexModelOverride {
                    auto_review_model: "my-model".into(),
                },
            );
            crate::config_edit::write_full_config(&path, &cfg).unwrap();
        }

        rename(&path, "my-model", "renamed-model").unwrap();
        let cfg = Config::load(&path).unwrap();
        assert!(!cfg.models.contains_key("my-model"));
        assert!(cfg.models.contains_key("renamed-model"));
        assert_eq!(
            cfg.models["referencing-model"].openai_chat_providers[0].model,
            "renamed-model"
        );
        assert_eq!(
            cfg.codex.auto_review_model.as_deref(),
            Some("renamed-model")
        );
        assert_eq!(
            cfg.codex.model_overrides["deepseek-v4-flash-lp"].auto_review_model,
            "renamed-model"
        );
    }

    /// renaming a routing target must rewrite the parent hop's route
    /// entry, or the next load fails on a dangling target reference.
    #[test]
    fn rename_updates_routing_route_targets() {
        let mut cfg = crate::config::default_deepseek_config();
        let mut router = routing_hop_from(&cfg, "deepseek-v4-flash-lp");
        router.reasoning_routes = Some(vec![crate::config::RouteEntry {
            level: "low".into(),
            target: "deepseek-v4-flash-lp".into(),
        }]);
        cfg.models.insert("router-lp".into(), router);

        rename_model_in_config(&mut cfg, "deepseek-v4-flash-lp", "flash-renamed").unwrap();

        let routes = cfg.models["router-lp"].reasoning_routes.as_ref().unwrap();
        assert_eq!(
            routes[0].target, "flash-renamed",
            "the hop's route must follow the renamed target"
        );
        cfg.validate()
            .expect("renaming a routing target must not leave a dangling route");
    }

    /// removing a model that is a routing target must be refused —
    /// the parent hop would be left dangling and fail the next load.
    #[test]
    fn remove_refuses_a_model_that_is_a_routing_target() {
        let mut cfg = crate::config::default_deepseek_config();
        let mut router = routing_hop_from(&cfg, "deepseek-v4-flash-lp");
        router.reasoning_routes = Some(vec![crate::config::RouteEntry {
            level: "low".into(),
            target: "deepseek-v4-pro-lp".into(),
        }]);
        cfg.models.insert("router-lp".into(), router);

        let err = apply_mutation(
            cfg,
            ConfigMutation::Remove {
                model_id: "deepseek-v4-pro-lp".into(),
                force: true,
            },
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("routing target"),
            "a routing target must not be removable: {err}"
        );
        assert!(
            err.contains("router-lp"),
            "the error must name the parent hop that references it: {err}"
        );
    }

    /// A copy of `base` stripped of every execution-layer field and provider
    /// binding, i.e. a valid routing hop (bindings/windows) awaiting a
    /// `reasoning_routes` table.
    fn routing_hop_from(cfg: &Config, base: &str) -> crate::config::ModelConfig {
        let mut hop = cfg.models[base].clone();
        hop.openai_chat_providers.clear();
        hop.openai_responses_providers.clear();
        hop.anthropic_providers.clear();
        hop.reasoning_levels = None;
        hop.default_reasoning_level = None;
        hop.features.clear();
        hop.cooldown = None;
        hop.supports_thinking = None;
        hop.force_think = None;
        hop.force_no_think = None;
        hop.default_think = None;
        hop.enable_thinking = None;
        hop.chat_reasoning_off_format = None;
        hop.codex_service_tier = None;
        hop.context_window = 0;
        hop.max_output_tokens = 0;
        hop
    }

    #[test]
    fn rename_rejects_unknown_existing_and_invalid_ids() {
        let _state = isolated_state();
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        crate::config::init_config(&path).unwrap();
        add(&path, "my-model", Some(100), Some(10), None, None).unwrap();
        add(&path, "other-model", Some(100), Some(10), None, None).unwrap();

        assert!(
            rename(&path, "missing", "new-model")
                .unwrap_err()
                .to_string()
                .contains("unknown model")
        );
        assert!(
            rename(&path, "my-model", "other-model")
                .unwrap_err()
                .to_string()
                .contains("already exists")
        );
        assert!(
            rename(&path, "my-model", "bad id")
                .unwrap_err()
                .to_string()
                .contains("invalid model id")
        );
        assert!(Config::load(&path).unwrap().models.contains_key("my-model"));
    }

    #[test]
    fn add_rejects_duplicate_model_and_unknown_copy_source() {
        let _state = isolated_state();
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        crate::config::init_config(&path).unwrap();
        assert!(
            add(&path, "deepseek-v4-flash-lp", Some(1), Some(1), None, None)
                .unwrap_err()
                .to_string()
                .contains("already exists")
        );
        assert!(
            add(&path, "copy", None, None, Some("missing"), None)
                .unwrap_err()
                .to_string()
                .contains("unknown source model")
        );
        assert!(
            add(
                &path,
                "copy-with-limits",
                Some(1),
                None,
                Some("deepseek-v4-flash-lp"),
                None
            )
            .unwrap_err()
            .to_string()
            .contains("--copy-from cannot be combined with --context-window or --max-output")
        );
    }

    #[test]
    fn validate_add_windows_shared_by_cli_and_gate() {
        // 缺窗口 → requires 报错
        let err = validate_add_windows(None, None, None, false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("requires --context-window"));
        let err = validate_add_windows(Some(10), None, None, false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("requires --max-output"));
        // 显式 0 → must be > 0
        let err = validate_add_windows(Some(0), Some(10), None, false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("--context-window must be > 0"));
        // copy-from 模式跳过，返回 None
        assert_eq!(
            validate_add_windows(None, None, Some("src"), false).unwrap(),
            None
        );
        // copy-from 模式与任何窗口参数共存必须报错（防静默丢弃）
        let err = validate_add_windows(Some(100), None, Some("src"), false)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("--copy-from cannot be combined with --context-window or --max-output")
        );
        let err = validate_add_windows(None, Some(50), Some("src"), false)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("--copy-from cannot be combined with --context-window or --max-output")
        );
        let err = validate_add_windows(Some(100), Some(50), Some("src"), false)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("--copy-from cannot be combined with --context-window or --max-output")
        );
        // 合法值返回校验后的窗口对
        assert_eq!(
            validate_add_windows(Some(100), Some(20), None, false).unwrap(),
            Some((100, 20))
        );
        // route 模式跳过窗口要求，返回 None
        assert_eq!(validate_add_windows(None, None, None, true).unwrap(), None);
        // route 模式与窗口参数共存必须报错
        let err = validate_add_windows(Some(100), None, None, true)
            .unwrap_err()
            .to_string();
        assert!(err.contains("--route cannot be combined with --context-window"));
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
        let err = add(&path, "m", None, None, None, None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("requires --context-window"));
        let err = add(&path, "m", Some(10), None, None, None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("requires --max-output"));
        // 显式提供 0 → must be > 0 报错（与"未提供"区分）
        let err = add(&path, "m", Some(0), Some(10), None, None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("--context-window must be > 0"));
        let err = add(&path, "m", Some(10), Some(0), None, None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("--max-output must be > 0"));
        // copy-from 模式跳过窗口 gate（不报 requires，报 unknown source）
        let err = add(&path, "m", None, None, Some("missing"), None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown source model"));
    }

    #[test]
    fn parse_route_entries_parses_valid_entries() {
        let raw = vec!["low=target-low".into(), "high=target-high".into()];
        let entries = parse_route_entries(&raw).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].level, "low");
        assert_eq!(entries[0].target, "target-low");
        assert_eq!(entries[1].level, "high");
        assert_eq!(entries[1].target, "target-high");
    }

    #[test]
    fn parse_route_entries_rejects_missing_equals() {
        let raw = vec!["no-equals-sign".into()];
        let err = parse_route_entries(&raw).unwrap_err().to_string();
        assert!(err.contains("must be LEVEL=TARGET"));
    }

    #[test]
    fn parse_route_entries_rejects_empty_level_or_target() {
        let raw = vec!["=target".into()];
        assert!(
            parse_route_entries(&raw)
                .unwrap_err()
                .to_string()
                .contains("must not be empty")
        );
        let raw = vec!["level=".into()];
        assert!(
            parse_route_entries(&raw)
                .unwrap_err()
                .to_string()
                .contains("must not be empty")
        );
    }

    #[test]
    fn parse_route_entries_rejects_duplicate_levels() {
        let raw = vec!["low=a".into(), "low=b".into()];
        let err = parse_route_entries(&raw).unwrap_err().to_string();
        assert!(err.contains("duplicate level"));
    }

    #[test]
    fn add_routing_model_with_routes() {
        let _state = isolated_state();
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        crate::config::init_config(&path).unwrap();
        // Add target models first
        add(&path, "target-low", Some(1000), Some(100), None, None).unwrap();
        add(&path, "target-high", Some(1000), Some(100), None, None).unwrap();
        // Add routing model
        let routes = vec![
            crate::config::RouteEntry {
                level: "low".into(),
                target: "target-low".into(),
            },
            crate::config::RouteEntry {
                level: "high".into(),
                target: "target-high".into(),
            },
        ];
        add(&path, "router", None, None, None, Some(routes)).unwrap();
        let cfg = Config::load(&path).unwrap();
        assert!(cfg.models["router"].is_routing_model());
        assert_eq!(cfg.models["router"].routes().len(), 2);
        assert_eq!(
            cfg.models["router"].resolve_route("low"),
            Some("target-low")
        );
        assert_eq!(
            cfg.models["router"].resolve_route("high"),
            Some("target-high")
        );
    }

    #[test]
    fn add_routing_model_rejects_routes_with_context_window() {
        let _state = isolated_state();
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        crate::config::init_config(&path).unwrap();
        let routes = vec![crate::config::RouteEntry {
            level: "low".into(),
            target: "t".into(),
        }];
        let err = add(&path, "router", Some(100), None, None, Some(routes))
            .unwrap_err()
            .to_string();
        assert!(err.contains("--route cannot be combined with --context-window"));
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
                reasoning_levels: None,
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
            None,
            None,
            Some("deepseek-v4-flash-lp"),
            None,
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
                    reasoning_levels: Some(ReasoningLevelsUpdate::Names(vec![
                        "low".into(),
                        "low".into()
                    ])),
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
                    reasoning_levels: Some(ReasoningLevelsUpdate::Names(vec![" ".into()])),
                    ..Default::default()
                }
            )
            .unwrap_err()
            .to_string()
            .contains("empty")
        );
    }

    #[test]
    fn set_preserves_existing_api_value_for_surviving_levels() {
        let _state = isolated_state();
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        crate::config::init_config(&path).unwrap();
        let entry = |level: &str, api_value: Option<&str>| {
            crate::config::ReasoningLevelEntry::new(
                level.to_string(),
                api_value.map(ToString::to_string),
            )
        };
        set(
            &path,
            "deepseek-v4-flash-lp",
            SetModelOptions {
                reasoning_levels: Some(ReasoningLevelsUpdate::Entries(vec![
                    entry("low", Some("lo")),
                    entry("high", Some("hi")),
                    entry("xhigh", Some("max")),
                ])),
                ..Default::default()
            },
        )
        .unwrap();

        // The CLI/TUI can only name levels; an edit must not reset surviving mappings.
        set(
            &path,
            "deepseek-v4-flash-lp",
            SetModelOptions {
                reasoning_levels: Some(ReasoningLevelsUpdate::Names(vec![
                    "low".into(),
                    "high".into(),
                ])),
                ..Default::default()
            },
        )
        .unwrap();

        let cfg: crate::config::Config =
            toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let table = cfg.models["deepseek-v4-flash-lp"]
            .reasoning_levels
            .as_ref()
            .unwrap();
        let pairs: Vec<(&str, Option<&str>)> =
            table.iter().map(|e| (e.level(), e.api_value())).collect();
        assert_eq!(pairs, vec![("low", Some("lo")), ("high", Some("hi"))]);
    }

    #[test]
    fn set_honours_full_entries_from_the_admin_api() {
        let _state = isolated_state();
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        crate::config::init_config(&path).unwrap();
        set(
            &path,
            "deepseek-v4-flash-lp",
            SetModelOptions {
                reasoning_levels: Some(ReasoningLevelsUpdate::Entries(vec![
                    crate::config::ReasoningLevelEntry::new("low", Some("high".to_string())),
                    crate::config::ReasoningLevelEntry::new("high", None),
                ])),
                ..Default::default()
            },
        )
        .unwrap();

        let cfg: crate::config::Config =
            toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let entry = &cfg.models["deepseek-v4-flash-lp"]
            .reasoning_levels
            .as_ref()
            .unwrap()[0];
        assert_eq!(entry.level(), "low");
        assert_eq!(
            entry.api_value(),
            Some("high"),
            "an admin-supplied api_value must not be silently dropped"
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
                reasoning_levels: Some(ReasoningLevelsUpdate::Names(vec![
                    "low".into(),
                    "medium".into(),
                    "high".into(),
                ])),
                thinking_level: Some("medium".into()),
                ..Default::default()
            },
        )
        .unwrap();
        let err = set(
            &path,
            "deepseek-v4-flash-lp",
            SetModelOptions {
                reasoning_levels: Some(ReasoningLevelsUpdate::Names(vec!["low".into()])),
                ..Default::default()
            },
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("existing default_reasoning_level"));
    }

    #[test]
    fn apply_mutation_set_applies_thinking_strategy_fields() {
        let _state = isolated_state();
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        crate::config::init_config(&path).unwrap();
        let cfg = Config::load(&path).unwrap();
        let (cfg, _) = apply_mutation(
            cfg,
            ConfigMutation::Set {
                model_id: "deepseek-v4-flash-lp".to_string(),
                options: Box::new(SetModelOptions {
                    supports_thinking: Some(true),
                    force_think: Some(false),
                    force_no_think: Some(false),
                    default_think: Some(true),
                    ..Default::default()
                }),
            },
        )
        .unwrap();
        let model = &cfg.models["deepseek-v4-flash-lp"];
        assert_eq!(model.supports_thinking, Some(true));
        assert_eq!(model.force_think, Some(false));
        assert_eq!(model.force_no_think, Some(false));
        assert_eq!(model.default_think, Some(true));
    }

    #[test]
    fn set_feature_toggles_are_idempotent_and_sorted() {
        let _state = isolated_state();
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        crate::config::init_config(&path).unwrap();
        add(&path, "feature-model", Some(100), Some(10), None, None).unwrap();
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
        add(&path, "m", Some(100), Some(10), None, None).unwrap();
        assert!(
            provider_add(&path, "m", Protocol::OpenaiChatCompletions, "missing", None)
                .unwrap_err()
                .to_string()
                .contains("unknown provider")
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
        add(&path, "m", Some(100), Some(10), None, None).unwrap();
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
    fn provider_add_cross_product_without_upstream_model_rejected() {
        let _state = isolated_state();
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        std::fs::write(
            &path,
            r#"[server]
listen = "127.0.0.1:8989"

[providers.demo]
product = "demo"
api_key_env = "DEMO_API_KEY"

[providers.demo.openai_chat]
url = "https://demo.example.com/v1/chat/completions"

[providers.deepseek]
product = "deepseek"
api_key_env = "DEEPSEEK_API_KEY"

[providers.deepseek.openai_chat]
url = "https://api.deepseek.com/chat/completions"

[models.m]
context_window = 100
max_output_tokens = 10
openai_chat_providers = [{ name = "demo", model = "gemini-3.7-flash-low" }]
"#,
        )
        .unwrap();

        // Cross-product (demo → deepseek) without --upstream-model must error.
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
            err.contains("different product") && err.contains("--upstream-model is required"),
            "unexpected error: {err}"
        );

        // Same call WITH --upstream-model must succeed.
        provider_add(
            &path,
            "m",
            Protocol::OpenaiChatCompletions,
            "deepseek",
            Some("deepseek-v3".to_string()),
        )
        .unwrap();
    }

    #[test]
    fn provider_add_same_product_without_upstream_model_copies() {
        let _state = isolated_state();
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("config.toml");
        std::fs::write(
            &path,
            r#"[server]
listen = "127.0.0.1:8989"

[providers.demo-a]
product = "demo"
api_key_env = "DEMO_API_KEY_A"

[providers.demo-a.openai_chat]
url = "https://demo-a.example.com/v1/chat/completions"

[providers.demo-b]
product = "demo"
api_key_env = "DEMO_API_KEY_B"

[providers.demo-b.openai_chat]
url = "https://demo-b.example.com/v1/chat/completions"

[models.m]
context_window = 100
max_output_tokens = 10
openai_chat_providers = [{ name = "demo-a", model = "gemini-3.7-flash-low" }]
"#,
        )
        .unwrap();

        // Same-product (demo-a → demo-b) without --upstream-model copies successfully.
        provider_add(&path, "m", Protocol::OpenaiChatCompletions, "demo-b", None).unwrap();

        let cfg = Config::load(&path).unwrap();
        let bindings = &cfg.models["m"].openai_chat_providers;
        assert_eq!(bindings.len(), 2);
        assert_eq!(bindings[1].name, "demo-b");
        assert_eq!(bindings[1].model, "gemini-3.7-flash-low");
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
        add(&path, "m", Some(100), Some(10), None, None).unwrap();
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
        add(&path, "m", Some(100), Some(10), None, None).unwrap();
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
        add(&path, "delete-me", Some(100), Some(10), None, None).unwrap();
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
                reasoning_levels: None,
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
        let (_temp, path) = write_config("[server]\nlisten='127.0.0.1:8989'\n[providers.p]\n");
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
                reasoning_levels: None,
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
            .contains("at least one protocol endpoint")
        );
    }

    #[test]
    fn model_edits_canonical_output_strips_comments() {
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
        // canonical formatter strips hand-written comments
        assert!(!text.contains("# root comment"));
        assert!(!text.contains("# provider comment"));
        assert!(!text.contains("# model comment"));
        // Semantic correctness: data survives
        assert!(text.contains("openai_chat_providers"));
        assert!(text.contains("[providers.deepseek]"));
        assert!(text.contains("[models.existing]"));
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
                reasoning_levels: Some(vec![
                    crate::config::ReasoningLevelEntry::new("low", None),
                    crate::config::ReasoningLevelEntry::new("medium", None),
                ]),
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
        let rl = model
            .reasoning_levels
            .as_ref()
            .expect("reasoning_levels table");
        assert_eq!(rl.len(), 2);
        assert_eq!(rl[0].level(), "low");
        assert_eq!(rl[1].level(), "medium");
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
        add(&path, "test-model", Some(1000), Some(100), None, None).expect("add model");

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
    fn model_provider_set_updates_upstream_model_in_place_preserving_order() {
        let _state = isolated_state();
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("config.toml");
        crate::config::init_config(&path).expect("init");

        // 写入包含 3 个 provider 的测试配置
        std::fs::write(
            &path,
            r#"[server]
listen = "127.0.0.1:8989"

[providers.p1]
[providers.p1.openai_chat]
url = "https://api.p1.com/v1/chat/completions"

[providers.p2]
[providers.p2.openai_chat]
url = "https://api.p2.com/v1/chat/completions"

[providers.p3]
[providers.p3.openai_chat]
url = "https://api.p3.com/v1/chat/completions"

[models.test-model]
context_window = 1000
max_output_tokens = 100
openai_chat_providers = [
  { name = "p1", model = "up-1" },
  { name = "p2", model = "up-2" },
  { name = "p3", model = "up-3" }
]
"#,
        )
        .expect("write config");

        // 原地更新首个 provider p1 的 upstream-model
        provider_set(
            &path,
            "test-model",
            Protocol::OpenaiChatCompletions,
            "p1",
            "up-1-modified",
        )
        .expect("provider set p1");

        let cfg = Config::load(&path).expect("reload config");
        let bindings = &cfg.models["test-model"].openai_chat_providers;
        // 关键断言：顺序完全不变（p1 仍然是 index 0，而不是被挪到末位），且 model 已更新
        assert_eq!(bindings.len(), 3);
        assert_eq!(bindings[0].name, "p1");
        assert_eq!(bindings[0].model, "up-1-modified");
        assert_eq!(bindings[1].name, "p2");
        assert_eq!(bindings[1].model, "up-2");
        assert_eq!(bindings[2].name, "p3");
        assert_eq!(bindings[2].model, "up-3");

        // 原地更新中间 provider p2 的 upstream-model
        provider_set(
            &path,
            "test-model",
            Protocol::OpenaiChatCompletions,
            "p2",
            "up-2-modified",
        )
        .expect("provider set p2");

        let cfg = Config::load(&path).expect("reload config");
        let bindings = &cfg.models["test-model"].openai_chat_providers;
        assert_eq!(bindings[1].name, "p2");
        assert_eq!(bindings[1].model, "up-2-modified");

        // 对不存在的 provider 更新应当报错
        let err = provider_set(
            &path,
            "test-model",
            Protocol::OpenaiChatCompletions,
            "p-nonexistent",
            "foo",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("has no provider \"p-nonexistent\""));
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
                reasoning_levels: Some(ReasoningLevelsUpdate::Names(vec![
                    "low".to_string(),
                    "medium".to_string(),
                    "high".to_string(),
                ])),
                thinking_level: Some("medium".to_string()),
                supports_thinking: Some(true),
                force_think: None,
                force_no_think: None,
                default_think: None,
                enable_features: vec![
                    "image_input".to_string(),
                    "audio_input".to_string(),
                    "video_input".to_string(),
                    "tools".to_string(),
                ],
                disable_features: vec!["tool_call_reasoning".to_string()],
                ..Default::default()
            },
        )
        .expect("set model");

        let cfg = Config::load(&path).expect("load");
        let model = &cfg.models["deepseek-v4-flash-lp"];
        assert_eq!(model.context_window, 200_000);
        assert_eq!(model.max_output_tokens, 8_192);
        let rl = model
            .reasoning_levels
            .as_ref()
            .expect("reasoning_levels table");
        let level_names: Vec<&str> = rl.iter().map(|e| e.level()).collect();
        assert_eq!(level_names, vec!["low", "medium", "high"]);
        assert_eq!(model.default_reasoning_level.as_deref(), Some("medium"));
        assert_eq!(model.supports_thinking, Some(true));
        assert!(model.features.contains(&"image_input".to_string()));
        assert!(model.features.contains(&"audio_input".to_string()));
        assert!(model.features.contains(&"video_input".to_string()));
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

    #[test]
    fn model_set_handles_description_format_tier_and_cooldown() {
        let _state = isolated_state();
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("config.toml");
        crate::config::init_config(&path).expect("init");

        // Enable native openai_responses endpoint on deepseek so codex_service_tier validates
        let mut cfg = Config::load(&path).expect("load");
        cfg.providers.get_mut("deepseek").unwrap().openai_responses = Some(
            crate::config::EndpointConfig::native("https://example.test/responses"),
        );
        std::fs::write(&path, toml::to_string_pretty(&cfg).expect("serialize")).expect("write");

        // 1. Set all 4 new fields
        set(
            &path,
            "deepseek-v4-flash-lp",
            SetModelOptions {
                description: Some("Custom model description".to_string()),
                chat_reasoning_off_format: Some("enable-thinking".to_string()),
                codex_service_tier: Some(crate::config::CodexServiceTierConfig {
                    tiers: vec!["priority".to_string()],
                    default_tier: Some("priority".to_string()),
                    forced_tier: None,
                }),
                cooldown: Some(crate::config::CooldownOverrideConfig {
                    network_seconds: Some(15),
                    server_error_seconds: Some(60),
                    rate_limit_seconds: Some(120),
                    model_unavailable_seconds: Some(30),
                    client_error_seconds: Some(5),
                    auth_error_seconds: Some(300),
                }),
                ..SetModelOptions::default()
            },
        )
        .expect("set model new fields");

        let cfg = Config::load(&path).expect("load");
        let model = &cfg.models["deepseek-v4-flash-lp"];
        assert_eq!(
            model.description.as_deref(),
            Some("Custom model description")
        );
        assert_eq!(
            model.chat_reasoning_off_format.as_deref(),
            Some("enable-thinking")
        );
        let tier = model.codex_service_tier.as_ref().expect("service tier");
        assert_eq!(tier.tiers, vec!["priority".to_string()]);
        assert_eq!(tier.default_tier.as_deref(), Some("priority"));
        assert_eq!(tier.forced_tier, None);
        let cd = model.cooldown.as_ref().expect("cooldown");
        assert_eq!(cd.network_seconds, Some(15));
        assert_eq!(cd.server_error_seconds, Some(60));
        assert_eq!(cd.rate_limit_seconds, Some(120));
        assert_eq!(cd.model_unavailable_seconds, Some(30));
        assert_eq!(cd.client_error_seconds, Some(5));
        assert_eq!(cd.auth_error_seconds, Some(300));

        // 2. Validate unknown chat_reasoning_off_format rejected
        let err = set(
            &path,
            "deepseek-v4-flash-lp",
            SetModelOptions {
                chat_reasoning_off_format: Some("unsupported-dialect".to_string()),
                ..SetModelOptions::default()
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("is not a known dialect"));

        // 3. Clear all 4 fields
        set(
            &path,
            "deepseek-v4-flash-lp",
            SetModelOptions {
                clear_description: true,
                clear_chat_reasoning_off_format: true,
                clear_codex_service_tier: true,
                clear_cooldown: true,
                ..SetModelOptions::default()
            },
        )
        .expect("clear model new fields");

        let cfg2 = Config::load(&path).expect("load");
        let model2 = &cfg2.models["deepseek-v4-flash-lp"];
        assert_eq!(model2.description, None);
        assert_eq!(model2.chat_reasoning_off_format, None);
        assert_eq!(model2.codex_service_tier, None);
        assert_eq!(model2.cooldown, None);
    }

    /// 字段写入路径不变式（ADR-031）:
    /// Verifies that EVERY field in `ModelConfig` is either:
    /// 1. Handled in `SetModelOptions` (and CLI `model set` flags), OR
    /// 2. Explicitly exempted with documented rationale.
    #[test]
    fn model_config_field_coverage_guard() {
        let probe = serde_json::json!({ "__probe_unknown_field__": 1 });
        let err = serde_json::from_value::<crate::config::ModelConfig>(probe)
            .unwrap_err()
            .to_string();

        // Serde with deny_unknown_fields produces:
        // "unknown field `__probe_unknown_field__`, expected one of `field1`, `field2`, ..."
        let prefix = "expected one of ";
        let pos = err
            .find(prefix)
            .expect("serde error contains expected fields list");
        let expected_part = &err[pos + prefix.len()..];
        let fields: Vec<String> = expected_part
            .split(',')
            .map(|s| s.trim().trim_matches('`').to_string())
            .filter(|s| !s.is_empty())
            .collect();

        assert!(
            !fields.is_empty(),
            "must extract fields from serde error: {err}"
        );

        // Explicit exemptions with documented rationale:
        let exemptions: std::collections::HashMap<&str, &str> = [
            ("enable_thinking", "Retired presence-rejection sentinel; intentionally non-writable"),
            ("reasoning_level_map", "Retired presence-rejection sentinel; intentionally non-writable"),
            ("supported_reasoning_levels", "Retired presence-rejection sentinel; intentionally non-writable"),
            ("reasoning_routes", "Routing model routes"),
            ("openai_chat_providers", "Provider binding chain, managed via `model provider {add, remove, move}` (and upstream-model)"),
            ("openai_responses_providers", "Provider binding chain, managed via `model provider {add, remove, move}`"),
            ("anthropic_providers", "Provider binding chain, managed via `model provider {add, remove, move}`"),
        ].into_iter().collect();

        // Fields covered by `SetModelOptions` (and CLI flags):
        let handled_fields: std::collections::HashSet<&str> = [
            "description",
            "context_window",
            "max_output_tokens",
            "features",
            "reasoning_levels",
            "default_reasoning_level",
            "supports_thinking",
            "force_think",
            "force_no_think",
            "default_think",
            "chat_reasoning_off_format",
            "codex_service_tier",
            "cooldown",
        ]
        .into_iter()
        .collect();

        for field in &fields {
            let is_handled = handled_fields.contains(field.as_str());
            let is_exempt = exemptions.contains_key(field.as_str());
            assert!(
                is_handled || is_exempt,
                "ModelConfig field `{field}` has no CLI/SetModelOptions write path and is not in the explicit exemption list!"
            );
        }

        // Verify that exemption list does not have stale entries
        for exempt_field in exemptions.keys() {
            assert!(
                fields.iter().any(|f| f == exempt_field),
                "Exemption `{exempt_field}` does not exist in ModelConfig anymore"
            );
        }
    }
}
