# Architecture Decision Records

本目录记录 llm-proxy 的重要架构决策。

ADR 只记录已经做出的重要设计选择：为什么这样做、考虑过哪些替代方案、接受了哪些代价。ADR 不追踪进度或实现状态；Rust v2 目标行为的细节规范以 [`../../spec.md`](../../spec.md) 为准。

## ADR 列表

- [`001-direct-protocol-conversion.md`](001-direct-protocol-conversion.md) — 协议转换采用直接 A → B 转换，而不是统一中间格式。
- [`002-server-side-reasoning-cache.md`](002-server-side-reasoning-cache.md) — 使用服务端缓存保留 DeepSeek `reasoning_content`。
- [`003-config-driven-provider-models.md`](003-config-driven-provider-models.md) — provider/model 采用配置驱动和双 ID 映射。
- [`004-codex-responses-wire-api.md`](004-codex-responses-wire-api.md) — Codex 集成默认使用 Responses API。
- [`005-provider-cooldown-policy.md`](005-provider-cooldown-policy.md) — Provider cooldown 采用按 model/provider 降优先级机制。
- [`006-config-migration-policy.md`](006-config-migration-policy.md) — 配置格式变更必须提供迁移策略。
- [`007-simple-model-provider-configuration.md`](007-simple-model-provider-configuration.md) — Model/provider 配置优先保持简单心智模型。
- [`008-proxy-must-protect-upstream.md`](008-proxy-must-protect-upstream.md) — Proxy 必须保护上游（client_error 触发 cooldown）。
- [`009-kimi-code-anthropic-only.md`](009-kimi-code-anthropic-only.md) — Kimi Code 仅使用 Anthropic 兼容端点，不提供 OpenAI 兼容端点。
- [`010-thinking-format-extension.md`](010-thinking-format-extension.md) — 扩展 `thinking_format` 以支持 StepFun reasoning_format 参数注入。
- [`011-no-claude-code-gateway-discovery.md`](011-no-claude-code-gateway-discovery.md) — 暂不实现 Claude Code gateway 模型发现。
- [`012-no-daemon-only-background.md`](012-no-daemon-only-background.md) — `--daemon` 改名为 `--background`，不实现系统服务集成。
- [`013-features-at-model-level.md`](013-features-at-model-level.md) — Features 声明在 Model 级别。
- [`014-self-implement-upstream-client.md`](014-self-implement-upstream-client.md) — 上游客户端自行实现，不引入官方 SDK。
- [`015-oauth-polling-safety-margin.md`](015-oauth-polling-safety-margin.md) — OAuth 设备码轮询使用 API interval + 3s 安全余量，防止时钟漂移触发限流。
- [`016-token-refresh-retry-singleflight.md`](016-token-refresh-retry-singleflight.md) — Token 刷新区分永久失败/瞬态失败重试，并发刷新用 singleflight 聚合。
- [`017-probe-reuses-proxy-body-logic.md`](017-probe-reuses-proxy-body-logic.md) — 探测请求复用正常转发的 body 构建逻辑，确保探测结果真实可靠。
- [`018-model-all-protocols-ingress.md`](018-model-all-protocols-ingress.md) — 模型是全协议入口的，协议支持由 provider 存在性推导，代理的协议转换消除原生协议依赖。
- [`019-duplication-boundary-tradeoff.md`](019-duplication-boundary-tradeoff.md) — 重复代码的边界权衡：DRY 不应跨越独立变化单元。
- [`020-antigravity-trainable-never.md`](020-antigravity-trainable-never.md) — Antigravity 请求强制设置 `trainable: "NEVER"`，防止用户数据被用于模型训练。
- [`021-unify-toml-library-and-roundtrip-boundary.md`](021-unify-toml-library-and-roundtrip-boundary.md) — 统一 TOML 库并限定 round-trip 编辑边界。
- [`022-rust-v2-config-editing-boundary.md`](022-rust-v2-config-editing-boundary.md) — Rust v2 不默认使用 tree-sitter，按文件类型选择 `toml_edit` / `serde_json::Value` / 全量托管序列化。
- [`023-responses-aggregation-type-scope.md`](023-responses-aggregation-type-scope.md) — Responses 聚合只保留 message/function_call，SSE 透传路径全量转发；Codex 直连不受影响。

## 模板

```markdown
# ADR-XXX: Title

## Status

Accepted

## Date

YYYY-MM-DD

## Context

背景、约束和问题。

## Decision

最终选择。

## Alternatives Considered

### Option A

优点、缺点、拒绝原因。

## Consequences

这个选择带来的收益、代价和后续影响。
```
