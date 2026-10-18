use crate::config::{
    AuthConfig, CodexServiceTierConfig, CompatConfig, Config, EndpointConfig, ModelConfig,
    Protocol, ProviderBinding, ProviderConfig, ReasoningLevelEntry,
};

#[derive(Debug, Clone)]
pub struct ProviderCatalogEntry {
    pub id: &'static str,
    pub provider: ProviderConfig,
    pub model_templates: Vec<ModelTemplate>,
}

#[derive(Debug, Clone)]
pub struct ModelTemplate {
    pub frontend_id: &'static str,
    pub upstream_model: &'static str,
    pub context_window: i64,
    pub max_output_tokens: i64,
    pub features: &'static [&'static str],
    pub protocols: &'static [Protocol],
    pub codex_service_tier: Option<CodexServiceTierConfig>,
    pub default_reasoning_level: Option<&'static str>,
    pub reasoning_level_map: Option<&'static [(&'static str, &'static str)]>,
    /// §5.5: entry-scoped strategy/dialect declarations. Set only
    /// by entry-specific template wrappers (e.g. Coding Plan), never by the
    /// shared template tables.
    pub force_think: Option<bool>,
    pub chat_reasoning_off_format: Option<&'static str>,
}

pub struct ModelDefaults<'a> {
    pub context_window: i64,
    pub max_output_tokens: i64,
    pub features: &'a [&'a str],
    pub protocols: &'a [Protocol],
    pub codex_service_tier: Option<CodexServiceTierConfig>,
    pub default_reasoning_level: Option<&'a str>,
    pub reasoning_level_map: Option<&'a [(&'a str, &'a str)]>,
    pub force_think: Option<bool>,
    pub chat_reasoning_off_format: Option<&'a str>,
}

impl<'a> From<&'a ModelTemplate> for ModelDefaults<'a> {
    fn from(template: &'a ModelTemplate) -> Self {
        Self {
            context_window: template.context_window,
            max_output_tokens: template.max_output_tokens,
            features: template.features,
            protocols: template.protocols,
            codex_service_tier: template.codex_service_tier.clone(),
            default_reasoning_level: template.default_reasoning_level,
            reasoning_level_map: template.reasoning_level_map,
            force_think: template.force_think,
            chat_reasoning_off_format: template.chat_reasoning_off_format,
        }
    }
}

impl ProviderCatalogEntry {
    pub fn with_templates(mut self, templates: Vec<ModelTemplate>) -> Self {
        self.model_templates = templates;
        self
    }
}

pub fn built_in_providers() -> Vec<ProviderCatalogEntry> {
    vec![
        deepseek(),
        openai_payg(),
        openai_sub(),
        openrouter(),
        anthropic(),
        ollama(),
        kimi_platform_global(),
        kimi_platform_cn(),
        kimi_sub(),
        zhipu_payg_global(),
        zhipu_payg_cn(),
        zhipu_coding_cn(),
        bailian_coding_plan_cn(),
        bailian_token_plan_personal_cn(),
        bailian_token_plan_team_cn(),
        bailian_payg_cn(),
        bailian_payg_us(),
        mimo_payg(),
        mimo_token_plan_cn(),
        mimo_token_plan_sgp(),
        mimo_token_plan_ams(),
        stepfun_payg(),
        stepfun_step_plan(),
    ]
}

fn entry(id: &'static str, mut provider: ProviderConfig) -> ProviderCatalogEntry {
    // Set the product field to the entry id so that providers created from
    // the catalog automatically belong to their product (e.g., "deepseek").
    provider.product = id.to_string();
    ProviderCatalogEntry {
        id,
        provider,
        model_templates: vec![],
    }
}

const ALL_CLIENT_PROTOCOLS: &[Protocol] = &Protocol::CLIENT_PROTOCOLS;
const OPENAI_CHAT_ONLY: &[Protocol] = &[Protocol::OpenaiChatCompletions];

const fn tmpl(fid: &'static str, upstream: &'static str, ctx: i64, max_out: i64) -> ModelTemplate {
    ModelTemplate {
        frontend_id: fid,
        upstream_model: upstream,
        context_window: ctx,
        max_output_tokens: max_out,
        features: &[],
        protocols: ALL_CLIENT_PROTOCOLS,
        codex_service_tier: None,
        default_reasoning_level: None,
        reasoning_level_map: None,
        force_think: None,
        chat_reasoning_off_format: None,
    }
}

const fn model_template(
    fid: &'static str,
    upstream: &'static str,
    ctx: i64,
    max_out: i64,
    features: &'static [&'static str],
    protocols: &'static [Protocol],
) -> ModelTemplate {
    ModelTemplate {
        frontend_id: fid,
        upstream_model: upstream,
        context_window: ctx,
        max_output_tokens: max_out,
        features,
        protocols,
        codex_service_tier: None,
        default_reasoning_level: None,
        reasoning_level_map: None,
        force_think: None,
        chat_reasoning_off_format: None,
    }
}

const QWEN_THINKING_LEVELS: &[(&str, &str)] =
    &[("low", "low"), ("medium", "medium"), ("high", "high")];
const KIMI_K3_THINKING_LEVELS: &[(&str, &str)] =
    &[("low", "low"), ("high", "high"), ("max", "max")];
const STEPFUN_THINKING_LEVELS: &[(&str, &str)] =
    &[("low", "low"), ("medium", "medium"), ("high", "high")];
fn reasoning_template(
    fid: &'static str,
    upstream: &'static str,
    ctx: i64,
    max_out: i64,
    default_reasoning_level: &'static str,
    reasoning_level_map: &'static [(&'static str, &'static str)],
) -> ModelTemplate {
    ModelTemplate {
        default_reasoning_level: Some(default_reasoning_level),
        reasoning_level_map: Some(reasoning_level_map),
        ..tmpl(fid, upstream, ctx, max_out)
    }
}

fn priority_tier_reasoning_template(
    fid: &'static str,
    upstream: &'static str,
    ctx: i64,
    max_out: i64,
    default_reasoning_level: &'static str,
    reasoning_level_map: &'static [(&'static str, &'static str)],
) -> ModelTemplate {
    ModelTemplate {
        codex_service_tier: Some(CodexServiceTierConfig {
            tiers: vec!["priority".to_string()],
            default_tier: Some("priority".to_string()),
            forced_tier: None,
        }),
        ..reasoning_template(
            fid,
            upstream,
            ctx,
            max_out,
            default_reasoning_level,
            reasoning_level_map,
        )
    }
}

fn deepseek_thinking_compat() -> CompatConfig {
    CompatConfig {
        supports_developer_role: Some(false),
        supports_reasoning_effort: Some(true),
        thinking_format: Some("deepseek".to_string()),
        requires_reasoning_content_on_assistant_messages: Some(true),
        max_tokens_field: Some("max_tokens".to_string()),
        ..CompatConfig::default()
    }
}

/// 百炼 Coding Plan Chat 端点 compat：Qwen thinking 格式，不支持 reasoning_effort，
/// 多轮 tool call 需要 reasoning_content 回传。
fn bailian_coding_plan_chat_compat() -> CompatConfig {
    CompatConfig {
        supports_developer_role: Some(false),
        supports_reasoning_effort: Some(false),
        thinking_format: Some("qwen".to_string()),
        requires_reasoning_content_on_assistant_messages: Some(true),
        max_tokens_field: Some("max_tokens".to_string()),
        ..CompatConfig::default()
    }
}

/// MiMo 的推理强度是**二态**（provider-mimo.md §5.5），本表只做**入参词表归一**：官方
/// Responses `reasoning.effort` 接受 8 个标签，`none` 表示关思考（不经过档位门禁，故不入
/// 表），其余全部落在启用态，且 `minimal`→`low`、`xhigh`/`max`/`ultra`→`high`（L2 原文
/// `official-snapshots/mimo-token-plan-20260922.md` §8）。归一后的 `api_value` 才是写进
/// 上游请求体的值，所以别名行不声明任何新档位，只是让客户端用官方词表内的标签时不被本地
/// 误拒——本表同时是 `client_reasoning_levels()` 的 advertise 来源（MiMo 模型无自有表，走
/// provider 回退），改动它等于改动客户端可见的档位列表。
pub fn mimo_reasoning_level_map() -> Vec<ReasoningLevelEntry> {
    vec![
        ReasoningLevelEntry::new("minimal", Some("low".to_string())),
        ReasoningLevelEntry::new("low", Some("low".to_string())),
        ReasoningLevelEntry::new("medium", Some("medium".to_string())),
        ReasoningLevelEntry::new("high", Some("high".to_string())),
        ReasoningLevelEntry::new("xhigh", Some("high".to_string())),
        ReasoningLevelEntry::new("max", Some("high".to_string())),
        ReasoningLevelEntry::new("ultra", Some("high".to_string())),
    ]
}

fn bailian_templates() -> Vec<ModelTemplate> {
    vec![
        ModelTemplate {
            default_reasoning_level: Some("medium"),
            reasoning_level_map: Some(QWEN_THINKING_LEVELS),
            ..model_template(
                "qwen3.7-plus-bailian-lp",
                "qwen3.7-plus",
                1_000_000,
                65_536,
                &["image_input"],
                ALL_CLIENT_PROTOCOLS,
            )
        },
        model_template(
            "qwen3.6-plus-bailian-lp",
            "qwen3.6-plus",
            1_000_000,
            65_536,
            &["image_input"],
            ALL_CLIENT_PROTOCOLS,
        ),
        model_template(
            "qwen3.5-plus-bailian-lp",
            "qwen3.5-plus",
            1_000_000,
            65_536,
            &["image_input"],
            ALL_CLIENT_PROTOCOLS,
        ),
        model_template(
            "kimi-k2.5-bailian-lp",
            "kimi-k2.5",
            262_144,
            98_304,
            &["image_input"],
            ALL_CLIENT_PROTOCOLS,
        ),
        tmpl("glm-5-bailian-lp", "glm-5", 202_752, 2_000_000),
        tmpl("minimax-m2.5-bailian-lp", "MiniMax-M2.5", 196_608, 32_768),
        tmpl(
            "qwen3-max-2026-01-23-bailian-lp",
            "qwen3-max-2026-01-23",
            262_144,
            32_768,
        ),
        tmpl(
            "qwen3-coder-next-bailian-lp",
            "qwen3-coder-next",
            262_144,
            65_536,
        ),
        tmpl(
            "qwen3-coder-plus-bailian-lp",
            "qwen3-coder-plus",
            1_000_000,
            65_536,
        ),
        tmpl("glm-4.7-bailian-lp", "glm-4.7", 202_752, 16_384),
    ]
}

/// §5.5: Coding-Plan-only strategy/dialect declarations layered on
/// the shared Bailian templates. PAYG entries must not inherit these:
/// - `minimax-m2.5-bailian-lp`: upstream rejects thinking-off attempts with
///   400, so the Coding Plan model forces thinking on.
/// - `glm-4.7-bailian-lp`: evidence supports the independent Chat off dialect
///   `enable_thinking=false` on Coding Plan Chat egress.
fn bailian_coding_plan_templates() -> Vec<ModelTemplate> {
    bailian_templates()
        .into_iter()
        .map(|mut template| {
            match template.frontend_id {
                "minimax-m2.5-bailian-lp" => {
                    template.force_think = Some(true);
                    template.default_reasoning_level = Some("high");
                }
                "glm-4.7-bailian-lp" => {
                    template.chat_reasoning_off_format = Some("enable-thinking");
                }
                _ => {}
            }
            template
        })
        .collect()
}

