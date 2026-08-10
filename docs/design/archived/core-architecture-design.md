# Core 架构设计：分层模型与多进程协调

> 状态：✅ 已实现（2026-08-03）— 分层模型、单一写者、UDS 委托、读/写分离、fallback 语义等核心决策已全部落地（Server Delegation 阶段 0-5，见 ROADMAP）；本文档保留作为决策记录。注：§"未来（可选）远程管理 = token 鉴权"仍未实现
> 创建：2026-07-30
> 最后更新：2026-07-30

---

## 1. 设计目标

1. **业务逻辑与 I/O 彻底分离**：Core 层持有内存状态，不碰 stdin/stdout/网络监听
2. **单一写者原则**：任何时刻只有一个进程负责写盘，避免状态不一致
3. **多进程安全**：多个 CLI 并发、CLI + Server 并发，均不丢数据、不冲突
4. **支持本地和远程两种部署模式**：CLI 和 Server 可在同一机器或不同机器
5. **统一命令模型**：一套 `AdminCommand` 定义，两种执行路径（委托 / 独立），结果一致

---

## 2. 分层架构

```
┌─────────────────────────────────────────────────────┐
│                    接入层 (Thin)                     │
│                                                     │
│   CLI (短命令)  │  TUI (交互)  │  未来外部应用       │
│                                                     │
│   职责：                                              │
│   - 解析用户输入                                      │
│   - 格式化输出                                        │
│   - 交互确认（y/N）                                   │
│   - 检测 Server 并选择执行路径                         │
├─────────────────────────────────────────────────────┤
│                  Core 层 (业务逻辑)                   │
│                                                     │
│   ┌─────────────────────────────────────────────┐   │
│   │  CoreState                                   │   │
│   │                                              │   │
│   │  - Config (providers, models, server...)     │   │
│   │  - UsageStore (内存缓存 + 持久化控制)          │   │
│   │  - OAuth state                               │   │
│   │  - Cooldown state                            │   │
│   │                                              │   │
│   │  方法：                                       │   │
│   │  - add_provider() / remove_provider()        │   │
│   │  - add_model() / remove_model()              │   │
│   │  - record_usage() / query_usage()            │   │
│   │  - save_config() / save_usage()              │   │
│   │                                              │   │
│   │  原则：                                       │   │
│   │  - 纯内存操作 + 受控持久化                     │   │
│   │  - 不碰 stdin/stdout                          │   │
│   │  - 不监听网络端口                              │   │
│   └─────────────────────────────────────────────┘   │
├─────────────────────────────────────────────────────┤
│                  持久化层 (Storage)                   │
│                                                     │
│   config.toml  │  usage.jsonl  │  usage.db          │
│   oauth_accounts.json │  llm-proxy.pid  │  ownership.lock  │
│                                                     │
│   只有 Core 层有权写入                                │
└─────────────────────────────────────────────────────┘
```

### 2.1 各层职责

| 层 | 做什么 | 不做什么 |
|----|--------|---------|
| 接入层 (CLI/TUI) | 解析参数、格式化输出、交互确认、检测 Server | 不直接读写 config 文件、不持有业务状态 |
| Core 层 | 业务逻辑、状态管理、持久化控制 | 不做用户交互、不监听网络 |
| 持久化层 | 存储数据 | 不包含业务逻辑 |

### 2.2 核心类型定义

