#![forbid(unsafe_code)]

//! Deterministic plan hashing.
//!
//! The authoritative hash binds a versioned canonical encoding. Maps and
//! operations sort, timestamps and telemetry never enter.

use sha2::{Digest, Sha256};

/// Canonical hash version. Bump when the preimage layout changes.
pub const HASH_VERSION: u32 = 1;

/// Hash one canonical preimage into lowercase hex.
pub fn plan_hash(canonical: &[u8]) -> String {
    format!("{:x}", Sha256::digest(canonical))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_are_stable_and_hex() {
        let first = plan_hash(b"v1:empty");
        let second = plan_hash(b"v1:empty");
        assert_eq!(first, second);
        assert!(first.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(first, plan_hash(b"v1:nonempty"));
    }
}
