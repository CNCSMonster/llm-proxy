//! OpenRouter 动态模型目录发现、本地缓存与 Connect 搜索选择
//!
//! 核心设计原则（参见 SPEC openrouter-dynamic-connect-and-search.md）：
//! 1. 严格 OpenRouter 专属（OpenRouter-only），不引入泛化 Provider Trait；
//! 2. 宁缺毋假原则：context_window <= 0 跳过，max_output_tokens 缺失置 None，思考等级绝不脑补虚构；
//! 3. 统一时钟源注入（全链路使用显式传入的 now: i64）；
//! 4. 纯本地配置写入（apply_dynamic_models_to_config 零网络探活，类型系统静态保证）；
//! 5. 跨会话幂等与 Provider 隔离消歧。

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::config::{Config, ModelConfig, Protocol, ProviderBinding, ReasoningLevelEntry};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

// ── 命名常量 ─────────────────────────────────────────────────────────────

pub const CONNECT_TIMEOUT_SECS: u64 = 5;
pub const READ_TIMEOUT_SECS: u64 = 15;
pub const CACHE_TTL_SECS: i64 = 86_400; // 24 小时
pub const NETWORK_BACKOFF_SECS: i64 = 300; // 5 分钟
pub const CLOCK_BACKOFF_SECS: i64 = 30; // 30 秒（本地时钟校正后快速重试）
pub const DEBOUNCE_INTERVAL_SECS: i64 = 3; // 3 秒
pub const CLOCK_SKEW_TOLERANCE_SECS: i64 = 60; // 60 秒时钟微漂移容差
pub const CACHE_SCHEMA_VERSION: u32 = 1;

// ── 上游 API 与缓存文件数据结构 ──────────────────────────────────────────

/// OpenRouter 原始模型元数据
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OpenRouterRawModel {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub context_length: Option<u64>,
    #[serde(default)]
    pub top_provider: Option<OpenRouterTopProvider>,
    #[serde(default)]
    pub architecture: Option<OpenRouterArchitecture>,
    #[serde(default)]
    pub supported_parameters: Vec<String>,
    #[serde(default)]
    pub reasoning: Option<OpenRouterRawReasoning>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OpenRouterTopProvider {
    #[serde(default)]
    pub max_completion_tokens: Option<u64>,
    #[serde(default)]
    pub context_length: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OpenRouterArchitecture {
    #[serde(default)]
    pub input_modalities: Vec<String>,
    #[serde(default)]
    pub output_modalities: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OpenRouterRawReasoning {
    #[serde(default)]
    pub supported_efforts: Vec<String>,
    #[serde(default)]
    pub default_effort: Option<String>,
}

/// 上游 GET https://openrouter.ai/api/v1/models 响应外层结构
#[derive(Debug, Clone, Deserialize)]
struct OpenRouterModelsApiResponse {
    #[serde(default)]
    pub data: Vec<OpenRouterRawModel>,
}

/// 本地缓存文件结构（JSON Schema）
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OpenRouterCacheFile {
    pub schema_version: u32,
    pub source: String,
    pub fetched_at: i64,
    pub models: Vec<OpenRouterRawModel>,
}

// ── 会话状态与结果枚举 ──────────────────────────────────────────────────

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    NetworkOrTimeout,
    ClockAnomaly,
}

#[derive(Debug, Default)]
pub struct OpenRouterConnectSession {
    /// TUI 事件循环专用防抖时间戳
    pub last_refresh_press: i64,
    /// 统合退避时间戳与类型
    pub last_failure: Option<(FailureKind, i64)>,
    /// 是否处于加载状态
    pub loading: bool,
    /// 递增任务版本，丢弃跨世代脏完成消息
    pub request_token: u64,
    /// 运行中后台任务句柄
    pub in_flight: Option<tokio::task::AbortHandle>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DegradedReason {
    NetworkBackoff,
    ClockAnomalyBackoff,
    OfflineStaleCache,
}

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq)]
pub enum OpenRouterCatalogResult {
    Fresh(Vec<OpenRouterRawModel>),
    CacheHit(Vec<OpenRouterRawModel>),
    DegradedFallback {
        models: Vec<OpenRouterRawModel>,
        reason: DegradedReason,
    },
}

// ── 本地归一化输出模型 ──────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub struct NormalizedOpenRouterModel {
    pub upstream_model: String,
    pub display_name: Option<String>,
    pub context_window: u64,
    pub max_output_tokens: Option<u64>,
    pub features: Vec<String>,
    pub reasoning_levels: Vec<String>,
    pub default_reasoning_level: Option<String>,
}

// ── 缓存文件路径辅助 ────────────────────────────────────────────────────

pub fn cache_file_path(cache_dir: &Path) -> PathBuf {
    cache_dir.join("openrouter_models.json")
}

// ── 元数据归一化（Normalization） ────────────────────────────────────────

