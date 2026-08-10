# 配置热重载设计方案

## 状态

**Rejected** — 2026-08-04 决定放弃实现

## 日期

2026-08-04

## 关键决策：放弃热重载

### 决策理由

经过分析，决定**不实现**配置热重载功能，原因如下：

#### 1. 环境变量无法热加载（技术限制）

**核心问题**：如果用户直接编辑 `config.toml` 添加新 provider，且该 provider 使用新的环境变量（如 `api_key_env = "NEW_KEY"`），热重载无法解决问题：

```toml
# 用户直接编辑 config.toml
[providers.new-provider]
api_key_env = "NEW_PROVIDER_KEY"  # ← 进程启动时此环境变量不存在
```

- Server 进程启动时，`NEW_PROVIDER_KEY` 环境变量不存在
- 热重载重新加载 config.toml，但**进程的环境变量不会自动更新**
- Provider 加载了，但找不到 API key，无法使用
- **结果**：用户仍需重启 server

#### 2. 使用场景有限（低 ROI）

热重载只覆盖部分场景：

| 场景 | 热重载可行性 | 频率 |
|------|-------------|------|
| 修改现有 provider 配置（不涉及新 env） | ✅ 可行 | 低 |
| 添加新 model 到已有 provider | ✅ 可行 | 低 |
| 添加新 provider（需要新 env） | ❌ 不可行 | **高** |
| 删除 provider/model | ✅ 可行 | 低 |

**关键场景（添加新 provider）不支持**，导致功能价值有限。

#### 3. 与设计哲学冲突

项目的配置管理哲学：

| 方式 | 推荐度 | 生效方式 | 环境变量问题 |
|------|--------|----------|-------------|
| **CLI/TUI** | ✅ 推荐 | 立即生效（admin API） | 无（使用现有 env） |
| **直接编辑 config.toml** | ⚠️ 不推荐 | 需要重启 | 有（新 env 无法加载） |

热重载鼓励"直接编辑 config.toml"，与"推荐 CLI/TUI"的理念不符。

#### 4. 复杂性和风险高

实现热重载需要：
- 文件监听机制（notify crate）
- Debounce 逻辑
- 并发控制（ConfigLock + RwLock）
- 错误处理和恢复
- 多实例协调
- 测试覆盖

评审发现的极端条件风险：
- 并发写入冲突（CLI 写配置时触发重载）
- 配置验证失败循环
- 文件系统事件丢失（NFS/容器）
- 热重载期间的请求处理（新旧配置混合）
- 多进程竞争

**复杂性高，但收益有限**。

### 替代方案

**保持现状**，使用已有的配置更新机制：

1. **CLI/TUI 命令**（推荐）
   - `provider add/remove`, `model add/remove/set`
   - 通过 admin API 委托给 server
   - 立即生效，无需重启

2. **手动 reload API**
   - `/admin/config/reload`
   - 适用于简单配置变更（不涉及新 env）

3. **重启 server**
   - 适用于涉及环境变量的变更
   - 确保干净的配置状态

### 未来考虑

如果未来出现以下情况，可重新评估：
- 多个用户反馈需要热重载功能
- 有具体的自动化工具需要修改配置并自动生效
- 找到解决环境变量限制的方案（如支持 env 文件热加载）

---

## 原始设计（仅供参考，不实现）

### 背景

当前配置更新流程：
1. 用户手动编辑 `config.toml`
2. 调用 `/admin/config/reload` API 或重启服务
3. 服务重新加载配置

问题：
- 用户容易忘记调用 reload
- 需要重启服务时中断正在处理的请求
- 不符合现代开发体验预期

### 目标

实现配置文件自动监听和热重载：
- 监听 `config.toml` 文件变化
- 自动重新加载并验证配置
- 不中断正在处理的请求
- 提供变化通知和错误反馈

## 设计

### 架构

```
┌─────────────────┐
│  config.toml    │
└────────┬────────┘
         │ file change
         ▼
┌─────────────────┐
│  FileWatcher    │  (notify crate)
│  (debounce)     │
└────────┬────────┘
         │ trigger
         ▼
┌─────────────────┐
│  ConfigReloader │
│  - load config  │
│  - validate     │
│  - apply        │
└────────┬────────┘
         │ success/failure
         ▼
┌─────────────────┐
│  CoreState      │
│  - update mem   │
│  - notify subs  │
└─────────────────┘
```

### 核心组件

#### 1. FileWatcher

使用 `notify` crate 监听文件变化：

