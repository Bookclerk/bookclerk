//! Host-side fetch options: packaging knobs plus scratch-cache limits.
//!
//! [`FetchOptions`] carries [`DownloadOptions`], which is built from the
//! host configuration, so it stays in this crate rather than in the slim
//! `bookclerk-storefront` crate guests link.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::options::DownloadOptions;
use crate::SourceError;

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

#[cfg(test)]
mod tests {
    use super::*;

    /// Fetch options over `cache_dir` with an optional budget and cancel flag.
    fn options(
        cache_dir: &Path,
        max: Option<u64>,
        cancel: Option<Arc<AtomicBool>>,
    ) -> FetchOptions {
        FetchOptions {
            download: DownloadOptions::default(),
            cache_dir: cache_dir.to_path_buf(),
            files_dir: cache_dir.to_path_buf(),
            max_cache_bytes: max,
            cancel,
        }
    }

    #[test]
    fn budget_counts_nested_files_and_fails_over_quota() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(dir.path().join("part")).expect("mkdir");
        std::fs::write(dir.path().join("a.bin"), [0u8; 10]).expect("write");
        std::fs::write(dir.path().join("part").join("b.bin"), [0u8; 6]).expect("write");

        assert_eq!(sync_dir_size(dir.path()), 16);
        assert!(options(dir.path(), Some(16), None)
            .enforce_cache_budget()
            .is_ok());
        assert!(options(dir.path(), None, None)
            .enforce_cache_budget()
            .is_ok());
        let err = options(dir.path(), Some(15), None)
            .enforce_cache_budget()
            .expect_err("over quota");
        assert!(err.to_string().contains("16 > 15"), "{err}");
    }

    #[test]
    fn budget_fails_once_cancelled_and_missing_dirs_are_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("missing");
        assert_eq!(sync_dir_size(&missing), 0);

        let cancel = Arc::new(AtomicBool::new(false));
        let opts = options(&missing, Some(0), Some(Arc::clone(&cancel)));
        assert!(opts.enforce_cache_budget().is_ok());
        cancel.store(true, Ordering::SeqCst);
        let err = opts.enforce_cache_budget().expect_err("cancelled");
        assert!(err.to_string().contains("cancelled"), "{err}");
    }
}
