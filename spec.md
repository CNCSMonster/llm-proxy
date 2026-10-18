# LLM Proxy 方案文档（公开版）

状态：公开面契约
对应发布：v0.2.0
定位：**公开版**——足以重新实现 pub 版本；零内部引用
来源：`spec.md`（私有完整版）的裁剪投影，只删不造

本文档描述 llm-proxy 的行为契约与协议语义公开面。

## 1. 目标

llm-proxy 保持其产品目标不变：为编码智能体（coding agents）暴露稳定的本地端点，同时隐藏上游 API 密钥与各提供方特有的协议差异。

### 配置设计原则

1. **默认简单、显式可配置。** 普通用户只需选择一个模型服务 Product，即可获得一套完整的推荐 Provider，无需理解端点拓扑、协议转换、认证接线、兼容性怪癖（quirks）、探测或速率治理。高级用户通过显式配置表达多账号、区域访问或回退（fallback）策略。简单是默认的设置路径，而不是对高级配置的限制。

2. **模型选择对 Product 可见。** 能力在 Model 层声明，但由目录（catalog）生成的模型 ID 同时也标识获取该能力的服务 Product。当同一个上游模型可通过不同 Product 获得时，目录默认值会生成彼此不同的客户端可见 Model ID，而不是静默合并它们。Product 身份保持可见，因为计费、配额（quota）、速率限制、隐私/数据训练政策、服务条款与账号要求可能不同。默认回退只在同一 Product 类内组合 Provider；跨 Product 回退（包括同一上游模型的订阅制转按量付费回退）要求高级用户在绑定顺序中显式配置。

3. **Provider 是治理单元；端点表达协议可达性。** Provider 是高级路由与运行时治理的最小单元：认证引用、请求频率池、探测调度、冷却（cooldown）、健康状态与回退身份。其端点只声明经由同一 Provider 可达的原生或派生协议；它们不是独立账号、回退候选或治理池。

4. **端点协议是受控扩展点。** 客户端协议是稳定的本地契约。对于线格式（wire format）独特的服务，项目可以注册一个仅上游使用的原生协议，再从它派生所支持的客户端协议。此类扩展需要协议代码、适配器（adapters）、校验、提供方调研与测试；用户 TOML 不能凭空发明任意端点名。

5. **证据驱动配置。** 目录默认值、模型能力元数据、端点兼容行为与产品特定政策，必须有当前的提供方调研、官方文档、真实 API 探测或用户显式覆盖作为支撑。未知事实保持不宣传或需选择启用（opt-in）；模型名、相似的 API、输出形态和第三方目录都不构成充分证据。StepFun 的 `deepseek-style` 响应格式不得被视为其接受 DeepSeek 思考控制（thinking-control）请求字段的证据。

6. **显式优于推断。** 用户可表达的策略必须来自显式配置。代理不得因为 Product、API、模型名或输出形态看起来相似，就推断回退、合并或拆分 Provider、在凭证之间做选择，或假定共享配额/政策。跨 Product 回退、多 Provider 主备路由与凭证共享都是用户的显式选择。

7. **确定性本地错误尽早拒绝；保留协议错误语义。** llm-proxy 能在本地判定的无效配置与请求错误，必须在发起任何上游请求之前被拒绝。对于上游或转换错误，llm-proxy 必须保留客户端协议所能表达的每一个状态码、类别、消息、请求 ID、重试提示及其他可操作细节，返回该协议的错误形态，而不是把它压扁成通用代理错误。跨协议逐字节保留是不可能的；要求是语义保留。

8. **目录只建议；用户配置才是持久权威。** 目录更新只为将来的 Product 选择提供候选，绝不静默覆盖、删除或重接已存在的用户 Model 或 Provider。用户显式地重新进入已配置的 Product，从其当前目录模板中选择增删。该操作只改动所选 Product 的绑定；它保留其他 Product 的绑定以及每一个用户编辑过的 Model 字段，除非用户显式修改它们。

配置的归属边界：

- **Model** 描述客户端可见 ID；对目录默认值而言，它同时标识模型能力与所选服务 Product；它们声明能力元数据，并对每个客户端协议声明有序的 Provider 绑定与同 Product 回退顺序。
- **Provider** 描述一条具体的产品访问路径及其共享的运行时治理，包括该访问路径如何提供各协议端点。
- **协议转换声明在 provider 端点之下**，而不是从 model-provider 绑定中隐式推断。

这使得「模型 X 能否对协议 P 使用 provider Y？」成为一个配置校验问题：

```text
model.<id>.<protocol>_providers 包含 provider Y
        ↓
provider Y 必须为协议 P 声明一个端点
        ↓
该端点可以是原生的，或经适配器从另一端点派生
```

## 2. 核心术语

### 客户端协议（client protocol）

本地客户端对 llm-proxy 说的协议。恰好三种：

- `openai-chat`
- `openai-responses`
- `anthropic`

### Provider 生命周期与治理边界

`Provider` 是用户配置的、针对一条产品访问路径的访问与运行时治理边界。它可以包含多个协议端点；协议数量不决定 provider 数量。

内置目录、`connect` 与 `provider add` 必须推荐**每条独立治理的产品访问路径对应一个 Provider**。该推荐 Provider 包含该产品可用的原生/派生端点，并在所有绑定的模型与客户端协议之间共享其认证引用、请求频率限制器、探测调度、冷却与健康状态。

schema 允许高级用户为同一 Product 配置多个 Provider，例如主号与备号。这是一种显式路由选择：独立的 Provider 有意拥有各自的认证引用、探测调度、限制器、冷却、健康状态，并可能在模型回退绑定中分别出现。代理不得仅因为存在多个凭证，就把一个 Product 自动拆分为多个 Provider。

Platform 与 Product 是事实，属于目录/调研输入，而不是 TOML 表层：**Platform** 是独立运营的模型服务系统（账号、购买/订阅、密钥作用域、配额、网络区域、条款或故障边界）；**Product** 是 Platform 中区分计费、权益、配额或使用政策的单元。一家公司可以运营多个 Platform。当产品模型如此定义时，区域计算集群可以是不同的 Platform。MiMo 的 CN、SGP、AMS 集群被视为彼此独立的 Platform；它们的按量付费（PAYG）与 Token Plan 产品是各 Platform 内相互独立的 Product。

### Provider 认证（auth）

当前 Rust 实现保持 Go/v1 的凭证归属边界，同时让用户侧 provider 配置更规整：

- `config.toml` 只存储 provider 的认证模式与凭证/账号引用。
- API key 型 provider 引用环境变量；生成的配置应优先使用环境变量名而非明文密钥。
- OAuth refresh/access token 永不存入 `config.toml`。
- 状态/日志/诊断输出绝不能打印 API key、bearer token、OAuth refresh token、device code 或原始认证头。

精简的 TOML 形态：

```toml
[providers.deepseek]
api_key_env = "DEEPSEEK_API_KEY"

[providers.openai-subscription]
auth = { type = "openai_oauth", account = "openai-subscription" }

[providers.ollama]
auth = { type = "none" }
```

`api_key_env = "ENV_NAME"` 是 `auth = { type = "api_key_env", env = "ENV_NAME" }` 的语法糖。配置规范化会把两种形式转成同一个运行时枚举。除非 `api_key_env` 与 `[auth]` / 内联 `auth = {...}` 描述的是完全相同的 API key 环境变量引用，否则 provider 不得同时声明二者；对 API key 型产品，推荐的生成形式仍是 `api_key_env`，因为它更短且符合既有用户预期。

### Provider 端点（endpoint）

由 provider 声明的能力。它表示该 provider 可以直接或经转换服务一种协议。

Provider 以按协议命名的直接可选字段声明所支持的协议（如 `openai_chat`、`openai_responses`、`anthropic`）。字段存在即表示 provider 支持该协议。不存在 `endpoints` 映射层，端点内也没有 `protocol` 字段；字段名本身就是协议标识符，这在结构上保证了每个 provider 内的唯一性。

**端点协议是受控扩展点。** 三种客户端协议是稳定的本地契约。当某个服务具有必须被忠实表达的独特线格式时，provider 可以额外声明一个已注册的仅上游使用的原生协议。添加此类协议需要代码级的 `Protocol` 变体、原生请求/响应处理、任何必需的适配器、配置校验、提供方调研证据与测试；用户 TOML 不能凭空发明任意端点名。这把特殊协议的复杂性留在目录/provider/适配器之内，普通用户只需选择服务产品。

一个端点恰好有且只有两个互斥字段之一，这也决定了它的种类——不存在单独的 `kind` 字段：

- `url`：完整的上游 URL。存在即表示**原生端点（native endpoint）**（不做 base/path 拼接）。
- `derive_from`：同级协议字段名（如 `openai_chat`）。存在即表示**派生端点（derived endpoint）**。

可选字段（**仅原生端点**）：

- `compat`：一个可选子表，声明上游 API 的兼容性怪癖，用于驱动不对称的出站行为（见 §5.1）。缺省时应用默认 compat 行为（标准 OpenAI 兼容假设；见 §5.1）。灵感来自 pi 的 provider 级 `compat` 对象，但按协议端点划定作用域，因为怪癖属于该端点背后的具体上游 API，同一 provider 的两个端点可能不同。

派生端点不得声明 `compat`：派生端点的出站行为在其源链的原生端点处执行，因此它从该原生端点继承 compat。

不存在能力开关（`streaming`、`tools` 等）。所有适配器同时支持非流式与流式，运行时也没有任何逻辑以这类开关为闸——声明它们不会驱动任何行为。如果将来某个上游确实缺少流式或工具支持、且运行时学会了利用这一事实，届时再加该开关即可（添加可选字段是向后兼容的）。

不存在 `adapter` 字段。转换语义由（源协议，目标协议）二元组唯一决定，因此派生端点只声明 `derive_from`。provider 特有的转换差异是数据——由该二元组的单一适配器消费的 `compat` 字段——而不是不同的适配器实现。

### 原生端点（native endpoint）

由同协议的真实上游 API URL 支撑的 provider 端点。

示例：DeepSeek 原生 Chat Completions 端点，并声明其上游怪癖。

```toml
[providers.deepseek]
api_key_env = "DEEPSEEK_API_KEY"

[providers.deepseek.openai_chat]
url = "https://api.deepseek.com/chat/completions"

[providers.deepseek.openai_chat.compat]
supports_developer_role = false
supports_reasoning_effort = true
thinking_format = "deepseek"
requires_reasoning_content_on_assistant_messages = true
max_tokens_field = "max_tokens"
```

### 派生端点（derived endpoint）

由同一 provider 上的另一端点加协议适配器支撑的 provider 端点。

示例：某 provider 只有 Chat Completions 上游，但 llm-proxy 可以通过把 Anthropic Messages 请求转换为 Chat Completions 请求，暴露一个 Anthropic 兼容的 provider 端点。

```toml
[providers.provider-a]
api_key_env = "PROVIDER_A_API_KEY"

# 可选的高级覆盖。省略时，llm-proxy 应用保守的 provider 级默认值，
# 后续的 provider 调研可能细化这些内置值。
[providers.provider-a.request_frequency]
requests_per_minute = 60
# requests_per_hour = 1000
burst = 5
queue_timeout_seconds = 10

[providers.provider-a.openai_chat]
url = "https://api.example.com/v1/chat/completions"

[providers.provider-a.anthropic]
derive_from = "openai_chat"
```

有此声明后，即使上游服务没有原生 Anthropic API，`provider-a` 也允许出现在 Anthropic provider 列表中。`anthropic ← openai_chat` 二元组明确选中 `anthropic-from-chat-completions` 适配器。

## 3. 期望的配置形态（Desired Config Shape）

目标 llm-proxy 用户侧 schema 采用模型级、按协议划分的 provider 绑定列表，而转换细节住在 provider 里。

示意 TOML：

```toml
[server]
listen = "127.0.0.1:8989"
# `listen` 同时是绑定地址与客户端可达地址（`launch` 用它推导 base_url），
# 因此端口不得为 0：端口 0 让内核挑一个随机端口，而该端口不向任何客户端公布。
# 违反时 `llm-proxy` / `serve --foreground` / `restart` 在产生任何副作用前直接拒绝
# 该校验刻意不放进 `Config::validate()`，以免连带击穿合法使用
# 临时端口的配置编辑路径与单测夹具。
# 聚合器内存保护（可选）：SSE 缓冲上限（字节）与 output item 数量上限，
# 防止恶意/挂死上游导致内存无界增长。不配置时用默认值 64MB / 4096。
# max_sse_buffer_bytes = 67108864
# max_output_items = 4096

[providers.provider-a]
api_key_env = "PROVIDER_A_API_KEY"

[providers.provider-a.openai_chat]
url = "https://api.example.com/v1/chat/completions"

[providers.provider-a.anthropic]
derive_from = "openai_chat"

[fallback]
max_retries = 2
server_error_retries = 2
server_error_retry_interval_ms = 200
timeout_seconds = 300  # 5 分钟，LLM 深度推理常态可达数分钟
max_timeout_seconds = 600

[fallback.cooldown]
network_seconds = 30
server_error_seconds = 300
rate_limit_seconds = 300
model_unavailable_seconds = 1800
client_error_seconds = 300
auth_error_seconds = 300  # 402 额度/账单；401/403 不写冷却

[protection.bad_request]
enabled = true
window_seconds = 300
max_errors = 2
block_seconds = 300

[protection.error_cooldown]
enabled = true
max_errors_per_class = 3
cooldown_seconds = 300
window_seconds = 600

[protection.local_reject]
enabled = true
max_local_rejects = 10
cooldown_seconds = 300
window_seconds = 600

[protection.bad_request_budget]
enabled = true
daily_limit = 2
lifetime_limit = 10

# 针对缺失 Responses 历史的可选占位符。
# 默认 false，保持对缺失 function_call_output 或孤儿 reasoning 的严格 400 拒绝。
# 为 true 时，缺失的 output 以占位内容替换、孤儿 reasoning 被丢弃，请求得以继续。
placeholder_on_missing_history = false

[launch]
# 可选：控制向 AI 编码客户端导出的模型范围（三级交集过滤，详见 §13.0）
include_models = ["gemini-3.8-flash-256k-lp", "mimo-v2.6-flash-256k-lp"]

[diagnostics]
enabled = true
capture = "metadata"        # "metadata"（默认，只记特征元数据）| "full"（额外落 egress body，截断封顶）
max_record_bytes = 65536    # full 模式单条 body 截断上限
file_max_bytes = 2097152    # diagnostics.jsonl 总量封顶（超限保留最新一半重写）

[models.model-a]
description = "Model A via provider-a"
context_window = 200000
max_output_tokens = 8192
anthropic_providers = [
  { name = "provider-a", model = "upstream-model-a" },
]
openai_chat_providers = [
  { name = "provider-a", model = "upstream-model-a" },
]
```

重要语义：

- `models.model-a.anthropic_providers` 表示 `model-a` 可以通过这些 provider 绑定服务 Anthropic 客户端请求。
- `anthropic_providers` 中的每一个绑定都必须引用一个声明了 `anthropic` 端点的 provider。
- 该 provider 的 `anthropic` 端点可以是：
  - 原生 Anthropic 上游，或
  - 从另一源端点派生的 Anthropic 端点（适配器由协议二元组唯一决定）。
- 模型绑定不指定转换细节。它们只选择 provider 名与上游模型 ID。

**仅客户端模式**：用 `[remote]` 节代替 `[server]` 节，把 CLI 配置为连接远程服务器。`[server]` 与 `[remote]` 互斥。仅客户端模式下没有本地服务器；`launch` 生成指向远程 URL 的客户端配置，`status` 通过 HTTP 探测远程服务器。详见 §13.0。

- 运行时策略节 `[fallback]`、`[fallback.cooldown]`、`[protection.bad_request]`、`[protection.error_cooldown]`、`[protection.local_reject]`、`[protection.bad_request_budget]`、`[protection]`（顶层 `placeholder_on_missing_history`）与 `[diagnostics]` 使用上表所示的名称、字段与默认值（语义见 §10.5.5、§10.5.6、§5.1）。它们是全局运行时旋钮，不是 per-provider/per-model 设置；若出现真实需求，可后续以可选字段加入 per-provider 覆盖。`[diagnostics]` 写入 `state_dir/diagnostics.jsonl`；capture 语义见 §5.1。
- `[providers.<id>.request_frequency]` 则不同：它有意限定在 provider 作用域，因为请求频率通常由上游产品/账号/区域池执行。整块可选；省略的 provider 仍获得保守的内置默认值。

### 3.1 Token 配置格式（tokens.toml 或 tokens/*.toml）

客户端认证 token 与配额配置存放于二者之一：
1. **目录模式（推荐）**：配置目录内的 `tokens/*.toml`，每个文件定义单个 token（如 `tokens/alice.toml`）。
2. **单文件模式（遗留）**：配置目录内的 `tokens.toml`，包含一个 `[[tokens]]` 数组。

**互斥**：`tokens/` 目录与 `tokens.toml` 互斥。若二者同时存在，代理启动与配置重载将以错误快速失败（fail fast）。

每个 token 配置定义：
```toml
key = "lp-xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"
name = "alice"
allowed_models = ["claude-sonnet-4", "gpt-4o", "deepseek-v3"]
expired_at = "2026-12-31T23:59:59Z"

# Token 级遗留配额窗口（可选）
[[quota_windows]]
window = "30d"
quota_mode = "tokens"
quota_limit = 50000000

# 模型作用域配额组
[[model_quota_groups]]
models = ["claude-sonnet-4", "gpt-4o"]

  [[model_quota_groups.quota_windows]]
  window = "1d"
  quota_mode = "requests"
  quota_limit = 100

  [[model_quota_groups.quota_windows]]
  window = "7d"
  quota_mode = "tokens"
  quota_limit = 10000000
```

## 4. 校验规则（Validation Rules）

校验必须在服务器启动前拒绝有歧义或不可能的配置。

### 4.1 Provider 校验

对每一个 provider：

1. Provider ID/名称必须唯一。
2. 至少存在一个协议端点字段。
3. 协议端点字段名必须来自受支持协议列表：客户端协议 `openai_chat`、`openai_responses`、`anthropic`。字段唯一性是结构性的——TOML 字段名在表内唯一，因此同一 provider 上不可能有两个端点声称同一协议。
4. 每个端点必须恰好配置 `url`（原生）与 `derive_from`（派生）之一。两者都配或两者都不配必须校验失败。
5. 原生端点（存在 `url`）：
   - `url` 必须是绝对且有效的；
   - 可选的 `compat` 子表只允许出现在这里（派生端点从其源链继承 compat）。
6. 派生端点（存在 `derive_from`）：
   - `derive_from` 必须引用同一 provider 上存在的协议字段；
   - 被引用的端点必须是**原生的**——不允许多跳派生链（派生于派生）。这使无环性与“可解析到具有具体 URL 的原生端点”成为结构性保证，而非校验规则；
   - 适配器注册表必须包含源协议 → 目标协议二元组的实现；
   - 不得配置 `compat`（从被引用的原生端点继承）——校验必须拒绝它。
7. Provider 认证必须规范化为恰好一个运行时 `AuthConfig`：
   - `api_key_env = "ENV"` 作为简写的 API key 形式被接受；
   - `auth.type = "api_key_env"` 要求非空的 `env`；
   - `auth.type = "openai_oauth"` 以 `auth.account` 作为 OAuth 存储键；省略时 `account` 默认为 provider ID；
   - `auth.type = "none"` 不得携带 `env` 或 `account`；
   - 未知认证类型以带字段路径的错误使启动校验失败；
   - `api_key_env` 与 `auth` 不得同时存在，除非它们描述同一个 API key 环境变量引用；生成的配置不应同时输出二者。
8. OAuth 配置校验**不**要求已存在 token 存储账号。缺失、过期或刷新失败的 OAuth 凭证是 `status` / `provider info` 展示的认证状态，在请求时处理，而非无效的 provider 配置。若 token 存储中存在 `auth.account` 的记录，其 `kind` 必须与 provider 认证类型匹配才能被使用。

### 4.2 Model 校验

对每一个模型与每一个按协议划分的 provider 列表：

1. 每个 provider 绑定必须包含 provider `name` 与上游 `model`。
2. 同一协议列表内 provider 名不得重复。
3. 被引用的 provider 必须存在。
4. 被引用的 provider 必须声明协议与该模型 provider 列表协议相匹配的端点。
5. 若 provider 端点是派生的，上述 provider 端点校验必须能证明它可解析到原生源端点。
6. 若声明了 `reasoning_levels`：条目必须非空；每个 `level` 必须唯一且不得为 `"adaptive"`（那不是 effort 值）；`api_value` 不得为 null 或空字符串。`default_reasoning_level`（若设置）必须是已声明的 `level` 之一。空表 `reasoning_levels = []` 是加载错误。
7. 旧键 `supported_reasoning_levels` 与 `reasoning_level_map` 不得与 `reasoning_levels` 并列或取而代之；它们的出现是加载错误。
8. **思考控制字段校验**（字段语义见 §5.2）：
   - `supports_thinking`、`force_think`、`force_no_think`、`default_think` 是 Model 专属字段；Provider 不得声明它们。
   - `force_think = true` 与 `force_no_think = true` 同时为真是配置错误。
   - `force_think = true` 配 `default_think = false` 是配置错误（force 覆盖 default；矛盾）。
   - `force_no_think = true` 配 `default_think = true` 是配置错误。
   - `force_no_think = true` 配任何生效的 `default_reasoning_level` 是**合法**的（两个独立旋钮：强制关闭控制开关，默认档位控制强度）。
   - `supports_thinking = false` 配 `force_think = true`、`default_think = true`、任何生效的 `default_reasoning_level`、或非空 `reasoning_levels` 是配置错误（能力与策略矛盾）。
   - `supports_thinking = true` 配 `force_no_think = true` 是**合法**的（部署策略覆盖能力，例如成本/延迟优化）。
   - `chat_reasoning_off_format` 必须来自运行时白名单（当前仅 `"enable-thinking"`）；未知值校验失败。
9. **路由模型（routing model）校验**。带 `reasoning_routes` 表的 Model 是**路由模型**：它自身不携带执行行为，而是按思考档位（reasoning level）转发给其他 Model。该校验在通用的逐模型执行层校验**之前**运行，因此落在某一跳上的字段会报告路由规则本身，而不是该跳无法遵循的建议。规则：
   - 显式空表（`reasoning_routes = []`）是加载错误。它不是“声明非路由模型”的方式——应省略该字段。“不是路由模型”与“是没有任何路由的路由模型”不得混淆。
   - 每个 `level` 在模型内必须唯一；每个 `target` 必须指向 `models` 中存在的 Model——未知 target 是加载错误。
   - 路由模型不得声明任何协议 provider 列表（`openai_chat_providers`、`openai_responses_providers`、`anthropic_providers`）——路由模型只转发，不执行。
   - 路由模型不得声明任何执行层字段：`reasoning_levels`、`default_reasoning_level`、`features`、`cooldown`、`supports_thinking`、`force_think`、`force_no_think`、`default_think`、`enable_thinking`、`chat_reasoning_off_format` 或 `codex_service_tier`。这些字段要么在请求时从解析出的目标读取，要么为客户端派生（见下），在跳上声明它们会被静默忽略——因此改为加载错误。
   - 路由模型不得声明非零的 `context_window` 或 `max_output_tokens`。客户端可见窗口从目标派生（下面的目标能力一致性保证它们一致）；在跳上声明的值会与之矛盾。
   - 路由是**单跳**：每个 `target` 必须指向常规（可执行）Model，绝不能是另一个路由模型——一跳转发给一跳是加载错误。这使环在构造上不可能。理由：唯一的路由键是思考档位，而档位穿过一跳时保持不变，因此任何跳链都坍缩为一张 `level → target` 映射、不增加表达力；而链式转发恰恰是引入不完整链与协议撕裂（protocol tear）故障模式的温床。禁止它就直接移除了那几类故障。
   - **目标能力一致性**：路由模型 `reasoning_routes` 引用的所有可执行目标模型必须声明完全相同的客户端可见能力：
     - `context_window` 与 `max_output_tokens` 在所有目标间必须严格一致。
     - `features`（模态、工具使用等）在所有目标间必须严格一致。
     - 暴露的客户端协议（`openai_chat`、`openai_responses`、`anthropic`）在所有目标间必须严格一致。
     - `codex_service_tier` 在所有目标间必须严格一致。
     这些客户端面向能力中任何不一致都表明配置矛盾，在加载时校验失败。只有执行层细节（provider 绑定、上游模型 ID、定价/成本指标、冷却设置）与内部的目标思考档位映射可以在目标间不同。

**路由模型的客户端可见元数据。** 路由模型是一跳，不是执行者，因此它无法从自身字段回答“我是什么”。客户端可见的答案对其路由目标求得（单跳，因此恰好就是路由所指的那些 Model）：

| 客户端可见属性 | 路由模型的派生方式 |
|---|---|
| 上下文窗口 / 最大输出 token | 各目标间一致的窗口（由校验保证一致）。 |
| `features` | 各目标间一致的 features（由校验保证一致）。 |
| 思考档位词汇 | 恰好是 `reasoning_routes` 的 `level` 键。一跳没有自己的 `api_value`：每个目标把收到的档位映射为自己的上游值。 |
| 协议支持 | 各目标间一致服务的协议（由校验保证一致）。 |
| `codex_service_tier` | 各目标间一致的 tier 声明（由校验保证一致）。 |

HTTP `/v1/models` 列表（chat、Responses、Anthropic、claude-desktop）、`model list` / `model info`、`status`、TUI 模型管理、launch 生成的客户端配置与 Codex 模型目录都取用这些派生值，因此每个客户端面向面在“这一跳提供什么”上都一致。请求时执行*同一条*规则，因此一跳恰好在其广而告之的协议上可达——不存在“客户端发现不了、但猜中某个恰好由其服务的档位就能驱动”的协议。

无效配置示例：

```toml
[models.model-a]
anthropic_providers = [
  { name = "provider-a", model = "upstream-model-a" },
]
```

若 `provider-a` 只声明了 `openai_chat` 而没有 `anthropic` 端点字段，校验必须失败。仅凭 llm-proxy 理论上知道某种转换是不够的。provider 必须显式声明它提供 Anthropic 端点。

### 4.2.1 跨 Product 绑定（Cross-product binding）

Model 的 provider 绑定可以混用来自不同 Product 的 provider。Product 是计费与治理边界（账单、配额、速率限制、隐私/数据训练政策、服务条款、账号要求）——不是能力边界。

前提是**能力等价**：所有绑定必须能交付等价的模型能力（上下文窗口、输入模态、思考档位、最大输出 token）。在架构上，能力在 Model 层声明（`ModelConfig.context_window` / `max_output_tokens` / `features` / `reasoning_levels`）；所有绑定共享同一份声明。因此，同一 Model 下的绑定天然满足能力等价——无需额外的自动化检查。

实际风险在于**上游模型名的正确性**：不同 Product 的 provider 可能对同一模型使用不同的上游模型名。跨 Product 添加绑定时，用户必须显式指定 `--upstream-model`；系统不得静默照抄第一个绑定的上游模型名。

本条补充原则 2：“跨 Product 回退……要求高级用户在绑定顺序中显式配置”——本节定义“显式配置”的含义及其约束。

### 4.3 本地路径映射是代码，不是配置

不存在 `routes` 配置节。本地路径 → 客户端协议的映射是固定的客户端契约（见 §10.1），住在代码里而不是用户配置里。一个请求唯一需要的路由决定是模型解析（§5），而模型解析绝不回退到默认值：被请求的模型 ID 不在 `models` 中时，返回客户端协议形态的 4xx 错误，而不是静默替换。

模型级 `reasoning_routes`（§4.2 规则 9）是另一个概念：它是 Model 条目内的一个字段，按思考档位在 Model 之间转发请求。它不是顶层配置节，也不影响本地路径映射。

所有模型 provider 绑定都必须在启动时校验，无论客户端实际请求哪些模型。

## 5. 请求解析（Request Resolution）

请求时路由应遵循以下顺序：

```text
本地端点路径
  -> 客户端协议（代码内固定映射，见 §10.1）
  -> Layer 0a 入站审查：按客户端协议规范做结构校验（§5.1）
  -> 被请求的模型 ID（必填；未知模型 ID -> 4xx，不回退默认值）
  -> 路由模型解析：若被请求的模型是路由跳，先要求它服务
     请求所用协议——每个目标都必须服务它，这与发现机制
     广而告之的规则相同——否则 4xx 拒绝并列出它实际服务的
     协议（因此一跳绝不会在客户端发现不了的协议上可达，
     即使猜中某个恰好由其服务的目标档位也不行）。然后读取
     请求的思考档位并查表得目标；档位未命中 -> 4xx 并
     列出该跳可用的路由。目标永远是常规模型（路由是
     单跳，加载时强制）。只有当跳解析成功后，请求的模型
     字段才会被改写，因此失败时请求与客户端发出时完全
     一致。原始模型 ID 被记住用于下游响应。
     思考档位按协议读取：
       - Chat：先读顶层 `reasoning_effort`，回退到嵌套的
         `reasoning.effort`（qwen 0.24.7 线格式）；两者同时
         存在时顶层优先。
       - Responses：嵌套的 `reasoning.effort`。
       - Anthropic：嵌套的 `output_config.effort`。
     所有形式在路由中地位相同；下游不区分来源。
  -> 该客户端协议的模型 provider 列表（空列表 -> 4xx）
  -> 请求内容能力过滤：若请求包含图像、音频、视频或
     文档输入，而模型未声明相应的 `image_input`、
     `audio_input`、`video_input` 或 `document_input` 能力，
     则在任何 provider 选择之前以客户端协议形态的 4xx
     本地拒绝并点名缺失能力——不写冷却、不计指纹
  -> Layer 1 指纹封禁检查（§10.5.6）：被封禁的请求形态在
     联系任何 provider 之前以 429 本地拒绝
  -> 选择首个健康的 provider 绑定，遵守回退/冷却
  -> 解析该客户端协议的 provider 端点
  -> 若为派生（存在 derive_from）：应用（源，目标）协议
     二元组对应的适配器（单跳——derive_from 必须引用原生端点）
  -> Layer 0b 出站审查：按目标原生端点的 compat 校验/适配
     出站 body（§5.1）；转换 bug 本地失败，绝不转发
  -> 把请求发送到解析出的原生端点 url
  -> 上游请求前把前端模型 ID 改写为 binding.model
  -> 下游响应中把上游模型 ID 改写回前端模型 ID
```

路由模型对客户端透明：响应携带的是原始路由模型 ID，而不是解析出的目标 ID。两种身份保持分离：

- **执行身份（execution identity）** 是解析出的目标 Model。Provider 选择、回退、冷却身份、思考策略与思考档位映射、replay 身份、能力过滤与指纹都对着目标求解——因此某个路由跳 `high` 档的失败绝不会冷却它 `low` 档的兄弟，而到达上游的是目标自己的 `reasoning_levels` 映射。
- **客户端身份（client identity）** 是客户端请求的路由模型。它是下游响应上报的身份，也是 usage 记录归属的对象，与同样以被请求模型 ID 为键的 token 访问控制一致。

路由跳自身没有 provider、没有冷却、没有执行路径：它绝不会增加第二次上游尝试。路由模型不做跨档位回退决定——若解析出的目标失败，它自己的 provider 回退链跑到底，而这一跳不会去重试另一个档位。

模型解析有两种不同的失败模式，都以客户端协议错误形态显式报错：

- **未知模型**：被请求的模型 ID 根本没在 `models` 中声明 → 4xx。没有默认模型替换；launch 生成的客户端配置总是携带已知前端模型 ID，因此未知 ID 意味着客户端/代理配置偏差，用户应修正而不是掩盖。
- **协议不支持**：模型存在但没有该请求协议的 provider 绑定 → 4xx，并解释该模型实际支持哪些协议。

### 能力声明是信任契约

Model `features`（如 `image_input`、`audio_input`、`video_input`、`document_input`）只在模型层声明。刻意不设描述“上游模型是否支持某能力”的配置字段——代理无法验证上游能力，也不去尝试。在模型上声明某 feature 并绑定到 provider，意味着运维者断言被绑定的上游模型具备该能力。若绑定错了（上游模型其实没有），那是配置错误：上游会拒绝该请求，常规错误映射（§10.5.8）会把它暴露出来。白名单也接受行为类成员（如 `effort_ignored`）——它们是改变代理转换行为、而非声明上游能力的特性开关，但共享同一模型级挂载点，且对未知值大声失败（fail-loud）校验。

当一个协议绑定有多个 Provider 候选时，Model 元数据是面向每个可能收到请求的候选的客户端契约。因此 `context_window`、`max_output_tokens`、`features` 与所支持的思考语义必须是所有候选都能满足的能力——通常是保守的公共下界。例如，上下文窗口为 1M 与 256K 的绑定，只有在该 Model 声明不超过 256K 上下文时才能共享同一 Model。用户若绑定一个无此能力的 Provider 却声明更大的 Model 契约，就制造了一种高级错误配置；llm-proxy 在该情形下不承诺成功回退，而是以客户端协议错误形态暴露上游拒绝。

上面的能力过滤是这份契约的镜像：它只*阻止*模型明确不声称支持的请求，从不试图证明支持。能力过滤发生在冷却过滤之前，因此“没有绑定支持此内容”绝不会被“所有绑定都在冷却中”掩盖——两种失败原因不同、用户动作也不同。

## 5.1 请求校验层（Request Validation Layers）

llm-proxy 在两个边界校验请求，严格度刻意不对称。这是上游保护的 Layer 0，位于指纹封禁与冷却层（§10.5）之前。

**统辖原则**：本地校验只拦截*本地可检测的格式错误*——即无论上游状态如何、结构上就无效的请求。结构有效的请求总是被转发给上游，即使代理能预测它们大概率会失败（token 过期、配额耗尽、账号被限速）。服务端失败条件在上游响应之后分类处理（§10.5.8）；代理绝不在本地预判账号状态。

这里的“账号状态”指**上游账号状态**——provider 的凭证、配额或速率限制，代理只能通过询问得知。它**不**约束代理自身的**客户端 token 准入**（见下文*客户端 token 准入*）：客户端 token 属于代理，由本地配置决定、不联系任何上游，并且设计上在 provider 选择之前执行。二者不得混为一谈。

