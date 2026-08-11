//! Hashing for claim tickets and session cookies (store hashes only).

use sha2::{Digest, Sha256};

/// SHA-256 hex digest of a raw ticket/session token for durable storage.
#[must_use]
pub fn hash_token(raw: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(raw.as_bytes());
    hex::encode(hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_token_is_stable_hex() {
        let a = hash_token("example-token");
        let b = hash_token("example-token");
        assert_eq!(a, b);
        assert_eq!(a.len(), 64);
        assert_ne!(hash_token("other"), a);
    }
}
