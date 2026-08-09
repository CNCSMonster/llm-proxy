//! Usage TUI state model.

use chrono::{DateTime, Local};

use crate::usage_stats::UsageRecord;

/// Current view mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
// variant 名保持 By* 前缀：与 CLI 的 ViewArg（by-model 等 kebab 值）语义对齐，
// 且 TUI 显示与切换逻辑依赖该命名，重命名收益低、改动面大，故保留并 allow。
#[allow(clippy::enum_variant_names)]
pub enum ViewMode {
    ByModel,
    ByProvider,
    ByEndpoint,
    ByHour,
    ByDay,
}

impl ViewMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            ViewMode::ByModel => "By Model",
            ViewMode::ByProvider => "By Provider",
            ViewMode::ByEndpoint => "By Endpoint",
            ViewMode::ByHour => "By Hour",
            ViewMode::ByDay => "By Day",
        }
    }

    pub fn cycle(self) -> Self {
        match self {
            ViewMode::ByModel => ViewMode::ByProvider,
            ViewMode::ByProvider => ViewMode::ByEndpoint,
            ViewMode::ByEndpoint => ViewMode::ByHour,
            ViewMode::ByHour => ViewMode::ByDay,
            ViewMode::ByDay => ViewMode::ByModel,
        }
    }
}

/// Current time period filter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Period {
    Today,
    Last7Days,
    Last30Days,
    AllTime,
}

impl Period {
    pub fn as_str(&self) -> &'static str {
        match self {
            Period::Today => "Today",
            Period::Last7Days => "Last 7 days",
            Period::Last30Days => "Last 30 days",
            Period::AllTime => "All time",
        }
    }

    pub fn cycle(self) -> Self {
        match self {
            Period::Today => Period::Last7Days,
            Period::Last7Days => Period::Last30Days,
            Period::Last30Days => Period::AllTime,
            Period::AllTime => Period::Today,
        }
    }

    /// Convert to (start_timestamp, end_timestamp) in Unix seconds.
    pub fn to_range(self) -> (Option<i64>, Option<i64>) {
        let now = Local::now();
        let today_start = now.date_naive().and_hms_opt(0, 0, 0).unwrap();
        match self {
            Period::Today => (Some(today_start.and_utc().timestamp()), None),
            Period::Last7Days => {
                let start = (today_start - chrono::Duration::days(7))
                    .and_utc()
                    .timestamp();
                (Some(start), None)
            }
            Period::Last30Days => {
                let start = (today_start - chrono::Duration::days(30))
                    .and_utc()
                    .timestamp();
                (Some(start), None)
            }
            Period::AllTime => (None, None),
        }
    }
}

/// Aggregated row for display.
#[derive(Debug, Clone)]
pub struct UsageRow {
    pub label: String,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub total_tokens: i64,
    pub request_count: i64,
}

/// TUI state.
pub struct UsageTuiState {
    pub view_mode: ViewMode,
    pub period: Period,
    pub rows: Vec<UsageRow>,
    pub cursor: usize,
    pub total_tokens: i64,
    pub total_requests: i64,
    pub filter_provider: Option<String>,
    pub filter_model: Option<String>,
    pub show_json: bool,
    /// 过滤后的原始记录（detail view 数据源）
    pub all_records: Vec<UsageRecord>,
    /// 当前选中行展开的明细记录
    pub detail_records: Vec<UsageRecord>,
    /// detail view 是否打开
    pub detail_open: bool,
}

impl UsageTuiState {
    pub fn new() -> Self {
        let mut state = Self {
            view_mode: ViewMode::ByModel,
            period: Period::Last7Days,
            rows: Vec::new(),
            cursor: 0,
            total_tokens: 0,
            total_requests: 0,
            filter_provider: None,
            filter_model: None,
            show_json: false,
            all_records: Vec::new(),
            detail_records: Vec::new(),
            detail_open: false,
        };
        state.load_data();
        state
    }

