//! Same-origin cover images: `GET /api/covers/{token}/{size}`.
//!
//! Storefront and metadata responses name covers by remote URL. Next to each,
//! the API hands out a same-origin path so a client need not load the remote
//! URL, and this module turns a request for that path into a JPEG.
//!
//! - **Tokens.** A cover path carries the remote URL and an HMAC of it under a
//!   key that never leaves this node, so the route only ever fetches URLs the
//!   server itself put in a response. A forged or edited token is a 404.
//! - **Fetching.** The first plugin that exports the `covers` entrypoint
//!   downloads the image (public HTTPS hosts only, size-capped); the host
//!   makes no request of its own.
//! - **Processing.** The media pool's confined cover worker checks the file
//!   signature, decodes under limits and re-encodes the 128 px, 360 px and
//!   full-size JPEGs, so the bytes served are never the bytes downloaded.
//! - **Cache.** The three JPEGs land in `cache/covers/<sha256(url)>/`, written
//!   to a temporary directory and renamed into place. Least recently served
//!   entries are evicted once the cache passes [`CACHE_BUDGET_BYTES`]. One
//!   request per URL does the work while the others wait for it, and a URL
//!   that failed is not retried for [`FAILURE_TTL`].

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use bookclerk_cover::token::{self, gravatar_url, is_coverable, TokenKey};
use bookclerk_media::{CoverSet, CoverSize};
use sha2::{Digest, Sha256};

/// Where the API mounts cover paths.
pub(crate) const ROUTE_PREFIX: &str = "/api/covers";

/// Where the API mounts signed images that are not covers (profile
/// pictures, remote logos): the same tokens, served as a 128 px JPEG with a
/// daily cache header because the remote image can change in place.
pub(crate) const IMAGE_PREFIX: &str = "/api/images";

/// Bytes of processed covers kept on disk before the least recently served
/// are evicted. Three JPEGs per cover average well under 300 KiB, so this
/// holds on the order of a thousand covers.
pub(crate) const CACHE_BUDGET_BYTES: u64 = 256 * 1024 * 1024;

/// How long a URL that could not be fetched or decoded is answered from
/// memory with the same failure instead of being fetched again.
pub(crate) const FAILURE_TTL: Duration = Duration::from_secs(10 * 60);

/// Most covers one search response warms in the background.
pub(crate) const PREFETCH_LIMIT: usize = 24;

/// Background prefetch batches downloading at once; a search while both are
/// busy prefetches nothing and its covers load on request.
const PREFETCH_BATCHES: usize = 2;

/// Failures remembered at once; older ones are dropped first when full.
const MAX_FAILURES: usize = 4096;

/// A served file is re-stamped as recently used at most this often.
const TOUCH_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// File holding this node's token key inside the cache directory.
const KEY_FILE: &str = "token.key";

/// Directory, inside the cache, where entries are assembled before they are
/// renamed into place.
const STAGING_DIR: &str = ".staging";

/// Why a cover could not be served.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CoverFailure {
    /// The token is malformed or was not issued by this node.
    UnknownToken,
    /// Nothing on this node can fetch or process covers right now (no
    /// `covers` plugin, isolation unavailable, or no cache directory).
    Unavailable(String),
    /// The download failed or the image was refused.
    Refused(String),
}

/// What the cover service needs from the rest of the daemon: a way to
/// download images and a way to process one.
pub(crate) trait CoverBackend: Send + Sync + 'static {
    /// Downloads `urls`, returning one outcome per URL in request order.
    ///
    /// # Errors
    ///
    /// [`CoverFailure::Unavailable`] when nothing can fetch covers at all.
    fn fetch(
        &self,
        urls: Vec<String>,
    ) -> impl Future<Output = Result<Vec<Result<Vec<u8>, String>>, CoverFailure>> + Send;

    /// Turns downloaded bytes into the served JPEG sizes.
    ///
    /// # Errors
    ///
    /// [`CoverFailure::Refused`] for an image that is not one, and
    /// [`CoverFailure::Unavailable`] when no confined worker can run.
    fn process(
        &self,
        bytes: Vec<u8>,
    ) -> impl Future<Output = Result<CoverSet, CoverFailure>> + Send;
}

/// The production backend: the integration registry's `covers` plugins and
/// the media pool's cover worker.
pub(crate) struct LiveBackend {
    /// Loaded integration guests, read when a cover is fetched.
    pub(crate) integrations: Arc<tokio::sync::RwLock<bookclerk_integrations::IntegrationRegistry>>,
}

impl CoverBackend for LiveBackend {
    async fn fetch(&self, urls: Vec<String>) -> Result<Vec<Result<Vec<u8>, String>>, CoverFailure> {
        let fetcher = self
            .integrations
            .read()
            .await
            .cover_fetchers()
            .into_iter()
            .next()
            .ok_or_else(|| CoverFailure::Unavailable("no deployed plugin fetches covers".into()))?;
        let max_bytes = u32::try_from(bookclerk_cover::MAX_INPUT_BYTES).unwrap_or(u32::MAX);
        let count = urls.len();
        let fetched = fetcher
            .fetch_covers(urls, max_bytes)
            .await
            .map_err(|err| CoverFailure::Unavailable(format!("cover fetch failed: {err}")))?;
        let mut outcomes: Vec<Result<Vec<u8>, String>> = fetched
            .into_iter()
            .map(|cover| match (cover.body, cover.error) {
                (Some(body), _) => Ok(body),
                (None, Some(error)) => Err(error),
                (None, None) => Err("no body".into()),
            })
            .collect();
        outcomes.resize_with(count, || Err("no answer for this URL".into()));
        outcomes.truncate(count);
        Ok(outcomes)
    }

    async fn process(&self, bytes: Vec<u8>) -> Result<CoverSet, CoverFailure> {
        bookclerk_media::pool()
            .process_cover(bytes)
            .await
            .map_err(|err| {
                use bookclerk_media::MediaError;
                match err {
                    MediaError::Cover(message) => CoverFailure::Refused(message),
                    MediaError::Worker { .. }
                    | MediaError::NotIsolated { .. }
                    | MediaError::InputMissing(_)
                    | MediaError::OutputMissing(_)
                    | MediaError::Mp4(_)
                    | MediaError::Native(_)
                    | MediaError::Io(_)
                    | MediaError::Other(_) => CoverFailure::Unavailable(err.to_string()),
                }
            })
    }
}

