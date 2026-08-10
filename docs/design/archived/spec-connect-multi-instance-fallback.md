# Draft Spec: Connect 多 Provider 与 Fallback 用户交互设计

Status: draft (讨论收敛中)
Date: 2026-08-07
Class: user-interaction design
Related: connect / provider / model 命令职责

---

## 1. 背景与动机

### 1.1 现状问题

1. **同名 connect 静默覆盖**：`connect.rs` 用 `cfg.providers.insert(provider_id, provider)` 直接覆盖同名 provider（BTreeMap insert 语义），用户第二次 connect 同一产品时旧配置被静默替换。
2. **OAuth 多 Provider 机制是半成品**：`NameOption` 只有 Recommended/Custom 两值（无 AutoSuffix）、`OAuthName` 屏无提交路径、`OAuthOverwrite` 屏是死代码、推荐名硬编码 `"openai-subscription"`、正常 OAuth 流程不经过命名屏。
3. **没有模型管理面板**：endpoint 级 fallback 配置在 TUI 无入口（只有 connect 时的 ModelSelection）。
4. **`--upstream-model` 默认值错误**：`model provider add` 默认用前端 ID（`deepseek-v4-flash-lp`）而非上游模型名（`deepseek-v4-flash`），fallback 场景会导致上游 400。

### 1.2 目标

- 用户可自由决定配置形态：fallback（自动兜底）或独立模型（手动路由）
- 手动编辑配置文件 + CLI + TUI 三条路径能力等价（配置文件是唯一真相源）
- connect 保持简短，fallback 配置顺手可达但不强制内嵌

### 1.3 非目标

- 不维护数据安全属性（`data_safety` 字段）——用户自行用命名约定管理
- 不做跨产品 fallback 的自动判定——用户显式指定
- 不做 Provider 数量上限/警告

---

## 2. 核心概念与职责划分

### 2.1 概念

- **产品（Product）**：模型服务套餐（如 DeepSeek PAYG、Kimi Code）。**Provider 级字段** `product` 声明 Provider 归属的产品。
- **Provider**：产品下的上游服务配置块（如 `deepseek`、`deepseek-2`），含独立 api_key_env/endpoint + **Provider 级 `product` 字段**。同产品可有多个 Provider（不同 key / 不同区域）。
- **custom 产品**：特殊产品标记，是 `product` 字段的**缺省值**。`product` 缺省（不写）即视为 custom；显式写其他值则归属对应产品。custom 不构成产品分组，custom provider 之间不参与批量 fallback（端点不同、无模板、upstream 无法推断），只能走 `model provider add` 精细入口显式指定。

**product 字段赋值规则**：
- catalog 模板生成的 Provider → connect 自动填产品 ID（如 `product = "deepseek"`）
- **TUI 流程中 product 不可改**：先选产品，选定后 product 字段锁定；要改只能回退到产品选择重新配置
- **名字 ≠ 归属**：Provider 名只是名字（可自定义），归属由 product 字段决定；自定义名像别的产品也不改变 product
- 手动编辑 config 不写 `product` → 缺省即 custom（保证配置平等性，最简形态可用）
- **不做自动迁移**：未正式发版，旧配置（无 product 字段）由使用者手动迁移，或发版时提供迁移脚本
- **Model ID**：客户端请求中使用的模型名（如 `deepseek-v4-flash-lp`），代理对外暴露的能力契约。内部实现对应 `ModelConfig`。
- **上游模型名（Upstream Model）**：Provider 发往上游的真实模型名（如 `deepseek-v4-flash`），与 Model ID 是映射关系。Binding 中的 `model` 字段。
- **Binding**：Model ID 到 provider 的挂载点（含上游模型名），列表顺序 = fallback 优先级

### 2.2 命令职责

| 命令 | 职责 | 视角 |
|------|------|------|
| `connect` | 创建 Provider（命名 + env + 模型选择或 fallback）| 产品向导 |
| `provider` | 管理 Provider；**批量 fallback**（provider 视角）| provider 为中心 |
| `model` | 管理 Model ID；**单模型 fallback**（model 视角）| 模型为中心 |