### 入站审查（Ingress review，client → proxy）

按**客户端协议规范**校验入站请求。客户端契约是固定的（§10.1），因此这里严格是安全的：缺失必填字段、字段类型错误、畸形的消息/工具结构都是客户端 bug，以本地客户端协议形态的 4xx 失败，报错比上游给的更清晰。任何东西都不会被转发。

**模型代理 HTTP 请求体上限**：
- 外部模型代理 HTTP 路由（model proxy HTTP requests）采用内部固定的 **64 MiB** (`DefaultBodyLimit::max(64 * 1024 * 1024)`)，不向用户暴露配置项（no user configuration）。
- **设计依据与容量预估**：主流超长上下文场景下，预估 8M-token 纯文本请求体约 36 MB；固定 64 MiB 为完整 JSON 结构、复杂工具定义与元数据保留了充足预算。
- **有限 OOM 防护**：作为确定性的有限内存防线（finite OOM guard），既避免框架默认过小的限制截断合法大上下文请求，又杜绝无界请求体耗尽代理进程内存。
- **作用范围**：精确限定于外部模型代理 HTTP 请求；管理通道 UDS（management UDS）行为保持不变。

### 客户端 token 准入（Client token admission，client → proxy）

代理可以在模型代理路由上要求客户端 token（声明于 `tokens.toml` 或与 `config.toml` 同级的 `tokens/*.toml` 目录；两者并存时启动/重载以错误拒绝）。这一层是代理**对自身调用方的准入控制**——由本地配置决定、不联系任何上游，因此独立于上面统辖原则禁止预判的上游账号状态。

| 方面 | 契约 |
|---|---|
| 身份 | `Authorization: Bearer <key>`（OpenAI 风格）或 `x-api-key: <key>`（Anthropic 风格）；常数时间比较。 |
| 密钥形态 | `lp-` + 32 位字母数字；在 token 存储内唯一。 |
| 开放模式 | 未配置任何 token 时，模型代理路由开放且不执行准入控制（开发默认）。 |
| 模型作用域 | `allowed_models`（空 = 所有模型）。body 解析完成后在 handler 中检查：超出作用域的模型返回本地 `403`。 |
| 配额组/窗口 | `quota_windows[] = { window, quota_mode, quota_limit }`（token 级遗留）或 `model_quota_groups[] = { models, quota_windows[] }`（模型作用域组）；时长用 `s`/`m`/`h`/`d`/`w`，可复合。跨匹配组按 AND 语义：一个请求匹配多个组时，准入时检查所有匹配组、响应后计费。窗口不嵌套：**任何**耗尽的窗口都拒绝；不匹配的组保持隔离。 |
| `requests` 模式 | 每个**已派发上游**的客户端请求计 1 点。 |
| `tokens` 模式 | 加权 token 成本，响应后计费，因为成本事先未知。它是**预算表**：准入只读取已记录的用量，不做任何预留。 |
| 状态码 | `401` 密钥无效或过期；`403` 模型超作用域；`429` 配额耗尽。 |

**计数规则。** `requests` 窗口统计代理实际发往上游的数量——而不是成功发送的数量。准入与记账是**同一个原子步骤**，在请求第一次上游派发之前立即执行，因此在途请求占用窗口，`used <= quota_limit` 作为硬不变式成立。本地拒绝的请求（入站 4xx、模型作用域 `403`、指纹 `429`、全候选冷却 `503`）永远到不了派发、也永不消耗点数。跨 provider 的内部回退只计一次费：窗口统计的是客户端请求，不是上游尝试。上游事后失败不退款——该请求已经消耗了上游容量，而那正是窗口所保护的东西。

**原子步骤发生在存储里，不在进程里。** 每个配额写者都持有自己到配额数据库的连接——服务器请求路径、管理 handler、每一次 CLI 调用——因此 `used <= quota_limit` 不能依赖单个进程串行化自己的句柄。所以整步是**一个写事务**：创建配置的窗口行、把已过期的置为过期、读取每个窗口、自增。失败的 upsert 回滚，而不是把一个从未被检查过的窗口留给下一个读者。

**无法求值的配额一律拒绝，绝不放行。** 该事务的任何失败——存储不可用、窗口行创建失败、写入等待另一写者超时——都必须以 `503` 加 `quota_unavailable` 错误拒绝请求，并记录日志。“配额未知”与“未配置配额”是不同的陈述，不得坍缩为同一结果；在前者上放行，等于在计量器恰好坏掉时发放不计量的访问。对应的请求路径开销是有界的：冲突的写者等待（有界忙超时）而不是立即失败，因为别人在途的写入不是拒绝这位调用者的理由。

**tokens 模式准入是检查，不是预留。** `tokens` 窗口无法在派发时预留，因为其成本在响应前未知——上面的原子步骤对它不可能。因此准入读取已记录的用量，一旦 `used >= quota_limit` 就拒绝（`429`），而计费仍落在响应之后。其后果是刻意的：由于并发在途请求尚未计入，一次突发可以把 `used` 顶过限额至多（并发数 × 单请求成本）。这种有界超调被接受——`tokens` 窗口是预算表（“这个 key 大致花掉了它的额度”），不是逐请求硬上限。请求路径上的那一步仍然是原子的：它读取每个窗口（两种模式），**任何**耗尽的窗口都拒绝且**不**自增任何窗口，因此共存于同一 token 上的 `requests` 与 `tokens` 窗口都为同一请求把关。

**耗尽要解释，不只是拒绝。** 配额 `429` 携带结构化 body，点名耗尽的窗口（`window`、`quota_mode`、`quota_limit`、`used`、`remaining`、`reset_at`）。

**配额绝不能挡住对自己的读数。** `GET /v1/token/usage` 上报调用者自己的窗口。它的认证方式与其他模型代理路由相同，但**从不**被配额拦截。拦截它恰好会在配额耗尽时隐藏调用者的用量：它破坏自助闭环（调用者只能去问运维）、让“我的配额用完了”与“服务坏了”无法区分、隐藏服务何时恢复，并剥夺守规矩客户端在调用前自查的能力。计量器的可观测性本身不是被计量的资源。同理，`GET /v1/token/models` 上报调用者被授权的模型与能力边界，**从不**被配额耗尽拦截，保证用量触顶时客户端仍能发现模型。

**窗口集合跟随配置。** 用量存储在配置加载边界（启动与重载）收敛到配置的窗口，绝不在请求路径上。配置中删除的窗口不复存在。修改窗口时长会把该窗口从零重启——窗口在被配置之前没有历史。

**token 行跟随配置，但只在配置了内容时。** 删除 token 会在同一加载边界删除其用量行，因此清理步骤失败的 `token delete` 不会留下无人认领的行。当配置完全没有列出 token 时跳过清理：缺席或暂时读不到的 `tokens.toml` 不得清零所有在用 token 的窗口计数器，那等于发放一次静默配额重置。因此 `token delete` 报告清理失败而不是吞掉它——配置半边已经应用、无法回滚，半成功的删除不是成功。

**配置半边与配额半边不是两次独立的写。**“查 token → 写 `tokens.toml` → reload 发布 → 清配额行”构成**同一个提交边界**，由 单一临界区保证：临界区内不得插入让另一个写者整份覆盖的窗口，否则一次丢失更新会同时撤销“配置删除”和“配额清理”两半。

**窗口标识是复合的。** 配额窗口由 `(token_key, group_id, window_key, quota_mode)` 唯一标识，而非仅靠 `(token_key, window_key)`。`group_id` 是模型配额组的规范化标识（排序后的逗号分隔模型名，token 级/遗留窗口为 `""`）。这使得同一时长（如 `1w`）可以在同一 token 上跨不同组、在 `requests` 与 `tokens` 两种模式下承载互相独立的限额。存储 schema 以该复合键为主键（`PRIMARY KEY (token_key, group_id, window_key, quota_mode)`），因此 upsert 与对账把 `(token_key, group_id, window_key, quota_mode)` 视为不同行。

> 原子预留仍然仅限 requests：`tokens` 闸门搭同一套全有全无的准入步骤，但做的是读取并拒绝、而非预留。复合主键 `(token_key, group_id, window_key, quota_mode)` 允许同长度窗口跨模式与模型组共存而不冲突。

### 出站审查（Egress review，proxy → upstream endpoint）

在发送到具体上游端点之前，按**目标原生端点的 `compat` 声明**校验并适配出站 body。这一层设计上不对称：每个 provider 端点声明其上游实际容忍什么，适配器/转发器执行该声明：

| `compat` 字段 | 出站行为 |
|---|---|
| `supports_developer_role`（默认：`true`） | `false` → developer 角色消息降级为 system 角色。 |
| `supports_reasoning_effort`（默认：透传） | effort 默认原样转发给上游。只有在模型级显式声明（`features` 含 `effort_ignored`）或端点级声明（`false` → `reasoning_effort` 被剥离，而不是转发进去换一个大概率 400）时才剥离。 |
| `thinking_format`（默认：无） | 选择适配器发出的该 provider 思考参数格式。 |
| `requires_reasoning_content_on_assistant_messages`（默认：`false`） | `true` → 多轮工具调用历史必须携带 reasoning_content；推理缓存提供它。 |
| `max_tokens_field`（默认：`max_completion_tokens`） | 校验/发出最大输出 token 参数时使用的字段名。只接受 `max_tokens` 的遗留模型需显式设置此项。 |
| `force_stream`（默认：`false`） | `true` → 出站 responses 请求中 `stream` 被强制为 `true`；该上游只接受流式。非流式客户端于是收到聚合后的 JSON 响应（见 §5.1a）。 |
| `strip_max_output_tokens`（默认：`false`） | `true` → 出站 responses 请求中剥离 `max_output_tokens`；该上游拒绝该参数。 |

当端点没有 `compat` 子表时，应用上述默认值（标准 OpenAI 兼容假设）。

### 出站响应侧契约（force_stream 聚合）

当原生 responses 端点声明 `compat.force_stream = true` 时，上游始终流式返回。运行时随后按客户端请求（`stream`）选择面向客户端的响应形态，该判断在出站适配**之前**记录：

| 客户端 `stream` | 上游行为 | 面向客户端的响应 |
|---|---|---|
| `true` | SSE 透传 | SSE 事件原样转发（只改写 `model` 字段） |
| `false` | 上游被强制为 `true` | SSE 聚合回 responses JSON 值 |

聚合路径从上游事件流重建 `output` 项：`message` 项保留声明的 `content[]` 部分并按 `content_index` 累积文本；`function_call` 项保留 `call_id`/名称/参数；`reasoning` 项保留 summary 文本与签名。其他项类型被有意丢弃。usage 从 `response.completed` 记录。

### 出站审查严格度与层组合（Egress review strictness and layer composition）

严格度规则：出站审查**适配**它能安全适配的一切（改名、剥离、降级），只拒绝无法表达的内容。转换产出的 body 必须满足目标协议 schema——转换 bug 就是代理 bug：以内部错误本地失败，绝不转发，绝不把它计到 provider 或请求指纹上。

| 失败类别 | 发往上游 | 冷却 | 指纹 | 配额 | 下一候选 | 客户端看到 |
|---|---|---|---|---|---|---|
| 预检违规 | 绝不 | 否 | 否 | 否（准备在请求预留**之前**运行） | 继续 | 本地 400 `unsupported_request`，点名 provider 与最多 32 条 `path: rule` 违规（“未联系任何上游”） |
| 逐候选转换 `Err` | 绝不 | 否 | 否 | 否 | 继续（后续候选可以原生服务该形态） | 400，携带该候选的转换消息 |
| 真实上游 400 | 已发送 | token 认证请求按 **(token × provider) 对**（`client_error_seconds`）；开放模式下仅 provider 全局 | 是（§10.5.6 不变） | 已花费 | 继续 | 镜像上游 400 |

V7 声明名子集检查只在信封至少携带一个 `functionDeclarations` 条目时适用：多轮 replay 续传合法地省略 `tools`，没有可核对的声明权威，上游容忍。预检拒绝在 `state_dir/diagnostics.jsonl` 留下结构化 `egress_preflight` 记录；真实上游 4xx 留下 `upstream_client_error` 记录，携带请求特征与存在时的上游 `fieldViolations`。capture 默认仅元数据——字面 prompt 内容永不到达文件，只有计数/长度/摘要；`capture = "full"` 内嵌出站信封（认证在头部，且顶层键集必须恰好是封闭的 agent 信封形态，否则记录降级为元数据并置 `truncated = true`），受 `max_record_bytes`（单条）与 `file_max_bytes`（单文件，轮转保留最新一半）约束。前端 token 以显示名记录，绝不以密钥记录。校验器只判形状——绝不判 provider 健康或账号状态（见上面的信任契约）。

**查询面**（`GET /admin/diagnostics`）：结构化记录可通过单个管理端点读取，该端点**只**注册在 UDS 管理通道上（仅属主可读的文件系统权限，`0600`）；它刻意不出现在公共 HTTP 监听器上（在那里 404——`capture = "full"` 的记录内嵌出站 body，绝不能可被网络访问）。查询参数：`event`（按 `egress_preflight` / `upstream_client_error` 过滤）、`period`（与 `/admin/usage` 相同的语法）、`limit` / `offset`（分页）。可见性规则：响应默认剔除 `body_full`；调用者必须传 `include_body=true` 选择启用，且即便如此，该字段也只在 `capture = "full"` 实际内嵌时才存在。响应携带 `capture` 字段（`"metadata"` / `"full"`），让调用者知道服务端配置记录什么。

层的组合如下：

```text
Layer 0a：入站审查（严格，客户端协议规范）      → 尽早捕获客户端 bug
Layer 0b：出站审查（不对称，端点 compat）        → 适配上游怪癖；捕获代理转换 bug
Layer 1： 指纹封禁（§10.5.6）                   → 捕获重复的语义 400
Layer 2： client_error 冷却（§10.5.5）           → 上游 400 归因于（token × provider）对而非 provider；4xx 风暴被遏制且不惩罚同租户
Layer 3： 账号坏请求预算（逐候选）                → 只计真正的 400（限速类 400 除外）；耗尽则跳过该候选、回退继续；只有全部候选耗尽才 429
```

**账号坏请求预算**（配置 `[protection.bad_request_budget]`，默认开启 / 每日 2 / 终身 10；`tokens.toml` 中 per-token 覆盖 `bad_request_budget_daily_limit` / `bad_request_budget_lifetime_limit`；管理走仅 UDS 的 `GET /admin/protection/bad-request-budget` 与 `POST /admin/protection/bad-request-budget/reset`；响应以显示名加脱敏 key（`lp-kz0…2b8` 形式）标识账号——完整 key 绝不序列化，阈值日志只打印显示名）：

- **计数**：只有通过 `is_protectable_client_error` 的真正上游 4xx 消耗预算——401/403/429 被排除（各有专用冷却路径），本地预检拒绝或转换缺陷永不计入。上游 400 若 body 属限速类——Google RPC `RESOURCE_EXHAUSTED` detail 类型，或 `error.message` 含限速关键词（`rate limit`、`ratelimit`、`quota`、`too many requests`、`exceeded`、`throttled`）——则**不**计入：那是属于限速冷却路径的运营限制，不是请求 bug 的证据。402/404/408 同样被排除——它们映射到专用冷却路径，代表 provider/账号/网络状况（计费、模型不可用、超时），绝非请求 bug。按客户端请求计数：每个被计状态在候选循环中都是终态，因此一次失败请求至多贡献一次计数、被救援的尝试从不消耗预算。无法解析的 body 保守计入（向保护侧失败）。
- **耗尽是逐候选的，不是一刀切**：每日或终身预算耗尽的候选在候选循环内被跳过，回退继续到下一候选；只有全部候选耗尽才返回 429。指纹封禁与 token 错误类冷却按设计仍是循环前的一刀切拒绝——它们说的是“这个请求形态 / 这个 token 不对”，换 provider 也没用。
- **每日恢复消息**：429 body 携带精确的 UTC 恢复时间戳与实时倒计时（`recovers at <ISO-8601> (in Nh Nm)`），绝不含糊地说“明天恢复”；恢复点是下一个 UTC 午夜的每日窗口边界。终身耗尽需要通过 UDS 管理重置。

Provider 选择与协议转换是刻意分开的：

- Provider 选择从模型的按协议列表中挑选一个 provider 绑定。
- 端点解析决定该 provider 如何服务所选协议。
- 适配器执行在需要时于端点协议之间做转换。

## 5.2 思考控制（Thinking Control）

Model 级思考控制将**能力声明**与**入口策略**显式分离。能力声明描述模型是否支持思考，入口策略控制请求的默认行为和强制约束。

### 5.2.1 配置字段契约

Model 配置支持以下 thinking 控制字段（全部可选，Provider 不接受这些字段）：

| 字段 | 维度 | 类型 | 语义 |
|------|------|------|------|
| `supports_thinking` | 能力 | `Option<bool>` | 纯能力声明：“模型能不能思考”。只被 launch 客户端能力元数据消费，无任何请求路径行为。None 不声明。 |
| `force_think` | 策略 | `Option<bool>` | 仅 `Some(true)` 启用强制开启；`None` 或 `Some(false)` 不启用此约束。 |
| `force_no_think` | 策略 | `Option<bool>` | 仅 `Some(true)` 启用强制关闭；`None` 或 `Some(false)` 不启用此约束。 |
| `default_think` | 策略 | `Option<bool>` | 可选默认开关；true/false 分别默认开/关，均可被合法的客户端显式意图覆盖；None 不补开关。 |
| `reasoning_levels` | 兼容层 | `Option<Vec<ReasoningLevelEntry>>` | 一张表同时表达允许活动档和映射。每行一个 `level`（活动档），可选 `api_value`（映射到上游值；缺省 = 恒等）。表里有的活动档 → 允许并按 `api_value` 映射；表里没有的活动档 → 400。关意图（`none`/`disabled`）和「开、无档」（`adaptive` 无 effort）不走活动档门禁。Model 有表则只用 Model 表（整表覆盖 Provider）；Model 没表、Provider 有表则用 Provider 表；都没表则不声明、恒等放行。禁止 `level="adaptive"`（不是 effort）、`api_value=null`/`""`、空表。 |
| `default_reasoning_level` | 策略 | `Option<String>` | 可选活动默认等级；只补缺失等级，不覆盖客户端显式等级。`none` 和 `adaptive` 不是合法默认等级。`default_think=true` 时必须同时有默认档，否则加载失败；`force_think=true` 可以没有默认档——含义是「思考恒开、不控档」（如 Claude extended thinking），见 §5.2.2。 |
| `chat_reasoning_off_format` | 方言（已废弃） | `Option<String>` | **本路径忽略**，写了也不驱动跨协议关思考。关思考方言由目标原生端点 B 的 `compat.thinking_format` 决定。保留字段定义仅为向后兼容加载，不产生请求路径行为。 |

**核心原则**：
- 能力维度与策略维度正交。`supports_*` = 能力声明，`force_*`/`default_*` = 入口策略。
- 入口强制/默认只属于 frontend Model，不继承 Provider。同一概念上游模型若需不同入口或兼容策略，使用不同 frontend model IDs。
- 两个 force 同时为 true 是配置错误；`force_think=true` 与 `default_think=false` 冲突；`force_no_think=true` 与 `default_think=true` 或活动默认等级冲突。

### 5.2.2 意图解析与默认/强制落实

请求处理遵循以下规则（按顺序）：

1. **保留不可变原始请求**：分别记录客户端显式开关/等级/预算、Model 默认、Model 强制约束及转换结果；不能在注入参数后的 body 上反推“客户端显式”。

2. **验证显式参数一致性**：活动等级或正预算属于显式开启意图，`none`、显式 false、标准 disabled 属于关闭意图；未知值按 schema/声明处理，不猜成预算或开启表达。显式 on+off 矛盾返回本地 400。

3. **显式意图优先**：有合法显式意图时使用它，默认只补尚缺且不矛盾的部分；没有显式开关/开启或关闭意图时才采用 `default_think`。有效关闭时绝不补活动等级/正预算。

4. **默认等级的补档规则**：有效状态不是 off、缺等级时可补活动默认等级。`default_reasoning_level` 不反向创建旧方言开关（`enable_thinking=true`、`thinking.type=enabled` 等）。4.6+ Anthropic 出站时：有默认档 + `adaptive` 无档 → 补成 `adaptive + effort=该档`；出站 Chat/Responses 时：补成 `effort=该档`。`default_think=true` 必须同时有默认档（加载失败）；`force_think=true` 无默认档合法，按规则 5 落实为开且不设档。

5. **强制落实必须表达**：有效 force 必须落实，即使请求没有相关字段。force-on 有默认档时落实默认档；无默认档时不设档位、不擅自选 `high` 或预算，由转换器注入上游默认思考配置（如 Claude extended thinking）；方言开关（如 qwen 的 `enable_thinking=true`）仅在目标方言需要时随附。force-off 同理，必须发送可表达关闭的参数。

6. **全缺省不注入**：请求及配置均缺省时不补任何思考参数，让上游决定；只有 force 为 true 时不能沿用这一缺省发送结果。

**冲突检测矩阵**：

| 配置 | 客户端请求 | 预期语义 |
|------|-----------|---------|
| 全部缺省，或仅两个 force 为 false | 全部缺省 | 不补参数，上游自行决定。 |
| `default_think=false`、默认等级 `high` | 全部缺省 | 有效 off。Anthropic `disabled`（不带 effort）；Chat/Responses `none`。默认档不补入关闭态。 |
| 同上 | 显式 off | 同上，有效 off。 |
| 同上 | 显式 on、无等级 | 显式 on 覆盖默认 off，补 high。 |
| 同上 | 显式 effort=E | 显式覆盖默认 off，保持 E。 |
| `default_think=true`、默认等级 `high` | 显式 off | 尊重客户端显式 off：出站 `disabled`，不补默认档。`default_think` 是补缺省不是强制。 |
| 仅默认等级 `high` | 全部缺省 | 隐含开 + high。Anthropic `adaptive + effort=high`；Chat/Responses `effort=high`。 |
| 仅默认等级 `high` | `adaptive` 无档 | 补成开 + high。Anthropic `adaptive + high`；Chat/Responses `effort=high`。 |
| `force_no_think=true`、默认等级 `high` | 全部缺省 | 落实关 + 档。Anthropic `disabled + output_config.effort=high`；Chat/Responses `none`（degraded，丢档）。 |
| `force_no_think=true` | 显式 on / 活动档 / `adaptive` | 客户端显式与强制约束冲突，本地 400。文案只改请求。 |
| `force_think=true`，有默认等级 | 全部缺省 | 落实开 + 默认档。 |
| `force_think=true` | 显式 off / none | 客户端显式与强制约束冲突，本地 400。文案只改请求。 |
| `force_think=true`，无默认等级 | 全部缺省 | 落实开，不设档位：由转换器注入上游默认思考配置（如 Claude extended thinking）。 |
| `default_think=true`，无默认等级 | 全部缺省 | **加载失败**，不拖到请求。 |

### 5.2.3 请求校验与错误归因

- 对 Responses `reasoning.effort=none` 或 Chat `reasoning_effort=none` 与显式 `enable_thinking=true` 的组合，本地返回 HTTP 400。`enable_thinking` 不是三个入站协议的官方字段，见到一律 400。
- Anthropic 的 `thinking.type=disabled` + `output_config.effort=E`（活动档）是**合法**组合：关思考但仍控输出力度。Anthropic 出站原样透传；转 Chat/Responses 时保关丢档（写 `none`，丢掉 `E`，结果类型 `degraded`），不反向写成 `effort=E`（对侧语义是开思考）。`thinking.type=enabled`（有无 `budget_tokens`）官方已废弃，本路径一律 400。合法的显式 thinking/effort 不被默认等级覆盖。
- 显式请求与已启用 force 冲突也返回本地 400。以上入口校验必须发生在联系任何上游、建立上游连接或选择 fallback 来执行请求之前；**零上游发送、不重试、不 fallback、不记 Provider 冷却或请求指纹惩罚**。
- 非冲突合法组合（活动等级 + true、none + false 等）按既有协议转换及已配置兼容规则表达，不误拒绝；合法显式 off 覆盖默认 on、合法显式 on 覆盖默认 off 均不是客户端错误。
- 配置自相矛盾须在静态校验时拒绝（见 §4.2）。默认/强制注入、等级 map、协议转换或方言适配制造的矛盾/无法表达，属于**配置或代理适配错误**，返回 500 而非客户端 400，不能伪装客户端错误，也不能发给上游碰运气或用 fallback 隐藏缺陷；运行期不计上游冷却。

### 5.2.4 错误分类

请求路径错误分为两类，必须明确区分：

- **客户端错误**（400）：客户端请求语义矛盾（如显式 on+off、显式与 force 冲突）。不联系上游，不计冷却。
- **配置/适配错误**（500）：配置或适配错误（如 `reasoning_levels` 表不合法、`default_think=true` 无默认档）。不联系上游，不计冷却，错误消息包含修正方向。

### 5.2.5 候选隔离

每个 candidate 都从不可变原始请求和同一份入口意图重新构造，不能继承前候选的等级替换、方言转换或剥离结果。Fallback 只更换上游绑定与合法 egress 表达，不重新选择入口 Model 的 force/default，不继承候选 Provider 的入口默认。

共享默认值处理不得抹掉原始 missingness；仅在循环内 clone 已加默认值的 body 不够。固定本次请求的 frontend Model 配置快照，先完成原始请求校验与入口意图解析，分别保留显式/默认/强制来源。

### 5.2.6 launch 能力证据

launch 客户端能力元数据判定“模型支持思考”的证据：
- 非空 `reasoning_levels` 表（Model 或沿用的 Provider）
- `supports_thinking=true`
- force-on
- `default_think=true`
- 活动 `default_reasoning_level`

任一即为支持思考的证据；force-off 与 `default_think=false` 不构成不支持证据（策略维度），`supports_thinking=false` 是唯一显式否定证据。

## 6. 适配器注册表（Adapter Registry）

适配器是显式、具名的转换实现，**每个（源协议，目标协议）二元组恰好一个实现**。转换语义由 conversion spec 按二元组定义；provider 特有差异是被该单一适配器消费的数据（如 `thinking_format`、`max_tokens_field` 这类 provider 策略字段），绝不是同一二元组的第二个适配器。配置从不命名适配器——派生端点仅通过声明 `derive_from` 选择其转换。

必需的适配器集（三种客户端协议间全部 6 个双向方向）：

| 适配器 | 源协议 | 目标协议 | 用途 |
|---|---|---|---|
| `responses-from-chat-completions` | `openai-chat` | `openai-responses` | 通过 Chat Completions 上游服务 Codex Responses 客户端。 |
| `chat-completions-from-responses` | `openai-responses` | `openai-chat` | 通过 Responses 上游服务 Chat Completions 客户端。 |
| `anthropic-from-chat-completions` | `openai-chat` | `anthropic` | 通过 Chat Completions 上游服务 Claude 兼容客户端。 |
| `chat-completions-from-anthropic` | `anthropic` | `openai-chat` | 通过 Anthropic 上游服务 Chat Completions 客户端。 |
| `responses-from-anthropic` | `anthropic` | `openai-responses` | 通过 Anthropic 上游服务 Responses 客户端。 |
| `anthropic-from-responses` | `openai-responses` | `anthropic` | 通过 Responses 上游服务 Anthropic 客户端。 |

所有适配器同时支持非流式与流式变体。

### 流式工具调用身份路由（Streaming tool-call identity routing）

流式工具调用是跨事件实体，不是单个 SSE 帧。每个转换工具调用的流式适配器都必须在创建、参数增量、完成与工具结果关联之间保持源实体身份。代理必须维护一张从源协议身份键到目标协议工具调用身份或索引的本地映射。

| 源 SSE 协议 | 源身份键 | 需保持的目标身份 |
|---|---|---|
| OpenAI Responses | `response.output_item.added.item.id` / 随后的 `item_id` | Chat `tool_calls[index]` 或 Anthropic `content_block.index` |
| OpenAI Chat Completions | `delta.tool_calls[n].index` | Anthropic `content_block.index` |
| Anthropic Messages | `content_block_start.index` / 随后事件的 `index` | Chat `tool_calls[index]` 或 Responses `{item_id, output_index, call_id}` |

流式转换不变式：

1. 创建事件分配一个稳定的目标身份，并在转发后续增量事件之前记录映射。
2. 每个参数增量与完成事件用其自身的源身份键查映射。转换器绝不得用固定目标索引、最近创建的工具或到达顺序作替代。
3. 完成事件只终结其关联的目标调用。它不得关闭、覆盖或丢弃另一个活动调用。
4. 转换器保持连续 SSE 输出：创建与参数增量事件在收到时即发出。任何改变可观测事件时序的缓冲都需要显式的协议证据、适配器契约更新与成文的客户端影响说明。
5. 未知或畸形的源身份事件不得被挂到另一个工具调用上。按客户端协议可表达的错误语义，它们被忽略或作为协议错误浮出。

命名约定：适配器名描述“从源协议所提供”的端点，形式为 `target-from-source`。适配器名是内部注册表标识符，绝不出现在用户配置中。

校验注册表必须知道每个适配器支持的源与目标协议二元组。派生端点的校验很简单：`derive_from` 字段引用同 provider 上存在的端点，且注册表包含（源协议，端点协议）二元组的适配器。运行时代码不得靠字符串匹配推断适配器兼容性；必须经注册表。

### 适配器兼容性政策（Adapter compatibility policy）

当前 Rust 实现必须保持 Go/v1 的转发能力。Go/v1 转换器已实现主要服务方向，且总体是 best-effort：核心消息/工具/多模态/推理结构被转换，而某些未知或 provider 特有字段可能被忽略、而不是导致本地拒绝。当前 Rust 实现不得引入会破坏 Go/v1 能转发的请求的大范围新适配器拒绝，除非该差异在本设计文档中被显式接受。

每个适配器契约应把请求/响应字段归入三种成文的结果：

| 结果 | 含义 | 默认政策 |
|---|---|---|
| converted（已转换） | 字段以等价的目标协议语义被保留。 | 对 Go/v1 支持的核心语义是必需的：文本消息、system/developer 角色处理、工具声明、工具调用/结果、工具调用 ID、Go/v1 支持处的图像/文档块、provider/model 策略覆盖的 thinking/reasoning 字段、流式增量与模型 ID 改写。 |
| degraded/dropped（降级/丢弃） | 字段没有有用的目标表达，被有意省略或近似。 | 只允许在被记录为 Go/v1 兼容的 best-effort 行为或有意的对等差异时。未知扩展字段、非核心元数据、细粒度 usage 子字段、无目标等价物的 annotations/引用、不支持的 provider 私有扩展通常归此类。 |
| local reject（本地拒绝） | 代理返回客户端协议形态的本地 4xx/5xx，不联系上游。 | 保留给以下情形：转发会违反硬性用户契约、制造已知的上游 400 循环、暴露不安全的本地行为，或指示代理/配置 bug。 |

初始本地拒绝应贴近 Go/v1 行为：

- 不支持的协议/端点组合或缺失的适配器注册表条目；
- 畸形的工具调用历史，如孤儿工具调用；
- 与已配置 model/provider 策略冲突的 reasoning/thinking；
- 启动校验捕获的不可能端点图；
- 需要适配器未显式实现的不安全本地文件/网络访问的请求形态；
- 由代理转换 bug 产生、通不过目标原生端点 schema 的出站 body。

严格结构化输出/JSON schema 之类的硬输出约束不会被自动拒绝。适配器必须要么转换它们，要么带成文的 Go/v1 兼容理由有意降级/丢弃，要么在静默降级对受支持客户端更糟时拒绝它们。该选择属于本设计文档中的适配器契约，不属于临时的转发代码。

#### 截断终态子契约（所有 Responses 与 Anthropic 出站）

输出预算耗尽（或内容被拦截）的上游绝不能被呈现为干净完成：形如 `status == "incomplete" && incomplete_details.reason == "max_output_tokens"`（Responses）或 `stop_reason == "max_tokens"`（Anthropic）的客户端守卫，是截断变得可见的**唯一**通道，而修复前观察到的最坏形态是零内容响应被报为 `completed`（于是 harness 把这轮当完成、永不重试）。每个协议二元组映射自己的终态信号；Chat→Responses 映射是参考实现：

| 上游终态信号 | Responses 出站 | Anthropic 出站 |
|---|---|---|
| Chat `finish_reason: "length"` | `status: "incomplete"` + `incomplete_details.reason: "max_output_tokens"` | `stop_reason: "max_tokens"` |
| Chat `finish_reason: "content_filter"` | `status: "incomplete"` + `incomplete_details.reason: "content_filter"` | `stop_reason: "end_turn"`（Anthropic 无 content-filter stop reason） |
| Anthropic `stop_reason: "max_tokens"` | `status: "incomplete"` + `reason: "max_output_tokens"` | —（透传） |
| Anthropic `stop_reason: "refusal"` | `status: "incomplete"` + `reason: "content_filter"` | —（透传） |
| 干净完成（`stop`、`tool_calls`、`end_turn`、`stop_sequence`、`STOP`、…） | `status: "completed"` + `incomplete_details: null` | `stop_reason: "end_turn"` / `"tool_use"` |

上述每个出站的附加不变式：

- **项状态**：终态为 `incomplete` 时，`output` 中每一项也携带 `status: "incomplete"`——被截断的文本或部分发出的 `function_call` 不得声称完成。
- **流式 Responses**：终态 SSE 事件是 `response.incomplete`（携带同一 response 对象）而非 `response.completed`；`response.completed` 只为干净完成发出。
- `incomplete_details` 在这些转换器产生的 Responses 出站 body 上始终存在（完成时为 `null`）。

#### 上游生成失败子契约（所有协议与入站形态）

因生成错误或异常 finish reason 而中止的上游模型，绝不能被伪造为干净完成或通用传输失败：**“宁可显式失败，不谎报成功；不引入任何语义伪造”**（prefer explicit failure over false success；引入零语义伪造）。

