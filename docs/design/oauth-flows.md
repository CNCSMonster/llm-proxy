# OAuth 流程设计文档

## 概述

llm-proxy 支持两种 OAuth 2.0 认证流程，用于不同的 provider：

1. **OpenAI Device Code Flow** — 用于 ChatGPT Subscription (openai-sub)
2. **Antigravity Authorization Code Flow** — 用于 Google Antigravity (google-antigravity)

两种流程的核心区别在于**用户交互方式**和**适用场景**。

---

## 1. OpenAI Device Code Flow

### 适用场景

- **远程服务器场景**：CLI 运行在 SSH 服务器上，用户在本地浏览器完成认证
- **无浏览器环境**：CLI 运行在没有浏览器的环境（如容器、服务器）
- **设备受限场景**：CLI 运行在智能电视、IoT 设备等受限环境

### 流程概述

```
┌─────────────────┐                    ┌─────────────────┐                    ┌─────────────────┐
│  CLI (Server)   │                    │  Auth Server    │                    │  User (Browser) │
│  llm-proxy      │                    │  (OpenAI)       │                    │  (Local)        │
└────────┬────────┘                    └────────┬────────┘                    └────────┬────────┘
         │                                      │                                      │
         │  1. POST /device/code                │                                      │
         │  (client_id)                         │                                      │
         │─────────────────────────────────────>│                                      │
         │                                      │                                      │
         │  2. Return:                          │                                      │
         │     - device_code                    │                                      │
         │     - user_code                      │                                      │
         │     - verification_uri               │                                      │
         │     - expires_in                     │                                      │
         │     - interval                       │                                      │
         │<─────────────────────────────────────│                                      │
         │                                      │                                      │
         │  3. Display to user:                 │                                      │
         │     "Visit: https://..."             │                                      │
         │     "Enter code: XXXX-XXXX"          │                                      │
         │                                      │                                      │
         │                                      │         4. User visits URI           │
         │                                      │<─────────────────────────────────────│
         │                                      │                                      │
         │                                      │         5. User enters user_code     │
         │                                      │<─────────────────────────────────────│
         │                                      │                                      │
         │                                      │         6. User authorizes           │
         │                                      │<─────────────────────────────────────│
         │                                      │                                      │
         │  7. POST /token (poll)               │                                      │
         │     - device_code                    │                                      │
         │     - user_code                      │                                      │
         │─────────────────────────────────────>│                                      │
         │                                      │                                      │
         │  8a. If not authorized:              │                                      │
         │      Return 400/403/404              │                                      │
         │      {"error": "authorization_       │                                      │
         │       pending"}                      │                                      │
         │<─────────────────────────────────────│                                      │
         │                                      │                                      │
         │  8b. If authorized:                  │                                      │
         │      Return 200                      │                                      │
         │      {"authorization_code": "...",   │                                      │
         │       "code_verifier": "..."}        │                                      │
         │<─────────────────────────────────────│                                      │
         │                                      │                                      │
         │  9. POST /oauth/token                │                                      │
         │     - grant_type=authorization_code  │                                      │
         │     - code=authorization_code        │                                      │
         │     - code_verifier                  │                                      │
         │     - client_id                      │                                      │
         │─────────────────────────────────────>│                                      │
         │                                      │                                      │
         │  10. Return:                         │                                      │
         │      - access_token                  │                                      │
         │      - refresh_token                 │                                      │
         │      - expires_in                    │                                      │
         │<─────────────────────────────────────│                                      │
         │                                      │                                      │
         │  11. Store tokens                    │                                      │
         │                                      │                                      │
```

### 关键实现细节

**文件位置**：`src/auth/login.rs`

#### 1. 请求 Device Code

```rust
pub(super) async fn request_openai_device_code(url: &str) -> Result<DeviceCode> {
    let payload: Value = reqwest::Client::new()
        .post(url)
        .json(&serde_json::json!({ "client_id": OPENAI_CLIENT_ID }))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    // 解析 device_auth_id, user_code, expires_in, interval
}
```

**关键点**：
- 使用 `error_for_status()` 接受所有 2xx 状态码（200-299）
- 支持字段名别名（`device_auth_id` / `deviceAuthID`，`user_code` / `usercode`）
- 默认 `expires_in=900`（15 分钟），`interval=5`（秒）

