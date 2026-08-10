# Status 命令验证 SOP

本文档是 `llm-proxy status` 命令的完整验证流程，覆盖 16 种场景组合（4 维 × 2 值）。

---

## 📋 验证矩阵

### 4 个维度

| 维度 | 值 | 说明 |
|------|-----|------|
| **模式** | 本地 / 远程 | 本地：config 有 providers/models；远程：config 无 providers/models |
| **Server** | 可达 / 不可达 | 可达：server 在监听；不可达：server 未启动 |
| **缓存** | 有 / 无 | 有：`status-cache.json` 存在且非空；无：缓存不存在或为空 |
| **--probe** | 有 / 无 | 有：触发实际探测；无：只读缓存/实时数据 |

### 16 种场景

| # | 模式 | Server | 缓存 | --probe | 预期行为 | 提示 |
|---|------|--------|------|---------|---------|------|
| 1 | 本地 | 可达 | 有 | 无 | Server 实时 + 写缓存 | `ℹ` |
| 2 | 本地 | 可达 | 有 | 有 | Server probe + 写缓存 | `✓` |
| 3 | 本地 | 可达 | 无 | 无 | Server 实时 + 写缓存 | `ℹ` |
| 4 | 本地 | 可达 | 无 | 有 | Server probe + 写缓存 | `✓` |
| 5 | 本地 | 不可达 | 有 | 无 | 读缓存 | `⚠` |
| 6 | 本地 | 不可达 | 有 | 有 | CLI probe → 成功写缓存 / 失败回退缓存 | `✓` / `⚠` |
| 7 | 本地 | 不可达 | 无 | 无 | 无数据 | `✗` |
| 8 | 本地 | 不可达 | 无 | 有 | CLI probe → 成功写缓存 / 失败无数据 | `✓` / `✗` |
| 9 | 远程 | 可达 | 有 | 无 | Server 实时 + 写缓存 | `ℹ` |
| 10 | 远程 | 可达 | 有 | 有 | Server probe + 写缓存 | `✓` |
| 11 | 远程 | 可达 | 无 | 无 | Server 实时 + 写缓存 | `ℹ` |
| 12 | 远程 | 可达 | 无 | 有 | Server probe + 写缓存 | `✓` |
| 13 | 远程 | 不可达 | 有 | 无 | 读缓存 | `⚠` |
| 14 | 远程 | 不可达 | 有 | 有 | 尝试 probe 失败 → 回退缓存 | `⚠` |
| 15 | 远程 | 不可达 | 无 | 无 | 无数据 | `✗` |
| 16 | 远程 | 不可达 | 无 | 有 | 尝试 probe 失败 | `✗` |

**简化观察**：
- Server 可达时（#1-4, #9-12）：缓存维度不影响行为（都从 server 拿实时 + 写缓存）
- #1/#3, #2/#4, #9/#11, #10/#12 行为相同
- 缓存维度只在 server 不可达时影响（有缓存 → 回退；无缓存 → 无数据）

---

## 🔧 模拟方法

### 1. 本地模式 vs 远程模式

**本地模式**：使用完整的 config.toml（有 providers/models）
```bash
# 使用默认 config（~/.config/llm-proxy/config.toml）
llm-proxy status
```

**远程模式**：创建极简 config.toml（只有 `[server]` 段）
```bash
cat > /tmp/remote-config.toml <<EOF
[server]
listen = "127.0.0.1:8989"
EOF

llm-proxy --config /tmp/remote-config.toml status
```

### 2. Server 可达 vs 不可达

**Server 可达**：启动 server
```bash
llm-proxy serve --foreground &
```

**Server 不可达**：关闭 server
```bash
llm-proxy shutdown
```

### 3. 缓存有 vs 无

**缓存有**：确保 `status-cache.json` 存在且非空
```bash
# 检查缓存
cat ~/.local/state/llm-proxy/status-cache.json

# 如果为空，先运行一次 status --probe 生成缓存
llm-proxy status --probe
```

**缓存无**：清空或删除缓存文件
```bash
# 清空缓存
echo '{"probes":{},"dynamic_models":{}}' > ~/.local/state/llm-proxy/status-cache.json

# 或删除缓存
rm ~/.local/state/llm-proxy/status-cache.json
```

### 4. 完整隔离（推荐）

为避免与运行中 server 冲突，使用 `LLM_PROXY_STATE_DIR` 环境变量隔离所有状态文件：

```bash
# 创建隔离的 state 目录
mkdir -p /tmp/test-state

# 使用隔离的 state 目录运行命令
LLM_PROXY_STATE_DIR=/tmp/test-state llm-proxy status

# 远程模式 + 隔离
LLM_PROXY_STATE_DIR=/tmp/remote-state llm-proxy --config /tmp/remote-config.toml status
```

**隔离范围**（参见 design §15.1）：
- `status-cache.json` — status 缓存
- `cooldowns.json` — cooldown 状态
- `llm-proxy.pid` — PID 文件
- `llm-proxy.sock` — UDS socket
- `llm-proxy.log` — 日志文件

**注意**：OAuth 存储（`oauth-accounts.db`）位于 config 目录，不受 `LLM_PROXY_STATE_DIR` 影响。

---

## 📝 验证步骤

### 准备阶段

1. **确认 server 状态**
   ```bash
   ps aux | grep llm-proxy
   ```

2. **确认缓存状态**
   ```bash
   cat ~/.local/state/llm-proxy/status-cache.json
   ```

