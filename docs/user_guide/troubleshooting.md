# 错误排查指南

本文档涵盖 llm-proxy 常见错误及解决方案。

---

## 目录

- [1. OAuth Token 过期](#1-oauth-token-过期)
- [2. 上游 Provider 返回 429/500 错误](#2-上游-provider-返回-429500-错误)
- [3. 模型协议不支持错误](#3-模型协议不支持错误)
- [4. 网络连接失败](#4-网络连接失败)
- [5. 服务启动失败](#5-服务启动失败)
- [6. 服务意外停止或请求链路异常](#6-服务意外停止或请求链路异常)
- [7. 上游生成失败与畸形工具调用](#7-上游生成失败与畸形工具调用)

---

## 1. OAuth Token 过期

### 症状

- `llm-proxy status` 显示 provider 状态为 `WARN`，auth 信息包含 `state=expired`
- 请求返回 `401 Unauthorized` 或 `403 Forbidden`
- 日志中出现 `token expired` 相关信息

> **多数情形不需要手动刷新（2026-09-23 更正）**：`state=expired` 只是 `oauth_accounts.json`
> 里 `expires_at` 的**静态读数**（`status/display.rs` 按 `is_expired()` 打印），不代表请求会
> 失败。转发请求取 token 时（`proxy::auth::resolve_oauth_auth`）发现过期会**自动刷新并更新
> 内存缓存**（context `proxy_expired_token`，并发由 `oauth_refresh_lock` 合并成单次刷新）
> ⇒ **下一次请求通常就自愈**，无需人工干预。
>
> 真正需要人工的只有两种：
> 1. **未登录** —— 报错 `OAuth account '...' not found; run: llm-proxy provider login <ID>`
>    （该分支不刷新）；
> 2. **刷新失败**（refresh_token 失效或被撤销）—— 此时要的是 **`provider login` 重新授权**，
>    `provider refresh` 解决不了。
>
> 下面的 401/403 症状若由其它原因产生（如权限范围变更、上游撤销授权），再按解决方案处理。

### 诊断

```bash
# 查看 provider 认证状态
llm-proxy status

# 查看特定 provider 详情
llm-proxy provider info <provider-name>
```

### 解决方案

**方法一：手动提前刷新（多数情形无需 —— 见上方「多数情形不需要手动刷新」；仅当确认属于未登录/刷新失败之外又想不发请求就刷新，或刷新本身失败需排查时）**

```bash
llm-proxy provider refresh <provider-name>
```

**方法二：重新登录**

```bash
# 先登出再登录
llm-proxy provider logout <provider-name>
llm-proxy provider login <provider-name>
```

**方法三：一键重新登录**

```bash
llm-proxy provider relogin <provider-name>
```

> **提示**：OAuth token 的有效期取决于上游 provider（如 OpenAI 通常为数小时）。
> 频繁过期时，检查系统时间是否准确：`date` 命令确认。

---

## 2. 上游 Provider 返回 429/500 错误，或本地 503 冷却

### 症状

- 请求返回 `429 Too Many Requests`（速率限制）
- 请求返回 `500/502/503`（上游故障，或本地持久型全冷却）
- 本地 `503` 带 `Retry-After`，正文含模型名、各 provider 剩余秒数和冷却类别/原因
- `llm-proxy status` / `cooldown list` 显示 cooldown 记录

瞬时型冷却（`network` / `server_error` / `rate_limit`）在全部候选都冷却时**仍会探测**上游。只有全部候选都是持久型冷却（`auth_error` / `model_unavailable` / `client_error`）时，请求才不接触上游、直接本地 `503`。

### 诊断

```bash
# 查看当前 cooldown 状态（含 kind / reason）
llm-proxy cooldown list

# 查看完整状态（含 cooldown 信息）
llm-proxy status
```

客户端拿到本地 `503` 时，先读正文里的类别：额度/鉴权问题去充值或重新登录；`Retry-After` 是最早一条冷却的剩余秒数。

### 解决方案

**429 速率限制 / Token 冷却：**

1. **上游 Provider 速率限制**：
   - **等待冷却结束**：该 provider 在限制窗口内降优先级；若还有健康候选会 fallback
   - **检查用量**：对于 OpenAI OAuth provider，使用 `llm-proxy quota` 查看订阅配额
   - **配置多个 provider**：添加同类型的备用 provider，llm-proxy 会自动 fallback

   ```bash
   # 添加备用 provider
   llm-proxy provider copy <original-name> <backup-name> --api-key-env <BACKUP_KEY>
   ```

2. **本地 Token 错误类别冷却（429 `token blocked after repeated upstream errors`）**：
   - **成因**：客户端使用同一 token 在短时间内累计触发同类上游 4xx 错误（默认 10 分钟内达到 3 次同类错误，如工具调用 schema 不合法、格式缺失等），触发了保护机制，该 token 进入 300 秒冷却期并在入口处被 429 阻断，以保护上游账号不被死循环流量封禁。其他未受影响的 token 正常服务。
   - **排查与解决**：
     - 检查客户端调用日志，修复引发 4xx 的请求内容（例如修正 tool 定义或参数格式）；
     - 等待冷却期（默认 300 秒）过期后自动恢复；
     - 若需调整阈值，可在 `~/.config/llm-proxy/config.toml` 中配置 `[protection.error_cooldown]`：
       ```toml
       [protection.error_cooldown]
       enabled = true
       max_errors_per_class = 3     # 允许同类错误最大次数
       cooldown_seconds = 300       # 冷却时长（秒）
       window_seconds = 600         # 滑动窗口时长（秒）
       ```

**500/502/503/529 上游错误：**

1. 同一 provider 会先按 `fallback.server_error_retries` 重试，耗尽后再写 `server_error` 冷却并尝试下一个候选
2. **检查上游服务状态**：访问 provider 的状态页面确认是否为全局故障
3. **确认已恢复后再清冷却**：不要把瞬时故障的自动冷却当成必须立刻清掉

```bash
llm-proxy cooldown clear --provider <provider-name>
```

**401 / 402 / 403：**

- `401`：fallback 到下一个候选，**不**写冷却；先 `provider refresh` / `relogin`
- `403`：本轮只 fallback，不写冷却（状态码不足以区分额度、region、权限）
- `402`：写 `auth_error` 冷却并 fallback

**已知配额要到次日才恢复：**

自动冷却通常只有几分钟。若某个 `(provider, model)` 组合确定不可用更久，可手动注入更长条目，避免周期性重试浪费配额：

```bash
llm-proxy cooldown set --provider <provider-name> --model <model-id> --duration 42h --reason "quota exhausted"
```

手动注入的默认 `kind=manual`，按持久型处理：该模型全部候选都是这类冷却时，请求直接本地 `503`。

---

## 3. 模型协议不支持错误

### 症状

错误消息类似：

```
model "xxx" does not support chat_completions requests; supported protocols: anthropic
```

或

```
model "xxx" does not declare the image_input capability required by this request
```

### 原因

客户端发送的请求协议（如 `chat_completions`、`anthropic`、`openai_responses`）与模型配置的 supported protocols 不匹配。

### 诊断

```bash
# 查看模型支持的协议
llm-proxy model info <model-name>

# 查看 provider 支持的协议
llm-proxy provider info <provider-name>
```

### 解决方案

**方法一：修改客户端配置**

确保客户端使用模型支持的协议。例如：
- Codex CLI 使用 `openai_responses` 协议
- Claude Code 使用 `anthropic` 协议
- 通用 OpenAI 兼容客户端使用 `openai_chat` 协议

**方法二：添加协议绑定**

为模型添加所需的协议绑定：

```bash
# 为模型添加 chat_completions 协议支持
llm-proxy model provider add <model-name> \
  --type openai-chat \
  --provider <provider-name> \
  --upstream-model <upstream-model>
```

**方法三：检查模型 features 声明**

如果错误提到缺少 capability（如 `image_input`），确认模型配置中声明了相应的 feature：

```bash
llm-proxy model set <model-name> --enable-feature image_input
```

---

## 4. 网络连接失败

### 症状

- 请求超时或返回 `Connection refused`
- `llm-proxy status` 显示 provider 探测失败
- 日志中出现 `connection error` 或 `DNS resolution failed`

### 诊断

```bash
# 检查服务是否运行
llm-proxy status

# 测试端口是否可达
curl -s http://127.0.0.1:8989/admin/ping

# 检查 provider 端点 URL 是否可达
curl -s <provider-url>/v1/models
```

### 解决方案

**代理配置问题：**

llm-proxy 通过网络环境变量配置代理，支持标准的 `HTTPS_PROXY` / `HTTP_PROXY`：

```bash
# 设置代理（在启动 llm-proxy 前设置）
export HTTPS_PROXY=http://127.0.0.1:7890
export HTTP_PROXY=http://127.0.0.1:7890

# 如果需要绕过某些地址
export NO_PROXY=localhost,127.0.0.1

# 重新启动服务
llm-proxy serve
```

**DNS 解析失败：**

1. 检查 `/etc/resolv.conf` 配置
2. 尝试使用 IP 地址替代域名配置 provider endpoint
3. 确认代理设置正确（代理可能影响 DNS 解析）

**端口被占用：**

```bash
# 查看端口占用
lsof -i :8989
# 或
ss -tlnp | grep 8989
```

---

## 5. 服务启动失败

### 症状

- `llm-proxy serve` 报错退出
- 日志中出现 `Address already in use` 或 `Permission denied`

### 诊断

```bash
# 检查是否已有实例运行
llm-proxy status

# 查看端口占用（记下 PID）
ss -tlnp | grep 8989
# 或
lsof -i :8989
```

> **提示**：`llm-proxy serve` 启动遇到端口占用时会自动解析 `/proc` 并在报错中直接提供占用者 PID、进程名及清理建议；`llm-proxy status` 在当前服务 not running 且端口被占时亦会自动输出 `WARN` 徽标与占用 PID，无需手动执行上述命令即可定位。

若 `status` 显示 **not running**，但仍提示某个 PID 在监听：占着端口的**不是**你当前这套配置在管的实例，而是残留的其他 llm-proxy 进程（常见于测试、实验环境没清干净）。`llm-proxy shutdown` 只能关闭它认识的那个实例，对这种残留进程无效——只能按下面「情形 B」手动结束它。

### 解决方案

**端口占用（Address already in use）：**

```bash
# 情形 A：status 显示 running —— 是当前实例占着端口
llm-proxy shutdown

# 情形 B：status 显示 not running，端口却有 PID —— 残留的其他进程
kill <PID>        # 先普通终止；几秒后仍在则 kill -9 <PID>

# 使用其他端口启动（修改 config.toml）
# [server]
# listen = "127.0.0.1:9879"
```

**配置文件错误：**

```bash
# 验证配置文件格式
llm-proxy status  # 会报告配置加载错误

# 检查配置文件路径
ls -la ~/.config/llm-proxy/config.toml
```

**权限问题：**

```bash
# 配置目录需可写
chmod -R u+w ~/.config/llm-proxy/

# 状态目录（日志、tokens.db、cooldowns.json、replay 记录）需可写
chmod -R u+w ~/.local/state/llm-proxy/

# 管理 socket 目录 = $XDG_RUNTIME_DIR/llm-proxy
# （未设 XDG_RUNTIME_DIR 时为 ~/.local/state/llm-proxy/runtime/llm-proxy）
ls -ld "${XDG_RUNTIME_DIR:-$HOME/.local/state/llm-proxy/runtime}/llm-proxy"
```

> 两条不要做：① 不要对 socket 文件本身 `chmod` —— 它的 `0600` 由服务在绑定后强制并向内核回读校验，设不上就直接启动失败（设计如此，不是可用权限位问题）；② 不要 `chmod -R ~/.local/share/llm-proxy/` —— 本项目的状态与运行文件不在 `share` 下，而在 `~/.local/state/llm-proxy/` 与运行时目录。

**管理通道不可用：`XDG_RUNTIME_DIR` 路径过长**

AF_UNIX socket 路径上限为 **107 字节**（`sockaddr_un::sun_path` 是 108 字节定长数组，含结尾 NUL）。常规名 `$XDG_RUNTIME_DIR/llm-proxy/llm-proxy.sock` 超界时，服务**自动改用哈希短名**并正常启动：

```
/tmp/llm-proxy-<uid>/<runtime_dir 的 sha256 前 16 位>.sock
```

这不是静默兜底：`llm-proxy serve`（后台）与 `llm-proxy serve --foreground`（前台）都打印 `socket: <实际生效路径>`，`llm-proxy status` 的 `socket=` 行亦然。日志里另有 `WARN … event="management_socket_fallback"`，但默认日志级别是 ERROR，看不到 warn 级事件（见 §6）——生效路径以命令输出为准。

```bash
# 实际生效的 socket 路径
llm-proxy status | grep 'socket='

# 规避事件（默认日志级别是 ERROR，看不到 warn 级事件，见 §6）
RUST_LOG=info llm-proxy serve --foreground   # Ctrl-C 停止，或另开终端 llm-proxy shutdown
```

若日志出现 `event="management_socket_fallback_unusable"`（随后 `event="service_start_failed" stage="management_channel"`），说明 `/tmp/llm-proxy-<uid>` 这个位置不能安全使用，共三种成因，事件文案会写明是哪一种：它是**软链接**（服务不跟随，避免把权限改掉链接目标上）、它属**其他 uid**（可能被他人抢先创建）、或它被**非目录**的东西占了名字。服务拒绝启动而不是换目录继续 —— 这条边界是提权面，不会自动修复，也不会替你删任何东西。处理：

```bash
ls -ld /tmp/llm-proxy-"$(id -u)"    # 只可能是 owner=<你自己> 且 mode=700
# 由该目录的属主删除，或避开超界：把 XDG_RUNTIME_DIR 指到短路径
XDG_RUNTIME_DIR=/run/user/"$(id -u)" llm-proxy serve
```

**报"已被活监听者占用、拒绝接管"**

```
management socket /run/user/1000/llm-proxy/llm-proxy.sock is already served by a live listener; 另一个 server 正在该通道上服务，拒绝接管：先执行 `llm-proxy shutdown`
```

这是防止两个写者同时改配置的闸门（日志事件 `management_socket_already_served`），不是残留文件损坏。**不要手工 `rm` 那个 socket**。按提示先 `llm-proxy shutdown`；若 `llm-proxy status` 显示未运行却仍报此错，说明存在另一个用了不同 `XDG_RUNTIME_DIR`、却共享同一份 config 的实例：

```bash
llm-proxy status
pgrep -fa 'llm-proxy.*serve'
```

找出并停掉多余实例后重启。

**服务无法正常关闭：**

```bash
# 始终使用 llm-proxy 自带的 shutdown 命令
llm-proxy shutdown

# 如果需要按新配置重新启动
llm-proxy restart
```

> **重要**：不要直接 `kill` llm-proxy 进程或手动修改 PID/socket 文件；这些操作会绕过服务的资源清理与退出原因日志。

---

## 6. 服务意外停止或请求链路异常

后台服务的日志位于状态目录的 `llm-proxy.log`（通常为 `~/.local/state/llm-proxy/llm-proxy.log`）。启动后台服务前设置 `RUST_LOG=info`，才能写入结构化 `info` 级别事件。

```bash
# 查看最近的生命周期和请求链路事件
RUST_LOG=info llm-proxy restart
tail -n 200 ~/.local/state/llm-proxy/llm-proxy.log
```

### TUI OAuth 诊断日志

`llm-proxy connect` 和 `llm-proxy provider` 的 TUI 日志写入状态目录下的 `llm-proxy-cli.log`（通常为 `~/.local/state/llm-proxy/llm-proxy-cli.log`），与后台服务的 `llm-proxy.log` 分开。OAuth 流程会记录 token exchange、userinfo、project discovery、凭证持久化以及 TUI 任务启动、超时和最终失败事件。

```bash
# 在另一个 SSH 终端实时查看 TUI OAuth 日志
RUST_LOG=info tail -f ~/.local/state/llm-proxy/llm-proxy-cli.log
```

日志中的 `oauth_tui_login_failure` 会包含 provider、脱敏账号标签、generation ID 和脱敏错误链；`account_present=false` 可用于识别 TUI 是否未正确初始化账号。日志不会记录授权码、access token、refresh token、Authorization header 或请求体。

### 服务生命周期

按时间查找以下事件：

- `service_start`：确认 PID、版本/commit、监听地址和配置路径；
- `shutdown_received`：`source=management_uds` 代表 `llm-proxy shutdown` 或 `restart` 的正常关停；`source=SIGINT` / `SIGTERM` 代表收到外部信号；`source=management-channel-failed` **不是用户要求的关停** —— 是管理通道自身撑不住了（见下条）；
- `service_stop`：记录对应的退出原因与 `uptime_ms`，并带 `config_writer_stop=ok|failed_stop|undetermined` 标签，表示配置写入器是否给出了干净的关闭终态；
- `management_accept_failed`：管理通道接受连接连续失败（`consecutive_errors=1..8`，`error=` 带成因，常见为 `Too many open files`）。前 7 次只记录并继续服务（运行期允许勉强继续，但必须留痕）；第 8 次触发升级关停；
- `management_channel_degraded_stop`：上一条的终态 —— 进程是因为**子系统死了**才停的，不是按要求停机。此时 `serve --foreground` 以**非 0** 退出码收尾（正常 `shutdown` 仍是 0），systemd 的 `Restart=on-failure` 会据此重启；包装脚本可用退出码区分这两种关停。终端同时打印 `Error: 管理通道在连续 accept 失败后升级停机…`；
- `management_socket_fallback`：`XDG_RUNTIME_DIR` 太深使常规 socket 名超出 107 字节上限，已改用哈希短名（字段 `runtime_dir` / `primary` / `path`）。这不是故障，但生效路径与配置路径不同，排障时以该事件或命令输出的 `socket=` 为准（见 §5）；
- `management_socket_fallback_unusable` / `service_start_failed`：短名基目录不可安全使用（是软链接、属其他 uid、或被非目录的东西占名）。这是**启动硬失败**：数据面从未发布、进程非 0 退出、不自动换位置。处置见 §5；
- `management_socket_bind_failed` / `management_socket_already_served`：管理通道绑定失败（唯一写路径不可用 ⇒ 启动失败）或该名字上已有活监听者在服务（拒绝接管，防两个写者）。后者按提示先 `llm-proxy shutdown`，**不要手工 `rm` socket**；
- `pid_file_revoke_skipped`：启动失败的进程发现自己那份 pid 文件里的数字不是它的 pid，因此**没有删除**它（那是别人的现场）。出现在同时有多个实例试图启动的场景，配合 `llm-proxy status` 与 `pgrep -fa 'llm-proxy.*serve'` 排查；
- `event="stop_undetermined"`：关闭结论在预算内取不到（等待超时，或 writer 任务在结论产出后仍未退出）。这是一次**失败**，不代表关闭成功：`serve --foreground` 与库入口此时以非 0 退出码收尾。后台 server 的 `llm-proxy shutdown` / `restart` 是发出请求即返回的委托调用，其自身退出码不反映该结论，只能靠本事件与 `config_writer_stop=undetermined` 判断。看到该事件时按"最后一次配置写入是否落盘未知"处理：先核对配置与 `tokens.toml` 的实际内容，再决定是否重试写入，不得凭 `exit 0` 假定已干净收尾；
- `event="panic"`：记录 panic 的线程、源码位置，以及**脱敏后的 payload 摘要**（先按凭据形态替换、再折叠控制字符、最后按字符截断到 300 并补省略号）。摘要是"为什么炸"的唯一来源，例如 `` `async fn` resumed after completion `` 这类常量消息会原样保留；非字符串的自定义 panic 只记占位标记，不会转成 `Debug` 输出。

需要完整调用栈时，用 `LLM_PROXY_PANIC_DEBUG=1` 起一次可复现的实例，会额外打一条
`event="panic_backtrace"`。注意 backtrace 含本机路径与符号名，**不属于脱敏承诺的范围**：
只用于本地排障，取到日志后不要直接上传、贴到 issue 或提交进仓库。

若只看到 `service_start` 而没有后续 `shutdown_received` / `service_stop`，保留该时间点附近的系统日志和服务日志；这表明进程未走应用层的优雅退出路径。

### 请求与 OAuth 转发

每个 HTTP 响应含 `x-llm-proxy-request-id`。用该 ID 在日志中关联 `proxy_request`、`oauth_refresh_*` 和 `generation_attempt_*` / `generation_fallback` 事件。OAuth refresh 事件带 `refresh_id`、provider、auth kind、脱敏账号标签、触发来源、阶段和耗时；失败时额外带安全的 HTTP status、OAuth error code、错误类型、可重试性与受限摘要。日志不会包含 token、Authorization、prompt、模型输出、请求体或完整上游响应。

---

## 7. 上游生成失败与畸形工具调用

### 症状与错误码

- **非流式响应**：返回 `502 Bad Gateway`，响应头携带 `x-llm-proxy-upstream-generation-failed: true`，响应体中包含错误码 `code: "upstream_generation_failed"`。
  - **OpenAI Chat 格式**：`{"error": {"message": "upstream generation failed (<reason>); request_id: <request_id>; …", "type": "server_error", "code": "upstream_generation_failed"}}`
  - **OpenAI Responses 格式**：`{"error": {"message": "upstream generation failed (<reason>); request_id: <request_id>; …", "code": "upstream_generation_failed"}}`
  - **Anthropic Messages 格式**：`{"type": "error", "error": {"type": "api_error", "message": "upstream generation failed (<reason>); request_id: <request_id>; …", "code": "upstream_generation_failed"}}`
- **流式响应**：流式连接在生成过程中中断，向下游抛出显式协议级 error 事件（Chat 输出带有 `error` 对象的 chunk，Responses 输出 `event: response.failed`，Anthropic 输出 `event: error`），并且**绝不**以常规的 `[DONE]` 或 `message_stop` 收尾。同时在 OpenAI Responses 协议中，若有未完整生成的 output item，会明确标记 `status: "incomplete"`。流式 error 事件的 `message` 同样携带 `request_id`（与非流式响应体一致），可对照响应头 `x-llm-proxy-request-id` 在日志中定位。
- **后台日志**：`llm_proxy::upstream_finish` 输出告警 `upstream non-clean finish detected`，记录受限脱敏的 `finish_reason`、哈希摘要 `finish_message`（`len=...,sha256=...`）、分类 `classification` 以及出口路径。

### 什么是 upstream_generation_failed

`upstream_generation_failed` 表示上游模型在推理生成阶段遇到了语义级不可恢复故障或返回了非正常终态，导致模型无法产出合法的完成响应。

在部分上游体系（如 Google Gemini 风格的 finishReason 体系）中，这对应于候选结果中携带的明确失败终态：
- `MALFORMED_FUNCTION_CALL`：模型生成的工具调用入参不符合 JSON 语法或破坏了定义的参数结构；
- `UNEXPECTED_TOOL_CALL`：模型输出了非预期的函数调用结构；
- `TOO_MANY_TOOL_CALLS`：模型单次生成的工具调用数量超出了合法上限；
- `IMAGE_OTHER` / `NO_IMAGE` 或其他未识别的异常 finishReason：安全按未知失败（fail-closed）处理。

### 为何故意 Bypass 故障转移与冷却池

遇到此类错误时，llm-proxy **故意不触发候选池重试与故障转移（Fallback），也不将该 provider 关进冷却惩罚（Cooldown），更不记录客户端指纹拉黑**。

**设计原因**：
1. **语义错误非节点可用性故障**：此错误源于用户输入的提示词（prompt）、工具定义（schema）或模型内在生成倾向，并非上游 Provider 网络不可达、进程宕机或 429 限流。
2. **避免重复扣费与雪崩**：若将此类错误误判为可用性故障并自动切换到候选池其他节点重试，其他模型大概率同样会生成畸形结构或再次触发异常，造成重复扣费与延迟放大。
3. **防止健康节点被误封**：如果因单个 prompt 的特定生成异常而将健康 Provider 关入冷却惩罚，会导致后续完全正常的请求无可用节点。

因此，代理层实行 **Verdict Bypass** 原则：宁可显式交付失败，不谎报成功，立即中断候选循环并透传标准错误，将处置权交还客户端。

### 客户端适配与排查指引

1. **识别错误与阻止盲目重试**：
   客户端遇到 `code: "upstream_generation_failed"` 时，切忌无间隔原地重试相同请求。这通常不是偶发网络抖动，盲目重试几乎注定重复失败。
2. **检查与简化 Tool / Parameter Schema**：
   - 检查 `tools` 中定义的 JSON Schema，确保属性类型清晰，避免过深的对象嵌套、过于复杂的 `anyOf` / `oneOf` 联合类型。
   - 确保每个参数都有清晰的 `description`，帮助模型准确理解入参约束。
3. **优化 System Prompt**：
   - 如果使用了工具调用，在系统提示词中强调返回合规的 JSON 参数，或增加少量合规的工具调用样例（few-shot examples）。
4. **流式客户端检查中断处理**：
   - 确保客户端能正确处理 SSE 连接中的显式 `error` / `response.failed` 事件，切勿假设所有流式响应都会以 `[DONE]` 正常结束。
5. **运维与指标核对**：
   - 在指标监控中，`upstream_generation_failed` 事件统一归集至 `upstream_error` 计数器，与可用性指标判定一致；
   - 审计日志中已对原始 `finish_message` 进行哈希脱敏，排查时可通过 `request_id` 关联日志中的 `elapsed_ms` 和已交付事件数。

---

## 通用排查步骤

当遇到未列出的问题时，按以下步骤排查：

1. **检查服务状态**：`llm-proxy status`
2. **查看日志**：检查 llm-proxy 的日志输出（前台运行时会直接输出到终端）
3. **验证配置**：`llm-proxy provider list` 确认 provider 配置正确
4. **测试连通性**：`curl http://127.0.0.1:8989/admin/ping`
5. **查看版本**：`llm-proxy version` 确认运行的是最新版本
