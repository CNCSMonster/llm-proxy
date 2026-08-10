# OAuth Accounts Storage Design

> Status: accepted
> Last updated: 2026-07-30

## 概述

本文档定义 OAuth 账号凭据的存储格式、验证规则和边界处理策略。

## 文件位置

- 路径：`~/.config/llm-proxy/oauth_accounts.json`
- 权限：`0600`（仅所有者可读写）
- 编码：UTF-8
- 格式：JSON

## 顶层结构

```json
{
  "version": 1,
  "antigravity": { ... },
  "openai": { ... }
}
```

### 约束

| 字段 | 类型 | 必填 | 说明 |
|------|------|------|------|
| `version` | integer | ✅ | Schema 版本号，当前为 1 |
| `antigravity` | object | ❌ | Antigravity OAuth 账号分组 |
| `openai` | object | ❌ | OpenAI OAuth 账号分组 |

**规则：**
- `version` 必须为正整数；`version > 1` 拒绝加载（见"迁移策略"）
- 类型分组可选（允许只存储部分类型）
- 允许空分组（`"antigravity": {}`）
- 未知类型分组不保留（serde 忽略，下次写入时丢弃——接受的向前不兼容行为）

## Account ID 规范

### 格式要求

```
^[a-zA-Z0-9_-]+$
```

- 只允许 ASCII 字母、数字、下划线、连字符
- **不允许** Unicode 字符
- 长度：1-64 字符
- **大小写敏感**：`Default` 和 `default` 是不同账号

### 保留名称

**无保留名称**。所有符合格式的 ID 都允许使用。

**建议命名约定：**
- `default`：默认账号
- `personal`：个人账号
- `work`：工作账号
- `test`：测试账号

### 唯一性

- 在同一类型分组内必须唯一
- 跨类型分组可以重复（如 `antigravity.default` 和 `openai.default`）
- **已知限制：** 手编文件中的 JSON 重复 key 被 serde_json 静默覆盖（后者覆盖前者），无法检测

## 通用字段规范

所有 OAuth 类型共享的字段：

| 字段 | 类型 | 必填 | 约束 | 说明 |
|------|------|------|------|------|
| `account_label` | string | ✅ | minLength: 1 | 邮箱或显示名 |
| `access_token` | string | ✅ | minLength: 20 | OAuth access token |
| `refresh_token` | string | ✅ | minLength: 20 | OAuth refresh token |
| `expires_at_unix` | integer | ✅ | ≥ 1000000000 | Token 过期时间（Unix 秒） |
| `updated_at_unix` | integer | ❌ | ≥ 1000000000 | 最后更新时间（Unix 秒） |

**验证规则：**
- `access_token` 和 `refresh_token` 不能相同
- `expires_at_unix` 不得早于 `updated_at_unix`（允许相等——某些 provider 刷新后返回相同 expiry）
- `expires_at_unix` / `updated_at_unix` ≥ 1000000000（2001 年后，防止占位符或误填）
- Token 长度 ≥ 20 字符（防止占位符）

## Antigravity 账号规范

### 特有字段

| 字段 | 类型 | 必填 | 约束 | 说明 |
|------|------|------|------|------|
| `project_id` | string | ✅ | `^[a-z][a-z0-9-]{4,28}[a-z0-9]$` | Google Cloud 项目 ID |

**Project ID 格式说明：**
- 以小写字母开头
- 只允许小写字母、数字、连字符
- 长度：6-30 字符
- 以小写字母或数字结尾

### 示例

```json
{
  "default": {
    "account_label": "user@gmail.com",
    "project_id": "example-project-12345",
    "access_token": "ya29.a0EXAMPLE...",
    "refresh_token": "1//0EXAMPLE...",
    "expires_at_unix": 1785245247,
    "updated_at_unix": 1785226019
  }
}
```

## OpenAI 账号规范

### 特有字段

**无**。仅使用通用字段。

### 示例

```json
{
  "personal": {
    "account_label": "user@gmail.com",
    "access_token": "eyJhbGciOiJSUzI1NiIs...",
    "refresh_token": "rt.1.EXAMPLE...",
    "expires_at_unix": 1786090020,
    "updated_at_unix": 1785226020
  }
}
```

## 文件操作规范

### 权限控制

- 文件权限必须为 `0600` 或更严格
- 加载时检查权限，如果过于宽松则警告
- 创建时自动设置 `0600` 权限

### 备份策略

- 每次写入前创建 `.bak` 备份
- 备份文件命名：`oauth_accounts.json.bak.{timestamp_nanos}`（纳秒时间戳，避免同秒写入互相覆盖）
- 保留最近 3 个备份
- 清理失败（如文件权限问题）仅警告，不阻塞写入

**已知局限：** serve 长跑时 access token 约 1 小时刷新一次，3 份备份只覆盖约 3 小时窗口；且备份是写入前的旧数据，其中的 refresh_token 可能已被轮换作废。恢复后必须提示用户重新验证账号（见"错误恢复"章节）。如需更长保留窗口，未来可按时间分层（最近 3 份 + 每天 1 份 × 7 天），当前不实现。

### 并发访问

- 使用**目录级锁**（`data.lock`，位于 config 目录），全局一把锁保护目录内所有数据文件（`config.toml`、`oauth_accounts.json` 等），确保跨文件操作自然原子化
- 锁文件为 `~/.config/llm-proxy/data.lock`，与数据文件同目录（不受 `/tmp` 清理影响，scp 分发时多带一个 0 字节文件也无害）
- **锁覆盖整个 load → modify → save 事务**（不是只锁写阶段），防止跨进程丢更新
- 写入走原子 rename，读取因此无需加锁（读安全的唯一保证是写端 rename 原子性；不存在"小文件读取原子"的说法）

### 原子写入

```rust
// 伪代码（调用方必须持有写锁）
let temp_path = path.with_extension("tmp");
// Unix 上以 0600 权限创建临时文件，避免 token 明文短暂暴露
let mut file = OpenOptions::new()
    .create(true).truncate(true).write(true)
    .mode(0o600)  // Unix only
    .open(&temp_path)?;
file.write_all(json.as_bytes())?;
file.sync_all()?;              // 内容落盘
fs::rename(&temp_path, path)?; // 原子替换
sync_directory(parent)?;       // 目录落盘（best-effort）
```

## 验证策略

### 加载时验证（Eager Validation）

**必须验证：**
- JSON 格式正确
- Schema 符合规范
- 必填字段存在
- 字段类型正确
- 字段约束满足（长度、格式、范围）
- `version == 1`（大于 1 拒绝加载）

**不验证：**
- Token 是否过期（运行时检查）
- Token 是否有效（API 调用时检查）
- Account ID 唯一性——JSON 重复 key 被 serde_json 静默覆盖，无法检测（已知限制，见"迁移策略"）
- 文件权限（仅警告）

### 写入时验证

**必须验证：**
- 所有加载时验证规则（`OAuthAccounts::validate()` 在序列化前执行）

### 运行时验证

