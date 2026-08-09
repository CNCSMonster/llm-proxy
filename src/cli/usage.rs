use super::types::*;
use crate::{admin_client, cache, config, usage_stats};
use anyhow::Result;

/// (start, end) 时间范围。
type TimeRange = (
    Option<chrono::DateTime<chrono::Utc>>,
    Option<chrono::DateTime<chrono::Utc>>,
);
/// 单模型统计 (input, output, total, count)。
type ModelTotals = (i64, i64, i64, usize);
/// provider 统计：总览 (input, output, total, count) + 按模型的细分统计。
type ProviderAgg = (
    i64,
    i64,
    i64,
    usize,
    std::collections::HashMap<String, ModelTotals>,
);

/// Run the usage statistics command
pub async fn run_usage(config_path: &std::path::Path, args: UsageArgs) -> Result<()> {
    // Try server delegation first（§13：版本不兼容时报错退出）
    match admin_client::detect_server(config_path).await {
        Ok(Some(server)) => {
            let result = server
                .usage(
                    args.period.as_deref(),
                    args.provider.as_deref(),
                    args.model.as_deref(),
                    args.endpoint.as_ref().map(|e| e.as_str()),
                )
                .await;
            match result {
                Ok(resp) => {
                    // 写缓存：远程模式 server 不可达时兜底（usage 纳入统一缓存机制）
                    let key = format!("usage:{}", usage_cache_key(&args));
                    let _ = cache::QueryCache::new().save(&key, &resp);
                    print_usage_response(&resp, args.json)?;
                    return Ok(());
                }
                Err(e) => {
                    eprintln!("Server error: {e}");
                    return Err(e);
                }
            }
        }
        Ok(None) => {
            // 远程模式（config 极简）读缓存兜底
            let is_remote = config::Config::load(config_path)
                .map(|cfg| cfg.providers.is_empty() && cfg.models.is_empty())
                .unwrap_or(false);
            if is_remote {
                let key = format!("usage:{}", usage_cache_key(&args));
                let cache = cache::QueryCache::new();
                if let Some((cached_at, resp)) = cache.load(&key) {
                    println!(
                        "ℹ 从缓存获取（{}，可能过期）",
                        cache::QueryCache::format_cached_at(cached_at)
                    );
                    print_usage_response(&resp, args.json)?;
                    return Ok(());
                }
                anyhow::bail!("server unreachable and no cached usage data");
            }
        }
        Err(e) => {
            eprintln!("{e}");
            return Err(e);
        }
    }

    // Fallback: local mode
    run_usage_local(config_path, &args)
}

/// usage 查询缓存 key（区分查询参数，避免串数据）。
fn usage_cache_key(args: &UsageArgs) -> String {
    format!(
        "{}-{}-{}-{}-{}",
        args.period.as_deref().unwrap_or("all"),
        args.provider.as_deref().unwrap_or("*"),
        args.model.as_deref().unwrap_or("*"),
        args.endpoint.as_ref().map(|e| e.as_str()).unwrap_or("*"),
        if args.json { "json" } else { "text" }
    )
}

/// 打印 usage 响应（委托路径与缓存路径共用，保证展示一致）。
fn print_usage_response(result: &serde_json::Value, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(result)?);
    } else {
        if let Some(data) = result.get("data") {
            if let Some(count) = data.get("count").and_then(|v| v.as_i64())
                && count == 0
            {
                println!("No usage records found.");
                return Ok(());
            }
            println!("{}", serde_json::to_string_pretty(data)?);
        }
    }
    Ok(())
}

