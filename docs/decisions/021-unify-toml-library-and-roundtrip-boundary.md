# ADR-021: 统一 TOML 库并限定 Round-Trip 编辑边界

## Status

Accepted

## Date

2026-07-17

## Context

llm-proxy 曾同时使用两个 TOML 库：

- `github.com/BurntSushi/toml`：常规配置解析和序列化。
- `github.com/pelletier/go-toml/v2`：round-trip TOML 编辑，保留注释、空白行和字段顺序。

双库带来维护成本和行为差异风险。项目又需要在 `connect`、`launch codex`、`launch claude-desktop` 等路径中尽量保留用户配置格式，因此仍需要 `go-toml/v2/unstable/edit` 的 round-trip 能力。

迁移前通过实验对比验证了两个库在项目关心的解析场景下行为一致，包括缺失字段、类型错误、重复键、内联表、数组、数字格式、多行字符串、环境变量占位符、表数组和自定义 unmarshaler。

主要差异是：

- Pelletier 序列化字符串时可能使用不同引号风格，但 TOML 规范允许，项目测试不依赖具体引号风格。
- BurntSushi 的 `MetaData.IsDefined()` 曾用于区分“字段未设置”和“用户显式设置为 0”。Pelletier 没有直接等价 API。
- Pelletier 提供 `unstable/edit`，支持 round-trip 编辑，这是 BurntSushi 不具备的能力。

## Decision

项目统一使用 `github.com/pelletier/go-toml/v2` 作为 TOML 解析、序列化和 round-trip 编辑库，移除 `github.com/BurntSushi/toml`。

### Explicit zero 检测

用 `github.com/pelletier/go-toml/v2/unstable` parser 构建显式定义 key 集合，替代 BurntSushi `MetaData.IsDefined()`。

该机制用于保留以下语义：

- 如果用户未配置 cooldown/protection 字段，加载后使用默认值。
- 如果用户显式配置为 `0`，加载后保留 `0`，不被默认值覆盖。

### Round-trip 编辑边界

需要保留用户注释和格式的配置修改路径使用 `DocumentConfig` + `go-toml/v2/unstable/edit`。

当前适用路径：

- `llm-proxy connect` 添加/更新 provider。
- `llm-proxy launch codex` 更新 Codex `config.toml` 的托管模型接入字段。
- `llm-proxy launch claude-desktop` 更新 profile route 配置。

不需要 round-trip 的路径继续允许全量序列化：

- `llm-proxy init` 首次创建配置。
- `llm-proxy migrate` 执行格式迁移或兼容性 metadata 修复。
- llm-proxy 自己生成并全量托管的投影文件，例如 Codex catalog。

## Consequences

收益：

- 项目只维护一套 TOML API。
- 可在需要的配置修改路径保留用户注释、空白和字段顺序。
- explicit zero 语义继续被测试覆盖。
- 减少依赖和二进制供应链面积。

代价：

- `unstable` / `unstable/edit` API 仍是上游标记为 unstable 的包，需要通过测试和版本 pinning 控制风险。
- `DocumentConfig` 需要维护一层文件系统抽象和错误路径测试。
- 全量序列化路径的输出引号风格可能与旧版本不同，测试不得依赖具体引号样式。

## Alternatives Considered

### 保留双 TOML 库

优点是迁移成本低，`MetaData.IsDefined()` 可继续直接使用。

拒绝原因：双库增加认知和维护成本，并且 round-trip 编辑已经要求引入 Pelletier。长期保留 BurntSushi 只会扩大行为差异面。

### 将 explicit zero 字段改为指针类型

优点是无需 AST/unstable parser。

拒绝原因：会污染运行时配置结构，使业务代码到处处理指针，且迁移范围更大。显式定义 key 集合把“是否在 TOML 中出现”的问题限制在配置加载层。

### 所有配置写入都使用 round-trip 编辑

优点是格式保留一致。

拒绝原因：`init` 和 `migrate` 本质是生成/重写配置，round-trip 编辑没有收益；Codex catalog 这类投影文件也应全量托管。过度使用 round-trip 会增加实现复杂度。