**必须验证：**
- Token 是否过期（`get_*_token` 使用时检查，过期报错提示 refresh）
- Token 是否有效（API 调用失败时处理）
- Provider 引用的账号是否存在（启动时校验 + 使用时检查）

## 迁移策略

**版本兼容原则：支持单向升级（新应用读旧配置），拒绝反向使用（旧应用读新配置）。**

- **向前兼容（新 binary 读旧 config）**：允许。新版本 llm-proxy 可以加载旧版本配置文件，并提供迁移提示或自动迁移机制，帮助用户轻松升级到新格式
- **向后兼容（旧 binary 读新 config）**：不允许。旧版本 llm-proxy 遇到新版本配置文件时严格拒绝加载，报错提示升级，绝不"尽力解析"（防止误写或数据损坏）

**当前状态（v1）：**
- 当前版本为 v1，无迁移需求
- 未来升级到 v2 时，v2 binary 应能读取 v1 config 并提供迁移路径
- v1 binary 遇到 v2 config 时严格拒绝

**版本演进的硬性规则（避免静默数据损坏）：**

- `version > 1`（当前 binary 不认识的版本）：**严格拒绝加载，不尝试恢复**，报错提示升级 llm-proxy（防止旧 binary 用旧备份覆盖新配置）
- `version == 1`（当前 binary 认识的版本）：正常加载，未来 v2 binary 应能读取并迁移
- 未知类型分组（如未来的 `github` 分组）：serde 默认忽略，**下次写入时被静默丢弃**。这是当前接受的明确行为——升级 binary 前不要用旧 binary 执行任何写操作（login/refresh/logout）
- 同版本内新增字段：新字段必须为可选（`Option` + `serde(default)`），旧 binary 加载时忽略、写回时丢失——同样属于接受的向前不兼容行为
- JSON 重复 key（手编文件可能出现）：serde_json 后者静默覆盖前者，**不检测**，视为已知限制

**未来如需版本升级：**
1. 定义新版本 schema
2. 编写迁移脚本
3. 手动执行迁移

## 错误处理

### 错误消息格式

```
Error: Invalid OAuth account '{account_id}' in {type} group:
  Field: {field_name}
  Expected: {expected}
  Actual: {actual}
  File: {file_path}
  
  Hint: {suggestion}
```

### 示例

```
Error: Invalid OAuth account 'default' in antigravity group:
  Field: project_id
  Expected: Google Cloud project ID format (^[a-z][a-z0-9-]{4,28}[a-z0-9]$)
  Actual: "INVALID_PROJECT"
  File: ~/.config/llm-proxy/oauth_accounts.json
  
  Hint: Project ID must start with lowercase letter, contain only lowercase letters, numbers, and hyphens, and be 6-30 characters long.
```

## 账号删除与引用处理

### 删除账号时的处理

**策略：Provider 引用已删除账号时，标记为失效状态。**

1. **删除账号**：从 `oauth_accounts.json` 中移除账号
2. **Provider 状态标记**：引用该账号的 provider 状态标记为 `auth_invalid`
3. **用户提示**：显示 "Provider 认证失效，需要重新登录"
4. **重新登录**：用户执行 `llm-proxy provider login <provider>` 重新完成 OAuth 流程

**实现逻辑：**
```rust
// 加载 provider 时验证账号引用
fn validate_provider_auth(provider: &ProviderConfig, accounts: &OAuthAccounts) -> Result<()> {
    match &provider.auth {
        AuthConfig::AntigravityOauth { account } => {
            let account_id = account.as_ref().unwrap_or(&provider.id);
            if !accounts.antigravity.contains_key(account_id) {
                return Err(anyhow!(
                    "Provider '{}' references non-existent antigravity account '{}'. \
                     Run: llm-proxy provider login {}",
                    provider.id, account_id, provider.id
                ));
            }
        }
        AuthConfig::OpenaiOauth { account } => {
            let account_id = account.as_ref().unwrap_or(&provider.id);
            if !accounts.openai.contains_key(account_id) {
                return Err(anyhow!(
                    "Provider '{}' references non-existent openai account '{}'. \
                     Run: llm-proxy provider login {}",
                    provider.id, account_id, provider.id
                ));
            }
        }
        _ => {}
    }
    Ok(())
}
```

### 账号重命名

**不支持原子性重命名。**

如需重命名：
1. 创建新账号（复制配置）
2. 更新所有引用该账号的 provider 配置
3. 删除旧账号

**建议：** 避免重命名，使用稳定的 account ID。

### 敏感数据保护

- 文件权限 `0600`
- 不在日志中输出 token
- 不在错误消息中输出完整 token（只显示前 10 个字符）
- 考虑未来支持加密存储（当前不实现）

### Token 泄露防护

- 定期提醒用户检查文件权限
- 提供 `llm-proxy oauth rotate` 命令轮换 token
- 提供 `llm-proxy oauth revoke` 命令撤销 token

## 并发访问控制

### 问题场景

- 多个 `llm-proxy` 实例同时运行
- `llm-proxy provider login` 和 `llm-proxy serve` 并发执行
- Token 刷新时的竞态条件

### 解决方案

**文件锁覆盖整个"读-改-写"事务（不是只锁写阶段）：**

只锁写阶段会导致跨进程丢更新：进程 A（login 新账号）和进程 B（refresh token）各自在锁外 load 旧文件、内存修改、持锁写回——后写者全量覆盖先写者。因此锁的临界区必须是 load → modify → save 整体：

```rust
fn with_locked_accounts<T>(path: &Path, f: impl FnOnce(&mut OAuthAccounts) -> Result<T>) -> Result<T> {
    let lock_file = acquire_lock(path)?;      // 锁 `parent(path)/data.lock`，非 path.with_extension("lock")
    let _guard = scopeguard::guard(lock_file, |f| { let _ = f.unlock(); });
    let mut accounts = load_oauth_accounts(path)?;  // 锁内 load，保证读到最新状态
    f(&mut accounts)                          // 调用方决定改什么、是否 save
}
```

**各操作的锁策略：**

| 操作 | 网络阶段 | 临界区（data.lock 内） | 说明 |
|------|---------|----------------------|------|
| login | 锁外（OAuth 流程可能数分钟） | 锁内 insert + save | 长时间持锁会阻塞其他进程，网络阶段必须在锁外。若网络成功但构造的账号数据通过不了验证（如没有 refresh_token），流程报错退出，不保存任何数据 |
| refresh | **锁内**（单次 HTTP，秒级） | 锁内 load + refresh + save | 必须串行化，见下方"Token 刷新竞态" |
| logout | 无 | 锁内 remove + save | — |
| config 写入（add/remove provider） | 无 | 锁内（当前未实现，均为 CLI 单进程操作） | 未来多进程场景下，config.toml 写入也应走 data.lock 保证跨文件原子性 |
| 读 token（serve 请求路径） | 无 | 不加锁 | 依赖写端 rename 原子性 |

**读取不需要锁的唯一前提是写端永远走原子 rename。** 没有"JSON 小于 4KB 读取即原子"的性质——读安全完全来自 rename 语义。

### Token 刷新竞态

