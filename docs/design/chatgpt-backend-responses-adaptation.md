# Responses Egress 适配层落实与 openai-sub 接入修复

**状态**: 已合并（2026-08-02）
**日期**: 2026-08-01
**作者**: llm-proxy team

> 本草案已实现并合并：
> - 实现：commit `c071c98`（egress 适配 + 聚合 + catalog）、`d88859e`（anthropic 字符串 content 修复）
> - 设计：§5.1 compat 表新增 `force_stream` / `strip_max_output_tokens`、新增 §5.1a 响应侧契约（design §5.1）
> - 决策：`ADR-023`（聚合类型范围）
> - 调研：`provider-chatgpt-subscription.md` §3.1 请求侧 L1 实测
> - E2E：远程独立验证环境全矩阵验证通过（非流式聚合 / 流式透传 / chat+工具调用 / anthropic 双 content 形式）
> 保留本文档作为实现依据与验证记录。

## 1. 背景与动机

在 openai-sub（ChatGPT 订阅）provider 接入验证过程中，`llm-proxy provider add openai-sub --model ...` 的连通性验证（verify）失败，上游返回 `400 Bad Request: {"detail":"Input must be a list"}`。

深入调研后发现：问题的根源不是 probe 单个函数的问题，而是 **Responses 协议的 egress 适配（design §5.1 的 Layer 0b）在 Rust v2 中未按设计落实**——现有的上游适配以 `provider_id == "openai-sub"` 硬编码形式散落在 responses passthrough 单一路径，未覆盖转换路径，也未由 compat 配置驱动。本草案将硬编码适配收敛为 compat 驱动的 egress 适配层（请求侧 + 响应侧），并修正 openai-sub 的 catalog 模板。

## 2. 上游行为实测（L1 证据）

对 `https://chatgpt.com/backend-api/codex/responses` 的控制变量实测（使用 openai-subscription OAuth token）：

| # | 请求体关键差异 | 响应 | 结论 |
|---|---|---|---|
| 0 | `"input": "字符串"` | 400 `Input must be a list` | `input` 必须为数组 |
| 1 | input 数组，无 `store` | 400 `Store must be set to false` | `store` 必须显式存在 |
| 2 | `"store": true` | 400 `Store must be set to false` | `store` 值必须为 `false` |
| 3 | `"store": false`，`"stream": false` | 400 `Stream must be set to true` | `stream` 必须为 `true` |
| 4 | `"stream": true`，带 `max_output_tokens` | 400 `Unsupported parameter: max_output_tokens` | 不支持该字段 |
| 5 | input 数组 + `store:false` + `stream:true`，无 `max_output_tokens` | 200，SSE 正常返回 | 正确请求体 |

### 2.1 模型可用性实测

`GET https://chatgpt.com/backend-api/codex/models?client_version=0.136.0` 返回权威清单（4 个）：`gpt-5.5`、`gpt-5.4`、`gpt-5.4-mini`、`codex-auto-review`（内部用途）。

catalog 模板逐个实测：

| 上游模型 | 实测 | 结论 |
|---|---|---|
| `gpt-5.5` | 200 | ✅ |
| `gpt-5.4` | 200 | ✅ |
| `gpt-5.4-mini` | 200 | ✅ |
| `gpt-5.3-codex` | 400 `not supported when using Codex with a ChatGPT account` | ❌ 废模板 |
| `gpt-5.2` | 400 同上 | ❌ 废模板 |

### 2.2 与官方 Responses API 的差异

| 字段 | 官方 Responses API | chatgpt backend |
|------|--------------------|-----------------|
| `input` | 字符串或数组 | 仅数组 |
| `store` | 可选，默认 false | 必须显式 false |
| `stream` | 可选，默认 false | 必须 true |
| `max_output_tokens` | 支持 | 不支持（400） |

> 官方 API 行为为官方文档依据（L2），未实测；backend 行为为 L1 实测。backend 的 SSE 输出事件为标准 Responses 格式，但其请求侧校验为标准协议子集 + 更严格约束。

