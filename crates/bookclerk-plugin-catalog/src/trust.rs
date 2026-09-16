//! Staged trust policy for installs.

use serde::{Deserialize, Serialize};

use crate::error::{CatalogError, Result};

/// Trust policy for plugin installation (digest-required artifacts).
///
/// [`Self::allow_unverified_publisher`] means the operator accepted a package
/// that has no independent publisher authenticity proof. Archive SHA-256 is
/// still required. Bookclerk does **not** verify publisher signatures.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrustPolicy {
    /// Allow community packages without independent publisher authenticity
    /// (content digests are still required).
    pub allow_unverified_publisher: bool,
    /// Refuse yanked versions (always true for unattended).
    pub refuse_yanked: bool,
    /// When true, warn instead of refuse on missing OS code signature.
    pub warn_on_unsigned_os: bool,
}

impl Default for TrustPolicy {
    fn default() -> Self {
        Self {
            allow_unverified_publisher: false,
            refuse_yanked: true,
            warn_on_unsigned_os: true,
        }
    }
}

impl TrustPolicy {
    /// Interactive default: allow community packages after an explicit flag.
    #[must_use]
    pub fn allow_unverified_publisher() -> Self {
        Self {
            allow_unverified_publisher: true,
            ..Self::default()
        }
    }

    /// Returns `Ok(())` when community packages without publisher authenticity
    /// are permitted by this policy.
    ///
    /// # Errors
    ///
    /// Returns an error when the operator has not opted in.
    pub fn check_unverified_publisher_allowed(&self) -> Result<()> {
        if self.allow_unverified_publisher {
            Ok(())
        } else {
            Err(CatalogError::message(
                "refusing community plugin without an explicit operator override; \
                 pass --allow-unverified-publisher after verifying the digest",
            ))
        }
    }
}
