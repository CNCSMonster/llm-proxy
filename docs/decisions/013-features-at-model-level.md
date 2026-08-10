# ADR-013: Features 声明在 Model 级别

## Status

Accepted

## Date

2026-06-21

## Context

`llm-proxy` 的配置 schema 中，features（能力特征）的声明位置有两种设计：

1. **Provider 级别**：`provider.features` 声明该 provider 端点的能力
2. **Model 级别**：`model.features` 声明该模型的能力

原有设计中，`image_input`、`document_input`、`tool_call_reasoning` 同时存在于 provider 和 model 的 features 数组中，语义模糊：

- Provider features 用于运行时路由（图片请求只路由到声明 `image_input` 的 provider）
- Model features 用于 launch 配置生成（告知客户端模型能力）

这导致两个问题：

1. **配置重复**：同一能力需要在 provider 和 model 两处声明
2. **粒度不足**：同一 provider 下不同模型可能有不同能力（如 deepseek-chat 不支持图片，deepseek-vl 支持图片），但 provider 级别的 features 无法区分

## Decision

**所有 features 声明在 model 级别**：

| 特征 | 级别 | 说明 |
|------|------|------|
| `image_input` | Model | 模型支持图片输入——对客户端的承诺 |
| `document_input` | Model | 模型支持文档输入——对客户端的承诺 |
| `tool_call_reasoning` | Model | 请求需要注入 `reasoning_content` |

**Provider 不声明 features**：Provider 仅保留基础配置（端点、认证、协议类型、thinking 相关配置）。

**静态校验**：配置加载时校验 model 的每个 feature 至少有一个配置的 provider 支持。

## Why this is better

1. **粒度更细**：同一 provider 下不同模型可以有不同的 features
2. **配置简洁**：不需要在 provider 和 model 两处重复声明
3. **语义清晰**：features 描述"模型能力"，provider 描述"请求目标"
4. **符合直觉**：用户理解"这个模型支持图片"比"这个 provider 支持图片"更自然

## Alternatives Considered

### 保持 Provider + Model 双层 features

Pros:
- 运行时路由可以基于 provider features 过滤
- 支持"部分 provider 支持某 feature"的路由策略

Cons:
- 配置重复，容易遗漏或不一致
- 粒度不足，无法区分同一 provider 下不同模型的能力
- 语义模糊，用户困惑为什么要声明两次

Rejected because it causes configuration redundancy and user confusion.

### 只用 Provider features，Model 能力由 Provider 推导

Pros:
- 配置更简洁
- 单一来源

Cons:
- 无法区分同一 provider 下不同模型的能力
- Launch 配置生成需要聚合多个 provider 的 features，逻辑复杂
- 不符合"模型能力是模型本身的属性"这一直觉

Rejected because it cannot express per-model capabilities within the same provider.

## Consequences

1. **配置迁移**：现有 `provider.features` 需要迁移到 `model.features`
2. **代码变更**：
   - `ProviderConfig.Features` 字段移除
   - 运行时路由逻辑调整：不再按 provider features 过滤
   - 静态校验逻辑新增：校验 model features 与 provider 能力的一致性
3. **文档更新**：spec.md 已更新，删除 Provider Features 章节，更新 Model Features 章节

## Implementation Notes

- 配置加载时新增 `ValidateModelFeatures()` 函数
- 校验规则：model 的每个 feature 至少有一个配置的 provider 支持（通过 provider 类型或其他方式判断）
- Launch 配置生成逻辑不变，仍基于 model features 生成客户端配置
