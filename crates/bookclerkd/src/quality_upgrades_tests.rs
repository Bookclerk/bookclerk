// Test code: a failed fixture write or read is the test failing.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::missing_panics_doc)]

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use bookclerk_library::{
    AcquireStatus, AudioStream, FileOwner, FileProbe, FileRole, LibraryStore, NewBook,
};
use bookclerk_source::{PortalAuthMode, SourceBrand};
use bookclerk_tags::{AacProfile, AudioCodec, AudioFormat};

use super::*;

/// The default download options every check in these tests fetches with.
fn download() -> bookclerk_source::DownloadOptions {
    bookclerk_source::DownloadOptions::default()
}

/// Brand the fake store shows; never rendered.
const BRAND: SourceBrand = SourceBrand {
    id: "audible",
    name: "Audible",
    bg: "#000000",
    fg: "#ffffff",
    accent: "#111111",
    icon_url: "https://example.invalid/icon",
};

/// A store that answers quality offers from a table by product id and
/// records every batch it was asked.
struct OfferingStore {
    /// The stream offered per product id; absent ids get no answer.
    offers: HashMap<String, StreamQuality>,
    /// Every batch of product ids asked, in order.
    asked: Mutex<Vec<Vec<String>>>,
    /// Fail every call with a rate limit instead of answering.
    limited: bool,
}

impl OfferingStore {
    fn new(offers: &[(&str, StreamQuality)]) -> Self {
        Self {
            offers: offers
                .iter()
                .map(|(id, stream)| ((*id).to_owned(), *stream))
                .collect(),
            asked: Mutex::new(Vec::new()),
            limited: false,
        }
    }

    fn asked(&self) -> Vec<Vec<String>> {
        self.asked.lock().unwrap().clone()
    }
}

#[async_trait]
impl ContentSource for OfferingStore {
    fn id(&self) -> &str {
        "audible"
    }

    fn portal_auth_mode(&self) -> PortalAuthMode {
        PortalAuthMode::Oauth
    }

    fn portal_brand(&self) -> SourceBrand {
        BRAND
    }

    async fn login(
        &self,
        _scope: &bookclerk_library::SourceScope,
        _opts: bookclerk_source::LoginOptions,
    ) -> bookclerk_source::Result<bookclerk_source::SourceAccount> {
        Err(bookclerk_source::SourceError::api("not in this test"))
    }

    async fn list_accounts(
        &self,
        _scope: &bookclerk_library::SourceScope,
    ) -> bookclerk_source::Result<Vec<bookclerk_source::SourceAccount>> {
        Ok(Vec::new())
    }

    async fn scan(
        &self,
        _scope: &bookclerk_library::SourceScope,
        _opts: bookclerk_source::ScanOptions,
    ) -> bookclerk_source::Result<bookclerk_source::ScanSummary> {
        Err(bookclerk_source::SourceError::api("not in this test"))
    }

    async fn fetch_title(
        &self,
        _scope: &bookclerk_library::SourceScope,
        _account_id: &str,
        _title_id: &str,
        _opts: &bookclerk_source::FetchOptions,
    ) -> bookclerk_source::Result<bookclerk_source::SourceFetch> {
        Err(bookclerk_source::SourceError::api("not in this test"))
    }

    fn answers_quality_offers(&self) -> bool {
        true
    }

    async fn quality_offers(
        &self,
        _scope: &bookclerk_library::SourceScope,
        _account_id: &str,
        title_ids: &[String],
        _download: &bookclerk_source::DownloadOptions,
    ) -> bookclerk_source::Result<Vec<Option<StreamQuality>>> {
        self.asked.lock().unwrap().push(title_ids.to_vec());
        if self.limited {
            return Err(bookclerk_source::SourceError::RateLimited(None));
        }
        Ok(title_ids
            .iter()
            .map(|id| self.offers.get(id).copied())
            .collect())
    }
}

/// An empty in-memory library.
async fn test_store() -> LibraryStore {
    LibraryStore::from_connection(
        bookclerk_plugin_database_sqlite::open_memory()
            .await
            .unwrap(),
    )
}

fn lc(kbps: u32) -> StreamQuality {
    StreamQuality {
        codec: AudioCodec::Aac,
        aac_profile: Some(AacProfile::Lc),
        bitrate_kbps: Some(kbps),
    }
}

/// Records `asin` as acquired by `user-1` with an original-quality AAC-LC
/// file probed at `kbps`, and gives the account its scan state.
async fn acquired(store: &LibraryStore, asin: &str, kbps: u32) -> AcquireTitle {
    store
        .upsert_account("audible", "user-1", "us", None, true, "audible")
        .await
        .unwrap();
    store
        .upsert_book(&NewBook::minimal(asin, "user-1", "us", asin))
        .await
        .unwrap();
    let key = format!("A/{asin}.m4b");
    store
        .set_acquire_status(asin, "user-1", AcquireStatus::Acquired, Some(&key), None)
        .await
        .unwrap();
    let title = store
        .acquire_title("audible", "user-1", "us", asin)
        .await
        .unwrap()
        .expect("title");
    let owner = FileOwner::Edition(title.edition_id.clone());
    let copy = bookclerk_config::OutputCopy {
        format: bookclerk_config::CopyFormat::M4b,
        quality: bookclerk_config::CopyQuality::ORIGINAL,
    };
    assert!(store
        .record_edition_copy_file("local", &key, &owner, FileRole::Audio, Some(&copy))
        .await
        .unwrap());
    let probe = FileProbe {
        content_type: "audio/mp4".into(),
        bitrate_kbps: Some(kbps),
        lossless: false,
        source_store: Some("audible".into()),
    };
    assert!(store
        .set_edition_file_probe("local", &key, &owner, &probe)
        .await
        .unwrap());
    let stream = AudioStream {
        codec: Some(AudioCodec::Aac),
        format: AudioFormat {
            channels: Some(2),
            sample_rate_hz: Some(44_100),
            aac_profile: Some(AacProfile::Lc),
        },
        duration_ms: Some(3_600_000),
    };
    assert!(store
        .set_edition_file_stream("local", &key, &owner, &stream)
        .await
        .unwrap());
    store
        .finish_scan_pass("audible", "user-1", "", true)
        .await
        .unwrap();
    title
}