**问题：** 多数 OAuth provider 的 refresh token 是轮换式的（用一次旧的就作废，甚至触发 token family 撤销）。两个进程同时发现 token 过期、同时用旧 refresh_token 调刷新 API：一个成功一个失败，或两个都成功后写者覆盖先写者的新 token 对。

**解决：** 整个"读 refresh_token → 调刷新 API → 写回新 token 对"在跨进程文件锁内串行执行。持锁后重新 load，若发现 token 未过期且 60 秒内刚被刷新过（`updated_at_unix` 很近），说明另一个进程刚完成刷新，直接复用结果、跳过网络调用。`tokio::Mutex` 之类的进程内 singleflight 对多进程/多实例无效，只能作为未来 serve 内自动刷新的补充优化。

**手动 refresh 语义保留：** 用户显式执行 `provider refresh` 时，若 token 未过期但 `updated_at_unix` 较旧（> 60 秒），仍强制执行刷新——60 秒跳过窗口只用于识别并发进程的刚完成刷新，不改变手动命令的强制语义。

## 文件权限控制

### 权限要求

- **必须：** `0600`（所有者可读写）
- **允许：** `0400`（所有者只读，不推荐）
- **禁止：** `0644`, `0664`, `0666` 等（组/其他用户可访问）

### 权限检查

```rust
use std::os::unix::fs::PermissionsExt;

fn check_file_permissions(path: &Path) -> Result<()> {
    let metadata = std::fs::metadata(path)?;
    let permissions = metadata.permissions();
    let mode = permissions.mode() & 0o777;
    
    if mode & 0o077 != 0 {
        warn!(
            "OAuth accounts file {} has insecure permissions: {:o}. \
             Expected: 600. Fix with: chmod 600 {}",
            path.display(), mode, path.display()
        );
    }
    
    Ok(())
}
```

### 权限设置

```rust
fn set_secure_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    
    let permissions = std::fs::Permissions::from_mode(0o600);
    std::fs::set_permissions(path, permissions)?;
    
    Ok(())
}
```

## 备份策略

### 自动备份

**每次写入前创建备份（在写锁内执行）：**

```rust
fn backup_oauth_accounts(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    // 纳秒时间戳，避免同秒写入的备份名互相覆盖；
    // 字符串拼接生成 `oauth_accounts.json.bak.{nanos}`（with_extension 会错误地替换掉 .json）
    let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let backup_path = PathBuf::from(format!("{}.bak.{}", path.display(), timestamp));
    fs::copy(path, &backup_path)?;
    cleanup_old_backups(path, 3)?;  // 保留最近 3 个；清理失败仅警告，不阻塞写入（在写锁内执行，无跨进程竞态）
    Ok(())
}
```

**保留窗口的已知局限：** 每次写入都备份 × 只留 3 份 → serve 自动刷新场景下备份窗口仅约数小时；且备份是写入前的旧数据，refresh_token 可能已被轮换作废。恢复后必须提示用户重新验证（见"错误恢复"）。

### 手动备份

**提供命令：**
```bash
llm-proxy oauth backup
# 输出：Backed up to ~/.config/llm-proxy/oauth_accounts.json.bak.20260729_143022
```

## Token 格式验证

### 验证策略

**不验证 Token 格式（JWT 结构等）。**

**原因：**
1. Token 格式可能变化（不同 OAuth 提供商）
2. 验证增加复杂性，收益有限
3. 无效 Token 会在 API 调用时被发现

**仅验证：**
- 非空
- 长度 ≥ 20 字符
- `access_token` ≠ `refresh_token`

### Token 过期处理

**运行时检查，不在加载时检查：**

```rust
impl OpenaiAccount {
    /// 严格判断，不含容差
    pub fn is_expired(&self) -> bool {
        let now = unix_secs() as i64;
        self.expires_at_unix <= now
    }
}
```

**当前设计：**
- `is_expired()` 严格判断（`expires_at <= now`），不含任何容差
- 获取 token 时（`get_*_token`）若已过期则报错，提示运行 `llm-proxy provider refresh <provider>`
- **刷新是显式操作**（CLI 命令或 TUI），当前不在请求路径上自动刷新；未来实现自动刷新时，"提前 N 分钟刷新"的容差只用于刷新决策，不改变 `is_expired` 的严格语义
- TUI 的 `⚠ Expired` 状态含义是"access token 已过期，需要 refresh"，**不等于**必须重新登录；只有 refresh token 失效（refresh 失败）时才需要重新 login

## 类型扩展性

### 新增 OAuth 类型

**步骤：**

1. **定义 Rust struct：**
```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewTypeAccount {
    pub account_label: String,
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at_unix: i64,
    pub updated_at_unix: Option<i64>,
    // 特有字段
    pub special_field: String,
}
```

2. **更新 OAuthAccounts：**
```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthAccounts {
    pub version: u32,
    pub antigravity: HashMap<String, AntigravityAccount>,
    pub openai: HashMap<String, OpenaiAccount>,
    pub new_type: HashMap<String, NewTypeAccount>,  // 新增
}
```

3. **更新 JSON Schema**

4. **更新验证逻辑**

5. **更新 config.toml 解析**

### 未知类型处理

**不保留未知类型分组（与"不做向前兼容"立场一致）。**

serde 默认忽略未知字段：旧二进制加载含新分组的文件后，下次写入时新分组被静默丢弃。这是接受的明确行为——升级二进制前不要用旧二进制执行写操作（login/refresh/logout）。`version > 1` 的文件会被直接拒绝加载（见"迁移策略"），因此该场景只发生在同版本（version = 1）新增分组时。

## 验证时机

已合并至前文"验证策略"章节（加载时 / 写入时 / 运行时三段），此处不再重复。

## 默认账号概念

**不需要 "default" 特殊概念。**

**当前设计已处理：**
- `account` 字段省略时，默认使用 `provider_id` 作为 account ID
- 用户可以使用任何名称（如 `default`, `personal`, `work`）

**建议命名约定：**
- 单账号：使用 `default` 或 provider 名称
- 多账号：使用描述性名称（`personal`, `work`, `test`）

## 账号共享

**支持多个 provider 共享同一账号。**

**示例：**
```toml
[providers.openai-subscription-1.auth]
type = "openai_oauth"
account = "shared-account"

[providers.openai-subscription-2.auth]
type = "openai_oauth"
account = "shared-account"  # 共享同一账号
```

**注意事项：**
- 共享账号的 token 刷新会影响所有引用
- 删除账号会影响所有引用的 provider
- 建议避免共享，每个 provider 使用独立账号

## Account ID 规范补充

### Unicode 支持

**不允许 Unicode 字符。**

**原因：**
- 跨平台兼容性问题
- 配置文件可读性
- 简化验证逻辑

### 大小写敏感

**大小写敏感。**

- `Default` 和 `default` 是不同账号
- 建议使用小写，避免混淆

### 账号数量限制

**无硬性限制。**

**实际限制：**
- 文件大小（建议 < 1MB）
- 内存占用（每个账号约 1-2KB）
- 管理复杂度（建议 < 100 个账号）

## 账号元数据

**当前不存储额外元数据。**

