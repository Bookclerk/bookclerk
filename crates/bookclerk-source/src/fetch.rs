//! Host-side fetch options: packaging knobs plus scratch-cache limits.
//!
//! [`FetchOptions`] carries [`DownloadOptions`], which is built from the
//! host configuration, so it stays in this crate rather than in the slim
//! `bookclerk-storefront` crate guests link.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::SourceError;
use crate::options::DownloadOptions;

/// Options passed to [`crate::ContentSource::fetch_title`].
#[derive(Debug, Clone)]
pub struct FetchOptions {
    /// Packaging / naming knobs for this acquire (from `[output]` + destination).
    pub download: DownloadOptions,
    /// Absolute directory for scratch downloads for this fetch.
    pub cache_dir: PathBuf,
    /// Root files directory (`BOOKCLERK_FILES_DIR`). Used for CDM / Widevine path
    /// resolution by Audible. Non-auth operations only; auth is loaded from the
    /// [`bookclerk_library::LibraryStore`] passed directly to the trait method.
    pub files_dir: PathBuf,
    /// Optional on-disk budget for `cache_dir` (bytes). Sources should call
    /// [`Self::enforce_cache_budget`] between chunks.
    pub max_cache_bytes: Option<u64>,
    /// Cooperative cancel for mid-fetch quota or a lost job fence.
    pub cancel: Option<Arc<AtomicBool>>,
}

impl FetchOptions {
    /// Fails when cancel is set or `cache_dir` exceeds [`Self::max_cache_bytes`].
    ///
    /// # Errors
    ///
    /// Returns [`SourceError::Other`] when the fetch was cancelled or the
    /// cache directory is over the remaining scratch quota.
    pub fn enforce_cache_budget(&self) -> crate::Result<()> {
        if self
            .cancel
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::SeqCst))
        {
            return Err(SourceError::Other(anyhow::anyhow!("cancelled")));
        }
        if let Some(max) = self.max_cache_bytes {
            let used = sync_dir_size(&self.cache_dir);
            if used > max {
                return Err(SourceError::Other(anyhow::anyhow!(
                    "acquire scratch quota exceeded ({used} > {max} bytes)"
                )));
            }
        }
        Ok(())
    }
}

/// Recursive directory size; missing paths count as zero.
fn sync_dir_size(path: &Path) -> u64 {
    let mut total = 0u64;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in rd.flatten() {
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            if meta.is_dir() {
                stack.push(entry.path());
            } else {
                total = total.saturating_add(meta.len());
            }
        }
    }
    total
}
