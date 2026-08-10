# 订阅额度查询功能设计方案

## 1. 背景和目标

### 1.1 背景
当前 `llm-proxy status` 可以显示 OAuth token 过期时间、provider 可达性、cooldown 状态，但无法显示订阅服务的剩余额度。用户希望能够在本地代理层面了解各订阅服务的可用额度，避免在额度耗尽时才发现。

### 1.2 目标
- 支持查询订阅类 provider 的额度信息（ChatGPT Subscription、Antigravity 等）
- 提供 CLI 命令和 TUI 界面两种访问方式
- 缓存额度信息，避免频繁调用上游 API
- 在额度低于阈值时发出警告

## 2. 当前订阅类 Provider 分析

> **调研结果**: 详见内部调研文档

### 2.1 ChatGPT Subscription (openai-sub)
- **认证方式**: OAuth (ChatGPT account)
- **后端**: `https://chatgpt.com/backend-api/codex/`
- **额度 API**: ✅ **已发现**
  - `GET /backend-api/wham/usage` — 实时限流状态
  - `GET /backend-api/wham/profiles/me` — 历史使用统计
  - 返回 `rate_limit.primary_window.used_percent`、`reset_after_seconds` 等
  - Codex CLI 源码已实现（`3rdparty/codex/codex-rs/backend-client/src/client/rate_limit_resets.rs`）

### 2.2 Antigravity (google-antigravity)
- **认证方式**: Google OAuth2
- **后端**: `https://cloudcode-pa.googleapis.com/v1internal`
- **额度 API**: ✅ **已发现**
  - `POST /v1internal:loadCodeAssist` — 返回额度信息
  - 响应包含 `paidTier.availableCredits` 数组
  - 关键字段：`creditAmount`（当前额度）、`minimumCreditAmountForUsage`（最低阈值）
  - CLIProxyAPI 已实现（`3rdparty/CLIProxyAPI/internal/runtime/executor/antigravity_executor_credits.go`）

### 2.3 MiMo Token Plan (mimo-token-plan)
- **认证方式**: API Key
- **后端**: `token-plan-{cn,sgp,ams}.xiaomimimo.com`
- **额度 API**: ❌ **未发现**
  - 官方文档未提及额度查询功能
  - 替代方案：本地累计（基于配置的初始额度）

### 2.4 Codex Subscription
- **说明**: 与 ChatGPT Subscription 使用相同后端
- **额度 API**: ✅ **已发现**（同 ChatGPT Subscription）

## 3. API 调研计划

### 3.1 ChatGPT Subscription 额度 API 调研

**调研方法**:
1. 检查 Codex CLI 源码是否有额度相关 API 调用
2. 使用 OAuth token 尝试常见的额度端点
3. 检查 JWT token 中是否包含额度信息
4. 搜索 OpenAI 开发者论坛/文档

**可能的端点**（待验证）:
- `GET /backend-api/codex/usage`
- `GET /backend-api/codex/quota`
- `GET /backend-api/codex/account`
- JWT payload 中的额度字段

**验证步骤**:
```bash
# 1. 获取 OAuth token
llm-proxy provider login openai-sub

# 2. 尝试常见端点
curl -H "Authorization: Bearer $TOKEN" \
  https://chatgpt.com/backend-api/codex/usage

# 3. 检查 JWT payload
echo $TOKEN | cut -d. -f2 | base64 -d | jq .
```

### 3.2 Antigravity 额度 API 调研

**调研方法**:
1. 检查 Antigravity IDE/CLI 源码是否有额度相关 API 调用
2. 使用 OAuth token 尝试常见的额度端点
3. 检查 `loadCodeAssist` 响应是否包含额度信息
4. 搜索 Google Cloud 文档

**可能的端点**（待验证）:
- `POST /v1internal:getQuota`
- `POST /v1internal:getUsage`
- `POST /v1internal:loadCodeAssist` (可能包含额度信息)
- `POST /v1internal:fetchAvailableModels` (可能包含额度限制)

**验证步骤**:
```bash
# 1. 获取 OAuth token
llm-proxy provider login google-antigravity

# 2. 尝试 loadCodeAssist
curl -X POST -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"project": "$PROJECT_ID"}' \
  https://cloudcode-pa.googleapis.com/v1internal:loadCodeAssist

# 3. 检查响应中的额度信息
```

