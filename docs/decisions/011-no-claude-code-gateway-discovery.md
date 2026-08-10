# 011: 暂不实现 Claude Code gateway 模型发现

## Status

Accepted

## Date

2026-06-17

## Context

Claude Code 从 v2.1.129 开始支持 `CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1`。设置后，启动时 Claude Code 会向 `ANTHROPIC_BASE_URL/v1/models` 发起请求，把返回的模型加入 `/model` 选择器。

M18 协议前缀路由实现时，ROADMAP 曾计划让 `launch claude-code` 自动注入该环境变量。

## 调研结论

官方文档（[LLM gateway configuration](https://code.claude.com/docs/en/llm-gateway)）确认：

1. 该变量确实有效，且默认关闭。
2. 只把 model ID **以 `claude` 或 `anthropic` 开头**的模型加入 picker。
3. 请求认证与 inference 一致，使用 `ANTHROPIC_AUTH_TOKEN`。
4. 缓存文件：`~/.claude/cache/gateway-models.json`。

当前 llm-proxy 的 `launch claude-code` 已支持：

- `ANTHROPIC_MODEL`（默认模型）
- `ANTHROPIC_DEFAULT_HAIKU_MODEL`
- `ANTHROPIC_DEFAULT_SONNET_MODEL`
- `ANTHROPIC_DEFAULT_OPUS_MODEL`

用户可通过 `--haiku` / `--sonnet` / `--opus` 把 llm-proxy 模型映射到 Claude Code 内置别名上，并手动在 `settings.json` 里加 `ANTHROPIC_CUSTOM_MODEL_OPTION` 添加单个自定义条目。

## Decision

**当前阶段不在 `launch claude-code` 中注入 `CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1`。**

原因：

1. Claude Code 的 discovery 过滤器只接受以 `claude` 或 `anthropic` 开头的 model ID，而 llm-proxy 的 frontend model ID（如 `deepseek-v4-flash-lp`、`kimi-for-coding-lp`）会被过滤，注入后用户也看不到任何新模型。
2. 若要让 discovery 生效，需要把 `/anthropic/v1/models` 返回的 model ID 改成 `claude-*` 或 `anthropic-*` 前缀，再在推理阶段把前缀剥离回 llm-proxy 的 model ID，这会增加 model ID 映射复杂度。
3. 现有 alias 映射 + 手动 custom option 已能覆盖 Claude Code 使用 llm-proxy 多模型的需求。
4. 这是 Claude Code gateway 模型发现机制的不成熟之处，不应由 llm-proxy 用复杂的 ID 欺骗来 workaround。

## Alternatives Considered

### Option A: 注入变量但保持现有 model ID 不变

- 优点：代码简单，对现有架构零侵入。
- 缺点：discovery 实际不显示任何模型，等同于没做。
- 拒绝原因：没有真实用户体验收益。

### Option B: 注入变量 + 修改 `/anthropic/v1/models` 返回 `claude-*` 前缀 ID

- 优点：用户能在 `/model` picker 里看到 llm-proxy 模型。
- 缺点：
  - 需要前后端双向 ID 映射（`/v1/models` 输出 fake ID，请求 `/v1/messages` 时再还原）。
  - 破坏 `frontend model id` 作为能力契约的稳定性原则。
  - 与 `availableModels`、`ANTHROPIC_CUSTOM_MODEL_OPTION` 等机制交互复杂。
- 拒绝原因：收益不值得引入 model ID 伪造和额外复杂度。

## Consequences

- `launch claude-code` 继续只写入 base URL、auth token 和模型槽位。
- 用户如需更多模型选择，使用 role alias flag（`--sonnet`/`--opus`/`--haiku`）或手动配置 `ANTHROPIC_CUSTOM_MODEL_OPTION`。
- 保留以后接入的可能性：当 Claude Code 放宽 discovery 过滤器（不再限制 `claude`/`anthropic` 前缀），或 llm-proxy 决定承担 ID 映射成本时，可重新评估。
