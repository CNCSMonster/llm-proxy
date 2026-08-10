# Token 用量统计功能设计

**状态**: ✅ 已实现（2026-08-01）— 最终方案演进为 [`spec.md`](../../spec.md) §14（Server 内存权威 + 智能落盘 + CLI 委托/独立双模式），本文档保留作为早期设计记录  
**日期**: 2026-07-30  
**作者**: llm-proxy team

## 1. 背景与动机

用户需要追踪和分析模型使用情况，包括：
- 每天/每小时用了多少 token
- 每个模型用了多少 token
- 成本分析和趋势追踪

当前 llm-proxy 只转发请求，不记录用量历史。需要新增用量统计功能，支持按时间维度和模型维度聚合查询。

## 2. 设计目标

1. **数据采集**：在 proxy 层记录每次请求的 token 用量（input_tokens、output_tokens、total_tokens）
2. **多维度统计**：
   - 时间维度：每小时/每日/每周用量
   - 模型维度：每个模型的用量分布
   - Provider 维度（可选）：每个 provider 的用量
3. **灵活查询**：CLI 命令支持时间范围、模型过滤等
4. **外部集成**：支持 JSON 格式输出，方便 web 页面等外部应用集成
5. **性能友好**：不阻塞请求处理，异步写入

## 3. 存储架构

### 3.1 三层存储

```
内存缓存（最近 N 条请求）
    ↓ 定期落盘
明文文件（JSON Lines，2MB 阈值）
    ↓ 超过阈值时，迁移旧数据
SQLite 数据库（50MB 上限）
    ↓ 超过上限时
删除最旧记录
```

### 3.2 存储层级说明

| 层级 | 格式 | 阈值 | 用途 |
|------|------|------|------|
| 内存 | 数据结构 | 最近 N 条 | 快速查询，保持状态一致性 |
| 明文文件 | JSON Lines | 2MB | 近期数据，编辑器友好，方便调试 |
| SQLite | 数据库 | 50MB | 历史数据，结构化查询 |

### 3.3 迁移逻辑

1. **明文文件迁移**：
   - 当 `usage.jsonl` 超过 `file_threshold_mb`（默认 2MB）
   - 迁移最旧的 `migration_ratio`（默认 50%）记录到 SQLite
   - 明文文件保留最新的 50% 记录

2. **SQLite 清理**：
   - 当 `usage.db` 超过 `db_max_size_mb`（默认 50MB）
   - 删除最旧的记录，直到文件大小 < 50MB

### 3.4 文件格式

**明文文件**（`~/.local/state/llm-proxy/usage.jsonl`）：

```json
{"ts":"2026-07-30T14:23:45Z","model":"gpt-4","provider":"openai","input":1234,"output":567,"total":1801,"latency_ms":2345}
{"ts":"2026-07-30T14:22:12Z","model":"claude-3","provider":"anthropic","input":2345,"output":890,"total":3235,"latency_ms":1890}
```

**SQLite 数据库**（`~/.local/state/llm-proxy/usage.db`）：

```sql
CREATE TABLE usage_records (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    timestamp TEXT NOT NULL,
    model TEXT NOT NULL,
    provider TEXT NOT NULL,
    input_tokens INTEGER NOT NULL,
    output_tokens INTEGER NOT NULL,
    total_tokens INTEGER NOT NULL,
    latency_ms INTEGER
);

CREATE INDEX idx_timestamp ON usage_records(timestamp);
CREATE INDEX idx_model ON usage_records(model);
CREATE INDEX idx_provider ON usage_records(provider);
```

## 4. 配置设计

### 4.1 配置位置

配置放在 `[server.usage]` 嵌套结构中：

```toml
[server]
listen = "127.0.0.1:8989"

[server.usage]
file_threshold_mb = 2      # 明文文件阈值（默认 2MB）
migration_ratio = 0.5      # 迁移比例（默认 50%）
db_max_size_mb = 50        # SQLite 上限（默认 50MB）
```

### 4.2 配置项说明

| 配置项 | 类型 | 默认值 | 说明 |
|--------|------|--------|------|
| `file_threshold_mb` | f64 | 2.0 | 明文文件大小阈值（MB），超过后触发迁移 |
| `migration_ratio` | f64 | 0.5 | 每次迁移的比例（0.0-1.0） |
| `db_max_size_mb` | f64 | 50.0 | SQLite 文件最大大小（MB），超过后删除最旧记录 |