/// Signs cover URLs into same-origin paths and serves the processed images.
pub(crate) struct CoverService {
    /// `cache/covers`, or `None` for a service that only signs (tests).
    dir: Option<PathBuf>,
    /// Key for the token HMAC.
    key: TokenKey,
    /// Bytes of processed covers on disk, as last counted plus what was
    /// written since.
    used: Arc<AtomicU64>,
    /// Whether the cache was counted since start.
    counted: AtomicBool,
    /// Whether an eviction sweep is running.
    sweeping: Arc<AtomicBool>,
    /// Eviction threshold in bytes.
    budget: u64,
    /// One lock per URL being processed, by cache key.
    flights: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// Recent failures by cache key: when, and why.
    failures: Mutex<HashMap<String, (Instant, CoverFailure)>>,
    /// Cache keys a background batch is downloading, so overlapping
    /// searches do not download them again.
    prefetching: Mutex<HashSet<String>>,
    /// One permit per background batch, at most [`PREFETCH_BATCHES`].
    batches: Arc<tokio::sync::Semaphore>,
}

impl std::fmt::Debug for CoverService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CoverService")
            .field("dir", &self.dir)
            .field("budget", &self.budget)
            .finish_non_exhaustive()
    }
}

impl CoverService {
    /// A service caching under `dir`, creating it and its token key when
    /// missing.
    ///
    /// # Errors
    ///
    /// The I/O error when the directory or the key cannot be created or read.
    pub(crate) fn open(dir: PathBuf, budget: u64) -> std::io::Result<Self> {
        std::fs::create_dir_all(&dir)?;
        let key = load_or_create_key(&dir.join(KEY_FILE))?;
        // Leftovers of an interrupted write.
        let _ = std::fs::remove_dir_all(dir.join(STAGING_DIR));
        Ok(Self::with(Some(dir), key, budget))
    }

    /// A service that signs paths but has nowhere to cache, so every image
    /// request is [`CoverFailure::Unavailable`]. Used when the cache directory
    /// cannot be created, and by tests that do not serve covers.
    pub(crate) fn detached() -> Self {
        Self::with(None, random_key(), CACHE_BUDGET_BYTES)
    }

    /// The service from its parts.
    fn with(dir: Option<PathBuf>, key: TokenKey, budget: u64) -> Self {
        Self {
            dir,
            key,
            used: Arc::new(AtomicU64::new(0)),
            counted: AtomicBool::new(false),
            sweeping: Arc::new(AtomicBool::new(false)),
            budget,
            flights: Mutex::default(),
            failures: Mutex::default(),
            prefetching: Mutex::default(),
            batches: Arc::new(tokio::sync::Semaphore::new(PREFETCH_BATCHES)),
        }
    }

    /// The same-origin path for a remote cover URL, without a size: append
    /// `/128`, `/360` or `/full`.
    ///
    /// # Returns
    ///
    /// `None` for anything but an absolute `https` URL with a host, or one
    /// over 2048 bytes.
    pub(crate) fn path_for(&self, url: &str) -> Option<String> {
        let url = url.trim();
        if !is_coverable(url) {
            return None;
        }
        let token = token::sign(&self.key, url)?;
        Some(format!("{ROUTE_PREFIX}/{token}"))
    }

    /// Fills `path` from `url` for one response item.
    pub(crate) fn decorate(&self, url: Option<&str>, path: &mut Option<String>) {
        *path = url.and_then(|url| self.path_for(url));
    }

    /// The same-origin 128 px source for a remote image that is not a cover
    /// (a store icon, a sign-in provider's profile picture), so the web app
    /// can show it under an `img-src 'self'` policy. It is served by
    /// [`get_image`], which the browser re-checks daily, unlike a cover path.
    ///
    /// # Returns
    ///
    /// `None` for anything but an absolute `https` URL with a host, or one
    /// over 2048 bytes.
    pub(crate) fn thumb_src(&self, url: &str) -> Option<String> {
        let url = url.trim();
        if !is_coverable(url) {
            return None;
        }
        let token = token::sign(&self.key, url)?;
        Some(format!("{IMAGE_PREFIX}/{token}"))
    }

    /// An image source the web app may load under `img-src 'self'`: a
    /// same-origin path as is, a remote `https` URL as its
    /// [`Self::thumb_src`].
    ///
    /// # Returns
    ///
    /// `None` for anything else (a protocol-relative URL, plain `http`).
    pub(crate) fn same_origin_src(&self, src: &str) -> Option<String> {
        let src = src.trim();
        if src.starts_with('/') && !src.starts_with("//") && !src.starts_with("/\\") {
            Some(src.to_owned())
        } else {
            self.thumb_src(src)
        }
    }

    /// The remote URL a token names, when this node issued it.
    pub(crate) fn open_token(&self, token: &str) -> Option<String> {
        token::open(&self.key, token)
    }

    /// The JPEG for `size` of the cover a token names, fetching and processing
    /// it on first use.
    ///
    /// # Errors
    ///
    /// [`CoverFailure::UnknownToken`] for a token this node did not issue,
    /// and the cached or fresh failure otherwise.
    #[cfg(test)]
    pub(crate) async fn image(
        &self,
        backend: &impl CoverBackend,
        token: &str,
        size: CoverSize,
    ) -> Result<Vec<u8>, CoverFailure> {
        let url = self.open_token(token).ok_or(CoverFailure::UnknownToken)?;
        self.image_for_url(backend, url, size).await
    }

    /// [`Self::image_for_url`] in its own task, so a caller that is dropped
    /// (a request past its deadline, a cancelled acquire) never abandons a
    /// flight: the cover is still fetched, cached and its flight released,
    /// and the next request is served from disk.
    ///
    /// # Errors
    ///
    /// As [`Self::image_for_url`].
    pub(crate) async fn image_detached<B: CoverBackend>(
        self: &Arc<Self>,
        backend: Arc<B>,
        url: String,
        size: CoverSize,
    ) -> Result<Vec<u8>, CoverFailure> {
        let service = Arc::clone(self);
        tokio::spawn(async move { service.image_for_url(&*backend, url, size).await })
            .await
            .unwrap_or_else(|err| {
                Err(CoverFailure::Unavailable(format!(
                    "cover task failed: {err}"
                )))
            })
    }

