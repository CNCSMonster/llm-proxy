//! Token-based access control module
//!
//! This module provides:
//! - Token configuration parsing from tokens.toml
//! - Token key generation
//! - Usage tracking with quota windows (SQLite)
//! - Authentication middleware (to be implemented)

pub mod auth;
pub mod config;
pub mod dimension;
pub mod keygen;
pub mod manage;
pub mod ratio;
pub mod usage;

// Re-exports will be enabled when consumed by other modules
// pub use config::{QuotaWindow, TokenConfig, TokensConfig};
// pub use keygen::generate_key;
// pub use usage::TokenUsageStore;