### 4.3 配置结构（Rust）

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    pub listen: String,
    #[serde(default)]
    pub usage: UsageConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageConfig {
    #[serde(default = "default_file_threshold_mb")]
    pub file_threshold_mb: f64,
    #[serde(default = "default_migration_ratio")]
    pub migration_ratio: f64,
    #[serde(default = "default_db_max_size_mb")]
    pub db_max_size_mb: f64,
}

fn default_file_threshold_mb() -> f64 { 2.0 }
fn default_migration_ratio() -> f64 { 0.5 }
fn default_db_max_size_mb() -> f64 { 50.0 }
```

## 5. CLI 命令设计

### 5.1 模式选择

**规则**：
- **无参数** → 进入 TUI 交互模式
- **有参数**（如 `--json`、`--period`、`--provider` 等）→ CLI 输出模式

```bash
llm-proxy usage                          # TUI 模式（无参数）
llm-proxy usage --json                   # CLI 模式（JSON 输出）
llm-proxy usage --period 7d --json       # CLI 模式（带筛选）
llm-proxy usage --provider openai --json # CLI 模式（带筛选）
```

### 5.2 筛选参数

| 参数 | 说明 | 示例 |
|------|------|------|
| `--provider <NAME>` | 过滤指定 provider | `--provider openai` |
| `--model <MODEL>` | 过滤指定模型 | `--model gpt-4` |
| `--endpoint <TYPE>` | 过滤服务端点 | `--endpoint openai_chat` |
| `--period <DURATION>` | 时间范围 | `--period 7d`, `--period 1w`, `--period 2026-03-12:2026-03-20` |
| `--json` | JSON 格式输出 | `--json` |
| `--view <MODE>` | 视图模式 | `--view by-model` |

**时间范围格式**：

| 格式 | 说明 | 示例 |
|------|------|------|
| 相对时间 | 最近 N 天/周/月 | `7d`, `1w`, `30d`, `3m` |
| 绝对范围 | 开始日期:结束日期 | `2026-03-12:2026-03-20` |
| 单日期 | 指定日期 | `2026-03-15` |
| 特殊值 | 预定义范围 | `today`, `yesterday`, `this-week`, `last-week`, `this-month`, `last-month` |

### 5.3 CLI 补全设计

**补全实现**：使用 clap 的 `value_parser` + 自定义补全候选列表

```rust
#[derive(Parser)]
pub struct UsageCommand {
    /// Time period (e.g., today, 7d, 2026-03-12:2026-03-20)
    #[arg(short, long, value_parser = parse_period)]
    #[arg(completion_candidates = period_completions)]
    period: Option<String>,
    
    /// Provider filter
    #[arg(long, completion_candidates = provider_completions)]
    provider: Option<String>,
    
    /// Model filter
    #[arg(long, completion_candidates = model_completions)]
    model: Option<String>,
    
    /// Endpoint filter
    #[arg(long, completion_candidates = endpoint_completions)]
    endpoint: Option<String>,
    
    /// View mode
    #[arg(long, completion_candidates = view_completions)]
    view: Option<String>,
    
    /// JSON output
    #[arg(long)]
    json: bool,
}
```

**补全候选列表**：

```rust
fn period_completions() -> Vec<&'static str> {
    vec![
        // 特殊值
        "today", "yesterday", 
        "this-week", "last-week",
        "this-month", "last-month",
        // 相对时间示例
        "1d", "7d", "30d",
        "1w", "4w",
        "1m", "3m",
    ]
}

fn view_completions() -> Vec<&'static str> {
    vec!["by-model", "by-provider", "by-endpoint", "by-hour", "by-day"]
}

fn endpoint_completions() -> Vec<&'static str> {
    vec!["openai_chat", "openai_responses", "anthropic"]
}

// 动态补全：从配置文件读取
fn provider_completions() -> Vec<String> {
    // 读取 config.toml 中的 provider 列表
}

