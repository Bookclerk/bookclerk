//! Optional facilities negotiated inside ABI v2 (not a substitute for versioning).

/// Guest honors scalar / stream-window / list-page caps.
pub const FEATURE_SCALAR_LIMITS: &str = "rpc.scalarLimits";

/// Media moves through transferred [`super::roles::ByteRange`] / `ByteSource` streams.
pub const FEATURE_STREAMS: &str = "rpc.streams";

/// Guest implements server-side [`super::Destination::copy`].
pub const FEATURE_STORAGE_COPY: &str = "storage.copy";

/// Known v2 RPC feature names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RpcFeature {
    /// Wire name (`rpc.streams`, …).
    pub name: &'static str,
}

/// Host-required features for every v2 guest. Streams are required for
/// destination/source/worker roles (see host `negotiate_describe`).
pub const REQUIRED_V2_FEATURES: &[&str] = &[FEATURE_SCALAR_LIMITS];

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
    for required in REQUIRED_V2_FEATURES {
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