1. **分类与优先级锁存**：
   - 上游终态 finish 信号被分为六个有序类别：`Clean`、`ToolCalls`、`Length`、`Safety`、`GenerationFailed`、`Unknown`。
   - 跨候选优先级排序：`GenerationFailed > Safety > Length > ToolCalls > Clean`。无法识别或非终态的 finish 码归为 `Unknown`，按不可恢复生成失败 fail-closed（在分类锁存与投递中，`Unknown` 并入 `GenerationFailed`）。
   - `GenerationFailed` 覆盖不可恢复的模型生成失败（如 Gemini `MALFORMED_FUNCTION_CALL`、`UNEXPECTED_TOOL_CALL`、`TOO_MANY_TOOL_CALLS`）。
   - `Unknown` 覆盖无法识别或非终态的码（如 `IMAGE_OTHER`、`NO_IMAGE`，或未文档化的 finish reason）。

2. **双形态失败投递**：
   - **非流式出站（HTTP 502 Bad Gateway）**：
     - 附加头部 `x-llm-proxy-upstream-generation-failed: true`，跨内部层传递生成失败信号。
     - OpenAI Chat Completions：返回 `{"error": {"message": "upstream generation failed (<reason>); request_id: <request_id>; check request parameters, retry, or consult docs/user_guide/troubleshooting.md", "type": "server_error", "code": "upstream_generation_failed"}}`。
     - OpenAI Responses：返回 `{"error": {"message": "upstream generation failed (<reason>); request_id: <request_id>; check request parameters, retry, or consult docs/user_guide/troubleshooting.md", "code": "upstream_generation_failed"}}`。
     - Anthropic Messages：返回 `{"type": "error", "error": {"type": "api_error", "message": "upstream generation failed (<reason>); request_id: <request_id>; check request parameters, retry, or consult docs/user_guide/troubleshooting.md", "code": "upstream_generation_failed"}}`。
   - **流式出站（渐进式流中断）**：
     - 遇到生成失败时，SSE 流立即以协议级错误事件终止，**严格省略**任何完成标记（`[DONE]` 或 `message_stop`）：
       - OpenAI Chat SSE：发出携带 `{"error": {"message": "upstream generation failed (<reason>); request_id: <request_id>; check request parameters, retry, or consult docs/user_guide/troubleshooting.md", "type": "server_error", "code": "upstream_generation_failed"}}` 的错误 chunk，不以 `data: [DONE]` 结尾。
       - OpenAI Responses SSE：发出 `event: response.failed`，载荷 `{"type": "response.failed", "response": {"status": "failed", "error": {"type": "upstream_error", "message": "upstream generation failed (<reason>); request_id: <request_id>; check request parameters, retry, or consult docs/user_guide/troubleshooting.md", "code": "upstream_generation_failed"}}}`，不以 `data: [DONE]` 结尾。任何部分流出的输出项携带 `status: "incomplete"`。
       - Anthropic Messages SSE：先关闭打开的内容块（在终态错误前发出 `event: content_block_stop`），再发出 `event: error`，载荷 `{"type": "error", "error": {"type": "upstream_error", "message": "upstream generation failed (<reason>); request_id: <request_id>; check request parameters, retry, or consult docs/user_guide/troubleshooting.md", "code": "upstream_generation_failed"}}`，不以 `message_stop` 结尾。

3. **裁决旁路**：
   候选重试循环一检测到上游生成失败即中止。生成失败旁路重试、回退、 provider 冷却与保护指纹记账：

   | 失败类别 | 重试 | 回退 | 冷却 | 记账/黑名单 |
   |---|---|---|---|---|
   | 生成失败（`GenerationFailed` / fail-closed 的 `Unknown`） | 否（中止尝试） | 否（不换 provider） | 否（provider 保持活跃） | 排除出指纹黑名单 |
   | 标准 5xx（网络错误 / 上游 500） | 是（至配置重试次数） | 是（尝试其他候选） | 是（冷却失败 provider） | 记入失败记账与冷却状态 |

4. **记账与遥测**：
   - Provider 指标：记在 `upstream_error` 计数器槽下，与可用性 SLO 测量对齐，usage 状态置为 `generation_failed`。
   - 流式 Replay guard：丢弃未投递的工具调用（`item.delivered == false`），防止不完整或畸形调用污染会话历史。
   - 隐私保护 WARN 日志：发出结构化警告日志，含安全净化后的 finish reason（截到 64 字节安全字符）、消息摘要（`len={len},sha256={hash}`，绝不明文）、工具数与 schema 摘要、耗时以及已投递事件数。

#### Chat max-token 预算参数子契约

Chat Completions 以两个名字承载输出预算：遗留的 `max_tokens` 与当前的 `max_completion_tokens`（官方 OpenAI SDK 发出的名字，且对某些推理模型是唯一被接受的）。代理在**目标协议只有单一预算字段**的每个 Chat 入站边缘（Responses `max_output_tokens`、Anthropic `max_tokens`）把它们当作同一个预算：

| Chat 输入 | 出站效果 | 结果 |
|---|---|---|
| 仅 `max_completion_tokens` | 预算设为其值 | converted |
| 仅 `max_tokens` | 预算设为其值 | converted |
| 两者都在 | `max_completion_tokens` 胜出（较新的规范名）；遗留值被丢弃 | converted |
| 任一键为 JSON `null` | 该键视为缺席；回退到另一个 | converted |
| 两者都缺席 | 出站 body 省略预算关键字 | converted |

同协议 Chat→Chat 透传原样转发 body：由上游自己的 schema 决定接受哪个名字。provider 兼容旋钮 `max_tokens_field` 选择代理自身**探测** body 发送的字段名；它不是请求路径改写。静默丢弃预算（转换边缘上的旧行为）是违约而非降级：客户端的 token 上限凭空消失，用户为无界输出付费。

#### 响应 ID 与时间戳命名空间子契约

| 协议表面 | ID 命名空间 | 时间戳字段 |
|---|---|---|
| Chat Completions 响应 / SSE chunk | `chatcmpl` 前缀 | `created`（Unix 秒） |
| Responses 响应 | `resp_` 前缀 | `created_at`（Unix 秒） |
| Responses `message` 输出项 | `msg_` 前缀 | — |
| Responses `reasoning` 输出项 | `rs_` 前缀 | — |
| Anthropic Messages 响应 | `msg_` 前缀 | — |

规则，按优先级：

1. **传播优先**：上游提供的响应 ID 原样转发。唯一例外是 Anthropic Messages 的 `id` 槽：外来命名空间的 ID（如 OpenAI `resp_…`、`chatcmpl…`）绝不出现在那里——代理代之以生成的 `msg_` ID（因为 Anthropic 客户端以该命名空间为键记录消息日志）。
2. **代理生成的 ID** 必须携带表中目标协议的前缀（前缀及其分隔符，再加唯一后缀，如 `msg_{uuid-simple}`），且每个响应唯一。`*_llm_proxy` 这类常量占位符是违约，包括在上游省略 ID 的降级回退路径中。
3. **时间戳是真实值**：上游提供的时间戳原样转发；合成的时间戳是响应合成时的 Unix 秒。伪造的 `created`/`created_at: 0` 是违约。

合成的 Chat SSE chunk（`*_sse_to_chat_sse`）每个响应生成一个 ID/时间戳，并在所有 chunk 与终态 chunk 间共享，因此同一响应的流式与非流式形态一致。唯一生成的 ID 是 `previous_response_id` 的前提：常量 ID 会使跨会话查找不可能。

## 7. 目标 Rust 配置结构（Target Rust Config Structures）

目标 provider 形态使用直接可选协议字段：

```rust
pub struct ProviderConfig {
    pub auth: Option<AuthConfig>,
    pub product: String,  // 产品级标识（如 "deepseek"、"kimi"），默认 "custom"；同产品的 provider 可批量 fallback
    pub openai_chat: Option<EndpointConfig>,
    pub openai_responses: Option<EndpointConfig>,
    pub anthropic: Option<EndpointConfig>,
    // 可选的 provider 级思考档位词汇；模型级 reasoning_levels 覆盖（整表替换）。
    pub reasoning_levels: Option<Vec<ReasoningLevelEntry>>,
    // 注意：思考控制字段（supports_thinking、force_think 等）仅属于 Model（§5.2）。
}

pub enum AuthConfig {
    ApiKeyEnv { env: String },
    OpenAiOAuth { account: String },
    None,
}

pub enum Protocol {
    OpenAiChat,
    OpenAiResponses,
    Anthropic,
}

pub enum EndpointConfig {
    Native {
        url: String,                  // 完整上游 URL
        compat: Option<CompatConfig>, // 缺席表示默认 compat 行为（§5.1）
        evidence: EndpointEvidence,
        anthropic_family_models: Vec<String>,  // glob 匹配哪些 upstream model 走 Anthropic Messages 转换
        store: Option<bool>,                    // 强制 store 值（如 ChatGPT backend 强制 store: false）
    },
    Derived {
        derive_from: Protocol,        // 同级协议字段；适配器由协议二元组推断
    },
}

pub enum EndpointEvidence {
    SourceBacked,
    PendingE2E,
    PrivateBackend,
    UserCustom,
}
```

用户侧 TOML 仍使用 `url` 与 `derive_from` 字段，因为该形态简洁。加载后的运行时配置应把它规范化为枚举，使原生/派生互斥在校验后不可能被违反。同理，用户侧的 `api_key_env` 或 `auth = {...}` 被规范化为 `AuthConfig`；请求时代码不得检查原始认证字符串。`EndpointEvidence` 由内置目录元数据或用户/自定义来源生成；它不是协议行为开关。它存在的目的是让 `connect`、`status`、发布门禁与测试能够区分稳定的 source-backed 端点与 `pending-e2e` 或 private-backend 端点。

`ProviderConfig` 上字段存在即表示 provider 支持该协议。唯一性是结构性的——Rust 结构体字段定义上就唯一。

目标模型形态使用按协议的 provider 绑定列表：

```rust
pub struct ModelConfig {
    pub description: Option<String>,
    pub context_window: i64,
    pub max_output_tokens: i64,
    pub features: Vec<String>,  // 如 ["tool_call", "image_input"]，用字符串而非枚举
    pub providers_by_protocol: BTreeMap<Protocol, Vec<ModelProviderBinding>>,
    // 思考档位词汇：允许档位 + 映射的单张表。
    // Model 有表 → 只用 Model 表（整表覆盖）；Model 无表、Provider 有表 → 用 Provider 表；都没有 → 不门禁，恒等透传。
    pub reasoning_levels: Option<Vec<ReasoningLevelEntry>>, // level + 可选 api_value；取代旧的 supported_reasoning_levels + reasoning_level_map
    pub default_reasoning_level: Option<String>, // 客户端未发 reasoning_effort 时使用；必须是生效表中的活动档
    // 思考控制（§5.2）：能力与策略两个维度，全部仅属于 Model。
    pub supports_thinking: Option<bool>,           // 能力：模型能否思考
    pub force_think: Option<bool>,                 // 策略：强制开（仅 Some(true) 激活）
    pub force_no_think: Option<bool>,              // 策略：强制关（仅 Some(true) 激活）
    pub default_think: Option<bool>,               // 策略：可选默认开/关
    pub chat_reasoning_off_format: Option<String>, // 方言：Chat 关思考表达格式（白名单："enable-thinking"）
    // 路由模型：存在 → 本 Model 是转发跳，必须
    // 不声明任何执行层字段（无 provider 绑定、无 reasoning_levels、
    // 无 features/cooldown/思考策略/service tier、无非零窗口）。
    // 显式空列表是加载错误；应省略该字段。
    pub reasoning_routes: Option<Vec<ReasoningRouteEntry>>,
}

pub struct ReasoningRouteEntry {
    pub level: String,   // 本路由匹配的思考档位
    pub target: String,  // 要转发到的前端模型 ID
}

pub struct ModelProviderBinding {
    pub name: String,
    pub model: String,
}

pub enum ModelFeature {
    ToolCall,
    ImageInput,
    DocumentInput,
    VideoInput,
    StructuredOutput,
}

pub enum ModelMetadataSource {
    BuiltInResearch { doc: String, refreshed_at: Date },
    DynamicProbe { provider: String, source_url: String, probed_at: DateTime },
    UserOverride,
}

pub enum ReasoningLevel {
    None,
    Minimal,
    Low,
    Medium,
    High,
    XHigh,
    Max,
}

pub enum ThinkingFormat {
    // Chat Completions 端点的 thinking 格式
    ReasoningEffort,              // OpenAI 标准：reasoning_effort: "low|medium|high"
    DeepSeek,                     // DeepSeek/Qwen/StepFun Chat：enable_thinking: true
    MimoThinkingToggle,           // MiMo Chat：thinking: {type: "enabled"}（无 budget_tokens）
    ZhipuThinking,                // Zhipu Chat：thinking: {type: "enabled", clear_thinking: false}
    KimiReasoningEffort,          // Kimi K3 Chat：reasoning_effort: "low|high|max"
    Zai,                          // Z.ai Chat：同 DeepSeek 格式

    // Anthropic Messages 端点的 thinking 格式
    AnthropicThinking,            // Anthropic/MiMo Anthropic/Zhipu Anthropic：thinking: {type: "enabled", budget_tokens: N}

    // OpenAI Responses 端点的 thinking 格式
    OpenaiResponsesReasoning,     // OpenAI/MiMo Responses：reasoning: {effort: "low|medium|high"}

    // OpenRouter 特殊格式（透传上游）
    OpenrouterChatReasoningDetails,
    OpenrouterResponsesReasoning,
    OpenrouterAnthropicMessages,

    // Qwen 特殊格式
    Qwen,                         // 百炼 Chat：enable_thinking: true（同 DeepSeek）
    QwenChatTemplate,             // Qwen chat-template 格式
    QwenChatTemplateKwargs,       // Qwen chat-template-kwargs 格式
    QwenEnableThinking,           // 显式 enable_thinking（同 DeepSeek）
}

pub enum MaxTokensField {
    MaxTokens,
    MaxCompletionTokens,
    MaxOutputTokens,
}
```

来自 TOML 的字符串值在配置边界解码为这些枚举。运行时代码应对枚举做 match，而不是裸字符串。

用户自定义模型不要求 `metadata_source`，但每个内置或探测生成的模型都应携带足够的出处信息，使 `status`、`connect` 与文档/调试输出能解释其上下文/features 来自内置调研、带时间戳的动态探测，还是显式用户覆盖。这是对提供方调研现实的设计回应：并非所有模型能力表都完整，且 OpenRouter/Ollama 等提供方刻意是动态的。

### Raw 与运行时配置边界

llm-proxy 应把用户侧 TOML 解码形态与运行时配置形态分开：

```text
TOML -> RawConfig -> 校验/规范化 -> Config
```

`RawConfig` 可以用 `Option<T>` 与字符串，因为它镜像 TOML。运行时 `Config` 应使用已解析的布尔、已解析的枚举、已校验的 URL，以及表达互斥状态的枚举变体。请求时代码不应反复套默认值或解析字符串。

实现约束：

- `RawEndpointConfig` 可含 `url: Option<String>`、`derive_from: Option<String>` 与 `compat: Option<RawCompatConfig>`。
- 运行时 `EndpointConfig` 为 `Native { url, compat, evidence } | Derived { derive_from }`。
- `RawCompatConfig` 可含可选布尔/字符串；运行时 `CompatConfig` 使用 `Option<T>` 类型并提供带默认值的 `effective_*` 访问器。
- `RawModelConfig` 镜像 TOML 的按协议字段，例如 `openai_chat_providers`、`openai_responses_providers` 与 `anthropic_providers`，外加 `features`、思考档位的 TOML 字符串。规范化把这些字段转成 `providers_by_protocol: BTreeMap<Protocol, Vec<ModelProviderBinding>>`。
- `supports_developer_role = true`、`max_tokens_field = "max_completion_tokens"` 之类的默认值通过 `effective_*` 访问方法施加，而不是散落在代理代码里。
- 派生端点在运行时不能携带 compat，因为配置形态中没有该字段。

**设计决策**：配置字段保持 `Option<String>` 而非闭合枚举。理由：上游格式变化频繁（DeepSeek、OpenAI、Google、Kimi 各自有 thinking 格式），闭合枚举会导致 serde 反序列化失败 → config 加载失败 → 代理无法启动；`Option<String>` + 白名单校验已能捕获非法值，且允许未知值透传平滑降级。未来若行业格式标准化，可再改为枚举。

实际运行时结构：

```rust
pub struct CompatConfig {
    pub supports_developer_role: Option<bool>,
    pub supports_reasoning_effort: Option<bool>,
    pub thinking_format: Option<String>,
    pub max_tokens_field: Option<String>,
    pub force_stream: Option<bool>,
    pub strip_max_output_tokens: Option<bool>,
    pub must_not_store: Option<bool>,
}

impl CompatConfig {
    pub fn effective_supports_developer_role(&self) -> bool {
        self.supports_developer_role.unwrap_or(true)
    }
    // ... 其他 effective_* 方法
}
```

### thinking/reasoning 的字段层级归属

思考相关字段住在两个层级，因为它们描述两件不同的事：

- **端点 `compat`**（`thinking_format`、`supports_reasoning_effort`）：上游 API 表面的属性。同一 provider 不同端点可使用不同参数名——StepFun 的 Chat 端点收 `reasoning_effort`，其 Anthropic 端点收 `output_config.effort`——因此它们不能住在 provider 或模型层。
- **Model 条目**（`supports_thinking`、`force_think`、`force_no_think`、`default_think`、`default_reasoning_level`、`reasoning_levels`、`chat_reasoning_off_format`）：前端模型背后上游模型的思考能力、策略与档位词汇。所有思考控制字段仅属于 Model（§5.2）；Provider 不得声明它们。档位词汇按模型不同（DeepSeek 只收 `high`/`max`，OpenRouter 收六档，ChatGPT 订阅四档），因此它们不能住在端点或仅 provider 层。`reasoning_levels` 是同时表达允许档位与映射的单张表。
- **Provider 级可选默认**（`reasoning_levels`）：当某 provider 的所有模型共享同一词汇（如 DeepSeek）时，在 provider 级声明一次这张表可避免在每个模型上重复。解析：Model 有表 → 只用 Model 表（整表覆盖，不是逐行合并）；Model 无表、Provider 有表 → 用 Provider 表；都没有 → 不门禁，恒等透传。

绑定跨越档位词汇不兼容上游的前端模型，在信任契约（§5）下是配置错误；目录与 `connect` 只创建词汇兼容的绑定。

## 8. 设计不变式（Design Invariants）

随 llm-proxy 演进，以下不变式应持续成立：

1. 只有当 provider 为绑定的协议声明了端点字段时，模型 provider 绑定才合法。
2. 即使上游不原生支持某协议，provider 也可以声明该协议端点字段，但只能通过声明合法的派生端点。
3. 转换细节住在 provider 端点配置里，不在模型绑定里。
4. 运行时协议转换必须遵循已配置的 provider 端点声明。
5. 启动校验应在处理任何请求前捕获不可能的 model/provider/协议组合。
6. 本地路径 → 协议映射是代码中的固定客户端契约；不存在 `routes` 节——与路由相关的配置只有 providers 与 models。
7. 前端模型 ID 与上游模型 ID 保持分离。
8. 未知的被请求模型 ID、以及已知但缺少该请求协议绑定的模型，都产生显式的客户端协议形态 4xx 错误；无静默默认替换。
9. 能力声明只住在模型上（`features`）；它是与运维者的信任契约，绝不向上游探测。
10. ~~闭合的协议/配置变体应表示为 Rust 枚举~~ **已决策**：配置字段保持 `Option<String>` 而非闭合枚举。理由：上游格式变化频繁，闭合枚举会导致 config 加载失败；`Option<String>` + 白名单校验已能捕获非法值，且允许未知值透传平滑降级。
11. 任何有副作用或影响可用性的操作，收尾只能落在**成功 / 失败 / 不可判定**三态之一，且“不可判定”必须先用改变信息来源的手段复核终态，再保留现场报错——永不折叠成“成功”（§11.9.1）。
12. **启动阶段零静默**：影响对外契约或安全边界的准备动作失败时，进程必须非 0 退出、必须留下日志事件与可执行提示、必须撤销已写入的对外可见痕迹；走了非常规分支（如管理 socket 路径回退）也必须留痕（§11.9.2）。运行时允许降级继续，但必须留痕，且权限 / 认证 / 单一写者 / 凭据不落盘属不可降级项（§11.9.3）。

## 9. 示例：为什么端点声明重要

无效：

```toml
[providers.provider-a]
api_key_env = "PROVIDER_A_API_KEY"

[providers.provider-a.openai_chat]
url = "https://api.example.com/v1/chat/completions"

[models.model-a]
anthropic_providers = [
  { name = "provider-a", model = "upstream-model-a" },
]
```

原因：`provider-a` 没有 `anthropic` 端点字段。

有效：

```toml
[providers.provider-a]
api_key_env = "PROVIDER_A_API_KEY"

[providers.provider-a.openai_chat]
url = "https://api.example.com/v1/chat/completions"

[providers.provider-a.anthropic]
derive_from = "openai_chat"

[models.model-a]
anthropic_providers = [
  { name = "provider-a", model = "upstream-model-a" },
]
```

原因：`provider-a` 显式声明它能提供 Anthropic 协议，并且声明了如何提供：从其 `openai_chat` 端点派生（适配器由协议二元组推断）。

## 10. 产品需求（Product Requirements）

llm-proxy 是完整的本地 LLM 网关，不是受限的概念验证。本节定义所需的产品表面。

### 10.1 支持的本地客户端协议

llm-proxy 必须暴露以下面向客户端的协议族：

| 客户端协议 | 本地路径 | 服务于 |
|---|---|---|
| OpenAI Chat Completions | `/openai/v1/chat/completions` | pi、Qwen Code、通用 OpenAI 兼容客户端 |
| OpenAI Responses | `/responses/v1/responses`、`/openai/v1/responses` | Codex CLI/Desktop 与 Responses 原生客户端 |
| Anthropic Messages | `/anthropic/v1/messages` | Claude Code 与通用 Anthropic 兼容客户端 |
| Claude Desktop 网关 | `/claude-desktop/v1/messages`、`/claude-desktop/v1/models`、可选 `/claude-desktop/v1/messages/count_tokens` | Claude Desktop / Claude Code Desktop 3P 网关档位 |
| 模型列表 | 仅协议前缀的 `/models` | launch 生成的客户端与交互式模型切换 |

这些路径是固定客户端契约，硬编码在服务器中——它们不是用户可配置的路由（见 §4.3）。Provider 端点 URL 面向上游，不得泄漏进客户端 launch 配置。

**无兼容路由**：llm-proxy 只使用协议前缀路由。不存在 `/v1/...` 兼容路由。所有客户端必须使用协议专属前缀（`/openai/v1/...`、`/responses/v1/...`、`/anthropic/v1/...`）。

模型列表语义：

| 路径 | 响应 |
|---|---|
| `/openai/v1/models` | 带 `openai_chat` **或** `openai_responses` 绑定的模型（并集——两种协议都在此前缀下服务） |
| `/responses/v1/models` | 只带 `openai_responses` 绑定的模型 |
| `/anthropic/v1/models` | 只带 `anthropic` 绑定的模型（Anthropic 列表格式） |

带前缀的列表让配置了某一协议前缀的客户端恰好发现它能通过该前缀使用的模型。当 token 认证生效时，模型列表进一步按被认证 token 允许的模型限定；不在 token `allowed_models` 白名单内的模型被排除，以防拓扑与未授权模型泄露。开放模式（未配置 token）或 token 的 `allowed_models` 为空时，返回所有协议匹配的模型。

#### token 作用域能力读出（`GET /v1/token/models`）

对于需要跨协议结构化能力画像与客户端预算的客户端与智能体，`GET /v1/token/models` 提供限定于调用者 token 的统一能力读出：

| 方面 | 契约 |
|---|---|
| 认证 | `Authorization: Bearer <key>` 或 `x-api-key: <key>`。开放模式返回 `token_name: null` 并含所有模型；token 模式对未认证请求返回 `401`。 |
| 模型作用域 | 被禁模型与未授权叶子永不出现在响应中。 |
| 能力单一事实源 | 上下文窗口、最大输出 token、features 与思考档位经代理的核心配置求值器派生。 |
| 路由模型边界 | 路由模型（带 reasoning routes 的 `context_window = 0`）上报所有可达目标共享的能力（`bounds: "derived"`、`routing: true`）：`context_window`、`max_output_tokens`、`features`、路由级词汇（`reasoning_levels`）与默认思考档位。目标能力一致性（§4.2 规则 9）使它们在目标间相同——无需计算最小/交集。原始 `0` 值从不暴露。 |
| 拓扑与密钥隔离 | 内部路由目标（`reasoning_routes[].target`）、provider 名、账号名与上游模型 ID 完全排除在响应载荷外。 |
| 配额豁免 | 可观测性不计量；请求不花费配额点数，即使配额耗尽也成功（`200 OK`）。 |
| 反向代理中立 | 返回结构化 JSON，无硬编码 `base_url` 或客户端配置指引，可无缝部署在任意反向代理与路径前缀之后。 |
| 排序 | 模型按 `id` 升序确定性排序。 |

响应 schema：
```json
{
  "token_name": "client-token",
  "models": [
    {
      "id": "gemini-3.8-flash-lp",
      "protocols": ["openai-chat", "openai-responses", "anthropic"],
      "context_window": 1048576,
      "max_output_tokens": 65536,
      "features": ["image_input"],
      "features_declared": true,
      "reasoning_levels": ["high"],
      "default_reasoning_level": "high",
      "routing": true,
      "bounds": "derived"
    }
  ]
}
```

### 10.2 Provider/产品覆盖

provider 目录必须能表达这些 provider 产品：

- OpenAI PAYG：Chat Completions 与 Responses。
- Anthropic PAYG：Messages。
- DeepSeek：Chat Completions 与 Anthropic 兼容端点。
- OpenRouter：Chat Completions 与 Anthropic 兼容端点。
- 智谱 / BigModel CN PAYG 与 CN Coding Plan；全球 Z.ai 在文档/E2E 刷新前仍为证据门控。
- MiMo PAYG 与 Token Plan；PAYG 对 Chat/Responses/Anthropic 为 source-backed，Token Plan CN 已在三个原生端点上通过主机与密钥验证，SGP/AMS 在获得集群限定密钥前仍为文档支撑。
- 百炼 PAYG 区域变体与 CN Coding Plan。
- Kimi 开放平台 CN PAYG 与 Kimi Code API 作为彼此独立的 source-backed API key 产品；Kimi Code 托管 OAuth 在认证流实现前仍属高级项。
- StepFun PAYG 与 Step Plan，均为 `.ai` 产品，带原生 Chat 与 Messages 端点；在出现原生证据前 Responses 为派生。
- Ollama 本地 OpenAI 兼容 Chat/Responses 与 Anthropic 兼容 Messages 端点；模型清单/能力是动态且本地的。
- 通过 OpenAI OAuth 的 ChatGPT 订阅 provider。

目录条目应按产品分组以优化 `connect` 体验。对每条独立治理的 Product 访问路径，目录安装一个推荐 Provider，包含该 Product 的全部协议端点声明。其模型、端点、探测与请求频率治理因此共享一个边界。高级用户可以显式创建额外的 Provider 实例以实现主备或独立账号策略；这有意地创建独立的认证、探测、限制器、冷却、健康与回退边界。

### 10.2.1 目录更新与 Product 模型选择

目录数据编译进二进制。更新的二进制向首次连接某 Product 的用户提供其更新模板，但绝不自动覆盖或删除既有用户配置。

已配置某 Product 的用户可以显式地再次选择该 Provider/Product 并审阅当前目录模板列表。以该 Product 既有绑定为起点，用户可选择添加新可用的 Models 或移除先前从该 Product 选择的 Models。这是一次显式的配置操作，而非自动目录同步：

- 添加模板只在其前端 ID 尚不存在时创建新 Model；否则在用户确认后只添加或保留所选 Product 的绑定；
- 移除模板在用户确认后只移除该 Product 的匹配绑定；它绝不删除其他 Product 的绑定或 Model 的用户已编辑元数据；
- 目录中被拒绝或过时模板的移除绝不删除用户配置。`provider info`、`model info`、`status` 与探测让用户自行检查并移除不再可用的绑定；
- 默认目录 Model ID 以 `-lp` 结尾，通常在需要时带 Product 消歧后缀（如 `-sub`、`-ag`、`-bailian`、`-zhipu-cn`）。这是 llm-proxy 管理的命名约定，避免与裸上游 ID 冲突并让 Product 选择可见。它不是 schema 归属标志：用户自建 Model 可用任何合法 ID（含 `-lp`），绝不因后缀而被覆盖。

当前目录添加实现已具备第一条不覆盖属性：既有前端 Model ID 保持不变。只增删单一 Product 绑定的显式重选 UI/CLI 是配置管理工作流的必需行为。

### 10.2.2 Provider ID 命名规则

Provider ID（即 catalog product ID 和用户配置中的 provider 实例名）遵循 **平台-产品类型-区域** 命名规则：

| 维度 | 说明 | 示例 |
|------|------|------|
| 平台 | 厂商名称（小写） | `kimi`, `google`, `openai`, `deepseek` |
| 产品类型 | 产品/计费方式简称 | `platform`（开放平台）, `code`（编程订阅）, `payg`（按量）, `sub`（订阅）, `token`（Token Plan） |
| 区域 | 仅在需要区分时出现 | `cn`, `global`, `sgp`, `ams`, `us` |

**规则要点**：
1. 平台名必须体现公司/品牌
2. 同一平台不同区域 = 不同产品实例（如 `kimi-platform-cn` 和 `kimi-platform-global` 是两个独立 provider，因为 API Key 不互通）
3. 产品类型用简称（`sub` 而非 `subscription`，`payg` 而非 `pay-as-you-go`）
4. 无区域区分时省略区域后缀（如 `deepseek` 而非 `deepseek-global`）

**当前 catalog 命名对照**：

| Provider ID | 产品 | 说明 |
|-------------|------|------|
| `deepseek` | DeepSeek API | 单一产品，无区域区分 |
| `openai-payg` | OpenAI 按量付费 | PAYG = Pay As You Go |
| `openai-sub` | OpenAI 订阅 | sub = subscription |
| `kimi-platform-cn` | Kimi 开放平台 CN | 中国区按量付费 |
| `kimi-platform-global` | Kimi 开放平台 Global | 国际区按量付费 |
| `kimi-sub` | Kimi Code 订阅 | 编程专用订阅制（Kimi 订阅计划额度） |
| `mimo-token-plan-cn` | MiMo Token Plan CN | CN 区域 Token Plan |
| `mimo-token-plan-sgp` | MiMo Token Plan SGP | SGP 区域 Token Plan |

### 10.3 Provider 配置字段

Provider 配置由以下部分组成：

| 字段 | 含义 |
|---|---|
| provider 表键 | provider ID（product/region/account 作用域） |
| 协议端点字段 | provider 能服务哪些协议；字段存在即能力声明 |
| 原生端点 `url` | 完整上游 URL，支持环境变量覆盖 |
| `api_key_env` | 简写的 API key 环境变量引用 |
| `auth` | 可选的类型化认证声明，用于 OAuth/无密钥/高级 API key 场景 |
| 原生端点 `compat.thinking_format` | provider 的思考参数格式 |
| 原生端点 `compat.supports_reasoning_effort` | 上游是否接受 `reasoning_effort` |
| 原生端点 `compat.requires_reasoning_content_on_assistant_messages` | assistant 工具调用轮是否必须携带 reasoning_content |
| 原生端点 `compat.max_tokens_field` | 上游最大输出 token 的字段名 |
| `reasoning_levels` | 允许档位 + 映射的单张表；Model 或 Provider 级，整表覆盖 |
| 思考控制（`supports_thinking`、`force_think`、`force_no_think`、`default_think`、`default_reasoning_level`、`chat_reasoning_off_format`） | 仅属于 Model 的字段（§5.2）；Provider 不得声明 |

环境变量端点覆盖必须保持可行。`${ENV}|default` 完整 URL 形式直接映射到端点 `url` 字段——无需 base/path 拆分。用户侧不变式是：自定义端点 URL 与自定义 provider 仍可从 CLI 和 TUI 配置。

OAuth 凭证状态不是 provider 配置。llm-proxy 在应用配置目录下使用一个统一的 OAuth token 存储：`$XDG_CONFIG_HOME/llm-proxy/oauth_accounts.json`，类 Unix 系统回退 `~/.config/llm-proxy/oauth_accounts.json`，Windows 上用平台配置目录等价路径。该存储包含所有 OAuth 支撑产品的类型化账号记录：

```json
{
  "version": 1,
  "accounts": {
    "openai-subscription": {
      "kind": "openai_oauth",
      "account_id": "...",
      "email": "...",
      "refresh_token": "...",
      "authenticated_at": 123,
      "last_refresh": 456,
      "expires_at": 789
    }
  }
}
```

OAuth 存储加载/写入规则：

- 未知的顶层 `version` 在使用 token 前以可操作的错误失败；
- 未知的账号 `kind` 对无关 provider 被忽略、由 `status` 显示为不支持，但 `auth.account` 解析到该记录的 provider 不能使用它；
- provider 认证类型与存储记录 `kind` 必须匹配才能使用 token；
- 账号记录缺失意味着未认证，而不是无效配置；
- 刷新/写入路径必须用异步互斥锁或文件锁串行化读-改-写操作，避免丢失更新；
- 写入使用临时文件 + fsync + rename，并保留严格的权限；

### 10.4 Model 目录覆盖

模型元数据语义：

- 前端模型 ID 稳定且与上游模型 ID 分离；
- `description`、`context_window`、`max_output_tokens`、`features` 与 `metadata_source`；
- `image_input`、文档/PDF 输入、`tool_call`、思考/推理相关特性开关；
- 模型级思考档位映射、支持的思考档位、默认思考档位与思考默认（档位分配理由见 §7）；
- `connect` 用于预选或安装推荐模型的产品默认标记；
- 用于回退的按协议 provider 绑定顺序。

schema 允许一个前端模型按协议/provider 绑定由不同的上游模型名服务。

对发布构建的内置项，launch 生成使用的每个非保守模型能力都必须有出处路径：提供方调研文档、官方快照、动态探测缓存条目，或显式用户覆盖。若上下文/最大输出/能力证据缺失，内置目录必须在生成的 launch 元数据中省略该字段/特性，或将产品/模型标记为 advanced/证据门控。不要用模型名猜测填充未知的上下文、最大输出、图像/视频/文档、工具或推理能力。

