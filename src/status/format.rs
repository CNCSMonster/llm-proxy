use std::io::IsTerminal;

use super::display::format_context_window;
use crate::config::Protocol;

pub fn probe_key(model_id: &str, provider_id: &str, protocol: Protocol) -> String {
    format!("{model_id}:{provider_id}:{}", protocol.route_key())
}

#[allow(dead_code)]
pub(super) fn is_success(status: reqwest::StatusCode) -> bool {
    status.is_success()
}

pub(super) fn age_label(checked_at_unix: u64) -> String {
    let now = super::cache::unix_now();
    if checked_at_unix > now {
        return format!("checked_at={checked_at_unix}");
    }
    let age = now - checked_at_unix;
    if age < 60 {
        format!("{age}s ago")
    } else if age < 60 * 60 {
        format!("{}m ago", age / 60)
    } else if age < 60 * 60 * 24 {
        format!("{}h ago", age / 60 / 60)
    } else {
        format!("{}d ago", age / 60 / 60 / 24)
    }
}

pub(super) fn latency_rating(ms: u64) -> (&'static str, fn(&str) -> String, &'static str) {
    match ms {
        0..=200 => ("●", green, "极快"),
        201..=1000 => ("◆", yellow, "正常"),
        1001..=3000 => ("▲", yellow, "偏慢"),
        _ => ("✖", red, "超时"),
    }
}

pub(super) fn format_latency(ms: u64) -> String {
    let (symbol, color_fn, label) = latency_rating(ms);
    format!("{} {}ms {}", color_fn(symbol), ms, dim(label))
}

/// 1. 格式化 provider 状态显示
#[allow(dead_code)]
pub fn format_provider_status(provider_id: &str, state: &str, auth_summary: &str) -> String {
    format!("Provider {provider_id}: [{state}] {auth_summary}")
}

/// 2. 格式化 model 状态显示
#[allow(dead_code)]
pub fn format_model_status(model_id: &str, context_window: i64, max_output_tokens: i64) -> String {
    format!(
        "{model_id} (ctx: {}, max_out: {})",
        format_context_window(context_window),
        format_context_window(max_output_tokens)
    )
}

/// 3. 计算健康状态
#[allow(dead_code)]
pub fn calculate_health_status(
    ok_count: usize,
    total_count: usize,
    has_cooldown: bool,
) -> &'static str {
    if total_count == 0 {
        "UNKNOWN"
    } else if ok_count == total_count && !has_cooldown {
        "OK"
    } else if ok_count > 0 {
        "WARN"
    } else {
        "FAIL"
    }
}

/// 4. 解析 cooldown 原因
#[allow(dead_code)]
pub fn parse_cooldown_reason(reason: &str) -> String {
    if reason.is_empty() {
        "unknown reason".to_string()
    } else if reason.contains("429") {
        "rate limit exceeded (429)".to_string()
    } else if reason.contains("500") || reason.contains("502") || reason.contains("503") {
        "upstream server error".to_string()
    } else if reason.contains("timeout") || reason.contains("timed out") {
        "request timeout".to_string()
    } else {
        reason.to_string()
    }
}

/// 5. 格式化时间 duration
#[allow(dead_code)]
pub fn format_duration(seconds: u64) -> String {
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        let mins = seconds / 60;
        let secs = seconds % 60;
        if secs == 0 {
            format!("{mins}m")
        } else {
            format!("{mins}m {secs}s")
        }
    } else {
        let hours = seconds / 3600;
        let mins = (seconds % 3600) / 60;
        if mins == 0 {
            format!("{hours}h")
        } else {
            format!("{hours}h {mins}m")
        }
    }
}

/// 6. 格式化字节大小
#[allow(dead_code)]
pub fn format_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;

    if bytes >= GB {
        format!("{:.2} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.2} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.2} KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} B")
    }
}

/// 7. 字符串截断
#[allow(dead_code)]
pub fn truncate_string(s: &str, max_len: usize) -> String {
    if s.chars().count() <= max_len {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max_len).collect();
        format!("{truncated}...")
    }
}

/// 8. 判断是否应该跳过 probe
#[allow(dead_code)]
pub fn should_skip_probe(
    in_cooldown: bool,
    last_checked_unix: Option<u64>,
    now_unix: u64,
    min_interval_secs: u64,
) -> bool {
    if in_cooldown {
        return true;
    }
    if let Some(last_checked) = last_checked_unix
        && now_unix.saturating_sub(last_checked) < min_interval_secs
    {
        return true;
    }
    false
}

/// 9. 获取认证状态摘要
#[allow(dead_code)]
pub fn get_auth_status_summary(auth_type: &str, is_set: bool, is_expired: bool) -> &'static str {
    if !is_set {
        "MISSING"
    } else if is_expired {
        "EXPIRED"
    } else if auth_type == "none" {
        "NO_AUTH"
    } else {
        "VALID"
    }
}

/// 10. 格式化错误消息
#[allow(dead_code)]
pub fn format_error_message(http_status: Option<u16>, err_msg: Option<&str>) -> String {
    let msg = err_msg.filter(|s| !s.is_empty());
    match (http_status, msg) {
        (Some(status), Some(msg)) => format!("HTTP {status}: {msg}"),
        (Some(status), None) => format!("HTTP {status}"),
        (None, Some(msg)) => msg.to_string(),
        (None, None) => "unknown error".to_string(),
    }
}

pub(super) fn heading(text: &str) -> String {
    bold(&cyan(text))
}

pub(super) fn section(text: &str) -> String {
    bold(&blue(text))
}

pub(super) fn label(text: &str) -> String {
    bold(text)
}

pub(super) fn badge(state: &str) -> String {
    match state {
        "OK" => green("✓ OK"),
        "WARN" => yellow("! WARN"),
        "FAIL" => red("✗ FAIL"),
        "MISS" => dim("? MISS"),
        other => other.to_string(),
    }
}

pub(super) fn auth_label(state: &str, text: &str) -> String {
    match state {
        "OK" => green(text),
        "WARN" => yellow(text),
        "FAIL" => red(text),
        _ => text.to_string(),
    }
}

pub(super) fn bold(text: &str) -> String {
    style(text, "1")
}

pub(super) fn dim(text: &str) -> String {
    style(text, "2")
}

pub(super) fn red(text: &str) -> String {
    style(text, "31")
}

pub(super) fn green(text: &str) -> String {
    style(text, "32")
}

pub(super) fn yellow(text: &str) -> String {
    style(text, "33")
}

pub(super) fn blue(text: &str) -> String {
    style(text, "34")
}

pub(super) fn cyan(text: &str) -> String {
    style(text, "36")
}

pub(super) fn style(text: &str, code: &str) -> String {
    if color_enabled() {
        format!("\x1b[{code}m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

pub(super) fn color_enabled() -> bool {
    if std::env::var_os("NO_COLOR").is_some() {
        return false;
    }
    std::env::var_os("CLICOLOR_FORCE").is_some_and(|value| value != "0")
        || std::io::stdout().is_terminal()
}

pub(super) fn protocol_label(protocol_key: &str) -> &str {
    match protocol_key {
        "responses" => "responses",
        "chat_completions" => "chat",
        "anthropic" => "anthropic",
        _ => protocol_key,
    }
}