/// 将上游 OpenRouter 原始模型归一化为规范配置格式。
/// 严格遵循宁缺毋假原则：
/// - 若 raw.context_length 缺失或 <= 0，返回 None 并打 warn，绝不虚构；
/// - max_output_tokens 缺失置 None，绝不虚构 4096；
/// - features 仅映射 image_input 与 tools；temperature/max_tokens/reasoning 绝不混入；
/// - reasoning_levels 严格双向 lowercase 比对；若 levels 为空，孤儿 default_effort 忽略不落盘。
pub fn normalize_openrouter_model(raw: &OpenRouterRawModel) -> Option<NormalizedOpenRouterModel> {
    // 宁缺毋假原则：严格以顶层 context_length > 0 为唯一准则
    let context_window = match raw.context_length {
        Some(cw) if cw > 0 => cw,
        _ => {
            tracing::warn!(
                target: "llm_proxy::connect::openrouter",
                model_id = %raw.id,
                "skipping model with missing or non-positive context_length"
            );
            return None;
        }
    };

    // max_output_tokens: 取 top_provider.max_completion_tokens > 0，缺失置 None
    let max_output_tokens = raw
        .top_provider
        .as_ref()
        .and_then(|tp| tp.max_completion_tokens)
        .filter(|&tokens| tokens > 0);

    // features: 仅依据样本实证推导
    let mut features = Vec::new();
    if let Some(ref arch) = raw.architecture
        && arch.input_modalities.iter().any(|m| m == "image")
    {
        features.push("image_input".to_string());
    }
    if raw
        .supported_parameters
        .iter()
        .any(|p| p == "tools" || p == "tool_choice")
    {
        features.push("tools".to_string());
    }
    // reasoning, temperature, max_tokens 绝不混入 features

    // reasoning levels & default reasoning level: 严格双向 lowercase 比对
    let reasoning_levels: Vec<String> = raw
        .reasoning
        .as_ref()
        .map(|r| {
            r.supported_efforts
                .iter()
                .map(|e| e.to_lowercase())
                .collect()
        })
        .unwrap_or_default();

    // 显式短路顺序：若 reasoning_levels 为空直接为 None（忽略孤儿 default_effort）
    let default_reasoning_level = if reasoning_levels.is_empty() {
        None
    } else {
        raw.reasoning
            .as_ref()
            .and_then(|r| r.default_effort.as_ref())
            .map(|d| d.to_lowercase())
            .filter(|d| reasoning_levels.contains(d))
    };

    Some(NormalizedOpenRouterModel {
        upstream_model: raw.id.clone(),
        display_name: raw.name.clone().filter(|s| !s.trim().is_empty()),
        context_window,
        max_output_tokens,
        features,
        reasoning_levels,
        default_reasoning_level,
    })
}

/// 将原始模型批量转换为 TUI ModelItem
pub fn convert_raw_models_to_items(
    raw_models: &[OpenRouterRawModel],
) -> Vec<crate::tui::model::ModelItem> {
    raw_models
        .iter()
        .filter_map(normalize_openrouter_model)
        .map(|norm| {
            let display_name = norm
                .display_name
                .unwrap_or_else(|| norm.upstream_model.clone());
            crate::tui::model::ModelItem {
                id: norm.upstream_model,
                display_name,
                context_window: Some(norm.context_window as i64),
                max_output_tokens: norm.max_output_tokens.map(|v| v as i64),
                features: norm.features,
                reasoning_levels: norm.reasoning_levels,
                default_reasoning_level: norm.default_reasoning_level,
            }
        })
        .collect()
}

// ── 消歧与防重算法（Frontend ID 分配） ──────────────────────────────────

/// 生成 OpenRouter 专属基准 slug："{org}-{model}-or-lp"
pub fn generate_base_slug(upstream_model: &str) -> String {
    let sanitized = upstream_model.replace(['/', '_', '.'], "-").to_lowercase();
    format!("{sanitized}-or-lp")
}

/// 全局消歧与批次防重分配算法
///
/// 优先级 1: 反查 existing_openrouter_bindings 若命中，直接短路沿用 key，绝不生成 -2
/// 优先级 2: 首次添加生成基准 slug "{org}-{model}-or-lp"
/// 优先级 3: 若被全局 allocated_keys 占用（跨 Provider 同名占用），循环探测首个空位 "{base}-or-lp-{N}"
/// 批次防重: 分配出的 key 立即插入 allocated_keys
pub fn resolve_openrouter_frontend_id(
    upstream_model: &str,
    existing_openrouter_bindings: &HashMap<String, String>,
    allocated_keys: &mut HashSet<String>,
) -> String {
    // 优先级 1：同 Provider 绑定反查短路
    if let Some(existing_key) = existing_openrouter_bindings.get(upstream_model) {
        allocated_keys.insert(existing_key.clone());
        return existing_key.clone();
    }

    // 优先级 2：基准 slug
    let base_slug = generate_base_slug(upstream_model);
    if !allocated_keys.contains(&base_slug) {
        allocated_keys.insert(base_slug.clone());
        return base_slug;
    }

    // 优先级 3：递增探测空位
    let mut counter = 2;
    loop {
        let candidate = format!("{base_slug}-{counter}");
        if !allocated_keys.contains(&candidate) {
            allocated_keys.insert(candidate.clone());
            return candidate;
        }
        counter += 1;
    }
}

// ── 本地缓存读取与原子落盘 ──────────────────────────────────────────────

