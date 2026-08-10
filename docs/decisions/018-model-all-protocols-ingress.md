# ADR-018: 模型是全协议入口的——协议支持由 provider 存在性推导

## Status

Accepted

## Date

2026-07-08

## Context

`llm-proxy` 的核心能力是三种协议（openai-chat / openai-responses / anthropic）之间的双向转换。代理可以作为协议桥接，让任何入站协议到达任何上游 provider——不需要 provider 原生支持该协议。

但此前有两次实践的偏差暴露了这条原则尚未明确：

1. **GPT subscription 补全断裂**：GPT subscription 模型在 defaults.go 中只声明了 `openai-responses` providers，缺少 `openai-chat` 和 `anthropic` providers。导致 `launch claude-code` 的 shell 补全不显示这些模型、Claude Code 或其他 openai-chat 客户端无法使用。

2. **PI reasoning 声明不完整**：PI models.json 生成逻辑中，reasoning 能力从 provider 的 `ThinkingFormat` 字段推断。anthropic 路径的 provider 正确没有 `ThinkingFormat`（这是 openai-chat 协议的注入参数），但导致 PI 认为该模型在 anthropic 路径下不支持 thinking——实际上代理的 Anthropic→Responses 转换已完整支持。

两件事指向同一个原则缺口：**模型的协议入口应该齐全而不依赖 provider 的原生能力——代理的协议转换已消除这种依赖。**

## Decision

**一个模型只要为任意 protocol type 声明了至少一个 provider，就应为三种入站协议（openai-chat / openai-responses / anthropic）都提供入口。三种 type entry 可以绑定同一个 provider（代理做协议转换），不需要三种不同的 provider。**

具体规则：

1. `defaults.go` 中每个模型条目必须声明全部存在 provider 的协议类型。
2. 添加新产品或新模型时，确保三者入口齐全。
3. `connect` 命令生成配置时，对产品下每个模型写入三种 protocol providers。

### 协议优选规则（connect 生成 model entry 时的 provider 绑定策略）

| 产品有的 provider 类型 | openai_chat_providers | openai_responses_providers | anthropic_providers |
|----------------------|----------------------|---------------------------|-------------------|
| chat 一种 | chat（同协议） | chat（代理转换） | chat（代理转换） |
| anthropic 一种 | anthropic（代理转换） | anthropic（代理转换） | anthropic（同协议） |
| responses 一种 | responses（代理转换） | responses（同协议） | responses（代理转换） |
| chat + anthropic | chat（同协议） | **anthropic**（非 chat） | anthropic（同协议） |
| chat + responses | chat（同协议） | responses（同协议） | **responses**（非 chat） |
| anthropic + responses | **anthropic**（非 chat） | responses（同协议） | anthropic（同协议） |
| 三者全有 | chat（同协议） | responses（同协议） | anthropic（同协议） |

**优先策略**：
- 同协议优先——有原生的用原生。
- 无原生同协议时，跨协议选非 chat 的 provider（chat 协议将废弃）。

代理的 `forwardWithConversion` 路由表支持全部 6 种跨协议组合（三种入站 × 两种出站），不需要 runtime 判断。

## Consequences

- **正面**：用户用任何客户端（Codex/PI/Claude Code/Qwen Code）都能发现同一套模型，不再出现"connect 配了但 launch 看不到"的困惑。
- **正面**：模型能力是声明性的，不依赖 provider 配置推导，减少配置漂移。
- **维护成本**：添加新模型时需多写几行 provider 声明，但这是机械操作，不会引入逻辑复杂度。
- **不解决的问题**：不同客户端对协议有不同语法细节（如 Claude Code 的 `[1m]` 后缀），这些仍是 protocol-specific 的行为，不在此 ADR 范围内。
