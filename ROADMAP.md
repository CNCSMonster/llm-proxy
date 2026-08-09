# Roadmap

llm-proxy 是一个**本地 LLM 协议聚合代理**，将多个上游 LLM 服务聚合到统一的本地端点，为 coding agent 提供零泄露的配置注入和协议透明转发。

## 核心价值

- **聚合多源**：将 DeepSeek、OpenAI、Anthropic、Google Antigravity、Kimi、MiMo、Qwen 等多个平台聚合到 `http://127.0.0.1:8989`
- **零泄露**：agent 配置只含代理地址和 dummy key，真实 API Key 仅保留在 llm-proxy
- **协议透明**：支持 OpenAI Chat、OpenAI Responses、Anthropic Messages 三种协议互转
- **隔离转发**：为容器、WSL2、远程环境提供安全的转发服务

## 支持的协议

| 协议 | 端点 |
|------|------|
| OpenAI Chat Completions | `/openai/v1/chat/completions` |
| OpenAI Responses | `/openai/v1/responses`、`/responses/v1/responses` |
| Anthropic Messages | `/anthropic/v1/messages` |

## 支持的 Provider

| 平台 | 产品 | 协议 |
|------|------|------|
| DeepSeek | 按量付费 | Chat, Responses, Anthropic |
| OpenAI | 按量付费 / ChatGPT 订阅 | Chat, Responses, Anthropic |
| Anthropic | 按量付费 | Anthropic |
| Google Antigravity | OAuth | Chat, Responses, Anthropic |
| OpenRouter | 按量付费 | Chat, Responses, Anthropic |
| Kimi | 平台 / 订阅 | Chat, Responses, Anthropic |
| MiMo (小米) | 按量付费 / Token Plan (CN/SGP/AMS) | Chat, Responses, Anthropic |
| 智谱 | 按量付费 / Coding Plan | Chat, Responses, Anthropic |
| 百炼 (阿里) | 按量付费 / Coding Plan | Chat, Responses, Anthropic |
| StepFun | 按量付费 / Step Plan | Chat, Responses, Anthropic |
| Ollama | 本地 | Chat |

## 里程碑

### M1 — 核心功能完成（当前）

**客户端支持**

| Agent | 命令 | 状态 |
|-------|------|------|
| Codex CLI | `launch codex` | ✅ 已完成（完整 E2E 验证） |
| Codex Desktop | `launch codex-desktop` | ✅ 已完成 |
| pi | `launch pi` | 🚧 实现中（待完整 E2E） |
| Qwen Code | `launch qwen-code` | 🚧 实现中（待完整 E2E） |
| Claude Code | `launch claude-code` | ⚠️ 已弃用（Claude 产品线封闭化） |
| Claude Desktop | `launch claude-desktop` | ⚠️ 已弃用（Claude 产品线封闭化） |

**CLI 命令**

| 命令 | 功能 |
|------|------|
| `init` | 初始化配置文件 |
| `serve` | 启动代理服务 |
| `shutdown` / `restart` | 关闭/重启服务 |
| `status` | 查看配置健康状态和 provider 可达性 |
| `connect` / `provider add` | 添加 provider（交互式或 CLI） |
| `provider list` / `info` | 列出/查看 provider 详情 |
| `provider copy` / `remove` | 复制/删除 provider |
| `provider login` / `logout` / `relogin` / `refresh` | OAuth 登录管理 |
| `model list` / `info` | 列出/查看模型详情 |
| `model add` / `set` / `remove` | 添加/修改/删除模型 |
| `model provider add` / `remove` / `move` | 管理模型的 provider 绑定（fallback 链） |
| `cooldown list` / `clear` | 查看/清除 provider 冷却状态 |
| `usage` | Token 用量统计 |
| `quota` | 订阅额度查询（OAuth provider） |
| `completion` | Shell 补全脚本生成 |

**核心功能**

- ✅ 三协议互转（OpenAI Chat ↔ Responses ↔ Anthropic）
- ✅ 同 model 多 provider fallback（按优先级尝试）
- ✅ Provider 熔断冷却机制
- ✅ OAuth 设备码登录（OpenAI、Google Antigravity）
- ✅ Token 自动刷新
- ✅ TUI 配置界面（`provider` 无参数进入）
- ✅ Server Delegation 架构（UDS 写 / HTTP 读）
- ✅ 多模态输入转发（图片、PDF/文档）
- ✅ Thinking 控制与级别映射
- ✅ 流式响应聚合与转换
- ✅ 请求拦截保护（防止无效请求浪费上游配额）

**内置模型目录**

预配置 30+ 模型，覆盖主流 coding 场景：
- DeepSeek V4 系列
- GPT-5.x 系列（订阅）
- Claude 4.x 系列（Antigravity）
- Gemini 3.x 系列（Antigravity）
- Kimi K2.7 / K3 系列
- MiMo V2.5 系列
- Qwen 3.7 系列

### M2 — 多客户端完善与 CLI 自解释性

**目标**：完成 pi、qwen-code 的完整 E2E 验证，新增 OpenCode 支持，恢复 CLI 自解释能力。

**客户端支持**

- [ ] pi 完整 E2E 验证（单轮、多轮工具调用、流式）
- [ ] qwen-code 完整 E2E 验证
- [ ] OpenCode 支持（`launch opencode`）
- [ ] 更多客户端适配（按需）

**CLI 自解释性（`doc` 命令）**

把用户手册嵌入二进制，让用户无需查阅外部文档即可获取使用帮助：

- [ ] 扩展 `src/doc.rs`：嵌入 `docs/user_guide/` 下所有用户手册
- [ ] `llm-proxy doc --list`：列出所有可用章节
- [ ] `llm-proxy doc [section]`：打印指定章节内容
- [ ] `llm-proxy doc`：打印默认章节（快速开始）
- [ ] 用户手册内容补全（配置参考、协议转换、FAQ 等）

**`status` 命令增强**

支持范围过滤，快速定位特定 provider/model 的状态：

- [ ] `--provider <pattern>`：按正则过滤 provider（如 `--provider "deepseek.*"`）
- [ ] `--model <pattern>`：按正则过滤 model（如 `--model "gpt-5|claude-4"`）
- [ ] 组合使用：`--provider openai --probe` 只探活 openai 相关

### M3 — 鉴权与计量

**目标**：支持多租户场景，实现 token 鉴权和用量计量。

- [ ] Token 鉴权模式（启用后请求需携带 token）
- [ ] 按 token 统计用量（模型/provider 维度）
- [ ] 额度限制（每 token 可设置使用上限）
- [ ] Token 派发与管理命令

## 未来方向

- 更多 Provider 支持（按需扩展）
- 更好的错误提示与诊断
- 配置热重载（评估中）
- 远程管理接口（评估中）

## 不做的事

- ❌ 会话管理（交给客户端自身处理）
- ❌ 图片生成工作流
- ❌ 内部 web_search / 搜索工具协议适配
- ❌ 所有 provider 私有参数的通用适配
- ❌ 通用全能力 LLM 网关（专注 coding agent 场景）