/// 原子写入缓存文件（临时文件 + sync_all + 0600 + rename）
pub fn write_cache_atomic(cache_dir: &Path, models: &[OpenRouterRawModel], now: i64) -> Result<()> {
    fs::create_dir_all(cache_dir)
        .with_context(|| format!("failed to create cache dir: {}", cache_dir.display()))?;

    let cache_file = cache_file_path(cache_dir);
    let cache_content = OpenRouterCacheFile {
        schema_version: CACHE_SCHEMA_VERSION,
        source: "openrouter".to_string(),
        fetched_at: now,
        models: models.to_vec(),
    };
    let json_bytes = serde_json::to_vec_pretty(&cache_content)
        .context("failed to serialize openrouter cache json")?;

    let random_nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let temp_path = cache_dir.join(format!(".openrouter_models.json.tmp.{random_nonce}"));

    let mut file = std::fs::File::create(&temp_path)
        .with_context(|| format!("failed to create temp file {}", temp_path.display()))?;
    file.write_all(&json_bytes)
        .context("failed to write openrouter cache temp file")?;
    file.sync_all()
        .context("failed to sync openrouter cache temp file")?;
    drop(file);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&temp_path, std::fs::Permissions::from_mode(0o600));
    }

    std::fs::rename(&temp_path, &cache_file).with_context(|| {
        format!(
            "failed to rename {} -> {}",
            temp_path.display(),
            cache_file.display()
        )
    })?;

    Ok(())
}

/// 尝试读取本地缓存文件。
/// 严格遵守双层兼容与损坏隔离原则：
/// - 若 JSON 截断或反序列化失败，或 schema_version != CACHE_SCHEMA_VERSION：
///   隔离重命名为 `.bak.<timestamp>`，返回 None；
/// - 正常文件返回 Some(OpenRouterCacheFile)。
pub fn read_cache_file(cache_dir: &Path, now: i64) -> Option<OpenRouterCacheFile> {
    let cache_file = cache_file_path(cache_dir);
    if !cache_file.exists() {
        return None;
    }

    let content = match fs::read_to_string(&cache_file) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(
                target: "llm_proxy::connect::openrouter",
                error = %e,
                "failed to read openrouter cache file, isolating"
            );
            isolate_corrupted_cache(&cache_file, now);
            return None;
        }
    };

    match serde_json::from_str::<OpenRouterCacheFile>(&content) {
        Ok(cache) => {
            if cache.schema_version != CACHE_SCHEMA_VERSION {
                tracing::warn!(
                    target: "llm_proxy::connect::openrouter",
                    expected = CACHE_SCHEMA_VERSION,
                    actual = cache.schema_version,
                    "openrouter cache schema mismatch, isolating"
                );
                isolate_corrupted_cache(&cache_file, now);
                None
            } else {
                Some(cache)
            }
        }
        Err(e) => {
            tracing::warn!(
                target: "llm_proxy::connect::openrouter",
                error = %e,
                "openrouter cache json corrupted, isolating"
            );
            isolate_corrupted_cache(&cache_file, now);
            None
        }
    }
}

fn isolate_corrupted_cache(cache_file: &Path, now: i64) {
    let bak_path = cache_file.with_extension(format!("json.bak.{}", now));
    let _ = fs::rename(cache_file, bak_path);
}

/// 纯无网络本地缓存有效性判定辅助函数
#[allow(dead_code)]
pub fn read_cached_catalog_if_valid(cache_dir: &Path, now: i64) -> Option<Vec<OpenRouterRawModel>> {
    let cache = read_cache_file(cache_dir, now)?;
    let fetched_at = cache.fetched_at;

    // 时钟异常分支：超前当前时间 60s 以上
    if fetched_at > now + CLOCK_SKEW_TOLERANCE_SECS {
        return None;
    }

    // 正常有效或合法微漂移
    if (now >= fetched_at && now - fetched_at <= CACHE_TTL_SECS)
        || (now < fetched_at && fetched_at <= now + CLOCK_SKEW_TOLERANCE_SECS)
    {
        Some(cache.models)
    } else {
        None
    }
}

// ── 获取计划与决策（纯函数，供 TUI 流程调用） ─────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum CatalogFetchPlan {
    UseValidCache(Vec<OpenRouterRawModel>),
    UseDegradedCache {
        models: Vec<OpenRouterRawModel>,
        reason: DegradedReason,
    },
    NeedOnlineFetch,
    BlockedByBackoffNoCache {
        reason: DegradedReason,
    },
}

/// 判定是否允许响应手动刷新按键（防抖 3 秒）
pub fn check_refresh_debounce(session: &mut OpenRouterConnectSession, now: i64) -> bool {
    if session.last_refresh_press > 0 && now - session.last_refresh_press < DEBOUNCE_INTERVAL_SECS {
        false
    } else {
        session.last_refresh_press = now;
        true
    }
}

