# Reasoning Cache Miss 来源分析

> 分析日期：2026-05-10
> 数据来源：E2E 长任务（黑白棋 AI），deepseek-v4-flash-lp
> 数据文件：`/tmp/llm-proxy-e2e-logs-20260509-155305-1402867/proxy.log`

## 摘要

| 指标 | 值 |
|------|-----|
| STORE（新缓存条目） | 68 |
| GET（缓存查询） | 3003 |
| GET resolved=true（命中） | 2673 |
| GET resolved=false（未命中） | 330 |
| 缓存命中率 | **89.0%** |
| 客户端显式提供 reasoning（from_input=true） | **0** |

330 个 MISS **100% 为"客户端合成 tool_call，本就无上游 reasoning"**，无 proxy 漏存。

## call_id 前缀分析

| 前缀模式 | 数量 | STORE | 来源 |
|----------|------|-------|------|
| `call_00_*`（不含 ET） | 68 个唯一 ID | 68/68 | DeepSeek 返回的 tool_call，proxy 缓存了 reasoning |
| `call_00_ET_*`（含 ET 段） | 9 个唯一 ID | 0/9 | Codex 客户端合成的 tool_call，从未有 reasoning |

### `call_00_ET_*` 详细

9 个 `call_00_ET_*` 前缀的 call_id 产生了全部 330 个 MISS，每个 MISS 多次、跨多轮重复查询：

| call_id | MISS 次数 |
|---------|----------|
| `call_00_ET_uA9WxMlgLKHPnaxk0Myl4939` | 64 |
| `call_00_ET_CxJTG1w9cGPY9ccRdn1A8137` | 49 |
| `call_00_ET_ASqWyDD3aKIPPjlmlckE7961` | 46 |
| `call_00_ET_VFaMF7yWHtlOI1QMoXG62034` | 40 |
| `call_00_ET_tZNVOifygVRXfIWBNEn04435` | 38 |
| `call_00_ET_Q3xqxVsPF7q1mJY9v0jZ8604` | 33 |
| `call_00_ET_87TtuAqVpnmUOkUK1Gp84215` | 26 |
| `call_00_ET_gFTAkEXSHk5zHQEqvVQP5891` | 19 |
| `call_00_ET_dmWwOCKRsTBYJvXzozCe0166` | 15 |

这些 call_id **从未被 STORE**——说明这些 tool_call 不是 upstream provider 返回的，而是 Codex 在对话历史中自己合成的。

## 分类

| 分类 | 数量 | 占比 | 根因 |
|------|------|------|------|
| **客户端合成无 reasoning** | 330 | 100% | Codex 生成的 `call_00_ET_*` ID，不是 upstream 返回的 |
| **proxy 漏存** | 0 | 0% | — |

## 结论

1. **缓存工作正常**：89% 命中率，所有命中都是正确的缓存恢复
2. **无 proxy 漏存**：所有 STORE 的 call_id 都是上游 DeepSeek 返回的，后续查询全部命中
3. **Codex 合成 tool_call**：`call_00_ET_*` 前缀是 Codex 自己的 ID 格式，这些 tool_call 从未经过上游 provider，所以缓存中不存在是预期的
4. **MISS 不会导致 400**：`&""` 兜底机制确保即使 cache miss，DeepSeek 也不会报 400

## 建议

- **不需要改 proxy 代码**。当前行为正确。
- `call_00_ET_*` 前缀可以在日志中标记为 `[CLIENT_SYNTHESIZED]`，帮助后续分析时快速识别
- ROADMAP 中的"reasoning cache miss 来源分析"任务可以标记完成

---

## 附录：Compaction 频率评估

### 数据

| 指标 | 值 |
|------|-----|
| Compaction 次数 | 43 |
| STORE 次数 | 68 |
| 测试时长 | ~30 分钟 |
| 平均间隔 | ~40 秒 |
| Compaction/STORE 比例 | 63% |

### 观察

- 早期（15:55-16:01）：compaction 间隔 15-30 秒，条目从 13 增长到 40
- 后期（16:04-16:09）：compaction 间隔 2-10 秒，条目稳定在 40 左右
- 43 次 compaction 写入的是逐渐增长的条目（13→47），每次都有实际增量

### 结论

- 当前 compaction 触发阈值为 2x（内存/磁盘比例）
- 在活跃 STORE 阶段，每个新条目几乎都会触发 compaction
- 2741 次磁盘写入中仅 68 次是真正的 STORE，其余 2673 次是滑动过期重写。43 次 compaction / 2741 次写入 ≈ 1.6%，比例合理
- root cause 是滑动过期策略导致每个 GET hit 都写磁盘。如果要减少 compaction，应优化滑动过期（批量延后写入），不是调整 2x 阈值
- **建议：不调整。** 2x 阈值 + 实时触发是正确的语义——在推理过程中，每个新的 reasoning 都应该尽快落盘以保证重启后缓存可用。
