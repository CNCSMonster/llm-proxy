# Status Probe 性能修复 Spec

## 目标

修复 `status --probe` 的性能和 UX 问题，达到用户可接受的性能指标。

## 问题描述

### 当前问题
1. **串行探测**：`probe_all_inactive` 四层嵌套串行循环（600+ 组合逐个 await），总时长可达数分钟
2. **阻塞等待**：`status_probe` 等 server 全部探测完才响应，CLI 阻塞 60s+ 无反馈
3. **一次性输出**：探测过程不可见，等全部完成才显示结果
4. **错误消息模糊**：`failed to reach admin API` 无上下文、无建议

### 用户体感
- 等 60s+ 无反馈，以为卡死
- 超时失败，不知道怎么办
- 无法感知探测进度

## 修复目标

### 性能指标（SLA）
| 指标 | 目标 | 当前 | 差距 |
|------|------|------|------|
| 首条探测结果显示 | ≤ 3s | >60s（超时） | 20x+ |
| 全部探测完成 | ≤ 30s | >5min（串行） | 10x+ |
| 中间反馈 | 每 5-10s 至少一条 | 无 | 不可接受 |
| 输出结构 | 流式（边探测边显示） | 阻塞（等全部完成） | 不可接受 |

### UX 目标
- ✅ 执行后立即看到反馈（≤3s）
- ✅ 探测过程实时显示（每完成一个立即输出）
- ✅ 进度提示（`[3/11] 探测 deepseek...`）
- ✅ 错误消息清晰、可操作

## 修复方案

### 1. 服务端并发（probe_coordinator.rs）

**改动**：`probe_all_inactive` 改为并发探测

```rust
pub async fn probe_all_inactive(&self, cfg, client, active) -> Vec<String> {
    // 收集所有 (provider, model, protocol) 组合
    let mut tasks = Vec::new();
    for provider_id in cfg.providers.keys() {
        if active.contains(provider_id) { continue; }
        for (model_id, model) in &cfg.models {
            for protocol in CLIENT_PROTOCOLS {
                for binding in model.provider_bindings(protocol) {
                    if binding.name == *provider_id {
                        tasks.push((provider_id, model_id, protocol));
                    }
                }
            }
        }
    }
    
    // 并发执行（并发度 8）
    let results = futures::stream::iter(tasks)
        .map(|(p, m, proto)| async move {
            let outcome = self.probe(p, m, cfg, client).await;
            (p, m, proto, outcome)
        })
        .buffer_unordered(8)
        .collect::<Vec<_>>()
        .await;
    
    // 返回 probed 列表
    results.into_iter()
        .filter(|(_, _, _, outcome)| outcome.executed)
        .map(|(p, _, _, _)| p.to_string())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect()
}
```

**效果**：总时长从数分钟 → 最慢单探测（~30s），通常 <10s

### 2. 协议流式（admin.rs + admin_client.rs）

**改动**：`status_probe` handler 改 SSE 流式响应

**服务端**（admin.rs）：
```rust
pub async fn status_probe(
    State(state): State<AppState>,
) -> Sse<impl Stream<Item = Event>> {
    let (tx, rx) = tokio::sync::mpsc::channel(32);
    
    // 后台任务：并发探测 + 边探测边发结果
    tokio::spawn(async move {
        let cfg = state.core.lock().await.config().clone();
        let client = reqwest::Client::new();
        let active = state.active_providers.active_list();
        
        let mut stream = state.probe_coordinator.probe_stream(&cfg, &client, &active);
        while let Some((provider, model, protocol, outcome)) = stream.next().await {
            let event = Event::default()
                .event("probe_result")
                .data(json!({
                    "provider": provider,
                    "model": model,
                    "protocol": protocol,
                    "ok": outcome.result.is_ok(),
                    "latency_ms": outcome.result.latency_ms(),
                }));
            if tx.send(event).await.is_err() { break; }
        }
        
        // 完成事件
        let done = Event::default()
            .event("done")
            .data(json!({"probed": 11}));
        let _ = tx.send(done).await;
    });
    
    Sse::new(ReceiverStream::new(rx))
}
```

**客户端**（admin_client.rs）：
```rust
pub async fn status_probe_stream(&self) -> Result<impl Stream<Item = Event>> {
    let resp = self.client
        .post(format!("{}/admin/status/probe", self.base_url))
        .header("Accept", "text/event-stream")
        .send()
        .await?;
    
    Ok(resp.bytes_stream().map(|b| parse_sse_event(b)))
}
```

**CLI 渲染**（status.rs）：
```rust
// status --probe 流式渲染
let mut stream = conn.status_probe_stream().await?;
let mut count = 0;
while let Some(event) = stream.next().await {
    match event.event.as_str() {
        "probe_result" => {
            let data: ProbeResult = serde_json::from_value(event.data)?;
            let badge = if data.ok { "✓" } else { "✗" };
            let latency = data.latency_ms.map(|l| format!("{}ms", l)).unwrap_or_default();
            println!("  {} {} {} {} {}", badge, data.model, data.protocol, data.provider, latency);
            count += 1;
        }
        "done" => {
            println!("  ✓ {}/{} 探测完成", count, count);
            break;
        }
        _ => {}
    }
}
```

### 3. 错误消息改进

**当前**：`failed to reach admin API`

**改进**：
- `✗ Server 探活失败: timeout 60s (server 探测 600+ 组合超时)`
- `建议：检查 server 状态或增加 timeout`

## 验收标准

### 性能
- [ ] 首条探测结果显示 ≤3s
- [ ] 全部探测完成 ≤30s
- [ ] 中间反馈：每 5-10s 至少一条

### UX
- [ ] 探测过程实时显示（流式）
- [ ] 进度提示（`[3/11]`）
- [ ] 错误消息清晰、可操作

### 测试
- [ ] 单元测试：并发探测逻辑
- [ ] E2E 测试：`status --probe` 实际执行
- [ ] 性能测试：实际耗时 ≤30s

## 实现步骤

1. **probe_coordinator.rs**：`probe_all_inactive` 并发化
2. **admin.rs**：`status_probe` handler 改 SSE
3. **admin_client.rs**：`status_probe_stream` 读 SSE 流
4. **status.rs**：probe 分支流式渲染
5. **测试**：单元测试 + E2E 验证
6. **Review**：codex review + 人工 review
7. **Commit**

## 风险

- SSE 流式实现复杂度较高（需要处理连接中断、超时）
- 并发探测可能触发上游限流（需要控制并发度）
- 错误处理需要覆盖各种边界情况

## 回滚方案

如果修复引入新问题，可以回滚到串行版本（保留并发化，回滚 SSE）。
