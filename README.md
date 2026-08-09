# llm-proxy

本地 LLM 协议聚合代理，将多个上游 LLM 服务聚合到统一的本地端点。

## 功能特性

- **三协议互转**：OpenAI Chat ↔ Responses ↔ Anthropic Messages
- **多 Provider 自动 Fallback**：按优先级自动切换
- **OAuth 设备码登录**：支持 OpenAI、Google Antigravity
- **TUI 交互式配置界面**：可视化管理 Provider 和 Model
- **内置 30+ 模型目录**：覆盖主流 coding 场景
- **Token 用量统计**：本地 SQLite 持久化

## 快速开始

### 1. 安装

```bash
# 从源码
cargo install --path .

# 或下载预编译二进制（见 Releases）
```

### 2. 初始化配置

```bash
llm-proxy init
```

### 3. 设置 API Key

```bash
# DeepSeek（示例）
export DEEPSEEK_API_KEY="sk-your-key-here"
```

### 4. 启动代理

```bash
llm-proxy serve
```

### 5. 配置客户端

```bash
# Codex CLI
llm-proxy launch codex

# Qwen Code
llm-proxy launch qwen-code

# pi
llm-proxy launch pi
```

### 6. 使用

配置完成后，直接使用你的 AI 编码工具即可，所有请求自动通过 llm-proxy 转发。

## 支持的 Provider

| 平台 | 产品 | 协议 |
|------|------|------|
| DeepSeek | 按量付费 | Chat, Responses, Anthropic |
| OpenAI | 按量付费 / ChatGPT 订阅 | Chat, Responses, Anthropic |
| Anthropic | 按量付费 | Anthropic |
| Google Antigravity | OAuth | Chat, Responses, Anthropic |
| OpenRouter | 按量付费 | Chat, Responses, Anthropic |
| Kimi | 平台 / 订阅 | Chat, Responses, Anthropic |
| MiMo (小米) | 按量付费 / Token Plan | Chat, Responses, Anthropic |
| 智谱 | 按量付费 / Coding Plan | Chat, Responses, Anthropic |
| 百炼 (阿里) | 按量付费 / Coding Plan | Chat, Responses, Anthropic |
| StepFun | 按量付费 / Step Plan | Chat, Responses, Anthropic |
| Ollama | 本地 | Chat |

## 常用命令

```bash
# 查看状态
llm-proxy status

# 探测上游 Provider
llm-proxy status --probe

# 查看用量
llm-proxy usage

# TUI 管理界面
llm-proxy provider

# 添加 Provider
llm-proxy connect

# 查看帮助
llm-proxy --help
```

## 文档

- [用户手册](docs/user_guide/cli-guide.md) — CLI 命令完整参考
- [TUI 使用指南](docs/user_guide/tui-guide.md) — 交互式界面快捷键
- [故障排查](docs/user_guide/troubleshooting.md) — 常见问题
- [OAuth 账户管理](docs/user_guide/user-guide-oauth-accounts.md) — 登录流程
- [路线图](ROADMAP.md) — 版本规划

## 从源码构建

```bash
git clone https://github.com/CNCSMonster/llm-proxy.git
cd llm-proxy
cargo build --release
cargo install --path .
```

## 许可证

[LICENSE](LICENSE)
