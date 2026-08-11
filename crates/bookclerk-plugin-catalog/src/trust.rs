//! Staged trust policy for installs.

use serde::{Deserialize, Serialize};

use crate::error::{CatalogError, Result};

/// Trust / signing policy for plugin installation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrustPolicy {
    /// Allow packages without publisher signatures (digests still required).
    pub allow_unsigned: bool,
    /// Refuse yanked versions (always true for unattended).
    pub refuse_yanked: bool,
    /// When true, warn instead of refuse on missing OS code signature.
    pub warn_on_unsigned_os: bool,
}

impl Default for TrustPolicy {
    fn default() -> Self {
        Self {
            allow_unsigned: false,
            refuse_yanked: true,
            warn_on_unsigned_os: true,
        }
    }
}

impl TrustPolicy {
    /// Interactive default: allow unsigned after explicit flag.
    #[must_use]
    pub fn allow_unsigned() -> Self {
        Self {
            allow_unsigned: true,
            ..Self::default()
        }
    }

    /// Returns `Ok(())` when unsigned packages are permitted by this policy.
    pub fn check_unsigned_allowed(&self) -> Result<()> {
        if self.allow_unsigned {
            Ok(())
        } else {
            Err(CatalogError::message(
                "refusing unsigned community plugin; pass --allow-unsigned after verifying the digest",
            ))
        }
    }
}
