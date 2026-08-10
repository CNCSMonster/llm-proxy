# llm-proxy Design Specification

Status: accepted (primary design authority for llm-proxy)
Scope: configuration model and implementation plan
Last updated: 2026-07-30

## 1. Goal

llm-proxy keeps its product goal: expose stable local endpoints for coding agents while hiding upstream API keys and provider-specific protocol differences.

The configuration ownership boundary:

- **Models** describe client-visible model IDs and, for each client protocol, the ordered upstream provider bindings that may serve that protocol.
- **Providers** describe how a concrete upstream service can provide each protocol endpoint.
- **Protocol conversion is declared under provider endpoints**, not implicitly inferred from model-provider bindings.

This makes the question "can model X use provider Y for protocol P?" a configuration validation question:

```text
model.<id>.<protocol>_providers includes provider Y
        ↓
provider Y must declare an endpoint for protocol P
        ↓
that endpoint may be native or derived from another endpoint through an adapter
```

## 1.1 Source Documents

This document is the single llm-proxy design authority. Supporting reference sources remain useful as evidence only:

- 内部 provider catalog 调研 — provider-product research evidence backing catalog defaults.
- [`../decisions/`](../decisions/) — accepted architecture decision records (ADRs); §17 lists how each applies.

If supporting evidence conflicts with this document, stop and ask for a design decision instead of silently changing this document.

## 2. Core Terms

### Client protocol

The protocol spoken by the local client to llm-proxy. Exactly three:

- `openai-chat`
- `openai-responses`
- `anthropic`

### Upstream-only protocol: `antigravity`

`antigravity` is a valid provider endpoint field, but it is **not** a client protocol. It exists only as a native endpoint pointing at a real Antigravity backend, and may only enter the serving chain as the `derive_from` source of a client-protocol endpoint. Consequently:

- there is no local antigravity route (§10.1 has no antigravity path);
- there is no `antigravity_providers` model binding list — models bind the three client protocols only;
- no derived endpoint may target antigravity (nothing needs conversion *toward* it); it is a source-only node in the endpoint graph;
- its auth is OAuth-based (Google account), referenced from the provider auth declaration rather than `api_key_env`.

### Provider auth

The current Rust implementation keeps the Go/v1 credential ownership boundary while making the user-facing provider config more regular:

- `config.toml` stores only the provider's auth mode and a credential/account reference.
- API-key providers reference environment variables; generated config should prefer env names over literal keys.
- OAuth refresh/access tokens are never stored in `config.toml`.
- Unlike Go/v1, the current Rust implementation stores all OAuth account material in **one unified OAuth store file**, not separate OpenAI and Antigravity files. The store contains multiple accounts keyed by a stable account key; by default that key is the provider ID.
- The runtime auth layer dispatches by account kind (`openai_oauth`, `antigravity_oauth`, etc.) inside that unified store.
- Status/log/diagnostic output must never print API keys, bearer tokens, OAuth refresh tokens, device codes, or raw authorization headers.

The concise TOML shape is:

```toml
[providers.deepseek]
api_key_env = "DEEPSEEK_API_KEY"

[providers.openai-subscription]
auth = { type = "openai_oauth", account = "openai-subscription" }

[providers.antigravity]
auth = { type = "antigravity_oauth", account = "antigravity" }

[providers.ollama]
auth = { type = "none" }
```

`api_key_env = "ENV_NAME"` is syntax sugar for `auth = { type = "api_key_env", env = "ENV_NAME" }`. Config normalization converts both forms to one runtime enum. A provider must not declare both `api_key_env` and `[auth]` / inline `auth = {...}` unless they describe the exact same API-key env reference; the recommended generated form for API-key products remains `api_key_env` because it is shorter and matches existing user expectations.

### Provider endpoint

A capability declared by a provider. It says that the provider can serve one protocol, either directly or through conversion.

Providers declare supported protocols as direct optional fields named after the protocol (e.g. `openai_chat`, `openai_responses`, `anthropic`, `antigravity`). Field presence means the provider supports that protocol. There is no `endpoints` map layer and no `protocol` field inside the endpoint; the field name itself is the protocol identifier, which structurally guarantees uniqueness per provider.

An endpoint has exactly one of two mutually exclusive fields, which also determines its kind — there is no separate `kind` field:

- `url`: complete upstream URL. Presence means **native endpoint** (no base/path concatenation).
- `derive_from`: sibling protocol field name (e.g. `openai_chat`). Presence means **derived endpoint**.

Optional field (**native endpoints only**):

- `compat`: an optional sub-table declaring the upstream API's compatibility quirks, used to drive asymmetric egress behavior (see §5.1). When absent, default compat behavior applies (standard OpenAI-compatible assumptions; see §5.1). Inspired by pi's provider-level `compat` object, but scoped per protocol endpoint because quirks belong to the concrete upstream API behind that endpoint, and two endpoints of one provider may differ.

Derived endpoints must not declare `compat`: a derived endpoint's outbound behavior is executed at its source chain's native endpoint, so it inherits compat from that native endpoint.

There are no capability flags (`streaming`, `tools`, etc.). All adapters support both non-streaming and streaming, and nothing in the runtime gates on such flags — declaring them would not drive any behavior. If a future upstream genuinely lacks streaming or tool support and the runtime learns to exploit that fact, the flag can be added then (adding an optional field is backward compatible).

There is no `adapter` field. Conversion semantics are uniquely determined by the (source protocol, target protocol) pair, so a derived endpoint only declares `derive_from`. Provider-specific conversion differences are data — `compat` fields consumed by the single adapter for that pair — not alternative adapter implementations.

### Native endpoint

A provider endpoint backed by a real upstream API URL with the same protocol.

Example: DeepSeek native Chat Completions endpoint with its upstream quirks declared.

```toml
[providers.deepseek]
api_key_env = "DEEPSEEK_API_KEY"

[providers.deepseek.openai_chat]
url = "https://api.deepseek.com/chat/completions"

[providers.deepseek.openai_chat.compat]
supports_developer_role = false
supports_reasoning_effort = true
thinking_format = "deepseek"
requires_reasoning_content_on_assistant_messages = true
max_tokens_field = "max_tokens"
```

### Derived endpoint

A provider endpoint backed by another endpoint on the same provider plus a protocol adapter.

Example: a provider has only Chat Completions upstream, but llm-proxy can expose an Anthropic-compatible provider endpoint by converting Anthropic Messages requests to Chat Completions requests.

```toml
[providers.provider-a]
api_key_env = "PROVIDER_A_API_KEY"

# Optional advanced override. If omitted, llm-proxy applies conservative
# provider-level defaults and later provider research may refine built-ins.
[providers.provider-a.request_frequency]
requests_per_minute = 60
# requests_per_hour = 1000
burst = 5
queue_timeout_seconds = 10

[providers.provider-a.openai_chat]
url = "https://api.example.com/v1/chat/completions"

[providers.provider-a.anthropic]
derive_from = "openai_chat"
```

After this declaration, `provider-a` is allowed in an Anthropic provider list even if the upstream service has no native Anthropic API. The `anthropic ← openai_chat` pair unambiguously selects the `anthropic-from-chat-completions` adapter.

## 3. Desired Config Shape

The target llm-proxy user-facing schema uses model-level protocol-specific provider binding lists, while conversion details live in providers.

Illustrative TOML:

```toml
[server]
listen = "127.0.0.1:8989"
# 聚合器内存保护（可选）：SSE 缓冲上限（字节）与 output item 数量上限，
# 防止恶意/挂死上游导致内存无界增长。不配置时用默认值 64MB / 4096。
# max_sse_buffer_bytes = 67108864
# max_output_items = 4096

[providers.provider-a]
api_key_env = "PROVIDER_A_API_KEY"

[providers.provider-a.openai_chat]
url = "https://api.example.com/v1/chat/completions"

[providers.provider-a.anthropic]
derive_from = "openai_chat"

[fallback]
max_retries = 2
timeout_seconds = 300  # 5 分钟，LLM 深度推理常态可达数分钟（ADR-024: 30s 会误判）
max_timeout_seconds = 600

[fallback.cooldown]
network_seconds = 30
server_error_seconds = 300
rate_limit_seconds = 300
model_unavailable_seconds = 1800
client_error_seconds = 300

[protection.bad_request]
enabled = true
window_seconds = 300
max_errors = 2
block_seconds = 300

[models.model-a]
description = "Model A via provider-a"
context_window = 200000
max_output_tokens = 8192
anthropic_providers = [
  { name = "provider-a", model = "upstream-model-a" },
]
openai_chat_providers = [
  { name = "provider-a", model = "upstream-model-a" },
]
```

Important semantics:

- `models.model-a.anthropic_providers` means `model-a` may serve Anthropic client requests through those provider bindings.
- Every binding in `anthropic_providers` must reference a provider that declares an `anthropic` endpoint.
- The provider's `anthropic` endpoint can be either:
  - native Anthropic upstream, or
  - derived Anthropic endpoint from another source endpoint (adapter uniquely determined by the protocol pair).
- Model bindings do not specify conversion details. They only choose provider name and upstream model ID.
- Runtime policy sections `[fallback]`, `[fallback.cooldown]`, and `[protection.bad_request]` use the names, fields, and defaults shown above (semantics in §10.5.5, §10.5.6). They are global runtime knobs, not per-provider/per-model settings; per-provider override can be added later as optional fields if a real need appears.
- `[providers.<id>.request_frequency]` is different: it is intentionally provider-scoped because request frequency is normally enforced by the upstream product/account/region pool. The whole block is optional; omitted providers still get conservative built-in defaults.

## 4. Validation Rules

Validation must reject ambiguous or impossible configuration before the server starts.

### 4.1 Provider validation

For every provider:

1. Provider ID/name must be unique.
2. At least one protocol endpoint field must be present.
3. Protocol endpoint field names must be from the supported protocol list: the client protocols `openai_chat`, `openai_responses`, `anthropic`, plus the upstream-only `antigravity`. Field uniqueness is structural — TOML field names are unique within a table, so no two endpoints on the same provider can claim the same protocol. `antigravity` endpoints must be native (they represent a real backend) and may only be referenced as `derive_from` sources.
4. Every endpoint must configure exactly one of `url` (native) or `derive_from` (derived). Configuring both or neither must fail validation.
5. Native endpoints (`url` present):
   - the `url` must be absolute and valid;
   - the optional `compat` sub-table is allowed here only (derived endpoints inherit compat from their source chain).
6. Derived endpoints (`derive_from` present):
   - `derive_from` must reference a protocol field present on the same provider;
   - the referenced endpoint must be **native** — multi-hop derived chains (derived from derived) are not allowed. This makes cycle-freedom and "resolves to a native endpoint with a concrete URL" structural guarantees rather than validation rules;
   - the adapter registry must contain an implementation for the source protocol → target protocol pair;
   - must not configure `compat` (inherited from the referenced native endpoint) — validation must reject it.
7. Provider auth must normalize to exactly one runtime `AuthConfig`:
   - `api_key_env = "ENV"` is accepted as the concise API-key form and normalizes to `AuthConfig::ApiKeyEnv`;
   - `auth.type = "api_key_env"` requires a non-empty `env`;
   - `auth.type = "openai_oauth"` and `auth.type = "antigravity_oauth"` use `auth.account` as the OAuth store key; if omitted, `account` defaults to the provider ID;
   - `auth.type = "none"` must not carry `env` or `account`;
   - unknown auth types fail startup validation with a field-path-rich error;
   - `api_key_env` and `auth` must not both be present unless they describe the same API-key env reference; generated configs should not emit both.
8. OAuth config validation does **not** require an existing token-store account. Missing, expired, or refresh-failed OAuth credentials are auth state shown by `status` / `provider info` and handled at request time, not invalid provider config. If a token-store record exists for `auth.account`, its `kind` must match the provider auth type before it is used.

### 4.2 Model validation

For every model and every protocol-specific provider list:

1. Each provider binding must include provider `name` and upstream `model`.
2. Provider names must not repeat in the same protocol list.
3. The referenced provider must exist.
4. The referenced provider must declare an endpoint whose protocol matches the model provider-list protocol.
5. If the provider endpoint is derived, the provider endpoint validation above must prove that it can be resolved to a native source endpoint.
6. If `supported_reasoning_levels` is declared: entries must be non-empty and unique, and `default_reasoning_level` (when set) must be one of them.
7. `reasoning_level_map` entries must have non-empty, unique levels.

Example invalid config:

```toml
[models.model-a]
anthropic_providers = [
  { name = "provider-a", model = "upstream-model-a" },
]
```

If `provider-a` declares only `openai_chat` and no `anthropic` endpoint field, validation must fail. It is not enough that llm-proxy might theoretically know a conversion. The provider has to explicitly declare that it offers the Anthropic endpoint.

### 4.3 Local path mapping is code, not config

There is no `routes` configuration section. Local path → client protocol mapping is a fixed client contract (see §10.1) and lives in code, not in user config. The only routing decision a request needs is model resolution (§5), and model resolution never falls back to a default: a requested model ID that is not in `models` is a client-protocol-shaped 4xx error, not a silent substitution.

All model provider bindings must be validated at startup, regardless of which models clients actually request.

## 5. Request Resolution

Request-time routing should follow this sequence:

```text
local endpoint path
  -> client protocol (fixed mapping in code, see §10.1)
  -> Layer 0a ingress review: structural validation against the client protocol spec (§5.1)
  -> requested model ID (required; unknown model ID -> 4xx, no default fallback)
  -> model's provider list for client protocol (empty list -> 4xx)
  -> request-content capability filter: if the request contains image input and the model
     does not declare `image_input`, or document input and the model does not declare
     `document_input`, reject locally with a client-protocol-shaped 4xx naming the missing
     capability — before any provider selection, with no cooldown and no fingerprint accounting
  -> Layer 1 fingerprint block check (§10.5.6): a blocked request shape is rejected locally
     with 429 before any provider is contacted
  -> select first healthy provider binding, respecting fallback/cooldown
  -> resolve provider endpoint for client protocol
  -> if derived (derive_from present): apply the adapter for the (source, target) protocol
     pair (single hop — derive_from must reference a native endpoint)
  -> Layer 0b egress review: validate/adapt the outbound body against the target native
     endpoint's compat (§5.1); conversion bugs fail locally, never forward
  -> send request to the resolved native endpoint url
  -> rewrite frontend model ID to binding.model before upstream request
  -> rewrite upstream model ID back to frontend model ID in downstream response
```

Model resolution has two distinct failure modes, both explicit errors in the client protocol's error shape:

- **Unknown model**: the requested model ID is not declared in `models` at all → 4xx. There is no default-model substitution; launch-generated client configs always carry known frontend model IDs, so an unknown ID means client/proxy config skew that the user should fix, not hide.
- **Protocol not supported**: the model exists but has no provider bindings for the request's protocol → 4xx explaining which protocols the model does support.

### Capability declaration is a trust contract

Model `features` (e.g. `image_input`, `document_input`) are declared only at the model layer. There is deliberately no config field describing whether an upstream model supports a capability — the proxy cannot verify upstream capabilities and does not try. Declaring a feature on a model and binding it to a provider means the operator asserts that the bound upstream model has that capability. If the binding is wrong (the upstream model actually lacks it), that is a configuration error: the upstream will reject the request and the normal error mapping (§10.5.8) surfaces it.

The capability filter above is the mirror image of this contract: it only *prevents* requests a model explicitly does not claim to support. It never tries to prove support. Capability filtering happens before cooldown filtering, so "no binding supports this content" is never masked by "all bindings are cooling down" — the two failures have different causes and different user actions.

## 5.1 Request Validation Layers

llm-proxy validates requests at two boundaries, with deliberately asymmetric strictness. This is Layer 0 of upstream protection, ahead of the fingerprint block and cooldown layers (§10.5).

**Governing principle**: local validation only ever blocks *locally detectable format errors* — requests that are structurally invalid regardless of any upstream state. Structurally valid requests are always forwarded to the upstream, even when the proxy can predict they will probably fail (expired token, exhausted quota, rate-limited account). Server-side failure conditions are classified and handled after the upstream responds (§10.5.8); the proxy never pre-judges account state locally.

### Ingress review (client → proxy)

Validate the inbound request against the **client protocol spec**. The client contract is fixed (§10.1), so strictness is safe here: missing required fields, wrong field types, malformed message/tool structures are client bugs and fail with a local client-protocol-shaped 4xx, with a clearer message than the upstream would give. Nothing is forwarded.

### Egress review (proxy → upstream endpoint)

Before sending to the concrete upstream endpoint, validate and adapt the outbound body against the **target native endpoint's `compat` declaration**. This layer is asymmetric by design: each provider endpoint declares what its upstream actually tolerates, and the adapter/forwarder applies that declaration:

| `compat` field | Egress behavior |
|---|---|
| `supports_developer_role` (default: `true`) | `false` → developer role messages are downgraded to system role. |
| `supports_reasoning_effort` (default: `false`) | `false` → `reasoning_effort` is stripped instead of forwarded into a likely 400. |
| `thinking_format` (default: none) | Selects the provider's thinking parameter format the adapter emits (see [`ADR-010`](../decisions/010-thinking-format-extension.md)). |
| `requires_reasoning_content_on_assistant_messages` (default: `false`) | `true` → multi-turn tool-call history must carry reasoning_content; the reasoning cache ([`ADR-002`](../decisions/002-server-side-reasoning-cache.md)) supplies it. |
| `max_tokens_field` (default: `max_tokens`) | Field name used when validating/emitting the max output tokens parameter. |
| `force_stream` (default: `false`) | `true` → `stream` is forced to `true` on outbound responses requests; the upstream only accepts streaming. A non-streaming client then receives an aggregated JSON response (see §5.1a). |
| `strip_max_output_tokens` (default: `false`) | `true` → `max_output_tokens` is stripped from outbound responses requests; the upstream rejects the parameter. |

When an endpoint has no `compat` sub-table, the defaults above apply (standard OpenAI-compatible assumptions).

### Egress response-side contract (force_stream aggregation)

When a native responses endpoint declares `compat.force_stream = true`, the upstream always streams. The runtime then chooses the client-facing response shape by the client's request (`stream`), recorded **before** egress adaptation:

| Client `stream` | Upstream behavior | Client-facing response |
|---|---|---|
| `true` | SSE passthrough | SSE events forwarded verbatim (only `model` field rewritten) |
| `false` | forced to `true` upstream | SSE aggregated back into a responses JSON value |

The aggregation path rebuilds `output` items from the upstream event stream: `message` items (text accumulated from `output_text.delta`, finalized by `output_text.done`) and `function_call` items (call_id/name/arguments) are preserved; other item types are dropped by design ([`ADR-023`](../decisions/023-responses-aggregation-type-scope.md)). Usage is recorded from `response.completed`.

### Egress review (proxy → upstream endpoint)

Strictness rule: egress review **adapts** what it safely can (rename, strip, downgrade) and only rejects what cannot be represented. Conversion-produced bodies must satisfy the target protocol schema — a conversion bug is a proxy bug: fail locally with an internal error, never forward it, never count it against the provider or the request fingerprint.

The two layers compose as:

```text
Layer 0a: ingress review (strict, client protocol spec)      → catches client bugs early
Layer 0b: egress review (asymmetric, endpoint compat)        → adapts to upstream quirks; catches proxy conversion bugs
Layer 1:  fingerprint block (§10.5.6)                        → catches repeated semantic 400s
Layer 2:  provider client_error cooldown (§10.5)         → catches shape-drifting 400 storms
```

Provider selection and protocol conversion are deliberately separate:

- Provider selection chooses a provider binding from the model's protocol-specific list.
- Endpoint resolution decides how that provider serves the selected protocol.
- Adapter execution converts between endpoint protocols when needed.

**相关独立设计**：
- Responses Egress 适配层（`force_stream` / `strip_max_output_tokens` compat 驱动，openai-sub 接入修复与验证记录）：[`docs/design/chatgpt-backend-responses-adaptation.md`](docs/design/chatgpt-backend-responses-adaptation.md)

## 6. Adapter Registry

Adapters are explicit named conversion implementations with **exactly one implementation per (source protocol, target protocol) pair**. Conversion semantics are defined per pair by conversion specs; provider-specific variations are data consumed by that one adapter (provider policy fields such as `thinking_format` or `max_tokens_field`), never a second adapter for the same pair. Config never names adapters — a derived endpoint selects its conversion by declaring `derive_from` alone.

Required adapter set (all 6 bidirectional directions across the three client protocols, plus the two pairs that let the upstream-only Antigravity backend serve client protocols):

| Adapter | Source protocol | Target protocol | Purpose |
|---|---|---|---|
| `responses-from-chat-completions` | `openai-chat` | `openai-responses` | Serve Codex Responses clients through Chat Completions upstreams. |
| `chat-completions-from-responses` | `openai-responses` | `openai-chat` | Serve Chat Completions clients through Responses upstreams. |
| `anthropic-from-chat-completions` | `openai-chat` | `anthropic` | Serve Claude-compatible clients through Chat Completions upstreams. |
| `chat-completions-from-anthropic` | `anthropic` | `openai-chat` | Serve Chat Completions clients through Anthropic upstreams. |
| `responses-from-anthropic` | `anthropic` | `openai-responses` | Serve Responses clients through Anthropic upstreams. |
| `anthropic-from-responses` | `openai-responses` | `anthropic` | Serve Anthropic clients through Responses upstreams. |
| `responses-from-antigravity` | `antigravity` | `openai-responses` | Serve Responses clients through the Antigravity backend. |
| `anthropic-from-antigravity` | `antigravity` | `anthropic` | Serve Anthropic clients through the Antigravity backend. |

Antigravity appears only as a source protocol in this table, matching its source-only role (§2). There is deliberately no adapter targeting antigravity and no `chat-completions-from-antigravity` — no current serving scenario needs Chat clients over the Antigravity backend; the pair can be registered later if one appears.

All adapters support both non-streaming and streaming variants.

Naming convention: adapter name describes the endpoint being provided from the source protocol, using `target-from-source`. Adapter names are internal registry identifiers and never appear in user config.

The validation registry must know each adapter's supported source and target protocol pair. Validation of a derived endpoint is simply: the `derive_from` field references an endpoint present on the same provider, and the registry contains an adapter for the (source protocol, endpoint protocol) pair. Runtime code must not infer adapter compatibility from string matching; it must go through the registry.

### Adapter compatibility policy

The current Rust implementation must preserve Go/v1's forwarding ability. The Go/v1 converters already implement the major serving directions and are generally best-effort: core message/tool/multimodal/reasoning structures are converted, while some unknown or provider-specific fields may be ignored rather than causing local rejection. The current Rust implementation must not introduce broad new adapter rejections that break requests Go/v1 could forward unless the difference is explicitly accepted in this design document.

Each adapter contract should classify request/response fields into three documented outcomes:

| Outcome | Meaning | Default policy |
|---|---|---|
| converted | The field is preserved with equivalent target-protocol semantics. | Required for core Go/v1-supported semantics: text messages, system/developer role handling, tool declarations, tool calls/results, tool-call IDs, image/document blocks where Go/v1 supports them, thinking/reasoning fields covered by provider/model policy, streaming deltas, and model ID rewriting. |
| degraded/dropped | The field has no useful target representation and is intentionally omitted or approximated. | Allowed only when documented as Go/v1-compatible best-effort behavior or as an intentional parity difference. Unknown extension fields, non-core metadata, fine-grained usage subfields, annotations/citations without a target equivalent, and unsupported provider-private extras normally belong here. |
| local reject | The proxy returns a client-protocol-shaped local 4xx/5xx and does not contact upstream. | Reserved for cases where forwarding would violate a hard user contract, create a known upstream 400 loop, expose unsafe local behavior, or indicate a proxy/config bug. |

Initial local rejects should stay close to Go/v1 behavior:

- unsupported protocol/endpoint combinations or missing adapter registry entries;
- malformed tool-call history such as orphan tool calls;
- reasoning/thinking conflicts against configured model/provider policy;
- impossible endpoint graphs caught during startup validation;
- request shapes requiring unsafe local file/network access not explicitly implemented by the adapter;
- egress bodies produced by a proxy conversion bug that fail the target native endpoint schema.

Hard output constraints such as strict structured output/JSON schema are not automatically rejected. The adapter must either convert them, intentionally degrade/drop them with a documented Go/v1-compatible reason, or reject them if silent degradation would be worse for the supported client. The choice belongs in the adapter contract in this design document, not in ad-hoc forwarding code.

**相关独立设计**：
- 上游格式族声明（endpoint 级 `anthropic_family_models`）与 antigravity 协议绑定（转换器按模型族选择 Gemini 原生 / Anthropic 转换路径）：[`docs/design/model-format-family-and-antigravity-bindings-design.md`](docs/design/model-format-family-and-antigravity-bindings-design.md)

## 7. Target Rust Config Structures

The target provider shape uses direct optional protocol fields:

```rust
pub struct ProviderConfig {
    pub auth: Option<AuthConfig>,
    pub product: String,  // 产品级标识（如 "deepseek"、"kimi"），默认 "custom"；同产品的 provider 可批量 fallback
    pub openai_chat: Option<EndpointConfig>,
    pub openai_responses: Option<EndpointConfig>,
    pub anthropic: Option<EndpointConfig>,
    pub antigravity: Option<EndpointConfig>,
    // Optional provider-level defaults; model-level fields override them.
    pub reasoning_level_map: Option<Vec<ReasoningLevelMapping>>,
    pub enable_thinking: Option<bool>,
}

pub enum AuthConfig {
    ApiKeyEnv { env: String },
    OpenAiOAuth { account: String },
    AntigravityOAuth { account: String },
    None,
}

pub enum Protocol {
    OpenAiChat,
    OpenAiResponses,
    Anthropic,
    Antigravity,
}

pub enum EndpointConfig {
    Native {
        url: String,                  // complete upstream URL
        compat: Option<CompatConfig>, // absent means default compat behavior (§5.1)
        evidence: EndpointEvidence,
        anthropic_family_models: Vec<String>,  // glob 匹配哪些 upstream model 走 Anthropic Messages 转换
        store: Option<bool>,                    // 强制 store 值（如 ChatGPT backend 强制 store: false）
    },
    Derived {
        derive_from: Protocol,        // sibling protocol field; adapter inferred from protocol pair
    },
}

pub enum EndpointEvidence {
    SourceBacked,
    PendingE2E,
    PrivateBackend,
    UserCustom,
}
```

