# LLM Proxy (Rust v2) — 全局文档索引与导航 (Documentation Index)

本文档是 `llm-proxy-rust-v2` 项目的**官方文档导航主页**。无论您是人类开发者还是 AI Agent，均可通过本索引快速跳转定位所���的设计规范、架构决策、调研报告与 SOP 操作指南。

---

## 📌 一、 核心总控文档

| 文档 | 作用与定位 |
| :--- | :--- |
| **[`README.md`](../README.md)** | 项目入口门面：一句话定位、极简 Quickstart 与常用命令。 |
| **[`CONTRIBUTING.md`](../CONTRIBUTING.md)** *(软链接至 `AGENTS.md`)* | 开发者与 AI Agent 操作指南：技术栈说明、`just` 构建测试指令、代码目录结构及 Spec-Driven 工作流。 |
| **[`ROADMAP.md`](../ROADMAP.md)** | **最高权威进度表**：Rust v2 远景目标、各 Milestone 进度、已收敛 Scope 及计划。 |
| **[`issues/README.md`](../issues/README.md)** | 需求与 Bug 跟踪矩阵：已接受 Feature Requests、已归档 Bug 与待评估列表。 |

---

## 📐 二、 架构决策记录 (ADRs — `docs/decisions/`)

记录项目演进过程中的关键架构决策与技术选型（Architecture Decision Records）：

### 核心协议与转换
- [`001-direct-protocol-conversion.md`](decisions/001-direct-protocol-conversion.md) — 6 方向直接协议转换架构设计
- [`002-server-side-reasoning-cache.md`](decisions/002-server-side-reasoning-cache.md) — 服务端 Reasoning Cache 设计
- [`002a-cache-miss-analysis.md`](decisions/002a-cache-miss-analysis.md) — Reasoning Cache Miss 来源分析
- [`004-codex-responses-wire-api.md`](decisions/004-codex-responses-wire-api.md) — Codex Responses API 报文兼容设计
- [`010-thinking-format-extension.md`](decisions/010-thinking-format-extension.md) — Thinking 思考过程格式扩展与传递
- [`017-probe-reuses-proxy-body-logic.md`](decisions/017-probe-reuses-proxy-body-logic.md) — 探测请求复用正常转发的 body 构建逻辑
- [`018-model-all-protocols-ingress.md`](decisions/018-model-all-protocols-ingress.md) — 模型全协议入站统一转发设计
- [`023-responses-aggregation-type-scope.md`](decisions/023-responses-aggregation-type-scope.md) — Responses 聚合只保留 message/function_call，SSE 透传全量转发

### 配置、策略与存储
- [`003-config-driven-provider-models.md`](decisions/003-config-driven-provider-models.md) — 配置驱动的 Provider 与 Model 管理
- [`005-provider-cooldown-policy.md`](decisions/005-provider-cooldown-policy.md) — Provider 熔断冷却与故障恢复策略
- [`006-config-migration-policy.md`](decisions/006-config-migration-policy.md) — 配置文件平滑迁移策略
- [`007-simple-model-provider-configuration.md`](decisions/007-simple-model-provider-configuration.md) — 保持 Model/Provider 配置心智模型简单
- [`008-proxy-must-protect-upstream.md`](decisions/008-proxy-must-protect-upstream.md) — Proxy 对上游 API 的过载与无效请求保护机制
- [`013-features-at-model-level.md`](decisions/013-features-at-model-level.md) — Features 声明在 Model 级别而非 Provider 级别
- [`015-oauth-polling-safety-margin.md`](decisions/015-oauth-polling-safety-margin.md) — OAuth 设备码轮询 Safety Margin 防限流设计
- [`016-token-refresh-retry-singleflight.md`](decisions/016-token-refresh-retry-singleflight.md) — Token 刷新并发 Singleflight 与重试控制
- [`017-secure-defaults-user-override.md`](decisions/017-secure-defaults-user-override.md) — 安全默认值 + 用户覆盖 > 强制限制
- [`019-duplication-boundary-tradeoff.md`](decisions/019-duplication-boundary-tradeoff.md) — 重复代码的边界权衡——DRY 不应跨越独立变化单元
- [`021-unify-toml-library-and-roundtrip-boundary.md`](decisions/021-unify-toml-library-and-roundtrip-boundary.md) — TOML 解析库统一与 roundtrip 保留格式边界
- [`022-rust-v2-config-editing-boundary.md`](decisions/022-rust-v2-config-editing-boundary.md) — Rust v2 配置编辑与内存修改边界

### Provider 与产品决策
- [`009-kimi-code-anthropic-only.md`](decisions/009-kimi-code-anthropic-only.md) — Kimi Code 仅使用 Anthropic 兼容端点
- [`011-no-claude-code-gateway-discovery.md`](decisions/011-no-claude-code-gateway-discovery.md) — 暂不实现 Claude Code gateway 模型发现
- [`014-self-implement-upstream-client.md`](decisions/014-self-implement-upstream-client.md) — 上游客户端自行实现，不引入官方 SDK
- [`020-antigravity-trainable-never.md`](decisions/020-antigravity-trainable-never.md) — Antigravity `trainable=NEVER` 决策废弃

