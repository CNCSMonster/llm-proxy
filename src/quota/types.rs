//! 订阅额度查询的数据结构（跨 provider 标准化）。

use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// 统一的订阅额度信息表示。
///
/// 不同 provider 的额度 API 返回格式不同（ChatGPT 返回限流窗口百分比，
/// Antigravity 返回 tier / credits），本结构将其标准化，简化 CLI 展示层。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuotaInfo {
    /// 配置中的 provider id（如 "openai-sub"）。
    pub provider_id: String,
    /// 订阅计划类型（如 "plus"、"Gemini Code Assist"）；不可知时为 None。
    pub plan_type: Option<String>,
    /// 主窗口已用百分比（0-100）；无额度概念（如 unlimited）时为 None。
    pub used_percent: Option<f64>,
    /// 限流窗口时长（秒），如 7 天 = 604800。
    pub limit_window_seconds: Option<i64>,
    /// 距窗口重置的剩余秒数。
    pub reset_after_seconds: Option<i64>,
    /// 窗口重置时间（unix 秒）。
    pub reset_at_unix: Option<i64>,
    /// 本次查询时间（unix 秒）。
    pub fetched_at_unix: i64,
}

/// 当前 unix 秒（quota 模块内部使用）。
pub(crate) fn unix_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}