### 2.3 配置平等性

配置文件 `config.toml` 是唯一真相源，手动编辑 / CLI / TUI 三条路径能力等价：

```
# 手动编辑能写出的形态，CLI/TUI 必须能生成：

# 单 Provider
[providers.deepseek]
api_key_env = "DEEPSEEK_API_KEY"

# 多 Provider + fallback（同一 Model ID 多个 binding）
[providers.deepseek-2]
api_key_env = "DEEPSEEK_API_KEY_2"

[models."deepseek-v4-flash-lp"]
openai_chat_providers = [{name="deepseek", model="deepseek-v4-flash"},
                          {name="deepseek-2", model="deepseek-v4-flash"}]

# 独立 Model ID（手动路由）
[models."deepseek-v4-flash-deepseek-2-lp"]
openai_chat_providers = [{name="deepseek-2", model="deepseek-v4-flash"}]
```

---

## 3. Connect 流程设计

### 3.1 总体流程

```
首次 connect 产品：
  选产品 → 选 env → [模型多选界面] ←默认 → 完成
                      ↑ [F] 快捷键
                   [fallback 配置界面]

重复 connect 产品（已有同名 Provider）：
  选产品 → 命名屏 → 选 env → [fallback 配置界面] ←默认 → 完成
                              ↑ [M] 快捷键
                           [模型多选界面]
```

设计原则：**默认路径 = 最高频意图，快捷键 = 备选，无中间菜单**。

### 3.2 命名屏（仅第 2+ 次出现）

- **A1 首次不显示**：首次 connect 直接用产品名（如 `deepseek`），不打扰新用户
- **A2 预填输入框交互**：
  ```
  Provider Name:  deepseek-2          ← 预填默认值，焦点在此但不在编辑态
                [e] 编辑  [Enter] 确认
  ```
  - `e` → 进入编辑态（光标激活，可修改）
  - `Enter` → 确认当前名字（单行输入，Enter = 提交）
  - `Esc` → 退出编辑态（不提交）；编辑态外 Esc → 返回产品列表
- **A2b 实时校验**：编辑时立即校验，提示常驻（不闪烁不消失）：
  - 冲突 → "名字已被占用"
  - 可用 → "名字可用"
- **A3 默认值自动递进**：`next_instance_id` 只数 `{product}-N` 模式（N 为正整数）取最小未用序号（已有 `deepseek`、`deepseek-2` → 预填 `deepseek-3`）。自定义名（如 `my-deepseek`）不参与计数，只做冲突检查。

### 3.3 env 选择

- 用户自由选择（无约束）
- **重复警告**：选中的 env 已被其他 Provider 使用 → 警告提示（"该 key 已被 deepseek 使用"），允许继续

### 3.4 模型选择界面（默认 B）

- 多选模型，显示上游模型名 + 注入提示
- **零选直接 Enter = 跳过**（只建 provider，不建 Model ID）
- 生成 Model ID 绑当前 Provider（Model ID = 产品模板标准名，不带后缀）
- 快捷键 `[F]` → fallback 配置界面

### 3.5 Fallback 配置界面（默认 A，重复时）

- 选择目标 provider（被兜底的）
- 多选 (model, endpoint) 组合（组合式选项，默认全不选）
- 快捷键 `[M]` → 模型选择界面
- **底层逻辑与 `provider fallback add` 同一套**（选 target → 选组合 → 插入 P1 到 P2 之后）；区别仅 P1 状态：connect 流程中 P1 刚创建，provider 命令中 P1 已存在

### 3.6 OAuth 产品特殊规则

