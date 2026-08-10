# ADR-012: `--daemon` 改名为 `--background`，不实现系统服务集成

## 状态

Accepted

## 日期

2026-06-17

## 背景

llm-proxy 使用 `--daemon` 标记表示“后台启动当前进程”。该标记的实际行为是：

1. fork 子进程
2. 调用 `setsid` 脱离当前 terminal
3. 将 stdout/stderr 重定向到 `~/.local/state/llm-proxy/llm-proxy.log`
4. 写 PID 文件到 `~/.local/state/llm-proxy/llm-proxy.pid`
5. 父进程立即退出

然而 `daemon` 一词在 Unix 传统中通常暗示“系统级常驻服务”，容易让用户误以为 llm-proxy 会通过 systemd/launchd 等方式开机自启或接受系统管理。这与当前实现不符。

同时，团队讨论过是否把 llm-proxy 升级为真正的系统服务（L3），最终认为对轻量本地代理来说这是 over-engineering。

## 决策

### 1. `--daemon` 改名为 `--background`

- 新 flag 名为 `--background`，**不再保留 `--daemon` 别名**。
- help 文案明确说明："后台启动当前 llm-proxy 进程（非系统服务），日志写到 state 目录"。
- 相关内部函数和路径同步改名：
  - `runDaemon` → `runBackground`
  - `FilterDaemonArgs` → `FilterBackgroundArgs`
  - `IsDaemonRunning` → `IsBackgroundRunning`
  - `CleanupDaemonRemnants` → `CleanupBackgroundRemnants`
  - `DaemonPidPath` → `BackgroundPidPath`
  - `DaemonLogPath` → `BackgroundLogPath`

这是 breaking change。由于项目仍处于 v1 前快速迭代期，术语准确性优先于向后兼容。

### 2. 不实现系统服务集成

llm-proxy 的核心定位是**开发工具 / coding agent 的本地代理**，不是生产级常驻服务。系统服务集成带来的收益有限，但会引入显著复杂性和维护负担：

| 系统服务的好处 | 在 llm-proxy 场景下的价值 |
|---|---|
| 开机自启 | 低。用户只在 coding 时需要代理，不需要一直运行。 |
| 崩溃自动重启 | 低。代理崩溃说明存在 bug，自动重启可能掩盖问题。 |
| 日志集中管理 | 低。日志量不大，直接查看文件即可。 |
| 不占用 terminal | 已由 `--background` 解决。 |
| 系统级权限/隔离 | 不需要，本地用户级即可。 |

此外，跨平台系统服务实现差异大（systemd、launchd、Windows Service），会增加发布、测试和文档成本。

### 3. 新增 `restart` 命令，但边界清晰

为简化配置变更后的重启流程，新增：

```bash
llm-proxy restart
llm-proxy --config /path/to/config.toml restart
```

**边界**：
- `restart` = `shutdown` + `--background`
- **不保存、不继承上一次启动参数**，只使用当前命令行参数或默认值。
- 只支持当前用户单实例场景。
- 不做 `logs` 子命令，用户直接查看日志文件。

## 替代方案

| 方案 | 说明 | 结论 |
|---|---|---|
| 保留 `--daemon` 并加 `--background` 别名 | 兼容性好，但术语误导仍存在 | 拒绝 |
| `--daemon` 改名为 `--background` 并保留别名 | 渐进迁移 | 拒绝，术语准确性优先 |
| 实现系统服务集成（systemd/launchd） | 真正的 L3 daemon | 拒绝，超出项目定位 |
| 实现自监护 daemon（崩溃自动重启） | 不依赖 OS supervisor | 拒绝，复杂且平台差异大 |

## 影响

- **Breaking change**：`llm-proxy --daemon` 不再识别，用户需改用 `llm-proxy --background`。
- 文档、提示信息、FAQ 统一更新为 `--background`。
- 不需要新增系统服务安装/卸载命令。
- 单二进制文件定位保持不变。

## 相关文件

- `cmd/llm-proxy/main.go`
- `internal/config/daemon.go`
- `internal/config/path.go`
- `internal/config/daemon_test.go`
- `docs/spec.md`
- `docs/guide/commands/10-shutdown.md`
- `docs/guide/commands/12-restart.md`
- `ROADMAP.md`

## 备注

历史 draft `docs/drafts/xdg-paths-and-daemon-spec.md` 和 `docs/drafts/spec-service-startup-protection-and-status.md` 中的内容已合并到 `docs/spec.md`，本决策确认后已删除这两个 draft。
