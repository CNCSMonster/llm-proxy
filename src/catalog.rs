use crate::config::{
    AuthConfig, CompatConfig, Config, EndpointConfig, ModelConfig, Protocol, ProviderBinding,
    ProviderConfig, ReasoningLevelMapping,
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
        google_antigravity(),
        ollama(),
        kimi_platform_global(),
        kimi_platform_cn(),
        kimi_sub(),
        zhipu_payg_global(),
        zhipu_payg_cn(),
        zhipu_coding_cn(),
        bailian_coding_plan_cn(),
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

const fn tmpl(fid: &'static str, upstream: &'static str, ctx: i64, max_out: i64) -> ModelTemplate {
    ModelTemplate {
        frontend_id: fid,
        upstream_model: upstream,
        context_window: ctx,
        max_output_tokens: max_out,
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

pub fn mimo_reasoning_level_map() -> Vec<ReasoningLevelMapping> {
    vec![
        ReasoningLevelMapping {
            level: "minimal".to_string(),
            api_value: Some("low".to_string()),
        },
        ReasoningLevelMapping {
            level: "low".to_string(),
            api_value: Some("low".to_string()),
        },
        ReasoningLevelMapping {
            level: "medium".to_string(),
            api_value: Some("medium".to_string()),
        },
        ReasoningLevelMapping {
            level: "high".to_string(),
            api_value: Some("high".to_string()),
        },
        ReasoningLevelMapping {
            level: "xhigh".to_string(),
            api_value: Some("high".to_string()),
        },
    ]
}

fn bailian_templates() -> Vec<ModelTemplate> {
    vec![
        tmpl("qwen3.7-plus-bailian-lp", "qwen3.7-plus", 1_000_000, 65_536),
        tmpl("qwen3.6-plus-bailian-lp", "qwen3.6-plus", 1_000_000, 65_536),
        tmpl("qwen3.5-plus-bailian-lp", "qwen3.5-plus", 1_000_000, 65_536),
        tmpl("kimi-k2.5-bailian-lp", "kimi-k2.5", 262_144, 98_304),
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

fn mimo_templates() -> Vec<ModelTemplate> {
    vec![
        tmpl("mimo-v2.5-pro-lp", "mimo-v2.5-pro", 1_048_576, 131_072),
        tmpl("mimo-v2.5-lp", "mimo-v2.5", 1_048_576, 131_072),
    ]
}

fn stepfun_templates() -> Vec<ModelTemplate> {
    vec![tmpl("step-3.7-flash-lp", "step-3.7-flash", 256_000, 32_768)]
}

fn zhipu_templates() -> Vec<ModelTemplate> {
    vec![tmpl("glm-5.2-zhipu-cn-lp", "glm-5.2", 128_000, 32_768)]
}

fn kimi_platform_templates() -> Vec<ModelTemplate> {
    vec![
        tmpl("kimi-k3-lp", "kimi-k3", 1_048_576, 131_072),
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
/// Note: DeepSeek officially provides a native Responses endpoint at
/// `https://api.deepseek.com/v1/responses`, but it currently only supports
/// `deepseek-v4-flash` (as of 2026-08-04). `deepseek-v4-pro` is expected to
/// support native Responses in early August 2026.
///
/// Since the provider-level endpoint configuration is shared across all models,
/// we currently derive Responses from Chat for all models. Once v4-pro supports
/// native Responses, we can switch to native endpoint.
pub fn deepseek() -> ProviderCatalogEntry {
    entry(
        "deepseek",
        ProviderConfig {
            api_key_env: Some("DEEPSEEK_API_KEY".to_string()),
            openai_chat: Some(
                EndpointConfig::native("https://api.deepseek.com/chat/completions")
                    .with_compat(deepseek_thinking_compat()),
            ),
            openai_responses: Some(EndpointConfig::derived(Protocol::OpenaiChatCompletions)),
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
    .with_templates(vec![tmpl("gpt-5.5-lp", "gpt-5.5", 400_000, 128_000)])
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
        // gpt-5.5 / gpt-5.4 / gpt-5.4-mini verified against
        // /backend-api/codex/models (2026-08-01); gpt-5.3-codex and gpt-5.2 are
        // not supported by ChatGPT accounts and were removed.
        tmpl("gpt-5.5-sub-lp", "gpt-5.5", 272_000, 128_000),
        tmpl("gpt-5.4-sub-lp", "gpt-5.4", 272_000, 128_000),
        tmpl("gpt-5.4-mini-sub-lp", "gpt-5.4-mini", 272_000, 32_768),
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

pub fn google_antigravity() -> ProviderCatalogEntry {
    entry(
        "google-antigravity",
        ProviderConfig {
            auth: Some(AuthConfig::AntigravityOauth {
                account: Some("antigravity".to_string()),
            }),
            antigravity: Some({
                let mut ep = EndpointConfig::native(
                    "https://cloudcode-pa.googleapis.com/v1internal:streamGenerateContent",
                );
                // antigravity 上游行为（L1 验证 2026-08-02）：claude-* 与 gpt-oss-*
                // 走 Anthropic Messages 转换，functionCall/functionResponse 必须带 id。
                ep.anthropic_family_models = vec!["claude-*".into(), "gpt-oss-*".into()];
                ep
            }),
            openai_responses: Some(EndpointConfig::derived(Protocol::Antigravity)),
            anthropic: Some(EndpointConfig::derived(Protocol::Antigravity)),
            ..ProviderConfig::default()
        },
    )
    .with_templates(vec![
        // Gemini 3.6 Flash (L1 verified via fetchAvailableModels)
        tmpl(
            "gemini-3.6-flash-high-lp",
            "gemini-3.6-flash-high",
            1_048_576,
            65_536,
        ),
        tmpl(
            "gemini-3.6-flash-medium-lp",
            "gemini-3.6-flash-medium",
            1_048_576,
            65_536,
        ),
        tmpl(
            "gemini-3.6-flash-low-lp",
            "gemini-3.6-flash-low",
            1_048_576,
            65_536,
        ),
        // Gemini 3.5 Flash / 3 Flash Agent (L1 verified)
        tmpl(
            "gemini-3-flash-agent-lp",
            "gemini-3-flash-agent",
            1_048_576,
            65_536,
        ),
        tmpl(
            "gemini-3.5-flash-low-lp",
            "gemini-3.5-flash-low",
            1_048_576,
            65_536,
        ),
        tmpl(
            "gemini-3.5-flash-extra-low-lp",
            "gemini-3.5-flash-extra-low",
            1_048_576,
            65_536,
        ),
        // Gemini 3.1 Flash Lite (web search / mquery / commit message)
        tmpl(
            "gemini-3.1-flash-lite-lp",
            "gemini-3.1-flash-lite",
            1_048_576,
            65_535,
        ),
        // Gemini Pro (L1 verified)
        tmpl("gemini-pro-agent-lp", "gemini-pro-agent", 1_048_576, 65_535),
        tmpl(
            "gemini-3.1-pro-low-lp",
            "gemini-3.1-pro-low",
            1_048_576,
            65_535,
        ),
        // Claude (L1 verified, Anthropic-family conversion)
        tmpl(
            "claude-sonnet-4-6-ag-lp",
            "claude-sonnet-4-6",
            250_000,
            64_000,
        ),
        tmpl(
            "claude-opus-4-6-ag-lp",
            "claude-opus-4-6-thinking",
            250_000,
            64_000,
        ),
        // GPT-OSS (L1 verified, Anthropic-family conversion)
        tmpl("gpt-oss-120b-ag-lp", "gpt-oss-120b-medium", 131_072, 32_768),
    ])
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
        tmpl("k3-kimi-sub-lp", "k3", 1_048_576, 131_072),
        tmpl("k3-256k-kimi-sub-lp", "k3-256k", 262_144, 65_536),
        tmpl(
            "kimi-for-coding-kimi-sub-lp",
            "kimi-for-coding",
            262_144,
            32_768,
        ),
        tmpl(
            "kimi-for-coding-highspeed-kimi-sub-lp",
            "kimi-for-coding-highspeed",
            262_144,
            32_768,
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
    .with_templates(bailian_templates())
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
        reasoning_level_map: Some(mimo_reasoning_level_map()),
        ..ProviderConfig::default()
    }
}

pub fn mimo_payg() -> ProviderCatalogEntry {
    entry(
        "mimo-payg",
        mimo_provider("MIMO_API_KEY", "https://api.xiaomimimo.com"),
    )
    .with_templates(mimo_templates())
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
/// - deepseek-style 格式产出 reasoning_content 字段
fn stepfun_chat_compat() -> CompatConfig {
    CompatConfig {
        supports_developer_role: Some(false),
        supports_reasoning_effort: Some(true),
        thinking_format: Some("deepseek".to_string()),
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
                    t.context_window,
                    t.max_output_tokens,
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
        cfg.models
            .entry(t.frontend_id.to_string())
            .or_insert_with(|| {
                model_with_supported_protocols(
                    binding_name,
                    provider_config,
                    t.upstream_model,
                    t.context_window,
                    t.max_output_tokens,
                )
            });
    }
    Ok(())
}

/// Build a ModelConfig with bindings for all protocols the provider supports.
pub fn model_with_supported_protocols(
    provider_id: &str,
    provider_config: &ProviderConfig,
    upstream_model: &str,
    context_window: i64,
    max_output_tokens: i64,
) -> ModelConfig {
    let binding = || ProviderBinding {
        name: provider_id.to_string(),
        model: upstream_model.to_string(),
    };
    ModelConfig {
        description: None,
        context_window,
        max_output_tokens,
        features: Vec::new(),
        supported_reasoning_levels: Vec::new(),
        default_reasoning_level: None,
        enable_thinking: None,
        openai_chat_providers: if provider_config.openai_chat.is_some() {
            vec![binding()]
        } else {
            Vec::new()
        },
        openai_responses_providers: if provider_config.openai_responses.is_some() {
            vec![binding()]
        } else {
            Vec::new()
        },
        anthropic_providers: if provider_config.anthropic.is_some() {
            vec![binding()]
        } else {
            Vec::new()
        },
        reasoning_level_map: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            "google-antigravity",
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
                server: crate::config::ServerConfig {
                    listen: "127.0.0.1:8989".to_string(),
                    usage: crate::config::UsageConfig::default(),
                    max_sse_buffer_bytes: crate::config::default_max_sse_buffer_bytes(),
                    max_output_items: crate::config::default_max_output_items(),
                },
                fallback: crate::config::FallbackConfig::default(),
                protection: crate::config::ProtectionConfig::default(),
                status: crate::config::StatusConfig::default(),
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
        assert_eq!(responses.derive_from.as_deref(), Some("openai_chat"));
        assert!(responses.url.is_none());
        assert!(responses.compat.is_none());
    }

    #[test]
    fn oauth_catalog_entries_use_unified_store_auth_and_source_only_antigravity() {
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

        let ag = google_antigravity().provider;
        assert_eq!(
            ag.auth_config("google-antigravity").expect("auth"),
            AuthConfig::AntigravityOauth {
                account: Some("antigravity".to_string())
            }
        );
        assert!(ag.antigravity.as_ref().is_some_and(|e| e.url.is_some()));
        assert_eq!(
            ag.openai_responses
                .as_ref()
                .and_then(|e| e.derive_from.as_deref()),
            Some("antigravity")
        );
        assert_eq!(
            ag.anthropic.as_ref().and_then(|e| e.derive_from.as_deref()),
            Some("antigravity")
        );
        assert!(ag.openai_chat.is_none());
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
        let provider = kimi_sub().provider;
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
            assert!(provider.reasoning_level_map.is_some());
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
}
