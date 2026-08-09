# OAuth 账号存储用户指南

本文档说明 llm-proxy v2 的 OAuth 账号存储格式和使用方法。

## 概述

llm-proxy v2 使用新的 OAuth 账号存储格式，按 OAuth 类型分组存储，提供更清晰的组织结构、更强的类型安全性和更好的并发安全性。

**新格式文件位置：** `~/.config/llm-proxy/oauth_accounts.json`

## 新格式结构

```json
{
  "version": 1,
  "antigravity": {
    "default": {
      "account_label": "user@gmail.com",
      "project_id": "my-project-123",
      "access_token": "ya29.a0...",
      "refresh_token": "1//0e...",
      "expires_at_unix": 1785245247,
      "updated_at_unix": 1785226019
    }
  },
  "openai": {
    "personal": {
      "account_label": "user@gmail.com",
      "access_token": "eyJhbG...",
      "refresh_token": "rt.1.AA...",
      "expires_at_unix": 1786090020,
      "updated_at_unix": 1785226020
    }
  }
}
```

### 字段说明

**顶层字段：**
- `version`：Schema 版本号（当前为 1）
- `antigravity`：Antigravity OAuth 账号分组
- `openai`：OpenAI OAuth 账号分组

**Antigravity 账号字段：**
- `account_label`：账号标签（通常是邮箱）
- `project_id`：Google Cloud 项目 ID（必需）
- `access_token`：OAuth access token
- `refresh_token`：OAuth refresh token
- `expires_at_unix`：Token 过期时间（Unix 时间戳，秒）
- `updated_at_unix`：最后更新时间（Unix 时间戳，秒，可选）

**OpenAI 账号字段：**
- `account_label`：账号标签（通常是邮箱）
- `access_token`：OAuth access token
- `refresh_token`：OAuth refresh token
- `expires_at_unix`：Token 过期时间（Unix 时间戳，秒）
- `updated_at_unix`：最后更新时间（Unix 时间戳，秒，可选）

## 账号 ID 规范

账号 ID 必须满足以下要求：
- 只允许 ASCII 字母、数字、下划线、连字符
- 长度：1-64 字符
- 大小写敏感（`Default` 和 `default` 是不同账号）
- 在同一类型分组内必须唯一

**建议命名约定：**
- `default`：默认账号
- `personal`：个人账号
- `work`：工作账号
- `test`：测试账号

## 使用方法

### 1. 登录 OAuth Provider

```bash
# 登录 OpenAI
llm-proxy provider login openai-subscription

# 登录 Antigravity
llm-proxy provider login antigravity
```

登录流程会自动保存凭据到 `oauth_accounts.json`。

### 2. 查看登录状态

```bash
# 查看 provider 状态
llm-proxy provider list

# 查看 OAuth 状态
llm-proxy auth status
```

### 3. 在 TUI 中查看状态

运行 `llm-proxy provider` 进入 TUI 界面，每个 provider 旁边会显示登录状态：

- `✓ Ready`：已登录或 API Key 已配置
- `⚠ Not logged in`：OAuth provider 未登录
- `⚠ Login expired`：OAuth token 已过期
- `✗ Missing KEY_NAME`：API Key 环境变量未设置

### 4. 刷新 Token

Token 会在过期前自动刷新。如果需要手动刷新：

```bash
llm-proxy provider refresh openai-subscription
llm-proxy provider refresh antigravity
```

### 5. 登出

```bash
llm-proxy provider logout openai-subscription
llm-proxy provider logout antigravity
```

## 文件安全

### 权限

`oauth_accounts.json` 文件权限自动设置为 `0600`（仅所有者可读写），确保敏感信息不会泄露。

### 并发安全

文件使用排他锁（flock）防止并发写入，确保多进程环境下的数据一致性。

### 原子写入

所有写入操作都是原子的（先写入临时文件，然后重命名），防止写入中断导致文件损坏。

## 错误处理

### 文件不存在

如果 `oauth_accounts.json` 不存在，llm-proxy 会在启动时显示警告：

```
⚠️  Warning: OAuth accounts file not found
   Found 2 OAuth provider(s) that require login:
   - antigravity
   - openai-subscription
   
   Run: llm-proxy provider login <provider>
   Or use TUI: llm-proxy provider
```

### 账号不存在

如果使用未登录的 OAuth provider，会返回明确的错误：

```
Provider 'antigravity' requires OAuth login
Run: llm-proxy provider login antigravity
Or use TUI: llm-proxy provider
```

### Token 过期

