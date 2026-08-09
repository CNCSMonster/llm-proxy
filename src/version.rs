#![allow(dead_code)] // Version utilities reserved for future use
//! Version info injected by build.rs at compile time.

macro_rules! env_str {
    ($key:expr, $default:expr) => {
        match option_env!($key) {
            Some(v) if !v.is_empty() => v,
            _ => $default,
        }
    };
}

/// Short git commit hash (e.g. "a1b2c3d").
pub const GIT_COMMIT: &str = env_str!("GIT_COMMIT", "unknown");

/// UTC build timestamp (e.g. "2026-07-29T14:30:00Z").
pub const BUILD_TIME: &str = env_str!("BUILD_TIME", "unknown");

/// Whether the working tree was dirty when built.
pub fn is_dirty() -> bool {
    option_env!("IS_DIRTY") == Some("true")
}

/// SHA-256 of `git diff HEAD`, first 8 hex chars. Only set when dirty.
/// Suppressed on formal builds (--features formal).
#[cfg(not(feature = "formal"))]
pub const DIRTY_HASH: Option<&str> = match option_env!("DIRTY_HASH") {
    Some(v) if !v.is_empty() => Some(v),
    _ => None,
};

#[cfg(feature = "formal")]
pub const DIRTY_HASH: Option<&str> = None;

/// Print version information to stdout.
pub fn print_version() {
    println!("llm-proxy {}", env!("CARGO_PKG_VERSION"));
    println!("  commit:  {GIT_COMMIT}");
    println!("  built:   {BUILD_TIME}");
    if let Some(hash) = DIRTY_HASH {
        println!("  dirty:   {hash} (uncommitted changes)");
    }
}
