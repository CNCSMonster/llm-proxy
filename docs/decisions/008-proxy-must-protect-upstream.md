# 008: 代理层必须保护上游——client_error 也应触发 cooldown

## 日期

2026-05-25

## 触发事件

qwen-code 通过 llm-proxy 使用 `gpt-5.5-caaa` 时，Chat→Responses 转换产生的工具 JSON 格式错误导致上游报 400。客户端持续重试，每次重试都经过 llm-proxy 重新转发到上游，累积大量无效 400 请求，最终导致上游账号被限制。

详见 `issues/bugs/bugs-gpt55-caaa-400.md`。

## 原设计假设

原始 cooldown spec（`docs/spec-provider-cooldown.md`）对 `client_error`（HTTP 400）不设 cooldown：

```
| client_error | no | yes | no |
```

设计假设是：4xx 表示请求格式错误，属于**客户端的责任**。客户端收到 400 后应修请求或放弃，不应重试。因此代理层不需要额外保护。

## 为什么这个假设是错的

在实践中：

1. **代理层不控制客户端行为**。客户端可能是 qwen-code、Codex、pi、或任何其他工具。每个客户端有自己的重试策略。有的客户端会把 400 当作可重试的错误。

2. **上游不区分请求来源**。上游看到的是"来自代理的 N 个 400 请求"，不会考虑"这些请求是客户端的重试"。

3. **Bug 会放大影响**。如果代理层本身的转换产生格式错误（如本次），客户的正常重试逻辑会将一次 bug 放大为对上游的批量轰炸。

4. **代价由上游用户承担**。结果是账号被限流或封禁，而根因（客户端重试 + 代理层未拦截）用户完全无法控制。

## 决定

`client_error` 应设置 cooldown，与 `server_error` 同级别（默认 300 秒）。首次 400 后，同一 provider 在 cooldown 期间不再转发同类请求，防止客户端重试轰炸上游。

## 实现

1. `docs/spec-provider-cooldown.md` 中 `client_error` cooldown 从 `no` 改为 `client_error_seconds`。
2. `internal/config/config.go` 的 `FallbackCooldownConfig` 新增 `ClientErrorSeconds` 字段。
3. `internal/config/defaults.go` 默认值 300 秒。
4. `internal/proxy/policy.go` 的 `policyForCategory` 中 `CategoryClientError` 应用该配置。

## 教训

**代理层不能假定客户端会正确处理错误。** 作为中间层，llm-proxy 有责任保护上游，即使错误"理论上"是客户端的责任。保守的策略是：任何上游拒绝的请求类型，都应在 proxy 层设冷却，不应依赖客户端行为来保护上游。