/// Token Plan **personal edition** model templates.
/// ⚠️ Data policy: input/output used for service improvement and model optimization.
/// Verified 2026-09-15 from official docs.
fn bailian_token_plan_personal_templates() -> Vec<ModelTemplate> {
    vec![
        // ── Qwen ──
        model_template(
            "qwen3.8-max-token-plan-personal-lp",
            "qwen3.8-max",
            1_000_000,
            131_072,
            &["image_input"],
            ALL_CLIENT_PROTOCOLS,
        ),
        tmpl(
            "qwen3.8-flash-token-plan-personal-lp",
            "qwen3.8-flash",
            1_000_000,
            65_536,
        ),
        reasoning_template(
            "qwen3.7-max-token-plan-personal-lp",
            "qwen3.7-max",
            1_000_000,
            131_072,
            "medium",
            QWEN_THINKING_LEVELS,
        ),
        ModelTemplate {
            default_reasoning_level: Some("medium"),
            reasoning_level_map: Some(QWEN_THINKING_LEVELS),
            ..model_template(
                "qwen3.7-plus-token-plan-personal-lp",
                "qwen3.7-plus",
                1_000_000,
                65_536,
                &["image_input"],
                ALL_CLIENT_PROTOCOLS,
            )
        },
        model_template(
            "qwen3.6-flash-token-plan-personal-lp",
            "qwen3.6-flash",
            1_000_000,
            65_536,
            &["image_input"],
            ALL_CLIENT_PROTOCOLS,
        ),
        // ── DeepSeek ──
        tmpl(
            "deepseek-v4.1-flash-token-plan-personal-lp",
            "deepseek-v4.1-flash",
            1_000_000,
            65_536,
        ),
        tmpl(
            "deepseek-v4-pro-token-plan-personal-lp",
            "deepseek-v4-pro",
            1_000_000,
            65_536,
        ),
        tmpl(
            "deepseek-v4-pro-0813-token-plan-personal-lp",
            "deepseek-v4-pro-0813",
            1_000_000,
            65_536,
        ),
        tmpl(
            "deepseek-v4-flash-0731-token-plan-personal-lp",
            "deepseek-v4-flash-0731",
            1_000_000,
            65_536,
        ),
        // ── Zhipu GLM ──
        tmpl(
            "glm-5.2-token-plan-personal-lp",
            "glm-5.2",
            1_000_000,
            32_768,
        ),
    ]
}

/// Token Plan **team edition** model templates.
/// ✅ Data policy: does NOT use conversation data for model training.
/// Includes all personal edition models plus team-exclusive models.
/// Verified 2026-09-15 from official docs.
fn bailian_token_plan_team_templates() -> Vec<ModelTemplate> {
    vec![
        // ── Qwen (same as personal + qwen3.6-plus) ──
        model_template(
            "qwen3.8-max-token-plan-team-lp",
            "qwen3.8-max",
            1_000_000,
            131_072,
            &["image_input"],
            ALL_CLIENT_PROTOCOLS,
        ),
        tmpl(
            "qwen3.8-flash-token-plan-team-lp",
            "qwen3.8-flash",
            1_000_000,
            65_536,
        ),
        reasoning_template(
            "qwen3.7-max-token-plan-team-lp",
            "qwen3.7-max",
            1_000_000,
            131_072,
            "medium",
            QWEN_THINKING_LEVELS,
        ),
        ModelTemplate {
            default_reasoning_level: Some("medium"),
            reasoning_level_map: Some(QWEN_THINKING_LEVELS),
            ..model_template(
                "qwen3.7-plus-token-plan-team-lp",
                "qwen3.7-plus",
                1_000_000,
                65_536,
                &["image_input"],
                ALL_CLIENT_PROTOCOLS,
            )
        },
        tmpl(
            "qwen3.6-plus-token-plan-team-lp",
            "qwen3.6-plus",
            1_000_000,
            65_536,
        ),
        model_template(
            "qwen3.6-flash-token-plan-team-lp",
            "qwen3.6-flash",
            1_000_000,
            65_536,
            &["image_input"],
            ALL_CLIENT_PROTOCOLS,
        ),
        // ── DeepSeek (same as personal + v4-flash, v3.2) ──
        tmpl(
            "deepseek-v4.1-flash-token-plan-team-lp",
            "deepseek-v4.1-flash",
            1_000_000,
            65_536,
        ),
        tmpl(
            "deepseek-v4-pro-token-plan-team-lp",
            "deepseek-v4-pro",
            1_000_000,
            65_536,
        ),
        tmpl(
            "deepseek-v4-pro-0813-token-plan-team-lp",
            "deepseek-v4-pro-0813",
            1_000_000,
            65_536,
        ),
        tmpl(
            "deepseek-v4-flash-token-plan-team-lp",
            "deepseek-v4-flash",
            1_000_000,
            65_536,
        ),
        tmpl(
            "deepseek-v4-flash-0731-token-plan-team-lp",
            "deepseek-v4-flash-0731",
            1_000_000,
            65_536,
        ),
        tmpl(
            "deepseek-v3.2-token-plan-team-lp",
            "deepseek-v3.2",
            131_072,
            65_536,
        ),
        // ── Kimi (team exclusive) ──
        // k2.7-code: accepts enable_thinking=false but silently ignores it —
        // thinking stays ON regardless. Force thinking to avoid invalid config.
        ModelTemplate {
            force_think: Some(true),
            ..tmpl(
                "kimi-k2.7-code-token-plan-team-lp",
                "kimi-k2.7-code",
                262_144,
                65_536,
            )
        },
        tmpl("kimi-k2.6-token-plan-team-lp", "kimi-k2.6", 262_144, 65_536),
        tmpl("kimi-k2.5-token-plan-team-lp", "kimi-k2.5", 262_144, 65_536),
        // ── Zhipu GLM (same as personal + glm-5.1, glm-5) ──
        tmpl("glm-5.2-token-plan-team-lp", "glm-5.2", 1_000_000, 32_768),
        tmpl("glm-5.1-token-plan-team-lp", "glm-5.1", 202_752, 32_768),
        tmpl("glm-5-token-plan-team-lp", "glm-5", 202_752, 2_000_000),
        // ── MiniMax (team exclusive) ──
        // MiniMax-M2.5: rejects enable_thinking=false with 400 —
        // "restricted to True". Thinking is always ON.
        ModelTemplate {
            force_think: Some(true),
            ..tmpl(
                "minimax-m2.5-token-plan-team-lp",
                "MiniMax-M2.5",
                196_608,
                32_768,
            )
        },
    ]
}

fn mimo_templates() -> Vec<ModelTemplate> {
    vec![
        // 模板顺序即 `connect` 优先级：index 0 是 `apply_catalog_model_defaults(…, None)`
        // 唯一自动安装的模型，因此 V2.6 必须在最前，V2.5 只能留在可选位。
        model_template(
            "mimo-v2.6-pro-lp",
            "mimo-v2.6-pro",
            1_048_576,
            131_072,
            &["image_input", "audio_input", "video_input"],
            ALL_CLIENT_PROTOCOLS,
        ),
        model_template(
            "mimo-v2.6-flash-lp",
            "mimo-v2.6-flash",
            1_048_576,
            131_072,
            &["image_input", "audio_input", "video_input"],
            ALL_CLIENT_PROTOCOLS,
        ),
        // V2.5 代次于 2026-10-21 10:00 CST 下线且**不自动转发**（到期后按
        // provider-mimo.md §6.4 删除这两个模板，而不是重排）。`mimo-v2.5-pro` 是
        // 多模态例外：图像入参已被 L1 否证（404 "No endpoints found that support
        // image input"），所以它不声明任何 feature。
        tmpl("mimo-v2.5-pro-lp", "mimo-v2.5-pro", 1_048_576, 131_072),
        model_template(
            "mimo-v2.5-lp",
            "mimo-v2.5",
            1_048_576,
            32_768,
            &["image_input", "audio_input", "video_input"],
            ALL_CLIENT_PROTOCOLS,
        ),
    ]
}

fn mimo_payg_templates() -> Vec<ModelTemplate> {
    let mut templates = mimo_templates();
    // UltraSpeed 只在 PAYG 侧售卖：Token Plan CN 三个出口一律 400
    // `Not supported model …`（权益分支，不是命名问题），PAYG 侧本轮无密钥可测，
    // 因此不声明任何媒体能力、不扩协议（provider-mimo.md §5.5、§6.4、§7.4）。
    templates.push(model_template(
        "mimo-v2.6-pro-ultraspeed-lp",
        "mimo-v2.6-pro-ultraspeed",
        1_048_576,
        131_072,
        &[],
        OPENAI_CHAT_ONLY,
    ));
    templates
}

fn stepfun_templates() -> Vec<ModelTemplate> {
    vec![reasoning_template(
        "step-3.7-flash-lp",
        "step-3.7-flash",
        256_000,
        32_768,
        "medium",
        STEPFUN_THINKING_LEVELS,
    )]
}

fn zhipu_templates() -> Vec<ModelTemplate> {
    vec![tmpl("glm-5.2-zhipu-cn-lp", "glm-5.2", 128_000, 32_768)]
}

fn kimi_platform_templates() -> Vec<ModelTemplate> {
    vec![
        reasoning_template(
            "kimi-k3-lp",
            "kimi-k3",
            1_048_576,
            131_072,
            "high",
            KIMI_K3_THINKING_LEVELS,
        ),
        tmpl("kimi-k2.7-code-lp", "kimi-k2.7-code", 262_144, 32_768),
        tmpl(
            "kimi-k2.7-code-highspeed-lp",
            "kimi-k2.7-code-highspeed",
            262_144,
            32_768,
        ),
        tmpl("kimi-k2.6-lp", "kimi-k2.6", 256_000, 32_768),
        tmpl("kimi-k2.5-lp", "kimi-k2.5", 262_144, 32_768),
    ]
}

/// DeepSeek officially serves Chat Completions and Anthropic Messages; the
/// DeepSeek provider configuration.
///
/// Note: DeepSeek provides a native Responses endpoint at
/// `https://api.deepseek.com/v1/responses`. Both `deepseek-v4-flash` and
/// `deepseek-v4-pro` natively support Responses (v4-pro support confirmed
/// 2026-10-01 via L1 API probe).
pub fn deepseek() -> ProviderCatalogEntry {
    entry(
        "deepseek",
        ProviderConfig {
            api_key_env: Some("DEEPSEEK_API_KEY".to_string()),
            openai_chat: Some(
                EndpointConfig::native("https://api.deepseek.com/chat/completions")
                    .with_compat(deepseek_thinking_compat()),
            ),
            openai_responses: Some(EndpointConfig::native(
                "https://api.deepseek.com/v1/responses",
            )),
            anthropic: Some(EndpointConfig::native(
                "https://api.deepseek.com/anthropic/v1/messages",
            )),
            ..ProviderConfig::default()
        },
    )
    .with_templates(vec![
        tmpl(
            "deepseek-v4-flash-lp",
            "deepseek-v4-flash",
            1_000_000,
            393_216,
        ),
        tmpl("deepseek-v4-pro-lp", "deepseek-v4-pro", 1_000_000, 393_216),
    ])
}

const OPENAI_6_THINKING_LEVELS: &[(&str, &str)] = &[
    ("low", "low"),
    ("medium", "medium"),
    ("high", "high"),
    ("xhigh", "xhigh"),
    ("max", "max"),
    ("ultra", "ultra"),
];

const OPENAI_LUNA_THINKING_LEVELS: &[(&str, &str)] = &[
    ("low", "low"),
    ("medium", "medium"),
    ("high", "high"),
    ("xhigh", "xhigh"),
    ("max", "max"),
];

const OPENAI_5_THINKING_LEVELS: &[(&str, &str)] = &[
    ("low", "low"),
    ("medium", "medium"),
    ("high", "high"),
    ("xhigh", "xhigh"),
];

pub fn openai_payg() -> ProviderCatalogEntry {
    entry(
        "openai-payg",
        ProviderConfig {
            api_key_env: Some("OPENAI_API_KEY".to_string()),
            openai_chat: Some(EndpointConfig::native(
                "https://api.openai.com/v1/chat/completions",
            )),
            openai_responses: Some(EndpointConfig::native(
                "https://api.openai.com/v1/responses",
            )),
            anthropic: Some(EndpointConfig::derived(Protocol::OpenaiChatCompletions)),
            ..ProviderConfig::default()
        },
    )
    .with_templates(vec![reasoning_template(
        "gpt-5.5-lp",
        "gpt-5.5",
        400_000,
        128_000,
        "medium",
        OPENAI_5_THINKING_LEVELS,
    )])
}

