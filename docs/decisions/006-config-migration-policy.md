# ADR-006: 配置格式变更必须提供迁移策略

## Status

Accepted

## Date

2026-05-06

## Context

`llm-proxy` 的配置文件是用户跨版本升级时最容易保留的状态。只更新 validator/defaults 而不提供迁移，会导致用户升级二进制后旧配置无法加载。

触发案例：旧配置中 `provider.type = "openai"`，当前版本只接受 `openai-chat` / `openai-responses` / `anthropic`。用户在新机器运行 `llm-proxy init` 后再 `llm-proxy launch codex` 仍失败，因为 `init` 未迁移旧字段，错误提示还误导为“请先运行 init”。

当前用户已手动升级所有机器上的配置；因此本 ADR 先收敛为**开发规范**，不要求立即实现完整迁移框架。后续任何配置格式变更都必须遵守此规范。

## Decision

以后凡是改变配置格式、字段语义、枚举值、默认 provider/model 结构，必须同时设计配置迁移策略。

配置迁移应满足：

1. **迁移优先于报错**：能安全迁移的旧配置，应自动迁移并提示；不能安全迁移时，错误必须指向具体字段和人工修复方式。
2. **模块化**：每个迁移必须是独立函数/API，不能散落在 validator/defaults/CLI handler 中。
3. **幂等**：同一迁移重复执行不会继续改变结果，不产生重复 provider/model。
4. **可组合**：多个历史迁移按确定顺序执行。
5. **可观察**：迁移发生时输出提示，说明改了什么、为什么改、是否写回、备份路径在哪里。
6. **保留窗口**：每个迁移逻辑至少保留 3 个版本；直到下一个版本产生后，才允许评估移除旧迁移。
7. **覆盖入口**：`llm-proxy init` 更新已有配置时必须考虑迁移；正常加载/启动路径（如 `launch codex`）也必须避免给出误导性“请先运行 init”。

推荐接口形态：

```go
type Migration struct {
    ID          string
    FromVersion string
    ToVersion   string
    Introduced  string
    RemoveAfter string
    Description string
    Apply       func(*Config) (changed bool, warnings []string, err error)
}

func ApplyMigrations(cfg *Config) (changed bool, warnings []string, err error)
```

配置文件可引入 schema metadata：

```toml
config_version = 1
```

旧配置没有 `config_version` 时视为 legacy version `0`。

迁移执行顺序应在 validation 之前：

```text
read TOML
  → decode raw config
  → ApplyMigrations(raw)
  → Validate(migrated)
  → resolve env/defaults
```

否则旧枚举/旧字段会先被 validator 拒绝，无法迁移。

## Initial Known Migration

已知历史迁移：

- `provider.type == "openai"` → `"openai-chat"`
- `model.type == "openai"` → `"openai-chat"`

原则：provider `name` 不应自动改名，避免破坏 `models[].providers` 引用。例如 `name = "openai"` 可以保留，只迁移 `type`。

为什么迁到 `openai-chat`：旧 `openai` 语义更接近 Chat Completions；Responses 是新的协议身份，应由用户显式配置或由默认配置新增 `openai-responses` provider。

当前用户已手动完成该迁移，因此不要求立即实现此历史迁移；但后续如果实现 migration layer，应把它作为第一个迁移用例。

## Alternatives Considered

### 只在错误信息里提示用户手动改配置

- Pros: 实现简单。
- Cons: 每次升级都可能让用户卡住；`init` 也无法修复已有旧配置；不符合“升级应提供提示和迁移”的目标。
- Rejected: 作为 fallback 可以保留，但不能作为唯一机制。

### `init` 直接覆盖用户配置

- Pros: 彻底消除旧格式。
- Cons: 会丢用户自定义 provider/model/API key/env 设置，风险不可接受。
- Rejected: 必须备份并做结构化迁移，不能重写用户配置。

### 把旧枚举永久兼容在 validator 中

- Pros: 用户无感。
- Cons: 旧语义永久泄漏到核心配置模型，增加协议类型歧义；不利于清理。
- Rejected: 过渡期可兼容，但必须有迁移和移除窗口。

## Consequences

- 后续配置变更需要同时设计迁移、测试和提示文案。
- migration layer 会增加少量维护成本，但避免用户升级断裂。
- 配置 schema 需要版本化或至少迁移 ID 化。
- `Load` / `init` / `launch` 的职责边界需要明确：库函数可返回 warnings；CLI 路径负责备份和写回。

## Testing Expectations

后续实现 migration layer 时必须覆盖：

- legacy config 能在 validation 前迁移。
- 迁移幂等。
- 自动备份成功后才写回。
- 备份失败时不覆盖原配置。
- 错误提示不得误导用户“请先运行 init”，除非配置文件确实不存在。
- 每个迁移都有单独测试。

## Documentation Placement

- 开发规范和设计取舍放在本 ADR。
- 已接受的配置行为合并到 `docs/spec.md`。
- 具体正在实现的迁移草案放 `docs/drafts/`，实现后合并并删除 draft。
- 后续任务和优先级放 [`../../ROADMAP.md`](../../ROADMAP.md)。
