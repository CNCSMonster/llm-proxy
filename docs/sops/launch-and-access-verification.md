# SOP：launch 与接入验证（Provider / Model / LLM App）

> 适用范围：接入新 provider、新 model、新 LLM app 时的接入成功验证。
> 核心定位：本 SOP **以 `launch` 客户端接入为主线**，`status` / `provider` / `model` 命令作为配置管理层面的辅助验证。

## 核心原则（一句话）

**接入成功 = 用真实客户端（codex/pi/qwen-code）通过代理，以真实使用方式（无头/交互、单轮/多轮、工具调用）完成真实任务，且配置管理命令（status/provider/model）状态正确、有客观证据。**

分层可信度：`codex exec`（无头） < 真实终端 TUI 交互。前者可自动化、必须有；后者是最终人工确认。

## 0. 前置条件（每次验证先做）

> 下文中 `127.0.0.1:8989` 是被验证代理的地址示例，测试者可替换为实际代理地址（本地、远程或独立验证实例）。`<model>` / `<已有模型>` 指被验证 provider 绑定的模型。

```bash
# 1. 确认代理在运行
ps aux | grep 'llm-proxy.*serve' | grep -v grep
curl -s http://127.0.0.1:8989/openai/v1/responses -H 'Content-Type: application/json' \
  -d '{"model":"<已有模型>","input":"ping","stream":false}'   # 返回正常响应

# 2. 确认版本是当前最新
llm-proxy version    # commit 与 git HEAD 一致（否则重新 cargo install --force）

# 3. 记录基线
echo "proxy_version=$(llm-proxy version | grep commit)" && git -C llm-proxy-rust-v2 log -1 --oneline
```

### 0.1 环境依赖（真实客户端验证的前提）

验证矩阵中 **L1/L3/L4（`codex exec`）和 L6（TUI 交互）依赖安装了对应真实客户端的验证环境**：

- 运行 `codex` / `pi` / `qwen-code` 前先确认二进制存在：`which codex`（或用 `find ~ -name codex` 排查非 PATH 位置）
- 仅有客户端配置目录（如 `~/.codex/`）不等于已安装二进制——配置可能由 `llm-proxy launch` 生成但客户端从未安装
- 无真实客户端的验证环境（如精简远程环境）只能做协议层验证（L2/L5/L5a/L7）；真实客户端层（L1/L3/L4/L6）需在安装了对应客户端的机器上补做，不能跳过或宣称完成

## 1. 接入新 Provider

### 1.1 配置管理验证（`provider` / `status` 命令）

```bash
# provider 出现且状态正确
llm-proxy provider list                     # 新 provider 在列表中
llm-proxy provider info <name>              # 认证类型、协议、端点正确
llm-proxy status --refresh                  # 该 provider 显示 OK（非 Key 缺失/Cooldown/网络不通）

# Key 未配置时的行为（如适用）
# status 应显示 "Key 缺失" 而非静默跳过或报错
```

### 1.2 协议层验证（curl 三协议各一发）

```bash
# OpenAI Chat
curl -s http://127.0.0.1:8989/openai/v1/chat/completions -H 'Content-Type: application/json' \
  -d '{"model":"<model-lp>","messages":[{"role":"user","content":"hi"}]}'
# OpenAI Responses
curl -s http://127.0.0.1:8989/openai/v1/responses -H 'Content-Type: application/json' \
  -d '{"model":"<model-lp>","input":"hi","stream":false}'
# Anthropic Messages
curl -s http://127.0.0.1:8989/anthropic/v1/messages -H 'Content-Type: application/json' \
  -H 'x-api-key: test' -H 'anthropic-version: 2023-06-01' \
  -d '{"model":"<model-lp>","max_tokens":100,"messages":[{"role":"user","content":"hi"}]}'
# Anthropic Messages（content 数组形式——两种形式都必须验证）
curl -s http://127.0.0.1:8989/anthropic/v1/messages -H 'Content-Type: application/json' \
  -H 'x-api-key: test' -H 'anthropic-version: 2023-06-01' \
  -d '{"model":"<model-lp>","max_tokens":100,"messages":[{"role":"user","content":[{"type":"text","text":"hi"}]}]}'
```

**通过标准**：三种协议均返回 200 + 有效内容；Anthropic 的字符串 content 与数组 content **两种形式都要验证**（规范允许字符串形式，转换层缺失会导致 400，见 bugs-anthropic-string-content-input-empty）。