pub fn openai_sub() -> ProviderCatalogEntry {
    entry(
        "openai-sub",
        ProviderConfig {
            auth: Some(AuthConfig::OpenaiOauth {
                account: Some("openai-subscription".to_string()),
            }),
            openai_responses: Some(
                EndpointConfig::native("https://chatgpt.com/backend-api/codex/responses")
                    .with_compat(CompatConfig {
                        // ChatGPT Codex backend requires streaming, rejects
                        // max_output_tokens, and requires store=false
                        // (L1 verified 2026-08-01, research §3.1/§3.1a).
                        force_stream: Some(true),
                        strip_max_output_tokens: Some(true),
                        must_not_store: Some(true),
                        ..CompatConfig::default()
                    }),
            ),
            openai_chat: Some(EndpointConfig::derived(Protocol::OpenaiResponses)),
            anthropic: Some(EndpointConfig::derived(Protocol::OpenaiResponses)),
            ..ProviderConfig::default()
        },
    )
    .with_templates(vec![
        // Official OpenAI per-model docs confirm 1,050,000 context and 128,000 max output
        // for GPT-6 Astra and GPT-5.6 Sol/Terra/Luna.
        // Astra does not configure priority/Fast because upstream requests report service_tier=default.
        // gpt-5.3-codex-spark and gpt-5.2 returned HTTP 400 and remain excluded.
        // Image generation models (gpt-image-*) and duplicate alias gpt-5.6 remain excluded.
        reasoning_template(
            "gpt-5.6-sol-sub-lp",
            "gpt-5.6-sol",
            1_050_000,
            128_000,
            "low",
            OPENAI_6_THINKING_LEVELS,
        ),
        reasoning_template(
            "gpt-5.6-terra-sub-lp",
            "gpt-5.6-terra",
            1_050_000,
            128_000,
            "medium",
            OPENAI_6_THINKING_LEVELS,
        ),
        reasoning_template(
            "gpt-5.6-luna-sub-lp",
            "gpt-5.6-luna",
            1_050_000,
            128_000,
            "medium",
            OPENAI_LUNA_THINKING_LEVELS,
        ),
        reasoning_template(
            "gpt-6-astra-sub-lp",
            "gpt-6-astra",
            1_050_000,
            128_000,
            "low",
            OPENAI_6_THINKING_LEVELS,
        ),
        priority_tier_reasoning_template(
            "gpt-5.5-sub-lp",
            "gpt-5.5",
            272_000,
            128_000,
            "medium",
            OPENAI_5_THINKING_LEVELS,
        ),
        priority_tier_reasoning_template(
            "gpt-5.4-sub-lp",
            "gpt-5.4",
            272_000,
            128_000,
            "medium",
            OPENAI_5_THINKING_LEVELS,
        ),
        reasoning_template(
            "gpt-5.4-mini-sub-lp",
            "gpt-5.4-mini",
            272_000,
            32_768,
            "medium",
            OPENAI_5_THINKING_LEVELS,
        ),
        reasoning_template(
            "codex-auto-review-sub-lp",
            "codex-auto-review",
            272_000,
            128_000,
            "medium",
            OPENAI_LUNA_THINKING_LEVELS,
        ),
    ])
}

pub fn openrouter() -> ProviderCatalogEntry {
    entry(
        "openrouter",
        ProviderConfig {
            api_key_env: Some("OPENROUTER_API_KEY".to_string()),
            openai_chat: Some(
                EndpointConfig::native("https://openrouter.ai/api/v1/chat/completions")
                    .with_compat(openrouter_chat_compat()),
            ),
            openai_responses: Some(EndpointConfig::derived(Protocol::OpenaiChatCompletions)),
            anthropic: Some(EndpointConfig::native(
                "https://openrouter.ai/api/v1/messages",
            )),
            ..ProviderConfig::default()
        },
    )
}

/// OpenRouter Chat 端点 compat（调研 provider-openrouter.md §Endpoint compat）：
/// - 无 developer role（仅 user/assistant/system/tool）
/// - 归一化 reasoning_details（Chat 端点），需按原序回显
/// - 支持 reasoning effort（reasoning 对象，effort 值 max/xhigh/high/medium/low/minimal/none）
fn openrouter_chat_compat() -> CompatConfig {
    CompatConfig {
        supports_developer_role: Some(false),
        supports_reasoning_effort: Some(true),
        thinking_format: Some("reasoning_details".to_string()),
        requires_reasoning_content_on_assistant_messages: Some(true),
        ..CompatConfig::default()
    }
}

pub fn anthropic() -> ProviderCatalogEntry {
    entry(
        "anthropic",
        ProviderConfig {
            api_key_env: Some("ANTHROPIC_API_KEY".to_string()),
            openai_chat: Some(EndpointConfig::derived(Protocol::Anthropic)),
            openai_responses: Some(EndpointConfig::derived(Protocol::Anthropic)),
            anthropic: Some(EndpointConfig::native(
                "https://api.anthropic.com/v1/messages",
            )),
            ..ProviderConfig::default()
        },
    )
    .with_templates(vec![tmpl(
        "claude-sonnet-lp",
        "claude-sonnet-5",
        1_000_000,
        128_000,
    )])
}

/// Ollama exposes native OpenAI Chat, OpenAI Responses, and Anthropic
/// Messages local compatibility endpoints. All three are documented by
/// Ollama as direct API surfaces.
pub fn ollama() -> ProviderCatalogEntry {
    entry(
        "ollama",
        ProviderConfig {
            api_key_env: None,
            openai_chat: Some(EndpointConfig::native(
                "http://127.0.0.1:11434/v1/chat/completions",
            )),
            openai_responses: Some(EndpointConfig::native(
                "http://127.0.0.1:11434/v1/responses",
            )),
            anthropic: Some(EndpointConfig::native("http://127.0.0.1:11434/v1/messages")),
            ..ProviderConfig::default()
        },
    )
    .with_templates(vec![tmpl("qwen3-27b-lp", "qwen3:27b", 262_144, 32_768)])
}

/// chat_with_derived + Chat 端点 compat（用于有 dialect 要求的 provider）。
fn chat_with_derived_compat(
    api_key_env: &str,
    chat_url: &str,
    compat: CompatConfig,
) -> ProviderConfig {
    ProviderConfig {
        api_key_env: Some(api_key_env.to_string()),
        openai_chat: Some(EndpointConfig::native(chat_url).with_compat(compat)),
        openai_responses: Some(EndpointConfig::derived(Protocol::OpenaiChatCompletions)),
        anthropic: Some(EndpointConfig::derived(Protocol::OpenaiChatCompletions)),
        ..ProviderConfig::default()
    }
}

fn kimi_platform_chat_compat() -> CompatConfig {
    CompatConfig {
        supports_developer_role: Some(false),
        supports_reasoning_effort: None, // model-specific: K3 yes, K2.x no
        thinking_format: None, // model-specific: K3 uses reasoning_effort, K2.x uses thinking object
        requires_reasoning_content_on_assistant_messages: Some(true),
        max_tokens_field: Some("max_completion_tokens".to_string()), // 官方文档推荐；L1 实测 2026-08-07 确认 max_tokens 仍被接受（非硬性错误）
        ..CompatConfig::default()
    }
}

pub fn kimi_platform_global() -> ProviderCatalogEntry {
    entry(
        "kimi-platform-global",
        ProviderConfig {
            api_key_env: Some("MOONSHOT_API_KEY".to_string()),
            openai_chat: Some(
                EndpointConfig::native("https://api.moonshot.ai/v1/chat/completions")
                    .with_compat(kimi_platform_chat_compat()),
            ),
            openai_responses: Some(EndpointConfig::derived(Protocol::OpenaiChatCompletions)),
            anthropic: Some(EndpointConfig::native(
                "https://api.moonshot.ai/anthropic/v1/messages",
            )),
            ..ProviderConfig::default()
        },
    )
    .with_templates(kimi_platform_templates())
}

pub fn kimi_platform_cn() -> ProviderCatalogEntry {
    entry(
        "kimi-platform-cn",
        ProviderConfig {
            api_key_env: Some("MOONSHOT_API_KEY".to_string()),
            openai_chat: Some(
                EndpointConfig::native("https://api.moonshot.cn/v1/chat/completions")
                    .with_compat(kimi_platform_chat_compat()),
            ),
            openai_responses: Some(EndpointConfig::derived(Protocol::OpenaiChatCompletions)),
            anthropic: Some(EndpointConfig::native(
                "https://api.moonshot.cn/anthropic/v1/messages",
            )),
            ..ProviderConfig::default()
        },
    )
    .with_templates(kimi_platform_templates())
}

/// Kimi Code is a membership-subscription product for coding agents.
/// It issues API keys from the Kimi Code Console and exposes both
/// OpenAI Chat and Anthropic Messages native endpoints.
/// Note: the Chat endpoint has a User-Agent whitelist; the Anthropic
/// endpoint is the recommended path for non-whitelisted clients.
pub fn kimi_sub() -> ProviderCatalogEntry {
    entry(
        "kimi-sub",
        ProviderConfig {
            api_key_env: Some("KIMI_CODE_API_KEY".to_string()),
            openai_chat: Some(
                EndpointConfig::native("https://api.kimi.com/coding/v1/chat/completions")
                    .with_compat(kimi_platform_chat_compat()),
            ),
            openai_responses: Some(EndpointConfig::derived(Protocol::OpenaiChatCompletions)),
            anthropic: Some(EndpointConfig::native(
                "https://api.kimi.com/coding/v1/messages",
            )),
            ..ProviderConfig::default()
        },
    )
    .with_templates(vec![
        reasoning_template(
            "k3-kimi-sub-lp",
            "k3",
            1_048_576,
            131_072,
            "high",
            KIMI_K3_THINKING_LEVELS,
        ),
        reasoning_template(
            "k3-256k-kimi-sub-lp",
            "k3-256k",
            262_144,
            65_536,
            "high",
            KIMI_K3_THINKING_LEVELS,
        ),
        tmpl(
            "kimi-for-coding-kimi-sub-lp",
            "kimi-for-coding",
            1_048_576,
            65_536,
        ),
        tmpl(
            "kimi-for-coding-highspeed-kimi-sub-lp",
            "kimi-for-coding-highspeed",
            262_144,
            65_536,
        ),
    ])
}

pub fn zhipu_payg_global() -> ProviderCatalogEntry {
    entry(
        "zhipu-payg-global",
        chat_with_derived_compat(
            "ZAI_API_KEY",
            "https://api.z.ai/api/paas/v4/chat/completions",
            zhipu_chat_compat(false),
        ),
    )
    .with_templates(vec![tmpl(
        "glm-5.2-zhipu-global-lp",
        "glm-5.2",
        128_000,
        32_768,
    )])
}

pub fn zhipu_payg_cn() -> ProviderCatalogEntry {
    entry(
        "zhipu-payg-cn",
        chat_with_derived_compat(
            "BIGMODEL_API_KEY",
            "https://open.bigmodel.cn/api/paas/v4/chat/completions",
            zhipu_chat_compat(false),
        ),
    )
    .with_templates(zhipu_templates())
}

pub fn zhipu_coding_cn() -> ProviderCatalogEntry {
    entry(
        "zhipu-coding-cn",
        ProviderConfig {
            api_key_env: Some("BIGMODEL_API_KEY".to_string()),
            openai_chat: Some(
                EndpointConfig::native(
                    "https://open.bigmodel.cn/api/coding/paas/v4/chat/completions",
                )
                .with_compat(zhipu_chat_compat(true)),
            ),
            openai_responses: Some(EndpointConfig::derived(Protocol::OpenaiChatCompletions)),
            anthropic: Some(EndpointConfig::native(
                "https://open.bigmodel.cn/api/anthropic/v1/messages",
            )),
            ..ProviderConfig::default()
        },
    )
    .with_templates(zhipu_templates())
}

/// 智谱/Zhipu Chat 端点 compat（调研 provider-zhipu.md §Compat field table）：
/// - 无 developer role（仅 user/assistant/system/tool）
/// - thinking.type + clear_thinking 格式（zhipu_thinking）
/// - GLM-5.2 支持 reasoning effort
/// - Coding Plan 端点 preserved thinking 默认开启，需 reasoning_content 回传（requires_reasoning=true）
fn zhipu_chat_compat(requires_reasoning: bool) -> CompatConfig {
    CompatConfig {
        supports_developer_role: Some(false),
        supports_reasoning_effort: Some(true),
        thinking_format: Some("zhipu_thinking".to_string()),
        requires_reasoning_content_on_assistant_messages: Some(requires_reasoning),
        max_tokens_field: Some("max_tokens".to_string()),
        ..CompatConfig::default()
    }
}

pub fn bailian_coding_plan_cn() -> ProviderCatalogEntry {
    entry(
        "bailian-coding-plan-cn",
        ProviderConfig {
            api_key_env: Some("BAILIAN_CODING_PLAN_API_KEY".to_string()),
            openai_chat: Some(
                EndpointConfig::native("https://coding.dashscope.aliyuncs.com/v1/chat/completions")
                    .with_compat(bailian_coding_plan_chat_compat()),
            ),
            openai_responses: Some(EndpointConfig::derived(Protocol::OpenaiChatCompletions)),
            anthropic: Some(EndpointConfig::native(
                "https://coding.dashscope.aliyuncs.com/apps/anthropic/v1/messages",
            )),
            ..ProviderConfig::default()
        },
    )
    .with_templates(bailian_coding_plan_templates())
}