```rust
/// 统一命令模型——接入层构造命令，Core 层执行
/// 注意：AdminCommand 只覆盖 server 需要远程委托的子集。
/// ProviderAdd/ModelAdd 等操作直接作为 CoreState 方法调用，不通过 AdminCommand 分发。
pub enum AdminCommand {
    // Provider 管理（server 委托）
    ProviderRemove { id: String, force: bool },

    // Usage 查询
    UsageQuery {
        period: Option<String>,
        provider: Option<String>,
        model: Option<String>,
        endpoint: Option<String>,
        view: UsageView,
    },

    // 状态查询
    Status,
    ConfigReload,
}

pub enum AdminResponse {
    Ok { message: String },
    UsageRecords(Vec<UsageRecord>),
    StatusInfo(StatusInfo),
}
    ClientConfigPayload(ClientConfigPayload),
    Error { code: ErrorCode, message: String },
}

/// Core 状态——持有所有业务数据
pub struct CoreState {
    config: Config,
    usage_store: UsageStore,
    config_path: PathBuf,
    state_dir: PathBuf,
}

impl CoreState {
    /// 从文件加载
    pub fn load(config_path: &Path) -> Result<Self> { ... }

    /// 执行命令并持久化
    pub fn apply(&mut self, cmd: AdminCommand) -> Result<AdminResponse> {
        let resp = self.execute(cmd)?;
        self.save()?;  // 统一落盘
        Ok(resp)
    }

    /// 纯业务逻辑（不触发保存，供 server 精细控制）
    fn execute(&mut self, cmd: AdminCommand) -> Result<AdminResponse> { ... }

    /// 持久化所有状态
    pub fn save(&self) -> Result<()> { ... }
}
```

---

## 3. 两种部署模式

### 3.1 模式 A：本地模式（Server + CLI 同机器）

```
┌─────────────────────────────────────────┐
│              同一台机器                   │
│                                         │
│  ┌─────────────┐   ┌─────────────────┐  │
│  │ CLI / TUI   │   │ Server 进程      │  │
│  │             │   │                 │  │
│  │ 检测 server │──→│ CoreState       │  │
│  │ 有则委托    │   │ Proxy HTTP      │  │
│  │ 无则独立    │   │ Admin API       │  │
│  └──────┬──────┘   └────────┬────────┘  │
│         │                   │           │
│         │  独立模式时        │  委托时    │
│         │  直接操作文件      │  通过 API  │
│         ↓                   ↓           │
│  ┌──────────────────────────────────┐   │
│  │         共享文件系统              │   │
│  │                                  │   │
│  │  config.toml                     │   │
│  │  ownership.lock ← 所有权锁       │   │
│  │  llm-proxy.pid                   │   │
│  │  usage.jsonl / usage.db          │   │
│  │  oauth_accounts.json             │   │
│  └──────────────────────────────────┘   │
└─────────────────────────────────────────┘
```

**特点：**
- CLI 和 Server 共享文件系统
- CLI 可以检测 Server 是否在跑（PID file + admin ping）
- Server 不在时 CLI 可退化为独立模式（直接操作文件）
- 并发控制：文件锁（多 CLI）+ Server 内部 Mutex（Server 在跑时）

### 3.2 模式 B：远程模式（Server 在远端，CLI 在本地）

```
┌──────────────────┐              ┌──────────────────┐
│   机器 A (本地)   │              │   机器 B (远程)   │
│                  │              │                  │
│  ┌────────────┐  │              │  ┌────────────┐  │
│  │ CLI / TUI  │  │   HTTP/HTTPS │  │ Server     │  │
│  │            │──┼─────────────→│  │            │  │
│  │ 本地 config│  │  HTTP 读接口 │  │ CoreState  │  │
│  │ 只有:       │  │              │  │ Proxy      │  │
│  │ server.listen│  │             │  │ Admin API  │  │
│  │（只读）    │  │              │  │            │  │
│  └────────────┘  │              │  └─────┬──────┘  │
│                  │              │        │         │
│  ❌ 无 provider  │              │  ┌─────┴──────┐  │
│  ❌ 无 model 配置│              │  │ 文件系统    │  │
│  ❌ 无 lock file │              │  │ config.toml │  │
│  ❌ 无 PID file  │              │  │ usage.*     │  │
│                  │              │  │ oauth_accounts.json │  │
│  ✅ 客户端配置    │              │  └────────────┘  │
│     (.codex/...) │              │                  │
└──────────────────┘              └──────────────────┘
```

**特点：**
- 本地 config.toml 只有 `server.listen`（HTTP 公开接口地址）
- 所有 provider/model/usage 数据在远程 Server
- CLI 通过 HTTP 公开接口与 Server 通信（**只读**：status、usage 查询）
- **写操作仅本机 UDS**，远程不可达
- Server 宕机时 CLI **无法退化为独立模式**（碰不到远程文件）