/// Run usage command locally (no server)
fn run_usage_local(config_path: &std::path::Path, args: &UsageArgs) -> Result<()> {
    use crate::config::UsageConfig;
    use usage_stats::UsageStore;
    let usage_config = if config_path.exists() {
        let cfg = config::Config::load(config_path)?;
        cfg.server.usage.clone()
    } else {
        UsageConfig::default()
    };

    // Create usage store
    let store = UsageStore::new(usage_config)?;

    // Parse time period if provided
    let (start, end) = if let Some(period) = &args.period {
        parse_period(period)?
    } else {
        (None, None)
    };

    // Get records with filters
    let mut records = store.get_by_period(start, end);

    // Apply additional filters
    if let Some(provider) = &args.provider {
        records.retain(|r| &r.provider == provider);
    }
    if let Some(model) = &args.model {
        records.retain(|r| &r.model == model);
    }
    if let Some(endpoint) = &args.endpoint {
        records.retain(|r| r.endpoint == endpoint.as_str());
    }

    // Output based on format
    if args.json {
        output_json(&records, args.view.as_str(), start, end)?;
    } else {
        output_human_readable(&records, args.view.as_str(), start, end)?;
    }

    Ok(())
}

/// Parse period string into start/end datetime range
fn parse_period(period: &str) -> Result<TimeRange> {
    use chrono::{Datelike, Duration, NaiveDate, TimeZone, Utc};

    let now = Utc::now();

    match period {
        "today" => {
            let start = now
                .date_naive()
                .and_hms_opt(0, 0, 0)
                .map(|d| Utc.from_utc_datetime(&d))
                .unwrap();
            let end = now
                .date_naive()
                .and_hms_opt(23, 59, 59)
                .map(|d| Utc.from_utc_datetime(&d))
                .unwrap();
            Ok((Some(start), Some(end)))
        }
        "yesterday" => {
            let yesterday = now.date_naive() - Duration::days(1);
            let start = yesterday
                .and_hms_opt(0, 0, 0)
                .map(|d| Utc.from_utc_datetime(&d))
                .unwrap();
            let end = yesterday
                .and_hms_opt(23, 59, 59)
                .map(|d| Utc.from_utc_datetime(&d))
                .unwrap();
            Ok((Some(start), Some(end)))
        }
        "this-week" => {
            let weekday = now.weekday().num_days_from_monday();
            let start_date = now.date_naive() - Duration::days(weekday as i64);
            let start = start_date
                .and_hms_opt(0, 0, 0)
                .map(|d| Utc.from_utc_datetime(&d))
                .unwrap();
            Ok((Some(start), None))
        }
        "last-week" => {
            let weekday = now.weekday().num_days_from_monday();
            let end_date = now.date_naive() - Duration::days(weekday as i64 + 1);
            let start_date = end_date - Duration::days(6);
            let start = start_date
                .and_hms_opt(0, 0, 0)
                .map(|d| Utc.from_utc_datetime(&d))
                .unwrap();
            let end = end_date
                .and_hms_opt(23, 59, 59)
                .map(|d| Utc.from_utc_datetime(&d))
                .unwrap();
            Ok((Some(start), Some(end)))
        }
        "this-month" => {
            let start_date = now.date_naive().with_day(1).unwrap();
            let start = start_date
                .and_hms_opt(0, 0, 0)
                .map(|d| Utc.from_utc_datetime(&d))
                .unwrap();
            Ok((Some(start), None))
        }
        "last-month" => {
            let first_of_this_month = now.date_naive().with_day(1).unwrap();
            let end_date = first_of_this_month - Duration::days(1);
            let start_date = end_date.with_day(1).unwrap();
            let start = start_date
                .and_hms_opt(0, 0, 0)
                .map(|d| Utc.from_utc_datetime(&d))
                .unwrap();
            let end = end_date
                .and_hms_opt(23, 59, 59)
                .map(|d| Utc.from_utc_datetime(&d))
                .unwrap();
            Ok((Some(start), Some(end)))
        }
        s if s.ends_with('d') => {
            // Relative days: 7d, 30d
            let days: i64 = s[..s.len() - 1]
                .parse()
                .map_err(|_| anyhow::anyhow!("invalid period format: {s}"))?;
            let start = now - Duration::days(days);
            Ok((Some(start), None))
        }
        s if s.ends_with('w') => {
            // Relative weeks: 1w, 4w
            let weeks: i64 = s[..s.len() - 1]
                .parse()
                .map_err(|_| anyhow::anyhow!("invalid period format: {s}"))?;
            let start = now - Duration::weeks(weeks);
            Ok((Some(start), None))
        }
        s if s.ends_with('m') && !s.contains(':') => {
            // Relative months: 1m, 3m
            let months: i64 = s[..s.len() - 1]
                .parse()
                .map_err(|_| anyhow::anyhow!("invalid period format: {s}"))?;
            let start = now - Duration::days(months * 30);
            Ok((Some(start), None))
        }
        s if s.contains(':') => {
            // Date range: 2026-03-12:2026-03-20
            let parts: Vec<&str> = s.split(':').collect();
            if parts.len() != 2 {
                anyhow::bail!("invalid date range format: {s}");
            }
            let start_date = NaiveDate::parse_from_str(parts[0], "%Y-%m-%d")
                .map_err(|e| anyhow::anyhow!("invalid start date: {e}"))?;
            let end_date = NaiveDate::parse_from_str(parts[1], "%Y-%m-%d")
                .map_err(|e| anyhow::anyhow!("invalid end date: {e}"))?;
            let start = start_date
                .and_hms_opt(0, 0, 0)
                .map(|d| Utc.from_utc_datetime(&d))
                .unwrap();
            let end = end_date
                .and_hms_opt(23, 59, 59)
                .map(|d| Utc.from_utc_datetime(&d))
                .unwrap();
            Ok((Some(start), Some(end)))
        }
        s => {
            // Single date: 2026-03-15
            let date = NaiveDate::parse_from_str(s, "%Y-%m-%d")
                .map_err(|e| anyhow::anyhow!("invalid date format: {e}"))?;
            let start = date
                .and_hms_opt(0, 0, 0)
                .map(|d| Utc.from_utc_datetime(&d))
                .unwrap();
            let end = date
                .and_hms_opt(23, 59, 59)
                .map(|d| Utc.from_utc_datetime(&d))
                .unwrap();
            Ok((Some(start), Some(end)))
        }
    }
}