## 3. 现状与根因分析

### 3.1 egress 适配未按 design §5.1 落实（请求侧 + 响应侧）

[design §5.1](../../spec.md)（Layer 0b egress review）规定：发送到 native endpoint 前，按该 endpoint 的 `compat` 声明适配出站 body。现状：

- **compat 字段零消费**：`CompatConfig` 现有 5 个字段（`supports_developer_role` / `supports_reasoning_effort` / `thinking_format` / `requires_reasoning_content_on_assistant_messages` / `max_tokens_field`）在 `proxy.rs` / `convert.rs` 运行时**无任何消费点**——仅解析、校验、TUI 展示（`config.rs` 的 `effective_*` 访问器带 `#[allow(dead_code)]` 佐证）。
- **请求侧硬编码特判**：`forward_responses_native`（proxy.rs:1656）中 `if plan.provider_id == "openai-sub"` 块实现 store/stream/max_output_tokens/input 适配，只覆盖 responses **passthrough** 单一路径。
- **响应侧硬编码特判**：同一函数（proxy.rs:1687）的 `else if plan.provider_id == "openai-sub"` 分支——非流式客户端请求被强制 stream 后，将上游 SSE 聚合回 JSON（`aggregate_responses_sse_to_json`）。同样以 provider_id 硬编码。
- **转换路径完全无适配**：`forward_chat_via_responses`（proxy.rs:1512）与 `forward_anthropic_via_responses`（proxy.rs:1967）直接发送 convert 产物（缺 store / 可能带 max_output_tokens / stream 透传客户端值），**实测必 400**；其响应分支（`body.stream ? SSE 转换 : JSON 转换`）在强制 stream 后对非流式客户端会返回错误格式。

### 3.2 probe 独立构造 body（ADR-017 未在 Rust v2 落实）

[`ADR-017`](../decisions/017-probe-reuses-proxy-body-logic.md)（Go v1 决策）要求探测请求复用正常转发的 body 构建逻辑。Rust v2 中 `src/probe.rs` 的 `probe_body_with_auth` 独立构造探测体（`connect.rs:453` verify 与 `status.rs:300` 探测共用），不含上游适配 → 探测 400，与真实转发行为不一致。

### 3.3 catalog openai_sub 模板含废模型

`openai_sub()` 模板 5 个模型中 `gpt-5.3-codex-sub-lp`、`gpt-5.2-sub-lp` 上游不可用（§2.1），新增即失败。

### 3.4 运行时路径现状汇总

| 路径 | 请求侧 | 响应侧 | 运行时状态 |
|---|---|---|---|
| responses passthrough（openai-sub） | 硬编码特判（proxy.rs:1656） | 硬编码聚合（proxy.rs:1687） | ✅ |
| chat→responses 转换（openai-sub chat 端点） | 无适配 | 无强制流式聚合 | ❌ 必 400 |
| anthropic→responses 转换（openai-sub anthropic 端点） | 无适配 | 无强制流式聚合 | ❌ 必 400 |
| probe（connect verify / status） | 无适配 | 仅检查 2xx，不解析 body | ❌ 必 400 |

## 4. 修复方案

### 4.1 核心：compat 驱动的 egress 适配层（请求侧，落实 design §5.1）

新增单点 truth 适配函数 `apply_responses_egress_compat(body, compat)`，应用于**所有发往 Responses native endpoint 的出站 body**（转换产物、passthrough、probe）：

| 适配动作 | 触发 | 语义 |
|---|---|---|
| `input` 字符串 → 数组 | 内建（无条件） | 官方 API 接受数组，等价转换，无副作用 |
| 插入 `store: false` | 内建（set-if-absent） | 仅当字段**缺失**时插入 false；官方 API 默认即 false，无副作用。客户端显式 `store:true` 不覆盖（官方合法语义），openai-sub 上仍会被上游拒绝，属客户端问题（已知限制，见 §6） |
| 强制 `stream: true` | `compat.force_stream = true` | 覆盖客户端值（openai-sub 必需） |
| 移除 `max_output_tokens` | `compat.strip_max_output_tokens = true` | openai-sub 必需 |