### 进程模型
- [`012-no-daemon-only-background.md`](decisions/012-no-daemon-only-background.md) — `--daemon` 改名为 `--background`，不实现系统服务集成

---

## 🎨 三、 确立的设计方案 (`docs/design/`)

通过 Spec Draft 审查并已合并落地的完整设计方案：

### 核心架构
- [`rust-v2-implementation-design.md`](design/rust-v2-implementation-design.md) — **Rust v2 总体实现架构设计**（Axum + Tokio + Rusqlite）
- [`core-architecture-design.md`](design/core-architecture-design.md) — 核心架构设计（Status/Usage/Delegation 等 §12-§14）
- [`core-code-definition.md`](design/core-code-definition.md) — 核心代码定义与覆盖率目标

### 协议转换
- [`chatgpt-backend-responses-adaptation.md`](design/chatgpt-backend-responses-adaptation.md) — ChatGPT Backend Responses 适配方案
- [`model-format-family-and-antigravity-bindings-design.md`](design/model-format-family-and-antigravity-bindings-design.md) — 模型格式族与 Antigravity 绑定设计

### 认证与 OAuth
- [`oauth-accounts-storage.md`](design/oauth-accounts-storage.md) — OAuth 账号凭证与 Token 持久化 SQLite 存储设计
- [`oauth-flows.md`](design/oauth-flows.md) — OAuth 登录流程设计

### Status 与性能
- [`status-singleflight-review-checklist.md`](design/status-singleflight-review-checklist.md) — Status 命令 Singleflight 审查检查清单
- [`status-probe-performance.md`](design/status-probe-performance.md) — Status Probe 性能分析与优化

### 其他功能
- [`proxy-module-refactoring.md`](design/proxy-module-refactoring.md) — Proxy 模块重构方案
- [`quota-query-feature.md`](design/quota-query-feature.md) — 额度查询功能设计
- [`rust-v2-tui-design.md`](design/rust-v2-tui-design.md) — Rust v2 Ratatui TUI 界面设计
- [`token-usage-statistics-design.md`](design/token-usage-statistics-design.md) — Token 使用量统计设计（早期方案，已演进为 core-architecture §14）
- [`config-hot-reload.md`](design/config-hot-reload.md) — ~~配置热重载~~（❌ 已放弃，保留作为决策记录）

---

## 📋 四、 测试与实施计划 (`docs/plans/`)

测试策略、覆盖率提升方案与实施计划：

- [`test-coverage-improvement.md`](plans/test-coverage-improvement.md) — 核心模块覆盖率提升计划
- [`test-design-proxy.md`](plans/test-design-proxy.md) — Proxy 模块测试设计
- [`test-design-service-core.md`](plans/test-design-service-core.md) — Service Core 模块测试设计
- [`test-design-status-cooldown.md`](plans/test-design-status-cooldown.md) — Status/Cooldown 模块测试设计
- [`test-design-connect-auth.md`](plans/test-design-connect-auth.md) — Connect/Auth 模块测试设计
- [`unit-test-supplement.md`](plans/unit-test-supplement.md) — 单元测试补充计划
- [`integration-test-design.md`](plans/integration-test-design.md) — 集成测试方案设计（Feature 门控，暂缓实施）

---

## 🔬 五、 技术调研与 API 镜像抓包 (`docs/research/`)

对客户端、上游 Provider 和三方协议的深层调研与抓包备份：

### 1. 客户端协议 (`research/clients/`)
- [`codex-tool-format.md`](research/clients/codex-tool-format.md) — Codex CLI Tool Call 格式与消息排版
- [`codex-desktop.md`](research/clients/codex-desktop.md) — Codex Desktop 接入机制
- [`pi.md`](research/clients/pi.md) — pi CLI 接入与协议抓包
- [`qwen-code.md`](research/clients/qwen-code.md) — Qwen Code 客户端行为特性
- [`claude-code.md`](research/clients/claude-code.md) — Claude Code 接入分析
- [`claude-desktop.md`](research/clients/claude-desktop.md) — Claude Desktop 接入分析