#### 2. 轮询 Token 端点

```rust
pub(super) async fn poll_openai_device_token(
    url: &str,
    device_auth_id: &str,
    user_code: &str,
) -> Result<Option<DevicePoll>> {
    let response = reqwest::Client::new()
        .post(url)
        .json(&serde_json::json!({
            "device_auth_id": device_auth_id,
            "user_code": user_code,
        }))
        .send()
        .await?;
    
    // 403/404 = 用户尚未完成授权，继续轮询
    if response.status() == reqwest::StatusCode::FORBIDDEN
        || response.status() == reqwest::StatusCode::NOT_FOUND
    {
        return Ok(None);
    }
    
    // 其他非 2xx = 错误
    let payload: Value = response.error_for_status()?.json().await?;
    // 解析 authorization_code, code_verifier
}
```

**关键点**：
- **403/404** = 用户尚未完成授权，返回 `Ok(None)`，CLI 继续轮询
- **其他非 2xx** = 错误，`error_for_status()` 会返回错误
- **2xx（包括 200-299）** = 成功，解析 authorization_code 和 code_verifier
- **轮询间隔**：`max(server_interval, 2) + 3` 秒（安全边距）
- **超时**：`expires_in` 秒（默认 15 分钟）

#### 3. 交换 Access Token

```rust
async fn exchange_openai_device_token(
    url: &str,
    authorization_code: &str,
    code_verifier: &str,
) -> Result<RefreshedToken> {
    let params = [
        ("grant_type", "authorization_code"),
        ("code", authorization_code),
        ("code_verifier", code_verifier),
        ("redirect_uri", "https://auth.openai.com/deviceauth/callback"),
        ("client_id", OPENAI_CLIENT_ID),
    ];
    let payload = post_refresh_form(url, &params).await?;
    let token = refreshed_token_from_json(payload)?;
    // 验证 refresh_token 存在
}
```

**关键点**：
- 使用 PKCE（Proof Key for Code Exchange）增强安全性
- `redirect_uri` 是固定的回调地址（OpenAI 设备认证回调）
- 必须验证 `refresh_token` 存在

### 用户交互流程

```bash
$ llm-proxy provider login openai-sub

OpenAI login device code: ABCD-1234
Open: https://auth.openai.com/codex/device

# 用户在本地浏览器打开 https://auth.openai.com/codex/device
# 输入 device code: ABCD-1234
# 完成授权

# CLI 自动检测到授权完成，交换 token
logged in OAuth account=openai-sub provider=openai-sub
```

### 错误处理

| 场景 | 处理 |
|------|------|
| Device code 请求失败 | 立即报错，提示用户重试 |
| 轮询超时（15 分钟） | 报错 "device login expired; run provider login again" |
| 轮询遇到非 403/404 错误 | 立即报错（如 500、网络错误） |
| Token 交换失败 | 报错，提示用户重试 |
| 缺少 refresh_token | 报错 "OAuth server did not return a refresh token" |

---

## 2. Antigravity Authorization Code Flow

### 适用场景

- **本地开发环境**：CLI 和浏览器在同一台机器上
- **有浏览器环境**：CLI 运行在有浏览器的环境
- **标准 OAuth 2.0 场景**：使用 PKCE 的标准授权码流程

### 流程概述