调研完整性因此是分层的，而非二值的：

| 目录层级 | 端点证据要求 | 模型元数据要求 | 能否默认生成？ | 示例 / 当前备注 |
|---|---|---|---|---|
| `stable-default` | 完整原生 URL 或由当前提供方调研、官方快照或 E2E 支撑的合法派生端点 | 所选默认模型具有 source-backed/probe-backed 的上下文、必要时的最大输出、输入模态、工具与 launch 所用推理元数据 | 能 | DeepSeek PAYG、Kimi Code API 所选模型、MiMo PAYG、带动态本地模型的 source-backed Ollama 端点 |
| `stable-endpoint-partial-models` | 端点图为 source-backed | 只知道保守的模型元数据子集；未知特性保持不宣传，launch 省略派生能力字段 | 能，但仅限已知必需客户端元数据的模型 | OpenAI PAYG、Anthropic PAYG、百炼 PAYG/Coding Plan、在捕获更丰富模型表/E2E 前的智谱 CN |
| `dynamic-catalog` | provider 模型列表/探测端点为 source-backed | 模型条目来自带出处与时效的显式探测缓存 | `connect`/刷新写入确定性本地元数据后能 | OpenRouter、Ollama |
| `advanced-requires-verification` | 端点/产品证据不完整、推断、区域/账号相关或待 E2E | 模型元数据可能不完整或受账号门控 | 不能作为稳定默认；在高级/自定义 UI 后带警告暴露 | MiMo Token Plan SGP/AMS、百炼 Token Plan、E2E 前的智谱 Coding Plan Anthropic、全球 Z.ai |
| `private-backend` | 观察到的源/E2E，而非公开稳定 API | 模型元数据可能在服务端漂移，必须在临近发布时刷新 | 仅 opt-in，带认证/status 探测 | ChatGPT Subscription/Codex 后端 |

发布门控规则：内置 provider 可以在穷尽调研每个上游模型之前发布，但前提是所发布的模型条目遵守上述层级。端点支持与模型能力支持是两回事。provider 可以端点完备而模型能力不完备；此时 launch 生成只能暴露已验证子集，并在 `connect`/`status` 中浮出缺失证据，而不是伪造默认值。

目录层级与端点证据作为两个维度组合：目录层级是产品/模型发布政策，而 `EndpointEvidence` 是逐端点出处。例如，一个 `stable-endpoint-partial-models` 产品可以含 source-backed 的 Chat/Responses 端点但只暴露保守模型子集；一个 `advanced-requires-verification` 产品可以含 SourceBacked 的 Chat 端点与 `PendingE2E` 的 Anthropic 端点；一个 `private-backend` 产品即使当前 E2E 通过也应使用 `PrivateBackend` 原生端点。

### 10.4.1 网关/本地提供方的动态模型发现

动态模型目录：

- 某些 provider 不应用静态穷举的内置模型表来表达。OpenRouter 与 Ollama 是头等例子。
- OpenRouter 模型条目必须通过显式发现（`GET https://openrouter.ai/api/v1/models`）获取，并带时间戳/来源元数据缓存。探测行提供上游模型 ID、`context_length`、输入/输出模态、`supported_parameters`、provider 最大输出与可选推理元数据。用户选择的 ID 原样存储。
- Ollama 模型条目必须通过显式发现从已配置的守护进程获取（`GET /api/tags`，广泛列表可用 OpenAI 兼容的 `/v1/models`），并对所选 tag 用 `POST /api/show`。`/api/show` 提供能力、详情、如 `num_ctx` 之类的参数，以及可用时的 `model_info.*.context_length`。
- 动态发现只由 `connect` 或 `status --probe`/未来的显式刷新命令触发。launch 命令与默认 `status` 读取已配置模型条目与缓存元数据；它们不得执行突发的网络/本地守护进程探测。
- 探测得来的元数据一旦写入配置/缓存仍是信任契约。运行时请求校验使用本地模型条目，不做实时上游探测。
- 缺失的动态元数据必须保持未知/不宣传，除非用户提供显式覆盖。不要从提供商品牌或模型名子串推断图像/文件/工具/推理支持。
- 探测缓存应位于用户自写配置之外，放在应用 state/cache 目录下（如类 Unix 的 XDG state/cache 与 Windows 的平台专属应用数据），使刷新波动不重写手工维护的配置。
- 探测缓存应存储规范化行，而不是把原始提供方 JSON 作为唯一事实源。最小缓存形态：
  - `provider_id`、`source_url`、`probed_at`、`expires_at` 或 `stale_after`；
  - 上游 `model_id`、可用时的显示名/描述；
  - `context_window`、`max_output_tokens`、输入/输出模态、支持的参数/features、思考档位/默认值；
  - 用于调试的 `raw_ref` 或可选原始载荷，已脱敏凭证。
- `connect` 可以把所选探测得来的模型带 `metadata_source = DynamicProbe { ... }` 写入配置；或者把从缓存生成的模型条目保留为 llm-proxy 自有节。两种情况下，launch 命令都必须有确定性本地元数据，不依赖实时探测。

### 10.5 运行时需求

运行时必须覆盖以下行为组：

1. **协议转换**
   - OpenAI Chat ↔ Anthropic Messages。
   - OpenAI Chat ↔ OpenAI Responses。
   - Anthropic Messages ↔ OpenAI Responses。
   - 非流式与流式变体。
2. **工具使用与多轮状态**
   - function/tool 声明；
   - 工具调用输出/结果映射；
   - 工具调用 ID；
   - assistant 工具调用历史；
   - 空 assistant 消息处理；
   - 归一化的工具参数/输入对象处理。
3. **多模态与文档输入**
   - 图像 URL 与 base64/data URI 输入；
   - source-backed 的音频与视频 content-part 形态；
   - OpenAI、Responses 与 Anthropic 形态下的文档/PDF/文件块；
   - 能力门控遵循 §5 的信任契约：包含图像/音频/视频/文档输入的请求只在模型未声明相应 `image_input`/`audio_input`/`video_input`/`document_input` 特性时才被本地拒绝；声明被信任，从不探测；
   - 无图像生成工作流。
4. **思考/推理适配**
   - 客户端 `reasoning_effort` / Anthropic `output_config.effort` 档位转换：`thinking` 与 `effort` 是两个独立旋钮；`thinking.type=adaptive`/`disabled` 管开关，`output_config.effort` 管力度；`enabled` 已废弃，本路径不支持；
   - provider 特有字段，如 DeepSeek/Qwen/StepFun/Gemini 思考格式，经 `compat.thinking_format` 按端点选择；每个值表示该上游自己的请求/响应方言，不得仅因另一 provider 输出形态相似而复用；
   - `none` 是禁用思考的协议中立客户端意图。有效的 `enable_thinking = false` 与 provider/model 策略表达相同意图。对 `compat.thinking_format = "deepseek"` 的 OpenAI Chat 原生端点，最终上游 body 必须移除通用 reasoning 字段并包含 `thinking: {"type":"disabled"}`。其他格式需要各自的 source-backed 映射，不得接收 DeepSeek 对象；
   - 经 `reasoning_levels` 单表的逐档 API 值映射（Model 或 Provider 级，整表覆盖不按行合并）；表里有的活动档允许并按 `api_value` 映射，未列出的活动档 → 400。没表 = 不声明、恒等放行。关意图（`none`/`disabled`）不走活动档门禁；
   - `thinking.type=disabled` + `output_config.effort=E` 是合法组合（关思考仍控输出力度）；转 Chat/Responses 时保关丢档（`none`，degraded）；
   - 按提供方兼容性可选地剥离或保留 reasoning 内容；
   - 多轮工具工作流所需的 reasoning 内容缓存行为。
5. **回退与冷却**
   - 每模型/协议的有序 provider 尝试；
   - 按失败类别的重试政策（传输错误用 `max_retries`；首个下游字节前的 HTTP 5xx 用 `server_error_retries` + `server_error_retry_interval_ms`）；
   - 冷却持久化；
   - 限速的 `Retry-After` 处理；
   - 分级全冷却：瞬时类（`network` / `server_error` / `rate_limit`）仍探测；持久类（`auth_error` / `model_unavailable` / `client_error`）以 `503` + `Retry-After` 与原因文本 fail-fast；
   - token 作用域的 `client_error` 条目：冷却键携带可选的前端 token 维度；当每个候选都*对请求 token* 冷却时，该 token 看到同样的全冷却 `503` + `Retry-After` 形态，而其他 token 不受影响、provider 本身不被冷却；`cooldown clear` 同时清除 token 作用域条目；
   - 双标识 token 冷却契约：为防止 token 改名导致活动冷却孤立化并绕过限制，冷却条目与键携带双标识：(1) `token`（可变的人类可读名，用于日志、CLI 检查与错误上报）与 (2) `token_key_hash`（`token.key` 不可变的 32 十六进制 SHA-256 前缀，非敏感/不可逆，用于底层条目匹配）。两侧都带 hash 时按 `token_key_hash` 匹配（免疫 token 改名）；缺少 hash 的遗留条目回退按 `token` 名匹配。运行时在启动与配置重载时对账内存与持久化冷却（`cooldowns.json`），自动更新显示名并回填遗留 hash。CLI `llm-proxy token rename <key> <new-name>` 与委托的 `/admin/token/rename` 端点在锁下原子地更新 `tokens.toml` 并同步持久化冷却条目；
   - 认证错误可操作，不应错误地毒化 provider 冷却；401/403 回退不冷却，402 用 `auth_error_seconds`；
   - 流重放边界：安全重放窗口持续到代理向客户端写入**第一个下游字节**。在那之前——上游错误状态、网络错误或超时——重试/回退/冷却与非流式完全一致，跨所有入站协议统一。第一个下游字节之后，无重试/回退/冷却：错误被透传，由客户端决定是否重试；
   - 超时：`timeout_seconds`（默认 300s，5 分钟）约束建连；`max_timeout_seconds`（默认 600s）约束非流式请求整体；流式请求无整体超时——长流会话是合法的；
   - 流中断计数：第一个下游字节之后的流中上游失败绝不重试/回退，但重复中断是 provider 健康信号。仅当同时满足以下全部条件才计一次中断：(a) 已向下游写入至少一字节，(b) 上游连接失败，(c) 下游客户端上下文仍存活（客户端主动断开从不计）。阈值：滑动 10 分钟窗口内每个冷却键（model:provider:protocol）3 次中断触发短暂的网络类冷却（默认 30s），使误报廉价且自愈；
   - 中断错误分类（Layer A）：只计错误指示上游侧原因的中断——HTTP/2 `RST_STREAM`/`GOAWAY`、上游 TLS 关闭、来自上游对端的 TCP RST。指示本地原因的错误（接口 down、`ENETUNREACH`、DNS 失败）从不计。不确定的错误（读超时、意外 EOF）计，但落入廉价的 30s 冷却；
   - 未来工作，不在首版范围：跨连接关联（若到多个不相关 provider 的流同时死亡，怀疑本地网络并对所有人豁免）与事后可达性探测（计数前先对上游 TCP connect）。二者都能进一步收窄残余误报窗口，但要为近乎零的收益增加一个全局连接状态子系统，而误报已经只花一次自愈的 30s。**两者排期前的前提**：一份完整的失败类别设计，同时覆盖计费/配额耗尽错误（402/403/quota 响应——既非限速也非认证失败；冷却时长、回退语义与面向用户的消息都需要专属定义），使这些机制基于完成的错误分类法设计，而不是事后修补上去。
6. **坏请求封禁 / 上游保护**
   - 在重复客户端错误失败后做请求指纹；
   - 指纹按协议与模型为键对**整个请求体**取哈希（§10.5.6）。L1 是全局跨 token 精确重放守卫：字节级相同的重试——观察到的客户端在同一坏请求上循环的失败形态——共享一个指纹，且精确匹配使跨 token 误报保持为零（在全局范围用形状提取的指纹会与结构相同、坏在被排除内容里的合法请求碰撞）。内容有变化的重试刻意不是 L1 的职责：它们积累在逐 token 错误类冷却（见下）里，后者按上游错误的类为键，而不是按请求字节；
   - 计数：当请求**以客户端错误终结**时指纹计数递增——可保护的 4xx（含 400）在候选循环中是终态：指纹被喂入、错误被转发，不再尝试后续候选（§10.5.8 分类表）。若任何候选成功，什么都不计；
   - 在 `window_seconds` 内达到 `max_errors` 时，匹配请求在 `block_seconds` 期间以 429 本地拒绝（默认值见 §3）；
   - 后续回退 provider 成功时避免全局封禁；
   - 前置 Layer 0 入站/出站校验层（§5.1）：结构无效的客户端请求与代理转换 bug 在任何指纹或冷却记账之前被本地拒绝——这包括逐候选转换失败，它们不向上游发送任何东西、不花费配额、冷却或指纹计数；
   - **逐 token 错误类冷却**：除整请求体指纹外，同类重复上游客户端错误（从结构化 `@type`、错误消息的 SHA256 或关键词模式提取）在滑动窗口（`window_seconds`）内按已认证 token **按入站模型**（客户端实际请求的模型；路由模型按自己的条目为键）追踪。计数遵循与坏请求预算共享的单一客户端过错谓词：只有通过 `is_protectable_client_error` 的 4xx（除 401/403/429 外的 4xx）、没有专属冷却路径（**402/404/408**）**且**不是限速类 400（`RESOURCE_EXHAUSTED` detail 类型或 `error.message` 中的限速关键词）才被计入——凭证失败、运营限速与 provider/账号/网络状况（402 计费、404 模型不可用、408 超时）从不进入两本账。按客户端请求计数：每个被计状态在候选循环中都是终态，因此一次失败请求至多贡献一次计数、被回退救援的尝试从不计；每个被转发的 4xx 仍记录 `counted=true|false` 及 `exclude` 原因供取证。当某 token 在 `window_seconds`（默认 600s）内累计 `max_errors_per_class`（默认 3）个同类错误时，该 token 进入 `cooldown_seconds`（默认 300s）的冷却。在 (token, model) 条目冷却期间，该 token**对该模型**的来请求在联系任何上游 provider 之前以 HTTP 429 本地拒绝（`"token blocked after repeated upstream errors; wait for cooldown to expire"`）——同一 token 的其他模型与其他所有 token 完全不受影响；状态跨重启持久化在 `state_dir/token-error-cooldowns.json`（按 token → model 为键；每次变更原子重写；加载时过期窗口丢弃计数、过期冷却释放、空条目删除——因此计数自然在到期时清零），使手工清除与自然到期成为仅有的移除路径。服务管理员可通过 `GET /admin/protection/token-error-cooldown` 检查计数（token + model），并通过 `POST /admin/protection/token-error-cooldown/clear` 提前清除（token 名或 key；字段组合作为过滤器：可选 `model` 把清除收窄到目标的该模型，省略则清除每个模型，`"*"` 选中每个 token：`{"token":"*"}` 清除一切，`{"token":"*","model":m}` 对每个 token 清除模型 `m`）——两者与预算端点一样注册在管理 UDS 路由器与已认证公共路由器上。每个被计错误与每次冷却拒绝发出一行仅含 token 显示名（名字为空时用脱敏 `prefix…suffix`）的 INFO 日志，绝不含 key；
   - **逐 token 本地拒绝容忍（B 档冷却）**：当 A 档（上面的上游错误类冷却）在重复上游客户端错误后紧保护上游账号与 provider 封禁预算（`max_errors_per_class = 3`、`window_seconds = 600`、`cooldown_seconds = 300`）时，B 档提供另一个更宽的本地保护阈值（`[protection.local_reject]`，默认 `max_local_rejects = 10`、`window_seconds = 600`、`cooldown_seconds = 300`），针对不联系上游、本地失败的失控客户端循环。本地拒绝消耗代理 CPU 与网络套接字，却不威胁上游账号处境；因此阈值刻意更容忍（10 对 3），使迭代修改客户端请求载荷、工具 schema 或 prompt 模板的开发者不会被激进冷却，而自动化紧循环失败仍被约束。即使从不碰上游，代价也是真实的：每次本地拒绝都要付入站解析、认证与路由解析，每次被计的拒绝都原子重写 `state_dir/token-error-cooldowns.json`——所有工作都由持有合法 token 的人驱动。把可重复的拒绝类别排除出计数，等于把一个不计价的解析与磁盘写入通道交给持有者，同时关掉约束它的那道闸，因此没有任何本地拒绝类别被豁免。
     - **被计群体**：入站格式/路由拒绝（如未知模型路由、不支持的协议、工具形状校验失败）、Layer 1 指纹 429 封禁，以及已知本地预检（如带 `x-llm-proxy-local-rejection` 标记的 replay 历史无效）。
     - **排除群体**：代理内部转换/适配器缺陷（非客户端过错）、模型作用域 403（权限边界而非请求格式缺陷）、token 配额 429，以及全候选冷却 503。
     - **作用域与触发**：限定在 `(token, 入站模型)`。复用 `state_dir/token-error-cooldowns.json`，`cooldown_reason = "local_reject_tolerance"`。冷却期间，该 (token, model) 的来请求以 HTTP 429 本地拒绝（`"token blocked after repeated bad requests; wait for cooldown to expire"`），与 A 档的上游错误消息明确区分。**反试探披露边界**：该冷却 429（及 A 档的）保持类别级——从不点名触发的错误类、拒绝原因、请求档位、计数、阈值或剩余冷却，这些都会让试探型客户端通过试错映射保护规则集并绕开它；被封的客户端只被告知“重复坏请求导致封禁，等待即可解除”。根因仅服务端运维可见，由上述 WARN 日志行与管理通道的 `GET /admin/protection/token-error-cooldown` 承载。该边界只覆盖封禁类响应：冷却前的功能性 400 按 §5 路由契约保留其全部可操作细节（如 `available routes`）。
     - **隔离**：B 档拒绝从不递增 A 档错误类、账号预算或上游指纹观察器；A 档上游错误从不递增 B 档计数器。
     - **清除**：通过 `POST /admin/protection/token-error-cooldown/clear` 提前手工清除，对过滤选中的 token 与模型目标同时解除 A 档与 B 档冷却；
7. **OAuth providers**
   - OpenAI Device Code / ChatGPT 订阅 token 生命周期；
   - 支持处 401 时的 token 刷新；
   - 认证状态在 `status` 中展示，并通过 `provider` 命令/TUI 管理（`provider login/logout/relogin/refresh/info`）。
8. **错误映射**
   - 保留客户端协议形态的错误响应；
   - 一致地映射上游认证/限速/服务器/网络/模型不可用错误；
   - 所有 provider 都失败时优先给可操作的认证/付款错误。
   - 失败分类，统辖原则是结构有效的请求总是被转发、失败在上游响应后分类：

     | 上游信号 | 类别 | 同 provider 重试 | 回退 | 冷却 |
     |---|---|---|---|---|
     | 网络错误 / 408 | `network` | 是（`max_retries`） | 是 | `network_seconds`（默认 30s）；瞬时——全冷却仍探测 |
     | 5xx | `server_error` | 是（`server_error_retries`） | 是 | `server_error_seconds`（默认 300s）；瞬时——全冷却仍探测 |
     | 429 | `rate_limit` | 否 | 是 | 有 `Retry-After` 则用之（封顶 24h），否则配额 `reset_time`，否则 `rate_limit_seconds`（默认 300s）；瞬时——全冷却仍探测 |
     | 404 | `model_unavailable` | 否 | 是 | `model_unavailable_seconds`（默认 1800s）；持久——全冷却 fail-fast |
     | 400（及其他可保护 4xx，除 401/402/403/404/408/429） | `client_error` | 否 | **否**——请求级错误：在另一 provider 重试会把同一坏 body 发给上游。喂入指纹并立即返回 | `client_error_seconds`（默认 300s）；对 token 认证前端，条目限定在 **(token × provider) 对**而非 provider——其他 token 继续服务；开放模式请求保持 provider 全局条目。持久——在被冷却范围内全冷却 fail-fast |
     | 401 | `auth` | 否 | 是 | **无**——token 无效是账号状态问题；回退到下一 provider，但不要用冷却毒化这个 |
     | 403 | `auth` | 否 | 是 | **本轮无**——RFC 9110 只保证“理解但拒绝”；provider 用 403 表示区域、权限、UA 白名单*以及*配额，因此仅凭状态码不是可靠的持久失败信号 |
     | 402 | `auth_error` | 否 | 是 | `auth_error_seconds`（默认 300s）——保留付款/计费/额度耗尽是唯一在已调研 provider 间一致映射的 4xx；持久——全冷却 fail-fast |
     | 首个下游字节后流中断 | `stream_interrupted` | 否 | 否 | 无（除 §10.5.5 的重复中断计数器） |

     全冷却 fail-fast 返回客户端协议形态的 `503`，带 `Retry-After`（最早剩余秒数）与点名模型、每个 provider、剩余秒数与冷却种类/原因的文本。空候选列表是配置错误（`model X has no usable provider binding`），不是冷却。部分冷却成功只记日志；客户端响应不变。

     限速冷却遵循差异化回退链：
     1. `Retry-After` 头：若上游响应携带合法 `Retry-After` 头（秒数或 HTTP-date），它跨所有 provider 最高优先（封顶 24h）。

     401/403/402 **不是**统一的产品分类法。RFC 9110 只保证 401=未认证、402=保留付款、403=理解但拒绝。提供方证据：OpenAI 配额是 429；Anthropic 402=计费而花费上限常为 400/429；DeepSeek/StepFun/MiMo/OpenRouter 402=余额/额度；Kimi/Right Code 403 可能意味配额*或*区域/UA。因此本表将它们拆开：402 可冷却，401/403 回退不冷却。provider 特有的配额 body 码仍是未来工作。
9. **状态/探测缓存**
   - 按需缓存 provider/协议/模型的可用性；
   - 默认 status 不做意外的付费探测；
   - 显式刷新命令做真实上游探测。
10. **Provider 请求频率限制 / 反封禁保护**
   - 冷却与指纹封禁在失败后保护上游，但正确的请求也可能压垮或触发模型服务产品的限速。因此 llm-proxy 除反应式冷却外，还支持 provider 作用域的主动请求频率限制。
   - 频率限制住在 llm-proxy **provider** 层，因为在新配置模型中一个 provider 代表具体的产品/账号/区域。该 provider 下声明的所有端点（`openai_chat`、`openai_responses`、`anthropic`、派生端点等）共享一个频率池。这与产品/配额与防滥用系统通常在上游执行的方式一致。
   - MVP 字段作为可选的高级 provider 配置暴露。这是可见但非必需的逃生通道：知道自家产品配额的用户可以覆盖它，但普通生成配置在不需要产品级覆盖时应省略。省略时运行时应用保守的内置默认值。提供方/平台调研以后可能为特定产品收紧或放宽内置默认值；那些调研得出的默认值应编码进目录/connect 生成，而不是强迫每个用户配置写出该块。

     ```toml
     [providers.deepseek.request_frequency]
     requests_per_minute = 60   # 可选；默认 60
     requests_per_hour = 1000  # 可选；提供方调研无小时配额证据时省略
     burst = 5                 # 可选；默认 5；保守的令牌桶容量
     queue_timeout_seconds = 10 # 可选；默认 10；最后一搏跨候选共享等待上限
     ```

   - 默认值刻意保守且对本地智能体友好：
     - `requests_per_minute = 60` 每 provider 频率池；
     - `requests_per_hour` 可选且默认未设，除非提供方/平台调研记录了小时配额或防滥用窗口；
     - `burst = 5` 每 provider 频率池默认值；该字段可选，只应在提供方/平台调研或真实使用表明不同突发额度更安全时调优；
     - `queue_timeout_seconds = 10`（最后一搏共享等待上限，见频率行为）。
   - 理由：编码智能体通常发出低而持续的请求速率，但可能在重试/工具循环中突发。`60 rpm` 允许正常交互使用，同时让意外循环在看起来像滥用之前自我节流。`burst` 作为可选高级旋钮暴露但保守默认 `5`，因为 provider 产品通常公布持续请求窗口而非突发容量。有些产品发布或执行更长的滚动窗口（例如小时请求配额），因此 `requests_per_hour` 作为由提供方调研驱动的可选第二桶存在。这些默认值不是 provider 权益声明；已知产品配额的用户可以调高/调低。
   - 频率行为：
     - 限制器在发送任何上游请求之前、以及每次重试/回退尝试之前检查；
     - 同一 provider 的原生与派生端点消耗同一 provider 级频率池；
     - `burst` 是令牌桶容量，不是每秒请求数也不是并发；它控制可立即消费的预存请求令牌数；
     - 同时配置分钟与小时限制时，一次尝试必须先从两个桶都获得容量才能打上游；
     - 候选遍历零等待：对每个候选单次尝试取频率池容量，无容量立即尝试下一候选，不原地等待；
     - 全部候选均无容量时进入最后一搏：跨候选共享至多 `queue_timeout_seconds` 的总等待预算，按候选原序逐轮扫描全部候选，任一候选先恢复容量即发送；单候选部署下遍历即终点，退化为对该唯一候选等待，与既往行为一致；`queue_timeout_seconds = 0` 表示最后一搏不等待、快路径失败即返回本地 429；
     - 多候选 `queue_timeout_seconds` 配置不一致时，共享预算取其中最大值；已发起发送后的重试序列内取槽沿用各候选自身的 `queue_timeout_seconds` 逐次等待节流，不计入共享预算；
     - 候选在最后一搏中恢复容量并实际发出请求后即退出重放集合（无论其结果如何），最后一搏只继续等待仍处本地频控的候选；
     - 最后一搏预算耗尽仍无容量，以客户端协议形态返回本地 429，不打上游；
     - 因本地频率限制等待或失败不算 provider 失败、冷却或坏请求指纹；
     - 上游 429 的 `Retry-After` 仍按上述定义创建 provider 冷却；
     - 流式请求在流开始时消耗一个频率 token；它们不为每个 SSE chunk 额外消耗频率 token；
     - 除非以后加入显式管理覆盖，状态/探测刷新共享同一 provider 频率池。
   - 更长窗口与非默认 burst 值只应在提供方/平台调研证明其合理时加入。MVP 支持分钟级限制、可选小时级限制与可选 provider 级 `burst`；日/月预算上限、基于 token 的限制、并发限制（`max_in_flight`）、per-model 覆盖、从观察到的 `Retry-After` 学习的自适应限制都是未来工作。除非提供方调研证明同一产品/账号下的端点有独立的上游配额池，否则应避免 per-endpoint 频率覆盖。
11. **Launch 配置生成**
   - Codex CLI、Codex Desktop、pi、Claude Code、Claude Desktop / Claude Code Desktop、Qwen Code 与 OpenCode。
   - 保留用户拥有的配置字段，只替换 llm-proxy 托管区域。
   - 客户端专属的生成能力字段是核心模型元数据的投影。例：Claude Desktop `inferenceModels[].supports1m` 只从解析出的模型 `context_window >= 1_000_000` 生成；llm-proxy 不得添加重复的 `supports_1m` 配置字段，也不得用 `[1M]` 模型名后缀编码能力。
   - **写入者职责**：`serve` bind 成功后，对所有已知客户端的默认配置路径检查文件是否存在，存在则重跑对应的 launch 函数，用实际绑定地址更新 baseUrl。无 registry 文件、无归属判定——ServerConfigLock 的 flock 已保证单实例，launch 的 merge 逻辑天然幂等（覆盖托管字段、保留非托管字段）。设计原则：谁最后负责写入就用谁的，一致性由写入者维护，不推给用户。

## 11. CLI 与服务生命周期设计（CLI and Service Lifecycle Design）

### 11.1 命令面（Command surface）

目标命令面：

```text
llm-proxy [--config-dir PATH]               # 默认后台启动服务
llm-proxy serve [--foreground]            # 显式服务命令；除 --foreground 外均后台
llm-proxy serve --foreground              # 前台运行
llm-proxy init [--config-dir PATH]          # 创建全新 llm-proxy 配置；从不迁移旧 schema
llm-proxy connect [PRODUCT] [flags]       # 用户友好的成熟产品设置；写入前总先验证
llm-proxy status [--probe]
llm-proxy provider                         # 交互式下进入 provider 管理 TUI
llm-proxy provider list                    # 列出 providers
llm-proxy provider info [NAME]             # provider 详情，支持时含用量
llm-proxy provider add [PRODUCT] [flags]   # 创建 provider（交互或非交互）
llm-proxy provider copy SOURCE NAME [flags] # 把 provider 配置拷贝为新 provider ID
llm-proxy provider login NAME              # OAuth 登录
llm-proxy provider logout NAME             # OAuth 登出
llm-proxy provider relogin NAME            # OAuth 重新登录
llm-proxy provider refresh NAME            # 刷新 OAuth access token；仅 OAuth provider
llm-proxy provider remove NAME [--force]   # 删除 provider
llm-proxy provider reset-usage NAME [--force] # 重置用量（OAuth provider）
llm-proxy model                            # 交互式下进入 model 管理 TUI
llm-proxy model list                       # 列出模型
llm-proxy model info MODEL                 # 模型详情
llm-proxy model add MODEL [flags]          # 创建前端模型
llm-proxy model remove MODEL [--force]     # 删除前端模型
llm-proxy model set MODEL [flags]          # 原子地编辑上下文/输出/思考/features
llm-proxy model provider add MODEL --type TYPE --provider NAME [--upstream-model MODEL]
llm-proxy model provider set MODEL --type TYPE --provider NAME --upstream-model MODEL
llm-proxy model provider remove MODEL --type TYPE --provider NAME
llm-proxy model provider move MODEL --type TYPE --provider NAME --to INDEX
llm-proxy launch codex [flags]
llm-proxy launch codex-desktop [flags]
llm-proxy launch pi [flags]
llm-proxy launch claude-code [flags]
llm-proxy launch claude-desktop [flags]
llm-proxy launch qwen-code [flags]
llm-proxy launch opencode [flags]
llm-proxy cooldown list
llm-proxy cooldown set --provider PROVIDER [--model MODEL] --duration DURATION [--protocol PROTOCOL] [--kind KIND] [--reason REASON] [--dry-run] [--yes|-y]
llm-proxy cooldown clear --provider PROVIDER [--model MODEL] [--dry-run] [--yes|-y]
llm-proxy shutdown
llm-proxy restart
llm-proxy doc [--list|--raw|--section SECTION]
llm-proxy version
llm-proxy completion <shell>
```

命令入口区分：

- `llm-proxy connect` 在交互式终端中进入添加 provider 向导（`provider add`）。
- `llm-proxy provider` 进入完整的 provider 管理 TUI。
- `llm-proxy model` 进入完整的模型管理/编辑 TUI。

### 11.2 服务与状态语义

- 默认命令在后台启动服务；前台执行需要 `serve --foreground`（向后兼容别名：`serve --frontend`）。
- `status` 默认读缓存/本地状态，从不执行网络探测；真实上游可用性探测是 `status --probe`。
- `provider refresh NAME` 只是 OAuth 凭证操作：刷新该 provider 账号的 OAuth access token。它不清冷却、不作废活动证据、不改探测缓存、不跑可用性探测；API key 与无认证 provider 被拒绝。
- `status --probe` 是可达性/状态操作：它在活动证据、短缓存、singleflight 与请求频率规则下取得当前最佳探测结果，并在真实探测执行时更新探测缓存。它不刷新 OAuth 凭证、不写活动证据、不清冷却。
- 状态展示每个已声明的 Model。没有 Provider/协议绑定的 Model 显示 `WARN no configured provider bindings; not selected for probe`；这表示它没有探测候选，而不是上游探测失败。有绑定的模型按绑定保留一行状态：依活动/缓存/探测证据为 `OK`、`FAIL` 或 `MISS`。
- 活动证据只由成功的正常代理转发写入，按 TTL 过期。冷却由请求失败策略、显式 `cooldown set` 命令与 `cooldown clear` 维护；上述两个 refresh/probe 命令都不重置这些状态。
- `cooldown set --provider PROVIDER [--model MODEL] --duration DURATION` 注入持久化的 `(model, provider, protocol)` 条目。时长接受人类可读形式（`42h`、`1h30m`、`30m`、`7d`、`151200s`），封顶 `30d`，独立于自动的 24h 上限。省略 `--protocol` 时为每个客户端协议各写一条（`openai-chat`、`openai-responses`、`anthropic`）；省略 `--kind` 默认 `manual`。省略 `--model` 展开为 `config.toml` 中所有引用该 provider 的模型；带 glob 模式（`*`、`?`、`[...]`）的 `--model` 匹配该 provider 下的模型名。批量操作在交互式 TTY 终端提示确认，在非交互环境要求 `--yes` 或 `--dry-run`。`--dry-run` 打印计划操作，不变更状态也不加锁。写入路径与 `cooldown clear` 相同：有运行中服务器则委托 `POST /admin/cooldown/set`，否则离线写 `cooldowns.json`。

#### 11.2.1 OAuth 刷新行为与可观测性规范

- **TUI 批量刷新契约（`[r]`）**：TUI 已配置 Provider 列表界面的 `[r]` 快捷键**仅针对当前配置中已过期的 OAuth Provider 账号**发起刷新批次；未过期账号与非 OAuth provider 不进入候选批次。强制刷新特定账号继续通过 CLI `provider refresh <name>` 执行；
- **结构化日志与脱敏契约**：所有 OAuth refresh 入口统一注入结构化字段（`refresh_id`、`provider_id`、`auth_kind`、脱敏 `account_label`、`trigger`、`phase`、耗时与可重试性）。刷新失败仅记录安全错误摘要，**严禁**在日志或 TUI 错误中打印真实 account ID、access token、refresh token、client secret 或 Authorization 请求头；
- **主链请求前刷新与并发安全**：在 Proxy 转发主链路中，OAuth token 即将过期或已过期时在发送前触发刷新；由 singleflight 机制保证同一账号的并发请求只刷新一次，并在刷新成功后统一注入新 Bearer token 完成转发；上游返回 401 时安全透传，不触发死循环重试。

### 11.3 `init` 默认配置

