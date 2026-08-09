mod cache;
mod display;
mod format;
mod probe;
#[cfg(test)]
mod tests;

// Re-export public API
// DynamicModelCacheEntry is used by model.rs tests via crate::status::DynamicModelCacheEntry
#[allow(unused_imports)]
pub use cache::{
    DynamicModelCacheEntry, ProbeCacheEntry, ProbeResult, StatusCache, cache_path, read_cache,
    write_cache,
};
pub(crate) use display::print_providers;
pub(crate) use display::provider_auth_summary;
pub use display::{print_provider_info, print_provider_info_json};
pub use probe::print_status;
pub(crate) use probe::run_one_online_probe;

// Re-export format helpers used by other modules and tests
pub use format::probe_key;
// The following format helpers are public API surface (used by tests and external callers)
#[allow(unused_imports)]
pub use format::{
    calculate_health_status, format_bytes, format_duration, format_error_message,
    format_model_status, format_provider_status, get_auth_status_summary, parse_cooldown_reason,
    should_skip_probe, truncate_string,
};