```rust
use notify::{Watcher, RecursiveMode, watcher};
use std::sync::mpsc;
use std::time::Duration;

pub struct ConfigFileWatcher {
    watcher: notify::RecommendedWatcher,
    rx: mpsc::Receiver<notify::Result<notify::Event>>,
}

impl ConfigFileWatcher {
    pub fn new(config_path: &Path) -> Result<Self> {
        let (tx, rx) = mpsc::channel();
        let mut watcher = notify::recommended_watcher(move |res| {
            let _ = tx.send(res);
        })?;
        
        watcher.watch(config_path, RecursiveMode::NonRecursive)?;
        
        Ok(Self { watcher, rx })
    }
    
    pub fn wait_for_change(&self, debounce: Duration) -> Option<notify::Event> {
        // Debounce: 等待 debounce 时间内没有新事件
        let mut last_event = None;
        loop {
            match self.rx.recv_timeout(debounce) {
                Ok(Ok(event)) => {
                    last_event = Some(event);
                    // 继续等待，看是否有更多事件
                }
                Ok(Err(e)) => {
                    tracing::error!("File watch error: {}", e);
                    return last_event;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    // 超时说明没有新事件，返回最后一个
                    return last_event;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return None;
                }
            }
        }
    }
}
```

#### 2. ConfigReloader

负责加载、验证、应用配置：

```rust
pub struct ConfigReloader {
    config_path: PathBuf,
    state_dir: PathBuf,
}

impl ConfigReloader {
    pub fn reload(&self, core: &mut CoreState) -> Result<ReloadResult> {
        // 1. 持锁加载配置
        let new_config = core.with_config_file_lock(|| {
            Config::load(&self.config_path)
        })?;
        
        // 2. 验证配置
        if let Err(e) = new_config.validate() {
            return Ok(ReloadResult::Invalid { error: e.to_string() });
        }
        
        // 3. 比较差异
        let diff = ConfigDiff::compute(&core.config, &new_config);
        
        // 4. 应用配置
        core.config = new_config;
        
        Ok(ReloadResult::Success { diff })
    }
}

pub enum ReloadResult {
    Success { diff: ConfigDiff },
    Invalid { error: String },
    Unchanged,
}
```

#### 3. ConfigDiff

计算配置差异，用于通知和日志：

```rust
pub struct ConfigDiff {
    pub providers_added: Vec<String>,
    pub providers_removed: Vec<String>,
    pub providers_modified: Vec<String>,
    pub models_added: Vec<String>,
    pub models_removed: Vec<String>,
    pub models_modified: Vec<String>,
}

impl ConfigDiff {
    pub fn compute(old: &Config, new: &Config) -> Self {
        // 比较 providers
        let old_providers: HashSet<_> = old.providers.keys().collect();
        let new_providers: HashSet<_> = new.providers.keys().collect();
        
        let providers_added = new_providers.difference(&old_providers)
            .map(|s| s.to_string()).collect();
        let providers_removed = old_providers.difference(&new_providers)
            .map(|s| s.to_string()).collect();
        let providers_modified = old_providers.intersection(&new_providers)
            .filter(|id| old.providers.get(*id) != new.providers.get(*id))
            .map(|s| s.to_string()).collect();
        
        // 比较 models (类似逻辑)
        // ...
        
        Self {
            providers_added,
            providers_removed,
            providers_modified,
            models_added: vec![],
            models_removed: vec![],
            models_modified: vec![],
        }
    }
    
    pub fn is_empty(&self) -> bool {
        self.providers_added.is_empty()
            && self.providers_removed.is_empty()
            && self.providers_modified.is_empty()
            && self.models_added.is_empty()
            && self.models_removed.is_empty()
            && self.models_modified.is_empty()
    }
}
```

#### 4. 集成到服务

在 `service.rs` 中启动文件监听任务：

```rust
pub async fn serve_with_shutdown(
    config: Config,
    config_path: PathBuf,
    state_dir: PathBuf,
    shutdown: impl Future<Output = ()>,
) -> Result<()> {
    // ... 现有初始化逻辑 ...
    
    // 启动配置热重载任务
    let hot_reload_core = core.clone();
    let hot_reload_path = config_path.clone();
    let hot_reload_state_dir = state_dir.clone();
    
    tokio::spawn(async move {
        if let Err(e) = run_config_hot_reload(
            hot_reload_core,
            hot_reload_path,
            hot_reload_state_dir,
        ).await {
            tracing::error!("Config hot reload failed: {}", e);
        }
    });
    
    // ... 启动服务器 ...
}

async fn run_config_hot_reload(
    core: Arc<RwLock<CoreState>>,
    config_path: PathBuf,
    state_dir: PathBuf,
) -> Result<()> {
    let watcher = ConfigFileWatcher::new(&config_path)?;
    let reloader = ConfigReloader {
        config_path,
        state_dir,
    };
    
    loop {
        // 等待文件变化（2秒 debounce）
        if let Some(event) = watcher.wait_for_change(Duration::from_secs(2)) {
            tracing::info!("Config file changed: {:?}", event);
            
            // 重新加载配置
            let mut core_guard = core.write().await;
            match reloader.reload(&mut core_guard) {
                Ok(ReloadResult::Success { diff }) => {
                    if !diff.is_empty() {
                        tracing::info!("Config reloaded: {:?}", diff);
                        // TODO: 通知订阅者
                    }
                }
                Ok(ReloadResult::Invalid { error }) => {
                    tracing::error!("Config validation failed: {}", error);
                    // TODO: 通知管理员
                }
                Ok(ReloadResult::Unchanged) => {
                    tracing::debug!("Config unchanged");
                }
                Err(e) => {
                    tracing::error!("Config reload failed: {}", e);
                }
            }
        }
    }
}
```