    /// The JPEG for `size` of the cover at `url`, fetching and processing it
    /// on first use.
    ///
    /// # Errors
    ///
    /// The cached or fresh failure.
    pub(crate) async fn image_for_url(
        &self,
        backend: &impl CoverBackend,
        url: String,
        size: CoverSize,
    ) -> Result<Vec<u8>, CoverFailure> {
        if !is_coverable(&url) {
            return Err(CoverFailure::Refused("not an https URL".into()));
        }
        let dir = self
            .dir
            .clone()
            .ok_or_else(|| CoverFailure::Unavailable("no cover cache".into()))?;
        let key = cache_key(&url);
        if let Some(bytes) = read_cached(&dir, &key, size).await {
            return Ok(bytes);
        }
        self.recent_failure(&key)?;

        let flight = self.flight(&key);
        let guard = flight.lock().await;
        // Whoever held the lock may have just written it.
        if let Some(bytes) = read_cached(&dir, &key, size).await {
            drop(guard);
            self.land(&key, &flight);
            return Ok(bytes);
        }
        let outcome = match self.recent_failure(&key) {
            Err(failure) => Err(failure),
            Ok(()) => {
                let fetched = backend.fetch(vec![url]).await.and_then(|mut outcomes| {
                    outcomes
                        .pop()
                        .unwrap_or_else(|| Err("no answer".into()))
                        .map_err(CoverFailure::Refused)
                });
                match fetched {
                    Ok(bytes) => self.store(backend, &dir, &key, bytes).await,
                    Err(failure) => Err(failure),
                }
            }
        };
        // Recorded before the flight is released, so a request waiting on it
        // sees the failure instead of fetching again.
        if let Err(failure) = &outcome {
            self.note_failure(&key, failure);
        }
        drop(guard);
        self.land(&key, &flight);
        Ok(outcome?.get(size).to_vec())
    }

    /// Warms the cache for up to [`PREFETCH_LIMIT`] of `urls` in the
    /// background, in one download batch.
    ///
    /// Skips covers already cached, recently failed, being processed by a
    /// request or downloading in another batch, and does nothing while
    /// [`PREFETCH_BATCHES`] batches are already running.
    pub(crate) fn prefetch<B: CoverBackend>(
        self: &Arc<Self>,
        backend: Arc<B>,
        urls: impl IntoIterator<Item = String>,
    ) {
        let Some(dir) = self.dir.clone() else {
            return;
        };
        let mut seen = HashSet::new();
        let wanted: Vec<String> = urls
            .into_iter()
            .map(|url| url.trim().to_owned())
            .filter(|url| is_coverable(url) && seen.insert(url.clone()))
            .take(PREFETCH_LIMIT)
            .collect();
        if wanted.is_empty() {
            return;
        }
        let Ok(permit) = Arc::clone(&self.batches).try_acquire_owned() else {
            return;
        };
        let service = Arc::clone(self);
        tokio::spawn(async move {
            let _permit = permit;
            let mut todo = Vec::new();
            for url in wanted {
                let key = cache_key(&url);
                let cached = tokio::fs::try_exists(dir.join(&key)).await.unwrap_or(false);
                if !cached && service.recent_failure(&key).is_ok() {
                    todo.push((url, key));
                }
            }
            // Claim the keys no other batch is downloading.
            let reserved = Reserved::claim(&service, &mut todo);
            if todo.is_empty() {
                return;
            }
            // No flight is held while the batch downloads, so a request for
            // one of these covers never waits on the slowest of the others;
            // at worst it downloads the same image once more.
            let urls = todo.iter().map(|(url, _)| url.clone()).collect();
            let outcomes: Vec<Result<Vec<u8>, CoverFailure>> = match backend.fetch(urls).await {
                Ok(outcomes) => outcomes
                    .into_iter()
                    .map(|outcome| outcome.map_err(CoverFailure::Refused))
                    .collect(),
                Err(failure) => vec![Err(failure); todo.len()],
            };
            let mut work = tokio::task::JoinSet::new();
            for ((_, key), fetched) in todo.into_iter().zip(outcomes) {
                let service = Arc::clone(&service);
                let backend = Arc::clone(&backend);
                let dir = dir.clone();
                work.spawn(async move {
                    let flight = service.flight(&key);
                    // A request is processing it now.
                    let Ok(guard) = Arc::clone(&flight).try_lock_owned() else {
                        return;
                    };
                    let cached = tokio::fs::try_exists(dir.join(&key)).await.unwrap_or(false);
                    if !cached {
                        let outcome = match fetched {
                            Ok(bytes) => service.store(&*backend, &dir, &key, bytes).await,
                            Err(failure) => Err(failure),
                        };
                        if let Err(failure) = &outcome {
                            service.note_failure(&key, failure);
                        }
                    }
                    drop(guard);
                    service.land(&key, &flight);
                });
            }
            while work.join_next().await.is_some() {}
            drop(reserved);
        });
    }

    /// Processes downloaded bytes and writes the entry for `key`.
    async fn store(
        &self,
        backend: &impl CoverBackend,
        dir: &Path,
        key: &str,
        bytes: Vec<u8>,
    ) -> Result<CoverSet, CoverFailure> {
        let set = backend.process(bytes).await?;
        let written = {
            let dir = dir.to_path_buf();
            let key = key.to_owned();
            let set = set.clone();
            tokio::task::spawn_blocking(move || write_entry(&dir, &key, &set))
                .await
                .map_err(|err| err.to_string())
                .and_then(|result| result.map_err(|err| err.to_string()))
        };
        match written {
            Ok(bytes) => {
                self.used.fetch_add(bytes, Ordering::Relaxed);
                self.maybe_sweep(dir);
            }
            // Still serve this request; the next one tries the write again.
            Err(err) => tracing::warn!(error = %err, "could not cache a cover"),
        }
        Ok(set)
    }