### 1.2b 协议层验证（官方 SDK 验证）

curl 只验证 HTTP 层正确性，**官方 SDK 验证更能保证协议兼容性**（SDK 会处理 headers、content types、请求/响应格式校验）。对每种协议，使用对应官方 SDK 替换 `base_url` 和 `api_key` 为代理提供的值进行验证。

**前置**：安装 SDK（如未安装）
```bash
pip3 install openai anthropic  # Python SDK
# 或
npm install openai @anthropic-ai/sdk  # Node.js SDK
```

**OpenAI Chat Completions（Python SDK）**：
```python
from openai import OpenAI

client = OpenAI(
    base_url="http://127.0.0.1:8989/openai/v1",
    api_key="test"  # dummy key，代理会注入真实 key
)

response = client.chat.completions.create(
    model="<model-lp>",
    messages=[{"role": "user", "content": "hi"}],
    stream=False
)
print(response.choices[0].message.content)
```

**OpenAI Responses（Python SDK）**：
```python
from openai import OpenAI

client = OpenAI(
    base_url="http://127.0.0.1:8989/openai/v1",
    api_key="test"
)

response = client.responses.create(
    model="<model-lp>",
    input="hi",
    stream=False
)
print(response.output_text)
```

**Anthropic Messages（Python SDK）**：
```python
import anthropic

client = anthropic.Anthropic(
    base_url="http://127.0.0.1:8989",
    api_key="test"  # dummy key
)

# 字符串 content
message = client.messages.create(
    model="<model-lp>",
    max_tokens=100,
    messages=[{"role": "user", "content": "hi"}]
)
print(message.content[0].text)

# 数组 content（两种形式都要验证）
message = client.messages.create(
    model="<model-lp>",
    max_tokens=100,
    messages=[{"role": "user", "content": [{"type": "text", "text": "hi"}]}]
)
print(message.content[0].text)
```

**通过标准**：
- SDK 不抛异常（协议格式正确）
- 返回有效内容（转换逻辑正确）
- Anthropic 字符串/数组 content 两种形式都通过

**为什么 SDK 验证更有说服力**：
1. SDK 自动处理 headers（`Content-Type`、`anthropic-version` 等）
2. SDK 校验请求/响应格式（字段名、类型、必填项）
3. SDK 是真实用户会使用的方式
4. SDK 验证失败意味着协议层有 bug，curl 可能无法发现

### 1.2c 流式响应验证（官方 SDK）

流式响应（`stream=True`）的验证比非流式更重要——SSE 解析复杂，转换层容易出错（如丢失 tool_calls、thinking 内容、usage 信息等）。**必须用 SDK 验证流式路径**。

**OpenAI Chat Completions 流式（Python SDK）**：
```python
from openai import OpenAI

client = OpenAI(
    base_url="http://127.0.0.1:8989/openai/v1",
    api_key="test"
)

stream = client.chat.completions.create(
    model="<model-lp>",
    messages=[{"role": "user", "content": "hi"}],
    stream=True
)

collected = ""
for chunk in stream:
    if chunk.choices[0].delta.content:
        collected += chunk.choices[0].delta.content
        
print(f"Collected {len(collected)} chars")
assert len(collected) > 0, "Stream should return content"
```

**OpenAI Responses 流式（Python SDK）**：
```python
from openai import OpenAI

client = OpenAI(
    base_url="http://127.0.0.1:8989/openai/v1",
    api_key="test"
)

stream = client.responses.create(
    model="<model-lp>",
    input="hi",
    stream=True
)

collected = ""
for event in stream:
    if event.type == "response.output_text.delta":
        collected += event.delta
        
print(f"Collected {len(collected)} chars")
assert len(collected) > 0, "Stream should return content"
```

**Anthropic Messages 流式（Python SDK）**：
```python
import anthropic

client = anthropic.Anthropic(
    base_url="http://127.0.0.1:8989",
    api_key="test"
)

collected = ""
with client.messages.stream(
    model="<model-lp>",
    max_tokens=100,
    messages=[{"role": "user", "content": "hi"}]
) as stream:
    for text in stream.text_stream:
        collected += text
        
print(f"Collected {len(collected)} chars")
assert len(collected) > 0, "Stream should return content"
```

