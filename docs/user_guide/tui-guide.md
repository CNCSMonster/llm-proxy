# TUI 使用指南

llm-proxy 提供两个 TUI 交互入口：

| 命令 | 功能 |
|------|------|
| `llm-proxy provider`（无子命令） | 主面板：Provider 管理（列出、增删改、登录、fallback、usage） |
| `llm-proxy connect`（无参数） | 添加 provider 的完整交互流程 |

两个入口共用同一套界面组件，本文按界面逐张列出快捷键。

---

## 1. Provider 管理面板

`llm-proxy provider` 主面板，列出所有已配置 provider。

| 按键 | 功能 |
|------|------|
| `↑` / `k`、`↓` / `j` | 上下移动光标 |
| `/` | 进入过滤模式（输入关键字过滤） |
| `Enter` | 查看选中 provider 的详情 |
| `a` | 添加 provider（进入 connect 流程） |
| `d` | 删除选中 provider（确认对话框） |
| `e` | 编辑选中 provider（自定义配置编辑器） |
| `f` | 为选中 provider 配置 fallback |
| `l` | OAuth 登录（非 OAuth provider 会显示提示） |
| `r` | 刷新 provider 列表（自动刷新过期的 OAuth token） |
| `R`（Shift+R） | 重置选中 provider 的 usage 配额（确认对话框） |
| `u` | 查看选中 provider 的 usage 统计摘要 |
| `q` | 退出 |

---

## 2. Provider 详情面板

在管理面板按 `Enter` 进入，展示 provider 的认证状态、各协议端点与绑定的模型列表。

| 按键 | 功能 |
|------|------|
| `Esc` | 返回管理面板（保持光标位置） |
| `↑` / `k`、`↓` / `j` | 在绑定的模型列表中移动光标 |
| `r` | 重命名光标处模型（见下文"模型重命名"） |
| `d` | 删除当前 provider（确认对话框） |
| `e` | 编辑当前 provider |

---

## 3. connect 流程

无参数 `llm-proxy connect` 启动添加 provider 的交互流程，按顺序经过以下界面：

```
产品选择 → 命名屏（仅第 2+ 次 connect）→ env 选择 → 模型选择 / fallback 配置 → 完成
```

- **首次 connect**：默认进入**模型选择**界面（勾选模型模板）。
- **再次 connect 同一产品**：默认进入 **fallback 配置**界面（把新 provider 配置为已有 provider 的 fallback），且先经过**命名屏**（预填 `产品名-2` 等递进 ID）。

### 3.1 产品选择

| 按键 | 功能 |
|------|------|
| `↑` / `k`、`↓` / `j`、`Ctrl+P`、`Ctrl+N` | 移动光标 |
| `/` | 过滤 |
| `Enter` | 确认选择 |

### 3.2 命名屏（仅第 2+ 次 connect）

为新的 provider 指定名称。有两种模式：

- **浏览模式**（默认）：`e` 进入编辑模式，`Enter` 直接确认预填名称。
- **编辑模式**：直接输入字符（字母、数字、`-`、`_`）、`Backspace` 删除、`←`/`→` 移动光标、`Enter` 确认。输入时**实时校验重名**，与已有 provider 重名时显示错误并禁止确认。

### 3.3 env 选择

选择 API Key 环境变量。带 `you may want` 标记的条目是根据产品/provider 命名的推荐项（如 `DEEPSEEK_API_KEY`）。

| 按键 | 功能 |
|------|------|
| `↑` / `k`、`↓` / `j`、`Ctrl+P`、`Ctrl+N` | 移动光标 |
| `/` | 过滤 |
| `Enter` | 确认选择 |
| `s` | 跳过（不设置 API Key，适用于 Ollama 等无需认证的产品） |

### 3.4 模型选择

从 catalog 中勾选模型模板（作为 Model ID 绑定到新 provider）。

| 按键 | 功能 |
|------|------|
| `Enter` | 确认选择，进入写入流程 |
| `Space` | 切换光标处模型的勾选状态 |
| `a` | 全选 / 取消全选 |
| `↑` / `k`、`↓` / `j`、`Ctrl+P`、`Ctrl+N` | 移动光标 |
| `/` | 过滤 |
| `F` / `f` | 进入 fallback 配置界面 |
| `c` | 复制光标处模型（见下文"模型复制"） |

> 零选择时按 `Enter` = 跳过：只创建 provider，不绑定任何模型。

### 3.5 fallback 配置

双栏界面：左侧选择 target provider，右侧勾选 `模型:endpoint` 组合，把新 provider 批量插入为这些组合的 fallback（同产品约束，upstream 自动复制 target 的值）。

| 按键 | 功能 |
|------|------|
| `Tab` | 切换焦点（target provider 列表 ↔ 选项列表） |
| `Space` | 勾选/取消勾选选项（选项栏焦点时） |
| `Enter` | 确认并写入 |
| `M` / `m` | 切回模型选择界面 |
| `↑` / `k`、`↓` / `j` | 在焦点栏内移动光标 |

> connect 流程中零选时按 `Enter` 会显示提示"未选择任何 (model, endpoint) 组合"；此时按 `Esc` 跳过 fallback 配置、直接完成添加。从 Provider 管理面板进入且无可用选项时按 `Enter` 直接返回列表。

---

## 4. 模型重命名 / 复制

这两个操作都**只存在于 TUI**，CLI 没有对应子命令。

### 模型重命名

1. 在 Provider 管理面板按 `Enter` 进入详情。
2. 用 `↑`/`↓` 选中要重命名的模型，按 `r`。
3. 输入新 Model ID，按 `Enter` 校验重名并进入确认。
4. 确认对话框按 `y` 提交——重命名会**自动更新所有 binding 中对该模型的引用**。

> 与命名屏/复制屏不同（实时校验），重命名在按 `Enter` 时才校验重名。

### 模型复制

1. 在 connect 流程的模型选择界面，用 `↑`/`↓` 选中源模型，按 `c`。
2. 进入复制确认屏（**初始即编辑模式**）：直接输入新 Model ID（默认 `源ID-<序号>`，实时校验重名）。
3. 按 `Enter` 提交——复制模型配置但**不复制 bindings**。

> 与命名屏不同（默认浏览模式，需 `e` 进入编辑），复制屏默认就是编辑模式，输入后一次 `Enter` 即提交。

---

## 5. 通用按键

| 按键 | 功能 |
|------|------|
| `Esc` | 返回上一级 / 取消 |
| `y` / `n` | 确认对话框确认 / 取消 |
| `Ctrl+N` / `Ctrl+P` | 下拉列表向下 / 向上（Emacs 风格导航） |

---

## 相关文档

- [CLI 使用指南](cli-guide.md) — 命令行的完整用法
- [OAuth 账户管理](user-guide-oauth-accounts.md) — OAuth 认证流程