> **状态**：远程模式为**待实现**设计（未来方向）。当前实现只覆盖本地模式（CLI 和 Server 同机器，通过 UDS 通信）。设计决策（2026-08-09）：远程模式只支持读操作（status/query），写操作仅通过本机 UDS 管理通道。

### 3.3 配置差异

**本地模式 config.toml（完整）：**
```toml
[server]
listen = "127.0.0.1:8989"

[providers.deepseek]
api_key_env = "DEEPSEEK_API_KEY"
# ...

[models.deepseek-chat]
# ...
```

**远程模式 config.toml（极简）：**
```toml
[server]
listen = "http://192.168.1.100:8989"  # 远程 Server 的 HTTP 公开接口地址（待实现）

# 没有 providers、没有 models
# 所有配置在远程 Server 上
# 只支持读操作（status、usage 查询），写操作仅本机 UDS
```

---

## 4. CLI 执行路径决策

### 4.1 决策流程

```
CLI 启动
  │
  ├─ 检测到本地 Server（PID + UDS socket）？
  │   │
  │   ├─ 是 → 本地模式：委托写操作（UDS 管理通道），读操作走 HTTP
  │   │       │
  │   │       ├─ 连 Server 成功？
  │   │       │   ├─ 是 → 委托操作
  │   │       │   └─ 否 → 独立模式（直接操作文件）
  │   │       │
  │   │       └─ 有 fallback（本地文件可达）
  │   │
  │   └─ 否 → 本地模式
  │           │
  │           ├─ 检测本地 Server：
  │           │   1. 读 PID file → 存在？
  │           │   2. 检查进程存活 → 存活？
  │           │   3. 检查 UDS socket 存在 → 存在？
  │           │
  │           ├─ 三步全过 → 委托本地 Server
  │           │
  │           └─ 任一失败 → 独立模式
  │               ├─ 清理 stale PID file（如果进程已死）
  │               ├─ 获取所有权锁（阻塞等待，超时 5s）
  │               ├─ CoreState::load() → execute → save
  │               └─ 释放所有权锁
  │
  └─ 执行完毕，退出
```

### 4.2 关键原则

| 原则 | 说明 |
|------|------|
| 启动时决定模式，运行期间不切换 | 避免状态同步复杂性 |
| 远程模式无 fallback | 远程文件不可达，无法独立操作 |
| 本地 Server 挂了可退化 | 因为能访问文件 |
| 远程 Server 挂了只能等 | 因为碰不到远程文件 |

---

## 5. 并发控制

### 5.1 所有权锁（本地模式，无 Server 时）

实际实现使用 `ownership.lock`（含元数据、持有者类型、CLI 5 步写流程），非简单 `config.lock`。代码示例：

```rust
use fs2::FileExt;

pub struct OwnershipLock {
    file: File,
}

impl OwnershipLock {
    /// 获取排他锁（阻塞等待，带超时）
    pub fn acquire(state_dir: &Path, timeout: Duration) -> Result<Self> {
        let lock_path = state_dir.join("ownership.lock");
        let file = File::create(&lock_path)?;
        let start = Instant::now();
        loop {
            if file.try_lock_exclusive().is_ok() {
                return Ok(Self { file });
            }
            if start.elapsed() > timeout {
                bail!("another process is holding the ownership lock (timeout: {}s)",
                      timeout.as_secs());
            }
            thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for OwnershipLock {
    fn drop(&mut self) {
        self.file.unlock().ok();
    }
}
```

**使用方式：**
```rust
fn standalone_execute(state_dir: &Path, config_path: &Path, cmd: AdminCommand)
    -> Result<AdminResponse>
{
    let _lock = OwnershipLock::acquire(state_dir, Duration::from_secs(5))?;
    let mut core = CoreState::load(config_path)?;   // read
    let resp = core.apply(cmd)?;                     // modify + save
    Ok(resp)                                         // drop _lock → 释放
}
```

### 5.2 Server 内部锁（Server 在跑时）

