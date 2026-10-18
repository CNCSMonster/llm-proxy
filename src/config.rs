use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Deserializer, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub server: Option<ServerConfig>,
    #[serde(default)]
    pub remote: Option<RemoteConfig>,
    #[serde(default)]
    pub fallback: FallbackConfig,
    #[serde(default)]
    pub protection: ProtectionConfig,
    #[serde(default)]
    pub diagnostics: DiagnosticsConfig,
    #[serde(default)]
    pub status: StatusConfig,
    #[serde(default)]
    pub codex: CodexConfig,
    #[serde(default)]
    pub launch: LaunchConfig,
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderConfig>,
    #[serde(default)]
    pub models: BTreeMap<String, ModelConfig>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct LaunchConfig {
    #[serde(default, alias = "models", skip_serializing_if = "Vec::is_empty")]
    pub include_models: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude_models: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CodexConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_review_model: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub model_overrides: BTreeMap<String, CodexModelOverride>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodexModelOverride {
    pub auto_review_model: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    pub listen: String,
    /// 是否要求客户端认证（Bearer token）。
    /// 默认根据 listen 地址自动检测：本机地址（127.0.0.1/localhost/::1）不要求，
    /// 非本机地址（0.0.0.0/具体 IP）要求。可显式覆盖。
    #[serde(default)]
    pub require_auth: Option<bool>,
    /// 认证 token（明文，不推荐）。优先使用 auth_token_env。
    #[serde(default)]
    pub auth_token: Option<String>,
    /// 认证 token 环境变量名（推荐）。
    #[serde(default)]
    pub auth_token_env: Option<String>,
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
    /// 代理配置（可选）。如果未设置，会尝试读取环境变量 HTTP_PROXY/HTTPS_PROXY。
    #[serde(default)]
    pub proxy: Option<ProxyConfig>,
    /// Windows 管理通道 TCP 端口（可选，默认 8989）。
    /// 仅 Windows 使用（Unix 走 UDS，忽略此字段）。
    /// 强制绑 127.0.0.1，不可配置绑定地址——管理面始终仅本机可访问。
    #[serde(default)]
    pub management_port: Option<u16>,
}

impl ServerConfig {
    /// 返回有效的管理通道端口：有配置用配置值，无配置返回默认 8989。
    pub fn management_port(&self) -> u16 {
        self.management_port.unwrap_or(8989)
    }
}

/// `[remote]` 远程 server 配置段（client-only 模式）。
///
/// 当本机不运行 server，只作为 client 连接远程 server 时使用。
/// 与 `[server]` 互斥：不能同时配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteConfig {
    /// 远程 server URL，如 "http://server.example.com:8080"
    pub url: String,
    /// 认证 token 环境变量名。未指定时默认为 `LLM_PROXY_API_KEY`。
    #[serde(default = "default_remote_auth_token_env")]
    pub auth_token_env: String,
    /// 远程 server 的认证模式。
    /// - "open": 开放模式，launch 注入占位凭证
    /// - "token": Token 模式，launch 注入真实 token
    ///   默认根据是否配置了 auth_token_env 自动推断，但推荐显式声明。
    #[serde(default)]
    pub auth_mode: Option<RemoteAuthMode>,
}

fn default_remote_auth_token_env() -> String {
    "LLM_PROXY_API_KEY".to_string()
}

/// 远程 server 认证模式
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RemoteAuthMode {
    /// 开放模式：server 不要求认证，launch 注入占位凭证
    Open,
    /// Token 模式：server 要求认证，launch 注入真实 token
    Token,
}

/// 代理配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyConfig {
    /// 代理 URL，如 "http://127.0.0.1:7890" 或 "socks5://127.0.0.1:1080"
    pub url: String,
    /// 不需要代理的主机列表（逗号分隔），如 "localhost,127.0.0.1,*.aliyuncs.com"
    #[serde(default)]
    pub no_proxy: Option<String>,
}

pub(crate) fn default_max_sse_buffer_bytes() -> usize {
    64 * 1024 * 1024 // 64MB
}

pub(crate) fn default_max_output_items() -> usize {
    4096
}

/// `[status]` 全局配置段。
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
    /// 探测并发度（最大并发数，默认 4，收敛秒级突发）。
    #[serde(default = "default_probe_concurrency")]
    pub probe_concurrency: usize,
    /// 探测间隔基础值（毫秒，默认 150ms）。
    #[serde(default = "default_probe_interval_ms")]
    pub probe_interval_ms: u64,
    /// 探测间隔随机抖动（毫秒，默认 100ms）。
    #[serde(default = "default_probe_interval_jitter_ms")]
    pub probe_interval_jitter_ms: u64,
}

impl Default for StatusConfig {
    fn default() -> Self {
        Self {
            probe_timeout: default_probe_timeout(),
            active_ttl: default_active_ttl(),
            probe_concurrency: default_probe_concurrency(),
            probe_interval_ms: default_probe_interval_ms(),
            probe_interval_jitter_ms: default_probe_interval_jitter_ms(),
        }
    }
}

fn default_probe_timeout() -> u64 {
    30
}

fn default_active_ttl() -> u64 {
    30
}

fn default_probe_concurrency() -> usize {
    4
}

fn default_probe_interval_ms() -> u64 {
    150
}

fn default_probe_interval_jitter_ms() -> u64 {
    100
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

/// `[diagnostics]`: structured observability for the failure classes
/// that previously left zero server-side trace (upstream 400 INVALID_ARGUMENT
/// and local egress-preflight rejections). `metadata` (default) records every
/// request *feature* but never literal content; `full` additionally embeds the
/// upstream egress body, capped per record and per file so a debug session
/// cannot fill the disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagnosticsConfig {
    #[serde(default = "default_diagnostics_enabled")]
    pub enabled: bool,
    /// "metadata" | "full"
    #[serde(default = "default_diagnostics_capture")]
    pub capture: String,
    /// Hard cap for one serialized record in `full` mode (bytes).
    #[serde(default = "default_diagnostics_max_record_bytes")]
    pub max_record_bytes: usize,
    /// diagnostics.jsonl size cap (bytes); newest half is kept on overflow.
    #[serde(default = "default_diagnostics_file_max_bytes")]
    pub file_max_bytes: u64,
}

impl Default for DiagnosticsConfig {
    fn default() -> Self {
        Self {
            enabled: default_diagnostics_enabled(),
            capture: default_diagnostics_capture(),
            max_record_bytes: default_diagnostics_max_record_bytes(),
            file_max_bytes: default_diagnostics_file_max_bytes(),
        }
    }
}

fn default_diagnostics_enabled() -> bool {
    true
}

fn default_diagnostics_capture() -> String {
    "metadata".to_string()
}

fn default_diagnostics_max_record_bytes() -> usize {
    65_536
}

fn default_diagnostics_file_max_bytes() -> u64 {
    2_097_152
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FallbackConfig {
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
    #[serde(default = "default_timeout_seconds")]
    pub timeout_seconds: u64,
    #[serde(default = "default_max_timeout_seconds")]
    pub max_timeout_seconds: u64,
    #[serde(default = "default_server_error_retries")]
    pub server_error_retries: u32,
    #[serde(default = "default_server_error_retry_interval_ms")]
    pub server_error_retry_interval_ms: u64,
    #[serde(default)]
    pub cooldown: FallbackCooldownConfig,
}

impl Default for FallbackConfig {
    fn default() -> Self {
        Self {
            max_retries: default_max_retries(),
            timeout_seconds: default_timeout_seconds(),
            max_timeout_seconds: default_max_timeout_seconds(),
            server_error_retries: default_server_error_retries(),
            server_error_retry_interval_ms: default_server_error_retry_interval_ms(),
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
    #[serde(default = "default_auth_error_cooldown_seconds")]
    pub auth_error_seconds: u64,
}

impl Default for FallbackCooldownConfig {
    fn default() -> Self {
        Self {
            network_seconds: default_network_cooldown_seconds(),
            server_error_seconds: default_server_error_cooldown_seconds(),
            rate_limit_seconds: default_rate_limit_cooldown_seconds(),
            model_unavailable_seconds: default_model_unavailable_cooldown_seconds(),
            client_error_seconds: default_client_error_cooldown_seconds(),
            auth_error_seconds: default_auth_error_cooldown_seconds(),
        }
    }
}

/// Provider/model-level cooldown override.
/// All fields are `Option<u64>` — `None` means "not declared, fall back to next level".
/// This enables per-field fallback: only explicitly declared fields override;
/// undeclared fields fall through to provider → global.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CooldownOverrideConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network_seconds: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_error_seconds: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit_seconds: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_unavailable_seconds: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_error_seconds: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_error_seconds: Option<u64>,
}

fn default_max_retries() -> u32 {
    2
}
fn default_server_error_retries() -> u32 {
    2
}
fn default_server_error_retry_interval_ms() -> u64 {
    200
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
fn default_auth_error_cooldown_seconds() -> u64 {
    300
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProtectionConfig {
    #[serde(default)]
    pub bad_request: BadRequestProtectionConfig,
    /// Per-token error-class cooldown.
    /// When a token accumulates repeated errors of the same class,
    /// ALL requests from that token are blocked for a cooldown period.
    #[serde(default)]
    pub error_cooldown: ErrorCooldownConfig,
    /// Per-account cumulative bad request budget (daily + lifetime layers).
    /// When a token accumulates real upstream 4xx errors (excluding 401/403/429),
    /// the daily and lifetime counters increment; exceeding either limit blocks
    /// all requests from that account locally (no upstream traffic).
    #[serde(default)]
    pub bad_request_budget: AccountBadBudgetConfig,
    /// Local reject tolerance protection.
    /// Tracks client-attributable local rejections per (token, inbound model).
    /// When threshold is reached, triggers (token, model) cooldown.
    #[serde(default)]
    pub local_reject: LocalRejectConfig,
    /// When true, missing `function_call_output` items in Responses
    /// history are replaced with placeholder content instead of rejecting the
    /// request with 400. Orphan reasoning is discarded with a warning.
    /// Default false preserves the existing strict-reject behavior.
    #[serde(default)]
    pub placeholder_on_missing_history: bool,
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

/// Per-token error-class cooldown configuration.
///
/// When a token accumulates `max_errors_per_class` errors of the same
/// error-class within `window_seconds`, ALL requests from that token
/// are blocked for `cooldown_seconds`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorCooldownConfig {
    #[serde(default = "default_error_cooldown_enabled")]
    pub enabled: bool,
    /// Number of same-class errors before the token enters cooldown.
    #[serde(default = "default_error_cooldown_max_errors_per_class")]
    pub max_errors_per_class: u32,
    /// How long the token is blocked (seconds) after hitting the threshold.
    #[serde(default = "default_error_cooldown_seconds")]
    pub cooldown_seconds: u64,
    /// Sliding window for error counts (seconds). Counts reset if no errors
    /// of the same class occur within this window.
    #[serde(default = "default_error_cooldown_window_seconds")]
    pub window_seconds: u64,
}

impl Default for ErrorCooldownConfig {
    fn default() -> Self {
        Self {
            enabled: default_error_cooldown_enabled(),
            max_errors_per_class: default_error_cooldown_max_errors_per_class(),
            cooldown_seconds: default_error_cooldown_seconds(),
            window_seconds: default_error_cooldown_window_seconds(),
        }
    }
}

fn default_error_cooldown_enabled() -> bool {
    true
}
fn default_error_cooldown_max_errors_per_class() -> u32 {
    3
}
fn default_error_cooldown_seconds() -> u64 {
    300
}
fn default_error_cooldown_window_seconds() -> u64 {
    600
}

/// Local reject tolerance configuration.
///
/// Tracks client-attributable local rejections (ingress 4xx format errors,
/// fingerprint 429 blocks, and known preflight/replay rejections) per
/// (token, inbound model) pair over a sliding window.
///
/// When a token accumulates `max_local_rejects` rejections within `window_seconds`,
/// subsequent requests for that model are blocked with HTTP 429 for `cooldown_seconds`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalRejectConfig {
    #[serde(default = "default_local_reject_enabled")]
    pub enabled: bool,
    /// Number of local rejections before the (token, model) enters cooldown.
    #[serde(default = "default_local_reject_max_local_rejects")]
    pub max_local_rejects: u32,
    /// How long the (token, model) is blocked (seconds) after hitting the threshold.
    #[serde(default = "default_local_reject_cooldown_seconds")]
    pub cooldown_seconds: u64,
    /// Sliding window for local rejection counts (seconds).
    #[serde(default = "default_local_reject_window_seconds")]
    pub window_seconds: u64,
}

impl Default for LocalRejectConfig {
    fn default() -> Self {
        Self {
            enabled: default_local_reject_enabled(),
            max_local_rejects: default_local_reject_max_local_rejects(),
            cooldown_seconds: default_local_reject_cooldown_seconds(),
            window_seconds: default_local_reject_window_seconds(),
        }
    }
}

fn default_local_reject_enabled() -> bool {
    true
}
fn default_local_reject_max_local_rejects() -> u32 {
    10
}
fn default_local_reject_cooldown_seconds() -> u64 {
    300
}
fn default_local_reject_window_seconds() -> u64 {
    600
}

/// Per-account cumulative bad request budget configuration.
///
/// Two independent layers protect upstream from sustained bad-request floods:
/// - **Daily**: resets at natural-day boundary; default 2 errors/day.
/// - **Lifetime**: persistent across restarts; default 10 errors total.
///
/// When either layer's counter reaches its limit, all requests from that
/// account are rejected locally (no upstream traffic). The daily layer
/// auto-recovers the next day; the lifetime layer requires admin reset.
///
/// Per-account overrides in `tokens.toml` take precedence over these globals.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountBadBudgetConfig {
    #[serde(default = "default_bad_budget_enabled")]
    pub enabled: bool,
    /// Maximum protectable upstream 4xx errors per natural day before locking.
    #[serde(default = "default_bad_budget_daily_limit")]
    pub daily_limit: u32,
    /// Maximum protectable upstream 4xx errors (cumulative, persistent) before locking.
    #[serde(default = "default_bad_budget_lifetime_limit")]
    pub lifetime_limit: u32,
}

impl Default for AccountBadBudgetConfig {
    fn default() -> Self {
        Self {
            enabled: default_bad_budget_enabled(),
            daily_limit: default_bad_budget_daily_limit(),
            lifetime_limit: default_bad_budget_lifetime_limit(),
        }
    }
}

fn default_bad_budget_enabled() -> bool {
    true
}
fn default_bad_budget_daily_limit() -> u32 {
    2
}
fn default_bad_budget_lifetime_limit() -> u32 {
    10
}

/// Provider capability declaration: each present protocol field means the
/// provider can serve that protocol. There is no `endpoints` map layer and no
/// provider-level `base_url`; native endpoints carry complete URLs.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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
    /// Retired key: presence triggers migration error at serde boundary.
    /// Strategy fields are Model-only; Provider does not inherit or declare them.
    /// `pub(crate)` only so struct literals/FRU elsewhere in the crate compile;
    /// deserialization always rejects a present key and serialization skips it.
    #[allow(dead_code)] // presence-rejection sentinel: written by serde, never read
    #[serde(
        default,
        skip_serializing,
        deserialize_with = "reject_retired_enable_thinking"
    )]
    pub(crate) enable_thinking: Option<serde_json::Value>,
    /// Model-only keys misplaced on a Provider are rejected at the serde
    /// boundary; never serialized, never read at runtime.
    #[allow(dead_code)] // presence-rejection sentinel: written by serde, never read
    #[serde(
        default,
        skip_serializing,
        deserialize_with = "reject_model_only_thinking_key"
    )]
    pub(crate) supports_thinking: Option<serde_json::Value>,
    #[allow(dead_code)] // presence-rejection sentinel: written by serde, never read
    #[serde(
        default,
        skip_serializing,
        deserialize_with = "reject_model_only_thinking_key"
    )]
    pub(crate) force_think: Option<serde_json::Value>,
    #[allow(dead_code)] // presence-rejection sentinel: written by serde, never read
    #[serde(
        default,
        skip_serializing,
        deserialize_with = "reject_model_only_thinking_key"
    )]
    pub(crate) force_no_think: Option<serde_json::Value>,
    #[allow(dead_code)] // presence-rejection sentinel: written by serde, never read
    #[serde(
        default,
        skip_serializing,
        deserialize_with = "reject_model_only_thinking_key"
    )]
    pub(crate) default_think: Option<serde_json::Value>,
    #[allow(dead_code)] // presence-rejection sentinel: written by serde, never read
    #[serde(
        default,
        skip_serializing,
        deserialize_with = "reject_model_only_thinking_key"
    )]
    pub(crate) chat_reasoning_off_format: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub openai_chat: Option<EndpointConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub openai_responses: Option<EndpointConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anthropic: Option<EndpointConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_levels: Option<Vec<ReasoningLevelEntry>>,
    /// Retired key: presence triggers migration error at serde boundary.
    #[allow(dead_code)]
    #[serde(
        default,
        skip_serializing,
        deserialize_with = "reject_retired_reasoning_level_map"
    )]
    pub(crate) reasoning_level_map: Option<Vec<ReasoningLevelEntry>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_frequency: Option<RequestFrequencyConfig>,
    /// Provider 级 status 覆盖：`[providers.<id>.status]`，可选。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<StatusConfig>,
    /// Provider-level cooldown override. When present, overrides
    /// global [fallback.cooldown] for this provider. Unset fields fall back to global.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cooldown: Option<CooldownOverrideConfig>,
    /// 异常请求容忍量——provider 收到 N 次上游 400 后才触发 provider 级冷却。
    /// 默认 2 次。catalog 注入，connect 写入 config，用户可覆盖。
    /// 等于默认值时不写出，保持渲染字节稳定（config_render 语义相等测试）。
    #[serde(
        default = "default_bad_request_tolerance",
        skip_serializing_if = "is_default_bad_request_tolerance"
    )]
    pub bad_request_tolerance: u32,
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            product: default_product(),
            api_key_env: None,
            auth: None,
            enable_thinking: None,
            supports_thinking: None,
            force_think: None,
            force_no_think: None,
            default_think: None,
            chat_reasoning_off_format: None,
            openai_chat: None,
            openai_responses: None,
            anthropic: None,
            reasoning_levels: None,
            reasoning_level_map: None,
            request_frequency: None,
            status: None,
            cooldown: None,
            bad_request_tolerance: default_bad_request_tolerance(),
        }
    }
}