/// Bailian Token Plan **personal edition** (CN, Beijing only).
/// ⚠️ Data policy: input/output used for service improvement and model optimization.
/// Isolated API key (`sk-sp-` prefix), base URL (`token-plan.cn-beijing.maas.aliyuncs.com`).
/// Verified 2026-09-15 from official docs.
pub fn bailian_token_plan_personal_cn() -> ProviderCatalogEntry {
    entry(
        "bailian-token-plan-personal-cn",
        ProviderConfig {
            api_key_env: Some("BAILIAN_TOKEN_PLAN_PERSONAL_API_KEY".to_string()),
            openai_chat: Some(
                EndpointConfig::native(
                    "https://token-plan.cn-beijing.maas.aliyuncs.com/compatible-mode/v1/chat/completions",
                )
                .with_compat(bailian_token_plan_chat_compat()),
            ),
            // Derived from Chat: not all Token Plan models support native Responses
            // (kimi-k2.x, MiniMax-M2.5, deepseek-v3.2 return InternalError.Algo).
            // Chat conversion path is well-tested (same as Coding Plan).
            openai_responses: Some(EndpointConfig::derived(Protocol::OpenaiChatCompletions)),
            anthropic: Some(EndpointConfig::native(
                "https://token-plan.cn-beijing.maas.aliyuncs.com/apps/anthropic/v1/messages",
            )),
            ..ProviderConfig::default()
        },
    )
    .with_templates(bailian_token_plan_personal_templates())
}

/// Bailian Token Plan **team edition** (CN, Beijing only).
/// ✅ Data policy: does NOT use conversation data for model training.
/// Isolated API key (`sk-sp-` prefix), base URL (`token-plan.cn-beijing.maas.aliyuncs.com`).
/// Verified 2026-09-15 from official docs.
pub fn bailian_token_plan_team_cn() -> ProviderCatalogEntry {
    entry(
        "bailian-token-plan-team-cn",
        ProviderConfig {
            api_key_env: Some("BAILIAN_TOKEN_PLAN_TEAM_API_KEY".to_string()),
            openai_chat: Some(
                EndpointConfig::native(
                    "https://token-plan.cn-beijing.maas.aliyuncs.com/compatible-mode/v1/chat/completions",
                )
                .with_compat(bailian_token_plan_chat_compat()),
            ),
            // Derived from Chat: not all Token Plan models support native Responses
            // (kimi-k2.x, MiniMax-M2.5, deepseek-v3.2 return InternalError.Algo).
            // Chat conversion path is well-tested (same as Coding Plan).
            openai_responses: Some(EndpointConfig::derived(Protocol::OpenaiChatCompletions)),
            anthropic: Some(EndpointConfig::native(
                "https://token-plan.cn-beijing.maas.aliyuncs.com/apps/anthropic/v1/messages",
            )),
            ..ProviderConfig::default()
        },
    )
    .with_templates(bailian_token_plan_team_templates())
}

pub fn bailian_payg_cn() -> ProviderCatalogEntry {
    entry(
        "bailian-payg-cn",
        chat_with_derived_compat(
            "DASHSCOPE_API_KEY",
            "https://dashscope.aliyuncs.com/compatible-mode/v1/chat/completions",
            bailian_payg_chat_compat(),
        ),
    )
    .with_templates(bailian_templates())
}

pub fn bailian_payg_us() -> ProviderCatalogEntry {
    entry(
        "bailian-payg-us",
        ProviderConfig {
            api_key_env: Some("DASHSCOPE_API_KEY".to_string()),
            openai_chat: Some(
                EndpointConfig::native(
                    "https://dashscope-us.aliyuncs.com/compatible-mode/v1/chat/completions",
                )
                .with_compat(bailian_payg_chat_compat()),
            ),
            openai_responses: Some(EndpointConfig::native(
                "https://dashscope-us.aliyuncs.com/compatible-mode/v1/responses",
            )),
            anthropic: Some(EndpointConfig::native(
                "https://dashscope-us.aliyuncs.com/apps/anthropic/v1/messages",
            )),
            ..ProviderConfig::default()
        },
    )
    .with_templates(bailian_templates())
}

/// 百炼 PAYG Chat 端点 compat（调研 provider-qwen.md）：
/// 无 developer role（仅 system/user/assistant/tool），其余字段用默认保守值。
fn bailian_payg_chat_compat() -> CompatConfig {
    CompatConfig {
        supports_developer_role: Some(false),
        ..CompatConfig::default()
    }
}

/// Token Plan Chat 端点 compat（2026-09-15 SDK 实验验证）。
/// - `enable_thinking` 格式（与 PAYG/Coding Plan 相同 compatible-mode 端点）
/// - 默认开启 thinking，返回 `reasoning_content` 字段
/// - **不要求** reasoning_content 回填（多轮 tool call 实验：with/without 均通过）
/// - `max_tokens` 字段
fn bailian_token_plan_chat_compat() -> CompatConfig {
    CompatConfig {
        supports_developer_role: Some(false),
        thinking_format: Some("qwen".to_string()),
        requires_reasoning_content_on_assistant_messages: Some(false),
        max_tokens_field: Some("max_tokens".to_string()),
        ..CompatConfig::default()
    }
}

/// MiMo Chat 端点 compat：独特的 thinking toggle 格式，不支持 reasoning_effort，
/// 使用 max_completion_tokens 字段，需要 reasoning_content 回传。
fn mimo_chat_compat() -> CompatConfig {
    CompatConfig {
        supports_developer_role: Some(true),
        supports_reasoning_effort: Some(false),
        thinking_format: Some("mimo-thinking-toggle".to_string()),
        requires_reasoning_content_on_assistant_messages: Some(true),
        max_tokens_field: Some("max_completion_tokens".to_string()),
        ..CompatConfig::default()
    }
}

/// MiMo Responses 端点 compat：原生 reasoning 对象，支持 reasoning_effort，
/// 使用 max_output_tokens 字段。
fn mimo_responses_compat() -> CompatConfig {
    CompatConfig {
        supports_developer_role: Some(true),
        supports_reasoning_effort: Some(true),
        thinking_format: Some("openai-responses-reasoning".to_string()),
        requires_reasoning_content_on_assistant_messages: Some(true),
        max_tokens_field: Some("max_output_tokens".to_string()),
        ..CompatConfig::default()
    }
}

/// MiMo Anthropic 端点 compat：标准 anthropic thinking，不支持 reasoning_effort，
/// 使用 max_tokens 字段。
fn mimo_anthropic_compat() -> CompatConfig {
    CompatConfig {
        supports_developer_role: Some(false),
        supports_reasoning_effort: Some(false),
        thinking_format: Some("anthropic-thinking".to_string()),
        requires_reasoning_content_on_assistant_messages: Some(true),
        max_tokens_field: Some("max_tokens".to_string()),
        ..CompatConfig::default()
    }
}

fn mimo_provider(api_key_env: &str, host: &str) -> ProviderConfig {
    ProviderConfig {
        api_key_env: Some(api_key_env.to_string()),
        openai_chat: Some(
            EndpointConfig::native(format!("{host}/v1/chat/completions"))
                .with_compat(mimo_chat_compat()),
        ),
        openai_responses: Some(
            EndpointConfig::native(format!("{host}/v1/responses"))
                .with_compat(mimo_responses_compat()),
        ),
        anthropic: Some(
            EndpointConfig::native(format!("{host}/anthropic/v1/messages"))
                .with_compat(mimo_anthropic_compat()),
        ),
        reasoning_levels: Some(mimo_reasoning_level_map()),
        ..ProviderConfig::default()
    }
}

pub fn mimo_payg() -> ProviderCatalogEntry {
    entry(
        "mimo-payg",
        mimo_provider("MIMO_API_KEY", "https://api.xiaomimimo.com"),
    )
    .with_templates(mimo_payg_templates())
}

pub fn mimo_token_plan_cn() -> ProviderCatalogEntry {
    entry(
        "mimo-token-plan-cn",
        mimo_provider(
            "MIMO_TOKEN_PLAN_API_KEY",
            "https://token-plan-cn.xiaomimimo.com",
        ),
    )
    .with_templates(mimo_templates())
}

pub fn mimo_token_plan_sgp() -> ProviderCatalogEntry {
    entry(
        "mimo-token-plan-sgp",
        mimo_provider(
            "MIMO_TOKEN_PLAN_SGP_API_KEY",
            "https://token-plan-sgp.xiaomimimo.com",
        ),
    )
    .with_templates(mimo_templates())
}

pub fn mimo_token_plan_ams() -> ProviderCatalogEntry {
    entry(
        "mimo-token-plan-ams",
        mimo_provider(
            "MIMO_TOKEN_PLAN_AMS_API_KEY",
            "https://token-plan-ams.xiaomimimo.com",
        ),
    )
    .with_templates(mimo_templates())
}

pub fn stepfun_payg() -> ProviderCatalogEntry {
    entry(
        "stepfun-payg",
        ProviderConfig {
            api_key_env: Some("STEP_API_KEY".to_string()),
            openai_chat: Some(
                EndpointConfig::native("https://api.stepfun.ai/v1/chat/completions")
                    .with_compat(stepfun_chat_compat()),
            ),
            openai_responses: Some(EndpointConfig::native(
                "https://api.stepfun.ai/v1/responses",
            )),
            anthropic: Some(EndpointConfig::native("https://api.stepfun.ai/v1/messages")),
            ..ProviderConfig::default()
        },
    )
    .with_templates(stepfun_templates())
}

pub fn stepfun_step_plan() -> ProviderCatalogEntry {
    entry(
        "stepfun-step-plan",
        ProviderConfig {
            api_key_env: Some("STEP_API_KEY".to_string()),
            openai_chat: Some(
                EndpointConfig::native("https://api.stepfun.ai/step_plan/v1/chat/completions")
                    .with_compat(stepfun_chat_compat()),
            ),
            openai_responses: Some(EndpointConfig::derived(Protocol::OpenaiChatCompletions)),
            anthropic: Some(EndpointConfig::native(
                "https://api.stepfun.ai/step_plan/v1/messages",
            )),
            ..ProviderConfig::default()
        },
    )
    .with_templates(stepfun_templates())
}

/// StepFun Chat 端点 compat（调研 provider-stepfun.md §Endpoint compat）：
/// - 无 developer role（仅 system/user/assistant/tool）
/// - 支持 reasoning_effort + reasoning_format
/// - 可选 deepseek-style 响应格式产出 reasoning_content 字段
fn stepfun_chat_compat() -> CompatConfig {
    CompatConfig {
        supports_developer_role: Some(false),
        supports_reasoning_effort: Some(true),
        thinking_format: Some("stepfun".to_string()),
        max_tokens_field: Some("max_tokens".to_string()),
        ..CompatConfig::default()
    }
}

/// Apply catalog model defaults when adding a provider.
/// If selected_models is provided, only those models are added; otherwise the first template.
pub fn apply_catalog_model_defaults(
    cfg: &mut Config,
    provider_id: &str,
    selected_models: Option<&[String]>,
) -> anyhow::Result<()> {
    let Some(entry) = built_in_providers()
        .into_iter()
        .find(|e| e.id == provider_id)
    else {
        // Custom provider — no catalog templates
        if let Some(selected_models) = selected_models
            && !selected_models.is_empty()
        {
            anyhow::bail!(
                "provider {provider_id:?} has no catalog model templates; model bindings must be added with `model provider add`"
            );
        }
        return Ok(());
    };
    let templates = &entry.model_templates;
    let provider_config = &entry.provider;

    let template_ids: Vec<&str> = templates.iter().map(|t| t.frontend_id).collect();
    if templates.is_empty() {
        if let Some(selected_models) = selected_models
            && !selected_models.is_empty()
        {
            anyhow::bail!(
                "provider {provider_id:?} has no catalog model templates; model bindings must be added with `model provider add`"
            );
        }
        return Ok(());
    }

    let models_to_add: Vec<&ModelTemplate> = if let Some(selected) = selected_models {
        let mut chosen = Vec::new();
        for model_id in selected {
            if let Some(t) = templates.iter().find(|t| t.frontend_id == *model_id) {
                chosen.push(t);
            } else {
                anyhow::bail!(
                    "unknown model template {model_id:?} for provider {provider_id:?}; available: {}",
                    template_ids.join(", ")
                );
            }
        }
        if chosen.is_empty() {
            return Ok(());
        }
        chosen
    } else {
        vec![&templates[0]]
    };

    for t in models_to_add {
        cfg.models
            .entry(t.frontend_id.to_string())
            .or_insert_with(|| {
                model_with_supported_protocols(
                    provider_id,
                    provider_config,
                    t.upstream_model,
                    ModelDefaults::from(t),
                )
            });
    }
    Ok(())
}