### 2. 协议转换与 Error Code (`research/protocols/`)
- [`conversion-mapping.md`](research/protocols/conversion-mapping.md) — Chat Completions ↔ Responses ↔ Messages 字段映射字典
- [`api-conversion-analysis.md`](research/protocols/api-conversion-analysis.md) — API 转换边界与损失分析
- [`error-codes-comparison.md`](research/protocols/error-codes-comparison.md) — 上游 Provider Error Code 与标准 HTTP 状态码映射比较
- [`anthropic-messages-api.md`](research/protocols/anthropic-messages-api.md) — Anthropic Messages API 协议详解
- [`anthropic-messages-error-codes.md`](research/protocols/anthropic-messages-error-codes.md) — Anthropic Messages 错误码
- [`chat-completions-api.md`](research/protocols/chat-completions-api.md) — OpenAI Chat Completions API 协议详解
- [`chat-completions-error-codes.md`](research/protocols/chat-completions-error-codes.md) — Chat Completions 错误码
- [`responses-api-format.md`](research/protocols/responses-api-format.md) — OpenAI Responses API 报文格式
- [`responses-api-error-codes.md`](research/protocols/responses-api-error-codes.md) — Responses API 错误码
- [`responses-api-implementation-guide.md`](research/protocols/responses-api-implementation-guide.md) — Responses API 实现指南
- [`anthropic-sdk-go-evaluation.md`](research/protocols/anthropic-sdk-go-evaluation.md) — Anthropic Go SDK 评估
- [`openai-go-evaluation.md`](research/protocols/openai-go-evaluation.md) — OpenAI Go SDK 评估

### 3. Provider 调研 (`research/providers/`)
- [`rust-v2-current-provider-catalog.md`](research/providers/rust-v2-current-provider-catalog.md) — 官方支持 Provider Catalog 清单
- [`provider-deepseek.md`](research/providers/provider-deepseek.md) — DeepSeek 调研
- [`provider-openai.md`](research/providers/provider-openai.md) — OpenAI 调研
- [`provider-anthropic.md`](research/providers/provider-anthropic.md) — Anthropic 调研
- [`provider-antigravity.md`](research/providers/provider-antigravity.md) — Google Antigravity 调研
- [`provider-chatgpt-subscription.md`](research/providers/provider-chatgpt-subscription.md) — ChatGPT Subscription 调研
- [`provider-kimi.md`](research/providers/provider-kimi.md) — Kimi 调研
- [`provider-mimo.md`](research/providers/provider-mimo.md) — MiMo 调研
- [`provider-ollama.md`](research/providers/provider-ollama.md) — Ollama 调研
- [`provider-openrouter.md`](research/providers/provider-openrouter.md) — OpenRouter 调研
- [`provider-qwen.md`](research/providers/provider-qwen.md) — Qwen 调研
- [`provider-stepfun.md`](research/providers/provider-stepfun.md) — StepFun 调研
- [`provider-zhipu.md`](research/providers/provider-zhipu.md) — 智谱 调研
- **`official-snapshots/`** — 各 Provider 官方 API 响应快照（20+ 篇）

### 4. 竞品与三方项目分析 (`research/competitors/`)
- [`provider-switch-tools-comparison.md`](research/competitors/provider-switch-tools-comparison.md) — 竞品对比总结
- [`cc-switch.md`](research/competitors/cc-switch.md) & [`cliproxyapi.md`](research/competitors/cliproxyapi.md) — 竞品实现细节拆解
- [`llm-proxy-implementation-comparison.md`](research/competitors/llm-proxy-implementation-comparison.md) — llm-proxy 实现对比分析

### 5. 调研规划
- [`research-targets.md`](research/research-targets.md) — 调研目标与优先级
- [`quota-api-research.md`](research/quota-api-research.md) — 额度查询 API 调研

---

## 📚 六、 标准操作规程 (SOPs — `docs/sops/`)

开发流程、回归验证与文档维护的标准化指南：

- [`launch-and-access-verification.md`](sops/launch-and-access-verification.md) — **E2E 接入验证与回归矩阵 SOP**（功能发布前的必跑流程）
- [`status-verification.md`](sops/status-verification.md) — **Status 命令完整验证 SOP**（16 种场景矩阵、模拟方法、验证步骤）
- [`review-research-doc.md`](sops/review-research-doc.md) — 调研文档评审与验收 SOP
- [`external-research-refresh.md`](sops/external-research-refresh.md) — 外部 Provider 接口与模型目录刷新 SOP

---

## 📖 七、 用户指南 (`docs/user_guide/`)

面向使用者的操作指南和最佳实践：

- [`cli-guide.md`](user_guide/cli-guide.md) — **CLI 命令完整使用指南**（Provider/Model 管理、Fallback、Quota、状态查看、常见操作示例）
- [`tui-guide.md`](user_guide/tui-guide.md) — **TUI 交互界面使用指南**（Provider 管理面板、connect 流程、各界面快捷键）
- [`user-guide-oauth-accounts.md`](user_guide/user-guide-oauth-accounts.md) — OAuth 账户管理与认证流程指南

---

## 📊 八、 体验报告 (`docs/reports/`)

用户体验测试报告与评估（按日期归档）：

- [`2026-08-06-ux-report.md`](reports/2026-08-06-ux-report.md) — 首次用户体验测试（功能完整性、手册完善度、改进建议）