`llm-proxy init` 创建一个最小的仅 DeepSeek 的 llm-proxy 配置，从不碰已有配置文件。它不迁移遗留 schema，也不启动服务。其他 provider 通过 `connect` / `provider add` 添加。`--config-dir PATH` 选择目标配置目录；省略时使用常规配置目录。缺失的 provider 环境变量不阻塞初始化——命令写入配置并打印所需的 export。

DeepSeek 原生服务 Chat Completions 与 Anthropic Messages（官方文档记载），不服务 Responses。因此生成的 provider 声明两个原生端点与一个派生端点：

```toml
[server]
listen = "127.0.0.1:8989"

[providers.deepseek]
api_key_env = "DEEPSEEK_API_KEY"

[providers.deepseek.openai_chat]
url = "https://api.deepseek.com/chat/completions"

[providers.deepseek.openai_chat.compat]
supports_developer_role = false
supports_reasoning_effort = true
thinking_format = "deepseek"
requires_reasoning_content_on_assistant_messages = true
max_tokens_field = "max_tokens"

[providers.deepseek.anthropic]
url = "https://api.deepseek.com/anthropic/v1/messages"

[providers.deepseek.openai_responses]
derive_from = "openai_chat"
```

注意：`anthropic` 是**原生的**——DeepSeek 官方提供 Anthropic Messages 端点，因此不得从 chat 派生。`openai_responses` 是派生的，因为 DeepSeek 没有 Responses API。原生端点在需要处携带自己的 `compat`；Anthropic 端点用默认值。

生成的模型（镜像目录默认值）：`deepseek-v4-flash-lp` 与 `deepseek-v4-pro-lp`，各自对全部三种客户端协议绑定 `deepseek` provider，并带模型元数据（上下文窗口 1M、最大输出 384k、`tool_call_reasoning` 特性、`default_think = true`、`default_reasoning_level`；思考词汇从 provider 级 `reasoning_levels` 继承）。

设计词汇用 **frontend** 表示前台执行。

### 11.4 后台服务要求

服务模型刻意简单：**每个用户环境一个 llm-proxy 服务实例**。没有按配置的实例派生——运行时路径是全局的，服务存活时第二次启动被拒绝（用 `restart` 切换配置）。

后台模式必须：

- 在 daemon 化/派生前校验配置；
 - 若已存在存活服务则拒绝启动（通过运行时目录中的 pid 文件/管理套接字检测）；
- 按生命周期类别写文件：
  - 运行时：`llm-proxy.pid`、`llm-proxy.sock` 与配置锁，在 `$XDG_RUNTIME_DIR/llm-proxy/<identity>` 或固定回退 `~/.local/state/llm-proxy/runtime`；
  - 持久状态：`llm-proxy.log`、冷却、状态缓存与用量，在 `$XDG_STATE_HOME/llm-proxy/<identity>` 或固定回退 `~/.local/state/llm-proxy`；
  - 配置与 OAuth 账号在 `$XDG_CONFIG_HOME/llm-proxy` 或固定平台配置回退；
- 暴露本地管理通道（上述套接字）用于 shutdown/重启/环境注入**以及只读状态查询**（如 `status` 用的坏请求封禁状态，§11.6）；
- 打印简洁的连接信息与关闭指引；
- `shutdown`/`status`/`restart` 指向已解析 runtime/state 目录中的单一服务；`--config` 选择服务用哪个配置文件，但不创建独立实例。

每个类别只读自己对应的 XDG 变量。变量缺失时用该类别的固定平台默认；没有任何类别通过另一类别的 XDG 变量回退。

`<identity>` 是由规范化配置目录路径派生的 16 字符十六进制哈希。它使多实例共存成为可能——每个配置目录有自己的状态与运行时分片。单实例默认（配置在 `~/.config/llm-proxy/`）解析为固定 identity，因此现有部署不受影响。

`serve --foreground` 必须：

- 在当前终端运行；
- 按常规 CLI 约定向 stderr/stdout 记日志；
- 优雅处理 Ctrl-C；
- 遵循与后台模式相同的单实例规则：服务已存活时拒绝启动，并创建管理套接字（§11.8），使 `shutdown`/`status` 状态查询对前台与后台服务统一生效。

### 11.5 关闭与重启

`shutdown`：

- 向已解析 state 目录中的服务发送本地管理请求；
- 停止接受新请求；
- 有界等待在途请求；
- 结算用量账本：待用量记录刷盘（§12.6）；
- 清理 pid/套接字状态。

`restart`：

- 关闭当前运行的服务；
- 用当前 CLI/配置参数启动新的后台服务；
- 不从旧进程继承过期参数。

### 11.6 status 命令

默认 `status` 必须安全且缓存优先：

- 加载并校验配置；
- 展示服务 pid/套接字状态；
- 展示已配置的 providers/产品/模型；
- 展示 API key/环境变量/OAuth 凭证存在性，不打印密钥；
- 展示活动冷却与坏请求封禁状态；
- 有缓存时展示缓存探测结果；
- 默认绝不发送真实 LLM 请求。

状态可见性跟随状态所在位置：

- **持久化状态**（冷却、探测缓存）直接从磁盘读。
- **进程内状态**（坏请求封禁条目与计数器）通过管理通道（§11.4）向运行中的代理进程查询：**本地部署**下 `status` 向本地管理套接字发只读状态请求并渲染结果，没有服务运行时把封禁节显示为不可用（“service not running”），而不是失败或静默省略；**纯 Remote（client-only）部署**（存在 `[remote]`、不存在 `[server]`，与配置表部署模式定义一致）本地不运行后台进程，Runtime State 节输出客户端模式标注 `client-only (remote) — 本地无后台进程`，不向本地管理套接字发起任何连接、也不出现 “service not running”（原“没有服务运行时”条款收窄为本地部署口径）。进程内状态绝不仅为可观测性写盘；远端进程内状态当前无只读端点，不在 status 展示（已知边界，另案）。

纯 Remote（client-only）部署的元数据渲染源是远端配置镜像：

- `/admin/ping` 成功（远端在线）后，`status` 请求 `GET /admin/config/snapshot`（与 ping/status 同一 Bearer 鉴权与超时预算）。成功输出信息行 `已加载远程配置快照`，并以镜像配置作为 Providers/Models 两节的**唯一渲染源**；端点响应携带的 `revision`（TUI 编辑器 CAS 用）不消费不展示，缺失不构成错误；本地 `providers`/`models` 在该模式下不承载流量、不参与渲染；
- 快照获取失败（网络错误、4xx/5xx、缺 `data.config`、解析失败）必须向 stderr 输出 `获取远程配置快照失败: <错误摘要>`（脱敏，不得携带 Authorization/token）并以**非零退出码终止**（fail-fast）：不渲染半截状态、不回退本地空口径；远端不可达（ping 失败）保持既有宽容口径，不触发快照获取；
- 镜像渲染下 Providers 凭证列不做本地核验：`auth = none` 显示 OK，其余显示 `N/A` 与 `remote-managed`（凭证由远程 Server 维护，本地不适用）；OAuth 凭证与 Cooldowns 两节保持本地持久化状态口径，不随镜像切换；
- `status` 保持只读：不调用任何写端点、不做 CAS。

`status` 与 `status --probe` 必须：

- 接受可选的单值 `--provider <pattern>` 与 `--model <pattern>` 选择器；两者都支持独立使用（只过滤展示不探测）以及配 `--probe`（只探测并展示匹配目标）；重复的标志是解析错误，无选择器保持全展示/全探测行为；
- 支持统一的 Glob 通配匹配（`*` 匹配任意字符序列，`?` 匹配单个字符，精确匹配是零通配特例）；
- 通过共享的 `ProbeSelection` 定义选择绑定：只给 provider 则选其全部绑定，只给 model 则选该前端模型的全部绑定，两者都给取交集，未知 ID / 未闭合 glob 语法 / 已知空交集在 `--probe` 下失败而不是以空结果成功；
- 展示模式下过滤 Provider、OAuth 凭证、Cooldowns 与 Models 各节，只显示匹配实体；无匹配时显示明确提示而非静默省略；
- 使用绑定级 `(provider, frontend-model, protocol)` 选择、对级 `(provider, frontend-model)` 执行与按协议结果。每个被选绑定有一个结果，处置为 `executed`、`not_executed`、`active`、`cached` 或 `coalesced`；`not_executed` 表示本地认证或探测 body/请求计划预检在上游请求之前失败；
- 委托模式下对服务器配置校验选择器。远程最小 CLI 配置可能没有本地 Provider/条目而委托；若该服务器不可用，选择器必须以 `cannot validate probe selection because the configured server is unavailable` 失败，而不是报告空结果或本地臆造的结果；
- 可选选择器 JSON 只发给 `POST /admin/status/probe`：空 body 与 `{}` 保持全探测兼容，非空 body 必须是只含可选 `provider` 与 `model` 的 `application/json` 对象。JSON 校验与选择器校验发生在任何 SSE 流或探测任务创建之前；失败为 HTTP 400；
- 委托的选择器探测必须先过其功能版本门（不支持版本报 `server does not support probe selection`；只有 dev CLI + dev server 用 dev 例外）与常规 CLI/服务器兼容门，才能发送探测请求；
- 把动态模型发现限定在所选绑定集内的 provider，并使用配置关联的 OAuth 账号存储；被排除的 provider 不收到发现请求或缓存更新；
- 只在活动证据、短探测缓存窗口与 singleflight 都未提供可复用结果时，才对已配置/启用的 provider 执行真实上游探测；
- 真实探测执行时更新探测缓存，但绝不创建活动证据、刷新 OAuth 凭证或清除冷却；
- 尽可能使用便宜/最小的模型探测；
- 清楚展示刷新时间与失败；
- 除非被后续标志显式强制，避免探测已禁用或缺凭证的 provider；
- **探测并发按 provider 计**：探测同一 provider 下的多个模型绑定时，这些探测请求与实际业务请求共享该 provider 的 `request_frequency` 并发限制。若该 provider 已因实时流量达到并发上限，探测排队或延后——探测从不绕过或抢占 provider 的速率控制；
- **Listen 端口交叉探测（G2）**：当 `status` 检测到自身处于 not running 状态（无有效 pid 文件）时，主动对配置的 `server.listen` 地址做占用检测；若端口已被监听，输出 `WARN` 提示、占用者 PID、进程名及人工清理引导（`kill <PID>`），避免端口被占却显示 not running 产生排障矛盾。

### 11.7 核心运维命令：`connect`、`provider` 与 `model`

llm-proxy 有三个围绕 provider/模型配置的核心运维命令。三者都必须同时支持 CLI/脚本操作与交互式 TUI 操作：

| 命令 | 主对象 | CLI 角色 | TUI 角色 | 默认写用户配置？ |
|---|---|---|---|---|
| `connect` | 成熟模型服务产品 | `provider add` 的别名 | （委托给 `provider add`） | 是，预览/确认后 |
| `provider` | provider 产品/账号/区域与端点图 | 创建/列表/信息/认证/删除/用量重置 | provider 管理 UI | 写操作是；list/info 否 |
| `model` | 前端模型目录与协议/provider 绑定 | list/info/参数编辑/特性开关/绑定管理 | 模型管理与编辑 UI | 写操作是；list/info 否 |

这些命令共享同一份目录、解析器、动态探测缓存与校验代码。它们的边界是有意的：

- `connect` 是 `provider add` 的 CLI 别名，为脚本便利与既有用户习惯保留。
- `provider` 是 provider 管理的**唯一入口**：创建、查看、认证（OAuth）、删除与用量重置。
- `model` 管理前端模型：查看、参数配置、特性开关与按协议的 provider 绑定。

**详细设计**（命令矩阵、标志语义、TUI 状态机、内部模块与 ChatGPT 用量 API）在 **§12**。llm-proxy 用子命令实现可脚本的 provider/模型操作；裸 `provider` 与裸 `model` 保留给交互式终端的 TUI 入口。

`provider`/`model` 可就相关操作互相交接。TUI 从不显示 API key、bearer token、OAuth refresh token 或原始认证头。

#### 11.7.1 Provider 连接提交边界与原子性

为了避免孤立凭据、状态裂缝与配置所有权破坏，Provider 连接（`connect` 及 TUI `ProductSelection`）遵循严格的原子提交与所有权不变量：

1. **Product 列表无状态与多实例**：Product 列表为纯静态 catalog 模板，不包含 runtime/auth/health 状态，不渲染 `✓ Ready`；同一 Product 允许创建多个不同名称的 Provider 实例。
2. **候选状态暂存与内存验证**：新建 OAuth 凭据在提交前保留在内存（`PendingAccount`）作为候选状态的一部分。验证（Probe 与鉴权检查）针对候选状态执行，`skip_probe: bool` 贯穿全调用链路：
   - `skip_probe == true` 时跳过网络探针与外部鉴权就绪检查（支持未配置环境变量的“延迟配置”场景）；
   - **严格校验底线**：`skip_probe` 绝不绕过基础 Schema 格式校验、Provider 命名冲突检查与 Model binding 引用完整性校验。
3. **提交方向与收敛式恢复**：提交顺序保持“写入 account → commit config”。`accounts.json` 与 `config.toml` 是两个独立文件，二者之间不存在原子提交，因此失败处置按**两半的重建成本**定向：凭据只有用户重新走一遍 OAuth 流程才能得到，配置可由代理重建 ⇒ **commit config 失败时保留刚写入的凭据并响亮报告**（错误消息必须说明凭据已留在磁盘、下次启动会据此补完、无需重新登录），绝不“回滚式删除”——旧规则“立即删除本次刚写入且无引用的 account，达成零孤立账户”销毁的正是不可再生的一半，已作废。丢失的第二步由**启动边界补完**：按 `(OAuth 类型, account 键)` 反查 catalog 模板重建 provider 并提交，因此补完是幂等的、且不需要用户重跑 OAuth。能力边界必须在报告里显式承认：模型选择**不可**恢复（catalog 只能给出候选清单），补出的是“能认证、缺 model”的可用半成品，报告引导 `provider reselect`；account 键不匹配任何 catalog 模板（手工编写的 provider）时**只报告、不臆造** provider 名；多个 provider 共用一个 account 键时只会补出一个。补完只在启动时执行——该边界上本进程必然没有进行中的 connect。
4. **统一所有权调度与禁止本地写盘降级（No-Fallback Invariant）**：
   - 所有提交操作必须经由统一入口调度；
   - **严禁本地降级**：当 Server 已被检测为所有者时，委托发生传输失败、超时、409 冲突或 5xx 错误必须明确失败终止，彻底切除 CLI 越过 Server 本地写盘的逃生路径；达成：0 本地写盘、0 generation 发布。“0 orphan account”不再是达成手段——按第 3 条，残留凭据是**待补完的输入**而非待清理的垃圾。

5. **结构化错误分类与 TUI 门禁**：`/admin/provider/add` 错误响应携带机器可读的 `kind`（`probe_failed` / `auth_not_ready` / `config_conflict` / `invalid_config` / `internal_error`）。TUI 仅在 `probe_failed` 或 `auth_not_ready` 时允许进入 `WarningConfirm`（支持 Continue anyway 与 Back 编辑）；其他硬错误直接终止。

### 11.8 管理通道（Management channel）

管理通道是运行时目录中的 Unix 域套接字（`$XDG_RUNTIME_DIR/llm-proxy/llm-proxy.sock`；`XDG_RUNTIME_DIR` 未设置时回退 `~/.local/state/llm-proxy/runtime/llm-proxy.sock`），以仅属主权限（`0600`）创建，在 UDS 上讲明文 HTTP。Windows 用 TCP localhost 回退：服务器绑定 `127.0.0.1:{management_port}`，其中 `management_port` 来自配置 `[server] management_port`（默认 8989）。绑定地址**硬编码为 `127.0.0.1`**——不可配置——因此管理面无论 `server.listen` 为何（哪怕设为 `0.0.0.0`）都只在 localhost。

**管理通道绑定失败 = 启动失败**：代理必须在绑定管理通道后立即向启动流程回报就绪结果，绑定失败时记录 `management_socket_bind_failed`（含 socket 路径、路径字节数与 OS 错误；`AF_UNIX` 路径超过 `sun_path` 上限 107 字节属此类）并以非 0 退出，同时撤销本次已写入的 pid 文件。不得出现"进程健康但控制面静默缺失"的状态：§11.7.1 第 4 条已禁止 CLI 越过 server 本地写盘，管理通道因此是**唯一的写路径**，静默缺失等于静默砍掉全部写能力，比拒绝启动更糟。旧条款"socket 创建失败则照常启动、仅记录 warning"已作废。

**`sun_path` 超限必须规避，不得止步于报错**：`AF_UNIX` 地址结构的 `sun_path` 是 108 字节定长数组（含结尾 NUL ⇒ 可用 107），`$XDG_RUNTIME_DIR`（或 fallback 的 HOME 派生路径）过长时 `bind()` 直接 `ENAMETOOLONG`，与权限和文件系统无关。管理通道路径因此按**单一规则**派生：主路径 `<runtime_dir>/llm-proxy.sock` 不超过 107 字节时原样使用；超过时改用 `/tmp/llm-proxy-<uid>/<sha256(runtime_dir) 前 16 位十六进制>.sock`。三条约束缺一不可：

- **派生必须是纯函数**：server 与 CLI 各自从同一 `runtime_dir` 算出同一结果，不引入发现协议、不写发现文件；**`runtime_dir` 必须来自 `--config-dir` 注入，不得取全局默认**——否则同进程内 ownership/pid/state 与管理 socket 分属不同 identity 分片，CLI 连不上自己启动的 server；
- **短名必须由 `runtime_dir` 哈希得出**：不同 `runtime_dir` ⇒ 不同 socket。若退化为单一全局名（如 `/tmp/llm-proxy-<uid>/llm-proxy.sock`），并行隔离实例会因启动前"清理陈旧 socket"而互相删除对方正在监听的 UDS；
- **回退不得静默**：生效时启动日志必须打 `management_socket_fallback`（含原始主路径与实际生效路径）。回退基目录 `/tmp/llm-proxy-<uid>` 必须以 `0700` 创建（`/tmp` 世界可写，共享该目录等于把唯一写路径的访问控制交给他人）；目录已存在且属主不是当前 uid 时**硬失败退出**，不得复用、不得再往别处退。`/tmp` 本身不可用等无法自动规避的情形按 §11.9.2 处理：记录错误事件、非 0 退出并提示用户缩短 `XDG_RUNTIME_DIR`。
- **"以 `0700` 创建"指 mode 出现在创建系统调用里**：先 `mkdir` 再 `chmod` 之间有一个窗口，同 uid 的其他进程可在其中看到并抢占这个名字；权限收回（`chmod`）仍然是必须的收尾，但它补的是"已存在的目录权限过宽"，不能当作创建手段。
- **基目录的属主与类型校验必须用 `lstat` 语义**：`mkdir` 落在一个**指向目录的软链接**上会跟随解析并"成功"，用 `stat` 校验等于校验了别人的目录，随后的 `chmod 0700` 还会顺着链接改掉别人的权限。基目录位置上是软链接时**拒绝**（既不跟随，也不删除）。
- **比较的 uid 是有效 uid**（`geteuid`）：目录由本进程创建，创建者即 euid；拿 real uid 比对，在 `euid ≠ ruid` 的调用下会把自家目录判成外人所有，并把读者引向"去查别人的属主"。
- **server 与 CLI 必须共用同一处目录派生**（同一 `runtime_dir` 与同一回退基目录的来源）：两侧各自调用"同一个纯函数"仍不足以证明同源，参数从哪来也是契约的一部分（`connect()` 与 `bind()` 受同一个 107 字节限制，名字不一致时没有发现协议可纠正）。

**写操作仅通过管理通道**（安全设计：写端点从 TCP 公开接口迁到 UDS，0600 权限即信任）。可达性探测与委托请求都指向本机回环，**不得经环境代理转发**（`HTTP_PROXY` / `HTTPS_PROXY` / `ALL_PROXY` 对 `127.0.0.1` 与 UDS 目标必须无效）。socket 文件的 `0600` 权限设置失败或其结果不可验证时，**不得继续启动**：`0600` 就是本通道的信任边界，权限设置静默失败等于把写路径开放给同机其他用户（判据见 §11.9.2）。

**常规运行时目录与 PID 文件权限必须硬性校验并收紧为属主独占**：常规路径下的管理 socket 所在目录（`runtime_dir`）与回退基目录同样必须满足安全信任边界。创建常规运行时目录时必须使用 `0700` 模式，禁止裸 `create_dir_all` 导致受宽松 `umask` 影响（如 `0775` 让组或其他用户可写）；若目录已存在，必须经 `lstat` 严格校验不是符号链接（拒绝跟随软链接以防劫持或权限穿透）、属主必须为当前有效 uid，且自动将其权限收紧至 `0700` 并经内核回读核验。PID 文件（`llm-proxy.pid`）同属安全敏感文件，写入后必须显式收紧为 `0600` 属主独占权限。若管理通道主路径的运行时目录存在软链接或外人属主等安全违约，启动期必须硬失败退出，杜绝在不安全目录下盲目监听。

Endpoints:

| 端点 | 方法 | 用途 |
|---|---|---|
| `/shutdown` | POST | 立即回复 200，然后停止接受新请求、最多 30s 等待在途请求、关闭管理通道并关停主服务器。 |
| `/env` | POST | 向运行中服务注入环境变量（`connect` 写完配置后使用，使运行中服务不重启即可拿到新 API key）。JSON body `{"env": {...}}`，最大 1MB。响应列出已应用的变量。 |
| `/state` | GET | `status` 用的只读进程内状态：坏请求封禁条目与流中断计数器。从不包含请求体或密钥。 |
| `/admin/provider/add` | POST | 添加 provider（写入 config.toml） |
| `/admin/provider/remove` | POST | 删除 provider |
| `/admin/provider/copy` | POST | 复制 provider |
| `/admin/model/add` | POST | 添加 model |
| `/admin/model/set` | POST | 修改 model 参数 |
| `/admin/model/remove` | POST | 删除 model |
| `/admin/model/rename` | POST | 重命名 model 并更新引用它的 binding |
| `/admin/model/provider` | POST | 管理 model 的 provider binding（add/remove/move） |
| `/admin/config/reload` | POST | 显式重新加载磁盘配置并替换运行时配置；不监听文件变化 |
| `/admin/config/snapshot` | GET | 返回配置正文**连同它所属的代（`revision`）**。要整体替换配置的客户端必须从这里取基线：正文与代来自同一次读取，否则 CAS 只剩下“假冲突或静默覆盖”二选一。**公开 HTTP listener 上响应剥离 `server.auth_token` 明文**（任意有效 token / open 模式不得拉取 `[server]` 认证明文）；UDS 管理通道保留完整（受 socket 文件权限 0600 门禁，TUI 编辑器 CAS 无损回环依赖）；`config/update` 收到 `server.auth_token` 缺失时保留当前代值，防“快照→更新”回环把它抹掉 |
| `/admin/config/update` | POST | 更新配置（原子写入）。请求体 `{"base", "config"}`：`base` 必须是 `config` 读出时的代；与当前代不符 → `409`（`configuration changed since it was read`），**不静默覆盖**；缺 `base`（客户端未能说明依据）→ `409` 拒收，因为无依据的整体替换正是覆盖本身 |
| `/admin/cooldown/clear` | POST | 清除冷却状态 |
| `/admin/cooldown/set` | POST | 手动注入冷却状态 |
| `/admin/oauth/write` | POST | 写入 OAuth 凭据 |
| `/admin/oauth/logout` | POST | 删除指定 provider 的 OAuth 凭据并刷新 server 内存缓存。请求体 `{"provider_id"}`；provider 不存在/非 OAuth → 400 `invalid_config`，凭据删除或缓存刷新失败 → 500 `internal_error`（不吞错）。**server 存活时，凡是"登出"语义的 CLI 入口都必须委托到此端点**——`provider logout` 与 `provider relogin` 的登出半段共用同一接线函数，不得各自实现本地直写 |
| `/admin/token/create` | POST | 创建 token 并生成 key，写入 `tokens.toml`。请求体 `{"name", "models"?, "windows"?, "expires"?}`；成功响应在 `data` 下携带明文 `key`（仅此一次返回）。 |
| `/admin/token/delete` | POST | 删除 token（按 key 或 name），并清理其配额运行时行。请求体 `{"key_or_name"}`；配置已删但配额行清理失败 → 500（如实上报"已生效一半"，不吞错） |
| `/admin/token/reset-usage` | POST | 重置该 token 的窗口用量计数。请求体 `{"key_or_name"}` |
| `/admin/status/probe` | POST | 定向/全量探针委托：发起真实上游请求并写探测缓存/活跃状态，属副作用操作，不挂公开 listener；契约见 §11.8.1 第 2 条 |
| `/admin/metrics` | GET | 返回 per-provider×model 请求指标快照（请求计数、可用率、延迟/吞吐分位数）。支持 `window`（5m/1h/24h/lifetime，默认 lifetime）、`provider`、`model` 参数。纯读操作，无副作用 |
| `/admin/metrics/history` | GET | 查询历史持久化指标快照。支持 window、provider、model、since、limit 过滤 |

**`/admin/metrics` 与 `/admin/metrics/history` 契约**：
- **`/admin/metrics` 查询参数与窗口**：
  - `window`：可选 `5m`、`1h`、`24h`、`lifetime`（默认 `lifetime`，与既有响应格式完全兼容）。
  - `provider` / `model`：可选过滤指定 provider 与 model。
  - `is_partial` 解释：当服务端运行时间（`uptime_seconds`）小于所选滑动窗口时长时（如运行 120s 时查询 `5m` 窗口），响应中的 `is_partial` 标记为 `true`，提示该窗口统计仍在冷启动爬坡期，尚未填满完整时间窗口；`lifetime` 窗口始终为 `false`。
  - 非法窗口参数（如 `?window=invalid`）返回 HTTP 400 `invalid_request`。
- **`total_ms` 与延迟分位数**：`latency` 统计除既有首字延迟 `ttft_ms`（p50/p95/p99）外，并列提供总耗时分位数 `total_ms`（p50/p95/p99 与 `sample_count`），口径与设计底稿一致。
- **可用率口径**：可用率衡量"上游是否给出了有意义的响应"。分母 = `completed + upstream_error + upstream_interrupted`；分子 = `completed + upstream_error`。`upstream_error`（含 4xx/5xx/unclassified）视为"可用"——上游返回了错误码但确实响应了；`upstream_interrupted`（超时、连接重置、流中断）视为"不可用"。`client_disconnected` 不计入分母——客户端主动断开不代表上游故障。延迟/吞吐采样采用间隔采样（每 10 次请求写一次环缓冲区，缓冲区容量 100 条，覆盖约 1000 次请求历史）。
- **快照持久化与历史查询**：
  - 后台异步任务定期（每 60 秒）将指标快照写入 `state_dir/metrics.db`（SQLite WAL 模式），请求热路径零 I/O 阻塞；服务端停机前执行同步刷盘保存最终快照。默认自动清理超过 7 天的历史快照。**持久化快照恒为 `lifetime` 口径**（单表快照行，不按 5m/1h/24h 分窗口维度分别存储）。
  - `GET /admin/metrics/history`（亦支持 `GET /admin/metrics?history=true`）返回历史快照列表，支持 `window`、`provider`、`model`、`since`（ISO 8601）、`limit`（默认 100，最大 1000）参数。其中 `window`（5m/1h/24h）仅作**时间过滤**——只返回对应时段内写入的快照行，与 `/admin/metrics` 的滑动窗口统计口径无关；显式 `since` 优先于 `window`；`lifetime` 或缺省表示不做时间过滤。进程重启后持久化历史仍可完整追溯。

**写操作与带上游副作用的端点受严格 RBAC 权限隔离与安全收敛**：
- 在公开 HTTP listener 上，`/admin/*` 端点在 `token_auth` 认证基础上实施 RBAC 权限分级：
  1. **租户只读与状态自查端点**：允许所有有效 Token（含普通 Token 与 Admin Token）访问，包括 `GET /admin/ping`、`GET /admin/status`、`GET /admin/client-config/{client}`（按 Token 的 `allowed_models` 过滤返回模型）、`GET /admin/config/snapshot`（剥离明文敏感密码）、`GET /admin/usage`、`GET /admin/metrics` 等。
  2. **特权管理端点（Admin-only）**：必须要求调用方持有管理员 Token（在 `tokens.toml` 或 `tokens/*.toml` 中声明 `admin = true`）或处于免鉴权 Open 模式。凡普通租户 Token（`admin = false` 或缺省）访问，一律由 `require_admin` 中间件直接拦截并返回 HTTP 403 Forbidden（`{"status": "error", "error": "Admin privilege required for this endpoint"}`）：
     - `/admin/token/*`（`create`, `delete`, `rename`, `reset-usage`）；
     - `/admin/config/*`（`reload`, `update`）；
     - `/admin/provider/*`（`add`, `remove`, `copy`, `reselect`）；
     - `/admin/model/*`（`add`, `set`, `remove`, `rename`, `provider`）；
     - `/admin/cooldown/*`（`clear`, `set`）；
     - `/admin/oauth/*`（`write`, `logout`）；
     - `/admin/protection/*`（`bad-request-budget/reset`, `token-error-cooldown/clear`）。
  3. **探针委托端点 `POST /admin/status/probe` 的双角色分流契约**：
     - **管理员（Admin Token / 本地 UDS 通道）**：全权全量探活，可指定任意 provider/model 选择器或执行全局探测，不扣除租户配额，刷新全局活跃状态与探测缓存。
     - **普通租户 Token（Regular Token）**：支持按需探活，但实行严格的安全与经济闭环：
       - **模型授权过滤与收敛**：普通 Token 仅允许探活其 `allowed_models` 范围内的模型。显式请求探活未授权模型返回 `403 Forbidden` (`{"status":"error","error":"Model '...' is not allowed for this token"}`)；未指定模型（全量请求）时自动收敛为该 Token 授权的模型集合；
       - **配额准入强门限（Fail-Closed Quota Gate）**：探活执行前，逐一检查目标模型的配额窗口（`reserve_request_slot`）；若租户配额已耗尽，在发起任何外部网络请求前直接拦截并返回 `429 Too Many Requests` (`{"status":"error","error":"Token quota exhausted for model '...'; probe rejected"}`)，杜绝零成本蹭网；
       - **用量如实计费**：每次探活消耗 1 次请求配额槽位；真实发出的探活调用计入该 Token 的 token 用量并记账。
- 本地 UDS 管理通道（`management_router`）作为本地特权通道运行在 Unix Domain Socket 上，不受 HTTP Token 权限限制，保持全功能特权访问。

**公开存活探针 `/health`**：公开 listener 提供无鉴权、零拓扑泄露的公开存活探针 `GET /health`，无条件返回 200 `{"status": "ok"}`（不泄露版本、不泄露配置、不访问上游），供自动化脚本、容器编排探针与反向代理做存活性检查。

#### 11.8.1 管理通道写操作与探测协议契约

1. **`POST /admin/provider/add`（原子提交与结构化分类）**：
   - **请求体（JSON）**：
     - `provider_id`: catalog 产品 ID（如 `"deepseek"`）；
     - `provider_name`: 可选自定义实例名称（如 `"deepseek-2"`，默认等于 `provider_id`）；
     - `api_key_env`: 可选环境变量名称；
     - `no_api_key`: 是否免密钥；
     - `provider_type` / `endpoint_url`: 自定义 provider 类型与端点；
     - `models`: 可选绑定的 model 列表；
     - `skip_probe: bool`: 是否跳过探针与外部鉴权就绪检查（默认 `false`）；
     - `pending_account: Option<PendingAccount>`: 待原子提交的 OAuth 凭据（包含 `auth_type`、`account`、`access_token`、`refresh_token`、`project_id`、`expires_at_unix`）。
   - **响应体与错误契约**：
     - 成功返回 200 `{"status": "ok", "data": {"message": "..."}}`；
     - 失败返回相应 HTTP 状态码，且 JSON 必须包含机器可识别的结构化错误分类 `kind`：
       - `probe_failed` (422)：模型不可达、超时等上游探针失败；
       - `auth_not_ready` (422)：环境变量未设置、OAuth 未就绪；
       - `config_conflict` (409)：Provider ID 冲突或重复命名；
       - `invalid_config` (400)：Schema 校验不通过或引用不存在；
       - `internal_error` (500)：磁盘持久化 I/O、generation 发布失败等。
2. **`POST /admin/status/probe`（定向探针委托）**：
   - **请求体（JSON）**：支持 `ProbeSelection` 对象（`{"provider": "...", "model": "..."}`），无参数时表示全量探测；
   - **流式支持**：请求头包含 `Accept: text/event-stream` 时，按 SSE 流式实时推送探针进度与结果事件；普通请求返回聚合后的完整 JSON 结果。

服务不监视 `config.toml` 变化。经管理通道委托的配置变更把持久化文件与内存运行时配置作为一次受管操作一起更新。直接的外部编辑不会自动更新运行中服务：默认用 `llm-proxy restart`，或对不需要新进程环境变量的变更显式调用 `/admin/config/reload`。引入或修改 `api_key_env` 值的变更需要重启，使服务以预期环境启动。

异常退出遗留的陈旧套接字文件在启动时、绑定之前被清除。

### 11.9 终态判定与静默失败禁令

本节把启动与关闭路径上的几类共同根因升级为规范。这些缺陷都不是"漏了一个错误分支"，
而是**把尚未成立的中间状态当成了终态**：`spawn` 了 = 在跑、写入了 = 已生效、超时没等到 = 成功。

#### 11.9.1 三态：成功 / 失败 / 不可判定

任何有副作用或影响可用性的操作，收尾必须落在三态之一，且每态有确定的下游行为：

| 终态 | 判据（必须是正向证据） | 必须做 | 禁止做 |
|---|---|---|---|
| 成功 | 拿到"已经成立"的证据：返回值、就绪事件、状态复核、进程确认 | 继续 | 把"没报错"当成功 |
| 失败 | 拿到明确的否定证据：错误码、校验不通过、进程确认已退出 | 记录错误事件并终止当前路径 | 吞掉错误继续 |
| 不可判定 | 只有"没等到 / 没听到"，正反证据都没有 | ① **升级手段**复核终态；② 仍不可判定则保留现场并报错 | 折叠成"成功"；也禁止当成"失败"直接重试（会重复副作用） |

