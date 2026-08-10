# ADR-020: Antigravity `trainable=NEVER` 决策废弃

## Status

Superseded（2026-07-16）

## Date

2026-07-14

## Context

早期调研从 agy 二进制字符串中观察到类似 protobuf 的 `Chunk_Trainable` 定义：

```protobuf
enum Chunk_Trainable {
    UNKNOWN_TRAINABLE = 0;
    FORMATTER_DEFINED = 1;
    ALWAYS = 2;
    NEVER = 3;
}
```

当时推断 `GeminiPart` 可能支持 JSON 字段 `trainable`，并希望通过在每个 part 上发送 `"trainable": "NEVER"` 来表达“不用于训练”。

后续实机验证推翻了这个推断：

- llm-proxy 发送 `trainable` 时，Antigravity `generateContent` 返回 HTTP 400：`Unknown name "trainable" at request.contents[0].parts[0]`。
- mitmproxy 抓取 AGY 原生 `streamGenerateContent` 请求，确认 AGY 当前也不发送 `trainable` 字段。

## Decision

废弃“Antigravity 请求强制设置 `trainable=NEVER`”决策。

当前实现：

- `GeminiPart` 不定义 `Trainable` 字段。
- Anthropic → Antigravity、OpenAI Responses → Antigravity 转换均不发送 `trainable`。
- 数据使用/训练 opt-out 视为 Antigravity 账号或用户设置流程，不属于每次模型请求的 `GeminiPart` 字段。

## Consequences

- ✅ Antigravity 请求结构与 AGY 原生请求对齐。
- ✅ 避免服务端因 unknown field 返回 HTTP 400。
- ⚠️ llm-proxy 不再尝试通过模型请求体控制 Google/Antigravity 的数据使用政策。
- ⚠️ 如果未来要支持数据使用 opt-out，应单独调研 `loadCodeAssist` / `fetchUserInfo` / `setUserSettings`，不要在 `generateContent` 请求体中自造字段。

## Verification

- [x] 实机请求带 `trainable` 返回 HTTP 400：`Unknown name "trainable" at request.contents[0].parts[0]`。
- [x] mitmproxy 抓取 AGY `streamGenerateContent` 请求，确认原生请求不包含 `trainable`。
- [x] 回归测试确认 Antigravity 转换输出不包含 `trainable`。
