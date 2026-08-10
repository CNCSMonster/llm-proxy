# ADR-009: Kimi Code 仅使用 Anthropic 兼容端点

## Status

Accepted

## Date

2026-06-03

## Context

Kimi Code 官方文档提供两个协议端点：

| 协议 | Base URL | 常见用途 |
|------|----------|----------|
| OpenAI 兼容 | `https://api.kimi.com/coding/v1` | Roo Code / OpenAI-compatible 工具 |
| Anthropic 兼容 | `https://api.kimi.com/coding/` | Claude Code / Anthropic Messages 工具 |

当使用标准 OpenAI SDK 风格请求 OpenAI 兼容端点（`coding/v1/chat/completions`）时，返回 403：

```json
{"error":{"message":"Kimi For Coding is currently only available for Coding Agents such as Kimi CLI, Claude Code, Roo Code, Kilo Code, etc.","type":"access_terminated_error"}}
```

**根因**：Kimi Code CLI 请求中会附带一组 `X-Msh-*` 身份头（`X-Msh-Platform`、`X-Msh-Version`、`X-Msh-Device-Id` 等），用来标识请求来自 Kimi Code CLI。OpenAI 兼容端点依赖这些头来识别客户端身份，缺少则拒绝。

相比之下，Anthropic 兼容端点（`coding/v1/messages`）只要求 `x-api-key` + `anthropic-version` 头，不检查客户端身份，任何工具均可调用。

参考：[Kimi Code API Access 403 排查](https://cncsmonster.github.io/posts/kimi-code-api-access-403/)

## Decision

Kimi Code 在 llm-proxy 中**仅配置 Anthropic 兼容端点**（`kimi-code-anthropic`），不提供 OpenAI 兼容端点（`kimi-code`）。

具体做法：
1. `defaults.go` 中只保留 `kimi-code-anthropic` provider（type=anthropic）
2. `kimi-for-coding-lp` 模型的 `openai-responses` 和 `anthropic` 两个 frontend type 均绑定到 `kimi-code-anthropic`
3. `openai-responses → anthropic` 的转换路径已由现有 converter 覆盖

## Alternatives Considered

### Option A：配置 kimi-code provider + custom headers

在 `ProviderConfig` 中新增 `custom_headers` 字段，注入 `X-Msh-*` 身份头。

- 优点：可支持 OpenAI 兼容端点
- 缺点：Kim 官方文档提醒不应伪造客户端身份；公开或生产使用时篡改标识可能导致会员权益暂停
- 拒绝原因：违反官方建议，有合规风险

### Option B：在 proxy 层硬编码注入 X-Msh-* 头

- 优点：对客户端透明
- 缺点：隐蔽行为、增加维护负担、同样涉及身份伪造
- 拒绝原因：同上，且增加代码复杂度

## Consequences

- **收益**：Kimi Code 接入简单可靠，不依赖客户端身份信息
- **代价**：`kimi-for-coding-lp` 的 `openai-responses` 类型走 Responses → Anthropic 转换路径（已有完整 converter 覆盖，功能无损）
- **影响**：所有通过 `kimi-for-coding-lp` 的请求最终都以 Anthropic Messages 格式发到 upstream