## 4. 架构设计

### 4.1 核心组件

```
┌─────────────────────────────────────────┐
│         Quota Manager                   │
│  - 缓存管理 (QueryCache 扩展)           │
│  - 阈值检查和警告                       │
│  - 统一的额度查询接口                   │
└─────────────────────────────────────────┘
           │
           ├─► ChatGPT Quota Client
           │   - OAuth token 管理
           │   - API 调用和解析
           │   - 额度信息标准化
           │
           └─► Antigravity Quota Client
               - OAuth token 管理
               - API 调用和解析
               - 额度信息标准化
```

### 4.2 数据结构

```rust
/// 统一的额度信息表示
pub struct QuotaInfo {
    pub provider_id: String,
    pub plan_type: Option<String>,  // e.g., "plus", "pro", "free"
    pub usage: QuotaUsage,
    pub limits: QuotaLimits,
    pub reset_at: Option<DateTime<Utc>>,
    pub fetched_at: DateTime<Utc>,
}

pub struct QuotaUsage {
    pub requests_used: Option<u64>,
    pub tokens_used: Option<u64>,
    pub dollars_used: Option<f64>,
}

pub struct QuotaLimits {
    pub requests_limit: Option<u64>,
    pub tokens_limit: Option<u64>,
    pub dollars_limit: Option<f64>,
    pub window: Option<String>,  // e.g., "daily", "weekly", "monthly"
}

/// Provider 额度查询 trait
#[async_trait]
pub trait QuotaClient: Send + Sync {
    async fn fetch_quota(&self) -> Result<QuotaInfo>;
    fn provider_id(&self) -> &str;
}
```

### 4.3 缓存策略

**缓存位置**: `state_dir/quota-cache.json`

**缓存结构**:
```json
{
  "openai-sub": {
    "quota_info": { ... },
    "fetched_at": "2026-08-04T12:00:00Z",
    "expires_at": "2026-08-04T13:00:00Z"
  },
  "google-antigravity": {
    "quota_info": { ... },
    "fetched_at": "2026-08-04T12:00:00Z",
    "expires_at": "2026-08-04T13:00:00Z"
  }
}
```

**缓存策略**:
- **TTL**: 1 小时（可配置）
- **强制刷新**: `--refresh` 参数
- **后台刷新**: Server 启动时 + 每 30 分钟（可选）

### 4.4 模块归属

```
src/
├── quota/
│   ├── mod.rs              # Quota Manager 主模块
│   ├── cache.rs            # 缓存管理
│   ├── types.rs            # 数据结构定义
│   ├── chatgpt.rs          # ChatGPT Subscription 客户端
│   └── antigravity.rs      # Antigravity 客户端
```

## 5. 用户界面设计

### 5.1 CLI 命令

**方案 A: 扩展 `status` 命令**（推荐）
```bash
# 显示额度信息（从缓存读取）
llm-proxy status --probe

# 输出示例：
# Provider: openai-sub
#   Plan: Plus
#   Usage: 45/100 requests (45%)
#   Reset: 2026-08-05 00:00:00 UTC
#   Status: ✓ 正常
```

**方案 B: 新增 `quota` 命令**
```bash
# 查询所有订阅类 provider 的额度
llm-proxy quota

# 查询特定 provider
llm-proxy quota openai-sub

# 强制刷新
llm-proxy quota --refresh

# 输出示例：
# Provider: openai-sub
#   Plan: Plus
#   Requests: 45/100 (45%)
#   Tokens: 125,000/500,000 (25%)
#   Reset: 2026-08-05 00:00:00 UTC
#   Fetched: 2026-08-04 12:00:00 UTC
#
# Provider: google-antigravity
#   Plan: Standard
#   Requests: 120/200 (60%)
#   Reset: 2026-08-05 00:00:00 UTC
#   Fetched: 2026-08-04 12:00:00 UTC
```

### 5.2 TUI 集成

**Provider Management Panel 扩展**:
- 在 provider 详情中显示额度信息
- 使用颜色标识额度状态：
  - 🟢 绿色: < 50%
  - 🟡 黄色: 50-80%
  - 🔴 红色: > 80%

