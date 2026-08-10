# ADR-014: 上游客户端自行实现，不引入官方 SDK

## 状态

Accepted

## 日期

2026-06-24

## 背景

llm-proxy 需要调用多种上游 LLM provider（百炼、stepfun、DeepSeek、智谱、OpenRouter、Ollama、Anthropic 等）。这些 provider 的端点格式差异很大：

- 标准 OpenAI 兼容：`https://xx.com/v1/chat/completions`
- 带前缀的 OpenAI 兼容：`https://coding.dashscope.aliyuncs.com/v1/chat/completions`
- Anthropic 兼容：`https://xx.com/v1/messages`
- 带前缀的 Anthropic 兼容：`https://coding.dashscope.aliyuncs.com/apps/anthropic/v1/messages`
- 自定义路径：`https://xx.com/inference`、`https://xx.com/api/llm/query` 等
- 甚至只有裸域名：`https://xx.com`

团队评估了是否引入官方 SDK（`openai-go`、`anthropic-sdk-go`）作为上游调用客户端，并在 `experiments/sdk-eval/` 完成了 PoC，在 内部评估文档 和 内部评估文档 记录了详细评估。

## 决策

**不引入官方 SDK，llm-proxy 自行实现上游 HTTP 客户端。** 具体包括：

1. 自行实现 HTTP 请求发送（含 SSE 流式解析）
2. 自行实现请求/响应侧的非标字段（extra fields）机制
3. 自行实现重试逻辑（退避公式、`Retry-After` 解析、`x-should-retry` 处理）
4. 自行实现错误分类（按 HTTP 状态码 + provider 特定语义）
5. 保留对任意端点路径格式的支持，不假设固定后缀

## 替代方案

### Option A: 引入官方 SDK（`openai-go` + `anthropic-sdk-go`）

**优点**：

- 错误分类开箱即用（OpenAI 系 ~8 种、Anthropic 系 9 种）
- 重试逻辑符合官方推荐（退避公式、`Retry-After` 解析、`x-should-retry` 自动处理）
- 协议演进免费跟进（新字段随 SDK 发版）
- 类型安全（Responses API 双层错误自动转 error）
- 两个 SDK 同源（Stainless），API 对称，学习成本可复用

**缺点**：

- **端点路径硬编码**：SDK 强制在 baseURL 后拼接固定路径（`/chat/completions`、`/v1/messages` 等），无法关闭或覆盖。任何端点路径不符合此结构的 provider 都无法直接使用
- `anthropic-sdk-go` 对接非 Anthropic 官方 provider 几乎没有公开案例，遇到问题无社区可查
- `anthropic-sdk-go` 的 typed stream 会静默丢弃未知 event type，需要走 `WithResponseInto` 自 parse，削弱了 typed 的价值
- 二进制体积增大 5-10MB（Go 静态链接）
- 仍需保留 `RawHTTPClient` 兜底路径处理非标 provider，形成双轨维护

**拒绝原因**：

端点路径硬编码与 llm-proxy 的核心定位冲突。llm-proxy 的价值之一是"任意 provider 接进来"，配置层不应限制 provider 端点必须符合 `/v1/chat/completions` 或 `/v1/messages` 结构。一旦引入 SDK，对非标路径 provider 必须走另一条路径，形成永久性的双轨架构，维护成本高于收益。

### Option B: 混合方案（标准路径用 SDK，非标路径用 RawHTTPClient）

**优点**：

- 多数 provider 能享受 SDK 的类型安全和自动重试
- 非标 provider 仍可通过 RawHTTPClient 接入

**缺点**：

- 两套实现长期并存，测试、文档、错误处理行为需要分别维护
- provider 路径格式未来可能变化，SDK 实现和 RawHTTPClient 实现之间的切换逻辑复杂
- 用户配置心智负担增加（需要理解哪些 provider 走 SDK、哪些走手写）

**拒绝原因**：

双轨架构的长期维护成本超过 SDK 带来的收益。且"哪些 provider 走 SDK"的判据本身需要进 spec，增加配置复杂度。

## 补充决策：不实现通用 extra fields 双向通路

SDK 评估文档里把 extra fields（请求/响应侧的非标字段通路）列为"重要能力"。调研阶段也曾考虑把 `SetExtraFields` / `JSON.ExtraFields` 机制纳入自实现范围。最终决定**不实现通用的 extra fields 机制**。

### 现状

llm-proxy 当前已有一个窄通道处理非标字段：`types.OpenAIChatRequest.ExtraBody map[string]interface{}`（`internal/types/openai.go:21`），在序列化时展开到顶层 JSON。这个通道**仅有一处生产用途**：

- `internal/proxy/thinking_adapter.go:55-58`：为 StepFun 注入 `reasoning_format: "deepseek-style"`

