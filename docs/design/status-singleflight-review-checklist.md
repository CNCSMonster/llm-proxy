# Status Singleflight 重构 - Review Checklist

## 重构目标

实现真正的 Singleflight 机制，替代原有的时间窗口去重方案，解决以下问题：
1. 并发 probe 请求重复发送到 upstream（浪费 API 配额）
2. 无法共享正在进行的 probe 结果（等待者超时）
3. 缓存检查逻辑不完善（5 秒窗口失效）

## 关键设计决策

### 1. 三层判断逻辑

```
L1: Active Provider Check (30s TTL)
  ↓ miss
L2: Cache Result Check (5s window, all protocols)
  ↓ miss
L3: Singleflight Merge (wait and share result)
```

**设计理由**：
- L1 利用实际流量作为"活的证据"，避免冗余 probe
- L2 缓存检查覆盖所有协议（openai_chat, anthropic 等），避免 key 不一致
- L3 真正的并发合并，等待者共享 leader 的结果

### 2. DRY 原则

`ProbeCoordinator<S: ProbeState>` 泛型设计，Server 和 CLI 复用同一套代码：
- `ServerProbeState`: 内存活跃状态 + 磁盘缓存
- `CliProbeState`: 仅本地缓存

### 3. ProbeOutcome 结构

返回 `ProbeOutcome { result, executed }` 而非 `ProbeResult`：
- `executed=true`: 实际执行了 probe（加入 probed 列表）
- `executed=false`: 从缓存/singleflight 返回（不加入 probed 列表）

**设计理由**：让调用方知道是否实际执行了 probe，用于统计和日志。

## 并发安全性

### 锁顺序

```
flights (tokio::sync::Mutex)
  ↓
state (tokio::sync::RwLock read)
  ↓
cache (internal tokio::sync::RwLock)
```

**无死锁保证**：
- `state` 只使用 `read()`（永不阻塞）
- `flights` 持有跨 await 是安全的（tokio Mutex 特性）
- 无环形依赖

### 取消安全性

`FlightGuard` 实现 `Drop` trait：
- Leader 被取消/panic 时，自动重置 `in_progress=false`
- 通知所有 waiters（发送 Error）
- 使用 `try_lock` 避免阻塞 runtime

### 已知权衡

1. **try_lock 模式**：`is_active`/`get_cached`/`update_cache` 使用 `try_lock`，锁竞争时静默跳过
   - 影响：高并发下可能重复 probe 或丢失更新
   - 可接受：probe 是幂等操作，且频率低

2. **Flights map 增长**：完成后的 flight 保留在 map 中
   - 影响：内存缓慢增长（每个 provider×model 对）
   - 可接受：模型集合通常静态，内存有限

3. **磁盘缓存竞态**：多个 probe 并发写磁盘缓存
   - 影响：last-writer-wins，可能丢失更新
   - 可接受：probe 结果幂等，且 `atomic_write` 保证文件不损坏

## 测试覆盖

- ✅ 245 测试全部通过（连跑 5 次稳定）
- ✅ **probe_coordinator.rs 覆盖率 92.1%**（151/164）
- ✅ 核心路径全覆盖：
  - 三层判断（L1 活跃 / L2 缓存 / L3 并发合并）
  - 真实并发合并（2 并发 → 1 executed，mock 计数 1）
  - FlightGuard 取消安全
  - CliProbeState 缓存往返 + 过期
  - execute_probe 错误分支（500 → Error）
  - 多协议遍历 + singleflight 合并
- ✅ 全部测试不依赖真实 API key（mock upstream / 纯逻辑 / 临时文件）
- ✅ 共享全局 cache 的测试用 serial_test 串行化（防 flaky）

## 架构结构（最终）

```text
admin.rs（薄 handler，HTTP 层）
  └─ status_probe → probe_coordinator.probe_all_inactive()

probe_coordinator.rs（核心逻辑）
├─ ProbeCoordinator: probe() / probe_all_inactive() / execute_probe()
├─ ActiveProviderStore: 活跃状态单一源
├─ CacheState: L2 缓存读写（DRY，get/update 只写一次）
├─ ServerProbeState = ActiveProviderStore + CacheState（组合）
└─ CliProbeState = CacheState（预留 CLI 集成）
```

## 提交历史

```
c8a4fb2 feat: 实现 ProbeCoordinator 模块（真 Singleflight 基础）
f64ebe1 feat: 重构 admin.rs:status_probe 使用 ProbeCoordinator
cce3ce1 fix: 修复 singleflight 窗口检查，确保 probe 结果正确缓存
9967e21 fix: 添加 FlightGuard 防止 leader 取消导致单飞槽永久卡死
a7d4974 fix: L2 缓存检查遍历所有协议，避免 key 不一致
18d296f docs: 添加详细的模块文档，提供 review 上下文
67b9054 docs: 创建 Status Singleflight review checklist
eca1f30 fix: 统一活跃 provider 状态源为 ActiveProviderStore
d6e6d13 test: 补充 Singleflight 单元测试，删除 probe_due 死代码
ec5f02b test: 补充 admin handler 测试，删除 probe_provider 死代码
96f38e3 refactor: admin.rs 薄化，核心遍历逻辑移入 probe_coordinator
bf9a0f6 test: 补充核心 singleflight 路径测试，覆盖率 67.4% → 90%
（待提交）refactor: CacheState DRY 提取 + 文档收尾
```

---

**最后更新**：2026-08-02
**维护者**：llm-proxy-rust-v2 团队