    /// Starts an eviction sweep when the cache may be over budget, or was not
    /// counted since start.
    fn maybe_sweep(&self, dir: &Path) {
        let first = !self.counted.swap(true, Ordering::AcqRel);
        if !first && self.used.load(Ordering::Relaxed) <= self.budget {
            return;
        }
        if self.sweeping.swap(true, Ordering::AcqRel) {
            return;
        }
        let dir = dir.to_path_buf();
        let budget = self.budget;
        let sweeping = Arc::clone(&self.sweeping);
        let used = Arc::clone(&self.used);
        tokio::task::spawn_blocking(move || {
            // The count replaces the estimate. A cover written while the
            // sweep ran may be left out, which only delays the next sweep.
            used.store(sweep(&dir, budget), Ordering::Relaxed);
            sweeping.store(false, Ordering::Release);
        });
    }

    /// The cached failure for `key`, when one is still fresh.
    fn recent_failure(&self, key: &str) -> Result<(), CoverFailure> {
        let failures = lock(&self.failures);
        match failures.get(key) {
            Some((at, failure)) if at.elapsed() < FAILURE_TTL => Err(failure.clone()),
            _ => Ok(()),
        }
    }

    /// Remembers that `key` failed, for [`FAILURE_TTL`], unless the failure
    /// is [`CoverFailure::Unavailable`].
    fn note_failure(&self, key: &str, failure: &CoverFailure) {
        // Nothing can fetch or process at all: not this URL's fault, and a
        // plugin deployed a minute from now should not wait out the TTL.
        if !matches!(failure, CoverFailure::Unavailable(_)) {
            self.remember_failure(key, failure);
        }
    }

    /// Remembers that `key` failed, for [`FAILURE_TTL`].
    fn remember_failure(&self, key: &str, failure: &CoverFailure) {
        let mut failures = lock(&self.failures);
        if failures.len() >= MAX_FAILURES {
            failures.retain(|_, (at, _)| at.elapsed() < FAILURE_TTL);
            if failures.len() >= MAX_FAILURES {
                failures.clear();
            }
        }
        failures.insert(key.to_owned(), (Instant::now(), failure.clone()));
    }

    /// The lock for `key`'s single flight.
    fn flight(&self, key: &str) -> Arc<tokio::sync::Mutex<()>> {
        Arc::clone(lock(&self.flights).entry(key.to_owned()).or_default())
    }

    /// Forgets `key`'s flight lock once nobody else holds it.
    fn land(&self, key: &str, flight: &Arc<tokio::sync::Mutex<()>>) {
        let mut flights = lock(&self.flights);
        if flights
            .get(key)
            .is_some_and(|held| Arc::ptr_eq(held, flight) && Arc::strong_count(held) <= 2)
        {
            flights.remove(key);
        }
    }
}

/// Locks a map, recovering it after a panic elsewhere.
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Cache keys one prefetch batch claimed; released when it is dropped.
struct Reserved {
    /// The service whose `prefetching` set holds the keys.
    service: Arc<CoverService>,
    /// The claimed keys.
    keys: Vec<String>,
}

impl Reserved {
    /// Keeps in `todo` only the covers no other batch is downloading, and
    /// claims them.
    fn claim(service: &Arc<CoverService>, todo: &mut Vec<(String, String)>) -> Self {
        let mut prefetching = lock(&service.prefetching);
        todo.retain(|(_, key)| prefetching.insert(key.clone()));
        Self {
            service: Arc::clone(service),
            keys: todo.iter().map(|(_, key)| key.clone()).collect(),
        }
    }
}

impl Drop for Reserved {
    fn drop(&mut self) {
        let mut prefetching = lock(&self.service.prefetching);
        for key in &self.keys {
            prefetching.remove(key);
        }
    }
}

/// The cache directory name for `url`: lowercase hex SHA-256.
fn cache_key(url: &str) -> String {
    hex::encode(Sha256::digest(url.as_bytes()))
}

/// File name of one size inside an entry.
fn file_name(size: CoverSize) -> String {
    format!("{}.jpg", size.as_str())
}

/// Reads one size of a cached entry, marking it recently used.
async fn read_cached(dir: &Path, key: &str, size: CoverSize) -> Option<Vec<u8>> {
    let path = dir.join(key).join(file_name(size));
    let bytes = tokio::fs::read(&path).await.ok()?;
    tokio::task::spawn_blocking(move || touch(&path));
    Some(bytes)
}

/// Stamps `path` as used now, unless it was within [`TOUCH_INTERVAL`].
fn touch(path: &Path) {
    let Ok(file) = std::fs::File::options().write(true).open(path) else {
        return;
    };
    let stale = file
        .metadata()
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|modified| modified.elapsed().ok())
        .is_none_or(|age| age >= TOUCH_INTERVAL);
    if stale {
        let _ = file.set_modified(SystemTime::now());
    }
}

/// Writes the three sizes of `set` as entry `key`, atomically.
///
/// # Returns
///
/// Bytes written, or zero when another writer got there first.
///
/// # Errors
///
/// The I/O error when the entry cannot be written.
fn write_entry(dir: &Path, key: &str, set: &CoverSet) -> std::io::Result<u64> {
    let target = dir.join(key);
    if target.is_dir() {
        return Ok(0);
    }
    let staging = dir.join(STAGING_DIR);
    std::fs::create_dir_all(&staging)?;
    let mut nonce = [0u8; 8];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut nonce);
    let temp = staging.join(format!("{key}-{}", hex::encode(nonce)));
    std::fs::create_dir(&temp)?;
    let mut total = 0u64;
    let result = (|| {
        for size in CoverSize::ALL {
            let bytes = set.get(size);
            std::fs::write(temp.join(file_name(size)), bytes)?;
            total += u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        }
        std::fs::rename(&temp, &target)
    })();
    if let Err(err) = result {
        let _ = std::fs::remove_dir_all(&temp);
        return if target.is_dir() { Ok(0) } else { Err(err) };
    }
    Ok(total)
}