fn model_completions() -> Vec<String> {
    // 读取 config.toml 中的 model 列表
}
```

**时间解析逻辑**：

```rust
fn parse_period(s: &str) -> Result<Period, String> {
    match s {
        // 特殊值
        "today" => Ok(Period::today()),
        "yesterday" => Ok(Period::yesterday()),
        "this-week" => Ok(Period::this_week()),
        "last-week" => Ok(Period::last_week()),
        "this-month" => Ok(Period::this_month()),
        "last-month" => Ok(Period::last_month()),
        
        // 相对时间 (7d, 1w, 30d, 3m)
        s if s.ends_with('d') => parse_relative_days(s),
        s if s.ends_with('w') => parse_relative_weeks(s),
        s if s.ends_with('m') => parse_relative_months(s),
        
        // 绝对范围 (2026-03-12:2026-03-20)
        s if s.contains(':') => parse_date_range(s),
        
        // 单日期 (2026-03-15)
        s => parse_single_date(s),
    }
}
```

**补全效果示例**：

```bash
$ llm-proxy usage --period <TAB>
today           yesterday       this-week       last-week       
this-month      last-month      1d              7d              
30d             1w              4w              1m              
3m              

$ llm-proxy usage --period 7<TAB>
7d

$ llm-proxy usage --period to<TAB>
today

$ llm-proxy usage --provider <TAB>
openai          anthropic       deepseek      bailian       mimo

$ llm-proxy usage --model <TAB>
gpt-4           claude-3        deepseek-v3   qwen-3        mimo-v2

$ llm-proxy usage --endpoint <TAB>
openai_chat     openai_responses    anthropic

$ llm-proxy usage --view <TAB>
by-model        by-provider     by-endpoint   by-hour       by-day
```

### 5.4 视图模式

| 模式 | 说明 |
|------|------|
| `by-model` | 按模型分组（默认），多 provider 的 token 叠加 |
| `by-provider` | 按 provider 分组，内部再按 model 分组 |
| `by-endpoint` | 按服务端点分组（chat/responses/anthropic） |
| `by-hour` | 按小时分组 |
| `by-day` | 按天分组 |

### 5.4 输出格式

**人类可读格式**（默认）：

```
$ llm-proxy usage --period 7d

Token Usage Summary (2026-07-23 to 2026-07-30)
═══════════════════════════════════════════════

Total:
  Input:  1,234,567 tokens
  Output:   234,567 tokens
  Total:  1,469,134 tokens
  Requests: 1,234

By Model:
  gpt-4          456,789 tokens (31%)
  claude-3       345,678 tokens (24%)
  deepseek-v3    234,567 tokens (16%)
  ...

By Day:
  2026-07-30    234,567 tokens
  2026-07-29    198,765 tokens
  2026-07-28    212,345 tokens
  ...
