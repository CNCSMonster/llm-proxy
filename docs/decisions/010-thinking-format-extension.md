# ADR-010: Thinking Format 扩展机制 vs 通用请求字段注入

## Status

Accepted

## Date

2026-06-15

## Context

接入 StepFun (阶跃星辰) provider 时，发现其 OpenAI Chat 兼容接口默认使用 `reasoning` 字段返回 thinking 内容，而非标准的 `reasoning_content` 字段。StepFun 提供 `reasoning_format` 请求参数，设置为 `"deepseek-style"` 后可强制返回标准的 `reasoning_content` 字段。

需要一种机制在请求时注入这个额外参数。

### 方案对比

| 方案 | 描述 | 优点 | 缺点 |
|------|------|------|------|
| **ExtraRequestFields** | 在 ProviderConfig 中新增通用字段 `extra_request_fields`，支持注入任意请求参数 | 通用性强，可注入任意字段 | 过度设计；配置复杂；语义不清晰 |
| **ThinkingFormat case** | 在 `thinking_adapter.go` 中新增 `"stepfun"` case，在处理 thinking 时同时注入 `reasoning_format` | 语义清晰；扩展模式统一；配置简洁 | 每个需要注入参数的 provider 都要加 case |

## Decision

采用 **ThinkingFormat case** 方案。

具体做法：
1. 在 `thinking_adapter.go` 的 `applyOpenAIThinkingAdapter` 函数中新增 `"stepfun"` case
2. 该 case 与 `"deepseek"` 行为相同，但额外注入 `reasoning_format: "deepseek-style"` 到请求体
3. StepFun provider 配置为 `thinking_format = "stepfun"`
4. 回退 `ExtraRequestFields` 字段

## Rationale

### 1. 语义清晰

`ThinkingFormat` 的语义是"这个 provider 的 thinking 协议变体"。StepFun 的 `reasoning_format` 参数正是 thinking 协议的一部分——它告诉上游"请用标准格式返回 thinking 内容"。

相比之下，`ExtraRequestFields` 是通用机制，但：
- 目前只有 1 个用例（stepfun）
- 未来大概率也不会有更多用例——provider 需要固定注入的非 thinking 参数极少
- 大多数参数（temperature、top_p 等）是用户请求中带的，不是 provider 级别的

### 2. 扩展模式统一

未来如果有其他 provider 也需要注入额外参数，按协议维度扩展：

| 场景 | 处理方式 |
|------|---------|
| 参数与 thinking 相关（如 stepfun 的 reasoning_format） | 加 `ThinkingFormat` case |
| 参数与 tool_call 相关 | 加 `Features` 或新字段如 `ToolCallFormat` |
| 参数与图片输入相关 | 加 `Features` 或新字段如 `ImageInputFormat` |

每种协议差异都有对应的"格式字段"来处理，不会混在一起。

### 3. 配置简洁

```toml
# ThinkingFormat 方式（采用）
[[providers]]
name = "stepfun-step-plan"
thinking_format = "stepfun"

# ExtraRequestFields 方式（不采用）
[[providers]]
name = "stepfun-step-plan"
[providers.extra_request_fields]
reasoning_format = "deepseek-style"
```

前者更简洁，且语义明确。

### 4. YAGNI 原则

不要为 hypothetical 的未来场景过度设计。等真正出现第 2 个需要注入额外参数的 provider 时，再考虑通用机制。

## Consequences

- **收益**：
  - 代码集中在 `thinking_adapter.go` 一个文件，易于理解和维护
  - 配置简洁，语义清晰
  - 扩展模式统一，未来新增 provider 时知道该加在哪里

- **代价**：
  - 每个需要注入 thinking 相关参数的 provider 都要加一个 case
  - 但这正是"一种协议差异对应一个格式字段"的设计原则

- **影响**：
  - StepFun provider 配置为 `thinking_format = "stepfun"`
  - 回退 `ExtraRequestFields` 字段，减少代码复杂度
  - Spec 中 `thinking_format` 可选值新增 `"stepfun"`

## Future Considerations

如果未来出现需要注入**非 thinking 相关**额外参数的 provider，应该：
1. 分析该参数属于哪个协议维度（tool_call? image_input? 其他?）
2. 按维度新增格式字段（如 `ToolCallFormat`）或扩展 `Features`
3. 在对应的 adapter/handler 中处理

避免引入通用的 `ExtraRequestFields` 机制，除非有 3+ 个不同维度的用例证明其必要性。