/// 纯函数决策状态机：确定拉取动作
pub fn plan_catalog_fetch(
    cache_dir: &Path,
    session: &OpenRouterConnectSession,
    now: i64,
) -> CatalogFetchPlan {
    let cache_file = cache_file_path(cache_dir);
    let cache_exists_initially = cache_file.exists();

    let cache_opt = read_cache_file(cache_dir, now);

    // 损坏隔离优先穿透：若初始存在但读取损坏（已隔离返回 None），立即穿透退避发起在线拉取
    if cache_exists_initially && cache_opt.is_none() {
        return CatalogFetchPlan::NeedOnlineFetch;
    }

    if let Some(ref cache) = cache_opt {
        let fetched_at = cache.fetched_at;

        // 时钟异常：超前当前时间 60s
        let is_clock_anomaly = fetched_at > now + CLOCK_SKEW_TOLERANCE_SECS;

        if !is_clock_anomaly {
            let is_valid = (now >= fetched_at && now - fetched_at <= CACHE_TTL_SECS)
                || (now < fetched_at && fetched_at <= now + CLOCK_SKEW_TOLERANCE_SECS);

            if is_valid {
                return CatalogFetchPlan::UseValidCache(cache.models.clone());
            }
        }
    }

    // 缓存不存在、过期或异常，检查退避
    if let Some((kind, fail_time)) = session.last_failure {
        let in_backoff = match kind {
            FailureKind::NetworkOrTimeout => {
                now >= fail_time && now - fail_time < NETWORK_BACKOFF_SECS
            }
            FailureKind::ClockAnomaly => now >= fail_time && now - fail_time < CLOCK_BACKOFF_SECS,
        };

        if in_backoff {
            let reason = match kind {
                FailureKind::NetworkOrTimeout => DegradedReason::NetworkBackoff,
                FailureKind::ClockAnomaly => DegradedReason::ClockAnomalyBackoff,
            };

            return if let Some(ref cache) = cache_opt {
                CatalogFetchPlan::UseDegradedCache {
                    models: cache.models.clone(),
                    reason,
                }
            } else {
                CatalogFetchPlan::BlockedByBackoffNoCache { reason }
            };
        }
    }

    CatalogFetchPlan::NeedOnlineFetch
}

/// 纯异步后台在线拉取函数（无状态引用借用，可安全 tokio::spawn）
pub async fn fetch_openrouter_catalog_online(
    cache_dir: PathBuf,
    client: &reqwest::Client,
    api_url: &str,
    now: i64,
) -> Result<Vec<OpenRouterRawModel>> {
    let custom_client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(CONNECT_TIMEOUT_SECS))
        .timeout(std::time::Duration::from_secs(READ_TIMEOUT_SECS))
        .build()
        .unwrap_or_else(|_| client.clone());

    let resp = custom_client
        .get(api_url)
        .header("User-Agent", "llm-proxy")
        .send()
        .await
        .context("failed to request openrouter models endpoint")?;

    let status = resp.status();
    if !status.is_success() {
        anyhow::bail!("openrouter models endpoint returned error status: {status}");
    }

    let api_resp: OpenRouterModelsApiResponse = resp
        .json()
        .await
        .context("failed to parse openrouter models json response")?;

    // 原子写入本地缓存
    write_cache_atomic(&cache_dir, &api_resp.data, now)?;

    Ok(api_resp.data)
}

// ── 配置更新与持久化（纯本地内存操作，零网络探活） ────────────────────

/// 扫描现有配置中仅属于 openrouter 的模型绑定映射：upstream_model -> model_key
pub fn build_openrouter_existing_bindings(
    models: &BTreeMap<String, ModelConfig>,
) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for (key, model_cfg) in models {
        for protocol in Protocol::CLIENT_PROTOCOLS {
            for binding in model_cfg.provider_bindings(protocol) {
                if binding.name.to_lowercase() == "openrouter" {
                    map.insert(binding.model.clone(), key.clone());
                }
            }
        }
    }
    map
}