**未来可扩展字段（可选）：**
- `created_at_unix`: 账号创建时间
- `last_used_unix`: 最后使用时间
- `last_refreshed_unix`: 最后刷新时间
- `notes`: 备注信息

**当前仅存储：**
- `account_label`: 邮箱或显示名
- `updated_at_unix`: 最后更新时间

## 加密存储

**当前不实现加密。**

**原因：**
- 文件权限 `0600` 已提供基本保护
- 加密增加复杂性（密钥管理、性能开销）
- 本地配置文件风险较低

**未来如需加密：**
1. 使用系统密钥链（macOS Keychain, Windows Credential Manager, Linux Secret Service）
2. 或使用用户密码派生密钥（PBKDF2, Argon2）
3. 加密算法：AES-256-GCM

## 部分更新

**不支持单账号原子更新。**

**当前实现：**
- 读取整个文件
- 修改内存中的结构
- 写入整个文件（原子写入）

**原因：**
- 文件小（< 100KB），全量读写性能可接受
- 简化实现逻辑
- 避免文件碎片化

## 账号模板

**不提供模板功能。**

**原因：**
- OAuth 账号必须通过登录流程创建
- 模板无法包含有效的 token
- 增加维护成本

**替代方案：**
- 文档中提供示例配置
- CLI 命令 `llm-proxy provider login` 引导用户完成登录

## 导入导出

**提供导入导出命令。**

### 导出

```bash
llm-proxy oauth export [--type antigravity|openai] [--account <id>]
# 输出：JSON 格式的账号数据（不含 token，仅元数据）
```

**安全考虑：**
- 默认不导出 token（敏感信息）
- 如需导出 token，使用 `--include-tokens`（警告用户风险）
- 导出文件权限设置为 `0600`

### 导入

```bash
llm-proxy oauth import <file>
# 输入：JSON 格式的账号数据
```

**验证：**
- Schema 验证
- Account ID 冲突检查
- 提示用户确认覆盖

## 缺失配置文件的处理

### 场景

**情况：** 只有 config.toml，没有 oauth_accounts.json

```bash
~/.config/llm-proxy/
├── config.toml  ✅ 存在
└── oauth_accounts.json  ❌ 不存在
```

### 处理策略

**核心原则：配置文件不存在 = 登录状态信息不存在**

#### 1. 启动时处理

**允许启动，但给出警告：**

```rust
fn validate_oauth_on_startup(config: &Config, accounts_path: &Path) -> Result<()> {
    // 检查文件是否存在
    if !accounts_path.exists() {
        // 检查是否有 OAuth provider
        let oauth_providers: Vec<_> = config.providers.iter()
            .filter(|(_, p)| matches!(&p.auth, AuthConfig::AntigravityOauth { .. } | AuthConfig::OpenaiOauth { .. }))
            .collect();
        
        if !oauth_providers.is_empty() {
            warn!(
                "OAuth accounts file not found: {}\n\
                 Found {} OAuth provider(s) that require login:\n\
                 {}\n\
                 Run: llm-proxy provider login <provider>\n\
                 Or use TUI: llm-proxy provider",
                accounts_path.display(),
                oauth_providers.len(),
                oauth_providers.iter()
                    .map(|(id, _)| format!("  - {}", id))
                    .collect::<Vec<_>>()
                    .join("\n")
            );
        }
        
        return Ok(());  // 允许启动
    }
    
    // 文件存在，验证每个 OAuth provider 的账号引用
    let accounts = load_oauth_accounts(accounts_path)?;
    
    for (provider_id, provider) in &config.providers {
        match &provider.auth {
            AuthConfig::AntigravityOauth { account } => {
                let account_id = account.as_ref().unwrap_or(provider_id);
                if !accounts.antigravity.contains_key(account_id) {
                    warn!(
                        "Provider '{}' references non-existent antigravity account '{}'\n\
                         Run: llm-proxy provider login {}",
                        provider_id, account_id, provider_id
                    );
                }
            }
            AuthConfig::OpenaiOauth { account } => {
                let account_id = account.as_ref().unwrap_or(provider_id);
                if !accounts.openai.contains_key(account_id) {
                    warn!(
                        "Provider '{}' references non-existent openai account '{}'\n\
                         Run: llm-proxy provider login {}",
                        provider_id, account_id, provider_id
                    );
                }
            }
            _ => {}
        }
    }
    
    Ok(())
}
```

**启动输出示例：**

```bash
$ llm-proxy serve

⚠️  Warning: OAuth accounts file not found
   Found 2 OAuth provider(s) that require login:
   - antigravity
   - openai-subscription
   
   Run: llm-proxy provider login <provider>
   Or use TUI: llm-proxy provider

✓  llm-proxy started on 127.0.0.1:8989
```

#### 2. 运行时处理

**请求未登录的 OAuth provider 时返回错误：**

```rust
async fn get_oauth_token(provider_id: &str, accounts_path: &Path) -> Result<String> {
    let accounts = load_oauth_accounts(accounts_path)
        .map_err(|_| anyhow!(
            "Provider '{}' requires OAuth login\n\
             Run: llm-proxy provider login {}\n\
             Or use TUI: llm-proxy provider",
            provider_id, provider_id
        ))?;
    
    // 查找账号
    let token = match &provider.auth {
        AuthConfig::AntigravityOauth { account } => {
            let account_id = account.as_ref().unwrap_or(provider_id);
            accounts.antigravity.get(account_id)
                .map(|a| a.access_token.clone())
                .ok_or_else(|| anyhow!(
                    "Antigravity account '{}' not found\n\
                     Run: llm-proxy provider login {}",
                    account_id, provider_id
                ))?
        }
        AuthConfig::OpenaiOauth { account } => {
            let account_id = account.as_ref().unwrap_or(provider_id);
            accounts.openai.get(account_id)
                .map(|a| a.access_token.clone())
                .ok_or_else(|| anyhow!(
                    "OpenAI account '{}' not found\n\
                     Run: llm-proxy provider login {}",
                    account_id, provider_id
                ))?
        }
        _ => bail!("Provider '{}' is not OAuth type", provider_id),
    };
    
    Ok(token)
}
```

**API 错误响应：**

```json
{
  "error": {
    "message": "Provider 'antigravity' requires OAuth login\nRun: llm-proxy provider login antigravity",
    "type": "auth_required",
    "provider_id": "antigravity",
    "auth_type": "antigravity_oauth"
  }
}
```

#### 3. TUI 界面登录状态显示

**`llm-proxy provider` 命令进入 TUI 界面：**

```
┌─ Provider Management ──────────────────────────────────────────┐
│                                                                  │
│  ▶ deepseek                 OpenAI Chat    API Key    ✓ Ready   │
│    bailian-coding-plan      OpenAI Chat    API Key    ✓ Ready   │
│    antigravity              OpenAI Resp    OAuth      ⚠ Not logged in
│    openai-subscription      OpenAI Resp    OAuth      ⚠ Not logged in
│    kimi-code-api            OpenAI Chat    API Key    ✓ Ready   │
│                                                                  │
│  ────────────────────────────────────────────────────────────── │
│  ↑↓ Navigate  Enter Select/Login  d Delete  q Quit             │
└──────────────────────────────────────────────────────────────────┘
```