The user-facing TOML still uses `url` vs `derive_from` fields because that shape is concise. The loaded runtime config should normalize it into an enum so native/derived exclusivity is impossible to violate after validation. Likewise, user-facing `api_key_env` or `auth = {...}` is normalized into `AuthConfig`; request-time code must not inspect raw auth strings. `EndpointEvidence` is generated from built-in catalog metadata or user/custom origin; it is not a protocol behavior switch. It exists so `connect`, `status`, release gates, and tests can distinguish stable source-backed endpoints from `pending-e2e` or private-backend endpoints.

Field presence on `ProviderConfig` means the provider supports that protocol. Uniqueness is structural — Rust struct fields are unique by definition.

The target model shape uses protocol-specific provider binding lists:

```rust
pub struct ModelConfig {
    pub description: Option<String>,
    pub context_window: i64,
    pub max_output_tokens: i64,
    pub features: Vec<String>,  // 如 ["tool_call", "image_input"]，用字符串而非枚举（ADR-024）
    pub providers_by_protocol: BTreeMap<Protocol, Vec<ModelProviderBinding>>,
    // Reasoning level vocabulary of the upstream model(s) behind this frontend model.
    pub supported_reasoning_levels: Vec<String>,
    pub reasoning_level_map: Option<Vec<ReasoningLevelMapping>>, // level -> upstream API value; overrides provider default
    pub default_reasoning_level: Option<String>, // used when the client sends no reasoning_effort
    pub enable_thinking: Option<bool>,                 // default thinking switch; overrides provider default
}

pub struct ModelProviderBinding {
    pub name: String,
    pub model: String,
}

pub enum ModelFeature {
    ToolCall,
    ImageInput,
    DocumentInput,
    VideoInput,
    StructuredOutput,
}

pub enum ModelMetadataSource {
    BuiltInResearch { doc: String, refreshed_at: Date },
    DynamicProbe { provider: String, source_url: String, probed_at: DateTime },
    UserOverride,
}

pub enum ReasoningLevel {
    None,
    Minimal,
    Low,
    Medium,
    High,
    XHigh,
    Max,
}

pub enum ThinkingFormat {
    // Chat Completions 端点的 thinking 格式
    ReasoningEffort,              // OpenAI 标准：reasoning_effort: "low|medium|high"
    DeepSeek,                     // DeepSeek/Qwen/StepFun Chat：enable_thinking: true
    MimoThinkingToggle,           // MiMo Chat：thinking: {type: "enabled"}（无 budget_tokens）
    ZhipuThinking,                // Zhipu Chat：thinking: {type: "enabled", clear_thinking: false}
    KimiReasoningEffort,          // Kimi K3 Chat：reasoning_effort: "low|high|max"
    Zai,                          // Z.ai Chat：同 DeepSeek 格式

    // Anthropic Messages 端点的 thinking 格式
    AnthropicThinking,            // Anthropic/MiMo Anthropic/Zhipu Anthropic：thinking: {type: "enabled", budget_tokens: N}

    // OpenAI Responses 端点的 thinking 格式
    OpenaiResponsesReasoning,     // OpenAI/MiMo Responses：reasoning: {effort: "low|medium|high"}

    // OpenRouter 特殊格式（透传上游）
    OpenrouterChatReasoningDetails,
    OpenrouterResponsesReasoning,
    OpenrouterAnthropicMessages,

    // Gemini/Antigravity 格式
    GeminiThinkingConfig,         // Antigravity/Gemini：generationConfig: {thinkingConfig: {...}}

    // Qwen 特殊格式
    Qwen,                         // 百炼 Chat：enable_thinking: true（同 DeepSeek）
    QwenChatTemplate,             // Qwen chat-template 格式
    QwenChatTemplateKwargs,       // Qwen chat-template-kwargs 格式
    QwenEnableThinking,           // 显式 enable_thinking（同 DeepSeek）
}

pub enum MaxTokensField {
    MaxTokens,
    MaxCompletionTokens,
    MaxOutputTokens,
}
```

String values from TOML are decoded at the configuration boundary into these enums. Runtime code should match on enums, not raw strings.

`metadata_source` is not required for user-authored custom models, but every built-in or probe-generated model should carry enough provenance for `status`, `connect`, and docs/debug output to explain whether its context/features came from built-in research, a timestamped dynamic probe, or an explicit user override. This is a design response to the provider research reality: not all model capability tables are complete, and some providers such as OpenRouter/Ollama are intentionally dynamic.

### Raw vs runtime config boundary

llm-proxy should separate the user-facing TOML decode shape from the runtime configuration shape:

```text
TOML -> RawConfig -> validation/normalization -> Config
```

`RawConfig` may use `Option<T>` and strings because it mirrors TOML. Runtime `Config` should use resolved booleans, parsed enums, validated URLs, and enum variants for mutually exclusive states. Request-time code should not repeatedly apply defaults or parse strings.

Implementation constraints:

- `RawEndpointConfig` may contain `url: Option<String>`, `derive_from: Option<String>`, and `compat: Option<RawCompatConfig>`.
- Runtime `EndpointConfig` is `Native { url, compat, evidence } | Derived { derive_from }`.
- `RawCompatConfig` may contain optional booleans/strings; runtime `CompatConfig` uses `Option<T>` types with `effective_*` accessors providing defaults.
- `RawModelConfig` mirrors TOML's protocol-specific fields, for example `openai_chat_providers`, `openai_responses_providers`, and `anthropic_providers`, plus TOML strings for `features`, reasoning levels. Normalization converts these fields into `providers_by_protocol: BTreeMap<Protocol, Vec<ModelProviderBinding>>`.
- Defaults such as `supports_developer_role = true` and `max_tokens_field = "max_tokens"` are applied via `effective_*` accessor methods, not scattered through proxy code.
- Derived endpoints cannot carry compat in runtime because the config shape has no such field.

**设计决策（ADR-024）**：配置字段保持 `Option<String>` 而非闭合枚举。理由：上游格式变化频繁（DeepSeek、OpenAI、Google、Kimi 各自有 thinking 格式），闭合枚举会导致 serde 反序列化失败 → config 加载失败 → 代理无法启动；`Option<String>` + 白名单校验已能捕获非法值，且允许未知值透传平滑降级。未来若行业格式标准化，可再改为枚举。

实际运行时结构：

```rust
pub struct CompatConfig {
    pub supports_developer_role: Option<bool>,
    pub supports_reasoning_effort: Option<bool>,
    pub thinking_format: Option<String>,
    pub max_tokens_field: Option<String>,
    pub force_stream: Option<bool>,
    pub strip_max_output_tokens: Option<bool>,
    pub must_not_store: Option<bool>,
}

impl CompatConfig {
    pub fn effective_supports_developer_role(&self) -> bool {
        self.supports_developer_role.unwrap_or(true)
    }
    // ... 其他 effective_* 方法
}
```

### Field level assignment for thinking/reasoning

The four thinking-related field groups live at two levels, because they describe two different things (evidence: 内部 provider 调研):

- **Endpoint `compat`** (`thinking_format`, `supports_reasoning_effort`): properties of the upstream API surface. The same provider can use different parameter names per endpoint — StepFun's Chat endpoint takes `reasoning_effort` while its Anthropic endpoint takes `output_config.effort` — so these cannot live at provider or model level.
- **Model entry** (`reasoning_level_map`, `supported_reasoning_levels`, `default_reasoning_level`, `enable_thinking`): the reasoning level vocabulary and defaults of the upstream model behind a frontend model. Level vocabularies differ per model (DeepSeek accepts only `high`/`max`, OpenRouter accepts six levels, ChatGPT subscription four), so these cannot live at endpoint or provider-only level.
- **Provider-level optional defaults** (`reasoning_level_map`, `enable_thinking`): when every model of a provider shares one vocabulary (e.g. DeepSeek), declaring the map once at provider level avoids repeating it on every model. Resolution order: model field > provider default > no mapping (forward as-is).

A frontend model whose bindings span upstreams with incompatible level vocabularies is a configuration error under the trust contract (§5); catalog and `connect` only create bindings with compatible vocabularies.

## 8. Design Invariants

These invariants should remain true as llm-proxy grows:

1. A model provider binding is legal only if the provider declares an endpoint field for the binding's protocol.
2. A provider may declare a protocol endpoint field even when the upstream does not natively support that protocol, but only by declaring a valid derived endpoint.
3. Conversion details live in provider endpoint config, not in model bindings.
4. Runtime protocol conversion must follow the configured provider endpoint declarations.
5. Startup validation should catch impossible model/provider/protocol combinations before any request is handled.
6. Local path → protocol mapping is fixed client contract in code; there is no `routes` section — routing-relevant config consists only of providers and models.
7. Frontend model IDs and upstream model IDs stay separate.
8. Unknown requested model IDs and known models lacking a binding for the request protocol both produce explicit client-protocol-shaped 4xx errors; no silent default substitution.
9. Capability declarations live only on models (`features`); they are a trust contract with the operator, never probed against upstreams.
10. ~~Closed protocol/config variants should be represented as Rust enums~~ **已决策（ADR-024）**：配置字段保持 `Option<String>` 而非闭合枚举。理由：上游格式变化频繁，闭合枚举会导致 config 加载失败；`Option<String>` + 白名单校验已能捕获非法值，且允许未知值透传平滑降级。

## 9. Example: Why the Endpoint Declaration Matters

Invalid:

```toml
[providers.provider-a]
api_key_env = "PROVIDER_A_API_KEY"

[providers.provider-a.openai_chat]
url = "https://api.example.com/v1/chat/completions"

[models.model-a]
anthropic_providers = [
  { name = "provider-a", model = "upstream-model-a" },
]
```

Reason: `provider-a` has no `anthropic` endpoint field.

Valid:

```toml
[providers.provider-a]
api_key_env = "PROVIDER_A_API_KEY"

[providers.provider-a.openai_chat]
url = "https://api.example.com/v1/chat/completions"

[providers.provider-a.anthropic]
derive_from = "openai_chat"

[models.model-a]
anthropic_providers = [
  { name = "provider-a", model = "upstream-model-a" },
]
```

Reason: `provider-a` explicitly declares that it can provide Anthropic protocol, and the declaration explains how: by deriving it from its `openai_chat` endpoint (adapter inferred from the protocol pair).

## 10. Product Requirements

llm-proxy is a full local LLM gateway, not a limited proof of concept. This section defines the required product surface.

### 10.1 Supported local client protocols

llm-proxy must expose these client-facing protocol families:

| Client protocol | Local paths | Required for |
|---|---|---|
| OpenAI Chat Completions | `/openai/v1/chat/completions` | pi, Qwen Code, generic OpenAI-compatible clients |
| OpenAI Responses | `/responses/v1/responses`, `/openai/v1/responses` | Codex CLI/Desktop and Responses-native clients |
| Anthropic Messages | `/anthropic/v1/messages` | Claude Code and generic Anthropic-compatible clients |
| Claude Desktop gateway | `/claude-desktop/v1/messages`, `/claude-desktop/v1/models`, optional `/claude-desktop/v1/messages/count_tokens` | Claude Desktop / Claude Code Desktop 3P gateway profile |
| Model listing | protocol-prefixed `/models` only | launch-generated clients and interactive model switching |

These paths are the fixed client contract, hardcoded in the server — they are not user-configurable routes (see §4.3). Provider endpoint URLs are upstream-facing and must not leak into client launch config.

**No compatibility routes**: llm-proxy uses only protocol-prefixed routes. There are no `/v1/...` compatibility routes. All clients must use the protocol-specific prefix (`/openai/v1/...`, `/responses/v1/...`, `/anthropic/v1/...`).

Model listing semantics:

| Path | Response |
|---|---|
| `/openai/v1/models` | models with an `openai_chat` **or** `openai_responses` binding (union — both protocols are served under this prefix) |
| `/responses/v1/models` | models with an `openai_responses` binding only |
| `/anthropic/v1/models` | models with an `anthropic` binding only (Anthropic list format) |

The prefixed listings let a client configured with one protocol prefix discover exactly the models it can use through that prefix.

### 10.2 Provider/product coverage

The provider catalog must be able to represent these provider products:

- OpenAI PAYG: Chat Completions and Responses.
- Anthropic PAYG: Messages.
- DeepSeek: Chat Completions and Anthropic-compatible endpoint.
- OpenRouter: Chat Completions and Anthropic-compatible endpoint.
- Zhipu / BigModel CN PAYG and CN Coding Plan; global Z.ai remains evidence-gated until docs/E2E are refreshed.
- MiMo PAYG and Token Plan; PAYG is source-backed for Chat/Responses/Anthropic, Token Plan CN/SGP/AMS are advanced until host/key evidence is refreshed.
- Bailian PAYG region variants and CN Coding Plan.
- Kimi Open Platform CN PAYG and Kimi Code API as separate source-backed API-key products; Kimi Code managed OAuth remains advanced until auth flow is implemented.
- StepFun PAYG and Step Plan, both as `.ai` products with native Chat and Messages endpoints; Responses derived unless native evidence appears.
- Ollama local OpenAI-compatible Chat/Responses and Anthropic-compatible Messages endpoints; model inventory/capabilities are dynamic and local.
- ChatGPT subscription provider through OpenAI OAuth.
- Google Antigravity provider through Google OAuth and Antigravity protocol adapters.

Catalog entries should be grouped as products for `connect` UX. A product may install multiple provider endpoint declarations that share one API key environment variable or one OAuth account.

### 10.2.1 Provider ID 命名规则

Provider ID（即 catalog product ID 和用户配置中的 provider 实例名）遵循 **平台-产品类型-区域** 命名规则：

| 维度 | 说明 | 示例 |
|------|------|------|
| 平台 | 厂商名称（小写） | `kimi`, `google`, `openai`, `deepseek` |
| 产品类型 | 产品/计费方式简称 | `platform`（开放平台）, `code`（编程订阅）, `payg`（按量）, `sub`（订阅）, `token`（Token Plan） |
| 区域 | 仅在需要区分时出现 | `cn`, `global`, `sgp`, `ams`, `us` |

**规则要点**：
1. 平台名必须体现公司/品牌（如 `google-antigravity` 而非 `antigravity`）
2. 同一平台不同区域 = 不同产品实例（如 `kimi-platform-cn` 和 `kimi-platform-global` 是两个独立 provider，因为 API Key 不互通）
3. 产品类型用简称（`sub` 而非 `subscription`，`payg` 而非 `pay-as-you-go`）
4. 无区域区分时省略区域后缀（如 `deepseek` 而非 `deepseek-global`）

**当前 catalog 命名对照**：

| Provider ID | 产品 | 说明 |
|-------------|------|------|
| `deepseek` | DeepSeek API | 单一产品，无区域区分 |
| `openai-payg` | OpenAI 按量付费 | PAYG = Pay As You Go |
| `openai-sub` | OpenAI 订阅 | sub = subscription |
| `google-antigravity` | Google Antigravity | 公司 + 产品名 |
| `kimi-platform-cn` | Kimi 开放平台 CN | 中国区按量付费 |
| `kimi-platform-global` | Kimi 开放平台 Global | 国际区按量付费 |
| `kimi-sub` | Kimi Code 订阅 | 编程专用订阅制（Kimi 订阅计划额度） |
| `mimo-token-plan-cn` | MiMo Token Plan CN | CN 区域 Token Plan |
| `mimo-token-plan-sgp` | MiMo Token Plan SGP | SGP 区域 Token Plan |

### 10.3 Provider configuration fields

Provider config consists of:

| Field | Meaning |
|---|---|
| provider table key | provider ID (product/region/account scoped) |
| protocol endpoint fields | which protocols the provider can serve; presence is the capability declaration |
| native endpoint `url` | complete upstream URL, with env override support |
| `api_key_env` | concise API-key env var reference; normalized to `AuthConfig::ApiKeyEnv` |
| `auth` | optional typed auth declaration for OAuth/no-key/advanced API-key cases |
| auth strategy | API key env, OpenAI OAuth, Antigravity OAuth, or no auth |
| native endpoint `compat.thinking_format` | provider's thinking parameter format |
| native endpoint `compat.supports_reasoning_effort` | whether upstream accepts `reasoning_effort` |
| native endpoint `compat.requires_reasoning_content_on_assistant_messages` | whether assistant tool-call turns must carry reasoning_content |
| native endpoint `compat.max_tokens_field` | upstream field name for max output tokens |
| `reasoning_level_map` | model entry, with optional provider-level default (model overrides) |
| `enable_thinking` | model entry, with optional provider-level default (model overrides) |

**Egress review (Layer 0b) 实现状态**：

当前实现中，compat 字段在 `config.rs:validate_compat` 中验证，但转发逻辑（Layer 0b egress review）**尚未完整实现**：

| Compat 字段 | 验证 | 转发消费 | 状态 |
|------------|------|---------|------|
| `thinking_format` | ✅ | ❌ | 验证白名单已包含 14 种格式，但转发时未根据此字段格式化 thinking 参数 |
| `supports_reasoning_effort` | ✅ | ❌ | 验证通过，但转发时未根据此字段剥离 reasoning_effort |
| `requires_reasoning_content_on_assistant_messages` | ✅ | ✅ | reasoning cache 注入已实现 |
| `max_tokens_field` | ✅ | ❌ | 验证通过，但转发时未根据此字段映射字段名 |
| `supports_developer_role` | ✅ | ❌ | 验证通过，但转发时未根据此字段降级 developer role |

**待实现**：在 `proxy.rs` 的转发路径中添加 egress review 层，根据 endpoint compat 字段适配出站请求。这是 M4（Proxy runtime）的待完成项。

Environment-variable endpoint overrides must remain possible. The `${ENV}|default` full-URL form maps directly to the endpoint `url` field — no base/path splitting needed. The user-facing invariant is that custom endpoint URLs and custom providers remain configurable from CLI and TUI.

OAuth credential state is not provider config. llm-proxy uses one unified OAuth token store under the application config directory: `$XDG_CONFIG_HOME/llm-proxy/oauth_accounts.json`, falling back to `~/.config/llm-proxy/oauth_accounts.json` on Unix-like systems, and the platform config directory equivalent on Windows. The store contains typed account records for all OAuth-backed products:

```json
{
  "version": 1,
  "accounts": {
    "openai-subscription": {
      "kind": "openai_oauth",
      "account_id": "...",
      "email": "...",
      "refresh_token": "...",
      "authenticated_at": 123,
      "last_refresh": 456,
      "expires_at": 789
    },
    "antigravity": {
      "kind": "antigravity_oauth",
      "email": "...",
      "project_id": "...",
      "refresh_token": "...",
      "access_token": "...",
      "authenticated_at": 123,
      "last_refresh": 456,
      "expires_at": 789
    }
  }
}
```

The example fields mirror Go/v1's two stores (`oauth_tokens.json` and `antigravity_tokens.json`) but collapse them into one typed store. OpenAI access tokens may remain in-memory only; Antigravity may persist the current access token if needed for parity with Go/v1, but refresh tokens are the authoritative long-lived credential. Store writes must be atomic and owner-only (`0600` on Unix-like systems). The store is keyed by `auth.account`, not necessarily by provider ID. If `auth.account` is omitted, normalization defaults it to the provider ID. Provider ID remains the routing/model-binding identity; `auth.account` is the credential identity and may be shared by multiple providers only when the user explicitly configures that sharing.

OAuth store load/write rules:

- unknown top-level `version` fails with an actionable error before token use;
- unknown account `kind` is ignored for unrelated providers and shown by `status` as unsupported, but a provider whose `auth.account` resolves to that record cannot use it;
- provider auth type and store record `kind` must match before a token is used;
- a missing account record means unauthenticated, not invalid config;
- refresh/write paths must serialize read-modify-write operations with an async mutex or file lock to avoid lost updates;
- writes use temp file + fsync + rename and preserve restrictive permissions.

### 10.4 Model catalog coverage

Model metadata semantics:

- frontend model ID is stable and separate from upstream model ID;
- `description`, `context_window`, `max_output_tokens`, `features`, and `metadata_source`;
- `image_input`, document/PDF input, `tool_call`, thinking/reasoning-related feature flags;
- model-level reasoning level map, supported reasoning levels, default reasoning level, and thinking default (level assignment rationale in §7);
- product default markers used by `connect` to preselect or install recommended models;
- protocol-specific provider binding order for fallback.

The schema allows one frontend model to be served by different upstream model names per protocol/provider binding.

For release-built built-ins, every non-conservative model capability used by launch generation must have a provenance path: provider research doc, official snapshot, dynamic probe cache entry, or explicit user override. If context/max-output/capability evidence is missing, the built-in catalog must either omit the field/feature from generated launch metadata or mark the product/model as advanced/evidence-gated. Do not fill unknown context, max output, image/video/document, tool, or reasoning capability from model-name guesses.

Research completeness is therefore tiered, not binary:

| Catalog tier | Endpoint evidence requirement | Model metadata requirement | Can be generated by default? | Examples / current notes |
|---|---|---|---|---|
| `stable-default` | complete native URL or valid derived endpoint backed by current provider research, official snapshot, or E2E | selected default models have source-backed/probe-backed context, max output when needed, input modality, tool, and reasoning metadata used by launch | yes | DeepSeek PAYG, Kimi Code API selected models, MiMo PAYG, source-backed Ollama endpoints with dynamic local models |
| `stable-endpoint-partial-models` | endpoint graph is source-backed | only a conservative subset of model metadata is known; unknown features stay unadvertised and launch omits derived capability fields | yes, but only for models whose required client metadata is known | OpenAI PAYG, Anthropic PAYG, Bailian PAYG/Coding Plan, Zhipu CN until richer model tables/E2E are captured |
| `dynamic-catalog` | provider model listing/probe endpoint is source-backed | model entries come from explicit probe cache with provenance and staleness | yes after `connect`/refresh writes deterministic local metadata | OpenRouter, Ollama |
| `advanced-requires-verification` | endpoint/product evidence is incomplete, inferred, regional/account-specific, or pending E2E | model metadata may be incomplete or account-gated | no stable default; expose behind advanced/custom UI with warning | MiMo Token Plan SGP/AMS, Bailian Token Plan, Zhipu Coding Plan Anthropic until E2E, global Z.ai |
| `private-backend` | observed source/E2E rather than public stable API | model metadata can drift server-side and must be refreshed close to release | opt-in only, with auth/status probes | ChatGPT Subscription/Codex backend, Antigravity |

Release gating rule: a built-in provider can be shipped before every upstream model is exhaustively researched, but only if the shipped model entries obey the tier above. Endpoint support and model capability support are separate claims. A provider may be endpoint-complete while model-capability-incomplete; in that case launch generation must only expose the verified subset and must surface the missing evidence in `connect`/`status` instead of fabricating defaults.

Catalog tier and endpoint evidence compose as two dimensions: catalog tier is product/model release policy, while `EndpointEvidence` is per-endpoint provenance. For example, one `stable-endpoint-partial-models` product can contain `SourceBacked` Chat/Responses endpoints but only expose a conservative model subset; one `advanced-requires-verification` product may contain a `SourceBacked` Chat endpoint and a `PendingE2E` Anthropic endpoint; one `private-backend` product should use `PrivateBackend` native endpoints even if E2E currently passes.

### 10.4.1 Dynamic model discovery for gateway/local providers

Dynamic model catalogs:

- Some providers should not be represented by a static exhaustive built-in model table. OpenRouter and Ollama are the first-class examples.
- OpenRouter model entries must be obtained through explicit discovery (`GET https://openrouter.ai/api/v1/models`) and cached with timestamp/source metadata. The probed row supplies upstream model ID, `context_length`, input/output modalities, `supported_parameters`, provider max output, and optional reasoning metadata. User-selected IDs are stored verbatim.
- Ollama model entries must be obtained from the configured daemon through explicit discovery (`GET /api/tags`, optionally OpenAI-compatible `/v1/models` for broad listing) and `POST /api/show` for selected tags. `/api/show` supplies capabilities, details, parameters such as `num_ctx`, and `model_info.*.context_length` where available.
- Dynamic discovery is triggered by `connect` or `status --probe`/future explicit refresh commands only. Launch commands and default `status` read configured model entries and cached metadata; they must not perform surprise network/local daemon probes.
- Probe-derived metadata is still a trust contract once written into config/cache. Runtime request validation uses local model entries, not live upstream probing.
- Missing dynamic metadata must stay unknown/unadvertised unless the user supplies an explicit override. Do not infer image/file/tool/reasoning support from provider brand or model-name substring.
- The probe cache should live outside the user-authored config, under the application state/cache directory (for example XDG state/cache on Unix and platform-specific app data on Windows), so refresh churn does not rewrite hand-maintained config.
- The probe cache should store normalized rows, not raw provider JSON as the only source of truth. Minimum cache shape:
  - `provider_id`, `source_url`, `probed_at`, `expires_at` or `stale_after`;
  - upstream `model_id`, display name/description where available;
  - `context_window`, `max_output_tokens`, input/output modalities, supported parameters/features, reasoning levels/defaults;
  - `raw_ref` or optional raw payload for debugging, redacted of credentials.
- `connect` may write selected probe-derived models into config with `metadata_source = DynamicProbe { ... }`; alternatively it may keep model entries generated from cache as llm-proxy-owned sections. In both cases, launch commands must have deterministic local metadata and must not depend on live probing.

### 10.5 Runtime requirements

The runtime must cover these behavior groups:

1. **Protocol conversion**
   - OpenAI Chat ↔ Anthropic Messages.
   - OpenAI Chat ↔ OpenAI Responses.
   - Anthropic Messages ↔ OpenAI Responses.
   - Responses/Anthropic clients served from the Antigravity backend via derived endpoints (`derive_from = "antigravity"`).
   - Non-streaming and streaming variants.
2. **Tool use and multi-turn state**
   - function/tool declarations;
   - tool call output/result mapping;
   - tool call IDs;
   - assistant tool call history;
   - empty assistant message handling;
   - normalized tool argument/input object handling.
3. **Multimodal and document input**
   - image URL and base64/data URI inputs;
   - document/PDF/file blocks across OpenAI, Responses, and Anthropic forms;
   - capability gating follows the trust contract in §5: requests containing image/document input are only rejected locally when the model does not declare the matching `image_input`/`document_input` feature; declarations are trusted, never probed;
   - no image generation workflow.
4. **Thinking/reasoning adaptation**
   - client `reasoning_effort` / Anthropic thinking budget mapping;
   - provider-specific fields such as DeepSeek/Qwen/Stepfun/Gemini thinking formats, selected per endpoint via `compat.thinking_format`;
   - per-level API value mapping via model `reasoning_level_map` (with optional provider-level default);
   - level support declared per model via `supported_reasoning_levels`; a client-sent level outside the declared set is rejected locally with a client-protocol-shaped 4xx naming the model's supported levels — never silently clamped, never substituted with the default level. When `supported_reasoning_levels` is not declared, no gating applies. A level whose map entry has `api_value = null` (thinking disabled at that level) is rejected the same way. These rejections are deterministic client/config errors: no cooldown, no fingerprint accounting;
   - optional stripping or preserving of reasoning content according to provider compatibility;
   - reasoning content cache behavior needed for multi-turn tool workflows.