    /// Load usage data from the store.
    pub fn load_data(&mut self) {
        let config_path = crate::config::default_config_path();
        let usage_config = if config_path.exists() {
            crate::config::Config::load(&config_path)
                .map(|cfg| cfg.server.usage)
                .unwrap_or_default()
        } else {
            crate::config::UsageConfig::default()
        };

        let store = match crate::usage_stats::UsageStore::new(usage_config) {
            Ok(s) => s,
            Err(_) => return,
        };

        let (start, end) = self.period.to_range();
        let start_dt = start.and_then(|ts| DateTime::from_timestamp(ts, 0));
        let end_dt = end.and_then(|ts| DateTime::from_timestamp(ts, 0));

        let mut records = store.get_by_period(start_dt, end_dt);

        // Apply filters
        if let Some(ref provider) = self.filter_provider {
            records.retain(|r| &r.provider == provider);
        }
        if let Some(ref model) = self.filter_model {
            records.retain(|r| &r.model == model);
        }

        // Detail view 数据源与行统计保持一致
        self.all_records = records.clone();
        self.detail_records.clear();
        self.detail_open = false;

        // Aggregate
        self.total_tokens = records.iter().map(|r| r.total_tokens).sum();
        self.total_requests = records.len() as i64;

        use std::collections::HashMap;
        let mut groups: HashMap<String, UsageRow> = HashMap::new();

        for r in &records {
            let key = Self::group_key(&self.view_mode, r);

            let entry = groups.entry(key.clone()).or_insert_with(|| UsageRow {
                label: key,
                input_tokens: 0,
                output_tokens: 0,
                total_tokens: 0,
                request_count: 0,
            });
            entry.input_tokens += r.input_tokens;
            entry.output_tokens += r.output_tokens;
            entry.total_tokens += r.total_tokens;
            entry.request_count += 1;
        }

        self.rows = groups.into_values().collect();
        self.rows.sort_by_key(|b| std::cmp::Reverse(b.total_tokens));
        self.cursor = self.cursor.min(self.rows.len().saturating_sub(1));
    }

    /// 计算记录在当前 view mode 下的分组 key。
    fn group_key(view_mode: &ViewMode, r: &UsageRecord) -> String {
        match view_mode {
            ViewMode::ByModel => r.model.clone(),
            ViewMode::ByProvider => r.provider.clone(),
            ViewMode::ByEndpoint => r.endpoint.clone(),
            ViewMode::ByHour => r
                .parsed_timestamp()
                .map(|dt| dt.format("%Y-%m-%d %H:00").to_string())
                .unwrap_or_else(|| "unknown".to_string()),
            ViewMode::ByDay => r
                .date()
                .map(|d| d.format("%Y-%m-%d").to_string())
                .unwrap_or_else(|| "unknown".to_string()),
        }
    }

    pub fn move_up(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn move_down(&mut self) {
        if !self.rows.is_empty() {
            self.cursor = (self.cursor + 1).min(self.rows.len() - 1);
        }
    }

    pub fn cycle_view(&mut self) {
        self.view_mode = self.view_mode.cycle();
        self.load_data();
    }

    pub fn cycle_period(&mut self) {
        self.period = self.period.cycle();
        self.load_data();
    }

    pub fn toggle_filter(&mut self) {
        // 在 provider filter 候选间循环：None → p1 → p2 → ... → None
        let config_path = crate::config::default_config_path();
        let mut providers: Vec<String> = if config_path.exists() {
            crate::config::Config::load(&config_path)
                .map(|cfg| cfg.providers.keys().cloned().collect())
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        providers.sort();

        let next = match &self.filter_provider {
            None => providers.first().cloned(),
            Some(current) => match providers.iter().position(|p| p == current) {
                Some(i) if i + 1 < providers.len() => providers.get(i + 1).cloned(),
                _ => None,
            },
        };
        self.filter_provider = next;
        self.load_data();
    }

    pub fn toggle_json(&mut self) {
        self.show_json = !self.show_json;
    }

    pub fn show_details(&mut self) {
        if self.rows.is_empty() {
            return;
        }
        if self.detail_open {
            self.detail_records.clear();
            self.detail_open = false;
            return;
        }
        let label = &self.rows[self.cursor].label;
        self.detail_records = self
            .all_records
            .iter()
            .filter(|r| Self::group_key(&self.view_mode, r) == *label)
            .cloned()
            .collect();
        self.detail_open = !self.detail_records.is_empty();
    }
}
