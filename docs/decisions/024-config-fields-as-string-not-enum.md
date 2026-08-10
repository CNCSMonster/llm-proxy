# ADR-024: 配置字段保持 Option<String> 而非闭合枚举

- 日期：2026-08-09
- 状态：已接受
- 决策者：用户 + 开发者讨论

## 上下文

`CompatConfig` 中的 `thinking_format`、`max_tokens_field` 等字段，设计文档 §7 建议使用 Rust 闭合枚举（如 `ThinkingFormat::DeepseekThinking`），代码中实际使用 `Option<String>` + `effective_*` accessor + 白名单校验。

## 决策

**保持 `Option<String>` 方案，不改为闭合枚举。**

## 理由

1. **上游格式变化频繁**：DeepSeek、OpenAI、Google、Kimi 等各有不同的 thinking 格式，新格式随时可能出现
2. **用户体验**：闭合枚举会导致 serde 反序列化失败 → 整个 config 加载失败 → 代理无法启动；`Option<String>` 允许未知值透传，平滑降级
3. **收益有限**：配置文件中的值来自用户手写（不是代码生成），编译期类型安全的收益不大
4. **已有兜底**：`validate_thinking_format` 等白名单校验在运行时捕获非法值

## 风险

- 运行时才会发现非法值（而非编译期）
- IDE 无自动补全

## 缓解

- 白名单校验覆盖所有已知格式
- 未来若行业格式标准化，可再改为枚举

## 相关文件

- `src/config.rs` — CompatConfig 定义
- `spec.md` §7 — 设计文档