5. **Fallback and cooldown**
   - ordered provider attempts per model/protocol;
   - retry policy by failure category;
   - cooldown persistence;
   - `Retry-After` handling for rate limits;
   - auth errors are actionable and should not poison provider cooldown incorrectly;
   - stream replay boundary: the safe-replay window lasts until the proxy writes the **first byte downstream to the client**. Before that point — upstream error status, network error, or timeout — retry/fallback/cooldown apply exactly as for non-streaming, uniformly across all ingress protocols. After the first downstream byte, no retry/fallback/cooldown: the error is passed through and the client decides whether to retry;
   - timeouts: `timeout_seconds` (default 300s, 5 分钟) bounds connection establishment; `max_timeout_seconds` (default 600s) bounds non-streaming requests overall; streaming requests have no overall timeout — long streaming sessions are legitimate;
   - stream interruption counting: a mid-stream upstream failure after the first downstream byte never retries/falls back, but repeated interruptions are a provider health signal. Count an interruption only when all of: (a) at least one byte was written downstream, (b) the upstream connection failed, (c) the downstream client context is still alive (client-initiated disconnects never count). Threshold: 3 interruptions per cooldown key (model:provider:protocol) in a sliding 10-minute window triggers a short network-category cooldown (default 30s), making false positives cheap and self-healing;
   - interruption error classification (Layer A): only count interruptions whose error indicates an upstream-side cause — HTTP/2 `RST_STREAM`/`GOAWAY`, upstream TLS close, TCP RST from the upstream peer. Errors indicating local causes (interface down, `ENETUNREACH`, DNS failure) never count. Indeterminate errors (read timeout, unexpected EOF) count but fall into the cheap 30s cooldown;
   - future work, not first-version scope: cross-connection correlation (if streams to multiple unrelated providers die simultaneously, suspect the local network and exempt everyone) and post-incident reachability probes (TCP connect to the upstream before counting). Both narrow the residual false-positive window further but add a global connection-state subsystem for near-zero benefit, since false positives already cost only a self-healing 30s. **Prerequisite before either is scheduled**: a complete failure-category design that also covers billing/quota-exhaustion errors (402/403/quota responses — neither rate-limit nor auth failure; cooldown duration, fallback semantics, and user-facing messaging all need dedicated definitions), so these mechanisms are designed against a finished error taxonomy rather than retrofitted onto it.
6. **Bad Request Block / upstream protection**
   - request fingerprinting after repeated client-error failures;
   - the fingerprint is **shape-based**: it hashes the request's structural skeleton (model, message role sequence, tool definitions, parameter shapes) while excluding prompt text and tool argument contents. Retry loops that vary arguments but repeat the same failing shape share one fingerprint; a whole-body hash would defeat the mechanism, since every varied retry would look like a new request and never reach the threshold;
   - counting: a fingerprint's count is incremented only when every provider candidate for that request has failed and at least one candidate returned a client error (§10.5.8). If any candidate succeeds, nothing is counted;
   - at `max_errors` within `window_seconds`, matching requests are rejected locally with 429 for `block_seconds` (defaults in §3);
   - avoid global blocking when a later fallback provider succeeds;
   - preceded by the Layer 0 ingress/egress validation layers (§5.1): structurally invalid client requests and proxy conversion bugs are rejected locally before any fingerprinting or cooldown accounting.
7. **OAuth providers**
   - OpenAI Device Code / ChatGPT subscription token lifecycle;
   - Antigravity Google OAuth lifecycle;
   - token refresh on 401 where supported;
   - auth status shown in `status` and managed through the `provider` command/TUI (`provider login/logout/relogin/refresh/info`).
8. **Error mapping**
   - preserve client-protocol-shaped error responses;
   - map upstream auth/rate-limit/server/network/model-unavailable errors consistently;
   - prefer actionable auth/payment errors when all providers fail.
   - failure classification, with the governing principle that structurally valid requests are always forwarded and failures are classified after the upstream responds:

     | Upstream signal | Category | Retry same provider | Fallback | Cooldown |
     |---|---|---|---|---|
     | network error / 408 | `network` | yes | yes | `network_seconds` (default 30s) |
     | 5xx | `server_error` | yes | yes | `server_error_seconds` (default 300s) |
     | 429 | `rate_limit` | no | yes | `Retry-After` if present (capped 24h), else `rate_limit_seconds` (default 300s) |
     | 404 | `model_unavailable` | no | yes | `model_unavailable_seconds` (default 1800s) |
     | 400 (and other 4xx) | `client_error` | no | yes | `client_error_seconds` (default 300s); also feeds the fingerprint block only if every candidate fails |
     | 401 / 403 / 402 | `auth` | no | yes | **none** — token invalid, permission denied, and quota/billing exhaustion are account-state problems: the request was structurally valid, so it is forwarded, the failure is surfaced as an actionable error (re-auth, top up, check plan), and the provider must not be cooldown-poisoned for it |
     | stream interrupted after first downstream byte | `stream_interrupted` | no | no | none (except the repeated-interruption counter in §10.5.5) |

     This taxonomy is expected to be refined later — in particular finer-grained handling for quota/billing exhaustion (402 and provider-specific quota signals, e.g. distinct cooldown semantics or dedicated user messaging) — but only after provider product behavior research confirms how each upstream actually reports quota/billing/auth failures. Until that research lands, no provider-specific error-category assumptions should be added.
9. **Status/probe cache**
   - cached provider availability by provider/protocol/model where needed;
   - no accidental paid probe on default status;
   - explicit refresh command for real upstream probes.
10. **Provider request frequency limiting / anti-ban protection**
   - cooldown and fingerprint blocking protect upstreams after failures, but correct requests can also overload or rate-limit a model-service product. llm-proxy therefore supports provider-scoped proactive request frequency limits in addition to reactive cooldowns.
   - Frequency limits live at the llm-proxy **provider** level because one provider represents a concrete model-service product/account/region in the new config model. All endpoints declared under that provider (`openai_chat`, `openai_responses`, `anthropic`, derived endpoints, etc.) share one frequency pool. This matches how product/account quota and anti-abuse systems are usually enforced upstream.
   - MVP fields are exposed as optional advanced provider config. This is a visible but non-required escape hatch: users can override it when they know their product quota, but normal generated configs should omit it unless they need a product-specific override. If omitted, runtime applies conservative built-in defaults. Provider/platform research may later tighten or relax built-in defaults for specific products; those research-derived defaults should be encoded in catalog/connect generation rather than forcing every user config to spell out the block.

     ```toml
     [providers.deepseek.request_frequency]
     requests_per_minute = 60   # optional; default 60
     requests_per_hour = 1000  # optional; omit when provider research has no hourly quota evidence
     burst = 5                 # optional; default 5; conservative token-bucket capacity
     queue_timeout_seconds = 10 # optional; default 10
     ```

   - Defaults are intentionally conservative and local-agent friendly:
     - `requests_per_minute = 60` per provider frequency pool;
     - `requests_per_hour` is optional and unset by default unless provider/platform research documents an hourly quota or anti-abuse window;
     - `burst = 5` per provider frequency pool by default; this field is optional and should be tuned only when provider/platform research or real usage shows a different burst allowance is safer;
     - `queue_timeout_seconds = 10`.
   - Reasoning: coding agents usually issue low sustained request rates but may burst during retries/tool loops. `60 rpm` allows normal interactive use while making accidental loops self-throttle before they look like abuse. `burst` is exposed as an optional advanced knob but defaults conservatively to `5` because provider products usually document sustained request windows rather than burst capacity. Some products publish or enforce longer rolling windows (for example hourly request quotas), so `requests_per_hour` exists as an optional second bucket driven by provider research. These defaults are not provider entitlement claims; users with known product quotas can raise/lower them.
   - Frequency behavior:
     - the limiter is checked before sending any upstream request and before each retry/fallback attempt;
     - native and derived endpoints of the same provider consume the same provider-level frequency pool;
     - `burst` is token-bucket capacity, not requests-per-second and not concurrency; it controls how many saved request tokens can be consumed immediately;
     - when both minute and hour limits are configured, an attempt must acquire capacity from both buckets before it can hit upstream;
     - if the selected provider frequency pool has no capacity, the attempt waits up to `queue_timeout_seconds`;
     - if the wait times out for one provider, the proxy may try a later fallback provider whose own frequency pool has capacity;
     - if every candidate provider is locally frequency-limited, return a local 429 in the client protocol shape and do not hit upstream;
     - waiting or failing due to local frequency limit does not count as provider failure, cooldown, or bad-request fingerprint;
     - `Retry-After` from upstream 429 still creates provider cooldown as defined above;
     - streaming requests consume one frequency token at stream start; they do not consume additional frequency tokens for each SSE chunk;
     - status/probe refreshes share the same provider frequency pool unless an explicit admin override is added later.
   - Longer windows and non-default burst values should be added only when provider/platform research justifies them. MVP supports minute-level limits, optional hour-level limits, and optional provider-level `burst`; day/month budget caps, token-based limits, concurrency limiting (`max_in_flight`), per-model overrides, and adaptive limits learned from observed `Retry-After` are future work. Per-endpoint frequency overrides should be avoided unless provider research proves that endpoints under one product/account have independent upstream quota pools.
11. **Launch config generation**
   - Codex CLI, Codex Desktop, pi, Claude Code, Claude Desktop / Claude Code Desktop, and Qwen Code.
   - Preserve user-owned config fields and only replace llm-proxy-managed regions.
   - Client-specific generated capability fields are projections from the core model metadata. Example: Claude Desktop `inferenceModels[].supports1m` is generated only from resolved model `context_window >= 1_000_000`; llm-proxy must not add a duplicate `supports_1m` config field or encode capability in a `[1M]` model-name suffix.

### 10.6 Provider research evidence

Provider/product defaults in this document are design placeholders unless backed by the current-product review in 内部 provider catalog 调研.

### 10.5 分层架构（Layered Architecture）

```
┌─────────────────────────────────────────────────────┐
│                    接入层 (Thin)                     │
│                                                     │
│   CLI (短命令)  │  TUI (交互)  │  未来外部应用       │
│                                                     │
│   职责：                                              │
│   - 解析用户输入                                      │
│   - 格式化输出                                        │
│   - 交互确认（y/N）                                   │
│   - 检测 Server 并选择执行路径                         │
├─────────────────────────────────────────────────────┤
│                  Core 层 (业务逻辑)                   │
│                                                     │
│   ┌─────────────────────────────────────────────┐   │
│   │  CoreState                                   │   │
│   │                                              │   │
│   │  - Config (providers, models, server...)     │   │
│   │  - UsageStore (内存缓存 + 持久化控制)          │   │
│   │  - OAuth state                               │   │
│   │  - Cooldown state                            │   │
│   │                                              │   │
│   │  方法：                                       │   │
│   │  - add_provider() / remove_provider()        │   │
│   │  - add_model() / remove_model()              │   │
│   │  - record_usage() / query_usage()            │   │
│   │  - save_config() / save_usage()              │   │
│   │                                              │   │
│   │  原则：                                       │   │
│   │  - 纯内存操作 + 受控持久化                     │   │
│   │  - 不碰 stdin/stdout                          │   │
│   │  - 不监听网络端口                              │   │
│   └─────────────────────────────────────────────┘   │
├─────────────────────────────────────────────────────┤
│                  持久化层 (Storage)                   │
│                                                     │
│   config.toml  │  usage.jsonl  │  usage.db          │
│   oauth_accounts.json │  llm-proxy.pid  │  ownership.lock  │
│                                                     │
│   只有 Core 层有权写入                                │
└─────────────────────────────────────────────────────┘
```

#### 10.5.1 各层职责

| 层 | 做什么 | 不做什么 |
|----|--------|---------|
| 接入层 (CLI/TUI) | 解析参数、格式化输出、交互确认、检测 Server | 不直接读写 config 文件、不持有业务状态 |
| Core 层 | 业务逻辑、状态管理、持久化控制 | 不做用户交互、不监听网络 |
| 持久化层 | 存储数据 | 不包含业务逻辑 |

#### 10.5.2 核心类型定义

```rust
/// 统一命令模型——接入层构造命令，Core 层执行
/// 注意：AdminCommand 只覆盖 server 需要远程委托的子集。
/// ProviderAdd/ModelAdd 等操作直接作为 CoreState 方法调用，不通过 AdminCommand 分发。
pub enum AdminCommand {
    // Provider 管理（server 委托）
    ProviderRemove { id: String, force: bool },

    // Usage 查询
    UsageQuery {
        period: Option<String>,
        provider: Option<String>,
        model: Option<String>,
        endpoint: Option<String>,
        view: UsageView,
    },

    // 状态查询
    Status,
    ConfigReload,
}

pub enum AdminResponse {
    Ok { message: String },
    UsageRecords(Vec<UsageRecord>),
    StatusInfo(StatusInfo),
}
    ClientConfigPayload(ClientConfigPayload),
    Error { code: ErrorCode, message: String },
}

/// Core 状态——持有所有业务数据
pub struct CoreState {
    config: Config,
    usage_store: UsageStore,
    config_path: PathBuf,
    state_dir: PathBuf,
}

impl CoreState {
    /// 从文件加载
    pub fn load(config_path: &Path) -> Result<Self> { ... }

    /// 执行命令并持久化
    pub fn apply(&mut self, cmd: AdminCommand) -> Result<AdminResponse> {
        let resp = self.execute(cmd)?;
        self.save()?;  // 统一落盘
        Ok(resp)
    }

    /// 纯业务逻辑（不触发保存，供 server 精细控制）
    fn execute(&mut self, cmd: AdminCommand) -> Result<AdminResponse> { ... }

    /// 持久化所有状态
    pub fn save(&self) -> Result<()> { ... }
}
```

---


## 11. CLI and Service Lifecycle Design

### 11.1 Command surface

Target command surface:

```text
llm-proxy [--config PATH]                 # start service in background by default
llm-proxy serve [--foreground]            # explicit service command; background unless --foreground
llm-proxy serve --foreground              # run in foreground
llm-proxy init [--config PATH]            # create fresh llm-proxy config; never migrates old schemas
llm-proxy connect [PRODUCT] [flags]       # user-friendly mature-product setup; always verifies before writing
llm-proxy status [--probe]
llm-proxy provider                         # provider management TUI when interactive
llm-proxy provider list                    # list providers
llm-proxy provider info [NAME]             # provider details with usage when supported
llm-proxy provider add [PRODUCT] [flags]   # create provider (interactive or non-interactive)
llm-proxy provider copy SOURCE NAME [flags] # copy provider config into a new provider ID
llm-proxy provider login NAME              # OAuth login
llm-proxy provider logout NAME             # OAuth logout
llm-proxy provider relogin NAME            # OAuth relogin
llm-proxy provider refresh NAME            # refresh token / usage cache as applicable
llm-proxy provider remove NAME [--force]   # delete provider
llm-proxy provider reset-usage NAME [--force] # reset usage (OAuth providers)
llm-proxy model                            # model management TUI when interactive
llm-proxy model list                       # list models
llm-proxy model info MODEL                 # model details
llm-proxy model add MODEL [flags]          # create frontend model
llm-proxy model remove MODEL [--force]     # delete frontend model
llm-proxy model set MODEL [flags]          # edit context/output/thinking/features atomically
llm-proxy model provider add MODEL --type TYPE --provider NAME [--upstream-model MODEL]
llm-proxy model provider remove MODEL --type TYPE --provider NAME
llm-proxy model provider move MODEL --type TYPE --provider NAME --to INDEX
llm-proxy launch codex [flags]
llm-proxy launch codex-desktop [flags]
llm-proxy launch pi [flags]
llm-proxy launch claude-code <model-id> [slot flags]
llm-proxy launch claude-desktop [flags]
llm-proxy launch qwen-code [model-id] [flags]
llm-proxy cooldown list
llm-proxy cooldown clear [--model MODEL] [--provider PROVIDER]
llm-proxy shutdown
llm-proxy restart
llm-proxy doc [--list|--raw|--section SECTION]
llm-proxy version
llm-proxy completion <shell>
```


Command entry distinction:

- `llm-proxy connect` enters the add-provider wizard (`provider add`) in interactive terminals.
- `llm-proxy provider` enters the full provider management TUI.
- `llm-proxy model` enters the full model management/editor TUI.

### 11.2 Service and status semantics

- The default command starts the service in the background; foreground execution requires `serve --foreground` (backward-compatible alias: `serve --frontend`).
- `status` reads cache/local state by default and never performs network probes; real upstream refresh is `status --probe`.

### 11.3 `init` default config

`llm-proxy init` creates a minimal DeepSeek-only llm-proxy config and never touches an existing config file. It does not migrate legacy schemas and does not start the service. Other providers are added through `connect` / `provider add`. `--config PATH` selects the target config path; if omitted, the normal config path is used. Missing provider environment variables do not block initialization — the command writes the config and prints the required exports.

DeepSeek natively serves Chat Completions and Anthropic Messages (officially documented), and does not serve Responses. The generated provider therefore declares two native endpoints and one derived:

```toml
[server]
listen = "127.0.0.1:8989"

[providers.deepseek]
api_key_env = "DEEPSEEK_API_KEY"

[providers.deepseek.openai_chat]
url = "https://api.deepseek.com/chat/completions"

[providers.deepseek.openai_chat.compat]
supports_developer_role = false
supports_reasoning_effort = true
thinking_format = "deepseek"
requires_reasoning_content_on_assistant_messages = true
max_tokens_field = "max_tokens"

[providers.deepseek.anthropic]
url = "https://api.deepseek.com/anthropic/v1/messages"

[providers.deepseek.openai_responses]
derive_from = "openai_chat"
```

Note: `anthropic` is **native** — DeepSeek officially provides an Anthropic Messages endpoint, so it must not be derived from chat. `openai_responses` is derived because DeepSeek has no Responses API. Native endpoints carry their own `compat` where needed; the Anthropic endpoint uses defaults.

Generated models (mirroring the catalog defaults): `deepseek-v4-flash-lp` and `deepseek-v4-pro-lp`, each bound to the `deepseek` provider for all three client protocols, with model metadata (context window 1M, max output 384k, `tool_call_reasoning` feature, `enable_thinking = true`, per-model `supported_reasoning_levels` / `default_reasoning_level` / `reasoning_level_map`).

The design vocabulary uses **frontend** for foreground execution.

### 11.4 Background service requirements

The service model is deliberately simple: **one llm-proxy service instance per user environment**. There is no per-config instance derivation — state paths are global, and a second start while a service is alive is refused (use `restart` to switch configs). Test isolation is achieved through environment override or containers, not through the service model.

Background mode must:

- validate config before daemonizing/spawning;
- refuse to start if an alive service already exists (detected via the pid file / management socket in the state dir);
- write state files with fixed names in the state dir:
  - `~/.local/state/llm-proxy/llm-proxy.pid`
  - `~/.local/state/llm-proxy/llm-proxy.sock`
  - `~/.local/state/llm-proxy/llm-proxy.log`
  - state dir resolution: `$XDG_STATE_HOME/llm-proxy` when set, otherwise the platform default (`~/.local/state/llm-proxy` on Unix, `%LOCALAPPDATA%/llm-proxy` on Windows);
- expose a local management channel (the socket above) for shutdown/restart/env injection **and read-only state queries** (e.g. bad-request-block state for `status`, §11.6);
- print concise connection information and shutdown instructions;
- `shutdown`/`status`/`restart` target the single service in the resolved state dir; `--config` selects which config file the service runs with, but does not create separate instances.

Test isolation:

- `XDG_STATE_HOME=/tmp/xyz` redirects the entire state tree (pid/sock/log/cooldown/probe-cache) for test scripts — the primary per-test isolation mechanism;
- container-based isolation (running the service inside a container) is the recommended approach for heavier integration testing.

`serve --foreground` must:

- run in the current terminal;
- log to stderr/stdout according to normal CLI conventions;
- handle Ctrl-C gracefully;
- follow the same single-instance rules as background mode: refuse to start when a service is already alive, and create the management socket (§11.8) so `shutdown`/`status` state queries work uniformly against foreground and background services.

### 11.5 Shutdown and restart

`shutdown`:

- sends a local management request to the service in the resolved state dir;
- stops accepting new requests;
- waits for in-flight requests for a bounded period;
- cleans up pid/socket state.

`restart`:

- shuts down the currently running service;
- starts a new background service using the current CLI/config arguments;
- does not inherit stale arguments from the old process.

### 11.6 Status command

Default `status` must be safe and cache-first:

- load and validate config;
- show service pid/socket state;
- show configured providers/products/models;
- show API key/env/OAuth credential presence without printing secrets;
- show active cooldown and bad-request-block state;
- show cached probe results when available;
- never send real LLM requests by default.

State visibility follows where the state lives:

- **Persisted state** (cooldowns, probe cache) is read from disk directly.
- **In-process state** (bad-request-block entries and counters) is queried from the running proxy process through the management channel (§11.4): `status` sends a read-only state request to the local management socket and renders the result. When no service is running, `status` shows the block section as unavailable ("service not running") instead of failing or silently omitting it. In-process state is never written to disk just for observability.

`status --probe` must:

- perform real upstream probes only for configured/enabled providers;
- update probe cache;
- use cheap/minimal model probes where possible;
- show refresh time and failures clearly;
- avoid probing disabled or credential-missing providers unless explicitly forced by a later flag;
- **probe concurrency is per-provider**: when probing multiple model bindings under one provider, those probe requests share the provider's `request_frequency` concurrency limit with actual business requests. If the provider is already at its concurrency limit from live traffic, probes queue behind or are deferred — probes never bypass or preempt the provider's rate control.

### 11.7 Core operator commands: `connect`, `provider`, and `model`

llm-proxy has three core operator commands around provider/model configuration. All three must support both CLI/script operation and interactive TUI operation:

| Command | Primary object | CLI role | TUI role | Writes user config by default? |
|---|---|---|---|---|
| `connect` | mature model-service product | alias for `provider add` | (delegates to `provider add`) | yes, after preview/confirmation |
| `provider` | provider product/account/region and endpoint graph | create/list/info/auth/delete/usage reset | provider management UI | yes for write operations; no for list/info |
| `model` | frontend model catalog and protocol/provider bindings | list/info/parameter edit/feature toggle/provider binding management | model management and editing UI | yes for write operations; no for list/info |

The commands share the same catalog, resolver, dynamic-probe cache, and validation code. Their boundaries are intentional:

- `connect` is a CLI alias for `provider add`, retained for script convenience and existing user habits.
- `provider` is the **single entry point** for provider management: creation, viewing, authentication (OAuth), deletion, and usage reset.
- `model` manages frontend models: viewing, parameter configuration, feature toggles, and protocol-specific provider bindings.

**Detailed design** (command matrix, flag semantics, TUI state machines, internal modules, and ChatGPT usage API) is in **§12**. llm-proxy uses subcommands for scriptable provider/model actions; bare `provider` and bare `model` are reserved for TUI entry in interactive terminals.

`provider`/`model` may hand off to each other for related operations. The TUI never displays API keys, bearer tokens, OAuth refresh tokens, or raw authorization headers.

### 11.8 Management channel

The management channel is a Unix domain socket at `$STATE_DIR/llm-proxy.sock`, created with owner-only permissions (`0600`), speaking plain HTTP over UDS. Windows uses TCP localhost as fallback. If socket creation fails, the service starts without the management channel and logs a warning.

**写操作仅通过管理通道**（安全设计：写端点从 TCP 公开接口迁到 UDS，0600 权限即信任）。

Endpoints:

| Endpoint | Method | Purpose |
|---|---|---|
| `/shutdown` | POST | Reply 200 immediately, then stop accepting new requests, wait for in-flight requests up to 30s, close the management channel, and shut down the main server. |
| `/env` | POST | Inject environment variables into the running service (used by `connect` after writing config so a running service picks up new API keys without restart). JSON body `{"env": {...}}`, max 1MB. Response lists the variables that were applied. |
| `/state` | GET | Read-only in-process state for `status`: bad-request-block entries and stream interruption counters. Never includes request bodies or secrets. |
| `/admin/provider/add` | POST | 添加 provider（写入 config.toml） |
| `/admin/provider/remove` | POST | 删除 provider |
| `/admin/provider/copy` | POST | 复制 provider |
| `/admin/model/add` | POST | 添加 model |
| `/admin/model/set` | POST | 修改 model 参数 |
| `/admin/model/remove` | POST | 删除 model |
| `/admin/model/provider` | POST | 管理 model 的 provider binding（add/remove/move） |
| `/admin/config/reload` | POST | 热重载配置 |
| `/admin/config/update` | POST | 更新配置（原子写入） |
| `/admin/cooldown/clear` | POST | 清除冷却状态 |
| `/admin/oauth/write` | POST | 写入 OAuth 凭据 |

Stale socket files from abnormal exits are removed on startup before binding.

### 11.4A 全场景矩阵（Deployment Scenario Matrix）

#### 11.4A.1 远程模式场景（待实现，只读）

| # | 时序 | CLI 行为 | 并发控制 | 冲突风险 |
|---|------|---------|---------|---------|
| R1 | Server 已启动 → CLI 启动 | 检测 server.listen → 连 Server → 只读查询（status/usage） | Server Mutex | 无 |
| R2 | Server 未启动 → CLI 启动 | 连不上 → 报错退出 | — | 无 |
| R3 | CLI 查询中 → Server 宕机 | 连接错误 → 报错，用户重试 | — | 无 |
| R4 | CLI-1 运行中 → CLI-2 启动 | 两个都连远程 Server → Server 串行 | Server Mutex | 无 |
| R5 | Server 恢复 → CLI 重试 | 重连成功 → 正常查询 | — | 无 |

> 远程模式只支持读操作（status/usage 查询），写操作仅本机 UDS（2026-08-09 决策）。

#### 11.4A.2 本地模式场景

