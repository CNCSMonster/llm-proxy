use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub server: ServerConfig,
    #[serde(default)]
    pub fallback: FallbackConfig,
    #[serde(default)]
    pub protection: ProtectionConfig,
    #[serde(default)]
    pub status: StatusConfig,
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderConfig>,
    #[serde(default)]
    pub models: BTreeMap<String, ModelConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    pub listen: String,
    #[serde(default)]
    pub usage: UsageConfig,
    /// 聚合器 SSE 缓冲上限（字节），防止恶意/挂死上游导致内存无界增长。
    /// 默认 64MB。
    #[serde(default = "default_max_sse_buffer_bytes")]
    pub max_sse_buffer_bytes: usize,
    /// 聚合器 output item 数量上限，防止单个响应包含过多 item。
    /// 默认 4096。
    #[serde(default = "default_max_output_items")]
    pub max_output_items: usize,
}

pub(crate) fn default_max_sse_buffer_bytes() -> usize {
    64 * 1024 * 1024 // 64MB
}

pub(crate) fn default_max_output_items() -> usize {
    4096
}

/// `[status]` 全局配置段（§12.12 Status 配置设计）。
///
/// 提供 probe 与活跃证据的默认参数，可通过 `[providers.<id>.status]`
/// 做 Provider 级覆盖。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusConfig {
    /// 探测超时（秒）：向 upstream 发探活请求的超时时间。
    #[serde(default = "default_probe_timeout")]
    pub probe_timeout: u64,
    /// 活跃证据 TTL（秒）："最近有正常使用"的时间窗口。
    #[serde(default = "default_active_ttl")]
    pub active_ttl: u64,
}

impl Default for StatusConfig {
    fn default() -> Self {
        Self {
            probe_timeout: default_probe_timeout(),
            active_ttl: default_active_ttl(),
        }
    }
}

fn default_probe_timeout() -> u64 {
    30
}

fn default_active_ttl() -> u64 {
    30
}

/// Usage statistics configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageConfig {
    /// File threshold in MB before migration to SQLite
    #[serde(default = "default_file_threshold_mb")]
    pub file_threshold_mb: f64,

    /// Ratio of records to migrate (0.0-1.0)
    #[serde(default = "default_migration_ratio")]
    pub migration_ratio: f64,

    /// SQLite max size in MB before cleanup
    #[serde(default = "default_db_max_size_mb")]
    pub db_max_size_mb: f64,
}

impl Default for UsageConfig {
    fn default() -> Self {
        Self {
            file_threshold_mb: default_file_threshold_mb(),
            migration_ratio: default_migration_ratio(),
            db_max_size_mb: default_db_max_size_mb(),
        }
    }
}

fn default_file_threshold_mb() -> f64 {
    2.0
}

fn default_migration_ratio() -> f64 {
    0.5
}

