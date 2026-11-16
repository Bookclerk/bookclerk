//! Baking the covers a new demo session shows into the image's cover cache.
//!
//! A visitor's first pages (the Discover shelves they land on and the
//! Library) request a cover for every title they show. Without a baked
//! cache, each one is downloaded through the `covers` plugin and decoded
//! by the media worker while the visitor waits, on a container with a
//! quarter of a CPU. The bake warms those covers instead, in two steps
//! (`packaging/demo/bake-seed.sh`):
//!
//! 1. With bookclerkd stopped, `--list-covers <file>` reads the library and
//!    writes the cover URLs those pages can show ([`cover_urls`]): each held
//!    work's cover, then the covers of the store listings Discover ranks
//!    from (the cached store calls and the stored candidates), then every
//!    stored work cover, at most [`MAX_BAKED_COVERS`].
//! 2. With bookclerkd running again, `--bake-covers <file> <base-url>` signs
//!    each URL with the bake's cover key and requests it from bookclerkd
//!    ([`warm`]), which downloads and processes it into `cache/covers/`.
//!
//! The cache is keyed by the image URL, not by the signing key, so the
//! entries stay valid after `--bake-finish` deletes the key and each
//! container makes its own.

use std::collections::{BTreeSet, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Context;
use bookclerk_cover::token::{self, TokenKey};
use bookclerk_library::entities::{
    discover_candidates, discover_store_call_hits, store_listings, works,
};
use bookclerk_library::{LibraryStore, WorkPageQuery};
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder};

/// Most covers a bake warms. The seeded library and the store calls
/// Discover caches name about 300; the cap keeps a larger seed from
/// growing the image without bound (each cover is three JPEGs, about
/// 100 KiB together).
pub const MAX_BAKED_COVERS: usize = 400;

/// Library rows read per page while listing covers.
const PAGE: u64 = 100;

/// Requests to bookclerkd in flight at once while warming. The media
/// worker processes covers on a small pool, so more would only queue.
const WARM_WORKERS: usize = 4;

/// Longest a single cover request may take.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// Longest the whole warm may take; covers not reached by then are left
/// for a container to fetch.
pub const WARM_BUDGET: Duration = Duration::from_secs(300);

/// Size requested for each cover. Processing any size writes all three
/// (128 px, 360 px and full), so one request warms the entry.
const WARM_SIZE: &str = "128";

/// The cover URLs a new session's first pages can show, best first, without
/// duplicates and at most [`MAX_BAKED_COVERS`].
///
/// # Arguments
///
/// * `store` - The seeded library, with its page index and Discover's
///   cached store calls written.
///
/// # Returns
///
/// Absolute `https` URLs, in the order: held works' covers (as the Library
/// page shows them), covers of the listings Discover's cached store calls
/// and stored candidates name, then any other stored work cover.
///
/// # Errors
///
/// Returns an error when a library read fails.
pub async fn cover_urls(store: &LibraryStore) -> anyhow::Result<Vec<String>> {
    let mut urls = CoverList::default();

    let mut query = WorkPageQuery {
        limit: PAGE,
        ..WorkPageQuery::default()
    };
    loop {
        let page = store
            .library_works_page(&query)
            .await
            .context("read the library index")?;
        let editions: Vec<String> = page
            .works
            .iter()
            .filter_map(|w| w.cover_edition_id.clone())
            .collect();
        let covers = store
            .edition_cover_urls(&editions)
            .await
            .context("read edition covers")?;
        for edition in &editions {
            urls.push(covers.get(edition).map(String::as_str));
        }
        match page.next {
            Some(next) => query.after = Some(next),
            None => break,
        }
    }

    let db = store.db();
    let mut listing_ids: Vec<String> = Vec::new();
    for hit in discover_store_call_hits::Entity::find()
        .order_by_asc(discover_store_call_hits::Column::CallKey)
        .order_by_asc(discover_store_call_hits::Column::Position)
        .all(db)
        .await
        .context("read Discover's cached store calls")?
    {
        listing_ids.push(hit.listing_id);
    }
    for candidate in discover_candidates::Entity::find()
        .order_by_asc(discover_candidates::Column::Id)
        .all(db)
        .await
        .context("read Discover's candidates")?
    {
        listing_ids.push(candidate.listing_id);
    }
    let mut seen = HashSet::new();
    listing_ids.retain(|id| seen.insert(id.clone()));
    for chunk in listing_ids.chunks(usize::try_from(PAGE).unwrap_or(100)) {
        let rows = store_listings::Entity::find()
            .filter(store_listings::Column::Id.is_in(chunk.iter().cloned()))
            .all(db)
            .await
            .context("read listings")?;
        // Keep the store calls' order rather than the read's.
        for id in chunk {
            urls.push(
                rows.iter()
                    .find(|row| &row.id == id)
                    .and_then(|row| row.cover_url.as_deref()),
            );
        }
    }

    for work in works::Entity::find()
        .filter(works::Column::CoverUrl.is_not_null())
        .order_by_asc(works::Column::Id)
        .all(db)
        .await
        .context("read work covers")?
    {
        urls.push(work.cover_url.as_deref());
    }
    Ok(urls.into_vec())
}