**流式 + 工具调用验证（关键场景）**：
```python
# OpenAI Chat 流式 + tool_calls
stream = client.chat.completions.create(
    model="<model-lp>",
    messages=[{"role": "user", "content": "What's the weather in NYC?"}],
    tools=[{
        "type": "function",
        "function": {
            "name": "get_weather",
            "description": "Get weather",
            "parameters": {
                "type": "object",
                "properties": {"city": {"type": "string"}},
                "required": ["city"]
            }
        }
    }],
    stream=True
)

tool_calls = []
for chunk in stream:
    delta = chunk.choices[0].delta
    if delta.tool_calls:
        for tc in delta.tool_calls:
            if tc.index >= len(tool_calls):
                tool_calls.append({"name": "", "arguments": ""})
            if tc.function.name:
                tool_calls[tc.index]["name"] = tc.function.name
            if tc.function.arguments:
                tool_calls[tc.index]["arguments"] += tc.function.arguments

print(f"Collected {len(tool_calls)} tool calls")
assert len(tool_calls) > 0, "Stream should return tool_calls"
assert tool_calls[0]["name"] == "get_weather"
```

**通过标准**：
- 流式响应正常完成（无中断、无解析错误）
- 内容完整收集（与非流式结果一致）
- 工具调用字段完整（`name`、`arguments`、`id` 不丢失）
- 流式事件顺序正确（`message.start` → `content_block.delta` → `message.stop`）

**为什么流式 SDK 验证至关重要**：
1. SSE 解析复杂——SDK 处理 chunk 边界、事件类型、数据格式
2. 转换层容易出错——tool_calls 分块传输时 `index`/`id` 容易丢失
3. 真实用户主要使用流式——非流式验证通过不代表流式正常
4. 历史 bug 多发区——`index: 0` 被 omitempty 吞掉、`thought_signature` 丢失等都在流式路径

### 1.3 force_stream 聚合场景（上游强制流式）

对于声明 `compat.force_stream = true` 的 provider（如 openai-sub/ChatGPT 订阅），上游**只接受流式**，但客户端可能请求非流式。代理必须把上游 SSE 聚合回 JSON。**该场景必须单独验证**：

```bash
# 非流式 responses（触发聚合路径）：上游强制流式 → 代理聚合回 JSON
curl -s http://127.0.0.1:8989/responses/v1/responses -H 'Content-Type: application/json' \
  -d '{"model":"<model-lp>","input":"say hi","stream":false}'
# 通过标准：200 + 标准 responses JSON（output 数组含 message/function_call，usage 非零）

# 非流式 + 工具调用（聚合必须保留 function_call）
curl -s http://127.0.0.1:8989/openai/v1/chat/completions -H 'Content-Type: application/json' \
  -d '{"model":"<model-lp>","messages":[{"role":"user","content":"call the tool"}],"stream":false,"tools":[{"type":"function","function":{"name":"get_weather","description":"Get weather","parameters":{"type":"object","properties":{"city":{"type":"string"}},"required":["city"]}}}]}'
# 通过标准：200 + message.tool_calls 完整（name/arguments 正确），无 400

# 非流式 anthropic 转换路径（聚合 → 转回 anthropic 格式）
curl -s http://127.0.0.1:8989/anthropic/v1/messages -H 'Content-Type: application/json' \
  -H 'x-api-key: test' -H 'anthropic-version: 2023-06-01' \
  -d '{"model":"<model-lp>","max_tokens":100,"messages":[{"role":"user","content":"hi"}]}'
# 通过标准：200 + content[].text 非空
```

> 聚合路径的已知范围（ADR-023）：只保留 message/function_call 两种 item，其余类型（reasoning/shell_call 等）在聚合时丢弃；流式透传路径（stream:true）全量转发不受影响。

### 1.4 launch + 真实客户端（本节核心）

对每个支持的客户端执行：

```bash
llm-proxy launch codex          # 生成/更新客户端配置
llm-proxy launch pi
llm-proxy launch qwen-code
```

然后按 §4 验证矩阵跑 L1-L4。

## 2. 接入新 Model

### 2.1 配置管理验证（`model` 命令）

```bash
llm-proxy model list                          # 新 model 出现
llm-proxy model info <model-lp>               # 能力契约正确：context_window / max_output_tokens / features / thinking 配置
llm-proxy status --refresh                    # 该 model 的 provider 绑定可达
```

### 2.2 格式族识别（关键！）

对照已有模型判断该模型的上游格式需求：