/// Same as [`apply_catalog_model_defaults`] but allows the config key name
/// (`binding_name`) to differ from the catalog lookup ID (`product_id`).
///
/// Used by the TUI connect flow when the user customises the provider name
/// on the naming screen (e.g. "deepseek-2" instead of the catalog ID "deepseek").
pub fn reselect_catalog_model_bindings(
    cfg: &mut Config,
    provider_id: &str,
    selected_models: &[String],
) -> anyhow::Result<Vec<String>> {
    let provider = cfg
        .providers
        .get(provider_id)
        .ok_or_else(|| anyhow::anyhow!("unknown provider {provider_id:?}"))?;
    let product_id = provider.product.as_str();
    let entry = built_in_providers()
        .into_iter()
        .find(|entry| entry.id == product_id)
        .ok_or_else(|| anyhow::anyhow!(
            "provider {provider_id:?} belongs to unknown or custom product {product_id:?}; only catalog-backed providers can reselect models"
        ))?;
    let selected = selected_models
        .iter()
        .map(String::as_str)
        .collect::<std::collections::BTreeSet<_>>();
    for model_id in selected_models {
        if !entry
            .model_templates
            .iter()
            .any(|t| t.frontend_id == model_id)
        {
            anyhow::bail!("unknown model template {model_id:?} for product {product_id:?}");
        }
    }

    let mut affected_models = Vec::new();
    for template in entry.model_templates {
        let is_selected = selected.contains(template.frontend_id);
        if is_selected {
            affected_models.push(template.frontend_id.to_string());
            let model = cfg
                .models
                .entry(template.frontend_id.to_string())
                .or_insert_with(|| {
                    model_with_supported_protocols(
                        provider_id,
                        &entry.provider,
                        template.upstream_model,
                        ModelDefaults::from(&template),
                    )
                });
            for protocol in Protocol::CLIENT_PROTOCOLS {
                if template.protocols.contains(&protocol)
                    && entry.provider.endpoint(protocol).is_some()
                {
                    let bindings = match protocol {
                        Protocol::OpenaiChatCompletions => &mut model.openai_chat_providers,
                        Protocol::OpenaiResponses => &mut model.openai_responses_providers,
                        Protocol::Anthropic => &mut model.anthropic_providers,
                    };
                    if !bindings.iter().any(|binding| {
                        binding.name == provider_id && binding.model == template.upstream_model
                    }) {
                        bindings.push(ProviderBinding {
                            name: provider_id.to_string(),
                            model: template.upstream_model.to_string(),
                        });
                    }
                }
            }
        } else if let Some(model) = cfg.models.get_mut(template.frontend_id) {
            let mut changed = false;
            for protocol in Protocol::CLIENT_PROTOCOLS {
                let bindings = match protocol {
                    Protocol::OpenaiChatCompletions => &mut model.openai_chat_providers,
                    Protocol::OpenaiResponses => &mut model.openai_responses_providers,
                    Protocol::Anthropic => &mut model.anthropic_providers,
                };
                let before = bindings.len();
                bindings.retain(|binding| {
                    binding.name != provider_id || binding.model != template.upstream_model
                });
                changed |= bindings.len() != before;
            }
            if changed {
                affected_models.push(template.frontend_id.to_string());
            }
        }
    }
    cfg.validate()?;
    Ok(affected_models)
}

pub fn apply_catalog_model_defaults_with_name(
    cfg: &mut Config,
    product_id: &str,
    binding_name: &str,
    selected_models: Option<&[String]>,
) -> anyhow::Result<()> {
    let Some(entry) = built_in_providers()
        .into_iter()
        .find(|e| e.id == product_id)
    else {
        if let Some(selected_models) = selected_models
            && !selected_models.is_empty()
        {
            anyhow::bail!(
                "provider {product_id:?} has no catalog model templates; model bindings must be added with `model provider add`"
            );
        }
        return Ok(());
    };

    if product_id == "openrouter" {
        if let Some(selected) = selected_models
            && !selected.is_empty()
        {
            let now = chrono::Utc::now().timestamp();
            let state_dir = crate::service::state_dir();
            let cache_opt = crate::connect::openrouter::read_cache_file(&state_dir, now);
            let cached_models = cache_opt.map(|c| c.models).unwrap_or_default();

            let mut raw_models = Vec::new();
            for id in selected {
                if let Some(m) = cached_models.iter().find(|m| &m.id == id) {
                    raw_models.push(m.clone());
                } else {
                    raw_models.push(crate::connect::openrouter::OpenRouterRawModel {
                        id: id.clone(),
                        name: Some(id.clone()),
                        context_length: Some(128_000),
                        top_provider: None,
                        architecture: None,
                        supported_parameters: vec!["tools".to_string()],
                        reasoning: None,
                    });
                }
            }
            crate::connect::openrouter::apply_dynamic_models_to_config(cfg, &raw_models)?;
        }
        return Ok(());
    }

    let templates = &entry.model_templates;
    let provider_config = &entry.provider;

    let template_ids: Vec<&str> = templates.iter().map(|t| t.frontend_id).collect();
    if templates.is_empty() {
        if let Some(selected_models) = selected_models
            && !selected_models.is_empty()
        {
            anyhow::bail!(
                "provider {product_id:?} has no catalog model templates; model bindings must be added with `model provider add`"
            );
        }
        return Ok(());
    }

    let models_to_add: Vec<&ModelTemplate> = if let Some(selected) = selected_models {
        let mut chosen = Vec::new();
        for model_id in selected {
            if let Some(t) = templates.iter().find(|t| t.frontend_id == *model_id) {
                chosen.push(t);
            } else {
                anyhow::bail!(
                    "unknown model template {model_id:?} for provider {product_id:?}; available: {}",
                    template_ids.join(", ")
                );
            }
        }
        if chosen.is_empty() {
            return Ok(());
        }
        chosen
    } else {
        vec![&templates[0]]
    };

    for t in models_to_add {
        let model = cfg
            .models
            .entry(t.frontend_id.to_string())
            .or_insert_with(|| {
                model_with_supported_protocols(
                    binding_name,
                    provider_config,
                    t.upstream_model,
                    ModelDefaults::from(t),
                )
            });
        for protocol in Protocol::CLIENT_PROTOCOLS {
            if !t.protocols.contains(&protocol) || provider_config.endpoint(protocol).is_none() {
                continue;
            }
            let bindings = match protocol {
                Protocol::OpenaiChatCompletions => &mut model.openai_chat_providers,
                Protocol::OpenaiResponses => &mut model.openai_responses_providers,
                Protocol::Anthropic => &mut model.anthropic_providers,
            };
            if !bindings.iter().any(|binding| binding.name == binding_name) {
                bindings.push(ProviderBinding {
                    name: binding_name.to_string(),
                    model: t.upstream_model.to_string(),
                });
            }
        }
    }
    Ok(())
}

