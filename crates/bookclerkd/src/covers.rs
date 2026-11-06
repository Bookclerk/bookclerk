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
//!   to a temporary directory and renamed into place. The cache's journal
//!   index (see [`index`]) records when each entry was fetched, until when
//!   it is fresh, upstream's validators and when it was last served. Least
//!   recently served entries are evicted once the cache passes
//!   [`CACHE_BUDGET_BYTES`]. One request per URL does the work while the
//!   others wait for it, and a URL that failed is not retried for
//!   [`FAILURE_TTL`].
//! - **Freshness.** Upstream's caching headers decide how long an image is
//!   fresh (see [`upstream_policy`]); an image sent with `no-store` is
//!   served but not kept. A cover path always names the same image, so a
//!   cached cover is never refreshed. Any other image (a profile picture, a
//!   store icon) is served from the cache while fresh; once stale, the
//!   cached copy is still served while a background request revalidates it
//!   with upstream's validators. A `304` extends its lifetime, a new image
//!   replaces it, and a failure keeps the old copy.

mod index;

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use bookclerk_cover::http_cache::{
    upstream_policy, Storage, UpstreamHeaders, UpstreamPolicy, Validators,
};
use bookclerk_cover::token::{self, gravatar_url, is_coverable, TokenKey};
use bookclerk_media::{CoverSet, CoverSize};
use index::{Entry, Index, Stamp};
use sha2::{Digest, Sha256};

/// Where the API mounts cover paths.
pub(crate) const ROUTE_PREFIX: &str = "/api/covers";

/// Where the API mounts signed images that are not covers (profile
/// pictures, remote logos): the same tokens, served as a 128 px JPEG with a
/// daily browser cache header instead of `immutable`.
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

/// Downloads at once for images whose address the caller picks (a
/// Gravatar hash), so a burst of new addresses can't queue unbounded
/// fetches; a request over the limit is answered `503` and the web app
/// shows its fallback.
const OPEN_FETCHES: usize = 4;

/// Background revalidations of stale images running at once; a stale image
/// served while all are busy is checked on a later request instead.
const REVALIDATIONS: usize = 4;

/// Failures remembered at once; older ones are dropped first when full.
const MAX_FAILURES: usize = 4096;

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

/// One image to download, with what to revalidate a cached copy with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FetchRequest {
    /// The remote image.
    pub(crate) url: String,
    /// Upstream's validators from the cached copy; empty for a plain fetch.
    pub(crate) validators: Validators,
}

impl FetchRequest {
    /// A plain fetch of `url`, for an image with no cached copy.
    fn plain(url: String) -> Self {
        Self {
            url,
            validators: Validators::default(),
        }
    }
}

/// What came back for one [`FetchRequest`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Download {
    /// The image, still to be processed, and what upstream's headers say
    /// about keeping it.
    Body {
        /// The downloaded bytes, untrusted.
        bytes: Vec<u8>,
        /// Upstream's caching policy for them.
        policy: UpstreamPolicy,
    },
    /// Upstream answered `304 Not Modified` to the request's validators.
    NotModified {
        /// Upstream's caching policy for the cached copy from now on.
        policy: UpstreamPolicy,
    },
    /// The download failed, and why.
    Failed(String),
}

/// What the cover service needs from the rest of the daemon: a way to
/// download images and a way to process one.
pub(crate) trait CoverBackend: Send + Sync + 'static {
    /// Downloads each request's image, returning one outcome per request in
    /// order. A request with validators may come back
    /// [`Download::NotModified`]; one without never does.
    ///
    /// # Errors
    ///
    /// [`CoverFailure::Unavailable`] when nothing can fetch covers at all.
    fn fetch(
        &self,
        requests: Vec<FetchRequest>,
    ) -> impl Future<Output = Result<Vec<Download>, CoverFailure>> + Send;

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
    async fn fetch(&self, requests: Vec<FetchRequest>) -> Result<Vec<Download>, CoverFailure> {
        let fetcher = self
            .integrations
            .read()
            .await
            .cover_fetchers()
            .into_iter()
            .next()
            .ok_or_else(|| CoverFailure::Unavailable("no deployed plugin fetches covers".into()))?;
        let max_bytes = u32::try_from(bookclerk_cover::MAX_INPUT_BYTES).unwrap_or(u32::MAX);
        let conditional: Vec<bool> = requests
            .iter()
            .map(|request| !request.validators.is_empty())
            .collect();
        let conditions = if conditional.contains(&true) {
            requests
                .iter()
                .map(|request| bookclerk_plugin_abi::CoverCondition {
                    if_none_match: request.validators.if_none_match().map(str::to_owned),
                    if_modified_since: request.validators.if_modified_since(),
                })
                .collect()
        } else {
            Vec::new()
        };
        let params = bookclerk_plugin_abi::CoverFetchParams {
            urls: requests.into_iter().map(|request| request.url).collect(),
            max_bytes,
            conditions,
        };
        let fetched = fetcher
            .fetch_covers(params)
            .await
            .map_err(|err| CoverFailure::Unavailable(format!("cover fetch failed: {err}")))?;
        let received = SystemTime::now();
        let mut fetched = fetched.into_iter();
        Ok(conditional
            .into_iter()
            .map(|conditional| match fetched.next() {
                Some(cover) => download(cover, conditional, received),
                None => Download::Failed("no answer for this URL".into()),
            })
            .collect())
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

/// One plugin answer as a [`Download`].
///
/// # Arguments
///
/// * `cover` - The plugin's answer.
/// * `conditional` - Whether the request carried validators; a
///   `304 Not Modified` to a plain request is refused.
/// * `received` - When the answer arrived, for upstream's freshness.
fn download(
    cover: bookclerk_integrations::FetchedCover,
    conditional: bool,
    received: SystemTime,
) -> Download {
    let headers = &cover.headers;
    let policy = upstream_policy(
        &UpstreamHeaders {
            cache_control: headers.cache_control.as_deref().map(str::as_bytes),
            expires: headers.expires.as_deref().map(str::as_bytes),
            date: headers.date.as_deref().map(str::as_bytes),
            age: headers.age.as_deref().map(str::as_bytes),
            etag: headers.etag.as_deref().map(str::as_bytes),
            last_modified: headers.last_modified.as_deref().map(str::as_bytes),
        },
        received,
    );
    match (cover.body, cover.not_modified, cover.error) {
        (Some(bytes), _, _) => Download::Body { bytes, policy },
        (None, true, _) if conditional => Download::NotModified { policy },
        (None, true, _) => Download::Failed("not modified, but nothing was asked".into()),
        (None, false, Some(error)) => Download::Failed(error),
        (None, false, None) => Download::Failed("no body".into()),
    }
}

/// Whether a cached image is checked with its source once it is stale.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Refresh {
    /// A cover: the address always names the same image, so the cached copy
    /// is served for as long as it is kept.
    Never,
    /// An image that can change at its address (a profile picture, a store
    /// icon): once stale, the cached copy is served while a background
    /// request revalidates it.
    WhenStale,
}