| # | 时序 | 检测方式 | CLI 行为 | 并发控制 |
|---|------|---------|---------|---------|
| L1 | Server 已启动 → CLI 启动 | PID + 进程 + ping OK | 委托 Server | Server 内部锁 |
| L2 | Server 未启动 → CLI 启动 | 无 PID file | 独立模式 | 文件锁 |
| L3 | Server 崩溃 → CLI 启动 | PID 存在但进程死 | 清理 stale PID → 独立模式 | 文件锁 |
| L4 | CLI 独立模式运行中 → Server 启动 | CLI 持有文件锁 | CLI 完成 → 释放锁 → Server 加载最新 | 文件锁保证不冲突 |
| L5 | CLI-1 独立模式 → CLI-2 启动 | CLI-1 持有文件锁 | CLI-2 等锁 → CLI-1 释放后执行 | 文件锁串行化 |
| L6 | Server 运行中 → CLI-1 → CLI-2 | 两个都检测到 Server | 两个都委托 Server | Server 内部锁 |
| L7 | CLI 写盘中 → Server 恰好启动 | CLI 持有文件锁 | Server 启动时等锁 → CLI 写完释放 → Server 加载 | 文件锁保护 |
| L8 | TUI 运行中 → Server 被其他终端启动 | TUI 启动时选了独立模式 | TUI 继续独立模式（不中途切换） | 文件锁保护每次写操作 |

#### 11.4A.3 微妙场景详解

**L4：CLI 写盘 → Server 启动**
```
CLI:    [lock] [read] [modify] [write] [unlock]
                                        ↑
Server:                                 [start] [load config] [serve]

结果：Server 读到 CLI 写入的最新配置 ✅
```

**L7：CLI 和 Server 同时启动**
```
时间线 A（CLI 先拿到锁）：
  CLI:    [lock ✓] [read] [write] [unlock]
  Server:          [等锁...]           [lock ✓] [read] [serve]
  结果：Server 加载 CLI 的修改 ✅

时间线 B（Server 先拿到锁）：
  Server: [lock ✓] [read] [init] [unlock]
  CLI:             [等锁...]    [lock ✓] [read] [write] [unlock]
  结果：CLI 基于 Server 初始化后的状态修改 ✅
  注意：此时 Server 内存不含 CLI 的修改
  → CLI 应检测到 Server 已启动（等锁期间重新检测 PID）
  → 如果发现 Server 已启动，放弃独立模式，改为委托
```

**L8：TUI 不中途切换模式**
```
原因：
1. 状态一致性复杂（TUI 内存已有状态，切换要同步）
2. 文件锁已保证不冲突
3. 简单原则：启动时决定模式，运行期间不变
```

---


## 12. Provider and Model Management Design

llm-proxy provides unified management commands for providers and models, replacing the legacy `auth` command. These commands are intentionally configuration editors: `provider` edits provider-related config and manages provider authentication state; `model` edits model-related config and provider bindings. `connect` is retained as a CLI alias for `provider add`.

### 12.1 Command Responsibility

| Command | Responsibility | Notes |
|---------|---------------|-------|
| `provider` | **Edit provider config and auth state** | Create/view/delete provider config, OAuth login/logout/refresh/relogin, usage reset |
| `connect` | Alias for `provider add` | CLI shortcut for adding provider config, flags pass through unchanged |
| `model` | **Edit model config** | Create/copy/delete models, edit parameters/features, manage protocol-specific provider bindings |
| ~~`auth`~~ | ~~Deleted~~ | All functionality covered by `provider` |

**Why delete `auth`:**

| Original `auth` functionality | Now belongs to |
|------------------------------|----------------|
| `auth --login` (create new OAuth provider) | `provider add` / `connect` (alias) |
| `auth --login --provider X` | `provider login X` |
| `auth --logout` | `provider logout NAME` |
| `auth --relogin` | `provider relogin NAME` |
| `auth --refresh` | `provider refresh NAME` |
| `auth --info` | `provider info [NAME]` |
| `auth --delete` | `provider remove NAME` |
| `auth --reset-usage` | `provider reset-usage NAME` |
| `auth --list` | `provider list` |
| `auth` (TUI) | `provider` (TUI) |

### 12.2 `provider add` / `connect` — Create Provider

`provider add` is the **single provider-management entry point** for creating all provider types. `connect` is the user-friendly alias for the same creation path and is primarily aimed at mature catalog products; custom/advanced endpoint creation should be documented under `provider add`.

#### Command forms

```bash
# Interactive (no PRODUCT or missing required flags)
provider add
# → Product selection → Configuration → Write

# Non-interactive: API key product
provider add deepseek --api-key-env DEEPSEEK_API_KEY --model deepseek-v4-pro-lp

# Non-interactive: OAuth product (initiates login flow)
provider add openai-subscription --model gpt-5.5-lp

# Non-interactive: Custom endpoint
provider add my-server --type openai-chat \
           --endpoint-url http://localhost:8080/v1/chat/completions --api-key-env MY_KEY

# Non-interactive: No key (e.g., ollama)
provider add ollama --no-api-key

# Equivalent connect syntax (alias)
connect deepseek --api-key-env DEEPSEEK_API_KEY --model deepseek-v4-pro-lp
connect openai-subscription --model gpt-5.5-lp
```

#### Creation flow

**API key product:**

```
provider add deepseek --api-key-env DEEPSEEK_API_KEY --model deepseek-v4-pro-lp
→ Validate catalog product + credential reference → Select/confirm model(s) → Run real connectivity probe → Write provider config → Create selected model binding(s) → Done
```

**OAuth product:**

```
provider add openai-subscription --model gpt-5.5-lp
→ Start OAuth login flow (Device Code / Browser)
→ Login success → Select/confirm model(s) → Run real connectivity probe → Write provider config + auth state → Create selected model binding(s) → Done
```

**Interactive (no PRODUCT):**

```
provider add

Select product:
  1. OpenAI Subscription (OAuth)
  2. Google Antigravity (OAuth)
  3. DeepSeek API (API Key)
  4. Custom provider...
> 1

Provider name [openai-subscription]: ↵
→ Follow corresponding product creation flow
```

All writes are **atomic**: validate all inputs first, write to config.toml in one operation; on failure, write nothing.

### 12.3 `provider` — Manage Provider

#### Entry behavior

| Usage | Behavior | Mode |
|-------|----------|------|
| `provider` | Enter TUI list interface when stdin/stdout are interactive; usage error otherwise | TUI |
| `provider NAME` is invalid | Provider actions use explicit subcommands | CLI |
| `provider list` | CLI text list of all providers | CLI |
| `provider info [NAME]` | CLI text detail or summary | CLI |
| `provider add [PRODUCT]` | Create provider (interactive or non-interactive) | CLI/TUI prompts |
| `provider copy SOURCE NAME` | Copy provider config into a new provider ID | CLI/TUI prompts |
| `provider login/logout/relogin/refresh/remove/reset-usage NAME` | Execute corresponding action | CLI |

Pattern:
- **Bare `provider`** → TUI only in interactive terminals; non-interactive invocation returns a usage error.
- **Subcommand present** → CLI execution.
- Action subcommands are mutually exclusive by construction; do not reintroduce flag-driven action dispatch.

#### Complete command matrix

```bash
# Entry
provider                                        → TUI list
provider --select X                             → TUI detail (interactive only convenience)
provider list                                   → CLI text list

# Create/copy
provider add [PRODUCT] [--api-key-env Y|--no-api-key] [--type TYPE --endpoint-url URL]
provider copy SOURCE NAME [--api-key-env Y|--no-api-key]  # OAuth copies always re-login
connect [PRODUCT] [--api-key-env Y|--no-api-key] [--model MODEL...]  Mature catalog-product setup path

# View
provider info [NAME]                            → CLI text detail/summary with usage when supported

# Auth (OAuth provider)
provider login NAME                             → Login
provider logout NAME                            → Logout (clear token)
provider relogin NAME                           → Logout + login
provider refresh NAME                           → Refresh token / cached usage where applicable

# Delete
provider remove NAME                            → Delete provider if unreferenced (secondary confirmation)
provider remove NAME --force                    → Delete unreferenced provider (skip confirmation)

# Actions
provider reset-usage NAME                       → Reset usage (tiered confirmation)
provider reset-usage NAME --force               → Reset usage (skip confirmation)
```

#### Flag and argument semantics

| Command | Argument/flag | Description |
|------|------|-------------|
| `provider add [PRODUCT]` | `PRODUCT` | Catalog product ID or custom provider ID. If omitted, interactive product picker is shown. |
| `provider add` | `--api-key-env ENV` | Store the environment variable name containing the upstream API key; never store the key value. |
| `provider add` | `--no-api-key` | Declare provider as not requiring upstream authentication. |
| `provider add` | `--type TYPE` | Required for custom endpoints. Valid values: `openai-chat`, `openai-responses`, `anthropic`. `antigravity` is not accepted for custom client-facing providers. |
| `provider add` | `--endpoint-url URL` | Complete upstream endpoint URL, not a base URL. Required with `--type`. Built-in provider endpoint URLs are plain upstream API URLs; they do not contain tokens and do not rely on query parameters for auth/routing. |
| `provider add` / `connect` | repeated `--model MODEL_ID` | For mature catalog products, explicitly select catalog models to bind during setup and verification. May be passed multiple times. Non-interactive mature-product add/connect requires at least one model; interactive mode prompts for model selection. Custom provider add does not accept immediate model binding. |
| `provider copy SOURCE NAME` | `SOURCE` | Existing provider ID to copy from. |
| `provider copy SOURCE NAME` | `NAME` | New provider ID. Must be unused. |
| actions | `NAME` | Provider ID to act on. |
| destructive actions | `--force` | Skip confirmation for `remove` and `reset-usage`. |


Connectivity verification is part of the normal `connect` / mature-product `provider add` contract, not a passive default that may be skipped silently. After the user selects the API key environment variable or completes OAuth login and selects model(s), the wizard sends a real upstream probe using the resolved endpoint/auth/model. A failed probe leaves the config unchanged and returns the provider/model/error context needed for correction.

The only creation path that does not verify immediately is **custom provider configuration**: custom providers may be added as provider-only records with explicit `--type` / `--endpoint-url` / auth reference and no immediate model binding. Because no frontend model is selected at that point, there is no required model-specific probe. Users attach custom providers later through `model provider add` or the model TUI; verification can happen there or at first request.

Endpoint display rules for TUI/CLI:

- Built-in endpoint URLs are complete upstream request URLs (for example `/v1/chat/completions`, `/v1/responses`, `/v1/messages`), not base URLs.
- For currently supported built-ins, endpoint URLs do not carry API tokens and do not use query parameters for credential selection or routing, so the UI may display them as normal non-secret configuration.
- Secrets remain in environment variables or the unified OAuth store; the TUI must still never show API key values, OAuth access tokens, refresh tokens, or token-store contents.
- Custom endpoint URLs are user input and should be validated as URLs, but llm-proxy should not invent a token-in-query masking model for built-in providers.

Provider commands primarily edit provider configuration and provider auth state. They must not silently add, remove, or modify model provider bindings. The only allowed exception is the user-confirmed **mature product setup flow** in `provider add` / `connect`: catalog-backed products may show a model list before saving, then verify the selected provider/model combination, then atomically write provider config/auth state plus selected frontend model bindings. Those bindings are explicit selections in the add/connect flow, not hidden side effects. Custom providers and `provider copy/remove/auth` commands never create model bindings; users attach them later with `model provider add` or the model TUI. After any write, the full config is reloaded and validated before the atomic file replacement is committed.

Mature product model selection:

- Applies only to catalog-backed mature LLM service products added through `provider add` / `connect`.
- Runs before any config write. OAuth products must complete OAuth login first; API-key products must resolve the selected env var first. Then the selected model(s) are probed against the resolved endpoint/auth/model combination. The command cannot complete provider setup unless the probe succeeds.
- Interactive add/connect displays catalog-supported models/protocols for that product and requires the user to select at least one binding to verify and create.
- Non-interactive add/connect requires explicitly repeated `--model MODEL_ID` flags. Example: `connect deepseek --api-key-env DEEPSEEK_API_KEY --model deepseek-v4-pro-lp --model deepseek-v4-flash-lp`. Omitting `--model` is an error for mature products, not a provider-only setup mode.
- `--model` values are mature-product catalog template IDs, not arbitrary frontend IDs. The current Rust implementation should follow the Go/v1 naming style for these templates: the frontend model ID is a readable composite such as `gpt-5.5-openai-subscription-lp`, usually `<upstream-model-family>-<product/provider-family>-lp`. The selected template is copied into `[models.<template-id>]`; provider bindings inside the copied entry are rewritten to the actual provider ID chosen during `connect`/`provider add` when the user named the provider differently from the product default. A separate `display_name` may show the common upstream model name consistently across providers.
- Writes selected bindings into `[models.*].*_providers` only after explicit user selection (`--model` in non-interactive mode or checked rows in interactive mode) and successful verification.
- Provider-only setup without immediate verification is available only for custom provider configuration, not mature catalog products.
- Does not run for custom providers, provider copy, provider remove, or auth-only commands.

Example: selecting the `gpt-5.5-openai-subscription-lp` template while adding provider `openai-sub-work` creates a model entry with the same frontend ID and rewrites only the binding provider name:

```toml
[models.gpt-5.5-openai-subscription-lp]
display_name = "GPT 5.5"
context_window = 272000
max_output_tokens = 128000
openai_responses_providers = [
  { name = "openai-sub-work", model = "gpt-5.5" },
]
```

This mirrors the Go/v1 behavior: model template IDs are stable product-family IDs, while provider IDs inside bindings can be user-chosen account/provider names.


#### Provider copy semantics

`provider copy SOURCE NAME` is the provider-side equivalent of safely copying one `[providers.SOURCE]` TOML block to `[providers.NAME]` and then editing it with validation. It is useful for creating multiple accounts/regions/products with similar endpoint configuration while avoiding manual TOML mistakes.

Rules:

- `SOURCE` must exist and `NAME` must be unused.
- The copied provider keeps product/endpoint/compat/request-frequency configuration unless overridden by flags.
- API key providers must re-confirm the credential reference. In non-interactive CLI, `provider copy` for an API-key provider requires `--api-key-env ENV` unless the source provider is explicitly copied as `--no-api-key` where valid. In TUI, the user must explicitly enter or confirm `api_key_env` before save. The tool must not invent or suggest env var names from provider IDs; the environment variable naming is the user's decision. This prevents accidentally reusing the old account/key while keeping naming policy user-owned.
- No-key providers copy as configured.
- OAuth providers copy provider config only; access tokens, refresh tokens, account IDs, and usage cache are **never copied**. By default, copy uses fresh-account mode: the new provider gets a new `auth.account` defaulting to the new provider ID, and the user must complete a fresh login before the copied provider is considered configured. An explicit account-sharing option may be added later (for example `--auth-account EXISTING`); it would copy no tokens and only reference the existing credential record deliberately.
- Model bindings are not created or duplicated by `provider copy`; users attach the copied provider to models explicitly with `model provider add` or edit bindings in the model TUI.
- Copying is not rename: `SOURCE` remains valid until explicitly removed.

OAuth provider identity is represented at two layers:

1. **Provider ID** — the stable routing/config key, e.g. `openai-sub-personal`, `openai-sub-work`, `antigravity-main`. Model bindings reference provider IDs only.
2. **OAuth account key** — the credential identity from `auth.account`; when omitted, it defaults to the provider ID. The unified OAuth store is keyed by this account key.
3. **Authenticated account metadata** — display-only metadata learned after login, such as issuer/product, account email or subject, plan/workspace when available, expiration, and usage state.

This separation allows several OAuth providers for the same product to coexist, and also allows deliberate account sharing if an explicit future CLI/TUI flow enables it. If two providers authenticate as the same email/account but use different `auth.account` keys, they are still distinct credential records; `provider list/info` should show both and may warn that they appear to share the same upstream account.


#### `list` output

```
$ provider list

openai-subscription    OpenAI Subscription (Device Code)    user@a.com
deepseek               DeepSeek API                         api-key
antigravity            Google Antigravity OAuth              user@b.com
```

#### `info` output

**Single provider detail:**

```
$ provider info openai-subscription

provider:     openai-subscription
type:         openai-device-code
account:      user@a.com
status:       authenticated
expires:      2026-07-28 (7d remaining)

usage:
  plan:       Pro
  5-hour:     62%  reset in 2h 31m
  weekly:     30%  reset in 3d 12h
  reset credits: 2
```

**Pay-as-you-go product (no window, has balance):**

```
$ provider info openai-billing

provider:     openai-billing
type:         openai-device-code
account:      user@a.com
status:       authenticated
expires:      2026-07-28 (7d remaining)

usage:
  plan:       Usage-Based
  balance:    $12.34
```

**API key type (no usage):**

```
$ provider info deepseek

provider:     deepseek
type:         api-key
status:       configured
```

Does not display usage area, does not explain why.