判据只有一条：**"已发出" ≠ "已完成"，"没等到坏消息" ≠ "是好消息"**。
超时、轮询次数用尽、channel 被关闭、日志里没有报错，全都只是"不可判定"。
升级手段必须**改变信息来源**（问内核、问进程、重读状态文件、换更强的终止手段），
不能只是把等待时间调长——把 timeout 从 3s 改成 30s 不产生任何新证据。

#### 11.9.2 启动阶段：零静默失败、零静默异常状态

启动的前置条件是可枚举的，因此必须在建 listener、写 pid、接受第一条请求**之前**全部检查完：

- 任一影响对外契约或安全边界的准备动作失败——绑定管理通道、设置其 `0600` 权限、
  建立数据面 listener、写入 pid 文件、创建回退目录——进程必须以非 0 退出，
  退出前留下明确日志事件与**可执行的**用户提示（下一步跑什么命令、改哪个配置）。
  不得"带着半个子系统继续启动"，也不得只打日志不退出。
- **走了非常规分支同样属于异常状态**，必须留痕：例如 §11.8 的 `sun_path` 路径回退。
- 失败退出前必须撤销本次已产生的对外可见痕迹（pid 文件等）。否则 `status` 会指向一个
  已退出的进程。
- **撤销只能针对能证明属于自己那一份的痕迹**：pid 文件内容与本次启动的 pid 不符（读不出、
  解析不了、是别人的 pid）时**不得删除**，并留下事件说明为何没删。并发启动下"我把现场清了"
  会毁掉的是**健康服务**的可观测性：`status` 从此报未运行、`shutdown` 退化成按 pid 发信号。
- 权限类检查失败不得降级为"尽力而为"：`0600` 未生效即信任边界未成立，属启动失败。
- **端口绑定失败带占用者自查与明确收尾提示（G1）**：数据面端口 `server.listen` 绑定遇到 `EADDRINUSE` 时，禁止只返回裸系统错误；必须在 Linux `/proc` 等信息源下解析占用进程的 PID、comm、cmdline，明确区分「疑似残留的 llm-proxy 实例」与「外部第三方陌生进程」，并输出含具体 PID 的可执行清理命令（`kill <PID>`）或修改端口指引。严禁代理自动 kill 占用者。

#### 11.9.3 运行阶段：允许降级，但必须留痕

运行时不能拿整个服务给单次故障陪葬。若服务**仍能继续提供主要能力**，允许记录日志后继续运行；
但"继续"不等于"无痕"：

- 降级必须可观测——日志事件，且尽量经 `status` / `GET /state` 或计数器暴露出来；
- **降级导致的停机不是"按要求停机"**：控制面耗尽容忍预算后自己请求关停，其停机原因以
  `Ok(reason)` 形状到达数据面。若退出码沿用"正常关闭"的 0，supervisor（如
  `Restart=on-failure`）与包装脚本都无法把"用户停的"与"子系统死了才停的"区分开——这正是
  §11.9.2 禁止的异常状态静默在退出码上的表现。此类原因必须映射为**非 0 退出**并留下独立事件
  （`management_channel_degraded_stop`）；"按要求停"一类原因（信号、`/shutdown` 委托）保持 0。
- 探测与观察他人状态的动作**不得把自己挂住**：在有限预算内轮询的判定循环里，任何一次外部
  观察（`connect()` 一个可能无人 accept 的地址、等待一个可能不存在的文件）都必须自带边界，
  否则预算形同虚设，"不可判定 ⇒ 报错"这条出口永远走不到。
- **安全边界不属于可降级项**：权限位、token 认证、写路径唯一性（§11.7.1 第 4 条）、
  凭据不落盘这些在任何情况下都不能"记录一下继续跑"。运行时触碰它们等同于 §11.9.2 的启动失败处理。

本节与 §8 第 11 条等价互为索引。

### 11.4A 全场景矩阵（Deployment Scenario Matrix）

#### 11.4A.1 远程模式场景（待实现，只读）

| # | 时序 | CLI 行为 | 并发控制 | 冲突风险 |
|---|------|---------|---------|---------|
| R1 | Server 已启动 → CLI 启动 | 检测 server.listen → 连 Server → 只读查询（status/usage） | Server Mutex | 无 |
| R2 | Server 未启动 → CLI 启动 | 连不上 → 报错退出 | — | 无 |
| R3 | CLI 查询中 → Server 宕机 | 连接错误 → 报错，用户重试 | — | 无 |
| R4 | CLI-1 运行中 → CLI-2 启动 | 两个都连远程 Server → Server 串行 | Server Mutex | 无 |
| R5 | Server 恢复 → CLI 重试 | 重连成功 → 正常查询 | — | 无 |

> 远程模式只支持读操作（status/usage 查询），写操作仅本机 UDS（2026-08-09 决策）。

#### 11.4A.2 本地模式场景

| # | 时序 | 检测方式 | CLI 行为 | 并发控制 |
|---|------|---------|---------|---------|
| L1 | Server 已启动 → CLI 启动 | PID + 进程 + ping OK | 委托 Server | Server 内部锁 |
| L2 | Server 未启动 → CLI 启动 | 无 PID file | 独立模式 | 文件锁 |
| L3 | Server 崩溃 → CLI 启动 | PID 存在但进程死 | 清理 stale PID → 独立模式 | 文件锁 |
| L4 | CLI 独立模式运行中 → Server 启动 | CLI 持有文件锁 | CLI 完成 → 释放锁 → Server 加载最新 | 文件锁保证不冲突 |
| L5 | CLI-1 独立模式 → CLI-2 启动 | CLI-1 持有文件锁 | CLI-2 等锁 → CLI-1 释放后执行 | 文件锁串行化 |
| L6 | Server 运行中 → CLI-1 → CLI-2 | 两个都检测到 Server | 两个都委托 Server | Server 内部锁 |
| L7 | CLI 写盘中 → Server 恰好启动 | CLI 持有文件锁 | Server 启动时等锁 → CLI 写完释放 → Server 加载 | 文件锁保护 |
| L8 | TUI 运行中 → Server 被其他终端启动 | TUI 启动时选了独立模式 | TUI 继续独立模式（不中途切换） | 文件锁保护每次写操作 |

#### 11.4A.3 微妙场景详解

**L4：CLI 写盘 → Server 启动**
```
CLI:    [lock] [read] [modify] [write] [unlock]
                                        ↑
Server:                                 [start] [load config] [serve]

结果：Server 读到 CLI 写入的最新配置 ✅
```

**L7：CLI 和 Server 同时启动**
```
时间线 A（CLI 先拿到锁）：
  CLI:    [lock ✓] [read] [write] [unlock]
  Server:          [等锁...]           [lock ✓] [read] [serve]
  结果：Server 加载 CLI 的修改 ✅

时间线 B（Server 先拿到锁）：
  Server: [lock ✓] [read] [init] [unlock]
  CLI:             [等锁...]    [lock ✓] [read] [write] [unlock]
  结果：CLI 基于 Server 初始化后的状态修改 ✅
  注意：此时 Server 内存不含 CLI 的修改
  → CLI 应检测到 Server 已启动（等锁期间重新检测 PID）
  → 如果发现 Server 已启动，放弃独立模式，改为委托
```

**L8：TUI 不中途切换模式**
```
原因：
1. 状态一致性复杂（TUI 内存已有状态，切换要同步）
2. 文件锁已保证不冲突
3. 简单原则：启动时决定模式，运行期间不变
```

## 12. Provider 与模型管理设计（Provider and Model Management Design）

llm-proxy 为 provider 与模型提供统一管理命令，取代旧的 `auth` 命令。这些命令被有意设计为配置编辑器：`provider` 编辑 provider 相关配置并管理 provider 认证状态；`model` 编辑模型相关配置与 provider 绑定；`connect` 保留为 `provider add` 的 CLI 别名。

### 12.1 命令职责（Command Responsibility）

| 命令 | 职责 | 备注 |
|---------|---------------|-------|
| `provider` | **编辑 provider 配置与认证状态** | 创建/查看/删除 provider 配置、OAuth 登录/登出/刷新/重登、用量重置 |
| `connect` | `provider add` 的别名 | 添加 provider 配置的 CLI 快捷方式，标志原样透传 |
| `model` | **编辑模型配置** | 创建/复制/删除模型、编辑参数/特性、管理按协议的 provider 绑定 |
| ~~`auth`~~ | ~~已删除~~ | 全部功能由 `provider` 覆盖 |

**删除 `auth` 的原因：**

| 原 `auth` 功能 | 现归属 |
|------------------------------|----------------|
| `auth --login`（创建新 OAuth provider） | `provider add` / `connect`（别名） |
| `auth --login --provider X` | `provider login X` |
| `auth --logout` | `provider logout NAME` |
| `auth --relogin` | `provider relogin NAME` |
| `auth --refresh` | `provider refresh NAME` |
| `auth --info` | `provider info [NAME]` |
| `auth --delete` | `provider remove NAME` |
| `auth --reset-usage` | `provider reset-usage NAME` |
| `auth --list` | `provider list` |
| `auth` (TUI) | `provider` (TUI) |

### 12.2 `provider add` / `connect` — 创建 Provider

`provider add` 是创建所有 provider 类型的**唯一 provider 管理入口**。`connect` 是同一创建路径的用户友好别名，主要面向 catalog 成熟产品；自定义/高级端点的创建应记录在 `provider add` 之下。

#### 命令形式

```bash
# 交互式（无 PRODUCT 或缺必填标志）
provider add
# → 产品选择 → 配置 → 写入

# 非交互：API key 产品
provider add deepseek --api-key-env DEEPSEEK_API_KEY --model deepseek-v4-pro-lp

# 非交互：OAuth 产品（发起登录流）
provider add openai-subscription --model gpt-5.5-lp

# 非交互：自定义端点
provider add my-server --type openai-chat \
           --endpoint-url http://localhost:8080/v1/chat/completions --api-key-env MY_KEY

# 非交互：无密钥（如 ollama）
provider add ollama --no-api-key

# 等价的 connect 语法（别名）
connect deepseek --api-key-env DEEPSEEK_API_KEY --model deepseek-v4-pro-lp
connect openai-subscription --model gpt-5.5-lp
```

#### 创建流程

**API key 产品：**

```text
provider add deepseek --api-key-env DEEPSEEK_API_KEY --model deepseek-v4-pro-lp
→ 校验 catalog 产品 + 凭据引用 → 选择/确认模型 → 运行真实连通性探测 → 写入 provider 配置 → 创建所选模型绑定 → 完成
```

**OAuth 产品：**

```text
provider add openai-subscription --model gpt-5.5-lp
→ 启动 OAuth 登录流（Device Code / 浏览器）
→ 登录成功 → 选择/确认模型 → 运行真实连通性探测 → 写入 provider 配置 + 认证状态 → 创建所选模型绑定 → 完成
```

**交互式（无 PRODUCT）：**

```text
provider add

选择产品：
  1. OpenAI Subscription (OAuth)
  3. DeepSeek API (API Key)
  4. Custom provider...
> 1

Provider name [openai-subscription]: ↵
→ 跟随对应产品创建流程
```

所有写入都是**原子的**：先校验全部输入，再一次性写入 config.toml；失败则什么都不写。

### 12.3 `provider` — 管理 Provider

#### 入口行为

| 用法 | 行为 | 模式 |
|-------|------|------|
| `provider` | stdin/stdout 为交互式时进入 TUI 列表界面，否则报用法错误 | TUI |
| `provider NAME`（非法） | Provider 操作使用显式子命令 | CLI |
| `provider list` | CLI 文本列出所有 providers | CLI |
| `provider info [NAME]` | CLI 文本详情或摘要 | CLI |
| `provider add [PRODUCT]` | 创建 provider（交互或非交互） | CLI/TUI 提示 |
| `provider copy SOURCE NAME` | 把 provider 配置复制为新 provider ID | CLI/TUI 提示 |
| `provider login/logout/relogin/refresh/remove/reset-usage NAME` | 执行对应操作 | CLI |

模式：
- **裸 `provider`** → 仅在交互式终端进 TUI；非交互调用返回用法错误。
- **有子命令** → CLI 执行。
- 操作子命令在构造上互斥；不要重新引入标志驱动的动作分发。

#### 完整命令矩阵

```text
# 入口
provider                                        → TUI 列表
provider --select X                             → TUI 详情（仅交互式便捷入口）
provider list                                   → CLI 文本列表

# 创建/复制
provider add [PRODUCT] [--api-key-env Y|--no-api-key] [--type TYPE --endpoint-url URL]
provider copy SOURCE NAME [--api-key-env Y|--no-api-key]  # OAuth 复制总是重新登录
connect [PRODUCT] [--api-key-env Y|--no-api-key] [--model MODEL...]  成熟 catalog 产品设置路径

# 查看
provider info [NAME]                            → CLI 文本详情/摘要，支持时含用量

# 认证（OAuth provider）
provider login NAME                             → 登录
provider logout NAME                            → 登出（清除 token）
provider relogin NAME                           → 登出 + 登录
provider refresh NAME                           → 刷新 token / 缓存用量（适用时）

# 删除
provider remove NAME                            → 未被引用时删除 provider（二次确认）
provider remove NAME --force                    → 删除 provider（跳过确认）；被引用时同时删除引用它的模型绑定（与 Admin `/admin/provider/remove` force 语义一致）

# 操作
provider reset-usage NAME                       → 重置用量（分层确认）
provider reset-usage NAME --force               → 重置用量（跳过确认）
```

#### 标志与参数语义

| 命令 | 参数/标志 | 说明 |
|------|------|-------------|
| `provider add [PRODUCT]` | `PRODUCT` | catalog 产品 ID 或自定义 provider ID。省略时显示交互式产品选择器。 |
| `provider add` | `--api-key-env ENV` | 存储承载上游 API key 的环境变量名；从不存储密钥值。 |
| `provider add` | `--no-api-key` | 声明该 provider 不需要上游认证。 |
| `provider add` | `--endpoint-url URL` | 完整上游端点 URL，不是 base URL。与 `--type` 一起必填。内置 provider 端点 URL 是纯上游 API URL；不含 token，也不依赖查询参数做认证/路由。 |
| `provider add` / `connect` | 可重复的 `--model MODEL_ID` | 对成熟 catalog 产品，在设置与验证期间显式选择要绑定的 catalog 模型。可传多次。非交互成熟产品 add/connect 至少需要一个模型；交互模式会提示选择。自定义 provider add 不接受即时模型绑定。 |
| `provider copy SOURCE NAME` | `SOURCE` | 要拷贝来源的现有 provider ID。 |
| `provider copy SOURCE NAME` | `NAME` | 新 provider ID，必须未被占用。 |
| 操作类 | `NAME` | 要操作的 provider ID。 |
| 破坏性操作 | `--force` | 跳过 `remove` 与 `reset-usage` 的确认。 |

连通性验证是常规 `connect` / 成熟产品 `provider add` 契约的一部分，不是可以静默跳过的被动默认。用户选定 API key 环境变量或完成 OAuth 登录并选择模型后，向导使用解析出的端点/认证/模型发送真实上游探测。探测失败时配置保持不变，并返回修正所需的 provider/模型/错误上下文。

唯一不立即验证的创建路径是**自定义 provider 配置**：自定义 provider 可以只作为 provider 记录添加，带显式 `--type` / `--endpoint-url` / 认证引用，不即时绑定模型。因为此时没有选择任何前端模型，也就没有必须的模型专项探测。用户稍后通过 `model provider add` 或模型 TUI 挂接自定义 provider；验证可以在那里发生，或在首次请求时发生。

TUI/CLI 的端点显示规则：

- 内置端点 URL 是完整上游请求 URL（例如 `/v1/chat/completions`、`/v1/responses`、`/v1/messages`），不是 base URL。
- 对当前支持的内置端点，端点 URL 不携带 API token，也不用查询参数做凭据选择或路由，因此 UI 可以把它们作为普通非机密配置展示。
- 机密仍在环境变量或统一 OAuth 存储中；TUI 必须始终不显示 API key 值、OAuth access token、refresh token 或 token 存储内容。
- 自定义端点 URL 是用户输入，应按 URL 校验，但 llm-proxy 不应为内置 provider 发明“查询串含 token”的掩码模型。

Provider 命令主要编辑 provider 配置与 provider 认证状态。它们不得静默添加、删除或修改模型 provider 绑定。唯一允许的例外是 `provider add` / `connect` 中用户确认的**成熟产品设置流**：catalog 支持的产品可在保存前展示模型列表，然后验证所选 provider/模型组合，再原子地写入 provider 配置/认证状态加上所选前端模型绑定。这些绑定是 add/connect 流中的显式选择，不是隐藏副作用。自定义 provider 与 `provider copy/remove/auth` 命令从不创建模型绑定；用户之后用 `model provider add` 或模型 TUI 挂接。任何写入后，都会重新加载并校验完整配置，再提交原子文件替换。

成熟产品模型选择：

- 内置 provider catalog 是成熟产品模型模板 ID、上游显示名、上下文/输出限制、特性、协议范围与排序的唯一事实源（SSOT）。CLI 配置生成与 TUI 模型选择器消费同一套模板；TUI 必须投影 catalog 条目，而不是维护第二张静态模型表。
- 仅适用于经 `provider add` / `connect` 添加的 catalog 支持的成熟 LLM 服务产品。
- 在任何配置写入之前运行。OAuth 产品必须先完成 OAuth 登录；API key 产品必须先解析所选环境变量。然后针对解析出的端点/认证/模型组合探测所选模型。除非探测成功，命令不能完成 provider 设置。
- 交互式 add/connect 展示该产品受 catalog 支持的模型/协议，并要求用户至少选择一个绑定来验证和创建。
- 非交互 add/connect 要求显式重复的 `--model MODEL_ID` 标志。例：`connect deepseek --api-key-env DEEPSEEK_API_KEY --model deepseek-v4-pro-lp --model deepseek-v4-flash-lp`。对成熟产品省略 `--model` 是错误，而不是 provider-only 设置模式。
- `--model` 的值是成熟产品 catalog 模板 ID，不是任意前端 ID。当前 Rust 实现应对这些模板沿用 Go/v1 命名风格：前端模型 ID 是可读组合，如 `gpt-5.5-openai-subscription-lp`，通常为 `<上游模型家族>-<产品/provider 家族>-lp`。所选模板拷贝进 `[models.<template-id>]`；当用户给 provider 起了与产品默认不同的名字时，拷入条目内的 provider 绑定在 `connect`/`provider add` 中被重写为实际选定的 provider ID。另可用独立的 `display_name` 跨 provider 一致地展示常见上游模型名。
- 只在显式用户选择（非交互模式的 `--model` 或交互模式的勾选行）与验证成功之后，才把所选绑定写入 `[models.*].*_providers`。
- 不含即时验证的 provider-only 设置仅对自定义 provider 配置可用，不对成熟 catalog 产品可用。
- 对自定义 provider、provider copy、provider remove 与纯 auth 命令不运行。

例：在添加 provider `openai-sub-work` 时选择 `gpt-5.5-openai-subscription-lp` 模板，会创建一个同名前端 ID 的模型条目，且只重写绑定的 provider 名：

```toml
[models.gpt-5.5-openai-subscription-lp]
display_name = "GPT 5.5"
context_window = 272000
max_output_tokens = 128000
openai_responses_providers = [
  { name = "openai-sub-work", model = "gpt-5.5" },
]
```

这镜像了 Go/v1 行为：模型模板 ID 是稳定的产品家族 ID，而绑定内的 provider ID 可以是用户自选的账号/provider 名。

#### Provider copy 语义

`provider copy SOURCE NAME` 是 provider 侧的对等操作：安全地把一个 `[providers.SOURCE]` TOML 块拷贝到 `[providers.NAME]`，再带校验地编辑它。它适用于在避免手工 TOML 出错的前提下，创建端点配置相似的多个账号/区域/产品。

规则：

- `SOURCE` 必须存在，`NAME` 必须未被占用。
- 被拷贝的 provider 保留产品/端点/兼容（compat）/请求频率配置，除非被标志覆盖。
- API key provider 必须重新确认凭据引用。在非交互 CLI 中，对 API key provider 的 `provider copy` 需要 `--api-key-env ENV`，除非源 provider 在合法时被显式拷贝为 `--no-api-key`。在 TUI 中，用户必须在保存前显式输入或确认 `api_key_env`。工具不得从 provider ID 臆造或建议环境变量名；环境变量命名是用户的决定。这既防止意外复用旧账号/密钥，又让命名策略保持用户所有。
- 无密钥 provider 按原样拷贝。
- OAuth provider 只拷贝 provider 配置；access token、refresh token、账号 ID 与用量缓存**从不拷贝**。默认使用新账号模式：新 provider 获得新的 `auth.account`（默认等于新 provider ID），用户必须完成一次全新登录，被拷贝的 provider 才算配置完成。以后可加显式账号共享选项（如 `--auth-account EXISTING`）；它不拷贝任何 token，只是刻意引用现有凭据记录。
- `provider copy` 不创建也不复制模型绑定；用户之后用 `model provider add` 显式挂接，或在模型 TUI 中编辑绑定。
- 拷贝不是重命名：`SOURCE` 在被显式移除前一直有效。

OAuth provider 身份由两层表示：

1. **Provider ID** — 稳定的路由/配置键，如 `openai-sub-personal`、`openai-sub-work`。模型绑定只引用 provider ID。
2. **OAuth 账号键** — 来自 `auth.account` 的凭据身份；省略时默认为 provider ID。统一 OAuth 存储以该账号键为主键。
3. **已认证账号元数据** — 登录后学到的仅供展示的元数据，如签发方/产品、账号邮箱或 subject、可用时的套餐/工作区、过期时间与用量状态。

这种分离允许同一产品的多个 OAuth provider 共存，也允许在将来有显式 CLI/TUI 流启用时进行刻意的账号共享。若两个 provider 以同一邮箱/账号认证但使用不同的 `auth.account` 键，它们仍是不同的凭据记录；`provider list/info` 应展示二者，并可警告它们看似共享同一上游账号。

#### `list` 输出

```
$ provider list

openai-subscription    OpenAI Subscription (Device Code)    user@a.com
deepseek               DeepSeek API                         api-key
```

#### `info` 输出

**单 provider 详情：**

```
$ provider info openai-subscription

provider:     openai-subscription
type:         openai-device-code
account:      user@a.com
status:       authenticated
expires:      2026-07-28 (7d remaining)

usage:
  plan:       Pro
  5-hour:     62%  reset in 2h 31m
  weekly:     30%  reset in 3d 12h
  reset credits: 2
```

**按量付费产品（无窗口，有余额）：**

```
$ provider info openai-billing

provider:     openai-billing
type:         openai-device-code
account:      user@a.com
status:       authenticated
expires:      2026-07-28 (7d remaining)

usage:
  plan:       Usage-Based
  balance:    $12.34
```

**API key 类型（无用量）：**

```
$ provider info deepseek

provider:     deepseek
type:         api-key
status:       configured
```

不展示用量区域，也不解释原因。

**不带 `--select`（列出所有 provider 的信息摘要）：**

```
$ provider info

openai-subscription (user@a.com) — Pro
  5-hour:  62%  reset in 2h 31m
  weekly:  30%  reset in 3d 12h
  reset credits: 2

deepseek — api-key
  (no usage tracking)
```

When no OAuth provider:
```
$ provider info

No providers with usage tracking configured.
```

#### `remove` 确认与引用安全

默认二次确认：
```
$ provider remove openai-subscription

Remove provider "openai-subscription" (user@a.com)?
This will remove provider config and credentials. Model bindings are not changed. [y/N]
```

`--force` 跳过确认：
```
$ provider remove openai-subscription --force

✅ Provider "openai-subscription" removed.
```

移除只删除 `[providers.NAME]` 与匹配的认证状态，不编辑模型绑定。若仍有模型 provider 绑定引用 `NAME`，`provider remove NAME` 会在确认前列出可操作的引用清单并失败，告知用户先通过 `model provider remove` 或模型 TUI 移除它们。这既保持了“provider 命令不静默编辑模型配置”的规则，也防止了悬空无效引用。

#### `reset-usage` 分层确认

**确认策略：**

| 用量状态 | 条件 | 行为 |
|------------|-----------|----------|
| 已用尽 | 任一窗口 `limit_reached: true` | 直接执行，不确认 |
| 未用尽 | 所有窗口 `used_percent >= 50%` | 二次确认 |
| 用量充足 | 任一窗口 `used_percent < 50%` | 二次确认 + 三色警告 |
| `--force` | 任意状态 | 跳过全部确认 |

**已用尽（直接执行）：**

```
$ provider reset-usage openai-subscription

✅ Usage reset.
  5-hour:  0%
  weekly:  0%
  reset credits: 1
```

**未用尽（二次确认）：**

```
$ provider reset-usage openai-subscription

Consume 1 reset credit? (2 → 1) [y/N] y

✅ Usage reset.
  5-hour:  0%
  weekly:  0%
  reset credits: 1
```

**用量充足（二次确认 + 颜色警告）：**

```
$ provider reset-usage openai-subscription

Consume 1 reset credit? (2 → 1) [y/N] y

[WARNING] Usage is low (5-hour: 20%, weekly: 35%).
          This reset credit will be wasted.
Confirm anyway? (yes/N)
```

`[WARNING]` 行以红/黄高亮渲染。

**`--force`（跳过全部确认）：**

```
$ provider reset-usage openai-subscription --force

✅ Usage reset.
  5-hour:  0%
  weekly:  0%
  reset credits: 1
```

#### 不支持的 provider 处理

原则：不泄露实现细节，不迷惑用户。

API key provider 的 `info`：正常展示 provider 信息，不显示用量区域，不解释原因。

不支持 provider 的 `reset-usage`：
```
$ provider reset-usage deepseek

Provider "deepseek" does not support usage reset.
```

### 12.4 Provider TUI 设计

##### 12.4.1 事件循环架构

**终端恢复不变量：** raw mode、备用屏幕模式与光标可见性都是终端资源。每个 TUI 必须在正常退出时、以及 setup/渲染/事件处理/业务逻辑返回错误时都恢复它们。这防止意外的 TUI 错误把用户的 shell 留在 raw mode 或备用屏幕上。

**全局控制键契约：** 由于 raw mode 把控制字符作为按键事件投递、而不是终端行规程信号，每个 TUI 入口必须先经共享全局层分发这些键，再做屏幕内处理：

- `Ctrl+C` 请求 TUI 退出，取消或脱离在途后台任务，依赖共享的终端会话守卫做清理。
- `Ctrl+D` 提供 EOF 语义：退出非编辑态 TUI，或从空输入流返回；不得丢弃非空输入，也不得隐式提交它。
- `Ctrl+Z` 在支持的平台上提供挂起语义：在 `SIGTSTP` 之前恢复终端，`SIGCONT` 之后重新进入 raw mode 与备用屏幕。不支持的平台必须安全失败，不留下被改变的终端。
- 现有的 `Esc`、`q` 与屏幕内绑定保持当前语义。

该契约适用于 `connect`、`provider` 与 `model` TUI 入口，包括异步 OAuth/探测/配置操作进行期间。

##### 12.4.4 Connect / `provider add` 向导流

TUI 中的 connect 向导与 Go v1 bubbletea 实现遵循同一顺序：

| 步 | 屏幕 | 用户动作 | 下一屏 |
|------|--------|-------------|-------------|
| 1 | `ProductSelection` | 选择成熟产品（方向键 + 回车）或“Custom provider” | `EnvVarSelection` 或 `CustomProviderName` |
| 2 | `EnvVarSelection` | 从已扫描环境中选择变量，或“[Skip]” | 配置 → `ModelSelection` |
| 3 | `ModelSelection` | 多选模型（空格），全选（a）；至少选一个 | `Verifying` |
| 4 | `Verifying` | 对解析出的端点/认证/模型做连通性探测 | `Done`（成功）/ `ProductSelection`（重试） |
| 5 | `Done` | 展示结果；Enter 返回 `ProductSelection` | `ProductSelection` 或 `Quit` |

自定义 provider 子流程：

| 步 | 屏幕 | 用户动作 | 下一屏 |
|------|--------|-------------|-------------|
| 2a | `CustomProviderName` | 输入唯一的 provider 名 | `CustomProviderEndpoint` |
| 2b | `CustomProviderEndpoint` | 输入完整上游 URL | `EnvVarSelection` |
| 2c | `EnvVarSelection` | 选择环境变量或“[Skip]” | 配置 → 保存（无模型验证） |

OAuth 产品子流程：

| 步 | 屏幕 | 用户动作 | 下一屏 |
|------|--------|-------------|-------------|
| 1 | `ProductSelection` | 选择 OAuth 支持的产品 | → |
| 2 | `OAuthLogin` | 选择 provider 名（推荐/自定义） | → |
| 3 | 启动 OAuth 流 | 展示设备验证 URL；复制/打开/跳过快捷键 | 开始轮询 |
| 4 | 轮询超时或成功 | 成功 → `EnvVarSelection` | 超时 → 回到 `OAuthLogin` |

##### 12.4.4A 状态机收敛、模型重选与原子提交交互契约

1. **ProductSelection 静态模板性**：
   - 列表界面不展示 `✓ Ready` 等健康状态；
   - 用户可对同一 Product 多次连接为不同名称的 Provider 实例（如 `deepseek-1`、`deepseek-2`）。
2. **Verifying 与 WarningConfirm 严格分流**：
   - `Verifying` 统一调用 `connect_provider`；
   - 遇到系统硬错误（UDS 通信失败、409 冲突、非法配置校验失败、5xx 内部错误）时，直接进入 `Done` 失败终止，**绝不进入 WarningConfirm**，不可 Continue；
   - 仅当返回 `probe_failed` 或 `auth_not_ready` 时进入 `WarningConfirm`：
     - 用户选择 `Continue anyway`：设置 `skip_probe = true`，带上内存中保留的 `pending_account` 重新提交，由 Server 或本地所有权锁原子持久化；
     - 用户选择 `Back`：返回配置编辑页面，内存中的 `pending_account` 完整保留，避免重复登录；
   - 提交成功、报错终止或用户按 `Esc`/`q` 退出时，安全清空 `pending_account`。
3. **Provider 详情页模型重选与路由契约**：
   - `ProviderDetail` 页面提供 `[m] Models` 快捷键，进入当前 Provider 的模型重选；
   - 进入时预选当前已绑定的 models，允许用户取消既有 bindings 或选择新 bindings；
   - 提交时仅原子更新该 Provider 的 Model bindings，不触碰认证配置与 OAuth 账号；
   - 在详情页及从详情页进入的模型重选页按 `Esc`，始终返回已配置 Provider 列表并保持原光标位置，绝不跳转至新建 Provider 的 `ProductSelection` 页面。
4. **Provider 列表快捷键刷新契约**：
   - `ProviderManagement` 列表界面的 `[r] Refresh` 快捷键仅过滤并刷新当前配置中**已过期的 OAuth 账号**；未过期或非 OAuth provider 不进入候选批次。

#### 总体层级

```
Layer 1: Provider List
  ├─ [a] Add → Interactive creation flow (reuses provider add logic)
  ├─ [c] Copy → Same provider edit form as add, pre-filled from selected provider (**未实现**)
  ├─ [d] Delete → Delete with confirmation dialog
  ├─ [e] Edit → Open custom provider editor for selected provider
  ├─ [f] Fallback → Enter fallback configuration for selected provider
  ├─ [l] OAuth login → OAuth login flow (OAuth providers only)
  ├─ [r] Refresh → Re-probe provider status
  ├─ [u] Usage → Display usage statistics for selected provider
  ├─ [/] Search → Filter providers by name or product
  └─ [Enter] → Layer 2

Layer 2: Provider Detail
  ├─ Basic info (type, account, status, expiration)
  ├─ Usage area (window / balance / not displayed)
  └─ [r] Reset → Tiered confirmation → Result

Navigation: vim style
  Enter → Enter next layer
  Esc   → Return to previous layer
```

#### 第 1 层：Provider 列表

```
┌─── 🔑 Provider Management ─────────────────────────────┐
│                                                       │
│  ✅ openai-sub-1        user@a.com     Pro   7d left   │
│  🔑 deepseek            api-key         Configured      │
│                                                       │
│  [a] Add  [d] Delete  [e] Edit  [f] Fallback  [l] Login  [r] Refresh  [u] Usage  [q] Quit │
└───────────────────────────────────────────────────────┘
```

选中某个 provider 时，底部快捷栏动态追加：
- 有 OAuth token 且后端支持用量 → `[u] Usage`
- API key 类型 → 不追加

**[a] 添加流程：**

按 `[a]` → 产品选择（复用 `provider add` 产品选择逻辑）：

```
┌─── Add Provider ────────────────────────────────────────┐
│                                                       │
│  Select product type:                                  │
│                                                       │
│  1. OpenAI Subscription (OAuth)                       │
│  3. DeepSeek API (API Key)                            │
│  4. Custom provider...                                │
│                                                       │
│  [Enter] Confirm    [Esc] Cancel                      │
└───────────────────────────────────────────────────────┘
```

选择后，跟随对应流程：
- **OAuth 成熟产品** → 命名 → OAuth 登录 → 选择模型 → 用所选模型验证连通性 → 写入 provider 配置 + 认证状态 + 所选模型绑定
- **API Key 成熟产品** → 命名 → 输入 API Key 环境变量 → 选择模型 → 用所选模型验证连通性 → 写入 provider 配置 + 所选模型绑定
- **Custom** → 命名 → 输入 type + endpoint_url + api_key_env/no-api-key → 只写 provider 配置；不即时选模型、不做即时连通性验证

**[c] 复制流程（**未实现**）：**

在选中的 provider 上按 `[c]`，打开与 add 相同的 provider 编辑表单，并从所选 provider 预填。默认新 provider ID 为 `SOURCE-<num>`，其中 `<num>` 是第一个未用的正整数。

对 OAuth provider，表单拷贝产品与端点设置，但认证状态留空。用户必须确认或修改生成的 provider ID，并为该新 provider ID 完成一次全新 OAuth 登录，拷贝才保存为已配置。这使用户能在浏览器/设备码登录流中选择另一个账号。