/// Output usage data in JSON format
fn output_json(
    records: &[usage_stats::UsageRecord],
    view: &str,
    start: Option<chrono::DateTime<chrono::Utc>>,
    end: Option<chrono::DateTime<chrono::Utc>>,
) -> Result<()> {
    use std::collections::HashMap;

    // Calculate summary
    let total_input: i64 = records.iter().map(|r| r.input_tokens).sum();
    let total_output: i64 = records.iter().map(|r| r.output_tokens).sum();
    let total_tokens: i64 = records.iter().map(|r| r.total_tokens).sum();
    let request_count = records.len();

    let mut output = serde_json::json!({
        "period": {
            "start": start.map(|d| d.to_rfc3339()),
            "end": end.map(|d| d.to_rfc3339()),
        },
        "summary": {
            "input_tokens": total_input,
            "output_tokens": total_output,
            "total_tokens": total_tokens,
            "request_count": request_count,
        }
    });

    // Add view-specific data
    match view {
        "by-model" => {
            let mut by_model: HashMap<String, serde_json::Value> = HashMap::new();
            for r in records {
                let entry = by_model.entry(r.model.clone()).or_insert_with(|| {
                    serde_json::json!({
                        "model": r.model,
                        "input_tokens": 0,
                        "output_tokens": 0,
                        "total_tokens": 0,
                        "request_count": 0
                    })
                });
                *entry.get_mut("input_tokens").unwrap() =
                    serde_json::json!(entry["input_tokens"].as_i64().unwrap() + r.input_tokens);
                *entry.get_mut("output_tokens").unwrap() =
                    serde_json::json!(entry["output_tokens"].as_i64().unwrap() + r.output_tokens);
                *entry.get_mut("total_tokens").unwrap() =
                    serde_json::json!(entry["total_tokens"].as_i64().unwrap() + r.total_tokens);
                *entry.get_mut("request_count").unwrap() =
                    serde_json::json!(entry["request_count"].as_i64().unwrap() + 1);
            }
            output["by_model"] = serde_json::json!(by_model.values().collect::<Vec<_>>());
        }
        "by-provider" => {
            let mut by_provider: HashMap<String, serde_json::Value> = HashMap::new();
            for r in records {
                let entry = by_provider.entry(r.provider.clone()).or_insert_with(|| {
                    serde_json::json!({
                        "provider": r.provider,
                        "input_tokens": 0,
                        "output_tokens": 0,
                        "total_tokens": 0,
                        "request_count": 0,
                        "models": {}
                    })
                });
                *entry.get_mut("input_tokens").unwrap() =
                    serde_json::json!(entry["input_tokens"].as_i64().unwrap() + r.input_tokens);
                *entry.get_mut("output_tokens").unwrap() =
                    serde_json::json!(entry["output_tokens"].as_i64().unwrap() + r.output_tokens);
                *entry.get_mut("total_tokens").unwrap() =
                    serde_json::json!(entry["total_tokens"].as_i64().unwrap() + r.total_tokens);
                *entry.get_mut("request_count").unwrap() =
                    serde_json::json!(entry["request_count"].as_i64().unwrap() + 1);

                // Add model breakdown
                let models = entry.get_mut("models").unwrap();
                let model_entry = models
                    .as_object_mut()
                    .unwrap()
                    .entry(r.model.clone())
                    .or_insert_with(|| {
                        serde_json::json!({
                            "model": r.model,
                            "input_tokens": 0,
                            "output_tokens": 0,
                            "total_tokens": 0,
                            "request_count": 0
                        })
                    });
                *model_entry.get_mut("input_tokens").unwrap() = serde_json::json!(
                    model_entry["input_tokens"].as_i64().unwrap() + r.input_tokens
                );
                *model_entry.get_mut("output_tokens").unwrap() = serde_json::json!(
                    model_entry["output_tokens"].as_i64().unwrap() + r.output_tokens
                );
                *model_entry.get_mut("total_tokens").unwrap() = serde_json::json!(
                    model_entry["total_tokens"].as_i64().unwrap() + r.total_tokens
                );
                *model_entry.get_mut("request_count").unwrap() =
                    serde_json::json!(model_entry["request_count"].as_i64().unwrap() + 1);
            }
            output["by_provider"] = serde_json::json!(by_provider.values().collect::<Vec<_>>());
        }
        "by-endpoint" => {
            let mut by_endpoint: HashMap<String, serde_json::Value> = HashMap::new();
            for r in records {
                let entry = by_endpoint.entry(r.endpoint.clone()).or_insert_with(|| {
                    serde_json::json!({
                        "endpoint": r.endpoint,
                        "input_tokens": 0,
                        "output_tokens": 0,
                        "total_tokens": 0,
                        "request_count": 0
                    })
                });
                *entry.get_mut("input_tokens").unwrap() =
                    serde_json::json!(entry["input_tokens"].as_i64().unwrap() + r.input_tokens);
                *entry.get_mut("output_tokens").unwrap() =
                    serde_json::json!(entry["output_tokens"].as_i64().unwrap() + r.output_tokens);
                *entry.get_mut("total_tokens").unwrap() =
                    serde_json::json!(entry["total_tokens"].as_i64().unwrap() + r.total_tokens);
                *entry.get_mut("request_count").unwrap() =
                    serde_json::json!(entry["request_count"].as_i64().unwrap() + 1);
            }
            output["by_endpoint"] = serde_json::json!(by_endpoint.values().collect::<Vec<_>>());
        }
        "by-day" => {
            let mut by_day: HashMap<String, serde_json::Value> = HashMap::new();
            for r in records {
                if let Some(date) = r.date() {
                    let date_str = date.format("%Y-%m-%d").to_string();
                    let entry = by_day.entry(date_str).or_insert_with(|| {
                        serde_json::json!({
                            "date": date.format("%Y-%m-%d").to_string(),
                            "input_tokens": 0,
                            "output_tokens": 0,
                            "total_tokens": 0,
                            "request_count": 0
                        })
                    });
                    *entry.get_mut("input_tokens").unwrap() =
                        serde_json::json!(entry["input_tokens"].as_i64().unwrap() + r.input_tokens);
                    *entry.get_mut("output_tokens").unwrap() = serde_json::json!(
                        entry["output_tokens"].as_i64().unwrap() + r.output_tokens
                    );
                    *entry.get_mut("total_tokens").unwrap() =
                        serde_json::json!(entry["total_tokens"].as_i64().unwrap() + r.total_tokens);
                    *entry.get_mut("request_count").unwrap() =
                        serde_json::json!(entry["request_count"].as_i64().unwrap() + 1);
                }
            }
            output["by_day"] = serde_json::json!(by_day.values().collect::<Vec<_>>());
        }
        "by-hour" => {
            let mut by_hour: HashMap<String, serde_json::Value> = HashMap::new();
            for r in records {
                if let Some(dt) = r.parsed_timestamp() {
                    let hour_str = dt.format("%Y-%m-%d %H:00").to_string();
                    let entry = by_hour.entry(hour_str).or_insert_with(|| {
                        serde_json::json!({
                            "hour": dt.format("%Y-%m-%d %H:00").to_string(),
                            "input_tokens": 0,
                            "output_tokens": 0,
                            "total_tokens": 0,
                            "request_count": 0
                        })
                    });
                    *entry.get_mut("input_tokens").unwrap() =
                        serde_json::json!(entry["input_tokens"].as_i64().unwrap() + r.input_tokens);
                    *entry.get_mut("output_tokens").unwrap() = serde_json::json!(
                        entry["output_tokens"].as_i64().unwrap() + r.output_tokens
                    );
                    *entry.get_mut("total_tokens").unwrap() =
                        serde_json::json!(entry["total_tokens"].as_i64().unwrap() + r.total_tokens);
                    *entry.get_mut("request_count").unwrap() =
                        serde_json::json!(entry["request_count"].as_i64().unwrap() + 1);
                }
            }
            output["by_hour"] = serde_json::json!(by_hour.values().collect::<Vec<_>>());
        }
        _ => {}
    }

    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
}

