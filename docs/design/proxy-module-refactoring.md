# proxy.rs 模块拆分重构方案

## 背景

`src/proxy.rs` 当前包含 2186 行代码和 82 个函数，职责过多，难以维护和测试。根据"代码拆分阈值：800行+逻辑整体＝不拆"的原则，需要将其拆分为多个模块。

## 目标

将 `proxy.rs` 拆分为以下结构，每个文件控制在 300-600 行：

```
src/proxy/
├── mod.rs              # 服务器、路由、协议入口（~600 行）
├── forward_chat.rs     # Chat 协议转发逻辑（~400 行）
├── forward_responses.rs # Responses 协议转发逻辑（~500 行）
├── forward_anthropic.rs # Anthropic 协议转发逻辑（~400 行）
├── forward_antigravity.rs # Antigravity 协议转发逻辑（~300 行）
├── sse_aggregate.rs    # SSE 流聚合逻辑（~500 行）
├── request.rs          # 请求解析、验证、reasoning 处理（~300 行）
└── auth.rs             # 认证授权逻辑（~200 行）
```

## 拆分规则

### 1. sse_aggregate.rs
提取以下函数：
- `aggregate_responses_sse_to_value` (行 2009-2456)
- `aggregate_responses_sse_to_json` (行 2488-2514)
- 相关的辅助函数和类型

### 2. forward_chat.rs
提取以下函数：
- `forward_chat_request` (行 1639-1711)
- `forward_chat_via_responses` (行 1711-1785)
- `forward_chat_via_anthropic` (行 1785-1844)

### 3. forward_responses.rs
提取以下函数：
- `forward_responses_via_chat` (行 1844-1913)
- `forward_responses_native` (行 1913-2009)
- `forward_responses_via_antigravity` (行 2514-2583)
- `forward_responses_via_anthropic` (行 2583-2645)

### 4. forward_anthropic.rs
提取以下函数：
- `forward_anthropic_via_antigravity` (行 2645-2715)
- `forward_anthropic_via_responses` (行 2715-2787)
- `forward_anthropic_via_chat` (行 2787-2855)
- `forward_anthropic_native` (行 2855-?)

### 5. forward_antigravity.rs
提取 Antigravity 特有的转发逻辑（如果有的话）

### 6. request.rs
提取以下函数：
- `resolve_request_candidates` (行 1064-1127)
- `missing_capability` (行 1127-1142)
- `requested_reasoning_level` (行 1142-1157)
- `body_with_default_reasoning` (行 1157-1202)
- `body_with_mapped_reasoning` (行 1202-1226)
- `effective_enable_thinking` (行 1226-1241)
- `mapped_reasoning_api_value` (行 1241-1268)
- `set_reasoning_level` (行 1268-1292)
- `body_without_reasoning` (行 1292-1311)
- `request_has_image` (行 1311-1316)
- `request_has_document` (行 1316-1321)
- `content_part_types` (行 1321-1360)

### 7. auth.rs (proxy 模块的 auth.rs，不是顶层的 auth.rs)
提取以下函数：
- `apply_bearer_auth` (行 1360-1373)
- `apply_anthropic_auth` (行 1373-1391)
- `resolve_plan_token` (行 1391-1399)
- `resolve_plan_auth` (行 1399-1459)
- `resolve_oauth_auth` (行 1459-1533)
- `oauth_token_from_guard` (行 1533-1559)

### 8. mod.rs
保留以下函数：
- `serve` (行 498-503)
- `serve_with_shutdown` (行 503-549)
- `router` (行 549-580)
- `list_openai_models` (行 580-594)
- `list_responses_models` (行 594-605)
- `list_anthropic_models` (行 605-616)
- `openai_model_list` (行 616-624)
- `anthropic_model_list` (行 624-637)
- `count_tokens` (行 637-643)
- `collect_text_for_token_count` (行 643-665)
- `rough_token_count` (行 665-669)
- `chat_completions` (行 669-785)
- `responses` (行 785-922)
- `anthropic_messages` (行 922-1064)
- `send_upstream` (行 1559-1600)
- `upstream_send_failure_response` (行 1600-1623)
- `is_local_frequency_limited_response` (行 1623-1630)
- `request_timeout` (行 1630-1639)
- `record_usage_from_responses_value` (行 2456-2488)

## 重构要求

1. **保持所有公共 API 不变**：所有 `pub` 函数和类型必须保持相同的签名
2. **使用 `pub(crate)` 或 `pub(super)` 适当控制可见性**
3. **在 mod.rs 中使用 `pub use` 重新导出需要的类型和函数**
4. **保持所有测试通过**：运行 `cargo test` 确保没有破坏
5. **保持覆盖率不降低**：运行 `cargo tarpaulin` 验证
6. **添加适当的模块文档注释**

## 验证步骤

1. 创建目录结构：`mkdir -p src/proxy`
2. 创建所有新文件
3. 移动函数到对应文件
4. 更新 mod.rs 的模块声明和 re-exports
5. 运行 `cargo build` 确保编译通过
6. 运行 `cargo test` 确保所有测试通过
7. 运行 `cargo tarpaulin` 验证覆盖率
8. 删除旧的 src/proxy.rs 文件

## 注意事项

- 不要修改任何业务逻辑，只做代码移动
- 确保所有 use 语句正确
- 处理循环依赖问题（如果有的话）
- 保持代码格式一致（使用 rustfmt）

## 预期收益

1. **可维护性提升**：每个文件职责单一，易于理解和修改
2. **可测试性提升**：模块边界清晰，便于编写单元测试
3. **代码导航改善**：相关文件组织在一起，易于查找
4. **并行开发支持**：不同模块可以由不同开发者并行开发
5. **符合架构原则**：遵循"800行阈值"原则

## 风险评估

- **低风险**：纯代码移动，不修改业务逻辑
- **测试覆盖**：现有测试应该继续通过
- **编译检查**：Rust 的类型系统会捕获大部分错误
- **回滚方案**：可以通过 git 轻松回滚

## 时间估算

- 代码移动：1-2 小时
- 修复编译错误：1-2 小时
- 测试验证：30 分钟
- 总计：3-4.5 小时