```
┌─────────────────┐                    ┌─────────────────┐                    ┌─────────────────┐
│  CLI (Local)    │                    │  Auth Server    │                    │  User (Browser) │
│  llm-proxy      │                    │  (Google)       │                    │  (Same Machine) │
└────────┬────────┘                    └────────┬────────┘                    └────────┬────────┘
         │                                      │                                      │
         │  1. Generate PKCE                    │                                      │
         │     - code_verifier                  │                                      │
         │     - code_challenge (S256)          │                                      │
         │     - state                          │                                      │
         │                                      │                                      │
         │  2. Build auth URL                   │                                      │
         │     - client_id                      │                                      │
         │     - redirect_uri                   │                                      │
         │     - response_type=code             │                                      │
         │     - code_challenge                 │                                      │
         │     - scope                          │                                      │
         │                                      │                                      │
         │  3. Display URL to user              │                                      │
         │     "Open this URL in browser..."    │                                      │
         │                                      │                                      │
         │                                      │         4. User opens URL            │
         │                                      │<─────────────────────────────────────│
         │                                      │                                      │
         │                                      │         5. User authenticates        │
         │                                      │<─────────────────────────────────────│
         │                                      │                                      │
         │                                      │         6. User authorizes           │
         │                                      │<─────────────────────────────────────│
         │                                      │                                      │
         │                                      │         7. Redirect to callback      │
         │                                      │         with authorization_code      │
         │                                      │─────────────────────────────────────>│
         │                                      │                                      │
         │  8. User copies authorization_code   │                                      │
         │     from browser URL                 │                                      │
         │<─────────────────────────────────────────────────────────────────────────────│
         │                                      │                                      │
         │  9. User pastes code to CLI          │                                      │
         │─────────────────────────────────────────────────────────────────────────────>│
         │                                      │                                      │
         │  10. POST /token                     │                                      │
         │      - grant_type=authorization_code │                                      │
         │      - code=authorization_code       │                                      │
         │      - code_verifier                 │                                      │
         │      - redirect_uri                  │                                      │
         │      - client_id                     │                                      │
         │      - client_secret                 │                                      │
         │─────────────────────────────────────>│                                      │
         │                                      │                                      │
         │  11. Return:                         │                                      │
         │      - access_token                  │                                      │
         │      - refresh_token                 │                                      │
         │      - expires_in                    │                                      │
         │<─────────────────────────────────────│                                      │
         │                                      │                                      │
         │  12. GET /userinfo                   │                                      │
         │      (get email)                     │                                      │
         │─────────────────────────────────────>│                                      │
         │                                      │                                      │
         │  13. Return email                    │                                      │
         │<─────────────────────────────────────│                                      │
         │                                      │                                      │
         │  14. POST /loadCodeAssist            │                                      │
         │      (get project_id)                │                                      │
         │─────────────────────────────────────>│                                      │
         │                                      │                                      │
         │  15. Return project_id               │                                      │
         │<─────────────────────────────────────│                                      │
         │                                      │                                      │
         │  16. Store tokens + email + project  │                                      │
         │                                      │                                      │
```

### 关键实现细节

**文件位置**：`src/auth/login.rs`

#### 1. 生成 PKCE 参数

```rust
pub(super) fn generate_pkce_code_verifier() -> Result<String> {
    let bytes = random_bytes(64)?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

pub(super) fn pkce_challenge(verifier: &str) -> String {
    let digest = sha2::Sha256::digest(verifier.as_bytes());
    URL_SAFE_NO_PAD.encode(digest)
}
```

**关键点**：
- `code_verifier`：64 字节随机数，base64url 编码
- `code_challenge`：SHA256(code_verifier)，base64url 编码
- 使用 `URL_SAFE_NO_PAD` 编码（无 padding）

#### 2. 构建授权 URL

```rust
pub(super) fn build_antigravity_auth_url(
    auth_url: &str,
    state: &str,
    code_challenge: &str,
) -> Result<String> {
    let mut url = url::Url::parse(auth_url)?;
    url.query_pairs_mut()
        .append_pair("client_id", ANTIGRAVITY_CLIENT_ID)
        .append_pair("redirect_uri", ANTIGRAVITY_REDIRECT_URI)
        .append_pair("response_type", "code")
        .append_pair("state", state)
        .append_pair("code_challenge", code_challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("access_type", "offline")
        .append_pair("prompt", "consent")
        .append_pair("scope", &antigravity_scopes().join(" "));
    Ok(url.to_string())
}
```

**关键点**：
- `access_type=offline`：请求 refresh_token
- `prompt=consent`：强制用户确认授权（确保返回 refresh_token）
- `code_challenge_method=S256`：使用 SHA256（最安全）

#### 3. 交换 Access Token

```rust
pub(super) async fn exchange_antigravity_code(
    token_url: &str,
    code: &str,
    code_verifier: &str,
) -> Result<RefreshedToken> {
    let params = [
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", ANTIGRAVITY_REDIRECT_URI),
        ("client_id", ANTIGRAVITY_CLIENT_ID),
        ("client_secret", ANTIGRAVITY_CLIENT_SECRET),
        ("code_verifier", code_verifier),
    ];
    let payload = post_refresh_form(token_url, &params).await?;
    let token = refreshed_token_from_json(payload)?;
    // 验证 refresh_token 存在
}
```