/// Cover URLs in first-seen order, deduplicated and capped.
#[derive(Debug, Default)]
struct CoverList {
    /// The URLs kept, in order.
    urls: Vec<String>,
    /// Every URL kept, for deduplication.
    seen: BTreeSet<String>,
}

impl CoverList {
    /// Keeps `url` when it is a coverable (absolute `https`) URL not kept
    /// yet and the list is not full.
    fn push(&mut self, url: Option<&str>) {
        let Some(url) = url.map(str::trim).filter(|u| token::is_coverable(u)) else {
            return;
        };
        if self.urls.len() < MAX_BAKED_COVERS && self.seen.insert(url.to_string()) {
            self.urls.push(url.to_string());
        }
    }

    /// The URLs kept, in order.
    fn into_vec(self) -> Vec<String> {
        self.urls
    }
}

/// The URL list file's text: one URL per line.
#[must_use]
pub fn list_text(urls: &[String]) -> String {
    let mut text = urls.join("\n");
    text.push('\n');
    text
}

/// The URLs in a list file's text, skipping blank lines.
#[must_use]
pub fn parse_list(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

/// Reads the cover cache's signing key that bookclerkd made for this bake.
///
/// # Arguments
///
/// * `cache_dir` - The files dir's cache dir (`paths.cache_dir`).
///
/// # Errors
///
/// Returns an error when `covers/token.key` is missing or is not 32 bytes:
/// bookclerkd has not run yet, or another program wrote it.
pub fn read_key(cache_dir: &Path) -> anyhow::Result<TokenKey> {
    let path = cache_dir.join("covers").join("token.key");
    let bytes = std::fs::read(&path).with_context(|| format!("read {}", path.display()))?;
    let bytes = <[u8; 32]>::try_from(bytes.as_slice())
        .map_err(|_| anyhow::anyhow!("{} is not a 32-byte key", path.display()))?;
    Ok(TokenKey::from_bytes(bytes))
}

/// What warming the covers did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WarmReport {
    /// Covers bookclerkd served, and so cached.
    pub cached: usize,
    /// Covers that failed (a download refused or an image that would not
    /// decode); a container tries them again.
    pub failed: usize,
    /// Covers not requested before [`WARM_BUDGET`] ran out.
    pub skipped: usize,
}

