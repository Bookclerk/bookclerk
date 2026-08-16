//! Contact-email normalization and library-backed validation.

use email_address::EmailAddress;
use sha2::{Digest, Sha256};

use crate::error::{LibraryError, Result};

/// RFC 5321 maximum length of an email address.
const EMAIL_MAX: usize = 254;

/// Trim, lowercase, and validate a contact email; empty becomes `None`.
///
/// # Errors
///
/// Returns [`LibraryError::InvalidEmail`] when the value is non-empty but not a
/// valid address (including a domain without a dot, such as `user@gmail`).
pub fn normalize_user_email(email: Option<&str>) -> Result<Option<String>> {
    let Some(raw) = email.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    let lowered = raw.to_ascii_lowercase();
    if lowered.len() > EMAIL_MAX || !is_valid_user_email(&lowered) {
        return Err(LibraryError::InvalidEmail);
    }
    Ok(Some(lowered))
}

/// True when `value` is a syntactically valid email with a dotted domain.
#[must_use]
pub fn is_valid_user_email(value: &str) -> bool {
    if value.parse::<EmailAddress>().is_err() {
        return false;
    }
    value
        .rsplit_once('@')
        .is_some_and(|(_, domain)| domain.contains('.'))
}

/// SHA-256 hex digest of a trimmed, lowercased email for Gravatar URLs.
///
/// Gravatar matches on this digest; callers should pass a stored/normalized
/// address. Empty or whitespace-only input still hashes (callers should skip).
#[must_use]
pub fn gravatar_hash(email: &str) -> String {
    let normalized = email.trim().to_ascii_lowercase();
    hex::encode(Sha256::digest(normalized.as_bytes()))
}

/// Trim + lowercase for lookups; empty/whitespace becomes `None`.
///
/// Does not validate syntax so a failed login still hashes the same way.
pub(crate) fn normalize_email_lookup(email: Option<&str>) -> Option<String> {
    email.and_then(|s| {
        let trimmed = s.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_ascii_lowercase())
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dotted_domain_is_valid() {
        assert!(is_valid_user_email("alex@example.com"));
        assert_eq!(
            normalize_user_email(Some("Alex@Example.COM")).unwrap(),
            Some(String::from("alex@example.com"))
        );
    }

    #[test]
    fn missing_tld_is_invalid() {
        assert!(!is_valid_user_email("alex.doe@gmail"));
        assert!(matches!(
            normalize_user_email(Some("alex.doe@gmail")),
            Err(LibraryError::InvalidEmail)
        ));
    }

    #[test]
    fn empty_clears() {
        assert_eq!(normalize_user_email(Some("  ")).unwrap(), None);
        assert_eq!(normalize_user_email(None).unwrap(), None);
    }

    #[test]
    fn gravatar_hash_is_lowercase_sha256() {
        assert_eq!(
            gravatar_hash("Alex@Example.COM"),
            gravatar_hash("alex@example.com")
        );
        assert_eq!(
            gravatar_hash("alex@example.com"),
            "6db61e6dcbcf2390e4a46af426f26a133a3bee45021422fc7ae86e9136f14110"
        );
    }
}
