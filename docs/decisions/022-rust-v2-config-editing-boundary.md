# ADR-022: Rust v2 配置编辑边界

## Status

Accepted

## Date

2026-07-20

## Context

Rust v2 会持续增加 `launch`、`connect`、provider 管理和客户端配置生成能力。很多命令需要修改已有配置文件，并且必须避免破坏用户手写字段或外部工具生成字段。

当前涉及三类文件：

- llm-proxy 主配置：项目自己的结构化配置，当前 Rust v2 使用 TOML。
- 外部客户端 JSON 配置：例如 pi `models.json`、Qwen Code `settings.json`、Claude Code `settings.json`。
- 全量托管投影文件：例如 Codex model catalog、status cache。

曾讨论是否引入 tree-sitter 作为配置编辑工具。tree-sitter 擅长构建语法树，但它不直接提供语义编辑、path set/delete、托管区域替换、注释归属和格式写回策略。对本项目当前配置编辑问题，引入 tree-sitter 会增加维护面，而不会直接解决核心约束。

## Decision

Rust v2 不把 tree-sitter 作为默认配置编辑工具。

配置编辑按文件类型分层：

- 主 TOML 配置：使用 `toml_edit` 做 round-trip 编辑；运行时配置仍反序列化为强类型 `Config` 校验。
- 外部客户端 JSON 配置：使用 `serde_json::Value` 做局部编辑，保留未知字段，只替换 llm-proxy 明确托管的局部区域。
- 全量托管投影文件：允许强类型结构全量序列化。

JSON 局部编辑必须遵守：

- 不强类型解析整个用户 JSON 文件。
- 不要求非托管字段满足 llm-proxy schema。
- 只替换带明确管理标识的对象或数组项，例如 `llm-proxy-*` provider、`envKey = LLM_PROXY_API_KEY` 的 Qwen Code model provider。
- 写入前先通过强类型结构生成托管片段，再转换成 `serde_json::Value` 合并。

Rust v2 当前将这些规则收拢到 `json_edit` helper，避免每个 launch 命令重复实现 JSON path 操作。

## Consequences

收益：

- 避免 tree-sitter 引入后的解析器、查询、编辑和写回复杂度。
- 修复并预防“用户已有 JSON 字段缺失导致 launch 失败”的问题。
- 外部客户端配置可以在保留未知字段的同时进行确定性局部更新。
- 主配置仍保留强类型校验和 round-trip 编辑能力。

代价：

- JSON 写回会重新格式化整个文件，不能保留原始空白和字段格式。
- `serde_json::Value` 局部编辑需要维护小型 helper，避免散落的手写 path 操作。
- 如果未来要支持带注释的 JSONC/JSON5，需要重新评估解析和写回策略。

## Alternatives Considered

### 使用 tree-sitter 统一编辑所有配置

优点是可以处理更通用的语法树场景。

拒绝原因：tree-sitter 不直接解决语义 path 编辑和格式写回问题；对 JSON 没必要，对 TOML 已有 `toml_edit` 更贴合需求。

### 所有 JSON 都强类型解析后全量写回

优点是类型安全强，代码直观。

拒绝原因：外部客户端配置不归 llm-proxy 完全拥有，强类型解析会误伤用户字段和旧版本字段。B-01 的 pi `input` 缺失问题就是该方案的实际失败案例。

### 对 JSON 使用字符串替换

优点是短期实现成本低。

拒绝原因：容易破坏 JSON 结构，无法可靠处理字段顺序、转义、数组项替换和嵌套路径。