- **不配置** `force_stream` / `strip_max_output_tokens` → 官方 API 标准行为零影响。
- **替换** `forward_responses_native` 中两处 `provider_id == "openai-sub"` 硬编码块。
- **应用位置**：统一在发送函数入口调用（`forward_responses_native`、`forward_chat_via_responses`、`forward_anthropic_via_responses`），签名改为 `mut responses_body`。**不变式：所有发往 Responses native endpoint 的出站 body 必经该函数**，各路径加断言测试锁住。

### 4.2 响应侧契约（补齐审查缺口，必修）

`force_stream` 生效时上游永远返回 SSE，响应形态按**客户端原始请求**的 `stream` 值决定：

| 客户端请求 `stream` | 响应处理 |
|---|---|
| `true` | 现有 SSE→SSE 转换路径（`responses_sse_to_chat_sse` / `responses_sse_to_anthropic_sse` 等） |
| `false` / 缺省 | **聚合 SSE 为 JSON** → 走既有 JSON 协议转换（`responses_to_chat_response` / `responses_to_anthropic_response`） |

- `forward_responses_native`：响应分支改为 `client_wants_stream ? SSE 重写 : force_stream ? 聚合 : JSON`（`force_stream` 读自 `plan.compat`，替换 provider_id 特判；聚合函数 `aggregate_responses_sse_to_json` 复用）。
- `forward_chat_via_responses` / `forward_anthropic_via_responses`：新增"聚合 SSE → JSON 协议转换"分支。响应分支判定不能再用适配后 body 的 `stream`（已被强制为 true）。**不变式：`client_wants_stream` 必须在调用 `apply_responses_egress_compat` 之前记录**（现有代码已先读 `stream_response` 再发请求，顺序正确，实现时须保持并在各路径以断言锁住）。聚合后须保留 usage 统计（现有聚合分支已提取 usage，转换路径补聚合时保持一致）。
- **聚合函数补工具调用事件（必修）**：`aggregate_responses_sse_to_json` 现仅处理 `response.created` / `response.output_text.delta` / `response.completed`，上游事件流中的 `response.output_item.added`（type=function_call）与 `response.function_call_arguments.delta` / `.done` 被 `_ => {}` 丢弃——非流式客户端的**工具调用静默丢失**，`responses_to_chat_response` 的 function_call 分支（convert.rs:890）与 `responses_to_anthropic_response` 的 tool_use 分支（convert.rs:1179）永不可达，且 `responses_status_to_anthropic_stop`（convert.rs:1255）依赖 output 中 function_call 项决定 `stop_reason="tool_use"`——修复须同时补齐 **content 与 stop_reason 双语义**。该缺陷已存在于现有硬编码聚合分支（proxy.rs:1687，非本次引入），但本次将聚合正式化为通用响应侧契约，须一并修复：聚合 `output` 数组除 message 项外，收集 function_call 项（call_id / name / 累积 arguments），结构与 Responses API 标准一致。
  - **多 function_call 匹配**：`function_call_arguments.delta` 事件携带 `item_id`，`output_item.added` 的 function_call item 携带 `id`/`call_id`——两个 id 空间需统一匹配（item 的 `id` 即后续事件的 `item_id`），单测须覆盖两个 function_call 交错 delta 的独立累积（多工具并行是 coding agent 常规场景）。
  - **联动清理**：`forward_responses_native` 中适配块之后残留的 `let _stream_response = body.get("stream")...`（proxy.rs:1670，下划线前缀未使用）随硬编码块一并删除，避免误导后续读者。
- probe 路径无响应侧问题：probe 成功路径仅校验 2xx、不解析 body（`connect.rs` / `status.rs`），强制 stream 不引入额外处理。