/// Default product value for providers: "custom" means the provider does not
/// belong to any specific product and was manually configured.
fn default_product() -> String {
    "custom".to_string()
}

/// 异常请求容忍量默认值——provider 级，允许 2 次上游 400 后才触发冷却。
fn default_bad_request_tolerance() -> u32 {
    2
}

/// 等于默认值 2 时跳过序列化：省略与显式 2 语义等价，重解析仍得 2。
fn is_default_bad_request_tolerance(v: &u32) -> bool {
    *v == default_bad_request_tolerance()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AuthConfig {
    ApiKeyEnv { env: String },
    OpenaiOauth { account: Option<String> },
    None,
}

impl AuthConfig {
    pub fn normalized_for_provider(&self, provider_id: &str) -> AuthConfig {
        match self {
            AuthConfig::OpenaiOauth { account } => AuthConfig::OpenaiOauth {
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
    /// (passthrough client value, else inject `false`).
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
#[allow(dead_code)]
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
/// mean default behavior (standard OpenAI-compatible assumptions).
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
    /// and client value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub must_not_store: Option<bool>,
}

// Accessors encoding the compat trust-contract defaults; consumed by the
// egress adaptation (thinking injection, reasoning mapping, token field).
// Some methods are consumed only by specific adapter paths; allow(dead_code)
// covers those not yet reached from all build configurations.
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
        self.max_tokens_field
            .as_deref()
            .unwrap_or("max_completion_tokens")
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
/// A reasoning-level entry in the unified `reasoning_levels` table (ADR-034).
///
/// Supports Cargo-dependency-style compact syntax via `#[serde(untagged)]`:
/// - `"low"` → `Simple("low")`
/// - `{level = "high", api_value = "extended"}` → `Detailed { ... }`
///
/// Mixed arrays are supported: `["low", {level = "high", api_value = "extended"}]`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ReasoningLevelEntry {
    /// Compact string form: `reasoning_levels = ["low", "medium", "high"]`
    Simple(String),
    /// Full table form: `{level = "high", api_value = "extended"}`
    Detailed {
        level: String,
        /// Missing `api_value` = identity (level sent as-is to upstream).
        /// null/empty is not allowed; validated at config load time.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        api_value: Option<String>,
    },
}

impl ReasoningLevelEntry {
    /// The reasoning level name (e.g. "low", "medium", "high").
    pub fn level(&self) -> &str {
        match self {
            Self::Simple(s) => s,
            Self::Detailed { level, .. } => level,
        }
    }

    /// The upstream API value mapping, if any.
    pub fn api_value(&self) -> Option<&str> {
        match self {
            Self::Simple(_) => None,
            Self::Detailed { api_value, .. } => api_value.as_deref(),
        }
    }

    /// Construct from parts. Uses `Simple` when `api_value` is None (compact
    /// serialization), `Detailed` when an API value mapping is present.
    pub fn new(level: impl Into<String>, api_value: Option<String>) -> Self {
        match api_value {
            None => Self::Simple(level.into()),
            Some(v) => Self::Detailed {
                level: level.into(),
                api_value: Some(v),
            },
        }
    }
}

/// Reject retired `supported_reasoning_levels` key on ModelConfig at serde
/// boundary. Presence triggers a migration error directing users to the
/// unified `reasoning_levels` table (ADR-034).
fn reject_retired_supported_reasoning_levels<'de, D>(
    deserializer: D,
) -> Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let _ = Vec::<String>::deserialize(deserializer)?;
    Err(serde::de::Error::custom(
        "supported_reasoning_levels is retired; migrate to reasoning_levels: each entry has level + optional api_value",
    ))
}

/// Reject retired `reasoning_level_map` key at serde boundary. Both
/// ModelConfig and ProviderConfig use `reasoning_levels` now.
fn reject_retired_reasoning_level_map<'de, D>(
    deserializer: D,
) -> Result<Option<Vec<ReasoningLevelEntry>>, D::Error>
where
    D: Deserializer<'de>,
{
    let _ = Option::<Vec<ReasoningLevelEntry>>::deserialize(deserializer)?;
    Err(serde::de::Error::custom(
        "reasoning_level_map is retired; migrate to reasoning_levels",
    ))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Protocol {
    #[serde(rename = "openai-chat", alias = "openai-chat-completions")]
    OpenaiChatCompletions,
    #[serde(rename = "openai-responses", alias = "responses")]
    OpenaiResponses,
    #[serde(rename = "anthropic")]
    Anthropic,
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
        }
    }

    pub fn from_field_name(name: &str) -> Option<Protocol> {
        match name {
            "openai_chat" => Some(Protocol::OpenaiChatCompletions),
            "openai_responses" => Some(Protocol::OpenaiResponses),
            "anthropic" => Some(Protocol::Anthropic),
            _ => None,
        }
    }

    /// Stable key used in cooldown state and status output.
    pub fn route_key(self) -> &'static str {
        match self {
            Protocol::OpenaiChatCompletions => "chat_completions",
            Protocol::OpenaiResponses => "responses",
            Protocol::Anthropic => "anthropic",
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodexServiceTierConfig {
    pub tiers: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_tier: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forced_tier: Option<String>,
}

/// Reject retired `enable_thinking` key at serde boundary. Presence of this
/// key (regardless of value type, including JSON null) triggers a migration
/// error directing users to the new capability/strategy fields. Absent keys
/// never reach this function (`serde(default)`), so it always errors.
fn reject_retired_enable_thinking<'de, D>(
    deserializer: D,
) -> Result<Option<serde_json::Value>, D::Error>
where
    D: Deserializer<'de>,
{
    let _ = serde_json::Value::deserialize(deserializer)?;
    Err(serde::de::Error::custom(
        "enable_thinking is retired; migrate to model-level supports_thinking (capability) and/or force_think/force_no_think/default_think (strategy)",
    ))
}

/// Reject Model-only thinking keys misplaced on a Provider.
/// Same presence-rejection mechanics as the retired-key sentinel.
fn reject_model_only_thinking_key<'de, D>(
    deserializer: D,
) -> Result<Option<serde_json::Value>, D::Error>
where
    D: Deserializer<'de>,
{
    let _ = serde_json::Value::deserialize(deserializer)?;
    Err(serde::de::Error::custom(
        "supports_thinking/force_think/force_no_think/default_think/chat_reasoning_off_format are model-only keys; move them to models.<id>",
    ))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub context_window: i64,
    #[serde(default)]
    pub max_output_tokens: i64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub features: Vec<String>,
    /// Unified reasoning levels table: allowed activity levels + optional mapping.
    /// Replaces old `supported_reasoning_levels` + `reasoning_level_map` dual-key system.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_levels: Option<Vec<ReasoningLevelEntry>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_reasoning_level: Option<String>,
    /// Capability declaration: whether the model can think. Pure metadata for
    /// launch client capability reporting; no request-path behavior.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_thinking: Option<bool>,
    /// Strategy: force thinking on. Only Some(true) enables; None preserves
    /// default; Some(false) does not enable this constraint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub force_think: Option<bool>,
    /// Strategy: force thinking off. Only Some(true) enables; None preserves
    /// default; Some(false) does not enable this constraint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub force_no_think: Option<bool>,
    /// Strategy: default thinking switch. true/false set default on/off; can
    /// be overridden by explicit client intent; None does not inject.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_think: Option<bool>,
    /// Chat egress dialect for expressing effective reasoning-off.
    /// Known values: "enable-thinking". Validated against
    /// the whitelist at config load; unknown values are rejected, not
    /// silently ignored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat_reasoning_off_format: Option<String>,
    /// Retired key: presence triggers migration error at serde boundary.
    /// `pub(crate)` only so struct literals/FRU elsewhere in the crate compile;
    /// deserialization always rejects a present key and serialization skips it.
    #[allow(dead_code)] // presence-rejection sentinel: written by serde, never read
    #[serde(
        default,
        skip_serializing,
        deserialize_with = "reject_retired_enable_thinking"
    )]
    pub(crate) enable_thinking: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_service_tier: Option<CodexServiceTierConfig>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub openai_chat_providers: Vec<ProviderBinding>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub openai_responses_providers: Vec<ProviderBinding>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub anthropic_providers: Vec<ProviderBinding>,
    /// Retired key: presence triggers migration error at serde boundary.
    #[allow(dead_code)]
    #[serde(
        default,
        skip_serializing,
        deserialize_with = "reject_retired_reasoning_level_map"
    )]
    pub(crate) reasoning_level_map: Option<Vec<ReasoningLevelEntry>>,
    /// Retired key: presence triggers migration error at serde boundary.
    #[allow(dead_code)]
    #[serde(
        default,
        skip_serializing,
        deserialize_with = "reject_retired_supported_reasoning_levels"
    )]
    pub(crate) supported_reasoning_levels: Vec<String>,
    /// Model-level cooldown override. When present, overrides
    /// provider-level and global [fallback.cooldown] for this model.
    /// Unset fields fall back to provider, then global.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cooldown: Option<CooldownOverrideConfig>,
    /// Routing rules: when non-empty, this model is a routing model
    /// that forwards requests to target models based on reasoning level.
    /// Routing models have no provider bindings, no context_window, no reasoning_levels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_routes: Option<Vec<RouteEntry>>,
}

/// A single routing rule in a routing model.
/// Maps a reasoning level to a target frontend model ID.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteEntry {
    pub level: String,
    pub target: String,
}