**Without `--select` (list all providers' info summary):**

```
$ provider info

openai-subscription (user@a.com) — Pro
  5-hour:  62%  reset in 2h 31m
  weekly:  30%  reset in 3d 12h
  reset credits: 2

deepseek — api-key
  (no usage tracking)
```

When no OAuth provider:
```
$ provider info

No providers with usage tracking configured.
```

#### `remove` confirmation and reference safety

Default secondary confirmation:
```
$ provider remove openai-subscription

Remove provider "openai-subscription" (user@a.com)?
This will remove provider config and credentials. Model bindings are not changed. [y/N]
```

`--force` skips confirmation:
```
$ provider remove openai-subscription --force

✅ Provider "openai-subscription" removed.
```

Removal deletes `[providers.NAME]` and matching auth state only. It does not edit model bindings. If any model provider binding still references `NAME`, `provider remove NAME` fails before confirmation with an actionable list of references and tells the user to remove them through `model provider remove` or the model TUI first. This preserves the rule that provider commands do not silently edit model config and also prevents dangling invalid references.

#### `reset-usage` tiered confirmation

**Confirmation strategy:**

| Usage state | Condition | Behavior |
|------------|-----------|----------|
| Used up | Any window `limit_reached: true` | Execute directly, no confirmation |
| Not used up | All windows `used_percent >= 50%` | Secondary confirmation |
| Usage sufficient | Any window `used_percent < 50%` | Secondary confirmation + three-color warning |
| `--force` | Any state | Skip all confirmations |

**Used up (execute directly):**

```
$ provider reset-usage openai-subscription

✅ Usage reset.
  5-hour:  0%
  weekly:  0%
  reset credits: 1
```

**Not used up (secondary confirmation):**

```
$ provider reset-usage openai-subscription

Consume 1 reset credit? (2 → 1) [y/N] y

✅ Usage reset.
  5-hour:  0%
  weekly:  0%
  reset credits: 1
```

**Usage sufficient (secondary confirmation + color warning):**

```
$ provider reset-usage openai-subscription

Consume 1 reset credit? (2 → 1) [y/N] y

[WARNING] Usage is low (5-hour: 20%, weekly: 35%).
          This reset credit will be wasted.
Confirm anyway? (yes/N)
```

`[WARNING]` line rendered in red/yellow highlight.

**`--force` (skip all confirmations):**

```
$ provider reset-usage openai-subscription --force

✅ Usage reset.
  5-hour:  0%
  weekly:  0%
  reset credits: 1
```

#### Unsupported provider handling

Principle: Do not leak implementation details, do not confuse users.

`info` for API key provider: Display provider info normally, do not show usage area, do not explain why.

`reset-usage` for unsupported provider:
```
$ provider reset-usage deepseek

Provider "deepseek" does not support usage reset.
```

### 12.4 Provider TUI Design

#### TUI Technology Stack

The current Rust implementation uses **ratatui + crossterm** as the TUI framework, following an Elm-inspired Model-Update-View architecture (the same pattern used by the Go v1 `bubbletea` library). This section specifies:

- how the TUI event loop is structured (§12.4.1);
- how screen states are encoded (§12.4.2);
- how input is handled (§12.4.3);
- how connect/`provider add` wizard flows map to the state machine (§12.4.4).

##### 12.4.1 Event Loop Architecture

The event loop runs in a single dedicated async task and follows a strict render→input→update→render cycle:

```rust
use ratatui::{Terminal, backend::CrosstermBackend};
use crossterm::event::{self, Event, KeyCode};

fn run_tui(state: &mut model::AppModel) -> Result<()> {
    let mut terminal = Terminal::new(CrosstermBackend::new(std::io::stdout()))?;
    crossterm::terminal::enable_raw_mode()?;
    crossterm::execute!(std::io::stdout(), EnterAlternateScreen)?;

    loop {
        terminal.draw(|f| view::render(f, state))?;
        if matches!(state.screen, Screen::Quit) {
            break;
        }
        if let Ok(Event::Key(key)) = event::read() {
            update::handle_key(state, key);
        }
    }

    crossterm::execute!(std::io::stdout(), LeaveAlternateScreen)?;
    crossterm::terminal::disable_raw_mode()?;
    Ok(())
}
```

Dependencies:

```toml
[dependencies]
ratatui = "0.29"
crossterm = "0.28"
tui-textarea = "0.7"     # optional: editable multi-line text
```

##### 12.4.2 State Machine — Screen Enum

The TUI state machine is encoded as a flat `Screen` enum. Each variant owns only the sub-state it requires:

```rust
pub enum Screen {
    // Provider management
    ProviderList(ProviderListState),
    ProviderDetail(ProviderDetailState),

    // Connect / provider add wizard
    ProductSelection(ProductSelectionState),
    EnvVarSelection(EnvVarSelectionState),
    ModelSelection(ModelSelectionState),
    CustomProviderName(CustomProviderNameState),
    CustomProviderEndpoint(CustomProviderEndpointState),
    OAuthLogin(OAuthLoginState),
    OAuthDeviceCode(OAuthDeviceCodeState),
    AntigravityLogin(AntigravityLoginState),

    // Confirmations
    WarningConfirm(WarningConfirmState),
    Verifying(VerifyingState),
    Done(DoneState),
    Quit,
}
```

State transitions follow the flow defined in §12.4, with one additional rule: **any state must respond to `Esc` by returning to its parent state, and to `q` by returning to the top-level list (or quitting from the list itself).**

##### 12.4.3 Input Handling

All key events are dispatched through a single `handle_key` function:

```rust
pub fn handle_key(state: &mut AppModel, key: KeyEvent) {
    match (&mut state.screen, key.code) {
        // Global: Esc → back, q → quit (or back depending on context)
        (_, KeyCode::Esc) => state.go_back(),
        (Screen::ProviderList(_), KeyCode::Char('q')) => state.screen = Screen::Quit,
        (_, KeyCode::Char('q')) => state.go_back(),

        // Navigation: ↑/↓/j/k — unified list cursor movement
        (Screen::ProductSelection(s), KeyCode::Up | KeyCode::Char('k')) => s.cursor_up(),
        (Screen::ProductSelection(s), KeyCode::Down | KeyCode::Char('j')) => s.cursor_down(),

        // Selection: Space — toggle multi-select item
        (Screen::ModelSelection(s), KeyCode::Char(' ')) => s.toggle_selected(),

        // Confirmation: Enter
        (Screen::ProductSelection(s), KeyCode::Enter) => state.confirm_product(s.selected()),
        // ... (other states similar)

        // Search: '/' — toggle filter mode on list screens
        (Screen::ProductSelection(s), KeyCode::Char('/')) => s.toggle_filter(),

        // Quick actions: 'a' — select all, 'c' — copy, 'o' — open, 's' — skip
        (Screen::EnvVarSelection(s), KeyCode::Char('s')) => state.skip_env_var(),
        (Screen::ModelSelection(s), KeyCode::Char('a')) => s.toggle_all(),
        _ => {}  // unhandled keys are ignored
    }
}
```

**Invariant**: All screen states expose the same navigation API (`cursor_up`, `cursor_down`, `selected`, `toggle_selected`, `toggle_filter`), so the dispatch table never needs to know which concrete list widget is active.

##### 12.4.4 Connect / `provider add` Wizard Flow

The connect wizard in the TUI follows the same sequence as the Go v1 bubbletea implementation:

| Step | Screen | User Action | Next Screen |
|------|--------|-------------|-------------|
| 1 | `ProductSelection` | Select a mature product (arrow keys + enter) or "Custom provider" | `EnvVarSelection` or `CustomProviderName` |
| 2 | `EnvVarSelection` | Select env var from scanned environment, or "[Skip]" | Configure → `ModelSelection` |
| 3 | `ModelSelection` | Multi-select models (space), All (a); at least one required | `Verifying` |
| 4 | `Verifying` | Connectivity probe against resolved endpoint/auth/model | `Done` (success) / `ProductSelection` (retry) |
| 5 | `Done` | Results displayed; Enter returns to `ProductSelection` | `ProductSelection` or `Quit` |

Custom provider sub-flow:

| Step | Screen | User Action | Next Screen |
|------|--------|-------------|-------------|
| 2a | `CustomProviderName` | Type a unique provider name | `CustomProviderEndpoint` |
| 2b | `CustomProviderEndpoint` | Type complete upstream URL | `EnvVarSelection` |
| 2c | `EnvVarSelection` | Select env var or "[Skip]" | Configure → save (no model verification) |

OAuth product sub-flow:

| Step | Screen | User Action | Next Screen |
|------|--------|-------------|-------------|
| 1 | `ProductSelection` | Select an OAuth-backed product | → |
| 2 | `OAuthLogin` | Choose provider name (recommended/custom) | → |
| 3 | Start OAuth flow | Display device verification URL; copy/open/skip shortcuts | Start polling |
| 4 | Polling timeout or success | Success → `EnvVarSelection` | Timeout → back to `OAuthLogin` |

##### 12.4.5 Widget Selection

| UI Element | ratatui Widget | Configuration |
|------------|---------------|---------------|
| Product list | `List` + `ListState` | Filterable (Title: "选择模型服务产品"), show display name + description |
| Env var list | `List` + `ListState` | Filterable, skip item appended at end |
| Model multi-select list | `List` + `ListState` + custom `highlight_symbol` | Toggle "[✓]" / "[ ]" markers per item |
| Text input | `Paragraph` with `Span::styled` cursor | Border with title showing prompt; raw mode keys handled directly |
| Error banner | `Paragraph` with red foreground | Displayed above the active list/input when error state is non-empty |
| Bottom help bar | `Paragraph` with dim style | Shortcut hints: `[Enter] Confirm  [Esc] Back  [q] Quit  [/] Search` |
| Confirmation popup | Clear block with double border | Centered on screen, overlaying current content |

##### 12.4.6 File Structure

The TUI implementation lives in a dedicated module:

```
src/tui/
  mod.rs         # Event loop, global state (AppModel)
  model.rs       # Screen enum, per-screen state structs
  update.rs      # handle_key dispatch, state transitions
  view.rs        # render function, per-screen draw helpers
  widgets.rs     # Shared widget constructors (list, input, help bar)
```

Separation rationale: `model.rs` is pure data (no IO), `update.rs` mutates state based on input, `view.rs` only reads state. This three-way split matches the Model-Update-View pattern and makes each file testable independently.

#### Overall hierarchy

```
Layer 1: Provider List
  ├─ [a] Add → Interactive creation flow (reuses provider add logic)
  ├─ [c] Copy → Same provider edit form as add, pre-filled from selected provider (**未实现**)
  ├─ [d] Delete → Delete with confirmation dialog
  ├─ [e] Edit → Open custom provider editor for selected provider
  ├─ [f] Fallback → Enter fallback configuration for selected provider
  ├─ [l] OAuth login → OAuth login flow (OAuth providers only)
  ├─ [r] Refresh → Re-probe provider status
  ├─ [u] Usage → Display usage statistics for selected provider
  ├─ [/] Search → Filter providers by name or product
  └─ [Enter] → Layer 2

Layer 2: Provider Detail
  ├─ Basic info (type, account, status, expiration)
  ├─ Usage area (window / balance / not displayed)
  └─ [r] Reset → Tiered confirmation → Result

Navigation: vim style
  Enter → Enter next layer
  Esc   → Return to previous layer
```

#### Layer 1: Provider List

```
┌─── 🔑 Provider Management ─────────────────────────────┐
│                                                       │
│  ✅ openai-sub-1        user@a.com     Pro   7d left   │
│  🔑 deepseek            api-key         Configured      │
│  ✅ antigravity         user@b.com     Logged in        │
│                                                       │
│  [a] Add  [d] Delete  [e] Edit  [f] Fallback  [l] Login  [r] Refresh  [u] Usage  [q] Quit │
└───────────────────────────────────────────────────────┘
```

When a provider is selected, the bottom shortcut bar dynamically appends:
- Has OAuth token and backend supports usage → `[u] Usage`
- API key type → Not appended

**[a] Add flow:**

Press `[a]` → Product selection (reuses `provider add` product selection logic):

```
┌─── Add Provider ────────────────────────────────────────┐
│                                                       │
│  Select product type:                                  │
│                                                       │
│  1. OpenAI Subscription (OAuth)                       │
│  2. Google Antigravity (OAuth)                        │
│  3. DeepSeek API (API Key)                            │
│  4. Custom provider...                                │
│                                                       │
│  [Enter] Confirm    [Esc] Cancel                      │
└───────────────────────────────────────────────────────┘
```

After selection, follow corresponding flow:
- **OAuth mature product** → Name → OAuth login → Select model(s) → Verify connectivity with the selected model(s) → Write provider config + auth state + selected model binding(s)
- **API Key mature product** → Name → Input API Key env var → Select model(s) → Verify connectivity with the selected model(s) → Write provider config + selected model binding(s)
- **Custom** → Name → Input type + endpoint_url + api_key_env/no-api-key → Write provider config only; no immediate model selection and no immediate connectivity verification

**[c] Copy flow (**未实现**):**

Press `[c]` on a selected provider to open the same provider edit form as add, pre-filled from the selected provider. The default new provider ID is `SOURCE-<num>`, where `<num>` is the first unused positive integer.

For OAuth providers, the form copies product and endpoint settings but leaves auth state empty. The user must confirm or edit the generated provider ID and then complete a fresh OAuth login for that new provider ID before the copy is saved as configured. This allows the user to choose a different account in the browser/device-code login flow.

For API-key providers, save is blocked until the user explicitly enters or confirms the credential env var. The TUI must not generate or suggest env var names from the provider ID; the user owns the naming decision. The key value itself is never shown or copied.

**Quick usage refresh in list:**

Press `[u]` → Display usage statistics for selected provider:

```
  📊 openai-sub-1 — 输入: 1.2M 输出: 340K 总计: 1.5M tokens | 156 次请求
```

#### Layer 2: Provider Detail

Press `Enter` or `l` to enter:

```
┌─── 📋 Provider Detail — openai-sub-1 ─────────────────┐
│                                                       │
│  Type:     OpenAI Subscription (Device Code)          │
│  Account:  user@a.com                                 │
│  Status:   ✅ authenticated                           │
│  Expires:  2026-07-28 (7d remaining)                  │
│                                                       │
│  ─── Usage ──────────────────────────────────────────  │
│                                                       │
│  Plan: Pro                                            │
│                                                       │
│  5-hour   ████████████░░░░░░░░  62%   reset 2h 31m    │
│  Weekly   ██████░░░░░░░░░░░░░░  30%   reset 3d 12h    │
│                                                       │
│  Available reset credits: 2                           │
│                                                       │
│  [r] Reset usage    [u] Refresh    [Esc/h] Back      │
└───────────────────────────────────────────────────────┘
```

Pay-as-you-go product:
```
│  ─── Usage ──────────────────────────────────────────  │
│                                                       │
│  Plan: Usage-Based                                    │
│  Balance: $12.34                                      │
│                                                       │
│  [u] Refresh    [Esc/h] Back                          │
└───────────────────────────────────────────────────────┘
```

No reset button.

API key type: Do not display usage area.

#### TUI reset flow

Reset in TUI follows the same tiered confirmation strategy:
- Used up → Execute directly
- Not used up → Secondary confirmation popup
- Usage < 50% → Secondary confirmation + three-color warning

#### TUI state machine

```rust
enum ProviderState {
    // Existing
    List,
    DeleteConfirm,
    Done,

    // New
    AddSelectProduct,        // Add: product selection
    AddConfigure,            // Add: configure parameters
    CopyConfigure,           // Copy: same form as add, pre-filled from selected provider
    ProviderDetail,          // Provider detail
    UsageConfirm,            // Secondary confirmation: query usage
    ResetConfirm,            // Secondary confirmation: reset usage
    ResetWarnLowUsage,       // Three-level confirmation: sufficient usage warning
    ResetLoading,            // Resetting
    ResetResult,             // Reset result display
}
```

State transitions:

```
List ──[+]──→ AddSelectProduct ──[Enter]──→ AddConfigure ──→ (creation flow) ──→ List
                                    └─[Esc]──→ List

List ──[c]──→ CopyConfigure ──→ (copy/save, optional OAuth login) ──→ List
                         └─[Esc]──→ List

List ──[Enter/l]──→ ProviderDetail
ProviderDetail ──[Esc/h]──→ List

List ──[u]──→ UsageConfirm ──[Enter]──→ (async query) ──→ List (update summary)
                          └─[Esc]──→ List

ProviderDetail ──[r]──→ Check usage state
    ↓ (limit_reached)          Execute directly ──→ ResetResult
    ↓ (used_percent >= 50%)    ResetConfirm ──[Enter]──→ ResetLoading ──→ ResetResult
    ↓ (used_percent < 50%)     ResetConfirm ──[Enter]──→ ResetWarnLowUsage ──[Enter]──→ ResetLoading ──→ ResetResult
```

##### 12.4.7 Provider Management Panel (✅ Implemented)

Bare `llm-proxy provider` enters the **Provider Management Panel** — a list view of all configured providers. This is the default TUI entry point (Phase 1-5 implemented, commits `294a98d`..`916c06b`).

**Provider list row displays:**

| Field | Source | Example |
|-------|--------|---------|
| Provider name | `config.toml` key | `deepseek`, `kimi-work` |
| Product | Catalog match by endpoint URL or `Custom` | `DeepSeek API`, `Kimi Platform CN` |
| Auth type | `auth` field | `ApiKey` / `OpenAI OAuth` / `Antigravity OAuth` |
| Status | Runtime probe | `✓` ok / `⚠` needs login / `✗` error |
| Protocols | Endpoint fields present | `chat · responses · anthropic` |

**Key bindings on the list view:**

| Key | Action |
|-----|--------|
| `a` | Add provider → product selection wizard |
| `Enter` | Provider detail view |
| `d` | Delete with confirmation dialog |
| `e` | Edit provider (open custom provider editor) |
| `f` | Fallback configuration for selected provider |
| `l` | OAuth login (OAuth providers only) |
| `r` | Refresh status (re-probe) |
| `u` | Usage statistics for selected provider |
| `/` | Search/filter by name or product |
| `q` | Quit |

**Delete confirmation** checks model bindings before removing. If the provider is referenced by model bindings, the dialog shows the referencing models and requires `[f]` force-delete or cancellation. This preserves the rule that provider commands do not silently edit model config.

**Product identification**: When loading a provider, its endpoint URLs are matched against the built-in catalog. A match displays the catalog product's display name; no match displays `Custom`. The same product can have multiple provider instances (e.g., `kimi-platform-cn`, `kimi-work`, `kimi-personal` all map to "Kimi Platform CN").

**相关独立设计**：
- Provider/Model 管理 TUI 界面设计：[`docs/design/rust-v2-tui-design.md`](docs/design/rust-v2-tui-design.md)

### 12.5 `model` Command — Manage Models

#### Entry behavior

| Usage | Behavior | Mode |
|-------|----------|------|
| `model` | TUI model list when stdin/stdout are interactive; usage error otherwise | TUI |
| `model --select X` | TUI model detail/edit convenience entry | TUI |
| `model list` | CLI text list all models | CLI |
| `model info MODEL` | CLI text detail | CLI |
| `model add MODEL ...` | Create a frontend model | CLI |
| `model remove MODEL [--force]` | Delete a frontend model | CLI |
| `model set MODEL ...` | Atomically edit model parameters/features | CLI |
| `model provider add/remove/move MODEL ...` | Manage protocol-specific provider bindings | CLI |

Pattern consistent with `provider`:
- **Bare `model`** → TUI only in interactive terminals; non-interactive invocation returns a usage error.
- **Subcommand present** → CLI execution.
- Model edits use subcommands rather than mutually exclusive action flags.

#### Complete command matrix

```bash
# Entry
model                                           → TUI list
model --select X                                → TUI detail/edit (interactive only convenience)
model list                                      → CLI text list

# View
model info MODEL                                → CLI text detail

# Create/delete frontend models; rename is intentionally unsupported
model add MODEL --context-window 200000 --max-output 8192
model add MODEL --copy-from SOURCE_MODEL        → Copy an existing model entry, then edit new name/bindings
model remove MODEL                              → Delete with secondary confirmation
model remove MODEL --force                      → Delete without confirmation

# Model parameters and features; all flags in one command apply atomically
model set MODEL --context-window 200000         → Set context length
model set MODEL --max-output 8192               → Set max output
model set MODEL --thinking-level high           → Set default thinking level
model set MODEL --enable-thinking               → Enable thinking
model set MODEL --disable-thinking              → Disable thinking
model set MODEL --enable-feature image_input    → Enable feature
model set MODEL --disable-feature image_input   → Disable feature
model set MODEL --context-window 200000 --thinking-level high --enable-feature image_input

# Provider bindings
model provider add MODEL --type anthropic --provider anthropic-direct --upstream-model claude-sonnet-20250514
model provider add MODEL --type openai-chat --provider openrouter
model provider remove MODEL --type anthropic --provider anthropic-direct
model provider move MODEL --type anthropic --provider anthropic-direct --to 1
```

#### Model command validation

- `MODEL` must exist for `info`, `set`, `remove`, and provider binding edits.
- `model add MODEL` creates a new frontend model ID and fails if it already exists. `display_name` may duplicate across models; model IDs must not. Rename is intentionally unsupported; users who need a similar model should create a new model and remove the old one explicitly.
- `model add MODEL --copy-from SOURCE_MODEL` is a safe copy-paste helper for model config editing: it copies the existing `[models.SOURCE_MODEL]` entry into a new `[models.MODEL]` entry, then lets the user change the new model ID and provider bindings without hand-editing TOML. It is a copy, not a rename: launch configs and existing clients using `SOURCE_MODEL` are unaffected. In TUI copy flow, when the user has not typed a target ID yet, the default target ID is generated as `SOURCE_MODEL-<num>` with `<num>` increasing until the ID is unused.
- `model add MODEL` without `--copy-from` must provide at least `--context-window` and `--max-output`; provider bindings are normally added with `model provider add`.
- `model remove MODEL` deletes the full frontend model entry after secondary confirmation; `--force` skips confirmation. Deleting a model never deletes providers or provider credentials. Launch-managed client configs are not rewritten immediately; the next `launch` command refreshes client-side model catalogs.
- Model rename is not implemented in CLI or TUI.
- `--context-window` and `--max-output` must be positive integers.
- `--enable-thinking` and `--disable-thinking` are mutually exclusive.
- `--thinking-level LEVEL` must be present in `supported_reasoning_levels` when that list exists; otherwise the command fails with supported values.
- `--enable-feature` / `--disable-feature` accept known feature names only.
- `model provider add` requires `--type`; valid values are `openai-chat`, `openai-responses`, `anthropic`.
- `model provider add` requires an existing provider whose endpoint graph declares the requested protocol.
- A provider may not appear twice in the same model/protocol binding list.
- If `--upstream-model` is omitted, it defaults to the frontend model ID.
- All edits validate the full config first and commit with one atomic write.

#### Model config structure

Complete fields for one model in config.toml (v3 flat format):

```toml
[models.claude-sonnet-lp]
# ── Model-level parameters ──
display_name = "Claude Sonnet"
context_window = 200000
max_output_tokens = 8192
features = ["image_input", "tool_call_reasoning"]
enable_thinking = true
supported_reasoning_levels = ["low", "medium", "high"]
default_reasoning_level = "medium"

# ── Protocol-specific provider bindings ──
openai_chat_providers = [
  { name = "openrouter", model = "anthropic/claude-sonnet" },
]
anthropic_providers = [
  { name = "anthropic-direct", model = "claude-sonnet-20250514" },
]
openai_responses_providers = [
  { name = "openrouter-resp", model = "anthropic/claude-sonnet" },
]
```

Field descriptions:

| Field | Type | Description |
|-------|------|-------------|
| `display_name` | string? | Optional user-facing display label; does not participate in model ID resolution. |
| `context_window` | int | Context window size |
| `max_output_tokens` | int | Max output token count |
| `features` | []string | Feature switches (image_input, tool_call_reasoning, etc.) |
| `enable_thinking` | bool | Whether to enable thinking |
| `supported_reasoning_levels` | []string | Supported thinking level list |
| `default_reasoning_level` | string | Default thinking level |
| `{type}_providers` | []binding | Provider list bound to each protocol type (order = fallback priority) |

Each provider binding includes:
- `name`: Reference to existing provider name
- `model`: Model name on that provider (may differ across providers)

#### `list` output

```
$ model list

claude-sonnet-lp         200K  thinking:medium  image_input  3 providers
deepseek-v3-lp           64K   thinking:off                    1 provider
gpt-5.5-lp               272K  thinking:medium  image_input  2 providers
```

#### `info` output

```
$ model info claude-sonnet-lp

model:          claude-sonnet-lp
context_window: 200000
max_output:     8192
thinking:       enabled (default: medium, supported: low/medium/high)
features:       image_input, tool_call_reasoning

provider bindings:
  openai-chat:
    [1] openrouter        → anthropic/claude-sonnet
    [2] openrouter-2      → anthropic/claude-sonnet
  anthropic:
    [1] anthropic-direct  → claude-sonnet-20250514
  openai-responses:
    (none)
```

#### `model provider add/remove/move` flow

Full parameters (all flags explicit, non-interactive):
```
$ model provider add claude-sonnet-lp \
        --type anthropic \
        --provider anthropic-direct \
        --upstream-model claude-sonnet-20250514

✅ Added provider "anthropic-direct" (upstream model: claude-sonnet-20250514) to claude-sonnet-lp [anthropic]
```

Omit `--upstream-model` (defaults to model's own name):
```
$ model provider add claude-sonnet-lp --type openai-chat --provider openrouter

✅ Added provider "openrouter" (upstream model: claude-sonnet-lp) to claude-sonnet-lp [openai-chat]
```

Omit `--type`, CLI mode errors requiring explicit specification:
```
$ model provider add claude-sonnet-lp --provider openrouter

Error: --type is required. Valid types: openai-chat, openai-responses, anthropic
```

Interactive provider/type selection is only provided in the TUI edit interface.


#### TUI model list actions

The model list supports:

- `[+] Add model` — open the model edit form with empty/default values. The form edits the future `[models.<id>]` entry before writing it.
- `[c] Copy model` — open the **same model edit form** used by add, but pre-filled from the selected existing model. The default new model ID is `SOURCE-<num>`, where `<num>` is the smallest positive integer that avoids an existing model ID (`claude-sonnet-lp-1`, `claude-sonnet-lp-2`, ...). The user may keep that generated ID or edit it to any valid unused model ID before saving.
- `[-] Delete model` — delete the selected frontend model after confirmation; never deletes providers or credentials.
- `[Enter/l] Detail` — open model detail/edit view.
- `[q] Quit`.

Add and copy intentionally converge on one edit area so the user can review/change the same fields before commit: model ID, context window, max output, thinking defaults, features, protocol-specific provider bindings, and upstream model names. Copying is the TUI equivalent of copying one `[models.X]` TOML block, pasting it as `[models.Y]`, then editing the pasted block — but with validation so fields are not missed or mistyped.

Copied models are independent. Copying is deliberately not rename: the original model ID remains valid until the user explicitly removes it.

TUI configuration is not a hidden runtime database. Its durable output is the same user config file that a human could edit by hand. After a machine is configured through the TUI, the resulting config file can be copied to another machine as the model/provider configuration source of truth, subject to that machine supplying the referenced environment variables and OAuth auth state where applicable.

#### TUI detail/edit interface

The model detail screen is a field-level editor for one `[models.<id>]` table. Scalar fields and provider-list fields are shown as peers. The three provider-list fields are not collapsed into one generic "providers" area; they are separate editable fields because they map to separate config keys and separate client protocols.

```
┌─── 📋 Model Detail — claude-sonnet-lp ────────────────┐
│                                                       │
│  model_id:                  claude-sonnet-lp          │
│  context_window:            200000                    │
│  max_output_tokens:         8192                      │
│  enable_thinking:           true                      │
│  default_reasoning_level:   medium                    │
│  supported_reasoning_levels: low, medium, high        │
│  features:                  image_input, tool_call_reasoning │
│                                                       │
│  openai_chat_providers:      2 bindings               │
│  openai_responses_providers: 0 bindings               │
│  anthropic_providers:        1 binding                │
│                                                       │
│  [Enter] Edit selected field    [Esc/h] Back          │
└───────────────────────────────────────────────────────┘
```

Field behavior:

- Selecting scalar fields opens a scalar editor: integer input for `context_window` / `max_output_tokens`, boolean toggle for `enable_thinking`, level selector for `default_reasoning_level`, and multi-select for `features` / `supported_reasoning_levels`.
- Selecting `openai_chat_providers` opens the provider binding editor scoped only to the OpenAI Chat protocol.
- Selecting `openai_responses_providers` opens the provider binding editor scoped only to the OpenAI Responses protocol.
- Selecting `anthropic_providers` opens the provider binding editor scoped only to the Anthropic protocol.

#### TUI provider-list field editor

When the user enters one `xx_providers` field, the UI manages only that list. The main view is the already-configured provider binding list in fallback order; shortcut help at the bottom explains how to edit the selected binding. Example for `openai_chat_providers`:

```
┌─── openai_chat_providers — claude-sonnet-lp ──────────┐
│ Configured providers (fallback order):                │
│                                                       │
│  › [1] deepseek-main      upstream: deepseek-v4-pro   │
│        endpoint: native openai_chat                   │
│    [2] openrouter         upstream: anthropic/claude-sonnet │
│        endpoint: native openai_chat                   │
│    [3] kimi-code-api      upstream: k3                │
│        endpoint: derived from openai_responses         │
│                                                       │
│ Available to add: 2 providers with openai_chat endpoint │
│                                                       │
│ [a] Add provider   [e] Edit upstream model            │
│ [-] Remove         [K] Move up   [J] Move down        │
│ [Enter] Detail     [s] Save      [Esc/h] Back         │
└───────────────────────────────────────────────────────┘
```

The configured list is the primary object being edited because it maps directly to the TOML array:

```toml
openai_chat_providers = [
  { name = "deepseek-main", model = "deepseek-v4-pro" },
  { name = "openrouter", model = "anthropic/claude-sonnet" },
  { name = "kimi-code-api", model = "k3" },
]
```

Provider-list actions:

- `[a] Add provider` opens a filtered picker of configured providers that declare the selected protocol endpoint and are not already in this `xx_providers` list. After selecting one, the UI prompts for the upstream model name; default is the frontend model ID.
- `[e] Edit upstream model` edits the `model = "..."` value for the currently selected provider binding.
- `[K] Move up` and `[J] Move down` move the selected provider within the current list, changing fallback priority. Arrow-key alternatives are allowed, but the UI should show explicit shortcuts so the behavior is discoverable.
- `[-] Remove` opens a secondary confirmation before removing the selected binding. The default selected action must be cancel / do not delete, so pressing Enter without changing the choice is safe.
- `[Enter] Detail` may show provider endpoint/auth/status details read-only to help the user decide ordering, but edits to provider config itself belong in the provider TUI.
- `[s] Save` validates the full config and writes atomically. Leaving the editor with unsaved changes asks whether to save, discard, or cancel.

Provider-list rules:

- The available picker contains only configured providers that declare the selected protocol endpoint. For `anthropic_providers`, a provider must declare `providers.<id>.anthropic`; for `openai_chat_providers`, it must declare `openai_chat`; for `openai_responses_providers`, it must declare `openai_responses`.
- Providers already present in the selected list are excluded from the available picker. They can be edited, removed, or reordered from the configured list instead.
- Unsupported providers may be hidden by default or shown disabled with a reason, but they must not be selectable.
- Reordering is limited to the current `xx_providers` list and directly changes fallback priority for that protocol.
- Saving validates the full config and writes atomically.

This field-scoped design prevents accidental cross-protocol edits: editing `openai_chat_providers` cannot modify `anthropic_providers` or `openai_responses_providers`.

#### clap definition

```rust
Command::Model(ModelCommand),

enum ModelCommand {
    /// Interactive TUI entry; optional selected model for detail view.
    Tui {
        #[arg(long)]
        select: Option<String>,
    },
    List,
    Info {
        model: String,
    },
    Add {
        model: String,
        #[arg(long = "copy-from")]
        copy_from: Option<String>,
        #[arg(long)]
        context_window: Option<i64>,
        #[arg(long)]
        max_output: Option<i64>,
    },
    Remove {
        model: String,
        #[arg(long)]
        force: bool,
    },
    Set {
        model: String,
        #[arg(long)]
        context_window: Option<i64>,
        #[arg(long)]
        max_output: Option<i64>,
        #[arg(long)]
        thinking_level: Option<String>,
        #[arg(long)]
        enable_thinking: bool,
        #[arg(long)]
        disable_thinking: bool,
        #[arg(long)]
        enable_feature: Vec<String>,
        #[arg(long)]
        disable_feature: Vec<String>,
    },
    Provider {
        command: ModelProviderCommand,
    },
}

enum ModelProviderCommand {
    Add {
        model: String,
        #[arg(long = "type")]
        provider_type: String,
        #[arg(long)]
        provider: String,
        #[arg(long)]
        upstream_model: Option<String>,
    },
    Remove {
        model: String,
        #[arg(long = "type")]
        provider_type: String,
        #[arg(long)]
        provider: String,
    },
    Move {
        model: String,
        #[arg(long = "type")]
        provider_type: String,
        #[arg(long)]
        provider: String,
        #[arg(long)]
        to: usize,
    },
}
```

#### Internal module design

```
src/model/
  mod.rs         # Model management (list/info/edit + CLI dispatch)
  edit.rs        # Parameter modification, feature toggle, provider binding add/remove
```

Core functions:

```rust
/// List all model summaries
pub fn list_models(config: &Config) -> Vec<ModelSummary>;

/// Get complete details for specified model
pub fn get_model_detail(config: &Config, name: &str) -> Result<ModelDetail>;

/// Create a new frontend model, optionally by copying an existing model entry.
pub fn add_model(config_path: &Path, name: &str, patch: ModelPatch, copy_from: Option<&str>) -> Result<()>;

/// Delete a frontend model; never deletes providers or credentials.
pub fn remove_model(config_path: &Path, name: &str) -> Result<()>;

/// Modify model parameters (context_window, max_output, thinking_level, etc.)
pub fn update_model_params(config_path: &Path, name: &str, patch: ModelPatch) -> Result<()>;

/// Enable/disable feature
pub fn toggle_feature(config_path: &Path, name: &str, feature: &str, enable: bool) -> Result<()>;

/// Add provider binding
pub fn add_provider_binding(
    config_path: &Path,
    model_name: &str,
    type_name: &str,
    provider_name: &str,
    upstream_model: &str,
) -> Result<()>;

/// Remove provider binding
pub fn remove_provider_binding(
    config_path: &Path,
    model_name: &str,
    type_name: &str,
    provider_name: &str,
) -> Result<()>;
```

#### Style comparison with `provider` command

| Dimension | `provider` | `model` |
|-----------|-----------|---------|
| Interactive entry | `provider` → TUI | `model` → TUI |
| TUI selected target | `provider --select X` | `model --select X` |
| List | `provider list` | `model list` |
| Detail | `provider info [NAME]` | `model info MODEL` |
| Modify | action subcommands (`remove`, `reset-usage`, auth actions) | `set`, `provider add/remove/move` |
| Skip confirmation | `--force` on destructive actions | reserved for future destructive actions |

Style is consistent: bare commands are TUI entries; scriptable actions use subcommands. This avoids flag-driven mutually-exclusive action dispatch while preserving interactive convenience.

### 12.6 ChatGPT Backend API (Source: Code Analysis)

#### Query usage

```
GET https://chatgpt.com/backend-api/wham/usage
Authorization: Bearer {access_token}
```

Response structure (`RateLimitStatusPayload`):

| Field | Type | Description |
|-------|------|-------------|
| `plan_type` | enum | `free` / `go` / `plus` / `pro` / `team` / `business` / `enterprise` / `education` / `self_serve_business_usage_based` / `enterprise_cbp_usage_based` etc. |
| `rate_limit` | object? | `{ allowed, limit_reached, primary_window, secondary_window }` |
| `credits` | object? | `{ has_credits, unlimited, balance }` |
| `spend_control` | object? | Spend control (enterprise scenarios) |
| `additional_rate_limits` | array? | Additional limits (distinguished by `metered_feature`) |
| `rate_limit_reached_type` | enum? | Reached type |
| `rate_limit_reset_credits` | object? | `{ available_count: i64 }` — Available reset credits |

Window structure (`RateLimitWindowSnapshot`):

| Field | Type | Description |
|-------|------|-------------|
| `used_percent` | i32 | Used percentage |
| `limit_window_seconds` | i32 | Window duration |
| `reset_after_seconds` | i32 | Seconds until reset |
| `reset_at` | i32 | Reset timestamp |

#### Consume reset

```
POST https://chatgpt.com/backend-api/wham/rate-limit-reset-credits/consume
Authorization: Bearer {access_token}
Content-Type: application/json

{ "redeem_request_id": "<uuid-v4>" }
```

Response:

| `code` | Meaning |
|--------|---------|
| `reset` | Reset successful |
| `nothing_to_reset` | Currently nothing to reset (limit not reached) |
| `no_credit` | No available reset credits |
| `already_redeemed` | Idempotency key already used |

#### Authentication requirements

Both endpoints require ChatGPT OAuth access_token (not API key).

> Credibility: 🔵 1 (based on Codex CLI source analysis, not verified with actual API calls)

### 12.6A Usage 数据源设计（Usage Data Source）

#### 12.6A.1 核心原则

**Server 统一管理 usage 数据**：只有经过 server 的请求才统计，内存为权威数据源，磁盘为持久化备份。

#### 12.6A.2 数据流

```text
Server 启动：
  磁盘（JSONL/SQLite）→ 加载 → 内存（权威数据源）
  
Server 运行：
  转发请求 → 写入内存 → 定期智能落盘
  
CLI 查询：
  CLI → 委托 Server → 读取内存 → 返回
  
Server 崩溃：
  内存丢失 → 重启时从磁盘加载（最多丢失"上次落盘到现在"的数据）
```

#### 12.6A.3 智能落盘策略

**核心思想**：根据请求频率动态调整落盘间隔，平衡及时性和磁盘寿命。

#### 不落盘条件

- `dirty = false`（所有数据已落盘）→ **不启动定时器**（零 I/O）

#### 落盘条件

- `dirty = true`（有未落盘的数据）→ 按频率间隔落盘

#### 频率分档

| 请求频率（条/分钟） | 落盘间隔 | 场景 |
|-------------------|---------|------|
| 0 | 不落盘（dirty=false 时） | 完全空闲 |
| 1-10 | 180 秒（3 分钟） | 轻度使用 |
| 11-100 | 45 秒 | 中度使用 |
| >100 | 15 秒 | 重度使用 |

#### 状态机

| 状态 | dirty | 定时器 | 行为 |
|------|-------|--------|------|
| **空闲** | false | 未启动 | 不落盘，不启动定时器（零 I/O） |
| **有数据未落盘** | true | 运行中 | 按频率间隔落盘 |
| **刚落盘完** | false | 运行中 | 停止定时器，回到空闲 |

#### 实现要点

```rust
struct UsageStore {
    records: Vec<UsageRecord>,      // 内存中的 usage 数据
    dirty: bool,                     // 是否有新数据
    recent_requests: VecDeque<i64>,  // 最近 60 秒的请求时间戳（滑动窗口）
}

impl UsageStore {
    fn on_request(&mut self) {
        let now = now_timestamp();
        self.recent_requests.push_back(now);
        self.dirty = true;
        // 清理 60 秒前的记录
        while let Some(&old) = self.recent_requests.front() {
            if now - old > 60 {
                self.recent_requests.pop_front();
            } else {
                break;
            }
        }
    }
    
    fn calculate_interval(&self) -> Duration {
        let rpm = self.recent_requests.len() as u32;
        match rpm {
            0 => Duration::from_secs(180),  // 3 分钟（最低档）
            1..=10 => Duration::from_secs(180),
            11..=100 => Duration::from_secs(45),
            _ => Duration::from_secs(15),
        }
    }
}
```

#### 12.6A.4 磁盘写入频率（优化后）

| 使用模式 | 每天落盘次数 | 说明 |
|---------|-------------|------|
| 完全空闲 | **0 次** | dirty=false，定时器不启动 |
| 偶尔请求（1-10 条/分钟） | ~480 次（每 3 分钟一次） | 只在 dirty=true 时 |
| 中度（11-100 条/分钟） | ~1920 次（每 45 秒一次） | 只在 dirty=true 时 |
| 重度（>100 条/分钟） | ~5760 次（每 15 秒一次） | 只在 dirty=true 时 |

#### 12.6A.5 独立模式兜底

**Server 未启动时**：CLI 直接读磁盘（JSONL/SQLite），加文件锁避免竞争。

---


### 12.7 Internal Module Design

#### File structure

```
src/provider/
  mod.rs         # Provider management (add/list/info/remove + CLI dispatch)
  add.rs         # Create provider (product selection, configuration, OAuth login, atomic write)
  usage.rs       # Usage query + reset (pure logic, no TUI/CLI output)
```

#### Core types

```rust
// src/provider/usage.rs

pub struct UsageStatus {
    pub plan_type: PlanType,
    pub rate_limit: Option<RateLimitInfo>,
    pub credits: Option<CreditInfo>,
    pub reset_credits_available: Option<i64>,
}

pub enum PlanType {
    Subscription(SubscriptionPlan),
    UsageBased,
    Unknown(String),
}

pub enum SubscriptionPlan {
    Free, Go, Plus, Pro, ProLite,
    Team, Business, Enterprise, Education,
    FreeWorkspace, K12, Quorum,
}

pub struct RateLimitInfo {
    pub allowed: bool,
    pub limit_reached: bool,
    pub primary_window: Option<WindowSnapshot>,
    pub secondary_window: Option<WindowSnapshot>,
}

pub struct WindowSnapshot {
    pub used_percent: i32,
    pub reset_after_seconds: i32,
}

pub struct CreditInfo {
    pub has_credits: bool,
    pub unlimited: bool,
    pub balance: Option<String>,
}

pub enum ConsumeResult {
    Reset,
    NothingToReset,
    NoCredit,
    AlreadyRedeemed,
}

/// Confirmation level (shared by CLI and TUI)
pub enum ResetConfirmLevel {
    None,           // Used up, execute directly
    Confirm,        // Not used up, requires secondary confirmation
    ConfirmWarn,    // Usage < 50%, requires secondary confirmation + three-level warning
}

impl UsageStatus {
    pub fn reset_confirm_level(&self) -> ResetConfirmLevel {
        let any_limit_reached = self.rate_limit.as_ref()
            .is_some_and(|r| r.limit_reached);
        if any_limit_reached {
            return ResetConfirmLevel::None;
        }

        let any_low_usage = self.rate_limit.as_ref()
            .is_some_and(|r| {
                let primary_low = r.primary_window.as_ref()
                    .is_some_and(|w| w.used_percent < 50);
                let secondary_low = r.secondary_window.as_ref()
                    .is_some_and(|w| w.used_percent < 50);
                primary_low || secondary_low
            });

        if any_low_usage {
            ResetConfirmLevel::ConfirmWarn
        } else {
            ResetConfirmLevel::Confirm
        }
    }
}
```

#### Core functions

```rust
/// Query usage
pub async fn query_usage(cred: &AuthCredential) -> Result<UsageStatus>;

/// Consume one reset
pub async fn consume_reset(cred: &AuthCredential) -> Result<ConsumeResult>;

/// Resolve target credential from auth state
pub fn resolve_credential(auth_path: &Path, provider: &str) -> Result<AuthCredential>;
```

#### HTTP calls

```rust
const CHATGPT_BACKEND_BASE: &str = "https://chatgpt.com/backend-api";

async fn query_usage(cred: &AuthCredential) -> Result<UsageStatus> {
    let token = cred.access_token.as_deref()
        .context("no access_token, try refreshing first")?;

    let resp = reqwest::Client::new()
        .get(format!("{CHATGPT_BACKEND_BASE}/wham/usage"))
        .bearer_auth(token)
        .send()
        .await?
        .error_for_status()?;

    let payload: serde_json::Value = resp.json().await?;
    Ok(UsageStatus::from_json(payload))
}

async fn consume_reset(cred: &AuthCredential) -> Result<ConsumeResult> {
    let token = cred.access_token.as_deref()
        .context("no access_token")?;

    let idempotency_key = uuid::Uuid::new_v4().to_string();

    let resp = reqwest::Client::new()
        .post(format!(
            "{CHATGPT_BACKEND_BASE}/wham/rate-limit-reset-credits/consume"
        ))
        .bearer_auth(token)
        .json(&serde_json::json!({ "redeem_request_id": idempotency_key }))
        .send()
        .await?
        .error_for_status()?;

    let result: serde_json::Value = resp.json().await?;
    Ok(ConsumeResult::from_json(result))
}
```

### 12.8 Boundaries and Limitations

1. **Only applicable to OAuth providers**: API key authenticated providers have no usage API; `provider info` does not display usage area; `provider reset-usage` reports unsupported
2. **Pay-as-you-go products have no reset**: `provider reset-usage` reports unsupported for usage-based plans
3. **Token expiration**: If access_token has expired, `provider info` usage area prompts to refresh token first
4. **Network dependency**: Usage API requires access to `chatgpt.com`, unavailable in offline environments
5. **API not verified**: Current design based on Codex CLI source analysis (credibility 1), implementation requires verification with actual OAuth token
6. **Multi-window adaptive**: Some plan_types may have only one window (primary_window has value, secondary_window is None), UI should adapt

### 12.9 `connect` Command

``connect` is retained as a user-friendly mature-product setup alias for `provider add` for script convenience and existing user habits. `connect` without `PRODUCT` in an interactive terminal opens the provider-add wizard, not the full provider management TUI; use bare `provider` for the full provider TUI. For mature catalog products, non-interactive `connect` requires at least one `--model` and verifies before writing; custom provider-only setup belongs to `provider add --type ... --endpoint-url ...`. All supported mature-product flags pass through unchanged:

```bash
# These two are equivalent
connect deepseek --api-key-env DEEPSEEK_API_KEY --model deepseek-v4-pro-lp
provider add deepseek --api-key-env DEEPSEEK_API_KEY --model deepseek-v4-pro-lp

# These two are also equivalent
connect
provider add
```

`connect` implementation directly calls `provider::add()`:

```rust
Command::Connect(args) => {
    provider::add(args.into_provider_add_args())
}
```

For detailed `provider add` design, see §12.2.

## 13. Launch Command Design

Launch commands generate client config pointing at the proxy address the CLI actually uses to reach the server, with dummy/local credentials. Upstream secrets remain in llm-proxy config/env/token storage.

**地址来源统一原则**：launch 生成的客户端 base URL = **CLI 实际访问 server 的地址** + 协议前缀：
- 本地模式：`cfg.server.listen`（现状）
- 远程模式（待实现）：`server.listen` 的 origin（scheme://host:port），地址由 CLI 自己推导——server 不知道 CLI 的网络视角，client-config 端点不返回 base URL
- 不提供 `--proxy-url` 类覆盖 flag：CLI 与目标客户端处于同一网络位置，CLI 可达 server 的地址即客户端可达地址

### 13.1 Shared launch invariants

All launch commands must:

- load and validate llm-proxy config;
- choose models that support the client protocol;
- write atomically where possible;
- preserve user-owned settings;
- only replace llm-proxy-managed blocks/entries;
- support dry-run consistently;
- support path override flags for isolated testing;
- never write upstream API keys into client config.

### 13.2 Codex CLI and Codex Desktop

`llm-proxy launch codex` and `llm-proxy launch codex-desktop` share the same Codex config generator unless a future Desktop build proves separate state. Both targets must:

- generate/update Codex `~/.codex/config.toml` and a deterministic llm-proxy-owned model catalog JSON;
- preserve `~/.codex/auth.json` and unrelated `config.toml` settings;
- configure `wire_api = "responses"`; Codex current source rejects `wire_api = "chat"`;
- point base URL at the Responses-compatible prefix derived from the CLI's server address (`server.listen`), normally `http://127.0.0.1:8989/v1` so Codex appends `/responses`;
- omit unsupported inline `api_key` fields for unauthenticated local proxy; if frontend auth is later enabled, use Codex-supported `env_key` or `experimental_bearer_token`;
- include all client-visible models that support Responses via the model catalog;
- warn for Codex Desktop that the model picker may filter out `model_catalog_json` models due to upstream issue #19694; direct `model`/`model_provider` config remains the fallback.

Flags:

```text
--codex-home PATH
--dry-run
```

### 13.3 pi

`llm-proxy launch pi` must:

- generate or update `~/.pi/agent/models.json`;
- preserve non-llm-proxy providers;
- replace only the generated llm-proxy OpenAI Chat-compatible provider entry;
- include models that support OpenAI Chat;
- treat the exact Pi schema as evidence-limited until Pi CLI smoke/source validation passes, because current research relies partly on existing llm-proxy behavior/tests.

Flags:

```text
--pi-home PATH
--dry-run
```

### 13.4 Claude Code

`llm-proxy launch claude-code <model-id>` must:

- require a model that supports Anthropic protocol;
- write Claude Code settings/env variables for local Anthropic endpoint;
- preserve permissions, hooks, MCP, and unrelated user settings;
- support model slot flags:
  - `--default`
  - `--haiku`
  - `--sonnet`
  - `--opus`
- allow combined slot updates in one command.

Flags:

```text
--claude-home PATH
--default
--haiku
--sonnet
--opus
--dry-run
```

### 13.5 Claude Desktop / Claude Code Desktop

`llm-proxy launch claude-desktop` must:

- generate Claude Desktop third-party gateway profile config, not Claude Code `settings.json`;
- support macOS and Windows paths; Linux is unsupported/deferred until live Desktop behavior is verified;
- manage a deterministic llm-proxy-owned profile ID distinct from cc-switch and preserve unrelated `_meta.json` entries;
- snapshot and rollback the normal config, 3P config, gateway profile, and `_meta.json` on partial write failure;
- write both normal and 3P marker files to `deploymentMode = "3p"` when enabling, and restore both to `"1p"` when restoring official mode;
- set `inferenceGatewayBaseUrl` to a gateway mount derived from the CLI's server address, such as `http://127.0.0.1:8989/claude-desktop`, with local frontend token only; never write upstream provider secrets;
- expose safe Claude-like route IDs (`claude-sonnet-*`, `claude-opus-*`, `claude-haiku-*`) and map them internally to real provider/model bindings;
- generate `inferenceModels[].supports1m` only when the resolved target model has `context_window >= 1_000_000`; do not add a `supports_1m` config field;
- require a full Claude Desktop restart after config changes; no hot reload promise;
- keep path-appending behavior, `/v1/models` discovery, streaming, and `coworkEgressAllowedHosts` necessity as smoke-test acceptance items before release.

Flags:

```text
--dry-run
--profile NAME
--create-profile NAME
--edit-profile NAME
--haiku MODEL
--sonnet MODEL
--opus MODEL
```

### 13.6 Qwen Code

`llm-proxy launch qwen-code [model-id]` must:

- generate/update `~/.qwen/settings.json`;
- preserve permissions, hooks, MCP, privacy, and unrelated settings;
- configure `modelProviders.openai` with local `baseUrl`, `envKey`, and `generationConfig` fields as documented by Qwen Code;
- avoid relying on stale top-level `contextWindow` / `maxOutputTokens` if current Qwen Code expects `generationConfig.contextWindowSize`;
- set auth selection to OpenAI-compatible mode when required;
- include or select models supporting OpenAI Chat.

Flags:

```text
--qwen-home PATH
--dry-run
```

## 14. Config Editing and Docs

### 14.1 Round-trip editing

llm-proxy should use `toml_edit` for its TOML updates so `init`, `connect`, `provider`, `model`, and future config editing preserve user comments, ordering, and unrelated fields. External JSON client configs should use `serde_json::Value` or typed structs only for fully managed projection files.

### 14.2 No migration command

The current Rust implementation does **not** provide `llm-proxy migrate`. The supported configuration lifecycle is:

1. `llm-proxy init` creates a fresh minimal llm-proxy config when no config exists.
2. `llm-proxy connect` / `llm-proxy provider add` add provider configuration.
3. `llm-proxy model` edits frontend model metadata and provider bindings.

Legacy Go/v1 configs are not rewritten in place by the current Rust implementation. Users who need llm-proxy should create a new config with `init` and re-add providers/models through the management commands. This keeps the rewrite strict and avoids silently inventing provider endpoint declarations during schema changes.

### 14.2A 迁移策略（Migration Strategy）

现有代码不需要一次性重构，采用渐进式迁移：

### Phase 1：定义 Core 层（不改动现有代码）

```rust
// 新增 src/core.rs
pub struct CoreState { ... }
impl CoreState { ... }
```

### Phase 2：新功能的 Core 化

- usage_stats 功能直接按新架构实现
- `UsageStore` 作为 `CoreState` 的组件

### Phase 3：逐步迁移现有功能

- `connect.rs` 中的 `add_provider`、`remove_provider` 等业务逻辑迁入 `CoreState`
- 现有函数变为薄包装：

```rust
// 过渡期：旧函数调用 Core
pub fn remove_provider(config_path: &Path, id: &str, force: bool) -> Result<()> {
    if !force { confirm_remove_provider(id)?; }  // 交互层
    let mut core = CoreState::load(config_path)?;
    core.remove_provider(id)?;                    // 业务层
    core.save()?;                                 // 持久化
    Ok(())
}
```

### Phase 4：Server 化

- 添加 Admin API 路由
- Server 启动时创建 `ServerCore { inner: Mutex<CoreState> }`
- CLI 添加 Server 检测逻辑

### Phase 5：远程模式（待实现）

- 使用 `server.listen` 配置远程 Server 地址
- CLI 添加远程 HTTP 只读客户端（status/usage 查询）
- 写操作仅本机 UDS（2026-08-09 决策：远程只读）
- 远程读操作认证方案待设计

---


### 14.3 Embedded docs

The current Rust implementation must preserve `llm-proxy doc` behavior:

- `llm-proxy doc` opens/browses docs when interactive;
- `--list` prints the doc tree;
- `--raw` prints all content;
- `--section SECTION` prints a selected section;
- embedded docs are generated from the documented source directory and checked in CI if that remains the chosen distribution model.

### 14.5 数据位置与敏感信息（Data Locations）

#### 14.5.1 核心原则：谁发请求，谁持有凭据

Server 是实际向上游 API 发请求的进程，因此 API Key、OAuth Token 必须在 Server 机器上可用。

#### 14.5.2 各数据的存放位置

| 数据 | 本地模式 | 远程模式 | 说明 |
|------|---------|---------|------|
| config.toml | Server 机器（共享 fs） | Server 机器 | CLI 通过 API 或文件访问 |
| api_key_env | Server 机器环境变量 | Server 机器环境变量 | CLI 设置的是变量名，实际值在 Server |
| OAuth tokens | Server 机器 state 目录 | Server 机器 state 目录 | Server 使用 token 访问上游 |
| usage 数据 | Server 机器 state 目录 | Server 机器 state 目录 | Server 记录每次请求的用量 |
| probe 验证结果 | Server 发起并存储 | Server 发起并存储 | Server 验证自己的网络可达性 |
| lock file | 同机器有效 | 不存在 | 远程模式不需要文件锁 |
| PID file | 同机器有效 | 不存在 | 远程模式无法检测进程 |
| 客户端配置 (.codex/...) | CLI 本地机器 | CLI 本地机器 | 从 Server 获取信息后写入本地 |

#### 14.5.3 远程模式下的环境变量问题

用户在本地 CLI 设置 `api_key_env = "DEEPSEEK_API_KEY"`，这个环境变量指的是 **Server 机器上的环境变量**，不是 CLI 本地的。

**处理方式：**
1. CLI 发送 `ProviderAdd { api_key_env: "DEEPSEEK_API_KEY" }` 给 Server
2. Server 检查自己的环境中是否存在该变量
3. 不存在 → 返回警告：`"env var DEEPSEEK_API_KEY not set on server"`
4. 存在 → 发 probe 请求验证连通性

**设置远程环境变量的方式（分阶段）：**
- **Phase 1（当前）**：用户 SSH 到 Server 手动设置
- **Phase 2（后续）**：Admin API 提供 `/admin/env/set` 端点（需加密传输）

#### 14.5.4 远程模式下的 Launch/Connect

```
用户在本地：llm-proxy launch codex

CLI 流程：
1. GET /admin/client-config/codex → Server 返回配置内容（模型/协议/能力/auth）
2. CLI 用 server.listen 的 origin 推导 base_url（地址 = CLI 实际访问 server 的地址）
3. 翻译成本地客户端格式
4. 写入本地 ~/.codex/config.toml

写入的内容指向本地/远程 Server：
  [model_providers.remote]
  base_url = "<server.listen origin>/v1"
```

**地址来源原则**：base_url 由 **CLI 推导**（用 `server.listen` origin + 协议前缀），**server 不返回 base_url**——server 不知道 CLI 是从哪个地址访问它的，返回地址反而可能错。CLI 与目标客户端处于同一网络位置，CLI 可达 server 的地址即客户端可达地址。launch 不提供 `--proxy-url` 类覆盖 flag。

---


## 15. State, Cache, and Security

llm-proxy should keep user data locations predictable and secure:

- config under XDG config conventions, defaulting to `~/.config/llm-proxy/config.toml`;
- state under XDG state conventions (single instance per user environment, fixed file names; see §11.4), including pid/socket/log/cooldown/probe-cache files;
- OAuth account material stored in one unified typed store with restrictive file permissions; unlike Go/v1, the current Rust implementation must not split OpenAI and Antigravity OAuth material into separate files;
- config files containing env var names are not secrets, but any accidental literal API key should be masked in output;
- logs and status must mask API keys, bearer tokens, device codes, refresh tokens, and authorization headers;
- management socket must be local-user scoped and not exposed over TCP.

### 15.1 Testability and Isolation

为便于测试和多实例隔离，所有状态/缓存文件路径支持环境变量覆盖：

| 环境变量 | 覆盖范围 | 默认值 |
|---------|---------|--------|
| `LLM_PROXY_STATE_DIR` | 所有 state 文件（pid/socket/log/cooldowns/status-cache） | `~/.local/state/llm-proxy/` |
| `LLM_PROXY_CONFIG_DIR` | config 文件目录 | `~/.config/llm-proxy/` |

**使用场景**：
- **测试隔离**：测试用例使用独立的 state/config 目录，避免与运行中 server 冲突
- **多实例**：同一用户运行多个 llm-proxy 实例（不同端口/配置）
- **远程模式模拟**：创建极简 config（无 providers/models），配合独立 state 目录模拟远程场景

**示例**：
```bash
# 测试隔离
LLM_PROXY_STATE_DIR=/tmp/test-state llm-proxy status

# 远程模式模拟
mkdir -p /tmp/remote-state
cat > /tmp/remote-config.toml <<EOF
[server]
listen = "127.0.0.1:8989"
EOF
LLM_PROXY_STATE_DIR=/tmp/remote-state llm-proxy --config /tmp/remote-config.toml status
```

**限制**：
- OAuth 存储（`oauth_accounts.json`）目前不支持环境变量覆盖（位于 config 目录，2026-08-09 已修复：尊重 `$XDG_CONFIG_HOME`）
- 多实例需确保端口不冲突（`server.listen` 配置）

### 15.2 Ownership and Write Serialization（权威持有者与写串行化）

> 本方案经 codex + gpt-5.5 代码评审迭代（2026-08-03），整合 P0-P3 全部建议。
> 核心原则：**锁权威（flock 状态）与诊断信息（元数据）分离**——元数据永远只是提示，
> 判定持有状态只依赖 `try_lock` 结果。

#### 术语表

| 术语 | 含义 |
|------|------|
| 所有权锁（ownership lock） | `state_dir/ownership.lock` 上的 flock，跨进程排他 |
| 权威操作者（authoritative operator） | 当前持有所有权锁的进程（server 或 CLI） |
| 委托（delegation） | CLI 经 UDS 请求 server 执行持久化操作 |
| 独立模式（standalone mode） | CLI 无 server 时自己获取所有权并执行写操作 |
| stale-lock | 锁文件残留但无进程持有（flock 已释放）的状态 |

#### 设计目标

任何时刻仅有一个**权威操作者**能执行持久化操作（写 config/OAuth/cooldown），
从架构上消除"CLI 直写 + server 内存不一致"的竞争窗口（C1 根治的正解），
同时**不牺牲 server 可用性**（server 是常驻服务，优先级高于 CLI 临时操作）。

#### 核心机制：所有权锁

- 锁文件：`state_dir/ownership.lock`（flock 跨进程排他锁）
- **判定持有状态只用 `try_lock`（非阻塞 flock）**；文件存在 ≠ 被持有（stale-lock 不误判）
- **元数据仅用于诊断**（写入报错提示），不参与持有判定：
  ```json
  {
    "pid": 338320,
    "process_type": "server" | "cli",
    "started_at": 1785750000,
    "command": "llm-proxy provider add deepseek"
  }
  ```
- **元数据损坏/部分写入的处理**：读取失败时按"未知持有者"处理（保守失败），
  持有判定仍以 `try_lock` 结果为准，元数据读取失败不阻塞获取锁。
- **命令元数据清理**：只记录命令名和关键参数（如 provider id），
  不记录 API key、token、路径等敏感信息。
- **symlink/路径遍历防护**：锁文件路径解析前检查 state_dir 不是 symlink 链
  （复用 auth.rs 的 `validate_path_safety` 模式），防止恶意软链劫持锁文件。

#### server 生命周期持有

**server 在启动时获取所有权锁，并在整个生命周期持有**（直到 shutdown/退出）。
- 持有期间：CLI 的写操作必须委托（UDS），不能本地获取锁
- server 退出（正常 shutdown 或崩溃）→ flock 自动释放 → CLI 可获取

**server 启动流程（顺序固定）**：
```
1. 获取所有权锁（try_lock）
   ├─ 成功 → 持有（进入下一步）
   ├─ 持有者是 CLI → 等待（默认 10s，CLI 写操作毫秒级）
   │    显示："等待 CLI 写操作完成（pid=xxx）..."
   │    超时 → 报错 + 指引（见"僵局缓解"）
   ├─ 持有者是 server → 立即失败（防多实例）
   └─ 元数据不可读 → 保守等待 + 重试 try_lock
2. 获取锁后 → 读取 config / OAuth 账号到内存
3. 绑定监听端口 + UDS socket
4. 启动完成，进入服务循环
```
**顺序理由**：先获取锁再读配置，避免"CLI 正在写 config 时 server 读到半写状态"；
先锁后绑定端口，避免"端口已绑定但锁未获取"导致并发 server 短暂并存。

#### CLI/TUI 启动

**不持有锁**（只读操作可随时执行，不阻塞 server 启动）。
读命令（status/list/info）从 HTTP 委托（server 持有）或本地文件（无 server）读取，均不获取锁。

#### CLI/TUI 写操作（5 步流程）

每次写操作按固定流程决策（操作时点，非启动时点）：

```
1. detect_server（HTTP ping，§13 版本校验）
2. 有有效 server → 委托写（UDS）
3. 无有效 server → try_lock 所有权锁
4. 获取到锁：
   a. 写入 sanitized CLI 元数据
   b. 执行本地写事务（validate → atomic_write）
   c. 释放锁
5. 未获取到锁：
   a. 尽力读取持有者元数据（失败按"未知持有者"处理）
   b. 持有者是 server → 重试 detect_server 一次
        ├─ 发现 server → 委托写
        └─ 仍无 → 安全失败（不回退本地写）
   c. 持有者是 CLI → 失败 + 指引（另一个 CLI 正在写）
   d. 未知持有者 → 保守失败（不冒险本地写）
```

**关键：detect_server 与 try_lock 之间的竞态闭环**——CLI 检测无 server 后 try_lock，
若恰逢 server 启动（已获取锁），CLI 获取失败 → 步骤 5b 重试 detect_server →
发现 server → 委托。不会出现"CLI 检测无 server 但实际有 server"的窗口。

#### TUI 编辑器

- 编辑过程：只读预览，**不持有锁**
- 保存瞬间：按 5 步流程（委托 or try_lock）→ 写入 → 释放

#### 竞态时序图

**场景 A：CLI 写操作与 server 启动并发**
```
CLI:            Server:
| detect_server  |
|   ↓ (无 server)|
| try_lock       |
|   ↓            | try_lock（失败：CLI 持有）
| 本地写         | 等待 10s
| 释放锁         | try_lock（成功）
|                | 读 config → 绑定 → 启动
```
CLI 写操作毫秒级，server 等待后成功启动。

**场景 B：server 启动后 CLI 写操作**
```
Server:          CLI:
| try_lock(成功)  |
| 读 config       |
| 绑定端口        |
|                | detect_server（发现 server）
|                | 委托写（UDS）
| 执行持久化       |
```

**场景 C：CLI 获取锁后 server 才启动**
```
CLI:             Server:
| try_lock(成功)  |
| 本地写          | try_lock（失败：CLI 持有）
| 释放锁          | 等待 10s → try_lock(成功) → 启动
```
（= 场景 A，server 等待 CLI 写操作完成）

#### 僵局缓解：stale-lock 恢复 + 报错指引

**僵局场景**：持有者进程假死（存活但无响应），flock 不释放，锁无法自动获取。

**对策 1：报错指引**（可诊断 + 可自助）：
```
Error: 写入权被占用
  持有者: CLI (pid=12345), 启动于 17:13:22
  命令: llm-proxy provider add deepseek
  可能原因: 该进程卡死（写操作未在 10s 内完成）
  解决:
    1. 等待数秒后重试（CLI 写操作通常毫秒级）
    2. 若持续占用，确认持有者进程已退出
    3. 确认**没有任何 llm-proxy 进程在运行**后，可手动删除锁文件
       （正常自动化流程不删除锁文件；仅全部进程退出后的手动清理）
```

**对策 2：stale-lock 恢复（不自动删除锁文件）**：
- 获取锁失败时，读元数据中的 pid，检查进程是否存活（`kill(pid, 0)`）
- **进程不存在** → 锁实际已释放（flock 随进程退出自动释放）→ **直接重试 `try_lock` 应成功**
- **关键约束：正常自动化恢复流程永不删除 `ownership.lock` 文件**
  - 删除/重建锁文件有危险：另一个进程可能仍持有旧 inode（flock 语义下
    删除后新建文件会绕过原锁），或重建时产生竞争窗口
  - 正确做法：始终打开**规范锁路径**并 `try_lock`；成功获取锁后
    再覆盖/截断元数据（写元数据必须在持锁后）
- **进程存在** → 真被持有 → 走"报错指引"
- **手动删除锁文件**仅在以下情况下允许：确认**没有任何 llm-proxy 进程**在运行
  （server 与 CLI 均退出），作为最后的清理手段（此时 flock 已全部释放，
  删除仅清理残留文件，无竞争风险）

#### UDS 安全

| 项 | 要求 |
|----|------|
| state_dir 权限 | 0700（仅 owner） |
| UDS socket 权限 | 0600（仅 owner） |
| peer credential | 连接时通过 `SO_PEERCRED` 校验对端 uid = 本进程 uid（防其他用户/进程冒用） |
| socket 路径 | state_dir 下固定短路径，防 symlink 劫持 |

#### server 写事务模型

server 执行持久化（config_update/oauth_write/cooldown_clear）时的原子性保证：

```
1. validate（配置/数据校验，失败返回错误，不写盘）
2. atomic_write（临时文件 + rename 覆盖）
3. in-memory swap（更新内存状态：CoreState.config / OAuth 缓存）
4. generation 递增（内存状态代次 +1，供状态一致性校验/调试）
```

**server 崩溃于委托写入期间**：
- CLI 委托请求发出后 server 崩溃 → UDS 连接失败/响应中断 → CLI 收到错误
- **CLI 不回退本地写**（设计决策②：检测到 server 但调用失败 → 报错，防内存态/盘分裂）
- server 重启后从磁盘恢复（atomic_write 保证盘状态完整）

#### 持有者行为矩阵

| 操作 | server 持有 | CLI 独立持有 | 无持有（stale） |
|------|------------|-------------|----------------|
| CLI 写命令 | 委托（UDS） | 失败 + 指引 | try_lock 获取 → 本地写 |
| CLI 读命令 | HTTP 委托 / 本地读 | 本地读 | 本地读 |
| server 启动 | （N/A，本身持有） | 等待 10s → 获取 | try_lock 获取 → 启动 |
| 第二个 server 启动 | 立即失败 | 立即失败 | 获取 → 启动 |

#### 显式不变量

1. 任何时刻至多一个进程持有所有权锁（flock 保证）
2. server 运行期间，CLI 写操作只能委托，不能本地获取锁
3. 持久化写入总是 `atomic_write`（临时文件 + rename），与锁互补
4. 元数据永远不参与持有判定（只用于诊断提示）
5. 委托失败（server 不可达/崩溃）时，CLI 不回退本地写（设计决策②）
6. server 获取锁 → 读配置 → 绑定端口的顺序固定（防半写读取 / 并发并存）

#### 公平性 / 饥饿假设

- server 等待 CLI 的默认超时 10s，基于"CLI 写操作毫秒级"的假设
- 若 CLI 写操作异常超过 10s（卡死），server 报错并指引，不无限等待
- **不引入 server 启动意图标记（started intent marker）**——假设 server 启动
  等待 10s 足够覆盖 CLI 写操作窗口；如未来出现频繁竞争，再考虑意图标记
- 无锁的 CLI 读操作不参与竞争，可随时执行（不受饥饿影响）

#### 与现有机制的关系

| 机制 | 现状 | 改造后 |
|------|------|--------|
| `ConfigLock`（flock） | 写时抢锁（5s 超时），无元数据 | **所有权锁增强**：server 生命周期持有 + 元数据 + 类型区分 + 等待策略 |
| server 防多实例 | PID 文件（`refuse_if_running`） | 所有权锁（server 持有 → 失败）+ PID 文件（状态信息） |
| CLI 独立模式 | 写时抢 ConfigLock | 5 步流程（detect_server → try_lock → 写事务 → 释放） |
| CLI 委托 | server 运行时写命令走 UDS | 不变（委托优先） |
| server 内部写 | `Mutex<CoreState>`（进程内） | 不变（进程内 handler 并发仍需要） |

#### 设计原则

1. **server 生命周期持有**：server 整个生命周期持有所有权锁，CLI 写操作只能委托
2. **操作时点决策**：CLI/TUI 每次写操作时按 5 步流程重新判断（委托 or 本地持有）
3. **server 优先**：server 启动等待 CLI（10s，CLI 写操作毫秒级），不被 CLI 阻塞
4. **可诊断**：元数据 + 报错指引 + stale-lock 恢复，僵局可自助解决
5. **原子写**：所有持久化写入保持 `atomic_write` + validate + in-memory swap（写事务模型）
6. **锁权威与诊断分离**：判定只依赖 flock 状态，元数据仅提示

#### 多文件操作的事务语义

某些写操作涉及多个持久化目标（如 `provider remove` 同时改 config 和 OAuth logout、
`provider relogin` 先 logout 再 login）。需要明确事务语义：

- **语义：serialized + ordered + idempotent recovery**（非 all-or-nothing）
- 操作按固定顺序串行执行（先 config 后 OAuth，或反之，固定约定）
- 每步独立 `atomic_write`（单文件原子性由 atomic_write 保证）
- **失败恢复**：某一步失败后，已成功的步骤保持（不回滚），
  提供幂等的清理/重试路径（如"已删除 config 中 provider，但 OAuth 账号残留"——
  用户可重试 `provider remove` 或手动 `provider logout` 清理）
- **不在所有权锁内做跨文件回滚**（回滚本身是写操作，会引入新的失败点）

#### server 路径避免阻塞 async executor

- CoreState 的写操作（config_update/oauth_write/cooldown_clear）保持 `spawn_blocking` 模式
- **禁止**在 async handler 中 `core.lock().await` 后直接执行同步文件 IO（会阻塞 tokio worker）
- 模式：`tokio::task::spawn_blocking(move || core.blocking_lock().mutation())`
- 若未来 CoreState 写路径变重，可考虑专用 actor / 命令队列（当前 admin 写路径频率低，不必要）

#### 验收测试映射（对应 §16）

| 场景 | 验收测试 |
|------|---------|
| server 生命周期持有 | server 启动后 CLI 写操作委托（不获取锁） |
| server 等待 CLI | CLI 持锁时 server 启动等待 10s 后成功 |
| server 遇 server 持锁 | 第二个 server 启动立即失败 |
| CLI 5 步流程 | CLI 写操作在 server 前后启动切换委托/本地 |
| stale-lock | 残留锁文件但无 flock 不误判；持有者进程退出后可清理 |
| 委托失败不回退 | server 存活但 UDS 失败时 CLI 报错，不本地写 |
| 竞态闭环 | detect_server 与 try_lock 之间 server 启动 → CLI 重试发现 → 委托 |
| UDS 安全 | socket 权限 0600 + peer credential 校验 |
| 崩溃一致性 | server 委托写入中崩溃 → atomic_write 保证盘完整 |

**相关独立设计**：
- OAuth 登录流程（OpenAI Device Code Flow / Google OAuth）：[`docs/design/oauth-flows.md`](docs/design/oauth-flows.md)
- OAuth 凭据存储（统一 token store 与多账户）：[`docs/design/oauth-accounts-storage.md`](docs/design/oauth-accounts-storage.md)

## 16. Testing and Acceptance Plan

llm-proxy is complete for this design only when tests and E2E evidence cover the old feature set and the new CLI semantics.

Required test groups:

1. **Config validation tests**
   - all provider endpoint validation rules;
   - all model provider binding rules;
2. **Adapter unit tests**
   - request/response conversion for all supported protocol pairs;
   - streaming event conversion;
   - tool calls, tool results, reasoning content, images, and documents.
3. **Proxy routing tests**
   - provider selection per model/protocol;
   - native endpoint execution;
   - derived endpoint adapter chain execution;
   - fallback/cooldown behavior;
   - bad-request-block behavior.
4. **CLI tests**
   - default command starts background service;
   - `serve --foreground` runs foreground;
   - `status` does not probe network by default;
   - `status --probe` refreshes probe cache;
   - `shutdown` and `restart` lifecycle.
5. **TUI tests**
   - connect product selection and config writes;
   - auth login/status/logout state transitions with mocked OAuth servers.
6. **Launch tests**
   - Codex CLI, Codex Desktop, pi, Claude Code, Claude Desktop / Claude Code Desktop, Qwen Code config generation;
   - dry-run and home/path override behavior;
   - user-owned config preservation.
7. **Provider catalog tests**
   - every built-in product installs provider endpoint declarations for every protocol used by its default models;
   - every default model provider binding is backed by a same-protocol provider endpoint;
   - no catalog entry leaks real secrets.
8. **E2E tests**
   - mock upstream for each protocol pair;
   - real-provider smoke tests gated by environment variables;
   - isolated HOME/client config tests;
   - no default-status network traffic test.

Acceptance checklist for implementation:

- [ ] All built-in provider products in §10.2 can be represented in config, or are explicitly downgraded to custom/managed future work in this design document.
- [ ] All client launch workflows in §13 are implemented.
- [ ] `connect` CLI and TUI can install API-key, no-key, and OAuth-backed products.
- [ ] `provider` command/TUI can manage OAuth providers without leaking tokens.
- [ ] Default service start is background.
- [ ] Foreground service start uses `--foreground`.
- [ ] Default `status` is cache/local-state only.
- [ ] Real upstream status refresh uses `--probe`.
- [ ] All model provider bindings are validated against same-protocol provider endpoint declarations.
- [ ] Derived endpoints make protocol conversion explicit through `derive_from` declarations resolved via the adapter registry.

## 17. Applying Existing Architecture Decisions

The ADRs in [`../decisions/`](../decisions/) remain design input unless this document or a later ADR explicitly supersedes them.

### 17.1 ADR-001: direct protocol-to-protocol conversion

This design preserves [`ADR-001`](../decisions/001-direct-protocol-conversion.md): protocol conversion uses direct A → B modules, not a unified intermediate protocol format.

Provider endpoint declarations do **not** introduce a canonical internal message format. They only declare how a provider can serve a requested client protocol:

```text
provider endpoint field: anthropic
  derive_from = openai_chat
  # adapter inferred from pair: anthropic-from-chat-completions
```

The adapter inferred above is a direct conversion module from the source endpoint protocol to the target endpoint protocol. It must not convert through a shared `LlmProxyMessage`, `UniversalRequest`, or Chat-as-hub representation.

Implementation consequences:

- Each protocol pair remains its own module and test surface.
- Request conversion, response conversion, streaming conversion, and error mapping can share small utilities only when they are the same change unit.
- Adding a new protocol adds explicit adapters for the required pairs; it does not mutate a global intermediate schema.
- Endpoint resolution may compose configuration metadata, but payload transformation remains direct pairwise conversion.

This matters especially for llm-proxy because the endpoint declarations can make conversion look generic. The declarations are generic routing metadata; the conversion implementation is deliberately not generic.

### 17.2 ADR-019: do not DRY across independent change units

Apply [`ADR-019`](../decisions/019-duplication-boundary-tradeoff.md) during implementation.

Examples:

| llm-proxy area | Same change unit? | Decision |
|---|---:|---|
| `responses-from-chat` and `chat-from-responses` payload conversion | No; opposite protocol directions evolve independently | Keep separate modules/tests. |
| non-streaming conversion and streaming SSE conversion for the same pair | Usually no; event protocols evolve separately from JSON body shape | Keep separate dispatch/conversion paths unless proven same change unit. |
| provider-specific thinking adapters for DeepSeek, Qwen, Stepfun, Gemini | No; provider APIs evolve independently | Do not merge just because current code looks similar. |
| endpoint validation of `native` url presence | Yes; one config invariant | Centralize validation helper. |
| Anthropic version header value | Yes; one protocol constant | Centralize constant. |

The code review rule is: ask whether two similar code blocks change for the same external reason. If not, duplication is acceptable and often preferred.

### 17.3 ADR-018 is superseded by explicit endpoint declarations

[`ADR-018`](../decisions/018-model-all-protocols-ingress.md) captured a valid product goal: when semantics can be preserved, users should be able to use the same frontend model from Chat, Responses, and Anthropic clients.

Its old runtime rule is **not** carried forward: llm-proxy must not infer protocol support from mere provider presence. Protocol support is explicit:

```text
model.<protocol>_providers includes provider P
  -> provider P declares the same protocol endpoint field
  -> endpoint is native, or derived from a declared native source endpoint
```

This supersedes the ADR-018 inference rule while preserving the UX goal as a catalog/connect generation preference:

- catalog/connect may generate full ingress coverage when adapters preserve required semantics;
- runtime/config validation never invents missing protocol bindings;
- if image/document/tool/reasoning semantics cannot be represented safely, the catalog must omit that protocol binding or mark it advanced with a clear reason;
- model listing reflects explicit protocol-specific bindings, not inferred global availability.

### 17.4 Other ADRs to carry forward

| ADR | Application |
|---|---|
| [`ADR-002`](../decisions/002-server-side-reasoning-cache.md) | Preserve equivalent reasoning cache/state for multi-turn tool workflows where upstream providers require reasoning content. |
| [`ADR-003`](../decisions/003-config-driven-provider-models.md) | Keep provider/model behavior config-driven; the provider endpoint fields refine this rather than replacing it. |
| [`ADR-004`](../decisions/004-codex-responses-wire-api.md) | Codex launch must use Responses wire API against local proxy; Chat Completions is only an upstream protocol behind conversion, not a Codex frontend alternative. |
| [`ADR-005`](../decisions/005-provider-cooldown-policy.md) | Preserve cooldown categories, persistence, and fallback interaction. |
| [`ADR-007`](../decisions/007-simple-model-provider-configuration.md) | Carry forward the simple user mental model, but not the old schema. A model answers client ID, per-protocol provider order, upstream model IDs, and model capabilities/reasoning metadata. |
| [`ADR-008`](../decisions/008-proxy-must-protect-upstream.md) | Preserve Bad Request Block / upstream protection; proxy cannot assume clients behave safely. Local deterministic validation errors remain separate from upstream 400 protection. |
| [`ADR-009`](../decisions/009-kimi-code-anthropic-only.md) | Treat as provider catalog/research input only. Do not add runtime Kimi special cases; represent reliable endpoints through explicit native/derived endpoint declarations. |
| [`ADR-010`](../decisions/010-thinking-format-extension.md) | Preserve provider-specific thinking formats as explicit extension points, but parse them into Rust enums and attach them to native endpoint `compat`, not provider-level string switches. |
| [`ADR-013`](../decisions/013-features-at-model-level.md) | Carry forward model-level `image_input`/`document_input` capability declarations. Supersede `tool_call_reasoning` as a runtime trigger; reasoning-content injection is driven by native endpoint `compat.requires_reasoning_content_on_assistant_messages`. |
| [`ADR-014`](../decisions/014-self-implement-upstream-client.md) | Upstream transport/client implementation is orthogonal to protocol conversion; do not hide conversion inside HTTP client code and do not introduce official SDK path assumptions. |
| [`ADR-015`](../decisions/015-oauth-polling-safety-margin.md) and [`ADR-016`](../decisions/016-token-refresh-retry-singleflight.md) | M7 OAuth keeps safe polling and singleflight token refresh semantics; 401 refresh behavior must not poison provider cooldown. |
| [`ADR-017`](../decisions/017-probe-reuses-proxy-body-logic.md) | `status --probe` probes should reuse the same execution-plan/egress body construction as normal proxy requests, including reasoning compat, without making default status online. |
| [`ADR-017 secure defaults`](../decisions/017-secure-defaults-user-override.md) | Keep safe defaults plus user override for user-authored config; OAuth/token caches are proxy-authored secrets and should use stricter owner-only storage. |
| [`ADR-020`](../decisions/020-antigravity-trainable-never.md) | Use as a negative constraint: do not invent upstream request fields from guesses. Endpoint compat fields must be backed by docs, experiments, or provider research. |
| [`ADR-021`](../decisions/021-unify-toml-library-and-roundtrip-boundary.md) and [`ADR-022`](../decisions/022-rust-v2-config-editing-boundary.md) | Config edits use `toml_edit`/round-trip-preserving boundaries; do not introduce tree-sitter as default config editor. |

### 17.5 When this design intentionally differs from an ADR

If the design needs to differ from an accepted ADR, the difference must be documented as one of:

1. a new ADR that supersedes the old decision;
2. an amendment section in the old ADR;
3. an explicit section in this design document explaining why equivalent behavior is still preserved.

Do not make silent changes to accepted architecture decisions.


## 18. AI Iterative Development Workflow

This section is the executable development loop for AI agents implementing llm-proxy. It keeps the workflow in the main implementation design so the design, acceptance criteria, and stop conditions live in one file.

### 18.1 Core Rule

Each development cycle must move one clearly bounded slice from **specified** → **implemented** → **verified**.

Do not start from code edits when the slice lacks:

1. a referenced design section;
2. concrete acceptance checks;
3. a Go/v1 capability impact statement;
4. a rollback/failure condition.

### 18.2 Loop State Machine

Every slice follows this state machine:

```text
Select slice
  -> Read design + relevant ADR/source evidence
  -> Write slice acceptance checklist
  -> Implement minimal coherent change
  -> Run local verification
  -> Audit against checklist
  -> Update docs/matrix if behavior changed
  -> Mark slice complete or split remaining work
```

A slice is complete only when its acceptance checklist is satisfied by current evidence, not by intent.

### 18.3 Required Inputs per Slice

Before implementation, the agent must identify:

| Input | Required evidence |
|---|---|
| Design anchor | Section in this document. |
| ADR anchor | Relevant ADR(s), especially ADR-001 for conversion work. |
| Current code anchor | Rust file(s) to change and current behavior. |
| Test anchor | Existing or new tests that will prove the slice. |
| Out-of-scope | Explicitly named behavior not touched by this slice. |

If any input is missing, the first task is to add the missing spec/checklist, not to code.

### 18.4 Standard Slice Template

Each implementation issue/task should use this template:

```md
# Slice: <name>

## Goal
One paragraph describing the behavior to make true.

## Design anchors
- spec.md#...
- ADR-...

## In scope
- ...

## Out of scope
- ...

## Acceptance checks
- [ ] Code behavior: ...
- [ ] Config/schema behavior: ...
- [ ] CLI behavior: ...
- [ ] Tests: command and expected coverage
- [ ] Docs/matrix updated if behavior changed

## Stop conditions
- Stop and split if ...
- Stop and ask for decision if ...
```

### 18.5 Milestone Order

AI agents should implement in this order unless a human explicitly changes priority.

#### M0 — Spec normalization gate

Goal: make the spec executable enough for coding.

Acceptance:

- [ ] Intentional differences are listed.
- [ ] Parity matrix exists.
- [ ] Current provider catalog research exists.
- [ ] ADR inheritance is documented.
- [ ] This iterative workflow exists.
- [ ] Open product decisions are either resolved or assigned conservative default policy.

Exit condition: a new slice can point to concrete design and ADR/source-evidence anchors.

#### M1 — Config schema and validation

Goal: implement provider endpoint field config schema and full validation before runtime changes.

Acceptance:

- [ ] TOML schema supports direct protocol endpoint fields on providers (`openai_chat`, `openai_responses`, `anthropic`, `antigravity`).
- [ ] Model protocol provider lists exist.
- [ ] Every model binding must reference a provider that declares the corresponding protocol endpoint field.
- [ ] Native endpoints require complete upstream `url`.
- [ ] Derived endpoints use `derive_from`; adapter is uniquely determined by the protocol pair via the registry.
- [ ] Derived endpoint `derive_from` references an existing protocol field on the same provider, and the registry contains an adapter for the source/target pair.
- [ ] Endpoints configuring both `url` and `derive_from`, or neither, are rejected.
- [ ] Derived endpoints must reference a native endpoint directly; multi-hop chains are rejected.
- [ ] All model bindings are validated, including models no client ever requests.
- [ ] Tests cover valid native, valid derived, missing endpoint, wrong protocol, missing `derive_from` target, pair missing from the adapter registry, multi-hop derived chain, both/neither of `url` and `derive_from`, non-default invalid model, and reasoning metadata validation (empty/duplicate levels, `default_reasoning_level` outside `supported_reasoning_levels`).

Stop conditions:

- Stop if TOML compatibility requires a schema decision not in the design.
- Stop if a provider product needs unresolved endpoint assumptions for default config.

#### M2 — Resolver and execution plan

Goal: centralize request resolution before expanding protocol logic.

Acceptance:

- [ ] Resolver maps protocol/requested model to a provider binding; unknown model IDs and models lacking a binding for the request protocol produce explicit 4xx errors with no default substitution.
- [ ] Resolver returns an execution plan containing frontend protocol/model, provider ID, upstream model, endpoint chain, native URL, auth strategy, and the effective compat of the target native endpoint.
- [ ] Fallback selection is separated from protocol conversion.
- [ ] Cooldown keys are defined and tested.
- [ ] Request-content capability filtering follows §5: image/document requests against models without the matching feature fail locally with a deterministic 4xx before provider selection, with no cooldown and no fingerprint accounting.
- [ ] Status/probe code can use resolver metadata without sending requests by default.

Stop conditions:

- Stop if fallback/cooldown key semantics are ambiguous.
- Stop if a conversion case appears to require multi-hop derivation; multi-hop is not allowed, so such a case needs a direct adapter for the pair instead.

#### M3 — Direct adapter modules

Goal: implement protocol conversion per ADR-001 with no intermediate protocol format.

Acceptance:

- [ ] Each adapter is direct source protocol → target protocol.
- [ ] No universal internal message/request format is introduced.
- [ ] Request, response, streaming, and error mapping are tested separately where applicable.
- [ ] Tests are ported from the Go/v1 converter behavior that this design preserves.
- [ ] Each adapter has a documented compatibility contract listing converted, degraded/dropped, and local-reject fields.
- [ ] Tests prove Go/v1 parity for converted/degraded cases that existed in Go/v1.
- [ ] Tests prove local rejects remain limited to documented cases.
- [ ] Tool calls, tool results, reasoning, image input, and document/PDF input are covered where supported.

Stop conditions:

- Stop if implementation starts creating a canonical `UniversalMessage` / `LlmProxyMessage` / Chat-as-hub pipeline.
- Stop if conversion loss needs a product decision not documented.

#### M4 — Proxy runtime, fallback, cooldown, protection

Goal: use resolver + adapters in live request forwarding.

Acceptance:

- [ ] Native endpoint forwarding works.
- [ ] Derived endpoint forwarding uses direct adapters.
- [ ] Fallback order follows model protocol provider list.
- [ ] Cooldown policies match the categories in §10.5.8.
- [ ] Bad Request Block protects upstream before provider selection.
- [ ] Stream replay boundary matches §10.5.5.
- [ ] Error mapping preserves client protocol shape.
- [ ] OAuth 401 refresh retry is singleflight where applicable.

Stop conditions:

- Stop if a runtime behavior conflicts with this design document or an ADR.

#### M5 — CLI lifecycle and status

Goal: implement intentional CLI differences.

Acceptance:

- [ ] `llm-proxy` starts background service by default.
- [ ] `llm-proxy serve --foreground` runs foreground.
- [ ] `shutdown` gracefully stops running service.
- [ ] `restart` uses current CLI/config arguments and starts background service.
- [ ] `status` is local/cache-only by default.
- [ ] `status --probe` performs real probes and updates cache.
- [ ] `status` shows, per model/protocol, each provider binding and whether its endpoint is native or derived.
- [ ] `provider list/info/refresh` and `model list/info` exist with default offline behavior; explicit provider/status refresh behavior is opt-in.
- [ ] `llm-proxy provider` and `llm-proxy model` open TUI only in interactive terminals; non-interactive invocations without subcommands return usage errors.
- [ ] Tests prove default `status`, `provider list/info`, `model list/info`, and initial provider/model TUI loads do not hit network.

Stop conditions:

- Stop if pid/socket/log path rules are underspecified.

#### M6 — Provider catalog

Goal: implement corrected current provider catalog for fresh llm-proxy configs and provider/model management.

Acceptance:

- [ ] Catalog uses product/account provider IDs with protocol endpoint fields.
- [ ] Kimi defaults separate `kimi-open-platform-cn`, source-backed `kimi-code-api`, and managed `kimi-code-managed`; `k3-256k` is the Kimi Code daily 256K route and `k3` is the high-context route.
- [ ] Stepfun defaults use `.ai` hosts.
- [ ] Unverified native endpoints use derived/custom/advanced policy.
- [ ] Catalog tests prove every built-in model binding has same-protocol provider endpoint.
- [ ] `init`, `connect`, and `provider add` use the catalog without rewriting legacy config schemas; only the explicit mature-product model selection step in `provider add`/`connect` may create model bindings.

Stop conditions:

- Stop and downgrade to custom/advanced if a product endpoint cannot be source-backed.

#### M7 — Provider and model management

Goal: implement the provider/model management workflows in §12.

Acceptance:

**Provider command:**
- [ ] `provider add PRODUCT --api-key-env ENV` creates API key providers non-interactively.
- [ ] `provider add` creates no-key providers (`--no-api-key`).
- [ ] `provider add` creates custom endpoint providers (`--type` + `--endpoint-url` + `--api-key-env`).
- [ ] `provider add` creates OAuth providers with required OAuth login flow, including when `--model` is supplied non-interactively.
- [ ] `provider add` interactive mode (no `PRODUCT`) shows product picker.
- [ ] `provider add` / `connect` for mature catalog products requires model selection before writing; non-interactive mode supports repeated `--model MODEL_ID`; selecting none/omitting `--model` is an error for mature products.
- [ ] Mature product model selection uses Go/v1-style stable catalog template IDs such as `gpt-5.5-openai-subscription-lp`; when the actual provider ID differs from the product default, copied model bindings are rewritten to that provider ID. `display_name` may provide common human-readable names.
- [ ] `provider copy SOURCE NAME --api-key-env ENV` copies API-key provider config into a new provider ID only after the credential env var is selected/confirmed.
- [ ] `provider copy SOURCE NAME` for OAuth providers requires a fresh login for the new provider ID and never copies OAuth tokens/secrets.
- [ ] `connect` works as alias for `provider add` with identical flags.
- [ ] `provider list` lists all providers with type, account/auth mode, configured state.
- [ ] `provider info X` shows provider details including usage for OAuth providers.
- [ ] `provider info X` for API key providers shows info without usage area.
- [ ] `provider info` without `NAME` shows summary for all providers.
- [ ] `provider login X` performs OAuth login.
- [ ] `provider logout X` clears token, preserves config.
- [ ] `provider relogin X` performs logout + login.
- [ ] `provider refresh X` refreshes OAuth token.
- [ ] `provider remove X` deletes an unreferenced provider with secondary confirmation; if models still reference it, the command fails with an actionable references list and does not edit model bindings.
- [ ] `provider remove X --force` skips confirmation only; it still refuses to delete providers referenced by model bindings.
- [ ] `provider reset-usage X` implements tiered confirmation (used up → direct; ≥50% → confirm; <50% → confirm + warning).
- [ ] `provider reset-usage X --force` skips all confirmations.
- [ ] `provider reset-usage X` for unsupported provider shows "does not support usage reset".
- [ ] Provider TUI layer 1 (list) displays providers with status, supports `[+]` add, `[u]` refresh usage, `[Enter]` detail.
- [ ] Provider TUI layer 2 (detail) displays basic info + usage area (window/balance/none), supports `[r]` reset with tiered confirmation.
- [ ] Provider TUI state machine implements all states: List, AddSelectProduct, AddConfigure, CopyConfigure, ProviderDetail, UsageConfirm, ResetConfirm, ResetWarnLowUsage, ResetLoading, ResetResult.

**Model command:**
- [ ] `model list` lists all models with context window, thinking status, features, provider count.
- [ ] `model info X` shows model details including all protocol-specific provider bindings.
- [ ] `model add X --context-window N --max-output M` creates a new frontend model.
- [ ] `model add X --copy-from SOURCE_MODEL` copies the existing model config into a new independent model entry without renaming the source.
- [ ] Model TUI copy opens the same edit form as add, pre-filled from the source model, with default target name `SOURCE-<num>` using the first unused positive integer.
- [ ] `model remove X` deletes a frontend model with secondary confirmation and does not delete providers/credentials.
- [ ] `model remove X --force` skips confirmation.
- [ ] Model rename is not available in CLI or TUI.
- [ ] `model set X --context-window N` updates context window.
- [ ] `model set X --max-output N` updates max output tokens.
- [ ] `model set X --thinking-level LEVEL` updates default reasoning level.
- [ ] `model set X --enable-thinking` and `model set X --disable-thinking` toggle thinking.
- [ ] `model set X --enable-feature FEATURE` and `model set X --disable-feature FEATURE` toggle features.
- [ ] `model provider add X --type TYPE --provider NAME --upstream-model MODEL` adds provider binding.
- [ ] `model provider add` without `--upstream-model` defaults to model name.
- [ ] `model provider add` without `--type` errors with valid types list.
- [ ] `model provider remove X --type TYPE --provider NAME` removes provider binding.
- [ ] Multiple `model set` flags in one command apply atomically.
- [ ] Model TUI list supports add/delete/copy model flows; detail view displays model fields as editable peers, including `openai_chat_providers`, `openai_responses_providers`, and `anthropic_providers`.
- [ ] Selecting an `xx_providers` field opens a protocol-scoped provider-list editor centered on the already-configured provider list in fallback order; it can add only configured providers with that endpoint, excludes already-selected providers, edits upstream model names, removes bindings with secondary confirmation defaulting to cancel/do-not-delete, and reorders fallback priority within that field only. TUI saves are reflected in the user config file, which remains portable to another machine when referenced env vars/auth state are supplied.

**Usage API (OAuth providers):**
- [ ] `query_usage()` fetches usage from ChatGPT backend API with OAuth access_token.
- [ ] `consume_reset()` consumes one reset credit with idempotency key.
- [ ] `UsageStatus::reset_confirm_level()` correctly determines confirmation level based on usage state.
- [ ] Usage types correctly parse plan_type, rate_limit windows, credits, reset_credits_available.

**General:**
- [ ] Provider commands never implicitly attach providers to models; model bindings change only through `model` commands/direct config edits or through the explicit mature-product model selection step in `provider add`/`connect`.
- [ ] `auth` command is deleted; all functionality accessible through `provider`.
- [ ] Secrets/tokens/API keys are never printed in any command output.
- [ ] All write operations are atomic (validate first, write once, rollback on failure).

Stop conditions:

- Stop if an OAuth product's auth flow is not specified; do not fake it with stale API endpoint defaults.
- Stop if ChatGPT backend API behavior differs from source analysis; verify with actual OAuth token before implementation.

#### M8 — Launch clients

Goal: implement all launch workflows in §13.

Acceptance:

- [ ] Codex launch writes config and model catalog for Responses.
- [ ] pi launch writes/merges OpenAI Chat models config.
- [ ] Claude Code launch writes Anthropic local endpoint settings and slot variables.
- [ ] Claude Desktop launch writes third-party/Gateway config and profile mappings.
- [ ] Qwen Code launch writes OpenAI-compatible provider settings.
- [ ] All launch commands preserve user-owned fields and never write upstream secrets.
- [ ] Dry-run and home/path override behavior is covered.

Stop conditions:

- Stop if a client behavior changed upstream and 内部调研文档缺失.

#### M9 — Final release gate

Goal: prove llm-proxy satisfies this design document.

Acceptance:

- [ ] `cargo fmt -- --check`
- [ ] `cargo clippy -- -D warnings`
- [ ] `cargo check`
- [ ] `cargo test`
- [ ] Config/schema tests pass.
- [ ] Adapter tests pass.
- [ ] Proxy/runtime tests pass.
- [ ] CLI lifecycle tests pass.
- [ ] TUI tests pass or have documented manual/mock coverage.
- [ ] Launch tests pass.
- [ ] Catalog tests pass.
- [ ] Mock E2E covers every protocol family.
- [ ] Env-gated real provider smoke tests are documented and skipped safely without keys.
- [ ] Parity matrix has no uncovered row except explicit future-work decisions.

Exit condition: release candidate passes a full audit against this design document.

### 18.6 Per-Cycle Verification Commands

Default local verification:

```bash
cargo fmt -- --check
cargo clippy -- -D warnings
cargo check
cargo test
```

When touching docs only:

```bash
git diff --check
```

When touching CLI lifecycle or runtime networking:

```bash
cargo test service
cargo test status
cargo test proxy
```

When touching config/catalog:

```bash
cargo test config
```

When touching launch clients:

```bash
cargo test codex
cargo test clients
```

The exact test filters may change as modules are renamed. If a command no longer matches any tests, update this workflow or the module names in the same slice.

### 18.7 Completion Audit for AI Agents

Before marking a slice complete, the agent must answer:

1. What intentional difference, if any, does this slice implement?
2. Which required behavior does this slice implement?
3. Which ADR constrains this slice?
4. Which tests prove the behavior?
5. Which design sections or supporting docs changed?
6. What remains out of scope?

A slice is not complete if any acceptance item is only supported by a claim without code, test, or documented decision evidence.

### 18.8 Failure and Split Rules

Split the slice when:

- implementation touches unrelated milestones;
- adapter work starts requiring new schema decisions;
- provider catalog work starts requiring live product research;
- TUI work requires runtime management changes;
- tests reveal a behavior mismatch outside the slice scope.

Stop for human/design decision when:

- a provider product is no longer publicly documented;
- native endpoint support is unclear and derived behavior may be lossy;
- implementing equivalent behavior requires storing new secrets/tokens;
- an accepted ADR appears to conflict with a desired llm-proxy change.

### 18.9 Documentation Update Rule

Every slice that changes behavior must update at least one of:

- `spec.md`
- `rust-v2-current-provider-catalog.md`
- a new ADR or amendment

If behavior changes but no document changes, the agent must explicitly justify why the existing design already covered it.

### 18.10 Agent Handoff Contract

When this design is handed to an AI development agent, the agent must treat it as an executable contract, not background reading. The agent should produce work in bounded slices and must not claim completion by broad implementation intent.

For every cycle, the agent response should include:

1. **Selected slice**: the milestone and exact behavior being implemented.
2. **Anchors**: design section, ADR/source evidence or explicit note that none applies.
3. **Acceptance checklist**: copied or specialized from this document before coding starts.
4. **Evidence**: tests, commands, code paths, and document updates after implementation.
5. **Residual gap**: anything intentionally left incomplete.
6. **Next slice recommendation**: the smallest next bounded step.

The handoff is valid only if another agent can continue from the previous cycle's evidence without re-discovering the whole codebase.

### 18.11 Done Definitions

llm-proxy uses three levels of done. Agents must name which level they are claiming.

| Level | Meaning | Required evidence | Allowed remaining work |
|---|---|---|---|
| Slice done | One bounded behavior is implemented and verified. | Slice acceptance checklist, relevant tests, design update when behavior changed. | Other slices and unrelated design gaps. |
| Milestone done | All acceptance checks under one M0-M9 milestone are satisfied. | All slice evidence for that milestone plus a milestone audit against this document. | Later milestones only. |
| Rewrite done / release candidate | llm-proxy satisfies this design document except documented intentional future work. | M9 acceptance, full design audit, mock E2E, skipped-safe real-provider smoke plan. | Only explicitly documented post-release/future-work items. |

An agent must not mark the overall rewrite complete while any requirement in this design document is uncovered, ambiguous, or only supported by a TODO.

### 18.12 Acceptance Evidence Rules

Evidence must be concrete and reproducible:

- Prefer automated tests over manual claims.
- For CLI behavior, include command examples and expected result shape.
- For config behavior, include valid and invalid TOML examples or tests.
- For converter behavior, include request, response, streaming, and error-shape tests when relevant.
- For provider catalog behavior, include validation proving each default model binding points to a same-protocol endpoint.
- For auth/secrets behavior, include evidence that tokens and API keys are not printed in logs/status/errors.
- For intentionally incomplete behavior, update this design document with owner/status/future-work reason instead of hiding the gap.

Claims such as "implemented", "supported", or "compatible" are not acceptance evidence unless tied to tests, code paths, or documented decisions.

### 18.13 Default Next-Slice Selection Policy

If the human does not specify the next slice, choose the first milestone with an unchecked acceptance item in M0-M9 order. Within a milestone, prefer the slice that:

1. closes a user-visible capability gap;
2. has clear local tests;
3. does not require unresolved provider product research;
4. does not require credentials or paid real-provider calls;
5. reduces future implementation ambiguity.

If two candidate slices conflict, stop and ask for a human decision instead of silently choosing a product direction.

## 19. Status Command Design (2026-08-01)

### 19.1 Core Principles

**Read/Write Separation Architecture**:
- **Write operations** (provider/model CRUD, OAuth token writes): UDS only (local management channel)
- **Read operations** (status queries, launch data retrieval): HTTP public endpoints (same port as forwarding service)

### 19.2 Two Scenarios with Different Behaviors

#### Scenario 1: CLI and Server on Different Machines/Containers (Remote Mode)

**Environment**:
- CLI in container, Server on host machine
- CLI config only has `server.listen` (HTTP public endpoint address)
- CLI **cannot directly access providers** (no API keys, no credential files)
- CLI can only communicate with Server via HTTP public endpoints

**Behavior**:
- No flag: Get real-time status from Server HTTP endpoint (active provider list + Server cached probe results)
- `--probe`: **Request Server to execute probe** (CLI cannot probe itself, must delegate to Server)

#### Scenario 2: CLI and Server on Same Machine (Local Mode)

**Environment**:
- CLI and Server both on host machine (or same container)
- CLI can access Server's UDS (management channel) + HTTP public endpoints
- CLI **has API keys and credentials** (config.toml has `api_key_env` references)

**Behavior**:
- **Server running**:
  - No flag: Get real-time status from Server (via UDS or HTTP)
  - `--probe`: **Request Server to execute probe** (delegation mode, same as Scenario 1)

- **Server not running**:
  - No flag: Read local cache (status_cache.json)
  - `--probe`: **CLI executes probe itself** (independent mode, has API keys)

### 19.3 Probe Logic

**Core Insight**: **Ongoing successful requests = already probed successfully** (live evidence)

```text
For each provider:
├─ Has ongoing successful request?
│   └─ Yes → Skip (consider probe successful)
└─ No?
    ├─ Server running → Request Server to probe
    ├─ Server unreachable + has credentials → CLI probes itself
    └─ Server unreachable + no credentials → Read cache (cannot probe)
```

**"Needs probing" definition**: Providers without ongoing successful requests.

### 19.4 Flag Semantics

| Flag | Behavior |
|------|----------|
| No flag | Show current status (prefer real-time from Server, fallback to cache) |
| `--probe` | Probe providers "**without ongoing successful requests**" |

**No `--refresh` retained** (clean slate design, no legacy baggage).

### 19.5 Implementation Points

**Server Side**:
- Maintain "active provider list" (in-memory, records providers with successful forwarding + timestamps)
- HTTP read-only endpoint: `/admin/status` (returns active list + cached probe results)
- HTTP read-only endpoint: `/admin/status/probe` (triggers probe, returns results)

**CLI Side**:
- Detect mode (remote/local + Server availability)
- `--probe`: Get active list → Trigger probe for inactive providers (delegate to Server or execute independently)

### 19.6 Concurrent Probe Anti-Abuse (True Singleflight)

**Core Principle**: **Three-layer judgment + (provider, model) pair-level true Singleflight**

#### Three-Layer Judgment Flow

```text
Request probe (provider_id, model_id)
│
├─ L1: Active provider check (30s TTL)
│   └─ Yes → Don't probe, return "Active"
│
├─ L2: Cache result check (5s window)
│   └─ Has valid cache → Don't probe, return cached result
│
└─ L3: Singleflight merge
    ├─ Same (provider, model) probing in progress → Wait and share result
    └─ No probe in progress → Execute probe, notify waiters
```

#### Data Structure

```rust
// Key: (provider_id, model_id) pair
struct ProbeFlight {
    in_progress: bool,
    result: Option<ProbeResult>,
    waiters: Vec<oneshot::Sender<ProbeResult>>,
    last_probed: Option<Instant>,  // For 5s window judgment
}

// In AppState
status_probe_flights: Arc<tokio::sync::Mutex<BTreeMap<(String, String), ProbeFlight>>>
```

#### Timeout Strategy

| Dimension | Setting |
|-----------|---------|
| **Probe timeout** | 30 seconds (reqwest client timeout) |
| **Waiter timeout** | 30 seconds (same as probe timeout) |
| **Timeout status** | `ProbeResult::Timeout`, displayed as "Timeout/Unreachable" |

#### Concurrency Strategy

| Dimension | Strategy |
|-----------|----------|
| **Different providers** | **Concurrent** (improve efficiency) |
| **Different models of same provider** | **Serialized** (avoid triggering RPM) |
| **Same (provider, model)** | **Singleflight merge** (avoid duplication) |

#### Unified Entry Layer (ProbeCoordinator)

**DRY Principle**: Create independent `src/probe_coordinator.rs` module, both Server and CLI reuse same code.

```rust
pub trait ProbeState: Send + Sync {
    fn is_active(&self, provider_id: &str, ttl: Duration) -> bool;
    fn get_cached(&self, key: &(String, String), window: Duration) -> Option<ProbeResult>;
    fn update_cache(&mut self, key: &(String, String), result: ProbeResult);
    fn record_active(&mut self, provider_id: &str);
}

pub struct ProbeCoordinator<S: ProbeState> {
    state: Arc<tokio::sync::RwLock<S>>,
    flights: Arc<tokio::sync::Mutex<BTreeMap<(String, String), ProbeFlight>>>,
}
```

**Server Implementation**: `ServerProbeState` (in-memory active providers + disk cache)
**CLI Implementation**: `CliProbeState` (local cache only)

### 19.7 Probe Granularity Analysis (Provider-level vs Model-level)

- **Analysis**:
  - **Model-level probe**: Send probe for each model bound to Provider, causes probe requests to upstream to multiply, easily triggers 429 rate limit, models usually share same API Key and Base URL.
  - **Provider-level probe**: Verify network connectivity, API Key authentication, and main API routes for each Provider. For individual model temporary overload/unavailability, rely on framework's existing **Cooldown & Fallback retry mechanism** for real-time traffic switching during actual calls.
- **Decision**: Probe granularity remains **Provider-level** (covers representative protocol endpoints), ensuring low latency and low resource consumption.

**Note**: Singleflight merge granularity is **(provider, model) pair-level** (§19.6), but actual probe execution is Provider-level.

### 19.8 Probe Result Flush Responsibility and Atomicity

- **Server running scenario**: Probe results written to disk by **Server uniformly** (updates `status_cache.json`), follows Core layer's "single writer" principle.
- **Server not running scenario (CLI offline)**: When CLI executes Probe itself, must use `crate::config_edit::atomic_write` (write temp file then rename to overwrite) for atomic disk writes, preventing concurrent writes from corrupting JSON.

### 19.9 Active Provider Live Evidence TTL Tuning (30 seconds)

- **Decision**: Set live evidence default TTL effective window to **30 seconds**.
- **Rationale**: Upstream Provider access rate limits (like RPM) typically use minute (60s) as basic refresh cycle. Setting live evidence window to half of rolling refresh cycle (30s) achieves optimal balance between "ultra-fast response/skip redundant probes" and "timely detection of service failures".

### 19.10 Status Prompts and User Feedback

**Core Principle**: Let users clearly know data source and limitations.

#### Prompt Symbols

- `ℹ` = Normal info (real-time data)
- `✓` = Successfully executed probe
- `⚠` = Degraded mode (cached data, may be outdated)
- `✗` = Cannot get data (error)

#### Scenario Matrix

| Scenario | Server | Cache | --probe | Behavior | Prompt |
|----------|--------|-------|---------|----------|--------|
| Local + Server running | ✅ | - | No | Server real-time | `ℹ Data from local Server (real-time)` |
| Local + Server running | ✅ | - | Yes | Server probe | `✓ Requested Server to probe inactive providers` |
| Local + Server not running + has cache | ❌ | ✅ | No | Read cache | `⚠ Data from local cache (Server not running)` |
| Local + Server not running + has cache | ❌ | ✅ | Yes | **CLI probe (ignore cache)** → Success | `✓ Executed local probe` |
| Local + Server not running + has cache | ❌ | ✅ | Yes | **CLI probe (ignore cache)** → Fail → Fallback cache | `⚠ Local probe failed, fell back to cache data` |
| Local + Server not running + no cache | ❌ | ❌ | No | No data | `✗ Server not running, no cache data` |
| Local + Server not running + no cache | ❌ | ❌ | Yes | CLI probe → Fail → No data | `✗ Local probe failed, no cache to fallback` |
| Remote + Server running | ✅ | - | No | Remote Server real-time | `ℹ Data from remote Server (real-time)` |
| Remote + Server running | ✅ | - | Yes | Remote Server probe | `✓ Requested remote Server to probe` |
| Remote + Server unreachable + has cache | ❌ | ✅ | No | Read cache | `⚠ Data from local cache (remote Server unreachable)` |
| Remote + Server unreachable + has cache | ❌ | ✅ | Yes | **Try probe (fail) → Fallback cache** | `⚠ Remote Server unreachable, probe failed, fell back to cache data` |
| Remote + Server unreachable + no cache | ❌ | ❌ | No | No data | `✗ Remote Server unreachable, no cache data` |
| Remote + Server unreachable + no cache | ❌ | ❌ | Yes | Try probe (fail) → No data | `✗ Remote Server unreachable, probe failed, no cache to fallback` |

#### Core Logic

- **No --probe**: Prefer real-time (Server), then cache
- **With --probe**: **Ignore cache**, try probe first → Fallback to cache only if fails (if exists)

### 19.11 CLI Local Cache Update Timing

**Core Principle**: Cache is backup of "last known true state" — when Server unreachable, cache is only degradation data source.

#### Update Timing

| Scenario | Server Status | Update Cache? | Description |
|----------|---------------|---------------|-------------|
| **Local + Server running** | ✅ | **Update after every fetch** | Keep cache latest, backup for Server unreachable |
| **Local + Server not running + --probe** | ❌ | **Update after CLI probe success** | CLI independent mode, probe results written to cache |
| **Local + Server not running + no --probe** | ❌ | **Don't update** | Read-only cache, no write |
| **Remote + Server running** | ✅ | **Update after every fetch** | Keep cache latest, backup for remote Server unreachable |
| **Remote + Server unreachable** | ❌ | **Don't update** | Cannot get new data, cache stays as-is |

#### Core Logic

```text
if Server reachable:
    Get real-time status from Server
    Update local cache (status_cache.json)  ← Update every time
    Show real-time data
else:
    if --probe:
        Try CLI independent probe
        if probe success:
            Update local cache
            Show probe results
        else:
            Fallback to cache (don't update)
            Show cache data + warning
    else:
        Read cache (don't update)
        Show cache data + warning
```

#### Comparison with Usage Smart Flush

| Mechanism | Update Frequency | Purpose |
|-----------|------------------|---------|
| **Usage smart flush** | 15s - 3min (dynamic) | Protect disk lifespan (high-frequency writes) |
| **Status cache update** | Every status command execution | Keep cache latest (low-frequency writes, users don't run status frequently) |

Status cache update frequency is very low (users manually execute status command), no need for smart flush strategy.

### 19.12 Status Configuration Design

**Core Principle**: Provide reasonable defaults while supporting flexible configuration to adapt to different scenarios.

#### Configurable Parameters

| Parameter | Meaning | Default | Description |
|-----------|---------|---------|-------------|
| `probe_timeout` | Probe timeout (seconds) | 30 | Timeout for sending requests to upstream (connection timeout 10s + read timeout 30s) |
| `active_ttl` | Live evidence TTL (seconds) | 30 | Time window for "recently used normally" |

#### Two-Layer Configuration: Global + Provider-level Override

**Configuration File Structure**:

```toml
# Global default (required, written during init)
[status]
probe_timeout = 30
active_ttl = 30

# Provider-level override (optional, manually added)
[providers.openai.status]
probe_timeout = 15  # Only add when special configuration needed

[providers.slow-provider.status]
probe_timeout = 60
active_ttl = 60
```

#### Priority

```text
Provider-level config (if exists) > Global config > Default values
```

#### init Command Behavior

**When initializing config file**:
- Write `[status]` section (global defaults)
- **Don't ask** for Provider-level status config (optional by default)
- Users can manually add `[providers.xxx.status]` section later

#### Adding Provider Behavior

**When CLI/TUI adds provider**:
- **Don't ask** for status config (optional by default)
- Only write provider basic config (api_key, endpoint, etc.)
- Users can manually add `[providers.xxx.status]` section later

---

## 20. 总结（Summary）

### 核心设计决策

| 决策 | 选择 | 理由 |
|------|------|------|
| 业务逻辑位置 | Core 层（内存状态） | 可测试、可复用、与 I/O 解耦 |
| 持久化控制权 | Core 层独占 | 避免多个外层并发写盘 |
| 多 CLI 并发（无 Server） | 文件锁（flock） | 简单、跨平台、OS 自动释放 |
| 多 CLI 并发（有 Server） | Server 内部 Mutex | Server 是唯一写者 |
| 远程模式 fallback | 无 fallback，报错 | 远程文件不可达 |
| 本地 Server 挂了 | 退化为独立模式 | 本地文件可达 |
| 运行期间模式切换 | 不允许 | 避免状态同步复杂性 |
| 敏感数据存储 | Server 机器 | 谁发请求谁持有凭据 |

### 一句话总结

**Core 层拥有状态和持久化，接入层只做交互和转发。有 Server 就委托，没有就自己干（加锁）。远程连不上就报错，本地 Server 挂了就退化。**