### 4.3 probe 复用 egress 适配（落实 ADR-017）

`probe_body_with_auth` 构造基础 body 后调用 `apply_responses_egress_compat`（基于 `plan.compat`），`connect` verify 与 `status` 探测同时修复。

### 4.4 catalog openai_sub 模板修正

- 移除废模板：`gpt-5.3-codex-sub-lp`、`gpt-5.2-sub-lp`。
- 为 `openai_responses` native endpoint 配置 compat：
  ```toml
  [providers.openai-sub.openai_responses.compat]
  force_stream = true
  strip_max_output_tokens = true
  ```

### 4.5 后续（不在本次范围）

egress 适配层建立后，现有 5 个 compat 字段（`thinking_format`、`max_tokens_field` 等）的消费可作为后续迭代按相同机制补全。本次只落地 Responses 相关字段。

## 5. 配置项设计

新增 `CompatConfig` 字段（`src/config.rs`，可选，向后兼容）：

| 字段 | 类型 | 默认 | 语义 |
|------|------|------|------|
| `force_stream` | `Option<bool>` | `None`（false） | 为 true 时出站 body 强制 `stream: true`，并按 §4.2 处理响应形态 |
| `strip_max_output_tokens` | `Option<bool>` | `None`（false） | 为 true 时移除 `max_output_tokens` 字段 |

两者均挂 **native endpoint** 的 `compat`（derived endpoint 不得声明 compat，继承 source chain native endpoint，校验已存在，符合 design §5.1 约束）。

## 6. 影响面与兼容性

| 路径 | 改动 | 影响 |
|------|------|------|
| responses passthrough | 请求/响应硬编码 → compat 驱动 | 行为等价，消除硬编码 |
| chat→responses 转换 | 发送前适配 + 强制流式聚合 | openai-sub 的 `/v1/chat/completions` 路径修复（流式/非流式均正确） |
| anthropic→responses 转换 | 发送前适配 + 强制流式聚合 | openai-sub 的 `/v1/messages` 路径修复（流式/非流式均正确） |
| probe（connect/status） | 复用适配 | 探测与真实转发一致（ADR-017） |
| 官方 API provider | 不配置新字段 | 内建适配（input 数组、store set-if-absent）对其无副作用，行为不变 |
| 配置 schema | 新增 2 个可选字段 | 向后兼容 |

**已知限制**：客户端显式 `store:true` 的请求在 openai-sub 上仍会被上游拒绝（backend 强制 false，覆盖显式值会破坏官方 API 合法语义，故不覆盖）。该场景属客户端使用问题，由上游 400 反馈。

## 7. 验证计划

1. `cargo test`：新增 `apply_responses_egress_compat` 单测（含 set-if-absent / force / strip / input 归一化各场景）；**新增 `aggregate_responses_sse_to_json` 单测**（现为零测试，覆盖：文本聚合、usage 提取、**工具调用事件聚合**）；更新受影响的 proxy/probe 测试；catalog 模板数量断言同步。
2. `provider add openai-sub`（修正后的 3 个模型）verify 通过；`llm-proxy status` 探测通过。
3. 端到端（经代理）：
   - chat 请求（流式 + 非流式）走 openai-sub 返回正确格式（非流式收聚合 JSON）；
   - anthropic 请求（流式 + 非流式）走 openai-sub 返回正确格式；
   - **非流式 + 工具调用场景**：agent 发起工具调用，确认聚合后 function_call 不丢失（coding agent 核心场景）；
   - responses passthrough 回归（Codex 场景）。
4. 模型实测：catalog 剩余 3 个模板（gpt-5.5/gpt-5.4/gpt-5.4-mini）探测 200。
5. 回归：未配置新 compat 字段的 provider（官方 API）请求/响应行为不变（含显式 `store:true` 透传场景）。
6. 文档项（项目工作流）：spec/design §5.1 compat 表并入 2 个新字段、ADR 记录、ROADMAP 更新、draft 合并后删除。
