# 错误排查指南

本文档涵盖 llm-proxy 常见错误及解决方案。

---

## 目录

- [1. OAuth Token 过期](#1-oauth-token-过期)
- [2. 上游 Provider 返回 429/500 错误](#2-上游-provider-返回-429500-错误)
- [3. 模型协议不支持错误](#3-模型协议不支持错误)
- [4. 网络连接失败](#4-网络连接失败)
- [5. 服务启动失败](#5-服务启动失败)

---

## 1. OAuth Token 过期

### 症状

- `llm-proxy status` 显示 provider 状态为 `WARN`，auth 信息包含 `state=expired`
- 请求返回 `401 Unauthorized` 或 `403 Forbidden`
- 日志中出现 `token expired` 相关信息

### 诊断

```bash
# 查看 provider 认证状态
llm-proxy status

# 查看特定 provider 详情
llm-proxy provider --select <provider-name>
```

### 解决方案

**方法一：刷新 token（推荐）**

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

## 2. 上游 Provider 返回 429/500 错误

### 症状

- 请求返回 `429 Too Many Requests`（速率限制）
- 请求返回 `500/502/503 Internal Server Error`（上游服务故障）
- `llm-proxy status` 显示 cooldown 记录

### 诊断

```bash
# 查看当前 cooldown 状态
llm-proxy cooldown list

# 查看完整状态（含 cooldown 信息）
llm-proxy status
```

### 解决方案

**429 速率限制：**

1. **等待冷却结束**：llm-proxy 会自动管理 cooldown，在限制窗口内暂停使用该 provider
2. **检查用量**：对于 OpenAI OAuth provider，使用 `llm-proxy quota` 查看订阅配额
3. **配置多个 provider**：添加同类型的备用 provider，llm-proxy 会自动 fallback

```bash
# 添加备用 provider
llm-proxy provider copy <original-name> <backup-name> --api-key-env <BACKUP_KEY>
```

**500/502/503 上游错误：**

1. **检查上游服务状态**：访问 provider 的状态页面确认是否为全局故障
2. **清除 cooldown 重试**：如果确认上游已恢复，手动清除 cooldown

```bash
llm-proxy cooldown clear --provider <provider-name>
```

3. **检查日志**：查看 llm-proxy 日志获取详细错误信息

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
llm-proxy provider --select <provider-name>
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

# 查看端口占用
lsof -i :8989
```

### 解决方案

**端口占用（Address already in use）：**

```bash
# 方法一：关闭已有实例
llm-proxy shutdown

# 方法二：如果 shutdown 不工作，强制终止
# 先找到 PID
lsof -ti :8989 | xargs kill

# 方法三：使用其他端口启动（修改 config.toml）
# [server]
# port = 9879
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
# 确保配置目录可写
chmod -R u+w ~/.config/llm-proxy/

# 确保 socket 文件目录可写
chmod -R u+w ~/.local/share/llm-proxy/
```

**服务无法正常关闭：**

```bash
# 始终使用 llm-proxy 自带的 shutdown 命令
llm-proxy shutdown

# 如果 shutdown 无响应，使用 restart
llm-proxy restart

# 最后手段：手动清理 pid 文件
rm -f ~/.local/share/llm-proxy/server.pid
```

> **重要**：不要直接 `kill` llm-proxy 进程，使用 `llm-proxy shutdown` 确保正确清理资源。

---

## 通用排查步骤

当遇到未列出的问题时，按以下步骤排查：

1. **检查服务状态**：`llm-proxy status`
2. **查看日志**：检查 llm-proxy 的日志输出（前台运行时会直接输出到终端）
3. **验证配置**：`llm-proxy provider list` 确认 provider 配置正确
4. **测试连通性**：`curl http://127.0.0.1:8989/admin/ping`
5. **查看版本**：`llm-proxy version` 确认运行的是最新版本
