//! Optional facilities negotiated inside the plugin ABI (not a substitute for versioning).

/// Guest honors scalar / stream-window / list-page caps.
pub const FEATURE_SCALAR_LIMITS: &str = crate::plugin_capnp::FEATURE_SCALAR_LIMITS;

/// Media moves through transferred [`crate::roles::ByteRange`] / `ByteSource` streams.
pub const FEATURE_STREAMS: &str = crate::plugin_capnp::FEATURE_STREAMS;

/// Guest implements server-side [`super::Destination::copy`].
pub const FEATURE_STORAGE_COPY: &str = crate::plugin_capnp::FEATURE_STORAGE_COPY;

/// Guest implements [`super::Destination::move_object`].
pub const FEATURE_STORAGE_MOVE: &str = crate::plugin_capnp::FEATURE_STORAGE_MOVE;

/// Guest implements [`super::Destination::move_many`].
pub const FEATURE_STORAGE_MOVE_MANY: &str = crate::plugin_capnp::FEATURE_STORAGE_MOVE_MANY;

/// Guest implements [`super::Destination::delete_many`].
pub const FEATURE_STORAGE_DELETE_MANY: &str = crate::plugin_capnp::FEATURE_STORAGE_DELETE_MANY;

/// Guest implements [`super::Destination::head_many`].
pub const FEATURE_STORAGE_HEAD_MANY: &str = crate::plugin_capnp::FEATURE_STORAGE_HEAD_MANY;

/// Guest enforces [`super::WriteOptions::if_match`] and
/// [`super::WriteOptions::if_none_match`].
pub const FEATURE_STORAGE_CONDITIONAL_WRITE: &str =
    crate::plugin_capnp::FEATURE_STORAGE_CONDITIONAL_WRITE;

/// Guest enforces [`super::WriteOptions::if_match`] and
/// [`super::WriteOptions::if_none_match`] without an atomic primitive: it
/// writes, then checks and withdraws its write when another got in first,
/// so a reader may briefly see the losing write. The host reports such a
/// write as not atomic.
pub const FEATURE_STORAGE_CHECKED_WRITE: &str = crate::plugin_capnp::FEATURE_STORAGE_CHECKED_WRITE;

/// Guest implements [`super::Destination::changes`].
pub const FEATURE_STORAGE_CHANGES: &str = crate::plugin_capnp::FEATURE_STORAGE_CHANGES;

/// Guest implements [`super::Destination::translate_keys`].
pub const FEATURE_STORAGE_TRANSLATE_KEYS: &str =
    crate::plugin_capnp::FEATURE_STORAGE_TRANSLATE_KEYS;

/// Every optional storage operation a host offers a storage guest, beside
/// [`FEATURE_STORAGE_COPY`]. Each one the guest accepts replaces the host's
/// fallback for that operation.
pub const STORAGE_OPERATION_FEATURES: &[&str] = &[
    FEATURE_STORAGE_MOVE,
    FEATURE_STORAGE_MOVE_MANY,
    FEATURE_STORAGE_DELETE_MANY,
    FEATURE_STORAGE_HEAD_MANY,
    FEATURE_STORAGE_CONDITIONAL_WRITE,
    FEATURE_STORAGE_CHECKED_WRITE,
    FEATURE_STORAGE_CHANGES,
    FEATURE_STORAGE_TRANSLATE_KEYS,
];

/// Known RPC feature names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RpcFeature {
    /// Wire name (`rpc.streams`, …).
    pub name: &'static str,
}

/// Host-required features for every guest. Streams are required for
/// destination/source/worker roles (see host `negotiate_describe`).
pub const REQUIRED_RPC_FEATURES: &[&str] = &[FEATURE_SCALAR_LIMITS];

/// Intersects host-offered and guest-accepted feature lists.
///
/// # Errors
///
/// Returns a message when a required feature is missing or the guest offers an
/// unsafe combination (streams without scalar limits).
pub fn negotiate_rpc_features(
    host_offers: &[&str],
    guest_accepts: &[String],
) -> crate::Result<Vec<String>> {
    let guest: Vec<String> = guest_accepts
        .iter()
        .filter(|f| host_offers.iter().any(|h| h == f))
        .cloned()
        .collect();
    let has = |name: &str| guest.iter().any(|g| g == name);
    if has(FEATURE_STREAMS) && !has(FEATURE_SCALAR_LIMITS) {
        return Err(crate::PluginError::new(
            crate::PluginErrorCode::Unsupported,
            "rpc.streams requires rpc.scalarLimits",
        ));
    }
    for required in REQUIRED_RPC_FEATURES {
        if !has(required) {
            return Err(crate::PluginError::new(
                crate::PluginErrorCode::Unsupported,
                format!("guest did not accept required feature `{required}`"),
            ));
        }
    }
    Ok(guest)
}

#[cfg(test)]
#[allow(clippy::missing_panics_doc)]
mod tests {
    use super::*;

    #[test]
    fn rejects_streams_without_limits() {
        let err = negotiate_rpc_features(
            &[FEATURE_SCALAR_LIMITS, FEATURE_STREAMS],
            &[FEATURE_STREAMS.to_string()],
        )
        .unwrap_err();
        assert_eq!(err.code, crate::PluginErrorCode::Unsupported);
    }

    #[test]
    fn scalar_limits_alone_is_enough() {
        let got = negotiate_rpc_features(
            &[FEATURE_SCALAR_LIMITS, FEATURE_STREAMS],
            &[FEATURE_SCALAR_LIMITS.to_string()],
        )
        .unwrap();
        assert_eq!(got, vec![FEATURE_SCALAR_LIMITS.to_string()]);
    }

    #[test]
    fn accepts_required_pair() {
        let got = negotiate_rpc_features(
            &[FEATURE_SCALAR_LIMITS, FEATURE_STREAMS, FEATURE_STORAGE_COPY],
            &[
                FEATURE_SCALAR_LIMITS.to_string(),
                FEATURE_STREAMS.to_string(),
            ],
        )
        .unwrap();
        assert_eq!(got.len(), 2);
    }
}