**登录状态标识：**

| 状态 | 图标 | 说明 |
|------|------|------|
| `✓ Ready` | ✅ | API Key 已配置或 OAuth 已登录 |
| `⚠ Not logged in` | ⚠️ | OAuth provider 未登录 |
| `⚠ Token expired` | ⚠️ | access token 已过期，运行 `provider refresh` 即可（无需重新登录；仅 refresh 失败时才需重新 login） |
| `✗ Missing key` | ❌ | API Key 环境变量未设置 |

**TUI 状态检查逻辑：**

```rust
fn get_provider_auth_status(provider: &ProviderConfig, accounts: &OAuthAccounts) -> AuthStatus {
    match &provider.auth {
        AuthConfig::ApiKeyEnv { env } => {
            if std::env::var(env).is_ok() {
                AuthStatus::Ready
            } else {
                AuthStatus::MissingKey(env.clone())
            }
        }
        AuthConfig::AntigravityOauth { account } => {
            let account_id = account.as_ref().unwrap_or(&provider.id);
            match accounts.antigravity.get(account_id) {
                Some(acc) if acc.is_expired() => AuthStatus::Expired,
                Some(_) => AuthStatus::Ready,
                None => AuthStatus::NotLoggedIn,
            }
        }
        AuthConfig::OpenaiOauth { account } => {
            let account_id = account.as_ref().unwrap_or(&provider.id);
            match accounts.openai.get(account_id) {
                Some(acc) if acc.is_expired() => AuthStatus::Expired,
                Some(_) => AuthStatus::Ready,
                None => AuthStatus::NotLoggedIn,
            }
        }
        AuthConfig::None => AuthStatus::Ready,
    }
}

enum AuthStatus {
    Ready,
    NotLoggedIn,
    Expired,
    MissingKey(String),
}
```

**TUI 交互流程：**

```
1. 用户运行: llm-proxy provider
   ↓
2. 进入 TUI 界面，显示 provider 列表
   ↓
3. 用户选择未登录的 OAuth provider（如 antigravity）
   ↓
4. 按 Enter
   ↓
5. 显示选项菜单：
   ┌─ antigravity (OAuth) ─────────────────┐
   │                                         │
   │  Status: ⚠ Not logged in               │
   │                                         │
   │  [ Login ]  [ Back ]                   │
   └─────────────────────────────────────────┘
   ↓
6. 用户选择 Login
   ↓
7. 执行 OAuth 登录流程（浏览器或设备码）
   ↓
8. 登录成功，保存凭据
   ↓
9. 返回 provider 列表，状态更新为 ✓ Ready
```

#### 4. CLI 登录命令

**`llm-proxy provider login <provider>`：**

```bash
$ llm-proxy provider login antigravity

Opening browser for OAuth login...
✓  Logged in as user@gmail.com
✓  Saved to ~/.config/llm-proxy/oauth_accounts.json

Provider 'antigravity' is now ready to use.
```

**登录流程：**
1. 检查 provider 是否为 OAuth 类型
2. 执行 OAuth 登录流程（浏览器或设备码）
3. 获取 access_token 和 refresh_token
4. 保存到 oauth_accounts.json
5. 设置文件权限 0600

#### 5. 自动创建空文件（可选）

**首次启动时自动创建：**

```rust
fn ensure_oauth_accounts_file(path: &Path) -> Result<()> {
    if !path.exists() {
        let empty = OAuthAccounts {
            version: 1,
            antigravity: HashMap::new(),
            openai: HashMap::new(),
        };
        
        let json = serde_json::to_string_pretty(&empty)?;
        std::fs::write(path, json)?;
        set_secure_permissions(path)?;
        
        info!("Created empty OAuth accounts file: {}", path.display());
    }
    
    Ok(())
}
```

### 用户体验流程

**场景 1：新环境，只有 config.toml**

```bash
# 1. 启动服务
$ llm-proxy serve

⚠️  Warning: OAuth accounts file not found
   Found 2 OAuth provider(s) that require login:
   - antigravity
   - openai-subscription
   
   Run: llm-proxy provider login <provider>
   Or use TUI: llm-proxy provider

✓  llm-proxy started on 127.0.0.1:8989

# 2. 使用 TUI 登录
$ llm-proxy provider

# 在 TUI 中选择 antigravity，按 Enter，选择 Login
# 完成 OAuth 登录流程

# 3. 返回，状态更新为 ✓ Ready
# 选择 openai-subscription，登录

# 4. 所有 provider 就绪，退出 TUI

# 5. 现在可以正常使用所有 provider
```

**场景 2：运行时请求未登录的 provider**

```bash
# 请求未登录的 provider
$ curl -X POST http://localhost:8989/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{"model": "antigravity/gemini-pro", "messages": [...]}'

# 返回错误
{
  "error": {
    "message": "Provider 'antigravity' requires OAuth login\nRun: llm-proxy provider login antigravity",
    "type": "auth_required"
  }
}

# 登录后重试
$ llm-proxy provider login antigravity
✓  Logged in as user@gmail.com

$ curl -X POST http://localhost:8989/v1/chat/completions ...
# 成功
```

### 关键设计决策

| 决策 | 说明 |
|------|------|
| **允许启动** | 即使 OAuth 账号缺失也允许启动（非 OAuth provider 仍可用） |
| **启动时警告** | 清晰提示哪些 provider 需要登录 |
| **运行时错误** | 使用未登录 provider 时返回明确的错误消息和修复命令 |
| **TUI 状态显示** | provider 列表旁边显示登录状态 |
| **TUI 登录** | 可以在 TUI 中选择 provider 并登录 |
| **CLI 登录** | `llm-proxy provider login <provider>` 命令 |
| **自动创建文件** | 可选：首次启动时创建空的 oauth_accounts.json |

## 错误恢复

### 文件损坏处理

**场景：** JSON 文件损坏（语法错误、截断、编码问题）

**处理策略：按 mtime 倒序遍历所有备份，恢复成功后写回主文件。**

```rust
pub fn load_oauth_accounts_with_recovery(path: &Path) -> Result<OAuthAccounts> {
    match load_oauth_accounts(path) {
        Ok(accounts) => Ok(accounts),
        Err(err) => {
            let backups = find_backups_newest_first(path);  // 全部备份，mtime 倒序
            for backup in &backups {
                if let Ok(accounts) = load_oauth_accounts(backup) {
                    warn!("recovered from backup {}", backup.display());
                    eprintln!(
                        "Warning: OAuth accounts file corrupted; recovered from backup {}.\n\
                         The backup may contain stale or rotated-out tokens. \
                         Run `llm-proxy provider refresh <provider>` or relogin if requests fail.",
                        backup.display()
                    );
                    // 写回主文件，避免每次启动都走恢复路径（best-effort）
                    let _ = save_oauth_accounts(path, &accounts);
                    return Ok(accounts);
                }
                // 最新备份也损坏 → 继续尝试次新的
            }
            bail!("OAuth accounts file corrupted: {err}; all backups invalid");
        }
    }
}
```