| 格式族 | 特征 | 转换差异 |
|--------|------|---------|
| **Claude-adapter**（antigravity claude 系列） | 上游模型名含 `claude` | functionCall/functionResponse 必须带 id（映射 tool_use.id / tool_result.tool_use_id） |
| **Gemini-native**（gemini 系列） | 上游模型名含 `gemini` | 不带 id；function_response.name 必须是非空函数名 |
| **标准 OpenAI**（deepseek/qwen/kimi 等直连） | 原生 Chat/Responses | 走 passthrough 或标准转换 |

> 识别错误是历史上最常踩的坑（Claude/Gemini 格式冲突导致多轮工具调用 400）。

### 2.3 launch + 真实客户端验证

按 §4 验证矩阵跑 L1-L4，**特别关注多轮工具调用**（L3/L4）——模型格式差异主要在工具调用的多轮往返中暴露。

## 3. 接入新 LLM App

### 3.1 launch 配置生成

```bash
llm-proxy launch <app>          # 生成/更新该客户端配置（指向 127.0.0.1:8989 + 协议前缀 + dummy key）
# 检查生成结果：
#   - base_url 带正确协议前缀（/openai /responses /anthropic）
#   - model_catalog / settings.json / models.json 内容正确
#   - 不泄露上游真实 key
```

### 3.2 无头模式验证

用该客户端的非交互模式发单轮 + 工具调用请求（如 codex 的 `codex exec`）。

### 3.3 TUI 交互验证（可通过 tmux 自动化）

在**具备真实终端能力的环境**中运行客户端交互模式。可通过 tmux CLI 操作实现自动化，无需人工交互。

**tmux 自动化示例**：
```bash
# 1. 创建 tmux 会话
tmux new-session -d -s test-codex

# 2. 发送命令启动 codex
tmux send-keys -t test-codex "cd /path/to/test/dir && codex" Enter

# 3. 等待初始化完成（关键！）
sleep 10  # 或捕获 model 栏变化

# 4. 发送 prompt
tmux send-keys -t test-codex "say hello" Enter

# 5. 捕获输出验证
tmux capture-pane -t test-codex -p | tail -20

# 6. 清理
tmux kill-session -t test-codex
```

**Codex 特殊注意事项**：

1. **目录信任配置**（避免初始化询问）：
   ```bash
   # 方法 1：全局信任所有目录（开发环境推荐）
   echo '{"trust_level": "full"}' > ~/.codex/config.json
   
   # 方法 2：信任特定目录
   cd /path/to/test/dir
   codex  # 首次会询问，选择 "Always trust"
   
   # 方法 3：通过环境变量跳过
   export CODEX_TRUST_DIR=1  # 如支持
   ```
   
   不配置的话，codex 启动时会询问 "Do you trust this directory?"，阻塞自动化流程。

2. **初始化等待**（关键！）：
   - codex 启动后需要 5-15 秒初始化（加载模型、建立连接）
   - 初始化期间 model 栏显示 `loading`，**不响应输入**
   - 必须等待 model 栏显示实际模型名后再发送 prompt
   - 过早发送会导致 prompt 被忽略或丢失
   
   **检测方法**：
   ```bash
   # 等待 model 栏变化（简化版）
   sleep 10  # 保守等待
   
   # 或捕获 pane 内容检查
   until tmux capture-pane -t test-codex -p | grep -q "deepseek\|gpt\|claude"; do
     sleep 1
   done
   ```

**通用要点**：
1. **准备环境**：确保验证环境中已安装目标客户端（codex/pi/qwen-code）且二进制在 PATH 中可执行；客户端配置指向被验证的代理实例（`launch` 生成或手动配置均可）
2. **等初始化完成**：model 栏从 `loading` 变为实际模型名后再发 prompt（过早发送会被忽略）
3. 依次验证：单轮 prompt → 多轮追问 → 工具调用任务（如"列出并介绍当前目录"）
4. **客观证据**：客户端有响应 + `Token usage` 统计 + 代理侧 `admin/usage` 有该 provider 记录
5. 若 TUI 直连官方（日志 `pooling idle connection for ("https", chatgpt.com)`），检查是否运行在持久交互式终端内（非交互环境导致）

> 验证环境准备要求：目标客户端已安装且可执行、配置指向代理、有持久交互式终端。具体如何准备（用哪台机器、哪个 hostname、如何配置 PATH）由测试者自行解决。

## 4. 验证矩阵（L1-L8）