```

**JSON 格式**（`--json`）：

```json
{
  "period": {
    "start": "2026-07-23T00:00:00Z",
    "end": "2026-07-30T23:59:59Z"
  },
  "summary": {
    "input_tokens": 1234567,
    "output_tokens": 234567,
    "total_tokens": 1469134,
    "request_count": 1234
  },
  "by_model": [
    {
      "model": "gpt-4",
      "input_tokens": 456789,
      "output_tokens": 78901,
      "total_tokens": 535690,
      "request_count": 456
    }
  ],
  "by_day": [
    {
      "date": "2026-07-30",
      "input_tokens": 234567,
      "output_tokens": 45678,
      "total_tokens": 280245,
      "request_count": 234
    }
  ]
}
```

## 6. TUI 设计

### 6.1 入口

```bash
llm-proxy usage    # 无参数时进入 TUI 模式
```

### 6.2 筛选维度

| 维度 | 说明 | 示例 |
|------|------|------|
| **Provider** | 指定 provider | `openai`, `anthropic` |
| **Model** | 指定模型 | `gpt-4`, `claude-3` |
| **Endpoint** | 指定服务端点 | `openai_chat`, `openai_responses`, `anthropic` |
| **Time** | 时间范围 | `Today`, `Last 7 days`, `Last 30 days` |

### 6.3 视图模式

| 模式 | 说明 |
|------|------|
| **By Model**（默认） | 按模型分组，多 provider 的 token 叠加 |
| **By Provider** | 按 provider 分组，内部再按 model 分组 |
| **By Endpoint** | 按服务端点分组 |
| **By Hour** | 按小时分组（今日） |
| **By Day** | 按天分组 |

### 6.4 Screen 1: 主面板（By Model，默认）

```
┌─ Token Usage Statistics ─────────────────────────────────────────────────────┐
│  Filter: All Providers | All Endpoints | Last 7 days                        │
│                                                                              │
│  Total: 1,469,134 tokens | 1,234 requests                                   │
│                                                                              │
│  By Model (aggregated across providers)                                      │
│  ─────────────────────────────────────────────────────────────────────────── │
│  Model              Input      Output      Total      Requests    %          │
│  ▶ gpt-4           456,789     78,901     535,690        456    36.5%       │
│    claude-3        345,678     67,890     413,568        345    28.1%       │
│    deepseek-v3     234,567     45,678     280,245        234    19.1%       │
│    qwen-3          123,456     23,456     146,912        123    10.0%       │
│    mimo-v2          74,077     18,642      92,719         76     6.3%       │
│                                                                              │
│  [f] Filter  [v] View Mode  [p] Period  [Enter] Details  [q] Quit          │
└──────────────────────────────────────────────────────────────────────────────┘
```

### 6.5 Screen 2: By Provider 视图（内部按 Model 分组）

```
┌─ Token Usage by Provider ────────────────────────────────────────────────────┐
│  Filter: All Providers | All Endpoints | Last 7 days                        │
│                                                                              │
│  Total: 1,469,134 tokens | 1,234 requests                                   │
│                                                                              │
│  ▼ openai (535,690 tokens, 36.5%)                                            │
│    Model              Input      Output      Total      Requests             │
│    gpt-4             456,789     78,901     535,690        456              │
│                                                                              │
│  ▼ anthropic (413,568 tokens, 28.1%)                                         │
│    Model              Input      Output      Total      Requests             │
│    claude-3          345,678     67,890     413,568        345              │
│                                                                              │
│  ▼ deepseek (280,245 tokens, 19.1%)                                          │
│    Model              Input      Output      Total      Requests             │
│    deepseek-v3       234,567     45,678     280,245        234              │
│                                                                              │
│  [f] Filter  [v] View Mode  [p] Period  [↑/↓] Navigate  [q] Quit           │
└──────────────────────────────────────────────────────────────────────────────┘
```

### 6.6 Screen 3: By Endpoint 视图

```
┌─ Token Usage by Endpoint ────────────────────────────────────────────────────┐
│  Filter: All Providers | All Endpoints | Last 7 days                        │
│                                                                              │
│  Total: 1,469,134 tokens | 1,234 requests                                   │
│                                                                              │
│  Endpoint           Input      Output      Total      Requests    %          │
│  ─────────────────────────────────────────────────────────────────────────── │
│  ▶ openai_chat      823,456    156,789     980,245        823    66.7%       │
│    openai_responses 234,567     45,678     280,245        234    19.1%       │
│    anthropic        345,678     67,890     413,568        345    28.1%       │
│    (other)           65,433     12,345      77,778         65     5.3%       │
│                                                                              │
│  [f] Filter  [v] View Mode  [p] Period  [Enter] Details  [q] Quit          │
└──────────────────────────────────────────────────────────────────────────────┘
```

### 6.7 Screen 4: 筛选器

```
┌─ Filter Usage Data ──────────────────────────────────────────────────────────┐
│                                                                              │
│  Provider:  [All ▼]                                                          │
│             ○ All                                                            │
│             ○ openai                                                         │
│             ○ anthropic                                                      │
│             ○ deepseek                                                       │
│             ○ bailian                                                        │
│             ○ mimo                                                           │
│                                                                              │
│  Model:     [All ▼]                                                          │
│             ○ All                                                            │
│             ○ gpt-4                                                          │
│             ○ claude-3                                                       │
│             ○ deepseek-v3                                                    │
│             ...                                                              │
│                                                                              │
│  Endpoint:  [All ▼]                                                          │
│             ○ All                                                            │
│             ○ openai_chat                                                    │
│             ○ openai_responses                                               │
│             ○ anthropic                                                      │
│                                                                              │
│  Period:    [Last 7 days ▼]                                                  │
│             ○ Today                                                          │
│             ○ Yesterday                                                      │
│             ○ Last 7 days                                                    │
│             ○ Last 30 days                                                   │
│             ○ This week                                                      │
│             ○ Last week                                                      │
│             ○ This month                                                     │
│             ○ Last month                                                     │
│             ○ Custom range...                                                │
│                                                                              │
│  [↑/↓] Navigate  [Enter] Select  [Esc] Cancel  [a] Apply                     │
└──────────────────────────────────────────────────────────────────────────────┘
```

**自定义时间范围输入**（选择 "Custom range..." 后）：

```
┌─ Custom Date Range ──────────────────────────────────────────────────────────┐
│                                                                              │
│  Start date:  [2026-03-12    ]                                               │
│  End date:    [2026-03-20    ]                                               │
│                                                                              │
│  Format: YYYY-MM-DD (e.g., 2026-03-12)                                       │
│                                                                              │
│  [Tab] Switch field  [Enter] Apply  [Esc] Cancel                             │
└──────────────────────────────────────────────────────────────────────────────┘
```

### 6.8 Screen 5: 视图模式选择

```
┌─ Select View Mode ───────────────────────────────────────────────────────────┐
│                                                                              │
│  ▶ By Model (default)        Group by model, aggregate across providers     │
│    By Provider               Group by provider, then by model               │
│    By Endpoint               Group by service endpoint                      │
│    By Hour                   Group by hour (today)                          │
│    By Day                    Group by day                                   │
│                                                                              │
│  [↑/↓] Navigate  [Enter] Confirm  [Esc] Cancel                               │
└──────────────────────────────────────────────────────────────────────────────┘
```

### 6.9 Screen 6: 模型详情（带筛选上下文）

```
┌─ Model Details: gpt-4 ───────────────────────────────────────────────────────┐
│  Filter: All Providers | All Endpoints | Last 7 days                        │
│                                                                              │
│  Summary                                                                     │
│  ─────────────────────────────────────────────────────────────────────────── │
│  Input:      456,789 tokens                                                  │
│  Output:      78,901 tokens                                                  │
│  Total:      535,690 tokens                                                  │
│  Requests:       456                                                         │
│  Avg/Req:    1,174 tokens                                                    │
│                                                                              │
│  By Provider                                                                 │
│  ─────────────────────────────────────────────────────────────────────────── │
│  Provider           Input      Output      Total      Requests               │
│  openai            456,789     78,901     535,690        456                │
│                                                                              │
│  By Endpoint                                                                 │
│  ─────────────────────────────────────────────────────────────────────────── │
│  Endpoint           Input      Output      Total      Requests               │
│  openai_chat       345,678     56,789     402,467        345                │
│  openai_responses  111,111     22,112     133,223        111                │
│                                                                              │
│  Daily Breakdown                                                             │
│  ─────────────────────────────────────────────────────────────────────────── │
│  07-30      78,901     13,456     92,357         78                         │
│  07-29      67,890     11,234     79,124         67                         │
│  ...                                                                         │
│                                                                              │
│  [Esc] Back  [q] Quit                                                        │
└──────────────────────────────────────────────────────────────────────────────┘
```

### 6.10 快捷键总结

| 快捷键 | 功能 |
|--------|------|
| `f` | 打开筛选器 |
| `v` | 切换视图模式 |
| `p` | 选择时间范围 |
| `↑/↓` 或 `j/k` | 导航列表 |
| `Enter` | 查看详情 / 展开折叠 |
| `Esc` | 返回上一级 / 取消 |
| `q` | 退出 |

### 6.11 数据聚合逻辑

**By Model（默认）**：
```
如果 gpt-4 有多个 provider：
  - openai 提供 gpt-4: 300,000 tokens
  - openrouter 提供 gpt-4: 150,000 tokens
  → 聚合显示: gpt-4: 450,000 tokens