**关键点**：
- 使用 `client_secret`（与 OpenAI 不同，Antigravity 需要 client_secret）
- 必须验证 `refresh_token` 存在

#### 4. 获取用户信息和项目 ID

```rust
async fn fetch_antigravity_userinfo(userinfo_url: &str, access_token: &str) -> Result<String> {
    // GET /userinfo，返回 email
}

async fn fetch_antigravity_project_id(
    load_code_assist_url: &str,
    onboard_user_url: &str,
    access_token: &str,
) -> Result<String> {
    // POST /loadCodeAssist，返回 project_id
}
```

**关键点**：
- Antigravity 需要额外的 `project_id`（Google Cloud 项目 ID）
- 通过 `/loadCodeAssist` 端点获取

### 用户交互流程

```bash
$ llm-proxy provider login google-antigravity

Open this url in your browser and authorize Antigravity:
https://accounts.google.com/o/oauth2/v2/auth?client_id=...&redirect_uri=...&response_type=code&...

Paste authorization code: 
# 用户在浏览器完成授权后，从回调 URL 中复制 authorization_code
# 粘贴到 CLI

# CLI 交换 token，获取 email 和 project_id
logged in OAuth account=user@gmail.com provider=google-antigravity
```

### 错误处理

| 场景 | 处理 |
|------|------|
| 非交互式终端 | 报错 "requires an interactive terminal to paste the authorization code" |
| 用户输入空 code | 报错 "authorization code must not be empty" |
| Token 交换失败 | 报错，提示用户重试 |
| 缺少 refresh_token | 报错 "OAuth server did not return a refresh token" |
| 获取 project_id 失败 | 报错 "failed to determine project ID; relogin required" |

---

## 3. 两种流程对比

| 维度 | Device Code Flow | Authorization Code Flow |
|------|------------------|-------------------------|
| **适用场景** | 远程/无浏览器环境 | 本地/有浏览器环境 |
| **用户交互** | 在另一台设备的浏览器完成 | 在同一台设备的浏览器完成 |
| **授权码传递** | 自动（CLI 轮询） | 手动（用户复制粘贴） |
| **PKCE** | ✅ 使用 | ✅ 使用 |
| **Client Secret** | ❌ 不需要 | ✅ 需要 |
| **轮询** | ✅ 需要（轮询 token 端点） | ❌ 不需要 |
| **超时** | 15 分钟（expires_in） | 无（用户手动粘贴） |
| **额外信息** | 无 | email + project_id |
| **复杂度** | 中等（轮询逻辑） | 简单（一次性交换） |
| **安全性** | 高（PKCE + 短时效） | 高（PKCE + state） |

---

## 4. Token 存储

两种流程的 token 都存储在 `~/.config/llm-proxy/oauth_accounts.json`：

```json
{
  "version": 1,
  "openai": {
    "openai-sub": {
      "account_label": "openai-sub",
      "access_token": "...",
      "refresh_token": "...",
      "expires_at_unix": 1786090020,
      "updated_at_unix": 1785226020
    }
  },
  "antigravity": {
    "google-antigravity": {
      "account_label": "user@gmail.com",
      "project_id": "example-project-12345",
      "access_token": "...",
      "refresh_token": "...",
      "expires_at_unix": 1786090020,
      "updated_at_unix": 1785226020
    }
  }
}
```

**关键点**：
- `expires_at_unix`：access_token 过期时间（Unix 时间戳）
- `refresh_token`：用于刷新 access_token
- `project_id`：仅 Antigravity 需要（Google Cloud 项目 ID）

---

## 5. Token 刷新

Token 刷新逻辑在 `src/auth/refresh.rs` 中实现，两种流程使用相同的刷新机制：

```rust
pub async fn refresh_openai_token(...) -> Result<RefreshedToken> {
    // POST /oauth/token with grant_type=refresh_token
}

pub async fn refresh_antigravity_token(...) -> Result<RefreshedToken> {
    // POST /token with grant_type=refresh_token
}
```

