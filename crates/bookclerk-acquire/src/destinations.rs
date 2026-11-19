//! Destination-specific storage backends for one acquire operation.

use bookclerk_config::{DestinationId, MultiDestinationMode};
use bookclerk_source::DownloadOptions;
use bookclerk_storage::{FanoutBackend, StorageBackend};

use crate::error::Result;

/// One configured output destination for an acquire run.
pub struct AcquireDestination {
    /// The destination's id: its storage plugin's alias (`local`, `s3`, …).
    pub id: DestinationId,
    /// Constructed storage backend for this destination.
    pub backend: Box<dyn StorageBackend>,
    /// Source download options (quality, chapter prefs, …).
    pub options: DownloadOptions,
}

/// Ordered set of acquire destinations built from the deployed destination plugins.
pub struct AcquireDestinations {
    /// Configured acquire destinations in evaluation order.
    pub items: Vec<AcquireDestination>,
    /// The primary destination, whose keys the library records as the
    /// title's storage key.
    pub primary: DestinationId,
    /// Whether multiple destinations are enabled for this run.
    pub multi_destination: MultiDestinationMode,
}

impl AcquireDestinations {
    /// Number of destinations.
    #[must_use]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Returns true when no destinations are deployed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Returns the primary destination used for library storage keys.
    #[must_use]
    pub fn primary_destination(&self) -> &AcquireDestination {
        self.items
            .iter()
            .find(|dest| dest.id == self.primary)
            .unwrap_or_else(|| &self.items[0])
    }

    /// Returns the destination `id`, if configured.
    #[must_use]
    pub fn destination(&self, id: &DestinationId) -> Option<&AcquireDestination> {
        self.items.iter().find(|dest| &dest.id == id)
    }

    /// Clone destination backends into a listing/match storage handle.
    ///
    /// Reuses the backends acquire already built instead of spawning the
    /// destination guests a second time.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub fn listing_backend(&self) -> Result<Box<dyn StorageBackend>> {
        let backends = self
            .items
            .iter()
            .map(|dest| dest.backend.clone_box())
            .collect::<Vec<_>>();
        if backends.len() == 1 {
            let mut backends = backends;
            return Ok(backends.remove(0));
        }
        Ok(Box::new(FanoutBackend::new(backends)?))
    }

    /// Consumes this set and returns a listing backend over the primary destination.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub fn into_listing_backend(self) -> Result<Box<dyn StorageBackend>> {
        let backends = self
            .items
            .into_iter()
            .map(|dest| dest.backend)
            .collect::<Vec<_>>();
        if backends.len() == 1 {
            let mut backends = backends;
            return Ok(backends.remove(0));
        }
        Ok(Box::new(FanoutBackend::new(backends)?))
    }
}
