# ADR-002: Use Server-Side Reasoning Cache

## Status

Accepted

## Date

2026-04-30

## Context

DeepSeek thinking mode returns `reasoning_content` in assistant messages. Follow-up requests that include tool results must send the assistant message back with the same `reasoning_content`; otherwise DeepSeek can reject the conversation.

**This is documented by DeepSeek** ([thinking_mode](https://api-docs.deepseek.com/zh-cn/guides/thinking_mode)): `reasoning_content` must be passed back for all assistant messages with tool_calls, across all subsequent turns, or the API returns HTTP 400.

OpenAI Responses API does not provide a stable native field for preserving DeepSeek's `reasoning_content`. Codex may also strip custom fields from response items before sending the next request back to the proxy.

## Decision

Store `reasoning_content` server-side, keyed by tool `call_id`, and restore it during Responses → OpenAI Chat conversion when the client does not send it back.

The cache is time-limited and only acts as a compatibility bridge for multi-round tool-call conversations.

## Related

- [`002a-cache-miss-analysis.md`](002a-cache-miss-analysis.md) — Reasoning cache miss 来源分析（结论：无 proxy 漏存，无需改动）。

## Alternatives Considered

### Pass `reasoning_content` through custom Responses fields

Pros:

- Keeps state in the client conversation payload.
- Avoids server-side cache.

Cons:

- Codex strips custom fields.
- Responses API clients are not required to preserve provider-specific fields.
- Not reliable for the main Codex use case.

Rejected because the target client does not reliably round-trip the field.

### Drop `reasoning_content`

Pros:

- Simplest implementation.
- No server-side state.

Cons:

- Breaks DeepSeek thinking mode in multi-round tool-call flows.
- Causes follow-up requests to fail even when the original assistant response was valid.

Rejected because it breaks a core provider compatibility path.

### Store full conversation state server-side

Pros:

- Could support more provider-specific fields.
- Could emulate stateful Responses behavior.

Cons:

- Much larger state-management surface.
- Harder to reason about expiry, isolation, and memory usage.
- More complex than needed for the observed DeepSeek requirement.

Rejected in favor of the minimal cache keyed by `call_id`.

## Consequences

- The proxy has a small amount of transient server-side state.
- Multi-round DeepSeek thinking mode can work even when Codex strips custom fields.
- Cache expiry can still break very long-delayed tool result requests.
- Streaming and non-streaming paths must both store reasoning content consistently.

## Persistence Update (2026-05-03)

### Motivation

Before this update, the cache was in-memory only. Proxy restarts caused `codex resume` sessions to lose cached reasoning and fall back to the space placeholder.

### Decision

Persist cache to disk as JSONL file at `~/.cache/llm-proxy/reasoning_cache.jsonl`.

**Design choices:**
- **JSONL format**: Append-only writes (O(1) per store) instead of rewriting entire file (O(n))
- **TTL extended to 7 days**: Supports `codex resume` after multi-day pauses
- **Compaction every 24 hours**: Removes expired entries and rewrites file
- **CleanupExpired unchanged (5 minutes)**: Only cleans memory, not disk

**Why not JSON with atomic writes?**
- Atomic write requires rewriting entire file on every store
- With 2000 stores/day × 9MB file = 18GB I/O/day
- JSONL append: 2000 × 640 bytes = 1.3MB I/O/day

**Why 7-day TTL?**
- Supports `codex resume` after days-long pauses
- Disk usage: ~9-30MB for 7 days of heavy usage (2000 calls/day)
- Acceptable for modern storage

**Why call_id as sole key (no session ID)?**
- DeepSeek API is stateless, provides no session/conversation ID
- call_id collision probability is negligible
- Collision consequence (wrong reasoning) is better than no reasoning

## Experimental Validation (2026-05-01)

实际 API 测试发现：DeepSeek 的校验是 **presence check**，不是 content check。空字符串 `""` 也能通过，只有 `null` 和省略字段会 400。

这意味着本 ADR 中"Drop reasoning_content"方案的实际影响比预估的要小 — 缓存丢失时 fallback 到空字符串即可，不会导致请求失败。但缓存仍应保留作为优先路径，因为原始推理链对模型效果有价值。

详见 [`experiments/deepseek/reasoning_content_findings.md`](../../experiments/deepseek/reasoning_content_findings.md)。

## Plan B Implementation (2026-05-05)

为了正确表达"显式空字符串 vs 字段缺失"语义，`OpenAIMessage.ReasoningContent` 与 `ResponsesOutputItem.ReasoningContent` 改为 `*string`：

- `nil` → wire 上省略字段（normal path）
- `&""` → wire 上 `"reasoning_content":""`（cache miss 兜底）
- `&"text"` → 真实推理

**为什么不用 `string + omitempty`**：Go `omitempty` 把 `""` 视为 empty 直接省略，使得 cache miss 兜底无法表达"字段存在且为空"，导致 DeepSeek 返回 HTTP 400。早期实现使用单空格 `" "` 作为 hack，但语义上不诚实（客户端可能误读为"模型真的吐了一个空格"）。

`GetReasoning` 接口同步改为 `(string, bool)`，区分 hit/miss——避免用空串当 sentinel 与"hit 但内容为空"混淆。

input 三态严谨化：`responses_to_openai.go` 用 `extractOptionalString` helper 区分 input 中的"显式非空"/"显式空串"/"缺失或 null"，前两种优先于 cache。

## Reliability Hardening (2026-05-06)

Reasoning cache 继续作为服务端状态源，而不是把状态藏进客户端未知字段。理由：`call_id` 跨轮一致是 tool-call 协议配对的核心要求，proxy 依赖它是合理的；相反，依赖 Codex 不剥离自定义字段没有协议保证。

本轮可靠性强化：

- `GetReasoning` 命中后把滑动后的 `expires_at` 追加写入 JSONL，proxy 重启后按 last occurrence wins 恢复最新 TTL。
- 自动 compaction：请求路径上 append 行数估算值 ≥100 且大于 unique entry 2 倍时触发；启动加载时文件 >50MB 也触发；保留原 24h 周期 compaction。
- `loadFromDisk` 使用 8MB scanner buffer，避免长 reasoning 超过 Go 默认 64KB 单行上限被丢。
- reasoning 超过 4MB 时写入前截断到 1MB（保持 UTF-8 边界）并记录 WARN，避免生成将来无法加载的超长 JSONL 行。
- cache 文件加载错误显式 WARN：坏行/坏时间格式会跳过并计数，坏行占比 ≥10% 时告警；文件 >100MB 时告警。
- 对剩余 TTL <24h 的未过期 entry，在启动加载时续到 `now+7d`，降低崩溃/重启临近 TTL 边界时的误 miss 风险。

多租户 namespace / cache key 隔离不纳入本轮，保留为 spec §5.4 已知边界。当前部署目标是单租户/可信客户端。
