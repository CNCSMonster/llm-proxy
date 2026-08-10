# ADR-015: OAuth 设备码轮询使用 API interval + 安全余量

## Status

Accepted

## Date

2026-07-04

## Context

OAuth Device Code 流程中，客户端需要轮询 `/api/accounts/deviceauth/token` 检查用户是否已完成浏览器授权。OpenAI API 在 `/api/accounts/deviceauth/usercode` 响应中返回 `interval` 字段，指定推荐的轮询间隔。

初始实现硬编码 2 秒间隔，忽略 API 返回的 `interval`。这导致：
- 若 API 返回 `interval=5`，我们以 2.5 倍频率请求 → 触发 429 限流
- `interval` 字段可能为字符串类型（`"5"`），按数字解析失败 → Interval=0 → 使用默认 2s → 同上
- 紧贴 API 建议间隔请求时，时钟漂移和网络抖动可能使实际到达时间早于预期 → 触发限流

参考项目 cc-switch（`codex_oauth_auth.rs`）采用 `interval + POLLING_SAFETY_MARGIN_SECS(3s)` 的防御性设计，Codex CLI（`device_code_auth.rs`）无额外余量但完全遵循 API interval。

## Decision

轮询间隔 = `max(API interval, 2s) + 3s 安全余量`

关键设计：
- API 返回 `interval` 时使用 `interval + 3s`；未返回时使用默认 `2s`（`2s` 为最小间隔，不会低于此值）
- 安全余量 3 秒参照 cc-switch `POLLING_SAFETY_MARGIN_SECS`，防止时钟漂移和网络抖动导致请求早于服务端预期到达
- `interval` 字段兼容字符串 `"5"` 和数字 `5` 两种 JSON 类型（参照 Codex CLI `deserialize_interval` 的 string→u64 解析）
- `DeviceCodeResponse` 字段名兼容 `user_code`/`usercode` 和 `device_auth_id`/`deviceAuthID` 两种格式（参照 Codex CLI `serde(alias)`）

## Alternatives Considered

### 严格遵循 API interval（无安全余量）

Codex CLI 的做法。优点是与官方行为一致。但 Codex CLI 有完整的客户端重试和限流恢复机制，我们作为代理的轮询失败会直接暴露给用户（整个登录流程中断），代价不对称。

### 更大的安全余量（如 5s 或 10s）

更保守，但用户已打开浏览器等待授权，每多等 1 秒都是摩擦。3s 是合理的平衡——实际测量中，与 API interval 叠加后（5+3=8s），用户体验无明显恶化，而安全边际显著降低 429 概率。

## Consequences

- 防止因硬编码 2s 间隔或忽略服务端 interval 导致的 429 限流
- 字段类型和命名兼容性确保 OpenAI API 实际返回格式（camelCase + string 类型）可正确解析
- 安全余量使我们在请求频率上比 Codex CLI 保守约 60%（8s vs 5s），但换来更低的限流风险
- 规范记入 `docs/spec.md` §2.5