/// Requests each cover in `urls` from bookclerkd at `base`, so it downloads
/// and caches them.
///
/// Best effort: a failed cover is counted, never an error, since the demo
/// works without it.
///
/// # Arguments
///
/// * `base` - bookclerkd's address, such as `http://127.0.0.1:8787`.
/// * `key` - The cover signing key bookclerkd uses ([`read_key`]).
/// * `bearer` - The operator token, sent as `Authorization: Bearer`, when
///   the bake has one.
/// * `urls` - The cover URLs ([`cover_urls`]).
/// * `budget` - Longest the whole warm may take.
pub fn warm(
    base: &str,
    key: &TokenKey,
    bearer: Option<&str>,
    urls: &[String],
    budget: Duration,
) -> WarmReport {
    let base = base.trim_end_matches('/');
    let paths: Vec<String> = urls
        .iter()
        .filter_map(|url| token::sign(key, url))
        .map(|token| format!("{base}/api/covers/{token}/{WARM_SIZE}"))
        .collect();
    let unsigned = urls.len() - paths.len();
    let queue = Arc::new(Mutex::new(paths.into_iter()));
    let cached = Arc::new(AtomicUsize::new(0));
    let failed = Arc::new(AtomicUsize::new(unsigned));
    let deadline = Instant::now() + budget;
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(REQUEST_TIMEOUT))
        .http_status_as_error(false)
        .build()
        .into();
    let auth = bearer.map(|token| format!("Bearer {token}"));
    std::thread::scope(|scope| {
        for _ in 0..WARM_WORKERS {
            let queue = Arc::clone(&queue);
            let cached = Arc::clone(&cached);
            let failed = Arc::clone(&failed);
            let agent = agent.clone();
            let auth = auth.clone();
            scope.spawn(move || loop {
                if Instant::now() >= deadline {
                    return;
                }
                let next = queue.lock().ok().and_then(|mut q| q.next());
                let Some(path) = next else {
                    return;
                };
                let mut request = agent.get(&path);
                if let Some(auth) = &auth {
                    request = request.header("Authorization", auth);
                }
                let ok = request.call().is_ok_and(|mut response| {
                    response.status() == 200 && response.body_mut().read_to_vec().is_ok()
                });
                if ok {
                    cached.fetch_add(1, Ordering::Relaxed);
                } else {
                    failed.fetch_add(1, Ordering::Relaxed);
                }
            });
        }
    });
    let skipped = queue.lock().map(|q| q.len()).unwrap_or(0);
    WarmReport {
        cached: cached.load(Ordering::Relaxed),
        failed: failed.load(Ordering::Relaxed),
        skipped,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_list_keeps_coverable_urls_once_in_order_up_to_the_cap() {
        let mut list = CoverList::default();
        list.push(Some(" https://img.example/a.jpg "));
        list.push(Some("http://img.example/insecure.jpg"));
        list.push(None);
        list.push(Some("https://img.example/a.jpg"));
        list.push(Some("https://img.example/b.jpg"));
        assert_eq!(
            list.into_vec(),
            ["https://img.example/a.jpg", "https://img.example/b.jpg"]
        );
        let mut full = CoverList::default();
        for i in 0..=MAX_BAKED_COVERS {
            full.push(Some(&format!("https://img.example/{i}.jpg")));
        }
        assert_eq!(full.into_vec().len(), MAX_BAKED_COVERS);
    }

    #[test]
    fn a_list_file_round_trips() {
        let urls = vec![
            "https://img.example/a.jpg".to_string(),
            "https://img.example/b.jpg".to_string(),
        ];
        assert_eq!(parse_list(&format!("\n{}\n\n", list_text(&urls))), urls);
    }

    #[test]
    fn the_key_must_be_32_bytes() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(read_key(dir.path()).is_err(), "no key yet");
        std::fs::create_dir_all(dir.path().join("covers")).expect("mkdir");
        std::fs::write(dir.path().join("covers/token.key"), [7u8; 31]).expect("write");
        assert!(read_key(dir.path()).is_err(), "short key");
        std::fs::write(dir.path().join("covers/token.key"), [7u8; 32]).expect("write");
        assert_eq!(read_key(dir.path()).expect("key").as_bytes(), &[7u8; 32]);
    }

    #[tokio::test]
    async fn warming_requests_each_signed_cover_and_counts_the_answers() {
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        let key = TokenKey::from_bytes([3u8; 32]);
        let good = "https://img.example/good.jpg".to_string();
        let bad = "https://img.example/bad.jpg".to_string();
        let good_token = token::sign(&key, &good).expect("sign");
        Mock::given(method("GET"))
            .and(path(format!("/api/covers/{good_token}/128")))
            .and(header("Authorization", "Bearer op"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![0xFFu8, 0xD8]))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(502))
            .mount(&server)
            .await;
        let base = server.uri();
        let urls = vec![good, bad, "http://img.example/unsigned.jpg".to_string()];
        let report = tokio::task::spawn_blocking(move || {
            warm(&base, &key, Some("op"), &urls, Duration::from_secs(30))
        })
        .await
        .expect("join");
        assert_eq!(
            report,
            WarmReport {
                cached: 1,
                failed: 2,
                skipped: 0
            }
        );
        let none = tokio::task::spawn_blocking(|| {
            warm(
                "http://127.0.0.1:9",
                &TokenKey::from_bytes([3u8; 32]),
                None,
                &["https://img.example/late.jpg".to_string()],
                Duration::ZERO,
            )
        })
        .await
        .expect("join");
        assert_eq!(none.skipped, 1, "nothing is requested past the budget");
    }
}