| 层 | 场景 | 命令 | 通过标准 | 自动/人工 |
|----|------|------|---------|----------|
| L1 | 冒烟：构建+单轮 | `cargo test` → `codex exec "say hello"` | 有响应文本，tokens > 0 | 自动 |
| L2 | 协议：三入站 | curl Chat/Responses/Anthropic | 均 200 + 有效内容 | 自动 |
| L3 | 工具往返 | `codex exec "list files and count"` | functionCall→functionResponse→最终文本，无 400 | 自动 |
| L4 | 多轮工具 | `codex exec "introduce this repo"`（ls→read→analyze） | 2-3 轮工具调用全通过，上下文正确 | 自动 |
| L5 | 流式 | curl `stream:true` | delta 拼接完整、usage 非零 | 自动 |
| L5a | force_stream 聚合 | §1.2a 三 curl（responses/chat+工具/anthropic 非流式） | 聚合 JSON 完整（message/function_call 保留）、tool_calls 正确、usage 非零 | 自动 |
| L6 | 客户端 TUI 交互 | tmux 自动化或真实终端交互模式 | 初始化后 prompt→响应→多轮→工具调用 | 自动/人工 |
| L7 | 配置管理 CLI | `provider/model/status` 命令 | §1.1/§2.1 各命令输出正确 | 自动 |
| L8 | TUI 写入与 delegation | §4.1 connect TUI + §4.2 provider TUI | 见下方详细场景 | 自动/人工 |

> L1-L5 + L7 是接入的自动化底线；L6 可通过 tmux 自动化，也是发布/交付前的最终确认；L8 覆盖 TUI 写入路径和 Server delegation。

### 4.1 L8a — connect TUI 场景

connect TUI 是配置向导，覆盖产品选择、env key 选择、模型选择、OAuth 登录等流程。

**前置条件**：
- 隔离环境（`LLM_PROXY_STATE_DIR=<tmp>` + `--config <tmp>/config.toml`）
- 或真实环境（注意会修改真实配置）

| # | 场景 | 操作步骤 | 预期结果 | 备注 |
|---|------|---------|---------|------|
| C1 | API Key 产品（无 Server） | `connect` → 选 DeepSeek → 选 env → 选模型 → 验证 | 配置写入成功，Done 页面显示 | 基本流程 |
| C2 | API Key 产品（有 Server） | 启动 serve → `connect` → 选产品 → 选 env → 选模型 | UDS 委托写入成功 | 验证 delegation |
| C3 | OAuth 产品（OpenAI） | `connect` → 选 openai-sub → 显示设备码 → 浏览器授权 | Token 写入 OAuth store | 需真实浏览器 |
| C4 | OAuth 产品（Antigravity） | `connect` → 选 google-antigravity → 显示 URL → 粘贴 code | Token 写入 | 需真实浏览器 |
| C5 | 自定义 Provider | `connect` → 选 Custom → 填写名称/URL/key → 验证 | 自定义配置正确写入 | |
| C6 | 返回/取消 | 各步骤按 Esc/返回 | 状态正确回退，不写残留配置 | |
| C7 | OAuth 覆盖 | 已有 OAuth 账号 → 重新 connect 同产品 → 覆盖确认 | 覆盖/取消逻辑正确 | |
| C8 | 无 env 可选 | 选产品后无任何 env 在环境中 | 明确提示，不崩溃 | 边界场景 |

**tmux 自动化示例**：
```bash
# 隔离环境
export TMP_DIR=$(mktemp -d)
export LLM_PROXY_STATE_DIR="$TMP_DIR"
mkdir -p "$TMP_DIR"
cp ~/.config/llm-proxy/config.toml "$TMP_DIR/config.toml"

# 创建 tmux 会话
tmux new-session -d -s test-connect

# 启动 connect（隔离环境）
tmux send-keys -t test-connect "LLM_PROXY_STATE_DIR=$TMP_DIR llm-proxy --config $TMP_DIR/config.toml connect" Enter
sleep 2

# 捕获产品选择界面
tmux capture-pane -t test-connect -p | tail -30

# 选择产品（上下箭头 + Enter）
tmux send-keys -t test-connect Down Down Enter
sleep 1

# 捕获 env 选择界面
tmux capture-pane -t test-connect -p | tail -20

# 选择 env + 模型 → 完成
tmux send-keys -t test-connect Enter
sleep 1
tmux send-keys -t test-connect Enter
sleep 3

# 捕获结果
tmux capture-pane -t test-connect -p | tail -20

# 清理
tmux kill-session -t test-connect
rm -rf "$TMP_DIR"
```

