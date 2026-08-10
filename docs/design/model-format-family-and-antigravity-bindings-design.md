# 上游格式族声明（endpoint 级）与 antigravity 协议绑定设计

**状态**: ✅ 已实现（2026-08-04，commit `6424e7e`）— endpoint 级 `anthropic_family_models` 声明 + `antigravity_needs_tool_call_ids` 判定 + 解析链已全部落地（含单元测试与 antigravity 多轮工具调用 E2E 验证），本文档保留作为设计记录
**日期**: 2026-08-02
**作者**: llm-proxy team

## 1. 背景与动机

### 1.1 问题：antigravity 模型族的转换路径差异

antigravity 上游（`cloudcode-pa.googleapis.com/v1internal:streamGenerateContent`）不是原生 Gemini API——它内部对不同模型族做不同转换：

```
antigravity 上游
├── gemini-*   → 原生 Gemini generateContent 格式
├── claude-*   → 转 Anthropic Messages（要求 tool_use.id / thinking 签名）
└── gpt-oss-*  → 也转 Anthropic Messages（同 claude 转换器）
```

llm-proxy 的转换器需要知道"该模型在上游走 Anthropic 转换（functionCall 必须带 id）还是 Gemini 原生（不带 id）"。

### 1.2 实证（L1，2026-08-02）

- gpt-oss-120b 通过 llm-proxy 多轮工具调用 → HTTP 400 `Expected the 'id' of a(n) 'assistant' 'tool_calls' array element to be populated`
- agy 官方 CLI 直连 antigravity 用 gpt-oss 多轮 → 成功（证明是 llm-proxy 转换问题，非服务端）
- 修复后多轮成功（4181 正确）

### 1.3 配置一致性要求（用户确认）

model 声明方式必须**与所有 provider 类型一致**——不因 antigravity 的特殊性而让 model 配置多写字段。antigravity 的特殊性（格式族、认证）应内聚在 provider/endpoint 层。

### 1.4 v1 方案的不足（被否决）

v1 在 `ModelConfig.format_family` 显式声明格式族——model 配置需要多写 `format_family = "anthropic-family"`，与其他 provider 不一致，且把"上游行为"摊进通用模型结构。

## 2. 概念模型（用户确认）

```
provider 提供若干 endpoint（openai_chat / openai_responses / anthropic / antigravity）
model 的 xx_providers 绑定 = 用某个 provider 的 xx endpoint 提供 + 对应 upstream model name
```

- **provider 层**：`ProviderConfig` 声明提供哪些 endpoint（字段存在即提供）
- **endpoint 层**：`EndpointConfig` 承载该端点上游的行为属性（格式族）
- **model 层**：`ProviderBinding { name = provider_id, model = upstream_model }`，**声明方式对所有 provider 统一**

## 3. 设计

### 3.1 EndpointConfig 增加格式族属性

```rust
// config.rs EndpointConfig
pub struct EndpointConfig {
    pub url: Option<String>,
    pub derive_from: Option<String>,
    pub compat: Option<CompatConfig>,
    /// 该 endpoint 上游中走 Anthropic Messages 转换的模型族（glob 匹配 upstream_model）。
    /// 例：["claude-*", "gpt-oss-*"] 表示这些模型需要 functionCall/functionResponse id。
    /// 空/缺省 = 全部 Gemini 原生语义。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub anthropic_family_models: Vec<String>,
}
```

### 3.2 Provider 配置形态（antigravity）

```toml
[providers.google-antigravity.auth]
type = "antigravity_oauth"
account = "antigravity"

[providers.google-antigravity.openai_responses]
derive_from = "antigravity"

[providers.google-antigravity.anthropic]
derive_from = "antigravity"

[providers.google-antigravity.antigravity]
url = "https://cloudcode-pa.googleapis.com/v1internal:streamGenerateContent"
# antigravity 上游行为：这些模型族走 Anthropic 转换（需要 tool_use id）
anthropic_family_models = ["claude-*", "gpt-oss-*"]
```

### 3.3 Model 配置形态（与其他 provider 完全一致，零额外字段）