**设计要点：**
- 不只试"最新一份"——最新备份可能同样损坏，按 mtime 倒序逐个尝试直到成功
- 恢复成功后**立即写回主文件**（走 `save_oauth_accounts`，内部自带写锁），否则每次启动都重复恢复，且后续写入会基于恢复结果覆盖，掩盖根因。写回与并发的 login/refresh 通过锁串行化，不会互相覆盖
- 备份是写入前的旧数据，refresh_token 可能已被轮换作废——恢复提示必须明确告知"token 可能已失效，请求失败时请 refresh 或重新 login"
- 全部备份无效时才 bail，要求人工介入
- **版本不匹配（`version > 1`）不进入恢复路径**——直接报错提示升级 llm-proxy，防止用旧备份覆盖新配置（这是"旧 binary 不读新 config"规则的延伸）

### 文件不存在时的行为

**数据文件不存在（首次使用/新环境）：**
- `load_oauth_accounts` 返回空的 `OAuthAccounts::new()`（不报错）
- `with_locked_accounts` 锁内 load 拿到空结构，closure 插入新账号后 save 创建文件
- 因此首次 `provider login` 无需任何特殊处理，锁保证并发首次写入不互相覆盖

**锁文件不存在：**
- `acquire_lock` 通过 `OpenOptions::create(true)` 创建锁文件
- `fs2::lock_exclusive()` 阻塞直到获取锁，两进程同时创建锁文件也安全
- 锁文件永不删除（无害，占用可忽略）

### 写入失败处理

**场景：** 磁盘满、权限不足、I/O 错误

**处理策略：**

```rust
fn save_oauth_accounts_locked(path: &Path, data: &OAuthAccounts) -> Result<()> {
    data.validate()?;              // 写入前验证（内存中）
    backup_oauth_accounts(path)?;  // 备份现有文件

    let temp_path = path.with_extension("tmp");
    let write_result = write_temp_and_rename(&temp_path, path, data);
    if write_result.is_err() {
        let _ = std::fs::remove_file(&temp_path);  // 清理临时文件
    }
    write_result
}
```

**说明：** 验证在序列化前对内存结构执行（`OAuthAccounts::validate()`），不存在"临时文件验证失败"阶段；备份在写入前创建，连续写入失败会产生多份相同内容的备份（无害，都是同一份好数据）。

## 符号链接处理

**策略：文件本身为符号链接 → 拒绝；父目录为符号链接 → 允许但警告。**

**原因：**
- 数据文件本身是 symlink 可能指向敏感文件或导致写穿，安全风险不可接受，拒绝
- 父目录（如 `~/.config`）是 symlink 是 dotfiles 管理工具（stow/chezmoi 等）的常见合法用法，一刀切拒绝会破坏正常环境，降级为警告并提示确认目标可信
- 检查与使用之间存在 TOCTOU 窗口（检查后路径被替换），本地单用户威胁模型下可接受

```rust
fn validate_path_safety(path: &Path) -> Result<()> {
    if path.is_symlink() {
        bail!("OAuth accounts path is a symbolic link: {}", path.display());
    }
    if let Some(parent) = path.parent() {
        if parent.is_symlink() {
            warn!("parent directory is a symlink: {}; ensure the target is trusted", parent.display());
        }
    }
    Ok(())
}
```

**已知限制：** 临时文件、备份文件、锁文件与主文件同目录，未单独做 symlink 检查——攻击者需要已有目录写权限才能利用，与主文件威胁模型一致。

## 跨平台兼容性

### Windows

**文件锁：** `fs2::FileExt` 跨平台封装（Windows 上使用 `LockFileEx`），无需平台特定代码。

**原子写入：** Rust `std::fs::rename` 在 Windows 上使用 `MoveFileEx` + `MOVEFILE_REPLACE_EXISTING`，目标已存在时也能原子替换——"临时文件 + rename"方案在 Windows 上同样成立。

**文件权限：** Unix 上创建临时文件时设置 `0600`；Windows 无 POSIX 权限，依赖用户目录 ACL 默认隔离（未来可用 Windows ACL 显式加固，当前不实现）。

### macOS Keychain 集成（未来）

**当前不实现，未来可选：**

```rust
#[cfg(target_os = "macos")]
fn store_in_keychain(account: &str, token: &str) -> Result<()> {
    use security_framework::passwords::set_password;

    set_password(
        Some("llm-proxy"),  // service name
        account,
        token,
    )?;

    Ok(())
}
```

## 容器化场景

### Docker 卷挂载

**问题：** 容器重启后 `oauth_accounts.json` 丢失

**解决方案：**

1. **使用命名卷：**
```yaml
# docker-compose.yml
services:
  llm-proxy:
    volumes:
      - oauth-data:/home/user/.config/llm-proxy

volumes:
  oauth-data:
```

2. **使用绑定挂载：**
```yaml
services:
  llm-proxy:
    volumes:
      - ./oauth-accounts:/home/user/.config/llm-proxy
```

3. **使用环境变量传递 token（不推荐）：**
```bash
docker run -e OAUTH_TOKEN=... llm-proxy
```

### 临时文件系统

**问题：** `/tmp` 或 `tmpfs` 挂载点重启后丢失

**解决：** 使用持久化存储路径（`~/.config` 而非 `/tmp`）

## CI/CD 场景

### 非交互式登录

**问题：** CI 环境无法完成 OAuth 浏览器流程

**解决方案：**

1. **使用 API Key 而非 OAuth（推荐）：**
```toml
[providers.openai-payg]
api_key_env = "OPENAI_API_KEY"
```

2. **预置 OAuth token：**
```bash
# CI 脚本
echo '{"version":1,"openai":{"default":{...}}}' > ~/.config/llm-proxy/oauth_accounts.json
chmod 600 ~/.config/llm-proxy/oauth_accounts.json
```

3. **使用 Service Account：**
```bash
# Google Cloud Service Account
export GOOGLE_APPLICATION_CREDENTIALS=/path/to/service-account.json
```

## 多用户系统

### 共享系统

**问题：** 多个用户共享同一系统，各自需要独立的 OAuth 账号

**解决：** 每个用户有自己的 `~/.config/llm-proxy/` 目录

```
/home/user1/.config/llm-proxy/oauth_accounts.json
/home/user2/.config/llm-proxy/oauth_accounts.json
```

**权限：** 文件权限 `0600` 确保用户间隔离

### 系统级配置（未来）

**当前不支持，未来可选：**

```
/etc/llm-proxy/oauth_accounts.json  # 系统级配置
~/.config/llm-proxy/oauth_accounts.json  # 用户级配置（优先）
```

## 审计日志

**当前不实现审计日志。**

**原因：**
- 本地配置文件，风险较低
- 增加 I/O 开销
- 日志文件管理复杂

**未来如需审计：**
- 记录账号访问（读取 token）
- 记录账号修改（登录、刷新、删除）
- 使用系统日志（syslog, journald）

## 速率限制