### 4.2 L8b — provider TUI 管理面板场景

`llm-proxy provider`（无子命令）进入 Provider 管理面板。

| # | 场景 | 操作 | 预期结果 | 备注 |
|---|------|------|---------|------|
| M1 | 列表浏览 | 进入面板 | 显示 provider 列表（名称/产品/状态/协议） | |
| M2 | 搜索过滤 | `/` + 输入关键词 | 实时过滤 | |
| M3 | 查看详情 | `Enter` 选中 provider | 显示完整配置 | |
| M4 | 添加 provider | `a` → 进入 connect 流程 | 同 C1-C5 | |
| M5 | 删除 provider | `d` → 确认对话框 | 有模型引用时显示警告 | |
| M6 | 删除有引用的 provider | 删除被 model 引用的 provider → 强制确认 | 警告后仍可删除 | |
| M7 | OAuth 登录 | `l` 选中 OAuth provider | 启动 OAuth 流程 | |
| M8 | 刷新 | `r` | 状态更新 | |
| M9 | Server 运行时删除 | serve 运行中 → `d` 删除 | UDS 委托成功 | **当前 bug #5 场景** |
| M10 | Server 运行时编辑 | serve 运行中 → 编辑自定义 provider | UDS 委托成功 | **当前 bug #5 场景** |

### 4.3 L8c — Server delegation 边缘场景

| # | 场景 | 预期行为 | 当前状态 |
|---|------|---------|---------|
| D1 | 无 Server + TUI 写入 | 本地 flock，直接写 | ✅ 正常 |
| D2 | 有 Server + detect 成功 | UDS 委托写入 | ✅ 正常 |
| D3 | 有 Server + detect 失败 | 重试或明确报错 | ❌ bug #5（报"写入权被占用"） |
| D4 | Server 刚启动（TCP 未就绪） | 等待或重试 | 待验证 |
| D5 | CLI 写入 + Server 持有 flock | HeldByServer 重试 → 委托 | ✅ 已实现 |

### 4.4 TUI 测试记录（2026-08-07）

**测试环境**：llm-proxy 0.2.0 (ec3768f)，tmux 自动化

**已验证场景**：

| # | 场景 | 结果 | 备注 |
|---|------|------|------|
| C1 | connect TUI — API Key 产品（无 Server） | ✅ 配置写入成功 | 基本流程正常 |
| C2 | connect TUI — 搜索过滤 | ✅ 模糊匹配工作 | "DPK" 匹配到 DEEPSEEK_API_KEY |
| M1 | provider TUI — 列表浏览 | ✅ 正常显示 | |
| M2 | provider TUI — 搜索过滤 | ✅ 实时过滤 | |

**发现的问题**：

| # | 问题 | 严重度 | TODO 编号 |
|---|------|--------|----------|
| 1 | WarningConfirm "Continue anyway" 无限循环 | **High** | #6 |
| 2 | TUI 搜索不支持 Ctrl+N/P | Medium | #7 |
| 3 | connect TUI 写入权冲突（Server 运行时） | **High** | #5 |

**待验证场景**（需要真实 OAuth 流程或特定环境）：
- C3/C4: OAuth 产品登录（需浏览器交互）
- C5: 自定义 Provider 编辑
- M9/M10: Server 运行时 TUI 写入（需复现 bug #5）

## 5. 回归规则

1. **修复 bug 后**：必须用最初触发 bug 的相同场景复现验证（`codex exec` 或 curl），确认问题消失；涉及 antigravity 模型时加跑 L4。
2. **每修改转换逻辑**：重跑 L1 + L3；涉及多轮格式时加跑 L4 + 已有模型（防止 Claude/Gemini 互相回归）。
3. **验证未结束不宣布完成**：验证命令运行中不得提前回复结果。

## 6. 记录模板

接入验证完成后，在接入文档（如 issue 或模型配置说明）追加：

```markdown
## 接入验证记录（<日期>）
- proxy commit: <hash>
- provider/model/app: <名称>
- 场景结果：
  | 场景 | 结果 | 证据（prompt→输出/tokens/错误） |
  |------|------|-------------------------------|
  | L1 冒烟 | ✅/❌ | ... |
  | L3 工具往返 | ✅/❌ | ... |
  | L4 多轮工具 | ✅/❌ | ... |
  | L6 TUI 交互 | ✅/❌ | ... |
  | provider/model/status | ✅/❌ | ... |
- 已知限制: ...
```