对 API key provider，保存被阻止，直到用户显式输入或确认凭据环境变量。TUI 不得从 provider ID 生成或建议环境变量名；命名决定归用户所有。密钥值本身从不展示或拷贝。

**列表内快捷用量刷新：**

按 `[u]` → 展示所选 provider 的用量统计：

```
  📊 openai-sub-1 — 输入: 1.2M 输出: 340K 总计: 1.5M tokens | 156 次请求
```

#### 第 2 层：Provider 详情

按 `Enter` 或 `l` 进入：

```
┌─── 📋 Provider Detail — openai-sub-1 ─────────────────┐
│                                                       │
│  Type:     OpenAI Subscription (Device Code)          │
│  Account:  user@a.com                                 │
│  Status:   ✅ authenticated                           │
│  Expires:  2026-07-28 (7d remaining)                  │
│                                                       │
│  ─── Usage ──────────────────────────────────────────  │
│                                                       │
│  Plan: Pro                                            │
│                                                       │
│  5-hour   ████████████░░░░░░░░  62%   reset 2h 31m    │
│  Weekly   ██████░░░░░░░░░░░░░░  30%   reset 3d 12h    │
│                                                       │
│  Available reset credits: 2                           │
│                                                       │
│  [r] Reset usage    [u] Refresh    [Esc/h] Back      │
└───────────────────────────────────────────────────────┘
```

按量付费产品：
```
│  ─── Usage ──────────────────────────────────────────  │
│                                                       │
│  Plan: Usage-Based                                    │
│  Balance: $12.34                                      │
│                                                       │
│  [u] Refresh    [Esc/h] Back                          │
└───────────────────────────────────────────────────────┘
```

无重置按钮。

API key 类型：不展示用量区域。

#### TUI 重置流程

TUI 中的重置遵循同一分层确认策略：
- 已用尽 → 直接执行
- 未用尽 → 二次确认弹窗
- 用量 < 50% → 二次确认 + 三色警告

#### TUI 状态机

状态转移：

```
List ──[+]──→ AddSelectProduct ──[Enter]──→ AddConfigure ──→ (creation flow) ──→ List
                                    └─[Esc]──→ List

List ──[c]──→ CopyConfigure ──→ (copy/save, optional OAuth login) ──→ List
                         └─[Esc]──→ List

List ──[Enter/l]──→ ProviderDetail
ProviderDetail ──[Esc/h]──→ List

List ──[u]──→ UsageConfirm ──[Enter]──→ (async query) ──→ List (update summary)
                          └─[Esc]──→ List

ProviderDetail ──[r]──→ Check usage state
    ↓ (limit_reached)          Execute directly ──→ ResetResult
    ↓ (used_percent >= 50%)    ResetConfirm ──[Enter]──→ ResetLoading ──→ ResetResult
    ↓ (used_percent < 50%)     ResetConfirm ──[Enter]──→ ResetWarnLowUsage ──[Enter]──→ ResetLoading ──→ ResetResult
```

##### 12.4.7 Provider 管理面板

裸 `llm-proxy provider` 进入 **Provider 管理面板** —— 全部已配置 provider 的列表视图。这是默认 TUI 入口。

**Provider 列表行展示：**

| 字段 | 来源 | 示例 |
|-------|--------|---------|
| Provider 名 | `config.toml` 键 | `deepseek`, `kimi-work` |
| 产品 | 按端点 URL 匹配 catalog，否则 `Custom` | `DeepSeek API`, `Kimi Platform CN` |
| 状态 | 运行时探测 | `✓` ok / `⚠` 需登录 / `✗` 错误 |
| 协议 | 存在的端点字段 | `chat · responses · anthropic` |

**列表视图按键绑定：**

| 键 | 动作 |
|-----|--------|
| `a` | 添加 provider → 产品选择向导 |
| `c` | 拷贝所选 provider → CopyConfigure |
| `Enter` | Provider 详情视图 |
| `d` | 带确认对话框删除 |
| `e` | 编辑 provider（打开自定义 provider 编辑器） |
| `f` | 所选 provider 的 fallback 配置 |
| `l` | OAuth 登录（仅 OAuth provider） |
| `r` | 刷新状态（重新探测） |
| `u` | 所选 provider 的用量统计 |
| `/` | 按名称或产品搜索/过滤 |
| `q` | 退出 |

**删除确认**在移除前检查模型绑定。若 provider 被模型绑定引用，对话框展示引用它的模型，并要求 `[f]` 强制删除或取消。这保持了“provider 命令不静默编辑模型配置”的规则。

**产品识别**：加载 provider 时，其端点 URL 与内置 catalog 匹配。命中则展示 catalog 产品的显示名；未命中展示 `Custom`。同一产品可有多个 provider 实例（如 `kimi-platform-cn`、`kimi-work`、`kimi-personal` 都映射到“Kimi Platform CN”）。

### 12.5 `model` 命令 — 管理模型

#### 入口行为

| 用法 | 行为 | 模式 |
|-------|----------|------|
| `model` | stdin/stdout 为交互式时进 TUI 模型列表，否则报用法错误 | TUI |
| `model --select X` | TUI 模型详情/编辑便捷入口 | TUI |
| `model list` | CLI 文本列出所有模型 | CLI |
| `model info MODEL` | CLI 文本详情 | CLI |
| `model rename OLD_MODEL NEW_MODEL` | 重命名模型 ID 并更新引用它的绑定 | CLI |
| `model add MODEL ...` | 创建前端模型 | CLI |
| `model remove MODEL [--force]` | 删除前端模型 | CLI |
| `model set MODEL ...` | 原子编辑模型参数/特性 | CLI |
| `model provider add/set/remove/move MODEL ...` | 管理按协议的 provider 绑定 | CLI |

模式与 `provider` 一致：
- **裸 `model`** → 仅在交互式终端进 TUI；非交互调用返回用法错误。
- **有子命令** → CLI 执行。
- 模型编辑用子命令，而不是互斥的动作标志。

#### 完整命令矩阵

```text
# 入口
model                                           → TUI 列表
model --select X                                → TUI 详情/编辑（仅交互式便捷入口）
model list                                      → CLI 文本列表

# 查看
model info MODEL                                → CLI 文本详情

# 创建/删除/重命名前端模型
model add MODEL --context-window 200000 --max-output 8192
model add MODEL --copy-from SOURCE_MODEL        → 完整拷贝，含有序的 provider 绑定；源不变
model add MODEL --route LEVEL=TARGET [--route LEVEL=TARGET ...]  → 创建按思考等级分发的路由模型
model rename OLD_MODEL NEW_MODEL                → 重命名模型并更新引用它的绑定
model remove MODEL                              → 二次确认后删除
model remove MODEL --force                      → 不确认删除

# 模型参数与特性；同一命令中的所有标志原子生效
model set MODEL --context-window 200000         → 设置上下文长度
model set MODEL --max-output 8192               → 设置最大输出
model set MODEL --thinking-level high           → 设置默认思考等级
model set MODEL --enable-thinking               → 启用思考
model set MODEL --disable-thinking              → 禁用思考
model set MODEL --enable-feature image_input    → 启用特性
model set MODEL --disable-feature image_input   → 禁用特性
model set MODEL --context-window 200000 --thinking-level high --enable-feature image_input
model set MODEL --description "Model description" [--clear-description]
model set MODEL --chat-reasoning-off-format enable-thinking [--clear-chat-reasoning-off-format]
model set MODEL --service-tier priority [--default-tier | --forced-tier] [--clear-service-tier]
model set MODEL --cooldown-network 30 --cooldown-server-error 300 [--clear-cooldown]

# Provider 绑定
model provider add MODEL --type anthropic --provider anthropic-direct --upstream-model claude-sonnet-20250514
model provider add MODEL --type openai-chat --provider openrouter
model provider set MODEL --type anthropic --provider anthropic-direct --upstream-model claude-sonnet-20250514
model provider remove MODEL --type anthropic --provider anthropic-direct
model provider move MODEL --type anthropic --provider anthropic-direct --to 1
```

#### 模型命令校验

- `info`、`set`、`remove` 与 provider 绑定编辑要求 `MODEL` 已存在。
- `model add MODEL` 创建新前端模型 ID，已存在则失败。`display_name` 可跨模型重复；模型 ID 不可。
- `model add MODEL --copy-from SOURCE_MODEL` 把完整的 `[models.SOURCE_MODEL]` 条目拷贝进新 `[models.MODEL]` 条目，保留全部参数与三个协议绑定列表的原有顺序。CLI 与 TUI 拷贝都用这种完整拷贝行为；源、其 launch 配置与现有客户端保持不变。在 TUI 拷贝流中，用户尚未输入目标 ID 时，默认目标 ID 生成为 `SOURCE_MODEL-<num>`，`<num>` 递增直到 ID 未被占用。裸 `model` 打开独立的模型管理 TUI；`model --select X` 打开该模型的详情。已有模型的标量字段经 `model set` 立即写入；add/copy 草稿只在 `[s]` 时写入。
- `--copy-from` 是完整克隆：不得与 `--context-window`、`--max-output` 或 `--upstream-model` 组合。要调整拷贝模型的窗口限制，添加后运行 `model set`。
- `model rename OLD_MODEL NEW_MODEL` 把 `[models.OLD_MODEL]` 重命名为 `[models.NEW_MODEL]`。若 `OLD_MODEL` 不存在或 `NEW_MODEL` 已存在则失败。重命名原子迁移模型表键，保留全部参数与 provider 绑定，并更新所有 `binding.model == OLD_MODEL` 的引用绑定。在 TUI 中，模型重命名经模型条目上的 `[r]` 快捷键进入，写入前有实时冲突校验与二次确认。launch 管理的客户端配置在下次 `launch` 执行时刷新。
- 不带 `--copy-from` 的 `model add MODEL` 必须至少提供 `--context-window` 与 `--max-output`；provider 绑定通常用 `model provider add` 添加。
- `model add MODEL --route LEVEL=TARGET` 创建路由模型，按思考等级分发到目标模型。`--route` 是与裸 `model add`、`--copy-from` 并列的第三种创建路径；与 `--copy-from`、`--context-window`、`--max-output` 互斥。每个 `--route` 把一个思考等级（如 `low`、`medium`、`high`）映射到既有目标模型 ID。至少需要一个 `--route`；所有目标模型必须已存在。
- `model remove MODEL` 在二次确认后删除完整前端模型条目；`--force` 跳过确认。删除模型从不删除 provider 或 provider 凭据。launch 管理的客户端配置不会立即重写；下一条 `launch` 命令刷新客户端模型目录。
- `--context-window` 与 `--max-output` 必须是正整数。
- `--enable-thinking` 与 `--disable-thinking` 互斥。
- 存在有效 `reasoning_levels` 表（Model 级或继承自 Provider）时，`--thinking-level LEVEL` 必须在该表中；否则命令失败并列出支持的值。
- `--enable-feature` / `--disable-feature` 只接受已知特性名。
- `model provider add` 需要 `--type`；合法值：`openai-chat`、`openai-responses`、`anthropic`。
- `model provider add` 需要一个端点图声明了所请求协议的既有 provider。
- `model provider add` 成功时回显生效的上游模型。
- `model provider set` 就地编辑既有绑定的上游模型，保持其相对顺序与该协议下已配置 provider 间的 fallback 优先级。
- 同一模型/协议绑定列表中，一个 provider 不得出现两次。
- 省略 `--upstream-model` 时默认为前端模型 ID。
- 所有编辑先校验完整配置，再一次性原子写入提交。

#### 模型配置结构

config.toml 中单个模型的完整字段（v3 扁平格式）：

```toml
[models.claude-sonnet-lp]
# ── 模型级参数 ──
display_name = "Claude Sonnet"
context_window = 200000
max_output_tokens = 8192
features = ["image_input", "tool_call_reasoning"]
default_think = true
default_reasoning_level = "medium"

# reasoning_levels：单表存允许等级 + 映射
# [[models.claude-sonnet-lp.reasoning_levels]]
# level = "low"
# [[models.claude-sonnet-lp.reasoning_levels]]
# level = "medium"
# [[models.claude-sonnet-lp.reasoning_levels]]
# level = "high"

# ── 按协议的 provider 绑定 ──
openai_chat_providers = [
  { name = "openrouter", model = "anthropic/claude-sonnet" },
]
anthropic_providers = [
  { name = "anthropic-direct", model = "claude-sonnet-20250514" },
]
openai_responses_providers = [
  { name = "openrouter-resp", model = "anthropic/claude-sonnet" },
]
```

字段说明：

| 字段 | 类型 | 说明 |
|-------|------|-------------|
| `display_name` | string? | 可选的用户可见展示标签；不参与模型 ID 解析。 |
| `context_window` | int | 上下文窗口大小 |
| `max_output_tokens` | int | 最大输出 token 数 |
| `features` | []string | 特性开关（image_input、tool_call_reasoning 等） |
| `default_think` | bool | 默认思考开/关 |
| `force_think` | bool | 强制开思考 |
| `force_no_think` | bool | 强制关思考 |
| `supports_thinking` | bool | 能力：模型可思考 |
| `reasoning_levels` | []{level, api_value?} | 允许等级 + 映射表；取代旧的 `supported_reasoning_levels` + `reasoning_level_map` |
| `default_reasoning_level` | string | 默认思考等级（必须在生效表中） |
| `reasoning_routes` | []{level, target} | 路由模型：按推理 `level` 转发到 `target` 模型。每个 `target` 必须是普通模型——路由是单跳。与 provider 绑定及所有执行层字段互斥；空表是加载错误 |
| `{type}_providers` | []binding | 绑定到各协议类型的 provider 列表（顺序 = fallback 优先级） |

每个 provider 绑定包含：
- `name`：对既有 provider 名的引用
- `model`：该 provider 上的模型名（可跨 provider 不同）

#### `list` 输出

```
$ model list

claude-sonnet-lp         200K  thinking:medium  image_input  3 providers
deepseek-v3-lp           64K   thinking:off                    1 provider
gpt-5.5-lp               272K  thinking:medium  image_input  2 providers
```

#### `info` 输出

```
$ model info claude-sonnet-lp

model:          claude-sonnet-lp
context_window: 200000
max_output:     8192
thinking:       enabled (default: medium, supported: low/medium/high)
features:       image_input, tool_call_reasoning

provider bindings:
  openai-chat:
    [1] openrouter        → anthropic/claude-sonnet
    [2] openrouter-2      → anthropic/claude-sonnet
  anthropic:
    [1] anthropic-direct  → claude-sonnet-20250514
  openai-responses:
    (none)
```

#### `model provider add/remove/move` 流程

完整参数（所有标志显式、非交互）：
```
$ model provider add claude-sonnet-lp \
        --type anthropic \
        --provider anthropic-direct \
        --upstream-model claude-sonnet-20250514

✅ Added provider "anthropic-direct" (upstream model: claude-sonnet-20250514) to claude-sonnet-lp [anthropic]
```

省略 `--upstream-model`（默认为模型自身名）：
```
$ model provider add claude-sonnet-lp --type openai-chat --provider openrouter

✅ Added provider "openrouter" (upstream model: claude-sonnet-lp) to claude-sonnet-lp [openai-chat]
```

省略 `--type`，CLI 模式报错要求显式指定：
```
$ model provider add claude-sonnet-lp --provider openrouter

Error: --type is required. Valid types: openai-chat, openai-responses, anthropic
```

交互式 provider/类型选择只在 TUI 编辑界面提供。

#### TUI 模型列表操作

模型列表支持：

- `[+] Add model` — 以空/默认值打开模型编辑表单。表单在写入前编辑未来的 `[models.<id>]` 条目。
- `[c] Copy model` — 打开与 add **同一个模型编辑表单**，但从所选既有模型预填。默认新模型 ID 为 `SOURCE-<num>`，其中 `<num>` 是避开既有模型 ID 的最小正整数（`claude-sonnet-lp-1`、`claude-sonnet-lp-2`、…）。用户可保留该生成 ID，或在保存前改成任意合法未占用的模型 ID。
- `[-] Delete model` — 确认后删除所选前端模型；从不删除 provider 或凭据。
- `[Enter/l] Detail` — 打开模型详情/编辑视图。
- `[q] Quit`。

add 与 copy 有意收敛到同一个编辑区，使用户在提交前审阅/修改同一批字段：模型 ID、上下文窗口、最大输出、思考默认值、特性、按协议的 provider 绑定与上游模型名。拷贝等价于 TUI 版的“拷贝一个 `[models.X]` TOML 块、粘贴为 `[models.Y]` 再编辑粘贴块”——但带校验，字段不会漏或敲错。

被拷贝的模型是独立的。拷贝在原模型旁创建一个独立的新模型（原模型 ID 仍有效），而 rename 迁移既有模型 ID 并更新引用绑定。

TUI 配置不是隐藏的运行时数据库。它的持久产出就是人可以手改的同一份用户配置文件。经 TUI 配置完一台机器后，得到的配置文件可拷到另一台机器作为模型/配置事实源，前提是那台机器提供所引用的环境变量与（适用时的）OAuth 认证状态。

#### TUI 详情/编辑界面

模型详情屏是单个 `[models.<id>]` 表的字段级编辑器。标量字段与 provider 列表字段平级展示。三个 provider 列表字段不折叠成一个泛化的“providers”区域；它们是独立可编辑字段，因为映射到独立的配置键与独立的客户端协议。

```
┌─── 📋 Model Detail — claude-sonnet-lp ────────────────┐
│                                                       │
│  model_id:                  claude-sonnet-lp          │
│  context_window:            200000                    │
│  max_output_tokens:         8192                      │
│  default_think:             true                      │
│  default_reasoning_level:   medium                    │
│  reasoning_levels:          low, medium, high         │
│  features:                  image_input, tool_call_reasoning │
│                                                       │
│  openai_chat_providers:      2 bindings               │
│  openai_responses_providers: 0 bindings               │
│  anthropic_providers:        1 binding                │
│                                                       │
│  [Enter] Edit selected field    [Esc/h] Back          │
└───────────────────────────────────────────────────────┘
```

字段行为：

- 选中标量字段打开标量编辑器：`context_window` / `max_output_tokens` 整数输入，`default_think` / `force_think` / `force_no_think` / `supports_thinking` 布尔开关，`default_reasoning_level` 等级选择器，`reasoning_levels` 表编辑器（增删行，含 level + 可选 api_value），`features` 多选。
- 选中 `openai_chat_providers` 打开只限 OpenAI Chat 协议范围的 provider 绑定编辑器。
- 选中 `openai_responses_providers` 打开只限 OpenAI Responses 协议范围的 provider 绑定编辑器。
- 选中 `anthropic_providers` 打开只限 Anthropic 协议范围的 provider 绑定编辑器。

#### TUI provider 列表字段编辑器

用户进入某个 `xx_providers` 字段时，UI 只管理该列表。主视图是按 fallback 顺序已配置的 provider 绑定列表；底部快捷键帮助解释如何编辑选中绑定。以 `openai_chat_providers` 为例：

```
┌─── openai_chat_providers — claude-sonnet-lp ──────────┐
│ Configured providers (fallback order):                │
│                                                       │
│  › [1] deepseek-main      upstream: deepseek-v4-pro   │
│        endpoint: native openai_chat                   │
│    [2] openrouter         upstream: anthropic/claude-sonnet │
│        endpoint: native openai_chat                   │
│    [3] kimi-code-api      upstream: k3                │
│        endpoint: derived from openai_responses         │
│                                                       │
│ Available to add: 2 providers with openai_chat endpoint │
│                                                       │
│ [a] Add provider   [e] Edit upstream model            │
│ [-] Remove         [K] Move up   [J] Move down        │
│ [Enter] Detail     [s] Save      [Esc/h] Back         │
└───────────────────────────────────────────────────────┘
```

已配置列表是被编辑的主体，因为它直接映射 TOML 数组：

```toml
openai_chat_providers = [
  { name = "deepseek-main", model = "deepseek-v4-pro" },
  { name = "openrouter", model = "anthropic/claude-sonnet" },
  { name = "kimi-code-api", model = "k3" },
]
```

Provider 列表操作：

- `[a] Add provider` 打开一个过滤选择器，只列出声明了所选协议端点、且尚不在本 `xx_providers` 列表中的已配置 provider。选中后，UI 提示输入上游模型名；默认为前端模型 ID。
- `[e] Edit upstream model` 编辑当前选中 provider 绑定的 `model = "..."` 值。
- `[K] Move up` 与 `[J] Move down` 在当前列表内移动所选 provider，改变 fallback 优先级。允许方向键替代，但 UI 应展示显式快捷键，使行为可被发现。
- `[-] Remove` 在移除选中绑定前打开二次确认。默认选中动作必须是取消/不删除，因此不改变选择直接按 Enter 是安全的。
- `[Enter] Detail` 可只读展示 provider 端点/认证/状态详情以帮助用户决定排序，但对 provider 配置本身的编辑属于 provider TUI。
- `[s] Save` 校验完整配置并原子写入。带未保存修改退出编辑器时会询问保存、丢弃还是取消。

Provider 列表规则：

- 可选选择器只包含声明了所选协议端点的已配置 provider。对 `anthropic_providers`，provider 必须声明 `providers.<id>.anthropic`；对 `openai_chat_providers` 必须声明 `openai_chat`；对 `openai_responses_providers` 必须声明 `openai_responses`。
- 已在所选列表中的 provider 从可选选择器中排除。它们改为在已配置列表中编辑、移除或重排。
- 不支持的 provider 可默认隐藏，或带原因置灰展示，但不得可选。
- 重排限于当前 `xx_providers` 列表，直接改变该协议的 fallback 优先级。
- 保存时校验完整配置并原子写入。

这种字段范围的设计防止意外的跨协议编辑：编辑 `openai_chat_providers` 不会修改 `anthropic_providers` 或 `openai_responses_providers`。

#### 与 `provider` 命令的风格对比

| 维度 | `provider` | `model` |
|-----------|-----------|---------|
| 交互式入口 | `provider` → TUI | `model` → TUI |
| TUI 选中目标 | `provider --select X` | `model --select X` |
| 列表 | `provider list` | `model list` |
| 详情 | `provider info [NAME]` | `model info MODEL` |
| 修改 | 动作子命令（`remove`、`reset-usage`、auth 操作） | `set`、`provider add/remove/move` |
| 跳过确认 | 破坏性操作上的 `--force` | 预留给未来破坏性操作 |

风格一致：裸命令是 TUI 入口；可脚本操作用子命令。这避免了标志驱动的互斥动作分发，同时保留交互式便利。

### 12.6 Usage 数据源设计（Usage Data Source）

#### 12.6.1 核心原则

**Server 统一管理 usage 数据**：只有经过 server 的请求才统计。SQLite (`usage.db`) 为唯一权威持久化账本（SSOT），内存缓存存储近期及未落盘记录，消除多层存储盲区。

**记录范围**：`UsageRecord` 是**一次请求的完整记录**，含计量（tokens）、归属（model/provider/endpoint/token_key）、体验（`latency_ms`、`status`）、判定（请求固有结果标记）四组字段；**不含**需跨请求聚合、分位数或归因的诊断指标（TTFB、P95/P99、上游分段耗时、排队时长）——后者按用途选择诊断层落点。

#### 12.6.2 数据流

```text
Server 启动：
  磁盘（SQLite usage.db）→ 加载最多 max_cache_size 条最新记录 → 内存缓存（忽略遗留 JSONL）

Server 运行：
  转发请求 → 写入内存→ 定期智能落盘（直接写入 SQLite）

查询（CLI / TUI / Admin API）：
  合并查询：内存未落盘记录 ∪ SQLite 持久化记录 → 内存去重并按时间倒序排序 → 全量统一口径返回（彻底消除中间盲区）

Server 优雅关停：
  请求 drain 完成 → 显式 flush pending 直接写入 SQLite → 进程退出（不丢已 record 的账目，含终态 status）

Server 崩溃/强杀：
  未落盘内存丢失 → 重启时从 SQLite 加载（最多丢失"上次落盘到现在"的数据）
```

#### 12.6.5 独立模式兜底

**Server 未启动时**：CLI 直接读取 SQLite (`usage.db`)。

---

### 12.8 边界与限制

1. **仅适用于 OAuth provider**：API key 认证的 provider 没有用量 API；`provider info` 不展示用量区域；`provider reset-usage` 报告不支持
2. **按量付费产品无重置**：对 usage-based 套餐，`provider reset-usage` 报告不支持
3. **Token 过期**：若 access_token 已过期，`provider info` 用量区域提示先刷新 token
4. **网络依赖**：用量 API 需要访问 `chatgpt.com`，离线环境不可用
5. **API 未验证**：当前设计基于 Codex CLI 源码分析（可信度 1），实现需用真实 OAuth token 验证
6. **多窗口自适应**：部分 plan_types 可能只有一个窗口（primary_window 有值、secondary_window 为 None），UI 应自适应

### 12.9 `connect` 命令

`connect` 为脚本便利与既有用户习惯保留为成熟产品设置的用户友好别名（`provider add`）。在交互式终端中不带 `PRODUCT` 的 `connect` 打开 provider 添加向导，而不是完整的 provider 管理 TUI；完整 provider TUI 用裸 `provider`。对成熟 catalog 产品，非交互 `connect` 至少需要一个 `--model` 并在写入前验证；仅 provider 的自定义设置属于 `provider add --type ... --endpoint-url ...`。所有受支持的成熟产品标志原样透传：

```bash
# 以下两条等价
connect deepseek --api-key-env DEEPSEEK_API_KEY --model deepseek-v4-pro-lp
provider add deepseek --api-key-env DEEPSEEK_API_KEY --model deepseek-v4-pro-lp

# 以下两条也等价
connect
provider add
```

`connect` 实现直接调用 `provider::add()`：

```rust
Command::Connect(args) => {
    provider::add(args.into_provider_add_args())
}
```

详细的 `provider add` 设计见 §12.2。

## 13. Launch 命令设计（Launch Command Design）

Launch 命令生成指向代理地址的客户端配置（CLI 实际用来访问服务器的地址），使用哑/本地凭证。上游机密保留在 llm-proxy 配置/环境变量/token 存储中。

### 13.0 部署模式与 base URL 来源

Launch 支持两种互斥的部署模式，由配置决定：

| 模式 | 配置 | Base URL 来源 | 凭证来源 |
|------|--------|-----------------|-------------------|
| **Local**（server+client） | 存在 `[server]` | `cfg.server.listen` + 协议前缀 | 占位符 `"local"` |
| **Remote**（client-only） | 存在 `[remote]`、不存在 `[server]` | `cfg.remote.url` + 协议前缀 | 按认证模式（见下） |

`[server]` 与 `[remote]` 互斥；`Config::validate()` 对两者并存报错。

**`[remote]` 配置形态**：

```toml
[remote]
url = "https://llm-proxy.example.com"     # 必填：远程服务器 URL
auth_token_env = "LLM_PROXY_API_KEY"      # 可选：认证 token 的环境变量名；默认 "LLM_PROXY_API_KEY"
auth_mode = "token"                        # 可选："open" | "token"；省略时由环境变量是否存在推断
```

**按认证模式注入凭证**：

| 认证模式 | 条件 | Qwen/Claude 凭证 | Codex 凭证 |
|-----------|-----------|------------------------|------------------|
| **Open** | `auth_mode = "open"` 或环境变量未设置 | 占位符 `"local"` | 不注入（原生免 key） |
| **Token** | `auth_mode = "token"` 或环境变量已设置 | 环境变量中的真实 token | 写入 `<codex_home>/.env`，内容 `LLM_PROXY_API_KEY=<token>` |

**认证模式推断**：显式 `auth_mode` 优先。省略时，由（`auth_token_env` 命名的）环境变量是否存在决定：已设置 → Token，未设置 → Open。

**安全**：`[remote]` 只接受环境变量引用（`auth_token_env`），从不明文接收 token——与 provider `api_key_env` 设计一致。`config.toml` 从不包含明文机密。

**Base URL 构造**：`protocol_base_url(cfg, prefix)` 在 remote 模式返回 `remote.url + prefix`，local 模式返回 `local_server_base_url(listen) + prefix`。支持反向代理部署的路径前缀（如 `/lp/`）。

**Remote 鉴权与协议安全**：
- **Header 注入与 Fail-Closed**：CLI 在 Remote Token 模式执行 `launch` 时，向远端 `/admin/client-config/{client}` 发起请求时必须在请求头注入 `Authorization: Bearer <token>`。若配置为 Token 模式但环境变量未设置或为空，执行 Fail-Closed 阻断，严禁发出未鉴权裸请求；
- **强制 HTTPS**：非回环（非 `127.0.0.1` / `localhost`）的远程 URL 强制使用 HTTPS 协议，拒绝非加密 HTTP 传输凭据；
- **重定向安全防御**：3xx 重定向立即阻断且不消费 body，防止凭证被重定向泄露至外部；
- **错误响应脱敏**：任何上游或网络错误信息，在终端输出前必须过滤脱敏，剥离 `Bearer`、`token`、`Authorization` 及高熵字符串，防止控制台泄露；
- **凭据文件权限**：在 Unix 系统上，生成的包含敏感凭据的客户端配置文件（如 Pi `models.json`、Codex `.env`、Qwen `settings.json`）在写入时设置严格的 `0600` 权限（仅所有者读写）。

**管理端点租户模型过滤与路由物化**：
- `GET /admin/client-config/{client}` 端点同时挂载于公开 HTTP 监听器（经 `token_auth` 中间件）与本地 UDS；
- **租户模型过滤（Tenant Model Filtering）**：当请求由 Token 认证时，下发的 `models` 必须受 约束；未在 `allowed_models` 中的模型一律剔除；
- **路由模型物化与拓扑屏蔽（Routing Model Materialization & Hop Shielding）**：当授权列表中包含路由模型（Routing Model）时，服务端将其作为面向客户端的独立物化模型实体导出（保留思考档位 `supported_reasoning_levels`、协议 `protocol` 以及推导的上下文/输出边界），同时对其底层路由目标跳板模型（如 `*-low-lp`、`*-medium-lp`、`*-high-lp`）实施屏蔽隐藏；未授权给客户端的内部跳板模型绝不出现在下发的配置中，保护服务端内部路由拓扑不被泄露；
- **Provider 级联清理**：被过滤后不再被任何可见模型引用的 Provider 配置从 payload 中剥离，确保不暴露冗余上游凭证或端点；
- **全量兼容通道**：无 Token 约束（如本地 UDS 管理通道或 `allowed_models = []` 开放 Token）的请求保留全量模型下发。

**客户端本地与命令行级联模型过滤**：
- **三级级联收缩管道（Cascade Intersection Pipeline）**：
  $$M_2 = \text{ServerGranted} \cap \text{ConfigFiltered} \cap \text{CliFiltered}$$
  1. **第一级（服务端授权基线 $M_0$）**：来自服务端 `/admin/client-config/{client}` 下发的基础模型；
  2. **第二级（本地静态配置 $M_1$）**：若本地 `config.toml` 包含 `[launch]` 块：
     - `include_models = [...]`（别名 `models`）：若非空，保留 $M_0 \cap \text{include\_models}$；
     - `exclude_models = [...]`：若非空，剔除 $M_1 \setminus \text{exclude\_models}$；
  3. **第三级（命令行动态参数 $M_2$）**：若运行 `launch` 时传递命令行参数：
     - `--models <MODELS>`（别名 `--include-models`）：若非空，保留 $M_1 \cap \text{cli.models}$；
     - `--exclude-models <MODELS>`：若非空，剔除 $M_2 \setminus \text{cli.exclude\_models}$；
- **空交集防损坏保护（Fail-Fast）**：若最终模型集 $M_2$ 为空，`launch` 立即报错退出并明确展示服务端基线、配置保留项及命令行收缩规则，严禁向客户端写入空的无模型配置；
- **Provider 与 Codex Overrides 级联修剪**：未被任何保留模型引用的 Provider 配置自动从客户端导出中移除；`codex.auto_review_model` 与 `model_overrides` 中指向已被过滤模型的条目自动清理。

**模型列表一致性**（`--check` 标志）：以 Bearer 认证（Token 模式）拉取远程 `/admin/model/list`，与本地配置比对并报告漂移。不执行 launch。

**Admin 端点远程访问**：`/admin/*` 端点在公开 HTTP listener 上以 `token_auth` 中间件（Bearer token）暴露。UDS 仍是仅本机、无认证的通道。

**纯客户端模式**：存在 `[remote]` 而无 `[server]` 时，`status` 命令经 HTTP 探测远程服务器（`/admin/ping`、`/admin/status/probe`、`/admin/status`），而不是要求本地服务器。

### 13.1 架构边界与不变量

llm-proxy 与 AI 编码客户端之间的交互遵循严格的分层边界：

| 层 | 职责 | 禁区 |
|---|---|---|
| L1 服务器运行时（`serve`） | 监听、代理、计费/冷却、状态目录、管理套接字响应 | 客户端配置文件及其写路径 |
| L2 客户端 launch 接口（`launch` CLI） | llm-proxy 内客户端配置的唯一写者：显式命令、解析出的目标目录、只管受管字段 | 后台进程隐式执行、未经提示的客户端文件变更、非 launch 入口的副作用 |
| L3 客户端运行时 | 在原生隔离环境下读取自身配置 | — |

不变量：
1. **单一写路径**：客户端配置文件只由显式执行的 `llm-proxy launch` 写入。目标目录由 `--<client>-config-dir <dir>` 指定，或回退到解析出的默认目录。
2. **零 L1 → L3 写路径**：`serve` 在整个生命周期（启动、运行时管理请求、关停）中对客户端配置零写入。远程 launch 模式只经管理 API 响应配置数据；客户端侧 CLI 执行文件写入。
3. **非 launch 入口零副作用**：`--help`、shell 补全与其他子命令从不创建或修改客户端文件。
4. **写读对齐**：L2 写路径在原生隔离机制下与 L3 读路径对齐，受探测与验证级别约束。
5. **多实例隔离**：启动任何 `serve` 实例从不改变任何客户端配置文件。

#### 服务器启动提示

成功绑定监听套接字后，`serve` 输出单行 INFO 日志，指示其实际绑定的 host 与 port：
`llm-proxy listening on <addr>; client configs are not auto-modified — point clients with 'llm-proxy launch <client>'`
当 `server.listen = "127.0.0.1:0"` 时，`<addr>` 报告实际分配的 host 与 port。