**当前不实现登录速率限制。**

**原因：**
- 本地操作，非网络服务
- OAuth 提供商已有速率限制
- 增加复杂性

**未来如需限制：**
- 限制单位时间内的登录尝试次数
- 防止暴力破解（虽然风险很低）

## 时间相关问题

### 时区处理

**所有时间戳使用 UTC（Unix timestamp）。**

**原因：**
- 避免时区转换错误
- 跨时区一致性
- 简化计算

**显示时转换为用户本地时区：**
```rust
fn format_expiry_time(expires_at_unix: i64) -> String {
    let utc = chrono::DateTime::from_timestamp(expires_at_unix, 0).unwrap();
    let local = utc.with_timezone(&chrono::Local);
    local.format("%Y-%m-%d %H:%M:%S %Z").to_string()
}
```

### 时钟偏移

**问题：** 系统时钟不准确导致 token 过期判断错误

**处理策略：**

`is_expired()` 严格判断（不含容差），原因：

- 真正危险的方向是**系统钟慢**（把已过期 token 当有效 → API 401），`+300` 容差对此毫无帮助，反而扩大误判窗口
- 系统钟快（把有效 token 判过期）的后果只是提前 refresh，无害且自愈
- 容差只属于"提前刷新"决策（未来自动刷新时使用），不属于"是否已过期"判断

**用户提示（时钟明显异常时）：**
```
Warning: System clock may be inaccurate. Token refresh may fail.
Please synchronize your system clock.
```

### 夏令时

**不影响 Unix timestamp。**

- Unix timestamp 始终是 UTC
- 夏令时只影响显示时间
- 无需特殊处理

## 网络文件系统

### NFS/SMB 场景

**问题：** 网络文件系统上的文件锁可能不可靠

**处理策略：**

1. **使用文件锁（flock）：**
   - NFS v3+ 支持 flock
   - SMB/CIFS 支持文件锁

2. **锁失败即报错，不降级：**
   - 无锁的"纯原子写入"降级会失去并发写互斥（丢更新），此时"读不加锁"和"并发写防护"两个承诺同时失效
   - 因此锁获取失败直接向用户报错，提示将配置目录迁移到本地文件系统，绝不静默降级
   - 检测方式：`fs2::lock_exclusive()` 在 NFS 不支持时返回 `ENOLCK` 或 `EOPNOTSUPP` 错误

### 延迟写入

**问题：** 网络文件系统写入延迟导致数据不一致

**解决：**
- 写入后调用 `fsync` 确保数据落盘
- 验证写入结果

```rust
fn write_and_sync(path: &Path, data: &[u8]) -> Result<()> {
    let mut file = std::fs::File::create(path)?;
    file.write_all(data)?;
    file.sync_all()?;  // 确保数据写入磁盘
    Ok(())
}
```

## 远程访问场景

### SSH 远程会话

**问题：** SSH 会话中无法打开浏览器完成 OAuth

**解决方案：**

1. **使用设备授权流程（Device Authorization Grant）：**
```rust
// 显示设备码和验证 URL
println!("To authorize, visit: https://...");
println!("Enter code: XXXX-YYYY");
```

2. **使用预置 token：**
```bash
# 在本地机器登录，然后复制 token 到远程机器
scp ~/.config/llm-proxy/oauth_accounts.json remote:~/.config/llm-proxy/
```

3. **使用 API Key 替代 OAuth：**
```toml
[providers.openai-payg]
api_key_env = "OPENAI_API_KEY"
```

### 远程桌面（RDP/VNC）

**问题：** 远程桌面中浏览器可能无法正常工作

**解决：**
- 使用设备授权流程
- 或在本地机器登录后同步配置

## 文件编码与格式

### UTF-8 编码

**强制使用 UTF-8 编码（无 BOM）。**

**原因：**
- JSON 标准要求 UTF-8
- 跨平台一致性
- 避免 BOM 导致的解析错误

```rust
fn write_oauth_accounts_utf8(path: &Path, data: &OAuthAccounts) -> Result<()> {
    let json = serde_json::to_string_pretty(data)?;
    // 确保不包含 BOM
    std::fs::write(path, json.as_bytes())?;
    Ok(())
}
```

### 行尾符

**使用 LF（`\n`）作为行尾符。**

**原因：**
- Unix 标准
- Git 友好
- 跨平台兼容

```rust
fn write_oauth_accounts_lf(path: &Path, data: &OAuthAccounts) -> Result<()> {
    let json = serde_json::to_string_pretty(data)?;
    // serde_json 默认使用 LF
    std::fs::write(path, json.as_bytes())?;
    Ok(())
}
```

### JSON 格式化

**使用 pretty-print（2 空格缩进）。**

**原因：**
- 人类可读
- 便于手动编辑
- Git diff 友好

```rust
let json = serde_json::to_string_pretty(data)?;
```

## 大文件处理

### 文件大小限制

**建议限制：< 1MB。**

**原因：**
- 每个账号约 1-2KB
- 100 个账号约 100-200KB
- 超过 1MB 说明账号数量异常

**警告机制：**
```rust
fn check_file_size(path: &Path) -> Result<()> {
    let metadata = std::fs::metadata(path)?;
    let size = metadata.len();
    
    if size > 1_000_000 {
        warn!(
            "OAuth accounts file is large: {} bytes. \
             Consider removing unused accounts.",
            size
        );
    }
    
    Ok(())
}
```

### 内存使用

**全量加载到内存（文件小，可接受）。**

**当前实现：**
```rust
fn load_oauth_accounts(path: &Path) -> Result<OAuthAccounts> {
    let content = std::fs::read_to_string(path)?;
    let accounts: OAuthAccounts = serde_json::from_str(&content)?;
    Ok(accounts)
}
```

**未来如需优化（文件 > 10MB）：**
- 使用流式解析（`serde_json::from_reader`）
- 按需加载账号

## 文件系统配额

### 磁盘空间不足

**问题：** 磁盘满导致写入失败

**处理：** 见"写入失败处理"章节

**预防措施：**
```rust
fn check_disk_space(path: &Path) -> Result<()> {
    // 检查可用空间（至少 1MB）
    // ...
    
    if available_space < 1_000_000 {
        warn!(
            "Low disk space: {} bytes available. \
             OAuth accounts write may fail.",
            available_space
        );
    }
    
    Ok(())
}
```

### inode 耗尽

**问题：** inode 用尽导致无法创建文件

**处理：**
- 清理旧备份文件
- 提示用户清理磁盘

## 电源故障与崩溃恢复

### 写入中断

**问题：** 写入过程中断电/崩溃导致文件损坏

**解决：** 原子写入（见"写入失败处理"章节）

**额外保护：**
```rust
fn write_oauth_accounts_crash_safe(path: &Path, data: &OAuthAccounts) -> Result<()> {
    // 1. 写入临时文件
    let temp_path = path.with_extension("tmp");
    write_oauth_accounts_atomic(&temp_path, data)?;
    
    // 2. 同步到磁盘
    sync_file(&temp_path)?;
    
    // 3. 原子重命名
    std::fs::rename(&temp_path, path)?;
    
    // 4. 同步目录（确保重命名持久化）
    sync_directory(path.parent().unwrap())?;
    
    Ok(())
}

fn sync_file(path: &Path) -> Result<()> {
    let file = std::fs::File::open(path)?;
    file.sync_all()?;
    Ok(())
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let dir = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY)
        .open(path)?;
    dir.sync_all()?;
    Ok(())
}
```

