//! Turning the host keys naming templates build into the keys a destination
//! stores.
//!
//! Acquire fills a destination's naming templates into a host key
//! ([`bookclerk_storage::key_syntax`]). The destination's storage plugin
//! translates it into the path or object key it actually stores, and that
//! translated key is what acquire writes, checks for and records.

use std::sync::Arc;

use bookclerk_storage::key_syntax::HostKey;
use bookclerk_storage::StorageBackend;

/// The destination an acquire request names files for, which translates
/// its host keys.
///
/// The default stands for no destination in particular: host keys are
/// kept as written. [`crate::AcquireRequest`]s made for one destination
/// carry that destination's backend.
#[derive(Clone, Default)]
pub struct DestinationKeys {
    /// The destination's storage backend; `None` keeps host keys.
    backend: Option<Arc<dyn StorageBackend>>,
}

impl std::fmt::Debug for DestinationKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DestinationKeys")
            .field("translates", &self.backend.is_some())
            .finish()
    }
}

impl DestinationKeys {
    /// Keys translated by `backend`.
    #[must_use]
    pub fn of(backend: &dyn StorageBackend) -> Self {
        Self {
            backend: Some(Arc::from(backend.clone_box())),
        }
    }

    /// The key the destination stores `host_key` under.
    ///
    /// A key that is not a valid host key (a reconcile pattern, an empty
    /// name) is returned as given. When the destination cannot translate it
    /// (its guest is down), the host key is returned and a warning logged:
    /// the write that follows then either stores it as written or fails and
    /// is retried like any other failed copy.
    pub async fn translate(&self, host_key: String) -> String {
        let Some(backend) = &self.backend else {
            return host_key;
        };
        let Ok(parsed) = HostKey::parse(&host_key) else {
            return host_key;
        };
        match backend.translate_keys(std::slice::from_ref(&parsed)).await {
            Ok(mut keys) if keys.len() == 1 => keys.pop().unwrap_or(host_key),
            Ok(keys) => {
                tracing::warn!(
                    key = %host_key,
                    returned = keys.len(),
                    "destination translated one key into several; keeping the host key"
                );
                host_key
            }
            Err(err) => {
                tracing::warn!(
                    key = %host_key,
                    error = %err,
                    "destination could not translate a key; keeping the host key"
                );
                host_key
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn keys_follow_the_destination_that_stores_them() {
        assert_eq!(
            DestinationKeys::default()
                .translate("A: B/c?.m4b".into())
                .await,
            "A: B/c?.m4b"
        );
        let dir = tempfile::tempdir().unwrap();
        let local = bookclerk_storage::LocalFsBackend::new(dir.path().to_path_buf()).unwrap();
        let keys = DestinationKeys::of(&local);
        let expected = bookclerk_storage::key_syntax::translate_for_filesystem(
            &HostKey::parse("A: B/c?.m4b").unwrap(),
            bookclerk_storage::key_syntax::FilesystemRules::native(),
        );
        assert_eq!(keys.translate("A: B/c?.m4b".into()).await, expected);
        // Not a host key: kept as given.
        assert_eq!(keys.translate("a//b".into()).await, "a//b");
    }
}