/// Deletes the least recently used entries until the cache is at most 90%
/// of `budget`.
///
/// # Returns
///
/// Bytes left in the cache.
fn sweep(dir: &Path, budget: u64) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut found: Vec<(SystemTime, u64, PathBuf)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let is_entry = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.len() == 64 && name.bytes().all(|b| b.is_ascii_hexdigit()));
        if !is_entry || !path.is_dir() {
            continue;
        }
        let mut bytes = 0u64;
        let mut newest = SystemTime::UNIX_EPOCH;
        for file in std::fs::read_dir(&path).into_iter().flatten().flatten() {
            if let Ok(meta) = file.metadata() {
                bytes += meta.len();
                if let Ok(modified) = meta.modified() {
                    newest = newest.max(modified);
                }
            }
        }
        found.push((newest, bytes, path));
    }
    let mut total: u64 = found.iter().map(|(_, bytes, _)| bytes).sum();
    if total <= budget {
        return total;
    }
    let target = budget / 10 * 9;
    found.sort_by_key(|(newest, ..)| *newest);
    for (_, bytes, path) in found {
        if total <= target {
            break;
        }
        if std::fs::remove_dir_all(&path).is_ok() {
            total = total.saturating_sub(bytes);
        }
    }
    total
}

/// A fresh token key from the thread-local CSPRNG (ChaCha seeded from the OS).
fn random_key() -> TokenKey {
    TokenKey::from_bytes(rand::random())
}

/// Reads the token key, or creates it (owner-only on Unix).
fn load_or_create_key(path: &Path) -> std::io::Result<TokenKey> {
    match std::fs::read(path) {
        Ok(bytes) => {
            if let Ok(key) = <[u8; 32]>::try_from(bytes.as_slice()) {
                return Ok(TokenKey::from_bytes(key));
            }
            // A truncated or foreign file: replace it.
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        // Unreadable now does not mean lost: keep it, and the paths it signed.
        Err(err) => return Err(err),
    }
    let key = random_key();
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    std::io::Write::write_all(&mut file, key.as_bytes())?;
    Ok(key)
}

/// Serves one size of a cover the API linked to.
///
/// The path comes from a `cover_path` field in a Discover or catalog
/// response; append `/128`, `/360` or `/full`. The first request fetches the
/// image through the `covers` plugin and re-encodes it in the confined media
/// worker; later ones are served from the node's cover cache. Responses are
/// immutable: a path always names the same image.
#[utoipa::path(
    get,
    path = "/api/covers/{token}/{size}",
    params(
        ("token" = String, Path, description = "Signed cover token from a `cover_path` field."),
        ("size" = String, Path, description = "`128` (thumbnail), `360` (card) or `full`."),
    ),
    responses(
        (status = 200, description = "The cover as a JPEG.", content_type = "image/jpeg"),
        (status = 404, description = "The token was not issued by this node, or the size is unknown."),
        (status = 502, description = "The image could not be downloaded or is not a supported image."),
        (status = 503, description = "This node cannot fetch or process covers right now."),
    ),
)]
pub(crate) async fn get_cover(
    axum::extract::State(state): axum::extract::State<Arc<crate::api::AppState>>,
    axum::extract::Path((token, size)): axum::extract::Path<(String, String)>,
) -> axum::response::Response {
    use axum::http::StatusCode;
    use axum::response::IntoResponse;

    let Some(size) = CoverSize::parse(&size) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let url = state.covers.open_token(&token);
    serve(&state, url, size, IMMUTABLE).await
}

/// `Cache-Control` for a cover path, which always names the same image.
const IMMUTABLE: &str = "private, max-age=31536000, immutable";

/// `Cache-Control` for an image whose remote copy can change under the same
/// address (a Gravatar, a store icon): the browser checks again daily.
const DAILY: &str = "private, max-age=86400";

/// Serves one size of the image at `url` through the cover service.
///
/// # Arguments
///
/// * `state` - Daemon state holding the cover service and the plugins that
///   fetch for it.
/// * `url` - The remote image; `None` is a 404.
/// * `size` - The size to serve.
/// * `cache_control` - `Cache-Control` for a successful answer.
async fn serve(
    state: &crate::api::AppState,
    url: Option<String>,
    size: CoverSize,
    cache_control: &'static str,
) -> axum::response::Response {
    use axum::http::{header, StatusCode};
    use axum::response::IntoResponse;

    let backend = Arc::new(LiveBackend {
        integrations: Arc::clone(&state.integrations),
    });
    let outcome = match url {
        Some(url) => state.covers.image_detached(backend, url, size).await,
        None => Err(CoverFailure::UnknownToken),
    };
    match outcome {
        Ok(bytes) => (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, "image/jpeg"),
                (header::CACHE_CONTROL, cache_control),
            ],
            bytes,
        )
            .into_response(),
        Err(CoverFailure::UnknownToken) => StatusCode::NOT_FOUND.into_response(),
        Err(CoverFailure::Refused(reason)) => {
            tracing::debug!(reason = %reason, "cover refused");
            failure_response(StatusCode::BAD_GATEWAY)
        }
        Err(CoverFailure::Unavailable(reason)) => {
            tracing::warn!(reason = %reason, "cover unavailable");
            failure_response(StatusCode::SERVICE_UNAVAILABLE)
        }
    }
}

/// Serves a signed image that is not a cover as a 128 px JPEG.
///
/// The path comes from a `picture_path` or `logo` field: a sign-in
/// provider's profile picture or a plugin's remote logo. It is fetched and
/// processed like a cover, but the browser re-checks it daily because the
/// remote image can change at the same address.
#[utoipa::path(
    get,
    path = "/api/images/{token}",
    params(
        ("token" = String, Path, description = "Signed token from a `picture_path` or `logo` field."),
    ),
    responses(
        (status = 200, description = "The image as a JPEG.", content_type = "image/jpeg"),
        (status = 404, description = "The token was not issued by this node."),
        (status = 502, description = "The image could not be downloaded or is not a supported image."),
        (status = 503, description = "This node cannot fetch or process images right now."),
    ),
)]
pub(crate) async fn get_image(
    axum::extract::State(state): axum::extract::State<Arc<crate::api::AppState>>,
    axum::extract::Path(token): axum::extract::Path<String>,
) -> axum::response::Response {
    let url = state.covers.open_token(&token);
    serve(&state, url, CoverSize::Thumb, DAILY).await
}