```rust
pub struct ServerCore {
    inner: Mutex<CoreState>,
}

impl ServerCore {
    /// 处理 admin 请求——自动串行化
    pub fn handle(&self, cmd: AdminCommand) -> Result<AdminResponse> {
        let mut core = self.inner.lock().unwrap();
        core.apply(cmd)  // 内存操作 + 落盘，原子完成
    }
}
```

### 5.3 统一锁模型

实际实现使用双锁架构：
- `ownership.lock`：CLI/Server 写操作互斥（含元数据、持有者类型、CLI 5 步写流程）

```
场景 A：无 Server，多个 CLI
──────────────────────────────
CLI-1: [ownership.lock] [read] [modify] [write] [unlock]
CLI-2:   [等待...]                       [lock] [read] [modify] [write] [unlock]

场景 B：Server 在跑，多个 CLI
──────────────────────────────
CLI-1 → Admin API (UDS) → Server 内部处理 → [Mutex lock] ... [unlock]
CLI-2 → Admin API (UDS) → Server 内部处理 → [等待 Mutex...] ...
所有写操作通过 UDS 管理通道委托到 Server，Server 内部串行化

场景 C：Server 写盘 + CLI 独立模式
──────────────────────────────
Server 落盘时使用 ownership.lock：
Server: [ownership.lock] [write config.toml] [unlock]
CLI:    [等待...]  [lock] [read] [write] [unlock]
```

---

## 6. 全场景矩阵

### 6.1 远程模式场景（待实现，只读）

| # | 时序 | CLI 行为 | 并发控制 | 冲突风险 |
|---|------|---------|---------|---------|
| R1 | Server 已启动 → CLI 启动 | 检测 server.listen → 连 Server → 只读查询（status/usage） | Server Mutex | 无 |
| R2 | Server 未启动 → CLI 启动 | 连不上 → 报错退出 | — | 无 |
| R3 | CLI 查询中 → Server 宕机 | 连接错误 → 报错，用户重试 | — | 无 |
| R4 | CLI-1 运行中 → CLI-2 启动 | 两个都连远程 Server → Server 串行 | Server Mutex | 无 |
| R5 | Server 恢复 → CLI 重试 | 重连成功 → 正常查询 | — | 无 |

> 远程模式只支持读操作（status/usage 查询），写操作仅本机 UDS（2026-08-09 决策）。

### 6.2 本地模式场景

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

### 6.3 微妙场景详解

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

---

## 7. 数据位置与敏感信息

### 7.1 核心原则：谁发请求，谁持有凭据

Server 是实际向上游 API 发请求的进程，因此 API Key、OAuth Token 必须在 Server 机器上可用。

### 7.2 各数据的存放位置

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

### 7.3 远程模式下的环境变量问题

用户在本地 CLI 设置 `api_key_env = "DEEPSEEK_API_KEY"`，这个环境变量指的是 **Server 机器上的环境变量**，不是 CLI 本地的。

**处理方式：**
1. CLI 发送 `ProviderAdd { api_key_env: "DEEPSEEK_API_KEY" }` 给 Server
2. Server 检查自己的环境中是否存在该变量
3. 不存在 → 返回警告：`"env var DEEPSEEK_API_KEY not set on server"`
4. 存在 → 发 probe 请求验证连通性

**设置远程环境变量的方式（分阶段）：**
- **Phase 1（当前）**：用户 SSH 到 Server 手动设置
- **Phase 2（后续）**：Admin API 提供 `/admin/env/set` 端点（需加密传输）

### 7.4 远程模式下的 Launch/Connect

```
用户在本地：llm-proxy launch codex

CLI 流程：
1. GET /admin/client-config/codex → Server 返回配置内容（模型/协议/能力/auth）
2. CLI 用 server.listen 的 origin 推导 base_url（地址 = CLI 实际访问 server 的地址）
3. 翻译成本地客户端格式
4. 写入本地 ~/.codex/config.toml

写入的内容指向本地/远程 Server：
  [model_providers.remote]
  base_url = "<server.listen origin>/v1"
```

**地址来源原则**：base_url 由 **CLI 推导**（用 `server.listen` origin + 协议前缀），**server 不返回 base_url**——server 不知道 CLI 是从哪个地址访问它的，返回地址反而可能错。CLI 与目标客户端处于同一网络位置，CLI 可达 server 的地址即客户端可达地址。launch 不提供 `--proxy-url` 类覆盖 flag。