impl Refresh {
    /// The browser's `Cache-Control` for an image refreshed this way.
    fn cache_control(self) -> &'static str {
        match self {
            Self::Never => IMMUTABLE,
            Self::WhenStale => CHANGEABLE,
        }
    }
}

/// Signs cover URLs into same-origin paths and serves the processed images.
pub(crate) struct CoverService {
    /// `cache/covers`, or `None` for a service that only signs (tests).
    dir: Option<PathBuf>,
    /// Key for the token HMAC.
    key: TokenKey,
    /// The cache's journal index: freshness, validators, last served and
    /// size of every entry.
    index: Arc<Mutex<Index>>,
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
    /// One permit per download of an image whose address the caller picks,
    /// at most [`OPEN_FETCHES`].
    open_fetches: Arc<tokio::sync::Semaphore>,
    /// One permit per background revalidation, at most [`REVALIDATIONS`].
    revalidations: Arc<tokio::sync::Semaphore>,
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
        let index = Index::open(&dir);
        Ok(Self::with(Some(dir), key, budget, index))
    }

    /// A service that signs paths but has nowhere to cache, so every image
    /// request is [`CoverFailure::Unavailable`]. Used when the cache directory
    /// cannot be created, and by tests that do not serve covers.
    pub(crate) fn detached() -> Self {
        Self::with(None, random_key(), CACHE_BUDGET_BYTES, Index::detached())
    }

    /// The service from its parts.
    fn with(dir: Option<PathBuf>, key: TokenKey, budget: u64, index: Index) -> Self {
        Self {
            dir,
            key,
            index: Arc::new(Mutex::new(index)),
            sweeping: Arc::new(AtomicBool::new(false)),
            budget,
            flights: Mutex::default(),
            failures: Mutex::default(),
            prefetching: Mutex::default(),
            batches: Arc::new(tokio::sync::Semaphore::new(PREFETCH_BATCHES)),
            open_fetches: Arc::new(tokio::sync::Semaphore::new(OPEN_FETCHES)),
            revalidations: Arc::new(tokio::sync::Semaphore::new(REVALIDATIONS)),
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
    /// [`get_image`], which the browser asks for again daily, unlike a cover
    /// path.
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
        // A tab or newline inside the path would be dropped by the browser's
        // URL parser, turning `/\t/host` into `//host`.
        if src.chars().any(|c| c.is_ascii_control()) {
            return None;
        }
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
    pub(crate) async fn image<B: CoverBackend>(
        self: &Arc<Self>,
        backend: &Arc<B>,
        token: &str,
        size: CoverSize,
    ) -> Result<Vec<u8>, CoverFailure> {
        let url = self.open_token(token).ok_or(CoverFailure::UnknownToken)?;
        self.image_for_url(backend, url, size, Refresh::Never).await
    }

    /// [`Self::image_detached`] for an image whose address the caller picks:
    /// a cached image is served as usual, but a download needs one of
    /// [`OPEN_FETCHES`] permits, held until it finishes.
    ///
    /// # Errors
    ///
    /// [`CoverFailure::Unavailable`] when every permit is taken, otherwise as
    /// [`Self::image_for_url`].
    pub(crate) async fn image_admitted<B: CoverBackend>(
        self: &Arc<Self>,
        backend: Arc<B>,
        url: String,
        size: CoverSize,
        refresh: Refresh,
    ) -> Result<Vec<u8>, CoverFailure> {
        if let Some(dir) = &self.dir {
            let key = cache_key(&url);
            if let Some(bytes) = self.cached(&backend, dir, &key, &url, size, refresh).await {
                return Ok(bytes);
            }
        }
        let permit = Arc::clone(&self.open_fetches)
            .try_acquire_owned()
            .map_err(|_| CoverFailure::Unavailable("too many image downloads at once".into()))?;
        let service = Arc::clone(self);
        tokio::spawn(async move {
            let outcome = service.image_for_url(&backend, url, size, refresh).await;
            drop(permit);
            outcome
        })
        .await
        .unwrap_or_else(|err| {
            Err(CoverFailure::Unavailable(format!(
                "cover task failed: {err}"
            )))
        })
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
        refresh: Refresh,
    ) -> Result<Vec<u8>, CoverFailure> {
        let service = Arc::clone(self);
        tokio::spawn(async move { service.image_for_url(&backend, url, size, refresh).await })
            .await
            .unwrap_or_else(|err| {
                Err(CoverFailure::Unavailable(format!(
                    "cover task failed: {err}"
                )))
            })
    }

    /// The JPEG for `size` of the image at `url`, fetching and processing it
    /// on first use. With [`Refresh::WhenStale`], a stale cached copy is
    /// served while it is revalidated in the background.
    ///
    /// # Errors
    ///
    /// The cached or fresh failure.
    pub(crate) async fn image_for_url<B: CoverBackend>(
        self: &Arc<Self>,
        backend: &Arc<B>,
        url: String,
        size: CoverSize,
        refresh: Refresh,
    ) -> Result<Vec<u8>, CoverFailure> {
        if !is_coverable(&url) {
            return Err(CoverFailure::Refused("not an https URL".into()));
        }
        let dir = self
            .dir
            .clone()
            .ok_or_else(|| CoverFailure::Unavailable("no cover cache".into()))?;
        let key = cache_key(&url);
        if let Some(bytes) = self.cached(backend, &dir, &key, &url, size, refresh).await {
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
                let fetched = backend
                    .fetch(vec![FetchRequest::plain(url)])
                    .await
                    .and_then(|mut downloads| match downloads.pop() {
                        Some(Download::Body { bytes, policy }) => Ok((bytes, policy)),
                        Some(Download::Failed(error)) => Err(CoverFailure::Refused(error)),
                        Some(Download::NotModified { .. }) => Err(CoverFailure::Refused(
                            "not modified, but nothing was asked".into(),
                        )),
                        None => Err(CoverFailure::Refused("no answer".into())),
                    });
                match fetched {
                    Ok((bytes, policy)) => self.store(&**backend, &dir, &key, bytes, policy).await,
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

    /// One size of the cached entry for `key`, noting that it was served
    /// and, for [`Refresh::WhenStale`], revalidating it in the background
    /// when it is stale.
    ///
    /// # Returns
    ///
    /// `None` when the entry is not on disk.
    async fn cached<B: CoverBackend>(
        self: &Arc<Self>,
        backend: &Arc<B>,
        dir: &Path,
        key: &str,
        url: &str,
        size: CoverSize,
        refresh: Refresh,
    ) -> Option<Vec<u8>> {
        let bytes = read_cached(dir, key, size).await?;
        let now = Stamp::now();
        // An entry the index lost track of (a sweep racing a write) is
        // served as fresh; it is indexed again when next written.
        let stale = lock(&self.index)
            .get(key)
            .is_some_and(|entry| entry.is_stale(now));
        let index = Arc::clone(&self.index);
        let served = key.to_owned();
        tokio::task::spawn_blocking(move || lock(&index).served(&served, now));
        if stale && refresh == Refresh::WhenStale {
            self.revalidate(Arc::clone(backend), url.to_owned(), key.to_owned());
        }
        Some(bytes)
    }

    /// Revalidates the stale entry for `key` in the background, unless it
    /// failed recently, a request or another revalidation holds its flight,
    /// or [`REVALIDATIONS`] are already running.
    fn revalidate<B: CoverBackend>(self: &Arc<Self>, backend: Arc<B>, url: String, key: String) {
        if self.recent_failure(&key).is_err() {
            return;
        }
        let Ok(permit) = Arc::clone(&self.revalidations).try_acquire_owned() else {
            return;
        };
        let flight = self.flight(&key);
        let Ok(guard) = Arc::clone(&flight).try_lock_owned() else {
            return;
        };
        let service = Arc::clone(self);
        tokio::spawn(async move {
            service.refresh_entry(&*backend, url, &key).await;
            drop(guard);
            drop(permit);
            service.land(&key, &flight);
        });
    }

    /// Asks upstream about the cached entry for `key` with its validators:
    /// a `304` extends its lifetime, a new image replaces it, and a failure
    /// keeps the old copy and is remembered for [`FAILURE_TTL`].
    async fn refresh_entry(&self, backend: &impl CoverBackend, url: String, key: &str) {
        let Some(dir) = self.dir.clone() else {
            return;
        };
        let validators = lock(&self.index)
            .get(key)
            .map(Entry::validators)
            .unwrap_or_default();
        let downloaded = backend
            .fetch(vec![FetchRequest { url, validators }])
            .await
            .map(|mut downloads| downloads.pop());
        let outcome = match downloaded {
            Ok(Some(Download::NotModified { policy })) => {
                self.revalidated(&dir, key, policy).await;
                Ok(())
            }
            Ok(Some(Download::Body { bytes, policy })) => self
                .store(backend, &dir, key, bytes, policy)
                .await
                .map(drop),
            Ok(Some(Download::Failed(error))) => Err(CoverFailure::Refused(error)),
            Ok(None) => Err(CoverFailure::Refused("no answer".into())),
            Err(failure) => Err(failure),
        };
        if let Err(failure) = outcome {
            tracing::debug!(
                ?failure,
                "image revalidation failed; keeping the cached copy"
            );
            self.note_failure(key, &failure);
        }
    }

    /// Applies a `304 Not Modified` to the entry for `key`: a new lifetime
    /// and any validators upstream sent, or, when upstream now says
    /// `no-store`, deletes the entry.
    async fn revalidated(&self, dir: &Path, key: &str, policy: UpstreamPolicy) {
        let fresh_for = match policy.storage {
            Storage::DoNotStore => {
                self.forget(dir, key).await;
                return;
            }
            Storage::Store { fresh_for } => fresh_for,
        };
        let index = Arc::clone(&self.index);
        let key = key.to_owned();
        let _ = tokio::task::spawn_blocking(move || {
            let mut index = lock(&index);
            let Some(old) = index.get(&key).cloned() else {
                return;
            };
            let now = Stamp::now();
            let validators = &policy.validators;
            let entry = Entry {
                fetched: now,
                fresh_until: now.after(fresh_for),
                // A 304 need not repeat the validators.
                etag: validators.if_none_match().map(str::to_owned).or(old.etag),
                last_modified: validators
                    .last_modified()
                    .map(Stamp::from_time)
                    .or(old.last_modified),
                ..old
            };
            index.put(&key, entry);
        })
        .await;
    }

    /// Deletes the entry for `key` from the index and the disk.
    async fn forget(&self, dir: &Path, key: &str) {
        let index = Arc::clone(&self.index);
        let path = dir.join(key);
        let key = key.to_owned();
        let _ = tokio::task::spawn_blocking(move || {
            lock(&index).remove(&key);
            let _ = std::fs::remove_dir_all(path);
        })
        .await;
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
            let requests = todo
                .iter()
                .map(|(url, _)| FetchRequest::plain(url.clone()))
                .collect();
            let outcomes: Vec<Result<(Vec<u8>, UpstreamPolicy), CoverFailure>> =
                match backend.fetch(requests).await {
                    Ok(downloads) => downloads
                        .into_iter()
                        .map(|download| match download {
                            Download::Body { bytes, policy } => Ok((bytes, policy)),
                            Download::Failed(error) => Err(CoverFailure::Refused(error)),
                            Download::NotModified { .. } => Err(CoverFailure::Refused(
                                "not modified, but nothing was asked".into(),
                            )),
                        })
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
                            Ok((bytes, policy)) => {
                                service.store(&*backend, &dir, &key, bytes, policy).await
                            }
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

    /// Processes downloaded bytes and, unless upstream said `no-store`,
    /// writes them as the entry for `key`, replacing any older copy, and
    /// indexes it. With `no-store`, any older copy is deleted instead.
    ///
    /// # Returns
    ///
    /// The processed images, also when they could not be written.
    ///
    /// # Errors
    ///
    /// The processing failure.
    async fn store(
        &self,
        backend: &impl CoverBackend,
        dir: &Path,
        key: &str,
        bytes: Vec<u8>,
        policy: UpstreamPolicy,
    ) -> Result<CoverSet, CoverFailure> {
        let set = backend.process(bytes).await?;
        let fresh_for = match policy.storage {
            Storage::DoNotStore => {
                self.forget(dir, key).await;
                return Ok(set);
            }
            Storage::Store { fresh_for } => fresh_for,
        };
        let written = {
            let dir = dir.to_path_buf();
            let key = key.to_owned();
            let set = set.clone();
            let index = Arc::clone(&self.index);
            tokio::task::spawn_blocking(move || {
                let bytes = write_entry(&dir, &key, &set)?;
                let now = Stamp::now();
                let entry = Entry {
                    fetched: now,
                    fresh_until: now.after(fresh_for),
                    etag: policy.validators.if_none_match().map(str::to_owned),
                    last_modified: policy.validators.last_modified().map(Stamp::from_time),
                    served: now,
                    bytes,
                };
                lock(&index).put(&key, entry);
                Ok::<_, std::io::Error>(())
            })
            .await
            .map_err(|err| err.to_string())
            .and_then(|result| result.map_err(|err| err.to_string()))
        };
        match written {
            Ok(()) => self.maybe_sweep(dir),
            // Still serve this request; the next one tries the write again.
            Err(err) => tracing::warn!(error = %err, "could not cache a cover"),
        }
        Ok(set)
    }

    /// Starts an eviction sweep when the cache is over budget.
    fn maybe_sweep(&self, dir: &Path) {
        if lock(&self.index).total() <= self.budget {
            return;
        }
        if self.sweeping.swap(true, Ordering::AcqRel) {
            return;
        }
        let dir = dir.to_path_buf();
        let budget = self.budget;
        let sweeping = Arc::clone(&self.sweeping);
        let index = Arc::clone(&self.index);
        tokio::task::spawn_blocking(move || {
            sweep(&dir, &index, budget);
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

/// Reads one size of a cached entry.
async fn read_cached(dir: &Path, key: &str, size: CoverSize) -> Option<Vec<u8>> {
    tokio::fs::read(dir.join(key).join(file_name(size)))
        .await
        .ok()
}

/// Writes the three sizes of `set` as entry `key`, replacing any older
/// entry: the new one is assembled in the staging directory, the old one
/// moved aside, the new one renamed into place and the old one deleted.
///
/// # Returns
///
/// Bytes written.
///
/// # Errors
///
/// The I/O error when the entry cannot be written.
fn write_entry(dir: &Path, key: &str, set: &CoverSet) -> std::io::Result<u64> {
    let target = dir.join(key);
    let staging = dir.join(STAGING_DIR);
    std::fs::create_dir_all(&staging)?;
    let mut nonce = [0u8; 8];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut nonce);
    let temp = staging.join(format!("{key}-{}", hex::encode(nonce)));
    let old = staging.join(format!("{key}-{}-old", hex::encode(nonce)));
    std::fs::create_dir(&temp)?;
    let mut total = 0u64;
    let result = (|| {
        for size in CoverSize::ALL {
            let bytes = set.get(size);
            std::fs::write(temp.join(file_name(size)), bytes)?;
            total = total.saturating_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
        }
        match std::fs::rename(&target, &old) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err),
        }
        std::fs::rename(&temp, &target)
    })();
    let _ = std::fs::remove_dir_all(&old);
    if let Err(err) = result {
        let _ = std::fs::remove_dir_all(&temp);
        return Err(err);
    }
    Ok(total)
}

/// Deletes the least recently served entries until the cache is at most 90%
/// of `budget`.
///
/// An entry that cannot be deleted (a file held open on Windows) is
/// indexed again, so it still counts toward the budget and goes on a later
/// sweep. An entry written again between being picked and being deleted is
/// lost from disk while still indexed; its next request downloads it again.
fn sweep(dir: &Path, index: &Mutex<Index>, budget: u64) {
    let evicted = lock(index).evict_to(budget / 10 * 9);
    for (key, entry) in evicted {
        let path = dir.join(&key);
        if let Err(err) = std::fs::remove_dir_all(&path) {
            if path.exists() {
                tracing::debug!(error = %err, "could not evict a cached image; kept for a later sweep");
                lock(index).restore(&key, entry);
            }
        }
    }
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
/// immutable, since a path always names the same image, and carry an `ETag`
/// so a client revalidating anyway gets `304 Not Modified`.
#[utoipa::path(
    get,
    path = "/api/covers/{token}/{size}",
    params(
        ("token" = String, Path, description = "Signed cover token from a `cover_path` field."),
        ("size" = String, Path, description = "`128` (thumbnail), `360` (card) or `full`."),
    ),
    responses(
        (status = 200, description = "The cover as a JPEG.", content_type = "image/jpeg"),
        (status = 304, description = "`If-None-Match` names the current `ETag`; no body."),
        (status = 404, description = "The token was not issued by this node, or the size is unknown."),
        (status = 502, description = "The image could not be downloaded or is not a supported image."),
        (status = 503, description = "This node cannot fetch or process covers right now."),
    ),
)]
pub(crate) async fn get_cover(
    axum::extract::State(state): axum::extract::State<Arc<crate::api::AppState>>,
    axum::extract::Path((token, size)): axum::extract::Path<(String, String)>,
    headers: axum::http::HeaderMap,
) -> axum::response::Response {
    use axum::http::StatusCode;
    use axum::response::IntoResponse;

    let Some(size) = CoverSize::parse(&size) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let url = state.covers.open_token(&token);
    serve(
        &state,
        &headers,
        url,
        size,
        Refresh::Never,
        Admission::Signed,
    )
    .await
}

/// `Cache-Control` for a cover path, which always names the same image.
const IMMUTABLE: &str = "private, max-age=31536000, immutable";

/// `Cache-Control` for an image whose remote copy can change under the same
/// address (a Gravatar, a store icon): the browser uses its copy for an
/// hour, then shows it while asking Bookclerk again in the background for up
/// to a day. The `ETag` makes that check a `304` when nothing changed.
/// Bookclerk answers from its disk cache and revalidates its own copy with
/// the image host once upstream's freshness lifetime runs out.
const CHANGEABLE: &str = "private, max-age=3600, stale-while-revalidate=86400";

/// How [`serve`] admits a download.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Admission {
    /// The address was signed or chosen by this node, so the set of
    /// addresses is bounded: download without a limit.
    Signed,
    /// The caller picks the address (a Gravatar hash): download only within
    /// [`OPEN_FETCHES`].
    Open,
}

/// Serves one size of the image at `url` through the cover service.
///
/// # Arguments
///
/// * `state` - Daemon state holding the cover service and the plugins that
///   fetch for it.
/// * `request` - The request's headers, for `If-None-Match`.
/// * `url` - The remote image; `None` is a 404.
/// * `size` - The size to serve.
/// * `refresh` - Whether a stale cached copy is revalidated, which also
///   picks the browser's `Cache-Control`.
/// * `admission` - Whether a download is limited by [`OPEN_FETCHES`].
async fn serve(
    state: &crate::api::AppState,
    request: &axum::http::HeaderMap,
    url: Option<String>,
    size: CoverSize,
    refresh: Refresh,
    admission: Admission,
) -> axum::response::Response {
    use axum::http::StatusCode;
    use axum::response::IntoResponse;

    let backend = Arc::new(LiveBackend {
        integrations: Arc::clone(&state.integrations),
    });
    let outcome = match url {
        Some(url) => match admission {
            Admission::Signed => {
                state
                    .covers
                    .image_detached(backend, url, size, refresh)
                    .await
            }
            Admission::Open => {
                state
                    .covers
                    .image_admitted(backend, url, size, refresh)
                    .await
            }
        },
        None => Err(CoverFailure::UnknownToken),
    };
    match outcome {
        Ok(bytes) => crate::cache_policy::image_response(
            request,
            bytes,
            "image/jpeg",
            refresh.cache_control(),
        ),
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
/// processed like a cover, but answers with an hour-long browser cache
/// header instead of `immutable`, because the remote image can change at the
/// same address. Once upstream's freshness lifetime (one hour to seven
/// days) runs out, Bookclerk serves its cached copy while it checks with the
/// image host in the background.
#[utoipa::path(
    get,
    path = "/api/images/{token}",
    params(
        ("token" = String, Path, description = "Signed token from a `picture_path` or `logo` field."),
    ),
    responses(
        (status = 200, description = "The image as a JPEG.", content_type = "image/jpeg"),
        (status = 304, description = "`If-None-Match` names the current `ETag`; no body."),
        (status = 404, description = "The token was not issued by this node."),
        (status = 502, description = "The image could not be downloaded or is not a supported image."),
        (status = 503, description = "This node cannot fetch or process images right now."),
    ),
)]
pub(crate) async fn get_image(
    axum::extract::State(state): axum::extract::State<Arc<crate::api::AppState>>,
    axum::extract::Path(token): axum::extract::Path<String>,
    headers: axum::http::HeaderMap,
) -> axum::response::Response {
    let url = state.covers.open_token(&token);
    serve(
        &state,
        &headers,
        url,
        CoverSize::Thumb,
        Refresh::WhenStale,
        Admission::Signed,
    )
    .await
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
        (status = 304, description = "`If-None-Match` names the current `ETag`; no body."),
        (status = 404, description = "The hash is not 64 lowercase hex digits."),
        (status = 502, description = "The email has no Gravatar, or the image is not a supported image."),
        (status = 503, description = "This node cannot fetch or process images right now, or too many Gravatars are downloading at once."),
    ),
)]
pub(crate) async fn get_gravatar(
    axum::extract::State(state): axum::extract::State<Arc<crate::api::AppState>>,
    axum::extract::Path(hash): axum::extract::Path<String>,
    headers: axum::http::HeaderMap,
) -> axum::response::Response {
    serve(
        &state,
        &headers,
        gravatar_url(&hash),
        CoverSize::Thumb,
        Refresh::WhenStale,
        Admission::Open,
    )
    .await
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
        (status = 304, description = "`If-None-Match` names the current `ETag`; no body."),
        (status = 404, description = "No deployed storefront or integration has this id, or it names no icon."),
        (status = 502, description = "The icon could not be downloaded or is not a supported image."),
        (status = 503, description = "This node cannot fetch or process images right now."),
    ),
)]
pub(crate) async fn get_store_icon(
    axum::extract::State(state): axum::extract::State<Arc<crate::api::AppState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
    headers: axum::http::HeaderMap,
) -> axum::response::Response {
    let url = store_icon_url(&state, &id).await;
    serve(
        &state,
        &headers,
        url,
        CoverSize::Thumb,
        Refresh::WhenStale,
        Admission::Signed,
    )
    .await
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
                Refresh::Never,
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

    /// A 2×1 opaque PNG, a different image from [`PNG`].
    fn wide_png() -> Vec<u8> {
        let mut out = Vec::new();
        let image = image::RgbImage::from_pixel(2, 1, image::Rgb([200, 10, 10]));
        image::DynamicImage::ImageRgb8(image)
            .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
            .expect("png");
        out
    }

    /// What [`Fake`] answers to a request with validators.
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
    enum Revalidation {
        /// `304 Not Modified`.
        #[default]
        NotModified,
        /// A different image.
        Changed,
        /// An error.
        Fails,
    }

    /// Serves [`PNG`] for every URL containing `ok`, an error otherwise, and
    /// processes in this process. A URL containing `nostore` comes with
    /// `no-store`; any other with an `ETag`.
    #[derive(Default)]
    struct Fake {
        /// URLs asked for, across calls.
        fetched: AtomicUsize,
        /// Calls to `fetch`.
        calls: AtomicUsize,
        /// When set, `fetch` reports that nothing can fetch.
        unavailable: bool,
        /// The answer to a request with validators.
        revalidation: Revalidation,
        /// Validators seen, in order.
        conditions: Mutex<Vec<Validators>>,
    }

    /// The policy upstream's headers give: kept for an hour with `etag`, or
    /// not kept.
    fn policy(store: bool, etag: &str) -> UpstreamPolicy {
        upstream_policy(
            &UpstreamHeaders {
                cache_control: Some(if store {
                    &b"max-age=3600"[..]
                } else {
                    b"no-store"
                }),
                etag: Some(etag.as_bytes()),
                ..UpstreamHeaders::default()
            },
            SystemTime::now(),
        )
    }

    impl CoverBackend for Fake {
        async fn fetch(&self, requests: Vec<FetchRequest>) -> Result<Vec<Download>, CoverFailure> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.unavailable {
                return Err(CoverFailure::Unavailable("no fetcher".into()));
            }
            self.fetched.fetch_add(requests.len(), Ordering::SeqCst);
            // Let concurrent callers pile up behind the flight lock.
            tokio::time::sleep(Duration::from_millis(20)).await;
            Ok(requests
                .into_iter()
                .map(|FetchRequest { url, validators }| {
                    let store = !url.contains("nostore");
                    if !validators.is_empty() {
                        lock(&self.conditions).push(validators);
                        return match self.revalidation {
                            Revalidation::NotModified => Download::NotModified {
                                policy: policy(store, "\"v1\""),
                            },
                            Revalidation::Changed => Download::Body {
                                bytes: wide_png(),
                                policy: policy(store, "\"v2\""),
                            },
                            Revalidation::Fails => Download::Failed("HTTP 500".into()),
                        };
                    }
                    if url.contains("ok") {
                        Download::Body {
                            bytes: PNG.to_vec(),
                            policy: policy(store, "\"v1\""),
                        }
                    } else if url.contains("html") {
                        Download::Body {
                            bytes: b"<html>".to_vec(),
                            policy: policy(store, "\"v1\""),
                        }
                    } else {
                        Download::Failed("HTTP 404".into())
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
            "/\t/evil.example/x.png",
            "/\n/evil.example/x.png",
            "/api/images/a\rb",
            "",
        ] {
            assert_eq!(service.same_origin_src(refused), None, "{refused:?}");
        }
    }

    #[tokio::test]
    async fn open_downloads_are_limited_but_cached_images_are_not() {
        let temp = tempfile::tempdir().expect("tempdir");
        let service = service(temp.path());
        let backend = Arc::new(Fake::default());
        let cached = "https://img.example/ok-cached.png".to_owned();
        service
            .image_admitted(
                Arc::clone(&backend),
                cached.clone(),
                CoverSize::Thumb,
                Refresh::WhenStale,
            )
            .await
            .expect("first download");

        // Every permit taken: a new address is refused without a download,
        // and the refusal isn't remembered as the address's own failure.
        let held: Vec<_> = (0..OPEN_FETCHES)
            .map(|_| {
                Arc::clone(&service.open_fetches)
                    .try_acquire_owned()
                    .expect("permit")
            })
            .collect();
        let fresh = "https://img.example/ok-fresh.png".to_owned();
        let refused = service
            .image_admitted(
                Arc::clone(&backend),
                fresh.clone(),
                CoverSize::Thumb,
                Refresh::WhenStale,
            )
            .await;
        assert!(matches!(refused, Err(CoverFailure::Unavailable(_))));
        assert_eq!(backend.fetched.load(Ordering::SeqCst), 1);
        let bytes = service
            .image_admitted(
                Arc::clone(&backend),
                cached,
                CoverSize::Thumb,
                Refresh::WhenStale,
            )
            .await
            .expect("served from disk while full");
        assert!(bytes.starts_with(&[0xFF, 0xD8, 0xFF]));

        drop(held);
        service
            .image_admitted(
                Arc::clone(&backend),
                fresh,
                CoverSize::Thumb,
                Refresh::WhenStale,
            )
            .await
            .expect("downloaded once a permit is free");
        assert_eq!(backend.fetched.load(Ordering::SeqCst), 2);
        assert_eq!(service.open_fetches.available_permits(), OPEN_FETCHES);
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
            service.image_detached(
                Arc::clone(&backend),
                url.clone(),
                CoverSize::Card,
                Refresh::Never,
            ),
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
            .image_detached(Arc::clone(&backend), url, CoverSize::Full, Refresh::Never)
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
                service.image(&backend, &token, size).await
            }));
        }
        for task in tasks {
            let bytes = task.await.expect("join").expect("image");
            assert!(bytes.starts_with(&[0xFF, 0xD8, 0xFF]));
        }
        assert_eq!(backend.fetched.load(Ordering::SeqCst), 1);
        service
            .image(&backend, &token, CoverSize::Full)
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
        let backend = Arc::new(Fake::default());
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

        let detached = Arc::new(CoverService::detached());
        let token = token_for(&detached, "https://img.example/ok.png");
        assert!(matches!(
            detached.image(&backend, &token, CoverSize::Thumb).await,
            Err(CoverFailure::Unavailable(_))
        ));
        let unavailable = Arc::new(Fake {
            unavailable: true,
            ..Fake::default()
        });
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
            .image(&backend, &token, CoverSize::Card)
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

    #[tokio::test]
    async fn the_sweep_evicts_least_recently_served_entries() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = temp.path().join("covers");
        let one = {
            let set = bookclerk_cover::process(PNG).expect("set");
            CoverSize::ALL
                .into_iter()
                .map(|size| u64::try_from(set.get(size).len()).expect("size"))
                .sum::<u64>()
        };
        // Room for two entries and a half: writing a fourth evicts down to
        // 90%, so the two served longest ago go.
        let service =
            Arc::new(CoverService::open(dir.clone(), one * 2 + one / 2 + one / 10).expect("open"));
        let backend = Arc::new(Fake::default());
        let urls: Vec<String> = (0..4)
            .map(|n| format!("https://img.example/ok-{n}.png"))
            .collect();
        for (n, url) in urls.iter().enumerate().take(3) {
            service
                .image(&backend, &token_for(&service, url), CoverSize::Thumb)
                .await
                .expect("cover");
            let mut index = lock(&service.index);
            let mut entry = index.get(&cache_key(url)).expect("indexed").clone();
            entry.served = Stamp::from_time(
                SystemTime::now() - Duration::from_secs(3600 * (10 - u64::try_from(n).expect("n"))),
            );
            index.put(&cache_key(url), entry);
        }
        service
            .image(&backend, &token_for(&service, &urls[3]), CoverSize::Thumb)
            .await
            .expect("cover");
        for _ in 0..200 {
            if !service.sweeping.load(Ordering::Acquire) && !dir.join(cache_key(&urls[1])).exists()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(!dir.join(cache_key(&urls[0])).exists());
        assert!(!dir.join(cache_key(&urls[1])).exists());
        assert!(dir.join(cache_key(&urls[2])).exists());
        assert!(dir.join(cache_key(&urls[3])).exists());
        assert_eq!(lock(&service.index).total(), one * 2);
        assert!(!dir
            .join(STAGING_DIR)
            .read_dir()
            .expect("staging")
            .any(|_| true));
        // The journal remembers it after a restart.
        drop(service);
        let reopened = CoverService::open(dir, CACHE_BUDGET_BYTES).expect("reopen");
        let index = lock(&reopened.index);
        assert_eq!(index.total(), one * 2);
        assert_eq!(
            index
                .get(&cache_key(&urls[3]))
                .expect("kept")
                .etag
                .as_deref(),
            Some("\"v1\"")
        );
    }

    /// Waits until no flight is held, so background work has finished.
    async fn settle(service: &CoverService) {
        for _ in 0..200 {
            if lock(&service.flights).is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(lock(&service.flights).is_empty());
    }

    #[tokio::test]
    async fn a_stale_image_is_served_and_then_revalidated() {
        let temp = tempfile::tempdir().expect("tempdir");
        let service = service(temp.path());
        let url = "https://img.example/ok-avatar.png".to_owned();
        let key = cache_key(&url);
        let first = Arc::new(Fake::default());
        let original = service
            .image_detached(
                Arc::clone(&first),
                url.clone(),
                CoverSize::Thumb,
                Refresh::WhenStale,
            )
            .await
            .expect("downloaded");
        let fresh_until = lock(&service.index).get(&key).expect("indexed").fresh_until;
        assert!(fresh_until > Stamp::now(), "fresh for upstream's hour");

        // Fresh: served from disk, nothing asked.
        service
            .image_detached(
                Arc::clone(&first),
                url.clone(),
                CoverSize::Thumb,
                Refresh::WhenStale,
            )
            .await
            .expect("cached");
        assert_eq!(first.calls.load(Ordering::SeqCst), 1);

        // Stale and unchanged: the copy is served, then a 304 renews it.
        lock(&service.index).expire(&key);
        let unchanged = Arc::new(Fake::default());
        let served = service
            .image_detached(
                Arc::clone(&unchanged),
                url.clone(),
                CoverSize::Thumb,
                Refresh::WhenStale,
            )
            .await
            .expect("stale copy");
        assert_eq!(served, original);
        settle(&service).await;
        assert_eq!(unchanged.calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            lock(&unchanged.conditions)[0].if_none_match(),
            Some("\"v1\"")
        );
        let entry = lock(&service.index).get(&key).expect("kept").clone();
        assert!(!entry.is_stale(Stamp::now()));
        assert_eq!(entry.etag.as_deref(), Some("\"v1\""));

        // A cover path never revalidates, however stale.
        lock(&service.index).expire(&key);
        let cover = Arc::new(Fake::default());
        service
            .image_detached(
                Arc::clone(&cover),
                url.clone(),
                CoverSize::Thumb,
                Refresh::Never,
            )
            .await
            .expect("cover");
        settle(&service).await;
        assert_eq!(cover.calls.load(Ordering::SeqCst), 0);

        // Stale and changed: the new image replaces the old.
        let changed = Arc::new(Fake {
            revalidation: Revalidation::Changed,
            ..Fake::default()
        });
        service
            .image_detached(
                Arc::clone(&changed),
                url.clone(),
                CoverSize::Thumb,
                Refresh::WhenStale,
            )
            .await
            .expect("stale copy");
        settle(&service).await;
        let replaced = service
            .image_detached(
                Arc::clone(&changed),
                url.clone(),
                CoverSize::Thumb,
                Refresh::WhenStale,
            )
            .await
            .expect("new copy");
        assert_ne!(replaced, original);
        assert_eq!(changed.calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            lock(&service.index)
                .get(&key)
                .expect("kept")
                .etag
                .as_deref(),
            Some("\"v2\"")
        );

        // Stale and failing: the copy stays, and the failure isn't retried
        // on every view.
        lock(&service.index).expire(&key);
        let failing = Arc::new(Fake {
            revalidation: Revalidation::Fails,
            ..Fake::default()
        });
        for _ in 0..2 {
            let kept = service
                .image_detached(
                    Arc::clone(&failing),
                    url.clone(),
                    CoverSize::Thumb,
                    Refresh::WhenStale,
                )
                .await
                .expect("old copy kept");
            assert_eq!(kept, replaced);
            settle(&service).await;
        }
        assert_eq!(failing.calls.load(Ordering::SeqCst), 1);
        assert!(lock(&service.index).get(&key).is_some());
    }

    #[tokio::test]
    async fn no_store_images_are_served_but_not_kept() {
        let temp = tempfile::tempdir().expect("tempdir");
        let service = service(temp.path());
        let backend = Arc::new(Fake::default());
        let url = "https://img.example/ok-nostore.png".to_owned();
        for _ in 0..2 {
            let bytes = service
                .image_detached(
                    Arc::clone(&backend),
                    url.clone(),
                    CoverSize::Thumb,
                    Refresh::WhenStale,
                )
                .await
                .expect("served");
            assert!(bytes.starts_with(&[0xFF, 0xD8, 0xFF]));
        }
        assert_eq!(backend.calls.load(Ordering::SeqCst), 2);
        assert!(!temp.path().join("covers").join(cache_key(&url)).exists());
        assert!(lock(&service.index).get(&cache_key(&url)).is_none());

        // A kept image whose host now says no-store, on a 304 or with a new
        // image, is deleted.
        for revalidation in [Revalidation::NotModified, Revalidation::Changed] {
            let kept = "https://img.example/ok-kept.png".to_owned();
            let key = cache_key(&kept);
            service
                .image_detached(
                    Arc::new(Fake::default()),
                    kept.clone(),
                    CoverSize::Thumb,
                    Refresh::WhenStale,
                )
                .await
                .expect("kept");
            assert!(lock(&service.index).get(&key).is_some());
            lock(&service.index).expire(&key);
            // The same entry, now answered with no-store.
            let entry = service.dir.clone().expect("dir").join(&key);
            let renamed = cache_key("https://img.example/ok-kept-nostore.png");
            let moved = service.dir.clone().expect("dir").join(&renamed);
            std::fs::rename(&entry, &moved).expect("rename");
            let old = lock(&service.index).remove(&key).expect("entry");
            lock(&service.index).put(&renamed, old);
            lock(&service.index).expire(&renamed);
            let refusing = Arc::new(Fake {
                revalidation,
                ..Fake::default()
            });
            service
                .image_detached(
                    Arc::clone(&refusing),
                    "https://img.example/ok-kept-nostore.png".into(),
                    CoverSize::Thumb,
                    Refresh::WhenStale,
                )
                .await
                .expect("stale copy");
            settle(&service).await;
            assert_eq!(refusing.calls.load(Ordering::SeqCst), 1, "{revalidation:?}");
            assert!(!moved.exists(), "{revalidation:?}");
            assert!(lock(&service.index).get(&renamed).is_none());
        }
    }

    /// A `covers` plugin that records each call's params and answers with
    /// `answers`, in order.
    struct Plugin {
        /// Params of each call.
        calls: Mutex<Vec<bookclerk_plugin_abi::CoverFetchParams>>,
        /// What every call answers.
        answers: Vec<bookclerk_integrations::FetchedCover>,
    }

    #[async_trait::async_trait]
    impl bookclerk_integrations::Integration for Plugin {
        fn id(&self) -> &str {
            "covers"
        }

        async fn start(
            &self,
            _ctx: bookclerk_integrations::IntegrationContext,
        ) -> bookclerk_integrations::Result<()> {
            Ok(())
        }

        async fn health(
            &self,
        ) -> bookclerk_integrations::Result<bookclerk_integrations::IntegrationHealth> {
            Ok(bookclerk_integrations::IntegrationHealth {
                id: "covers".into(),
                enabled: true,
                ok: true,
                detail: None,
            })
        }

        async fn deliver_domain_event(
            &self,
            _event: bookclerk_integrations::DomainEvent,
        ) -> bookclerk_integrations::Result<bookclerk_integrations::EventResult> {
            Ok(bookclerk_integrations::EventResult::Ack)
        }

        fn supports_covers(&self) -> bool {
            true
        }

        async fn fetch_covers(
            &self,
            params: bookclerk_plugin_abi::CoverFetchParams,
        ) -> bookclerk_integrations::Result<Vec<bookclerk_integrations::FetchedCover>> {
            lock(&self.calls).push(params);
            Ok(self.answers.clone())
        }
    }

    #[tokio::test]
    async fn the_live_backend_sends_conditions_only_when_it_has_them() {
        use bookclerk_integrations::{FetchedCover, IntegrationRegistry};
        let empty = LiveBackend {
            integrations: Arc::new(tokio::sync::RwLock::new(IntegrationRegistry::new())),
        };
        assert!(matches!(
            empty
                .fetch(vec![FetchRequest::plain("https://a".into())])
                .await,
            Err(CoverFailure::Unavailable(_))
        ));

        let plugin = Arc::new(Plugin {
            calls: Mutex::default(),
            answers: vec![
                FetchedCover {
                    not_modified: true,
                    ..FetchedCover::default()
                },
                FetchedCover {
                    not_modified: true,
                    ..FetchedCover::default()
                },
            ],
        });
        let mut registry = IntegrationRegistry::new();
        registry.register(Arc::clone(&plugin) as Arc<dyn bookclerk_integrations::Integration>);
        let live = LiveBackend {
            integrations: Arc::new(tokio::sync::RwLock::new(registry)),
        };

        // Plain fetches send no conditions; a 304 to one is refused, and a
        // URL the plugin left unanswered fails.
        let plain = live
            .fetch(vec![
                FetchRequest::plain("https://a".into()),
                FetchRequest::plain("https://b".into()),
                FetchRequest::plain("https://c".into()),
            ])
            .await
            .expect("fetched");
        assert!(lock(&plugin.calls)[0].conditions.is_empty());
        assert!(matches!(plain[0], Download::Failed(_)));
        assert_eq!(plain[2], Download::Failed("no answer for this URL".into()));

        // One revalidation among plain fetches: one condition per URL, the
        // plain one empty.
        let modified = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let revalidated = live
            .fetch(vec![
                FetchRequest {
                    url: "https://a".into(),
                    validators: Validators::new(Some("\"v1\""), Some(modified)),
                },
                FetchRequest::plain("https://b".into()),
            ])
            .await
            .expect("fetched");
        let conditions = lock(&plugin.calls)[1].conditions.clone();
        assert_eq!(conditions.len(), 2);
        assert_eq!(conditions[0].if_none_match.as_deref(), Some("\"v1\""));
        assert_eq!(
            conditions[0].if_modified_since.as_deref(),
            Some("Tue, 14 Nov 2023 22:13:20 GMT")
        );
        assert_eq!(
            conditions[1],
            bookclerk_plugin_abi::CoverCondition::default()
        );
        assert!(matches!(revalidated[0], Download::NotModified { .. }));
        assert!(matches!(revalidated[1], Download::Failed(_)));
        assert!(lock(&plugin.calls).iter().all(|call| call.max_bytes > 0));
    }

    #[test]
    fn plugin_answers_become_downloads() {
        use bookclerk_integrations::{CoverHeaders, FetchedCover};
        let received = SystemTime::now();
        let headers = CoverHeaders {
            etag: Some("\"a\"".into()),
            cache_control: Some("max-age=86400".into()),
            ..CoverHeaders::default()
        };
        let body = FetchedCover {
            url: "https://a".into(),
            body: Some(vec![1]),
            headers: headers.clone(),
            ..FetchedCover::default()
        };
        let Download::Body { bytes, policy } = download(body, false, received) else {
            panic!("a body");
        };
        assert_eq!(bytes, vec![1]);
        assert_eq!(policy.validators.if_none_match(), Some("\"a\""));
        assert_eq!(
            policy.storage,
            Storage::Store {
                fresh_for: Duration::from_secs(86400)
            }
        );
        let unchanged = FetchedCover {
            not_modified: true,
            headers,
            ..FetchedCover::default()
        };
        assert!(matches!(
            download(unchanged.clone(), true, received),
            Download::NotModified { .. }
        ));
        assert!(matches!(
            download(unchanged, false, received),
            Download::Failed(_)
        ));
        let failed = FetchedCover {
            error: Some("HTTP 404".into()),
            ..FetchedCover::default()
        };
        assert_eq!(
            download(failed, false, received),
            Download::Failed("HTTP 404".into())
        );
        assert_eq!(
            download(FetchedCover::default(), false, received),
            Download::Failed("no body".into())
        );
        assert_eq!(Refresh::Never.cache_control(), IMMUTABLE);
        assert_eq!(Refresh::WhenStale.cache_control(), CHANGEABLE);
    }

    #[test]
    fn an_entry_that_cannot_be_deleted_stays_indexed() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = temp.path();
        let mut index = Index::detached();
        let stuck = cache_key("https://img.example/stuck.png");
        let gone = cache_key("https://img.example/gone.png");
        // A file where the directory should be cannot be removed as one.
        std::fs::write(dir.join(&stuck), b"x").expect("file");
        let entry = |served: u64| Entry {
            fetched: Stamp::from_time(SystemTime::UNIX_EPOCH),
            fresh_until: Stamp::from_time(SystemTime::UNIX_EPOCH),
            etag: None,
            last_modified: None,
            served: Stamp::from_time(SystemTime::UNIX_EPOCH + Duration::from_secs(served)),
            bytes: 10,
        };
        index.put(&stuck, entry(1));
        index.put(&gone, entry(2));
        let index = Mutex::new(index);
        sweep(dir, &index, 0);
        let index = lock(&index);
        assert!(index.get(&stuck).is_some());
        assert!(
            index.get(&gone).is_none(),
            "already absent counts as deleted"
        );
        assert_eq!(index.total(), 10);
    }

    #[test]
    fn writing_an_entry_replaces_the_old_one() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = temp.path();
        let key = cache_key("https://img.example/ok.png");
        let first = bookclerk_cover::process(PNG).expect("set");
        let second = bookclerk_cover::process(&wide_png()).expect("set");
        assert!(write_entry(dir, &key, &first).expect("write") > 0);
        let written = write_entry(dir, &key, &second).expect("replace");
        assert_eq!(
            std::fs::read(dir.join(&key).join("full.jpg")).expect("read"),
            second.get(CoverSize::Full)
        );
        assert_eq!(
            written,
            CoverSize::ALL
                .into_iter()
                .map(|size| u64::try_from(second.get(size).len()).expect("size"))
                .sum::<u64>()
        );
        assert!(!dir
            .join(STAGING_DIR)
            .read_dir()
            .expect("staging")
            .any(|_| true));
    }
}
