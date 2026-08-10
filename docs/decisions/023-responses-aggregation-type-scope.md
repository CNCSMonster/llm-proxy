# ADR-023: Responses 聚合只保留 message/function_call，SSE 透传全量转发

## Status

Accepted

## Date

2026-08-02

## Context

openai-sub（ChatGPT 订阅）上游强制 `stream: true`。客户端请求非流式（`stream: false`）时，代理必须把上游 SSE 事件流聚合回 Responses JSON。聚合函数 `aggregate_responses_sse_to_value` 目前只重组两种 output item：

- `message`（assistant 文本，`content[0].text`）
- `function_call`（工具调用，`call_id` / `name` / `arguments`）

其余类型（`reasoning`、`web_search_call`、`file_search_call`、`shell_call` 等）在聚合时被丢弃。

讨论中确认了以下事实：

- **chat/anthropic 转换路径**：聚合产物要经 convert.rs 转成 chat/anthropic 协议，这两种目标协议没有承载 reasoning / web_search 等 item 的字段位置，裁剪是必要的。
- **Codex 直连（responses 原生）路径**：Codex 客户端构造请求时 `stream: true` 是硬编码的（codex-rs/core/src/client.rs:884），永远走 SSE 透传路径 `responses_native_sse_rewrite_model`，该路径对事件流**零裁剪、全量转发**——聚合函数对 Codex 不可达。
- **参考实现**：CLIProxyAPI 流式转发所有事件（reasoning 转 anthropic thinking）；cc-switch 聚合时收集全部 `output_item.done` 原样保留。两者下游都是 responses 原生客户端，因此不裁剪。

## Decision

聚合函数 `aggregate_responses_sse_to_value` **保持现状**：只重组 `message` 和 `function_call` 两种 item，其余类型丢弃。

理由：

1. 聚合产物的唯一现实消费方是 chat/anthropic 转换路径，目标协议无位置承载其余类型，裁剪不造成协议缺失。
2. Codex 直连 openai-sub 走 SSE 透传路径，全量事件原样转发，不存在缺失场景；聚合路径对 Codex 不可达。
3. 不为"非流式 responses 原生客户端"这个边缘场景增加聚合复杂度——该类客户端 + force_stream 上游是少见组合，且将来若出现，可在聚合层补齐全量 item 收集（对齐 cc-switch），不改动透传路径。

## Consequences

收益：

- 聚合逻辑保持简单，只处理下游转换真正消费的类型。
- Codex 直连场景零影响（SSE 透传全量转发）。
- 裁剪行为与 convert.rs 的 `_ => {}` 一致，避免"聚合保留但转换丢弃"的冗余。

代价：

- 非流式 responses 原生客户端（如 SDK 脚本）经聚合路径会缺失 reasoning 等 item——已知限制，当前无现实触发场景。
- 若未来支持非流式 responses 原生客户端，需要在聚合层增加全量 item 收集，与 cc-switch 对齐。

## Alternatives Considered

### 聚合层全量保留所有 item（对齐 cc-switch）

优点是聚合产物是完整 Responses 结构，任何下游都能消费。

拒绝原因：聚合产物的实际消费方（chat/anthropic 转换）不需要其余类型，全量保留会引入"聚合保留但 convert 丢弃"的冗余；且 Codex 直连走 SSE 透传，全量保留对主流场景无收益。

### 聚合层增加 reasoning 到 anthropic thinking 的映射

优点是推理文本不丢失。

拒绝原因：聚合路径本身是边缘场景（非流式客户端 + force_stream 上游），且 thinking 映射在流式透传路径已存在；为边缘场景增加映射复杂度不值得。

## 相关

- 参考实现：CLIProxyAPI `codex_claude_response.go`（流式转发）、cc-switch `handlers.rs` `responses_sse_to_response_value`（聚合全量收集）。
- 协议语义：OpenAI Responses streaming-events 文档定义 done 事件携带 finalized 完整值；delta 累积 + done 定稿是官方推荐主流程。
- 设计：`../design/chatgpt-backend-responses-adaptation.md` §4.2 响应侧契约。