---

## 8. Admin API 设计

### 8.1 端点定义

**写操作**（仅 UDS 管理通道，0600 权限即信任）：

```
POST   /shutdown                    停止服务
POST   /env                         注入环境变量
GET    /state                       查询进程内状态（bad-request-block 等）
POST   /admin/provider/add          添加 provider
POST   /admin/provider/remove       删除 provider
POST   /admin/provider/copy         复制 provider
POST   /admin/model/add             添加 model
POST   /admin/model/set             设置 model 参数
POST   /admin/model/remove          删除 model
POST   /admin/model/provider        管理 model 的 provider binding（add/remove/move）
POST   /admin/config/reload         重载配置
POST   /admin/config/update         更新配置（原子写入）
POST   /admin/cooldown/clear        清除冷却状态
POST   /admin/oauth/write           写入 OAuth 凭据
```

**读操作**（HTTP 公开接口，与转发服务同端口）：

```
GET    /admin/ping                  健康检查（CLI 检测 Server 用）
GET    /admin/status                查询状态
POST   /admin/status/probe          探测上游
GET    /admin/provider/list         列出 provider
GET    /admin/provider/{id}         查询 provider 详情
GET    /admin/model/list            列出 model
GET    /admin/model/{id}            查询 model 详情
GET    /admin/client-config/{client} 获取客户端配置
GET    /admin/usage                 查询用量
```

### 8.2 请求/响应格式

```json
// POST /admin/provider/add
{
  "name": "deepseek",
  "api_key_env": "DEEPSEEK_API_KEY",
  "endpoints": {
    "openai_chat": {
      "url": "https://api.deepseek.com/v1/chat/completions"
    }
  }
}

// Response
{
  "status": "ok",
  "data": { "message": "provider deepseek added" }
}

// GET /admin/usage?period=7d&provider=deepseek&view=by-model
// Response
{
  "status": "ok",
  "data": {
    "records": [...],
    "summary": {
      "total_input": 12345,
      "total_output": 6789,
      "total_requests": 42
    }
  }
}
```

### 8.3 安全

| 场景 | 认证方式 |
|------|---------|
| 本地模式写操作（UDS） | 无需 token（0600 权限 + 本机 UDS 即信任） |
| 本地模式读操作（HTTP） | 无需认证（只接受 127.0.0.1 来源） |
| 远程模式（待实现） | 只读操作，认证方案待设计 |

> 2026-08-09 决策：远程模式只支持读操作。`admin_token` 认证方案在远程模式实现时重新设计。

---

## 9. Server 发现机制（本地模式）

### 9.1 PID File

```
~/.local/state/llm-proxy/llm-proxy.pid
```

内容：纯数字 PID（Linux 惯例，如 `12345\n`），不含 JSON。

管理通道使用固定路径的 UDS socket（`~/.local/state/llm-proxy/llm-proxy.sock`），不需要在 PID 文件中记录端口。

### 9.2 检测流程

```rust
fn detect_local_server(state_dir: &Path) -> Option<()> {
    // 1. 读 PID file（纯数字）
    let pid = read_pid_file(state_dir)?;

    // 2. 检查进程是否存活
    if !process_alive(pid) {
        cleanup_stale_pid_file(state_dir);
        return None;
    }

    // 3. 检查 UDS socket 是否存在
    let sock = state_dir.join("llm-proxy.sock");
    if !sock.exists() {
        return None;
    }
    Some(())
}

    Some(client)
}
```

### 9.3 Stale PID 处理

| 情况 | 处理 |
|------|------|
| PID file 存在，进程已死 | CLI 清理 PID file，进入独立模式 |
| PID file 存在，进程存活，ping 失败 | Server 可能还在启动中，CLI 短暂重试或报错 |
| PID file 不存在 | 无 Server，进入独立模式 |
| Server 正常退出 | Server 自己清理 PID file |

---

## 10. 迁移策略

现有代码不需要一次性重构，采用渐进式迁移：

