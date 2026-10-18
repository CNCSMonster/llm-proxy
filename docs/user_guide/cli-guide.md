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
7. [Token 命令](#token-命令)
8. [Connect 命令](#connect-命令)
9. [Completion 命令](#completion-命令)
10. [路由模型](#路由模型)
11. [常见操作示例](#常见操作示例)
   - 示例 6：[自定义 Provider/Model 处理边缘场景](#示例-6自定义-providermodel-处理边缘场景)
12. [协议转换说明](#协议转换说明)
13. [最佳实践](#最佳实践)
14. [Changelog](#changelog)
15. [相关文档](#相关文档)

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

### 跨 Product 绑定

同一 Model 的 bindings 允许跨 Product 混绑——不同计费通道的 provider 可以配到同一个 model。

```bash
# 例：同一 model 绑定 OpenRouter 和 DeepSeek 两个不同 Product 的 provider
llm-proxy model provider add gemini-model --type openai-chat --provider openrouter --upstream-model gemini-3.8-flash-low
llm-proxy model provider add gemini-model --type openai-chat --provider deepseek --upstream-model deepseek-v4-flash
```

**注意事项**：

- **跨 Product 添加 binding 时必须显式指定 `--upstream-model`**——不同 Product 的 provider 对同一模型使用不同的 upstream model 名，不能依赖默认复制
- Product-specific 字段（如上游特有的扩展字段）在跨 Product 路由时会被自动剥离，不会发送给不消费该字段的端点
- RoundRobin 会跨 Product 轮转——确保所有 binding 都能服务该模型
- 如果不确定目标 provider 是否支持该模型，先用 `status --probe` 验证

### 3. 启动代理服务

```bash
llm-proxy   # 默认监听 http://127.0.0.1:8989
```

### 4. 为 Coding Agent 生成配置

```bash
llm-proxy launch codex     # 为 Codex CLI 生成接入配置
llm-proxy launch pi        # 为 pi 生成接入配置
llm-proxy launch qwen-code # 为 Qwen Code 生成接入配置
llm-proxy launch opencode  # 为 OpenCode 生成接入配置
```

---

## Bare Provider/Model 命令的终端要求

不带子命令的以下两个管理入口会尝试启动 TUI：

```bash
llm-proxy provider
llm-proxy model
```

只有 **stdin 和 stdout 都是 TTY** 时，程序才会继续进入既有 TUI 路径。stdin 或 stdout 任一不是 TTY（例如管道、重定向、后台脚本或 CI）时，命令会：

- 不初始化 TUI；
- 以退出码 `2` 结束；
- 输出不含 ANSI 转义序列的 usage error；
- 指向显式子命令和对应的 `llm-proxy provider --help` 或 `llm-proxy model --help`。

该门禁适用于 bare `provider` / `model`，以及交互详情入口 `provider --select <NAME>` 与 `model --select <MODEL>`。`provider list`、`provider info`、`model list`、`model info` 等显式只读子命令不受影响。

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
# 交互终端：直接打开指定 Provider 的 TUI 详情
llm-proxy provider --select <PROVIDER_ID>

# 非交互 / 脚本：纯文本详情
llm-proxy provider info <PROVIDER_ID>
```

`--select` 只在 stdin/stdout 都是 TTY 时进入 TUI；未知 Provider 立即失败。从详情 Esc 返回列表时，光标停在该 Provider 上。

### 删除 Provider

```bash
llm-proxy provider remove <PROVIDER_ID>
```

### 复制 Provider

```bash
llm-proxy provider copy <SOURCE> <TARGET> --api-key-env <ENV_VAR>
```

TUI 管理面板按 `c` 进入 CopyConfigure：预填递进名称并实时校验重名；API Key provider 必须显式确认环境变量名（或按 `s` 复制为无密钥），OAuth 复制后进入登录。模型绑定不会被复制。

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

# 手动刷新 OAuth access token（TUI 中对应管理面板的 [r] 键，会自动刷新过期 token）
llm-proxy provider refresh <PROVIDER_ID>
```

> 注意：`provider login` 会启动交互式 TUI（OAuth 设备码流程）；非 OAuth provider（API Key / None 认证）执行 login/logout/refresh 会被拒绝。
>
> `provider refresh` 只更新 OAuth 凭据，不执行上游可用性探测，也不清除 cooldown、active evidence 或探测缓存。检查上游可用性请使用 `status --probe`；清除冷却请使用 `cooldown clear`。

OAuth 账户管理详见 [OAuth 账户管理](user-guide-oauth-accounts.md)。

---

## Model 管理

交互终端下，无子命令的 `llm-proxy model` 打开独立 Model 管理 TUI（列表、详情、新增、复制、重命名、删除、协议绑定编辑），不再进入 connect 向导。`model --select <MODEL>` 直接打开目标模型详情。未知模型立即失败。完整快捷键见 [TUI 使用指南](tui-guide.md)。

### 添加模型

```bash
# 创建空模型
llm-proxy model add <MODEL_ID> --context-window <SIZE> --max-output <SIZE>

# 从已有模型复制参数创建（克隆窗口与全部绑定；禁止与 --context-window/--max-output/--upstream-model 混用，复制后可用 model set 调整参数）
llm-proxy model add <MODEL_ID> --copy-from <SOURCE_MODEL>

# 从产品发现创建（需指定 upstream model）
llm-proxy model add <MODEL_ID> --from-discovery <PRODUCT> --upstream-model <UPSTREAM_MODEL>

# 示例
llm-proxy model add my-model --context-window 100000 --max-output 4096
```

### 绑定 Provider 到模型

```bash
llm-proxy model provider add <MODEL_ID> --type <PROTOCOL> --provider <PROVIDER_ID> --upstream-model <UPSTREAM_MODEL>

# 示例（首次 binding 必须指定 --upstream-model；后续同 Product binding 可省略，自动复制第一个；命令成功后回显最终生效的 upstream）
llm-proxy model provider add my-model --type openai-chat --provider deepseek --upstream-model deepseek-v4-pro
```

> **跨 Product 添加 binding**：后续 binding 的 provider 与已有 binding 属于不同 Product 时，`--upstream-model` **不可省略**（不同 Product 的 upstream model 名不通用，静默复制会生成必然失败的绑定）。同 Product 内省略仍自动复制首 binding 的模型名。

**支持的协议类型**：
- `openai-chat` — OpenAI Chat Completions API
- `openai-responses` — OpenAI Responses API
- `anthropic` — Anthropic Messages API

### 修改 Provider 的上游模型（原地修改，保持 fallback 优先级）

`model provider set` 原地更新指定 binding 的上游模型名称，**不改变 provider 在 fallback 链中的相对顺序与优先级**（避免通过 remove+add 导致该 provider 被静默移到 fallback 列表末位）：

```bash
llm-proxy model provider set <MODEL_ID> --type <PROTOCOL> --provider <PROVIDER_ID> --upstream-model <NEW_UPSTREAM_MODEL>

# 示例
llm-proxy model provider set my-model --type openai-chat --provider deepseek --upstream-model deepseek-v4-flash
```

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

输出每行一个模型：`<模型ID> context=<上下文> max_output=<最大输出> protocols=<协议列表>`。
声明了思考等级的模型行尾追加 **`[thinking: client-driven]`**——表示思考档位由客户端选择并声明给代理；接入后（如 `launch codex`）客户端会出现对应的思考控制项。未声明思考等级的模型没有此标记。

### 查看模型详情

```bash
# 交互终端：直接打开指定 Model 的 TUI 详情
llm-proxy model --select <MODEL_ID>

# 非交互 / 脚本：纯文本详情（展示各协议 Provider Binding 及其冷却状态）
llm-proxy model info <MODEL_ID>
```

输出示例：
```text
model=qwen3.8-flash-256k-lp
context_window=262144
max_output_tokens=8192
openai_chat:
  1. provider=ubt-bailian upstream_model=qwen3.8-flash-256k [COOLDOWN until 2026-10-01 12:00]
  2. provider=bailian-token-plan-team upstream_model=qwen3.8-flash-256k [OK]
```
终端环境下 `[COOLDOWN until ...]` 显示为红色，`[OK]` 显示为绿色；管道或非 TTY 环境下自动降级为纯文本。

`--select` 只在 stdin/stdout 都是 TTY 时进入 TUI；未知 Model 立即失败。从详情 Esc 返回列表时，光标停在该 Model 上。

### 重命名模型

```bash
llm-proxy model rename <OLD_MODEL_ID> <NEW_MODEL_ID>
```

重命名会迁移 `[models.<OLD>]` 到 `[models.<NEW>]`，并同步更新仍引用旧 ID 的 binding。旧 ID 不存在或新 ID 已占用时立即失败。server 运行中时通过 admin API 写入。

### 修改模型参数

```bash
# 修改上下文与输出窗口
llm-proxy model set <MODEL_ID> --context-window <SIZE> --max-output <SIZE>

# 修改模型描述
llm-proxy model set <MODEL_ID> --description "模型描述"
llm-proxy model set <MODEL_ID> --clear-description

# 修改推理关闭方言 (当前支持 enable-thinking)
llm-proxy model set <MODEL_ID> --chat-reasoning-off-format enable-thinking
llm-proxy model set <MODEL_ID> --clear-chat-reasoning-off-format

# 修改 Codex 服务层级配置
llm-proxy model set <MODEL_ID> --service-tier priority [--default-tier | --forced-tier]
llm-proxy model set <MODEL_ID> --clear-service-tier

# 修改模型级错误冷却 override（秒）
llm-proxy model set <MODEL_ID> --cooldown-network 30 --cooldown-server-error 300
llm-proxy model set <MODEL_ID> --clear-cooldown
```

### 路由模型

路由模型是一个纯转发层：客户端向它发请求，它按 `reasoning_effort`（low / medium / high）派发到不同的上游目标模型。客户端只需知道一个统一的 model ID，不需要关心思考档位对应哪个上游变体。

#### 配置方式

**CLI 创建（推荐）**：

```bash
# 先添加目标模型（普通模型，各有 provider bindings）
llm-proxy model add gemini-3.8-flash-low-lp --context-window 1048576 --max-output 65536
llm-proxy model add gemini-3.8-flash-medium-lp --context-window 1048576 --max-output 65536
llm-proxy model add gemini-3.8-flash-high-lp --context-window 1048576 --max-output 65536
# ... 各自绑定 provider ...

# 一步创建路由模型
llm-proxy model add gemini-3.8-flash-lp \
  --route low=gemini-3.8-flash-low-lp \
  --route medium=gemini-3.8-flash-medium-lp \
  --route high=gemini-3.8-flash-high-lp
```

`--route` 可重复，格式 `LEVEL=TARGET`。提供 `--route` 时模型自动成为路由模型，不需要 `--context-window` / `--max-output`。

**手动编辑配置文件**：

路由模型也可以通过直接编辑 `config.toml` 创建。在配置文件中添加 `[models."<hop-id>"]` 段，声明 `reasoning_routes` 表：

```toml
# 三个目标模型（普通模型，各有 provider bindings）
[models."gemini-3.8-flash-low-lp"]
context_window = 1048576
max_output_tokens = 65536
# ... provider bindings ...

[models."gemini-3.8-flash-medium-lp"]
context_window = 1048576
max_output_tokens = 65536
# ... provider bindings ...

[models."gemini-3.8-flash-high-lp"]
context_window = 1048576
max_output_tokens = 65536
# ... provider bindings ...

# 路由模型：统一入口，按思考级别派发
[models."gemini-3.8-flash-lp"]
reasoning_routes = [
    { level = "low",    target = "gemini-3.8-flash-low-lp" },
    { level = "medium", target = "gemini-3.8-flash-medium-lp" },
    { level = "high",   target = "gemini-3.8-flash-high-lp" },
]
```

**约束**：
- 路由模型**不能有** provider bindings、`context_window`、`max_output_tokens`、`features`、`reasoning_levels` 等执行层字段——这些属性从目标模型派生
- 每条 route 的 `level` 必须唯一
- 每个 `target` 必须是已存在的普通模型（不能指向另一个路由模型）
- 所有 target 的客户端可见能力必须一致（`context_window`、`max_output_tokens`、`features`、协议）
- 路由表不能为空（`reasoning_routes = []` 是加载错误）

#### 验证配置

手改 `config.toml` 后，用 `--dry-run` 验证再启动：

```bash
# 验证配置文件（加载 + 校验 + 输出摘要，不启动服务、不写状态）
llm-proxy --dry-run
# 或等价写法
llm-proxy serve --dry-run
```

成功输出：
```text
configuration OK: 5 provider(s), 12 model(s), listen 127.0.0.1:8989
```

验证失败时输出具体错误（如 `routing model "X" route level "low" references unknown target model "Y"`），修复后重新验证。

#### 查看路由信息

```bash
# 查看路由模型的派生元数据和路由表
llm-proxy model info gemini-3.8-flash-lp
```

输出示例：
```text
model=gemini-3.8-flash-lp
context_window=1048576
max_output_tokens=65536
routing_model=true
protocols=openai_chat,openai_responses
features=...
routes:
  1. level=low target=gemini-3.8-flash-low-lp
  2. level=medium target=gemini-3.8-flash-medium-lp
  3. level=high target=gemini-3.8-flash-high-lp
```

`model list` 中路由模型显示 `N routes` 标记，与普通模型的 `N bindings` 区分。

#### 与 launch 的交互

`launch` 命令自动识别路由模型，将其路由级别（low / medium / high）写入客户端的思考能力声明。例如 `launch qwen-code` 会输出：

```json
{
  "capabilities": {
    "reasoning": {
      "profile": "openai-effort",
      "efforts": ["low", "medium", "high"],
      "defaultEffort": "high"
    }
  }
}
```

客户端切换思考档位时，llm-proxy 自动路由到对应的目标模型。

### 删除模型

```bash
llm-proxy model remove <MODEL_ID>
```

---

## 状态查看

### 查看整体状态

```bash
# 显示全部缓存/配置数据（不发网络探测）
llm-proxy status

# 筛选指定模型（支持 Glob 通配符，不触发网络探测，仅展示该模型相关 Provider、Cooldown 及 Binding）
llm-proxy status --model "qwen-*"
llm-proxy status --model deepseek-v4-pro-lp

# 筛选指定 Provider（支持 Glob 通配符，仅展示该 Provider 及其绑定的模型与冷却）
llm-proxy status --provider "ubt-*"
llm-proxy status --provider deepseek

# 组合筛选 Provider 与模型
llm-proxy status --provider deepseek --model "deepseek-*-flash*"

# 配合 --probe 仅对筛选目标执行实时在线探测并过滤显示
llm-proxy status --probe --provider deepseek
llm-proxy status --probe --model "deepseek-*-pro*"
llm-proxy status --probe --provider deepseek --model deepseek-v4-pro-lp
```

`--provider` 与 `--model` 支持独立使用或与 `--probe` 配合使用，支持 Glob 通配符（`*` 匹配任意字符，`?` 匹配单个字符；无通配符时退化为精确匹配）。每个选项只能提供一次。
- **只读模式**（无 `--probe`）：按筛选条件过滤显示的 Providers、OAuth 凭据、Cooldown 及 Models；若指定模式无任何匹配项，给出友好提示。语法非法（如未闭合的 `[`）会直接报错。
- **探测模式**（带 `--probe`）：未知 Provider/Model 或没有 binding 的组合会以非零错误退出，不会把空结果当作成功；匹配成功时只对匹配集合触发在线探活。
- 无 selector 时保持全量展示 / 全量探测行为。

若服务可达，带 `--probe` 的 selector 会由服务端配置校验，因此远程最小 CLI 配置无需复制完整的 Provider/Model 表。服务不可达时，最小配置上的 selector 无法验证，会明确报错而不回退为全量或空探测。

**数据来源提示**：
- `ℹ` — 实时数据（来自 Server）
- `✓` — 探测成功
- `⚠` — 缓存数据（可能过时）
- `✗` — 无数据或错误

模型没有任何 Provider/protocol binding 时，模型行会显示 `WARN no configured provider bindings; not selected for probe`。这表示该模型没有可探测配置；它不同于上游网络或 HTTP 探测失败，后者会显示对应 binding 的 `FAIL` 状态。
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

# 分页查询
llm-proxy usage --period 30d --limit 100 --offset 0
llm-proxy usage --period 30d --limit 100 --offset 100
```

**参数**：
- `--period` — 时间范围（`today`、`7d`、`2026-03-12:2026-03-20`）
- `--provider` / `--model` / `--endpoint` — 按维度过滤
- `--view` — 聚合视图：`by-model`（默认）、`by-provider`、`by-endpoint`、`by-hour`、`by-day`
- `--json` — JSON 输出
- `--limit` — 返回记录数上限（默认 1000，最大 10000）
- `--offset` — 跳过前 N 条记录（默认 0，用于分页）

> **说明**：usage 统计来自本地 SQLite 持久化（Token 用量记录），TUI 中对应 Provider 管理面板的 `[u]` 键可查看单个 provider 的用量摘要。查询结果融合内存缓存（最近记录）与 SQLite（历史记录），支持分页浏览大量数据。

### 使用量自助查询（HTTP 端点）

终端用户可通过 HTTP 端点查看自己 token 的使用量历史，无需 SSH 到服务器运行 CLI。

```bash
# 查询最近 7 天的使用量（按天聚合）
curl -H "Authorization: Bearer YOUR_TOKEN" \
  "http://localhost:6660/v1/token/usage/history"

# 查询最近 30 天（按天聚合）
curl -H "Authorization: Bearer YOUR_TOKEN" \
  "http://localhost:6660/v1/token/usage/history?period=30d&group_by=day"

# 查询今天（按小时聚合）
curl -H "Authorization: Bearer YOUR_TOKEN" \
  "http://localhost:6660/v1/token/usage/history?period=today&group_by=hour"

# 自定义日期范围
curl -H "Authorization: Bearer YOUR_TOKEN" \
  "http://localhost:6660/v1/token/usage/history?period=2026-09-01:2026-09-22"
```

**查询参数**：
- `period` — 时间范围（默认 `7d`）。支持 `today`、`7d`、`30d`、`90d`、`YYYY-MM-DD:YYYY-MM-DD`。超过 90 天返回 400。
- `group_by` — 聚合维度（默认 `day`）。支持 `hour`、`day`。

**响应格式**：
```json
{
  "status": "ok",
  "data": {
    "token_name": "lyc",
    "period": "7d",
    "group_by": "day",
    "summary": {
      "requests": 638,
      "input_tokens": 83497092,
      "output_tokens": 110957,
      "total_tokens": 83608049
    },
    "groups": [
      {
        "key": "2026-09-22",
        "requests": 413,
        "input_tokens": 70696933,
        "output_tokens": 72327,
        "total_tokens": 70769260
      }
    ]
  }
}
```

**限流**：每个 token 每分钟最多 10 次查询，超限返回 429。

**数据隔离**：每个 token 只能查看自己的使用量数据。

#### 数据保留期配置

默认 `db_max_size_mb = 50` 只保留约 3 天数据。如需查询更长时间范围，需增大配置：

| 查询需求 | 建议 `db_max_size_mb` | 磁盘占用 | 保留天数 |
|---------|----------------------|---------|---------|
| 7 天 | 200 | ~120 MB | ~12 天 |
| 30 天 | 500 | ~300 MB | ~30 天 |
| 90 天 | 1500 | ~1.5 GB | ~90 天 |

配置示例（config.toml）：
```toml
[usage]
db_max_size_mb = 500.0  # 支持 30 天查询
```

### 查看与设置冷却状态

```bash
# 查看当前活跃的冷却条目（显示命中时间、命中次数及配置存在性状态）
llm-proxy cooldown list

# 单模型冷却：设置特定模型冷却
llm-proxy cooldown set --provider <PROVIDER_ID> --model <MODEL_ID> --duration 42h

# Provider 级批量冷却：省略 --model 为该 Provider 下全部配置模型设置冷却
llm-proxy cooldown set --provider <PROVIDER_ID> --duration 720h

# 通配符批量冷却：支持 glob 通配符（如 *、?）匹配模型名
llm-proxy cooldown set --provider <PROVIDER_ID> --model "qwen-*" --duration 720h

# 单模型清除：清除指定模型的冷却
llm-proxy cooldown clear --provider <PROVIDER_ID> --model <MODEL_ID>

# Provider 级批量清除：省略 --model 清除该 Provider 下全部模型的冷却
llm-proxy cooldown clear --provider <PROVIDER_ID>

# 通配符批量清除：匹配模型名清除
llm-proxy cooldown clear --provider <PROVIDER_ID> --model "qwen-*"
```

**列表输出格式**：
```text
Active cooldowns (~/.local/share/llm-proxy/cooldowns.json):
PROVIDER          MODEL                         PROTOCOL    DURATION    REMAINING     LAST HIT    HITS    STATUS
ubt-bailian       qwen3.8-flash-256k-lp         openai-chat 720h        719h 55m      2h ago      15      active
ubt-bailian       qwen3.8-turbo-256k-lp         openai-chat 720h        718h 30m      never       0       active
ubt-bailian       qwen3.8-flash                 openai-chat 720h        710h          never       0       [ZOMBIE: unknown model 'qwen3.8-flash']
```
- `LAST HIT`：最近一次命中该冷却的时间（`never`、`30s ago`、`2h ago` 等）。
- `HITS`：被请求命中的累计次数；若设了长冷却但命中始终为 0，便于发现配置与模型名称误写。
- `STATUS`：结合当前配置文件的校验结果；若引用的 Provider/Model 未在配置中声明或未正确绑定，显示 `[ZOMBIE: ...]` 明确指示僵尸条目。

**批量操作安全机制**：
- **交互确认**：在 TTY 终端环境下，省略 `--model` 或使用通配符匹配多个模型时，默认列出目标模型并提示 `Proceed? [y/N]`；单模型精确匹配不触发提示。
- **`--dry-run`**：仅列出将要操作的模型列表，不实际修改或加锁。
- **`--yes` / `-y`**：跳过交互确认直接执行；非 TTY 环境（管道/脚本）下执行批量操作时必须显式指定 `--yes` 或 `--dry-run`，否则报错阻断。

`cooldown set` 按 `(provider, model)` 手动注入冷却条目。`--duration` 支持 `42h` / `1h30m` / `30m` / `7d`，上限 `30d`。不指定 `--protocol` 时对全部客户端协议生效；默认 `kind=manual`。server 运行中时通过 admin API 写入，否则直接写 `cooldowns.json`。

请求路径按冷却类别分级：`network` / `server_error` / `rate_limit` 全冷却时仍探测上游；`auth_error` / `model_unavailable` / `client_error` / `manual` 全冷却时直接返回本地 `503` + `Retry-After`，正文含模型名、剩余秒数和原因。上游 `401`/`403` 会 fallback 但不写冷却；`402` 写 `auth_error` 冷却后 fallback。同一 provider 的 HTTP 5xx 在首字节前按 `[fallback] server_error_retries` 重试。

### 查看请求指标（Metrics）

`GET /admin/metrics` 返回 per-provider×model 的请求指标快照，包含请求计数、可用率、延迟（TTFT 与总耗时 total_ms）和吞吐分位数。该端点挂在 UDS 管理通道上。

```bash
# 查询当前指标（默认 lifetime 累计窗口）
curl --unix-socket ~/.local/state/llm-proxy/management.sock \
  http://localhost/admin/metrics

# 按时间窗口过滤（支持 5m、1h、24h、lifetime）与 provider/model
curl --unix-socket ~/.local/state/llm-proxy/management.sock \
  "http://localhost/admin/metrics?window=5m&provider=openai"

# 查询历史持久化快照（后台每 60 秒自动落盘至 SQLite，重启后可查）
curl --unix-socket ~/.local/state/llm-proxy/management.sock \
  "http://localhost/admin/metrics/history?window=1h&limit=20"
```

**响应格式**：
```json
{
  "status": "ok",
  "data": {
    "window": "5m",
    "is_partial": false,
    "metrics": [
      {
        "provider": "openai",
        "model": "gpt-4o",
        "window": "5m",
        "is_partial": false,
        "requests": { "total": 120, "completed": 115, "no_response": 2, "upstream_error": 3, "client_disconnected": 0 },
        "availability": { "rate": 0.983, "numerator": 118, "denominator": 120 },
        "latency": {
          "ttft_ms": { "p50": 180, "p95": 450, "p99": 820, "sample_count": 12 },
          "total_ms": { "p50": 1250, "p95": 2300, "p99": 3100, "sample_count": 12 }
        },
        "throughput": { "generation_tps": { "p50": 45.2, "p95": 78.1, "p99": 95.0, "sample_count": 10 } }
      }
    ],
    "uptime_seconds": 3600,
    "generated_at": "2026-10-03T12:00:00Z"
  }
}
```

**窗口与指标说明**：
- **`window` 过滤**：支持 `5m`、`1h`、`24h` 与 `lifetime`（默认）。未满窗口时 `is_partial` 为 `true`（例如实例刚启动 2 分钟时查询 `5m` 窗口），表示统计仍在窗口爬坡期。
- **`total_ms` 分位数**：在首字延迟 `ttft_ms` 之外，并列提供端到端总延迟（`total_ms`）的 p50、p95、p99 及采样数。
- **快照持久化与历史查询**：后台异步任务每 60 秒自动生成快照并持久化落入 `state_dir/metrics.db`（SQLite），写入不阻塞请求热路径；停机时执行平滑刷盘，支持通过 `GET /admin/metrics/history`（或 `?history=true`）追溯重启前后的历史快照。
- **可用率口径**：分子 = `completed + upstream_error`（上游给出了响应，含错误码）；分母 = 分子 + `no_response`（超时/断连/流中断）。`client_disconnected` 不计入分母。
- **采样策略**：延迟和吞吐使用间隔采样——每 10 次请求写一次环缓冲区（容量 100 条），覆盖约 1000 次请求的滑动窗口历史。

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
llm-proxy launch opencode      # OpenCode
```

**配置目录与隔离参数口径**：
所有 `--<client>-config-dir <dir>` 参数指向该客户端的配置目录本身（与原生隔离环境变量所指目录一一对应；例外：claude-desktop 无原生 env，只承诺写侧平台路径），不是 `$HOME`。做隔离实验时：指向空目录，再按映射表用对应环境变量启动客户端。

### 客户端配置映射表

| 客户端 | 目标目录参数 | 托管写入文件 | 默认目录解析规则 | 原生隔离环境变量 | 探活与验证级别 |
|---|---|---|---|---|---|
| codex | `--codex-config-dir <dir>` | `config.toml`、`llm-proxy-<hash>-model-catalog.json` | `$CODEX_HOME` 若设置，否则 `$HOME/.codex` | `CODEX_HOME` | 探活：`CODEX_HOME=<dir> codex exec --skip-git-repo-check "<最小请求>"` |
| codex-desktop | `--codex-config-dir <dir>`（与 codex 共用） | 同上 | 同 codex | 同 `CODEX_HOME`（桌面入口 shell 继承 env） | 探活：复用 codex 行（代码级同实现；GUI env 继承未验证） |
| pi | `--pi-config-dir <dir>` | `models.json`（直接写在目标目录下，无 `.pi/agent` 嵌套） | `$PI_CODING_AGENT_DIR` 若设置，否则 `$HOME/.pi/agent` | `PI_CODING_AGENT_DIR` | 探活：`PI_CODING_AGENT_DIR=<dir> pi -p --no-session --model <proxy/model> "<最小请求>"` |
| qwen-code | `--qwen-config-dir <dir>` | `settings.json` | `$QWEN_HOME` 若设置，否则 `$HOME/.qwen` | `QWEN_HOME` | 探活：`QWEN_HOME=<dir> qwen -m <model> -p "<最小请求>"` |
| claude-code | `--claude-config-dir <dir>` | `settings.json`、**`.claude.json`（侧车 onboarding，见下）** | `$CLAUDE_CONFIG_DIR` 若设置，否则 `$HOME/.claude` | `CLAUDE_CONFIG_DIR` | 探活：`CLAUDE_CONFIG_DIR=<dir> claude -p "<最小请求>"` |
| opencode | `--opencode-config-dir <dir>` | `opencode.json` | `$OPENCODE_CONFIG_DIR` 若设置，否则 `${XDG_CONFIG_HOME:-$HOME/.config}/opencode` | `OPENCODE_CONFIG_DIR`（叠加 precedence：global < custom < project） | 探活：`OPENCODE_CONFIG_DIR=<dir> opencode run "<最小请求>"` |
| claude-desktop | `--claude-desktop-config-dir <dir>` | `claude_desktop_config.json` | macOS `~/Library/Application Support/Claude/`；Windows `%APPDATA%\Claude\`；Linux 未指定默认报错 | 无原生 env（平台路径见解析规则） | 仅写侧：按平台常量写对路径 + Linux 双分支（默认报错/显式允许） |

> **侧车 onboarding 文件 `.claude.json` 的位置**：claude-code 是唯一在配置目录**之外**还写一个侧车文件的客户端（内容为 `hasCompletedOnboarding`，用于跳过首次启动的浏览器 OAuth 弹窗）。它的位置取决于「是否显式指定配置目录」，而非目录形状：
>
> | 条件 | `settings.json` | `.claude.json` |
> |---|---|---|
> | 传 `--claude-config-dir <dir>`，或已设 `CLAUDE_CONFIG_DIR=<dir>` | `<dir>/settings.json` | **`<dir>/.claude.json`（目录内部）** |
> | 两者都未设（全默认） | `$HOME/.claude/settings.json` | **`$HOME/.claude.json`（家目录根，与 `$HOME/.claude/` 同级）** |
>
> 此规则与 Claude Code 官方在 `CLAUDE_CONFIG_DIR` 下的实际行为一致（实测 claude 2.1.293）。做隔离实验时，`.claude.json` 不会写到你指定的目录之外。

### 旧参数迁移与行为变更说明

所有旧版 `--*-home` 参数均已被弃用并移除活别名。直接使用旧参数将在命令行解析阶段报错并提示对应新参数：

| 旧参数（已拒绝） | 新参数 | 对应原生环境变量 |
|---|---|---|
| `--codex-home <dir>` | `--codex-config-dir <dir>` | `CODEX_HOME` |
| `--pi-home <dir>` | `--pi-config-dir <dir>` | `PI_CODING_AGENT_DIR` |
| `--qwen-home <dir>` | `--qwen-config-dir <dir>` | `QWEN_HOME` |
| `--claude-home <dir>` | `--claude-config-dir <dir>` | `CLAUDE_CONFIG_DIR` |
| `--claude-desktop-home <dir>` | `--claude-desktop-config-dir <dir>` | （无原生 env，平台路径见映射表） |
| `--opencode-home <dir>` | `--opencode-config-dir <dir>` | `OPENCODE_CONFIG_DIR` |

**重要行为变更**：
1. **`serve` 不再自动刷新客户端配置**：服务端启动和运行期实现零客户端文件副作用，改端口或重置监听地址后不会自动覆写客户端文件。客户端需通过显式重新执行 `llm-proxy launch <client>` 切至新端点。
2. **`launch pi` 语义对齐**：`--pi-config-dir <dir>` 直接将 `models.json` 写入 `<dir>` 本身，不再在内部生成 `.pi/agent` 嵌套子目录。
3. **`launch claude-desktop` 平台路径收窄**：默认写入各操作系统标准配置目录。Linux 下因无官方客户端默认拒绝写入，如需自定义写入需显式传入 `--claude-desktop-config-dir <dir>`；旧版本曾写在 `$HOME/claude_desktop_config.json` 的遗留文件不会被自动清理，请用户按需手动删除。
4. **`launch claude-code` 侧车 onboarding 文件位置修正**：传 `--claude-config-dir <dir>` 时，`.claude.json` 改写到 **`<dir>` 内部**，不再写到 `<dir>` 的父目录——旧版本会逃出你指定的隔离目录（极端情况下落到 `/tmp` 等共享路径）。默认（不传该参数且未设 `CLAUDE_CONFIG_DIR`）时仍写 `$HOME/.claude.json`，**行为不变**。若旧版本隔离实验曾在目录外留下 `.claude.json`，可按需手动删除。

**常用选项**：
- `--dry-run` — 只打印配置，不写入文件
- `--profile <PROFILE>`（仅 `claude-desktop`）— 写入指定的 llm-proxy profile 配置名（不会切换客户端当前 active profile）

`launch` 只注入 llm-proxy 支持的 provider、模型列表和元数据，不设置客户端默认模型，也不替客户端选择 provider/profile。Codex 是例外：`launch codex` 会把 Codex 的默认 provider 和默认 catalog 切换到当前 llm-proxy 实例，但不会设置具体默认模型，因此执行后可以直接运行 `codex`。已有其他 `[model_providers.*]` 定义会保留。

```bash
llm-proxy launch codex
codex
```

如果手动切换到其他 Codex provider，必须同时切换到该 provider 对应的 catalog；Codex 不会根据 catalog 自动推断 provider。

### Pi 思考能力与协议选择

`launch pi` 分别生成 Chat、Responses 和 Anthropic provider。Anthropic 的 `baseUrl` 使用 `/anthropic`，由 SDK 追加 `/v1/messages`，不要手动再加一层 `/v1`。

思考能力来自模型的显式配置。客户端档位仅映射到代理前端级别，上游 API 值仍由代理处理；`launch` 不写入 Pi 默认思考设置，但声明能力后，Pi 原有的默认档位可能开始生效。

Anthropic 使用预算模式，不提供实际等同 `high` 的独立 `xhigh` 档位。若任一 fallback 路径不能保留该预算语义（如经 Chat/Responses 派生），模型仍被注册，但不向 Pi 宣告 Anthropic 思考控制能力，并输出提示；这不表示上游模型本身没有思考能力。可选择该模型的 OpenAI 协议条目使用已声明的 effort 档位。显式 Anthropic 预算和关闭请求不会被代理默认级别覆盖，已有模型/provider 策略仍有优先权。

### Qwen Code 思考能力

`launch qwen-code` 对声明了 reasoning levels 的模型（含路由模型的 route levels）自动输出 `capabilities.reasoning`：

```json
{
  "capabilities": {
    "reasoning": {
      "profile": "openai-effort",
      "efforts": ["low", "medium", "high"],
      "defaultEffort": "high"
    }
  }
}
```

- `profile` 固定为 `"openai-effort"`（qwen 0.24.7 chat 端点的枚举值，写错会被 `validateReasoningDeclaration` 拒绝）；
- `efforts` 取自模型的 `reasoning_levels`（路由模型取其 route levels）；
- `defaultEffort` 取自 `default_reasoning_level`（路由模型取首条 route）；
- 路由模型不提供 `off`/`none` 档（对齐 pi launch 行为）；
- 不支持 reasoning 的模型不输出该字段。

如果手写 `capabilities.reasoning` 补救，必须包含 `profile` 字段，否则 qwen 0.24.7 自定义端点会报 `invalid profile, efforts or defaultEffort`。

### Codex Auto-Review

Codex Guardian 的 auto-review 模型可在 llm-proxy 配置中独立设置，不需要加入 OpenAI Sub 或其他 product 的普通模型选择：

```toml
[codex]
auto_review_model = "codex-auto-review-sub-lp"

[codex.model_overrides.deepseek-v4-pro-lp]
auto_review_model = "deepseek-review-lp"
```

模型级配置优先于全局配置。配置值必须是 `config.toml` 中存在且具有 OpenAI Responses binding 的逻辑模型 ID。被引用的 review model 会自动加入 Codex catalog，但不会进入普通 product 模型列表；删除或修改仍被引用的模型/provider binding 会被阻止并提示引用影响。

未配置 auto-review model 时，`launch codex` 保留 Codex 自身的 provider-default/fallback 行为，并输出 warning。配置了 review model 时，Guardian 会沿用 Codex 当前默认的 llm-proxy provider，只替换 review model ID，不会自动切换到其他 provider。真实 Guardian 审批验证应使用隔离的 Codex home、配置、运行时路径和凭证环境；本项目提供显式入口：

```bash
just e2e-codex-auto-review
```

该验证会检查 coding 与 review 请求均通过隔离代理的 `/admin/usage` 到达，并不能据此把账户 mapping 结果称为原生 `codex-auto-review`。

### 远程客户端模式（Remote Client Mode）

`launch` 支持指向**远端 server**，让多台机器上的客户端共享同一个 llm-proxy 实例。配置方式是在 `config.toml` 中添加 `[remote]` 段（与 `[server]` 互斥）：

```toml
# 场景 A：远端 server 开放模式（无 token 验证）
[remote]
url = "https://llm-proxy.example.com"

# 场景 B：远端 server Token 模式（需要认证）
[remote]
url = "https://llm-proxy.example.com"
auth_mode = "token"              # 显式声明（推荐）
# auth_token_env 默认为 "LLM_PROXY_API_KEY"，从同名环境变量读取 token
# 如果 server 使用不同的环境变量名，可显式指定：
# auth_token_env = "MY_CUSTOM_TOKEN"
```

配置好 `[remote]` 后，`launch` 命令会自动：
1. **URL 注入**：客户端配置的 `baseUrl` 指向远端地址（而非 `127.0.0.1`）
2. **凭证注入**：根据远端 server 的认证模式注入对应凭证
3. **清单注入**：从远端 server 获取模型列表写入客户端配置

#### 认证模式与凭证注入

远端 server 有两种认证模式，`launch` 根据配置自动判定（**不猜测**）：

| 模式 | 配置 | Qwen/Claude/Pi/OpenCode 凭证 | Codex 凭证 |
|------|------|------------------|------------|
| **开放模式** | `auth_mode = "open"` 或未配置且无 token | 占位值 `"local"` | 不注入（原生免 key） |
| **Token 模式** | `auth_mode = "token"` + token 配置 | 环境变量引用（Qwen `envKey`；Claude 顶层 `apiKeyHelper`；Pi `$ENV`；OpenCode `${ENV}`） | 远端 Token 模式不写 `.env`；本地 Token 模式才写 `.env`（本地 mock token） |

**状态判定规则**：
- 显式 `auth_mode` 字段优先（推荐）
- 未显式声明时，根据 `auth_token_env` 指定的环境变量是否已设置推断（已设置 → Token 模式，否则 → 开放模式）

#### 清单一致性检查

远端 server 的模型列表可能变化，而本地 launch 写入的清单是"写入即定格"。使用 `--check` 标志检测漂移：

```bash
llm-proxy launch codex --check
```

该命令会：
1. 从远端 server 获取当前模型列表（带 token 认证）
2. 与本地配置对比
3. 报告新增/消失的模型
4. **不执行 launch**（仅检查）

若发现漂移，重新运行 `llm-proxy launch <client>`（不带 `--check`）即可更新本地清单。

#### 管理功能远程访问

远端模式下，`/admin/*` 端点通过 HTTP + token 认证访问（与本地 UDS 管理通道并存）：

```bash
# 远程查看状态（需要 token）
curl -H "Authorization: Bearer lp-your-token" https://llm-proxy.example.com/admin/status

# 远程查看模型列表
curl -H "Authorization: Bearer lp-your-token" https://llm-proxy.example.com/admin/model/list
```

本地 CLI 仍优先使用 UDS 管理通道（无需认证），远端 CLI 走 HTTPS + token。

#### ⚠️ 凭证安全提示

**Remote Token 模式下，真实 token 不写入客户端配置文件**：`launch` 只写环境变量引用/helper，客户端运行时从 `auth_token_env` 指定的环境变量读取：
- Qwen Code：`settings.json` 只写 `envKey`，不写 `env.<key>` 值
- Claude Code：顶层 `apiKeyHelper`（`echo $ENV_VAR`），不写 `env.ANTHROPIC_AUTH_TOKEN`
- Pi：`apiKey: "$ENV_VAR"`
- OpenCode：`apiKey: "${ENV_VAR}"`
- Codex：远端 Token 模式不写 `.env`；本地 Token 模式才写 `.env`（本地 mock token）

`[remote]` 配置段**只接受环境变量引用**（`auth_token_env`），不接受明文 token——与 provider 的 `api_key_env` 设计一致，确保 `config.toml` 中不出现明文密钥。请确保：
- 启动客户端前，shell 已设置 `auth_token_env` 指定的环境变量
- 客户端配置文件权限仍建议 `0600`（配置中可能含非敏感元数据）
- 不在公共仓库或共享环境中提交/泄露配置文件
- 定期轮换 token（使用 `llm-proxy token delete` + `token create`）

> **切换 `auth_token_env` 时旧键不自动清理**：`launch` 只管理当前 `auth_token_env` 对应的 env 键，不扫描/删除旧名称下的键（包括之前 launch 写入的键）。这是有意保留，避免误删你手工维护或故意保留的配置。需要清理时请自行检查 `settings.json` 的 `env` 与 `modelProviders`。

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

## Token 命令

`token` 管理**入站访问控制**：给不同的人/工具分发独立 API key，限制各自可用模型、设置多窗口配额、按 key 统计用量。

### 配置存储与目录化

Token 配置支持两种形式（存放在与 `config.toml` 同级的配置目录下）：
- **目录模式（推荐）**：`~/.config/llm-proxy/tokens/*.toml`，每个文件定义一个独立的 Token（文件名建议为 `<name>.toml`）。支持多人管理或细粒度维护，结构清晰。
- **单文件模式（传统）**：`~/.config/llm-proxy/tokens.toml`，在单个文件中使用 `[[tokens]]` 数组定义所有 Token。

单 Token 文件示例（`tokens/alice.toml`）：
```toml
key = "lp-xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"
name = "alice"
allowed_models = ["gpt-4o", "deepseek-v3"]

# gpt-4o 独立配额组
[[model_quota_groups]]
models = ["gpt-4o"]

  [[model_quota_groups.quota_windows]]
  window = "5h"
  quota_mode = "requests"
  quota_limit = 100

# deepseek-v3 独立配额组
[[model_quota_groups]]
models = ["deepseek-v3"]

  [[model_quota_groups.quota_windows]]
  window = "30d"
  quota_mode = "tokens"
  quota_limit = 10000000
```

> **互斥与冲突检测**：`tokens/` 目录与 `tokens.toml` 单文件是互斥的。若两者同时存在，代理启动或重载时将拒绝启动并报错，防止配置分裂。可以使用 `llm-proxy token migrate` 将现有的 `tokens.toml` 一键迁移至 `tokens/` 目录。
>
> **开放模式（向后兼容）**：`tokens/` 目录或 `tokens.toml` 不存在或无有效 Token 时，代理不启用认证（开放模式，所有请求放行），个人使用零摩擦。一旦写入至少一个 Token，所有代理路由（`/openai/*`、`/anthropic/*` 等）都会要求携带 Token；`/admin/*` 本地管理路由不受影响。

```bash
# 创建 token（生成 lp- 前缀的随机 key，写入 tokens.toml 或 tokens/ 目录）
llm-proxy token create --name alice --models gpt-4o,gpt-4o-mini

# 带配额窗口（可重复 --window/--quota-mode/--quota-limit 配置多窗口，任一超限即拒绝）
llm-proxy token create --name alice \
  --models gpt-4o \
  --window 5h  --quota-mode requests --quota-limit 60 \
  --window 30d --quota-mode tokens   --quota-limit 5000000 \
  --expires 2026-12-31T23:59:59Z

# 模型作用域配额窗口（指定 --group-models 为特定模型组设置独立限额）
llm-proxy token create --name bob \
  --models claude-sonnet-4,gpt-4o,deepseek-v3 \
  --group-models claude-sonnet-4,gpt-4o \
  --window 1d --quota-mode requests --quota-limit 100

# 迁移旧配置：将 tokens.toml 拆分迁移至 tokens/ 目录（原文件重命名为 tokens.toml.migrated 备份）
llm-proxy token migrate

# 列出所有 token 及用量摘要
llm-proxy token list
llm-proxy token list --json

# 查看单个 token 的详细窗口用量（按 key 或 name）
llm-proxy token usage alice

# 删除 token（同时清理其配额状态；历史 usage_records 保留作审计）
llm-proxy token delete alice

# 重置 token 用量计数
llm-proxy token reset-usage alice
```

**参数说明**：

| 参数 | 含义 |
|------|------|
| `--name` | 人类可读名称（唯一） |
| `--models` | 允许使用的模型，逗号分隔；**省略 = 允许所有模型** |
| `--group-models` | 模型组过滤：当指定时，`--window`/`--quota-mode`/`--quota-limit` 定义的配额仅对这些模型生效（写入 `model_quota_groups`） |
| `--window` | 配额窗口时长：`30m` / `5h` / `1d` / `7d` / `30d` / `1h30m` |
| `--quota-mode` | 计量方式：`tokens`（按 token 数）或 `requests`（按请求次数），与每个 `--window` 对齐 |
| `--quota-limit` | 窗口内上限，与每个 `--window` 对齐 |
| `--expires` | ISO 8601 过期时间，省略则永不过期 |

**配额与用量**：
- 多窗口**叠加生效**，任一窗口超限即返回 `429`。
- **AND 语义与作用域隔离**：当请求命中多个配额组（例如全局配额与特定模型组配额，或多个重合模型组）时，所有命中的配额组均执行检查与扣费（任一超限即拒绝请求）；未命中的配额组完全隔离不受影响。
- 窗口过期后**懒重置**（下次请求时清零，无需后台任务）。
- `requests` 模式按成功请求次数计；`tokens` 模式按 token 数计，并支持**模型倍率**统一代价（倍率配置见 `tokens.toml` 的 `[model_ratios]` / `[defaults]`）。

**异常保护与错误类别容忍限制**：
为防止客户端程序死循环或错误调用导致上游账号被限流封禁，llm-proxy 对每个 Token 启用了**同类错误容忍与冷却保护**：
- **最大容忍次数**：同一 Token 在滑动窗口内（默认 10 分钟），若累计触发同一类上游 4xx 错误（如工具参数 schema 错误、缺失必要字段等）达到上限（默认 3 次），该 Token 将立即进入冷却阻断期。
- **超出后的报错**：在冷却期内（默认 300 秒），该 Token 发起的所有后续请求均在代理入口直接拦截，返回 `429 Too Many Requests`：
  ```json
  {
    "error": {
      "message": "token blocked after repeated upstream errors; wait for cooldown to expire",
      "type": "rate_limit_error"
    }
  }
  ```
- **隔离性与自愈**：冷却仅对该异常 Token 生效，其他正常 Token 完全不受影响；冷却时间结束后自动解封恢复正常。服务端可在 `config.toml` 的 `[protection.error_cooldown]` 中调整容忍次数（`max_errors_per_class`）与冷却时长（`cooldown_seconds`）。

**配置生效与手动重载**：
- `token create/delete/migrate` 等写命令若检测到 server 正在运行，会自动通知其 `/admin/config/reload` 使变更即时生效；否则下次 `llm-proxy serve` 启动时加载。
- 手动编辑 `tokens/` 目录或 `tokens.toml` 后需 `llm-proxy restart` 或通过 `/admin/config/reload` 触发手动重载生效（注意：服务不使用自动文件监听热重载，必须手动触发）。

**使用 token 的客户端**：把代理地址配到客户端，并用 token key 作为 API key —— OpenAI 协议走 `Authorization: Bearer <key>`，Anthropic 协议走 `x-api-key: <key>`。

---

## Connect 命令

`connect` 是 `provider add` 的 TUI 交互方式，通过交互式界面添加 provider：

```bash
# 启动 TUI 交互添加 provider
llm-proxy connect

# 指定产品 + 模型（非交互，一步完成）
llm-proxy connect deepseek --model deepseek-v4-pro-lp

# OpenRouter：模型模板来自在线目录（450+ 模型，首次进入联网拉取并本地缓存 24h）
llm-proxy connect openrouter --model anthropic/claude-sonnet-4 --model deepseek/deepseek-chat
```

> 注意：带产品名的 `connect <产品>` 必须同时指定 `--model`，且此时是非交互直接添加；完整的 TUI 交互流程（产品选择、命名屏、fallback 配置、探测确认）只在使用无参数 `connect` 时出现。

**通用参数**：

| 参数 | 作用 |
|------|------|
| `--model <ID>` | 选择模型模板，可重复指定多个 |
| `--api-key-env <VAR>` | 指定 API Key 环境变量名 |
| `--no-api-key` | 不绑定 API Key（如 Ollama 等无需认证的产品） |
| `--type <PROTOCOL>` | 指定协议类型 |
| `--endpoint-url <URL>` | 覆盖端点地址 |
| `--skip-probe` | **跳过网络可达性探测**，直接写入配置（见下文「探测失败的处置」） |

**TUI 流程**：

```
产品选择 → 命名屏（仅第 2+ 次 connect）→ env 选择 → 模型选择 / fallback 配置 → 探测确认 → 完成
```

1. 选择产品（如 DeepSeek、OpenAI Subscription、Anthropic、OpenRouter 等）
2. **命名屏**：第 2+ 次 connect 同一产品时出现，为 provider 指定名称（预填 `产品名-2` 等递进 ID，实时校验重名）；首次 connect 自动跳过
3. 确认 API Key 环境变量（带 `you may want` 推荐标记，可按 `s` 跳过）
4. 模型选择（勾选模型模板）或 **fallback 配置**（把新 provider 配置为已有 provider 的 fallback）：
   - 首次 connect 默认进模型选择
   - 再次 connect 默认进 fallback 配置界面
5. **探测确认**：默认高亮「探测（推荐）」，可切换为跳过探测；零选择模型时跳过本屏
6. 完成添加

> **提示**：`connect` 和 `provider add` 功能等价，`connect` 提供更友好的 TUI 交互体验。各界面的完整快捷键见 [TUI 使用指南](tui-guide.md)。

### OpenRouter 动态模型目录

OpenRouter 的模型模板不内置在二进制中，而是基于公开免鉴权接口 `GET /api/v1/models` **在线发现**：

- 首次进入即联网拉取 450+ 模型，落盘到 State 目录（权限 0600），**24 小时内再次进入直接用缓存**；
- 超过 24h 未刷新时界面显示**降级**警告，可按 `r` 手动强刷（防抖窗口内连按只发 1 次请求）；
- 断网自动降级到历史缓存；无缓存时提供 `Esc` 退出与 `r` 重试；
- 模型元数据（上下文窗口、最大输出、功能特性、思考档位）由接口返回值**推导**，缺失的字段保持未知（显示 `?`），不虚构默认值。

### 探测失败的处置

选定模型并确认探测后，代理会**逐个**探活所选模型（不因单个失败中断整批），把全部失败聚合进一条清单：

```
1 / 2 个模型当前不可达（其余 1 个已通过探测）：
  - stepfun-step-5-preview-or-lp: connectivity probe failed ... 上游返回 404 ...

选择「继续」可照常写入配置，这些模型稍后仍可重试。
```

按运行环境分三条通路：

| 环境 | 行为 |
|------|------|
| **TUI** | 弹出警告框列出不可达模型，选「继续」以跳过探测重试写入，选「返回」回到产品/模型选择 |
| **CLI + 交互终端**（TTY，且未设 `LLM_PROXY_NO_INTERACTIVE`） | 打印同一份清单后询问 `是否照常写入配置（跳过网络探测）？[y/N]`；答 `y` 重跑写入，答 `n` 以 `连接已取消：存在不可达模型` 退出且不写配置 |
| **CLI + 非交互终端**（管道、重定向，或设了 `LLM_PROXY_NO_INTERACTIVE`） | 不提问，保留原错误退出（exit 1）不写配置，并提示加 `--skip-probe` 显式放行 |

```bash
# 非交互场景一次性放行：跳过探测直接写入配置
llm-proxy connect openrouter --model anthropic/claude-sonnet-4 --skip-probe
```

> **`--skip-probe` 只跳过网络探测，不豁免本地凭据校验**——两者是相互独立的检查。
> `provider add` 同样支持该参数。

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
# TUI 交互：选择 openai-sub 产品，勾选当前可用模型模板（gpt-5.6-sol-sub-lp、gpt-5.6-terra-sub-lp、gpt-5.6-luna-sub-lp、gpt-6-astra-sub-lp、gpt-5.5-sub-lp、gpt-5.4-sub-lp、gpt-5.4-mini-sub-lp）
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
# TUI 交互：选择 openai-sub 产品，勾选当前可见模型模板
llm-proxy provider add
# 选择：gpt-5.6-sol-sub-lp、gpt-5.6-terra-sub-lp、gpt-5.6-luna-sub-lp、gpt-6-astra-sub-lp、gpt-5.5-sub-lp、gpt-5.4-sub-lp、gpt-5.4-mini-sub-lp
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
- 更多架构边界说明请参考 [`spec.md`](../../spec.md) 的"架构边界与不变量"章节

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
- `status --probe`：按 active evidence、短期探测缓存、singleflight 和 request-frequency 规则获取当前上游探测结果；需要真实探测时更新 probe cache
- 探测成功不会写入 active evidence；active evidence 只来自成功的正常代理请求
- `status --probe` 不刷新 OAuth token，也不清除 cooldown；分别使用 `provider refresh` 和 `cooldown clear`

### 3. 远程模式使用 `host.docker.internal`

容器内无法访问 `localhost`，需要用 `host.docker.internal` 指向宿主机。

### 4. 确认恢复后再清冷却

瞬时故障的自动冷却会自己过期。只有确认上游已恢复，或冷却类别/时长明显不对时，才手动清理：

```bash
llm-proxy cooldown list
llm-proxy cooldown clear --provider <PROVIDER_ID>
```

持久型全冷却（额度、模型不可用、手动长冷却）会直接给客户端 `503`，清掉之前先看 `kind` / `reason`，避免刚清完又打到已知不可用的上游。

---

## Changelog

### 2026-10-13 — OpenRouter 动态模型目录、探测确认与探测失败放行；多家 Provider 思考等级补全

**新增**：

- **OpenRouter 作为可接入产品**：模型模板不再内置，进入时基于公开免鉴权 `GET /api/v1/models` **联网发现 450+ 模型**，本地缓存 24h（0600 权限、原子写入、损坏隔离）。模型选择屏支持 **Fuzzy 模糊搜索**与 `r` 键强刷（防抖），断网自动降级到历史缓存，缓存过期显示**降级**警告。
- **探测确认屏**：模型确认后、网络探测前新增一层确认，默认高亮「探测（推荐）」，可切换为跳过探测；覆盖全部产品。零选择模型时不出现本屏。
- **探测失败不再硬性中止**：逐个探活全部选中模型后聚合清单，按环境分三条通路交还用户——TUI 警告框「继续」、交互终端 `[y/N]` 询问、非交互终端报错并提示放行开关。
- **新增 `--skip-probe` 参数**（`connect` 与 `provider add` 均支持）：跳过网络可达性探测直接写入配置；**不豁免本地凭据校验**。

**修复 / 增强**：

- **`model list` 新增 `[thinking: client-driven]` 标记**：声明了思考等级的模型会显示该标记，接入后客户端会出现思考控制项；未声明的不显示（不虚构默认）。
- **补全多家 Provider 的思考等级声明**：百炼 Qwen 3.7 Plus 与 Token Plan、Kimi K3、StepFun step-3.7-flash、OpenAI / Codex 全家族——此前这些模型接入 Codex 后以 `reasoning_effort: "none"` 运行且 TUI 隐藏思考控制（表现为响应"失忆"、注意力漂移）。
- 修复**空配置**下执行 connect 写入时的 panic（table 索引越界）。
- 修复 **OpenRouter 探测被硬编码跳过**、以及探测误用上游 ID 而非前端 ID 的缺陷。

> 涉及具体模型清单的背景与取证细节不在手册收录范围。

### 2026-09-25 — CLI 破坏性变更：`--config` → `--config-dir`

**变更**：全局参数从 `--config <file>` 改为 `--config-dir <dir>`。

**理由**：实例 = 目录（config.toml + tokens.toml + oauth_accounts.json 都在目录内），CLI 参数直接表达目录更清晰，与多实例分片方案对齐。

**迁移**：

```bash
# 改前
llm-proxy --config /path/to/config.toml serve
llm-proxy -c /path/to/config.toml status

# 改后
llm-proxy --config-dir /path/to/ serve
llm-proxy -d /path/to/ status
```

**默认行为不变**：不传参数时仍使用 `$XDG_CONFIG_HOME/llm-proxy/`（内部拼接 `config.toml`）。

---

## 相关文档

- [TUI 使用指南](tui-guide.md) — 交互式界面的快捷键与操作说明
- [OAuth 账户管理](user-guide-oauth-accounts.md) — OAuth 认证流程
- [项目 README](../../README.md) — 项目入口和 Quickstart
