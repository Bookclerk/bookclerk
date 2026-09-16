//! Scalar and stream window limits for the plugin ABI.
//!
//! Every value aliases the constant declared in `schema/plugin.capnp` (via the
//! generated [`crate::plugin_capnp`] module), so the schema is the single
//! source of truth and Rust cannot drift from it.

/// Product ABI version (`apiVersion` / `plugin.toml` `api_version`).
pub const PRODUCT_API_VERSION: u32 = crate::plugin_capnp::API_VERSION;

/// Maximum decoded size of a domain-event scalar payload (not a stream).
pub const MAX_EVENT_PAYLOAD_BYTES: u32 = crate::plugin_capnp::MAX_EVENT_PAYLOAD_BYTES;

/// Maximum decoded size of an ordinary RPC scalar value (not a stream window).
pub const MAX_SCALAR_BYTES: u32 = crate::plugin_capnp::MAX_SCALAR_BYTES;

/// Maximum bytes returned by one `ByteSource.pull` (flow-control window).
pub const MAX_STREAM_WINDOW_BYTES: u32 = crate::plugin_capnp::MAX_STREAM_WINDOW_BYTES;

/// Maximum objects in one `Destination.list` page.
pub const MAX_LIST_PAGE: u32 = crate::plugin_capnp::MAX_LIST_PAGE;

/// Maximum already-separated operations in one plugin-owned migration.
pub const MAX_PLUGIN_MIGRATION_OPS: u32 = crate::plugin_capnp::MAX_PLUGIN_MIGRATION_OPS;

/// Maximum already-separated operations across one `databaseMigrations`
/// registration (independent of the per-migration cap and of id+SQL UTF-8 bytes).
pub const MAX_PLUGIN_MIGRATION_TOTAL_OPS: u32 = crate::plugin_capnp::MAX_PLUGIN_MIGRATION_TOTAL_OPS;

/// Maximum aggregate UTF-8 bytes of plugin migration ids plus SQL in one
/// `databaseMigrations` registration (Cap'n Proto framing is not counted).
pub const MAX_PLUGIN_MIGRATION_REGISTRATION_BYTES: u32 =
    crate::plugin_capnp::MAX_PLUGIN_MIGRATION_REGISTRATION_BYTES;

/// Negotiated numeric limits advertised at [`super::PluginDescribe`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScalarLimits {
    /// [`MAX_SCALAR_BYTES`] (or a host-clamped lower value).
    pub max_scalar_bytes: u32,
    /// [`MAX_STREAM_WINDOW_BYTES`] (or a host-clamped lower value).
    pub max_stream_window_bytes: u32,
    /// [`MAX_LIST_PAGE`] (or a host-clamped lower value).
    pub max_list_page: u32,
}

impl Default for ScalarLimits {
    fn default() -> Self {
        Self {
            max_scalar_bytes: MAX_SCALAR_BYTES,
            max_stream_window_bytes: MAX_STREAM_WINDOW_BYTES,
            max_list_page: MAX_LIST_PAGE,
        }
    }
}

impl ScalarLimits {
    /// Rejects zero or above-product-maxima limits.
    ///
    /// # Errors
    ///
    /// Returns [`crate::PluginError::invalid_params`] when any component is
    /// zero or larger than the product constant.
    pub fn validate(self) -> crate::Result<Self> {
        if self.max_scalar_bytes == 0
            || self.max_stream_window_bytes == 0
            || self.max_list_page == 0
        {
            return Err(crate::PluginError::invalid_params(
                "scalar limits must be non-zero",
            ));
        }
        if self.max_scalar_bytes > MAX_SCALAR_BYTES
            || self.max_stream_window_bytes > MAX_STREAM_WINDOW_BYTES
            || self.max_list_page > MAX_LIST_PAGE
        {
            return Err(crate::PluginError::invalid_params(
                "scalar limits exceed product maxima",
            ));
        }
        Ok(self)
    }

    /// Intersection of host-offered and guest-accepted limits (component-wise min).
    #[must_use]
    pub fn intersect(self, other: Self) -> Self {
        Self {
            max_scalar_bytes: self.max_scalar_bytes.min(other.max_scalar_bytes),
            max_stream_window_bytes: self
                .max_stream_window_bytes
                .min(other.max_stream_window_bytes),
            max_list_page: self.max_list_page.min(other.max_list_page).max(1),
        }
    }

    /// Clamps a list page `limit` into `(0, max_list_page]`.
    #[must_use]
    pub fn clamp_list_limit(self, limit: u32) -> u32 {
        let requested = if limit == 0 {
            self.max_list_page
        } else {
            limit
        };
        requested.min(self.max_list_page).max(1)
    }
}

#[cfg(test)]
#[allow(clippy::missing_panics_doc)]
mod tests {
    use super::*;

    #[test]
    fn intersect_takes_minima() {
        let host = ScalarLimits::default();
        let guest = ScalarLimits {
            max_scalar_bytes: 64 * 1024,
            max_stream_window_bytes: 32 * 1024,
            max_list_page: 10,
        };
        let out = host.intersect(guest);
        assert_eq!(out.max_scalar_bytes, 64 * 1024);
        assert_eq!(out.max_stream_window_bytes, 32 * 1024);
        assert_eq!(out.max_list_page, 10);
    }

    #[test]
    fn clamp_list_limit_defaults_and_caps() {
        let limits = ScalarLimits::default();
        assert_eq!(limits.clamp_list_limit(0), MAX_LIST_PAGE);
        assert_eq!(limits.clamp_list_limit(1), 1);
        assert_eq!(limits.clamp_list_limit(10_000), MAX_LIST_PAGE);
    }

    #[test]
    fn validate_rejects_zero_and_oversize() {
        assert!(ScalarLimits::default().validate().is_ok());
        assert!(ScalarLimits {
            max_scalar_bytes: 0,
            ..ScalarLimits::default()
        }
        .validate()
        .is_err());
        assert!(ScalarLimits {
            max_list_page: MAX_LIST_PAGE + 1,
            ..ScalarLimits::default()
        }
        .validate()
        .is_err());
    }
}