/// Build a ModelConfig with bindings for the protocols supported by both the
/// model template and provider endpoints.
pub fn model_with_supported_protocols(
    provider_id: &str,
    provider_config: &ProviderConfig,
    upstream_model: &str,
    defaults: ModelDefaults<'_>,
) -> ModelConfig {
    let binding = || ProviderBinding {
        name: provider_id.to_string(),
        model: upstream_model.to_string(),
    };
    let supported_reasoning_levels = if provider_config
        .openai_chat
        .as_ref()
        .and_then(|endpoint| endpoint.compat.as_ref())
        .and_then(|compat| compat.thinking_format.as_deref())
        == Some("deepseek")
    {
        vec![
            "none".to_string(),
            "low".to_string(),
            "medium".to_string(),
            "high".to_string(),
            "xhigh".to_string(),
        ]
    } else {
        Vec::new()
    };
    // Build the unified reasoning_levels table (ADR-034).
    let reasoning_levels = if let Some(mappings) = defaults.reasoning_level_map {
        Some(
            mappings
                .iter()
                .map(|(level, api_value)| {
                    ReasoningLevelEntry::new((*level).to_string(), Some((*api_value).to_string()))
                })
                .collect(),
        )
    } else if !supported_reasoning_levels.is_empty() {
        // Auto-detected levels (e.g. deepseek thinking format) → identity mapping.
        Some(
            supported_reasoning_levels
                .iter()
                .filter(|level| *level != "none" && *level != "off")
                .map(|level| ReasoningLevelEntry::new(level.clone(), None))
                .collect(),
        )
    } else {
        None
    };
    ModelConfig {
        description: None,
        context_window: defaults.context_window,
        max_output_tokens: defaults.max_output_tokens,
        features: defaults
            .features
            .iter()
            .map(|feature| (*feature).to_string())
            .collect(),
        reasoning_levels,
        default_reasoning_level: defaults.default_reasoning_level.map(ToString::to_string),
        supports_thinking: None,
        force_think: defaults.force_think,
        force_no_think: None,
        default_think: None,
        enable_thinking: None,
        chat_reasoning_off_format: defaults.chat_reasoning_off_format.map(ToString::to_string),
        codex_service_tier: defaults.codex_service_tier,
        openai_chat_providers: if defaults
            .protocols
            .contains(&Protocol::OpenaiChatCompletions)
            && provider_config.openai_chat.is_some()
        {
            vec![binding()]
        } else {
            Vec::new()
        },
        openai_responses_providers: if defaults.protocols.contains(&Protocol::OpenaiResponses)
            && provider_config.openai_responses.is_some()
        {
            vec![binding()]
        } else {
            Vec::new()
        },
        anthropic_providers: if defaults.protocols.contains(&Protocol::Anthropic)
            && provider_config.anthropic.is_some()
        {
            vec![binding()]
        } else {
            Vec::new()
        },
        reasoning_level_map: None,
        supported_reasoning_levels: Vec::new(),
        cooldown: None,
        reasoning_routes: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_models_expose_reasoning_levels_only_for_deepseek_thinking_format() {
        let deepseek = deepseek();
        let deepseek_model = model_with_supported_protocols(
            "deepseek",
            &deepseek.provider,
            "deepseek-v4-flash",
            ModelDefaults {
                context_window: 1,
                max_output_tokens: 1,
                features: &[],
                protocols: ALL_CLIENT_PROTOCOLS,
                codex_service_tier: None,
                default_reasoning_level: None,
                reasoning_level_map: None,
                force_think: None,
                chat_reasoning_off_format: None,
            },
        );
        // Deepseek thinking format auto-detects levels (excluding "none"/"off")
        // into the unified reasoning_levels table (ADR-034).
        assert!(
            deepseek_model
                .reasoning_levels
                .as_ref()
                .is_some_and(|table| !table.is_empty())
        );

        let stepfun = stepfun_step_plan();
        let stepfun_model = model_with_supported_protocols(
            "stepfun-step-plan",
            &stepfun.provider,
            "step-3.7-flash",
            ModelDefaults {
                context_window: 1,
                max_output_tokens: 1,
                features: &[],
                protocols: ALL_CLIENT_PROTOCOLS,
                codex_service_tier: None,
                default_reasoning_level: None,
                reasoning_level_map: None,
                force_think: None,
                chat_reasoning_off_format: None,
            },
        );
        assert!(stepfun_model.reasoning_levels.is_none());
    }

    #[test]
    fn reselect_updates_only_the_selected_product_bindings() {
        let entry = deepseek();
        let template = entry
            .model_templates
            .iter()
            .find(|template| template.frontend_id == "deepseek-v4-flash-lp")
            .expect("deepseek flash template");
        let mut cfg = crate::config::default_deepseek_config();
        cfg.providers
            .insert("deepseek-work".to_string(), entry.provider);
        let model = cfg
            .models
            .get_mut(template.frontend_id)
            .expect("catalog model");
        model.description = Some("user metadata".to_string());
        model.openai_chat_providers.push(ProviderBinding {
            name: "other-product".to_string(),
            model: "other-model".to_string(),
        });
        cfg.providers.insert(
            "other-product".to_string(),
            ProviderConfig {
                openai_chat: Some(EndpointConfig::native("https://example.test/chat")),
                ..ProviderConfig::default()
            },
        );

        reselect_catalog_model_bindings(&mut cfg, "deepseek-work", &[])
            .expect("empty selection removes this provider's catalog bindings");

        let flash = cfg
            .models
            .get(template.frontend_id)
            .expect("flash model remains");
        assert_eq!(flash.description.as_deref(), Some("user metadata"));
        assert!(
            flash
                .openai_chat_providers
                .iter()
                .any(|binding| binding.name == "other-product")
        );
        assert!(
            !flash
                .openai_chat_providers
                .iter()
                .any(|binding| binding.name == "deepseek-work")
        );

        reselect_catalog_model_bindings(
            &mut cfg,
            "deepseek-work",
            &["deepseek-v4-pro-lp".to_string()],
        )
        .expect("reselect product");

        let flash = cfg
            .models
            .get(template.frontend_id)
            .expect("flash model remains");
        assert_eq!(flash.description.as_deref(), Some("user metadata"));
        assert!(
            flash
                .openai_chat_providers
                .iter()
                .any(|binding| binding.name == "other-product")
        );
        assert!(
            !flash
                .openai_chat_providers
                .iter()
                .any(|binding| binding.name == "deepseek-work")
        );

        let pro = cfg.models.get("deepseek-v4-pro-lp").expect("pro added");
        assert!(
            pro.openai_chat_providers.iter().any(
                |binding| binding.name == "deepseek-work" && binding.model == "deepseek-v4-pro"
            )
        );
    }

    #[test]
    fn reselect_rejects_custom_or_unknown_product() {
        let mut cfg = crate::config::default_deepseek_config();
        cfg.providers
            .insert("custom".to_string(), ProviderConfig::default());
        let err = reselect_catalog_model_bindings(
            &mut cfg,
            "custom",
            &["deepseek-v4-flash-lp".to_string()],
        )
        .expect_err("custom providers have no catalog");
        assert!(err.to_string().contains("unknown or custom product"));
    }

    #[test]
    fn openai_sub_catalog_tracks_current_visible_models_and_excludes_hidden_or_rejected_models() {
        let templates = openai_sub().model_templates;
        let models: Vec<_> = templates
            .iter()
            .map(|template| (template.frontend_id, template.upstream_model))
            .collect();

        assert_eq!(models.len(), 8);
        assert_eq!(
            models.first(),
            Some(&("gpt-5.6-sol-sub-lp", "gpt-5.6-sol")),
            "the internal no-selection fallback must retain its existing first template"
        );
        assert!(models.contains(&("gpt-6-astra-sub-lp", "gpt-6-astra")));
        assert!(models.contains(&("gpt-5.6-sol-sub-lp", "gpt-5.6-sol")));
        assert!(models.contains(&("gpt-5.6-terra-sub-lp", "gpt-5.6-terra")));
        assert!(models.contains(&("gpt-5.6-luna-sub-lp", "gpt-5.6-luna")));
        assert!(models.contains(&("gpt-5.5-sub-lp", "gpt-5.5")));
        assert!(models.contains(&("gpt-5.4-sub-lp", "gpt-5.4")));
        assert!(models.contains(&("gpt-5.4-mini-sub-lp", "gpt-5.4-mini")));
        assert!(models.contains(&("codex-auto-review-sub-lp", "codex-auto-review")));
        assert!(models.iter().all(|(_, upstream_model)| !matches!(
            *upstream_model,
            "gpt-5.2"
                | "gpt-5.3-codex-spark"
                | "gpt-image-1"
                | "gpt-image-1.5"
                | "gpt-image-2"
                | "gpt-reserve"
                | "gpt-5.6"
        )));

        let astra = templates
            .iter()
            .find(|template| template.frontend_id == "gpt-6-astra-sub-lp")
            .expect("gpt-6-astra template");
        assert_eq!(astra.upstream_model, "gpt-6-astra");
        assert_eq!(astra.context_window, 1_050_000);
        assert_eq!(astra.max_output_tokens, 128_000);
        assert!(
            astra.codex_service_tier.is_none(),
            "Astra must not configure priority tier because upstream reports service_tier=default"
        );

        let sol = templates
            .iter()
            .find(|template| template.frontend_id == "gpt-5.6-sol-sub-lp")
            .expect("gpt-5.6-sol template");
        assert_eq!(sol.upstream_model, "gpt-5.6-sol");
        assert_eq!(sol.context_window, 1_050_000);
        assert_eq!(sol.max_output_tokens, 128_000);

        let terra = templates
            .iter()
            .find(|template| template.frontend_id == "gpt-5.6-terra-sub-lp")
            .expect("gpt-5.6-terra template");
        assert_eq!(terra.upstream_model, "gpt-5.6-terra");
        assert_eq!(terra.context_window, 1_050_000);
        assert_eq!(terra.max_output_tokens, 128_000);

        let luna = templates
            .iter()
            .find(|template| template.frontend_id == "gpt-5.6-luna-sub-lp")
            .expect("gpt-5.6-luna template");
        assert_eq!(luna.upstream_model, "gpt-5.6-luna");
        assert_eq!(luna.context_window, 1_050_000);
        assert_eq!(luna.max_output_tokens, 128_000);

        let auto_review = openai_sub()
            .model_templates
            .into_iter()
            .find(|template| template.upstream_model == "codex-auto-review")
            .expect("auto-review catalog template");
        assert_eq!(auto_review.context_window, 272_000);
        assert_eq!(auto_review.default_reasoning_level, Some("medium"));
        assert_eq!(
            auto_review.reasoning_level_map,
            Some(&[
                ("low", "low"),
                ("medium", "medium"),
                ("high", "high"),
                ("xhigh", "xhigh"),
                ("max", "max"),
            ] as _)
        );
    }

    #[test]
    fn openai_payg_and_sub_expose_reasoning_levels() {
        let sub_entry = openai_sub();
        let mut sub_cfg: Config =
            toml::from_str("[server]\nlisten = \"127.0.0.1:0\"\n").expect("minimal config");
        sub_cfg
            .providers
            .insert(sub_entry.id.to_string(), sub_entry.provider.clone());
        let all_sub_models: Vec<String> = sub_entry
            .model_templates
            .iter()
            .map(|t| t.frontend_id.to_string())
            .collect();
        apply_catalog_model_defaults(&mut sub_cfg, sub_entry.id, Some(&all_sub_models))
            .expect("apply sub defaults");

        let level_names = |levels: &[crate::config::ReasoningLevelEntry]| -> Vec<String> {
            levels.iter().map(|e| e.level().to_string()).collect()
        };

        let astra = sub_cfg
            .models
            .get("gpt-6-astra-sub-lp")
            .expect("astra model in sub");
        assert_eq!(astra.default_reasoning_level.as_deref(), Some("low"));
        let astra_levels = astra.reasoning_levels.as_ref().expect("astra levels");
        assert_eq!(
            level_names(astra_levels),
            vec!["low", "medium", "high", "xhigh", "max", "ultra"]
        );

        let sol = sub_cfg
            .models
            .get("gpt-5.6-sol-sub-lp")
            .expect("sol model in sub");
        assert_eq!(sol.default_reasoning_level.as_deref(), Some("low"));
        assert_eq!(
            level_names(sol.reasoning_levels.as_ref().expect("sol levels")),
            vec!["low", "medium", "high", "xhigh", "max", "ultra"]
        );

        let terra = sub_cfg
            .models
            .get("gpt-5.6-terra-sub-lp")
            .expect("terra model in sub");
        assert_eq!(terra.default_reasoning_level.as_deref(), Some("medium"));
        assert_eq!(
            level_names(terra.reasoning_levels.as_ref().expect("terra levels")),
            vec!["low", "medium", "high", "xhigh", "max", "ultra"]
        );

        let luna = sub_cfg
            .models
            .get("gpt-5.6-luna-sub-lp")
            .expect("luna model in sub");
        assert_eq!(luna.default_reasoning_level.as_deref(), Some("medium"));
        assert_eq!(
            level_names(luna.reasoning_levels.as_ref().expect("luna levels")),
            vec!["low", "medium", "high", "xhigh", "max"]
        );

        let gpt55 = sub_cfg
            .models
            .get("gpt-5.5-sub-lp")
            .expect("gpt-5.5 in sub");
        assert_eq!(gpt55.default_reasoning_level.as_deref(), Some("medium"));
        assert_eq!(
            level_names(gpt55.reasoning_levels.as_ref().expect("gpt-5.5 levels")),
            vec!["low", "medium", "high", "xhigh"]
        );
        assert_eq!(
            gpt55
                .codex_service_tier
                .as_ref()
                .and_then(|t| t.default_tier.as_deref()),
            Some("priority"),
            "gpt-5.5 must retain priority tier alongside reasoning levels"
        );

        let payg_entry = openai_payg();
        let mut payg_cfg: Config =
            toml::from_str("[server]\nlisten = \"127.0.0.1:0\"\n").expect("minimal config");
        payg_cfg
            .providers
            .insert(payg_entry.id.to_string(), payg_entry.provider.clone());
        apply_catalog_model_defaults(&mut payg_cfg, payg_entry.id, None)
            .expect("apply payg defaults");

        let payg_55 = payg_cfg
            .models
            .get("gpt-5.5-lp")
            .expect("gpt-5.5 model in payg");
        assert_eq!(payg_55.default_reasoning_level.as_deref(), Some("medium"));
        assert_eq!(
            level_names(payg_55.reasoning_levels.as_ref().expect("payg 5.5 levels")),
            vec!["low", "medium", "high", "xhigh"]
        );
    }

    #[test]
    fn named_catalog_provider_is_added_to_existing_model_bindings() {
        let entry = super::deepseek();
        let template = &entry.model_templates[0];
        let selected = vec![template.frontend_id.to_string()];
        let mut cfg = crate::config::default_deepseek_config();
        cfg.providers
            .insert(entry.id.to_string(), entry.provider.clone());
        apply_catalog_model_defaults(&mut cfg, entry.id, Some(&selected))
            .expect("add original provider binding");

        apply_catalog_model_defaults_with_name(
            &mut cfg,
            entry.id,
            "deepseek-named",
            Some(&selected),
        )
        .expect("add named provider binding");

        let model = cfg.models.get(template.frontend_id).expect("model");
        for bindings in [
            &model.openai_chat_providers,
            &model.openai_responses_providers,
            &model.anthropic_providers,
        ] {
            assert!(
                bindings
                    .iter()
                    .any(|binding| binding.name == "deepseek-named")
            );
        }
    }

    #[test]
    fn existing_binding_with_same_name_different_model_is_not_duplicated() {
        let mut cfg = crate::config::default_deepseek_config();
        let model = cfg.models.get_mut("deepseek-v4-flash-lp").unwrap();
        for binding in model.openai_chat_providers.iter_mut() {
            if binding.name == "deepseek" {
                binding.model = "deepseek-v3-old".to_string();
            }
        }
        for binding in model.openai_responses_providers.iter_mut() {
            if binding.name == "deepseek" {
                binding.model = "deepseek-v3-old".to_string();
            }
        }
        for binding in model.anthropic_providers.iter_mut() {
            if binding.name == "deepseek" {
                binding.model = "deepseek-v3-old".to_string();
            }
        }

        let selected = vec!["deepseek-v4-flash-lp".to_string()];
        apply_catalog_model_defaults_with_name(&mut cfg, "deepseek", "deepseek", Some(&selected))
            .expect("apply defaults");

        let model = cfg.models.get("deepseek-v4-flash-lp").unwrap();
        assert_eq!(
            model
                .openai_chat_providers
                .iter()
                .filter(|b| b.name == "deepseek")
                .count(),
            1,
            "must not create duplicate provider-name binding"
        );
        cfg.validate().expect("config must validate after apply");
    }

    #[test]
    fn built_in_catalog_contains_core_provider_products() {
        let providers = built_in_providers();
        let ids: Vec<_> = providers.iter().map(|entry| entry.id).collect();
        for expected in [
            "deepseek",
            "openai-payg",
            "openai-sub",
            "openrouter",
            "anthropic",
            "ollama",
            "kimi-platform-global",
            "kimi-platform-cn",
            "kimi-sub",
            "zhipu-payg-global",
            "zhipu-payg-cn",
            "zhipu-coding-cn",
            "bailian-coding-plan-cn",
            "bailian-payg-cn",
            "bailian-payg-us",
            "mimo-payg",
            "mimo-token-plan-cn",
            "mimo-token-plan-sgp",
            "mimo-token-plan-ams",
            "stepfun-payg",
            "stepfun-step-plan",
        ] {
            assert!(ids.contains(&expected), "missing catalog entry {expected}");
        }
    }

    #[test]
    fn built_in_catalog_entries_validate_as_providers() {
        for catalog_entry in built_in_providers() {
            let cfg = crate::config::Config {
                server: Some(crate::config::ServerConfig {
                    listen: "127.0.0.1:8989".to_string(),
                    usage: crate::config::UsageConfig::default(),
                    max_sse_buffer_bytes: crate::config::default_max_sse_buffer_bytes(),
                    max_output_items: crate::config::default_max_output_items(),
                    proxy: None,
                    require_auth: None,
                    auth_token: None,
                    auth_token_env: None,
                    management_port: None,
                }),
                remote: None,
                fallback: crate::config::FallbackConfig::default(),
                protection: crate::config::ProtectionConfig::default(),
                diagnostics: Default::default(),
                status: crate::config::StatusConfig::default(),
                codex: crate::config::CodexConfig::default(),
                launch: crate::config::LaunchConfig::default(),
                providers: [(catalog_entry.id.to_string(), catalog_entry.provider.clone())]
                    .into_iter()
                    .collect(),
                models: std::collections::BTreeMap::new(),
            };
            cfg.validate().unwrap_or_else(|err| {
                panic!(
                    "built-in provider {} must validate: {err}",
                    catalog_entry.id
                )
            });
        }
    }

    #[test]
    fn catalog_endpoints_write_complete_urls_without_base_url() {
        for catalog_entry in built_in_providers() {
            let text = toml::to_string_pretty(&catalog_entry.provider).expect("serialize");
            assert!(
                !text.contains("base_url"),
                "{} must not use base_url",
                catalog_entry.id
            );
            for (protocol, endpoint) in catalog_entry.provider.endpoints() {
                if let Some(url) = &endpoint.url {
                    assert!(
                        url.starts_with("http://") || url.starts_with("https://"),
                        "{} {:?} endpoint url must be complete: {url}",
                        catalog_entry.id,
                        protocol
                    );
                } else {
                    let source = endpoint.derive_from.as_deref().expect("derive_from");
                    assert!(
                        crate::config::Protocol::from_field_name(source).is_some(),
                        "{} {:?} derive_from must name a protocol field",
                        catalog_entry.id,
                        protocol
                    );
                }
            }
        }
    }

    #[test]
    fn deepseek_matches_design_init_shape() {
        let provider = deepseek().provider;
        let chat = provider.openai_chat.as_ref().expect("chat");
        assert_eq!(
            chat.url.as_deref(),
            Some("https://api.deepseek.com/chat/completions")
        );
        let compat = chat.compat.as_ref().expect("chat compat");
        assert_eq!(compat.supports_developer_role, Some(false));
        assert_eq!(compat.supports_reasoning_effort, Some(true));
        assert_eq!(compat.thinking_format.as_deref(), Some("deepseek"));
        assert_eq!(
            compat.requires_reasoning_content_on_assistant_messages,
            Some(true)
        );
        assert_eq!(compat.max_tokens_field.as_deref(), Some("max_tokens"));

        let anthropic = provider.anthropic.as_ref().expect("anthropic");
        assert_eq!(
            anthropic.url.as_deref(),
            Some("https://api.deepseek.com/anthropic/v1/messages")
        );

        let responses = provider.openai_responses.as_ref().expect("responses");
        assert_eq!(
            responses.url.as_deref(),
            Some("https://api.deepseek.com/v1/responses")
        );
        assert!(responses.derive_from.is_none());
        assert!(responses.compat.is_none());
    }

    #[test]
    fn oauth_catalog_entries_use_unified_store_auth_and_source() {
        let openai = openai_sub().provider;
        assert_eq!(
            openai.auth_config("openai-sub").expect("auth"),
            AuthConfig::OpenaiOauth {
                account: Some("openai-subscription".to_string())
            }
        );
        assert_eq!(
            openai
                .openai_responses
                .as_ref()
                .and_then(|e| e.url.as_deref()),
            Some("https://chatgpt.com/backend-api/codex/responses")
        );
        assert_eq!(
            openai
                .openai_chat
                .as_ref()
                .and_then(|e| e.derive_from.as_deref()),
            Some("openai_responses")
        );
    }

    #[test]
    fn anthropic_provider_is_native_messages_with_derived_openai() {
        let provider = anthropic().provider;
        assert_eq!(
            provider.anthropic.as_ref().and_then(|e| e.url.as_deref()),
            Some("https://api.anthropic.com/v1/messages")
        );
        assert_eq!(
            provider
                .openai_responses
                .as_ref()
                .and_then(|e| e.derive_from.as_deref()),
            Some("anthropic")
        );
        assert_eq!(
            provider
                .openai_chat
                .as_ref()
                .and_then(|e| e.derive_from.as_deref()),
            Some("anthropic")
        );
    }

    #[test]
    fn kimi_sub_is_keyed_with_chat_and_anthropic_native() {
        let entry = kimi_sub();
        let provider = entry.provider;
        assert_eq!(provider.api_key_env.as_deref(), Some("KIMI_CODE_API_KEY"));
        assert_eq!(
            provider.openai_chat.as_ref().and_then(|e| e.url.as_deref()),
            Some("https://api.kimi.com/coding/v1/chat/completions")
        );
        // Responses is derived from chat; no native Responses endpoint
        assert_eq!(
            provider
                .openai_responses
                .as_ref()
                .and_then(|e| e.derive_from.as_deref()),
            Some("openai_chat")
        );
        assert_eq!(
            provider.anthropic.as_ref().and_then(|e| e.url.as_deref()),
            Some("https://api.kimi.com/coding/v1/messages")
        );

        let kimi_coding = entry
            .model_templates
            .iter()
            .find(|t| t.frontend_id == "kimi-for-coding-kimi-sub-lp")
            .expect("kimi-for-coding template should exist");
        assert_eq!(kimi_coding.context_window, 1_048_576);
        assert_eq!(kimi_coding.max_output_tokens, 65_536);
    }

    #[test]
    fn ollama_is_no_key_chat_native_provider() {
        let provider = ollama().provider;
        assert_eq!(provider.api_key_env, None);
        assert_eq!(
            provider.openai_chat.as_ref().and_then(|e| e.url.as_deref()),
            Some("http://127.0.0.1:11434/v1/chat/completions")
        );
        assert_eq!(
            provider
                .openai_responses
                .as_ref()
                .and_then(|e| e.url.as_deref()),
            Some("http://127.0.0.1:11434/v1/responses")
        );
        assert_eq!(
            provider.anthropic.as_ref().and_then(|e| e.url.as_deref()),
            Some("http://127.0.0.1:11434/v1/messages")
        );
    }

    #[test]
    fn mimo_catalog_matches_product_model_contracts() {
        let payg = mimo_payg();
        let token_plan = mimo_token_plan_cn();

        let payg_models = payg
            .model_templates
            .iter()
            .map(|template| template.upstream_model)
            .collect::<Vec<_>>();
        assert_eq!(
            payg_models,
            vec![
                "mimo-v2.6-pro",
                "mimo-v2.6-flash",
                "mimo-v2.5-pro",
                "mimo-v2.5",
                "mimo-v2.6-pro-ultraspeed",
            ]
        );
        // UltraSpeed is a PAYG-only entitlement: on Token Plan CN it is rejected on
        // all three egresses with the *entitlement* branch (`Not supported model …`),
        // so it must never reach the shared Token Plan template list.
        assert!(
            token_plan
                .model_templates
                .iter()
                .all(|template| !template.upstream_model.contains("ultraspeed")),
            "Token Plan templates must not offer UltraSpeed"
        );
        // The stale V2.5 UltraSpeed ID is not registered upstream any more (it takes
        // the `Unsupported model …` branch, same as a fabricated ID).
        assert!(
            payg_models
                .iter()
                .all(|model| *model != "mimo-v2.5-pro-ultraspeed")
                && token_plan
                    .model_templates
                    .iter()
                    .all(|template| template.upstream_model != "mimo-v2.5-pro-ultraspeed"),
            "retired UltraSpeed ID must not be catalogued anywhere"
        );

        let v25 = payg
            .model_templates
            .iter()
            .find(|template| template.upstream_model == "mimo-v2.5")
            .expect("mimo-v2.5 template");
        assert_eq!(v25.max_output_tokens, 32_768);
        let v25_model = model_with_supported_protocols(
            payg.id,
            &payg.provider,
            v25.upstream_model,
            ModelDefaults::from(v25),
        );
        for feature in ["image_input", "audio_input", "video_input"] {
            assert!(
                v25_model
                    .features
                    .iter()
                    .any(|declared| declared == feature),
                "mimo-v2.5 missing {feature}"
            );
        }

        let ultraspeed = payg
            .model_templates
            .iter()
            .find(|template| template.upstream_model == "mimo-v2.6-pro-ultraspeed")
            .expect("UltraSpeed template");
        let ultraspeed_model = model_with_supported_protocols(
            payg.id,
            &payg.provider,
            ultraspeed.upstream_model,
            ModelDefaults::from(ultraspeed),
        );
        assert_eq!(ultraspeed_model.openai_chat_providers.len(), 1);
        assert!(ultraspeed_model.openai_responses_providers.is_empty());
        assert!(ultraspeed_model.anthropic_providers.is_empty());
        // Nothing about this model was measurable inside this refresh's evidence
        // boundary (400 on Token Plan CN, no PAYG key), so it declares no media
        // capability rather than inheriting its sibling's.
        assert!(
            ultraspeed.features.is_empty(),
            "unmeasured capabilities must not be declared: {:?}",
            ultraspeed.features
        );

        // No MiMo model is documented or measured to accept document/PDF input, and the
        // Anthropic egress drops `document` parts with HTTP 200 (provider-mimo.md §5.6),
        // so declaring the feature anywhere would turn a local rejection into a silent no-op.
        for entry in [&payg, &token_plan] {
            for template in &entry.model_templates {
                assert!(
                    !template.features.contains(&"document_input"),
                    "{} must not declare document_input",
                    template.upstream_model
                );
            }
        }

        let mut cfg = crate::config::default_deepseek_config();
        cfg.providers
            .insert(payg.id.to_string(), payg.provider.clone());
        apply_catalog_model_defaults(&mut cfg, payg.id, None).expect("apply PAYG default");
        assert!(cfg.models.contains_key("mimo-v2.6-pro-lp"));
        assert!(!cfg.models.contains_key("mimo-v2.5-pro-lp"));
        assert!(!cfg.models.contains_key("mimo-v2.6-pro-ultraspeed-lp"));

        apply_catalog_model_defaults(
            &mut cfg,
            payg.id,
            Some(&["mimo-v2.6-pro-ultraspeed-lp".to_string()]),
        )
        .expect("apply UltraSpeed opt-in");
        let configured_ultraspeed = cfg
            .models
            .get("mimo-v2.6-pro-ultraspeed-lp")
            .expect("configured UltraSpeed model");
        assert_eq!(configured_ultraspeed.openai_chat_providers.len(), 1);
        assert!(configured_ultraspeed.openai_responses_providers.is_empty());
        assert!(configured_ultraspeed.anthropic_providers.is_empty());
    }

    /// The retiring V2.5 generation must never be what `connect`
    /// installs on its own. `apply_catalog_model_defaults(…, None)` takes
    /// `templates[0]`, so this pins the position, not just the presence.
    #[test]
    fn mimo_default_template_is_a_current_generation_model() {
        for catalog_entry in [
            mimo_payg(),
            mimo_token_plan_cn(),
            mimo_token_plan_sgp(),
            mimo_token_plan_ams(),
        ] {
            let first = catalog_entry
                .model_templates
                .first()
                .unwrap_or_else(|| panic!("{} has no model templates", catalog_entry.id));
            assert_eq!(
                first.upstream_model, "mimo-v2.6-pro",
                "{} default template",
                catalog_entry.id
            );
            assert_eq!(
                first.frontend_id, "mimo-v2.6-pro-lp",
                "{} default frontend id",
                catalog_entry.id
            );
            assert!(
                catalog_entry
                    .model_templates
                    .iter()
                    .any(|t| t.upstream_model == "mimo-v2.6-flash"),
                "{} must offer the V2.6 flash tier",
                catalog_entry.id
            );
            assert!(
                !first.features.is_empty(),
                "{} default declares media input",
                catalog_entry.id
            );
        }
    }

    #[test]
    fn mimo_entries_have_native_three_protocols_and_thinking_compat() {
        for catalog_entry in [
            mimo_payg(),
            mimo_token_plan_cn(),
            mimo_token_plan_sgp(),
            mimo_token_plan_ams(),
        ] {
            let provider = &catalog_entry.provider;
            let chat = provider.openai_chat.as_ref().expect("chat");
            assert!(chat.url.is_some(), "{} chat url", catalog_entry.id);
            assert_eq!(
                chat.compat
                    .as_ref()
                    .and_then(|c| c.thinking_format.as_deref()),
                Some("mimo-thinking-toggle"),
                "{} chat thinking format",
                catalog_entry.id
            );
            assert!(
                provider
                    .openai_responses
                    .as_ref()
                    .is_some_and(|e| e.url.is_some())
            );
            assert!(provider.anthropic.as_ref().is_some_and(|e| e.url.is_some()));
            assert!(provider.reasoning_levels.is_some());
        }
        assert_eq!(
            mimo_token_plan_sgp()
                .provider
                .openai_chat
                .as_ref()
                .and_then(|e| e.url.as_deref()),
            Some("https://token-plan-sgp.xiaomimimo.com/v1/chat/completions")
        );
    }

    /// provider-mimo.md §5.5/§8: MiMo reasoning strength is **binary**, so
    /// `mimo_reasoning_level_map()` is input-vocabulary normalisation, not a tier list. It
    /// must equal the official Responses `reasoning.effort` enum minus `none` (the off
    /// intent never passes the level gate), and every alias must fold onto a real upstream
    /// value. A label the official enum accepts but the table lacks makes the proxy 400 a
    /// request the upstream would have served identically to `high`.
    #[test]
    fn mimo_reasoning_level_map_is_official_vocabulary_with_no_extra_tiers() {
        let table = mimo_reasoning_level_map();
        let folded = |level: &str| -> &str {
            table
                .iter()
                .find(|entry| entry.level() == level)
                .and_then(ReasoningLevelEntry::api_value)
                .unwrap_or_else(|| panic!("reasoning level {level:?} is not in the MiMo table"))
        };
        assert_eq!(
            table
                .iter()
                .map(ReasoningLevelEntry::level)
                .collect::<Vec<_>>(),
            vec!["minimal", "low", "medium", "high", "xhigh", "max", "ultra"]
        );
        for level in ["minimal", "low", "medium", "high", "xhigh", "max", "ultra"] {
            assert!(
                matches!(folded(level), "low" | "medium" | "high"),
                "{level} must fold onto a tier MiMo really has, got {:?}",
                folded(level)
            );
        }
        assert_eq!(folded("minimal"), "low");
        for real in ["low", "medium", "high"] {
            assert_eq!(folded(real), real);
        }
        for alias in ["xhigh", "max", "ultra"] {
            assert_eq!(
                folded(alias),
                "high",
                "{alias} must not invent a fourth tier"
            );
        }

        // MiMo templates declare no table of their own, so `Config::effective_reasoning_levels`
        // falls back to the provider map — which is also what `client_reasoning_levels()`
        // advertises. The two effects of an edit here (gate + advertisement) are inseparable.
        for entry in [mimo_payg(), mimo_token_plan_cn()] {
            for template in &entry.model_templates {
                let model = model_with_supported_protocols(
                    entry.id,
                    &entry.provider,
                    template.upstream_model,
                    ModelDefaults::from(template),
                );
                assert!(
                    model.reasoning_levels.is_none(),
                    "{} declares its own table; the provider map stops being the gate",
                    template.upstream_model
                );
            }
        }
    }

    #[test]
    fn stepfun_defaults_use_ai_hosts() {
        let payg = stepfun_payg().provider;
        assert_eq!(
            payg.openai_chat.as_ref().and_then(|e| e.url.as_deref()),
            Some("https://api.stepfun.ai/v1/chat/completions")
        );
        assert_eq!(
            payg.openai_responses
                .as_ref()
                .and_then(|e| e.url.as_deref()),
            Some("https://api.stepfun.ai/v1/responses")
        );
        assert_eq!(
            payg.anthropic.as_ref().and_then(|e| e.url.as_deref()),
            Some("https://api.stepfun.ai/v1/messages")
        );
        let plan = stepfun_step_plan().provider;
        assert_eq!(
            plan.anthropic.as_ref().and_then(|e| e.url.as_deref()),
            Some("https://api.stepfun.ai/step_plan/v1/messages")
        );
    }

    #[test]
    fn catalog_entries_have_product_field_set() {
        // Verify that all catalog entries have their product field set to the entry id
        for catalog_entry in built_in_providers() {
            assert_eq!(
                catalog_entry.provider.product, catalog_entry.id,
                "catalog entry {} should have product = id",
                catalog_entry.id
            );
            assert!(
                !catalog_entry.provider.is_custom_product(),
                "catalog entry {} should not be custom product",
                catalog_entry.id
            );
        }
    }

    fn find_template<'a>(entry: &'a ProviderCatalogEntry, id: &str) -> &'a ModelTemplate {
        entry
            .model_templates
            .iter()
            .find(|template| template.frontend_id == id)
            .expect("template")
    }

    #[test]
    fn bailian_coding_plan_templates_apply_entry_scoped_thinking_rules() {
        let coding = bailian_coding_plan_cn();
        let payg_cn = bailian_payg_cn();
        let payg_us = bailian_payg_us();

        // §5.5: Coding Plan only.
        let minimax = find_template(&coding, "minimax-m2.5-bailian-lp");
        assert_eq!(minimax.force_think, Some(true));
        assert_eq!(minimax.chat_reasoning_off_format, None);
        let glm47 = find_template(&coding, "glm-4.7-bailian-lp");
        assert_eq!(glm47.chat_reasoning_off_format, Some("enable-thinking"));
        assert_eq!(glm47.force_think, None);

        // PAYG entries must not inherit entry-scoped declarations.
        for entry in [&payg_cn, &payg_us] {
            assert_eq!(
                find_template(entry, "minimax-m2.5-bailian-lp").force_think,
                None
            );
            assert_eq!(
                find_template(entry, "glm-4.7-bailian-lp").chat_reasoning_off_format,
                None
            );
        }

        // Other Coding Plan models keep the shared defaults.
        let qwen = find_template(&coding, "qwen3.7-plus-bailian-lp");
        assert_eq!(qwen.force_think, None);
        assert_eq!(qwen.chat_reasoning_off_format, None);
        assert_eq!(qwen.default_reasoning_level, Some("medium"));
        assert_eq!(qwen.reasoning_level_map, Some(QWEN_THINKING_LEVELS));
    }

    #[test]
    fn bailian_qwen_3_7_plus_exposes_reasoning_levels() {
        let coding = bailian_coding_plan_cn();
        let qwen = find_template(&coding, "qwen3.7-plus-bailian-lp");
        assert_eq!(qwen.default_reasoning_level, Some("medium"));
        assert_eq!(qwen.reasoning_level_map, Some(QWEN_THINKING_LEVELS));

        let mut cfg: Config =
            toml::from_str("[server]\nlisten = \"127.0.0.1:0\"\n").expect("minimal config");
        cfg.providers.insert(coding.id.to_string(), coding.provider);
        apply_catalog_model_defaults(
            &mut cfg,
            "bailian-coding-plan-cn",
            Some(&["qwen3.7-plus-bailian-lp".to_string()]),
        )
        .expect("apply coding plan defaults");

        let model = &cfg.models["qwen3.7-plus-bailian-lp"];
        assert_eq!(model.default_reasoning_level.as_deref(), Some("medium"));
        let levels = model
            .reasoning_levels
            .as_ref()
            .expect("reasoning levels present");
        let level_names: Vec<&str> = levels.iter().map(|e| e.level()).collect();
        assert_eq!(level_names, vec!["low", "medium", "high"]);
    }

    #[test]
    fn coding_plan_materializes_force_think_and_off_dialect() {
        let mut cfg: Config =
            toml::from_str("[server]\nlisten = \"127.0.0.1:0\"\n").expect("minimal config");
        let entry = bailian_coding_plan_cn();
        cfg.providers.insert(entry.id.to_string(), entry.provider);
        apply_catalog_model_defaults(
            &mut cfg,
            "bailian-coding-plan-cn",
            Some(&[
                "minimax-m2.5-bailian-lp".to_string(),
                "glm-4.7-bailian-lp".to_string(),
            ]),
        )
        .expect("apply coding plan defaults");
        assert_eq!(
            cfg.models["minimax-m2.5-bailian-lp"].force_think,
            Some(true)
        );
        assert_eq!(
            cfg.models["glm-4.7-bailian-lp"]
                .chat_reasoning_off_format
                .as_deref(),
            Some("enable-thinking")
        );
        cfg.validate().expect("generated config validates");
    }

    /// : 百炼 Token Plan personal 与 team 中的 qwen3.7-max 正确暴露思考等级。
    #[test]
    fn bailian_token_plan_qwen_3_7_max_exposes_reasoning_levels() {
        let personal = bailian_token_plan_personal_cn();
        let team = bailian_token_plan_team_cn();
        for entry in [&personal, &team] {
            let model_fid = if entry.id == "bailian-token-plan-personal-cn" {
                "qwen3.7-max-token-plan-personal-lp"
            } else {
                "qwen3.7-max-token-plan-team-lp"
            };
            let template = find_template(entry, model_fid);
            assert_eq!(template.default_reasoning_level, Some("medium"));
            assert_eq!(template.reasoning_level_map, Some(QWEN_THINKING_LEVELS));

            let mut cfg: Config =
                toml::from_str("[server]\nlisten = \"127.0.0.1:0\"\n").expect("minimal config");
            cfg.providers
                .insert(entry.id.to_string(), entry.provider.clone());
            apply_catalog_model_defaults(&mut cfg, entry.id, Some(&[model_fid.to_string()]))
                .expect("apply defaults");

            let model = &cfg.models[model_fid];
            assert_eq!(model.default_reasoning_level.as_deref(), Some("medium"));
            let levels = model.reasoning_levels.as_ref().expect("levels present");
            let level_names: Vec<&str> = levels.iter().map(|e| e.level()).collect();
            assert_eq!(level_names, vec!["low", "medium", "high"]);
        }
    }

    /// : Kimi 平台与 Kimi Code 订阅中的 K3 模型正确暴露思考等级与默认 high。
    #[test]
    fn kimi_k3_templates_expose_reasoning_levels() {
        let platform_cn = kimi_platform_cn();
        let k3_plat = find_template(&platform_cn, "kimi-k3-lp");
        assert_eq!(k3_plat.default_reasoning_level, Some("high"));
        assert_eq!(k3_plat.reasoning_level_map, Some(KIMI_K3_THINKING_LEVELS));

        let sub = kimi_sub();
        let k3_sub = find_template(&sub, "k3-kimi-sub-lp");
        assert_eq!(k3_sub.default_reasoning_level, Some("high"));
        assert_eq!(k3_sub.reasoning_level_map, Some(KIMI_K3_THINKING_LEVELS));

        let mut cfg: Config =
            toml::from_str("[server]\nlisten = \"127.0.0.1:0\"\n").expect("minimal config");
        cfg.providers.insert(sub.id.to_string(), sub.provider);
        apply_catalog_model_defaults(&mut cfg, "kimi-sub", Some(&["k3-kimi-sub-lp".to_string()]))
            .expect("apply defaults");

        let model = &cfg.models["k3-kimi-sub-lp"];
        assert_eq!(model.default_reasoning_level.as_deref(), Some("high"));
        let levels = model.reasoning_levels.as_ref().expect("levels present");
        let level_names: Vec<&str> = levels.iter().map(|e| e.level()).collect();
        assert_eq!(level_names, vec!["low", "high", "max"]);
    }
    /// : StepFun payg 与 step-plan 中的 step-3.7-flash 正确暴露思考等级与默认 medium。
    #[test]
    fn stepfun_templates_expose_reasoning_levels() {
        let payg = stepfun_payg();
        let plan = stepfun_step_plan();
        for entry in [&payg, &plan] {
            let tmpl = find_template(entry, "step-3.7-flash-lp");
            assert_eq!(tmpl.default_reasoning_level, Some("medium"));
            assert_eq!(tmpl.reasoning_level_map, Some(STEPFUN_THINKING_LEVELS));

            let mut cfg: Config =
                toml::from_str("[server]\nlisten = \"127.0.0.1:0\"\n").expect("minimal config");
            cfg.providers
                .insert(entry.id.to_string(), entry.provider.clone());
            apply_catalog_model_defaults(
                &mut cfg,
                entry.id,
                Some(&["step-3.7-flash-lp".to_string()]),
            )
            .expect("apply defaults");

            let model = &cfg.models["step-3.7-flash-lp"];
            assert_eq!(model.default_reasoning_level.as_deref(), Some("medium"));
            let levels = model.reasoning_levels.as_ref().expect("levels present");
            let level_names: Vec<&str> = levels.iter().map(|e| e.level()).collect();
            assert_eq!(level_names, vec!["low", "medium", "high"]);
        }
    }
}