/// Serves a user's Gravatar as a 128 px JPEG through the cover service.
///
/// The `gravatar_hash` of a user (from `/api/auth/me` or `/api/users`) names
/// the picture. Bookclerk downloads it through the `covers` plugin, so the
/// browser never contacts Gravatar.
#[utoipa::path(
    get,
    path = "/api/avatars/gravatar/{hash}",
    params(
        ("hash" = String, Path, description = "A user's `gravatar_hash`: SHA-256 of the email, 64 lowercase hex digits."),
    ),
    responses(
        (status = 200, description = "The picture as a JPEG.", content_type = "image/jpeg"),
        (status = 404, description = "The hash is not 64 lowercase hex digits."),
        (status = 502, description = "The email has no Gravatar, or the image is not a supported image."),
        (status = 503, description = "This node cannot fetch or process images right now."),
    ),
)]
pub(crate) async fn get_gravatar(
    axum::extract::State(state): axum::extract::State<Arc<crate::api::AppState>>,
    axum::extract::Path(hash): axum::extract::Path<String>,
) -> axum::response::Response {
    serve(&state, gravatar_url(&hash), CoverSize::Thumb, DAILY).await
}

/// Serves a storefront's or integration's icon as a 128 px JPEG through the
/// cover service.
///
/// The icon is the one the plugin names for its sign-in button. A plugin
/// that is not deployed on this node, or names no icon, is a 404.
#[utoipa::path(
    get,
    path = "/api/stores/{id}/icon",
    params(
        ("id" = String, Path, description = "Storefront or integration id (`audible`, `libro`, `audiobookshelf`, …)."),
    ),
    responses(
        (status = 200, description = "The icon as a JPEG.", content_type = "image/jpeg"),
        (status = 404, description = "No deployed storefront or integration has this id, or it names no icon."),
        (status = 502, description = "The icon could not be downloaded or is not a supported image."),
        (status = 503, description = "This node cannot fetch or process images right now."),
    ),
)]
pub(crate) async fn get_store_icon(
    axum::extract::State(state): axum::extract::State<Arc<crate::api::AppState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> axum::response::Response {
    let url = store_icon_url(&state, &id).await;
    serve(&state, url, CoverSize::Thumb, DAILY).await
}

/// The icon a deployed storefront, or failing that an integration, names
/// for `id`.
async fn store_icon_url(state: &crate::api::AppState, id: &str) -> Option<String> {
    let source = state.sources.read().await.get(id);
    let href = match source {
        Some(source) => source.portal_brand().logo_href().to_owned(),
        None => {
            let integration = state.integrations.read().await.get(id);
            integration
                .and_then(|integration| integration.portal_brand())?
                .logo_href()
                .to_owned()
        }
    };
    let href = href.trim();
    (!href.is_empty()).then(|| href.to_owned())
}

/// A failed cover answer the browser may retry after a while, not cache.
fn failure_response(status: axum::http::StatusCode) -> axum::response::Response {
    use axum::response::IntoResponse;
    (status, [(axum::http::header::CACHE_CONTROL, "no-store")]).into_response()
}

/// The acquire overlay's covers: the provider's cover URL through this
/// node's cover service, so the acquire never requests it itself.
pub(crate) struct AcquireCovers {
    /// The node's cover service.
    pub(crate) service: Arc<CoverService>,
    /// Fetches and processes for it.
    pub(crate) backend: Arc<LiveBackend>,
}

#[async_trait::async_trait]
impl bookclerk_acquire::CoverImages for AcquireCovers {
    async fn full_cover(&self, url: &str) -> Option<Vec<u8>> {
        match self
            .service
            .image_detached(
                Arc::clone(&self.backend),
                url.trim().to_owned(),
                CoverSize::Full,
            )
            .await
        {
            Ok(jpeg) => Some(jpeg),
            Err(failure) => {
                tracing::debug!(?failure, "overlay cover unavailable");
                None
            }
        }
    }
}

/// The acquire metadata a node's acquires use: its deployed metadata
/// providers, with covers through its cover service.
pub(crate) async fn acquire_metadata(
    state: &crate::api::AppState,
) -> bookclerk_acquire::AcquireMetadata {
    bookclerk_acquire::AcquireMetadata::new(Arc::new(state.integrations.read().await.clone()))
        .with_covers(Arc::new(AcquireCovers {
            service: Arc::clone(&state.covers),
            backend: Arc::new(LiveBackend {
                integrations: Arc::clone(&state.integrations),
            }),
        }))
}

/// Warms the cache for the covers a response is about to link to.
pub(crate) fn prefetch(state: &crate::api::AppState, urls: impl IntoIterator<Item = String>) {
    let backend = Arc::new(LiveBackend {
        integrations: Arc::clone(&state.integrations),
    });
    state.covers.prefetch(backend, urls);
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;
    use std::sync::atomic::AtomicUsize;

    /// A 1×1 opaque PNG.
    const PNG: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00, 0x00, 0x90,
        0x77, 0x53, 0xDE, 0x00, 0x00, 0x00, 0x0C, 0x49, 0x44, 0x41, 0x54, 0x08, 0xD7, 0x63, 0xF8,
        0xCF, 0xC0, 0x00, 0x00, 0x03, 0x01, 0x01, 0x00, 0x18, 0xDD, 0x8D, 0xB0, 0x00, 0x00, 0x00,
        0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];

    /// Serves [`PNG`] for every URL containing `ok`, an error otherwise, and
    /// processes in this process.
    #[derive(Default)]
    struct Fake {
        /// URLs asked for, across calls.
        fetched: AtomicUsize,
        /// Calls to `fetch`.
        calls: AtomicUsize,
        /// When set, `fetch` reports that nothing can fetch.
        unavailable: bool,
    }

    impl CoverBackend for Fake {
        async fn fetch(
            &self,
            urls: Vec<String>,
        ) -> Result<Vec<Result<Vec<u8>, String>>, CoverFailure> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.unavailable {
                return Err(CoverFailure::Unavailable("no fetcher".into()));
            }
            self.fetched.fetch_add(urls.len(), Ordering::SeqCst);
            // Let concurrent callers pile up behind the flight lock.
            tokio::time::sleep(Duration::from_millis(20)).await;
            Ok(urls
                .iter()
                .map(|url| {
                    if url.contains("ok") {
                        Ok(PNG.to_vec())
                    } else if url.contains("html") {
                        Ok(b"<html>".to_vec())
                    } else {
                        Err("HTTP 404".into())
                    }
                })
                .collect())
        }

        async fn process(&self, bytes: Vec<u8>) -> Result<CoverSet, CoverFailure> {
            bookclerk_cover::process(&bytes).map_err(|err| CoverFailure::Refused(err.to_string()))
        }
    }

    fn service(dir: &Path) -> Arc<CoverService> {
        Arc::new(CoverService::open(dir.join("covers"), CACHE_BUDGET_BYTES).expect("open"))
    }

    fn token_for(service: &CoverService, url: &str) -> String {
        service
            .path_for(url)
            .expect("path")
            .strip_prefix(&format!("{ROUTE_PREFIX}/"))
            .expect("prefix")
            .to_owned()
    }

    #[test]
    fn tokens_round_trip_and_refuse_tampering() {
        let service = CoverService::detached();
        let url = "https://img.example/a.jpg?x=1";
        let token = token_for(&service, url);
        assert_eq!(service.open_token(&token).as_deref(), Some(url));

        // Another node's key, an edited URL, a cut tag, junk.
        assert_eq!(CoverService::detached().open_token(&token), None);
        let (encoded, mac) = token.split_once('.').expect("dot");
        let other =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode("https://evil.example/");
        assert_eq!(service.open_token(&format!("{other}.{mac}")), None);
        assert_eq!(
            service.open_token(&format!("{encoded}.{}", &mac[..4])),
            None
        );
        assert_eq!(service.open_token(encoded), None);
        assert_eq!(service.open_token("..."), None);

        for refused in [
            "http://img.example/a.jpg",
            "data:image/png;base64,AA",
            "/local.jpg",
            "",
        ] {
            assert_eq!(service.path_for(refused), None, "{refused}");
        }
        assert_eq!(
            service.path_for(&format!(
                "https://a.example/{}",
                "x".repeat(token::MAX_URL_BYTES)
            )),
            None
        );

        let mut path = Some("stale".to_owned());
        service.decorate(None, &mut path);
        assert_eq!(path, None);
        service.decorate(Some(url), &mut path);
        assert!(path.is_some_and(|path| path.starts_with("/api/covers/")));
    }

    #[test]
    fn image_sources_stay_on_this_origin() {
        let temp = tempfile::tempdir().expect("tempdir");
        let service = service(temp.path());
        let url = "https://avatars.example/u/7.png";

        let thumb = service.thumb_src(url).expect("signed");
        let token = thumb
            .strip_prefix("/api/images/")
            .expect("an image path, not an immutable cover path");
        assert_eq!(service.open_token(token).as_deref(), Some(url));
        assert_eq!(service.thumb_src("http://avatars.example/u/7.png"), None);

        assert_eq!(service.same_origin_src(url), Some(thumb));
        assert_eq!(
            service
                .same_origin_src(" /api/plugins/source/libro/logo ")
                .as_deref(),
            Some("/api/plugins/source/libro/logo")
        );
        for refused in [
            "//evil.example/x.png",
            "/\\evil.example/x.png",
            "data:image/png,x",
            "",
        ] {
            assert_eq!(service.same_origin_src(refused), None, "{refused}");
        }
    }

    #[test]
    fn only_a_sha256_names_a_gravatar() {
        let hash = "0123456789abcdef".repeat(4);
        assert_eq!(
            gravatar_url(&hash).as_deref(),
            Some(format!("https://www.gravatar.com/avatar/{hash}?d=404&s=360").as_str())
        );
        for refused in [
            String::new(),
            hash.to_uppercase(),
            hash[1..].to_owned(),
            format!("{hash}0"),
            format!("{}/../x", &hash[..60]),
            format!("{}?s=1", &hash[..60]),
        ] {
            assert_eq!(gravatar_url(&refused), None, "{refused}");
        }
    }

    #[test]
    fn the_token_key_survives_a_restart() {
        let temp = tempfile::tempdir().expect("tempdir");
        let url = "https://img.example/a.jpg";
        let first = token_for(&service(temp.path()), url);
        assert_eq!(
            service(temp.path()).open_token(&first).as_deref(),
            Some(url)
        );
    }

    #[test]
    fn an_unreadable_key_is_kept_and_a_short_one_replaced() {
        let temp = tempfile::tempdir().expect("tempdir");
        // A directory where the key should be cannot be read as one.
        let blocked = temp.path().join("blocked");
        std::fs::create_dir_all(blocked.join(KEY_FILE)).expect("dir");
        assert!(load_or_create_key(&blocked.join(KEY_FILE)).is_err());
        assert!(blocked.join(KEY_FILE).is_dir());
        let short = temp.path().join("short");
        std::fs::write(&short, b"too short").expect("write");
        let key = load_or_create_key(&short).expect("replaced");
        assert_eq!(std::fs::read(&short).expect("read"), *key.as_bytes());
        assert_eq!(format!("{key:?}"), "TokenKey(..)");
    }

    #[tokio::test]
    async fn a_dropped_caller_still_caches_the_cover_and_releases_its_flight() {
        let temp = tempfile::tempdir().expect("tempdir");
        let service = service(temp.path());
        let backend = Arc::new(Fake::default());
        let url = "https://img.example/ok-dropped.png".to_owned();
        // The caller gives up while the download is still sleeping.
        let gave_up = tokio::time::timeout(
            Duration::from_millis(1),
            service.image_detached(Arc::clone(&backend), url.clone(), CoverSize::Card),
        )
        .await;
        assert!(gave_up.is_err());
        let entry = temp.path().join("covers").join(cache_key(&url));
        for _ in 0..200 {
            if entry.join("full.jpg").is_file() && lock(&service.flights).is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(entry.join("full.jpg").is_file());
        assert!(lock(&service.flights).is_empty());
        let bytes = service
            .image_detached(Arc::clone(&backend), url, CoverSize::Full)
            .await
            .expect("cached");
        assert!(bytes.starts_with(&[0xFF, 0xD8, 0xFF]));
        assert_eq!(backend.fetched.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_cover_is_fetched_once_and_then_served_from_disk() {
        let temp = tempfile::tempdir().expect("tempdir");
        let service = service(temp.path());
        let backend = Arc::new(Fake::default());
        let token = token_for(&service, "https://img.example/ok.png");

        let mut tasks = Vec::new();
        for size in [
            CoverSize::Thumb,
            CoverSize::Card,
            CoverSize::Full,
            CoverSize::Thumb,
        ] {
            let service = Arc::clone(&service);
            let backend = Arc::clone(&backend);
            let token = token.clone();
            tasks.push(tokio::spawn(async move {
                service.image(&*backend, &token, size).await
            }));
        }
        for task in tasks {
            let bytes = task.await.expect("join").expect("image");
            assert!(bytes.starts_with(&[0xFF, 0xD8, 0xFF]));
        }
        assert_eq!(backend.fetched.load(Ordering::SeqCst), 1);
        service
            .image(&*backend, &token, CoverSize::Full)
            .await
            .expect("cached");
        assert_eq!(backend.fetched.load(Ordering::SeqCst), 1);
        let entry = temp
            .path()
            .join("covers")
            .join(cache_key("https://img.example/ok.png"));
        for name in ["128.jpg", "360.jpg", "full.jpg"] {
            assert!(entry.join(name).is_file(), "{name}");
        }
        assert!(lock(&service.flights).is_empty());
    }

    #[tokio::test]
    async fn failures_are_remembered_and_unknown_tokens_refused() {
        let temp = tempfile::tempdir().expect("tempdir");
        let service = service(temp.path());
        let backend = Fake::default();
        for url in [
            "https://img.example/missing.png",
            "https://img.example/html",
        ] {
            let token = token_for(&service, url);
            for _ in 0..2 {
                let err = service
                    .image(&backend, &token, CoverSize::Thumb)
                    .await
                    .expect_err("refused");
                assert!(matches!(err, CoverFailure::Refused(_)), "{err:?}");
            }
        }
        assert_eq!(backend.calls.load(Ordering::SeqCst), 2);
        assert_eq!(
            service.image(&backend, "nope.nope", CoverSize::Thumb).await,
            Err(CoverFailure::UnknownToken)
        );

        let detached = CoverService::detached();
        let token = token_for(&detached, "https://img.example/ok.png");
        assert!(matches!(
            detached.image(&backend, &token, CoverSize::Thumb).await,
            Err(CoverFailure::Unavailable(_))
        ));
        let unavailable = Fake {
            unavailable: true,
            ..Fake::default()
        };
        let token = token_for(&service, "https://img.example/ok-2.png");
        assert!(matches!(
            service.image(&unavailable, &token, CoverSize::Thumb).await,
            Err(CoverFailure::Unavailable(_))
        ));
        // Not remembered: once something can fetch, the cover is served.
        service
            .image(&backend, &token, CoverSize::Thumb)
            .await
            .expect("fetched once a fetcher is there");
    }

    #[tokio::test]
    async fn prefetch_warms_new_covers_in_one_batch() {
        let temp = tempfile::tempdir().expect("tempdir");
        let service = service(temp.path());
        let backend = Arc::new(Fake::default());
        let urls = [
            "https://img.example/ok-1.png",
            "https://img.example/ok-2.png",
            "https://img.example/ok-1.png",
            "https://img.example/gone.png",
            "http://plain.example/ok.png",
        ];
        service.prefetch(
            Arc::clone(&backend),
            urls.iter().map(|url| (*url).to_owned()),
        );
        let dir = temp.path().join("covers");
        for _ in 0..200 {
            if dir.join(cache_key(urls[1])).is_dir() && dir.join(cache_key(urls[0])).is_dir() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(backend.calls.load(Ordering::SeqCst), 1);
        assert_eq!(backend.fetched.load(Ordering::SeqCst), 3);
        let token = token_for(&service, urls[0]);
        service
            .image(&*backend, &token, CoverSize::Card)
            .await
            .expect("warm");
        assert_eq!(backend.calls.load(Ordering::SeqCst), 1);
        // Already cached: nothing to fetch.
        service.prefetch(Arc::clone(&backend), [urls[0].to_owned()]);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(backend.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn overlapping_prefetches_download_each_cover_once_and_are_bounded() {
        let temp = tempfile::tempdir().expect("tempdir");
        let service = service(temp.path());
        let backend = Arc::new(Fake::default());
        let page = |n: usize| (0..3).map(move |i| format!("https://img.example/ok-{n}-{i}.png"));
        // The first batch is still downloading when the second search asks
        // for the same covers, and two batches are already running when the
        // third arrives.
        service.prefetch(Arc::clone(&backend), page(1));
        tokio::task::yield_now().await;
        service.prefetch(Arc::clone(&backend), page(1).chain(page(2)));
        service.prefetch(Arc::clone(&backend), page(3));
        let dir = temp.path().join("covers");
        for _ in 0..200 {
            if page(1)
                .chain(page(2))
                .all(|url| dir.join(cache_key(&url)).is_dir())
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(backend.fetched.load(Ordering::SeqCst), 6);
        assert!(lock(&service.prefetching).is_empty());
        assert!(page(3).all(|url| !dir.join(cache_key(&url)).exists()));
    }

    #[test]
    fn the_sweep_evicts_least_recently_used_entries() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = temp.path();
        let set = bookclerk_cover::process(PNG).expect("set");
        let mut keys = Vec::new();
        for n in 0..4 {
            let key = cache_key(&format!("https://img.example/{n}.png"));
            let written = write_entry(dir, &key, &set).expect("write");
            assert!(written > 0);
            assert_eq!(write_entry(dir, &key, &set).expect("again"), 0);
            let age = Duration::from_secs(3600 * (10 - n));
            for size in CoverSize::ALL {
                let file = std::fs::File::options()
                    .write(true)
                    .open(dir.join(&key).join(file_name(size)))
                    .expect("open");
                file.set_modified(SystemTime::now() - age).expect("stamp");
            }
            keys.push((key, written));
        }
        let one = keys[0].1;
        // Room for two entries: the two oldest go.
        let left = sweep(dir, one * 2 + one / 2);
        assert!(!dir.join(&keys[0].0).exists());
        assert!(!dir.join(&keys[1].0).exists());
        assert!(dir.join(&keys[2].0).exists());
        assert!(dir.join(&keys[3].0).exists());
        assert_eq!(left, one * 2);
        // Under budget: nothing happens.
        assert_eq!(sweep(dir, u64::MAX), one * 2);
        assert!(!dir
            .join(STAGING_DIR)
            .read_dir()
            .expect("staging")
            .any(|_| true));
    }
}