### 备份验证

**定期验证备份文件完整性：**

```rust
fn verify_backups(path: &Path) -> Result<()> {
    let backups = find_all_backups(path);
    
    for backup in backups {
        match load_oauth_accounts(&backup) {
            Ok(_) => {
                info!("Backup verified: {}", backup.display());
            }
            Err(e) => {
                warn!("Backup corrupted: {} - {}", backup.display(), e);
            }
        }
    }
    
    Ok(())
}
```

## 数据完整性

### 校验和（未来）

**当前不实现，未来可选。**

**目的：** 检测文件损坏

```rust
// 未来实现
fn calculate_checksum(data: &OAuthAccounts) -> String {
    use sha2::{Sha256, Digest};
    let json = serde_json::to_string(data).unwrap();
    let mut hasher = Sha256::new();
    hasher.update(json.as_bytes());
    format!("{:x}", hasher.finalize())
}

// 在 OAuthAccounts 中添加
pub struct OAuthAccounts {
    pub version: u32,
    pub checksum: Option<String>,  // 未来添加
    // ...
}
```

### 版本控制集成

**建议：将 `oauth_accounts.json` 加入 `.gitignore`。**

**原因：**
- 包含敏感 token
- 用户特定配置
- 不应提交到版本控制

```gitignore
# .gitignore
.config/llm-proxy/oauth_accounts.json
.config/llm-proxy/oauth_accounts.json.bak.*
.config/llm-proxy/data.lock
```

## 特殊路径场景

### 路径长度限制

**问题：** Windows 路径长度限制（260 字符）

**解决：**
- 使用短路径（`~/.config/llm-proxy/`）
- 避免深层嵌套

```rust
fn validate_path_length(path: &Path) -> Result<()> {
    let path_str = path.to_str().unwrap_or("");
    
    #[cfg(windows)]
    if path_str.len() > 240 {  // 留 20 字符余量
        bail!(
            "Path too long for Windows: {} characters. \
             Maximum: 260 characters.",
            path_str.len()
        );
    }
    
    Ok(())
}
```

### 特殊字符路径

**问题：** 路径中包含空格、中文等特殊字符

**解决：**
- Rust 的 `Path` 类型原生支持 Unicode
- 使用 `path.display()` 显示路径
- 避免硬编码路径分隔符

```rust
// 正确
let path = dirs::home_dir().unwrap().join(".config/llm-proxy");

// 错误
let path = Path::new("~/config/llm-proxy");  // 不支持 ~ 展开
```

### 只读文件系统

**问题：** 配置文件位于只读文件系统

**处理：**
```rust
fn check_writable(path: &Path) -> Result<()> {
    let metadata = std::fs::metadata(path)?;
    
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let mode = metadata.mode();
        if mode & 0o200 == 0 {
            bail!(
                "OAuth accounts file is read-only: {}\n\
                 Fix with: chmod u+w {}",
                path.display(), path.display()
            );
        }
    }
    
    Ok(())
}
```

## 扩展性

### 新增 OAuth 类型

1. 在 Rust 代码中添加新的 struct 定义
2. 在 `OAuthAccounts` 中添加新的类型分组字段（`Option`/`HashMap` + `serde(default)`）
3. 更新验证逻辑
4. 注意：同版本新增分组后，旧二进制写回会静默丢弃该分组（接受的向前不兼容行为，见"迁移策略"）

### 新增字段

1. 字段必须标记为可选（`Option` + `serde(default)`，或提供默认值）
2. 更新验证逻辑
3. 同版本内新增，不需要版本升级；但旧二进制写回时该字段丢失（接受的向前不兼容行为）

### 未来版本

- 版本 2：可能添加加密支持
- 版本 3：可能添加账号元数据（创建时间、最后使用时间）
- 版本 4：可能添加账号分组/标签

## 完整示例

```json
{
  "version": 1,
  "antigravity": {
    "default": {
      "account_label": "user@gmail.com",
      "project_id": "example-project-12345",
      "access_token": "ya29.a0EXAMPLE...",
      "refresh_token": "1//0EXAMPLE...",
      "expires_at_unix": 1785245247,
      "updated_at_unix": 1785226019
    },
    "work": {
      "account_label": "work@company.com",
      "project_id": "my-company-project",
      "access_token": "ya29.a0EXAMPLE...",
      "refresh_token": "1//0EXAMPLE...",
      "expires_at_unix": 1785245247,
      "updated_at_unix": 1785226019
    }
  },
  "openai": {
    "personal": {
      "account_label": "user@gmail.com",
      "access_token": "eyJhbGciOiJSUzI1NiIsImtpZCI6IEXAMPLE...",
      "refresh_token": "rt.1.EXAMPLE...",
      "expires_at_unix": 1786090020,
      "updated_at_unix": 1785226020
    }
  }
}
```

## 对应 config.toml 配置

```toml
[providers.antigravity.auth]
type = "antigravity_oauth"
account = "default"  # 在 antigravity 分组中查找

[providers.openai-subscription.auth]
type = "openai_oauth"
account = "personal"  # 在 openai 分组中查找
```

## 实现清单

- [x] 定义 Rust struct（`OAuthAccounts`, `AntigravityAccount`, `OpenaiAccount`）
- [x] 实现 JSON Schema 验证（`OAuthAccounts::validate()`）
- [x] 实现加载和保存逻辑（`load_oauth_accounts` / `save_oauth_accounts`）
- [x] 实现文件权限设置（Unix 0600，临时文件创建即 0600）
- [x] 实现备份策略（写入前备份、纳秒时间戳、保留 3 份、清理失败容忍）
- [x] 实现原子写入（临时文件 + fsync + rename + 目录 fsync）
- [x] 实现锁内"读-改-写"事务（`with_locked_accounts`，login/logout/refresh 全部走锁内事务）
- [x] 实现 refresh 串行化（整个刷新事务持锁 + 60 秒跳过窗口）
- [x] 实现损坏恢复（遍历全部备份 + 写回主文件 + 过期提示）
- [x] 实现符号链接防护（文件拒绝、父目录警告）
- [x] 实现启动时校验（`validate_oauth_on_startup`，缺失/损坏降级警告不阻塞启动）
- [x] 实现错误消息格式化（含修复命令提示）
- [x] 更新 config.toml 解析逻辑（按 AuthConfig 类型定位分组）
- [x] 编写单元测试（备份/恢复/锁事务/符号链接/过期/skip 语义）
- [ ] 编写集成测试（真实 OAuth 流程，人工执行）

**明确不做：** 版本迁移逻辑（v0 不存在）、未知分组保留、加密存储、审计日志、登录速率限制、账号模板、导入导出命令、Windows ACL、macOS Keychain。
