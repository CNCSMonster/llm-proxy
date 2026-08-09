//! 读命令委托结果本地缓存（北极星"远程模式缓存兜底"）。
//!
//! 场景：CLI 在远程/容器环境，server 不可达时读取之前委托查询的缓存结果，
//! 输出开头标注来源与缓存时间（可能过期）。本地模式（同环境）无 server 时
//! 走独立模式（读盘/持锁），不使用此缓存。

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde_json::Value;

/// 读命令查询缓存。
pub struct QueryCache {
    dir: PathBuf,
}

impl QueryCache {
    /// 缓存目录：state_dir/query-cache（与 pid/sock/cooldowns/usage 同目录族）。
    pub fn new() -> Self {
        Self {
            dir: crate::service::state_dir().join("query-cache"),
        }
    }

    fn cache_path(&self, key: &str) -> PathBuf {
        let file = sanitize_key(key);
        self.dir.join(format!("{file}.json"))
    }

    /// 保存委托查询结果（含时间戳）。
    pub fn save(&self, key: &str, data: &Value) -> Result<()> {
        let path = self.cache_path(key);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create cache dir {}", parent.display()))?;
        }
        let cached_at = chrono::Utc::now().timestamp();
        let body = serde_json::to_string(&serde_json::json!({
            "cached_at": cached_at,
            "data": data,
        }))
        .context("failed to serialize cache entry")?;
        std::fs::write(&path, body)
            .with_context(|| format!("failed to write cache {}", path.display()))
    }

    /// 读取缓存：(cached_at, data)。无缓存或损坏返回 None。
    pub fn load(&self, key: &str) -> Option<(i64, Value)> {
        let path = self.cache_path(key);
        let body = std::fs::read_to_string(&path).ok()?;
        let value: Value = serde_json::from_str(&body).ok()?;
        let cached_at = value.get("cached_at").and_then(Value::as_i64)?;
        let data = value.get("data")?.clone();
        Some((cached_at, data))
    }

    /// 缓存时间的本地可读格式（用于输出标注）。
    pub fn format_cached_at(unix: i64) -> String {
        chrono::DateTime::from_timestamp(unix, 0)
            .map(|dt| {
                dt.with_timezone(&chrono::Local)
                    .format("%Y-%m-%d %H:%M:%S")
                    .to_string()
            })
            .unwrap_or_else(|| "unknown time".to_string())
    }
}

/// 缓存 key 安全化：仅保留字母数字和 `-_:.`，其余替换为 `_`。
fn sanitize_key(key: &str) -> String {
    key.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | ':' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_key_replaces_unsafe_chars() {
        assert_eq!(
            sanitize_key("provider-info:deepseek"),
            "provider-info:deepseek"
        );
        assert_eq!(sanitize_key("usage?period=7d"), "usage_period_7d");
        // '.' 保留（sanitize 允许），路径分隔符被替换
        assert_eq!(sanitize_key("../../etc/passwd"), ".._.._etc_passwd");
    }

    #[test]
    fn cache_roundtrip_preserves_data() {
        let temp = tempfile::tempdir().expect("tempdir");
        // 用临时目录替代 state_dir（QueryCache::new 用全局 state_dir，此处手动构造）
        let cache = QueryCache {
            dir: temp.path().to_path_buf(),
        };
        let data = serde_json::json!({"providers": 2, "active": []});
        cache.save("provider-info:test", &data).expect("save");
        let (cached_at, loaded) = cache.load("provider-info:test").expect("load");
        assert!(cached_at > 0);
        assert_eq!(loaded, data);
    }

    #[test]
    fn cache_load_missing_returns_none() {
        let temp = tempfile::tempdir().expect("tempdir");
        let cache = QueryCache {
            dir: temp.path().to_path_buf(),
        };
        assert!(cache.load("nonexistent-key").is_none());
    }
}