```

**By Provider**：
```
显示每个 provider 的总量，内部按 model 分组：
  ▼ openai (535,690 tokens)
      gpt-4: 535,690 tokens
  ▼ openrouter (150,000 tokens)
      gpt-4: 150,000 tokens
```

## 7. 数据采集流程

### 6.1 请求处理流程

```
客户端请求 → proxy 转发 → 上游响应
                ↓
        提取 usage 信息
                ↓
        写入内存缓存
                ↓
        定期落盘到明文文件
```

### 6.2 数据提取

从上游响应中提取 usage 信息：
- OpenAI Chat: `response.usage.prompt_tokens`, `response.usage.completion_tokens`
- OpenAI Responses: `response.usage.input_tokens`, `response.usage.output_tokens`
- Anthropic: `response.usage.input_tokens`, `response.usage.output_tokens`

### 6.3 异步写入

- 内存缓存使用线程安全的数据结构（如 `Arc<Mutex<Vec<UsageRecord>>>`）
- 定期（如每分钟）将内存数据追加到明文文件
- 迁移和清理操作在后台线程执行，不阻塞请求处理

## 7. 文件位置

```
~/.local/state/llm-proxy/
├── usage.jsonl          # 明文文件（近期数据）
└── usage.db             # SQLite 数据库（历史数据）
```

遵循 XDG Base Directory 规范，与现有的 socket、PID、日志文件保持一致。

## 8. 实现计划

### Phase 1: 基础功能（P0）

- [ ] 定义 `UsageRecord` 数据结构
- [ ] 实现内存缓存（线程安全）
- [ ] 实现明文文件写入（JSON Lines）
- [ ] 在 proxy 层提取 usage 信息并记录
- [ ] 实现基础 CLI 命令（`llm-proxy usage`）
- [ ] 支持 `--json` 输出

### Phase 2: 存储迁移（P1）

- [ ] 实现 SQLite 数据库初始化
- [ ] 实现明文文件 → SQLite 迁移逻辑
- [ ] 实现 SQLite 大小限制和清理逻辑
- [ ] 实现配置项（`[server.usage]`）
- [ ] 实现定时落盘和迁移（后台线程）

### Phase 3: 查询增强（P2）

- [ ] 支持时间范围过滤（`--period`）
- [ ] 支持模型过滤（`--model`）
- [ ] 支持 provider 过滤（`--provider`）
- [ ] 实现按时间维度聚合（小时/天/周）
- [ ] 实现按模型维度聚合
- [ ] 支持详细请求记录（`--detailed`）

### Phase 4: 高级功能（P3，可选）

- [ ] TUI 界面（`llm-proxy usage --tui`）
- [ ] 可视化图表
- [ ] 导出功能（CSV/JSON）
- [ ] Provider 维度统计

## 9. 技术考量

### 9.1 性能影响

- 内存写入：O(1)，几乎无影响
- 文件落盘：异步执行，不阻塞请求
- 数据库查询：使用索引，快速聚合

### 9.2 存储成本

- 每条记录约 150-200 字节
- 2MB 明文文件 ≈ 10,000-13,000 条记录
- 50MB SQLite ≈ 250,000-330,000 条记录
- 自动清理确保不会无限增长

### 9.3 准确性

- 从上游响应中提取真实 token 用量
- 如果上游未返回 usage 信息，记录为 0 或估算值
- 流式响应需要在最后一个 chunk 中提取 usage

### 9.4 隐私

- 仅记录 token 数量和时间戳
- 不记录请求内容、响应内容
- 不记录用户身份信息

## 10. 外部集成示例

### 10.1 Web 页面集成

```javascript
// 前端调用 CLI 获取数据
async function fetchUsageData() {
  const result = await exec('llm-proxy usage --period 7d --json');
  const data = JSON.parse(result.stdout);
  
  // 渲染图表
  renderChart(data.by_day);
  renderModelBreakdown(data.by_model);
}
```

### 10.2 定时任务

```bash
# 每天生成报告
0 0 * * * llm-proxy usage --period 1d --json > /var/log/llm-proxy/daily-usage.json
```

### 10.3 监控告警

```bash
# 检查是否超过阈值
USAGE=$(llm-proxy usage --period 1d --json | jq '.summary.total_tokens')
if [ $USAGE -gt 1000000 ]; then
  echo "Warning: Daily usage exceeded 1M tokens"
fi
```

## 11. 开放问题

1. **内存缓存大小**：保留多少条记录在内存中？（建议 1000-5000 条）
2. **落盘频率**：多久落盘一次？（建议每分钟或每 100 条）
3. **错误处理**：如果写入失败（磁盘满、权限问题），如何处理？
4. **并发安全**：多个 proxy 实例同时写入同一文件，如何处理？
5. **数据迁移**：如果用户升级版本，旧格式数据如何迁移？

## 12. 参考

- 类似工具：OpenRouter、LiteLLM 的用量统计功能
- 存储格式：JSON Lines（https://jsonlines.org/）
- SQLite 文档：https://www.sqlite.org/docs.html
