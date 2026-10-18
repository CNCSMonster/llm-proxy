# Changelog

## [0.2.0] - 2026-10-18

### 新增
- `llm-proxy launch` 一键接入 Pi、Qwen Code、Claude Code、Claude Desktop、OpenCode、Codex
- client-only 远程模式（`[remote]`）：客户端只消费模型与协议能力，无需本地 Provider 配置
- 模型目录契约与协议可用性视图；client-only `status` 只显示模型与协议状态
- 六客户端凭证引用（环境变量 / 助手命令），避免字面 token 落盘

### 变更
- 大重构后的 Rust v2 代码基线
- Provider 管理读接口需要管理员权限
- 公开文档精简：移除内部路线图、ADR、开发 SOP 与索引

### 修复
- 远端 client-config 对未知 Provider 的容错
- client-only 模式不再因单个 Provider 不支持而整体失败

## [0.1.0] - 2026-08-09

### 新增
- 三协议互转（OpenAI Chat ↔ Responses ↔ Anthropic Messages）
- 多 Provider 自动 Fallback
- OAuth 设备码登录
- TUI 交互式配置界面
- 内置 30+ 模型目录
- Token 用量统计与 SQLite 持久化
- GitHub Actions CI（macOS/Windows/Linux）
