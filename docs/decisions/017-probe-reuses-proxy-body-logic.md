# ADR-017: 探测请求复用正常转发的 body 构建逻辑

## Status

Accepted

## Date

2026-07-06

## Context

`llm-proxy status` 命令通过 `internal/probecache/scheduler.go` 发送探测请求，检验上游 provider 是否可达。同时，`internal/proxy/forward_helpers.go` 中的 `prepareResponsesRequestBody` 等函数负责实际转发时的 body 构建，包括各 provider 的特殊转换（如 ChatGPT subscription 的 OAuth 要求 `store: false`、删除 `max_output_tokens`）。

**问题**：scheduler 独立维护了一套 body 构建逻辑（`openAIProbeBody`、`anthropicProbeBody`、`responsesProbeBody` 三组 struct），绕过了 proxy 层的转换。这导致：

- **False negative**：ChatGPT subscription provider 的探测请求因为缺少 `store: false` 等转换，返回 HTTP 400，显示为"❌ 不可达"，但实际转发是正常的。
- **False positive 可能**：probe 用简单 body 碰巧通过，但实际转发的复杂请求因某种转换规则被拒绝。
- **维护成本**：body 构建逻辑在 scheduler 和 proxy 中重复维护，容易不同步。
- **职责混乱**：scheduler 本应只负责"如何发送请求"（并发、超时、分组），却承担了"请求体怎么构造"的知识。

核心矛盾：probe 和正常请求的唯一区别应该是 prompt 不同（一个短提示词 vs 用户真实请求）。其他差异都不应该存在，因为探测结果的目的是**真实反映服务状态**。如果探测逻辑和实际转发逻辑不一致，探测就失去了意义。

## Decision

**body 构建逻辑单点 truth 在 proxy 层**。探测请求通过调用 proxy 层的 body 准备函数获取 body，然后由 scheduler 发送。

### 设计原则

- **单点 truth**：body 构建逻辑只有一份，在 proxy 层（`internal/proxy/forward_helpers.go` 中的 `Prepare*RequestBody` 导出函数）。probe 调用它获取 body，不自己维护另一份。
- **职责分离**：
  - `internal/proxy`：知道"请求体该怎么构造"（body 转换、provider 特殊要求、认证参数）
  - `internal/probecache/scheduler`：知道"请求该怎么发送"（并发控制、超时、间隔、分组），只接收预构建的 body 并发送 HTTP 请求
  - `cmd/llm-proxy/status.go`（编排者）：从 config 读取目标列表 → 调用 proxy 构建 body → 交给 scheduler 发送 → 展示结果

### 实现方案

1. **`internal/proxy/forward_helpers.go`**：导出 `PrepareResponsesRequestBody`（首字母大写），使 `status.go` 可以调用。
2. **`internal/probecache/scheduler.go`**：`ProbeTarget` 添加 `RequestBody []byte` 字段（非空时直接用此 body 发送，跳过 scheduler 自建）。移除 `openAIProbeBody`、`anthropicProbeBody`、`responsesProbeBody` 三组 struct 和 builder 函数，消除 scheduler 中的 body 构建逻辑。
3. **`cmd/llm-proxy/status.go`**：`runScheduledProbe` 中对三种 provider type（`openai-chat`、`openai-responses`、`anthropic`）分别调用 proxy 导出函数构建 body，填入 `ProbeTarget.RequestBody`。
4. **（可选）`internal/config/connect.go`**：`ValidateConnectivity` 改用 proxy 导出函数（消除第三份重复 body 构建逻辑）。

依赖关系：

```
internal/proxy (拥有 body 构建逻辑，导出函数)
     ↑ 调用
cmd/llm-proxy/status.go (编排者：构建 body → 塞进 ProbeTarget → 交给 scheduler)
     │
     ↓ 只发送 bytes，不关心 body 内容
internal/probecache/scheduler.go (纯 HTTP 发送器)
```

`internal/proxy` 已经 import `internal/probecache`（proxy 运行时更新 probe cache），所以 `probecache` 不能反向 import `proxy`（循环依赖）。通过让 `status.go` 作为编排者，既避免了循环依赖，又保持了职责清晰。

## Alternatives Considered

### Option A: 可选的 RequestBody 字段

`scheduler.go` 保留自建的 body 构建逻辑作为 fallback，新增的 `RequestBody` 字段可选，仅在特殊 provider（如 OAuth）时使用。

**拒绝原因**：仍然是"补丁"方案，没有解决根本的设计问题。两套 body 构建逻辑仍然并存，长期维护成本高，容易不同步。违背"单点 truth"原则。

### Option B: 让 scheduler import proxy

`internal/probecache/scheduler.go` 直接 import `internal/proxy`，在 `checkModelRoute` 中调用 `proxy.PrepareRequestBody`。

**拒绝原因**：Go 不允许循环依赖。`internal/proxy` 已经 import `internal/probecache`，反向 import 会导致编译失败。即使通过抽象层绕过，也会增加复杂度。

### Option C: 提取公共包

新建 `internal/probebody` 包，将 body 构建逻辑从 `proxy` 移到这个公共包，让 scheduler 和 proxy 都 import 它。

**拒绝原因**：过度设计。body 构建逻辑本质上是 proxy 层的知识（涉及 provider 转换、OAuth 要求等），不应该抽离到公共包。增加一个包会增加复杂度，没有明显收益。

## Consequences

### 收益

- **探测结果真实可靠**：探测请求使用与正常转发相同的 body 构建逻辑，结果真实反映服务状态。
- **消除 false negative/positive**：ChatGPT subscription 等 OAuth provider 的探测不再因为缺少转换而误报。
- **单点维护**：body 构建逻辑只在 proxy 层，新增 provider 特殊要求时只需改一处。
- **职责清晰**：scheduler 专注发送，proxy 专注构建，status.go 负责编排。

### 代价

- **需要导出函数**：`prepareResponsesRequestBody` 等函数需要改为导出（首字母大写），增加了 API 暴露面。
- **调用方复杂度增加**：`status.go` 需要构建请求对象并调用 proxy 函数，比之前直接传 struct 稍复杂。

### 后续影响

- 新增 provider 类型或 body 转换规则时，只需在 proxy 层修改，scheduler 无需改动。
- `internal/config/connect.go` 的 `ValidateConnectivity` 可以类似重构（消除第三份重复），但非强制。
- 未来如果需要更复杂的探测逻辑（如带 thinking 参数的请求），只需在 proxy 层新增函数，scheduler 无感知。