fn default_db_max_size_mb() -> f64 {
    50.0
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FallbackConfig {
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
    #[serde(default = "default_timeout_seconds")]
    pub timeout_seconds: u64,
    #[serde(default = "default_max_timeout_seconds")]
    pub max_timeout_seconds: u64,
    #[serde(default)]
    pub cooldown: FallbackCooldownConfig,
}

impl Default for FallbackConfig {
    fn default() -> Self {
        Self {
            max_retries: default_max_retries(),
            timeout_seconds: default_timeout_seconds(),
            max_timeout_seconds: default_max_timeout_seconds(),
            cooldown: FallbackCooldownConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FallbackCooldownConfig {
    #[serde(default = "default_network_cooldown_seconds")]
    pub network_seconds: u64,
    #[serde(default = "default_server_error_cooldown_seconds")]
    pub server_error_seconds: u64,
    #[serde(default = "default_rate_limit_cooldown_seconds")]
    pub rate_limit_seconds: u64,
    #[serde(default = "default_model_unavailable_cooldown_seconds")]
    pub model_unavailable_seconds: u64,
    #[serde(default = "default_client_error_cooldown_seconds")]
    pub client_error_seconds: u64,
}

impl Default for FallbackCooldownConfig {
    fn default() -> Self {
        Self {
            network_seconds: default_network_cooldown_seconds(),
            server_error_seconds: default_server_error_cooldown_seconds(),
            rate_limit_seconds: default_rate_limit_cooldown_seconds(),
            model_unavailable_seconds: default_model_unavailable_cooldown_seconds(),
            client_error_seconds: default_client_error_cooldown_seconds(),
        }
    }
}

fn default_max_retries() -> u32 {
    2
}
fn default_timeout_seconds() -> u64 {
    // LLM 长响应（思考型模型深度思考 + 长输出）常态可达数分钟，
    // 30s 会把正常慢响应误判为挂死。300s（5 分钟）允许慢响应，
    // 同时仍能兜底真正的挂死（>5 分钟无响应）。
    300
}
fn default_max_timeout_seconds() -> u64 {
    600
}
fn default_network_cooldown_seconds() -> u64 {
    30
}
fn default_server_error_cooldown_seconds() -> u64 {
    300
}
fn default_rate_limit_cooldown_seconds() -> u64 {
    300
}
fn default_model_unavailable_cooldown_seconds() -> u64 {
    1800
}
fn default_client_error_cooldown_seconds() -> u64 {
    300
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProtectionConfig {
    #[serde(default)]
    pub bad_request: BadRequestProtectionConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BadRequestProtectionConfig {
    #[serde(default = "default_bad_request_enabled")]
    pub enabled: bool,
    #[serde(default = "default_bad_request_window_seconds")]
    pub window_seconds: u64,
    #[serde(default = "default_bad_request_max_errors")]
    pub max_errors: u32,
    #[serde(default = "default_bad_request_block_seconds")]
    pub block_seconds: u64,
}

impl Default for BadRequestProtectionConfig {
    fn default() -> Self {
        Self {
            enabled: default_bad_request_enabled(),
            window_seconds: default_bad_request_window_seconds(),
            max_errors: default_bad_request_max_errors(),
            block_seconds: default_bad_request_block_seconds(),
        }
    }
}

fn default_bad_request_enabled() -> bool {
    true
}
fn default_bad_request_window_seconds() -> u64 {
    300
}
fn default_bad_request_max_errors() -> u32 {
    2
}
fn default_bad_request_block_seconds() -> u64 {
    300
}

/// Provider capability declaration: each present protocol field means the
/// provider can serve that protocol. There is no `endpoints` map layer and no
/// provider-level `base_url`; native endpoints carry complete URLs.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProviderConfig {
    /// Provider 归属的产品 ID（如 "deepseek"、"kimi"）。
    /// 缺省值为 "custom"，表示不属于任何特定产品（手动配置的通用 provider）。
    /// 非空且非 "custom" 时，表示该 provider 属于对应产品，可参与产品级批量 fallback。
    #[serde(default = "default_product")]
    pub product: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<AuthConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enable_thinking: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub openai_chat: Option<EndpointConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub openai_responses: Option<EndpointConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anthropic: Option<EndpointConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub antigravity: Option<EndpointConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_level_map: Option<Vec<ReasoningLevelMapping>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_frequency: Option<RequestFrequencyConfig>,
    /// Provider 级 status 覆盖（§12.12）：`[providers.<id>.status]`，可选。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<StatusConfig>,
}

/// Default product value for providers: "custom" means the provider does not
/// belong to any specific product and was manually configured.
fn default_product() -> String {
    "custom".to_string()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AuthConfig {
    ApiKeyEnv { env: String },
    OpenaiOauth { account: Option<String> },
    AntigravityOauth { account: Option<String> },
    None,
}

impl AuthConfig {
    pub fn normalized_for_provider(&self, provider_id: &str) -> AuthConfig {
        match self {
            AuthConfig::OpenaiOauth { account } => AuthConfig::OpenaiOauth {
                account: Some(account.clone().unwrap_or_else(|| provider_id.to_string())),
            },
            AuthConfig::AntigravityOauth { account } => AuthConfig::AntigravityOauth {
                account: Some(account.clone().unwrap_or_else(|| provider_id.to_string())),
            },
            other => other.clone(),
        }
    }

    #[allow(dead_code)]
    pub fn api_key_env(&self) -> Option<&str> {
        match self {
            AuthConfig::ApiKeyEnv { env } => Some(env),
            _ => None,
        }
    }
}

/// A provider endpoint: exactly one of `url` (native) or `derive_from`
/// (derived) is set. There is deliberately no `kind` or `adapter` field —
/// kind is determined by which field is present, and the adapter is uniquely
/// determined by the (source protocol, target protocol) pair.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EndpointConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub derive_from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compat: Option<CompatConfig>,
    /// 该 endpoint 上游中走 Anthropic Messages 转换的模型族（glob 匹配 upstream_model，
    /// globset 语义：`*` 任意、`?` 单字符、字符类）。例：["claude-*", "gpt-oss-*"]
    /// 表示这些模型需要 functionCall/functionResponse id 映射 tool_use.id /
    /// tool_result.tool_use_id。空/缺省 = 全部 Gemini 原生语义。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub anthropic_family_models: Vec<String>,
    /// Responses/Chat-endpoint-level `store` declaration (protocol-specific;
    /// anthropic endpoints have no store). When set, overrides the client's
    /// value on outbound bodies; when unset, the runtime default applies
    /// (passthrough client value, else inject `false` — see research §3.1a).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub store: Option<bool>,
}

impl EndpointConfig {
    pub fn native(url: impl Into<String>) -> Self {
        Self {
            url: Some(url.into()),
            derive_from: None,
            compat: None,
            anthropic_family_models: Vec::new(),
            store: None,
        }
    }

    pub fn derived(source: Protocol) -> Self {
        Self {
            url: None,
            derive_from: Some(source.field_name().to_string()),
            compat: None,
            anthropic_family_models: Vec::new(),
            store: None,
        }
    }

    pub fn with_compat(mut self, compat: CompatConfig) -> Self {
        self.compat = Some(compat);
        self
    }
}

/// Build a GlobSet from `anthropic_family_models` patterns for endpoint-level
/// Anthropic-family detection. Returns None when empty (all models use
/// Gemini-native semantics). User-provided patterns are validated at config
/// load and catalog patterns are compile-time constants, so `Glob::new`
/// cannot fail here.
pub fn anthropic_family_glob_set(patterns: &[String]) -> Option<globset::GlobSet> {
    if patterns.is_empty() {
        return None;
    }
    let mut builder = globset::GlobSetBuilder::new();
    for pat in patterns {
        builder
            .add(globset::Glob::new(pat).expect(
                "anthropic_family_models patterns must be valid globs (validated at load)",
            ));
    }
    Some(
        builder
            .build()
            .expect("GlobSet build succeeds after Glob::new succeeds"),
    )
}

/// Upstream API compatibility quirks for a native endpoint. Absent fields
/// mean default behavior (standard OpenAI-compatible assumptions, see the
/// design doc §5.1).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CompatConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_developer_role: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_reasoning_effort: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_format: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requires_reasoning_content_on_assistant_messages: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens_field: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub force_stream: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strip_max_output_tokens: Option<bool>,
    /// Upstream hard-requires `store: false` (ChatGPT Codex backend).
    /// Forces `store: false` on outbound bodies, overriding endpoint config
    /// and client value (research §3.1a scenario 4).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub must_not_store: Option<bool>,
}

// Accessors encoding the §5.1 compat trust-contract defaults; consumed by the
// M4 egress adaptation (thinking injection, reasoning mapping, token field).
#[allow(dead_code)]
impl CompatConfig {
    pub fn effective_supports_developer_role(&self) -> bool {
        self.supports_developer_role.unwrap_or(true)
    }
    pub fn effective_supports_reasoning_effort(&self) -> bool {
        self.supports_reasoning_effort.unwrap_or(false)
    }
    pub fn effective_requires_reasoning_content_on_assistant_messages(&self) -> bool {
        self.requires_reasoning_content_on_assistant_messages
            .unwrap_or(false)
    }
    pub fn effective_max_tokens_field(&self) -> &str {
        self.max_tokens_field.as_deref().unwrap_or("max_tokens")
    }
    /// Upstream requires streaming: force `stream: true` on outbound body.
    pub fn effective_force_stream(&self) -> bool {
        self.force_stream.unwrap_or(false)
    }
    /// Upstream does not support `max_output_tokens`: strip it from outbound body.
    pub fn effective_strip_max_output_tokens(&self) -> bool {
        self.strip_max_output_tokens.unwrap_or(false)
    }
    /// Upstream hard-requires `store: false`: force it on outbound body.
    pub fn effective_must_not_store(&self) -> bool {
        self.must_not_store.unwrap_or(false)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReasoningLevelMapping {
    pub level: String,
    /// `null` means thinking is disabled at this level.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_value: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Protocol {
    #[serde(rename = "openai-chat", alias = "openai-chat-completions")]
    OpenaiChatCompletions,
    #[serde(rename = "openai-responses", alias = "responses")]
    OpenaiResponses,
    #[serde(rename = "anthropic")]
    Anthropic,
    #[serde(rename = "antigravity")]
    Antigravity,
}

impl Protocol {
    /// The three protocols a local client can speak to llm-proxy.
    pub const CLIENT_PROTOCOLS: [Protocol; 3] = [
        Protocol::OpenaiChatCompletions,
        Protocol::OpenaiResponses,
        Protocol::Anthropic,
    ];

    /// Provider endpoint field name for this protocol.
    pub fn field_name(self) -> &'static str {
        match self {
            Protocol::OpenaiChatCompletions => "openai_chat",
            Protocol::OpenaiResponses => "openai_responses",
            Protocol::Anthropic => "anthropic",
            Protocol::Antigravity => "antigravity",
        }
    }

    pub fn from_field_name(name: &str) -> Option<Protocol> {
        match name {
            "openai_chat" => Some(Protocol::OpenaiChatCompletions),
            "openai_responses" => Some(Protocol::OpenaiResponses),
            "anthropic" => Some(Protocol::Anthropic),
            "antigravity" => Some(Protocol::Antigravity),
            _ => None,
        }
    }

    /// Stable key used in cooldown state and status output.
    pub fn route_key(self) -> &'static str {
        match self {
            Protocol::OpenaiChatCompletions => "chat_completions",
            Protocol::OpenaiResponses => "responses",
            Protocol::Anthropic => "anthropic",
            Protocol::Antigravity => "antigravity",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AdapterKind {
    /// Entry endpoint is native for the requested protocol: no conversion.
    Passthrough,
    ResponsesFromChatCompletions,
    ChatCompletionsFromResponses,
    AnthropicFromChatCompletions,
    ChatCompletionsFromAnthropic,
    ResponsesFromAnthropic,
    AnthropicFromResponses,
    ResponsesFromAntigravity,
    AnthropicFromAntigravity,
}

/// Adapter registry: exactly one adapter per (source, target) protocol pair.
/// Derived endpoints select their conversion through this registry alone.
pub fn adapter_for_pair(source: Protocol, target: Protocol) -> Option<AdapterKind> {
    match (source, target) {
        (Protocol::OpenaiChatCompletions, Protocol::OpenaiResponses) => {
            Some(AdapterKind::ResponsesFromChatCompletions)
        }
        (Protocol::OpenaiResponses, Protocol::OpenaiChatCompletions) => {
            Some(AdapterKind::ChatCompletionsFromResponses)
        }
        (Protocol::OpenaiChatCompletions, Protocol::Anthropic) => {
            Some(AdapterKind::AnthropicFromChatCompletions)
        }
        (Protocol::Anthropic, Protocol::OpenaiChatCompletions) => {
            Some(AdapterKind::ChatCompletionsFromAnthropic)
        }
        (Protocol::Anthropic, Protocol::OpenaiResponses) => {
            Some(AdapterKind::ResponsesFromAnthropic)
        }
        (Protocol::OpenaiResponses, Protocol::Anthropic) => {
            Some(AdapterKind::AnthropicFromResponses)
        }
        (Protocol::Antigravity, Protocol::OpenaiResponses) => {
            Some(AdapterKind::ResponsesFromAntigravity)
        }
        (Protocol::Antigravity, Protocol::Anthropic) => Some(AdapterKind::AnthropicFromAntigravity),
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestFrequencyConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requests_per_minute: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requests_per_hour: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub burst: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue_timeout_seconds: Option<u64>,
}

impl Default for RequestFrequencyConfig {
    fn default() -> Self {
        Self {
            requests_per_minute: Some(60),
            requests_per_hour: None,
            burst: Some(5),
            queue_timeout_seconds: Some(10),
        }
    }
}

impl RequestFrequencyConfig {
    pub fn effective(provider: &ProviderConfig) -> Self {
        let configured = provider.request_frequency.clone().unwrap_or_default();
        Self {
            requests_per_minute: configured.requests_per_minute.or(Some(60)),
            requests_per_hour: configured.requests_per_hour,
            burst: configured.burst.or(Some(5)),
            queue_timeout_seconds: configured.queue_timeout_seconds.or(Some(10)),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub context_window: i64,
    pub max_output_tokens: i64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub features: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub supported_reasoning_levels: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_reasoning_level: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enable_thinking: Option<bool>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub openai_chat_providers: Vec<ProviderBinding>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub openai_responses_providers: Vec<ProviderBinding>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub anthropic_providers: Vec<ProviderBinding>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_level_map: Option<Vec<ReasoningLevelMapping>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderBinding {
    pub name: String,
    pub model: String,
}

impl ModelConfig {
    pub fn provider_bindings(&self, protocol: Protocol) -> &[ProviderBinding] {
        match protocol {
            Protocol::OpenaiChatCompletions => &self.openai_chat_providers,
            Protocol::OpenaiResponses => &self.openai_responses_providers,
            Protocol::Anthropic => &self.anthropic_providers,
            Protocol::Antigravity => &[],
        }
    }

    pub fn exposes_protocol(&self, protocol: Protocol) -> bool {
        !self.provider_bindings(protocol).is_empty()
    }

    /// Protocols this model can serve, for error messages.
    pub fn supported_protocols(&self) -> Vec<Protocol> {
        Protocol::CLIENT_PROTOCOLS
            .into_iter()
            .filter(|protocol| self.exposes_protocol(*protocol))
            .collect()
    }
}

impl ProviderConfig {
    pub fn auth_config(&self, provider_id: &str) -> Result<AuthConfig> {
        match (&self.api_key_env, &self.auth) {
            (Some(env), None) => Ok(AuthConfig::ApiKeyEnv { env: env.clone() }),
            (None, Some(auth)) => Ok(auth.normalized_for_provider(provider_id)),
            (None, None) => Ok(AuthConfig::None),
            (Some(env), Some(AuthConfig::ApiKeyEnv { env: auth_env })) if env == auth_env => {
                Ok(AuthConfig::ApiKeyEnv { env: env.clone() })
            }
            (Some(_), Some(_)) => bail!(
                "provider {provider_id} configures both api_key_env and auth; only matching api_key_env forms may be duplicated"
            ),
        }
    }

    pub fn endpoint(&self, protocol: Protocol) -> Option<&EndpointConfig> {
        match protocol {
            Protocol::OpenaiChatCompletions => self.openai_chat.as_ref(),
            Protocol::OpenaiResponses => self.openai_responses.as_ref(),
            Protocol::Anthropic => self.anthropic.as_ref(),
            Protocol::Antigravity => self.antigravity.as_ref(),
        }
    }

    pub fn set_endpoint(&mut self, protocol: Protocol, endpoint: EndpointConfig) {
        match protocol {
            Protocol::OpenaiChatCompletions => self.openai_chat = Some(endpoint),
            Protocol::OpenaiResponses => self.openai_responses = Some(endpoint),
            Protocol::Anthropic => self.anthropic = Some(endpoint),
            Protocol::Antigravity => self.antigravity = Some(endpoint),
        }
    }

    pub fn endpoints(&self) -> Vec<(Protocol, &EndpointConfig)> {
        let mut out = Vec::new();
        for protocol in [
            Protocol::OpenaiChatCompletions,
            Protocol::OpenaiResponses,
            Protocol::Anthropic,
            Protocol::Antigravity,
        ] {
            if let Some(endpoint) = self.endpoint(protocol) {
                out.push((protocol, endpoint));
            }
        }
        out
    }

    /// Returns true if this provider belongs to no specific product
    /// (product is empty or "custom"). Custom providers do not participate
    /// in product-level batch fallback.
    pub fn is_custom_product(&self) -> bool {
        self.product.is_empty() || self.product == "custom"
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedAuth {
    pub token: Option<String>,
    pub project_id: Option<String>,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text = fs::read_to_string(path)
            .with_context(|| format!("failed to read config {}", path.display()))?;
        let cfg: Self = toml::from_str(&text)
            .with_context(|| format!("failed to parse config {}", path.display()))?;
        let cfg = cfg.migrate_provider_ids()?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// 迁移历史 provider ID → 当前 ID（breaking change 兼容）。
    /// 只影响配置中显式存在的旧 ID；新配置无需迁移。
    fn migrate_provider_ids(mut self) -> Result<Self> {
        let rename_map: &[(&str, &str)] = &[
            // 智谱国际版品牌名从 Z.ai 统一为 Zhipu（2026-08 命名规范化）
            ("zai-payg-global", "zhipu-payg-global"),
            ("zhipu-coding-plan-cn", "zhipu-coding-cn"),
        ];
        let mut renamed = false;
        for (old, new) in rename_map {
            if self.providers.contains_key(*old)
                && !self.providers.contains_key(*new)
                && let Some(provider) = self.providers.remove(*old)
            {
                self.providers.insert(new.to_string(), provider);
                renamed = true;
            }
        }
        if renamed {
            // 同步更新 model 中的 provider binding 引用
            for (_, model) in self.models.iter_mut() {
                for protocol in Protocol::CLIENT_PROTOCOLS {
                    let bindings = match protocol {
                        Protocol::OpenaiChatCompletions => &mut model.openai_chat_providers,
                        Protocol::OpenaiResponses => &mut model.openai_responses_providers,
                        Protocol::Anthropic => &mut model.anthropic_providers,
                        Protocol::Antigravity => continue,
                    };
                    for binding in bindings.iter_mut() {
                        for (old, new) in rename_map {
                            if binding.name == *old {
                                binding.name = new.to_string();
                            }
                        }
                    }
                }
            }
        }
        Ok(self)
    }

    pub fn validate(&self) -> Result<()> {
        for (provider_id, provider) in &self.providers {
            validate_provider(provider_id, provider)?;
        }
        for (model_id, model) in &self.models {
            validate_model_reasoning(model_id, model)?;
            self.validate_model_bindings(model_id, model)?;
        }
        Ok(())
    }

    fn validate_model_bindings(&self, model_id: &str, model: &ModelConfig) -> Result<()> {
        for protocol in Protocol::CLIENT_PROTOCOLS {
            let mut seen = std::collections::BTreeSet::new();
            for binding in model.provider_bindings(protocol) {
                if binding.name.is_empty() || binding.model.is_empty() {
                    bail!("model {model_id} has an empty provider binding for {protocol:?}");
                }
                if !seen.insert(binding.name.clone()) {
                    bail!(
                        "model {model_id} repeats provider {} for {:?}",
                        binding.name,
                        protocol
                    );
                }
                let provider = self.providers.get(&binding.name).with_context(|| {
                    format!(
                        "model {model_id} references unknown provider {}",
                        binding.name
                    )
                })?;
                if provider.endpoint(protocol).is_none() {
                    bail!(
                        "model {model_id} binds provider {} for {:?}, but provider does not declare a same-protocol endpoint ({} field)",
                        binding.name,
                        protocol,
                        protocol.field_name()
                    );
                }
            }
        }
        Ok(())
    }

    pub fn resolve_model_request(
        &self,
        protocol: Protocol,
        requested: &str,
    ) -> Option<(String, ExecutionPlan)> {
        self.resolve_model_request_candidates(protocol, requested)
            .into_iter()
            .next()
    }

    /// Resolve a requested model ID to candidate execution plans in binding
    /// order. Unknown models and models without a binding for the protocol
    /// both return an empty list — the caller maps that to an explicit 4xx;
    /// there is no default-model substitution.
    pub fn resolve_model_request_candidates(
        &self,
        protocol: Protocol,
        requested: &str,
    ) -> Vec<(String, ExecutionPlan)> {
        let Some(model) = self.models.get(requested) else {
            return Vec::new();
        };
        model
            .provider_bindings(protocol)
            .iter()
            .filter_map(|binding| {
                self.resolve_binding(protocol, binding)
                    .ok()
                    .map(|plan| (requested.to_string(), plan))
            })
            .collect()
    }

    fn resolve_binding(
        &self,
        protocol: Protocol,
        binding: &ProviderBinding,
    ) -> Result<ExecutionPlan> {
        let provider = self
            .providers
            .get(&binding.name)
            .with_context(|| format!("unknown provider {}", binding.name))?;
        let endpoint = provider
            .endpoint(protocol)
            .with_context(|| format!("provider {} has no {:?} endpoint", binding.name, protocol))?;
        let (source_protocol, adapter, native_endpoint) =
            resolve_endpoint(provider, protocol, endpoint).with_context(|| {
                format!(
                    "provider {} {:?} endpoint is not resolvable",
                    binding.name, protocol
                )
            })?;
        let native_url = native_endpoint.url.as_deref().with_context(|| {
            format!(
                "provider {} native {:?} endpoint has no url",
                binding.name, source_protocol
            )
        })?;
        Ok(ExecutionPlan {
            frontend_protocol: protocol,
            provider_id: binding.name.clone(),
            upstream_model: binding.model.clone(),
            source_protocol,
            adapter,
            native_url: resolve_env_reference(native_url),
            auth: provider.auth_config(&binding.name)?,
            compat: native_endpoint.compat.clone().unwrap_or_default(),
            anthropic_family_models: native_endpoint.anthropic_family_models.clone(),
            store: native_endpoint.store,
            request_frequency: RequestFrequencyConfig::effective(provider),
        })
    }

    /// First model (ID order) that can serve the protocol. Used by launch
    /// commands that need a default model selection.
    pub fn default_model_for(&self, protocol: Protocol) -> Option<(&str, &ModelConfig)> {
        self.models
            .iter()
            .find(|(_, model)| model.exposes_protocol(protocol))
            .map(|(id, model)| (id.as_str(), model))
    }

    pub fn resolve_auth(&self, provider_id: &str) -> Result<ResolvedAuth> {
        let provider = self
            .providers
            .get(provider_id)
            .with_context(|| format!("unknown provider {provider_id}"))?;
        match provider.auth_config(provider_id)? {
            AuthConfig::ApiKeyEnv { env } => Ok(ResolvedAuth {
                token: std::env::var(&env).ok().filter(|value| !value.is_empty()),
                project_id: None,
            }),
            AuthConfig::OpenaiOauth { account } => {
                let account_id = account.unwrap_or_else(|| provider_id.to_string());
                let token = crate::auth::get_openai_token(
                    &crate::auth::default_state_path(),
                    &account_id,
                    provider_id,
                )?;
                Ok(ResolvedAuth {
                    token: Some(token),
                    project_id: None,
                })
            }
            AuthConfig::AntigravityOauth { account } => {
                let account_id = account.unwrap_or_else(|| provider_id.to_string());
                let token = crate::auth::get_antigravity_token(
                    &crate::auth::default_state_path(),
                    &account_id,
                    provider_id,
                )?;
                // 从 accounts 中获取 project_id
                let (accounts, _skipped) =
                    crate::auth::load_oauth_accounts(&crate::auth::default_state_path())?;
                let project_id = accounts
                    .antigravity
                    .get(&account_id)
                    .map(|a| Some(a.project_id.clone()))
                    .unwrap_or(None);
                Ok(ResolvedAuth {
                    token: Some(token),
                    project_id,
                })
            }
            AuthConfig::None => Ok(ResolvedAuth {
                token: None,
                project_id: None,
            }),
        }
    }

    pub fn auth_token(&self, provider_id: &str) -> Result<Option<String>> {
        Ok(self.resolve_auth(provider_id)?.token)
    }
}

/// Resolve an entry endpoint to its native source: returns the source
/// protocol, the adapter to run (Passthrough for native entry endpoints), and
/// the native endpoint config. Single hop only — validation guarantees
/// `derive_from` targets are native.
fn resolve_endpoint<'a>(
    provider: &'a ProviderConfig,
    protocol: Protocol,
    endpoint: &'a EndpointConfig,
) -> Result<(Protocol, AdapterKind, &'a EndpointConfig)> {
    if endpoint.url.is_some() {
        return Ok((protocol, AdapterKind::Passthrough, endpoint));
    }
    let source_name = endpoint.derive_from.as_deref().with_context(|| {
        format!(
            "{:?} endpoint configures neither url nor derive_from",
            protocol
        )
    })?;
    let source_protocol = Protocol::from_field_name(source_name)
        .with_context(|| format!("unknown derive_from protocol field {source_name:?}"))?;
    let adapter = adapter_for_pair(source_protocol, protocol).with_context(|| {
        format!("no adapter registered for {source_protocol:?} -> {protocol:?}")
    })?;
    let source = provider
        .endpoint(source_protocol)
        .with_context(|| format!("derive_from source {source_name:?} is not declared"))?;
    if source.url.is_none() {
        bail!("derive_from source {source_name:?} is not a native endpoint");
    }
    Ok((source_protocol, adapter, source))
}

fn validate_provider(provider_id: &str, provider: &ProviderConfig) -> Result<()> {
    validate_provider_auth(provider_id, provider)?;
    if provider.endpoints().is_empty() {
        bail!("provider {provider_id} must declare at least one protocol endpoint field");
    }
    if let Some(map) = &provider.reasoning_level_map {
        validate_reasoning_level_map(map)
            .with_context(|| format!("provider {provider_id} reasoning_level_map"))?;
    }
    if let Some(freq) = &provider.request_frequency {
        validate_request_frequency(provider_id, freq)?;
    }
    if let Some(antigravity) = &provider.antigravity
        && antigravity.derive_from.is_some()
    {
        bail!(
            "provider {provider_id} antigravity endpoint must be native (derive_from is not allowed)"
        );
    }
    for (protocol, endpoint) in provider.endpoints() {
        validate_anthropic_family_patterns(provider_id, protocol, endpoint)?;
        match (&endpoint.url, &endpoint.derive_from) {
            (Some(_), Some(_)) => bail!(
                "provider {provider_id} {:?} endpoint configures both url and derive_from; exactly one is allowed",
                protocol
            ),
            (None, None) => bail!(
                "provider {provider_id} {:?} endpoint configures neither url nor derive_from",
                protocol
            ),
            (Some(url), None) => validate_native_endpoint(provider_id, protocol, url, endpoint)?,
            (None, Some(source_name)) => {
                validate_derived_endpoint(provider_id, provider, protocol, endpoint, source_name)?
            }
        }
    }
    Ok(())
}

fn validate_request_frequency(provider_id: &str, freq: &RequestFrequencyConfig) -> Result<()> {
    if matches!(freq.requests_per_minute, Some(0)) {
        bail!("provider {provider_id} request_frequency.requests_per_minute must be > 0");
    }
    if matches!(freq.requests_per_hour, Some(0)) {
        bail!("provider {provider_id} request_frequency.requests_per_hour must be > 0");
    }
    if matches!(freq.burst, Some(0)) {
        bail!("provider {provider_id} request_frequency.burst must be > 0");
    }
    if matches!(freq.queue_timeout_seconds, Some(0)) {
        bail!("provider {provider_id} request_frequency.queue_timeout_seconds must be > 0");
    }
    Ok(())
}

fn validate_provider_auth(provider_id: &str, provider: &ProviderConfig) -> Result<()> {
    let auth = provider.auth_config(provider_id)?;
    match auth {
        AuthConfig::ApiKeyEnv { env } => {
            if env.trim().is_empty() {
                bail!("provider {provider_id} api_key_env/auth env must not be empty");
            }
        }
        AuthConfig::OpenaiOauth { account } | AuthConfig::AntigravityOauth { account } => {
            if account.as_deref().unwrap_or_default().trim().is_empty() {
                bail!("provider {provider_id} OAuth account must not be empty");
            }
        }
        AuthConfig::None => {}
    }
    Ok(())
}

fn validate_anthropic_family_patterns(
    provider_id: &str,
    protocol: Protocol,
    endpoint: &EndpointConfig,
) -> Result<()> {
    for pat in &endpoint.anthropic_family_models {
        globset::Glob::new(pat).map_err(|e| {
            anyhow::anyhow!(
                "provider {provider_id} {:?} endpoint anthropic_family_models pattern {pat:?} is not a valid glob: {e}",
                protocol
            )
        })?;
    }
    Ok(())
}

fn validate_native_endpoint(
    provider_id: &str,
    protocol: Protocol,
    raw_url: &str,
    endpoint: &EndpointConfig,
) -> Result<()> {
    if let Some(compat) = &endpoint.compat {
        validate_compat(provider_id, protocol, compat)?;
    }
    let resolved = resolve_env_reference(raw_url);
    let parsed = url::Url::parse(&resolved).with_context(|| {
        format!(
            "provider {provider_id} {:?} endpoint url {raw_url:?} is not a valid absolute URL",
            protocol
        )
    })?;
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        bail!(
            "provider {provider_id} {:?} endpoint url must use http or https: {raw_url:?}",
            protocol
        );
    }
    Ok(())
}

fn validate_compat(provider_id: &str, protocol: Protocol, compat: &CompatConfig) -> Result<()> {
    if let Some(value) = compat.thinking_format.as_deref() {
        let valid = matches!(
            value,
            "deepseek"
                | "stepfun"
                | "qwen"
                | "qwen-enable-thinking"
                | "qwen-chat-template"
                | "qwen-chat-template-kwargs"
                | "zai"
                | "zhipu_thinking"
                | "mimo-thinking-toggle"
                | "anthropic-thinking"
                | "openai-responses-reasoning"
                | "openrouter-chat-reasoning-details"
                | "reasoning_details"
                | "openrouter-responses-reasoning"
                | "openrouter-anthropic-messages"
                | "gemini-thinking-config"
        );
        if !valid {
            bail!(
                "provider {provider_id} {:?} compat.thinking_format has unknown value {value:?}",
                protocol
            );
        }
    }
    if let Some(value @ ("max_tokens" | "max_completion_tokens" | "max_output_tokens")) =
        compat.max_tokens_field.as_deref()
    {
        // valid
        let _ = value;
    } else if let Some(value) = compat.max_tokens_field.as_deref() {
        bail!(
            "provider {provider_id} {:?} compat.max_tokens_field has unknown value {value:?}",
            protocol
        );
    }
    Ok(())
}

fn validate_derived_endpoint(
    provider_id: &str,
    provider: &ProviderConfig,
    protocol: Protocol,
    endpoint: &EndpointConfig,
    source_name: &str,
) -> Result<()> {
    if endpoint.compat.is_some() {
        bail!(
            "provider {provider_id} derived {:?} endpoint must not configure compat (inherited from the native source endpoint)",
            protocol
        );
    }
    let source_protocol = Protocol::from_field_name(source_name).with_context(|| {
        format!(
            "provider {provider_id} derived {:?} endpoint has unknown derive_from field {source_name:?}; expected one of openai_chat, openai_responses, anthropic, antigravity",
            protocol
        )
    })?;
    if source_protocol == protocol {
        bail!(
            "provider {provider_id} {:?} endpoint cannot derive from itself",
            protocol
        );
    }
    let source = provider.endpoint(source_protocol).with_context(|| {
        format!(
            "provider {provider_id} derived {:?} endpoint references missing derive_from target {source_name:?}",
            protocol
        )
    })?;
    if source.url.is_none() {
        bail!(
            "provider {provider_id} derived {:?} endpoint must reference a native endpoint; {source_name:?} is itself derived (multi-hop chains are not allowed)",
            protocol
        );
    }
    if adapter_for_pair(source_protocol, protocol).is_none() {
        bail!(
            "provider {provider_id} derived {:?} endpoint from {source_name:?}: no adapter registered for {source_protocol:?} -> {protocol:?}",
            protocol
        );
    }
    Ok(())
}

fn validate_model_reasoning(model_id: &str, model: &ModelConfig) -> Result<()> {
    let mut seen = std::collections::BTreeSet::new();
    for level in &model.supported_reasoning_levels {
        if level.is_empty() {
            bail!("model {model_id} supported_reasoning_levels contains an empty level");
        }
        if !seen.insert(level.clone()) {
            bail!("model {model_id} supported_reasoning_levels repeats level {level:?}");
        }
    }
    if let Some(default) = &model.default_reasoning_level
        && !model.supported_reasoning_levels.is_empty()
        && !model.supported_reasoning_levels.contains(default)
    {
        bail!(
            "model {model_id} default_reasoning_level {default:?} is not in supported_reasoning_levels"
        );
    }
    if let Some(map) = &model.reasoning_level_map {
        validate_reasoning_level_map(map)
            .map_err(|err| anyhow::anyhow!("model {model_id} reasoning_level_map {err}"))?;
    }
    Ok(())
}

fn validate_reasoning_level_map(mappings: &[ReasoningLevelMapping]) -> Result<()> {
    let mut seen = std::collections::BTreeSet::new();
    for mapping in mappings {
        if mapping.level.is_empty() {
            bail!("contains an empty level");
        }
        if !seen.insert(mapping.level.clone()) {
            bail!("repeats level {:?}", mapping.level);
        }
    }
    Ok(())
}

/// `${VAR}` / `${VAR}|default` environment reference resolution for endpoint
/// URLs. Plain strings pass through unchanged.
pub fn resolve_env_reference(raw: &str) -> String {
    let Some(rest) = raw.strip_prefix("${") else {
        return raw.to_string();
    };
    let Some(close) = rest.find('}') else {
        return raw.to_string();
    };
    let var = &rest[..close];
    let default = rest[close + 1..].strip_prefix('|');
    match std::env::var(var).ok().filter(|value| !value.is_empty()) {
        Some(value) => value,
        None => default.unwrap_or_default().to_string(),
    }
}

pub fn local_server_base_url(listen: &str) -> String {
    let host_port = if let Some(port) = listen.strip_prefix("0.0.0.0:") {
        format!("127.0.0.1:{port}")
    } else if let Some(port) = listen.strip_prefix("[::]:") {
        format!("127.0.0.1:{port}")
    } else {
        listen.to_string()
    };
    format!("http://{host_port}")
}

pub fn local_protocol_base_url(listen: &str, prefix: &str) -> String {
    format!(
        "{}/{}",
        local_server_base_url(listen).trim_end_matches('/'),
        prefix.trim_start_matches('/')
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CooldownKey {
    pub model_id: String,
    pub provider_id: String,
    pub protocol: Protocol,
}

impl CooldownKey {
    pub fn stable_id(&self) -> String {
        format!(
            "{}:{}:{}",
            self.model_id,
            self.provider_id,
            self.protocol.route_key()
        )
    }
}

#[derive(Debug, Clone)]
pub struct ExecutionPlan {
    pub frontend_protocol: Protocol,
    pub provider_id: String,
    pub upstream_model: String,
    /// Protocol of the native endpoint that will actually be contacted.
    #[allow(dead_code)] // read by adapter dispatch tests; M4 will branch on it
    pub source_protocol: Protocol,
    /// Passthrough when the entry endpoint is native for the frontend protocol.
    pub adapter: AdapterKind,
    pub native_url: String,
    pub auth: AuthConfig,
    /// Effective compat of the target native endpoint.
    #[allow(dead_code)] // M4 egress adaptation input (design §5.1)
    pub compat: CompatConfig,
    /// 目标 native endpoint 中走 Anthropic Messages 转换的模型族（glob 匹配
    /// upstream_model）。转换器据此决定 functionCall/functionResponse 是否带 id。
    pub anthropic_family_models: Vec<String>,
    /// Effective `store` declaration of the target native endpoint (research §3.1a).
    #[allow(dead_code)] // M4 egress adaptation input
    pub store: Option<bool>,
    pub request_frequency: RequestFrequencyConfig,
}

impl ExecutionPlan {
    pub fn adapter(&self) -> AdapterKind {
        self.adapter
    }

    pub fn cooldown_key(&self, frontend_model_id: &str) -> CooldownKey {
        CooldownKey {
            model_id: frontend_model_id.to_string(),
            provider_id: self.provider_id.clone(),
            protocol: self.frontend_protocol,
        }
    }
}

pub fn default_config_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".config")
        .join("llm-proxy")
        .join("config.toml")
}

pub fn init_config(path: &Path) -> Result<()> {
    if path.exists() {
        println!("config already exists: {}", path.display());
        return Ok(());
    }

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create config dir {}", parent.display()))?;
    }

    let cfg = default_deepseek_config();
    let text = toml::to_string_pretty(&cfg).context("failed to serialize default config")?;
    crate::config_edit::atomic_write(path, text.as_bytes())?;
    println!("config created: {}", path.display());
    Ok(())
}

pub fn deepseek_reasoning_level_map() -> Vec<ReasoningLevelMapping> {
    vec![
        ReasoningLevelMapping {
            level: "minimal".to_string(),
            api_value: Some("high".to_string()),
        },
        ReasoningLevelMapping {
            level: "low".to_string(),
            api_value: Some("high".to_string()),
        },
        ReasoningLevelMapping {
            level: "medium".to_string(),
            api_value: Some("high".to_string()),
        },
        ReasoningLevelMapping {
            level: "high".to_string(),
            api_value: Some("high".to_string()),
        },
        ReasoningLevelMapping {
            level: "xhigh".to_string(),
            api_value: Some("max".to_string()),
        },
    ]
}

pub fn default_deepseek_config() -> Config {
    let mut providers = BTreeMap::new();
    let deepseek = crate::catalog::deepseek();
    providers.insert(deepseek.id.to_string(), deepseek.provider);

    let mut models = BTreeMap::new();
    for (frontend_id, upstream_model) in [
        ("deepseek-v4-flash-lp", "deepseek-v4-flash"),
        ("deepseek-v4-pro-lp", "deepseek-v4-pro"),
    ] {
        let binding = || ProviderBinding {
            name: "deepseek".to_string(),
            model: upstream_model.to_string(),
        };
        models.insert(
            frontend_id.to_string(),
            ModelConfig {
                description: None,
                context_window: 1_000_000,
                max_output_tokens: 393_216,
                features: vec!["tool_call_reasoning".to_string()],
                supported_reasoning_levels: vec![
                    "low".to_string(),
                    "medium".to_string(),
                    "high".to_string(),
                    "xhigh".to_string(),
                ],
                default_reasoning_level: Some("high".to_string()),
                enable_thinking: Some(true),
                openai_chat_providers: vec![binding()],
                openai_responses_providers: vec![binding()],
                anthropic_providers: vec![binding()],
                reasoning_level_map: Some(deepseek_reasoning_level_map()),
            },
        );
    }

    Config {
        server: ServerConfig {
            listen: "127.0.0.1:8989".to_string(),
            usage: UsageConfig::default(),
            max_sse_buffer_bytes: default_max_sse_buffer_bytes(),
            max_output_items: default_max_output_items(),
        },
        fallback: FallbackConfig::default(),
        protection: ProtectionConfig::default(),
        status: StatusConfig::default(),
        providers,
        models,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID_CONFIG: &str = r#"
[server]
listen = "127.0.0.1:8989"

[providers.provider-a]
api_key_env = "PROVIDER_A_API_KEY"

[providers.provider-a.openai_chat]
url = "https://api.example.com/v1/chat/completions"

[providers.provider-a.anthropic]
derive_from = "openai_chat"

[models.model-a]
context_window = 200000
max_output_tokens = 8192
openai_chat_providers = [{ name = "provider-a", model = "upstream-a" }]
anthropic_providers = [{ name = "provider-a", model = "upstream-a" }]
"#;

    #[test]
    fn provider_request_frequency_validates_and_defaults() {
        let cfg: Config = toml::from_str(
            r#"
[server]
listen = "127.0.0.1:8989"

[providers.provider-a]
api_key_env = "KEY"

[providers.provider-a.request_frequency]
requests_per_minute = 30
burst = 2

[providers.provider-a.openai_chat]
url = "https://api.example.com/v1/chat/completions"

[models.model-a]
context_window = 100
max_output_tokens = 10
openai_chat_providers = [{ name = "provider-a", model = "upstream" }]
"#,
        )
        .expect("parse");
        cfg.validate().expect("validate");
        let (_id, plan) = cfg
            .resolve_model_request(Protocol::OpenaiChatCompletions, "model-a")
            .expect("resolve");
        assert_eq!(plan.request_frequency.requests_per_minute, Some(30));
        assert_eq!(plan.request_frequency.requests_per_hour, None);
        assert_eq!(plan.request_frequency.burst, Some(2));
        assert_eq!(plan.request_frequency.queue_timeout_seconds, Some(10));
    }

    #[test]
    fn provider_request_frequency_rejects_zero_values() {
        let cfg: Config = toml::from_str(
            r#"
[server]
listen = "127.0.0.1:8989"

[providers.provider-a]
api_key_env = "KEY"

[providers.provider-a.request_frequency]
requests_per_minute = 0

[providers.provider-a.openai_chat]
url = "https://api.example.com/v1/chat/completions"
"#,
        )
        .expect("parse");
        let err = cfg.validate().expect_err("zero rejected");
        assert!(err.to_string().contains("requests_per_minute"));
    }

    #[test]
    fn provider_auth_normalizes_api_key_and_oauth_forms() {
        let api_key: ProviderConfig = toml::from_str(
            r#"api_key_env = "DEEPSEEK_API_KEY"
[openai_chat]
url = "https://api.example.com/v1/chat/completions"
"#,
        )
        .expect("parse api key provider");
        assert_eq!(
            api_key.auth_config("deepseek").expect("auth"),
            AuthConfig::ApiKeyEnv {
                env: "DEEPSEEK_API_KEY".to_string()
            }
        );

        let oauth: ProviderConfig = toml::from_str(
            r#"auth = { type = "openai_oauth" }
[openai_responses]
url = "https://chatgpt.com/backend-api/codex/responses"
"#,
        )
        .expect("parse oauth provider");
        assert_eq!(
            oauth.auth_config("openai-subscription").expect("auth"),
            AuthConfig::OpenaiOauth {
                account: Some("openai-subscription".to_string())
            }
        );
    }

    #[test]
    fn resolved_auth_struct_can_carry_antigravity_project_id() {
        let auth = ResolvedAuth {
            token: Some("token".to_string()),
            project_id: Some("project".to_string()),
        };
        assert_eq!(auth.token.as_deref(), Some("token"));
        assert_eq!(auth.project_id.as_deref(), Some("project"));
    }

    #[test]
    fn oauth_provider_config_validates_without_token_store_account() {
        let cfg: Config = toml::from_str(
            r#"[server]
listen = "127.0.0.1:8989"

[providers.openai-subscription]
auth = { type = "openai_oauth", account = "openai-subscription" }

[providers.openai-subscription.openai_responses]
url = "https://chatgpt.com/backend-api/codex/responses"

[models."gpt-5.5-openai-subscription-lp"]
context_window = 400000
max_output_tokens = 128000
features = ["tools"]
openai_responses_providers = [{ name = "openai-subscription", model = "gpt-5.5" }]
"#,
        )
        .expect("parse");
        cfg.validate()
            .expect("OAuth provider config validates without reading token store");
        let (_id, plan) = cfg
            .resolve_model_request(Protocol::OpenaiResponses, "gpt-5.5-openai-subscription-lp")
            .expect("resolve");
        assert_eq!(
            plan.auth,
            AuthConfig::OpenaiOauth {
                account: Some("openai-subscription".to_string())
            }
        );
    }

    #[test]
    fn provider_auth_rejects_conflicting_forms() {
        let provider: ProviderConfig = toml::from_str(
            r#"api_key_env = "A"
auth = { type = "api_key_env", env = "B" }
[openai_chat]
url = "https://api.example.com/v1/chat/completions"
"#,
        )
        .expect("parse provider");
        let err = validate_provider("bad", &provider).expect_err("conflict");
        assert!(err.to_string().contains("both api_key_env and auth"));
    }

    #[test]
    fn default_config_validates_and_resolves_derived_responses() {
        let cfg = default_deepseek_config();
        cfg.validate().expect("default config validates");

        let (_id, plan) = cfg
            .resolve_model_request(Protocol::OpenaiResponses, "deepseek-v4-flash-lp")
            .expect("resolve responses");
        assert_eq!(plan.adapter(), AdapterKind::ResponsesFromChatCompletions);
        assert_eq!(plan.source_protocol, Protocol::OpenaiChatCompletions);
        assert_eq!(plan.native_url, "https://api.deepseek.com/chat/completions");

        let (_id, plan) = cfg
            .resolve_model_request(Protocol::Anthropic, "deepseek-v4-flash-lp")
            .expect("resolve anthropic");
        assert_eq!(plan.adapter(), AdapterKind::Passthrough);
        assert_eq!(
            plan.native_url,
            "https://api.deepseek.com/anthropic/v1/messages"
        );
    }

    #[test]
    fn valid_native_and_derived_config_passes_validation() {
        let cfg: Config = toml::from_str(VALID_CONFIG).expect("parse");
        cfg.validate().expect("valid config");

        let (_id, plan) = cfg
            .resolve_model_request(Protocol::Anthropic, "model-a")
            .expect("resolve");
        assert_eq!(plan.adapter(), AdapterKind::AnthropicFromChatCompletions);
        assert_eq!(plan.source_protocol, Protocol::OpenaiChatCompletions);
        assert_eq!(
            plan.native_url,
            "https://api.example.com/v1/chat/completions"
        );
    }

    #[test]
    fn binding_without_same_protocol_endpoint_fails() {
        let text = VALID_CONFIG.replace(
            "[providers.provider-a.anthropic]\nderive_from = \"openai_chat\"\n",
            "",
        );
        let cfg: Config = toml::from_str(&text).expect("parse");
        let err = cfg.validate().expect_err("must fail");
        assert!(
            err.to_string()
                .contains("does not declare a same-protocol endpoint")
        );
    }

    #[test]
    fn endpoint_with_both_url_and_derive_from_fails() {
        let text = VALID_CONFIG.replace(
            "[providers.provider-a.anthropic]\nderive_from = \"openai_chat\"",
            "[providers.provider-a.anthropic]\nurl = \"https://api.example.com/messages\"\nderive_from = \"openai_chat\"",
        );
        let cfg: Config = toml::from_str(&text).expect("parse");
        let err = cfg.validate().expect_err("must fail");
        assert!(err.to_string().contains("both url and derive_from"));
    }

    #[test]
    fn endpoint_with_neither_url_nor_derive_from_fails() {
        let text = VALID_CONFIG.replace(
            "derive_from = \"openai_chat\"\n\n[models.model-a]",
            "\n[models.model-a]",
        );
        let cfg: Config = toml::from_str(&text).expect("parse");
        let err = cfg.validate().expect_err("must fail");
        assert!(err.to_string().contains("neither url nor derive_from"));
    }

    #[test]
    fn derive_from_missing_target_fails() {
        let text = VALID_CONFIG.replace(
            "derive_from = \"openai_chat\"",
            "derive_from = \"openai_responses\"",
        );
        let cfg: Config = toml::from_str(&text).expect("parse");
        let err = cfg.validate().expect_err("must fail");
        assert!(err.to_string().contains("missing derive_from target"));
    }

    #[test]
    fn multi_hop_derived_chain_fails() {
        let text = r#"
[server]
listen = "127.0.0.1:8989"

[providers.provider-a]
api_key_env = "PROVIDER_A_API_KEY"

[providers.provider-a.openai_chat]
url = "https://api.example.com/v1/chat/completions"

[providers.provider-a.openai_responses]
derive_from = "openai_chat"

[providers.provider-a.anthropic]
derive_from = "openai_responses"
"#;
        let cfg: Config = toml::from_str(text).expect("parse");
        let err = cfg.validate().expect_err("multi-hop must fail");
        assert!(err.to_string().contains("multi-hop"));
    }

    #[test]
    fn unregistered_adapter_pair_fails() {
        // No adapter converts anything *toward* antigravity.
        let text = r#"
[server]
listen = "127.0.0.1:8989"

[providers.provider-a]
api_key_env = "PROVIDER_A_API_KEY"

[providers.provider-a.openai_chat]
url = "https://api.example.com/v1/chat/completions"

[providers.provider-a.antigravity]
derive_from = "openai_chat"
"#;
        let cfg: Config = toml::from_str(text).expect("parse");
        let err = cfg.validate().expect_err("must fail");
        assert!(
            err.to_string().contains("no adapter registered")
                || err
                    .to_string()
                    .contains("antigravity endpoint must be native")
        );
    }

    #[test]
    fn antigravity_derived_fails() {
        let text = r#"
[server]
listen = "127.0.0.1:8989"

[providers.provider-a]
api_key_env = "PROVIDER_A_API_KEY"

[providers.provider-a.openai_chat]
url = "https://api.example.com/v1/chat/completions"

[providers.provider-a.antigravity]
derive_from = "openai_chat"
"#;
        let cfg: Config = toml::from_str(text).expect("parse");
        let err = cfg.validate().expect_err("must fail");
        assert!(
            err.to_string()
                .contains("antigravity endpoint must be native")
        );
    }

    #[test]
    fn unknown_compat_enum_values_fail_validation() {
        let cfg: Config = toml::from_str(
            r#"[server]
listen = "127.0.0.1:8989"

[providers.provider-a]
api_key_env = "KEY"

[providers.provider-a.openai_chat]
url = "https://api.example.com/v1/chat/completions"

[providers.provider-a.openai_chat.compat]
thinking_format = "typo"
max_tokens_field = "max_tokens"

[models.model-a]
context_window = 1000
max_output_tokens = 100
openai_chat_providers = [{ name = "provider-a", model = "m" }]
"#,
        )
        .expect("parse");
        let err = cfg.validate().expect_err("unknown thinking format");
        assert!(err.to_string().contains("compat.thinking_format"));

        let cfg: Config = toml::from_str(
            r#"[server]
listen = "127.0.0.1:8989"

[providers.provider-a]
api_key_env = "KEY"

[providers.provider-a.openai_chat]
url = "https://api.example.com/v1/chat/completions"

[providers.provider-a.openai_chat.compat]
max_tokens_field = "max_completion_tokenz"

[models.model-a]
context_window = 1000
max_output_tokens = 100
openai_chat_providers = [{ name = "provider-a", model = "m" }]
"#,
        )
        .expect("parse");
        let err = cfg.validate().expect_err("unknown max tokens field");
        assert!(err.to_string().contains("compat.max_tokens_field"));
    }

    #[test]
    fn derived_endpoint_with_compat_fails() {
        let text = VALID_CONFIG.replace(
            "derive_from = \"openai_chat\"",
            "derive_from = \"openai_chat\"\n\n[providers.provider-a.anthropic.compat]\nthinking_format = \"deepseek\"",
        );
        let cfg: Config = toml::from_str(&text).expect("parse");
        let err = cfg.validate().expect_err("must fail");
        assert!(err.to_string().contains("must not configure compat"));
    }

    #[test]
    fn native_endpoint_requires_absolute_http_url() {
        let text = VALID_CONFIG.replace(
            "url = \"https://api.example.com/v1/chat/completions\"",
            "url = \"not-a-url\"",
        );
        let cfg: Config = toml::from_str(&text).expect("parse");
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn anthropic_family_models_invalid_glob_fails_validation() {
        let text = VALID_CONFIG.replace(
            "url = \"https://api.example.com/v1/chat/completions\"",
            "url = \"https://api.example.com/v1/chat/completions\"\nanthropic_family_models = [\"claude-[\"]",
        );
        let cfg: Config = toml::from_str(&text).expect("parse");
        let err = cfg.validate().expect_err("invalid glob must fail");
        let msg = err.to_string();
        assert!(msg.contains("anthropic_family_models"));
        assert!(msg.contains("claude-["));
    }

    #[test]
    fn anthropic_family_models_valid_globs_pass_validation() {
        let text = VALID_CONFIG.replace(
            "url = \"https://api.example.com/v1/chat/completions\"",
            "url = \"https://api.example.com/v1/chat/completions\"\nanthropic_family_models = [\"claude-*\", \"gpt-oss-[0-9]*\", \"claude-sonnet-?-*\"]",
        );
        let cfg: Config = toml::from_str(&text).expect("parse");
        cfg.validate().expect("valid globs must pass");
    }

    #[test]
    fn native_endpoint_url_supports_env_reference_with_default() {
        let text = VALID_CONFIG.replace(
            "url = \"https://api.example.com/v1/chat/completions\"",
            "url = \"${LLM_PROXY_TEST_UNSET_VAR}|https://api.example.com/v1/chat/completions\"",
        );
        let cfg: Config = toml::from_str(&text).expect("parse");
        cfg.validate()
            .expect("env reference with default validates");
        let (_id, plan) = cfg
            .resolve_model_request(Protocol::OpenaiChatCompletions, "model-a")
            .expect("resolve");
        assert_eq!(
            plan.native_url,
            "https://api.example.com/v1/chat/completions"
        );
    }

    #[test]
    fn unknown_model_resolves_to_no_candidates() {
        let cfg = default_deepseek_config();
        assert!(
            cfg.resolve_model_request_candidates(Protocol::OpenaiChatCompletions, "no-such-model")
                .is_empty()
        );
    }

    #[test]
    fn model_without_protocol_binding_resolves_to_no_candidates() {
        let text = VALID_CONFIG.replace(
            "anthropic_providers = [{ name = \"provider-a\", model = \"upstream-a\" }]\n",
            "",
        );
        let cfg: Config = toml::from_str(&text).expect("parse");
        cfg.validate().expect("valid");
        assert!(
            cfg.resolve_model_request_candidates(Protocol::Anthropic, "model-a")
                .is_empty()
        );
    }

    #[test]
    fn reasoning_metadata_validation() {
        let base: Config = toml::from_str(VALID_CONFIG).expect("parse");

        let mut dup = base.clone();
        dup.models
            .get_mut("model-a")
            .unwrap()
            .supported_reasoning_levels = vec!["low".to_string(), "low".to_string()];
        let err = dup.validate().expect_err("duplicate levels fail");
        assert!(err.to_string().contains("repeats level"));

        let mut empty = base.clone();
        empty
            .models
            .get_mut("model-a")
            .unwrap()
            .supported_reasoning_levels = vec![String::new()];
        let err = empty.validate().expect_err("empty level fails");
        assert!(err.to_string().contains("empty level"));

        let mut bad_default = base.clone();
        {
            let model = bad_default.models.get_mut("model-a").unwrap();
            model.supported_reasoning_levels = vec!["low".to_string()];
            model.default_reasoning_level = Some("high".to_string());
        }
        let err = bad_default
            .validate()
            .expect_err("default outside supported fails");
        assert!(
            err.to_string()
                .contains("not in supported_reasoning_levels")
        );

        let mut dup_map = base.clone();
        dup_map
            .models
            .get_mut("model-a")
            .unwrap()
            .reasoning_level_map = Some(vec![
            ReasoningLevelMapping {
                level: "low".to_string(),
                api_value: Some("low".to_string()),
            },
            ReasoningLevelMapping {
                level: "low".to_string(),
                api_value: None,
            },
        ]);
        let err = dup_map.validate().expect_err("duplicate map levels fail");
        assert!(err.to_string().contains("repeats level"));
    }

    #[test]
    fn default_config_serializes_protocol_endpoint_fields() {
        let cfg = default_deepseek_config();
        let text = toml::to_string_pretty(&cfg).expect("serialize default config");

        assert!(text.contains("[providers.deepseek.openai_chat]"));
        assert!(text.contains("[providers.deepseek.openai_responses]"));
        assert!(text.contains("derive_from = \"openai_chat\""));
        assert!(text.contains("[providers.deepseek.anthropic]"));
        assert!(text.contains("url = \"https://api.deepseek.com/anthropic/v1/messages\""));
        assert!(text.contains("[fallback.cooldown]"));
        assert!(text.contains("[protection.bad_request]"));
        assert!(text.contains("openai_chat_providers"));
        assert!(!text.contains("base_url"));
        assert!(!text.contains("endpoints"));
        assert!(!text.contains("[[routes]]"));

        let parsed: Config = toml::from_str(&text).expect("round-trip parse");
        parsed.validate().expect("round-trip validates");
    }

    #[test]
    fn execution_plan_carries_native_compat_and_cooldown_key() {
        let cfg = default_deepseek_config();
        let (_id, plan) = cfg
            .resolve_model_request(Protocol::OpenaiChatCompletions, "deepseek-v4-flash-lp")
            .expect("resolve");

        assert_eq!(plan.frontend_protocol, Protocol::OpenaiChatCompletions);
        assert_eq!(plan.provider_id, "deepseek");
        assert_eq!(plan.upstream_model, "deepseek-v4-flash");
        assert_eq!(plan.auth.api_key_env(), Some("DEEPSEEK_API_KEY"));
        assert!(!plan.compat.effective_supports_developer_role());
        assert!(plan.compat.effective_supports_reasoning_effort());
        assert_eq!(plan.compat.thinking_format.as_deref(), Some("deepseek"));
        assert!(
            plan.compat
                .effective_requires_reasoning_content_on_assistant_messages()
        );
        assert_eq!(plan.compat.effective_max_tokens_field(), "max_tokens");
        assert_eq!(
            plan.cooldown_key("deepseek-v4-flash-lp").stable_id(),
            "deepseek-v4-flash-lp:deepseek:chat_completions"
        );

        // Derived plans inherit compat from the native source endpoint.
        let (_id, plan) = cfg
            .resolve_model_request(Protocol::OpenaiResponses, "deepseek-v4-flash-lp")
            .expect("resolve");
        assert_eq!(plan.compat.thinking_format.as_deref(), Some("deepseek"));
    }

    #[test]
    fn adapter_registry_covers_all_required_pairs() {
        use AdapterKind::*;
        use Protocol::*;
        let cases = [
            (
                OpenaiChatCompletions,
                OpenaiResponses,
                ResponsesFromChatCompletions,
            ),
            (
                OpenaiResponses,
                OpenaiChatCompletions,
                ChatCompletionsFromResponses,
            ),
            (
                OpenaiChatCompletions,
                Anthropic,
                AnthropicFromChatCompletions,
            ),
            (
                Anthropic,
                OpenaiChatCompletions,
                ChatCompletionsFromAnthropic,
            ),
            (Anthropic, OpenaiResponses, ResponsesFromAnthropic),
            (OpenaiResponses, Anthropic, AnthropicFromResponses),
            (Antigravity, OpenaiResponses, ResponsesFromAntigravity),
            (Antigravity, Anthropic, AnthropicFromAntigravity),
        ];
        for (source, target, expected) in cases {
            assert_eq!(adapter_for_pair(source, target), Some(expected));
        }
        // Nothing converts toward antigravity; chat-from-antigravity is not registered.
        assert_eq!(adapter_for_pair(OpenaiChatCompletions, Antigravity), None);
        assert_eq!(adapter_for_pair(Antigravity, OpenaiChatCompletions), None);
        assert_eq!(adapter_for_pair(Antigravity, Antigravity), None);
    }

    #[test]
    fn status_config_defaults_match_design_spec() {
        // §12.12：默认 probe_timeout=30、active_ttl=30；缺失 [status] 段时全部默认
        let cfg: Config = toml::from_str(VALID_CONFIG).expect("parse");
        assert_eq!(cfg.status.probe_timeout, 30);
        assert_eq!(cfg.status.active_ttl, 30);
        assert_eq!(
            StatusConfig::default(),
            StatusConfig {
                probe_timeout: 30,
                active_ttl: 30,
            }
        );
    }

    #[test]
    fn status_config_parses_global_and_provider_override() {
        // §12.12：全局 [status] + [providers.xxx.status] 覆盖
        let text = r#"
[server]
listen = "127.0.0.1:8989"

[status]
probe_timeout = 15
active_ttl = 60

[providers.slow]
api_key_env = "SLOW_API_KEY"
[providers.slow.status]
probe_timeout = 60
active_ttl = 120
"#;
        let cfg: Config = toml::from_str(text).expect("parse");
        assert_eq!(cfg.status.probe_timeout, 15);
        assert_eq!(cfg.status.active_ttl, 60);
        // Provider 级覆盖字段在 ProviderConfig 内解析（可选）
        let slow = &cfg.providers["slow"];
        assert!(slow.status.is_some());
    }

    #[test]
    fn status_config_serializes_into_default_config() {
        // init 生成的默认配置应包含 [status] 段（§12.12 init 行为）
        let cfg = default_deepseek_config();
        let text = toml::to_string_pretty(&cfg).expect("serialize");
        assert!(
            text.contains("[status]"),
            "default config should contain [status]"
        );
        assert!(text.contains("probe_timeout = 30"));
        assert!(text.contains("active_ttl = 30"));
    }

    #[test]
    fn migrate_provider_ids_renames_legacy_zhipu_ids() {
        // 旧 ID（zai-payg-global / zhipu-coding-plan-cn）加载时自动迁移到新 ID
        let text = r#"
[server]
listen = "127.0.0.1:8989"

[providers.zai-payg-global]
api_key_env = "ZAI_API_KEY"
[providers.zai-payg-global.openai_chat]
url = "https://api.z.ai/api/paas/v4/chat/completions"

[providers.zhipu-coding-plan-cn]
api_key_env = "BIGMODEL_API_KEY"
[providers.zhipu-coding-plan-cn.openai_chat]
url = "https://open.bigmodel.cn/api/coding/paas/v4/chat/completions"

[models."glm-5.2-zai-global-lp"]
description = "migration test model"
context_window = 128000
max_output_tokens = 32768
openai_chat_providers = [{ name = "zai-payg-global", model = "glm-5.2" }]
"#;
        let cfg: Config = toml::from_str(text).expect("parse");
        let migrated = cfg.migrate_provider_ids().expect("migrate");
        assert!(migrated.providers.contains_key("zhipu-payg-global"));
        assert!(!migrated.providers.contains_key("zai-payg-global"));
        assert!(migrated.providers.contains_key("zhipu-coding-cn"));
        assert!(!migrated.providers.contains_key("zhipu-coding-plan-cn"));
        // model binding 引用同步更新
        let model = migrated.models.get("glm-5.2-zai-global-lp").expect("model");
        let bindings = model.provider_bindings(Protocol::OpenaiChatCompletions);
        assert_eq!(bindings[0].name, "zhipu-payg-global");
    }

    #[test]
    fn product_field_defaults_to_custom() {
        // When product is not specified in TOML, it should default to "custom"
        let provider: ProviderConfig = toml::from_str(
            r#"
api_key_env = "TEST_API_KEY"
[openai_chat]
url = "https://api.example.com/v1/chat/completions"
"#,
        )
        .expect("parse provider without product");
        assert_eq!(provider.product, "custom");
        assert!(provider.is_custom_product());
    }

    #[test]
    fn product_field_explicit_value() {
        // When product is explicitly set, it should be preserved
        let provider: ProviderConfig = toml::from_str(
            r#"
product = "deepseek"
api_key_env = "DEEPSEEK_API_KEY"
[openai_chat]
url = "https://api.deepseek.com/v1/chat/completions"
"#,
        )
        .expect("parse provider with product");
        assert_eq!(provider.product, "deepseek");
        assert!(!provider.is_custom_product());
    }

    #[test]
    fn product_field_is_custom_product_logic() {
        // Test the is_custom_product() helper method
        let mut provider = ProviderConfig::default();

        // Default (empty string from Default trait) → custom
        assert!(provider.is_custom_product());

        // Explicit "custom" → custom
        provider.product = "custom".to_string();
        assert!(provider.is_custom_product());

        // Empty string → custom
        provider.product = String::new();
        assert!(provider.is_custom_product());

        // Non-custom value → not custom
        provider.product = "deepseek".to_string();
        assert!(!provider.is_custom_product());

        provider.product = "kimi".to_string();
        assert!(!provider.is_custom_product());
    }

    #[test]
    fn product_field_serialization_roundtrip() {
        // Test that product field survives serialization/deserialization
        let provider = ProviderConfig {
            product: "openai".to_string(),
            api_key_env: Some("OPENAI_API_KEY".to_string()),
            ..Default::default()
        };

        let serialized = toml::to_string(&provider).expect("serialize");
        assert!(serialized.contains("product = \"openai\""));

        let deserialized: ProviderConfig = toml::from_str(&serialized).expect("deserialize");
        assert_eq!(deserialized.product, "openai");
    }
}