### 配置选项

在 `config.toml` 中添加可选配置：

```toml
[server]
# 是否启用配置热重载（默认 true）
hot_reload = true

# 文件变化后的 debounce 时间（秒，默认 2）
hot_reload_debounce_secs = 2
```

### 错误处理

1. **文件不存在**：记录警告，继续监听（文件可能被删除后重建）
2. **加载失败**：记录错误，保持旧配置，通知管理员
3. **验证失败**：记录错误，保持旧配置，通知管理员
4. **应用失败**：理论上不会发生（验证已通过），如发生则记录严重错误

### 通知机制（可选，Phase 2）

通过 WebSocket 或 SSE 通知连接的客户端：

```rust
// 通知订阅者
for subscriber in subscribers.lock().await.iter() {
    let _ = subscriber.send(Notification::ConfigChanged {
        diff: diff.clone(),
    }).await;
}
```

## 实现计划

### Phase 1: 基础热重载（MVP）

1. 添加 `notify` 依赖到 `Cargo.toml`
2. 实现 `ConfigFileWatcher`
3. 实现 `ConfigReloader`
4. 实现 `ConfigDiff`
5. 集成到 `service.rs`
6. 添加配置选项
7. 编写单元测试
8. 编写集成测试

**预计工作量**：3-4 天

### Phase 2: 通知和监控

1. 实现 WebSocket/SSE 通知
2. 添加 `/admin/config/watch` 端点（查看监听状态）
3. 添加配置变化历史记录
4. 添加 Prometheus 指标（reload_success_total, reload_failed_total）

**预计工作量**：2-3 天

### Phase 3: 高级特性

1. 支持监听多个配置文件
2. 支持配置模板和继承
3. 支持配置版本回滚
4. 支持配置差异对比 API

**预计工作量**：5-7 天

## 风险和缓解

### 风险 1: 文件监听不可靠

**问题**：某些文件系统（NFS、SMB）可能不支持文件监听

**缓解**：
- 提供 fallback 轮询模式（每 N 秒检查文件 mtime）
- 配置选项允许禁用热重载

### 风险 2: 频繁变化导致性能问题

**问题**：编辑器保存时可能触发多次写入

**缓解**：
- 使用 debounce（默认 2 秒）
- 只处理 Modify 事件，忽略 Access 事件

### 风险 3: 配置错误导致服务不可用

**问题**：用户可能写入无效配置

**缓解**：
- 加载后验证，失败则保持旧配置
- 记录详细错误信息
- 可选：通知管理员

### 风险 4: 并发访问冲突

**问题**：热重载和手动 reload 可能冲突

**缓解**：
- 使用现有的 `ConfigLock` (flock) 机制
- `CoreState` 使用 `RwLock` 保护

## 测试策略

### 单元测试

1. `ConfigFileWatcher`: 测试文件变化检测
2. `ConfigReloader`: 测试加载、验证、应用
3. `ConfigDiff`: 测试差异计算

### 集成测试

1. 启动服务，修改配置文件，验证自动重载
2. 写入无效配置，验证保持旧配置
3. 频繁修改文件，验证 debounce 工作
4. 并发测试：热重载 + 手动 reload + 请求处理

### E2E 测试

1. 使用真实客户端（Codex）连接
2. 修改配置添加新 provider
3. 验证客户端可以使用新 provider
4. 修改配置删除 provider
5. 验证客户端收到错误

## 文档更新

1. `docs/user_guide/cli-guide.md`: 添加热重载使用说明
2. `docs/design/config-hot-reload.md`: 本设计文档
3. `AGENTS.md`: 添加热重载架构说明
4. `README.md`: 添加热重载特性说明

## 参考

- Go 版本的 `WatchConfig` 实现（`internal/config/store.go`）
- `notify` crate 文档：https://docs.rs/notify/
- 类似项目：nginx reload, systemd daemon-reload

## 开放问题

1. **是否需要通知连接的客户端？**
   - Phase 1 不实现，Phase 2 考虑
   
2. **是否需要记录配置变化历史？**
   - Phase 1 不实现，Phase 2 考虑
   
3. **是否需要支持配置回滚？**
   - Phase 1 不实现，Phase 3 考虑
   
4. **debounce 时间默认值？**
   - 建议 2 秒，可配置

5. **是否支持监听多个配置文件？**
   - Phase 1 只监听主配置文件
   - Phase 3 考虑支持多个
