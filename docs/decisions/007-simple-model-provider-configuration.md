# ADR-007: Keep Model/Provider Configuration Mental Model Simple

> **注意**：v2 中 `frontend_id` 已统一为 `[models.<id>]`（model id）。本文档中 `client-facing model id / frontend id` 等历史术语均指当前 config v2 的 **model id**。
>
> **历史格式说明**：本文是 2026-05-06 的架构决策记录，示例中保留了当时讨论的旧配置格式（如 `[[models]]`、`id = ...`）。当前正式配置格式以 [`spec.md`](../../spec.md) 为准。

## Status

Accepted

## Date

2026-05-06

## Context

`llm-proxy` 的核心价值是：客户端用一个稳定模型名调用 proxy，proxy 按配置把请求转发到一个或多个 provider，并在必要时做协议转换。

随着配置能力扩展，我们讨论过多个概念：

- client-facing model id / frontend id / display id
- logical model id / canonical id
- upstream model id / provider-specific model id
- provider bindings
- provider fallback

这些概念都能更精确地描述系统，但如果直接暴露给用户，会让配置心智模型变复杂。用户明确要求：配置设计和文档解释应尽量简单。

## Decision

后续 model/provider 配置设计优先采用一个简单心智模型：

```text
一个 model 配置回答三个问题：
1. 客户端看到什么模型名？
2. 这个模型可以走哪些 provider？
3. 每个 provider 那边真实叫什么模型？
```

因此，未来 config v2 方向优先采用：

```toml
[[models]]
id = "deepseek-v4-flash-lp"
type = "openai-responses"
providers = [
  { name = "deepseek", model = "deepseek-v4-flash" },
  { name = "openrouter", model = "deepseek/deepseek-chat-v3.1" },
]
```

字段语义：

| 字段 | 含义 |
|---|---|
| `models.id` | 客户端/CLI 看到并请求的模型名。可带 `-lp` 后缀，表示经 llm-proxy 转发。 |
| `models.type` | 客户端使用的入站协议类型，如 `openai-responses`。 |
| `models.providers[].name` | 使用哪个 provider endpoint。 |
| `models.providers[].model` | 该 provider 实际收到的上游模型名。 |

这意味着：

- 不再优先引入 `display_id` / `logical_id` / `upstream_id` 等额外字段名。
- 不再把 `model_id` 作为顶层必备字段；如果未来有 config v2，`models.id` 是客户端可见 id，不是 provider 上游 id。
- provider-specific model name 放在 provider binding 里，用 `providers[].model` 表达。
- `/v1/models` 和 `llm-proxy launch codex` 暴露 `models.id`。
- 转发上游时使用当前 provider binding 的 `model`。

## Why this is simpler

用户只需要理解：

```text
id              = 我在客户端里选的名字
providers.name  = 走哪个上游
providers.model = 这个上游那边的真实名字
```

不需要理解三张表，也不需要区分 display/logical/canonical/upstream 等抽象术语。

## Relationship to ADR-003

ADR-003 接受了 provider/model 配置驱动与客户端模型名和上游模型名解耦。

本 ADR 不推翻 ADR-003，而是进一步约束后续配置演进：

- ADR-003 的目标仍然成立：配置是 model/provider 路由的 source of truth。
- 本 ADR 要求：在表达更复杂 provider-specific model name 时，优先保持单个 `[[models]]` 下的简单结构，而不是拆成 `frontend_models` / `logical_models` / `providers` 多层表。

## Alternatives Considered

### Three explicit layers: frontend_models / models / providers

Example:

```toml
[[frontend_models]]
id = "deepseek-v4-flash-lp"
model = "deepseek-v4-flash"

[[models]]
id = "deepseek-v4-flash"
providers = [...]
```

Pros:

- 概念最精确。
- 每一层职责严格隔离。

Cons:

- 配置文件明显变复杂。
- 用户需要理解更多抽象。
- 对当前需求过度设计。

Rejected because the mental model is too heavy for the current project needs.

### Keep top-level model_id as default upstream model

Example:

```toml
[[models]]
frontend_id = "deepseek-v4-flash-lp"
model_id = "deepseek-v4-flash"
provider_bindings = [
  { name = "openrouter", model_id = "deepseek/deepseek-chat-v3.1" },
]
```

Pros:

- Backward compatible with current schema.
- Minimal code change.

Cons:

- `model_id` becomes ambiguous once provider-specific IDs exist.
- Users must understand default vs override.
- Field names (`frontend_id`, `model_id`, `provider_bindings`) expose implementation language rather than user goals.

Rejected as the ideal long-term design, but may still be used as an incremental migration step if needed.

### Mixed `providers` list with string/object values

Example:

```toml
providers = [
  "deepseek",
  { name = "openrouter", model = "deepseek/deepseek-chat-v3.1" },
]
```

Pros:

- Compact.
- Fallback order and override live in one place.

Cons:

- Union type complicates TOML parsing and validation.
- Error messages become worse.
- Users may mix styles inconsistently.

Rejected in favor of a uniform object list.

## Consequences

- Future config v2 is likely a breaking schema change and must follow ADR-006.
- New config should prefer explicit provider objects rather than mixed string/object arrays.
- Simple single-provider configs become slightly more verbose but easier to reason about.
- `status` output should mirror this mental model:

```text
CLIENT_MODEL             TYPE              PROVIDER      UPSTREAM_MODEL
deepseek-v4-flash-lp     openai-responses  deepseek      deepseek-v4-flash
deepseek-v4-flash-lp     openai-responses  openrouter    deepseek/deepseek-chat-v3.1
```

## Implementation Notes

This ADR was implemented in config v2.

Implementation choices:

1. Default config generation writes `models.id` and nested `[[models.providers]]` entries.
2. `models.id` is used by `/v1/models`, `launch codex`, and request routing as the client-visible model name.
3. Provider dispatch uses the selected provider binding's `model` as the upstream model name.
4. Old `frontend_id/model_id/providers = ["..."]` config is intentionally not migrated; users should delete/regenerate config for v2.