```toml
[models.claude-opus-4-6-ag-lp]
context_window = 200000
max_output_tokens = 64000

[[models.claude-opus-4-6-ag-lp.openai_responses_providers]]
name = "google-antigravity"
model = "claude-opus-4-6-thinking"

[[models.claude-opus-4-6-ag-lp.anthropic_providers]]
name = "google-antigravity"
model = "claude-opus-4-6-thinking"
```

### 3.4 解析链（转换器如何判定格式族）

```
model.claude-opus-4-6-ag-lp
  → binding {google-antigravity, claude-opus-4-6-thinking}  (openai_responses_providers)
  → provider.google-antigravity.openai_responses (derive_from=antigravity)
  → native endpoint antigravity: anthropic_family_models = ["claude-*", "gpt-oss-*"]
  → upstream_model "claude-opus-4-6-thinking" 匹配 "claude-*"
  → 格式族 = AnthropicFamily → 转换器给 functionCall/functionResponse 带 id
```

`ExecutionPlan` 增加 `anthropic_family_models: Vec<String>`（resolve 时从 native endpoint 填充），转换器判定：

```rust
pub fn antigravity_needs_tool_call_ids(
    upstream_model: &str,
    anthropic_family_models: &[String],
) -> bool {
    anthropic_family_models.iter().any(|pat| {
        // glob/前缀匹配：claude-* / gpt-oss-*
        pattern_match(pat, upstream_model)
    })
}
```

### 3.5 向后兼容

- `anthropic_family_models` 缺省为空 Vec → 全部 Gemini 原生（与现状一致）
- 现有 provider（deepseek/kimi/bailian）不写该字段，行为不变
- antigravity gemini 模型无需任何配置（默认原生）

## 4. 与 v1 的差异（回退内容）

| 项 | v1（已实现） | v2（本方案） |
|---|---|---|
| 格式族位置 | `ModelConfig.format_family` | `EndpointConfig.anthropic_family_models` |
| model 配置 | 需写 format_family | **零额外字段** |
| 判定函数 | `needs_tool_call_ids(FormatFamily)` | `needs_tool_call_ids(upstream_model, &[String])` |
| ExecutionPlan | `format_family: FormatFamily` | `anthropic_family_models: Vec<String>` |
| 需要回退 | — | ModelConfig.format_family / antigravity_providers 字段（如已加入） |

**注**：`antigravity_providers`（ModelConfig 第 4 协议绑定）与格式族方案正交——v2 中保留与否另议（客户端目前经 openai_responses/anthropic 访问 antigravity，不直接说第 4 协议）。

## 5. 验证计划

1. **单元测试**：
   - `anthropic_family_models` 序列化（数组 / 缺省空）
   - `needs_tool_call_ids` glob 匹配（claude-* 命中、gemini-* 不命中）
   - `ExecutionPlan.anthropic_family_models` 从 native endpoint 正确填充
   - resolve 链：binding → provider → endpoint → 格式族判定
2. **回归**：12 个 antigravity 模型多轮工具调用（fib 任务）全通过
3. **反向验证**：gpt-oss 靠 endpoint 声明（非字符串匹配）多轮成功（4181 正确）

## 6. 开放问题（已定案 2026-08-05）

1. `antigravity_providers`（ModelConfig 第 4 协议绑定）：**不加入**。antigravity 原生协议（`v1internal:streamGenerateContent`）是 Google 内部协议，无第三方客户端直接消费；model 声明保持对所有 provider 统一，特殊性内聚在 endpoint 层（`anthropic_family_models` 即此模式）。未来若出现直接说该协议的客户端，扩展点在 endpoint 层，model 绑定形态不变。
2. glob 匹配语法：**完整 glob（globset crate，2026-08-05 从前缀匹配升级）**——支持 `*` 任意、`?` 单字符、字符类、`{a,b}` 交替；配置加载时语法验证（fail-fast，非法 pattern 直接报错）；实现见 `config.rs` `anthropic_family_glob_set`/`validate_anthropic_family_patterns` 与 `convert/antigravity.rs` `antigravity_needs_tool_call_ids`。现有 `claude-*`/`gpt-oss-*` 配置行为不变。