- 重复 connect OAuth 产品（OpenAI 订阅 / Antigravity）时，命名屏逻辑与 API Key 产品一致（预填 `openai-subscription-2` 等）
- **account = Provider 名**：新 Provider 的 OAuth account 即其 Provider 名，需重新走 OAuth 登录流程
- 与 API Key 产品语义一致：每个 Provider 独立凭据，fallback 有配额/限流隔离价值
- 同账号重配（不换凭据）→ 走 provider 面板编辑，不重复 connect
- **写入路径统一**：OAuth connect 与其他 provider 添加一样，验证通过后交所有权执行者独占写入配置（`with_cli_write_lock_or_delegate` → 本地持锁或 UDS 委托 server），同时写 Provider 配置块（含 product 字段）+ OAuth account 凭据

### 3.7 模型命名规则

| 配置形态 | 模型名 | 语义 |
|---------|--------|------|
| fallback（自动兜底）| **不变**（`deepseek-v4-flash-lp`）| 主挂自动切换，客户端无感 |
| 独立 Model ID（手动路由）| 带后缀（`deepseek-v4-flash-deepseek-2-lp`）| 客户端 `/model` 显式切换 |

- connect 默认生成标准模型名（fallback 语义）
- 独立带后缀条目：connect 的模型选择界面生成，或用户 `model add` 手动配置

---

## 4. Fallback 配置设计

### 4.1 命令归属

| 操作 | 命令 | 粒度 |
|------|------|------|
| 批量 fallback（同产品）| `provider fallback add` | 多模型 × 多 endpoint |
| 单模型 fallback（任意，含跨产品）| `model provider add` | 单模型单 endpoint |

**批量限定同产品**：`provider fallback add` 的 target 与 provider 必须同产品（upstream 自动从链首复制，无需用户指定）。跨产品 / 特殊上游映射 → `model provider add` 精细配置。

### 4.2 provider 批量 fallback（TUI）

```
provider TUI（开场 = 已配置 provider 列表）：
  [deepseek]
  [deepseek-2]
  [deepseek-3]      ← ① 选中"提供 fallback 的 provider"（源）
  [kimi-code]

选中 deepseek-3 → 操作 → "为其他 provider 提供 fallback"
  → ② 选择目标 provider（谁被兜底）：[deepseek] [deepseek-2] ...
  → ③ 组合式多选（每个选项 = model [endpoint] 独立勾选），**默认全不选**，用户显式勾选：
       [ ] deepseek-v4-flash-lp [chat]
       [ ] deepseek-v4-flash-lp [responses]
       [x] deepseek-v4-pro-lp   [responses]
       [x] deepseek-v4-pro-lp   [anthropic]
  → ④ 确认：将给选中的 (model, endpoint) 插入 deepseek-3 的 binding → 应用
```

### 4.3 provider 批量 fallback（CLI）

```
llm-proxy provider fallback add \
    --provider deepseek-3 \              # 提供 fallback 的（兜底者）
    --target deepseek \                  # 被兜底的（目标）
    --bindings deepseek-v4-pro-lp:responses \
    --bindings deepseek-v4-pro-lp:anthropic   # 每个 = model:endpoint，可重复，必填
```

- **`--bindings` 必填**（至少一个，缺省报错提示）——与 TUI"默认全不选"一致，防误操作
- **支持正则**：如 `--bindings deepseek-v4-pro:.*` 匹配该 model 的所有 endpoint
- **`--provider` / `--target` 只接受精确名**（不支持正则，正则仅 `--bindings` 支持）
- 语义：在指定 (model, endpoint) 组合的链中，target 之后插入 provider
- **校验与跳过规则**：
  - 显式指定组合（如 `X:chat`）→ target 在该组合无 binding → **报错**（明确指定却不存在）
  - 正则匹配组合（如 `X:.*`）→ target 未参与的组合 → **自动跳过**（正则是候选范围，生效以 target 存在为准）
  - provider 已在链中 → **跳过**（幂等，不报错）
- **批量操作输出**：每个 (model, endpoint) 组合显示执行状态（✓ inserted / ✓ skipped / ✗ error + 原因），最后汇总统计（N inserted, N skipped, N failed）

### 4.4 model 单模型 fallback（现有 + 修复）