/// Output usage data in human-readable format
fn output_human_readable(
    records: &[usage_stats::UsageRecord],
    view: &str,
    start: Option<chrono::DateTime<chrono::Utc>>,
    end: Option<chrono::DateTime<chrono::Utc>>,
) -> Result<()> {
    use std::collections::HashMap;

    if records.is_empty() {
        println!("No usage records found.");
        return Ok(());
    }

    // Calculate summary
    let total_input: i64 = records.iter().map(|r| r.input_tokens).sum();
    let total_output: i64 = records.iter().map(|r| r.output_tokens).sum();
    let total_tokens: i64 = records.iter().map(|r| r.total_tokens).sum();
    let request_count = records.len();

    // Print header
    let period_str = match (start, end) {
        (Some(s), Some(e)) => format!("{} to {}", s.format("%Y-%m-%d"), e.format("%Y-%m-%d")),
        (Some(s), None) => format!("since {}", s.format("%Y-%m-%d")),
        _ => "all time".to_string(),
    };

    println!();
    println!("Token Usage Summary ({})", period_str);
    println!("{}", "═".repeat(60));
    println!();
    println!("Total:");
    println!("  Input:    {:>12} tokens", format_number(total_input));
    println!("  Output:   {:>12} tokens", format_number(total_output));
    println!("  Total:    {:>12} tokens", format_number(total_tokens));
    println!("  Requests: {:>12}", request_count);
    println!();

    // Print view-specific data
    match view {
        "by-model" => {
            println!("By Model (aggregated across providers):");
            println!("{}", "─".repeat(60));

            let mut by_model: HashMap<String, (i64, i64, i64, usize)> = HashMap::new();
            for r in records {
                let entry = by_model.entry(r.model.clone()).or_insert((0, 0, 0, 0));
                entry.0 += r.input_tokens;
                entry.1 += r.output_tokens;
                entry.2 += r.total_tokens;
                entry.3 += 1;
            }

            let mut models: Vec<_> = by_model.into_iter().collect();
            models.sort_by_key(|b| std::cmp::Reverse(b.1.2)); // Sort by total tokens desc

            println!(
                "  {:<20} {:>12} {:>12} {:>12} {:>10} {:>6}",
                "Model", "Input", "Output", "Total", "Requests", "%"
            );
            println!("  {}", "─".repeat(76));

            for (model, (input, output, total, count)) in models {
                let pct = if total_tokens > 0 {
                    (total as f64 / total_tokens as f64) * 100.0
                } else {
                    0.0
                };
                println!(
                    "  {:<20} {:>12} {:>12} {:>12} {:>10} {:>5.1}%",
                    truncate_str(&model, 20),
                    format_number(input),
                    format_number(output),
                    format_number(total),
                    count,
                    pct
                );
            }
        }
        "by-provider" => {
            println!("By Provider:");
            println!("{}", "─".repeat(60));

            let mut by_provider: HashMap<String, ProviderAgg> = HashMap::new();
            for r in records {
                let entry =
                    by_provider
                        .entry(r.provider.clone())
                        .or_insert((0, 0, 0, 0, HashMap::new()));
                entry.0 += r.input_tokens;
                entry.1 += r.output_tokens;
                entry.2 += r.total_tokens;
                entry.3 += 1;

                let model_entry = entry.4.entry(r.model.clone()).or_insert((0, 0, 0, 0));
                model_entry.0 += r.input_tokens;
                model_entry.1 += r.output_tokens;
                model_entry.2 += r.total_tokens;
                model_entry.3 += 1;
            }

            let mut providers: Vec<_> = by_provider.into_iter().collect();
            providers.sort_by_key(|b| std::cmp::Reverse(b.1.2));

            for (provider, (_input, _output, total, _count, models)) in providers {
                let pct = if total_tokens > 0 {
                    (total as f64 / total_tokens as f64) * 100.0
                } else {
                    0.0
                };
                println!("  {} ({}, {:.1}%)", provider, format_number(total), pct);
                println!(
                    "    {:<18} {:>12} {:>12} {:>12} {:>10}",
                    "Model", "Input", "Output", "Total", "Requests"
                );
                println!("    {}", "─".repeat(66));

                let mut model_list: Vec<_> = models.into_iter().collect();
                model_list.sort_by_key(|b| std::cmp::Reverse(b.1.2));

                for (model, (m_input, m_output, m_total, m_count)) in model_list {
                    println!(
                        "    {:<18} {:>12} {:>12} {:>12} {:>10}",
                        truncate_str(&model, 18),
                        format_number(m_input),
                        format_number(m_output),
                        format_number(m_total),
                        m_count
                    );
                }
                println!();
            }
        }
        "by-endpoint" => {
            println!("By Endpoint:");
            println!("{}", "─".repeat(60));

            let mut by_endpoint: HashMap<String, (i64, i64, i64, usize)> = HashMap::new();
            for r in records {
                let entry = by_endpoint
                    .entry(r.endpoint.clone())
                    .or_insert((0, 0, 0, 0));
                entry.0 += r.input_tokens;
                entry.1 += r.output_tokens;
                entry.2 += r.total_tokens;
                entry.3 += 1;
            }

            let mut endpoints: Vec<_> = by_endpoint.into_iter().collect();
            endpoints.sort_by_key(|b| std::cmp::Reverse(b.1.2));

            println!(
                "  {:<20} {:>12} {:>12} {:>12} {:>10} {:>6}",
                "Endpoint", "Input", "Output", "Total", "Requests", "%"
            );
            println!("  {}", "─".repeat(76));

            for (endpoint, (input, output, total, count)) in endpoints {
                let pct = if total_tokens > 0 {
                    (total as f64 / total_tokens as f64) * 100.0
                } else {
                    0.0
                };
                println!(
                    "  {:<20} {:>12} {:>12} {:>12} {:>10} {:>5.1}%",
                    truncate_str(&endpoint, 20),
                    format_number(input),
                    format_number(output),
                    format_number(total),
                    count,
                    pct
                );
            }
        }
        "by-day" => {
            println!("By Day:");
            println!("{}", "─".repeat(60));

            let mut by_day: HashMap<String, (i64, i64, i64, usize)> = HashMap::new();
            for r in records {
                if let Some(date) = r.date() {
                    let date_str = date.format("%Y-%m-%d").to_string();
                    let entry = by_day.entry(date_str).or_insert((0, 0, 0, 0));
                    entry.0 += r.input_tokens;
                    entry.1 += r.output_tokens;
                    entry.2 += r.total_tokens;
                    entry.3 += 1;
                }
            }

            let mut days: Vec<_> = by_day.into_iter().collect();
            days.sort_by(|a, b| b.0.cmp(&a.0)); // Sort by date desc

            println!(
                "  {:<12} {:>12} {:>12} {:>12} {:>10}",
                "Date", "Input", "Output", "Total", "Requests"
            );
            println!("  {}", "─".repeat(60));

            for (date, (input, output, total, count)) in days {
                println!(
                    "  {:<12} {:>12} {:>12} {:>12} {:>10}",
                    date,
                    format_number(input),
                    format_number(output),
                    format_number(total),
                    count
                );
            }
        }
        "by-hour" => {
            println!("By Hour:");
            println!("{}", "─".repeat(60));

            let mut by_hour: HashMap<String, (i64, i64, i64, usize)> = HashMap::new();
            for r in records {
                if let Some(dt) = r.parsed_timestamp() {
                    let hour_str = dt.format("%Y-%m-%d %H:00").to_string();
                    let entry = by_hour.entry(hour_str).or_insert((0, 0, 0, 0));
                    entry.0 += r.input_tokens;
                    entry.1 += r.output_tokens;
                    entry.2 += r.total_tokens;
                    entry.3 += 1;
                }
            }

            let mut hours: Vec<_> = by_hour.into_iter().collect();
            hours.sort_by(|a, b| b.0.cmp(&a.0)); // Sort by hour desc

            println!(
                "  {:<18} {:>12} {:>12} {:>12} {:>10}",
                "Hour", "Input", "Output", "Total", "Requests"
            );
            println!("  {}", "─".repeat(66));

            for (hour, (input, output, total, count)) in hours {
                println!(
                    "  {:<18} {:>12} {:>12} {:>12} {:>10}",
                    hour,
                    format_number(input),
                    format_number(output),
                    format_number(total),
                    count
                );
            }
        }
        _ => {
            println!("Unknown view mode: {}", view);
        }
    }

    println!();
    Ok(())
}

/// Format number with thousands separator
fn format_number(n: i64) -> String {
    let s = n.to_string();
    let mut result = String::new();
    for (i, c) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            result.push(',');
        }
        result.push(c);
    }
    result.chars().rev().collect()
}

/// Truncate string to max length with ellipsis
fn truncate_str(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else {
        format!("{}...", &s[..max_len.saturating_sub(3)])
    }
}