3. **准备两种 config**
   ```bash
   # 本地模式 config（默认）
   # ~/.config/llm-proxy/config.toml（已有 providers/models）
   
   # 远程模式 config
   cat > /tmp/remote-config.toml <<EOF
   [server]
   listen = "127.0.0.1:8989"
   EOF
   ```

### 执行阶段

按矩阵逐项验证（建议顺序）：

#### 组 1：本地 + Server 可达（#1-4）

```bash
# 启动 server
llm-proxy serve --foreground &
sleep 2

# 场景 1：本地 + 可达 + 有缓存 + 无 probe
llm-proxy status | grep -E "ℹ|✓|⚠|✗|数据来自"
# 预期：ℹ 数据来自 Server（实时）

# 场景 2：本地 + 可达 + 有缓存 + 有 probe
llm-proxy status --probe | grep -E "ℹ|✓|⚠|✗|探测|已请求"
# 预期：✓ 已请求 Server 探测非活跃 provider

# 场景 3-4：清空缓存后重试
echo '{"probes":{},"dynamic_models":{}}' > ~/.local/state/llm-proxy/status-cache.json
llm-proxy status | grep -E "ℹ|✓|⚠|✗|数据来自"
# 预期：ℹ 数据来自 Server（实时）

llm-proxy status --probe | grep -E "ℹ|✓|⚠|✗|探测|已请求"
# 预期：✓ 已请求 Server 探测非活跃 provider
```

#### 组 2：本地 + Server 不可达（#5-8）

```bash
# 关闭 server
llm-proxy shutdown
sleep 1

# 场景 5：本地 + 不可达 + 有缓存 + 无 probe
llm-proxy status | grep -E "ℹ|✓|⚠|✗|数据来自|缓存"
# 预期：⚠ 数据来自本地缓存（Server 未启动，可能过时）

# 场景 6：本地 + 不可达 + 有缓存 + 有 probe
llm-proxy status --probe | grep -E "ℹ|✓|⚠|✗|探测|已执行|回退"
# 预期：✓ 已执行本地探测（或 ⚠ 回退到缓存）

# 场景 7：清空缓存
echo '{"probes":{},"dynamic_models":{}}' > ~/.local/state/llm-proxy/status-cache.json
llm-proxy status | grep -E "ℹ|✓|⚠|✗|数据来自|缓存"
# 预期：✗ Server 未启动，无缓存数据

# 场景 8：清空缓存 + probe
llm-proxy status --probe | grep -E "ℹ|✓|⚠|✗|探测|已执行"
# 预期：✓ 已执行本地探测（或 ✗ 本地探测失败）
```

#### 组 3：远程 + Server 可达（#9-12）

```bash
# 启动 server
llm-proxy serve --foreground &
sleep 2

# 场景 9-12：使用远程 config
# 行为与场景 1-4 相同（Server 可达时缓存维度不影响）
llm-proxy --config /tmp/remote-config.toml status | grep -E "ℹ|✓|⚠|✗|数据来自"
# 预期：ℹ 数据来自 Server（实时）

llm-proxy --config /tmp/remote-config.toml status --probe | grep -E "ℹ|✓|⚠|✗|探测|已请求"
# 预期：✓ 已请求 Server 探测非活跃 provider
```

#### 组 4：远程 + Server 不可达（#13-16）

```bash
# 关闭 server
llm-proxy shutdown
sleep 1

# 场景 13：远程 + 不可达 + 有缓存 + 无 probe
llm-proxy --config /tmp/remote-config.toml status | grep -E "ℹ|✓|⚠|✗|数据来自|缓存"
# 预期：⚠ 数据来自本地缓存（Server 未启动，可能过时）

# 场景 14：远程 + 不可达 + 有缓存 + 有 probe
llm-proxy --config /tmp/remote-config.toml status --probe | grep -E "ℹ|✓|⚠|✗|探测|远程|回退"
# 预期：✗ 远程模式无法本地探测 + ⚠ 回退到本地缓存

# 场景 15：清空缓存
echo '{"probes":{},"dynamic_models":{}}' > ~/.local/state/llm-proxy/status-cache.json
llm-proxy --config /tmp/remote-config.toml status | grep -E "ℹ|✓|⚠|✗|数据来自|缓存"
# 预期：✗ Server 未启动，无缓存数据

# 场景 16：清空缓存 + probe
llm-proxy --config /tmp/remote-config.toml status --probe | grep -E "ℹ|✓|⚠|✗|探测|远程"
# 预期：✗ 远程模式无法本地探测 + ✗ 无缓存数据
```

### 通过标准

- ✅ 16 种情况全部符合预期
- ✅ 数据来源提示正确（ℹ/✓/⚠/✗）
- ✅ 缓存更新时机正确（Server 可达时每次更新）
- ✅ 远程模式判定正确（无 providers/models 时判定为远程）

---

## 🐛 已知问题与修复

### status --probe 超时问题（已修复）

**问题**：`status --probe` 失败——"Server 探活失败: failed to reach admin API"

**根因**：admin_client 的超时只有 2 秒，但 `/admin/status/probe` 需要 42 秒（探测多个 provider）

**修复**：为 status_probe 创建独立的 reqwest client，超时设为 60 秒

**Commit**: d7fb64f

---

## 📚 相关文档

- [CLI 使用指南](../user_guide/cli-guide.md) — status 命令用法
- [核心架构设计](../../spec.md) — §12 Status 命令设计决策
- [E2E 验证 SOP](launch-and-access-verification.md) — 功能发布前的完整验证流程