/// A better offer is found once a day; a title whose offer is no better
/// is not, and an account that was never scanned is not checked.
#[tokio::test]
async fn a_better_offer_is_found_once_a_day() {
    let store = test_store().await;
    let now = Utc::now();
    let source = OfferingStore::new(&[]);
    assert!(
        find_upgrades(&store, &source, "user-1", &download(), 5, now)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(source.asked().is_empty(), "no scan state, nothing to ask");

    let low = acquired(&store, "B00LOW", 64).await;
    let same = acquired(&store, "B00SAME", 128).await;
    let source = OfferingStore::new(&[("B00LOW", lc(128)), ("B00SAME", lc(128))]);
    let found = find_upgrades(&store, &source, "user-1", &download(), 5, now)
        .await
        .unwrap();
    assert_eq!(found, vec![low.holding_id.clone()]);
    assert_eq!(source.asked().len(), 1);
    assert_eq!(source.asked()[0].len(), 2);
    assert_ne!(same.holding_id, low.holding_id);

    // Within the day the account is not asked again.
    let later = now + chrono::Duration::hours(2);
    assert!(
        find_upgrades(&store, &source, "user-1", &download(), 5, later)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(source.asked().len(), 1);
    let checked = store
        .quality_check_state("audible", "user-1")
        .await
        .unwrap()
        .expect("state");
    assert_eq!(
        checked.cursor, "",
        "the walk reached the end and starts over"
    );

    // A day later it is.
    let tomorrow = now + chrono::Duration::hours(24);
    assert_eq!(
        find_upgrades(&store, &source, "user-1", &download(), 5, tomorrow)
            .await
            .unwrap(),
        vec![low.holding_id]
    );
}

/// The daily limit stops the check at the last upgrade it found, and the
/// next check continues after it.
#[tokio::test]
async fn the_daily_limit_stops_at_the_last_upgrade() {
    let store = test_store().await;
    let first = acquired(&store, "B00ONE", 64).await;
    let second = acquired(&store, "B00TWO", 64).await;
    let source = OfferingStore::new(&[("B00ONE", lc(128)), ("B00TWO", lc(128))]);
    let now = Utc::now();
    let mut ordered = [first.holding_id.clone(), second.holding_id.clone()];
    ordered.sort();
    let found = find_upgrades(&store, &source, "user-1", &download(), 1, now)
        .await
        .unwrap();
    assert_eq!(found, vec![ordered[0].clone()]);
    let state = store
        .quality_check_state("audible", "user-1")
        .await
        .unwrap()
        .expect("state");
    assert_eq!(state.cursor, ordered[0]);
    let found = find_upgrades(
        &store,
        &source,
        "user-1",
        &download(),
        1,
        now + chrono::Duration::days(1),
    )
    .await
    .unwrap();
    assert_eq!(found, vec![ordered[1].clone()]);
}

/// A store that asks to slow down fails the check, which still counts as
/// the day's check so the store is not asked again within the day.
#[tokio::test]
async fn a_rate_limited_store_is_left_alone_for_the_day() {
    let store = test_store().await;
    acquired(&store, "B00LOW", 64).await;
    let mut source = OfferingStore::new(&[("B00LOW", lc(128))]);
    source.limited = true;
    let now = Utc::now();
    assert!(
        find_upgrades(&store, &source, "user-1", &download(), 5, now)
            .await
            .is_err()
    );
    assert!(find_upgrades(
        &store,
        &source,
        "user-1",
        &download(),
        5,
        now + chrono::Duration::hours(1)
    )
    .await
    .unwrap()
    .is_empty());
    assert_eq!(source.asked().len(), 1);
}

/// With the setting off nothing is checked; with it on, an upgrade is
/// queued as a forced, paced acquire of the account's title.
#[tokio::test]
async fn the_setting_gates_the_check_and_upgrades_queue_acquires() {
    let files = tempfile::tempdir().unwrap();
    let (state, store) = crate::node_api::tests::test_state(&files).await;
    acquired(&store, "B00LOW", 64).await;
    let source = Arc::new(OfferingStore::new(&[("B00LOW", lc(128))]));
    state.sources.write().await.register(source.clone());
    assert_eq!(queue_quality_upgrades(state.clone()).await.unwrap(), 0);
    assert!(source.asked().is_empty());

    state.config.write().await.library.upgrade_quality = true;
    assert_eq!(queue_quality_upgrades(state.clone()).await.unwrap(), 1);
    let jobs = store.list_jobs(10).await.unwrap();
    let acquire = jobs
        .iter()
        .find(|job| job.kind == bookclerk_library::JobKind::Acquire)
        .expect("an acquire job");
    assert!(acquire.payload.force);
    assert!(acquire.payload.paced);
    // The account was checked today.
    assert_eq!(queue_quality_upgrades(state.clone()).await.unwrap(), 0);
    assert_eq!(source.asked().len(), 1);
}