除此之外，各 provider 调研文档中没有发现其他需要非标字段注入的场景。DeepSeek 的 `top_k` 被文档明确标注"忽略"；OpenRouter 的 `reasoning.effort` 通过 thinking adapter 路径处理；其他 provider 均未报告非标字段需求。

### 客户端侧

llm-proxy 服务的客户端全部是标准 coding agent（Codex、pi、Claude Code、Qwen Code）。这些客户端只发送标准协议字段，不存在需要透传非标字段的场景。`handler.go` 解析客户端请求时，`ExtraBody` 字段因 `json:"-"` 标签直接被丢弃——没有人需要它。

### 响应侧

Response 类型里没有任何 extra 字段机制。上游返回的非标字段在 `json.Unmarshal` 时全部丢弃——也没有客户端需要它们。

### 决策理由

1. **没有真实需求**：所有现存的非标字段需求都已被 `ExtraBody` 这个窄通道满足，且只有一处生产用途。通用双向通路是 YAGNI。
2. **SDK 有 extra fields 是因为 SDK 是通用库**：`openai-go` / `anthropic-sdk-go` 要服务各种业务场景，必须提供逃生舱。llm-proxy 是专用工具（面向 coding agent 的本地代理），不需要通用逃生舱。
3. **窄通道比通用机制更可控**：如果未来某个 provider 真的需要新非标字段（例如新的 thinking 格式），在 `thinking_adapter.go` 里加专项处理比开放通用逃生舱更清晰——逻辑集中、可测试、可追溯。
4. **避免"配置即功能"陷阱**：`providers.extra.request_fields` 这种配置项会让用户误以为 llm-proxy 能自动处理各种非标字段，实际上字段语义还是要 proxy 显式实现。配置项带来的心智负担超过它的价值。

### 影响

- 通用 extra fields 双向通路**不在 upstream client 自实现范围内**
- 现有 `types.OpenAIChatRequest.ExtraBody` 窄通道**保留**，继续作为未来可能出现的非标字段注入点
- 如果未来出现新非标字段需求，按既有方式在专项 adapter 里处理（参考 `thinking_adapter.go` 的模式），不开通用口子
- 对应的 spec draft 章节（extra 字段机制、配置层 `providers.extra`）从 draft 中删除

---

## 影响

### 收益

- **端点路径完全自由**：provider 配置不限制端点格式，符合 llm-proxy "任意 provider" 的核心定位
- **单一实现路径**：所有 provider 走同一套 HTTP 客户端，无双轨维护负担
- **配置心智简单**：用户只需填 baseURL + apiKey，不需要理解底层实现差异

### 代价

- **工作量增加**：需要自行实现 SSE 解析、重试逻辑、错误分类等能力
- **协议跟进成本**：OpenAI / Anthropic 协议演进时，需要手动跟进新字段和新行为
- **没有官方背书**：重试公式、错误分类等实现需要自己验证正确性

### 调研文档的角色转换

内部评估文档 和 内部评估文档 中的评估结论，**从"是否引入 SDK"的决策依据，变为"自行实现时必须做到什么程度"的规格书**。两份文档中的能力清单（重试触发条件、退避公式、Retry-After 解析、错误类型等）成为自行实现的 checklist。**注意**：extra 字段机制经评估为 YAGNI，不纳入 checklist（见本节"补充决策"）。

### 实现路径

按 `docs/decisions/001-direct-protocol-conversion.md` 的既有决策，协议转换采用直接 A → B 转换。上游客户端的实现与此正交——客户端只负责把 B 协议的请求发到上游、把响应拿回来，不做协议转换。

## 相关文件

- 内部评估文档 — OpenAI Go SDK 评估（现作为自实现 checklist）
- 内部评估文档 — Anthropic Go SDK 评估（现作为自实现 checklist）
- `experiments/sdk-eval/` — SDK PoC 代码（验证了 SDK 能力，但决策不走 SDK）
- `docs/decisions/001-direct-protocol-conversion.md` — 协议转换策略（与客户端实现正交）
- `docs/spec.md` — 当前行为规范
- `docs/spec-provider-cooldown.md` — Provider cooldown 策略（错误分类是其上游依赖）

## 备注

本决策做出后，原计划写 spec draft 定义 `UpstreamClient` 抽象。但在讨论中发现，**当前最紧迫的问题是上游错误信息透传不足**（客户端看不到上游 error type/code、request ID、真实状态码）。

因此当前优先实现 `docs/drafts/upstream-error-forwarding.md`（上游错误协议映射），范围小、风险低、直击痛点。完整的 `UpstreamClient` 抽象（如果最终需要）推迟到错误转发改进稳定后再评估。

`UpstreamClient` 抽象的完整 spec draft 曾写在 `docs/archive/upstream-client-self-impl-withdrawn.md`，已撤回但保留作为参考。