### 13.2 Launch 命令参数契约

- 只有显式的 `llm-proxy launch <client>` 命令写客户端配置。合并只覆盖受管字段。
- 非写入入口：`--dry-run` 与 `--check` 只做校验，从不创建或修改文件。
- **统一参数语义**：所有 `--<client>-config-dir <dir>` 标志直接指向客户端配置目录本身（与客户端原生隔离环境变量 1:1 对应；例外：`claude-desktop` 无原生环境变量，只承诺写侧平台路径），而不是 `$HOME`。
- **默认目录解析规则**：按优先级解析：原生环境变量（若已设置）→ XDG 规则（opencode）→ `$HOME` 派生。
- 废弃的 `--*-home` 标志在 CLI 解析时报错，并指向 `--<client>-config-dir`。

#### 客户端 launch 映射与验证矩阵

| 客户端 | 标志 | 受管文件 | 默认目录解析 | 原生隔离环境变量 | 探测与验证级别 |
|---|---|---|---|---|---|
| codex | `--codex-config-dir` | `config.toml`, `llm-proxy-<hash>-model-catalog.json` | 已设置则 `$CODEX_HOME`，否则 `$HOME/.codex` | `CODEX_HOME` | 探测：`CODEX_HOME=<dir> codex exec --skip-git-repo-check "<prompt>"` |
| codex-desktop | `--codex-config-dir`（与 codex 共用） | `config.toml`, `llm-proxy-<hash>-model-catalog.json` | 已设置则 `$CODEX_HOME`，否则 `$HOME/.codex` | `CODEX_HOME` | 探测：复用 codex 探测（实现与目标共用）；GUI 环境继承未验证 |
| pi | `--pi-config-dir` | `models.json` | 已设置则 `$PI_CODING_AGENT_DIR`，否则 `$HOME/.pi/agent` | `PI_CODING_AGENT_DIR` | 探测：`PI_CODING_AGENT_DIR=<dir> pi -p --no-session --model <proxy/model> "<prompt>"`（直接写 `<dir>/models.json`） |
| qwen-code | `--qwen-config-dir` | `settings.json` | 已设置则 `$QWEN_HOME`，否则 `$HOME/.qwen` | `QWEN_HOME` | 探测：`QWEN_HOME=<dir> qwen -m <model> -p "<prompt>"` |
| claude-code | `--claude-config-dir` | `settings.json` | 已设置则 `$CLAUDE_CONFIG_DIR`，否则 `$HOME/.claude` | `CLAUDE_CONFIG_DIR` | 探测：`CLAUDE_CONFIG_DIR=<dir> claude -p "<prompt>"` |
| opencode | `--opencode-config-dir` | `opencode.json` | 已设置则 `$OPENCODE_CONFIG_DIR`，否则 `${XDG_CONFIG_HOME:-$HOME/.config}/opencode` | `OPENCODE_CONFIG_DIR`（分层优先级：global < custom < project） | 探测：`OPENCODE_CONFIG_DIR=<dir> opencode run "<prompt>"` |
| claude-desktop | `--claude-desktop-config-dir` | `claude_desktop_config.json` | macOS `~/Library/Application Support/Claude/`；Windows `%APPDATA%\Claude\`；Linux 需显式 `--claude-desktop-config-dir`（默认报错） | 无（无原生环境变量；仅写侧平台路径） | 仅写侧：已验证的平台路径与 Linux 双分支错误处理 |

### 13.3 共享 launch 不变量

所有 launch 命令必须：

- 加载并校验 llm-proxy 配置；
- 选择支持客户端协议的模型；
- 尽可能原子写入；
- 保留用户自有设置；
- 只替换 llm-proxy 管理的块/条目；
- 一致地支持 dry-run；
- 支持用于隔离测试的路径覆盖标志；
- 从不把上游 API key 写入客户端配置；
- 注入客户端所支持协议暴露的全部已配置模型；
- 从不选择、替换或移除客户端当前模型、默认模型、provider 或 profile；
- 把模型选择与运行时切换留给客户端自身（`/model`、选择器、`--model` 或客户端等价物）。

### 13.4 Codex CLI 与 Codex Desktop

`llm-proxy launch codex` 与 `llm-proxy launch codex-desktop` 共享同一份 Codex 配置生成器。两个目标都必须：

- 生成/更新 Codex `~/.codex/config.toml` 与一个确定性的 llm-proxy 所有模型目录 JSON；
- 保留 `~/.codex/auth.json` 与无关的 `config.toml` 设置；
- 配置 `wire_api = "responses"`；Codex 当前源码拒绝 `wire_api = "chat"`；
- base URL 指向由 CLI 服务器地址（`server.listen`）派生的 Responses 兼容 `/openai/v1` 前缀，通常为 `http://127.0.0.1:8989/openai/v1`，以便 Codex 追加 `/responses`；
- 对无认证的本地代理省略不支持的内联 `api_key` 字段；若以后启用前端认证，使用 Codex 支持的 `env_key` 或 `experimental_bearer_token`；
- 经模型目录包含所有支持 Responses 的客户端可见模型；
- 存在时保留既有的 `model`、`model_provider`、`model_context_window` 与 `model_max_output_tokens` 字段，不存在时不创建它们；
- 把 Codex 的模型/provider 选择留给 Codex 自己的 `--model` 选项、TUI 选择器或等价物；
- 对 Codex Desktop 给出警告：由于上游 issue #19694，模型选择器可能过滤掉 `model_catalog_json` 模型；客户端侧直接选择仍是回退手段。

标志：

```text
--codex-config-dir PATH
--dry-run
--check
```

### 13.5 pi

`llm-proxy launch pi` 必须：

- 在 `--pi-config-dir <dir>`（或 `$PI_CODING_AGENT_DIR`，或 `~/.pi/agent`）指定的目标目录内直接生成/更新 `models.json`，不嵌套 `.pi/agent` 子目录；
- 保留非 llm-proxy provider 与无关的顶层字段；
- 只替换键以 `llm-proxy-` 开头的 llm-proxy 受管 provider；
- 为已配置模型暴露的每个客户端面向协议各生成一个受管 provider：
  - `openai_chat` → `llm-proxy-openai-chat`，Pi API `openai-completions`；
  - `openai_responses` → `llm-proxy-openai-responses`，Pi API `openai-responses`；
  - `anthropic` → `llm-proxy-anthropic`，Pi API `anthropic-messages`；
- 允许同一模型 ID 出现在多个协议 provider 中，以便 Pi 自行选择 provider/模型条目；
- 不设置也不覆盖 Pi 的默认模型或 provider 选择；
- 当没有已配置模型暴露对应协议时，省略该受管 provider。

Pi 专属连接与推理契约：

- Anthropic `baseUrl` 以 `/anthropic` 结尾；SDK 追加 `/v1/messages`。Chat 与 Responses 用 `/openai/v1`。
- 从显式模型声明而非模型名导出推理能力。激活的声明推理等级即使 `default_think=false` 也证明能力；只含 `none` 的非空列表则不证明。若无声明等级，启用的思考策略或激活的默认等级可提供显式证据。
- 面向 Pi 的 `thinkingLevelMap` 表示前端等级，而非上游 API 值：受支持等级用恒等值，`none` 映射为 Pi `off` 且值为 `none`，Pi 不支持的等级用 null 条目。上游 `reasoning_levels` 表仍归服务端所有。Chat 显式启用 Pi `compat.supportsReasoningEffort`。
- Anthropic 用 Pi 基于 budget 的模式，而非强制自适应模式。不广告独立的 `xhigh`，Pi 的 budget 实现会把它降为 `high`。只有当每个 fallback 候选都保持原生 Anthropic 思考语义（原生透传）时才广告 Anthropic 推理；否则保留不带推理能力的模型条目并给出清晰的 launch 诊断。
- 面向 Pi 的推理还要求至少有一个激活等级可被该协议表示。只有未知等级的声明、以及仅 Anthropic `xhigh` 会保留模型但不带 Pi 推理能力并发出诊断，而不是生成 `reasoning=true` 的全 null 映射。
- 例外：`force_think=true` 且无声明等级（思考常开、无等级控制，如 Claude extended thinking）导出 `reasoning: true` 但**没有** `thinkingLevelMap`、也没有 `compat`——没有可表示的等级控制，全 null 映射被省略而不是输出。
- 显式 Anthropic `thinking` 不被默认推理注入替换。`thinking.type=disabled` + `output_config.effort=E` 是合法组合（思考关，effort 控制输出强度）；当目标是 Chat/Responses 时退化为 `none`（丢弃 effort）。`thinking.type=enabled` + `budget_tokens` 已废弃并在本地拒绝（400）。既有的生效思考关闭策略（`force_no_think=true` 或 `default_think=false`）保持优先。抽象 effort/level 请求继续使用已配置的 `reasoning_levels` 映射。
- Launch 不写 Pi 的默认思考设置。然而广告推理可能激活 Pi 既有的默认思考等级；验证必须检查实际客户端请求语义，而不只是生成的 JSON。
- Anthropic budget 到 OpenAI effort 的转换不在本契约范围；不支持的派生路径不得广告为保持 budget 控制。

标志：

```text
--pi-config-dir PATH
--dry-run
```

### 13.6 Claude Code

`llm-proxy launch claude-code` 必须：

- 只注入本地 Anthropic 端点与认证配置；
- 保留既有 Claude Code 模型环境变量与无关设置；
- 从不创建或覆盖 `ANTHROPIC_MODEL`、`ANTHROPIC_DEFAULT_*_MODEL` 或其他当前模型选择变量；
- 把模型选择留给 Claude Code 的 `/model` 选择器、`--model`，或显式启用时的网关发现；
- 从不把第一个已配置的 llm-proxy 模型当作隐式默认。

标志：

```text
--claude-config-dir PATH
--dry-run
```

### 13.7 Claude Desktop / Claude Code Desktop

`llm-proxy launch claude-desktop` 必须：

- 生成 Claude Desktop 第三方网关 profile 配置，而不是 Claude Code `settings.json`；
- 支持 macOS 与 Windows 默认路径；Linux 除非显式提供 `--claude-desktop-config-dir <dir>` 否则默认报错；
- 管理一个确定性的、llm-proxy 所有且区别于 cc-switch 的 profile ID，并保留无关的 `_meta.json` 条目；
- 部分写入失败时对普通配置、3P 配置、网关 profile 与 `_meta.json` 做快照与回滚；
- 启用时把普通与 3P 两个标记文件都写为 `deploymentMode = "3p"`，恢复官方模式时都恢复为 `"1p"`；
- `inferenceGatewayBaseUrl` 设为由 CLI 服务器地址派生的网关挂载点，如 `http://127.0.0.1:8989/claude-desktop`，只带本地前端 token；从不写入上游 provider 机密；
- 暴露安全的类 Claude 路由 ID（`claude-sonnet-*`、`claude-opus-*`、`claude-haiku-*`）并在内部映射到真实 provider/模型绑定；
- 注入受支持模型列表，但不设置 `defaultModel`、`activeProfile` 或其他当前模型选择字段；
- 保留任何既有活动 profile 与模型选择；
- 把模型选择留给 Claude Desktop 自己的选择器；
- 只在解析出的目标模型 `context_window >= 1_000_000` 时生成 `inferenceModels[].supports1m`；不添加 `supports_1m` 配置字段；
- 配置变更后要求完整重启 Claude Desktop；不承诺热加载；
- 把路径追加行为、`/v1/models` 发现、流式传输与 `coworkEgressAllowedHosts` 必要性保持为发布前的冒烟验收项。

标志：

```text
--claude-desktop-config-dir PATH
--dry-run
--profile NAME
--create-profile NAME
--edit-profile NAME
--haiku MODEL
--sonnet MODEL
--opus MODEL
```

### 13.8 Qwen Code

`llm-proxy launch qwen-code` 必须：

- 生成/更新 `~/.qwen/settings.json`；
- 保留 permissions、hooks、MCP、隐私、无关设置与所有既有 Qwen 模型/provider 选择字段；
- 按 Qwen Code 文档配置 `modelProviders.openai`，含本地 `baseUrl`、`envKey` 与逐模型 `generationConfig` 字段（`contextWindowSize` 与 `maxOutputTokens`）；
- 需要时把认证选择设为 OpenAI 兼容模式；
- 包含所有支持 OpenAI Chat 的已配置模型；
- 在写入前把 `capabilities.reasoning.efforts` 与 `defaultEffort` 过滤到 Qwen Code 自己的 effort 词表（`low`、`medium`、`high`、`xhigh`、`max`）：词表外等级（`minimal`、`ultra`、`none`、`off`、…）被丢弃（路由模型额外丢弃禁用等级，与 `thinkingLevelMap` 的 `off` 抑制一致），`defaultEffort` 必须落在过滤后的集合内（否则回退到其第一项），过滤后集合为空时整个 `capabilities.reasoning` 块被省略——Qwen Code 对任何词表外取值会拒绝整条声明，这会静默禁用该模型的推理；
- 从不把模型思考关闭策略（`force_no_think=true` 或 `default_think=false`）投影进 Qwen `generationConfig.extra_body` 或其他固定 think-off 客户端字段；
- 从不创建或覆盖 `model.name`、`model.baseUrl` 或其他当前模型选择字段；
- 把模型选择与运行时切换留给 Qwen Code 的 `/model`、`--model`、选择器或等价物。

标志：

```text
--qwen-config-dir PATH
--dry-run
```

### 13.9 OpenCode

`llm-proxy launch opencode` 必须：

- 生成/更新 OpenCode 配置（`opencode.json` 或 `opencode.jsonc`）；
- 适用时保留 JSONC 文件中的注释与结构；
- 配置指向 llm-proxy 端点的 OpenAI 兼容 provider 条目；
- 保留既有用户自定义 provider；
- 遵守 OpenCode 的优先级规则（global < custom < project）。

标志：

```text
--opencode-config-dir PATH
--dry-run
```

## 14. 配置编辑与文档（Config Editing and Docs）

### 14.1 规范配置格式

`config.toml` 由机器管理。所有配置写入路径（`init`、`connect`、`provider`、`model`、TUI 编辑器、admin API）都经 `canonical_format()`，带层级间距（顶层节之间、同级 provider/模型之间 2 个空行，provider/模型子表内 1 个空行）。手写注释**不被保留**。输出是确定性的：同一个 `Config` 总产生相同字节。未知节在加载时被拒绝。

### 14.2 无迁移命令

当前 Rust 实现**不**提供 `llm-proxy migrate`。受支持的配置生命周期是：

1. 无配置时 `llm-proxy init` 创建全新的最小 llm-proxy 配置。
2. `llm-proxy connect` / `llm-proxy provider add` 添加 provider 配置。
3. `llm-proxy model` 编辑前端模型元数据与 provider 绑定。

旧的 Go/v1 配置不会被当前 Rust 实现原地重写。需要 llm-proxy 的用户应用 `init` 创建新配置，再经管理命令重新添加 provider/模型。这保持重写严格，避免在 schema 变更中静默臆造 provider 端点声明。

### 14.3 内嵌文档

当前 Rust 实现必须保留 `llm-proxy doc` 行为：

- 交互式时 `llm-proxy doc` 打开/浏览文档；
- `--list` 打印文档树；
- `--raw` 打印全部内容；
- `--section SECTION` 打印选定节；
- 内嵌文档从被文档化的源目录生成，若仍采用该分发模式则在 CI 中校验。

### 14.5 数据位置与敏感信息（Data Locations）

#### 14.5.1 核心原则：谁发请求，谁持有凭据

Server 是实际向上游 API 发请求的进程，因此 API Key、OAuth Token 必须在 Server 机器上可用。

#### 14.5.2 各数据的存放位置

| 数据 | 本地模式 | 远程模式 | 说明 |
|------|---------|---------|------|
| config.toml | Server 机器（共享 fs） | Server 机器 | CLI 通过 API 或文件访问 |
| api_key_env | Server 机器环境变量 | Server 机器环境变量 | CLI 设置的是变量名，实际值在 Server |
| OAuth tokens | Server 机器 state 目录 | Server 机器 state 目录 | Server 使用 token 访问上游 |
| usage 数据 | Server 机器 state 目录 | Server 机器 state 目录 | Server 记录每次请求的用量 |
| probe 验证结果 | Server 发起并存储 | Server 发起并存储 | Server 验证自己的网络可达性 |
| lock file | 同机器有效 | 不存在 | 远程模式不需要文件锁 |
| PID file | 同机器有效 | 不存在 | 远程模式无法检测进程 |
| 客户端配置 (.codex/...) | CLI 本地机器 | CLI 本地机器 | 从 Server 获取信息后写入本地 |

#### 14.5.3 远程模式下的环境变量问题

用户在本地 CLI 设置 `api_key_env = "DEEPSEEK_API_KEY"`，这个环境变量指的是 **Server 机器上的环境变量**，不是 CLI 本地的。

**处理方式：**
1. CLI 发送 `ProviderAdd { api_key_env: "DEEPSEEK_API_KEY" }` 给 Server
2. Server 检查自己的环境中是否存在该变量
3. 不存在 → 返回警告：`"env var DEEPSEEK_API_KEY not set on server"`
4. 存在 → 发 probe 请求验证连通性

**设置远程环境变量的方式（分阶段）：**
- **Phase 1（当前）**：用户 SSH 到 Server 手动设置
- **Phase 2（后续）**：Admin API 提供 `/admin/env/set` 端点（需加密传输）

#### 14.5.4 远程模式下的 Launch/Connect

```
用户在本地：llm-proxy launch codex

CLI 流程：
1. GET /admin/client-config/codex（附带 Bearer token 鉴权头） → Server 返回过滤后的配置内容（模型/协议/能力/auth）
2. CLI 用 server.listen 的 origin 推导 base_url（地址 = CLI 实际访问 server 的地址）
3. 翻译成本地客户端格式
4. 写入本地 ~/.codex/config.toml

写入的内容指向本地/远程 Server：
  [model_providers.remote]
  base_url = "<server.listen origin>/openai/v1"
```

**地址来源原则**：base_url 由 **CLI 推导**（用 `server.listen` origin + 协议前缀），**server 不返回 base_url**——server 不知道 CLI 是从哪个地址访问它的，返回地址反而可能错。CLI 与目标客户端处于同一网络位置，CLI 可达 server 的地址即客户端可达地址。launch 不提供 `--proxy-url` 类覆盖 flag。

---

## 15. 状态、缓存与安全（State, Cache, and Security）

llm-proxy 应保持用户数据位置可预测且安全：

- 配置与 OAuth 账号位于 XDG config 类别（`$XDG_CONFIG_HOME/llm-proxy`，或固定的平台配置回退）；
- 持久状态位于 XDG state 类别（`$XDG_STATE_HOME/llm-proxy/<identity>`，Unix 上为 `~/.local/state/llm-proxy`），含日志、冷却、状态缓存与用量；
- 运行时文件位于 XDG runtime 类别（`$XDG_RUNTIME_DIR/llm-proxy/<identity>`，Unix 上为 `~/.local/state/llm-proxy/runtime`），含 pid、socket、ownership 与配置锁；
- 每个类别只读自己对应的 XDG 变量，从不从其他类别的变量派生回退；
- OAuth 账号材料存于一个带限制性文件权限的统一类型化存储；与 Go/v1 不同，当前 Rust 实现不得把 OpenAI OAuth 材料拆到不同文件；
- 含环境变量名的配置文件不是机密，但任何误入的字面 API key 应在输出中掩码；
- 日志与状态必须掩码 API key、bearer token、设备码、refresh token 与授权头；
- 管理套接字必须限于本地用户，不得经 TCP 暴露。

### 15.1 可测试性与隔离（Testability and Isolation）

路径按生命周期类别解析，不提供项目私有路径覆盖变量：

| 类别 | 唯一环境变量 | Unix 默认路径 | 测试隔离方式 |
|---------|---------|---------|---------|
| 配置与 OAuth | `XDG_CONFIG_HOME` | `~/.config/llm-proxy/` | 临时 config root |
| 持久 state、cache、usage、日志 | `XDG_STATE_HOME` | `~/.local/state/llm-proxy/<identity>/` | 临时 state root |
| PID、socket、ownership、config lock | `XDG_RUNTIME_DIR` | `~/.local/state/llm-proxy/runtime/<identity>/` | 临时 runtime root |

每一类只读取对应的 XDG 环境变量；变量缺失时使用该类固定默认路径，禁止从其他 XDG 变量推导 fallback。`XDG_*` 变量适用于进程级 E2E 隔离；单元测试应优先使用显式 cache/runtime path 构造器，不修改进程全局环境。

**进程级隔离示例**：

```bash
E2E_ROOT=$(mktemp -d)
mkdir -p "$E2E_ROOT/config" "$E2E_ROOT/state" "$E2E_ROOT/runtime"
XDG_CONFIG_HOME="$E2E_ROOT/config" \
XDG_STATE_HOME="$E2E_ROOT/state" \
XDG_RUNTIME_DIR="$E2E_ROOT/runtime" \
llm-proxy --config-dir "$E2E_ROOT" serve --foreground
```

配置、持久 state 和 runtime 必须使用不同目录；多实例仍需确保监听端口不冲突。

## 17. Status 命令设计（2026-08-01）

### 17.1 核心原则

**读写分离架构**：
- **写操作**（provider/model CRUD、OAuth token 写入）：仅 UDS（本地管理通道）
- **读操作**（status 查询、launch 数据获取）：HTTP 公开端点（与转发服务同端口）

### 17.2 两种行为不同的场景

#### 场景 1：CLI 与 Server 在不同机器/容器（远程模式）

**环境**：
- CLI 在容器中，Server 在宿主机上
- CLI 配置只有 `server.listen`（HTTP 公开端点地址）
- CLI **无法直接访问 providers**（无 API key、无凭据文件）
- CLI 只能经 HTTP 公开端点与 Server 通信

**行为**：
- 无标志：从 Server HTTP 端点获取实时状态（活动 provider 列表 + Server 缓存的探测结果）
- `--probe`：**请求 Server 执行探测**（CLI 无法自行探测，必须委托 Server）

#### 场景 2：CLI 与 Server 同机（本地模式）

**环境**：
- CLI 与 Server 都在宿主机（或同一容器）
- CLI 可访问 Server 的 UDS（管理通道）+ HTTP 公开端点
- CLI **有 API key 与凭据**（config.toml 含 `api_key_env` 引用）

**行为**：
- **Server 运行中**：
  - 无标志：从 Server 获取实时状态（经 UDS 或 HTTP）
  - `--probe`：**请求 Server 执行探测**（委托模式，同场景 1）

- **Server 未运行**：
  - 无标志：读本地缓存（status_cache.json）
  - `--probe`：**CLI 自己执行探测**（独立模式，有 API key）

#### 场景 3：client-only 节点（本地无 Server，配置仅 `[remote]`）

**环境**：
- 本地配置只有 `[remote]`、无 `[server]`（纯客户端部署）；本地无后台进程、无本地 provider 凭证
- CLI 经 `[remote].url` 公开端点与远端 Server 通信（认证按 `[remote]` 认证模式）

**行为**：
- Runtime State：标注 `client-only (remote) — 本地无后台进程`（不查本地套接字）
- 元数据渲染源：远端配置镜像（`GET /admin/config/snapshot`），Providers/Models 树按镜像渲染；快照失败 fail-fast 非零退出（不回退本地空口径）
- Providers 凭证列：`N/A` + `remote-managed`（本地不核验，避免全行假 WARN）
- `--probe`：委托远端执行（同场景 1）

### 17.3 探测逻辑

**核心洞察**：**持续成功的请求 = 已经探测成功**（活动证据）

```text
对每个 provider：
├─ 有持续成功的请求吗？
│   └─ 有 → 跳过（视为探测成功）
└─ 没有？
    ├─ Server 运行中 → 请求 Server 探测
    ├─ Server 不可达 + 有凭据 → CLI 自己探测
    └─ Server 不可达 + 无凭据 → 读缓存（无法探测）
```

**“需要探测”的定义**：没有持续成功请求的 providers。

### 17.4 标志语义

| 标志 | 行为 |
|------|----------|
| 无标志 | 展示当前状态（优先来自 Server 的实时数据，回退缓存） |
| `--probe` | 探测“**没有持续成功请求**”的 providers |

**不保留 `--refresh`**（干净全新设计，无历史包袱）。

### 17.5 实现要点

**服务端**：
- 维护“活动 provider 列表”（内存，记录成功转发过的 providers + 时间戳）
- HTTP 只读端点：`/admin/status`（返回活动列表 + 缓存的探测结果）
- 探测委托：`POST /admin/status/probe`，**只在管理通道上**（触发探测、返回结果；副作用端点——不暴露在公开 listener 上，见 §11.8）

**CLI 端**：
- 检测模式（远程/本地 + Server 可用性）
- `--probe`：取活动列表 → 对非活动 providers 触发探测（委托 Server 或独立执行）

### 17.6 并发探测防滥用（真 Singleflight）

**核心原则**：**三层判定 + (provider, model) 对级真 Singleflight**

#### 三层判定流程

```text
请求探测 (provider_id, model_id)
│
├─ L1：活动 provider 判定（30s TTL）
│   └─ 是 → 不探测，返回“活动”
│
├─ L2：缓存结果判定（5s 窗口）
│   └─ 有有效缓存 → 不探测，返回缓存结果
│
└─ L3：Singleflight 合并
    ├─ 同一 (provider, model) 正在探测 → 等待并共享结果
    └─ 无探测进行中 → 执行探测，通知等待者
```

#### 超时策略

| 维度 | 设置 |
|-----------|---------|
| **探测超时** | 30 秒 |
| **等待者超时** | 30 秒（与探测超时相同） |
| **超时状态** | 展示为“Timeout/Unreachable” |

#### 并发策略

| 维度 | 策略 |
|-----------|----------|
| **不同 providers** | **并发**（提高效率） |
| **同一 provider 的不同模型** | **串行**（避免触发 RPM） |
| **同一 (provider, model)** | **Singleflight 合并**（避免重复） |

### 17.7 探测粒度分析（Provider 级 vs 模型级）

- **分析**：
  - **模型级探测**：为 Provider 绑定的每个模型发探测，使上游探测请求成倍增加，易触发 429 限流；模型通常共享同一 API Key 与 Base URL。
  - **Provider 级探测**：对每个 Provider 验证网络连通性、API Key 认证与主要 API 路由。单个模型的临时过载/不可用，依赖框架既有的 **Cooldown & Fallback 重试机制**在实际调用时做实时流量切换。
- **决策**：探测粒度保持 **Provider 级**（覆盖代表性协议端点），确保低延迟与低资源消耗。

**注意**：Singleflight 合并粒度是 **(provider, model) 对级**（§17.6），但实际探测执行是 Provider 级。

### 17.8 探测结果刷盘责任与原子性

- **Server 运行场景**：探测结果由 **Server 统一**写盘（更新 `status_cache.json`），遵循核心层“单写者”原则。
- **Server 未运行场景（CLI 离线）**：CLI 自己执行探测时，必须先写临时文件再 rename 覆盖做原子写盘，防止并发写损坏 JSON。

### 17.9 活动 Provider 活动证据 TTL 调优（30 秒）

- **决策**：活动证据默认 TTL 生效窗口设为 **30 秒**。
- **理由**：上游 Provider 访问限流（如 RPM）通常以分钟（60s）为基本刷新周期。活动证据窗口设为滚动刷新周期的一半（30s），在“超快响应/跳过多余探测”与“及时发现服务故障”之间达到最优平衡。

### 17.10 状态提示与用户反馈

**核心原则**：让用户清楚知道数据来源与局限。

#### 提示符号

- `ℹ` = 普通信息（实时数据）
- `✓` = 成功执行了探测
- `⚠` = 降级模式（缓存数据，可能过时）
- `✗` = 无法获取数据（错误）

#### 场景矩阵

| 场景 | Server | 缓存 | --probe | 行为 | 提示 |
|----------|--------|-------|---------|----------|--------|
| 本地 + Server 运行 | ✅ | - | 否 | Server 实时 | `ℹ Data from local Server (real-time)` |
| 本地 + Server 运行 | ✅ | - | 是 | Server 探测 | `✓ Requested Server to probe inactive providers` |
| 本地 + Server 未运行 + 有缓存 | ❌ | ✅ | 否 | 读缓存 | `⚠ Data from local cache (Server not running)` |
| 本地 + Server 未运行 + 有缓存 | ❌ | ✅ | 是 | **CLI 探测（忽略缓存）** → 成功 | `✓ Executed local probe` |
| 本地 + Server 未运行 + 有缓存 | ❌ | ✅ | 是 | **CLI 探测（忽略缓存）** → 失败 → 回退缓存 | `⚠ Local probe failed, fell back to cache data` |
| 本地 + Server 未运行 + 无缓存 | ❌ | ❌ | 否 | 无数据 | `✗ Server not running, no cache data` |
| 本地 + Server 未运行 + 无缓存 | ❌ | ❌ | 是 | CLI 探测 → 失败 → 无数据 | `✗ Local probe failed, no cache to fallback` |
| 远程 + Server 运行 | ✅ | - | 否 | 远程 Server 实时 | `ℹ Data from remote Server (real-time)` |
| 远程 + Server 运行 | ✅ | - | 是 | 远程 Server 探测 | `✓ Requested remote Server to probe` |
| 远程 + Server 不可达 + 有缓存 | ❌ | ✅ | 否 | 读缓存 | `⚠ Data from local cache (remote Server unreachable)` |
| 远程 + Server 不可达 + 有缓存 | ❌ | ✅ | 是 | **尝试探测（失败）→ 回退缓存** | `⚠ Remote Server unreachable, probe failed, fell back to cache data` |
| 远程 + Server 不可达 + 无缓存 | ❌ | ❌ | 否 | 无数据 | `✗ Remote Server unreachable, no cache data` |
| 远程 + Server 不可达 + 无缓存 | ❌ | ❌ | 是 | 尝试探测（失败）→ 无数据 | `✗ Remote Server unreachable, probe failed, no cache to fallback` |

#### 核心逻辑

- **无 --probe**：优先实时（Server），然后缓存
- **有 --probe**：**忽略缓存**，先尝试探测 → 仅失败时回退缓存（若存在）

### 17.11 CLI 本地缓存更新时机

**核心原则**：缓存是“最后已知真实状态”的备份——当 Server 不可达时，缓存是唯一的降级数据源。

#### 更新时机

| 场景 | Server 状态 | 更新缓存？ | 说明 |
|----------|---------------|---------------|-------------|
| **本地 + Server 运行** | ✅ | **每次获取后更新** | 保持缓存最新，为 Server 不可达做备份 |
| **本地 + Server 未运行 + --probe** | ❌ | **CLI 探测成功后更新** | CLI 独立模式，探测结果写入缓存 |
| **本地 + Server 未运行 + 无 --probe** | ❌ | **不更新** | 只读缓存，不写入 |
| **远程 + Server 运行** | ✅ | **每次获取后更新** | 保持缓存最新，为远程 Server 不可达做备份 |
| **远程 + Server 不可达** | ❌ | **不更新** | 拿不到新数据，缓存保持原样 |

#### 核心逻辑

```text
if Server 可达:
    从 Server 获取实时状态
    更新本地缓存 (status_cache.json)  ← 每次都更新
    展示实时数据
else:
    if --probe:
        尝试 CLI 独立探测
        if 探测成功:
            更新本地缓存
            展示探测结果
        else:
            回退缓存（不更新）
            展示缓存数据 + 警告
    else:
        读缓存（不更新）
        展示缓存数据 + 警告
```

### 17.12 Status 配置设计

**核心原则**：提供合理默认值，同时支持灵活配置以适应不同场景。

#### 可配置参数

| 参数 | 含义 | 默认 | 说明 |
|-----------|---------|---------|-------------|
| `probe_timeout` | 探测超时（秒） | 30 | 向上游发送请求的超时（连接超时 10s + 读超时 30s） |
| `active_ttl` | 活动证据 TTL（秒） | 30 | “近期正常使用”的时间窗口 |
| `probe_concurrency` | 最大探测并发 | 4 | 跨绑定的最大并发探测执行数（约束突发频率） |
| `probe_interval_ms` | 基础探测节奏间隔（ms） | 150 | 计划探测请求之间的基础延迟 |
| `probe_interval_jitter_ms` | 探测节奏抖动（ms） | 100 | 加在基础间隔上的动态随机抖动（平滑机器节奏） |

- **User-Agent 与协议头一致性**：探测必须与正常请求转发严格对齐。探测请求绝不能注入自我标识标记（如 `llm-proxy` 或 `connectivity-probe`）。探测按协议使用标准官方客户端 User-Agent（Anthropic 用 `anthropic-typescript/0.39.0`，OpenAI 用 `OpenAI/Python 1.65.0`）。

#### 两层配置：全局 + Provider 级覆盖

**配置文件结构**：

```toml
# 全局默认（必填，init 时写入）
[status]
probe_timeout = 30
active_ttl = 30

# Provider 级覆盖（可选，手动添加）
[providers.openai.status]
probe_timeout = 15  # 仅在需要特殊配置时添加

[providers.slow-provider.status]
probe_timeout = 60
active_ttl = 60
```

#### 优先级

```text
Provider 级配置（若存在） > 全局配置 > 默认值
```

#### init 命令行为

**初始化配置文件时**：
- 写入 `[status]` 节（全局默认）
- **不询问** Provider 级 status 配置（默认可选）
- 用户可稍后手动添加 `[providers.xxx.status]` 节

#### 添加 Provider 行为

**CLI/TUI 添加 provider 时**：
- **不询问** status 配置（默认可选）
- 只写 provider 基本配置（api_key、端点等）
- 用户可稍后手动添加 `[providers.xxx.status]` 节

## 18. 总结（Summary）

### 核心设计决策

| 决策 | 选择 | 理由 |
|------|------|------|
| 远程模式 fallback | 无 fallback，报错 | 远程文件不可达 |
| 本地 Server 挂了 | 退化为独立模式 | 本地文件可达 |
| 运行期间模式切换 | 不允许 | 避免状态同步复杂性 |
| 敏感数据存储 | Server 机器 | 谁发请求谁持有凭据 |

### 一句话总结

**有 Server 就委托，没有就自己干（加锁）。远程连不上就报错，本地 Server 挂了就退化。**