**关键点**：
- 刷新时使用 `refresh_token` 获取新的 `access_token`
- 刷新后更新 `oauth_accounts.json`
- 如果 server 运行，通过 UDS 委托更新（server 内存 + 磁盘）

---

## 6. 安全考虑

### PKCE (Proof Key for Code Exchange)

两种流程都使用 PKCE 防止授权码拦截攻击：

1. CLI 生成 `code_verifier`（随机字符串）
2. CLI 计算 `code_challenge = SHA256(code_verifier)`
3. 授权请求携带 `code_challenge`
4. Token 交换时携带 `code_verifier`
5. 服务端验证 `SHA256(code_verifier) == code_challenge`

### State 参数（仅 Antigravity）

Antigravity 流程使用 `state` 参数防止 CSRF 攻击：

1. CLI 生成随机 `state`
2. 授权请求携带 `state`
3. 回调时验证 `state` 匹配

**注意**：当前实现中，用户手动粘贴 authorization_code，`state` 验证由 Google 服务端处理。

### Token 存储安全

- `oauth_accounts.json` 文件权限应为 `0600`（仅所有者可读写）
- 不应将 token 提交到版本控制
- 应定期刷新 token（access_token 通常 1 小时过期）

---

## 7. 故障排查

### Device Code Flow 常见问题

| 问题 | 原因 | 解决方案 |
|------|------|----------|
| "device login expired" | 15 分钟内未完成授权 | 重新运行 `provider login` |
| 轮询一直 pending | 用户未在网站完成授权 | 确认用户在网站输入了正确的 device code |
| "OAuth server did not return a refresh token" | OpenAI 服务端问题 | 重试，或检查 OpenAI 服务状态 |

### Authorization Code Flow 常见问题

| 问题 | 原因 | 解决方案 |
|------|------|----------|
| "requires an interactive terminal" | 在非交互式环境运行 | 在终端（非脚本）中运行 |
| "authorization code must not be empty" | 用户未粘贴 code | 从浏览器 URL 复制 authorization_code 参数 |
| "failed to determine project ID" | Antigravity 服务端问题 | 重新登录，或检查 Antigravity 服务状态 |

---

## 8. 参考实现

### 三方对比

| 实现 | Device Code Flow | Authorization Code Flow |
|------|------------------|-------------------------|
| **llm-proxy (Rust v2)** | ✅ `src/auth/login.rs` | ✅ `src/auth/login.rs` |
| **Codex CLI (Rust)** | ✅ `codex-rs/login/src/device_code_auth.rs` | ❌ 不支持 |
| **CLIProxyAPI (Go)** | ✅ `sdk/auth/codex_device.go` | ❌ 不支持 |

### 关键差异

1. **轮询成功判断**：
   - Codex CLI：`status.is_success()`（接受所有 2xx）
   - CLIProxyAPI：`code >= 200 && code < 300`（接受所有 2xx）
   - llm-proxy：`error_for_status()`（接受所有 2xx）✅ 一致

2. **轮询间隔**：
   - Codex CLI：使用服务端 `interval`
   - CLIProxyAPI：使用服务端 `interval`（默认 5s）
   - llm-proxy：`max(server_interval, 2) + 3`（安全边距）

3. **Accept header**：
   - Codex CLI：无（reqwest 默认）
   - CLIProxyAPI：`application/json`
   - llm-proxy：无（reqwest 默认）⚠️ 可能需要添加

---

## 9. 未来改进

1. **添加 Accept header**：在 `poll_openai_device_token` 中添加 `Accept: application/json`
2. **自动打开浏览器**：Antigravity 流程可以自动打开浏览器（使用 `open` 命令）
3. **本地回调服务器**：Antigravity 流程可以启动本地 HTTP 服务器接收回调，避免手动粘贴
4. **Token 自动刷新**：在 token 过期前自动刷新，避免用户手动重新登录

---

## 10. 相关文档

- [ADR-015: OAuth 轮询安全余量](../decisions/015-oauth-polling-safety-margin.md)
- [ADR-016: Token 刷新重试 Singleflight](../decisions/016-token-refresh-retry-singleflight.md)