### Phase 1：定义 Core 层（不改动现有代码）

```rust
// 新增 src/core.rs
pub struct CoreState { ... }
impl CoreState { ... }
```

### Phase 2：新功能的 Core 化

- usage_stats 功能直接按新架构实现
- `UsageStore` 作为 `CoreState` 的组件

### Phase 3：逐步迁移现有功能

- `connect.rs` 中的 `add_provider`、`remove_provider` 等业务逻辑迁入 `CoreState`
- 现有函数变为薄包装：

```rust
// 过渡期：旧函数调用 Core
pub fn remove_provider(config_path: &Path, id: &str, force: bool) -> Result<()> {
    if !force { confirm_remove_provider(id)?; }  // 交互层
    let mut core = CoreState::load(config_path)?;
    core.remove_provider(id)?;                    // 业务层
    core.save()?;                                 // 持久化
    Ok(())
}
```

### Phase 4：Server 化

- 添加 Admin API 路由
- Server 启动时创建 `ServerCore { inner: Mutex<CoreState> }`
- CLI 添加 Server 检测逻辑

### Phase 5：远程模式（待实现）

- 使用 `server.listen` 配置远程 Server 地址
- CLI 添加远程 HTTP 只读客户端（status/usage 查询）
- 写操作仅本机 UDS（2026-08-09 决策：远程只读）
- 远程读操作认证方案待设计

---

## 12. Status 命令设计决策

