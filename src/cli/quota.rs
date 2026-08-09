//! `llm-proxy quota` 命令：查询订阅类 provider 的额度。

use std::path::Path;

use anyhow::Result;

use super::types::QuotaArgs;
use crate::quota::{self, QuotaInfo};

/// Run the quota command: query subscription quota for all OAuth providers.
pub async fn run_quota(config_path: &Path, args: QuotaArgs) -> Result<()> {
    // 缓存尚未实现（后续接入 state_dir/quota-cache.json）：--refresh 为占位参数，当前 no-op。
    let _ = args.refresh;

    let infos = quota::fetch_quota(config_path).await?;
    if infos.is_empty() {
        println!(
            "No OAuth subscription providers found in config: {}",
            config_path.display()
        );
        return Ok(());
    }
    for info in &infos {
        print_quota_info(info);
        println!();
    }
    Ok(())
}

/// 打印单个 provider 的额度信息。
fn print_quota_info(info: &QuotaInfo) {
    println!("Provider: {}", info.provider_id);
    println!("  Plan:    {}", info.plan_type.as_deref().unwrap_or("N/A"));
    match info.used_percent {
        Some(percent) => println!("  Usage:   {percent:.0}% used"),
        None => println!("  Usage:   N/A (unlimited or not reported)"),
    }
    println!(
        "  Window:  {}",
        info.limit_window_seconds
            .map(format_window)
            .unwrap_or_else(|| "N/A".to_string())
    );
    println!(
        "  Reset:   {}",
        info.reset_at_unix
            .and_then(format_unix_utc)
            .unwrap_or_else(|| "N/A".to_string())
    );
    println!(
        "  Fetched: {}",
        format_unix_utc(info.fetched_at_unix).unwrap_or_else(|| "N/A".to_string())
    );
}

/// 把秒数格式化为人类可读窗口（604800 → "7 days"）。
fn format_window(seconds: i64) -> String {
    if seconds > 0 && seconds % 86_400 == 0 {
        format!("{} days", seconds / 86_400)
    } else if seconds > 0 && seconds % 3_600 == 0 {
        format!("{} hours", seconds / 3_600)
    } else {
        format!("{seconds}s")
    }
}

fn format_unix_utc(unix: i64) -> Option<String> {
    chrono::DateTime::<chrono::Utc>::from_timestamp(unix, 0)
        .map(|dt| dt.format("%Y-%m-%d %H:%M:%S UTC").to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_seconds_formatted_correctly() {
        assert_eq!(format_window(604_800), "7 days");
        assert_eq!(format_window(86_400), "1 days");
        assert_eq!(format_window(3_600), "1 hours");
        assert_eq!(format_window(90), "90s");
        assert_eq!(format_window(0), "0s");
        assert_eq!(format_window(-1), "-1s");
    }

    #[test]
    fn unix_timestamp_formatted_utc() {
        let formatted = format_unix_utc(1_786_184_630).expect("format");
        assert_eq!(formatted, "2026-08-08 10:23:50 UTC");
    }
}
