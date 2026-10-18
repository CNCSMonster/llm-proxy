//! Token key generation
//!
//! Keys are generated as: `lp-` + 32 random alphanumeric characters
//! Total length: 35 characters

#![allow(dead_code)] // Used in later stages

use rand::rng;
use rand::seq::IndexedRandom;
use sha2::{Digest, Sha256};

/// Key prefix for all llm-proxy tokens
pub const KEY_PREFIX: &str = "lp-";

/// Length of the random part (after prefix)
pub const RANDOM_LENGTH: usize = 32;

/// Alphanumeric characters for key generation
const ALPHANUMERIC: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";

/// Generate a new random token key
///
/// Format: `lp-` + 32 random alphanumeric characters
/// Total length: 35 characters
pub fn generate_key() -> String {
    let mut rng = rng();
    let random_part: String = (0..RANDOM_LENGTH)
        .map(|_| ALPHANUMERIC.choose(&mut rng).copied().unwrap() as char)
        .collect();
    format!("{}{}", KEY_PREFIX, random_part)
}

/// Compute the non-sensitive matching identifier for a token key.
///
/// Returns the first 32 hex characters (128 bits) of SHA-256(key).
/// Used as an immutable, non-secret identifier for cooldown tracking.
pub fn token_key_hash(key: &str) -> String {
    let digest = Sha256::digest(key.as_bytes());
    hex::encode(&digest[..16])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_key_has_correct_format() {
        let key = generate_key();
        assert!(key.starts_with(KEY_PREFIX));
        assert_eq!(key.len(), KEY_PREFIX.len() + RANDOM_LENGTH);
    }

    #[test]
    fn generate_key_is_random() {
        let key1 = generate_key();
        let key2 = generate_key();
        assert_ne!(key1, key2);
    }

    #[test]
    fn generate_key_is_alphanumeric() {
        let key = generate_key();
        let random_part = &key[KEY_PREFIX.len()..];
        assert!(random_part.chars().all(|c| c.is_ascii_alphanumeric()));
    }

    #[test]
    fn token_key_hash_is_deterministic_and_32_chars() {
        let key = "lp-0123456789abcdef0123456789abcdef";
        let hash1 = token_key_hash(key);
        let hash2 = token_key_hash(key);
        assert_eq!(hash1, hash2);
        assert_eq!(hash1.len(), 32);
        assert!(hash1.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