如果 token 已过期，会自动尝试刷新。如果刷新失败，会提示重新登录：

```
OAuth account 'default' access token is expired
Run: llm-proxy provider refresh antigravity or relogin
```

## 验证规则

llm-proxy 会自动验证账号数据，确保：

1. **Account ID 格式**：只允许字母数字下划线连字符，长度 ≤64
2. **Token 长度**：access_token 和 refresh_token 长度 ≥20 字符
3. **Token 不相同**：access_token 和 refresh_token 不能相同
4. **时间戳合理性**：expires_at_unix 和 updated_at_unix 必须在合理范围内
5. **Project ID 格式**（Antigravity）：必须符合 Google Cloud 项目 ID 格式

如果验证失败，会显示详细的错误信息：

```
Invalid antigravity account ID: invalid id with spaces
Antigravity account test-account access_token too short
Antigravity account test-account has identical access and refresh tokens
Invalid Google Cloud project ID for account test: INVALID
```

## 迁移

### 从旧格式迁移

如果你之前使用 llm-proxy v1 或早期 v2 版本，需要手动迁移到 new format。

**步骤：**

1. 备份旧文件：
   ```bash
   cp ~/.config/llm-proxy/oauth_accounts.json ~/.config/llm-proxy/oauth_accounts.json.bak
   ```

2. 重新登录所有 OAuth provider：
   ```bash
   llm-proxy provider login openai-subscription
   llm-proxy provider login antigravity
   ```

3. 验证新格式：
   ```bash
   cat ~/.config/llm-proxy/oauth_accounts.json
   ```

### 手动编辑

如果需要手动编辑 `oauth_accounts.json`，请确保：

1. 保持 JSON 格式正确
2. 遵循字段规范
3. 遵守 Account ID 命名约定
4. 保存后运行 `llm-proxy auth status` 验证

## antigravity 模型上游稳定性说明

antigravity 提供的模型（gemini-* / claude-* / gpt-oss-*）中，**gpt-oss-120b** 存在**间歇性上游不稳定**（L1 实测，2026-08-02）：

- **表现**：连续/高频请求时偶发上游 5xx（`internal_server_error`），上游返回 `"We're currently experiencing high demand, which may cause temporary errors."`。在 codex TUI 中使用 `/goal` 长任务时表现为 `Goal blocked (/goal resume)`。
- **原因**：antigravity 服务端高负载/限流间歇触发，**非 llm-proxy 配置或转换问题**（代理日志无错误）。
- **处理**：
  1. 遇到 `Goal blocked` 时稍等片刻，用 `/goal resume` 恢复（codex 会重试）
  2. 若持续失败，间隔 1-2 分钟后再试（上游会自行恢复）
  3. 该模型单轮请求（普通对话/exec 单轮）通常正常；仅高频多轮任务易触发

其余 11 个模型（gemini-*、claude-sonnet-4-6、claude-opus-4-6）验证稳定，无此问题。

## 故障排除

### 问题：登录后仍然提示未登录

**解决方案：**
1. 检查文件权限：`ls -l ~/.config/llm-proxy/oauth_accounts.json`（应该是 `-rw-------`）
2. 检查文件内容：`cat ~/.config/llm-proxy/oauth_accounts.json`
3. 重新登录：`llm-proxy provider login <provider>`

### 问题：Token 频繁过期

**解决方案：**
1. 检查系统时间是否准确：`date`
2. 手动刷新 token：`llm-proxy provider refresh <provider>`
3. 如果问题持续，重新登录

### 问题：并发写入错误

**解决方案：**
1. 确保只有一个 llm-proxy 实例在运行
2. 检查是否有其他进程在访问文件：`lsof ~/.config/llm-proxy/oauth_accounts.json`
3. 删除锁文件：`rm ~/.config/llm-proxy/oauth_accounts.lock`

## 技术细节

### 文件锁

使用 `fs2` crate 的 `lock_exclusive()` 实现排他锁，防止并发写入。

### 原子写入

使用临时文件 + `rename` 实现原子写入，防止写入中断导致文件损坏。

### 权限设置

使用 `std::os::unix::fs::PermissionsExt` 设置文件权限为 `0o600`。

### 验证

使用 `regex` crate 验证 Account ID 和 Project ID 格式。

## 相关文档

- [OAuth 账号存储设计文档](../design/oauth-accounts-storage.md)
- [Provider 管理命令](../AGENTS.md#provider-命令)
- [TUI 用户界面](../design/rust-v2-tui-design.md)

## 反馈

如果遇到问题或有改进建议，请提交 issue 或联系开发团队。