/// 纯本地内存编辑与落盘函数（函数签名仅接受 &mut Config，无网络 Client，从类型系统杜绝网络探活）
pub fn apply_dynamic_models_to_config(
    cfg: &mut Config,
    selected: &[OpenRouterRawModel],
) -> Result<Vec<String>> {
    // 1. 确保 providers.openrouter 存在（若已存在严格无损保留，若不存在则新建）
    if !cfg.providers.contains_key("openrouter") {
        let entry = crate::catalog::openrouter();
        cfg.providers
            .insert("openrouter".to_string(), entry.provider);
    }

    // 2. 建立 Provider 隔离的反查映射与全局 allocated_keys 占用池
    let existing_bindings = build_openrouter_existing_bindings(&cfg.models);
    let mut allocated_keys: HashSet<String> = cfg.models.keys().cloned().collect();

    // 3. 稳定排序：按 upstream_model 升序遍历以保障批次内确定性
    let mut sorted_selected = selected.to_vec();
    sorted_selected.sort_by(|a, b| a.id.cmp(&b.id));

    let mut applied_keys = Vec::new();

    for raw in &sorted_selected {
        // 归一化提取（宁缺毋假）
        let normalized = match normalize_openrouter_model(raw) {
            Some(n) => n,
            None => continue,
        };

        // 消歧与防重分配 slug
        let frontend_id = resolve_openrouter_frontend_id(
            &normalized.upstream_model,
            &existing_bindings,
            &mut allocated_keys,
        );

        let reasoning_entries = if normalized.reasoning_levels.is_empty() {
            None
        } else {
            Some(
                normalized
                    .reasoning_levels
                    .iter()
                    .map(|l| ReasoningLevelEntry::new(l.clone(), None))
                    .collect(),
            )
        };

        let binding = ProviderBinding {
            name: "openrouter".to_string(),
            model: normalized.upstream_model.clone(),
        };

        // 字段级安全合并：若已有配置存在，原地保留用户手写的未知字段（如自定义 description、probe 等）
        let mut model_cfg = cfg
            .models
            .get(&frontend_id)
            .cloned()
            .unwrap_or_else(|| ModelConfig {
                description: None,
                context_window: normalized.context_window as i64,
                max_output_tokens: normalized.max_output_tokens.unwrap_or(0) as i64,
                features: normalized.features.clone(),
                reasoning_levels: reasoning_entries.clone(),
                default_reasoning_level: normalized.default_reasoning_level.clone(),
                supports_thinking: None,
                force_think: None,
                force_no_think: None,
                default_think: None,
                enable_thinking: None,
                chat_reasoning_off_format: None,
                codex_service_tier: None,
                openai_chat_providers: vec![binding.clone()],
                openai_responses_providers: vec![binding.clone()],
                anthropic_providers: vec![binding.clone()],
                reasoning_level_map: None,
                supported_reasoning_levels: Vec::new(),
                cooldown: None,
                reasoning_routes: None,
            });

        // 刷新基本元数据
        model_cfg.context_window = normalized.context_window as i64;
        if let Some(tokens) = normalized.max_output_tokens {
            model_cfg.max_output_tokens = tokens as i64;
        }
        model_cfg.features = normalized.features;
        model_cfg.reasoning_levels = reasoning_entries;
        model_cfg.default_reasoning_level = normalized.default_reasoning_level;

        // 绑定到 openrouter provider（对支持的客户端协议配置 bindings）
        model_cfg.openai_chat_providers = vec![binding.clone()];
        model_cfg.openai_responses_providers = vec![binding.clone()];
        model_cfg.anthropic_providers = vec![binding.clone()];

        cfg.models.insert(frontend_id.clone(), model_cfg);
        applied_keys.push(frontend_id);
    }

    Ok(applied_keys)
}