**额度监控面板**（可选）:
- 新增 TUI 页面显示所有订阅类 provider 的额度
- 支持排序和过滤
- 支持手动刷新

### 5.3 警告机制

**警告阈值**（可配置）:
```toml
[quota]
warning_threshold = 0.8  # 80% 时警告
critical_threshold = 0.95  # 95% 时严重警告
check_interval_minutes = 60  # 检查间隔
```

**警告方式**:
- CLI 输出中显示警告图标
- TUI 中高亮显示
- 日志记录（可选）

## 6. 实现计划

### Phase 1: API 调研和验证（1-2 天）
1. 调研 ChatGPT Subscription 额度 API
2. 调研 Antigravity 额度 API
3. 验证 API 可用性和返回格式
4. 更新 provider 调研文档

### Phase 2: 核心实现（2-3 天）
1. 实现 `QuotaInfo` 数据结构和 trait
2. 实现缓存管理（基于 QueryCache）
3. 实现 ChatGPT Quota Client
4. 实现 Antigravity Quota Client
5. 实现 Quota Manager 统一接口

### Phase 3: CLI 集成（1-2 天）
1. 扩展 `status` 命令显示额度信息
2. 或实现独立的 `quota` 命令
3. 添加 `--refresh` 参数支持
4. 添加警告阈值配置

### Phase 4: TUI 集成（1-2 天）
1. 在 Provider Management Panel 显示额度信息
2. 实现颜色标识和状态图标
3. 可选：实现额度监控面板

### Phase 5: 测试和文档（1 天）
1. 单元测试（缓存、API 客户端）
2. 集成测试（端到端额度查询）
3. 更新用户文档
4. 更新 provider 调研文档

## 7. 风险和注意事项

### 7.1 API 可用性风险
- **风险**: 订阅类 provider 可能不提供公开的额度查询 API
- **缓解**: 优先调研，如果 API 不存在则降低优先级或寻找替代方案

### 7.2 API 稳定性风险
- **风险**: 私有 API 可能随时变更
- **缓解**: 
  - 使用生产端点（非 staging）
  - 添加详细的错误处理
  - 文档标注 API 稳定性级别

### 7.3 认证风险
- **风险**: OAuth token 过期或刷新失败
- **缓解**: 
  - 复用现有的 OAuth token 管理机制
  - 优雅降级（token 不可用时显示"不可用"）

### 7.4 性能风险
- **风险**: 频繁调用额度 API 影响性能
- **缓解**: 
  - 强制缓存（默认 1 小时 TTL）
  - 后台异步刷新
  - 批量查询优化

## 8. 验收标准

- [ ] ChatGPT Subscription 额度查询功能可用（如果 API 存在）
- [ ] Antigravity 额度查询功能可用（如果 API 存在）
- [ ] 额度信息缓存正常工作
- [ ] CLI 命令显示额度信息
- [ ] TUI 界面显示额度信息
- [ ] 警告阈值配置生效
- [ ] 所有测试通过
- [ ] 文档更新完成

## 9. 后续扩展

### 9.1 支持更多 Provider
- Kimi Code（如果有额度 API）
- MiMo（如果有额度 API）
- 其他订阅类 provider

### 9.2 额度预测
- 基于历史使用数据预测额度耗尽时间
- 提供使用建议

### 9.3 自动降级
- 额度耗尽时自动切换到备用 provider
- 与 fallback 机制集成

## 10. 决策记录

### 10.1 CLI 命令设计
**决策**: 优先扩展 `status` 命令，而非新增 `quota` 命令
**理由**: 
- `status` 已经是查看 provider 状态的主要入口
- 避免命令过多增加用户学习成本
- 额度信息是 provider 状态的一部分

### 10.2 缓存策略
**决策**: 使用文件系统缓存（`state_dir/quota-cache.json`）
**理由**:
- 与现有 QueryCache 机制一致
- Server 和 CLI 可以共享缓存
- 重启后缓存仍然有效

### 10.3 额度信息标准化
**决策**: 定义统一的 `QuotaInfo` 结构
**理由**:
- 不同 provider 的额度 API 返回格式不同
- 统一结构简化 UI 层处理
- 便于后续扩展新 provider