```
llm-proxy model provider add \
    --model deepseek-v4-flash-lp \
    --protocol openai-chat \             # 协议参数（--type 加 --protocol 别名统一术语）
    --provider deepseek-2 \
    --upstream-model deepseek-v4-flash   # 默认值修复：从链首 binding 复制上游名
```

**model TUI 绑定管理**：model 详情视图展示各 (model, endpoint) 的 binding 链（顺序 = fallback 优先级），支持对链中 provider 进行**顺序调整**（move 上/下）、添加、移除——批量 fallback 插入后的微调闭环。

### 4.5 约束与不变式

**同链 provider 名唯一**（已由 `validate_model_bindings` 强制，config.rs:736-749）：

```rust
if !seen.insert(binding.name.clone()) {
    bail!("model {model_id} repeats provider {} for {:?}", ...);
}
```

- 同一 (model, endpoint) 链中不允许出现同名 provider（无论 upstream 是否不同）
- 手动编辑 config 写同 provider 两个 binding → `Config::load` 报错
- 想要两个上游变体 → 配两个 provider（如 `deepseek-flash` / `deepseek-lite`）
- 该约束使批量 fallback 复制链首 upstream 无歧义（同链 provider 唯一）

**批量 fallback 规则**（约束下完全收敛）：

```
批量添加（--provider P1 为 --target P2 兜底，同产品）：
  遍历 P2 参与的所有 (model, endpoint) 组合（--bindings 指定，支持正则批量选组合）：
    upstream = 该组合中 P2 binding 自己的 upstream   # 始终跟随 target，不跨组合推断
    若 P1 不在链中（provider 名唯一校验）:
      将 (P1, upstream) 插入到 P2 binding 之后   # 紧跟被兜底的 provider
  完成
```

**插入位置语义**：P1 紧跟 P2 之后（P2 挂了立刻轮到 P1，不被链中其他 provider 插队）；链尾其他产品 provider 保持不动。精确位置控制 → `model provider add` / `move`。

### 4.6 待决策点（Open）

- ~~D1: upstream-model 默认值~~ → 已收敛（链首复制，同链唯一无歧义）
- ~~D2: 跨产品 fallback~~ → 已收敛（批量限定同产品；跨产品走 model 精细入口）
- ~~D3: 重复 binding~~ → 已收敛（provider 名唯一校验，跳过幂等）
- **全部决策已收敛，无 Open 项**

---

## 5. 安全与信任

- **不添加 `data_safety` 字段**，不维护信任属性
- 用户自行管理：可用命名约定区分（如 `deepseek-safe`），也可混合使用
- llm-proxy 只提供命名自由度，不做安全拦截

---

## 6. 影响范围

### 代码改动点（预估）

| 文件 | 改动 |
|------|------|
| `src/connect.rs` | 命名逻辑、`next_instance_id` 纯函数、流程分支、verify 修复 |
| `src/catalog.rs` | `apply_catalog_model_defaults` 解耦模板来源与 binding 目标 |
| `src/tui/model.rs` | 命名屏状态、fallback 配置屏状态、Screen 枚举 |
| `src/tui/update/products.rs` | 产品选中后的存在检测与命名屏跳转 |
| `src/tui/update/oauth.rs` | 补完 OAuthName（提交路径）、移除/复用 OAuthOverwrite |
| `src/cli/types.rs` | `--protocol` 别名、`provider fallback add` 参数 |
| `src/model.rs` | `--upstream-model` 默认值修复 |
| `src/admin.rs` / `admin_client.rs` | 委托链路透传 |

### 回归重点

- 全量 `cargo test` + lint
- connect 首次/重复/自定义名/冲突名各分支
- fallback 批量/单模型/跨产品
- server 运行时委托路径
- 手动编辑 config.toml 后 restart 的等价性验证

---

## 7. 参考

- `src/connect.rs` — 现状静默覆盖
- `src/tui/update/oauth.rs` — OAuthName 半成品
- `src/model.rs` — `model provider add/remove/move`
- `docs/sops/launch-and-access-verification.md` — SOP C7 场景（已有 OAuth 账号重连覆盖确认）