Status 命令的完整设计见 [`rust-v2-implementation-design.md` §19](rust-v2-implementation-design.md#19-status-command-design-2026-08-01)（权威版本），本处以代码实现为准不再重复。

核心原则（详见 §8）：
- **读操作**（status 查询、launch 数据获取）：HTTP 公开接口（与转发服务同端口）
- **写操作**（provider/model 增删改、OAuth token 写入）：仅 UDS（本机管理通道）
- `status --probe` 执行真实上游探测；默认 `status` 只读缓存

## 13. 版本兼容性检查设计（2026-08-01）

### 13.1 核心原则

**CLI 和 Server 版本必须兼容**——不兼容时报错退出，提示用户升级。

### 13.2 版本格式

使用 **semver**（`major.minor.patch`，如 `0.2.1`）。

### 13.3 兼容性规则

**semver 范围表达式**——灵活表达兼容范围：

```rust
// Server 声明兼容的 CLI 版本范围
const COMPATIBLE_CLI_VERSIONS: &str = "^0.1.0";  // 0.1.x 都兼容

// 或更灵活
const COMPATIBLE_CLI_VERSIONS: &str = ">=0.1.0, <0.3.0";  // 0.1.x 和 0.2.x 兼容
```

**语义**：
- `^0.1.0`：`0.1.x` 都兼容（major=0 时，minor 相同即兼容）
- `~0.2.0`：`0.2.x` 都兼容（patch 可以变）
- `>=0.1.0, <0.3.0`：`0.1.0` 到 `0.2.x` 都兼容

### 13.4 检查时机

**`ping` 时检查**（一次性），不兼容则报错退出。

### 13.5 不兼容时的行为

**报错退出**，提示升级：

```text
Error: server version 0.1.5 incompatible with CLI 0.2.0
Please upgrade server: llm-proxy serve (on server machine)
```

### 13.6 实现要点

**Server 侧**：
- `ping` 端点返回 `{status: "ok", version: "0.2.0"}`
- 启动时记录版本（`env!("CARGO_PKG_VERSION")`）

**CLI 侧**：
- 解析 `ping` 返回的 version
- 用 `semver` crate 检查：`semver::satisfies(server_version, COMPATIBLE_SERVER_VERSIONS)`
- 不兼容则报错退出

---

## 14. Usage 数据源设计（2026-08-01）

### 14.1 核心原则

**Server 统一管理 usage 数据**：只有经过 server 的请求才统计，内存为权威数据源，磁盘为持久化备份。

### 14.2 数据流

```text
Server 启动：
  磁盘（JSONL/SQLite）→ 加载 → 内存（权威数据源）
  
Server 运行：
  转发请求 → 写入内存 → 定期智能落盘
  
CLI 查询：
  CLI → 委托 Server → 读取内存 → 返回
  
Server 崩溃：
  内存丢失 → 重启时从磁盘加载（最多丢失"上次落盘到现在"的数据）
```

### 14.3 智能落盘策略

**核心思想**：根据请求频率动态调整落盘间隔，平衡及时性和磁盘寿命。

#### 不落盘条件

- `dirty = false`（所有数据已落盘）→ **不启动定时器**（零 I/O）

#### 落盘条件

- `dirty = true`（有未落盘的数据）→ 按频率间隔落盘

#### 频率分档

| 请求频率（条/分钟） | 落盘间隔 | 场景 |
|-------------------|---------|------|
| 0 | 不落盘（dirty=false 时） | 完全空闲 |
| 1-10 | 180 秒（3 分钟） | 轻度使用 |
| 11-100 | 45 秒 | 中度使用 |
| >100 | 15 秒 | 重度使用 |

#### 状态机

| 状态 | dirty | 定时器 | 行为 |
|------|-------|--------|------|
| **空闲** | false | 未启动 | 不落盘，不启动定时器（零 I/O） |
| **有数据未落盘** | true | 运行中 | 按频率间隔落盘 |
| **刚落盘完** | false | 运行中 | 停止定时器，回到空闲 |

#### 实现要点

```rust
struct UsageStore {
    records: Vec<UsageRecord>,      // 内存中的 usage 数据
    dirty: bool,                     // 是否有新数据
    recent_requests: VecDeque<i64>,  // 最近 60 秒的请求时间戳（滑动窗口）
}

impl UsageStore {
    fn on_request(&mut self) {
        let now = now_timestamp();
        self.recent_requests.push_back(now);
        self.dirty = true;
        // 清理 60 秒前的记录
        while let Some(&old) = self.recent_requests.front() {
            if now - old > 60 {
                self.recent_requests.pop_front();
            } else {
                break;
            }
        }
    }
    
    fn calculate_interval(&self) -> Duration {
        let rpm = self.recent_requests.len() as u32;
        match rpm {
            0 => Duration::from_secs(180),  // 3 分钟（最低档）
            1..=10 => Duration::from_secs(180),
            11..=100 => Duration::from_secs(45),
            _ => Duration::from_secs(15),
        }
    }
}
```

### 14.4 磁盘写入频率（优化后）

| 使用模式 | 每天落盘次数 | 说明 |
|---------|-------------|------|
| 完全空闲 | **0 次** | dirty=false，定时器不启动 |
| 偶尔请求（1-10 条/分钟） | ~480 次（每 3 分钟一次） | 只在 dirty=true 时 |
| 中度（11-100 条/分钟） | ~1920 次（每 45 秒一次） | 只在 dirty=true 时 |
| 重度（>100 条/分钟） | ~5760 次（每 15 秒一次） | 只在 dirty=true 时 |

### 14.5 独立模式兜底

**Server 未启动时**：CLI 直接读磁盘（JSONL/SQLite），加文件锁避免竞争。

---

## 15. 总结

### 核心设计决策

| 决策 | 选择 | 理由 |
|------|------|------|
| 业务逻辑位置 | Core 层（内存状态） | 可测试、可复用、与 I/O 解耦 |
| 持久化控制权 | Core 层独占 | 避免多个外层并发写盘 |
| 多 CLI 并发（无 Server） | 文件锁（flock） | 简单、跨平台、OS 自动释放 |
| 多 CLI 并发（有 Server） | Server 内部 Mutex | Server 是唯一写者 |
| 远程模式 fallback | 无 fallback，报错 | 远程文件不可达 |
| 本地 Server 挂了 | 退化为独立模式 | 本地文件可达 |
| 运行期间模式切换 | 不允许 | 避免状态同步复杂性 |
| 敏感数据存储 | Server 机器 | 谁发请求谁持有凭据 |

### 一句话总结

**Core 层拥有状态和持久化，接入层只做交互和转发。有 Server 就委托，没有就自己干（加锁）。远程连不上就报错，本地 Server 挂了就退化。**