// ── 单元测试 ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ProviderConfig;

    #[test]
    fn test_openrouter_metadata_normalization() {
        // 1. 标准思考模型归一化（双向 lowercase 比对成功）
        let raw_thinking = OpenRouterRawModel {
            id: "stepfun/step-5-preview".to_string(),
            name: Some("StepFun: Step 5 Preview".to_string()),
            context_length: Some(1_000_000),
            top_provider: Some(OpenRouterTopProvider {
                max_completion_tokens: Some(64_000),
                context_length: Some(1_000_000),
            }),
            architecture: Some(OpenRouterArchitecture {
                input_modalities: vec!["text".to_string(), "image".to_string()],
                output_modalities: vec!["text".to_string()],
            }),
            supported_parameters: vec![
                "tools".to_string(),
                "temperature".to_string(),
                "reasoning".to_string(),
            ],
            reasoning: Some(OpenRouterRawReasoning {
                supported_efforts: vec![
                    "High".to_string(),
                    "Medium".to_string(),
                    "LOW".to_string(),
                ],
                default_effort: Some("MEDIUM".to_string()),
            }),
        };

        let norm = normalize_openrouter_model(&raw_thinking).expect("should normalize");
        assert_eq!(norm.upstream_model, "stepfun/step-5-preview");
        assert_eq!(norm.context_window, 1_000_000);
        assert_eq!(norm.max_output_tokens, Some(64_000));
        assert_eq!(norm.features, vec!["image_input", "tools"]);
        assert_eq!(norm.reasoning_levels, vec!["high", "medium", "low"]);
        assert_eq!(norm.default_reasoning_level, Some("medium".to_string()));

        // 2. 缺失思考档位（无 reasoning 对象），但在 supported_parameters 里声明了 reasoning
        let raw_unconfigured = OpenRouterRawModel {
            id: "test/unconfigured-thinking".to_string(),
            name: Some("Unconfigured".to_string()),
            context_length: Some(128_000),
            top_provider: None,
            architecture: None,
            supported_parameters: vec!["reasoning".to_string(), "tools".to_string()],
            reasoning: None,
        };
        let norm_unconf = normalize_openrouter_model(&raw_unconfigured).expect("should normalize");
        assert_eq!(norm_unconf.reasoning_levels, Vec::<String>::new());
        assert_eq!(norm_unconf.default_reasoning_level, None);
        assert_eq!(norm_unconf.max_output_tokens, None); // 缺失置 None，绝不虚构 4096

        // 3. 孤儿 default_effort（有 default 无 supported_efforts）
        let raw_orphan = OpenRouterRawModel {
            id: "test/orphan-default".to_string(),
            name: None,
            context_length: Some(32_000),
            top_provider: None,
            architecture: None,
            supported_parameters: vec![],
            reasoning: Some(OpenRouterRawReasoning {
                supported_efforts: vec![],
                default_effort: Some("high".to_string()),
            }),
        };
        let norm_orphan = normalize_openrouter_model(&raw_orphan).expect("should normalize");
        assert_eq!(norm_orphan.reasoning_levels, Vec::<String>::new());
        assert_eq!(norm_orphan.default_reasoning_level, None); // 孤儿 default 被忽略，不虚构
        assert_eq!(norm_orphan.display_name, None);
    }

    #[test]
    fn test_openrouter_batch_resilience_parse() {
        // 构造包含 500+ 模型列表的 JSON，其中穿插畸形项（context_length 缺失/为0/格式畸形）
        let mut raw_models = Vec::new();
        for i in 0..520 {
            if i % 50 == 0 {
                // 畸形项 1：缺失 context_length
                raw_models.push(OpenRouterRawModel {
                    id: format!("provider/bad-model-{i}"),
                    name: None,
                    context_length: None,
                    top_provider: None,
                    architecture: None,
                    supported_parameters: vec![],
                    reasoning: None,
                });
            } else if i % 50 == 1 {
                // 畸形项 2：context_length 为 0
                raw_models.push(OpenRouterRawModel {
                    id: format!("provider/zero-model-{i}"),
                    name: None,
                    context_length: Some(0),
                    top_provider: None,
                    architecture: None,
                    supported_parameters: vec![],
                    reasoning: None,
                });
            } else {
                // 正常模型
                raw_models.push(OpenRouterRawModel {
                    id: format!("provider/valid-model-{i}"),
                    name: Some(format!("Valid Model {i}")),
                    context_length: Some(128_000),
                    top_provider: Some(OpenRouterTopProvider {
                        max_completion_tokens: Some(4096),
                        context_length: Some(128_000),
                    }),
                    architecture: None,
                    supported_parameters: vec!["tools".to_string()],
                    reasoning: None,
                });
            }
        }

        // 批量过滤与归一化
        let normalized: Vec<_> = raw_models
            .iter()
            .filter_map(normalize_openrouter_model)
            .collect();

        // 验证 520 个模型中，畸形项全部被安全跳过且不 panic
        assert_eq!(normalized.len(), 520 - 22);
        assert!(normalized.iter().all(|m| m.context_window > 0));
    }

    #[test]
    fn test_openrouter_offline_fallback_and_debounce() {
        let temp_dir = tempfile::tempdir().unwrap();
        let cache_dir = temp_dir.path();
        let now = 1_000_000;

        let mut session = OpenRouterConnectSession::default();

        // 1. 无缓存、无失败记录时，计划应为 NeedOnlineFetch
        let plan = plan_catalog_fetch(cache_dir, &session, now);
        assert_eq!(plan, CatalogFetchPlan::NeedOnlineFetch);

        // 2. 模拟网络超时失败并进入退避（无缓存）
        session.last_failure = Some((FailureKind::NetworkOrTimeout, now));
        let plan = plan_catalog_fetch(cache_dir, &session, now + 10);
        assert_eq!(
            plan,
            CatalogFetchPlan::BlockedByBackoffNoCache {
                reason: DegradedReason::NetworkBackoff
            }
        );

        // 3. 写入一份旧缓存（创建于 30 小时前，已超 24h TTL）
        let old_models = vec![OpenRouterRawModel {
            id: "old/m1".to_string(),
            name: None,
            context_length: Some(32_000),
            top_provider: None,
            architecture: None,
            supported_parameters: vec![],
            reasoning: None,
        }];
        let old_time = now - 30 * 3600;
        write_cache_atomic(cache_dir, &old_models, old_time).unwrap();

        // 在退避期内，应降级使用陈旧缓存
        let plan = plan_catalog_fetch(cache_dir, &session, now + 50);
        assert!(matches!(
            plan,
            CatalogFetchPlan::UseDegradedCache {
                reason: DegradedReason::NetworkBackoff,
                ..
            }
        ));

        // 退避期过后（301 秒后），应再次尝试在线拉取
        let plan = plan_catalog_fetch(cache_dir, &session, now + NETWORK_BACKOFF_SECS + 1);
        assert_eq!(plan, CatalogFetchPlan::NeedOnlineFetch);

        // 4. 防抖测试
        session.last_refresh_press = 0;
        assert!(check_refresh_debounce(&mut session, now));
        assert_eq!(session.last_refresh_press, now);

        // 1 秒后再次按刷新，被防抖拦截
        assert!(!check_refresh_debounce(&mut session, now + 1));
        assert_eq!(session.last_refresh_press, now);

        // 3 秒后再次按刷新，允许放行
        assert!(check_refresh_debounce(&mut session, now + 3));
        assert_eq!(session.last_refresh_press, now + 3);
    }

    #[test]
    fn test_openrouter_cache_corruption_punches_through_backoff() {
        let temp_dir = tempfile::tempdir().unwrap();
        let cache_dir = temp_dir.path();
        let cache_file = cache_file_path(cache_dir);
        let now = 1_000_000;

        let session = OpenRouterConnectSession {
            last_failure: Some((FailureKind::NetworkOrTimeout, now)),
            ..OpenRouterConnectSession::default()
        };

        // 写入损坏文件
        fs::write(&cache_file, b"corrupted data").unwrap();

        // 损坏检测应穿透退避，返回 NeedOnlineFetch
        let plan = plan_catalog_fetch(cache_dir, &session, now + 10);
        assert_eq!(plan, CatalogFetchPlan::NeedOnlineFetch);
    }

    #[test]
    fn test_openrouter_context_window_missing_skip() {
        // 缺少 context_length 或 <= 0 时跳过并返回 None
        let raw_missing = OpenRouterRawModel {
            id: "bad/missing-cw".to_string(),
            name: None,
            context_length: None,
            top_provider: None,
            architecture: None,
            supported_parameters: vec![],
            reasoning: None,
        };
        assert!(normalize_openrouter_model(&raw_missing).is_none());

        let raw_zero = OpenRouterRawModel {
            id: "bad/zero-cw".to_string(),
            name: None,
            context_length: Some(0),
            top_provider: None,
            architecture: None,
            supported_parameters: vec![],
            reasoning: None,
        };
        assert!(normalize_openrouter_model(&raw_zero).is_none());
    }

    #[test]
    fn test_openrouter_frontend_id_idempotency_and_provider_isolation() {
        let mut bindings = HashMap::new();
        bindings.insert(
            "google/gemini-2.5-flash".to_string(),
            "google-gemini-2-5-flash-or-lp".to_string(),
        );

        let mut allocated = HashSet::new();
        allocated.insert("google-gemini-2-5-flash-or-lp".to_string());

        // 1. 同一 OpenRouter 模型重复添加：短路沿用 key，绝不生成 -2
        let key1 =
            resolve_openrouter_frontend_id("google/gemini-2.5-flash", &bindings, &mut allocated);
        assert_eq!(key1, "google-gemini-2-5-flash-or-lp");

        // 2. 非 OpenRouter Provider 下若无绑定，但基准 slug 被占用，递增生成 -2
        let empty_bindings = HashMap::new();
        let key2 = resolve_openrouter_frontend_id(
            "google/gemini-2.5-flash",
            &empty_bindings,
            &mut allocated,
        );
        assert_eq!(key2, "google-gemini-2-5-flash-or-lp-2");

        // 再次添加冲突项，生成 -3
        let key3 = resolve_openrouter_frontend_id(
            "google/gemini-2.5-flash",
            &empty_bindings,
            &mut allocated,
        );
        assert_eq!(key3, "google-gemini-2-5-flash-or-lp-3");
    }

    #[test]
    fn test_openrouter_frontend_id_batch_and_cross_provider_collision() {
        let bindings = HashMap::new();
        let mut allocated = HashSet::new();
        allocated.insert("vertex-gemini-or-lp".to_string()); // 其他 provider 占用

        // 同批次选两个前缀相同的模型
        let id_a = resolve_openrouter_frontend_id("foo/bar", &bindings, &mut allocated);
        let id_b = resolve_openrouter_frontend_id("foo-bar", &bindings, &mut allocated);

        assert_eq!(id_a, "foo-bar-or-lp");
        assert_eq!(id_b, "foo-bar-or-lp-2"); // 批次内不碰撞
    }

    #[test]
    fn test_openrouter_cache_ttl_and_backoff() {
        let temp_dir = tempfile::tempdir().unwrap();
        let cache_dir = temp_dir.path();
        let now = 1_000_000;

        let models = vec![OpenRouterRawModel {
            id: "test/m1".to_string(),
            name: Some("Model 1".to_string()),
            context_length: Some(100_000),
            top_provider: None,
            architecture: None,
            supported_parameters: vec![],
            reasoning: None,
        }];

        // 1. 原子落盘写入
        write_cache_atomic(cache_dir, &models, now).unwrap();

        // 2. 正常有效缓存命中（5 小时后）
        let cached = read_cached_catalog_if_valid(cache_dir, now + 18_000);
        assert!(cached.is_some());
        assert_eq!(cached.unwrap().len(), 1);

        // 3. 微小漂移（当前时钟比落盘慢 30 秒，容忍内）
        let drift = read_cached_catalog_if_valid(cache_dir, now - 30);
        assert!(drift.is_some());

        // 4. 时钟异常（当前时钟比落盘慢 100 秒，超过 60s 容差）
        let anomaly = read_cached_catalog_if_valid(cache_dir, now - 100);
        assert!(anomaly.is_none());

        // 5. 过期（25 小时后）
        let expired = read_cached_catalog_if_valid(cache_dir, now + CACHE_TTL_SECS + 1);
        assert!(expired.is_none());
    }

    #[test]
    fn test_openrouter_cache_file_corruption_and_backup() {
        let temp_dir = tempfile::tempdir().unwrap();
        let cache_dir = temp_dir.path();
        let cache_file = cache_file_path(cache_dir);
        let now = 1_000_000;

        // 写入损坏截断的 JSON
        fs::write(&cache_file, b"{\"schema_version\": 1, \"models\": [").unwrap();

        // 读取检测损坏，应自动重命名备份并返回 None
        let res = read_cache_file(cache_dir, now);
        assert!(res.is_none());
        assert!(!cache_file.exists());

        let bak_file = cache_file.with_extension(format!("json.bak.{}", now));
        assert!(bak_file.exists());
    }

    #[test]
    fn test_openrouter_convert_raw_models_to_items() {
        let raw = vec![
            OpenRouterRawModel {
                id: "anthropic/claude-3.7-sonnet".to_string(),
                name: Some("Claude 3.7 Sonnet".to_string()),
                context_length: Some(200_000),
                top_provider: Some(OpenRouterTopProvider {
                    max_completion_tokens: Some(64_000),
                    context_length: Some(200_000),
                }),
                architecture: Some(OpenRouterArchitecture {
                    input_modalities: vec!["image".to_string()],
                    output_modalities: vec![],
                }),
                supported_parameters: vec!["tools".to_string()],
                reasoning: None,
            },
            // 畸形模型（会被跳过）
            OpenRouterRawModel {
                id: "invalid/missing-cw".to_string(),
                name: None,
                context_length: None,
                top_provider: None,
                architecture: None,
                supported_parameters: vec![],
                reasoning: None,
            },
        ];

        let items = convert_raw_models_to_items(&raw);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, "anthropic/claude-3.7-sonnet");
        assert_eq!(items[0].display_name, "Claude 3.7 Sonnet");
        assert_eq!(items[0].context_window, Some(200_000));
        assert_eq!(items[0].max_output_tokens, Some(64_000));
        assert!(items[0].supports_image());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_openrouter_online_fetch_and_cache_integration() {
        use axum::{Router, routing::get};

        let app = Router::new().route(
            "/api/v1/models",
            get(|| async {
                axum::Json(serde_json::json!({
                    "data": [
                        {
                            "id": "anthropic/claude-3.7-sonnet",
                            "name": "Claude 3.7 Sonnet",
                            "context_length": 200000,
                            "top_provider": {
                                "max_completion_tokens": 64000
                            },
                            "architecture": {
                                "input_modalities": ["text", "image"]
                            },
                            "supported_parameters": ["tools", "reasoning"],
                            "reasoning": {
                                "supported_efforts": ["low", "medium", "high"],
                                "default_effort": "medium"
                            }
                        }
                    ]
                }))
            }),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let temp_dir = tempfile::tempdir().unwrap();
        let cache_dir = temp_dir.path().to_path_buf();
        let client = reqwest::Client::new();
        let url = format!("http://{addr}/api/v1/models");
        let now = 1_700_000_000;

        let models = fetch_openrouter_catalog_online(cache_dir.clone(), &client, &url, now)
            .await
            .unwrap();

        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "anthropic/claude-3.7-sonnet");

        // 验证原子落盘缓存已存在且内容正确
        let cached = read_cache_file(&cache_dir, now).expect("cache should exist");
        assert_eq!(cached.fetched_at, now);
        assert_eq!(cached.models.len(), 1);
    }

    #[test]
    fn test_openrouter_provider_and_model_field_level_merge() {
        let mut cfg = toml::from_str::<Config>(
            r#"
[server]
listen = "127.0.0.1:0"
"#,
        )
        .unwrap();

        // 用户已有手动配置的 openrouter provider（包含自定义反代与超时）
        let custom_provider = ProviderConfig {
            api_key_env: Some("MY_CUSTOM_OR_KEY".to_string()),
            ..Default::default()
        };
        cfg.providers
            .insert("openrouter".to_string(), custom_provider);

        // 用户已有一个已有模型配置，手写了 description 和 probe，并绑定了 openrouter
        let existing_model = ModelConfig {
            description: Some("my custom description".to_string()),
            context_window: 50_000,
            max_output_tokens: 2048,
            features: vec![],
            reasoning_levels: None,
            default_reasoning_level: None,
            supports_thinking: None,
            force_think: None,
            force_no_think: None,
            default_think: None,
            enable_thinking: None,
            chat_reasoning_off_format: None,
            codex_service_tier: None,
            openai_chat_providers: vec![ProviderBinding {
                name: "openrouter".to_string(),
                model: "stepfun/step-5-preview".to_string(),
            }],
            openai_responses_providers: vec![],
            anthropic_providers: vec![],
            reasoning_level_map: None,
            supported_reasoning_levels: vec![],
            cooldown: None,
            reasoning_routes: None,
        };
        cfg.models
            .insert("stepfun-step-5-preview-or-lp".to_string(), existing_model);

        let selected = vec![OpenRouterRawModel {
            id: "stepfun/step-5-preview".to_string(),
            name: Some("Step 5".to_string()),
            context_length: Some(1_000_000),
            top_provider: None,
            architecture: None,
            supported_parameters: vec!["tools".to_string()],
            reasoning: None,
        }];

        let applied = apply_dynamic_models_to_config(&mut cfg, &selected).unwrap();
        assert_eq!(applied, vec!["stepfun-step-5-preview-or-lp"]);

        // 验证已存在的 provider 未被覆盖
        assert_eq!(
            cfg.providers["openrouter"].api_key_env,
            Some("MY_CUSTOM_OR_KEY".to_string())
        );

        // 验证已有模型的自定义 description 完整保留，context_window 被成功更新为 1M
        let updated_model = &cfg.models["stepfun-step-5-preview-or-lp"];
        assert_eq!(
            updated_model.description,
            Some("my custom description".to_string())
        );
        assert_eq!(updated_model.context_window, 1_000_000);
        assert!(updated_model.features.contains(&"tools".to_string()));
    }
}
