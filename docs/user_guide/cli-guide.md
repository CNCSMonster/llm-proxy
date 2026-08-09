# CLI 使用指南

本文档是 `llm-proxy` CLI 命令的完整使用指南，涵盖常见操作流程和最佳实践。

---

## 📋 目录

1. [快速开始](#快速开始)
2. [Provider 管理](#provider-管理)
3. [Model 管理](#model-管理)
4. [状态查看](#状态查看)
5. [Launch 命令](#launch-命令)
6. [Quota 命令](#quota-命令)
7. [Connect 命令](#connect-命令)
8. [Completion 命令](#completion-命令)
9. [常见操作示例](#常见操作示例)
   - 示例 6：[自定义 Provider/Model 处理边缘场景](#示例-6自定义-providermodel-处理边缘场景)
10. [协议转换说明](#协议转换说明)
11. [最佳实践](#最佳实践)
12. [相关文档](#相关文档)

---

## 快速开始

### 1. 导出 API Key

```bash
export DEEPSEEK_API_KEY="sk-xxx"
export OPENAI_API_KEY="sk-xxx"
export ANTHROPIC_API_KEY="sk-ant-xxx"
```

### 2. 添加 Provider + 模型

**方式 1：一步完成（推荐）**

使用 `provider add` 命令，TUI 交互选择产品与模型模板：

```bash
llm-proxy provider add
# TUI 交互：选择产品 → 选择模型模板（deepseek-v4-flash-lp、deepseek-v4-pro-lp 等）→ 确认 API Key

# 或指定产品 + 模型（非交互，一步完成）
llm-proxy provider add deepseek --model deepseek-v4-pro-lp
```

**方式 2：两步操作（自定义模型）**

先添加 provider（TUI 中不勾选任何模型即可跳过模型绑定），再添加模型并绑定：

```bash
# 1. 添加 provider（TUI 交互，模型选择界面零选择直接 Enter = 跳过）
llm-proxy provider add

# 2. 添加自定义模型
llm-proxy model add my-custom-model --context-window 100000 --max-output 4096

# 3. 绑定 provider（首次 binding 必须指定 --upstream-model）
llm-proxy model provider add my-custom-model --type openai-chat --provider deepseek --upstream-model deepseek-v4-pro
```

### 3. 启动代理服务

```bash
llm-proxy   # 默认监听 http://127.0.0.1:8989
```

### 4. 为 Coding Agent 生成配置

```bash
llm-proxy launch codex     # 为 Codex CLI 生成接入配置
llm-proxy launch pi        # 为 pi 生成接入配置
llm-proxy launch qwen-code # 为 Qwen Code 生成接入配置
```

---

## Provider 管理

### 添加 Provider

```bash
# TUI 交互模式（推荐）：选择产品与模型模板
llm-proxy provider add

# 非交互：指定产品 + 模型（一步完成）
llm-proxy provider add deepseek --model deepseek-v4-pro-lp
```

**TUI 流程**：
1. 选择产品（如 DeepSeek、OpenAI Subscription）
2. 选择模型模板（从 catalog 中选择，零选择按 Enter 可跳过）
3. 确认 API Key 环境变量
4. 完成添加

> 注意：`provider add <产品>` 必须带 `--model`（成熟 catalog 产品）；只想添加 provider 不绑定模型时，用 TUI 模式并在模型选择界面直接按 Enter 跳过。

### 列出 Provider

```bash
llm-proxy provider list
```

### 查看 Provider 详情

```bash
llm-proxy provider info <PROVIDER_ID>
```

### 删除 Provider

```bash
llm-proxy provider remove <PROVIDER_ID>
```

### 复制 Provider

```bash
llm-proxy provider copy <SOURCE> <TARGET> --api-key-env <ENV_VAR>
```

### 重置 Usage 配额

```bash
llm-proxy provider reset-usage <PROVIDER_ID>

# 跳过确认提示
llm-proxy provider reset-usage <PROVIDER_ID> --force
```

适用于 OpenRouter 等按周期重置额度的 provider（如 OpenAI Subscription 的 5h 配额）。TUI 中对应 Provider 管理面板的 `[R]` 键。

### 配置批量 Fallback

把某个 provider 批量插入为另一 provider 的 fallback，一次配置多个 `模型:endpoint` 组合：

```bash
llm-proxy provider fallback add --provider <FALLBACK_PROVIDER> --target <TARGET_PROVIDER> --bindings <MODEL:ENDPOINT>...

# 示例：把 deepseek-2 作为 deepseek 的 fallback（chat 协议）
llm-proxy provider fallback add --provider deepseek-2 --target deepseek --bindings deepseek-v4-pro-lp:chat

# bindings 支持正则
llm-proxy provider fallback add --provider deepseek-2 --target deepseek --bindings "deepseek-v4-.*:.*"
```

**`ENDPOINT` 取值**：`chat`（Chat Completions）、`responses`（Responses）、`anthropic`（Anthropic Messages）。

**约束**：
- `--target` 与 `--provider` 必须属于**同一产品**（upstream model 自动从 target 的 binding 复制，无需手动指定）
- 插入位置：target 之后
- 正则匹配所有协议的 binding 时，fallback provider 必须声明对应的协议端点，否则整体失败
- 跨产品 / 特殊上游映射 → 用 `model provider add` 精细配置

### OAuth 登录 / 登出 / 刷新

```bash
# 登录（OAuth 设备码流程，交互式 TUI）
llm-proxy provider login <PROVIDER_ID>

# 登出（清除本地 token）
llm-proxy provider logout <PROVIDER_ID>

# 重新登录
llm-proxy provider relogin <PROVIDER_ID>

# 手动刷新 access token（TUI 中对应管理面板的 [r] 键，会自动刷新过期 token）
llm-proxy provider refresh <PROVIDER_ID>
```

> 注意：`provider login` 会启动交互式 TUI（OAuth 设备码流程）；非 OAuth provider（API Key / None 认证）执行 login/logout/refresh 会被拒绝。

OAuth 账户管理详见 [OAuth 账户管理](user-guide-oauth-accounts.md)。

---

## Model 管理

### 添加模型

```bash
# 创建空模型
llm-proxy model add <MODEL_ID> --context-window <SIZE> --max-output <SIZE>

# 从已有模型复制参数创建
llm-proxy model add <MODEL_ID> --copy-from <SOURCE_MODEL>

# 从产品发现创建（需指定 upstream model）
llm-proxy model add <MODEL_ID> --from-discovery <PRODUCT> --upstream-model <UPSTREAM_MODEL>

# 示例
llm-proxy model add my-model --context-window 100000 --max-output 4096
```

### 绑定 Provider 到模型

```bash
llm-proxy model provider add <MODEL_ID> --type <PROTOCOL> --provider <PROVIDER_ID> --upstream-model <UPSTREAM_MODEL>

# 示例（首次 binding 必须指定 --upstream-model；后续 binding 可省略，自动复制第一个）
llm-proxy model provider add my-model --type openai-chat --provider deepseek --upstream-model deepseek-v4-pro
```

**支持的协议类型**：
- `openai-chat` — OpenAI Chat Completions API
- `openai-responses` — OpenAI Responses API
- `anthropic` — Anthropic Messages API

### 解绑 Provider

```bash
llm-proxy model provider remove <MODEL_ID> --type <PROTOCOL> --provider <PROVIDER_ID>
```

### 调整 binding 顺序（fallback 优先级）

`model provider add` 按添加顺序排列 binding，先添加的优先级高；`--to` 把 binding 移动到指定位置（**1-based，1 为最前**）：

```bash
# 把 deepseek 移到第一个（最高优先级）
llm-proxy model provider move <MODEL_ID> --type openai-chat --provider deepseek --to 1
```

### 列出模型

```bash
llm-proxy model list
```

### 查看模型详情

```bash
llm-proxy model info <MODEL_ID>
```

### 修改模型参数

```bash
llm-proxy model set <MODEL_ID> --context-window <SIZE> --max-output <SIZE>
```

### 删除模型

```bash
llm-proxy model remove <MODEL_ID>
```

---

## 状态查看

### 查看整体状态

```bash
# 显示缓存数据（不发额外请求）
llm-proxy status

# 触发实际探测（发请求到上游）
llm-proxy status --probe
```

**数据来源提示**：
- `ℹ` — 实时数据（来自 Server）
- `✓` — 探测成功
- `⚠` — 缓存数据（可能过时）
- `✗` — 无数据或错误

### 查看使用量统计

```bash
# TUI 模式
llm-proxy usage

# CLI 模式
llm-proxy usage --period 7d
llm-proxy usage --provider deepseek
llm-proxy usage --model deepseek-v4-pro-lp
llm-proxy usage --endpoint anthropic
llm-proxy usage --view by-provider
llm-proxy usage --json
```

**参数**：
- `--period` — 时间范围（`today`、`7d`、`2026-03-12:2026-03-20`）
- `--provider` / `--model` / `--endpoint` — 按维度过滤
- `--view` — 聚合视图：`by-model`（默认）、`by-provider`、`by-endpoint`、`by-hour`、`by-day`
- `--json` — JSON 输出

> **说明**：usage 统计来自本地 SQLite 持久化（Token 用量记录），TUI 中对应 Provider 管理面板的 `[u]` 键可查看单个 provider 的用量摘要。

### 查看冷却状态

```bash
llm-proxy cooldown list
llm-proxy cooldown clear --provider <PROVIDER_ID>
```

---

## Launch 命令

为 Coding Agent 生成接入配置：

```bash
llm-proxy launch codex         # Codex CLI
llm-proxy launch codex-desktop # Codex 桌面客户端
llm-proxy launch pi            # pi CLI
llm-proxy launch qwen-code     # Qwen Code
llm-proxy launch claude-code   # Claude Code
llm-proxy launch claude-desktop # Claude 桌面客户端
```

**选项**：
- `--dry-run` — 只打印配置，不写入文件
- `[MODEL_ID]`（仅 `qwen-code` / `claude-code`）— 指定默认模型（位置参数）
- `--profile <PROFILE>`（仅 `claude-desktop`）— 配置文件名（默认 `llm-proxy`）

---

## Quota 命令

查看订阅类 provider（如 OpenAI Subscription）的配额使用情况：

```bash
# 查看所有 OAuth provider 的配额
llm-proxy quota

# 强制刷新（绕过缓存）
llm-proxy quota --refresh
```

**输出信息**：
- `Plan` — 订阅计划类型（如 pro、plus）
- `Usage` — 已使用百分比
- `Window` — 限速窗口时长
- `Reset` — 配额重置时间（UTC）

> **提示**：需要手动重置配额时（如 OpenAI Subscription 的 5h 窗口），见 [重置 Usage 配额](#重置-usage-配额)。

---

## Connect 命令

`connect` 是 `provider add` 的 TUI 交互方式，通过交互式界面添加 provider：

```bash
# 启动 TUI 交互添加 provider
llm-proxy connect

# 指定产品 + 模型（非交互，一步完成）
llm-proxy connect deepseek --model deepseek-v4-pro-lp
```

> 注意：带产品名的 `connect <产品>` 必须同时指定 `--model`，且此时是非交互直接添加；完整的 TUI 交互流程（产品选择、命名屏、fallback 配置）只在使用无参数 `connect` 时出现。

**TUI 流程**：

```
产品选择 → 命名屏（仅第 2+ 次 connect）→ env 选择 → 模型选择 / fallback 配置 → 完成
```

1. 选择产品（如 DeepSeek、OpenAI Subscription、Anthropic 等）
2. **命名屏**：第 2+ 次 connect 同一产品时出现，为 provider 指定名称（预填 `产品名-2` 等递进 ID，实时校验重名）；首次 connect 自动跳过
3. 确认 API Key 环境变量（带 `you may want` 推荐标记，可按 `s` 跳过）
4. 模型选择（勾选模型模板）或 **fallback 配置**（把新 provider 配置为已有 provider 的 fallback）：
   - 首次 connect 默认进模型选择
   - 再次 connect 默认进 fallback 配置界面
5. 完成添加

> **提示**：`connect` 和 `provider add` 功能等价，`connect` 提供更友好的 TUI 交互体验。各界面的完整快捷键见 [TUI 使用指南](tui-guide.md)。

---

## Completion 命令

生成 shell 自动补全脚本，让 `llm-proxy` 命令支持 Tab 补全：

```bash
# Bash
llm-proxy completion bash >> ~/.bashrc

# Zsh
llm-proxy completion zsh > "${fpath[1]}/_llm-proxy"

# Fish
llm-proxy completion fish > ~/.config/fish/completions/llm-proxy.fish

# PowerShell
llm-proxy completion power-shell >> $PROFILE

# Elvish
llm-proxy completion elvish >> ~/.elvish/rc.elv
```

**支持的 shell**：`bash`、`zsh`、`fish`、`power-shell`、`elvish`

> **提示**：补全脚本会自动补全子命令名、`--select` 参数的 provider 名、`--model` 参数的模型名等。

---

## 常见操作示例

### 示例 1：添加 OpenAI Subscription + 所有模型

```bash
# TUI 交互：选择 openai-sub 产品，勾选所有模型模板（gpt-5.5-sub-lp、gpt-5.4-sub-lp 等）
llm-proxy provider add

# 或非交互一步完成
llm-proxy provider add openai-sub --model gpt-5.5-sub-lp
```

### 示例 2：添加自定义模型（多 Provider 绑定）

```bash
# 1. 添加模型
llm-proxy model add my-model --context-window 100000 --max-output 4096

# 2. 绑定多个 provider（支持 fallback；首个 binding 指定 --upstream-model）
llm-proxy model provider add my-model --type openai-chat --provider deepseek --upstream-model deepseek-v4-pro
llm-proxy model provider add my-model --type anthropic --provider anthropic

# 3. 调整 binding 顺序（优先级）：把 anthropic 设为最高优先级
llm-proxy model provider move my-model --type anthropic --provider anthropic --to 1
```

> **批量 fallback**：已有 provider 想整体作为另一个 provider 的备用，用 `provider fallback add` 一次配置多个 `模型:endpoint` 组合（见 [配置批量 Fallback](#配置批量-fallback)）。

### 示例 3：按产品族批量添加模型

```bash
# TUI 交互：选择 openai-sub 产品，勾选所有模型模板
llm-proxy provider add
# 选择：gpt-5.5-sub-lp、gpt-5.4-sub-lp、gpt-5.4-mini-sub-lp
```

### 示例 4：查看状态并触发探测

```bash
# 先查看缓存状态
llm-proxy status

# 触发实际探测（发请求到上游）
llm-proxy status --probe
```

### 示例 5：容器场景（远程模式）

在容器内使用 llm-proxy（server 在宿主机）：

```bash
# 容器内配置极简 config.toml
cat > ~/.config/llm-proxy/config.toml <<EOF
[server]
listen = "host.docker.internal:8989"
EOF

# 从 server 获取状态
llm-proxy status

# 为 Codex 生成配置（使用 host.docker.internal）
llm-proxy launch codex
```

### 示例 6：自定义 Provider/Model 处理边缘场景

当同一 provider 的不同模型需要不同端点配置时（如 DeepSeek v4-flash 支持原生 Responses，v4-pro 暂不支持），可以通过自定义配置处理：

**场景**：DeepSeek v4-flash 支持原生 Responses 端点，但 v4-pro 还不支持。你希望 v4-flash 使用原生端点以获得更好的性能。

**解决方案**：创建自定义 provider 和模型

```bash
# 1. 编辑配置文件，添加自定义 provider
cat >> ~/.config/llm-proxy/config.toml <<'EOF'

# 自定义 provider：使用原生 Responses 端点
[providers.deepseek-native]
api_key_env = "DEEPSEEK_API_KEY"

[providers.deepseek-native.openai_responses]
url = "https://api.deepseek.com/v1/responses"
EOF

# 2. 添加自定义模型，绑定到自定义 provider
cat >> ~/.config/llm-proxy/config.toml <<'EOF'

# 自定义模型：使用原生 Responses 端点
[models.ds-flash-resp-lp]
description = "DeepSeek V4 Flash (Native Responses)"
context_window = 1000000
max_output_tokens = 393216
openai_responses_providers = [
    { name = "deepseek-native", model = "deepseek-v4-flash" }
]
EOF

# 3. 重启代理使配置生效
llm-proxy shutdown && llm-proxy

# 4. 验证新模型可用
llm-proxy model info ds-flash-resp-lp
```

**说明**：
- 这种方式适用于临时场景或边缘用例
- 不需要修改核心代码或架构
- 当上游支持后（如 v4-pro 支持原生 Responses），可以删除自定义配置，使用标准配置
- 更多架构边界说明请参考 [`AGENTS.md`](../../AGENTS.md) 的"架构边界"章节

---

## 协议转换说明

llm-proxy 支持三种 LLM API 协议，并在它们之间自动转换。理解协议有助于正确配置和使用。

### 三种协议

| 协议标识 | 全称 | 端点路径 | 典型客户端 |
|---------|------|---------|-----------|
| `chat` | OpenAI Chat Completions | `/v1/chat/completions` | Codex CLI、Qwen Code、通用 OpenAI 客户端 |
| `responses` | OpenAI Responses | `/v1/responses` | Codex CLI（新版）、支持 Responses API 的客户端 |
| `anthropic` | Anthropic Messages | `/v1/messages` | Claude Code、Claude Desktop、Anthropic SDK |

### 协议转换原理

llm-proxy 作为中间层，接收客户端请求后：

1. **识别客户端协议**：根据请求路径判断客户端使用的协议（如 `/v1/chat/completions` → chat 协议）
2. **查找目标 provider**：根据模型配置找到对应的 provider 和支持的协议
3. **格式转换**：将请求从客户端协议转换为 provider 原生协议
4. **转发并转换响应**：将 provider 的响应转换回客户端期望的协议格式

**示例**：Claude Code（anthropic 协议）→ llm-proxy → DeepSeek（chat 协议）
- Claude Code 发送 Anthropic Messages 格式的请求
- llm-proxy 将其转换为 OpenAI Chat Completions 格式
- 转发到 DeepSeek API
- 收到响应后转换回 Anthropic Messages 格式返回给 Claude Code

### 模型绑定与协议

每个模型可以绑定多个 provider，每个绑定指定使用的协议：

```bash
# 绑定 deepseek 使用 chat 协议（首个 binding 必须指定 --upstream-model）
llm-proxy model provider add my-model --type openai-chat --provider deepseek --upstream-model deepseek-v4-pro

# 绑定 anthropic 使用 anthropic 协议（后续 binding 自动复制 upstream，可省略）
llm-proxy model provider add my-model --type anthropic --provider anthropic
```

`model list` 输出中的 `protocols` 字段显示该模型支持的协议（route key）：

```
my-model context=100000 max_output=4096 protocols=chat_completions,responses,anthropic
```

### 使用示例

**场景 1：用 Claude Code 调用 DeepSeek**

```bash
# 1. 添加 deepseek provider（自动使用 chat 协议）
llm-proxy provider add deepseek --model deepseek-v4-pro-lp

# 2. 启动代理
llm-proxy

# 3. 为 Claude Code 生成配置
llm-proxy launch claude-code

# 4. 使用 Claude Code（自动通过 anthropic 协议访问，llm-proxy 转换为 chat 协议转发给 deepseek）
```

**场景 2：用 Codex CLI 调用 Anthropic**

```bash
# 1. 添加 anthropic provider（自动使用 anthropic 协议）
llm-proxy provider add anthropic --model claude-sonnet-lp

# 2. 启动代理
llm-proxy

# 3. 为 Codex 生成配置
llm-proxy launch codex

# 4. 使用 Codex（自动通过 responses 协议访问，llm-proxy 转换为 anthropic 协议转发）
```

---

## 最佳实践

### 1. 优先使用 `provider add`（一步完成）

对于 catalog 中的标准产品，使用 `provider add` 一步完成 provider + 模型添加，避免两步操作。

### 2. 使用 `status` 前先理解数据来源

- `status`（无 `--probe`）：不发额外请求，显示缓存数据
- `status --probe`：发请求到上游，触发实际探测

### 3. 远程模式使用 `host.docker.internal`

容器内无法访问 `localhost`，需要用 `host.docker.internal` 指向宿主机。

### 4. 定期清理冷却状态

如果 provider 被冷却（cooldown），可以手动清理：

```bash
llm-proxy cooldown clear --provider <PROVIDER_ID>
```

---

## 相关文档

- [TUI 使用指南](tui-guide.md) — 交互式界面的快捷键与操作说明
- [OAuth 账户管理](user-guide-oauth-accounts.md) — OAuth 认证流程
- [E2E 验证 SOP](../sops/launch-and-access-verification.md) — 功能发布前的验证流程
- [项目 README](../../README.md) — 项目入口和 Quickstart