/// The capability bounds a client should see for a model. For a
/// routing model these are the conservative lower bound across its direct targets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelCapabilityBounds {
    pub context_window: i64,
    pub max_output_tokens: i64,
    pub features: Vec<String>,
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
        }
    }

    pub fn exposes_protocol(&self, protocol: Protocol) -> bool {
        !self.provider_bindings(protocol).is_empty()
    }

    /// Whether this model references the given provider ID in any of its protocol bindings.
    pub fn references_provider(&self, provider: &str) -> bool {
        self.openai_chat_providers
            .iter()
            .any(|b| b.name == provider)
            || self
                .openai_responses_providers
                .iter()
                .any(|b| b.name == provider)
            || self.anthropic_providers.iter().any(|b| b.name == provider)
    }

    /// Routing rules, empty for a regular model. Declaring an empty table is a
    /// load error, so `Some(vec![])` never survives validation.
    pub fn routes(&self) -> &[RouteEntry] {
        self.reasoning_routes.as_deref().unwrap_or(&[])
    }

    /// Whether this model is a routing model (has routes, no provider bindings).
    pub fn is_routing_model(&self) -> bool {
        !self.routes().is_empty()
    }

    /// Resolve a reasoning level to a target model ID.
    /// Returns None if the level is not in the routes table.
    pub fn resolve_route(&self, level: &str) -> Option<&str> {
        self.routes()
            .iter()
            .find(|entry| entry.level == level)
            .map(|entry| entry.target.as_str())
    }

    /// Protocols this model can serve, for error messages.
    pub fn supported_protocols(&self) -> Vec<Protocol> {
        Protocol::CLIENT_PROTOCOLS
            .into_iter()
            .filter(|protocol| self.exposes_protocol(*protocol))
            .collect()
    }

    /// This Model's *own* reasoning table (ADR-034 §2).
    /// `None` means the Model declares no table — it may still inherit the bound
    /// provider's table via `Config::effective_reasoning_levels`.
    pub fn own_reasoning_levels(&self) -> Option<&[ReasoningLevelEntry]> {
        self.reasoning_levels
            .as_deref()
            .filter(|table| !table.is_empty())
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
        }
    }

    pub fn set_endpoint(&mut self, protocol: Protocol, endpoint: EndpointConfig) {
        match protocol {
            Protocol::OpenaiChatCompletions => self.openai_chat = Some(endpoint),
            Protocol::OpenaiResponses => self.openai_responses = Some(endpoint),
            Protocol::Anthropic => self.anthropic = Some(endpoint),
        }
    }

    pub fn endpoints(&self) -> Vec<(Protocol, &EndpointConfig)> {
        let mut out = Vec::new();
        for protocol in [
            Protocol::OpenaiChatCompletions,
            Protocol::OpenaiResponses,
            Protocol::Anthropic,
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
            for model in self.models.values_mut() {
                for protocol in Protocol::CLIENT_PROTOCOLS {
                    let bindings = match protocol {
                        Protocol::OpenaiChatCompletions => &mut model.openai_chat_providers,
                        Protocol::OpenaiResponses => &mut model.openai_responses_providers,
                        Protocol::Anthropic => &mut model.anthropic_providers,
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

    /// Returns the sorted list of model IDs that reference the given provider ID
    /// in any of their protocol bindings.
    pub fn models_for_provider(&self, provider: &str) -> Vec<String> {
        self.models
            .iter()
            .filter(|(_, model)| model.references_provider(provider))
            .map(|(name, _)| name.clone())
            .collect()
    }

    pub fn validate(&self) -> Result<()> {
        // 检查 server 和 remote 互斥
        if self.server.is_some() && self.remote.is_some() {
            bail!(
                "Cannot have both [server] and [remote] in config. \
                 Choose either local server mode or remote client mode. \
                 Use separate config directories (--config-dir) for different modes."
            );
        }

        // 如果配置了 server，验证认证要求
        if let Some(ref server) = self.server {
            self.validate_server_auth(server)?;
        }

        for (provider_id, provider) in &self.providers {
            validate_provider(provider_id, provider)?;
        }
        // Routing models are validated first so that an execution-layer field
        // placed on a hop reports the routing-model rule. Running the generic
        // execution-layer validation first would answer with guidance the routing
        // model cannot follow (e.g. "add default_reasoning_level" for
        // `force_think`, which is itself forbidden on a hop).
        self.validate_routing_models()?;
        for (model_id, model) in &self.models {
            let effective_levels = self.effective_reasoning_levels(model_id);
            validate_model_reasoning(model_id, model, effective_levels)?;
            self.validate_model_bindings(model_id, model)?;
            self.validate_codex_service_tier(model_id, model)?;
        }
        self.validate_codex_auto_review()?;
        self.validate_diagnostics()?;
        Ok(())
    }

    /// 验证 server 认证配置。
    ///
    /// 根据 listen 地址自动检测是否需要认证（本机不要求，跨机要求）。
    /// 如果要求认证但未配置 token，报错。
    fn validate_server_auth(&self, server: &ServerConfig) -> Result<()> {
        let require_auth = server
            .require_auth
            .unwrap_or_else(|| Self::should_require_auth(&server.listen));

        if require_auth {
            let has_token = server.auth_token.is_some()
                || server
                    .auth_token_env
                    .as_ref()
                    .is_some_and(|env_var| std::env::var(env_var).is_ok());

            if !has_token {
                bail!(
                    "Server listens on {} (non-local) and requires authentication, \
                     but no auth token is configured. \
                     Set auth_token_env or auth_token in [server], \
                     or explicitly disable with require_auth = false",
                    server.listen
                );
            }
        }

        Ok(())
    }

    /// 判断 listen 地址是否为本机地址。
    ///
    /// 本机地址：127.0.0.1、localhost、::1、0.0.0.0（绑定所有接口但仍是本地）
    /// 非本机地址：其他 IP 或域名
    fn should_require_auth(listen: &str) -> bool {
        // 提取 host 部分（去掉 port）
        let host = listen
            .split(':')
            .next()
            .unwrap_or(listen)
            .trim()
            .trim_start_matches('[')
            .trim_end_matches(']');

        // 本机地址列表
        !matches!(host, "127.0.0.1" | "localhost" | "::1" | "0.0.0.0")
    }

    /// 解析 server 认证 token（优先环境变量，其次明文）。
    #[allow(dead_code)]
    pub fn resolve_server_auth_token(&self) -> Option<String> {
        self.server.as_ref().and_then(|server| {
            server
                .auth_token_env
                .as_ref()
                .and_then(|env_var| std::env::var(env_var).ok())
                .or_else(|| server.auth_token.clone())
        })
    }

    /// 解析 remote 认证 token（从环境变量读取）。
    pub fn resolve_remote_auth_token(&self) -> Option<String> {
        self.remote
            .as_ref()
            .and_then(|remote| std::env::var(&remote.auth_token_env).ok())
    }

    /// 获取远程 server 的有效认证模式。
    ///
    /// 优先级：
    /// 1. 显式声明的 `auth_mode`
    /// 2. 推断：环境变量已设置则为 Token，否则为 Open
    ///
    /// 如果没有配置 `[remote]`，返回 None。
    pub fn effective_remote_auth_mode(&self) -> Option<RemoteAuthMode> {
        self.remote.as_ref().map(|remote| {
            remote.auth_mode.unwrap_or_else(|| {
                if std::env::var(&remote.auth_token_env).is_ok() {
                    RemoteAuthMode::Token
                } else {
                    RemoteAuthMode::Open
                }
            })
        })
    }

    /// 获取 server 配置（如果存在）。
    ///
    /// 在 client-only 模式下返回 None。
    pub fn server_config(&self) -> Option<&ServerConfig> {
        self.server.as_ref()
    }

    /// 获取 server 配置，如果不存在则报错。
    ///
    /// 用于必须运行 server 的场景（如 serve 命令）。
    pub fn require_server(&self) -> Result<&ServerConfig> {
        self.server.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "No [server] section in config. \
                 This command requires server mode. \
                 Add [server] section or use a different config directory."
            )
        })
    }

    /// Reject a `[server] listen` that external clients cannot discover.
    ///
    /// Port 0 lets the kernel pick an ephemeral port, but the port actually
    /// bound is never published anywhere a client can read it. `llm-proxy
    /// launch` derives client `base_url` from this field, so a port-0 config
    /// produces `http://127.0.0.1:0/...` and the client can never connect.
    ///
    /// This is deliberately checked at the serve entry points rather than in
    /// [`Config::validate`]: `validate()` guards every config edit path (TUI,
    /// catalog, connect) and unit tests legitimately use ephemeral ports.
    pub fn ensure_serve_listen(&self) -> Result<()> {
        let server = self.require_server()?;
        if let Ok(addr) = server.listen.parse::<std::net::SocketAddr>()
            && addr.port() == 0
        {
            bail!(
                "[server] listen port must not be 0: the OS-assigned port is never published, \
                 so generated client configs would point at http://0 and never connect. \
                 Set a fixed port, e.g. listen = \"127.0.0.1:8989\" (got {:?})",
                server.listen
            );
        }
        Ok(())
    }

    fn validate_diagnostics(&self) -> Result<()> {
        if !matches!(self.diagnostics.capture.as_str(), "metadata" | "full") {
            bail!(
                "[diagnostics] capture must be \"metadata\" or \"full\", got {:?}",
                self.diagnostics.capture
            );
        }
        if !(1024..=1_048_576).contains(&self.diagnostics.max_record_bytes) {
            bail!(
                "[diagnostics] max_record_bytes must be within 1KiB..=1MiB, got {}",
                self.diagnostics.max_record_bytes
            );
        }
        if !(65_536..=268_435_456).contains(&self.diagnostics.file_max_bytes) {
            bail!(
                "[diagnostics] file_max_bytes must be within 64KiB..=256MiB, got {}",
                self.diagnostics.file_max_bytes
            );
        }
        Ok(())
    }

    pub fn codex_review_target(&self, model_id: &str) -> Option<&str> {
        self.codex
            .model_overrides
            .get(model_id)
            .map(|override_config| override_config.auto_review_model.as_str())
            .or(self.codex.auto_review_model.as_deref())
    }

    pub fn codex_review_provider_references(&self, provider_id: &str) -> Vec<String> {
        let mut references = Vec::new();
        let targets = self.codex.auto_review_model.iter().chain(
            self.codex
                .model_overrides
                .values()
                .map(|override_config| &override_config.auto_review_model),
        );
        for target in targets {
            if let Some(model) = self.models.get(target) {
                for binding in &model.openai_responses_providers {
                    if binding.name == provider_id {
                        references.push(format!("{target}.openai_responses_providers"));
                    }
                }
            }
        }
        references.sort();
        references.dedup();
        references
    }

    pub fn codex_model_references(&self, target_model_id: &str) -> Vec<String> {
        let mut references = Vec::new();
        if self.codex.auto_review_model.as_deref() == Some(target_model_id) {
            references.push("codex.auto_review_model".to_string());
        }
        for (model_id, override_config) in &self.codex.model_overrides {
            if override_config.auto_review_model == target_model_id {
                references.push(format!(
                    "codex.model_overrides.{model_id}.auto_review_model"
                ));
            }
        }
        references
    }

    /// Validate routing models:
    /// - an explicitly declared empty table is an error
    /// - target existence; a level may appear once per model
    /// - mutual exclusion with every execution-layer field
    /// - a hop must target a regular (executable) model: routing is a single hop
    fn validate_routing_models(&self) -> Result<()> {
        for (model_id, model) in &self.models {
            if let Some(routes) = &model.reasoning_routes
                && routes.is_empty()
            {
                bail!(
                    "model {model_id:?} declares an empty reasoning_routes table; \
                     either list at least one route or remove the field"
                );
            }
            if !model.is_routing_model() {
                continue;
            }

            // A routing model only routes. Anything that describes how a request is
            // executed belongs on the target and would be silently ignored here.
            for protocol in Protocol::CLIENT_PROTOCOLS {
                if !model.provider_bindings(protocol).is_empty() {
                    bail!(
                        "routing model {model_id:?} has {protocol:?} provider bindings; \
                         routing models must not have provider bindings — \
                         remove the bindings or remove reasoning_routes"
                    );
                }
            }
            let execution_layer_fields: [(&str, bool); 10] = [
                ("reasoning_levels", model.reasoning_levels.is_some()),
                (
                    "default_reasoning_level",
                    model.default_reasoning_level.is_some(),
                ),
                ("features", !model.features.is_empty()),
                ("cooldown", model.cooldown.is_some()),
                ("supports_thinking", model.supports_thinking.is_some()),
                ("force_think", model.force_think.is_some()),
                ("force_no_think", model.force_no_think.is_some()),
                ("default_think", model.default_think.is_some()),
                ("enable_thinking", model.enable_thinking.is_some()),
                ("codex_service_tier", model.codex_service_tier.is_some()),
            ];
            if let Some((field, _)) = execution_layer_fields.iter().find(|(_, set)| *set) {
                bail!(
                    "routing model {model_id:?} declares {field}; routing models are pure \
                     forwarding hops and take execution behaviour from the resolved target — \
                     move {field} onto the target or remove reasoning_routes"
                );
            }
            if model.chat_reasoning_off_format.is_some() {
                bail!(
                    "routing model {model_id:?} declares chat_reasoning_off_format; routing \
                     models take execution behaviour from the resolved target"
                );
            }
            // Capability and window declarations belong to the target too. The
            // client-visible values are derived from the reachable targets.
            if model.context_window != 0 || model.max_output_tokens != 0 {
                bail!(
                    "routing model {model_id:?} declares context_window / max_output_tokens; \
                     a routing model reports the conservative bound over its targets — \
                     move these onto the targets or remove reasoning_routes"
                );
            }

            let mut seen_levels = std::collections::BTreeSet::new();
            for entry in model.routes() {
                if !seen_levels.insert(&entry.level) {
                    bail!(
                        "routing model {model_id:?} has duplicate route level {:?}",
                        entry.level
                    );
                }
                if !self.models.contains_key(&entry.target) {
                    bail!(
                        "routing model {model_id:?} route level {:?} references unknown target model {:?}",
                        entry.level,
                        entry.target
                    );
                }
                // Routing forwards by reasoning level, whose only key is the level
                // itself — so any chain of hops collapses to a single level → target
                // map and adds no expressive power, while creating the incomplete-chain
                // and protocol-tear failure modes a chain invites. Require a direct
                // hop to an executable model; this also makes cycles impossible.
                if self.models[&entry.target].is_routing_model() {
                    bail!(
                        "routing model {model_id:?} route level {:?} targets routing model {:?}; \
                         a routing model must forward directly to an executable model — \
                         routing is a single hop",
                        entry.level,
                        entry.target
                    );
                }
            }

            // All targets of a routing model must declare identical client-facing capabilities:
            // context_window, max_output_tokens, features (modalities/tools), exposed client
            // protocols, and codex_service_tier. Execution-layer attributes (provider bindings,
            // upstream model IDs, costs, cooldowns) can vary per target.
            let mut target_iter = model
                .routes()
                .iter()
                .map(|entry| (&entry.target, &self.models[&entry.target]));
            if let Some((first_id, first_target)) = target_iter.next() {
                let first_features: std::collections::BTreeSet<_> =
                    first_target.features.iter().collect();
                let first_protocols: std::collections::BTreeSet<_> = Protocol::CLIENT_PROTOCOLS
                    .iter()
                    .filter(|p| first_target.exposes_protocol(**p))
                    .collect();

                for (target_id, target) in target_iter {
                    if target.context_window != first_target.context_window {
                        bail!(
                            "routing model {model_id:?} targets have mismatched context_window: \
                             target {first_id:?} has {}, but target {target_id:?} has {}",
                            first_target.context_window,
                            target.context_window
                        );
                    }
                    if target.max_output_tokens != first_target.max_output_tokens {
                        bail!(
                            "routing model {model_id:?} targets have mismatched max_output_tokens: \
                             target {first_id:?} has {}, but target {target_id:?} has {}",
                            first_target.max_output_tokens,
                            target.max_output_tokens
                        );
                    }
                    let target_features: std::collections::BTreeSet<_> =
                        target.features.iter().collect();
                    if target_features != first_features {
                        bail!(
                            "routing model {model_id:?} targets have mismatched features: \
                             target {first_id:?} has {:?}, but target {target_id:?} has {:?}",
                            first_target.features,
                            target.features
                        );
                    }
                    let target_protocols: std::collections::BTreeSet<_> =
                        Protocol::CLIENT_PROTOCOLS
                            .iter()
                            .filter(|p| target.exposes_protocol(**p))
                            .collect();
                    if target_protocols != first_protocols {
                        bail!(
                            "routing model {model_id:?} targets have mismatched protocol support: \
                             target {first_id:?} exposes {:?}, but target {target_id:?} exposes {:?}",
                            first_target.supported_protocols(),
                            target.supported_protocols()
                        );
                    }
                    if target.codex_service_tier != first_target.codex_service_tier {
                        bail!(
                            "routing model {model_id:?} targets have mismatched codex_service_tier: \
                             target {first_id:?} has {:?}, but target {target_id:?} has {:?}",
                            first_target.codex_service_tier,
                            target.codex_service_tier
                        );
                    }
                }
            }
        }

        Ok(())
    }

    fn validate_codex_auto_review(&self) -> Result<()> {
        let mut targets = Vec::new();
        if let Some(target) = &self.codex.auto_review_model {
            targets.push(("codex.auto_review_model".to_string(), target));
        }
        for (model_id, override_config) in &self.codex.model_overrides {
            let coding_model = self.models.get(model_id).with_context(|| {
                format!("codex model override references unknown coding model {model_id}")
            })?;
            if !coding_model.exposes_protocol(Protocol::OpenaiResponses) {
                bail!(
                    "codex.model_overrides.{model_id}.auto_review_model requires the coding model to expose OpenAI Responses"
                );
            }
            targets.push((
                format!("codex.model_overrides.{model_id}.auto_review_model"),
                &override_config.auto_review_model,
            ));
        }
        let mut referenced_targets = std::collections::BTreeSet::new();
        for (source, target) in targets {
            if target.is_empty() {
                bail!("{source} must not be empty");
            }
            let target_model = self
                .models
                .get(target)
                .with_context(|| format!("{source} references unknown review model {target}"))?;
            if !target_model.exposes_protocol(Protocol::OpenaiResponses) {
                bail!("{source} review model {target} must expose OpenAI Responses");
            }
            if !referenced_targets.insert(target.clone()) {
                continue;
            }
            if let Some(nested) = self
                .codex
                .model_overrides
                .get(target)
                .map(|override_config| override_config.auto_review_model.as_str())
            {
                bail!(
                    "review model {target} must not configure another auto-review model ({nested})"
                );
            }
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
        // ADR-034 §5: a frontend model may inherit its vocabulary from at most one
        // bound provider. Two bound providers that each declare `reasoning_levels`
        // have incompatible vocabularies; the fallback would silently apply one
        // provider's table to the other. Split into two frontend IDs instead.
        let mut table_providers: Vec<&str> = Protocol::CLIENT_PROTOCOLS
            .iter()
            .flat_map(|protocol| model.provider_bindings(*protocol))
            .map(|binding| binding.name.as_str())
            .filter(|name| {
                self.providers
                    .get(*name)
                    .and_then(|provider| provider.reasoning_levels.as_deref())
                    .is_some_and(|table| !table.is_empty())
            })
            .collect();
        table_providers.sort_unstable();
        table_providers.dedup();
        if table_providers.len() > 1 {
            bail!(
                "model {model_id} binds multiple providers that declare reasoning_levels ({}); their vocabularies cannot both apply — split the model into separate frontend IDs",
                table_providers.join(", ")
            );
        }
        Ok(())
    }

    fn validate_codex_service_tier(&self, model_id: &str, model: &ModelConfig) -> Result<()> {
        let Some(service_tier) = &model.codex_service_tier else {
            return Ok(());
        };
        if service_tier.tiers.is_empty() {
            bail!("model {model_id} codex_service_tier.tiers must not be empty");
        }
        let mut seen = std::collections::BTreeSet::new();
        for tier in &service_tier.tiers {
            if tier != "priority" {
                bail!(
                    "model {model_id} codex_service_tier.tiers contains unsupported tier {tier:?}"
                );
            }
            if !seen.insert(tier.clone()) {
                bail!("model {model_id} codex_service_tier.tiers repeats tier {tier:?}");
            }
        }
        if service_tier.default_tier.is_some() && service_tier.forced_tier.is_some() {
            bail!(
                "model {model_id} codex_service_tier.default_tier and forced_tier are mutually exclusive"
            );
        }
        for (field, tier) in [
            ("default_tier", service_tier.default_tier.as_deref()),
            ("forced_tier", service_tier.forced_tier.as_deref()),
        ] {
            if let Some(tier) = tier
                && !seen.contains(tier)
            {
                bail!("model {model_id} codex_service_tier.{field} must appear in tiers");
            }
        }
        let plans = self.resolve_model_request_candidates(Protocol::OpenaiResponses, model_id);
        if plans.is_empty()
            || plans
                .iter()
                .any(|(_, plan)| plan.source_protocol != Protocol::OpenaiResponses)
        {
            bail!(
                "model {model_id} codex_service_tier requires every openai_responses binding to resolve to a native openai_responses endpoint"
            );
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

    /// The reasoning vocabulary that actually applies to a frontend model:
    /// its own table, else the table of its unique bound provider (ADR-034 §2).
    /// `None` means nothing is declared → no gating, identity passthrough.
    ///
    /// Config validation rejects more than one tabled provider per frontend, so the
    /// provider fallback is unambiguous.
    pub fn effective_reasoning_levels(&self, model_id: &str) -> Option<&[ReasoningLevelEntry]> {
        let model = self.models.get(model_id)?;
        if let Some(table) = model.own_reasoning_levels() {
            return Some(table);
        }
        Protocol::CLIENT_PROTOCOLS
            .into_iter()
            .flat_map(|protocol| model.provider_bindings(protocol))
            .find_map(|binding| {
                self.providers
                    .get(&binding.name)
                    .and_then(|provider| provider.reasoning_levels.as_deref())
                    .filter(|table| !table.is_empty())
            })
    }

    /// Every execution plan a client request for `model_id` may end up using for
    /// `protocol`. A routing hop itself has no plans; it contributes
    /// its targets' plans.
    pub fn client_candidate_plans(
        &self,
        protocol: Protocol,
        model_id: &str,
    ) -> Vec<(String, ExecutionPlan)> {
        let mut leaves = Vec::new();
        self.collect_capability_leaves(model_id, &mut leaves);
        leaves
            .into_iter()
            .flat_map(|(leaf_id, _)| self.resolve_model_request_candidates(protocol, leaf_id))
            .collect()
    }

    /// The reasoning levels a client should see for `model_id`.
    /// For a routing model this is exactly its route level
    /// keys: the hop has no `api_value` of its own — each target maps the level
    /// it receives.
    pub fn client_reasoning_levels(&self, model_id: &str) -> Option<Vec<ReasoningLevelEntry>> {
        let model = self.models.get(model_id)?;
        if model.is_routing_model() {
            return Some(
                model
                    .routes()
                    .iter()
                    .map(|entry| ReasoningLevelEntry::new(entry.level.clone(), None))
                    .collect(),
            );
        }
        self.effective_reasoning_levels(model_id).map(<[_]>::to_vec)
    }

    /// The default reasoning level a client should see for `model_id`.
    ///
    /// For a regular model, returns its explicit `default_reasoning_level`.
    /// For a routing model (which cannot declare execution fields directly),
    /// falls back to the first available reasoning route level.
    pub fn client_default_reasoning_level(&self, model_id: &str) -> Option<String> {
        let model = self.models.get(model_id)?;
        model.default_reasoning_level.clone().or_else(|| {
            self.client_reasoning_levels(model_id)
                .and_then(|levels| levels.first().map(|e| e.level().to_string()))
        })
    }

    /// Whether `model_id` can serve `protocol`.
    ///
    /// A routing hop serves a protocol only when **every** target can: the
    /// client's reasoning level decides which target executes the request, so
    /// advertising a protocol that only some levels can serve would be a lie.
    /// Routing is a single hop, so the reachable set is the hop's direct targets.
    pub fn model_serves_protocol(&self, model_id: &str, protocol: Protocol) -> bool {
        let Some(model) = self.models.get(model_id) else {
            return false;
        };
        if model.is_routing_model() {
            !model.routes().is_empty()
                && model.routes().iter().all(|entry| {
                    self.models
                        .get(&entry.target)
                        .is_some_and(|target| target.exposes_protocol(protocol))
                })
        } else {
            model.exposes_protocol(protocol)
        }
    }

    /// The capability bounds a client should see for `model_id`.
    ///
    /// For a regular model these are its own declarations. For a routing model
    /// they are the conservative lower bound across every target — the client may
    /// be routed to any of them, so the reported window must hold for all (same
    /// rule as a regular model's multiple provider candidates).
    pub fn model_capability_bounds(&self, model_id: &str) -> Option<ModelCapabilityBounds> {
        let mut leaves = Vec::new();
        self.collect_capability_leaves(model_id, &mut leaves);
        let ((_, first), rest) = leaves.split_first()?;
        let mut bounds = ModelCapabilityBounds {
            context_window: first.context_window,
            max_output_tokens: first.max_output_tokens,
            features: first.features.clone(),
        };
        for (_, leaf) in rest {
            bounds.context_window = bounds.context_window.min(leaf.context_window);
            bounds.max_output_tokens = bounds.max_output_tokens.min(leaf.max_output_tokens);
            bounds.features.retain(|f| leaf.features.contains(f));
        }
        Some(bounds)
    }

    /// Whether `model_id` (or all its reachable targets if routing) explicitly
    /// declared any capability features.
    pub fn model_features_declared(&self, model_id: &str) -> bool {
        let mut leaves = Vec::new();
        self.collect_capability_leaves(model_id, &mut leaves);
        !leaves.is_empty() && leaves.iter().all(|(_, leaf)| !leaf.features.is_empty())
    }

    /// The Codex service-tier declaration a client should see for `model_id`.
    ///
    /// A routing model may execute on any target, so it can only advertise a tier
    /// declaration that every target declares identically; otherwise the hop has
    /// no single promise to make and reports none. A routing model cannot declare
    /// `codex_service_tier` itself, so the declaration always comes from targets.
    pub fn client_codex_service_tier(&self, model_id: &str) -> Option<CodexServiceTierConfig> {
        let mut leaves = Vec::new();
        self.collect_capability_leaves(model_id, &mut leaves);
        let mut declared = leaves
            .iter()
            .map(|(_, leaf)| leaf.codex_service_tier.as_ref());
        let first = declared.next()??;
        declared
            .all(|tier| tier == Some(first))
            .then(|| first.clone())
    }

    /// The set of executable Models a client could reach through `model_id`.
    /// For a regular model that is itself; for a routing hop, its
    /// direct route targets — routing is a single hop, validated so a target is
    /// never itself a hop.
    fn collect_capability_leaves<'a>(
        &'a self,
        model_id: &'a str,
        out: &mut Vec<(&'a str, &'a ModelConfig)>,
    ) {
        let Some(model) = self.models.get(model_id) else {
            return;
        };
        if model.is_routing_model() {
            for entry in model.routes() {
                if let Some((key, target)) = self.models.get_key_value(&entry.target) {
                    out.push((key.as_str(), target));
                }
            }
        } else {
            out.push((model_id, model));
        }
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
            routed_target: None,
        })
    }

    pub fn resolve_auth_at(&self, provider_id: &str, accounts_path: &Path) -> Result<ResolvedAuth> {
        let provider = self
            .providers
            .get(provider_id)
            .with_context(|| format!("unknown provider {provider_id}"))?;
        match provider.auth_config(provider_id)? {
            AuthConfig::ApiKeyEnv { env } => Ok(ResolvedAuth {
                token: std::env::var(&env).ok().filter(|value| !value.is_empty()),
                project_id: None,
            }),
            AuthConfig::OpenaiOauth { .. } => {
                // OAuth 取数改走分发面（resolve→source 唯一取数路径）；
                // apikey/env-optional 与 None 两臂语义分层保留（L1 分类在调用点）。
                let token = crate::auth::get_token_valid(provider_id, provider, accounts_path)?;
                Ok(ResolvedAuth {
                    token: Some(token),
                    project_id: None,
                })
            }
            AuthConfig::None => Ok(ResolvedAuth {
                token: None,
                project_id: None,
            }),
        }
    }

    /// Get effective cooldown configuration for a model/provider combination.
    /// Priority: model > provider > global [fallback.cooldown], **per-field**.
    /// Each field independently falls back: if model doesn't declare it, try provider;
    /// if provider doesn't declare it, use global.
    pub fn effective_cooldown(&self, model_id: &str, provider_id: &str) -> FallbackCooldownConfig {
        let global = &self.fallback.cooldown;
        let provider = self
            .providers
            .get(provider_id)
            .and_then(|p| p.cooldown.as_ref());
        let model = self.models.get(model_id).and_then(|m| m.cooldown.as_ref());

        FallbackCooldownConfig {
            network_seconds: model
                .and_then(|m| m.network_seconds)
                .or(provider.and_then(|p| p.network_seconds))
                .unwrap_or(global.network_seconds),
            server_error_seconds: model
                .and_then(|m| m.server_error_seconds)
                .or(provider.and_then(|p| p.server_error_seconds))
                .unwrap_or(global.server_error_seconds),
            rate_limit_seconds: model
                .and_then(|m| m.rate_limit_seconds)
                .or(provider.and_then(|p| p.rate_limit_seconds))
                .unwrap_or(global.rate_limit_seconds),
            model_unavailable_seconds: model
                .and_then(|m| m.model_unavailable_seconds)
                .or(provider.and_then(|p| p.model_unavailable_seconds))
                .unwrap_or(global.model_unavailable_seconds),
            client_error_seconds: model
                .and_then(|m| m.client_error_seconds)
                .or(provider.and_then(|p| p.client_error_seconds))
                .unwrap_or(global.client_error_seconds),
            auth_error_seconds: model
                .and_then(|m| m.auth_error_seconds)
                .or(provider.and_then(|p| p.auth_error_seconds))
                .unwrap_or(global.auth_error_seconds),
        }
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
    if let Some(table) = &provider.reasoning_levels {
        if table.is_empty() {
            bail!(
                "provider {provider_id} reasoning_levels is empty; remove the key or add at least one entry"
            );
        }
        validate_reasoning_levels_table(table)
            .with_context(|| format!("provider {provider_id} reasoning_levels"))?;
    }
    if let Some(freq) = &provider.request_frequency {
        validate_request_frequency(provider_id, freq)?;
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
        AuthConfig::OpenaiOauth { account } => {
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
            "provider {provider_id} derived {:?} endpoint has unknown derive_from field {source_name:?}; expected one of openai_chat, openai_responses, anthropic",
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

fn validate_model_reasoning(
    model_id: &str,
    model: &ModelConfig,
    effective_levels: Option<&[ReasoningLevelEntry]>,
) -> Result<()> {
    // Validate the unified reasoning_levels table (ADR-034).
    if let Some(table) = &model.reasoning_levels {
        if table.is_empty() {
            bail!(
                "model {model_id} reasoning_levels is empty; remove the key or add at least one entry"
            );
        }
        validate_reasoning_levels_table(table)
            .map_err(|err| anyhow::anyhow!("model {model_id} reasoning_levels {err}"))?;
    }
    // default_reasoning_level must be an active level of the *effective* table — the
    // Model's own table, or the one it inherits from its bound provider (ADR-034 §2).
    if let Some(default) = &model.default_reasoning_level
        && let Some(table) = effective_levels
        && !table.iter().any(|entry| entry.level() == *default)
    {
        let allowed = table
            .iter()
            .map(|entry| entry.level())
            .collect::<Vec<_>>()
            .join(", ");
        bail!(
            "model {model_id} default_reasoning_level {default:?} is not in reasoning_levels; allowed: {allowed}"
        );
    }
    // default_think=true requires default_reasoning_level (ADR-034 §3).
    // force_think=true without default_reasoning_level is allowed: it means
    // "thinking always on, no level control" (e.g. Claude extended thinking).
    if model.default_think == Some(true) && model.default_reasoning_level.is_none() {
        bail!("model {model_id} default_think=true requires default_reasoning_level");
    }
    // default_reasoning_level must not be "none" or "adaptive".
    if let Some(default) = &model.default_reasoning_level {
        if default == "none" {
            bail!(
                "model {model_id} default_reasoning_level must not be \"none\"; use default_think=false instead"
            );
        }
        if default == "adaptive" {
            bail!(
                "model {model_id} default_reasoning_level must not be \"adaptive\"; adaptive is a thinking.type, not an effort level"
            );
        }
    }
    // Chat off-dialect whitelist (ADR-024): unknown formats
    // are rejected here, never silently mapped to a standard expression.
    if let Some(format) = &model.chat_reasoning_off_format
        && format != "enable-thinking"
    {
        bail!(
            "model {model_id} chat_reasoning_off_format {format:?} is not a known dialect; known values: \"enable-thinking\""
        );
    }
    // Strategy field contradictions.
    let force_on = model.force_think == Some(true);
    let force_off = model.force_no_think == Some(true);
    if force_on && force_off {
        bail!("model {model_id} force_think and force_no_think are both true; pick one");
    }
    if force_on && model.default_think == Some(false) {
        bail!("model {model_id} force_think=true conflicts with default_think=false");
    }
    if force_off && model.default_think == Some(true) {
        bail!("model {model_id} force_no_think=true conflicts with default_think=true");
    }
    // force_no_think + default_reasoning_level is VALID (ADR-034):
    // two independent knobs — force_no_think overrides thinking strategy,
    // default_reasoning_level sets the default effort when thinking is on.
    // Capability/strategy contradictions.
    if model.supports_thinking == Some(false) {
        if force_on {
            bail!(
                "model {model_id} supports_thinking=false conflicts with force_think=true (capability denies what strategy forces)"
            );
        }
        if model.default_think == Some(true) {
            bail!("model {model_id} supports_thinking=false conflicts with default_think=true");
        }
        if model.default_reasoning_level.is_some() {
            bail!(
                "model {model_id} supports_thinking=false conflicts with default_reasoning_level"
            );
        }
        if model.reasoning_levels.is_some() {
            bail!("model {model_id} supports_thinking=false conflicts with reasoning_levels table");
        }
    }
    Ok(())
}

/// Validate the unified reasoning_levels table (ADR-034).
fn validate_reasoning_levels_table(table: &[ReasoningLevelEntry]) -> Result<()> {
    let mut seen = std::collections::BTreeSet::new();
    for entry in table {
        if entry.level().is_empty() {
            bail!("contains an empty level");
        }
        if entry.level() == "adaptive" {
            bail!(
                "level \"adaptive\" is not allowed; adaptive is a thinking.type, not an effort level"
            );
        }
        if !seen.insert(entry.level().to_string()) {
            bail!("repeats level {:?}", entry.level());
        }
        if let Some(api_value) = entry.api_value()
            && api_value.is_empty()
        {
            bail!(
                "level {:?} has empty api_value; remove the field for identity mapping",
                entry.level()
            );
        }
    }
    Ok(())
}

/// Default config directory (`$XDG_CONFIG_HOME/llm-proxy/`).
///
/// The instance identity is derived from this directory.
pub fn default_config_dir() -> PathBuf {
    config_dir_in(dirs::config_dir().unwrap_or_else(|| {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".config")
    }))
}

fn config_dir_in(config_root: PathBuf) -> PathBuf {
    config_root.join("llm-proxy")
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

/// 稳定、不可逆的 config 身份标识。
///
/// CLI 在委托写操作前用它确认目标 server 用的是**同一份** config，而不只是
/// 监听同一地址：两个 config 的 `listen` 相同时，隔离环境的写会
/// 静默落到另一个 server 上。绝对路径经过哈希，因此该值可以出现在公开的
/// `/admin/ping` 响应里而不泄露运维者的文件系统布局。
///
/// 输入为 config **目录**路径（非文件路径）。同目录内移动
/// config.toml 不影响 identity；不同目录一定产生不同 identity。
///
/// 用 FNV-1a 而不是 `DefaultHasher`：后者的算法不保证跨 Rust 版本稳定，而本
/// 标识恰恰需要在 CLI 与 server 版本可能不同时仍可比较。
pub fn config_identity(config_dir: &Path) -> String {
    let canonical = config_dir
        .canonicalize()
        .unwrap_or_else(|_| config_dir.to_path_buf());
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in canonical.to_string_lossy().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
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

/// 根据配置构造协议 base URL。
///
/// 优先级：
/// 1. 如果配置了 `[remote]`，使用远程 URL + prefix
/// 2. 否则使用本地 server 的 listen 地址（现有行为）
///
/// 支持路径前缀（用于反向代理部署，如 `/lp/`）。
pub fn protocol_base_url(cfg: &Config, prefix: &str) -> String {
    if let Some(ref remote) = cfg.remote {
        // 远程模式：使用 remote URL
        format!(
            "{}/{}",
            remote.url.trim_end_matches('/'),
            prefix.trim_start_matches('/')
        )
    } else if let Some(ref server) = cfg.server {
        // 本地模式：使用 local server（现有行为）
        local_protocol_base_url(&server.listen, prefix)
    } else {
        // 验证应该已经拦截这种情况
        unreachable!("Config must have either [remote] or [server] after validation")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CooldownKey {
    pub model_id: String,
    pub provider_id: String,
    pub protocol: Protocol,
    /// frontend token identity for (token × provider) 400
    /// cooldowns. `None` = provider-global entry (health-family cooldowns and
    /// manual CLI sets), which is what every pre-existing key expresses.
    pub token: Option<String>,
    /// Immutable token key hash for matching across renames.
    pub token_key_hash: Option<String>,
}

impl CooldownKey {
    pub fn stable_id(&self) -> String {
        cooldown_stable_id(
            &self.model_id,
            &self.provider_id,
            self.protocol,
            self.token.as_deref(),
            self.token_key_hash.as_deref(),
        )
    }
}

pub(crate) fn cooldown_stable_id(
    model_id: &str,
    provider_id: &str,
    protocol: Protocol,
    token: Option<&str>,
    token_key_hash: Option<&str>,
) -> String {
    if let Some(hash) = token_key_hash {
        format!(
            "{model_id}:{provider_id}:{}:tok_hash:{hash}",
            protocol.route_key()
        )
    } else if let Some(token) = token {
        format!(
            "{model_id}:{provider_id}:{}:tok:{token}",
            protocol.route_key()
        )
    } else {
        format!("{model_id}:{provider_id}:{}", protocol.route_key())
    }
}

#[derive(Debug, Clone)]
pub struct ExecutionPlan {
    pub frontend_protocol: Protocol,
    pub provider_id: String,
    pub upstream_model: String,
    /// Protocol of the native endpoint that will actually be contacted.
    #[allow(dead_code)] // read by adapter dispatch tests; egress adaptation will branch on it
    pub source_protocol: Protocol,
    /// Passthrough when the entry endpoint is native for the frontend protocol.
    pub adapter: AdapterKind,
    pub native_url: String,
    pub auth: AuthConfig,
    /// Effective compat of the target native endpoint.
    #[allow(dead_code)] // egress adaptation input (compat trust-contract)
    pub compat: CompatConfig,
    /// 目标 native endpoint 中走 Anthropic Messages 转换的模型族（glob 匹配
    /// upstream_model）。转换器据此决定 functionCall/functionResponse 是否带 id。
    #[allow(dead_code)]
    pub anthropic_family_models: Vec<String>,
    /// Effective `store` declaration of the target native endpoint.
    #[allow(dead_code)] // egress adaptation input
    pub store: Option<bool>,
    pub request_frequency: RequestFrequencyConfig,
    /// Leaf target model when this plan was produced via Model Routing.
    /// `None` for regular (non-routed) models. Set by the dispatch loop
    /// after `resolve_routing` confirms a routing hop occurred.
    pub routed_target: Option<String>,
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
            token: None,
            token_key_hash: None,
        }
    }

    /// upstream-400 cooldowns attribute to the requesting
    /// frontend token, not the provider — other users of the same provider
    /// keep serving while only this (token × provider) pair is skipped.
    pub fn token_cooldown_key(
        &self,
        frontend_model_id: &str,
        token_name: &str,
        token_key_hash: Option<&str>,
    ) -> CooldownKey {
        CooldownKey {
            model_id: frontend_model_id.to_string(),
            provider_id: self.provider_id.clone(),
            protocol: self.frontend_protocol,
            token: Some(token_name.to_string()),
            token_key_hash: token_key_hash.map(|s| s.to_string()),
        }
    }
}

/// Resolve the default config path.
///
/// `$XDG_CONFIG_HOME/llm-proxy/config.toml`, falling back to
/// `$HOME/.config/llm-proxy/config.toml` when `XDG_CONFIG_HOME` is unset.
///
/// ADR-027 puts the config file and the OAuth account store in the same
/// `XDG_CONFIG_HOME` category, so this must resolve under the same root as
/// `auth::storage::default_state_path`. An earlier version read `$HOME`
/// directly, which silently split the two whenever `XDG_CONFIG_HOME` was set.
pub fn default_config_path() -> PathBuf {
    default_config_dir().join("config.toml")
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
    let text = crate::config_edit::canonical_format(&cfg)?;
    crate::config_edit::atomic_write(path, text.as_bytes())?;
    println!("config created: {}", path.display());
    Ok(())
}

pub fn deepseek_reasoning_levels() -> Vec<ReasoningLevelEntry> {
    vec![
        ReasoningLevelEntry::new("minimal", Some("high".to_string())),
        ReasoningLevelEntry::new("low", Some("high".to_string())),
        ReasoningLevelEntry::new("medium", Some("high".to_string())),
        ReasoningLevelEntry::new("high", Some("high".to_string())),
        ReasoningLevelEntry::new("xhigh", Some("max".to_string())),
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
                reasoning_levels: Some(deepseek_reasoning_levels()),
                default_reasoning_level: Some("high".to_string()),
                supports_thinking: Some(true),
                force_think: None,
                force_no_think: None,
                default_think: None,
                enable_thinking: None,
                chat_reasoning_off_format: None,
                codex_service_tier: None,
                openai_chat_providers: vec![binding()],
                openai_responses_providers: vec![binding()],
                anthropic_providers: vec![binding()],
                reasoning_level_map: None,
                supported_reasoning_levels: Vec::new(),
                cooldown: None,
                reasoning_routes: None,
            },
        );
    }

    Config {
        server: Some(ServerConfig {
            listen: "127.0.0.1:8989".to_string(),
            usage: UsageConfig::default(),
            max_sse_buffer_bytes: default_max_sse_buffer_bytes(),
            max_output_items: default_max_output_items(),
            proxy: None,
            require_auth: None,
            auth_token: None,
            auth_token_env: None,
            management_port: None,
        }),
        remote: None,
        fallback: FallbackConfig::default(),
        protection: ProtectionConfig::default(),
        diagnostics: DiagnosticsConfig::default(),
        status: StatusConfig::default(),
        codex: CodexConfig::default(),
        launch: LaunchConfig::default(),
        providers,
        models,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    #[test]
    fn test_default_config_path_uses_llm_proxy_layout() {
        let path = default_config_path();
        assert!(
            path.ends_with("llm-proxy/config.toml"),
            "unexpected path: {}",
            path.display()
        );
    }

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
    fn codex_auto_review_model_override_precedes_global_value() {
        let cfg: Config = toml::from_str(
            r#"
[server]
listen = "127.0.0.1:8989"

[providers.provider-a]
api_key_env = "KEY"
[providers.provider-a.openai_responses]
url = "https://api.example.com/v1/responses"

[models.coding]
context_window = 100
max_output_tokens = 10
openai_responses_providers = [{ name = "provider-a", model = "coding" }]
[models.review-global]
context_window = 100
max_output_tokens = 10
openai_responses_providers = [{ name = "provider-a", model = "review-global" }]
[models.review-local]
context_window = 100
max_output_tokens = 10
openai_responses_providers = [{ name = "provider-a", model = "review-local" }]

[codex]
auto_review_model = "review-global"
[codex.model_overrides.coding]
auto_review_model = "review-local"
"#,
        )
        .expect("parse");
        cfg.validate().expect("validate");
        assert_eq!(cfg.codex_review_target("coding"), Some("review-local"));
        assert_eq!(cfg.codex_review_target("other"), Some("review-global"));
        assert_eq!(
            cfg.codex_review_target("review-local"),
            Some("review-global")
        );
    }

    #[test]
    fn codex_auto_review_rejects_unknown_target() {
        let mut cfg: Config = toml::from_str(VALID_CONFIG).expect("parse");
        cfg.codex.auto_review_model = Some("missing".to_string());
        let err = cfg.validate().expect_err("missing target rejected");
        assert!(err.to_string().contains("unknown review model"));
    }

    #[test]
    fn codex_auto_review_rejects_non_responses_target() {
        let mut cfg: Config = toml::from_str(VALID_CONFIG).expect("parse");
        cfg.codex.auto_review_model = Some("model-a".to_string());
        let err = cfg.validate().expect_err("non-Responses target rejected");
        assert!(err.to_string().contains("must expose OpenAI Responses"));
    }

    #[test]
    fn codex_auto_review_rejects_nested_target() {
        let mut cfg: Config = toml::from_str(VALID_CONFIG).expect("parse");
        cfg.codex.auto_review_model = Some("model-a".to_string());
        cfg.providers
            .get_mut("provider-a")
            .unwrap()
            .openai_responses = Some(EndpointConfig::native(
            "https://api.example.com/v1/responses",
        ));
        cfg.models
            .get_mut("model-a")
            .unwrap()
            .openai_responses_providers = vec![ProviderBinding {
            name: "provider-a".to_string(),
            model: "upstream".to_string(),
        }];
        cfg.codex.model_overrides.insert(
            "model-a".to_string(),
            CodexModelOverride {
                auto_review_model: "model-a".to_string(),
            },
        );
        let err = cfg.validate().expect_err("nested target rejected");
        assert!(err.to_string().contains("must not configure another"));
    }

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
    fn codex_service_tier_requires_priority_and_native_responses_bindings() {
        let mut cfg = default_deepseek_config();
        cfg.providers.get_mut("deepseek").unwrap().openai_responses =
            Some(EndpointConfig::native("https://example.test/responses"));
        let model = cfg.models.get_mut("deepseek-v4-flash-lp").unwrap();
        model.codex_service_tier = Some(CodexServiceTierConfig {
            tiers: vec!["priority".to_string()],
            default_tier: Some("priority".to_string()),
            forced_tier: None,
        });
        cfg.validate()
            .expect("priority tier with native Responses is valid");

        let mut unsupported = cfg.clone();
        unsupported
            .models
            .get_mut("deepseek-v4-flash-lp")
            .unwrap()
            .codex_service_tier
            .as_mut()
            .unwrap()
            .tiers = vec!["standard".to_string()];
        assert!(
            unsupported
                .validate()
                .expect_err("unknown tier is rejected")
                .to_string()
                .contains("unsupported tier")
        );

        let mut missing_default = cfg.clone();
        missing_default
            .models
            .get_mut("deepseek-v4-flash-lp")
            .unwrap()
            .codex_service_tier
            .as_mut()
            .unwrap()
            .default_tier = Some("other".to_string());
        assert!(
            missing_default
                .validate()
                .expect_err("default outside tiers is rejected")
                .to_string()
                .contains("default_tier")
        );

        let mut forced_and_default = cfg.clone();
        forced_and_default
            .models
            .get_mut("deepseek-v4-flash-lp")
            .unwrap()
            .codex_service_tier
            .as_mut()
            .unwrap()
            .forced_tier = Some("priority".to_string());
        assert!(
            forced_and_default
                .validate()
                .expect_err("default and forced are mutually exclusive")
                .to_string()
                .contains("mutually exclusive")
        );

        let mut forced_outside_tiers = cfg.clone();
        let tier_config = forced_outside_tiers
            .models
            .get_mut("deepseek-v4-flash-lp")
            .unwrap()
            .codex_service_tier
            .as_mut()
            .unwrap();
        tier_config.default_tier = None;
        tier_config.forced_tier = Some("other".to_string());
        assert!(
            forced_outside_tiers
                .validate()
                .expect_err("forced outside tiers is rejected")
                .to_string()
                .contains("forced_tier")
        );

        let mut derived_responses = cfg;
        derived_responses
            .providers
            .get_mut("deepseek")
            .unwrap()
            .openai_responses = Some(EndpointConfig::derived(Protocol::OpenaiChatCompletions));
        assert!(
            derived_responses
                .validate()
                .expect_err("derived Responses endpoint is rejected")
                .to_string()
                .contains("native openai_responses")
        );
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
        assert_eq!(plan.adapter(), AdapterKind::Passthrough);
        assert_eq!(plan.source_protocol, Protocol::OpenaiResponses);
        assert_eq!(plan.native_url, "https://api.deepseek.com/v1/responses");

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
    fn effective_max_tokens_field_defaults_to_max_completion_tokens() {
        // default flipped from "max_tokens" to "max_completion_tokens"
        // so probe bodies work on modern OpenAI-compatible models out of the box.
        let cfg: Config = toml::from_str(
            r#"[server]
listen = "127.0.0.1:8989"

[providers.provider-a]
api_key_env = "KEY"

[providers.provider-a.openai_chat]
url = "https://api.example.com/v1/chat/completions"

[models.model-a]
context_window = 1000
max_output_tokens = 100
openai_chat_providers = [{ name = "provider-a", model = "m" }]
"#,
        )
        .expect("parse");
        cfg.validate().expect("valid");
        let (_id, plan) = cfg
            .resolve_model_request(Protocol::OpenaiChatCompletions, "model-a")
            .expect("resolve");
        // No compat.max_tokens_field set → new default
        assert_eq!(
            plan.compat.effective_max_tokens_field(),
            "max_completion_tokens"
        );
    }

    #[test]
    fn effective_max_tokens_field_explicit_legacy_override() {
        // backward compat — explicit "max_tokens" still works for old models.
        let cfg: Config = toml::from_str(
            r#"[server]
listen = "127.0.0.1:8989"

[providers.provider-a]
api_key_env = "KEY"

[providers.provider-a.openai_chat]
url = "https://api.example.com/v1/chat/completions"

[providers.provider-a.openai_chat.compat]
max_tokens_field = "max_tokens"

[models.model-a]
context_window = 1000
max_output_tokens = 100
openai_chat_providers = [{ name = "provider-a", model = "m" }]
"#,
        )
        .expect("parse");
        cfg.validate().expect("valid");
        let (_id, plan) = cfg
            .resolve_model_request(Protocol::OpenaiChatCompletions, "model-a")
            .expect("resolve");
        assert_eq!(plan.compat.effective_max_tokens_field(), "max_tokens");
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

        // Duplicate levels in the unified reasoning_levels table fail.
        let mut dup = base.clone();
        dup.models.get_mut("model-a").unwrap().reasoning_levels = Some(vec![
            ReasoningLevelEntry::new("low", None),
            ReasoningLevelEntry::new("low", None),
        ]);
        let err = dup.validate().expect_err("duplicate levels fail");
        assert!(err.to_string().contains("repeats level"));

        // Empty level in the table fails.
        let mut empty = base.clone();
        empty.models.get_mut("model-a").unwrap().reasoning_levels =
            Some(vec![ReasoningLevelEntry::new(String::new(), None)]);
        let err = empty.validate().expect_err("empty level fails");
        assert!(err.to_string().contains("empty level"));

        // Default outside the table fails.
        let mut bad_default = base.clone();
        {
            let model = bad_default.models.get_mut("model-a").unwrap();
            model.reasoning_levels = Some(vec![ReasoningLevelEntry::new("low", None)]);
            model.default_reasoning_level = Some("high".to_string());
        }
        let err = bad_default
            .validate()
            .expect_err("default outside table fails");
        assert!(err.to_string().contains("not in reasoning_levels"));
    }

    #[test]
    fn default_config_serializes_protocol_endpoint_fields() {
        let cfg = default_deepseek_config();
        let text = toml::to_string_pretty(&cfg).expect("serialize default config");

        assert!(text.contains("[providers.deepseek.openai_chat]"));
        assert!(text.contains("[providers.deepseek.openai_responses]"));
        assert!(text.contains("url = \"https://api.deepseek.com/v1/responses\""));
        assert!(text.contains("[providers.deepseek.anthropic]"));
        assert!(text.contains("url = \"https://api.deepseek.com/anthropic/v1/messages\""));
        assert!(text.contains("[fallback.cooldown]"));
        assert!(text.contains("[protection.bad_request]"));
        assert!(text.contains("[protection.local_reject]"));
        assert!(text.contains("openai_chat_providers"));
        assert!(!text.contains("base_url"));
        assert!(!text.contains("endpoints"));
        assert!(!text.contains("[[routes]]"));

        let parsed: Config = toml::from_str(&text).expect("round-trip parse");
        parsed.validate().expect("round-trip validates");
    }

    #[test]
    fn local_reject_config_defaults_and_parse() {
        let text = r#"
[protection.local_reject]
enabled = false
max_local_rejects = 5
window_seconds = 120
cooldown_seconds = 60
"#;
        #[derive(Deserialize)]
        struct Wrapper {
            protection: ProtectionConfig,
        }
        let parsed: Wrapper = toml::from_str(text).expect("parse wrapper");
        assert!(!parsed.protection.local_reject.enabled);
        assert_eq!(parsed.protection.local_reject.max_local_rejects, 5);
        assert_eq!(parsed.protection.local_reject.window_seconds, 120);
        assert_eq!(parsed.protection.local_reject.cooldown_seconds, 60);

        let default_cfg = LocalRejectConfig::default();
        assert!(default_cfg.enabled);
        assert_eq!(default_cfg.max_local_rejects, 10);
        assert_eq!(default_cfg.window_seconds, 600);
        assert_eq!(default_cfg.cooldown_seconds, 300);
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

        // Native Responses endpoint uses standard OpenAI Responses format (no DeepSeek thinking compat).
        let (_id, plan) = cfg
            .resolve_model_request(Protocol::OpenaiResponses, "deepseek-v4-flash-lp")
            .expect("resolve");
        assert_eq!(plan.adapter(), AdapterKind::Passthrough);
        assert_eq!(plan.source_protocol, Protocol::OpenaiResponses);
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
        ];
        for (source, target, expected) in cases {
            assert_eq!(adapter_for_pair(source, target), Some(expected));
        }
    }

    #[test]
    fn status_config_defaults_match_design_spec() {
        // 默认 probe_timeout=30、active_ttl=30、probe_concurrency=4、probe_interval_ms=150、probe_interval_jitter_ms=100
        let cfg: Config = toml::from_str(VALID_CONFIG).expect("parse");
        assert_eq!(cfg.status.probe_timeout, 30);
        assert_eq!(cfg.status.active_ttl, 30);
        assert_eq!(cfg.status.probe_concurrency, 4);
        assert_eq!(cfg.status.probe_interval_ms, 150);
        assert_eq!(cfg.status.probe_interval_jitter_ms, 100);
        assert_eq!(
            StatusConfig::default(),
            StatusConfig {
                probe_timeout: 30,
                active_ttl: 30,
                probe_concurrency: 4,
                probe_interval_ms: 150,
                probe_interval_jitter_ms: 100,
            }
        );
    }

    #[test]
    fn status_config_parses_global_and_provider_override() {
        // 全局 [status] + [providers.xxx.status] 覆盖
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
        // init 生成的默认配置应包含 [status] 段
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

    // ---- C2a: retired-key migration errors and strategy validation ----

    #[test]
    fn retired_enable_thinking_key_rejected_on_model_and_provider_toml() {
        for value in ["true", "false"] {
            let text = format!(
                "[providers.provider-a]\n\
                 api_key_env = \"K\"\n\
                 [providers.provider-a.openai_chat]\n\
                 url = \"https://api.example.com/v1/chat/completions\"\n\
                 [models.model-a]\n\
                 context_window = 1\n\
                 max_output_tokens = 1\n\
                 enable_thinking = {value}\n"
            );
            let err = toml::from_str::<Config>(&text).expect_err("retired model key rejected");
            assert!(
                err.to_string().contains("enable_thinking is retired"),
                "model value {value}: {err}"
            );
        }

        let text = "[providers.provider-a]\napi_key_env = \"K\"\nenable_thinking = true\n";
        let err = toml::from_str::<Config>(text).expect_err("retired provider key rejected");
        assert!(
            err.to_string().contains("enable_thinking is retired"),
            "provider: {err}"
        );
    }

    #[test]
    fn retired_enable_thinking_json_null_and_wrong_types_rejected() {
        for fragment in [
            "\"enable_thinking\": null",
            "\"enable_thinking\": \"yes\"",
            "\"enable_thinking\": 1",
            "\"enable_thinking\": []",
            "\"enable_thinking\": {}",
        ] {
            let text = format!("{{\"context_window\": 1, \"max_output_tokens\": 1, {fragment}}}");
            let err = serde_json::from_str::<ModelConfig>(&text)
                .expect_err("retired key presence must be rejected");
            assert!(
                err.to_string().contains("enable_thinking is retired"),
                "model {fragment}: {err}"
            );

            let text = format!("{{{fragment}}}");
            let err = serde_json::from_str::<ProviderConfig>(&text)
                .expect_err("retired key presence must be rejected");
            assert!(
                err.to_string().contains("enable_thinking is retired"),
                "provider {fragment}: {err}"
            );
        }

        // Absent key still deserializes normally.
        serde_json::from_str::<ModelConfig>("{\"context_window\": 1, \"max_output_tokens\": 1}")
            .expect("absent key deserializes");
        serde_json::from_str::<ProviderConfig>("{}").expect("absent key deserializes");
    }

    #[test]
    fn model_only_thinking_keys_rejected_on_provider() {
        for key in [
            "supports_thinking",
            "force_think",
            "force_no_think",
            "default_think",
        ] {
            let text = format!("[providers.provider-a]\napi_key_env = \"K\"\n{key} = true\n");
            let err = toml::from_str::<Config>(&text).expect_err("model-only key rejected");
            assert!(err.to_string().contains("model-only keys"), "{key}: {err}");
        }
        let text = "[providers.provider-a]\napi_key_env = \"K\"\nchat_reasoning_off_format = \"enable-thinking\"\n";
        let err = toml::from_str::<Config>(text).expect_err("model-only dialect key rejected");
        assert!(
            err.to_string().contains("model-only keys"),
            "chat_reasoning_off_format: {err}"
        );
    }

    /// Config with all thinking metadata stripped from the flash model, so
    /// each case controls exactly the fields under test.
    fn thinking_test_config() -> Config {
        let mut cfg = default_deepseek_config();
        let model = cfg.models.get_mut("deepseek-v4-flash-lp").unwrap();
        model.reasoning_levels = None;
        model.supported_reasoning_levels = Vec::new();
        model.default_reasoning_level = None;
        model.reasoning_level_map = None;
        model.supports_thinking = None;
        model.force_think = None;
        model.force_no_think = None;
        model.default_think = None;
        model.chat_reasoning_off_format = None;
        cfg
    }

    type ThinkingMutation = fn(&mut ModelConfig);

    #[test]
    fn thinking_strategy_fields_serialize_explicit_false_and_omit_none() {
        let mut cfg = thinking_test_config();
        let model = cfg.models.get_mut("deepseek-v4-flash-lp").unwrap();
        model.force_think = Some(false);
        model.default_think = Some(false);
        let text = toml::to_string_pretty(&cfg).expect("serialize");
        // explicit false must survive serialization (distinct from
        // None), while None fields are omitted entirely. Assertions are
        // scoped to the flash model's section: the pro model legitimately
        // declares supports_thinking.
        let start = text
            .find("[models.deepseek-v4-flash-lp]")
            .expect("flash section");
        let end = text[start + 1..]
            .find("\n[models.")
            .map(|offset| start + 1 + offset)
            .unwrap_or(text.len());
        let section = &text[start..end];
        assert!(section.contains("force_think = false"), "{section}");
        assert!(section.contains("default_think = false"), "{section}");
        assert!(!section.contains("supports_thinking"), "{section}");
        assert!(!section.contains("force_no_think"), "{section}");
        assert!(!section.contains("chat_reasoning_off_format"), "{section}");
        let roundtrip: Config = toml::from_str(&text).expect("deserialize");
        let model = &roundtrip.models["deepseek-v4-flash-lp"];
        assert_eq!(model.force_think, Some(false));
        assert_eq!(model.default_think, Some(false));
        assert_eq!(model.supports_thinking, None);
        assert_eq!(model.force_no_think, None);
        assert_eq!(model.chat_reasoning_off_format, None);
    }

    #[test]
    fn thinking_strategy_and_capability_contradictions_rejected() {
        let cases: [(&str, ThinkingMutation, &str); 7] = [
            (
                "both forces true",
                |m| {
                    m.force_think = Some(true);
                    m.force_no_think = Some(true);
                    m.default_reasoning_level = Some("low".to_string()); // satisfy ADR-034
                },
                "are both true",
            ),
            (
                "force on + default off",
                |m| {
                    m.force_think = Some(true);
                    m.default_reasoning_level = Some("low".to_string());
                    m.default_think = Some(false);
                },
                "conflicts with default_think=false",
            ),
            (
                "force off + default on",
                |m| {
                    m.force_no_think = Some(true);
                    m.default_think = Some(true);
                    m.default_reasoning_level = Some("low".to_string()); // satisfy ADR-034
                },
                "conflicts with default_think=true",
            ),
            (
                "capability false + force on",
                |m| {
                    m.supports_thinking = Some(false);
                    m.force_think = Some(true);
                    m.default_reasoning_level = Some("low".to_string());
                },
                "supports_thinking=false conflicts with force_think=true",
            ),
            (
                "capability false + default on",
                |m| {
                    m.supports_thinking = Some(false);
                    m.default_think = Some(true);
                    m.default_reasoning_level = Some("low".to_string()); // satisfy ADR-034
                },
                "supports_thinking=false conflicts with default_think=true",
            ),
            (
                "capability false + default level",
                |m| {
                    m.supports_thinking = Some(false);
                    m.default_reasoning_level = Some("low".to_string());
                },
                "supports_thinking=false conflicts with default_reasoning_level",
            ),
            (
                "capability false + declared levels",
                |m| {
                    m.supports_thinking = Some(false);
                    m.reasoning_levels = Some(vec![ReasoningLevelEntry::new("low", None)]);
                },
                "supports_thinking=false conflicts with reasoning_levels table",
            ),
        ];
        for (name, mutate, fragment) in cases {
            let mut cfg = thinking_test_config();
            mutate(cfg.models.get_mut("deepseek-v4-flash-lp").unwrap());
            let err = cfg.validate().expect_err(name);
            assert!(err.to_string().contains(fragment), "{name}: {err}");
        }
    }

    #[test]
    fn thinking_strategy_legal_combinations_accepted() {
        let cases: [(&str, ThinkingMutation); 3] = [
            (
                "capability true + force off (deployment policy overrides capability)",
                |m| {
                    m.supports_thinking = Some(true);
                    m.force_no_think = Some(true);
                },
            ),
            (
                "capability false + force off (redundant but not contradictory)",
                |m| {
                    m.supports_thinking = Some(false);
                    m.force_no_think = Some(true);
                },
            ),
            (
                "default off + active default level (level only fills explicit-on)",
                |m| {
                    m.default_think = Some(false);
                    m.default_reasoning_level = Some("high".to_string());
                },
            ),
        ];
        for (name, mutate) in cases {
            let mut cfg = thinking_test_config();
            mutate(cfg.models.get_mut("deepseek-v4-flash-lp").unwrap());
            cfg.validate().unwrap_or_else(|err| panic!("{name}: {err}"));
        }
    }

    #[test]
    fn chat_reasoning_off_format_whitelist() {
        let mut cfg = thinking_test_config();
        cfg.models
            .get_mut("deepseek-v4-flash-lp")
            .unwrap()
            .chat_reasoning_off_format = Some("bogus".to_string());
        let err = cfg.validate().expect_err("unknown dialect rejected");
        assert!(err.to_string().contains("not a known dialect"), "{err}");

        let mut cfg = thinking_test_config();
        cfg.models
            .get_mut("deepseek-v4-flash-lp")
            .unwrap()
            .chat_reasoning_off_format = Some("enable-thinking".to_string());
        cfg.validate().expect("known dialect accepted");
    }

    // Unknown fields in ProviderConfig and ModelConfig must be rejected
    #[test]
    fn provider_config_rejects_unknown_fields() {
        let text = r#"
[server]
listen = "127.0.0.1:8989"

[providers.test-provider]
product = "test"
openai_chat = { url = "https://example.com/v1/chat/completions" }
totally_bogus_field = "should_fail"

[models.test-model]
context_window = 1000
max_output_tokens = 100
[[models.test-model.openai_chat_providers]]
name = "test-provider"
model = "test-upstream"
"#;
        let err: toml::de::Error = toml::from_str::<Config>(text).unwrap_err();
        assert!(
            err.to_string().contains("unknown field") || err.to_string().contains("cooldown"),
            "Expected unknown field error, got: {err}"
        );
    }

    #[test]
    fn model_config_rejects_unknown_fields() {
        let text = r#"
[server]
listen = "127.0.0.1:8989"

[providers.test-provider]
product = "test"
openai_chat = { url = "https://example.com/v1/chat/completions" }

[models.test-model]
context_window = 1000
max_output_tokens = 100
unknown_field = "should fail"
[[models.test-model.openai_chat_providers]]
name = "test-provider"
model = "test-upstream"
"#;
        let err: toml::de::Error = toml::from_str::<Config>(text).unwrap_err();
        assert!(
            err.to_string().contains("unknown field") || err.to_string().contains("unknown_field"),
            "Expected unknown field error, got: {err}"
        );
    }

    #[test]
    fn valid_config_no_unknown_field_errors() {
        let text = r#"
[server]
listen = "127.0.0.1:8989"

[providers.test-provider]
product = "test"
openai_chat = { url = "https://example.com/v1/chat/completions" }

[models.test-model]
context_window = 1000
max_output_tokens = 100
[[models.test-model.openai_chat_providers]]
name = "test-provider"
model = "test-upstream"
"#;
        let cfg: Config = toml::from_str(text).expect("valid config should parse");
        cfg.validate().expect("valid config should validate");
    }

    // ---- reasoning_levels compact syntax ----

    /// Helper: TOML requires a top-level table, so we wrap the Vec in a struct.
    #[derive(Serialize, Deserialize)]
    struct ReasoningLevelsWrapper {
        reasoning_levels: Vec<ReasoningLevelEntry>,
    }

    fn parse_levels(toml_str: &str) -> Vec<ReasoningLevelEntry> {
        let wrapper: ReasoningLevelsWrapper =
            toml::from_str(&format!("reasoning_levels = {toml_str}")).expect("parse");
        wrapper.reasoning_levels
    }

    fn serialize_levels(entries: &[ReasoningLevelEntry]) -> String {
        let wrapper = ReasoningLevelsWrapper {
            reasoning_levels: entries.to_vec(),
        };
        toml::to_string_pretty(&wrapper).expect("serialize")
    }

    #[test]
    fn reasoning_levels_deserialize_pure_string_array() {
        let entries = parse_levels(r#"["low", "medium", "high"]"#);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].level(), "low");
        assert_eq!(entries[0].api_value(), None);
        assert_eq!(entries[1].level(), "medium");
        assert_eq!(entries[1].api_value(), None);
        assert_eq!(entries[2].level(), "high");
        assert_eq!(entries[2].api_value(), None);
    }

    #[test]
    fn reasoning_levels_deserialize_full_table_format() {
        let entries =
            parse_levels(r#"[{level = "low"}, {level = "high", api_value = "extended"}]"#);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].level(), "low");
        assert_eq!(entries[0].api_value(), None);
        assert_eq!(entries[1].level(), "high");
        assert_eq!(entries[1].api_value(), Some("extended"));
    }

    #[test]
    fn reasoning_levels_deserialize_mixed_array() {
        let entries =
            parse_levels(r#"["low", "medium", {level = "high", api_value = "extended"}]"#);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].level(), "low");
        assert_eq!(entries[0].api_value(), None);
        assert_eq!(entries[1].level(), "medium");
        assert_eq!(entries[1].api_value(), None);
        assert_eq!(entries[2].level(), "high");
        assert_eq!(entries[2].api_value(), Some("extended"));
    }

    #[test]
    fn reasoning_levels_serialize_simple_as_string() {
        let entries = vec![
            ReasoningLevelEntry::Simple("low".to_string()),
            ReasoningLevelEntry::Simple("high".to_string()),
        ];
        let serialized = serialize_levels(&entries);
        assert!(
            serialized.contains("\"low\""),
            "Simple should serialize as string: {serialized}"
        );
        assert!(
            serialized.contains("\"high\""),
            "Simple should serialize as string: {serialized}"
        );
        assert!(
            !serialized.contains("level = "),
            "Simple should NOT contain table keys: {serialized}"
        );
    }

    #[test]
    fn reasoning_levels_serialize_detailed_as_table() {
        let entries = vec![ReasoningLevelEntry::Detailed {
            level: "high".to_string(),
            api_value: Some("extended".to_string()),
        }];
        let serialized = serialize_levels(&entries);
        assert!(
            serialized.contains("level"),
            "Detailed should contain level key: {serialized}"
        );
        assert!(
            serialized.contains("api_value"),
            "Detailed should contain api_value key: {serialized}"
        );
    }

    #[test]
    fn reasoning_levels_serialize_detailed_without_api_value_omits_field() {
        let entries = vec![ReasoningLevelEntry::Detailed {
            level: "low".to_string(),
            api_value: None,
        }];
        let serialized = serialize_levels(&entries);
        assert!(
            serialized.contains("level"),
            "Detailed should contain level key: {serialized}"
        );
        assert!(
            !serialized.contains("api_value"),
            "api_value=None should be skipped: {serialized}"
        );
    }

    #[test]
    fn reasoning_levels_roundtrip_mixed_array() {
        let original = vec![
            ReasoningLevelEntry::Simple("low".to_string()),
            ReasoningLevelEntry::Detailed {
                level: "high".to_string(),
                api_value: Some("extended".to_string()),
            },
        ];
        let serialized = serialize_levels(&original);
        let deserialized = parse_levels(&serialized.replace("reasoning_levels = ", ""));
        assert_eq!(original, deserialized);
    }

    #[test]
    fn reasoning_levels_roundtrip_all_simple() {
        let original = vec![
            ReasoningLevelEntry::Simple("low".to_string()),
            ReasoningLevelEntry::Simple("medium".to_string()),
            ReasoningLevelEntry::Simple("high".to_string()),
        ];
        let serialized = serialize_levels(&original);
        let deserialized = parse_levels(&serialized.replace("reasoning_levels = ", ""));
        assert_eq!(original, deserialized);
        // Verify compact output: should be string array, not [[...]] blocks
        assert!(
            !serialized.contains("[["),
            "all-Simple should not produce [[...]] blocks: {serialized}"
        );
    }

    // ---- proptest round-trip ----

    use proptest::prelude::*;

    /// Strategy: generate a `ReasoningLevelEntry` — either `Simple(level)` or
    /// `Detailed { level, api_value }` with non-empty strings.
    fn arb_reasoning_level_entry() -> impl Strategy<Value = ReasoningLevelEntry> {
        let level = "[a-z]{1,10}";
        prop_oneof![
            // Simple variant
            level.prop_map(ReasoningLevelEntry::Simple),
            // Detailed variant without api_value
            level.prop_map(|l| ReasoningLevelEntry::Detailed {
                level: l,
                api_value: None,
            }),
            // Detailed variant with api_value
            (level, level).prop_map(|(l, v)| ReasoningLevelEntry::Detailed {
                level: l,
                api_value: Some(v),
            }),
        ]
    }

    proptest! {
        #[test]
        fn reasoning_level_entry_toml_roundtrip(entry in arb_reasoning_level_entry()) {
            let wrapper = ReasoningLevelsWrapper {
                reasoning_levels: vec![entry.clone()],
            };
            let serialized = toml::to_string_pretty(&wrapper)?;
            let deserialized: ReasoningLevelsWrapper = toml::from_str(&serialized)?;
            let roundtripped = &deserialized.reasoning_levels[0];

            prop_assert_eq!(entry.level(), roundtripped.level());
            prop_assert_eq!(entry.api_value(), roundtripped.api_value());
        }

        #[test]
        fn reasoning_level_entry_vec_roundtrip(entries in proptest::collection::vec(arb_reasoning_level_entry(), 1..8)) {
            let wrapper = ReasoningLevelsWrapper {
                reasoning_levels: entries.clone(),
            };
            let serialized = toml::to_string_pretty(&wrapper)?;
            let deserialized: ReasoningLevelsWrapper = toml::from_str(&serialized)?;

            prop_assert_eq!(entries.len(), deserialized.reasoning_levels.len());
            for (original, roundtripped) in entries.iter().zip(deserialized.reasoning_levels.iter()) {
                prop_assert_eq!(original.level(), roundtripped.level());
                prop_assert_eq!(original.api_value(), roundtripped.api_value());
            }
        }

        #[test]
        fn simple_entry_serializes_as_string_not_table(entry in arb_reasoning_level_entry().prop_filter("must be Simple", |e| matches!(e, ReasoningLevelEntry::Simple(_)))) {
            let wrapper = ReasoningLevelsWrapper {
                reasoning_levels: vec![entry.clone()],
            };
            let serialized = toml::to_string_pretty(&wrapper)?;

            // Simple entries should NOT produce [[...]] block syntax
            prop_assert!(!serialized.contains("[["), "Simple entry produced block syntax");
            // Should contain the level string as a quoted value
            let expected = format!("\"{}\"", entry.level());
            prop_assert!(serialized.contains(&expected));
        }

        #[test]
        fn detailed_entry_with_api_value_serializes_as_table(entry in arb_reasoning_level_entry().prop_filter("must be Detailed with api_value", |e| matches!(e, ReasoningLevelEntry::Detailed { api_value: Some(_), .. }))) {
            let wrapper = ReasoningLevelsWrapper {
                reasoning_levels: vec![entry.clone()],
            };
            let serialized = toml::to_string_pretty(&wrapper)?;

            prop_assert!(serialized.contains("level"), "missing level key");
            prop_assert!(serialized.contains("api_value"), "missing api_value key");
        }
    }

    // ---- effective_cooldown priority and per-field fallback ----

    fn config_with_cooldown_overrides(
        provider_cooldown: Option<&str>,
        model_cooldown: Option<(&str, &str)>,
    ) -> Config {
        let mut toml = String::from(
            r#"
            [server]
            listen = "127.0.0.1:8989"

            [fallback.cooldown]
            network_seconds = 60
            server_error_seconds = 300
            rate_limit_seconds = 600
            model_unavailable_seconds = 120
            client_error_seconds = 0
            auth_error_seconds = 0

            [providers.my-provider]
            product = "test"
            openai_chat = { url = "https://example.com/v1" }
        "#,
        );
        if let Some(pc) = provider_cooldown {
            toml.push_str(&format!("[providers.my-provider.cooldown]\n{pc}\n"));
        }
        if let Some((model_id, mc)) = model_cooldown {
            toml.push_str(&format!(
                "[models.\"{model_id}\"]\ncontext_window = 1000\nmax_output_tokens = 100\n[models.\"{model_id}\".cooldown]\n{mc}\n"
            ));
        }
        toml::from_str(&toml).expect("valid test config")
    }

    #[test]
    fn effective_cooldown_returns_global_when_no_overrides() {
        let cfg = config_with_cooldown_overrides(None, None);
        let effective = cfg.effective_cooldown("any-model", "my-provider");
        assert_eq!(effective.server_error_seconds, 300);
        assert_eq!(effective.rate_limit_seconds, 600);
    }

    #[test]
    fn effective_cooldown_provider_overrides_global_per_field() {
        let cfg = config_with_cooldown_overrides(Some("server_error_seconds = 151200"), None);
        let effective = cfg.effective_cooldown("any-model", "my-provider");
        // Declared field: override from provider
        assert_eq!(effective.server_error_seconds, 151200);
        // Undeclared field: per-field fallback to global (NOT serde default)
        assert_eq!(effective.rate_limit_seconds, 600);
        assert_eq!(effective.network_seconds, 60);
    }

    #[test]
    fn effective_cooldown_model_overrides_provider_per_field() {
        let cfg = config_with_cooldown_overrides(
            Some("server_error_seconds = 3600\nrate_limit_seconds = 7200"),
            Some(("my-model", "server_error_seconds = 60")),
        );
        let effective = cfg.effective_cooldown("my-model", "my-provider");
        // Model declares server_error → use model's value
        assert_eq!(effective.server_error_seconds, 60);
        // Model doesn't declare rate_limit → fall back to provider
        assert_eq!(effective.rate_limit_seconds, 7200);
        // Neither model nor provider declares network → fall back to global
        assert_eq!(effective.network_seconds, 60);
    }

    #[test]
    fn effective_cooldown_no_model_falls_to_provider() {
        let cfg = config_with_cooldown_overrides(
            Some("server_error_seconds = 3600\nrate_limit_seconds = 7200"),
            None,
        );
        // No model-level cooldown → all fields come from provider
        let effective = cfg.effective_cooldown("any-model", "my-provider");
        assert_eq!(effective.server_error_seconds, 3600);
        assert_eq!(effective.rate_limit_seconds, 7200);
    }

    #[test]
    fn effective_cooldown_unrelated_provider_uses_global() {
        let cfg = config_with_cooldown_overrides(Some("server_error_seconds = 99999"), None);
        // Querying a different provider should use global, not my-provider's override
        let effective = cfg.effective_cooldown("any-model", "other-provider");
        assert_eq!(effective.server_error_seconds, 300);
    }

    // ---- routing model validation ----

    fn routing_config(route_toml: &str) -> Config {
        let toml_str = format!(
            r#"
[server]
listen = "127.0.0.1:0"

[providers.p]
openai_responses = {{ url = "https://example.test/responses" }}

[models."target-low"]
context_window = 1000
max_output_tokens = 100
openai_responses_providers = [{{ name = "p", model = "upstream-low" }}]

[models."target-high"]
context_window = 1000
max_output_tokens = 100
openai_responses_providers = [{{ name = "p", model = "upstream-high" }}]

[models."router"]
context_window = 0
max_output_tokens = 0
{route_toml}
"#
        );
        toml::from_str(&toml_str).expect("parse config")
    }

    #[test]
    fn routing_model_valid_routes_passes_validation() {
        let cfg = routing_config(
            r#"reasoning_routes = [
                { level = "low", target = "target-low" },
                { level = "high", target = "target-high" },
            ]"#,
        );
        assert!(cfg.models["router"].is_routing_model());
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn routing_model_unknown_target_fails_validation() {
        let cfg =
            routing_config(r#"reasoning_routes = [{ level = "low", target = "nonexistent" }]"#);
        let err = cfg.validate().unwrap_err();
        assert!(
            err.to_string().contains("unknown target model"),
            "expected unknown target error, got: {err}"
        );
    }

    #[test]
    fn routing_model_with_provider_bindings_fails_validation() {
        let toml_str = r#"
[server]
listen = "127.0.0.1:0"

[providers.p]
openai_responses = { url = "https://example.test/responses" }

[models."target"]
context_window = 1000
max_output_tokens = 100
openai_responses_providers = [{ name = "p", model = "upstream" }]

[models."router"]
context_window = 0
max_output_tokens = 0
reasoning_routes = [{ level = "low", target = "target" }]
openai_responses_providers = [{ name = "p", model = "should-not-be-here" }]
"#;
        let cfg: Config = toml::from_str(toml_str).expect("parse");
        let err = cfg.validate().unwrap_err();
        assert!(
            err.to_string().contains("must not have provider bindings"),
            "expected provider bindings error, got: {err}"
        );
    }

    #[test]
    fn routing_model_with_reasoning_levels_fails_validation() {
        let toml_str = r#"
[server]
listen = "127.0.0.1:0"

[providers.p]
openai_responses = { url = "https://example.test/responses" }

[models."target"]
context_window = 1000
max_output_tokens = 100
openai_responses_providers = [{ name = "p", model = "upstream" }]

[models."router"]
context_window = 0
max_output_tokens = 0
reasoning_routes = [{ level = "low", target = "target" }]
reasoning_levels = ["low"]
"#;
        let cfg: Config = toml::from_str(toml_str).expect("parse");
        let err = cfg.validate().unwrap_err();
        assert!(
            err.to_string().contains("declares reasoning_levels"),
            "expected reasoning_levels error, got: {err}"
        );
    }

    #[test]
    fn routing_model_duplicate_levels_fails_validation() {
        let cfg = routing_config(
            r#"reasoning_routes = [
                { level = "low", target = "target-low" },
                { level = "low", target = "target-high" },
            ]"#,
        );
        let err = cfg.validate().unwrap_err();
        assert!(
            err.to_string().contains("duplicate route level"),
            "expected duplicate level error, got: {err}"
        );
    }

    /// Routing is a single hop: a routing model must forward to an executable
    /// model, never to another routing model. This also makes cycles impossible.
    #[test]
    fn routing_model_must_target_a_regular_model() {
        let toml_str = r#"
[server]
listen = "127.0.0.1:0"

[providers.p]
openai_responses = { url = "https://example.test/responses" }

[models."target"]
context_window = 1000
max_output_tokens = 100
openai_responses_providers = [{ name = "p", model = "upstream" }]

[models."router-a"]
reasoning_routes = [{ level = "low", target = "router-b" }]

[models."router-b"]
reasoning_routes = [{ level = "low", target = "target" }]
"#;
        let cfg: Config = toml::from_str(toml_str).expect("parse");
        let err = cfg.validate().unwrap_err();
        assert!(
            err.to_string().contains("single hop"),
            "a hop whose target is itself a routing model must be a load error: {err}"
        );
    }

    #[test]
    fn routing_model_resolve_route_returns_target() {
        let cfg = routing_config(
            r#"reasoning_routes = [
                { level = "low", target = "target-low" },
                { level = "high", target = "target-high" },
            ]"#,
        );
        let router = &cfg.models["router"];
        assert_eq!(router.resolve_route("low"), Some("target-low"));
        assert_eq!(router.resolve_route("high"), Some("target-high"));
        assert_eq!(router.resolve_route("medium"), None);
    }

    #[test]
    fn routing_model_empty_table_is_a_load_error() {
        let cfg = routing_config("reasoning_routes = []");
        let err = cfg.validate().unwrap_err();
        assert!(
            err.to_string().contains("empty reasoning_routes"),
            "an explicitly empty table must not be silently treated as a regular model: {err}"
        );
    }

    #[test]
    fn routing_model_rejects_execution_layer_fields() {
        for (field, snippet) in [
            ("features", "features = [\"image_input\"]"),
            ("cooldown", "cooldown = { network_seconds = 5 }"),
            ("supports_thinking", "supports_thinking = true"),
            ("force_think", "force_think = true"),
            ("force_no_think", "force_no_think = true"),
            ("default_think", "default_think = true"),
            (
                "chat_reasoning_off_format",
                "chat_reasoning_off_format = \"enable-thinking\"",
            ),
            (
                "codex_service_tier",
                "codex_service_tier = { tiers = [\"priority\"] }",
            ),
        ] {
            let cfg = routing_config(&format!(
                "reasoning_routes = [{{ level = \"low\", target = \"target-low\" }}]\n{snippet}"
            ));
            let err = cfg
                .validate()
                .expect_err(&format!("{field} on a routing model must be rejected"));
            assert!(
                err.to_string().contains("routing model"),
                "{field}: expected a routing-model error, got: {err}"
            );
        }
    }

    #[test]
    fn routing_model_rejects_declared_windows() {
        fn set_context_window(model: &mut ModelConfig) {
            model.context_window = 4096;
        }
        fn set_max_output_tokens(model: &mut ModelConfig) {
            model.max_output_tokens = 1024;
        }
        for (field, declare) in [
            ("context_window", set_context_window as fn(&mut ModelConfig)),
            ("max_output_tokens", set_max_output_tokens),
        ] {
            let mut cfg =
                routing_config(r#"reasoning_routes = [{ level = "low", target = "target-low" }]"#);
            declare(cfg.models.get_mut("router").expect("router"));
            let err = cfg
                .validate()
                .expect_err(&format!("{field} on a routing model must be rejected"));
            assert!(
                err.to_string()
                    .contains("context_window / max_output_tokens"),
                "{field}: expected the window exclusion error, got: {err}"
            );
        }
    }

    #[test]
    fn routing_model_capability_bounds_match_consistent_targets() {
        let toml_str = r#"
[server]
listen = "127.0.0.1:0"

[providers.p]
openai_responses = { url = "https://example.test/responses" }

[models."target-a"]
context_window = 200_000
max_output_tokens = 64_000
features = ["image_input"]
openai_responses_providers = [{ name = "p", model = "target-a" }]

[models."target-b"]
context_window = 200_000
max_output_tokens = 64_000
features = ["image_input"]
openai_responses_providers = [{ name = "p", model = "target-b" }]

[models."router"]
reasoning_routes = [
    { level = "low", target = "target-a" },
    { level = "high", target = "target-b" },
]
"#;
        let cfg: Config = toml::from_str(toml_str).expect("parse");
        cfg.validate().expect("validate");
        let bounds = cfg.model_capability_bounds("router").expect("bounds");
        assert_eq!(bounds.context_window, 200_000);
        assert_eq!(bounds.max_output_tokens, 64_000);
        assert_eq!(bounds.features, vec!["image_input".to_string()]);
    }

    #[test]
    fn routing_model_targets_with_mismatched_context_window_fails_validation() {
        let toml_str = r#"
[server]
listen = "127.0.0.1:0"
[providers.p]
openai_chat = { url = "https://example.test/chat/completions" }
[models.t1]
context_window = 100_000
max_output_tokens = 4_000
openai_chat_providers = [{ name = "p", model = "t1" }]
[models.t2]
context_window = 200_000
max_output_tokens = 4_000
openai_chat_providers = [{ name = "p", model = "t2" }]
[models.router]
reasoning_routes = [
    { level = "low", target = "t1" },
    { level = "high", target = "t2" },
]
"#;
        let cfg: Config = toml::from_str(toml_str).expect("parse");
        let err = cfg
            .validate()
            .expect_err("mismatched context_window must fail");
        assert!(
            err.to_string().contains("mismatched context_window"),
            "got: {err}"
        );
    }

    #[test]
    fn routing_model_targets_with_mismatched_max_output_tokens_fails_validation() {
        let toml_str = r#"
[server]
listen = "127.0.0.1:0"
[providers.p]
openai_chat = { url = "https://example.test/chat/completions" }
[models.t1]
context_window = 100_000
max_output_tokens = 4_000
openai_chat_providers = [{ name = "p", model = "t1" }]
[models.t2]
context_window = 100_000
max_output_tokens = 8_000
openai_chat_providers = [{ name = "p", model = "t2" }]
[models.router]
reasoning_routes = [
    { level = "low", target = "t1" },
    { level = "high", target = "t2" },
]
"#;
        let cfg: Config = toml::from_str(toml_str).expect("parse");
        let err = cfg
            .validate()
            .expect_err("mismatched max_output_tokens must fail");
        assert!(
            err.to_string().contains("mismatched max_output_tokens"),
            "got: {err}"
        );
    }

    #[test]
    fn routing_model_targets_with_mismatched_features_fails_validation() {
        let toml_str = r#"
[server]
listen = "127.0.0.1:0"
[providers.p]
openai_chat = { url = "https://example.test/chat/completions" }
[models.t1]
context_window = 100_000
max_output_tokens = 4_000
features = ["image_input"]
openai_chat_providers = [{ name = "p", model = "t1" }]
[models.t2]
context_window = 100_000
max_output_tokens = 4_000
features = ["image_input", "tools"]
openai_chat_providers = [{ name = "p", model = "t2" }]
[models.router]
reasoning_routes = [
    { level = "low", target = "t1" },
    { level = "high", target = "t2" },
]
"#;
        let cfg: Config = toml::from_str(toml_str).expect("parse");
        let err = cfg.validate().expect_err("mismatched features must fail");
        assert!(
            err.to_string().contains("mismatched features"),
            "got: {err}"
        );
    }

    #[test]
    fn routing_model_targets_with_mismatched_protocols_fails_validation() {
        let toml_str = r#"
[server]
listen = "127.0.0.1:0"
[providers.p]
openai_responses = { url = "https://example.test/responses" }
openai_chat = { url = "https://example.test/chat/completions" }
[models."chat-only"]
context_window = 1000
max_output_tokens = 100
openai_chat_providers = [{ name = "p", model = "chat-only" }]
[models."both"]
context_window = 1000
max_output_tokens = 100
openai_chat_providers = [{ name = "p", model = "both-chat" }]
openai_responses_providers = [{ name = "p", model = "both-responses" }]
[models."router-partial"]
reasoning_routes = [
    { level = "low", target = "chat-only" },
    { level = "high", target = "both" },
]
"#;
        let cfg: Config = toml::from_str(toml_str).expect("parse");
        let err = cfg.validate().expect_err("mismatched protocols must fail");
        assert!(
            err.to_string().contains("mismatched protocol support"),
            "got: {err}"
        );
    }

    #[test]
    fn routing_model_targets_with_mismatched_service_tier_fails_validation() {
        let toml_str = r#"
[server]
listen = "127.0.0.1:0"
[providers.p]
openai_chat = { url = "https://example.test/chat/completions" }
[models.t1]
context_window = 100_000
max_output_tokens = 4_000
codex_service_tier = { tiers = ["fast", "default"] }
openai_chat_providers = [{ name = "p", model = "t1" }]
[models.t2]
context_window = 100_000
max_output_tokens = 4_000
openai_chat_providers = [{ name = "p", model = "t2" }]
[models.router]
reasoning_routes = [
    { level = "low", target = "t1" },
    { level = "high", target = "t2" },
]
"#;
        let cfg: Config = toml::from_str(toml_str).expect("parse");
        let err = cfg
            .validate()
            .expect_err("mismatched service tier must fail");
        assert!(
            err.to_string().contains("mismatched codex_service_tier"),
            "got: {err}"
        );
    }

    #[test]
    fn ensure_serve_listen_rejects_port_zero() {
        let toml_str = r#"
[server]
listen = "127.0.0.1:0"

[providers.p]
api_key_env = "KEY"
[providers.p.openai_chat]
url = "https://api.example.com/v1/chat/completions"

[models.m]
context_window = 1000
max_output_tokens = 100
openai_chat_providers = [{ name = "p", model = "m" }]
"#;
        let cfg: Config = toml::from_str(toml_str).expect("parse");
        let err = cfg
            .ensure_serve_listen()
            .expect_err("port 0 must be rejected at serve time");
        assert!(
            err.to_string().contains("port must not be 0"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn ensure_serve_listen_accepts_fixed_port() {
        let toml_str = r#"
[server]
listen = "127.0.0.1:8989"

[providers.p]
api_key_env = "KEY"
[providers.p.openai_chat]
url = "https://api.example.com/v1/chat/completions"

[models.m]
context_window = 1000
max_output_tokens = 100
openai_chat_providers = [{ name = "p", model = "m" }]
"#;
        let cfg: Config = toml::from_str(toml_str).expect("parse");
        cfg.ensure_serve_listen()
            .expect("should accept fixed port 8989");
    }

    /// The port-0 rejection lives at the serve entry point, not in
    /// `Config::validate()`: `validate()` guards every config edit path and
    /// unit tests legitimately bind ephemeral ports. Moving the check into
    /// `validate()` breaks ~113 fixtures that use `127.0.0.1:0`.
    #[test]
    fn validate_still_accepts_port_zero() {
        let toml_str = r#"
[server]
listen = "127.0.0.1:0"

[providers.p]
api_key_env = "KEY"
[providers.p.openai_chat]
url = "https://api.example.com/v1/chat/completions"

[models.m]
context_window = 1000
max_output_tokens = 100
openai_chat_providers = [{ name = "p", model = "m" }]
"#;
        let cfg: Config = toml::from_str(toml_str).expect("parse");
        cfg.validate()
            .expect("generic validate must keep accepting ephemeral-port fixtures");
    }

    #[test]
    fn models_for_provider_returns_sorted_referencing_models() {
        let toml_str = r#"
[server]
listen = "127.0.0.1:8989"

[providers.p1]
api_key_env = "KEY"
[providers.p1.openai_chat]
url = "https://api.example.com/v1/chat/completions"

[providers.p2]
api_key_env = "KEY"
[providers.p2.openai_chat]
url = "https://api.example.com/v1/chat/completions"

[models.beta]
context_window = 1000
max_output_tokens = 100
openai_chat_providers = [{ name = "p1", model = "m-beta" }]

[models.alpha]
context_window = 1000
max_output_tokens = 100
anthropic_providers = [{ name = "p1", model = "m-alpha" }]

[models.gamma]
context_window = 1000
max_output_tokens = 100
openai_responses_providers = [{ name = "p2", model = "m-gamma" }]
"#;
        let cfg: Config = toml::from_str(toml_str).expect("parse");
        assert_eq!(cfg.models_for_provider("p1"), vec!["alpha", "beta"]);
        assert_eq!(cfg.models_for_provider("p2"), vec!["gamma"]);
        assert!(cfg.models_for_provider("p3").is_empty());

        assert!(cfg.models["alpha"].references_provider("p1"));
        assert!(!cfg.models["alpha"].references_provider("p2"));
    }

    // ── : RemoteConfig helper function tests ──

    fn make_remote_config(
        url: &str,
        auth_token_env: &str,
        auth_mode: Option<RemoteAuthMode>,
    ) -> Config {
        let mut cfg = default_deepseek_config();
        cfg.server = None;
        cfg.remote = Some(RemoteConfig {
            url: url.to_string(),
            auth_token_env: auth_token_env.to_string(),
            auth_mode,
        });
        cfg
    }

    #[test]
    fn protocol_base_url_remote_mode() {
        let cfg = make_remote_config("https://example.com", "LLM_PROXY_API_KEY", None);
        assert_eq!(
            protocol_base_url(&cfg, "/openai/v1"),
            "https://example.com/openai/v1"
        );
    }

    #[test]
    fn protocol_base_url_remote_trailing_slash() {
        let cfg = make_remote_config("https://example.com/", "LLM_PROXY_API_KEY", None);
        assert_eq!(
            protocol_base_url(&cfg, "/openai/v1"),
            "https://example.com/openai/v1"
        );
    }

    #[test]
    fn protocol_base_url_remote_with_path_prefix() {
        let cfg = make_remote_config("https://example.com/lp", "LLM_PROXY_API_KEY", None);
        assert_eq!(
            protocol_base_url(&cfg, "/openai/v1"),
            "https://example.com/lp/openai/v1"
        );
    }

    #[test]
    fn protocol_base_url_local_fallback() {
        let mut cfg = default_deepseek_config();
        cfg.remote = None;
        assert_eq!(
            protocol_base_url(&cfg, "/openai/v1"),
            "http://127.0.0.1:8989/openai/v1"
        );
    }

    #[test]
    #[serial]
    fn resolve_remote_auth_token_from_env() {
        let env_key = "LP_TEST_TOKEN_ENV";
        unsafe { std::env::set_var(env_key, "lp-from-env") };
        let cfg = make_remote_config("https://example.com", env_key, None);
        assert_eq!(
            cfg.resolve_remote_auth_token(),
            Some("lp-from-env".to_string())
        );
        unsafe { std::env::remove_var(env_key) };
    }

    #[test]
    fn resolve_remote_auth_token_missing_env() {
        let cfg = make_remote_config("https://example.com", "LP_TEST_NONEXISTENT", None);
        assert_eq!(cfg.resolve_remote_auth_token(), None);
    }

    #[test]
    fn effective_remote_auth_mode_explicit_open() {
        let cfg = make_remote_config(
            "https://example.com",
            "LP_TEST_NOEXIST",
            Some(RemoteAuthMode::Open),
        );
        assert_eq!(
            cfg.effective_remote_auth_mode(),
            Some(RemoteAuthMode::Open),
            "explicit auth_mode should override inference"
        );
    }

    #[test]
    fn effective_remote_auth_mode_explicit_token() {
        let cfg = make_remote_config(
            "https://example.com",
            "LP_TEST_NOEXIST",
            Some(RemoteAuthMode::Token),
        );
        assert_eq!(
            cfg.effective_remote_auth_mode(),
            Some(RemoteAuthMode::Token)
        );
    }

    #[test]
    #[serial]
    fn effective_remote_auth_mode_inferred_token_from_env() {
        let env_key = "LP_TEST_INFER_TEST";
        unsafe { std::env::set_var(env_key, "lp-token") };
        let cfg = make_remote_config("https://example.com", env_key, None);
        assert_eq!(
            cfg.effective_remote_auth_mode(),
            Some(RemoteAuthMode::Token),
            "should infer Token mode when env var is set"
        );
        unsafe { std::env::remove_var(env_key) };
    }

    #[test]
    fn effective_remote_auth_mode_inferred_open() {
        let cfg = make_remote_config("https://example.com", "LP_TEST_NOEXIST_INFER", None);
        assert_eq!(
            cfg.effective_remote_auth_mode(),
            Some(RemoteAuthMode::Open),
            "should infer Open mode when env var is not set"
        );
    }

    #[test]
    fn effective_remote_auth_mode_no_remote() {
        let mut cfg = default_deepseek_config();
        cfg.remote = None;
        assert_eq!(cfg.effective_remote_auth_mode(), None);
    }

    #[test]
    fn validate_rejects_server_and_remote() {
        let mut cfg = default_deepseek_config();
        cfg.remote = Some(RemoteConfig {
            url: "https://example.com".to_string(),
            auth_token_env: "LLM_PROXY_API_KEY".to_string(),
            auth_mode: None,
        });
        let err = cfg.validate().unwrap_err();
        assert!(
            err.to_string()
                .contains("Cannot have both [server] and [remote]"),
            "should reject config with both [server] and [remote]: {err}"
        );
    }

    #[test]
    fn remote_config_default_auth_token_env() {
        let toml_str = r#"
[remote]
url = "https://example.com"
"#;
        let cfg: Config = toml::from_str(toml_str).expect("parse");
        let remote = cfg.remote.expect("should have remote");
        assert_eq!(
            remote.auth_token_env, "LLM_PROXY_API_KEY",
            "auth_token_env should default to LLM_PROXY_API_KEY"
        );
    }

    /// Regression test : the E2E config template must be parseable
    /// by `Config::load`.  Catches retired keys (e.g. `enable_thinking`) that
    /// would cause immediate rejection.
    #[test]
    fn e2e_config_template_is_parseable() {
        let template_path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("scripts")
            .join("e2e")
            .join("config.example.toml");
        let cfg = Config::load(&template_path)
            .expect("scripts/e2e/config.example.toml must be loadable by Config::load");
        // Sanity: both example models should be present.
        assert!(cfg.models.contains_key("deepseek-v4-pro-lp"));
        assert!(cfg.models.contains_key("deepseek-v4-pro-nothink-lp"));
    }

    // ── : management_port 配置字段 ──────────────────────────────

    #[test]
    fn management_port_defaults_to_8989_when_absent() {
        let toml_str = r#"
[server]
listen = "127.0.0.1:8989"
"#;
        let cfg: Config = toml::from_str(toml_str).expect("parse");
        let server = cfg.server.as_ref().expect("server present");
        assert_eq!(server.management_port, None);
        assert_eq!(server.management_port(), 8989);
    }

    #[test]
    fn management_port_uses_configured_value() {
        let toml_str = r#"
[server]
listen = "127.0.0.1:8989"
management_port = 9999
"#;
        let cfg: Config = toml::from_str(toml_str).expect("parse");
        let server = cfg.server.as_ref().expect("server present");
        assert_eq!(server.management_port, Some(9999));
        assert_eq!(server.management_port(), 9999);
    }

    #[test]
    fn management_port_accepts_different_ports_for_multi_instance() {
        let toml_a = r#"
[server]
listen = "127.0.0.1:8989"
management_port = 9001
"#;
        let toml_b = r#"
[server]
listen = "127.0.0.1:8990"
management_port = 9002
"#;
        let cfg_a: Config = toml::from_str(toml_a).expect("parse a");
        let cfg_b: Config = toml::from_str(toml_b).expect("parse b");
        let port_a = cfg_a.server.as_ref().unwrap().management_port();
        let port_b = cfg_b.server.as_ref().unwrap().management_port();
        assert_ne!(port_a, port_b, "multi-instance must use distinct ports");
        assert_eq!(port_a, 9001);
        assert_eq!(port_b, 9002);
    }

    /// bad_request_tolerance 缺省 2（connect 未注入时的 fallback），
    /// 用户可覆盖；等于默认值时序列化跳过（渲染字节稳定），非默认值写出。
    #[test]
    fn bad_request_tolerance_default_override_and_serialize() {
        let cfg: Config = toml::from_str(
            r#"
[providers.p1]
product = "custom"
"#,
        )
        .expect("parse default");
        assert_eq!(cfg.providers["p1"].bad_request_tolerance, 2);

        let cfg3: Config = toml::from_str(
            r#"
[providers.p1]
product = "custom"
bad_request_tolerance = 3
"#,
        )
        .expect("parse override");
        assert_eq!(cfg3.providers["p1"].bad_request_tolerance, 3);

        let rendered = toml::to_string(&cfg).expect("serialize default");
        assert!(
            !rendered.contains("bad_request_tolerance"),
            "default value must be omitted from rendered config: {rendered}"
        );

        let rendered3 = toml::to_string(&cfg3).expect("serialize override");
        assert!(
            rendered3.contains("bad_request_tolerance = 3"),
            "user override must survive a render round-trip: {rendered3}"
        );
    }

    #[test]
    fn config_identity_same_path_produces_same_hash() {
        let dir = std::env::temp_dir().join("llm-proxy-test-identity");
        let a = config_identity(&dir);
        let b = config_identity(&dir);
        assert_eq!(a, b, "same path must produce identical identity");
        assert_eq!(a.len(), 16, "identity is 16-char hex");
    }

    #[test]
    fn config_identity_different_paths_produce_different_hashes() {
        let a = config_identity(Path::new("/tmp/llm-proxy-instance-a"));
        let b = config_identity(Path::new("/tmp/llm-proxy-instance-b"));
        assert_ne!(a, b, "different paths must produce different identities");
    }

    #[test]
    fn config_identity_symlink_resolves_to_same_hash() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let real = tmp.path().join("real-config");
        std::fs::create_dir(&real).expect("mkdir real");
        let link = tmp.path().join("link-to-config");
        std::os::unix::fs::symlink(&real, &link).expect("symlink");

        let from_real = config_identity(&real);
        let from_link = config_identity(&link);
        assert_eq!(
            from_real, from_link,
            "symlink and its target must resolve to the same identity"
        );
    }
}
